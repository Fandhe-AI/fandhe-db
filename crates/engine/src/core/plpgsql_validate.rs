//! PL/pgSQL の埋め込み SQL を定義時に構文検証する core 側の実装（Issue #2224・SQL-68。ADR D2・D3）。
//!
//! `plpgsql::embedded::validate_embedded` が、変数をダミーへ置換したトークン列を
//! [`EmbeddedSqlChecker`] 経由で渡す。ここでは既存の許可リスト解析（`EngineCore::parse_tokens`）へ
//! 委譲し、定義時に拒否すべき失敗（`42601`／`0A000`／`54000`／`54001`）だけを伝播する。
//! 未知の表・列・型エラー（`42P01`・`42703` 等）は PostgreSQL と同じ late binding として
//! 握りつぶし、呼び出し時（#2226）の実値での完全な検証へ回す（その検証は必ず行われるため
//! fail-open にならない）。呼び出し元は #2217／#2225 が結線するまでテストだけ。

use super::EngineCore;
use crate::plpgsql::embedded::EmbeddedSqlChecker;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::lexer::Token;

impl EmbeddedSqlChecker for EngineCore {
    fn check_syntax(&self, tokens: Vec<Token>) -> Result<(), SqlSurfaceError> {
        // 構文エラー以外の畳み込みエラー（0 除算等）は無視して文全体を走査させる。
        let _guard = crate::sql::const_expr::defer_all_non_syntax_errors();
        match self.parse_tokens(tokens) {
            Ok(_) => Ok(()),
            Err(e) => match e.wire_code() {
                "42601" | "0A000" | "54000" | "54001" => Err(e),
                _ => Ok(()),
            },
        }
    }
}
