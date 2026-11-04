//! `gpu_batch.rs::GpuBatchBackend` の結合テスト（TASK-128〜130・対象ビヘイビア:
//! CORE-6, 8, 16。ポインタ: Issue #178）。
//!
//! 実行環境に GPU が無い場合（CI の GitHub ホステッド runner 等）でも意味の
//! ある回帰にするため、環境条件で両分岐を検証する（TASK-128 設計方針 §3.5。
//! skip・ignore にはしない）:
//! - GPU 初期化に失敗する環境: `FallbackBatchEngine::build_with_gpu` が
//!   `FallbackEvent{reason: Init, target: "cpu-simd"}` を 1 件通知し、結果が
//!   CPU オラクル（`CpuScalarProvider`）と一致すること（CORE-8 の実バックエンド
//!   に対する回帰）
//! - GPU 初期化に成功する環境: GPU 結果が CPU オラクルと一致し（id 集合一致・
//!   スコア相対誤差 1e-3 以内）、複数テナント混在バッチで混入 0 件・奇数次元を
//!   含めて検証する

use std::sync::{Arc, Mutex};

use engine::batch_fallback::{BatchBackend, FallbackBatchEngine, FallbackEvent, FallbackObserver};
use engine::batch_search::BatchQuery;
use engine::gpu_batch::{GpuBatchBackend, GpuF32ContrastBackend};
use engine::kernel::{CandidateHit, CpuScalarProvider, SearchInput, SearchProvider};
use engine::policy::PolicyContext;
use engine::storage::Visibility;

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant id")
}

/// `Visibility::Private` を許可する `PolicyContext`（既定の
/// `PolicyContext::new` は `Public` のみ許可するため、自テナントの
/// `Private` 行を見るにはこちらを使う）。
fn ctx_with_private(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Private, Visibility::Public])
        .expect("valid tenant id")
}

#[derive(Clone, Default)]
struct RecordingObserver(Arc<Mutex<Vec<FallbackEvent>>>);

impl RecordingObserver {
    fn events(&self) -> Vec<FallbackEvent> {
        self.0.lock().expect("lock").clone()
    }
}

impl FallbackObserver for RecordingObserver {
    fn on_fallback(&self, event: FallbackEvent) {
        self.0.lock().expect("lock").push(event);
    }
}

/// 常駐行列の元データ 1 式（`GpuBatchBackend`・CPU オラクルの双方に同じ
/// フィクスチャを渡すための束ね）。
struct Fixture {
    ids: Vec<u64>,
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
    dim: usize,
    vectors: Vec<f32>,
}

/// 4 行・dim=2・tenant-a/tenant-b が各 2 行のフィクスチャ（`tests/batch_fallback.rs`
/// と同一形状。オラクル比較を単純にするため揃える）。`private` が `true` の
/// 場合は各行を自テナントのみ可視（`Visibility::Private`）にする（テナント
/// 混入 0 件の検証用。`Visibility::Public` はテナント境界を跨いで可視になる
/// 設計のため、その検証には使えない）。
fn fixture(private: bool) -> Fixture {
    let visibility = if private {
        Visibility::Private
    } else {
        Visibility::Public
    };
    Fixture {
        ids: vec![1, 2, 3, 4],
        tenant_ids: vec![
            "tenant-a".to_string(),
            "tenant-a".to_string(),
            "tenant-b".to_string(),
            "tenant-b".to_string(),
        ],
        visibilities: vec![visibility; 4],
        dim: 2,
        #[rustfmt::skip]
        vectors: vec![
            1.0, 0.0,
            0.0, 1.0,
            2.0, 0.0,
            0.0, 2.0,
        ],
    }
}

/// CPU オラクル（`kernel.rs::CpuScalarProvider`）で 1 クエリを検索する。
/// `GpuBatchBackend`/`FallbackBatchEngine` の結果が「縮退後の CPU-SIMD 経路と
/// 構成的に一致する」契約（CORE-8）の基準として使う。`SearchInput` は
/// `core.rs` が可視性フィルタ済みの行だけを渡す契約（`kernel.rs` モジュール
/// ドキュメント参照）のため、本関数側で `PolicyContext::is_visible` により
/// 可視行だけへ絞り込んでから渡す。
fn cpu_oracle(fx: &Fixture, query: &[f32], k: usize, ctx: &PolicyContext) -> Vec<CandidateHit> {
    let provider = CpuScalarProvider;
    let mut visible_ids: Vec<u64> = Vec::new();
    let mut visible_vectors: Vec<f32> = Vec::new();
    for (((id, tenant), vis), vec) in fx
        .ids
        .iter()
        .zip(fx.tenant_ids.iter())
        .zip(fx.visibilities.iter())
        .zip(fx.vectors.chunks(fx.dim))
    {
        if ctx.is_visible(tenant, *vis) {
            visible_ids.push(*id);
            visible_vectors.extend_from_slice(vec);
        }
    }
    provider
        .search(SearchInput {
            ids: &visible_ids,
            vectors: &visible_vectors,
            dim: fx.dim as u32,
            query,
            k,
        })
        .expect("cpu oracle search should not fail on well-formed fixture")
}

/// GPU が初期化できない環境での CORE-8 回帰（実バックエンドに対する初期化
/// 失敗→縮退）。GPU が使える環境ではこのテスト自体は何も主張せず終了する
/// （成功パスは下の `*_when_gpu_available` 系テストが担う）。
#[test]
fn build_with_gpu_falls_back_to_cpu_when_gpu_unavailable() {
    let fx = fixture(false);
    let observer = RecordingObserver::default();
    let engine = FallbackBatchEngine::build_with_gpu(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
        Box::new(observer.clone()),
    )
    .expect("build_with_gpu should not fail on well-formed fixture regardless of gpu availability");

    let events = observer.events();
    if events.is_empty() {
        eprintln!("gpu available in this environment; init-failure branch not exercised here");
        return;
    }

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].reason.to_string(), "init");
    assert_eq!(events[0].target, "cpu-simd");

    let query = [1.0f32, 1.0];
    let c = ctx("tenant-a");
    let batch_query = BatchQuery {
        vector: &query,
        k: 4,
        ctx: &c,
    };
    let hits = engine
        .batch_search(std::slice::from_ref(&batch_query))
        .expect("cpu fallback batch_search should succeed");
    let expected = cpu_oracle(&fx, &query, 4, &c);
    let mut actual_ids: Vec<u64> = hits[0].hits.iter().map(|h| h.id).collect();
    let mut expected_ids: Vec<u64> = expected.iter().map(|h| h.id).collect();
    actual_ids.sort_unstable();
    expected_ids.sort_unstable();
    assert_eq!(actual_ids, expected_ids);
}

/// GPU が利用可能な環境でのみ実走する: `GpuBatchBackend` 単体の結果が CPU
/// オラクルと一致し、複数テナント混在バッチで他テナントの id が混入しない
/// ことを検証する。
#[test]
fn gpu_backend_matches_cpu_oracle_when_gpu_available() {
    let fx = fixture(true);
    let ctx_a = ctx_with_private("tenant-a");
    let ctx_b = ctx_with_private("tenant-b");
    let matrix = engine::batch_search::ResidentMatrix::build(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
    )
    .expect("resident matrix build should succeed for well-formed fixture");

    let backend = match GpuBatchBackend::try_new(matrix) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gpu unavailable in this environment, skipping: {e}");
            return;
        }
    };

    let query_a = [1.0f32, 1.0];
    let query_b = [2.0f32, 0.5];
    let bq_a = BatchQuery {
        vector: &query_a,
        k: 4,
        ctx: &ctx_a,
    };
    let bq_b = BatchQuery {
        vector: &query_b,
        k: 4,
        ctx: &ctx_b,
    };
    let hits = backend
        .batch_search(&[bq_a, bq_b])
        .expect("gpu batch_search should succeed once the device initialized");
    assert_eq!(hits.len(), 2);

    // テナント混入 0 件: tenant-a のクエリ結果には id 1,2 のみ、tenant-b の
    // クエリ結果には id 3,4 のみが現れること。
    for hit in &hits[0].hits {
        assert!(
            hit.id == 1 || hit.id == 2,
            "unexpected id {} leaked into tenant-a result",
            hit.id
        );
    }
    for hit in &hits[1].hits {
        assert!(
            hit.id == 3 || hit.id == 4,
            "unexpected id {} leaked into tenant-b result",
            hit.id
        );
    }

    let expected_a = cpu_oracle(&fx, &query_a, 4, &ctx_a);
    let mut actual_a: Vec<(u64, f32)> = hits[0].hits.iter().map(|h| (h.id, h.score)).collect();
    let mut expected_a_sorted: Vec<(u64, f32)> =
        expected_a.iter().map(|h| (h.id, h.score)).collect();
    actual_a.sort_by_key(|(id, _)| *id);
    expected_a_sorted.sort_by_key(|(id, _)| *id);
    assert_eq!(actual_a.len(), expected_a_sorted.len());
    for ((aid, ascore), (eid, escore)) in actual_a.iter().zip(expected_a_sorted.iter()) {
        assert_eq!(aid, eid);
        assert!(
            (ascore - escore).abs() < 1e-3,
            "score mismatch for id {aid}: gpu={ascore} cpu={escore}"
        );
    }
}

/// 奇数次元（`dim` が 2 で割り切れない）でも GPU 経路がパディングを正しく
/// 扱うこと（`ResidentMatrix::build` の f16 パックは奇数次元の最終ペアを
/// 0 埋めする。`gpu_batch.rs` のクエリ側パディングも同じ規約に揃える）。
#[test]
fn gpu_backend_handles_odd_dimension_when_gpu_available() {
    let ids = [1u64, 2];
    let tenant_ids = ["t".to_string(), "t".to_string()];
    let visibilities = [Visibility::Public, Visibility::Public];
    let dim = 3;
    let vectors = [1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0];
    let matrix = engine::batch_search::ResidentMatrix::build(
        &ids,
        &tenant_ids,
        &visibilities,
        dim,
        &vectors,
    )
    .expect("resident matrix build should succeed for well-formed fixture");

    let backend = match GpuBatchBackend::try_new(matrix) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gpu unavailable in this environment, skipping: {e}");
            return;
        }
    };

    let c = ctx("t");
    let query = [1.0f32, 1.0, 0.0];
    let bq = BatchQuery {
        vector: &query,
        k: 2,
        ctx: &c,
    };
    let hits = backend
        .batch_search(std::slice::from_ref(&bq))
        .expect("gpu batch_search should succeed once the device initialized");
    let mut scored: Vec<(u64, f32)> = hits[0].hits.iter().map(|h| (h.id, h.score)).collect();
    scored.sort_by_key(|(id, _)| *id);
    assert_eq!(scored.len(), 2);
    assert!((scored[0].1 - 1.0).abs() < 1e-3);
    assert!((scored[1].1 - 1.0).abs() < 1e-3);
}

