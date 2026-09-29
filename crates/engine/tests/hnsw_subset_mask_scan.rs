//! HNSW `Subset` 形状（SCALAR 事前フィルタ付き DISTANCE）が plain scan へ
//! 縮退する際、`VectorArena` の複製（`build_from_cached_rls_rows_subset`）を
//! 経ずキャッシュ済みスナップショットを借用したまま候補 id マスク経路
//! （`SearchProvider::search_subset`。Issue #654）へ委譲することを検証する
//! 結合テスト（Issue #676。親 `sql::hnsw_cache::SubsetSlotPlan`／
//! `prepare_subset_from_slots`・`sql::exec` の DISTANCE 段結線）。
//!
//! `tests/hnsw_cache.rs` の既存テスト（`filtered_distance_uses_subset_shape_
//! and_matches_default_engine_recall`・`full_scan_ratio_plain_scan_below_
//! ratio_subset_shape_matches_brute_force_and_never_leaks_across_tenants`・
//! `acorn_disabled_by_default_keeps_existing_behavior_unaffected` 等）は
//! Issue #676 適用後も無変更のまま green（要件 R2）——本ファイルは新設の
//! カウンタ（`subset_mask_scans`／`subset_arena_copies`）・複製有無の観点を
//! 追加で固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// ---------- 決定的擬似乱数（`tests/hnsw_cache.rs::TestRng` の複製。結合テストは
// crate 外の公開 API のみを使う流儀のため独立に複製する） ----------

struct TestRng {
    state: u64,
}

impl TestRng {
    fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_unit(&mut self) -> f32 {
        let bits = (self.next_u64() >> 40) as u32;
        (bits as f32) / (1u32 << 24) as f32 * 2.0 - 1.0
    }
}

fn normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn gen_clustered_corpus(seed: u64, dim: usize, rows: usize, clusters: usize) -> Vec<Vec<f32>> {
    let mut center_rng = TestRng::new(seed ^ 0xC1C1_C1C1_C1C1_C1C1);
    let centers: Vec<Vec<f32>> = (0..clusters.max(1))
        .map(|_| (0..dim).map(|_| center_rng.next_unit()).collect())
        .collect();
    let mut rng = TestRng::new(seed);
    (0..rows)
        .map(|i| {
            let center = &centers[i % centers.len()];
            let mut v: Vec<f32> = center.iter().map(|c| c + rng.next_unit() * 0.2).collect();
            normalize(&mut v);
            v
        })
        .collect()
}

const DIM: u32 = 16;
// `MIN_INDEXED_ROWS`（1,024）超・`build_parallel` が逐次 `build` へ縮退する
// `SEQUENTIAL_PREFIX_NODES`（256）超だが `thread_count_for` の実質並列化閾値
// （2,048）未満に収め、構築グラフを決定的に保つ（`tests/hnsw_cache.rs` と同じ
// fixture 方針）。
const BASE_ROWS: usize = 1_200;

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

fn query_ids(core: &EngineCore, ctx: &PolicyContext, query: &[f32], k: usize) -> Vec<u64> {
    let sql = format!(
        "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT {}",
        vec_literal(query),
        k
    );
    let result = core.execute_sql(ctx, &sql).expect("query should succeed");
    result.rows.iter().map(|r| r.id).collect()
}

fn tag_schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("tag", ColumnType::Text, false),
        ],
    )
}

/// `tag='x'`（`selector` が `true` を返す行）・`tag='y'`（それ以外）の 2 値
/// タグ付きコーパスを投入する。`hnsw_cache.rs::seed_acorn_fixture` と同型だが
/// 選択率を呼び出し元が選べるよう一般化した版。
fn seed_tagged_corpus(
    storage: &Storage,
    schema: &TableSchema,
    ctx: &PolicyContext,
    op_tag: &str,
    vectors: &[Vec<f32>],
    selector: impl Fn(usize) -> bool,
) {
    let op_id = OperationId::parse(&format!("hnsw-subset-mask-{op_tag}")).expect("valid op id");
    let metadata_x = engine::row_codec::encode_scalar_columns(
        schema,
        &[Value::Null, Value::Text("x".to_string())],
    )
    .expect("encode tag=x metadata");
    let metadata_y = engine::row_codec::encode_scalar_columns(
        schema,
        &[Value::Null, Value::Text("y".to_string())],
    )
    .expect("encode tag=y metadata");
    let rows: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let metadata = if selector(i) {
                metadata_x.as_slice()
            } else {
                metadata_y.as_slice()
            };
            (
                i as u64 + 1,
                RowInput {
                    tenant_id: ctx.tenant_id(),
                    visibility: Visibility::Public,
                    embedding: v.as_slice(),
                    metadata,
                },
            )
        })
        .collect();
    engine::tenant::insert_rows(storage, "docs", ctx, &rows, &op_id).expect("seed rows");
}

