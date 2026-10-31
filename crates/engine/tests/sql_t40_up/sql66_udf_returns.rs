//! `CREATE FUNCTION ... RETURNS <型>` の結合テスト（Issue #1916、対象ビヘイビア: SQL-66 (1)。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md`・`docs/spec/05-tasks.md` TASK-309）。
//!
//! RETURNS 句の受理・定義時の本体型検査（不一致は `42P13`、静的に決められない形は `0A000`）・
//! 呼び出し時の戻り値型への変換・入れ子呼び出し・永続化経路の fail-closed・セッション境界の維持を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, true),
            ],
        ))
        .expect("create table");
    let op_id = engine::recovery::required_op_id::OperationId::parse("op-1").expect("op");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx_for("tenant-a"),
        1,
        Visibility::Public,
        &[Value::Vector(vec![0.1, 0.2]), Value::Integer(7)],
        &op_id,
    )
    .expect("insert row");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run_as(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, String> {
    core.execute_sql_in_session(&ctx_for(tenant), session, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn run(core: &EngineCore, session: &mut SessionState, sql: &str) -> Result<SqlOutcome, String> {
    run_as(core, "tenant-a", session, sql)
}

fn code(core: &EngineCore, session: &mut SessionState, sql: &str) -> String {
    run(core, session, sql).expect_err("expected error")
}

fn define(core: &EngineCore, session: &mut SessionState, sql: &str) {
    match run(core, session, sql) {
        Ok(SqlOutcome::CreateFunction { .. }) => {}
        other => panic!("expected CreateFunction for {sql}, got {other:?}"),
    }
}

fn first_cell(core: &EngineCore, session: &mut SessionState, sql: &str) -> Cell {
    match run(core, session, sql) {
        Ok(SqlOutcome::Query(q)) => q.rows.first().expect("row").cells[0].clone(),
        other => panic!("expected query for {sql}, got {other:?}"),
    }
}

#[test]
fn returns_clause_is_accepted_and_result_is_converted() {
    let (core, _g) = new_core("sql66r-basic");
    let mut s = SessionState::default();
    define(
        &core,
        &mut s,
        "CREATE FUNCTION add1(x integer) RETURNS integer AS x + 1",
    );
    let cell = first_cell(&core, &mut s, "SELECT add1(2) FROM docs LIMIT 1");
    assert_ne!(cell, Cell::Float(3.0), "result must carry the integer type");
    assert_eq!(cell, Cell::SignedInteger(3));
}

#[test]
fn identity_and_widening_bodies_are_accepted() {
    let (core, _g) = new_core("sql66r-widen");
    let mut s = SessionState::default();
    define(
        &core,
        &mut s,
        "CREATE FUNCTION w(x integer) RETURNS bigint AS x",
    );
    assert_eq!(
        first_cell(&core, &mut s, "SELECT w(5) FROM docs LIMIT 1"),
        Cell::SignedInteger(5)
    );
    define(
        &core,
        &mut s,
        "CREATE FUNCTION t(x integer) RETURNS text AS x",
    );
    assert_eq!(
        first_cell(&core, &mut s, "SELECT t(2) FROM docs LIMIT 1"),
        Cell::Text("2".to_string())
    );
    define(
        &core,
        &mut s,
        "CREATE FUNCTION c() RETURNS integer AS 1.5::float8",
    );
    define(&core, &mut s, "CREATE FUNCTION n() RETURNS integer AS '5'");
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE FUNCTION m() RETURNS integer AS 'abc'"
        ),
        "22P02"
    );
    define(&core, &mut s, "CREATE FUNCTION z() RETURNS integer AS 1");
}

#[test]
fn numeric_literal_bodies_are_assignable_to_numeric_returns() {
    let (core, _g) = new_core("sql66r-numlit");
    let mut s = SessionState::default();
    for (i, ret) in ["integer", "double precision", "numeric", "bigint"]
        .iter()
        .enumerate()
    {
        define(
            &core,
            &mut s,
            &format!("CREATE FUNCTION d{i}() RETURNS {ret} AS 1.5"),
        );
        define(
            &core,
            &mut s,
            &format!("CREATE FUNCTION e{i}() RETURNS {ret} AS 2"),
        );
    }
    assert_eq!(
        first_cell(&core, &mut s, "SELECT e0() FROM docs LIMIT 1"),
        Cell::SignedInteger(2)
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE FUNCTION bad() RETURNS boolean AS 1.5"
        ),
        "42P13"
    );
}

#[test]
fn body_type_mismatch_is_42p13() {
    let (core, _g) = new_core("sql66r-mismatch");
    let mut s = SessionState::default();
    for (i, sql) in [
        "CREATE FUNCTION f(x integer) RETURNS boolean AS x + 1",
        "CREATE FUNCTION f(x integer) RETURNS integer AS 1::boolean",
        "CREATE FUNCTION f(x integer) RETURNS date AS x",
        "CREATE FUNCTION f(x text) RETURNS integer AS concat(x, 'a')",
        "CREATE FUNCTION f(x text) RETURNS integer AS x",
    ]
    .iter()
    .enumerate()
    {
        assert_eq!(code(&core, &mut s, sql), "42P13", "case {i}: {sql}");
    }
    // 失敗した定義はセッションに残らず、同名で再定義できる。
    define(
        &core,
        &mut s,
        "CREATE FUNCTION f(x integer) RETURNS integer AS x + 1",
    );
}

