//! `agg_count`／`rls_isolation`／`vector_knn_where`（`docs/design/crossdb-bench.md`）
//! の段別プロファイル（Issue #464。親 #456・ルート #455）向けの時間非依存ロジック。
//!
//! `scan_stage_profile_bench.rs`（実測本体・時間依存。`required-features =
//! ["bench-internals"]`）と `tests/scan_stage_profile_accept.rs`（`make ci` 対象の
//! 回帰。既定 feature でコンパイル）の双方から `#[path]` で取り込む。実測タイマー
//! （`std::time::Instant`）には依存しない純関数のみを置く（`knn_wire.rs`
//! （Issue #463）・`knn_profile.rs`（Issue #362）と同じ分離方針）。
//!
//! # 背景（測定対象の実行経路）
//!
//! - `agg_count`／`rls_isolation`（`SELECT COUNT(*) FROM docs`）は
//!   `sql/aggregate.rs::execute_aggregate` を通り、`SqlArenaCache` を経由せず
//!   毎クエリ redb を全行走査する。行ループは `redb 走査 → ヘッダデコード →
//!   RLS 判定（`PolicyContext::is_visible` ＋ TABLE-12 キー/ヘッダ tenant 整合検査）
//!   → dim/metadata 借用デコード（`DecodeTier::Fast` でも構造検証は行う。
//!   `docs/design/aggregate-decode-skip.md`）→ `validate_scalar_columns` →
//!   accumulator」の順（`sql/aggregate.rs` 冒頭コメント参照）。crossdb の可視性
//!   モデル（tenant-a 23,000 行 Public・tenant-b 2,000 行 Private。wire セッションは
//!   Public のみ可視）では `agg_count`（tenant-a 接続）と `rls_isolation`
//!   （tenant-b 接続）は同一の `COUNT(*)`＝23,000 を返す同一走査であり、差は
//!   `PolicyContext` のテナント文字列のみ。
//! - `vector_knn_where`（`SELECT id FROM docs WHERE lang = 'ja' ORDER BY embedding
//!   <=> '<vec>' LIMIT 10`）は `sql/exec.rs::execute_statement_with_cache` を通り、
//!   `WHERE` があるため `cache_fast_path_eligible = false`。`SqlArenaCache`
//!   ヒット時は全可視行に対し `on_visible_row`（`row_codec::scan_scalar_columns`
//!   で全列を借用スキャン → `matches_all`）を呼び、一致行の embedding のみを
//!   新規 arena へ複製したうえで `provider.search`（距離＋Top-k）を呼ぶ。
//!
//! # 段（stage）の定義
//!
//! A 系列（`agg_count`／`rls_isolation`。入れ子: A1 ⊆ A2 ⊆ A3 ⊆ A4 ⊆ A5）:
//!
//! | 段 | 内容 |
//! | --- | --- |
//! | A1 `redb_scan` | 生 `redb::Database` 再オープンで行テーブルを per-entry 走査するのみ |
//! | A2 `header_decode` | A1 ＋ ヘッダデコード（[`super::knn_profile::decode_header_reimpl`]） |
//! | A3 `rls_visible` | A2 ＋ `PolicyContext::is_visible`（不可視行は除外）＋ キー/ヘッダ tenant 整合検査 |
//! | A4 `dim_meta_decode` | A3 ＋ dim・metadata 借用デコード（[`decode_dim_and_metadata_reimpl`]） |
//! | A5 `scalar_validate` | A4 ＋ `row_codec::validate_scalar_columns` |
//!
//! W 系列（`vector_knn_where`。入れ子: W1 ⊆ W2 ⊆ W3。W4 は別経路）:
//!
//! | 段 | 内容 |
//! | --- | --- |
//! | W1 `scalar_scan` | 可視行の metadata へ `row_codec::scan_scalar_columns` |
//! | W2 `predicate` | W1 ＋ `lang = 'ja'` 判定（`declarative_filter::matches_all`） |
//! | W3 `arena_copy` | W2 一致行の embedding を連続 `Vec<f32>` へ複製 |
//! | W4 `provider_search` | 一致行のみへ `SearchProvider::search`（k=10） |
//!
//! ドリフト対策・fail-closed 検証・整合性検証は
//! `docs/design/scan-stage-profile.md` を正本とする。
//!
//! `std`・`engine::row_codec`・`engine::policy`・`engine::storage::Visibility` の
//! pub API のみに依存する（`super::knn_profile` は `std` のみ依存のため取り込み可能）。

