//! 利用者定義ポリシーを「このセッションに適用するか」の単一の判定点（RLS-13・Issue #2278）。
//!
//! 役割: `catalog::row_security::RowSecurityState`（表 x テナントの ENABLE／FORCE）と、セッション側の
//! 事情（所有者か・`BYPASSRLS` を持つか・`row_security` が on か）を PostgreSQL 17 の判定順で合成する。
//! 読み取り側（#2276）・書き込み側（#2277 の WITH CHECK）の判定点は、第 2 の判定系を作らずここを呼ぶ。
//!
//! **テナント境界には一切触れない**: [`PolicyDecision::Skip`] で外れるのは利用者ポリシーだけで、
//! `PolicyContext::is_visible`／`ImplicitRlsHook`／`RlsSafetyNet` は本判定と独立に常に適用される
//! （`docs/design/row-security-policy-composition.md` の判断 A。BYPASSRLS でも他テナントは不可視）。
//!
//! fail-closed: ロール未紐付け・所有者表（#2236）未実装の間は「BYPASSRLS なし・非所有者」とみなし、
//! 利用者ポリシーを適用する側（狭める側）へ倒す。

use crate::catalog::row_security::RowSecurityState;
use crate::sql::mode::SessionState;

thread_local! {
    /// 現在の文の実行区間に束縛された `row_security`（`true` = on）。スコープが無い経路（NoSQL・内部
    /// 書き込み等）は既定の on（利用者ポリシーを適用する側。fail-closed）。
    static ROW_SECURITY_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

/// セッションの `row_security` を文の実行区間へ束縛する RAII（`core.rs` のセッション入口が
/// `session_time_zone_scope` と同じ区間で張る）。書き込みの検査点（`policy_eval::compile_in_txn`）は
/// `SessionState` を持たないため、この束縛経由で [`decide`] へ実行時の値を渡す。
pub(crate) struct RowSecurityScope {
    prev: bool,
}

impl RowSecurityScope {
    pub(crate) fn enter(on: bool) -> Self {
        let prev = ROW_SECURITY_ON.with(|c| c.replace(on));
        Self { prev }
    }
}

impl Drop for RowSecurityScope {
    fn drop(&mut self) {
        ROW_SECURITY_ON.with(|c| c.set(self.prev));
    }
}

/// 現在スレッドに束縛された `row_security`（スコープが無ければ on）。
pub(crate) fn current_row_security() -> bool {
    ROW_SECURITY_ON.with(std::cell::Cell::get)
}

/// 判定に使うセッション側の事情。判定は実行時に現在の [`SessionState`] から作り、準備済み文や
/// 各種キャッシュのキーには混ぜない（判断 A）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PolicyActor {
    /// 現在のロール ID（未紐付けは `None`）。
    pub current_role_id: Option<u64>,
    /// セッションのロール ID（未紐付けは `None`）。
    pub session_role_id: Option<u64>,
    /// 現在のロール自身が `BYPASSRLS` を持つか。継承しない（PG 17 と同じ）。ロール属性のカタログ
    /// （#1961）が入るまで常に `false`（利用者ポリシーを適用する側。fail-closed）。
    pub bypass_rls: bool,
    /// `row_security` が on か。
    pub row_security: bool,
}

impl PolicyActor {
    /// 現在のセッションから作る。呼び出し元は読み書きの判定点（`sql::policy_eval` 等）。
    pub fn from_session(session: &SessionState) -> Self {
        Self {
            current_role_id: session.current_role_id(),
            session_role_id: session.session_role_id(),
            bypass_rls: false,
            row_security: session.row_security(),
        }
    }
}

/// [`decide`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyDecision {
    /// 利用者ポリシーを適用しない（テナント境界は別途常に適用される）。
    Skip,
    /// 利用者ポリシーを適用する。
    Apply,
    /// 適用されるはずだが `row_security = off` のため文を拒否する（`42501`。fail-closed）。
    /// 呼び出し元は副作用ゼロのうちに（行を書く前に）文全体を拒否する。
    RejectRowSecurityOff,
}

/// PostgreSQL 17 の `check_enable_rls` と同じ順序で適用可否を決める。
///
/// 1. 表が ENABLE でなければ `Skip`
/// 2. `BYPASSRLS` なら `Skip`
/// 3. 所有者で FORCE でなければ `Skip`
/// 4. `row_security = off` なら `RejectRowSecurityOff`
/// 5. それ以外は `Apply`
///
/// `is_owner` は呼び出し側が渡す。所有者表（#2236）が入るまでは `false` 固定（狭める側）。
pub fn decide(state: RowSecurityState, actor: &PolicyActor, is_owner: bool) -> PolicyDecision {
    if !state.enabled {
        return PolicyDecision::Skip;
    }
    if actor.bypass_rls {
        return PolicyDecision::Skip;
    }
    if is_owner && !state.forced {
        return PolicyDecision::Skip;
    }
    if !actor.row_security {
        return PolicyDecision::RejectRowSecurityOff;
    }
    PolicyDecision::Apply
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RLS-13: enabled x forced x is_owner x bypass_rls x row_security の全 32 通りが PG 17 の順序どおり。
    #[test]
    fn decide_matches_pg17_order_for_all_combinations() {
        for bits in 0u8..32 {
            let enabled = bits & 1 != 0;
            let forced = bits & 2 != 0;
            let is_owner = bits & 4 != 0;
            let bypass = bits & 8 != 0;
            let rs = bits & 16 != 0;
            let actor = PolicyActor {
                current_role_id: None,
                session_role_id: None,
                bypass_rls: bypass,
                row_security: rs,
            };
            let expected = if !enabled || bypass || (is_owner && !forced) {
                PolicyDecision::Skip
            } else if !rs {
                PolicyDecision::RejectRowSecurityOff
            } else {
                PolicyDecision::Apply
            };
            assert_eq!(
                decide(RowSecurityState { enabled, forced }, &actor, is_owner),
                expected,
                "enabled={enabled} forced={forced} owner={is_owner} bypass={bypass} rs={rs}"
            );
        }
    }

    /// 既定セッション（ロール未紐付け・row_security on）は ENABLE 済みの表に利用者ポリシーを適用する。
    #[test]
    fn default_session_applies_policies_fail_closed() {
        let actor = PolicyActor::from_session(&SessionState::default());
        assert!(!actor.bypass_rls && actor.row_security);
        assert_eq!(actor.current_role_id, None);
        let st = RowSecurityState {
            enabled: true,
            forced: false,
        };
        assert_eq!(decide(st, &actor, false), PolicyDecision::Apply);
    }

    /// off のセッションは、ENABLE していない表には影響しない。
    #[test]
    fn row_security_off_is_irrelevant_for_disabled_tables() {
        let mut s = SessionState::default();
        s.set_row_security(false);
        let actor = PolicyActor::from_session(&s);
        assert_eq!(
            decide(RowSecurityState::default(), &actor, false),
            PolicyDecision::Skip
        );
    }
}

#[cfg(test)]
mod scope_tests {
    use super::*;

    /// スコープは入れ子で巻き戻り、既定は on。
    #[test]
    fn scope_nests_and_restores() {
        assert!(current_row_security());
        {
            let _a = RowSecurityScope::enter(false);
            assert!(!current_row_security());
            {
                let _b = RowSecurityScope::enter(true);
                assert!(current_row_security());
            }
            assert!(!current_row_security());
        }
        assert!(current_row_security());
    }
}
