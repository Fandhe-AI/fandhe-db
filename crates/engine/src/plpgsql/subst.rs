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

/// 数値の置換形式。`Typed` は役割に応じた型を保つ形、`Bare` は裸のリテラル（`WHERE` 形の比較の
/// 検査〔`where_probe`〕のように、値同士の比較で宣言型が結果へ伝わらない用途だけに使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NumStyle {
    Typed,
    Bare,
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

/// `NULL` のトークン列。列としても読める位置（`Value`・`SelectItemBare`・`OrderKey`・`GroupKey`）は
/// 型付き（`SELECT NULL` 単独が受理されない・裸の `NULL` は識別子として列名に解釈され `42703`
/// や `null` 列での並べ替えに化ける）。型語の無い型は `OrderKey`／`GroupKey` では `0A000`
/// （fail-closed）、`Value`・`SelectItemBare` では裸の `NULL`。リテラル専用位置・`WHERE` の先頭は
/// 裸の `NULL`（`sql::params` の NULL 束縛と同じ）。
fn null_tokens(ty: &ColumnType, role: VarRole) -> Result<Vec<Token>, SqlSurfaceError> {
    let typed = matches!(
        role,
        VarRole::Value | VarRole::SelectItemBare | VarRole::OrderKey | VarRole::GroupKey
    );
    if typed {
        if let Some(words) = null_type_words(ty) {
            let mut out = vec![
                Token::Ident("CAST".to_string()),
                Token::Punct('('),
                Token::Ident("NULL".to_string()),
                Token::Ident("AS".to_string()),
            ];
            out.extend(words.iter().map(|w| Token::Ident((*w).to_string())));
            out.push(Token::Punct(')'));
            return Ok(out);
        }
        if matches!(role, VarRole::OrderKey | VarRole::GroupKey) {
            return Err(unsupported_type());
        }
    }
    Ok(vec![Token::Ident("NULL".to_string())])
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

/// 日付・時刻のトークン列。`CAST` を受理しない定数式（`ConstExpr`）は型付きリテラル
/// （`DATE '...'` 等）、それ以外は `CAST('...' AS 型)`。
fn temporal_tokens(text: String, word: &str, role: VarRole) -> Vec<Token> {
    if role == VarRole::ConstExpr {
        vec![
            Token::Ident(word.to_ascii_uppercase()),
            Token::StringLiteral(text),
        ]
    } else {
        cast_text_tokens(text, &[word])
    }
}

/// 数値の `CAST` の型語。変数の宣言型を保つ（整数リテラルのまま置換すると SQL 側が値から型を
/// 推定し直し、`bigint` 変数が `int4` として扱われて範囲エラーになる等、宣言型が失われる）。
/// 数値型以外の宣言（`PlScalar` が数値セルを別の型で持つ場合）は値の大きさから整数型を選ぶ。
fn number_type_words(v: &PlScalar) -> &'static [&'static str] {
    match (&v.ty, &v.cell) {
        (ColumnType::SmallInt, _) => &["smallint"],
        (ColumnType::Integer, _) => &["integer"],
        (ColumnType::BigInt, _) => &["bigint"],
        (ColumnType::Real, _) => &["real"],
        (ColumnType::Double, _) => &["double", "precision"],
        (ColumnType::Numeric { .. }, _) => &["numeric"],
        (_, Cell::Float(_)) => &["double", "precision"],
        (_, Cell::Numeric(_)) => &["numeric"],
        (_, Cell::SignedInteger(i)) if i32::try_from(*i).is_ok() => &["integer"],
        (_, Cell::Integer(u)) if i32::try_from(*u).is_ok() => &["integer"],
        _ => &["bigint"],
    }
}

/// 数値セルの値を表す裸のリテラルのトークン列（符号は `Punct('-')`）。NaN／Infinity・裸で書けない
/// 形は `0A000`。浮動小数は `Debug` 表記（整数値でも `3.0` と小数点を残す。`Display` の `3` は
/// 整数リテラルになり `p / 2` が整数除算へ化ける）、numeric は `Display` 表記（scale 0 は `3`）。
fn bare_number_tokens(v: &PlScalar) -> Result<Vec<Token>, SqlSurfaceError> {
    match &v.cell {
        Cell::SignedInteger(i) => Ok(int_tokens(*i < 0, i.unsigned_abs().to_string())),
        Cell::Integer(u) => Ok(int_tokens(false, u.to_string())),
        Cell::Float(f) if f.is_finite() => {
            number_tokens_from_text(&format!("{f:?}")).ok_or_else(unsupported_type)
        }
        Cell::Numeric(d) => number_tokens_from_text(&d.to_string()).ok_or_else(unsupported_type),
        _ => Err(unsupported_type()),
    }
}

