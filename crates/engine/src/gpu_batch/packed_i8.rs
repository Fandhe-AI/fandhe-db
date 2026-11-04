//! i8 パック常駐と `dot4I8Packed` シェーダによる opt-in GPU バッチ候補生成
//! バックエンド（Issue #542・親 #541・Phase 5 親 #460・ルート #455）。
//!
//! [`super::GpuBatchBackend`]（f16 2 要素/u32 パック常駐）の行データ移動量を
//! さらに半減させる（1 byte/要素）ため、行単位対称 SQ8 量子化（各行の絶対値
//! 最大からスケールを独立に導出する。D3 改訂）で行を i8 へ落とし 4 要素/u32
//! でパックし、WGSL 組み込み `dot4I8Packed` で整数内積を計算する。親 Issue
//! #541 の制約により**候補生成専用**（opt-in・最終スコアは必ず f32 再計算）
//! とし、[`super::FallbackBatchEngine::build_with_gpu`] の primary へは接続
//! しない（CORE-12「経路を外部から上書きする機構を作らない」と整合。構築は
//! [`GpuI8BatchBackend::try_new`] からのみ）。
//!
//! # 依存 #521 の実装状況（着手前の確認事項）
//!
//! 2026-09-07 時点で #521（対称 SQ8 量子化）は OPEN・実装未着手
//! （`crates/engine/src/` に `Sq8`/`sq8`/量子化のトップレベル共有モジュール
//! が存在しない）。そのため本モジュールは行単位対称 SQ8 エンコーダを自前で
//! 持つ（[`encode_rows`]／[`Sq8RowScales`]）。#521 マージ後はそちらの型へ
//! 統合する（`f16.rs` の前例に倣い、共有層の所有は #521 側に委ねる。詳細は
//! `docs/design/gpu-batch-i8-packed.md` 参照）。
//!
//! # スコア契約（D8: 候補生成のみ・最終スコアは f32 再計算）
//!
//! GPU は i32 整数内積で候補を粗く選び、ホストが `super::ResidentMatrix`
//! （f16 常駐。既存 [`super::GpuBatchBackend`] と同じ真値）から f32 を復号
//! して `kernel::dot` で再計算した値だけを最終スコアとして返す。真値は常に
//! f16 復号 f32 dot であり、i8 量子化誤差は順位の粗い絞り込みにしか影響
//! しない。
//!
//! # metric 対応（Issue #2004・親 #2002・CORE-8・CORE-16・SQL-33）
//!
//! 本経路は候補生成専用のため、metric ごとに変わるのは**ホスト側の候補選出キー**
//! と**最終 f32 再スコア**だけで、シェーダ（整数内積）は変えない。
//!
//! | metric | 候補選出キー（大きいほど近い） | 最終スコア | 対応 |
//! | ---- | ---- | ---- | ---- |
//! | InnerProduct | `Σ·s_i` | `dot` | 対応 |
//! | Cosine | `Σ·s_i / ‖x_i‖` | `-(1 - cos)` | 対応（実測ゲート済み） |
//! | L2 | `2·s_i·s_q·Σ - ‖x_i‖²` | `-l2_sq` | 対応可否は [`i8_supports_metric`] |
//! | L1 | 内積へ分解できない | - | 非対応（`InitFailed`） |
//!
//! 非対応 metric は GPU に触れる前に [`GpuI8BatchBackend::try_new_with_metric`] が
//! `InitFailed` で拒否する（fail-closed。黙って内積で計算しない）。縮退先 CPU 経路の
//! metric 対応は #2003／#2002 の範囲で、本経路は `FallbackBatchEngine` の primary に
//! 接続しない。実測値は `docs/design/gpu-batch-i8-packed.md` §10 参照。
//!
//! # panic を作らない設計
//!
//! `unwrap`/`expect`/添字アクセスは使わない（[`super`] モジュール冒頭コメント
//! と同じ契約）。新規 `unsafe` は追加しない（`isa.rs` 局在の既存契約を維持）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use crate::batch_fallback::{BatchBackend, BatchBackendError, BatchExecError};
use crate::batch_search::{
    try_reserve_exact, BatchHit, BatchQuery, BatchSearchError, ResidentMatrix,
};
use crate::kernel::{
    cosine_distance_from_parts, dot, l2_sq, row_norms_sq, CandidateHit, TopKSelector,
};

use super::{
    block_on_with_device_poll, bytes_of_u32_slice, check_reachable_batch_work,
    create_topk_pipeline, finalize_gpu_hits, gather_reachable_rows, global_context,
    gpu_dispatch_lock, group_queries_by_ctx, validate_batch_queries, wait_and_read_buffer,
    GpuBatchMetric, GpuContext, GpuParams,
};

/// クランプ後の量子化値の絶対値上限（D3: `-128` は使わない。対称範囲で
/// `[-127, 127]` に収め、極値の掛け算がオーバーフロー計算を単純にする）。
const I8_CLAMP_ABS: f32 = 127.0;

/// [`GpuI8Options::oversample`] の既定値（D9）。実装既定値であり spec 由来の
/// 数値ではない。調整は #523／#543 の実測後の申し送り。
pub const DEFAULT_I8_OVERSAMPLE: usize = 4;
/// [`GpuI8Options::oversample`] の上限（D9・CORE-12: 候補選出量を有界にする
/// ための実装既定値）。
pub const MAX_I8_OVERSAMPLE: usize = 32;

/// `MAX_BATCH_DIM`（8,192）× 127 × 127 の理論上限（D6）。i32 に厳密に収まる
/// ことを単体テストで固定する。
#[cfg(test)]
const MAX_I8_DOT_MAGNITUDE: i64 = 8_192 * 127 * 127;

/// WGSL: i8 パック常駐（4 要素/u32・[`pack_i8x4`] と同一のレーン順規約）に
/// 対する 1 クエリ分の整数内積を `dot4I8Packed` で計算する（Issue #542）。
///
/// [`super::DOT_SHADER_WGSL`]（f16 パック・複数クエリタイル）と異なり、本
/// シェーダは 1 dispatch = 1 クエリに単純化している（D11 の申し送り: i8 用
/// workgroup 内 Top-k・複数クエリタイル化は本 Issue の対象外）。bind group
/// layout（バインディング構成・型）は [`super::DOT_SHADER_WGSL`] と同一のため
/// `GpuContext::bind_group_layout` を共用できる（WGSL の要素型 `array<u32>`
/// vs `array<f32>`/`array<i32>` は wgpu のバインドグループレイアウト検証には
/// 現れない。`DOT_SHADER_F32_WGSL` のドキュメンテーションコメントと同じ理由）。
///
/// `params.row_stride`/`params.query_stride` は「1 行・1 クエリあたりの
/// `array<u32>` 要素数」= `dim.div_ceil(4)`（[`row_stride_for_dim`]）。
/// `params.query_count` は本シェーダでは常に 1 だが、`Params` の形は
/// [`super::GpuParams`] と揃えてある（bind group layout 共用のため）。
const DOT_SHADER_I8_WGSL: &str = r#"
struct Params {
    row_stride: u32,
    row_count: u32,
    query_count: u32,
    query_stride: u32,
};

@group(0) @binding(0) var<uniform> params: Params;
@group(0) @binding(1) var<storage, read> packed_rows: array<u32>;
@group(0) @binding(2) var<storage, read> row_ids: array<u32>;
@group(0) @binding(3) var<storage, read> query: array<u32>;
@group(0) @binding(4) var<storage, read_write> scores: array<i32>;

@compute @workgroup_size(256)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let i = gid.x;
    if (i >= params.row_count) {
        return;
    }
    let row = row_ids[i];
    let row_base = row * params.row_stride;

    var acc: i32 = 0;
    var j: u32 = 0u;
    loop {
        if (j >= params.row_stride) {
            break;
        }
        acc = acc + dot4I8Packed(packed_rows[row_base + j], query[j]);
        j = j + 1u;
    }

    scores[i] = acc;
}
"#;

/// 1 行・1 クエリあたりの `array<u32>` 要素数（[`DOT_SHADER_I8_WGSL`] の
/// `row_stride`/`query_stride`。4 レーン/u32・末尾パディングは 0 詰め）。
fn row_stride_for_dim(dim: usize) -> usize {
    dim.div_ceil(4)
}

/// i8 パック常駐の**行単位**対称量子化パラメータ（D3 改訂。codex-review
/// 指摘対応・P0: 当初実装は次元別 `min`/`max`（`ResidentMatrix` 全行・全
/// テナント横断）から `center`/`alpha` を導出しており、他テナントの不可視行
/// の値・存在が量子化スケール経由で候補選出（ひいては検索結果）に影響し
/// うるテナント境界侵害だった。行 `i` 単独から決まるスケール
/// `s_i = max_j(|x_{i,j}|) / 127` へ変更し、**他行（他テナントの不可視行を
/// 含む）の値に一切依存しない**構成にした。`s_i == 0`（零行）は除算せず
/// 量子化値を常に 0 とする。#521 マージ後に共有層へ統合予定
/// （`docs/design/gpu-batch-i8-packed.md`「D3」節参照。#521 自体が次元別
/// min/max 方式を踏襲する場合は同種の境界問題を引き継ぐため、オーナーへの
/// 申し送り事項として同ドキュメントに記録する）。
#[derive(Debug, Clone)]
pub struct Sq8RowScales {
    /// 行 `i` のスケール（`ResidentMatrix` のスロット順と一致）。
    scales: Vec<f32>,
}