use std::fmt;
use std::time::Duration;

use engine::declarative_filter::{self, DeclarativeFilter, MetadataFilter};
use engine::policy::PolicyContext;
use engine::row_codec;

use super::knn_profile::{decode_header_reimpl, KnnProfileError};

/// [`decode_dim_and_metadata_reimpl`] が拒否する上限（`storage.rs::MAX_EMBEDDING_DIM`・
/// `MAX_METADATA_LEN` と同種の防御をベンチ内デコード再実装側にも独立に持たせる。
/// `knn_profile.rs::MAX_REIMPL_DIM`／`MAX_REIMPL_METADATA_LEN` と同値）。
pub use super::knn_profile::{MAX_REIMPL_DIM, MAX_REIMPL_METADATA_LEN};

/// 本モジュールのエラー型（`knn_profile.rs::KnnProfileError`・`knn_wire.rs::
/// KnnWireError` と同じ理由で独立させる）。
#[derive(Debug, Clone, PartialEq)]
pub enum ScanStageError {
    /// `GITHUB_ACTIONS` 環境下での実行が拒否された。
    RefusedUnderGitHubActions,
    /// `BENCH_SCAN_PROFILE_ROUNDS` の値が数値として解釈できない、または範囲外だった。
    InvalidRounds(String),
    /// `BENCH_SCAN_PROFILE_SCALE` の値が不正だった。
    InvalidScale(String),
    /// `BENCH_SCAN_PROFILE_SELECTIVITY` の値が不正だった（Issue #653）。
    InvalidSelectivity(String),
    /// ベンチ内デコード再実装が行バイト列を解釈できなかった（破損・レイアウト
    /// ドリフトのいずれか）。
    Codec(String),
    /// 整合性検証（行数・可視行数・COUNT 期待値・id 境界等）が不成立だった。
    ConsistencyViolation(String),
    /// 統計計算対象のサンプル列が空だった。
    EmptySamples,
    /// 比率計算の分母が 0 で NaN／inf 化する状態だった。
    DegenerateRatio(&'static str),
}

impl fmt::Display for ScanStageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ScanStageError::RefusedUnderGitHubActions => write!(
                f,
                "scan_stage_profile_bench refuses to run under GitHub Actions (GITHUB_ACTIONS is set); this bench is manual-only and not wired into any workflow"
            ),
            ScanStageError::InvalidRounds(reason) => {
                write!(f, "invalid BENCH_SCAN_PROFILE_ROUNDS: {reason}")
            }
            ScanStageError::InvalidScale(reason) => {
                write!(f, "invalid BENCH_SCAN_PROFILE_SCALE: {reason}")
            }
            ScanStageError::InvalidSelectivity(reason) => {
                write!(f, "invalid BENCH_SCAN_PROFILE_SELECTIVITY: {reason}")
            }
            ScanStageError::Codec(msg) => write!(f, "row decode failed: {msg}"),
            ScanStageError::ConsistencyViolation(msg) => {
                write!(f, "consistency check failed: {msg}")
            }
            ScanStageError::EmptySamples => write!(f, "empty sample set"),
            ScanStageError::DegenerateRatio(reason) => write!(f, "degenerate ratio: {reason}"),
        }
    }
}

impl std::error::Error for ScanStageError {}

impl From<KnnProfileError> for ScanStageError {
    fn from(e: KnnProfileError) -> Self {
        ScanStageError::Codec(e.to_string())
    }
}

/// `GITHUB_ACTIONS` 下での実行を拒否する（`knn_profile.rs::refuse_under_github_actions`
/// と同一パターン）。
pub fn refuse_under_github_actions(under_github_actions: bool) -> Result<(), ScanStageError> {
    if under_github_actions {
        return Err(ScanStageError::RefusedUnderGitHubActions);
    }
    Ok(())
}

/// ラウンド数の下限・上限・既定値（`knn_wire.rs::{MIN,MAX,DEFAULT}_ROUNDS` と
/// 同値・同じ根拠: `docs/design/benchmark-judgement-policy.md` §3「交互実行
/// min-of-N（N≥5）」）。
pub const MIN_ROUNDS: u32 = 5;
pub const MAX_ROUNDS: u32 = 50;
pub const DEFAULT_ROUNDS: u32 = 5;

