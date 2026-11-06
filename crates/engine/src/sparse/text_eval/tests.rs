//! `text_eval` モジュールの単体テスト（Issue #2115・SEARCH-12／RLS-8 ポインタ）。
//! 構文の意味・決定性・後方互換（D8）・`54000`／`22000` の境界・可視集合の縮約を確かめる。

use super::*;

const CORPUS: &[(DocId, &str)] = &[
    (1, "alpha beta gamma"),
    (2, "alpha delta"),
    (3, "beta gamma tokyo tower"),
    (4, "foobar foobaz alpha"),
    (5, "foo food"),
    (6, "東京タワー"),
    (7, "東 京"),
];

fn index() -> SparseIndex {
    SparseIndex::build(CORPUS).unwrap()
}

fn all() -> BTreeSet<DocId> {
    CORPUS.iter().map(|&(id, _)| id).collect()
}

fn run_with(
    index: &SparseIndex,
    q: &str,
    visible: &BTreeSet<DocId>,
    limits: &TextQueryLimits,
) -> Result<Vec<(DocId, f64)>, TextSearchError> {
    let mut s = index.score_text_query_str_within(q, visible, index.params(), limits)?;
    Ok(s.top(1000)
        .into_iter()
        .map(|d| (d.doc_id, d.score))
        .collect())
}

fn run(q: &str) -> Vec<(DocId, f64)> {
    run_with(&index(), q, &all(), &TextQueryLimits::default()).unwrap()
}

fn ids(q: &str) -> Vec<DocId> {
    let mut v: Vec<DocId> = run(q).into_iter().map(|(d, _)| d).collect();
    v.sort();
    v
}

#[test]
fn sparse_text_eval_plain_query_is_bit_identical_to_score_within() {
    let idx = index();
    let vis = all();
    for q in [
        "alpha",
        "alpha beta",
        "beta gamma tokyo",
        "foo food foobar",
        "東京タワー",
        "alpha 東京",
        "unknownterm alpha",
        "",
    ] {
        let mut a = idx.score_within_with_params(q, &vis, idx.params()).unwrap();
        let mut b = idx
            .score_text_query_str_within(q, &vis, idx.params(), &TextQueryLimits::default())
            .unwrap();
        let ta: Vec<(DocId, u64)> = a
            .top(100)
            .into_iter()
            .map(|d| (d.doc_id, d.score.to_bits()))
            .collect();
        let tb: Vec<(DocId, u64)> = b
            .top(100)
            .into_iter()
            .map(|d| (d.doc_id, d.score.to_bits()))
            .collect();
        assert_eq!(ta, tb, "query {q:?}");
    }
}

#[test]
fn sparse_text_eval_exclusion_removes_matching_docs_only() {
    assert_eq!(ids("alpha"), vec![1, 2, 4]);
    assert_eq!(ids("alpha -beta"), vec![2, 4]);
    assert_eq!(ids("-alpha"), Vec::<DocId>::new());
    assert_eq!(ids("-\"alpha beta\""), Vec::<DocId>::new());
    assert_eq!(ids("alpha -\"alpha beta\""), vec![2, 4]);
}

#[test]
fn sparse_text_eval_exclusion_does_not_change_idf() {
    let with = run("alpha -delta");
    let plain = run("alpha");
    for (d, s) in &with {
        let p = plain.iter().find(|(x, _)| x == d).unwrap();
        assert_eq!(s.to_bits(), p.1.to_bits(), "doc {d}");
    }
}

#[test]
fn sparse_text_eval_phrase_requires_adjacent_tokens() {
    assert_eq!(ids("\"alpha beta\""), vec![1]);
    assert_eq!(ids("\"beta alpha\""), Vec::<DocId>::new());
    assert_eq!(ids("\"beta gamma\""), vec![1, 3]);
    assert_eq!(ids("\"unknown phrase\""), Vec::<DocId>::new());
    assert_eq!(ids("\"alpha beta\" or \"tokyo tower\""), vec![1, 3]);
}

#[test]
fn sparse_text_eval_exclusion_of_cjk_uses_bigram_not_unigrams() {
    // doc 7 は「東」「京」を別々に持つが「東京」の連なりは無いので落ちない。
    assert_eq!(ids("東 -東京"), vec![7]);
    assert_eq!(ids("タワー -東京"), Vec::<DocId>::new());
}

