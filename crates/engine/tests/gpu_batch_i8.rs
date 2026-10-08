//! `gpu_batch::packed_i8::GpuI8BatchBackend` の結合テスト（Issue #542・親
//! #541・Phase 5 親 #460）。
//!
//! `tests/gpu_batch.rs` は GPU 有無いずれの環境でも意味のある回帰になるよう
//! `eprintln!` による skip 分岐を許しているが、本ファイルはそれをそのまま
//! 踏襲すると「GPU はあるが i8 パイプライン生成が naga に拒否された」
//! ようなリグレッションを `InitFailed` → skip で隠してしまう
//! （advisor レビュー指摘）。そこで既存の f16 常駐バックエンド
//! （[`GpuBatchBackend`]）の `try_new` が成功する環境では「この環境に GPU は
//! ある」と確定できるため、その場合は `GpuI8BatchBackend::try_new` の失敗を
//! そのまま assert 失敗として扱う（vacuous pass を防ぐ）。f16 バックエンドも
//! 失敗する環境（CI の GitHub ホステッド runner 等）でのみ本当に skip する。

use engine::batch_fallback::BatchBackend;
use engine::batch_search::{BatchQuery, ResidentMatrix};
#[cfg(feature = "bench-internals")]
use engine::gpu_batch::packed_i8::{dot_i8_packed_ref, encode_rows, quantize_query};
use engine::gpu_batch::packed_i8::{GpuI8BatchBackend, GpuI8Options};
use engine::gpu_batch::GpuBatchBackend;
use engine::policy::PolicyContext;
use engine::storage::Visibility;

fn ctx_with_private(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Private, Visibility::Public])
        .expect("valid tenant id")
}

struct Fixture {
    ids: Vec<u64>,
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
    dim: usize,
    vectors: Vec<f32>,
}

/// 6 行・dim=5（4 の倍数でない奇数次元でパディング経路を確認）・
/// tenant-a/tenant-b 混在フィクスチャ。レーン値が非対称になるよう構成し、
/// パック時のレーン順取り違えを検出できるようにする。
fn fixture() -> Fixture {
    Fixture {
        ids: vec![1, 2, 3, 4, 5, 6],
        tenant_ids: vec![
            "tenant-a".to_string(),
            "tenant-a".to_string(),
            "tenant-a".to_string(),
            "tenant-b".to_string(),
            "tenant-b".to_string(),
            "tenant-b".to_string(),
        ],
        visibilities: vec![Visibility::Private; 6],
        dim: 5,
        #[rustfmt::skip]
        vectors: vec![
            1.0, 0.0, 0.3, -0.2, 0.9,
            0.0, 1.0, -0.5, 0.1, 0.4,
            2.0, -1.0, 0.2, 0.7, -0.3,
            0.0, 2.0, 1.1, -0.9, 0.05,
            -1.0, -1.0, 0.6, 0.6, 0.6,
            3.0, 0.5, -0.8, 0.2, 1.5,
        ],
    }
}

fn build_matrix(fx: &Fixture) -> ResidentMatrix {
    ResidentMatrix::build(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
    )
    .expect("resident matrix build should succeed for well-formed fixture")
}

/// この開発環境に実 GPU があるかどうかを、既存の f16 常駐バックエンド
/// （[`GpuBatchBackend`]）の初期化可否で確定する（モジュール冒頭コメント
/// 参照）。i8 バックエンドの `try_new` 失敗を隠れた skip にしないためのゲート。
fn gpu_definitely_available(fx: &Fixture) -> bool {
    GpuBatchBackend::try_new(build_matrix(fx)).is_ok()
}

/// GPU が初期化できない環境では `try_new` が panic せず `InitFailed` を返す
/// （CORE-8 と同様の「初期化失敗は `Err` で返す」契約の回帰）。GPU が使える
/// 環境ではこのテスト自体は何も主張せず終了する。
#[test]
fn i8_backend_try_new_fails_closed_when_gpu_unavailable() {
    let fx = fixture();
    if gpu_definitely_available(&fx) {
        eprintln!("gpu available in this environment; init-failure branch not exercised here");
        return;
    }
    let err = GpuI8BatchBackend::try_new(build_matrix(&fx), GpuI8Options::default())
        .err()
        .expect("try_new should fail closed when no gpu is available");
    assert!(matches!(
        err,
        engine::batch_fallback::BatchBackendError::InitFailed(_)
    ));
}

