//! 一致スパン（バイトオフセット）を求める純関数（Issue #2124・親 #2123。
//! 対象ビヘイビア SEARCH-14 のポインタ: `docs/spec/04-behavior/search.md`）。
//!
//! 責務境界: 「本文」と「解釈済みの検索語」から、本文中の一致箇所のバイト範囲の列を返す。
//! ハイライト／スニペット（`ts_headline` 相当。#2125）が下回りとして呼ぶ想定で、
//! storage／catalog／RLS とは結線しない。渡された 1 行の本文だけを見るため、可視行・
//! 列権限の検査は呼び出し側（#2125・#2126）の責務。
//!
//! 分割規則は親モジュールの `walk_tokens`（索引・クエリと共通の単一真実源）を使う。
//! 小文字化で長さが変わる文字（`İ`・ケルビン記号等）でもオフセットがずれず、範囲の端は
//! 常に元の本文の UTF-8 文字境界になる。CJK のバイグラム一致は元の文字の範囲へ戻る。
//!
//! 一致の意味づけ（OR・フレーズ・除外・前方一致）は #2113 以降のパーサと #2125 の責務で、
//! 本モジュールは「正規化済み検索語の集合に含まれるトークン」の範囲だけを返す。
//! 助詞 1 文字の検索語は索引と同じく単独では一致しない（その文字を含むバイグラムは一致）。
//!
//! エラーは [`MatchSpanError`]。`wire_code`（上限超過は `54000` 想定）への写像は #2125 が担う。

use std::borrow::Cow;
use std::collections::BTreeSet;

use super::{validate_query_bounds, MAX_DOC_BYTES, MAX_QUERY_BYTES, MAX_QUERY_TERMS};

/// [`MatchSpanLimits::max_spans`] の既定値。
///
/// 1 MiB の CJK 本文は約 35 万文字でトークンは最大で文字数の 2 倍程度になるが、
/// ハイライト用途では 65,536 件で十分に大きく、`Vec<MatchSpan>`（16 B/件）の確保を
/// 約 1 MiB に抑えられる。
pub const DEFAULT_MAX_MATCH_SPANS: usize = 65_536;

/// 本文の一致範囲（バイトの半開区間 `[start, end)`）。
///
/// 両端とも元の本文の UTF-8 文字境界に揃う。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MatchSpan {
    /// 開始バイトオフセット（含む）。
    pub start: usize,
    /// 終了バイトオフセット（含まない）。
    pub end: usize,
}

/// 入力長・スパン数の上限（fail-closed）。#2125 が CLI 設定値を流し込める形にしてある。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MatchSpanLimits {
    /// 本文のバイト長の上限。走査の前に `O(1)` で検証する。
    pub max_text_bytes: usize,
    /// 統合前の生の一致数の上限。超過時は切り詰めず `Err` にする。
    pub max_spans: usize,
}

impl Default for MatchSpanLimits {
    fn default() -> Self {
        Self {
            max_text_bytes: MAX_DOC_BYTES,
            max_spans: DEFAULT_MAX_MATCH_SPANS,
        }
    }
}

/// 一致スパン算出のエラー。メッセージに本文・クエリの内容は含めない（長さと上限値のみ）。
///
/// 後続（SEARCH-16 のステミング等）で variant を足すため `#[non_exhaustive]`
/// （`docs/design/error-enum-non-exhaustive-policy.md`）。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum MatchSpanError {
    /// 本文のバイト長が上限を超える。
    TextTooLong { len: usize, max: usize },
    /// 一致数が上限を超える（切り詰めて返さない）。
    TooManySpans { max: usize },
    /// 検索語の一意語数が上限を超える。
    TooManyQueryTerms { unique_terms: usize, max: usize },
    /// 検索語（クエリ）のバイト長が上限を超える。
    QueryTooLong { len: usize, max: usize },
}

