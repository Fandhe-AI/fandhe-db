//! GPU バッチ検索（`engine::gpu_batch`。TASK-128〜130・Issue #178 ポインタ）と
//! CPU-SIMD バッチ経路（`engine::batch_search::BatchEngine`）のどちらが速いかを
//! 規模・バッチサイズごとに実測するための、時間非依存な純関数群。
//!
//! 本モジュールが提供するのは「env 変数からの計測条件パース」「出力行の整形」
//! 「Top-k 結果の同点許容つき不一致検知」に加え（Issue #540）
//! `engine::batch_search::pack_f16x2`/`unpack_f16x2`（純粋な host 側 f16
//! 変換関数。GPU デバイスは経由しない）を借りた [`round_to_f16_exact`] のみで、
//! いずれも GPU デバイスそのものには依存しない（`tests/gpu_scaling_accept.rs`
//! から GPU 非依存で単体検証できる。`bench_engine.rs`・`recall_engine.rs` と
//! 同じ切り分け方針）。
//! GPU バックエンドの構築・計測ループ本体は `benches/gpu_scaling_bench.rs`
//! （手動専用・`harness = false`）が担う。
//!
//! 本ベンチは spec が定める受け入れ基準（CORE-6/CORE-16 等の閾値ゲート）を
//! 持たない情報提供専用の計測ツールである
//! （`hnsw_parallel_build_bench.rs`・`Makefile: bench-hnsw-parallel-build` と
//! 同型の位置づけ）。したがって本モジュールにも判定用の閾値は持たせない。

use std::fmt;
use std::time::Duration;

use engine::batch_search::{pack_f16x2, unpack_f16x2};

/// 計測条件パース・出力整形いずれかの失敗を表す fail-closed なエラー型。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuScalingError {
    message: String,
}

impl fmt::Display for GpuScalingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for GpuScalingError {}

fn err(message: impl Into<String>) -> GpuScalingError {
    GpuScalingError {
        message: message.into(),
    }
}

/// `BENCH_GPU_SCALING_ROWS`/`BENCH_GPU_SCALING_DIMS`/`BENCH_GPU_SCALING_BATCH`
/// が受け付けるカンマ区切りリストの最大要素数（本ベンチ固有の安全弁。開発者の
/// typo で桁違いの直積が組まれ計測が長時間・大容量アロケーションに陥ることを
/// 防ぐための上限であり、spec の数値基準ではない）。
const MAX_LIST_ENTRIES: usize = 16;

/// 行数（rows）の上限（本ベンチ固有の安全弁）。500,000 行・dim 256 の f32 常駐
/// バッファが約 512 MiB になる規模を上回る値を開発者が誤って指定した場合に、
/// アロケーション前に拒否する。
const MAX_ROWS: usize = 5_000_000;

/// 次元数（dim）の上限（本ベンチ固有の安全弁）。
const MAX_DIM: usize = 8192;

/// バッチサイズの上限（本ベンチ固有の安全弁）。
const MAX_BATCH: usize = 100_000;

/// `BENCH_GPU_SCALING_TOPK` の上限（本ベンチ固有の安全弁）。
const MAX_TOPK: usize = 10_000;

/// `BENCH_GPU_SCALING_ITERS`（計測反復回数）の上限。
/// `harness::protocol::MeasurementConfig::new` 自体が持つ上限
/// （`MAX_ITERATIONS` = 1,000,000）と同じ値を採用し、プロトコル層より緩い
/// 上限を課さない。
const MAX_MEASURED_ITERATIONS: u32 = 1_000_000;

/// `harness::protocol::MeasurementConfig::new` が要求する計測回数の下限
/// （20 回）と同じ値。`BENCH_GPU_SCALING_ITERS` 未満の値は `MeasurementConfig::new`
/// 自体が拒否するが、本モジュールでも早期に同じ理由で拒否し、パース段階と
/// 計測プロトコル段階のエラーメッセージを一致させる。
const MIN_MEASURED_ITERATIONS: u32 = 20;

/// `std::env::var` を fail-closed に読む（`bench_engine.rs::read_env_var` と同型。
/// 非 UTF-8 値〔`NotUnicode`〕を「未設定」へ黙って合流させない）。
pub fn read_env_var(name: &'static str) -> Result<Option<String>, GpuScalingError> {
    match std::env::var(name) {
        Ok(v) => Ok(Some(v)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(err(format!("{name} value is not valid UTF-8")))
        }
    }
}

