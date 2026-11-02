//! `catalog::privilege::sequence` の単体テスト（SQL-40・RLS-12 のポインタ、Issue #2235）。
//!
//! SEQUENCE 単位の ACL の読み取り（ロール集合と PUBLIC の和・暗黙の既定なし）・削除に伴う掃除（他の
//! SEQUENCE・他の種別・他テナントの行に触れない）・掃除後に `DROP ROLE` の依存が解けることを固定する。

use super::*;
use crate::catalog::privilege::{
    acl_references_role_in_txn, upsert_acl_in_txn, PRIV_SELECT, PRIV_UPDATE, PRIV_USAGE,
    PUBLIC_GRANTEE_ID,
};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn open(label: &str) -> (CleanupGuard, Storage) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let s = Storage::open(&path).expect("open");
    (guard, s)
}

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
fn privileges_sum_role_rows_and_public_without_implicit_default() {
    let (_g, s) = open("seq_acl_sum");
    let seq = PrivilegeObject::Sequence(5);
    // 暗黙の既定は無い（行が無ければ 0）。
    assert_eq!(s.sequence_privileges("t1", 5, &[7]).expect("p"), 0);
    put(&s, "t1", &seq, 7, PRIV_USAGE);
    put(&s, "t1", &seq, 8, PRIV_SELECT);
    put(&s, "t1", &seq, PUBLIC_GRANTEE_ID, PRIV_UPDATE);
    assert_eq!(
        s.sequence_privileges("t1", 5, &[7]).expect("p"),
        PRIV_USAGE | PRIV_UPDATE
    );
    // 他テナントは構造上引けない。
    assert_eq!(s.sequence_privileges("t2", 5, &[7, 8]).expect("p"), 0);
}

#[test]
fn removal_sweeps_every_tenant_and_only_that_sequence() {
    let (_g, s) = open("seq_acl_sweep");
    let doomed = PrivilegeObject::Sequence(5);
    let other = PrivilegeObject::Sequence(6);
    let schema = PrivilegeObject::Schema(5);
    for t in ["t1", "t2"] {
        put(&s, t, &doomed, 7, PRIV_USAGE);
        put(&s, t, &other, 7, PRIV_USAGE);
    }
    put(&s, "t1", &schema, 7, PRIV_USAGE);
    write(&s, |t| remove_sequence_acl_in_txn(t, 5).expect("sweep"));
    for t in ["t1", "t2"] {
        assert!(s.list_acl_for_object(t, &doomed).expect("l").is_empty());
        assert_eq!(s.list_acl_for_object(t, &other).expect("l").len(), 1);
    }
    // 同じ ID の SCHEMA の行には触れない。
    assert_eq!(s.list_acl_for_object("t1", &schema).expect("l").len(), 1);
}

#[test]
fn removal_releases_the_role_dependency() {
    use redb::ReadableDatabase;
    let (_g, s) = open("seq_acl_role_dep");
    put(&s, "t1", &PrivilegeObject::Sequence(9), 7, PRIV_USAGE);
    let rt = s.db().begin_read().expect("rt");
    assert!(acl_references_role_in_txn(&rt, "t1", 7).expect("ref"));
    drop(rt);
    write(&s, |t| remove_sequence_acl_in_txn(t, 9).expect("sweep"));
    let rt = s.db().begin_read().expect("rt");
    assert!(!acl_references_role_in_txn(&rt, "t1", 7).expect("ref"));
}
