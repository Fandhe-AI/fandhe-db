//! `UPDATE ... FROM`／`DELETE ... USING` の SQL 入口（SQL-53 (b)、Issue #1754・#3201・#3202）。
//!
//! 責務境界: 構文・束縛（`sql::dml_from`）→ 変換（`sql::join::dml_plan`）→ 結合実行器
//! （`dml_join_exec::execute_joined_dml_in`）の橋渡しだけを担う。autocommit は `core.rs` が文単位の暗黙
//! トランザクションへ振り分け、明示トランザクションは共有 write txn 上で実行して `mark_written` する。
//! 束縛・変換の拒否（`42702`・`42P01`・`42804`・`0A000` 等）は Execute でも Describe でも同じコードで返る。
//! `RETURNING` つき（Issue #3204）は `SqlOutcome::Returning` を返し、Describe は同じ列メタデータを返す。
//!
//! 台帳（Issue #3203・RECOVER-10/11）: `USING OPERATION_ID` 付きの文は `operation_id` を実行器へ渡す。
//! 台帳の事前照合（再送 `23505`・別文への再利用 `22023`）と確定記録は実行器が同じ write txn で行い、
//! 束縛・変換のエラーや実行の失敗は txn が commit されないため台帳を消費しない。同一トランザクション内の
//! 再利用検査は `core.rs` の `parsed_operation_id` が担う。
//!
//! テナント境界（P0）: 行の走査・RLS 適用は実行器の契約に従い、本モジュールは RLS を迂回する経路を
//! 作らない。エラー文言には入力された名前と固定文言だけを含め、他テナントの情報を含めない。
//!
//! 呼び出し元: `core.rs` の明示トランザクション腕・Describe 腕の `ParsedSql::UpdateFrom`／`DeleteUsing`。

use super::dml_join_exec::JoinedDmlOutcome;
use super::EngineCore;
use crate::catalog::TableSchema;
use crate::policy::PolicyContext;
use crate::sql::allowlist::{
    DmlFromSource, SqlSurfaceError, ValidatedDeleteUsing, ValidatedUpdateFrom,
};
use crate::sql::dml_from_returning::column_meta;
use crate::sql::exec::{DeleteOutcome, QueryResult, ReturningOutcome, UpdateOutcome};
use crate::sql::join::dml::JoinedDmlPlan;
use crate::sql::mode::SessionState;
use crate::sql::returning::DmlCommand;
use crate::sql::transaction::SessionTransaction;
use crate::sql::udf_call::UdfRegistry;
use crate::sql::SqlOutcome;
use crate::tenant::WriteTarget;

/// 暗黙トランザクションを通らない経路（ネストした実行等）への固定文言（`0A000`）。
pub(super) const JOINED_DML_UNSUPPORTED_HERE: &str =
    "UPDATE ... FROM / DELETE ... USING is not supported for this statement";

/// 結合形 DML（`UPDATE ... FROM`／`DELETE ... USING`）かどうか。autocommit の暗黙トランザクション振り分けに使う。
pub(super) fn is_joined_dml(parsed: &super::ParsedSql) -> bool {
    matches!(
        parsed,
        super::ParsedSql::UpdateFrom(_) | super::ParsedSql::DeleteUsing(_)
    )
}

/// 結合形 DML の `EXPLAIN ANALYZE`（実際に書き込む）かどうか（Issue #3293）。autocommit では
/// 内側の実行が共有 write txn を要するため、[`is_joined_dml`] と同じく暗黙トランザクションへ振り分ける。
/// 静的な `EXPLAIN`（`ParsedSql::ExplainDml`）は書き込まないので対象外。
pub(super) fn is_joined_explain_analyze(parsed: &super::ParsedSql) -> bool {
    use crate::sql::explain_analyze::ExplainAnalyzeDmlTarget;
    use crate::sql::explain_dml::ExplainDml;
    matches!(
        parsed,
        super::ParsedSql::ExplainAnalyzeDml(ea)
            if matches!(
                &ea.target,
                ExplainAnalyzeDmlTarget::Dml(
                    ExplainDml::UpdateFrom(_) | ExplainDml::DeleteUsing(_)
                )
            )
    )
}

