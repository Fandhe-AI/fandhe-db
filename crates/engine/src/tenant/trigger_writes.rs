//! トリガ本体が書いた表の記録区間（TABLE-45・Issue #1724）。
//!
//! 明示トランザクション内の `INSERT` では、遅延 FK（`INITIALLY DEFERRED`）の検査が COMMIT 時に
//! 先送りされ、その対象は `SessionTransaction::mark_written` した表だけになる。AFTER ROW トリガが
//! 書いた別表を `mark_written` しないと、遅延 FK 違反が COMMIT で見逃される（fail-open）。
//!
//! `core` の `execute_in_active_txn`（`INSERT`・`INSERT ... SELECT` の腕）がフォーム関数の呼び出しを
//! [`TriggerWrittenTablesScope`] で囲み、`tenant/trigger_insert.rs` が書いた表名を [`record`] へ渡す。
//! 呼び出しが `Ok` なら `core` が [`TriggerWrittenTablesScope::take`] で表名を取り出して
//! `mark_written` する。区間が張られていない `InTxn` 経路（ビュー経由・一時表・データ変更 CTE 等）では
//! [`record`] が `false` を返し、呼び出し側が文の終わりに遅延 FK を即時検査する（PostgreSQL より
//! 厳しいが、fail-open にはならない側に倒す）。

use std::cell::RefCell;

thread_local! {
    /// 現スレッドで有効な記録区間（無ければ `None`）。
    static WRITTEN: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// 記録区間を現スレッドへ張る RAII ガード。drop（panic・早期 return を含む）で直前の状態へ戻す。
pub(crate) struct TriggerWrittenTablesScope {
    prev: Option<Vec<String>>,
}

impl TriggerWrittenTablesScope {
    /// 区間を開始する（空の記録で始める）。
    pub(crate) fn enter() -> Self {
        let prev = WRITTEN.with(|w| w.borrow_mut().replace(Vec::new()));
        Self { prev }
    }

    /// 区間中に記録された表名を取り出す（以降の記録は空から）。
    pub(crate) fn take(&self) -> Vec<String> {
        WRITTEN.with(|w| {
            w.borrow_mut()
                .as_mut()
                .map(std::mem::take)
                .unwrap_or_default()
        })
    }
}

impl Drop for TriggerWrittenTablesScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        WRITTEN.with(|w| *w.borrow_mut() = prev);
    }
}

/// 区間が張られていれば `tables` を記録して `true`、無ければ何もせず `false`。
pub(crate) fn record(tables: &[String]) -> bool {
    WRITTEN.with(|w| match w.borrow_mut().as_mut() {
        Some(list) => {
            for t in tables {
                if !list.iter().any(|x| x == t) {
                    list.push(t.clone());
                }
            }
            true
        }
        None => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // 区間が無ければ記録せず `false`。区間内では重複なく記録し、drop で直前の状態へ戻る。
    #[test]
    fn scope_records_and_restores() {
        assert!(!record(&["a".to_string()]));
        {
            let outer = TriggerWrittenTablesScope::enter();
            assert!(record(&["a".to_string(), "a".to_string()]));
            {
                let inner = TriggerWrittenTablesScope::enter();
                assert!(record(&["b".to_string()]));
                assert_eq!(inner.take(), vec!["b".to_string()]);
            }
            assert!(record(&["c".to_string()]));
            assert_eq!(outer.take(), vec!["a".to_string(), "c".to_string()]);
        }
        assert!(!record(&["a".to_string()]));
    }
}
