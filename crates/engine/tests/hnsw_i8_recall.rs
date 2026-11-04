//! I8（SQ8）常駐 opt-in（Issue #521・#522）が候補生成に与える影響を、
//! F32 常駐と同一コーパス・同一クエリでの brute-force（`engine::kernel::
//! CpuScalarProvider`）対照 Recall@10 で `ef ∈ {64, 128, 256}` ごとに実測する
//! （Issue #523・R5「oversampling（候補幅 `ef`）で補えるか」の判断材料）。
//!
//! 層 A（常時実行・縮小フィクスチャ）は「I8 の Recall@10 が F32 と同水準
//! （`>= F32 − 0.15`。I8（8-bit 対称量子化）は F16 よりも探索順序への影響が
//! 大きく、実測に基づき本リポ独自の実装既定値として設定した余裕。
//! `hnsw_cache.rs::min_recall_for` の絶対下限 0.8 とも整合する）を
//! ef=64 で維持する」ことの回帰保護。層 B（`#[ignore]`・`make hnsw-i8-recall`・
//! N=10,000・dim=128・release 実行）が `ef` 掃引表（F32 vs I8 × ef 64/128/256）
//! を出力する受け入れ条件の正本。`tests/hnsw_search.rs` のヘルパ
//! （`TestRng`・`gen_clustered_corpus`・`gen_queries`・`gen_uniform_corpus`・
//! `recall_at_10`）を crate 外の公開 API のみで独立に複製する（結合テストの
//! 既存流儀。`tests/*.rs` は個別バイナリのため `mod` で共有できない）。
//!
//! Issue #2001: 本ファイルの層 A・B は内積 metric 専用。`HnswMetric` が `pub(crate)` で
//! 結合テストから L2・L1・Cosine の索引を作れないため、metric 別（Cosine の F16・I8 常駐を含む）の
//! 再計測は crate 内 `hnsw::metric_recall_tests`（`make hnsw-metric-recall`）が担う。

use std::collections::HashSet;

use engine::hnsw::{HnswIndex, HnswParams, HnswSearchScratch, ResidentPrecision};
use engine::isa;
use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};

/// 決定的シードの xorshift64*（`tests/hnsw_search.rs::TestRng` の複製）。
struct TestRng {
    state: u64,
}

impl TestRng {
    fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_f32(&mut self) -> f32 {
        let bits = (self.next_u64() >> 40) as u32;
        (bits as f32) / (1u32 << 24) as f32
    }

    fn next_unit(&mut self) -> f32 {
        self.next_f32() * 2.0 - 1.0
    }
}

fn normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// `tests/hnsw_search.rs::gen_clustered_corpus` の複製。
fn gen_clustered_corpus(seed: u64, dim: usize, rows: usize, clusters: usize) -> Vec<f32> {
    let mut center_rng = TestRng::new(seed ^ 0xC1C1_C1C1_C1C1_C1C1);
    let centers: Vec<Vec<f32>> = (0..clusters.max(1))
        .map(|_| (0..dim).map(|_| center_rng.next_unit()).collect())
        .collect();
    let mut rng = TestRng::new(seed);
    let mut out = Vec::with_capacity(rows * dim);
    for i in 0..rows {
        let center = &centers[i % centers.len()];
        let mut v: Vec<f32> = center.iter().map(|c| c + rng.next_unit() * 0.2).collect();
        normalize(&mut v);
        out.extend(v);
    }
    out
}

/// `tests/hnsw_search.rs::gen_query`／`gen_queries` の複製。
fn gen_query(corpus_seed: u64, query_seed: u64, dim: usize, clusters: usize) -> Vec<f32> {
    let mut center_rng = TestRng::new(corpus_seed ^ 0xC1C1_C1C1_C1C1_C1C1);
    let centers: Vec<Vec<f32>> = (0..clusters.max(1))
        .map(|_| (0..dim).map(|_| center_rng.next_unit()).collect())
        .collect();
    let mut rng = TestRng::new(query_seed);
    let center_idx = (rng.next_u64() as usize) % centers.len();
    let mut v: Vec<f32> = centers[center_idx]
        .iter()
        .map(|c| c + rng.next_unit() * 0.2)
        .collect();
    normalize(&mut v);
    v
}

