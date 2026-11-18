//! 明示トランザクション内の VIEW・SEQUENCE・SCHEMA・ENUM 型の DDL（SQL-80・SQL-31・SQL-23・TABLE-22・
//! TABLE-24・TABLE-27・TABLE-14・TASK-349 のポインタ、Issue #3315。設計記録:
//! `docs/design/explicit-transaction.md`「明示トランザクション内の DDL の方式」）の結合テスト。
//!
//! `EngineCore::execute_sql_in_txn`（明示 Tx の production 入口）で、対象の DDL を共有 write txn へ積み、
//! COMMIT で原子的に確定し、ROLLBACK では痕跡を残さないことを固定する。SEQUENCE の採番状態は
//! 非トランザクション性を保つ（ROLLBACK しても採番済みの値は戻らない）。未対応の形は `0A000` で
//! fail-closed になる（受け皿は #3311）。`sql80_txn_table_ddl.rs` と同じ流儀。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
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

fn begin<'a>(core: &'a EngineCore, session: &mut SessionState, txn: &mut SessionTransaction<'a>) {
    assert_eq!(tx_ok(core, session, txn, "BEGIN"), SqlOutcome::Begin);
}

fn commit<'a>(core: &'a EngineCore, session: &mut SessionState, txn: &mut SessionTransaction<'a>) {
    assert_eq!(tx_ok(core, session, txn, "COMMIT"), SqlOutcome::Commit);
}

fn rollback<'a>(
    core: &'a EngineCore,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'a>,
) {
    assert_eq!(tx_ok(core, session, txn, "ROLLBACK"), SqlOutcome::Rollback);
}

/// autocommit の文（別セッション相当）。
fn auto(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), &mut ddl_session(), sql)
}

fn auto_ok(core: &EngineCore, tenant: &str, sql: &str) {
    auto(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

fn auto_code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    auto(core, tenant, sql).expect_err(sql).wire_code()
}

/// `SELECT id, n FROM t` の `n` を id 昇順で返す。
fn column_n(core: &EngineCore) -> Vec<i64> {
    let r = core
        .execute_sql(&ctx("alice"), "SELECT id, n FROM t LIMIT 1000")
        .expect("select");
    let mut rows: Vec<(i64, i64)> = r
        .rows
        .iter()
        .map(|row| {
            let id = match row.cells[0] {
                Cell::SignedInteger(v) => v,
                Cell::Integer(v) => v as i64,
                ref o => panic!("id cell {o:?}"),
            };
            let n = match row.cells[1] {
                Cell::SignedInteger(v) => v,
                ref o => panic!("n cell {o:?}"),
            };
            (id, n)
        })
        .collect();
    rows.sort();
    rows.into_iter().map(|(_, n)| n).collect()
}

fn insert_next(id: u64, seq: &str) -> String {
    format!(
        "INSERT INTO t (id, n) VALUES ({id}, nextval('{seq}')) USING OPERATION_ID 'op-{id}-{seq}'"
    )
}

const CREATE_T: &str = "CREATE TABLE t (id BIGINT PRIMARY KEY, n BIGINT)";

// ---------------------------------------------------------------------------
// SEQUENCE
// ---------------------------------------------------------------------------

#[test]
fn sequence_create_commits_and_rollback_leaves_nothing() {
    let (core, _g) = new_core("sql3315-seq-create");
    let mut session = ddl_session();

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "CREATE SEQUENCE s1");
    // Tx が writer を保持している間、別セッション（別テナントを含む）の DDL は writer の待機で `55P03`
    // （COMMIT 前の内容は観測できない）。
    assert_eq!(auto_code(&core, "alice", "DROP SEQUENCE s1"), "55P03");
    assert_eq!(auto_code(&core, "bob", "DROP SEQUENCE s1"), "55P03");
    rollback(&core, &mut session, &mut txn);
    assert_eq!(auto_code(&core, "alice", "DROP SEQUENCE s1"), "42P01");

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "CREATE SEQUENCE s1");
    commit(&core, &mut session, &mut txn);
    // 確定後は全テナント共有カタログとして衝突が見える。
    assert_eq!(auto_code(&core, "bob", "CREATE SEQUENCE s1"), "42P07");
    auto_ok(&core, "alice", "DROP SEQUENCE s1");
}

