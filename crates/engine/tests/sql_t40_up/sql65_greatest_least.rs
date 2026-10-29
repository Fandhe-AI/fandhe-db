//! `GREATEST`／`LEAST` の結合テスト（Issue #1909、対象ビヘイビア: SQL-65 ポインタ。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md`）。
//!
//! NULL 引数の無視、`CASE` と同じ型統一（不一致は `42804`）、引数 100 個上限（`54023`）、
//! 未対応型の fail-closed 拒否、テナント境界（他テナントの行が結果に現れない）を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::storage::{Storage, Visibility};

use crate::legacy_table::LegacyExec;
use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 列順: embedding, a(INTEGER), b(INTEGER), big(BIGINT), r(REAL), t(TEXT), d(DATE), ts(TIMESTAMP)
fn new_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("a", ColumnType::Integer, true),
                ColumnDef::new("b", ColumnType::Integer, true),
                ColumnDef::new("big", ColumnType::BigInt, true),
                ColumnDef::new("r", ColumnType::Real, true),
                ColumnDef::new("t", ColumnType::Text, true),
                ColumnDef::new("d", ColumnType::Date, true),
                ColumnDef::new("ts", ColumnType::Timestamp, true),
            ],
        ))
        .expect("create table");
    // d=19723 は 2024-01-01、ts は 1970-01-01 00:00:00 起点のマイクロ秒。
    let rows: Vec<(&str, u64, Visibility, Vec<Value>)> = vec![
        (
            "tenant-a",
            1,
            Visibility::Public,
            vec![
                Value::Vector(vec![0.1, 0.2]),
                Value::Integer(1),
                Value::Integer(5),
                Value::BigInt(10),
                Value::Real(1.5),
                Value::Text("apple".to_string()),
                Value::Date(19723),
                Value::Timestamp(1_000_000),
            ],
        ),
        (
            "tenant-a",
            2,
            Visibility::Public,
            vec![
                Value::Vector(vec![0.1, 0.2]),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Null,
            ],
        ),
        (
            "tenant-a",
            3,
            Visibility::Public,
            vec![
                Value::Vector(vec![0.1, 0.2]),
                Value::Integer(7),
                Value::Null,
                Value::BigInt(2),
                Value::Real(0.5),
                Value::Text("Zed".to_string()),
                Value::Date(19724),
                Value::Timestamp(5),
            ],
        ),
        // 別テナントの Private 行は tenant-a から不可視（RLS 相当を緩めていない確認）。
        (
            "tenant-b",
            4,
            Visibility::Private,
            vec![
                Value::Vector(vec![0.1, 0.2]),
                Value::Integer(999),
                Value::Integer(999),
                Value::BigInt(999),
                Value::Real(999.0),
                Value::Text("zzz".to_string()),
                Value::Date(30000),
                Value::Timestamp(999_999_999),
            ],
        ),
    ];
    for (tenant, id, vis, values) in rows {
        let op_id =
            engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}")).expect("op");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx_for(tenant),
            id,
            vis,
            &values,
            &op_id,
        )
        .expect("insert row");
    }
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn cells(core: &EngineCore, sql: &str) -> Vec<Vec<Cell>> {
    let ctx = ctx_for("tenant-a");
    let r = core
        .execute_sql(&ctx, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    r.rows.iter().map(|row| row.cells.clone()).collect()
}

fn code_of(core: &EngineCore, sql: &str) -> String {
    let ctx = ctx_for("tenant-a");
    core.execute_sql(&ctx, sql)
        .expect_err("expected error")
        .wire_code()
        .to_string()
}

/// `SELECT <expr> FROM docs ORDER BY id` の 1 列目を行順（id 昇順）で返す。
fn col(core: &EngineCore, expr: &str) -> Vec<Cell> {
    cells(core, &format!("SELECT {expr} FROM docs ORDER BY id"))
        .into_iter()
        .map(|mut r| r.remove(0))
        .collect()
}

macro_rules! fixture {
    ($name:literal) => {{
        let path = unique_db_path($name);
        let guard = CleanupGuard(path.clone());
        let core = new_core(&path);
        (core, guard)
    }};
}

/// 定数引数の基本動作。SQL-65。
#[test]
fn greatest_and_least_pick_extremes_of_constants() {
    let (core, _g) = fixture!("sql65-gl-const");
    assert_eq!(col(&core, "greatest(1, 2, 3)")[0], Cell::SignedInteger(3));
    assert_eq!(col(&core, "least(3, 1, 2)")[0], Cell::SignedInteger(1));
    assert_eq!(col(&core, "greatest(5)")[0], Cell::SignedInteger(5));
}

/// NULL を無視し、全引数が NULL の行は NULL。他テナント行は現れない。SQL-65。
#[test]
fn null_arguments_are_ignored_and_all_null_yields_null() {
    let (core, _g) = fixture!("sql65-gl-null");
    assert_eq!(
        col(&core, "greatest(a, b)"),
        vec![Cell::SignedInteger(5), Cell::Null, Cell::SignedInteger(7)]
    );
    assert_eq!(
        col(&core, "least(a, b)"),
        vec![Cell::SignedInteger(1), Cell::Null, Cell::SignedInteger(7)]
    );
    assert_eq!(
        col(&core, "greatest(NULL, a, NULL, 3)"),
        vec![
            Cell::SignedInteger(3),
            Cell::SignedInteger(3),
            Cell::SignedInteger(7)
        ]
    );
}

/// リテラル NULL のみは型が決まらず `0A000`（`CASE`／`COALESCE` と共通の既知制約）。SQL-65。
#[test]
fn only_literal_null_arguments_is_0a000() {
    let (core, _g) = fixture!("sql65-gl-allnull");
    assert_eq!(
        code_of(&core, "SELECT greatest(NULL, NULL) FROM docs"),
        "0A000"
    );
}

/// 整数種別の結果列型は `COALESCE` と同じく透過する。SQL-65。
#[test]
fn integer_and_bigint_mix_keeps_integer_cells() {
    let (core, _g) = fixture!("sql65-gl-int");
    assert_eq!(
        col(&core, "greatest(a, big)"),
        vec![Cell::SignedInteger(10), Cell::Null, Cell::SignedInteger(7)]
    );
}

/// TEXT はバイト順（大文字が小文字より小さい）、DATE・TIMESTAMP も比較できる。SQL-65。
#[test]
fn text_date_timestamp_comparisons() {
    let (core, _g) = fixture!("sql65-gl-types");
    assert_eq!(
        col(&core, "greatest(t, 'B')"),
        vec![
            Cell::Text("apple".to_string()),
            Cell::Text("B".to_string()),
            Cell::Text("Zed".to_string())
        ]
    );
    assert_eq!(
        col(&core, "least(t, 'B')"),
        vec![
            Cell::Text("B".to_string()),
            Cell::Text("B".to_string()),
            Cell::Text("B".to_string())
        ]
    );
    let g = col(&core, "greatest(d, DATE '2024-01-02')");
    assert_eq!(g[0], Cell::Date(19724));
    assert_eq!(g[2], Cell::Date(19724));
    let l = col(&core, "least(ts, TIMESTAMP '1970-01-01 00:00:02')");
    assert_eq!(l[0], Cell::Timestamp(1_000_000));
    assert_eq!(l[2], Cell::Timestamp(5));
}

/// 型が統一できなければ `42804`、比較器の無い型は fail-closed。SQL-65。
#[test]
fn incompatible_and_unsupported_types_fail_closed() {
    let (core, _g) = fixture!("sql65-gl-types-err");
    assert_eq!(code_of(&core, "SELECT greatest(a, t) FROM docs"), "42804");
    assert_eq!(
        code_of(&core, "SELECT least(embedding, embedding) FROM docs"),
        "42883"
    );
}

/// 引数は 100 個まで受理し、101 個は `54023`、0 個は `42883`。SQL-65 (3)。
#[test]
fn argument_count_limits() {
    let (core, _g) = fixture!("sql65-gl-arity");
    for n in [2usize, 33, 100] {
        let args = vec!["a"; n].join(", ");
        let r = col(&core, &format!("greatest({args})"));
        assert_eq!(r[0], Cell::SignedInteger(1), "n={n}");
    }
    let args = vec!["a"; 101].join(", ");
    assert_eq!(
        code_of(&core, &format!("SELECT greatest({args}) FROM docs")),
        "54023"
    );
    assert_eq!(code_of(&core, "SELECT least() FROM docs"), "42883");
}

/// WHERE・ORDER BY の位置でも使え、他テナントの行は現れない。SQL-65。
#[test]
fn usable_in_where_and_order_by_within_tenant() {
    let (core, _g) = fixture!("sql65-gl-where");
    let r = cells(
        &core,
        "SELECT id FROM docs WHERE greatest(a, b) > 4 ORDER BY id",
    );
    assert_eq!(r, vec![vec![Cell::Integer(1)], vec![Cell::Integer(3)]]);
    let r = cells(
        &core,
        "SELECT id FROM docs WHERE least(a, big) < 3 ORDER BY id",
    );
    assert_eq!(r, vec![vec![Cell::Integer(1)], vec![Cell::Integer(3)]]);
}

/// `CREATE FUNCTION greatest(...)` は予約名として `42723` で拒否され、UDF 本体では使える。SQL-65。
#[test]
fn reserved_name_and_udf_body() {
    let (core, _g) = fixture!("sql65-gl-udf");
    let ctx = ctx_for("tenant-a");
    let mut session = engine::sql::mode::SessionState::default();
    for name in ["greatest", "least"] {
        let err = core
            .execute_sql_in_session(
                &ctx,
                &mut session,
                &format!("CREATE FUNCTION {name}(x) AS x"),
            )
            .expect_err("reserved");
        assert_eq!(err.wire_code(), "42723", "{name}");
    }
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE FUNCTION clamp0(x) AS greatest(x, 0)",
    )
    .expect("udf body");
    let r = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT clamp0(a - 3) FROM docs WHERE id = 1",
        )
        .expect("call udf");
    let engine::sql::SqlOutcome::Query(q) = r else {
        panic!("expected query outcome");
    };
    assert_eq!(q.rows[0].cells[0], Cell::Float(0.0));
}

