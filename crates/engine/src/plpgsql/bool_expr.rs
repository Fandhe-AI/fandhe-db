//! PL/pgSQL の条件式を `OR`・`AND`・`NOT`・`BETWEEN`・`IN (リスト)` で分割する純粋関数群
//! （Issue #2880・SQL-68 ポインタ）。
//!
//! `core::plpgsql_host` の条件評価（`eval_cond`）が、複合条件を 3 値論理で結合するために呼ぶ。
//! ここはトークン列の範囲を返すだけで、ストレージ・テナント・SQL 実行には触れない。範囲は
//! 置換前の元のトークン列（`SqlExpr::tokens`）上の位置で表し、変数の位置（`var_refs`）は
//! 演算子の語として解釈しない（変数の値で式の構造が変わらないようにする。ADR D3）。
//! 括弧・`CASE .. END` の不整合は `None` を返し、呼び出し側が fail-closed（`0A000`）にする。

use crate::sql::lexer::{Keyword, Token};
use std::ops::Range;

/// 論理結合の再帰深さの上限（`sql::allowlist` の WHERE グループ深さと同じ系統の上限）。
pub(crate) const MAX_BOOL_DEPTH: usize = 32;

/// `IN (リスト)` の要素数の上限（WHERE の葉の上限と同じ値）。
pub(crate) const MAX_IN_ITEMS: usize = crate::declarative_filter::MAX_METADATA_FILTERS;

/// 語（`IS`・`NOT` 等）の一致（大文字小文字を区別しない）。
fn is_word(t: &Token, w: &str) -> bool {
    matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case(w))
}

/// 全体を囲む対応した外側の括弧を（入れ子の分まで）外した部分列の、`toks` 上の範囲。
pub(crate) fn strip_outer_parens_range(toks: &[Token]) -> Range<usize> {
    let mut lo = 0usize;
    let mut hi = toks.len();
    loop {
        let Some(cur) = toks.get(lo..hi) else {
            return lo..hi;
        };
        let (Some(Token::Punct('(')), Some(Token::Punct(')'))) = (cur.first(), cur.last()) else {
            return lo..hi;
        };
        let mut depth = 0usize;
        for (i, t) in cur.iter().enumerate() {
            match t {
                Token::Punct('(') => depth += 1,
                Token::Punct(')') => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 && i + 1 != cur.len() {
                        return lo..hi;
                    }
                }
                _ => {}
            }
        }
        if hi.saturating_sub(lo) < 2 {
            return lo..hi;
        }
        lo += 1;
        hi -= 1;
    }
}

/// 分割対象の式（元のトークン列と変数の位置）。
pub(crate) struct Exprs<'a> {
    toks: &'a [Token],
    vars: &'a [usize],
}

impl<'a> Exprs<'a> {
    /// `toks` は `SqlExpr::tokens`、`vars` は `SqlExpr::var_refs` の位置。
    pub(crate) fn new(toks: &'a [Token], vars: &'a [usize]) -> Self {
        Self { toks, vars }
    }

    fn word(&self, i: usize, w: &str) -> bool {
        !self.vars.contains(&i) && self.toks.get(i).is_some_and(|t| is_word(t, w))
    }

    fn is_and(&self, i: usize) -> bool {
        matches!(self.toks.get(i), Some(Token::Keyword(Keyword::And)))
    }

    /// `r` の外側の括弧を外した範囲（`toks` 上）。
    pub(crate) fn strip_parens(&self, r: Range<usize>) -> Range<usize> {
        let Some(slice) = self.toks.get(r.clone()) else {
            return r;
        };
        let inner = strip_outer_parens_range(slice);
        r.start.saturating_add(inner.start)..r.start.saturating_add(inner.end)
    }

    /// `r` の深さ 0（括弧・`CASE .. END` の外）で `pred` が真になる位置。括弧・`CASE` の不整合は `None`。
    fn top_level(&self, r: &Range<usize>, pred: impl Fn(usize) -> bool) -> Option<Vec<usize>> {
        let slice = self.toks.get(r.clone())?;
        let (mut paren, mut case) = (0usize, 0usize);
        let mut out = Vec::new();
        for (off, t) in slice.iter().enumerate() {
            let i = r.start.checked_add(off)?;
            match t {
                Token::Punct('(') => paren = paren.checked_add(1)?,
                Token::Punct(')') => paren = paren.checked_sub(1)?,
                _ if self.word(i, "CASE") => case = case.checked_add(1)?,
                _ if self.word(i, "END") => case = case.checked_sub(1)?,
                _ if paren == 0 && case == 0 && pred(i) => out.push(i),
                _ => {}
            }
        }
        (paren == 0 && case == 0).then_some(out)
    }

