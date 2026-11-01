//! 共通ゲートの単体テスト（SQL-40・SQL-23 ポインタ）。DDL は `ddl_allowed` だけで決まり、
//! ロールの束縛が権限を広げないことと、ファサードの同値性を確認する。

use super::*;
use crate::sql::ddl::require_ddl_permission;

fn allowed() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn bound_only() -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(1, 1);
    s
}

fn is_denied(r: Result<(), SqlSurfaceError>) -> bool {
    matches!(&r, Err(e @ SqlSurfaceError::InsufficientPrivilege) if e.wire_code() == "42501")
}

#[test]
fn default_session_is_denied_for_all_kinds() {
    let s = SessionState::default();
    for &k in DdlStatementKind::ALL {
        assert!(is_denied(require_ddl(&s, k)), "{k:?}");
    }
}

#[test]
fn allowed_session_passes_for_all_kinds() {
    let s = allowed();
    for &k in DdlStatementKind::ALL {
        assert!(require_ddl(&s, k).is_ok(), "{k:?}");
    }
}

#[test]
fn role_binding_does_not_widen_ddl() {
    let s = bound_only();
    for &k in DdlStatementKind::ALL {
        assert!(is_denied(require_ddl(&s, k)), "{k:?}");
    }
}

#[test]
fn facades_agree_with_gate() {
    for s in [SessionState::default(), allowed(), bound_only()] {
        let expect = require_server_ddl(&s).is_ok();
        assert_eq!(require_ddl_permission(&s).is_ok(), expect);
        for &k in DdlStatementKind::ALL {
            assert_eq!(require_ddl(&s, k).is_ok(), expect, "{k:?}");
        }
    }
}

#[test]
fn privilege_table_is_total_and_unique() {
    assert_eq!(DdlStatementKind::ALL.len(), 28);
    for (i, a) in DdlStatementKind::ALL.iter().enumerate() {
        assert_eq!(a.required_privilege(), RequiredPrivilege::ServerDdl);
        for b in &DdlStatementKind::ALL[i + 1..] {
            assert_ne!(a, b);
        }
    }
}

#[test]
fn bound_principal_contract() {
    assert!(is_denied(
        bound_principal(&SessionState::default()).map(|_| ())
    ));
    let mut zero = SessionState::default();
    zero.bind_role_ids(0, 0);
    assert!(is_denied(bound_principal(&zero).map(|_| ())));
    let mut s = SessionState::default();
    s.bind_role_ids(7, 3);
    assert_eq!(bound_principal(&s).ok(), Some(7));
}

#[test]
fn owner_equivalent_follows_ddl_allowed_only() {
    assert!(!is_owner_equivalent(&SessionState::default()));
    assert!(!is_owner_equivalent(&bound_only()));
    assert!(is_owner_equivalent(&allowed()));
    let mut s = bound_only();
    s.allow_ddl();
    assert!(is_owner_equivalent(&s));
}

#[test]
fn denial_message_is_fixed_and_has_no_detail() {
    let mut s = SessionState::default();
    s.bind_role_ids(4242, 4242);
    let e = require_ddl(&s, DdlStatementKind::DropTable).expect_err("denied");
    assert_eq!(format!("{e:?}"), "InsufficientPrivilege");
    assert_eq!(e.client_message(), "permission denied for DDL statement");
}
