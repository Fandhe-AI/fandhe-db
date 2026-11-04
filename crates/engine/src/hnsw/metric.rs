//! HNSW の距離（類似度）計算を 1 か所へ集める metric 抽象（Issue #1997。
//! 親 Issue #1996。ビヘイビア INDEX-7・SQL-33 のポインタ）。
//!
//! # 役割
//!
//! 構築（`GraphBuilder`）・探索（`HnswIndex`）・近傍選択ヒューリスティック・
//! 修復パス・並列構築（`parallel_build`）・SQ8 探索ソース（`i8_query`）は、
//! 距離計算をすべて本モジュールの [`HnswMetric`] 経由で呼ぶ。後続 Issue
//! （#1998: L2・コサイン、#1999: 負の内積・L1。後者は本 Issue で完了）は variant と match の腕を足すだけで
//! 済む。キャッシュキー・永続化・EXPLAIN への反映は #2000 の担当。
//!
//! # スコア規約
//!
//! 返す値は常に「**大きいほど近い**」類似度スコア（`kernel::CandidateHit`・
//! `ScoredNode` と同じ規約）。距離系 metric を足すときは `kernel::search_l1`／
//! `search_l2` と同じく距離の符号反転値を返し、近傍選択の `d > cand.score` 比較と
//! `ScoredNode` の `Ord` をそのまま使えるようにする。
//!
//! # variant（Issue #1998 で L2・Cosine を追加）
//!
//! - [`HnswMetric::InnerProduct`]: 従来の挙動。`kernel::dot`・`dot_f16`・`dot_i8_f32` へ
//!   委譲しビット単位で不変。
//! - [`HnswMetric::L2`]: スコアは `-kernel::l2_sq(a, b)`（二乗距離の符号反転。有限な f32 は
//!   反転しても値が厳密に保たれるため、距離昇順とスコア降順が一致する）。f16・SQ8 用の L2
//!   カーネルは無いので、常駐精度は凍結時に F32 へ縮退させる（`freeze_from`）。f16・SQ8 の腕は
//!   到達不能の防御で、NaN を返し `finite_score` で fail-closed に拒否させる。
//! - [`HnswMetric::Cosine`]: ADR `docs/design/cosine-distance-method.md` の C2′（正規化済み
//!   内積への帰着）。構築時に行を 1 回だけ [`normalize_into`] で正規化したコピーを挿入・修復・
//!   凍結のすべてで使い、探索側ではクエリを 1 回だけ同じ関数で正規化する。スコア関数自体は
//!   内積の腕と同一。ゼロノルム（アンダー／オーバーフロー含む）の行は構築を、クエリは探索を
//!   fail-closed に拒否する（行の除外は #2241 の担当）。
//! - [`HnswMetric::L1`]: スコアは `-kernel::l1(a, b)`（`kernel::search_l1` と同じ符号反転）。
//!   f16・SQ8 用の L1 カーネルは無いので L2 と同様に常駐精度を F32 へ縮退させる。距離の公理を
//!   満たすため近傍選択ヒューリスティックは変更なしで使える。
//! - 負の内積（`<#>`）は新しい variant を持たず [`HnswMetric::InnerProduct`] を再利用する。
//!   距離 `-dot` の昇順とスコア `dot` の降順は、有限な f32 の符号反転が厳密なため同じ全順序
//!   （同点は id 昇順）で、構築グラフも探索結果も内積と同一になる。距離値への変換（`-score`）
//!   は SQL 側（#2000）の役割。
//!
//! 新しい `unsafe` は持たない。

use super::HnswError;
use crate::kernel::{dot, l1, l2_sq};

/// HNSW が使う距離（類似度）種別。`Copy` の fieldless enum で、`GraphBuilder`・
/// `HnswIndex`・`BuildGraph` が保持し、距離計算の free 関数へ引数で渡す。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum HnswMetric {
    /// 内積（大きいほど近い）。従来の唯一の挙動。
    #[default]
    InnerProduct,
    /// L2（二乗距離の符号反転。大きいほど近い）。
    #[allow(dead_code)] // SQL 経路の結線（#2000）までは単体テストのみが構築する
    L2,
    /// コサイン（正規化済み内積への帰着。C2′）。行・クエリは [`normalize_into`] 済みが前提。
    #[allow(dead_code)] // 同上
    Cosine,
    /// L1（マンハッタン距離の符号反転。大きいほど近い）。`<+>` 用。
    #[allow(dead_code)] // 同上
    L1,
}

