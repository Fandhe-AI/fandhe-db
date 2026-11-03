//! L2・Cosine metric の HNSW 構築・探索の受入テスト（Issue #1998。親 #1996。
//! ビヘイビア INDEX-7・SQL-33 のポインタ）。`hnsw.rs` の `mod tests` が約 8000 行あるため
//! 分けている。対照は本ファイル内の f64 brute-force（L2 は二乗距離昇順、Cosine は
//! 生の行同士のコサイン類似度降順）。フィクスチャは **正規化しない**（行ごとに長さを
//! [0.25, 4] で変える。正規化済みだとコサインと内積の一致が自明でテストにならないため）。

#![allow(clippy::chunks_exact_to_as_chunks)] // DIM は定数だが可変長スライスの行分割として扱う

use super::metric::{normalize_into, HnswMetric};
use super::*;

const DIM: usize = 32;
const ROWS: usize = 2000;
const QUERIES: usize = 100;
const CLUSTERS: usize = 20;
const K: usize = 10;

/// 固定シードの xorshift64*（テスト専用。非暗号）。[0,1) の一様乱数を返す。
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let v = self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40;
        v as f32 / (1u64 << 24) as f32
    }
    fn sym(&mut self) -> f32 {
        self.next() * 2.0 - 1.0
    }
}

/// クラスタ構造を持つ非正規化コーパスとクエリ（行・クエリごとに長さが異なる）。
fn fixture() -> (Vec<f32>, Vec<f32>) {
    let mut rng = Rng(0x1998_0001);
    let centers: Vec<Vec<f32>> = (0..CLUSTERS)
        .map(|_| (0..DIM).map(|_| rng.sym()).collect())
        .collect();
    let gen = |rng: &mut Rng, n: usize| {
        let mut out = Vec::with_capacity(n * DIM);
        for i in 0..n {
            let c = &centers[i % CLUSTERS];
            let len = 0.25 + rng.next() * 3.75;
            out.extend(c.iter().map(|&x| (x + rng.sym() * 0.3) * len));
        }
        out
    };
    let rows = gen(&mut rng, ROWS);
    let queries = gen(&mut rng, QUERIES);
    (rows, queries)
}

fn l2_sq64(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| {
            let d = f64::from(*x) - f64::from(*y);
            d * d
        })
        .sum()
}

fn cos64(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
        .sum();
    let na: f64 = a
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt();
    let nb: f64 = b
        .iter()
        .map(|x| f64::from(*x) * f64::from(*x))
        .sum::<f64>()
        .sqrt();
    dot / (na * nb)
}

/// brute-force の top-k（ノード番号）。`key` は小さいほど近い。
fn brute(rows: &[f32], q: &[f32], key: impl Fn(&[f32], &[f32]) -> f64) -> Vec<u32> {
    let mut v: Vec<(f64, u32)> = rows
        .chunks_exact(DIM)
        .enumerate()
        .map(|(i, r)| (key(r, q), i as u32))
        .collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    v.into_iter().take(K).map(|(_, i)| i).collect()
}

fn recall(index: &HnswIndex, rows: &[f32], queries: &[f32], ef: usize, metric: HnswMetric) -> f64 {
    let mut scratch = HnswSearchScratch::default();
    let mut hit = 0usize;
    for q in queries.chunks_exact(DIM) {
        let truth = match metric {
            HnswMetric::L2 => brute(rows, q, l2_sq64),
            _ => brute(rows, q, |a, b| -cos64(a, b)),
        };
        let got = index.search(q, K, ef, &mut scratch).unwrap();
        hit += got
            .iter()
            .filter(|h| truth.contains(&(h.id as u32)))
            .count();
    }
    hit as f64 / (QUERIES * K) as f64
}

fn build(
    metric: HnswMetric,
    precision: ResidentPrecision,
    rows: &[f32],
    threads: usize,
) -> HnswIndex {
    HnswIndex::build_with_metric(
        HnswParams::default(),
        metric,
        precision,
        DIM as u32,
        rows,
        42,
        threads,
    )
    .unwrap()
}

/// INDEX-7・SQL-33: L2 の Recall@10（ef=64 で 0.95 以上、ef=256 で 0.99 以上）。
#[test]
fn l2_recall_meets_thresholds() {
    let (rows, queries) = fixture();
    let index = build(HnswMetric::L2, ResidentPrecision::F32, &rows, 1);
    assert_eq!(index.metric(), HnswMetric::L2);
    let r64 = recall(&index, &rows, &queries, 64, HnswMetric::L2);
    let r256 = recall(&index, &rows, &queries, 256, HnswMetric::L2);
    eprintln!("l2 recall ef64={r64} ef256={r256}");
    assert!(r64 >= 0.95, "ef=64 recall={r64}");
    assert!(r256 >= 0.99, "ef=256 recall={r256}");
}

