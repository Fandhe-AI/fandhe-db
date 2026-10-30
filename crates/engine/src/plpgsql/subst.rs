//! 変数・`USING` の値を SQL トークンへ置換する純粋関数（Issue #2226・SQL-68。設計は
//! `docs/design/plpgsql-subset.md` D3・D6）。
//!
//! `core::plpgsql_host` が埋め込み SQL を実行する直前に呼ぶ。定義時に記録した位置と役割
//! （`VarSite`）に従い、変数の**型付きの値から直接**、字句解析器が同じ値をリテラルで書いた SQL から
//! 生成するのと同一のトークン列を組み立てて置換する。値を SQL テキストへ戻して再び字句解析する
//! 経路は存在せず、値経由のインジェクションは構造的に起きない（`sql::params` の型付き置換と同じ
//! 構成原理）。ストレージ・カタログ・テナントには触れない。
//!
//! 置換後のトークン列の総バイト数は [`lexer::MAX_INPUT_LEN`] 以下に制限する（巨大な値を同じ変数の
//! 複数参照で複製して増幅させない。`sql::params::decode_bind_values` と同じ規約）。エラー文言は
//! 固定の英語で、値・変数名を含めない。対応しない型・位置は `0A000`（fail-closed）。

use crate::catalog::ColumnType;
use crate::plpgsql::ast::{VarRole, VarSite};
use crate::plpgsql::host::{PlScalar, VarLookup};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::lexer::{self, Token};

fn unsupported_type() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(
        "unsupported variable type for substitution into a PL/pgSQL embedded SQL fragment",
    )
}

fn too_big() -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large("substituted PL/pgSQL statement is too large")
}

/// 整数のトークン列。負値は字句解析器と同じく `Punct('-')` + `Number`。
fn int_tokens(negative: bool, digits: String) -> Vec<Token> {
    let mut out = Vec::with_capacity(2);
    if negative {
        out.push(Token::Punct('-'));
    }
    out.push(Token::Number(digits));
    out
}

/// `CAST('<text>' AS <type>)` のトークン列（許可リストが `CAST` を受理する位置でだけ使う）。
fn cast_text_tokens(text: String, type_words: &[&str]) -> Vec<Token> {
    let mut out = vec![
        Token::Ident("CAST".to_string()),
        Token::Punct('('),
        Token::StringLiteral(text),
        Token::Ident("AS".to_string()),
    ];
    out.extend(type_words.iter().map(|w| Token::Ident((*w).to_string())));
    out.push(Token::Punct(')'));
    out
}

/// 型付き `NULL`（`CAST(NULL AS <type>)`）の型語。対応外の型は `None`（裸の `NULL` へ倒す）。
fn null_type_words(ty: &ColumnType) -> Option<&'static [&'static str]> {
    Some(match ty {
        ColumnType::SmallInt => &["smallint"],
        ColumnType::Integer => &["integer"],
        ColumnType::BigInt => &["bigint"],
        ColumnType::Real => &["real"],
        ColumnType::Double => &["double", "precision"],
        ColumnType::Boolean => &["boolean"],
        ColumnType::Numeric { .. } => &["numeric"],
        ColumnType::Text | ColumnType::Varchar(_) | ColumnType::Bpchar(_) => &["text"],
        ColumnType::Date => &["date"],
        ColumnType::Timestamp => &["timestamp"],
        _ => return None,
    })
}

/// `NULL` のトークン列。値・投影項目の位置（`Value`・`SelectItemBare`）は型付き（`SELECT NULL` 単独が受理されないため）、
/// それ以外（リテラル専用位置・`WHERE` の先頭等）は裸の `NULL`（`sql::params` の NULL 束縛と同じ）。
fn null_tokens(ty: &ColumnType, role: VarRole) -> Vec<Token> {
    if matches!(role, VarRole::Value | VarRole::SelectItemBare) {
        if let Some(words) = null_type_words(ty) {
            let mut out = vec![
                Token::Ident("CAST".to_string()),
                Token::Punct('('),
                Token::Ident("NULL".to_string()),
                Token::Ident("AS".to_string()),
            ];
            out.extend(words.iter().map(|w| Token::Ident((*w).to_string())));
            out.push(Token::Punct(')'));
            return out;
        }
    }
    vec![Token::Ident("NULL".to_string())]
}

/// 数値リテラルとして字句解析器が 1 個の `Number` にするテキストか（符号は呼び出し側で剥がす）。
fn number_tokens_from_text(text: &str) -> Option<Vec<Token>> {
    let (negative, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    lexer::is_single_number_literal(body).then(|| int_tokens(negative, body.to_string()))
}

/// 小数点を含む数値リテラル 1 個のトークン列（整数リテラルとは型が異なる値を、元の型を保って置換する）。
fn typed_decimal_tokens(text: &str) -> Option<Vec<Token>> {
    if !text.contains('.') {
        return None;
    }
    number_tokens_from_text(text)
}

/// 浮動小数点の `CAST` の型語（`Real` 以外は倍精度）。
fn float_type_words(ty: &ColumnType) -> &'static [&'static str] {
    if matches!(ty, ColumnType::Real) {
        &["real"]
    } else {
        &["double", "precision"]
    }
}

