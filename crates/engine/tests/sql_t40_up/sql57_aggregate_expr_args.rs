//! 集計引数の式（算術・関数・CASE）の結合テスト（SQL-57・Issue #1809。関連: SQL-13・SQL-25 (c)・
//! RLS-7/RLS-8）。
//!
//! 期待値は PostgreSQL 17 の意味論に従う固定値（NULL 入力は無視・結果型は式の型。数値式の
//! 結果型は本エンジンの式エンジン既定の double）。`unique_db_path`＋`CleanupGuard`、
//! `EngineCore::execute_sql` の production 経路で検証し、他テナントの Private 行が結果へ
//! 影響しないこと（RLS）、式経由でも RLS 内部列を参照できないこと、未対応形の fail-closed な
//! `wire_code` を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "t";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("g", ColumnType::Text, true),
            ColumnDef::new("k", ColumnType::Integer, true),
            ColumnDef::new("f", ColumnType::Boolean, true),
            ColumnDef::new("dt", ColumnType::Date, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `(id, g, k, f, dt)`。`k`・`f`・`dt` の NULL を含む。
type Seed = (u64, &'static str, Option<i32>, Option<bool>, Option<i32>);

const SEEDS: [Seed; 8] = [
    (1, "a", Some(3), Some(true), Some(19_001)),
    (2, "a", Some(1), Some(true), Some(19_000)),
    (3, "a", None, None, None),
    (4, "b", Some(5), Some(false), Some(19_005)),
    (5, "b", Some(2), Some(true), Some(19_002)),
    (6, "c", None, None, None),
    (7, "d", Some(4), Some(false), Some(19_004)),
    (8, "d", None, Some(false), None),
];

fn insert(storage: &Storage, tenant: &str, seed: &Seed, vis: Visibility) {
    let (id, g, k, f, dt) = *seed;
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(g.to_string()),
            k.map_or(Value::Null, Value::Integer),
            f.map_or(Value::Null, Value::Bool),
            dt.map_or(Value::Null, Value::Date),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn build_core(tag: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(tag);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for seed in &SEEDS {
        insert(&storage, "tenant-a", seed, Visibility::Public);
    }
    // 他テナントの Private 行。見えてしまうと各集計の結果が変わる値を入れる。
    insert(
        &storage,
        "tenant-b",
        &(100, "c", Some(0), Some(false), Some(10_000)),
        Visibility::Private,
    );
    insert(
        &storage,
        "tenant-b",
        &(101, "a", Some(0), Some(false), Some(10_000)),
        Visibility::Private,
    );
    // 辞書順で最大・大文字（lower/upper の結果も変える）になる他テナントの Private 行。
    insert(
        &storage,
        "tenant-b",
        &(102, "ZZZ", Some(1000), Some(true), Some(30_000)),
        Visibility::Private,
    );
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    core.execute_sql(&ctx("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e:?}"))
}

fn code_of(core: &EngineCore, sql: &str) -> &'static str {
    core.execute_sql(&ctx("tenant-a"), sql)
        .expect_err(&format!("query must fail: {sql}"))
        .wire_code()
}

/// 単一行・単一列の結果セル。
fn one(core: &EngineCore, sql: &str) -> Cell {
    let r = run(core, sql);
    assert_eq!(r.rows.len(), 1, "{sql}");
    r.rows[0].cells[0].clone()
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

/// `GROUP BY g` の結果を `(g, 残りのセル)` の並びで返す。
fn groups(core: &EngineCore, sql: &str) -> Vec<(String, Vec<Cell>)> {
    run(core, sql)
        .rows
        .iter()
        .map(|row| {
            let g = match &row.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected text key, got {other:?}"),
            };
            (g, row.cells[1..].to_vec())
        })
        .collect()
}

#[test]
fn arithmetic_arguments() {
    let (core, path) = build_core("sql57-expr-arith");
    let _g = CleanupGuard(path);
    assert_eq!(one(&core, "SELECT sum(k * 2) FROM t"), Cell::Float(30.0));
    assert_eq!(one(&core, "SELECT avg(k + 1) FROM t"), Cell::Float(4.0));
    assert_eq!(one(&core, "SELECT max(k * 2) FROM t"), Cell::Float(10.0));
    // 単項マイナス（集計引数の中でも受理する）。
    assert_eq!(one(&core, "SELECT sum(-k) FROM t"), Cell::Float(-15.0));
    assert_eq!(one(&core, "SELECT sum(abs(-k)) FROM t"), Cell::Float(15.0));
    assert_eq!(
        one(&core, "SELECT count(DISTINCT (k + 1)) FROM t"),
        Cell::Integer(5)
    );
    assert_eq!(
        one(&core, "SELECT count(DISTINCT -k) FROM t"),
        Cell::Integer(5)
    );
}

#[test]
fn function_arguments() {
    let (core, path) = build_core("sql57-expr-func");
    let _g = CleanupGuard(path);
    assert_eq!(one(&core, "SELECT max(lower(g)) FROM t"), text("d"));
    assert_eq!(one(&core, "SELECT min(upper(g)) FROM t"), text("A"));
    assert_eq!(
        one(&core, "SELECT count(lower(g)) FROM t"),
        Cell::Integer(8)
    );
    assert_eq!(
        one(&core, "SELECT count(DISTINCT lower(g)) FROM t"),
        Cell::Integer(4)
    );
    assert_eq!(
        one(&core, "SELECT count(DISTINCT upper(g)) FROM t"),
        Cell::Integer(4)
    );
}

#[test]
fn case_arguments() {
    let (core, path) = build_core("sql57-expr-case");
    let _g = CleanupGuard(path);
    assert_eq!(
        one(&core, "SELECT count(CASE WHEN k > 2 THEN 'x' END) FROM t"),
        Cell::Integer(3)
    );
    assert_eq!(
        one(
            &core,
            "SELECT count(CASE WHEN k > 2 THEN 'x' ELSE NULL END) FROM t"
        ),
        Cell::Integer(3)
    );
    assert_eq!(
        one(&core, "SELECT count(CAST(k AS boolean)) FROM t"),
        Cell::Integer(5)
    );
    assert_eq!(
        one(&core, "SELECT max(CASE WHEN k > 2 THEN g END) FROM t"),
        text("d")
    );
    assert_eq!(
        one(
            &core,
            "SELECT count(DISTINCT CASE WHEN k > 2 THEN g END) FROM t"
        ),
        Cell::Integer(3)
    );
    // Bool 式（CAST の結果）。値が全て真のため DISTINCT は 1。
    assert_eq!(
        one(&core, "SELECT count(DISTINCT CAST(k AS boolean)) FROM t"),
        Cell::Integer(1)
    );
}

#[test]
fn datetime_arguments() {
    let (core, path) = build_core("sql57-expr-datetime");
    let _g = CleanupGuard(path);
    assert_eq!(one(&core, "SELECT min(dt + 1) FROM t"), Cell::Date(19_001));
    assert_eq!(one(&core, "SELECT max(dt + 1) FROM t"), Cell::Date(19_006));
    assert_eq!(
        one(&core, "SELECT count(DISTINCT (dt + 1)) FROM t"),
        Cell::Integer(5)
    );
    // Timestamp／TimestampTz／Time 式（CAST の結果）。
    assert_eq!(
        one(&core, "SELECT min(CAST(dt AS timestamp)) FROM t"),
        Cell::Timestamp(19_000 * 86_400_000_000)
    );
    assert_eq!(
        one(&core, "SELECT max(CAST(dt AS timestamp)) FROM t"),
        Cell::Timestamp(19_005 * 86_400_000_000)
    );
    assert_eq!(
        one(&core, "SELECT count(DISTINCT CAST(dt AS timestamp)) FROM t"),
        Cell::Integer(5)
    );
    assert_eq!(
        one(&core, "SELECT count(CAST(dt AS timestamptz)) FROM t"),
        Cell::Integer(5)
    );
    assert_eq!(
        one(&core, "SELECT max(CAST(dt AS timestamptz)) FROM t"),
        Cell::TimestampTz(19_005 * 86_400_000_000)
    );
    assert_eq!(
        one(
            &core,
            "SELECT max(CAST(CAST(dt AS timestamp) AS time)) FROM t"
        ),
        Cell::Time(0)
    );
}

#[test]
fn group_by_results() {
    let (core, path) = build_core("sql57-expr-group");
    let _g = CleanupGuard(path);
    let mut rows = groups(
        &core,
        "SELECT g, max(lower(g)), count(CASE WHEN k > 2 THEN 'x' END), sum(k * 2) FROM t GROUP BY g",
    );
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        rows,
        vec![
            (
                "a".to_string(),
                vec![text("a"), Cell::Integer(1), Cell::Float(8.0)]
            ),
            (
                "b".to_string(),
                vec![text("b"), Cell::Integer(1), Cell::Float(14.0)]
            ),
            (
                "c".to_string(),
                vec![text("c"), Cell::Integer(0), Cell::Null]
            ),
            (
                "d".to_string(),
                vec![text("d"), Cell::Integer(1), Cell::Float(8.0)]
            ),
        ]
    );
}

#[test]
fn aggregate_in_expression_context() {
    let (core, path) = build_core("sql57-expr-context");
    let _g = CleanupGuard(path);
    let mut rows = groups(
        &core,
        "SELECT g, count(*) FROM t GROUP BY g HAVING max(lower(g)) > 'b'",
    );
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        vec!["c", "d"]
    );
}

