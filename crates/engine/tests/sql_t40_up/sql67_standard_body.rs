//! SQL 標準形の関数本体（`RETURN`・`BEGIN ATOMIC`。SQL-67 (2)・Issue #2219）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-67 (2)、SQL-66 (3)。設計記録は
//! `docs/design/plpgsql-subset.md`「Issue #2219 追記」。
//!
//! 実 `Storage` ＋ `CpuScalarProvider` の production 経路で次を固定する。
//! - 標準形の関数が、同じ内容の `LANGUAGE sql` 文字列本体の関数と同じ結果・同じ SQLSTATE を返す。
//! - 定義時の名前解決（存在しない表は CREATE 時に `42P01`）と、参照先の DROP・改名の `2BP01`。
//! - 構文の拒否（`42P13`・`42601`）・テナント境界・永続化。

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

fn open(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
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

/// 同じ呼び出しを 2 つの関数（標準形・文字列本体）に対して実行し、結果が一致することを確かめる。
fn same(core: &EngineCore, s: &mut SessionState, call_a: &str, call_b: &str) -> Vec<Vec<Cell>> {
    let a = rows(core, s, "alice", call_a);
    let b = rows(core, s, "alice", call_b);
    assert_eq!(a, b, "{call_a} vs {call_b}");
    a
}

#[test]
fn return_form_equals_string_body() {
    let (core, mut s, _g) = setup("sql67_std_return");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION f1(x int) RETURNS int RETURN x + 1",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION f2(x int) RETURNS int LANGUAGE sql AS $$ SELECT x + 1 $$",
    );
    let r = same(&core, &mut s, "SELECT f1(41)", "SELECT f2(41)");
    assert_eq!(r, vec![vec![int(42)]]);
    // `$n` 参照・名前参照・LANGUAGE 句の併記（前置）・末尾 `;`
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION f3(a int, b int) RETURNS int LANGUAGE sql RETURN a * b;",
    );
    assert_eq!(one(&core, &mut s, "SELECT f3(6, 7)"), int(42));
}

#[test]
fn begin_atomic_equals_string_body() {
    let (core, mut s, _g) = setup("sql67_std_atomic");
    run(&core, &mut s, "alice", "CREATE FUNCTION g1(x int) RETURNS int BEGIN ATOMIC INSERT INTO log VALUES (x); SELECT count(*) FROM log; END");
    run(&core, &mut s, "alice", "CREATE FUNCTION g2(x int) RETURNS int LANGUAGE sql AS $$ INSERT INTO log VALUES (x); SELECT count(*) FROM log $$");
    assert_eq!(one(&core, &mut s, "SELECT g1(1)"), int(1));
    assert_eq!(one(&core, &mut s, "SELECT g2(1)"), int(2));
    assert_eq!(count(&core, &mut s, "alice", "log"), 2);
    // 中間位置の RETURN と CASE ... END
    run(&core, &mut s, "alice", "CREATE FUNCTION g3(x int) RETURNS int BEGIN ATOMIC RETURN CASE WHEN x > 0 THEN 1 ELSE 2 END; END;");
    run(&core, &mut s, "alice", "CREATE FUNCTION g4(x int) RETURNS int LANGUAGE sql AS $$ SELECT CASE WHEN x > 0 THEN 1 ELSE 2 END $$");
    same(&core, &mut s, "SELECT g3(5)", "SELECT g4(5)");
    same(&core, &mut s, "SELECT g3(-5)", "SELECT g4(-5)");
}

