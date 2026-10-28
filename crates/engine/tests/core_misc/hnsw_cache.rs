//! `sql::hnsw_cache::HnswIndexCache`（Issue #408・親 #402・前提 #404〜#407）の SQL
//! 表層結合テスト。`EngineCore::execute_sql` 経由で `SearchEngineKind::Hnsw` opt-in
//! エンジンを使い、以下を検証する（対象ビヘイビア CORE-9・CORE-10・TASK-132・
//! ポインタのみ）:
//!
//! - R1: 構築 → 差分 brute-force（追加挿入） → 再構築（さらなる追加挿入） →
//!   `update_row`/`delete_row` の各段で、既定エンジン（brute-force 対照）に対する
//!   Recall@10 が本リポの回帰基準（0.9 目安）以上であること。統計カウンタ
//!   （`builds`/`delta_searches`/`rebuilds`）で経路が実際に非 vacuous に動いたこと
//!   を固定する。
//! - R4: テナント境界（`(table, ctx)` 完全一致キー）。他テナントの private 行が
//!   結果へ混入しない・エントリが ctx ごとに独立していること。
//! - フィルタ付き（`WHERE`）DISTANCE クエリは `HnswIndexCache` の `FullVisible`
//!   エントリを占有せず、既定エンジンと完全一致すること（適用条件の遵守）。
//! - hybrid クエリの密側再取得ループ（`sql::hnsw_hybrid::HnswDenseProvider`。
//!   Issue #410）は既定エンジン対照 Recall@10 が本リポの回帰基準（0.9 目安）
//!   以上・可視外テナント非混入・実際に索引経路が非 vacuous に使われること
//!   （`hybrid_dense_searches > 0`）を満たすこと。
//! - `precision` モードのフィルタなし DISTANCE クエリは `HnswIndexCache` を経由
//!   せず（Bugbot 指摘。TASK-162・SEARCH-9）、既定エンジンの確信度ゲート判定と
//!   完全一致すること。
//! - Rust API（`VectorCore::search`）は本キャッシュを経由しない（`sql` 表層専用の
//!   段階化。既存 Issue #407 の契約を維持）。
//! - Issue #515: HNSW 索引ノードの F16 常駐（Issue #514）opt-in でも上記の
//!   Recall・非漏えい契約が同水準で成立すること（`f16_*` テストとして R4・
//!   hybrid・Rust API・`Subset` 形状・`full_scan_ratio` ANN 側のそれぞれに
//!   対応する版を追加）。加えて範囲外成分による自動縮退（D6）が SQL 表層
//!   経由でも非漏えい・Recall を維持したまま fail-closed に働くこと。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, VectorCore};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::storage::{RowInput, Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// ---------- 決定的擬似乱数（`tests/hnsw_search.rs::TestRng` の複製。結合テストは
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

/// クラスタ構造を持つ、L2 正規化済みの決定的コーパス（`tests/hnsw_search.rs::
/// gen_clustered_corpus` と同型の複製）。
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
// （2,048）未満に収め、構築グラフを決定的に保つ（`hnsw_cache.rs` モジュール
// ドキュメント・実装計画の fixture 方針参照）。
const BASE_ROWS: usize = 1_200;

fn schema(dim: u32) -> TableSchema {
    TableSchema::new(
        "docs",
        vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
    )
}

fn seed_rows(storage: &Storage, tenant: &str, start_id: u64, vectors: &[Vec<f32>], tag: &str) {
    let ctx = PolicyContext::new(tenant).expect("valid tenant");
    let rows: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                start_id + i as u64,
                RowInput {
                    tenant_id: tenant,
                    visibility: Visibility::Public,
                    embedding: v.as_slice(),
                    metadata: &[],
                },
            )
        })
        .collect();
    let op_id = OperationId::parse(&format!("hnsw-cache-seed-{tag}")).expect("valid operation_id");
    engine::tenant::insert_rows(storage, "docs", &ctx, &rows, &op_id).expect("seed rows batch");
}

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

fn recall_at_k(got: &[u64], want: &[u64]) -> f64 {
    if want.is_empty() {
        return 1.0;
    }
    let want_set: std::collections::HashSet<u64> = want.iter().copied().collect();
    let hits = got.iter().filter(|id| want_set.contains(id)).count();
    hits as f64 / want.len() as f64
}

/// `search_engine::hnsw_kind`（既定 `HnswParams` の唯一の検証入口）が返す
/// 検証済み `SearchEngineKind::Hnsw` へ、常駐精度 opt-in（`ResidentPrecision`。
/// Issue #514・#515）を適用するテスト専用ヘルパ。`precision ==
/// ResidentPrecision::F32` のときは `hnsw_kind(HnswParams::default())` と
/// 完全に等価（`ValidatedHnswParams` の既定値が F32 のため）。
fn hnsw_kind_with(precision: engine::hnsw::ResidentPrecision) -> search_engine::SearchEngineKind {
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    match kind {
        search_engine::SearchEngineKind::Hnsw(validated) => {
            search_engine::SearchEngineKind::Hnsw(validated.with_resident_precision(precision))
        }
        other => panic!("hnsw_kind must return SearchEngineKind::Hnsw, got {other:?}"),
    }
}

/// [`hnsw_kind_with`] に加え、ACORN-1 の 2-hop 展開（Issue #501。
/// `ValidatedHnswParams::acorn_max_visible_ratio`）を opt-in する統合テスト
/// 専用ヘルパ。
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

/// 既定エンジン対照 Recall@10 の合格基準（回帰基準 0.9 目安）を精度別に返す。
/// F32／F16 は本リポの既存回帰基準（0.9）をそのまま使うが、I8（SQ8。1 成分
/// あたり ±127 段の対称量子化）は候補生成の探索順序への影響が F16（ほぼ
/// 無損失な半精度）より大きく、本ファイルの小規模フィクスチャ（`DIM=16`・
/// `BASE_ROWS=1_200`）では実測で Recall@10 が 0.86〜0.89 程度まで下がる
/// テストが存在した（Issue #523）。索引ヒットの最終スコアは常に
/// `kernel::dot` の f32 再計算のため探索順序のみへの影響であり、下記の
/// 0.8 は「量子化ノイズによる候補漏れの許容枠」として本リポ独自に設定した
/// 実装既定値（spec 由来の閾値ではない）。
fn min_recall_for(precision: engine::hnsw::ResidentPrecision) -> f64 {
    match precision {
        engine::hnsw::ResidentPrecision::I8 => 0.8,
        engine::hnsw::ResidentPrecision::F32 | engine::hnsw::ResidentPrecision::F16 => 0.9,
    }
}

/// F16 常駐 opt-in（Issue #515）の非 vacuous 確認: 実際にエンジンへ opt-in が
/// 到達し（`search_engine_kind()` の Display に `resident=f16`）、この
/// コーパスでは自動縮退（D6）が発生していないこと（`f16_residency_fallbacks
/// == 0`）を固定する。F32 側では逆に `resident=f32` を固定する。
fn assert_resident_precision_reached(
    core: &EngineCore,
    precision: engine::hnsw::ResidentPrecision,
) {
    let expect_suffix = match precision {
        engine::hnsw::ResidentPrecision::F16 => "resident=f16",
        engine::hnsw::ResidentPrecision::F32 => "resident=f32",
        engine::hnsw::ResidentPrecision::I8 => "resident=i8",
    };
    let kind_display = core
        .search_engine_kind()
        .map(|k| k.to_string())
        .unwrap_or_default();
    assert!(
        kind_display.contains(expect_suffix),
        "expected search_engine_kind() display to contain {expect_suffix:?}, got {kind_display:?}"
    );
    if precision == engine::hnsw::ResidentPrecision::F16 {
        assert_eq!(
            core.hnsw_index_cache_stats().f16_residency_fallbacks,
            0,
            "embeddings in this fixture's corpus must stay within the f16 finite range \
             and must not trigger the F32 fallback (D6)"
        );
    }
    if precision == engine::hnsw::ResidentPrecision::I8 {
        assert_eq!(
            core.hnsw_index_cache_stats().i8_residency_fallbacks,
            0,
            "embeddings in this fixture's corpus must be finite and must not trigger \
             the F32 fallback (D6 と同型。Issue #521)"
        );
    }
}

