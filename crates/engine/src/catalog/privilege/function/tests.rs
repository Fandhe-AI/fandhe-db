//! `catalog::privilege::function` の単体テスト（SQL-40・RLS-12 のポインタ、Issue #2235）。
//!
//! 暗黙の既定（印が無ければ PUBLIC が EXECUTE を持つ）・印がある状態の行だけの判定・「印が無いのに行がある」
//! 不整合の fail-closed・削除に伴う行と印の掃除（他テナント・他の関数に触れない）を固定する。

use super::*;
use crate::catalog::privilege::upsert_acl_in_txn;
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

#[test]
fn no_mark_means_public_execute() {
    let (_g, s) = open("fn_acl_default");
    assert_eq!(
        s.function_privileges("t1", 3, &[7]).expect("p"),
        PRIV_EXECUTE
    );
    assert_eq!(
        s.function_privileges("t1", 3, &[]).expect("p"),
        PRIV_EXECUTE
    );
}

#[test]
fn marked_function_is_judged_by_rows_only() {
    let (_g, s) = open("fn_acl_marked");
    write(&s, |t| {
        mark_acl_initialized_in_txn(t, "t1", 3).expect("mark")
    });
    // 行が 0 件なら誰も EXECUTE を持たない（PUBLIC から取り消し済みの状態）。
    assert_eq!(s.function_privileges("t1", 3, &[7]).expect("p"), 0);
    write(&s, |t| {
        upsert_acl_in_txn(
            t,
            "t1",
            &PrivilegeObject::Function(3),
            7,
            1,
            PRIV_EXECUTE,
            0,
        )
        .expect("upsert");
    });
    assert_eq!(
        s.function_privileges("t1", 3, &[7]).expect("p"),
        PRIV_EXECUTE
    );
    assert_eq!(s.function_privileges("t1", 3, &[8]).expect("p"), 0);
    // 他テナントの印・行は影響しない（既定の PUBLIC EXECUTE のまま）。
    assert_eq!(
        s.function_privileges("t2", 3, &[8]).expect("p"),
        PRIV_EXECUTE
    );
}

#[test]
fn owner_keeps_execute_after_revoke_from_public() {
    use crate::catalog::privilege::owner::set_owner_unchecked_in_txn;
    let (_g, s) = open("fn_acl_owner");
    write(&s, |t| {
        set_owner_unchecked_in_txn(t, "t1", &PrivilegeObject::Function(3), 7).expect("owner");
        // ACL 実体化済みで行が 0 件（PUBLIC から取り消し済み）。
        mark_acl_initialized_in_txn(t, "t1", 3).expect("mark")
    });
    assert_eq!(
        s.function_privileges("t1", 3, &[7]).expect("p"),
        PRIV_EXECUTE
    );
    // 所有者以外は EXECUTE できない。
    assert_eq!(s.function_privileges("t1", 3, &[8]).expect("p"), 0);
    assert_eq!(s.function_privileges("t1", 3, &[]).expect("p"), 0);
}

#[test]
fn rows_without_a_mark_fail_closed() {
    let (_g, s) = open("fn_acl_corrupt");
    write(&s, |t| {
        upsert_acl_in_txn(
            t,
            "t1",
            &PrivilegeObject::Function(3),
            7,
            1,
            PRIV_EXECUTE,
            0,
        )
        .expect("upsert");
    });
    assert!(s.function_privileges("t1", 3, &[7]).is_err());
}

#[test]
fn removal_clears_rows_and_mark_only_for_that_function() {
    let (_g, s) = open("fn_acl_remove");
    for id in [3u64, 4] {
        write(&s, |t| {
            mark_acl_initialized_in_txn(t, "t1", id).expect("mark");
            upsert_acl_in_txn(
                t,
                "t1",
                &PrivilegeObject::Function(id),
                7,
                1,
                PRIV_EXECUTE,
                0,
            )
            .expect("upsert");
        });
    }
    write(&s, |t| {
        mark_acl_initialized_in_txn(t, "t2", 3).expect("mark");
    });
    write(&s, |t| {
        remove_function_acl_in_txn(t, "t1", 3).expect("remove")
    });
    // 削除した関数は印も行も無い（新しい同 ID は無いが、状態は既定へ戻る）。
    assert!(s
        .list_acl_for_object("t1", &PrivilegeObject::Function(3))
        .expect("l")
        .is_empty());
    assert_eq!(
        s.function_privileges("t1", 3, &[7]).expect("p"),
        PRIV_EXECUTE
    );
    // 他の関数・他テナントは残る。
    assert_eq!(
        s.function_privileges("t1", 4, &[7]).expect("p"),
        PRIV_EXECUTE
    );
    assert_eq!(s.function_privileges("t1", 4, &[8]).expect("p"), 0);
    assert_eq!(s.function_privileges("t2", 3, &[8]).expect("p"), 0);
}