#[test]
fn sequence_statement_with_trailing_semicolon_is_accepted() {
    let (core, _g) = new_core("sql3315-seq-semicolon");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "CREATE SEQUENCE s1;");
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "ALTER SEQUENCE s1 INCREMENT BY 2;",
    );
    commit(&core, &mut session, &mut txn);
    auto_ok(&core, "alice", "DROP SEQUENCE s1");
}

#[test]
fn sequence_numbers_consumed_in_a_rolled_back_txn_are_not_returned() {
    let (core, _g) = new_core("sql3315-seq-nontx");
    let mut session = ddl_session();
    auto_ok(&core, "alice", CREATE_T);
    auto_ok(&core, "alice", "CREATE SEQUENCE s");

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    // 消費して ROLLBACK（DDL を積まない Tx。採番は非トランザクション）。
    tx_ok(&core, &mut session, &mut txn, &insert_next(1, "s"));
    rollback(&core, &mut session, &mut txn);

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    // DDL を積んだ Tx でも、既存の SEQUENCE の採番は進み続ける。
    tx_ok(&core, &mut session, &mut txn, "CREATE SEQUENCE other");
    commit(&core, &mut session, &mut txn);
    auto_ok(&core, "alice", &insert_next(2, "s"));
    assert_eq!(column_n(&core), vec![2]);
}

#[test]
fn drop_sequence_rollback_keeps_the_sequence_and_its_position() {
    let (core, _g) = new_core("sql3315-seq-drop-rollback");
    let mut session = ddl_session();
    auto_ok(&core, "alice", CREATE_T);
    auto_ok(&core, "alice", "CREATE SEQUENCE s");
    auto_ok(&core, "alice", &insert_next(1, "s"));

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "DROP SEQUENCE s");
    rollback(&core, &mut session, &mut txn);
    // メモリ上の現在値を捨てていない（ROLLBACK では何も変わらない）。
    auto_ok(&core, "alice", &insert_next(2, "s"));
    assert_eq!(column_n(&core), vec![1, 2]);

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "DROP SEQUENCE s");
    commit(&core, &mut session, &mut txn);
    assert_eq!(auto_code(&core, "alice", &insert_next(3, "s")), "42P01");
    // 同名で作り直すと START から始まる。
    auto_ok(&core, "alice", "CREATE SEQUENCE s");
    auto_ok(&core, "alice", &insert_next(3, "s"));
    assert_eq!(column_n(&core), vec![1, 2, 1]);
}

#[test]
fn internal_id_reuse_after_rollback_does_not_inherit_a_stale_cursor() {
    let (core, _g) = new_core("sql3315-seq-id-reuse");
    let mut session = ddl_session();
    auto_ok(&core, "alice", CREATE_T);

    // 同じ Tx で作って採番し ROLLBACK する。内部 ID のカウンタは巻き戻るが、メモリ上のカーソルは残りうる。
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "CREATE SEQUENCE a START 100");
    // 作成直後の同 Tx の `nextval` が通るか否かに依存しないよう、結果は問わない。
    tx_ok(&core, &mut session, &mut txn, &insert_next(1, "a"));
    let _ = tx(&core, &mut session, &mut txn, "ROLLBACK");

    // 再利用された内部 ID で作る SEQUENCE は START から始まる。
    auto_ok(&core, "alice", "CREATE SEQUENCE b START 7");
    auto_ok(&core, "alice", &insert_next(1, "b"));
    assert_eq!(column_n(&core), vec![7]);
}