impl HnswMetric {
    /// f32 行同士の類似度（大きいほど近い）。`InnerProduct` は `kernel::dot` と同一
    /// （引数順も保つ。加算順序・FMA の結果をビット単位で保つため）。
    #[inline]
    pub(crate) fn score_f32(self, a: &[f32], b: &[f32]) -> f32 {
        match self {
            HnswMetric::InnerProduct | HnswMetric::Cosine => dot(a, b),
            HnswMetric::L2 => -l2_sq(a, b),
            HnswMetric::L1 => -l1(a, b),
        }
    }

    /// f16 常駐行 × f32 クエリの類似度。`InnerProduct` は
    /// `isa::current_f16().dot_f16` と同一。
    #[inline]
    pub(crate) fn score_f16(self, row: &[u16], query: &[f32]) -> f32 {
        match self {
            HnswMetric::InnerProduct | HnswMetric::Cosine => {
                crate::isa::current_f16().dot_f16(row, query)
            }
            // 到達不能の防御（L2 は凍結時に F32 へ縮退）。NaN で fail-closed に拒否させる。
            HnswMetric::L2 | HnswMetric::L1 => f32::NAN,
        }
    }

    /// SQ8 常駐行の復号 dot × f32 クエリの類似度。`InnerProduct` は
    /// `sq8::dot_i8_f32` と同一。
    #[inline]
    pub(crate) fn score_sq8_dequant(self, row: &[i8], scales: &[f32], query: &[f32]) -> f32 {
        match self {
            HnswMetric::InnerProduct | HnswMetric::Cosine => {
                crate::sq8::dot_i8_f32(row, scales, query)
            }
            // 到達不能の防御（L2 は凍結時に F32 へ縮退）。NaN で fail-closed に拒否させる。
            HnswMetric::L2 | HnswMetric::L1 => f32::NAN,
        }
    }

    /// 要求された常駐精度のうち、この metric で実際に使えるもの。L2・L1 は f16・SQ8 用の
    /// カーネルが無いため常に F32 へ縮退する（D6 と同じ考え方。`resident_precision()` が
    /// 実効値を返す）。組み合わせ表の確定は親 #1996 の担当。
    pub(crate) fn effective_precision(
        self,
        requested: super::ResidentPrecision,
    ) -> super::ResidentPrecision {
        match self {
            HnswMetric::L2 | HnswMetric::L1 => super::ResidentPrecision::F32,
            HnswMetric::InnerProduct | HnswMetric::Cosine => requested,
        }
    }

    /// 行・クエリに事前変換が要るか（Cosine のみ正規化）。
    #[inline]
    pub(crate) fn needs_normalization(self) -> bool {
        matches!(self, HnswMetric::Cosine)
    }
}

/// `row` を単位長へ正規化して `out` へ書く（Cosine の C2′。行とクエリで同じ関数を使う）。
///
/// 可否の判定は brute-force 側と同じく `kernel::dot(row, row)` が有限かつ正であること。
/// 値の計算は ISA に依存しない f64 の逐次和（ノルム² → `1/sqrt` → 各成分へ乗算 → f32 丸め）
/// で、同じ入力から常に同じビット列になる。`out` の長さが `row` と違う場合も拒否する。
/// 呼び出し元: 構築（`HnswIndex::build_with_metric` 系の `normalize_rows`）と探索入口。
pub(crate) fn normalize_into(row: &[f32], out: &mut [f32]) -> Result<(), ()> {
    if row.len() != out.len() {
        return Err(());
    }
    let n2_f32 = dot(row, row);
    if !(n2_f32.is_finite() && n2_f32 > 0.0) {
        return Err(());
    }
    let n2: f64 = row.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
    if !(n2.is_finite() && n2 > 0.0) {
        return Err(());
    }
    let inv = 1.0 / n2.sqrt();
    for (o, &v) in out.iter_mut().zip(row.iter()) {
        *o = (f64::from(v) * inv) as f32;
    }
    Ok(())
}

/// 全行を [`normalize_into`] で正規化したバッファを返す。正規化できない最初の行は
/// `NonNormalizableVector { node }`（索引全体を拒否。fail-closed）。確保は
/// `try_reserve_exact` で行う。`vectors` は `validate_build_input` 済み（`dim > 0`・
/// 長さが `dim` の倍数）が前提で、満たさなければ `DimMismatch`。
pub(crate) fn normalize_rows(vectors: &[f32], dim: usize) -> Result<Vec<f32>, HnswError> {
    if dim == 0 || !vectors.len().is_multiple_of(dim) {
        return Err(HnswError::DimMismatch {
            dim: dim as u32,
            len: vectors.len(),
        });
    }
    let mut out: Vec<f32> = Vec::new();
    out.try_reserve_exact(vectors.len())
        .map_err(|_| HnswError::CapacityOverflow)?;
    out.resize(vectors.len(), 0.0);
    for (node, (src, dst)) in vectors
        .chunks_exact(dim)
        .zip(out.chunks_exact_mut(dim))
        .enumerate()
    {
        normalize_into(src, dst).map_err(|()| HnswError::NonNormalizableVector { node })?;
    }
    Ok(out)
}