/// R1: 構築 → 差分 brute-force → 再構築 → update/delete の段階遷移で Recall@10 が
/// 本リポの回帰基準（0.9）以上のまま維持され、統計カウンタが各段の経路を非
/// vacuous に固定することを検証する。
#[test]
fn r1_staged_transition_maintains_recall_and_exercises_all_cache_paths() {
    let dir = unique_db_path("hnsw-cache-r1-hnsw");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&schema(DIM)).expect("create table");

    let base_vectors = gen_clustered_corpus(1, DIM as usize, BASE_ROWS, 12);
    seed_rows(&storage, "tenant-a", 1, &base_vectors, "base");

    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind);

    // 対照用の brute-force 側は独立した Storage へ同一行を複製する。
    let ref_dir = unique_db_path("hnsw-cache-r1-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage
        .create_table(&schema(DIM))
        .expect("create ref table");
    seed_rows(&ref_storage, "tenant-a", 1, &base_vectors, "ref-base");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    // クエリはコーパス自身の点から抽出する（独立生成した乱数クラスタだと分布外
    // クエリになり、brute-force 対照でも HNSW 近似の Recall@10 が構造的に低下する
    // ため。`base_vectors` から等間隔に 20 点採る）。
    let queries: Vec<Vec<f32>> = (0..20)
        .map(|i| base_vectors[i * (BASE_ROWS / 20)].clone())
        .collect();

    // 段階 (i): 初回構築後。
    let mut recalls = Vec::new();
    for q in &queries {
        let got = query_ids(&core, &ctx, q, 10);
        let want = query_ids(&ref_core, &ctx, q, 10);
        recalls.push(recall_at_k(&got, &want));
    }
    let avg = recalls.iter().sum::<f64>() / recalls.len() as f64;
    assert!(avg >= 0.9, "stage (i) recall too low: {avg}");
    let stats = core.hnsw_index_cache_stats();
    assert_eq!(stats.builds, 1, "stage (i) must build exactly once");
    assert_eq!(stats.rebuilds, 0);

    // 段階 (ii): 5% 追加挿入後（差分 brute-force 経路）。
    let extra1 = gen_clustered_corpus(2, DIM as usize, BASE_ROWS / 20, 12);
    seed_rows_on_core(&core, "tenant-a", BASE_ROWS as u64 + 1, &extra1, "extra1");
    seed_rows_on_core(
        &ref_core,
        "tenant-a",
        BASE_ROWS as u64 + 1,
        &extra1,
        "ref-extra1",
    );
    let mut recalls2 = Vec::new();
    for q in &queries {
        let got = query_ids(&core, &ctx, q, 10);
        let want = query_ids(&ref_core, &ctx, q, 10);
        recalls2.push(recall_at_k(&got, &want));
    }
    let avg2 = recalls2.iter().sum::<f64>() / recalls2.len() as f64;
    assert!(avg2 >= 0.9, "stage (ii) recall too low: {avg2}");
    let stats2 = core.hnsw_index_cache_stats();
    assert_eq!(stats2.builds, 1, "stage (ii) must not rebuild yet");
    assert!(
        stats2.delta_searches > 0,
        "stage (ii) must exercise the delta brute-force path"
    );

    // 段階 (iii): さらに 15% 追加挿入後（差分比率が閾値を超え再構築）。
    let extra2 = gen_clustered_corpus(3, DIM as usize, (BASE_ROWS * 15) / 100, 12);
    let next_id = BASE_ROWS as u64 + extra1.len() as u64 + 1;
    seed_rows_on_core(&core, "tenant-a", next_id, &extra2, "extra2");
    seed_rows_on_core(&ref_core, "tenant-a", next_id, &extra2, "ref-extra2");
    let mut recalls3 = Vec::new();
    for q in &queries {
        let got = query_ids(&core, &ctx, q, 10);
        let want = query_ids(&ref_core, &ctx, q, 10);
        recalls3.push(recall_at_k(&got, &want));
    }
    let avg3 = recalls3.iter().sum::<f64>() / recalls3.len() as f64;
    assert!(avg3 >= 0.9, "stage (iii) recall too low: {avg3}");
    let stats3 = core.hnsw_index_cache_stats();
    assert_eq!(stats3.builds, 2, "stage (iii) must rebuild once");
    assert_eq!(stats3.rebuilds, 1);

    // 段階 (iv): id=1 の行をクエリと同一ベクトルへ `update_row` し、id=2 を
    // `delete_row` する。索引ヒットも `kernel::dot` で再計算するため、id=1 は
    // 両エンジンで完全一致の先頭ヒット・id=2 はいずれの結果にも現れないはず。
    let op_update = OperationId::parse("hnsw-cache-r1-update").expect("valid operation_id");
    core.update_row(
        &ctx,
        "docs",
        1,
        &RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: queries[0].as_slice(),
            metadata: &[],
        },
        Some(&op_update),
    )
    .expect("update row on hnsw core");
    ref_core
        .update_row(
            &ctx,
            "docs",
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: queries[0].as_slice(),
                metadata: &[],
            },
            Some(&op_update),
        )
        .expect("update row on ref core");

    let op_delete = OperationId::parse("hnsw-cache-r1-delete").expect("valid operation_id");
    core.delete_row(&ctx, "docs", 2, Some(&op_delete))
        .expect("delete row on hnsw core");
    ref_core
        .delete_row(&ctx, "docs", 2, Some(&op_delete))
        .expect("delete row on ref core");

    let got4 = query_ids(&core, &ctx, &queries[0], 10);
    let want4 = query_ids(&ref_core, &ctx, &queries[0], 10);
    assert_eq!(
        got4.first(),
        Some(&1u64),
        "the row updated to exactly match the query must rank first"
    );
    assert!(!got4.contains(&2), "the deleted row must not appear");
    assert!(!want4.contains(&2));
    let recall4 = recall_at_k(&got4, &want4);
    assert!(recall4 >= 0.9, "stage (iv) recall too low: {recall4}");
}

fn seed_rows_on_core(
    core: &EngineCore,
    tenant: &str,
    start_id: u64,
    vectors: &[Vec<f32>],
    tag: &str,
) {
    let ctx = PolicyContext::new(tenant).expect("valid tenant");
    for (i, v) in vectors.iter().enumerate() {
        let op_id =
            OperationId::parse(&format!("hnsw-cache-{tag}-{i}")).expect("valid operation_id");
        core.insert_row(
            &ctx,
            "docs",
            start_id + i as u64,
            &RowInput {
                tenant_id: tenant,
                visibility: Visibility::Public,
                embedding: v.as_slice(),
                metadata: &[],
            },
            Some(&op_id),
        )
        .expect("insert row via EngineCore");
    }
}

/// R4: テナント境界。tenant-a の private 行は tenant-b の可視結果に現れず、
/// キャッシュエントリは `(table, ctx)` ごとに独立する。
/// [`r4_tenant_isolation_never_leaks_across_ctx`]／[`f16_r4_tenant_isolation_never_leaks_across_ctx`]
/// が共有する本体（Issue #515。`precision` で常駐精度を切り替える）。
fn run_r4_tenant_isolation_never_leaks_across_ctx(precision: engine::hnsw::ResidentPrecision) {
    let dir = unique_db_path("hnsw-cache-r4");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&schema(DIM)).expect("create table");

    let a_vectors = gen_clustered_corpus(11, DIM as usize, BASE_ROWS, 8);
    let b_vectors = gen_clustered_corpus(22, DIM as usize, BASE_ROWS, 8);

    // `Private` 行を自テナントへ見せるには `PolicyContext::with_visibilities` で
    // 明示的に許可可視性集合へ含める必要がある（`PolicyContext::new` は `Public`
    // のみ許可する既定・最小権限のコンストラクタ。`policy.rs` モジュール
    // ドキュメント参照）。
    let ctx_a = PolicyContext::with_visibilities(
        "tenant-a",
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant");
    let ctx_b = PolicyContext::with_visibilities(
        "tenant-b",
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant");

    let rows_a: Vec<(u64, RowInput<'_>)> = a_vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                i as u64 + 1,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Private,
                    embedding: v.as_slice(),
                    metadata: &[],
                },
            )
        })
        .collect();
    let op_a = OperationId::parse("hnsw-cache-r4-a").expect("valid operation_id");
    engine::tenant::insert_rows(&storage, "docs", &ctx_a, &rows_a, &op_a).expect("seed tenant-a");

    let rows_b: Vec<(u64, RowInput<'_>)> = b_vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                i as u64 + 1,
                RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Private,
                    embedding: v.as_slice(),
                    metadata: &[],
                },
            )
        })
        .collect();
    let op_b = OperationId::parse("hnsw-cache-r4-b").expect("valid operation_id");
    engine::tenant::insert_rows(&storage, "docs", &ctx_b, &rows_b, &op_b).expect("seed tenant-b");

    let kind = hnsw_kind_with(precision);
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let query = &a_vectors[0];
    let got_a = query_ids(&core, &ctx_a, query, 10);
    assert!(!got_a.is_empty(), "tenant-a must see its own rows");

    let got_b = query_ids(&core, &ctx_b, query, 10);
    // tenant-b の private 行だけが返るはず（tenant-a の行は不可視）。id 空間は
    // 両テナントとも 1..=BASE_ROWS で重なるため、id 一致では判定できない代わりに
    // 件数のみ確認する（0 件にはならない: tenant-b 自身の行が存在する）。
    assert!(!got_b.is_empty());

    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.entries >= 2,
        "tenant-a and tenant-b must occupy independent cache entries"
    );
    assert_resident_precision_reached(&core, precision);
}

#[test]
fn r4_tenant_isolation_never_leaks_across_ctx() {
    run_r4_tenant_isolation_never_leaks_across_ctx(engine::hnsw::ResidentPrecision::F32);
}

/// Issue #515: F16 常駐でもテナント境界（`(table, ctx)` 完全一致キー）は
/// 不変であることを固定する。
#[test]
fn f16_r4_tenant_isolation_never_leaks_across_ctx() {
    run_r4_tenant_isolation_never_leaks_across_ctx(engine::hnsw::ResidentPrecision::F16);
}

/// Issue #523: I8（SQ8）常駐でもテナント境界（`(table, ctx)` 完全一致キー）は
/// 不変であることを固定する（f16 版と同型）。
#[test]
fn i8_r4_tenant_isolation_never_leaks_across_ctx() {
    run_r4_tenant_isolation_never_leaks_across_ctx(engine::hnsw::ResidentPrecision::I8);
}

/// フィルタ付き（`WHERE`）DISTANCE クエリは `HnswIndexCache` の `FullVisible`
/// エントリを一切占有しない（`entries == 0`。`Subset` 形状〔#409〕の別経路を
/// 使うため。`filtered_distance_uses_subset_shape_and_matches_default_engine_recall`
/// が `Subset` 経路自体の Recall を検証する）。hybrid クエリの契約は
/// `hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall`
/// （Issue #410 で新設。以前は本テストが「hybrid も含め常にキャッシュを迂回する」
/// ことを主張していたが、実際には hybrid の SQL を一切実行しておらずその主張は
/// 検証されていなかった。Issue #410 で hybrid 密側再取得ループを `sql::
/// hnsw_hybrid::HnswDenseProvider` 経由で結線したことにより、そもそも
/// 「hybrid は迂回する」という主張自体が成り立たなくなったため、本テストの
/// docstring・関数名から hybrid への言及を除いた）。
#[test]
fn filtered_distance_bypasses_full_visible_entries_and_matches_default_engine() {
    let dir = unique_db_path("hnsw-cache-bypass-hnsw");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("tag", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create table");

    let vectors = gen_clustered_corpus(5, DIM as usize, BASE_ROWS, 6);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op_id = OperationId::parse("hnsw-cache-bypass").expect("valid operation_id");
    let tag_metadata = engine::row_codec::encode_scalar_columns(
        &schema,
        &[
            engine::row_codec::Value::Null,
            engine::row_codec::Value::Text("x".to_string()),
        ],
    )
    .expect("encode tag metadata");
    let rows: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                i as u64 + 1,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: v.as_slice(),
                    metadata: tag_metadata.as_slice(),
                },
            )
        })
        .collect();
    engine::tenant::insert_rows(&storage, "docs", &ctx, &rows, &op_id).expect("seed rows");

    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let ref_dir = unique_db_path("hnsw-cache-bypass-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage.create_table(&schema).expect("create ref table");
    engine::tenant::insert_rows(&ref_storage, "docs", &ctx, &rows, &op_id).expect("seed ref rows");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let query = &vectors[0];
    // 全行が `tag = 'x'` を満たすため、フィルタは実質すべて通過させる（本テストの
    // 目的はフィルタあり経路が `HnswIndexCache` を経由しないことの確認であり、
    // フィルタ自体の選択性は問わない）。
    let sql = format!(
        "SELECT id FROM docs WHERE tag = 'x' ORDER BY embedding <=> '{}' LIMIT 10",
        vec_literal(query)
    );
    let got = core.execute_sql(&ctx, &sql).expect("filtered query").rows;
    let want = ref_core
        .execute_sql(&ctx, &sql)
        .expect("filtered query (ref)")
        .rows;
    assert_eq!(
        got.iter().map(|r| r.id).collect::<Vec<_>>(),
        want.iter().map(|r| r.id).collect::<Vec<_>>(),
        "filtered DISTANCE queries must bypass HnswIndexCache and match the default engine exactly"
    );

    let stats = core.hnsw_index_cache_stats();
    assert_eq!(
        stats.entries, 0,
        "filtered queries must never populate HnswIndexCache"
    );
}