const ACORN_ALWAYS: engine::hnsw::Ratio = engine::hnsw::Ratio {
    numerator: 1,
    denominator: 1,
};

fn hnsw_kind_with_full_scan_ratio(ratio: engine::hnsw::Ratio) -> search_engine::SearchEngineKind {
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    match kind {
        search_engine::SearchEngineKind::Hnsw(validated) => search_engine::SearchEngineKind::Hnsw(
            validated
                .with_full_scan_ratio(ratio)
                .expect("valid full_scan_ratio"),
        ),
        other => panic!("hnsw_kind must return SearchEngineKind::Hnsw, got {other:?}"),
    }
}

fn hnsw_kind_with_acorn(ratio: engine::hnsw::Ratio) -> search_engine::SearchEngineKind {
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    match kind {
        search_engine::SearchEngineKind::Hnsw(validated) => search_engine::SearchEngineKind::Hnsw(
            validated
                .with_acorn_max_visible_ratio(ratio)
                .expect("valid acorn_max_visible_ratio"),
        ),
        other => panic!("hnsw_kind must return SearchEngineKind::Hnsw, got {other:?}"),
    }
}

/// T1: `PlainScanBelowRatio`（選択率 50%・`full_scan_ratio=60/100`）は候補 id
/// マスク経路（複製なし）で完了し、`VectorArena` の複製が一切発生しないことを
/// 固定する。`tests/hnsw_cache.rs::full_scan_ratio_plain_scan_below_ratio_
/// subset_shape_matches_brute_force_and_never_leaks_across_tenants` と同型の
/// fixture・パラメータで、新設カウンタのみを追加検証する。
#[test]
fn plain_scan_below_ratio_uses_mask_path_without_arena_copy() {
    let dir = unique_db_path("hnsw-subset-mask-t1");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = tag_schema();
    storage.create_table(&schema).expect("create table");

    let vectors = gen_clustered_corpus(101, DIM as usize, BASE_ROWS, 6);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    seed_tagged_corpus(&storage, &schema, &ctx, "t1", &vectors, |i| i % 2 == 0);

    let ratio = engine::hnsw::Ratio {
        numerator: 60,
        denominator: 100,
    };
    let core = EngineCore::from_storage_with_engine(storage, hnsw_kind_with_full_scan_ratio(ratio));

    // フィルタなしクエリを 1 本先に投げ、`FullVisible` 経路に索引を構築させる
    // （`Subset` 経路は `Lookup::Miss` では構築せず plain scan へ縮退する契約）。
    let _ = query_ids(&core, &ctx, &vectors[0], 10);
    let baseline = core.hnsw_index_cache_stats();
    assert_eq!(baseline.entries, 1, "unfiltered query must build one entry");

    const K: usize = 10;
    for i in 0..10 {
        let query = &vectors[i * (BASE_ROWS / 10)];
        let sql = format!(
            "SELECT id FROM docs WHERE tag = 'x' ORDER BY embedding <=> '{}' LIMIT {K}",
            vec_literal(query)
        );
        let got = core.execute_sql(&ctx, &sql).expect("filtered query").rows;
        for row in &got {
            assert_eq!(row.id % 2, 1, "row {} does not satisfy tag='x'", row.id);
        }
    }

    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.plain_scans > 0,
        "this fixture must exercise PlainScanBelowRatio (non-vacuous)"
    );
    assert!(
        stats.subset_mask_scans > 0,
        "plain scan degradation must go through the no-copy candidate id mask path"
    );
    assert_eq!(
        stats.subset_arena_copies, 0,
        "plain scan degradation must never copy the VectorArena"
    );
    let scalar_stats = core.scalar_index_cache_stats();
    assert!(
        scalar_stats.index_mask_scans > 0,
        "the scalar secondary index must have been consumed via the no-copy mask path"
    );
}

