//! サブクエリ深さ・サブクエリ実行回数・CTE 定義数・CTE 連鎖深さの上限の起動時設定
//! （Issue #1552。ポインタ: `docs/spec/04-behavior/` SQL-46）。
//!
//! `wire-server` の `nesting_limits_opt` が CLI フラグ（`--max-subquery-depth`・
//! `--max-subquery-executions`・`--max-cte-definitions`・`--max-cte-nesting-depth`）を
//! 厳密パースし、本モジュールの `validate_*` で範囲検証したうえで [`NestingLimits`] を
//! 組み立て、`EngineCore::with_nesting_limits` へ 1 回だけ注入する
//! （`group_by_limits`・`schema_limits` と同じ流儀）。範囲検証の単一情報源は本モジュール。
//!
//! 設計上の不変条件（`docs/design/nesting-limits-cli.md`）:
//!
//! - **ハード上限は現行定数のまま**（`sql::allowlist::MAX_SUBQUERY_DEPTH`・
//!   `sql::subquery::MAX_SUBQUERY_EXECUTIONS`・`sql::cte::MAX_CTE_DEFINITIONS`・
//!   `sql::cte::MAX_CTE_NESTING_DEPTH`）。設定できるのは「下げる方向」だけで、既定値は
//!   ハード上限と同じ（フラグ未指定で挙動不変）。深さは再帰とスタックに直結するため、
//!   引き上げは本機能の対象外。
//! - **設定値は構文解析段（サブクエリ深さ・CTE 定義数・CTE 連鎖深さ）とサブクエリ解決段
//!   （実行回数予算・内側の再検証）で判定する**。構造ガード（アロケーション上限・再帰ガード）は
//!   ハード定数のまま残す。

use crate::sql::allowlist::MAX_SUBQUERY_DEPTH;
use crate::sql::cte::{MAX_CTE_DEFINITIONS, MAX_CTE_NESTING_DEPTH};
use crate::sql::subquery::MAX_SUBQUERY_EXECUTIONS;

/// 4 上限に設定できる最小値。
pub const MIN_NESTING_LIMIT: usize = 1;

/// CTE 連鎖深さのハード上限（`u32`）を `usize` へ変換する。変換不能は起こり得ないが、
/// 失敗時は定数の現行値 4 へ倒す（fail-closed。`as` キャストは使わない）。
fn hard_cte_nesting_depth() -> usize {
    usize::try_from(MAX_CTE_NESTING_DEPTH).unwrap_or(4)
}

/// 4 上限の設定値。フィールドは非公開で、[`NestingLimits::new`] はハード上限で飽和させる
/// ため範囲外の値を保持しない。`Default` はハード上限と同値＝現行挙動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NestingLimits {
    max_subquery_depth: usize,
    max_subquery_executions: usize,
    max_cte_definitions: usize,
    max_cte_nesting_depth: usize,
}

impl Default for NestingLimits {
    fn default() -> Self {
        Self {
            max_subquery_depth: MAX_SUBQUERY_DEPTH,
            max_subquery_executions: MAX_SUBQUERY_EXECUTIONS,
            max_cte_definitions: MAX_CTE_DEFINITIONS,
            max_cte_nesting_depth: hard_cte_nesting_depth(),
        }
    }
}

impl NestingLimits {
    /// 検証済みの 4 値から組み立てる。直接渡された場合もハード上限で飽和させる（fail-closed）。
    pub fn new(
        max_subquery_depth: usize,
        max_subquery_executions: usize,
        max_cte_definitions: usize,
        max_cte_nesting_depth: usize,
    ) -> Self {
        Self {
            max_subquery_depth: max_subquery_depth.clamp(MIN_NESTING_LIMIT, MAX_SUBQUERY_DEPTH),
            max_subquery_executions: max_subquery_executions
                .clamp(MIN_NESTING_LIMIT, MAX_SUBQUERY_EXECUTIONS),
            max_cte_definitions: max_cte_definitions.clamp(MIN_NESTING_LIMIT, MAX_CTE_DEFINITIONS),
            max_cte_nesting_depth: max_cte_nesting_depth
                .clamp(MIN_NESTING_LIMIT, hard_cte_nesting_depth()),
        }
    }

    /// サブクエリ入れ子深さの上限。
    pub fn max_subquery_depth(&self) -> usize {
        self.max_subquery_depth
    }