/// hybrid クエリ（`ORDER BY HYBRID(...)`）の密側再取得ループ（Issue #410。
/// `sql::hnsw_hybrid::HnswDenseProvider`）が実際に非 vacuous に使われ、
/// 既定エンジン対照 Recall@10 が本リポの回帰基準（0.9 目安）以上・可視外
/// テナントの id が非混入であることを固定する（以前は hybrid が
/// `HnswIndexCache` を一切経由しない設計だったが、本 Issue で密側のみ結線した。
/// 疎索引側は Issue #357 の `SparseIndexCache` が別途担うため対象外）。
/// クラスタ語彙をキーワードに埋め込み、密・疎の両チャネルが同じクラスタへ
/// 一致するようクエリを組み立てる（`default_preset.rs::hybrid_corpus` と同じ
/// 「密・疎ともに当たる文書を作る」設計方針。ANN の近似性を許容するため
/// 完全一致ではなく Recall 基準で判定する）。
/// [`hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall`]／
/// [`f16_hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall`]
/// が共有する本体（Issue #515）。
fn run_hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall(
    precision: engine::hnsw::ResidentPrecision,
) {
    const HYBRID_DIM: u32 = 16;
    const CLUSTERS: usize = 6;
    let hybrid_schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(HYBRID_DIM), false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    );

    fn seed_hybrid_rows(
        storage: &Storage,
        tenant: &str,
        start_id: u64,
        vectors: &[Vec<f32>],
        visibility: Visibility,
    ) {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        for (i, v) in vectors.iter().enumerate() {
            let id = start_id + i as u64;
            let cluster = i % CLUSTERS;
            let body = format!("clusterword{cluster} filler text for hybrid recall fixture");
            let op_id =
                OperationId::parse(&format!("hnsw-hybrid-seed-{tenant}-{id}")).expect("op id");
            engine::tenant::insert_typed_row(
                storage,
                "docs",
                &ctx,
                id,
                visibility,
                &[Value::Vector(v.clone()), Value::Text(body)],
                &op_id,
            )
            .unwrap_or_else(|e| panic!("insert hybrid row id={id} failed: {e}"));
        }
    }

    let vectors = gen_clustered_corpus(11, HYBRID_DIM as usize, BASE_ROWS, CLUSTERS);

    let dir = unique_db_path("hnsw-cache-hybrid-hnsw");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&hybrid_schema).expect("create table");
    seed_hybrid_rows(&storage, "tenant-a", 1, &vectors, Visibility::Public);
    // tenant-b の private 行（不可視）。可視外混入がないことの検証対象。
    // `Visibility::Public` はテナントをまたいで可視になる（`PolicyContext::
    // is_visible` の契約。TABLE-9）ため、非漏えいを意味のある形で検証するには
    // `Private` を使う必要がある（`Public` のままだと tenant-a の ctx から
    // 正規に見えてしまい、テナント境界の検証にならない。R4
    // `r4_tenant_isolation_never_leaks_across_ctx` と同じ方針）。
    let other_vectors = gen_clustered_corpus(97, HYBRID_DIM as usize, 64, CLUSTERS);
    seed_hybrid_rows(
        &storage,
        "tenant-b",
        BASE_ROWS as u64 + 1,
        &other_vectors,
        Visibility::Private,
    );
    let kind = hnsw_kind_with(precision);
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let ref_dir = unique_db_path("hnsw-cache-hybrid-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage
        .create_table(&hybrid_schema)
        .expect("create ref table");
    seed_hybrid_rows(&ref_storage, "tenant-a", 1, &vectors, Visibility::Public);
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    const K: usize = 10;
    let mut total_hits = 0usize;
    let mut total_want = 0usize;
    for (cluster, query) in vectors.iter().enumerate().take(CLUSTERS) {
        let sql = format!(
            "SELECT id FROM docs ORDER BY HYBRID(embedding, '{}', body, 'clusterword{cluster}') LIMIT {K}",
            vec_literal(query)
        );
        let got = core.execute_sql(&ctx, &sql).expect("hybrid query").rows;
        let want = ref_core
            .execute_sql(&ctx, &sql)
            .expect("hybrid query (ref)")
            .rows;
        // RLS（対象ビヘイビア RLS-1〜4）自体が tenant-a の可視集合以外を
        // `execute_sql` へ一切渡さないため、可視外テナントの id 範囲
        // （`BASE_ROWS + 1 ..`。tenant-b の行）が含まれないことで非漏えいを
        // 固定する（`ResultRow` は `tenant_id` を保持しない設計のため、
        // R4（`r4_tenant_isolation_never_leaks_across_ctx`）と同じ id 範囲判定
        // 方式を使う）。
        for row in &got {
            assert!(
                row.id <= BASE_ROWS as u64,
                "hybrid search must never return a row from an invisible tenant (id={})",
                row.id
            );
        }
        let want_ids: std::collections::HashSet<u64> = want.iter().map(|r| r.id).collect();
        total_hits += got.iter().filter(|r| want_ids.contains(&r.id)).count();
        total_want += want_ids.len();
    }
    let min_recall = min_recall_for(precision);
    let recall = total_hits as f64 / total_want.max(1) as f64;
    assert!(
        recall >= min_recall,
        "hybrid recall@{K} against the default-engine reference must be >= {min_recall} (got {recall})"
    );

    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.hybrid_dense_searches > 0,
        "hybrid queries must exercise HnswDenseProvider (non-vacuous)"
    );
    assert!(
        stats.hybrid_queries > 0 && stats.hybrid_rounds_max > 0,
        "hybrid per-query round accounting must be non-vacuous"
    );
    assert_resident_precision_reached(&core, precision);
}

#[test]
fn hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall() {
    run_hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall(
        engine::hnsw::ResidentPrecision::F32,
    );
}

/// Issue #515: F16 常駐でも hybrid 密側再取得ループ（`HnswDenseProvider`）が
/// 既定エンジン対照 Recall@10 ≥ 0.9・可視外非混入を維持することを固定する。
#[test]
fn f16_hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall() {
    run_hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall(
        engine::hnsw::ResidentPrecision::F16,
    );
}

/// Issue #523: I8（SQ8）常駐でも hybrid 密側再取得ループ（`HnswDenseProvider`）
/// が既定エンジン対照 Recall@10 ≥ 0.8・可視外非混入を維持することを固定する
/// （f16 版と同型）。
#[test]
fn i8_hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall() {
    run_hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall(
        engine::hnsw::ResidentPrecision::I8,
    );
}

/// SCALAR 事前フィルタ付き hybrid（`Subset` 形状。`hnsw_hybrid_subset_eligible`。
/// Issue #410）: `WHERE` 付き hybrid クエリが `prepare_subset` 経由の per-query
/// 写像で候補マスク付き ANN 探索を経由し、既定エンジン対照 Recall@10 が本リポの
/// 回帰基準（0.9 目安）以上であること・`WHERE` を満たさない行が混入しないこと・
/// 実際に `Subset` 経路が非 vacuous に動いたこと（`subset_searches > 0` **かつ**
/// `hybrid_dense_searches > 0`）を固定する。`hnsw_hybrid_full_visible_eligible`
/// 版（`hybrid_queries_use_hnsw_dense_provider_and_match_default_engine_recall`）
/// と異なり `sql/exec.rs` の `hnsw_hybrid_subset_eligible` 分岐（新設・
/// `prepare_subset` 呼び出し）はこのテストでしか経由しないため、経路自体が
/// 生きていることを固定する目的を持つ。
#[test]
fn hybrid_queries_use_subset_shape_and_match_default_engine_recall() {
    const HYBRID_DIM: u32 = 16;
    const CLUSTERS: usize = 6;
    let hybrid_schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(HYBRID_DIM), false),
            ColumnDef::new("body", ColumnType::Text, false),
            ColumnDef::new("tag", ColumnType::Text, false),
        ],
    );

    fn seed_hybrid_rows_with_tag(storage: &Storage, tenant: &str, vectors: &[Vec<f32>]) {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        for (i, v) in vectors.iter().enumerate() {
            let id = i as u64 + 1;
            let cluster = i % CLUSTERS;
            let body = format!("clusterword{cluster} filler text for hybrid subset fixture");
            // 選択率 50%（偶奇）の `WHERE tag = 'x'`。
            let tag = if i % 2 == 0 { "x" } else { "y" };
            let op_id = OperationId::parse(&format!("hnsw-hybrid-subset-seed-{tenant}-{id}"))
                .expect("op id");
            engine::tenant::insert_typed_row(
                storage,
                "docs",
                &ctx,
                id,
                Visibility::Public,
                &[
                    Value::Vector(v.clone()),
                    Value::Text(body),
                    Value::Text(tag.to_string()),
                ],
                &op_id,
            )
            .unwrap_or_else(|e| panic!("insert hybrid subset row id={id} failed: {e}"));
        }
    }

    let vectors = gen_clustered_corpus(9, HYBRID_DIM as usize, BASE_ROWS, CLUSTERS);

    let dir = unique_db_path("hnsw-cache-hybrid-subset-hnsw");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&hybrid_schema).expect("create table");
    seed_hybrid_rows_with_tag(&storage, "tenant-a", &vectors);
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let ref_dir = unique_db_path("hnsw-cache-hybrid-subset-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage
        .create_table(&hybrid_schema)
        .expect("create ref table");
    seed_hybrid_rows_with_tag(&ref_storage, "tenant-a", &vectors);
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // フィルタなしクエリを 1 本先に投げ、`FullVisible` 経路に索引を構築させる
    // （`Subset` 経路は `Lookup::Miss` では構築せず plain scan へ縮退する契約——
    // `filtered_distance_uses_subset_shape_and_matches_default_engine_recall`
    // と同じ理由。§`prepare_subset` ドキュメンテーションコメント「2.」）。
    let _ = query_ids(&core, &ctx, &vectors[0], 10);

    const K: usize = 10;
    let mut total_hits = 0usize;
    let mut total_want = 0usize;
    for (cluster, query) in vectors.iter().enumerate().take(CLUSTERS) {
        let sql = format!(
            "SELECT id FROM docs WHERE tag = 'x' ORDER BY HYBRID(embedding, '{}', body, 'clusterword{cluster}') LIMIT {K}",
            vec_literal(query)
        );
        let got = core
            .execute_sql(&ctx, &sql)
            .expect("hybrid subset query")
            .rows;
        let want = ref_core
            .execute_sql(&ctx, &sql)
            .expect("hybrid subset query (ref)")
            .rows;
        // WHERE を満たさない行（tag='y'。1-indexed の偶数 id）が混入しないこと。
        for row in &got {
            assert_eq!(
                row.id % 2,
                1,
                "row {} does not satisfy tag='x' (1-indexed odd rows are tag='x')",
                row.id
            );
        }
        let want_ids: std::collections::HashSet<u64> = want.iter().map(|r| r.id).collect();
        total_hits += got.iter().filter(|r| want_ids.contains(&r.id)).count();
        total_want += want_ids.len();
    }
    let recall = total_hits as f64 / total_want.max(1) as f64;
    assert!(
        recall >= 0.9,
        "hybrid subset recall@{K} against the default-engine reference must be >= 0.9 (got {recall})"
    );

    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.subset_searches > 0,
        "Subset shape must be exercised for hybrid queries (non-vacuous)"
    );
    assert!(
        stats.hybrid_dense_searches > 0,
        "hybrid Subset queries must exercise HnswDenseProvider (non-vacuous)"
    );
}