/// T2: `mask_splits_graph`（選択率 20%・既定 `full_scan_ratio`）も同様に
/// 複製なしのマスク経路で完了することを固定する。`tests/hnsw_cache.rs::
/// seed_acorn_fixture`／`acorn_disabled_by_default_keeps_existing_behavior_
/// unaffected` と同じ fixture（20% 選択率・OneHop で分断）を流用する。
#[test]
fn mask_splits_graph_uses_mask_path_without_arena_copy() {
    let dir = unique_db_path("hnsw-subset-mask-t2");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = tag_schema();
    storage.create_table(&schema).expect("create table");

    let vectors = gen_clustered_corpus(9, DIM as usize, BASE_ROWS, 6);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    seed_tagged_corpus(&storage, &schema, &ctx, "t2", &vectors, |i| i % 5 == 0);

    // ACORN-1 を opt-in しない既定エンジン（`mask_splits_graph` が発火する側）。
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let _ = query_ids(&core, &ctx, &vectors[0], 10);

    const K: usize = 10;
    const QUERIES: usize = 20;
    // スカラー列二次索引（`ScalarIndex`）は gated 構築のため、最初の `WHERE`
    // クエリはこのクエリ自身の走査から索引を piggyback 構築するだけで候補
    // 削減（`index_candidate_slots`）を消費できない（`sql::scalar_index_cache`
    // の gated 構築契約。`tests/scalar_index_cache.rs::where_query_builds_once_
    // then_hits_within_same_generation` 参照）。ウォームアップの 1 本を計測対象
    // 外にすることで、以降の `QUERIES` 本すべてが候補 id マスク経路を通ることを
    // 保証する。
    let warmup_sql = format!(
        "SELECT id FROM docs WHERE tag = 'x' ORDER BY embedding <=> '{}' LIMIT {K}",
        vec_literal(&vectors[0])
    );
    let _ = core.execute_sql(&ctx, &warmup_sql).expect("warmup query");

    for i in 0..QUERIES {
        let query = &vectors[i * (BASE_ROWS / QUERIES)];
        let sql = format!(
            "SELECT id FROM docs WHERE tag = 'x' ORDER BY embedding <=> '{}' LIMIT {K}",
            vec_literal(query)
        );
        let _ = core.execute_sql(&ctx, &sql).expect("filtered query");
    }

    let stats = core.hnsw_index_cache_stats();
    assert_eq!(
        stats.mask_splits_graph,
        QUERIES as u64 + 1,
        "this fixture must reliably trigger mask_splits_graph on every query \
         including the warmup (fixture regression check, mirrors tests/hnsw_cache.rs)"
    );
    assert_eq!(
        stats.subset_mask_scans, QUERIES as u64,
        "every post-warmup mask_splits_graph degradation must go through the no-copy mask path"
    );
    assert_eq!(
        stats.subset_arena_copies, 0,
        "mask_splits_graph degradation must never copy the VectorArena"
    );
}