// --- CORE-16（GPU 常駐コピーの f16 パック vs f32 常駐の A/B 対照経路。
// Issue #234・ポインタ: `docs/spec/04-behavior/core-engine.md` CORE-16）の
// 対照バックエンド `GpuF32ContrastBackend` に対する結合テスト。上の
// `GpuBatchBackend`（f16 パック常駐）用テストと同じ設計方針（GPU 有無の
// 両分岐を検証・skip/ignore にしない）を踏襲する。

/// GPU が初期化できない環境では `try_new` が panic せず `InitFailed` を返す
/// （CORE-8 と同様の「初期化失敗は `Err` で返す」契約の回帰）。GPU が使える
/// 環境ではこのテスト自体は何も主張せず終了する（成功パスは下の
/// `*_when_gpu_available` 系テストが担う）。
#[test]
fn f32_contrast_backend_try_new_fails_closed_when_gpu_unavailable() {
    let fx = fixture(false);
    match GpuF32ContrastBackend::try_new(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
    ) {
        Ok(_) => {
            eprintln!("gpu available in this environment; init-failure branch not exercised here");
        }
        Err(e) => {
            // `InitFailed` 以外のバリアントへ写像されていないこと（`try_new` は
            // 初期化系の失敗のみを返す契約。`gpu_batch.rs::GpuBatchBackend::try_new`
            // と同じ契約）。
            assert!(
                matches!(e, engine::batch_fallback::BatchBackendError::InitFailed(_)),
                "unexpected error variant on init failure: {e:?}"
            );
        }
    }
}

/// GPU が利用可能な環境でのみ実走する: `GpuF32ContrastBackend` 単体の結果が
/// CPU オラクルと一致し、複数テナント混在バッチで他テナントの id が混入しない
/// ことを検証する（`GpuBatchBackend` 側の同名テストと対になる回帰）。f32
/// 常駐は f16 パックのような量子化誤差を持たないため、許容誤差を f16 経路
/// より厳しくする。
#[test]
fn f32_contrast_backend_matches_cpu_oracle_when_gpu_available() {
    let fx = fixture(true);
    let ctx_a = ctx_with_private("tenant-a");
    let ctx_b = ctx_with_private("tenant-b");

    let backend = match GpuF32ContrastBackend::try_new(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
    ) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gpu unavailable in this environment, skipping: {e}");
            return;
        }
    };

    let query_a = [1.0f32, 1.0];
    let query_b = [2.0f32, 0.5];
    let bq_a = BatchQuery {
        vector: &query_a,
        k: 4,
        ctx: &ctx_a,
    };
    let bq_b = BatchQuery {
        vector: &query_b,
        k: 4,
        ctx: &ctx_b,
    };
    let hits = backend
        .batch_search(&[bq_a, bq_b])
        .expect("gpu batch_search should succeed once the device initialized");
    assert_eq!(hits.len(), 2);

    for hit in &hits[0].hits {
        assert!(
            hit.id == 1 || hit.id == 2,
            "unexpected id {} leaked into tenant-a result",
            hit.id
        );
    }
    for hit in &hits[1].hits {
        assert!(
            hit.id == 3 || hit.id == 4,
            "unexpected id {} leaked into tenant-b result",
            hit.id
        );
    }

    let expected_a = cpu_oracle(&fx, &query_a, 4, &ctx_a);
    let mut actual_a: Vec<(u64, f32)> = hits[0].hits.iter().map(|h| (h.id, h.score)).collect();
    let mut expected_a_sorted: Vec<(u64, f32)> =
        expected_a.iter().map(|h| (h.id, h.score)).collect();
    actual_a.sort_by_key(|(id, _)| *id);
    expected_a_sorted.sort_by_key(|(id, _)| *id);
    assert_eq!(actual_a.len(), expected_a_sorted.len());
    for ((aid, ascore), (eid, escore)) in actual_a.iter().zip(expected_a_sorted.iter()) {
        assert_eq!(aid, eid);
        // f32 常駐は f16 量子化を経ないため、GPU vs CPU オラクルの誤差は
        // 浮動小数点演算順序差のみに由来する。f16 経路の許容誤差（1e-3）より
        // 厳しい 1e-5 で一致を確認する。
        assert!(
            (ascore - escore).abs() < 1e-5,
            "score mismatch for id {aid}: gpu={ascore} cpu={escore}"
        );
    }
}

/// 奇数次元でも f32 常駐対照経路が正しく扱うこと（f32 経路はパディング不要
/// だが、`ResidentMatrix::build` 自体は奇数次元でも f16 パック側で 0 埋めする
/// ため、対照経路がその影響を受けずに `dim` そのものをストライドとして扱う
/// ことを確認する）。
#[test]
fn f32_contrast_backend_handles_odd_dimension_when_gpu_available() {
    let ids = [1u64, 2];
    let tenant_ids = ["t".to_string(), "t".to_string()];
    let visibilities = [Visibility::Public, Visibility::Public];
    let dim = 3;
    let vectors = [1.0f32, 0.0, 0.0, 0.0, 1.0, 0.0];

    let backend =
        match GpuF32ContrastBackend::try_new(&ids, &tenant_ids, &visibilities, dim, &vectors) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("gpu unavailable in this environment, skipping: {e}");
                return;
            }
        };

    let c = ctx("t");
    let query = [1.0f32, 1.0, 0.0];
    let bq = BatchQuery {
        vector: &query,
        k: 2,
        ctx: &c,
    };
    let hits = backend
        .batch_search(std::slice::from_ref(&bq))
        .expect("gpu batch_search should succeed once the device initialized");
    let mut scored: Vec<(u64, f32)> = hits[0].hits.iter().map(|h| (h.id, h.score)).collect();
    scored.sort_by_key(|(id, _)| *id);
    assert_eq!(scored.len(), 2);
    assert!((scored[0].1 - 1.0).abs() < 1e-5);
    assert!((scored[1].1 - 1.0).abs() < 1e-5);
}

// --- Issue #532: クエリタイル化（1 dispatch で複数クエリを処理）の結合テスト ---

/// テスト専用の決定的 xorshift32。`rand` 系クレートを追加せず
/// （依存最小方針）、GPU vs CPU オラクル比較に使う再現可能な浮動小数点値を
/// 生成する。
struct Xorshift32(u32);

impl Xorshift32 {
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// `[-1.0, 1.0)` の f32 を返す。
    fn next_f32(&mut self) -> f32 {
        let bits = self.next_u32();
        ((bits as f64 / u32::MAX as f64) * 2.0 - 1.0) as f32
    }

    /// `next_f32` が返す値を [`engine::batch_search::pack_f16x2`]/
    /// `unpack_f16x2` で 1 度だけ f16 へ丸めた値を返す（PR #591 レビュー P2
    /// 指摘対応）。丸め後の値は f16 の表現グリッド上に乗るため、以後同じ
    /// pack/unpack を何度施しても不変（冪等）であり、`gpu_batch.rs::
    /// f16_round_trip_exact` の判定を常に満たす。`f16_arith_dot_shader`
    /// モジュールの成功経路テスト（既定選択で実際に `F16Arith` へ dispatch
    /// されることを要求する）はクエリ成分に丸めを含めてはならないため、
    /// [`multi_query_fixture`] の乱数クエリ生成をこちらへ差し替えて使う。
    fn next_f32_f16_round_trippable(&mut self) -> f32 {
        let v = self.next_f32();
        let (rounded, _) =
            engine::batch_search::unpack_f16x2(engine::batch_search::pack_f16x2(v, 0.0));
        rounded
    }
}

/// 行・クエリともに `Xorshift32` で生成した固定次元のベクトル集合
/// （odd dim を含む。dim チャンク化は本 Issue の対象外だが、行数チャンク化
/// との組み合わせを兼ねて奇数次元でも検証する）。
struct MultiQueryFixture {
    ids: Vec<u64>,
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
    dim: usize,
    vectors: Vec<f32>,
    queries: Vec<Vec<f32>>,
}

fn multi_query_fixture(
    row_count: usize,
    dim: usize,
    query_count: usize,
    seed: u32,
) -> MultiQueryFixture {
    let mut rng = Xorshift32(seed | 1);
    let mut vectors = Vec::with_capacity(row_count * dim);
    for _ in 0..row_count * dim {
        vectors.push(rng.next_f32());
    }
    let mut queries = Vec::with_capacity(query_count);
    for _ in 0..query_count {
        let mut q = Vec::with_capacity(dim);
        for _ in 0..dim {
            q.push(rng.next_f32());
        }
        queries.push(q);
    }
    MultiQueryFixture {
        ids: (1..=row_count as u64).collect(),
        tenant_ids: vec!["tenant-a".to_string(); row_count],
        visibilities: vec![Visibility::Public; row_count],
        dim,
        vectors,
        queries,
    }
}

