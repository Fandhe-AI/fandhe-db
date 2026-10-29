//! 埋め込み SQL・式のトークン列に対する純粋な補助処理と、定義時ダミー検証（Issue #2224・SQL-68）。
//!
//! `plpgsql::parser` が切り出した SQL トークン列に対して、`INTO` 句の除去・識別子位置の分類・
//! 本体内で禁止する句（`USING OPERATION_ID`／`PARTITIONED`。ADR D8）の検出・ダミートークンの選択を行う。
//! 検証の実体（許可リスト解析）は `EmbeddedSqlChecker` を介して core が実装する
//! （`core::plpgsql_validate`）。`plpgsql` はストレージにもカタログにも触れない。
//! [`is_ident_position`] は #2226 の呼び出し時の値置換でも同じ分類を共有する。
//! ADR D3: 定義時はダミー、呼び出し時は型付きの値から作ったトークンで置換し、SQL テキストへ連結しない。

use std::collections::HashMap;

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::ddl_column_type::SqlColumnTypeName;
use crate::sql::lexer::{Keyword, Token};

use super::ast::{Block, Body, DeclType, EmbeddedKind, EmbeddedSql, SqlExpr, Stmt, VarRef};
use super::{oos_err, syn_err};

/// `Ident` が（大小無視で）`word` か。
pub(super) fn ident_is(t: &Token, word: &str) -> bool {
    matches!(t, Token::Ident(s) if s.eq_ignore_ascii_case(word))
}

/// `INTO` 句の生の対象（修飾子, 名前）。
pub(super) struct RawInto {
    pub(super) strict: bool,
    pub(super) targets: Vec<(Option<String>, String)>,
}

/// 括弧の深さ 0 の文脈キーワード `INTO` と対象列を取り除く。`INSERT INTO` の `INTO` は対象外。
/// `INTO` が 2 個あれば `42601`。
pub(super) fn split_into(
    tokens: Vec<Token>,
) -> Result<(Vec<Token>, Option<RawInto>), SqlSurfaceError> {
    let mut depth = 0usize;
    let mut found: Option<usize> = None;
    for (i, t) in tokens.iter().enumerate() {
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            _ => {}
        }
        if depth == 0 && ident_is(t, "INTO") {
            let after_insert = i
                .checked_sub(1)
                .and_then(|p| tokens.get(p))
                .is_some_and(|p| ident_is(p, "INSERT"));
            if after_insert {
                continue;
            }
            if found.is_some() {
                return Err(syn_err("multiple INTO clauses in a PL/pgSQL statement"));
            }
            found = Some(i);
        }
    }
    let Some(at) = found else {
        return Ok((tokens, None));
    };
    let mut j = at.saturating_add(1);
    let mut strict = false;
    if tokens.get(j).is_some_and(|t| ident_is(t, "STRICT")) {
        strict = true;
        j = j.saturating_add(1);
    }
    let mut targets = Vec::new();
    loop {
        match tokens.get(j) {
            Some(Token::Ident(n)) => targets.push((None, n.clone())),
            Some(Token::QualifiedIdent { qualifier, name }) => {
                targets.push((Some(qualifier.clone()), name.clone()))
            }
            _ => return Err(syn_err("invalid INTO target in a PL/pgSQL statement")),
        }
        j = j.saturating_add(1);
        if matches!(tokens.get(j), Some(Token::Punct(','))) {
            j = j.saturating_add(1);
        } else {
            break;
        }
    }
    let mut out = Vec::with_capacity(tokens.len());
    for (i, t) in tokens.into_iter().enumerate() {
        if i < at || i >= j {
            out.push(t);
        }
    }
    Ok((out, Some(RawInto { strict, targets })))
}

/// 本体の中で禁止する句（`USING OPERATION_ID`・括弧の深さ 0 の `PARTITIONED`）を `0A000` で拒否する。
pub(super) fn reject_forbidden_clauses(tokens: &[Token]) -> Result<(), SqlSurfaceError> {
    let mut depth = 0usize;
    for (i, t) in tokens.iter().enumerate() {
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            _ => {}
        }
        if ident_is(t, "USING")
            && tokens
                .get(i.saturating_add(1))
                .is_some_and(|n| ident_is(n, "OPERATION_ID"))
        {
            return Err(oos_err(
                "USING OPERATION_ID is not supported inside a PL/pgSQL body",
            ));
        }
        if depth == 0 && ident_is(t, "PARTITIONED") {
            return Err(oos_err(
                "PARTITIONED is not supported inside a PL/pgSQL body",
            ));
        }
    }
    Ok(())
}

