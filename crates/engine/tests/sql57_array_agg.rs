//! `array_agg([DISTINCT] <式> [ORDER BY ...])`（集計内 `ORDER BY`・`DISTINCT` 対応）の結合テスト
//! （SQL-57・Issue #1801。関連: SQL-13・SQL-14・SQL-25 (c)・SQL-56・TABLE-35・WIRE-13・RLS-7/RLS-8）。
//!
//! 期待値は PostgreSQL 17 の意味論（NULL 入力は NULL 要素・入力なしは NULL・`DISTINCT` は昇順で
//! NULL を 1 個・`DISTINCT` で引数にない `ORDER BY` は `42P10`・型を決められない引数は `42804`）に
//! 従う固定値。`unique_db_path`＋`CleanupGuard`、`EngineCore::execute_sql` の production 経路で
//! 検証し、他テナントの Private 行が結果・`54000` の有無へ影響しないこと（RLS）と、
//! `wire_code` のエラー契約を固定する。

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::collation::Collation;
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::numeric::Decimal;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{ArrayValue, Value};
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::storage::{Storage, Visibility};
use engine::uuid::Uuid;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "t";
const BIG: &str = "big";
const MIB: usize = 1024 * 1024;

fn schema() -> TableSchema {
    let col = |name: &str, ty: ColumnType, c: Option<Collation>| ColumnDef {
        collation: c,
        ..ColumnDef::new(name, ty, true)
    };
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            col("g", ColumnType::Text, None),
            col("s", ColumnType::Text, None),
            col("k", ColumnType::Integer, None),
            col("ic", ColumnType::Text, Some(Collation::UndIcu)),
            col("bg", ColumnType::BigInt, None),
            col("r", ColumnType::Real, None),
            col("d", ColumnType::Double, None),
            col("dt", ColumnType::Date, None),
            col("ts", ColumnType::Timestamp, None),
            col("u", ColumnType::Uuid, None),
            col(
                "n",
                ColumnType::Numeric {
                    precision: 10,
                    scale: 2,
                },
                None,
            ),
            col("b", ColumnType::Bytea, None),
            col("j", ColumnType::Json, None),
            col("jb", ColumnType::Jsonb, None),
            col("bo", ColumnType::Boolean, None),
            col("sm", ColumnType::SmallInt, None),
            col("ch", ColumnType::Bpchar(4), None),
            col("v", ColumnType::Varchar(Some(20)), None),
            col("tm", ColumnType::Time, None),
            col(
                "arr",
                ColumnType::Array(ArrayType::new(ArrayElemType::Integer, 4).expect("array type")),
                None,
            ),
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

/// `(id, g, s, k)`。`s`・`k` の NULL を含む。
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
            Value::Text(
                ["b", "a", "C", "B", "a", "B", "c", "A"][(id as usize - 1) % 8].to_string(),
            ),
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
            Value::Integer(id as i32),
            Value::Text("ab  ".to_string()),
            opt(s),
            Value::Time(idn),
            Value::Array(ArrayValue::Integer(vec![Some(id as i32)])),
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
    // 要素数の上限（1,024）の境界: `fit` は自テナントの可視行がちょうど 1,024 件（他テナントの
    // Private 行 10 件は上限判定に影響しない）、`over` は 1,025 件。
    let mut id = 1_000u64;
    for _ in 0..1_024 {
        insert_big(&storage, "tenant-a", id, "fit", 1, Visibility::Public);
        id += 1;
    }
    for _ in 0..10 {
        insert_big(&storage, "tenant-b", id, "fit", 1, Visibility::Private);
        id += 1;
    }
    for _ in 0..1_025 {
        insert_big(&storage, "tenant-a", id, "over", 1, Visibility::Public);
        id += 1;
    }
    // 可変長本文の累計上限（4 MiB）の境界: `ok` は 2 MiB + 2 MiB = ちょうど 4 MiB、`pay` は 3 MiB + 2 MiB。
    insert_big(&storage, "tenant-a", 1, "ok", 2 * MIB, Visibility::Public);
    insert_big(&storage, "tenant-a", 2, "ok", 2 * MIB, Visibility::Public);
    insert_big(&storage, "tenant-a", 3, "pay", 3 * MIB, Visibility::Public);
    insert_big(&storage, "tenant-a", 4, "pay", 2 * MIB, Visibility::Public);
    insert_big(&storage, "tenant-b", 5, "ok", 3 * MIB, Visibility::Private);
    // GROUP BY 全体の予算（16 MiB）: 3 MiB の単一要素グループが 6 つ（合計 18 MiB）。
    for h in 0..6u64 {
        insert_big(
            &storage,
            "tenant-a",
            10 + h,
            &format!("h{h}"),
            3 * MIB,
            Visibility::Public,
        );
    }
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

/// 単一行・単一列の結果の配列（NULL は `None`）。
fn arr(core: &EngineCore, sql: &str) -> Option<ArrayValue> {
    arr_as(core, "tenant-a", sql)
}

fn arr_as(core: &EngineCore, tenant: &str, sql: &str) -> Option<ArrayValue> {
    let r = core
        .execute_sql(&ctx(tenant), sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e:?}"));
    assert_eq!(r.rows.len(), 1, "{sql}");
    match &r.rows[0].cells[0] {
        Cell::Array(a) => Some(a.clone()),
        Cell::Null => None,
        other => panic!("expected array or NULL, got {other:?}"),
    }
}

fn ints(v: &[Option<i32>]) -> Option<ArrayValue> {
    Some(ArrayValue::Integer(v.to_vec()))
}

fn texts(v: &[Option<&str>]) -> Option<ArrayValue> {
    Some(ArrayValue::Text(
        v.iter().map(|s| s.map(str::to_string)).collect(),
    ))
}

fn ddl(core: &EngineCore, sql: &str) {
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("ddl must succeed: {sql}: {e:?}"));
}

