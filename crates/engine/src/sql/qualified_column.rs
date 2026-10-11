//! 単一テーブル SELECT 系の修飾列参照（`t.col`）正規化（SQL-44 ポインタ、Issue #1535、
//! 親 #1533。先行 #1534）。
//!
//! 責務境界: `sql::allowlist::validate_select_statement`（トップレベル SELECT・
//! `EXPLAIN`・カーソル・`COPY (SELECT) TO`・サブクエリ内側の再検証）と CTE 主クエリが、
//! 構文解析の**前**に本モジュールの [`normalize_single_table_qualifiers`] を 1 回だけ
//! 呼ぶ。FROM の公開名（別名があれば別名、無ければ表名）に一致する
//! [`Token::QualifiedIdent`] を `Token::Ident(name)` へ 1 対 1 で置き換えるため、
//! 修飾あり・なしで AST が完全に同一になる（行・値・列名・エラーコードが一致）。
//! トークン数は変わらず `quoted_null` の添字も保たれる。
//!
//! fail-closed の判断:
//! - 公開名に一致しない修飾子は `42P01`。カタログは照会しない（存在オラクルにしない）。
//! - 外側スコープの公開名に一致する修飾（相関参照）は `42601`（SQL-29 の契約を維持）。
//! - スキーマ修飾 FROM・JOIN・集合演算・FROM なし・解析不能なスコープは「不透明」として
//!   書き換えず、従来のパーサー判定（`42601` 等）に委ねる。
//! - 関数名位置（直後が `(`）・`AS` の直後・`::` の直後・`EXTRACT(` 直後のフィールド名は
//!   列参照ではないため書き換えない。
//!
//! 走査は反復処理（再帰なし）で、入れ子は `MAX_SUBQUERY_DEPTH` で打ち切る。修飾が無ければ
//! 借用を返し確保を増やさない。untrusted 入力経路のため添字アクセスは使わない。

use std::borrow::Cow;

use crate::sql::allowlist::{
    from_clause_starts_join, is_set_operator_ident, probe_from_table_ref,
    set_operator_is_followed_by_branch, SqlSurfaceError, MAX_SUBQUERY_DEPTH,
};
use crate::sql::lexer::{quoted_bool_word_in_leaf_position, Keyword, Token};
use crate::sql::relation::TableRef;

/// `( SELECT ... )` または先頭 `SELECT` 1 つ分の解析スコープ。
struct Scope {
    /// `SELECT` トークンの位置。
    start: usize,
    /// スコープ終端（対応する `)` の位置、無ければ総トークン数）。
    end: usize,
    /// 直近の外側スコープの添字。
    parent: Option<usize>,
    /// 外側スコープの個数（収集時に確定し、祖先走査による O(n^2) を避ける）。
    depth: usize,
    /// 書き換え対象の区間（投影・FROM 参照以降〜終端語の手前）と FROM 参照。
    /// `None` は不透明スコープ（書き換えない）。
    info: Option<ScopeInfo>,
}

struct ScopeInfo {
    from_ref: TableRef,
    proj: (usize, usize),
    tail: (usize, usize),
}

