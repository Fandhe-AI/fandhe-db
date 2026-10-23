//! 結合形 `UPDATE ... FROM`／`DELETE ... USING` の実行入口（SQL-53 (b)・RLS-10・RECOVER-11。
//! Issue #1755。親: #1753）。
//!
//! 責務境界: 束縛済みの実行入力（[`JoinedDmlPlan`]。Issue #1754 の束縛器が生成する契約型）を受け取り、
//! 書き込みトランザクション内で FROM／USING 側を走査して結合の probe を用意し、既存の述語形 DML の
//! 書き込み経路（`tenant::update_rows_joined_unchecked`／`delete_rows_where_unchecked`）へ渡す。
//! 結合・一意化・上限の本体は `sql::join::dml`、書き込み・制約検査・世代 bump は `tenant` が担い、
//! 本モジュールはゲート（`WriteTarget`・台帳・未対応形）と結果・エラーの写像だけを持つ。
//!
//! 結線状況（分岐 B）: Issue #1754（構文・束縛）が未マージのため SQL 文字列からの dispatch は
//! まだ無く、構文段は従来どおり FROM／USING を拒否する（挙動不変）。#1754 のマージ後に `core.rs` の
//! UPDATE／DELETE 分岐から本入口を呼ぶ（autocommit は `execute_subquery_dml_autocommit` と同じ
//! 文単位の暗黙トランザクションに流す）。それまで本モジュールは単体テストからのみ到達する。
//!
//! テナント境界（P0）: 対象側は既存の候補列挙（テナントのキー範囲に閉じた走査・所有かつ可視）だけ、
//! FROM／USING 側は書き込むセッションの `PolicyContext`（RLS 暗黙適用）で走査する。エラー文言に
//! 表名・テナント・行 id・他テナントの件数を含めない。
//!
//! fail-closed: `WriteTarget::InTxn` 以外・台帳記録を伴う構成・未対応の形は固定文言の `0A000` で
//! 拒否する。上限超過（`54000`）やエラーは write txn を commit しないため副作用ゼロ。
//! 呼び出し元（#1754 の dispatch）の責務: 書き込み後に `SessionTransaction::mark_written`
//! （テナント・対象表）で書き込みを記録する（commit 時の遅延 FK 検査・commit 境界に必要）。
//!
//! 暫定ゲート（Issue #1757 で撤去）: 台帳（RECOVER-11）の content_hash 結線が未実装のため、
//! `ledger_mode.resolve(op_id)` が記録を伴う構成を内容照合の前に拒否する
//! （[`EngineCore::joined_dml_operation_id_gate`]）。

// 分岐 B では SQL 経路からの呼び出しが無い。#1754 の結線時に撤去する。
#![allow(dead_code)]

use super::EngineCore;
use crate::policy::PolicyContext;
use crate::recovery::ledger::LedgerWrite;
use crate::recovery::required_op_id::OperationId;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::join::dml::{JoinedDmlPlan, JoinedProbe};
use crate::sql::join::JoinLimits;
use crate::sql::mode::SessionState;
use crate::tenant::{PredicateDmlError, PredicateDmlOutcome, ReturningProjectFn, WriteTarget};

fn not_supported(detail: &str) -> SqlSurfaceError {
    SqlSurfaceError::FeatureNotSupported {
        detail: detail.to_string(),
    }
}

/// 結合形 DML の実行結果。`matches` は一致した対象行 id と採った source の走査位置
/// （Issue #1756 が FROM 側列参照の RETURNING で使う内部値）。
pub(crate) struct JoinedDmlOutcome {
    pub(crate) rows_affected: u64,
    pub(crate) matches: Vec<(u64, u32)>,
}

impl EngineCore {
    /// 暫定ゲート（Issue #1757 で撤去）。台帳記録を伴う構成（`operation_id` 指定・Ledgered 構成）は
    /// 内容照合ハッシュの結線が済むまで `0A000` で拒否し、`Omitted`／`Disabled` のときだけ通す。
    fn joined_dml_operation_id_gate(
        &self,
        op_id: Option<&OperationId>,
    ) -> Result<(), SqlSurfaceError> {
        let ledger_write = self
            .ledger_mode
            .resolve(op_id)
            .map_err(|_| SqlSurfaceError::MissingOperationId)?;
        match ledger_write {
            LedgerWrite::Omitted | LedgerWrite::Disabled => Ok(()),
            LedgerWrite::Record(_) | LedgerWrite::RecordEvaluated { .. } => Err(not_supported(
                "operation_id is not supported for UPDATE ... FROM / DELETE ... USING yet",
            )),
        }
    }

    /// 結合形 DML を実行する。起動時設定の上限（結合・走査・影響行数）を使う。
    pub(super) fn execute_joined_dml_in(
        &self,
        target: WriteTarget<'_>,
        ctx: &PolicyContext,
        session: &SessionState,
        plan: &JoinedDmlPlan,
        operation_id: Option<&OperationId>,
        project: Option<&mut ReturningProjectFn<'_>>,
    ) -> Result<JoinedDmlOutcome, SqlSurfaceError> {
        let limits = JoinLimits {
            max_input_rows: self.join_limits.max_input_rows(),
            max_output_rows: self.join_limits.max_output_rows(),
            max_relations: self.join_limits.max_relations(),
            ..JoinLimits::default()
        };
        self.execute_joined_dml_with_limits(
            target,
            ctx,
            session,
            plan,
            operation_id,
            project,
            &limits,
            self.dml_limits.max_affected_rows,
            self.scan_limits.max_scanned_rows(),
        )
    }