impl std::fmt::Display for MatchSpanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TextTooLong { len, max } => {
                write!(f, "text is too long: {len} bytes (max {max})")
            }
            Self::TooManySpans { max } => write!(f, "too many match spans (max {max})"),
            Self::TooManyQueryTerms { unique_terms, max } => {
                write!(f, "too many query terms: {unique_terms} (max {max})")
            }
            Self::QueryTooLong { len, max } => {
                write!(f, "query is too long: {len} bytes (max {max})")
            }
        }
    }
}

impl std::error::Error for MatchSpanError {}

/// 比較前の語の正規化点（ステミング〔SEARCH-16〕の拡張点。今は恒等で確保しない）。
///
/// 後でステミングを入れるときは、本文トークンと [`MatchTerms`] の構築側の両方にこの関数を
/// 通す。範囲は元のトークンのものを保つ。
fn normalize_for_match(term: &str) -> Cow<'_, str> {
    Cow::Borrowed(term)
}

/// 解釈済み（正規化済み）の検索語の集合。決定性のため `BTreeSet` を使う。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct MatchTerms {
    terms: BTreeSet<String>,
}

impl MatchTerms {
    /// クエリ文字列を索引と同じトークナイザ（ストップワード除去 ON）で分割して作る。
    ///
    /// `MAX_QUERY_BYTES`・`MAX_QUERY_TERMS` を検証してからトークン化する。
    pub fn from_query(query: &str) -> Result<Self, MatchSpanError> {
        validate_query_bounds(query).map_err(|e| match e {
            super::SparseError::QueryTooLong { len, max } => {
                MatchSpanError::QueryTooLong { len, max }
            }
            super::SparseError::TooManyQueryTerms { unique_terms, max } => {
                MatchSpanError::TooManyQueryTerms { unique_terms, max }
            }
            // validate_query_bounds が返すのは上の 2 種のみ。万一増えても拒否側に倒す。
            _ => MatchSpanError::QueryTooLong {
                len: query.len(),
                max: MAX_QUERY_BYTES,
            },
        })?;
        let mut terms = BTreeSet::new();
        for t in super::tokenize(query) {
            terms.insert(normalize_for_match(&t).into_owned());
        }
        Ok(Self { terms })
    }

    /// 呼び出し側（将来のクエリパーサ）が正規化済みの語を直接渡す入口。
    ///
    /// 語数（一意）・1 語のバイト長・合計バイト長を上限検証する。空文字列は無視する。
    pub fn from_terms<I, S>(terms: I) -> Result<Self, MatchSpanError>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut set = BTreeSet::new();
        let mut total = 0usize;
        for t in terms {
            let t: String = t.into();
            if t.is_empty() {
                continue;
            }
            total = total.saturating_add(t.len());
            if t.len() > MAX_QUERY_BYTES || total > MAX_QUERY_BYTES {
                return Err(MatchSpanError::QueryTooLong {
                    len: total.max(t.len()),
                    max: MAX_QUERY_BYTES,
                });
            }
            set.insert(normalize_for_match(&t).into_owned());
            if set.len() > MAX_QUERY_TERMS {
                return Err(MatchSpanError::TooManyQueryTerms {
                    unique_terms: set.len(),
                    max: MAX_QUERY_TERMS,
                });
            }
        }
        Ok(Self { terms: set })
    }

    /// 検索語が 1 つも無いか。
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }
}

fn check_text(text: &str, limits: &MatchSpanLimits) -> Result<(), MatchSpanError> {
    if text.len() > limits.max_text_bytes {
        return Err(MatchSpanError::TextTooLong {
            len: text.len(),
            max: limits.max_text_bytes,
        });
    }
    Ok(())
}

