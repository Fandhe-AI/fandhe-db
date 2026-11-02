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
//! 依存しないため、他テナントの行の存在・件数のオラクルにならない（SQL-38 (5)・RLS-12）。
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

    /// `SKIP LOCKED` が writer を取得できなかったときの 0 行結果。句を除いた文を、句なし `SELECT` と同じ
    /// execute 経路（明示 Tx は `read_only_in_active_txn`、autocommit は `execute_parsed_statement`）で
    /// 実行し、結果の行だけを捨てて列を返す。これにより BEGIN 時点のスナップショット・負の
    /// `LIMIT`／`OFFSET` の `22000`・`row_security = off` の `42501` など execute 時の検査を必ず通し、
    /// 検査に落ちる文が成功の空結果に化けない（fail-closed）。読んだ行は結果へ載せないため、他者保持中の
    /// 行の存在・件数は観測できない。`txn` が `Some` なら明示 Tx（writer 未取得で `BeginSnapshot`）の経路。
    fn skip_locked_empty_result<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: Option<&SessionTransaction<'e>>,
        locking: &LockingSelect,
    ) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
        let stmt = locking.statement().clone();
        let outcome = match txn {
            Some(txn) => self.read_only_in_active_txn(ctx, session, txn, stmt)?,
            None => {
                self.execute_parsed_statement(ctx, session, &super::ParsedSql::Statement(stmt))?
            }
        };
        match outcome {
            crate::sql::SqlOutcome::Query(q) => Ok(crate::sql::SqlOutcome::Query(
                crate::sql::exec::QueryResult {
                    columns: q.columns,
                    rows: Vec::new(),
                },
            )),
            _ => Err(SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            }),
        }
    }
}
