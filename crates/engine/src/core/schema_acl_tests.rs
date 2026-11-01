//! SCHEMA 単位の GRANT／REVOKE と USAGE 検査の SQL 水準の単体テスト（SQL-40・RLS-12 のポインタ、Issue #2234）。
//!
//! `Storage::create_role` が crate 内限定のため、束縛ロールを要する経路（USAGE の拒否・付与・PUBLIC・
//! 取り消し・`search_path` での不可視・準備済み文の再検査・`ALL TABLES IN SCHEMA` の時点固定・他テナントの
//! 存在オラクル）をここで固定する。未束縛・所有者相当が従来どおり検査されないことも併せて確認する。

use super::EngineCore;
use crate::catalog::privilege::{PrivilegeObject, PRIV_SELECT, PRIV_USAGE};
use crate::catalog::role::{RoleAttributes, RoleDef};
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn new_core(tag: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn mk_role(core: &EngineCore, tenant: &str, name: &str) -> u64 {
    core.storage
        .create_role(
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

/// 束縛ロールのセッション。`owner` なら DDL 権限ゲート通過済み（所有者相当）。
fn bound(id: u64, owner: bool) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    if owner {
        s.allow_ddl();
    }
    s
}

fn ok(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {} {e}", e.wire_code()));
}

fn code(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> &'static str {
    match core.execute_sql_in_session(&ctx(tenant), s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn message(core: &EngineCore, tenant: &str, s: &mut SessionState, sql: &str) -> String {
    match core.execute_sql_in_session(&ctx(tenant), s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.to_string(),
    }
}

/// owner（所有者相当）が `s.t` を持つ SCHEMA を作り、非所有者 alice を返す。
fn fixture(tag: &str) -> (EngineCore, CleanupGuard, SessionState, SessionState) {
    let (core, g) = new_core(tag);
    let owner = mk_role(&core, "t1", "owner_r");
    let alice = mk_role(&core, "t1", "alice");
    let mut o = bound(owner, true);
    ok(&core, "t1", &mut o, "CREATE SCHEMA s");
    ok(
        &core,
        "t1",
        &mut o,
        "CREATE TABLE s.t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    (core, g, o, bound(alice, false))
}

#[test]
fn qualified_reference_requires_usage_for_bound_role() {
    let (core, _g, mut o, mut a) = fixture("sacl_usage");
    // USAGE 無し: 修飾名の参照は `42501`。
    assert_eq!(code(&core, "t1", &mut a, "SELECT id FROM s.t"), "42501");
    // 付与後は通る。
    ok(&core, "t1", &mut o, "GRANT USAGE ON SCHEMA s TO alice");
    ok(&core, "t1", &mut a, "SELECT id FROM s.t");
    // 取り消すと再び `42501`。
    ok(&core, "t1", &mut o, "REVOKE USAGE ON SCHEMA s FROM alice");
    assert_eq!(code(&core, "t1", &mut a, "SELECT id FROM s.t"), "42501");
    // CREATE だけでは USAGE にならない。
    ok(&core, "t1", &mut o, "GRANT CREATE ON SCHEMA s TO alice");
    assert_eq!(code(&core, "t1", &mut a, "SELECT id FROM s.t"), "42501");
}

#[test]
fn public_grant_and_all_expansion_cover_every_role() {
    let (core, _g, mut o, mut a) = fixture("sacl_public");
    ok(&core, "t1", &mut o, "GRANT ALL ON SCHEMA s TO PUBLIC");
    ok(&core, "t1", &mut a, "SELECT id FROM s.t");
    let id = core
        .storage
        .schema_id("t1", "s")
        .expect("id")
        .expect("some");
    let rows = core
        .storage
        .list_acl_for_object("t1", &PrivilegeObject::Schema(id))
        .expect("acl");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].grantee_id, 0);
    assert_eq!(
        rows[0].privileges,
        PRIV_USAGE | crate::catalog::privilege::PRIV_CREATE
    );
}

#[test]
fn unbound_and_owner_equivalent_sessions_are_not_checked() {
    let (core, _g, mut o, _a) = fixture("sacl_unbound");
    ok(&core, "t1", &mut o, "SELECT id FROM s.t");
    let mut unbound = SessionState::default();
    ok(&core, "t1", &mut unbound, "SELECT id FROM s.t");
}

#[test]
fn other_tenant_schema_is_indistinguishable_from_missing() {
    let (core, _g, _o, _a) = fixture("sacl_tenant");
    let r2 = mk_role(&core, "t2", "carol");
    // t2 から見た t1 の `s` と、どこにも無い `nope` は、束縛・未束縛とも同じ `3F000`・同じ文言。
    for mut sess in [bound(r2, false), SessionState::default()] {
        let theirs = message(&core, "t2", &mut sess, "SELECT id FROM s.t");
        let missing = message(&core, "t2", &mut sess, "SELECT id FROM nope.t");
        assert_eq!(code(&core, "t2", &mut sess, "SELECT id FROM s.t"), "3F000");
        assert_eq!(
            theirs.replace(": s", ": X"),
            missing.replace(": nope", ": X")
        );
    }
    // GRANT ... ON SCHEMA も同じ（t2 のロールが t1 の SCHEMA 名を書いても不在と区別できない）。
    let mut o2 = bound(r2, true);
    let a = message(&core, "t2", &mut o2, "GRANT USAGE ON SCHEMA s TO carol");
    let b = message(&core, "t2", &mut o2, "GRANT USAGE ON SCHEMA nope TO carol");
    assert_eq!(
        code(&core, "t2", &mut o2, "GRANT USAGE ON SCHEMA s TO carol"),
        "3F000"
    );
    assert_eq!(a.replace(": s", ": X"), b.replace(": nope", ": X"));
}

#[test]
fn all_tables_in_schema_only_covers_existing_relations() {
    let (core, _g, mut o, _a) = fixture("sacl_all_tables");
    ok(
        &core,
        "t1",
        &mut o,
        "CREATE TABLE s.t2 (id BIGINT PRIMARY KEY)",
    );
    ok(&core, "t1", &mut o, "CREATE VIEW s.v AS SELECT id FROM s.t");
    ok(
        &core,
        "t1",
        &mut o,
        "GRANT SELECT ON ALL TABLES IN SCHEMA s TO alice",
    );
    // 付与後に作った表には効かない。
    ok(
        &core,
        "t1",
        &mut o,
        "CREATE TABLE s.t3 (id BIGINT PRIMARY KEY)",
    );
    let sid = core
        .storage
        .schema_id("t1", "s")
        .expect("id")
        .expect("some");
    let sel = |name: &str| {
        core.storage
            .list_acl_for_object("t1", &PrivilegeObject::Table(format!("#{sid}.{name}")))
            .expect("acl")
            .iter()
            .fold(0u16, |acc, e| acc | e.privileges)
    };
    assert_eq!(sel("t"), PRIV_SELECT);
    assert_eq!(sel("t2"), PRIV_SELECT);
    assert_eq!(sel("v"), PRIV_SELECT);
    assert_eq!(sel("t3"), 0);
    // REVOKE も実行時点の既存 relation に作用する。
    ok(
        &core,
        "t1",
        &mut o,
        "REVOKE SELECT ON ALL TABLES IN SCHEMA s FROM alice",
    );
    assert_eq!(sel("t"), 0);
}

#[test]
fn schema_grant_error_contracts() {
    let (core, _g, mut o, mut a) = fixture("sacl_errors");
    // 組み込み SCHEMA は構文段の `0A000`。
    for sql in [
        "GRANT USAGE ON SCHEMA public TO alice",
        "GRANT USAGE ON SCHEMA pg_catalog TO alice",
        "GRANT SELECT ON ALL TABLES IN SCHEMA public TO alice",
    ] {
        assert_eq!(code(&core, "t1", &mut o, sql), "0A000", "{sql}");
    }
    // SCHEMA に許されない権限は `0LP01`。不在の SCHEMA は `3F000`。
    assert_eq!(
        code(&core, "t1", &mut o, "GRANT SELECT ON SCHEMA s TO alice"),
        "0LP01"
    );
    assert_eq!(
        code(&core, "t1", &mut o, "GRANT USAGE ON SCHEMA nope TO alice"),
        "3F000"
    );
    // 付与者が SCHEMA に権限を持たなければ `42501`。
    assert_eq!(
        code(&core, "t1", &mut a, "GRANT USAGE ON SCHEMA s TO alice"),
        "42501"
    );
}

#[test]
fn search_path_skips_schema_without_usage() {
    let (core, _g, mut o, mut a) = fixture("sacl_path");
    ok(&core, "t1", &mut a, "SET search_path = s, public");
    let current = |core: &EngineCore, s: &mut SessionState| -> String {
        match core
            .execute_sql_in_session(&ctx("t1"), s, "SELECT current_schema()")
            .expect("select")
        {
            crate::sql::SqlOutcome::Query(r) => format!("{:?}", r.rows[0].cells[0]),
            other => panic!("{other:?}"),
        }
    };
    // USAGE が無ければ path 上で飛ばされ、`public` が current_schema になる。
    assert!(current(&core, &mut a).contains("public"));
    ok(&core, "t1", &mut o, "GRANT USAGE ON SCHEMA s TO alice");
    assert!(current(&core, &mut a).contains('s'));
    assert!(!current(&core, &mut a).contains("public"));
}

#[test]
fn dropping_schema_releases_roles_for_drop_role() {
    let (core, _g, mut o, _a) = fixture("sacl_drop");
    ok(&core, "t1", &mut o, "GRANT USAGE ON SCHEMA s TO alice");
    assert_eq!(code(&core, "t1", &mut o, "DROP ROLE alice"), "2BP01");
    ok(&core, "t1", &mut o, "DROP SCHEMA s CASCADE");
    ok(&core, "t1", &mut o, "DROP ROLE alice");
}