/// INDEX-7・SQL-33: Cosine の Recall@10（生の行同士のコサインが対照）。
#[test]
fn cosine_recall_meets_thresholds() {
    let (rows, queries) = fixture();
    let index = build(HnswMetric::Cosine, ResidentPrecision::F32, &rows, 1);
    let r64 = recall(&index, &rows, &queries, 64, HnswMetric::Cosine);
    let r256 = recall(&index, &rows, &queries, 256, HnswMetric::Cosine);
    eprintln!("cosine recall ef64={r64} ef256={r256}");
    assert!(r64 >= 0.95, "ef=64 recall={r64}");
    assert!(r256 >= 0.99, "ef=256 recall={r256}");
}

/// 並列構築（threads=4、rows > SEQUENTIAL_PREFIX_NODES）でも Recall が逐次から 0.02 以内で、
/// 層 0 の次数上限を守る。
#[test]
fn parallel_build_matches_sequential_recall() {
    let (rows, queries) = fixture();
    const { assert!(ROWS > SEQUENTIAL_PREFIX_NODES) };
    for metric in [HnswMetric::L2, HnswMetric::Cosine] {
        let seq = build(metric, ResidentPrecision::F32, &rows, 1);
        let par = build(metric, ResidentPrecision::F32, &rows, 4);
        let rs = recall(&seq, &rows, &queries, 64, metric);
        let rp = recall(&par, &rows, &queries, 64, metric);
        eprintln!("{metric:?} seq={rs} par={rp}");
        assert!(rp >= rs - 0.02, "{metric:?} seq={rs} par={rp}");
        assert!(rp >= 0.95, "{metric:?} par={rp}");
        let max_deg = HnswParams::default().m * 2;
        for node in 0..ROWS as u32 {
            let n = par.neighbors(0, node).map_or(0, <[u32]>::len);
            assert!(n <= max_deg, "node={node} degree={n}");
            assert!(n > 0, "node={node} isolated");
        }
    }
}

/// Cosine は F16・SQ8 常駐でも既存の常駐精度テストの基準を下回らない。
#[test]
fn cosine_resident_precision_recall() {
    let (rows, queries) = fixture();
    for (p, floor) in [(ResidentPrecision::F16, 0.7), (ResidentPrecision::I8, 0.5)] {
        let index = build(HnswMetric::Cosine, p, &rows, 1);
        assert_eq!(index.resident_precision(), p);
        let r = recall(&index, &rows, &queries, 64, HnswMetric::Cosine);
        eprintln!("cosine {p:?} recall={r}");
        assert!(r >= floor, "{p:?} recall={r}");
    }
}

/// L2 は f16・SQ8 を要求しても F32 へ縮退し、結果は F32 の L2 と同一。
#[test]
fn l2_degrades_to_f32_resident() {
    let (rows, queries) = fixture();
    let base = build(HnswMetric::L2, ResidentPrecision::F32, &rows, 1);
    let mut s0 = HnswSearchScratch::default();
    let mut s1 = HnswSearchScratch::default();
    for p in [ResidentPrecision::F16, ResidentPrecision::I8] {
        let index = build(HnswMetric::L2, p, &rows, 1);
        assert_eq!(index.resident_precision(), ResidentPrecision::F32);
        for q in queries.chunks_exact(DIM).take(20) {
            let a = base.search(q, K, 64, &mut s0).unwrap();
            let b = index.search(q, K, 64, &mut s1).unwrap();
            assert_eq!(a, b);
        }
    }
}

/// L2 の近傍選択ヒューリスティック: 近接して冗長な候補は刈られ、遠い候補が残る。
#[test]
fn l2_heuristic_prunes_redundant_and_keeps_far() {
    let dim = 2usize;
    let a = [1.0f32, 0.0];
    let b = [1.1f32, 0.1]; // A に極めて近い（冗長）
    let c = [-5.0f32, 0.0]; // クエリからは遠いが A からはさらに遠い
    let vectors: Vec<f32> = [a, b, c].concat();
    let q = [0.0f32, 0.0];
    let score = |v: &[f32; 2]| -crate::kernel::l2_sq(v, &q);
    let mut cands = vec![
        ScoredNode {
            node: 0,
            score: score(&a),
        },
        ScoredNode {
            node: 1,
            score: score(&b),
        },
        ScoredNode {
            node: 2,
            score: score(&c),
        },
    ];
    cands.sort_by(|x, y| y.score.total_cmp(&x.score));
    let selected =
        select_neighbors_heuristic_free(HnswMetric::L2, &cands, 2, dim, &vectors).unwrap();
    assert!(selected.contains(&0));
    assert!(selected.contains(&2));
    assert!(!selected.contains(&1));
}

