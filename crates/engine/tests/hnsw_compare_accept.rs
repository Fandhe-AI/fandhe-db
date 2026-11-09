//! `benches/harness/hnsw_compare.rs`（自作 HNSW と usearch の構築時間・Recall・
//! 探索レイテンシ比較。Issue #402 系 ADR の実測補強）の回帰テスト。
//!
//! `hnsw_compare_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/hnsw_build_accept.rs` と同様、実測タイマー・env に依存しない
//! 時間非依存の契約のみを `#[path]` で取り込み `cargo test`〔`make ci` 対象〕で
//! 検証する）。usearch に依存しない部分（env 解析・速度比・Recall 集計・行分割・
//! 出力整形）は feature 無指定でも検証し、usearch 依存部分
//! （`usearch_adapter` モジュール）は `contrast-bench` feature 限定で検証する
//! （`tests/contrast_smoke.rs` と同一方針。`make test` は `--all-features` の
//! ため両方回る）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use harness::hnsw_compare::{
    average_recall_at_k, l2_normalize_corpus, parse_dim, parse_queries, parse_rows,
    parse_thread_ladder, partition_rows, ratio_self_over_usearch, refuse_under_github_actions,
    render_build_line, render_header_line, render_latency_line, render_ratio_line,
    render_recall_line, render_self_params_line, render_usearch_params_line, speedup,
    HnswCompareBenchError, ALLOWED_DIMS, DEFAULT_DIM, DEFAULT_QUERIES, DEFAULT_ROWS,
    MAX_QUERIES_GUARD, MAX_ROWS_GUARD,
};
use std::time::Duration;

// --- refuse_under_github_actions ---

#[test]
fn refuse_under_github_actions_rejects_when_true() {
    let err = refuse_under_github_actions(true).unwrap_err();
    assert_eq!(err, HnswCompareBenchError::RefusedUnderGitHubActions);
}

#[test]
fn refuse_under_github_actions_allows_when_false() {
    assert!(refuse_under_github_actions(false).is_ok());
}

// --- parse_rows ---

#[test]
fn parse_rows_accepts_valid_value() {
    assert_eq!(parse_rows(Some("12345")), 12345);
}

#[test]
fn parse_rows_falls_back_to_default_on_missing() {
    assert_eq!(parse_rows(None), DEFAULT_ROWS);
}

#[test]
fn parse_rows_falls_back_to_default_on_invalid() {
    assert_eq!(parse_rows(Some("not-a-number")), DEFAULT_ROWS);
}

#[test]
fn parse_rows_falls_back_to_default_on_zero() {
    assert_eq!(parse_rows(Some("0")), DEFAULT_ROWS);
}

#[test]
fn parse_rows_falls_back_to_default_on_guard_overflow() {
    let overflow = (MAX_ROWS_GUARD + 1).to_string();
    assert_eq!(parse_rows(Some(&overflow)), DEFAULT_ROWS);
}

#[test]
fn parse_rows_accepts_guard_boundary() {
    let boundary = MAX_ROWS_GUARD.to_string();
    assert_eq!(parse_rows(Some(&boundary)), MAX_ROWS_GUARD);
}

// --- parse_dim ---

#[test]
fn parse_dim_accepts_allowed_values() {
    for &dim in &ALLOWED_DIMS {
        assert_eq!(parse_dim(Some(&dim.to_string())), dim);
    }
}

#[test]
fn parse_dim_falls_back_to_default_on_missing() {
    assert_eq!(parse_dim(None), DEFAULT_DIM);
}

#[test]
fn parse_dim_falls_back_to_default_on_disallowed_value() {
    assert_eq!(parse_dim(Some("96")), DEFAULT_DIM);
}

// --- parse_queries ---

#[test]
fn parse_queries_accepts_valid_value() {
    assert_eq!(parse_queries(Some("50")), 50);
}

#[test]
fn parse_queries_falls_back_to_default_on_missing() {
    assert_eq!(parse_queries(None), DEFAULT_QUERIES);
}

#[test]
fn parse_queries_falls_back_to_default_on_guard_overflow() {
    let overflow = (MAX_QUERIES_GUARD + 1).to_string();
    assert_eq!(parse_queries(Some(&overflow)), DEFAULT_QUERIES);
}

// --- parse_thread_ladder ---

#[test]
fn parse_thread_ladder_uses_explicit_list_when_valid() {
    let ladder = parse_thread_ladder(Some("4,1,2"), 16, 8);
    assert_eq!(ladder, vec![1, 2, 4]);
}

#[test]
fn parse_thread_ladder_dedups_and_sorts() {
    let ladder = parse_thread_ladder(Some("2,2,1,1"), 16, 8);
    assert_eq!(ladder, vec![1, 2]);
}