#[test]
fn strict_and_null_results_match() {
    let (core, mut s, _g) = setup("sql67_std_strict");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION h1(x int) RETURNS int STRICT RETURN x + 1",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION h2(x int) RETURNS int STRICT LANGUAGE sql AS $$ SELECT x + 1 $$",
    );
    same(
        &core,
        &mut s,
        "SELECT h1(CAST(NULL AS int))",
        "SELECT h2(CAST(NULL AS int))",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION h3(x int) RETURNS int BEGIN ATOMIC SELECT a FROM t WHERE a = x; END",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION h4(x int) RETURNS int LANGUAGE sql AS $$ SELECT a FROM t WHERE a = x $$",
    );
    same(&core, &mut s, "SELECT h3(1)", "SELECT h4(1)");
    run(&core, &mut s, "alice", "CREATE FUNCTION h5(x int) RETURNS int IMMUTABLE BEGIN ATOMIC INSERT INTO log VALUES (x); SELECT 1; END");
    run(&core, &mut s, "alice", "CREATE FUNCTION h6(x int) RETURNS int IMMUTABLE LANGUAGE sql AS $$ INSERT INTO log VALUES (x); SELECT 1 $$");
    assert_eq!(
        code(&core, &mut s, "SELECT h5(1)"),
        code(&core, &mut s, "SELECT h6(1)")
    );
}

#[test]
fn syntax_rejections() {
    let (core, mut s, _g) = setup("sql67_std_syntax");
    for (sql, want) in [
        (
            "CREATE FUNCTION a(x int) RETURNS int AS 'x' RETURN x",
            "42P13",
        ),
        (
            "CREATE FUNCTION a(x int) RETURNS int RETURN x AS x",
            "42601",
        ),
        (
            "CREATE FUNCTION a(x int) RETURNS int LANGUAGE plpgsql RETURN x",
            "42P13",
        ),
        (
            "CREATE FUNCTION a(x int) RETURNS int BEGIN SELECT 1; END",
            "42601",
        ),
        (
            "CREATE FUNCTION a(x int) RETURNS int BEGIN ATOMIC SELECT 1; ",
            "42601",
        ),
        (
            "CREATE FUNCTION a(x int) RETURNS int BEGIN ATOMIC SELECT 1 END",
            "42601",
        ),
        (
            "CREATE FUNCTION a(x int) RETURNS int BEGIN ATOMIC END",
            "42P13",
        ),
        ("CREATE FUNCTION a(x int) RETURN x", "42P13"),
        ("CREATE FUNCTION a(x int) RETURNS int RETURN", "42601"),
    ] {
        assert_eq!(code(&core, &mut s, sql), want, "{sql}");
    }
}

/// 描画した本文が `LANGUAGE sql` の再解析（呼び出し時の字句解析）を通ること。引用符つき識別子・
/// 文字列リテラルの `'`・多文字演算子・`CAST`/`::`・`CASE` を、文字列本体と同じ結果で返す。
#[test]
fn rendered_body_round_trips_through_the_sql_body_parser() {
    let (core, mut s, _g) = setup("sql67_std_roundtrip");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE TABLE \"Mixed\" (a INTEGER, b TEXT)",
    );
    run(
        &core,
        &mut s,
        "alice",
        "INSERT INTO \"Mixed\" VALUES (1, 'it''s'), (2, 'x')",
    );
    for (std, text) in [
        (
            "CREATE FUNCTION r1(x int) RETURNS text BEGIN ATOMIC SELECT b FROM \"Mixed\" WHERE a = x; END",
            "CREATE FUNCTION r1s(x int) RETURNS text LANGUAGE sql AS $$ SELECT b FROM \"Mixed\" WHERE a = x $$",
        ),
        (
            "CREATE FUNCTION r2(x int) RETURNS text RETURN COALESCE(CAST(x AS text), 'it''s')",
            "CREATE FUNCTION r2s(x int) RETURNS text LANGUAGE sql AS $$ SELECT COALESCE(CAST(x AS text), 'it''s') $$",
        ),
        (
            "CREATE FUNCTION r3(x int) RETURNS int RETURN (x + 1) * 2 - x",
            "CREATE FUNCTION r3s(x int) RETURNS int LANGUAGE sql AS $$ SELECT (x + 1) * 2 - x $$",
        ),
    ] {
        run(&core, &mut s, "alice", std);
        run(&core, &mut s, "alice", text);
    }
    same(&core, &mut s, "SELECT r1(1)", "SELECT r1s(1)");
    same(&core, &mut s, "SELECT r2(5)", "SELECT r2s(5)");
    same(&core, &mut s, "SELECT r3(5)", "SELECT r3s(5)");
    assert_eq!(
        one(&core, &mut s, "SELECT r1(1)"),
        Cell::Text("it's".to_string())
    );
}