/// Rust API（`VectorCore::search`）は Issue #409 で HNSW 世代整合キャッシュへ結線
/// された（`rls.rs::PrefilterSnapshot::search_with_hnsw`）。本テストの契約は
/// Issue #407〜#408 時点の「常にキャッシュを迂回し既定エンジンと完全一致する」
/// から、「既定エンジン対照 Recall@10 が本リポの回帰基準（0.9 目安）以上・
/// 可視外テナントの id が混入しない・実際にキャッシュ経路が働いたこと
/// （`hits >= 1`）」へ意図的に反転する（契約変更であり、アサーション弱体化では
/// ない。旧テストは ANN 経路が一切使われないことを固定していたが、Issue #409
/// はまさにその迂回を解消することが目的のため、ANN 特有の近似性を許容する
/// Recall 基準へ揃える必要がある。詳細は `docs/design/hnsw-rls-cardinality-switch.md`
/// 参照）。
/// [`rust_api_search_uses_hnsw_cache_and_matches_default_engine_recall`]／
/// [`f16_rust_api_search_uses_hnsw_cache_and_matches_default_engine_recall`]
/// が共有する本体（Issue #515）。
fn run_rust_api_search_uses_hnsw_cache_and_matches_default_engine_recall(
    precision: engine::hnsw::ResidentPrecision,
) {
    let dir = unique_db_path("hnsw-cache-rust-api-hnsw");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&schema(DIM)).expect("create table");
    let vectors = gen_clustered_corpus(7, DIM as usize, BASE_ROWS, 6);
    seed_rows(&storage, "tenant-a", 1, &vectors, "rust-api");
    // tenant-b の private 行（不可視）。可視外混入がないことの検証対象。
    let other_vectors = gen_clustered_corpus(70, DIM as usize, 64, 4);
    seed_rows(
        &storage,
        "tenant-b",
        BASE_ROWS as u64 + 1,
        &other_vectors,
        "rust-api-other-tenant",
    );
    let kind = hnsw_kind_with(precision);
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let ref_dir = unique_db_path("hnsw-cache-rust-api-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage
        .create_table(&schema(DIM))
        .expect("create ref table");
    seed_rows(&ref_storage, "tenant-a", 1, &vectors, "rust-api-ref");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    const K: usize = 10;
    const QUERIES: usize = 20;
    let mut total_hits = 0usize;
    for i in 0..QUERIES {
        let query = &vectors[i * (BASE_ROWS / QUERIES)];
        let got = core.search(&ctx, "docs", query, K).expect("search");
        let want = ref_core
            .search(&ctx, "docs", query, K)
            .expect("search (ref)");
        // 可視外テナント（tenant-b）の id が一切混入しないこと（TABLE-12・
        // security.md P0「テナント境界」）。
        for hit in &got {
            assert_eq!(
                hit.tenant_id, "tenant-a",
                "search must never return a row from an invisible tenant"
            );
        }
        let want_ids: std::collections::HashSet<u64> = want.iter().map(|h| h.id).collect();
        total_hits += got.iter().filter(|h| want_ids.contains(&h.id)).count();
    }
    let min_recall = min_recall_for(precision);
    let recall = total_hits as f64 / (QUERIES * K) as f64;
    assert!(
        recall >= min_recall,
        "recall@{K} against the default-engine reference must be >= {min_recall} (got {recall})"
    );

    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.hits + stats.plain_scans + stats.masked_short > 0,
        "Rust API search must exercise the HnswIndexCache path (non-vacuous)"
    );
    assert_resident_precision_reached(&core, precision);
}

#[test]
fn rust_api_search_uses_hnsw_cache_and_matches_default_engine_recall() {
    run_rust_api_search_uses_hnsw_cache_and_matches_default_engine_recall(
        engine::hnsw::ResidentPrecision::F32,
    );
}

/// Issue #515: F16 常駐でも Rust API（`VectorCore::search`）が `HnswIndexCache`
/// を経由して既定エンジン対照 Recall@10 ≥ 0.9・可視外非混入を維持することを
/// 固定する。
#[test]
fn f16_rust_api_search_uses_hnsw_cache_and_matches_default_engine_recall() {
    run_rust_api_search_uses_hnsw_cache_and_matches_default_engine_recall(
        engine::hnsw::ResidentPrecision::F16,
    );
}

/// Issue #523: I8（SQ8）常駐でも Rust API（`VectorCore::search`）が
/// `HnswIndexCache` を経由して既定エンジン対照 Recall@10 ≥ 0.8・可視外非混入を
/// 維持することを固定する（f16 版と同型）。
#[test]
fn i8_rust_api_search_uses_hnsw_cache_and_matches_default_engine_recall() {
    run_rust_api_search_uses_hnsw_cache_and_matches_default_engine_recall(
        engine::hnsw::ResidentPrecision::I8,
    );
}