/// T3: ANN 探索へ進む場合（ACORN-1 opt-in で T2 と同じ疎なマスクが TwoHop で
/// 完走する）は、従来どおり `VectorArena` の複製が発生し、`(id, score)` 集合が
/// 既定エンジン対照 Recall@10 ≥ 0.9・tenant-b 非混入を維持することを固定する
/// （ANN 進行時の複製が Issue #676 で撤去されていないことの証拠）。
#[test]
fn ann_path_still_copies_and_matches_default_engine() {
    let dir = unique_db_path("hnsw-subset-mask-t3");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = tag_schema();
    storage.create_table(&schema).expect("create table");
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let vectors = gen_clustered_corpus(9, DIM as usize, BASE_ROWS, 6);
    seed_tagged_corpus(&storage, &schema, &ctx_a, "t3", &vectors, |i| i % 5 == 0);

    // tenant-b の private 行を混在させ、可視外テナントの id が結果へ混入しない
    // ことを検証する（TABLE-12・security.md P0）。
    let b_vectors = gen_clustered_corpus(42, DIM as usize, 100, 4);
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Private]).expect("valid tenant");
    let metadata_b = engine::row_codec::encode_scalar_columns(
        &schema,
        &[Value::Null, Value::Text("x".to_string())],
    )
    .expect("encode tenant-b metadata");
    let rows_b: Vec<(u64, RowInput<'_>)> = b_vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                BASE_ROWS as u64 + 1 + i as u64,
                RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Private,
                    embedding: v.as_slice(),
                    metadata: metadata_b.as_slice(),
                },
            )
        })
        .collect();
    let op_b = OperationId::parse("hnsw-subset-mask-t3-b").expect("valid operation_id");
    engine::tenant::insert_rows(&storage, "docs", &ctx_b, &rows_b, &op_b).expect("seed tenant-b");

    let core = EngineCore::from_storage_with_engine(storage, hnsw_kind_with_acorn(ACORN_ALWAYS));

    let ref_dir = unique_db_path("hnsw-subset-mask-t3-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage.create_table(&schema).expect("create ref table");
    seed_tagged_corpus(&ref_storage, &schema, &ctx_a, "t3-ref", &vectors, |i| {
        i % 5 == 0
    });
    engine::tenant::insert_rows(&ref_storage, "docs", &ctx_b, &rows_b, &op_b)
        .expect("seed ref tenant-b");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let _ = query_ids(&core, &ctx_a, &vectors[0], 10);

    const K: usize = 10;
    const QUERIES: usize = 20;
    let mut total_hits = 0usize;
    for i in 0..QUERIES {
        let query = &vectors[i * (BASE_ROWS / QUERIES)];
        let sql = format!(
            "SELECT id FROM docs WHERE tag = 'x' ORDER BY embedding <=> '{}' LIMIT {K}",
            vec_literal(query)
        );
        let got = core.execute_sql(&ctx_a, &sql).expect("filtered query").rows;
        let want = ref_core
            .execute_sql(&ctx_a, &sql)
            .expect("filtered query (ref)")
            .rows;
        for row in &got {
            assert!(
                row.id <= BASE_ROWS as u64,
                "tenant-a result must not include tenant-b row id {}",
                row.id
            );
        }
        let want_ids: std::collections::HashSet<u64> = want.iter().map(|r| r.id).collect();
        total_hits += got.iter().filter(|r| want_ids.contains(&r.id)).count();
    }
    let recall = total_hits as f64 / (QUERIES * K) as f64;
    assert!(
        recall >= 0.9,
        "ANN path (Subset shape, ACORN-1 opt-in) recall@{K} against the default engine \
         must be >= 0.9 (got {recall})"
    );

    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.subset_searches > 0,
        "ANN path must be exercised (non-vacuous)"
    );
    assert_eq!(
        stats.plain_scans, 0,
        "this fixture must not degrade to plain scan"
    );
    assert!(
        stats.subset_arena_copies > 0,
        "the ANN path must still copy the VectorArena (unchanged from before Issue #676)"
    );
}