/// カンマ区切りの正整数リストをパースする（`BENCH_GPU_SCALING_ROWS` 等）。
/// 未設定・空文字列は `default` をそのまま返す。要素は `min..=max` の範囲内かつ
/// 全体で `MAX_LIST_ENTRIES` 件以下でなければならない（超過・範囲外・非数値は
/// すべて fail-closed で拒否する）。
pub fn parse_usize_list(
    raw: Option<&str>,
    default: &[usize],
    min: usize,
    max: usize,
    field_name: &str,
) -> Result<Vec<usize>, GpuScalingError> {
    let trimmed = raw.map(str::trim);
    match trimmed {
        None | Some("") => Ok(default.to_vec()),
        Some(s) => {
            let parts: Vec<&str> = s.split(',').collect();
            if parts.is_empty() || parts.len() > MAX_LIST_ENTRIES {
                return Err(err(format!(
                    "{field_name} must list 1..={MAX_LIST_ENTRIES} comma-separated values (got {})",
                    parts.len()
                )));
            }
            let mut out = Vec::with_capacity(parts.len());
            for part in parts {
                let value: usize = part.trim().parse().map_err(|_| {
                    err(format!(
                        "{field_name} entries must be positive integers (got {part:?})"
                    ))
                })?;
                if value < min || value > max {
                    return Err(err(format!(
                        "{field_name} entries must be in range {min}..={max} (got {value})"
                    )));
                }
                out.push(value);
            }
            Ok(out)
        }
    }
}

/// `BENCH_GPU_SCALING_ROWS` を規模リストへ解決する。
pub fn parse_rows(raw: Option<&str>, default: &[usize]) -> Result<Vec<usize>, GpuScalingError> {
    parse_usize_list(raw, default, 1, MAX_ROWS, "BENCH_GPU_SCALING_ROWS")
}

/// `BENCH_GPU_SCALING_DIMS` を次元リストへ解決する。
pub fn parse_dims(raw: Option<&str>, default: &[usize]) -> Result<Vec<usize>, GpuScalingError> {
    parse_usize_list(raw, default, 1, MAX_DIM, "BENCH_GPU_SCALING_DIMS")
}

/// `BENCH_GPU_SCALING_BATCH` をバッチサイズリストへ解決する。
pub fn parse_batches(raw: Option<&str>, default: &[usize]) -> Result<Vec<usize>, GpuScalingError> {
    parse_usize_list(raw, default, 1, MAX_BATCH, "BENCH_GPU_SCALING_BATCH")
}

/// `BENCH_GPU_SCALING_TOPK`（Top-k）を解決する。未設定・空文字列は `default`。
pub fn parse_top_k(raw: Option<&str>, default: usize) -> Result<usize, GpuScalingError> {
    match raw.map(str::trim) {
        None | Some("") => Ok(default),
        Some(s) => {
            let value: usize = s.parse().map_err(|_| {
                err(format!(
                    "BENCH_GPU_SCALING_TOPK must be a positive integer (got {s:?})"
                ))
            })?;
            if value == 0 || value > MAX_TOPK {
                return Err(err(format!(
                    "BENCH_GPU_SCALING_TOPK must be in range 1..={MAX_TOPK} (got {value})"
                )));
            }
            Ok(value)
        }
    }
}

/// `BENCH_GPU_SCALING_ITERS`（計測反復回数。warmup は固定 20 回で本設定の対象外）を
/// 解決する。未設定・空文字列は `default`。
pub fn parse_measured_iterations(raw: Option<&str>, default: u32) -> Result<u32, GpuScalingError> {
    match raw.map(str::trim) {
        None | Some("") => Ok(default),
        Some(s) => {
            let value: u32 = s.parse().map_err(|_| {
                err(format!(
                    "BENCH_GPU_SCALING_ITERS must be a positive integer (got {s:?})"
                ))
            })?;
            if !(MIN_MEASURED_ITERATIONS..=MAX_MEASURED_ITERATIONS).contains(&value) {
                return Err(err(format!(
                    "BENCH_GPU_SCALING_ITERS must be in range {MIN_MEASURED_ITERATIONS}..={MAX_MEASURED_ITERATIONS} \
                     (got {value})"
                )));
            }
            Ok(value)
        }
    }
}

/// `BENCH_GPU_SCALING_QUERY_F16_EXACT`（Issue #540。PR #591・Issue #539 の
/// f16 算術版 S0 シェーダ〔`GpuDotShaderKind::F16Arith`〕は
/// `select_dot_shader` の条件 5（クエリ成分が f16 へ厳密往復できない場合は
/// 縮退）により、本ベンチの既定クエリ生成〔`DeterministicRng::next_vector`。
/// 任意精度 f32〕をそのまま使うと常に unpack 版へ縮退し、before/after を
/// そのまま比較しても unpack 同士の比較にしかならない。この opt-in を有効化
/// すると [`round_to_f16_exact`] でクエリ成分を f16 厳密往復可能な値へ丸め、
/// f16 算術版シェーダが実際に選ばれる条件を満たす。未設定・空文字列は無効
/// （既定挙動を変えない）。`1` のみ有効値として受理し、それ以外は fail-closed
/// で拒否する（本ベンチ固有の安全弁。他の bool 系 env と同じ厳格パース方針）。
pub fn parse_query_f16_exact(raw: Option<&str>) -> Result<bool, GpuScalingError> {
    match raw.map(str::trim) {
        None | Some("") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => Err(err(format!(
            "BENCH_GPU_SCALING_QUERY_F16_EXACT must be unset or \"1\" (got {other:?})"
        ))),
    }
}

