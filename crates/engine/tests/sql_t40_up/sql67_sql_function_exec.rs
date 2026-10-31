//! `LANGUAGE sql` 関数の本体の実行（SQL-67 (5)・Issue #2218）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-67 (2)(3)(5)(6)(7)、SQL-68、RLS-10。
//! 設計記録は `docs/design/plpgsql-subset.md`「Issue #2218 追記」。
//!
//! 実 `Storage` ＋ `CpuScalarProvider` の production 経路（`execute_sql_in_session`・
//! `execute_sql_in_txn`）で次を固定する。
//! - スカラー関数の結果（名前参照・`$n` 参照・最後の文の結果・0 行は NULL・暗黙変換・STRICT）。
//! - 本体の DML が呼び出し元と同じ Tx・RLS で実行され、失敗時は副作用が残らない。
//! - 揮発性（`IMMUTABLE`／`STABLE` の書き込みは `0A000`）・`42P13`・本体で拒否する文。
//! - 上限（呼び出し深さ `54001`・文数 `54000`）と `Ledgered` 構成の `23502`（いずれも副作用ゼロ）。
//! - 未対応の呼び出し位置は `0A000`（副作用なし）。テナント境界（他テナントの関数・行が見えない）。
//! - 拡張クエリの Describe は戻り値型を公告し、本体を実行しない。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::plpgsql_limits::PlpgsqlLimits;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
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

fn open(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn open_with(
    path: &std::path::Path,
    limits: Option<PlpgsqlLimits>,
    mode: LedgerMode,
) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    let mut core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_ledger_mode(mode);
    if let Some(l) = limits {
        core = core.with_plpgsql_limits(l);
    }
    core
}

/// DDL 権限つきのセッション（`CREATE FUNCTION` 後に同じセッションで呼べる）。
fn session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn run(core: &EngineCore, s: &mut SessionState, tenant: &str, sql: &str) -> SqlOutcome {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn err(core: &EngineCore, s: &mut SessionState, tenant: &str, sql: &str) -> SqlSurfaceError {
    core.execute_sql_in_session(&ctx(tenant), s, sql)
        .expect_err(sql)
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> &'static str {
    err(core, s, "alice", sql).wire_code()
}

fn rows(core: &EngineCore, s: &mut SessionState, tenant: &str, sql: &str) -> Vec<Vec<Cell>> {
    match run(core, s, tenant, sql) {
        SqlOutcome::Query(q) => q.rows.into_iter().map(|r| r.cells).collect(),
        other => panic!("{sql}: expected a query result, got {other:?}"),
    }
}

fn one(core: &EngineCore, s: &mut SessionState, sql: &str) -> Cell {
    let r = rows(core, s, "alice", sql);
    assert_eq!(r.len(), 1, "{sql}");
    r.into_iter()
        .next()
        .and_then(|c| c.into_iter().next())
        .expect("cell")
}

fn int(i: i64) -> Cell {
    Cell::SignedInteger(i)
}

fn count(core: &EngineCore, s: &mut SessionState, tenant: &str, table: &str) -> i64 {
    let r = rows(core, s, tenant, &format!("SELECT count(*) FROM {table}"));
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
    run(&core, &mut s, "alice", "CREATE TABLE log (n INTEGER)");
    run(&core, &mut s, "alice", "CREATE TABLE t (a INTEGER, b TEXT)");
    (core, s, guard)
}

#[test]
fn scalar_functions_use_names_and_positional_parameters() {
    let (core, mut s, _g) = setup("sql67_exec_scalar");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION add(a int, b int) RETURNS int LANGUAGE sql AS $$ SELECT a + $2 $$",
    );
    assert_eq!(one(&core, &mut s, "SELECT add(1, 2)"), int(3));
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION shout(t text) RETURNS text LANGUAGE sql AS $$ SELECT upper($1) $$",
    );
    assert_eq!(
        one(&core, &mut s, "SELECT shout('ab')"),
        Cell::Text("AB".to_string())
    );
    // 入れ子と引数の評価順（引数が先）。
    assert_eq!(
        one(&core, &mut s, "SELECT add(add(1, 2), add(3, 4))"),
        int(10)
    );
    // 暗黙変換: int の結果を bigint で返す。
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION widen(a int) RETURNS bigint LANGUAGE sql AS $$ SELECT a $$",
    );
    assert_eq!(one(&core, &mut s, "SELECT widen(7)"), int(7));
}

