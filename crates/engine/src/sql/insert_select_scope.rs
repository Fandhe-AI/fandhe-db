//! `INSERT ... SELECT` の台帳照合ハッシュを書き込み経路へ搬送するスレッドローカルのスコープ
//! （SQL-51 (5)・RECOVER-10/11・Issue #1733。ハッシュ導出は #1734 の
//! `recovery::content_hash::for_insert_select`）。
//!
//! 責務境界: `core::insert_select_exec` が内側 SELECT の結果を既存のバッチ書き込み経路
//! （`tenant::insert_typed_rows_unchecked` 等）へ渡す間だけ [`InsertSelectLedgerScope`] を張る。
//! 台帳記録の唯一の入口 `recovery::ledger::record_in_txn_accepting` が [`override_hash`] を読み、
//! 「結果行の値」ではなく「正規化済みの文」から導いたハッシュで照合・記録する（結果が再送時に
//! 変わっても同一文なら `23505`、別の文なら `22023`）。第 2 の書き込み経路を作らず、
//! `sql::view_write_scope` と同じ搬送方式を取る。
//!
//! 対象は書き込み先の表名が一致する記録だけ（FK の参照アクション等、別表への書き込みには
//! 適用しない）。スコープの外では何も変わらない（既存の台帳ハッシュはビット同一）。

use std::cell::RefCell;

use crate::recovery::content_hash::ContentHash;

struct Active {
    table: String,
    hash: ContentHash,
}

thread_local! {
    static CURRENT: RefCell<Option<Active>> = const { RefCell::new(None) };
}

/// `INSERT ... SELECT` 実行区間を現スレッドへ張る RAII ガード。drop で直前の状態へ戻す。
pub(crate) struct InsertSelectLedgerScope {
    prev: Option<Active>,
}

impl InsertSelectLedgerScope {
    /// 書き込み先 `table` への台帳記録に `hash` を使う区間に入る。
    pub(crate) fn enter(table: &str, hash: ContentHash) -> Self {
        let active = Active {
            table: table.to_string(),
            hash,
        };
        let prev = CURRENT.with(|c| c.replace(Some(active)));
        InsertSelectLedgerScope { prev }
    }
}

impl Drop for InsertSelectLedgerScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        CURRENT.with(|c| *c.borrow_mut() = prev);
    }
}

/// 現スレッドが `INSERT ... SELECT` 区間にいて、記録先が `table` のとき、台帳に使う文ハッシュ。
pub(crate) fn override_hash(table: &str) -> Option<ContentHash> {
    CURRENT.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|a| (a.table == table).then_some(a.hash))
    })
}