/// T4: 縮退経路（T1 と同型の fixture）の結果が既定エンジン（厳密
/// brute-force）と `id` 順序込みで完全一致することを固定する——マスク経路は
/// 候補行を絞るだけで検索アルゴリズム自体は既定エンジンと同一の総当たりの
/// ため、縮退時は厳密一致するはずである。
#[test]
fn degraded_results_are_bit_identical_to_default_engine() {
    let dir = unique_db_path("hnsw-subset-mask-t4");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = tag_schema();
    storage.create_table(&schema).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let vectors = gen_clustered_corpus(202, DIM as usize, BASE_ROWS, 6);
    seed_tagged_corpus(&storage, &schema, &ctx, "t4", &vectors, |i| i % 2 == 0);

    let ratio = engine::hnsw::Ratio {
        numerator: 60,
        denominator: 100,
    };
    let core = EngineCore::from_storage_with_engine(storage, hnsw_kind_with_full_scan_ratio(ratio));

    let ref_dir = unique_db_path("hnsw-subset-mask-t4-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage.create_table(&schema).expect("create ref table");
    seed_tagged_corpus(&ref_storage, &schema, &ctx, "t4-ref", &vectors, |i| {
        i % 2 == 0
    });
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let _ = query_ids(&core, &ctx, &vectors[0], 10);

    for i in 0..10 {
        let query = &vectors[i * (BASE_ROWS / 10)];
        let sql = format!(
            "SELECT id FROM docs WHERE tag = 'x' ORDER BY embedding <=> '{v}' LIMIT 10",
            v = vec_literal(query)
        );
        let got = core.execute_sql(&ctx, &sql).expect("filtered query").rows;
        let want = ref_core
            .execute_sql(&ctx, &sql)
            .expect("filtered query (ref)")
            .rows;
        let got_ids: Vec<u64> = got.iter().map(|r| r.id).collect();
        let want_ids: Vec<u64> = want.iter().map(|r| r.id).collect();
        assert_eq!(
            got_ids, want_ids,
            "degraded (plain scan) Subset shape must return the same id order as the default engine"
        );
    }
}

/// T5: 同一 hnsw エンジンで、1 本目（キャッシュミス・複製経路。`Lookup::Miss`
/// では `Subset` 側は常に `FullScan` へ縮退する）と 2 本目以降（`FullVisible`
/// エントリが揃った後の候補 id マスク経路）が、複数列投影（`id`・`tag`）を
/// 含めて完全一致することを固定する（`EagerSubset` 写像の検証を兼ねる）。
#[test]
fn cold_and_hot_results_are_bit_identical() {
    let dir = unique_db_path("hnsw-subset-mask-t5");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = tag_schema();
    storage.create_table(&schema).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let vectors = gen_clustered_corpus(303, DIM as usize, BASE_ROWS, 6);
    seed_tagged_corpus(&storage, &schema, &ctx, "t5", &vectors, |i| i % 2 == 0);

    let ratio = engine::hnsw::Ratio {
        numerator: 60,
        denominator: 100,
    };
    let core = EngineCore::from_storage_with_engine(storage, hnsw_kind_with_full_scan_ratio(ratio));

    let query = &vectors[0];
    let sql = format!(
        "SELECT id, tag FROM docs WHERE tag = 'x' ORDER BY embedding <=> '{}' LIMIT 10",
        vec_literal(query)
    );

    // 1 本目: `Subset` 側は `Lookup::Miss` のため `FullScan`（複製経路の
    // brute-force。まだ候補 id マスク経路ではない）。
    let cold = core.execute_sql(&ctx, &sql).expect("cold query").rows;
    assert_eq!(
        core.hnsw_index_cache_stats().entries,
        0,
        "the first Subset-shape query must not build a FullVisible entry by itself"
    );

    // `FullVisible` 経路に索引を構築させたうえで再度同じクエリを投げる
    // （候補 id マスク経路。`PlainScanBelowRatio` へ縮退する fixture のため）。
    let _ = query_ids(&core, &ctx, &vectors[0], 10);
    let hot = core.execute_sql(&ctx, &sql).expect("hot query").rows;
    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.subset_mask_scans > 0,
        "the hot query must go through the no-copy candidate id mask path"
    );

    assert_eq!(cold.len(), hot.len(), "cold/hot result count must match");
    for (c, h) in cold.iter().zip(hot.iter()) {
        assert_eq!(c.id, h.id, "cold/hot id must match");
        assert_eq!(c.cells, h.cells, "cold/hot projected cells must match");
    }
}