/// `BENCH_SCAN_PROFILE_ROUNDS` を fail-closed にパースする
/// （`knn_wire.rs::parse_rounds` と同型）。
pub fn parse_rounds(raw: Option<&str>) -> Result<u32, ScanStageError> {
    let raw = match raw {
        None => return Ok(DEFAULT_ROUNDS),
        Some(raw) => raw,
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_ROUNDS);
    }
    let value: u32 = trimmed
        .parse()
        .map_err(|_| ScanStageError::InvalidRounds(format!("{raw:?} is not a valid u32")))?;
    if value < MIN_ROUNDS {
        return Err(ScanStageError::InvalidRounds(format!(
            "{value} is below the protocol minimum {MIN_ROUNDS}"
        )));
    }
    if value > MAX_ROUNDS {
        return Err(ScanStageError::InvalidRounds(format!(
            "{value} exceeds the protocol maximum {MAX_ROUNDS}"
        )));
    }
    Ok(value)
}

/// 規模倍率の上限（`bench_engine.rs::parse_scale` を薄くラップする。
/// 1 倍＝25,000 行・4 倍＝100,000 行を許容する Issue #464 の受け入れ条件に合わせ
/// 上限を 4 とする）。
pub const MAX_SCALE: u64 = 4;

/// `BENCH_SCAN_PROFILE_SCALE` を fail-closed にパースする
/// （`bench_engine.rs::parse_scale` へ委譲し、エラー型のみ本モジュール向けに
/// 変換する）。
pub fn parse_scale(raw: Option<&str>) -> Result<u64, ScanStageError> {
    super::bench_engine::parse_scale(raw, MAX_SCALE)
        .map_err(|e| ScanStageError::InvalidScale(e.to_string()))
}

/// A4 段（dim・metadata 借用デコード）の再実装本体。[`super::knn_profile::
/// decode_row_reimpl`] と同じ検証順序・エラー条件を踏襲しつつ、embedding の
/// f32 変換（ヒープ確保を伴う `out_embedding.push` ループ）を行わず、embedding
/// バイト範囲の境界検証のみで読み飛ばす（`storage.rs::
/// decode_row_dim_and_metadata_borrowed` が「embedding をデコードせず dim と
/// metadata borrowed slice のみを返す」設計であるのに対応する再実装。当該関数は
/// `pub(crate)` のためベンチから直接呼べない。モジュール冒頭コメント参照）。
///
/// 戻り値は `(dim, metadata)`。`metadata` は `buf` を借用した slice
/// （ヒープアロケーションを伴わない）。
pub fn decode_dim_and_metadata_reimpl(buf: &[u8]) -> Result<(u32, &[u8]), ScanStageError> {
    let (_tenant_id, _is_public, mut offset) = decode_header_reimpl(buf)?;

    let dim_end = offset
        .checked_add(4)
        .ok_or_else(|| ScanStageError::Codec("offset overflow at dim".to_string()))?;
    let dim_bytes = buf
        .get(offset..dim_end)
        .ok_or_else(|| ScanStageError::Codec("truncated at dim".to_string()))?;
    let dim_arr: [u8; 4] = dim_bytes
        .try_into()
        .map_err(|_| ScanStageError::Codec("dim is not 4 bytes".to_string()))?;
    let dim = u32::from_le_bytes(dim_arr);
    if dim > MAX_REIMPL_DIM {
        return Err(ScanStageError::Codec(format!(
            "dim {dim} exceeds reimpl limit {MAX_REIMPL_DIM}"
        )));
    }
    offset = dim_end;

    let embedding_bytes_len = (dim as usize)
        .checked_mul(4)
        .ok_or_else(|| ScanStageError::Codec("embedding byte length overflow".to_string()))?;
    let embedding_end = offset
        .checked_add(embedding_bytes_len)
        .ok_or_else(|| ScanStageError::Codec("offset overflow at embedding".to_string()))?;
    if buf.get(offset..embedding_end).is_none() {
        return Err(ScanStageError::Codec("truncated at embedding".to_string()));
    }
    offset = embedding_end;

    let metadata_len_end = offset
        .checked_add(4)
        .ok_or_else(|| ScanStageError::Codec("offset overflow at metadata_len".to_string()))?;
    let metadata_len_bytes = buf
        .get(offset..metadata_len_end)
        .ok_or_else(|| ScanStageError::Codec("truncated at metadata_len".to_string()))?;
    let metadata_len_arr: [u8; 4] = metadata_len_bytes
        .try_into()
        .map_err(|_| ScanStageError::Codec("metadata_len is not 4 bytes".to_string()))?;
    let metadata_len = u32::from_le_bytes(metadata_len_arr);
    if metadata_len > MAX_REIMPL_METADATA_LEN {
        return Err(ScanStageError::Codec(format!(
            "metadata_len {metadata_len} exceeds reimpl limit {MAX_REIMPL_METADATA_LEN}"
        )));
    }
    offset = metadata_len_end;

    let metadata_end = offset
        .checked_add(metadata_len as usize)
        .ok_or_else(|| ScanStageError::Codec("offset overflow at metadata".to_string()))?;
    let metadata = buf
        .get(offset..metadata_end)
        .ok_or_else(|| ScanStageError::Codec("row buffer truncated at metadata".to_string()))?;
    if metadata_end != buf.len() {
        return Err(ScanStageError::Codec(
            "row buffer has trailing bytes beyond declared metadata length".to_string(),
        ));
    }

    Ok((dim, metadata))
}

