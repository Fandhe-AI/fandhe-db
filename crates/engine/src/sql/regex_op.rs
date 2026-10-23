//! 正規表現マッチ演算子 `~`／`~*`／`!~`／`!~*` の束縛補助（SQL-55・Issue #1787。ポインタ:
//! `docs/spec/04-behavior/sql-surface.md`）。
//!
//! 役割は 2 つ。(1) `EngineCore::regex_limits`（起動時 CLI で下げた上限）を文の束縛区間へ
//! 運ぶ thread-local の RAII スコープ（[`RegexLimitsScope`]。`sql::jsonpath_fn::JsonPathLimitsScope`
//! と同じ流儀）。(2) パターンのコンパイルを束縛時 1 回に集約するヘルパ
//! （[`compile_regex_pattern`]）。呼び出し元は `sql::udf_call` の `bind_expr_in`
//! （`Expr::RegexMatch` の束縛）で、評価器の本体は [`crate::regex_match`]。
//! エラーは [`SqlSurfaceError::Regex`] に写り、固定文言のみをクライアントへ返す。

use std::cell::Cell;

use crate::regex_limits::RegexLimits;
use crate::regex_match::{CompiledRegex, RegexCaseMode};
use crate::sql::allowlist::SqlSurfaceError;

thread_local! {
    /// 現在の束縛スコープの正規表現上限。未設定は `None`（ハード上限へフォールバック）。
    static CURRENT_LIMITS: Cell<Option<RegexLimits>> = const { Cell::new(None) };
}

/// `EngineCore::regex_limits` を現スレッドの束縛スコープへ設定する RAII ガード。
/// `core.rs` の文実行入口が保持している間だけ有効で、drop で直前の値へ戻す
/// （入れ子・panic でも他文脈へ漏れない）。プロセス大域の状態ではない。
pub(crate) struct RegexLimitsScope {
    prev: Option<RegexLimits>,
}

impl RegexLimitsScope {
    pub(crate) fn enter(limits: RegexLimits) -> Self {
        let prev = CURRENT_LIMITS.with(|c| c.replace(Some(limits)));
        RegexLimitsScope { prev }
    }
}

impl Drop for RegexLimitsScope {
    fn drop(&mut self) {
        CURRENT_LIMITS.with(|c| c.set(self.prev));
    }
}

/// 現在の束縛スコープの上限。スコープが無い経路はハード上限（CLI は下げることしかできない）。
pub(crate) fn current_regex_limits() -> RegexLimits {
    CURRENT_LIMITS.with(|c| c.get()).unwrap_or_default()
}

/// パターンを現在の上限でコンパイルする。`~*` 系は ASCII のみ大小無視。
/// 不正・非対応・上限超過は [`SqlSurfaceError::Regex`]（`2201B`／`0A000`／`54000`）。
pub(crate) fn compile_regex_pattern(
    pattern: &str,
    case_insensitive: bool,
) -> Result<CompiledRegex, SqlSurfaceError> {
    let case = if case_insensitive {
        RegexCaseMode::AsciiInsensitive
    } else {
        RegexCaseMode::Sensitive
    };
    CompiledRegex::compile(pattern, case, current_regex_limits()).map_err(SqlSurfaceError::Regex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_nests_and_restores() {
        let default = current_regex_limits();
        let small = RegexLimits::new(8, 8, 1024);
        let tiny = RegexLimits::new(2, 2, 1024);
        {
            let _outer = RegexLimitsScope::enter(small);
            assert_eq!(current_regex_limits(), small);
            {
                let _inner = RegexLimitsScope::enter(tiny);
                assert_eq!(current_regex_limits(), tiny);
            }
            assert_eq!(current_regex_limits(), small);
        }
        assert_eq!(current_regex_limits(), default);
    }

    #[test]
    fn compile_maps_errors_to_regex_variant() {
        assert!(compile_regex_pattern("a+", false).is_ok());
        assert!(matches!(
            compile_regex_pattern("(", false),
            Err(SqlSurfaceError::Regex(
                crate::regex_match::RegexError::InvalidPattern
            ))
        ));
        let _s = RegexLimitsScope::enter(RegexLimits::new(2, 8, 1024 * 1024));
        assert!(matches!(
            compile_regex_pattern("abc", true),
            Err(SqlSurfaceError::Regex(
                crate::regex_match::RegexError::PatternTooLong
            ))
        ));
    }
}
