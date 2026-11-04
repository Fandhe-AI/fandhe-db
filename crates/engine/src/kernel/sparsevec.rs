//! 疎ベクトル（非ゼロ要素のみ保持）の L2・負の内積・コサイン距離と brute-force Top-k
//! （Issue #2026。親 #2024・ポインタ: TABLE-47）。
//!
//! 責務境界: 検証済み借用ビュー [`SparseVecRef`] の構築（fail-closed）、index 昇順の
//! マージ走査による距離計算、距離昇順の Top-k を受け持つ。列型・DDL・保存・入出力は
//! #2025、非ゼロ数の上限（CLI 設定）は #2027 の担当で、本モジュールは保存形式に依存しない
//! 純関数カーネルである。BM25 用の `sparse.rs`（語彙ベースの疎表現）とは無関係。
//!
//! 数値規約: 累積は f32、加算順は index 昇順、積と差は個別演算（`mul_add` 不使用）。
//! 密化した参照計算（[`crate::isa::dot_scalar`]・[`crate::isa::l2_sq_scalar`]）とビット一致する。
//! コサインは [`cosine_distance_from_parts`] を再利用し、ゼロノルム・オーバーフローは NaN。
//! SIMD 版は設けない（マージ走査は分岐が多く、走査は確保なしの O(nnz_a + nnz_b)）。
//!
//! エラー契約: 公開 API 破壊を避けるため `KernelError` に variant を足さず、構造不正・
//! 非有限値・オペランド間の次元不一致はすべて [`KernelError::DimMismatch`] で返す
//! （`expected` は次元、`found` は問題の値または位置）。
//!
//! 現時点では SQL 経路から呼ばれない（`<->`／`<#>`／`<=>` への接続は別 Issue）。
//! 呼び出し元が RLS で絞った行を渡す契約で、テナント境界の検査は本モジュールの外で完結する。

use std::cmp::Ordering;

use super::cosine::{AscDistanceTopK, DistanceHit};
use super::{cosine_distance_from_parts, l2_distance_from_sq, KernelError};

/// 検証済みの疎ベクトル借用ビュー。index は 0 始まりで狭義単調増加、値は有限。
///
/// フィールドは非公開で、[`SparseVecRef::new`] の検証を通らない値は作れない。
/// 明示的な 0.0 は受理する（結果は変わらない。除去は #2025 の入力側の担当）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SparseVecRef<'a> {
    dim: u32,
    indices: &'a [u32],
    values: &'a [f32],
}

impl<'a> SparseVecRef<'a> {
    /// 構造を検証してビューを作る。長さ不一致・index の非単調（重複含む）・`dim` 以上の
    /// index・非有限値はいずれも [`KernelError::DimMismatch`]（fail-closed）。
    pub fn new(dim: u32, indices: &'a [u32], values: &'a [f32]) -> Result<Self, KernelError> {
        if indices.len() != values.len() {
            return Err(KernelError::DimMismatch {
                expected: dim,
                found: values.len(),
            });
        }
        let mut prev: Option<u32> = None;
        for (pos, (&idx, &v)) in indices.iter().zip(values).enumerate() {
            if idx >= dim {
                return Err(KernelError::DimMismatch {
                    expected: dim,
                    found: idx as usize,
                });
            }
            if prev.is_some_and(|p| idx <= p) || !v.is_finite() {
                return Err(KernelError::DimMismatch {
                    expected: dim,
                    found: pos,
                });
            }
            prev = Some(idx);
        }
        Ok(Self {
            dim,
            indices,
            values,
        })
    }

    /// 次元数。
    pub fn dim(&self) -> u32 {
        self.dim
    }

    /// 保持された要素数。
    pub fn nnz(&self) -> usize {
        self.indices.len()
    }

    /// index 列（0 始まり・狭義昇順）。
    pub fn indices(&self) -> &'a [u32] {
        self.indices
    }

    /// 値列（`indices` と 1 対 1）。
    pub fn values(&self) -> &'a [f32] {
        self.values
    }
}

/// 距離の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SparseVecMetric {
    /// ユークリッド距離。
    L2,
    /// 負の内積 `-(a·q)`（pgvector の `<#>` と同じ符号）。
    NegativeInnerProduct,
    /// コサイン距離（ゼロノルムは NaN で昇順の末尾）。
    Cosine,
}

