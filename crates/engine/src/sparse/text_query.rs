//! 全文検索クエリ構文のパーサ（Issue #2113・親 #2111。対象ビヘイビア SEARCH-12 のポインタ:
//! `docs/spec/04-behavior/search.md`。字句規則の根拠は公開済みの
//! `docs/design/fulltext-query-syntax.md` の D1・D3・D4・D10）。
//!
//! 責務境界: 検索語の文字列を構文木 [`TextQuery`] へ変換する純関数 [`parse_text_query`] だけを
//! 提供する。索引・統計・RLS・評価（フレーズ一致・前方一致の展開・グループの和）・`wire_code`
//! への写像は扱わない。呼び出し元は #2114（位置・フレーズ一致）・#2115（評価・統計・上限）・
//! #2116（SQL／NoSQL の結線）で、可視集合に基づく統計や展開数の計数もそちらの責務。
//!
//! 契約: 入口で親モジュールの `validate_query_bounds`（`MAX_QUERY_BYTES`・`MAX_QUERY_TERMS`）を
//! 最初に検証し、超過は切り詰めず `Err`（fail-closed）にする。上限を通過した入力は構文の
//! 不正で `Err` にならず、解釈できない記号は通常の区切りへ縮退する。
//!
//! 線形時間・有界性: 構文に括弧・入れ子はなく、走査は再帰しない 1 パス（後戻りなし）。
//! チャンク・フレーズは入力の互いに重ならない部分区間で、各々を `walk_tokens` へ 1 回だけ渡す。
//! 区切りは非単語文字の位置でしか入力を切らないため、木に現れる語はすべて
//! `tokenize(query)` のトークンの部分集合で、一意語数は `MAX_QUERY_TERMS` 以下、
//! 総バイト数は入力長の定数倍以下になる。

use std::collections::BTreeSet;
use std::ops::Range;

use super::{
    is_ascii_word_char, is_cjk_char, validate_query_bounds, walk_tokens, SparseError,
    MAX_QUERY_BYTES,
};

/// 構文解析済みの検索語。トップレベルの `or` で分けたグループの和（D2）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TextQuery {
    groups: Vec<QueryGroup>,
}

impl TextQuery {
    /// `or` で分けたグループの列（空のグループは含まない）。
    pub fn groups(&self) -> &[QueryGroup] {
        &self.groups
    }

    /// グループが 1 つも無い（トークンが出ない入力）か。
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// 木に現れる一意語数。#2115 が新しい項数の上限を数えるための補助。
    pub fn unique_term_count(&self) -> usize {
        let mut set: BTreeSet<&str> = BTreeSet::new();
        for g in &self.groups {
            for c in &g.clauses {
                match c {
                    Clause::Term(t) | Clause::Prefix(t) | Clause::Exclude(Excluded::Prefix(t)) => {
                        set.insert(t);
                    }
                    Clause::Phrase(p) | Clause::Exclude(Excluded::Phrase(p)) => {
                        set.extend(p.tokens.iter().map(String::as_str));
                    }
                }
            }
        }
        set.len()
    }
}

/// `or` で区切られた 1 グループ。節は入力順を保つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryGroup {
    clauses: Vec<Clause>,
}

impl QueryGroup {
    /// 節の列（入力順・重複あり。一意化は評価側の責務）。
    pub fn clauses(&self) -> &[Clause] {
        &self.clauses
    }

    /// 正の節（Term・Prefix・Phrase）を 1 つでも持つか。偽なら除外だけのグループ（D2）。
    pub fn has_positive(&self) -> bool {
        self.clauses
            .iter()
            .any(|c| !matches!(c, Clause::Exclude(_)))
    }
}

/// グループ内の 1 つの節。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Clause {
    /// 任意語（`tokenize` の 1 トークン。スコアのみに寄与）。
    Term(String),
    /// 任意の前方一致（ASCII 語の語幹。小文字化済み）。
    Prefix(String),
    /// 必須のフレーズ（フィルタ）。
    Phrase(Phrase),
    /// 除外（フィルタ）。
    Exclude(Excluded),
}

/// 除外の対象。1 語の除外は長さ 1 のフレーズで表す。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Excluded {
    /// フレーズ（または複数トークンになる語）の除外。
    Phrase(Phrase),
    /// ASCII 語の前方一致の除外。
    Prefix(String),
}

/// フレーズ。#2114 が位置モデルを元の部分文字列から再構成できるよう両方を保持する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phrase {
    source: String,
    tokens: Vec<String>,
}

