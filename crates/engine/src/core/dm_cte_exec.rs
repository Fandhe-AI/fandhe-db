//! データ変更 CTE（`WITH d AS (INSERT|UPDATE|DELETE ... RETURNING ...) SELECT ... FROM d`）の
//! 実行本体（SQL-63・Issue #1876。親: #1874。構文受理・名前・列の確定は #1875）。
//!
//! 責務境界: 検証済みの実行計画（[`ValidatedDataModifyingCte`]）を 1 つの書き込みトランザクションの
//! 上で実行する。副文は単独の DML と**同じフォーム関数**（`execute_*_returning_form`）を通す
//! （第 2 の書き込み経路を作らない）ため、RLS の暗黙適用・制約検査・行数上限・`RETURNING` の
//! バイト上限はそのまま効く。副文の `RETURNING` 行は具体化し、主クエリの外側処理は評価後射影形
//! ビューと同じ `sql::view_buffered`（`plan_outer_parts`／`apply_outer`）を通す。
//!
//! 原子性: 呼び出し元が write txn を用意する。autocommit は `execute_subquery_dml_autocommit` と
//! 同じ文単位の暗黙トランザクション（commit 時に遅延 FK の検査を通る）、明示・暗黙トランザクションは
//! 共有 write txn（`execute_in_active_txn` の腕）。失敗（副文の制約違反・主クエリのエラー・上限超過）は
//! txn の abort で全体が破棄される。
//!
//! 台帳（SQL-63 (5)・RECOVER-1・RECOVER-11）: 文末の `USING OPERATION_ID` で文全体を 1 エントリで記録する。
//! 最初の副文を実行する**前**に事前照合し（再送は何も実行せず `23505`／`22023`）、全副文の後に記録する。
//! 副文自身は `operation_id` を持たないため、副文の実行中だけ台帳モードを「省略可」へ切り替える
//! （[`SubstatementLedgerScope`]。`Ledgered` のままだと副文が `23502` になる）。台帳・世代の単位は
//! 定義順で最初の副文の対象表（決定的。`docs/design/data-modifying-cte.md`）。
//!
//! テナント境界（P0）: 副文・主クエリとも呼び出したセッション自身の `PolicyContext` で実行し、
//! 計画はテナントを保持しない（RLS-10 (b)）。エラーは固定文言で、行の値・他テナントの存在を含めない。
//!
//! 呼び出し元: `core.rs` の `execute_in_active_txn`（`ParsedSql::DataModifyingCte` 腕。autocommit も
//! 暗黙トランザクション経由で同じ腕に到達する）と Describe 腕（[`EngineCore::describe_data_modifying_cte`]）。

use super::EngineCore;
use crate::policy::PolicyContext;
use crate::recovery::required_op_id::LedgerMode;
use crate::sql::allowlist::{BufferedOuter, SqlSurfaceError, ValidatedDataModifyingCte};
use crate::sql::cte::{DataModifyingCteBody, DmlStatement};
use crate::sql::exec::{ColumnMeta, QueryResult, ReturningOutcome};
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::tenant::WriteTarget;
use std::cell::Cell;

thread_local! {
    /// 副文の実行中だけ有効な台帳モードの上書き（未設定なら `EngineCore::ledger_mode`）。
    static SUBSTATEMENT_LEDGER: Cell<Option<LedgerMode>> = const { Cell::new(None) };
}

/// 副文の実行区間を現スレッドへ張る RAII ガード。drop で直前の状態へ戻す。副文は文単位の
/// `USING OPERATION_ID` を持てない（構文段で `42601`）ため、台帳記録は文全体の 1 エントリに集約し、
/// 副文の書き込み経路は「省略可」の台帳モードで実行する。
pub(super) struct SubstatementLedgerScope {
    prev: Option<LedgerMode>,
}

impl SubstatementLedgerScope {
    /// 副文用の台帳モード（`Ledgered`・`Optional` は `Optional`、`CompareOnlyWithoutLedger` はそのまま）
    /// へ切り替える。
    pub(super) fn enter(engine_mode: LedgerMode) -> Self {
        let mode = match engine_mode {
            LedgerMode::Ledgered | LedgerMode::Optional => LedgerMode::Optional,
            LedgerMode::CompareOnlyWithoutLedger => LedgerMode::CompareOnlyWithoutLedger,
        };
        let prev = SUBSTATEMENT_LEDGER.with(|c| c.replace(Some(mode)));
        SubstatementLedgerScope { prev }
    }
}