/// `storage.rs::verify_row_key_tenant`（`pub(crate)`）の再実装（A3 段。
/// キー側 tenant とヘッダ側 tenant の単純な等値検査で、`pub(crate)` API を
/// 経由しない）。不一致は TABLE-12 の整合性違反として `Err` にする。
pub fn verify_row_key_tenant_reimpl(
    key_tenant: &str,
    header_tenant: &str,
) -> Result<(), ScanStageError> {
    if key_tenant != header_tenant {
        return Err(ScanStageError::Codec(format!(
            "row key tenant {key_tenant:?} does not match header tenant {header_tenant:?}"
        )));
    }
    Ok(())
}

/// `lang = 'ja'` 相当の束縛済みメタデータフィルタを構築する（W2 段。
/// `declarative_filter::DeclarativeFilter::equals` → `bind` の pub API 経由）。
pub fn build_lang_filter(
    schema: &engine::catalog::TableSchema,
    column: &str,
    value: &str,
) -> Result<MetadataFilter, ScanStageError> {
    DeclarativeFilter::equals(column, value)
        .bind(schema)
        .map_err(|e| ScanStageError::Codec(e.to_string()))
}

/// W2 段本体: `scan_scalar_columns` 済みの値へ `filters` を適用する薄いラッパー
/// （`declarative_filter::matches_all` そのもの。呼び出し側の可読性のため
/// 名前だけ用意する）。
pub fn matches_lang_filter(
    filters: &[MetadataFilter],
    scanned: &[Option<row_codec::ScalarRef<'_>>],
) -> bool {
    declarative_filter::matches_all(filters, scanned)
}

/// W1 段本体: `row_codec::scan_scalar_columns` の薄いラッパー（pub API を直接
/// 呼ぶだけだが、呼び出し側の意図を明確にするため名前を用意する）。
pub fn scan_scalar_columns<'a>(
    schema: &'a engine::catalog::TableSchema,
    metadata: &'a [u8],
) -> Result<Vec<Option<row_codec::ScalarRef<'a>>>, ScanStageError> {
    row_codec::scan_scalar_columns(schema, metadata)
        .map_err(|e| ScanStageError::Codec(e.to_string()))
}

/// [`PolicyContext::is_visible`] の薄いラッパー（A3 段の可読性のため）。
pub fn is_visible(
    ctx: &PolicyContext,
    row_tenant: &str,
    row_visibility: engine::storage::Visibility,
) -> bool {
    ctx.is_visible(row_tenant, row_visibility)
}

/// 1 段あたりの ns/行換算（`knn_profile.rs::ns_per_row` と同型）。
pub fn ns_per_row(total: Duration, rows: usize) -> Result<f64, ScanStageError> {
    if rows == 0 {
        return Err(ScanStageError::ConsistencyViolation(
            "cannot compute ns/row for zero rows".to_string(),
        ));
    }
    Ok(total.as_secs_f64() * 1e9 / rows as f64)
}

