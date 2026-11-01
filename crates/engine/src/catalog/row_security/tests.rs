//! `catalog::row_security` の単体テスト（RLS-13 のポインタ、Issue #2275）。
//!
//! 受入基準の「4 状態 x 所有者/非所有者で利用者ポリシーを適用するか」の真理値表（PostgreSQL 17 の意味）、
//! アクションごとの遷移、保存 blob の fail-closed、テナントごとの独立、表のライフサイクル追随、
//! 世代の進み方を Storage 水準で固定する。行の見え方の照合は #2276 以降の担当。

use super::*;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};

const ALL_ACTIONS: [RowSecurityAction; 4] = [
    RowSecurityAction::Enable,
    RowSecurityAction::Disable,
    RowSecurityAction::Force,
    RowSecurityAction::NoForce,
];

fn schema(name: &str) -> TableSchema {
    TableSchema::new(name, vec![ColumnDef::new("a", ColumnType::Text, true)])
}

fn open(label: &str) -> (Storage, crate::test_util::temp_db::CleanupGuard) {
    let path = crate::test_util::temp_db::unique_db_path(label);
    let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
    let s = Storage::open(&path).expect("open");
    s.create_table(&schema("t")).expect("create");
    (s, guard)
}

/// 真理値表: `enabled && (!owner || forced)`。FORCE だけでは（ENABLE なしでは）誰にも適用しない。
#[test]
fn truth_table_matches_postgres_semantics() {
    for enabled in [false, true] {
        for forced in [false, true] {
            let st = RowSecurityState { enabled, forced };
            assert_eq!(st.user_policies_apply(false), enabled, "{st:?} non-owner");
            assert_eq!(
                st.user_policies_apply(true),
                enabled && forced,
                "{st:?} owner"
            );
        }
    }
}

/// 遷移: 各アクションは自分のビットだけを変える（4 アクション x 4 初期状態）。
#[test]
fn each_action_changes_only_its_own_bit() {
    for enabled in [false, true] {
        for forced in [false, true] {
            let init = RowSecurityState { enabled, forced };
            for a in ALL_ACTIONS {
                let want = match a {
                    RowSecurityAction::Enable => RowSecurityState {
                        enabled: true,
                        forced,
                    },
                    RowSecurityAction::Disable => RowSecurityState {
                        enabled: false,
                        forced,
                    },
                    RowSecurityAction::Force => RowSecurityState {
                        enabled,
                        forced: true,
                    },
                    RowSecurityAction::NoForce => RowSecurityState {
                        enabled,
                        forced: false,
                    },
                };
                assert_eq!(init.apply(a), want, "{init:?} {a:?}");
            }
        }
    }
}

#[test]
fn blob_roundtrip_and_fail_closed_decode() {
    for enabled in [false, true] {
        for forced in [false, true] {
            let st = RowSecurityState { enabled, forced };
            assert_eq!(RowSecurityState::decode(&st.encode()).expect("rt"), st);
        }
    }
    for bad in [
        &[][..],
        &[1][..],
        &[1, 0, 0][..],
        &[2, 1][..],
        &[0, 1][..],
        &[1, 0b100][..],
        &[1, 0xff][..],
    ] {
        match RowSecurityState::decode(bad) {
            Err(CatalogError::CorruptSchema(m)) => {
                assert_eq!(m, "invalid row security catalog entry");
            }
            other => panic!("{bad:?} must be corrupt: {other:?}"),
        }
    }
}

#[test]
fn transitions_persist_and_only_real_changes_bump_generation() {
    let (s, _g) = open("rs-gen");
    assert_eq!(
        s.table_row_security("a", "t").expect("get"),
        RowSecurityState::default()
    );
    let g0 = s.table_generation("t").expect("gen");
    // FORCE だけでは適用されない状態（enabled=false）。
    assert_eq!(
        s.alter_table_row_security("a", "t", RowSecurityAction::Force)
            .expect("force"),
        AlterApply::Applied
    );
    let st = s.table_row_security("a", "t").expect("get");
    assert!(st.forced && !st.enabled);
    assert!(!st.user_policies_apply(true) && !st.user_policies_apply(false));
    let g1 = s.table_generation("t").expect("gen");
    assert!(g1 > g0);
    // ENABLE で所有者・非所有者の両方に適用。
    s.alter_table_row_security("a", "t", RowSecurityAction::Enable)
        .expect("enable");
    let st = s.table_row_security("a", "t").expect("get");
    assert!(st.user_policies_apply(true) && st.user_policies_apply(false));
    // 再 ENABLE は no-op（世代を進めない）。
    let g2 = s.table_generation("t").expect("gen");
    assert_eq!(
        s.alter_table_row_security("a", "t", RowSecurityAction::Enable)
            .expect("again"),
        AlterApply::Unchanged
    );
    assert_eq!(s.table_generation("t").expect("gen"), g2);
    // NO FORCE で所有者が外れ、DISABLE で全員外れ、行は消える。
    s.alter_table_row_security("a", "t", RowSecurityAction::NoForce)
        .expect("noforce");
    let st = s.table_row_security("a", "t").expect("get");
    assert!(!st.user_policies_apply(true) && st.user_policies_apply(false));
    s.alter_table_row_security("a", "t", RowSecurityAction::Disable)
        .expect("disable");
    assert_eq!(
        s.table_row_security("a", "t").expect("get"),
        RowSecurityState::default()
    );
    let rt = s.db().begin_read().expect("rt");
    let table = ReadSource::open_table(&rt, ROW_SECURITY_TABLE).expect("table");
    assert!(table.get(("t", "a")).expect("get").is_none());
}