#[test]
fn last_statement_is_the_result_and_zero_rows_is_null() {
    let (core, mut s, _g) = setup("sql67_exec_last");
    run(
        &core,
        &mut s,
        "alice",
        "INSERT INTO t VALUES (1, 'x'), (2, 'y'), (3, 'z')",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION firstb() RETURNS text LANGUAGE sql AS $$ SELECT 1; SELECT b FROM t ORDER BY a $$",
    );
    assert_eq!(
        one(&core, &mut s, "SELECT firstb()"),
        Cell::Text("x".to_string())
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION nob() RETURNS text LANGUAGE sql AS $$ SELECT b FROM t WHERE a = 99 $$",
    );
    assert_eq!(one(&core, &mut s, "SELECT nob()"), Cell::Null);
}

#[test]
fn dml_body_shares_the_transaction_and_rolls_back_on_failure() {
    let (core, mut s, _g) = setup("sql67_exec_dml");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION ins(x int) RETURNS bigint LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (x); SELECT count(*) FROM log $$",
    );
    // 本体の INSERT の後の SELECT が未 commit の行を読む。
    assert_eq!(one(&core, &mut s, "SELECT ins(1)"), int(1));
    assert_eq!(one(&core, &mut s, "SELECT ins(2)"), int(2));
    assert_eq!(count(&core, &mut s, "alice", "log"), 2);
    // 失敗する関数（0 除算）は書き込みごと取り消される。
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION bad(x int) RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (x); SELECT 1 / 0 $$",
    );
    assert_eq!(code(&core, &mut s, "SELECT bad(9)"), "22012");
    assert_eq!(count(&core, &mut s, "alice", "log"), 2);
    // INSERT ... RETURNING を最後の文にできる。
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION insr(x int) RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (x) RETURNING n $$",
    );
    assert_eq!(one(&core, &mut s, "SELECT insr(5)"), int(5));
    assert_eq!(count(&core, &mut s, "alice", "log"), 3);
}

#[test]
fn explicit_transaction_failure_fails_the_transaction() {
    let (core, mut s, _g) = setup("sql67_exec_txn");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION bad(x int) RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (x); SELECT 1 / 0 $$",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION ins(x int) RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (x) RETURNING n $$",
    );
    let c = ctx("alice");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT ins(1)")
        .expect("ins");
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT bad(2)")
        .expect_err("bad");
    assert_eq!(e.wire_code(), "22012");
    let e = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, "SELECT 1")
        .expect_err("failed");
    assert_eq!(e.wire_code(), "25P02");
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(count(&core, &mut s, "alice", "log"), 0);
}

#[test]
fn strict_functions_skip_the_body_on_null_input() {
    let (core, mut s, _g) = setup("sql67_exec_strict");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION sf(x int) RETURNS int LANGUAGE sql STRICT AS \
         $$ INSERT INTO log VALUES (1); SELECT x $$",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION nf(x int) RETURNS int LANGUAGE sql CALLED ON NULL INPUT AS \
         $$ INSERT INTO log VALUES (1); SELECT 7 $$",
    );
    assert_eq!(
        one(&core, &mut s, "SELECT sf(CAST(NULL AS int))"),
        Cell::Null
    );
    assert_eq!(count(&core, &mut s, "alice", "log"), 0);
    assert_eq!(one(&core, &mut s, "SELECT nf(CAST(NULL AS int))"), int(7));
    assert_eq!(count(&core, &mut s, "alice", "log"), 1);
}

#[test]
fn non_volatile_functions_cannot_write() {
    let (core, mut s, _g) = setup("sql67_exec_volatility");
    for kind in ["IMMUTABLE", "STABLE"] {
        run(
            &core,
            &mut s,
            "alice",
            &format!(
                "CREATE FUNCTION w_{kind}() RETURNS int LANGUAGE sql {kind} AS \
                 $$ INSERT INTO log VALUES (1) RETURNING n $$"
            ),
        );
        assert_eq!(code(&core, &mut s, &format!("SELECT w_{kind}()")), "0A000");
    }
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION w_v() RETURNS int LANGUAGE sql VOLATILE AS \
         $$ INSERT INTO log VALUES (1) RETURNING n $$",
    );
    assert_eq!(one(&core, &mut s, "SELECT w_v()"), int(1));
    assert_eq!(count(&core, &mut s, "alice", "log"), 1);
}

