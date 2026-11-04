//! `avg(<VECTOR>)` 集約の結合テスト（SQL-72 (3)・(5)・(6)・Issue #2039。関連: SQL-13・RLS-7/RLS-8）。
//!
//! 期待値は f32 で正確に表せる整数・0.5 刻みの固定値で、完全一致で比べる（索引経路と全走査で
//! 足す順序が違っても揺れないため）。`unique_db_path`＋`CleanupGuard`、`EngineCore::execute_sql` の
//! production 経路で検証し、他テナントの Private 行が結果・エラーの有無に影響しないこと（RLS）と
//! `wire_code` のエラー契約を固定する。次元不一致の `22000` は SQL から作れないため、
//! `sql::aggregate` の単体テストで固定している。

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
            ColumnDef::new("embedding", ColumnType::Vector(2), true),
            ColumnDef::new("g", ColumnType::Text, true),
            ColumnDef::new("k", ColumnType::Integer, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// `(id, embedding, g, k)`。NULL 埋め込み（`dim == 0`）の行は表層から作れないため、
/// 飛ばす挙動は `sql::aggregate_input` の単体テストで固定している。
type Seed = (u64, Option<[f32; 2]>, &'static str, i32);

fn insert_t(storage: &Storage, tenant: &str, seed: &Seed, vis: Visibility) {
    let (id, emb, g, k) = *seed;
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            emb.map_or(Value::Null, |e| Value::Vector(e.to_vec())),
            Value::Text(g.to_string()),
            Value::Integer(k),
        ],
        &OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid operation id"),
    )
    .expect("insert row");
}

/// tenant-a の Public 行。f32 累積なら全体和が `16777222` に潰れ平均が一致しない並びを含む。
const SEEDS_A: [Seed; 6] = [
    (1, Some([16_777_216.0, 1.0]), "a", 1),
    (2, Some([1.0, 2.0]), "a", 2),
    (3, Some([1.0, 3.0]), "a", 3),
    (4, Some([2.0, 4.0]), "b", 4),
    (5, Some([4.0, 8.0]), "b", 5),
    (6, Some([0.0, 0.0]), "c", 6),
];

/// 他テナントの Private 行。漏れれば値が変わる巨大値。
const SEEDS_B_HUGE: [Seed; 2] = [
    (100, Some([3.0e38, 3.0e38]), "a", 3),
    (101, Some([3.0e38, 3.0e38]), "b", 3),
];