/// [`sparsevec_top_k`] の入力ビュー。呼び出し元が可視行だけへ絞った借用を渡す契約。
pub struct SparseVecSearchInput<'a> {
    /// `rows` と 1 対 1 の識別子（長さが違えば対応のある範囲だけを走査する）。
    pub ids: &'a [u64],
    /// 検証済みの行ビュー。
    pub rows: &'a [SparseVecRef<'a>],
    /// クエリ。
    pub query: SparseVecRef<'a>,
    /// 返す件数の上限。
    pub k: usize,
}

fn check_same_dim(a: &SparseVecRef<'_>, b: &SparseVecRef<'_>) -> Result<(), KernelError> {
    if a.dim != b.dim {
        return Err(KernelError::DimMismatch {
            expected: a.dim,
            found: b.dim as usize,
        });
    }
    Ok(())
}

/// 共通 index の積和（index 昇順の f32 累積）。
fn merge_dot(a: &SparseVecRef<'_>, b: &SparseVecRef<'_>) -> f32 {
    let (mut i, mut j) = (0usize, 0usize);
    let mut acc = 0.0f32;
    while let (Some(&ia), Some(&ib), Some(&va), Some(&vb)) = (
        a.indices.get(i),
        b.indices.get(j),
        a.values.get(i),
        b.values.get(j),
    ) {
        match ia.cmp(&ib) {
            Ordering::Less => i += 1,
            Ordering::Greater => j += 1,
            Ordering::Equal => {
                acc += va * vb;
                i += 1;
                j += 1;
            }
        }
    }
    acc
}

/// 二乗 L2。片方だけの index は `x*x`（`(x-0)²` と f32 で厳密に一致）。
fn merge_l2_sq(a: &SparseVecRef<'_>, b: &SparseVecRef<'_>) -> f32 {
    let (mut i, mut j) = (0usize, 0usize);
    let mut acc = 0.0f32;
    loop {
        match (
            a.indices.get(i).zip(a.values.get(i)),
            b.indices.get(j).zip(b.values.get(j)),
        ) {
            (None, None) => break,
            (Some((_, &va)), None) => {
                acc += va * va;
                i += 1;
            }
            (None, Some((_, &vb))) => {
                acc += vb * vb;
                j += 1;
            }
            (Some((&ia, &va)), Some((&ib, &vb))) => match ia.cmp(&ib) {
                Ordering::Less => {
                    acc += va * va;
                    i += 1;
                }
                Ordering::Greater => {
                    acc += vb * vb;
                    j += 1;
                }
                Ordering::Equal => {
                    let d = va - vb;
                    acc += d * d;
                    i += 1;
                    j += 1;
                }
            },
        }
    }
    acc
}

fn norm_sq(a: &SparseVecRef<'_>) -> f32 {
    a.values.iter().fold(0.0f32, |acc, &x| acc + x * x)
}

/// 内積（f32）。次元不一致は `DimMismatch`。
pub fn sparsevec_dot(a: &SparseVecRef<'_>, b: &SparseVecRef<'_>) -> Result<f32, KernelError> {
    check_same_dim(a, b)?;
    Ok(merge_dot(a, b))
}

/// 負の内積 `-(a·b)`（f64。`<#>` と同じ符号）。
pub fn sparsevec_negative_inner_product(
    a: &SparseVecRef<'_>,
    b: &SparseVecRef<'_>,
) -> Result<f64, KernelError> {
    Ok(-f64::from(sparsevec_dot(a, b)?))
}

/// 二乗 L2 距離（f32）。
pub fn sparsevec_l2_sq(a: &SparseVecRef<'_>, b: &SparseVecRef<'_>) -> Result<f32, KernelError> {
    check_same_dim(a, b)?;
    Ok(merge_l2_sq(a, b))
}

/// L2 距離（f64。平方根は 1 回だけ取る）。
pub fn sparsevec_l2_distance(
    a: &SparseVecRef<'_>,
    b: &SparseVecRef<'_>,
) -> Result<f64, KernelError> {
    Ok(l2_distance_from_sq(sparsevec_l2_sq(a, b)?))
}

/// コサイン距離（f64。ゼロノルム・オーバーフローは NaN）。
pub fn sparsevec_cosine_distance(
    a: &SparseVecRef<'_>,
    b: &SparseVecRef<'_>,
) -> Result<f64, KernelError> {
    check_same_dim(a, b)?;
    Ok(cosine_distance_from_parts(
        merge_dot(a, b),
        norm_sq(a),
        norm_sq(b),
    ))
}