/// 修飾列参照を非修飾へ正規化する。修飾が無ければ `Cow::Borrowed`。
pub(crate) fn normalize_single_table_qualifiers(
    tokens: &[Token],
) -> Result<Cow<'_, [Token]>, SqlSurfaceError> {
    if !tokens
        .iter()
        .any(|t| matches!(t, Token::QualifiedIdent { .. }))
    {
        return Ok(Cow::Borrowed(tokens));
    }
    let mut scopes = collect_scopes(tokens);
    for idx in 0..scopes.len() {
        let info = scope_info(tokens, &scopes, idx);
        if let Some(s) = scopes.get_mut(idx) {
            s.info = info;
        }
    }

    // 1 パスで各 QualifiedIdent の最内スコープを求める（開始位置順に並んでいる）。
    let mut out: Option<Vec<Token>> = None;
    let mut active: Vec<usize> = Vec::new();
    let mut next_scope = 0usize;
    for (i, tok) in tokens.iter().enumerate() {
        while let Some(&top) = active.last() {
            if scopes.get(top).is_some_and(|s| s.end <= i) {
                active.pop();
            } else {
                break;
            }
        }
        if scopes.get(next_scope).is_some_and(|s| s.start == i) {
            active.push(next_scope);
            next_scope += 1;
        }
        let Token::QualifiedIdent { qualifier, name } = tok else {
            continue;
        };
        let Some(&cur) = active.last() else { continue };
        let Some(info) = scopes.get(cur).and_then(|s| s.info.as_ref()) else {
            continue;
        };
        if !(in_range(info.proj, i) || in_range(info.tail, i)) || is_non_column_position(tokens, i)
        {
            continue;
        }
        if info.from_ref.matches_qualifier(qualifier) {
            let buf = out.get_or_insert_with(|| tokens.to_vec());
            if let Some(slot) = buf.get_mut(i) {
                // `x.true`／`x.false` は列参照であり真偽リテラルではない。WHERE の述語葉位置では
                // 引用付き相当の `QuotedBoolWord` にして列述語として扱う（PR #2309 指摘）。
                // それ以外の位置（比較右辺・投影・ORDER BY 等）で `Ident("true")` に落とすと
                // 後段が真偽リテラル扱い（`flag = x.true` が定数比較化）するため、fail-closed で
                // 明示拒否する（列比較は未対応）。
                let is_bool_word =
                    name.eq_ignore_ascii_case("true") || name.eq_ignore_ascii_case("false");
                let leaf = is_bool_word
                    && tokens.get(..i).is_some_and(|h| {
                        h.iter()
                            .any(|t| matches!(t, Token::Keyword(Keyword::Where)))
                    })
                    && quoted_bool_word_in_leaf_position(tokens, i);
                // `x.null` も同様に、`COALESCE`／`NULLIF`／`CASE` の引数位置で `Ident("null")` が
                // `Expr::Null` に解釈され列検証を素通りするため、fail-closed で拒否する。
                if name.eq_ignore_ascii_case("null") {
                    return Err(SqlSurfaceError::unsupported(
                        "qualified null column reference is not supported",
                    ));
                }
                // `x.distinct`／`x.not` 等は Ident へ落とすと後段が構文語（DISTINCT 修飾子・
                // NOT 述語等）として再解釈し、列の存在検証なしで受理される（PR #2309 指摘・P1）。
                // 構文語と衝突する名前の修飾列参照は fail-closed で拒否する（引用識別子は対象外）。
                if is_syntax_word(name) {
                    return Err(SqlSurfaceError::unsupported(
                        "qualified column reference named like a SQL keyword is not supported",
                    ));
                }
                if is_bool_word && !leaf {
                    return Err(SqlSurfaceError::unsupported(
                        "qualified boolean-word column reference is only supported as a WHERE predicate",
                    ));
                }
                *slot = if leaf {
                    Token::QuotedBoolWord(name.clone())
                } else {
                    Token::Ident(name.clone())
                };
            }
            continue;
        }
        // 外側スコープの公開名に一致する修飾は相関参照（未対応）。
        let mut anc = scopes.get(cur).and_then(|s| s.parent);
        while let Some(a) = anc {
            let s = scopes.get(a);
            if let Some(ai) = s.and_then(|s| s.info.as_ref()) {
                if ai.from_ref.matches_qualifier(qualifier) {
                    return Err(SqlSurfaceError::unsupported(
                        "correlated column reference to an outer query is not supported",
                    ));
                }
            }
            anc = s.and_then(|s| s.parent);
        }
        return Err(SqlSurfaceError::undefined_table(qualifier.clone()));
    }
    Ok(match out {
        Some(v) => Cow::Owned(v),
        None => Cow::Borrowed(tokens),
    })
}

/// 後段パーサが `Ident` を構文語として消費しうる語か（修飾列名の再解釈防止用）。
fn is_syntax_word(name: &str) -> bool {
    const WORDS: &[&str] = &[
        "distinct",
        "all",
        "not",
        "and",
        "or",
        "is",
        "in",
        "between",
        "like",
        "ilike",
        "as",
        "asc",
        "desc",
        "case",
        "when",
        "then",
        "else",
        "end",
        "group",
        "having",
        "over",
        "using",
        "offset",
        "with",
        "union",
        "intersect",
        "except",
        "join",
        "inner",
        "left",
        "right",
        "full",
        "natural",
        "fetch",
        "for",
        "on",
    ];
    WORDS.iter().any(|w| name.eq_ignore_ascii_case(w))
}

