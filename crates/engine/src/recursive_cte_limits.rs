//! 再帰 CTE の反復回数・累積行数上限の起動時設定（Issue #1871。
//! ポインタ: `docs/spec/04-behavior/` SQL-35 (3)・SQL-46）。
//!
//! `wire-server` の `recursive_cte_limits_opt` が CLI フラグ
//! （`--max-recursive-cte-iterations`・`--max-recursive-cte-rows`）を厳密パースし、本モジュールの
//! `validate_*` で範囲検証したうえで [`RecursiveCteLimits`] を組み立て、
//! `EngineCore::with_recursive_cte_limits` へ 1 回だけ注入する（`join_limits` と同じ流儀）。
//! 範囲検証の単一情報源は本モジュール。内部実行用の `sql::recursive_eval::RecursiveLimits` とは別型。
//!
//! 設計上の不変条件:
//!
//! - **ハード上限は既定値と同値**（反復 1,000 回・累積 100,000 行）。設定できるのは
//!   「下げる方向」だけで、フラグ未指定なら挙動は変わらない。
//! - 上限超過は部分結果を返さず文全体を `54000` で拒否する（fail-closed。評価器側の責務）。
//! - 累積行数の実効上限は `--max-result-rows`（SQL-34）との小さい方（評価器側で合成）。

/// 反復回数（再帰項の実行回数）のハード上限かつ既定値。
pub const MAX_RECURSIVE_CTE_ITERATIONS: usize = 1_000;

/// 累積行数（アンカー行を含む全反復の合計）のハード上限かつ既定値。
pub const MAX_RECURSIVE_CTE_ROWS: usize = 100_000;

/// 両上限に設定できる最小値。
pub const MIN_RECURSIVE_CTE_LIMIT: usize = 1;

/// 再帰 CTE の 2 上限の設定値。フィールドは非公開で、[`RecursiveCteLimits::new`] は範囲で
/// 飽和させるため範囲外の値を保持しない。`Default` はハード上限と同値＝現行挙動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecursiveCteLimits {
    max_iterations: usize,
    max_rows: usize,
}

impl Default for RecursiveCteLimits {
    fn default() -> Self {
        Self {
            max_iterations: MAX_RECURSIVE_CTE_ITERATIONS,
            max_rows: MAX_RECURSIVE_CTE_ROWS,
        }
    }
}

impl RecursiveCteLimits {
    /// 検証済みの 2 値から組み立てる。直接渡された場合も範囲で飽和させる（fail-closed）。
    pub fn new(max_iterations: usize, max_rows: usize) -> Self {
        Self {
            max_iterations: max_iterations
                .clamp(MIN_RECURSIVE_CTE_LIMIT, MAX_RECURSIVE_CTE_ITERATIONS),
            max_rows: max_rows.clamp(MIN_RECURSIVE_CTE_LIMIT, MAX_RECURSIVE_CTE_ROWS),
        }
    }

    /// 再帰項の実行回数の上限。
    pub fn max_iterations(&self) -> usize {
        self.max_iterations
    }

    /// 累積行数の上限。
    pub fn max_rows(&self) -> usize {
        self.max_rows
    }
}

/// `validate_*` の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct RecursiveCteLimitOutOfRange {
    value: usize,
    min: usize,
    max: usize,
}

impl std::fmt::Display for RecursiveCteLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "value {} is out of range {}..={}",
            self.value, self.min, self.max
        )
    }
}

impl std::error::Error for RecursiveCteLimitOutOfRange {}

fn validate_in(value: usize, min: usize, max: usize) -> Result<usize, RecursiveCteLimitOutOfRange> {
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(RecursiveCteLimitOutOfRange { value, min, max })
    }
}

/// 反復回数上限の設定値を `1..=MAX_RECURSIVE_CTE_ITERATIONS` で検証する単一情報源。
pub fn validate_max_recursive_cte_iterations(
    value: usize,
) -> Result<usize, RecursiveCteLimitOutOfRange> {
    validate_in(value, MIN_RECURSIVE_CTE_LIMIT, MAX_RECURSIVE_CTE_ITERATIONS)
}

/// 累積行数上限の設定値を `1..=MAX_RECURSIVE_CTE_ROWS` で検証する単一情報源。
pub fn validate_max_recursive_cte_rows(value: usize) -> Result<usize, RecursiveCteLimitOutOfRange> {
    validate_in(value, MIN_RECURSIVE_CTE_LIMIT, MAX_RECURSIVE_CTE_ROWS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limits() {
        let d = RecursiveCteLimits::default();
        assert_eq!(d.max_iterations(), 1_000);
        assert_eq!(d.max_rows(), 100_000);
    }

    #[test]
    fn validators_accept_boundaries_and_reject_outside() {
        assert_eq!(validate_max_recursive_cte_iterations(1), Ok(1));
        assert_eq!(validate_max_recursive_cte_iterations(1_000), Ok(1_000));
        assert!(validate_max_recursive_cte_iterations(0).is_err());
        assert!(validate_max_recursive_cte_iterations(1_001).is_err());
        assert_eq!(validate_max_recursive_cte_rows(1), Ok(1));
        assert_eq!(validate_max_recursive_cte_rows(100_000), Ok(100_000));
        assert!(validate_max_recursive_cte_rows(0).is_err());
        assert!(validate_max_recursive_cte_rows(100_001).is_err());
    }

    #[test]
    fn error_display_has_only_value_and_range() {
        let e = validate_max_recursive_cte_iterations(0).unwrap_err();
        assert_eq!(e.to_string(), "value 0 is out of range 1..=1000");
    }

    #[test]
    fn new_saturates_to_range() {
        assert_eq!(
            RecursiveCteLimits::new(usize::MAX, usize::MAX),
            RecursiveCteLimits::default()
        );
        let l = RecursiveCteLimits::new(0, 0);
        assert_eq!((l.max_iterations(), l.max_rows()), (1, 1));
        let m = RecursiveCteLimits::new(10, 50);
        assert_eq!((m.max_iterations(), m.max_rows()), (10, 50));
    }
}
