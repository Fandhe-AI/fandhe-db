//! `vector_dims`・`vector_norm`・`l2_normalize`・`subvector` の結合テスト
//! （SQL-72 (1)(2)(5)(6)・Issue #2012。関連: RLS-7/RLS-8）。
//!
//! `EngineCore::execute_sql` の production 経路で、投影・WHERE・ORDER BY・集約内の式位置、
//! 境界値、`wire_code` のエラー契約、他テナントの Private 行が評価されないこと（RLS）を固定する。
//! 期待値は f32 で正確に表せる値（3-4-5 など）を使い完全一致で比べる。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "t";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(3), true),
            ColumnDef::new("g", ColumnType::Text, true),
            ColumnDef::new("k", ColumnType::Integer, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `(id, embedding, g, k)`。
type Seed = (u64, [f32; 3], &'static str, Option<i32>);

fn insert_t(storage: &Storage, tenant: &str, seed: &Seed, vis: Visibility) {
    let (id, emb, g, k) = *seed;
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(emb.to_vec()),
            Value::Text(g.to_string()),
            k.map_or(Value::Null, Value::Integer),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// tenant-a の Public 行。id 1 は 3-4-5 の直角三角形、id 3 はゼロベクトル、id 4 は k が NULL。
const SEEDS_A: [Seed; 4] = [
    (1, [3.0, 4.0, 0.0], "a", Some(1)),
    (2, [1.0, 2.0, 2.0], "a", Some(2)),
    (3, [0.0, 0.0, 0.0], "b", Some(3)),
    (4, [0.0, 6.0, 8.0], "b", None),
];

/// 他テナントの Private 行。評価されると `subvector(embedding, k, 1)` が start > dim の
/// `22000` になる値（`k = 99`）。見えない行が評価されれば自テナントのクエリが失敗する。
const SEEDS_B: [Seed; 1] = [(100, [1.0, 1.0, 1.0], "a", Some(99))];

fn build_core(tag: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for seed in &SEEDS_A {
        insert_t(&storage, "tenant-a", seed, Visibility::Public);
    }
    for seed in &SEEDS_B {
        insert_t(&storage, "tenant-b", seed, Visibility::Private);
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn exec(core: &EngineCore, sql: &str) -> QueryResult {
    core.execute_sql(&ctx("tenant-a"), sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e:?}"))
}

fn code_of(core: &EngineCore, sql: &str) -> &'static str {
    core.execute_sql(&ctx("tenant-a"), sql)
        .expect_err(&format!("query must fail: {sql}"))
        .wire_code()
}

fn col0(core: &EngineCore, sql: &str) -> Vec<Cell> {
    exec(core, sql)
        .rows
        .iter()
        .map(|r| r.cells[0].clone())
        .collect()
}

fn vec_cell(v: &[f32]) -> Cell {
    Cell::Vector(v.to_vec())
}

#[test]
fn projection_values_and_types() {
    let (core, _g) = build_core("sql72-fn-proj");
    let r = exec(
        &core,
        "SELECT vector_dims(embedding), vector_norm(embedding), l2_normalize(embedding), \
         subvector(embedding, 2, 2) FROM t WHERE k = 1",
    );
    assert_eq!(r.rows.len(), 1);
    let cells = &r.rows[0].cells;
    assert_eq!(cells[0], Cell::SignedInteger(3));
    assert_eq!(cells[1], Cell::Float(5.0));
    assert_eq!(
        cells[2],
        vec_cell(&[(3.0f64 / 5.0) as f32, (4.0f64 / 5.0) as f32, 0.0])
    );
    assert_eq!(cells[3], vec_cell(&[4.0, 0.0]));
    let tys: Vec<_> = r
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Computed { ty, .. } => ty.clone(),
            other => panic!("unexpected column meta: {other:?}"),
        })
        .collect();
    assert_eq!(tys[0], Some(ColumnType::Integer));
    assert_eq!(tys[1], Some(ColumnType::Double));
    assert_eq!(tys[2], None);
    assert_eq!(tys[3], None);
}

#[test]
fn expression_positions() {
    let (core, _g) = build_core("sql72-fn-expr");
    assert_eq!(
        col0(
            &core,
            "SELECT count(*) FROM t WHERE vector_dims(embedding) = 3"
        ),
        vec![Cell::Integer(4)]
    );
    assert_eq!(
        col0(
            &core,
            "SELECT k FROM t WHERE vector_norm(embedding) > 4 ORDER BY id"
        ),
        vec![Cell::SignedInteger(1), Cell::Null]
    );
    assert_eq!(
        col0(
            &core,
            "SELECT k FROM t WHERE k IS NOT NULL ORDER BY vector_norm(embedding) DESC"
        ),
        vec![
            Cell::SignedInteger(1),
            Cell::SignedInteger(2),
            Cell::SignedInteger(3)
        ]
    );
    assert_eq!(
        col0(
            &core,
            "SELECT CASE WHEN vector_dims(embedding) = 3 THEN 1 ELSE 0 END FROM t WHERE k = 2"
        ),
        vec![Cell::SignedInteger(1)]
    );
    assert_eq!(
        col0(
            &core,
            "SELECT g FROM t GROUP BY g HAVING max(vector_norm(embedding)) > 9"
        ),
        vec![Cell::Text("b".to_string())]
    );
    assert_eq!(
        col0(
            &core,
            "SELECT vector_norm(l2_normalize(subvector(embedding, 2, 2))) FROM t WHERE k = 1"
        ),
        vec![Cell::Float(1.0)]
    );
    assert_eq!(
        col0(
            &core,
            "SELECT sum(subvector(embedding, 1, 2)) FROM t WHERE g = 'a'"
        ),
        vec![Cell::Vector(vec![4.0, 6.0])]
    );
}

#[test]
fn subvector_boundaries() {
    let (core, _g) = build_core("sql72-fn-sub");
    let q = |s: &str, c: &str| {
        col0(
            &core,
            &format!("SELECT subvector(embedding, {s}, {c}) FROM t WHERE k = 2"),
        )
    };
    assert_eq!(q("1", "3"), vec![vec_cell(&[1.0, 2.0, 2.0])]);
    assert_eq!(q("2", "5"), vec![vec_cell(&[2.0, 2.0])]);
    assert_eq!(q("0", "2"), vec![vec_cell(&[1.0])]);
    assert_eq!(q("-1", "3"), vec![vec_cell(&[1.0])]);
    for (s, c) in [
        ("-1", "2"),
        ("4", "1"),
        ("1", "0"),
        ("1", "-1"),
        ("1.5", "1"),
    ] {
        assert_eq!(
            code_of(
                &core,
                &format!("SELECT subvector(embedding, {s}, {c}) FROM t WHERE k = 2")
            ),
            "22000",
            "{s} {c}"
        );
    }
    assert_eq!(
        code_of(
            &core,
            "SELECT subvector(embedding, 3000000000, 1) FROM t WHERE k = 2"
        ),
        "22003"
    );
}

#[test]
fn l2_normalize_zero_vector_stays_zero() {
    let (core, _g) = build_core("sql72-fn-zero");
    assert_eq!(
        col0(&core, "SELECT l2_normalize(embedding) FROM t WHERE k = 3"),
        vec![vec_cell(&[0.0, 0.0, 0.0])]
    );
}

#[test]
fn null_arguments_propagate() {
    let (core, _g) = build_core("sql72-fn-null");
    assert_eq!(
        col0(
            &core,
            "SELECT subvector(embedding, k, 1) FROM t WHERE g = 'b' AND k IS NULL"
        ),
        vec![Cell::Null]
    );
}

#[test]
fn invisible_rows_are_never_evaluated() {
    let (core, _g) = build_core("sql72-fn-rls");
    // tenant-b の行（k = 99）は評価されれば start > dim の 22000。tenant-a は k = 1..3 と NULL のみ。
    let r = exec(&core, "SELECT subvector(embedding, k, 1) FROM t");
    assert_eq!(r.rows.len(), 4);
    assert_eq!(
        col0(
            &core,
            "SELECT count(*) FROM t WHERE vector_dims(embedding) = 3"
        ),
        vec![Cell::Integer(4)]
    );
}

#[test]
fn unsupported_shapes_stay_fail_closed() {
    let (core, _g) = build_core("sql72-fn-closed");
    assert_eq!(
        code_of(&core, "SELECT vector_dims('[1,2]') FROM t"),
        "42804"
    );
    assert_eq!(
        code_of(&core, "SELECT vector_dims(embedding, 1) FROM t"),
        "42883"
    );
    assert_eq!(
        code_of(&core, "SELECT subvector(embedding, 1) FROM t"),
        "42883"
    );
    assert_eq!(
        code_of(&core, "SELECT k FROM t ORDER BY l2_normalize(embedding)"),
        "22000"
    );
}
