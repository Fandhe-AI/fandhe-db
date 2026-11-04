//! GPU バッチ検索（`engine::gpu_batch`。TASK-128〜130・Issue #178 ポインタ）が
//! CPU-SIMD バッチ経路（`engine::batch_search::BatchEngine`）に対してどの規模・
//! バッチサイズから優位になるかを実測する、手動専用の規模スイープベンチ。
//!
//! `batch_bench.rs`（TASK-130・CORE-6/CORE-16）は固定規模（20,000 行・dim 256・
//! batch 8）での単一の A/B ゲート判定を担うのに対し、本ベンチは spec 由来の
//! 閾値を持たない情報提供専用の測定ツールであり、`BENCH_GPU_SCALING_ROWS`/
//! `BENCH_GPU_SCALING_DIMS`/`BENCH_GPU_SCALING_BATCH` の直積で構成した複数点を
//! 掃引し、各点の p50/p95・1 クエリあたりのレイテンシ・GPU f16 常駐経路の
//! 短縮率（`speedup_f16_p95`）を出力する（`hnsw_parallel_build_bench.rs`・
//! `Makefile: bench-hnsw-parallel-build` と同じ「手動専用・CI 非配線・情報提供」
//! の位置づけ）。
//!
//! 計測する 3 経路（いずれも `engine::batch_fallback::BatchBackend` 実装。
//! `FallbackBatchEngine` は経由しない——batch_bench.rs の CORE-6/CORE-16 と
//! 同方針で、CPU 縮退が混ざると経路比較にならないため）:
//! - A: CPU-SIMD（`engine::batch_search::BatchEngine`。f16 パック常駐行列を
//!   CPU の実行時 SIMD 検出カーネルで全走査）
//! - B: GPU f16 常駐（`engine::gpu_batch::GpuBatchBackend`。本番の dispatch 経路）
//! - C: GPU f32 常駐対照（`engine::gpu_batch::GpuF32ContrastBackend`。
//!   Issue #234・CORE-16 の対照経路を流用）
//!
//! 各構成の計測に先立ち、A を厳密対照として B・C の Top-k 結果が「A の id
//! 集合に含まれる」か「A の k 位スコア以上」のいずれかを満たすかを 1 回だけ
//! （計測区間外で）確認し、満たさない件数を `mismatch=` として出力する
//! （`harness::gpu_scaling::count_boundary_tolerant_mismatches`）。
//!
//! GPU アダプタ名・backend 種別の出力: `engine::gpu_batch` は adapter 情報を
//! 取得できる公開関数を持たないため（`wgpu::AdapterInfo` を返す getter が
//! `pub` で露出していない）、本ベンチはアダプタ名を出力しない（production
//! コード変更なしの制約下での取得経路なし）。
//!
//! GPU が全く初期化できない環境（adapter 未検出等）では起動直後の疎通確認で
//! 即座に非 0 終了する（fail-closed。CPU 経路だけの実測を GPU 実測の代替として
//! 出力しない）。個々の (rows, dim, batch) 点が測定量上限（`MAX_BATCH_WORK`・
//! `MAX_BATCH_ROWS`）を超える、またはメモリ確保に失敗する場合はその点だけを
//! `skip`/`not measurable` として飛ばし、他の点の計測は継続する。
//!
//! # 経路 D: GPU i8 パック常駐（Issue #543・親 #541）
//!
//! 上記 A/B/C の後段で、opt-in 専用の `engine::gpu_batch::packed_i8::
//! GpuI8BatchBackend`（Issue #542。候補生成のみ・最終スコアは f32 再計算）を
//! 追加で計測し、`gpu_scaling_i8:`/`gpu_scaling_i8_stats:` 行として出力する。
//! 既存 A/B/C の分岐・出力順序は一切変更しない（`gpu_scaling:`/
//! `gpu_scaling_stats:` 行は 1 文字も変えない）——i8 経路の構築・計測に失敗
//! しても、その規模点の A/B/C の結果は失われない。
//!
//! i8 バックエンドは `GpuI8Options::oversample` を構築時に固定するため
//! （`docs/design/gpu-batch-i8-packed.md`「D9」節）、プロセス内スイープは行わず
//! `BENCH_GPU_SCALING_I8_OVERSAMPLE`（単一値・既定
//! `packed_i8::DEFAULT_I8_OVERSAMPLE`）で 1 プロセス = 1 oversample を計測する。
//! oversample のスイープは `scripts/bench_gpu_scaling_ab.sh` 側でプロセスを
//! 複数回起動して行う。
//!
//! # metric 別計測（Issue #2005・親 #2002。ポインタ: CORE-6・CORE-16・SQL-33）
//!
//! `BENCH_GPU_SCALING_METRIC`（`inner_product`〔既定〕・`l2`・`cosine` の
//! allowlist。`l1` を含むそれ以外は fail-closed で起動拒否）で、A/B/C/D の全経路を
//! 同じ metric で構築し直す。1 プロセス = 1 metric（同一プロセス内の連続計測は
//! ノイズが乗るため、metric 間の比較はプロセスを分けて行う）。既存行
//! （`gpu_scaling:`・`gpu_scaling_stats:`・`gpu_scaling_i8:`・`gpu_scaling_i8_stats:`）の
//! 書式は 1 文字も変えず、次の追加行だけを出す:
//! - 起動直後に 1 回: `gpu_scaling_metric: metric=<name>`
//! - 規模点ごと: `gpu_scaling_recall: ... f16_recall=.. f32_recall=..`
//!   （基準は同じ metric の CPU-SIMD 経路の Top-k の id 集合）
//!
//! packed i8 は常駐行列を生の行（内積）で持ち、metric は別引数で渡す。i8 の
//! コサインの最終スコアは `-(1 - cos)` で CPU 基準（cos 類似度）と 1.0 ずれるため、
//! 不一致件数の算出時だけ +1.0 して尺度を揃える（丸めの影響で参考値。主指標は
//! id ベースの Recall@k）。

#[allow(dead_code)]
mod harness;