/// `INSERT INTO t ( cols ) ...` の列リスト（`(` の次から `)` の手前まで）の範囲。
fn insert_column_list(tokens: &[Token]) -> Option<std::ops::Range<usize>> {
    if !tokens.first().is_some_and(|t| ident_is(t, "INSERT")) {
        return None;
    }
    if !tokens.get(1).is_some_and(|t| ident_is(t, "INTO")) {
        return None;
    }
    // 表名は `Ident` または `QualifiedIdent` の 1 トークン。
    if !matches!(tokens.get(3), Some(Token::Punct('('))) {
        return None;
    }
    let first = tokens.get(4)?;
    if matches!(first, Token::Keyword(Keyword::Select))
        || ident_is(first, "VALUES")
        || ident_is(first, "WITH")
    {
        return None;
    }
    let mut depth = 1usize;
    let mut k = 4usize;
    while let Some(t) = tokens.get(k) {
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(4..k);
                }
            }
            _ => {}
        }
        k = k.saturating_add(1);
    }
    None
}

/// 表名位置（`FROM`・`JOIN`・`ONLY`・`UPDATE`・`INTO`・`TABLE`・`USING` の直後）の 1 トークン前か。
fn is_table_slot(tokens: &[Token], j: usize) -> bool {
    match j.checked_sub(1).and_then(|p| tokens.get(p)) {
        Some(Token::Keyword(Keyword::From)) => true,
        Some(p) => ["JOIN", "INTO", "UPDATE", "TABLE", "ONLY", "USING"]
            .iter()
            .any(|w| ident_is(p, w)),
        None => false,
    }
}

/// 暗黙別名ではなく句の語になりうる語（表名の直後に現れる）。
const CLAUSE_WORDS: &[&str] = &[
    "WHERE",
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
    "ORDER",
    "LIMIT",
    "OFFSET",
    "HAVING",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "SET",
    "VALUES",
    "SELECT",
    "RETURNING",
    "FOR",
    "WINDOW",
    "FETCH",
    "OVERRIDING",
    "DEFAULT",
    "LATERAL",
    "TABLESAMPLE",
    "OUTER",
    "WHEN",
    "AS",
    "FROM",
    "DO",
    "AND",
    "OR",
    "NOT",
];

