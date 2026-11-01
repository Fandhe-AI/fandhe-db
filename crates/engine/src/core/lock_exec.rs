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
        Ok(crate::sql::SqlOutcome::Lock)
    }
}