#[test]
fn definition_time_shape_errors() {
    let (core, mut s, _g) = setup("sql67_exec_define");
    let create =
        |body: &str| format!("CREATE FUNCTION f(a int) RETURNS int LANGUAGE sql AS $${body}$$");
    // 最後の文が RETURNING の無い DML。
    assert_eq!(
        code(&core, &mut s, &create(" INSERT INTO log VALUES (a) ")),
        "42P13"
    );
    // 空の本体。
    assert_eq!(code(&core, &mut s, &create("  ")), "42P13");
    // トランザクション制御・DDL・SET・OPERATION_ID は 0A000。
    for body in [
        "COMMIT",
        "BEGIN; SELECT 1",
        "SELECT 1; CREATE TABLE zz (a int)",
        "SET search_mode = 'recall'; SELECT 1",
        "INSERT INTO log VALUES (a) USING OPERATION_ID 'x1'; SELECT 1",
    ] {
        let c = code(&core, &mut s, &create(body));
        assert!(c == "0A000" || c == "42601", "{body}: {c}");
    }
    assert_eq!(code(&core, &mut s, &create("COMMIT")), "0A000");
    assert_eq!(code(&core, &mut s, &create("SELECT (1")), "42601");
    // 失敗した定義は呼び出せる関数を残さない。
    assert_eq!(code(&core, &mut s, "SELECT f(1)"), "42883");
}

#[test]
fn result_shape_errors_at_call_time() {
    let (core, mut s, _g) = setup("sql67_exec_shape");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION two() RETURNS int LANGUAGE sql AS $$ SELECT 1, 2 $$",
    );
    assert_eq!(code(&core, &mut s, "SELECT two()"), "42P13");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION mism() RETURNS int LANGUAGE sql AS $$ SELECT 'abc'::text $$",
    );
    assert_eq!(code(&core, &mut s, "SELECT mism()"), "42P13");
}

#[test]
fn recursion_hits_the_call_depth_limit_with_no_side_effects() {
    let (core, mut s, _g) = setup("sql67_exec_depth");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION rec(n int) RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (n); SELECT rec(n + 1) $$",
    );
    assert_eq!(code(&core, &mut s, "SELECT rec(1)"), "54001");
    assert_eq!(count(&core, &mut s, "alice", "log"), 0);
}

/// 呼び出し深さの上限ちょうどは成功し、+1 で `54001`（いずれも副作用ゼロ）。
#[test]
fn call_depth_limit_is_exact() {
    let path = unique_db_path("sql67_exec_depth_exact");
    let _g = CleanupGuard(path.clone());
    let limits = PlpgsqlLimits::new(100_000, 100_000, 32, 100_000, 3);
    let core = open_with(&path, Some(limits), LedgerMode::default());
    let mut s = session();
    run(&core, &mut s, "alice", "CREATE TABLE log (n INTEGER)");
    for (name, body) in [
        ("c4", "INSERT INTO log VALUES (4); SELECT 4"),
        ("c3", "INSERT INTO log VALUES (3); SELECT c4()"),
        ("c2", "INSERT INTO log VALUES (2); SELECT c3()"),
        ("c1", "INSERT INTO log VALUES (1); SELECT c2()"),
    ] {
        run(
            &core,
            &mut s,
            "alice",
            &format!("CREATE FUNCTION {name}() RETURNS int LANGUAGE sql AS $${body}$$"),
        );
    }
    // c3 -> c4 は深さ 2、c2 -> c3 -> c4 は深さ 3（ちょうど）で成功する。
    assert_eq!(one(&core, &mut s, "SELECT c3()"), int(4));
    assert_eq!(one(&core, &mut s, "SELECT c2()"), int(4));
    assert_eq!(count(&core, &mut s, "alice", "log"), 5);
    // c1 -> c2 -> c3 -> c4 は深さ 4 で 54001、副作用はゼロ。
    assert_eq!(code(&core, &mut s, "SELECT c1()"), "54001");
    assert_eq!(count(&core, &mut s, "alice", "log"), 5);
}