    /// 1 文あたりの内側サブクエリ実行回数の上限。
    pub fn max_subquery_executions(&self) -> usize {
        self.max_subquery_executions
    }

    /// CTE 定義数の上限。
    pub fn max_cte_definitions(&self) -> usize {
        self.max_cte_definitions
    }

    /// CTE→CTE 連鎖深さの上限。
    pub fn max_cte_nesting_depth(&self) -> usize {
        self.max_cte_nesting_depth
    }
}

/// `validate_*` の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct NestingLimitOutOfRange {
    value: usize,
    max: usize,
}

impl std::fmt::Display for NestingLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "value {} is out of range 1..={}", self.value, self.max)
    }
}

impl std::error::Error for NestingLimitOutOfRange {}

fn validate_in(value: usize, max: usize) -> Result<usize, NestingLimitOutOfRange> {
    if (MIN_NESTING_LIMIT..=max).contains(&value) {
        Ok(value)
    } else {
        Err(NestingLimitOutOfRange { value, max })
    }
}

/// サブクエリ深さの設定値を `1..=MAX_SUBQUERY_DEPTH` で検証する単一情報源。
pub fn validate_max_subquery_depth(value: usize) -> Result<usize, NestingLimitOutOfRange> {
    validate_in(value, MAX_SUBQUERY_DEPTH)
}

/// サブクエリ実行回数の設定値を `1..=MAX_SUBQUERY_EXECUTIONS` で検証する単一情報源。
pub fn validate_max_subquery_executions(value: usize) -> Result<usize, NestingLimitOutOfRange> {
    validate_in(value, MAX_SUBQUERY_EXECUTIONS)
}

/// CTE 定義数の設定値を `1..=MAX_CTE_DEFINITIONS` で検証する単一情報源。
pub fn validate_max_cte_definitions(value: usize) -> Result<usize, NestingLimitOutOfRange> {
    validate_in(value, MAX_CTE_DEFINITIONS)
}

/// CTE 連鎖深さの設定値を `1..=MAX_CTE_NESTING_DEPTH` で検証する単一情報源。
pub fn validate_max_cte_nesting_depth(value: usize) -> Result<usize, NestingLimitOutOfRange> {
    validate_in(value, hard_cte_nesting_depth())
}

#[cfg(test)]
mod tests {
    use super::*;

    type Validator = fn(usize) -> Result<usize, NestingLimitOutOfRange>;

    #[test]
    fn default_equals_hard_limits() {
        let d = NestingLimits::default();
        assert_eq!(d.max_subquery_depth(), MAX_SUBQUERY_DEPTH);
        assert_eq!(d.max_subquery_executions(), MAX_SUBQUERY_EXECUTIONS);
        assert_eq!(d.max_cte_definitions(), MAX_CTE_DEFINITIONS);
        assert_eq!(d.max_cte_nesting_depth(), hard_cte_nesting_depth());
    }

    #[test]
    fn validators_accept_boundaries_and_reject_outside() {
        let cases: [(Validator, usize); 4] = [
            (validate_max_subquery_depth, MAX_SUBQUERY_DEPTH),
            (validate_max_subquery_executions, MAX_SUBQUERY_EXECUTIONS),
            (validate_max_cte_definitions, MAX_CTE_DEFINITIONS),
            (validate_max_cte_nesting_depth, hard_cte_nesting_depth()),
        ];
        for (f, max) in cases {
            assert_eq!(f(1), Ok(1));
            assert_eq!(f(max), Ok(max));
            assert!(f(0).is_err());
            assert!(f(max + 1).is_err());
        }
    }

    #[test]
    fn error_display_has_only_value_and_range() {
        let e = validate_max_subquery_depth(0).unwrap_err();
        assert_eq!(e.to_string(), "value 0 is out of range 1..=4");
    }

    #[test]
    fn new_saturates_to_hard_limits() {
        assert_eq!(
            NestingLimits::new(usize::MAX, usize::MAX, usize::MAX, usize::MAX),
            NestingLimits::default()
        );
        let l = NestingLimits::new(0, 0, 0, 0);
        assert_eq!(
            (
                l.max_subquery_depth(),
                l.max_subquery_executions(),
                l.max_cte_definitions(),
                l.max_cte_nesting_depth()
            ),
            (1, 1, 1, 1)
        );
    }
}
