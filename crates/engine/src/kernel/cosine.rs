//! コサイン距離カーネルと brute-force Top-k（Issue #1987。ポインタ: SQL-33・TASK-237／238。
//! 方式の比較は #1986 の ADR（Proposed）を参照）。
//!
//! 責務境界: 正規化していない入力でも `1 - cos` を返す距離計算と、距離昇順の総当たり
//! Top-k を提供する。内積経路（`kernel.rs::CpuScalarProvider`、スコア降順・f32）とは
//! 別系統で、距離は昇順・f64 で扱う。`<=>` の実行は #1991 でこのカーネルへ切り替わり、
//! `SearchProvider::search_cosine`／`search_cosine_subset` の既定実装経由で `sql::exec` の
//! DISTANCE 段が呼ぶ。HNSW／GPU の metric 対応は #1996〜#2005 の担当。
//!
//! 計算式は [`cosine_distance_from_parts`] に一本化する。内積と各ノルム² は
//! `kernel::dot`（ISA 別 SIMD）で f32 累積し、除算以降を f64 で行う。行ノルム²は
//! 外から渡された値（C2）でも、その場の `dot(x, x)`（C1）でも同じ関数・同じ加算順序
//! なので、両経路の結果はビット一致する。
//!
//! ゼロノルム（Issue #2240。ポインタ: SQL-33 (3)）: ゼロノルムの行・クエリの距離は NaN
//! になり、エラーにも除外にもせず昇順の末尾（全数値より後ろ）に並べる。NaN 同士は
//! 順序キー昇順で、NaN 行も `k` の枠を消費する（pgvector の `LIMIT k` と同じ）。
//! クエリがゼロノルムなら全行 NaN で、順序キー昇順の先頭 `k` 行が返る。一方クエリの
//! 非有限成分は入力不正として従来どおり `NonFiniteQuery` で拒否する（fail-closed）。

use std::collections::{BinaryHeap, TryReserveError};

use super::{dot, dot_block4, KernelError, SubsetSearchInput};
use crate::search_order::{self, TieKey};

/// 距離付きの検索結果 1 件。`id` は [`CosineSearchInput::ids`] の値がそのまま返る。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DistanceHit {
    pub id: u64,
    pub distance: f64,
}

/// [`cosine_top_k`] の入力ビュー。`SearchInput` と同じく呼び出し元が可視行だけへ
/// 絞り込んだスライスを借用で渡す契約（テナント境界は呼び出し元で完結している）。
pub struct CosineSearchInput<'a> {
    /// `vectors` の行と 1 対 1 の識別子。
    pub ids: &'a [u64],
    /// `ids.len() * dim` 要素のフラット化済み行列。
    pub vectors: &'a [f32],
    /// 行ごとのノルム²（C2）。[`row_norms_sq`] の出力。長さが `ids.len()` と
    /// 一致しない場合は無視して全行をその場で計算する（C1。結果はビット一致）。
    pub row_norms_sq: Option<&'a [f32]>,
    /// ベクトル次元。
    pub dim: u32,
    /// クエリベクトル。
    pub query: &'a [f32],
    /// 返す件数の上限。
    pub k: usize,
}

/// 内積と各ノルム²からコサイン距離を求める唯一の実装。
///
/// 類似度を f64 で計算して [-1, 1] にクランプし `1 - s` を返す。NaN（ゼロノルム・
/// 次元 0・非有限入力）は NaN のまま返る。f32 でのオーバーフロー（ノルム² や内積が
/// inf）も fail-closed で NaN とし、昇順の末尾へ回す（距離 1.0 への誤収束を防ぐ）。
pub fn cosine_distance_from_parts(dot: f32, a_norm_sq: f32, b_norm_sq: f32) -> f64 {
    if !dot.is_finite() || !a_norm_sq.is_finite() || !b_norm_sq.is_finite() {
        return f64::NAN;
    }
    // f32 アンダーフローでノルム² が 0 になった行は、内積が正でも除算が +Inf となり
    // クランプで距離 0 に化けるため、除算前に正値を要求して NaN（除外）にする。
    if a_norm_sq <= 0.0 || b_norm_sq <= 0.0 {
        return f64::NAN;
    }
    let denom = (f64::from(a_norm_sq) * f64::from(b_norm_sq)).sqrt();
    let s = f64::from(dot) / denom;
    1.0 - s.clamp(-1.0, 1.0)
}