/// 文数の上限（`--max-plpgsql-statements`）は LANGUAGE sql にも効き、入れ子の呼び出しを合算する。
#[test]
fn statement_limit_is_shared_and_exact() {
    let path = unique_db_path("sql67_exec_stmts");
    let _g = CleanupGuard(path.clone());
    let limits = PlpgsqlLimits::new(3, 100_000, 32, 100_000, 32);
    let core = open_with(&path, Some(limits), LedgerMode::default());
    let mut s = session();
    run(&core, &mut s, "alice", "CREATE TABLE log (n INTEGER)");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION s3() RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (1); INSERT INTO log VALUES (2); SELECT 3 $$",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION s4() RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (1); INSERT INTO log VALUES (2); \
            INSERT INTO log VALUES (3); SELECT 4 $$",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION nest() RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (1); SELECT s3() $$",
    );
    assert_eq!(one(&core, &mut s, "SELECT s3()"), int(3));
    assert_eq!(count(&core, &mut s, "alice", "log"), 2);
    assert_eq!(code(&core, &mut s, "SELECT s4()"), "54000");
    assert_eq!(count(&core, &mut s, "alice", "log"), 2);
    // 入れ子の呼び出しの文数も合算される（外側 1 + 内側 3 = 4 > 3）。
    assert_eq!(code(&core, &mut s, "SELECT nest()"), "54000");
    assert_eq!(count(&core, &mut s, "alice", "log"), 2);
}

/// 台帳必須（`Ledgered`）の構成では、operation_id を持たない `SELECT` から本体が書き込む時点で
/// `23502`、読み取りだけの関数は成功する。
#[test]
fn ledgered_configuration_rejects_writes_from_a_select() {
    let path = unique_db_path("sql67_exec_ledgered");
    let _g = CleanupGuard(path.clone());
    let core = open_with(&path, None, LedgerMode::Ledgered);
    let mut s = session();
    run(&core, &mut s, "alice", "CREATE TABLE log (n INTEGER)");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION w() RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (1) RETURNING n $$",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION r() RETURNS int LANGUAGE sql AS $$ SELECT 5 $$",
    );
    assert_eq!(code(&core, &mut s, "SELECT w()"), "23502");
    assert_eq!(count(&core, &mut s, "alice", "log"), 0);
    assert_eq!(one(&core, &mut s, "SELECT r()"), int(5));
}

/// 未対応の呼び出し位置は `0A000` で、本体は実行されない（副作用なし）。
#[test]
fn unsupported_call_positions_are_rejected_without_side_effects() {
    let (core, mut s, _g) = setup("sql67_exec_positions");
    run(&core, &mut s, "alice", "INSERT INTO t VALUES (1, 'x')");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION w(x int) RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (x) RETURNING n $$",
    );
    for sql in [
        "SELECT w(a) FROM t",
        "SELECT a FROM t WHERE w(a) = 1",
        "SELECT CASE WHEN 1 = 1 THEN 1 ELSE w(1) END",
        "SELECT coalesce(1, w(1))",
        "SELECT w(1) LIMIT 1",
        "SELECT count(w(1))",
    ] {
        assert_eq!(code(&core, &mut s, sql), "0A000", "{sql}");
    }
    // INSERT の VALUES は従来どおり定数式だけを受理する（構文段の拒否）。
    assert_eq!(
        code(&core, &mut s, "INSERT INTO t VALUES (w(1), 'y')"),
        "42601"
    );
    assert_eq!(count(&core, &mut s, "alice", "log"), 0);
    assert_eq!(count(&core, &mut s, "alice", "t"), 1);
    // `RETURNS trigger` の関数を直接呼ぶのは 0A000。
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS \
         $$ BEGIN RETURN NEW; END $$",
    );
    assert_eq!(code(&core, &mut s, "SELECT trg()"), "0A000");
}

