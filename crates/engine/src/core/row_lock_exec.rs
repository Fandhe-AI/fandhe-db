//! 行ロック句つき `SELECT`（`FOR UPDATE`／`FOR NO KEY UPDATE`／`FOR SHARE`／`FOR KEY SHARE`、
//! `NOWAIT`／`SKIP LOCKED`）の実行（SQL-38・Issue #2230・#2231。構文解釈は `sql::row_lock`、
//! 設計記録は `docs/design/explicit-transaction.md`）。
//!
//! 呼び出し元は `core.rs` の 2 か所だけ。明示 Tx は `execute_in_active_txn_inner` の
//! `ParsedSql::LockingSelect` 腕が [`EngineCore::execute_locking_select_in_active_txn`] を、
//! autocommit は `execute_parsed_in_session_inner` の振り分けが
//! [`EngineCore::execute_locking_select_autocommit`] を呼ぶ。
//!
//! 単一ライタ（TABLE-3）のため、行・表・強度の粒度は区別せず writer の取得として扱う
//! （`LOCK TABLE`〔`lock_exec`〕と同じ写像）。writer は明示 Tx なら COMMIT／ROLLBACK まで、
//! autocommit なら文の終わりまで保持する。取得後の読み取りは句のない `SELECT` と同じ経路
//! （`read_only_in_active_txn`）を通るため、結果と RLS の暗黙適用は同一になる。
//!
//! `SKIP LOCKED` は待たずに取得を試み、他セッションが保持していれば 0 行を返す（結果の行は捨てる）
//! （句なし `SELECT` と同じ execute 経路を通すため 22000／42501 等の検査も効く）。0 行はテナントの可視行に
//! 依存しないため、他テナントの行の存在・件数のオラクルにならない（SQL-38 (5)・RLS-12）。空結果の前に対象表の
//! `row_security = off` 拒否（`policies_apply`。RLS-13）を行う。
//! 取得できた場合は `SKIP LOCKED` を外した行ロック句と同じに扱い、`40001` 判定も効く。
//!
//! fail-closed の順序: writer の取得（`55P03`／`SKIP LOCKED` の空結果／`40001`）→ 読み取り。
//! ロックを取れなかった文が素の `SELECT` として行を返すことはない。エラー文言は固定で、
//! tenant 名・表名を含まない。読み取り専用 Tx の `25006` は呼び出し元の許可リストが先に返す。

use super::EngineCore;
use crate::policy::PolicyContext;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;
use crate::sql::row_lock::{LockingSelect, WriterWait};
use crate::sql::transaction::SessionTransaction;
use redb::ReadableDatabase;

