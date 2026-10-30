//! PL/pgSQL の実行上限（Issue #2224・#2225。設計は `docs/design/plpgsql-subset.md` D7・SQL-68）。
//!
//! ハード上限の定数（構文段は #2224）と、起動時に下げる方向だけ設定できる [`PlpgsqlLimits`]
//! （#2225）を置く。`wire-server` の `plpgsql_limits_opt` が CLI フラグ
//! （`--max-plpgsql-statements` 等 5 本）を厳密パースし、本モジュールの `validate_*` で範囲検証した
//! うえで [`PlpgsqlLimits`] を組み立て、`EngineCore::with_plpgsql_limits` へ 1 回だけ注入する
//! （`recursive_cte_limits` と同じ流儀）。範囲検証の単一情報源は本モジュール。
//!
//! 不変条件: **ハード上限は既定値と同値**で、設定できるのは下げる方向だけ。構文解析器
//! （`plpgsql::parse_body`）は入れ子・本体長を、インタプリタ（`plpgsql::interp`）は文数・反復・
//! 呼び出し深さを参照する。全テナントで同じ値を使う（グローバル可変状態は持たない）。

/// 本体（原文）の最大バイト数。格納上限（`catalog::function`）と同値になるよう参照で共有する。
/// 字句解析の前に検査する（untrusted 入力に対する無制限確保の禁止）。
pub const HARD_MAX_PLPGSQL_BODY_BYTES: usize = crate::catalog::function::MAX_FUNCTION_BODY_BYTES;

/// ブロック・制御構文の入れ子の深さの上限（超過は `54001`）。再帰下降のスタック保護も兼ねる。
pub const HARD_MAX_PLPGSQL_NESTING_DEPTH: usize = 32;

/// 本体の走査で許容するトークン数の上限（超過は `54000`）。
pub(crate) const HARD_MAX_PLPGSQL_TOKENS: usize = 65_536;

/// 1 個の埋め込み文・式が参照できる異なる変数の数の上限（超過は `54000`。`sql::params` の
/// 1 文あたり番号上限と同値）。
pub(crate) const MAX_PLPGSQL_VARS_PER_STATEMENT: usize = crate::sql::params::MAX_PARAMS as usize;

/// 1 回の起動（トップレベル呼び出し）で実行できる文数のハード上限かつ既定値（超過は `54000`）。
pub const HARD_MAX_PLPGSQL_STATEMENTS: usize = 100_000;

/// 1 回の起動で数えるループ反復数（全ループの合計）のハード上限かつ既定値（超過は `54000`）。
pub const HARD_MAX_PLPGSQL_LOOP_ITERATIONS: usize = 1_000_000;

/// 関数呼び出しの深さのハード上限かつ既定値（超過は `54001`。#2218 と共有する）。
pub const HARD_MAX_FUNCTION_CALL_DEPTH: usize = 32;

/// 5 上限に設定できる最小値。
pub const MIN_PLPGSQL_LIMIT: usize = 1;

/// PL/pgSQL 実行上限の設定値。フィールドは非公開で、[`PlpgsqlLimits::new`] は範囲で飽和させる
/// ため範囲外の値を保持しない。`Default` はハード上限と同値＝既定挙動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlpgsqlLimits {
    max_statements: usize,
    max_loop_iterations: usize,
    max_nesting_depth: usize,
    max_body_bytes: usize,
    max_call_depth: usize,
}

impl Default for PlpgsqlLimits {
    fn default() -> Self {
        Self {
            max_statements: HARD_MAX_PLPGSQL_STATEMENTS,
            max_loop_iterations: HARD_MAX_PLPGSQL_LOOP_ITERATIONS,
            max_nesting_depth: HARD_MAX_PLPGSQL_NESTING_DEPTH,
            max_body_bytes: HARD_MAX_PLPGSQL_BODY_BYTES,
            max_call_depth: HARD_MAX_FUNCTION_CALL_DEPTH,
        }
    }
}

impl PlpgsqlLimits {
    /// 検証済みの 5 値から組み立てる。直接渡された場合も範囲で飽和させる（fail-closed）。
    pub fn new(
        max_statements: usize,
        max_loop_iterations: usize,
        max_nesting_depth: usize,
        max_body_bytes: usize,
        max_call_depth: usize,
    ) -> Self {
        Self {
            max_statements: max_statements.clamp(MIN_PLPGSQL_LIMIT, HARD_MAX_PLPGSQL_STATEMENTS),
            max_loop_iterations: max_loop_iterations
                .clamp(MIN_PLPGSQL_LIMIT, HARD_MAX_PLPGSQL_LOOP_ITERATIONS),
            max_nesting_depth: max_nesting_depth
                .clamp(MIN_PLPGSQL_LIMIT, HARD_MAX_PLPGSQL_NESTING_DEPTH),
            max_body_bytes: max_body_bytes.clamp(MIN_PLPGSQL_LIMIT, HARD_MAX_PLPGSQL_BODY_BYTES),
            max_call_depth: max_call_depth.clamp(MIN_PLPGSQL_LIMIT, HARD_MAX_FUNCTION_CALL_DEPTH),
        }
    }

    /// 1 回の起動で実行できる文数の上限。
    pub fn max_statements(&self) -> usize {
        self.max_statements
    }

    /// 1 回の起動で数えるループ反復数の上限。
    pub fn max_loop_iterations(&self) -> usize {
        self.max_loop_iterations
    }