/// 2 段間の差分を ns/行へ換算する（`knn_profile.rs::stage_diff_ns_per_row` と同型。
/// 入れ子な累積段どうしの比較であり、逆転は測定異常として `Err` を返す）。
pub fn stage_diff_ns_per_row(
    earlier: Duration,
    later: Duration,
    rows: usize,
    earlier_name: &'static str,
    later_name: &'static str,
) -> Result<f64, ScanStageError> {
    let diff = later.checked_sub(earlier).ok_or_else(|| {
        ScanStageError::ConsistencyViolation(format!(
            "stage timings are not monotonically non-decreasing: {earlier_name} > {later_name}"
        ))
    })?;
    ns_per_row(diff, rows)
}

/// 所要時間列の最小値（min-of-N。`knn_wire.rs::min_of` と同型）。
pub fn min_of(samples: &[Duration]) -> Result<Duration, ScanStageError> {
    samples
        .iter()
        .copied()
        .min()
        .ok_or(ScanStageError::EmptySamples)
}

/// 所要時間列の中央値（median-of-N。`knn_wire.rs::median_of` と同型）。
pub fn median_of(samples: &[Duration]) -> Result<Duration, ScanStageError> {
    if samples.is_empty() {
        return Err(ScanStageError::EmptySamples);
    }
    let mut sorted: Vec<Duration> = samples.to_vec();
    sorted.sort_unstable();
    let mid = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        Ok((sorted[mid - 1] + sorted[mid]) / 2)
    } else {
        Ok(sorted[mid])
    }
}

/// 参照区間帯（`knn_wire.rs::reference_band` と同型）: 変更を含まない区間
/// （本ベンチでは距離カーネルのみの R_dot）の複数ラウンド中央値列から
/// `(max - min) / min` を算出する。
pub fn reference_band(round_medians: &[Duration]) -> Result<f64, ScanStageError> {
    let min = min_of(round_medians)?;
    let max = round_medians
        .iter()
        .copied()
        .max()
        .ok_or(ScanStageError::EmptySamples)?;
    if min.is_zero() {
        return Err(ScanStageError::DegenerateRatio(
            "reference_band: min of round medians is zero",
        ));
    }
    Ok((max.as_secs_f64() - min.as_secs_f64()) / min.as_secs_f64())
}

/// 固定 ±5% 帯（`knn_wire.rs::FIXED_BAND_PCT` と同値）。
pub const FIXED_BAND_PCT: f64 = 5.0;

/// ノイズ帯判定（`knn_wire.rs::BandClass`／`classify_against_bands` と同型）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BandClass {
    WithinNoiseBand,
    AboveNoiseBand,
}

impl fmt::Display for BandClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BandClass::WithinNoiseBand => write!(f, "within_noise_band"),
            BandClass::AboveNoiseBand => write!(f, "above_noise_band"),
        }
    }
}

pub fn classify_against_bands(diff_ratio_pct: f64, reference_band_pct: f64) -> BandClass {
    let band = FIXED_BAND_PCT.max(reference_band_pct);
    if diff_ratio_pct.abs() <= band {
        BandClass::WithinNoiseBand
    } else {
        BandClass::AboveNoiseBand
    }
}

/// 対象区間自身の増分率 `|to/from - 1| * 100`（`knn_wire.rs::step_ratio_pct` と同型）。
pub fn step_ratio_pct(from: Duration, to: Duration) -> Result<f64, ScanStageError> {
    if from.is_zero() {
        return Err(ScanStageError::DegenerateRatio(
            "step_ratio_pct: from duration is zero",
        ));
    }
    Ok((to.as_secs_f64() / from.as_secs_f64() - 1.0) * 100.0)
}