/// 定義時の名前解決: 存在しない表を参照する標準形は CREATE 時に `42P01`。同じ本体の文字列本体は
/// late binding のため CREATE が成功する（対照）。
#[test]
fn missing_relations_are_rejected_at_definition_time() {
    let (core, mut s, _g) = setup("sql67_std_bind");
    for (i, body) in [
        "SELECT a FROM nope WHERE a = x",
        "INSERT INTO nope VALUES (x); SELECT 1",
        "UPDATE nope SET a = x; SELECT 1",
        "DELETE FROM nope WHERE a = x; SELECT 1",
        "SELECT t.a FROM t JOIN nope ON t.a = nope.a",
        "SELECT a FROM t WHERE a IN (SELECT a FROM nope)",
    ]
    .iter()
    .enumerate()
    {
        let std = format!("CREATE FUNCTION b{i}(x int) RETURNS int BEGIN ATOMIC {body}; END");
        assert_eq!(code(&core, &mut s, &std), "42P01", "{i}: {body}");
        let text = format!("CREATE FUNCTION c{i}(x int) RETURNS int LANGUAGE sql AS $$ {body} $$");
        run(&core, &mut s, "alice", &text);
    }
}

/// 列名・別名が `update` / `delete` でも DML の対象表として誤認されない（列名 `update` の後の `AS` を
/// 表名として引いて `42P01` にならない）。一方、データ変更 CTE 内の不在の対象表は `42P01` のまま。
#[test]
fn column_named_like_dml_keyword_is_not_a_target() {
    let (core, mut s, _g) = setup("sql67_std_kwcol");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE TABLE u (\"update\" INTEGER)",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION k1() RETURNS int BEGIN ATOMIC SELECT \"update\" AS n FROM u; END",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION k2() RETURNS int BEGIN ATOMIC SELECT CAST(\"update\" AS int) FROM u; END",
    );
    let cte = "CREATE FUNCTION k3() RETURNS int BEGIN ATOMIC \
               WITH d AS (DELETE FROM nope WHERE a = 1 RETURNING a) SELECT count(*) FROM d; END";
    assert_eq!(code(&core, &mut s, cte), "0A000");
}

/// 依存: 参照される表・ビュー・関数の DROP は RESTRICT・CASCADE とも `2BP01`（副作用なし）。依存元の
/// 関数を消した後は消せる。
#[test]
fn drop_of_referenced_objects_is_blocked() {
    let (core, mut s, _g) = setup("sql67_std_drop");
    run(&core, &mut s, "alice", "CREATE VIEW v AS SELECT a FROM t");
    run(&core, &mut s, "alice", "INSERT INTO t VALUES (1, 'x')");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION ft(x int) RETURNS int BEGIN ATOMIC SELECT a FROM t WHERE a = x; END",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION fv(x int) RETURNS int BEGIN ATOMIC SELECT a FROM v WHERE a = x; END",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION fg(x int) RETURNS int RETURN ft(x) + 1",
    );
    for sql in [
        "DROP TABLE t",
        "DROP TABLE t CASCADE",
        "DROP VIEW v",
        "DROP VIEW v CASCADE",
        "DROP FUNCTION ft",
        "DROP FUNCTION ft CASCADE",
    ] {
        assert_eq!(code(&core, &mut s, sql), "2BP01", "{sql}");
    }
    // 副作用なし: 関数はそのまま呼べる。
    assert_eq!(one(&core, &mut s, "SELECT ft(1)"), int(1));
    assert_eq!(one(&core, &mut s, "SELECT fv(1)"), int(1));
    assert_eq!(one(&core, &mut s, "SELECT fg(1)"), int(2));
    run(&core, &mut s, "alice", "DROP FUNCTION fg");
    run(&core, &mut s, "alice", "DROP FUNCTION fv");
    run(&core, &mut s, "alice", "DROP VIEW v");
    // ft がまだ t を参照している。
    assert_eq!(code(&core, &mut s, "DROP TABLE t"), "2BP01");
    run(&core, &mut s, "alice", "DROP FUNCTION ft");
    run(&core, &mut s, "alice", "DROP TABLE t");
}

