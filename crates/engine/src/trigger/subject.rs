//! 発火元セッションの主体の引き継ぎ区間（TABLE-45・Issue #3184。実行モデルの D9）。
//!
//! トリガの発火点（`tenant` の書き込み経路 → `trigger::fire`）は `SessionState` を持たない。一方、
//! トリガ関数の本体が実行する埋め込み SQL は、発火元の文と同じ主体（ロール・`search_path`・TimeZone・
//! 一時カタログ・`row_security`）で解析・権限判定しなければならない（権限は広げない）。そこで `core` の
//! 文の実行入口（`session_time_zone_scope` と同じ箇所）が [`TriggerSubjectScope`] で主体の写しを現スレッドへ
//! 張り、`core::plpgsql_trigger_host` のトリガ用ホストが [`current`] で読んで派生セッションを作る。
//!
//! 区間が無い経路（セッションを持たない公開 API 等）で発火したトリガは、ホストが固定文言の `0A000` で
//! 拒否する（主体を推測して既定セッションで実行する fail-open を作らない）。drop は直前の状態へ戻すため、
//! 入れ子（本体の SQL が別の文として入口を通る場合）でも外側の区間を壊さない。

use std::cell::RefCell;

use crate::sql::mode::SessionState;

thread_local! {
    /// 現スレッドで有効な発火元セッションの主体の写し（無ければ `None`）。
    static SUBJECT: RefCell<Option<SessionState>> = const { RefCell::new(None) };
}

/// 主体の写しを現スレッドへ張る RAII ガード。drop（panic・早期 return を含む）で直前の状態へ戻す。
pub(crate) struct TriggerSubjectScope {
    prev: Option<SessionState>,
}

impl TriggerSubjectScope {
    /// `session` の主体の写しを張る（`SessionState::trigger_subject_snapshot`）。
    pub(crate) fn enter(session: &SessionState) -> Self {
        let snapshot = session.trigger_subject_snapshot();
        let prev = SUBJECT.with(|s| s.borrow_mut().replace(snapshot));
        Self { prev }
    }
}

impl Drop for TriggerSubjectScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        SUBJECT.with(|s| *s.borrow_mut() = prev);
    }
}

/// 現在張られている主体の写し（派生セッションの元）。区間が無ければ `None`。
pub(crate) fn current() -> Option<SessionState> {
    SUBJECT.with(|s| s.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_restores_previous_and_nests() {
        assert!(current().is_none());
        let mut a = SessionState::default();
        a.set_session_user("alice");
        let mut b = SessionState::default();
        b.set_session_user("bob");
        let outer = TriggerSubjectScope::enter(&a);
        assert_eq!(current().expect("outer").session_user(), Some("alice"));
        {
            let _inner = TriggerSubjectScope::enter(&b);
            assert_eq!(current().expect("inner").session_user(), Some("bob"));
        }
        assert_eq!(current().expect("restored").session_user(), Some("alice"));
        drop(outer);
        assert!(current().is_none());
    }
}
