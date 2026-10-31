//! 集合返却の `LANGUAGE sql` 関数を `FROM` 句で使う経路（SQL-67・Issue #2220）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-67、RLS-10。設計記録は
//! `docs/design/plpgsql-subset.md`「Issue #2220 追記」。
//!
//! 実 `Storage` ＋ `CpuScalarProvider` の production 経路（`execute_sql_in_session`・
//! `execute_sql_in_txn`）で次を固定する。
//! - `RETURNS SETOF`／`RETURNS TABLE` の定義（`ROWS`・重複・未対応形の拒否）と再起動後の保持。
//! - `SELECT ... FROM f(...)` の行集合・列名・別名・外側の WHERE／ORDER BY／LIMIT／集計。
//! - 本体の DML が同じ Tx に載り、失敗・ロールバックで副作用が残らない。
//! - 上限（`--max-result-rows`）・STRICT・RLS・テナント境界・Describe が本体を実行しないこと。
//! - 未対応の呼び出し位置・形は固定の `0A000`（スカラー位置の集合返却関数を含む）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::result_limits::ResultLimits;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run(core: &EngineCore, s: &mut SessionState, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx("alice"), s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn err(core: &EngineCore, s: &mut SessionState, sql: &str) -> SqlSurfaceError {
    core.execute_sql_in_session(&ctx("alice"), s, sql)
        .expect_err(sql)
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> &'static str {
    err(core, s, sql).wire_code()
}

fn query(core: &EngineCore, s: &mut SessionState, sql: &str) -> engine::sql::exec::QueryResult {
    match run(core, s, sql) {
        SqlOutcome::Query(q) => q,
        other => panic!("{sql}: expected a query result, got {other:?}"),
    }
}

fn rows(core: &EngineCore, s: &mut SessionState, sql: &str) -> Vec<Vec<Cell>> {
    query(core, s, sql)
        .rows
        .into_iter()
        .map(|r| r.cells)
        .collect()
}

fn names(core: &EngineCore, s: &mut SessionState, sql: &str) -> Vec<String> {
    query(core, s, sql)
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Scalar { name, .. } | ColumnMeta::Computed { name, .. } => name.clone(),
            other => panic!("unexpected column meta {other:?}"),
        })
        .collect()
}

fn int(i: i64) -> Cell {
    Cell::SignedInteger(i)
}

fn text(t: &str) -> Cell {
    Cell::Text(t.to_string())
}

fn count(core: &EngineCore, s: &mut SessionState, table: &str) -> i64 {
    let r = rows(core, s, &format!("SELECT count(*) FROM {table}"));
    match r.first().and_then(|c| c.first()) {
        Some(Cell::SignedInteger(n)) => *n,
        Some(Cell::Integer(n)) => i64::try_from(*n).expect("count"),
        other => panic!("count: {other:?}"),
    }
}

fn setup(label: &str) -> (EngineCore, SessionState, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let core = open(&path);
    let mut s = session();
    run(&core, &mut s, "CREATE TABLE log (n INTEGER)");
    run(&core, &mut s, "CREATE TABLE t (a INTEGER, b TEXT)");
    run(
        &core,
        &mut s,
        "INSERT INTO t VALUES (1, 'x'), (2, 'y'), (3, 'z')",
    );
    (core, s, guard)
}

/// SQL-67（受け入れ条件）: `SELECT * FROM f(1)` が集合を返し、列名は関数名になる。
#[test]
fn setof_scalar_returns_the_row_set_with_the_function_name_as_column() {
    let (core, mut s, _g) = setup("sql67_setof_basic");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION f(lo int) RETURNS SETOF int LANGUAGE sql STABLE AS \
         $$ SELECT a FROM t WHERE a >= $1 ORDER BY a $$",
    );
    assert_eq!(
        rows(&core, &mut s, "SELECT * FROM f(2)"),
        vec![vec![int(2)], vec![int(3)]]
    );
    assert_eq!(names(&core, &mut s, "SELECT * FROM f(2)"), vec!["f"]);
    // 別名は列名にもなる。列名リストは改名する。
    assert_eq!(names(&core, &mut s, "SELECT * FROM f(2) AS x"), vec!["x"]);
    assert_eq!(
        names(&core, &mut s, "SELECT * FROM f(2) AS x(c)"),
        vec!["c"]
    );
    // 0 行は空集合。
    assert!(rows(&core, &mut s, "SELECT * FROM f(99)").is_empty());
}