/// `BENCH_GPU_SCALING_SHADER_AB`（Issue #540・codex-review P2 指摘対応
/// 〔PR #611〕）: 既存の [`parse_query_f16_exact`] opt-in は before/after で
/// クエリ集合そのものを変える（未丸め＝unpack 版縮退／丸め済み＝f16 算術版
/// 選択）ため、"unpack 版 vs f16 算術版" の比較が「シェーダの違い」と
/// 「クエリの違い」の 2 要因を同時に動かす交絡を含んでいた
/// （`docs/design/gpu-batch-f16-arith.md` §8.3 参照）。本 opt-in を有効化すると
/// `gpu_scaling_bench.rs`（`bench-internals` feature 必須）が同一の f16 厳密
/// 往復済みクエリに対し `GpuBatchBackend::batch_search_with_options_for_tests`
/// （[`engine::gpu_batch::GpuSearchTestOptions::dot_shader`]）で S0 シェーダ選択を
/// `Unpack`／`F16Arith` へ交互に強制し、クエリを固定したままシェーダ単体の
/// 効果を計測する（`gpu_scaling_shader_ab:` 行）。[`parse_query_f16_exact`] の
/// opt-in と同時に有効化する契約（`gpu_scaling_bench.rs` 側が fail-closed に
/// 強制する。クエリが f16 厳密往復可能でなければ `F16Arith` 強制は
/// `select_dot_shader` の条件 5 で必ず拒否されるため）。未設定・空文字列は無効。
pub fn parse_shader_ab(raw: Option<&str>) -> Result<bool, GpuScalingError> {
    match raw.map(str::trim) {
        None | Some("") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => Err(err(format!(
            "BENCH_GPU_SCALING_SHADER_AB must be unset or \"1\" (got {other:?})"
        ))),
    }
}

/// クエリ成分 1 個を f16 へ厳密往復可能な値へ丸める（[`parse_query_f16_exact`]
/// の opt-in が有効なときのみ [`gpu_scaling_bench`] から呼ばれる）。
/// `engine::batch_search::pack_f16x2`/`unpack_f16x2`
/// （`crates/engine/src/f16.rs` 実装への薄いラッパ。round-to-nearest-even）で
/// 実際に f16 へ往復させてから返すため、`gpu_batch.rs::f16_round_trip_exact`
/// の判定基準と丸め結果が完全に一致する（`tests/gpu_batch.rs::
/// next_f32_f16_round_trippable` と同じ手法）。`f32::NAN`/`f32::INFINITY` は
/// クエリ生成（`DeterministicRng::next_vector`）が返さない値域のため、
/// 呼び出し元はそれらを渡さない契約とする。
pub fn round_to_f16_exact(v: f32) -> f32 {
    let (rounded, _) = unpack_f16x2(pack_f16x2(v, v));
    rounded
}

/// 1 つの (rows, dim, batch) 構成に対する実測結果（出力整形の入力）。
#[derive(Debug, Clone, Copy)]
pub struct GpuScalingResult {
    pub rows: usize,
    pub dim: usize,
    pub batch: usize,
    pub k: usize,
    pub cpu_simd_p50: Duration,
    pub cpu_simd_p95: Duration,
    pub gpu_f16_p50: Duration,
    pub gpu_f16_p95: Duration,
    pub gpu_f32_p50: Duration,
    pub gpu_f32_p95: Duration,
    /// 1 クエリあたりの CPU-SIMD 中央値所要時間（`cpu_simd_p50 / batch`）。
    pub per_query_cpu_p50: Duration,
    /// 1 クエリあたりの GPU f16 常駐経路の中央値所要時間（`gpu_f16_p50 / batch`）。
    pub per_query_gpu_f16_p50: Duration,
    /// GPU f16 常駐経路の p95 短縮率（`cpu_simd_p95 / gpu_f16_p95`）。
    /// 1.0 を上回るほど GPU 側が高速。
    pub speedup_f16_p95: f64,
    /// A（CPU-SIMD）を厳密対照としたときの、B（GPU f16）・C（GPU f32）双方の
    /// Top-k 結果の同点許容つき不一致件数の合計（全クエリ分。0 が期待値）。
    pub mismatch: usize,
}