#[test]
fn tenants_are_independent_on_a_shared_table() {
    let (s, _g) = open("rs-tenant");
    s.alter_table_row_security("a", "t", RowSecurityAction::Enable)
        .expect("a enable");
    s.alter_table_row_security("a", "t", RowSecurityAction::Force)
        .expect("a force");
    assert_eq!(
        s.table_row_security("b", "t").expect("b"),
        RowSecurityState::default()
    );
    // B の DISABLE（B にとっては no-op）は A に影響しない。
    assert_eq!(
        s.alter_table_row_security("b", "t", RowSecurityAction::Disable)
            .expect("b disable"),
        AlterApply::Unchanged
    );
    assert!(s.table_row_security("a", "t").expect("a").enabled);
    s.alter_table_row_security("b", "t", RowSecurityAction::Enable)
        .expect("b enable");
    s.alter_table_row_security("b", "t", RowSecurityAction::Disable)
        .expect("b disable");
    assert_eq!(
        s.table_row_security("a", "t").expect("a"),
        RowSecurityState {
            enabled: true,
            forced: true
        }
    );
}

#[test]
fn rejects_missing_table_and_bad_scope_without_side_effects() {
    let (s, _g) = open("rs-reject");
    assert!(matches!(
        s.alter_table_row_security("a", "nosuch", RowSecurityAction::Enable),
        Err(CatalogError::TableNotFound(_))
    ));
    assert!(matches!(
        s.alter_table_row_security("", "t", RowSecurityAction::Enable),
        Err(CatalogError::Invalid(_))
    ));
    assert!(s.table_row_security("", "t").is_err());
    assert_eq!(
        s.table_row_security("a", "t").expect("a"),
        RowSecurityState::default()
    );
}

#[test]
fn corrupt_stored_blob_is_reported_without_fragments() {
    let (s, _g) = open("rs-corrupt");
    let wt = s.begin_write_txn().expect("w");
    {
        let mut t = wt.open_table(ROW_SECURITY_TABLE).expect("open");
        t.insert(("t", "secret-tenant"), [9u8, 9, 9].as_slice())
            .expect("ins");
    }
    crate::storage::commit_log::note_ddl(&wt, "t");
    bump_table_generation_in_txn(&wt, "t").expect("bump");
    crate::recovery::commit_boundary::commit(wt).expect("commit");
    let err = s
        .table_row_security("secret-tenant", "t")
        .expect_err("corrupt");
    assert!(matches!(err, CatalogError::CorruptSchema(_)));
    let msg = err.to_string();
    assert!(!msg.contains("secret-tenant"), "{msg}");
}

#[test]
fn drop_and_rename_follow_all_tenants() {
    let (s, _g) = open("rs-life");
    for tenant in ["a", "b", "c"] {
        s.alter_table_row_security(tenant, "t", RowSecurityAction::Enable)
            .expect("enable");
    }
    s.alter_table_rename("t", "t2").expect("rename");
    for tenant in ["a", "b", "c"] {
        assert!(s.table_row_security(tenant, "t2").expect("new").enabled);
        assert!(!s.table_row_security(tenant, "t").expect("old").enabled);
    }
    s.drop_table("t2").expect("drop");
    s.create_table(&schema("t2")).expect("recreate");
    for tenant in ["a", "b", "c"] {
        assert_eq!(
            s.table_row_security(tenant, "t2").expect("fresh"),
            RowSecurityState::default()
        );
    }
}

#[test]
fn state_survives_reopen() {
    let path = crate::test_util::temp_db::unique_db_path("rs-reopen");
    let _g = crate::test_util::temp_db::CleanupGuard(path.clone());
    {
        let s = Storage::open(&path).expect("open");
        s.create_table(&schema("t")).expect("create");
        s.alter_table_row_security("a", "t", RowSecurityAction::Enable)
            .expect("enable");
        s.alter_table_row_security("b", "t", RowSecurityAction::Force)
            .expect("force");
    }
    let s = Storage::open(&path).expect("reopen");
    assert_eq!(
        s.table_row_security("a", "t").expect("a"),
        RowSecurityState {
            enabled: true,
            forced: false
        }
    );
    assert_eq!(
        s.table_row_security("b", "t").expect("b"),
        RowSecurityState {
            enabled: false,
            forced: true
        }
    );
}