fn gen_queries(
    corpus_seed: u64,
    query_seed: u64,
    dim: usize,
    clusters: usize,
    count: usize,
) -> Vec<Vec<f32>> {
    (0..count)
        .map(|i| {
            gen_query(
                corpus_seed,
                query_seed.wrapping_add(i as u64).wrapping_mul(0x9E37_79B1),
                dim,
                clusters,
            )
        })
        .collect()
}

/// `tests/hnsw_search.rs::gen_uniform_corpus`／`gen_uniform_queries` の複製
/// （informational 参考値専用。3.3 節と同じ位置づけ）。
fn gen_uniform_corpus(seed: u64, dim: usize, rows: usize) -> Vec<f32> {
    let mut rng = TestRng::new(seed);
    let mut out = Vec::with_capacity(rows * dim);
    for _ in 0..rows {
        let mut v: Vec<f32> = (0..dim).map(|_| rng.next_unit()).collect();
        normalize(&mut v);
        out.extend(v);
    }
    out
}

fn gen_uniform_queries(seed: u64, dim: usize, count: usize) -> Vec<Vec<f32>> {
    (0..count)
        .map(|i| {
            let mut rng = TestRng::new(seed.wrapping_add(i as u64).wrapping_mul(0x9E37_79B1));
            let mut v: Vec<f32> = (0..dim).map(|_| rng.next_unit()).collect();
            normalize(&mut v);
            v
        })
        .collect()
}

/// brute-force（`CpuScalarProvider`）対照で Recall@10 を計測する
/// （`tests/hnsw_search.rs::recall_at_10` の複製）。
fn recall_at_10(
    index: &HnswIndex,
    vectors: &[f32],
    dim: usize,
    rows: usize,
    ef: usize,
    queries: &[Vec<f32>],
) -> f64 {
    let ids: Vec<u64> = (0..rows as u64).collect();
    let provider = CpuScalarProvider;
    let mut scratch = HnswSearchScratch::default();
    let mut hits_total = 0usize;
    for query in queries {
        let brute = provider
            .search(SearchInput {
                ids: &ids,
                vectors,
                dim: dim as u32,
                query,
                k: 10,
            })
            .expect("brute-force search must succeed");
        let brute_ids: HashSet<u64> = brute.iter().map(|h| h.id).collect();

        let hnsw = index
            .search(query, 10, ef, &mut scratch)
            .expect("hnsw search must succeed");
        let hit = hnsw.iter().filter(|h| brute_ids.contains(&h.id)).count();
        hits_total += hit;
    }
    hits_total as f64 / (queries.len() as f64 * 10.0)
}

/// I8 常駐から `oversample_k`（`>= 10`）件を候補として取得し、各候補を
/// **元の f32 ベクトル**（量子化前）で `query` との内積を再計算してから
/// 上位 10 件を選び直した Recall@10 を brute-force 対照で計測する
/// （codex-review 指摘対応・PR #621。`ef` 掃引〔候補幅＝探索の到達範囲〕と
/// oversampling＋再採点〔量子化スコアで丸めた順位を f32 で引き直す〕は
/// 別の操作であり、`recall_at_10` の固定 `k=10` 呼び出しだけでは
/// 「探索経路が量子化ノイズで候補そのものを取りこぼしているのか」
/// 「取得できた候補の中で量子化スコアの順位が f32 の真の順位と食い違って
/// いるだけなのか」を区別できない。本関数は後者（順位の食い違い）を
/// f32 再採点で解消できるかを見る）。
fn recall_at_10_oversample_rescored(
    index: &HnswIndex,
    vectors: &[f32],
    dim: usize,
    rows: usize,
    ef: usize,
    oversample_k: usize,
    queries: &[Vec<f32>],
) -> f64 {
    assert!(oversample_k >= 10, "oversample_k must cover top-10");
    let ids: Vec<u64> = (0..rows as u64).collect();
    let provider = CpuScalarProvider;
    let mut scratch = HnswSearchScratch::default();
    let kernel = isa::detect();
    let mut hits_total = 0usize;
    for query in queries {
        let brute = provider
            .search(SearchInput {
                ids: &ids,
                vectors,
                dim: dim as u32,
                query,
                k: 10,
            })
            .expect("brute-force search must succeed");
        let brute_ids: HashSet<u64> = brute.iter().map(|h| h.id).collect();

        // I8 索引から量子化スコアで oversample_k 件を取得し、各候補を
        // 元の f32 ベクトル（量子化前）で再採点してから上位 10 件を選び直す
        // （最終スコアは常に f32 の `kernel::dot` で再計算する既定契約
        // 〔`docs/design/simd-intrinsics-adoption.md` 決定 5〕をこのテスト
        // ハーネス自身でも踏襲する）。
        let candidates = index
            .search(query, oversample_k, ef, &mut scratch)
            .expect("hnsw search must succeed");
        let mut rescored: Vec<(u64, f32)> = candidates
            .iter()
            .map(|hit| {
                let row = &vectors[hit.id as usize * dim..hit.id as usize * dim + dim];
                (hit.id, kernel.dot(row, query))
            })
            .collect();
        rescored.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        rescored.truncate(10);
        let hit = rescored
            .iter()
            .filter(|(id, _)| brute_ids.contains(id))
            .count();
        hits_total += hit;
    }
    hits_total as f64 / (queries.len() as f64 * 10.0)
}

