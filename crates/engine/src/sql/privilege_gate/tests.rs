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
    matches!(
        &r,
        Err(e @ (SqlSurfaceError::InsufficientPrivilege | SqlSurfaceError::RelationPermissionDenied))
            if e.wire_code() == "42501"
    )
}

#[test]
fn relation_denial_message_is_fixed_and_not_ddl() {
    let mut s = SessionState::default();
    s.bind_role_ids(4242, 4242);
    let e = rel(
        &Principal::from_session(&s),
        crate::catalog::privilege::PRIV_SELECT,
        0,
    )
    .expect_err("denied");
    assert!(matches!(e, SqlSurfaceError::RelationPermissionDenied));
    assert_eq!(e.client_message(), "permission denied for relation");
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
    assert_eq!(DdlStatementKind::ALL.len(), 29);
    for (i, a) in DdlStatementKind::ALL.iter().enumerate() {
        let owner_capable = matches!(
            a,
            DdlStatementKind::DropSchema
                | DdlStatementKind::AlterSchema
                | DdlStatementKind::DropFunction
                | DdlStatementKind::AlterOwner
        );
        let expected = if owner_capable {
            RequiredPrivilege::ServerDdlOrOwner
        } else {
            RequiredPrivilege::ServerDdl
        };
        assert_eq!(a.required_privilege(), expected, "{a:?}");
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
fn bootstrap_all_privileges_is_owner_equivalent_for_session_role_only() {
    // 未束縛では効かない（フラグだけでは所有者相当にならない）。
    let mut unbound = SessionState::default();
    unbound.grant_bootstrap_all_privileges();
    assert!(!is_owner_equivalent(&unbound));
    // 束縛済みで現在ロール＝セッションのロールなら所有者相当（ACL の照会は不要）で、DDL は通さない。
    let mut s = SessionState::default();
    s.bind_role_ids(7, 7);
    s.grant_bootstrap_all_privileges();
    assert!(is_owner_equivalent(&s));
    assert!(!Principal::from_session(&s).needs_acl_lookup());
    assert!(is_denied(require_server_ddl(&s)));
    // SET ROLE 相当で現在ロールが変わると外れ、ACL の照会が要る。
    s.set_current_role_id(9);
    assert!(!is_owner_equivalent(&s));
    assert!(Principal::from_session(&s).needs_acl_lookup());
    s.reset_current_role();
    assert!(is_owner_equivalent(&s));
}

#[test]
fn denial_message_is_fixed_and_has_no_detail() {
    let mut s = SessionState::default();
    s.bind_role_ids(4242, 4242);
    let e = require_ddl(&s, DdlStatementKind::DropTable).expect_err("denied");
    assert_eq!(format!("{e:?}"), "InsufficientPrivilege");
    assert_eq!(e.client_message(), "permission denied");
}

// --- オブジェクト権限（#1967）-------------------------------------------------------------------

fn rel(
    principal: &Principal,
    required_bits: u16,
    granted_bits: u16,
) -> Result<(), SqlSurfaceError> {
    check(
        principal,
        PrivilegeTarget::Relation {
            required_bits,
            granted_bits,
        },
    )
    .map(|_| ())
}

#[test]
fn object_privilege_decision_table() {
    use crate::catalog::privilege::{PRIV_SELECT, PRIV_UPDATE};
    // 未束縛は移行期の主体として素通し（#1971 で束縛されると有効になる）。
    let unbound = Principal::from_session(&SessionState::default());
    assert!(rel(&unbound, PRIV_SELECT, 0).is_ok());
    assert!(!unbound.needs_acl_lookup());
    // PUBLIC の予約 ID は拒否（運用者権限があっても）。
    let mut zero = allowed();
    zero.bind_role_ids(0, 0);
    assert!(is_denied(rel(
        &Principal::from_session(&zero),
        PRIV_SELECT,
        !0
    )));
    // 束縛済み: 要求ビットを全部持つときだけ許可。
    let p = Principal::from_session(&bound_only());
    assert!(p.needs_acl_lookup());
    assert!(is_denied(rel(&p, PRIV_SELECT, 0)));
    assert!(rel(&p, PRIV_SELECT, PRIV_SELECT).is_ok());
    assert!(is_denied(rel(&p, PRIV_SELECT | PRIV_UPDATE, PRIV_SELECT)));
    assert!(rel(&p, PRIV_SELECT | PRIV_UPDATE, PRIV_SELECT | PRIV_UPDATE).is_ok());
    // 要求ビットが空なら（設計上の誤用）許可しない。
    assert!(is_denied(rel(&p, 0, PRIV_SELECT)));
    // 運用者権限は所有者相当として許可（ACL の照会も不要）。
    let mut owner = bound_only();
    owner.allow_ddl();
    let p = Principal::from_session(&owner);
    assert!(!p.needs_acl_lookup());
    assert!(rel(&p, PRIV_SELECT, 0).is_ok());
}

#[test]
fn owner_capable_kinds_defer_to_owner_check_for_bound_roles_only() {
    let kinds = [
        DdlStatementKind::DropSchema,
        DdlStatementKind::AlterSchema,
        DdlStatementKind::DropFunction,
        DdlStatementKind::AlterOwner,
    ];
    for k in kinds {
        // 運用者権限は所有者検査なしで許可。
        assert_eq!(
            require_ddl_or_owner(&allowed(), k).ok(),
            Some(GateDecision::Allowed)
        );
        // 束縛ロールだけなら所有者検査を要求する（ゲート単体では許可しない）。
        let mut s = SessionState::default();
        s.bind_role_ids(7, 7);
        assert_eq!(
            require_ddl_or_owner(&s, k).ok(),
            Some(GateDecision::OwnerCheckRequired(7))
        );
        assert!(is_denied(require_ddl(&s, k)), "{k:?}");
        // 未束縛・PUBLIC（0）はカタログを引く前に 42501。
        assert!(is_denied(
            require_ddl_or_owner(&SessionState::default(), k).map(|_| ())
        ));
        let mut zero = SessionState::default();
        zero.bind_role_ids(0, 0);
        assert!(is_denied(require_ddl_or_owner(&zero, k).map(|_| ())));
    }
}

/// 構文解析が実在を確認した参照先（ビュー展開前のビュー自身を含む）が ACL 照会の時点で見つからない
/// ときは、通常 SQL でも検査を省略せず拒否する（解析後に別セッションが DROP した競合。Issue #1967）。
/// 解析で実在を確認できなかった名前（構文語・別名などの誤分類）だけを読み飛ばす。
#[test]
fn relation_missing_at_acl_lookup_is_rejected_when_parser_confirmed_it() {
    use crate::catalog::privilege::PRIV_SELECT;
    let path = std::env::temp_dir().join(format!(
        "gate-missing-{}-{:?}.redb",
        std::process::id(),
        std::thread::current().id()
    ));
    let storage = crate::storage::Storage::open(&path).expect("open storage");
    let mut s = SessionState::default();
    s.bind_role_ids(1, 1);
    let principal = Principal::from_session(&s);
    let names = vec!["dropped_view".to_string()];
    let run = |parsed: &[String]| {
        read::require_relations(
            &storage,
            "t1",
            &principal,
            PRIV_SELECT,
            &names,
            None,
            parsed,
        )
    };
    // 解析が確認していない名前は読み飛ばす（誤分類された識別子）。
    assert!(run(&[]).is_ok());
    // 解析が確認した名前が消えていれば、存在情報を示さない固定の `42P01` で拒否する。
    let err = run(&names).expect_err("must reject");
    assert_eq!(err.wire_code(), "42P01");
    let _ = std::fs::remove_file(&path);
}