/// `INSERT .. VALUES` の要素（定数式。利用者の `CAST` を受理しない）の数値のトークン列。
/// 整数（smallint／integer／bigint）と浮動小数（real／double）は、内部専用の型付き値（合成識別子
/// `$typed:<型>` + 数値。字句解析器は利用者の文字列から生成しない）で置換し、定数式の畳み込みが
/// 宣言型どおりの算術を行う（`p + 1` が bigint の算術になる）。numeric は定数式で算術できないため、
/// 単独の要素（[`is_lone`]）なら裸の数値（書き込み先の列型が決める）、式の中では `0A000`
/// （型が黙って変わる経路を残さない）。
fn const_number_tokens(
    v: &PlScalar,
    mut bare: Vec<Token>,
    lone: bool,
) -> Result<Vec<Token>, SqlSurfaceError> {
    let kind = match (&v.ty, &v.cell) {
        (_, Cell::Numeric(_)) => {
            return if lone {
                Ok(bare)
            } else {
                Err(SqlSurfaceError::feature_not_supported(
                    "a numeric variable inside an expression of an INSERT VALUES element is not supported",
                ))
            };
        }
        (ColumnType::SmallInt, _) => "int2",
        (ColumnType::Integer, _) => "int4",
        (ColumnType::BigInt, _) => "int8",
        (ColumnType::Real, _) => "float4",
        (_, Cell::Float(_)) => "float8",
        (_, Cell::SignedInteger(i)) if i32::try_from(*i).is_ok() => "int4",
        (_, Cell::Integer(u)) if i32::try_from(*u).is_ok() => "int4",
        _ => "int8",
    };
    match bare.pop() {
        Some(Token::Number(raw)) => {
            bare.push(Token::Ident(format!(
                "{}{kind}",
                crate::sql::udf_call::TYPED_VALUE_PREFIX
            )));
            bare.push(Token::Number(raw));
            Ok(bare)
        }
        _ => Err(unsupported_type()),
    }
}

/// 数値セルの置換トークン。列としても式としても読める位置は `CAST(<literal> AS <宣言型>)` で
/// 宣言型を保つ（`OrderKey`／`GroupKey` では裸の整数が序数参照に化けることの防止も兼ねる）。
/// `CAST` を受理しない位置は、宣言型が結果へ伝わらない `LiteralSlot`（`LIMIT`・`IN` リスト・
/// `BETWEEN` 境界・`LIKE`）では値を保つ裸のリテラル、`ConstExpr` では [`const_number_tokens`]。
fn number_tokens(
    v: &PlScalar,
    role: VarRole,
    style: NumStyle,
    lone: bool,
) -> Result<Vec<Token>, SqlSurfaceError> {
    let bare = bare_number_tokens(v)?;
    if role == VarRole::LiteralSlot || style == NumStyle::Bare {
        return Ok(bare);
    }
    if role == VarRole::ConstExpr {
        return const_number_tokens(v, bare, lone);
    }
    let mut out = vec![Token::Ident("CAST".to_string()), Token::Punct('(')];
    out.extend(bare);
    out.push(Token::Ident("AS".to_string()));
    out.extend(
        number_type_words(v)
            .iter()
            .map(|w| Token::Ident((*w).to_string())),
    );
    out.push(Token::Punct(')'));
    Ok(out)
}

/// 値 1 個を、型を保った SQL トークン列にする。`role` は NULL・数値の表現に影響する（文字列・
/// 真偽値は役割に依らず同一のトークン）。
pub(crate) fn scalar_tokens(v: &PlScalar, role: VarRole) -> Result<Vec<Token>, SqlSurfaceError> {
    scalar_tokens_styled(v, role, NumStyle::Typed, false)
}

