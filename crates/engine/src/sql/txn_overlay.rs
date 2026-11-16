//! DDL を積んだ明示・暗黙トランザクションの文の区間だけ張る、write txn 上の表スキーマの重ね見
//! （SQL-80・SQL-31・Issue #3313。設計記録: `docs/design/explicit-transaction.md`
//! 「明示トランザクション内の DDL の方式」）。
//!
//! 責務境界: 文の解析（`core.rs` の `parse_tokens_gated` 以降。`impl TableLookup for Storage` が確定済み
//! カタログから表の存在・列を引く）は、同じ Tx で作った表・変更した列を知らない。そこで
//! `core/txn_ddl.rs` の [`TxnCatalogOverlayScope::enter`] が、文の開始時に Tx が変更した表（dirty）の
//! スキーマを write txn から読んで現スレッドへ張り、`catalog.rs` の `TableLookup for Storage` と INSERT の
//! スキーマ取得（`core.rs::InsertSchemaLookup`）だけがそれを優先して見る。実行本体は write txn を直接読む
//! ため、ここで重ねるのは「解析が確定済みカタログを引く点」だけ。
//!
//! 安全性: 重ね見は張った `Storage`（永続側）にだけ効く（`Storage` の同一性をアドレスで照合する。
//! 一時表用の別 `Storage` へは効かない）。autocommit・DDL を積んでいない Tx では張らないため従来どおり。
//! 現スレッド専用で、RAII の drop で元へ戻す（大域状態ではない）。

use crate::catalog::TableSchema;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::transaction::SessionTransaction;
use crate::storage::Storage;
use std::cell::RefCell;
use std::collections::HashMap;

/// 張った重ね見（対象の `Storage` のアドレスと、Tx が変更した表のスキーマ。`None` は同じ Tx で DROP 済み）。
struct Overlay {
    storage_addr: usize,
    schemas: HashMap<String, Option<TableSchema>>,
}

thread_local! {
    static OVERLAY: RefCell<Option<Overlay>> = const { RefCell::new(None) };
}

/// [`Storage`] の同一性（アドレス）。重ね見の照合専用で、参照として使い直さない。
fn addr_of(storage: &Storage) -> usize {
    std::ptr::from_ref(storage) as usize
}

/// 重ね見の RAII ガード（drop で元の重ね見へ戻す）。
pub(crate) struct TxnCatalogOverlayScope {
    prev: Option<Overlay>,
}

impl TxnCatalogOverlayScope {
    /// `txn` が DDL を積んでいれば、変更した表（dirty）のスキーマを write txn から読んで現スレッドへ張る。
    /// 積んでいなければ `None`（何もしない）。読み取りの失敗は固定文言の内部エラー（fail-closed）。
    pub(crate) fn enter(
        storage: &Storage,
        txn: &SessionTransaction<'_>,
    ) -> Result<Option<Self>, SqlSurfaceError> {
        let internal = || SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        };
        if !txn.has_ddl() {
            return Ok(None);
        }
        let write_txn = txn.write_txn().ok_or_else(internal)?;
        let mut schemas: HashMap<String, Option<TableSchema>> = HashMap::new();
        for name in txn.dirty_tables()? {
            let entry = match crate::catalog::get_table_schema_in_txn(write_txn, &name) {
                Ok(schema) => Some(schema),
                Err(crate::catalog::CatalogError::TableNotFound(_)) => None,
                Err(_) => return Err(internal()),
            };
            schemas.insert(name, entry);
        }
        let prev = OVERLAY.with(|cell| {
            cell.borrow_mut().replace(Overlay {
                storage_addr: addr_of(storage),
                schemas,
            })
        });
        Ok(Some(Self { prev }))
    }
}

impl Drop for TxnCatalogOverlayScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        OVERLAY.with(|cell| *cell.borrow_mut() = prev);
    }
}

/// 重ね見に `name` の情報があれば返す（`Some(Some)` は存在・スキーマ、`Some(None)` は同じ Tx で DROP 済み）。
/// 重ね見が張られていない・`storage` が張った `Storage` と異なる・`name` を含まなければ `None`
/// （呼び出し元は確定済みカタログを読む）。
pub(crate) fn overlay_schema(storage: &Storage, name: &str) -> Option<Option<TableSchema>> {
    OVERLAY.with(|cell| {
        let guard = cell.borrow();
        let overlay = guard.as_ref()?;
        if overlay.storage_addr != addr_of(storage) {
            return None;
        }
        overlay.schemas.get(name).cloned()
    })
}

/// `name` の表スキーマを、重ね見があればそれ、無ければ確定済みカタログから引く
/// （`impl TableLookup for Storage` と INSERT のスキーマ取得が共有する）。
pub(crate) fn table_schema_overlaid(
    storage: &Storage,
    name: &str,
) -> crate::catalog::Result<TableSchema> {
    match overlay_schema(storage, name) {
        Some(Some(schema)) => Ok(schema),
        Some(None) => Err(crate::catalog::CatalogError::TableNotFound(
            name.to_string(),
        )),
        None => storage.get_table_schema(name),
    }
}