/// 入力順（行の処理順）で NULL 要素を含む配列を返し、結果列の型は入力の型の配列として公告される。
#[test]
fn basic_keeps_input_order_with_null_elements_and_advertises_array_type() {
    let (core, path) = build_core("sql57-arr-basic");
    let _g = CleanupGuard(path);
    assert_eq!(
        arr(&core, "SELECT array_agg(k) FROM t"),
        ints(&[
            Some(3),
            Some(1),
            Some(2),
            Some(5),
            None,
            Some(1),
            Some(4),
            Some(9)
        ])
    );
    let r = run(&core, "SELECT array_agg(k) FROM t");
    match &r.columns[0] {
        ColumnMeta::Computed {
            name,
            ty: Some(ColumnType::Array(at)),
        } => {
            assert_eq!(name, "array_agg");
            assert_eq!(at.elem(), ArrayElemType::Integer);
        }
        other => panic!("unexpected column meta: {other:?}"),
    }
    assert_eq!(
        arr(&core, "SELECT array_agg(s) FROM t WHERE g = 'a'"),
        texts(&[Some("x"), None, Some("y"), Some("x")])
    );
}

/// 入力行が無ければ NULL（空配列ではない）。全行 NULL は NULL 要素の配列。
#[test]
fn empty_input_is_null_and_all_null_is_null_elements() {
    let (core, path) = build_core("sql57-arr-empty");
    let _g = CleanupGuard(path);
    assert_eq!(
        arr(&core, "SELECT array_agg(k) FROM t WHERE g = 'none'"),
        None
    );
    assert_eq!(
        arr(&core, "SELECT array_agg(s) FROM t WHERE g = 'c'"),
        texts(&[None])
    );
}

