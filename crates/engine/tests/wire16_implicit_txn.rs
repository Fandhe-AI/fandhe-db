//! 暗黙トランザクション（Issue #1175・WIRE-16・SQL-31・RECOVER-12）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/wire-protocol.md` WIRE-16・
//! `docs/spec/05-tasks.md` TASK-219。
//!
//! `EngineCore::begin_implicit_transaction`（`wire-server` がメッセージ先頭で呼ぶ入口）と
//! `SessionTransaction::commit_implicit`／`fail`／`abort_implicit` の状態遷移を、
//! engine API だけで検証する（wire 層のオーケストレーションは
//! `wire-server/tests/wire16_implicit_transaction.rs`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::{SearchMode, SessionState};
use engine::sql::transaction::TransactionStatus;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("wire16-implicit-txn");
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

fn count_rows(engine: &EngineCore, caller: &PolicyContext) -> usize {
    let outcome = engine
        .execute_sql_in_session(
            caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 100"),
        )
        .expect("select");
    match outcome {
        SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn implicit_insert_insert_commit_makes_both_rows_visible() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .begin_implicit_transaction(&mut txn, &session)
        .expect("begin implicit");
    assert!(txn.is_implicit_active());
    assert_eq!(txn.status(), TransactionStatus::InTransaction);

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-1"))
        .expect("insert 1");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(2, "op-2"))
        .expect("insert 2");
    txn.commit_implicit(&mut session).expect("commit implicit");

    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert!(!txn.is_implicit_active());
    assert_eq!(count_rows(&engine, &caller), 2);
}

#[test]
fn error_mid_message_rolls_back_rows_and_ledger() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .begin_implicit_transaction(&mut txn, &session)
        .expect("begin implicit");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-1"))
        .expect("insert 1");
    // 対応外の文（トランザクション内の `DROP TABLE`。Issue #1272 で UPDATE RETURNING は受理）で失敗する。
    // 暗黙トランザクションは Failed を経由せず Idle へ戻る。
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("DROP TABLE {TABLE}"),
        )
        .expect_err("DROP TABLE is not supported inside a transaction");
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert!(!txn.is_implicit_active());
    assert_eq!(count_rows(&engine, &caller), 0);

    // 台帳もロールバックされているため、同じ operation_id で再実行できる。
    engine
        .execute_sql_in_session(&caller, &mut session, &insert_sql(1, "op-1"))
        .expect("the operation_id ledger entry must have been rolled back");
    assert_eq!(count_rows(&engine, &caller), 1);
}

#[test]
fn truncate_and_insert_are_atomic_inside_an_implicit_transaction() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    engine
        .execute_sql_in_session(&caller, &mut session, &insert_sql(1, "op-seed"))
        .expect("seed");

    // TRUNCATE + INSERT を commit する。
    let mut txn = engine.new_session_transaction();
    engine
        .begin_implicit_transaction(&mut txn, &session)
        .expect("begin implicit");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("TRUNCATE TABLE {TABLE} USING OPERATION_ID 'op-t'"),
        )
        .expect("truncate");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(2, "op-2"))
        .expect("insert");
    txn.commit_implicit(&mut session).expect("commit");
    assert_eq!(count_rows(&engine, &caller), 1);

    // TRUNCATE の後で失敗すれば TRUNCATE も残らない。
    engine
        .begin_implicit_transaction(&mut txn, &session)
        .expect("begin implicit");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!("TRUNCATE TABLE {TABLE} USING OPERATION_ID 'op-t2'"),
        )
        .expect("truncate");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "SELECT nonsense FROM")
        .expect_err("syntax error");
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(count_rows(&engine, &caller), 1);
}

#[test]
fn reusing_operation_id_inside_an_implicit_transaction_is_25000_and_returns_to_idle() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .begin_implicit_transaction(&mut txn, &session)
        .expect("begin implicit");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-dup"))
        .expect("insert 1");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(2, "op-dup"))
        .expect_err("reused operation_id");
    assert_eq!(err.wire_code(), "25000");
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(count_rows(&engine, &caller), 0);
}

#[test]
fn begin_implicit_is_rejected_unless_idle_and_does_not_disturb_the_existing_state() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    // 明示トランザクション中は拒否（XX000）され、明示トランザクションは壊れない。
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = engine
        .begin_implicit_transaction(&mut txn, &session)
        .expect_err("begin_implicit from Active");
    assert_eq!(err.wire_code(), "XX000");
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    assert!(!txn.is_implicit_active());
    // 暗黙用の後始末は明示トランザクションに触れない。
    txn.abort_implicit();
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");

    // Failed からも拒否される。
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "SELECT nonsense FROM")
        .expect_err("syntax error fails the explicit txn");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    let err = engine
        .begin_implicit_transaction(&mut txn, &session)
        .expect_err("begin_implicit from Failed");
    assert_eq!(err.wire_code(), "XX000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

#[test]
fn commit_implicit_outside_an_implicit_transaction_is_an_internal_error() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    assert_eq!(
        txn.commit_implicit(&mut session)
            .expect_err("idle")
            .wire_code(),
        "XX000"
    );
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-1"))
        .expect("insert");
    assert_eq!(
        txn.commit_implicit(&mut session)
            .expect_err("explicit")
            .wire_code(),
        "XX000"
    );
    // 明示トランザクションは維持されたまま、通常の COMMIT で確定できる。
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(count_rows(&engine, &caller), 1);
}

#[test]
fn explicit_begin_on_an_implicit_transaction_aborts_it_defensively() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .begin_implicit_transaction(&mut txn, &session)
        .expect("begin implicit");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-1"))
        .expect("insert");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect_err("explicit COMMIT inside an implicit transaction");
    assert_eq!(err.wire_code(), "XX000");
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(count_rows(&engine, &caller), 0);
}

#[test]
fn abort_implicit_returns_to_idle_and_releases_the_writer() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .begin_implicit_transaction(&mut txn, &session)
        .expect("begin implicit");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-1"))
        .expect("insert");
    txn.abort_implicit();
    assert_eq!(txn.status(), TransactionStatus::Idle);
    // 冪等。
    txn.abort_implicit();
    // ライタが解放されているため、続けて autocommit の書き込みができる。
    engine
        .execute_sql_in_session(&caller, &mut session, &insert_sql(1, "op-1"))
        .expect("writer released and ledger rolled back");
    assert_eq!(count_rows(&engine, &caller), 1);
}

#[test]
fn commit_implicit_after_a_session_change_keeps_the_new_session_state() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();

    engine
        .begin_implicit_transaction(&mut txn, &session)
        .expect("begin implicit");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "SET search_mode = 'recall'",
        )
        .expect("set");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1, "op-1"))
        .expect("insert");
    txn.commit_implicit(&mut session).expect("commit");
    assert_eq!(session.search_mode(), Some(SearchMode::Recall));
}
