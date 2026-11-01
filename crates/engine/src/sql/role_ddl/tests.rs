//! `sql::role_ddl` の crate 内テスト（SQL-39 のポインタ、Issue #1962）。
//!
//! 保存された検証子の形式（平文から導出されたこと）・メンバーシップの掃除・ACL 依存による `2BP01`・
//! テナント境界・ID の非再利用・平文の非漏えいを、`pub(crate)` のカタログ API で直接確かめる。
//! SQL 表層からの経路（権限ゲート・明示 Tx・タグ）は `tests/table_rls/sql39_role_ddl.rs` が担う。

use super::*;
use crate::catalog::privilege::{upsert_acl_in_txn, PrivilegeObject, PRIV_SELECT};
use crate::catalog::role::{
    get_role_in_txn, put_membership_in_txn, MembershipOptions, RoleAttributes,
};
use crate::sql::allowlist::{validate_create_role_tokens, validate_drop_role_tokens};
use crate::sql::lexer::tokenize;
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn open(label: &str) -> (CleanupGuard, Storage) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let s = Storage::open(&path).expect("open");
    (guard, s)
}

fn create(s: &Storage, tenant: &str, sql: &str) -> Result<CreateRoleOutcome, SqlSurfaceError> {
    let stmt = validate_create_role_tokens(&tokenize(sql).expect("lex"))?;
    execute_create_role(s, tenant, &SessionState::default(), &stmt)
}

fn drop_(s: &Storage, tenant: &str, sql: &str) -> Result<DropRoleOutcome, SqlSurfaceError> {
    let stmt = validate_drop_role_tokens(&tokenize(sql).expect("lex"))?;
    execute_drop_role(s, tenant, &stmt)
}

fn code<T: std::fmt::Debug>(r: Result<T, SqlSurfaceError>) -> &'static str {
    r.expect_err("must fail").wire_code()
}

/// 専用表（ロール・メンバーシップ・ACL）だけを書く txn を commit する（表の世代の対象外。呼び出しを 1 か所に集める）。
fn finish_txn(txn: crate::storage::GatedWriteTxn) {
    crate::recovery::commit_boundary::commit(txn).expect("commit");
}

fn role(s: &Storage, tenant: &str, name: &str) -> RoleDef {
    s.get_role(tenant, name).expect("get").expect("exists")
}

#[test]
fn stored_verifier_is_derived_from_the_plaintext() {
    let (_g, s) = open("role-ddl-verifier");
    create(&s, "t1", "CREATE ROLE r PASSWORD 'correct horse'").expect("create");
    let def = role(&s, "t1", "r");
    assert!(def.legacy_phc.is_empty());
    let text = String::from_utf8(def.scram_verifier.clone()).expect("ascii");
    let parsed = ScramVerifier::parse(&text).expect("parse");
    assert_eq!(parsed.iterations, SCRAM_ITERATIONS);
    assert_eq!(parsed.salt.len(), SALT_LEN);
    let regenerated =
        generate_verifier(b"correct horse", &parsed.salt, parsed.iterations).expect("gen");
    assert_eq!(regenerated, parsed);
    assert!(!text.contains("correct horse"));
}

#[test]
fn salts_differ_between_roles() {
    let (_g, s) = open("role-ddl-salt");
    create(&s, "t1", "CREATE ROLE a PASSWORD 'same'").expect("a");
    create(&s, "t1", "CREATE ROLE b PASSWORD 'same'").expect("b");
    assert_ne!(
        role(&s, "t1", "a").scram_verifier,
        role(&s, "t1", "b").scram_verifier
    );
}

#[test]
fn no_password_and_empty_password_store_no_verifier() {
    let (_g, s) = open("role-ddl-nopw");
    create(&s, "t1", "CREATE ROLE a").expect("a");
    create(&s, "t1", "CREATE ROLE b PASSWORD ''").expect("b");
    create(&s, "t1", "CREATE ROLE c PASSWORD NULL").expect("c");
    for n in ["a", "b", "c"] {
        assert!(role(&s, "t1", n).scram_verifier.is_empty(), "{n}");
    }
}