/// 内側 `ORDER BY`: 方向・NULL 位置・複数キー・引数以外のキー・照合付きキー・同値キーの入力順。
#[test]
fn inner_order_by_directions_nulls_and_keys() {
    let (core, path) = build_core("sql57-arr-order");
    let _g = CleanupGuard(path);
    let q = |tail: &str| {
        arr(
            &core,
            &format!("SELECT array_agg(k ORDER BY {tail}) FROM t"),
        )
    };
    assert_eq!(
        q("k"),
        ints(&[
            Some(1),
            Some(1),
            Some(2),
            Some(3),
            Some(4),
            Some(5),
            Some(9),
            None
        ])
    );
    assert_eq!(
        q("k DESC NULLS LAST"),
        ints(&[
            Some(9),
            Some(5),
            Some(4),
            Some(3),
            Some(2),
            Some(1),
            Some(1),
            None
        ])
    );
    assert_eq!(q("k DESC").map(|a| alen(&a)), Some(8));
    // 引数以外の列・式のキー。
    assert_eq!(
        arr(&core, "SELECT array_agg(s ORDER BY k) FROM t"),
        texts(&[
            None,
            None,
            Some("y"),
            Some("x"),
            Some("x"),
            Some("z"),
            Some("x"),
            Some("w")
        ])
    );
    assert_eq!(
        arr(
            &core,
            "SELECT array_agg(k ORDER BY k + 0 DESC, id DESC) FROM t WHERE k > 0"
        ),
        ints(&[
            Some(9),
            Some(5),
            Some(4),
            Some(3),
            Some(2),
            Some(1),
            Some(1)
        ])
    );
    // 同値キーは入力順を保つ（k = 1 の id 2 と id 6 は s が NULL。id で識別できる列で確認）。
    assert_eq!(
        arr(&core, "SELECT array_agg(g ORDER BY k) FROM t WHERE k = 1"),
        texts(&[Some("a"), Some("c")])
    );
    // 照合付き（ICU）の text 列キー（バイト列順なら大文字が先になる）。
    assert_eq!(
        arr(
            &core,
            "SELECT array_agg(k ORDER BY ic, id) FROM t WHERE k IS NOT NULL"
        ),
        ints(&[
            Some(1),
            Some(9),
            Some(3),
            Some(5),
            Some(1),
            Some(4),
            Some(2)
        ])
    );
}

/// `DISTINCT`: 昇順で NULL は 1 個だけ末尾、`ORDER BY` は引数のみ、引数以外は `42P10`。
#[test]
fn distinct_dedups_sorts_and_rejects_foreign_order_keys() {
    let (core, path) = build_core("sql57-arr-distinct");
    let _g = CleanupGuard(path);
    assert_eq!(
        arr(&core, "SELECT array_agg(DISTINCT k) FROM t"),
        ints(&[Some(1), Some(2), Some(3), Some(4), Some(5), Some(9), None])
    );
    assert_eq!(
        arr(&core, "SELECT array_agg(DISTINCT k ORDER BY k DESC) FROM t"),
        ints(&[None, Some(9), Some(5), Some(4), Some(3), Some(2), Some(1)])
    );
    assert_eq!(
        arr(&core, "SELECT array_agg(DISTINCT s) FROM t"),
        texts(&[Some("w"), Some("x"), Some("y"), Some("z"), None])
    );
    assert_eq!(
        code_of(&core, "SELECT array_agg(DISTINCT k ORDER BY s) FROM t"),
        "42P10"
    );
    // 列名 `distinct` の読みは壊さない（列が無いので未定義列）。
    assert_eq!(code_of(&core, "SELECT array_agg(distinct) FROM t"), "42703");
}

/// 重複が多い入力でも、重複除去後が上限以下なら成功する（重複行は件数の予算を消費しない）。
#[test]
fn distinct_duplicates_do_not_consume_the_element_limit() {
    let (core, path) = build_core("sql57-arr-distinct-dup");
    let _g = CleanupGuard(path);
    assert_eq!(
        arr(
            &core,
            "SELECT array_agg(DISTINCT s) FROM big WHERE g = 'over'"
        ),
        texts(&[Some("q")])
    );
    assert_eq!(
        code_of(&core, "SELECT array_agg(s) FROM big WHERE g = 'over'"),
        "54000"
    );
}

