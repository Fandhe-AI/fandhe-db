//! `sql::grant_ddl` の単体テスト（SQL-40、Issue #1965）。`Storage::create_role` が `pub(crate)` のため
//! crate 内に置く。付与者の判定・所有者相当・GRANT OPTION の範囲・REVOKE の連鎖・テナント境界・
//! エラー契約（SQLSTATE）・副作用ゼロを固定する。

use super::*;
use crate::catalog::privilege::{
    AclEntry, PrivilegeObject, PRIV_DELETE, PRIV_INSERT, PRIV_SELECT, PRIV_UPDATE,
};
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
        vec![
            ColumnDef::new("a", ColumnType::Text, true),
            ColumnDef::new("b", ColumnType::Text, true),
        ],
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

/// 現在ロールを束縛したセッション（`owner` なら DDL 権限ゲート通過済み＝所有者相当）。
fn session(id: u64, owner: bool) -> SessionState {
    let mut s = SessionState::default();
    s.bind_role_ids(id, id);
    if owner {
        s.allow_ddl();
    }
    s
}

fn run(
    f: &Fx,
    tenant: &str,
    session: &SessionState,
    sql: &str,
) -> Result<GrantRevokeOutcome, SqlSurfaceError> {
    let stmt = crate::sql::allowlist::validate_grant_revoke_tokens(&tokenize(sql).expect("lex"))?;
    execute_grant_revoke(&f.s, tenant, session, &stmt)
}

fn code(r: Result<GrantRevokeOutcome, SqlSurfaceError>) -> &'static str {
    r.expect_err("must fail").wire_code()
}

fn acl(f: &Fx, tenant: &str, obj: &PrivilegeObject) -> Vec<AclEntry> {
    f.s.list_acl_for_object(tenant, obj).expect("list")
}

fn tbl() -> PrivilegeObject {
    PrivilegeObject::Table("docs".to_string())
}

fn col(c: &str) -> PrivilegeObject {
    PrivilegeObject::Column {
        table: "docs".to_string(),
        column: c.to_string(),
    }
}

#[test]
fn unbound_session_is_denied_before_anything_else() {
    let f = fx("grant_unbound");
    let s = SessionState::default();
    // 存在する表・存在しない表・存在しないロールのどれも同じ `42501`。
    for sql in [
        "GRANT SELECT ON docs TO alice",
        "GRANT SELECT ON missing TO alice",
        "GRANT USAGE ON SCHEMA x TO a",
    ] {
        let stmt =
            crate::sql::allowlist::validate_grant_revoke_tokens(&tokenize(sql).expect("lex"));
        if let Ok(stmt) = stmt {
            assert_eq!(
                execute_grant_revoke(&f.s, T, &s, &stmt)
                    .expect_err("deny")
                    .wire_code(),
                "42501"
            );
        }
    }
    assert!(require_grant_principal(&s).is_err());
    // ddl 許可だけでは束縛されたことにならない。
    let mut s = SessionState::default();
    s.allow_ddl();
    assert_eq!(
        code(run(&f, T, &s, "GRANT SELECT ON docs TO alice")),
        "42501"
    );
}

#[test]
fn owner_equivalent_grant_writes_rows_with_current_role_as_grantor() {
    let f = fx("grant_owner");
    let me = role(&f, T, "owner_role");
    let alice = role(&f, T, "alice");
    let s = session(me, true);
    let out = run(&f, T, &s, "GRANT SELECT, UPDATE ON docs TO alice").expect("grant");
    assert!(!out.is_revoke() && out.warning.is_none());
    let rows = acl(&f, T, &tbl());
    assert_eq!(
        rows,
        vec![AclEntry {
            grantee_id: alice,
            grantor_id: me,
            privileges: PRIV_SELECT | PRIV_UPDATE,
            grant_options: 0
        }]
    );
    // 繰り返すとビットが OR される。
    run(&f, T, &s, "GRANT INSERT ON docs TO alice WITH GRANT OPTION").expect("grant2");
    let rows = acl(&f, T, &tbl());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].privileges, PRIV_SELECT | PRIV_UPDATE | PRIV_INSERT);
    assert_eq!(rows[0].grant_options, PRIV_INSERT);
    // PUBLIC・CURRENT_USER・列単位。
    run(
        &f,
        T,
        &s,
        "GRANT SELECT (a) ON docs TO PUBLIC, CURRENT_USER",
    )
    .expect("cols");
    assert_eq!(acl(&f, T, &col("a")).len(), 2);
    assert!(acl(&f, T, &col("b")).is_empty());
}