#[test]
fn sparse_text_eval_or_is_union_with_exclusions_per_group() {
    assert_eq!(ids("delta or tokyo"), vec![2, 3]);
    // 第 1 グループは alpha かつ -beta、第 2 グループは tokyo。
    assert_eq!(ids("alpha -beta or tokyo"), vec![2, 3, 4]);
    assert_eq!(ids("or alpha or"), vec![1, 2, 4]);
    // 除外だけのグループは何も足さない。
    assert_eq!(ids("delta or -alpha"), vec![2]);
}

#[test]
fn sparse_text_eval_or_ranks_same_as_space_separated_for_two_terms() {
    let a = run("beta or delta");
    let b = run("beta delta");
    let a_bits: Vec<(DocId, u64)> = a.iter().map(|&(d, s)| (d, s.to_bits())).collect();
    let b_bits: Vec<(DocId, u64)> = b.iter().map(|&(d, s)| (d, s.to_bits())).collect();
    assert_eq!(a_bits, b_bits);
    let c = run("delta or beta");
    let c_bits: Vec<(DocId, u64)> = c.iter().map(|&(d, s)| (d, s.to_bits())).collect();
    assert_eq!(a_bits, c_bits);
}

#[test]
fn sparse_text_eval_prefix_equals_sum_over_expanded_terms() {
    let a = run("foo*");
    let b = run("foo foobar foobaz food");
    let a_bits: Vec<(DocId, u64)> = a.iter().map(|&(d, s)| (d, s.to_bits())).collect();
    let b_bits: Vec<(DocId, u64)> = b.iter().map(|&(d, s)| (d, s.to_bits())).collect();
    assert_eq!(a_bits, b_bits);
    assert_eq!(ids("alpha -foo*"), vec![1, 2]);
    assert_eq!(ids("foob*"), vec![4]);
    assert_eq!(ids("zzz*"), Vec::<DocId>::new());
    // 重なる語幹でも結果は同じ。
    assert_eq!(ids("foo* foob*"), vec![4, 5]);
}

#[test]
fn sparse_text_eval_cjk_star_is_a_plain_term_and_not_counted_as_expansion() {
    let limits = TextQueryLimits::new(1024, 1);
    // 展開上限 1 でも拒否されない（展開数に数えない）。ユニグラムの 東・京 を持つ doc 7 も一致する。
    let mut r: Vec<DocId> = run_with(&index(), "東京*", &all(), &limits)
        .unwrap()
        .into_iter()
        .map(|x| x.0)
        .collect();
    r.sort();
    assert_eq!(r, vec![6, 7]);
}

#[test]
fn sparse_text_eval_ranking_is_deterministic_and_ties_break_by_doc_id() {
    let docs: Vec<(DocId, &str)> = vec![(9, "same"), (3, "same"), (7, "same"), (1, "same")];
    let idx = SparseIndex::build(&docs).unwrap();
    let vis: BTreeSet<DocId> = docs.iter().map(|d| d.0).collect();
    let l = TextQueryLimits::default();
    let mut s = idx
        .score_text_query_str_within("same or same", &vis, idx.params(), &l)
        .unwrap();
    let got: Vec<DocId> = s.top(10).into_iter().map(|d| d.doc_id).collect();
    assert_eq!(got, vec![1, 3, 7, 9]);

    let first = run("alpha or beta* or foo* -delta");
    for _ in 0..5 {
        let again = run("alpha or beta* or foo* -delta");
        assert_eq!(
            first
                .iter()
                .map(|&(d, s)| (d, s.to_bits()))
                .collect::<Vec<_>>(),
            again
                .iter()
                .map(|&(d, s)| (d, s.to_bits()))
                .collect::<Vec<_>>()
        );
    }
}

#[test]
fn sparse_text_eval_top_k_is_prefix_consistent() {
    let idx = index();
    let l = TextQueryLimits::default();
    let mut s = idx
        .score_text_query_str_within("alpha or beta or gamma*", &all(), idx.params(), &l)
        .unwrap();
    let small = s.top(2);
    let big = s.top(5);
    assert_eq!(small.as_slice(), &big[..small.len()]);
}

#[test]
fn sparse_text_eval_result_is_independent_of_corpus_insertion_order() {
    let mut rev: Vec<(DocId, &str)> = CORPUS.to_vec();
    rev.reverse();
    let a = SparseIndex::build(CORPUS).unwrap();
    let b = SparseIndex::build(&rev).unwrap();
    let l = TextQueryLimits::default();
    for q in [
        "alpha -beta or foo*",
        "beta or gamma or \"tokyo tower\"",
        "f* -food",
    ] {
        let ra = run_with(&a, q, &all(), &l).unwrap();
        let rb = run_with(&b, q, &all(), &l).unwrap();
        assert_eq!(
            ra.iter()
                .map(|&(d, s)| (d, s.to_bits()))
                .collect::<Vec<_>>(),
            rb.iter()
                .map(|&(d, s)| (d, s.to_bits()))
                .collect::<Vec<_>>(),
            "{q}"
        );
    }
}

