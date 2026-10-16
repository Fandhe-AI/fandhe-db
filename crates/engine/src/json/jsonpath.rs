//! SQL/JSON path（`jsonpath`）の字句・構文解析器（Issue #2183・TABLE-25 ポインタ）。
//!
//! 責務境界: untrusted な jsonpath 文字列を [`SqlJsonPath`]（AST）へ解析するところ
//! （[`parse_jsonpath`]）と、解析済みの AST を jsonb 値へ適用する評価器（Issue #2184。
//! [`jsonpath_query`]・[`jsonpath_exists`]・[`jsonpath_match`]。意味論と上限は `eval.rs`）
//! まで。`jsonpath` 型の入出力・`jsonb_path_*` 関数（#2185）・`@?`／`@@`（#2186）・
//! `JSON_EXISTS` 等（#2187）は呼び出し側の後続 Issue が担い、本モジュールはそれらから
//! 呼ばれる前提の入口だけを公開する。`super` の `JsonPath`／`JsonPathStep`（`#>` 用の
//! Rust 内部 API）とは無関係。
//!
//! 受理文法は PostgreSQL 17 の `jsonpath` に合わせる。意図的な差分は 3 点。
//! (1) 長さ・入れ子深さ・数値リテラル長・式の高さに上限を設ける（PostgreSQL には無い。
//! fail-closed の DoS 対策。超過は `54000`）。(2) `like_regex` は構文を解析した上で
//! 0A000 で拒否する（正規表現の依存が未承認のため）。(3) `\u0000` は PostgreSQL では
//! `22P05` だが、対応する `ErrorClass` が無いため暫定で `22P02` とする。
//! エラーは固定の英語文のみを返し、入力の断片を含めない。

pub mod ast;
mod eval;
mod lexer;
mod numeric;
mod parser;

pub use ast::{
    AccessorOp, AnyLevel, ArithOp, CompareOp, Method, PathMode, PathNode, PathNumeric, RegexFlags,
    SqlJsonPath, StartsWithOperand, Subscript, UnaryOp,
};

pub use eval::{
    jsonpath_exists, jsonpath_match, jsonpath_query, JsonPathEvalError, JsonPathEvalOptions,
    MAX_SEQUENCE_ITEMS,
};

use crate::error_format::{ClassifiedError, ErrorClass};

/// 入力の最大バイト長（字句解析の前に判定する）。
pub const MAX_JSONPATH_INPUT_LEN: usize = 65_536;
/// 括弧・フィルタ・単項演算子・添字式の最大入れ子深さ。再帰下降の各再帰入口で
/// `depth >= MAX` なら拒否する（`MAX` 段ちょうどは受理）。
pub const MAX_JSONPATH_NESTING_DEPTH: usize = 64;
/// 数値リテラル 1 個の最大バイト長（基数変換の前に判定する）。
pub const MAX_JSONPATH_NUMERIC_LITERAL_LEN: usize = 1_024;
/// 式木の高さの上限。`1+1+1+...` のような左結合の連鎖が AST を深くし、再帰的な drop・
/// 評価でスタックを使い切ることを防ぐ。
pub const MAX_JSONPATH_NODE_HEIGHT: usize = 1_024;

/// [`parse_jsonpath`] の失敗。`wire_code()` は PostgreSQL の解析段階の SQLSTATE に
/// 合わせる（`2203x` 系は評価時のエラーで本モジュールでは発生しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonPathParseError {
    /// 文法違反・余計な文字・閉じていない引用符やコメント・`@`／`last` の位置違反等（42601）。
    Syntax,
    /// 空文字列・空白だけの入力（22P02）。
    Empty,
    /// 対になっていないサロゲート・範囲外のコードポイント（22P02）。
    InvalidUnicode,
    /// NUL 文字（`\u0000`・`\x00`）。PostgreSQL は 22P05。暫定で 22P02（下記）。
    UnsupportedUnicodeEscape,
    /// `**{n}` の n が int4 の範囲外（22003）。
    IntegerOutOfRange,
    /// `like_regex`（0A000）。
    RegexNotSupported,
    /// [`MAX_JSONPATH_INPUT_LEN`] 超過（54000）。
    TooLong,
    /// [`MAX_JSONPATH_NESTING_DEPTH`]・[`MAX_JSONPATH_NODE_HEIGHT`] 超過（54000）。
    TooDeep,
    /// [`MAX_JSONPATH_NUMERIC_LITERAL_LEN`] 超過（54000）。
    LiteralTooLong,
}

impl ClassifiedError for JsonPathParseError {
    fn error_class(&self) -> ErrorClass {
        match self {
            JsonPathParseError::Syntax => ErrorClass::UnsupportedSqlSyntax,
            JsonPathParseError::Empty
            | JsonPathParseError::InvalidUnicode
            // 22P05 用の分類を足すと error_format の件数・wire-server の HTTP 写像まで
            // 波及する。jsonpath を SQL 表層へ配線する Issue（#2185／#2186）で追加する。
            | JsonPathParseError::UnsupportedUnicodeEscape => ErrorClass::InvalidTextRepresentation,
            JsonPathParseError::IntegerOutOfRange => ErrorClass::NumericOutOfRange,
            JsonPathParseError::RegexNotSupported => ErrorClass::FeatureNotSupported,
            JsonPathParseError::TooLong
            | JsonPathParseError::TooDeep
            | JsonPathParseError::LiteralTooLong => ErrorClass::PayloadTooLarge,
        }
    }

    fn client_message(&self) -> String {
        match self {
            JsonPathParseError::Syntax => "invalid input syntax for type jsonpath",
            JsonPathParseError::Empty => "invalid input syntax for type jsonpath: empty input",
            JsonPathParseError::InvalidUnicode => "invalid unicode sequence in jsonpath",
            JsonPathParseError::UnsupportedUnicodeEscape => {
                "unsupported Unicode escape sequence in jsonpath"
            }
            JsonPathParseError::IntegerOutOfRange => "jsonpath integer is out of range",
            JsonPathParseError::RegexNotSupported => "like_regex is not supported in jsonpath",
            JsonPathParseError::TooLong => "jsonpath exceeds maximum length",
            JsonPathParseError::TooDeep => "jsonpath exceeds maximum nesting depth",
            JsonPathParseError::LiteralTooLong => "jsonpath numeric literal is too long",
        }
        .to_string()
    }
}

impl JsonPathParseError {
    /// SQLSTATE 風 `wire_code`（[`ClassifiedError`] へ委譲）。
    pub fn wire_code(&self) -> &'static str {
        ClassifiedError::wire_code(self)
    }
}

/// jsonpath 文字列を解析する。成功値だけが [`SqlJsonPath`] を作れるため、評価器へ渡る
/// AST は `@`／`last` の位置検査と `like_regex` 拒否を通過済みである。
pub fn parse_jsonpath(input: &str) -> Result<SqlJsonPath, JsonPathParseError> {
    if input.len() > MAX_JSONPATH_INPUT_LEN {
        return Err(JsonPathParseError::TooLong);
    }
    parser::parse(input)
}