impl Drop for SubstatementLedgerScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        SUBSTATEMENT_LEDGER.with(|c| c.set(prev));
    }
}

impl EngineCore {
    /// DML のフォーム関数（`execute_*_form`／`execute_*_returning_form`）が書き込み経路へ渡す台帳モード。
    /// 通常は `self.ledger_mode`。データ変更 CTE の副文の実行中だけ [`SubstatementLedgerScope`] が
    /// 上書きする。
    pub(super) fn dml_ledger_mode(&self) -> LedgerMode {
        SUBSTATEMENT_LEDGER
            .with(Cell::get)
            .unwrap_or(self.ledger_mode)
    }

    /// 外側処理の事前計画（Describe と Execute が共有）。参照される副文の対象表スキーマで
    /// `RETURNING` を束縛し、その結果列に対して主クエリの外側を束縛する。書き込みは行わない。
    fn plan_data_modifying_outer(
        &self,
        session: &SessionState,
        stmt: &ValidatedDataModifyingCte,
        limits: &crate::result_limits::ResultLimits,
    ) -> Result<crate::sql::view_buffered::OuterPlan, SqlSurfaceError> {
        let body = stmt
            .referenced_body()
            .ok_or_else(crate::sql::cte::data_modifying_cte_unsupported)?;
        // 外側 `LIMIT` の範囲は設定値で検査する（評価後射影形ビューと同じ契約。Issue #1555）。
        if let BufferedOuter::Rows(rows) = &stmt.outer {
            if rows.limit_explicit {
                crate::sql::parser::validate_search_limit_with(rows.limit, limits)?;
            }
        }
        let schema = self.load_table_schema_for_dml(&body.target_table)?;
        // 照合が運ばれずバイト列順で黙って比較される fail-open を作らない（TABLE-28・Issue #2190）。
        if stmt.outer.depends_on_order()
            && schema
                .columns
                .iter()
                .any(|c| c.collation.is_some_and(|coll| !coll.is_byte_order()))
        {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: "this collation is not supported in this expression".to_string(),
            });
        }
        let projection = crate::sql::parser::bind_returning(body.returning(), &schema)?
            .ok_or_else(crate::sql::cte::data_modifying_cte_unsupported)?;
        let columns: Vec<ColumnMeta> = crate::sql::returning::column_meta(&projection, &schema)?;
        crate::sql::view_buffered::plan_outer_parts(
            &columns,
            &stmt.view_name,
            &stmt.column_aliases,
            &stmt.outer,
            session.udfs(),
        )
    }

    /// 拡張クエリの Describe 腕から呼ぶ。書き込みは一切実行せず、主クエリの外側処理後の列メタを返す。
    pub(super) fn describe_data_modifying_cte(
        &self,
        session: &SessionState,
        stmt: &ValidatedDataModifyingCte,
        limits: &crate::result_limits::ResultLimits,
    ) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
        let plan = self.plan_data_modifying_outer(session, stmt, limits)?;
        Ok(plan.columns().to_vec())
    }

    /// 副文 1 件を既存のフォーム関数で実行する（`RETURNING` 付きだけ。実行形のゲートが保証する）。
    fn run_data_modifying_substatement(
        &self,
        write_txn: &redb::WriteTransaction,
        ctx: &PolicyContext,
        session: &SessionState,
        body: &DataModifyingCteBody,
    ) -> Result<ReturningOutcome, SqlSurfaceError> {
        let target = WriteTarget::InTxn(write_txn);
        match &body.statement {
            DmlStatement::Insert(v) if v.returning.is_some() => {
                let lookup = super::InsertSchemaLookup::new(&self.storage);
                self.execute_insert_returning_form(target, ctx, v, &lookup)
            }
            DmlStatement::Update(form) => {
                let has_returning = match form {
                    crate::sql::allowlist::ValidatedUpdateForm::Single(v) => v.returning.is_some(),
                    crate::sql::allowlist::ValidatedUpdateForm::Predicate(v) => {
                        v.returning().is_some()
                    }
                };
                if !has_returning {
                    return Err(crate::sql::cte::data_modifying_cte_unsupported());
                }
                self.execute_update_returning_form(target, ctx, session, form)
            }
            DmlStatement::Delete(crate::sql::allowlist::DeleteStatement::SingleRow(v))
                if v.returning.is_some() =>
            {
                self.execute_delete_returning_form(target, ctx, v)
            }
            DmlStatement::Delete(crate::sql::allowlist::DeleteStatement::Predicate(v))
                if v.returning().is_some() =>
            {
                self.execute_predicate_delete_returning_form(target, ctx, session, v)
            }
            // `RETURNING` なしの副文は本 Issue では実行しない（検証段のゲートで到達しない。多層防御）。
            _ => Err(crate::sql::cte::data_modifying_cte_unsupported()),
        }
    }

    /// データ変更 CTE を含む文の実行本体。`write_txn` 上で台帳の事前照合 → 副文を定義順に実行 →
    /// 文の台帳記録 → 主クエリの外側処理、の順で行う。戻り値は（結果, 書き込んだ表名の一覧）で、
    /// 呼び出し元（明示・暗黙トランザクションの腕）が全表へ `mark_written` する。
    pub(super) fn execute_data_modifying_cte(
        &self,
        write_txn: &redb::WriteTransaction,
        ctx: &PolicyContext,
        session: &SessionState,
        stmt: &ValidatedDataModifyingCte,
    ) -> Result<(SqlOutcome, Vec<String>), SqlSurfaceError> {
        let limits = self.result_limits;
        // 外側の束縛エラー（未知列・曖昧な列・`LIMIT` 範囲・照合）は何かを書く前に確定させる。
        self.plan_data_modifying_outer(session, stmt, &limits)?;

        let ledger_table = stmt.ledger_table();
        crate::sql::exec::precheck_insert_select_ledger(
            &WriteTarget::InTxn(write_txn),
            ctx,
            ledger_table,
            stmt.operation_id.as_ref(),
            self.ledger_mode,
            &stmt.statement_hash,
        )?;

        let mut written: Vec<String> = Vec::new();
        let mut referenced: Option<QueryResult> = None;
        {
            let _ledger_scope = SubstatementLedgerScope::enter(self.ledger_mode);
            for (idx, body) in stmt.dm_ctes.iter().enumerate() {
                let outcome =
                    self.run_data_modifying_substatement(write_txn, ctx, session, body)?;
                if !written.contains(&body.target_table) {
                    written.push(body.target_table.clone());
                }
                if idx == stmt.referenced {
                    referenced = Some(outcome.result);
                }
            }
        }
        let result = referenced.ok_or_else(|| SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        })?;

        // 文全体を台帳 1 エントリで記録する（省略時・記録しない構成では何もしない）。
        crate::sql::exec::record_insert_select_ledger_only(
            WriteTarget::InTxn(write_txn),
            ctx,
            ledger_table,
            stmt.operation_id.as_ref(),
            self.ledger_mode,
            &stmt.statement_hash,
        )?;

        // 主クエリ: 実際の結果列に対して外側を束縛し直して適用する（事前計画と同じ入口）。
        let plan = crate::sql::view_buffered::plan_outer_parts(
            &result.columns,
            &stmt.view_name,
            &stmt.column_aliases,
            &stmt.outer,
            session.udfs(),
        )?;
        let processed =
            crate::sql::view_buffered::apply_outer(result, &plan, self.group_by_limits)?;
        // `LIMIT` 省略・`ALL` の外側は後処理後の件数が結果行数上限を超えたら `54000`
        // （黙って切り詰めない。SQL-34）。集計は HAVING・OFFSET 適用後の件数で判定する。
        let processed = match &stmt.outer {
            BufferedOuter::Rows(r) if r.limit_implicit => {
                Self::enforce_result_cap(processed, limits.max_result_rows())?
            }
            BufferedOuter::Aggregate(agg) => {
                Self::enforce_group_result_cap(agg, processed, &limits)?
            }
            BufferedOuter::Rows(_) => processed,
        };
        Ok((SqlOutcome::Query(processed), written))
    }
}