/// SCALAR 事前フィルタ付き DISTANCE（`Subset` 形状。Issue #409）: フィルタなし
/// クエリが先に索引を構築した後、選択率 50% の `WHERE` 付きクエリが per-query
/// 写像（`sql::hnsw_cache::search_subset_or_fallback`）で候補マスク付き ANN
/// 探索を経由し、既定エンジン対照 Recall@10 が本リポの回帰基準（0.9 目安）以上
/// であること・`WHERE` を満たさない行が混入しないこと・実際に `Subset` 経路が
/// 動いたこと（`subset_searches > 0`）・`Subset` 経路がキャッシュへエントリを
/// 追加しないこと（§`search_subset_or_fallback` ドキュメンテーションコメント
/// 「3.」）を固定する。
/// [`filtered_distance_uses_subset_shape_and_matches_default_engine_recall`]／
/// [`f16_filtered_distance_uses_subset_shape_and_matches_default_engine_recall`]
/// が共有する本体（Issue #515）。
fn run_filtered_distance_uses_subset_shape_and_matches_default_engine_recall(
    precision: engine::hnsw::ResidentPrecision,
) {
    let dir = unique_db_path("hnsw-cache-subset-hnsw");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("tag", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create table");

    let vectors = gen_clustered_corpus(9, DIM as usize, BASE_ROWS, 6);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op_id = OperationId::parse("hnsw-cache-subset").expect("valid operation_id");
    let metadata_x = engine::row_codec::encode_scalar_columns(
        &schema,
        &[
            engine::row_codec::Value::Null,
            engine::row_codec::Value::Text("x".to_string()),
        ],
    )
    .expect("encode tag=x metadata");
    let metadata_y = engine::row_codec::encode_scalar_columns(
        &schema,
        &[
            engine::row_codec::Value::Null,
            engine::row_codec::Value::Text("y".to_string()),
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

    let kind = hnsw_kind_with(precision);
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let ref_dir = unique_db_path("hnsw-cache-subset-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage.create_table(&schema).expect("create ref table");
    engine::tenant::insert_rows(&ref_storage, "docs", &ctx, &rows, &op_id).expect("seed ref rows");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    // フィルタなしクエリを 1 本先に投げ、`FullVisible` 経路に索引を構築させる
    // （`Subset` 経路は `Lookup::Miss` では構築せず plain scan へ縮退する契約
    // ——§`search_subset_or_fallback` ドキュメンテーションコメント「2.」）。
    let _ = query_ids(&core, &ctx, &vectors[0], 10);
    let baseline_entries = core.hnsw_index_cache_stats().entries;
    assert_eq!(baseline_entries, 1, "unfiltered query must build one entry");

    const K: usize = 10;
    const QUERIES: usize = 20;
    let mut total_hits = 0usize;
    for i in 0..QUERIES {
        let query = &vectors[i * (BASE_ROWS / QUERIES)];
        let sql = format!(
            "SELECT id FROM docs WHERE tag = 'x' ORDER BY embedding <=> '{}' LIMIT {K}",
            vec_literal(query)
        );
        let got = core.execute_sql(&ctx, &sql).expect("filtered query").rows;
        let want = ref_core
            .execute_sql(&ctx, &sql)
            .expect("filtered query (ref)")
            .rows;
        // WHERE を満たさない行（tag='y', id が奇数）が混入しないこと。
        for row in &got {
            assert_eq!(
                row.id % 2,
                1,
                "row {} does not satisfy tag='x' (1-indexed odd rows are tag='x')",
                row.id
            );
        }
        let want_ids: std::collections::HashSet<u64> = want.iter().map(|r| r.id).collect();
        total_hits += got.iter().filter(|r| want_ids.contains(&r.id)).count();
    }
    let min_recall = min_recall_for(precision);
    let recall = total_hits as f64 / (QUERIES * K) as f64;
    assert!(
        recall >= min_recall,
        "filtered DISTANCE recall@{K} against the default engine must be >= {min_recall} (got {recall})"
    );

    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.subset_searches > 0,
        "Subset shape must be exercised (non-vacuous)"
    );
    assert_eq!(
        stats.entries, baseline_entries,
        "Subset shape must never register a cache entry"
    );
    assert_resident_precision_reached(&core, precision);
}

#[test]
fn filtered_distance_uses_subset_shape_and_matches_default_engine_recall() {
    run_filtered_distance_uses_subset_shape_and_matches_default_engine_recall(
        engine::hnsw::ResidentPrecision::F32,
    );
}

/// Issue #515: F16 常駐でも `Subset` 形状（SCALAR 事前フィルタ付き DISTANCE）
/// が既定エンジン対照 Recall@10 ≥ 0.9・可視外非混入を維持することを固定する。
#[test]
fn f16_filtered_distance_uses_subset_shape_and_matches_default_engine_recall() {
    run_filtered_distance_uses_subset_shape_and_matches_default_engine_recall(
        engine::hnsw::ResidentPrecision::F16,
    );
}

/// Issue #523: I8（SQ8）常駐でも `Subset` 形状（SCALAR 事前フィルタ付き
/// DISTANCE）が既定エンジン対照 Recall@10 ≥ 0.8・可視外非混入を維持することを
/// 固定する（f16 版と同型）。
#[test]
fn i8_filtered_distance_uses_subset_shape_and_matches_default_engine_recall() {
    run_filtered_distance_uses_subset_shape_and_matches_default_engine_recall(
        engine::hnsw::ResidentPrecision::I8,
    );
}

/// `Subset` 形状の早期打ち切り（Issue #488・`PreparedHnswSearch::PlainScanBelowRatio`）:
/// `WHERE` 適用後の部分集合カーディナリティ（`arena.len()`。索引済みノード数の
/// 上界）が `full_scan_ratio` 未満まで下がった場合に、`Overlay::compute` を
/// 経由せず plain scan へ縮退すること・既定エンジン対照 Recall@10 が回帰基準
/// 以上であること・`WHERE` を満たさない行が混入しないこと・実際にこの早期
/// 打ち切り分岐を経由したこと（`stats.plain_scans > 0` かつ `Overlay::compute`
/// を経由する `subset_searches == 0`）を固定する（対称版である `FullVisible`
/// 形状側の `full_scan_ratio_plain_scan_side_matches_brute_force_and_never_leaks_across_tenants`
/// に対応する `Subset` 形状側のテスト）。
#[test]
fn full_scan_ratio_plain_scan_below_ratio_subset_shape_matches_brute_force_and_never_leaks_across_tenants(
) {
    let dir = unique_db_path("hnsw-cache-ratio-subset");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("tag", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create table");

    let vectors = gen_clustered_corpus(53, DIM as usize, BASE_ROWS, 6);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let op_id = OperationId::parse("hnsw-cache-ratio-subset").expect("valid operation_id");
    let metadata_x = engine::row_codec::encode_scalar_columns(
        &schema,
        &[Value::Null, Value::Text("x".to_string())],
    )
    .expect("encode tag=x metadata");
    let metadata_y = engine::row_codec::encode_scalar_columns(
        &schema,
        &[Value::Null, Value::Text("y".to_string())],
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

    // tenant-b の行を同一テーブルへ混在させ、可視外テナントの id が結果へ
    // 混入しないことを検証できるようにする（TABLE-12・security.md P0）。
    // `seed_rows` ヘルパーは metadata を空にするため、`tag` 列（not null）を
    // 持つ本テストのスキーマでは使えず、`rows` と同じ手順で個別に組み立てる。
    let other_vectors = gen_clustered_corpus(54, DIM as usize, 100, 4);
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
    let other_rows: Vec<(u64, RowInput<'_>)> = other_vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                BASE_ROWS as u64 + 1 + i as u64,
                RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Public,
                    embedding: v.as_slice(),
                    metadata: metadata_x.as_slice(),
                },
            )
        })
        .collect();
    let op_other = OperationId::parse("hnsw-cache-ratio-subset-other").expect("valid operation_id");
    engine::tenant::insert_rows(&storage, "docs", &ctx_b, &other_rows, &op_other)
        .expect("seed tenant-b rows");

    // `full_scan_ratio` を 60/100 まで引き上げる。`tag = 'x'` の選択率は
    // ちょうど 50% であるため、`arena.len() / index.len() ≈ 0.5` が常に
    // 0.6 未満となり `PlainScanBelowRatio` を確実に踏む。
    let ratio = engine::hnsw::Ratio {
        numerator: 60,
        denominator: 100,
    };
    let params = engine::hnsw::ValidatedHnswParams::new(engine::hnsw::HnswParams::default())
        .expect("valid hnsw params")
        .with_full_scan_ratio(ratio)
        .expect("valid full_scan_ratio");
    let kind = engine::search_engine::SearchEngineKind::Hnsw(params);
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let ref_dir = unique_db_path("hnsw-cache-ratio-subset-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage.create_table(&schema).expect("create ref table");
    engine::tenant::insert_rows(&ref_storage, "docs", &ctx, &rows, &op_id).expect("seed ref rows");
    let op_other_ref =
        OperationId::parse("hnsw-cache-ratio-subset-other-ref").expect("valid operation_id");
    engine::tenant::insert_rows(&ref_storage, "docs", &ctx_b, &other_rows, &op_other_ref)
        .expect("seed ref tenant-b rows");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    // フィルタなしクエリを 1 本先に投げ、`FullVisible` 経路に索引を構築させる
    // （可視カーディナリティ比は構築直後 1.0 のため `full_scan_ratio`=0.6 を
    // 上回り、この呼び出し自体は plain scan を踏まない）。
    let _ = query_ids(&core, &ctx, &vectors[0], 10);
    let baseline_entries = core.hnsw_index_cache_stats().entries;
    assert_eq!(baseline_entries, 1, "unfiltered query must build one entry");

    const K: usize = 10;
    const QUERIES: usize = 20;
    let mut total_hits = 0usize;
    for i in 0..QUERIES {
        let query = &vectors[i * (BASE_ROWS / QUERIES)];
        let sql = format!(
            "SELECT id FROM docs WHERE tag = 'x' ORDER BY embedding <=> '{}' LIMIT {K}",
            vec_literal(query)
        );
        let got = core.execute_sql(&ctx, &sql).expect("filtered query").rows;
        let want = ref_core
            .execute_sql(&ctx, &sql)
            .expect("filtered query (ref)")
            .rows;
        for row in &got {
            assert_eq!(
                row.id % 2,
                1,
                "row {} does not satisfy tag='x' (1-indexed odd rows are tag='x')",
                row.id
            );
            assert!(
                row.id <= BASE_ROWS as u64,
                "tenant-a query must not return tenant-b row id {}",
                row.id
            );
        }
        let want_ids: std::collections::HashSet<u64> = want.iter().map(|r| r.id).collect();
        total_hits += got.iter().filter(|r| want_ids.contains(&r.id)).count();
    }
    let recall = total_hits as f64 / (QUERIES * K) as f64;
    assert!(
        recall >= 0.9,
        "plain-scan-side (below full_scan_ratio) Subset shape recall@{K} against the default engine must be >= 0.9 (got {recall})"
    );

    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.plain_scans > 0,
        "arena.len() / index.len() < full_scan_ratio must exercise the early-cutoff plain scan path (non-vacuous)"
    );
    assert_eq!(
        stats.subset_searches, 0,
        "early cutoff must skip Overlay::compute entirely, never touching the subset_searches counter"
    );
    assert_eq!(
        stats.entries, baseline_entries,
        "Subset early cutoff must never register a cache entry"
    );
}

/// 可視カーディナリティ比が `full_scan_ratio` 以上（既定 1/10。構築直後は
/// `visible_in_index == index.len()` で比 1.0）の場合、`FullVisible` 形状は
/// マスク付き ANN 探索（`search_masked`）側を選び、`plain_scans` を一切
/// 計上しないまま既定エンジン（brute-force）と同水準の Recall@10 を維持する
/// こと（Issue #409 受入基準 2「切替閾値の前後で結果が brute-force と同水準」の
/// ANN 側）。tenant-b の private 行が tenant-a の結果へ混入しないことも併せて
/// 固定する。
///
/// [`full_scan_ratio_ann_side_matches_brute_force_and_never_leaks_across_tenants`]／
/// [`f16_full_scan_ratio_ann_side_matches_brute_force_and_never_leaks_across_tenants`]
/// が共有する本体（Issue #515）。
fn run_full_scan_ratio_ann_side_matches_brute_force_and_never_leaks_across_tenants(
    precision: engine::hnsw::ResidentPrecision,
) {
    let dir = unique_db_path("hnsw-cache-ratio-ann");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&schema(DIM)).expect("create table");

    let a_vectors = gen_clustered_corpus(41, DIM as usize, BASE_ROWS, 10);
    seed_rows(&storage, "tenant-a", 1, &a_vectors, "ratio-ann-a");
    // tenant-b の private 行（id 空間を tenant-a と分離し、混入の有無を id 範囲
    // だけで判定できるようにする）。
    let b_vectors = gen_clustered_corpus(42, DIM as usize, 100, 4);
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Private]).expect("valid tenant");
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
                    metadata: &[],
                },
            )
        })
        .collect();
    let op_b = OperationId::parse("hnsw-cache-ratio-ann-b").expect("valid operation_id");
    engine::tenant::insert_rows(&storage, "docs", &ctx_b, &rows_b, &op_b).expect("seed tenant-b");

    // 既定の `full_scan_ratio`（1/10）をそのまま使う。
    let kind = hnsw_kind_with(precision);
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let ref_dir = unique_db_path("hnsw-cache-ratio-ann-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage
        .create_table(&schema(DIM))
        .expect("create ref table");
    seed_rows(&ref_storage, "tenant-a", 1, &a_vectors, "ratio-ann-ref-a");
    engine::tenant::insert_rows(&ref_storage, "docs", &ctx_b, &rows_b, &op_b)
        .expect("seed ref tenant-b");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    const K: usize = 10;
    const QUERIES: usize = 20;
    let mut total_hits = 0usize;
    for i in 0..QUERIES {
        let query = &a_vectors[i * (BASE_ROWS / QUERIES)];
        let got = query_ids(&core, &ctx_a, query, K);
        let want = query_ids(&ref_core, &ctx_a, query, K);
        for id in &got {
            assert!(
                *id <= BASE_ROWS as u64,
                "tenant-a result must not include tenant-b row id {id}"
            );
        }
        let want_set: std::collections::HashSet<u64> = want.iter().copied().collect();
        total_hits += got.iter().filter(|id| want_set.contains(id)).count();
    }
    let recall = total_hits as f64 / (QUERIES * K) as f64;
    let min_recall = min_recall_for(precision);
    assert!(
        recall >= min_recall,
        "ANN-side (ratio >= threshold) recall@{K} against default engine must be >= {min_recall} (got {recall})"
    );

    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.hits > 0,
        "ANN-side masked search must be exercised (non-vacuous)"
    );
    assert_eq!(
        stats.plain_scans, 0,
        "ratio >= full_scan_ratio must never fall back to plain scan"
    );
    assert_resident_precision_reached(&core, precision);
}