    /// 長さ 1 の区切り位置 `seps` で `r` を分けた各部分（空の部分があれば `None`）。
    fn pieces(r: &Range<usize>, seps: &[usize]) -> Option<Vec<Range<usize>>> {
        let mut out = Vec::with_capacity(seps.len().checked_add(1)?);
        let mut start = r.start;
        for &s in seps {
            out.push(start..s);
            start = s.checked_add(1)?;
        }
        out.push(start..r.end);
        out.iter().all(|p| p.start < p.end).then_some(out)
    }

    /// 深さ 0 の `OR` で分割する（`OR` が無ければ全体 1 個）。
    pub(crate) fn split_or(&self, r: &Range<usize>) -> Option<Vec<Range<usize>>> {
        let seps = self.top_level(r, |i| self.word(i, "OR"))?;
        Self::pieces(r, &seps)
    }

    /// 深さ 0 の `AND` で分割する。`BETWEEN` と対になる `AND` は分割点にしない。
    pub(crate) fn split_and(&self, r: &Range<usize>) -> Option<Vec<Range<usize>>> {
        let marks = self.top_level(r, |i| self.is_and(i) || self.word(i, "BETWEEN"))?;
        let mut pending = 0usize;
        let mut seps = Vec::new();
        for i in marks {
            if self.is_and(i) {
                if pending > 0 {
                    pending -= 1;
                } else {
                    seps.push(i);
                }
            } else {
                pending = pending.checked_add(1)?;
            }
        }
        Self::pieces(r, &seps)
    }

    /// 先頭の `NOT` を（反復で）剥がした範囲と、剥がした個数。
    pub(crate) fn strip_not(&self, r: Range<usize>) -> (Range<usize>, usize) {
        let mut r = r;
        let mut n = 0usize;
        while r.start.saturating_add(1) < r.end && self.word(r.start, "NOT") {
            r.start += 1;
            n += 1;
        }
        (r, n)
    }

    /// 深さ 0 に `OR`・`AND`・`BETWEEN`・`IN` があるか、先頭が `NOT` の式か。
    pub(crate) fn is_boolean_form(&self, r: &Range<usize>) -> bool {
        if self.word(r.start, "NOT") {
            return true;
        }
        self.top_level(r, |i| {
            self.is_and(i) || ["OR", "BETWEEN", "IN"].iter().any(|w| self.word(i, w))
        })
        .is_some_and(|v| !v.is_empty())
    }

    /// 末尾の `NOT`（`x NOT BETWEEN`・`x NOT IN`）を除いた被演算子の範囲と否定か。
    fn trim_not(&self, before: Range<usize>) -> Option<(Range<usize>, bool)> {
        let last = before.end.checked_sub(1)?;
        let (x, neg) = if before.start < before.end && self.word(last, "NOT") {
            (before.start..last, true)
        } else {
            (before, false)
        };
        (x.start < x.end).then_some((x, neg))
    }

    #[allow(clippy::type_complexity)]
    /// `x [NOT] BETWEEN lo AND hi` の (x, 否定か, lo, hi)。`SYMMETRIC`・`ASYMMETRIC` は `None`。
    pub(crate) fn split_between(
        &self,
        r: &Range<usize>,
    ) -> Option<(Range<usize>, bool, Range<usize>, Range<usize>)> {
        let marks = self.top_level(r, |i| self.is_and(i) || self.word(i, "BETWEEN"))?;
        let [b, a] = marks.as_slice() else {
            return None;
        };
        if !self.word(*b, "BETWEEN") || !self.is_and(*a) {
            return None;
        }
        let (x, neg) = self.trim_not(r.start..*b)?;
        let lo = b.checked_add(1)?..*a;
        let hi = a.checked_add(1)?..r.end;
        if lo.start >= lo.end
            || hi.start >= hi.end
            || self.word(lo.start, "SYMMETRIC")
            || self.word(lo.start, "ASYMMETRIC")
        {
            return None;
        }
        Some((x, neg, lo, hi))
    }

