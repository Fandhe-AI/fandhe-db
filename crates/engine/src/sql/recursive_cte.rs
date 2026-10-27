//! `WITH RECURSIVE` の構文受理と再帰構造の検証（段階 A。SQL-35、Issue #1869）。
//!
//! 責務境界: `sql::allowlist::validate_sql_tokens_impl` の `WITH` 分岐が、先頭の
//! `RECURSIVE` を検出した時点で本モジュールの [`validate_recursive_with`] へ分岐する
//! （非再帰経路の `sql::cte::resolve_relation` へは一切流さない。流すと自己参照が同名の
//! 実テーブルへ黙って解決されるため）。本モジュールは次の 2 つだけを担う。
//!
//! 1. 構文の受理: アンカー項（非再帰項）と再帰項を `UNION [ALL]` で結んだ形の切り出し
//! 2. 再帰構造の検証: 自己参照の位置・回数（SQL-35 (2)・ERR-6 の `42P19`）
//!
//! 反復評価は #1870 の担当で、本モジュールを通過した文も呼び出し元が一律 `0A000` で終える。
//! 判定順序は ①構文（`42601`）→ ②再帰構造（`42P19`）→ ③機能ゲート（`0A000`）。構造検証は
//! カタログを照会しない [`StructuralOnlyLookup`] で行うため、同名の実テーブルや他テナントの
//! 表の有無で wire_code が変わらない（存在オラクルを作らない。RLS-10 (b)）。
//!
//! [`RecursiveWithClause`] が保持するのはトークン列だけで、テナント情報・`PolicyContext`・
//! 検証済み `Statement` は持たない（段階 B が実カタログで再検証する）。

use super::allowlist::{
    is_set_operator_ident, set_operator_is_followed_by_branch, split_parenthesized,
    split_recursive_with_clause, validate_data_modifying_cte_syntax,
    validate_sql_tokens_with_subquery_ctx_limits, JoinKind, SqlSurfaceError, Statement,
    StructuralOnlyLookup,
};
use super::lexer::{Keyword, Token};
use crate::nesting_limits::NestingLimits;

/// 検証済みの `WITH RECURSIVE` 句（トークン列のみ保持。#1870 の反復評価器が消費する）。
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // #1870（反復評価器）が消費する
pub(crate) struct RecursiveWithClause {
    pub(crate) defs: Vec<RecursiveCteDef>,
    pub(crate) main_tokens: Vec<Token>,
}

/// 1 件の CTE 定義（`name [(列名...)] AS (本文)`）。
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // #1870（反復評価器）が消費する
pub(crate) struct RecursiveCteDef {
    pub(crate) name: String,
    pub(crate) column_aliases: Vec<String>,
    pub(crate) body: RecursiveCteBody,
}

/// CTE 本文の種別。
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // #1870（反復評価器）が消費する
pub(crate) enum RecursiveCteBody {
    /// 自己参照を持たない本文。
    NonRecursive { tokens: Vec<Token> },
    /// 自己参照を持つ本文（アンカー項 `UNION [ALL]` 再帰項）。
    Recursive {
        anchor: Vec<Token>,
        recursive: Vec<Token>,
        union_all: bool,
    },
}

/// 関係名の参照 1 件（`FROM`／`JOIN`／FROM リストの `,` の直後の位置）。
struct RelRef {
    in_subquery: bool,
}

/// 括弧ごとの走査状態。
struct Frame {
    subq: bool,
    in_from: bool,
    expect_rel: bool,
}

fn is_word(t: Option<&Token>, w: &str) -> bool {
    matches!(t, Some(Token::Ident(n)) if n.eq_ignore_ascii_case(w))
}

/// `tokens` 中の関係名 `name`（大文字小文字不問）への参照を数える。修飾列 `r.n`・関数呼び出し
/// `r(`・スキーマ修飾の一部は除外する。部分クエリ（`(SELECT`／`(WITH`／`(VALUES`）の内側かどうかも返す。
fn relation_refs(tokens: &[Token], name: &str) -> Vec<RelRef> {
    let mut out = Vec::new();
    let mut stack = vec![Frame {
        subq: false,
        in_from: false,
        expect_rel: false,
    }];
    for (i, t) in tokens.iter().enumerate() {
        let next = tokens.get(i.saturating_add(1));
        match t {
            Token::Punct('(') => {
                let is_sub = matches!(next, Some(Token::Keyword(Keyword::Select)))
                    || is_word(next, "WITH")
                    || is_word(next, "VALUES");
                let (psub, pexpect) = match stack.last_mut() {
                    Some(f) => {
                        let e = f.expect_rel;
                        f.expect_rel = false;
                        (f.subq, e)
                    }
                    None => (false, false),
                };
                let group = pexpect && !is_sub;
                stack.push(Frame {
                    subq: psub || is_sub,
                    in_from: group,
                    expect_rel: group,
                });
            }
            Token::Punct(')') => {
                if stack.len() > 1 {
                    stack.pop();
                }
            }
            Token::Punct(',') => {
                if let Some(f) = stack.last_mut() {
                    if f.in_from {
                        f.expect_rel = true;
                    }
                }
            }
            Token::Keyword(Keyword::From) => {
                if let Some(f) = stack.last_mut() {
                    f.in_from = true;
                    f.expect_rel = true;
                }
            }
            Token::Keyword(Keyword::Where | Keyword::Order | Keyword::Limit) => {
                if let Some(f) = stack.last_mut() {
                    f.in_from = false;
                    f.expect_rel = false;
                }
            }
            Token::Ident(w) => {
                if let Some(f) = stack.last_mut() {
                    if f.expect_rel {
                        if (w.eq_ignore_ascii_case("LATERAL") || w.eq_ignore_ascii_case("ONLY"))
                            && starts_relation(next)
                        {
                            continue;
                        }
                        if w.eq_ignore_ascii_case(name)
                            && !matches!(next, Some(Token::Punct('.' | '(')))
                        {
                            out.push(RelRef {
                                in_subquery: f.subq,
                            });
                        }
                        f.expect_rel = false;
                    } else if w.eq_ignore_ascii_case("JOIN")
                        && f.in_from
                        && is_join_operator_position(tokens, i)
                    {
                        f.in_from = true;
                        f.expect_rel = true;
                    } else if [
                        "GROUP",
                        "HAVING",
                        "UNION",
                        "INTERSECT",
                        "EXCEPT",
                        "OFFSET",
                        "FETCH",
                        "WINDOW",
                    ]
                    .iter()
                    .any(|k| w.eq_ignore_ascii_case(k))
                    {
                        f.in_from = false;
                    }
                }
            }
            _ => {
                if let Some(f) = stack.last_mut() {
                    f.expect_rel = false;
                }
            }
        }
    }
    out
}

