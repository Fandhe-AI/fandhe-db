//! PL/pgSQL サブセットの構文解析器（Issue #2224・SQL-68。設計は `docs/design/plpgsql-subset.md`）。
//!
//! 関数本体（原文）を AST（[`ast::Body`]）へ解析する。`lexer` が span 付きで走査し、`parser` が
//! 制御構造だけを再帰下降で解析して、式・埋め込み SQL は切り出して `sql::lexer` のトークン列にする。
//! 埋め込み SQL の構文検証は [`embedded::validate_embedded`] が `EmbeddedSqlChecker`（core が既存の
//! 許可リスト解析へ委譲して実装）経由で行う（ADR D3 の定義時ダミー検証）。サブセット外は `0A000`、
//! 構文誤りは `42601`、上限超過は `54000`／`54001`。呼び出し元は #2217（`LANGUAGE plpgsql` の受理）・
//! #2225（インタプリタ）が結線するまでテストだけ。ストレージ・テナントには触れない純粋な処理。

pub(crate) mod ast;
pub(crate) mod embedded;
mod lexer;
mod parser;
#[cfg(test)]
mod tests;

use crate::sql::allowlist::SqlSurfaceError;

pub(crate) use ast::{Body, ParseOptions};

/// 構文誤り（`42601`）。文言は固定の英語で、本体の断片や名前の値を含めない。
pub(super) fn syn_err(detail: impl Into<String>) -> SqlSurfaceError {
    SqlSurfaceError::unsupported(detail)
}

/// サブセット外（`0A000`）。
pub(super) fn oos_err(detail: impl Into<String>) -> SqlSurfaceError {
    SqlSurfaceError::feature_not_supported(detail)
}

/// 上限超過（`54000`）。
pub(super) fn too_big_err(detail: impl Into<String>) -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large(detail)
}

/// 関数本体 `src` を解析する。本体長はスキャンの前に検査する。
pub(crate) fn parse_body(src: &str, opts: &ParseOptions) -> Result<Body, SqlSurfaceError> {
    parser::parse(src, opts)
}

/// 下位の型・SQL 構文解析器のエラーを、本体の断片・名前を含まない固定の英語文言へ丸める。
/// SQLSTATE（`wire_code`）は保つ。想定外の符号は fail-closed に `42601` へ倒す。
/// `parser`（型名解析）と `core::plpgsql_validate`（許可リスト解析）の委譲境界で使う。
pub(crate) fn sanitize_delegated(e: &SqlSurfaceError, ctx: &'static str) -> SqlSurfaceError {
    match e.wire_code() {
        "0A000" => oos_err(format!("unsupported construct in {ctx}")),
        "54000" => too_big_err(format!("limit exceeded in {ctx}")),
        "54001" => SqlSurfaceError::StatementTooComplex {
            detail: "statement too complex in a PL/pgSQL body",
        },
        _ => syn_err(format!("syntax error in {ctx}")),
    }
}
