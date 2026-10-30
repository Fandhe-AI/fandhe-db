//! トリガ本体が行を書くための狭い窓口（TABLE-45・Issue #1724。実行モデルの D5・D9・D10）。
//!
//! 発火側（`tenant/trigger_insert.rs`）が、外側の文と同じ write txn・同じ呼び出しテナントの
//! `PolicyContext` を借用して作り、[`TriggerCall`](super::call::TriggerCall) に載せてトリガ関数の実体へ
//! 渡す。実体は raw な `redb::WriteTransaction` を持たず、この窓口の [`TriggerWriteHandle::insert_row`]
//! だけで書く。
//!
//! 契約（fail-closed）:
//! - 書き込みは SQL `INSERT` と同じ `tenant::insert_typed_row_unchecked` を通る（第 2 の書き込み経路を
//!   作らない）。呼び出し元の `PolicyContext` を差し替える手段は無く、行キーは常に呼び出しテナントの
//!   `(tenant_id, rowid)` になる（他テナントの行は構造上書けない）
//! - 台帳は `Disabled`（外側の文の 1 エントリだけが台帳に載る。D10。共通化は #2226 の担当）
//! - 書き込み先の表に INSERT トリガがあれば `0A000`（入れ子の発火は深さ上限〔#1727〕が入るまで行わない）
//! - 表名は裸の識別子だけ受理し、SCHEMA 配下の内部キー（`#<id>.<name>`）は不在と同じ応答で拒否する
//!   （内部 ID から他テナントの SCHEMA の表を指せない。本番の SCHEMA 修飾の解決は #2226）
//! - 書いた表名を記録し、発火側が遅延 FK の検査対象にする
//!
//! 本番の結線（PL/pgSQL の SQL 文をこの窓口経由で実行するホスト）は #2226・#2228 の担当。

use std::cell::RefCell;

use crate::catalog::TableSchema;
use crate::policy::PolicyContext;
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::tenant::{TenantWriteError, WriteTarget};

/// トリガ内書き込みの窓口。フィールドはすべて非公開で、構築は発火側（`tenant`）だけが行う。
pub(crate) struct TriggerWriteHandle<'a> {
    write_txn: &'a redb::WriteTransaction,
    ctx: &'a PolicyContext,
    written_tables: RefCell<Vec<String>>,
}

impl<'a> TriggerWriteHandle<'a> {
    /// 外側の文の write txn と呼び出しテナントの文脈を借用して作る。
    pub(crate) fn new(write_txn: &'a redb::WriteTransaction, ctx: &'a PolicyContext) -> Self {
        Self {
            write_txn,
            ctx,
            written_tables: RefCell::new(Vec::new()),
        }
    }

    /// 表 `table`（裸の識別子）へ 1 行（`values` は `schema.columns` と同じ幅・順）を、呼び出し
    /// テナントの行として書く。`id` は従来方式の表の利用者指定 id（採番方式・移行済みの表では
    /// `None` で払い出す）。制約検査・索引同期・世代の更新は SQL `INSERT` と同じ経路で行う。
    // 実体（#2226・#2228）が使うまでは、本クレート内ではテストだけが呼ぶ。
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn insert_row(
        &self,
        table: &str,
        id: Option<u64>,
        values: &[Value],
    ) -> Result<(), SqlSurfaceError> {
        if table.starts_with('#') {
            // 内部キーは窓口から受けない（不在と同じ応答）。
            return Err(SqlSurfaceError::UndefinedTable {
                name: table.to_string(),
            });
        }
        let schema: TableSchema =
            crate::catalog::require_table_schema_write_any_identity(self.write_txn, table)
                .map_err(|e| crate::sql::exec::map_insert_write_error(TenantWriteError::from(e)))?;
        let (row_id, id_omitted) = match (id, schema.is_migrated_id(), schema.is_allocated()) {
            // 採番方式は利用者指定の id を持たない（呼び出し元の id は仮値）。
            (_, _, true) => (0, false),
            (Some(id), _, false) => (id, false),
            (None, true, false) => (0, true),
            (None, false, false) => {
                return Err(SqlSurfaceError::InvalidInput {
                    detail: "row id is required for this table".to_string(),
                })
            }
        };
        crate::tenant::insert_typed_row_unchecked(
            WriteTarget::InTxn(self.write_txn),
            table,
            self.ctx,
            row_id,
            crate::storage::Visibility::Private,
            values,
            crate::recovery::ledger::LedgerWrite::Disabled,
            None,
            &[],
            id_omitted,
            // 入れ子の発火は行わない（書き込み先に INSERT トリガがあれば `0A000`）。
            None,
        )
        .map_err(crate::sql::exec::map_insert_write_error)?;
        let mut written = self.written_tables.borrow_mut();
        if !written.iter().any(|t| t == table) {
            written.push(table.to_string());
        }
        Ok(())
    }

    /// これまでにトリガが書いた表名（重複なし。書いた順）。
    pub(crate) fn written_tables(&self) -> Vec<String> {
        self.written_tables.borrow().clone()
    }
}