/// 数値の形式を指定する [`scalar_tokens`]。
pub(crate) fn scalar_tokens_styled(
    v: &PlScalar,
    role: VarRole,
    style: NumStyle,
    lone: bool,
) -> Result<Vec<Token>, SqlSurfaceError> {
    Ok(match &v.cell {
        Cell::Null => null_tokens(&v.ty, role)?,
        Cell::SignedInteger(_) | Cell::Integer(_) | Cell::Float(_) | Cell::Numeric(_) => {
            number_tokens(v, role, style, lone)?
        }
        Cell::Bool(b) => vec![Token::Ident(if *b { "true" } else { "false" }.to_string())],
        Cell::Text(s) => vec![Token::StringLiteral(s.clone())],
        Cell::Date(d) => temporal_tokens(crate::datetime::format_date(*d), "date", role),
        Cell::Timestamp(us) => {
            temporal_tokens(crate::datetime::format_timestamp(*us), "timestamp", role)
        }
        Cell::Time(us) => temporal_tokens(crate::datetime::format_time(*us), "time", role),
        _ => return Err(unsupported_type()),
    })
}

/// 役割に応じた置換トークン（数値の `CAST` 包みは [`scalar_tokens`]）。`SelectItemBare` の別名付与は呼び出し側（[`substitute`]）が行う。
fn role_tokens(
    v: &PlScalar,
    role: VarRole,
    name_for_alias: Option<&str>,
    style: NumStyle,
    lone: bool,
) -> Result<Vec<Token>, SqlSurfaceError> {
    let mut out = scalar_tokens_styled(v, role, style, lone)?;
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
    if role == VarRole::SelectItemBare {
        if let Some(name) = name_for_alias {
            out.push(Token::Ident("AS".to_string()));
            out.push(Token::Ident(name.to_string()));
        }
    }
    Ok(out)
}

/// `tokens[pos]` が `VALUES` の行の中で単独の要素（符号 `-` を除いて、直前が `,` か行頭の `(`、直後が
/// `,` か `)`）か。行頭の `(` は直前が `VALUES`・`,`・`(` のものに限る（`abs(p)` の `(` を除く）。
/// 関数の第 2 引数以降（`f(a, p)`）は区別できず単独とみなす（numeric の型は書き込み先の列型が決める）。
fn is_lone(tokens: &[Token], pos: usize) -> bool {
    let before = |n: usize| pos.checked_sub(n).and_then(|p| tokens.get(p));
    let skip = usize::from(matches!(before(1), Some(Token::Punct('-')))) + 1;
    let prev_ok = match before(skip) {
        Some(Token::Punct(',')) => true,
        Some(Token::Punct('(')) => match pos.checked_sub(skip + 1).and_then(|p| tokens.get(p)) {
            Some(Token::Punct(',' | '(')) => true,
            Some(Token::Ident(w)) => w.eq_ignore_ascii_case("VALUES"),
            _ => false,
        },
        _ => false,
    };
    prev_ok
        && matches!(
            tokens.get(pos.saturating_add(1)),
            Some(Token::Punct(',' | ')'))
        )
}

/// `-` の直前のトークンが値の終端（二項演算子の左オペランドの末尾）か。単項の `-` かの判定に使う。
/// 値を終えない語（`THEN`・`ELSE`・`AND` など。字句解析器は `Keyword` 以外を `Ident` にする）は偽。
fn ends_operand(t: Option<&Token>) -> bool {
    match t {
        Some(Token::Ident(w)) => ![
            "THEN", "ELSE", "WHEN", "CASE", "OR", "NOT", "IN", "BETWEEN", "LIKE", "ILIKE", "IS",
            "VALUES", "SET", "DISTINCT", "ON", "AS", "BY",
        ]
        .iter()
        .any(|k| w.eq_ignore_ascii_case(k)),
        Some(
            Token::Number(_)
            | Token::StringLiteral(_)
            | Token::Param(_)
            | Token::QualifiedIdent { .. }
            | Token::Punct(')' | ']'),
        ) => true,
        _ => false,
    }
}

