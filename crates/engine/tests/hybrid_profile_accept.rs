//! `benches/harness/hybrid_profile.rs`（Issue #356。hybrid_rrf クエリの段別内訳
//! プロファイル切り分け。親 Issue #355・SEARCH-1・SEARCH-3 関連ポインタ）の
//! 回帰テスト。
//!
//! `hybrid_profile_bench.rs` は時間依存のためこのテストからは実行しない
//! （`tests/hybrid_latency_accept.rs` と同様、実測タイマー・env・実ストレージに
//! 依存しない時間非依存の契約のみを `#[path]` で取り込み `cargo test`
//! 〔`make ci` 対象〕で検証する）。
//!
//! `harness/hybrid_profile.rs` 自体に `#[cfg(test)] mod tests` を置かない理由は
//! `tests/hybrid_latency_accept.rs` 冒頭コメントと同一（`harness/*.rs` は
//! `#[path]` 経由で bench クレートと本テストクレートの双方に取り込まれ、bench
//! コンパイル時は `#[test]` 項目が丸ごと除去されるため `use super::*;` が
//! unused import になる）。
//!
//! Issue #387 PR #416 codex-review P2 指摘対応（2 巡目）: `harness::hybrid_profile`
//! モジュール自体は既定 feature でもコンパイルされる（`benches/harness/mod.rs`
//! 参照）。`engine::hybrid::sparse_refetch_observed`（非既定 feature
//! `bench-internals` 限定）に依存する [`sparse_refetch_schedule`] とその import・
//! 依存テストのみを同 feature の背後に個別に置き、コーパス生成・SQL 文組み立て・
//! tokenize 複製・`refuse_under_github_actions` 等の時間非依存テストは
//! `cargo test -p fandhe-db-engine`（feature 無指定）でも実行される（1 巡目の対応では
//! ファイル全体を `#![cfg(...)]` で覆っていたため、これらの既存テストまで既定
//! feature で 0 件になっていた）。

#[allow(dead_code)]
#[path = "../benches/harness/mod.rs"]
mod harness;

use std::collections::BTreeSet;

use harness::hybrid_profile::{
    bodyclone_replica, boundary_tie_decision, bucket_diff, build_actually_succeeds,
    collect_body_strings, dense_refetch_schedule, expected_visible_count, fetch_cap,
    generate_corpus, generate_queries, initial_fetch_k, is_exhaustive, next_fetch_k, parse_rows,
    parse_visible_ratio_denominator, refetch_schedule_matches_observed_calls,
    refuse_under_github_actions, render_baseline_bucket_line, render_cache_stats_delta_line,
    render_dense_refetch_line, render_memory_line, render_sparse_refetch_line,
    render_sparse_refetch_summary_line, render_sql_surface_bucket_line,
    render_sql_surface_round_raw_line, render_sql_surface_summary_line, render_stage_line,
    replica_matches_real, rowcopy_replica, scan_replica, select_visible_ids, slotmap_replica,
    sql_dense_statement, sql_dense_statement_with_projection, sql_hybrid_statement,
    sql_hybrid_statement_with_projection, summarize_sparse_refetch, tokenize_only,
    tokenize_term_doc_freq, tokenize_term_freq, HybridProjection, ProfileError, ProfileSparseIndex,
    RefetchSchedule, TieDecision, MAX_CORPUS_DOCS_GUARD, MAX_FETCH_K_MIRROR, MAX_POOL_DEPTH_MIRROR,
    MAX_VISIBLE_RATIO_DENOMINATOR,
};
use harness::proc_stats::{parse_kb_line, read_vm_hwm_kb, read_vm_rss_kb};
// `sparse_refetch_schedule` は `sparse_refetch_observed`（非既定 feature
// `bench-internals` 限定）に依存するため import も同 feature 限定にする。
#[cfg(feature = "bench-internals")]
use harness::hybrid_profile::sparse_refetch_schedule;

use harness::hybrid_latency::RefetchTrackingProvider;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::hybrid::{hybrid_search, RrfConfig};
use engine::kernel::SearchInput;
use engine::parallel_search::ParallelSearchProvider;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::sparse::SparseIndex;

// --- generate_corpus: 決定性・形状 ---

#[test]
fn generate_corpus_is_deterministic_for_same_seed() {
    let a = generate_corpus(1, 64, 8).expect("corpus ok");
    let b = generate_corpus(1, 64, 8).expect("corpus ok");
    assert_eq!(a.ids, b.ids);
    assert_eq!(a.vectors, b.vectors);
    assert_eq!(a.bodies, b.bodies);
}

#[test]
fn generate_corpus_differs_across_seeds() {
    let a = generate_corpus(1, 64, 8).expect("corpus ok");
    let b = generate_corpus(2, 64, 8).expect("corpus ok");
    assert_ne!(
        a.vectors, b.vectors,
        "異なるシードから同一ベクトル系列が生成された"
    );
}

#[test]
fn generate_corpus_shapes_are_consistent() {
    let corpus = generate_corpus(7, 40, 8).expect("corpus ok");
    assert_eq!(corpus.ids.len(), 40);
    assert_eq!(corpus.bodies.len(), 40);
    assert_eq!(corpus.vectors.len(), 40 * 8);
    assert_eq!(corpus.dim, 8);
    assert!(corpus.bodies.iter().all(|b| !b.is_empty()));
}

#[test]
fn generate_corpus_bodies_are_independent_of_dim() {
    // 疎チャネル（bodies）は密チャネル用 RNG と別系列のため、dim を変えても
    // bodies の内容は変わらない契約（モジュールドキュメント参照）。
    let a = generate_corpus(3, 20, 8).expect("corpus ok");
    let b = generate_corpus(3, 20, 32).expect("corpus ok");
    assert_eq!(a.bodies, b.bodies);
}

