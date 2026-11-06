//! ハイブリッド検索の投影で系統別順位と融合スコアを返す引数なし関数
//! `hybrid_dense_rank()`・`hybrid_sparse_rank()`・`hybrid_score()` の受け入れテスト
//! （Issue #2137。ビヘイビア: SEARCH-17 (1)〜(3)・SQL-4・RLS-8。ポインタ:
//! `docs/spec/04-behavior/search.md`）。
//!
//! 密側の順位は id 昇順（id=1 が最も近い）、疎側は id=6,5,4 の順に語 `apple` の頻度が高く、
//! id=1〜3 は疎側に現れない。この非対称なデータで NULL 順位・スコア一致・結果不変・
//! 越境なしを固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const HYBRID: &str = "ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'apple'";

fn seed(storage: &Storage, tenant: &str, vis: Visibility, rows: &[(u64, [f32; 2], &str)]) {
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    for (id, emb, body) in rows {
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            *id,
            vis,
            &[
                Value::Vector(emb.to_vec()),
                Value::Text((*body).to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{tenant}-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
}

fn setup(name: &str, with_other_tenant: bool) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    seed(
        &storage,
        "tenant-a",
        Visibility::Public,
        &[
            (1, [1.0, 0.0], "zzz zzz zzz"),
            (2, [0.9, 0.1], "zzz zzz zzz"),
            (3, [0.8, 0.2], "zzz zzz zzz"),
            (4, [0.7, 0.3], "apple zzz zzz"),
            (5, [0.6, 0.4], "apple apple zzz"),
            (6, [0.5, 0.5], "apple apple apple"),
        ],
    );
    if with_other_tenant {
        // 別テナントの強く一致する行（密・疎ともに自テナントより上位になりうる配置）。
        seed(
            &storage,
            "tenant-b",
            Visibility::Private,
            &[
                (11, [1.0, 0.0], "apple apple apple"),
                (12, [1.0, 0.0], "apple apple apple"),
                (13, [1.0, 0.0], "apple apple apple"),
            ],
        );
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn ctx() -> PolicyContext {
    PolicyContext::new("tenant-a").expect("valid tenant")
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    core.execute_sql(&ctx(), sql).expect("query must succeed")
}

fn code(core: &EngineCore, sql: &str) -> &'static str {
    core.execute_sql(&ctx(), sql)
        .expect_err("must be rejected")
        .wire_code()
}

fn rank(cell: &Cell) -> Option<i64> {
    match cell {
        Cell::SignedInteger(v) => Some(*v),
        Cell::Null => None,
        other => panic!("unexpected rank cell: {other:?}"),
    }
}

fn score(cell: &Cell) -> f64 {
    match cell {
        Cell::Float(v) => *v,
        other => panic!("unexpected score cell: {other:?}"),
    }
}

/// 行ごとの (id, 密順位, 疎順位, スコア)。
fn detail(r: &QueryResult) -> Vec<(u64, Option<i64>, Option<i64>, f64)> {
    r.rows
        .iter()
        .map(|row| {
            (
                row.id,
                rank(&row.cells[0]),
                rank(&row.cells[1]),
                score(&row.cells[2]),
            )
        })
        .collect()
}

fn detail_sql(tail: &str, limit: &str) -> String {
    format!(
        "SELECT hybrid_dense_rank(), hybrid_sparse_rank(), hybrid_score() FROM docs {HYBRID}{tail}) {limit}"
    )
}

#[test]
fn search17_ranks_are_per_system_with_null_for_absent_and_types_are_int8_float8() {
    let (core, _g) = setup("search17-values", false);
    let r = run(&core, &detail_sql("", "LIMIT 6"));
    let got = detail(&r);
    let by = |id: u64| {
        got.iter()
            .find(|g| g.0 == id)
            .copied()
            .expect("row present")
    };
    // 密側: id 昇順に 1..6、疎側: id=6,5,4 の順に 1,2,3 で id=1〜3 は NULL。
    for id in 1..=6u64 {
        assert_eq!(by(id).1, Some(id as i64), "dense rank of {id}");
    }
    assert_eq!((by(6).2, by(5).2, by(4).2), (Some(1), Some(2), Some(3)));
    assert_eq!((by(1).2, by(2).2, by(3).2), (None, None, None));
    assert!(matches!(
        r.columns.as_slice(),
        [
            ColumnMeta::Computed { name: a, ty: Some(ColumnType::BigInt) },
            ColumnMeta::Computed { name: b, ty: Some(ColumnType::BigInt) },
            ColumnMeta::Computed { name: c, ty: Some(ColumnType::Double) },
        ] if a == "hybrid_dense_rank" && b == "hybrid_sparse_rank" && c == "hybrid_score"
    ));
}

#[test]
fn search17_score_equals_sum_of_weighted_reciprocal_ranks() {
    let (core, _g) = setup("search17-score", false);
    for (tail, k, wd, ws) in [
        ("", 60.0_f64, 1.0_f64, 1.0_f64),
        (", rrf_k => 10", 10.0, 1.0, 1.0),
        (
            ", rrf_k => 10, weights => '{\"dense\": 2, \"sparse\": 0.5}'",
            10.0,
            2.0,
            0.5,
        ),
    ] {
        let r = run(&core, &detail_sql(tail, "LIMIT 6"));
        for (id, d, s, got) in detail(&r) {
            // 加算順は密 → 疎（`rrf_fuse` と同じ）。
            let mut expected = 0.0_f64;
            if let Some(d) = d {
                expected += wd / (k + d as f64);
            }
            if let Some(s) = s {
                expected += ws / (k + s as f64);
            }
            assert!(
                (expected - got).abs() < 1e-12,
                "id={id} tail={tail}: expected {expected}, got {got}"
            );
        }
    }
}

#[test]
fn search17_alias_default_name_and_case_insensitive_names() {
    let (core, _g) = setup("search17-alias", false);
    let r = run(
        &core,
        &format!("SELECT id, HYBRID_SCORE() AS s, Hybrid_Dense_Rank() FROM docs {HYBRID}) LIMIT 3"),
    );
    assert_eq!(r.rows.len(), 3);
    assert!(matches!(
        &r.columns[1],
        ColumnMeta::Computed { name, .. } if name == "s"
    ));
    assert!(matches!(
        &r.columns[2],
        ColumnMeta::Computed { name, .. } if name == "hybrid_dense_rank"
    ));
}

#[test]
fn search17_result_rows_and_order_are_unchanged_by_the_functions() {
    let (core, _g) = setup("search17-invariant", true);
    for (tail, limit) in [
        ("", "LIMIT 4"),
        ("", "LIMIT 3 OFFSET 2"),
        (", rrf_k => 5", "LIMIT 6"),
    ] {
        let plain = run(
            &core,
            &format!("SELECT id FROM docs {HYBRID}{tail}) {limit}"),
        );
        let with = run(
            &core,
            &format!(
                "SELECT id, hybrid_score(), hybrid_sparse_rank() FROM docs {HYBRID}{tail}) {limit}"
            ),
        );
        let a: Vec<u64> = plain.rows.iter().map(|r| r.id).collect();
        let b: Vec<u64> = with.rows.iter().map(|r| r.id).collect();
        assert_eq!(a, b, "{tail} {limit}");
    }
}

#[test]
fn search17_other_tenant_rows_do_not_affect_ranks_or_scores() {
    let (alone, _g1) = setup("search17-rls-alone", false);
    let (shared, _g2) = setup("search17-rls-shared", true);
    let sql = detail_sql("", "LIMIT 6");
    let a = detail(&run(&alone, &sql));
    let b = detail(&run(&shared, &sql));
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!((x.0, x.1, x.2), (y.0, y.1, y.2));
        assert_eq!(x.3.to_bits(), y.3.to_bits());
    }
    assert!(
        b.iter().all(|r| r.0 <= 6),
        "other tenant ids must not appear"
    );
}

#[test]
fn search17_misuse_is_rejected_with_42601() {
    let (core, _g) = setup("search17-misuse", false);
    for sql in [
        "SELECT hybrid_score() FROM docs LIMIT 3".to_string(),
        "SELECT hybrid_score() FROM docs ORDER BY id LIMIT 3".to_string(),
        "SELECT hybrid_dense_rank() FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3"
            .to_string(),
        format!("SELECT id FROM docs WHERE hybrid_score() > 0 {HYBRID}) LIMIT 3"),
        format!("SELECT hybrid_score() + 1 FROM docs {HYBRID}) LIMIT 3"),
        format!("SELECT COALESCE(hybrid_dense_rank(), 0) FROM docs {HYBRID}) LIMIT 3"),
    ] {
        assert_eq!(code(&core, &sql), "42601", "{sql}");
    }
}

/// 引数付きの同名呼び出しは予約対象外（既存 UDF の解決へ回り、未定義なら `42883`）。
#[test]
fn search17_call_with_arguments_is_not_reserved() {
    let (core, _g) = setup("search17-args-not-reserved", false);
    let sql = format!("SELECT hybrid_score(1) FROM docs {HYBRID}) LIMIT 3");
    assert_ne!(code(&core, &sql), "42601", "{sql}");
}