/// `i` を直接囲む `(` の 1 つ前のトークン（囲む括弧が無ければ `None`）。
fn enclosing_paren_prev(tokens: &[Token], i: usize) -> Option<&Token> {
    let mut depth = 0usize;
    let mut k = i;
    while k > 0 {
        k -= 1;
        match tokens.get(k)? {
            Token::Punct(')') => depth = depth.saturating_add(1),
            Token::Punct('(') => {
                if depth == 0 {
                    return k.checked_sub(1).and_then(|p| tokens.get(p));
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    None
}

/// 括弧の外側（深さ 0）の直近の `SET` が、`FROM`/`WHERE` 等を挟まずに存在するか（`SET` 代入リスト内か）。
fn in_set_list(tokens: &[Token], i: usize) -> bool {
    let mut depth = 0usize;
    let mut k = i;
    while k > 0 {
        k -= 1;
        match tokens.get(k) {
            Some(Token::Punct(')')) => depth = depth.saturating_add(1),
            Some(Token::Punct('(')) => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            Some(t) if depth == 0 => {
                // SET 節の開始は「直前が `,`／SELECT でなく、直後が `対象 =`」の `SET` だけ。
                // SELECT リストの `set` 列（`SELECT set, v = 1`）を SET 節と誤認しない。
                if ident_is(t, "SET") {
                    let prev_ok = k
                        .checked_sub(1)
                        .and_then(|p| tokens.get(p))
                        .is_some_and(|p| {
                            !matches!(p, Token::Punct(',') | Token::Keyword(Keyword::Select))
                        });
                    let next_ok =
                        matches!(
                            tokens.get(k.saturating_add(1)),
                            Some(Token::Ident(_) | Token::QualifiedIdent { .. })
                        ) && matches!(tokens.get(k.saturating_add(2)), Some(Token::Punct('=')));
                    if prev_ok && next_ok {
                        return true;
                    }
                }
                if matches!(
                    t,
                    Token::Keyword(Keyword::From | Keyword::Where | Keyword::Select)
                ) || ["RETURNING", "ON", "VALUES"].iter().any(|w| ident_is(t, w))
                {
                    return false;
                }
            }
            _ => {}
        }
    }
    false
}

/// `i` 以降が `AS [[NOT] MATERIALIZED] (` の形か（CTE 定義の入口）。`from` は `AS` の位置。
fn cte_as_follows(tokens: &[Token], from: usize) -> bool {
    if !tokens.get(from).is_some_and(|t| ident_is(t, "AS")) {
        return false;
    }
    let mut j = from.saturating_add(1);
    if tokens.get(j).is_some_and(|t| ident_is(t, "NOT")) {
        j = j.saturating_add(1);
    }
    if tokens.get(j).is_some_and(|t| ident_is(t, "MATERIALIZED")) {
        j = j.saturating_add(1);
    }
    matches!(tokens.get(j), Some(Token::Punct('(')))
}

/// `tokens[i]` が CTE の宣言名（直後が `AS (` 系）か。`SELECT c AS x` の別名元とは `(` の有無で区別する。
fn is_cte_decl_name(tokens: &[Token], i: usize) -> bool {
    cte_as_follows(tokens, i.saturating_add(1))
}

/// `tokens[i]` が CTE の列名リスト `name (a, b) AS (` の内側か。
fn in_cte_column_list(tokens: &[Token], i: usize) -> bool {
    let mut depth = 0usize;
    let mut k = i;
    while k > 0 {
        k -= 1;
        match tokens.get(k) {
            Some(Token::Punct(')')) => depth = depth.saturating_add(1),
            Some(Token::Punct('(')) => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    if depth != 0
        || !tokens
            .get(k)
            .is_some_and(|t| matches!(t, Token::Punct('(')))
    {
        return false;
    }
    // 対応する `)` を探す。
    let mut d = 0usize;
    let mut m = k;
    while let Some(t) = tokens.get(m) {
        match t {
            Token::Punct('(') => d = d.saturating_add(1),
            Token::Punct(')') => {
                d = d.saturating_sub(1);
                if d == 0 {
                    return cte_as_follows(tokens, m.saturating_add(1))
                        && k.checked_sub(1)
                            .and_then(|p| tokens.get(p))
                            .is_some_and(|p| matches!(p, Token::Ident(_)));
                }
            }
            _ => {}
        }
        m = m.saturating_add(1);
    }
    false
}

/// `WITH` で始まる文の主文の種別と、データ変更 CTE（`( INSERT|UPDATE|DELETE`）の有無を返す。
/// 括弧の深さ 0 で最初に現れる主文の語で種別を決める（CTE 本体は括弧内なので読み飛ばされる）。
pub(super) fn classify_with(tokens: &[Token]) -> (EmbeddedKind, bool) {
    let mut depth = 0usize;
    let mut kind = EmbeddedKind::Select;
    let mut found = false;
    for t in tokens {
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            _ if depth == 0 && !found => {
                if ident_is(t, "INSERT") {
                    kind = EmbeddedKind::Insert;
                    found = true;
                } else if ident_is(t, "UPDATE") {
                    kind = EmbeddedKind::Update;
                    found = true;
                } else if ident_is(t, "DELETE") {
                    kind = EmbeddedKind::Delete;
                    found = true;
                } else if matches!(t, Token::Keyword(Keyword::Select)) || ident_is(t, "VALUES") {
                    found = true;
                }
            }
            _ => {}
        }
    }
    (kind, has_write_cte(tokens))
}

/// `(` の直後が `INSERT`／`UPDATE`／`DELETE` のトークンがあるか（データ変更 CTE）。
pub(super) fn has_write_cte(tokens: &[Token]) -> bool {
    tokens.windows(2).any(|w| {
        matches!(w.first(), Some(Token::Punct('(')))
            && w.get(1).is_some_and(|t| {
                ident_is(t, "INSERT") || ident_is(t, "UPDATE") || ident_is(t, "DELETE")
            })
    })
}

/// `tokens[i]` の `Ident` が識別子の位置（関数名・表名・列名・別名・暗黙別名・`USING` の列／表・
/// `ON CONFLICT` の対象・`UPDATE ... SET`／`DO UPDATE SET` の代入先）にあるか。そうなら変数参照として
/// 置換しない。#2226 の呼び出し時の置換と共有する単一の分類関数。
pub(crate) fn is_ident_position(tokens: &[Token], i: usize) -> bool {
    if matches!(tokens.get(i.saturating_add(1)), Some(Token::Punct('('))) {
        return true;
    }
    let prev = i.checked_sub(1).and_then(|p| tokens.get(p));
    match prev {
        // `::型名` は型の位置であり値ではない。
        Some(Token::TypeCast) => return true,
        Some(Token::Keyword(Keyword::From)) => return true,
        Some(p)
            if [
                "JOIN",
                "INTO",
                "UPDATE",
                "TABLE",
                "AS",
                "ONLY",
                "USING",
                "CONSTRAINT",
            ]
            .iter()
            .any(|w| ident_is(p, w)) =>
        {
            return true
        }
        _ => {}
    }
    // CTE の宣言名（`WITH c AS (`・`c AS MATERIALIZED (`）と列名リスト（`c (a, b) AS (`）。
    if is_cte_decl_name(tokens, i) || in_cte_column_list(tokens, i) {
        return true;
    }
    // 暗黙別名: 表名の直後の語（`FROM t x`）。句の語は別名ではない。
    if let (Some(p @ (Token::Ident(_) | Token::QualifiedIdent { .. })), Some(Token::Ident(me))) =
        (prev, tokens.get(i))
    {
        if i >= 1
            && is_table_slot(tokens, i - 1)
            && !ident_is(p, "ONLY")
            && !CLAUSE_WORDS.iter().any(|w| me.eq_ignore_ascii_case(w))
        {
            return true;
        }
    }
    // `JOIN ... USING (cols)`・`ON CONFLICT (cols)` の列。
    if enclosing_paren_prev(tokens, i)
        .is_some_and(|p| ident_is(p, "USING") || ident_is(p, "CONFLICT"))
    {
        return true;
    }
    // `UPDATE ... SET` / `DO UPDATE SET` の代入先。
    if matches!(tokens.get(i.saturating_add(1)), Some(Token::Punct('=')))
        && prev.is_some_and(|p| ident_is(p, "SET") || matches!(p, Token::Punct(',')))
        && in_set_list(tokens, i)
    {
        return true;
    }
    insert_column_list(tokens).is_some_and(|r| r.contains(&i))
}

/// 定義時検証に使うダミートークン。整数・浮動小数は `0`、真偽値は `false`、それ以外は文字列 `'0'`。
pub(crate) fn dummy_token(ty: &DeclType) -> Token {
    fn named(t: &SqlColumnTypeName) -> Token {
        match t {
            SqlColumnTypeName::SmallInt
            | SqlColumnTypeName::Integer
            | SqlColumnTypeName::BigInt
            | SqlColumnTypeName::Real
            | SqlColumnTypeName::Double
            | SqlColumnTypeName::Numeric { .. } => Token::Number("0".to_string()),
            SqlColumnTypeName::Boolean => Token::Ident("false".to_string()),
            SqlColumnTypeName::Alias { target, .. } => named(target),
            _ => Token::StringLiteral("0".to_string()),
        }
    }
    match ty {
        DeclType::Named(t) => named(t),
        _ => Token::StringLiteral("0".to_string()),
    }
}

/// 埋め込み SQL の検証口。core が `parse_tokens`（許可リスト解析）へ委譲して実装する
/// （`core::plpgsql_validate`）。受け取るのは変数をダミーへ置換したトークン列。
pub(crate) trait EmbeddedSqlChecker {
    /// 構文検証。定義時に拒否すべき失敗（`42601`／`0A000`／`54000`／`54001`）だけを `Err` にする。
    fn check_syntax(&self, tokens: Vec<Token>) -> Result<(), SqlSurfaceError>;
}

fn substituted(tokens: &[Token], refs: &[(usize, VarRef)], body: &Body) -> Vec<Token> {
    let map: HashMap<usize, &VarRef> = refs.iter().map(|(i, r)| (*i, r)).collect();
    tokens
        .iter()
        .enumerate()
        .map(|(i, t)| match map.get(&i) {
            Some(VarRef::Var(id)) => match body.vars.get(id.0 as usize) {
                Some(v) => dummy_token(&v.ty),
                None => Token::StringLiteral("0".to_string()),
            },
            Some(VarRef::Field(..)) => Token::StringLiteral("0".to_string()),
            None => t.clone(),
        })
        .collect()
}

fn check_expr(e: &SqlExpr, body: &Body, c: &dyn EmbeddedSqlChecker) -> Result<(), SqlSurfaceError> {
    // 条件式は `SELECT 1 WHERE <cond>`、値の式は `SELECT <expr>` の形で検証する。
    let mut toks = vec![Token::Keyword(Keyword::Select)];
    if e.cond {
        toks.push(Token::Number("1".to_string()));
        toks.push(Token::Keyword(Keyword::Where));
    }
    toks.extend(substituted(&e.tokens, &e.var_refs, body));
    c.check_syntax(toks)
}

fn check_sql(
    s: &EmbeddedSql,
    body: &Body,
    c: &dyn EmbeddedSqlChecker,
) -> Result<(), SqlSurfaceError> {
    c.check_syntax(substituted(&s.tokens, &s.var_refs, body))
}

fn check_stmts(
    stmts: &[Stmt],
    body: &Body,
    c: &dyn EmbeddedSqlChecker,
) -> Result<(), SqlSurfaceError> {
    for s in stmts {
        match s {
            Stmt::Block(b) => check_block(b, body, c)?,
            Stmt::Assign { expr, .. } => check_expr(expr, body, c)?,
            Stmt::If { branches, else_ } => {
                for (cond, st) in branches {
                    check_expr(cond, body, c)?;
                    check_stmts(st, body, c)?;
                }
                if let Some(st) = else_ {
                    check_stmts(st, body, c)?;
                }
            }
            Stmt::Case {
                selector,
                whens,
                else_,
            } => {
                if let Some(sel) = selector {
                    check_expr(sel, body, c)?;
                }
                for (exprs, st) in whens {
                    for e in exprs {
                        check_expr(e, body, c)?;
                    }
                    check_stmts(st, body, c)?;
                }
                if let Some(st) = else_ {
                    check_stmts(st, body, c)?;
                }
            }
            Stmt::Loop { body: st, .. } => check_stmts(st, body, c)?,
            Stmt::While { cond, body: st, .. } => {
                check_expr(cond, body, c)?;
                check_stmts(st, body, c)?;
            }
            Stmt::ForInt {
                lower,
                upper,
                step,
                body: st,
                ..
            } => {
                check_expr(lower, body, c)?;
                check_expr(upper, body, c)?;
                if let Some(e) = step {
                    check_expr(e, body, c)?;
                }
                check_stmts(st, body, c)?;
            }
            Stmt::ForQuery {
                query, body: st, ..
            } => {
                check_sql(query, body, c)?;
                check_stmts(st, body, c)?;
            }
            Stmt::Exit { when, .. } | Stmt::Continue { when, .. } => {
                if let Some(e) = when {
                    check_expr(e, body, c)?;
                }
            }
            Stmt::Return(e) | Stmt::ReturnNext(e) => {
                if let Some(e) = e {
                    check_expr(e, body, c)?;
                }
            }
            Stmt::ReturnQuery(q) | Stmt::Perform(q) | Stmt::Sql(q) => check_sql(q, body, c)?,
            Stmt::Execute { query, using, .. } => {
                check_expr(query, body, c)?;
                for e in using {
                    check_expr(e, body, c)?;
                }
            }
            Stmt::GetDiagnostics { .. } | Stmt::Null => {}
        }
    }
    Ok(())
}

fn check_block(b: &Block, body: &Body, c: &dyn EmbeddedSqlChecker) -> Result<(), SqlSurfaceError> {
    for id in &b.decls {
        if let Some(d) = body
            .vars
            .get(id.0 as usize)
            .and_then(|v| v.default.as_ref())
        {
            check_expr(d, body, c)?;
        }
    }
    check_stmts(&b.stmts, body, c)
}

/// 本体の全ての埋め込み SQL・式を、変数をダミーへ置換して `checker` へ渡す（ADR D3）。
/// 解析木の深さは `parse_body` が上限で抑えているため、再帰で辿ってよい。
pub(crate) fn validate_embedded(
    body: &Body,
    checker: &dyn EmbeddedSqlChecker,
) -> Result<(), SqlSurfaceError> {
    check_block(&body.block, body, checker)
}
