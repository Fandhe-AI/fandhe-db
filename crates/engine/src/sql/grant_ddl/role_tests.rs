//! ロール形 `GRANT`／`REVOKE`・`CREATE ROLE ... IN ROLE`・継承の単体テスト（SQL-39 (7)・SQL-40、Issue #2232）。
//!
//! 付与者の記録・ADMIN の継承・循環・オプションの更新・RESTRICT／CASCADE・存在オラクルの防止・ACL の付与権限の
//! 継承・`pg_has_role` 相当の評価を、`pub(crate)` のカタログ API と SQL 表層の実行本体で確かめる。
//! ロール ID は 1 から順に採番される（`create_role` の契約）ため、テストは作成順で ID を使う。

use super::*;
use crate::catalog::privilege::{AclEntry, PrivilegeObject, PRIV_SELECT};
use crate::catalog::role::graph::{has_role_privilege, HasRoleError};
use crate::catalog::role::{get_membership, MembershipOptions, RoleAttributes, RoleDef};
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::sql::lexer::tokenize;
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
use redb::ReadableDatabase;

const T: &str = "t1";

struct Fx {
    _g: CleanupGuard,
    s: Storage,
}

fn fx(label: &str) -> Fx {
    let path = unique_db_path(label);
    let g = CleanupGuard(path.clone());
    let s = Storage::open(&path).expect("open");
    s.create_table(&TableSchema::new(
        "docs",
        vec![ColumnDef::new("a", ColumnType::Text, true)],
    ))
    .expect("table");
    Fx { _g: g, s }
}

fn mk(f: &Fx, tenant: &str, name: &str, inherit: bool) -> u64 {
    f.s.create_role(
        tenant,
        &RoleDef {
            id: 0,
            name: name.to_string(),
            attributes: RoleAttributes {
                inherit,
                ..RoleAttributes::default()
            },
            connection_limit: -1,
            valid_until: None,
            scram_verifier: Vec::new(),
            legacy_phc: Vec::new(),
        },
    )
    .expect("role")
}

fn role(f: &Fx, name: &str) -> u64 {
    mk(f, T, name, true)
}

fn session(id: u64, owner: bool) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    if owner {
        s.allow_ddl();
    }
    s
}

fn run(f: &Fx, s: &SessionState, sql: &str) -> Result<GrantRevokeOutcome, SqlSurfaceError> {
    let stmt = crate::sql::allowlist::validate_grant_revoke_tokens(&tokenize(sql).expect("lex"))?;
    execute_grant_revoke(&f.s, T, s, &stmt)
}

fn code(r: Result<GrantRevokeOutcome, SqlSurfaceError>) -> &'static str {
    r.expect_err("must fail").wire_code()
}

fn mem(f: &Fx, m: u64, g: u64) -> Option<MembershipOptions> {
    let rt = f.s.db().begin_read().expect("read");
    get_membership(&rt, T, m, g).expect("get")
}

fn priv_check(f: &Fx, user: u64, role: u64, privs: &str) -> bool {
    let rt = f.s.db().begin_read().expect("read");
    has_role_privilege(&rt, T, user, role, privs).expect("check")
}

#[test]
fn grant_records_grantor_and_defaults() {
    let f = fx("rm_defaults");
    let me = role(&f, "me");
    let r = role(&f, "r");
    let m_inh = mk(&f, T, "m_inh", true);
    let m_noinh = mk(&f, T, "m_noinh", false);
    let s = session(me, true);
    let out = run(&f, &s, "GRANT r TO m_inh, m_noinh").expect("grant");
    assert!(out.is_role() && !out.is_revoke() && out.warning.is_none());
    assert_eq!(
        mem(&f, m_inh, r),
        Some(MembershipOptions {
            admin: false,
            inherit: true,
            set: true,
            grantor_id: Some(me)
        })
    );
    // NOINHERIT 属性のロールへの既定は INHERIT 偽。属性を後から変えても既存の付与は遡って変わらない。
    assert!(!mem(&f, m_noinh, r).expect("row").inherit);
}