#[test]
fn prehashed_verifier_is_stored_and_malformed_ones_are_rejected() {
    let (_g, s) = open("role-ddl-prehashed");
    let salt = [7u8; SALT_LEN];
    let v = generate_verifier(b"pw", &salt, SCRAM_ITERATIONS)
        .expect("gen")
        .to_verifier_string();
    create(&s, "t1", &format!("CREATE ROLE a PASSWORD '{v}'")).expect("a");
    assert_eq!(role(&s, "t1", "a").scram_verifier, v.into_bytes());

    // 接頭辞があるが条件を満たさない形は平文として扱わず拒否する。
    for bad in [
        "SCRAM-SHA-256$garbage",
        "SCRAM-SHA-256$1:AAAA$BBBB:CCCC",
        "SCRAM-SHA-256$4096:AAAAAAAAAAAAAAAAAAAAAA==$short:short",
    ] {
        assert_eq!(
            code(create(
                &s,
                "t1",
                &format!("CREATE ROLE bad PASSWORD '{bad}'")
            )),
            "22023",
            "{bad}"
        );
    }
    let md5 = format!("md5{}", "a".repeat(32));
    assert_eq!(
        code(create(&s, "t1", &format!("CREATE ROLE m PASSWORD '{md5}'"))),
        "0A000"
    );
    assert!(s.get_role("t1", "bad").expect("get").is_none());
}

#[test]
fn unusable_passwords_are_rejected_without_creating_the_role() {
    let (_g, s) = open("role-ddl-badpw");
    assert_eq!(
        code(create(&s, "t1", "CREATE ROLE a PASSWORD 'pässword'")),
        "22023"
    );
    let long = "x".repeat(MAX_PASSWORD_BYTES + 1);
    assert_eq!(
        code(create(
            &s,
            "t1",
            &format!("CREATE ROLE b PASSWORD '{long}'")
        )),
        "22023"
    );
    assert!(s.list_roles("t1").expect("list").is_empty());
}

#[test]
fn password_errors_do_not_echo_the_plaintext() {
    let (_g, s) = open("role-ddl-noecho");
    let marker = "ZZ-marker-ZZ-é";
    let e = create(&s, "t1", &format!("CREATE ROLE a PASSWORD '{marker}'")).expect_err("non-ascii");
    assert!(!e.to_string().contains("ZZ-marker"), "{e}");
    assert!(!format!("{e:?}").contains("ZZ-marker"));
    assert!(!e.client_message().contains("ZZ-marker"));
}

#[test]
fn duplicate_is_per_tenant() {
    let (_g, s) = open("role-ddl-dup");
    create(&s, "t1", "CREATE ROLE r").expect("first");
    assert_eq!(code(create(&s, "t1", "CREATE USER r")), "42710");
    create(&s, "t2", "CREATE ROLE r").expect("other tenant is independent");
}

#[test]
fn attributes_are_persisted() {
    let (_g, s) = open("role-ddl-attrs");
    create(&s, "t1", "CREATE USER u CREATEDB NOINHERIT CREATEROLE").expect("u");
    assert_eq!(
        role(&s, "t1", "u").attributes,
        RoleAttributes {
            login: true,
            inherit: false,
            createrole: true,
            createdb: true
        }
    );
    create(&s, "t1", "CREATE ROLE r").expect("r");
    let a = role(&s, "t1", "r").attributes;
    assert!(!a.login && a.inherit && !a.createrole && !a.createdb);
}

#[test]
fn drop_missing_and_cross_tenant_roles_look_the_same() {
    let (_g, s) = open("role-ddl-drop-missing");
    create(&s, "t1", "CREATE ROLE r").expect("create");
    let missing = drop_(&s, "t2", "DROP ROLE r").expect_err("cross tenant");
    assert_eq!(missing.wire_code(), "42704");
    assert_eq!(missing.client_message(), "role \"r\" does not exist");
    let absent = drop_(&s, "t2", "DROP ROLE nosuch").expect_err("absent");
    assert_eq!(absent.client_message(), "role \"nosuch\" does not exist");
    assert!(drop_(&s, "t2", "DROP ROLE IF EXISTS r")
        .expect("skip")
        .skipped());
    assert!(s.get_role("t1", "r").expect("get").is_some());
}

#[test]
fn drop_is_atomic_across_names() {
    let (_g, s) = open("role-ddl-atomic");
    create(&s, "t1", "CREATE ROLE a").expect("a");
    assert_eq!(code(drop_(&s, "t1", "DROP ROLE a, b")), "42704");
    assert!(s.get_role("t1", "a").expect("get").is_some());
    let out = drop_(&s, "t1", "DROP ROLE IF EXISTS a, b").expect("ok");
    assert!(out.skipped());
    assert!(s.get_role("t1", "a").expect("get").is_none());
}