impl Phrase {
    /// 演算子の文字を除いた元の部分文字列（小文字化しない）。
    pub fn source(&self) -> &str {
        &self.source
    }

    /// `source` を `tokenize`（ストップワード除去 ON）した順序つきトークン列。空にならない。
    pub fn tokens(&self) -> &[String] {
        &self.tokens
    }

    /// トークンが 1 つも出ないときは `None`（D3: 空のフレーズは捨てる）。
    fn new(source: &str) -> Option<Self> {
        let tokens: Vec<String> = collect_tokens(source).into_iter().map(|(t, _)| t).collect();
        if tokens.is_empty() {
            None
        } else {
            Some(Self {
                source: source.to_owned(),
                tokens,
            })
        }
    }
}

/// [`parse_text_query`] のエラー。内容は長さと上限値のみで、検索語の中身は含めない。
///
/// `wire_code` への写像は #2115／#2116 が担う（既存上限の超過は `22000` のまま）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TextQueryError {
    /// 検索語のバイト長が上限を超える。
    QueryTooLong { len: usize, max: usize },
    /// 検索語の一意語数が上限を超える。
    TooManyQueryTerms { unique_terms: usize, max: usize },
}

impl std::fmt::Display for TextQueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueryTooLong { len, max } => {
                write!(f, "query is too long: {len} bytes (max {max})")
            }
            Self::TooManyQueryTerms { unique_terms, max } => {
                write!(f, "too many query terms: {unique_terms} (max {max})")
            }
        }
    }
}

impl std::error::Error for TextQueryError {}

/// `walk_tokens`（ストップワード除去 ON）の結果を範囲つきで集める。
fn collect_tokens(s: &str) -> Vec<(String, Range<usize>)> {
    let mut out = Vec::new();
    walk_tokens(s, true, |t, r| {
        out.push((t.to_owned(), r));
        true
    });
    out
}

/// `walk_tokens` と同じ基準（小文字展開後）で「語の文字」か。
fn is_word_char(c: char) -> bool {
    c.to_lowercase()
        .any(|l| is_ascii_word_char(l) || is_cjk_char(l))
}

/// トークンが ASCII 語か（CJK トークンは前方一致の対象にしない。D4）。
fn is_ascii_token(t: &str) -> bool {
    t.chars().next().is_some_and(is_ascii_word_char)
}

/// 末尾が「語の文字 + `*`」なら `*` を除いた本体を返す（前方一致の候補）。
fn strip_star(s: &str) -> Option<&str> {
    s.strip_suffix('*')
        .filter(|b| b.chars().next_back().is_some_and(is_word_char))
}

/// 先頭の `"` の次から始まる文字列を受け取り、(フレーズ本体, 消費後の残り) を返す。
/// 閉じていない場合は終端までをフレーズにする。
fn split_phrase(after_quote: &str) -> (&str, &str) {
    match after_quote.find('"') {
        Some(i) => (
            after_quote.get(..i).unwrap_or(""),
            after_quote.get(i + 1..).unwrap_or(""),
        ),
        None => (after_quote, ""),
    }
}

/// 組み立て中のグループ列。
struct Builder {
    groups: Vec<QueryGroup>,
    current: Vec<Clause>,
}

impl Builder {
    fn flush(&mut self) {
        if !self.current.is_empty() {
            self.groups.push(QueryGroup {
                clauses: std::mem::take(&mut self.current),
            });
        }
    }

    /// 通常のチャンク（語末 `*` の前方一致を含む）を節へ変換する。
    fn push_word(&mut self, chunk: &str) {
        let star_body = strip_star(chunk);
        let body = star_body.unwrap_or(chunk);
        let tokens = collect_tokens(body);
        let last_is_prefix = star_body.is_some()
            && tokens
                .last()
                .is_some_and(|(t, r)| r.end == body.len() && is_ascii_token(t));
        let n = tokens.len();
        for (i, (t, _)) in tokens.into_iter().enumerate() {
            if last_is_prefix && i + 1 == n {
                self.current.push(Clause::Prefix(t));
            } else {
                self.current.push(Clause::Term(t));
            }
        }
    }

    fn push_exclude_phrase(&mut self, source: &str) {
        if let Some(p) = Phrase::new(source) {
            self.current.push(Clause::Exclude(Excluded::Phrase(p)));
        }
    }

