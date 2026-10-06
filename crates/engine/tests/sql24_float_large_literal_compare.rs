//! REAL／DOUBLE 列と大きな数値リテラル（`1e21` 等）の比較の結合テスト
//! （Issue #1438。ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-24・
//! `docs/spec/04-behavior/errors.md` ERR-2・ERR-4）。
//!
//! 浮動小数列相手の比較は float8 同士の比較として受理し（2^53 exactness 判定は
//! 整数列・疑似列 `id` 専用）、範囲外は `22003`、整数列の既存挙動は不変であることを、
//! 実 `Storage`＋`CpuScalarProvider` の production 経路（`EngineCore::execute_sql`）で固定する。
//! テナント境界（他テナント行が結果に現れないこと）・エラーがリテラル本文を反射しない
//! ことも確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("qty", ColumnType::Integer, true),
            ColumnDef::new("big", ColumnType::BigInt, true),
            ColumnDef::new("r", ColumnType::Real, true),
            ColumnDef::new("d", ColumnType::Double, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql24-float-large-literal");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert(core: &EngineCore, ctx: &PolicyContext, id: u64, extra: &[(&str, &str)]) {
    let mut columns = vec!["id".to_string(), "embedding".into()];
    let mut values = vec![id.to_string(), "'[0.1,0.2]'".into()];
    for (c, v) in extra {
        columns.push((*c).to_string());
        values.push((*v).to_string());
    }
    let sql = format!(
        "INSERT INTO {TABLE} ({}) VALUES ({}) USING OPERATION_ID 'op-{id}'",
        columns.join(", "),
        values.join(", ")
    );
    core.execute_sql_in_session(ctx, &mut SessionState::default(), &sql)
        .unwrap_or_else(|e| panic!("{sql:?} should succeed, got {e:?}"));
}

/// alice: d / r に大きな値を持つ行。bob: alice と同じ値を持つ他テナント行。
fn seed(core: &EngineCore, alice: &PolicyContext, bob: &PolicyContext) {
    insert(core, alice, 1, &[("d", "1e21"), ("r", "1e21")]);
    insert(
        core,
        alice,
        2,
        &[("d", "-1e21"), ("r", "1180591620717411303424")],
    );
    insert(core, alice, 3, &[("d", "9007199254740992"), ("r", "0.5")]);
    insert(core, alice, 4, &[("d", "1.5")]);
    // 浮動小数列がすべて NULL の行（三値論理による fail-open の検出用）。
    insert(core, alice, 5, &[("qty", "1"), ("big", "1")]);
    insert(core, bob, 9, &[("d", "1e21"), ("r", "1e21")]);
}

fn ids(core: &EngineCore, ctx: &PolicyContext, where_clause: &str) -> Vec<u64> {
    let result = core
        .execute_sql(
            ctx,
            &format!("SELECT id FROM {TABLE} WHERE {where_clause} LIMIT 100"),
        )
        .unwrap_or_else(|e| panic!("query {where_clause:?} should succeed, got {e:?}"));
    let mut out: Vec<u64> = result
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Integer(v)) => *v,
            other => panic!("expected Cell::Integer for id, got {other:?}"),
        })
        .collect();
    out.sort_unstable();
    out
}

fn code(core: &EngineCore, ctx: &PolicyContext, where_clause: &str) -> String {
    core.execute_sql(
        ctx,
        &format!("SELECT id FROM {TABLE} WHERE {where_clause} LIMIT 100"),
    )
    .expect_err(&format!("query {where_clause:?} must be rejected"))
    .wire_code()
    .to_string()
}

#[test]
fn double_column_accepts_large_literals_for_every_operator() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    assert_eq!(ids(&core, &alice, "d = 1e21"), vec![1]);
    assert_eq!(ids(&core, &alice, "d = 1000000000000000000000"), vec![1]);
    assert_eq!(ids(&core, &alice, "d = 1E+21"), vec![1]);
    assert_eq!(ids(&core, &alice, "d = -1e21"), vec![2]);
    assert_eq!(ids(&core, &alice, "d > 1e20"), vec![1]);
    assert_eq!(ids(&core, &alice, "d >= 1e21"), vec![1]);
    assert_eq!(ids(&core, &alice, "d < 1e20"), vec![2, 3, 4]);
    assert_eq!(ids(&core, &alice, "d <= -1e21"), vec![2]);
    // リテラルを左辺にした形。
    assert_eq!(ids(&core, &alice, "1e21 = d"), vec![1]);
    assert_eq!(ids(&core, &alice, "1e20 < d"), vec![1]);
    // 2^53 超の整数字面は float8 比較として最近接値（2^53）の行に一致する。
    assert_eq!(ids(&core, &alice, "d = 9007199254740993"), vec![3]);
    assert_eq!(ids(&core, &alice, "d = 9007199254740993.0"), vec![3]);
    assert_eq!(ids(&core, &alice, "d = 9.007199254740993e15"), vec![3]);
}