#[test]
fn grant_all_expands_to_table_privileges() {
    let f = fx("grant_all");
    let me = role(&f, T, "owner_role");
    role(&f, T, "alice");
    let s = session(me, true);
    run(&f, T, &s, "GRANT ALL ON docs TO alice").expect("grant");
    let rows = acl(&f, T, &tbl());
    assert_eq!(rows[0].privileges, 0xFF);
}

#[test]
fn non_owner_without_any_privilege_is_denied_with_no_side_effects() {
    let f = fx("grant_nopriv");
    let me = role(&f, T, "owner_role");
    let bob = role(&f, T, "bob");
    role(&f, T, "carol");
    let before = acl(&f, T, &tbl());
    assert_eq!(
        code(run(
            &f,
            T,
            &session(bob, false),
            "GRANT SELECT ON docs TO carol"
        )),
        "42501"
    );
    assert_eq!(acl(&f, T, &tbl()), before);
    let _ = me;
}

#[test]
fn privilege_without_grant_option_only_warns_and_changes_nothing() {
    let f = fx("grant_warn");
    let me = role(&f, T, "owner_role");
    let bob = role(&f, T, "bob");
    role(&f, T, "carol");
    run(&f, T, &session(me, true), "GRANT SELECT ON docs TO bob").expect("seed");
    let before = acl(&f, T, &tbl());
    let out = run(&f, T, &session(bob, false), "GRANT SELECT ON docs TO carol").expect("warn");
    assert!(out.had_no_privileges_warning());
    assert_eq!(acl(&f, T, &tbl()), before);
}

#[test]
fn grant_option_holder_regrants_within_range_and_partial_warns() {
    let f = fx("grant_regrant");
    let me = role(&f, T, "owner_role");
    let bob = role(&f, T, "bob");
    let carol = role(&f, T, "carol");
    run(
        &f,
        T,
        &session(me, true),
        "GRANT SELECT, UPDATE ON docs TO bob WITH GRANT OPTION",
    )
    .expect("seed");
    run(&f, T, &session(me, true), "GRANT DELETE ON docs TO bob").expect("seed2");
    let sb = session(bob, false);
    // bob は SELECT・UPDATE の GRANT OPTION だけを持つ。DELETE は範囲外のため部分付与の警告。
    let out = run(&f, T, &sb, "GRANT SELECT, DELETE ON docs TO carol").expect("partial");
    assert!(out.had_partial_warning());
    let rows: Vec<_> = acl(&f, T, &tbl())
        .into_iter()
        .filter(|e| e.grantee_id == carol)
        .collect();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].privileges, PRIV_SELECT);
    assert_eq!(rows[0].grantor_id, bob);
    let _ = PRIV_DELETE;
}

#[test]
fn column_grant_uses_table_level_grant_option() {
    let f = fx("grant_col_go");
    let me = role(&f, T, "owner_role");
    let bob = role(&f, T, "bob");
    let carol = role(&f, T, "carol");
    run(
        &f,
        T,
        &session(me, true),
        "GRANT SELECT ON docs TO bob WITH GRANT OPTION",
    )
    .expect("seed");
    run(
        &f,
        T,
        &session(bob, false),
        "GRANT SELECT (a) ON docs TO carol",
    )
    .expect("col");
    let rows = acl(&f, T, &col("a"));
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].grantee_id, rows[0].grantor_id), (carol, bob));
}