/// T6: T1（`PlainScanBelowRatio`）・T2（`mask_splits_graph`）いずれの縮退
/// fixture でも、tenant-a の候補 id マスク経路が tenant-b の private 行を
/// 一切混入させないことを固定する（可視外テナント非漏えい。security.md P0）。
#[test]
fn rls_never_leaks_across_tenants_on_mask_path() {
    let dir = unique_db_path("hnsw-subset-mask-t6");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = tag_schema();
    storage.create_table(&schema).expect("create table");
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let vectors = gen_clustered_corpus(404, DIM as usize, BASE_ROWS, 6);
    seed_tagged_corpus(&storage, &schema, &ctx_a, "t6", &vectors, |i| i % 2 == 0);

    let b_vectors = gen_clustered_corpus(405, DIM as usize, 100, 4);
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Private]).expect("valid tenant");
    let metadata_b = engine::row_codec::encode_scalar_columns(
        &schema,
        &[Value::Null, Value::Text("x".to_string())],
    )
    .expect("encode tenant-b metadata");
    let rows_b: Vec<(u64, RowInput<'_>)> = b_vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                BASE_ROWS as u64 + 1 + i as u64,
                RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Private,
                    embedding: v.as_slice(),
                    metadata: metadata_b.as_slice(),
                },
            )
        })
        .collect();
    let op_b = OperationId::parse("hnsw-subset-mask-t6-b").expect("valid operation_id");
    engine::tenant::insert_rows(&storage, "docs", &ctx_b, &rows_b, &op_b).expect("seed tenant-b");

    let ratio = engine::hnsw::Ratio {
        numerator: 60,
        denominator: 100,
    };
    let core = EngineCore::from_storage_with_engine(storage, hnsw_kind_with_full_scan_ratio(ratio));
    let _ = query_ids(&core, &ctx_a, &vectors[0], 10);

    for i in 0..10 {
        let query = &vectors[i * (BASE_ROWS / 10)];
        let sql = format!(
            "SELECT id FROM docs WHERE tag = 'x' ORDER BY embedding <=> '{}' LIMIT 10",
            vec_literal(query)
        );
        let got = core.execute_sql(&ctx_a, &sql).expect("filtered query").rows;
        for row in &got {
            assert!(
                row.id <= BASE_ROWS as u64,
                "tenant-a mask-path result must never include tenant-b row id {}",
                row.id
            );
        }
    }
    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.subset_mask_scans > 0,
        "this fixture must exercise the no-copy mask path (non-vacuous)"
    );
}