#[test]
fn rejected_forms_have_stable_wire_codes() {
    let (core, path) = build_core("sql57-expr-reject");
    let _g = CleanupGuard(path);
    // 構文: 不完全な式・入れ子集計・比較式の裸の引数。
    assert_eq!(code_of(&core, "SELECT sum(k * ) FROM t"), "42601");
    assert_eq!(code_of(&core, "SELECT sum(count(*)) FROM t"), "42601");
    assert_eq!(code_of(&core, "SELECT count(k > 1) FROM t"), "42601");
    // 未知の列。RLS 内部列は式経由でも解決できない（テナント境界）。
    assert_eq!(code_of(&core, "SELECT sum(nope * 2) FROM t"), "42703");
    assert_eq!(code_of(&core, "SELECT max(lower(nope)) FROM t"), "42703");
    assert_eq!(
        code_of(&core, "SELECT count(CASE WHEN nope > 1 THEN 1 END) FROM t"),
        "42703"
    );
    assert_eq!(
        code_of(&core, "SELECT max(lower(tenant_id)) FROM t"),
        "42703"
    );
    assert_eq!(code_of(&core, "SELECT count(visibility) FROM t"), "42703");
    assert_eq!(
        code_of(&core, "SELECT count(lower(visibility)) FROM t"),
        "42703"
    );
    // 該当シグネチャの関数が無い。
    assert_eq!(code_of(&core, "SELECT sum(lower(g)) FROM t"), "42883");
    assert_eq!(
        code_of(&core, "SELECT avg(CASE WHEN k > 2 THEN 'a' END) FROM t"),
        "42883"
    );
    assert_eq!(
        code_of(&core, "SELECT min(CAST(k AS boolean)) FROM t"),
        "42883"
    );
    assert_eq!(code_of(&core, "SELECT max(no_such_fn(k)) FROM t"), "42883");
    assert_eq!(code_of(&core, "SELECT variance(lower(g)) FROM t"), "42883");
    // 評価時エラーは部分結果を返さず失敗する。
    assert_eq!(code_of(&core, "SELECT sum(k / 0) FROM t"), "22012");
}

#[test]
fn other_tenant_rows_do_not_affect_results() {
    let (core, path) = build_core("sql57-expr-rls");
    let _g = CleanupGuard(path);
    // tenant-b の Private 行（g = 'ZZZ'、k = 1000）は tenant-a から見えない。
    assert_eq!(one(&core, "SELECT max(lower(g)) FROM t"), text("d"));
    assert_eq!(one(&core, "SELECT max(upper(g)) FROM t"), text("D"));
    assert_eq!(
        one(&core, "SELECT count(DISTINCT lower(g)) FROM t"),
        Cell::Integer(4)
    );
    assert_eq!(one(&core, "SELECT sum(k * 2) FROM t"), Cell::Float(30.0));
    assert_eq!(one(&core, "SELECT max(dt + 1) FROM t"), Cell::Date(19_006));
}
