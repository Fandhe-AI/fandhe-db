//! 要素つき `CREATE SCHEMA <名> [AUTHORIZATION <ロール>] <要素>...` の構文段（TABLE-27・TABLE-44・SQL-39・
//! SQL-40・RLS-7・RLS-8・RLS-10 のポインタ、Issue #3286。設計記録: `docs/design/schema-tenant-namespace.md`
//! 「Issue #3286 実装記録」）。
//!
//! 役割: `validate_create_schema_tokens`（`core.rs` の `parse_tokens_in_impl` から呼ばれる）が、SCHEMA の見出しの
//! 後ろに続くトークン列を「要素ごとのトークン列」へ分割し、各要素を既存の `CREATE TABLE`／`VIEW`／`SEQUENCE`
//! の検証器で構造検証する。カタログは一切照会しない。要素の名前解決と実行（SCHEMA の作成と全要素の作成を
//! 1 つの write txn に載せる）は `core::schema_elements` が担う。
//!
//! 受理する要素: `CREATE TABLE`・`CREATE VIEW`（列別名・`WITH CHECK OPTION`・`OR REPLACE` なしの単純形）・
//! `CREATE SEQUENCE`。それ以外の要素（`INDEX`・`GRANT`・`TRIGGER`・`FUNCTION` 等）と要素側の
//! `IF NOT EXISTS` は、黙って無視せず固定文言の `0A000` で拒否する（fail-closed）。
//!
//! 分割の限界: 字句段では引用付き識別子（`"create"`）と構文語を区別できないため、括弧の外の `CREATE`／
//! `GRANT` のうち「直後が要素を導く語」のものだけを境界にする。病的な入力（ビュー本文の引用付き列別名
//! `"create"` の直後に `table` という別名が続く形など）で誤って分割しても、各要素は個別の検証と実行段の
//! 権限ゲートを通るため、権限・テナント境界を越える経路にはならない（`42601`／`0A000` で終わる）。

use super::*;

/// 要素数の上限（分割中に数え、`Vec` へ積む前に検査する。DoS 対策）。超過は `54000`。
pub(crate) const MAX_SCHEMA_ELEMENTS: usize = 64;

/// 要素の種別。宣言順は PostgreSQL 17 が要素を実行する順（SEQUENCE → TABLE → VIEW）と同じで、
/// `Ord` の導出順をそのまま実行順に使う（同種の中は記述順を保つ安定ソート）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum SchemaElementKind {
    /// `CREATE SEQUENCE`。
    Sequence,
    /// `CREATE TABLE`。
    Table,
    /// `CREATE VIEW`。
    View,
}

/// 要素 1 件（原文のトークン列。`schema.name` の修飾は未解決のまま保持する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SchemaElement {
    /// 要素の種別。
    pub(crate) kind: SchemaElementKind,
    /// 要素のトークン列（先頭の `CREATE` から、末尾の `;` を除いた最後まで）。
    pub(crate) tokens: Vec<Token>,
}

/// 要素の開始を導く語（`CREATE` の直後）。ここに無い語は境界にしない。
const CREATE_INTRODUCERS: &[&str] = &[
    "TABLE",
    "VIEW",
    "SEQUENCE",
    "INDEX",
    "UNIQUE",
    "OR",
    "TEMP",
    "TEMPORARY",
    "GLOBAL",
    "LOCAL",
    "UNLOGGED",
    "MATERIALIZED",
    "TRIGGER",
    "FUNCTION",
    "PROCEDURE",
    "TYPE",
    "POLICY",
    "DOMAIN",
];

/// `GRANT` の直後に現れる権限語（`GRANT` を要素の境界とみなす条件）。
const GRANT_PRIVILEGES: &[&str] = &[
    "SELECT",
    "INSERT",
    "UPDATE",
    "DELETE",
    "TRUNCATE",
    "REFERENCES",
    "TRIGGER",
    "USAGE",
    "CREATE",
    "ALL",
    "EXECUTE",
    "TEMP",
    "TEMPORARY",
    "CONNECT",
];

fn word(tokens: &[Token], i: usize) -> Option<&str> {
    match tokens.get(i) {
        Some(Token::Ident(w)) => Some(w.as_str()),
        // `SELECT` だけは専用の Keyword トークンになる。
        Some(Token::Keyword(Keyword::Select)) => Some("SELECT"),
        _ => None,
    }
}

fn is_word(tokens: &[Token], i: usize, expected: &str) -> bool {
    word(tokens, i).is_some_and(|w| w.eq_ignore_ascii_case(expected))
}