/// `GpuI8Options::oversample` の範囲外値は GPU デバイスへ触れる前に拒否
/// される（D9）。GPU 有無に関わらず実行できる。
#[test]
fn i8_backend_rejects_out_of_range_oversample() {
    let fx = fixture();
    for bad in [0usize, 33] {
        let err = GpuI8BatchBackend::try_new(build_matrix(&fx), GpuI8Options { oversample: bad })
            .err()
            .expect("out-of-range oversample should be rejected");
        assert!(matches!(
            err,
            engine::batch_fallback::BatchBackendError::InitFailed(_)
        ));
    }
}

/// GPU が利用可能な環境でのみ実走する中核テスト: 生の i32 スコア
/// （`bench-internals` feature 限定フック）が CPU 参照実装
/// （[`dot_i8_packed_ref`]）と完全一致すること（「CPU i8 経路との整数一致」の
/// 実体）。奇数次元（dim=5、4 の倍数でない）を含む。
#[cfg(feature = "bench-internals")]
#[test]
fn i8_backend_raw_scores_match_cpu_reference_when_gpu_available() {
    let fx = fixture();
    if !gpu_definitely_available(&fx) {
        eprintln!("gpu unavailable in this environment, skipping");
        return;
    }
    let matrix = build_matrix(&fx);
    let backend = GpuI8BatchBackend::try_new(matrix, GpuI8Options::default()).expect(
        "i8 backend try_new must succeed once the f16 backend already confirmed gpu availability",
    );

    let (_row_scales, packed_rows) = encode_rows(fx.dim, fx.ids.len(), &fx.vectors)
        .expect("encode_rows should succeed for well-formed fixture");
    let row_stride = fx.dim.div_ceil(4);

    let ctx_a = ctx_with_private("tenant-a");
    let query = [0.5f32, -0.2, 1.0, 0.3, -0.7];
    let bq = BatchQuery {
        vector: &query,
        k: 3,
        ctx: &ctx_a,
    };

    let raw = backend
        .batch_search_raw_i32_for_tests(std::slice::from_ref(&bq))
        .expect("raw i32 score dispatch should succeed once gpu is available");
    assert_eq!(raw.len(), 1);
    let per_query = &raw[0];
    // tenant-a の可視行（id 1,2,3 → slot 0,1,2）のみが現れること。
    assert_eq!(per_query.len(), 3);

    let (_s_q, qq) = quantize_query(&query).expect("query quantize should succeed");
    for &(slot, gpu_score) in per_query {
        let row_start = (slot as usize) * row_stride;
        let row_end = row_start + row_stride;
        let row = packed_rows
            .get(row_start..row_end)
            .expect("slot should be within packed rows");
        let cpu_score = dot_i8_packed_ref(row, &qq);
        assert_eq!(
            gpu_score, cpu_score,
            "gpu i32 score for slot {slot} does not match cpu reference"
        );
    }
}

/// 混在テナントバッチで他テナントの id が混入しないこと（GPU 利用可能な
/// 環境でのみ実走）。最終結果は D8 の f32 再スコア契約により CPU オラクル
/// （既存 `GpuBatchBackend` の f16 常駐真値）と同じ可視性判定を通る。
#[test]
fn i8_backend_mixed_tenant_batch_has_no_cross_tenant_leak_when_gpu_available() {
    let fx = fixture();
    if !gpu_definitely_available(&fx) {
        eprintln!("gpu unavailable in this environment, skipping");
        return;
    }
    let matrix = build_matrix(&fx);
    let backend = GpuI8BatchBackend::try_new(matrix, GpuI8Options::default()).expect(
        "i8 backend try_new must succeed once the f16 backend already confirmed gpu availability",
    );

    let ctx_a = ctx_with_private("tenant-a");
    let ctx_b = ctx_with_private("tenant-b");
    let query_a = [1.0f32, 0.0, 0.0, 0.0, 0.0];
    let query_b = [0.0f32, 1.0, 0.0, 0.0, 0.0];
    let bq_a = BatchQuery {
        vector: &query_a,
        k: 3,
        ctx: &ctx_a,
    };
    let bq_b = BatchQuery {
        vector: &query_b,
        k: 3,
        ctx: &ctx_b,
    };

    let hits = engine::batch_fallback::BatchBackend::batch_search(&backend, &[bq_a, bq_b])
        .expect("i8 batch_search should succeed once gpu is available");
    assert_eq!(hits.len(), 2);
    for hit in &hits[0].hits {
        assert!(
            (1..=3).contains(&hit.id),
            "unexpected id {} leaked into tenant-a result",
            hit.id
        );
        assert_eq!(hit.tenant_id, "tenant-a");
    }
    for hit in &hits[1].hits {
        assert!(
            (4..=6).contains(&hit.id),
            "unexpected id {} leaked into tenant-b result",
            hit.id
        );
        assert_eq!(hit.tenant_id, "tenant-b");
    }

    // stats() が非 vacuous であること（実際に dispatch・再スコアが発生した）。
    let stats = backend.stats();
    assert!(stats.dispatches >= 1);
    assert!(stats.rescored_candidates >= 1);

    // meta(): backend は有効な wgpu::Backend、dot4_impl は D12 の契約どおり
    // 常に Undetermined（wgpu 30.0.1 の公開 API では判別できないため）。
    let meta = backend.meta();
    assert_eq!(
        meta.dot4_impl,
        engine::gpu_batch::packed_i8::Dot4I8Impl::Undetermined
    );
}