    /// ブロック・制御構文の入れ子の深さの上限（構文段）。
    pub fn max_nesting_depth(&self) -> usize {
        self.max_nesting_depth
    }

    /// 本体（原文）の最大バイト数（構文段）。
    pub fn max_body_bytes(&self) -> usize {
        self.max_body_bytes
    }

    /// 関数呼び出しの深さの上限。
    pub fn max_call_depth(&self) -> usize {
        self.max_call_depth
    }
}

/// `validate_*` の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct PlpgsqlLimitOutOfRange {
    value: usize,
    min: usize,
    max: usize,
}

impl std::fmt::Display for PlpgsqlLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "value {} is out of range {}..={}",
            self.value, self.min, self.max
        )
    }
}

impl std::error::Error for PlpgsqlLimitOutOfRange {}

fn validate_in(value: usize, max: usize) -> Result<usize, PlpgsqlLimitOutOfRange> {
    if (MIN_PLPGSQL_LIMIT..=max).contains(&value) {
        Ok(value)
    } else {
        Err(PlpgsqlLimitOutOfRange {
            value,
            min: MIN_PLPGSQL_LIMIT,
            max,
        })
    }
}

/// 文数上限の設定値を `1..=HARD_MAX_PLPGSQL_STATEMENTS` で検証する単一情報源。
pub fn validate_max_plpgsql_statements(value: usize) -> Result<usize, PlpgsqlLimitOutOfRange> {
    validate_in(value, HARD_MAX_PLPGSQL_STATEMENTS)
}

/// ループ反復数上限の設定値を `1..=HARD_MAX_PLPGSQL_LOOP_ITERATIONS` で検証する単一情報源。
pub fn validate_max_plpgsql_loop_iterations(value: usize) -> Result<usize, PlpgsqlLimitOutOfRange> {
    validate_in(value, HARD_MAX_PLPGSQL_LOOP_ITERATIONS)
}

/// 入れ子の深さ上限の設定値を `1..=HARD_MAX_PLPGSQL_NESTING_DEPTH` で検証する単一情報源。
pub fn validate_max_plpgsql_nesting_depth(value: usize) -> Result<usize, PlpgsqlLimitOutOfRange> {
    validate_in(value, HARD_MAX_PLPGSQL_NESTING_DEPTH)
}

/// 本体バイト数上限の設定値を `1..=HARD_MAX_PLPGSQL_BODY_BYTES` で検証する単一情報源。
pub fn validate_max_plpgsql_body_bytes(value: usize) -> Result<usize, PlpgsqlLimitOutOfRange> {
    validate_in(value, HARD_MAX_PLPGSQL_BODY_BYTES)
}

/// 関数呼び出し深さ上限の設定値を `1..=HARD_MAX_FUNCTION_CALL_DEPTH` で検証する単一情報源。
pub fn validate_max_function_call_depth(value: usize) -> Result<usize, PlpgsqlLimitOutOfRange> {
    validate_in(value, HARD_MAX_FUNCTION_CALL_DEPTH)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limits() {
        let d = PlpgsqlLimits::default();
        assert_eq!(d.max_statements(), 100_000);
        assert_eq!(d.max_loop_iterations(), 1_000_000);
        assert_eq!(d.max_nesting_depth(), 32);
        assert_eq!(d.max_body_bytes(), HARD_MAX_PLPGSQL_BODY_BYTES);
        assert_eq!(d.max_call_depth(), 32);
    }

    #[test]
    fn validators_accept_boundaries_and_reject_outside() {
        assert_eq!(validate_max_plpgsql_statements(1), Ok(1));
        assert_eq!(validate_max_plpgsql_statements(100_000), Ok(100_000));
        assert!(validate_max_plpgsql_statements(0).is_err());
        assert!(validate_max_plpgsql_statements(100_001).is_err());
        assert!(validate_max_plpgsql_loop_iterations(1_000_001).is_err());
        assert_eq!(
            validate_max_plpgsql_loop_iterations(1_000_000),
            Ok(1_000_000)
        );
        assert!(validate_max_plpgsql_nesting_depth(33).is_err());
        assert_eq!(validate_max_plpgsql_nesting_depth(32), Ok(32));
        assert!(validate_max_plpgsql_body_bytes(0).is_err());
        assert!(validate_max_plpgsql_body_bytes(HARD_MAX_PLPGSQL_BODY_BYTES + 1).is_err());
        assert!(validate_max_function_call_depth(33).is_err());
        assert_eq!(validate_max_function_call_depth(1), Ok(1));
    }

    #[test]
    fn error_display_has_only_value_and_range() {
        let e = validate_max_function_call_depth(0).unwrap_err();
        assert_eq!(e.to_string(), "value 0 is out of range 1..=32");
    }

    #[test]
    fn new_saturates_to_range() {
        assert_eq!(
            PlpgsqlLimits::new(usize::MAX, usize::MAX, usize::MAX, usize::MAX, usize::MAX),
            PlpgsqlLimits::default()
        );
        let l = PlpgsqlLimits::new(0, 0, 0, 0, 0);
        assert_eq!(
            (
                l.max_statements(),
                l.max_loop_iterations(),
                l.max_nesting_depth(),
                l.max_body_bytes(),
                l.max_call_depth()
            ),
            (1, 1, 1, 1, 1)
        );
    }
}