/// `GROUP BY` と併用でき、`ORDER BY` なしでも行の処理順になる（キー順の列挙形を使わない）。
#[test]
fn group_by_with_and_without_order_by() {
    let (core, path) = build_core("sql57-arr-group");
    let _g = CleanupGuard(path);
    let r = run(
        &core,
        "SELECT g, array_agg(s ORDER BY k) FROM t GROUP BY g ORDER BY g",
    );
    let got: Vec<(String, ArrayValue)> = r
        .rows
        .iter()
        .map(|row| match (&row.cells[0], &row.cells[1]) {
            (Cell::Text(g), Cell::Array(a)) => (g.clone(), a.clone()),
            other => panic!("unexpected row: {other:?}"),
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (
                "a".to_string(),
                ArrayValue::Text(vec![
                    None,
                    Some("y".to_string()),
                    Some("x".to_string()),
                    Some("x".to_string())
                ])
            ),
            (
                "b".to_string(),
                ArrayValue::Text(vec![
                    Some("x".to_string()),
                    Some("z".to_string()),
                    Some("w".to_string())
                ])
            ),
            ("c".to_string(), ArrayValue::Text(vec![None])),
        ]
    );
    let r = run(&core, "SELECT g, array_agg(s) FROM t GROUP BY g ORDER BY g");
    assert_eq!(
        r.rows[0].cells[1],
        Cell::Array(ArrayValue::Text(vec![
            Some("x".to_string()),
            None,
            Some("y".to_string()),
            Some("x".to_string())
        ]))
    );
    // 索引経路・cold/hot で結果が変わらない。
    let first = format!(
        "{:?}",
        run(&core, "SELECT g, array_agg(k) FROM t GROUP BY g ORDER BY g")
    );
    let second = format!(
        "{:?}",
        run(&core, "SELECT g, array_agg(k) FROM t GROUP BY g ORDER BY g")
    );
    assert_eq!(first, second);
    let mut session = engine::sql::mode::SessionState::default();
    let plan = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT g, array_agg(k) FROM t GROUP BY g",
        )
        .expect("explain");
    let text = format!("{plan:?}");
    assert!(!text.contains("enumeration"), "{text}");
}

/// 列経路の要素型: ネイティブ値のまま配列になる（bigint は 2^53 を超えても正確）。
#[test]
fn column_element_types_are_preserved() {
    let (core, path) = build_core("sql57-arr-types");
    let _g = CleanupGuard(path);
    let q = |col: &str| {
        arr(
            &core,
            &format!("SELECT array_agg({col} ORDER BY id) FROM t WHERE id <= 2"),
        )
    };
    assert_eq!(q("bg"), Some(ArrayValue::BigInt(vec![Some(BIG_INT), None])));
    assert_eq!(q("r"), Some(ArrayValue::Real(vec![Some(0.5), Some(1.0)])));
    assert_eq!(q("d"), Some(ArrayValue::Double(vec![Some(1.5), Some(3.0)])));
    assert_eq!(
        q("dt"),
        Some(ArrayValue::Date(vec![Some(19_001), Some(19_002)]))
    );
    assert_eq!(
        q("ts"),
        Some(ArrayValue::Timestamp(vec![
            Some(1_700_000_000_000_001),
            Some(1_700_000_000_000_002)
        ]))
    );
    assert_eq!(
        q("u"),
        Some(ArrayValue::Uuid(vec![
            Some(Uuid::from_bytes([1; 16])),
            Some(Uuid::from_bytes([2; 16]))
        ]))
    );
    assert_eq!(
        q("n"),
        Some(ArrayValue::Numeric(vec![
            Some(Decimal::from_parts(105, 2).expect("decimal")),
            Some(Decimal::from_parts(205, 2).expect("decimal"))
        ]))
    );
    assert_eq!(
        q("b"),
        Some(ArrayValue::Bytea(vec![Some(vec![1]), Some(vec![2])]))
    );
    assert_eq!(
        q("j"),
        Some(ArrayValue::Json(vec![
            Some("{\"a\": 1}".to_string()),
            Some("{\"a\": 2}".to_string())
        ]))
    );
    assert!(matches!(q("jb"), Some(ArrayValue::Jsonb(v)) if v.len() == 2));
    assert_eq!(
        q("bo"),
        Some(ArrayValue::Bool(vec![Some(false), Some(true)]))
    );
    assert_eq!(q("v"), texts(&[Some("x"), None]));
    // 式経路: `ARRAY[<式>]` と同じ型規則。
    assert_eq!(
        arr(
            &core,
            "SELECT array_agg(upper(s) ORDER BY id) FROM t WHERE id <= 3"
        ),
        texts(&[Some("X"), None, Some("Y")])
    );
    assert_eq!(
        arr(
            &core,
            "SELECT array_agg(k::bigint ORDER BY id) FROM t WHERE id <= 2"
        ),
        Some(ArrayValue::BigInt(vec![Some(3), Some(1)]))
    );
}