#[test]
fn revoke_only_touches_own_rows_and_drops_grant_option_with_privilege() {
    let f = fx("revoke_own");
    let me = role(&f, T, "owner_role");
    let other = role(&f, T, "other_owner");
    let alice = role(&f, T, "alice");
    run(
        &f,
        T,
        &session(me, true),
        "GRANT SELECT ON docs TO alice WITH GRANT OPTION",
    )
    .expect("a");
    run(
        &f,
        T,
        &session(other, true),
        "GRANT SELECT ON docs TO alice",
    )
    .expect("b");
    // other の REVOKE は other 自身の行だけを落とす。
    run(
        &f,
        T,
        &session(other, true),
        "REVOKE SELECT ON docs FROM alice",
    )
    .expect("revoke");
    let rows = acl(&f, T, &tbl());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].grantor_id, me);
    // GRANT OPTION FOR は GRANT OPTION ビットだけを落とす。
    run(
        &f,
        T,
        &session(me, true),
        "REVOKE GRANT OPTION FOR SELECT ON docs FROM alice",
    )
    .expect("go");
    let rows = acl(&f, T, &tbl());
    assert_eq!(rows[0].grantee_id, alice);
    assert_eq!(
        (rows[0].privileges, rows[0].grant_options),
        (PRIV_SELECT, 0)
    );
    // 権限を落とすと行が消える。
    run(&f, T, &session(me, true), "REVOKE ALL ON docs FROM alice").expect("all");
    assert!(acl(&f, T, &tbl()).is_empty());
}

#[test]
fn revoke_restrict_rejects_dependents_and_cascade_removes_chain() {
    let f = fx("revoke_cascade");
    let me = role(&f, T, "owner_role");
    let bob = role(&f, T, "bob");
    let carol = role(&f, T, "carol");
    let dave = role(&f, T, "dave");
    let so = session(me, true);
    run(&f, T, &so, "GRANT SELECT ON docs TO bob WITH GRANT OPTION").expect("1");
    run(
        &f,
        T,
        &session(bob, false),
        "GRANT SELECT ON docs TO carol WITH GRANT OPTION",
    )
    .expect("2");
    run(
        &f,
        T,
        &session(carol, false),
        "GRANT SELECT ON docs TO dave",
    )
    .expect("3");
    let before = acl(&f, T, &tbl());
    assert_eq!(before.len(), 3);
    assert_eq!(
        code(run(&f, T, &so, "REVOKE SELECT ON docs FROM bob")),
        "2BP01"
    );
    assert_eq!(
        code(run(
            &f,
            T,
            &so,
            "REVOKE GRANT OPTION FOR SELECT ON docs FROM bob"
        )),
        "2BP01"
    );
    assert_eq!(
        acl(&f, T, &tbl()),
        before,
        "RESTRICT failure leaves no side effects"
    );
    run(&f, T, &so, "REVOKE SELECT ON docs FROM bob CASCADE").expect("cascade");
    assert!(acl(&f, T, &tbl()).is_empty());
    let _ = dave;
}

#[test]
fn cascade_survives_when_grantee_keeps_grant_option_from_another_source() {
    let f = fx("revoke_other_source");
    let me = role(&f, T, "owner_role");
    let other = role(&f, T, "other_owner");
    let bob = role(&f, T, "bob");
    let carol = role(&f, T, "carol");
    run(
        &f,
        T,
        &session(me, true),
        "GRANT SELECT ON docs TO bob WITH GRANT OPTION",
    )
    .expect("1");
    run(
        &f,
        T,
        &session(other, true),
        "GRANT SELECT ON docs TO bob WITH GRANT OPTION",
    )
    .expect("2");
    run(&f, T, &session(bob, false), "GRANT SELECT ON docs TO carol").expect("3");
    // bob は other からも GRANT OPTION を持つため、依存する付与は残る（RESTRICT でも成功）。
    run(&f, T, &session(me, true), "REVOKE SELECT ON docs FROM bob").expect("revoke");
    assert!(acl(&f, T, &tbl()).iter().any(|e| e.grantee_id == carol));
}

#[test]
fn circular_grant_option_is_rejected() {
    let f = fx("grant_cycle");
    let me = role(&f, T, "owner_role");
    let bob = role(&f, T, "bob");
    let carol = role(&f, T, "carol");
    run(
        &f,
        T,
        &session(me, true),
        "GRANT SELECT ON docs TO bob WITH GRANT OPTION",
    )
    .expect("1");
    run(
        &f,
        T,
        &session(bob, false),
        "GRANT SELECT ON docs TO carol WITH GRANT OPTION",
    )
    .expect("2");
    assert_eq!(
        code(run(
            &f,
            T,
            &session(carol, false),
            "GRANT SELECT ON docs TO bob WITH GRANT OPTION"
        )),
        "0LP01"
    );
}

