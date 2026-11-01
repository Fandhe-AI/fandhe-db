//! `LOCK [TABLE]` の実行（SQL-38・Issue #1958）。
//!
//! 呼び出し元は `core.rs::execute_in_active_txn` の `ParsedSql::Lock` 腕だけ（トランザクション外は
//! 入口が `25P01` で拒否する）。単一ライタ（TABLE-3）のため、対象表の存在確認の後に
//! [`SessionTransaction::ensure_writer`] で writer を確保して終了まで保持する。行・台帳・世代には
//! 触れず、`mark_written` も呼ばない（dirty にしない）。エラー時の `Failed` 遷移は呼び出し元が担う。
//! 存在しない表と他テナントの表は同一の応答にする（RLS-12。名前の解決は parse 時に tenant-scoped な
//! 解決器が済ませ、ここでは内部キー・名前を応答に含めない）。

use super::EngineCore;
use crate::sql::allowlist::{SqlSurfaceError, TableLookup};
use crate::sql::lock_table::ValidatedLockTable;
use crate::sql::transaction::{SessionTransaction, TxnAccessMode};

impl EngineCore {
    /// 開いている Tx の上で `LOCK` を実行する。
    pub(crate) fn execute_lock_in_active_txn(
        &self,
        txn: &mut SessionTransaction<'_>,
        stmt: &ValidatedLockTable,
    ) -> Result<crate::sql::SqlOutcome, SqlSurfaceError> {
        // 防御（fail-closed）: 読み取り専用 Tx は #1933 が `25006` へ置き換えるまで書き込みロックを拒否する。
        if txn.access_mode() == Some(TxnAccessMode::ReadOnly) {
            return Err(SqlSurfaceError::feature_not_supported(
                "LOCK TABLE in a read only transaction is not supported yet",
            ));
        }
        for name in &stmt.table_names {
            if !TableLookup::table_exists(&self.storage, name)? {
                // ビュー・索引など表でないものは `42809`、不在は `42P01`（INSERT／TRUNCATE と同じ分類）。
                return Err(
                    self.reclassify_write_to_view_error(SqlSurfaceError::undefined_table(
                        name.clone(),
                    )),
                );
            }
        }
        txn.ensure_writer_with(stmt.nowait)?;
        // writer 待機中に別セッションの DROP TABLE が確定しうるため、取得後に保持中の writer の
        // カタログで全対象表を再検証する（fail-closed。不在は取得前と同じ `42P01`／`42809`）。
        let write_txn = txn.write_txn().ok_or_else(|| SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        })?;
        for name in &stmt.table_names {
            let exists = crate::catalog::table_exists_in_txn(write_txn, name)
                .map_err(crate::catalog::table_lookup_error)?;
            if !exists {
                return Err(
                    self.reclassify_write_to_view_error(SqlSurfaceError::undefined_table(
                        name.clone(),
                    )),
                );
            }
        }
        Ok(crate::sql::SqlOutcome::Lock)
    }
}
#[cfg(test)]
mod tests {
    use crate::catalog::DropBehavior;
    use crate::kernel::CpuScalarProvider;
    use crate::policy::PolicyContext;
    use crate::sql::mode::SessionState;
    use crate::storage::{Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    use super::EngineCore;

    /// writer 待機中に別の書き込み者が `DROP TABLE` を確定した場合、取得後の再検証で `42P01`
    /// になる（SQL-38・Issue #1958 のレビュー指摘。取得前の存在確認だけでは通ってしまう）。
    #[test]
    fn table_dropped_while_waiting_for_writer_is_42p01() {
        let path = unique_db_path("lock-exec-drop-race");
        let _guard = CleanupGuard(path.clone());
        let core = EngineCore::from_storage(
            Storage::open(&path).expect("open storage"),
            Box::new(CpuScalarProvider),
        );
        let ctx = PolicyContext::with_visibilities("t1", [Visibility::Public, Visibility::Private])
            .expect("tenant");
        let mut ddl_session = SessionState::default();
        ddl_session.allow_ddl();
        core.execute_sql_in_session(
            &ctx,
            &mut ddl_session,
            "CREATE TABLE t (id BIGINT PRIMARY KEY)",
        )
        .expect("create");
        // 別の書き込み者が writer を保持したまま t を削除する（未確定）。
        let blocker = core
            .storage
            .begin_explicit_write_txn()
            .expect("blocker writer");
        core.storage
            .drop_tables_in_txn(&blocker, &["t".to_string()], false, DropBehavior::Restrict)
            .expect("drop in txn");
        let result = std::thread::scope(|scope| {
            let h = scope.spawn(|| {
                let mut session = SessionState::default();
                let mut txn = core.new_session_transaction();
                core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "BEGIN")
                    .expect("begin");
                // 事前の存在確認は通る（drop は未確定）。writer 待機中に blocker が確定する。
                core.execute_sql_in_txn(&ctx, &mut session, &mut txn, "LOCK TABLE t")
                    .map_err(|e| e.wire_code().to_string())
            });
            std::thread::sleep(std::time::Duration::from_millis(300));
            blocker.commit_raw_for_test().expect("commit blocker");
            h.join().expect("join")
        });
        assert_eq!(result, Err("42P01".to_string()));
    }
}
