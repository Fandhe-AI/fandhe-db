//! `catalog::role::bootstrap` の単体テスト（SQL-39 (3)(5)・Issue #2238）。状態表の全セル・冪等性・
//! 原子性・blob の破損検出・`DROP ROLE` での掃除を Storage 水準で固定する。

use super::*;
use crate::catalog::role::{drop_role_in_txn, TenantRoleLookup};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
use redb::ReadableDatabase;

fn open(label: &str) -> (CleanupGuard, Storage) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    (guard, Storage::open(&path).expect("open"))
}

fn user(line: usize, name: &str, tenant: &str, ddl: bool) -> BootstrapUser {
    BootstrapUser {
        line,
        username: name.to_string(),
        tenant_id: tenant.to_string(),
        phc: format!("$argon2id$dummy-{name}"),
        scram_verifier: None,
        ddl_listed: ddl,
    }
}

fn role(s: &Storage, tenant: &str, name: &str) -> Option<RoleDef> {
    s.get_role(tenant, name).expect("get")
}

fn privileges(s: &Storage, tenant: &str, id: u64) -> BootstrapPrivileges {
    let rt = s.db().begin_read().expect("read");
    bootstrap_privileges_in_txn(&rt, tenant, id).expect("priv")
}

fn marked(s: &Storage, tenant: &str) -> bool {
    let rt = s.db().begin_read().expect("read");
    tenant_bootstrapped_in_txn(&rt, tenant).expect("marker")
}

/// 専用表（ロール・マーカー・初期権限）だけを書く txn を commit する（表の世代の対象外。呼び出しを 1 か所に集める）。
fn finish(w: crate::storage::GatedWriteTxn) {
    crate::recovery::commit_boundary::commit(w).expect("commit");
}

fn drop_roles_now(s: &Storage, tenant: &str, names: &[&str]) {
    let w = s.begin_write_txn().expect("w");
    for n in names {
        drop_role_in_txn(&w, tenant, n).expect("drop");
    }
    finish(w);
}

#[test]
fn empty_tenant_is_bootstrapped_with_privileges_and_marker() {
    let (_g, s) = open("bootstrap-empty");
    let users = [user(1, "alice", "t1", true), user(2, "bob", "t1", false)];
    let r = s.bootstrap_users_file(&users).expect("bootstrap");
    assert_eq!(r.tenants_bootstrapped, 1);
    assert_eq!(r.tenants_adopted, 0);
    assert!(marked(&s, "t1"));
    let alice = role(&s, "t1", "alice").expect("alice");
    assert!(alice.attributes.login && alice.attributes.createrole && alice.attributes.inherit);
    assert_eq!(alice.legacy_phc, b"$argon2id$dummy-alice");
    assert_eq!(
        privileges(&s, "t1", alice.id),
        BootstrapPrivileges {
            all_object_privileges: true,
            ddl: true
        }
    );
    let bob = role(&s, "t1", "bob").expect("bob");
    assert!(!privileges(&s, "t1", bob.id).ddl);
    assert!(privileges(&s, "t1", bob.id).all_object_privileges);
}

#[test]
fn second_run_is_idempotent_and_never_recreates_dropped_roles() {
    let (_g, s) = open("bootstrap-idem");
    let users = [user(1, "alice", "t1", false), user(2, "bob", "t1", false)];
    s.bootstrap_users_file(&users).expect("first");
    let again = s.bootstrap_users_file(&users).expect("second");
    assert_eq!(again.tenants_bootstrapped, 0);
    assert!(again.entries.is_empty());
    // 全ロールを消しても、マーカーがあるので再投入しない。
    drop_roles_now(&s, "t1", &["alice", "bob"]);
    let after = s.bootstrap_users_file(&users).expect("third");
    assert_eq!(after.tenants_bootstrapped, 0);
    assert_eq!(
        after.entries.iter().map(|e| e.kind).collect::<Vec<_>>(),
        vec![BootstrapEntryKind::RoleMissing; 2]
    );
    assert!(role(&s, "t1", "alice").is_none());
    assert!(!s.tenant_has_roles("t1").expect("has"));
}

#[test]
fn drop_role_removes_initial_privileges_row() {
    let (_g, s) = open("bootstrap-drop");
    s.bootstrap_users_file(&[user(1, "alice", "t1", true)])
        .expect("boot");
    let id = role(&s, "t1", "alice").expect("alice").id;
    drop_roles_now(&s, "t1", &["alice"]);
    assert_eq!(privileges(&s, "t1", id), BootstrapPrivileges::default());
}

