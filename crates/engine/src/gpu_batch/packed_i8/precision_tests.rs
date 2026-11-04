//! i8 経路（SQ8）の metric 別精度・候補 Recall の CPU 実測テスト
//! （Issue #2004・親 #2002・CORE-8・CORE-16・SQL-33）。
//!
//! GPU の整数結果は CPU 参照 [`dot_i8_packed_ref`] とビット一致する契約のため、
//! 量子化誤差と候補 Recall は GPU 無しで決定的に測れる。本番と同じ `encode_rows`／
//! `quantize_query`／`reduce_top_k_prime`／`final_score` を通し、`i8_supports_metric` が
//! `true` の metric 全部が実装既定ゲート（oversample=4 で Recall@10 >= 0.9、かつ
//! oversample 1→2→4→8 で単調非減少）を満たすことを機械的に固定する。
//! 表は `cargo test -p engine --all-features --lib gpu_batch::packed_i8::precision_tests
//! -- --nocapture` で出力され、`docs/design/gpu-batch-i8-packed.md` §10 へ転記する。
//! 非対応 metric は値を出力するだけでアサートしない。

use super::*;
use crate::batch_search::ResidentMatrix;
use crate::kernel::row_norms_sq;
use crate::storage::Visibility;

const K: usize = 10;
const QUERIES: usize = 32;
const OVERSAMPLES: [usize; 4] = [1, 2, 4, 8];
const METRICS: [GpuBatchMetric; 3] = [
    GpuBatchMetric::InnerProduct,
    GpuBatchMetric::Cosine,
    GpuBatchMetric::L2,
];

/// 固定シードの LCG（oversample に依存させない）。
struct Lcg(u64);
impl Lcg {
    fn unit(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 40) as f32 / (1u64 << 24) as f32) * 2.0 - 1.0
    }
}

#[derive(Clone, Copy)]
enum Fixture {
    Uniform,
    Clustered,
    Offset,
    NormSpread,
}
impl Fixture {
    fn name(self) -> &'static str {
        match self {
            Fixture::Uniform => "F1-uniform",
            Fixture::Clustered => "F2-clustered",
            Fixture::Offset => "F3-offset+3",
            Fixture::NormSpread => "F4-norm-spread",
        }
    }
}

fn gen_row(fx: Fixture, rng: &mut Lcg, centers: &[Vec<f32>], i: usize, dim: usize) -> Vec<f32> {
    match fx {
        Fixture::Uniform => (0..dim).map(|_| rng.unit()).collect(),
        Fixture::Clustered => {
            let c = &centers[i % centers.len()];
            c.iter().map(|v| v + rng.unit() * 0.2).collect()
        }
        Fixture::Offset => (0..dim).map(|_| rng.unit() + 3.0).collect(),
        Fixture::NormSpread => {
            let mag = 10f32.powf(rng.unit() * 2.0);
            (0..dim).map(|_| rng.unit() * mag).collect()
        }
    }
}

#[derive(Default)]
struct Errs(Vec<f64>);
impl Errs {
    fn report(&mut self) -> (f64, f64, f64) {
        self.0.sort_by(|a, b| a.total_cmp(b));
        let n = self.0.len().max(1);
        let mean = self.0.iter().sum::<f64>() / n as f64;
        let p99 = self
            .0
            .get((n * 99 / 100).min(n - 1))
            .copied()
            .unwrap_or(0.0);
        let max = self.0.last().copied().unwrap_or(0.0);
        (mean, p99, max)
    }
}

fn exact_scores(metric: GpuBatchMetric, q: &[f32], decoded: &[f32], dim: usize) -> Vec<f32> {
    let qn = crate::kernel::dot(q, q);
    decoded
        .chunks(dim)
        .map(|row| final_score(metric, q, row, qn))
        .collect()
}

fn kth_threshold(scores: &[f32]) -> f32 {
    let mut v: Vec<f32> = scores.iter().copied().filter(|s| s.is_finite()).collect();
    v.sort_by(|a, b| b.total_cmp(a));
    v.get(K - 1).copied().unwrap_or(f32::NEG_INFINITY)
}

/// 本番の候補選出 -> 再スコア -> Top-k を CPU で再現し、返った slot 群を返す。
#[allow(clippy::too_many_arguments)]
fn pipeline_top_k(
    metric: GpuBatchMetric,
    decoded: &[f32],
    dim: usize,
    scales: &Sq8RowScales,
    packed: &[u32],
    norms: &[f32],
    q: &[f32],
    s_q: f32,
    qq: &[u32],
    oversample: usize,
) -> Vec<usize> {
    let stride = row_stride_for_dim(dim);
    let rows = decoded.len() / dim;
    let mut buf: Vec<(u32, i32)> = (0..rows)
        .map(|slot| {
            let row = &packed[slot * stride..(slot + 1) * stride];
            (slot as u32, dot_i8_packed_ref(row, qq))
        })
        .collect();
    let ctx = RankingCtx {
        metric,
        row_scales: scales,
        row_norms_sq: norms,
        query_scale: s_q,
    };
    let k_prime = (K * oversample).min(rows);
    reduce_top_k_prime(&mut buf, k_prime, &ctx).expect("reduce should succeed");
    sort_by_ranking_key(&mut buf, &ctx);
    let qn = crate::kernel::dot(q, q);
    let mut sel = TopKSelector::new(K);
    for (slot, _) in &buf {
        let row = &decoded[*slot as usize * dim..(*slot as usize + 1) * dim];
        let score = final_score(metric, q, row, qn);
        if score.is_finite() {
            sel.push(CandidateHit {
                id: u64::from(*slot),
                score,
            });
        }
    }
    sel.into_sorted_vec()
        .iter()
        .map(|h| h.id as usize)
        .collect()
}