/// 置換は辺を張り替える。標準形から文字列本体への置換で辺が消える。
#[test]
fn replace_rewires_and_clears_edges() {
    let (core, mut s, _g) = setup("sql67_std_replace");
    run(&core, &mut s, "alice", "CREATE TABLE u (a INTEGER)");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION f(x int) RETURNS int BEGIN ATOMIC SELECT a FROM t WHERE a = x; END",
    );
    assert_eq!(code(&core, &mut s, "DROP TABLE t"), "2BP01");
    run(&core, &mut s, "alice", "CREATE OR REPLACE FUNCTION f(x int) RETURNS int BEGIN ATOMIC SELECT a FROM u WHERE a = x; END");
    run(&core, &mut s, "alice", "DROP TABLE t");
    assert_eq!(code(&core, &mut s, "DROP TABLE u"), "2BP01");
    run(&core, &mut s, "alice", "CREATE OR REPLACE FUNCTION f(x int) RETURNS int LANGUAGE sql AS $$ SELECT a FROM u WHERE a = x $$");
    run(&core, &mut s, "alice", "DROP TABLE u");
}

/// 自己参照と循環は `0A000`。
#[test]
fn self_reference_and_cycles_are_rejected() {
    let (core, mut s, _g) = setup("sql67_std_cycle");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION s1(x int) RETURNS int LANGUAGE sql AS $$ SELECT x $$",
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE OR REPLACE FUNCTION s1(x int) RETURNS int RETURN s1(x)"
        ),
        "0A000"
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION g(x int) RETURNS int LANGUAGE sql AS $$ SELECT x $$",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION h(x int) RETURNS int RETURN g(x)",
    );
    // g を h を呼ぶ標準形へ置換すると g -> h -> g の循環。
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE OR REPLACE FUNCTION g(x int) RETURNS int RETURN h(x)"
        ),
        "0A000"
    );
    assert_eq!(one(&core, &mut s, "SELECT h(3)"), int(3));
}

/// 依存される表の改名・列の改名・削除・型変更は `2BP01`。依存の無い列追加は通る。
#[test]
fn rename_and_column_changes_of_referenced_tables_are_blocked() {
    let (core, mut s, _g) = setup("sql67_std_alter");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION f(x int) RETURNS int BEGIN ATOMIC SELECT a FROM t WHERE a = x; END",
    );
    for sql in [
        "ALTER TABLE t RENAME TO t2",
        "ALTER TABLE t RENAME COLUMN a TO c",
        "ALTER TABLE t DROP COLUMN b",
        "ALTER TABLE t ALTER COLUMN a TYPE BIGINT",
    ] {
        assert_eq!(code(&core, &mut s, sql), "2BP01", "{sql}");
    }
    run(
        &core,
        &mut s,
        "alice",
        "ALTER TABLE t ADD COLUMN extra INTEGER",
    );
    run(&core, &mut s, "alice", "DROP FUNCTION f");
    run(&core, &mut s, "alice", "ALTER TABLE t RENAME TO t2");
}

