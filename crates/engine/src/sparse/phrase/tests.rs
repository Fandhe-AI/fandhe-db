//! `phrase` モジュールの単体テスト（Issue #2114）。D4 の必須トークン規則の期待値固定、
//! 否定ケース、上限、可視集合の縮約、素朴な参照実装との一致を確かめる。

use super::*;
use std::collections::{BTreeMap, BTreeSet};

fn idx(docs: &[(DocId, &str)]) -> SparseIndex {
    SparseIndex::build(docs).unwrap()
}

fn pat(s: &str) -> PhrasePattern {
    PhrasePattern::compile(s).unwrap()
}

fn entries(s: &str) -> Vec<(u32, String)> {
    pat(s).entries().to_vec()
}

fn e(items: &[(u32, &str)]) -> Vec<(u32, String)> {
    items.iter().map(|&(o, t)| (o, t.to_owned())).collect()
}

fn ids(index: &SparseIndex, q: &str) -> Vec<(DocId, u32)> {
    index
        .phrase_matches(&pat(q))
        .iter()
        .map(|h| (h.doc_id(), h.count()))
        .collect()
}

#[test]
fn pattern_required_tokens_follow_d4() {
    assert_eq!(entries("tokyo tower"), e(&[(0, "tokyo"), (1, "tower")]));
    assert_eq!(entries("東京都"), e(&[(0, "東京"), (1, "京都")]));
    assert_eq!(entries("東"), e(&[(0, "東")]));
    assert_eq!(
        entries("東京の店"),
        e(&[(0, "東京"), (1, "京の"), (2, "の店")])
    );
    assert_eq!(entries("cafe の 東京"), e(&[(0, "cafe"), (2, "東京")]));
    assert_eq!(entries("東京 都"), e(&[(0, "東京"), (2, "都")]));
    assert_eq!(entries("cafe 東京"), e(&[(0, "cafe"), (1, "東京")]));
}

#[test]
fn pattern_drops_leading_and_trailing_holes() {
    assert_eq!(entries("の cafe の"), e(&[(0, "cafe")]));
    assert!(pat("の").is_empty());
    assert!(pat("").is_empty());
}

#[test]
fn matches_adjacent_terms_only() {
    let i = idx(&[
        (1, "tokyo tower"),
        (2, "tokyo big tower"),
        (3, "tower tokyo"),
        (4, "the Tokyo, Tower!"),
    ]);
    assert_eq!(ids(&i, "tokyo tower"), vec![(1, 1), (4, 1)]);
}

#[test]
fn matches_cjk_phrases_and_known_limits() {
    let i = idx(&[
        (1, "東京都"),
        (2, "東京・都"),
        (3, "京都 東京"),
        (4, "東京の店"),
    ]);
    assert_eq!(ids(&i, "東京都"), vec![(1, 1)]);
    // 位置 2 の `都` ユニグラムが存在するため、D4 の規則では `東京 都` は `東京都` と `東京・都` に一致する（区切りは位置を消費しない）。
    assert_eq!(ids(&i, "東京 都"), vec![(1, 1), (2, 1)]);
    assert_eq!(ids(&i, "東京の店"), vec![(4, 1)]);
    assert_eq!(ids(&i, "東"), vec![(1, 1), (2, 1), (3, 1), (4, 1)]);
}

#[test]
fn hole_matches_any_single_unit() {
    let i = idx(&[(1, "cafe x 東京"), (2, "cafe 東京"), (3, "cafe の 東京")]);
    assert_eq!(ids(&i, "cafe の 東京"), vec![(1, 1), (3, 1)]);
}

#[test]
fn counts_overlapping_and_multiple_matches() {
    let i = idx(&[(1, "a a a"), (2, "a b x a b")]);
    assert_eq!(ids(&i, "a a"), vec![(1, 2)]);
    assert_eq!(ids(&i, "a b"), vec![(2, 2)]);
}

#[test]
fn unknown_term_or_empty_pattern_yields_nothing() {
    let i = idx(&[(1, "tokyo tower")]);
    assert!(ids(&i, "tokyo nothere").is_empty());
    assert!(ids(&i, "の").is_empty());
}