impl Sq8RowScales {
    pub fn row_count(&self) -> usize {
        self.scales.len()
    }

    /// 行 `row` のスケール（範囲外は `None`。添字アクセスはしない）。
    pub fn scale(&self, row: usize) -> Option<f32> {
        self.scales.get(row).copied()
    }
}

/// [`encode_rows`]/[`quantize_query`] の失敗理由。テナント情報を含まない
/// （英語メッセージ・security.md 準拠）。
#[derive(Debug, Clone, PartialEq)]
pub enum I8EncodeError {
    /// 次元 0、または `vectors.len() != row_count * dim`。
    InvalidShape,
    /// 非有限（NaN/Inf）な入力値を検出した（fail-closed。量子化しない）。
    NonFinite,
    /// フォールブル確保の失敗（`try_reserve_exact` 相当）。
    AllocationFailed,
}

impl std::fmt::Display for I8EncodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            I8EncodeError::InvalidShape => write!(f, "i8 encode: invalid shape"),
            I8EncodeError::NonFinite => write!(f, "i8 encode: non-finite input"),
            I8EncodeError::AllocationFailed => write!(f, "i8 encode: allocation failed"),
        }
    }
}

/// `[i8; 4]` を 1 u32 へパックする（D4: レーン `j` を bit `8j..8j+7` へ、
/// 下位から詰める規約。`dot4I8Packed` の成分順・既存
/// `batch_search.rs::pack_f16x2` の「下位から」規約と一致）。
fn pack_i8x4(lanes: [i8; 4]) -> u32 {
    let mut out: u32 = 0;
    for (j, &lane) in lanes.iter().enumerate() {
        let byte = lane as u8 as u32;
        out |= byte << (8 * j);
    }
    out
}

/// [`pack_i8x4`] の逆写像（往復・単体テスト用）。
#[cfg(test)]
fn unpack_i8x4(packed: u32) -> [i8; 4] {
    let mut out = [0i8; 4];
    for (j, slot) in out.iter_mut().enumerate() {
        *slot = ((packed >> (8 * j)) & 0xFF) as u8 as i8;
    }
    out
}

/// 行単位対称量子化（D3 改訂）。`vectors` は `row_count * dim` 要素
/// （行優先・f32）。戻り値は `(scales, packed)` で、`packed` は
/// `row_count * row_stride_for_dim(dim)` 要素の行優先パック済み `u32` 列。
/// 行 `i` のスケール `s_i` は行 `i` 自身の成分だけから決まり、他行（他
/// テナントの不可視行を含む）には一切依存しない（P0 修正。モジュール冒頭
/// [`Sq8RowScales`] 参照）。
pub fn encode_rows(
    dim: usize,
    row_count: usize,
    vectors: &[f32],
) -> Result<(Sq8RowScales, Vec<u32>), I8EncodeError> {
    if dim == 0 {
        return Err(I8EncodeError::InvalidShape);
    }
    let expected_len = row_count
        .checked_mul(dim)
        .ok_or(I8EncodeError::InvalidShape)?;
    if vectors.len() != expected_len {
        return Err(I8EncodeError::InvalidShape);
    }
    for &v in vectors {
        if !v.is_finite() {
            return Err(I8EncodeError::NonFinite);
        }
    }

    let row_stride = row_stride_for_dim(dim);
    let packed_len = row_count
        .checked_mul(row_stride)
        .ok_or(I8EncodeError::AllocationFailed)?;

    let mut scales: Vec<f32> = Vec::new();
    scales
        .try_reserve_exact(row_count)
        .map_err(|_| I8EncodeError::AllocationFailed)?;
    let mut packed: Vec<u32> = Vec::new();
    packed
        .try_reserve_exact(packed_len)
        .map_err(|_| I8EncodeError::AllocationFailed)?;

    for row in vectors.chunks(dim) {
        // P1 #4 修正: 量子化の除算（`quantize_scalar`）へは f64 精度の
        // `scale_f64` をそのまま渡す。[`Sq8RowScales`] への格納・後段の
        // `ranking_key` 計算用途にのみ f32 へ丸めた `scale` を使う
        // （[`row_scale_f64`] のドキュメンテーションコメント参照）。
        let scale_f64 = row_scale_f64(row)?;
        scales.push(scale_f64 as f32);

        let mut d = 0usize;
        while d < row_stride.saturating_mul(4) {
            let mut lanes = [0i8; 4];
            for (j, lane) in lanes.iter_mut().enumerate() {
                let idx = d + j;
                let Some(&v) = row.get(idx) else {
                    continue;
                };
                *lane = quantize_scalar(v, scale_f64)?;
            }
            packed.push(pack_i8x4(lanes));
            d += 4;
        }
    }

    Ok((Sq8RowScales { scales }, packed))
}

/// `values` 自身の成分だけから対称量子化スケール `max_j(|v_j|) / 127` を
/// f64 で求める（codex-review 指摘対応・P1 #3・#4: f32 のみだと極端な入力
/// 〔非常に大きい／小さい有限値〕で中間演算がオーバーフローし非有限値を
/// 生みうるため、桁数に余裕のある f64 で計算する。この f64 精度のまま
/// [`quantize_scalar`] の除算まで保持しなければならない——最終結果だけ
/// 呼び出し元で f32 へ丸めると、極小の非零有限入力〔`f32::MIN_POSITIVE`
/// 未満の f32 劣化数域〕でスケールが f32 表現ではゼロへアンダーフローし、
/// 以降の量子化が `scale == 0.0`（本来は「値そのものが全成分ゼロの行」を
/// 表す契約）と誤って一致して非零な行・クエリを全要素 0 へ変換し、対称
/// 量子化の契約に違反したまま候補選出が slot 順（実質ランダム）に縮退
/// してしまう。`values` は呼び出し元で非有限値を検証済みの前提）。
fn row_scale_f64(values: &[f32]) -> Result<f64, I8EncodeError> {
    let mut max_abs: f64 = 0.0;
    for &v in values {
        let a = (v as f64).abs();
        if a > max_abs {
            max_abs = a;
        }
    }
    if max_abs == 0.0 {
        return Ok(0.0);
    }
    let scale = max_abs / (I8_CLAMP_ABS as f64);
    if !scale.is_finite() {
        return Err(I8EncodeError::NonFinite);
    }
    Ok(scale)
}

/// 単一のスカラー値をスケール `scale`（f64。[`row_scale_f64`] の戻り値を
/// そのまま渡す）で量子化し `[-127, 127]` へクランプする（`f64::round` =
/// half away from zero で固定。`scale == 0.0`〔零行〕は除算せず 0 を返す。
/// codex-review 指摘対応・P1 #3: 除算・丸めを f64 で行い中間結果が非有限に
/// なった場合は `NonFinite` を明示的に返すフェイルクローズ。`NaN as i8 == 0`
/// へ暗黙に丸めて有効なスコアを無言で除外することはしない。P1 #4:
/// `scale` を f32 へ丸めてから受け取らないことで、極小の非零スケールが
/// アンダーフローしてゼロ扱いされる不具合を避ける）。
fn quantize_scalar(v: f32, scale: f64) -> Result<i8, I8EncodeError> {
    if scale == 0.0 {
        return Ok(0);
    }
    let raw = (v as f64 / scale).round();
    if !raw.is_finite() {
        return Err(I8EncodeError::NonFinite);
    }
    let clamp_abs = I8_CLAMP_ABS as f64;
    let clamped = raw.clamp(-clamp_abs, clamp_abs);
    Ok(clamped as i8)
}

/// クエリ側の量子化（D5 改訂）。クエリ自身の成分だけから対称スケール
/// `s_q = max_i(|q_i|) / 127` を求め、`qq_i = round(q_i / s_q)` を
/// `[-127, 127]` へクランプする（`s_q == 0` なら全 0）。行側の量子化
/// （[`encode_rows`]）が [`Sq8RowScales`] へ再構成されたことに伴い、
/// クエリの量子化も行パラメータに依存しない自己完結の計算になった
/// （以前の版が計算していた「次元別 `center` との内積」定数項は、行単位
/// スケール方式には対応する概念が無いため消滅した）。
///
/// 戻り値の `f32` は `s_q`（クエリのスケール）。GPU から届く整数内積
/// `Σ qq_i·xq_i` と行スケール `s_i`・`s_q` を掛け合わせると近似内積
/// `s_i * s_q * Σ qq_i·xq_i` になるが、同一クエリ内の候補順位付けでは
/// `s_q` は全候補で共通の正の定数のため、順位付けには `s_i` だけを掛け
/// れば足りる（呼び出し元 [`GpuI8BatchBackend::raw_i32_scores`] 参照）。
pub fn quantize_query(query: &[f32]) -> Result<(f32, Vec<u32>), I8EncodeError> {
    let dim = query.len();
    if dim == 0 {
        return Err(I8EncodeError::InvalidShape);
    }
    for &v in query {
        if !v.is_finite() {
            return Err(I8EncodeError::NonFinite);
        }
    }

    // P1 #4 修正: 量子化の除算へは f64 精度の `s_q_f64` をそのまま渡す。
    // 戻り値（呼び出し元がランキングの定数項として使う）にのみ f32 丸めの
    // `s_q` を使う（[`row_scale_f64`] のドキュメンテーションコメント参照）。
    let s_q_f64 = row_scale_f64(query)?;
    let s_q = s_q_f64 as f32;

    let row_stride = row_stride_for_dim(dim);
    let mut packed: Vec<u32> = Vec::new();
    packed
        .try_reserve_exact(row_stride)
        .map_err(|_| I8EncodeError::AllocationFailed)?;

    let mut d = 0usize;
    while d < row_stride.saturating_mul(4) {
        let mut lanes = [0i8; 4];
        for (j, lane) in lanes.iter_mut().enumerate() {
            let idx = d + j;
            let Some(&v) = query.get(idx) else {
                continue;
            };
            *lane = quantize_scalar(v, s_q_f64)?;
        }
        packed.push(pack_i8x4(lanes));
        d += 4;
    }

    Ok((s_q, packed))
}

