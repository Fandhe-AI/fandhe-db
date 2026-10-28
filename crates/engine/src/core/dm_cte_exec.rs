//! データ変更 CTE（`WITH d AS (INSERT|UPDATE|DELETE ... [RETURNING ...]) SELECT ...`）の
//! 実行本体（SQL-63・Issue #1876・#1877。親: #1874。構文受理・名前・列の確定は #1875）。
//!
//! 責務境界: 検証済みの実行計画（[`ValidatedDataModifyingCte`]）を 1 つの書き込みトランザクションの
//! 上で実行する。副文は単独の DML と**同じフォーム関数**（`execute_*_form`／`execute_*_returning_form`）を
//! 通す（第 2 の書き込み経路を作らない）ため、RLS の暗黙適用・制約検査・行数上限・`RETURNING` の
//! バイト上限はそのまま効く。副文の `RETURNING` 行は具体化し、主クエリの外側処理は評価後射影形
//! ビューと同じ `sql::view_buffered`（`plan_outer_parts`／`apply_outer`）を通す。
//!
//! 1 文内の意味論（Issue #1877。PostgreSQL に揃える）: 全副文と主クエリは文の開始時点の状態を読み、
//! 互いの変更は `RETURNING` を通したときだけ見える。実行順（エラーの優先順位もこの順）は次のとおり。
//! 1. 外側の事前計画（束縛エラーの確定）と実行形のゲート（書き込み前）
//! 2. 台帳の事前照合
//! 3. 段 A: 全副文の `WHERE` サブクエリを書き込み前に定数へ解決する
//! 4. 段 B: 主クエリが実表・ビュー・SELECT 本文 CTE を読む形は、ここで実体化する（副文の書き込み前）
//! 5. 段 C: 副文を `WITH` の定義順にすべて実行する（参照されない副文・`RETURNING` なしの副文も実行）
//! 6. 台帳記録 → 主クエリの結果（副文を読む形は `RETURNING` 行に外側処理を適用）
//!
//! 同じ行を複数の副文が変更する場合は、`tenant::statement_writes` の文内変更集合で後の副文が先の副文の
//! 書いた行を対象にしないよう固定する（`docs/design/data-modifying-cte.md`）。
//!
//! 原子性: 呼び出し元が write txn を用意する。autocommit は `execute_subquery_dml_autocommit` と
//! 同じ文単位の暗黙トランザクション（commit 時に遅延 FK の検査を通る）、明示・暗黙トランザクションは
//! 共有 write txn（`execute_in_active_txn` の腕）。失敗（副文の制約違反・主クエリのエラー・上限超過）は
//! txn の abort で全体が破棄される（明示トランザクションは呼び出し元が `Failed` へ遷移させる）。
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
use crate::sql::allowlist::{
    BufferedOuter, DmMain, SqlSurfaceError, ValidatedDataModifyingCte, ValidatedUpdateForm,
};
use crate::sql::cte::{DataModifyingCteBody, DmlStatement};
use crate::sql::exec::{ColumnMeta, QueryResult};
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::tenant::WriteTarget;
use std::cell::Cell;

