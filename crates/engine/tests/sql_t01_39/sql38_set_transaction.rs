//! SQL-38 (7)(a)・Issue #1939: `SET TRANSACTION` と `SET SESSION CHARACTERISTICS AS TRANSACTION`
//! の受理を engine の公開入口（`EngineCore::execute_sql_in_txn`）から固定する結合テスト。
//! 最初の問い合わせの前だけ受理する契約・セッション既定の継承・fail-closed の拒否を確かめる。
//! 構文の網羅は `sql::transaction` の単体テスト側。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{
    RequestedIsolationLevel, SessionTransaction, TransactionLimits, TransactionStatus,
    TxnAccessMode,
};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql38-set-transaction");
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
            caller: ctx(),
            session: SessionState::default(),
            txn: engine.new_session_transaction(),
        }
    }

    fn run(&mut self, sql: &str) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
        self.engine
            .execute_sql_in_txn(&self.caller, &mut self.session, &mut self.txn, sql)
    }

    fn ok(&mut self, sql: &str) -> SqlOutcome {
        self.run(sql)
            .unwrap_or_else(|e| panic!("{sql}: {} {e}", e.wire_code()))
    }

    fn code(&mut self, sql: &str) -> String {
        match self.run(sql) {
            Ok(o) => panic!("{sql}: expected error, got {o:?}"),
            Err(e) => e.wire_code().to_string(),
        }
    }
}

fn insert(id: u64, op: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding) VALUES ({id}, '[1.0, 0.0]') USING OPERATION_ID '{op}'"
    )
}

#[test]
fn every_isolation_level_is_accepted_and_recorded() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    for (text, level) in [
        ("SERIALIZABLE", RequestedIsolationLevel::Serializable),
        ("REPEATABLE READ", RequestedIsolationLevel::RepeatableRead),
        ("READ COMMITTED", RequestedIsolationLevel::ReadCommitted),
        ("READ UNCOMMITTED", RequestedIsolationLevel::ReadUncommitted),
    ] {
        let mut h = Harness::new(&engine);
        h.ok("BEGIN");
        assert_eq!(
            h.ok(&format!("SET TRANSACTION ISOLATION LEVEL {text}")),
            SqlOutcome::SetTransaction
        );
        assert_eq!(h.txn.isolation_level(), Some(level), "{text}");
        h.ok("ROLLBACK");
    }
}

#[test]
fn set_transaction_read_only_rejects_writes() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.ok("BEGIN");
    h.ok("SET TRANSACTION READ ONLY");
    assert_eq!(h.txn.access_mode(), Some(TxnAccessMode::ReadOnly));
    assert_eq!(h.code(&insert(1, "op-ro")), "25006");
    assert_eq!(h.txn.status(), TransactionStatus::Failed);
    h.ok("ROLLBACK");
    assert_eq!(visible_rows(&engine, &h.caller), 0);
}

#[test]
fn read_only_begin_can_switch_to_read_write_before_first_query() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.ok("BEGIN READ ONLY");
    h.ok("SET TRANSACTION READ WRITE");
    assert_eq!(h.txn.access_mode(), Some(TxnAccessMode::ReadWrite));
    h.ok(&insert(1, "op-rw"));
    h.ok("COMMIT");
    assert_eq!(visible_rows(&engine, &h.caller), 1);
}

#[test]
fn consecutive_set_transactions_are_accepted_and_merge() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.ok("BEGIN");
    h.ok("SET TRANSACTION READ ONLY");
    h.ok("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(h.txn.access_mode(), Some(TxnAccessMode::ReadOnly));
    assert_eq!(
        h.txn.isolation_level(),
        Some(RequestedIsolationLevel::Serializable)
    );
    h.ok("SET TRANSACTION READ WRITE, ISOLATION LEVEL READ COMMITTED");
    assert_eq!(h.txn.access_mode(), Some(TxnAccessMode::ReadWrite));
    assert_eq!(
        h.txn.isolation_level(),
        Some(RequestedIsolationLevel::ReadCommitted)
    );
    h.ok("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY");
    assert_eq!(h.txn.access_mode(), Some(TxnAccessMode::ReadOnly));
}

#[test]
fn set_transaction_after_a_query_is_25001_and_fails_the_transaction() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.ok("BEGIN");
    h.ok(&format!(
        "SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 1"
    ));
    assert_eq!(h.code("SET TRANSACTION READ ONLY"), "25001");
    assert_eq!(h.txn.status(), TransactionStatus::Failed);
    assert_eq!(h.code("COMMIT"), "25P02");
    h.ok("ROLLBACK");
    assert_eq!(h.txn.status(), TransactionStatus::Idle);
}

#[test]
fn other_set_statement_counts_as_a_query() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.ok("BEGIN");
    h.ok("SET TIME ZONE 'UTC'");
    assert_eq!(h.code("SET TRANSACTION READ ONLY"), "25001");
}

#[test]
fn outside_a_transaction_is_25p01_and_changes_nothing() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    assert_eq!(h.code("SET TRANSACTION READ ONLY"), "25P01");
    assert_eq!(h.txn.status(), TransactionStatus::Idle);
    h.ok("BEGIN");
    assert_eq!(h.txn.access_mode(), Some(TxnAccessMode::ReadWrite));
}