use harness::accept::{p95_from_samples, recall_at_k};
use harness::env_report::EnvReport;
use harness::gpu_scaling::{
    count_boundary_tolerant_mismatches, format_i8_unavailable_line, format_metric_line,
    format_skip_line, format_unavailable_line, mean_recall_at_k, parse_batches, parse_dims,
    parse_i8_oversample, parse_measured_iterations, parse_metric, parse_query_f16_exact,
    parse_rows, parse_shader_ab, parse_top_k, read_env_var, readback_bytes_per_call,
    rescored_candidates_per_call, round_to_f16_exact, speedup_ratio, BenchMetric,
    GpuScalingI8Result, GpuScalingI8StatsLine, GpuScalingRecallLine, GpuScalingResult,
    GpuScalingStatsLine,
};
use harness::protocol::{run_fallible, MeasurementConfig, TrialFailure};
use harness::rng::DeterministicRng;

use engine::batch_fallback::{BatchBackend, BatchExecError};
use engine::batch_search::{
    BatchEngine, BatchHit, BatchQuery, ResidentMatrix, MAX_BATCH_QUERIES, MAX_BATCH_ROWS,
    MAX_BATCH_TOTAL_BYTES, MAX_BATCH_WORK,
};
use engine::gpu_batch::packed_i8::{
    GpuI8BatchBackend, GpuI8Options, DEFAULT_I8_OVERSAMPLE, MAX_I8_OVERSAMPLE,
};
use engine::gpu_batch::{GpuBatchBackend, GpuBatchMetric, GpuF32ContrastBackend};
#[cfg(feature = "bench-internals")]
use engine::gpu_batch::{GpuDotShaderKind, GpuSearchTestOptions};
use engine::kernel::SearchHit;
use engine::policy::PolicyContext;
use engine::storage::Visibility;
use std::time::Duration;

/// 本ベンチ専用の合成データセットが使うテナント ID（実データではない。
/// `batch_bench.rs::BENCH_TENANT` と同じ位置づけ）。
const BENCH_TENANT: &str = "gpu-scaling-bench-tenant";

/// `BENCH_GPU_SCALING_ROWS` 既定値。GPU 転送・dispatch の固定コストを償却できる
/// 下限（20,000）から、f16 常駐で 1GiB 予算（`MAX_BATCH_TOTAL_BYTES`）に迫る
/// 規模（500,000 × dim 256 で約 256 MiB）までを掃引する。
const DEFAULT_ROWS: [usize; 3] = [20_000, 100_000, 500_000];
/// `BENCH_GPU_SCALING_DIMS` 既定値。
const DEFAULT_DIMS: [usize; 2] = [128, 256];
/// `BENCH_GPU_SCALING_BATCH` 既定値。GPU 経路はクエリ単位に dispatch するため、
/// 1 本（固定コスト支配）から 256 本（転送・同期コストを償却できる規模）まで。
const DEFAULT_BATCH: [usize; 4] = [1, 8, 64, 256];
/// `BENCH_GPU_SCALING_TOPK` 既定値。
const DEFAULT_TOP_K: usize = 10;
/// `BENCH_GPU_SCALING_ITERS`（計測反復回数）既定値。warmup は
/// `harness::protocol::MeasurementConfig` の下限（20 回）固定で上書き対象外。
const DEFAULT_MEASURED_ITERATIONS: u32 = 20;
const WARMUP_ITERATIONS: u32 = 20;

/// 1 つの (rows, dim, batch) 構成分の合成データセット。`batch_bench.rs::
/// GateDataset` と同型だが、行数・次元・バッチ本数がすべて可変である点が異なる。
struct ScalingDataset {
    ids: Vec<u64>,
    tenant_ids: Vec<String>,
    visibilities: Vec<Visibility>,
    vectors: Vec<f32>,
    queries: Vec<Vec<f32>>,
}

/// `rows`/`dim`/`batch` から合成データセットを構築する。アロケーションは
/// すべて `try_reserve_exact` を経由し（coding-rust.md「長さフィールドは上限
/// 検証してからアロケーションに使う」）、確保失敗は panic ではなく `Err` として
/// 呼び出し元（`main` のスイープループ）へ返し、この構成点だけを飛ばせるようにする。
fn build_dataset(
    rng: &mut DeterministicRng,
    rows: usize,
    dim: usize,
    batch: usize,
) -> Result<ScalingDataset, String> {
    let elements = rows
        .checked_mul(dim)
        .ok_or_else(|| "rows * dim overflows usize".to_string())?;

    let mut vectors: Vec<f32> = Vec::new();
    vectors
        .try_reserve_exact(elements)
        .map_err(|e| format!("vectors allocation failed: {e}"))?;
    for _ in 0..rows {
        vectors.extend(rng.next_vector(dim));
    }

    let mut ids: Vec<u64> = Vec::new();
    ids.try_reserve_exact(rows)
        .map_err(|e| format!("ids allocation failed: {e}"))?;
    ids.extend(0..rows as u64);

    let mut tenant_ids: Vec<String> = Vec::new();
    tenant_ids
        .try_reserve_exact(rows)
        .map_err(|e| format!("tenant_ids allocation failed: {e}"))?;
    tenant_ids.extend(std::iter::repeat_n(BENCH_TENANT.to_string(), rows));

    let mut visibilities: Vec<Visibility> = Vec::new();
    visibilities
        .try_reserve_exact(rows)
        .map_err(|e| format!("visibilities allocation failed: {e}"))?;
    visibilities.extend(std::iter::repeat_n(Visibility::Public, rows));

    let mut queries: Vec<Vec<f32>> = Vec::new();
    queries
        .try_reserve_exact(batch)
        .map_err(|e| format!("queries allocation failed: {e}"))?;
    for _ in 0..batch {
        queries.push(rng.next_vector(dim));
    }

    Ok(ScalingDataset {
        ids,
        tenant_ids,
        visibilities,
        vectors,
        queries,
    })
}

/// `dataset.queries` から測定区間外で `BatchQuery` 列を組み立てる
/// （`batch_bench.rs::gate_batch_queries` と同方針）。
fn batch_queries<'a>(
    queries: &'a [Vec<f32>],
    ctx: &'a PolicyContext,
    k: usize,
) -> Vec<BatchQuery<'a>> {
    queries
        .iter()
        .map(|q| BatchQuery {
            vector: q.as_slice(),
            k,
            ctx,
        })
        .collect()
}

/// `BatchHit::hits`（`SearchHit` 列）を `(id, score)` の組へ変換する
/// （`harness::gpu_scaling::count_boundary_tolerant_mismatches` の入力形）。
fn hit_pairs(hits: &[SearchHit]) -> Vec<(u64, f32)> {
    hits.iter().map(|h| (h.id, h.score)).collect()
}

