//! 述語形 `UPDATE`／`DELETE` の `EXPLAIN` の構文段と実行段（Issue #2050・SQL-75 のポインタ。親: #2049）。
//!
//! 責務境界: `core.rs` の構文入口（`parse_tokens_in_impl`）が `EXPLAIN UPDATE|DELETE`（分割実行
//! 修飾なし）を見つけたときに [`EngineCore::parse_explain_dml`] を呼び、実行入口
//! （autocommit・明示トランザクション内）が [`EngineCore::run_explain_dml`] を呼ぶ。出力の木の形は
//! `sql::explain_dml` が担い、本モジュールは「実行時と同じ検証順で束縛し、書き込まずに返す」ことだけを担う。
//!
//! 副作用ゼロ（SQL-75 (3)）: write txn・writer・台帳・ジョブ表・行・シーケンス・トリガ・世代のいずれにも
//! 触れない。使うのは読み取りのスキーマ取得（`load_table_schema_for_dml`）と純粋な束縛・ハッシュ計算だけ。
//!
//! `MERGE`（Issue #3357）も同じ入口で受理し、木は `core/merge_exec.rs` の `merge_explain_outcome` が組む。
//!
//! 結合形 `UPDATE ... FROM`／`DELETE ... USING`（Issue #3293）も同じ入口で受理し、実行器
//! （`sql::join::dml`）と同じ束縛・変換・走査前検証・source 走査の束縛（`source_bound_scan`）で
//! 木を組む。source 側の行・索引には触れず、読むのはスキーマと GIN 宣言だけ。
//!
//! 権限・テナント境界: 権限は `sql::privilege_gate::write` が実行した場合と同じ DELETE／UPDATE 権限を
//! 要求する（SQL-40）。行を読まないので他テナントの行・件数・存在は出力に影響しない（SQL-75 (5)・RLS-10）。

use super::{EngineCore, LedgerMode, ParsedSql};
use crate::policy::PolicyContext;
use crate::sql::allowlist::{DeleteStatement, SqlSurfaceError, ValidatedUpdateForm};
use crate::sql::explain_dml::{
    explain_tree, joined_explain_tree, ExplainDml, ExplainDmlShape, JoinedExplainShape,
};

