//! `sql::owner_ddl` の単体テスト（SQL-40・RLS-12 のポインタ、Issue #2236）。`Storage::create_role` が
//! `pub(crate)` のため crate 内に置く。判定順序（未束縛・不在・新所有者の不在・非所有者・SET 不可）・
//! 所有者の移譲と ACL の書き換え・テナント境界・副作用ゼロを固定する。

use super::*;
use crate::catalog::privilege::owner::{is_owner_in_txn, set_owner_in_txn};
use crate::catalog::privilege::{PrivilegeObject, PRIV_SELECT};
use crate::catalog::role::{RoleAttributes, RoleDef};
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::sql::lexer::tokenize;
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

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

fn role(f: &Fx, tenant: &str, name: &str) -> u64 {
    f.s.create_role(
        tenant,
        &RoleDef {
            id: 0,
            name: name.to_string(),
            attributes: RoleAttributes::default(),
            connection_limit: -1,
            valid_until: None,
            scram_verifier: Vec::new(),
            legacy_phc: Vec::new(),
        },
    )
    .expect("role")
}

fn session(id: u64, operator: bool) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    if operator {
        s.allow_ddl();
    }
    s
}

fn run(
    f: &Fx,
    tenant: &str,
    session: &SessionState,
    sql: &str,
) -> Result<AlterOwnerOutcome, SqlSurfaceError> {
    let tokens = tokenize(sql).expect("lex");
    let stmt = crate::sql::allowlist::validate_alter_owner_tokens(&tokens)?;
    execute_alter_owner(&f.s, tenant, session, &stmt)
}

fn code(r: Result<AlterOwnerOutcome, SqlSurfaceError>) -> &'static str {
    r.expect_err("must fail").wire_code()
}

fn set_owner(f: &Fx, tenant: &str, obj: &PrivilegeObject, owner: u64) {
    let txn = f.s.begin_write_txn().expect("begin");
    set_owner_in_txn(&txn, tenant, obj, owner).expect("set");
    crate::recovery::commit_boundary::commit(txn).expect("commit");
}

fn owner_is(f: &Fx, tenant: &str, obj: &PrivilegeObject, role: u64) -> bool {
    let rt = redb::ReadableDatabase::begin_read(f.s.db()).expect("read");
    is_owner_in_txn(&rt, tenant, obj, role).expect("owner")
}

fn docs() -> PrivilegeObject {
    PrivilegeObject::Table("docs".to_string())
}

#[test]
fn unbound_session_is_denied_before_any_catalog_lookup() {
    let f = fx("owner_ddl_unbound");
    let s = SessionState::default();
    // 対象・ロールの有無に関わらず同じ `42501`。
    assert_eq!(
        code(run(&f, T, &s, "ALTER TABLE docs OWNER TO nobody")),
        "42501"
    );
    assert_eq!(
        code(run(&f, T, &s, "ALTER TABLE nope OWNER TO nobody")),
        "42501"
    );
    assert_eq!(
        code(run(&f, T, &s, "ALTER SCHEMA nope OWNER TO nobody")),
        "42501"
    );
}

#[test]
fn non_owner_is_denied_and_nothing_changes() {
    let f = fx("owner_ddl_non_owner");
    let alice = role(&f, T, "alice");
    let bob = role(&f, T, "bob");
    set_owner(&f, T, &docs(), alice);
    let r = run(&f, T, &session(bob, false), "ALTER TABLE docs OWNER TO bob");
    assert_eq!(code(r), "42501");
    assert!(owner_is(&f, T, &docs(), alice));
    assert!(!owner_is(&f, T, &docs(), bob));
}

#[test]
fn owner_can_transfer_only_to_a_settable_role() {
    let f = fx("owner_ddl_transfer");
    let alice = role(&f, T, "alice");
    let bob = role(&f, T, "bob");
    set_owner(&f, T, &docs(), alice);
    // alice は bob へ SET できない（メンバーシップなし）ので移譲できない。
    let r = run(
        &f,
        T,
        &session(alice, false),
        "ALTER TABLE docs OWNER TO bob",
    );
    assert_eq!(code(r), "42501");
    assert!(owner_is(&f, T, &docs(), alice));
    // 自分自身（CURRENT_USER）への移譲は no-op として成功する。
    run(
        &f,
        T,
        &session(alice, false),
        "ALTER TABLE docs OWNER TO CURRENT_USER",
    )
    .expect("self");
    // 運用者権限は SET 可能性を免除する。移譲後、旧所有者は変更できない。
    run(
        &f,
        T,
        &session(alice, true),
        "ALTER TABLE docs OWNER TO bob",
    )
    .expect("operator");
    assert!(owner_is(&f, T, &docs(), bob));
    let r = run(
        &f,
        T,
        &session(alice, false),
        "ALTER TABLE docs OWNER TO alice",
    );
    assert_eq!(code(r), "42501");
}

