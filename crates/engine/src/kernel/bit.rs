//! 二値（bit）ベクトルの Hamming・Jaccard 距離と brute-force Top-k（Issue #2022。
//! 親 #2020・ポインタ: TABLE-48）。
//!
//! 責務境界: 長さの検証（fail-closed）・パディングビットのマスク・距離値化（f64）・
//! 距離昇順の Top-k を受け持つ。popcount の数え上げ自体は `isa.rs` の
//! [`PopcountKernel`]（ISA 別。整数なので scalar と完全一致）が担う。Top-k の選出器は
//! コサイン経路の [`AscDistanceTopK`]（距離昇順・同点は id 昇順）を共用する。
//!
//! 現時点では SQL 経路から呼ばれない（`<~>`／`<%>` の実行接続は別 Issue。`bit(n)` 列の
//! 保存は #2021）。行データは MSB 先頭のバイト詰め（PostgreSQL の bit と同じ）で、
//! `nbits` が 8 の倍数でないとき最終バイトの下位ビットはパディング。保存側が 0 に
//! 正規化していなくても、本モジュールが両オペランドをマスクして数える。
//!
//! 長さが違うオペランドは黙って切り詰めず [`KernelError::DimMismatch`] で拒否する
//! （pgvector と同様。Top-k 中の長さ不正な行は既存規約どおり skip）。

use super::cosine::{AscDistanceTopK, DistanceHit};
use super::KernelError;
use crate::isa::{current_popcount, JaccardCounts};

/// 距離の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitMetric {
    /// 差のあるビット数。
    Hamming,
    /// `1 - |a & b| / |a | b|`（交差が 0 なら 1.0）。
    Jaccard,
}

/// [`bit_top_k`] の入力ビュー。呼び出し元が可視行だけへ絞った借用を渡す契約
/// （テナント境界は呼び出し元で完結している）。
pub struct BitSearchInput<'a> {
    /// `rows` の行と 1 対 1 の識別子。
    pub ids: &'a [u64],
    /// `ids.len() * ceil(nbits / 8)` バイトのフラット化済み行列。
    pub rows: &'a [u8],
    /// 1 行あたりのビット数。
    pub nbits: u32,
    /// クエリ（`ceil(nbits / 8)` バイト）。
    pub query: &'a [u8],
    /// 返す件数の上限。
    pub k: usize,
}

/// `nbits` ビットを収めるバイト数。
fn byte_len(nbits: u32) -> usize {
    (nbits as usize).div_ceil(8)
}

/// 最終バイトの有効ビットだけを残すマスク（MSB 先頭。8 の倍数なら 0xFF）。
fn tail_mask(nbits: u32) -> u8 {
    match nbits % 8 {
        0 => 0xFF,
        r => 0xFFu8 << (8 - r),
    }
}

/// [`split_masked`] の結果（先頭部 a・先頭部 b・マスク済み最終バイト a・同 b）。
type MaskedSplit<'a> = (&'a [u8], &'a [u8], u8, u8);

/// 長さ検証済みの 2 オペランドを「先頭部（そのまま）＋最終バイト（マスク済み）」へ分ける。
/// どちらかの長さが `ceil(nbits / 8)` と違えば `DimMismatch`。
fn split_masked<'a>(a: &'a [u8], b: &'a [u8], nbits: u32) -> Result<MaskedSplit<'a>, KernelError> {
    let n = byte_len(nbits);
    for side in [a, b] {
        if side.len() != n {
            return Err(KernelError::DimMismatch {
                expected: nbits,
                found: side.len().saturating_mul(8),
            });
        }
    }
    let mask = tail_mask(nbits);
    match (a.split_last(), b.split_last()) {
        (Some((la, ha)), Some((lb, hb))) => Ok((ha, hb, la & mask, lb & mask)),
        // nbits == 0（空オペランド同士）。
        _ => Ok((&[], &[], 0, 0)),
    }
}

/// 2 本の bit 列の Hamming 距離（差のあるビット数。パディングは無視する）。
pub fn hamming_distance(a: &[u8], b: &[u8], nbits: u32) -> Result<f64, KernelError> {
    let (ha, hb, la, lb) = split_masked(a, b, nbits)?;
    let body = current_popcount().hamming(ha, hb);
    let tail = u64::from((la ^ lb).count_ones());
    Ok(body.saturating_add(tail) as f64)
}