#[test]
fn internal_id_reuse_after_rollback_does_not_inherit_session_currval_or_lastval() {
    let (core, _g) = new_core("sql3315-seq-id-reuse-session");
    let mut session = ddl_session();
    auto_ok(&core, "alice", CREATE_T);

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "CREATE SEQUENCE a START 100");
    tx_ok(&core, &mut session, &mut txn, &insert_next(1, "a"));
    rollback(&core, &mut session, &mut txn);

    // 同じセッションで、再利用された内部 ID の別 SEQUENCE を作る（未採番）。
    core.execute_sql_in_session(&ctx("alice"), &mut session, "CREATE SEQUENCE b START 7")
        .expect("create b");
    let currval = core
        .execute_sql_in_session(
            &ctx("alice"),
            &mut session,
            "INSERT INTO t (id, n) VALUES (2, currval('b')) USING OPERATION_ID 'op-cv'",
        )
        .expect_err("currval before nextval");
    assert_eq!(currval.wire_code(), "55000");
    let lastval = core
        .execute_sql_in_session(&ctx("alice"), &mut session, "SELECT lastval()")
        .expect_err("lastval before nextval");
    assert_eq!(lastval.wire_code(), "55000");
}

#[test]
fn alter_sequence_options_and_rename_commit_and_rollback() {
    let (core, _g) = new_core("sql3315-seq-alter");
    let mut session = ddl_session();
    auto_ok(&core, "alice", CREATE_T);
    auto_ok(&core, "alice", "CREATE SEQUENCE s");

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "ALTER SEQUENCE s INCREMENT BY 10",
    );
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "ALTER SEQUENCE s RENAME TO s2",
    );
    rollback(&core, &mut session, &mut txn);
    // ROLLBACK 後は旧定義・旧名のまま。
    auto_ok(&core, "alice", &insert_next(1, "s"));
    auto_ok(&core, "alice", &insert_next(2, "s"));
    assert_eq!(column_n(&core), vec![1, 2]);

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "ALTER SEQUENCE s INCREMENT BY 10",
    );
    commit(&core, &mut session, &mut txn);
    auto_ok(&core, "alice", &insert_next(3, "s"));
    assert_eq!(column_n(&core), vec![1, 2, 12]);

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "ALTER SEQUENCE s RENAME TO s2",
    );
    commit(&core, &mut session, &mut txn);
    assert_eq!(auto_code(&core, "alice", &insert_next(4, "s")), "42P01");
    auto_ok(&core, "alice", &insert_next(4, "s2"));
}

#[test]
fn alter_sequence_restart_is_rejected_without_side_effects() {
    let (core, _g) = new_core("sql3315-seq-restart");
    let mut session = ddl_session();
    auto_ok(&core, "alice", "CREATE SEQUENCE s");
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    // tracked: #3311（RESTART の Tx 内対応）
    assert_eq!(
        tx_code(&core, &mut session, &mut txn, "ALTER SEQUENCE s RESTART"),
        "0A000"
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    rollback(&core, &mut session, &mut txn);
}

// ---------------------------------------------------------------------------
// ENUM 型
// ---------------------------------------------------------------------------

#[test]
fn enum_type_create_and_drop_commit_and_rollback() {
    let (core, _g) = new_core("sql3315-type");
    let mut session = ddl_session();

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE TYPE mood AS ENUM ('sad', 'ok')",
    );
    assert_eq!(auto_code(&core, "alice", "DROP TYPE mood"), "55P03");
    rollback(&core, &mut session, &mut txn);
    assert_eq!(auto_code(&core, "alice", "DROP TYPE mood"), "42704");

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE TYPE mood AS ENUM ('sad', 'ok');",
    );
    commit(&core, &mut session, &mut txn);
    assert_eq!(
        auto_code(&core, "bob", "CREATE TYPE mood AS ENUM ('x')"),
        "42710"
    );

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "DROP TYPE mood");
    rollback(&core, &mut session, &mut txn);
    assert_eq!(
        auto_code(&core, "bob", "CREATE TYPE mood AS ENUM ('x')"),
        "42710"
    );

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "DROP TYPE mood");
    // `IF EXISTS` で不在の no-op は成功する。
    tx_ok(&core, &mut session, &mut txn, "DROP TYPE IF EXISTS ghost");
    commit(&core, &mut session, &mut txn);
    auto_ok(&core, "bob", "CREATE TYPE mood AS ENUM ('x')");
}