/// 順序・等価の演算子を使う型の `DISTINCT`／`ORDER BY`（numeric・uuid・日時・bool・bigint）。
#[test]
fn distinct_and_order_by_work_for_comparable_column_types() {
    let (core, path) = build_core("sql57-arr-cmp");
    let _g = CleanupGuard(path);
    assert_eq!(
        arr(&core, "SELECT array_agg(DISTINCT bo) FROM t"),
        Some(ArrayValue::Bool(vec![Some(false), Some(true)]))
    );
    assert_eq!(
        arr(
            &core,
            "SELECT array_agg(bg ORDER BY bg DESC NULLS LAST) FROM t WHERE id <= 2"
        ),
        Some(ArrayValue::BigInt(vec![Some(BIG_INT), None]))
    );
    assert!(matches!(
        arr(&core, "SELECT array_agg(DISTINCT n) FROM t"),
        Some(ArrayValue::Numeric(v)) if v.len() == 8
    ));
    assert!(matches!(
        arr(&core, "SELECT array_agg(DISTINCT u ORDER BY u DESC) FROM t"),
        Some(ArrayValue::Uuid(v)) if v.len() == 8
    ));
}

/// 未対応の要素型は `0A000`（fail-closed）。
#[test]
fn unsupported_element_types_are_rejected() {
    let (core, path) = build_core("sql57-arr-unsupported");
    let _g = CleanupGuard(path);
    for col in ["sm", "ch", "tm", "embedding", "arr"] {
        assert_eq!(
            code_of(&core, &format!("SELECT array_agg({col}) FROM t")),
            "0A000",
            "{col}"
        );
    }
    // 疑似列 id。
    assert_eq!(code_of(&core, "SELECT array_agg(id) FROM t"), "0A000");
}

/// 引数の誤りと、順序・等価の演算子が無い型。
#[test]
fn argument_shape_and_type_errors_are_rejected_fail_closed() {
    let (core, path) = build_core("sql57-arr-errors");
    let _g = CleanupGuard(path);
    assert_eq!(code_of(&core, "SELECT array_agg(k, s) FROM t"), "42883");
    assert_eq!(code_of(&core, "SELECT array_agg() FROM t"), "42601");
    assert_eq!(code_of(&core, "SELECT array_agg(*) FROM t"), "42601");
    assert_eq!(code_of(&core, "SELECT array_agg(NULL) FROM t"), "42804");
    assert_eq!(code_of(&core, "SELECT array_agg('a') FROM t"), "42804");
    assert_eq!(code_of(&core, "SELECT array_agg(nope) FROM t"), "42703");
    // json（原文保存）には等価・順序の演算子が無い。
    assert_eq!(
        code_of(&core, "SELECT array_agg(j ORDER BY j) FROM t"),
        "42883"
    );
    assert_eq!(
        code_of(&core, "SELECT array_agg(DISTINCT j) FROM t"),
        "42883"
    );
    // 並べ替えない json の集約は成功する。
    assert!(arr(&core, "SELECT array_agg(j) FROM t").is_some());
    // 非集計関数への内側 ORDER BY は従来どおり `42601`。
    assert_eq!(code_of(&core, "SELECT sum(k ORDER BY k) FROM t"), "42601");
    // `ORDER BY` のキー数の上限。
    let keys = vec!["k"; 40].join(", ");
    assert_eq!(
        code_of(
            &core,
            &format!("SELECT array_agg(k ORDER BY {keys}) FROM t")
        ),
        "54000"
    );
}