// --------------------------------------------------
// 層 A（常時 `#[test]`・小規模・debug 実行で数秒以内を目標）
// --------------------------------------------------

/// I8 常駐の Recall@10（ef=64）が、同一コーパス・同一クエリの F32 常駐対比
/// `F32 − 0.15` 以上（実測に基づき設定した本リポ独自の実装既定値。Issue
/// #523・`hnsw_cache.rs::min_recall_for` と同じ判断）であることを固定する
/// 回帰保護。受け入れ条件の正本は層 B の
/// `ef` 掃引（`make hnsw-i8-recall`）。
#[test]
fn i8_recall_at_10_matches_f32_within_margin_on_small_fixture() {
    let dim = 32;
    let rows = 2_000;
    let vectors = gen_clustered_corpus(0xA5A5_1234_0000, dim, rows, 20);
    let params = HnswParams::default();
    let f32_index = HnswIndex::build(params, dim as u32, &vectors, 0xF00D_1234).unwrap();
    let i8_index = HnswIndex::build_with_precision(
        params,
        ResidentPrecision::I8,
        dim as u32,
        &vectors,
        0xF00D_1234,
    )
    .unwrap();
    assert_eq!(
        i8_index.resident_precision(),
        ResidentPrecision::I8,
        "this fixture's finite embeddings must not trigger the D6 fallback"
    );
    let queries = gen_queries(0xA5A5_1234_0000, 0x51DE_0001, dim, 20, 100);

    let f32_recall = recall_at_10(&f32_index, &vectors, dim, rows, 64, &queries);
    let i8_recall = recall_at_10(&i8_index, &vectors, dim, rows, 64, &queries);
    assert!(
        i8_recall >= f32_recall - 0.15,
        "I8 Recall@10(ef=64) = {i8_recall} must be >= F32 Recall@10(ef=64) = {f32_recall} \
         - 0.15 (small fixture regression guard)"
    );
}

// --------------------------------------------------
// 層 B（`#[ignore]`・受け入れ条件の正本。`make hnsw-i8-recall` から release
// 実行する。debug では 10k×dim128 の HnswIndex::build が数十秒〜規模になるため
// 常時実行対象からは除外する。`tests/hnsw_search.rs::recall_at_10_meets_
// threshold_on_large_fixture` と同型の位置づけ）
// --------------------------------------------------

