//! `json_agg`・`jsonb_agg`（集計内 `ORDER BY`・`DISTINCT`・`FILTER` 対応）の結合テスト
//! （SQL-57・Issue #3264。関連: SQL-13・SQL-14・SQL-25・TABLE-25・WIRE-13・RLS-7/RLS-8）。
//!
//! 期待値は PostgreSQL 17 の意味論（要素間は `, `・NULL は `null`・入力なしは NULL・
//! `DISTINCT` は昇順で NULL を 1 個・`DISTINCT` で引数にない `ORDER BY` は `42P10`）に従う固定値。
//! `jsonb_agg` は空白なしの正規形（PG の jsonb 出力との空白差は既知差分）。
//! `EngineCore::execute_sql` の production 経路で検証し、他テナントの Private 行が結果・
//! `54000` の有無へ影響しないこと（RLS）と、未対応の形の `wire_code` を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::numeric::Decimal;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::storage::{Storage, Visibility};
use engine::uuid::Uuid;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "t";
const BIG: &str = "big";
const MIB: usize = 1024 * 1024;

fn schema() -> TableSchema {
    let col = |name: &str, ty: ColumnType| ColumnDef::new(name, ty, true);
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            col("g", ColumnType::Text),
            col("s", ColumnType::Text),
            col("k", ColumnType::Integer),
            col("bg", ColumnType::BigInt),
            col("r", ColumnType::Real),
            col("d", ColumnType::Double),
            col("dt", ColumnType::Date),
            col("ts", ColumnType::Timestamp),
            col("u", ColumnType::Uuid),
            col(
                "n",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
            ),
            col("b", ColumnType::Bytea),
            col("j", ColumnType::Json),
            col("jb", ColumnType::Jsonb),
            col("bo", ColumnType::Boolean),
            col("tm", ColumnType::Time),
        ],
    )
}