/// 上限: 要素数（1,024）・可変長本文の累計（4 MiB）・GROUP BY 全体の予算（16 MiB）。
/// 他テナントの Private 行は結果にも `54000` の有無にも影響しない（RLS）。
#[test]
fn resource_limits_are_exact_and_independent_of_other_tenants() {
    let (core, path) = build_core("sql57-arr-limit");
    let _g = CleanupGuard(path);
    // 可視行がちょうど 1,024 件（他テナントの Private 行 10 件は数えない）。
    assert_eq!(
        arr(&core, "SELECT array_agg(s) FROM big WHERE g = 'fit'").map(|a| alen(&a)),
        Some(1_024)
    );
    assert_eq!(
        code_of(&core, "SELECT array_agg(s) FROM big WHERE g = 'over'"),
        "54000"
    );
    // tenant-b の視点では Public の 1,024 件に自分の Private 行 10 件が加わるため超過する
    // （上限は可視行だけで決まる）。
    let b = ctx("tenant-b");
    assert_eq!(
        core.execute_sql(&b, "SELECT array_agg(s) FROM big WHERE g = 'fit'")
            .expect_err("tenant-b sees 1,034 rows")
            .wire_code(),
        "54000"
    );
    // 本文の累計: ちょうど 4 MiB は成功、1 バイトでも超えれば 54000。
    assert_eq!(
        arr(&core, "SELECT array_agg(s) FROM big WHERE g = 'ok'").map(|a| alen(&a)),
        Some(2)
    );
    assert_eq!(
        code_of(&core, "SELECT array_agg(s) FROM big WHERE g = 'pay'"),
        "54000"
    );
    // GROUP BY 全体の予算（6 グループ × 3 MiB）。
    assert_eq!(
        code_of(
            &core,
            "SELECT g, array_agg(s) FROM big WHERE g LIKE 'h%' GROUP BY g"
        ),
        "54000"
    );
}

/// 他テナントの Private 行は結果に入らない。
#[test]
fn other_tenant_private_rows_never_appear() {
    let (core, path) = build_core("sql57-arr-rls");
    let _g = CleanupGuard(path);
    let a = arr(&core, "SELECT array_agg(s ORDER BY s) FROM t").expect("array");
    assert_eq!(
        a,
        ArrayValue::Text(
            ["w", "x", "x", "x", "y", "z"]
                .iter()
                .map(|s| Some(s.to_string()))
                .chain([None, None])
                .collect()
        )
    );
    let b = arr_as(&core, "tenant-b", "SELECT array_agg(s ORDER BY s) FROM t").expect("array");
    assert!(format!("{b:?}").contains("SECRET"));
    assert!(!format!("{a:?}").contains("SECRET"));
}

/// 対象外の経路は明示的に拒否する（成功してはならない）。
#[test]
fn unsupported_contexts_are_rejected_explicitly() {
    let (core, path) = build_core("sql57-arr-contexts");
    let _g = CleanupGuard(path);
    assert_eq!(code_of(&core, "SELECT array_agg(1::int)"), "0A000");
    let c = code_of(&core, "SELECT array_agg(k) OVER () FROM t");
    assert!(c == "42601" || c == "0A000", "{c}");
    assert_eq!(
        code_of(
            &core,
            "SELECT array_agg(a.k) FROM t a JOIN t b ON a.g = b.g"
        ),
        "0A000"
    );
}

/// 配列結果は文レベルの比較規約が無いため、`ORDER BY`／`HAVING` の対象や式の部品にできない。
#[test]
fn array_results_cannot_be_ordered_filtered_or_used_in_expressions() {
    let (core, path) = build_core("sql57-arr-stmt");
    let _g = CleanupGuard(path);
    assert_eq!(
        code_of(
            &core,
            "SELECT g, array_agg(k) AS a FROM t GROUP BY g ORDER BY a"
        ),
        "0A000"
    );
    assert_eq!(
        code_of(
            &core,
            "SELECT g, array_agg(k) AS a FROM t GROUP BY g HAVING a > 1"
        ),
        "0A000"
    );
    // 式の部品として使う形は構文段で拒否される（成功してはならない）。
    let c = code_of(&core, "SELECT cardinality(array_agg(k)) FROM t");
    assert!(c == "42601" || c == "22000" || c == "0A000", "{c}");
}

