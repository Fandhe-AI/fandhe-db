//! int8 ベクトルの L2・負の内積・コサイン距離と brute-force Top-k（Issue #2019。
//! 親 #2017・ポインタ: TABLE-49）。
//!
//! 責務境界: 長さ・次元上限の検証（fail-closed）・距離値化（f64）・距離昇順の Top-k を
//! 受け持つ。整数内積そのものは `isa.rs` の [`crate::isa::I8Kernel::dot_i8`]
//! （HNSW の SQ8 常駐用に実装済み。全 ISA で scalar と完全一致）を再利用し、
//! 量子化スケールは掛けず整数値をそのまま比べる。Top-k の選出器はコサイン経路の
//! [`AscDistanceTopK`]（距離昇順・同点は id 昇順・NaN は末尾）を共用する。
//!
//! L2 は新しい intrinsics を足さず、整数で丸めなしに成り立つ恒等式
//! `||a-q||² = ||a||² + ||q||² - 2(a·q)` で `dot_i8` に乗せる。ノルム² は
//! [`dot_i8_scalar`]（厳密な整数）で求め、合算は i64 で行う（各項の最大は
//! `128² * 65536 = 2^30` で、2 項の和は i32 を超えうるため）。平方根は出力時に
//! f64 で 1 回だけ取る。
//!
//! −128 の扱い: SQ8 と違い INT8VEC は −128 も取りうる。VNNI 系の復元
//! `acc - 128 * row_sum` は wrapping 演算で厳密に戻り、`dim <= I8_DOT_MAX_DIM`
//! （65536）なら `|dot| <= 2^30`・`|row_sum| <= 128 * 65536`・VNNI の途中値
//! `|acc| <= 255 * 128 * 65536 < i32::MAX` で溢れない。上限超過の次元は
//! [`KernelError::DimMismatch`] で拒否する。
//!
//! SQL 経路からは `int8_*_distance` 3 種が行ごとに呼ばれる（`sql::udf_call` の
//! `BoundExpr::Int8VecDistance`。`<->`／`<#>`／`<=>` の接続は Issue #2953）。`int8_top_k` は
//! 呼び出し元が無いまま残る（検索 SELECT の DISTANCE 段は f32 の arena 前提で、INT8VEC は式キー
//! `ORDER BY` の広域取得へ書き換えて実行するため。性能が必要になった時点の別 Issue）。
//! 呼び出し元が RLS で絞った行を渡す契約で、テナント境界の検査は本モジュールの外で完結している。

use super::cosine::{AscDistanceTopK, DistanceHit};
use super::KernelError;
use crate::isa::{current_i8, dot_i8_scalar, I8QueryOperands};
use crate::sq8::I8_DOT_MAX_DIM;

/// 距離の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Int8Metric {
    /// ユークリッド距離 `sqrt(Σ (a-q)²)`。
    L2,
    /// 負の内積 `-(a·q)`（pgvector の `<#>` と同じ符号。昇順で近い順）。
    NegativeInnerProduct,
    /// コサイン距離 `1 - a·q / (|a||q|)`（ゼロノルムは NaN で昇順の末尾）。
    Cosine,
}

/// [`int8_top_k`] の入力ビュー。呼び出し元が可視行だけへ絞った借用を渡す契約。
pub struct Int8SearchInput<'a> {
    /// `rows` の行と 1 対 1 の識別子。
    pub ids: &'a [u64],
    /// `ids.len() * dim` 要素のフラット化済み行列。
    pub rows: &'a [i8],
    /// 1 行あたりの次元数（`I8_DOT_MAX_DIM` 以下）。
    pub dim: u32,
    /// クエリ（`dim` 要素）。
    pub query: &'a [i8],
    /// 返す件数の上限。
    pub k: usize,
}

/// クエリ側の事前計算（`shifted` オペランドと `||q||²`）。Top-k ではループ外で 1 回だけ作る。
struct PreparedQuery<'a> {
    signed: &'a [i8],
    shifted: Vec<u8>,
    norm_sq: u64,
}