/// CPU 参照実装（D7）: [`encode_rows`]/[`quantize_query`] が生成したパック
/// 済み行・クエリの整数内積を計算する。`get()` のみを使い添字アクセスは
/// しない。レーン展開・i32 累積は GPU シェーダ（[`DOT_SHADER_I8_WGSL`]）と
/// 同一の演算順（次元昇順の逐次加算）を踏む。#522（VNNI 実装）はこの参照
/// 実装とビット同一であることを契約とする（Issue #542 実装計画ポインタ）。
pub fn dot_i8_packed_ref(row: &[u32], query: &[u32]) -> i32 {
    let mut acc: i64 = 0;
    let len = row.len().min(query.len());
    for j in 0..len {
        let (Some(&r), Some(&q)) = (row.get(j), query.get(j)) else {
            continue;
        };
        let r_lanes = unpack_i8x4_i32(r);
        let q_lanes = unpack_i8x4_i32(q);
        for lane in 0..4 {
            acc += i64::from(r_lanes[lane]) * i64::from(q_lanes[lane]);
        }
    }
    // D6: `MAX_BATCH_DIM(8,192) × 127 × 127 < 2^31` を実装契約として単体テストで
    // 固定しているため、i32 への切り詰めは正常系では常に無損失。防御的に
    // `clamp` で二重に範囲内へ収める（fail-closed。GPU 側 WGSL `i32` 演算も
    // 同じ理論上限の範囲内で動く）。
    acc.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn unpack_i8x4_i32(packed: u32) -> [i32; 4] {
    let mut out = [0i32; 4];
    for (j, slot) in out.iter_mut().enumerate() {
        let byte = ((packed >> (8 * j)) & 0xFF) as u8;
        *slot = i32::from(byte as i8);
    }
    out
}

/// i8 経路が `metric` に対応するか（対応表の単一の判定箇所。`match` は網羅で書き、
/// 新しい metric が増えたらコンパイルエラーで判断を強制する）。
pub(crate) fn i8_supports_metric(metric: GpuBatchMetric) -> bool {
    match metric {
        GpuBatchMetric::InnerProduct => true,
        GpuBatchMetric::Cosine => true,
        GpuBatchMetric::L2 => true,
        GpuBatchMetric::L1 => false,
    }
}

/// 候補選出キーの計算文脈（クエリごとに作る。行側の値は行自身だけから決まり、
/// 他行・他テナントの値には依存しない）。
pub(crate) struct RankingCtx<'a> {
    pub(crate) metric: GpuBatchMetric,
    pub(crate) row_scales: &'a Sq8RowScales,
    /// 行ごとのノルム²。Cosine／L2 のときだけ使う（InnerProduct では空）。
    pub(crate) row_norms_sq: &'a [f32],
    /// [`quantize_query`] が返すクエリのスケール（L2 の加法項と組むため省けない）。
    pub(crate) query_scale: f32,
}

/// `(slot, raw i32 score)` の候補選出キー（大きいほど近い。f64 で返す）。
/// InnerProduct は従来どおり `(score as f32) * scale` を f64 へ広げただけで順序は不変。
/// Cosine はノルムが不正（None・非正・非有限）なら除算せず `-inf`（最下位）にする。
fn ranking_key(slot: u32, score: i32, ctx: &RankingCtx<'_>) -> f64 {
    let scale = ctx.row_scales.scale(slot as usize).unwrap_or(0.0);
    match ctx.metric {
        GpuBatchMetric::InnerProduct => f64::from((score as f32) * scale),
        GpuBatchMetric::Cosine => {
            let norm_sq = ctx.row_norms_sq.get(slot as usize).copied().unwrap_or(0.0);
            if !norm_sq.is_finite() || norm_sq <= 0.0 {
                return f64::NEG_INFINITY;
            }
            let key = f64::from(score) * f64::from(scale) / f64::from(norm_sq).sqrt();
            if key.is_finite() {
                key
            } else {
                f64::NEG_INFINITY
            }
        }
        GpuBatchMetric::L2 => {
            let norm_sq = ctx
                .row_norms_sq
                .get(slot as usize)
                .copied()
                .unwrap_or(f32::NAN);
            let key = 2.0 * f64::from(scale) * f64::from(ctx.query_scale) * f64::from(score)
                - f64::from(norm_sq);
            if key.is_finite() {
                key
            } else {
                f64::NEG_INFINITY
            }
        }
        // 非対応（構築時に拒否済み）。到達しても最下位へ沈める fail-closed。
        GpuBatchMetric::L1 => f64::NEG_INFINITY,
    }
}

/// 最終 f32 再スコア（大きいほど近い）。非有限（ゼロノルム等）は呼び出し側が除外する。
/// InnerProduct は従来どおり `dot` のまま。
pub(crate) fn final_score(
    metric: GpuBatchMetric,
    query: &[f32],
    row: &[f32],
    query_norm_sq: f32,
) -> f32 {
    match metric {
        GpuBatchMetric::InnerProduct => dot(query, row),
        GpuBatchMetric::Cosine => {
            let d = cosine_distance_from_parts(dot(query, row), query_norm_sq, dot(row, row));
            -(d as f32)
        }
        GpuBatchMetric::L2 => -l2_sq(query, row),
        // 非対応: NaN を返し呼び出し側の非有限除外に落とす（fail-closed）。
        GpuBatchMetric::L1 => f32::NAN,
    }
}

/// [`GpuI8BatchBackend::try_new`] のオプション（D9・CORE-12: 環境変数では
/// 読まず、コンストラクタ引数のみで受ける）。
#[derive(Debug, Clone, Copy)]
pub struct GpuI8Options {
    /// 候補生成の広さ: `k' = min(reachable_rows, k * oversample)` 件を
    /// i32 スコア降順で GPU 側から選び、f32 再スコア後に上位 `k` 件へ
    /// 切り詰める。`0` または [`MAX_I8_OVERSAMPLE`] 超は
    /// [`BatchBackendError::InitFailed`]。
    pub oversample: usize,
}

impl Default for GpuI8Options {
    fn default() -> Self {
        Self {
            oversample: DEFAULT_I8_OVERSAMPLE,
        }
    }
}

/// 専用整数内積命令（`OpSDot`/`dot4add_i8packed`/`packed_char4`）と polyfill
/// のどちらが選ばれたかの判別結果（D12）。
///
/// wgpu 30.0.1 の公開 API では判別できない（`wgpu-types` に該当 feature 定数が
/// 無く、判別に使える唯一の経路 `Adapter::as_hal` は `unsafe` かつ本リポの
/// `unsafe` 原則禁止に反する。`docs/design/gpu-batch-i8-packed.md`「D12」節
/// 参照）ため、本実装では常に [`Dot4I8Impl::Undetermined`] を返す。将来 wgpu が
/// 判別可能な feature を公開した場合の実装は申し送りとする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dot4I8Impl {
    Native,
    Polyfill,
    Undetermined,
}

/// [`GpuI8BatchBackend::meta`] が返す観測専用メタ情報（D12）。テナント・
/// 行数等は含まない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GpuI8Meta {
    pub backend: wgpu::Backend,
    pub dot4_impl: Dot4I8Impl,
}

/// [`GpuI8BatchBackend::stats`] が返す観測専用カウンタ（D13）。
#[derive(Debug, Default)]
struct GpuI8Stats {
    dispatches: AtomicU64,
    readback_bytes: AtomicU64,
    rescored_candidates: AtomicU64,
}