/// 同一入力を 2 回実行した場合の決定性（id・スコア列が一致すること）。
#[test]
fn i8_backend_batch_search_is_deterministic_when_gpu_available() {
    let fx = fixture();
    if !gpu_definitely_available(&fx) {
        eprintln!("gpu unavailable in this environment, skipping");
        return;
    }
    let backend1 = GpuI8BatchBackend::try_new(build_matrix(&fx), GpuI8Options::default())
        .expect("i8 backend try_new must succeed once gpu availability is confirmed");
    let backend2 = GpuI8BatchBackend::try_new(build_matrix(&fx), GpuI8Options::default())
        .expect("i8 backend try_new must succeed once gpu availability is confirmed");

    let ctx_a = ctx_with_private("tenant-a");
    let query = [0.5f32, -0.2, 1.0, 0.3, -0.7];
    let bq = BatchQuery {
        vector: &query,
        k: 3,
        ctx: &ctx_a,
    };

    let hits1 =
        engine::batch_fallback::BatchBackend::batch_search(&backend1, std::slice::from_ref(&bq))
            .expect("batch_search should succeed");
    let hits2 =
        engine::batch_fallback::BatchBackend::batch_search(&backend2, std::slice::from_ref(&bq))
            .expect("batch_search should succeed");

    let extract = |hits: &engine::batch_search::BatchHit| -> Vec<(u64, u32)> {
        hits.hits
            .iter()
            .map(|h| (h.id, h.score.to_bits()))
            .collect()
    };
    assert_eq!(extract(&hits1[0]), extract(&hits2[0]));
}

/// 既存 GPU テスト（f16 常駐・CORE-16 対照経路）が本 Issue の変更後も green
/// であることの回帰アンカー（受け入れ条件「既存 GPU テストが green」）。
/// 実行自体は `tests/gpu_batch.rs`・`tests/batch_fallback.rs`・
/// `tests/gpu_scaling_accept.rs` が担うため、本テストはそれらのテストが
/// 同一バイナリに存在することの索引としてのみ機能する（実 CI コマンドは
/// `cargo test -p fandhe-db-engine --all-features --test gpu_batch --test
/// batch_fallback --test gpu_scaling_accept`。README・実装計画ポインタ）。
#[test]
fn existing_gpu_tests_are_run_via_separate_test_binaries_note() {
    // 本テストは常に成功する（ドキュメント目的のプレースホルダ）。
}

// ---------------------------------------------------------------------
// brute-force 対照 Recall（Issue #543）。`benches/gpu_scaling_bench.rs` の
// 実測（`docs/design/gpu-batch-i8-packed.md`「前後比較実測（Issue #543）」
// 節参照）を単体テストとして固定する。決定的な中規模合成コーパスを本
// ファイル内で自前生成する（`tests/hnsw_search.rs::gen_clustered_corpus` と
// 同型の xorshift64* 決定的 RNG。`tests/*.rs` の「crate 外の公開 API のみで
// 検証する」流儀のため独立に複製する）。
// ---------------------------------------------------------------------

/// 決定的シードの xorshift64*（`tests/hnsw_search.rs::TestRng` と同一アルゴリズム）。
struct RecallTestRng {
    state: u64,
}

