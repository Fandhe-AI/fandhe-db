//! `CREATE FUNCTION` のパラメータ型注釈の結合テスト（Issue #1915、対象ビヘイビア: SQL-66 (1)。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md`・`docs/spec/05-tasks.md` TASK-309）。
//!
//! 型付き引数リストの受理・型名の静的解決（未知の型は `42704`）・未対応形の fail-closed 拒否・
//! 呼び出し時の暗黙変換と型不一致の拒否・型なし従来形の回帰・RLS／セッション境界の維持を固定する。

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

/// DDL 権限つきのセッション。型付き引数・RETURNS 付きの定義は #2798 までカタログへ書かず、
/// 権限のあるセッションのセッション限定レジストリへ登録される（Issue #1922）。
fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

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
                ColumnDef::new("flag", ColumnType::Boolean, true),
            ],
        ))
        .expect("create table");
    for (tenant, id, n) in [("tenant-a", 1u64, 7i32), ("tenant-b", 2, 100)] {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}")).expect("op");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx_for(tenant),
            id,
            if tenant == "tenant-b" {
                Visibility::Private
            } else {
                Visibility::Public
            },
            &[
                Value::Vector(vec![0.1, 0.2]),
                Value::Integer(n),
                Value::Bool(true),
            ],
            &op_id,
        )
        .expect("insert row");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run(core: &EngineCore, session: &mut SessionState, sql: &str) -> Result<SqlOutcome, String> {
    core.execute_sql_in_session(&ctx_for("tenant-a"), session, sql)
        .map_err(|e| e.wire_code().to_string())
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
fn typed_definition_is_accepted_and_callable() {
    let (core, _g) = new_core("sql66-basic");
    let mut s = ddl_session();
    define(&core, &mut s, "CREATE FUNCTION add1(x integer) AS x + 1");
    assert_eq!(
        first_cell(&core, &mut s, "SELECT add1(2) FROM docs LIMIT 1"),
        Cell::Float(3.0)
    );
}

#[test]
fn multi_word_and_array_types_are_accepted() {
    let (core, _g) = new_core("sql66-types");
    let mut s = ddl_session();
    for (i, ty) in [
        "double precision",
        "character varying",
        "timestamp with time zone",
        "numeric",
        "text",
        "boolean",
        "int[]",
        "bigint",
        "real",
        "date",
    ]
    .iter()
    .enumerate()
    {
        define(&core, &mut s, &format!("CREATE FUNCTION f{i}(x {ty}) AS 1"));
    }
}

#[test]
fn untyped_form_is_unchanged() {
    let (core, _g) = new_core("sql66-untyped");
    let mut s = ddl_session();
    define(&core, &mut s, "CREATE FUNCTION f(x) AS x + 1");
    assert_eq!(
        first_cell(&core, &mut s, "SELECT f(2) FROM docs LIMIT 1"),
        Cell::Float(3.0)
    );
    define(&core, &mut s, "CREATE FUNCTION g() AS 1");
}

#[test]
fn unknown_and_serial_types_are_42704_deterministically() {
    let (core, _g) = new_core("sql66-unknown");
    let mut s = ddl_session();
    for _ in 0..2 {
        assert_eq!(
            code(&core, &mut s, "CREATE FUNCTION f(x nosuchtype) AS x"),
            "42704"
        );
        assert_eq!(
            code(&core, &mut s, "CREATE FUNCTION f(x serial) AS x"),
            "42704"
        );
    }
}

#[test]
fn unsupported_forms_are_fail_closed() {
    let (core, _g) = new_core("sql66-unsupported");
    let mut s = ddl_session();
    for sql in [
        "CREATE FUNCTION f(v vector(3)) AS 1",
        "CREATE FUNCTION f(x char(3)) AS 1",
        "CREATE FUNCTION f(IN x integer) AS x",
        "CREATE FUNCTION f(x integer DEFAULT 1) AS x",
        "CREATE FUNCTION f(x integer = 1) AS x",
    ] {
        assert_eq!(code(&core, &mut s, sql), "0A000", "{sql}");
    }
    assert_eq!(
        code(&core, &mut s, "CREATE FUNCTION f(x integer, y) AS x"),
        "42601"
    );
    assert_eq!(
        code(&core, &mut s, "CREATE FUNCTION f(x, y integer) AS x"),
        "42601"
    );
    // 失敗した定義はレジストリを変更しない。
    define(&core, &mut s, "CREATE FUNCTION f(x integer) AS x");
}

/// 引数モードは名前の前後どちらに書いても同じ `0A000`（名前の後ろを型名として解釈して `42704` に
/// ならない）。拒否した定義はレジストリを変更しない。
#[test]
fn argument_mode_after_name_is_0a000_like_before_name() {
    let (core, _g) = new_core("sql66-mode-after-name");
    let mut s = ddl_session();
    for sql in [
        "CREATE FUNCTION f(a OUT integer) AS a",
        "CREATE FUNCTION f(a IN integer) AS a",
        "CREATE FUNCTION f(a inout integer) AS a",
        "CREATE FUNCTION f(a VARIADIC integer) AS a",
        "CREATE FUNCTION f(a OUT) AS a",
        "CREATE FUNCTION f(OUT a integer) AS a",
    ] {
        assert_eq!(code(&core, &mut s, sql), "0A000", "{sql}");
    }
    define(&core, &mut s, "CREATE FUNCTION f(a integer) AS a");
}

#[test]
fn varchar_modifier_is_normalized_and_does_not_truncate() {
    let (core, _g) = new_core("sql66-varchar");
    let mut s = ddl_session();
    define(&core, &mut s, "CREATE FUNCTION id_t(x varchar(3)) AS x");
    assert_eq!(
        first_cell(&core, &mut s, "SELECT id_t('abcdef') FROM docs LIMIT 1"),
        Cell::Text("abcdef".to_string())
    );
}

#[test]
fn implicit_conversions_are_accepted() {
    let (core, _g) = new_core("sql66-implicit");
    let mut s = ddl_session();
    define(&core, &mut s, "CREATE FUNCTION b(x bigint) AS x");
    define(&core, &mut s, "CREATE FUNCTION d(x float8) AS x");
    define(&core, &mut s, "CREATE FUNCTION tx(x text) AS x");
    define(&core, &mut s, "CREATE FUNCTION i(x integer) AS x");
    for sql in [
        "SELECT b(1) FROM docs LIMIT 1",
        "SELECT d(1) FROM docs LIMIT 1",
        "SELECT tx('a') FROM docs LIMIT 1",
        "SELECT i('5') FROM docs LIMIT 1",
        "SELECT b(n) FROM docs LIMIT 1",
    ] {
        run(&core, &mut s, sql).unwrap_or_else(|c| panic!("{sql}: {c}"));
    }
}

#[test]
fn malformed_unknown_literal_is_input_error() {
    let (core, _g) = new_core("sql66-22p02");
    let mut s = ddl_session();
    define(&core, &mut s, "CREATE FUNCTION i(x integer) AS x");
    assert_eq!(
        code(&core, &mut s, "SELECT i('abc') FROM docs LIMIT 1"),
        "22P02"
    );
}

#[test]
fn type_mismatch_is_42883() {
    let (core, _g) = new_core("sql66-mismatch");
    let mut s = ddl_session();
    define(&core, &mut s, "CREATE FUNCTION i(x integer) AS x");
    define(&core, &mut s, "CREATE FUNCTION dt(x date) AS 1");
    define(&core, &mut s, "CREATE FUNCTION nt(x text) AS x");
    assert_eq!(
        code(&core, &mut s, "SELECT i(2.5) FROM docs LIMIT 1"),
        "42883"
    );
    assert_eq!(
        code(&core, &mut s, "SELECT nt(n) FROM docs LIMIT 1"),
        "42883"
    );
    assert_eq!(
        code(&core, &mut s, "SELECT dt(1) FROM docs LIMIT 1"),
        "42883"
    );
    // 引数個数の不一致は従来どおり 42883。
    assert_eq!(
        code(&core, &mut s, "SELECT i(1, 2) FROM docs LIMIT 1"),
        "42883"
    );
}

#[test]
fn statically_unknown_numeric_argument_is_fail_closed() {
    let (core, _g) = new_core("sql66-unknown-numeric");
    let mut s = ddl_session();
    define(&core, &mut s, "CREATE FUNCTION i(x integer) AS x");
    assert_eq!(
        code(&core, &mut s, "SELECT i(id) FROM docs LIMIT 1"),
        "0A000"
    );
    // 型付き算術は静的に整数種別へ解決できるため受理される。
    run(&core, &mut s, "SELECT i(n + 1) FROM docs LIMIT 1").expect("typed arithmetic");
    // numeric 宣言は CAST と同じく値の変換が未実装のため、呼び出し時に fail-closed。
    define(&core, &mut s, "CREATE FUNCTION nm(x numeric) AS x");
    assert_eq!(
        code(&core, &mut s, "SELECT nm(1) FROM docs LIMIT 1"),
        "0A000"
    );
}

#[test]
fn sessionless_entry_still_rejects_create_function() {
    let (core, _g) = new_core("sql66-sessionless");
    let err = core
        .execute_sql(&ctx_for("tenant-a"), "CREATE FUNCTION f(x integer) AS x")
        .expect_err("rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn typed_udf_respects_rls_and_session_boundaries() {
    let (core, _g) = new_core("sql66-rls");
    let mut s = ddl_session();
    define(&core, &mut s, "CREATE FUNCTION big(x bigint) AS x");
    // tenant-b の行 (n = 100) は不可視。
    match run(
        &core,
        &mut s,
        "SELECT id, big(n) FROM docs ORDER BY id LIMIT 10",
    ) {
        Ok(SqlOutcome::Query(q)) => assert_eq!(q.rows.len(), 1),
        other => panic!("unexpected {other:?}"),
    }
    let mut other_session = ddl_session();
    assert_eq!(
        code(&core, &mut other_session, "SELECT big(1) FROM docs LIMIT 1"),
        "42883"
    );
}

/// 入れ子の UDF 呼び出しでも、型注釈つきパラメータ参照は宣言型を保って暗黙変換検査を通る
/// （レビュー指摘対応: パラメータ参照が種別不明の数値になり `0A000` になっていた）。
#[test]
fn nested_typed_udf_call_keeps_declared_param_type() {
    let (core, _g) = new_core("sql66-nested");
    let mut s = ddl_session();
    define(&core, &mut s, "CREATE FUNCTION f(x integer) AS x");
    define(&core, &mut s, "CREATE FUNCTION g(y integer) AS f(y)");
    run(&core, &mut s, "SELECT g(1) FROM docs LIMIT 1").expect("nested integer");
    // 宣言型と合わない入れ子は静的な型で 42883。
    define(&core, &mut s, "CREATE FUNCTION t(z text) AS f(z)");
    assert_eq!(
        code(&core, &mut s, "SELECT t('a') FROM docs LIMIT 1"),
        "42883"
    );
}

/// 入れ子呼び出しで宣言型 integer のパラメータ参照を bigint パラメータへ渡せる（暗黙変換。
/// 宣言型を保たず種別不明の数値として `0A000` になっていた退行の防止。Issue #1915）。
#[test]
fn nested_typed_udf_call_widens_integer_to_bigint() {
    let (core, _g) = new_core("sql66-nested-widen");
    let mut s = ddl_session();
    define(&core, &mut s, "CREATE FUNCTION f(x bigint) AS x");
    define(&core, &mut s, "CREATE FUNCTION g(y integer) AS f(y)");
    run(&core, &mut s, "SELECT g(1) FROM docs LIMIT 1").expect("integer to bigint");
}

/// 配列型パラメータの暗黙変換検査は `CastRule::Element` を失敗扱いにせず要素型の組で判定する
/// （レビュー指摘対応）。同一要素型は型不一致（`42883`）にならない。配列値そのものを UDF の
/// 実引数として運ぶ経路は CAST 結果値の制約（既存）により未対応のため `0A000` で fail-closed のまま。
#[test]
fn exact_type_array_argument_is_not_a_type_mismatch() {
    let (core, _g) = new_core("sql66-array-arg");
    let mut s = ddl_session();
    define(&core, &mut s, "CREATE FUNCTION a(x int[]) AS 1");
    assert_eq!(
        code(&core, &mut s, "SELECT a(ARRAY[1, 2]) FROM docs LIMIT 1"),
        "0A000"
    );
}

/// boolean 列を boolean 型パラメータの実引数にできる（`bind_expr_in` が BOOLEAN 列を拒否して
/// `22000` になっていた退行の防止。Issue #1915）。
#[test]
fn boolean_column_is_accepted_for_boolean_param() {
    let (core, _g) = new_core("sql66-bool-col");
    let mut s = ddl_session();
    define(&core, &mut s, "CREATE FUNCTION f(x boolean) AS x");
    run(&core, &mut s, "SELECT f(flag) FROM docs LIMIT 1").expect("boolean column argument");
}

/// `%TYPE`（修飾名を含む）は型名解決より先に `0A000` で拒否する（`42704` にしない）。
#[test]
fn percent_type_forms_are_0a000_before_type_resolution() {
    let (core, _g) = new_core("sql66-percent-type");
    let mut s = ddl_session();
    for sql in [
        "CREATE FUNCTION f(x docs.n%TYPE) AS x",
        "CREATE FUNCTION f(x n%TYPE) AS x",
        "CREATE FUNCTION f(x integer%TYPE) AS x",
    ] {
        assert_eq!(code(&core, &mut s, sql), "0A000", "{sql}");
    }
}