#[test]
fn drop_removes_memberships_in_both_directions_only_in_its_tenant() {
    let (_g, s) = open("role-ddl-members");
    for t in ["t1", "t2"] {
        for n in ["a", "b", "c"] {
            create(&s, t, &format!("CREATE ROLE {n}")).expect("create");
        }
    }
    let ids = |t: &str| ["a", "b", "c"].map(|n| role(&s, t, n).id);
    let [a1, b1, c1] = ids("t1");
    let [a2, b2, _] = ids("t2");
    let opts = MembershipOptions {
        admin: false,
        inherit: true,
        set: true,
        grantor_id: None,
    };
    let txn = s.begin_write_txn().expect("begin");
    // a は b のメンバー、c は a のメンバー、b と c は別の関係（無関係に残る）。
    put_membership_in_txn(&txn, "t1", a1, b1, opts).expect("a in b");
    put_membership_in_txn(&txn, "t1", c1, a1, opts).expect("c in a");
    put_membership_in_txn(&txn, "t1", c1, b1, opts).expect("c in b");
    put_membership_in_txn(&txn, "t2", a2, b2, opts).expect("t2 a in b");
    finish_txn(txn);

    drop_(&s, "t1", "DROP ROLE a").expect("drop");
    use redb::ReadableDatabase as _;
    let rt = s.db().begin_read().expect("read");
    use crate::catalog::role::list_memberships_of_member_in_txn as list;
    assert!(list(&rt, "t1", a1).expect("a").is_empty());
    assert_eq!(
        list(&rt, "t1", c1)
            .expect("c")
            .iter()
            .map(|(g, _)| *g)
            .collect::<Vec<_>>(),
        vec![b1]
    );
    assert_eq!(list(&rt, "t2", a2).expect("t2 a").len(), 1);
    assert!(get_role_in_txn(&rt, "t1", "a").expect("get").is_none());
}

#[test]
fn drop_is_refused_while_acl_references_the_role() {
    let (_g, s) = open("role-ddl-acl");
    for n in ["grantee", "grantor", "free"] {
        create(&s, "t1", &format!("CREATE ROLE {n}")).expect("create");
    }
    let grantee = role(&s, "t1", "grantee").id;
    let grantor = role(&s, "t1", "grantor").id;
    let txn = s.begin_write_txn().expect("begin");
    upsert_acl_in_txn(
        &txn,
        "t1",
        &PrivilegeObject::Table("docs".to_string()),
        grantee,
        grantor,
        PRIV_SELECT,
        0,
    )
    .expect("acl");
    finish_txn(txn);

    for n in ["grantee", "grantor"] {
        let e = drop_(&s, "t1", &format!("DROP ROLE {n}")).expect_err("dependents");
        assert_eq!(e.wire_code(), "2BP01");
        let m = e.client_message();
        assert!(!m.contains("docs") && !m.contains("select"), "{m}");
        assert!(s.get_role("t1", n).expect("get").is_some());
    }
    drop_(&s, "t1", "DROP ROLE free").expect("unreferenced role drops");
    // 他テナントの同名ロールは同じ ID でも ACL の影響を受けない（テナントの範囲だけを走査する）。
    create(&s, "t2", "CREATE ROLE grantee").expect("t2");
    drop_(&s, "t2", "DROP ROLE grantee").expect("t2 drop");
}

#[test]
fn role_ids_are_not_reused_after_drop() {
    let (_g, s) = open("role-ddl-ids");
    create(&s, "t1", "CREATE ROLE r").expect("first");
    let first = role(&s, "t1", "r").id;
    drop_(&s, "t1", "DROP ROLE r").expect("drop");
    create(&s, "t1", "CREATE ROLE r").expect("recreate");
    assert!(role(&s, "t1", "r").id > first);
}

#[test]
fn catalog_errors_map_to_fixed_sqlstates() {
    let c = |e| map_role_error(e, "r").wire_code();
    assert_eq!(
        c(CatalogError::Invalid(
            ROLE_ALREADY_EXISTS_MESSAGE.to_string()
        )),
        "42710"
    );
    assert_eq!(
        c(CatalogError::Invalid(
            ROLE_HAS_DEPENDENTS_MESSAGE.to_string()
        )),
        "2BP01"
    );
    assert_eq!(c(CatalogError::Invalid("x".to_string())), "22000");
    assert_eq!(c(CatalogError::ViewLimitExceeded("x".to_string())), "54000");
    assert_eq!(c(CatalogError::WriteLockTimeout), "55P03");
    let internal = map_role_error(CatalogError::CorruptSchema("secret #1".to_string()), "r");
    assert_eq!(internal.wire_code(), "XX000");
    assert!(!internal.client_message().contains("secret"));
}

#[test]
fn plaintext_never_reaches_the_database_file() {
    let path = unique_db_path("role-ddl-file");
    let _g = CleanupGuard(path.clone());
    {
        let s = Storage::open(&path).expect("open");
        create(&s, "t1", "CREATE ROLE r PASSWORD 'UNIQUE-PW-MARKER-9137'").expect("create");
    }
    let bytes = std::fs::read(&path).expect("read db file");
    let needle = b"UNIQUE-PW-MARKER-9137";
    assert!(!bytes.windows(needle.len()).any(|w| w == needle));
}