/// `tokens[i]` が要素の境界（括弧の外の `CREATE`／`GRANT` で、直後が導入語）か。
fn is_boundary(tokens: &[Token], i: usize) -> bool {
    let next = i.saturating_add(1);
    if is_word(tokens, i, "CREATE") {
        return CREATE_INTRODUCERS.iter().any(
            |k| matches!(tokens.get(next), Some(Token::Ident(w)) if w.eq_ignore_ascii_case(k)),
        );
    }
    if is_word(tokens, i, "GRANT") {
        return GRANT_PRIVILEGES.iter().any(|k| is_word(tokens, next, k));
    }
    false
}

/// 見出し（`CREATE SCHEMA ... [AUTHORIZATION ...]`）より後ろの `rest` を要素ごとに分割する。括弧の深さ 0 の
/// 境界だけで区切る。上限は `Vec` へ積む前に検査する（`54000`）。
fn split_elements(rest: &[Token]) -> Result<Vec<&[Token]>, SqlSurfaceError> {
    let mut starts: Vec<usize> = vec![0];
    let mut depth: usize = 0;
    for (i, t) in rest.iter().enumerate() {
        match t {
            Token::Punct('(') => depth = depth.saturating_add(1),
            Token::Punct(')') => depth = depth.saturating_sub(1),
            _ if depth == 0 && i > 0 && is_boundary(rest, i) => {
                if starts.len() >= MAX_SCHEMA_ELEMENTS {
                    return Err(SqlSurfaceError::payload_too_large(
                        "too many schema elements",
                    ));
                }
                starts.push(i);
            }
            _ => {}
        }
    }
    let mut out: Vec<&[Token]> = Vec::with_capacity(starts.len());
    for (n, start) in starts.iter().enumerate() {
        let end = starts
            .get(n.saturating_add(1))
            .copied()
            .unwrap_or(rest.len());
        if let Some(slice) = rest.get(*start..end) {
            out.push(slice);
        }
    }
    Ok(out)
}

fn unsupported_element() -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(
        "CREATE SCHEMA with this kind of schema element is not supported",
    )
}

/// 要素の種別を判定する。受理しない種別は `0A000`、`CREATE`／`GRANT` で始まらない・導入語が無い形は `42601`。
fn classify(tokens: &[Token]) -> Result<SchemaElementKind, SqlSurfaceError> {
    if is_word(tokens, 0, "GRANT") {
        return Err(unsupported_element());
    }
    if !is_word(tokens, 0, "CREATE") {
        return Err(SqlSurfaceError::unsupported(
            "unexpected token in CREATE SCHEMA",
        ));
    }
    // 要素側の `IF NOT EXISTS`（PostgreSQL の要素文法には無い。黙って受理しない）。
    let kind = if is_word(tokens, 1, "TABLE") {
        SchemaElementKind::Table
    } else if is_word(tokens, 1, "VIEW") {
        SchemaElementKind::View
    } else if is_word(tokens, 1, "SEQUENCE") {
        SchemaElementKind::Sequence
    } else if CREATE_INTRODUCERS.iter().any(|k| is_word(tokens, 1, k)) {
        // INDEX・TRIGGER・FUNCTION・TYPE・MATERIALIZED VIEW・TEMP／UNLOGGED・OR REPLACE 等。
        return Err(unsupported_element());
    } else {
        return Err(SqlSurfaceError::unsupported(
            "unrecognized element in CREATE SCHEMA",
        ));
    };
    if is_word(tokens, 2, "IF") && is_word(tokens, 3, "NOT") {
        return Err(unsupported_element());
    }
    if kind == SchemaElementKind::Table && is_create_table_as_statement(tokens) {
        return Err(unsupported_element());
    }
    Ok(kind)
}