    /// `-` を除いたチャンクの残りを除外へ変換する。
    fn push_exclude_word(&mut self, rest: &str) {
        let Some(body) = strip_star(rest) else {
            self.push_exclude_phrase(rest);
            return;
        };
        let tokens = collect_tokens(body);
        if let [(t, r)] = tokens.as_slice() {
            if r.end == body.len() && is_ascii_token(t) {
                self.current
                    .push(Clause::Exclude(Excluded::Prefix(t.clone())));
                return;
            }
        }
        // 複数トークン・CJK では `*` を外したフレーズ除外にする（除外が狭い側＝広く返す側）。
        self.push_exclude_phrase(body);
    }
}

/// 検索語文字列を構文木へ変換する純関数。
///
/// 上限（`MAX_QUERY_BYTES`・`MAX_QUERY_TERMS`）の超過だけが `Err`。通過した入力は常に `Ok` で、
/// 解釈できない部分は通常の語（または区切り）に縮退する。引用符フレーズ・`or`・`-` 除外・
/// 語末 `*` の前方一致を解釈する。
pub fn parse_text_query(query: &str) -> Result<TextQuery, TextQueryError> {
    validate_query_bounds(query).map_err(|e| match e {
        SparseError::QueryTooLong { len, max } => TextQueryError::QueryTooLong { len, max },
        SparseError::TooManyQueryTerms { unique_terms, max } => {
            TextQueryError::TooManyQueryTerms { unique_terms, max }
        }
        // validate_query_bounds が返すのは上の 2 種のみ。万一増えても拒否側に倒す。
        _ => TextQueryError::QueryTooLong {
            len: query.len(),
            max: MAX_QUERY_BYTES,
        },
    })?;

    let mut b = Builder {
        groups: Vec::new(),
        current: Vec::new(),
    };
    let mut rest = query;
    // 直前が入力の先頭または空白か（`-` を演算子にできる位置）。
    let mut at_boundary = true;
    while let Some(c) = rest.chars().next() {
        if c.is_whitespace() {
            rest = rest.get(c.len_utf8()..).unwrap_or("");
            at_boundary = true;
            continue;
        }
        if c == '"' {
            let (body, tail) = split_phrase(rest.get(1..).unwrap_or(""));
            if let Some(p) = Phrase::new(body) {
                b.current.push(Clause::Phrase(p));
            }
            rest = tail;
            at_boundary = false;
            continue;
        }
        let end = rest
            .find(|ch: char| ch.is_whitespace() || ch == '"')
            .unwrap_or(rest.len());
        let chunk = rest.get(..end).unwrap_or("");
        rest = rest.get(end..).unwrap_or("");
        let was_boundary = at_boundary;
        at_boundary = false;

        if chunk.eq_ignore_ascii_case("or") {
            b.flush();
            continue;
        }
        if was_boundary {
            if let Some(after_dash) = chunk.strip_prefix('-') {
                if after_dash.is_empty() && rest.starts_with('"') {
                    let (body, tail) = split_phrase(rest.get(1..).unwrap_or(""));
                    b.push_exclude_phrase(body);
                    rest = tail;
                    continue;
                }
                if after_dash.chars().next().is_some_and(is_word_char) {
                    b.push_exclude_word(after_dash);
                    continue;
                }
            }
        }
        b.push_word(chunk);
    }
    b.flush();
    Ok(TextQuery { groups: b.groups })
}

#[cfg(test)]
mod tests {
    use super::super::{tokenize, MAX_QUERY_TERMS};
    use super::*;

    fn term(s: &str) -> Clause {
        Clause::Term(s.to_owned())
    }
    fn prefix(s: &str) -> Clause {
        Clause::Prefix(s.to_owned())
    }
    fn phrase(src: &str) -> Phrase {
        Phrase::new(src).expect("test phrase has tokens")
    }
    fn excl(src: &str) -> Clause {
        Clause::Exclude(Excluded::Phrase(phrase(src)))
    }
    fn parse(q: &str) -> TextQuery {
        parse_text_query(q).expect("within bounds")
    }
    fn groups(q: &str) -> Vec<Vec<Clause>> {
        parse(q).groups.into_iter().map(|g| g.clauses).collect()
    }

    #[test]
    fn search12_plain_terms_match_tokenize() {
        assert_eq!(groups("a b"), vec![vec![term("a"), term("b")]]);
    }

    #[test]
    fn search12_phrase_and_term() {
        assert_eq!(
            groups("\"a b\" c"),
            vec![vec![Clause::Phrase(phrase("a b")), term("c")]]
        );
    }

    #[test]
    fn search12_or_makes_groups() {
        assert_eq!(
            groups("\"a b\" or \"c d\""),
            vec![
                vec![Clause::Phrase(phrase("a b"))],
                vec![Clause::Phrase(phrase("c d"))]
            ]
        );
        assert_eq!(
            groups("a -b or c"),
            vec![vec![term("a"), excl("b")], vec![term("c")]]
        );
    }