/// 走査行数がすべて一致することを検証する（`knn_profile.rs::
/// assert_scan_row_counts_match` と同型）。
pub fn assert_scan_row_counts_match(
    counts: &[(&'static str, usize)],
) -> Result<(), ScanStageError> {
    let Some((_, first)) = counts.first() else {
        return Ok(());
    };
    for (name, count) in counts {
        if count != first {
            return Err(ScanStageError::ConsistencyViolation(format!(
                "row count mismatch at stage {name}: expected {first}, got {count}"
            )));
        }
    }
    Ok(())
}

/// 1 段の実測結果を人間可読な 1 行へ整形する（`knn_profile.rs::render_stage_line`
/// と同型。本ベンチは spec 由来の閾値を持たない情報提供専用のため、実測値を
/// そのまま出力してよい）。
pub fn render_stage_line(name: &str, rows: usize, median: Duration, ns_per_row: f64) -> String {
    format!(
        "stage({name}): rows={rows} median={:.3}ms ns_per_row={ns_per_row:.1}",
        median.as_secs_f64() * 1e3
    )
}

/// 段間差分の結果を 1 行へ整形する（`knn_profile.rs::render_diff_line` と同型）。
pub fn render_diff_line(from: &str, to: &str, diff_ns_per_row: f64) -> String {
    format!("diff({from}->{to}): ns_per_row={diff_ns_per_row:.1}")
}

/// 区分（bucket）1 行の描画（`knn_wire.rs::render_bucket_line` に相当。W 系列の
/// 「どの段が占有率何%か」を示す出力用）。
pub fn render_bucket_line(
    label: &str,
    diff_ns_per_row: f64,
    ratio_pct: f64,
    band: BandClass,
) -> String {
    format!("bucket({label}): ns_per_row={diff_ns_per_row:.1} ratio={ratio_pct:.2}% band={band}")
}

// --- 選択率 opt-in（Issue #653） ---------------------------------------------
//
// `BENCH_SCAN_PROFILE_SELECTIVITY=1/<N>` で `lang = 'ja'` の割当比率を変える。
// crossdb fixture（`lang='ja'` 8,309/25,000 ≒ 33%）に合わせて計測する場合は
// `1/3` を指定する。既定 `1/5` は既存 fixture（`LANGS[(id) % 5]`）とビット同一の
// 割当規則になる（`lang_for_id` のコメント参照）。

/// 選択率分母の下限（1 未満は「常に ja」になり索引の意味が失われるため禁止）。
pub const MIN_SELECTIVITY_DENOMINATOR: u32 = 2;
/// 選択率分母の上限（実装上の目安。過度に細かい選択率は本ベンチの目的
/// （索引経路の内訳把握）に対して意味を持たないため上限を設ける）。
pub const MAX_SELECTIVITY_DENOMINATOR: u32 = 100;
/// 既定分母。crossdb fixture 導入前の既存 `LANGS` 5 値輪番（`ja` ≒ 20%）と
/// ビット同一になる値。
pub const DEFAULT_SELECTIVITY_DENOMINATOR: u32 = 5;

/// `lang_for_id` が `\"ja\"` 以外に割り当てる候補（既存 `LANGS` の非 `ja` 4 値と
/// 同一の並び・同一の値集合）。
pub const OTHER_LANGS: &[&str] = &["en", "fr", "de", "es"];

/// `lang_for_id` が選択対象として扱う言語値（`sql WHERE lang = '<TARGET_LANG>'`
/// で参照する値そのもの）。
pub const TARGET_LANG: &str = "ja";

/// `BENCH_SCAN_PROFILE_SELECTIVITY` を fail-closed にパースする。受理形状は
/// `\"1/<N>\"`（`N` は [`MIN_SELECTIVITY_DENOMINATOR`]..=[`MAX_SELECTIVITY_DENOMINATOR`]
/// の整数）。未設定・空文字は [`DEFAULT_SELECTIVITY_DENOMINATOR`]。
pub fn parse_selectivity(raw: Option<&str>) -> Result<u32, ScanStageError> {
    let raw = match raw {
        None => return Ok(DEFAULT_SELECTIVITY_DENOMINATOR),
        Some(raw) => raw,
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_SELECTIVITY_DENOMINATOR);
    }
    let Some(("1", denominator_raw)) = trimmed.split_once('/') else {
        return Err(ScanStageError::InvalidSelectivity(format!(
            "{raw:?} does not match the required `1/<N>` shape"
        )));
    };
    let denominator: u32 = denominator_raw.parse().map_err(|_| {
        ScanStageError::InvalidSelectivity(format!(
            "{raw:?}: denominator {denominator_raw:?} is not a valid u32"
        ))
    })?;
    if denominator < MIN_SELECTIVITY_DENOMINATOR {
        return Err(ScanStageError::InvalidSelectivity(format!(
            "{raw:?}: denominator {denominator} is below the minimum {MIN_SELECTIVITY_DENOMINATOR}"
        )));
    }
    if denominator > MAX_SELECTIVITY_DENOMINATOR {
        return Err(ScanStageError::InvalidSelectivity(format!(
            "{raw:?}: denominator {denominator} exceeds the maximum {MAX_SELECTIVITY_DENOMINATOR}"
        )));
    }
    Ok(denominator)
}

/// id → `lang` 値の割当規則（シード時・期待値導出の双方が使う単一情報源）。
/// `id % denominator == 0` は常に [`TARGET_LANG`]（`\"ja\"`）、それ以外は
/// [`OTHER_LANGS`] を `((id % denominator) - 1) % OTHER_LANGS.len()` で輪番する。
///
/// `denominator == 5` のとき、既存 fixture の `LANGS[(id as usize) % LANGS.len()]`
/// （`LANGS = [\"ja\",\"en\",\"fr\",\"de\",\"es\"]`）とビット同一になる
/// （`id % 5 == 0` → ja、`1..=4` → en/fr/de/es の順）。
///
/// `denominator` は [`parse_selectivity`] が [`MIN_SELECTIVITY_DENOMINATOR`] 以上
/// であることを検証済みの前提で呼び出す（0 除算はここでは防御しない設計。
/// 呼び出し元が必ず `parse_selectivity` の戻り値を渡す契約）。
pub fn lang_for_id(id: u64, denominator: u32) -> &'static str {
    let denominator = denominator as u64;
    let rem = id % denominator;
    if rem == 0 {
        TARGET_LANG
    } else {
        let idx = ((rem - 1) % OTHER_LANGS.len() as u64) as usize;
        OTHER_LANGS[idx]
    }
}