/// テナント境界: 別テナントの標準形の関数が参照する表でも、DROP は `2BP01` で止まり、文言に依存元の
/// 関数名・テナントが含まれない。他テナントの関数は見えない。
#[test]
fn tenant_boundary_holds_for_dependencies() {
    let (core, mut s, _g) = setup("sql67_std_tenant");
    run(&core, &mut s, "alice", "CREATE FUNCTION secret_fn(x int) RETURNS int BEGIN ATOMIC SELECT a FROM t WHERE a = x; END");
    let e = err(&core, &mut s, "bob", "DROP TABLE t");
    assert_eq!(e.wire_code(), "2BP01");
    let msg = e.to_string();
    assert!(
        !msg.contains("secret_fn") && !msg.contains("alice"),
        "{msg}"
    );
    // 別テナントのセッションにはロードされず、関数は不在と同じ `42883`。
    let mut sb = session();
    core.load_session_udfs(&ctx("bob"), &mut sb).expect("load");
    assert_eq!(
        err(&core, &mut sb, "bob", "SELECT secret_fn(1)").wire_code(),
        "42883"
    );
}

/// 永続化: 開き直した後も呼べ、依存も保たれる。
#[test]
fn definitions_and_dependencies_survive_reopen() {
    let path = unique_db_path("sql67_std_reopen");
    let _g = CleanupGuard(path.clone());
    {
        let core = open(&path);
        let mut s = session();
        run(&core, &mut s, "alice", "CREATE TABLE t (a INTEGER)");
        run(&core, &mut s, "alice", "INSERT INTO t VALUES (7)");
        run(
            &core,
            &mut s,
            "alice",
            "CREATE FUNCTION f() RETURNS int BEGIN ATOMIC SELECT a FROM t; END",
        );
    }
    let core = open(&path);
    let mut s = session();
    core.load_session_udfs(&ctx("alice"), &mut s).expect("load");
    assert_eq!(one(&core, &mut s, "SELECT f()"), int(7));
    assert_eq!(code(&core, &mut s, "DROP TABLE t"), "2BP01");
}

/// 新規作成での自己参照（定義対象がまだカタログに無い）も `0A000`（codex 指摘・P1）。
#[test]
fn self_reference_on_fresh_create_is_rejected() {
    let (core, mut s, _g) = setup("sql67_std_selfnew");
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE FUNCTION f(x int) RETURNS int RETURN f(x)"
        ),
        "0A000"
    );
    assert_eq!(
        code(
            &core,
            &mut s,
            "CREATE FUNCTION f(x int) RETURNS int BEGIN ATOMIC SELECT F(x); END"
        ),
        "0A000"
    );
}

/// 引用付きの識別子（句の語と同じ綴り）は本体の切り出し・保存・再解析を通じて識別子のまま残る。
#[test]
fn quoted_clause_words_stay_identifiers() {
    let (core, mut s, _g) = setup("sql67_std_quoted");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION q1(x int) RETURNS int BEGIN ATOMIC SELECT x AS \"into\"; END",
    );
    assert_eq!(one(&core, &mut s, "SELECT q1(4)"), int(4));
    run(&core, &mut s, "alice", "CREATE TABLE t2 (\"end\" INTEGER)");
    run(&core, &mut s, "alice", "INSERT INTO t2 VALUES (4)");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION q2() RETURNS int BEGIN ATOMIC SELECT \"end\" + 1 FROM t2; END",
    );
    assert_eq!(one(&core, &mut s, "SELECT q2()"), int(5));
}

/// `INSERT INTO f(a)` の対象表・列リストは関数呼び出しではない（同名の関数 `f` を定義しても自己参照にならない）。
#[test]
fn insert_target_with_column_list_is_not_a_call() {
    let (core, mut s, _g) = setup("sql67_std_insert_cols");
    run(&core, &mut s, "alice", "CREATE TABLE f (a INTEGER)");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION f(x int) RETURNS int BEGIN ATOMIC INSERT INTO f(a) VALUES (x); SELECT x; END",
    );
    // 対象表への依存辺が残り、表の DROP は 2BP01 で止まる。
    assert_eq!(code(&core, &mut s, "DROP TABLE f"), "2BP01");
}

