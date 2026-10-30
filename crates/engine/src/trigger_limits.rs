//! トリガの入れ子（再帰）深さ上限の起動時設定（Issue #1727。ポインタ: `docs/spec/04-behavior/`
//! TABLE-45 (5)・SQL-46）。
//!
//! `wire-server` の `trigger_limits_opt` が CLI フラグ `--max-trigger-depth` を厳密パースし、
//! 本モジュールの [`validate_max_trigger_depth`] で範囲検証したうえで [`TriggerLimits`] を
//! 組み立て、`EngineCore::with_trigger_limits` へ 1 回だけ注入する（`window_limits` と同じ流儀）。
//! 範囲検証の単一情報源は本モジュール。検査点は `trigger::runtime::TriggerRuntime::invoke`
//! （`trigger::depth` のガード）で、超過は `54001` で文全体を拒否する。
//!
//! 設計上の不変条件（`docs/design/trigger-depth-limit-cli.md`）:
//!
//! - **ハード上限は 16**（実装既定値。既定＝ハード上限）。設定できるのは「下げる方向」だけ。
//! - 最小値は 1（外側の文が直接発火したトリガ呼び出しが深さ 1）。設定でトリガを黙って無効化できない。
//! - 変更手段は起動時の CLI のみ。セッション変数・SQL・HTTP からは変えられない。

/// 深さ上限に設定できる最小値（入れ子なしでトリガは動く）。
pub const MIN_TRIGGER_DEPTH: usize = 1;

/// 深さ上限のハード上限（設定できる最大値＝既定値）。
pub const HARD_MAX_TRIGGER_DEPTH: usize = 16;

/// トリガ深さ上限の設定値。フィールドは非公開で、[`TriggerLimits::new`] は範囲で飽和させる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TriggerLimits {
    max_trigger_depth: usize,
}

impl Default for TriggerLimits {
    fn default() -> Self {
        Self {
            max_trigger_depth: HARD_MAX_TRIGGER_DEPTH,
        }
    }
}

impl TriggerLimits {
    /// 検証済みの値から組み立てる。直接渡された場合も範囲で飽和させる（fail-closed）。
    pub fn new(max_trigger_depth: usize) -> Self {
        Self {
            max_trigger_depth: max_trigger_depth.clamp(MIN_TRIGGER_DEPTH, HARD_MAX_TRIGGER_DEPTH),
        }
    }

    /// トリガ呼び出しの入れ子の深さ上限。
    pub fn max_trigger_depth(&self) -> usize {
        self.max_trigger_depth
    }
}

/// [`validate_max_trigger_depth`] の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct TriggerLimitOutOfRange {
    value: usize,
    max: usize,
}

impl std::fmt::Display for TriggerLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "value {} is out of range 1..={}", self.value, self.max)
    }
}

impl std::error::Error for TriggerLimitOutOfRange {}

/// 深さ上限の設定値を `1..=HARD_MAX_TRIGGER_DEPTH` で検証する単一情報源。
pub fn validate_max_trigger_depth(value: usize) -> Result<usize, TriggerLimitOutOfRange> {
    if (MIN_TRIGGER_DEPTH..=HARD_MAX_TRIGGER_DEPTH).contains(&value) {
        Ok(value)
    } else {
        Err(TriggerLimitOutOfRange {
            value,
            max: HARD_MAX_TRIGGER_DEPTH,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limit() {
        assert_eq!(TriggerLimits::default().max_trigger_depth(), 16);
    }

    #[test]
    fn validator_accepts_boundaries_and_rejects_outside() {
        assert_eq!(validate_max_trigger_depth(1), Ok(1));
        assert_eq!(validate_max_trigger_depth(16), Ok(16));
        assert!(validate_max_trigger_depth(0).is_err());
        assert!(validate_max_trigger_depth(17).is_err());
    }

    #[test]
    fn error_display_has_only_value_and_range() {
        let e = validate_max_trigger_depth(17).unwrap_err();
        assert_eq!(e.to_string(), "value 17 is out of range 1..=16");
    }

    #[test]
    fn new_saturates() {
        assert_eq!(TriggerLimits::new(usize::MAX), TriggerLimits::default());
        assert_eq!(TriggerLimits::new(0).max_trigger_depth(), 1);
    }
}