/// 疎ベクトルの総当たり Top-k（距離昇順・同点 id 昇順・NaN は末尾）。
///
/// `k == 0`／行 0 件は空を返し、クエリと `dim` の違う行は skip する（クエリ自身の整合は
/// [`SparseVecRef::new`] で保証済み）。f32 オーバーフローによる非有限距離は NaN に
/// 正規化して末尾へ回す（−Inf が先頭に来ることを防ぐ fail-closed）。
#[allow(dead_code)] // SQL 接続（別 Issue）までは単体テストのみが呼び出し元
pub fn sparsevec_top_k(
    input: SparseVecSearchInput<'_>,
    metric: SparseVecMetric,
) -> Result<Vec<DistanceHit>, KernelError> {
    let q = &input.query;
    if input.k == 0 || input.ids.is_empty() || input.rows.is_empty() {
        return Ok(Vec::new());
    }
    let q_norm = norm_sq(q);
    let mut selector = AscDistanceTopK::new(input.k);
    for (&id, row) in input.ids.iter().zip(input.rows) {
        if row.dim != q.dim {
            continue;
        }
        let distance = match metric {
            SparseVecMetric::L2 => l2_distance_from_sq(merge_l2_sq(row, q)),
            SparseVecMetric::NegativeInnerProduct => -f64::from(merge_dot(row, q)),
            SparseVecMetric::Cosine => {
                cosine_distance_from_parts(merge_dot(row, q), norm_sq(row), q_norm)
            }
        };
        let distance = if distance.is_finite() {
            distance
        } else {
            f64::NAN
        };
        selector.push(DistanceHit { id, distance });
    }
    Ok(selector.into_sorted_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::isa::{dot_scalar, l2_sq_scalar};

    /// 決定的な xorshift 乱数。
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn val(&mut self) -> f32 {
            ((self.next() >> 40) as f32 / (1u64 << 24) as f32 - 0.5) * 4.0
        }
    }

    /// (indices, values) を生成する。density は千分率。
    fn gen(rng: &mut Rng, dim: u32, per_mille: u64, force_one: bool) -> (Vec<u32>, Vec<f32>) {
        let mut idx = Vec::new();
        let mut val = Vec::new();
        for d in 0..dim {
            if rng.next() % 1000 < per_mille {
                idx.push(d);
                val.push(rng.val());
            }
        }
        if force_one && idx.is_empty() {
            idx.push(dim / 2);
            val.push(rng.val());
        }
        (idx, val)
    }

    fn densify(dim: u32, idx: &[u32], val: &[f32]) -> Vec<f32> {
        let mut d = vec![0.0f32; dim as usize];
        for (&i, &v) in idx.iter().zip(val) {
            d[i as usize] = v;
        }
        d
    }

    fn same_f64(a: f64, b: f64) -> bool {
        (a.is_nan() && b.is_nan()) || a.to_bits() == b.to_bits()
    }

    /// TABLE-47: 密化した参照計算とビット一致する（受け入れ条件）。
    #[test]
    fn table47_matches_dense_reference() {
        for &dim in &[1u32, 2, 7, 16, 100, 1000, 30000] {
            for &pm in &[0u64, 10, 100, 500, 1000] {
                for seed in 1..4u64 {
                    let mut rng = Rng(seed * 0x9E37_79B9 + u64::from(dim));
                    let (ia, va) = gen(&mut rng, dim, pm, false);
                    let (ib, vb) = gen(&mut rng, dim, pm, false);
                    let a = SparseVecRef::new(dim, &ia, &va).unwrap();
                    let b = SparseVecRef::new(dim, &ib, &vb).unwrap();
                    let (da, db) = (densify(dim, &ia, &va), densify(dim, &ib, &vb));

                    let dot = sparsevec_dot(&a, &b).unwrap();
                    assert!(dot == dot_scalar(&da, &db), "dot dim={dim} pm={pm}");
                    let l2 = sparsevec_l2_sq(&a, &b).unwrap();
                    assert_eq!(l2.to_bits(), l2_sq_scalar(&da, &db).to_bits());
                    let cos = sparsevec_cosine_distance(&a, &b).unwrap();
                    let want = cosine_distance_from_parts(
                        dot_scalar(&da, &db),
                        dot_scalar(&da, &da),
                        dot_scalar(&db, &db),
                    );
                    assert!(same_f64(cos, want), "cos dim={dim} pm={pm}");
                    assert_eq!(
                        sparsevec_negative_inner_product(&a, &b).unwrap(),
                        -f64::from(dot)
                    );
                    assert_eq!(sparsevec_l2_distance(&a, &b).unwrap(), f64::from(l2).sqrt());
                }
            }
        }
    }

    #[test]
    fn table47_known_values() {
        let (ia, va) = (vec![0u32, 2], vec![1.0f32, 2.0]);
        let (ib, vb) = (vec![2u32, 3], vec![3.0f32, -4.0]);
        let a = SparseVecRef::new(5, &ia, &va).unwrap();
        let b = SparseVecRef::new(5, &ib, &vb).unwrap();
        assert_eq!(sparsevec_dot(&a, &b).unwrap(), 6.0);
        assert_eq!(sparsevec_negative_inner_product(&a, &b).unwrap(), -6.0);
        // 1 + (2-3)^2 + 16 = 18
        assert_eq!(sparsevec_l2_sq(&a, &b).unwrap(), 18.0);
        let cos = sparsevec_cosine_distance(&a, &b).unwrap();
        assert!((cos - (1.0 - 6.0 / (5.0f64 * 25.0).sqrt())).abs() < 1e-6);
    }

    #[test]
    fn table47_boundaries() {
        let e: [u32; 0] = [];
        let ev: [f32; 0] = [];
        let z = SparseVecRef::new(4, &e, &ev).unwrap();
        assert_eq!(sparsevec_dot(&z, &z).unwrap(), 0.0);
        assert_eq!(sparsevec_l2_sq(&z, &z).unwrap(), 0.0);
        assert!(sparsevec_cosine_distance(&z, &z).unwrap().is_nan());
        let (i, v) = (vec![1u32], vec![3.0f32]);
        let a = SparseVecRef::new(4, &i, &v).unwrap();
        assert_eq!(sparsevec_l2_sq(&a, &z).unwrap(), 9.0);
        assert!(sparsevec_cosine_distance(&a, &z).unwrap().is_nan());
        // 明示的な 0.0 は結果を変えない。
        let (i0, v0) = (vec![0u32, 1], vec![0.0f32, 3.0]);
        let a0 = SparseVecRef::new(4, &i0, &v0).unwrap();
        assert_eq!(sparsevec_l2_sq(&a0, &a).unwrap(), 0.0);
        // dim = 1。
        let (i1, v1) = (vec![0u32], vec![2.0f32]);
        let one = SparseVecRef::new(1, &i1, &v1).unwrap();
        assert_eq!(sparsevec_dot(&one, &one).unwrap(), 4.0);
    }

    #[test]
    fn table47_validation_fails_closed() {
        let bad = |d: u32, i: &[u32], v: &[f32]| SparseVecRef::new(d, i, v).is_err();
        assert!(bad(4, &[0, 1], &[1.0]));
        assert!(bad(4, &[2, 1], &[1.0, 1.0]));
        assert!(bad(4, &[1, 1], &[1.0, 1.0]));
        assert!(bad(4, &[4], &[1.0]));
        assert!(bad(4, &[0], &[f32::NAN]));
        assert!(bad(4, &[0], &[f32::INFINITY]));
        let (i, v) = (vec![0u32], vec![1.0f32]);
        let a = SparseVecRef::new(4, &i, &v).unwrap();
        let b = SparseVecRef::new(5, &i, &v).unwrap();
        assert!(sparsevec_dot(&a, &b).is_err());
        assert!(sparsevec_l2_sq(&a, &b).is_err());
        assert!(sparsevec_cosine_distance(&a, &b).is_err());
    }

    fn model_top_k(
        ids: &[u64],
        rows: &[(Vec<u32>, Vec<f32>)],
        q: &(Vec<u32>, Vec<f32>),
        dim: u32,
        k: usize,
        m: SparseVecMetric,
    ) -> Vec<DistanceHit> {
        let dq = densify(dim, &q.0, &q.1);
        let mut sel = AscDistanceTopK::new(k);
        for (&id, r) in ids.iter().zip(rows) {
            let dr = densify(dim, &r.0, &r.1);
            let distance = match m {
                SparseVecMetric::L2 => f64::from(l2_sq_scalar(&dr, &dq)).sqrt(),
                SparseVecMetric::NegativeInnerProduct => -f64::from(dot_scalar(&dr, &dq)),
                SparseVecMetric::Cosine => cosine_distance_from_parts(
                    dot_scalar(&dr, &dq),
                    dot_scalar(&dr, &dr),
                    dot_scalar(&dq, &dq),
                ),
            };
            sel.push(DistanceHit { id, distance });
        }
        sel.into_sorted_vec()
    }

    /// TABLE-47: Top-k が密化した参照モデルと id 列・距離ビットで一致する。
    #[test]
    fn table47_top_k_matches_model() {
        let dim = 50u32;
        let mut rng = Rng(77);
        let mut rows = vec![(vec![], vec![])]; // ゼロノルム行を含める
        for _ in 0..30 {
            rows.push(gen(&mut rng, dim, 150, true));
        }
        rows.push(rows[3].clone()); // 同点（id 昇順になること）
        let ids: Vec<u64> = (100..100 + rows.len() as u64).collect();
        let views: Vec<SparseVecRef> = rows
            .iter()
            .map(|(i, v)| SparseVecRef::new(dim, i, v).unwrap())
            .collect();
        let q = gen(&mut rng, dim, 200, true);
        for m in [
            SparseVecMetric::L2,
            SparseVecMetric::NegativeInnerProduct,
            SparseVecMetric::Cosine,
        ] {
            for k in [0usize, 1, 5, rows.len(), rows.len() + 10] {
                let got = sparsevec_top_k(
                    SparseVecSearchInput {
                        ids: &ids,
                        rows: &views,
                        query: SparseVecRef::new(dim, &q.0, &q.1).unwrap(),
                        k,
                    },
                    m,
                )
                .unwrap();
                let want = model_top_k(&ids, &rows, &q, dim, k, m);
                assert_eq!(got.len(), want.len(), "{m:?} k={k}");
                for (g, w) in got.iter().zip(&want) {
                    assert_eq!(g.id, w.id, "{m:?} k={k}");
                    assert!(same_f64(g.distance, w.distance));
                }
            }
        }
    }

    #[test]
    fn table47_top_k_zero_norm_query_and_dim_skip() {
        let (i, v) = (vec![0u32], vec![1.0f32]);
        let e: [u32; 0] = [];
        let ev: [f32; 0] = [];
        let r1 = SparseVecRef::new(3, &i, &v).unwrap();
        let r_other = SparseVecRef::new(9, &i, &v).unwrap();
        let rows = [r1, r_other, r1];
        let ids = [7u64, 5, 6];
        let out = sparsevec_top_k(
            SparseVecSearchInput {
                ids: &ids,
                rows: &rows,
                query: SparseVecRef::new(3, &e, &ev).unwrap(),
                k: 10,
            },
            SparseVecMetric::Cosine,
        )
        .unwrap();
        // dim 違いの行（id 5）は skip、残りは全行 NaN で id 昇順。
        assert_eq!(out.iter().map(|h| h.id).collect::<Vec<_>>(), vec![6, 7]);
        assert!(out.iter().all(|h| h.distance.is_nan()));
    }

    #[test]
    fn table47_overflow_goes_last_as_nan() {
        let big = (vec![0u32], vec![1e30f32]);
        let small = (vec![0u32], vec![1.0f32]);
        let neg = (vec![0u32], vec![-1e30f32]);
        let q = (vec![0u32], vec![1e30f32]);
        let rows = [&big, &small, &neg];
        let views: Vec<SparseVecRef> = rows
            .iter()
            .map(|(i, v)| SparseVecRef::new(2, i, v).unwrap())
            .collect();
        let ids = [1u64, 2, 3];
        for m in [SparseVecMetric::NegativeInnerProduct, SparseVecMetric::L2] {
            let out = sparsevec_top_k(
                SparseVecSearchInput {
                    ids: &ids,
                    rows: &views,
                    query: SparseVecRef::new(2, &q.0, &q.1).unwrap(),
                    k: 3,
                },
                m,
            )
            .unwrap();
            assert!(out.iter().take(1).all(|h| h.distance.is_finite()), "{m:?}");
            assert!(out.last().is_some_and(|h| h.distance.is_nan()), "{m:?}");
        }
    }
}