/// 段 A（副文の `WHERE` サブクエリの事前解決）の結果。`Some(解決済みの文)`・`None`（サブクエリ無し）の
/// 区別を保ったままフォーム関数へ渡す。
enum PreResolved {
    Update(Option<crate::sql::allowlist::ValidatedPredicateUpdate>),
    Delete(Option<crate::sql::allowlist::ValidatedPredicateDelete>),
    /// 事前解決の対象外の副文（`INSERT`・単一行の `UPDATE`／`DELETE`）。
    NotApplicable,
}

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
    /// 主クエリが副文を読まない形（[`DmMain::Read`]）では外側処理が無いため `None`。
    fn plan_data_modifying_outer(
        &self,
        session: &SessionState,
        stmt: &ValidatedDataModifyingCte,
        limits: &crate::result_limits::ResultLimits,
    ) -> Result<Option<crate::sql::view_buffered::OuterPlan>, SqlSurfaceError> {
        let DmMain::Referenced {
            view_name,
            column_aliases,
            outer,
            ..
        } = &stmt.main
        else {
            return Ok(None);
        };
        let body = stmt
            .referenced_body()
            .ok_or_else(crate::sql::cte::data_modifying_cte_unsupported)?;
        // 外側 `LIMIT` の範囲は設定値で検査する（評価後射影形ビューと同じ契約。Issue #1555）。
        if let BufferedOuter::Rows(rows) = outer {
            if rows.limit_explicit {
                crate::sql::parser::validate_search_limit_with(rows.limit, limits)?;
            }
        }
        let schema = self.load_table_schema_for_dml(&body.target_table)?;
        // 照合が運ばれずバイト列順で黙って比較される fail-open を作らない（TABLE-28・Issue #2190）。
        if outer.depends_on_order()
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
            view_name,
            column_aliases,
            outer,
            session.udfs(),
        )
        .map(Some)
    }

    /// 拡張クエリの Describe 腕から呼ぶ。書き込みは一切実行せず、主クエリの結果列メタを返す。
    /// 副文を読む形は外側処理後の列メタ、実表等を読む形は読み取り文の列メタ（Execute と同じ入口）。
    pub(super) fn describe_data_modifying_cte(
        &self,
        session: &SessionState,
        stmt: &ValidatedDataModifyingCte,
        limits: &crate::result_limits::ResultLimits,
    ) -> Result<Vec<ColumnMeta>, SqlSurfaceError> {
        if let DmMain::Read(read) = &stmt.main {
            let parsed = super::ParsedSql::Statement((**read).clone());
            return self
                .describe_parsed_in_session_impl(session, &parsed, false, &[], limits)?
                .ok_or_else(|| SqlSurfaceError::Internal {
                    detail: "internal error".to_string(),
                });
        }
        let plan = self
            .plan_data_modifying_outer(session, stmt, limits)?
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            })?;
        Ok(plan.columns().to_vec())
    }

    /// 副文が 2 つ以上ある文で、副文の対象表が `FOREIGN KEY` の参照アクション（`CASCADE`／`SET NULL`／
    /// `SET DEFAULT`）の参照先になっている場合は、書き込み前に拒否する。参照アクションが書く子行は
    /// 文内変更集合へ記録されず、後の副文が先の副文の変更した行を再び変更してしまうため、順序を
    /// 保証できない形は実行しない（fail-closed。Issue #1877）。
    fn reject_unordered_referential_actions(
        &self,
        write_txn: &redb::WriteTransaction,
        stmt: &ValidatedDataModifyingCte,
    ) -> Result<(), SqlSurfaceError> {
        if stmt.dm_ctes.len() < 2 {
            return Ok(());
        }
        for body in &stmt.dm_ctes {
            let referencing =
                crate::catalog::referencing_foreign_keys_in_txn(write_txn, &body.target_table)
                    .map_err(|_| SqlSurfaceError::Internal {
                        detail: "internal error".to_string(),
                    })?;
            let has_action = referencing.iter().any(|(_, fk)| {
                fk.on_delete() != crate::catalog::ReferentialAction::NoAction
                    || fk.on_update() != crate::catalog::ReferentialAction::NoAction
            });
            if has_action {
                return Err(SqlSurfaceError::feature_not_supported(
                    "several data-modifying CTEs on a table with FOREIGN KEY referential actions are not supported yet",
                ));
            }
        }
        Ok(())
    }

    /// 段 A: 副文の `WHERE` 内サブクエリを、最初の書き込みより前に書き込み txn の現在の状態
    /// （＝文の開始時点の状態）で定数の述語へ解決する。解決結果は段 C でフォーム関数へ渡し、フォーム関数の
    /// 内側で再び解決される（＝先の副文の変更を読む）ことを避ける。台帳ハッシュは解決前の副文から出す。
    fn pre_resolve_substatement(
        &self,
        write_txn: &redb::WriteTransaction,
        ctx: &PolicyContext,
        session: &SessionState,
        body: &DataModifyingCteBody,
    ) -> Result<PreResolved, SqlSurfaceError> {
        let target = WriteTarget::InTxn(write_txn);
        match &body.statement {
            DmlStatement::Update(form @ ValidatedUpdateForm::Predicate(_)) => {
                let schema = self.load_table_schema_for_dml(&body.target_table)?;
                Ok(PreResolved::Update(
                    self.resolve_predicate_update_subqueries(&target, ctx, session, form, &schema)?,
                ))
            }
            DmlStatement::Delete(crate::sql::allowlist::DeleteStatement::Predicate(v)) => {
                let schema = self.load_table_schema_for_dml(&body.target_table)?;
                Ok(PreResolved::Delete(
                    self.resolve_predicate_delete_subqueries(&target, ctx, session, v, &schema)?,
                ))
            }
            _ => Ok(PreResolved::NotApplicable),
        }
    }

    /// 副文 1 件を既存のフォーム関数で実行する。`RETURNING` 付きは結果行を返し、`RETURNING` なしは
    /// 書き込みだけを行って `None` を返す。`pre` は段 A で `WHERE` のサブクエリを解決済みの結果。
    fn run_data_modifying_substatement(
        &self,
        write_txn: &redb::WriteTransaction,
        ctx: &PolicyContext,
        session: &SessionState,
        statement: &DmlStatement,
        pre: PreResolved,
    ) -> Result<Option<QueryResult>, SqlSurfaceError> {
        let target = WriteTarget::InTxn(write_txn);
        let (pre_update, pre_delete) = match pre {
            PreResolved::Update(r) => (Some(r), None),
            PreResolved::Delete(r) => (None, Some(r)),
            PreResolved::NotApplicable => (None, None),
        };
        match statement {
            DmlStatement::Insert(v) => {
                let lookup = super::InsertSchemaLookup::new(&self.storage);
                if v.returning.is_some() {
                    self.execute_insert_returning_form(target, ctx, v, &lookup)
                        .map(|o| Some(o.result))
                } else {
                    self.execute_insert_form(target, ctx, v, &lookup)
                        .map(|_| None)
                }
            }
            DmlStatement::Update(form) => {
                let has_returning = match form {
                    ValidatedUpdateForm::Single(v) => v.returning.is_some(),
                    ValidatedUpdateForm::Predicate(v) => v.returning().is_some(),
                };
                if has_returning {
                    self.execute_update_returning_form_pre(target, ctx, session, form, pre_update)
                        .map(|o| Some(o.result))
                } else {
                    self.execute_predicate_update_form_pre(target, ctx, session, form, pre_update)
                        .map(|_| None)
                }
            }
            DmlStatement::Delete(crate::sql::allowlist::DeleteStatement::SingleRow(v)) => {
                if v.returning.is_some() {
                    self.execute_delete_returning_form(target, ctx, v)
                        .map(|o| Some(o.result))
                } else {
                    self.execute_delete_form(target, ctx, v).map(|_| None)
                }
            }
            DmlStatement::Delete(crate::sql::allowlist::DeleteStatement::Predicate(v)) => {
                if v.returning().is_some() {
                    self.execute_predicate_delete_returning_form_pre(
                        target, ctx, session, v, pre_delete,
                    )
                    .map(|o| Some(o.result))
                } else {
                    self.execute_predicate_delete_form_pre(target, ctx, session, v, pre_delete)
                        .map(|_| None)
                }
            }
        }
    }

    /// データ変更 CTE を含む文の実行本体。手順（エラーの優先順位）は先頭の `//!` を参照。戻り値は
    /// （結果, 書き込んだ表名の一覧）で、呼び出し元（明示・暗黙トランザクションの腕）が全表へ
    /// `mark_written` する。
    pub(super) fn execute_data_modifying_cte(
        &self,
        write_txn: &redb::WriteTransaction,
        ctx: &PolicyContext,
        session: &mut SessionState,
        stmt: &ValidatedDataModifyingCte,
    ) -> Result<(SqlOutcome, Vec<String>), SqlSurfaceError> {
        let limits = self.result_limits;
        // 外側の束縛エラー（未知列・曖昧な列・`LIMIT` 範囲・照合）は何かを書く前に確定させる。
        self.plan_data_modifying_outer(session, stmt, &limits)?;
        // 順序を保証できない形（参照アクションを持つ表に対する複数副文）は書き込み前に拒否する。
        self.reject_unordered_referential_actions(write_txn, stmt)?;

        let ledger_table = stmt.ledger_table();
        crate::sql::exec::precheck_insert_select_ledger(
            &WriteTarget::InTxn(write_txn),
            ctx,
            ledger_table,
            stmt.operation_id.as_ref(),
            self.ledger_mode,
            &stmt.statement_hash,
        )?;

        // 段 A: 全副文の `WHERE` サブクエリを書き込み前に解決する。
        let resolved: Vec<PreResolved> = {
            let _ledger_scope = SubstatementLedgerScope::enter(self.ledger_mode);
            stmt.dm_ctes
                .iter()
                .map(|body| self.pre_resolve_substatement(write_txn, ctx, session, body))
                .collect::<Result<_, _>>()?
        };

        // 段 B: 主クエリが実表等を読む形は、副文の書き込み前（文の開始時点の状態）に実体化する。
        // 共有 write txn を読み取り源にするため、明示トランザクションの先行文の未 commit の書き込みは
        // 見え、本文の副文の書き込みは見えない。
        let read_outcome: Option<SqlOutcome> = match &stmt.main {
            DmMain::Read(read) => Some(self.execute_read_statement_with_limits(
                ctx,
                session,
                (**read).clone(),
                write_txn,
                limits,
            )?),
            DmMain::Referenced { .. } => None,
        };

        // 段 C: 副文を定義順にすべて実行する（参照されない副文・`RETURNING` なしの副文を含む）。
        let mut written: Vec<String> = Vec::new();
        let mut referenced_result: Option<QueryResult> = None;
        {
            let _ledger_scope = SubstatementLedgerScope::enter(self.ledger_mode);
            let writes_scope = crate::tenant::statement_writes::StatementWritesScope::enter();
            for (idx, (body, pre)) in stmt.dm_ctes.iter().zip(resolved).enumerate() {
                let outcome = self.run_data_modifying_substatement(
                    write_txn,
                    ctx,
                    session,
                    &body.statement,
                    pre,
                )?;
                writes_scope.finish_substatement();
                if !written.contains(&body.target_table) {
                    written.push(body.target_table.clone());
                }
                if let DmMain::Referenced { referenced, .. } = &stmt.main {
                    if idx == *referenced {
                        referenced_result = outcome;
                    }
                }
            }
        }

        // 文全体を台帳 1 エントリで記録する（省略時・記録しない構成では何もしない）。
        crate::sql::exec::record_insert_select_ledger_only(
            WriteTarget::InTxn(write_txn),
            ctx,
            ledger_table,
            stmt.operation_id.as_ref(),
            self.ledger_mode,
            &stmt.statement_hash,
        )?;

        let DmMain::Referenced {
            view_name,
            column_aliases,
            outer,
            ..
        } = &stmt.main
        else {
            // 実表等を読む形: 段 B の結果（文の開始時点の状態）をそのまま返す。
            let outcome = read_outcome.ok_or_else(|| SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            })?;
            return Ok((outcome, written));
        };
        let result = referenced_result.ok_or_else(|| SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        })?;

        // 主クエリ: 実際の結果列に対して外側を束縛し直して適用する（事前計画と同じ入口）。
        let plan = crate::sql::view_buffered::plan_outer_parts(
            &result.columns,
            view_name,
            column_aliases,
            outer,
            session.udfs(),
        )?;
        let processed =
            crate::sql::view_buffered::apply_outer(result, &plan, self.group_by_limits)?;
        // `LIMIT` 省略・`ALL` の外側は後処理後の件数が結果行数上限を超えたら `54000`
        // （黙って切り詰めない。SQL-34）。集計は HAVING・OFFSET 適用後の件数で判定する。
        let processed = match outer {
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
