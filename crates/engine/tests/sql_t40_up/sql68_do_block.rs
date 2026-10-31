//! `DO` ブロック（SQL-68・Issue #2228）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/sql.md` SQL-68。設計記録は `docs/design/plpgsql-subset.md`
//! 「Issue #2228 追記」。期待値は PostgreSQL 17 の `DO` の公開された挙動（CommandComplete タグ `DO`・
//! 本体のエラーで副作用なし・明示 Tx への参加・言語エラーの SQLSTATE）に合わせる。
//!
//! 実 `Storage` ＋ `CpuScalarProvider` の production 経路（`execute_sql_in_session`／
//! `execute_sql_in_txn`）で、autocommit・明示 Tx・RLS・上限・禁止形を固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn setup(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx("alice"), &mut s, "CREATE TABLE t (n INTEGER)")
        .expect("create table");
    (core, CleanupGuard(path))
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    match run(core, tenant, sql) {
        Ok(o) => panic!("must fail: {sql} -> {o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn count(core: &EngineCore, tenant: &str) -> u64 {
    let r = core
        .execute_sql(&ctx(tenant), "SELECT COUNT(*) FROM t")
        .expect("count");
    match r.rows[0].cells[0] {
        Cell::Integer(v) => v,
        ref o => panic!("unexpected {o:?}"),
    }
}

#[test]
fn autocommit_do_writes_and_returns_do_outcome() {
    let (core, _g) = setup("sql68-do-auto");
    let out = run(
        &core,
        "alice",
        "DO 'BEGIN INSERT INTO t (n) VALUES (1); INSERT INTO t (n) VALUES (2); END'",
    )
    .expect("do");
    assert!(matches!(out, SqlOutcome::Do), "{out:?}");
    assert_eq!(count(&core, "alice"), 2);
    // 他テナントには見えない（RLS）。
    assert_eq!(count(&core, "bob"), 0);
}

#[test]
fn language_clause_forms_and_errors() {
    let (core, _g) = setup("sql68-do-lang");
    for s in [
        "DO LANGUAGE plpgsql 'BEGIN NULL; END'",
        "DO 'BEGIN NULL; END' LANGUAGE plpgsql",
    ] {
        assert!(matches!(run(&core, "alice", s), Ok(SqlOutcome::Do)), "{s}");
    }
    assert_eq!(code(&core, "alice", "DO LANGUAGE sql 'SELECT 1'"), "0A000");
    assert_eq!(code(&core, "alice", "DO LANGUAGE nosuch 'x'"), "42704");
    assert_eq!(
        code(
            &core,
            "alice",
            "DO 'BEGIN NULL; END' USING OPERATION_ID 'x'"
        ),
        "0A000"
    );
}

#[test]
fn body_error_leaves_no_side_effects() {
    let (core, _g) = setup("sql68-do-err");
    // 2 文目の構文誤りは定義時検証で拒否され、1 文目の書き込みも起きない。
    let c = code(
        &core,
        "alice",
        "DO 'BEGIN INSERT INTO t (n) VALUES (1); INSERT INTO nosuch (n) VALUES (2); END'",
    );
    assert!(!c.is_empty());
    assert_eq!(count(&core, "alice"), 0);
    // 実行時エラー（`0A000` の RAISE）でも同様。
    assert_eq!(
        code(
            &core,
            "alice",
            "DO 'BEGIN INSERT INTO t (n) VALUES (1); RAISE EXCEPTION ''x''; END'"
        ),
        "0A000"
    );
    assert_eq!(count(&core, "alice"), 0);
}

#[test]
fn explicit_transaction_participation_and_rollback() {
    let (core, _g) = setup("sql68-do-txn");
    let mut txn = core.new_session_transaction();
    let mut ts = SessionState::default();
    let c = ctx("alice");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &c,
        &mut ts,
        &mut txn,
        "DO 'BEGIN INSERT INTO t (n) VALUES (1); END'",
    )
    .expect("do in txn");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(count(&core, "alice"), 0);

    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "BEGIN")
        .expect("begin 2");
    core.execute_sql_in_txn(
        &c,
        &mut ts,
        &mut txn,
        "DO 'BEGIN INSERT INTO t (n) VALUES (1); END'",
    )
    .expect("do in txn 2");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(count(&core, "alice"), 1);
}

#[test]
fn body_error_in_explicit_transaction_fails_the_transaction() {
    let (core, _g) = setup("sql68-do-txn-fail");
    let mut txn = core.new_session_transaction();
    let mut ts = SessionState::default();
    let c = ctx("alice");
    core.execute_sql_in_txn(&c, &mut ts, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(
            &c,
            &mut ts,
            &mut txn,
            "DO 'BEGIN RAISE EXCEPTION ''x''; END'",
        )
        .expect_err("raise is unsupported");
    assert_eq!(e.wire_code(), "0A000");
    let e = core
        .execute_sql_in_txn(&c, &mut ts, &mut txn, "SELECT COUNT(*) FROM t")
        .expect_err("failed transaction");
    assert_eq!(e.wire_code(), "25P02");
}

#[test]
fn nested_do_and_ddl_in_body_are_rejected() {
    let (core, _g) = setup("sql68-do-nested");
    assert_eq!(
        code(&core, "alice", "DO 'BEGIN DO ''BEGIN NULL; END''; END'"),
        "42601"
    );
    assert_eq!(
        code(&core, "alice", "DO 'BEGIN DROP TABLE t; END'"),
        "0A000"
    );
}

#[test]
fn oversized_body_is_54000() {
    let (core, _g) = setup("sql68-do-big");
    let pad = "NULL; ".repeat(40_000);
    let sql = format!("DO 'BEGIN {pad} END'");
    assert_eq!(code(&core, "alice", &sql), "54000");
}

#[test]
fn multi_statement_message_with_do_is_treated_as_a_write() {
    use engine::sql::statement_splitter::{classify_statement, StatementEffect};
    assert_eq!(
        classify_statement("DO 'BEGIN NULL; END'"),
        StatementEffect::Write
    );
}
