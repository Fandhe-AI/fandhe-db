//! `isa.rs` dot カーネルの複数アキュムレータ化（Issue #365。前提: Issue #362・
//! `docs/design/knn-stage-profile.md`「`dot_lanes` の実アセンブリ確認」節で、
//! AVX2+FMA 環境の `dot_avx2_fma` が単一 FMA 依存チェーンに律速されていることが
//! 判明済み）のマイクロベンチが使う時間非依存ヘルパ。`benches/dot_kernel_bench.rs`
//! （実測。時間依存・`make ci` 対象外）と `tests/dot_kernel_accept.rs`（`#[path]` で
//! 本モジュールを取り込む時間非依存の回帰。`make ci` 対象）の双方から共有される
//! （`harness/mod.rs` 冒頭コメントと同じ取り込み方針）。
//!
//! `engine::isa::dot_scalar` を参照実装として使うため `engine::` に依存する
//! （`harness/hybrid_latency.rs` 冒頭コメントと同じ理由で、`crate::` の自己参照
//! 曖昧性は生じない）。
//!
//! # 暗号用途禁止
//!
//! [`crate::rng::DeterministicRng`] を経由するため非暗号 PRNG である。ベンチ入力
//! 生成専用。

use std::fmt;
use std::time::Duration;

use super::rng::DeterministicRng;

/// [`generate_corpus`] が許容する行数の安全上限（coding-rust.md「無制限確保禁止」。
/// `harness/hybrid_latency.rs::MAX_CORPUS_DOCS_GUARD` と同一方針）。
pub const MAX_CORPUS_ROWS_GUARD: usize = 200_000;

/// [`generate_corpus`] が許容する 1 行あたりの次元数（`dim`）の安全上限。`dim` は
/// 呼び出し元の引数でそれ自体に上限が無いため、[`MAX_CORPUS_ELEMENTS_GUARD`]
/// （`rows * dim` の総量）とは独立に単独でも検証する。本ベンチが使う最大次元
/// （`dot_kernel_bench.rs::DIMS` の 1536）に対し約 10 倍の余裕を持たせた値
/// （codex-review 指摘: `rows * dim` の乗算結果だけを見るガードでは、
/// `rows` が小さいまま `dim` だけ巨大にする入力を単独では弾けなかった）。
pub const MAX_DIM_GUARD: usize = 16_384;

/// [`generate_corpus`] が許容する総要素数（`rows * dim`）の安全上限。`rows` は
/// [`MAX_CORPUS_ROWS_GUARD`]、`dim` は [`MAX_DIM_GUARD`] でそれぞれ個別に上限
/// 検証しているが、両者を掛け合わせた総量そのものにも歯止めを掛けるため、
/// 乗算結果を `Vec::with_capacity` へ渡す前に総要素数として別途上限を課す
/// （coding-rust.md「無制限確保禁止」。乗算は `checked_mul` を使い、
/// オーバーフロー時に `saturating_mul` のような巨大値への丸めで
/// `Vec::with_capacity` が OOM/abort する経路を作らない）。値は本ベンチの実
/// ワークロード（`ARENA_SCALE_ROWS` の 25,000 行 × 最大次元 1536 ≈ 3,840 万要素・
/// 約 147 MiB）に対し約 1.75 倍の余裕を持たせた現実的な固定上限
/// （67,108,864 要素・f32 換算で約 256 MiB）とする（codex-review 指摘:
/// 旧値 `MAX_CORPUS_ROWS_GUARD * 65_536` ≈ 131 億要素・約 52 GiB は
/// `Vec::with_capacity` がプロセスを終了させ得る非現実的な値だった）。
pub const MAX_CORPUS_ELEMENTS_GUARD: usize = 64 * 1024 * 1024;