/// 2 本のベクトルのコサイン距離（C1。ノルム²をその場で計算する）。
pub fn cosine_distance(a: &[f32], b: &[f32]) -> f64 {
    cosine_distance_from_parts(dot(a, b), dot(a, a), dot(b, b))
}

/// 各行のノルム² を `dot(x, x)` で求める（C2 の入力）。確保は `try_reserve_exact`。
pub fn row_norms_sq(vectors: &[f32], dim: u32) -> Result<Vec<f32>, TryReserveError> {
    let dim = dim as usize;
    let rows = vectors.len().checked_div(dim).unwrap_or(0);
    let mut out = Vec::new();
    out.try_reserve_exact(rows)?;
    if dim > 0 {
        out.extend(vectors.chunks_exact(dim).map(|row| dot(row, row)));
    }
    Ok(out)
}

/// 最大ヒープ用の要素（距離が大きいほど「劣る」＝ヒープ頂点）。
struct HeapItem(DistanceHit);
impl HeapItem {
    fn tie(&self) -> TieKey {
        search_order::tie_key_for_response_id(self.0.id)
    }
}
impl PartialEq for HeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for HeapItem {}
impl PartialOrd for HeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for HeapItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        search_order::cmp_distance_asc_then_tie(
            self.0.distance,
            self.tie(),
            other.0.distance,
            other.tie(),
        )
    }
}

/// 距離昇順（同点は順序キー昇順）の Top-k 選出器。仕組みは `TopKSelector` と同じで、
/// `k` による事前確保はしない。NaN は受理して比較器に任せ（末尾に並ぶ）、
/// ISA 依存の符号ビットを持ち込まないよう push 時に正規の NaN へ揃える（Issue #2240）。
pub(crate) struct AscDistanceTopK {
    k: usize,
    heap: BinaryHeap<HeapItem>,
}

impl AscDistanceTopK {
    pub(crate) fn new(k: usize) -> Self {
        Self {
            k,
            heap: BinaryHeap::new(),
        }
    }

    pub(crate) fn push(&mut self, mut hit: DistanceHit) {
        if self.k == 0 {
            return;
        }
        if hit.distance.is_nan() {
            hit.distance = f64::NAN;
        }
        let item = HeapItem(hit);
        if self.heap.len() < self.k {
            self.heap.push(item);
        } else if let Some(worst) = self.heap.peek() {
            if item < *worst {
                self.heap.pop();
                self.heap.push(item);
            }
        }
    }

    /// 距離昇順（同点は id 昇順）で確定して返す。整列は安定ソートのみ。
    pub(crate) fn into_sorted_vec(self) -> Vec<DistanceHit> {
        let mut out: Vec<DistanceHit> = self.heap.into_iter().map(|i| i.0).collect();
        out.sort_by(|a, b| {
            search_order::cmp_distance_asc_then_tie(
                a.distance,
                search_order::tie_key_for_response_id(a.id),
                b.distance,
                search_order::tie_key_for_response_id(b.id),
            )
        });
        out
    }
}

