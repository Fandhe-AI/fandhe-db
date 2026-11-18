//! 明示トランザクション内の表・索引の DDL と `SAVEPOINT`・`ROLLBACK TO`・失敗からの復帰・接続断の整合
//! （SQL-80・SQL-31・SQL-69・TABLE-22・TASK-349 のポインタ、Issue #3317。設計記録:
//! `docs/design/explicit-transaction.md`「#3317 の実装記録」）の結合テスト。
//!
//! `ROLLBACK TO` が savepoint より後の DDL だけを取り消し、より前の DDL は COMMIT で残ること、
//! `25P02` からの `ROLLBACK`・drop（接続断）ではカタログの変更が残らないことを固定する。
//! `sql80_txn_table_ddl.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        CleanupGuard(path),
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn tx<'a>(
    core: &'a EngineCore,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_txn(&ctx("alice"), session, txn, sql)
}

fn tx_ok<'a>(
    core: &'a EngineCore,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> SqlOutcome {
    tx(core, session, txn, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

fn tx_code<'a>(
    core: &'a EngineCore,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> &'static str {
    tx(core, session, txn, sql).expect_err(sql).wire_code()
}

fn tx_rows<'a>(
    core: &'a EngineCore,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> usize {
    match tx_ok(core, session, txn, sql) {
        SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// 別セッション（autocommit）からの SELECT。`Ok(行数)`／`Err(wire_code)`。
fn read_rows(core: &EngineCore, tenant: &str, sql: &str) -> Result<usize, &'static str> {
    match core.execute_sql_in_session(&ctx(tenant), &mut SessionState::default(), sql) {
        Ok(SqlOutcome::Query(result)) => Ok(result.rows.len()),
        Ok(other) => panic!("expected Query, got {other:?}"),
        Err(e) => Err(e.wire_code()),
    }
}

fn begin<'a>(core: &'a EngineCore, session: &mut SessionState, txn: &mut SessionTransaction<'a>) {
    assert_eq!(tx_ok(core, session, txn, "BEGIN"), SqlOutcome::Begin);
}

fn ins(table: &str, id: u64) -> String {
    format!(
        "INSERT INTO {table} (id, body) VALUES ({id}, 'x') USING OPERATION_ID 'op-{table}-{id}'"
    )
}

const CREATE_A: &str = "CREATE TABLE a (id BIGINT PRIMARY KEY, body TEXT)";
const CREATE_B: &str = "CREATE TABLE b (id BIGINT PRIMARY KEY, body TEXT)";
const CREATE_C: &str = "CREATE TABLE c (id BIGINT PRIMARY KEY)";

#[test]
fn rollback_to_a_savepoint_taken_before_the_ddl_discards_it() {
    let (core, _guard) = new_core("sql80sp-restorable");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "SAVEPOINT s");
    tx_ok(&core, &mut session, &mut txn, CREATE_A);
    tx_ok(&core, &mut session, &mut txn, &ins("a", 1));
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK TO s");
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    // 取り消した後は同名の表を作り直せる（42P07 にならない）。
    tx_ok(&core, &mut session, &mut txn, CREATE_A);
    tx_ok(&core, &mut session, &mut txn, "COMMIT");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM a"), Ok(0));
}

#[test]
fn rollback_to_after_ddl_keeps_earlier_ddl_and_drops_later_ddl() {
    let (core, _guard) = new_core("sql80sp-imaged");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, CREATE_A);
    tx_ok(&core, &mut session, &mut txn, &ins("a", 1));
    tx_ok(&core, &mut session, &mut txn, "SAVEPOINT s");
    tx_ok(&core, &mut session, &mut txn, CREATE_B);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "ALTER TABLE a ADD COLUMN c BIGINT",
    );
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE INDEX a_body ON a (body)",
    );
    tx_ok(&core, &mut session, &mut txn, &ins("a", 2));
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK TO s");
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    // 取り消した表・列は存在しない（作り直せる）。確かめた分は同じ savepoint へ戻して消す。
    tx_ok(&core, &mut session, &mut txn, CREATE_B);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "ALTER TABLE a ADD COLUMN c BIGINT",
    );
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK TO s");
    assert_eq!(
        tx_rows(&core, &mut session, &mut txn, "SELECT id FROM a"),
        1
    );
    // 取り消した索引は作り直せる。
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE INDEX a_body ON a (body)",
    );
    tx_ok(&core, &mut session, &mut txn, &ins("a", 3));
    tx_ok(&core, &mut session, &mut txn, "COMMIT");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM a"), Ok(2));
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM b"), Err("42P01"));
    assert_eq!(read_rows(&core, "alice", "SELECT c FROM a"), Err("42703"));
}

