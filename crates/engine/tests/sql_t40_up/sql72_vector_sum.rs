//! `sum(<VECTOR>)` 集約の結合テスト（SQL-72 (3)・Issue #2038。関連: SQL-13・SQL-57・RLS-7/RLS-8）。
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

/// tenant-a の Public 行。f32 累積なら `16777216 + 1 + 1` が `16777216` に潰れる並びを含む。
const SEEDS_A: [Seed; 5] = [
    (1, Some([16_777_216.0, 1.0]), "b", 1),
    (2, Some([1.0, 2.0]), "a", 2),
    (3, Some([1.0, 4.0]), "a", 3),
    (4, Some([0.0, 0.0]), "b", 4),
    (5, Some([0.0, 1.0]), "c", 5),
];

/// 他テナントの Private 行。足せば f32 範囲を超える値（漏れれば `22003` か値が変わる）。
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

#[test]
fn sums_elementwise_with_f64_accumulation() {
    let (core, _g) = build_core("sql72-sum-basic", &SEEDS_A, &SEEDS_B_HUGE);
    // f32 累積なら 16777216 のまま。f64 累積なら 16777218（f32 で正確に表せる）。
    assert_eq!(
        single(&core, "SELECT sum(embedding) FROM t"),
        vec_cell(&[16_777_218.0, 8.0])
    );
    // 結果列の型は vector(2)（wire では OID 25 のテキスト表現）。
    let r = exec(&core, "SELECT sum(embedding) FROM t");
    match &r.columns[0] {
        ColumnMeta::Computed { ty, .. } => assert_eq!(*ty, Some(ColumnType::Vector(2))),
        other => panic!("unexpected column meta: {other:?}"),
    }
}

#[test]
fn sum_of_expression_halves_the_column_sum() {
    let (core, _g) = build_core("sql72-sum-expr", &SEEDS_A, &SEEDS_B_HUGE);
    assert_eq!(
        single(&core, "SELECT sum(vec_div(embedding, 2)) FROM t"),
        vec_cell(&[8_388_609.0, 4.0])
    );
}

#[test]
fn group_by_and_having_count_work_with_vector_sum() {
    let (core, _g) = build_core("sql72-sum-group", &SEEDS_A, &SEEDS_B_HUGE);
    let r = exec(
        &core,
        "SELECT g, sum(embedding) FROM t GROUP BY g ORDER BY g",
    );
    let got: Vec<(Cell, Cell)> = r
        .rows
        .iter()
        .map(|row| (row.cells[0].clone(), row.cells[1].clone()))
        .collect();
    assert_eq!(
        got,
        vec![
            (Cell::Text("a".into()), vec_cell(&[2.0, 6.0])),
            (Cell::Text("b".into()), vec_cell(&[16_777_216.0, 1.0])),
            (Cell::Text("c".into()), vec_cell(&[0.0, 1.0])),
        ]
    );
    // 別の集計への HAVING とは組み合わせられる。
    let r = exec(
        &core,
        "SELECT g, sum(embedding) FROM t GROUP BY g HAVING count(*) > 1 ORDER BY g",
    );
    assert_eq!(r.rows.len(), 2);
    // 複数キーの GROUP BY（全走査経路）でも同じ和になる。
    let r = exec(
        &core,
        "SELECT g, k, sum(embedding) FROM t GROUP BY g, k ORDER BY g, k",
    );
    assert_eq!(r.rows.len(), 5);
    assert_eq!(r.rows[0].cells[2], vec_cell(&[1.0, 2.0]));
}

#[test]
fn filter_and_empty_input_follow_the_null_contract() {
    let (core, _g) = build_core("sql72-sum-filter", &SEEDS_A, &SEEDS_B_HUGE);
    assert_eq!(
        single(&core, "SELECT sum(embedding) FILTER (WHERE k > 2) FROM t"),
        vec_cell(&[1.0, 5.0])
    );
    assert_eq!(
        single(&core, "SELECT sum(embedding) FILTER (WHERE k > 99) FROM t"),
        Cell::Null
    );
    // WHERE で 0 行になる場合は NULL（count(*) は 0）。
    let r = exec(&core, "SELECT sum(embedding), count(*) FROM t WHERE k > 99");
    assert_eq!(r.rows[0].cells[0], Cell::Null);
    assert_eq!(r.rows[0].cells[1], Cell::Integer(0));
}

#[test]
fn rls_hides_other_tenant_rows_from_value_and_overflow() {
    // tenant-b の Private 行は、足せば 22003 になる値だが tenant-a の結果へ一切影響しない。
    let (with_b, _g1) = build_core("sql72-sum-rls-b", &SEEDS_A, &SEEDS_B_HUGE);
    let (without_b, _g2) = build_core("sql72-sum-rls-nob", &SEEDS_A, &[]);
    for sql in [
        "SELECT sum(embedding) FROM t",
        "SELECT sum(embedding) FROM t WHERE k = 3",
    ] {
        assert_eq!(single(&with_b, sql), single(&without_b, sql), "{sql}");
    }
}