#[test]
fn full_scan_ratio_ann_side_matches_brute_force_and_never_leaks_across_tenants() {
    run_full_scan_ratio_ann_side_matches_brute_force_and_never_leaks_across_tenants(
        engine::hnsw::ResidentPrecision::F32,
    );
}

/// Issue #497: `ValidatedHnswParams::with_sparse_visited_max(usize::MAX)`
/// （マスク付き探索なら必ず [`engine::hnsw::VisitedSparse`] を選ぶ極端値）を
/// opt-in した provider でも、既定 engine（`sparse_visited_max` 既定値 0＝常に
/// dense）と同水準の Recall@10（≥ 0.9）を維持し、`HnswIndexCache` の統計
/// （`sparse_visited_searches > 0`。非 vacuous）に実際の採否が反映され、
/// tenant-b（Private）の private 行が tenant-a の結果へ混入しないことを固定
/// する。フィルタなし DISTANCE クエリ（`FullVisible` 形状）は可視行全体を
/// 受理する恒等マスクを使うため、`sparse_visited_max = usize::MAX` であれば
/// 索引ノード数に関わらず sparse 側が選ばれる。
#[test]
fn sparse_visited_max_opt_in_matches_default_engine_and_never_leaks_across_tenants() {
    let dir = unique_db_path("hnsw-cache-sparse-visited");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&schema(DIM)).expect("create table");

    let a_vectors = gen_clustered_corpus(71, DIM as usize, BASE_ROWS, 10);
    seed_rows(&storage, "tenant-a", 1, &a_vectors, "sparse-visited-a");
    // tenant-b の private 行（id 空間を tenant-a と分離し、混入の有無を id 範囲
    // だけで判定できるようにする。`run_full_scan_ratio_ann_side_matches_
    // brute_force_and_never_leaks_across_tenants` と同型）。
    let b_vectors = gen_clustered_corpus(72, DIM as usize, 100, 4);
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Private]).expect("valid tenant");
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
                    metadata: &[],
                },
            )
        })
        .collect();
    let op_b = OperationId::parse("hnsw-cache-sparse-visited-b").expect("valid operation_id");
    engine::tenant::insert_rows(&storage, "docs", &ctx_b, &rows_b, &op_b).expect("seed tenant-b");

    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let kind = match kind {
        search_engine::SearchEngineKind::Hnsw(validated) => {
            search_engine::SearchEngineKind::Hnsw(validated.with_sparse_visited_max(usize::MAX))
        }
        other => panic!("hnsw_kind must return SearchEngineKind::Hnsw, got {other:?}"),
    };
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let ref_dir = unique_db_path("hnsw-cache-sparse-visited-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage
        .create_table(&schema(DIM))
        .expect("create ref table");
    seed_rows(
        &ref_storage,
        "tenant-a",
        1,
        &a_vectors,
        "sparse-visited-ref-a",
    );
    engine::tenant::insert_rows(&ref_storage, "docs", &ctx_b, &rows_b, &op_b)
        .expect("seed ref tenant-b");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    const K: usize = 10;
    const QUERIES: usize = 20;
    let mut total_hits = 0usize;
    for i in 0..QUERIES {
        let query = &a_vectors[i * (BASE_ROWS / QUERIES)];
        let got = query_ids(&core, &ctx_a, query, K);
        let want = query_ids(&ref_core, &ctx_a, query, K);
        for id in &got {
            assert!(
                *id <= BASE_ROWS as u64,
                "tenant-a result must not include tenant-b row id {id}"
            );
        }
        let want_set: std::collections::HashSet<u64> = want.iter().copied().collect();
        total_hits += got.iter().filter(|id| want_set.contains(id)).count();
    }
    let recall = total_hits as f64 / (QUERIES * K) as f64;
    assert!(
        recall >= 0.9,
        "sparse_visited_max opt-in recall@{K} against default engine must be >= 0.9 (got {recall})"
    );

    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.hits > 0,
        "ANN-side masked search must be exercised (non-vacuous)"
    );
    // `sparse_visited_searches` は縮退なしで完走した全マスク付き探索
    // （`OverlaySuccessStat::Hits`／`SubsetSearches`／`None` のいずれでも）を
    // 数える診断カウンタで、`hits`（`Lookup::Ready` 到達分のみ）とは母集団が
    // 異なる（初回クエリは索引の初回構築を伴い `OverlaySuccessStat::None` に
    // 分類されるため、`sparse_visited_searches` の方が `hits` を上回りうる。
    // `search_with_overlay` ドキュメンテーションコメント参照）。ここでは
    // 「マスクがあれば必ず sparse を選ぶ」という本 opt-in の契約どおり、
    // 総クエリ数（`QUERIES`）以上（フォールバック等で `hits` に届かなかった
    // 呼び出しがあっても、その呼び出し自体は完走していれば計上される）である
    // ことのみを固定する。
    assert!(
        stats.sparse_visited_searches >= QUERIES as u64,
        "sparse_visited_max=usize::MAX must select VisitedSparse for every completed masked \
         search (mask is always Some and count_ones() < usize::MAX). stats={stats:?}"
    );
}

/// Issue #515: F16 常駐でも可視カーディナリティ比が `full_scan_ratio` 以上の
/// 場合はマスク付き ANN 探索側を選び、既定エンジン対照 Recall@10 ≥ 0.9・
/// 可視外非混入を維持することを固定する。
#[test]
fn f16_full_scan_ratio_ann_side_matches_brute_force_and_never_leaks_across_tenants() {
    run_full_scan_ratio_ann_side_matches_brute_force_and_never_leaks_across_tenants(
        engine::hnsw::ResidentPrecision::F16,
    );
}

/// Issue #523: I8（SQ8）常駐でも可視カーディナリティ比が `full_scan_ratio`
/// 以上の場合はマスク付き ANN 探索側を選び、既定エンジン対照 Recall@10 ≥ 0.8・
/// 可視外非混入を維持することを固定する（f16 版と同型）。
#[test]
fn i8_full_scan_ratio_ann_side_matches_brute_force_and_never_leaks_across_tenants() {
    run_full_scan_ratio_ann_side_matches_brute_force_and_never_leaks_across_tenants(
        engine::hnsw::ResidentPrecision::I8,
    );
}

/// D6（F16 常駐要求時、範囲外成分（`|x| > 65504.0`）を含む場合は索引全体を
/// F32 常駐へ自動縮退する。`hnsw.rs::HnswIndex::freeze_from` 参照）が SQL 表層
/// 経由でも fail-closed に働くことを固定する（Issue #515。`crates/engine/src/
/// sql/hnsw_cache.rs` の単体テスト
/// `f16_resident_precision_hits_the_ann_path_and_matches_default_engine_scores_exactly`
/// は範囲内成分のみを使う対照テストで、こちらは逆に縮退が実際に起きるケースを
/// 固定する）。tenant-a に 1 成分 `70000.0`（> 65504.0）を持つ行を 1 件だけ
/// 混ぜ、`f16_residency_fallbacks == 1`・`resident=f16`（静的設定は要求どおり
/// F16 のまま。実効的にはノードは F32 常駐だが opt-in 自体は取り消されない）・
/// tenant-b（Private）の可視外非混入・既定エンジン対照 Recall@10 ≥ 0.9 を
/// 固定する。
#[test]
fn f16_out_of_range_component_falls_back_to_f32_residency_without_leaking_or_losing_recall() {
    let dir = unique_db_path("hnsw-cache-f16-fallback");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&schema(DIM)).expect("create table");

    let mut a_vectors = gen_clustered_corpus(51, DIM as usize, BASE_ROWS, 6);
    // f16 の有限範囲（|x| <= 65504.0）を超える成分を 1 件だけ混ぜる
    // （`crate::f16::F16_MAX` は `pub(crate)` のため結合テストからは参照でき
    // ず、`hnsw.rs::freeze_from` ドキュメンテーションコメントに明記された
    // 65504.0 をそのまま使う）。
    a_vectors[0][0] = 70_000.0;
    seed_rows(&storage, "tenant-a", 1, &a_vectors, "f16-fallback-a");
    // tenant-b の private 行（不可視）。可視外混入がないことの検証対象
    // （`Public` はテナント横断可視のため `Private` を使う。R4 と同じ方針）。
    let b_vectors = gen_clustered_corpus(52, DIM as usize, 64, 4);
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Private]).expect("valid tenant");
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
                    metadata: &[],
                },
            )
        })
        .collect();
    let op_b = OperationId::parse("hnsw-cache-f16-fallback-b").expect("valid operation_id");
    engine::tenant::insert_rows(&storage, "docs", &ctx_b, &rows_b, &op_b).expect("seed tenant-b");

    let kind = hnsw_kind_with(engine::hnsw::ResidentPrecision::F16);
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let ref_dir = unique_db_path("hnsw-cache-f16-fallback-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage
        .create_table(&schema(DIM))
        .expect("create ref table");
    seed_rows(&ref_storage, "tenant-a", 1, &a_vectors, "f16-fallback-ref");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    const K: usize = 10;
    const QUERIES: usize = 20;
    let mut total_hits = 0usize;
    for i in 0..QUERIES {
        let query = &a_vectors[i * (BASE_ROWS / QUERIES)];
        let got = query_ids(&core, &ctx, query, K);
        // 可視外テナント（tenant-b）の id（`BASE_ROWS + 1 ..`）が一切混入しない
        // こと（TABLE-12・security.md P0「テナント境界」）。
        for id in &got {
            assert!(
                *id <= BASE_ROWS as u64,
                "must never return a row from an invisible tenant (id={id})"
            );
        }
        let want = query_ids(&ref_core, &ctx, query, K);
        let want_set: std::collections::HashSet<u64> = want.iter().copied().collect();
        total_hits += got.iter().filter(|id| want_set.contains(id)).count();
    }
    let recall = total_hits as f64 / (QUERIES * K) as f64;
    assert!(
        recall >= 0.9,
        "recall@{K} against the default-engine reference must be >= 0.9 even under D6 \
         fallback (got {recall})"
    );

    let stats = core.hnsw_index_cache_stats();
    assert_eq!(
        stats.f16_residency_fallbacks, 1,
        "the out-of-range component must trigger exactly one D6 fallback"
    );
    // 静的設定（opt-in 自体）は要求どおり F16 のまま——D6 縮退は索引ノードの
    // 実効表現のみを F32 へ切り替え、`ValidatedHnswParams::resident_precision`
    // が保持する opt-in 設定そのものは取り消さない
    // （`hnsw.rs::HnswIndex::resident_precision`・`freeze_from` 参照）。
    let kind_display = core
        .search_engine_kind()
        .map(|k| k.to_string())
        .unwrap_or_default();
    assert!(
        kind_display.contains("resident=f16"),
        "static opt-in configuration must remain resident=f16 even when D6 falls back \
         the effective node representation, got {kind_display:?}"
    );
}