impl GpuI8Stats {
    fn snapshot(&self) -> GpuI8BatchStatsSnapshot {
        GpuI8BatchStatsSnapshot {
            dispatches: self.dispatches.load(Ordering::Relaxed),
            readback_bytes: self.readback_bytes.load(Ordering::Relaxed),
            rescored_candidates: self.rescored_candidates.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GpuI8BatchStatsSnapshot {
    pub dispatches: u64,
    pub readback_bytes: u64,
    pub rescored_candidates: u64,
}

/// i8 パック常駐の compute pipeline をプロセス内で 1 回だけ遅延初期化する
/// （[`super::f32_contrast_pipeline`] と同方針。生成失敗もキャッシュし
/// 再試行しない）。bind group layout は本番経路と共用する
/// （[`DOT_SHADER_I8_WGSL`] ドキュメンテーションコメント参照）。
fn i8_pipeline() -> Result<&'static wgpu::ComputePipeline, String> {
    static PIPELINE: OnceLock<Result<wgpu::ComputePipeline, String>> = OnceLock::new();
    let ctx = global_context().as_ref().map_err(String::clone)?;
    let result = PIPELINE.get_or_init(|| {
        let _guard = gpu_dispatch_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        create_topk_pipeline(
            &ctx.device,
            &ctx.bind_group_layout,
            DOT_SHADER_I8_WGSL,
            "i8",
        )
    });
    result.as_ref().map_err(String::clone)
}

/// i8 パック常駐・`dot4I8Packed` による opt-in GPU 候補生成バックエンド
/// （Issue #542）。[`BatchBackend`] を実装するが、`FallbackBatchEngine` の
/// primary へは接続しない（モジュール冒頭コメント参照）。
pub struct GpuI8BatchBackend {
    matrix: ResidentMatrix,
    row_scales: Sq8RowScales,
    metric: GpuBatchMetric,
    row_norms_sq: Vec<f32>,
    row_buffer: wgpu::Buffer,
    row_stride: usize,
    options: GpuI8Options,
    device_lost: std::sync::Arc<std::sync::atomic::AtomicBool>,
    uncaptured_error: std::sync::Arc<std::sync::atomic::AtomicBool>,
    stats: std::sync::Arc<GpuI8Stats>,
    meta: GpuI8Meta,
}

impl GpuI8BatchBackend {
    /// 常駐行列から i8 パック常駐バックエンドを構築する。量子化パラメータ
    /// （次元別 min/max）は `matrix` 自身が保持する全行を
    /// [`ResidentMatrix::row_f32_into`]（`pub(crate)`。Issue #542 で公開）
    /// 経由で f16 往復デコードした値から導出する——別引数で元の f32
    /// ベクトル列を受け取る設計にすると、呼び出し元が渡す `vectors` の行順が
    /// `matrix` のスロット順と食い違った場合にスロット不整合を起こしうる
    /// ため、`matrix` を単一の真値にする（D8 の f32 再スコアが参照する真値
    /// とも一致させる。「読んでから決める」設計判断）。
    pub fn try_new(
        matrix: ResidentMatrix,
        options: GpuI8Options,
    ) -> Result<Self, BatchBackendError> {
        Self::try_new_with_metric(matrix, options, GpuBatchMetric::InnerProduct)
    }

    /// [`Self::try_new`] の metric 指定版（Issue #2004）。metric は構築時に固定する
    /// （CORE-12: 環境変数では読まない）。非対応 metric（[`i8_supports_metric`]）は
    /// GPU に触れる前に `InitFailed` で拒否する。
    pub fn try_new_with_metric(
        matrix: ResidentMatrix,
        options: GpuI8Options,
        metric: GpuBatchMetric,
    ) -> Result<Self, BatchBackendError> {
        if options.oversample == 0 || options.oversample > MAX_I8_OVERSAMPLE {
            return Err(BatchBackendError::InitFailed(
                "i8 oversample out of range".to_string(),
            ));
        }
        // 常駐行列は生の行（内積）で持つ。コサイン用の正規化済み行列（Issue #2003）は i8 側の
        // metric 処理と二重になるため拒否する。GPU コンテキスト取得より前に判定し fail-closed にする。
        if matrix.metric() != crate::batch_search::BatchMetric::InnerProduct {
            return Err(BatchBackendError::InitFailed(
                "metric not supported by i8 backend".to_string(),
            ));
        }
        if !i8_supports_metric(metric) {
            return Err(BatchBackendError::InitFailed(
                "i8 path does not support this metric".to_string(),
            ));
        }

        let ctx = match global_context() {
            Ok(ctx) => ctx,
            Err(msg) => return Err(BatchBackendError::InitFailed(msg.clone())),
        };
        i8_pipeline().map_err(BatchBackendError::InitFailed)?;

        let dim = matrix.dim();
        let row_count = matrix.row_count();
        if dim == 0 || row_count == 0 {
            return Err(BatchBackendError::InitFailed(
                "resident matrix is empty".to_string(),
            ));
        }

        // 量子化入力: 全行を f16 往復デコードして f32 行列を作る（一時
        // バッファ。D8 の真値〔f16 常駐〕とも一致させるための構築時コスト）。
        let mut decoded: Vec<f32> = Vec::new();
        decoded
            .try_reserve_exact(row_count.saturating_mul(dim))
            .map_err(|_| {
                BatchBackendError::InitFailed("i8 decode buffer allocation failed".to_string())
            })?;
        // 行デコード用スクラッチバッファを `dim` ぶんフォールブルに事前予約する
        // （codex-review 指摘対応・P1。`ResidentMatrix::row_f32_into` は
        // `out.clear()` してから `dim` 回 `Vec::push` するため、未予約だと
        // `push` の内部（amortized・infallible）確保がメモリ不足時に abort
        // しうる——`decoded` 側の予約とは別の `Vec` であり、`batch_search` の
        // 行デコード用スクラッチと同じ契約をここでも保つ）。
        let mut row_buf: Vec<f32> = Vec::new();
        try_reserve_exact(&mut row_buf, dim, "i8 decode row buffer")
            .map_err(|e| BatchBackendError::InitFailed(format!("{e}")))?;
        for idx in 0..row_count {
            if matrix.row_f32_into(idx, &mut row_buf).is_none() {
                return Err(BatchBackendError::InitFailed(
                    "resident matrix row decode failed".to_string(),
                ));
            }
            decoded.extend_from_slice(&row_buf);
        }

        let (row_scales, packed) = encode_rows(dim, row_count, &decoded)
            .map_err(|e| BatchBackendError::InitFailed(format!("i8 encode failed: {e}")))?;
        let row_stride = row_stride_for_dim(dim);

        // Cosine／L2 の候補選出キー用に、再スコアと同じ真値（f16 往復デコード行）から
        // 行ごとのノルム²を求める（行自身だけから決まる。他行・他テナントに非依存）。
        let row_norms: Vec<f32> = match metric {
            GpuBatchMetric::InnerProduct | GpuBatchMetric::L1 => Vec::new(),
            GpuBatchMetric::Cosine | GpuBatchMetric::L2 => {
                let dim_u32 = u32::try_from(dim).map_err(|_| {
                    BatchBackendError::InitFailed("i8 dimension out of range".to_string())
                })?;
                row_norms_sq(&decoded, dim_u32).map_err(|_| {
                    BatchBackendError::InitFailed("i8 row norm allocation failed".to_string())
                })?
            }
        };

        let packed_bytes = packed
            .len()
            .checked_mul(std::mem::size_of::<u32>())
            .ok_or_else(|| {
                BatchBackendError::InitFailed("i8 packed matrix byte size overflow".to_string())
            })?;
        if packed_bytes as u64 > ctx.max_storage_buffer_binding_size {
            return Err(BatchBackendError::InitFailed(
                "i8 resident matrix exceeds adapter storage buffer limit".to_string(),
            ));
        }
        if packed_bytes == 0 {
            return Err(BatchBackendError::InitFailed(
                "i8 resident matrix is empty".to_string(),
            ));
        }

        let _guard = gpu_dispatch_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let packed_staging = bytes_of_u32_slice(&packed)?;
        let validation_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let oom_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let row_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("i8 resident matrix packed rows"),
            size: packed_bytes as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ctx.queue.write_buffer(&row_buffer, 0, &packed_staging);
        let poll_failed = || {
            BatchBackendError::InitFailed(
                "device poll failed or timed out during i8 buffer upload".to_string(),
            )
        };
        if block_on_with_device_poll(&ctx.device, oom_scope.pop())
            .map_err(|_| poll_failed())?
            .is_some()
        {
            return Err(BatchBackendError::InitFailed(
                "gpu out of memory while uploading the i8 resident matrix".to_string(),
            ));
        }
        if block_on_with_device_poll(&ctx.device, validation_scope.pop())
            .map_err(|_| poll_failed())?
            .is_some()
        {
            return Err(BatchBackendError::InitFailed(
                "gpu validation error while uploading the i8 resident matrix".to_string(),
            ));
        }

        Ok(Self {
            matrix,
            row_scales,
            metric,
            row_norms_sq: row_norms,
            row_buffer,
            row_stride,
            options,
            device_lost: ctx.device_lost.clone(),
            uncaptured_error: ctx.uncaptured_error.clone(),
            stats: std::sync::Arc::new(GpuI8Stats::default()),
            meta: GpuI8Meta {
                backend: ctx.backend,
                dot4_impl: Dot4I8Impl::Undetermined,
            },
        })
    }

    pub fn stats(&self) -> GpuI8BatchStatsSnapshot {
        self.stats.snapshot()
    }

    pub fn meta(&self) -> GpuI8Meta {
        self.meta
    }

    /// **テスト・ベンチ専用**（`bench-internals` feature 限定）。再スコア前の
    /// 生 i32 スコア（`(slot, score)`）をクエリごとに返す。codex-review
    /// 指摘対応（P1 #2）により、GPU から readback した値をそのまま無制限に
    /// 保持するのではなく、[`raw_i32_scores`] 内でチャンクごとに逐次縮約した
    /// 上位 `k' = min(reachable_rows, k * oversample)` 件だけを返す
    /// （行数が少ない既存テストでは `reachable_rows <= k'` のため実質的に
    /// 全件が返る）。可視性判定・最終スコア契約には関与しない（既存
    /// `batch_search_with_row_budget_for_tests` と同じ露出方針）。CPU 参照
    /// 実装（[`dot_i8_packed_ref`]）との整数一致検証に使う。
    #[cfg(feature = "bench-internals")]
    pub fn batch_search_raw_i32_for_tests(
        &self,
        queries: &[BatchQuery<'_>],
    ) -> Result<Vec<Vec<(u32, i32)>>, BatchExecError> {
        self.raw_i32_scores(queries)
    }

    fn raw_i32_scores(
        &self,
        queries: &[BatchQuery<'_>],
    ) -> Result<Vec<Vec<(u32, i32)>>, BatchExecError> {
        self.guard_runtime()?;
        validate_batch_queries(self.matrix.dim(), queries).map_err(BatchExecError::Input)?;
        check_reachable_batch_work(&self.matrix, queries).map_err(BatchExecError::Input)?;

        let ctx = match global_context() {
            Ok(ctx) => ctx,
            Err(msg) => {
                return Err(BatchExecError::Backend(BatchBackendError::InitFailed(
                    msg.clone(),
                )))
            }
        };
        let pipeline = i8_pipeline()
            .map_err(|msg| BatchExecError::Backend(BatchBackendError::InitFailed(msg)))?;

        let _guard = gpu_dispatch_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let groups = group_queries_by_ctx(queries);
        let mut out: Vec<Option<Vec<(u32, i32)>>> = Vec::new();
        try_reserve_exact(&mut out, queries.len(), "gpu i8 raw scores")
            .map_err(BatchExecError::Input)?;
        out.resize_with(queries.len(), || None);

        for group in &groups {
            let Some(&first_idx) = group.first() else {
                continue;
            };
            let group_ctx = queries.get(first_idx).map(|q| q.ctx).ok_or_else(|| {
                BatchExecError::Backend(BatchBackendError::KernelLaunchFailed(
                    "i8 query group index out of range".to_string(),
                ))
            })?;
            let reachable = gather_reachable_rows(&self.matrix, group_ctx)
                .map_err(|e| BatchExecError::Input(e.into_batch_search_error()))?;

            for &qi in group {
                let query = queries.get(qi).ok_or_else(|| {
                    BatchExecError::Backend(BatchBackendError::KernelLaunchFailed(
                        "i8 query index out of range".to_string(),
                    ))
                })?;
                let (s_q, qq) = quantize_query(query.vector).map_err(|e| {
                    BatchExecError::Backend(BatchBackendError::KernelLaunchFailed(format!(
                        "i8 query quantize failed: {e}"
                    )))
                })?;

                // codex-review 指摘対応（P1 #2）: 到達行を無制限に
                // `per_query` へ溜め込むと、`dim` が小さい入力（例: dim=1）
                // では `MAX_BATCH_WORK`（rows × queries × dim）の枠内でも
                // `queries × reachable_rows` 要素分のメモリを要求しうる
                // （既存の `sum(k) <= MAX_BATCH_TOTAL_K` 上限はここでは効か
                // ない）。D8 が最終的に必要とするのは
                // `k' = min(reachable, k * oversample)` 件だけなので、
                // チャンクを読むたびに [`reduce_top_k_prime`] で逐次縮約し、
                // 保持量を常に `k' + 直近チャンクの行数` 以内へ抑える
                // （バッチ全体での保持量の上限は
                // `Σk' <= MAX_BATCH_TOTAL_K * MAX_I8_OVERSAMPLE`
                // = 1,000,000 * 32 = 32,000,000 要素で、`validate_batch_queries`
                // の `sum(k)` 上限と [`GpuI8BatchBackend::try_new`] の
                // oversample 範囲検証から導かれる）。
                let rank_ctx = RankingCtx {
                    metric: self.metric,
                    row_scales: &self.row_scales,
                    row_norms_sq: &self.row_norms_sq,
                    query_scale: s_q,
                };
                let k_prime = query
                    .k
                    .saturating_mul(self.options.oversample)
                    .min(reachable.len());

                let mut per_query: Vec<(u32, i32)> = Vec::new();
                try_reserve_exact(&mut per_query, k_prime, "gpu i8 raw scores (per query)")
                    .map_err(BatchExecError::Input)?;
                let chunk_cap = i8_chunk_rows(ctx.max_workgroups_per_dimension);
                for chunk in reachable.chunks(chunk_cap.max(1)) {
                    let scores = dispatch_i8_dot_products(
                        ctx,
                        pipeline,
                        &self.row_buffer,
                        self.row_stride,
                        chunk,
                        &qq,
                    )?;
                    if scores.len() != chunk.len() {
                        return Err(BatchExecError::Backend(BatchBackendError::TransferFailed(
                            "i8 readback length mismatch".to_string(),
                        )));
                    }
                    self.stats.dispatches.fetch_add(1, Ordering::Relaxed);
                    self.stats
                        .readback_bytes
                        .fetch_add((scores.len() as u64).saturating_mul(4), Ordering::Relaxed);

                    try_reserve_exact(&mut per_query, chunk.len(), "gpu i8 raw scores (chunk)")
                        .map_err(BatchExecError::Input)?;
                    for (&slot, &score) in chunk.iter().zip(scores.iter()) {
                        per_query.push((slot, score));
                    }
                    reduce_top_k_prime(&mut per_query, k_prime, &rank_ctx)
                        .map_err(BatchExecError::Input)?;
                }
                // 最後のチャンク追加がちょうど `k_prime` 件に収まり
                // ループ内の縮約が発火しなかった場合に備え、返却前に必ず
                // 1 回、行スケール込みの推定内積降順（同点は slot 昇順）へ
                // 確定させる（[`reduce_top_k_prime`] は保持量が `k_prime` を
                // 超えたときのみ並べ替えるため、size が最初から `k_prime`
                // 以下だった場合は挿入順のまま残る）。
                sort_by_ranking_key(&mut per_query, &rank_ctx);
                // ループ内で一度も [`reduce_top_k_prime`] の縮約が発火しなかった
                // 経路（size が最初から `k_prime` 以下）でも、直前のチャンク
                // push に備えた `try_reserve_exact` の予約量がそのまま容量に
                // 残っている可能性があるため、保存前に必ず容量を長さぴったりへ
                // 縮小する（[`shrink_to_len`] 参照。容量と長さが既に一致して
                // いれば no-op）。
                let per_query = shrink_to_len(per_query).map_err(BatchExecError::Input)?;
                if let Some(slot) = out.get_mut(qi) {
                    *slot = Some(per_query);
                }
            }
        }

        let mut results: Vec<Vec<(u32, i32)>> = Vec::new();
        try_reserve_exact(&mut results, out.len(), "gpu i8 raw scores (final)")
            .map_err(BatchExecError::Input)?;
        for slot in out {
            match slot {
                Some(v) => results.push(v),
                None => {
                    return Err(BatchExecError::Backend(
                        BatchBackendError::KernelLaunchFailed(
                            "i8 query result missing after dispatch".to_string(),
                        ),
                    ))
                }
            }
        }
        Ok(results)
    }

    fn guard_runtime(&self) -> Result<(), BatchExecError> {
        if self.device_lost.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(BatchExecError::Backend(BatchBackendError::DeviceLost(
                "gpu device lost".to_string(),
            )));
        }
        if self
            .uncaptured_error
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(BatchExecError::Backend(
                BatchBackendError::KernelLaunchFailed(
                    "gpu reported an uncaptured error".to_string(),
                ),
            ));
        }
        Ok(())
    }
}

/// 1 dispatch に含める行チャンクの行数上限（D11: i8 用の全量 readback のみ。
/// ワークグループ数が adapter 上限を超えないようにするだけの単純な上限）。
fn i8_chunk_rows(max_workgroups_per_dimension: u32) -> usize {
    (max_workgroups_per_dimension as usize).saturating_mul(256)
}

/// `buf` を [`ranking_key`] 降順（同点は `slot` 昇順）へ確定させる
/// （in-place・追加確保なし）。
///
/// `slice::sort_by`（安定ソート）はマージソート実装のため大きなスライスで
/// 内部的に作業用バッファをヒープ確保する（`Vec` の `try_reserve` のような
/// フォールブル API を経由しないため、確保失敗時は abort しうる）。到達行数
/// によっては `per_query` が数百万要素に達しうるため、フォールブル確保
/// 契約（coding-rust.md）を保つには内部確保のない `sort_unstable_by`
/// （pdqsort。作業用ヒープ確保を持たない）が必須（codex-review 指摘対応・
/// P1）。比較子は `slot` 昇順の完全なタイブレークを持つ全順序であり、
/// 安定/不安定のどちらでも出力は同じ（sort-determinism-check
/// （`docs/design/rrf-tie-break-determinism.md`）の許可マーカー参照）。
pub(crate) fn sort_by_ranking_key(buf: &mut [(u32, i32)], ctx: &RankingCtx<'_>) {
    let cmp = |&(slot_a, score_a): &(u32, i32), &(slot_b, score_b): &(u32, i32)| {
        let key_a = ranking_key(slot_a, score_a, ctx);
        let key_b = ranking_key(slot_b, score_b, ctx);
        key_b.total_cmp(&key_a).then(slot_a.cmp(&slot_b))
    };
    buf.sort_unstable_by(cmp); // sort-determinism: allow 比較子は slot 昇順の明示的タイブレークを含む全順序（フォールブル確保契約のため sort_by からの意図的な切り替え）
}

/// [`GpuI8BatchBackend::raw_i32_scores`] のチャンク処理ごとに呼ぶ逐次縮約
/// （codex-review 指摘対応・P1 #2）。`buf` の長さが `k_prime` を超えたときに
/// 限り [`ranking_key`] 降順（同点は `slot` 昇順）で並べ替えて上位
/// `k_prime` 件へ切り詰める。これは標準的なストリーミング top-k の性質
/// （`TopK(A ∪ B, k) == TopK(TopK(A, k) ∪ B, k)`）により、`buf` を超過の
/// たびに縮約しても最終的な上位 `k_prime` 件の**集合**は「全チャンクを
/// 一括保持してから 1 回だけ選出した場合」と一致する（超過しなかった
/// チャンクでは何も破棄しないため、より弱く「まだ何も捨てる必要が無い」
/// ケースになるだけで、この性質は崩れない）。
pub(crate) fn reduce_top_k_prime(
    buf: &mut Vec<(u32, i32)>,
    k_prime: usize,
    ctx: &RankingCtx<'_>,
) -> Result<(), BatchSearchError> {
    if buf.len() <= k_prime {
        return Ok(());
    }
    sort_by_ranking_key(buf, ctx);
    buf.truncate(k_prime);
    // `Vec::truncate` は長さのみ減らし確保済み容量を解放しない。この関数は
    // チャンク（最大 `i8_chunk_rows` 行）を丸ごと push した直後に呼ばれるため、
    // 縮約前の容量は「チャンク全量分」に達している——`raw_i32_scores` が
    // クエリごとに保持する `per_query` はこの縮約後の Vec をそのままバッチ
    // 全体（最大 `MAX_BATCH_QUERIES`）ぶん同時に保持し続けるため、縮小せずに
    // 放置すると設計上の Σk' によるメモリ上限契約（D9）が成立しなくなる
    // （codex-review 指摘対応・P1: dim=1・到達行 100 万・4096 クエリ・各 k=1
    // のような形状で候補バッファだけで約 32.768GB に達すると指摘された）。
    // 縮小後の長さぴったりの Vec へ移し替える。縮小予約自体の失敗は
    // 「容量が大きいまま成功扱いで返す」と Σk' 上限契約が崩れるため、
    // 単なる最適化として握り潰さず `Err` として呼び出し元へ伝播する
    // （codex-review 指摘対応・P2）。
    *buf = shrink_to_len(std::mem::take(buf))?;
    Ok(())
}

/// `buf` の容量を長さぴったりへ縮小した新しい `Vec` を返す（[`reduce_top_k_prime`]
/// 参照）。フォールブル確保（`try_reserve_exact`）に失敗した場合は `Err` を
/// 返す——縮小予約の失敗を「容量が大きいままの `buf` を成功扱いで返す」形で
/// 握り潰すと、`per_query` が容量縮小前の「チャンク全量分」を保持したまま
/// クエリ数分バッチ全体で同時に生存し、設計上の Σk' によるメモリ上限契約
/// （D9）がメモリ逼迫時にこそ成立しなくなる（abort を避けるつもりが、より
/// 大きなメモリ確保状態のまま処理を継続させてしまう。codex-review 指摘
/// 対応・P2）。呼び出し元（[`reduce_top_k_prime`]）で `Err` を伝播させ、
/// 最終的に [`GpuI8BatchBackend::raw_i32_scores`] が
/// `BatchExecError::Input` として fail-closed に拒否する。
fn shrink_to_len(buf: Vec<(u32, i32)>) -> Result<Vec<(u32, i32)>, BatchSearchError> {
    if buf.capacity() <= buf.len() {
        return Ok(buf);
    }
    let mut shrunk: Vec<(u32, i32)> = Vec::new();
    try_reserve_exact(&mut shrunk, buf.len(), "gpu i8 raw scores (shrink)")?;
    shrunk.extend_from_slice(&buf);
    Ok(shrunk)
}

impl BatchBackend for GpuI8BatchBackend {
    fn batch_search(&self, queries: &[BatchQuery<'_>]) -> Result<Vec<BatchHit>, BatchExecError> {
        let raw = self.raw_i32_scores(queries)?;

        let mut hits: Vec<BatchHit> = Vec::new();
        try_reserve_exact(&mut hits, queries.len(), "gpu i8 batch hits")
            .map_err(BatchExecError::Input)?;

        // 行デコード用スクラッチバッファを `dim` ぶんフォールブルに事前予約する
        // （codex-review 指摘対応・P1。`ResidentMatrix::row_f32_into` は
        // `out.clear()` してから `dim` 回 `Vec::push` するため、未予約だと
        // `push` の内部（amortized・infallible）確保がメモリ不足時に abort
        // しうる——既存 CPU バッチ経路〔`batch_search.rs::row_buffer_pool`〕が
        // フォールブル確保のみを行うのと同じ契約をここでも保つ）。`dim` は
        // 全行で共通のため、ループの外で一度予約すれば `out.clear()` は
        // 容量を保ったままなので以降のイテレーションでも再確保は起きない。
        let mut row_buf: Vec<f32> = Vec::new();
        try_reserve_exact(&mut row_buf, self.matrix.dim(), "gpu i8 row buffer")
            .map_err(BatchExecError::Input)?;
        for (qi, per_query) in raw.iter().enumerate() {
            let query = queries.get(qi).ok_or_else(|| {
                BatchExecError::Backend(BatchBackendError::KernelLaunchFailed(
                    "i8 query index out of range".to_string(),
                ))
            })?;

            // D8: `per_query` は `raw_i32_scores` 側で既に
            // `k' = min(reachable_rows, k * oversample)` 件へ縮約・
            // ソート済み（codex-review 指摘対応・P1 #2）。ここで再度
            // 全体を `clone` して並べ替える必要はない
            // （未予約 `clone` は確保失敗時に abort しうるため撤去した）。
            self.stats
                .rescored_candidates
                .fetch_add(per_query.len() as u64, Ordering::Relaxed);

            let mut selector = TopKSelector::new(query.k);
            // `query.k` は `validate_batch_queries`（`raw_i32_scores` 呼び出し
            // 前に `batch_search.rs` の上限検証を経由する）でバッチ全体の
            // `sum(k)` が上限内であることを検証済みのため、`TopKSelector::push`
            // の amortized 成長（`BinaryHeap::push`。内部確保は infallible）に
            // 任せず、既存 CPU バッチ経路（`batch_search.rs` の
            // `selector.try_reserve(q.k)`）と同じ契約でフォールブルに事前
            // 予約する（codex-review 指摘対応・P1）。
            selector.try_reserve(query.k).map_err(|e| {
                BatchExecError::Input(BatchSearchError::AllocationFailed(format!(
                    "failed to reserve i8 selector heap: {e}"
                )))
            })?;
            // Cosine の最終スコア用。クエリごとに 1 回だけ計算する。
            let query_norm_sq = dot(query.vector, query.vector);
            for (slot, _int_score) in per_query {
                if self
                    .matrix
                    .row_f32_into(*slot as usize, &mut row_buf)
                    .is_none()
                {
                    continue;
                }
                let score = final_score(self.metric, query.vector, &row_buf, query_norm_sq);
                if !score.is_finite() {
                    continue;
                }
                selector.push(CandidateHit {
                    id: u64::from(*slot),
                    score,
                });
            }

            let resolved = finalize_gpu_hits(&self.matrix, query.ctx, &selector.into_sorted_vec())
                .map_err(BatchExecError::Input)?;
            hits.push(BatchHit { hits: resolved });
        }

        Ok(hits)
    }
}

/// 1 クエリ × `row_indices` 分の i8 整数内積を GPU で計算し、readback した
/// `i32` 列を返す（[`super::dispatch_dot_products`] の i8 版。手順・防御は
/// 同一だが、readback の解釈が `u32 as i32`〔ビット再解釈。二の補数のため
/// `as` キャストで正しい値になる〕である点だけが異なる）。
fn dispatch_i8_dot_products(
    ctx: &GpuContext,
    pipeline: &wgpu::ComputePipeline,
    row_buffer: &wgpu::Buffer,
    row_stride: usize,
    row_indices: &[u32],
    query_packed: &[u32],
) -> Result<Vec<i32>, BatchExecError> {
    if row_indices.is_empty() {
        return Ok(Vec::new());
    }
    if query_packed.len() != row_stride {
        return Err(BatchExecError::Backend(BatchBackendError::TransferFailed(
            "i8 query buffer length does not match row_stride".to_string(),
        )));
    }

    let row_count = row_indices.len() as u32;
    let params = GpuParams {
        row_stride: row_stride as u32,
        row_count,
        query_count: 1,
        query_stride: row_stride as u32,
        // i8 版の WGSL `Params`（16 バイト）は metric を持たない。内積専用（Issue #2003）。
        metric: 0,
    };
    let params_bytes = params.to_ne_bytes_vec().map_err(BatchExecError::Backend)?;
    let row_ids_bytes = bytes_of_u32_slice(row_indices).map_err(BatchExecError::Backend)?;
    let query_bytes = bytes_of_u32_slice(query_packed).map_err(BatchExecError::Backend)?;

    let validation_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::Validation);
    let oom_scope = ctx.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);

    let params_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("i8 batch dot product params"),
        size: 32,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&params_buffer, 0, &params_bytes);