/// [`multi_query_fixture`] と同じだが、クエリ成分だけを
/// `Xorshift32::next_f32_f16_round_trippable` で生成し、f16 へ厳密往復
/// できる値に限定する（PR #591 レビュー P2 指摘対応）。行は対象外のまま
/// （`gpu_batch.rs::select_dot_shader` の `query_has_precision_loss` ガード
/// はクエリのみを母数にするため。`docs/design/gpu-batch-f16-arith.md` §3
/// 参照）。`f16_arith_dot_shader` モジュールの成功経路テスト（既定選択で
/// `SHADER_F16` 対応アダプタでは実際に `F16Arith` へ dispatch されることを
/// 要求する）は、丸め値を含みうる `multi_query_fixture` の乱数クエリでは
/// `has_precision_loss` ガードに拒否されて自動選択が常に `Unpack` へ縮退し
/// てしまうため、成功経路にはこちらを使う。丸みを伴うクエリでガードが
/// 実際に `Unpack` へ縮退することの確認は
/// `f16_arith_precision_loss_guard_falls_back_to_unpack` に分離する。
fn multi_query_fixture_f16_safe(
    row_count: usize,
    dim: usize,
    query_count: usize,
    seed: u32,
) -> MultiQueryFixture {
    let mut rng = Xorshift32(seed | 1);
    let mut vectors = Vec::with_capacity(row_count * dim);
    for _ in 0..row_count * dim {
        vectors.push(rng.next_f32());
    }
    let mut queries = Vec::with_capacity(query_count);
    for _ in 0..query_count {
        let mut q = Vec::with_capacity(dim);
        for _ in 0..dim {
            q.push(rng.next_f32_f16_round_trippable());
        }
        queries.push(q);
    }
    MultiQueryFixture {
        ids: (1..=row_count as u64).collect(),
        tenant_ids: vec!["tenant-a".to_string(); row_count],
        visibilities: vec![Visibility::Public; row_count],
        dim,
        vectors,
        queries,
    }
}

/// GPU_QUERY_TILE_MAX（本モジュール doc 記載の実装既定値: 16）の非倍数本の
/// クエリを 1 バッチで投げ、複数タイル・複数行チャンクにまたがっても各クエリの
/// Top-k が CPU オラクルと id 集合一致・スコア 1e-3 以内で一致することを確認する
/// （Issue #532・R1〜R4: 1 dispatch で複数クエリを処理する構造への変更が
/// 結果の正しさに影響しないことの回帰）。
#[test]
fn gpu_backend_multi_tile_matches_cpu_oracle_when_gpu_available() {
    // GPU_QUERY_TILE_MAX=16 の非倍数（タイル境界をまたぐ）本数。
    let fx = multi_query_fixture(300, 131, 21, 0x532_2026);
    let matrix = engine::batch_search::ResidentMatrix::build(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
    )
    .expect("resident matrix build should succeed for well-formed fixture");

    let backend = match GpuBatchBackend::try_new(matrix) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gpu unavailable in this environment, skipping: {e}");
            return;
        }
    };

    let c = ctx("tenant-a");
    let batch_queries: Vec<BatchQuery<'_>> = fx
        .queries
        .iter()
        .map(|v| BatchQuery {
            vector: v,
            k: 5,
            ctx: &c,
        })
        .collect();
    let hits = backend
        .batch_search(&batch_queries)
        .expect("gpu batch_search should succeed once the device initialized");
    assert_eq!(hits.len(), fx.queries.len());

    let simple_fx = Fixture {
        ids: fx.ids.clone(),
        tenant_ids: fx.tenant_ids.clone(),
        visibilities: fx.visibilities.clone(),
        dim: fx.dim,
        vectors: fx.vectors.clone(),
    };
    for (qi, query) in fx.queries.iter().enumerate() {
        let expected = cpu_oracle(&simple_fx, query, 5, &c);
        let mut actual: Vec<(u64, f32)> = hits[qi].hits.iter().map(|h| (h.id, h.score)).collect();
        let mut expected_sorted: Vec<(u64, f32)> =
            expected.iter().map(|h| (h.id, h.score)).collect();
        actual.sort_by_key(|(id, _)| *id);
        expected_sorted.sort_by_key(|(id, _)| *id);
        assert_eq!(
            actual.len(),
            expected_sorted.len(),
            "hit count mismatch for query {qi}"
        );
        for ((aid, ascore), (eid, escore)) in actual.iter().zip(expected_sorted.iter()) {
            assert_eq!(aid, eid, "id mismatch for query {qi}");
            // dim=131 の非正規化ランダムベクトルはスコアの絶対値が
            // 既存テスト（dim=2 の単位ベクトル）より大きく（数〜十数程度）、
            // f16 量子化の絶対誤差もスコアの大きさに比例して増える。他
            // テストの絶対許容誤差（1e-3）ではなく、スコアの大きさに応じた
            // 相対許容誤差を使う。
            let tolerance = 5e-3 * (1.0 + escore.abs());
            assert!(
                (ascore - escore).abs() < tolerance,
                "score mismatch for query {qi} id {aid}: gpu={ascore} cpu={escore}"
            );
        }
    }
}

/// 複数テナントのクエリを交互に並べた 1 バッチで、`group_queries_by_ctx`
/// によるグループ化後もタイル境界を越えてテナントが混入しないことを確認する
/// （Issue #532・R1。private の `group_queries_by_ctx` は直接呼べないため、
/// `GpuBatchBackend::batch_search` 経由の結果でテナント境界を検証する）。
#[test]
fn gpu_backend_mixed_tenant_batch_has_no_cross_tenant_leak_when_gpu_available() {
    let fx = fixture(true);
    let ctx_a = ctx_with_private("tenant-a");
    let ctx_b = ctx_with_private("tenant-b");
    let matrix = engine::batch_search::ResidentMatrix::build(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
    )
    .expect("resident matrix build should succeed for well-formed fixture");

    let backend = match GpuBatchBackend::try_new(matrix) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gpu unavailable in this environment, skipping: {e}");
            return;
        }
    };

    let query_a = [1.0f32, 1.0];
    let query_b = [2.0f32, 0.5];
    // tenant-a, tenant-b, tenant-a を交互に並べ、`group_queries_by_ctx` が
    // 入力順を保ったまま tenant-a を index [0, 2] へ、tenant-b を index [1]
    // へ束ねることを間接的に確認する（結果の入力順復元が壊れていれば
    // どこかの id セットが入れ替わって検出される）。
    let batch_queries = vec![
        BatchQuery {
            vector: &query_a,
            k: 4,
            ctx: &ctx_a,
        },
        BatchQuery {
            vector: &query_b,
            k: 4,
            ctx: &ctx_b,
        },
        BatchQuery {
            vector: &query_a,
            k: 4,
            ctx: &ctx_a,
        },
    ];
    let hits = backend
        .batch_search(&batch_queries)
        .expect("gpu batch_search should succeed once the device initialized");
    assert_eq!(hits.len(), 3);

    for (qi, expected_ids) in [(0usize, [1u64, 2]), (2, [1, 2])] {
        for hit in &hits[qi].hits {
            assert!(
                expected_ids.contains(&hit.id),
                "unexpected id {} leaked into tenant-a result at query {qi}",
                hit.id
            );
        }
    }
    for hit in &hits[1].hits {
        assert!(
            hit.id == 3 || hit.id == 4,
            "unexpected id {} leaked into tenant-b result",
            hit.id
        );
    }
    // index 0 と index 2 は同一クエリ・同一 ctx なので結果も一致するはず
    // （入力順復元・タイル分割の実装バグがあれば崩れる）。
    let mut hits0: Vec<(u64, f32)> = hits[0].hits.iter().map(|h| (h.id, h.score)).collect();
    let mut hits2: Vec<(u64, f32)> = hits[2].hits.iter().map(|h| (h.id, h.score)).collect();
    hits0.sort_by_key(|(id, _)| *id);
    hits2.sort_by_key(|(id, _)| *id);
    assert_eq!(hits0, hits2);
}

/// `plan_query_tile` の既定予算（`GPU_SCORE_BUFFER_BUDGET_BYTES` = 32MiB）は
/// 通常のデバイス上では 1 タイル分の行がすべて 1 チャンクに収まってしまい
/// （タイル幅 16・dim=131 なら約 49 万行まで無分割）、`gpu_backend_multi_tile_
/// matches_cpu_oracle_when_gpu_available`（300 行）が検証できているのは
/// クエリタイル分割だけで、行チャンク分割（複数チャンクにまたがるスコア
/// 配置・`TopKSelector` への累積）は通っていなかった（Issue #532 codex-review
/// P2 指摘対応）。`batch_search_with_row_budget_for_tests`（`bench-internals`
/// feature 限定。`gpu_batch.rs::GpuBatchBackend` 実装参照）で意図的に小さい
/// 予算を注入し、23 行を 4 行ずつのチャンク（6 チャンク目は端数の 3 行）へ
/// 分割させたうえで、CPU オラクルと id 集合一致・スコア相対誤差 1e-3 以内で
/// 一致することを確認する。この呼び出しは `force_full_readback: false`
/// 相当（`batch_search_with_row_budget_for_tests` は既定経路のまま予算だけ
/// を上書きする）だが、23 行程度では通常 Top-k パイプラインの `k_out` 予算
/// でも 1 チャンクに収まってしまうため、あえて全量 readback 経路の端数
/// チャンク分割を踏む唯一の手段として残す（`bench_search_with_row_budget_
/// for_tests` は Top-k 経路には影響しない `budget_bytes` のみを注入する
/// ため、既定で Top-k パイプラインが有効な環境では実際には Top-k 経路が
/// 選ばれる可能性があるが、その場合でも CPU オラクルとの一致という
/// アサーション自体は変わらず有効である）。
#[cfg(feature = "bench-internals")]
#[test]
fn gpu_backend_fractional_row_chunk_matches_cpu_oracle_when_gpu_available() {
    let row_count = 23;
    let dim = 17;
    let query_count = 5;
    let fx = multi_query_fixture(row_count, dim, query_count, 0x532_c8ff);
    let matrix = engine::batch_search::ResidentMatrix::build(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
    )
    .expect("resident matrix build should succeed for well-formed fixture");

    let backend = match GpuBatchBackend::try_new(matrix) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gpu unavailable in this environment, skipping: {e}");
            return;
        }
    };

    let c = ctx("tenant-a");
    let batch_queries: Vec<BatchQuery<'_>> = fx
        .queries
        .iter()
        .map(|v| BatchQuery {
            vector: v,
            k: 6,
            ctx: &c,
        })
        .collect();

    // Top-k パイプライン非対応（縮退時）の全量 readback 経路を強制し、
    // width（1 dispatch のクエリ本数）= min(query_count, GPU_QUERY_TILE_MAX)
    // = 5・per_row_bytes = width * 4（score）+ 4（row_id）= 24。
    // budget_bytes = 96 にすると chunk_rows = 96 / 24 = 4 となり、23 行が
    // [4,4,4,4,4,3] の 6 チャンク（端数を含む）に分割される（Issue #536 で
    // `batch_search_with_options_for_tests` を新設したため、既定経路
    // （Top-k）の影響を受けない `force_full_readback: true` で固定する）。
    let tiny_budget_bytes = 96;
    let hits = backend
        .batch_search_with_options_for_tests(
            &batch_queries,
            engine::gpu_batch::GpuSearchTestOptions {
                budget_bytes: tiny_budget_bytes,
                force_full_readback: true,
                dot_shader: None,
            },
        )
        .expect("gpu batch_search should succeed once the device initialized");
    assert_eq!(hits.len(), fx.queries.len());

    let simple_fx = Fixture {
        ids: fx.ids.clone(),
        tenant_ids: fx.tenant_ids.clone(),
        visibilities: fx.visibilities.clone(),
        dim: fx.dim,
        vectors: fx.vectors.clone(),
    };
    for (qi, query) in fx.queries.iter().enumerate() {
        let expected = cpu_oracle(&simple_fx, query, 6, &c);
        let mut actual: Vec<(u64, f32)> = hits[qi].hits.iter().map(|h| (h.id, h.score)).collect();
        let mut expected_sorted: Vec<(u64, f32)> =
            expected.iter().map(|h| (h.id, h.score)).collect();
        actual.sort_by_key(|(id, _)| *id);
        expected_sorted.sort_by_key(|(id, _)| *id);
        assert_eq!(
            actual.len(),
            expected_sorted.len(),
            "hit count mismatch for query {qi}"
        );
        for ((aid, ascore), (eid, escore)) in actual.iter().zip(expected_sorted.iter()) {
            assert_eq!(aid, eid, "id mismatch for query {qi}");
            let tolerance = 5e-3 * (1.0 + escore.abs());
            assert!(
                (ascore - escore).abs() < tolerance,
                "score mismatch for query {qi} id {aid}: gpu={ascore} cpu={escore}"
            );
        }
    }
}