    #[test]
    fn search12_exclude_only_group_has_no_positive() {
        let q = parse("-a");
        assert_eq!(q.groups().len(), 1);
        assert!(!q.groups()[0].has_positive());
        assert!(parse("a").groups()[0].has_positive());
    }

    #[test]
    fn search12_exclude_forms_and_prefix() {
        assert_eq!(groups("-\"a b\""), vec![vec![excl("a b")]]);
        assert_eq!(
            groups("-foo*"),
            vec![vec![Clause::Exclude(Excluded::Prefix("foo".into()))]]
        );
        assert_eq!(groups("foo*"), vec![vec![prefix("foo")]]);
        assert_eq!(groups("-foo.bar*"), vec![vec![excl("foo.bar")]]);
    }

    #[test]
    fn search12_cjk_prefix_and_exclude() {
        assert_eq!(groups("東京*"), groups("東京"));
        let g = groups("-東京");
        let Some(Clause::Exclude(Excluded::Phrase(p))) = g.first().and_then(|g| g.first()) else {
            panic!("expected phrase exclude");
        };
        assert_eq!(p.source(), "東京");
        assert_eq!(p.tokens(), ["東", "東京", "京"]);
        assert_eq!(
            groups("東京abc*"),
            vec![vec![term("東"), term("東京"), term("京"), prefix("abc")]]
        );
        assert_eq!(groups("abc東京*"), groups("abc東京"));
    }

    #[test]
    fn search12_empty_phrases_dropped() {
        assert!(parse("\"の\"").is_empty());
        assert!(parse("\"\"").is_empty());
        let q = parse("\"東京の店\"");
        let Some(Clause::Phrase(p)) = q.groups().first().and_then(|g| g.clauses().first()) else {
            panic!("expected phrase");
        };
        assert_eq!(p.source(), "東京の店");
    }

    #[test]
    fn search12_unterminated_quote() {
        assert_eq!(
            groups("a \"unterminated"),
            vec![vec![term("a"), Clause::Phrase(phrase("unterminated"))]]
        );
    }

    #[test]
    fn search12_or_operator_rules() {
        assert_eq!(groups("or a or"), vec![vec![term("a")]]);
        assert_eq!(groups("a or or b"), vec![vec![term("a")], vec![term("b")]]);
        assert_eq!(groups("a OR b"), vec![vec![term("a")], vec![term("b")]]);
        assert_eq!(groups("a Or b").len(), 2);
        assert_eq!(groups("\"or\""), vec![vec![Clause::Phrase(phrase("or"))]]);
        assert_eq!(groups("-or"), vec![vec![excl("or")]]);
        assert_eq!(groups("or*"), vec![vec![prefix("or")]]);
        assert_eq!(groups("ora"), vec![vec![term("ora")]]);
        assert!(parse("or").is_empty());
    }

    #[test]
    fn search12_non_operator_symbols_are_separators() {
        assert_eq!(groups("a-b"), vec![vec![term("a"), term("b")]]);
        assert_eq!(groups("--a"), vec![vec![term("a")]]);
        assert_eq!(groups("- a"), vec![vec![term("a")]]);
        assert_eq!(
            groups("\"x\"-y"),
            vec![vec![Clause::Phrase(phrase("x")), term("y")]]
        );
        assert_eq!(groups("foo**"), vec![vec![term("foo")]]);
        assert_eq!(groups("foo*bar"), vec![vec![term("foo"), term("bar")]]);
        assert_eq!(groups("foo*,"), vec![vec![term("foo")]]);
        assert!(parse("*").is_empty());
        assert_eq!(groups("a\\b"), vec![vec![term("a"), term("b")]]);
    }

    #[test]
    fn search12_prefix_before_quote_and_near() {
        assert_eq!(
            groups("foo*\"bar\""),
            vec![vec![prefix("foo"), Clause::Phrase(phrase("bar"))]]
        );
        assert_eq!(
            groups("NEAR(a b)"),
            vec![vec![term("near"), term("a"), term("b")]]
        );
    }

    #[test]
    fn search12_ideographic_space_is_whitespace() {
        assert_eq!(groups("a\u{3000}-b"), vec![vec![term("a"), excl("b")]]);
    }

    #[test]
    fn search12_empty_inputs_are_ok() {
        for q in ["", "   ", "!?.,", "or", "OR or Or"] {
            assert!(parse(q).is_empty(), "{q:?}");
        }
    }