/// D6（I8 常駐要求時、次元ごとスケール（`sq8::fit_dim_params`。次元 d の
/// 全行にわたる `max(|min_d|, |max_d|) / 127`）が f32 丸めで 0.0 へ
/// アンダーフローする場合は索引全体を F32 常駐へ自動縮退する。
/// `hnsw.rs::HnswIndex::freeze_from` 参照）が SQL 表層経由でも fail-closed に
/// 働くことを固定する（Issue #523・f16 版
/// `f16_out_of_range_component_falls_back_to_f32_residency_without_leaking_or_losing_recall`
/// と同型）。tenant-a 全行の次元 0 を `1e-44`（f32 subnormal。
/// `scale = 1e-44/127` が f32 の最小正 subnormal を下回り 0.0 へ丸まる。
/// `sq8.rs::fit_dim_params_rejects_scale_that_underflows_to_zero` 参照）へ
/// 揃え、`i8_residency_fallbacks == 1`・`resident=i8`（静的設定は要求どおり
/// I8 のまま）・tenant-b（Private）の可視外非混入・既定エンジン対照
/// Recall@10 ≥ 0.9 を固定する。
#[test]
fn i8_scale_underflow_dimension_falls_back_to_f32_residency_without_leaking_or_losing_recall() {
    let dir = unique_db_path("hnsw-cache-i8-fallback");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&schema(DIM)).expect("create table");

    let mut a_vectors = gen_clustered_corpus(53, DIM as usize, BASE_ROWS, 6);
    // 次元 0 を全行 1e-44 へ揃える（sq8.rs の同名ユニットテストと同じ値。
    // fit_dim_params の次元ごとスケールが f32 丸めで 0.0 へアンダーフローする
    // 唯一の到達経路）。
    for v in a_vectors.iter_mut() {
        v[0] = 1e-44;
    }
    seed_rows(&storage, "tenant-a", 1, &a_vectors, "i8-fallback-a");
    // tenant-b の private 行（不可視）。可視外混入がないことの検証対象。
    let b_vectors = gen_clustered_corpus(54, DIM as usize, 64, 4);
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Private]).expect("valid tenant");
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
                    metadata: &[],
                },
            )
        })
        .collect();
    let op_b = OperationId::parse("hnsw-cache-i8-fallback-b").expect("valid operation_id");
    engine::tenant::insert_rows(&storage, "docs", &ctx_b, &rows_b, &op_b).expect("seed tenant-b");

    let kind = hnsw_kind_with(engine::hnsw::ResidentPrecision::I8);
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let ref_dir = unique_db_path("hnsw-cache-i8-fallback-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage
        .create_table(&schema(DIM))
        .expect("create ref table");
    seed_rows(&ref_storage, "tenant-a", 1, &a_vectors, "i8-fallback-ref");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    const K: usize = 10;
    const QUERIES: usize = 20;
    let mut total_hits = 0usize;
    for i in 0..QUERIES {
        let query = &a_vectors[i * (BASE_ROWS / QUERIES)];
        let got = query_ids(&core, &ctx, query, K);
        // 可視外テナント（tenant-b）の id（`BASE_ROWS + 1 ..`）が一切混入しない
        // こと（TABLE-12・security.md P0「テナント境界」）。
        for id in &got {
            assert!(
                *id <= BASE_ROWS as u64,
                "must never return a row from an invisible tenant (id={id})"
            );
        }
        let want = query_ids(&ref_core, &ctx, query, K);
        let want_set: std::collections::HashSet<u64> = want.iter().copied().collect();
        total_hits += got.iter().filter(|id| want_set.contains(id)).count();
    }
    let recall = total_hits as f64 / (QUERIES * K) as f64;
    assert!(
        recall >= 0.9,
        "recall@{K} against the default-engine reference must be >= 0.9 even under D6 \
         fallback (got {recall})"
    );

    let stats = core.hnsw_index_cache_stats();
    assert_eq!(
        stats.i8_residency_fallbacks, 1,
        "the scale-underflowing dimension must trigger exactly one D6 fallback"
    );
    // 静的設定（opt-in 自体）は要求どおり I8 のまま——D6 縮退は索引ノードの
    // 実効表現のみを F32 へ切り替え、`ValidatedHnswParams::resident_precision`
    // が保持する opt-in 設定そのものは取り消さない
    // （`hnsw.rs::HnswIndex::resident_precision`・`freeze_from` 参照）。
    let kind_display = core
        .search_engine_kind()
        .map(|k| k.to_string())
        .unwrap_or_default();
    assert!(
        kind_display.contains("resident=i8"),
        "static opt-in configuration must remain resident=i8 even when D6 falls back \
         the effective node representation, got {kind_display:?}"
    );
}

/// 可視カーディナリティ比が `full_scan_ratio` 未満（構築後に少数行を削除し
/// `visible_in_index / index.len()` を閾値未満まで下げるが、再構築閾値
/// `REBUILD_DELTA_RATIO`〔1/10〕は超えない churn 幅に収める）の場合、
/// `FullVisible` 形状は plain scan（アリーナ全体の brute-force）側を選び、
/// 既定エンジンと完全一致する結果を返すこと（Issue #409 受入基準 2 の plain
/// scan 側）。tenant-b の private 行が混入しないことも併せて固定する。
#[test]
fn full_scan_ratio_plain_scan_side_matches_brute_force_and_never_leaks_across_tenants() {
    let dir = unique_db_path("hnsw-cache-ratio-plain");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&schema(DIM)).expect("create table");

    let a_vectors = gen_clustered_corpus(51, DIM as usize, BASE_ROWS, 10);
    seed_rows(&storage, "tenant-a", 1, &a_vectors, "ratio-plain-a");
    let b_vectors = gen_clustered_corpus(52, DIM as usize, 100, 4);
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Private]).expect("valid tenant");
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
                    metadata: &[],
                },
            )
        })
        .collect();
    let op_b = OperationId::parse("hnsw-cache-ratio-plain-b").expect("valid operation_id");
    engine::tenant::insert_rows(&storage, "docs", &ctx_b, &rows_b, &op_b).expect("seed tenant-b");

    // `full_scan_ratio` を 95/100 まで引き上げる。削除する行の churn 比（後述）を
    // `REBUILD_DELTA_RATIO`（1/10）以下に収めつつ、可視カーディナリティ比を
    // この閾値未満まで下げて plain scan 側を踏ませる。
    let ratio = engine::hnsw::Ratio {
        numerator: 95,
        denominator: 100,
    };
    let params = engine::hnsw::ValidatedHnswParams::new(engine::hnsw::HnswParams::default())
        .expect("valid hnsw params")
        .with_full_scan_ratio(ratio)
        .expect("valid full_scan_ratio");
    let kind = engine::search_engine::SearchEngineKind::Hnsw(params);
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let ref_dir = unique_db_path("hnsw-cache-ratio-plain-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage
        .create_table(&schema(DIM))
        .expect("create ref table");
    seed_rows(&ref_storage, "tenant-a", 1, &a_vectors, "ratio-plain-ref-a");
    engine::tenant::insert_rows(&ref_storage, "docs", &ctx_b, &rows_b, &op_b)
        .expect("seed ref tenant-b");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");

    // 索引を構築させる（フィルタなしクエリを 1 本）。
    let _ = query_ids(&core, &ctx_a, &a_vectors[0], 10);
    let stats_after_build = core.hnsw_index_cache_stats();
    assert_eq!(
        stats_after_build.builds, 1,
        "unfiltered query must build once"
    );

    // BASE_ROWS の 6%（72 行、id の末尾側でクエリに使わない範囲）を削除する。
    // churn 比 0.06 は `REBUILD_DELTA_RATIO`（0.1）以下のため再構築は起きず、
    // `visible_in_index / index.len()` = 0.94 だけが `full_scan_ratio`（0.95）
    // 未満まで下がる。
    let delete_count = (BASE_ROWS * 6) / 100;
    let delete_start = BASE_ROWS - delete_count;
    for i in delete_start..BASE_ROWS {
        let id = i as u64 + 1;
        let op = OperationId::parse(&format!("hnsw-cache-ratio-plain-del-{i}"))
            .expect("valid operation_id");
        core.delete_row(&ctx_a, "docs", id, Some(&op))
            .expect("delete row on hnsw core");
        ref_core
            .delete_row(&ctx_a, "docs", id, Some(&op))
            .expect("delete row on ref core");
    }

    const K: usize = 10;
    const QUERIES: usize = 20;
    // クエリは削除していない前半範囲からのみ選ぶ（削除済み行を指すクエリを
    // 避け、比較の焦点を切替判定そのものへ絞る）。
    let mut total_hits = 0usize;
    for i in 0..QUERIES {
        let query = &a_vectors[i * (delete_start / QUERIES)];
        let got = query_ids(&core, &ctx_a, query, K);
        let want = query_ids(&ref_core, &ctx_a, query, K);
        for id in &got {
            assert!(
                *id <= BASE_ROWS as u64,
                "tenant-a result must not include tenant-b row id {id}"
            );
            assert!(
                *id < delete_start as u64 + 1,
                "deleted row {id} must not appear in results"
            );
        }
        let want_set: std::collections::HashSet<u64> = want.iter().copied().collect();
        total_hits += got.iter().filter(|id| want_set.contains(id)).count();
    }
    let recall = total_hits as f64 / (QUERIES * K) as f64;
    assert!(
        recall >= 0.99,
        "plain-scan-side (ratio < threshold) recall@{K} against default engine must be >= 0.99 (got {recall})"
    );

    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.plain_scans > 0,
        "ratio < full_scan_ratio must exercise the plain scan path (non-vacuous)"
    );
    assert_eq!(
        stats.builds, 1,
        "churn within REBUILD_DELTA_RATIO must not trigger a rebuild"
    );
    assert_eq!(
        stats.subset_searches, 0,
        "unfiltered FullVisible queries must never use the Subset shape counter"
    );
}