/// `plan_query_tile` の全量 readback 予算式で端数チャンクを踏む代わりに、
/// workgroup 内部分 Top-k 経路（`plan_partial_topk_chunk_rows`）の予算式で
/// 端数チャンクを踏む兄弟テスト（Issue #536）。width=5・k_out=6・小さい
/// `budget_bytes` で複数チャンクへ分割させ、CPU オラクルと一致することを
/// 確認する。
#[cfg(feature = "bench-internals")]
#[test]
fn gpu_backend_partial_topk_fractional_row_chunk_matches_cpu_oracle_when_gpu_available() {
    let row_count = 40;
    let dim = 17;
    let query_count = 5;
    let fx = multi_query_fixture(row_count, dim, query_count, 0x536_a11c);
    let matrix = engine::batch_search::ResidentMatrix::build(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
    )
    .expect("resident matrix build should succeed for well-formed fixture");

    let backend = match GpuBatchBackend::try_new(matrix) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("gpu unavailable in this environment, skipping: {e}");
            return;
        }
    };
    let c = ctx("tenant-a");
    let batch_queries: Vec<BatchQuery<'_>> = fx
        .queries
        .iter()
        .map(|v| BatchQuery {
            vector: v,
            k: 6,
            ctx: &c,
        })
        .collect();

    // 小さい budget_bytes を注入し、部分 Top-k 経路が使われる場合は
    // 複数チャンクへ分割されることを狙う（Top-k パイプライン非対応環境
    // では自動的に全量 readback 経路へ縮退するが、その場合も本テストの
    // 「CPU オラクルと一致する」というアサーション自体は変わらず有効）。
    // codex 指摘対応（PR #578）: width=5・k_out=6 のとき
    // `plan_partial_topk_chunk_rows` の 1 ワークグループぶん出力バイト数は
    // 240 バイトであり、旧 budget_bytes=512 では chunk_rows が row_count
    // （40）を上回ってしまい全行が 1 チャンクに収まっていた（チャンク間
    // マージ・端数処理を検証できていなかった）。budget_bytes=300 では
    // chunk_rows=15 となり 40 行が 3 チャンクへ分割される。
    let tiny_budget_bytes = 300;
    let stats_before = backend.stats();
    let hits = backend
        .batch_search_with_options_for_tests(
            &batch_queries,
            engine::gpu_batch::GpuSearchTestOptions {
                budget_bytes: tiny_budget_bytes,
                force_full_readback: false,
                dot_shader: None,
            },
        )
        .expect("gpu batch_search should succeed once the device initialized");
    assert_eq!(hits.len(), fx.queries.len());

    // 部分 Top-k パイプラインが利用可能な環境では、複数 dispatch（複数
    // チャンク）に分割されたことを直接確認する（Top-k パイプライン非対応
    // 環境では全量 readback へ縮退するため、その場合はこのアサーションを
    // 免除する）。
    let stats_after = backend.stats();
    let partial_topk_dispatches =
        stats_after.partial_topk_dispatches - stats_before.partial_topk_dispatches;
    let full_readback_dispatches =
        stats_after.full_readback_dispatches - stats_before.full_readback_dispatches;
    if partial_topk_dispatches > 0 {
        assert!(
            partial_topk_dispatches > 1,
            "expected multiple partial topk dispatches (chunked), got {partial_topk_dispatches}"
        );
    } else {
        eprintln!(
            "partial topk pipeline unavailable in this environment              (full_readback_dispatches={full_readback_dispatches}), skipping chunk count assertion"
        );
    }

    let simple_fx = Fixture {
        ids: fx.ids.clone(),
        tenant_ids: fx.tenant_ids.clone(),
        visibilities: fx.visibilities.clone(),
        dim: fx.dim,
        vectors: fx.vectors.clone(),
    };
    for (qi, query) in fx.queries.iter().enumerate() {
        let expected = cpu_oracle(&simple_fx, query, 6, &c);
        let mut actual: Vec<(u64, f32)> = hits[qi].hits.iter().map(|h| (h.id, h.score)).collect();
        let mut expected_sorted: Vec<(u64, f32)> =
            expected.iter().map(|h| (h.id, h.score)).collect();
        actual.sort_by_key(|(id, _)| *id);
        expected_sorted.sort_by_key(|(id, _)| *id);
        assert_eq!(
            actual.len(),
            expected_sorted.len(),
            "hit count mismatch for query {qi}"
        );
        for ((aid, ascore), (eid, escore)) in actual.iter().zip(expected_sorted.iter()) {
            assert_eq!(aid, eid, "id mismatch for query {qi}");
            let tolerance = 5e-3 * (1.0 + escore.abs());
            assert!(
                (ascore - escore).abs() < tolerance,
                "score mismatch for query {qi} id {aid}: gpu={ascore} cpu={escore}"
            );
        }
    }
}

// --- Issue #536: workgroup 内部分 Top-k 経路の実機ビット同一・統計検証 ---
//
// `GpuBatchBackend::batch_search`（既定 = 部分 Top-k 経路が利用可能なら
// それを使う）と `batch_search_with_options_for_tests(force_full_readback:
// true)`（常に全量 readback 経路。旧来の唯一の経路）を同一入力で実行し、
// 結果が `(id, score.to_bits())` 列としてビット同一であることを確認する。
// `bench-internals` feature 限定（`GpuSearchTestOptions` 経由）。

#[cfg(feature = "bench-internals")]
mod topk_readback_bit_identity {
    use super::*;
    use engine::gpu_batch::GpuSearchTestOptions;

    /// (id, score bits) の昇順ソート列。`into_sorted_vec` の順序契約
    /// （score 降順・id 昇順のタイブレーク）はすでに GPU 側で保たれている
    /// ため、比較のため id 昇順へ正規化してから突き合わせる。
    fn id_score_bits(hits: &[engine::kernel::SearchHit]) -> Vec<(u64, u32)> {
        let mut v: Vec<(u64, u32)> = hits.iter().map(|h| (h.id, h.score.to_bits())).collect();
        v.sort_by_key(|(id, _)| *id);
        v
    }

    /// 重複ヘビー（多数の行が同一スコアになる）コーパス。同点タイブレーク
    /// （常駐スロット昇順）が全量 readback 経路と部分 Top-k 経路とで一致する
    /// ことを検証する土台にする。
    fn duplicate_heavy_fixture(row_count: usize, dim: usize) -> Fixture {
        let mut vectors = Vec::with_capacity(row_count * dim);
        for i in 0..row_count {
            // 半数の行を完全に同一ベクトルにして大量の同点を誘発する。
            let base = if i % 2 == 0 { 1.0 } else { 2.0 };
            for _ in 0..dim {
                vectors.push(base);
            }
        }
        Fixture {
            ids: (1..=row_count as u64).collect(),
            tenant_ids: vec!["tenant-a".to_string(); row_count],
            visibilities: vec![Visibility::Public; row_count],
            dim,
            vectors,
        }
    }