    /// `x [NOT] IN (e1, e2, ..)` の (x, 否定か, 要素の範囲)。副問い合わせ・空リスト・末尾カンマ・
    /// 要素数が上限超過の形は `None`。
    #[allow(clippy::type_complexity)]
    pub(crate) fn split_in_list(
        &self,
        r: &Range<usize>,
    ) -> Option<(Range<usize>, bool, Vec<Range<usize>>)> {
        let marks = self.top_level(r, |i| self.word(i, "IN"))?;
        let [at] = marks.as_slice() else {
            return None;
        };
        let open = at.checked_add(1)?;
        let close = r.end.checked_sub(1)?;
        if !matches!(self.toks.get(open), Some(Token::Punct('(')))
            || !matches!(self.toks.get(close), Some(Token::Punct(')')))
            || open >= close
        {
            return None;
        }
        // 先頭の `(` が末尾の `)` と対応していること（`IN (1) AND (2)` のような形を除く）。
        if strip_outer_parens_range(self.toks.get(open..r.end)?).start == 0 {
            return None;
        }
        let (x, neg) = self.trim_not(r.start..*at)?;
        let inner = open.checked_add(1)?..close;
        if self.word(inner.start, "WITH")
            || matches!(
                self.toks.get(inner.start),
                Some(Token::Keyword(Keyword::Select))
            )
        {
            return None;
        }
        let commas = self.top_level(&inner, |i| {
            matches!(self.toks.get(i), Some(Token::Punct(',')))
        })?;
        if commas.len() >= MAX_IN_ITEMS {
            return None;
        }
        let items = Self::pieces(&inner, &commas)?;
        Some((x, neg, items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn toks(s: &str) -> Vec<Token> {
        tokenize(s).expect("tokenize")
    }

    fn texts(t: &[Token], rs: &[Range<usize>]) -> Vec<usize> {
        let _ = t;
        rs.iter().map(|r| r.end - r.start).collect()
    }

    #[test]
    fn or_and_split_respect_parens_and_between() {
        let t = toks("a = 1 OR b BETWEEN 1 AND 2 AND (c OR d) AND e");
        let e = Exprs::new(&t, &[]);
        let all = 0..t.len();
        let or = e.split_or(&all).expect("or");
        assert_eq!(or.len(), 2);
        let and = e.split_and(&or[1]).expect("and");
        assert_eq!(and.len(), 3);
        assert_eq!(texts(&t, &and), vec![5, 5, 1]);
    }

    #[test]
    fn case_expression_hides_inner_and() {
        let t = toks("CASE WHEN a AND b THEN 1 ELSE 0 END = 1");
        let e = Exprs::new(&t, &[]);
        assert_eq!(e.split_and(&(0..t.len())).expect("and").len(), 1);
        assert!(!e.is_boolean_form(&(0..t.len())));
    }

    #[test]
    fn unbalanced_parens_fail_closed() {
        let t = toks("a ) AND ( b");
        let e = Exprs::new(&t, &[]);
        assert!(e.split_and(&(0..t.len())).is_none());
        let t = toks("( a AND b");
        let e = Exprs::new(&t, &[]);
        assert!(e.split_or(&(0..t.len())).is_none());
    }

    #[test]
    fn not_chain_is_stripped_iteratively() {
        let t = toks("NOT NOT NOT a");
        let e = Exprs::new(&t, &[]);
        let (r, n) = e.strip_not(0..t.len());
        assert_eq!((r, n), (3..4, 3));
    }

    #[test]
    fn variable_positions_are_not_operators() {
        let t = toks("a OR b");
        let e = Exprs::new(&t, &[1]);
        assert_eq!(e.split_or(&(0..t.len())).expect("or").len(), 1);
    }

    #[test]
    fn between_and_in_decompose() {
        let t = toks("x NOT BETWEEN 1 AND 5");
        let e = Exprs::new(&t, &[]);
        let (x, neg, lo, hi) = e.split_between(&(0..t.len())).expect("between");
        assert_eq!((x, neg, lo, hi), (0..1, true, 3..4, 5..6));
        let t = toks("BETWEEN SYMMETRIC 1 AND 5");
        assert!(Exprs::new(&t, &[]).split_between(&(0..t.len())).is_none());
        let t = toks("x NOT IN (1, (2, 3), f(4, 5))");
        let e = Exprs::new(&t, &[]);
        let (x, neg, items) = e.split_in_list(&(0..t.len())).expect("in");
        assert_eq!((x, neg, items.len()), (0..1, true, 3));
        for s in [
            "x IN (SELECT 1)",
            "x IN ()",
            "x IN (1,)",
            "x IN (1) AND (2)",
        ] {
            let t = toks(s);
            assert!(
                Exprs::new(&t, &[]).split_in_list(&(0..t.len())).is_none(),
                "{s}"
            );
        }
    }

    #[test]
    fn in_list_item_count_is_bounded() {
        let items = (0..=MAX_IN_ITEMS)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let t = toks(&format!("x IN ({items})"));
        assert!(Exprs::new(&t, &[]).split_in_list(&(0..t.len())).is_none());
    }
}