#[test]
fn nested_savepoints_roll_back_in_stages_and_can_be_reused() {
    let (core, _guard) = new_core("sql80sp-nested");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, CREATE_A);
    tx_ok(&core, &mut session, &mut txn, "SAVEPOINT s1");
    tx_ok(&core, &mut session, &mut txn, CREATE_B);
    tx_ok(&core, &mut session, &mut txn, "SAVEPOINT s2");
    tx_ok(&core, &mut session, &mut txn, CREATE_C);
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK TO s2");
    assert_eq!(
        tx_rows(&core, &mut session, &mut txn, "SELECT id FROM b"),
        0
    );
    // c は無い（作り直せる）。同じ savepoint へ 2 回戻せる。
    tx_ok(&core, &mut session, &mut txn, CREATE_C);
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK TO s2");
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK TO s1");
    // b も無い（作り直せる）。
    tx_ok(&core, &mut session, &mut txn, CREATE_B);
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK TO s1");
    assert_eq!(
        tx_rows(&core, &mut session, &mut txn, "SELECT id FROM a"),
        0
    );
    tx_ok(&core, &mut session, &mut txn, "COMMIT");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM a"), Ok(0));
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM b"), Err("42P01"));
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM c"), Err("42P01"));
}

#[test]
fn rolled_back_drop_table_keeps_rows_and_lifts_the_post_drop_restriction() {
    let (core, _guard) = new_core("sql80sp-drop");
    let mut setup = ddl_session();
    core.execute_sql_in_session(&ctx("alice"), &mut setup, CREATE_A)
        .expect("create");
    core.execute_sql_in_session(&ctx("alice"), &mut setup, &ins("a", 1))
        .expect("insert");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "SAVEPOINT s");
    tx_ok(&core, &mut session, &mut txn, "DROP TABLE a");
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK TO s");
    // DDL 状態の印が戻り、DDL 後に拒否される UPDATE が再び通る。
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "UPDATE a SET body = 'z' WHERE id = 1",
    );
    assert_eq!(
        tx_rows(&core, &mut session, &mut txn, "SELECT id FROM a"),
        1
    );
    tx_ok(&core, &mut session, &mut txn, "COMMIT");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM a"), Ok(1));
}

#[test]
fn rolled_back_drop_table_does_not_evict_the_owned_sequence() {
    let (core, _guard) = new_core("sql80sp-seq");
    let mut setup = ddl_session();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut setup,
        "CREATE TABLE s (id BIGSERIAL PRIMARY KEY, body TEXT)",
    )
    .expect("create");
    for _ in 0..3 {
        core.execute_sql_in_session(
            &ctx("alice"),
            &mut setup,
            "INSERT INTO s (body) VALUES ('x')",
        )
        .expect("insert");
    }
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "SAVEPOINT s1");
    tx_ok(&core, &mut session, &mut txn, "DROP TABLE s");
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK TO s1");
    tx_ok(&core, &mut session, &mut txn, "COMMIT");
    // 採番が続きの値から再開し、既存行と衝突しない。
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut setup,
        "INSERT INTO s (body) VALUES ('y')",
    )
    .expect("insert after rollback to");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM s"), Ok(4));
}

