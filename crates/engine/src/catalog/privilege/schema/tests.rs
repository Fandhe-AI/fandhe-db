//! `catalog::privilege::schema` の単体テスト（SQL-40・RLS-12 のポインタ、Issue #2234）。
//!
//! SCHEMA 単位の ACL の読み取り（ロール集合と PUBLIC の和）・`DROP SCHEMA` 相当の掃除（他の種別・他の
//! SCHEMA・他テナントの行に触れない）・`DROP SCHEMA` 経由の掃除と `DROP ROLE` の依存解放を固定する。

use super::*;
use crate::catalog::privilege::{upsert_acl_in_txn, PRIV_CREATE, PRIV_USAGE};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn open(label: &str) -> (CleanupGuard, Storage) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let s = Storage::open(&path).expect("open");
    (guard, s)
}

/// 書き込み txn を開いて `f` を実行し、commit する（テストの commit 呼び出しをここ 1 箇所に集める）。
fn write(s: &Storage, f: impl FnOnce(&redb::WriteTransaction)) {
    let txn = s.begin_write_txn().expect("begin");
    f(&txn);
    crate::recovery::commit_boundary::commit(txn).expect("commit");
}

fn put(s: &Storage, tenant: &str, obj: &PrivilegeObject, grantee: u64, privs: u16) {
    write(s, |t| {
        upsert_acl_in_txn(t, tenant, obj, grantee, 1, privs, 0).expect("upsert");
    });
}

#[test]
fn privileges_sum_role_rows_and_public_only() {
    let (_g, s) = open("schema_acl_sum");
    let sch = PrivilegeObject::Schema(5);
    put(&s, "t1", &sch, 7, PRIV_USAGE);
    put(&s, "t1", &sch, 8, PRIV_CREATE);
    assert_eq!(s.schema_privileges("t1", 5, &[7]).expect("p"), PRIV_USAGE);
    assert_eq!(
        s.schema_privileges("t1", 5, &[7, 8]).expect("p"),
        PRIV_USAGE | PRIV_CREATE
    );
    assert_eq!(s.schema_privileges("t1", 5, &[9]).expect("p"), 0);
    // PUBLIC の行は全ロールに効く。
    put(&s, "t1", &sch, PUBLIC_GRANTEE_ID, PRIV_USAGE);
    assert_eq!(s.schema_privileges("t1", 5, &[9]).expect("p"), PRIV_USAGE);
    // 別 SCHEMA・他テナントには効かない。
    assert_eq!(s.schema_privileges("t1", 6, &[7]).expect("p"), 0);
    assert_eq!(s.schema_privileges("t2", 5, &[7]).expect("p"), 0);
}

#[test]
fn remove_only_touches_the_target_schema_rows_of_the_tenant() {
    let (_g, s) = open("schema_acl_remove");
    put(&s, "t1", &PrivilegeObject::Schema(5), 7, PRIV_USAGE);
    put(&s, "t1", &PrivilegeObject::Schema(5), 8, PRIV_CREATE);
    put(&s, "t1", &PrivilegeObject::Schema(6), 7, PRIV_USAGE);
    put(&s, "t2", &PrivilegeObject::Schema(5), 7, PRIV_USAGE);
    // 同じ文字列キーの別種別（シーケンス）には触れない。
    put(&s, "t1", &PrivilegeObject::Sequence(5), 7, PRIV_USAGE);
    write(&s, |t| {
        remove_schema_acl_in_txn(t, "t1", 5).expect("remove")
    });
    assert_eq!(s.schema_privileges("t1", 5, &[7, 8]).expect("p"), 0);
    assert_eq!(s.schema_privileges("t1", 6, &[7]).expect("p"), PRIV_USAGE);
    assert_eq!(s.schema_privileges("t2", 5, &[7]).expect("p"), PRIV_USAGE);
    assert_eq!(
        s.list_acl_for_object("t1", &PrivilegeObject::Sequence(5))
            .expect("l")
            .len(),
        1
    );
}

#[test]
fn remove_without_acl_table_is_a_noop() {
    let (_g, s) = open("schema_acl_remove_empty");
    let txn = s.begin_write_txn().expect("begin");
    remove_schema_acl_in_txn(&txn, "t1", 5).expect("noop");
}

#[test]
fn drop_schema_releases_acl_rows_so_roles_can_be_dropped() {
    for cascade in [false, true] {
        let (_g, s) = open("schema_acl_drop");
        s.create_schema("t1", "s", false).expect("create");
        let id = s.schema_id("t1", "s").expect("id").expect("some");
        put(&s, "t1", &PrivilegeObject::Schema(id), 7, PRIV_USAGE);
        let txn = s.begin_write_txn().expect("begin");
        assert!(crate::catalog::privilege::acl_references_role_in_txn(&*txn, "t1", 7).expect("r"));
        drop(txn);
        if cascade {
            s.drop_schema_cascade("t1", "s", false).expect("drop");
        } else {
            s.drop_schema("t1", "s", false).expect("drop");
        }
        let txn = s.begin_write_txn().expect("begin");
        assert!(!crate::catalog::privilege::acl_references_role_in_txn(&*txn, "t1", 7).expect("r"));
        assert!(!crate::catalog::privilege::acl_references_role_in_txn(&*txn, "t1", 1).expect("r"));
    }
}