fn big_schema() -> TableSchema {
    TableSchema::new(
        BIG,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("g", ColumnType::Text, true),
            ColumnDef::new("s", ColumnType::Text, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn opt(v: Option<&str>) -> Value {
    v.map_or(Value::Null, |s| Value::Text(s.to_string()))
}

type Seed = (u64, &'static str, Option<&'static str>, Option<i32>);

const SEEDS: [Seed; 8] = [
    (1, "a", Some("x"), Some(3)),
    (2, "a", None, Some(1)),
    (3, "a", Some("y"), Some(2)),
    (4, "b", Some("z"), Some(5)),
    (5, "b", Some("w"), None),
    (6, "c", None, Some(1)),
    (7, "b", Some("x"), Some(4)),
    (8, "a", Some("x"), Some(9)),
];

/// 2^53 + 1（f64 では正確に表せない bigint）。
const BIG_INT: i64 = 9_007_199_254_740_993;

fn insert_t(storage: &Storage, tenant: &str, seed: &Seed, vis: Visibility) {
    let (id, g, s, k) = *seed;
    let idn = id as i64;
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(g.to_string()),
            opt(s),
            k.map_or(Value::Null, Value::Integer),
            if id == 2 {
                Value::Null
            } else if id == 1 {
                Value::BigInt(BIG_INT)
            } else {
                Value::BigInt(idn * 10)
            },
            Value::Real(id as f32 * 0.5),
            Value::Double(id as f64 * 1.5),
            Value::Date(19_000 + id as i32),
            Value::Timestamp(1_700_000_000_000_000 + idn),
            Value::Uuid(Uuid::from_bytes([id as u8; 16])),
            Value::Numeric(Decimal::from_parts(i128::from(id) * 100 + 5, 2).expect("decimal")),
            Value::Bytes(vec![id as u8]),
            Value::Json(format!("{{\"a\": {id}}}")),
            Value::Json(format!("{{\"a\":{id}}}")),
            Value::Bool(id % 2 == 0),
            Value::Time(idn),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

fn insert_big(storage: &Storage, tenant: &str, id: u64, g: &str, len: usize, vis: Visibility) {
    engine::tenant::insert_typed_row(
        storage,
        BIG,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(g.to_string()),
            Value::Text("q".repeat(len)),
        ],
        &OperationId::parse(&format!("big-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert big row");
}

fn build_core(tag: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(tag);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    storage.create_table(&big_schema()).expect("create big");
    for seed in &SEEDS {
        insert_t(&storage, "tenant-a", seed, Visibility::Public);
    }
    // 他テナントの Private 行（tenant-a からは見えてはならない）。
    insert_t(
        &storage,
        "tenant-b",
        &(100, "a", Some("SECRET"), Some(0)),
        Visibility::Private,
    );
    // `two`: 可視は 1 MiB x 2（上限内）。`pay`: 本文の累計が上限（4 MiB）を超える。
    insert_big(&storage, "tenant-a", 1, "two", MIB, Visibility::Public);
    insert_big(&storage, "tenant-a", 2, "two", MIB, Visibility::Public);
    insert_big(&storage, "tenant-a", 3, "pay", 3 * MIB, Visibility::Public);
    insert_big(&storage, "tenant-a", 4, "pay", 2 * MIB, Visibility::Public);
    // 他テナントの大きな Private 行は、自テナントの結果にも `54000` の有無にも影響しない。
    insert_big(&storage, "tenant-b", 5, "two", 3 * MIB, Visibility::Private);
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

/// 単一行・単一列の結果（NULL は `None`）。
fn js(core: &EngineCore, sql: &str) -> Option<String> {
    let r = run(core, sql);
    assert_eq!(r.rows.len(), 1, "{sql}");
    match &r.rows[0].cells[0] {
        Cell::Json(s) => Some(s.clone()),
        Cell::Null => None,
        other => panic!("expected json or NULL, got {other:?}"),
    }
}

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

/// 入力順・NULL 要素・結果列の型と既定列名。
#[test]
fn basic_input_order_null_elements_and_column_meta() {
    let (core, path) = build_core("sql57-js-basic");
    let _g = CleanupGuard(path);
    assert_eq!(
        js(&core, "SELECT json_agg(k) FROM t"),
        s("[3, 1, 2, 5, null, 1, 4, 9]")
    );
    assert_eq!(
        js(&core, "SELECT jsonb_agg(k) FROM t"),
        s("[3,1,2,5,null,1,4,9]")
    );
    assert_eq!(
        js(&core, "SELECT json_agg(s) FROM t WHERE g = 'a'"),
        s("[\"x\", null, \"y\", \"x\"]")
    );
    for (sql, name, ty) in [
        ("SELECT json_agg(k) FROM t", "json_agg", ColumnType::Json),
        ("SELECT jsonb_agg(k) FROM t", "jsonb_agg", ColumnType::Jsonb),
    ] {
        let r = run(&core, sql);
        match &r.columns[0] {
            ColumnMeta::Computed {
                name: n,
                ty: Some(t),
            } => {
                assert_eq!(n, name);
                assert_eq!(*t, ty);
            }
            other => panic!("unexpected column meta: {other:?}"),
        }
    }
}

/// 空集合は NULL。FILTER で全行除外されても NULL。
#[test]
fn empty_input_and_filter_are_null() {
    let (core, path) = build_core("sql57-js-empty");
    let _g = CleanupGuard(path);
    assert_eq!(
        js(&core, "SELECT json_agg(k) FROM t WHERE g = 'none'"),
        None
    );
    assert_eq!(
        js(&core, "SELECT jsonb_agg(k) FROM t WHERE g = 'none'"),
        None
    );
    assert_eq!(
        js(&core, "SELECT json_agg(k) FILTER (WHERE k > 100) FROM t"),
        None
    );
    assert_eq!(
        js(&core, "SELECT json_agg(s) FROM t WHERE g = 'c'"),
        s("[null]")
    );
}

/// GROUP BY あり: グループごとの JSON 配列（末尾 `;` 付きも同じ経路）。
#[test]
fn group_by_groups_and_trailing_semicolon() {
    let (core, path) = build_core("sql57-js-group");
    let _g = CleanupGuard(path);
    let r = run(
        &core,
        "SELECT g, json_agg(k ORDER BY k) FROM t GROUP BY g ORDER BY g;",
    );
    let got: Vec<(Cell, Cell)> = r
        .rows
        .iter()
        .map(|row| (row.cells[0].clone(), row.cells[1].clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (Cell::Text("a".into()), Cell::Json("[1, 2, 3, 9]".into())),
            (Cell::Text("b".into()), Cell::Json("[4, 5, null]".into())),
            (Cell::Text("c".into()), Cell::Json("[1]".into())),
        ]
    );
    assert_eq!(
        js(&core, "SELECT jsonb_agg(k ORDER BY k) FROM t;"),
        s("[1,1,2,3,4,5,9,null]")
    );
}

/// 要素型ごとの表記（bigint の厳密性・numeric の位取り・非有限 float・日時・uuid・bytea・json 系）。
#[test]
fn element_type_renderings() {
    let (core, path) = build_core("sql57-js-types");
    let _g = CleanupGuard(path);
    let q = |col: &str, extra: &str| js(&core, &format!("SELECT json_agg({col}) FROM t {extra}"));
    assert_eq!(
        q("bg", "WHERE id IN (1, 2, 4)"),
        s("[9007199254740993, null, 40]")
    );
    assert_eq!(q("n", "WHERE id <= 2"), s("[1.05, 2.05]"));
    assert_eq!(q("d", "WHERE id IN (2, 3)"), s("[3, 4.5]"));
    assert_eq!(q("bo", "WHERE id <= 2"), s("[false, true]"));
    assert_eq!(q("b", "WHERE id = 1"), s("[\"\\\\x01\"]"));
    assert_eq!(q("j", "WHERE id <= 2"), s("[{\"a\": 1}, {\"a\": 2}]"));
    assert_eq!(q("jb", "WHERE id <= 2"), s("[{\"a\":1}, {\"a\":2}]"));
    assert_eq!(
        js(&core, "SELECT jsonb_agg(j) FROM t WHERE id <= 2"),
        s("[{\"a\":1},{\"a\":2}]")
    );
    assert_eq!(
        js(
            &core,
            "SELECT json_agg(json_build_object('k', k)) FROM t WHERE id <= 2"
        ),
        s("[{\"k\" : 3}, {\"k\" : 1}]")
    );
    // 日時・uuid は引用文字列（値の細部は json_build_object 側の既存試験が固定）。
    for col in ["dt", "ts", "tm", "u"] {
        let v = q(col, "WHERE id = 1").expect("some");
        assert!(v.starts_with("[\"") && v.ends_with("\"]"), "{col}: {v}");
    }
}

/// ORDER BY・DISTINCT・FILTER の併用と `42P10`・json 要素の DISTINCT `42883`。
#[test]
fn order_by_distinct_filter() {
    let (core, path) = build_core("sql57-js-order");
    let _g = CleanupGuard(path);
    assert_eq!(
        js(
            &core,
            "SELECT json_agg(k ORDER BY k DESC NULLS LAST) FROM t"
        ),
        s("[9, 5, 4, 3, 2, 1, 1, null]")
    );
    assert_eq!(
        js(&core, "SELECT json_agg(DISTINCT k) FROM t"),
        s("[1, 2, 3, 4, 5, 9, null]")
    );
    assert_eq!(
        js(&core, "SELECT jsonb_agg(DISTINCT k) FROM t"),
        s("[1,2,3,4,5,9,null]")
    );
    assert_eq!(
        js(
            &core,
            "SELECT json_agg(s ORDER BY k) FILTER (WHERE g = 'a') FROM t"
        ),
        s("[null, \"y\", \"x\", \"x\"]")
    );
    assert_eq!(
        code_of(&core, "SELECT json_agg(DISTINCT k ORDER BY s) FROM t"),
        "42P10"
    );
    assert_eq!(
        code_of(&core, "SELECT json_agg(DISTINCT j) FROM t"),
        "42883"
    );
    assert_eq!(
        js(&core, "SELECT jsonb_agg(DISTINCT jb) FROM t WHERE id <= 2"),
        s("[{\"a\":1},{\"a\":2}]")
    );
}

/// 出力サイズ・本文累計の上限は確保前に `54000`。他テナントの行は影響しない（RLS）。
#[test]
fn resource_limits_and_tenant_isolation() {
    let (core, path) = build_core("sql57-js-limits");
    let _g = CleanupGuard(path);
    // `two`: 可視は 1 MiB x 2。他テナントの 3 MiB は無関係で成功する。
    let r = js(&core, "SELECT json_agg(s) FROM big WHERE g = 'two'").expect("some");
    assert_eq!(r.len(), 2 * MIB + 2 * 2 + 2 + 2);
    // 本文の累計が 4 MiB を超える。
    assert_eq!(
        code_of(&core, "SELECT json_agg(s) FROM big WHERE g = 'pay'"),
        "54000"
    );
    assert_eq!(
        code_of(&core, "SELECT jsonb_agg(s) FROM big WHERE g = 'pay'"),
        "54000"
    );
    // 他テナントの Private 行は結果に現れない。
    let all = js(&core, "SELECT json_agg(s) FROM t").expect("some");
    assert!(!all.contains("SECRET"));
}

/// 未対応の形は fail-closed（`wire_code` を固定）。
#[test]
fn unsupported_forms_are_rejected() {
    let (core, path) = build_core("sql57-js-reject");
    let _g = CleanupGuard(path);
    for (sql, code) in [
        ("SELECT json_agg(1)", "0A000"),
        ("SELECT json_agg(k + 1) FROM t", "0A000"),
        ("SELECT jsonb_agg(k) OVER () FROM t", "42601"),
        ("SELECT json_agg(k, s) FROM t", "42883"),
        ("SELECT json_agg(*) FROM t", "42601"),
        ("SELECT json_agg(embedding) FROM t", "0A000"),
        ("SELECT json_agg(id) FROM t", "0A000"),
        ("SELECT json_agg(NULL) FROM t", "42804"),
        // TIMESTAMPTZ の JSON 化は未対応。確定時の XX000 でなく束縛時に 0A000（Issue #3233）。
        (
            "SELECT json_agg(CAST('2026-01-01 00:00:00+00' AS TIMESTAMPTZ)) FROM t",
            "0A000",
        ),
        (
            "SELECT jsonb_agg(CAST('2026-01-01 00:00:00+00' AS TIMESTAMPTZ)) FROM t",
            "0A000",
        ),
        (
            "SELECT g, json_agg(k) FROM t GROUP BY g ORDER BY 2",
            "0A000",
        ),
    ] {
        assert_eq!(code_of(&core, sql), code, "{sql}");
    }
}