#[test]
fn returns_table_uses_declared_names_and_supports_the_outer_query() {
    let (core, mut s, _g) = setup("sql67_setof_table");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION g(lo int) RETURNS TABLE (a int, b text) LANGUAGE sql STABLE AS \
         $$ SELECT a, b FROM t WHERE a >= $1 $$",
    );
    assert_eq!(names(&core, &mut s, "SELECT * FROM g(1)"), vec!["a", "b"]);
    assert_eq!(
        rows(&core, &mut s, "SELECT * FROM g(1) ORDER BY a DESC"),
        vec![
            vec![int(3), text("z")],
            vec![int(2), text("y")],
            vec![int(1), text("x")]
        ]
    );
    assert_eq!(
        rows(&core, &mut s, "SELECT b FROM g(1) WHERE a > 1 ORDER BY a"),
        vec![vec![text("y")], vec![text("z")]]
    );
    assert_eq!(
        rows(
            &core,
            &mut s,
            "SELECT a FROM g(1) ORDER BY a LIMIT 1 OFFSET 1"
        ),
        vec![vec![int(2)]]
    );
    // 修飾参照と列名リストによる改名。
    assert_eq!(
        rows(&core, &mut s, "SELECT q.a FROM g(3) AS q"),
        vec![vec![int(3)]]
    );
    assert_eq!(
        names(&core, &mut s, "SELECT * FROM g(1) AS q(c, d)"),
        vec!["c", "d"]
    );
    // 集計。
    assert_eq!(
        rows(&core, &mut s, "SELECT count(*) FROM g(1)"),
        vec![vec![Cell::Integer(3)]]
    );
    assert_eq!(
        rows(&core, &mut s, "SELECT sum(a) FROM g(2)"),
        rows(&core, &mut s, "SELECT sum(a) FROM t WHERE a >= 2")
    );
}

#[test]
fn definitions_validate_rows_names_and_unsupported_forms() {
    let (core, mut s, _g) = setup("sql67_setof_ddl");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION ok1(x int) RETURNS SETOF int LANGUAGE sql ROWS 5 AS $$ SELECT x $$",
    );
    for (sql, expected) in [
        // `ROWS` は集合返却だけ。
        (
            "CREATE FUNCTION b1(x int) RETURNS int LANGUAGE sql ROWS 5 AS $$ SELECT x $$",
            "22000",
        ),
        // 列名の重複・入力引数名との衝突。
        (
            "CREATE FUNCTION b2(x int) RETURNS TABLE (a int, a int) LANGUAGE sql AS $$ SELECT 1, 2 $$",
            "42P13",
        ),
        (
            "CREATE FUNCTION b3(x int) RETURNS TABLE (x int) LANGUAGE sql AS $$ SELECT 1 $$",
            "42P13",
        ),
        // PL/pgSQL・宣言的・void・record は未対応。
        (
            "CREATE FUNCTION b4(x int) RETURNS SETOF int LANGUAGE plpgsql AS \
             $$ BEGIN RETURN NEXT 1; END $$",
            "0A000",
        ),
        ("CREATE FUNCTION b5(x int) RETURNS SETOF int AS x", "0A000"),
        (
            "CREATE FUNCTION b6(x int) RETURNS void LANGUAGE sql AS $$ SELECT 1 $$",
            "0A000",
        ),
        (
            "CREATE FUNCTION b7(x int) RETURNS SETOF record LANGUAGE sql AS $$ SELECT 1 $$",
            "0A000",
        ),
        // 戻り値の形の入れ替えは `OR REPLACE` でも不可。
        (
            "CREATE OR REPLACE FUNCTION ok1(x int) RETURNS int LANGUAGE sql AS $$ SELECT x $$",
            "42P13",
        ),
    ] {
        assert_eq!(code(&core, &mut s, sql), expected, "{sql}");
    }
}

