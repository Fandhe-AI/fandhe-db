//! トランザクション制御の別名 `START TRANSACTION`／`END`／`ABORT`（Issue #1930）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-69 (1)・SQL-31。
//!
//! `EngineCore::execute_sql_in_txn` を production 経路として、別名が `BEGIN`／`COMMIT`／
//! `ROLLBACK` と同じ状態遷移・上限・エラーに載ることと、未対応形（`AND CHAIN`・
//! `PREPARE TRANSACTION`）が `0A000` で fail-closed することを固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionLimits, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql69-txn-aliases");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_sql(id: u64, op_id: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding) VALUES ({id}, '[1.0, 0.0]') USING OPERATION_ID '{op_id}'"
    )
}

fn visible_rows(engine: &EngineCore, caller: &PolicyContext) -> usize {
    let outcome = engine
        .execute_sql_in_session(
            caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"),
        )
        .expect("select");
    match outcome {
        SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn run<'e>(
    engine: &'e EngineCore,
    caller: &PolicyContext,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    engine.execute_sql_in_txn(caller, session, txn, sql)
}

#[test]
fn start_transaction_end_commits_both_rows() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = engine.new_session_transaction();

    assert_eq!(
        run(&engine, &caller, &mut s, &mut txn, "START TRANSACTION").unwrap(),
        SqlOutcome::StartTransaction
    );
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    run(&engine, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(&engine, &caller, &mut s, &mut txn, &insert_sql(2, "op-2")).unwrap();
    assert_eq!(
        run(&engine, &caller, &mut s, &mut txn, "END").unwrap(),
        SqlOutcome::Commit
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(visible_rows(&engine, &caller), 2);
    // 別テナントからは見えない（テナント境界は別名でも不変）。
    assert_eq!(visible_rows(&engine, &ctx("tenant-b")), 0);
}

#[test]
fn start_transaction_abort_discards_rows() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = engine.new_session_transaction();

    run(&engine, &caller, &mut s, &mut txn, "START TRANSACTION").unwrap();
    run(&engine, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    assert_eq!(
        run(&engine, &caller, &mut s, &mut txn, "ABORT WORK").unwrap(),
        SqlOutcome::Rollback
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(visible_rows(&engine, &caller), 0);
}

#[test]
fn nested_begin_aliases_fail_with_25001() {
    for (first, second) in [
        ("BEGIN", "START TRANSACTION"),
        ("START TRANSACTION", "BEGIN"),
    ] {
        let (engine, path) = new_core();
        let _c = CleanupGuard(path);
        let caller = ctx("tenant-a");
        let mut s = SessionState::default();
        let mut txn = engine.new_session_transaction();
        run(&engine, &caller, &mut s, &mut txn, first).unwrap();
        let err = run(&engine, &caller, &mut s, &mut txn, second).unwrap_err();
        assert_eq!(err.wire_code(), "25001");
        assert_eq!(txn.status(), TransactionStatus::Failed);
    }
}

#[test]
fn end_and_abort_without_transaction_are_25p01() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = engine.new_session_transaction();
    for sql in ["END", "ABORT"] {
        let err = run(&engine, &caller, &mut s, &mut txn, sql).unwrap_err();
        assert_eq!(err.wire_code(), "25P01", "{sql}");
    }
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

#[test]
fn failed_state_rejects_end_and_commit_but_abort_recovers() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = engine.new_session_transaction();
    run(&engine, &caller, &mut s, &mut txn, "START TRANSACTION").unwrap();
    run(&engine, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(&engine, &caller, &mut s, &mut txn, "BOGUS !!").unwrap_err();
    assert_eq!(txn.status(), TransactionStatus::Failed);
    for sql in ["END", "END WORK", "COMMIT"] {
        let err = run(&engine, &caller, &mut s, &mut txn, sql).unwrap_err();
        assert_eq!(err.wire_code(), "25P02", "{sql}");
        assert_eq!(txn.status(), TransactionStatus::Failed);
    }
    assert_eq!(
        run(&engine, &caller, &mut s, &mut txn, "ABORT").unwrap(),
        SqlOutcome::Rollback
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(visible_rows(&engine, &caller), 0);
}

#[test]
fn statement_limit_applies_via_start_transaction() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let engine = engine.with_transaction_limits(TransactionLimits {
        max_duration: std::time::Duration::from_secs(20),
        max_statements: 1,
    });
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = engine.new_session_transaction();
    run(&engine, &caller, &mut s, &mut txn, "START TRANSACTION").unwrap();
    run(&engine, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    let err = run(&engine, &caller, &mut s, &mut txn, &insert_sql(2, "op-2")).unwrap_err();
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    run(&engine, &caller, &mut s, &mut txn, "ABORT").unwrap();
}

#[test]
fn chain_and_prepare_transaction_are_rejected_with_0a000() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = engine.new_session_transaction();

    // Idle の PREPARE TRANSACTION は拒否されるが状態は不変。
    let err = run(
        &engine,
        &caller,
        &mut s,
        &mut txn,
        "PREPARE TRANSACTION 'x'",
    )
    .unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Idle);

    for sql in [
        "COMMIT AND CHAIN",
        "END AND CHAIN",
        "PREPARE TRANSACTION 'x'",
    ] {
        run(&engine, &caller, &mut s, &mut txn, "START TRANSACTION").unwrap();
        let err = run(&engine, &caller, &mut s, &mut txn, sql).unwrap_err();
        assert_eq!(err.wire_code(), "0A000", "{sql}");
        assert_eq!(txn.status(), TransactionStatus::Failed);
        let err = run(&engine, &caller, &mut s, &mut txn, "END").unwrap_err();
        assert_eq!(err.wire_code(), "25P02");
        run(&engine, &caller, &mut s, &mut txn, "ABORT").unwrap();
        assert_eq!(txn.status(), TransactionStatus::Idle);
    }
}

#[test]
fn session_entry_without_txn_context_rejects_start_transaction_like_begin() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let a = engine
        .execute_sql_in_session(&caller, &mut s, "BEGIN")
        .unwrap_err();
    let b = engine
        .execute_sql_in_session(&caller, &mut s, "START TRANSACTION")
        .unwrap_err();
    assert_eq!(a.wire_code(), b.wire_code());
}