/// NoSQL 表層の直接構築入口（`BoundAggregateItem::bind`）は `array_agg` を `42601` で拒否する。
#[test]
fn nosql_direct_bind_rejects_array_agg() {
    use engine::sql::allowlist::AggregateFunc;
    use engine::sql::parser::{AggregateTarget, BoundAggregateItem};
    let schema_val = schema();
    let err = BoundAggregateItem::bind(
        AggregateFunc::ArrayAgg,
        AggregateTarget::Column("k".to_string()),
        &schema_val,
    )
    .expect_err("array_agg must not bind through the direct entry");
    assert_eq!(err.wire_code(), "42601");
}

/// ビュー越しの `array_agg`: 値・内側 `ORDER BY` の式も公開列に限定され、別名は物理列へ写って
/// 結果が元表と一致する。評価後射影形ビュー・ビュー本体への `array_agg` は fail-closed。
#[test]
fn views_scope_array_agg_columns_and_keep_results() {
    let (core, path) = build_core("sql57-arr-view");
    let _g = CleanupGuard(path);
    ddl(&core, "CREATE VIEW v AS SELECT g, s, k FROM t");
    ddl(&core, "CREATE VIEW va (grp, val) AS SELECT g, s FROM t");
    ddl(
        &core,
        "CREATE VIEW vb AS SELECT g, count(*) AS n FROM t GROUP BY g",
    );
    assert_eq!(
        arr(&core, "SELECT array_agg(s ORDER BY k) FROM v"),
        arr(&core, "SELECT array_agg(s ORDER BY k) FROM t")
    );
    assert_eq!(
        arr(&core, "SELECT array_agg(val ORDER BY val) FROM va"),
        arr(&core, "SELECT array_agg(s ORDER BY s) FROM t")
    );
    // 公開されていない列（`ic`）は値にも ORDER BY にも使えない。
    assert_eq!(code_of(&core, "SELECT array_agg(ic) FROM v"), "42703");
    assert_eq!(
        code_of(&core, "SELECT array_agg(s ORDER BY ic) FROM v"),
        "42703"
    );
    let c = code_of(&core, "SELECT array_agg(g) FROM vb");
    assert!(c == "42601" || c == "0A000", "{c}");
}

/// 1 次元配列の要素数。
fn alen(a: &ArrayValue) -> usize {
    a.dims().first().copied().map_or(0, |n| n as usize)
}

/// 配列結果の隣にある別の項目を式の `ORDER BY`／`HAVING` で参照しても、行ビューの合成で内部エラーに
/// ならない（配列セルは式から参照できない枠として扱う）。
#[test]
fn expression_order_and_having_next_to_array_agg_still_work() {
    let (core, path) = build_core("sql57-arr-expr-neighbor");
    let _g = CleanupGuard(path);
    let r = run(
        &core,
        "SELECT g, array_agg(k), count(*) AS c FROM t GROUP BY g ORDER BY lower(g) DESC",
    );
    assert_eq!(r.rows.len(), 3);
    let r = run(
        &core,
        "SELECT g, array_agg(k), count(*) AS c FROM t GROUP BY g HAVING abs(c) > 1",
    );
    assert_eq!(r.rows.len(), 2);
    // 配列結果そのものを式で参照する形は束縛時に拒否される（内部エラーにならない）。
    assert_eq!(
        code_of(
            &core,
            "SELECT g, array_agg(k) AS a FROM t GROUP BY g HAVING abs(a) > 1"
        ),
        "0A000"
    );
}

/// `array_agg` を含む `GROUP BY` 本体のビューは作成できない（fail-closed）。単一行集計の
/// 索引経路（`WHERE g = ...`）は cold/hot で結果が変わらない。
#[test]
fn view_body_is_rejected_and_index_path_is_stable() {
    let (core, path) = build_core("sql57-arr-view-body");
    let _g = CleanupGuard(path);
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "CREATE VIEW vbad AS SELECT g, array_agg(k) FROM t GROUP BY g",
        )
        .expect_err("array_agg must not be allowed in a view body");
    assert!(matches!(err.wire_code(), "0A000" | "42601"), "{err:?}");
    let sql = "SELECT array_agg(s) FROM t WHERE g = 'a'";
    let cold = arr(&core, sql);
    assert_eq!(cold, texts(&[Some("x"), None, Some("y"), Some("x")]));
    assert_eq!(arr(&core, sql), cold);
    assert_eq!(arr(&core, sql), cold);
}