#[test]
fn self_grant_option_is_rejected_so_revoke_cascade_reclaims_everything() {
    let f = fx("grant_self_go");
    let me = role(&f, T, "owner_role");
    let bob = role(&f, T, "bob");
    let so = session(me, true);
    run(&f, T, &so, "GRANT SELECT ON docs TO bob WITH GRANT OPTION").expect("1");
    // 非所有者の自己への GRANT OPTION 付与は循環として拒否する。
    assert_eq!(
        code(run(
            &f,
            T,
            &session(bob, false),
            "GRANT SELECT ON docs TO bob WITH GRANT OPTION"
        )),
        "0LP01"
    );
    run(&f, T, &so, "REVOKE SELECT ON docs FROM bob CASCADE").expect("cascade");
    assert!(acl(&f, T, &tbl()).is_empty());
}

#[test]
fn cycle_detection_is_per_privilege_bit() {
    let f = fx("grant_cycle_bits");
    let me = role(&f, T, "owner_role");
    let a = role(&f, T, "a");
    let b = role(&f, T, "b");
    let c = role(&f, T, "c");
    let so = session(me, true);
    run(&f, T, &so, "GRANT SELECT ON docs TO a WITH GRANT OPTION").expect("1");
    run(&f, T, &so, "GRANT UPDATE ON docs TO b WITH GRANT OPTION").expect("2");
    run(&f, T, &so, "GRANT SELECT ON docs TO c WITH GRANT OPTION").expect("3");
    run(
        &f,
        T,
        &session(a, false),
        "GRANT SELECT ON docs TO b WITH GRANT OPTION",
    )
    .expect("a->b select");
    run(
        &f,
        T,
        &session(b, false),
        "GRANT UPDATE ON docs TO c WITH GRANT OPTION",
    )
    .expect("b->c update");
    // 権限ごとには循環しない（SELECT: c<-owner のみ、UPDATE: a は上流に現れない）ため成功する。
    run(
        &f,
        T,
        &session(c, false),
        "GRANT SELECT, UPDATE ON docs TO a WITH GRANT OPTION",
    )
    .expect("no per-bit cycle");
    // 同じ権限での実際の循環（SELECT: a -> b なので b から a への SELECT は不可）は拒否される。
    assert_eq!(
        code(run(
            &f,
            T,
            &session(b, false),
            "GRANT SELECT ON docs TO a WITH GRANT OPTION"
        )),
        "0LP01"
    );
}

#[test]
fn table_level_revoke_cascades_to_column_rows_but_not_the_reverse() {
    let f = fx("revoke_cols");
    let me = role(&f, T, "owner_role");
    let alice = role(&f, T, "alice");
    let so = session(me, true);
    run(&f, T, &so, "GRANT SELECT, UPDATE ON docs TO alice").expect("t");
    run(&f, T, &so, "GRANT SELECT (a), UPDATE (b) ON docs TO alice").expect("c");
    // 列単位の REVOKE は表単位の行に触れない。
    run(&f, T, &so, "REVOKE SELECT (a) ON docs FROM alice").expect("col revoke");
    assert!(acl(&f, T, &col("a")).is_empty());
    assert_eq!(acl(&f, T, &tbl())[0].privileges, PRIV_SELECT | PRIV_UPDATE);
    run(&f, T, &so, "GRANT SELECT (a) ON docs TO alice").expect("again");
    // 表単位の REVOKE は同じ権限の列の行も落とす。
    run(&f, T, &so, "REVOKE SELECT ON docs FROM alice").expect("table revoke");
    assert!(acl(&f, T, &col("a")).is_empty());
    assert_eq!(acl(&f, T, &col("b"))[0].privileges, PRIV_UPDATE);
    assert_eq!(acl(&f, T, &tbl())[0].grantee_id, alice);
    assert_eq!(acl(&f, T, &tbl())[0].privileges, PRIV_UPDATE);
}