    /// [`Self::execute_joined_dml_in`] の本体。上限を明示で受け取る（テスト専用に差し替え可能にした
    /// もの。`sql::join::execute_with_limits` と同じ設計）。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn execute_joined_dml_with_limits(
        &self,
        target: WriteTarget<'_>,
        ctx: &PolicyContext,
        session: &SessionState,
        plan: &JoinedDmlPlan,
        operation_id: Option<&OperationId>,
        project: Option<&mut ReturningProjectFn<'_>>,
        limits: &JoinLimits,
        max_affected_rows: Option<std::num::NonZeroUsize>,
        max_scanned_rows: usize,
    ) -> Result<JoinedDmlOutcome, SqlSurfaceError> {
        // source の走査と書き込みを同じ write txn で行うため、暗黙・明示トランザクションの共有 txn に限る。
        let WriteTarget::InTxn(write_txn) = target else {
            return Err(not_supported(
                "UPDATE ... FROM / DELETE ... USING is not supported for this statement",
            ));
        };
        if plan.returning_references_source {
            return Err(not_supported(
                "RETURNING with a column of the FROM / USING table is not supported yet",
            ));
        }
        self.joined_dml_operation_id_gate(operation_id)?;

        // source のスキーマが束縛後に変わっていないこと（対象表は書き込み側が照合する）。
        let live_source =
            crate::catalog::require_table_schema_write_any_identity(write_txn, &plan.source.table)
                .map_err(|e| match e {
                    crate::catalog::CatalogError::TableNotFound(name) => {
                        SqlSurfaceError::UndefinedTable { name }
                    }
                    _ => SqlSurfaceError::Internal {
                        detail: "failed to load table schema".to_string(),
                    },
                })?;
        if live_source != plan.source.schema {
            return Err(SqlSurfaceError::invalid_input(
                "table schema changed after the statement was bound",
            ));
        }

        // FROM／USING 側を `ctx`（RLS 暗黙適用）で走査してハッシュ表にする。以降の書き込み側の
        // 候補列挙より前に完結させる（自己結合でも適用前に全部の一致を確定する）。
        let mut probe = JoinedProbe::new(write_txn, ctx, plan, session.udfs(), limits)?;

        // 暫定の content_hash（台帳記録に使われない。#1757 で正規化ハッシュへ置換する）。
        let hash = crate::recovery::content_hash::for_delete_where(
            &plan.target.table,
            &[],
            session.udfs(),
        )?;
        let ledger_write = self
            .ledger_mode
            .resolve(operation_id)
            .map_err(|_| SqlSurfaceError::MissingOperationId)?;
        let schema = &plan.target.schema;
        let target = WriteTarget::InTxn(write_txn);
        let needs_embedding_extra = probe.needs_embedding();

        let result = crate::sql::exec::run_where_predicate(
            schema,
            &plan.target.metadata_filters,
            &plan.target.expr_filters,
            &plan.target.or_filters,
            |needs_embedding, where_pred| match &plan.update {
                Some(update) => {
                    let predicate = |c: &crate::tenant::DmlCandidate<'_>| {
                        if !where_pred(c)? {
                            return Ok(None);
                        }
                        probe.probe_update(schema, c)
                    };
                    crate::tenant::update_rows_joined_unchecked(
                        target,
                        &plan.target.table,
                        ctx,
                        ledger_write,
                        &hash,
                        Some(schema),
                        &update.literal_assignments,
                        needs_embedding || needs_embedding_extra,
                        max_affected_rows,
                        max_scanned_rows,
                        predicate,
                        project,
                    )
                }
                None => {
                    let predicate = |c: &crate::tenant::DmlCandidate<'_>| {
                        if !where_pred(c)? {
                            return Ok(false);
                        }
                        probe.probe_delete(schema, c)
                    };
                    match project {
                        None => crate::tenant::delete_rows_where_unchecked(
                            target,
                            &plan.target.table,
                            ctx,
                            ledger_write,
                            &hash,
                            Some(schema),
                            needs_embedding,
                            max_affected_rows,
                            max_scanned_rows,
                            predicate,
                        ),
                        Some(project) => crate::tenant::delete_rows_where_capturing_unchecked(
                            target,
                            &plan.target.table,
                            ctx,
                            ledger_write,
                            &hash,
                            Some(schema),
                            needs_embedding,
                            max_affected_rows,
                            max_scanned_rows,
                            predicate,
                            project,
                        ),
                    }
                }
            },
        );
        let op = if plan.update.is_some() {
            "update"
        } else {
            "delete"
        };
        let rows_affected = match result {
            Ok(PredicateDmlOutcome::Applied { rows_affected }) => rows_affected as u64,
            Ok(PredicateDmlOutcome::LimitExceeded { count }) => {
                // 影響行数は一意化**後**の対象行数。超過は副作用ゼロで `54000`。
                crate::sql::parser::check_dml_affected_rows_with_limit(count, max_affected_rows)?;
                return Err(SqlSurfaceError::Internal {
                    detail: "joined DML limit check did not reject an over-limit count".to_string(),
                });
            }
            Err(PredicateDmlError::Predicate(e)) => return Err(e),
            Err(PredicateDmlError::Write(e)) => {
                return Err(crate::sql::exec::map_write_error(e, op))
            }
        };
        Ok(JoinedDmlOutcome {
            rows_affected,
            matches: probe.matches().to_vec(),
        })
    }
}
