//! 明示トランザクションの writer 遅延取得（Issue #2400。親 #2396・TASK-333）の結合テスト。
//! ポインタ: PERSIST-4・SQL-31・TABLE-3（設計記録は `docs/design/explicit-transaction.md`）。
//!
//! 同一スレッドで順に実行する。writer を握っていれば自己デッドロック防御で即座に `55P03` に
//! なるため、時間計測に依存せず決定的に回帰を検出できる。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::TransactionStatus;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(tag: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_write_lock_wait(std::time::Duration::from_millis(100));
    (core, guard)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert(id: u64, op: &str) -> String {
    format!(
        "INSERT INTO documents (id, embedding) VALUES ({id}, '[1.0, 0.0]') \
         USING OPERATION_ID '{op}'"
    )
}

/// AC1: 読み取りだけの明示 Tx の間も、別セッションの書き込みは待たされず成功する。
#[test]
fn read_only_explicit_txn_does_not_block_other_session_writes() {
    let (core, _g) = new_core("sql31-deferred-ac1");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in ["BEGIN", "SELECT id FROM documents LIMIT 10"] {
        core.execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
            .expect(sql);
    }
    let mut other = SessionState::default();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut other, &insert(1, "other-1"))
        .expect("other session write is not blocked");
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(txn.status(), TransactionStatus::Idle);
    let out = core
        .execute_sql_in_session(&caller, &mut other, "SELECT id FROM documents LIMIT 10")
        .expect("select");
    match out {
        engine::sql::SqlOutcome::Query(q) => assert_eq!(q.rows.len(), 1),
        other => panic!("unexpected outcome {other:?}"),
    }
}

/// 最初の書き込みの後は他セッションの書き込みが `55P03` になり、`ROLLBACK` 後は成功する。
#[test]
fn writes_after_first_write_are_serialized_until_rollback() {
    let (core, _g) = new_core("sql31-deferred-serialize");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, &insert(1, "mine"))
        .expect("first write");
    let mut other = SessionState::default();
    let err = core
        .execute_sql_in_session(&ctx("tenant-b"), &mut other, &insert(2, "other"))
        .expect_err("writer is held");
    assert_eq!(err.wire_code(), "55P03");
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    core.execute_sql_in_session(&ctx("tenant-b"), &mut other, &insert(2, "other"))
        .expect("write succeeds after rollback");
}

/// DDL は `0A000` で拒否され、その間も別セッションの書き込みは成功する（writer を取らない）。
#[test]
fn rejected_ddl_does_not_hold_the_writer() {
    let (core, _g) = new_core("sql31-deferred-ddl");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "CREATE TABLE t9 (id BIGINT PRIMARY KEY)",
        )
        .expect_err("DDL is rejected");
    assert_eq!(err.wire_code(), "0A000");
    let mut other = SessionState::default();
    core.execute_sql_in_session(&ctx("tenant-b"), &mut other, &insert(1, "other"))
        .expect("other write is not blocked");
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
}