    #[test]
    fn default_partial_topk_matches_forced_full_readback_bit_identically() {
        // row_count は 256（ワークグループサイズ）の非倍数にして端数
        // ワークグループを踏む。dim は奇数にしてパディング経路も踏む。
        let row_count = 1000;
        let dim = 33;
        let fx = duplicate_heavy_fixture(row_count, dim);
        let matrix = engine::batch_search::ResidentMatrix::build(
            &fx.ids,
            &fx.tenant_ids,
            &fx.visibilities,
            fx.dim,
            &fx.vectors,
        )
        .expect("resident matrix build should succeed for well-formed fixture");

        let backend = match GpuBatchBackend::try_new(matrix) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("gpu unavailable in this environment, skipping: {e}");
                return;
            }
        };

        let c = ctx("tenant-a");
        let query = vec![1.0f32; dim];
        for &k in &[1usize, 10, 256] {
            let bq = [BatchQuery {
                vector: &query,
                k,
                ctx: &c,
            }];

            let default_hits = backend
                .batch_search(&bq)
                .expect("default gpu batch_search should succeed once the device initialized");
            let forced_full_hits = backend
                .batch_search_with_options_for_tests(
                    &bq,
                    GpuSearchTestOptions {
                        budget_bytes: 32 * 1024 * 1024,
                        force_full_readback: true,
                        dot_shader: None,
                    },
                )
                .expect("forced full-readback gpu batch_search should succeed");

            assert_eq!(default_hits.len(), 1);
            assert_eq!(forced_full_hits.len(), 1);
            assert_eq!(
                id_score_bits(&default_hits[0].hits),
                id_score_bits(&forced_full_hits[0].hits),
                "k={k}: partial topk and forced full-readback must be bit-identical"
            );
        }
    }

    #[test]
    fn non_finite_rows_are_excluded_and_default_matches_forced_full_readback() {
        // f16 飽和で ±Inf になる行（65504 超の成分）を混ぜ、`inf * 0.0` が
        // NaN を生む列を query 側に用意する。両経路とも非有限スコアの行が
        // 結果へ混入しないこと、かつビット同一であることを確認する。
        let dim = 4;
        let ids = vec![1u64, 2, 3, 4];
        let tenant_ids = vec!["tenant-a".to_string(); 4];
        let visibilities = vec![Visibility::Public; 4];
        #[rustfmt::skip]
        let vectors = vec![
            1.0, 0.0, 0.0, 0.0,
            0.0, 1.0, 0.0, 0.0,
            // 65504 超の成分は f16 パックで +Inf に飽和する。
            100000.0, 0.0, 0.0, 0.0,
            0.0, 0.0, 1.0, 0.0,
        ];
        let matrix = engine::batch_search::ResidentMatrix::build(
            &ids,
            &tenant_ids,
            &visibilities,
            dim,
            &vectors,
        )
        .expect("resident matrix build should succeed for well-formed fixture");

        let backend = match GpuBatchBackend::try_new(matrix) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("gpu unavailable in this environment, skipping: {e}");
                return;
            }
        };

        let c = ctx("tenant-a");
        // クエリの第 1 成分を 0 にすることで、id=3 の行（+Inf）に対する
        // 内積は `Inf * 0.0` の項を含み NaN になる（非有限スコア）。
        let query = [0.0f32, 1.0, 1.0, 0.0];
        let bq = [BatchQuery {
            vector: &query,
            k: 4,
            ctx: &c,
        }];

        let default_hits = backend
            .batch_search(&bq)
            .expect("default gpu batch_search should succeed once the device initialized");
        let forced_full_hits = backend
            .batch_search_with_options_for_tests(
                &bq,
                GpuSearchTestOptions {
                    budget_bytes: 32 * 1024 * 1024,
                    force_full_readback: true,
                    dot_shader: None,
                },
            )
            .expect("forced full-readback gpu batch_search should succeed");

        for hits in [&default_hits[0].hits, &forced_full_hits[0].hits] {
            assert!(
                hits.iter().all(|h| h.id != 3),
                "non-finite scoring row (id=3) must not appear in results"
            );
        }
        assert_eq!(
            id_score_bits(&default_hits[0].hits),
            id_score_bits(&forced_full_hits[0].hits),
        );
    }

    #[test]
    fn default_path_reports_nonvacuous_partial_topk_stats() {
        // 既定経路（部分 Top-k）が実際に選ばれ、readback バイト数が
        // 全量 readback 経路より小さいことを統計カウンタで確認する
        // （ADR 受け入れ条件・#537 が読む `stats()` の非 vacuous 性）。
        let row_count = 2000;
        let dim = 33;
        let fx = duplicate_heavy_fixture(row_count, dim);
        let matrix = engine::batch_search::ResidentMatrix::build(
            &fx.ids,
            &fx.tenant_ids,
            &fx.visibilities,
            fx.dim,
            &fx.vectors,
        )
        .expect("resident matrix build should succeed for well-formed fixture");

        let backend = match GpuBatchBackend::try_new(matrix) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("gpu unavailable in this environment, skipping: {e}");
                return;
            }
        };

        let c = ctx("tenant-a");
        let query = vec![1.0f32; dim];
        let bq = [BatchQuery {
            vector: &query,
            k: 10,
            ctx: &c,
        }];

        let _ = backend
            .batch_search(&bq)
            .expect("default gpu batch_search should succeed once the device initialized");
        let stats_after_default = backend.stats();

        let _ = backend
            .batch_search_with_options_for_tests(
                &bq,
                GpuSearchTestOptions {
                    budget_bytes: 32 * 1024 * 1024,
                    force_full_readback: true,
                    dot_shader: None,
                },
            )
            .expect("forced full-readback gpu batch_search should succeed");
        let stats_after_forced = backend.stats();

        if stats_after_default.partial_topk_dispatches == 0 {
            eprintln!(
                "topk pipeline unavailable in this environment (fail-closed to full readback); skipping stats assertions"
            );
            return;
        }

        assert!(
            stats_after_default.partial_topk_dispatches > 0,
            "default path must use the partial topk dispatch at least once"
        );
        assert_eq!(
            stats_after_default.full_readback_dispatches, 0,
            "default path must not fall back to full readback for this fixture"
        );
        let full_readback_bytes_delta =
            stats_after_forced.readback_bytes - stats_after_default.readback_bytes;
        assert!(
            full_readback_bytes_delta > stats_after_default.readback_bytes,
            "forced full-readback dispatch must read back strictly more bytes than the default partial topk path"
        );
    }

    #[test]
    fn f32_contrast_backend_matches_between_default_and_forced_full_readback() {
        // CORE-16 公平性のため f32 常駐対照経路でも 1 本以上ビット同一検証
        // する（ADR §2.1「決定事項」）。
        let row_count = 300;
        let dim = 17;
        let fx = duplicate_heavy_fixture(row_count, dim);

        let backend = match GpuF32ContrastBackend::try_new(
            &fx.ids,
            &fx.tenant_ids,
            &fx.visibilities,
            fx.dim,
            &fx.vectors,
        ) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("gpu unavailable in this environment, skipping: {e}");
                return;
            }
        };

        let c = ctx("tenant-a");
        let query = vec![1.0f32; dim];
        let bq = [BatchQuery {
            vector: &query,
            k: 10,
            ctx: &c,
        }];
        let default_hits = backend
            .batch_search(&bq)
            .expect("default f32 contrast batch_search should succeed once available");
        assert_eq!(default_hits.len(), 1);
        assert!(!default_hits[0].hits.is_empty());

        // codex 指摘対応（PR #578）: 既定経路を 1 回呼んで空でないことを
        // 確認するだけでは、f32 専用の内積処理を含む新パイプラインの選出・
        // スコアが誤っていても検出できない。強制全量 readback 経路
        // （`GpuF32ContrastBackend::batch_search_with_options_for_tests`）を
        // 追加で呼び、両経路の (id, score.to_bits()) が一致することを確認する
        // （`GpuBatchBackend` 側の `topk_readback_bit_identity` と同じ方針）。
        let forced_hits = backend
            .batch_search_with_options_for_tests(
                &bq,
                GpuSearchTestOptions {
                    budget_bytes: 32 * 1024 * 1024,
                    force_full_readback: true,
                    dot_shader: None,
                },
            )
            .expect("forced full-readback f32 contrast batch_search should succeed");
        assert_eq!(forced_hits.len(), 1);

        let id_score_bits = |hits: &[engine::kernel::SearchHit]| -> Vec<(u64, u32)> {
            let mut v: Vec<(u64, u32)> = hits.iter().map(|h| (h.id, h.score.to_bits())).collect();
            v.sort_by_key(|(id, _)| *id);
            v
        };
        assert_eq!(
            id_score_bits(&default_hits[0].hits),
            id_score_bits(&forced_hits[0].hits),
            "f32 contrast default path and forced full-readback path must match bit-identically"
        );
    }
}

// --- Issue #539: SHADER_F16 対応アダプタでの f16 算術版シェーダ選択の実機検証 ---
//
// `GpuBatchBackend` の既定経路（`SHADER_F16` 対応アダプタでは
// `select_dot_shader` が自動的に f16 算術版を選ぶ）と、`GpuSearchTestOptions::
// dot_shader` による強制オーバーライドを組み合わせ、次を実機で確認する:
// - 既定経路 vs 強制 unpack 版の結果が境界同点許容つき Recall で一致し
//   （`harness::gpu_scaling::count_boundary_tolerant_mismatches`）、
//   `f16_arith_available()` が true の環境では実際に f16 算術版へ dispatch
//   されたこと（非 vacuous）
// - オーバーフローガード（`select_dot_shader`）が実際に働き、大振幅
//   フィクスチャでは自動選択が unpack 版へ縮退すること
// - 強制 `F16Arith` が利用不能・ガード不成立の場合は黙って縮退せず `Err`
//   を返すこと（fail-closed）
//
// `bench-internals` feature 限定（`GpuSearchTestOptions::dot_shader` 経由）。
#[cfg(feature = "bench-internals")]
#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod gpu_f16_arith_harness;

#[cfg(feature = "bench-internals")]
mod f16_arith_dot_shader {
    use super::*;
    use engine::gpu_batch::{GpuDotShaderKind, GpuSearchTestOptions};
    use gpu_f16_arith_harness::gpu_scaling::count_boundary_tolerant_mismatches;

    fn id_score_pairs(hits: &[engine::kernel::SearchHit]) -> Vec<(u64, f32)> {
        let mut v: Vec<(u64, f32)> = hits.iter().map(|h| (h.id, h.score)).collect();
        v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }

    /// 既定経路（`SHADER_F16` 対応時は自動的に f16 算術版）と強制 unpack 版が
    /// 境界同点許容つきで一致し、対応アダプタでは実際に f16 算術版へ
    /// dispatch されたこと（`f16_arith_dispatches > 0`。非 vacuous）を確認する。
    /// `dim` はパディング境界（偶数丸め）を踏む奇数・偶数の双方、`query_count`
    /// は `GPU_QUERY_TILE_MAX`（16）の非倍数にしてタイル境界も踏む。
    #[test]
    fn f16_arith_default_matches_unpack_within_boundary_tolerance() {
        for &dim in &[33usize, 128] {
            let fx = multi_query_fixture_f16_safe(3000, dim, 21, 0x539_2026);
            let matrix = engine::batch_search::ResidentMatrix::build(
                &fx.ids,
                &fx.tenant_ids,
                &fx.visibilities,
                fx.dim,
                &fx.vectors,
            )
            .expect("resident matrix build should succeed for well-formed fixture");

            let backend = match GpuBatchBackend::try_new(matrix) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!("gpu unavailable in this environment, skipping: {e}");
                    return;
                }
            };

