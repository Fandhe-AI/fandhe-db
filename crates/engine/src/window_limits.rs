//! ウィンドウ項目の `PARTITION BY`／`ORDER BY` キー数上限の起動時設定
//! （Issue #1900。ポインタ: `docs/spec/04-behavior/` SQL-46・SQL-64）。
//!
//! `wire-server` の `window_limits_opt` が CLI フラグ `--max-window-keys` を厳密パースし、
//! 本モジュールの [`validate_max_window_keys`] で範囲検証したうえで [`WindowLimits`] を
//! 組み立て、`EngineCore::with_window_limits` へ 1 回だけ注入する
//! （`nesting_limits`・`group_by_limits` と同じ流儀）。範囲検証の単一情報源は本モジュール。
//!
//! 設計上の不変条件（`docs/design/window-limits-cli.md`）:
//!
//! - **ハード上限は現行定数 `sql::allowlist::MAX_WINDOW_KEYS`（8）のまま**。設定できるのは
//!   「下げる方向」だけで、既定値はハード上限と同じ（フラグ未指定で挙動不変）。
//! - **設定値は構文解析段で判定する**（`PARTITION BY` と `ORDER BY` は 1 つの値を共有）。
//!   格納済みビュー本文の再パースはハード上限のまま残す。

use crate::sql::allowlist::MAX_WINDOW_KEYS;

/// 窓キー数上限に設定できる最小値。
pub const MIN_WINDOW_KEYS: usize = 1;

/// 窓キー数上限のハード上限（設定できる最大値＝既定値）。
pub const HARD_MAX_WINDOW_KEYS: usize = MAX_WINDOW_KEYS;

/// 窓キー数上限の設定値。フィールドは非公開で、[`WindowLimits::new`] はハード上限で
/// 飽和させるため範囲外の値を保持しない。`Default` はハード上限と同値＝現行挙動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowLimits {
    max_window_keys: usize,
}

impl Default for WindowLimits {
    fn default() -> Self {
        Self {
            max_window_keys: HARD_MAX_WINDOW_KEYS,
        }
    }
}

impl WindowLimits {
    /// 検証済みの値から組み立てる。直接渡された場合もハード上限で飽和させる（fail-closed）。
    pub fn new(max_window_keys: usize) -> Self {
        Self {
            max_window_keys: max_window_keys.clamp(MIN_WINDOW_KEYS, HARD_MAX_WINDOW_KEYS),
        }
    }

    /// `PARTITION BY`／`ORDER BY` 各句のキー数上限。
    pub fn max_window_keys(&self) -> usize {
        self.max_window_keys
    }
}

/// [`validate_max_window_keys`] の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct WindowLimitOutOfRange {
    value: usize,
    max: usize,
}

impl std::fmt::Display for WindowLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "value {} is out of range 1..={}", self.value, self.max)
    }
}

impl std::error::Error for WindowLimitOutOfRange {}

/// 窓キー数の設定値を `1..=HARD_MAX_WINDOW_KEYS` で検証する単一情報源。
pub fn validate_max_window_keys(value: usize) -> Result<usize, WindowLimitOutOfRange> {
    if (MIN_WINDOW_KEYS..=HARD_MAX_WINDOW_KEYS).contains(&value) {
        Ok(value)
    } else {
        Err(WindowLimitOutOfRange {
            value,
            max: HARD_MAX_WINDOW_KEYS,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limit() {
        assert_eq!(WindowLimits::default().max_window_keys(), 8);
    }

    #[test]
    fn validator_accepts_boundaries_and_rejects_outside() {
        assert_eq!(validate_max_window_keys(1), Ok(1));
        assert_eq!(validate_max_window_keys(8), Ok(8));
        assert!(validate_max_window_keys(0).is_err());
        assert!(validate_max_window_keys(9).is_err());
    }

    #[test]
    fn error_display_has_only_value_and_range() {
        let e = validate_max_window_keys(9).unwrap_err();
        assert_eq!(e.to_string(), "value 9 is out of range 1..=8");
    }

    #[test]
    fn new_saturates() {
        assert_eq!(WindowLimits::new(usize::MAX), WindowLimits::default());
        assert_eq!(WindowLimits::new(0).max_window_keys(), 1);
    }
}