#[test]
fn drop_type_with_a_dependent_column_is_rejected() {
    let (core, _g) = new_core("sql3315-type-dep");
    let mut session = ddl_session();
    auto_ok(&core, "alice", "CREATE TYPE mood AS ENUM ('sad', 'ok')");
    auto_ok(
        &core,
        "alice",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, m mood)",
    );
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    assert_eq!(
        tx_code(&core, &mut session, &mut txn, "DROP TYPE mood"),
        "2BP01"
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    rollback(&core, &mut session, &mut txn);
}

// ---------------------------------------------------------------------------
// SCHEMA
// ---------------------------------------------------------------------------

#[test]
fn schema_create_rename_drop_commit_and_rollback() {
    let (core, _g) = new_core("sql3315-schema");
    let mut session = ddl_session();

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "CREATE SCHEMA app");
    // writer を保持している間、別セッションの DDL は `55P03`。
    assert_eq!(auto_code(&core, "alice", "DROP SCHEMA app"), "55P03");
    rollback(&core, &mut session, &mut txn);
    assert_eq!(auto_code(&core, "alice", "DROP SCHEMA app"), "3F000");

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "CREATE SCHEMA app;");
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "ALTER SCHEMA app RENAME TO app2",
    );
    commit(&core, &mut session, &mut txn);
    // 他テナントの同名の SCHEMA には作用しない（テナント単位のカタログ）。
    assert_eq!(auto_code(&core, "bob", "DROP SCHEMA app2"), "3F000");
    assert_eq!(auto_code(&core, "alice", "DROP SCHEMA app"), "3F000");

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "DROP SCHEMA app2");
    rollback(&core, &mut session, &mut txn);
    auto_code(&core, "alice", "CREATE SCHEMA app2");

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "DROP SCHEMA app2");
    tx_ok(&core, &mut session, &mut txn, "DROP SCHEMA IF EXISTS ghost");
    commit(&core, &mut session, &mut txn);
    auto_ok(&core, "alice", "CREATE SCHEMA app2");
}

#[test]
fn statements_after_a_schema_change_are_rejected_fail_closed() {
    let (core, _g) = new_core("sql3315-schema-gate");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "CREATE SCHEMA app");
    // 後続文の `search_path` の解決・作成先の選択は確定済みカタログを読むため、SCHEMA の DDL 以外は拒否する。
    // tracked: #3311
    assert_eq!(tx_code(&core, &mut session, &mut txn, CREATE_T), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    rollback(&core, &mut session, &mut txn);
    assert_eq!(auto_code(&core, "alice", "DROP SCHEMA app"), "3F000");
}

#[test]
fn schema_with_elements_and_cascade_are_rejected_without_side_effects() {
    let (core, _g) = new_core("sql3315-schema-unsupported");
    let mut session = ddl_session();
    auto_ok(&core, "alice", "CREATE SCHEMA keep");
    // tracked: #3311
    for sql in [
        "CREATE SCHEMA s9 CREATE TABLE t (id BIGINT PRIMARY KEY)",
        "DROP SCHEMA keep CASCADE",
    ] {
        let mut txn = core.new_session_transaction();
        begin(&core, &mut session, &mut txn);
        assert_eq!(
            tx_code(&core, &mut session, &mut txn, sql),
            "0A000",
            "{sql}"
        );
        assert_eq!(txn.status(), TransactionStatus::Failed);
        rollback(&core, &mut session, &mut txn);
    }
    assert_eq!(auto_code(&core, "alice", "CREATE SCHEMA keep"), "42P06");
    auto_ok(&core, "alice", "CREATE SCHEMA s9");
}

// ---------------------------------------------------------------------------
// VIEW
// ---------------------------------------------------------------------------