#[test]
fn parse_thread_ladder_drops_values_exceeding_max() {
    let ladder = parse_thread_ladder(Some("1,2,99"), 16, 8);
    assert_eq!(ladder, vec![1, 2]);
}

#[test]
fn parse_thread_ladder_falls_back_to_default_ladder_when_all_invalid() {
    let ladder = parse_thread_ladder(Some("0,99,abc"), 16, 8);
    assert_eq!(ladder, vec![1, 2, 4, 8]);
}

#[test]
fn parse_thread_ladder_falls_back_to_default_ladder_when_missing() {
    let ladder = parse_thread_ladder(None, 16, 8);
    assert_eq!(ladder, vec![1, 2, 4, 8]);
}

#[test]
fn parse_thread_ladder_clamps_default_ladder_to_max_threads() {
    let ladder = parse_thread_ladder(None, 3, 16);
    assert_eq!(ladder.last(), Some(&3));
    assert!(ladder.iter().all(|&t| t <= 3));
}

#[test]
fn parse_thread_ladder_single_core_yields_singleton() {
    let ladder = parse_thread_ladder(None, 16, 1);
    assert_eq!(ladder, vec![1]);
}

// --- speedup ---

#[test]
fn speedup_without_baseline_is_one() {
    let sp = speedup(None, Duration::from_millis(10)).unwrap();
    assert!((sp - 1.0).abs() < f64::EPSILON);
}

#[test]
fn speedup_with_baseline_reflects_ratio() {
    let sp = speedup(Some(Duration::from_millis(100)), Duration::from_millis(25)).unwrap();
    assert!((sp - 4.0).abs() < 1e-9);
}

#[test]
fn speedup_rejects_zero_current() {
    let err = speedup(Some(Duration::from_millis(100)), Duration::ZERO).unwrap_err();
    assert_eq!(err, HnswCompareBenchError::InsufficientSamples);
}

// --- ratio_self_over_usearch ---

#[test]
fn ratio_self_over_usearch_computes_expected_value() {
    let ratio =
        ratio_self_over_usearch(Duration::from_millis(50), Duration::from_millis(25)).unwrap();
    assert!((ratio - 2.0).abs() < 1e-9);
}

#[test]
fn ratio_self_over_usearch_rejects_zero_usearch_median() {
    let err = ratio_self_over_usearch(Duration::from_millis(50), Duration::ZERO).unwrap_err();
    assert_eq!(err, HnswCompareBenchError::InsufficientSamples);
}

// --- average_recall_at_k ---

#[test]
fn average_recall_at_k_matches_manual_average() {
    let per_query = vec![
        (vec![1, 2, 3], vec![1, 2, 3]),  // 1.0
        (vec![1, 2, 3], vec![1, 2, 99]), // 2/3
    ];
    let avg = average_recall_at_k(&per_query).unwrap();
    assert!((avg - (1.0 + 2.0 / 3.0) / 2.0).abs() < 1e-9);
}

#[test]
fn average_recall_at_k_rejects_empty_input() {
    let err = average_recall_at_k(&[]).unwrap_err();
    assert_eq!(err, HnswCompareBenchError::InsufficientSamples);
}

// --- partition_rows ---

#[test]
fn partition_rows_covers_all_rows_without_gaps_or_overlap() {
    let ranges = partition_rows(10, 3);
    assert_eq!(ranges, vec![(0, 4), (4, 7), (7, 10)]);
}

#[test]
fn partition_rows_single_thread_covers_everything() {
    let ranges = partition_rows(7, 1);
    assert_eq!(ranges, vec![(0, 7)]);
}

#[test]
fn partition_rows_zero_threads_yields_empty() {
    let ranges = partition_rows(10, 0);
    assert!(ranges.is_empty());
}

#[test]
fn partition_rows_more_threads_than_rows_yields_empty_tail_ranges() {
    let ranges = partition_rows(2, 5);
    assert_eq!(ranges.len(), 5);
    let total: usize = ranges.iter().map(|&(s, e)| e - s).sum();
    assert_eq!(total, 2);
    // 空範囲（`s == e`）が含まれてよいが、範囲同士は連続していること。
    for pair in ranges.windows(2) {
        assert_eq!(pair[0].1, pair[1].0);
    }
}

// --- render_* (出力整形のスモーク。フォーマット崩れの回帰検出) ---