#[test]
fn generate_corpus_rejects_num_docs_beyond_guard() {
    let err = generate_corpus(1, MAX_CORPUS_DOCS_GUARD + 1, 8).unwrap_err();
    assert_eq!(err, ProfileError::CorpusTooLarge);
}

#[test]
fn generate_corpus_sparse_docs_round_trips_ids_and_bodies() {
    let corpus = generate_corpus(9, 10, 4).expect("corpus ok");
    let docs = corpus.sparse_docs();
    assert_eq!(docs.len(), 10);
    for (i, (doc_id, text)) in docs.iter().enumerate() {
        assert_eq!(*doc_id, corpus.ids[i]);
        assert_eq!(*text, corpus.bodies[i].as_str());
    }
}

// --- generate_queries ---

#[test]
fn generate_queries_is_deterministic_for_same_seed() {
    let a = generate_queries(3, 5, 8);
    let b = generate_queries(3, 5, 8);
    assert_eq!(a.len(), 5);
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(x.vector, y.vector);
        assert_eq!(x.text, y.text);
    }
}

#[test]
fn generate_queries_independent_of_corpus_generation() {
    let _ = generate_corpus(5, 100, 8).expect("corpus ok");
    let a = generate_queries(5, 3, 8);
    let b = generate_queries(5, 3, 8);
    assert_eq!(a[0].vector, b[0].vector);
}

// --- build_actually_succeeds: 複製ロジックの構造的整合性チェック ---

#[test]
fn build_actually_succeeds_for_generated_corpus() {
    let corpus = generate_corpus(11, 50, 8).expect("corpus ok");
    assert!(build_actually_succeeds(&corpus));
}

// --- tokenize/term_freq/doc_freq 複製の累積性・決定性 ---

#[test]
fn tokenize_stage_functions_are_deterministic() {
    let corpus = generate_corpus(13, 30, 8).expect("corpus ok");
    assert_eq!(tokenize_only(&corpus.bodies), tokenize_only(&corpus.bodies));
    assert_eq!(
        tokenize_term_freq(&corpus.bodies),
        tokenize_term_freq(&corpus.bodies)
    );
    assert_eq!(
        tokenize_term_doc_freq(&corpus.bodies),
        tokenize_term_doc_freq(&corpus.bodies)
    );
}

#[test]
fn tokenize_term_doc_freq_vocab_size_is_bounded_by_total_tokens() {
    // コーパス全体の語彙数（doc_freq.len()）は総トークン数を超えない
    // （各語彙エントリは少なくとも 1 トークンから生じる）。
    let corpus = generate_corpus(17, 40, 8).expect("corpus ok");
    let total_tokens = tokenize_only(&corpus.bodies);
    let vocab_size = tokenize_term_doc_freq(&corpus.bodies);
    assert!(
        vocab_size <= total_tokens,
        "vocab_size ({vocab_size}) exceeded total_tokens ({total_tokens})"
    );
}

#[test]
fn tokenize_term_freq_unique_terms_sum_is_bounded_by_total_tokens() {
    let corpus = generate_corpus(19, 40, 8).expect("corpus ok");
    let total_tokens = tokenize_only(&corpus.bodies);
    let unique_terms_sum = tokenize_term_freq(&corpus.bodies);
    assert!(
        unique_terms_sum <= total_tokens,
        "unique_terms_sum ({unique_terms_sum}) exceeded total_tokens ({total_tokens})"
    );
}

#[test]
fn tokenize_stage_functions_yield_nonzero_for_nonempty_corpus() {
    let corpus = generate_corpus(23, 5, 8).expect("corpus ok");
    assert!(tokenize_only(&corpus.bodies) > 0);
    assert!(tokenize_term_freq(&corpus.bodies) > 0);
    assert!(tokenize_term_doc_freq(&corpus.bodies) > 0);
}

// --- collect_body_strings ---

#[test]
fn collect_body_strings_preserves_ids_and_bodies() {
    let corpus = generate_corpus(29, 12, 4).expect("corpus ok");
    let collected = collect_body_strings(&corpus.ids, &corpus.bodies);
    assert_eq!(collected.len(), 12);
    for (i, (id, body)) in collected.iter().enumerate() {
        assert_eq!(*id, corpus.ids[i]);
        assert_eq!(body, &corpus.bodies[i]);
    }
}

// --- SQL 文字列組み立て: 識別子検証・リテラル埋め込み ---

#[test]
fn sql_hybrid_statement_embeds_vector_and_text() {
    let sql = sql_hybrid_statement(
        "docs",
        "embedding",
        "body",
        &[1.0, 0.0],
        "vector search",
        10,
    )
    .expect("well-formed statement");
    assert!(sql.contains("hybrid_rrf(embedding, '[1,0]', body, 'vector search')"));
    assert!(sql.contains("LIMIT 10"));
}

#[test]
fn sql_dense_statement_embeds_vector_literal() {
    let sql =
        sql_dense_statement("docs", "embedding", &[0.5, -0.5], 5).expect("well-formed statement");
    assert!(sql.contains("embedding <=> '[0.5,-0.5]'"));
    assert!(sql.contains("LIMIT 5"));
}

#[test]
fn sql_hybrid_statement_with_projection_id_selects_id_only() {
    let sql = sql_hybrid_statement_with_projection(
        "docs",
        "embedding",
        "body",
        &[1.0, 0.0],
        "vector search",
        10,
        HybridProjection::Id,
    )
    .expect("well-formed statement");
    assert!(sql.starts_with("SELECT id FROM docs"));
    assert!(sql.contains("hybrid_rrf(embedding, '[1,0]', body, 'vector search')"));
}