#[test]
fn existing_roles_without_marker_are_adopted_without_widening() {
    let (_g, s) = open("bootstrap-adopt");
    let mut def = RoleDef {
        id: 0,
        name: "alice".to_string(),
        attributes: RoleAttributes {
            login: true,
            ..RoleAttributes::default()
        },
        connection_limit: -1,
        valid_until: None,
        scram_verifier: Vec::new(),
        legacy_phc: Vec::new(),
    };
    s.create_role("t1", &def).expect("alice");
    def.name = "bob".to_string();
    def.legacy_phc = b"catalog-phc".to_vec();
    s.create_role("t1", &def).expect("bob");
    let users = [
        user(1, "alice", "t1", true),
        user(2, "bob", "t1", false),
        user(3, "carol", "t1", false),
    ];
    let r = s.bootstrap_users_file(&users).expect("adopt");
    assert_eq!(r.tenants_adopted, 1);
    assert!(marked(&s, "t1"));
    // 資格情報が空の同名ロールだけが補われ、全権限は付かず、列挙された行だけ DDL が付く。
    let alice = role(&s, "t1", "alice").expect("alice");
    assert_eq!(alice.legacy_phc, b"$argon2id$dummy-alice");
    assert_eq!(
        privileges(&s, "t1", alice.id),
        BootstrapPrivileges {
            all_object_privileges: false,
            ddl: true
        }
    );
    let bob = role(&s, "t1", "bob").expect("bob");
    assert_eq!(bob.legacy_phc, b"catalog-phc");
    assert_eq!(privileges(&s, "t1", bob.id), BootstrapPrivileges::default());
    assert!(role(&s, "t1", "carol").is_none());
    let kinds: Vec<_> = r.entries.iter().map(|e| (e.line, e.kind)).collect();
    assert_eq!(
        kinds,
        vec![
            (1, BootstrapEntryKind::AdoptedCredentials),
            (2, BootstrapEntryKind::CredentialMismatch),
            (3, BootstrapEntryKind::RoleMissing),
        ]
    );
}

#[test]
fn marked_tenant_reports_mismatches_and_writes_nothing() {
    let (_g, s) = open("bootstrap-mismatch");
    s.bootstrap_users_file(&[user(1, "alice", "t1", false)])
        .expect("boot");
    let mut changed = user(1, "alice", "t1", true);
    changed.phc = "$argon2id$other".to_string();
    let extra = user(2, "mallory", "t1", true);
    let r = s.bootstrap_users_file(&[changed, extra]).expect("again");
    let kinds: Vec<_> = r.entries.iter().map(|e| (e.line, e.kind)).collect();
    assert_eq!(
        kinds,
        vec![
            (1, BootstrapEntryKind::CredentialMismatch),
            (1, BootstrapEntryKind::DdlNotApplied),
            (2, BootstrapEntryKind::RoleMissing),
        ]
    );
    assert!(role(&s, "t1", "mallory").is_none());
    let alice = role(&s, "t1", "alice").expect("alice");
    assert_eq!(alice.legacy_phc, b"$argon2id$dummy-alice");
    assert!(!privileges(&s, "t1", alice.id).ddl);
}

#[test]
fn invalid_row_aborts_everything() {
    let (_g, s) = open("bootstrap-atomic");
    let users = [
        user(1, "alice", "t1", false),
        user(2, "bad name", "t2", false),
    ];
    let e = s.bootstrap_users_file(&users).expect_err("must fail");
    assert_eq!(e, BootstrapError::InvalidUser { line: 2 });
    assert!(!marked(&s, "t1"));
    assert!(role(&s, "t1", "alice").is_none());
}

#[test]
fn tenants_are_isolated() {
    let (_g, s) = open("bootstrap-tenants");
    s.bootstrap_users_file(&[user(1, "alice", "t1", false), user(2, "alice", "t2", false)])
        .expect("boot");
    let a1 = role(&s, "t1", "alice").expect("t1").id;
    let a2 = role(&s, "t2", "alice").expect("t2").id;
    assert_ne!(a1, a2);
    assert!(marked(&s, "t1") && marked(&s, "t2") && !marked(&s, "t3"));
    assert_eq!(
        s.resolve_tenant_role("t3", "alice").expect("t3"),
        TenantRoleLookup::NotAuthoritative
    );
}