fn in_range(r: (usize, usize), i: usize) -> bool {
    r.0 <= i && i < r.1
}

/// 列参照ではない位置（関数名・型名・別名・`EXTRACT` のフィールド名）。
fn is_non_column_position(tokens: &[Token], i: usize) -> bool {
    if matches!(tokens.get(i.wrapping_add(1)), Some(Token::Punct('('))) {
        return true;
    }
    let prev = i.checked_sub(1).and_then(|p| tokens.get(p));
    match prev {
        Some(Token::Ident(w)) if w.eq_ignore_ascii_case("AS") => true,
        Some(Token::TypeCast) => true,
        Some(Token::Punct('(')) => {
            matches!(
                i.checked_sub(2).and_then(|p| tokens.get(p)),
                Some(Token::Ident(w)) if w.eq_ignore_ascii_case("EXTRACT")
            )
        }
        _ => false,
    }
}

/// スコープ（先頭 `SELECT` と `( SELECT`）を開始位置順に集める。
fn collect_scopes(tokens: &[Token]) -> Vec<Scope> {
    let mut scopes: Vec<Scope> = Vec::new();
    // 括弧フレーム: Some(scope index) はサブクエリ括弧。
    let mut parens: Vec<Option<usize>> = Vec::new();
    let mut scope_stack: Vec<usize> = Vec::new();
    if matches!(tokens.first(), Some(Token::Keyword(Keyword::Select))) {
        scopes.push(Scope {
            start: 0,
            end: tokens.len(),
            parent: None,
            depth: 0,
            info: None,
        });
        scope_stack.push(0);
    }
    for (i, tok) in tokens.iter().enumerate() {
        match tok {
            Token::Punct('(') => {
                if matches!(
                    tokens.get(i.wrapping_add(1)),
                    Some(Token::Keyword(Keyword::Select))
                ) {
                    let idx = scopes.len();
                    scopes.push(Scope {
                        start: i + 1,
                        end: tokens.len(),
                        parent: scope_stack.last().copied(),
                        depth: scope_stack.len(),
                        info: None,
                    });
                    scope_stack.push(idx);
                    parens.push(Some(idx));
                } else {
                    parens.push(None);
                }
            }
            Token::Punct(')') => {
                if let Some(Some(idx)) = parens.pop() {
                    if let Some(s) = scopes.get_mut(idx) {
                        s.end = i;
                    }
                    scope_stack.pop();
                }
            }
            _ => {}
        }
    }
    scopes
}