fn build_core(tag: &str, seeds_a: &[Seed], seeds_b: &[Seed]) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    for seed in seeds_a {
        insert_t(&storage, "tenant-a", seed, Visibility::Public);
    }
    for seed in seeds_b {
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

fn vec_cell(v: &[f32]) -> Cell {
    Cell::Vector(v.to_vec())
}

fn single(core: &EngineCore, sql: &str) -> Cell {
    let r = exec(core, sql);
    assert_eq!(r.rows.len(), 1, "{sql}");
    r.rows[0].cells[0].clone()
}

fn group_values(r: &QueryResult) -> Vec<(Cell, Cell)> {
    r.rows
        .iter()
        .map(|row| (row.cells[0].clone(), row.cells[1].clone()))
        .collect()
}

#[test]
fn averages_elementwise_with_f64_accumulation() {
    let (core, _g) = build_core("sql72-avg-basic", &SEEDS_A, &SEEDS_B_HUGE);
    assert_eq!(
        single(&core, "SELECT avg(embedding) FROM t"),
        vec_cell(&[2_796_204.0, 3.0])
    );
    let r = exec(&core, "SELECT avg(embedding) FROM t");
    match &r.columns[0] {
        ColumnMeta::Computed { ty, .. } => assert_eq!(*ty, Some(ColumnType::Vector(2))),
        other => panic!("unexpected column meta: {other:?}"),
    }
}

/// Issue の「sum／count で組み立てる」: 同一文内で avg と sum／count が整合する。
#[test]
fn avg_matches_sum_divided_by_count() {
    let (core, _g) = build_core("sql72-avg-sumcount", &SEEDS_A, &SEEDS_B_HUGE);
    let r = exec(
        &core,
        "SELECT avg(embedding), sum(embedding), count(embedding) FROM t",
    );
    assert_eq!(r.rows.len(), 1);
    assert_eq!(r.rows[0].cells[0], vec_cell(&[2_796_204.0, 3.0]));
    assert_eq!(r.rows[0].cells[1], vec_cell(&[16_777_224.0, 18.0]));
    assert_eq!(r.rows[0].cells[2], Cell::Integer(6));
}

#[test]
fn avg_of_expression() {
    let (core, _g) = build_core("sql72-avg-expr", &SEEDS_A, &SEEDS_B_HUGE);
    assert_eq!(
        single(&core, "SELECT avg(vec_div(embedding, 2)) FROM t"),
        vec_cell(&[1_398_102.0, 1.5])
    );
}

#[test]
fn group_by_and_having_count_work_with_vector_avg() {
    let (core, _g) = build_core("sql72-avg-group", &SEEDS_A, &SEEDS_B_HUGE);
    let r = exec(
        &core,
        "SELECT g, avg(embedding) FROM t GROUP BY g ORDER BY g",
    );
    assert_eq!(
        group_values(&r),
        vec![
            (Cell::Text("a".into()), vec_cell(&[5_592_406.0, 2.0])),
            (Cell::Text("b".into()), vec_cell(&[3.0, 6.0])),
            (Cell::Text("c".into()), vec_cell(&[0.0, 0.0])),
        ]
    );
    let r = exec(
        &core,
        "SELECT g, avg(embedding) FROM t GROUP BY g HAVING count(*) > 1 ORDER BY g",
    );
    assert_eq!(r.rows.len(), 2);
    let r = exec(
        &core,
        "SELECT g, k, avg(embedding) FROM t GROUP BY g, k ORDER BY g, k",
    );
    assert_eq!(r.rows.len(), 6);
    assert_eq!(r.rows[0].cells[2], vec_cell(&[16_777_216.0, 1.0]));
}

/// Issue の「DISTINCT と併用」: `count(DISTINCT ...)` との同居は成功し、`avg(DISTINCT <vector>)` は
/// 構文層で拒否のまま（`sum` と同じ既存挙動）。
#[test]
fn distinct_combinations() {
    let (core, _g) = build_core("sql72-avg-distinct", &SEEDS_A, &SEEDS_B_HUGE);
    let r = exec(
        &core,
        "SELECT g, avg(embedding), count(DISTINCT k) FROM t GROUP BY g ORDER BY g",
    );
    assert_eq!(r.rows.len(), 3);
    assert_eq!(r.rows[0].cells[1], vec_cell(&[5_592_406.0, 2.0]));
    assert_eq!(r.rows[1].cells[1], vec_cell(&[3.0, 6.0]));
    assert_eq!(r.rows[2].cells[1], vec_cell(&[0.0, 0.0]));
    assert_eq!(r.rows[0].cells[2], Cell::Integer(3));
    assert_eq!(r.rows[1].cells[2], Cell::Integer(2));
    assert_eq!(r.rows[2].cells[2], Cell::Integer(1));

    assert_eq!(
        code_of(&core, "SELECT avg(DISTINCT embedding) FROM t"),
        "42601"
    );
    // `SELECT DISTINCT avg(...)` は受理でも拒否でもよいが、内部エラーにはしない。
    match core.execute_sql(&ctx("tenant-a"), "SELECT DISTINCT avg(embedding) FROM t") {
        Ok(r) => assert_eq!(r.rows.len(), 1),
        Err(e) => assert_ne!(e.wire_code(), "XX000"),
    }
}

#[test]
fn filter_and_empty_input_follow_the_null_contract() {
    let (core, _g) = build_core("sql72-avg-filter", &SEEDS_A, &SEEDS_B_HUGE);
    assert_eq!(
        single(&core, "SELECT avg(embedding) FILTER (WHERE k > 3) FROM t"),
        vec_cell(&[2.0, 4.0])
    );
    assert_eq!(
        single(&core, "SELECT avg(embedding) FILTER (WHERE k > 99) FROM t"),
        Cell::Null
    );
    let r = exec(&core, "SELECT avg(embedding), count(*) FROM t WHERE k > 99");
    assert_eq!(r.rows[0].cells[0], Cell::Null);
    assert_eq!(r.rows[0].cells[1], Cell::Integer(0));
}

/// f64 累積のため、`sum` なら f32 範囲を超える和でも平均は有限で返る。
#[test]
fn avg_absorbs_values_whose_sum_overflows_f32() {
    let seeds: [Seed; 2] = [
        (1, Some([3.0e38, 0.0]), "a", 1),
        (2, Some([3.0e38, 0.0]), "a", 2),
    ];
    let (core, _g) = build_core("sql72-avg-overflow", &seeds, &[]);
    assert_eq!(
        single(&core, "SELECT avg(embedding) FROM t"),
        vec_cell(&[3.0e38, 0.0])
    );
    assert_eq!(code_of(&core, "SELECT sum(embedding) FROM t"), "22003");
}

#[test]
fn rls_hides_other_tenant_rows_from_value() {
    let (with_b, _g1) = build_core("sql72-avg-rls-b", &SEEDS_A, &SEEDS_B_HUGE);
    let (without_b, _g2) = build_core("sql72-avg-rls-nob", &SEEDS_A, &[]);
    for sql in [
        "SELECT avg(embedding) FROM t",
        "SELECT avg(embedding) FROM t WHERE k = 3",
    ] {
        assert_eq!(single(&with_b, sql), single(&without_b, sql), "{sql}");
    }
    let sql = "SELECT g, avg(embedding) FROM t GROUP BY g ORDER BY g";
    assert_eq!(
        group_values(&exec(&with_b, sql)),
        group_values(&exec(&without_b, sql))
    );
}

#[test]
fn unsupported_shapes_stay_fail_closed() {
    let (core, _g) = build_core("sql72-avg-reject", &SEEDS_A, &SEEDS_B_HUGE);
    assert_eq!(code_of(&core, "SELECT min(embedding) FROM t"), "42883");
    assert_eq!(code_of(&core, "SELECT max(embedding) FROM t"), "42883");
    for (sql, expected) in [
        ("SELECT avg(embedding) OVER () FROM t", "0A000"),
        (
            "SELECT g, avg(embedding) FROM t GROUP BY g HAVING avg(embedding) > 1",
            "0A000",
        ),
        (
            "SELECT g, avg(embedding) FROM t GROUP BY g ORDER BY avg(embedding)",
            "0A000",
        ),
        (
            "SELECT g, avg(embedding) <-> avg(embedding) FROM t GROUP BY g",
            "42601",
        ),
        (
            "SELECT avg(a.embedding) FROM t a JOIN t b ON a.k = b.k",
            "42883",
        ),
    ] {
        let err = core
            .execute_sql(&ctx("tenant-a"), sql)
            .expect_err(&format!("must fail: {sql}"));
        assert_eq!(err.wire_code(), expected, "{sql}");
        let msg = format!("{err:?}");
        assert!(!msg.contains("tenant-"), "{sql}: {msg}");
        assert!(
            !msg.contains("3e38") && !msg.contains("3.0e38"),
            "{sql}: {msg}"
        );
    }
}

#[test]
fn vector_avg_is_rejected_by_deduplicating_set_operations() {
    let (core, _g) = build_core("sql72-avg-setop", &SEEDS_A, &SEEDS_B_HUGE);
    for (a, label) in [
        ("avg(embedding)", "col"),
        ("avg(vec_div(embedding, 2))", "expr"),
    ] {
        for op in ["UNION", "INTERSECT", "EXCEPT"] {
            let sql = format!("SELECT {a} FROM t {op} SELECT {a} FROM t");
            let code = code_of(&core, &sql);
            assert_ne!(code, "XX000", "{label}: {sql}");
            assert_eq!(code, "22000", "{label}: {sql}");
        }
        let r = exec(
            &core,
            &format!("SELECT {a} FROM t UNION ALL SELECT {a} FROM t"),
        );
        assert_eq!(r.rows.len(), 2, "{label}");
    }
}