/// コサイン距離の brute-force Top-k（距離昇順・同点は id 昇順）。
///
/// 検証順は `CpuScalarProvider::search` と同じ: 次元不一致は `DimMismatch`、
/// クエリの非有限値は `NonFiniteQuery`、`k == 0` か行 0 件なら空。範囲外の行は
/// skip する。距離が NaN の行（ゼロノルム等）は除外せず末尾に並べる。
pub fn cosine_top_k(input: CosineSearchInput<'_>) -> Result<Vec<DistanceHit>, KernelError> {
    let dim = input.dim as usize;
    if input.query.len() != dim {
        return Err(KernelError::DimMismatch {
            expected: input.dim,
            found: input.query.len(),
        });
    }
    if input.query.iter().any(|v| !v.is_finite()) {
        return Err(KernelError::NonFiniteQuery);
    }
    if input.k == 0 || input.ids.is_empty() {
        return Ok(Vec::new());
    }

    let q_norm_sq = dot(input.query, input.query);
    let norms = input.row_norms_sq.filter(|n| n.len() == input.ids.len());
    let row = |idx: usize| -> Option<&[f32]> {
        let start = idx.checked_mul(dim)?;
        input.vectors.get(start..start.checked_add(dim)?)
    };
    let row_norm = |idx: usize, v: &[f32]| -> f32 {
        norms
            .and_then(|n| n.get(idx).copied())
            .unwrap_or_else(|| dot(v, v))
    };

    let mut selector = AscDistanceTopK::new(input.k);
    let mut base = 0usize;
    for ids4 in input.ids.chunks(4) {
        // 4 行そろって範囲内のときだけブロックカーネルへ回す。端数・範囲外を含む塊は 1 行ずつ。
        let rows4 = if ids4.len() == 4 {
            match (row(base), row(base + 1), row(base + 2), row(base + 3)) {
                (Some(r0), Some(r1), Some(r2), Some(r3)) => Some([r0, r1, r2, r3]),
                _ => None,
            }
        } else {
            None
        };
        if let Some(rows) = rows4 {
            let dots = dot_block4(rows, input.query);
            for (i, (&id, v)) in ids4.iter().zip(rows).enumerate() {
                let d = cosine_distance_from_parts(
                    dots.get(i).copied().unwrap_or(f32::NAN),
                    row_norm(base + i, v),
                    q_norm_sq,
                );
                selector.push(DistanceHit { id, distance: d });
            }
        } else {
            for (i, &id) in ids4.iter().enumerate() {
                let Some(v) = row(base + i) else { continue };
                let d = cosine_distance_from_parts(
                    dot(v, input.query),
                    row_norm(base + i, v),
                    q_norm_sq,
                );
                selector.push(DistanceHit { id, distance: d });
            }
        }
        base += ids4.len();
    }
    Ok(selector.into_sorted_vec())
}

/// 行列全体を借用したまま `input.slots` の行だけを候補にするコサイン距離 Top-k（Issue #1991。
/// マスク経路〔スカラー索引の候補削減〕用）。返る [`DistanceHit::id`] は `slots[i] as u64`。
///
/// 行ごとの距離は [`cosine_top_k`] と同じ式（[`cosine_distance_from_parts`]）・同じ `dot` で
/// 求める。`dot_block4` は 1 行版の `dot` とビット同一の契約なので、同じ行集合を
/// [`cosine_top_k`] に渡した結果とビット一致する。範囲外の行は skip する（検証順も同じ）。
pub fn cosine_top_k_slots(input: SubsetSearchInput<'_>) -> Result<Vec<DistanceHit>, KernelError> {
    let dim = input.dim as usize;
    if input.query.len() != dim {
        return Err(KernelError::DimMismatch {
            expected: input.dim,
            found: input.query.len(),
        });
    }
    if input.query.iter().any(|v| !v.is_finite()) {
        return Err(KernelError::NonFiniteQuery);
    }
    if input.k == 0 || input.slots.is_empty() {
        return Ok(Vec::new());
    }
    let q_norm_sq = dot(input.query, input.query);
    let mut selector = AscDistanceTopK::new(input.k);
    for &slot in input.slots {
        let Some(start) = (slot as usize).checked_mul(dim) else {
            continue;
        };
        let Some(v) = start
            .checked_add(dim)
            .and_then(|end| input.vectors.get(start..end))
        else {
            continue;
        };
        let d = cosine_distance_from_parts(dot(v, input.query), dot(v, v), q_norm_sq);
        selector.push(DistanceHit {
            id: u64::from(slot),
            distance: d,
        });
    }
    Ok(selector.into_sorted_vec())
}

/// 最大ヒープ用の要素（遠い順の選出で、距離が小さいほど「劣る」＝ヒープ頂点）。
struct FarHeapItem(DistanceHit);
impl FarHeapItem {
    fn tie(&self) -> TieKey {
        search_order::tie_key_for_response_id(self.0.id)
    }
}
impl PartialEq for FarHeapItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}
impl Eq for FarHeapItem {}
impl PartialOrd for FarHeapItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for FarHeapItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        search_order::cmp_distance_desc_then_tie(
            self.0.distance,
            self.tie(),
            other.0.distance,
            other.tie(),
        )
    }
}

