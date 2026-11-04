//! L2・Cosine・L1・負の内積 metric の HNSW 構築・探索の受入テスト（Issue #1998・#1999。親 #1996。
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

fn l1_64(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (f64::from(*x) - f64::from(*y)).abs())
        .sum()
}

fn dot64(a: &[f32], b: &[f32]) -> f64 {
    a.iter()
        .zip(b)
        .map(|(x, y)| f64::from(*x) * f64::from(*y))
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
            HnswMetric::Cosine => brute(rows, q, |a, b| -cos64(a, b)),
            HnswMetric::L1 => brute(rows, q, l1_64),
            HnswMetric::InnerProduct => brute(rows, q, |a, b| -dot64(a, b)),
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
    for metric in [HnswMetric::L2, HnswMetric::Cosine, HnswMetric::L1] {
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

/// fail-closed: ゼロノルムのクエリは探索を拒否する。ゼロノルム行は構築を拒否せず索引から
/// 除外する（Issue #2241）。L2 の二乗和オーバーフローは `NonFiniteScore`。
#[test]
fn fail_closed_on_non_normalizable_and_overflow() {
    let (mut rows, _) = fixture();
    let node = 7usize;
    rows[node * DIM..(node + 1) * DIM].fill(0.0);
    for threads in [1usize, 4] {
        let index = build(HnswMetric::Cosine, ResidentPrecision::F32, &rows, threads);
        assert!(!index.is_member(7), "threads={threads}");
        assert_eq!(index.member_count(), ROWS - 1, "threads={threads}");
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
        // member が正規化できない行になった（ゼロ化）場合は「変更あり」。
        assert_eq!(index.node_matches(0, &[0.0; DIM]), Some(false));
    }
}

/// Issue #2241: 除外ノードの `node_matches` は、正規化不可のままなら一致（`Some(true)`。
/// false だと毎世代 delta／churn に積まれ続ける）、正規化可になったら不一致（delta で拾う）。
/// member のゼロ化は不一致。F32・F16・I8 で同じ規則。
#[test]
fn cosine_node_matches_for_excluded_nodes() {
    let (mut rows, _) = fixture();
    let bad = 11usize;
    rows[bad * DIM..(bad + 1) * DIM].fill(0.0);
    let good: Vec<f32> = (0..DIM).map(|i| i as f32 + 1.0).collect();
    for p in [
        ResidentPrecision::F32,
        ResidentPrecision::F16,
        ResidentPrecision::I8,
    ] {
        let index = build(HnswMetric::Cosine, p, &rows, 1);
        assert!(!index.is_member(bad as u32), "{p:?}");
        assert_eq!(
            index.node_matches(bad as u32, &[0.0; DIM]),
            Some(true),
            "{p:?}"
        );
        assert_eq!(
            index.node_matches(bad as u32, &[1e-30; DIM]),
            Some(true),
            "{p:?}"
        );
        assert_eq!(index.node_matches(bad as u32, &good), Some(false), "{p:?}");
        assert_eq!(index.node_matches(12, &[0.0; DIM]), Some(false), "{p:?}");
        // 範囲外・次元不一致は従来どおり None。
        assert_eq!(index.node_matches(ROWS as u32, &good), None, "{p:?}");
        assert_eq!(index.node_matches(0, &good[..DIM - 1]), None, "{p:?}");
    }
}

/// 正規化できない行の 3 種（ゼロ・ノルム²アンダーフロー・ノルム²オーバーフロー）。
fn bad_row(kind: usize) -> Vec<f32> {
    match kind % 3 {
        0 => vec![0.0; DIM],
        1 => vec![1e-30; DIM],
        _ => vec![1e30; DIM],
    }
}

/// `rows` の `stride` 行おき（node % stride == 3）を正規化できない行に置き換える。除外した
/// ノード番号の昇順リストも返す。
fn scatter_bad_rows(rows: &[f32], stride: usize) -> (Vec<f32>, Vec<u32>) {
    let mut out = rows.to_vec();
    let mut bad = Vec::new();
    for node in (3..ROWS).step_by(stride) {
        out[node * DIM..(node + 1) * DIM].copy_from_slice(&bad_row(node / stride));
        bad.push(node as u32);
    }
    (out, bad)
}

/// brute-force の Cosine top-k（除外行 `bad` を候補から外した対照）。
fn brute_cosine_excluding(rows: &[f32], q: &[f32], bad: &[u32]) -> Vec<u32> {
    let mut v: Vec<(f64, u32)> = rows
        .chunks_exact(DIM)
        .enumerate()
        .filter(|(i, _)| !bad.contains(&(*i as u32)))
        .map(|(i, r)| (-cos64(r, q), i as u32))
        .collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    v.into_iter().take(K).map(|(_, i)| i).collect()
}

/// INDEX-7・SQL-33（Issue #2241 AC1）: 正規化できない行を散在させても構築は成功し、F32・F16・
/// I8 常駐 × 逐次・並列のどれでも、探索結果（マスクなし・除外ノードを含むマスク）に除外行が
/// 現れない。除外ノードは全層で隣接が空で、全 member が entry から到達できる。
#[test]
fn excluded_rows_never_appear_in_results() {
    let (rows, queries) = fixture();
    let (rows, bad) = scatter_bad_rows(&rows, 20);
    assert!(bad.len() >= 90);
    for p in [
        ResidentPrecision::F32,
        ResidentPrecision::F16,
        ResidentPrecision::I8,
    ] {
        for threads in [1usize, 4] {
            let index = build(HnswMetric::Cosine, p, &rows, threads);
            let tag = format!("{p:?} threads={threads}");
            assert_eq!(index.len(), ROWS, "{tag}");
            assert_eq!(index.member_count(), ROWS - bad.len(), "{tag}");
            assert!(index.excludes_non_normalizable(), "{tag}");
            let entry = index.entry_point().expect("entry");
            assert!(index.is_member(entry), "{tag}: entry must be a member");
            for &b in &bad {
                assert!(!index.is_member(b), "{tag}");
                assert_eq!(index.level_of(b), Some(0), "{tag} node={b}");
                assert!(
                    index.neighbors(0, b).is_none_or(<[u32]>::is_empty),
                    "{tag} node={b}"
                );
            }
            // 入辺が無い: どの member の隣接にも除外ノードは現れない。
            for node in 0..ROWS as u32 {
                if !index.is_member(node) {
                    continue;
                }
                for level in 0..=index.level_of(node).unwrap_or(0) {
                    for nb in index.neighbors(level, node).unwrap_or(&[]) {
                        assert!(index.is_member(*nb), "{tag} node={node} -> {nb}");
                    }
                }
            }
            let mut all = NodeMask::new(ROWS);
            for n in 0..ROWS as u32 {
                all.set(n);
            }
            assert!(
                index.is_mask_fully_reachable_with(&all, HopMode::OneHop),
                "{tag}: identity mask including excluded nodes must be reachable"
            );
            let mut scratch = HnswSearchScratch::default();
            for q in queries.chunks_exact(DIM) {
                let plain = index.search(q, K, 64, &mut scratch).unwrap();
                assert_eq!(plain.len(), K, "{tag}");
                let masked = index
                    .search_masked(q, K, 64, Some(&all), &mut scratch)
                    .unwrap();
                for h in plain.iter().chain(masked.iter()) {
                    assert!(!bad.contains(&(h.id as u32)), "{tag} id={}", h.id);
                }
            }
        }
    }
}

/// INDEX-7・SQL-33（Issue #2241 AC2）: 除外行を散在させても、除外行を除いた対照に対する
/// Recall@10 は除外なしの既存閾値（ef=64 で 0.95、ef=256 で 0.99。F16・SQ8 は既存の常駐精度
/// テストの床）を下回らない。
#[test]
fn recall_with_excluded_rows_meets_thresholds() {
    let (rows, queries) = fixture();
    let (rows, bad) = scatter_bad_rows(&rows, 20);
    let measure = |index: &HnswIndex, ef: usize| {
        let mut scratch = HnswSearchScratch::default();
        let mut hit = 0usize;
        for q in queries.chunks_exact(DIM) {
            let truth = brute_cosine_excluding(&rows, q, &bad);
            let got = index.search(q, K, ef, &mut scratch).unwrap();
            hit += got
                .iter()
                .filter(|h| truth.contains(&(h.id as u32)))
                .count();
        }
        hit as f64 / (QUERIES * K) as f64
    };
    for threads in [1usize, 4] {
        let index = build(HnswMetric::Cosine, ResidentPrecision::F32, &rows, threads);
        let r64 = measure(&index, 64);
        let r256 = measure(&index, 256);
        eprintln!("cosine excluded threads={threads} recall ef64={r64} ef256={r256}");
        assert!(r64 >= 0.95, "threads={threads} ef=64 recall={r64}");
        assert!(r256 >= 0.99, "threads={threads} ef=256 recall={r256}");
    }
    for (p, floor) in [(ResidentPrecision::F16, 0.7), (ResidentPrecision::I8, 0.5)] {
        let index = build(HnswMetric::Cosine, p, &rows, 1);
        let r = measure(&index, 64);
        eprintln!("cosine excluded {p:?} recall={r}");
        assert!(r >= floor, "{p:?} recall={r}");
    }
}

/// INDEX-7・SQL-33（Issue #2241 AC2 の強い検証）: 除外行を末尾に足した入力の逐次構築は、除外行
/// が無い入力の逐次構築と member 部分のグラフがビット同一で、全クエリの探索結果（id と
/// スコアのビット）が一致する（レベル割当の乱数は全行ぶん順に引くため）。
#[test]
fn appending_excluded_rows_does_not_change_member_graph() {
    let (rows, queries) = fixture();
    let mut extended = rows.clone();
    for i in 0..60 {
        extended.extend(bad_row(i));
    }
    for p in [
        ResidentPrecision::F32,
        ResidentPrecision::F16,
        ResidentPrecision::I8,
    ] {
        let plain = build(HnswMetric::Cosine, p, &rows, 1);
        let with_bad = build(HnswMetric::Cosine, p, &extended, 1);
        assert_eq!(with_bad.len(), ROWS + 60);
        assert_eq!(with_bad.member_count(), ROWS);
        assert_eq!(plain.entry_point(), with_bad.entry_point(), "{p:?}");
        for node in 0..ROWS as u32 {
            assert_eq!(plain.level_of(node), with_bad.level_of(node), "{p:?}");
            for level in 0..=plain.level_of(node).unwrap_or(0) {
                assert_eq!(
                    plain.neighbors(level, node),
                    with_bad.neighbors(level, node),
                    "{p:?} node={node} level={level}"
                );
            }
        }
        let mut s0 = HnswSearchScratch::default();
        let mut s1 = HnswSearchScratch::default();
        for q in queries.chunks_exact(DIM) {
            let a = plain.search(q, K, 64, &mut s0).unwrap();
            let b = with_bad.search(q, K, 64, &mut s1).unwrap();
            assert_eq!(a.len(), b.len());
            for (x, y) in a.iter().zip(b.iter()) {
                assert_eq!(x.id, y.id, "{p:?}");
                assert_eq!(x.score.to_bits(), y.score.to_bits(), "{p:?}");
            }
        }
    }
}

/// Issue #2241 の境界: 全行除外・member が 1 行だけ・先頭が除外、のいずれも構築が成功し、
/// entry は最初の member になる。全行除外の探索は空の結果。
#[test]
fn excluded_row_boundaries() {
    let params = HnswParams::default();
    let good =
        |i: usize| -> Vec<f32> { (0..DIM).map(|d| ((i * 7 + d) % 11) as f32 + 1.0).collect() };
    for threads in [1usize, 4] {
        // 全行除外。
        let mut all_bad = Vec::new();
        for i in 0..5 {
            all_bad.extend(bad_row(i));
        }
        let index = HnswIndex::build_with_metric(
            params,
            HnswMetric::Cosine,
            ResidentPrecision::F32,
            DIM as u32,
            &all_bad,
            1,
            threads,
        )
        .unwrap();
        assert_eq!(index.entry_point(), None);
        assert_eq!(index.member_count(), 0);
        let mut scratch = HnswSearchScratch::default();
        assert!(index
            .search(&good(0), K, 64, &mut scratch)
            .unwrap()
            .is_empty());

        // 行 0・1 が除外、member は行 2 の 1 行だけ。
        let mut rows = Vec::new();
        rows.extend(bad_row(0));
        rows.extend(bad_row(1));
        rows.extend(good(2));
        rows.extend(bad_row(2));
        let index = HnswIndex::build_with_metric(
            params,
            HnswMetric::Cosine,
            ResidentPrecision::F32,
            DIM as u32,
            &rows,
            1,
            threads,
        )
        .unwrap();
        assert_eq!(index.entry_point(), Some(2));
        let hits = index.search(&good(2), K, 64, &mut scratch).unwrap();
        assert_eq!(hits.iter().map(|h| h.id).collect::<Vec<_>>(), vec![2]);
    }
    // 先頭が除外で、並列経路（n > SEQUENTIAL_PREFIX_NODES）でも entry が member になる。
    let (mut rows, _) = fixture();
    for node in 0..40 {
        rows[node * DIM..(node + 1) * DIM].copy_from_slice(&bad_row(node));
    }
    let index = build(HnswMetric::Cosine, ResidentPrecision::F32, &rows, 4);
    assert_eq!(index.member_count(), ROWS - 40);
    assert!(index.entry_point().is_some_and(|e| index.is_member(e)));
}

/// Issue #2241（codex P1・Bugbot）: `build_parallel_with_metric` の逐次縮退（行数が
/// `SEQUENTIAL_PREFIX_NODES` 以下）でも、Cosine のゼロノルム行は正規化・除外の共通経路を通り
/// member から外れる。
#[test]
fn parallel_entry_sequential_fallback_excludes_zero_norm_rows() {
    let (rows, _) = fixture();
    let n = 40usize;
    assert!(n <= SEQUENTIAL_PREFIX_NODES);
    let mut small = rows[..n * DIM].to_vec();
    for c in 0..DIM {
        small[3 * DIM + c] = 0.0;
        small[n * DIM - DIM + c] = 0.0;
    }
    let index = HnswIndex::build_parallel_with_metric(
        HnswParams::default(),
        HnswMetric::Cosine,
        ResidentPrecision::F32,
        DIM as u32,
        &small,
        42,
    )
    .unwrap();
    assert_eq!(index.len(), n);
    assert_eq!(index.member_count(), n - 2);
    assert!(!index.is_member(3));
    assert!(!index.is_member(n as u32 - 1));
    let entry = index.entry_point().expect("entry");
    assert!(index.is_member(entry));
    let mut scratch = HnswSearchScratch::default();
    let q = &small[5 * DIM..6 * DIM];
    for h in index.search(q, K, 64, &mut scratch).unwrap() {
        assert!(index.is_member(h.id as u32), "id={}", h.id);
    }
}

/// 非 Cosine の metric は除外規則を持たない（`members == None`。既存経路と同一）。
/// 除外のない Cosine は `members` を持たないが、規則（`node_matches`・Overlay のゲート）は有効。
#[test]
fn non_cosine_metrics_have_no_exclusion() {
    let (rows, _) = fixture();
    for metric in [HnswMetric::InnerProduct, HnswMetric::L2, HnswMetric::L1] {
        let index = build(metric, ResidentPrecision::F32, &rows, 1);
        assert!(!index.excludes_non_normalizable(), "{metric:?}");
        assert_eq!(index.member_count(), ROWS, "{metric:?}");
    }
    let index = build(HnswMetric::Cosine, ResidentPrecision::F32, &rows, 1);
    assert!(index.excludes_non_normalizable());
    assert_eq!(index.member_count(), ROWS);
}

/// f32 カーネルのスコア降順（同点は id 昇順）で全行を並べた先頭 K 件（完全一致の対照）。
fn exact_top_k(rows: &[f32], q: &[f32], score: impl Fn(&[f32], &[f32]) -> f32) -> Vec<(u64, f32)> {
    let mut v: Vec<(u64, f32)> = rows
        .chunks_exact(DIM)
        .enumerate()
        .map(|(i, r)| (i as u64, score(r, q)))
        .collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    v.truncate(K);
    v
}

fn assert_exact_match(metric: HnswMetric, score: impl Fn(&[f32], &[f32]) -> f32 + Copy) {
    let (rows, queries) = fixture();
    let index = build(metric, ResidentPrecision::F32, &rows, 1);
    let mut scratch = HnswSearchScratch::default();
    for q in queries.chunks_exact(DIM).take(30) {
        let truth = exact_top_k(&rows, q, score);
        let got = index.search(q, K, ROWS, &mut scratch).unwrap();
        let got: Vec<(u64, u32)> = got.iter().map(|h| (h.id, h.score.to_bits())).collect();
        let want: Vec<(u64, u32)> = truth.iter().map(|&(i, s)| (i, s.to_bits())).collect();
        assert_eq!(got, want, "{metric:?}");
    }
}

/// INDEX-7・SQL-33: L1 の探索結果（ef=ROWS）は brute-force と id・順序・スコアが一致する。
#[test]
fn l1_search_matches_brute_force_exactly() {
    assert_exact_match(HnswMetric::L1, |a, b| -crate::kernel::l1(a, b));
}

/// INDEX-7・SQL-33: 負の内積（`<#>`）は内積グラフを再利用でき、`-dot` 昇順の全探索と
/// id・順序・スコア（符号反転）が一致する。
#[test]
fn negative_inner_product_reuses_inner_product_graph() {
    assert_exact_match(HnswMetric::InnerProduct, crate::kernel::dot);
    let (rows, queries) = fixture();
    let index = build(HnswMetric::InnerProduct, ResidentPrecision::F32, &rows, 1);
    let mut scratch = HnswSearchScratch::default();
    for q in queries.chunks_exact(DIM).take(30) {
        let got = index.search(q, K, ROWS, &mut scratch).unwrap();
        // 距離 -dot の昇順（同点は id 昇順）に並べ替えても出力順と変わらない。
        let mut by_dist: Vec<(u64, f32)> = got.iter().map(|h| (h.id, -h.score)).collect();
        by_dist.sort_by(|a, b| a.1.total_cmp(&b.1).then(a.0.cmp(&b.0)));
        let ids: Vec<u64> = got.iter().map(|h| h.id).collect();
        assert_eq!(by_dist.iter().map(|x| x.0).collect::<Vec<_>>(), ids);
    }
}

/// INDEX-7・SQL-33: L1 の Recall@10（ef=64 で 0.95 以上、ef=256 で 0.99 以上）。
#[test]
fn l1_recall_meets_thresholds() {
    let (rows, queries) = fixture();
    let index = build(HnswMetric::L1, ResidentPrecision::F32, &rows, 1);
    assert_eq!(index.metric(), HnswMetric::L1);
    let r64 = recall(&index, &rows, &queries, 64, HnswMetric::L1);
    let r256 = recall(&index, &rows, &queries, 256, HnswMetric::L1);
    eprintln!("l1 recall ef64={r64} ef256={r256}");
    assert!(r64 >= 0.95, "ef=64 recall={r64}");
    assert!(r256 >= 0.99, "ef=256 recall={r256}");
}

/// 内積（負の内積）の Recall@10。非正規化コーパスでの値を測定して記録する。
#[test]
fn inner_product_recall_measured() {
    let (rows, queries) = fixture();
    let index = build(HnswMetric::InnerProduct, ResidentPrecision::F32, &rows, 1);
    let r64 = recall(&index, &rows, &queries, 64, HnswMetric::InnerProduct);
    let r256 = recall(&index, &rows, &queries, 256, HnswMetric::InnerProduct);
    eprintln!("ip recall ef64={r64} ef256={r256}");
    assert!(r64 >= 0.95, "ef=64 recall={r64}");
    assert!(r256 >= 0.99, "ef=256 recall={r256}");
}

/// L1 は f16・SQ8 を要求しても F32 へ縮退し、結果は F32 の L1 と同一。
#[test]
fn l1_degrades_to_f32_resident() {
    let (rows, queries) = fixture();
    let base = build(HnswMetric::L1, ResidentPrecision::F32, &rows, 1);
    let mut s0 = HnswSearchScratch::default();
    let mut s1 = HnswSearchScratch::default();
    for p in [ResidentPrecision::F16, ResidentPrecision::I8] {
        let index = build(HnswMetric::L1, p, &rows, 1);
        assert_eq!(index.resident_precision(), ResidentPrecision::F32);
        for q in queries.chunks_exact(DIM).take(20) {
            let a = base.search(q, K, 64, &mut s0).unwrap();
            let b = index.search(q, K, 64, &mut s1).unwrap();
            assert_eq!(a, b);
        }
    }
}

/// L1 の近傍選択ヒューリスティック: 近接して冗長な候補は刈られ、遠い候補が残る。
#[test]
fn l1_heuristic_prunes_redundant_and_keeps_far() {
    let dim = 2usize;
    let a = [1.0f32, 0.0];
    let b = [1.1f32, 0.1];
    let c = [-5.0f32, 0.0];
    let vectors: Vec<f32> = [a, b, c].concat();
    let q = [0.0f32, 0.0];
    let score = |v: &[f32; 2]| -crate::kernel::l1(v, &q);
    let mut cands: Vec<ScoredNode> = [&a, &b, &c]
        .iter()
        .enumerate()
        .map(|(i, v)| ScoredNode {
            node: i as u32,
            score: score(v),
        })
        .collect();
    cands.sort_by(|x, y| y.score.total_cmp(&x.score));
    let selected =
        select_neighbors_heuristic_free(HnswMetric::L1, &cands, 2, dim, &vectors).unwrap();
    assert!(selected.contains(&0));
    assert!(selected.contains(&2));
    assert!(!selected.contains(&1));
}

/// L1 の fail-closed: 絶対差の総和が f32 を超えると構築拒否、非有限クエリは探索拒否、
/// ゼロクエリは（正規化不要なので）受理する。
#[test]
fn l1_fail_closed_and_zero_query_ok() {
    let big = vec![3.0e38f32, 0.0, -3.0e38, 0.0];
    let r = HnswIndex::build_with_metric(
        HnswParams::default(),
        HnswMetric::L1,
        ResidentPrecision::F32,
        2,
        &big,
        1,
        1,
    );
    assert!(matches!(r, Err(HnswError::NonFiniteScore { .. })), "{r:?}");
    let (rows, _) = fixture();
    let index = build(HnswMetric::L1, ResidentPrecision::F32, &rows, 1);
    let mut scratch = HnswSearchScratch::default();
    assert!(matches!(
        index.search(&[f32::NAN; DIM], K, 64, &mut scratch),
        Err(HnswError::NonFiniteQuery)
    ));
    assert!(matches!(
        index.search(&[f32::INFINITY; DIM], K, 64, &mut scratch),
        Err(HnswError::NonFiniteQuery)
    ));
    let hits = index.search(&[0.0f32; DIM], K, 64, &mut scratch).unwrap();
    assert_eq!(hits.len(), K);
}

/// L1 の `node_matches`: 正規化せず生の行をビット比較する。
#[test]
fn l1_node_matches_compares_raw_rows() {
    let (rows, _) = fixture();
    let index = build(HnswMetric::L1, ResidentPrecision::F32, &rows, 1);
    for node in [0usize, 5, 1999] {
        let raw = &rows[node * DIM..(node + 1) * DIM];
        assert_eq!(index.node_matches(node as u32, raw), Some(true));
        let o = (node + 1) % ROWS;
        let other = &rows[o * DIM..(o + 1) * DIM];
        assert_eq!(index.node_matches(node as u32, other), Some(false));
    }
}