#[test]
fn pattern_length_limit_is_enforced() {
    let ok = vec!["a"; MAX_PHRASE_REQUIRED_TOKENS].join(" ");
    assert_eq!(pat(&ok).len(), MAX_PHRASE_REQUIRED_TOKENS);
    let over = vec!["a"; MAX_PHRASE_REQUIRED_TOKENS + 1].join(" ");
    assert_eq!(
        PhrasePattern::compile(&over),
        Err(PhraseError::PatternTooLong {
            len: MAX_PHRASE_REQUIRED_TOKENS + 1,
            max: MAX_PHRASE_REQUIRED_TOKENS
        })
    );
}

#[test]
fn query_bounds_error_does_not_echo_content() {
    let big = "secretword ".repeat(5000);
    let err = PhrasePattern::compile(&big).unwrap_err();
    assert!(matches!(err, PhraseError::QueryTooLong { .. }));
    assert!(!err.to_string().contains("secretword"));
}

#[test]
fn within_restricts_to_visible_documents() {
    let i = idx(&[(1, "tokyo tower"), (2, "tokyo tower"), (3, "tokyo tower")]);
    let p = pat("tokyo tower");
    let vis: BTreeSet<DocId> = [2, 99].into_iter().collect();
    let hits = i.phrase_matches_within(&p, &vis);
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].doc_id(), 2);
    assert!(i.phrase_matches_within(&p, &BTreeSet::new()).is_empty());
}

#[test]
fn within_result_is_independent_of_other_tenants_documents() {
    let a = idx(&[(1, "tokyo tower"), (2, "tokyo x tower")]);
    let b = idx(&[
        (1, "tokyo tower"),
        (2, "tokyo x tower"),
        (3, "tokyo tower tokyo tower"),
    ]);
    let p = pat("tokyo tower");
    let vis: BTreeSet<DocId> = [1, 2].into_iter().collect();
    assert_eq!(
        a.phrase_matches_within(&p, &vis),
        b.phrase_matches_within(&p, &vis)
    );
}

#[test]
fn result_is_sorted_by_doc_id_regardless_of_input_order() {
    let fwd = idx(&[(1, "a b"), (5, "a b"), (9, "a b")]);
    let rev = idx(&[(9, "a b"), (5, "a b"), (1, "a b")]);
    let p = pat("a b");
    assert_eq!(fwd.phrase_matches(&p), rev.phrase_matches(&p));
    let got: Vec<DocId> = rev.phrase_matches(&p).iter().map(|h| h.doc_id()).collect();
    assert_eq!(got, vec![1, 5, 9]);
}

/// 素朴な参照実装（位置 → 語の集合の表を作り、全開始位置を総当たり）との一致。
#[test]
fn matches_naive_oracle_on_random_corpora() {
    let vocab = ["a", "b", "c", "東", "京", "都", "の", "店", " ", "-"];
    let mut state: u64 = 0xD1B5_4A32_D192_ED03;
    let mut next = |m: usize| -> usize {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33) as usize % m
    };
    for _ in 0..200 {
        let n_docs = 1 + next(6);
        let texts: Vec<String> = (0..n_docs)
            .map(|_| {
                (0..(1 + next(14)))
                    .map(|_| vocab[next(vocab.len())])
                    .collect()
            })
            .collect();
        let docs: Vec<(DocId, &str)> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| (i as DocId + 1, t.as_str()))
            .collect();
        let index = idx(&docs);
        let q: String = (0..(1 + next(4)))
            .map(|_| vocab[next(vocab.len())])
            .collect();
        let pattern = pat(&q);
        let mut expected: Vec<(DocId, u32)> = Vec::new();
        for (id, text) in &docs {
            let mut table: BTreeMap<u32, BTreeSet<String>> = BTreeMap::new();
            walk_tokens_positioned(text, true, |t, _, pos| {
                table.entry(pos).or_default().insert(t.to_owned());
                true
            });
            let max = table.keys().next_back().copied().unwrap_or(0);
            let mut count = 0u32;
            if !pattern.is_empty() {
                for s in 0..=max {
                    let ok = pattern
                        .entries()
                        .iter()
                        .all(|(o, t)| table.get(&(s + o)).is_some_and(|set| set.contains(t)));
                    if ok {
                        count += 1;
                    }
                }
            }
            if count > 0 {
                expected.push((*id, count));
            }
        }
        assert_eq!(ids(&index, &q), expected, "query={q:?} docs={texts:?}");
    }
}
