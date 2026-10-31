//! PL/pgSQL の埋め込み SQL の定義時の構造解析を core 側で提供する（Issue #2224・SQL-68。ADR D2・D3）。
//!
//! `plpgsql::embedded::validate_embedded` が、変数を置換しない埋め込み SQL のトークン列（記録区間の下の
//! 位置の記録つき）を [`EmbeddedSqlChecker`] 経由で渡す。実体は `sql::embedded_unit::parse_unit`
//! （許可リスト解析をカタログ無しの構造専用で呼ぶ）で、カタログの状態に依存しない。未知の表・列・型
//! エラー（`42P01`・`42703` 等）は PostgreSQL と同じ late binding として先送りし、呼び出し時（#2226）の
//! 実値での完全な検証へ回す（その検証は必ず行われるため fail-open にならない）。
//!
//! 例外はデータ変更 CTE（`WITH d AS (DELETE ...) SELECT ...`）を含む最上位の `WITH` 文で、構造専用の
//! 解析では書き込みの実行形（副文の `RETURNING` 公開列・主クエリの形）を判定できない。この形だけは
//! カタログ付きの `EngineCore::parse_tokens` で解析し、解析木（`ParsedSql::DataModifyingCte`）から書き込みを
//! 確定する（表が未作成などで判定できなければ、書き込みの有無を決められないので呼び出し側が `0A000`
//! にする）。呼び出し元は `core::function_define`（`CREATE FUNCTION ... LANGUAGE plpgsql` の定義時検証。#2217）
//! と `DO` ブロック（#2228）。

use super::{EngineCore, ParsedSql};
use crate::plpgsql::embedded::{EmbeddedSqlChecker, StructuralChecker};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::colref_sink;
use crate::sql::embedded_unit::{UnitKind, UnitSummary};
use crate::sql::lexer::Token;

/// `( INSERT|UPDATE|DELETE` を含む `WITH` 始まりか。構造専用の解析で拒否されたときに、カタログ付きの
/// 解析へ回すかの**経路選択の目印**にだけ使う（書き込みの有無は解析木で決め、この目印では決めない。
/// 目印が外れても拒否側にしか倒れない）。
fn looks_like_data_modifying_with(tokens: &[Token]) -> bool {
    matches!(tokens.first(), Some(Token::Ident(w)) if w.eq_ignore_ascii_case("WITH"))
        && tokens.windows(2).any(|w| {
            matches!(w.first(), Some(Token::Punct('(')))
                && matches!(
                    w.get(1),
                    Some(Token::Ident(k))
                        if ["INSERT", "UPDATE", "DELETE"].iter().any(|x| k.eq_ignore_ascii_case(x))
                )
        })
}

impl EngineCore {
    /// データ変更 CTE を含む `WITH` 文を、カタログ付きの解析で解析木から分類する。`tokens` の複製は
    /// 位置の記録のため 1 対 1 の別名として登録する（複製が解析中に作り直されれば記録は範囲外になり、
    /// 呼び出し側が `0A000` にする）。
    fn check_data_modifying_with(&self, tokens: &[Token]) -> Result<UnitSummary, SqlSurfaceError> {
        let copy = tokens.to_vec();
        let _alias = colref_sink::register_alias(&copy, tokens);
        match self.parse_tokens(copy)? {
            ParsedSql::DataModifyingCte(_) => Ok(UnitSummary {
                kind: UnitKind::Select,
                has_write_cte: true,
            }),
            ParsedSql::Statement(_) => Ok(UnitSummary {
                kind: UnitKind::Select,
                has_write_cte: false,
            }),
            _ => Err(SqlSurfaceError::feature_not_supported(
                "unsupported WITH statement in a PL/pgSQL body",
            )),
        }
    }
}

impl EmbeddedSqlChecker for EngineCore {
    fn check_unit(
        &self,
        tokens: &[Token],
        depth: usize,
        is_root: bool,
    ) -> Result<UnitSummary, SqlSurfaceError> {
        let structural = StructuralChecker.check_unit(tokens, depth, is_root);
        if structural.is_err() && is_root && looks_like_data_modifying_with(tokens) {
            let _defer = crate::sql::const_expr::defer_all_non_syntax_errors();
            return self.check_data_modifying_with(tokens);
        }
        structural
    }
}