impl RecallTestRng {
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

    fn next_f32(&mut self) -> f32 {
        let bits = (self.next_u64() >> 40) as u32;
        (bits as f32) / (1u32 << 24) as f32
    }

    fn next_unit(&mut self) -> f32 {
        self.next_f32() * 2.0 - 1.0
    }
}

/// クラスタ構造ありコーパス（`tests/hnsw_search.rs::gen_clustered_corpus` と
/// 同方針）。L2 正規化はしない——i8 経路は cosine 前提の HNSW と異なり、任意の
/// f32 内積（dot）をそのまま量子化・再スコアする契約（`packed_i8.rs` モジュール
/// 冒頭コメント参照）のため、正規化の有無は本テストの主張に影響しない。
fn gen_clustered_corpus_i8(seed: u64, dim: usize, rows: usize, clusters: usize) -> Vec<f32> {
    let mut center_rng = RecallTestRng::new(seed ^ 0xC1C1_C1C1_C1C1_C1C1);
    let centers: Vec<Vec<f32>> = (0..clusters.max(1))
        .map(|_| (0..dim).map(|_| center_rng.next_unit()).collect())
        .collect();
    let mut rng = RecallTestRng::new(seed);
    let mut out = Vec::with_capacity(rows * dim);
    for i in 0..rows {
        let center = &centers[i % centers.len()];
        let v: Vec<f32> = center.iter().map(|c| c + rng.next_unit() * 0.2).collect();
        out.extend(v);
    }
    out
}

/// 一様乱数のみのコーパス（クラスタ構造なし）。
fn gen_uniform_corpus_i8(seed: u64, dim: usize, rows: usize) -> Vec<f32> {
    let mut rng = RecallTestRng::new(seed);
    let mut out = Vec::with_capacity(rows * dim);
    for _ in 0..rows {
        out.extend((0..dim).map(|_| rng.next_unit()));
    }
    out
}

/// brute-force 厳密対照（`BatchEngine`。CPU-SIMD 経路）と i8 経路の Recall@10
/// を比較する共通ヘルパー。`ids`/`ctx` は単一テナント Public（RLS 判定は本
/// テストの主張に無関係のため単純化）。GPU 利用不能環境では実行せず何も
/// 主張しない（`gpu_definitely_available` ゲート。モジュール冒頭コメント参照）。
fn measure_i8_recall_vs_brute_force(
    corpus: &[f32],
    dim: usize,
    rows: usize,
    query_count: usize,
    oversample: usize,
) -> Option<f64> {
    let ids: Vec<u64> = (0..rows as u64).collect();
    let tenant_ids = vec!["i8-recall-tenant".to_string(); rows];
    let visibilities = vec![Visibility::Public; rows];

    let brute_matrix = ResidentMatrix::build(&ids, &tenant_ids, &visibilities, dim, corpus)
        .expect("resident matrix build should succeed for well-formed corpus");
    let brute_engine = engine::batch_search::BatchEngine::new(brute_matrix);

    let i8_matrix = ResidentMatrix::build(&ids, &tenant_ids, &visibilities, dim, corpus)
        .expect("resident matrix build should succeed for well-formed corpus");
    let gpu_definitely_available_check = GpuBatchBackend::try_new(
        ResidentMatrix::build(&ids, &tenant_ids, &visibilities, dim, corpus)
            .expect("resident matrix build should succeed for well-formed corpus"),
    )
    .is_ok();
    if !gpu_definitely_available_check {
        eprintln!("gpu unavailable in this environment, skipping i8 recall measurement");
        return None;
    }
    let i8_backend = GpuI8BatchBackend::try_new(i8_matrix, GpuI8Options { oversample })
        .expect("i8 backend try_new must succeed once gpu availability is confirmed");

    let ctx = PolicyContext::new("i8-recall-tenant").expect("valid tenant id");
    // クエリ生成シードは oversample に依存させない（固定値のみ）。
    // `i8_backend_recall_is_monotone_non_decreasing_in_oversample_when_gpu_available`
    // が同一コーパス・同一クエリ集合で oversample=1/4/8 の Recall を比較する
    // 前提であり、シードを oversample から独立させないと各 oversample で
    // 異なるクエリ集合を比較することになり単調性（非減少）契約を正しく
    // 検証できない（codex-review 指摘・Cursor Bugbot 重複指摘）。
    let mut query_rng = RecallTestRng::new(0xF00D_F00D_0000_0001);
    let queries: Vec<Vec<f32>> = (0..query_count)
        .map(|_| (0..dim).map(|_| query_rng.next_unit()).collect())
        .collect();
    let batch_queries: Vec<BatchQuery<'_>> = queries
        .iter()
        .map(|q| BatchQuery {
            vector: q.as_slice(),
            k: 10,
            ctx: &ctx,
        })
        .collect();

    let brute_hits = brute_engine
        .batch_search(&batch_queries)
        .expect("brute-force batch_search should succeed");
    let i8_hits = i8_backend
        .batch_search(&batch_queries)
        .expect("i8 batch_search should succeed");
    assert_eq!(brute_hits.len(), i8_hits.len());

    let mut recall_sum = 0.0f64;
    for (baseline, candidate) in brute_hits.iter().zip(i8_hits.iter()) {
        let expected: std::collections::HashSet<u64> = baseline.hits.iter().map(|h| h.id).collect();
        if expected.is_empty() {
            continue;
        }
        let actual: std::collections::HashSet<u64> = candidate.hits.iter().map(|h| h.id).collect();
        let matched = actual.intersection(&expected).count();
        recall_sum += matched as f64 / expected.len() as f64;
    }
    let stats = i8_backend.stats();
    assert!(
        stats.rescored_candidates > 0,
        "i8 backend must have rescored a non-zero number of candidates (non-vacuous)"
    );
    Some(recall_sum / query_count as f64)
}