#[test]
fn render_lines_contain_expected_tokens() {
    let build = render_build_line("self", 4, Duration::from_millis(12), 2.5);
    assert!(build.contains("engine=self"));
    assert!(build.contains("threads=4"));

    let ratio = render_ratio_line(4, 1.25);
    assert!(ratio.contains("self_over_usearch=1.250x"));

    let recall = render_recall_line("usearch", 8, 0.9876);
    assert!(recall.contains("recall@10=0.9876"));

    let latency = render_latency_line("self", Duration::from_micros(42));
    assert!(latency.contains("median_us=42"));

    let header = render_header_line(1000, 64, 50, &[1, 2, 4]);
    assert!(header.contains("rows=1000"));
    assert!(header.contains("dim=64"));
    assert!(header.contains("queries=50"));
    assert!(header.contains("corpus=l2_normalized"));

    let self_params = render_self_params_line(16, 100, 64);
    assert!(self_params.contains("engine=self"));
    assert!(self_params.contains("m=16"));

    let usearch_params = render_usearch_params_line(16, 100, 64);
    assert!(usearch_params.contains("engine=usearch"));
    assert!(usearch_params.contains("connectivity=16"));
}

// --- l2_normalize_corpus（feature 非依存の共通正規化ヘルパ） ---

#[test]
fn l2_normalize_corpus_produces_unit_norm_rows() {
    // dim=3 の 2 行（ノルムが不揃い）を正規化し、各行のノルムが 1 になることを
    // 確認する（2 エンジン共通の正規化契約。`hnsw_compare.rs` モジュール
    // コメント「2 エンジン（self・usearch）共通のコーパス正規化ヘルパ」参照）。
    let vectors: Vec<f32> = vec![3.0, 4.0, 0.0, 1.0, 2.0, 2.0];
    let normalized = l2_normalize_corpus(&vectors, 3).expect("non-zero rows normalize");
    assert_eq!(normalized.len(), vectors.len());
    for row in normalized.chunks(3) {
        let norm_sq: f32 = row.iter().map(|v| v * v).sum();
        assert!(
            (norm_sq.sqrt() - 1.0).abs() < 1e-5,
            "normalized row must have unit L2 norm, got {norm_sq}"
        );
    }
}

#[test]
fn l2_normalize_corpus_rejects_zero_norm_row() {
    // 2 行目が全成分 0（ノルム 0）のケースは 0 除算・NaN 混入を招くため
    // fail-closed で拒否する契約（`HnswCompareBenchError::ZeroNormRow`）。
    let vectors: Vec<f32> = vec![1.0, 0.0, 0.0, 0.0];
    let err = l2_normalize_corpus(&vectors, 2).unwrap_err();
    assert_eq!(err, HnswCompareBenchError::ZeroNormRow(1));
}

#[test]
fn l2_normalize_corpus_rejects_zero_dimension() {
    // `dim == 0` を許すと 0 除算・空行の反復という未定義な挙動を招くため
    // fail-closed で拒否する契約（codex-review 指摘。是正前は `dim.max(1)`
    // で黙って 1 次元コーパス扱いへ丸められていた）。
    let vectors: Vec<f32> = vec![1.0, 2.0, 3.0];
    let err = l2_normalize_corpus(&vectors, 0).unwrap_err();
    assert_eq!(err, HnswCompareBenchError::ZeroDimension);
}

#[test]
fn l2_normalize_corpus_rejects_length_not_multiple_of_dim() {
    // `vectors.len()` が `dim` の倍数でない場合、末尾行が不完全なまま黙って
    // 切り捨てられる契約違反を検出し fail-closed で拒否する。
    let vectors: Vec<f32> = vec![1.0, 2.0, 3.0, 4.0, 5.0];
    let err = l2_normalize_corpus(&vectors, 3).unwrap_err();
    assert_eq!(
        err,
        HnswCompareBenchError::CorpusLengthNotMultipleOfDim { len: 5, dim: 3 }
    );
}

// --------------------------------------------------
// usearch 依存部分（`contrast-bench` feature 限定）
// --------------------------------------------------