/// 本文の全トークンとその範囲を順に渡す（#2125 が語数計算に使う下位 API）。
///
/// 本文長を検証してから `walk_tokens` を呼ぶ。確保は呼び出し側に任せる。
/// トークン列は索引と同じ規則（ストップワード除去 ON）。
pub fn for_each_token_span<F>(
    text: &str,
    limits: &MatchSpanLimits,
    mut f: F,
) -> Result<(), MatchSpanError>
where
    F: FnMut(&str, MatchSpan),
{
    check_text(text, limits)?;
    super::walk_tokens(text, true, |token, r| {
        f(
            token,
            MatchSpan {
                start: r.start,
                end: r.end,
            },
        );
        true
    });
    Ok(())
}

/// 本文中で `terms` に一致するトークンの範囲を、昇順・非重複で返す（SEARCH-14 ポインタ）。
///
/// 重なる範囲（`next.start < cur.end`）は統合する（CJK `東京都` はバイグラムの連鎖で
/// 1 本になる）。接するだけ（`next.start == cur.end`）の範囲は統合しない。統合前の生の
/// 一致数が `limits.max_spans` を超えたら切り詰めず `Err(TooManySpans)`。
/// 本文・検索語が空、または一致が無ければ空の `Vec`（`Err` にはしない）。
pub fn match_spans(
    text: &str,
    terms: &MatchTerms,
    limits: &MatchSpanLimits,
) -> Result<Vec<MatchSpan>, MatchSpanError> {
    check_text(text, limits)?;
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let mut raw: Vec<MatchSpan> = Vec::new();
    let mut overflow = false;
    super::walk_tokens(text, true, |token, r| {
        if terms.terms.contains(normalize_for_match(token).as_ref()) {
            if raw.len() >= limits.max_spans {
                overflow = true;
                return false;
            }
            raw.push(MatchSpan {
                start: r.start,
                end: r.end,
            });
        }
        true
    });
    if overflow {
        return Err(MatchSpanError::TooManySpans {
            max: limits.max_spans,
        });
    }
    raw.sort_unstable();
    let mut merged: Vec<MatchSpan> = Vec::with_capacity(raw.len());
    for s in raw {
        match merged.last_mut() {
            Some(cur) if s.start < cur.end => cur.end = cur.end.max(s.end),
            _ => merged.push(s),
        }
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(text: &str, q: &str) -> Vec<MatchSpan> {
        let terms = MatchTerms::from_query(q).expect("query ok");
        let out = match_spans(text, &terms, &MatchSpanLimits::default()).expect("spans ok");
        assert_invariants(text, &out);
        out
    }

    fn assert_invariants(text: &str, out: &[MatchSpan]) {
        let mut prev_end = 0usize;
        for s in out {
            assert!(s.start < s.end && s.end <= text.len());
            assert!(text.is_char_boundary(s.start) && text.is_char_boundary(s.end));
            assert!(s.start >= prev_end, "sorted and non-overlapping");
            prev_end = s.end;
            let _ = &text[s.start..s.end];
        }
    }

    fn sp(start: usize, end: usize) -> MatchSpan {
        MatchSpan { start, end }
    }

    #[test]
    fn search14_ascii_case_insensitive() {
        let t = "Hello, world_1 foo";
        assert_eq!(spans(t, "WORLD_1 hello"), vec![sp(0, 5), sp(7, 14)]);
    }

    #[test]
    fn search14_cjk_bigram_chain_merges() {
        assert_eq!(spans("東京都に行く", "東京都"), vec![sp(0, 9)]);
        assert_eq!(spans("東京都に行く", "京"), vec![sp(3, 6)]);
    }

    #[test]
    fn search14_mixed() {
        let t = "Rust で東京タワー";
        let out = spans(t, "rust タワー");
        assert_eq!(out.first(), Some(&sp(0, 4)));
        let tower = t.find("タワー").unwrap();
        assert!(out.contains(&sp(tower, t.len())));
    }

    #[test]
    fn search14_stopword_unigram_alone_does_not_match_but_bigram_does() {
        assert!(spans("東京の都", "の").is_empty());
        assert_eq!(spans("東京の都", "京の"), vec![sp(3, 9)]);
    }

    #[test]
    fn search14_touching_spans_are_not_merged() {
        assert_eq!(spans("東京", "東 京"), vec![sp(0, 3), sp(3, 6)]);
    }

    #[test]
    fn search14_lowercase_expansion_keeps_char_boundaries() {
        let t = "İstanbul \u{212A}ing";
        let out = spans(t, "i stanbul k ing");
        assert!(!out.is_empty());
        assert_eq!(out.first(), Some(&sp(0, 2)));
    }

    #[test]
    fn search14_emoji_in_word_is_a_separator() {
        let t = "ab😀cd";
        assert_eq!(spans(t, "ab cd"), vec![sp(0, 2), sp(6, 8)]);
    }

    #[test]
    fn search14_empty_inputs_yield_empty() {
        assert!(spans("", "abc").is_empty());
        assert!(spans("abc", "").is_empty());
        assert!(spans("!!!", "!!!").is_empty());
    }

    #[test]
    fn search14_limits_text_bytes() {
        let terms = MatchTerms::from_query("a").unwrap();
        let lim = MatchSpanLimits {
            max_text_bytes: 3,
            max_spans: 10,
        };
        assert!(match_spans("a b", &terms, &lim).is_ok());
        assert_eq!(
            match_spans("a b ", &terms, &lim),
            Err(MatchSpanError::TextTooLong { len: 4, max: 3 })
        );
    }

    #[test]
    fn search14_limits_span_count_fail_closed() {
        let terms = MatchTerms::from_query("a").unwrap();
        let lim = MatchSpanLimits {
            max_text_bytes: 100,
            max_spans: 2,
        };
        assert_eq!(match_spans("a a", &terms, &lim).unwrap().len(), 2);
        assert_eq!(
            match_spans("a a a", &terms, &lim),
            Err(MatchSpanError::TooManySpans { max: 2 })
        );
    }

    #[test]
    fn search14_query_limits() {
        let long = "a".repeat(MAX_QUERY_BYTES + 1);
        assert!(matches!(
            MatchTerms::from_query(&long),
            Err(MatchSpanError::QueryTooLong { .. })
        ));
        let many: Vec<String> = (0..=MAX_QUERY_TERMS).map(|i| format!("t{i}")).collect();
        assert!(matches!(
            MatchTerms::from_query(&many.join(" ")),
            Err(MatchSpanError::TooManyQueryTerms { .. })
        ));
        assert!(matches!(
            MatchTerms::from_terms(many),
            Err(MatchSpanError::TooManyQueryTerms { .. })
        ));
        assert!(matches!(
            MatchTerms::from_terms([long]),
            Err(MatchSpanError::QueryTooLong { .. })
        ));
    }

    #[test]
    fn search14_for_each_token_span_reports_all_tokens() {
        let mut got = Vec::new();
        for_each_token_span("ab 東京", &MatchSpanLimits::default(), |t, s| {
            got.push((t.to_owned(), s))
        })
        .unwrap();
        assert_eq!(got[0], ("ab".to_owned(), sp(0, 2)));
        assert!(got.iter().any(|(t, s)| t == "東京" && *s == sp(3, 9)));
    }

    #[test]
    fn search14_pseudo_random_invariants() {
        let alphabet: Vec<char> = "abAB_ -東京都の日本ーカ・İ\u{212A}ßΣé😀Ａ\u{301}"
            .chars()
            .collect();
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let lim = MatchSpanLimits::default();
        for _ in 0..2000 {
            let mut s = String::new();
            for _ in 0..24 {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                s.push(alphabet[((state >> 33) as usize) % alphabet.len()]);
            }
            let terms = MatchTerms::from_query(&s).unwrap();
            let out = match_spans(&s, &terms, &lim).unwrap();
            assert_invariants(&s, &out);
            for sp in &out {
                let toks = crate::sparse::tokenize(&s[sp.start..sp.end]);
                assert!(toks.iter().any(|t| terms.terms.contains(t)));
            }
        }
    }
}