/// 中規模合成フィクスチャ（クラスタ構造あり・一様乱数の 2 種）で、i8 経路の
/// brute-force 対照 Recall@10 が HNSW 系テスト（`tests/hnsw_search.rs`）と同水準
/// の閾値 0.9 以上であることを固定する（既定 oversample=4）。
#[test]
fn i8_backend_recall_at_k_vs_cpu_brute_force_meets_threshold_when_gpu_available() {
    const DIM: usize = 128;
    const ROWS: usize = 2_000;
    const QUERY_COUNT: usize = 30;
    const OVERSAMPLE: usize = 4;

    let clustered = gen_clustered_corpus_i8(0xA5A5_1234_5501_0000, DIM, ROWS, 20);
    if let Some(recall) =
        measure_i8_recall_vs_brute_force(&clustered, DIM, ROWS, QUERY_COUNT, OVERSAMPLE)
    {
        assert!(
            recall >= 0.9,
            "clustered corpus i8 recall@10 too low: {recall}"
        );
    }

    let uniform = gen_uniform_corpus_i8(0xB6B6_5678_5501_0000, DIM, ROWS);
    if let Some(recall) =
        measure_i8_recall_vs_brute_force(&uniform, DIM, ROWS, QUERY_COUNT, OVERSAMPLE)
    {
        assert!(
            recall >= 0.9,
            "uniform corpus i8 recall@10 too low: {recall}"
        );
    }
}

/// oversample を 1 → 4 → 8 と広げたとき、i8 経路の Recall@10（brute-force 対照）が
/// 非減少であることを固定する（候補幅拡大で悪化しない契約。
/// `packed_i8.rs::GpuI8Options::oversample` ドキュメンテーションコメント参照）。
#[test]
fn i8_backend_recall_is_monotone_non_decreasing_in_oversample_when_gpu_available() {
    const DIM: usize = 128;
    const ROWS: usize = 2_000;
    const QUERY_COUNT: usize = 30;

    let corpus = gen_clustered_corpus_i8(0x1357_9BDF_5501_0000, DIM, ROWS, 10);

    let mut recalls: Vec<f64> = Vec::new();
    for oversample in [1usize, 4, 8] {
        match measure_i8_recall_vs_brute_force(&corpus, DIM, ROWS, QUERY_COUNT, oversample) {
            Some(r) => recalls.push(r),
            None => {
                // GPU 利用不能環境（`measure_i8_recall_vs_brute_force` が
                // `None` を返す）では全 oversample で同じ理由により測定
                // できないため、ここで即座に何も主張せず終了する。
                return;
            }
        }
    }

    for pair in recalls.windows(2) {
        let (prev, next) = (pair[0], pair[1]);
        assert!(
            next >= prev - 1e-9,
            "recall should be non-decreasing as oversample grows: {recalls:?}"
        );
    }
}