impl fmt::Display for GpuScalingResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "gpu_scaling: rows={} dim={} batch={} k={} cpu_simd_p50={}us cpu_simd_p95={}us \
             gpu_f16_p50={}us gpu_f16_p95={}us gpu_f32_p50={}us gpu_f32_p95={}us \
             per_query_cpu_p50={}us per_query_gpu_f16_p50={}us speedup_f16_p95={:.2}x mismatch={}",
            self.rows,
            self.dim,
            self.batch,
            self.k,
            self.cpu_simd_p50.as_micros(),
            self.cpu_simd_p95.as_micros(),
            self.gpu_f16_p50.as_micros(),
            self.gpu_f16_p95.as_micros(),
            self.gpu_f32_p50.as_micros(),
            self.gpu_f32_p95.as_micros(),
            self.per_query_cpu_p50.as_micros(),
            self.per_query_gpu_f16_p50.as_micros(),
            self.speedup_f16_p95,
            self.mismatch,
        )
    }
}

/// `cpu_p95`/`gpu_f16_p95` から短縮率を算出する。`gpu_f16_p95` が 0 の場合
/// （実測上ほぼ起こらないが計測分解能未満に丸まった場合の防御）は fail-closed に
/// `Err` を返す（NaN/inf を出力へ混入させない）。
pub fn speedup_ratio(cpu_p95: Duration, gpu_f16_p95: Duration) -> Result<f64, GpuScalingError> {
    if gpu_f16_p95.is_zero() {
        return Err(err("gpu_f16_p95 is zero; cannot compute speedup ratio"));
    }
    Ok(cpu_p95.as_secs_f64() / gpu_f16_p95.as_secs_f64())
}

/// (rows, dim, batch) の組が測定量上限（`engine::batch_search::MAX_BATCH_WORK`
/// 相当）を超えるため計測をスキップした、という情報行を整形する。
pub fn format_skip_line(rows: usize, dim: usize, batch: usize, k: usize, reason: &str) -> String {
    format!("gpu_scaling: skip rows={rows} dim={dim} batch={batch} k={k} reason=\"{reason}\"")
}

/// GPU バックエンドが利用不能だった（構築失敗・デバイス未検出等）ことを示す行を
/// 整形する。`combo` が `None` の場合はプロセス全体としてこのベンチが GPU を
/// 一切使えないことを示す（起動直後の疎通確認失敗）。
pub fn format_unavailable_line(
    combo: Option<(usize, usize, usize, usize)>,
    reason: &str,
) -> String {
    match combo {
        Some((rows, dim, batch, k)) => format!(
            "gpu_scaling: not measurable rows={rows} dim={dim} batch={batch} k={k} reason=\"{reason}\""
        ),
        None => format!("gpu_scaling: gpu unavailable ({reason})"),
    }
}

/// A（対照。CPU-SIMD 経路の厳密 Top-k）を基準に、B/C（GPU 経路）1 クエリ分の
/// Top-k 結果が「A の id 集合に含まれる」か「A の k 位スコア（`baseline` 中の
/// 最小スコア）以上である」かのいずれかを満たすかを確認し、いずれも満たさない
/// 候補の件数（不一致件数）を返す。
///
/// `baseline`・`candidate` は `(id, score)` の組の列（`SearchHit` から
/// テナント情報を落として比較する。単一テナント合成データセットのみを
/// 対象とするベンチのため、`tenant_id` の突き合わせは不要——呼び出し元
/// `gpu_scaling_bench.rs` 側で保証する）。`baseline` が空の場合、`candidate` も
/// 空であれば不一致 0、そうでなければ `candidate` の全件を不一致として数える
/// （比較対象を持たない候補はすべて疑わしいとみなす fail-closed 側の扱い）。
/// `candidate` が `baseline` より短い場合は欠落件数を、同一 id の重複返却は
/// その重複件数を、境界スコアより高い baseline の id が `candidate` に無い場合は
/// その件数を、`candidate` が `baseline` より長い場合はその超過件数を、それぞれ
/// 不一致に加算する（同点による置換は境界上の結果に限る。件数超過は境界同点の
/// 水増しであっても Top-k の契約〔`baseline.len()` 件を超えない〕に反するため
/// 許容しない。codex-review P2 指摘: 超過分のみでは
/// `baseline=[(1,3),(2,2),(3,1)]`・`candidate=baseline + [(4,1)]` が不一致 0 に
/// なっていた）。
pub fn count_boundary_tolerant_mismatches(
    baseline: &[(u64, f32)],
    candidate: &[(u64, f32)],
) -> usize {
    if baseline.is_empty() {
        return candidate.len();
    }
    let boundary_score = baseline
        .iter()
        .map(|(_, score)| *score)
        .fold(f32::INFINITY, f32::min);
    let baseline_ids: std::collections::HashSet<u64> = baseline.iter().map(|(id, _)| *id).collect();
    let extra = candidate
        .iter()
        .filter(|(id, score)| !baseline_ids.contains(id) && *score < boundary_score)
        .count();
    // 候補が基準より少ない（Top-k が欠けている・空である）場合、その欠落分も
    // 不一致として数える。余分な候補だけを数えると GPU 経路が結果を取りこぼした
    // ときに `mismatch=0` と報告してしまう（fail-closed）。
    let missing = baseline.len().saturating_sub(candidate.len());
    // 同一 id の重複返却も不一致（Top-k は id の集合として一意であるべき）。
    let mut seen = std::collections::HashSet::with_capacity(candidate.len());
    let duplicates = candidate.iter().filter(|(id, _)| !seen.insert(*id)).count();
    // 同点による置換は境界スコア上の結果に限る。境界より高いスコアを持つ baseline の
    // id は必ず candidate に含まれていなければならず、欠けていれば不一致に数える
    // （件数差だけでは、境界同点の候補が上位 id を置き換えた取りこぼしを見逃す）。
    let candidate_ids: std::collections::HashSet<u64> =
        candidate.iter().map(|(id, _)| *id).collect();
    let dropped_above_boundary = baseline
        .iter()
        .filter(|(id, score)| *score > boundary_score && !candidate_ids.contains(id))
        .count();
    // 候補が短い場合の欠落件数と、境界より上位の id の欠落は同じ取りこぼしを別の
    // 側面から数えている（空の候補では両方が計上される）ため、二重計上せず大きい方を
    // 採る。
    //
    // `candidate` が `baseline` より長い場合の超過件数。境界同点の水増しは
    // `extra`（スコアが境界未満）でも `duplicates` でも捕捉できないため独立に
    // 加算するが、超過分のうち既に `extra`・`duplicates` として数えた件数は
    // 差し引く（Bugbot 指摘: 境界未満の余分な 1 件が `extra` と `excess` で
    // 二重計上されていた）。
    let excess = candidate
        .len()
        .saturating_sub(baseline.len())
        .saturating_sub(extra + duplicates);
    extra + duplicates + missing.max(dropped_above_boundary) + excess
}