#[test]
fn unknown_and_other_tenant_roles_are_the_same_error() {
    let f = fx("owner_ddl_role_oracle");
    let alice = role(&f, T, "alice");
    let _carol_elsewhere = role(&f, "t2", "carol");
    set_owner(&f, T, &docs(), alice);
    let s = session(alice, false);
    let missing = run(&f, T, &s, "ALTER TABLE docs OWNER TO ghost").expect_err("missing");
    let other = run(&f, T, &s, "ALTER TABLE docs OWNER TO carol").expect_err("other tenant");
    assert_eq!(missing.wire_code(), "42704");
    assert_eq!(other.wire_code(), "42704");
    assert!(owner_is(&f, T, &docs(), alice));
}

#[test]
fn target_kind_mismatch_and_missing_relation() {
    let f = fx("owner_ddl_kinds");
    let alice = role(&f, T, "alice");
    let s = session(alice, true);
    assert_eq!(
        code(run(&f, T, &s, "ALTER VIEW docs OWNER TO alice")),
        "42809"
    );
    assert_eq!(
        code(run(
            &f,
            T,
            &s,
            "ALTER MATERIALIZED VIEW docs OWNER TO alice"
        )),
        "42809"
    );
    assert_eq!(
        code(run(&f, T, &s, "ALTER TABLE nope OWNER TO alice")),
        "42P01"
    );
    assert_eq!(
        code(run(&f, T, &s, "ALTER SCHEMA nope OWNER TO alice")),
        "3F000"
    );
    let ok = run(&f, T, &s, "ALTER TABLE docs OWNER TO alice").expect("table");
    assert_eq!(ok.kind(), AlterOwnerKind::Table);
}

#[test]
fn transfer_rewrites_acl_rows_of_the_old_owner() {
    let f = fx("owner_ddl_acl");
    let alice = role(&f, T, "alice");
    let bob = role(&f, T, "bob");
    let carol = role(&f, T, "carol");
    set_owner(&f, T, &docs(), alice);
    {
        // alice（所有者）が carol へ付与し、bob へ付与した行もある。
        let txn = f.s.begin_write_txn().expect("begin");
        crate::catalog::privilege::upsert_acl_in_txn(
            &txn,
            T,
            &docs(),
            carol,
            alice,
            PRIV_SELECT,
            0,
        )
        .expect("acl");
        crate::catalog::privilege::upsert_acl_in_txn(&txn, T, &docs(), alice, bob, PRIV_SELECT, 0)
            .expect("acl2");
        crate::recovery::commit_boundary::commit(txn).expect("commit");
    }
    run(
        &f,
        T,
        &session(alice, true),
        "ALTER TABLE docs OWNER TO bob",
    )
    .expect("transfer");
    let rows = f.s.list_acl_for_object(T, &docs()).expect("list");
    // carol への付与の付与者は bob へ、bob→alice の付与は「alice が bob の付与先」のまま残る。
    assert!(rows
        .iter()
        .any(|e| e.grantee_id == carol && e.grantor_id == bob));
    assert!(rows.iter().all(|e| e.grantor_id != alice));
}

#[test]
fn create_schema_owner_resolution() {
    let f = fx("owner_ddl_resolve");
    let alice = role(&f, T, "alice");
    // 指定なし: 束縛中の現在ロール。未束縛は記録しない。
    assert_eq!(
        resolve_schema_owner(&f.s, T, &session(alice, true), None).expect("o"),
        Some(alice)
    );
    assert_eq!(
        resolve_schema_owner(&f.s, T, &SessionState::default(), None).expect("o"),
        None
    );
    // AUTHORIZATION <名>: 自テナントのロール。不在・他テナントは 42704。
    let spec = RoleSpec::Name("alice".to_string());
    assert_eq!(
        resolve_schema_owner(&f.s, T, &SessionState::default(), Some(&spec)).expect("o"),
        Some(alice)
    );
    let missing = RoleSpec::Name("ghost".to_string());
    let err = resolve_schema_owner(&f.s, "t2", &SessionState::default(), Some(&spec))
        .expect_err("other tenant");
    assert_eq!(err.wire_code(), "42704");
    let err = resolve_schema_owner(&f.s, T, &SessionState::default(), Some(&missing))
        .expect_err("missing");
    assert_eq!(err.wire_code(), "42704");
    // CURRENT_USER は未束縛で 42501。
    let err = resolve_schema_owner(
        &f.s,
        T,
        &SessionState::default(),
        Some(&RoleSpec::CurrentRole),
    )
    .expect_err("unbound");
    assert_eq!(err.wire_code(), "42501");
}