/// 可視 id 列のうち [`TARGET_LANG`] に一致する件数（[`lang_for_id`] による
/// 独立導出。計測対象コード〔`scan_scalar_columns`／`matches_all`〕を経由しない
/// 整合性検証用の期待値）。
pub fn expected_visible_hits(visible_ids: &[u64], denominator: u32) -> usize {
    visible_ids
        .iter()
        .filter(|id| lang_for_id(**id, denominator) == TARGET_LANG)
        .count()
}

/// SCALAR 事前フィルタ付き DISTANCE の実行経路を切り替える 2 アーム
/// （`sql/scalar_plan.rs::classify_scalar_plan` の分岐に対応）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileArm {
    /// 現行の索引経路（`WHERE <col> = '<value>'`）。`ScalarIndex::resolve_candidates`
    /// による候補削減が発火する（選択度が閾値以下の場合）。
    Index,
    /// `AND vec_norm(embedding) > 0` の（実運用上）恒真な残余述語を付け加え、
    /// `classify_scalar_plan` を `PlainScan` へ縮退させる（Issue #474 以前相当の
    /// 形状。索引を構築済みでも消費しない全可視行走査経路）。`AND 1 = 1` は
    /// 定数畳み込み（Issue #353・`sql/expr_program.rs`）により束縛時点で消去され
    /// `PlainScan` を強制できないため使わない。`vec_norm(embedding)` は
    /// `VectorRef` を参照する残余述語であり `id_predicate_from_expr` が
    /// `None` を返す（`tests/scalar_index_prune.rs::
    /// residual_builtin_expr_never_consumes_index` と同じ形状）。
    Plain,
}

impl fmt::Display for ProfileArm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProfileArm::Index => write!(f, "index"),
            ProfileArm::Plain => write!(f, "plain"),
        }
    }
}

/// アームごとの `WHERE` 節を組み立てる（ベンチ・accept テストの双方が同じ
/// 文字列を使うための単一情報源）。`column`・`value` は定数文字列（決定的 RNG・
/// 固定スキーマ由来）のみを渡す契約で、未検証の外部入力を SQL 文字列へ連結
/// しない。
pub fn where_clause_for_arm(arm: ProfileArm, column: &str, value: &str) -> String {
    match arm {
        ProfileArm::Index => format!("WHERE {column} = '{value}'"),
        ProfileArm::Plain => {
            format!("WHERE {column} = '{value}' AND vec_norm(embedding) > 0")
        }
    }
}

/// 区分（bucket）の e2e 全体に対する占有率（%）。`total` が 0 の場合は
/// [`ScanStageError::DegenerateRatio`]。
pub fn bucket_share_pct(part: Duration, total: Duration) -> Result<f64, ScanStageError> {
    if total.is_zero() {
        return Err(ScanStageError::DegenerateRatio(
            "bucket_share_pct: total duration is zero",
        ));
    }
    Ok(part.as_secs_f64() / total.as_secs_f64() * 100.0)
}

