//! Issue #2115（対象ビヘイビア: SEARCH-12 のポインタ `docs/spec/04-behavior/search.md`、
//! 可視集合の縮約は RLS-8 のポインタ）。除外語・前方一致・`or` の評価と `54000` 拒否を、
//! 公開 API（`parse_text_query` → `SparseIndex::score_text_query_within`）だけで確かめる。

use std::collections::BTreeSet;

use engine::sparse::text_eval::TextSearchError;
use engine::sparse::text_query::parse_text_query;
use engine::sparse::{DocId, SparseIndex};
use engine::text_query_limits::{TextQueryLimits, MAX_TEXT_QUERY_PREFIX_EXPANSIONS};

const DOCS: &[(DocId, &str)] = &[
    (1, "rust vector search"),
    (2, "rust database"),
    (3, "python vector database"),
    (4, "vectorize vectors vector"),
];

fn run(
    q: &str,
    visible: &[DocId],
    limits: &TextQueryLimits,
) -> Result<Vec<DocId>, TextSearchError> {
    let idx = SparseIndex::build(DOCS).unwrap();
    let vis: BTreeSet<DocId> = visible.iter().copied().collect();
    let parsed = parse_text_query(q).unwrap();
    let mut s = idx.score_text_query_within(&parsed, &vis, idx.params(), limits)?;
    Ok(s.top(100).into_iter().map(|d| d.doc_id).collect())
}

#[test]
fn exclusion_prefix_and_or_are_applied_within_visible_set() {
    let l = TextQueryLimits::default();
    let all = [1, 2, 3, 4];
    let mut got = run("vector -database", &all, &l).unwrap();
    got.sort();
    assert_eq!(got, vec![1, 4]);
    let mut got = run("vector* -rust", &all, &l).unwrap();
    got.sort();
    assert_eq!(got, vec![3, 4]);
    let mut got = run("search or python", &all, &l).unwrap();
    got.sort();
    assert_eq!(got, vec![1, 3]);
    // 不可視の文書は結果に出ない。
    assert_eq!(run("vector*", &[2], &l).unwrap(), Vec::<DocId>::new());
}

#[test]
fn ranking_is_deterministic_across_runs() {
    let l = TextQueryLimits::default();
    let first = run("vector* or rust -python", &[1, 2, 3, 4], &l).unwrap();
    for _ in 0..10 {
        assert_eq!(
            run("vector* or rust -python", &[1, 2, 3, 4], &l).unwrap(),
            first
        );
    }
}

#[test]
fn prefix_expansion_over_limit_is_rejected_with_54000() {
    // vector* の可視展開は vector・vectors・vectorize の 3 語。
    let ok = TextQueryLimits::new(1024, 3);
    assert!(run("vector*", &[1, 2, 3, 4], &ok).is_ok());
    let ng = TextQueryLimits::new(1024, 2);
    let e = run("vector*", &[1, 2, 3, 4], &ng).unwrap_err();
    assert_eq!(e.wire_code(), "54000");
    // 不可視文書の語は数えない（doc 4 を除くと展開は vector のみ）。
    assert!(run("vector*", &[1, 2, 3], &TextQueryLimits::new(1024, 1)).is_ok());
    assert_eq!(MAX_TEXT_QUERY_PREFIX_EXPANSIONS, 1024);
}