    #[test]
    fn search12_bounds_are_enforced() {
        let exact = "a".repeat(MAX_QUERY_BYTES);
        assert!(parse_text_query(&exact).is_ok());
        let over = "z".repeat(MAX_QUERY_BYTES + 1);
        let err = parse_text_query(&over).unwrap_err();
        assert_eq!(
            err,
            TextQueryError::QueryTooLong {
                len: MAX_QUERY_BYTES + 1,
                max: MAX_QUERY_BYTES
            }
        );
        assert!(!err.to_string().contains('z'));
        let many: String = (0..=MAX_QUERY_TERMS).map(|i| format!("t{i} ")).collect();
        assert!(many.len() <= MAX_QUERY_BYTES);
        assert!(matches!(
            parse_text_query(&many),
            Err(TextQueryError::TooManyQueryTerms { .. })
        ));
    }

    /// 固定シードの LCG（依存を増やさない）。
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) as usize
        }
    }

    const PIECES: &[&str] = &[
        "a",
        "b",
        "foo",
        "Bar",
        "x1",
        "_",
        " ",
        " ",
        "\u{3000}",
        "\"",
        "\"",
        "-",
        "-",
        "*",
        "*",
        "or",
        "OR",
        " or ",
        "東京都",
        "の",
        "ー",
        "・",
        "İ",
        "\u{212A}",
        "ß",
        "e\u{301}",
        "😀",
        "ＡＢ",
        "\\",
        "(",
        "<",
        "NEAR",
    ];

    fn random_query(rng: &mut Lcg) -> String {
        let n = rng.next() % 24;
        (0..n).map(|_| PIECES[rng.next() % PIECES.len()]).collect()
    }

    fn all_tokens(q: &TextQuery) -> Vec<&str> {
        let mut v = Vec::new();
        for g in q.groups() {
            for c in g.clauses() {
                match c {
                    Clause::Term(t) | Clause::Prefix(t) | Clause::Exclude(Excluded::Prefix(t)) => {
                        v.push(t.as_str())
                    }
                    Clause::Phrase(p) | Clause::Exclude(Excluded::Phrase(p)) => {
                        v.extend(p.tokens().iter().map(String::as_str))
                    }
                }
            }
        }
        v
    }

    #[test]
    fn search12_property_invariants() {
        let mut rng = Lcg(0x5EED_2113);
        for _ in 0..2000 {
            let input = random_query(&mut rng);
            let q = parse(&input);
            assert_eq!(q, parse(&input), "deterministic: {input:?}");
            let toks = tokenize(&input);
            let set: BTreeSet<&str> = toks.iter().map(String::as_str).collect();
            for t in all_tokens(&q) {
                assert!(!t.is_empty());
                assert!(set.contains(t), "invented {t:?} from {input:?}");
            }
            assert!(q.unique_term_count() <= set.len());
            for g in q.groups() {
                assert!(!g.clauses().is_empty());
                for c in g.clauses() {
                    if let Clause::Phrase(p) | Clause::Exclude(Excluded::Phrase(p)) = c {
                        assert!(!p.tokens().is_empty());
                        assert_eq!(p.tokens(), tokenize(p.source()).as_slice());
                    }
                }
            }
            let has_op = input.contains(['"', '-', '*'])
                || input
                    .split_whitespace()
                    .any(|c| c.eq_ignore_ascii_case("or"));
            if !has_op {
                let terms: Vec<String> = q
                    .groups()
                    .iter()
                    .flat_map(|g| g.clauses())
                    .map(|c| match c {
                        Clause::Term(t) => t.clone(),
                        other => panic!("non-term {other:?} for {input:?}"),
                    })
                    .collect();
                assert_eq!(terms, toks, "compat: {input:?}");
                assert!(q.groups().len() <= 1);
            }
        }
    }

    #[test]
    fn search12_worst_case_families_are_bounded() {
        let families: Vec<String> = vec![
            "\"".repeat(MAX_QUERY_BYTES),
            "-a* ".repeat(MAX_QUERY_BYTES / 4),
            "or ".repeat(MAX_QUERY_BYTES / 3),
            format!("\"{}", "word ".repeat(MAX_QUERY_BYTES / 5 - 1)),
            "東".repeat(MAX_QUERY_BYTES / 3),
        ];
        for f in families {
            assert!(f.len() <= MAX_QUERY_BYTES);
            let q = parse_text_query(&f).expect("within bounds");
            let bytes: usize = all_tokens(&q).iter().map(|t| t.len()).sum();
            assert!(bytes <= 4 * f.len());
        }
    }
}