/// データ変更 CTE を含む本体は定義時に拒否される（依存辺を取りこぼす形を保存しない。fail-closed）。
/// 対象表が存在しない場合も同じ拒否で、依存を迂回して保存される経路は無い。
#[test]
fn data_modifying_cte_is_rejected_at_definition_time() {
    let (core, mut s, _g) = setup("sql67_std_cte_dep");
    for target in ["t", "nope"] {
        let sql = format!(
            "CREATE FUNCTION fd(x int) RETURNS int BEGIN ATOMIC \
             WITH d AS (DELETE FROM {target} WHERE a = x RETURNING a) SELECT x; END"
        );
        let e = err(&core, &mut s, "alice", &sql);
        assert_eq!(e.wire_code(), "0A000", "{sql}: {e}");
    }
    // 何も保存されていないので表は DROP できる。
    run(&core, &mut s, "alice", "DROP TABLE t");
}

/// 引用識別子の対象表（`"NULL"`・`"true"` 等は別トークンになる）も存在確認と依存記録の対象になる。
#[test]
fn quoted_keyword_like_dml_targets_are_bound() {
    let (core, mut s, _g) = setup("sql67_std_quoted_target");
    for (i, (name, dml)) in [
        ("NULL", "INSERT INTO \"NULL\" VALUES (1)"),
        ("ARRAY", "UPDATE \"ARRAY\" SET a = 1"),
        ("true", "DELETE FROM \"true\" WHERE a = 1"),
    ]
    .iter()
    .enumerate()
    {
        let create =
            format!("CREATE FUNCTION qt{i}() RETURNS int BEGIN ATOMIC {dml}; SELECT 1; END");
        // 不在の表は定義時に 42P01。
        assert_eq!(code(&core, &mut s, &create), "42P01", "{dml}");
        run(
            &core,
            &mut s,
            "alice",
            &format!("CREATE TABLE \"{name}\" (a INTEGER)"),
        );
        run(&core, &mut s, "alice", &create);
        // 依存が記録され、表の DROP は止まる。
        assert_eq!(
            code(&core, &mut s, &format!("DROP TABLE \"{name}\"")),
            "2BP01",
            "{dml}"
        );
    }
}

/// CTE の列名リスト宣言 `WITH f(a) AS (...)` は関数呼び出しではない（同名関数への依存も自己参照も作らない）。
#[test]
fn cte_column_list_declaration_is_not_a_call() {
    let (core, mut s, _g) = setup("sql67_std_cte_decl");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION g(x int) RETURNS int BEGIN ATOMIC SELECT x; END",
    );
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION f(x int) RETURNS int BEGIN ATOMIC \
         WITH f(a) AS (SELECT b FROM t) SELECT count(*) FROM t; END",
    );
    assert_eq!(one(&core, &mut s, "SELECT f(3)"), int(0));
    run(
        &core,
        &mut s,
        "alice",
        "CREATE FUNCTION h(x int) RETURNS int BEGIN ATOMIC \
         WITH g(a) AS (SELECT b FROM t) SELECT count(*) FROM t; END",
    );
    // g への依存辺は無いので DROP できる。
    run(&core, &mut s, "alice", "DROP FUNCTION g");
}

/// `CREATE OR REPLACE VIEW` では引用句語識別子が特別扱いされず、ビューが通常どおり作れる。
#[test]
fn or_replace_view_with_quoted_clause_word() {
    let (core, mut s, _g) = setup("sql67_std_or_replace_view");
    run(
        &core,
        &mut s,
        "alice",
        "CREATE OR REPLACE VIEW vv AS SELECT a, b FROM t",
    );
    assert_eq!(
        rows(&core, &mut s, "alice", "SELECT * FROM vv").len(),
        rows(&core, &mut s, "alice", "SELECT a FROM t").len()
    );
}