impl EngineCore {
    /// 対象表と FROM／USING 側の表のスキーマを同一スナップショットから取得する。使った読み取り
    /// トランザクションも返す（`EXPLAIN` が索引宣言を同じスナップショットで読むため。Issue #3293）。
    fn dml_from_schemas(
        &self,
        target: &str,
        others: &[crate::sql::relation::TableRef],
        sources: &[DmlFromSource],
    ) -> Result<(redb::ReadTransaction, TableSchema, Vec<TableSchema>), SqlSurfaceError> {
        // ビュー source は基底表のスキーマを引く（スキーマは射影せず、束縛スコープだけを狭める。Issue #3292）。
        let physical = |i: usize, r: &crate::sql::relation::TableRef| -> String {
            match sources.get(i) {
                Some(DmlFromSource::View(v)) => v.base_table.clone(),
                _ => r.table().to_string(),
            }
        };
        let mut names: Vec<String> = Vec::with_capacity(
            others
                .len()
                .saturating_add(1)
                .min(crate::sql::relation::MAX_TABLE_REFS),
        );
        names.push(target.to_string());
        names.extend(others.iter().enumerate().map(|(i, r)| physical(i, r)));
        let (read_txn, schemas) = self.read_txn_with_schemas(&names)?;
        let internal = || SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        };
        // 自己結合（同じ表を別名で複数回参照）は取得を 1 回にまとめているため、複製して並べる。
        let target_schema = schemas.get(target).cloned().ok_or_else(internal)?;
        let mut other_schemas = Vec::with_capacity(others.len());
        for (i, r) in others.iter().enumerate() {
            other_schemas.push(schemas.get(&physical(i, r)).cloned().ok_or_else(internal)?);
        }
        Ok((read_txn, target_schema, other_schemas))
    }

    /// `UPDATE ... FROM` を束縛し、結合 DML の実行入力へ変換する（Issue #3201）。
    /// SQL 経路の dispatch（Issue #3202）が実行器へ渡す入口で、スキーマだけを参照する純粋な変換のため
    /// 行・台帳・書き込みトランザクションには触れない。
    pub(super) fn plan_update_from_stmt(
        &self,
        stmt: &ValidatedUpdateFrom,
        udfs: &UdfRegistry,
    ) -> Result<JoinedDmlPlan, SqlSurfaceError> {
        self.plan_update_from_stmt_with_txn(stmt, udfs)
            .map(|(plan, _read_txn)| plan)
    }

    /// Describe（拡張クエリの Parse 時のダミー値の計画を含む）用の [`Self::plan_update_from_stmt`]。
    /// `conjunct_dummy` は WHERE の連言分ごとの「`$n` 由来のダミー値か」（Issue #3297）。
    pub(super) fn plan_update_from_stmt_for_describe(
        &self,
        stmt: &ValidatedUpdateFrom,
        udfs: &UdfRegistry,
        conjunct_dummy: &[bool],
    ) -> Result<JoinedDmlPlan, SqlSurfaceError> {
        let (_read_txn, target, others) =
            self.dml_from_schemas(stmt.target_table(), &stmt.from, &stmt.sources)?;
        let refs: Vec<&TableSchema> = others.iter().collect();
        // Describe 用フラグの並びは「SET の代入ごと ++ WHERE の連言分ごと」（`joined_dml_dummy_flags`）。
        // 代入数に満たなければ全て検証側へ倒す。
        let n_set = stmt.assignments.len();
        let (set_dummy, where_dummy) = if conjunct_dummy.len() >= n_set {
            conjunct_dummy.split_at(n_set)
        } else {
            (&[][..], &[][..])
        };
        let bound =
            crate::sql::dml_from::bind_update_from_describe(stmt, &target, &refs, udfs, set_dummy)?;
        let mut plan = crate::sql::join::dml_plan::plan_update_from_describe(
            &bound,
            &target,
            &refs,
            udfs,
            where_dummy,
        )?;
        apply_view_source(&mut plan, &stmt.sources, udfs)?;
        Ok(plan)
    }

    /// [`Self::plan_delete_using_stmt`] の Describe 用版（Issue #3297。上と同じ契約）。
    pub(super) fn plan_delete_using_stmt_for_describe(
        &self,
        stmt: &ValidatedDeleteUsing,
        udfs: &UdfRegistry,
        conjunct_dummy: &[bool],
    ) -> Result<JoinedDmlPlan, SqlSurfaceError> {
        let (_read_txn, target, others) =
            self.dml_from_schemas(stmt.target_table(), &stmt.using, &stmt.sources)?;
        let refs: Vec<&TableSchema> = others.iter().collect();
        let bound = crate::sql::dml_from::bind_delete_using_with_udfs(stmt, &target, &refs, udfs)?;
        let mut plan = crate::sql::join::dml_plan::plan_delete_using_describe(
            &bound,
            &target,
            &refs,
            udfs,
            conjunct_dummy,
        )?;
        apply_view_source(&mut plan, &stmt.sources, udfs)?;
        Ok(plan)
    }

    /// [`Self::plan_update_from_stmt`] が使った読み取りトランザクションも返す版
    /// （`EXPLAIN UPDATE ... FROM` が GIN 宣言を同じスナップショットで読む。Issue #3293）。
    pub(super) fn plan_update_from_stmt_with_txn(
        &self,
        stmt: &ValidatedUpdateFrom,
        udfs: &UdfRegistry,
    ) -> Result<(JoinedDmlPlan, redb::ReadTransaction), SqlSurfaceError> {
        let (read_txn, target, others) =
            self.dml_from_schemas(stmt.target_table(), &stmt.from, &stmt.sources)?;
        let refs: Vec<&TableSchema> = others.iter().collect();
        let bound = crate::sql::dml_from::bind_update_from_with_udfs(stmt, &target, &refs, udfs)?;
        let mut plan = crate::sql::join::dml_plan::plan_update_from(&bound, &target, &refs, udfs)?;
        apply_view_source(&mut plan, &stmt.sources, udfs)?;
        Ok((plan, read_txn))
    }

    /// `DELETE ... USING` を束縛し、結合 DML の実行入力へ変換する（Issue #3201）。
    pub(super) fn plan_delete_using_stmt(
        &self,
        stmt: &ValidatedDeleteUsing,
        udfs: &UdfRegistry,
    ) -> Result<JoinedDmlPlan, SqlSurfaceError> {
        self.plan_delete_using_stmt_with_txn(stmt, udfs)
            .map(|(plan, _read_txn)| plan)
    }

    /// [`Self::plan_delete_using_stmt`] が使った読み取りトランザクションも返す版（Issue #3293）。
    pub(super) fn plan_delete_using_stmt_with_txn(
        &self,
        stmt: &ValidatedDeleteUsing,
        udfs: &UdfRegistry,
    ) -> Result<(JoinedDmlPlan, redb::ReadTransaction), SqlSurfaceError> {
        let (read_txn, target, others) =
            self.dml_from_schemas(stmt.target_table(), &stmt.using, &stmt.sources)?;
        let refs: Vec<&TableSchema> = others.iter().collect();
        let bound = crate::sql::dml_from::bind_delete_using_with_udfs(stmt, &target, &refs, udfs)?;
        let mut plan = crate::sql::join::dml_plan::plan_delete_using(&bound, &target, &refs, udfs)?;
        apply_view_source(&mut plan, &stmt.sources, udfs)?;
        Ok((plan, read_txn))
    }

    /// `UPDATE ... FROM` を共有 write txn 上で実行する（autocommit は暗黙 Tx 経由で同じ腕に入る）。
    /// 失敗時は呼び出し元が明示 Tx を `Failed` へ遷移させる。書いた表（対象表・トリガが書いた表）へ
    /// `mark_written` する（漏れると COMMIT が write txn を破棄し遅延 FK 検査も取りこぼす。P0）。
    pub(super) fn execute_update_from_in_txn(
        &self,
        ctx: &PolicyContext,
        session: &SessionState,
        txn: &mut SessionTransaction<'_>,
        stmt: &ValidatedUpdateFrom,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        let plan = self.plan_update_from_stmt(stmt, session.udfs())?;
        let out = self.run_joined_in_txn(
            ctx,
            session,
            txn,
            &plan,
            stmt.operation_id(),
            stmt.statement_hash(),
        )?;
        Ok(joined_outcome(
            &plan,
            out,
            DmlCommand::Update,
            |rows_affected| SqlOutcome::Update(UpdateOutcome { rows_affected }),
        ))
    }

    /// `DELETE ... USING` の [`Self::execute_update_from_in_txn`] 相当。
    pub(super) fn execute_delete_using_in_txn(
        &self,
        ctx: &PolicyContext,
        session: &SessionState,
        txn: &mut SessionTransaction<'_>,
        stmt: &ValidatedDeleteUsing,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        let plan = self.plan_delete_using_stmt(stmt, session.udfs())?;
        let out = self.run_joined_in_txn(
            ctx,
            session,
            txn,
            &plan,
            stmt.operation_id(),
            stmt.statement_hash(),
        )?;
        Ok(joined_outcome(
            &plan,
            out,
            DmlCommand::Delete,
            |rows_affected| SqlOutcome::Delete(DeleteOutcome { rows_affected }),
        ))
    }

    /// 一時コアが一時 DB の write txn 上で `UPDATE ... FROM` を実行する（Issue #3292）。
    /// 一時コアは台帳なし（`CompareOnlyWithoutLedger`）で、一時表はトリガを持てないため `mark_written` や
    /// トリガ走査は不要。呼び出し元は `core::temp_route::temp_dispatch`。
    pub(super) fn execute_update_from_on(
        &self,
        target: WriteTarget<'_>,
        ctx: &PolicyContext,
        session: &SessionState,
        stmt: &ValidatedUpdateFrom,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        let plan = self.plan_update_from_stmt(stmt, session.udfs())?;
        let out = self.execute_joined_dml_in(
            target,
            ctx,
            session,
            &plan,
            stmt.operation_id(),
            stmt.statement_hash(),
        )?;
        Ok(joined_outcome(
            &plan,
            out,
            DmlCommand::Update,
            |rows_affected| SqlOutcome::Update(UpdateOutcome { rows_affected }),
        ))
    }

    /// `DELETE ... USING` の [`Self::execute_update_from_on`] 相当。
    pub(super) fn execute_delete_using_on(
        &self,
        target: WriteTarget<'_>,
        ctx: &PolicyContext,
        session: &SessionState,
        stmt: &ValidatedDeleteUsing,
    ) -> Result<SqlOutcome, SqlSurfaceError> {
        let plan = self.plan_delete_using_stmt(stmt, session.udfs())?;
        let out = self.execute_joined_dml_in(
            target,
            ctx,
            session,
            &plan,
            stmt.operation_id(),
            stmt.statement_hash(),
        )?;
        Ok(joined_outcome(
            &plan,
            out,
            DmlCommand::Delete,
            |rows_affected| SqlOutcome::Delete(DeleteOutcome { rows_affected }),
        ))
    }

    /// 結合形 DML の共通実行本体。writer 取得 → 実行器 → `mark_written` を行い、影響行数を返す。
    fn run_joined_in_txn(
        &self,
        ctx: &PolicyContext,
        session: &SessionState,
        txn: &mut SessionTransaction<'_>,
        plan: &JoinedDmlPlan,
        operation_id: Option<&crate::recovery::required_op_id::OperationId>,
        content_hash: &crate::recovery::content_hash::ContentHash,
    ) -> Result<JoinedDmlOutcome, SqlSurfaceError> {
        // 最初の書き込み文で writer を取得する（待機超過は `55P03`。呼び出し元が `Failed` へ遷移）。
        txn.ensure_writer(ctx)?;
        let write_txn = txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        })?;
        // トリガが書いた表も COMMIT 時の遅延 FK 検査の対象にする。
        let trigger_scope = crate::tenant::trigger_writes::TriggerWrittenTablesScope::enter();
        // operation_id は構文段で検証済みの値をそのまま渡す（台帳は実行器が担う）。RETURNING は #3204 で収集する。
        let out = self.execute_joined_dml_in(
            WriteTarget::InTxn(write_txn),
            ctx,
            session,
            plan,
            operation_id,
            content_hash,
        )?;
        txn.mark_written(ctx.tenant_id(), &plan.target.table);
        for written in trigger_scope.take() {
            txn.mark_written(ctx.tenant_id(), &written);
        }
        Ok(out)
    }
}

