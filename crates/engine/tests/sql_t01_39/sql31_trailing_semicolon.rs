//! 末尾 `;` 付きの Tx 制御文・`SET TRANSACTION`・カーソル文の結合テスト（Issue #3171・SQL-31・
//! SQL-38・SQL-69 のポインタ）。psql の `-f`／`-c` は文末に `;` を付けて 1 文ずつ送るため、
//! `EngineCore::execute_sql_in_txn`（production 経路）が `;` 付きの入力を受理することを固定する。
//! 末尾 `;` の吸収は各検証器の入口（`sql::lexer::strip_one_trailing_semicolon`）が担う。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql31-trailing-semicolon");
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

fn insert_sql(id: u64) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding) VALUES ({id}, '[1.0, 0.0]') USING OPERATION_ID 'op-{id}';"
    )
}

fn select_sql() -> String {
    format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10")
}

/// `SessionTransaction` は `EngineCore` を借用するため、エンジンは呼び出し側が所有する。
struct Harness<'e> {
    engine: &'e EngineCore,
    caller: PolicyContext,
    session: SessionState,
    txn: SessionTransaction<'e>,
}

impl<'e> Harness<'e> {
    fn new(engine: &'e EngineCore) -> Self {
        Self {
            engine,
            caller: ctx("tenant-a"),
            session: SessionState::default(),
            txn: engine.new_session_transaction(),
        }
    }

    fn run(&mut self, sql: &str) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
        self.engine
            .execute_sql_in_txn(&self.caller, &mut self.session, &mut self.txn, sql)
    }

    fn ok(&mut self, sql: &str) -> SqlOutcome {
        self.run(sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    }

    fn row_count(&self, caller: &PolicyContext) -> usize {
        let outcome = self
            .engine
            .execute_sql_in_session(caller, &mut SessionState::default(), &select_sql())
            .expect("select");
        match outcome {
            SqlOutcome::Query(r) => r.rows.len(),
            other => panic!("expected Query, got {other:?}"),
        }
    }
}

#[test]
fn begin_insert_rollback_with_semicolons_discards_the_row() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    assert_eq!(h.ok("BEGIN;"), SqlOutcome::Begin);
    assert_eq!(h.txn.status(), TransactionStatus::InTransaction);
    h.ok(&insert_sql(1));
    assert_eq!(h.ok("ROLLBACK;"), SqlOutcome::Rollback);
    assert_eq!(h.txn.status(), TransactionStatus::Idle);
    assert_eq!(h.row_count(&ctx("tenant-a")), 0);
}

#[test]
fn begin_insert_commit_with_semicolons_keeps_the_row() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.ok("START TRANSACTION;");
    h.ok(&insert_sql(1));
    assert_eq!(h.ok("END;"), SqlOutcome::Commit);
    assert_eq!(h.row_count(&ctx("tenant-a")), 1);
    h.ok("BEGIN;");
    h.ok(&insert_sql(2));
    assert_eq!(h.ok("COMMIT;"), SqlOutcome::Commit);
    assert_eq!(h.row_count(&ctx("tenant-a")), 2);
}

#[test]
fn savepoint_statements_with_semicolons_roll_back_only_later_rows() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.ok("BEGIN;");
    h.ok(&insert_sql(1));
    h.ok("SAVEPOINT s1;");
    h.ok(&insert_sql(2));
    h.ok("ROLLBACK TO s1;");
    h.ok("RELEASE s1;");
    h.ok("COMMIT;");
    assert_eq!(h.row_count(&ctx("tenant-a")), 1);
}

#[test]
fn set_transaction_read_only_with_semicolon_rejects_writes() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.ok("BEGIN;");
    h.ok("SET TRANSACTION READ ONLY;");
    let err = h.run(&insert_sql(1)).expect_err("read only write");
    assert_eq!(err.wire_code(), "25006");
    assert_eq!(h.txn.status(), TransactionStatus::Failed);
}

#[test]
fn rollback_with_semicolon_recovers_from_failed_state() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.ok("BEGIN;");
    assert!(h.run("SELEC id FROM documents;").is_err());
    assert_eq!(h.txn.status(), TransactionStatus::Failed);
    h.ok("ROLLBACK;");
    assert_eq!(h.txn.status(), TransactionStatus::Idle);
}

#[test]
fn cursor_statements_with_semicolons_work_and_respect_tenant_boundary() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.ok("BEGIN;");
    h.ok(&insert_sql(1));
    h.ok("COMMIT;");
    h.ok("BEGIN;");
    assert_eq!(
        h.ok(&format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE};")),
        SqlOutcome::DeclareCursor
    );
    match h.ok("FETCH 1 FROM c;") {
        SqlOutcome::Fetch(r) => assert_eq!(r.rows.len(), 1),
        other => panic!("expected Fetch, got {other:?}"),
    }
    h.ok("CLOSE c;");
    h.ok("COMMIT;");

    // 別テナントからは行が見えない（テナント境界は末尾 `;` の有無で変わらない）。
    h.caller = ctx("tenant-b");
    h.ok("BEGIN;");
    h.ok(&format!("DECLARE c CURSOR FOR SELECT id FROM {TABLE};"));
    match h.ok("FETCH 1 FROM c;") {
        SqlOutcome::Fetch(r) => assert_eq!(r.rows.len(), 0),
        o => panic!("expected Fetch, got {o:?}"),
    }
}

/// Issue #3175: 内側が JOIN の `DECLARE` も末尾 `;` を 1 個だけ受理する（`;;` は拒否）。
#[test]
fn declare_join_accepts_one_trailing_semicolon() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.ok("BEGIN;");
    h.ok(&insert_sql(1));
    h.ok("COMMIT;");
    h.ok("BEGIN;");
    let join = format!("SELECT a.id FROM {TABLE} a JOIN {TABLE} b ON a.id = b.id LIMIT 10");
    assert_eq!(
        h.ok(&format!("DECLARE c CURSOR FOR {join};")),
        SqlOutcome::DeclareCursor
    );
    match h.ok("FETCH NEXT IN c;") {
        SqlOutcome::Fetch(r) => assert_eq!(r.rows.len(), 1),
        other => panic!("expected Fetch, got {other:?}"),
    }
    let err = h
        .run(&format!("DECLARE d CURSOR FOR {join};;"))
        .expect_err("double semicolon rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn double_trailing_semicolon_is_rejected_with_42601() {
    let (engine, path) = new_core();
    let _cleanup = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    let err = h.run("BEGIN;;").expect_err("rejected");
    assert_eq!(err.wire_code(), "42601");
    assert_eq!(h.txn.status(), TransactionStatus::Idle);
    h.ok("BEGIN;");
    let err = h.run("SAVEPOINT s1;;").expect_err("rejected");
    assert_eq!(err.wire_code(), "42601");
}
