//! 行ロック句つき `SELECT`（`FOR UPDATE`／`FOR NO KEY UPDATE`／`FOR SHARE`／`FOR KEY SHARE`、
//! `NOWAIT`）の実行（SQL-38・Issue #2230。構文解釈は `sql::row_lock`、設計記録は
//! `docs/design/explicit-transaction.md`）。
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
//! fail-closed の順序: `SKIP LOCKED`（`0A000`）→ writer の取得（`55P03`／`40001`）→ 読み取り。
//! writer・ゲート・台帳・世代へ触れる前に拒否できるものは先に拒否し、ロックを取れなかった文が
//! 素の `SELECT` として実行されることはない。エラー文言は固定で、tenant 名・表名を含めない。
//! 読み取り専用 Tx の `25006` は呼び出し元の許可リストが先に返す。

use super::EngineCore;
use crate::policy::PolicyContext;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;
use crate::sql::row_lock::LockingSelect;
use crate::sql::transaction::SessionTransaction;

impl EngineCore {
    /// 開いている Tx の上で行ロック句つき `SELECT` を実行する。writer は未取得なら遅延取得
    /// （#2396 の経路。`40001` 判定を含む）し、取得済みなら素通りする。dirty にはしない。
    /// エラー時の `Failed` 遷移は呼び出し元が担う。
    pub(crate) fn execute_locking_select_in_active_txn<'e>(
        &'e self,
        ctx: &PolicyContext,
        session: &mut SessionState,
        txn: &mut SessionTransaction<'e>,
        locking: &LockingSelect,
    ) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
        // `SKIP LOCKED` は writer に触れる前に `0A000`（#2231 まで）。
        let nowait = locking.locking().writer_wait()?;
        txn.ensure_writer_with(ctx, nowait)?;
        self.read_only_in_active_txn(ctx, session, txn, locking.statement().clone())
    }

    /// autocommit の行ロック句つき `SELECT` を、文単位の暗黙 Tx で実行する。待機方針は暗黙 Tx の
    /// 開始（writer 取得）の前に決める。成功時は書き込みなしとして writer を解放し、失敗時は
    /// Tx の drop で abort する（permit を取り残さない）。
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
        let nowait = locking.locking().writer_wait()?;
        let mut txn = self.new_session_transaction();
        txn.begin_implicit_with(&self.storage, session, nowait)?;
        let outcome = self.execute_parsed_in_txn_inner(ctx, session, &mut txn, parsed)?;
        txn.commit_implicit(session)?;
        Ok(outcome)
    }
}