#[test]
fn sql_hybrid_statement_with_projection_star_matches_wrapper() {
    let via_projection = sql_hybrid_statement_with_projection(
        "docs",
        "embedding",
        "body",
        &[1.0, 0.0],
        "q",
        10,
        HybridProjection::Star,
    )
    .expect("well-formed statement");
    let via_wrapper = sql_hybrid_statement("docs", "embedding", "body", &[1.0, 0.0], "q", 10)
        .expect("well-formed statement");
    assert_eq!(via_projection, via_wrapper);
}

#[test]
fn sql_dense_statement_with_projection_id_selects_id_only() {
    let sql = sql_dense_statement_with_projection(
        "docs",
        "embedding",
        &[0.5, -0.5],
        5,
        HybridProjection::Id,
    )
    .expect("well-formed statement");
    assert!(sql.starts_with("SELECT id FROM docs"));
}

#[test]
fn sql_dense_statement_with_projection_star_matches_wrapper() {
    let via_projection = sql_dense_statement_with_projection(
        "docs",
        "embedding",
        &[0.5, -0.5],
        5,
        HybridProjection::Star,
    )
    .expect("well-formed statement");
    let via_wrapper =
        sql_dense_statement("docs", "embedding", &[0.5, -0.5], 5).expect("well-formed statement");
    assert_eq!(via_projection, via_wrapper);
}

#[test]
fn bucket_diff_returns_saturating_forward_diff() {
    let from = std::time::Duration::from_micros(100);
    let to = std::time::Duration::from_micros(150);
    assert_eq!(
        bucket_diff(from, to),
        Some(std::time::Duration::from_micros(50))
    );
}

#[test]
fn bucket_diff_returns_none_when_inverted() {
    let from = std::time::Duration::from_micros(150);
    let to = std::time::Duration::from_micros(100);
    assert_eq!(bucket_diff(from, to), None);
}

#[test]
fn render_baseline_bucket_line_formats_present_values() {
    let line =
        render_baseline_bucket_line("sql_surface", Some(1234), Some(12.5), "within_noise_band");
    assert_eq!(
        line,
        "baseline_bucket label=sql_surface diff=1234us ratio_of_b1=12.50% band=within_noise_band"
    );
}

#[test]
fn render_baseline_bucket_line_formats_inverted_diff() {
    let line = render_baseline_bucket_line("dense", None, None, "above_noise_band");
    assert_eq!(
        line,
        "baseline_bucket label=dense diff=n/a(inverted) ratio_of_b1=n/a band=above_noise_band"
    );
}

#[test]
fn sql_hybrid_statement_rejects_invalid_table_identifier() {
    let err = sql_hybrid_statement("docs; DROP TABLE docs", "embedding", "body", &[1.0], "q", 1)
        .unwrap_err();
    assert_eq!(err, ProfileError::InvalidIdentifier("table"));
}

#[test]
fn sql_hybrid_statement_rejects_unsafe_query_text() {
    let err =
        sql_hybrid_statement("docs", "embedding", "body", &[1.0], "a' OR '1'='1", 1).unwrap_err();
    assert_eq!(err, ProfileError::UnsafeQueryText);
}

#[test]
fn generate_queries_text_is_always_sql_safe() {
    // generate_queries が組み立てるテキストは常に sql_hybrid_statement の検証を
    // 通過する契約（`generate_queries` ドキュメント参照）。
    let queries = generate_queries(31, 20, 8);
    for q in &queries {
        sql_hybrid_statement("docs", "embedding", "body", &q.vector, &q.text, 10)
            .unwrap_or_else(|e| panic!("generated query text {:?} was rejected: {e}", q.text));
    }
}

// --- fail-closed（GITHUB_ACTIONS） ---

#[test]
fn refuse_under_github_actions_rejects_when_true() {
    let err = refuse_under_github_actions(true).unwrap_err();
    assert_eq!(err, ProfileError::RefusedUnderGitHubActions);
}

#[test]
fn refuse_under_github_actions_allows_when_false() {
    assert!(refuse_under_github_actions(false).is_ok());
}

// --- render_stage_line: 実測値を必ず含む ---

#[test]
fn render_stage_line_includes_measured_values() {
    let line = render_stage_line("sql_hybrid", 1200, 1800, 25000);
    assert!(line.contains("stage=sql_hybrid"));
    assert!(line.contains("p95_us=1800"));
    assert!(line.contains("median_us=1200"));
    assert!(line.contains("check_value=25000"));
}

// --- Issue #387: ProfileSparseIndex 複製の忠実性 ---------------------------------

fn visible_set(corpus: &harness::hybrid_profile::ProfileCorpus) -> BTreeSet<u64> {
    corpus.ids.iter().copied().collect()
}

#[test]
fn profile_sparse_index_replica_matches_real_search_within() {
    let corpus = generate_corpus(41, 64, 8).expect("corpus ok");
    let real = SparseIndex::build(&corpus.sparse_docs()).expect("real index ok");
    let replica = ProfileSparseIndex::build(&corpus.sparse_docs()).expect("replica index ok");
    let full_visible = visible_set(&corpus);
    let even_visible: BTreeSet<u64> = full_visible
        .iter()
        .copied()
        .filter(|id| id % 2 == 0)
        .collect();
    let empty_visible: BTreeSet<u64> = BTreeSet::new();

    let queries = generate_queries(41, 4, 8);
    for q in &queries {
        for k in [1usize, 5, 64, 400] {
            for visible in [&full_visible, &even_visible, &empty_visible] {
                replica_matches_real(&real, &replica, &q.text, k, visible)
                    .unwrap_or_else(|e| panic!("replica mismatch: {e}"));
            }
        }
    }
}