/// 構文検証用に、関係位置の `schema.name` 修飾を裸の名前へ落としたトークン列を作る（要素の検証器は
/// 名前解決済みのトークン列を前提にするため）。解決・置換は実行段が行い、ここでは名前の形だけを確かめる。
fn strip_qualifiers_for_probe(tokens: &[Token]) -> Vec<Token> {
    let mut out = tokens.to_vec();
    for pos in crate::sql::relation_name::relation_positions(tokens).positions {
        if let Some(Token::QualifiedIdent { name, .. }) = tokens.get(pos.index) {
            if let Some(slot) = out.get_mut(pos.index) {
                *slot = Token::Ident(name.clone());
            }
        }
    }
    // `nextval('s.q')` 等の文字列引数（regclass 相当）の修飾も、名前解決器が内部キーの `Ident` へ置き換えた後の
    // 形（検証器が受理する形）へ落とす。修飾付きの文字列のままだと検証器が `0A000` で拒否するため。
    for (i, t) in tokens.iter().enumerate() {
        let is_call = matches!(t, Token::Ident(w)
            if ["nextval", "currval", "setval"].iter().any(|k| w.eq_ignore_ascii_case(k)))
            && matches!(tokens.get(i.saturating_add(1)), Some(Token::Punct('(')));
        if !is_call {
            continue;
        }
        let at = i.saturating_add(2);
        if let Some(Token::StringLiteral(lit)) = tokens.get(at) {
            if let Some((q, n)) = lit.split_once('.') {
                if crate::catalog::validate_identifier(q).is_ok()
                    && crate::catalog::validate_identifier(n).is_ok()
                {
                    // 検証器は解決済みの内部キー形（`#<id>.<名前>`）の `Ident` だけを受理する。ID は構文検証の
                    // 仮の値（実行段が払い出した ID で置き換える）。
                    if let Some(slot) = out.get_mut(at) {
                        *slot = Token::Ident(
                            crate::catalog::namespace::RelationKey::InSchema {
                                schema_id: 1,
                                name: n.to_string(),
                            }
                            .encode(),
                        );
                    }
                }
            }
        }
    }
    out
}

/// 要素の作成名の位置（`CREATE TABLE|VIEW|SEQUENCE` の名前。`IF NOT EXISTS` の `IF` は除く）。
pub(crate) fn create_name_index(tokens: &[Token]) -> Option<usize> {
    crate::sql::relation_name::relation_positions(tokens)
        .positions
        .iter()
        .find(|p| {
            p.create_name
                && !p.matview
                && !(is_word(tokens, p.index, "IF")
                    && is_word(tokens, p.index.saturating_add(1), "NOT"))
        })
        .map(|p| p.index)
}

/// 要素 1 件を構造検証して [`SchemaElement`] にする。
fn validate_element(schema_name: &str, tokens: &[Token]) -> Result<SchemaElement, SqlSurfaceError> {
    let kind = classify(tokens)?;
    let probe = strip_qualifiers_for_probe(tokens);
    match kind {
        SchemaElementKind::Table => {
            // 永続表以外の形（TEMP・UNLOGGED・CTAS）は上の分類が拒否済み。
            validate_create_table_tokens(&probe)?;
        }
        SchemaElementKind::View => {
            let v = validate_create_view_tokens(&probe)?;
            // 実行段が必要とするカタログ照会つきの事前検査（列別名の突合・`WITH CHECK OPTION` の宣言検査）は
            // 単独の `CREATE VIEW` 腕にしか無いため、要素としては受理しない（fail-closed）。
            if v.or_replace()
                || !v.column_aliases().is_empty()
                || v.check_option() != crate::catalog::ViewCheckOption::None
            {
                return Err(unsupported_element());
            }
        }
        SchemaElementKind::Sequence => {
            validate_create_sequence_tokens(&probe)?;
        }
    }
    // 作成名が作成中の SCHEMA 以外で修飾されていれば `42P15`。比較は実行段の名前解決と同じく大文字小文字を区別する完全一致。
    if let Some(idx) = create_name_index(tokens) {
        if let Some(Token::QualifiedIdent { qualifier, .. }) = tokens.get(idx) {
            if qualifier != schema_name {
                return Err(SqlSurfaceError::InvalidSchemaDefinition {
                    schema: truncate_for_error(qualifier),
                    created: truncate_for_error(schema_name),
                });
            }
        }
    }
    Ok(SchemaElement {
        kind,
        tokens: tokens.to_vec(),
    })
}

/// 見出しの後ろのトークン列 `rest`（`CREATE`／`GRANT` で始まる）を要素へ分割・検証する。末尾の単一 `;` は
/// 取り除く。要素が 1 つも無い形（`rest` が空）はここへ来ない。
pub(super) fn parse_elements(
    schema_name: &str,
    rest: &[Token],
) -> Result<Vec<SchemaElement>, SqlSurfaceError> {
    let rest = match rest.split_last() {
        Some((Token::Punct(';'), head)) => head,
        _ => rest,
    };
    let mut out = Vec::new();
    for slice in split_elements(rest)? {
        out.push(validate_element(schema_name, slice)?);
    }
    Ok(out)
}

/// 見出しの直後が要素の開始（`CREATE`／`GRANT`）か。
pub(super) fn starts_element(p: &Parser<'_>) -> bool {
    p.peek_ident_matches("CREATE") || p.peek_ident_matches("GRANT")
}

#[cfg(test)]
mod tests;