// ---------------------------------------------------------------------
// 読み戻しバイト数の前後比較（Issue #537。TASK-128〜130・CORE-6/CORE-16
// ポインタ）。Issue #536 で `engine::gpu_batch` へ追加された workgroup 内
// 部分 Top-k と統計カウンタ `GpuBatchStatsSnapshot`
// （`partial_topk_dispatches`／`full_readback_dispatches`／
// `full_readback_fallbacks`／`readback_bytes`）を、規模点ごとに 1 回だけ
// 差分取得して出力するための時間非依存な純関数群。
// ---------------------------------------------------------------------

/// `total_bytes` を `calls` で割った、1 回の `batch_search` 呼び出しあたりの
/// 読み戻しバイト数を返す。`calls == 0` は計測条件（`batch_search` を一度も
/// 呼んでいない）の誤りを表すため、無音の 0 除算にせず拒否する
/// （fail-closed。呼び出し元は「算出不能」として扱う）。
pub fn readback_bytes_per_call(total_bytes: u64, calls: u64) -> Result<u64, GpuScalingError> {
    if calls == 0 {
        return Err(err("readback_bytes_per_call: calls must be > 0"));
    }
    Ok(total_bytes / calls)
}

/// Issue #536 適用前（`895e6cd`）の全量 readback 経路が 1 回の `batch_search`
/// で読み戻すバイト数の算出値。旧経路は `scores: array<f32>`
/// （`row_stride * query_count` 要素、単一テナント・全行可視のベンチ条件では
/// `row_stride == rows`）をそのまま読み戻すため `rows * batch * 4` バイトに
/// 一意に定まる（`crates/engine/src/gpu_batch.rs` の
/// `f32_vec_from_ne_bytes`／WGSL `scores: array<f32>` 参照）。旧経路には
/// [`GpuBatchStatsSnapshot`] 相当のカウンタが無いため実測ではなく算出値
/// （`docs/design/gpu-batch-topk.md` の前後比較節で「算出値」と明記して扱う）。
pub fn full_readback_bytes_estimate(rows: usize, batch: usize) -> Result<u64, GpuScalingError> {
    let rows_u64 = u64::try_from(rows)
        .map_err(|_| err("full_readback_bytes_estimate: rows does not fit in u64"))?;
    let batch_u64 = u64::try_from(batch)
        .map_err(|_| err("full_readback_bytes_estimate: batch does not fit in u64"))?;
    rows_u64
        .checked_mul(batch_u64)
        .and_then(|v| v.checked_mul(4))
        .ok_or_else(|| err("full_readback_bytes_estimate: rows * batch * 4 overflows u64"))
}