#[test]
fn profile_sparse_index_replica_matches_real_search_within_full_index_stride_visible() {
    // Issue #547: 直接 API 段が使う「全件索引＋可視部分集合」の形（索引は
    // corpus 全件・可視集合は `select_visible_ids` の decimation 規則）で
    // 複製実装が実 API と数値一致することを固定する。`even_visible`
    // （2 で割った剰余）とは別に、Issue #547 が導入した可視率 1/10 の
    // 規則そのもので回帰を張る。
    let corpus = generate_corpus(43, 128, 8).expect("corpus ok");
    let real = SparseIndex::build(&corpus.sparse_docs()).expect("real index ok");
    let replica = ProfileSparseIndex::build(&corpus.sparse_docs()).expect("replica index ok");
    let stride_visible = select_visible_ids(corpus.ids.len(), 10);
    assert_eq!(
        stride_visible.len(),
        expected_visible_count(128, 10).unwrap()
    );

    let queries = generate_queries(43, 3, 8);
    for q in &queries {
        for k in [1usize, 5, 13] {
            replica_matches_real(&real, &replica, &q.text, k, &stride_visible)
                .unwrap_or_else(|e| panic!("replica mismatch (stride visible): {e}"));
        }
    }
}

#[test]
fn profile_sparse_index_rejects_duplicate_doc_id() {
    let docs: Vec<(u64, &str)> = vec![(1, "vector search"), (1, "dense sparse")];
    let err = ProfileSparseIndex::build(&docs).unwrap_err();
    assert_eq!(err, ProfileError::DuplicateDocId);
}

#[test]
fn subset_only_and_subset_df_are_consistent_with_replica() {
    let corpus = generate_corpus(43, 30, 8).expect("corpus ok");
    let replica = ProfileSparseIndex::build(&corpus.sparse_docs()).expect("replica index ok");
    let visible = visible_set(&corpus);
    let queries = generate_queries(43, 3, 8);
    for q in &queries {
        let subset_len = replica.subset_only(&q.text, &visible);
        assert!(subset_len <= visible.len());
        let unique_term_count: BTreeSet<String> =
            engine::sparse::tokenize(&q.text).into_iter().collect();
        let unique_term_count = unique_term_count.len();
        let total_df = replica.subset_df(&q.text, &visible);
        assert!(total_df <= subset_len * unique_term_count.max(1));
    }
}

// --- boundary_tie_decision: hybrid.rs の複製の判定分岐 ---------------------------

#[test]
fn boundary_tie_decision_no_boundary_when_scores_shorter_than_pool_depth() {
    let scores = [3.0, 2.0];
    assert_eq!(
        boundary_tie_decision(&scores, 5, false, true),
        TieDecision::Resolved
    );
}

#[test]
fn boundary_tie_decision_no_tie_at_boundary_is_resolved() {
    let scores = [5.0, 4.0, 3.0, 2.0];
    assert_eq!(
        boundary_tie_decision(&scores, 2, false, false),
        TieDecision::Resolved
    );
}

#[test]
fn boundary_tie_decision_tie_group_ends_within_observed_range_is_resolved() {
    let scores = [5.0, 4.0, 4.0, 3.0];
    assert_eq!(
        boundary_tie_decision(&scores, 2, false, false),
        TieDecision::Resolved
    );
}

#[test]
fn boundary_tie_decision_tie_to_tail_non_exhaustive_is_undetermined() {
    let scores = [5.0, 4.0, 4.0, 4.0];
    assert_eq!(
        boundary_tie_decision(&scores, 2, false, false),
        TieDecision::Undetermined
    );
}

#[test]
fn boundary_tie_decision_tie_to_tail_exhaustive_is_resolved() {
    let scores = [5.0, 4.0, 4.0, 4.0];
    assert_eq!(
        boundary_tie_decision(&scores, 2, true, false),
        TieDecision::Resolved
    );
}

#[test]
fn boundary_tie_decision_len_equals_pool_depth_non_exhaustive_is_undetermined() {
    let scores = [5.0, 4.0];
    assert_eq!(
        boundary_tie_decision(&scores, 2, false, false),
        TieDecision::Undetermined
    );
}

#[test]
fn boundary_tie_decision_zero_boundary_with_no_signal_is_resolved() {
    // 疎チャネル（zero_is_no_signal=true）で境界スコアが非正なら、末尾まで
    // 同点が続いていても即座に Resolved（`hybrid.rs::resolve_boundary_tie_group`
    // の無シグナル群早期確定と同じ挙動）。
    let scores = [5.0, 0.0, 0.0, 0.0];
    assert_eq!(
        boundary_tie_decision(&scores, 2, false, true),
        TieDecision::Resolved
    );
}

// --- fetch_k スケジュールヘルパ ---------------------------------------------------

#[test]
fn fetch_k_schedule_helpers_behave_as_expected() {
    assert_eq!(initial_fetch_k(200, 25_000), 400);
    assert_eq!(initial_fetch_k(200, 100), 100);
    assert_eq!(next_fetch_k(400, 25_000), Some(800));
    assert_eq!(next_fetch_k(25_000, 25_000), None);
    assert_eq!(fetch_cap(25_000), 25_000.min(MAX_FETCH_K_MIRROR));
    assert!(is_exhaustive(100, 50, 1_000));
    assert!(is_exhaustive(1_000, 100, 1_000));
    assert!(!is_exhaustive(100, 100, 1_000));
}

// --- sparse_refetch_schedule / dense_refetch_schedule ---------------------------
//
// `sparse_refetch_schedule` は `sparse_refetch_observed`（非既定 feature
// `bench-internals` 限定）経由のため、以下 2 件のみ同 feature 限定。

