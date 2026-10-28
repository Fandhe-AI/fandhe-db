//! `engine::sparse`（BM25 疎検索）の統合テスト（TASK-102、対象ビヘイビア: SEARCH-1,
//! SEARCH-3。ポインタ: `docs/spec/05-tasks.md` TASK-102）。
//!
//! `sparse` は storage/catalog に依存しない純関数的モジュールのため、他の統合テストの
//! ような DB パスヘルパは不要。公開 API（`SparseIndex::build`/`search`）経由で
//! 「キーワード一致文書が Top-k に入る」「同一入力なら同一順位（決定性）」を検証する。
//! 関連: TASK-104。CJK ストップワード除去（TASK-105・対象ビヘイビア SEARCH-5）固有の
//! 観点は `crates/engine/tests/sparse_stopwords.rs` を参照。

use engine::sparse::SparseIndex;

fn sample_corpus() -> Vec<(u64, &'static str)> {
    vec![
        (1, "vector database search engine for embeddings"),
        (2, "the quick brown fox jumps over the lazy dog"),
        (3, "hybrid search combines sparse and dense retrieval"),
        (4, "東京都のベクトル検索エンジン"),
        (5, "大阪府のカフェ巡りとレストラン紹介"),
    ]
}

#[test]
fn keyword_match_document_is_in_top_k() {
    let corpus = sample_corpus();
    let idx = SparseIndex::build(&corpus).expect("build");

    let results = idx.search("vector search embeddings", 3).expect("search");
    assert!(!results.is_empty());
    // 「vector」「search」「embeddings」を含む doc 1 が最上位に来ることを期待する。
    assert_eq!(results[0].doc_id, 1);
}

#[test]
fn cjk_keyword_match_document_is_in_top_k() {
    let corpus = sample_corpus();
    let idx = SparseIndex::build(&corpus).expect("build");

    let results = idx.search("東京 ベクトル検索", 3).expect("search");
    assert!(!results.is_empty());
    assert_eq!(results[0].doc_id, 4);
}

#[test]
fn sparse_search_is_reproducible_across_repeated_calls() {
    let corpus = sample_corpus();
    let idx = SparseIndex::build(&corpus).expect("build");

    let first = idx.search("hybrid sparse search", 5).expect("search");
    let second = idx.search("hybrid sparse search", 5).expect("search");
    assert_eq!(
        first, second,
        "同一クエリ・同一インデックスは同一順位を返す（再現性）"
    );
}

#[test]
fn no_keyword_overlap_yields_empty_result() {
    let corpus = sample_corpus();
    let idx = SparseIndex::build(&corpus).expect("build");

    // コーパスに存在しない語のみのクエリはスコア > 0 の文書がなく空になる。
    let results = idx.search("zzzznonexistentqueryterm", 5).expect("search");
    assert!(results.is_empty());
}
