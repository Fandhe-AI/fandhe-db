//! 外側の文の行ループが進行中の表を記録する区間（TABLE-45・Issue #3187。実行モデルの D6 の暫定ガード）。
//!
//! 外側の `INSERT`／`UPDATE`／`DELETE` が行ごとに BEFORE ROW トリガを呼んでいる間、その表の候補行・
//! 書いた行は文の内部状態（候補 id・書いた行の FK 検査対象・AFTER ROW のキュー）に取り込まれている。
//! この間にトリガ本体が同じ表の行を `DELETE` すると、未処理の候補行が消えて内部エラーになる、処理中の行が
//! 書き戻しで復活する、書いた行が消えて後続の検査・キューと不整合になる、といった状態になる。
//!
//! PostgreSQL は `27000`（triggered data change violation）で拒否するが、本クレートの公開エラー分類に
//! 対応する variant が無い（追加は破壊的変更）。そこで行単位の検出は行わず、行ループ区間の表への本体の
//! `DELETE` を固定文言の `0A000` で拒否する（fail-closed。既知の差は `docs/design/trigger-update-delete.md`）。
//!
//! 発火側（`tenant/trigger_insert.rs`・`tenant/trigger_dml.rs` の行ループ）が [`RowLoopGuard::enter`] で表を
//! 積み、`core::plpgsql_trigger_host` の本体の `DELETE` が [`is_active`] で照会する。ループ後の AFTER ROW・
//! AFTER STATEMENT と BEFORE STATEMENT、別表への書き込みは区間の外なので拒否しない。同一スレッドで同期的に
//! 入れ子になる前提（`trigger::depth` と同じ）で、drop（早期 return・panic を含む）で必ず戻る。

use std::cell::RefCell;

thread_local! {
    /// 行ループが進行中の `(tenant_id, 表キー)`（入れ子の順に積む）。
    static ACTIVE: RefCell<Vec<(String, String)>> = const { RefCell::new(Vec::new()) };
}

/// 行ループ区間を張る RAII ガード。drop で積んだ 1 件を外す。
pub(crate) struct RowLoopGuard {
    _private: (),
}

impl RowLoopGuard {
    /// `(tenant, table)` の行ループ区間に入る。
    pub(crate) fn enter(tenant: &str, table: &str) -> Self {
        ACTIVE.with(|a| a.borrow_mut().push((tenant.to_string(), table.to_string())));
        Self { _private: () }
    }
}

impl Drop for RowLoopGuard {
    fn drop(&mut self) {
        ACTIVE.with(|a| {
            a.borrow_mut().pop();
        });
    }
}

/// `(tenant, table)` の行ループが進行中か。表名は大文字小文字を区別せず比較する（誤って許す側に倒さない）。
pub(crate) fn is_active(tenant: &str, table: &str) -> bool {
    ACTIVE.with(|a| {
        a.borrow()
            .iter()
            .any(|(t, n)| t == tenant && n.eq_ignore_ascii_case(table))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nests_and_restores_on_drop() {
        assert!(!is_active("t1", "a"));
        let g1 = RowLoopGuard::enter("t1", "a");
        assert!(is_active("t1", "A"));
        assert!(!is_active("t2", "a"));
        assert!(!is_active("t1", "b"));
        {
            let _g2 = RowLoopGuard::enter("t1", "b");
            assert!(is_active("t1", "b"));
        }
        assert!(!is_active("t1", "b"));
        assert!(is_active("t1", "a"));
        drop(g1);
        assert!(!is_active("t1", "a"));
    }
}