            let c = ctx("tenant-a");
            let batch_queries: Vec<BatchQuery<'_>> = fx
                .queries
                .iter()
                .map(|v| BatchQuery {
                    vector: v,
                    k: 5,
                    ctx: &c,
                })
                .collect();

            let stats_before = backend.stats();
            let default_hits = backend
                .batch_search(&batch_queries)
                .expect("default gpu batch_search should succeed once the device initialized");
            let stats_after = backend.stats();

            let forced_unpack_hits = backend
                .batch_search_with_options_for_tests(
                    &batch_queries,
                    GpuSearchTestOptions {
                        budget_bytes: 32 * 1024 * 1024,
                        force_full_readback: false,
                        dot_shader: Some(GpuDotShaderKind::Unpack),
                    },
                )
                .expect("forced unpack gpu batch_search should succeed");

            assert_eq!(default_hits.len(), fx.queries.len());
            assert_eq!(forced_unpack_hits.len(), fx.queries.len());

            if !backend.f16_arith_available() {
                // 未対応環境（本開発環境の RTX 3060 では通常到達しない分岐）
                // では既定経路も unpack 版のはずであり、強制 unpack 版と
                // ビット同一になる。f16 算術版は 1 回も dispatch されない。
                assert_eq!(
                    stats_after.f16_arith_dispatches - stats_before.f16_arith_dispatches,
                    0,
                    "dim={dim}: f16 arith must not dispatch on an unsupported adapter"
                );
                for (default, forced) in default_hits.iter().zip(forced_unpack_hits.iter()) {
                    assert_eq!(
                        id_score_pairs(&default.hits)
                            .into_iter()
                            .map(|(id, s)| (id, s.to_bits()))
                            .collect::<Vec<_>>(),
                        id_score_pairs(&forced.hits)
                            .into_iter()
                            .map(|(id, s)| (id, s.to_bits()))
                            .collect::<Vec<_>>(),
                        "dim={dim}: default and forced-unpack must be bit-identical without SHADER_F16"
                    );
                }
                continue;
            }

            // 対応アダプタ: 実際に f16 算術版へ dispatch されたこと（非
            // vacuous）と、境界同点許容つきで unpack 版と結果が一致することを
            // 確認する（数値誤差の許容は `count_boundary_tolerant_mismatches`
            // が担い、境界より明確に上位の正解の脱落は 0 件を要求する）。
            assert!(
                stats_after.f16_arith_dispatches > stats_before.f16_arith_dispatches,
                "dim={dim}: f16 arith dot shader must actually be dispatched on a SHADER_F16 adapter"
            );
            for (qidx, (default, forced)) in default_hits
                .iter()
                .zip(forced_unpack_hits.iter())
                .enumerate()
            {
                let mismatches = count_boundary_tolerant_mismatches(
                    &id_score_pairs(&forced.hits),
                    &id_score_pairs(&default.hits),
                );
                assert_eq!(
                    mismatches, 0,
                    "dim={dim} query={qidx}: f16 arith result must match unpack result within boundary tolerance"
                );
            }
        }
    }

    /// [`GpuSearchTestOptions::dot_shader`] に `Some(F16Arith)`/`Some(Unpack)`
    /// を強制した経路がそれぞれ `force_full_readback` の有無に関わらず
    /// ビット同一であることを確認する（`topk_readback_bit_identity` と同じ
    /// 方針を f16 算術版の S0 にも適用。全量 readback／部分 Top-k いずれの
    /// 経路でも [`DOT_SHADER_F16_ARITH_WGSL`]/[`DOT_SHADER_TOPK_F16_ARITH_WGSL`]
    /// の S0 演算順が完全に一致する契約の根拠）。
    #[test]
    fn f16_arith_partial_topk_matches_forced_full_readback_bit_identically() {
        let row_count = 1000;
        let dim = 33;
        let fx = multi_query_fixture_f16_safe(row_count, dim, 5, 0x539_a11c);
        let matrix = engine::batch_search::ResidentMatrix::build(
            &fx.ids,
            &fx.tenant_ids,
            &fx.visibilities,
            fx.dim,
            &fx.vectors,
        )
        .expect("resident matrix build should succeed for well-formed fixture");

        let backend = match GpuBatchBackend::try_new(matrix) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("gpu unavailable in this environment, skipping: {e}");
                return;
            }
        };
        if !backend.f16_arith_available() {
            eprintln!("SHADER_F16 unavailable in this environment, skipping");
            return;
        }

        let c = ctx("tenant-a");
        let batch_queries: Vec<BatchQuery<'_>> = fx
            .queries
            .iter()
            .map(|v| BatchQuery {
                vector: v,
                k: 6,
                ctx: &c,
            })
            .collect();

        let partial_hits = backend
            .batch_search_with_options_for_tests(
                &batch_queries,
                GpuSearchTestOptions {
                    budget_bytes: 32 * 1024 * 1024,
                    force_full_readback: false,
                    dot_shader: Some(GpuDotShaderKind::F16Arith),
                },
            )
            .expect("forced f16 arith (partial topk) gpu batch_search should succeed");
        let full_hits = backend
            .batch_search_with_options_for_tests(
                &batch_queries,
                GpuSearchTestOptions {
                    budget_bytes: 32 * 1024 * 1024,
                    force_full_readback: true,
                    dot_shader: Some(GpuDotShaderKind::F16Arith),
                },
            )
            .expect("forced f16 arith (full readback) gpu batch_search should succeed");

        assert_eq!(partial_hits.len(), fx.queries.len());
        assert_eq!(full_hits.len(), fx.queries.len());
        for (partial, full) in partial_hits.iter().zip(full_hits.iter()) {
            let bits = |hits: &[engine::kernel::SearchHit]| -> Vec<(u64, u32)> {
                let mut v: Vec<(u64, u32)> =
                    hits.iter().map(|h| (h.id, h.score.to_bits())).collect();
                v.sort_by_key(|(id, _)| *id);
                v
            };
            assert_eq!(
                bits(&partial.hits),
                bits(&full.hits),
                "f16 arith partial topk and forced full-readback must be bit-identical"
            );
        }
    }

    /// 大振幅フィクスチャ（ブロック内部分和がオーバーフロー上限を超える）
    /// では自動選択（`select_dot_shader`）が unpack 版へ縮退し、
    /// `f16_arith_guard_fallbacks` が増加することを確認する。あわせて
    /// `Some(F16Arith)` の強制指定はガード不成立のため `Err` を返すこと
    /// （fail-closed。黙って縮退しない）も確認する。
    #[test]
    fn f16_arith_overflow_guard_falls_back_to_unpack() {
        let row_count = 40;
        let dim = 128;
        // 全成分 200.0 の行 × 全成分 200.0 のクエリ:
        // row_max_abs * query_max_abs * GPU_F16_ACC_BLOCK(1・PR #591 レビュー
        // P1 指摘対応で 8 から変更) = 200*200*1 = 40000
        // > F16_ARITH_PARTIAL_SUM_LIMIT(32768) のためガードが unpack へ倒す。
        let fx = Fixture {
            ids: (1..=row_count as u64).collect(),
            tenant_ids: vec!["tenant-a".to_string(); row_count],
            visibilities: vec![Visibility::Public; row_count],
            dim,
            vectors: vec![200.0f32; row_count * dim],
        };
        let matrix = engine::batch_search::ResidentMatrix::build(
            &fx.ids,
            &fx.tenant_ids,
            &fx.visibilities,
            fx.dim,
            &fx.vectors,
        )
        .expect("resident matrix build should succeed for well-formed fixture");

        let backend = match GpuBatchBackend::try_new(matrix) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("gpu unavailable in this environment, skipping: {e}");
                return;
            }
        };
        if !backend.f16_arith_available() {
            eprintln!("SHADER_F16 unavailable in this environment, skipping");
            return;
        }

        let c = ctx("tenant-a");
        let query = vec![200.0f32; dim];
        let bq = [BatchQuery {
            vector: &query,
            k: 4,
            ctx: &c,
        }];

        let stats_before = backend.stats();
        let default_hits = backend
            .batch_search(&bq)
            .expect("default gpu batch_search should succeed once the device initialized");
        let stats_after = backend.stats();
        assert_eq!(
            stats_after.f16_arith_dispatches - stats_before.f16_arith_dispatches,
            0,
            "overflow guard must prevent f16 arith dispatch on this large-magnitude fixture"
        );
        assert!(
            stats_after.f16_arith_guard_fallbacks > stats_before.f16_arith_guard_fallbacks,
            "overflow guard fallback counter must increase when the bound is exceeded"
        );

        let forced_unpack_hits = backend
            .batch_search_with_options_for_tests(
                &bq,
                GpuSearchTestOptions {
                    budget_bytes: 32 * 1024 * 1024,
                    force_full_readback: false,
                    dot_shader: Some(GpuDotShaderKind::Unpack),
                },
            )
            .expect("forced unpack gpu batch_search should succeed");
        let bits = |hits: &[engine::kernel::SearchHit]| -> Vec<(u64, u32)> {
            let mut v: Vec<(u64, u32)> = hits.iter().map(|h| (h.id, h.score.to_bits())).collect();
            v.sort_by_key(|(id, _)| *id);
            v
        };
        assert_eq!(
            bits(&default_hits[0].hits),
            bits(&forced_unpack_hits[0].hits),
            "guard-triggered default path must match forced-unpack path bit-identically"
        );

        let forced_f16_err = backend.batch_search_with_options_for_tests(
            &bq,
            GpuSearchTestOptions {
                budget_bytes: 32 * 1024 * 1024,
                force_full_readback: false,
                dot_shader: Some(GpuDotShaderKind::F16Arith),
            },
        );
        assert!(
            forced_f16_err.is_err(),
            "forcing f16 arith when the overflow guard rejects it must fail closed, not silently fall back"
        );
    }

    /// クエリ成分が f16 へ厳密往復できない場合（振幅・アンダーフローの
    /// 既存ガードの範囲内でも仮数部 10 bit で丸められるケース。
    /// `docs/design/gpu-batch-f16-arith.md`「クエリ成分自体の f16 パック時の
    /// 丸め」節の反例と同型）に、自動選択（`select_dot_shader`）が
    /// `has_precision_loss` ガードにより unpack 版へ縮退することを、
    /// オーバーフローガード（[`f16_arith_overflow_guard_falls_back_to_unpack`]）
    /// とは独立に確認する（PR #591 レビュー P2 指摘対応: `multi_query_fixture`
    /// の乱数クエリは丸めを含みうるため成功経路テストからは分離し、丸めを
    /// 意図的に含む最小フィクスチャで縮退確認専用に使う）。強制
    /// `F16Arith` はガード不成立のため `Err` を返すこと（fail-closed）も
    /// あわせて確認する。
    #[test]
    fn f16_arith_precision_loss_guard_falls_back_to_unpack() {
        let row_count = 40;
        let dim = 8;
        // 行は通常のランダムフィクスチャ（振幅は小さくオーバーフロー
        // ガードには抵触しない）を使い、クエリだけを手動で丸め誘発値へ
        // 差し替える。
        let fx = multi_query_fixture(row_count, dim, 1, 0x539_9a51);
        let matrix = engine::batch_search::ResidentMatrix::build(
            &fx.ids,
            &fx.tenant_ids,
            &fx.visibilities,
            fx.dim,
            &fx.vectors,
        )
        .expect("resident matrix build should succeed for well-formed fixture");

        let backend = match GpuBatchBackend::try_new(matrix) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("gpu unavailable in this environment, skipping: {e}");
                return;
            }
        };
        if !backend.f16_arith_available() {
            eprintln!("SHADER_F16 unavailable in this environment, skipping");
            return;
        }

        let c = ctx("tenant-a");
        // 2048.5 は f16 の分解能（この振幅で 2）で 2048 へ丸められ、
        // `f16_round_trip_exact` が偽になる（doc「クエリ成分自体の f16
        // パック時の丸め」節の反例）。振幅は F16_MAX_FINITE・オーバー
        // フロー上界のいずれも超えないため、他のガードは働かず
        // `has_precision_loss` 単独の効果を確認できる。
        let mut query = vec![0.0f32; dim];
        query[0] = 2048.5;
        let bq = [BatchQuery {
            vector: &query,
            k: 4,
            ctx: &c,
        }];

        let stats_before = backend.stats();
        let default_hits = backend
            .batch_search(&bq)
            .expect("default gpu batch_search should succeed once the device initialized");
        let stats_after = backend.stats();
        assert_eq!(
            stats_after.f16_arith_dispatches - stats_before.f16_arith_dispatches,
            0,
            "precision-loss guard must prevent f16 arith dispatch for a non-round-trippable query"
        );
        assert!(
            stats_after.f16_arith_guard_fallbacks > stats_before.f16_arith_guard_fallbacks,
            "precision-loss guard fallback counter must increase when a query component cannot round-trip through f16"
        );

        let forced_unpack_hits = backend
            .batch_search_with_options_for_tests(
                &bq,
                GpuSearchTestOptions {
                    budget_bytes: 32 * 1024 * 1024,
                    force_full_readback: false,
                    dot_shader: Some(GpuDotShaderKind::Unpack),
                },
            )
            .expect("forced unpack gpu batch_search should succeed");
        let bits = |hits: &[engine::kernel::SearchHit]| -> Vec<(u64, u32)> {
            let mut v: Vec<(u64, u32)> = hits.iter().map(|h| (h.id, h.score.to_bits())).collect();
            v.sort_by_key(|(id, _)| *id);
            v
        };
        assert_eq!(
            bits(&default_hits[0].hits),
            bits(&forced_unpack_hits[0].hits),
            "guard-triggered default path must match forced-unpack path bit-identically"
        );

        let forced_f16_err = backend.batch_search_with_options_for_tests(
            &bq,
            GpuSearchTestOptions {
                budget_bytes: 32 * 1024 * 1024,
                force_full_readback: false,
                dot_shader: Some(GpuDotShaderKind::F16Arith),
            },
        );
        assert!(
            forced_f16_err.is_err(),
            "forcing f16 arith when the precision-loss guard rejects it must fail closed, not silently fall back"
        );
    }
}
// ---------------------------------------------------------------------
// Issue #2003: metric（L2・コサイン）対応（親 #2002。ポインタ: CORE-6・CORE-16・SQL-33）
//
// 受け入れ条件: GPU アダプタの無い環境でも skip 扱いにせず、フォールバック経路（CPU）で
// L2・コサインの結果が検証される。`build_with_gpu_metric` 系テストは GPU 初期化の成否で
// 分岐しつつ、どちらの分岐でも必ず assert する（早期 return で空振りさせない）。
// ---------------------------------------------------------------------