/// 値 1 個を、字句解析器が同じ値のリテラルから生成するのと同一のトークン列にする。`role` は NULL の
/// 表現だけに影響する（整数・文字列・真偽値は役割に依らず同一のトークン）。
pub(crate) fn scalar_tokens(v: &PlScalar, role: VarRole) -> Result<Vec<Token>, SqlSurfaceError> {
    Ok(match &v.cell {
        Cell::Null => null_tokens(&v.ty, role),
        Cell::SignedInteger(i) => int_tokens(*i < 0, i.unsigned_abs().to_string()),
        Cell::Integer(u) => int_tokens(false, u.to_string()),
        Cell::Float(f) => {
            if !f.is_finite() {
                return Err(unsupported_type());
            }
            // `Debug` は整数値でも `3.0` と小数点を残す（`Display` の `3` は整数リテラルになり、
            // `p / 2` が整数除算へ化ける）。小数点の無い形（指数表記）は型を明示して保つ。
            let text = format!("{f:?}");
            match typed_decimal_tokens(&text) {
                Some(t) => t,
                None => cast_text_tokens(text, float_type_words(&v.ty)),
            }
        }
        Cell::Numeric(d) => {
            let text = d.to_string();
            match typed_decimal_tokens(&text) {
                Some(t) => t,
                None => cast_text_tokens(text, &["numeric"]),
            }
        }
        Cell::Bool(b) => vec![Token::Ident(if *b { "true" } else { "false" }.to_string())],
        Cell::Text(s) => vec![Token::StringLiteral(s.clone())],
        Cell::Date(d) => cast_text_tokens(crate::datetime::format_date(*d), &["date"]),
        Cell::Timestamp(us) => {
            cast_text_tokens(crate::datetime::format_timestamp(*us), &["timestamp"])
        }
        Cell::Time(us) => cast_text_tokens(crate::datetime::format_time(*us), &["time"]),
        _ => return Err(unsupported_type()),
    })
}

/// 役割に応じた置換トークン。`OrderKey`／`GroupKey` の整数リテラルは序数参照に化けるため
/// `CAST(n AS integer)` で包む。`SelectItemBare` の別名付与は呼び出し側（[`substitute`]）が行う。
fn role_tokens(
    v: &PlScalar,
    role: VarRole,
    name_for_alias: Option<&str>,
) -> Result<Vec<Token>, SqlSurfaceError> {
    let mut out = scalar_tokens(v, role)?;
    // 投影項目の位置は、文字列・真偽値の裸のリテラルを受理しない（`SELECT 'a' AS x`・`SELECT true AS x`
    // は `42601`／`42703`）ため、`CAST` で包んで式にする。
    if role == VarRole::SelectItemBare {
        match &v.cell {
            Cell::Text(t) => out = cast_text_tokens(t.clone(), &["text"]),
            Cell::Bool(b) => {
                out = cast_text_tokens(b.to_string(), &["boolean"]);
            }
            _ => {}
        }
    }
    match role {
        VarRole::OrderKey | VarRole::GroupKey
            if matches!(v.cell, Cell::SignedInteger(_) | Cell::Integer(_)) =>
        {
            let mut wrapped = vec![Token::Ident("CAST".to_string()), Token::Punct('(')];
            wrapped.append(&mut out);
            wrapped.push(Token::Ident("AS".to_string()));
            wrapped.push(Token::Ident("integer".to_string()));
            wrapped.push(Token::Punct(')'));
            out = wrapped;
        }
        VarRole::SelectItemBare => {
            if let Some(name) = name_for_alias {
                out.push(Token::Ident("AS".to_string()));
                out.push(Token::Ident(name.to_string()));
            }
        }
        _ => {}
    }
    Ok(out)
}

/// 負値（`-` + 数値の 2 トークン）を、`::` の直前または直前が `-` の位置で括弧に包む
/// （`-5::int` が `-(5::int)` と読まれるのを防ぐ。`sql::params` の `$n` 置換と同じ規則）。
fn parenthesize_if_needed(prev: Option<&Token>, next: Option<&Token>, out: &mut Vec<Token>) {
    let negative_pair = out.len() == 2 && matches!(out.first(), Some(Token::Punct('-')));
    let needs = negative_pair
        && (matches!(next, Some(Token::TypeCast)) || matches!(prev, Some(Token::Punct('-'))));
    if needs {
        out.insert(0, Token::Punct('('));
        out.push(Token::Punct(')'));
    }
}

/// 置換後トークン列のおおよそのバイト数（`sql::params::decode_bind_values_nullable` と同じ数え方）。
pub(crate) fn tokens_byte_len(tokens: &[Token]) -> Option<usize> {
    let mut total = 0usize;
    for t in tokens {
        let len = match t {
            Token::Ident(s)
            | Token::QuotedNull(s)
            | Token::QuotedBoolWord(s)
            | Token::QuotedCollationName(s)
            | Token::StringLiteral(s)
            | Token::Number(s) => s.len(),
            Token::QualifiedIdent { qualifier, name } => qualifier.len().checked_add(name.len())?,
            Token::QualifiedStar { qualifier } => qualifier.len().checked_add(2)?,
            _ => 1,
        };
        total = total.checked_add(len)?;
    }
    Some(total)
}