/// 式・別名を導入する語。直後の `JOIN` は結合演算子ではなく列名・別名として読む
/// （`NULL`／`TRUE`／`FALSE` は式を完結させる語なので含めない）。
const JOIN_NOT_AFTER: &[&str] = &[
    "AS", "ON", "AND", "OR", "NOT", "IN", "IS", "LIKE", "ILIKE", "BETWEEN", "USING", "CASE",
    "WHEN", "THEN", "ELSE", "BY",
];

/// 関係の直後に続いて FROM 項目を閉じる／次の句を始める語。`ONLY`／`LATERAL` の直後にこれらが
/// 来た場合、その `ONLY`／`LATERAL` は修飾語でなく関係名（引用符付き `"only"` 等）として読む。
const NOT_RELATION_START: &[&str] = &[
    "AS",
    "ON",
    "USING",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "NATURAL",
    "GROUP",
    "HAVING",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "OFFSET",
    "FETCH",
    "WINDOW",
    "WHERE",
];

/// `ONLY`／`LATERAL` の直後のトークン `next` が関係の開始（識別子・`(`）かを判定する。lexer は
/// 引用符付き識別子と未引用語を区別しないため、修飾語として読めるのは関係が続く場合に限り、
/// 続かない `FROM "only"` は関係名として数える（`WITH RECURSIVE "only"` の自己参照を取りこぼさない）。
fn starts_relation(next: Option<&Token>) -> bool {
    match next {
        Some(Token::Ident(n)) => !NOT_RELATION_START.iter().any(|k| n.eq_ignore_ascii_case(k)),
        Some(Token::Punct('(')) => true,
        _ => false,
    }
}

/// 直前のトークンが式・関係参照を完結させているか（`JOIN` を演算子と読める文法状態）。
/// 識別子（式を導入する語を除く）・数値・文字列・パラメータ・`)`・`]` のあとは項が完結しており、
/// 演算子・`AS`・`ON` などのあとは項を待つ位置なので `JOIN` は列名・別名になる。
fn ends_operand(prev: Option<&Token>) -> bool {
    match prev {
        Some(Token::Ident(p)) => !JOIN_NOT_AFTER.iter().any(|k| p.eq_ignore_ascii_case(k)),
        Some(
            Token::QualifiedIdent { .. }
            | Token::QuotedNull(_)
            | Token::QuotedBoolWord(_)
            | Token::Param(_)
            | Token::Number(_)
            | Token::StringLiteral(_)
            | Token::QuotedCollationName(_)
            | Token::Punct(')' | ']'),
        ) => true,
        _ => false,
    }
}

/// `tokens[i]`（識別子 `JOIN`）が結合演算子の位置にあるかを判定する。lexer は引用符付き識別子も
/// 未引用語も同じ `Token::Ident` にするため、文法状態で区別する。結合の `JOIN` は、直前で項が
/// 完結し（[`ends_operand`]）、直後に関係名（識別子・`(`）が続く位置だけ。`AS "join"` の別名や
/// `= "join"`・`ON join` の列参照は演算子とみなさない（`relation_refs` の構造判定用）。
fn is_join_operator_position(tokens: &[Token], i: usize) -> bool {
    ends_operand(i.checked_sub(1).and_then(|p| tokens.get(p)))
        && matches!(
            tokens.get(i.saturating_add(1)),
            Some(Token::Ident(_) | Token::Punct('('))
        )
}