#[test]
fn failed_transaction_rejects_both_statements_with_25p02() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.ok("BEGIN");
    h.ok("SET TRANSACTION READ ONLY");
    assert_eq!(h.code(&insert(1, "op-f")), "25006");
    assert_eq!(h.code("SET TRANSACTION READ WRITE"), "25P02");
    assert_eq!(
        h.code("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY"),
        "25P02"
    );
}

#[test]
fn session_characteristics_are_inherited_by_begin_and_start_transaction() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    assert_eq!(
        h.ok("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ ONLY"),
        SqlOutcome::SetTransaction
    );
    assert_eq!(h.txn.status(), TransactionStatus::Idle);
    for begin in ["BEGIN", "START TRANSACTION"] {
        h.ok(begin);
        assert_eq!(
            h.txn.access_mode(),
            Some(TxnAccessMode::ReadOnly),
            "{begin}"
        );
        assert_eq!(
            h.txn.isolation_level(),
            Some(RequestedIsolationLevel::Serializable),
            "{begin}"
        );
        assert_eq!(h.code(&insert(1, "op-d")), "25006", "{begin}");
        h.ok("ROLLBACK");
    }
}

#[test]
fn explicit_begin_modifiers_override_defaults_and_defaults_merge_per_item() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.ok("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL REPEATABLE READ");
    h.ok("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY");
    h.ok("BEGIN READ WRITE");
    assert_eq!(h.txn.access_mode(), Some(TxnAccessMode::ReadWrite));
    assert_eq!(
        h.txn.isolation_level(),
        Some(RequestedIsolationLevel::RepeatableRead)
    );
    h.ok(&insert(1, "op-o"));
    h.ok("COMMIT");
    assert_eq!(visible_rows(&engine, &h.caller), 1);
    h.ok("BEGIN ISOLATION LEVEL READ COMMITTED");
    assert_eq!(h.txn.access_mode(), Some(TxnAccessMode::ReadOnly));
    assert_eq!(
        h.txn.isolation_level(),
        Some(RequestedIsolationLevel::ReadCommitted)
    );
}

#[test]
fn session_default_changed_inside_a_transaction_follows_rollback_and_commit() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.ok("BEGIN");
    h.ok("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY");
    // 実行中の Tx 自身のアクセスモードは変わらない。
    assert_eq!(h.txn.access_mode(), Some(TxnAccessMode::ReadWrite));
    h.ok("ROLLBACK");
    h.ok("BEGIN");
    assert_eq!(h.txn.access_mode(), Some(TxnAccessMode::ReadWrite));
    h.ok("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY");
    h.ok("COMMIT");
    h.ok("BEGIN");
    assert_eq!(h.txn.access_mode(), Some(TxnAccessMode::ReadOnly));
}

#[test]
fn unsupported_forms_are_42601() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    for sql in [
        "SET TRANSACTION",
        "SET TRANSACTION DEFERRABLE",
        "SET TRANSACTION NOT DEFERRABLE",
        "SET TRANSACTION SNAPSHOT 'x'",
        "SET TRANSACTION ISOLATION LEVEL READ",
        "SET TRANSACTION READ ONLY, READ WRITE",
        "SET TRANSACTION READ ONLY,",
        "SET SESSION CHARACTERISTICS AS TRANSACTION",
        "SET SESSION CHARACTERISTICS TRANSACTION READ ONLY",
        "SET SESSION CHARACTERISTICS AS TRANSACTION DEFERRABLE",
        "BEGIN DEFERRABLE",
    ] {
        let mut h = Harness::new(&engine);
        if sql != "BEGIN DEFERRABLE" {
            h.ok("BEGIN");
        }
        assert_eq!(h.code(sql), "42601", "{sql}");
    }
}

#[test]
fn entry_without_transaction_context_rejects_both_statements() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    for sql in [
        "SET TRANSACTION READ ONLY",
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY",
    ] {
        let err = engine
            .execute_sql_in_session(&ctx(), &mut SessionState::default(), sql)
            .expect_err(sql);
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
}

#[test]
fn statement_limit_still_applies_to_set_transaction() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.txn = SessionTransaction::new(TransactionLimits {
        max_statements: 3,
        ..Default::default()
    });
    h.ok("BEGIN");
    h.ok("SET TRANSACTION READ ONLY");
    h.ok("SET TRANSACTION READ WRITE");
    h.ok("SET TRANSACTION READ ONLY");
    assert_eq!(h.code("SET TRANSACTION READ WRITE"), "54000");
}

#[test]
fn tenant_visibility_is_unchanged_by_set_transaction() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let mut h = Harness::new(&engine);
    h.ok("BEGIN");
    h.ok(&insert(1, "op-t"));
    h.ok("COMMIT");
    let before = visible_rows(&engine, &h.caller);
    h.ok("SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE, READ ONLY");
    h.ok("BEGIN");
    h.ok("SET TRANSACTION READ ONLY");
    h.ok("ROLLBACK");
    assert_eq!(visible_rows(&engine, &h.caller), before);
    let other =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("tenant");
    assert_eq!(visible_rows(&engine, &other), 0);
}
