//! SQL-38 (1)・Issue #1938: `BEGIN ISOLATION LEVEL <レベル>` の受理を engine の公開入口
//! （`EngineCore::execute_sql_in_txn`）から固定する結合テスト。4 レベルすべてを受理し、
//! 要求レベルは記録されるだけで実行（単一ライタの commit／rollback・状態遷移）は素の
//! `BEGIN` と同一であることを確かめる。単体の構文網羅は `sql::transaction` の単体テスト側。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{RequestedIsolationLevel, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql38-begin-isolation");
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

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn visible_rows(engine: &EngineCore, caller: &PolicyContext) -> usize {
    let sql = format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10");
    match engine
        .execute_sql_in_session(caller, &mut SessionState::default(), &sql)
        .expect("select")
    {
        SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

const LEVELS: [(&str, RequestedIsolationLevel); 4] = [
    ("SERIALIZABLE", RequestedIsolationLevel::Serializable),
    ("REPEATABLE READ", RequestedIsolationLevel::RepeatableRead),
    ("READ COMMITTED", RequestedIsolationLevel::ReadCommitted),
    ("READ UNCOMMITTED", RequestedIsolationLevel::ReadUncommitted),
];

/// 4 レベルとも `BEGIN` → INSERT → `COMMIT` が通り、行が確定する。要求レベルは
/// `Active` の間だけ読め、終了後は `None`。
#[test]
fn every_level_begins_commits_and_records_the_requested_level() {
    for (i, (text, level)) in LEVELS.into_iter().enumerate() {
        let (engine, path) = new_core();
        let _cleanup = CleanupGuard(path);
        let caller = ctx();
        let mut session = SessionState::default();
        let mut txn = engine.new_session_transaction();

        let begin = format!("BEGIN ISOLATION LEVEL {text}");
        assert_eq!(
            engine
                .execute_sql_in_txn(&caller, &mut session, &mut txn, &begin)
                .expect("begin"),
            SqlOutcome::Begin
        );
        assert_eq!(txn.status(), TransactionStatus::InTransaction);
        assert_eq!(txn.isolation_level(), Some(level));

        let insert = format!(
            "INSERT INTO {TABLE} (id, embedding) VALUES (1, '[1.0, 0.0]') USING OPERATION_ID 'op-{i}'"
        );
        engine
            .execute_sql_in_txn(&caller, &mut session, &mut txn, &insert)
            .expect("insert");
        assert_eq!(
            engine
                .execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
                .expect("commit"),
            SqlOutcome::Commit
        );
        assert_eq!(txn.status(), TransactionStatus::Idle);
        assert_eq!(txn.isolation_level(), None);
        assert_eq!(visible_rows(&engine, &caller), 1, "{text}");
    }
}

/// `ROLLBACK` は分離レベル指定付きの `BEGIN` でも書き込みを残さない。
#[test]
fn rollback_after_isolation_level_begin_discards_writes() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx();
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "BEGIN ISOLATION LEVEL READ COMMITTED",
        )
        .expect("begin");
    engine
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &format!(
                "INSERT INTO {TABLE} (id, embedding) VALUES (1, '[1.0, 0.0]') USING OPERATION_ID 'op-rb'"
            ),
        )
        .expect("insert");
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(visible_rows(&engine, &caller), 0);
}

/// 未知のレベルは `42601` で拒否され、writer を取らず `Idle` のまま。素の `BEGIN` は
/// レベルを持たない。
#[test]
fn unknown_level_is_rejected_without_starting_and_plain_begin_has_no_level() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let caller = ctx();
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    let err = engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN ISOLATION LEVEL FOO")
        .expect_err("rejected");
    assert_eq!(err.wire_code(), "42601");
    assert_eq!(txn.status(), TransactionStatus::Idle);

    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    assert_eq!(txn.isolation_level(), None);
}