#[test]
fn failed_ddl_recovers_through_rollback_to_an_earlier_savepoint() {
    let (core, _guard) = new_core("sql80sp-failed");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, CREATE_A);
    tx_ok(&core, &mut session, &mut txn, "SAVEPOINT s");
    assert_eq!(tx_code(&core, &mut session, &mut txn, CREATE_A), "42P07");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert_eq!(tx_code(&core, &mut session, &mut txn, "COMMIT"), "25P02");
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK TO s");
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    tx_ok(&core, &mut session, &mut txn, &ins("a", 1));
    tx_ok(&core, &mut session, &mut txn, "COMMIT");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM a"), Ok(1));
}

#[test]
fn rollback_after_a_statement_error_leaves_no_catalog_residue() {
    for with_savepoint in [false, true] {
        let (core, _guard) = new_core("sql80sp-25p02");
        let mut session = ddl_session();
        let mut txn = core.new_session_transaction();
        begin(&core, &mut session, &mut txn);
        tx_ok(&core, &mut session, &mut txn, CREATE_A);
        if with_savepoint {
            tx_ok(&core, &mut session, &mut txn, "SAVEPOINT s");
            tx_ok(&core, &mut session, &mut txn, CREATE_B);
        }
        assert_eq!(
            tx_code(&core, &mut session, &mut txn, "SELECT id FROM nope"),
            "42P01"
        );
        assert_eq!(txn.status(), TransactionStatus::Failed);
        assert_eq!(tx_code(&core, &mut session, &mut txn, "COMMIT"), "25P02");
        tx_ok(&core, &mut session, &mut txn, "ROLLBACK");
        assert_eq!(read_rows(&core, "alice", "SELECT id FROM a"), Err("42P01"));
        assert_eq!(read_rows(&core, "alice", "SELECT id FROM b"), Err("42P01"));
    }
}

#[test]
fn dropping_the_transaction_discards_everything_and_frees_the_writer() {
    let (core, _guard) = new_core("sql80sp-drop-conn");
    let mut session = ddl_session();
    {
        let mut txn = core.new_session_transaction();
        begin(&core, &mut session, &mut txn);
        tx_ok(&core, &mut session, &mut txn, CREATE_A);
        tx_ok(&core, &mut session, &mut txn, "SAVEPOINT s");
        tx_ok(&core, &mut session, &mut txn, CREATE_B);
        tx_ok(&core, &mut session, &mut txn, "ROLLBACK TO s");
        tx_ok(&core, &mut session, &mut txn, CREATE_C);
        // 接続断: Tx を持ったまま drop する。
    }
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM a"), Err("42P01"));
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM c"), Err("42P01"));
    core.execute_sql_in_session(&ctx("alice"), &mut ddl_session(), CREATE_A)
        .expect("writer is free and the name is unused");
}

#[test]
fn rollback_to_keeps_tenant_rows_and_visibility_unchanged() {
    let (core, _guard) = new_core("sql80sp-tenant");
    let mut setup = ddl_session();
    core.execute_sql_in_session(&ctx("alice"), &mut setup, CREATE_A)
        .expect("create");
    core.execute_sql_in_session(&ctx("alice"), &mut setup, &ins("a", 1))
        .expect("alice row");
    core.execute_sql_in_session(&ctx("bob"), &mut SessionState::default(), &ins("a", 2))
        .expect("bob row");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "SAVEPOINT s");
    tx_ok(&core, &mut session, &mut txn, CREATE_B);
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK TO s");
    assert_eq!(
        tx_rows(&core, &mut session, &mut txn, "SELECT id FROM a"),
        1
    );
    tx_ok(&core, &mut session, &mut txn, "COMMIT");
    assert_eq!(read_rows(&core, "alice", "SELECT id FROM a"), Ok(1));
    assert_eq!(read_rows(&core, "bob", "SELECT id FROM a"), Ok(1));
    assert_eq!(read_rows(&core, "bob", "SELECT id FROM b"), Err("42P01"));
}

// intentional: docs/design/explicit-transaction.md
#[test]
fn do_block_after_ddl_is_still_rejected_with_0a000() {
    let (core, _guard) = new_core("sql80sp-do");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, CREATE_A);
    assert_eq!(
        tx_code(&core, &mut session, &mut txn, "DO $$ BEGIN NULL; END $$"),
        "0A000"
    );
}