use engine::batch_search::{BatchMetric, BatchSearchError, ResidentMatrix};

/// metric ごとのオラクル。可視行の `(id, score)` を「大きいほど近い」で降順（同点は id 昇順）に
/// 返す。L2 は `-Σ(r-q)²`、コサインは cos 類似度（f64 で計算）。
fn metric_oracle(
    fx: &Fixture,
    query: &[f32],
    k: usize,
    ctx: &PolicyContext,
    metric: BatchMetric,
) -> Vec<(u64, f64)> {
    let mut scored: Vec<(u64, f64)> = Vec::new();
    for (((id, tenant), vis), row) in fx
        .ids
        .iter()
        .zip(fx.tenant_ids.iter())
        .zip(fx.visibilities.iter())
        .zip(fx.vectors.chunks(fx.dim))
    {
        if !ctx.is_visible(tenant, *vis) {
            continue;
        }
        let dot: f64 = row
            .iter()
            .zip(query)
            .map(|(&r, &q)| f64::from(r) * f64::from(q))
            .sum();
        let score = match metric {
            BatchMetric::L2 => -row
                .iter()
                .zip(query)
                .map(|(&r, &q)| (f64::from(r) - f64::from(q)).powi(2))
                .sum::<f64>(),
            BatchMetric::Cosine => {
                let nr: f64 = row
                    .iter()
                    .map(|&r| f64::from(r).powi(2))
                    .sum::<f64>()
                    .sqrt();
                let nq: f64 = query
                    .iter()
                    .map(|&q| f64::from(q).powi(2))
                    .sum::<f64>()
                    .sqrt();
                dot / (nr * nq)
            }
            BatchMetric::InnerProduct => dot,
        };
        scored.push((*id, score));
    }
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    scored.truncate(k);
    scored
}

/// 結果がオラクルと一致すること（id 集合一致・スコア許容差内。f16 常駐と f32 原本の丸め差を許容）。
fn assert_matches_metric_oracle(
    hits: &[engine::kernel::SearchHit],
    expected: &[(u64, f64)],
    label: &str,
) {
    assert_eq!(hits.len(), expected.len(), "{label}: hit count");
    let mut actual: Vec<(u64, f64)> = hits.iter().map(|h| (h.id, f64::from(h.score))).collect();
    let mut want: Vec<(u64, f64)> = expected.to_vec();
    actual.sort_by_key(|(id, _)| *id);
    want.sort_by_key(|(id, _)| *id);
    for ((aid, ascore), (eid, escore)) in actual.iter().zip(want.iter()) {
        assert_eq!(aid, eid, "{label}: id set mismatch");
        let tolerance = 5e-3 * (1.0 + escore.abs());
        assert!(
            (ascore - escore).abs() < tolerance,
            "{label}: score mismatch for id {aid}: got={ascore} want={escore}"
        );
    }
}

fn metric_fixture(
    row_count: usize,
    dim: usize,
    seed: u32,
    private_two_tenants: bool,
) -> (Fixture, Vec<Vec<f32>>) {
    let m = multi_query_fixture(row_count, dim, 3, seed);
    let mut tenant_ids = m.tenant_ids.clone();
    let mut visibilities = m.visibilities.clone();
    if private_two_tenants {
        for (i, (t, v)) in tenant_ids
            .iter_mut()
            .zip(visibilities.iter_mut())
            .enumerate()
        {
            if i % 2 == 1 {
                *t = "tenant-b".to_string();
            }
            *v = Visibility::Private;
        }
    }
    (
        Fixture {
            ids: m.ids,
            tenant_ids,
            visibilities,
            dim: m.dim,
            vectors: m.vectors,
        },
        m.queries,
    )
}

fn check_build_with_gpu_metric(metric: BatchMetric, seed: u32) {
    let (fx, queries) = metric_fixture(23, 17, seed, false);
    let observer = RecordingObserver::default();
    let engine = FallbackBatchEngine::build_with_gpu_metric(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
        metric,
        Box::new(observer.clone()),
    )
    .expect("build_with_gpu_metric should not fail on a well-formed fixture");

    let events = observer.events();
    if events.is_empty() {
        eprintln!("gpu available: {metric:?} verified against the oracle on the gpu path");
    } else {
        // GPU 初期化に失敗した環境: 縮退イベントはちょうど 1 件（init → cpu-simd）。
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].reason.to_string(), "init");
        assert_eq!(events[0].target, "cpu-simd");
    }

    // どちらの分岐でも結果はオラクルと一致する（CPU 縮退 or GPU）。
    let c = ctx("tenant-a");
    for (qi, query) in queries.iter().enumerate() {
        let bq = BatchQuery {
            vector: query,
            k: 6,
            ctx: &c,
        };
        let hits = engine
            .batch_search(std::slice::from_ref(&bq))
            .expect("batch_search should succeed");
        let expected = metric_oracle(&fx, query, 6, &c, metric);
        assert_matches_metric_oracle(&hits[0].hits, &expected, &format!("{metric:?} query {qi}"));
    }
}