#[cfg(feature = "bench-internals")]
#[test]
fn sparse_refetch_schedule_stops_at_first_resolved_for_non_tied_corpus() {
    // 文書ごとに固有の語彙を割り当て、同点が実質発生しないコーパスでは
    // 初回 fetch_k で終端確定できる（再取得ラウンド数 1）はず。
    let docs: Vec<(u64, String)> = (0..64u64)
        .map(|i| (i, format!("uniqueterm{i} filler filler filler")))
        .collect();
    let doc_refs: Vec<(u64, &str)> = docs.iter().map(|(id, t)| (*id, t.as_str())).collect();
    let index = SparseIndex::build(&doc_refs).expect("index ok");
    let visible: BTreeSet<u64> = (0..64u64).collect();
    let schedule = sparse_refetch_schedule(&index, "uniqueterm0", &visible, 8)
        .expect("schedule reproduction ok");
    assert_eq!(schedule.fetch_ks.len(), 1);
}

#[cfg(feature = "bench-internals")]
#[test]
fn sparse_refetch_schedule_reaches_cap_on_all_tied_corpus() {
    // 全文書が同一語彙・同一文書長を持つ同点誘発コーパス。可視集合 64 件・
    // pool_depth 8 なら 16→32→64 と倍増して cap（可視集合サイズ）まで到達する。
    let docs: Vec<(u64, String)> = (0..64u64)
        .map(|_| (0u64, "vector search dense sparse".to_string()))
        .enumerate()
        .map(|(i, (_, t))| (i as u64, t))
        .collect();
    let doc_refs: Vec<(u64, &str)> = docs.iter().map(|(id, t)| (*id, t.as_str())).collect();
    let index = SparseIndex::build(&doc_refs).expect("index ok");
    let visible: BTreeSet<u64> = (0..64u64).collect();
    let schedule = sparse_refetch_schedule(&index, "vector search", &visible, 8)
        .expect("schedule reproduction ok");
    assert!(schedule.reached_cap);
    assert_eq!(*schedule.fetch_ks.last().expect("nonempty schedule"), 64);
}

#[test]
fn dense_refetch_schedule_matches_real_hybrid_search_calls_normal_corpus() {
    let corpus = harness::hybrid_latency::generate_corpus(53, 300, 20, 8, None).expect("corpus ok");
    let sparse_index = corpus.build_sparse_index().expect("sparse index ok");
    let provider = RefetchTrackingProvider::new(ParallelSearchProvider);
    let query = harness::hybrid_latency::generate_query(53, 8, 20);
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 8).expect("valid cfg");

    let predicted = dense_refetch_schedule(
        &provider,
        &corpus.ids,
        &corpus.vectors,
        corpus.dim,
        &query.vector,
        8,
    )
    .expect("dense schedule reproduction ok");

    provider.reset();
    let input = SearchInput {
        ids: &corpus.ids,
        vectors: &corpus.vectors,
        dim: corpus.dim,
        query: &query.vector,
        k: 8,
    };
    hybrid_search(&provider, input, &sparse_index, &query.text, 8, &cfg).expect("hybrid_search ok");

    refetch_schedule_matches_observed_calls(0, &predicted, provider.calls())
        .unwrap_or_else(|e| panic!("refetch schedule mismatch: {e}"));
}

#[test]
fn dense_refetch_schedule_matches_real_hybrid_search_calls_tied_corpus() {
    let corpus =
        harness::hybrid_latency::generate_corpus(59, 300, 20, 8, Some(4)).expect("corpus ok");
    let sparse_index = corpus.build_sparse_index().expect("sparse index ok");
    let provider = RefetchTrackingProvider::new(ParallelSearchProvider);
    let query = harness::hybrid_latency::generate_query(59, 8, 20);
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 8).expect("valid cfg");

    let predicted = dense_refetch_schedule(
        &provider,
        &corpus.ids,
        &corpus.vectors,
        corpus.dim,
        &query.vector,
        8,
    )
    .expect("dense schedule reproduction ok");

    provider.reset();
    let input = SearchInput {
        ids: &corpus.ids,
        vectors: &corpus.vectors,
        dim: corpus.dim,
        query: &query.vector,
        k: 8,
    };
    hybrid_search(&provider, input, &sparse_index, &query.text, 8, &cfg).expect("hybrid_search ok");

    refetch_schedule_matches_observed_calls(0, &predicted, provider.calls())
        .unwrap_or_else(|e| panic!("refetch schedule mismatch: {e}"));
}

#[test]
fn dense_refetch_schedule_matches_real_hybrid_search_calls_small_visible_set() {
    // 可視集合が初期 fetch_k（pool_depth*2）より小さいケース: cap がすぐに
    // 可視集合サイズへ張り付き、1 回で終端確定するはず。
    let corpus = harness::hybrid_latency::generate_corpus(61, 20, 20, 8, None).expect("corpus ok");
    let sparse_index = corpus.build_sparse_index().expect("sparse index ok");
    let provider = RefetchTrackingProvider::new(ParallelSearchProvider);
    let query = harness::hybrid_latency::generate_query(61, 8, 20);
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 8).expect("valid cfg");

    let predicted = dense_refetch_schedule(
        &provider,
        &corpus.ids,
        &corpus.vectors,
        corpus.dim,
        &query.vector,
        8,
    )
    .expect("dense schedule reproduction ok");
    assert_eq!(predicted.fetch_ks.len(), 1);

    provider.reset();
    let input = SearchInput {
        ids: &corpus.ids,
        vectors: &corpus.vectors,
        dim: corpus.dim,
        query: &query.vector,
        k: 8,
    };
    hybrid_search(&provider, input, &sparse_index, &query.text, 8, &cfg).expect("hybrid_search ok");

    refetch_schedule_matches_observed_calls(0, &predicted, provider.calls())
        .unwrap_or_else(|e| panic!("refetch schedule mismatch: {e}"));
}