#[test]
fn view_create_and_drop_commit_and_rollback() {
    let (core, _g) = new_core("sql3315-view");
    let mut session = ddl_session();
    auto_ok(&core, "alice", CREATE_T);

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE VIEW v AS SELECT id, n FROM t",
    );
    // COMMIT 前の VIEW は別セッションの読み取りから見えない（一度も作られていない名前と同じ結果）。
    assert_eq!(auto_code(&core, "alice", "SELECT id FROM v"), "42P01");
    assert_eq!(auto_code(&core, "bob", "SELECT id FROM v"), "42P01");
    rollback(&core, &mut session, &mut txn);
    assert_eq!(auto_code(&core, "alice", "DROP VIEW v"), "42P01");

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE VIEW v AS SELECT id, n FROM t;",
    );
    commit(&core, &mut session, &mut txn);
    auto_ok(&core, "alice", "SELECT id FROM v");

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "DROP VIEW v");
    rollback(&core, &mut session, &mut txn);
    auto_ok(&core, "alice", "SELECT id FROM v");

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "DROP VIEW v");
    tx_ok(&core, &mut session, &mut txn, "DROP VIEW IF EXISTS ghost");
    commit(&core, &mut session, &mut txn);
    assert_eq!(auto_code(&core, "alice", "SELECT id FROM v"), "42P01");
}

#[test]
fn view_over_a_table_created_in_the_same_txn_commits_atomically() {
    let (core, _g) = new_core("sql3315-view-table");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, CREATE_T);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE VIEW v AS SELECT id, n FROM t",
    );
    commit(&core, &mut session, &mut txn);
    auto_ok(&core, "alice", "SELECT id FROM v");

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE TABLE u (id BIGINT PRIMARY KEY)",
    );
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE VIEW w AS SELECT id FROM u",
    );
    rollback(&core, &mut session, &mut txn);
    assert_eq!(auto_code(&core, "alice", "SELECT id FROM w"), "42P01");
    assert_eq!(auto_code(&core, "alice", "SELECT id FROM u"), "42P01");
}

#[test]
fn view_unsupported_forms_are_rejected_fail_closed() {
    let (core, _g) = new_core("sql3315-view-unsupported");
    let mut session = ddl_session();
    auto_ok(&core, "alice", CREATE_T);
    auto_ok(&core, "alice", "CREATE VIEW v AS SELECT id, n FROM t");
    // tracked: #3311（OR REPLACE・DDL 後の列別名／CHECK OPTION）
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    assert_eq!(
        tx_code(
            &core,
            &mut session,
            &mut txn,
            "CREATE OR REPLACE VIEW v AS SELECT id, n FROM t"
        ),
        "0A000"
    );
    rollback(&core, &mut session, &mut txn);

    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "CREATE SEQUENCE zz");
    assert_eq!(
        tx_code(
            &core,
            &mut session,
            &mut txn,
            "CREATE VIEW v2 (a, b) AS SELECT id, n FROM t"
        ),
        "0A000"
    );
    rollback(&core, &mut session, &mut txn);
    assert_eq!(auto_code(&core, "alice", "DROP SEQUENCE zz"), "42P01");
}

#[test]
fn view_with_column_aliases_is_accepted_before_any_ddl() {
    let (core, _g) = new_core("sql3315-view-aliases");
    let mut session = ddl_session();
    auto_ok(&core, "alice", CREATE_T);
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE VIEW v (a, b) AS SELECT id, n FROM t",
    );
    commit(&core, &mut session, &mut txn);
    auto_ok(&core, "alice", "SELECT a FROM v");
}

// ---------------------------------------------------------------------------
// 共通
// ---------------------------------------------------------------------------

#[test]
fn failed_ddl_marks_the_txn_failed_and_commit_rolls_back() {
    let (core, _g) = new_core("sql3315-failed");
    let mut session = ddl_session();
    auto_ok(&core, "alice", "CREATE SEQUENCE s");
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "CREATE SEQUENCE s2");
    // 既存名の作成は `42P07`。Tx は Failed になり、COMMIT は ROLLBACK 扱いで何も残さない。
    assert_eq!(
        tx_code(&core, &mut session, &mut txn, "CREATE SEQUENCE s"),
        "42P07"
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    let _ = tx(&core, &mut session, &mut txn, "COMMIT");
    assert_eq!(auto_code(&core, "alice", "DROP SEQUENCE s2"), "42P01");
}