    let row_ids_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("i8 batch dot product row ids"),
        size: row_ids_bytes.len() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&row_ids_buffer, 0, &row_ids_bytes);

    let query_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("i8 batch dot product query"),
        size: query_bytes.len() as u64,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    ctx.queue.write_buffer(&query_buffer, 0, &query_bytes);

    let scores_bytes = (row_indices.len() as u64).saturating_mul(4);
    let scores_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("i8 batch dot product scores"),
        size: scores_bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback_buffer = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("i8 batch dot product readback"),
        size: scores_bytes,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });

    let bind_group = ctx.device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("i8 batch dot product bind group"),
        layout: &ctx.bind_group_layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: params_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: row_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: row_ids_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 3,
                resource: query_buffer.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 4,
                resource: scores_buffer.as_entire_binding(),
            },
        ],
    });

    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("i8 batch dot product encoder"),
        });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("i8 batch dot product pass"),
            timestamp_writes: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        let workgroups = (row_count as u64).div_ceil(256);
        let workgroups_x = u32::try_from(workgroups).unwrap_or(u32::MAX);
        pass.dispatch_workgroups(workgroups_x, 1, 1);
    }
    encoder.copy_buffer_to_buffer(&scores_buffer, 0, &readback_buffer, 0, scores_bytes);
    ctx.queue.submit(std::iter::once(encoder.finish()));

    let poll_failed =
        || BatchBackendError::DeviceLost("device poll failed or timed out".to_string());
    let oom_err = block_on_with_device_poll(&ctx.device, oom_scope.pop())
        .map_err(|_| poll_failed())
        .map_err(BatchExecError::Backend)?;
    let validation_err = block_on_with_device_poll(&ctx.device, validation_scope.pop())
        .map_err(|_| poll_failed())
        .map_err(BatchExecError::Backend)?;
    if let Some(e) = oom_err {
        return Err(BatchExecError::Backend(
            BatchBackendError::KernelLaunchFailed(format!("gpu out of memory: {e}")),
        ));
    }
    if let Some(e) = validation_err {
        return Err(BatchExecError::Backend(
            BatchBackendError::KernelLaunchFailed(format!("gpu validation error: {e}")),
        ));
    }

    let bytes = wait_and_read_buffer(ctx, &readback_buffer).map_err(BatchExecError::Backend)?;
    let (quads, remainder) = bytes.as_chunks::<4>();
    if !remainder.is_empty() {
        return Err(BatchExecError::Backend(BatchBackendError::TransferFailed(
            "i8 readback byte length not a multiple of 4".to_string(),
        )));
    }
    let mut out: Vec<i32> = Vec::new();
    out.try_reserve_exact(quads.len())
        .map_err(|_| BatchBackendError::TransferFailed("i8 readback allocation failed".to_string()))
        .map_err(BatchExecError::Backend)?;
    for quad in quads {
        out.push(u32::from_ne_bytes(*quad) as i32);
    }
    Ok(out)
}

