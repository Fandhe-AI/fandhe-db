//! 1 文あたりの総走査行数上限（`tenant::MAX_SCANNED_ROWS`）の起動時設定
//! （Issue #1549。ポインタ: `docs/spec/04-behavior/` SQL-46）。
//!
//! `wire-server` の `scan_limits_opt` が CLI フラグ（`--max-scanned-rows`）を厳密
//! パースし、本モジュールの [`validate_max_scanned_rows`] で範囲検証したうえで
//! [`ScanLimits`] を組み立て、`EngineCore::with_scan_limits` へ 1 回だけ注入する
//! （`schema_limits`・`batch_limits` と同じ流儀）。範囲検証の単一情報源は本モジュール。
//!
//! 設計上の不変条件（`docs/design/scan-limits-cli.md`）:
//!
//! - **ハード上限は現行定数のまま**（[`HARD_MAX_SCANNED_ROWS`] ＝
//!   `tenant::MAX_SCANNED_ROWS` = 1,000,000）。設定できるのは「下げる方向」だけで、
//!   既定値はハード上限と同じ（フラグ未指定で挙動不変）。引き上げ・全行走査の許可は
//!   DoS 面を広げるため本機能の対象外。
//! - 設定値は原子経路（1 文 1 txn）の述語形 UPDATE／DELETE の候補列挙・辞書スナップ
//!   ショット構築・ファイル形 INSERT の置換走査にだけ効く。分割実行 DML のチャンク
//!   走査予算（`--partitioned-dml-scan-budget`）とは連動させず、その範囲上限も
//!   ハード上限のまま。

use crate::tenant::MAX_SCANNED_ROWS;

/// 設定できる最小値。
pub const MIN_MAX_SCANNED_ROWS: usize = 1;

/// 設定できる最大値（ハード上限）。実体は `tenant::MAX_SCANNED_ROWS` の 1 か所。
pub const HARD_MAX_SCANNED_ROWS: usize = MAX_SCANNED_ROWS;

/// 総走査行数上限の設定値。フィールドは非公開で、[`ScanLimits::new`] はハード上限で
/// 飽和させるため範囲外の値を保持しない。`Default` はハード上限＝現行挙動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanLimits {
    max_scanned_rows: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_scanned_rows: HARD_MAX_SCANNED_ROWS,
        }
    }
}

impl ScanLimits {
    /// 検証済みの値から組み立てる。直接渡された範囲外の値もハード上限で飽和させる
    /// （fail-closed）。
    pub fn new(max_scanned_rows: usize) -> Self {
        Self {
            max_scanned_rows: max_scanned_rows.clamp(MIN_MAX_SCANNED_ROWS, HARD_MAX_SCANNED_ROWS),
        }
    }

    /// 1 文あたりの総走査行数上限（可視・不可視を問わず対象テナント所有行を数える）。
    pub fn max_scanned_rows(&self) -> usize {
        self.max_scanned_rows
    }
}

/// [`validate_max_scanned_rows`] の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct ScanLimitOutOfRange {
    value: usize,
}

impl std::fmt::Display for ScanLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "value {} is out of range {}..={}",
            self.value, MIN_MAX_SCANNED_ROWS, HARD_MAX_SCANNED_ROWS
        )
    }
}

impl std::error::Error for ScanLimitOutOfRange {}

/// 設定値を `1..=HARD_MAX_SCANNED_ROWS` で検証する単一情報源。
pub fn validate_max_scanned_rows(value: usize) -> Result<usize, ScanLimitOutOfRange> {
    if (MIN_MAX_SCANNED_ROWS..=HARD_MAX_SCANNED_ROWS).contains(&value) {
        Ok(value)
    } else {
        Err(ScanLimitOutOfRange { value })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limit() {
        assert_eq!(ScanLimits::default().max_scanned_rows(), 1_000_000);
    }

    #[test]
    fn validator_accepts_boundaries_and_rejects_outside() {
        assert_eq!(validate_max_scanned_rows(1), Ok(1));
        assert_eq!(validate_max_scanned_rows(1_000_000), Ok(1_000_000));
        assert!(validate_max_scanned_rows(0).is_err());
        assert!(validate_max_scanned_rows(1_000_001).is_err());
        assert!(validate_max_scanned_rows(usize::MAX).is_err());
    }

    #[test]
    fn error_display_has_only_value_and_range() {
        let e = validate_max_scanned_rows(0).unwrap_err();
        assert_eq!(e.to_string(), "value 0 is out of range 1..=1000000");
    }

    #[test]
    fn new_saturates() {
        assert_eq!(ScanLimits::new(usize::MAX), ScanLimits::default());
        assert_eq!(ScanLimits::new(0).max_scanned_rows(), 1);
    }
}