#[test]
fn sparse_text_eval_prefix_expansion_limit_boundary_is_54000() {
    let idx = index();
    // foo* の可視展開は foo・foobar・foobaz・food の 4 語。
    let ok = run_with(&idx, "foo*", &all(), &TextQueryLimits::new(1024, 4));
    assert!(ok.is_ok());
    let err = run_with(&idx, "foo*", &all(), &TextQueryLimits::new(1024, 3)).unwrap_err();
    assert_eq!(err, TextSearchError::TooManyPrefixExpansions { max: 3 });
    assert_eq!(err.wire_code(), "54000");
    // 除外の前方一致も数える。
    let err = run_with(&idx, "alpha -foo*", &all(), &TextQueryLimits::new(1024, 3)).unwrap_err();
    assert_eq!(err.wire_code(), "54000");
}

#[test]
fn sparse_text_eval_prefix_expansions_are_counted_per_group() {
    let idx = index();
    // 同じ語幹を 2 グループで使うと 4 + 4 = 8 と数える。
    assert!(run_with(&idx, "foo* or foo*", &all(), &TextQueryLimits::new(1024, 8)).is_ok());
    assert!(run_with(&idx, "foo* or foo*", &all(), &TextQueryLimits::new(1024, 7)).is_err());
}

#[test]
fn sparse_text_eval_invisible_terms_are_not_counted_in_expansion_limit() {
    let idx = index();
    // 可視が {1, 2, 5} のとき foo* の展開は foo・food の 2 語（doc 4 の語は数えない）。
    let vis: BTreeSet<DocId> = [1, 2, 5].into_iter().collect();
    assert!(run_with(&idx, "foo*", &vis, &TextQueryLimits::new(1024, 2)).is_ok());
    // 可視が {1, 2} なら展開は 0 語で、上限 1 でも受理される。
    let vis: BTreeSet<DocId> = [1, 2].into_iter().collect();
    assert!(run_with(&idx, "foo*", &vis, &TextQueryLimits::new(1024, 1)).is_ok());
}

#[test]
fn sparse_text_eval_term_limit_counts_per_group_and_is_54000() {
    let idx = index();
    let q = "alpha or alpha or alpha";
    assert!(run_with(&idx, q, &all(), &TextQueryLimits::new(3, 8)).is_ok());
    let err = run_with(&idx, q, &all(), &TextQueryLimits::new(2, 8)).unwrap_err();
    assert_eq!(err, TextSearchError::TooManyTerms { count: 3, max: 2 });
    assert_eq!(err.wire_code(), "54000");
    // 項数の検査は visible_ids が空でも行う。
    let empty = BTreeSet::new();
    assert!(run_with(&idx, q, &empty, &TextQueryLimits::new(2, 8)).is_err());
}

#[test]
fn sparse_text_eval_phrase_longer_than_limit_is_54000() {
    let words: Vec<String> = (0..65).map(|i| format!("w{i}")).collect();
    let q = format!("\"{}\"", words.join(" "));
    let err = run_with(&index(), &q, &all(), &TextQueryLimits::default()).unwrap_err();
    assert!(matches!(
        err,
        TextSearchError::PhraseTooLong { len: 65, max: 64 }
    ));
    assert_eq!(err.wire_code(), "54000");
    let q64 = format!("\"{}\"", words[..64].join(" "));
    assert!(run_with(&index(), &q64, &all(), &TextQueryLimits::default()).is_ok());
}

