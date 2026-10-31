//! UDF（`CREATE FUNCTION`）の件数上限の起動時設定（SQL-66 (5)・Issue #1922。
//! 設計の流儀は SQL-46 の `temp_table_limits` と同じ）。
//!
//! `wire-server` の `function_limits_opt` が CLI フラグ（`--max-functions`）を厳密パースし、
//! 本モジュールの [`validate_max_functions`] で範囲検証したうえで [`FunctionLimits`] を組み立て、
//! `EngineCore::with_function_limits` へ 1 回だけ注入する。範囲検証の単一情報源は本モジュール。
//! 実行段は `CREATE FUNCTION` の件数判定（`sql::udf_ddl` 経由のカタログ件数、事前検証の
//! レジストリ複製）へ同じ値を渡す。DDL 権限の無いセッションは判定の前に `42501` になる。
//!
//! 設計上の不変条件:
//!
//! - **ハード上限は既定値のまま**（`catalog::function::MAX_FUNCTIONS`・
//!   `sql::udf_call::MAX_SESSION_UDFS`。両者は等しい）。設定できるのは「下げる方向」だけ。
//!   カタログの読み取り側はハード上限超過を `CorruptSchema` として fail-closed に扱うため、
//!   上げる方向を許すと正常な DB が破損扱いになる。
//! - 上限は書き込み（`CREATE FUNCTION`）にだけ適用する。起動時・セッション開始時のロードは
//!   ハード上限で判定し、上限を下げて再起動しても既存の関数は読める・呼べる（SQL-46 (5)）。
//! - 値は全テナントで同じ。件数はテナント scope ごとに数える。

/// 1 テナント（またはセッション）が持てる関数数のハード上限（かつ既定値）。
pub const MAX_FUNCTIONS_PER_SCOPE: usize = 64;

/// 設定できる最小値。
pub const MIN_FUNCTION_LIMIT: u64 = 1;

/// 関数数上限の設定値。フィールドは非公開で、[`FunctionLimits::new`] はハード上限で飽和させるため
/// 範囲外の値を保持しない。`Default` はハード上限と同値＝現行挙動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FunctionLimits {
    max_functions: usize,
}

impl Default for FunctionLimits {
    fn default() -> Self {
        Self {
            max_functions: MAX_FUNCTIONS_PER_SCOPE,
        }
    }
}

impl FunctionLimits {
    /// 検証済みの値から組み立てる。直接渡された場合も `1..=ハード上限` で飽和させる（fail-closed）。
    pub fn new(max_functions: usize) -> Self {
        Self {
            max_functions: max_functions
                .clamp(MIN_FUNCTION_LIMIT as usize, MAX_FUNCTIONS_PER_SCOPE),
        }
    }

    /// 関数数の上限。超過は `54000`。
    pub fn max_functions(&self) -> usize {
        self.max_functions
    }
}

/// [`validate_max_functions`] の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct FunctionLimitOutOfRange {
    value: u64,
}

impl std::fmt::Display for FunctionLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "value {} is out of range 1..={}",
            self.value, MAX_FUNCTIONS_PER_SCOPE
        )
    }
}

impl std::error::Error for FunctionLimitOutOfRange {}

/// 設定値を `1..=MAX_FUNCTIONS_PER_SCOPE` で検証する単一情報源。
pub fn validate_max_functions(value: u64) -> Result<usize, FunctionLimitOutOfRange> {
    if (MIN_FUNCTION_LIMIT..=MAX_FUNCTIONS_PER_SCOPE as u64).contains(&value) {
        Ok(value as usize)
    } else {
        Err(FunctionLimitOutOfRange { value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limits() {
        assert_eq!(
            FunctionLimits::default().max_functions(),
            MAX_FUNCTIONS_PER_SCOPE
        );
        assert_eq!(
            MAX_FUNCTIONS_PER_SCOPE,
            crate::sql::udf_call::MAX_SESSION_UDFS
        );
        assert_eq!(
            MAX_FUNCTIONS_PER_SCOPE,
            crate::catalog::function::MAX_FUNCTIONS
        );
    }

    #[test]
    fn new_clamps_into_range() {
        assert_eq!(FunctionLimits::new(0).max_functions(), 1);
        assert_eq!(FunctionLimits::new(2).max_functions(), 2);
        assert_eq!(FunctionLimits::new(10_000).max_functions(), 64);
    }

    #[test]
    fn validator_accepts_boundaries_and_rejects_outside() {
        assert_eq!(validate_max_functions(1), Ok(1));
        assert_eq!(validate_max_functions(64), Ok(64));
        assert!(validate_max_functions(0).is_err());
        assert!(validate_max_functions(65).is_err());
        assert!(validate_max_functions(u64::MAX).is_err());
    }
}