impl EngineCore {
    /// 開いている Tx の上で行ロック句つき `SELECT` を実行する。writer は未取得なら遅延取得
    /// （#2396 の経路。`40001` 判定を含む）し、取得済みなら素通りする。dirty にはしない。
    /// `SKIP LOCKED` で他者保持中なら writer を取らず、Tx は `Active` のまま 0 行を返す。
    /// エラー時の `Failed` 遷移は呼び出し元が担う。
    pub(crate) fn execute_locking_select_in_active_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: &mut SessionTransaction<'e>,
        locking: &LockingSelect,
    ) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
        match locking.locking().writer_wait() {
            WriterWait::Block => txn.ensure_writer_with(ctx, false)?,
            WriterWait::NoWait => txn.ensure_writer_with(ctx, true)?,
            WriterWait::SkipLocked => {
                if !txn.try_ensure_writer(ctx)? {
                    return self.skip_locked_empty_result(ctx, session, Some(&*txn), locking);
                }
            }
        }
        self.read_only_in_active_txn(ctx, session, txn, locking.statement().clone())
    }

    /// autocommit の行ロック句つき `SELECT` を、文単位の暗黙 Tx で実行する。待機方針は暗黙 Tx の
    /// 開始（writer 取得）の前に決める。成功時は書き込みなしとして writer を解放し、失敗時は
    /// Tx の drop で abort する（permit を取り残さない）。`SKIP LOCKED` で他者保持中なら暗黙 Tx を
    /// 開始せず（`Idle` のまま）0 行を返し、一時表の `ON COMMIT` だけ適用する。
    pub(super) fn execute_locking_select_autocommit(
        &self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        parsed: &super::ParsedSql,
    ) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
        let super::ParsedSql::LockingSelect(locking) = parsed else {
            return Err(SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            });
        };
        let mut txn = self.new_session_transaction();
        match locking.locking().writer_wait() {
            WriterWait::Block => txn.begin_implicit_with(&self.storage, session, false)?,
            WriterWait::NoWait => txn.begin_implicit_with(&self.storage, session, true)?,
            WriterWait::SkipLocked => {
                if !txn.try_begin_implicit(&self.storage, session)? {
                    let outcome = self.skip_locked_empty_result(ctx, session, None, locking)?;
                    // autocommit の文が成功したので、一時表の `ON COMMIT` を暗黙 Tx の commit と同様に適用する。
                    session.apply_temp_on_commit()?;
                    return Ok(outcome);
                }
            }
        }
        let outcome = self.execute_parsed_in_txn_inner(ctx, session, &mut txn, parsed)?;
        txn.commit_implicit(session)?;
        Ok(outcome)
    }

    /// `SKIP LOCKED` が writer を取得できなかったときの 0 行結果。句を除いた文の列メタデータを
    /// Describe と同じ経路（`describe_parsed_in_txn_unscrubbed`／`describe_parsed_in_session_unscrubbed`。
    /// 明示 Tx は `BEGIN` 時点のスナップショット）で導出し、行なしの結果として返す。行は読まないため、
    /// 他者保持中の行の存在・件数は観測できず、全件走査も明示 Tx の read_set 取り込みも起きない。
    /// `txn` が `Some` なら明示 Tx の経路。
    fn skip_locked_empty_result<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: Option<&SessionTransaction<'e>>,
        locking: &LockingSelect,
    ) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
        // 行走査を省いても、対象表の RLS 適用判定（`row_security = off` の 42501 拒否）は空結果の前に行う
        // （RLS-13。writer の空き具合で拒否と空結果が分かれて存在のオラクルにならないよう、取得の成否に依らず同じ検査）。
        self.reject_skip_locked_row_security_off(ctx, session, txn, locking)?;
        // 行を読む本体は実行せず、Describe と同じ列メタデータ導出（検索・集計の本体を呼ばない）だけを行う。
        // 全件走査・結果バイト上限（54000）・明示 Tx の read_set 取り込みを起こさず、構文・列・
        // `LIMIT`／`OFFSET` の範囲などの束縛時検査は Describe 経路で効く。
        let parsed = super::ParsedSql::Statement(locking.statement().clone());
        // サブクエリを含む `SELECT` は Describe 経路が未解決のまま束縛して `42601` になるため、通常実行と同じ
        // 「束縛前にサブクエリを解決」を内側の評価だけ行って通す（外側の行走査は省く）。
        let columns = match self.describe_with_resolved_subqueries(ctx, session, txn, locking)? {
            Some(columns) => Some(columns),
            None => match txn {
                Some(txn) => self.describe_parsed_in_txn_unscrubbed(session, txn, &parsed)?,
                None => self.describe_parsed_in_session_unscrubbed(session, &parsed)?,
            },
        };
        match columns {
            Some(columns) => Ok(crate::sql::SqlOutcome::Query(
                crate::sql::exec::QueryResult {
                    columns,
                    rows: Vec::new(),
                },
            )),
            None => Err(SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            }),
        }
    }

    /// サブクエリ（`WHERE` の `IN`／`EXISTS`／スカラー比較、投影・式の内側のスカラーサブクエリ）を含む
    /// 広域取得・集計の列メタデータを、通常実行と同じ順序（WHERE 側の解決 → 束縛 → 投影側の解決）で
    /// 導出する。内側の評価は参照セッション自身の `ctx`（RLS 暗黙適用）で行い、外側の行走査は省く。
    /// 読み取り源は [`Self::reject_skip_locked_row_security_off`] と同じ選び方（明示 Tx は `BEGIN` 時点の
    /// スナップショット等）。対象外の文形（サブクエリなし・他の文種別）は `Ok(None)` で、呼び出し元が
    /// 従来の Describe 経路へ回す。
    fn describe_with_resolved_subqueries(
        &self,
        ctx: &PolicyContext,
        session: &SessionState,
        txn: Option<&SessionTransaction<'_>>,
        locking: &LockingSelect,
    ) -> Result<Option<Vec<crate::sql::exec::ColumnMeta>>, SqlSurfaceError> {
        use crate::sql::allowlist::Statement;
        use crate::sql::transaction::TxnReadSource;
        let stmt = locking.statement();
        let needs = match stmt {
            Statement::Scan(v) => {
                super::dml_subquery_exec::where_has_subquery(&v.where_predicates)
                    || !v.embedded_scalar_subqueries.is_empty()
                    || !v.scalar_subquery_items.is_empty()
            }
            Statement::Aggregate(_) => true,
            _ => false,
        };
        if !needs {
            return Ok(None);
        }
        let _row_security =
            crate::sql::policy_applicability::RowSecurityScope::enter(session.row_security());
        if let Some(txn) = txn {
            match txn.read_source()? {
                TxnReadSource::BeginSnapshot(snap) => {
                    return self
                        .describe_resolved_in(ctx, session, stmt, snap)
                        .map(Some);
                }
                TxnReadSource::Writer(w) => {
                    return self.describe_resolved_in(ctx, session, stmt, w).map(Some);
                }
                TxnReadSource::Committed => {}
            }
        }
        let read_txn = self
            .storage
            .db()
            .begin_read()
            .map_err(|_| SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            })?;
        self.describe_resolved_in(ctx, session, stmt, &read_txn)
            .map(Some)
    }

    /// [`Self::describe_with_resolved_subqueries`] の本体。`read_txn` 上で通常実行の `Scan`／`Aggregate` 腕
    /// （`execute_read_statement_with_limits`）と同じ解決・束縛を行い、走査の代わりに行なしの列メタデータを返す。
    fn describe_resolved_in<R: crate::storage::read_source::ReadSource>(
        &self,
        ctx: &PolicyContext,
        session: &SessionState,
        stmt: &crate::sql::allowlist::Statement,
        read_txn: &R,
    ) -> Result<Vec<crate::sql::exec::ColumnMeta>, SqlSurfaceError> {
        use crate::sql::allowlist::Statement;
        use crate::sql::subquery as sq;
        let limits = self.result_limits;
        let mut budget = self.nesting_limits.max_subquery_executions();
        let mut in_value_budget = sq::MAX_SUBQUERY_IN_VALUES;
        let correlated = crate::sql::correlated_exists::CorrelatedCtx::enabled(
            self.join_limits,
            self.correlated_limits,
        );
        match stmt {
            Statement::Scan(v) => {
                let mut validated = v.clone();
                let schema = super::table_schema_in(read_txn, &validated.table_name)?;
                validated.where_predicates = sq::resolve_where_predicates(
                    validated.where_predicates,
                    &[&schema],
                    read_txn,
                    ctx,
                    &self.storage,
                    session.udfs(),
                    self.group_by_limits,
                    self.nesting_limits,
                    limits,
                    &mut budget,
                    &mut in_value_budget,
                    &correlated,
                )?;
                if !validated.embedded_scalar_subqueries.is_empty() {
                    let embedded = std::mem::take(&mut validated.embedded_scalar_subqueries);
                    let leaves = sq::resolve_embedded_scalar_subqueries(
                        &embedded,
                        &[&schema],
                        read_txn,
                        ctx,
                        &self.storage,
                        session.udfs(),
                        self.group_by_limits,
                        self.nesting_limits,
                        limits,
                        &mut budget,
                        &mut in_value_budget,
                        &correlated,
                    )?;
                    validated.embedded_scalar_subqueries = embedded
                        .into_iter()
                        .zip(leaves)
                        .map(|(mut e, leaf)| {
                            e.resolved = Some(leaf);
                            e
                        })
                        .collect();
                }
                let projection_items = std::mem::take(&mut validated.scalar_subquery_items);
                let bound = crate::sql::parser::bind_scan_with_dummy_flags(
                    &validated,
                    &schema,
                    session.udfs(),
                    &[],
                    &limits,
                )?;
                let resolved_items = sq::resolve_scalar_projection_items(
                    &projection_items,
                    &[&schema],
                    read_txn,
                    ctx,
                    &self.storage,
                    session.udfs(),
                    self.group_by_limits,
                    self.nesting_limits,
                    limits,
                    &mut budget,
                    &mut in_value_budget,
                    &correlated,
                )?;
                // 隠しキー列（集計形の相関スカラー）がある場合は通常実行と同じ写しで列を導き、合流で取り除く。
                let hidden = sq::hidden_key_columns(&resolved_items);
                let columns = if hidden.is_empty() {
                    crate::sql::describe::scan_columns(&bound, &schema)?
                } else {
                    let mut with_keys = validated.clone();
                    sq::append_hidden_key_columns(&mut with_keys, &hidden)?;
                    let bound_with_keys = crate::sql::parser::bind_scan_with_dummy_flags(
                        &with_keys,
                        &schema,
                        session.udfs(),
                        &[],
                        &limits,
                    )?;
                    crate::sql::describe::scan_columns(&bound_with_keys, &schema)?
                };
                let merged = sq::merge_scalar_projection_items(
                    crate::sql::exec::QueryResult {
                        columns,
                        rows: Vec::new(),
                    },
                    resolved_items,
                    &hidden,
                )?;
                Ok(merged.columns)
            }
            Statement::Aggregate(v) => {
                let mut validated = v.clone();
                let schema = super::table_schema_in(read_txn, &validated.table_name)?;
                validated.where_predicates = sq::resolve_where_predicates(
                    validated.where_predicates,
                    &[&schema],
                    read_txn,
                    ctx,
                    &self.storage,
                    session.udfs(),
                    self.group_by_limits,
                    self.nesting_limits,
                    limits,
                    &mut budget,
                    &mut in_value_budget,
                    &correlated,
                )?;
                sq::resolve_aggregate_item_filters(
                    &mut validated.items,
                    validated
                        .group_by
                        .as_mut()
                        .map_or(&mut [][..], |g| g.having_aggregates.as_mut_slice()),
                    &[&schema],
                    read_txn,
                    ctx,
                    &self.storage,
                    session.udfs(),
                    self.group_by_limits,
                    self.nesting_limits,
                    limits,
                    &mut budget,
                    &mut in_value_budget,
                )?;
                let bound =
                    crate::sql::parser::bind_aggregate(&validated, &schema, session.udfs())?;
                crate::sql::describe::aggregate_columns(&bound, &schema)
            }
            _ => Err(SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            }),
        }
    }

    /// `SKIP LOCKED` の空結果の前に、文が読む各表へ `row_policy::policies_apply` を適用する。
    /// 明示 Tx は `BEGIN` 時点のスナップショット（writer 未取得のため）、それ以外は確定済みの最新状態で判定する。
    /// 適用表（RLS 有効）は行を読まないため素通りで、`row_security = off` のときだけ `42501` を返す。
    fn reject_skip_locked_row_security_off(
        &self,
        ctx: &PolicyContext,
        session: &SessionState,
        txn: Option<&SessionTransaction<'_>>,
        locking: &LockingSelect,
    ) -> Result<(), SqlSurfaceError> {
        use crate::sql::transaction::TxnReadSource;
        // `row_security` は文の実行区間に束縛された値を判定点が読むため、空結果の経路でも束縛する。
        let _row_security =
            crate::sql::policy_applicability::RowSecurityScope::enter(session.row_security());
        let mut tables = Vec::new();
        super::buffered_body_tables(locking.statement(), &mut tables);
        // 外側の FROM に現れない表（サブクエリ内側など）はトークン列の識別子候補で補う。
        for t in locking.read_tables() {
            if !tables.iter().any(|x| x.eq_ignore_ascii_case(t)) {
                tables.push(t.clone());
            }
        }
        let check =
            |src: &dyn Fn(&str) -> Result<bool, SqlSurfaceError>| -> Result<(), SqlSurfaceError> {
                for t in &tables {
                    src(t)?;
                }
                Ok(())
            };
        if let Some(txn) = txn {
            match txn.read_source()? {
                TxnReadSource::BeginSnapshot(snap) => {
                    return check(&|t| crate::sql::row_policy::policies_apply(snap, ctx, t));
                }
                TxnReadSource::Writer(w) => {
                    return check(&|t| crate::sql::row_policy::policies_apply(w, ctx, t));
                }
                TxnReadSource::Committed => {}
            }
        }
        let read_txn = self
            .storage
            .db()
            .begin_read()
            .map_err(|_| SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            })?;
        check(&|t| crate::sql::row_policy::policies_apply(&read_txn, ctx, t))
    }
}