/// F32 vs I8 × `ef ∈ {64, 128, 256}` の brute-force 対照 Recall@10 掃引表を
/// 出力する（Issue #523・R5）。クラスタ構造ありコーパスが受け入れ判定の対象、
/// 一様乱数のみのコーパスは informational 参考値（`tests/hnsw_search.rs`
/// 「受け入れ判定はクラスタ構造ありフィクスチャの範囲に限定される」方針と
/// 同型）。
#[test]
#[ignore]
fn ef_sweep_recall_table_clustered_and_uniform_corpus() {
    let dim = 128;
    let rows = 10_000;
    let clusters = 80;

    let clustered = gen_clustered_corpus(0xBEEF_0001, dim, rows, clusters);
    let clustered_queries = gen_queries(0xBEEF_0001, 0x1357_9BDF, dim, clusters, 200);
    let uniform = gen_uniform_corpus(0xBEEF_0002, dim, rows);
    let uniform_queries = gen_uniform_queries(0x2468_ACE0, dim, 200);

    let params = HnswParams::default();
    let f32_clustered = HnswIndex::build(params, dim as u32, &clustered, 0xC0FF_EE01).unwrap();
    let i8_clustered = HnswIndex::build_with_precision(
        params,
        ResidentPrecision::I8,
        dim as u32,
        &clustered,
        0xC0FF_EE01,
    )
    .unwrap();
    assert_eq!(i8_clustered.resident_precision(), ResidentPrecision::I8);

    let f32_uniform = HnswIndex::build(params, dim as u32, &uniform, 0xC0FF_EE02).unwrap();
    let i8_uniform = HnswIndex::build_with_precision(
        params,
        ResidentPrecision::I8,
        dim as u32,
        &uniform,
        0xC0FF_EE02,
    )
    .unwrap();
    assert_eq!(i8_uniform.resident_precision(), ResidentPrecision::I8);

    println!(
        "ef_sweep_recall_table: rows={rows} dim={dim} clusters={clusters} (Issue #523・R5。\
         数値基準ではなく実測値・spec 非公開値は含まない)"
    );
    for ef in [64usize, 128, 256] {
        let f32_c = recall_at_10(
            &f32_clustered,
            &clustered,
            dim,
            rows,
            ef,
            &clustered_queries,
        );
        let i8_c = recall_at_10(&i8_clustered, &clustered, dim, rows, ef, &clustered_queries);
        let f32_u = recall_at_10(&f32_uniform, &uniform, dim, rows, ef, &uniform_queries);
        let i8_u = recall_at_10(&i8_uniform, &uniform, dim, rows, ef, &uniform_queries);
        println!(
            "ef={ef}: clustered f32={f32_c:.4} i8={i8_c:.4} diff={:.4} | \
             uniform(informational) f32={f32_u:.4} i8={i8_u:.4} diff={:.4}",
            f32_c - i8_c,
            f32_u - i8_u,
        );
        assert!(
            i8_c >= f32_c - 0.15,
            "clustered corpus: I8 Recall@10(ef={ef}) = {i8_c} must be >= F32 Recall@10(ef={ef}) \
             = {f32_c} - 0.15"
        );
    }

    // oversampling（候補数を増やし元の f32 ベクトルで再採点）が Recall@10 の
    // ギャップを縮められるかを、`ef` 掃引とは独立に見る（codex-review 指摘
    // 対応・PR #621）。呼び出し引数の `ef` は 64 で固定するが、
    // `HnswIndex::search` 内部は探索幅を `ef.max(k)`（本関数の `k` は
    // `oversample_k`）へ引き上げるため、`oversample_k > 64` の測定点
    // （`oversample_k=100`）は実効探索幅が 100 になり、`ef=64` 単独の
    // 効果とは切り分けられない（codex-review 指摘対応・PR #621）。表には
    // 「引数 ef」に加え実効探索幅 `ef_eff = ef.max(oversample_k)` を明示し、
    // この点を明確にする。
    println!(
        "oversample_rescore_table: rows={rows} dim={dim} clusters={clusters} ef_arg=64 \
         (candidates rescored with original f32 vectors; ef_eff = ef_arg.max(oversample_k) \
         per HnswIndex::search internals; Issue #523・R5 codex-review 追記)"
    );
    for oversample_k in [10usize, 20, 50, 100] {
        let ef_arg = 64usize;
        let ef_eff = ef_arg.max(oversample_k);
        let i8_rescored = recall_at_10_oversample_rescored(
            &i8_clustered,
            &clustered,
            dim,
            rows,
            ef_arg,
            oversample_k,
            &clustered_queries,
        );
        println!(
            "oversample_k={oversample_k}: clustered i8_rescored={i8_rescored:.4} \
             (brute-force f32 top-10 対照。ef_arg={ef_arg} ef_eff={ef_eff})"
        );
    }
}