/// 本モジュールのエラー型。
#[derive(Debug, Clone, PartialEq)]
pub enum DotKernelError {
    /// `GITHUB_ACTIONS` 環境下での実行が拒否された。
    RefusedUnderGitHubActions,
    /// 計測対象の dot 呼び出し回数が 0 のため ns/dot への換算ができない。
    ZeroDots,
    /// [`MAX_CORPUS_ROWS_GUARD`] を超過した。
    CorpusTooLarge,
    /// 実測値が非有限（NaN/inf）だった。
    NonFiniteResult,
    /// スカラー参照実装との数値差が許容差を超えた。
    ToleranceExceeded { actual: f32, expected: f32 },
    /// L2 二乗距離のスカラー参照実装との数値差が許容差を超えた（Issue #1984）。
    L2ToleranceExceeded { actual: f32, expected: f32 },
    /// tail A/B の分岐なし（padded）実装と現行スカラー実装の `to_bits()` が
    /// 一致しなかった（Issue #529。両方式はビット同一である契約
    /// 〔`docs/design/dot-kernel-branchless-tail.md`〕のため、この不一致は
    /// 実測値を出さず即座に拒否すべき破損入力・実装退行を示す）。
    BitMismatch {
        dim: usize,
        row: usize,
        scalar_bits: u32,
        padded_bits: u32,
    },
    /// `BENCH_DOT_KERNEL_TAIL_AB` env の値が固定語彙（未設定・`"0"`・`"1"`）に
    /// 一致しなかった（fail-closed。coding-rust.md「untrusted 入力」の env 版）。
    InvalidEnv {
        name: &'static str,
        reason: &'static str,
    },
    /// 統計算出対象のサンプル列が空だった（[`min_of_samples`]／[`relative_band`]）。
    EmptySamples,
    /// [`relative_band`] の算出時、値列に非有限（NaN/inf）が含まれる、または
    /// 分母（最小値）が 0 以下だった（fail-closed。ゼロ除算・NaN 判定による
    /// 暗黙の fail-open を防ぐ）。
    NonFiniteOrNonPositiveBand,
}

impl fmt::Display for DotKernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DotKernelError::RefusedUnderGitHubActions => write!(
                f,
                "dot_kernel_bench refuses to run under GitHub Actions (GITHUB_ACTIONS is set); \
                 this bench is manual-only and not wired into any workflow"
            ),
            DotKernelError::ZeroDots => write!(f, "cannot compute ns/dot for zero dot calls"),
            DotKernelError::CorpusTooLarge => {
                write!(
                    f,
                    "rows exceeds {MAX_CORPUS_ROWS_GUARD}, dim exceeds {MAX_DIM_GUARD}, \
                     or rows * dim exceeds {MAX_CORPUS_ELEMENTS_GUARD}"
                )
            }
            DotKernelError::NonFiniteResult => write!(f, "measured result is not finite"),
            DotKernelError::ToleranceExceeded { actual, expected } => write!(
                f,
                "dot result outside tolerance: actual={actual} expected={expected}"
            ),
            DotKernelError::L2ToleranceExceeded { actual, expected } => write!(
                f,
                "l2 result outside tolerance: actual={actual} expected={expected}"
            ),
            DotKernelError::BitMismatch {
                dim,
                row,
                scalar_bits,
                padded_bits,
            } => write!(
                f,
                "tail A/B bit mismatch at dim={dim} row={row}: scalar_tail_bits={scalar_bits:#010x} \
                 padded_tail_bits={padded_bits:#010x}"
            ),
            DotKernelError::InvalidEnv { name, reason } => {
                write!(f, "invalid value for {name}: {reason}")
            }
            DotKernelError::EmptySamples => write!(f, "empty sample set"),
            DotKernelError::NonFiniteOrNonPositiveBand => write!(
                f,
                "relative_band input contains a non-finite value or a non-positive minimum"
            ),
        }
    }
}

impl std::error::Error for DotKernelError {}

/// Issue #529 の tail A/B 実測対象 dim（端数長の異なる境界を選定。ステータス行・
/// `docs/design/dot-kernel-branchless-tail.md`「#529 への申し送り」節参照）。
/// dim=768 は AVX2（LANES=8）・AVX-512（LANES=16）いずれでも端数ゼロだが、
/// `padded_tail_sum` は端数が空でも `LANES` 個の零埋め要素の積和を実行するため
/// 「変更を含まない区間」ではなく定数上乗せコストの実測点として意味を持つ。
pub const TAIL_AB_DIMS: [usize; 3] = [100, 129, 768];

/// tail A/B セクション（`BENCH_DOT_KERNEL_TAIL_AB`）の有効・無効状態。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailAbMode {
    /// 従来どおり `label=current` の 10 ステージ・診断 A/B のみを実行する。
    Off,
    /// dim 100／129／768 の tail A/B（`dot_with_scalar_tail` vs
    /// `dot_with_padded_tail`）を追加実行する。
    On,
}