impl EngineCore {
    /// `EXPLAIN UPDATE|DELETE`（分割実行修飾なし）の構文段。先頭の `EXPLAIN` を除いた文を
    /// 非 EXPLAIN の DML と**同じ入口**（`parse_tokens_in_impl`）で解析する。構造検証・WHERE 省略ガード・
    /// 存在確認・採番表の単一行形→述語形の変換・ビュー判定の順序とエラーが実行時と一致する（SQL-75 (3)）。
    /// 述語形の表への DML と結合形（`UPDATE ... FROM`／`DELETE ... USING`。Issue #3293）を受理し、
    /// それ以外の形は固定文の `42601`（fail-closed）で拒否する。一時表・ビューが関与する形は
    /// 呼び出し元の振り分けが `0A000` にする。
    pub(super) fn parse_explain_dml(
        &self,
        tokens: &[crate::sql::lexer::Token],
        temp: &crate::sql::temp_catalog::SessionTempCatalog,
        hits: &crate::sql::temp_catalog::RelationHits,
        ledger_mode: LedgerMode,
        read_snap: Option<&redb::ReadTransaction>,
    ) -> Result<ParsedSql, SqlSurfaceError> {
        let reject = |detail: &str| SqlSurfaceError::unsupported(detail);
        let rest = tokens
            .get(1..)
            .ok_or_else(|| reject("EXPLAIN requires a SELECT or UPDATE/DELETE statement"))?
            .to_vec();
        // 名前解決は呼び出し元で済んでいる（冪等）ため resolver は渡さない。
        let inner =
            self.parse_tokens_in_impl(rest, temp, hits, ledger_mode, None, read_snap, None)?;
        match inner {
            ParsedSql::Delete(DeleteStatement::Predicate(v)) => {
                if super::dml_subquery_exec::where_has_subquery(v.where_predicates()) {
                    return Err(reject(
                        "EXPLAIN is not supported for UPDATE/DELETE with subqueries",
                    ));
                }
                Ok(ParsedSql::ExplainDml(ExplainDml::Delete(v)))
            }
            ParsedSql::Update(ValidatedUpdateForm::Predicate(v)) => {
                if super::dml_subquery_exec::where_has_subquery(v.where_predicates()) {
                    return Err(reject(
                        "EXPLAIN is not supported for UPDATE/DELETE with subqueries",
                    ));
                }
                Ok(ParsedSql::ExplainDml(ExplainDml::Update(v)))
            }
            // 物理キーを直接指す単一行形は、実行器が全走査でないため既存の語彙で表せない。
            ParsedSql::Delete(_) | ParsedSql::Update(_) => Err(reject(
                "EXPLAIN is not supported for single-row UPDATE/DELETE by id",
            )),
            ParsedSql::ViewDelete(_) | ParsedSql::ViewUpdate(_) => Err(reject(
                "EXPLAIN is not supported for UPDATE/DELETE on a view",
            )),
            // 結合形は非 EXPLAIN と同じ解析器が返した検証済みの値をそのまま包む（第 2 の解析器を持たない）。
            // `$n` は構文入口（拡張クエリの Parse ゲート）が前置つきでも `42601` で拒否済み。
            ParsedSql::UpdateFrom(v) => Ok(ParsedSql::ExplainDml(ExplainDml::UpdateFrom(v))),
            ParsedSql::DeleteUsing(v) => Ok(ParsedSql::ExplainDml(ExplainDml::DeleteUsing(v))),
            // `MERGE`（Issue #3357）も同じ。`$n` は構文入口が `42601` で拒否済み。
            ParsedSql::Merge(v) => Ok(ParsedSql::ExplainDml(ExplainDml::Merge(v))),
            // 一時表（`SessionRouted`）は呼び出し元の振り分けが分類するため、ここへは来ない想定。
            // それ以外は固定文で拒否する（fail-closed）。
            _ => Err(reject(
                "EXPLAIN is not supported for this UPDATE/DELETE form",
            )),
        }
    }