#[test]
fn resolution_errors_and_tenant_boundary() {
    let f = fx("grant_resolve");
    let me = role(&f, T, "owner_role");
    role(&f, "t2", "foreign_role");
    let so = session(me, true);
    // 他テナントにだけ在るロールと、どこにも無いロールは同じ `42704`・同じ文言形式。
    let a = run(&f, T, &so, "GRANT SELECT ON docs TO foreign_role").expect_err("foreign");
    let b = run(&f, T, &so, "GRANT SELECT ON docs TO nobody").expect_err("nobody");
    assert_eq!((a.wire_code(), b.wire_code()), ("42704", "42704"));
    assert_eq!(
        a.client_message().replace("foreign_role", "X"),
        b.client_message().replace("nobody", "X")
    );
    // 表の解決が先（PostgreSQL と同じ）。
    assert_eq!(
        code(run(&f, T, &so, "GRANT SELECT ON missing TO nobody")),
        "42P01"
    );
    assert_eq!(
        code(run(&f, T, &so, "GRANT SELECT (zzz) ON docs TO me_not")),
        "42704"
    );
    assert_eq!(
        code(run(&f, T, &so, "GRANT SELECT (zzz) ON docs TO owner_role")),
        "42703"
    );
    // 失敗では ACL は変わらない（原子性）。
    run(&f, T, &so, "GRANT SELECT ON docs TO owner_role").expect("self");
    let before = acl(&f, T, &tbl());
    assert_eq!(
        code(run(
            &f,
            T,
            &so,
            "GRANT UPDATE ON docs, missing TO owner_role"
        )),
        "42P01"
    );
    assert_eq!(acl(&f, T, &tbl()), before);
    // 他テナントの ACL は引けない。
    assert!(acl(&f, "t2", &tbl()).is_empty());
}

#[test]
fn dropped_bound_role_is_denied() {
    let f = fx("grant_dropped");
    let me = role(&f, T, "owner_role");
    role(&f, T, "alice");
    let s = session(me + 100, true);
    assert_eq!(
        code(run(&f, T, &s, "GRANT SELECT ON docs TO alice")),
        "42501"
    );
}

#[test]
fn role_form_unknown_roles_and_current_role_edge_cases() {
    let f = fx("grant_roleform");
    let me = role(&f, T, "owner_role");
    let so = session(me, true);
    // ロール形の実行は Issue #2232 で実装済み（詳細は `role_tests`）。存在しないロールは 42704。
    assert_eq!(code(run(&f, T, &so, "GRANT r1 TO r2")), "42704");
    assert_eq!(code(run(&f, T, &so, "REVOKE r1 FROM r2")), "42704");
    // SESSION_USER は束縛されたセッションロール。
    assert!(run(&f, T, &so, "GRANT SELECT ON docs TO SESSION_USER").is_ok());
}

#[test]
fn views_are_accepted_but_column_lists_on_views_are_not() {
    let f = fx("grant_view");
    let me = role(&f, T, "owner_role");
    role(&f, T, "alice");
    f.s.create_view("v_docs", "docs", "SELECT a FROM docs")
        .expect("view");
    let so = session(me, true);
    run(&f, T, &so, "GRANT SELECT ON v_docs TO alice").expect("view");
    assert_eq!(
        code(run(&f, T, &so, "GRANT SELECT (a) ON v_docs TO alice")),
        "0A000"
    );
}

#[test]
fn acl_entry_limit_maps_to_54000() {
    let e = map_acl_error(AclChangeError::Catalog(CatalogError::ViewLimitExceeded(
        "x".to_string(),
    )));
    assert_eq!(e.wire_code(), "54000");
    assert_eq!(
        map_acl_error(AclChangeError::LimitExceeded).wire_code(),
        "54000"
    );
}

#[test]
fn error_messages_do_not_leak_internal_keys() {
    for e in [
        AclChangeError::Catalog(CatalogError::TableNotFound("#12.docs".to_string())),
        AclChangeError::Catalog(CatalogError::WrongObjectKind("#12.docs".to_string())),
    ] {
        let m = map_acl_error(e).client_message();
        assert!(!m.contains("#12"), "{m}");
    }
    let corrupt = map_acl_error(AclChangeError::Catalog(CatalogError::CorruptSchema(
        "secret #1.f".to_string(),
    )));
    assert_eq!(corrupt.wire_code(), "XX000");
    assert!(!corrupt.client_message().contains("secret"));
}