/// CHECK 制約本体と INSERT VALUES の定数式でも使える（NULL 引数は無視）。SQL-65。
#[test]
fn usable_in_check_constraint_and_insert_values() {
    let (core, _g) = fixture!("sql65-gl-check");
    let ctx = ctx_for("tenant-a");
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session_legacy(
        &ctx,
        &mut session,
        "CREATE TABLE gl_ck (lo INTEGER, hi INTEGER, CONSTRAINT gl_ck_pos CHECK (greatest(lo, hi) >= 1))",
    )
    .expect("create table with greatest check");
    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO gl_ck (id, lo, hi) VALUES (1, 0, -3) USING OPERATION_ID 'op-gl-1'",
        )
        .expect_err("greatest(0, -3) < 1 must violate the CHECK");
    assert_eq!(err.wire_code(), "23514", "{err:?}");
    core.execute_insert_sql(
        &ctx,
        "INSERT INTO gl_ck (id, lo, hi) VALUES (2, 0, 4) USING OPERATION_ID 'op-gl-2'",
    )
    .expect("conforming row");
    core.execute_insert_sql(
        &ctx,
        "INSERT INTO gl_ck (id, lo, hi) VALUES (3, greatest(1, NULL, 7), least(2, 9)) \
         USING OPERATION_ID 'op-gl-3'",
    )
    .expect("constant greatest/least in VALUES");
    let r = cells(&core, "SELECT lo, hi FROM gl_ck WHERE id = 3");
    assert_eq!(
        r,
        vec![vec![Cell::SignedInteger(7), Cell::SignedInteger(2)]]
    );
}