#[test]
fn real_column_compares_as_float8_and_accepts_beyond_f32_range() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    // float4 を広げた値は 1e21 と一致しない（PostgreSQL と同じ）。
    assert_eq!(ids(&core, &alice, "r = 1e21"), Vec::<u64>::new());
    assert_eq!(ids(&core, &alice, "r > 1e20"), vec![1, 2]);
    // f32 の範囲外でも f64 の範囲内なら受理する。
    assert_eq!(ids(&core, &alice, "r < 1e39"), vec![1, 2, 3]);
    assert_eq!(ids(&core, &alice, "r = 1e39"), Vec::<u64>::new());
}

#[test]
fn composite_forms_and_string_literals_follow_numeric_form() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    let pairs = [
        ("d IN (1e21, 2)", "d IN ('1e21', '2')"),
        ("d NOT IN (1e21, 2)", "d NOT IN ('1e21', '2')"),
        ("d BETWEEN 1e20 AND 1e22", "d BETWEEN '1e20' AND '1e22'"),
        ("NOT d = 1e21", "NOT d = '1e21'"),
        ("d = 1e21 OR d = -1e21", "d = '1e21' OR d = '-1e21'"),
        ("r > 1e20", "r > '1e20'"),
    ];
    for (numeric, quoted) in pairs {
        assert_eq!(
            ids(&core, &alice, numeric),
            ids(&core, &alice, quoted),
            "{numeric}"
        );
    }
    assert_eq!(ids(&core, &alice, "d IN (1e21, 2)"), vec![1]);
    assert_eq!(ids(&core, &alice, "d NOT IN (1e21, 2)"), vec![2, 3, 4]);
    assert_eq!(ids(&core, &alice, "d BETWEEN 1e20 AND 1e22"), vec![1]);
    assert_eq!(ids(&core, &alice, "d = '9007199254740993'"), vec![3]);
}

#[test]
fn out_of_float_range_literals_are_rejected_with_22003() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    for w in [
        "d = 1e400",
        "d > -1e400",
        "d = 1e-400",
        "r = 1e400",
        "d = '1e400'",
        "1e400 = d",
        "d IN (1, 1e400)",
    ] {
        assert_eq!(code(&core, &alice, w), "22003", "{w}");
    }
    // 非正規化数は受理する。
    assert_eq!(ids(&core, &alice, "d = 1e-310"), Vec::<u64>::new());
    let err = core
        .execute_sql(
            &alice,
            &format!("SELECT id FROM {TABLE} WHERE d = 1e400123 LIMIT 1"),
        )
        .unwrap_err();
    assert!(!format!("{err:?}").contains("1e400123"));
}

#[test]
fn integer_columns_and_id_keep_exactness_guard() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    for w in [
        "qty = 1e21",
        "big = 9007199254740993",
        "big = 1e21",
        "id = 1e21",
        "d + 0 > 1e21",
    ] {
        assert_eq!(code(&core, &alice, w), "22003", "{w}");
    }
}

#[test]
fn tenant_boundary_holds_and_other_tenant_rows_never_appear() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let (alice, bob) = (ctx_for("alice"), ctx_for("bob"));
    seed(&core, &alice, &bob);
    assert_eq!(ids(&core, &bob, "d = 1e21"), vec![9]);
    assert_eq!(ids(&core, &alice, "d = 1e21"), vec![1]);
    let count = core
        .execute_sql(
            &alice,
            &format!("SELECT COUNT(*) FROM {TABLE} WHERE d > 1e20"),
        )
        .expect("count");
    assert!(matches!(
        count.rows.first().and_then(|r| r.cells.first()),
        Some(Cell::Integer(1))
    ));
}
