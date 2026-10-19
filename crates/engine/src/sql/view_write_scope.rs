//! ビューへの書き込み（`INSERT INTO <view>`。TABLE-43・Issue #1671）の実行区間を表すスレッド
//! ローカルのスコープ。
//!
//! 責務境界: `core.rs` の `execute_view_insert` が、ビューを基表へ書き換えた INSERT を既存の
//! 基表書き込み経路（`execute_insert_form` 等 → `tenant::WriteTarget::with_txn`）へ流す間だけ
//! [`ViewWriteScope`] を張る。第 2 の書き込み経路を作らず、既存経路に「ビュー経由であること」を
//! 搬送するための仕組みで、`time_fn::DefaultClockScope` と同じ RAII 型。スコープは次の 2 箇所が読む。
//!
//! - `tenant::WriteTarget::with_txn`: 書き込み txn の冒頭で [`verify_in_txn`] を呼び、書き換え時点の
//!   ビュー連鎖・基表スキーマの指紋と同一 txn の最新カタログを照合する。不一致なら書き込み前に
//!   中断する（Parse／Execute の分離と並行 DDL に対する TOCTOU 回避。`CREATE OR REPLACE VIEW` の
//!   指紋照合と同じ型）。不一致時は [`ViewWriteScope::mismatched`] が真になり、呼び出し元が
//!   書き換えからやり直す。
//! - `recovery::ledger::record_in_txn_accepting`: 台帳の内容ハッシュへビュー名のドメインを被せる
//!   ([`current_view_name`])。ビュー経由の文と基表への直接の文を同じ `operation_id` で取り違えた
//!   再送を内容不一致（`22023`）として検出する。台帳のキーは基表名のまま。
//!
//! スコープの外では何も変わらない（既存の基表書き込みと台帳ハッシュはビット同一）。

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::catalog::CatalogError;

/// 現在の書き込み区間が張るビュー書き込みの情報。
struct Active {
    view_name: String,
    fingerprint: Vec<u8>,
    mismatch: Rc<Cell<bool>>,
}

thread_local! {
    static CURRENT: RefCell<Option<Active>> = const { RefCell::new(None) };
}

/// ビュー経由の書き込み区間を現スレッドへ張る RAII ガード。drop で直前の状態へ戻す。
pub(crate) struct ViewWriteScope {
    prev: Option<Active>,
    mismatch: Rc<Cell<bool>>,
}

impl ViewWriteScope {
    /// `view_name` 経由の書き込み区間に入る。`fingerprint` は書き換えに使ったカタログの指紋
    /// （[`crate::catalog::Storage::view_write_fingerprint`]）。
    pub(crate) fn enter(view_name: &str, fingerprint: Vec<u8>) -> Self {
        let mismatch = Rc::new(Cell::new(false));
        let active = Active {
            view_name: view_name.to_string(),
            fingerprint,
            mismatch: Rc::clone(&mismatch),
        };
        let prev = CURRENT.with(|c| c.replace(Some(active)));
        ViewWriteScope { prev, mismatch }
    }

    /// 区間中に書き込み txn 内の指紋照合が不一致を検出したか。
    pub(crate) fn mismatched(&self) -> bool {
        self.mismatch.get()
    }
}

impl Drop for ViewWriteScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        CURRENT.with(|c| *c.borrow_mut() = prev);
    }
}

/// 現スレッドがビュー経由の書き込み区間にいるとき、そのビュー名。
pub(crate) fn current_view_name() -> Option<String> {
    CURRENT.with(|c| c.borrow().as_ref().map(|a| a.view_name.clone()))
}

/// 書き込み txn の冒頭の指紋照合。スコープ外なら何もしない。不一致は印を立てて
/// [`CatalogError::ViewDefinitionChanged`] を返す（書き込み前のため副作用はゼロ）。
/// 指紋の再計算に失敗した場合もそのまま返し、「一致」側へは倒さない（fail-closed）。
pub(crate) fn verify_in_txn(write_txn: &redb::WriteTransaction) -> Result<(), CatalogError> {
    let Some((view_name, expected, mismatch)) = CURRENT.with(|c| {
        c.borrow().as_ref().map(|a| {
            (
                a.view_name.clone(),
                a.fingerprint.clone(),
                Rc::clone(&a.mismatch),
            )
        })
    }) else {
        return Ok(());
    };
    let actual = crate::catalog::view_write_fingerprint_in_txn(write_txn, &view_name)?;
    if actual != expected {
        mismatch.set(true);
        return Err(CatalogError::ViewDefinitionChanged(view_name));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_is_restored_on_drop_and_nests() {
        assert_eq!(current_view_name(), None);
        let outer = ViewWriteScope::enter("v1", vec![1]);
        assert_eq!(current_view_name().as_deref(), Some("v1"));
        {
            let inner = ViewWriteScope::enter("v2", vec![2]);
            assert_eq!(current_view_name().as_deref(), Some("v2"));
            assert!(!inner.mismatched());
        }
        assert_eq!(current_view_name().as_deref(), Some("v1"));
        assert!(!outer.mismatched());
        drop(outer);
        assert_eq!(current_view_name(), None);
    }
}