#[test]
fn refetch_schedule_matches_observed_calls_detects_mismatch() {
    let predicted = RefetchSchedule {
        fetch_ks: vec![16, 32],
        final_hits: 5,
        reached_cap: false,
    };
    let err = refetch_schedule_matches_observed_calls(3, &predicted, 1).unwrap_err();
    assert_eq!(
        err,
        ProfileError::RefetchMismatch {
            query: 3,
            predicted: 2,
            observed: 1,
        }
    );
}

// --- 鏡像定数のドリフト検知 ---

#[test]
fn max_pool_depth_mirror_matches_rrf_config_bounds() {
    assert!(RrfConfig::new(60.0, 1.0, 1.0, MAX_POOL_DEPTH_MIRROR).is_ok());
    assert!(RrfConfig::new(60.0, 1.0, 1.0, MAX_POOL_DEPTH_MIRROR + 1).is_err());
}

// --- 描画関数 ---

#[test]
fn render_sparse_refetch_line_includes_schedule() {
    let schedule = RefetchSchedule {
        fetch_ks: vec![400, 800, 1600],
        final_hits: 200,
        reached_cap: false,
    };
    let line = render_sparse_refetch_line(2, &schedule);
    assert!(line.contains("query=2"));
    assert!(line.contains("calls=3"));
    assert!(line.contains("fetch_ks=400,800,1600"));
    assert!(line.contains("final_hits=200"));
    assert!(line.contains("reached_cap=false"));
}

#[test]
fn render_sparse_refetch_summary_line_includes_measured_values() {
    let schedules = vec![
        RefetchSchedule {
            fetch_ks: vec![400],
            final_hits: 10,
            reached_cap: false,
        },
        RefetchSchedule {
            fetch_ks: vec![400, 800],
            final_hits: 20,
            reached_cap: true,
        },
    ];
    let summary = summarize_sparse_refetch(&schedules);
    assert_eq!(summary.queries, 2);
    assert_eq!(summary.calls_max, 2);
    assert_eq!(summary.calls_total, 3);
    assert_eq!(summary.reached_cap_count, 1);
    assert_eq!(summary.max_fetch_k, 800);
    let line = render_sparse_refetch_summary_line(&summary, 1234);
    assert!(line.contains("queries=2"));
    assert!(line.contains("calls_max=2"));
    assert!(line.contains("calls_total=3"));
    assert!(line.contains("reached_cap_count=1"));
    assert!(line.contains("max_fetch_k=800"));
    assert!(line.contains("estimated_cumulative_mixed_median_us=1234"));
}

#[test]
fn render_dense_refetch_line_includes_measured_values() {
    let summary = harness::hybrid_latency::aggregate_refetch_stats(3, 800, 1000);
    let batch = harness::hybrid_latency::summarize_refetch_stats(&[summary]);
    let line = render_dense_refetch_line("hybrid_search_cached_index", 500, 900, &batch);
    assert!(line.contains("stage=hybrid_search_cached_index"));
    assert!(line.contains("p95_us=900"));
    assert!(line.contains("median_us=500"));
    assert!(line.contains("provider_calls_max=3"));
}

// --- ProfileError の Display ---

#[test]
fn profile_error_display_is_nonempty_for_new_variants() {
    let variants = [
        ProfileError::DuplicateDocId,
        ProfileError::ReplicaMismatch {
            position: 1,
            detail: "x".to_string(),
        },
        ProfileError::RefetchMismatch {
            query: 0,
            predicted: 1,
            observed: 2,
        },
        ProfileError::ContractViolation("boom".to_string()),
        ProfileError::InvalidRows("must be >= 1 (got 0)".to_string()),
        ProfileError::InvalidVisibleRatio("must be in \"1/<N>\" form".to_string()),
    ];
    for v in variants {
        assert!(!v.to_string().is_empty());
    }
}

// --- Issue #389: harness::proc_stats・render_memory_line（時間非依存の判定ロジック） ---

#[test]
fn parse_kb_line_reads_leading_numeric_token() {
    assert_eq!(parse_kb_line("    12345 kB"), Some(12345));
}

#[test]
fn parse_kb_line_rejects_non_numeric_input() {
    assert_eq!(parse_kb_line(""), None);
    assert_eq!(parse_kb_line("not-a-number kB"), None);
}

#[test]
fn read_vm_rss_and_hwm_do_not_panic_regardless_of_environment() {
    // `/proc` が読めない環境（Linux 以外・sandbox 制限）でも `None` を返す
    // だけで panic しないことを固定する（診断目的でベンチを止めない契約）。
    // 値の有無自体は環境依存のためここでは固定しない。
    let _ = read_vm_rss_kb();
    let _ = read_vm_hwm_kb();
}

#[test]
fn render_memory_line_includes_all_measured_values() {
    let line = render_memory_line(12345, Some(1000), Some(1200), Some(1500));
    assert!(line.contains("stage=sparse_index_resident"));
    assert!(line.contains("approx_heap_bytes=12345"));
    assert!(line.contains("vm_rss_kb_before=1000"));
    assert!(line.contains("vm_rss_kb_after=1200"));
    assert!(line.contains("rss_delta_kb=200"));
    assert!(line.contains("vm_hwm_kb=1500"));
}

#[test]
fn render_memory_line_reports_unavailable_when_rss_unreadable() {
    let line = render_memory_line(999, None, None, None);
    assert!(line.contains("vm_rss_kb_before=unavailable"));
    assert!(line.contains("vm_rss_kb_after=unavailable"));
    assert!(line.contains("rss_delta_kb=unavailable"));
    assert!(line.contains("vm_hwm_kb=unavailable"));
}

