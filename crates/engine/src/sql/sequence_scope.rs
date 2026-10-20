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
    /// キーは `(tenant_id, sequence_id)`。同じ `SessionState` を別テナントの `PolicyContext` で
    /// 使っても、他テナントの `nextval` の結果を `currval` で読めないようにする（テナント境界）。
    currval: HashMap<(String, u64), i64>,
    /// 直近に `nextval` が成功した `(tenant_id, sequence_id)`（`lastval` の対象。TABLE-24・Issue #1691。
    /// `setval` は変えない）。
    last: Option<(String, u64)>,
}

/// セッション内に保持する SEQUENCE の件数上限（DoS 対策。超過は `54000`）。
pub(crate) const MAX_SESSION_SEQUENCES: usize = crate::catalog::sequence::MAX_SEQUENCES;

impl SessionSequenceState {
    /// `nextval` が成功した値を記録する。件数上限を超える新規 SEQUENCE は `false`（拒否）。
    pub(crate) fn record_nextval(&mut self, tenant_id: &str, sequence_id: u64, value: i64) -> bool {
        let key = (tenant_id.to_string(), sequence_id);
        if !self.currval.contains_key(&key) && self.currval.len() >= MAX_SESSION_SEQUENCES {
            return false;
        }
        self.currval.insert(key.clone(), value);
        self.last = Some(key);
        true
    }

    /// このセッションの `currval`（`nextval` 未呼び出しは `None`）。
    pub(crate) fn currval(&self, tenant_id: &str, sequence_id: u64) -> Option<i64> {
        self.currval
            .get(&(tenant_id.to_string(), sequence_id))
            .copied()
    }

    /// 新しい SEQUENCE をこの状態へ記録できるか（件数上限の事前検査。`setval` が commit の前に
    /// 呼び、commit 後の記録が上限で失敗して「永続化済みなのにエラー」にならないようにする）。
    pub(crate) fn can_record(&self, tenant_id: &str, sequence_id: u64) -> bool {
        self.currval.len() < MAX_SESSION_SEQUENCES
            || self
                .currval
                .contains_key(&(tenant_id.to_string(), sequence_id))
    }

    /// `setval(..., true)` の値を `currval` へ記録する（TABLE-24・Issue #1691）。PostgreSQL の
    /// `setval` と同じく `lastval` の対象（`last`）は変えない。件数上限を超える新規 SEQUENCE は
    /// `false`（拒否）。`is_called = false` の `setval` は呼び出し元が本関数を呼ばない。
    pub(crate) fn record_setval(&mut self, tenant_id: &str, sequence_id: u64, value: i64) -> bool {
        let key = (tenant_id.to_string(), sequence_id);
        if !self.currval.contains_key(&key) && self.currval.len() >= MAX_SESSION_SEQUENCES {
            return false;
        }
        self.currval.insert(key, value);
        true
    }

    /// `lastval` の対象 `(sequence_id, 値)`。呼び出し元テナントのものだけを返し、`nextval` 未呼び出し・
    /// 他テナントが最後に使った場合は `None`（`55000`。テナント境界）。
    pub(crate) fn lastval(&self, tenant_id: &str) -> Option<(u64, i64)> {
        let (t, id) = self.last.as_ref()?;
        if t != tenant_id {
            return None;
        }
        let value = self.currval.get(&(t.clone(), *id)).copied()?;
        Some((*id, value))
    }

    /// 消えた SEQUENCE（DROP 済み）のエントリを捨てる（`lastval` が存在確認に失敗したとき。設計記録 D6）。
    pub(crate) fn forget(&mut self, tenant_id: &str, sequence_id: u64) {
        let key = (tenant_id.to_string(), sequence_id);
        self.currval.remove(&key);
        if self.last.as_ref() == Some(&key) {
            self.last = None;
        }
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

    /// 現スレッドの文脈を一時的に外す（一時表の文を一時コアで実行する区間用。一時 DB は永続 DB の
    /// SEQUENCE カタログを持たないため、外側の INSERT が張った採番文脈を一時表の書き込みへ
    /// 持ち込ませない。スコープ無しの採番は `tenant::assign_sequence_values` が `0A000` で拒否する）。
    pub(crate) fn suspend() -> Self {
        let prev = CURRENT.with(|c| c.replace(None));
        SequenceScope { prev }
    }

    /// セッション状態を持たない文脈を張る（公開入口 `execute_insert_returning` を直接呼ぶ経路用。
    /// 払い出し値の受け渡し（`RETURNING` 用）だけを有効にし、`currval` は `0A000` になる）。
    pub(crate) fn enter_sessionless(allocator: Arc<SequenceAllocator>) -> Self {
        let ctx = SequenceContext {
            allocator,
            session: None,
        };
        let prev = CURRENT.with(|c| c.replace(Some(ctx)));
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
        assert_eq!(s.currval("t", 1), None);
        assert!(s.record_nextval("t", 1, 5));
        assert_eq!(s.currval("t", 1), Some(5));
        assert!(s.record_nextval("t", 1, 6));
        assert_eq!(s.currval("t", 1), Some(6));
        assert_eq!(s.last, Some(("t".to_string(), 1)));
    }

    #[test]
    fn currval_is_isolated_per_tenant() {
        let mut s = SessionSequenceState::default();
        assert!(s.record_nextval("alice", 1, 5));
        assert_eq!(s.currval("bob", 1), None);
        assert_eq!(s.currval("alice", 1), Some(5));
    }

    #[test]
    fn setval_updates_currval_but_not_last() {
        let mut s = SessionSequenceState::default();
        assert!(s.record_setval("t", 1, 50));
        assert_eq!(s.currval("t", 1), Some(50));
        assert_eq!(s.lastval("t"), None);
        assert!(s.record_nextval("t", 2, 7));
        assert!(s.record_setval("t", 2, 50));
        assert_eq!(s.lastval("t"), Some((2, 50)));
        assert!(s.record_setval("t", 1, 60));
        assert_eq!(s.lastval("t"), Some((2, 50)));
    }

    #[test]
    fn lastval_is_isolated_per_tenant_and_forget_clears_it() {
        let mut s = SessionSequenceState::default();
        assert!(s.record_nextval("alice", 1, 5));
        assert_eq!(s.lastval("bob"), None);
        assert_eq!(s.lastval("alice"), Some((1, 5)));
        s.forget("alice", 1);
        assert_eq!(s.lastval("alice"), None);
        assert_eq!(s.currval("alice", 1), None);
    }

    #[test]
    fn can_record_allows_existing_key_at_the_cap() {
        let mut s = SessionSequenceState::default();
        assert!(s.can_record("t", 1));
        for id in 0..MAX_SESSION_SEQUENCES as u64 {
            assert!(s.record_setval("t", id, 1));
        }
        assert!(!s.can_record("t", u64::MAX));
        assert!(!s.record_setval("t", u64::MAX, 1));
        assert!(s.can_record("t", 0));
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