/// 1 規模点分の読み戻し統計行。`gpu_scaling_stats:` プレフィクスを使い、
/// `scripts/bench_gpu_scaling_ab.sh` の結果行 grep（`^gpu_scaling: rows=`）
/// とは意図的に異なる接頭辞にすることで、既存の A/B 集計スクリプトへ
/// 結果行として誤って取り込まれないようにする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuScalingStatsLine {
    pub rows: usize,
    pub dim: usize,
    pub batch: usize,
    pub k: usize,
    /// 計測ウィンドウ（warmup + measured）中に `batch_search` を呼んだ回数。
    /// `run_fallible` 呼び出し前後で同一の値（`gpu_scaling_bench.rs` が
    /// `max_excluded=0` で呼ぶため warmup・measured とも除外は発生しない）。
    pub calls: u64,
    pub f16_readback_bytes_total: u64,
    pub f16_readback_bytes_per_call: u64,
    pub f16_partial_topk_dispatches: u64,
    pub f16_full_readback_dispatches: u64,
    pub f16_full_readback_fallbacks: u64,
    /// `SHADER_F16` 対応アダプタで f16 算術版 S0 シェーダへ実際に dispatch
    /// された回数（Issue #539・`gpu_batch::GpuBatchStatsSnapshot::
    /// f16_arith_dispatches`）。f32 対照経路（`GpuF32ContrastBackend`）は
    /// Issue #539 の対象外のため常に 0。
    pub f16_arith_dispatches: u64,
    /// f16 算術版パイプラインは使えたがオーバーフローガード不成立により
    /// unpack 版へ縮退した回数（Issue #539・`f16_arith_guard_fallbacks`）。
    pub f16_arith_guard_fallbacks: u64,
    pub f32_readback_bytes_total: u64,
    pub f32_readback_bytes_per_call: u64,
    pub f32_partial_topk_dispatches: u64,
    pub f32_full_readback_dispatches: u64,
    pub f32_full_readback_fallbacks: u64,
}

impl fmt::Display for GpuScalingStatsLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "gpu_scaling_stats: rows={} dim={} batch={} k={} calls={} \
             f16_readback_bytes_total={} f16_readback_bytes_per_call={} \
             f16_partial_topk_dispatches={} f16_full_readback_dispatches={} \
             f16_full_readback_fallbacks={} f16_arith_dispatches={} \
             f16_arith_guard_fallbacks={} f32_readback_bytes_total={} \
             f32_readback_bytes_per_call={} f32_partial_topk_dispatches={} \
             f32_full_readback_dispatches={} f32_full_readback_fallbacks={}",
            self.rows,
            self.dim,
            self.batch,
            self.k,
            self.calls,
            self.f16_readback_bytes_total,
            self.f16_readback_bytes_per_call,
            self.f16_partial_topk_dispatches,
            self.f16_full_readback_dispatches,
            self.f16_full_readback_fallbacks,
            self.f16_arith_dispatches,
            self.f16_arith_guard_fallbacks,
            self.f32_readback_bytes_total,
            self.f32_readback_bytes_per_call,
            self.f32_partial_topk_dispatches,
            self.f32_full_readback_dispatches,
            self.f32_full_readback_fallbacks,
        )
    }
}

// ---------------------------------------------------------------------
// GPU i8 パック常駐経路（`engine::gpu_batch::packed_i8::GpuI8BatchBackend`。
// Issue #542）の前後比較・Recall 影響の記録（Issue #543）。既存
// `gpu_scaling:`/`gpu_scaling_stats:` 行は 1 文字も変更せず、i8 経路は
// 独立した接頭辞（`gpu_scaling_i8:`/`gpu_scaling_i8_stats:`）の追加行として
// 出力する（`scripts/bench_gpu_scaling_ab.sh` の既存 grep・before バイナリとの
// 出力互換を壊さないため）。
// ---------------------------------------------------------------------

/// `BENCH_GPU_SCALING_I8_OVERSAMPLE`（単一値。i8 常駐バックエンドは
/// `GpuI8Options::oversample` を構築時に固定するため 1 プロセス内でスイープ
/// できない——`docs/design/gpu-batch-i8-packed.md`「D9」節参照）を解決する。
/// 未設定・空文字列は `default`。範囲外・非数値は fail-closed で起動を拒否する
/// （`max` は呼び出し元が `packed_i8::MAX_I8_OVERSAMPLE` を渡す。本モジュールは
/// `engine::gpu_batch` の feature 状態に依存させないため定数を直接参照しない）。
pub fn parse_i8_oversample(
    raw: Option<&str>,
    default: usize,
    max: usize,
) -> Result<usize, GpuScalingError> {
    match raw.map(str::trim) {
        None | Some("") => Ok(default),
        Some(s) => {
            let value: usize = s.parse().map_err(|_| {
                err(format!(
                    "BENCH_GPU_SCALING_I8_OVERSAMPLE must be a positive integer (got {s:?})"
                ))
            })?;
            if value == 0 || value > max {
                return Err(err(format!(
                    "BENCH_GPU_SCALING_I8_OVERSAMPLE must be in range 1..={max} (got {value})"
                )));
            }
            Ok(value)
        }
    }
}