/// `precision` モードのフィルタなし DISTANCE クエリ（Bugbot High 指摘。TASK-162・
/// SEARCH-9）: `HnswIndexCache` を経由せず、既定エンジン（brute-force）の確信度
/// ゲート判定と完全一致すること。索引済みノードの近似近傍を渡すと Top-2 マージン
/// を誤って過大評価し、確信度不足時に空集合を返すべき応答が非空になり得るため。
#[test]
fn precision_mode_bypasses_cache_and_matches_default_engine_gate_decision() {
    let dir = unique_db_path("hnsw-cache-precision-hnsw");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&schema(DIM)).expect("create table");

    // クラスタ構造ありコーパス（既存フィクスチャと同型）。クラスタ内の点同士は
    // 僅差になりやすく、precision の確信度ゲートが非自明に働く条件を作る。
    let vectors = gen_clustered_corpus(11, DIM as usize, BASE_ROWS, 12);
    seed_rows(&storage, "tenant-a", 1, &vectors, "precision-hnsw");
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let ref_dir = unique_db_path("hnsw-cache-precision-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage
        .create_table(&schema(DIM))
        .expect("create ref table");
    seed_rows(&ref_storage, "tenant-a", 1, &vectors, "precision-ref");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // クラスタ中心そのものをクエリに使い、クラスタ内の複数点が僅差で並ぶ状況を
    // 誘発する（`sql_precision_mode.rs` のクラスタ内クエリと同じ方針）。
    let queries: Vec<Vec<f32>> = (0..10)
        .map(|i| vectors[i * (BASE_ROWS / 10)].clone())
        .collect();

    for q in &queries {
        let sql = format!(
            "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT 3 USING MODE 'precision'",
            vec_literal(q)
        );
        let got = core
            .execute_sql(&ctx, &sql)
            .expect("precision query on hnsw core should succeed")
            .rows;
        let want = ref_core
            .execute_sql(&ctx, &sql)
            .expect("precision query on ref core should succeed")
            .rows;
        assert_eq!(
            got.iter().map(|r| r.id).collect::<Vec<_>>(),
            want.iter().map(|r| r.id).collect::<Vec<_>>(),
            "precision mode must match the default engine's gate decision exactly \
             (query={q:?})"
        );
    }

    let stats = core.hnsw_index_cache_stats();
    assert_eq!(
        stats.entries, 0,
        "precision mode must never populate HnswIndexCache"
    );
    assert_eq!(
        stats.hits, 0,
        "precision mode must never hit HnswIndexCache"
    );
    assert_eq!(
        stats.misses, 0,
        "precision mode must not even attempt an HnswIndexCache lookup"
    );
    assert_eq!(
        stats.builds, 0,
        "precision mode must not trigger HnswIndexCache builds"
    );

    // 対照: 同一テーブル・同一 core で `recall`（既定）モードのクエリを 1 回発行
    // すると `HnswIndexCache` が実際に使われる（本テストのアサーションが cache
    // 自体の機能不全ではなく、precision モードの適用除外を検証していることの
    // 裏付け）。
    let recall_sql = format!(
        "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT 3",
        vec_literal(&queries[0])
    );
    core.execute_sql(&ctx, &recall_sql)
        .expect("recall query should succeed");
    let stats_after_recall = core.hnsw_index_cache_stats();
    assert_eq!(
        stats_after_recall.builds, 1,
        "recall mode on the same table must still populate HnswIndexCache"
    );
}

/// ACORN-1 の 2-hop 展開（Issue #501・親 #500）が実際に効果を持つ最小フィク
/// スチャを構成する共通ヘルパ。`tag = 'x'` の選択率 20%（`i % 5 == 0`）は、
/// 同じコーパス上で `acorn_max_visible_ratio == None`（既定・`HopMode::OneHop`）
/// だと `mask_splits_graph` が 20/20 で発火する（本 Issue の実測で確認済み。
/// `docs/design/hnsw-rls-cardinality-switch.md`「Issue #501」節参照）ほど
/// 疎な可視集合を作る一方、`acorn_max_visible_ratio = 1/1` を指定すると
/// 同じ 20 クエリすべてが `subset_searches`（縮退なしの ANN 完走）へ転じる。
fn seed_acorn_fixture(
    storage: &Storage,
    schema: &TableSchema,
    ctx: &PolicyContext,
    op_tag: &str,
) -> Vec<Vec<f32>> {
    let vectors = gen_clustered_corpus(9, DIM as usize, BASE_ROWS, 6);
    let op_id = OperationId::parse(&format!("hnsw-cache-acorn-{op_tag}")).expect("valid op id");
    let metadata_x = engine::row_codec::encode_scalar_columns(
        schema,
        &[
            engine::row_codec::Value::Null,
            engine::row_codec::Value::Text("x".to_string()),
        ],
    )
    .expect("encode tag=x metadata");
    let metadata_y = engine::row_codec::encode_scalar_columns(
        schema,
        &[
            engine::row_codec::Value::Null,
            engine::row_codec::Value::Text("y".to_string()),
        ],
    )
    .expect("encode tag=y metadata");
    let rows: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            // 20% 選択率（`i % 5 == 0`）: `full_scan_ratio`（既定 1/10）は
            // 上回るが、OneHop では `mask_splits_graph` が発火するほど疎な
            // 可視カーディナリティ（§本関数ドキュメンテーションコメント）。
            let metadata = if i % 5 == 0 {
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
    engine::tenant::insert_rows(storage, "docs", ctx, &rows, &op_id).expect("seed rows");
    vectors
}

const ACORN_ALWAYS: engine::hnsw::Ratio = engine::hnsw::Ratio {
    numerator: 1,
    denominator: 1,
};

/// ACORN-1 の 2-hop 展開（Issue #501）を SCALAR 事前フィルタ付き DISTANCE
/// （`Subset` 形状）で有効化し、(1) このフィクスチャでは分断（`mask_splits_
/// graph`）が発生しないこと（フィクスチャ不備を先に切り分ける）、(2)
/// `acorn_searches`／`acorn_expansions` が非 vacuous であること、(3) 既定
/// エンジン対照 Recall@10 が本リポの回帰基準（0.9 目安）以上であること、
/// (4) tenant-b の private 行が結果へ混入しないこと、(5) `plain_scans == 0`
/// （TwoHop 経路が実際に選ばれ plain scan へ縮退していない）ことを固定する。
#[test]
fn acorn_two_hop_subset_shape_matches_default_engine_and_never_leaks_across_tenants() {
    let dir = unique_db_path("hnsw-cache-acorn-two-hop");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("tag", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let vectors = seed_acorn_fixture(&storage, &schema, &ctx_a, "a");

    // tenant-b の private 行（id 空間を tenant-a と分離）。
    let b_vectors = gen_clustered_corpus(42, DIM as usize, 100, 4);
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Private]).expect("valid tenant");
    let metadata_b = engine::row_codec::encode_scalar_columns(
        &schema,
        &[
            engine::row_codec::Value::Null,
            engine::row_codec::Value::Text("x".to_string()),
        ],
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
    let op_b = OperationId::parse("hnsw-cache-acorn-two-hop-b").expect("valid operation_id");
    engine::tenant::insert_rows(&storage, "docs", &ctx_b, &rows_b, &op_b).expect("seed tenant-b");

    let kind = hnsw_kind_with_acorn(ACORN_ALWAYS);
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let ref_dir = unique_db_path("hnsw-cache-acorn-two-hop-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage.create_table(&schema).expect("create ref table");
    let _ = seed_acorn_fixture(&ref_storage, &schema, &ctx_a, "two-hop-ref");
    engine::tenant::insert_rows(&ref_storage, "docs", &ctx_b, &rows_b, &op_b)
        .expect("seed ref tenant-b");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    // フィルタなしクエリを 1 本先に投げ、`FullVisible` 経路に索引を構築させる
    // （`Subset` 経路は `Lookup::Miss` では構築を試みない契約）。
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

    let stats = core.hnsw_index_cache_stats();
    // (1) フィクスチャ不備を先に切り分ける。
    assert_eq!(
        stats.mask_splits_graph, 0,
        "TwoHop must resolve the disconnection that OneHop hits on this fixture \
         (mask_splits_graph must stay 0); if this fails the fixture no longer \
         demonstrates ACORN-1's effect"
    );
    // (2) 非 vacuous。
    assert!(
        stats.acorn_searches > 0,
        "acorn_searches must be non-vacuous"
    );
    assert!(
        stats.acorn_expansions > 0,
        "acorn_expansions must be non-vacuous (bridge_expand must have run)"
    );
    // (3) 既定エンジン対照 Recall@10 >= 0.9。
    assert!(
        recall >= 0.9,
        "TwoHop subset-shape recall@{K} against the default engine must be >= 0.9 (got {recall})"
    );
    // (5) plain scan へ縮退していない。
    assert_eq!(
        stats.plain_scans, 0,
        "TwoHop must not fall back to plain scan on this fixture"
    );
    assert!(
        stats.subset_searches > 0,
        "Subset shape must be exercised (non-vacuous)"
    );
}

/// ACORN-1（Issue #501）が既定（`acorn_max_visible_ratio == None`）のとき、
/// 同じ疎なマスク・同じコーパスで `HopMode::OneHop`（既存契約）のまま挙動が
/// 変わらないことを固定する——`acorn_searches`／`acorn_expansions` は常に
/// `0` のまま、かつこのフィクスチャでは（§`seed_acorn_fixture` ドキュメン
/// テーションコメントのとおり）`mask_splits_graph` が発火し plain scan へ
/// 縮退する。opt-in しない限り本 Issue の変更が既存動作へ影響しないことの
/// 直接証拠（R1）。
#[test]
fn acorn_disabled_by_default_keeps_existing_behavior_unaffected() {
    let dir = unique_db_path("hnsw-cache-acorn-disabled");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("tag", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let vectors = seed_acorn_fixture(&storage, &schema, &ctx, "disabled");

    // `acorn_max_visible_ratio` を opt-in しない既定エンジン。
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind);

    let _ = query_ids(&core, &ctx, &vectors[0], 10);

    const K: usize = 10;
    const QUERIES: usize = 20;
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
        stats.acorn_searches, 0,
        "acorn_max_visible_ratio == None must never select HopMode::TwoHop"
    );
    assert_eq!(stats.acorn_expansions, 0);
    assert_eq!(
        stats.mask_splits_graph, QUERIES as u64,
        "without ACORN-1 opt-in, this fixture's sparse mask must keep splitting \
         the graph exactly as before Issue #501 (existing behavior unchanged)"
    );
}