#[cfg(test)]
mod precision_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_i8x4_round_trips_and_preserves_lane_order() {
        let lanes = [1i8, -2, 3, -4];
        let packed = pack_i8x4(lanes);
        // レーン 0 が最下位バイトに来ることを固定する（レーン順の取り違えを
        // 検出できるよう非対称な値を使う）。
        assert_eq!(packed & 0xFF, 1u32);
        assert_eq!(unpack_i8x4(packed), lanes);
    }

    #[test]
    fn encode_rows_clamps_to_signed_range_and_handles_zero_row() {
        // 行単位量子化（D3 改訂）: 行 0 は全 0（scale == 0 の定数行）・
        // 行 1・2 は非零。
        let vectors = [0.0f32, 0.0, 10.0, 5.0, -10.0, 5.0];
        let (scales, packed) = encode_rows(2, 3, &vectors).expect("encode should succeed");
        assert_eq!(scales.scale(0), Some(0.0));
        assert_eq!(packed.len(), 3); // row_stride_for_dim(2) == 1, 3 rows
        assert_eq!(unpack_i8x4(packed[0]), [0i8; 4]);
        for &p in &packed[1..] {
            let lanes = unpack_i8x4(p);
            for &lane in &lanes[..2] {
                assert!((-127..=127).contains(&(lane as i32)));
            }
        }
    }

    #[test]
    fn encode_rows_row_scale_is_independent_of_other_rows_cross_tenant_leak_regression() {
        // codex-review 指摘（P0）の回帰: ある行の量子化結果（スケール・
        // パック済みバイト列）が、同じ常駐行列に同居する他テナントの行の値
        // には一切依存しないことを固定する。他テナント側の行を極端な外れ値
        // へ差し替えても、対象行の出力がビット同一であることを確認する
        // （`docs/design/gpu-batch-i8-packed.md`「D3」節参照）。
        let dim = 3usize;
        let tenant_a_rows = [1.0f32, -2.0, 0.5, 3.0, 0.0, -1.0];
        let tenant_b_normal = [0.1f32, 0.2, -0.3];
        let tenant_b_outlier = [1_000.0f32, -2_000.0, 500.0];

        let mut matrix_normal = tenant_a_rows.to_vec();
        matrix_normal.extend_from_slice(&tenant_b_normal);
        let mut matrix_outlier = tenant_a_rows.to_vec();
        matrix_outlier.extend_from_slice(&tenant_b_outlier);

        let (scales_normal, packed_normal) =
            encode_rows(dim, 3, &matrix_normal).expect("encode (normal) should succeed");
        let (scales_outlier, packed_outlier) =
            encode_rows(dim, 3, &matrix_outlier).expect("encode (outlier) should succeed");

        let row_stride = row_stride_for_dim(dim);
        for row in 0..2 {
            assert_eq!(
                scales_normal.scale(row),
                scales_outlier.scale(row),
                "tenant-a row {row} scale must not depend on tenant-b's row values"
            );
            let start = row * row_stride;
            let end = start + row_stride;
            assert_eq!(
                packed_normal.get(start..end),
                packed_outlier.get(start..end),
                "tenant-a row {row} packed bytes must not depend on tenant-b's row values"
            );
        }
    }

    #[test]
    fn encode_rows_rejects_non_finite_input() {
        let vectors = [0.0f32, f32::NAN];
        assert_eq!(
            encode_rows(2, 1, &vectors).err(),
            Some(I8EncodeError::NonFinite)
        );
    }

    #[test]
    fn encode_rows_rejects_shape_mismatch() {
        let vectors = [0.0f32, 1.0, 2.0];
        assert_eq!(
            encode_rows(2, 1, &vectors).err(),
            Some(I8EncodeError::InvalidShape)
        );
    }

    #[test]
    fn quantize_query_all_zero_scale_yields_zero_packed() {
        let (s_q, packed) = quantize_query(&[0.0, 0.0]).expect("quantize should succeed");
        assert_eq!(s_q, 0.0);
        for p in packed {
            assert_eq!(p, 0);
        }
    }

    #[test]
    fn quantize_scalar_uses_f64_scale_and_does_not_treat_f32_underflow_as_zero_row() {
        // codex-review 指摘対応・P1 #4 の回帰:
        // `max_abs / 127.0`（f64）が非零だが f32 表現では劣化数域を下回り
        // ゼロへアンダーフローするスケールでも、`quantize_scalar` へ渡す
        // スケールが f64 精度のままであれば「零行（scale == 0.0）」として
        // 誤って全要素 0 へ丸められないことを固定する。
        // `v` は f32 の非零最小劣化数（smallest positive subnormal）。これ自体は
        // f32 として表現可能な非零値だが、`v / 127.0` は f32 の表現域を下回り
        // f32 精度では 0.0 へアンダーフローする（f64 精度では非零のまま
        // 表現できる）。
        let v: f32 = f32::from_bits(1);
        let max_abs = f64::from(v);
        let scale_f64 = max_abs / f64::from(I8_CLAMP_ABS);
        assert_ne!(
            scale_f64, 0.0,
            "test precondition: scale must be nonzero in f64"
        );
        assert_eq!(
            scale_f64 as f32, 0.0,
            "test precondition: scale must underflow to zero when rounded to f32"
        );

        let lane = quantize_scalar(v, scale_f64).expect("quantize should succeed");
        assert_ne!(
            lane, 0,
            "a value at the row's own magnitude must not quantize to 0 just because the f32 \
             rounding of the scale underflows to zero"
        );
    }

    #[test]
    fn encode_rows_tiny_magnitude_row_is_not_quantized_to_all_zero_lanes() {
        // 上記単体テストの `encode_rows` 経由での end-to-end 回帰: 行の
        // 全成分が極小の非零有限値でも、`encode_rows` が返すパック済み行が
        // 全レーン 0（零行と誤認された状態）にならないことを確認する。
        //
        // 注: [`Sq8RowScales`] に格納される `scale`（表示・保存用途の f32
        // 丸め値。`ranking_key` の近似再スケールにのみ使う）自体は、この
        // 極端な形状では f32 の表現域を下回りアンダーフローして `0.0` の
        // ままで構わない（P1 #4 の修正対象は量子化の除算に使う内部精度で
        // あり、保存用の表示値の丸めは対象外——`row_scale_f64` のドキュメン
        // テーションコメント参照）。ここで固定するのは「パック済みレーンが
        // 全 0 にならない」という量子化契約のみ。
        let tiny: f32 = f32::from_bits(1); // f32 の非零最小劣化数
        let vectors = [tiny, -tiny];
        let (_scales, packed) = encode_rows(2, 1, &vectors).expect("encode should succeed");
        assert_ne!(
            unpack_i8x4(packed[0]),
            [0i8; 4],
            "a tiny-but-nonzero row must not be quantized to an all-zero packed row"
        );
    }

    #[test]
    fn reduce_top_k_prime_shrinks_capacity_after_truncating_down_from_a_large_chunk() {
        // codex-review 指摘対応・P1 #? の回帰: `Vec::truncate` は長さのみ
        // 減らし確保済み容量を解放しないため、`raw_i32_scores` のように
        // 1 チャンクを丸ごと push してから縮約する経路では、縮約後も
        // チャンク全量分の容量を保持し続けてしまう（`per_query` はクエリ
        // ごとにバッチ全体で同時に保持されるため、極端な形状では設計上の
        // Σk' によるメモリ上限契約が成立しなくなる）。
        let row_scales = Sq8RowScales {
            scales: vec![1.0f32; 8],
        };
        let rank_ctx = RankingCtx {
            metric: GpuBatchMetric::InnerProduct,
            row_scales: &row_scales,
            row_norms_sq: &[],
            query_scale: 1.0,
        };
        let mut buf: Vec<(u32, i32)> = Vec::new();
        buf.try_reserve_exact(100_000)
            .expect("reserve should succeed");
        let reserved_capacity = buf.capacity();
        for i in 0..100_000u32 {
            buf.push((i, 1));
        }

        reduce_top_k_prime(&mut buf, 4, &rank_ctx).expect("reserve should succeed");

        assert_eq!(buf.len(), 4);
        assert!(
            buf.capacity() < reserved_capacity,
            "reduce_top_k_prime must shrink capacity after truncating down from a large chunk \
             (capacity {} was not reduced from the pre-truncate reservation of {})",
            buf.capacity(),
            reserved_capacity
        );
    }

    #[test]
    fn dot_i8_packed_ref_extreme_values_stay_within_i32_bound() {
        // D6: dim=8192・全レーン ±127 の極値で i32 の理論上限内に厳密に収まる。
        let dim = 8_192usize;
        let row_stride = row_stride_for_dim(dim);
        let row: Vec<u32> = (0..row_stride)
            .map(|_| pack_i8x4([127, -127, 127, -127]))
            .collect();
        let query: Vec<u32> = (0..row_stride)
            .map(|_| pack_i8x4([127, -127, 127, -127]))
            .collect();
        let score = dot_i8_packed_ref(&row, &query);
        // 全レーンが `127*127` の正値（符号が一致するペアの積は常に正）に
        // なるよう組んだ入力のため、理論上限に一致する。
        assert_eq!(i64::from(score), MAX_I8_DOT_MAGNITUDE);
    }

    #[test]
    fn dot_i8_packed_ref_matches_naive_lane_expansion() {
        let row = vec![pack_i8x4([1, 2, 3, 4]), pack_i8x4([-1, -2, -3, -4])];
        let query = vec![pack_i8x4([4, 3, 2, 1]), pack_i8x4([1, 1, 1, 1])];
        // 4+6+6+4=20、-1-2-3-4=-10 で expected=10。
        let expected: i32 = 10;
        assert_eq!(dot_i8_packed_ref(&row, &query), expected);
    }

    #[test]
    fn dot_shader_i8_wgsl_uses_dot4_i8_packed_builtin() {
        assert!(DOT_SHADER_I8_WGSL.contains("dot4I8Packed"));
    }

    #[test]
    fn i8_supports_metric_table_matches_decision() {
        assert!(i8_supports_metric(GpuBatchMetric::InnerProduct));
        assert!(i8_supports_metric(GpuBatchMetric::Cosine));
        assert!(i8_supports_metric(GpuBatchMetric::L2));
        assert!(!i8_supports_metric(GpuBatchMetric::L1));
    }

    #[test]
    fn try_new_with_metric_rejects_l1_before_touching_gpu() {
        use crate::storage::Visibility;
        let matrix = ResidentMatrix::build(
            &[1],
            &["t".to_string()],
            &[Visibility::Public],
            2,
            &[1.0, 2.0],
        )
        .expect("matrix build");
        let err = GpuI8BatchBackend::try_new_with_metric(
            matrix,
            GpuI8Options::default(),
            GpuBatchMetric::L1,
        )
        .err()
        .expect("L1 must be rejected");
        match err {
            BatchBackendError::InitFailed(msg) => {
                assert_eq!(msg, "i8 path does not support this metric")
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn encode_rows_is_invariant_to_row_scaling_so_cosine_needs_no_normalized_copy() {
        let base = [0.3f32, -1.2, 2.5, 0.7];
        let scaled: Vec<f32> = base.iter().map(|v| v * 8.0).collect();
        let (_, a) = encode_rows(4, 1, &base).expect("encode");
        let (_, b) = encode_rows(4, 1, &scaled).expect("encode");
        assert_eq!(a, b);
    }

    #[test]
    fn cosine_key_sinks_zero_norm_rows_without_dividing() {
        let scales = Sq8RowScales {
            scales: vec![1.0, 1.0],
        };
        let norms = [0.0f32, 4.0];
        let ctx = RankingCtx {
            metric: GpuBatchMetric::Cosine,
            row_scales: &scales,
            row_norms_sq: &norms,
            query_scale: 1.0,
        };
        assert_eq!(ranking_key(0, 100, &ctx), f64::NEG_INFINITY);
        assert_eq!(ranking_key(1, 100, &ctx), 50.0);
        // 範囲外 slot も最下位へ沈める（fail-closed）。
        assert_eq!(ranking_key(9, 100, &ctx), f64::NEG_INFINITY);
    }

    #[test]
    fn l2_key_depends_on_query_scale_and_orders_by_true_distance() {
        // 行 0 はノルムが小さく内積が小さい、行 1 は内積が大きいがノルムも大きい。
        let scales = Sq8RowScales {
            scales: vec![1.0, 1.0],
        };
        let norms = [1.0f32, 100.0];
        let mk = |query_scale| RankingCtx {
            metric: GpuBatchMetric::L2,
            row_scales: &scales,
            row_norms_sq: &norms,
            query_scale,
        };
        let c1 = mk(1.0);
        // query_scale=1: 行0 = 2*1*1*10-1 = 19、行1 = 2*60-100 = 20 -> 行1が近い。
        assert!(ranking_key(1, 60, &c1) > ranking_key(0, 10, &c1));
        // query_scale=0.5 で順位が反転する（s_q を省けない反例）。
        let c2 = mk(0.5);
        assert!(ranking_key(1, 60, &c2) < ranking_key(0, 10, &c2));
    }

    #[test]
    fn inner_product_key_is_unchanged_from_scaled_integer_score() {
        let scales = Sq8RowScales { scales: vec![0.25] };
        let ctx = RankingCtx {
            metric: GpuBatchMetric::InnerProduct,
            row_scales: &scales,
            row_norms_sq: &[],
            query_scale: 3.0,
        };
        assert_eq!(ranking_key(0, 40, &ctx), 10.0);
    }

    #[test]
    fn gpu_i8_options_out_of_range_oversample_is_rejected_without_gpu() {
        // GPU 非搭載環境でも純粋に検証できる範囲（`try_new` 冒頭のオプション
        // 検証は GPU デバイスへ触れる前に走る）だけを固定する。
        const _: () = assert!(DEFAULT_I8_OVERSAMPLE >= 1);
        const _: () = assert!(DEFAULT_I8_OVERSAMPLE <= MAX_I8_OVERSAMPLE);
    }

    /// 内積以外の metric の常駐行列では、GPU コンテキストへ触れる前に fail-closed で拒否する
    /// （Issue #2003。L2・Cosine の i8 対応は #2004）。
    #[test]
    fn try_new_rejects_non_inner_product_metric_before_touching_gpu() {
        for metric in [
            crate::batch_search::BatchMetric::L2,
            crate::batch_search::BatchMetric::Cosine,
        ] {
            let matrix = ResidentMatrix::build_with_metric(
                &[1],
                &["t".to_string()],
                &[crate::storage::Visibility::Public],
                2,
                &[1.0, 0.5],
                metric,
            )
            .unwrap();
            match GpuI8BatchBackend::try_new(matrix, GpuI8Options::default()) {
                Err(BatchBackendError::InitFailed(msg)) => {
                    assert_eq!(msg, "metric not supported by i8 backend", "{metric:?}");
                }
                other => panic!("expected InitFailed, got {:?}", other.map(|_| ())),
            }
        }
    }
}