/// source がビューのとき、計画の読み取り先を基底表へ張り替え、ビュー定義の述語を source 側述語の
/// 先頭へ畳み込む（Issue #3292。集合演算枝のビュー展開と同じ順序）。
///
/// テナント境界（P0・RLS-10）: ビュー定義はテナント情報を持たないため、作成者の可視性は引き継がない。
/// 基底表は書き込むセッションの `PolicyContext` で走査され（実行器の契約）、ビュー述語は絞り込みだけを
/// 担うセキュリティ境界ではない述語である。述語は基底スキーマに対して再検証し、束縛できなければ拒否する。
fn apply_view_source(
    plan: &mut JoinedDmlPlan,
    sources: &[DmlFromSource],
    udfs: &UdfRegistry,
) -> Result<(), SqlSurfaceError> {
    let Some(DmlFromSource::View(view)) = sources.first() else {
        return Ok(());
    };
    let mut preds = view.view_predicates.clone();
    preds.append(&mut plan.source.where_predicates);
    let mut node_budget = crate::sql::udf_call::MAX_EXPR_NODES;
    crate::sql::parser::bind_where_predicates(
        &preds,
        &plan.source.schema,
        udfs,
        &mut node_budget,
        &[],
    )?;
    plan.source.table = view.base_table.clone();
    plan.source.where_predicates = preds;
    Ok(())
}

/// 実行結果を SQL 表層の応答へ写す。`RETURNING` つき（計画が持つ）なら `SqlOutcome::Returning`
/// （列は計画の束縛結果、行は実行器が収集したもの）、なければ従来の件数だけの応答。
/// `rows_affected` は書き込み層の実変更行数で、返却行数（RLS 再判定で減りうる）ではない。
fn joined_outcome(
    plan: &JoinedDmlPlan,
    out: JoinedDmlOutcome,
    command: DmlCommand,
    plain: impl FnOnce(u64) -> SqlOutcome,
) -> SqlOutcome {
    match (&plan.returning, out.returning_rows) {
        (Some(bound), Some(rows)) => SqlOutcome::Returning(ReturningOutcome {
            command,
            rows_affected: out.rows_affected,
            result: QueryResult {
                columns: column_meta(bound),
                rows,
            },
        }),
        _ => plain(out.rows_affected),
    }
}