/// クエリごとの Recall@k（[`crate::harness::accept::recall_at_k`] 相当の値）の
/// 平均を求める。空列は「1 クエリも計測できていない」計測条件の誤りを表すため
/// 拒否する（NaN を出力へ混入させない fail-closed）。
pub fn mean_recall_at_k(per_query: &[f64]) -> Result<f64, GpuScalingError> {
    if per_query.is_empty() {
        return Err(err("mean_recall_at_k: per_query must not be empty"));
    }
    let sum: f64 = per_query.iter().sum();
    Ok(sum / per_query.len() as f64)
}

/// `total` を `calls` で割った、1 回の `batch_search` 呼び出しあたりの
/// 再スコア候補件数を返す（[`readback_bytes_per_call`] と同型。`calls == 0` は
/// 無音の 0 除算にせず拒否する）。
pub fn rescored_candidates_per_call(total: u64, calls: u64) -> Result<u64, GpuScalingError> {
    if calls == 0 {
        return Err(err("rescored_candidates_per_call: calls must be > 0"));
    }
    Ok(total / calls)
}

/// 1 規模点分の i8 経路実測結果。既存 [`GpuScalingResult`] とは独立の型で、
/// `gpu_scaling_i8:` 接頭辞の行を出力する。
#[derive(Debug, Clone, Copy)]
pub struct GpuScalingI8Result {
    pub rows: usize,
    pub dim: usize,
    pub batch: usize,
    pub k: usize,
    pub oversample: usize,
    pub gpu_i8_p50: Duration,
    pub gpu_i8_p95: Duration,
    /// 1 クエリあたりの GPU i8 経路の中央値所要時間（`gpu_i8_p50 / batch`）。
    pub per_query_gpu_i8_p50: Duration,
    /// CPU-SIMD（A）対照との p95 短縮率。
    pub speedup_i8_vs_cpu_p95: f64,
    /// GPU f16 常駐（B）対照との p95 短縮率。
    pub speedup_i8_vs_f16_p95: f64,
    /// A（CPU-SIMD 厳密対照）に対する i8 経路の同点許容つき不一致件数
    /// （全クエリ分合計）。
    pub i8_mismatch: usize,
    /// A を正解集合としたクエリごとの Recall@k の平均
    /// （[`mean_recall_at_k`]。量子化・候補生成のみの i8 経路がどの程度
    /// 正解集合を再現できているかを示す確定的指標）。
    pub i8_recall_at_k: f64,
}

impl fmt::Display for GpuScalingI8Result {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "gpu_scaling_i8: rows={} dim={} batch={} k={} oversample={} \
             gpu_i8_p50={}us gpu_i8_p95={}us per_query_gpu_i8_p50={}us \
             speedup_i8_vs_cpu_p95={:.2}x speedup_i8_vs_f16_p95={:.2}x \
             i8_mismatch={} i8_recall_at_k={:.4}",
            self.rows,
            self.dim,
            self.batch,
            self.k,
            self.oversample,
            self.gpu_i8_p50.as_micros(),
            self.gpu_i8_p95.as_micros(),
            self.per_query_gpu_i8_p50.as_micros(),
            self.speedup_i8_vs_cpu_p95,
            self.speedup_i8_vs_f16_p95,
            self.i8_mismatch,
            self.i8_recall_at_k,
        )
    }
}

/// i8 バックエンドが利用不能（`try_new`／`batch_search` 失敗等）だった規模点の
/// 情報行。既存 3 経路（`gpu_scaling:`/`gpu_scaling_stats:`）の結果は失わず、
/// この行を追加で出力するだけに留める（呼び出し元の契約）。
pub fn format_i8_unavailable_line(
    rows: usize,
    dim: usize,
    batch: usize,
    k: usize,
    oversample: usize,
    reason: &str,
) -> String {
    format!(
        "gpu_scaling_i8: not measurable rows={rows} dim={dim} batch={batch} k={k} \
         oversample={oversample} reason=\"{reason}\""
    )
}

/// 1 規模点分の i8 経路の読み戻し・再スコア統計行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuScalingI8StatsLine {
    pub rows: usize,
    pub dim: usize,
    pub batch: usize,
    pub k: usize,
    pub oversample: usize,
    pub calls: u64,
    pub readback_bytes_total: u64,
    pub readback_bytes_per_call: u64,
    pub rescored_candidates_total: u64,
    pub rescored_candidates_per_call: u64,
    /// `GpuI8Meta::backend`（`wgpu::Backend`）の `Debug` 整形。
    pub backend: String,
    /// `GpuI8Meta::dot4_impl`（`Dot4I8Impl`）の `Debug` 整形。wgpu 30.0.1 の
    /// 公開 API では native/polyfill を判別できず、常に `Undetermined` になる
    /// （`packed_i8.rs::Dot4I8Impl` ドキュメンテーションコメント参照）。
    pub dot4_impl: String,
    /// `GpuI8BatchBackend::try_new` の所要時間（計測区間外で 1 回測った参考値）。
    pub build_ms: u128,
}

