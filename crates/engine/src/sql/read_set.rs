//! 明示トランザクションが writer 取得前に読んだ基底表の集合（SQL-38・TASK-333・Issue #2404。
//! 設計: `docs/design/explicit-transaction.md` 1a-4）。
//!
//! 役割: `ActiveTxn`（`sql::transaction`）が持つ「読み取り集合」の型 [`ReadSet`] と、文の実行区間だけ
//! 記録を有効にする thread-local の RAII 区間 [`ReadSetScope`] を提供する。#2405 の `40001` 判定が
//! writer 取得時にこの集合と commit 記録（`storage::commit_log`）を突き合わせる。
//!
//! 記録の方式: AST から参照表を集めない（ビュー・CTE・部分クエリ・PL/pgSQL の展開を AST で追うと
//! 取りこぼしが fail-open になる）。代わりに、読み取りが実行時に必ず通る共通点で記録する。
//!
//! - `catalog::user_rows_table_name`（行テーブルに触れる経路が必ず通る名前の生成点）
//! - 関係の解決点（`catalog::get_table_schema_in_txn_impl`。行を読まない `EXPLAIN` とビュー名を拾う）
//! - カタログ全体を列挙する読み取り（`catalog::list_tables_in_txn`）はワイルドカード
//!
//! いずれも読み取りを過大に見積もる側（偽陽性だけが増え偽陰性は生じない）に倒れる。可視性の
//! フィルタは書き込み側に掛かる（1a-6）ため、集合が大きくなっても他テナントの存在情報は漏れない。
//!
//! 区間は `core.rs::execute_in_active_txn` が「明示トランザクションかつ writer 未取得」のときだけ張り、
//! writer を取得した時点（`SessionTransaction::ensure_writer`）で [`disarm_current`] により確定して
//! 以降の記録を止める。区間が無いとき（autocommit・暗黙トランザクション・取得後）は何もせず、
//! 挙動は導入前とビット同一。thread-local のため、読み取りが別スレッドで表を開く経路は記録されない
//! （表の解決は文の実行スレッド上で行われる）。一時表を読む区間は [`SuspendGuard`] で記録を止める
//! （一時表はセッションローカルで他セッションの commit の影響を受けない。TABLE-44）。
//!
//! 数値の上限 [`MAX_READ_SET_TABLES`] は実装既定値。超えたらワイルドカードへ畳む（fail-closed）。

use std::cell::RefCell;
use std::collections::BTreeSet;

/// 読み取り集合が持つ表数の上限。超えたらワイルドカード（全表）へ畳む。
pub(crate) const MAX_READ_SET_TABLES: usize = 1024;

/// writer 取得前に読んだ基底表の集合。表名は ASCII 小文字化して持つ。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReadSet {
    tables: BTreeSet<Box<str>>,
    /// 参照先を確定できない読み取り（カタログ全体の列挙・上限超過）。全表を読んだ扱い。
    wildcard: bool,
}

#[cfg_attr(not(test), allow(dead_code))]
impl ReadSet {
    /// 表を 1 つ加える。上限を超えたらワイルドカードへ畳む。
    pub(crate) fn record(&mut self, table: &str) {
        if self.wildcard {
            return;
        }
        let key = table.to_ascii_lowercase();
        if self.tables.contains(key.as_str()) {
            return;
        }
        if self.tables.len() >= MAX_READ_SET_TABLES {
            self.set_wildcard();
            return;
        }
        self.tables.insert(key.into_boxed_str());
    }

    /// 全表を読んだ扱いにする（保持していた表名は不要になるので捨てる）。
    pub(crate) fn set_wildcard(&mut self) {
        self.wildcard = true;
        self.tables.clear();
    }

    /// 別の集合を取り込む（文ごとの区間の結果を `ActiveTxn` の集合へ足す）。
    pub(crate) fn merge(&mut self, other: ReadSet) {
        if other.wildcard {
            self.set_wildcard();
            return;
        }
        for t in other.tables {
            self.record(&t);
        }
    }

    /// `table` を読んだか。ワイルドカードなら常に真。
    pub(crate) fn contains(&self, table: &str) -> bool {
        self.wildcard || self.tables.contains(table.to_ascii_lowercase().as_str())
    }

    pub(crate) fn is_wildcard(&self) -> bool {
        self.wildcard
    }

    pub(crate) fn is_empty(&self) -> bool {
        !self.wildcard && self.tables.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.tables.len()
    }
}

#[derive(Default)]
struct ScopeState {
    set: ReadSet,
    /// 一時表の区間などで記録を止めている入れ子の深さ。
    suspended: u32,
}

thread_local! {
    static SCOPE: RefCell<Option<ScopeState>> = const { RefCell::new(None) };
}