/// Jaccard の交差・和集合カウント（パディングは無視する）。
fn jaccard_counts(a: &[u8], b: &[u8], nbits: u32) -> Result<JaccardCounts, KernelError> {
    let (ha, hb, la, lb) = split_masked(a, b, nbits)?;
    let body = current_popcount().jaccard_counts(ha, hb);
    Ok(JaccardCounts {
        intersection: body
            .intersection
            .saturating_add(u64::from((la & lb).count_ones())),
        union: body.union.saturating_add(u64::from((la | lb).count_ones())),
    })
}

/// 交差・和集合から Jaccard 距離を求める。交差 0（両方ゼロの 0/0 を含む）は 1.0。
fn jaccard_from_counts(c: JaccardCounts) -> f64 {
    if c.intersection == 0 {
        return 1.0;
    }
    1.0 - c.intersection as f64 / c.union as f64
}

/// 2 本の bit 列の Jaccard 距離（pgvector 準拠）。
pub fn jaccard_distance(a: &[u8], b: &[u8], nbits: u32) -> Result<f64, KernelError> {
    jaccard_counts(a, b, nbits).map(jaccard_from_counts)
}

/// bit ベクトルの brute-force Top-k（距離昇順・同点は id 昇順）。
///
/// 検証順は [`super::cosine_top_k`] と同じ: クエリ長不一致は `DimMismatch`、
/// `k == 0` か行 0 件なら空、範囲外・長さ不正の行は skip。
#[allow(dead_code)] // SQL 接続（別 Issue）までは単体テストのみが呼び出し元
pub fn bit_top_k(
    input: BitSearchInput<'_>,
    metric: BitMetric,
) -> Result<Vec<DistanceHit>, KernelError> {
    let n = byte_len(input.nbits);
    if input.query.len() != n {
        return Err(KernelError::DimMismatch {
            expected: input.nbits,
            found: input.query.len().saturating_mul(8),
        });
    }
    if input.k == 0 || input.ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut selector = AscDistanceTopK::new(input.k);
    for (idx, &id) in input.ids.iter().enumerate() {
        let Some(start) = idx.checked_mul(n) else {
            continue;
        };
        let Some(row) = start
            .checked_add(n)
            .and_then(|end| input.rows.get(start..end))
        else {
            continue;
        };
        let distance = match metric {
            BitMetric::Hamming => hamming_distance(row, input.query, input.nbits),
            BitMetric::Jaccard => jaccard_distance(row, input.query, input.nbits),
        };
        if let Ok(distance) = distance {
            selector.push(DistanceHit { id, distance });
        }
    }
    Ok(selector.into_sorted_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1 ビットずつ数える素朴な参照実装（有効 `nbits` ビットのみ）。
    fn bit_at(bytes: &[u8], i: usize) -> bool {
        bytes
            .get(i / 8)
            .is_some_and(|b| (b >> (7 - (i % 8))) & 1 == 1)
    }

    fn ref_counts(a: &[u8], b: &[u8], nbits: u32) -> (u64, u64, u64) {
        let (mut x, mut i, mut u) = (0, 0, 0);
        for k in 0..nbits as usize {
            let (p, q) = (bit_at(a, k), bit_at(b, k));
            x += u64::from(p != q);
            i += u64::from(p && q);
            u += u64::from(p || q);
        }
        (x, i, u)
    }

    fn ref_dist(a: &[u8], b: &[u8], nbits: u32, m: BitMetric) -> f64 {
        let (x, i, u) = ref_counts(a, b, nbits);
        match m {
            BitMetric::Hamming => x as f64,
            BitMetric::Jaccard if i == 0 => 1.0,
            BitMetric::Jaccard => 1.0 - i as f64 / u as f64,
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| (self.next() >> 24) as u8).collect()
        }
    }

    /// TABLE-48・Issue #2022: 既知値（pgvector と同じ式）。
    #[test]
    fn known_values() {
        let a = [0b1010_0000u8];
        let b = [0b1100_0000u8];
        assert_eq!(hamming_distance(&a, &b, 4).unwrap(), 2.0);
        assert_eq!(hamming_distance(&a, &a, 4).unwrap(), 0.0);
        assert_eq!(hamming_distance(&[0x00], &[0xFF], 8).unwrap(), 8.0);
        assert_eq!(jaccard_distance(&a, &a, 8).unwrap(), 0.0);
        assert_eq!(jaccard_distance(&[0xF0], &[0x0F], 8).unwrap(), 1.0);
        assert_eq!(jaccard_distance(&[0x00], &[0x00], 8).unwrap(), 1.0);
        // 交差 1・和集合 3 → 1 - 1/3。
        assert_eq!(jaccard_distance(&a, &b, 4).unwrap(), 1.0 - 1.0 / 3.0);
        // 交差 2・和集合 4 → 0.5。
        assert_eq!(
            jaccard_distance(&[0b1110_0000], &[0b0111_0000], 8).unwrap(),
            0.5
        );
    }

    /// パディングビットのごみは距離に影響しない。
    #[test]
    fn padding_bits_are_ignored() {
        let mut rng = Rng(0x2022);
        for nbits in [3u32, 9, 63, 65] {
            let n = byte_len(nbits);
            for _ in 0..50 {
                let a = rng.bytes(n);
                let b = rng.bytes(n);
                for m in [BitMetric::Hamming, BitMetric::Jaccard] {
                    let want = ref_dist(&a, &b, nbits, m);
                    let got = match m {
                        BitMetric::Hamming => hamming_distance(&a, &b, nbits),
                        BitMetric::Jaccard => jaccard_distance(&a, &b, nbits),
                    }
                    .unwrap();
                    assert_eq!(got, want, "nbits={nbits} {m:?}");
                }
            }
        }
    }

    #[test]
    fn length_mismatch_is_rejected() {
        assert!(matches!(
            hamming_distance(&[0], &[0, 0], 8),
            Err(KernelError::DimMismatch { .. })
        ));
        assert!(matches!(
            jaccard_distance(&[0, 0], &[0, 0], 8),
            Err(KernelError::DimMismatch { .. })
        ));
        let input = BitSearchInput {
            ids: &[1],
            rows: &[0],
            nbits: 8,
            query: &[0, 0],
            k: 1,
        };
        assert!(matches!(
            bit_top_k(input, BitMetric::Hamming),
            Err(KernelError::DimMismatch { .. })
        ));
    }

    #[test]
    fn empty_k_zero_and_short_rows() {
        let q = [0u8];
        let mk = |ids: &'static [u64], rows: &'static [u8], k| BitSearchInput {
            ids,
            rows,
            nbits: 8,
            query: &q,
            k,
        };
        assert!(bit_top_k(mk(&[], &[], 3), BitMetric::Hamming)
            .unwrap()
            .is_empty());
        assert!(bit_top_k(mk(&[1], &[1], 0), BitMetric::Hamming)
            .unwrap()
            .is_empty());
        // 行データが足りない行は skip。
        let hits = bit_top_k(mk(&[1, 2], &[1], 5), BitMetric::Hamming).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, 1);
        // k > 行数・同距離は id 昇順。
        let hits = bit_top_k(mk(&[7, 3, 5], &[1, 1, 1], 10), BitMetric::Hamming).unwrap();
        let ids: Vec<u64> = hits.iter().map(|h| h.id).collect();
        assert_eq!(ids, vec![3, 5, 7]);
    }

    #[test]
    fn nbits_zero_is_defined() {
        assert_eq!(hamming_distance(&[], &[], 0).unwrap(), 0.0);
        assert_eq!(jaccard_distance(&[], &[], 0).unwrap(), 1.0);
    }

    /// 総当たり参照との一致（Hamming・Jaccard）と Jaccard の値域。
    #[test]
    fn top_k_matches_brute_force() {
        let mut rng = Rng(0xB17);
        for nbits in [8u32, 13, 64, 65, 200] {
            let n = byte_len(nbits);
            let rows_n = 60usize;
            let ids: Vec<u64> = (0..rows_n as u64).map(|i| i * 3 % 17 + i).collect();
            let rows = rng.bytes(rows_n * n);
            let query = rng.bytes(n);
            for m in [BitMetric::Hamming, BitMetric::Jaccard] {
                let mut want: Vec<(f64, u64)> = ids
                    .iter()
                    .enumerate()
                    .map(|(i, &id)| (ref_dist(&rows[i * n..(i + 1) * n], &query, nbits, m), id))
                    .collect();
                want.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
                want.truncate(10);
                let got = bit_top_k(
                    BitSearchInput {
                        ids: &ids,
                        rows: &rows,
                        nbits,
                        query: &query,
                        k: 10,
                    },
                    m,
                )
                .unwrap();
                let got: Vec<(f64, u64)> = got.iter().map(|h| (h.distance, h.id)).collect();
                assert_eq!(got, want, "nbits={nbits} {m:?}");
                if m == BitMetric::Jaccard {
                    assert!(got
                        .iter()
                        .all(|(d, _)| d.is_finite() && (0.0..=1.0).contains(d)));
                }
            }
        }
    }
}