#[test]
fn explicit_options_update_only_what_is_named() {
    let f = fx("rm_update");
    let me = role(&f, "me");
    let r = role(&f, "r");
    let m = role(&f, "m");
    let s = session(me, true);
    run(&f, &s, "GRANT r TO m WITH ADMIN TRUE, INHERIT FALSE").expect("grant");
    let a = mem(&f, m, r).expect("row");
    assert!(a.admin && !a.inherit && a.set);
    run(&f, &s, "GRANT r TO m WITH SET FALSE").expect("update");
    let b = mem(&f, m, r).expect("row");
    assert!(b.admin && !b.inherit && !b.set);
    assert_eq!(b.grantor_id, Some(me));
    // 明示が無ければ何も変えない。
    run(&f, &s, "GRANT r TO m").expect("noop");
    assert_eq!(mem(&f, m, r), Some(b));
}

#[test]
fn cycles_are_rejected_with_0lp01() {
    let f = fx("rm_cycle");
    let me = role(&f, "me");
    role(&f, "a");
    role(&f, "b");
    role(&f, "c");
    let s = session(me, true);
    let e = run(&f, &s, "GRANT a TO a").expect_err("self");
    assert_eq!(e.wire_code(), "0LP01");
    run(&f, &s, "GRANT a TO b").expect("b in a");
    run(&f, &s, "GRANT b TO c").expect("c in b");
    // a を c のメンバーにすると a→c→b→a ではなく c→b→a、逆向きの a への付与が循環になる。
    let e = run(&f, &s, "GRANT c TO a").expect_err("indirect");
    assert_eq!(e.wire_code(), "0LP01");
    assert!(e
        .client_message()
        .ends_with("role \"c\" is a member of role \"a\""));
    // オプションが偽の辺でも循環は検出する。
    run(&f, &s, "GRANT a TO b WITH INHERIT FALSE, SET FALSE").expect("update");
    assert_eq!(code(run(&f, &s, "GRANT c TO a")), "0LP01");
}

#[test]
fn unknown_and_foreign_tenant_roles_are_indistinguishable() {
    let f = fx("rm_oracle");
    let me = role(&f, "me");
    role(&f, "r");
    mk(&f, "t2", "only_t2", true);
    let s = session(me, true);
    let a = run(&f, &s, "GRANT r TO nobody").expect_err("missing");
    let b = run(&f, &s, "GRANT r TO only_t2").expect_err("foreign");
    assert_eq!(a.wire_code(), "42704");
    assert_eq!(b.wire_code(), "42704");
    assert_eq!(
        a.client_message().replace("nobody", "X"),
        b.client_message().replace("only_t2", "X")
    );
    let a = run(&f, &s, "GRANT nobody TO me").expect_err("missing role");
    let b = run(&f, &s, "GRANT only_t2 TO me").expect_err("foreign role");
    assert_eq!(a.wire_code(), b.wire_code());
    assert_eq!(code(run(&f, &s, "GRANT r TO PUBLIC")), "42704");
}

#[test]
fn admin_is_checked_through_inherit_closure_only() {
    let f = fx("rm_admin");
    let me = role(&f, "me");
    let r = role(&f, "r");
    let x = role(&f, "x");
    let y = role(&f, "y");
    let target = role(&f, "target");
    let owner = session(me, true);
    run(&f, &owner, "GRANT r TO x WITH ADMIN TRUE").expect("x admin");
    // x は自分の ADMIN で付与でき、付与者は x。
    run(&f, &session(x, false), "GRANT r TO target").expect("x grants");
    assert_eq!(mem(&f, target, r).expect("row").grantor_id, Some(x));
    // ADMIN を持たない主体は 42501（存在しないロールへの付与は先に 42704）。
    assert_eq!(code(run(&f, &session(y, false), "GRANT r TO me")), "42501");
    // y が x を継承（INHERIT 真）すれば、x の ADMIN で付与でき、付与者は x。
    run(&f, &owner, "GRANT x TO y WITH INHERIT TRUE").expect("y in x");
    run(&f, &session(y, false), "GRANT r TO me").expect("y via x");
    assert_eq!(mem(&f, me, r).expect("row").grantor_id, Some(x));
    // 継承しない経路（INHERIT 偽・SET 真）では ADMIN は届かない。
    run(&f, &owner, "REVOKE r FROM me").expect("cleanup");
    run(&f, &owner, "GRANT x TO y WITH INHERIT FALSE").expect("no inherit");
    assert_eq!(code(run(&f, &session(y, false), "GRANT r TO me")), "42501");
    // ロールは自分自身の ADMIN を持たない。
    assert_eq!(code(run(&f, &session(x, false), "GRANT x TO me")), "42501");
}