/// 環境変数から本ベンチの計測条件一式を読み取る。
struct ScalingConfig {
    rows: Vec<usize>,
    dims: Vec<usize>,
    batches: Vec<usize>,
    top_k: usize,
    measured_iterations: u32,
    /// `BENCH_GPU_SCALING_I8_OVERSAMPLE`（単一値。Issue #543。
    /// `GpuI8Options::oversample` は構築時固定のためプロセス内スイープしない
    /// ——`gpu_scaling_bench.rs` モジュール冒頭コメント「経路 D」参照）。
    i8_oversample: usize,
    /// `BENCH_GPU_SCALING_QUERY_F16_EXACT`（Issue #540。
    /// `harness::gpu_scaling::parse_query_f16_exact` doc 参照）。
    query_f16_exact: bool,
    /// `BENCH_GPU_SCALING_SHADER_AB`（Issue #540 追記・codex-review P2 指摘
    /// 対応・PR #611。`harness::gpu_scaling::parse_shader_ab` doc 参照。
    /// `bench-internals` feature 必須・`query_f16_exact` 同時指定必須——
    /// `load_config` が fail-closed に検証する）。`bench-internals` 非有効時は
    /// 読み出し先（`main` の `#[cfg(feature = "bench-internals")]` 分岐）が
    /// 存在しないため `#[allow(dead_code)]`（`mod harness;` と同方針）。
    #[allow(dead_code)]
    shader_ab: bool,
    /// `BENCH_GPU_SCALING_METRIC`（Issue #2005。1 プロセス = 1 metric）。
    metric: BenchMetric,
}

fn load_config() -> Result<ScalingConfig, String> {
    let rows_raw = read_env_var("BENCH_GPU_SCALING_ROWS").map_err(|e| e.to_string())?;
    let dims_raw = read_env_var("BENCH_GPU_SCALING_DIMS").map_err(|e| e.to_string())?;
    let batch_raw = read_env_var("BENCH_GPU_SCALING_BATCH").map_err(|e| e.to_string())?;
    let topk_raw = read_env_var("BENCH_GPU_SCALING_TOPK").map_err(|e| e.to_string())?;
    let iters_raw = read_env_var("BENCH_GPU_SCALING_ITERS").map_err(|e| e.to_string())?;
    let i8_oversample_raw =
        read_env_var("BENCH_GPU_SCALING_I8_OVERSAMPLE").map_err(|e| e.to_string())?;
    let query_f16_exact_raw =
        read_env_var("BENCH_GPU_SCALING_QUERY_F16_EXACT").map_err(|e| e.to_string())?;
    let shader_ab_raw = read_env_var("BENCH_GPU_SCALING_SHADER_AB").map_err(|e| e.to_string())?;
    let metric_raw = read_env_var("BENCH_GPU_SCALING_METRIC").map_err(|e| e.to_string())?;

    let rows = parse_rows(rows_raw.as_deref(), &DEFAULT_ROWS).map_err(|e| e.to_string())?;
    let dims = parse_dims(dims_raw.as_deref(), &DEFAULT_DIMS).map_err(|e| e.to_string())?;
    let batches = parse_batches(batch_raw.as_deref(), &DEFAULT_BATCH).map_err(|e| e.to_string())?;
    let top_k = parse_top_k(topk_raw.as_deref(), DEFAULT_TOP_K).map_err(|e| e.to_string())?;
    let measured_iterations =
        parse_measured_iterations(iters_raw.as_deref(), DEFAULT_MEASURED_ITERATIONS)
            .map_err(|e| e.to_string())?;
    let i8_oversample = parse_i8_oversample(
        i8_oversample_raw.as_deref(),
        DEFAULT_I8_OVERSAMPLE,
        MAX_I8_OVERSAMPLE,
    )
    .map_err(|e| e.to_string())?;
    let query_f16_exact =
        parse_query_f16_exact(query_f16_exact_raw.as_deref()).map_err(|e| e.to_string())?;
    let shader_ab = parse_shader_ab(shader_ab_raw.as_deref()).map_err(|e| e.to_string())?;
    let metric = parse_metric(metric_raw.as_deref())
        .map_err(|e| format!("BENCH_GPU_SCALING_METRIC: {e}"))?;
    if shader_ab {
        // fail-closed（Issue #540 追記）: シェーダ強制 `F16Arith` は
        // `select_dot_shader` の条件 5（クエリが f16 厳密往復不能なら拒否）を
        // 経由するため、`query_f16_exact` なしで `shader_ab` だけを有効化すると
        // 必ず実行時エラーになる。起動直後に明示的に拒否する。
        if !query_f16_exact {
            return Err(
                "BENCH_GPU_SCALING_SHADER_AB=1 requires BENCH_GPU_SCALING_QUERY_F16_EXACT=1 \
                 (forcing the f16 arith shader on non-exact queries always fails the \
                 select_dot_shader overflow guard)"
                    .to_string(),
            );
        }
        #[cfg(not(feature = "bench-internals"))]
        {
            return Err(
                "BENCH_GPU_SCALING_SHADER_AB=1 requires the bench-internals feature \
                 (cargo bench --bench gpu_scaling_bench --features bench-internals)"
                    .to_string(),
            );
        }
    }

    Ok(ScalingConfig {
        rows,
        dims,
        batches,
        top_k,
        measured_iterations,
        i8_oversample,
        query_f16_exact,
        shader_ab,
        metric,
    })
}

/// `BenchMetric` を i8 経路の `GpuBatchMetric` へ写す（production に
/// `From<BatchMetric> for GpuBatchMetric` が無いためベンチ内で持つ）。
fn to_gpu_batch_metric(metric: BenchMetric) -> GpuBatchMetric {
    match metric {
        BenchMetric::InnerProduct => GpuBatchMetric::InnerProduct,
        BenchMetric::L2 => GpuBatchMetric::L2,
        BenchMetric::Cosine => GpuBatchMetric::Cosine,
    }
}

/// `baseline`（CPU-SIMD 基準）に対する `other` のクエリ平均 Recall@k（id 集合比較）。
/// 計測区間外で 1 回だけ呼ぶ。算出不能（クエリ数不一致・基準が空）は `Err`。
fn mean_recall_against(baseline: &[BatchHit], other: &[BatchHit]) -> Result<f64, String> {
    if baseline.len() != other.len() {
        return Err("query count mismatch".to_string());
    }
    let mut per_query: Vec<f64> = Vec::with_capacity(baseline.len());
    for (b, o) in baseline.iter().zip(other.iter()) {
        let expected: Vec<u64> = b.hits.iter().map(|h| h.id).collect();
        let actual: Vec<u64> = o.hits.iter().map(|h| h.id).collect();
        if let Ok(r) = recall_at_k(&expected, &actual) {
            per_query.push(r);
        }
    }
    mean_recall_at_k(&per_query).map_err(|e| e.to_string())
}

