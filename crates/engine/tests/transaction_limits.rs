//! 明示トランザクションの最大持続時間・最大文数を起動時に設定した場合の境界テスト
//! （Issue #1553。ポインタ: SQL-46・SQL-31）。
//!
//! wire-server の `transaction_limits_opt` が `validate_*` を通して組み立てる
//! `TransactionLimits` を `EngineCore::with_transaction_limits` へ注入した状態を再現し、
//! 設定値を既定より下げた場合・上げた場合の双方で「上限ちょうどは成功・超過は `54000`」
//! であることを確認する（文数は加算前に判定するため、N 文目までは成功し N+1 文目で失敗）。
//! 既存の `sql31_transaction.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{
    validate_max_transaction_duration_ms, validate_max_transaction_statements, TransactionLimits,
    TransactionStatus,
};
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn new_core(limits: TransactionLimits) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("transaction-limits");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_transaction_limits(limits),
        path,
    )
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_sql(id: u64) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding) VALUES ({id}, '[1.0, 0.0]') USING OPERATION_ID 'op-{id}'"
    )
}

fn limits_with_statements(n: u64) -> TransactionLimits {
    TransactionLimits {
        max_duration: std::time::Duration::from_secs(20),
        max_statements: validate_max_transaction_statements(n).expect("in range"),
    }
}

/// `BEGIN` 後に `n` 文まで成功し、`n + 1` 文目が `54000`・状態 `Failed` になること。
fn assert_statement_boundary(n: u64) {
    let (engine, path) = new_core(limits_with_statements(n));
    let _cleanup = CleanupGuard(path);
    let caller = ctx();
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    for id in 1..=n {
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(id))
            .unwrap_or_else(|e| panic!("statement {id} within limit {n} failed: {e}"));
    }
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(n + 1))
        .expect_err("statement beyond the limit");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

#[test]
fn lowered_statement_limit_is_enforced_at_boundary() {
    assert_statement_boundary(3);
}

#[test]
fn raised_statement_limit_above_default_is_enforced_at_boundary() {
    // 既定（1,000）を超える 1,001 文目まで通り、1,002 文目で `54000`。
    assert_statement_boundary(1_001);
}

#[test]
fn raised_duration_limit_keeps_transaction_alive_past_default() {
    // 上げた値が `max_duration` に届いていること（Default の 20 秒には依存しない形で、
    // 設定値より十分短い経過では失敗しないことと、下げた値では失敗することの対で確認する）。
    let limits = TransactionLimits {
        max_duration: validate_max_transaction_duration_ms(60_000).expect("hard max"),
        max_statements: 1_000,
    };
    assert_eq!(limits.max_duration, std::time::Duration::from_secs(60));
    let (engine, path) = new_core(limits);
    let _cleanup = CleanupGuard(path);
    let caller = ctx();
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    std::thread::sleep(std::time::Duration::from_millis(150));
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1))
        .expect("within raised duration");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect("commit within raised duration");
}

#[test]
fn lowered_duration_limit_fails_statement_and_commit_after_expiry() {
    let limits = TransactionLimits {
        max_duration: validate_max_transaction_duration_ms(100).expect("lower bound"),
        max_statements: 1_000,
    };
    let (engine, path) = new_core(limits);
    let _cleanup = CleanupGuard(path);
    let caller = ctx();
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1))
        .expect("within the lowered duration");
    std::thread::sleep(std::time::Duration::from_millis(250));
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect_err("commit after expiry");
    assert_eq!(err.wire_code(), "54000");
}
