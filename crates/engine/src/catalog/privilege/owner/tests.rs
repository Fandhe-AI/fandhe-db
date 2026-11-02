//! `catalog::privilege::owner` の単体テスト（SQL-40・RLS-12 のポインタ、Issue #2236）。
//!
//! 所有者行の往復・テナント分離・既存所有者を変えない書き込み・relation の掃除と改名・破損 blob の
//! fail-closed・`DROP ROLE` の依存判定を固定する。

use super::*;
use crate::catalog::Storage;
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

fn read(s: &Storage) -> redb::ReadTransaction {
    redb::ReadableDatabase::begin_read(s.db()).expect("read")
}

fn tbl(name: &str) -> PrivilegeObject {
    PrivilegeObject::Table(name.to_string())
}

#[test]
fn absent_table_means_no_owner() {
    let (_g, s) = open("owner_absent");
    let rt = read(&s);
    assert_eq!(owner_of_in_txn(&rt, "t1", &tbl("t")).expect("o"), None);
    assert!(!is_owner_in_txn(&rt, "t1", &tbl("t"), 1).expect("o"));
}

#[test]
fn roundtrip_and_tenant_isolation() {
    let (_g, s) = open("owner_roundtrip");
    write(&s, |t| {
        set_owner_unchecked_in_txn(t, "t1", &tbl("t"), 7).expect("set")
    });
    let rt = read(&s);
    assert!(is_owner_in_txn(&rt, "t1", &tbl("t"), 7).expect("o"));
    assert!(!is_owner_in_txn(&rt, "t1", &tbl("t"), 8).expect("o"));
    // 他テナントには所有者が現れない。
    assert_eq!(owner_of_in_txn(&rt, "t2", &tbl("t")).expect("o"), None);
    // 列は親の表の所有者を見る。
    let col = PrivilegeObject::Column {
        table: "t".to_string(),
        column: "c".to_string(),
    };
    assert!(is_owner_in_txn(&rt, "t1", &col, 7).expect("o"));
    // PUBLIC（0）は所有者にならない。
    assert!(!is_owner_in_txn(&rt, "t1", &tbl("t"), 0).expect("o"));
}

#[test]
fn set_owner_rejects_zero_and_if_absent_keeps_existing() {
    let (_g, s) = open("owner_if_absent");
    let txn = s.begin_write_txn().expect("begin");
    assert!(set_owner_unchecked_in_txn(&txn, "t1", &tbl("t"), 0).is_err());
    let if_absent = |id: u64| {
        if owner_of_in_txn(&*txn, "t1", &tbl("t"))
            .expect("o")
            .is_none()
        {
            set_owner_unchecked_in_txn(&txn, "t1", &tbl("t"), id).expect("set");
        }
    };
    if_absent(3);
    if_absent(4);
    assert_eq!(owner_of_in_txn(&*txn, "t1", &tbl("t")).expect("o"), Some(3));
    set_owner_unchecked_in_txn(&txn, "t1", &tbl("t"), 4).expect("replace");
    assert_eq!(owner_of_in_txn(&*txn, "t1", &tbl("t")).expect("o"), Some(4));
}

#[test]
fn schema_and_function_owners_are_independent_kinds() {
    let (_g, s) = open("owner_kinds");
    write(&s, |t| {
        set_owner_unchecked_in_txn(t, "t1", &PrivilegeObject::Schema(5), 1).expect("s");
        set_owner_unchecked_in_txn(t, "t1", &PrivilegeObject::Function(5), 2).expect("f");
    });
    let rt = read(&s);
    assert_eq!(
        owner_of_in_txn(&rt, "t1", &PrivilegeObject::Schema(5)).expect("o"),
        Some(1)
    );
    assert_eq!(
        owner_of_in_txn(&rt, "t1", &PrivilegeObject::Function(5)).expect("o"),
        Some(2)
    );
    // SEQUENCE は所有者を持てない対象（本 Issue では未対応）。
    assert!(owner_of_in_txn(&rt, "t1", &PrivilegeObject::Sequence(5)).is_err());
}