/// 起動直後の GPU 疎通確認。1 行だけの最小データセットで
/// [`GpuBatchBackend::try_new`] を試み、失敗した場合はプロセス全体として GPU が
/// 使えない環境だとみなす（fail-closed。以降のスイープを一切実行せず終了する）。
fn probe_gpu_available(rng: &mut DeterministicRng, dim: usize) -> Result<(), String> {
    let ids = vec![0u64];
    let tenant_ids = vec![BENCH_TENANT.to_string()];
    let visibilities = vec![Visibility::Public];
    let vectors = rng.next_vector(dim);
    let matrix = ResidentMatrix::build(&ids, &tenant_ids, &visibilities, dim, &vectors)
        .map_err(|e| format!("resident matrix build failed during gpu probe: {e}"))?;
    GpuBatchBackend::try_new(matrix)
        .map(|_backend| ())
        .map_err(|e| e.to_string())
}

/// 経路 D（Issue #543）の 1 規模点分の計測を行う。`cpu_hits` は A（CPU-SIMD
/// 厳密対照。呼び出し元がこの規模点で既に計測済みの結果）で、これを正解集合と
/// して i8 経路の同点許容つき不一致件数・Recall@k を確定的に算出する。
/// `cpu_p95`/`f16_p95` は呼び出し元が同じ規模点で既に確定済みの A・B の p95
/// （速度比の分子に使う。i8 経路自体の再計測はしない）。失敗はすべて
/// `format_i8_unavailable_line` を出力してこの規模点だけを飛ばし、呼び出し元
/// （`main`）の A/B/C の結果・継続に影響しない。
#[allow(clippy::too_many_arguments)]
fn measure_i8_scaling_point(
    dataset: &ScalingDataset,
    ctx: &PolicyContext,
    rows: usize,
    dim: usize,
    batch: usize,
    k: usize,
    oversample: usize,
    config: &ScalingConfig,
    cpu_hits: &[BatchHit],
    cpu_p95: Duration,
    f16_p95: Duration,
    seed: u64,
) {
    let i8_matrix = match ResidentMatrix::build(
        &dataset.ids,
        &dataset.tenant_ids,
        &dataset.visibilities,
        dim,
        &dataset.vectors,
    ) {
        Ok(m) => m,
        Err(e) => {
            println!(
                "{}",
                format_i8_unavailable_line(
                    rows,
                    dim,
                    batch,
                    k,
                    oversample,
                    &format!("gpu i8 resident matrix build failed: {e}")
                )
            );
            return;
        }
    };

    let build_started = std::time::Instant::now();
    // 常駐行列は生の行（内積）のまま。metric は別引数で渡す（`GpuI8BatchBackend::
    // try_new_with_metric` は内積以外の行列を拒否する）。
    let gpu_i8 = match GpuI8BatchBackend::try_new_with_metric(
        i8_matrix,
        GpuI8Options { oversample },
        to_gpu_batch_metric(config.metric),
    ) {
        Ok(b) => b,
        Err(e) => {
            println!(
                "{}",
                format_i8_unavailable_line(rows, dim, batch, k, oversample, &e.to_string())
            );
            return;
        }
    };
    let build_ms = build_started.elapsed().as_millis();

    let queries = batch_queries(&dataset.queries, ctx, k);

    // 正しさ・Recall（計測区間外・1 回のみ）: A（CPU-SIMD 厳密対照）を正解集合
    // として、i8 経路の同点許容つき不一致件数と Recall@k を確定的に算出する
    // （量子化・候補生成のみで決まる決定的指標。`docs/design/
    // benchmark-judgement-policy.md` の「共有環境でも確定的に判定できる指標」
    // と同じ位置づけ）。
    let i8_hits = match gpu_i8.batch_search(&queries) {
        Ok(h) => h,
        Err(e) => {
            println!(
                "{}",
                format_i8_unavailable_line(
                    rows,
                    dim,
                    batch,
                    k,
                    oversample,
                    &format!("gpu i8 batch_search failed: {e}")
                )
            );
            return;
        }
    };
    if cpu_hits.len() != i8_hits.len() {
        println!(
            "{}",
            format_i8_unavailable_line(
                rows,
                dim,
                batch,
                k,
                oversample,
                "query count mismatch between cpu baseline and i8 candidate"
            )
        );
        return;
    }

    let mut i8_mismatch = 0usize;
    let mut per_query_recall: Vec<f64> = Vec::with_capacity(cpu_hits.len());
    for i in 0..cpu_hits.len() {
        let baseline = hit_pairs(&cpu_hits[i].hits);
        let mut candidate = hit_pairs(&i8_hits[i].hits);
        if config.metric == BenchMetric::Cosine {
            // i8 のコサインは `-(1 - cos)` を返す。CPU 基準（cos 類似度）の尺度へ戻す。
            for (_, score) in candidate.iter_mut() {
                *score += 1.0;
            }
        }
        i8_mismatch += count_boundary_tolerant_mismatches(&baseline, &candidate);

        let expected_ids: Vec<u64> = cpu_hits[i].hits.iter().map(|h| h.id).collect();
        let actual_ids: Vec<u64> = i8_hits[i].hits.iter().map(|h| h.id).collect();
        match recall_at_k(&expected_ids, &actual_ids) {
            Ok(r) => per_query_recall.push(r),
            Err(_) => {
                // `expected_ids` が空（`k=0` 相当。本ベンチの `k` は常に
                // `>= 1` のためここへは実質到達しないが、防御的に「算出不能」
                // として計測を継続する）。
                continue;
            }
        }
    }
    let i8_recall_at_k = match mean_recall_at_k(&per_query_recall) {
        Ok(v) => v,
        Err(e) => {
            println!(
                "{}",
                format_i8_unavailable_line(
                    rows,
                    dim,
                    batch,
                    k,
                    oversample,
                    &format!("i8 recall unavailable: {e}")
                )
            );
            return;
        }
    };

    let measure_config = match MeasurementConfig::new(
        WARMUP_ITERATIONS,
        config.measured_iterations,
        seed.saturating_add(1),
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gpu_scaling_bench: i8 measurement config invalid: {e}");
            return;
        }
    };
    fn fatal(_: &BatchExecError) -> TrialFailure {
        TrialFailure::Fatal
    }

    let stats_before = gpu_i8.stats();
    let measurement =
        match run_fallible(&measure_config, 0, || gpu_i8.batch_search(&queries), fatal)
            .map(|m| m.measurement)
        {
            Ok(m) => m,
            Err(e) => {
                println!(
                    "{}",
                    format_i8_unavailable_line(
                        rows,
                        dim,
                        batch,
                        k,
                        oversample,
                        &format!("i8 measurement failed: {e}")
                    )
                );
                return;
            }
        };
    let stats_after = gpu_i8.stats();

    let i8_p95 = match p95_from_samples(&measurement.samples) {
        Ok(v) => v,
        Err(e) => {
            println!(
                "{}",
                format_i8_unavailable_line(
                    rows,
                    dim,
                    batch,
                    k,
                    oversample,
                    &format!("i8 p95 unavailable: {e}")
                )
            );
            return;
        }
    };

    let speedup_i8_vs_cpu_p95 = match speedup_ratio(cpu_p95, i8_p95) {
        Ok(v) => v,
        Err(e) => {
            println!(
                "{}",
                format_i8_unavailable_line(
                    rows,
                    dim,
                    batch,
                    k,
                    oversample,
                    &format!("i8 vs cpu speedup ratio unavailable: {e}")
                )
            );
            return;
        }
    };
    let speedup_i8_vs_f16_p95 = match speedup_ratio(f16_p95, i8_p95) {
        Ok(v) => v,
        Err(e) => {
            println!(
                "{}",
                format_i8_unavailable_line(
                    rows,
                    dim,
                    batch,
                    k,
                    oversample,
                    &format!("i8 vs f16 speedup ratio unavailable: {e}")
                )
            );
            return;
        }
    };

    let batch_u32 = u32::try_from(batch).unwrap_or(1).max(1);
    let result = GpuScalingI8Result {
        rows,
        dim,
        batch,
        k,
        oversample,
        gpu_i8_p50: measurement.summary.median,
        gpu_i8_p95: i8_p95,
        per_query_gpu_i8_p50: measurement.summary.median / batch_u32,
        speedup_i8_vs_cpu_p95,
        speedup_i8_vs_f16_p95,
        i8_mismatch,
        i8_recall_at_k,
    };
    println!("{result}");

    let calls = u64::from(WARMUP_ITERATIONS) + u64::from(measure_config.measured_iterations());
    let readback_bytes_total = stats_after
        .readback_bytes
        .saturating_sub(stats_before.readback_bytes);
    let rescored_candidates_total = stats_after
        .rescored_candidates
        .saturating_sub(stats_before.rescored_candidates);

    match (
        readback_bytes_per_call(readback_bytes_total, calls),
        rescored_candidates_per_call(rescored_candidates_total, calls),
    ) {
        (Ok(bytes_per_call), Ok(rescored_per_call)) => {
            let meta = gpu_i8.meta();
            let stats_line = GpuScalingI8StatsLine {
                rows,
                dim,
                batch,
                k,
                oversample,
                calls,
                readback_bytes_total,
                readback_bytes_per_call: bytes_per_call,
                rescored_candidates_total,
                rescored_candidates_per_call: rescored_per_call,
                backend: format!("{:?}", meta.backend),
                dot4_impl: format!("{:?}", meta.dot4_impl),
                build_ms,
            };
            println!("{stats_line}");
        }
        (a, b) => {
            // `calls == 0` は測定条件の誤りだが、`gpu_scaling_i8:` 結果行は
            // 既に出力済みのため本ベンチ自体は継続する（fail-closed に統計行
            // だけを欠落させる。既存 `gpu_scaling_stats:` の欠落方針と同じ）。
            eprintln!(
                "gpu_scaling_bench: i8 readback stats unavailable rows={rows} dim={dim} \
                 batch={batch} k={k} oversample={oversample}: readback={a:?} rescored={b:?}"
            );
        }
    }
}

