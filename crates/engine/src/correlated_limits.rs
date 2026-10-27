//! 相関サブクエリの再評価回数上限の起動時設定（Issue #1847。ポインタ: SQL-61、RLS-10）。
//!
//! `wire-server` の `correlated_limits_opt` が CLI フラグ `--max-correlated-reevaluations` を
//! 厳密パースし、本モジュールの [`validate_max_correlated_reevaluations`] で範囲検証したうえで
//! [`CorrelatedLimits`] を組み立て、`EngineCore::with_correlated_limits` へ起動時に 1 回だけ注入する
//! （`join_limits` と同じ流儀。JOIN 専用の `JoinSizeLimits` とは責務を分けるため別型）。
//!
//! 不変条件:
//! - ハード上限は [`MAX_CORRELATED_REEVALUATIONS`]（現行の定数値）のまま。設定できるのは
//!   「下げる方向」だけで、既定値はハード上限と同値（フラグ未指定で挙動不変）。
//! - 設定値は `sql::correlated_exists::CorrelatedCtx` が 1 文ごとに作る予算へ渡り、
//!   外側の行ごとの再評価（残余述語の評価・非集計形の相関スカラーの行引き当て）の累計で判定する。
//!   超過は `54000` で文全体を拒否する（部分結果を返さない。fail-closed）。

/// 相関サブクエリの再評価回数のハード上限（実装既定値。Issue #1842・SQL-61 ポインタ）。
/// 再評価 1 回 = 残余述語を評価する外側の行 1 行で、JOIN の入力行数上限と同じ桁に揃える。
pub const MAX_CORRELATED_REEVALUATIONS: usize = 100_000;

/// 再評価回数に設定できる最小値。
pub const MIN_CORRELATED_REEVALUATIONS: usize = 1;

/// 相関サブクエリの上限の設定値。フィールドは非公開で、[`CorrelatedLimits::new`] は範囲で
/// 飽和させるため範囲外の値を保持しない。`Default` はハード上限と同値＝現行挙動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorrelatedLimits {
    max_reevaluations: usize,
}

impl Default for CorrelatedLimits {
    fn default() -> Self {
        Self {
            max_reevaluations: MAX_CORRELATED_REEVALUATIONS,
        }
    }
}

impl CorrelatedLimits {
    /// 検証済みの値から組み立てる。直接渡された場合も範囲で飽和させる（fail-closed）。
    pub fn new(max_reevaluations: usize) -> Self {
        Self {
            max_reevaluations: max_reevaluations
                .clamp(MIN_CORRELATED_REEVALUATIONS, MAX_CORRELATED_REEVALUATIONS),
        }
    }

    /// 1 文あたりの再評価回数の上限。
    pub fn max_reevaluations(&self) -> usize {
        self.max_reevaluations
    }
}

/// [`validate_max_correlated_reevaluations`] の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct CorrelatedLimitOutOfRange {
    value: usize,
    min: usize,
    max: usize,
}

impl std::fmt::Display for CorrelatedLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "value {} is out of range {}..={}",
            self.value, self.min, self.max
        )
    }
}

impl std::error::Error for CorrelatedLimitOutOfRange {}

/// 再評価回数上限の設定値を `1..=MAX_CORRELATED_REEVALUATIONS` で検証する単一情報源。
pub fn validate_max_correlated_reevaluations(
    value: usize,
) -> Result<usize, CorrelatedLimitOutOfRange> {
    if (MIN_CORRELATED_REEVALUATIONS..=MAX_CORRELATED_REEVALUATIONS).contains(&value) {
        Ok(value)
    } else {
        Err(CorrelatedLimitOutOfRange {
            value,
            min: MIN_CORRELATED_REEVALUATIONS,
            max: MAX_CORRELATED_REEVALUATIONS,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limit() {
        assert_eq!(
            CorrelatedLimits::default().max_reevaluations(),
            MAX_CORRELATED_REEVALUATIONS
        );
        assert_eq!(MAX_CORRELATED_REEVALUATIONS, 100_000);
    }

    #[test]
    fn new_saturates_into_range() {
        assert_eq!(CorrelatedLimits::new(0).max_reevaluations(), 1);
        assert_eq!(CorrelatedLimits::new(7).max_reevaluations(), 7);
        assert_eq!(
            CorrelatedLimits::new(usize::MAX).max_reevaluations(),
            MAX_CORRELATED_REEVALUATIONS
        );
    }

    #[test]
    fn validate_accepts_boundaries_and_rejects_outside() {
        assert_eq!(validate_max_correlated_reevaluations(1), Ok(1));
        assert_eq!(validate_max_correlated_reevaluations(100_000), Ok(100_000));
        assert!(validate_max_correlated_reevaluations(0).is_err());
        assert!(validate_max_correlated_reevaluations(100_001).is_err());
        let msg = validate_max_correlated_reevaluations(0)
            .unwrap_err()
            .to_string();
        assert_eq!(msg, "value 0 is out of range 1..=100000");
    }
}