#[test]
fn statically_undecidable_forms_are_fail_closed() {
    let (core, _g) = new_core("sql66r-failclosed");
    let mut s = SessionState::default();
    for sql in [
        "CREATE FUNCTION f(x integer) RETURNS text AS x + 1",
        "CREATE FUNCTION f(x integer) RETURNS char(5) AS x",
        "CREATE FUNCTION f(x integer) RETURNS SETOF integer AS x",
        "CREATE FUNCTION f(x integer) RETURNS TABLE(a int) AS x",
        "CREATE FUNCTION f(x integer) RETURNS void AS x",
        "CREATE FUNCTION f(x) RETURNS integer AS x",
    ] {
        assert_eq!(code(&core, &mut s, sql), "0A000", "{sql}");
    }
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE FUNCTION f(x integer) RETURNS no_such_type AS x"
        ),
        "42704"
    );
}

#[test]
fn untyped_form_without_returns_is_unchanged() {
    let (core, _g) = new_core("sql66r-regress");
    let mut s = SessionState::default();
    define(&core, &mut s, "CREATE FUNCTION f(x) AS x + 1");
    assert_eq!(
        first_cell(&core, &mut s, "SELECT f(2) FROM docs LIMIT 1"),
        Cell::Float(3.0)
    );
    define(&core, &mut s, "CREATE FUNCTION g(x integer) AS x + 1");
}

#[test]
fn call_time_argument_type_checks_still_apply() {
    let (core, _g) = new_core("sql66r-args");
    let mut s = SessionState::default();
    define(
        &core,
        &mut s,
        "CREATE FUNCTION f(x integer) RETURNS integer AS x + 1",
    );
    assert_eq!(
        code(&core, &mut s, "SELECT f('abc') FROM docs LIMIT 1"),
        "22P02"
    );
    assert_eq!(
        code(&core, &mut s, "SELECT f(1.5) FROM docs LIMIT 1"),
        "42883"
    );
}

#[test]
fn nested_returns_call_has_static_type() {
    let (core, _g) = new_core("sql66r-nested");
    let mut s = SessionState::default();
    define(
        &core,
        &mut s,
        "CREATE FUNCTION f(x integer) RETURNS integer AS x + 1",
    );
    define(
        &core,
        &mut s,
        "CREATE FUNCTION g(y integer) RETURNS integer AS y * 2",
    );
    assert_eq!(
        first_cell(&core, &mut s, "SELECT g(f(1)) FROM docs LIMIT 1"),
        Cell::SignedInteger(4)
    );
    // 本体が RETURNS 付き UDF の呼び出しなら、その戻り値型で検査する。
    define(
        &core,
        &mut s,
        "CREATE FUNCTION h(x integer) RETURNS bigint AS f(x)",
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE FUNCTION k(x integer) RETURNS boolean AS f(x)"
        ),
        "42P13"
    );
}

/// SQL-66・#2798: DDL セッションでも非 DDL セッションと同じ検証（`42P13`）を通り、通った定義は
/// カタログへ永続化される（同名の再作成は `42723`）。検証に失敗した定義は何も書かない。
#[test]
fn ddl_session_validates_then_persists() {
    let (core, _g) = new_core("sql66r-ddl");
    let mut s = SessionState::default();
    s.allow_ddl();
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE FUNCTION f(x integer) RETURNS boolean AS x + 1"
        ),
        "42P13"
    );
    define(
        &core,
        &mut s,
        "CREATE FUNCTION f(x integer) RETURNS integer AS x + 1",
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE FUNCTION f(x integer) RETURNS integer AS x + 2"
        ),
        "42723"
    );
}

#[test]
fn returns_udf_respects_session_boundary() {
    let (core, _g) = new_core("sql66r-rls");
    let mut a = SessionState::default();
    define(
        &core,
        &mut a,
        "CREATE FUNCTION f(x integer) RETURNS integer AS x + 1",
    );
    let mut b = SessionState::default();
    assert_eq!(
        run_as(&core, "tenant-b", &mut b, "SELECT f(1) FROM docs LIMIT 1").expect_err("err"),
        "42883"
    );
}

#[test]
fn declared_return_type_propagates_to_explicit_cast() {
    let (core, _g) = new_core("sql66r-cast");
    let mut s = SessionState::default();
    define(&core, &mut s, "CREATE FUNCTION f() RETURNS integer AS 1");
    assert_eq!(
        first_cell(&core, &mut s, "SELECT f()::text FROM docs LIMIT 1"),
        Cell::Text("1".to_string())
    );
    assert_eq!(
        first_cell(&core, &mut s, "SELECT f()::bigint FROM docs LIMIT 1"),
        Cell::SignedInteger(1)
    );
}