/// `tokens[i]`（識別子 `w`）が列名・表名ではなく演算子・句として使われているかを文脈で判定する。
/// `UNION`／`INTERSECT`／`EXCEPT` は既存の集合演算ヘルパーと同じく直後に `SELECT` 枝が続く
/// 場合のみ、`OFFSET`／`FETCH` は直前が式の末尾（識別子・数値・文字列・`)`）で直後が
/// `FROM`・`,`・比較演算子などでない場合のみ句とみなす（`SELECT offset FROM t` 等の誤分類防止）。
fn is_operator_position(tokens: &[Token], i: usize, w: &str) -> bool {
    if is_set_operator_ident(w) {
        return set_operator_is_followed_by_branch(tokens, i);
    }
    if w.eq_ignore_ascii_case("OFFSET") || w.eq_ignore_ascii_case("FETCH") {
        // `AS offset` / `AS fetch` は別名であり句ではない。
        if i.checked_sub(1)
            .is_some_and(|p| is_word(tokens.get(p), "AS"))
        {
            return false;
        }
        let prev_ends_expr = i
            .checked_sub(1)
            .and_then(|p| tokens.get(p))
            .is_some_and(|t| {
                matches!(
                    t,
                    Token::Ident(_)
                        | Token::QualifiedIdent { .. }
                        | Token::QuotedNull(_)
                        | Token::QuotedBoolWord(_)
                        | Token::Param(_)
                        | Token::Number(_)
                        | Token::StringLiteral(_)
                        | Token::QuotedCollationName(_)
                        | Token::Punct(')' | ']')
                )
            });
        let next_is_operand = !matches!(
            tokens.get(i.saturating_add(1)),
            Some(Token::Keyword(_) | Token::Punct(',' | '=' | '>' | '<' | '*' | '/' | '%' | ')'))
        ) || matches!(
            tokens.get(i.saturating_add(1)),
            Some(Token::Keyword(Keyword::Limit | Keyword::Order))
        );
        return prev_ends_expr && next_is_operand;
    }
    true
}

/// 括弧の深さ 0 にある識別子トークンのうち、`words` のいずれかに一致する位置を返す。
fn depth0_words(tokens: &[Token], words: &[&str]) -> Vec<usize> {
    let mut depth: usize = 0;
    let mut out = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            Token::Ident(w)
                if depth == 0
                    && words.iter().any(|k| w.eq_ignore_ascii_case(k))
                    && is_operator_position(tokens, i, w) =>
            {
                out.push(i);
            }
            Token::Keyword(Keyword::Order | Keyword::Limit)
                if depth == 0 && (words.contains(&"ORDER") || words.contains(&"LIMIT")) =>
            {
                out.push(i);
            }
            _ => {}
        }
    }
    out
}

/// 再帰項の外側を囲む括弧を繰り返し剥がす。戻り値は（内側, 括弧を剥がしたか, 本文全体に掛かる
/// 後続句の有無, 括弧内に属する後続句の有無）。括弧の後ろに続く `ORDER BY`／`LIMIT`／`OFFSET`／
/// `FETCH` は捨てず、括弧内側と連結して構文を検証する（`42601`）。最外の後続句は本文全体の句
/// （機能ゲート `0A000`）、さらに外側の括弧に包まれた後続句は再帰項自身の句（`42P19`）として
/// 区別する（SQL-35 の判定順序: 構文 → 構造 → 機能ゲート）。
fn peel_term(
    mut tokens: &[Token],
    limits: NestingLimits,
) -> Result<(&[Token], bool, bool, bool), SqlSurfaceError> {
    let mut wrapped = false;
    let mut trailing = false;
    let mut inner_clause = false;
    let mut tails: Vec<&[Token]> = Vec::new();
    while matches!(tokens.first(), Some(Token::Punct('('))) {
        let Ok((inner, after)) = split_parenthesized(tokens) else {
            break;
        };
        let tail_clause = matches!(
            after.first(),
            Some(Token::Keyword(Keyword::Order | Keyword::Limit))
        ) || is_word(after.first(), "OFFSET")
            || is_word(after.first(), "FETCH");
        if after.is_empty() || tail_clause {
            if tail_clause {
                tails.push(after);
                if wrapped {
                    inner_clause = true;
                } else {
                    trailing = true;
                }
            }
            wrapped = true;
            tokens = inner;
        } else {
            break;
        }
    }
    // 後続句は最内の項と連結して構文だけを検証する（値の欠落などは `42601`）。
    for tail in tails {
        let mut joined = tokens.to_vec();
        joined.extend_from_slice(tail);
        structural(&joined, limits)?;
    }
    Ok((tokens, wrapped, trailing, inner_clause))
}

/// カタログ非依存の構文検証。構造判定（`42P19`）へ進む前に各項で必ず通す。既存パーサーは
/// `IN`／`EXISTS`／スカラー比較などのサブクエリ内部をトークン列のまま保存して検証を後段へ委ねるため、
/// 保存された内部も [`validate_nested_subqueries`] で再帰的に構文検証する
/// （構文 `42601` → 構造 `42P19` → 機能ゲート `0A000` の順序契約）。
fn structural(tokens: &[Token], limits: NestingLimits) -> Result<Statement, SqlSurfaceError> {
    let stmt =
        validate_sql_tokens_with_subquery_ctx_limits(tokens, &StructuralOnlyLookup, 0, limits)?;
    validate_nested_subqueries(tokens, 0, limits)?;
    Ok(stmt)
}