/// 距離の降順（遠い順・`ORDER BY <=> DESC`。Issue #1991・SQL-33 (2)(3)）の Top-k 選出。
///
/// NaN（ゼロノルム）は最も遠いものとして先頭に置き、`k` の枠を消費する。同点は順序キー昇順。
/// 最大ヒープで `O(n log k)`・保持は高々 `k` 件。事前確保は `k` に依存させない
/// （[`AscDistanceTopK`] と同じ理由）。NaN は ISA 依存の符号を持ち込まないよう正規の NaN へ揃える。
pub(crate) fn select_farthest_distance_k(
    candidates: impl Iterator<Item = DistanceHit>,
    k: usize,
) -> Vec<DistanceHit> {
    if k == 0 {
        return Vec::new();
    }
    let mut heap: BinaryHeap<FarHeapItem> = BinaryHeap::new();
    for mut hit in candidates {
        if hit.distance.is_nan() {
            hit.distance = f64::NAN;
        }
        let item = FarHeapItem(hit);
        if heap.len() < k {
            heap.push(item);
        } else if let Some(worst) = heap.peek() {
            if item < *worst {
                heap.pop();
                heap.push(item);
            }
        }
    }
    let mut out: Vec<DistanceHit> = heap.into_iter().map(|i| i.0).collect();
    out.sort_by(|a, b| {
        search_order::cmp_distance_desc_then_tie(
            a.distance,
            search_order::tie_key_for_response_id(a.id),
            b.distance,
            search_order::tie_key_for_response_id(b.id),
        )
    });
    out
}

/// 行のノルムの逆数（正規化の係数）。可否の判定は [`normalize_into`] と
/// `hnsw::is_cosine_indexable` で共有する単一の実装（両者の判定が食い違うと、索引から除く行の
/// 集合と全走査で NaN になる行の集合がずれるため）。
pub(crate) fn inverse_norm(row: &[f32]) -> Option<f64> {
    let n2_f32 = dot(row, row);
    if !(n2_f32.is_finite() && n2_f32 > 0.0) {
        return None;
    }
    let n2: f64 = row.iter().map(|&v| f64::from(v) * f64::from(v)).sum();
    if !(n2.is_finite() && n2 > 0.0) {
        return None;
    }
    Some(1.0 / n2.sqrt())
}