/// スコープの FROM 参照と書き換え区間を求める。不透明なら `None`。
fn scope_info(tokens: &[Token], scopes: &[Scope], idx: usize) -> Option<ScopeInfo> {
    let scope = scopes.get(idx)?;
    if scope.depth > MAX_SUBQUERY_DEPTH {
        return None;
    }
    let (start, end) = (scope.start, scope.end);
    let body = tokens.get(..end)?;
    // 括弧深さ 0 の最初の FROM と、集合演算子の有無。
    let mut depth = 0usize;
    let mut from_idx = None;
    for i in start..end {
        match body.get(i)? {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => depth = depth.checked_sub(1)?,
            Token::Keyword(Keyword::From) if depth == 0 && from_idx.is_none() => {
                from_idx = Some(i);
            }
            Token::Ident(w)
                if depth == 0
                    && is_set_operator_ident(w)
                    && set_operator_is_followed_by_branch(body, i) =>
            {
                return None;
            }
            _ => {}
        }
    }
    let from_idx = from_idx?;
    if from_clause_starts_join(body, from_idx + 1) {
        return None;
    }
    let probe_end = end.min(from_idx + 5);
    let (from_ref, consumed) = probe_from_table_ref(tokens.get(from_idx + 1..probe_end)?)?;
    let tail_start = from_idx + 1 + consumed;
    // FROM 参照の直後が単一テーブル SELECT の後続句（または終端）でなければ不透明
    // （カンマ結合・`OUTER JOIN` 等の未知の結合形で、別名と誤読した名前により
    // 従来の `42601` が `42P01` に化けるのを防ぐ。fail-closed）。
    let follows_ok = match body.get(tail_start) {
        None => true,
        Some(Token::Punct(';')) | Some(Token::Punct(')')) => true,
        Some(Token::Keyword(Keyword::Where | Keyword::Order | Keyword::Limit)) => true,
        Some(Token::Ident(w)) => ["GROUP", "HAVING", "OFFSET", "USING", "HINT"]
            .iter()
            .any(|k| w.eq_ignore_ascii_case(k)),
        _ => false,
    };
    if !follows_ok {
        return None;
    }
    // 終端語（LIMIT・`;`・USING PLAN/MODE・HINT ORDER）の手前まで。
    let mut depth = 0usize;
    let mut tail_end = end;
    for i in tail_start..end {
        match body.get(i)? {
            Token::Punct('(') => depth += 1,
            Token::Punct(')') => match depth.checked_sub(1) {
                Some(d) => depth = d,
                None => {
                    tail_end = i;
                    break;
                }
            },
            Token::Keyword(Keyword::Limit) | Token::Punct(';') if depth == 0 => {
                tail_end = i;
                break;
            }
            Token::Ident(w) if depth == 0 => {
                let next = body.get(i + 1);
                let stop = (w.eq_ignore_ascii_case("USING")
                    && matches!(next, Some(Token::Ident(n))
                        if n.eq_ignore_ascii_case("PLAN") || n.eq_ignore_ascii_case("MODE")))
                    || (w.eq_ignore_ascii_case("HINT")
                        && matches!(next, Some(Token::Keyword(Keyword::Order))));
                if stop {
                    tail_end = i;
                    break;
                }
            }
            _ => {}
        }
    }
    Some(ScopeInfo {
        from_ref,
        proj: (start + 1, from_idx),
        tail: (tail_start, tail_end),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::lexer::tokenize;

    fn norm(sql: &str) -> Result<Vec<Token>, SqlSurfaceError> {
        let toks = tokenize(sql).expect("lex");
        normalize_single_table_qualifiers(&toks).map(|c| c.into_owned())
    }

    fn code(sql: &str) -> &'static str {
        match norm(sql) {
            Ok(_) => "ok",
            Err(SqlSurfaceError::UndefinedTable { .. }) => "42P01",
            Err(SqlSurfaceError::UnsupportedSyntax { .. }) => "42601",
            Err(_) => "other",
        }
    }

    fn has_qualified(sql: &str) -> bool {
        norm(sql)
            .expect("normalize")
            .iter()
            .any(|t| matches!(t, Token::QualifiedIdent { .. }))
    }

    #[test]
    fn borrowed_when_no_qualifier() {
        let toks = tokenize("SELECT id FROM docs LIMIT 1").expect("lex");
        assert!(matches!(
            normalize_single_table_qualifiers(&toks),
            Ok(Cow::Borrowed(_))
        ));
    }

    #[test]
    fn rewrites_matching_qualifier_one_to_one() {
        let q = tokenize("SELECT x.id FROM docs AS x WHERE x.lang = 'en' ORDER BY x.id LIMIT 3")
            .expect("lex");
        let n = normalize_single_table_qualifiers(&q).expect("ok");
        let p = tokenize("SELECT id FROM docs AS x WHERE lang = 'en' ORDER BY id LIMIT 3")
            .expect("lex");
        assert_eq!(n.as_ref(), p.as_slice());
    }

    #[test]
    fn mismatch_is_undefined_table_and_original_name_hidden_by_alias() {
        assert_eq!(code("SELECT docs.id FROM docs AS x LIMIT 1"), "42P01");
        assert_eq!(code("SELECT id FROM docs ORDER BY zzz.id LIMIT 1"), "42P01");
    }

    #[test]
    fn outer_qualifier_inside_subquery_is_unsupported_inner_wins() {
        assert_eq!(
            code("SELECT id FROM docs WHERE lang IN (SELECT lang FROM langs WHERE docs.id = 1) LIMIT 1"),
            "42601"
        );
        assert_eq!(
            code("SELECT id FROM docs WHERE lang IN (SELECT lang FROM langs WHERE langs.id = 1) LIMIT 1"),
            "ok"
        );
        // 内側と外側が同名なら内側を優先する。
        assert_eq!(
            code("SELECT id FROM docs WHERE lang IN (SELECT lang FROM docs WHERE docs.id = 1) LIMIT 1"),
            "ok"
        );
    }

    #[test]
    fn non_column_positions_are_left_alone() {
        assert!(has_qualified("SELECT x.lower(lang) FROM docs x LIMIT 1"));
        assert!(has_qualified("SELECT id AS x.y FROM docs x LIMIT 1"));
        assert!(has_qualified(
            "SELECT id FROM docs x WHERE a::x.y = 1 LIMIT 1"
        ));
    }

    #[test]
    fn opaque_scopes_are_not_rewritten() {
        // スキーマ修飾 FROM・JOIN は従来のパーサー判定に委ねる。
        assert!(has_qualified("SELECT x.id FROM s.docs LIMIT 1"));
        assert!(has_qualified(
            "SELECT x.id FROM docs x JOIN langs y ON x.id = y.id LIMIT 1"
        ));
        assert!(has_qualified("SELECT x.id FROM docs x, langs y LIMIT 1"));
        assert!(has_qualified(
            "SELECT * FROM docs OUTER JOIN langs ON docs.id = langs.id LIMIT 1"
        ));
    }

    #[test]
    fn bool_word_columns_stay_columns_in_where_leaf() {
        for w in ["true", "false"] {
            let toks =
                tokenize(&format!("SELECT id FROM docs x WHERE x.{w} LIMIT 1")).expect("lex");
            let n = normalize_single_table_qualifiers(&toks).expect("ok");
            assert!(
                n.iter()
                    .any(|t| matches!(t, Token::QuotedBoolWord(s) if s == w)),
                "{w}"
            );
        }
        // 葉以外（比較右辺・投影・ORDER BY）は誤解釈を避けるため拒否する。
        for sql in [
            "SELECT id FROM docs x WHERE flag = x.true LIMIT 10",
            "SELECT id FROM docs x WHERE flag = x.false LIMIT 10",
            "SELECT x.true FROM docs x LIMIT 1",
            "SELECT id FROM docs x ORDER BY x.false LIMIT 1",
        ] {
            let toks = tokenize(sql).expect("lex");
            assert!(normalize_single_table_qualifiers(&toks).is_err(), "{sql}");
        }
    }

    #[test]
    fn qualified_syntax_word_columns_are_rejected() {
        for sql in [
            "SELECT x.distinct id FROM docs x LIMIT 10",
            "SELECT id FROM docs x WHERE x.not TRUE LIMIT 10",
            "SELECT id FROM docs x WHERE x.not = 1 LIMIT 10",
            "SELECT x.all FROM docs x LIMIT 10",
        ] {
            let toks = tokenize(sql).expect("lex");
            assert!(normalize_single_table_qualifiers(&toks).is_err(), "{sql}");
        }
    }

    #[test]
    fn qualified_null_column_is_rejected() {
        for sql in [
            "SELECT COALESCE(x.null, 1) FROM docs x LIMIT 10",
            "SELECT id FROM docs x WHERE x.null = 1 LIMIT 10",
        ] {
            let toks = tokenize(sql).expect("lex");
            assert!(normalize_single_table_qualifiers(&toks).is_err(), "{sql}");
        }
    }

    #[test]
    fn deep_nesting_does_not_walk_ancestors() {
        let n = MAX_SUBQUERY_DEPTH + 50;
        let sql = format!(
            "{}SELECT x.id FROM docs x{} LIMIT 1",
            "SELECT id FROM docs WHERE id IN (".repeat(n),
            ")".repeat(n)
        );
        let toks = tokenize(&sql).expect("lex");
        let _ = normalize_single_table_qualifiers(&toks);
    }

    #[test]
    fn set_operator_words_as_names_do_not_make_scope_opaque() {
        for w in ["union", "intersect", "except"] {
            assert_eq!(code(&format!("SELECT x.id FROM {w} x LIMIT 10")), "ok");
            assert_eq!(
                code(&format!("SELECT x.id FROM docs x WHERE {w} = 1 LIMIT 10")),
                "ok"
            );
            assert!(!has_qualified(&format!(
                "SELECT x.id FROM docs x WHERE {w} = 1 LIMIT 10"
            )));
        }
        assert!(has_qualified(
            "SELECT x.id FROM docs x UNION SELECT id FROM docs LIMIT 1"
        ));
    }

    #[test]
    fn tokens_after_limit_are_not_rewritten() {
        assert!(has_qualified("SELECT id FROM docs x LIMIT 1 OFFSET x.y"));
    }
}
