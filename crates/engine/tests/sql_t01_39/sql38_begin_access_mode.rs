//! `BEGIN READ ONLY`／`BEGIN READ WRITE` のアクセスモード修飾子（SQL-38・Issue #1932）の
//! 結合テスト。ポインタ: `docs/spec/04-behavior/` SQL-38・SQL-31。
//!
//! `EngineCore::execute_sql_in_txn` を production 経路として検証する。`BEGIN READ ONLY` は
//! Issue #1934 で開始できるようになった（書き込み文の `25006` 拒否は
//! `sql38_read_only_write_rejection` が検証する）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus, TxnAccessMode};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql38-begin-access-mode");
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

fn run<'a>(
    engine: &'a EngineCore,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> Result<SqlOutcome, String> {
    engine
        .execute_sql_in_txn(&ctx(), session, txn, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn insert_sql(id: u64, op: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding) VALUES ({id}, '[1.0, 0.0]') USING OPERATION_ID '{op}'"
    )
}

fn count_rows(engine: &EngineCore) -> usize {
    match engine
        .execute_sql_in_session(
            &ctx(),
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"),
        )
        .expect("select")
    {
        SqlOutcome::Query(r) => r.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

#[test]
fn begin_read_write_and_plain_begin_behave_the_same() {
    for begin in ["BEGIN READ WRITE", "BEGIN"] {
        let (engine, path) = new_core();
        let _cleanup = CleanupGuard(path);
        let mut session = SessionState::default();
        let mut txn = engine.new_session_transaction();
        assert_eq!(
            run(&engine, &mut session, &mut txn, begin),
            Ok(SqlOutcome::Begin)
        );
        assert_eq!(txn.status(), TransactionStatus::InTransaction);
        assert_eq!(txn.access_mode(), Some(TxnAccessMode::ReadWrite));
        run(&engine, &mut session, &mut txn, &insert_sql(1, "op-1")).expect("insert 1");
        run(&engine, &mut session, &mut txn, &insert_sql(2, "op-2")).expect("insert 2");
        assert_eq!(
            run(&engine, &mut session, &mut txn, "COMMIT"),
            Ok(SqlOutcome::Commit)
        );
        assert_eq!(txn.access_mode(), None);
        assert_eq!(count_rows(&engine), 2, "{begin}");
    }
}

#[test]
fn begin_read_only_starts_and_rejects_a_write_with_25006() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    assert_eq!(
        run(&engine, &mut session, &mut txn, "BEGIN READ ONLY"),
        Ok(SqlOutcome::Begin)
    );
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    assert_eq!(txn.access_mode(), Some(TxnAccessMode::ReadOnly));
    assert_eq!(
        run(&engine, &mut session, &mut txn, &insert_sql(1, "op-ro")),
        Err("25006".to_string())
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert_eq!(
        run(&engine, &mut session, &mut txn, "ROLLBACK"),
        Ok(SqlOutcome::Rollback)
    );
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(count_rows(&engine), 0);
    // 続く BEGIN（READ WRITE）が成功し、書き込めること。
    assert_eq!(
        run(&engine, &mut session, &mut txn, "BEGIN"),
        Ok(SqlOutcome::Begin)
    );
    run(&engine, &mut session, &mut txn, &insert_sql(1, "op-ro")).expect("insert");
}

#[test]
fn begin_read_only_inside_a_transaction_fails_it_like_a_nested_begin() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    run(&engine, &mut session, &mut txn, "BEGIN").expect("begin");
    assert_eq!(
        run(&engine, &mut session, &mut txn, "BEGIN READ ONLY"),
        Err("25001".to_string())
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert_eq!(
        run(&engine, &mut session, &mut txn, "BEGIN READ ONLY"),
        Err("25P02".to_string())
    );
}

#[test]
fn malformed_access_mode_is_a_syntax_error() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    for sql in ["BEGIN READ", "BEGIN READ ONLY READ WRITE", "BEGIN WRITE"] {
        assert_eq!(
            run(&engine, &mut session, &mut txn, sql),
            Err("42601".to_string()),
            "{sql}"
        );
        assert_eq!(txn.status(), TransactionStatus::Idle);
    }
}
