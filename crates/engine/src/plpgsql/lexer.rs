//! PL/pgSQL 本体の span 付きスキャナ（Issue #2224・SQL-68）。
//!
//! `plpgsql::parser` が制御構造を解析するための前段。単語のキーワード分類は行わず、
//! 式・SQL 文の切れ目の判定に必要な区切り（引用・コメント・ドル引用・`:=`・`..`・`<<`/`>>`）
//! だけを認識して、元のバイト範囲つきのトークンを返す。切り出した原文の範囲は
//! `sql::lexer::tokenize_with_params` で SQL トークンへ変換される（本体をそのまま SQL
//! 字句解析器に通すと `:=`・`1..10`・`<<l>>`・`%TYPE` で失敗するため、スキャナを別にしている）。
//! 受信データの経路なので `unwrap`／`expect`／添字アクセスを使わない。エラー文言は固定の英語。

use crate::plpgsql_limits::HARD_MAX_PLPGSQL_TOKENS;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::{
    block_comment_end, lex_string_literal, line_comment_end, quoted_identifier_end,
};

use super::{syn_err, too_big_err};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TokKind {
    /// 未引用の語（原文の綴り）。
    Word(String),
    /// 二重引用符付き識別子（エスケープ解除後）。
    Quoted(String),
    Number,
    Str,
    /// `$n`。
    Param(u16),
    /// ドル引用文字列（`$tag$...$tag$` を 1 トークンで読む）。
    Dollar,
    /// `:=`
    Assign,
    /// `..`
    DotDot,
    /// `<<`
    LabelOpen,
    /// `>>`
    LabelClose,
    Punct(char),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Tok {
    pub(super) kind: TokKind,
    pub(super) start: usize,
    pub(super) end: usize,
}

fn is_word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// `src` を走査してトークン列にする。トークン数は空白・コメントを読み飛ばした後で判定する。
pub(super) fn scan(src: &str) -> Result<Vec<Tok>, SqlSurfaceError> {
    let bytes = src.as_bytes();
    let mut i = 0usize;
    let mut out: Vec<Tok> = Vec::new();
    while let Some(&b) = bytes.get(i) {
        if b.is_ascii_whitespace() {
            i = i.saturating_add(1);
            continue;
        }
        let next = bytes.get(i.saturating_add(1)).copied();
        if b == b'-' && next == Some(b'-') {
            i = line_comment_end(src, i);
            continue;
        }
        if b == b'/' && next == Some(b'*') {
            i = block_comment_end(src, i)
                .ok_or_else(|| syn_err("unterminated block comment in PL/pgSQL body"))?;
            continue;
        }
        if out.len() >= HARD_MAX_PLPGSQL_TOKENS {
            return Err(too_big_err("PL/pgSQL body has too many tokens"));
        }
        let start = i;
        let kind = if b == b'\'' {
            let (_, end) = lex_string_literal(src, i)
                .map_err(|_| syn_err("unterminated string literal in PL/pgSQL body"))?;
            i = end;
            TokKind::Str
        } else if b == b'"' {
            let end = quoted_identifier_end(src, i)
                .ok_or_else(|| syn_err("unterminated quoted identifier in PL/pgSQL body"))?;
            let inner = src
                .get(i.saturating_add(1)..end.saturating_sub(1))
                .ok_or_else(|| syn_err("invalid quoted identifier in PL/pgSQL body"))?;
            i = end;
            TokKind::Quoted(inner.replace("\"\"", "\""))
        } else if b.is_ascii_digit() {
            while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                i = i.saturating_add(1);
            }
            // `1..10` の `1.` で止める（`.` の直後が `.` なら小数点として読まない）。
            if bytes.get(i) == Some(&b'.') && bytes.get(i.saturating_add(1)) != Some(&b'.') {
                i = i.saturating_add(1);
                while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                    i = i.saturating_add(1);
                }
            }
            TokKind::Number
        } else if b == b'$' {
            scan_dollar(src, &mut i)?
        } else if b == b':' && next == Some(b'=') {
            i = i.saturating_add(2);
            TokKind::Assign
        } else if b == b'.' && next == Some(b'.') {
            i = i.saturating_add(2);
            TokKind::DotDot
        } else if b == b'<' && next == Some(b'<') {
            i = i.saturating_add(2);
            TokKind::LabelOpen
        } else if b == b'>' && next == Some(b'>') {
            i = i.saturating_add(2);
            TokKind::LabelClose
        } else {
            let c = src
                .get(i..)
                .and_then(|r| r.chars().next())
                .ok_or_else(|| syn_err("invalid character in PL/pgSQL body"))?;
            if is_word_start(c) {
                let mut end = i;
                for (off, ch) in src.get(i..).unwrap_or("").char_indices() {
                    if !is_word_char(ch) {
                        break;
                    }
                    end = i.saturating_add(off).saturating_add(ch.len_utf8());
                }
                let text = src.get(i..end).unwrap_or("").to_string();
                i = end;
                TokKind::Word(text)
            } else {
                i = i.saturating_add(c.len_utf8());
                TokKind::Punct(c)
            }
        };
        out.push(Tok {
            kind,
            start,
            end: i,
        });
    }
    Ok(out)
}

/// `$` で始まるトークン（`$n`・ドル引用・単独の `$`）を読む。`*i` は `$` の位置。
fn scan_dollar(src: &str, i: &mut usize) -> Result<TokKind, SqlSurfaceError> {
    let bytes = src.as_bytes();
    let start = *i;
    let mut j = start.saturating_add(1);
    if bytes.get(j).is_some_and(u8::is_ascii_digit) {
        let digits_start = j;
        while bytes.get(j).is_some_and(u8::is_ascii_digit) {
            j = j.saturating_add(1);
        }
        let n: u16 = src
            .get(digits_start..j)
            .and_then(|d| d.parse().ok())
            .ok_or_else(|| syn_err("invalid parameter reference in PL/pgSQL body"))?;
        *i = j;
        return Ok(TokKind::Param(n));
    }
    while bytes
        .get(j)
        .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_')
    {
        j = j.saturating_add(1);
    }
    if bytes.get(j) == Some(&b'$') {
        let tag_end = j.saturating_add(1);
        let tag = src.get(start..tag_end).unwrap_or("$$");
        let rest = src.get(tag_end..).unwrap_or("");
        let pos = rest
            .find(tag)
            .ok_or_else(|| syn_err("unterminated dollar-quoted string in PL/pgSQL body"))?;
        *i = tag_end.saturating_add(pos).saturating_add(tag.len());
        return Ok(TokKind::Dollar);
    }
    *i = start.saturating_add(1);
    Ok(TokKind::Punct('$'))
}