    /// 受理済みの `EXPLAIN UPDATE|DELETE` の実行段。実行時と同じ束縛（`prepare_predicate_*`）で
    /// 未知の列・型不一致を同じ `wire_code` にし、書き込まずに計画の木を返す。結合形は
    /// [`Self::joined_explain_outcome`] へ回す（Issue #3293）。
    pub(super) fn run_explain_dml(
        &self,
        ctx: &PolicyContext,
        session: &crate::sql::mode::SessionState,
        explain: &ExplainDml,
    ) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
        let udfs = session.udfs();
        let shape = match explain {
            ExplainDml::Delete(stmt) => {
                let schema = self.load_table_schema_for_dml(stmt.table_name())?;
                // 通常実行（returning_form_pre）と同じ順序で RETURNING を先に束縛し、wire_code を一致させる。
                crate::sql::parser::bind_returning_with_udfs(stmt.returning(), &schema, udfs)?;
                let (bound, _hash) = Self::prepare_predicate_delete(stmt, stmt, &schema, udfs)?;
                ExplainDmlShape {
                    assignments: None,
                    has_filter: bound.has_where_filters(),
                    has_returning: stmt.returning().is_some(),
                    has_operation_id: stmt.operation_id().is_some(),
                }
            }
            ExplainDml::Update(stmt) => {
                let schema = self.load_table_schema_for_dml(stmt.table_name())?;
                // 通常実行（returning_form_pre）と同じ順序で RETURNING を先に束縛し、wire_code を一致させる。
                crate::sql::parser::bind_returning_with_udfs(stmt.returning(), &schema, udfs)?;
                let set_columns: Vec<&str> =
                    stmt.assignments().iter().map(|(n, _)| n.as_str()).collect();
                let _sparsevec_limits_scope = self.sparsevec_limits_scope();
                let _defer = self.defer_not_null_scope_for_update(ctx, &schema.name, &set_columns);
                let (bound, _hash, _legacy) =
                    Self::prepare_predicate_update(stmt, stmt, &schema, udfs)?;
                ExplainDmlShape {
                    assignments: Some(stmt.assignments().len()),
                    has_filter: !bound.metadata_filters().is_empty()
                        || !bound.expr_filters().is_empty()
                        || !bound.or_filters().is_empty(),
                    has_returning: stmt.returning().is_some(),
                    has_operation_id: stmt.operation_id().is_some(),
                }
            }
            ExplainDml::UpdateFrom(stmt) => {
                let (plan, read_txn) = self.plan_update_from_stmt_with_txn(stmt, udfs)?;
                return self.joined_explain_outcome(
                    &read_txn,
                    &plan,
                    Some(stmt.assignments.len()),
                    stmt.returning.is_some(),
                    stmt.operation_id().is_some(),
                    udfs,
                );
            }
            ExplainDml::Merge(stmt) => return self.merge_explain_outcome(session, stmt),
            ExplainDml::DeleteUsing(stmt) => {
                let (plan, read_txn) = self.plan_delete_using_stmt_with_txn(stmt, udfs)?;
                return self.joined_explain_outcome(
                    &read_txn,
                    &plan,
                    None,
                    stmt.returning.is_some(),
                    stmt.operation_id().is_some(),
                    udfs,
                );
            }
        };
        let node = explain_tree(&shape).map_err(|e| e.into_surface_error())?;
        Ok(crate::sql::SqlOutcome::Explain(node.into_query_result()))
    }

    /// 結合形 DML の木を組む（Issue #3293）。実行時と同じ順序（束縛・変換 → 走査前の形検証・キー型クラス →
    /// source 走査の束縛）で検証するため、拒否の `wire_code` は実行と一致する。読むのはスキーマと GIN 宣言だけで、
    /// 行・台帳・writer・世代には触れない。`read_txn` は束縛に使ったスナップショット。
    fn joined_explain_outcome(
        &self,
        read_txn: &redb::ReadTransaction,
        plan: &crate::sql::join::dml::JoinedDmlPlan,
        assignments: Option<usize>,
        has_returning: bool,
        has_operation_id: bool,
        udfs: &crate::sql::udf_call::UdfRegistry,
    ) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
        // FROM／USING の JOIN 連鎖・多表（Issue #3294）の計画木は未対応。単一表 source 前提の木を
        // 誤って返さないよう fail-closed で拒否する（実行側は受理済み）。
        if plan.source.fold.is_some() {
            return Err(SqlSurfaceError::feature_not_supported(
                "EXPLAIN of multi-relation FROM/USING is not supported yet",
            ));
        }
        let limits = self.joined_dml_limits();
        plan.validate(&limits)?;
        plan.key_classes()?;
        let bound = crate::sql::join::dml::source_bound_scan(plan, udfs, &limits)?;
        let gins = Self::gin_decls_for_explain(read_txn, &bound)?;
        let source_lines = Self::scan_explain_lines(&plan.source.schema, &bound, &gins);
        let shape = JoinedExplainShape {
            assignments,
            has_filter: !plan.target.metadata_filters.is_empty()
                || !plan.target.expr_filters.is_empty()
                || !plan.target.or_filters.is_empty(),
            has_returning,
            has_operation_id,
            join_keys: plan.keys.len(),
        };
        let node = joined_explain_tree(&shape, source_lines).map_err(|e| e.into_surface_error())?;
        Ok(crate::sql::SqlOutcome::Explain(node.into_query_result()))
    }
}
