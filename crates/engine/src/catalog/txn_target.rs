//! カタログ書き込みの「単独 commit」と「呼び出し元 Tx への相乗り」を 1 つの本体で表す小さな入口
//! （SQL-80・SQL-31・Issue #3316。設計記録: `docs/design/explicit-transaction.md`）。
//!
//! 役割: ポリシー・行セキュリティ・トリガ・権限・ロール等の `Storage::*` の本体は、従来「自前の write txn を
//! 開く → 検証・書き込み・世代の bump → commit」を 1 関数で行っていた。明示 Tx 内の DDL では同じ本体を
//! Tx の共有 write txn の上で実行し、commit を COMMIT まで遅らせる必要がある。本入口は第 2 の実装を作らず、
//! 本体を `FnOnce(&WriteTransaction)` として 1 つだけ持たせる。呼び出し元は `sql/*_ddl.rs`（autocommit は
//! `None`、`core/txn_security_ddl.rs` の明示 Tx は共有 write txn の `Some`）。
//!
//! 契約: `None` のとき本体が `changed == false`（no-op）を返したら commit せず write txn を破棄する
//! （従来の autocommit の挙動と同じ）。`Some` のとき commit も abort もしない（Tx の COMMIT／ROLLBACK が行う）。

use super::{convert_storage_error, CatalogError};
use crate::storage::Storage;

impl Storage {
    /// `body` を `target` の共有 write txn の上で、または `target` が `None` なら自前の write txn で実行する。
    /// `body` は成功時に結果と「カタログを変えたか」を返す。
    pub(crate) fn run_catalog_write<T, E>(
        &self,
        target: Option<&redb::WriteTransaction>,
        body: impl FnOnce(&redb::WriteTransaction) -> Result<(T, bool), E>,
    ) -> Result<T, E>
    where
        E: From<CatalogError>,
    {
        if let Some(write_txn) = target {
            return body(write_txn).map(|(v, _)| v);
        }
        let write_txn = self
            .begin_write_txn()
            .map_err(|e| E::from(convert_storage_error(e)))?;
        let (value, changed) = body(&write_txn)?;
        if changed {
            crate::recovery::commit_boundary::commit(write_txn)
                .map_err(|e| E::from(convert_storage_error(e)))?;
        }
        Ok(value)
    }
}