#[test]
fn revoke_options_and_grantor_matching() {
    let f = fx("rm_revoke");
    let me = role(&f, "me");
    let r = role(&f, "r");
    let x = role(&f, "x");
    let z = role(&f, "z");
    let m = role(&f, "m");
    let owner = session(me, true);
    run(&f, &owner, "GRANT r TO x WITH ADMIN TRUE").expect("x admin");
    run(&f, &owner, "GRANT r TO z WITH ADMIN TRUE").expect("z admin");
    run(&f, &session(x, false), "GRANT r TO m").expect("by x");
    // 付与者が一致しない（z が取り消そうとする）ときは何もしない。
    let out = run(&f, &session(z, false), "REVOKE r FROM m").expect("noop");
    assert!(out.is_revoke() && out.is_role());
    assert!(mem(&f, m, r).is_some());
    // OPTION FOR はそのビットだけを落とす。
    run(&f, &session(x, false), "REVOKE SET OPTION FOR r FROM m").expect("set off");
    let o = mem(&f, m, r).expect("row");
    assert!(!o.set && o.inherit);
    // 所有者相当は付与者を問わず取り消せる。
    run(&f, &owner, "REVOKE r FROM m").expect("owner");
    assert!(mem(&f, m, r).is_none());
    // 行が無ければ警告相当で何もしない。
    run(&f, &owner, "REVOKE r FROM m").expect("again");
}

#[test]
fn revoke_admin_restrict_and_cascade() {
    let f = fx("rm_cascade");
    let me = role(&f, "me");
    let r = role(&f, "r");
    let x = role(&f, "x");
    let m = role(&f, "m");
    let n = role(&f, "n");
    let owner = session(me, true);
    run(&f, &owner, "GRANT r TO x WITH ADMIN TRUE").expect("x admin");
    run(&f, &session(x, false), "GRANT r TO m WITH ADMIN TRUE").expect("m admin by x");
    // m が付けた付与（m の ADMIN は x の付与に依存）。
    run(&f, &session(m, false), "GRANT r TO n").expect("n by m");
    assert_eq!(mem(&f, n, r).expect("row").grantor_id, Some(m));
    // RESTRICT（既定）は依存する付与があれば 2BP01 で何も変えない。
    let e = run(&f, &owner, "REVOKE ADMIN OPTION FOR r FROM x").expect_err("restrict");
    assert_eq!(e.wire_code(), "2BP01");
    assert!(mem(&f, x, r).expect("kept").admin);
    // CASCADE は連鎖して消す（m の付与 → n の付与）。
    run(&f, &owner, "REVOKE ADMIN OPTION FOR r FROM x CASCADE").expect("cascade");
    assert!(!mem(&f, x, r).expect("x").admin);
    assert!(mem(&f, m, r).is_none());
    assert!(mem(&f, n, r).is_none());
}

/// `GRANT ... WITH ADMIN FALSE` による ADMIN の取り消しも `REVOKE ADMIN OPTION` と同じ依存検査（2BP01）を通る。
#[test]
fn grant_with_admin_false_respects_dependents() {
    let f = fx("grant_admin_false");
    let me = role(&f, "me");
    let r = role(&f, "r");
    let x = role(&f, "x");
    let m = role(&f, "m");
    let owner = session(me, true);
    run(&f, &owner, "GRANT r TO x WITH ADMIN TRUE").expect("x admin");
    run(&f, &session(x, false), "GRANT r TO m WITH ADMIN TRUE").expect("m admin by x");
    let e = run(&f, &owner, "GRANT r TO x WITH ADMIN FALSE").expect_err("restrict");
    assert_eq!(e.wire_code(), "2BP01");
    assert!(mem(&f, x, r).expect("kept").admin);
    assert!(mem(&f, m, r).is_some());
    // 依存が無ければ ADMIN を落とせる。
    run(&f, &owner, "REVOKE r FROM m").expect("drop dependent");
    run(&f, &owner, "GRANT r TO x WITH ADMIN FALSE").expect("no dependents");
    assert!(!mem(&f, x, r).expect("x").admin);
}