/// `BENCH_DOT_KERNEL_TAIL_AB` env の生値を解釈する純関数。未設定は `Off`、
/// `"0"` は `Off`、`"1"` は `On`、それ以外（空文字含む）は fail-closed に拒否する
/// （coding-rust.md の env 版。本ベンチは opt-in のためデフォルト無効を安全側とする）。
pub fn parse_tail_ab_env(raw: Option<&str>) -> Result<TailAbMode, DotKernelError> {
    match raw {
        None => Ok(TailAbMode::Off),
        Some("0") => Ok(TailAbMode::Off),
        Some("1") => Ok(TailAbMode::On),
        Some(_) => Err(DotKernelError::InvalidEnv {
            name: "BENCH_DOT_KERNEL_TAIL_AB",
            reason: "expected unset, \"0\", or \"1\"",
        }),
    }
}

/// `GITHUB_ACTIONS` 下での実行を拒否する（`harness::hybrid_latency::
/// refuse_under_github_actions` と同一パターン）。
pub fn refuse_under_github_actions(under_github_actions: bool) -> Result<(), DotKernelError> {
    if under_github_actions {
        return Err(DotKernelError::RefusedUnderGitHubActions);
    }
    Ok(())
}

/// 計測対象の作業集合規模。cache 常駐（L1/L2 相当に収まる規模）と arena 規模
/// （実運用のテーブル規模に近い行数）の 2 種を比較する（計画「参考プロトタイプ
/// 実測」節: 複数アキュムレータ化の効果は cache 常駐かつ次元が大きい場合に限られ、
/// DRAM 帯域律速の大規模データでは効果が薄いことを実測で切り分けるため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkingSet {
    /// 作業集合を [`CACHE_RESIDENT_BUDGET_BYTES`] に収める規模。
    CacheResident,
    /// 実運用のテーブル規模に近い arena 規模（固定行数）。
    ArenaScale,
}

/// cache 常駐段の目標作業集合サイズ（バイト）。L1/L2 に概ね収まる規模として
/// 64 KiB を採る（`knn_profile_bench.rs` 等の既存ベンチが特定 CPU の実キャッシュ
/// 容量に依存しない、控えめで移植可能な値を採る方針と同じ）。
pub const CACHE_RESIDENT_BUDGET_BYTES: usize = 64 * 1024;

/// arena 規模段の固定行数（計画「参考プロトタイプ実測」節の実測条件と同一）。
pub const ARENA_SCALE_ROWS: usize = 25_000;

/// `working_set` と `dim` から計測対象の行数を決める純関数。cache 常駐段は
/// [`CACHE_RESIDENT_BUDGET_BYTES`] を `dim * 4`（f32 バイト数）で割った行数
/// （最低 1 行）、arena 規模段は [`ARENA_SCALE_ROWS`] 固定。
///
/// `dim == 0` は cache 常駐段の除算がゼロ除算になるため [`DotKernelError::
/// CorpusTooLarge`] とは異なる用途外の値として扱わず、呼び出し元契約として
/// `dim >= 1` を前提とする（本ベンチの `dims` 定数は常に 1 以上のためこの関数は
/// 内部専用）。
pub fn rows_for(working_set: WorkingSet, dim: usize) -> Result<usize, DotKernelError> {
    let rows = match working_set {
        WorkingSet::CacheResident => {
            let bytes_per_row = dim.saturating_mul(4).max(1);
            (CACHE_RESIDENT_BUDGET_BYTES / bytes_per_row).max(1)
        }
        WorkingSet::ArenaScale => ARENA_SCALE_ROWS,
    };
    if rows > MAX_CORPUS_ROWS_GUARD {
        return Err(DotKernelError::CorpusTooLarge);
    }
    Ok(rows)
}