/// 同一クエリでの S0 シェーダ単体比較（Issue #540 追記・codex-review P2
/// 指摘対応・PR #611）。`BENCH_GPU_SCALING_SHADER_AB=1`（`bench-internals`
/// feature 必須・`config.query_f16_exact` 併用必須）のときのみ `main` から
/// 呼ばれる。`queries` は呼び出し元が既に [`round_to_f16_exact`] で丸め済み
/// （`config.query_f16_exact`）であることを前提とし、この関数自体は丸めを
/// 行わない——`GpuSearchTestOptions::dot_shader` で `Unpack`／`F16Arith` を
/// 強制する以外、`gpu_scaling: `（既存の自動選択計測）と同一のクエリ・
/// 同一の `gpu_f16`（`GpuBatchBackend`）インスタンスを使うことで、変動要因を
/// シェーダ選択のみに限定する。`F16Arith` 強制は `select_dot_shader` の
/// 条件不成立時に `Err` を返す fail-closed 契約（`GpuSearchTestOptions::
/// dot_shader` doc 参照）のため、クエリが実際に f16 算術版シェーダへ到達した
/// ことの証跡にもなる。
#[cfg(feature = "bench-internals")]
#[allow(clippy::too_many_arguments)]
fn measure_shader_ab(
    gpu_f16: &GpuBatchBackend,
    queries: &[BatchQuery<'_>],
    rows: usize,
    dim: usize,
    batch: usize,
    k: usize,
    measured_iterations: u32,
    seed: u64,
) -> Result<String, String> {
    let measure_config = MeasurementConfig::new(WARMUP_ITERATIONS, measured_iterations, seed)
        .map_err(|e| format!("shader_ab measurement config invalid: {e}"))?;

    fn fatal<E>(_: &E) -> TrialFailure {
        TrialFailure::Fatal
    }

    let unpack_options = GpuSearchTestOptions {
        // `GpuBatchBackend::batch_search`（既定 trait 実装）と同じ既定予算
        // （`gpu_batch.rs::GPU_SCORE_BUFFER_BUDGET_BYTES`。非 pub 定数のため
        // `tests/gpu_batch.rs` と同じくリテラルで複製する）。
        budget_bytes: 32 * 1024 * 1024,
        force_full_readback: false,
        dot_shader: Some(GpuDotShaderKind::Unpack),
    };
    let unpack_measurement = run_fallible(
        &measure_config,
        0,
        || gpu_f16.batch_search_with_options_for_tests(queries, unpack_options),
        fatal,
    )
    .map(|m| m.measurement)
    .map_err(|e| format!("shader_ab unpack measurement failed: {e}"))?;

    let f16_arith_options = GpuSearchTestOptions {
        // `GpuBatchBackend::batch_search`（既定 trait 実装）と同じ既定予算
        // （`gpu_batch.rs::GPU_SCORE_BUFFER_BUDGET_BYTES`。非 pub 定数のため
        // `tests/gpu_batch.rs` と同じくリテラルで複製する）。
        budget_bytes: 32 * 1024 * 1024,
        force_full_readback: false,
        dot_shader: Some(GpuDotShaderKind::F16Arith),
    };
    let f16_arith_measurement = run_fallible(
        &measure_config,
        0,
        || gpu_f16.batch_search_with_options_for_tests(queries, f16_arith_options),
        fatal,
    )
    .map(|m| m.measurement)
    .map_err(|e| {
        format!(
            "shader_ab f16_arith measurement failed (forced f16 arith shader was rejected \
             — queries may not be f16-exact; requires BENCH_GPU_SCALING_QUERY_F16_EXACT=1): {e}"
        )
    })?;

    let unpack_p95 = p95_from_samples(&unpack_measurement.samples)
        .map_err(|e| format!("shader_ab unpack p95 unavailable: {e}"))?;
    let f16_arith_p95 = p95_from_samples(&f16_arith_measurement.samples)
        .map_err(|e| format!("shader_ab f16_arith p95 unavailable: {e}"))?;

    Ok(format!(
        "gpu_scaling_shader_ab: rows={rows} dim={dim} batch={batch} k={k} \
         unpack_p50={}us unpack_p95={}us f16_arith_p50={}us f16_arith_p95={}us",
        unpack_measurement.summary.median.as_micros(),
        unpack_p95.as_micros(),
        f16_arith_measurement.summary.median.as_micros(),
        f16_arith_p95.as_micros(),
    ))
}

fn main() {
    let config = match load_config() {
        Ok(c) => c,
        Err(msg) => {
            eprintln!("gpu_scaling_bench: {msg}");
            std::process::exit(1);
        }
    };

    let detected_isa = format!("{:?}", engine::isa::current().isa());
    println!("{}", EnvReport::capture(detected_isa));
    println!("{}", format_metric_line(config.metric));

    let mut rng = DeterministicRng::new(1);

    // 起動直後の GPU 疎通確認（プロセス全体として GPU が使えない環境は即座に
    // fail-closed で終了する。CPU 経路だけの実測を GPU 実測の代替として
    // 出力しない）。プローブに使う次元はスイープ対象の最小次元を使う。
    let probe_dim = config.dims.iter().copied().min().unwrap_or(DEFAULT_DIMS[0]);
    if let Err(msg) = probe_gpu_available(&mut rng, probe_dim) {
        println!("{}", format_unavailable_line(None, &msg));
        std::process::exit(1);
    }

    let ctx = match PolicyContext::new(BENCH_TENANT) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("gpu_scaling_bench: failed to construct policy context: {e}");
            std::process::exit(1);
        }
    };

    let mut any_measured = false;
    let mut seed: u64 = 1;

    for &rows in &config.rows {
        for &dim in &config.dims {
            for &batch in &config.batches {
                seed = seed.saturating_add(1);
                let k = config.top_k;

                if batch > MAX_BATCH_QUERIES {
                    println!(
                        "{}",
                        format_skip_line(
                            rows,
                            dim,
                            batch,
                            k,
                            "batch exceeds engine::batch_search::MAX_BATCH_QUERIES"
                        )
                    );
                    continue;
                }

                if rows > MAX_BATCH_ROWS {
                    println!(
                        "{}",
                        format_skip_line(
                            rows,
                            dim,
                            batch,
                            k,
                            "rows exceeds engine::batch_search::MAX_BATCH_ROWS"
                        )
                    );
                    continue;
                }
                let total_work = rows.checked_mul(batch).and_then(|v| v.checked_mul(dim));
                match total_work {
                    Some(work) if work <= MAX_BATCH_WORK => {}
                    _ => {
                        println!(
                            "{}",
                            format_skip_line(
                                rows,
                                dim,
                                batch,
                                k,
                                "rows * batch * dim exceeds MAX_BATCH_WORK"
                            )
                        );
                        continue;
                    }
                }

                // データ生成前にバイト量を検査する。`ResidentMatrix::build` の
                // 1 GiB 上限（`MAX_BATCH_TOTAL_BYTES`）で拒否される組み合わせは f32 の
                // コーパスを確保する前に除外し、実ページ確保時のメモリ不足でプロセスが
                // 停止する前にスキップ行を出力する。
                let dataset_bytes = rows
                    .checked_mul(dim)
                    .and_then(|v| v.checked_mul(std::mem::size_of::<f32>()));
                match dataset_bytes {
                    Some(bytes) if bytes <= MAX_BATCH_TOTAL_BYTES => {}
                    _ => {
                        println!(
                            "{}",
                            format_skip_line(
                                rows,
                                dim,
                                batch,
                                k,
                                "rows * dim * 4 bytes exceeds MAX_BATCH_TOTAL_BYTES"
                            )
                        );
                        continue;
                    }
                }

                let mut dataset = match build_dataset(&mut rng, rows, dim, batch) {
                    Ok(d) => d,
                    Err(msg) => {
                        println!("{}", format_skip_line(rows, dim, batch, k, &msg));
                        continue;
                    }
                };
                // Issue #540: opt-in 時のみクエリ成分を f16 厳密往復可能な値へ
                // 丸め、`select_dot_shader` の条件 5（クエリの f16 精度損失）で
                // 常駐行 B が unpack 版へ縮退しないようにする。常駐行そのもの
                // （`dataset.vectors`）は既に f16 パック常駐（`ResidentMatrix`）
                // 経由で量子化済みのため丸め不要——ここで丸めるのはクエリのみ。
                if config.query_f16_exact {
                    for q in &mut dataset.queries {
                        for v in q.iter_mut() {
                            *v = round_to_f16_exact(*v);
                        }
                    }
                }

                let batch_metric = config.metric.to_batch_metric();
                let cpu_matrix = match ResidentMatrix::build_with_metric(
                    &dataset.ids,
                    &dataset.tenant_ids,
                    &dataset.visibilities,
                    dim,
                    &dataset.vectors,
                    batch_metric,
                ) {
                    Ok(m) => m,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("cpu resident matrix build failed: {e}")
                            )
                        );
                        continue;
                    }
                };
                let cpu_engine = BatchEngine::new(cpu_matrix);

                let f16_matrix = match ResidentMatrix::build_with_metric(
                    &dataset.ids,
                    &dataset.tenant_ids,
                    &dataset.visibilities,
                    dim,
                    &dataset.vectors,
                    batch_metric,
                ) {
                    Ok(m) => m,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("gpu f16 resident matrix build failed: {e}")
                            )
                        );
                        continue;
                    }
                };
                let gpu_f16 = match GpuBatchBackend::try_new(f16_matrix) {
                    Ok(b) => b,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(Some((rows, dim, batch, k)), &e.to_string())
                        );
                        continue;
                    }
                };

                let gpu_f32 = match GpuF32ContrastBackend::try_new_with_metric(
                    &dataset.ids,
                    &dataset.tenant_ids,
                    &dataset.visibilities,
                    dim,
                    &dataset.vectors,
                    batch_metric,
                ) {
                    Ok(b) => b,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(Some((rows, dim, batch, k)), &e.to_string())
                        );
                        continue;
                    }
                };

                let queries = batch_queries(&dataset.queries, &ctx, k);

                // 正しさの検証（計測区間外・1 回のみ）: A を厳密対照として B・C
                // の Top-k が同点許容つきで一致することを確認する。
                let cpu_hits = match cpu_engine.batch_search(&queries) {
                    Ok(h) => h,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("cpu batch_search failed: {e}")
                            )
                        );
                        continue;
                    }
                };
                let f16_hits = match gpu_f16.batch_search(&queries) {
                    Ok(h) => h,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("gpu f16 batch_search failed: {e}")
                            )
                        );
                        continue;
                    }
                };
                let f32_hits = match gpu_f32.batch_search(&queries) {
                    Ok(h) => h,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("gpu f32 batch_search failed: {e}")
                            )
                        );
                        continue;
                    }
                };

                if cpu_hits.len() != f16_hits.len() || cpu_hits.len() != f32_hits.len() {
                    println!(
                        "{}",
                        format_unavailable_line(
                            Some((rows, dim, batch, k)),
                            "query count mismatch across backends"
                        )
                    );
                    continue;
                }

                let mut mismatch = 0usize;
                for i in 0..cpu_hits.len() {
                    let baseline = hit_pairs(&cpu_hits[i].hits);
                    mismatch += count_boundary_tolerant_mismatches(
                        &baseline,
                        &hit_pairs(&f16_hits[i].hits),
                    );
                    mismatch += count_boundary_tolerant_mismatches(
                        &baseline,
                        &hit_pairs(&f32_hits[i].hits),
                    );
                }

                let measure_config = match MeasurementConfig::new(
                    WARMUP_ITERATIONS,
                    config.measured_iterations,
                    seed,
                ) {
                    Ok(c) => c,
                    Err(e) => {
                        eprintln!("gpu_scaling_bench: measurement config invalid: {e}");
                        std::process::exit(1);
                    }
                };

                // 各試行の `batch_search` が `Err` を返した場合（事前確認後の GPU 転送
                // 失敗等）は失敗処理の所要時間を正常サンプルに混ぜず、`Fatal` として
                // この規模点の計測失敗へ伝播する（除外・埋め合わせはしない）。
                fn fatal<E>(_: &E) -> TrialFailure {
                    TrialFailure::Fatal
                }
                let cpu_measurement = run_fallible(
                    &measure_config,
                    0,
                    || cpu_engine.batch_search(&queries),
                    fatal,
                )
                .map(|m| m.measurement);
                // 読み戻し統計（Issue #537）: f16/f32 各経路の計測ウィンドウ
                // （warmup + measured）の直前直後で `stats()` を差分取得する。
                // `max_excluded=0` で `fatal` 分類器を渡しているため、`Ok` で
                // 戻る限り warmup・measured とも除外は発生せず、呼び出し回数は
                // `WARMUP_ITERATIONS + config.measured_iterations` に一致する
                // （`FallibleMeasurement::measured_attempts` を経由せず定数から
                // 導出できる）。
                let f16_stats_before = gpu_f16.stats();
                let f16_measurement =
                    run_fallible(&measure_config, 0, || gpu_f16.batch_search(&queries), fatal)
                        .map(|m| m.measurement);
                let f16_stats_after = gpu_f16.stats();
                let f32_stats_before = gpu_f32.stats();
                let f32_measurement =
                    run_fallible(&measure_config, 0, || gpu_f32.batch_search(&queries), fatal)
                        .map(|m| m.measurement);
                let f32_stats_after = gpu_f32.stats();

                let (cpu_measurement, f16_measurement, f32_measurement) =
                    match (cpu_measurement, f16_measurement, f32_measurement) {
                        (Ok(a), Ok(b), Ok(c)) => (a, b, c),
                        (a, b, c) => {
                            let reason = [a.err(), b.err(), c.err()]
                                .into_iter()
                                .flatten()
                                .map(|e| e.to_string())
                                .collect::<Vec<_>>()
                                .join("; ");
                            println!(
                                "{}",
                                format_unavailable_line(
                                    Some((rows, dim, batch, k)),
                                    &format!("measurement failed: {reason}")
                                )
                            );
                            continue;
                        }
                    };

                let cpu_p95 = match p95_from_samples(&cpu_measurement.samples) {
                    Ok(v) => v,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("cpu p95 unavailable: {e}")
                            )
                        );
                        continue;
                    }
                };
                let f16_p95 = match p95_from_samples(&f16_measurement.samples) {
                    Ok(v) => v,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("gpu f16 p95 unavailable: {e}")
                            )
                        );
                        continue;
                    }
                };
                let f32_p95 = match p95_from_samples(&f32_measurement.samples) {
                    Ok(v) => v,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("gpu f32 p95 unavailable: {e}")
                            )
                        );
                        continue;
                    }
                };

                let speedup_f16_p95 = match speedup_ratio(cpu_p95, f16_p95) {
                    Ok(v) => v,
                    Err(e) => {
                        println!(
                            "{}",
                            format_unavailable_line(
                                Some((rows, dim, batch, k)),
                                &format!("speedup ratio unavailable: {e}")
                            )
                        );
                        continue;
                    }
                };

                let batch_u32 = u32::try_from(batch).unwrap_or(1).max(1);
                let result = GpuScalingResult {
                    rows,
                    dim,
                    batch,
                    k,
                    cpu_simd_p50: cpu_measurement.summary.median,
                    cpu_simd_p95: cpu_p95,
                    gpu_f16_p50: f16_measurement.summary.median,
                    gpu_f16_p95: f16_p95,
                    gpu_f32_p50: f32_measurement.summary.median,
                    gpu_f32_p95: f32_p95,
                    per_query_cpu_p50: cpu_measurement.summary.median / batch_u32,
                    per_query_gpu_f16_p50: f16_measurement.summary.median / batch_u32,
                    speedup_f16_p95,
                    mismatch,
                };
                println!("{result}");

                // Issue #2005: f16・f32 経路の Recall@k（同じ metric の CPU-SIMD 経路
                // の id 集合が基準。計測区間外・確定的指標）。算出不能でも
                // 結果行は出力済みのため、警告のみで継続する。
                match (
                    mean_recall_against(&cpu_hits, &f16_hits),
                    mean_recall_against(&cpu_hits, &f32_hits),
                ) {
                    (Ok(f16_recall), Ok(f32_recall)) => println!(
                        "{}",
                        GpuScalingRecallLine {
                            rows,
                            dim,
                            batch,
                            k,
                            metric: config.metric,
                            f16_recall,
                            f32_recall,
                        }
                    ),
                    (a, b) => eprintln!(
                        "gpu_scaling_bench: recall unavailable rows={rows} dim={dim} \
                         batch={batch} k={k}: f16={a:?} f32={b:?}"
                    ),
                }

                // Issue #540 追記（codex-review P2 指摘対応・PR #611）:
                // 同一クエリでの S0 シェーダ単体比較（交絡排除）。
                // `config.shader_ab` は `load_config` が
                // `config.query_f16_exact` 併用・`bench-internals` feature を
                // fail-closed に強制済みのため、ここでは opt-in の有無だけを
                // 見ればよい。
                #[cfg(feature = "bench-internals")]
                if config.shader_ab {
                    match measure_shader_ab(
                        &gpu_f16,
                        &queries,
                        rows,
                        dim,
                        batch,
                        k,
                        config.measured_iterations,
                        seed,
                    ) {
                        Ok(line) => println!("{line}"),
                        Err(msg) => {
                            println!(
                                "{}",
                                format_unavailable_line(Some((rows, dim, batch, k)), &msg)
                            )
                        }
                    }
                }

                // `calls` は `run_fallible` の戻り値を経由せず定数計算する
                // （上記コメント参照）。`saturating_sub` はカウンタが同一
                // ウィンドウ内で単調増加する契約を前提にした防御であり、
                // 逆転時は 0 として「差分不明」を表す（`stats()` の実装が
                // 変わっても本ベンチが panic しないための保険）。
                let calls =
                    u64::from(WARMUP_ITERATIONS) + u64::from(measure_config.measured_iterations());
                let f16_bytes_total = f16_stats_after
                    .readback_bytes
                    .saturating_sub(f16_stats_before.readback_bytes);
                let f32_bytes_total = f32_stats_after
                    .readback_bytes
                    .saturating_sub(f32_stats_before.readback_bytes);
                match (
                    readback_bytes_per_call(f16_bytes_total, calls),
                    readback_bytes_per_call(f32_bytes_total, calls),
                ) {
                    (Ok(f16_per_call), Ok(f32_per_call)) => {
                        let stats_line = GpuScalingStatsLine {
                            rows,
                            dim,
                            batch,
                            k,
                            calls,
                            f16_readback_bytes_total: f16_bytes_total,
                            f16_readback_bytes_per_call: f16_per_call,
                            f16_partial_topk_dispatches: f16_stats_after
                                .partial_topk_dispatches
                                .saturating_sub(f16_stats_before.partial_topk_dispatches),
                            f16_full_readback_dispatches: f16_stats_after
                                .full_readback_dispatches
                                .saturating_sub(f16_stats_before.full_readback_dispatches),
                            f16_full_readback_fallbacks: f16_stats_after
                                .full_readback_fallbacks
                                .saturating_sub(f16_stats_before.full_readback_fallbacks),
                            f16_arith_dispatches: f16_stats_after
                                .f16_arith_dispatches
                                .saturating_sub(f16_stats_before.f16_arith_dispatches),
                            f16_arith_guard_fallbacks: f16_stats_after
                                .f16_arith_guard_fallbacks
                                .saturating_sub(f16_stats_before.f16_arith_guard_fallbacks),
                            f32_readback_bytes_total: f32_bytes_total,
                            f32_readback_bytes_per_call: f32_per_call,
                            f32_partial_topk_dispatches: f32_stats_after
                                .partial_topk_dispatches
                                .saturating_sub(f32_stats_before.partial_topk_dispatches),
                            f32_full_readback_dispatches: f32_stats_after
                                .full_readback_dispatches
                                .saturating_sub(f32_stats_before.full_readback_dispatches),
                            f32_full_readback_fallbacks: f32_stats_after
                                .full_readback_fallbacks
                                .saturating_sub(f32_stats_before.full_readback_fallbacks),
                        };
                        println!("{stats_line}");
                    }
                    (f16_res, f32_res) => {
                        // `calls == 0` は測定条件の誤りだが、`gpu_scaling:`
                        // 結果行は既に出力済みのため本ベンチ自体は継続する
                        // （fail-closed に統計行だけを欠落させる）。
                        eprintln!(
                            "gpu_scaling_bench: readback stats unavailable rows={rows} dim={dim} \
                             batch={batch} k={k}: f16={f16_res:?} f32={f32_res:?}"
                        );
                    }
                }

                // 経路 D（Issue #543）: GPU i8 パック常駐。A/B/C の計測・出力が
                // 終わった後段に追加する——構築・計測いずれの失敗も A/B/C の
                // 結果を失わせない（`format_i8_unavailable_line` を出しこの
                // 規模点の i8 計測だけを飛ばして継続する）。
                measure_i8_scaling_point(
                    &dataset,
                    &ctx,
                    rows,
                    dim,
                    batch,
                    k,
                    config.i8_oversample,
                    &config,
                    &cpu_hits,
                    cpu_p95,
                    f16_p95,
                    seed,
                );

                any_measured = true;
            }
        }
    }

    if !any_measured {
        eprintln!("gpu_scaling_bench: no (rows, dim, batch) combination could be measured");
        std::process::exit(1);
    }
}