/// `tokens`（深さ `depth` の文）の中にある `( SELECT ...)` の括弧群を、それぞれ深さ `depth + 1` の
/// 文としてカタログ非依存で構文検証し、その内側へ再帰する。深さ上限は `limits` を引き継ぎ、
/// 超過は `54000`。括弧が閉じない場合は呼び出し元の検証が先に `42601` で拒否済みのため打ち切る。
fn validate_nested_subqueries(
    tokens: &[Token],
    depth: usize,
    limits: NestingLimits,
) -> Result<(), SqlSurfaceError> {
    let mut i = 0usize;
    while let Some(t) = tokens.get(i) {
        let is_subquery_open = matches!(t, Token::Punct('('))
            && matches!(
                tokens.get(i.saturating_add(1)),
                Some(Token::Keyword(Keyword::Select))
            );
        if !is_subquery_open {
            i = i.saturating_add(1);
            continue;
        }
        let Some(rest) = tokens.get(i..) else { break };
        let Ok((inner, after)) = split_parenthesized(rest) else {
            break;
        };
        let inner_depth = depth.saturating_add(1);
        validate_sql_tokens_with_subquery_ctx_limits(
            inner,
            &StructuralOnlyLookup,
            inner_depth,
            limits,
        )?;
        validate_nested_subqueries(inner, inner_depth, limits)?;
        i = tokens.len().saturating_sub(after.len());
    }
    Ok(())
}

/// `WITH RECURSIVE` 文の構文を受理し再帰構造を検証する（SQL-35、Issue #1869）。
/// 違反は `42601`（構文）・`42P19`（不正な再帰）・`0A000`（認識できるが未対応）・
/// `54000`（定義数の上限）で fail-closed に返す。成功時も評価は行わない。
pub(crate) fn validate_recursive_with(
    tokens: &[Token],
    limits: NestingLimits,
) -> Result<RecursiveWithClause, SqlSurfaceError> {
    let (raw_defs, main_tokens, has_search_cycle) = split_recursive_with_clause(tokens, limits)?;

    if !matches!(main_tokens.first(), Some(Token::Keyword(Keyword::Select))) {
        return Err(SqlSurfaceError::unsupported(
            "WITH must be followed by a SELECT statement",
        ));
    }
    structural(&main_tokens, limits)?;

    // 機能ゲート（0A000）は検出結果を保持し、全定義の構文・再帰構造の検証が終わってから返す。
    let mut deferred: Option<SqlSurfaceError> = None;
    if has_search_cycle {
        deferred = Some(SqlSurfaceError::feature_not_supported(
            "SEARCH and CYCLE clauses of WITH RECURSIVE are not supported yet",
        ));
    }
    // 再帰構造エラー（42P19）は保留し、後続定義の構文検証（42601）を優先させる。
    let mut structural_err: Option<SqlSurfaceError> = None;
    let mut defs = Vec::with_capacity(raw_defs.len());
    for (idx, (name, column_aliases, body)) in raw_defs.iter().enumerate() {
        let first = body.first();
        // データ変更本文・入れ子の WITH は評価できない（機能ゲート 0A000）が、ゲートは構文（42601）・
        // 再帰構造（42P19）の確定後に返す順序契約のため、本文の構文を検証してから保留する。
        let skipped_gate: Option<&'static str> = if matches!(first, Some(Token::Ident(w))
            if ["INSERT", "UPDATE", "DELETE"].iter().any(|k| w.eq_ignore_ascii_case(k)))
        {
            Some("data-modifying statements in WITH RECURSIVE are not supported")
        } else if is_word(first, "WITH") {
            Some("nested WITH inside WITH RECURSIVE is not supported")
        } else {
            None
        };
        if let Some(gate) = skipped_gate {
            // 入れ子の WITH は、既存の通常 WITH 経路が定数 SELECT の主クエリ等の妥当な形を `42601` で
            // 拒否するため（挙動を `0A000` から変えてしまう）、構文検証せず機能ゲートへ保留する。
            let checked = if is_word(first, "WITH") {
                Ok(())
            } else {
                validate_data_modifying_cte_syntax(body, limits).map(|_| ())
            };
            match checked {
                Ok(()) => {
                    deferred.get_or_insert_with(|| SqlSurfaceError::feature_not_supported(gate));
                }
                Err(e) if e.wire_code() == "42P19" => {
                    structural_err.get_or_insert(e);
                }
                Err(e) if e.wire_code() == "0A000" => {
                    deferred.get_or_insert(e);
                }
                Err(e) => return Err(e),
            }
            continue;
        }
        for (_, (later, _, _)) in raw_defs.iter().enumerate().skip(idx.saturating_add(1)) {
            if !relation_refs(body, later).is_empty() {
                deferred.get_or_insert_with(|| {
                    SqlSurfaceError::feature_not_supported(
                        "forward references between WITH RECURSIVE queries are not supported",
                    )
                });
            }
        }
        let refs = relation_refs(body, name);
        let checked = if refs.is_empty() {
            structural(body, limits).map(|_| RecursiveCteBody::NonRecursive {
                tokens: body.clone(),
            })
        } else {
            validate_recursive_body(name, body, limits, &mut deferred)
        };
        // 構文エラー（42601 等）は即返し、42P19 と 0A000 は保留して優先順位（構文 → 42P19 → 機能ゲート）を保つ。
        let kind = match checked {
            Ok(k) => k,
            Err(e) if e.wire_code() == "42P19" => {
                structural_err.get_or_insert(e);
                continue;
            }
            Err(e) if e.wire_code() == "0A000" => {
                deferred.get_or_insert(e);
                continue;
            }
            Err(e) => return Err(e),
        };
        defs.push(RecursiveCteDef {
            name: name.clone(),
            column_aliases: column_aliases.clone(),
            body: kind,
        });
    }
    if let Some(e) = structural_err {
        return Err(e);
    }
    if let Some(e) = deferred {
        return Err(e);
    }
    Ok(RecursiveWithClause { defs, main_tokens })
}