/// 決定的シードから `rows` 行 × `dim` 次元のコーパス（フラット化済み `f32`
/// ベクトル）を生成する。[`DeterministicRng`] 経由のため同一シードから常に
/// 同一の値を再生成できる。
pub fn generate_corpus(seed: u64, dim: usize, rows: usize) -> Result<Vec<f32>, DotKernelError> {
    if rows > MAX_CORPUS_ROWS_GUARD {
        return Err(DotKernelError::CorpusTooLarge);
    }
    // `dim` は呼び出し元の引数で上限が無いため、`rows * dim` の乗算結果だけでなく
    // `dim` 単独でも [`MAX_DIM_GUARD`] で検証する（`rows` が小さいまま `dim` だけ
    // 巨大にする入力は乗算結果側の上限だけでは弾ける保証が無いため）。
    if dim > MAX_DIM_GUARD {
        return Err(DotKernelError::CorpusTooLarge);
    }
    // `rows * dim` を `checked_mul` で計算し、オーバーフロー（`None`）または
    // [`MAX_CORPUS_ELEMENTS_GUARD`] 超過をここで拒否してから `Vec::with_capacity`
    // へ渡す（`saturating_mul` はオーバーフロー時に `usize::MAX` へ丸まり、巨大
    // capacity 要求で `Vec::with_capacity` が OOM/abort し得るため使わない）。
    let total_elements = rows
        .checked_mul(dim)
        .filter(|&total| total <= MAX_CORPUS_ELEMENTS_GUARD)
        .ok_or(DotKernelError::CorpusTooLarge)?;
    let mut rng = DeterministicRng::new(seed);
    let mut out = Vec::with_capacity(total_elements);
    for _ in 0..rows {
        out.extend_from_slice(&rng.next_vector(dim));
    }
    Ok(out)
}

/// [`generate_corpus`] と系列を分離したクエリベクトル生成（`harness::
/// hybrid_latency::generate_query` と同じくシードへ固定オフセットを加える）。
pub fn generate_query(seed: u64, dim: usize) -> Vec<f32> {
    let mut rng = DeterministicRng::new(seed.wrapping_add(0x1357_9bdf_1357_9bdf));
    rng.next_vector(dim)
}

/// 総 dot 呼び出し回数から ns/dot を換算する。
pub fn ns_per_dot(total: Duration, dots: usize) -> Result<f64, DotKernelError> {
    if dots == 0 {
        return Err(DotKernelError::ZeroDots);
    }
    Ok(total.as_secs_f64() * 1e9 / dots as f64)
}

/// 候補（`candidate_ns`）とベースライン（`baseline_ns`）の比率
/// （`candidate / baseline`。1.0 未満は改善、1.0 超過は悪化）。
pub fn speedup_ratio(baseline_ns: f64, candidate_ns: f64) -> f64 {
    candidate_ns / baseline_ns
}

/// 比率をノイズ帯（`noise_band`。例: 0.05 = ±5%）で分類する結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeClass {
    /// `ratio <= 1.0 - noise_band`。
    Improved,
    /// `1.0 - noise_band < ratio < 1.0 + noise_band`。
    Neutral,
    /// `ratio >= 1.0 + noise_band`。
    Regressed,
}

/// [`speedup_ratio`] の結果をノイズ帯で 3 分類する純関数（決定規則の判定条件を
/// 時間非依存に検証できるようにする）。
pub fn classify_change(ratio: f64, noise_band: f64) -> ChangeClass {
    if ratio <= 1.0 - noise_band {
        ChangeClass::Improved
    } else if ratio >= 1.0 + noise_band {
        ChangeClass::Regressed
    } else {
        ChangeClass::Neutral
    }
}

/// 1 行分の実測結果を人間可読な形へ整形する（stdout 出力用。本ベンチは spec 由来の
/// 閾値を持たない情報提供専用のため、実測値をそのまま出力してよい
/// （`.claude/rules/spec-confidentiality.md` のオーナー判断範囲）。
pub fn render_line(
    label: &str,
    working_set: WorkingSet,
    dim: usize,
    rows: usize,
    median: Duration,
    ns_per_dot: f64,
) -> String {
    let ws = match working_set {
        WorkingSet::CacheResident => "cache_resident",
        WorkingSet::ArenaScale => "arena_scale",
    };
    format!(
        "dot_kernel: label={label} working_set={ws} dim={dim} rows={rows} median_ms={:.3} ns_per_dot={ns_per_dot:.2}",
        median.as_secs_f64() * 1e3
    )
}