/// 列数・型が宣言と合わない最後の文は `42P13`。
#[test]
fn result_shape_mismatch_is_42p13() {
    let (core, mut s, _g) = setup("sql67_setof_shape");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION two() RETURNS SETOF int LANGUAGE sql AS $$ SELECT a, b FROM t $$",
    );
    assert_eq!(code(&core, &mut s, "SELECT * FROM two()"), "42P13");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION wrong() RETURNS SETOF int LANGUAGE sql AS $$ SELECT b FROM t $$",
    );
    assert_eq!(code(&core, &mut s, "SELECT * FROM wrong()"), "42P13");
}

/// 0 行の結果でも宣言型との不一致は `42P13`（空集合として成功させない）。
#[test]
fn empty_result_with_type_mismatch_is_42p13() {
    let (core, mut s, _g) = setup("sql67_setof_empty_mismatch");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION e() RETURNS SETOF int LANGUAGE sql AS \
         $$ SELECT b FROM t WHERE 1 = 0 $$",
    );
    assert_eq!(code(&core, &mut s, "SELECT * FROM e()"), "42P13");
}

/// スキーマ修飾の呼び出しでも既定の列名は関数名で、内部キー（`#<id>.`）を出さない。
#[test]
fn schema_qualified_call_does_not_leak_internal_key() {
    let (core, mut s, _g) = setup("sql67_setof_schema_name");
    run(&core, &mut s, "CREATE SCHEMA sc");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION sc.g() RETURNS SETOF int LANGUAGE sql AS $$ SELECT a FROM t $$",
    );
    for n in names(&core, &mut s, "SELECT * FROM sc.g()") {
        assert!(!n.contains('#'), "leaked: {n}");
        assert_eq!(n, "g");
    }
}

#[test]
fn strict_function_with_null_argument_returns_an_empty_set() {
    let (core, mut s, _g) = setup("sql67_setof_strict");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION sf(x int) RETURNS SETOF int LANGUAGE sql STRICT AS \
         $$ INSERT INTO log VALUES (1); SELECT x $$",
    );
    assert!(rows(&core, &mut s, "SELECT * FROM sf(NULL)").is_empty());
    assert_eq!(count(&core, &mut s, "log"), 0);
    assert_eq!(
        rows(&core, &mut s, "SELECT * FROM sf(7)"),
        vec![vec![int(7)]]
    );
    assert_eq!(count(&core, &mut s, "log"), 1);
}

/// 本体の DML（`RETURNING` 付き）は外側と同じ write txn に載り、失敗時は副作用が残らない。
#[test]
fn volatile_body_writes_in_the_same_transaction() {
    let (core, mut s, _g) = setup("sql67_setof_dml");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION ins(x int) RETURNS SETOF int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (x) RETURNING n $$",
    );
    assert_eq!(
        rows(&core, &mut s, "SELECT * FROM ins(4)"),
        vec![vec![int(4)]]
    );
    assert_eq!(count(&core, &mut s, "log"), 1);
    run(
        &core,
        &mut s,
        "CREATE FUNCTION bad(x int) RETURNS SETOF int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (x); SELECT 1 / 0 $$",
    );
    assert_eq!(code(&core, &mut s, "SELECT * FROM bad(5)"), "22012");
    assert_eq!(count(&core, &mut s, "log"), 1);
    // STABLE の本体の書き込みは拒否され、副作用は残らない。
    run(
        &core,
        &mut s,
        "CREATE FUNCTION st(x int) RETURNS SETOF int LANGUAGE sql STABLE AS \
         $$ INSERT INTO log VALUES (x) RETURNING n $$",
    );
    assert_eq!(code(&core, &mut s, "SELECT * FROM st(6)"), "0A000");
    assert_eq!(count(&core, &mut s, "log"), 1);
}

#[test]
fn explicit_transaction_rolls_back_the_body_writes() {
    let (core, mut s, _g) = setup("sql67_setof_txn");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION ins(x int) RETURNS SETOF int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (x) RETURNING n $$",
    );
    let c = ctx("alice");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT * FROM ins(1)")
        .expect("ins");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(count(&core, &mut s, "log"), 0);
    // 失敗した呼び出しは Tx を Failed にする。
    run(
        &core,
        &mut s,
        "CREATE FUNCTION bad() RETURNS SETOF int LANGUAGE sql AS $$ SELECT 1 / 0 $$",
    );
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT * FROM bad()")
        .expect_err("bad");
    assert_eq!(e.wire_code(), "22012");
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT 1")
        .expect_err("failed");
    assert_eq!(e.wire_code(), "25P02");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
}