// =============================================================================
// Issue #547: 行数・可視率 opt-in（`BENCH_HYBRID_PROFILE_ROWS`／
// `BENCH_HYBRID_PROFILE_VISIBLE_RATIO`）のパーサ・選択関数
// =============================================================================

#[test]
fn parse_rows_defaults_to_25000_when_unset_or_empty() {
    assert_eq!(parse_rows(None), Ok(25_000));
    assert_eq!(parse_rows(Some("")), Ok(25_000));
    assert_eq!(parse_rows(Some("  ")), Ok(25_000));
}

#[test]
fn parse_rows_accepts_boundary_values() {
    assert_eq!(parse_rows(Some("1")), Ok(1));
    assert_eq!(
        parse_rows(Some("100000")),
        Ok(MAX_CORPUS_DOCS_GUARD),
        "100,000 must be accepted: sparse::MAX_CORPUS_DOCS and \
         hybrid_profile::MAX_CORPUS_DOCS_GUARD both use a strict > comparison"
    );
}

#[test]
fn parse_rows_rejects_zero_over_limit_and_non_numeric() {
    assert!(matches!(
        parse_rows(Some("0")),
        Err(ProfileError::InvalidRows(_))
    ));
    assert!(matches!(
        parse_rows(Some("100001")),
        Err(ProfileError::InvalidRows(_))
    ));
    assert!(matches!(
        parse_rows(Some("not-a-number")),
        Err(ProfileError::InvalidRows(_))
    ));
    assert!(matches!(
        parse_rows(Some("-1")),
        Err(ProfileError::InvalidRows(_))
    ));
}

#[test]
fn parse_visible_ratio_denominator_defaults_to_1_when_unset_or_empty() {
    assert_eq!(parse_visible_ratio_denominator(None), Ok(1));
    assert_eq!(parse_visible_ratio_denominator(Some("")), Ok(1));
}

#[test]
fn parse_visible_ratio_denominator_accepts_1_over_n_form() {
    assert_eq!(parse_visible_ratio_denominator(Some("1/1")), Ok(1));
    assert_eq!(parse_visible_ratio_denominator(Some("1/10")), Ok(10));
    assert_eq!(
        parse_visible_ratio_denominator(Some(" 1/10 ")),
        Ok(10),
        "leading/trailing whitespace must be tolerated (GitHub Actions variable \
         expansion trailing-newline concern, matching recall_engine.rs's policy)"
    );
    assert_eq!(
        parse_visible_ratio_denominator(Some(&format!("1/{MAX_VISIBLE_RATIO_DENOMINATOR}"))),
        Ok(MAX_VISIBLE_RATIO_DENOMINATOR)
    );
}

#[test]
fn parse_visible_ratio_denominator_rejects_non_1_over_n_forms() {
    for bad in [
        "10", "2/10", "1/0", "1/", "/10", "1/1001", "1/abc", "abc/10",
    ] {
        assert!(
            matches!(
                parse_visible_ratio_denominator(Some(bad)),
                Err(ProfileError::InvalidVisibleRatio(_))
            ),
            "expected InvalidVisibleRatio for {bad:?}"
        );
    }
}

#[test]
fn select_visible_ids_is_deterministic_and_matches_expected_count() {
    let a = select_visible_ids(1_000, 10);
    let b = select_visible_ids(1_000, 10);
    assert_eq!(
        a, b,
        "select_visible_ids must be a pure deterministic function"
    );
    assert_eq!(a.len(), expected_visible_count(1_000, 10).unwrap());
    // 決定的規則: doc_id % 10 == 0 のみが可視。
    assert!(a.contains(&0));
    assert!(a.contains(&990));
    assert!(!a.contains(&1));
    assert!(!a.contains(&999));
}

#[test]
fn select_visible_ids_denominator_1_selects_all_rows() {
    let visible = select_visible_ids(2_000, 1);
    assert_eq!(visible.len(), 2_000);
    assert_eq!(
        visible,
        (0..2_000u64).collect::<std::collections::BTreeSet<u64>>()
    );
}

#[test]
fn expected_visible_count_matches_ceil_division() {
    assert_eq!(expected_visible_count(25_000, 1).unwrap(), 25_000);
    assert_eq!(expected_visible_count(2_000, 10).unwrap(), 200);
    // 割り切れない組み合わせは ceil で丸める（select_visible_ids の
    // `doc_id % denominator == 0` 規則は 0 始まりのため常に floor(n/d)+1 の
    // 端数だが、ceil_div と同じ件数になることを固定する）。
    assert_eq!(expected_visible_count(7, 3).unwrap(), 3); // ids 0,3,6
}

#[test]
fn expected_visible_count_never_yields_zero_for_rows_at_least_1() {
    // `select_visible_ids` は 0 始まり連番なので `doc_id=0` が常に可視
    // （`0 % denominator == 0`）となり、`rows>=1`（`parse_rows` が 0 を
    // 拒否するため呼び出し元の前提でもある）なら分母をどれだけ大きく
    // しても可視 0 件には到達しない。`expected_visible_count` の
    // 「0 件は Err」ガードは、この不変条件が崩れた場合の防御として
    // 残るだけであることを固定する。
    assert_eq!(
        expected_visible_count(1, MAX_VISIBLE_RATIO_DENOMINATOR).unwrap(),
        1
    );
}

// =============================================================================
// Issue #660: SQL 表層固定コスト内訳（S0〜S8）ヘルパの回帰テスト
// =============================================================================

fn issue660_schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