/// `actual` がスカラー参照実装（`engine::isa::dot_scalar`）の `expected` と
/// 許容差内で一致するか検証する（`crates/engine/src/isa.rs` の unit テスト
/// `current_dot_matches_scalar_reference_within_tolerance` と同じ許容差式を
/// 再利用し、ベンチ・回帰テストの双方から同一の判定基準で検証できるようにする）。
/// `magnitude` はスカラー参照値の絶対値（許容差算出に使う `dot_scalar(&a,&b).abs()`
/// 相当を呼び出し元が渡す）。
pub fn check_matches_scalar_reference(
    actual: f32,
    expected: f32,
    magnitude: f32,
) -> Result<(), DotKernelError> {
    if !actual.is_finite() {
        return Err(DotKernelError::NonFiniteResult);
    }
    let tolerance = 1e-5 * magnitude.abs().max(1.0) + 1e-4;
    if (actual - expected).abs() > tolerance {
        return Err(DotKernelError::ToleranceExceeded { actual, expected });
    }
    Ok(())
}

/// `actual`（分岐なし tail）が `expected`（現行スカラー tail）と `to_bits()` で
/// 完全一致するか検証する（Issue #529。両方式はビット同一である契約
/// 〔`docs/design/dot-kernel-branchless-tail.md`〕のため許容差を設けない）。
pub fn check_bit_identical(
    dim: usize,
    row: usize,
    scalar_tail: f32,
    padded_tail: f32,
) -> Result<(), DotKernelError> {
    if scalar_tail.to_bits() != padded_tail.to_bits() {
        return Err(DotKernelError::BitMismatch {
            dim,
            row,
            scalar_bits: scalar_tail.to_bits(),
            padded_bits: padded_tail.to_bits(),
        });
    }
    Ok(())
}

/// 所要時間サンプル列の最小値を取る（`stats::Summary` に `min` フィールドが
/// 無いため、`ab::run_ab`／`protocol::run` が返す `Measurement::samples`
/// （生サンプル列）から呼び出し側が算出する。§3 の min-of-N 統計量用）。
pub fn min_of_samples(samples: &[Duration]) -> Result<Duration, DotKernelError> {
    samples
        .iter()
        .copied()
        .min()
        .ok_or(DotKernelError::EmptySamples)
}

/// `values`（同一計測セッションで得た参照区間の run 値列。単位は任意で一貫していれば
/// よい。呼び出し元は通常 ns や µs 換算後の `f64` を渡す）から相対実測帯
/// `(max - min) / min` を算出する（`docs/design/benchmark-judgement-policy.md`
/// §4「実測帯」の定義そのもの）。空入力・非有限値・0 以下の最小値は
/// fail-closed に拒否する（ゼロ除算・NaN 判定による暗黙の fail-open を防ぐ）。
pub fn relative_band(values: &[f64]) -> Result<f64, DotKernelError> {
    if values.is_empty() {
        return Err(DotKernelError::EmptySamples);
    }
    if values.iter().any(|v| !v.is_finite()) {
        return Err(DotKernelError::NonFiniteOrNonPositiveBand);
    }
    // `f64` は `Ord` を実装しないため `min`/`max` の代わりに `fold` で比較する
    // （NaN は上の `is_finite` チェックで既に排除済み）。
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if min <= 0.0 {
        return Err(DotKernelError::NonFiniteOrNonPositiveBand);
    }
    Ok((max - min) / min)
}

/// tail A/B（`label` は `"scalar_tail"`／`"padded_tail"`）の 1 行分を整形する。
/// `render_line` の `dot_kernel: label=current ...` 形式とプレフィックスを変え
/// （`dot_kernel: tail_ab ...`）、`chip.rs::parse_dot_kernel_line`（`label=current`
/// のみを拾う）と衝突しないようにする。
#[allow(clippy::too_many_arguments)]
pub fn render_tail_ab_line(
    label: &str,
    dim: usize,
    rows: usize,
    min: Duration,
    median: Duration,
    ratio_min: f64,
    ratio_median: f64,
    class: ChangeClass,
) -> String {
    format!(
        "dot_kernel: tail_ab label={label} dim={dim} rows={rows} min_us={:.3} median_us={:.3} \
         ratio_min={ratio_min:.4} ratio_median={ratio_median:.4} class={class:?}",
        min.as_secs_f64() * 1e6,
        median.as_secs_f64() * 1e6,
    )
}