#[test]
fn remove_relation_owners_sweeps_all_tenants_only_for_that_key() {
    let (_g, s) = open("owner_sweep");
    write(&s, |t| {
        set_owner_unchecked_in_txn(t, "t1", &tbl("a"), 1).expect("a1");
        set_owner_unchecked_in_txn(t, "t2", &tbl("a"), 2).expect("a2");
        set_owner_unchecked_in_txn(t, "t1", &tbl("b"), 1).expect("b1");
    });
    write(&s, |t| remove_relation_owners_in_txn(t, "a").expect("rm"));
    let rt = read(&s);
    assert_eq!(owner_of_in_txn(&rt, "t1", &tbl("a")).expect("o"), None);
    assert_eq!(owner_of_in_txn(&rt, "t2", &tbl("a")).expect("o"), None);
    assert_eq!(owner_of_in_txn(&rt, "t1", &tbl("b")).expect("o"), Some(1));
}

#[test]
fn rename_moves_owners_and_drops_orphans_at_destination() {
    let (_g, s) = open("owner_rename");
    write(&s, |t| {
        set_owner_unchecked_in_txn(t, "t1", &tbl("old"), 1).expect("o1");
        set_owner_unchecked_in_txn(t, "t2", &tbl("old"), 2).expect("o2");
        set_owner_unchecked_in_txn(t, "t3", &tbl("new"), 9).expect("orphan");
    });
    write(&s, |t| {
        rename_relation_owners_in_txn(t, "old", "new").expect("mv")
    });
    let rt = read(&s);
    assert_eq!(owner_of_in_txn(&rt, "t1", &tbl("old")).expect("o"), None);
    assert_eq!(owner_of_in_txn(&rt, "t1", &tbl("new")).expect("o"), Some(1));
    assert_eq!(owner_of_in_txn(&rt, "t2", &tbl("new")).expect("o"), Some(2));
    assert_eq!(owner_of_in_txn(&rt, "t3", &tbl("new")).expect("o"), None);
}

#[test]
fn corrupt_blob_is_fail_closed() {
    let (_g, s) = open("owner_corrupt");
    for bad in [
        vec![],
        vec![1u8],
        vec![2u8, 1, 0, 0, 0, 0, 0, 0, 0],
        vec![1u8, 0, 0, 0, 0, 0, 0, 0, 0],
        vec![1u8, 1, 0, 0, 0, 0, 0, 0, 0, 0],
    ] {
        write(&s, |t| {
            let mut table = t.open_table(OWNERS_TABLE).expect("open");
            table
                .insert((KIND_TABLE, "t", "t1"), bad.as_slice())
                .expect("insert");
        });
        let rt = read(&s);
        assert!(matches!(
            owner_of_in_txn(&rt, "t1", &tbl("t")),
            Err(CatalogError::CorruptSchema(_))
        ));
    }
}

#[test]
fn references_role_is_tenant_scoped() {
    let (_g, s) = open("owner_refs");
    write(&s, |t| {
        set_owner_unchecked_in_txn(t, "t1", &PrivilegeObject::Schema(1), 7).expect("s");
    });
    let rt = read(&s);
    assert!(owner_references_role_in_txn(&rt, "t1", 7).expect("r"));
    assert!(!owner_references_role_in_txn(&rt, "t1", 8).expect("r"));
    // 同じ ID のロールでも他テナントの所有者行は依存にならない。
    assert!(!owner_references_role_in_txn(&rt, "t2", 7).expect("r"));
    assert!(!owner_references_role_in_txn(&rt, "t1", 0).expect("r"));
}

#[test]
fn checked_set_owner_rejects_a_role_that_does_not_exist_in_the_txn() {
    let (_g, s) = open("owner_role_missing");
    let txn = s.begin_write_txn().expect("begin");
    // ロール表が空の txn では、存在しないロールを所有者にできない（行も残らない）。
    assert!(set_owner_in_txn(&txn, "t1", &tbl("t"), 5).is_err());
    assert!(set_owner_if_absent_in_txn(&txn, "t1", &tbl("t"), 5).is_err());
    assert_eq!(owner_of_in_txn(&*txn, "t1", &tbl("t")).expect("o"), None);
}