/// fail-closed: ゼロノルム行は構築を、ゼロノルムのクエリは探索を拒否する。L2 の二乗和
/// オーバーフローは `NonFiniteScore`。
#[test]
fn fail_closed_on_non_normalizable_and_overflow() {
    let (mut rows, _) = fixture();
    let node = 7usize;
    rows[node * DIM..(node + 1) * DIM].fill(0.0);
    for threads in [1usize, 4] {
        let r = HnswIndex::build_with_metric(
            HnswParams::default(),
            HnswMetric::Cosine,
            ResidentPrecision::F32,
            DIM as u32,
            &rows,
            1,
            threads,
        );
        assert!(
            matches!(r, Err(HnswError::NonNormalizableVector { node: 7 })),
            "threads={threads}"
        );
    }
    let (rows, _) = fixture();
    let index = build(HnswMetric::Cosine, ResidentPrecision::F32, &rows, 1);
    let mut scratch = HnswSearchScratch::default();
    let zero = vec![0.0f32; DIM];
    assert!(matches!(
        index.search(&zero, K, 64, &mut scratch),
        Err(HnswError::NonNormalizableQuery)
    ));
    let nan = vec![f32::NAN; DIM];
    assert!(matches!(
        index.search(&nan, K, 64, &mut scratch),
        Err(HnswError::NonFiniteQuery)
    ));

    // L2: 有限な成分でも差の二乗和が f32 を超える。
    let big = vec![3.0e19f32, 0.0, -3.0e19, 0.0];
    let r = HnswIndex::build_with_metric(
        HnswParams::default(),
        HnswMetric::L2,
        ResidentPrecision::F32,
        2,
        &big,
        1,
        1,
    );
    assert!(matches!(r, Err(HnswError::NonFiniteScore { .. })), "{r:?}");
}

/// `normalize_into` の性質: 単位長・決定的・ゼロ／サブノーマル／オーバーフローを拒否。
#[test]
fn normalize_into_properties() {
    let row: Vec<f32> = (0..DIM).map(|i| (i as f32 - 10.0) * 0.37).collect();
    let mut a = vec![0.0; DIM];
    let mut b = vec![0.0; DIM];
    normalize_into(&row, &mut a).unwrap();
    normalize_into(&row, &mut b).unwrap();
    assert_eq!(
        a.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
        b.iter().map(|v| v.to_bits()).collect::<Vec<_>>()
    );
    let n: f64 = a.iter().map(|v| f64::from(*v).powi(2)).sum::<f64>().sqrt();
    assert!((n - 1.0).abs() < 1e-6, "norm={n}");
    assert!(normalize_into(&[0.0; 4], &mut [0.0; 4]).is_err());
    // ノルム² がアンダーフロー
    assert!(normalize_into(&[1e-30, 0.0, 0.0, 0.0], &mut [0.0; 4]).is_err());
    // ノルム² がオーバーフロー
    assert!(normalize_into(&[1e30, 1e30, 0.0, 0.0], &mut [0.0; 4]).is_err());
    assert!(normalize_into(&[1.0, 2.0], &mut [0.0; 3]).is_err());
}

/// Cosine の `node_matches`: 生の行で一致、別の行で不一致（F32・F16・I8）。
#[test]
fn cosine_node_matches_uses_normalized_comparison() {
    let (rows, _) = fixture();
    for p in [
        ResidentPrecision::F32,
        ResidentPrecision::F16,
        ResidentPrecision::I8,
    ] {
        let index = build(HnswMetric::Cosine, p, &rows, 1);
        assert_eq!(index.resident_precision(), p);
        for node in [0usize, 5, 1999] {
            let raw = &rows[node * DIM..(node + 1) * DIM];
            assert_eq!(index.node_matches(node as u32, raw), Some(true), "{p:?}");
            // 異なる向きの行（別ノードの行）は不一致。
            let o = (node + 1) % ROWS;
            let other = &rows[o * DIM..(o + 1) * DIM];
            assert_eq!(index.node_matches(node as u32, other), Some(false), "{p:?}");
        }
        // 正規化できない行は「変更あり」。
        assert_eq!(index.node_matches(0, &[0.0; DIM]), Some(false));
    }
}
