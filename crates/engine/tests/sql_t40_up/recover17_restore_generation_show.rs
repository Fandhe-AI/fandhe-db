//! 復元の世代番号の参照構文 `SHOW restore_generation` と変更の拒否（RECOVER-17・RECOVER-13・
//! Issue #2449 のポインタ）の結合テスト。値の型・テナント非依存・拒否・fail-closed を決定的に固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionLimits};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db::{unique_db_path, CleanupGuard};

fn ctx_of(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn core_at(path: &std::path::Path, advance: usize) -> EngineCore {
    let mut storage = Storage::open(path).expect("open storage");
    for _ in 0..advance {
        storage.advance_restore_generation().expect("advance");
    }
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn run(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx_of(tenant), session, sql)
}

fn show_as(core: &EngineCore, tenant: &str, session: &mut SessionState) -> i64 {
    match run(core, tenant, session, "SHOW restore_generation").expect("show") {
        SqlOutcome::Show(result) => {
            assert_eq!(result.columns.len(), 1);
            assert_eq!(result.rows.len(), 1);
            match &result.columns[0] {
                ColumnMeta::Computed { name, ty } => {
                    assert_eq!(name, "restore_generation");
                    assert_eq!(*ty, Some(engine::catalog::ColumnType::BigInt));
                }
                other => panic!("unexpected column {other:?}"),
            }
            match &result.rows[0].cells[..] {
                [Cell::SignedInteger(v)] => *v,
                other => panic!("expected one bigint, got {other:?}"),
            }
        }
        other => panic!("expected Show, got {other:?}"),
    }
}

#[test]
fn new_db_reads_zero_and_follows_advances_monotonically() {
    let path = unique_db_path("recover17-show");
    let _g = CleanupGuard(path.clone());
    let mut prev = {
        let core = core_at(&path, 0);
        let v = show_as(&core, "alice", &mut SessionState::default());
        assert_eq!(v, 0);
        v
    };
    for _ in 0..3 {
        let core = core_at(&path, 1);
        let v = show_as(&core, "alice", &mut SessionState::default());
        assert!(v > prev, "{v} must exceed {prev}");
        prev = v;
    }
}

#[test]
fn readable_in_explicit_and_read_only_transactions() {
    let path = unique_db_path("recover17-txn");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path, 1);
    let expected = show_as(&core, "alice", &mut SessionState::default());
    let c = ctx_of("alice");
    for begin in ["BEGIN", "BEGIN READ ONLY"] {
        let mut s = SessionState::default();
        let mut txn = SessionTransaction::new(TransactionLimits::default());
        core.execute_sql_in_txn(&c, &mut s, &mut txn, begin)
            .expect("begin");
        match core
            .execute_sql_in_txn(&c, &mut s, &mut txn, "SHOW restore_generation")
            .expect("show in txn")
        {
            SqlOutcome::Show(r) => {
                assert!(matches!(&r.rows[0].cells[..], [Cell::SignedInteger(v)] if *v == expected))
            }
            other => panic!("expected Show, got {other:?}"),
        }
        core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
}

#[test]
fn every_mutation_path_is_rejected_and_value_is_unchanged() {
    let path = unique_db_path("recover17-reject");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path, 1);
    let mut s = SessionState::default();
    let before = show_as(&core, "alice", &mut s);
    for sql in [
        "SET restore_generation = 5",
        "SET restore_generation TO 5",
        "SET SESSION restore_generation = 5",
        "SET LOCAL restore_generation = 5",
        "SET restore_generation TO DEFAULT",
        "RESET restore_generation",
        "set Restore_Generation = 1;",
    ] {
        let e = run(&core, "alice", &mut s, sql).expect_err(sql);
        assert_eq!(e.wire_code(), "42501", "{sql}");
        let msg = e.to_string();
        assert!(!msg.contains("alice") && !msg.contains('5'), "{msg}");
        assert_eq!(show_as(&core, "alice", &mut s), before, "{sql}");
    }
    let e = run(
        &core,
        "alice",
        &mut s,
        "ALTER ROLE r SET restore_generation = 1",
    )
    .expect_err("alter role set");
    assert_ne!(e.wire_code(), "00000");
    assert_eq!(show_as(&core, "alice", &mut s), before);
}

#[test]
fn value_is_identical_across_tenants() {
    let path = unique_db_path("recover17-tenants");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path, 2);
    let a = show_as(&core, "alice", &mut SessionState::default());
    let b = show_as(&core, "bob", &mut SessionState::default());
    assert_eq!(a, b);
    assert_eq!(show_as(&core, "alice", &mut SessionState::default()), a);
}

#[test]
fn commit_numbers_are_not_exposed() {
    let path = unique_db_path("recover17-nocommit");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path, 1);
    let mut s = SessionState::default();
    for sql in [
        "SHOW commit_number",
        "SHOW storage_generation",
        "SHOW restore_generation extra",
        "SELECT txid_current()",
    ] {
        assert!(run(&core, "alice", &mut s, sql).is_err(), "{sql}");
    }
}

#[test]
fn session_less_entry_rejects_fixed_message() {
    let path = unique_db_path("recover17-sessionless");
    let _g = CleanupGuard(path.clone());
    let core = core_at(&path, 1);
    let e = core
        .execute_sql(&ctx_of("alice"), "SHOW restore_generation")
        .expect_err("session-less entry");
    assert_eq!(e.wire_code(), "42601");
}

#[test]
fn corrupt_generation_table_fails_closed() {
    let path = unique_db_path("recover17-corrupt");
    let _g = CleanupGuard(path.clone());
    drop(core_at(&path, 1));
    {
        let db = redb::Database::open(&path).expect("open raw");
        let w = db.begin_write().expect("begin");
        {
            let def: redb::TableDefinition<&str, &[u8]> =
                redb::TableDefinition::new("restore_generation");
            // 許可: 破損した世代表を作る試験専用の直接書き込み（変更ログの対象外の表）。
            #[allow(clippy::disallowed_methods)]
            let mut t = w.open_table(def).expect("table");
            t.insert("generation", [1u8, 2, 3].as_slice()).expect("put");
        }
        w.commit().expect("commit");
    }
    let core = core_at(&path, 0);
    let e = run(
        &core,
        "alice",
        &mut SessionState::default(),
        "SHOW restore_generation",
    )
    .expect_err("corrupt value must not be returned");
    assert_eq!(e.wire_code(), "XX000");
}