/// PL/pgSQL 関数も同じ呼び出し位置から呼べ、LANGUAGE sql との相互呼び出しができる。
#[test]
fn plpgsql_and_sql_functions_call_each_other() {
    let (core, mut s, _g) = setup("sql67_exec_mixed");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION dbl(a int) RETURNS int LANGUAGE sql AS $$ SELECT a * 2 $$",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION viapl(a int) RETURNS int LANGUAGE plpgsql AS \
         $$ DECLARE r int; BEGIN SELECT dbl(a) + 1 INTO r; RETURN r; END $$",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION top(a int) RETURNS int LANGUAGE sql AS $$ SELECT viapl(a) $$",
    );
    assert_eq!(one(&core, &mut s, "SELECT viapl(4)"), int(9));
    assert_eq!(one(&core, &mut s, "SELECT top(4)"), int(9));
}

/// テナント境界: 本体の `SELECT`／`UPDATE` は呼び出し元テナントの行だけを見て、他テナントの関数は
/// 呼べない（不在と同じ `42883`）。
#[test]
fn tenant_boundary_is_preserved() {
    let (core, mut s, _g) = setup("sql67_exec_tenant");
    let mut sb = session();
    run(
        &core,
        &mut s,
        "alice",
        "INSERT INTO t VALUES (1, 'alice-row')",
    );
    run(&core, &mut sb, "bob", "INSERT INTO t VALUES (1, 'bob-row')");
    for (sess, tenant) in [(&mut s, "alice"), (&mut sb, "bob")] {
        run(
            &core,
            sess,
            tenant,
            "CREATE FUNCTION who() RETURNS text LANGUAGE sql AS $$ SELECT b FROM t $$",
        );
        run(
            &core,
            sess,
            tenant,
            "CREATE FUNCTION touch() RETURNS int LANGUAGE sql AS \
             $$ UPDATE t SET b = 'touched' RETURNING a $$",
        );
    }
    assert_eq!(
        rows(&core, &mut s, "alice", "SELECT who()"),
        vec![vec![Cell::Text("alice-row".to_string())]]
    );
    assert_eq!(
        rows(&core, &mut sb, "bob", "SELECT who()"),
        vec![vec![Cell::Text("bob-row".to_string())]]
    );
    run(&core, &mut s, "alice", "SELECT touch()");
    assert_eq!(
        rows(&core, &mut sb, "bob", "SELECT b FROM t"),
        vec![vec![Cell::Text("bob-row".to_string())]]
    );
    // 他テナントだけが持つ関数は、自テナントのセッションから呼べない。
    run(
        &core,
        &mut sb,
        "bob",
        "CREATE FUNCTION only_bob() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
    );
    let mut s2 = session();
    core.load_session_udfs(&ctx("alice"), &mut s2)
        .expect("load");
    let e = err(&core, &mut s2, "alice", "SELECT only_bob()");
    assert_eq!(e.wire_code(), "42883");
    assert!(!e.to_string().contains("SELECT 1"));
}

/// 別セッション（接続）で作った関数は、ハンドシェイクのロードで見える。
#[test]
fn functions_are_loaded_from_the_catalog_for_new_sessions() {
    let (core, mut s, _g) = setup("sql67_exec_load");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION add(a int, b int) RETURNS int LANGUAGE sql AS $$ SELECT a + b $$",
    );
    let mut s2 = SessionState::default();
    core.load_session_udfs(&ctx("alice"), &mut s2)
        .expect("load");
    assert_eq!(one(&core, &mut s2, "SELECT add(2, 3)"), int(5));
    // DROP 後は呼べない（同じセッションでも）。
    run(&core, &mut s, "alice", "DROP FUNCTION add");
    assert_eq!(code(&core, &mut s, "SELECT add(2, 3)"), "42883");
}

/// Describe（拡張クエリ）は戻り値型を公告するだけで本体を実行しない。
#[test]
fn describe_advertises_the_return_type_without_running_the_body() {
    let (core, mut s, _g) = setup("sql67_exec_describe");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION w(x int) RETURNS bigint LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (x); SELECT x $$",
    );
    let parsed = core.parse_sql("SELECT w(1)").expect("parse");
    let cols = core
        .describe_parsed_in_session(&s, &parsed)
        .expect("describe")
        .expect("columns");
    assert_eq!(cols.len(), 1);
    assert!(
        matches!(
            cols.first(),
            Some(engine::sql::exec::ColumnMeta::Computed {
                ty: Some(engine::catalog::ColumnType::BigInt),
                ..
            })
        ),
        "{cols:?}"
    );
    assert_eq!(count(&core, &mut s, "alice", "log"), 0);
}