#[test]
fn i8_metric_precision_and_candidate_recall_meet_gate_for_supported_metrics() {
    let rows = 2_000usize;
    let fixtures = [
        Fixture::Uniform,
        Fixture::Clustered,
        Fixture::Offset,
        Fixture::NormSpread,
    ];
    eprintln!("metric,fixture,dim,os1,os2,os4,os8,err_mean,err_p99,err_max,supported");
    for fx in fixtures {
        for dim in [128usize, 768] {
            let mut rng = Lcg(0x1234_5678_9ABC_DEF0 ^ (dim as u64));
            let centers: Vec<Vec<f32>> = (0..20)
                .map(|_| (0..dim).map(|_| rng.unit()).collect())
                .collect();
            let mut corpus = Vec::with_capacity(rows * dim);
            for i in 0..rows {
                corpus.extend(gen_row(fx, &mut rng, &centers, i, dim));
            }
            // 真値は本番と同じ f16 往復デコード行。
            let ids: Vec<u64> = (0..rows as u64).collect();
            let tenants = vec!["precision-tenant".to_string(); rows];
            let vis = vec![Visibility::Public; rows];
            let matrix =
                ResidentMatrix::build(&ids, &tenants, &vis, dim, &corpus).expect("matrix build");
            let mut decoded = Vec::with_capacity(rows * dim);
            let mut tmp = Vec::new();
            for i in 0..rows {
                matrix.row_f32_into(i, &mut tmp).expect("decode");
                decoded.extend_from_slice(&tmp);
            }
            let (scales, packed) = encode_rows(dim, rows, &decoded).expect("encode");
            let norms = row_norms_sq(&decoded, dim as u32).expect("norms");

            // クエリ: 半数は分布からの抽出、残りは既存行 + 小ノイズ。
            let mut queries: Vec<Vec<f32>> = Vec::new();
            for qi in 0..QUERIES {
                if qi % 2 == 0 {
                    queries.push(gen_row(fx, &mut rng, &centers, qi, dim));
                } else {
                    let base = (qi * 61) % rows;
                    let row = &decoded[base * dim..(base + 1) * dim];
                    let mag = row.iter().fold(0f32, |a, v| a.max(v.abs()));
                    queries.push(row.iter().map(|v| v + rng.unit() * 0.05 * mag).collect());
                }
            }

            for metric in METRICS {
                let mut recall_sum = [0f64; 4];
                let mut errs = Errs::default();
                for q in &queries {
                    let (s_q, qq) = quantize_query(q).expect("quantize");
                    let exact = exact_scores(metric, q, &decoded, dim);
                    let thr = kth_threshold(&exact);
                    let qn = crate::kernel::dot(q, q);
                    for (oi, os) in OVERSAMPLES.iter().enumerate() {
                        let got = pipeline_top_k(
                            metric, &decoded, dim, &scales, &packed, &norms, q, s_q, &qq, *os,
                        );
                        let ok = got.iter().filter(|s| exact[**s] >= thr).count();
                        recall_sum[oi] += ok as f64 / K as f64;
                    }
                    // 近似値の誤差（先頭 64 行を標本にする）。
                    for slot in 0..64.min(rows) {
                        let row_packed = &packed
                            [slot * row_stride_for_dim(dim)..(slot + 1) * row_stride_for_dim(dim)];
                        let sigma = f64::from(dot_i8_packed_ref(row_packed, &qq));
                        let s_i = f64::from(scales.scale(slot).unwrap_or(0.0));
                        let approx_dot = s_i * f64::from(s_q) * sigma;
                        let row = &decoded[slot * dim..(slot + 1) * dim];
                        let xn = f64::from(norms[slot]);
                        let ex_dot = f64::from(crate::kernel::dot(q, row));
                        let e = match metric {
                            GpuBatchMetric::InnerProduct => (approx_dot - ex_dot).abs(),
                            GpuBatchMetric::Cosine => {
                                let d = (xn * f64::from(qn)).sqrt();
                                if d > 0.0 {
                                    ((approx_dot - ex_dot) / d).abs()
                                } else {
                                    0.0
                                }
                            }
                            GpuBatchMetric::L2 => {
                                let ex = f64::from(crate::kernel::l2_sq(q, row));
                                let ap = xn + f64::from(qn) - 2.0 * approx_dot;
                                (ap - ex).abs() / ex.max(1e-9)
                            }
                            GpuBatchMetric::L1 => 0.0,
                        };
                        errs.0.push(e);
                    }
                }
                let r: Vec<f64> = recall_sum.iter().map(|s| s / QUERIES as f64).collect();
                let (mean, p99, max) = errs.report();
                let supported = i8_supports_metric(metric);
                eprintln!(
                    "{metric:?},{},{dim},{:.4},{:.4},{:.4},{:.4},{mean:.6},{p99:.6},{max:.6},{supported}",
                    fx.name(),
                    r[0],
                    r[1],
                    r[2],
                    r[3]
                );
                if supported {
                    assert!(
                        r[2] >= 0.9,
                        "{metric:?} {} dim={dim}: recall@10 (oversample=4) too low: {}",
                        fx.name(),
                        r[2]
                    );
                    for w in r.windows(2) {
                        assert!(
                            w[1] + 1e-12 >= w[0],
                            "{metric:?} {} dim={dim}: recall not monotone in oversample: {r:?}",
                            fx.name()
                        );
                    }
                }
            }
        }
    }
}
