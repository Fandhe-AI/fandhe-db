//! 明示トランザクションの最大持続時間・最大文数を起動時に設定した場合の境界テスト
//! （Issue #1553。ポインタ: SQL-46・SQL-31）。
//!
//! wire-server の `transaction_limits_opt` が `validate_*` を通して組み立てる
//! `TransactionLimits` を `EngineCore::with_transaction_limits` へ注入した状態を再現し、
//! 設定値を既定より下げた場合・上げた場合の双方で「上限ちょうどは成功・超過は `54000`」
//! であることを確認する（文数は加算前に判定するため、N 文目までは成功し N+1 文目で失敗）。
//! Issue #2409（SQL-31）: 持続時間・文数は writer の取得時点ではなく `BEGIN` から数える
//! （writer 取得前の読み取り文も文数に入る）ことを末尾の 4 テストで固定する。
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

use crate::temp_db;
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
const SELECT_SQL: &str = "SELECT id FROM documents ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10";

fn limits_with_duration_ms(ms: u64) -> TransactionLimits {
    TransactionLimits {
        max_duration: std::time::Duration::from_millis(ms),
        max_statements: 1_000,
    }
}

/// Issue #2409（SQL-31）: writer 取得前の読み取り文も文数に入る。最初の書き込みから数えるなら
/// 2 文目扱いで通ってしまうため、この形（読み取り 2 ＋ 書き込み 2）で起点の違いを区別できる。
#[test]
fn reads_before_first_write_count_toward_statement_limit() {
    let (engine, path) = new_core(limits_with_statements(3));
    let _cleanup = CleanupGuard(path);
    let caller = ctx();
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    for sql in ["BEGIN", SELECT_SQL, SELECT_SQL] {
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1))
        .expect("third statement is within the limit");
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(2))
        .expect_err("fourth statement beyond the limit");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

/// Issue #2409（SQL-31）: 持続時間は `BEGIN` から数える。writer 未取得のまま上限を越えたあとの
/// 最初の書き込み文は `54000`。
#[test]
fn duration_limit_counts_from_begin_not_from_first_write() {
    let (engine, path) = new_core(limits_with_duration_ms(200));
    let _cleanup = CleanupGuard(path);
    let caller = ctx();
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    for sql in ["BEGIN", SELECT_SQL] {
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1))
        .expect_err("first write after the begin-based deadline");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
}

/// Issue #2409（SQL-31）: writer の取得は持続時間の起点をリセットしない。
#[test]
fn writer_acquisition_does_not_reset_duration_clock() {
    let (engine, path) = new_core(limits_with_duration_ms(500));
    let _cleanup = CleanupGuard(path);
    let caller = ctx();
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    std::thread::sleep(std::time::Duration::from_millis(300));
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(1))
        .expect("first write acquires the writer within the limit");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect_err("600 ms since BEGIN exceeds 500 ms");
    assert_eq!(err.wire_code(), "54000");
    let _ = engine.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK");
    let mut other = SessionState::default();
    match engine
        .execute_sql_in_session(&caller, &mut other, "SELECT id FROM documents LIMIT 10")
        .expect("select")
    {
        engine::sql::SqlOutcome::Query(q) => assert!(q.rows.is_empty()),
        _ => panic!("unexpected outcome"),
    }
}

/// Issue #2409（SQL-31）: writer 未取得の `Active` でも期限は `BEGIN` 起点で効き、
/// `release_if_expired` が 1 度だけ解放して `Failed` にし、最初の `COMMIT` が `54000` を一度だけ返す。
#[test]
fn expiry_without_writer_releases_and_reports_once() {
    let (engine, path) = new_core(limits_with_duration_ms(200));
    let _cleanup = CleanupGuard(path);
    let caller = ctx();
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    for sql in ["BEGIN", SELECT_SQL] {
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let remaining = txn.remaining_duration().expect("active");
    assert!(
        remaining > std::time::Duration::ZERO && remaining <= std::time::Duration::from_millis(200),
        "remaining={remaining:?}"
    );
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert_eq!(txn.remaining_duration(), Some(std::time::Duration::ZERO));
    assert!(txn.release_if_expired());
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert!(!txn.release_if_expired());
    let first = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect_err("expired commit");
    assert_eq!(first.wire_code(), "54000");
    let second = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect_err("failed txn");
    assert_eq!(second.wire_code(), "25P02");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(txn.status(), TransactionStatus::Idle);
}