#[test]
fn read_only_txn_rejects_object_ddl_with_25006() {
    let (core, _g) = new_core("sql3315-read-only");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    tx_ok(&core, &mut session, &mut txn, "BEGIN READ ONLY");
    for sql in [
        "CREATE SEQUENCE s",
        "CREATE TYPE mood AS ENUM ('a')",
        "CREATE SCHEMA app",
    ] {
        let mut t = core.new_session_transaction();
        tx_ok(&core, &mut session, &mut t, "BEGIN READ ONLY");
        assert_eq!(tx_code(&core, &mut session, &mut t, sql), "25006", "{sql}");
        rollback(&core, &mut session, &mut t);
    }
    rollback(&core, &mut session, &mut txn);
}

#[test]
fn ddl_without_permission_is_rejected_with_42501() {
    let (core, _g) = new_core("sql3315-perm");
    let mut session = SessionState::default();
    for sql in [
        "CREATE SEQUENCE s",
        "CREATE TYPE mood AS ENUM ('a')",
        "CREATE SCHEMA app",
        "DROP VIEW v",
    ] {
        let mut txn = core.new_session_transaction();
        begin(&core, &mut session, &mut txn);
        assert_eq!(
            tx_code(&core, &mut session, &mut txn, sql),
            "42501",
            "{sql}"
        );
        rollback(&core, &mut session, &mut txn);
    }
}

#[test]
fn savepoint_rollback_undoes_object_ddl() {
    let (core, _g) = new_core("sql3315-savepoint");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "CREATE SEQUENCE keep");
    tx_ok(&core, &mut session, &mut txn, "SAVEPOINT sp");
    tx_ok(&core, &mut session, &mut txn, "CREATE SEQUENCE dropme");
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE TYPE mood AS ENUM ('a')",
    );
    tx_ok(&core, &mut session, &mut txn, "ROLLBACK TO sp");
    commit(&core, &mut session, &mut txn);
    auto_ok(&core, "alice", "DROP SEQUENCE keep");
    assert_eq!(auto_code(&core, "alice", "DROP SEQUENCE dropme"), "42P01");
    assert_eq!(auto_code(&core, "alice", "DROP TYPE mood"), "42704");
}

/// Issue #3315: 保護対象 DDL（ロール等）を積んだ後も、VIEW・SEQUENCE・ENUM 型・SCHEMA の DDL を受理する
/// （simple-query 経路の許可リストが拡張 Execute 経路と同じ結果になる）。
#[test]
fn object_ddl_is_accepted_after_guarded_ddl() {
    let (core, _g) = new_core("sql3315-guarded");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    tx_ok(&core, &mut session, &mut txn, "CREATE ROLE r3315");
    tx_ok(&core, &mut session, &mut txn, "CREATE SEQUENCE gs");
    tx_ok(
        &core,
        &mut session,
        &mut txn,
        "CREATE TYPE gmood AS ENUM ('a', 'b')",
    );
    tx_ok(&core, &mut session, &mut txn, "DROP SEQUENCE gs");
    tx_ok(&core, &mut session, &mut txn, "CREATE SCHEMA gsch");
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    rollback(&core, &mut session, &mut txn);
}

/// Issue #3315: Tx 内 `CREATE TYPE` の予約名の違反は writer 取得前に拒否される。Tx は Failed になるが
/// writer を保持せず、別の autocommit の DDL が ROLLBACK を待たずに通る。
#[test]
fn create_type_validation_failure_does_not_hold_the_writer() {
    let (core, _g) = new_core("sql3315-type-precheck");
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    begin(&core, &mut session, &mut txn);
    let _ = tx_code(
        &core,
        &mut session,
        &mut txn,
        "CREATE TYPE int4 AS ENUM ('a')",
    );
    assert_eq!(txn.status(), TransactionStatus::Failed);
    auto_ok(&core, "alice", "CREATE SEQUENCE after_bad");
    rollback(&core, &mut session, &mut txn);
}