#[test]
fn resolve_tenant_role_is_authoritative_with_marker_even_without_roles() {
    let (_g, s) = open("bootstrap-authoritative");
    s.bootstrap_users_file(&[user(1, "alice", "t1", false)])
        .expect("boot");
    drop_roles_now(&s, "t1", &["alice"]);
    assert_eq!(
        s.resolve_tenant_role("t1", "alice").expect("lookup"),
        TenantRoleLookup::Missing { bootstrapped: true }
    );
}

#[test]
fn corrupt_marker_and_privilege_blobs_fail_closed() {
    let (_g, s) = open("bootstrap-corrupt");
    s.bootstrap_users_file(&[user(1, "alice", "t1", false)])
        .expect("boot");
    let id = role(&s, "t1", "alice").expect("alice").id;
    let put = |marker: Option<&[u8]>, privs: Option<&[u8]>| {
        let w = s.begin_write_txn().expect("w");
        if let Some(m) = marker {
            w.open_table(BOOTSTRAP_MARKER_TABLE)
                .expect("t")
                .insert("t1", m)
                .expect("ins");
        }
        if let Some(p) = privs {
            w.open_table(BOOTSTRAP_PRIVILEGES_TABLE)
                .expect("t")
                .insert(("t1", id), p)
                .expect("ins");
        }
        finish(w);
    };
    put(None, Some(&[1, 0b100]));
    assert!(s.resolve_tenant_role("t1", "alice").is_err());
    put(None, Some(&[9, 1]));
    assert!(s.resolve_tenant_role("t1", "alice").is_err());
    put(None, Some(&[1]));
    assert!(s.resolve_tenant_role("t1", "alice").is_err());
    put(Some(&[2]), None);
    assert!(s.resolve_tenant_role("t1", "alice").is_err());
    put(Some(&[1, 1]), None);
    assert!(s.resolve_tenant_role("t1", "alice").is_err());
}

#[test]
fn legacy_db_with_empty_tenant_is_not_bootstrapped_but_marked() {
    let (_g, s) = open("bootstrap-legacy-empty");
    // マーカー導入前の既存 DB を模す: t0 にロールがありロール表はあるが、マーカー表は無い。
    let w = s.begin_write_txn().expect("w");
    create_role_in_txn(&w, "t0", &initial_role_def(&user(1, "root", "t0", false))).expect("role");
    finish(w);
    let users = [user(1, "root", "t0", false), user(2, "alice", "t1", true)];
    let r = s.bootstrap_users_file(&users).expect("bootstrap");
    assert_eq!(r.tenants_adopted, 1);
    assert_eq!(r.tenants_skipped, 1);
    assert_eq!(r.tenants_bootstrapped, 0);
    assert!(role(&s, "t1", "alice").is_none());
    assert!(marked(&s, "t1"));
    assert!(r
        .entries
        .iter()
        .any(|e| e.kind == BootstrapEntryKind::LegacyEmptyTenantSkipped));
    // 2 回目以降も再投入しない。
    let again = s.bootstrap_users_file(&users).expect("again");
    assert_eq!(again.tenants_bootstrapped, 0);
    assert!(role(&s, "t1", "alice").is_none());
}

#[test]
fn fresh_db_bootstraps_all_tenants_in_one_run() {
    let (_g, s) = open("bootstrap-fresh-multi");
    let users = [user(1, "a", "t1", false), user(2, "b", "t2", false)];
    let r = s.bootstrap_users_file(&users).expect("bootstrap");
    assert_eq!(r.tenants_bootstrapped, 2);
    assert_eq!(r.tenants_skipped, 0);
}

#[test]
fn legacy_db_tenant_absent_from_file_is_protected_from_later_reseed() {
    let (_g, s) = open("bootstrap-legacy-outside");
    // マーカー導入前の既存 DB: t2 にロールがあるが、今回のファイルには現れない。
    let w = s.begin_write_txn().expect("w");
    create_role_in_txn(&w, "t2", &initial_role_def(&user(1, "bob", "t2", false))).expect("role");
    finish(w);
    let first = [user(1, "root", "t1", false)];
    let r = s.bootstrap_users_file(&first).expect("bootstrap");
    assert_eq!(r.tenants_protected, 1);
    assert!(marked(&s, "t2"));
    // t2 の全ロールを削除してから、古い行がファイルへ戻っても再投入しない。
    drop_roles_now(&s, "t2", &["bob"]);
    let later = [user(1, "root", "t1", false), user(2, "bob", "t2", true)];
    let again = s.bootstrap_users_file(&later).expect("again");
    assert_eq!(again.tenants_bootstrapped, 0);
    assert!(role(&s, "t2", "bob").is_none());
}