#[test]
fn failed_statement_has_no_side_effects() {
    let f = fx("rm_atomic");
    let me = role(&f, "me");
    let r = role(&f, "r");
    let a = role(&f, "a");
    let s = session(me, true);
    // 2 つ目の付与先が存在しない: 1 つ目も書かれない。
    assert_eq!(code(run(&f, &s, "GRANT r TO a, nobody")), "42704");
    assert!(mem(&f, a, r).is_none());
    // 2 つ目の組が循環: 1 つ目も書かれない。
    assert_eq!(code(run(&f, &s, "GRANT r, a TO a")), "0LP01");
    assert!(mem(&f, a, r).is_none());
}

#[test]
fn unbound_session_is_denied_for_role_form() {
    let f = fx("rm_unbound");
    role(&f, "r");
    role(&f, "m");
    let mut s = SessionState::default();
    s.allow_ddl();
    assert_eq!(code(run(&f, &s, "GRANT r TO m")), "42501");
    assert_eq!(code(run(&f, &s, "GRANT nobody TO m")), "42501");
}

#[test]
fn pg_has_role_semantics_follow_inherit_and_set() {
    let f = fx("rm_has_role");
    let me = role(&f, "me");
    let r = role(&f, "r");
    role(&f, "a");
    let m = role(&f, "m");
    let o = session(me, true);
    run(&f, &o, "GRANT r TO m WITH INHERIT FALSE").expect("g");
    assert!(priv_check(&f, m, r, "MEMBER"));
    assert!(!priv_check(&f, m, r, "USAGE"));
    assert!(priv_check(&f, m, r, "SET"));
    assert!(priv_check(&f, m, r, "usage, member"));
    run(&f, &o, "GRANT r TO m WITH INHERIT TRUE, SET FALSE").expect("g2");
    assert!(priv_check(&f, m, r, "USAGE"));
    assert!(!priv_check(&f, m, r, "SET"));
    // 間接: m → a（INHERIT 真）→ r（INHERIT 偽）なら m の USAGE は r に届かない。
    run(&f, &o, "REVOKE r FROM m").expect("clean");
    run(&f, &o, "GRANT a TO m WITH INHERIT TRUE").expect("m in a");
    run(&f, &o, "GRANT r TO a WITH INHERIT FALSE").expect("a in r");
    assert!(!priv_check(&f, m, r, "USAGE"));
    assert!(priv_check(&f, m, r, "MEMBER"));
    assert!(priv_check(&f, m, r, "SET"));
    // 自分自身は常に真。ADMIN OPTION は付与していなければ偽。
    assert!(priv_check(&f, m, m, "USAGE"));
    assert!(!priv_check(&f, m, r, "MEMBER WITH ADMIN OPTION"));
    // 未知の語は、他の語が真でも必ず拒否する。
    let rt = f.s.db().begin_read().expect("read");
    assert!(matches!(
        has_role_privilege(&rt, T, m, r, "MEMBER, bogus"),
        Err(HasRoleError::InvalidPrivilege(w)) if w == "bogus"
    ));
}