/// 文の実行区間だけ現スレッドへ読み取り集合の記録を張る RAII ガード。drop で直前の状態へ戻す
/// （panic・早期 return でも他の文へ漏れない）。
pub(crate) struct ReadSetScope {
    prev: Option<ScopeState>,
}

impl ReadSetScope {
    /// 空の集合で区間を開始する。
    pub(crate) fn enter() -> Self {
        let prev = SCOPE.with(|s| s.borrow_mut().replace(ScopeState::default()));
        ReadSetScope { prev }
    }

    /// 区間の結果を取り出す。[`disarm_current`] で既に確定済みなら空。
    pub(crate) fn finish(self) -> ReadSet {
        SCOPE.with(|s| {
            s.borrow_mut()
                .as_mut()
                .map(|st| std::mem::take(&mut st.set))
                .unwrap_or_default()
        })
    }
}

impl Drop for ReadSetScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        SCOPE.with(|s| {
            *s.borrow_mut() = prev;
        });
    }
}

/// 現在の区間の内容を取り出して以降の記録を止める（writer 取得時の確定）。区間が無ければ `None`。
/// 区間の ガード自身は残り、drop で直前の状態へ戻す。
pub(crate) fn disarm_current() -> Option<ReadSet> {
    SCOPE.with(|s| s.borrow_mut().take().map(|st| st.set))
}

/// 現在の区間へ表の読み取りを記録する。区間が無い・止めているときは何もしない。
pub(crate) fn record(table: &str) {
    SCOPE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            if st.suspended == 0 {
                st.set.record(table);
            }
        }
    });
}

/// 現在の区間へ「全表を読んだ」を記録する（カタログ全体の列挙など）。
pub(crate) fn record_wildcard() {
    SCOPE.with(|s| {
        if let Some(st) = s.borrow_mut().as_mut() {
            if st.suspended == 0 {
                st.set.set_wildcard();
            }
        }
    });
}

/// 一時表を扱う区間で記録を止める RAII ガード（入れ子可）。
pub(crate) struct SuspendGuard {
    active: bool,
}

impl SuspendGuard {
    pub(crate) fn enter() -> Self {
        let active = SCOPE.with(|s| {
            if let Some(st) = s.borrow_mut().as_mut() {
                st.suspended = st.suspended.saturating_add(1);
                true
            } else {
                false
            }
        });
        SuspendGuard { active }
    }
}

impl Drop for SuspendGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        SCOPE.with(|s| {
            if let Some(st) = s.borrow_mut().as_mut() {
                st.suspended = st.suspended.saturating_sub(1);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_only_inside_a_scope_and_lowercases() {
        record("t0");
        let scope = ReadSetScope::enter();
        record("Orders");
        record("orders");
        record("users");
        let set = scope.finish();
        assert_eq!(set.len(), 2);
        assert!(set.contains("ORDERS"));
        assert!(!set.contains("other"));
        // 区間の外は記録しない。
        record("after");
        let scope = ReadSetScope::enter();
        assert!(scope.finish().is_empty());
    }

    #[test]
    fn exceeding_the_table_cap_collapses_to_wildcard() {
        let scope = ReadSetScope::enter();
        for i in 0..=MAX_READ_SET_TABLES {
            record(&format!("t{i}"));
        }
        let set = scope.finish();
        assert!(set.is_wildcard());
        assert!(set.contains("anything"));
        assert_eq!(set.len(), 0);
    }

    #[test]
    fn wildcard_is_sticky_and_merges() {
        let scope = ReadSetScope::enter();
        record_wildcard();
        record("a");
        let set = scope.finish();
        assert!(set.is_wildcard());
        let mut total = ReadSet::default();
        total.record("x");
        total.merge(set);
        assert!(total.is_wildcard());
    }

    #[test]
    fn disarm_stops_recording_and_returns_the_content() {
        let scope = ReadSetScope::enter();
        record("before");
        let taken = disarm_current().expect("scope");
        assert!(taken.contains("before"));
        record("after");
        assert!(scope.finish().is_empty());
    }

    #[test]
    fn suspend_guard_pauses_recording_and_nests() {
        let scope = ReadSetScope::enter();
        {
            let _a = SuspendGuard::enter();
            {
                let _b = SuspendGuard::enter();
                record("temp1");
            }
            record("temp2");
        }
        record("persistent");
        let set = scope.finish();
        assert!(set.contains("persistent"));
        assert!(!set.contains("temp1"));
        assert!(!set.contains("temp2"));
    }

    #[test]
    fn nested_scope_restores_the_outer_one() {
        let outer = ReadSetScope::enter();
        record("outer");
        {
            let inner = ReadSetScope::enter();
            record("inner");
            assert!(inner.finish().contains("inner"));
        }
        record("outer2");
        let set = outer.finish();
        assert!(set.contains("outer") && set.contains("outer2"));
        assert!(!set.contains("inner"));
    }
}
