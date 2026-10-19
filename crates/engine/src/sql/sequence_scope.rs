//! SEQUENCE の採番（`nextval`／`currval`。TABLE-24・Issue #1690）が書き込み区間で使う文脈を
//! 運ぶスレッドローカルのスコープと、セッション単位の `currval` 状態。
//!
//! 責務境界: `core.rs` が INSERT 系の文を実行する間だけ [`SequenceScope`] を張る
//! （`time_fn::DefaultClockScope`・`view_write_scope::ViewWriteScope` と同じ RAII 型）。
//! スコープが運ぶのは次の 2 つで、読み手は `tenant` の INSERT 書き込み
//! （`tenant::assign_sequence_values`）だけである。
//!
//! - プロセス内アロケータ（`catalog::sequence::SequenceAllocator`）: 明示トランザクションの
//!   `WriteTarget::InTxn` は `Storage` を持たないため、スコープ経由で受け取る。
//! - セッションの `currval` 状態（[`SessionSequenceState`]）: `SessionState` が接続単位で持つ
//!   `Arc` を共有する。BEGIN 時の `SessionState` のコピーは `Arc` を共有するので、ROLLBACK で
//!   `SessionState` を巻き戻しても `currval` は消えない（PostgreSQL と同じ）。
//!
//! スコープが無い経路（COPY・ファイル形 INSERT・分割実行・NoSQL 等）では `Autocommit` の
//! `Storage` のアロケータだけを使い、セッション状態は無い（`currval` は `0A000`）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::catalog::sequence::SequenceAllocator;

/// セッションが保持する `currval` 用の状態（SEQUENCE の内部 ID → このセッションが直近に
/// `nextval` で得た値）。接続単位の値型で、他接続・他テナントへ漏れる経路は構造上存在しない。
#[derive(Debug, Default, Clone)]
pub(crate) struct SessionSequenceState {
    currval: HashMap<u64, i64>,
    /// 直近に `nextval` が成功した SEQUENCE（`lastval` 用の器。意味の設計は #1691）。
    last: Option<u64>,
}

/// セッション内に保持する SEQUENCE の件数上限（DoS 対策。超過は `54000`）。
pub(crate) const MAX_SESSION_SEQUENCES: usize = crate::catalog::sequence::MAX_SEQUENCES;

impl SessionSequenceState {
    /// `nextval` が成功した値を記録する。件数上限を超える新規 SEQUENCE は `false`（拒否）。
    pub(crate) fn record_nextval(&mut self, sequence_id: u64, value: i64) -> bool {
        if !self.currval.contains_key(&sequence_id) && self.currval.len() >= MAX_SESSION_SEQUENCES {
            return false;
        }
        self.currval.insert(sequence_id, value);
        self.last = Some(sequence_id);
        true
    }

    /// このセッションの `currval`（`nextval` 未呼び出しは `None`）。
    pub(crate) fn currval(&self, sequence_id: u64) -> Option<i64> {
        self.currval.get(&sequence_id).copied()
    }
}

/// セッション単位で共有する `currval` 状態の共有ハンドル。
pub(crate) type SharedSessionSequences = Arc<Mutex<SessionSequenceState>>;

/// スコープが運ぶ文脈（アロケータとセッション状態）。
#[derive(Clone)]
pub(crate) struct SequenceContext {
    pub(crate) allocator: Arc<SequenceAllocator>,
    pub(crate) session: Option<SharedSessionSequences>,
}

thread_local! {
    static CURRENT: RefCell<Option<SequenceContext>> = const { RefCell::new(None) };
    /// 直近の書き込みが払い出した値（`RETURNING` が投影し直すための受け渡し。
    /// `(行番号, 列位置, 値)`）。`take_assigned` が読んで空にする。
    static ASSIGNED: RefCell<Vec<(usize, usize, i64)>> = const { RefCell::new(Vec::new()) };
}

/// INSERT 系の文の実行区間を表す RAII ガード。drop で直前の状態へ戻す（入れ子・panic でも
/// 他の文脈へ漏れない）。
pub(crate) struct SequenceScope {
    prev: Option<SequenceContext>,
}

impl SequenceScope {
    /// 現スレッドへ文脈を張る。
    pub(crate) fn enter(
        allocator: Arc<SequenceAllocator>,
        session: SharedSessionSequences,
    ) -> Self {
        let ctx = SequenceContext {
            allocator,
            session: Some(session),
        };
        let prev = CURRENT.with(|c| c.replace(Some(ctx)));
        // 前の文の受け渡し値を持ち越さない。
        ASSIGNED.with(|a| a.borrow_mut().clear());
        SequenceScope { prev }
    }
}

impl Drop for SequenceScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        CURRENT.with(|c| *c.borrow_mut() = prev);
        ASSIGNED.with(|a| a.borrow_mut().clear());
    }
}

/// 現スレッドの文脈（スコープ外は `None`）。
pub(crate) fn current() -> Option<SequenceContext> {
    CURRENT.with(|c| c.borrow().clone())
}

/// 書き込みが払い出した値を記録する（`RETURNING` 用）。
/// スコープ外（NoSQL 等）では記録しない（読み手が居らず、スレッドローカルが際限なく増えるため）。
pub(crate) fn record_assigned(row: usize, col: usize, value: i64) {
    if CURRENT.with(|c| c.borrow().is_none()) {
        return;
    }
    ASSIGNED.with(|a| a.borrow_mut().push((row, col, value)));
}

/// 記録済みの払い出し値を取り出して空にする。
pub(crate) fn take_assigned() -> Vec<(usize, usize, i64)> {
    ASSIGNED.with(|a| std::mem::take(&mut *a.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_is_restored_on_drop_and_nests() {
        assert!(current().is_none());
        let alloc = Arc::new(SequenceAllocator::default());
        let outer = SequenceScope::enter(Arc::clone(&alloc), Arc::default());
        assert!(current().is_some());
        {
            let _inner = SequenceScope::enter(Arc::clone(&alloc), Arc::default());
            assert!(current().is_some());
        }
        assert!(current().is_some());
        drop(outer);
        assert!(current().is_none());
    }

    #[test]
    fn session_state_records_and_bounds() {
        let mut s = SessionSequenceState::default();
        assert_eq!(s.currval(1), None);
        assert!(s.record_nextval(1, 5));
        assert_eq!(s.currval(1), Some(5));
        assert!(s.record_nextval(1, 6));
        assert_eq!(s.currval(1), Some(6));
        assert_eq!(s.last, Some(1));
    }

    #[test]
    fn assigned_values_are_taken_once() {
        let _scope = SequenceScope::enter(Arc::new(SequenceAllocator::default()), Arc::default());
        let _ = take_assigned();
        record_assigned(0, 1, 7);
        assert_eq!(take_assigned(), vec![(0, 1, 7)]);
        assert!(take_assigned().is_empty());
    }
}