#[test]
fn scan_replica_processes_every_encoded_row_and_matches_visible_count() {
    let schema = issue660_schema();
    let bodies = ["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
    let encoded: Vec<Vec<u8>> = bodies
        .iter()
        .map(|b| {
            encode_scalar_columns(&schema, &[Value::Null, Value::Text(b.clone())])
                .expect("encode fixture row")
        })
        .collect();
    assert_eq!(scan_replica(&schema, &encoded), bodies.len());
}

#[test]
#[should_panic(expected = "malformed row")]
fn scan_replica_fails_closed_on_malformed_row() {
    let schema = issue660_schema();
    let bogus = vec![vec![0xffu8; 4]];
    let _ = scan_replica(&schema, &bogus);
}

#[test]
fn rowcopy_replica_processes_every_row() {
    let ids: Vec<u64> = vec![0, 1, 2, 3];
    let dim = 4usize;
    let vectors: Vec<f32> = (0..ids.len() * dim).map(|i| i as f32).collect();
    assert_eq!(rowcopy_replica(&ids, &vectors, dim, "tenant-a"), ids.len());
}

#[test]
fn rowcopy_replica_handles_empty_input() {
    assert_eq!(rowcopy_replica(&[], &[], 4, "tenant-a"), 0);
}

#[test]
fn slotmap_replica_returns_slot_count_matching_ids_len() {
    let ids: Vec<u64> = vec![10, 20, 30, 10]; // 重複あり（同一 id が複数スロットに現れうる契約）
    assert_eq!(slotmap_replica(&ids), ids.len());
}

#[test]
fn bodyclone_replica_processes_every_body() {
    let bodies = vec!["one".to_string(), "two".to_string()];
    assert_eq!(bodyclone_replica(&bodies), bodies.len());
}

#[test]
fn bodyclone_replica_handles_empty_input() {
    let bodies: Vec<String> = Vec::new();
    assert_eq!(bodyclone_replica(&bodies), 0);
}

#[test]
fn render_sql_surface_round_raw_line_formats_all_stages() {
    let line = render_sql_surface_round_raw_line(1, &[("S0", 100), ("S1", 20), ("S2", 5)]);
    assert_eq!(
        line,
        "hybrid_profile: sql_surface_breakdown_raw round=1 S0=100us S1=20us S2=5us"
    );
}

#[test]
fn render_sql_surface_round_raw_line_handles_empty_values() {
    let line = render_sql_surface_round_raw_line(2, &[]);
    assert_eq!(line, "hybrid_profile: sql_surface_breakdown_raw round=2");
}

#[test]
fn render_sql_surface_summary_line_formats_min_median() {
    let line = render_sql_surface_summary_line("S4_scan_replica", 10, 15);
    assert_eq!(
        line,
        "hybrid_profile: sql_surface_breakdown_summary stage=S4_scan_replica min=10us median=15us"
    );
}

#[test]
fn render_sql_surface_bucket_line_formats_present_diff() {
    let line = render_sql_surface_bucket_line("hybrid_only", Some(3100), Some(34.76));
    assert_eq!(
        line,
        "hybrid_profile: sql_surface_breakdown_bucket label=hybrid_only diff=3100us ratio_of_b1=34.76%"
    );
}

#[test]
fn render_sql_surface_bucket_line_formats_inverted_diff() {
    let line = render_sql_surface_bucket_line("unexplained_common", None, None);
    assert_eq!(
        line,
        "hybrid_profile: sql_surface_breakdown_bucket label=unexplained_common diff=n/a(inverted) ratio_of_b1=n/a"
    );
}

#[test]
fn render_cache_stats_delta_line_reports_nonvacuous_hit_growth() {
    let line = render_cache_stats_delta_line("sql_arena_hits", 5, 55, 50, true);
    assert_eq!(
        line,
        "hybrid_profile: cache_stats_delta name=sql_arena_hits before=5 after=55 delta=50 expected_min=50 nonvacuous=true"
    );
}

#[test]
fn render_cache_stats_delta_line_reports_vacuous_when_delta_below_expected() {
    let line = render_cache_stats_delta_line("hnsw_hits", 0, 0, 0, false);
    assert_eq!(
        line,
        "hybrid_profile: cache_stats_delta name=hnsw_hits before=0 after=0 delta=0 expected_min=0 nonvacuous=false"
    );
}

#[test]
fn render_cache_stats_delta_line_delta_saturates_when_counter_decreases() {
    // カウンタは単調増加が既定契約だが、レンダラ自体は `saturating_sub` で
    // 逆転時にパニックしない（呼び出し元の非 vacuous 判定と表示を分離する）。
    let line = render_cache_stats_delta_line("stale_evictions", 10, 3, 0, false);
    assert_eq!(
        line,
        "hybrid_profile: cache_stats_delta name=stale_evictions before=10 after=3 delta=0 expected_min=0 nonvacuous=false"
    );
}

#[test]
fn scan_rowcopy_slotmap_bodyclone_replicas_are_deterministic_across_repeated_calls() {
    let schema = issue660_schema();
    let bodies = vec!["x".to_string(), "y".to_string()];
    let encoded: Vec<Vec<u8>> = bodies
        .iter()
        .map(|b| {
            encode_scalar_columns(&schema, &[Value::Null, Value::Text(b.clone())])
                .expect("encode fixture row")
        })
        .collect();
    let ids: Vec<u64> = vec![0, 1];
    let vectors: Vec<f32> = vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0];
    assert_eq!(
        scan_replica(&schema, &encoded),
        scan_replica(&schema, &encoded)
    );
    assert_eq!(
        rowcopy_replica(&ids, &vectors, 4, "t"),
        rowcopy_replica(&ids, &vectors, 4, "t")
    );
    assert_eq!(slotmap_replica(&ids), slotmap_replica(&ids));
    assert_eq!(bodyclone_replica(&bodies), bodyclone_replica(&bodies));
}