/// 次元上限を検証する。超過は fail-closed で `DimMismatch`。
fn check_dim(len: usize, expected: u32) -> Result<(), KernelError> {
    if len > I8_DOT_MAX_DIM {
        return Err(KernelError::DimMismatch {
            expected,
            found: len,
        });
    }
    Ok(())
}

/// 厳密な整数ノルム²（`Σ x²`。検証済みの `dim <= 65536` なら `<= 2^30` で非負）。
fn norm_sq(x: &[i8]) -> u64 {
    u64::from(dot_i8_scalar(x, x).max(0) as u32)
}

/// 行和 `Σ a_d`（VNNI 系の復元用。`|sum| <= 128 * 65536` で溢れない）。
fn row_sum(row: &[i8]) -> i32 {
    row.iter()
        .fold(0i32, |acc, &v| acc.wrapping_add(i32::from(v)))
}

fn prepare(query: &[i8]) -> Result<PreparedQuery<'_>, KernelError> {
    let mut shifted = Vec::new();
    shifted
        .try_reserve_exact(query.len())
        .map_err(|_| KernelError::DimMismatch {
            expected: 0,
            found: query.len(),
        })?;
    shifted.extend(query.iter().map(|&q| (i16::from(q) + 128) as u8));
    Ok(PreparedQuery {
        signed: query,
        shifted,
        norm_sq: norm_sq(query),
    })
}

/// 準備済みクエリと 1 行の整数内積（長さは呼び出し元が揃え済み）。
fn dot_prepared(row: &[i8], q: &PreparedQuery<'_>) -> i64 {
    let operands = I8QueryOperands {
        signed: q.signed,
        shifted: &q.shifted,
    };
    i64::from(current_i8().dot_i8(row, row_sum(row), operands))
}

/// 準備済みクエリに対する距離（f64）。
fn distance_prepared(row: &[i8], q: &PreparedQuery<'_>, metric: Int8Metric) -> f64 {
    let dot = dot_prepared(row, q);
    match metric {
        Int8Metric::NegativeInnerProduct => -(dot as f64),
        Int8Metric::L2 => (l2_sq_from_parts(norm_sq(row), q.norm_sq, dot) as f64).sqrt(),
        Int8Metric::Cosine => cosine_from_parts(dot, norm_sq(row), q.norm_sq),
    }
}

/// `||a-q||² = na + nq - 2 dot`（i64 で厳密。理論上非負）。
fn l2_sq_from_parts(na: u64, nq: u64, dot: i64) -> u64 {
    let sum = (na as i64) + (nq as i64) - 2 * dot;
    sum.max(0) as u64
}

/// コサイン距離（f64）。類似度は [-1, 1] にクランプし、ゼロノルムは NaN
/// （`cosine.rs` の Issue #2240 の規約に合わせ、昇順の末尾へ回す）。
fn cosine_from_parts(dot: i64, na: u64, nq: u64) -> f64 {
    if na == 0 || nq == 0 {
        return f64::NAN;
    }
    let denom = ((na as f64) * (nq as f64)).sqrt();
    1.0 - (dot as f64 / denom).clamp(-1.0, 1.0)
}

/// 2 本の int8 ベクトルの厳密な内積。長さ不一致・次元上限超過は `DimMismatch`。
pub fn int8_dot(a: &[i8], b: &[i8]) -> Result<i64, KernelError> {
    if a.len() != b.len() {
        return Err(KernelError::DimMismatch {
            expected: u32::try_from(a.len()).unwrap_or(u32::MAX),
            found: b.len(),
        });
    }
    check_dim(a.len(), u32::try_from(a.len()).unwrap_or(u32::MAX))?;
    let q = prepare(b)?;
    Ok(dot_prepared(a, &q))
}

/// 2 本の int8 ベクトルの厳密な二乗 L2 距離 `Σ (a-b)²`。
pub fn int8_l2_sq(a: &[i8], b: &[i8]) -> Result<u64, KernelError> {
    let dot = int8_dot(a, b)?;
    Ok(l2_sq_from_parts(norm_sq(a), norm_sq(b), dot))
}