#[test]
fn has_role_by_name_does_not_reveal_other_tenants() {
    let f = fx("rm_has_role_name");
    role(&f, "r");
    role(&f, "u");
    mk(&f, "t2", "only_t2", true);
    let missing =
        f.s.has_role_privilege_by_name(T, None, 2, "nobody", "MEMBER");
    let foreign =
        f.s.has_role_privilege_by_name(T, None, 2, "only_t2", "MEMBER");
    match (missing, foreign) {
        (Err(HasRoleError::RoleNotFound(a)), Err(HasRoleError::RoleNotFound(b))) => {
            assert_eq!((a.as_str(), b.as_str()), ("nobody", "only_t2"));
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(f
        .s
        .has_role_privilege_by_name(T, Some("u"), 0, "r", "MEMBER")
        .map(|v| !v)
        .expect("ok"));
}

#[test]
fn acl_grant_option_is_inherited_and_best_grantor_recorded() {
    let f = fx("rm_acl");
    let me = role(&f, "me");
    let r = role(&f, "r");
    let m = role(&f, "m");
    let q = role(&f, "q");
    let owner = session(me, true);
    run(&f, &owner, "GRANT SELECT ON docs TO r WITH GRANT OPTION").expect("r go");
    // m が r を継承しない: 付与できない（r の権限を何も持たない）。
    run(&f, &owner, "GRANT r TO m WITH INHERIT FALSE").expect("m in r");
    assert_eq!(
        code(run(&f, &session(m, false), "GRANT SELECT ON docs TO q")),
        "42501"
    );
    // INHERIT 真なら成功し、付与者は r。
    run(&f, &owner, "GRANT r TO m WITH INHERIT TRUE").expect("inherit");
    run(&f, &session(m, false), "GRANT SELECT ON docs TO q").expect("via r");
    let rows =
        f.s.list_acl_for_object(T, &PrivilegeObject::Table("docs".to_string()))
            .expect("acl");
    assert!(rows.contains(&AclEntry {
        grantee_id: q,
        grantor_id: r,
        privileges: PRIV_SELECT,
        grant_options: 0
    }));
    // REVOKE も同じ方法で選んだ付与者（r）の行だけを対象にする。
    run(&f, &session(m, false), "REVOKE SELECT ON docs FROM q").expect("revoke via r");
    let rows =
        f.s.list_acl_for_object(T, &PrivilegeObject::Table("docs".to_string()))
            .expect("acl");
    assert!(!rows.iter().any(|e| e.grantee_id == q));
}

#[test]
fn drop_role_detaches_grantor_references() {
    let f = fx("rm_drop_grantor");
    let me = role(&f, "me");
    let r = role(&f, "r");
    let x = role(&f, "x");
    let m = role(&f, "m");
    let owner = session(me, true);
    run(&f, &owner, "GRANT r TO x WITH ADMIN TRUE").expect("x admin");
    run(&f, &session(x, false), "GRANT r TO m").expect("by x");
    let stmt =
        crate::sql::allowlist::validate_drop_role_tokens(&tokenize("DROP ROLE x").expect("lex"))
            .expect("parse");
    crate::sql::role_ddl::execute_drop_role(&f.s, T, &stmt).expect("drop");
    let row = mem(&f, m, r).expect("row kept");
    assert_eq!(row.grantor_id, None);
    // 付与者なしの行は所有者相当だけが取り消せる。
    let z = role(&f, "z");
    run(&f, &owner, "GRANT r TO z WITH ADMIN TRUE").expect("z admin");
    run(&f, &session(z, false), "REVOKE r FROM m").expect("noop for non-owner");
    assert!(mem(&f, m, r).is_some());
    run(&f, &owner, "REVOKE r FROM m").expect("owner");
    assert!(mem(&f, m, r).is_none());
}

#[test]
fn create_role_membership_clauses_are_atomic() {
    use crate::sql::allowlist::validate_create_role_tokens;
    let f = fx("rm_create_clauses");
    let me = role(&f, "me");
    let a = role(&f, "a");
    let b = role(&f, "b");
    let c = role(&f, "c");
    let owner = session(me, true);
    let create = |sql: &str, s: &SessionState| {
        let stmt = validate_create_role_tokens(&tokenize(sql).expect("lex"))?;
        crate::sql::role_ddl::execute_create_role(&f.s, T, s, &stmt)
    };
    create("CREATE ROLE n IN ROLE a ROLE b ADMIN c", &owner).expect("create");
    let n = f.s.get_role(T, "n").expect("get").expect("some").id;
    assert_eq!(mem(&f, n, a).expect("in a").grantor_id, Some(me));
    assert!(mem(&f, b, n).is_some());
    let adm = mem(&f, c, n).expect("admin");
    assert!(adm.admin);
    // 未束縛は 42501、存在しないロールは 42704 で、どちらもロールは作られない。
    let unbound = {
        let mut s = SessionState::default();
        s.allow_ddl();
        s
    };
    assert_eq!(
        create("CREATE ROLE p IN ROLE a", &unbound)
            .expect_err("unbound")
            .wire_code(),
        "42501"
    );
    assert_eq!(
        create("CREATE ROLE p IN ROLE nobody", &owner)
            .expect_err("missing")
            .wire_code(),
        "42704"
    );
    assert!(f.s.get_role(T, "p").expect("get").is_none());
    // IN ROLE と ROLE に同じロールを指定すると循環。
    assert_eq!(
        create("CREATE ROLE q IN ROLE a ROLE a", &owner)
            .expect_err("cycle")
            .wire_code(),
        "0LP01"
    );
    assert!(f.s.get_role(T, "q").expect("get").is_none());
}