/// `row` を単位長へ正規化して `out` へ書く（Cosine の C2′。行とクエリで同じ関数を使う）。
///
/// 可否の判定は brute-force 側と同じく `kernel::dot(row, row)` が有限かつ正であること。
/// 値の計算は ISA に依存しない f64 の逐次和（ノルム² → `1/sqrt` → 各成分へ乗算 → f32 丸め）
/// で、同じ入力から常に同じビット列になる。`out` の長さが `row` と違う場合も拒否する。
/// 呼び出し元: HNSW の構築・探索入口（`hnsw::metric` から再エクスポート）と、GPU バッチの
/// 常駐行列・クエリ正規化（Issue #2003。`batch_search`／`gpu_batch`）。
pub(crate) fn normalize_into(row: &[f32], out: &mut [f32]) -> Result<(), ()> {
    if row.len() != out.len() {
        return Err(());
    }
    let inv = inverse_norm(row).ok_or(())?;
    for (o, &v) in out.iter_mut().zip(row.iter()) {
        *o = (f64::from(v) * inv) as f32;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(
        ids: &'a [u64],
        vectors: &'a [f32],
        dim: u32,
        query: &'a [f32],
        k: usize,
    ) -> CosineSearchInput<'a> {
        CosineSearchInput {
            ids,
            vectors,
            row_norms_sq: None,
            dim,
            query,
            k,
        }
    }

    #[test]
    fn overflowing_norm_yields_nan() {
        // 成分 2e19 の二乗は f32 で inf になる。
        let big = [2.0e19_f32, 0.0];
        assert!(cosine_distance(&big, &[1.0, 0.0]).is_nan());
        assert!(cosine_distance(&[1.0, 0.0], &big).is_nan());
        let ids = [1_u64, 2];
        let vectors = [2.0e19_f32, 0.0, 1.0, 0.0];
        let r = cosine_top_k(input(&ids, &vectors, 2, &[1.0, 0.0], 2)).unwrap();
        assert_eq!(r.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2, 1]);
        assert!(r[1].distance.is_nan());
    }

    #[test]
    fn underflowing_norm_yields_nan() {
        // 1e-30 の二乗は f32 で 0 にアンダーフローする（内積は正の有限値のまま）。
        let tiny = [1e-30_f32, 1e-30];
        assert!(cosine_distance(&tiny, &[1.0, 0.0]).is_nan());
        let ids = [1_u64, 2];
        let vectors = [1e-30_f32, 1e-30, 1.0, 0.0];
        let r = cosine_top_k(input(&ids, &vectors, 2, &[1.0, 0.0], 2)).unwrap();
        assert_eq!(r.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2, 1]);
        assert!(r[1].distance.is_nan());
    }

    #[test]
    fn basic_distances() {
        assert!(cosine_distance(&[2.0, 0.0], &[1.0, 0.0]).abs() < 1e-12);
        assert!((cosine_distance(&[1.0, 0.0], &[0.0, 3.0]) - 1.0).abs() < 1e-12);
        assert!((cosine_distance(&[1.0, 0.0], &[-5.0, 0.0]) - 2.0).abs() < 1e-12);
        assert!(cosine_distance(&[0.0, 0.0], &[1.0, 0.0]).is_nan());
        assert!(cosine_distance(&[], &[]).is_nan());
    }

    #[test]
    fn clamped_into_range() {
        let a = [0.1f32, 0.2, 0.3, 0.7];
        let b = [100.0f32, 200.0, 300.0, 700.0];
        let d = cosine_distance(&a, &b);
        assert!((0.0..=2.0).contains(&d));
    }

    #[test]
    fn row_norms_match_dot() {
        let v = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
        let n = row_norms_sq(&v, 3).unwrap();
        assert_eq!(n.len(), 2);
        assert_eq!(n[0].to_bits(), dot(&v[..3], &v[..3]).to_bits());
        assert_eq!(n[1].to_bits(), dot(&v[3..], &v[3..]).to_bits());
        assert!(row_norms_sq(&v, 0).unwrap().is_empty());
    }

    #[test]
    fn selector_ties_truncation_and_nan() {
        let mut s = AscDistanceTopK::new(2);
        for (id, d) in [(5, 0.5), (3, 0.5), (9, 0.1), (1, f64::NAN), (2, 0.9)] {
            s.push(DistanceHit { id, distance: d });
        }
        let out = s.into_sorted_vec();
        assert_eq!(out.iter().map(|h| h.id).collect::<Vec<_>>(), vec![9, 3]);
        let mut z = AscDistanceTopK::new(0);
        z.push(DistanceHit {
            id: 1,
            distance: 0.0,
        });
        assert!(z.into_sorted_vec().is_empty());

        // k=5 なら NaN の id 1 も末尾に残る。
        let mut s = AscDistanceTopK::new(5);
        for (id, d) in [(5, 0.5), (3, 0.5), (9, 0.1), (1, f64::NAN), (2, 0.9)] {
            s.push(DistanceHit { id, distance: d });
        }
        let out = s.into_sorted_vec();
        assert_eq!(
            out.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![9, 3, 5, 2, 1]
        );
    }

    #[test]
    fn selector_nan_consumes_k_and_normalizes_sign() {
        let neg_nan = f64::from_bits(0xFFF8_0000_0000_0000);
        let mut s = AscDistanceTopK::new(4);
        for (id, d) in [
            (7, neg_nan),
            (3, 0.2),
            (5, f64::NAN),
            (1, neg_nan),
            (4, 0.1),
        ] {
            s.push(DistanceHit { id, distance: d });
        }
        let out = s.into_sorted_vec();
        assert_eq!(
            out.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![4, 3, 1, 5]
        );
        for h in &out[2..] {
            assert_eq!(h.distance.to_bits(), f64::NAN.to_bits());
        }
        // 有限行が k 以上なら NaN 行は出ない。
        let mut s = AscDistanceTopK::new(2);
        for (id, d) in [(1, f64::NAN), (2, 0.3), (3, 0.4)] {
            s.push(DistanceHit { id, distance: d });
        }
        assert_eq!(
            s.into_sorted_vec().iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![2, 3]
        );
    }

    #[test]
    fn zero_rows_in_block_and_tail_paths_sort_last() {
        // 9 行（4 行ブロック 2 つ＋端数 1）。id 2 はブロック内、id 7 は 2 つ目のブロック内。
        let dim = 2usize;
        let ids: Vec<u64> = (1..=9).collect();
        let mut vectors = Vec::new();
        for id in &ids {
            if *id == 2 || *id == 7 {
                vectors.extend_from_slice(&[0.0f32, 0.0]);
            } else {
                vectors.extend_from_slice(&[1.0f32, *id as f32 * 0.1]);
            }
        }
        let out = cosine_top_k(input(&ids, &vectors, dim as u32, &[1.0, 0.0], 9)).unwrap();
        let order: Vec<u64> = out.iter().map(|h| h.id).collect();
        assert_eq!(order, vec![1, 3, 4, 5, 6, 8, 9, 2, 7]);
        assert!(out[7..]
            .iter()
            .all(|h| h.distance.to_bits() == f64::NAN.to_bits()));
        // C2（行ノルム²を外から渡す）でも同じ。
        let norms = row_norms_sq(&vectors, dim as u32).unwrap();
        let mut inp = input(&ids, &vectors, dim as u32, &[1.0, 0.0], 9);
        inp.row_norms_sq = Some(&norms);
        let c2 = cosine_top_k(inp).unwrap();
        assert_eq!(c2.iter().map(|h| h.id).collect::<Vec<_>>(), order);
    }

    #[test]
    fn top_k_unnormalized_and_errors() {
        // 行 10 は大きさが違うが同方向（距離 0）。
        let vectors = [3.0f32, 0.0, 0.0, 1.0, 1.0, 1.0];
        let ids = [10u64, 11, 12];
        let q = [1.0f32, 0.0];
        let out = cosine_top_k(input(&ids, &vectors, 2, &q, 2)).unwrap();
        assert_eq!(out[0].id, 10);
        assert!(out[0].distance.abs() < 1e-12);
        // 行 11 はゼロノルムでない [0,1]（距離 1）、行 12 は [1,1]（距離約 0.29）。
        assert_eq!(out[1].id, 12);

        assert_eq!(
            cosine_top_k(input(&ids, &vectors, 2, &[1.0], 2)),
            Err(KernelError::DimMismatch {
                expected: 2,
                found: 1
            })
        );
        assert_eq!(
            cosine_top_k(input(&ids, &vectors, 2, &[f32::NAN, 0.0], 2)),
            Err(KernelError::NonFiniteQuery)
        );
        assert!(cosine_top_k(input(&ids, &vectors, 2, &q, 0))
            .unwrap()
            .is_empty());
        assert!(cosine_top_k(input(&[], &[], 2, &q, 3)).unwrap().is_empty());
        // ゼロノルムのクエリは全行 NaN になり、順序キー昇順（id 昇順）で k 件返る。
        let z = cosine_top_k(input(&ids, &vectors, 2, &[0.0, 0.0], 3)).unwrap();
        assert_eq!(z.iter().map(|h| h.id).collect::<Vec<_>>(), vec![10, 11, 12]);
        assert!(z.iter().all(|h| h.distance.is_nan()));
        let z2 = cosine_top_k(input(&ids, &vectors, 2, &[0.0, 0.0], 2)).unwrap();
        assert_eq!(z2.iter().map(|h| h.id).collect::<Vec<_>>(), vec![10, 11]);
    }

    #[test]
    fn skips_out_of_range_rows_and_sorts_zero_norm_rows_last() {
        // 5 行分の id に対し 4 行分しかベクトルが無い（id 5 は範囲外）。id 2 はゼロノルム。
        let vectors = [1.0f32, 0.0, 0.0, 0.0, 0.0, 1.0, 1.0, 1.0];
        let ids = [1u64, 2, 3, 4, 5];
        let out = cosine_top_k(input(&ids, &vectors, 2, &[1.0, 0.0], 10)).unwrap();
        assert_eq!(
            out.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![1, 4, 3, 2]
        );
        assert!(out[3].distance.is_nan());
    }

    #[test]
    fn c1_c2_bit_identical_and_bad_norm_len_falls_back() {
        let dim = 5usize;
        let ids: Vec<u64> = (0..11).collect();
        let vectors: Vec<f32> = (0..11 * dim)
            .map(|i| ((i * 7919 % 101) as f32 - 50.0) * 0.37)
            .collect();
        let q = [0.3f32, -1.2, 2.2, 0.5, -0.7];
        let c1 = cosine_top_k(input(&ids, &vectors, dim as u32, &q, 11)).unwrap();
        let norms = row_norms_sq(&vectors, dim as u32).unwrap();
        let mut inp = input(&ids, &vectors, dim as u32, &q, 11);
        inp.row_norms_sq = Some(&norms);
        let c2 = cosine_top_k(inp).unwrap();
        let mut inp = input(&ids, &vectors, dim as u32, &q, 11);
        inp.row_norms_sq = Some(&norms[..3]);
        let c3 = cosine_top_k(inp).unwrap();
        for other in [&c2, &c3] {
            assert_eq!(c1.len(), other.len());
            for (a, b) in c1.iter().zip(other.iter()) {
                assert_eq!(a.id, b.id);
                assert_eq!(a.distance.to_bits(), b.distance.to_bits());
            }
        }
    }

    #[test]
    fn slots_variant_matches_gathered_cosine_top_k_bitwise() {
        let dim = 3_u32;
        let vectors: Vec<f32> = (0..7 * 3).map(|i| ((i * 7 % 11) as f32) - 4.0).collect();
        let slots = [0_u32, 2, 3, 5, 6, 99];
        let query = [0.5_f32, -1.0, 2.0];
        let got = cosine_top_k_slots(SubsetSearchInput {
            slots: &slots,
            vectors: &vectors,
            dim,
            query: &query,
            k: 4,
        })
        .unwrap();
        let mut gathered = Vec::new();
        let mut ids = Vec::new();
        for &s in &slots[..5] {
            gathered.extend_from_slice(&vectors[s as usize * 3..s as usize * 3 + 3]);
            ids.push(u64::from(s));
        }
        let want = cosine_top_k(input(&ids, &gathered, dim, &query, 4)).unwrap();
        assert_eq!(got.len(), want.len());
        for (g, w) in got.iter().zip(&want) {
            assert_eq!(g.id, w.id);
            assert_eq!(g.distance.to_bits(), w.distance.to_bits());
        }
        // 検証順: 次元不一致・非有限クエリ・k=0。
        let bad = SubsetSearchInput {
            slots: &slots,
            vectors: &vectors,
            dim,
            query: &[1.0],
            k: 1,
        };
        assert!(matches!(
            cosine_top_k_slots(bad),
            Err(KernelError::DimMismatch { .. })
        ));
        let nf = SubsetSearchInput {
            slots: &slots,
            vectors: &vectors,
            dim,
            query: &[1.0, f32::NAN, 0.0],
            k: 1,
        };
        assert!(matches!(
            cosine_top_k_slots(nf),
            Err(KernelError::NonFiniteQuery)
        ));
        let zero_k = SubsetSearchInput {
            slots: &slots,
            vectors: &vectors,
            dim,
            query: &query,
            k: 0,
        };
        assert!(cosine_top_k_slots(zero_k).unwrap().is_empty());
    }

    #[test]
    fn farthest_selector_puts_nan_first_with_tie_ascending() {
        let hits = [(5, 0.5), (3, 0.5), (9, 0.1), (1, f64::NAN), (2, 0.9)]
            .map(|(id, distance)| DistanceHit { id, distance });
        let out = select_farthest_distance_k(hits.into_iter(), 4);
        assert_eq!(
            out.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![1, 2, 3, 5]
        );
        assert!(select_farthest_distance_k(hits.into_iter(), 0).is_empty());
        let all = select_farthest_distance_k(hits.into_iter(), 10);
        assert_eq!(
            all.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![1, 2, 3, 5, 9]
        );
    }
}