/// 2 本の int8 ベクトルの L2 距離（f64。平方根は 1 回だけ）。
pub fn int8_l2_distance(a: &[i8], b: &[i8]) -> Result<f64, KernelError> {
    int8_l2_sq(a, b).map(|sq| (sq as f64).sqrt())
}

/// 2 本の int8 ベクトルの負の内積 `-(a·b)`。
pub fn int8_negative_inner_product(a: &[i8], b: &[i8]) -> Result<f64, KernelError> {
    int8_dot(a, b).map(|d| -(d as f64))
}

/// 2 本の int8 ベクトルのコサイン距離（ゼロノルムは NaN）。
pub fn int8_cosine_distance(a: &[i8], b: &[i8]) -> Result<f64, KernelError> {
    let dot = int8_dot(a, b)?;
    Ok(cosine_from_parts(dot, norm_sq(a), norm_sq(b)))
}

/// int8 ベクトルの brute-force Top-k（距離昇順・同点は id 昇順）。
///
/// 検証順は `bit_top_k` と同じ: クエリ長不一致・次元上限超過は `DimMismatch`、
/// `k == 0` か行 0 件なら空、範囲外の行は skip。
#[allow(dead_code)] // SQL 接続（別 Issue）までは単体テストのみが呼び出し元
pub fn int8_top_k(
    input: Int8SearchInput<'_>,
    metric: Int8Metric,
) -> Result<Vec<DistanceHit>, KernelError> {
    let dim = input.dim as usize;
    if input.query.len() != dim {
        return Err(KernelError::DimMismatch {
            expected: input.dim,
            found: input.query.len(),
        });
    }
    check_dim(dim, input.dim)?;
    if input.k == 0 || input.ids.is_empty() {
        return Ok(Vec::new());
    }
    let q = prepare(input.query)?;
    let mut selector = AscDistanceTopK::new(input.k);
    for (idx, &id) in input.ids.iter().enumerate() {
        let Some(start) = idx.checked_mul(dim) else {
            continue;
        };
        let Some(row) = start
            .checked_add(dim)
            .and_then(|end| input.rows.get(start..end))
        else {
            continue;
        };
        selector.push(DistanceHit {
            id,
            distance: distance_prepared(row, &q, metric),
        });
    }
    Ok(selector.into_sorted_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::isa::{self, available_i8_kernels};

    /// 決定的な xorshift 乱数（−128..=127 を含む）。
    fn rng_vec(seed: u64, n: usize) -> Vec<i8> {
        let mut s = seed | 1;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 24) as u8 as i8
            })
            .collect()
    }

    fn ref_dot(a: &[i8], b: &[i8]) -> i64 {
        a.iter()
            .zip(b)
            .map(|(&x, &y)| i64::from(x) * i64::from(y))
            .sum()
    }

    fn ref_l2_sq(a: &[i8], b: &[i8]) -> u64 {
        a.iter()
            .zip(b)
            .map(|(&x, &y)| (i64::from(x) - i64::from(y)).pow(2) as u64)
            .sum()
    }

    const DIMS: [usize; 14] = [0, 1, 15, 16, 17, 31, 32, 33, 63, 64, 65, 128, 1000, 1536];

    /// TABLE-49: 利用可能な全 i8 カーネルが scalar・i64 参照と完全一致する。
    #[test]
    fn all_kernels_match_scalar_and_reference() {
        for (i, &dim) in DIMS.iter().enumerate() {
            let a = rng_vec(1000 + i as u64, dim);
            let b = rng_vec(2000 + i as u64, dim);
            let q = prepare(&b).unwrap();
            let ops = I8QueryOperands {
                signed: &b,
                shifted: &q.shifted,
            };
            for kernel in available_i8_kernels() {
                let got = kernel.dot_i8(&a, row_sum(&a), ops);
                assert_eq!(got, dot_i8_scalar(&a, &b), "{:?} dim={dim}", kernel.isa());
                assert_eq!(i64::from(got), ref_dot(&a, &b));
            }
            assert_eq!(int8_dot(&a, &b).unwrap(), ref_dot(&a, &b));
            assert_eq!(int8_l2_sq(&a, &b).unwrap(), ref_l2_sq(&a, &b));
            assert_eq!(
                int8_negative_inner_product(&a, &b).unwrap(),
                -(ref_dot(&a, &b) as f64)
            );
        }
    }

    /// TABLE-49: f32 参照との誤差が許容内（int8 経路は厳密、f32 側が丸める）。
    #[test]
    fn matches_f32_reference_within_tolerance() {
        let mut max_l2 = 0f64;
        let mut max_dot = 0f64;
        let mut max_cos = 0f64;
        for (i, &dim) in DIMS.iter().enumerate().filter(|(_, &d)| d > 0) {
            let a = rng_vec(7 + i as u64, dim);
            let b = rng_vec(77 + i as u64, dim);
            let fa: Vec<f32> = a.iter().map(|&v| f32::from(v)).collect();
            let fb: Vec<f32> = b.iter().map(|&v| f32::from(v)).collect();
            let l2_ref = f64::from(isa::l2_sq_scalar(&fa, &fb));
            let dot_ref = f64::from(isa::dot_scalar(&fa, &fb));
            let l2 = int8_l2_sq(&a, &b).unwrap() as f64;
            let dot = int8_dot(&a, &b).unwrap() as f64;
            let rel = |x: f64, r: f64| (x - r).abs() / r.abs().max(1.0);
            max_l2 = max_l2.max(rel(l2, l2_ref));
            max_dot = max_dot.max(rel(dot, dot_ref));
            let na = ref_dot(&a, &a) as f64;
            let nb = ref_dot(&b, &b) as f64;
            let cos_ref = 1.0 - dot_ref / (na.sqrt() * nb.sqrt());
            let cos = int8_cosine_distance(&a, &b).unwrap();
            max_cos = max_cos.max((cos - cos_ref).abs());
        }
        println!(
            "int8 vs f32 max err: l2_sq_rel={max_l2:e} dot_rel={max_dot:e} cos_abs={max_cos:e}"
        );
        assert!(max_l2 < 1e-6, "l2 {max_l2}");
        assert!(max_dot < 1e-4, "dot {max_dot}");
        assert!(max_cos < 1e-6, "cos {max_cos}");
    }

    /// TABLE-49: Top-k の id 列が f64 brute-force と一致し、同点は id 昇順。
    #[test]
    fn top_k_matches_bruteforce_with_id_tiebreak() {
        let dim = 24usize;
        let n = 60usize;
        let mut rows = rng_vec(5, dim * n);
        // 行 3 と行 40 を同一にして同点を作る。
        let dup: Vec<i8> = rows[3 * dim..4 * dim].to_vec();
        rows[40 * dim..41 * dim].copy_from_slice(&dup);
        let ids: Vec<u64> = (0..n as u64).map(|i| 100 + i).collect();
        let query = rng_vec(9, dim);
        for metric in [
            Int8Metric::L2,
            Int8Metric::NegativeInnerProduct,
            Int8Metric::Cosine,
        ] {
            let mut expect: Vec<(f64, u64)> = (0..n)
                .map(|i| {
                    let r = &rows[i * dim..(i + 1) * dim];
                    let d = match metric {
                        Int8Metric::L2 => (ref_l2_sq(r, &query) as f64).sqrt(),
                        Int8Metric::NegativeInnerProduct => -(ref_dot(r, &query) as f64),
                        Int8Metric::Cosine => {
                            1.0 - (ref_dot(r, &query) as f64
                                / ((ref_dot(r, r) as f64) * (ref_dot(&query, &query) as f64))
                                    .sqrt())
                            .clamp(-1.0, 1.0)
                        }
                    };
                    (d, ids[i])
                })
                .collect();
            expect.sort_by(|x, y| x.0.total_cmp(&y.0).then(x.1.cmp(&y.1)));
            let got = int8_top_k(
                Int8SearchInput {
                    ids: &ids,
                    rows: &rows,
                    dim: dim as u32,
                    query: &query,
                    k: 10,
                },
                metric,
            )
            .unwrap();
            let got_ids: Vec<u64> = got.iter().map(|h| h.id).collect();
            let want: Vec<u64> = expect.iter().take(10).map(|e| e.1).collect();
            assert_eq!(got_ids, want, "{metric:?}");
        }
        // 同一行をクエリにすると行 3 と 40 が距離 0 で同点 → id 昇順。
        let hits = int8_top_k(
            Int8SearchInput {
                ids: &ids,
                rows: &rows,
                dim: dim as u32,
                query: &dup,
                k: 2,
            },
            Int8Metric::L2,
        )
        .unwrap();
        assert_eq!(
            hits.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![103, 140]
        );
    }

    /// TABLE-49: −128 を埋めた最大次元でも溢れず厳密。上限超過は拒否。
    #[test]
    fn boundary_max_dim_with_min_values() {
        let n = I8_DOT_MAX_DIM;
        let a = vec![-128i8; n];
        let b = vec![-128i8; n];
        let c = vec![127i8; n];
        assert_eq!(int8_dot(&a, &b).unwrap(), 128 * 128 * n as i64);
        assert_eq!(int8_dot(&a, &c).unwrap(), -128 * 127 * n as i64);
        assert_eq!(int8_l2_sq(&a, &b).unwrap(), 0);
        assert_eq!(int8_l2_sq(&a, &c).unwrap(), 255 * 255 * n as u64);
        for kernel in available_i8_kernels() {
            let q = prepare(&b).unwrap();
            let ops = I8QueryOperands {
                signed: &b,
                shifted: &q.shifted,
            };
            assert_eq!(
                i64::from(kernel.dot_i8(&a, row_sum(&a), ops)),
                128 * 128 * n as i64,
                "{:?}",
                kernel.isa()
            );
        }
        let big = vec![0i8; n + 1];
        assert!(matches!(
            int8_dot(&big, &big),
            Err(KernelError::DimMismatch { .. })
        ));
        let r = int8_top_k(
            Int8SearchInput {
                ids: &[1],
                rows: &big,
                dim: (n + 1) as u32,
                query: &big,
                k: 1,
            },
            Int8Metric::L2,
        );
        assert!(matches!(r, Err(KernelError::DimMismatch { .. })));
    }

    /// TABLE-49: 空入力・不整合・ゼロノルムの fail-closed 挙動。
    #[test]
    fn edge_cases() {
        let input = |ids: &'static [u64], rows: &'static [i8], dim, query: &'static [i8], k| {
            Int8SearchInput {
                ids,
                rows,
                dim,
                query,
                k,
            }
        };
        assert!(
            int8_top_k(input(&[1], &[1, 2], 2, &[1, 2], 0), Int8Metric::L2)
                .unwrap()
                .is_empty()
        );
        assert!(int8_top_k(input(&[], &[], 2, &[1, 2], 3), Int8Metric::L2)
            .unwrap()
            .is_empty());
        assert!(matches!(
            int8_top_k(input(&[1], &[1, 2], 2, &[1], 1), Int8Metric::L2),
            Err(KernelError::DimMismatch { .. })
        ));
        assert!(matches!(
            int8_dot(&[1, 2], &[1]),
            Err(KernelError::DimMismatch { .. })
        ));
        // 行列が半端（2 行目が欠ける）→ その行を skip。
        let hits = int8_top_k(input(&[1, 2], &[1, 2, 3], 2, &[1, 2], 5), Int8Metric::L2).unwrap();
        assert_eq!(hits.len(), 1);
        // dim == 0 は距離 0。
        assert_eq!(int8_l2_distance(&[], &[]).unwrap(), 0.0);
        // ゼロノルムの cosine は NaN で末尾。
        assert!(int8_cosine_distance(&[0, 0], &[1, 2]).unwrap().is_nan());
        let hits = int8_top_k(
            input(&[1, 2], &[0, 0, 1, 2], 2, &[1, 2], 2),
            Int8Metric::Cosine,
        )
        .unwrap();
        assert_eq!(hits[0].id, 2);
        assert!(hits[1].distance.is_nan());
    }
}
