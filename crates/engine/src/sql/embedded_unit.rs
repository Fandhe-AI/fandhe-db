//! PL/pgSQL の埋め込み SQL を、カタログを引かずに構造だけ解析する入口（Issue #2224・SQL-68。ADR D3）。
//!
//! `plpgsql::embedded` の定義時分類が使う。文全体（ルート）と、`colref_sink` が取り込んだサブクエリ
//! 本体の範囲（ルート上のスライス）を、文頭の語で既存の許可リスト解析（`sql::allowlist`）へ振り分ける。
//! 表は常に存在するものとして扱う（`StructuralOnlyLookup`）ので、結果は呼び出し時点のカタログに
//! 依存しない（PostgreSQL の late binding と同じく、名前の解決は呼び出し時の完全な検証が担う）。
//! 位置の記録（`colref_sink`）はアドレス基準のため、呼び出し側は**スライスをコピーせず**渡す。
//! 要約（文種・データ変更 CTE の有無）はここで解析木から取り出す。

use crate::recovery::required_op_id::LedgerMode;
use crate::sql::allowlist::{
    self, ParsedInsertTokens, SessionWith, SqlSurfaceError, StructuralOnlyLookup,
};
use crate::sql::lexer::Token;

/// 解析した 1 単位の要約。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnitSummary {
    pub(crate) kind: UnitKind,
    /// データ変更 CTE を含む最上位の `WITH` 文か（書き込みを伴う）。
    pub(crate) has_write_cte: bool,
}

/// 解析した文の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UnitKind {
    Select,
    Insert,
    Update,
    Delete,
}

fn head_word(tokens: &[Token]) -> String {
    match tokens.first() {
        Some(Token::Ident(w)) => w.to_ascii_uppercase(),
        _ => String::new(),
    }
}

/// `tokens`（文全体、または文の中のサブクエリ範囲）を構造解析する。`depth` は 0 が文全体で、
/// 取り込み範囲ごとに 1 ずつ深くなる（サブクエリの入れ子上限の判定に使われる）。
/// `is_root` が偽のとき `WITH` 先頭は通常の問い合わせとして扱う（範囲はデータ変更 CTE の本文を
/// 単独では持たない）。エラーは下位の解析器のものをそのまま返す（文言の丸めは呼び出し側）。
pub(crate) fn parse_unit(
    tokens: &[Token],
    depth: usize,
    is_root: bool,
) -> Result<UnitSummary, SqlSurfaceError> {
    let limits = crate::nesting_limits::NestingLimits::default();
    let ledger = LedgerMode::default();
    let lookup = StructuralOnlyLookup;
    let summary = |kind| UnitSummary {
        kind,
        has_write_cte: false,
    };
    match head_word(tokens).as_str() {
        "INSERT" => {
            // 値の形・`INSERT ... SELECT` のどちらも構文は受理する（内側 SELECT は取り込み範囲として
            // 呼び出し側が再帰的に解析する）。
            let _shape: ParsedInsertTokens =
                allowlist::parse_insert_any_shape_tokens_with_limit(tokens, ledger, None)?;
            Ok(summary(UnitKind::Insert))
        }
        "UPDATE" if allowlist::looks_like_update_from(tokens) => {
            allowlist::validate_update_from_tokens_with_limits(
                tokens, ledger, false, limits, &lookup,
            )?;
            Ok(summary(UnitKind::Update))
        }
        "UPDATE" => {
            allowlist::parse_update_form_shape_tokens_with_limits(tokens, ledger, limits)?;
            Ok(summary(UnitKind::Update))
        }
        "DELETE" if allowlist::looks_like_delete_using(tokens) => {
            allowlist::validate_delete_using_tokens_with_limits(
                tokens, ledger, false, limits, &lookup,
            )?;
            Ok(summary(UnitKind::Delete))
        }
        "DELETE" => {
            allowlist::parse_delete_statement_shape_tokens_with_limits(tokens, ledger, limits)?;
            Ok(summary(UnitKind::Delete))
        }
        "WITH" if is_root => {
            match allowlist::validate_session_with_tokens(tokens, &lookup, depth, limits, ledger)? {
                SessionWith::Statement(_) => Ok(summary(UnitKind::Select)),
                SessionWith::DataModifying(_) => Ok(UnitSummary {
                    kind: UnitKind::Select,
                    has_write_cte: true,
                }),
            }
        }
        _ => {
            allowlist::validate_sql_tokens_with_subquery_ctx_limits(
                tokens, &lookup, depth, limits,
            )?;
            Ok(summary(UnitKind::Select))
        }
    }
}

/// 行数制限句（`FETCH FIRST`・`OFFSET ... ROWS`）と名前付き `WINDOW` を、解析器が内部で行うのと同じ
/// 正規形へ書き換える（どちらも冪等。トークン数が変わるため、位置の記録は正規化後の列に対して行う）。
/// `colref_sink` の記録区間の外で呼ぶ（`FETCH FIRST <変数> ROWS` の変数名だけは区間が必要なので、
/// 呼び出し側が `enter` した区間の下で呼ぶ）。
pub(crate) fn normalize_statement(tokens: Vec<Token>) -> Result<Vec<Token>, SqlSurfaceError> {
    let tokens = crate::sql::fetch_clause::normalize_statement_tokens(tokens)?;
    Ok(crate::sql::window_clause::expand_named_windows(&tokens)?.into_owned())
}