/// 返却行数は `--max-result-rows` に従い、超えたら黙って切り詰めず `54000`。
#[test]
fn result_row_limit_is_enforced_without_truncation() {
    let path = unique_db_path("sql67_setof_limit");
    let _g = CleanupGuard(path.clone());
    let core = open(&path).with_result_limits(ResultLimits::new(2));
    let mut s = session();
    run(&core, &mut s, "CREATE TABLE t (a INTEGER)");
    run(&core, &mut s, "INSERT INTO t VALUES (1), (2), (3)");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION many() RETURNS SETOF int LANGUAGE sql STABLE AS \
         $$ SELECT a FROM t $$",
    );
    run(
        &core,
        &mut s,
        "CREATE FUNCTION few() RETURNS SETOF int LANGUAGE sql STABLE AS \
         $$ SELECT a FROM t LIMIT 2 $$",
    );
    assert_eq!(code(&core, &mut s, "SELECT * FROM many()"), "54000");
    assert_eq!(rows(&core, &mut s, "SELECT * FROM few()").len(), 2);
}

/// RLS・テナント境界: 他テナントの関数は不在と同じ `42883`、本体は呼び出し元の可視行だけを読む。
#[test]
fn tenant_boundary_hides_other_tenants_functions() {
    let (core, mut s, _g) = setup("sql67_setof_tenant");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION f() RETURNS SETOF int LANGUAGE sql STABLE AS $$ SELECT a FROM t $$",
    );
    let mut bob = session();
    let e = core
        .execute_sql_in_session(&ctx("bob"), &mut bob, "SELECT * FROM f()")
        .expect_err("bob");
    assert_eq!(e.wire_code(), "42883");
    let missing = core
        .execute_sql_in_session(&ctx("bob"), &mut bob, "SELECT * FROM nosuch()")
        .expect_err("missing");
    assert_eq!(missing.wire_code(), "42883");
    assert_eq!(format!("{e}"), format!("{missing}"));
}

/// 再起動（再オープン）後も `SETOF`／`TABLE` の定義を呼べる。
#[test]
fn definitions_survive_reopen() {
    let path = unique_db_path("sql67_setof_reopen");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut s = session();
        run(&core, &mut s, "CREATE TABLE t (a INTEGER)");
        run(&core, &mut s, "INSERT INTO t VALUES (1), (2)");
        run(
            &core,
            &mut s,
            "CREATE FUNCTION a1() RETURNS SETOF int LANGUAGE sql STABLE ROWS 7 AS $$ SELECT a FROM t $$",
        );
        run(
            &core,
            &mut s,
            "CREATE FUNCTION a2() RETURNS TABLE (v int) LANGUAGE sql STABLE AS \
             $$ SELECT a FROM t $$",
        );
        run(
            &core,
            &mut s,
            "CREATE FUNCTION sc() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
        );
    }
    let core = open(&path);
    let mut s = session();
    core.load_session_udfs(&ctx("alice"), &mut s)
        .expect("load session udfs");
    assert_eq!(rows(&core, &mut s, "SELECT * FROM a1()").len(), 2);
    assert_eq!(names(&core, &mut s, "SELECT * FROM a2()"), vec!["v"]);
    assert_eq!(rows(&core, &mut s, "SELECT sc()"), vec![vec![int(1)]]);
}