/// 位置指定の `ORDER BY`（`ORDER BY 2`）でも配列結果は並べ替えに使えず `0A000`（名前指定と同じ。
/// 未定義の比較で行が黙って誤順序になるのを防ぐ）。`DISTINCT`・集合演算の挙動も固定する。
#[test]
fn array_results_reject_positional_order_by_and_stay_correct_elsewhere() {
    let (core, path) = build_core("sql57-arr-pos");
    let _g = CleanupGuard(path);
    for sql in [
        "SELECT g, array_agg(k) FROM t GROUP BY g ORDER BY 2",
        "SELECT g, array_agg(k) FROM t GROUP BY g ORDER BY 2 DESC",
        "SELECT array_agg(k), g FROM t GROUP BY g ORDER BY 1",
    ] {
        assert_eq!(code_of(&core, sql), "0A000", "{sql}");
    }
    // 位置指定でも配列以外の列は従来どおり並べ替えられる。
    let r = run(
        &core,
        "SELECT g, array_agg(k) FROM t GROUP BY g ORDER BY 1 DESC",
    );
    assert_eq!(r.rows.len(), 3);
    assert_eq!(r.rows[0].cells[0], Cell::Text("c".to_string()));
    // `SELECT DISTINCT` は配列結果と併用できない（fail-closed）。
    assert_eq!(
        code_of(&core, "SELECT DISTINCT array_agg(k) FROM t"),
        "42601"
    );
    // 集合演算は配列の正準バイト列で同値判定する（同じ配列は 1 行へ、EXCEPT は空）。
    let u = run(
        &core,
        "SELECT g, array_agg(k) FROM t GROUP BY g UNION SELECT g, array_agg(k) FROM t GROUP BY g",
    );
    assert_eq!(u.rows.len(), 3);
    let e = run(
        &core,
        "SELECT g, array_agg(k) FROM t GROUP BY g EXCEPT SELECT g, array_agg(k) FROM t GROUP BY g",
    );
    assert!(e.rows.is_empty());
}

/// Bugbot 指摘: 引数が uuid の式（裸の列でない）でも、`DISTINCT` と引数一致の内側 `ORDER BY` が
/// 素の列と同じく動く。numeric・bytea の式要素は列と同じく `0A000`（fail-closed）。
#[test]
fn distinct_and_order_by_accept_uuid_expressions_like_columns() {
    let (core, path) = build_core("sql57-arr-expr-keys");
    let _g = CleanupGuard(path);
    let expr = "CAST(u AS uuid)";
    let by_col = arr(&core, "SELECT array_agg(DISTINCT u) FROM t");
    assert_eq!(
        arr(&core, &format!("SELECT array_agg(DISTINCT {expr}) FROM t")),
        by_col
    );
    let desc_col = arr(&core, "SELECT array_agg(u ORDER BY u DESC) FROM t");
    assert_eq!(
        arr(
            &core,
            &format!("SELECT array_agg({expr} ORDER BY {expr} DESC) FROM t")
        ),
        desc_col
    );
    assert!(arr(
        &core,
        &format!("SELECT array_agg(DISTINCT {expr} ORDER BY {expr} DESC) FROM t")
    )
    .is_some());
    // 引数と異なる uuid の式キーも列キーと同じく並べ替えに使える。
    assert!(arr(&core, "SELECT array_agg(k ORDER BY CAST(u AS uuid)) FROM t").is_some());
    for sql in [
        "SELECT array_agg(DISTINCT CAST(n AS numeric)) FROM t",
        "SELECT array_agg(DISTINCT b) FROM t",
        "SELECT array_agg(DISTINCT CAST(b AS bytea)) FROM t",
    ] {
        assert_eq!(code_of(&core, sql), "0A000", "{sql}");
    }
}