/// 索引経路の区分（bucket）1 行の描画（`us` はマイクロ秒。[`render_bucket_line`]
/// が ns/row 表記なのに対し、本関数は絶対時間 ＋ e2e 比 % を示す）。
pub fn render_bucket_share_line(label: &str, us: f64, pct: f64) -> String {
    format!("bucket_share({label}): us={us:.1} pct_of_e2e={pct:.2}%")
}

/// `scalar_index_cache_stats()` の増分を 1 行へ整形する（非 vacuous 確認用。
/// テナント ID・行 ID は一切含まない集計カウンタのみを扱う）。
#[allow(clippy::too_many_arguments)]
pub fn render_arm_stats_line(
    arm: ProfileArm,
    k: usize,
    index_scans_delta: u64,
    plain_scan_fallbacks_delta: u64,
    builds_delta: u64,
    arena_cache_hits_delta: u64,
) -> String {
    format!(
        "scalar_index({arm},k={k}): index_scans=+{index_scans_delta} plain_scan_fallbacks=+{plain_scan_fallbacks_delta} builds=+{builds_delta} arena_cache_hits=+{arena_cache_hits_delta}"
    )
}

/// `sql::hnsw_cache::HnswIndexCacheStats` の増分から、HNSW opt-in 時の
/// SCALAR 事前フィルタ付き DISTANCE（`Subset` 形状）がどの経路を通ったかを
/// 分類する（Issue #677。`knn_profile_bench.rs::observed_arm_label` と同じ
/// 判定方針の縮小版——本関数は Issue #676 より前から存在するフィールドの
/// 増分のみを受け取るため、`git archive` で書き出した #676 適用前の独立
/// ソースツリーへ本ファイルを overlay してもコンパイル・実行できる）。
///
/// `subset_searches_delta > 0` は ANN 探索が縮退なしで完走したことを表す
/// （`hits_delta` は `FullVisible` 形状専用のため、`Subset` 形状の ANN 完走は
/// ここでのみ捕捉できる）。`plain_scans_delta`／`mask_splits_graph_delta`／
/// `masked_short_delta` はいずれも「plain scan へ縮退した」内訳（互いに排他）
/// だが、Issue #676 が候補 id マスク経路（複製なし）へ委譲する対象という
/// 意味では **同列ではない**（codex-review・Cursor Bugbot 指摘・PR #688）:
/// `plain_scans`／`mask_splits_graph` は `sql::hnsw_cache::
/// resolve_subset_slot_plan` が ANN を試みる**前**（`kept` から subset
/// アリーナを複製する前）に `SubsetSlotPlan::MaskScan` を返す縮退であり、
/// #676 の複製回避がそのまま効く。対して `masked_short` は
/// `sql::hnsw_cache::finish_indexed_search` が ANN 探索を実際に実行した
/// **後**——`sql/exec.rs` の `SubsetSlotPlan::Ann` 分岐で既に
/// `build_from_cached_rls_rows_subset` により subset アリーナを複製した
/// **後**——に結果不足で `full_scan_with_arena` へ縮退する経路であり、
/// この regime では #676 導入前と同じく複製が発生している（複製なし委譲の
/// 証拠としては扱えない）。
pub fn classify_hnsw_subset_regime(
    hits_delta: u64,
    subset_searches_delta: u64,
    plain_scans_delta: u64,
    mask_splits_graph_delta: u64,
    masked_short_delta: u64,
) -> &'static str {
    if subset_searches_delta > 0 {
        "ann_masked"
    } else if plain_scans_delta > 0 {
        "plain_scan_ratio"
    } else if mask_splits_graph_delta > 0 {
        "plain_scan_mask_split"
    } else if masked_short_delta > 0 {
        "plain_scan_masked_short"
    } else if hits_delta > 0 {
        // `Subset` 形状のクエリでは通常到達しない（`hits` は `FullVisible`
        // 形状専用）が、呼び出し元の非 vacuous 検査が上記 4 カウンタと
        // `hits_delta` の少なくとも 1 つの非ゼロを要求するため網羅させる。
        "full_visible_hit (unexpected for a Subset-shaped query)"
    } else {
        "n/a (all counters delta=0)"
    }
}