#[test]
fn sparse_text_eval_existing_input_limits_stay_22000_and_take_precedence() {
    let long = "a ".repeat(9000);
    let err = run_with(&index(), &long, &all(), &TextQueryLimits::new(1, 1)).unwrap_err();
    assert!(matches!(err, TextSearchError::QueryTooLong { .. }));
    assert_eq!(err.wire_code(), "22000");
    let many: String = (0..1025).map(|i| format!("t{i} ")).collect();
    let err = run_with(&index(), &many, &all(), &TextQueryLimits::new(1, 1)).unwrap_err();
    assert!(matches!(err, TextSearchError::TooManyQueryTerms { .. }));
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn sparse_text_eval_plain_query_at_max_unique_terms_is_accepted_by_default() {
    let many: String = (0..1024).map(|i| format!("t{i} ")).collect();
    assert!(run_with(&index(), &many, &all(), &TextQueryLimits::default()).is_ok());
}

#[test]
fn sparse_text_eval_invalid_params_are_rejected() {
    let idx = index();
    let bad = Bm25Params {
        k1: f64::NAN,
        b: 0.5,
    };
    let err = idx
        .score_text_query_str_within("alpha", &all(), bad, &TextQueryLimits::default())
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn sparse_text_eval_invisible_docs_do_not_change_scores_or_ranking() {
    let vis: BTreeSet<DocId> = [1, 2, 3].into_iter().collect();
    let full = index();
    let subset: Vec<(DocId, &str)> = CORPUS
        .iter()
        .copied()
        .filter(|d| vis.contains(&d.0))
        .collect();
    let part = SparseIndex::build(&subset).unwrap();
    let l = TextQueryLimits::default();
    for q in [
        "alpha -delta or gamma*",
        "beta gamma",
        "\"beta gamma\" -tokyo",
        "foo* or alpha",
    ] {
        let a = run_with(&full, q, &vis, &l).unwrap();
        let b = run_with(&part, q, &vis, &l).unwrap();
        assert_eq!(
            a.iter().map(|&(d, s)| (d, s.to_bits())).collect::<Vec<_>>(),
            b.iter().map(|&(d, s)| (d, s.to_bits())).collect::<Vec<_>>(),
            "{q}"
        );
    }
}

#[test]
fn sparse_text_eval_error_display_does_not_leak_query_terms() {
    let e = run_with(
        &index(),
        "secretword* or secretword*",
        &all(),
        &TextQueryLimits::new(1024, 1),
    );
    // 展開が 0 語なので成功する。語を含むエラーは件数・上限のみ。
    assert!(e.is_ok());
    let e = run_with(
        &index(),
        "foo* secretword",
        &all(),
        &TextQueryLimits::new(1, 1024),
    )
    .unwrap_err();
    let s = e.to_string();
    assert!(!s.contains("secretword") && !s.contains("foo"), "{s}");
}

#[test]
fn sparse_text_eval_random_inputs_never_panic() {
    let idx = index();
    let alphabet: Vec<char> = "ab foo*-\"or OR東京の 京\t*-\"".chars().collect();
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    let l = TextQueryLimits::default();
    for _ in 0..2000 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let len = (state % 24) as usize;
        let mut s = String::new();
        let mut st = state;
        for _ in 0..len {
            st = st
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let i = ((st >> 33) as usize) % alphabet.len();
            if let Some(c) = alphabet.get(i) {
                s.push(*c);
            }
        }
        let r = idx.score_text_query_str_within(&s, &all(), idx.params(), &l);
        assert!(r.is_ok(), "{s:?}");
    }
}

#[test]
fn prefix_vocab_is_sorted_unique_ascii_and_one_to_one_with_terms() {
    let idx = index();
    let v: Vec<&str> = idx.prefix_vocab.iter().map(|(t, _)| &**t).collect();
    let mut sorted = v.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(v, sorted);
    assert!(v.iter().all(|t| t.is_ascii()));
    let ascii_terms = idx
        .terms
        .ids
        .keys()
        .filter(|t| {
            t.chars()
                .next()
                .is_some_and(super::super::is_ascii_word_char)
        })
        .count();
    assert_eq!(v.len(), ascii_terms);
    for (t, tid) in &idx.prefix_vocab {
        assert_eq!(idx.terms.lookup(t), Some(*tid));
    }
    assert!(idx.approx_prefix_vocab_heap_bytes() > 0);
}

#[test]
fn sparse_text_eval_duplicate_filters_are_deduplicated_before_evaluation() {
    // 同一のフレーズ・除外フレーズ・除外前方一致を大量に並べても、一意化により
    // 1 回分と同じ結果になる（走査量・中間結果が重複数に比例しない）。
    let many = |unit: &str| vec![unit; 600].join(" ");
    assert_eq!(run("\"alpha beta\""), run(&many("\"alpha beta\"")));
    assert_eq!(
        run("alpha -\"alpha beta\" -foo*"),
        run(&format!(
            "alpha {} {}",
            many("-\"alpha beta\""),
            many("-foo*")
        ))
    );
}