/// T7: `EXPLAIN` は検索本体を実行しない契約（Issue #411）が Issue #676 後も
/// 不変であることを固定する——`ann_plan: hnsw_subset` を報告しつつ、
/// `subset_mask_scans`／`subset_arena_copies`／`fallbacks` のいずれも増えない。
#[test]
fn explain_still_reports_hnsw_subset_without_touching_new_counters() {
    use engine::query_planner::{LlmClient, PlanError};
    use engine::sql::exec::{Cell, ColumnMeta};
    use engine::sql::mode::SessionState;
    use engine::sql::SqlOutcome;

    // `EXPLAIN` は `USING PLAN(...)` 経由の statement のみ受理する契約
    // （`tests/sql_explain.rs::explain_rejects_plain_select_without_using_plan`）
    // のため、`USING PLAN` が要求する `body` 列を追加したスキーマを使う。
    struct StubLlmClient;
    impl LlmClient for StubLlmClient {
        fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
            Ok(r#"{"search_terms": [], "path_hint": null, "kind_hint": null}"#.to_string())
        }
    }

    let dir = unique_db_path("hnsw-subset-mask-t7");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("tag", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let vectors = gen_clustered_corpus(505, DIM as usize, BASE_ROWS, 6);
    let op_id = OperationId::parse("hnsw-subset-mask-t7").expect("valid op id");
    let metadata_x = engine::row_codec::encode_scalar_columns(
        &schema,
        &[
            Value::Null,
            Value::Text("docs/a.md".to_string()),
            Value::Text("x".to_string()),
            Value::Text("body content".to_string()),
        ],
    )
    .expect("encode tag=x metadata");
    let metadata_y = engine::row_codec::encode_scalar_columns(
        &schema,
        &[
            Value::Null,
            Value::Text("docs/a.md".to_string()),
            Value::Text("y".to_string()),
            Value::Text("body content".to_string()),
        ],
    )
    .expect("encode tag=y metadata");
    let rows: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let metadata = if i % 2 == 0 {
                metadata_x.as_slice()
            } else {
                metadata_y.as_slice()
            };
            (
                i as u64 + 1,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: v.as_slice(),
                    metadata,
                },
            )
        })
        .collect();
    engine::tenant::insert_rows(&storage, "docs", &ctx, &rows, &op_id).expect("seed rows");

    let ratio = engine::hnsw::Ratio {
        numerator: 60,
        denominator: 100,
    };
    let core = EngineCore::from_storage_with_engine(storage, hnsw_kind_with_full_scan_ratio(ratio))
        .with_query_planner(Box::new(StubLlmClient));
    let _ = query_ids(&core, &ctx, &vectors[0], 10);

    let before = core.hnsw_index_cache_stats();
    let sql = "EXPLAIN SELECT id FROM docs WHERE tag = 'x' USING PLAN('find content') LIMIT 10"
        .to_string();
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, &sql)
        .expect("EXPLAIN should succeed");
    let SqlOutcome::Explain(result) = outcome else {
        panic!("expected SqlOutcome::Explain");
    };
    assert_eq!(
        result.columns,
        vec![ColumnMeta::Computed {
            name: "QUERY PLAN".to_string(),
            ty: Some(engine::catalog::ColumnType::Text),
        }]
    );
    let text: String = result
        .rows
        .iter()
        .map(|row| match &row.cells[0] {
            Cell::Text(s) => s.clone(),
            other => panic!("expected Cell::Text, got {other:?}"),
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("ann_plan: hnsw_subset"),
        "EXPLAIN output must still report ann_plan: hnsw_subset, got: {text}"
    );

    let after = core.hnsw_index_cache_stats();
    assert_eq!(
        before.subset_mask_scans, after.subset_mask_scans,
        "EXPLAIN must not execute the search itself (subset_mask_scans unchanged)"
    );
    assert_eq!(
        before.subset_arena_copies, after.subset_arena_copies,
        "EXPLAIN must not execute the search itself (subset_arena_copies unchanged)"
    );
    assert_eq!(
        before.fallbacks, after.fallbacks,
        "EXPLAIN must not execute the search itself (fallbacks unchanged)"
    );
}

/// T8: `LIMIT` が候補件数未満の境界でも、候補 id マスク経路が既定エンジンと
/// 完全一致することを固定する（Top-k 境界の取りこぼしがないことの確認）。
#[test]
fn limit_below_candidate_count_top_k_boundary() {
    let dir = unique_db_path("hnsw-subset-mask-t8");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = tag_schema();
    storage.create_table(&schema).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let vectors = gen_clustered_corpus(606, DIM as usize, BASE_ROWS, 6);
    seed_tagged_corpus(&storage, &schema, &ctx, "t8", &vectors, |i| i % 2 == 0);

    let ratio = engine::hnsw::Ratio {
        numerator: 60,
        denominator: 100,
    };
    let core = EngineCore::from_storage_with_engine(storage, hnsw_kind_with_full_scan_ratio(ratio));

    let ref_dir = unique_db_path("hnsw-subset-mask-t8-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage.create_table(&schema).expect("create ref table");
    seed_tagged_corpus(&ref_storage, &schema, &ctx, "t8-ref", &vectors, |i| {
        i % 2 == 0
    });
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let _ = query_ids(&core, &ctx, &vectors[0], 10);

    // LIMIT=1（600 件の候補に対し極端に小さい）で Top-1 境界を検証する。
    let sql = format!(
        "SELECT id FROM docs WHERE tag = 'x' ORDER BY embedding <=> '{}' LIMIT 1",
        vec_literal(&vectors[0])
    );
    let got = core.execute_sql(&ctx, &sql).expect("filtered query").rows;
    let want = ref_core
        .execute_sql(&ctx, &sql)
        .expect("filtered query (ref)")
        .rows;
    let got_ids: Vec<u64> = got.iter().map(|r| r.id).collect();
    let want_ids: Vec<u64> = want.iter().map(|r| r.id).collect();
    assert_eq!(
        got_ids, want_ids,
        "LIMIT below candidate count must match the default engine exactly"
    );
}