/// tail A/B の参照区間（production 経路 `isa::current().dot`）1 行分を整形する。
/// `reference_band`（§4 の実測帯・相対比率）は複数 run を集計した後にのみ確定する
/// ため、1 run 分のこの行には含めない（呼び出し元が run 間で別途集計する）。
pub fn render_tail_ab_reference_line(
    dim: usize,
    rows: usize,
    min: Duration,
    median: Duration,
) -> String {
    format!(
        "dot_kernel: tail_ab_ref dim={dim} rows={rows} min_us={:.3} median_us={:.3}",
        min.as_secs_f64() * 1e6,
        median.as_secs_f64() * 1e6,
    )
}

/// L2 A/B セクション（`BENCH_DOT_KERNEL_L2_AB`。Issue #1984・SQL-33 ポインタ）の
/// 有効・無効状態。`dot_kernel_bench.rs` の `main` が解釈結果で分岐する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum L2AbMode {
    /// 従来の出力のみ。
    Off,
    /// L2 二乗距離カーネルと内積カーネルの交互計測を追加実行する。
    On,
}

/// `BENCH_DOT_KERNEL_L2_AB` env の生値を解釈する純関数。未設定・`"0"` は `Off`、
/// `"1"` は `On`、それ以外（空文字含む）は fail-closed に拒否する。
pub fn parse_l2_ab_env(raw: Option<&str>) -> Result<L2AbMode, DotKernelError> {
    match raw {
        None | Some("0") => Ok(L2AbMode::Off),
        Some("1") => Ok(L2AbMode::On),
        Some(_) => Err(DotKernelError::InvalidEnv {
            name: "BENCH_DOT_KERNEL_L2_AB",
            reason: "expected unset, \"0\", or \"1\"",
        }),
    }
}

/// `actual`（`current().l2_sq`）が `expected`（`l2_sq_scalar`）と許容差内で一致するか
/// 検証する。許容差式は `tests/isa.rs` のディスパッチ照合と同じ
/// `1e-5 * expected + 1e-6`。非有限値は fail-closed に拒否する。
pub fn check_l2_matches_scalar_reference(actual: f32, expected: f32) -> Result<(), DotKernelError> {
    if !actual.is_finite() || !expected.is_finite() {
        return Err(DotKernelError::NonFiniteResult);
    }
    let tolerance = 1e-5 * expected.abs() + 1e-6;
    if (actual - expected).abs() > tolerance {
        return Err(DotKernelError::L2ToleranceExceeded { actual, expected });
    }
    Ok(())
}

/// L2 A/B の 1 区間分の実測値（[`render_l2_ab_line`] の入力）。
#[derive(Debug, Clone, Copy)]
pub struct L2AbRow {
    pub working_set: WorkingSet,
    pub dim: usize,
    pub rows: usize,
    pub dot_min: Duration,
    pub dot_median: Duration,
    pub l2_min: Duration,
    pub l2_median: Duration,
    pub ratio_min: f64,
    pub ratio_median: f64,
}

/// L2 A/B の 1 行分を整形する。プレフィックス `dot_kernel: l2_ab ` は
/// `chip.rs::parse_dot_kernel_line`（`label=current`）・`parse_dot_kernel_diag_line`
/// （`diagnostic_ab`）と衝突しない。
pub fn render_l2_ab_line(r: &L2AbRow) -> String {
    let ws = match r.working_set {
        WorkingSet::CacheResident => "cache_resident",
        WorkingSet::ArenaScale => "arena_scale",
    };
    format!(
        "dot_kernel: l2_ab working_set={ws} dim={} rows={} dot_min_ms={:.3} dot_median_ms={:.3} \
         l2_min_ms={:.3} l2_median_ms={:.3} ratio_min={:.4} ratio_median={:.4}",
        r.dim,
        r.rows,
        r.dot_min.as_secs_f64() * 1e3,
        r.dot_median.as_secs_f64() * 1e3,
        r.l2_min.as_secs_f64() * 1e3,
        r.l2_median.as_secs_f64() * 1e3,
        r.ratio_min,
        r.ratio_median,
    )
}

/// L2 診断 A/B（`l2_sq_scalar` に対する `current().l2_sq` の比。情報提供・合否に
/// 数えない）1 行分を整形する。
pub fn render_l2_diagnostic_line(
    dim: usize,
    rows: usize,
    simd_vs_scalar_ratio: f64,
    class: ChangeClass,
) -> String {
    format!(
        "dot_kernel: l2_diagnostic_ab dim={dim} rows={rows} \
         simd_vs_scalar_ratio={simd_vs_scalar_ratio:.3} class={class:?}"
    )
}