/// 自己参照を持つ本文を、アンカー項と再帰項へ分割して検証する。
fn validate_recursive_body(
    name: &str,
    body: &[Token],
    limits: NestingLimits,
    deferred: &mut Option<SqlSurfaceError>,
) -> Result<RecursiveCteBody, SqlSurfaceError> {
    let bad = |reason: &'static str| SqlSurfaceError::invalid_recursion(name, reason);

    let unions = depth0_words(body, &["UNION"]);
    let Some(&u) = unions.last() else {
        // 構文不正は構造エラーより優先して `42601` にする。
        structural(body, limits)?;
        return Err(bad(
            "recursive query must be UNION [ALL] of non-recursive and recursive terms",
        ));
    };
    let union_all = is_word(body.get(u.saturating_add(1)), "ALL");
    let right_start = u.saturating_add(if union_all { 2 } else { 1 });
    let anchor = body.get(..u).unwrap_or(&[]);
    let recursive = body.get(right_start..).unwrap_or(&[]);
    if anchor.is_empty() || recursive.is_empty() {
        return Err(SqlSurfaceError::unsupported(
            "expected a query on both sides of UNION in WITH RECURSIVE",
        ));
    }

    // 判定順序は構文 → 再帰構造 → 機能ゲート。構造判定の前に各項の構文を検証する。
    structural(anchor, limits)?;
    let (term, wrapped, trailing, inner_clause) = peel_term(recursive, limits)?;
    // 末尾句（本文全体に掛かる句。0A000 は後回し）は、FROM を持つ有効な select に連結して
    // 構文だけを検証する（`ORDER BY`／`FETCH` を受理しない定数 select へ連結しない）。
    // 構造検査は句を除いた項で行う（集計項などに句が付いても 42P19 を優先して返すため）。
    let core_len = depth0_words(term, &["ORDER", "LIMIT", "OFFSET", "FETCH"])
        .first()
        .copied()
        .unwrap_or(term.len());
    if let Some(tail) = term.get(core_len..).filter(|t| !t.is_empty()) {
        let mut probe = vec![
            Token::Keyword(Keyword::Select),
            Token::Ident("n".into()),
            Token::Keyword(Keyword::From),
            Token::Ident("docs".into()),
        ];
        probe.extend_from_slice(tail);
        structural(&probe, limits)?;
    }
    let stmt = structural(term.get(..core_len).unwrap_or(term), limits)?;

    if !relation_refs(anchor, name).is_empty() {
        return Err(bad(
            "recursive reference must not appear in the non-recursive term",
        ));
    }
    let rec_refs = relation_refs(term, name);
    if rec_refs.iter().any(|r| r.in_subquery) {
        return Err(bad("recursive reference must not appear within a subquery"));
    }
    if rec_refs.len() != 1 {
        return Err(bad(
            "recursive reference must appear exactly once in the recursive term",
        ));
    }
    if !depth0_words(term, &["INTERSECT", "EXCEPT"]).is_empty() {
        return Err(bad(
            "recursive reference must not appear within INTERSECT or EXCEPT",
        ));
    }
    let clauses = !depth0_words(term, &["ORDER", "LIMIT", "OFFSET", "FETCH"]).is_empty();
    // 括弧で囲んだ再帰項の内側の句は項自身の句（`OFFSET 0` も句の存在で判定）で `42P19`。
    if inner_clause || (wrapped && clauses) {
        return Err(bad(
            "LIMIT, OFFSET and ORDER BY are not allowed in a recursive term",
        ));
    }
    // 括弧の無い再帰項の末尾、または括弧の後ろに続く ORDER BY／LIMIT／OFFSET／FETCH は
    // 本文全体に掛かる句で、PostgreSQL も未実装（0A000）。
    if trailing || clauses {
        // 再帰項の集計・ウィンドウ違反（42P19）を優先するため、返却は検証完了後に回す。
        deferred.get_or_insert_with(|| {
            SqlSurfaceError::feature_not_supported(
                "ORDER BY, LIMIT, OFFSET and FETCH on WITH RECURSIVE queries are not supported",
            )
        });
    }

    check_recursive_term(name, &stmt)?;

    Ok(RecursiveCteBody::Recursive {
        anchor: anchor.to_vec(),
        recursive: recursive.to_vec(),
        union_all,
    })
}