impl fmt::Display for GpuScalingI8StatsLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "gpu_scaling_i8_stats: rows={} dim={} batch={} k={} oversample={} calls={} \
             readback_bytes_total={} readback_bytes_per_call={} \
             rescored_candidates_total={} rescored_candidates_per_call={} \
             backend={} dot4_impl={} build_ms={}",
            self.rows,
            self.dim,
            self.batch,
            self.k,
            self.oversample,
            self.calls,
            self.readback_bytes_total,
            self.readback_bytes_per_call,
            self.rescored_candidates_total,
            self.rescored_candidates_per_call,
            self.backend,
            self.dot4_impl,
            self.build_ms,
        )
    }
}
// ---------------------------------------------------------------------
// metric 別計測（Issue #2005。ポインタ: CORE-6・CORE-16・SQL-33）。
// `BENCH_GPU_SCALING_METRIC`（`gpu_scaling_bench`）・`BENCH_BATCH_METRIC`
// （`batch_bench`）の値解決と、metric を示す追加行の整形を担う。既存行
// （`gpu_scaling:` 等）は 1 文字も変えず、metric は独立した接頭辞の行で出す。
// ---------------------------------------------------------------------

/// ベンチが計測対象にできる距離 metric（allowlist）。`engine::batch_search::
/// BatchMetric` と同じ 3 値で、L1 はバッチ経路の対象外のため持たない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BenchMetric {
    /// 内積（既定。従来の計測構成）。
    InnerProduct,
    /// L2（二乗距離の符号反転）。
    L2,
    /// コサイン（正規化済み内積への帰着）。
    Cosine,
}

impl BenchMetric {
    /// 出力行・env で使うトークン。
    pub fn as_str(self) -> &'static str {
        match self {
            BenchMetric::InnerProduct => "inner_product",
            BenchMetric::L2 => "l2",
            BenchMetric::Cosine => "cosine",
        }
    }

    /// `engine::batch_search::BatchMetric` への写像（CPU・f16・f32 経路の行列構築用）。
    pub fn to_batch_metric(self) -> engine::batch_search::BatchMetric {
        match self {
            BenchMetric::InnerProduct => engine::batch_search::BatchMetric::InnerProduct,
            BenchMetric::L2 => engine::batch_search::BatchMetric::L2,
            BenchMetric::Cosine => engine::batch_search::BatchMetric::Cosine,
        }
    }
}

/// metric 指定 env の値を解決する。未設定・空白のみは内積。許可するのは
/// `inner_product`・`l2`・`cosine` の完全一致（前後空白は trim）のみで、
/// `l1`（バッチ経路の対象外）を含むそれ以外はすべて拒否する（fail-closed）。
pub fn parse_metric(raw: Option<&str>) -> Result<BenchMetric, GpuScalingError> {
    match raw.map(str::trim) {
        None | Some("") | Some("inner_product") => Ok(BenchMetric::InnerProduct),
        Some("l2") => Ok(BenchMetric::L2),
        Some("cosine") => Ok(BenchMetric::Cosine),
        Some("l1") => Err(err(
            "metric l1 is not supported by the batch path (allowed: inner_product, l2, cosine)",
        )),
        Some(other) => Err(err(format!(
            "unknown metric {other:?} (allowed: inner_product, l2, cosine)"
        ))),
    }
}

/// 起動直後に 1 回出す metric 宣言行（`gpu_scaling_metric:`）。
pub fn format_metric_line(metric: BenchMetric) -> String {
    format!("gpu_scaling_metric: metric={}", metric.as_str())
}

/// 規模点ごとの f16・f32 経路の Recall@k 行（`gpu_scaling_recall:`）。基準は
/// 「同じ metric の CPU-SIMD（f16 常駐）経路」の Top-k の id 集合。
#[derive(Debug, Clone, Copy)]
pub struct GpuScalingRecallLine {
    pub rows: usize,
    pub dim: usize,
    pub batch: usize,
    pub k: usize,
    pub metric: BenchMetric,
    pub f16_recall: f64,
    pub f32_recall: f64,
}

impl fmt::Display for GpuScalingRecallLine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "gpu_scaling_recall: rows={} dim={} batch={} k={} metric={} \
             f16_recall={:.6} f32_recall={:.6}",
            self.rows,
            self.dim,
            self.batch,
            self.k,
            self.metric.as_str(),
            self.f16_recall,
            self.f32_recall,
        )
    }
}