#[test]
fn overflow_beyond_f32_range_is_22003_without_echoing_values() {
    let seeds: [Seed; 2] = [
        (1, Some([3.0e38, 0.0]), "a", 1),
        (2, Some([3.0e38, 0.0]), "a", 2),
    ];
    let (core, _g) = build_core("sql72-sum-overflow", &seeds, &[]);
    let err = core
        .execute_sql(&ctx("tenant-a"), "SELECT sum(embedding) FROM t")
        .expect_err("overflow must fail");
    assert_eq!(err.wire_code(), "22003");
    let msg = format!("{err:?}");
    assert!(!msg.contains("tenant-"), "{msg}");
    assert!(!msg.contains("3e38") && !msg.contains("3.0e38"), "{msg}");
}

#[test]
fn unsupported_shapes_stay_fail_closed() {
    let (core, _g) = build_core("sql72-sum-reject", &SEEDS_A, &SEEDS_B_HUGE);
    // MIN/MAX は従来どおり 42883。
    assert_eq!(code_of(&core, "SELECT min(embedding) FROM t"), "42883");
    assert_eq!(code_of(&core, "SELECT max(embedding) FROM t"), "42883");
    // DISTINCT・ウィンドウ・集計結果の比較／演算は拒否のまま。
    assert_eq!(
        code_of(&core, "SELECT sum(DISTINCT embedding) FROM t"),
        "42601"
    );
    assert_eq!(
        code_of(&core, "SELECT sum(embedding) OVER () FROM t"),
        "0A000"
    );
    // 集計結果のベクトルの比較・並べ替え・演算は SQL-72 (4) の範囲外で、黙って通さず拒否する。
    for (sql, expected) in [
        (
            "SELECT g, sum(embedding) FROM t GROUP BY g HAVING sum(embedding) > 1",
            "0A000",
        ),
        (
            "SELECT g, sum(embedding) FROM t GROUP BY g ORDER BY sum(embedding)",
            "0A000",
        ),
        (
            "SELECT g, sum(embedding) <-> sum(embedding) FROM t GROUP BY g",
            "42601",
        ),
        // JOIN の集計経路は従来どおり拒否する（VECTOR 列の集計は単一表のみ）。
        (
            "SELECT sum(a.embedding) FROM t a JOIN t b ON a.k = b.k",
            "42883",
        ),
    ] {
        let code = code_of(&core, sql);
        assert_eq!(code, expected, "{sql}");
    }
}

/// PR #2925 レビュー指摘の回帰: 集計結果（`Computed`）のベクトル列も、重複除去を伴う集合演算
/// （UNION/INTERSECT/EXCEPT）の型ゲートで明示的に拒否する（行キー化の `XX000` に到達させない）。
/// `UNION ALL` は行キーを作らないため従来どおり通す。
#[test]
fn vector_sum_is_rejected_by_deduplicating_set_operations() {
    let (core, _g) = build_core("sql72-sum-setop", &SEEDS_A, &SEEDS_B_HUGE);
    for op in ["UNION", "INTERSECT", "EXCEPT"] {
        let sql = format!("SELECT sum(embedding) FROM t {op} SELECT sum(embedding) FROM t");
        let code = code_of(&core, &sql);
        assert_ne!(code, "XX000", "{sql}");
        assert_eq!(code, "22000", "{sql}");
    }
    let r = exec(
        &core,
        "SELECT sum(embedding) FROM t UNION ALL SELECT sum(embedding) FROM t",
    );
    assert_eq!(r.rows.len(), 2);
}

/// PR #2925 レビュー指摘の回帰: VECTOR 式（`vec_div(embedding, 2)` 等）への `sum` も、
/// 列への `sum` と同じく集合演算の型ゲートで 22000 になる（結果型の公告が VECTOR を保つ）。
#[test]
fn vector_expr_sum_is_rejected_by_deduplicating_set_operations() {
    let (core, _g) = build_core("sql72-sum-expr-setop", &SEEDS_A, &SEEDS_B_HUGE);
    for op in ["UNION", "INTERSECT", "EXCEPT"] {
        let sql = format!(
            "SELECT sum(vec_div(embedding, 2)) FROM t {op} SELECT sum(vec_div(embedding, 2)) FROM t"
        );
        let code = code_of(&core, &sql);
        assert_ne!(code, "XX000", "{sql}");
        assert_eq!(code, "22000", "{sql}");
    }
    let r = exec(
        &core,
        "SELECT sum(vec_div(embedding, 2)) FROM t UNION ALL SELECT sum(vec_div(embedding, 2)) FROM t",
    );
    assert_eq!(r.rows.len(), 2);
}