/// 値 1 個の置換トークンの総バイト数の見積り（複製の前に上限を検査するための上界寄りの値）。
fn scalar_len_estimate(v: &PlScalar) -> usize {
    // 文字列は本体、`CAST('..' AS ..)` 形の日時・数値は 64 バイトで上から押さえる。
    match &v.cell {
        Cell::Text(s) => s.len().saturating_add(64),
        _ => 64,
    }
}

/// 累積バイト数に `n` を足し、[`lexer::MAX_INPUT_LEN`] を超えるなら値を複製する前に拒否する。
fn charge(total: &mut usize, n: usize) -> Result<(), SqlSurfaceError> {
    *total = total.saturating_add(n);
    if *total > lexer::MAX_INPUT_LEN {
        return Err(too_big());
    }
    Ok(())
}

/// 変数参照の位置にあった元のトークンから、出力列名として使う名前を取る（`rec.f` はフィールド名）。
fn alias_name(tok: &Token) -> Option<&str> {
    match tok {
        Token::Ident(n) => Some(n.as_str()),
        Token::QualifiedIdent { name, .. } => Some(name.as_str()),
        _ => None,
    }
}

/// `tokens` の `sites` の位置を、`env` の変数の値から作ったトークン列へ置換する。`alias` が偽なら
/// `SelectItemBare` の別名付与を行わない（`WHERE` 形の式で使う）。
pub(crate) fn substitute(
    tokens: &[Token],
    sites: &[VarSite],
    env: &dyn VarLookup,
    alias: bool,
) -> Result<Vec<Token>, SqlSurfaceError> {
    let mut by_pos: Vec<&VarSite> = sites.iter().collect();
    by_pos.sort_by_key(|s| s.pos);
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut cursor = 0usize;
    let mut total = 0usize;
    for site in by_pos {
        let Some(before) = tokens.get(cursor..site.pos) else {
            return Err(SqlSurfaceError::Internal {
                detail: "PL/pgSQL variable site out of order".to_string(),
            });
        };
        charge(&mut total, tokens_byte_len(before).ok_or_else(too_big)?)?;
        out.extend_from_slice(before);
        let original = tokens
            .get(site.pos)
            .ok_or_else(|| SqlSurfaceError::Internal {
                detail: "PL/pgSQL variable site out of range".to_string(),
            })?;
        let value: &PlScalar = env.scalar(&site.var)?;
        charge(&mut total, scalar_len_estimate(value))?;
        let name = if alias { alias_name(original) } else { None };
        let mut produced = role_tokens(value, site.role, name)?;
        parenthesize_if_needed(
            site.pos.checked_sub(1).and_then(|p| tokens.get(p)),
            tokens.get(site.pos.saturating_add(1)),
            &mut produced,
        );
        out.extend(produced);
        cursor = site.pos.saturating_add(1);
    }
    let rest = tokens.get(cursor..).unwrap_or(&[]);
    charge(&mut total, tokens_byte_len(rest).ok_or_else(too_big)?)?;
    out.extend_from_slice(rest);
    Ok(out)
}

/// `EXECUTE` の文字列中の `$n` を `using` の値で置換する（D6）。`$n` の最大番号と `using` の個数が
/// 一致しなければ `42601`（欠落・過剰とも）。文字列連結ではなく型付きのトークンで置換する。
pub(crate) fn substitute_params(
    tokens: &[Token],
    using: &[PlScalar],
) -> Result<Vec<Token>, SqlSurfaceError> {
    let max = tokens
        .iter()
        .filter_map(|t| match t {
            Token::Param(n) => Some(usize::from(*n)),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    if max != using.len() {
        return Err(SqlSurfaceError::unsupported(
            "the number of USING values does not match the parameters of the EXECUTE string",
        ));
    }
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut total = 0usize;
    for (i, t) in tokens.iter().enumerate() {
        match t {
            Token::Param(n) => {
                let idx = usize::from(*n).checked_sub(1).ok_or_else(|| {
                    SqlSurfaceError::unsupported("parameter placeholder $0 is not valid")
                })?;
                let v = using.get(idx).ok_or_else(|| {
                    SqlSurfaceError::unsupported(
                        "the number of USING values does not match the parameters of the EXECUTE string",
                    )
                })?;
                charge(&mut total, scalar_len_estimate(v))?;
                let mut produced = scalar_tokens(v, VarRole::Value)?;
                parenthesize_if_needed(
                    i.checked_sub(1).and_then(|p| tokens.get(p)),
                    tokens.get(i.saturating_add(1)),
                    &mut produced,
                );
                out.extend(produced);
            }
            other => {
                charge(
                    &mut total,
                    tokens_byte_len(std::slice::from_ref(other)).ok_or_else(too_big)?,
                )?;
                out.push(other.clone());
            }
        }
    }
    Ok(out)
}