/// 負値の直前が単項の `-`（`-v`・`VALUES (-$1)`）なら、二重否定を畳んで `-` を取り除き値を正にする。
/// `VALUES` の定数式の解析は `-(..)`（単項マイナス + 括弧）を受理せず、括弧で包む方式では整数・
/// 浮動小数（3 トークン）が `42601` になる。範囲外になる値（`-(-2147483648)` の int4）は型付き値の
/// 検査が `22003` にする。二項の `-`（`a - v`）は畳まず、[`parenthesize_if_needed`] が括弧で包む。
fn fold_double_negation(
    tokens: &[Token],
    pos: usize,
    out: &mut Vec<Token>,
    produced: &mut Vec<Token>,
) {
    let Some(minus) = pos.checked_sub(1) else {
        return;
    };
    let unary = matches!(tokens.get(minus), Some(Token::Punct('-')))
        && !ends_operand(minus.checked_sub(1).and_then(|p| tokens.get(p)));
    let negative = produced.len() >= 2 && matches!(produced.first(), Some(Token::Punct('-')));
    if unary && negative && matches!(out.last(), Some(Token::Punct('-'))) {
        out.pop();
        produced.remove(0);
    }
}

/// 負値（先頭が `-` のトークン列。`-` + 数値の 2 トークンも、`ConstExpr` の整数・浮動小数が作る
/// `-` + `$typed:<型>` + 数値の 3 トークンも含む。トークン数には依存しない）を、`::` の直前または
/// 直前が `-` の位置で括弧に包む（`-5::int` が `-(5::int)`、`--5` がコメントや二重否定と読まれるのを
/// 防ぐ。`sql::params` の `$n` 置換と同じ規則）。
fn parenthesize_if_needed(prev: Option<&Token>, next: Option<&Token>, out: &mut Vec<Token>) {
    let negative = out.len() >= 2 && matches!(out.first(), Some(Token::Punct('-')));
    let needs = negative
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
    style: NumStyle,
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
        let mut produced = role_tokens(value, site.role, name, style, is_lone(tokens, site.pos))?;
        fold_double_negation(tokens, site.pos, &mut out, &mut produced);
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

fn is_word(t: Option<&Token>, w: &str) -> bool {
    matches!(t, Some(Token::Ident(s)) if s.eq_ignore_ascii_case(w))
}

/// `$n`（`tokens[i]`）がリテラル専用位置（`CAST` を受理しない位置）かを、同じ括弧深さの前後関係から
/// 推定する。`LIMIT`／`OFFSET`／`LIKE`／`ILIKE`／`BETWEEN` の直後、`BETWEEN .. AND` の上側境界、`IN (..)`
/// リストの要素が該当する。判定が確定しない位置は偽（`CAST` で型を保つ側）。
fn is_literal_slot(tokens: &[Token], i: usize) -> bool {
    let prev = i.checked_sub(1).and_then(|p| tokens.get(p));
    if matches!(prev, Some(Token::Keyword(lexer::Keyword::Limit)))
        || ["OFFSET", "LIKE", "ILIKE", "BETWEEN"]
            .iter()
            .any(|w| is_word(prev, w))
    {
        return true;
    }
    if matches!(prev, Some(Token::Keyword(lexer::Keyword::And))) {
        // `BETWEEN a AND $n`: 同じ深さで直前の論理演算子・節境界より前に `BETWEEN` があるか。
        let mut depth = 0usize;
        for j in (0..i.saturating_sub(1)).rev() {
            match tokens.get(j) {
                Some(Token::Punct(')')) => depth = depth.saturating_add(1),
                Some(Token::Punct('(')) => {
                    if depth == 0 {
                        return false;
                    }
                    depth -= 1;
                }
                _ if depth > 0 => {}
                t if is_word(t, "BETWEEN") => return true,
                t if is_word(t, "OR") => return false,
                Some(Token::Keyword(_)) => return false,
                _ => {}
            }
        }
        return false;
    }
    if matches!(prev, Some(Token::Punct('(' | ','))) {
        // `IN (.., $n, ..)`: 同じ深さの `(` まで遡り、その直前が `IN` か。
        let mut depth = 0usize;
        for j in (0..i).rev() {
            match tokens.get(j) {
                Some(Token::Punct(')')) => depth = depth.saturating_add(1),
                Some(Token::Punct('(')) => {
                    if depth == 0 {
                        return is_word(j.checked_sub(1).and_then(|p| tokens.get(p)), "IN");
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
    }
    false
}

/// `$n`（`tokens[i]`）が `INSERT .. VALUES` の要素の中か（直前の `VALUES` から `SELECT`／`SET`／
/// `RETURNING` 等を挟まずに到達する）。外れても、`ConstExpr` の置換形はどの位置でも受理される形。
fn in_values_list(tokens: &[Token], i: usize) -> bool {
    for j in (0..i).rev() {
        let t = tokens.get(j);
        if is_word(t, "VALUES") {
            return true;
        }
        // `AND`・`BY` は式の内側にも現れる（`CASE WHEN .. AND ..`）ため、境界にしない。
        let boundary = matches!(
            t,
            Some(Token::Keyword(
                lexer::Keyword::Select
                    | lexer::Keyword::From
                    | lexer::Keyword::Where
                    | lexer::Keyword::Order
                    | lexer::Keyword::Limit
            ))
        ) || ["SET", "RETURNING", "CONFLICT"]
            .iter()
            .any(|w| is_word(t, w));
        if boundary {
            return false;
        }
    }
    false
}

/// `EXECUTE` の文字列中の `$n`（`tokens[i]`）の構文上の役割を、同じ括弧深さの前後のトークンから
/// 推定する。リテラル専用位置は [`VarRole::LiteralSlot`]、`ORDER BY`／`GROUP BY` の直接のキーは
/// [`VarRole::OrderKey`]／[`VarRole::GroupKey`]、別名なしの投影項目は [`VarRole::SelectItemBare`]、
/// それ以外（括弧内を含む判定不能な位置）は [`VarRole::Value`]（静的 SQL の `role_tokens` と同じ保護）。
///
/// 後方走査では `AND`（`CASE WHEN .. AND ..` や `BETWEEN .. AND ..` の内側にも現れ、節境界ではない）で
/// 止めず、`CASE .. END` の内側も読み飛ばす。節境界は `FROM`／`WHERE`／`LIMIT` のみ。
/// 数値は全役割（リテラル専用位置を除く）で `CAST` 包みのため、推定を外しても序数参照・宣言型喪失には
/// ならず、`NULL` の型付けと投影項目の別名付与の要否だけに影響する。
fn param_role(tokens: &[Token], i: usize) -> VarRole {
    if is_literal_slot(tokens, i) {
        return VarRole::LiteralSlot;
    }
    if in_values_list(tokens, i) {
        return VarRole::ConstExpr;
    }
    let direct = matches!(
        i.checked_sub(1).and_then(|p| tokens.get(p)),
        Some(Token::Keyword(lexer::Keyword::Select | lexer::Keyword::By) | Token::Punct(','))
    );
    if !direct {
        return VarRole::Value;
    }
    // 括弧と `CASE .. END` の入れ子の深さ（後方走査なので `)`／`END` で増え `(`／`CASE` で減る）。
    let mut depth = 0usize;
    for j in (0..i).rev() {
        let t = tokens.get(j);
        match t {
            Some(Token::Punct(')')) => depth = depth.saturating_add(1),
            Some(Token::Punct('(')) => {
                if depth == 0 {
                    return VarRole::Value;
                }
                depth -= 1;
            }
            _ if is_word(t, "END") => depth = depth.saturating_add(1),
            _ if is_word(t, "CASE") => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            Some(Token::Keyword(lexer::Keyword::By)) => {
                return match j.checked_sub(1).and_then(|p| tokens.get(p)) {
                    Some(Token::Keyword(lexer::Keyword::Order)) => VarRole::OrderKey,
                    Some(Token::Ident(g)) if g.eq_ignore_ascii_case("group") => VarRole::GroupKey,
                    _ => VarRole::Value,
                };
            }
            Some(Token::Keyword(lexer::Keyword::Select)) => return VarRole::SelectItemBare,
            Some(Token::Keyword(
                lexer::Keyword::From | lexer::Keyword::Where | lexer::Keyword::Limit,
            )) => return VarRole::Value,
            _ => {}
        }
    }
    VarRole::Value
}

/// `EXECUTE` の文字列中の `$n` を `using` の値で置換する（D6）。`$n` の最大番号と `using` の個数が
/// 一致しなければ `42601`（欠落・過剰とも）。文字列連結ではなく型付きのトークンで置換する。
pub(crate) fn substitute_params(
    tokens: &[Token],
    using: &[PlScalar],
    style: NumStyle,
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
                let role = param_role(tokens, i);
                let mut produced = role_tokens(v, role, None, style, is_lone(tokens, i))?;
                fold_double_negation(tokens, i, &mut out, &mut produced);
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
