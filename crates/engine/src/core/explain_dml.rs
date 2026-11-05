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
//! 権限・テナント境界: 権限は `sql::privilege_gate::write` が実行した場合と同じ DELETE／UPDATE 権限を
//! 要求する（SQL-40）。行を読まないので他テナントの行・件数・存在は出力に影響しない（SQL-75 (5)・RLS-10）。

use super::{EngineCore, LedgerMode, ParsedSql};
use crate::policy::PolicyContext;
use crate::sql::allowlist::{DeleteStatement, SqlSurfaceError, ValidatedUpdateForm};
use crate::sql::explain_dml::{explain_tree, ExplainDml, ExplainDmlShape};

impl EngineCore {
    /// `EXPLAIN UPDATE|DELETE`（分割実行修飾なし）の構文段。先頭の `EXPLAIN` を除いた文を
    /// 非 EXPLAIN の DML と**同じ入口**（`parse_tokens_in_impl`）で解析する。構造検証・WHERE 省略ガード・
    /// 存在確認・採番表の単一行形→述語形の変換・ビュー判定の順序とエラーが実行時と一致する（SQL-75 (3)）。
    /// 述語形の表への DML だけを受理し、それ以外の形は固定文の `42601`（fail-closed）で拒否する。
    /// 一時表は呼び出し元の振り分けが `0A000` にする。
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
            // 一時表（`SessionRouted`）は呼び出し元の振り分けが分類するため、ここへは来ない想定。
            // それ以外（UPDATE ... FROM／DELETE ... USING など）は固定文で拒否する。
            _ => Err(reject(
                "EXPLAIN is not supported for UPDATE ... FROM / DELETE ... USING",
            )),
        }
    }

    /// 受理済みの `EXPLAIN UPDATE|DELETE` の実行段。実行時と同じ束縛（`prepare_predicate_*`）で
    /// 未知の列・型不一致を同じ `wire_code` にし、書き込まずに計画の木を返す。
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
                crate::sql::parser::bind_returning(stmt.returning(), &schema)?;
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
                crate::sql::parser::bind_returning(stmt.returning(), &schema)?;
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
        };
        let node = explain_tree(&shape).map_err(|e| e.into_surface_error())?;
        Ok(crate::sql::SqlOutcome::Explain(node.into_query_result()))
    }
}