/// 戻り値型ごとの往復（numeric は scale を保ち、date・bool・text・bigint も型付きで返る）。
#[test]
fn return_types_round_trip_through_the_call_position() {
    let (core, mut s, _g) = setup("sql67_exec_types");
    for (name, ret, body) in [
        ("rn", "numeric", "SELECT 1.50"),
        ("rni", "numeric", "SELECT 3"),
        ("rd", "date", "SELECT DATE '2024-02-29'"),
        ("rb", "boolean", "SELECT CAST('t' AS boolean)"),
        ("rt", "text", "SELECT 'it''s'"),
        ("rf", "double precision", "SELECT 2.5"),
    ] {
        run(
            &core,
            &mut s,
            "alice",
            &format!("CREATE FUNCTION {name}() RETURNS {ret} LANGUAGE sql AS $${body}$$"),
        );
    }
    assert_eq!(
        one(&core, &mut s, "SELECT rn()").clone_numeric_text(),
        "1.50"
    );
    assert_eq!(one(&core, &mut s, "SELECT rni()").clone_numeric_text(), "3");
    assert!(matches!(one(&core, &mut s, "SELECT rd()"), Cell::Date(_)));
    assert_eq!(one(&core, &mut s, "SELECT rb()"), Cell::Bool(true));
    assert_eq!(
        one(&core, &mut s, "SELECT rt()"),
        Cell::Text("it's".to_string())
    );
    assert_eq!(one(&core, &mut s, "SELECT rf()"), Cell::Float(2.5));
    // 関数の結果を別の関数・演算の引数にできる。
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION idn(x numeric) RETURNS numeric LANGUAGE sql AS $$ SELECT x $$",
    );
    assert_eq!(
        one(&core, &mut s, "SELECT idn(rn())").clone_numeric_text(),
        "1.50"
    );
}

trait NumericText {
    fn clone_numeric_text(self) -> String;
}

impl NumericText for Cell {
    fn clone_numeric_text(self) -> String {
        match self {
            Cell::Numeric(d) => d.to_string(),
            other => panic!("expected numeric, got {other:?}"),
        }
    }
}

/// `WHERE` と投影の両方で呼べる。`WHERE` が偽・NULL のときは投影の関数を起動しない。
#[test]
fn where_clause_calls_gate_the_projection() {
    let (core, mut s, _g) = setup("sql67_exec_where");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION yes() RETURNS boolean LANGUAGE sql AS $$ SELECT CAST('t' AS boolean) $$",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION no() RETURNS boolean LANGUAGE sql AS $$ SELECT CAST('f' AS boolean) $$",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION mark() RETURNS int LANGUAGE sql AS \
         $$ INSERT INTO log VALUES (1) RETURNING n $$",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION two() RETURNS int LANGUAGE sql AS $$ SELECT 2 $$",
    );
    assert_eq!(
        rows(&core, &mut s, "alice", "SELECT 1 WHERE yes()").len(),
        1
    );
    assert_eq!(rows(&core, &mut s, "alice", "SELECT 1 WHERE no()").len(), 0);
    assert_eq!(
        rows(&core, &mut s, "alice", "SELECT 1 WHERE two() = 2").len(),
        1
    );
    assert_eq!(
        rows(&core, &mut s, "alice", "SELECT 1 WHERE two() > 5").len(),
        0
    );
    // WHERE が偽なら投影の関数は起動しない（副作用なし）。真なら起動する。
    assert_eq!(
        rows(&core, &mut s, "alice", "SELECT mark() WHERE no()").len(),
        0
    );
    assert_eq!(count(&core, &mut s, "alice", "log"), 0);
    assert_eq!(
        rows(&core, &mut s, "alice", "SELECT mark() WHERE yes()").len(),
        1
    );
    assert_eq!(count(&core, &mut s, "alice", "log"), 1);
}