/// CORE-6・CORE-16: L2 の GPU 構築。GPU の有無に依らず結果がオラクルと一致する。
#[test]
fn build_with_gpu_metric_falls_back_or_matches_oracle_l2() {
    check_build_with_gpu_metric(BatchMetric::L2, 0x2003_0012);
}

/// CORE-6・CORE-16・SQL-33: コサインの GPU 構築。GPU の有無に依らず結果がオラクルと一致する。
#[test]
fn build_with_gpu_metric_falls_back_or_matches_oracle_cosine() {
    check_build_with_gpu_metric(BatchMetric::Cosine, 0x2003_0c05);
}

/// コサインのゼロノルムクエリは、GPU の有無に依らず primary に渡る前に `ZeroNormQuery`
/// で拒否され、縮退イベントも起きない（誤ラッチ防止）。
#[test]
fn build_with_gpu_metric_cosine_rejects_zero_norm_query_without_fallback_event() {
    let (fx, _queries) = metric_fixture(8, 4, 0x2003_0707, false);
    let observer = RecordingObserver::default();
    let engine = FallbackBatchEngine::build_with_gpu_metric(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
        BatchMetric::Cosine,
        Box::new(observer.clone()),
    )
    .expect("build");
    let events_after_build = observer.events().len();
    let c = ctx("tenant-a");
    let good = [1.0f32, 0.5, 0.25, 0.125];
    let zero = [0.0f32; 4];
    let queries = [
        BatchQuery {
            vector: &good,
            k: 3,
            ctx: &c,
        },
        BatchQuery {
            vector: &zero,
            k: 3,
            ctx: &c,
        },
    ];
    let err = engine
        .batch_search(&queries)
        .expect_err("zero norm query must be rejected");
    assert_eq!(err, BatchSearchError::ZeroNormQuery { query_index: 1 });
    assert_eq!(
        observer.events().len(),
        events_after_build,
        "no extra fallback event"
    );
}

fn gpu_backend_for(fx: &Fixture, metric: BatchMetric) -> Option<GpuBatchBackend> {
    let matrix = ResidentMatrix::build_with_metric(
        &fx.ids,
        &fx.tenant_ids,
        &fx.visibilities,
        fx.dim,
        &fx.vectors,
        metric,
    )
    .expect("resident matrix build");
    match GpuBatchBackend::try_new(matrix) {
        Ok(b) => Some(b),
        Err(e) => {
            eprintln!("gpu unavailable in this environment, skipping gpu branch: {e}");
            None
        }
    }
}

fn check_gpu_backend_metric(metric: BatchMetric, seed: u32) {
    // 奇数次元・複数タイル（GPU_QUERY_TILE_MAX = 16 を超える 20 本）。
    let many = multi_query_fixture(37, 17, 20, seed ^ 0x55);
    let fx = Fixture {
        ids: many.ids.clone(),
        tenant_ids: many.tenant_ids.clone(),
        visibilities: many.visibilities.clone(),
        dim: many.dim,
        vectors: many.vectors.clone(),
    };
    let Some(backend) = gpu_backend_for(&fx, metric) else {
        return;
    };
    assert_eq!(backend.metric(), metric);
    let c = ctx("tenant-a");
    let batch_queries: Vec<BatchQuery<'_>> = many
        .queries
        .iter()
        .map(|v| BatchQuery {
            vector: v,
            k: 5,
            ctx: &c,
        })
        .collect();
    let hits = backend
        .batch_search(&batch_queries)
        .expect("gpu batch_search");
    for (qi, query) in many.queries.iter().enumerate() {
        let expected = metric_oracle(&fx, query, 5, &c, metric);
        assert_matches_metric_oracle(
            &hits[qi].hits,
            &expected,
            &format!("{metric:?} tile query {qi}"),
        );
    }
    let stats = backend.stats();
    assert!(
        stats.partial_topk_dispatches + stats.full_readback_dispatches > 0,
        "gpu dispatch must actually have happened"
    );
}

#[test]
fn gpu_backend_l2_matches_oracle_when_gpu_available() {
    check_gpu_backend_metric(BatchMetric::L2, 0x2003_a001);
}

#[test]
fn gpu_backend_cosine_matches_oracle_when_gpu_available() {
    check_gpu_backend_metric(BatchMetric::Cosine, 0x2003_a002);
}

/// 複数テナント混在（Private）でも L2・コサインで他テナントの行が混ざらない。
#[test]
fn gpu_backend_metric_mixed_tenants_have_no_cross_tenant_leak_when_gpu_available() {
    for (metric, seed) in [
        (BatchMetric::L2, 0x2003_b001),
        (BatchMetric::Cosine, 0x2003_b002),
    ] {
        let (fx, queries) = metric_fixture(30, 8, seed, true);
        let Some(backend) = gpu_backend_for(&fx, metric) else {
            return;
        };
        let ctx_a = ctx_with_private("tenant-a");
        let ctx_b = ctx_with_private("tenant-b");
        let bq = [
            BatchQuery {
                vector: &queries[0],
                k: 30,
                ctx: &ctx_a,
            },
            BatchQuery {
                vector: &queries[1],
                k: 30,
                ctx: &ctx_b,
            },
        ];
        let hits = backend.batch_search(&bq).expect("gpu batch_search");
        for (hit_set, tenant) in hits.iter().zip(["tenant-a", "tenant-b"]) {
            assert!(!hit_set.hits.is_empty());
            for h in &hit_set.hits {
                assert_eq!(h.tenant_id, tenant, "{metric:?}: cross-tenant leak");
            }
        }
        let expected = metric_oracle(&fx, &queries[0], 30, &ctx_a, metric);
        assert_matches_metric_oracle(&hits[0].hits, &expected, &format!("{metric:?} tenant-a"));
    }
}

/// L2 で f16 算術版を強制すると、黙って縮退せず拒否される（fail-closed）。GPU が無い環境では
/// 構築自体が `InitFailed` のため GPU 分岐のみ検証する。
#[cfg(feature = "bench-internals")]
#[test]
fn gpu_backend_l2_rejects_forced_f16_arith_when_gpu_available() {
    let (fx, queries) = metric_fixture(10, 4, 0x2003_c001, false);
    let Some(backend) = gpu_backend_for(&fx, BatchMetric::L2) else {
        return;
    };
    let c = ctx("tenant-a");
    let bq = [BatchQuery {
        vector: &queries[0][..4],
        k: 3,
        ctx: &c,
    }];
    let res = backend.batch_search_with_options_for_tests(
        &bq,
        engine::gpu_batch::GpuSearchTestOptions {
            budget_bytes: 32 * 1024 * 1024,
            force_full_readback: false,
            dot_shader: Some(engine::gpu_batch::GpuDotShaderKind::F16Arith),
        },
    );
    assert!(res.is_err(), "forced f16 arith must be rejected for L2");
}

/// 部分 Top-k（既定）と強制全量 readback が L2・コサインでビット同一（シェーダ内の符号反転・
/// 正規化済み内積が両経路で同じ演算順であることの回帰）。
#[cfg(feature = "bench-internals")]
#[test]
fn gpu_backend_metric_partial_topk_matches_forced_full_readback_when_gpu_available() {
    for (metric, seed) in [
        (BatchMetric::L2, 0x2003_d001),
        (BatchMetric::Cosine, 0x2003_d002),
    ] {
        let (fx, queries) = metric_fixture(300, 17, seed, false);
        let Some(backend) = gpu_backend_for(&fx, metric) else {
            return;
        };
        let c = ctx("tenant-a");
        let bq: Vec<BatchQuery<'_>> = queries
            .iter()
            .map(|v| BatchQuery {
                vector: v,
                k: 8,
                ctx: &c,
            })
            .collect();
        let run = |force_full: bool, budget: usize| {
            backend
                .batch_search_with_options_for_tests(
                    &bq,
                    engine::gpu_batch::GpuSearchTestOptions {
                        budget_bytes: budget,
                        force_full_readback: force_full,
                        dot_shader: None,
                    },
                )
                .expect("gpu search")
        };
        let partial = run(false, 32 * 1024 * 1024);
        let full = run(true, 32 * 1024 * 1024);
        // 行チャンク分割（端数を含む）でも同じ結果になる。
        let chunked = run(true, 2_000);
        for other in [&full, &chunked] {
            for (a, b) in partial.iter().zip(other.iter()) {
                assert_eq!(a.hits.len(), b.hits.len());
                for (x, y) in a.hits.iter().zip(b.hits.iter()) {
                    assert_eq!(x.id, y.id, "{metric:?}");
                    assert_eq!(x.score.to_bits(), y.score.to_bits(), "{metric:?}");
                }
            }
        }
    }
}

/// f32 常駐対照経路の metric 指定版。GPU が無い環境では fail-closed で `Err`、ある環境では
/// オラクルと一致する。
#[test]
fn f32_contrast_backend_metric_matches_oracle_or_fails_closed() {
    for (metric, seed) in [
        (BatchMetric::L2, 0x2003_e001),
        (BatchMetric::Cosine, 0x2003_e002),
    ] {
        let (fx, queries) = metric_fixture(25, 7, seed, false);
        let built = GpuF32ContrastBackend::try_new_with_metric(
            &fx.ids,
            &fx.tenant_ids,
            &fx.visibilities,
            fx.dim,
            &fx.vectors,
            metric,
        );
        let backend = match built {
            Ok(b) => b,
            Err(e) => {
                // GPU 非搭載環境: 構築が InitFailed で fail-closed に止まる。
                assert!(
                    matches!(e, engine::batch_fallback::BatchBackendError::InitFailed(_)),
                    "unexpected error: {e}"
                );
                continue;
            }
        };
        assert_eq!(BatchBackend::metric(&backend), metric);
        let c = ctx("tenant-a");
        for (qi, q) in queries.iter().enumerate() {
            let bq = BatchQuery {
                vector: q,
                k: 5,
                ctx: &c,
            };
            let hits = backend
                .batch_search(std::slice::from_ref(&bq))
                .expect("f32 search");
            let expected = metric_oracle(&fx, q, 5, &c, metric);
            assert_matches_metric_oracle(
                &hits[0].hits,
                &expected,
                &format!("f32 {metric:?} q{qi}"),
            );
        }
    }
}