/// 非有限スコア（`Inf`・`NaN`）を `NonFiniteScore { node }` として拒否する共通
/// ヘルパー（fail-closed。各スコア算出の直後で呼ぶ）。
#[inline]
pub(crate) fn finite_score(score: f32, node: u32) -> Result<f32, HnswError> {
    if score.is_finite() {
        Ok(score)
    } else {
        Err(HnswError::NonFiniteScore { node })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定シードの擬似乱数（テスト専用。非暗号）。
    fn fill(seed: u64, n: usize) -> Vec<f32> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s >> 12;
                s ^= s << 25;
                s ^= s >> 27;
                let v = s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40;
                (v as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
            })
            .collect()
    }

    const DIMS: [usize; 10] = [1, 3, 4, 7, 8, 15, 16, 17, 128, 768];

    /// INDEX-7・SQL-33: 内積 metric は `kernel::dot` とビット同一。
    #[test]
    fn inner_product_f32_is_bit_identical_to_kernel_dot() {
        for &d in &DIMS {
            let a = fill(11 + d as u64, d);
            let b = fill(97 + d as u64, d);
            assert_eq!(
                HnswMetric::InnerProduct.score_f32(&a, &b).to_bits(),
                dot(&a, &b).to_bits(),
                "dim={d}"
            );
        }
    }

    /// f16 常駐行の内積は `dot_f16` とビット同一。
    #[test]
    fn inner_product_f16_is_bit_identical_to_dot_f16() {
        for &d in &DIMS {
            let q = fill(5 + d as u64, d);
            // 指数部が極端にならない有限 f16 ビット列（上位ビットを落として生成）。
            let row: Vec<u16> = fill(7 + d as u64, d)
                .iter()
                .map(|v| ((v.to_bits() >> 16) as u16) & 0x3fff)
                .collect();
            assert_eq!(
                HnswMetric::InnerProduct.score_f16(&row, &q).to_bits(),
                crate::isa::current_f16().dot_f16(&row, &q).to_bits(),
                "dim={d}"
            );
        }
    }

    /// SQ8 復号 dot は `sq8::dot_i8_f32` とビット同一。
    #[test]
    fn inner_product_sq8_dequant_is_bit_identical_to_dot_i8_f32() {
        for &d in &DIMS {
            let q = fill(3 + d as u64, d);
            let scales: Vec<f32> = fill(13 + d as u64, d)
                .iter()
                .map(|v| v.abs() + 0.01)
                .collect();
            let row: Vec<i8> = (0..d)
                .map(|i| ((i * 37 % 255) as i32 - 127) as i8)
                .collect();
            assert_eq!(
                HnswMetric::InnerProduct
                    .score_sq8_dequant(&row, &scales, &q)
                    .to_bits(),
                crate::sq8::dot_i8_f32(&row, &scales, &q).to_bits(),
                "dim={d}"
            );
        }
    }

    /// INDEX-7・SQL-33: L1 metric は `-kernel::l1` とビット同一。
    #[test]
    fn l1_f32_is_bit_identical_to_negated_kernel_l1() {
        for &d in &DIMS {
            let a = fill(21 + d as u64, d);
            let b = fill(55 + d as u64, d);
            assert_eq!(
                HnswMetric::L1.score_f32(&a, &b).to_bits(),
                (-l1(&a, &b)).to_bits(),
                "dim={d}"
            );
        }
    }

    /// L1 の f16・SQ8 の腕は到達不能の防御で NaN を返し、`finite_score` が拒否する。
    #[test]
    fn l1_f16_and_sq8_arms_are_rejected_fail_closed() {
        let q = [0.5f32, -0.25, 1.0, 2.0];
        let s16 = HnswMetric::L1.score_f16(&[0u16; 4], &q);
        let s8 = HnswMetric::L1.score_sq8_dequant(&[1i8; 4], &[1.0; 4], &q);
        for s in [s16, s8] {
            assert!(s.is_nan());
            assert!(matches!(
                finite_score(s, 3),
                Err(HnswError::NonFiniteScore { node: 3 })
            ));
        }
    }

    /// 非有限スコアは `NonFiniteScore { node }` で拒否し、有限値は素通しする。
    #[test]
    fn finite_score_rejects_non_finite() {
        for bad in [f32::INFINITY, f32::NEG_INFINITY, f32::NAN] {
            assert!(matches!(
                finite_score(bad, 9),
                Err(HnswError::NonFiniteScore { node: 9 })
            ));
        }
        assert_eq!(finite_score(1.5, 0).ok(), Some(1.5));
    }
}