#[cfg(feature = "contrast-bench")]
mod usearch_adapter_tests {
    use super::average_recall_at_k;
    use super::harness::hnsw_compare::usearch_adapter::{
        build_usearch_index_parallel, usearch_index_options, usearch_search_topk,
    };
    use super::harness::rng::DeterministicRng;
    use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};

    const ROWS: usize = 300;
    const DIM: usize = 16;
    const TOP_K: usize = 5;

    #[test]
    fn usearch_index_options_reflects_engine_defaults() {
        let options = usearch_index_options(DIM);
        assert_eq!(options.dimensions, DIM);
        assert_eq!(options.connectivity, 16);
        assert_eq!(options.expansion_add, 100);
        assert_eq!(options.expansion_search, 64);
        assert!(!options.multi);
    }

    /// 並列構築テストが使うクエリ件数。1 件だと 1 つの取りこぼしで閾値に触れるため、
    /// 複数件の平均 Recall で判定する（Issue #2255）。
    const RECALL_QUERIES: usize = 32;

    /// 単一・並列それぞれの平均 Recall@k の下限。元の 1 クエリ基準（4/5 = 0.8）を
    /// 下回らない値で、負荷下の実測最小（0.9125）に余裕を残して決めた。
    const MIN_AVG_RECALL: f64 = 0.85;

    /// 並列側が単一側からどこまで劣化してよいかの許容幅。
    const MAX_PARALLEL_DEGRADATION: f64 = 0.1;

    /// usearch の単一スレッド構築と 4 スレッド並列構築が、brute-force 参照
    /// （`CpuScalarProvider`）に対して近い平均 Recall を保つことの配線確認。
    ///
    /// 単一スレッド構築は挿入順とレベル乱数が固定で決定的だが、並列構築は
    /// 挿入の割り込み順でグラフが実行ごとに変わる（Issue #2255 の実測では 1 クエリ
    /// の top-5 が 4/5 や 3/5 になる実行が混ざった）。このため 1 クエリの個数一致
    /// ではなく複数クエリの平均 Recall で判定し、brute-force 比の下限と、
    /// 単一側に対する並列側の劣化幅の上限の両方を検査する。
    #[test]
    fn build_usearch_index_parallel_with_multiple_threads_matches_single_thread_recall() {
        let mut rng = DeterministicRng::new(11);
        let mut vectors = Vec::with_capacity(ROWS * DIM);
        for _ in 0..ROWS {
            vectors.extend(rng.next_vector(DIM));
        }
        let mut queries = Vec::with_capacity(RECALL_QUERIES * DIM);
        for _ in 0..RECALL_QUERIES {
            queries.extend(rng.next_vector(DIM));
        }

        let single = build_usearch_index_parallel(ROWS, DIM, &vectors, 1)
            .expect("single-threaded usearch build succeeds");
        let parallel = build_usearch_index_parallel(ROWS, DIM, &vectors, 4)
            .expect("parallel usearch build succeeds");

        assert_eq!(single.size(), ROWS);
        assert_eq!(parallel.size(), ROWS);

        let ids: Vec<u64> = (0..ROWS as u64).collect();
        let mut single_pairs = Vec::with_capacity(RECALL_QUERIES);
        let mut parallel_pairs = Vec::with_capacity(RECALL_QUERIES);
        for query in queries.chunks(DIM) {
            let reference = CpuScalarProvider
                .search(SearchInput {
                    ids: &ids,
                    vectors: &vectors,
                    dim: DIM as u32,
                    query,
                    k: TOP_K,
                })
                .expect("reference search succeeds");
            let expected: Vec<u64> = reference.into_iter().map(|hit| hit.id).collect();
            let single_topk = usearch_search_topk(&single, query, TOP_K)
                .expect("single-threaded search succeeds");
            let parallel_topk =
                usearch_search_topk(&parallel, query, TOP_K).expect("parallel search succeeds");
            single_pairs.push((expected.clone(), single_topk));
            parallel_pairs.push((expected, parallel_topk));
        }
        let single_avg = average_recall_at_k(&single_pairs).expect("single recall computes");
        let parallel_avg = average_recall_at_k(&parallel_pairs).expect("parallel recall computes");

        // usearch は近似探索のため厳密一致は要求しない（`contrast_smoke.rs` と異なり
        // `recall == 1.0` は要求しない）。brute-force 対照との平均 Recall で配線を確認する。
        assert!(
            single_avg >= MIN_AVG_RECALL,
            "single-threaded avg recall@{TOP_K}={single_avg} < {MIN_AVG_RECALL} \
             (queries={RECALL_QUERIES})"
        );
        assert!(
            parallel_avg >= MIN_AVG_RECALL,
            "parallel avg recall@{TOP_K}={parallel_avg} < {MIN_AVG_RECALL} \
             (queries={RECALL_QUERIES}, single={single_avg})"
        );
        assert!(
            parallel_avg >= single_avg - MAX_PARALLEL_DEGRADATION,
            "parallel avg recall@{TOP_K}={parallel_avg} degraded more than \
             {MAX_PARALLEL_DEGRADATION} from single={single_avg}"
        );
    }

    #[test]
    fn build_usearch_index_parallel_rejects_thread_count_wider_than_rows_gracefully() {
        let mut rng = DeterministicRng::new(3);
        let mut vectors = Vec::with_capacity(4 * DIM);
        for _ in 0..4 {
            vectors.extend(rng.next_vector(DIM));
        }
        // rows(4) < threads(8) でも一部ワーカーの担当範囲が空になるだけで
        // 失敗しないことを確認する（`partition_rows` が空範囲を許すため）。
        let index = build_usearch_index_parallel(4, DIM, &vectors, 8)
            .expect("build with more threads than rows should still succeed");
        assert_eq!(index.size(), 4);
    }
}