/// 検証済みの再帰項の形から、集計・ウィンドウ関数・`LIMIT`／`OFFSET`／`ORDER BY`・
/// 外部結合の NULL 側の自己参照を拒否する（`42P19`）。
fn check_recursive_term(name: &str, stmt: &Statement) -> Result<(), SqlSurfaceError> {
    let bad = |reason: &'static str| SqlSurfaceError::invalid_recursion(name, reason);
    match stmt {
        Statement::Scan(v) => {
            if !v.window_items.is_empty() {
                return Err(bad("window functions are not allowed in a recursive term"));
            }
            if !v.limit_implicit || v.offset > 0 || !v.order_by.is_empty() {
                return Err(bad(
                    "LIMIT, OFFSET and ORDER BY are not allowed in a recursive term",
                ));
            }
            Ok(())
        }
        Statement::Aggregate(_) => Err(bad(
            "aggregate functions are not allowed in a recursive term",
        )),
        Statement::Join(j) => {
            if j.aggregate.is_some() {
                return Err(bad(
                    "aggregate functions are not allowed in a recursive term",
                ));
            }
            if j.limit.is_some() || j.offset > 0 || !j.order_by.is_empty() {
                return Err(bad(
                    "LIMIT, OFFSET and ORDER BY are not allowed in a recursive term",
                ));
            }
            let Some(k) = j
                .relations
                .iter()
                .position(|r| r.table().eq_ignore_ascii_case(name))
            else {
                return Ok(());
            };
            for (i, step) in j.steps.iter().enumerate() {
                let joined = i.saturating_add(1);
                let null_side = match step.kind {
                    JoinKind::Inner => false,
                    JoinKind::Left => k == joined,
                    JoinKind::Right => k <= i,
                    JoinKind::Full => k <= joined,
                };
                if null_side {
                    return Err(bad(
                        "recursive reference must not appear on the nullable side of an outer join",
                    ));
                }
            }
            Ok(())
        }
        Statement::SetOperation(_) => Err(bad(
            "recursive reference must not appear within a set operation of the recursive term",
        )),
        Statement::ConstSelect(_) => Err(bad(
            "recursive reference must appear exactly once in the recursive term",
        )),
        _ => Err(SqlSurfaceError::unsupported(
            "unsupported form of the recursive term in WITH RECURSIVE",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::allowlist::validate_sql;

    fn code(sql: &str) -> &'static str {
        match validate_sql(sql, &StructuralOnlyLookup) {
            Ok(_) => "ok",
            Err(e) => e.wire_code(),
        }
    }

    const NOT_YET: &str = "0A000";

    #[test]
    fn accepted_forms_end_with_evaluation_not_supported() {
        for sql in [
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 10) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION SELECT n + 1 FROM t WHERE n < 10) SELECT n FROM t",
            "WITH RECURSIVE r AS (SELECT id, mgr FROM emp WHERE mgr = 1 UNION ALL SELECT e.id, e.mgr FROM emp e JOIN r ON e.mgr = r.id) SELECT id FROM r",
            "WITH RECURSIVE t(n) AS MATERIALIZED (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL (SELECT n + 1 FROM t WHERE n < 3)) SELECT n FROM t",
            "WITH RECURSIVE a AS (SELECT id FROM docs), t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) SELECT n FROM t",
            "WITH RECURSIVE x AS (SELECT id FROM docs) SELECT id FROM x",
        ] {
            assert_eq!(code(sql), NOT_YET, "{sql}");
        }
    }

    #[test]
    fn invalid_recursion_is_42p19() {
        for sql in [
            "WITH RECURSIVE t AS (SELECT n FROM t UNION ALL SELECT n FROM docs) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT a.n FROM t a JOIN t b ON a.n = b.n) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n FROM docs WHERE n IN (SELECT n FROM t)) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT d.n FROM docs d LEFT JOIN t ON d.n = t.n) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT d.n FROM t RIGHT JOIN docs d ON d.n = t.n) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT d.n FROM docs d FULL JOIN t ON d.n = t.n) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT count(*) FROM t) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL (SELECT n FROM t LIMIT 1)) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL (SELECT n FROM t OFFSET 0)) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL ((SELECT n FROM t) LIMIT 1)) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL (SELECT d.n FROM docs d JOIN t ON d.n = t.n OFFSET 0)) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT n FROM t) SELECT n FROM t",
        ] {
            assert_eq!(code(sql), "42P19", "{sql}");
        }
    }

    #[test]
    fn subquery_inner_syntax_is_checked_before_recursion_structure() {
        for sql in [
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n FROM docs WHERE n IN (SELECT FROM t)) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n FROM docs WHERE n IN (SELECT n FROM docs WHERE n IN (SELECT FROM docs))) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n FROM t) SELECT n FROM docs WHERE n IN (SELECT FROM docs)",
            "WITH RECURSIVE t(n) AS (SELECT n FROM docs WHERE n IN (SELECT FROM docs) UNION ALL SELECT n FROM t) SELECT n FROM t",
        ] {
            assert_eq!(code(sql), "42601", "{sql}");
        }
        // 構文が正しいサブクエリ内の自己参照は従来どおり 42P19。
        assert_eq!(
            code("WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n FROM docs WHERE n IN (SELECT n FROM t)) SELECT n FROM t"),
            "42P19"
        );
    }

    #[test]
    fn subquery_nesting_limit_is_inherited() {
        let mut sql = String::from("SELECT n FROM docs");
        for _ in 0..40 {
            sql = format!("SELECT n FROM docs WHERE n IN ({sql})");
        }
        let sql = format!(
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n FROM t) SELECT n FROM docs WHERE n IN ({sql})"
        );
        assert_eq!(code(&sql), "54000");
    }

    #[test]
    fn join_named_column_is_not_a_join_operator() {
        // 引用符付きの列 "join" が後続の識別子 `t` を関係位置へ押し上げて自己参照を捏造しない。
        assert_eq!(
            code("WITH RECURSIVE t(n) AS (SELECT \"join\", t FROM docs UNION ALL SELECT n, 1 FROM t) SELECT n FROM t"),
            NOT_YET
        );
    }

    #[test]
    fn join_named_alias_does_not_hide_real_join() {
        let count = |sql: &str| {
            let tokens = crate::sql::lexer::tokenize(sql).expect("tokenize");
            relation_refs(&tokens, "t").len()
        };
        // 式中の "join" が後続の本物の JOIN を関係位置として消費して t を取りこぼさない。
        assert_eq!(
            count("SELECT d.n FROM docs d JOIN u ON d.n = \"join\" JOIN t ON t.n = d.n"),
            1
        );
        assert_eq!(
            count("SELECT d.n FROM docs AS \"join\" JOIN t ON t.n = 1"),
            1
        );
        // 通常の結合は従来どおり数える。
        assert_eq!(
            count("SELECT d.n FROM docs d INNER JOIN t ON t.n = d.n LEFT JOIN u ON u.n = 1"),
            1
        );
    }

    #[test]
    fn cte_named_only_or_lateral_is_a_relation_name() {
        for sql in [
            "WITH RECURSIVE \"only\" AS (SELECT n FROM \"only\") SELECT n FROM \"only\"",
            "WITH RECURSIVE \"lateral\" AS (SELECT n FROM \"lateral\") SELECT n FROM \"lateral\"",
        ] {
            assert_eq!(code(sql), "42P19", "{sql}");
        }
        // 修飾語としての ONLY／LATERAL は従来どおり読み飛ばす。
        let count = |sql: &str, name: &str| {
            let tokens = crate::sql::lexer::tokenize(sql).expect("tokenize");
            relation_refs(&tokens, name).len()
        };
        assert_eq!(count("SELECT n FROM ONLY t", "t"), 1);
        assert_eq!(count("SELECT n FROM docs, LATERAL t", "t"), 1);
        assert_eq!(count("SELECT n FROM \"only\"", "only"), 1);
        assert_eq!(
            count("SELECT n FROM \"only\" JOIN docs ON 1 = 1", "only"),
            1
        );
    }

    #[test]
    fn join_after_literal_param_or_subscript_is_an_operator() {
        let count = |sql: &str| {
            let tokens = crate::sql::lexer::tokenize_with_params(sql).expect("tokenize");
            relation_refs(&tokens, "t").len()
        };
        for tail in ["1", "'a'", "$1", "a[1]", "TRUE", "NULL", "(1)", "d.n"] {
            let sql = format!("SELECT d.n FROM docs d JOIN u ON d.n = {tail} JOIN t ON t.n = 1");
            assert_eq!(count(&sql), 1, "{sql}");
        }
    }

    #[test]
    fn data_modifying_body_syntax_precedes_gate_and_structure() {
        let bad_rec = "t(n) AS (SELECT n FROM t)";
        for sql in [
            "WITH RECURSIVE d AS (DELETE) SELECT 1".to_string(),
            "WITH RECURSIVE d AS (INSERT INTO) SELECT 1".to_string(),
            format!("WITH RECURSIVE {bad_rec}, d AS (DELETE) SELECT 1"),
            format!("WITH RECURSIVE d AS (DELETE), {bad_rec} SELECT 1"),
            format!("WITH RECURSIVE d AS (UPDATE docs SET), {bad_rec} SELECT 1"),
        ] {
            assert_eq!(code(&sql), "42601", "{sql}");
        }
        // 妥当なデータ変更本文は従来どおり機能ゲート 0A000。不正な再帰との併記は 42P19 が先。
        assert_eq!(
            code("WITH RECURSIVE d AS (DELETE FROM docs WHERE id = 1) SELECT 1"),
            NOT_YET
        );
        assert_eq!(
            code("WITH RECURSIVE d AS (DELETE FROM docs WHERE id = 1), t(n) AS (SELECT n FROM t) SELECT 1"),
            "42P19"
        );
    }

    #[test]
    fn search_cycle_clause_syntax_precedes_gate() {
        for sql in [
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) SEARCH garbage SELECT n FROM t".to_string(),
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) SEARCH DEPTH FIRST BY n SELECT n FROM t".to_string(),
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) CYCLE n SELECT n FROM t".to_string(),
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) CYCLE n SET c TO 1 USING p SELECT n FROM t".to_string(),
        ] {
            assert_eq!(code(&sql), "42601", "{sql}");
        }
        for sql in [
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) SEARCH BREADTH FIRST BY n SET o SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) CYCLE n SET c TO 1 DEFAULT 0 USING p SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) SEARCH DEPTH FIRST BY n SET o CYCLE n SET c USING p SELECT n FROM t",
        ] {
            assert_eq!(code(sql), NOT_YET, "{sql}");
        }
    }

    #[test]
    fn unsupported_forms_are_0a000() {
        for sql in [
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3) SEARCH DEPTH FIRST BY n SET o SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) CYCLE n SET c USING p SELECT n FROM t",
            "WITH RECURSIVE t AS (DELETE FROM docs) SELECT 1",
            "WITH RECURSIVE a AS (SELECT id FROM b), b AS (SELECT id FROM docs) SELECT id FROM a",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t LIMIT 5) SELECT n FROM t",
            "WITH RECURSIVE t AS (WITH u AS (SELECT 1) SELECT 1) SELECT 1",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL ((SELECT n + 1 FROM t WHERE n < 3))) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL (SELECT n + 1 FROM t WHERE n < 3) LIMIT 5) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL ((SELECT n + 1 FROM t)) ORDER BY n) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL (SELECT n + 1 FROM t) OFFSET 1) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t ORDER BY n) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < 3 ORDER BY n LIMIT 2) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t FETCH FIRST 1 ROWS ONLY) SELECT n FROM t",
        ] {
            assert_eq!(code(sql), NOT_YET, "{sql}");
        }
    }

    #[test]
    fn structure_and_syntax_errors_take_precedence_over_feature_gate() {
        for (sql, want) in [
            ("WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT count(*) FROM t LIMIT 5) SELECT n FROM t", "42P19"),
            ("WITH RECURSIVE a AS (SELECT FROM b), b AS (SELECT 1) SELECT 1", "42601"),
            ("WITH RECURSIVE d AS (DELETE FROM docs), t AS (SELECT n FROM t) SELECT 1", "42P19"),
            ("WITH RECURSIVE u AS (WITH v AS (SELECT 1) SELECT 1), t AS (SELECT FROM t) SELECT 1", "42601"),
            ("WITH RECURSIVE t(n) AS (SELECT n FROM t), u AS (SELECT FROM docs) SELECT n FROM t", "42601"),
            ("WITH RECURSIVE t(n) AS (SELECT n FROM t) CYCLE n SET c USING p SELECT n FROM t", "42P19"),
            ("WITH RECURSIVE t(n) AS (SELECT n FROM t) SEARCH DEPTH FIRST BY n SET o SELECT n FROM t", "42P19"),
            ("WITH RECURSIVE t(n) AS (SELECT FROM t) CYCLE n SET c USING p SELECT n FROM t", "42601"),
            ("WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t) SEARCH DEPTH FIRST BY n, m SET o, u AS (SELECT FROM docs) SELECT n FROM t", "42601"),
            ("WITH RECURSIVE t(n) AS (SELECT n FROM t), u(m) AS (SELECT 1 UNION ALL SELECT m FROM u LIMIT 3) SELECT n FROM t", "42P19"),
            ("WITH RECURSIVE t(n) AS (SELECT n FROM t), u AS (DELETE FROM docs), w AS (WITH v AS (SELECT 1) SELECT 1) SELECT n FROM t", "42P19"),
        ] {
            assert_eq!(code(sql), want, "{sql}");
        }
    }

    #[test]
    fn offset_after_subscript_is_a_trailing_clause() {
        let sql = "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM t WHERE n < d[1] OFFSET 1) SELECT n FROM t";
        assert_eq!(code(sql), NOT_YET, "{sql}");
    }

    #[test]
    fn syntax_errors_are_42601() {
        for sql in [
            "WITH RECURSIVE t(n) (SELECT 1) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1; ) SELECT n FROM t",
            "WITH RECURSIVE t AS (SELECT FROM t) SELECT 1",
            "WITH RECURSIVE t(n) AS (SELECT FROM t UNION ALL SELECT n FROM t) SELECT 1",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT FROM t) SELECT 1",
            "WITH RECURSIVE t AS (SELECT 1), t AS (SELECT 2) SELECT 1",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL (SELECT n FROM t) LIMIT) SELECT n FROM t",
            "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL (SELECT n FROM t) ORDER BY) SELECT n FROM t",
            "WITH RECURSIVE t() AS (SELECT 1) SELECT 1",
            "WITH RECURSIVE t AS (SELECT 1) DELETE FROM docs",
        ] {
            assert_eq!(code(sql), "42601", "{sql}");
        }
    }

    #[test]
    fn operator_words_as_names_do_not_misclassify() {
        for name in ["union", "offset", "fetch", "intersect", "except"] {
            let sql = format!(
                "WITH RECURSIVE t(n) AS (SELECT 1 UNION ALL SELECT {name} + 1 FROM t WHERE {name} < 3) SELECT n FROM t"
            );
            assert_eq!(code(&sql), NOT_YET, "{sql}");
            let sql = format!(
                "WITH RECURSIVE t(n) AS (SELECT {name} FROM docs UNION ALL SELECT n + 1 FROM t WHERE n < 2) SELECT n FROM t"
            );
            assert_eq!(code(&sql), NOT_YET, "{sql}");
        }
    }

    #[test]
    fn definition_count_limit_is_54000() {
        let defs: Vec<String> = (0..=crate::sql::cte::MAX_CTE_DEFINITIONS)
            .map(|i| format!("c{i} AS (SELECT id FROM docs)"))
            .collect();
        let sql = format!("WITH RECURSIVE {} SELECT id FROM c0", defs.join(", "));
        assert_eq!(code(&sql), "54000");
    }
}