/// 未対応の形は固定の `0A000`（スカラー位置の集合返却関数は最初の 1 行を返さず拒否する）。
#[test]
fn unsupported_forms_are_rejected_fail_closed() {
    let (core, mut s, _g) = setup("sql67_setof_reject");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION f(x int) RETURNS SETOF int LANGUAGE sql STABLE AS $$ SELECT x $$",
    );
    run(
        &core,
        &mut s,
        "CREATE FUNCTION sc(x int) RETURNS int LANGUAGE sql STABLE AS $$ SELECT x $$",
    );
    run(
        &core,
        &mut s,
        "CREATE FUNCTION dc(x int) RETURNS int AS x + 1",
    );
    for sql in [
        "SELECT f(1)",
        "SELECT * FROM f(1), t",
        "SELECT * FROM f(1) JOIN t ON true",
        "SELECT * FROM f(1) WITH ORDINALITY",
        "SELECT * FROM ROWS FROM (f(1))",
        "SELECT * FROM sc(1)",
        "SELECT * FROM dc(1)",
        "SELECT * FROM f(sc(1))",
    ] {
        assert_eq!(code(&core, &mut s, sql), "0A000", "{sql}");
    }
    // 先頭のリレーションが関数でない複数リレーションの形は従来どおり構文誤り。
    assert_eq!(code(&core, &mut s, "SELECT * FROM t, f(1)"), "42601");
    // 組み込みの集合返却関数は未対応（この実装に無い関数名は不在と同じ `42883`）。
    assert_eq!(
        code(&core, &mut s, "SELECT * FROM generate_series(1, 3)"),
        "42883"
    );
    // 引数の個数不一致は不在と同じ `42883`、列参照の引数は定数式として拒否される（`22000`）。
    assert_eq!(code(&core, &mut s, "SELECT * FROM f()"), "42883");
    assert_eq!(code(&core, &mut s, "SELECT * FROM f(a)"), "22000");
}

/// Describe は宣言列を公告し、本体を実行しない。
#[test]
fn describe_advertises_declared_columns_without_running_the_body() {
    let (core, mut s, _g) = setup("sql67_setof_describe");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION w(x int) RETURNS TABLE (n bigint, label text) LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (x); SELECT 1, 'a' $$",
    );
    let parsed = core.parse_sql("SELECT * FROM w(1)").expect("parse");
    let cols = core
        .describe_parsed_in_session(&s, &parsed)
        .expect("describe")
        .expect("columns");
    assert_eq!(cols.len(), 2);
    assert!(
        matches!(
            cols.first(),
            Some(ColumnMeta::Scalar {
                ty: engine::catalog::ColumnType::BigInt,
                ..
            })
        ),
        "{cols:?}"
    );
    assert_eq!(count(&core, &mut s, "log"), 0);
}

#[test]
fn parameter_arithmetic_in_the_body_and_multi_column_row() {
    let (core, mut s, _g) = setup("sql67_setof_union");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION pair(x int) RETURNS TABLE (n int, label text) LANGUAGE sql STABLE \
         AS $$ SELECT 1, 'a' $$",
    );
    run(
        &core,
        &mut s,
        "CREATE FUNCTION one_col(x int) RETURNS SETOF int LANGUAGE sql STABLE \
         AS $$ SELECT $1 + 1 $$",
    );
    assert_eq!(
        rows(&core, &mut s, "SELECT * FROM one_col(4)"),
        vec![vec![int(5)]]
    );
    assert_eq!(
        rows(
            &core,
            &mut s,
            "SELECT n, label FROM pair(0) ORDER BY n DESC"
        ),
        vec![vec![int(1), text("a")]]
    );
}

/// 拡張クエリの `$n` を FROM 関数の引数に置く形は未対応（Parse で `42601` の fail-closed。後続の課題）。
#[test]
fn placeholder_arguments_are_fail_closed() {
    let (core, mut s, _g) = setup("sql67_setof_param");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION f(x int) RETURNS SETOF int LANGUAGE sql STABLE AS $$ SELECT x $$",
    );
    let e = core
        .parse_sql("SELECT * FROM f($1)")
        .expect_err("placeholder");
    assert_eq!(e.wire_code(), "42601");
}

/// 列名リストが列数より多いときは構文誤り（`42601`）として拒否する。
#[test]
fn column_alias_list_longer_than_the_columns_is_rejected() {
    let (core, mut s, _g) = setup("sql67_setof_alias_long");
    run(
        &core,
        &mut s,
        "CREATE FUNCTION f(x int) RETURNS SETOF int LANGUAGE sql STABLE AS $$ SELECT x $$",
    );
    // PostgreSQL は `42P10`。既存の改名処理（`plan_outer`）の `42601` のまま fail-closed（既知の差）。
    assert_eq!(
        code(&core, &mut s, "SELECT * FROM f(1) AS x(a, b)"),
        "42601"
    );
}
