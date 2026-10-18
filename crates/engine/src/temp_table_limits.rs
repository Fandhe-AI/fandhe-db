//! 一時表のセッションあたりの件数上限・総バイト数上限の起動時設定（TABLE-44・Issue #1683。
//! 設計の流儀は SQL-46 の `group_by_limits` と同じ）。
//!
//! `wire-server` の `temp_table_limits_opt` が CLI フラグ（`--max-temp-tables`・
//! `--max-temp-bytes`）を厳密パースし、本モジュールの `validate_*` で範囲検証したうえで
//! [`TempTableLimits`] を組み立て、`EngineCore::with_temp_table_limits` へ 1 回だけ注入する。
//! 範囲検証の単一情報源は本モジュール。実行段（`sql::ddl::execute_create_temp_table`）が
//! 件数は登録の前、バイト数は行ストアの生成時に渡す。
//!
//! 設計上の不変条件:
//!
//! - **ハード上限は既定値のまま**（`MAX_TEMP_TABLES_PER_SESSION`・`MAX_TEMP_BYTES_PER_SESSION`）。
//!   設定できるのは「下げる方向」だけで、既定値はハード上限と同じ（フラグ未指定で挙動不変）。
//!   一時表のメモリの最悪値（最大接続数 × セッション上限）を設定で増やさないため。
//! - 上限はセッション（接続）単位で、テナントをまたいで共有しない。

/// 1 セッションが持てる一時表の数のハード上限（かつ既定値）。
pub const MAX_TEMP_TABLES_PER_SESSION: usize = 64;

/// 1 セッションの一時表の論理総バイト数のハード上限（かつ既定値。64 MiB）。
pub const MAX_TEMP_BYTES_PER_SESSION: u64 = 64 * 1024 * 1024;

/// 2 上限に設定できる最小値。
pub const MIN_TEMP_TABLE_LIMIT: u64 = 1;

/// 一時表 2 上限の設定値。フィールドは非公開で、[`TempTableLimits::new`] はハード上限で
/// 飽和させるため範囲外の値を保持しない。`Default` はハード上限と同値＝現行挙動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TempTableLimits {
    max_tables_per_session: usize,
    max_bytes_per_session: u64,
}

impl Default for TempTableLimits {
    fn default() -> Self {
        Self {
            max_tables_per_session: MAX_TEMP_TABLES_PER_SESSION,
            max_bytes_per_session: MAX_TEMP_BYTES_PER_SESSION,
        }
    }
}

impl TempTableLimits {
    /// 検証済みの 2 値から組み立てる。直接渡された場合もハード上限で飽和させる（fail-closed）。
    pub fn new(max_tables_per_session: usize, max_bytes_per_session: u64) -> Self {
        Self {
            max_tables_per_session: max_tables_per_session
                .clamp(MIN_TEMP_TABLE_LIMIT as usize, MAX_TEMP_TABLES_PER_SESSION),
            max_bytes_per_session: max_bytes_per_session
                .clamp(MIN_TEMP_TABLE_LIMIT, MAX_TEMP_BYTES_PER_SESSION),
        }
    }

    /// 1 セッションが持てる一時表の数の上限。超過は `54000`。
    pub fn max_tables_per_session(&self) -> usize {
        self.max_tables_per_session
    }

    /// 1 セッションの一時表の論理総バイト数の上限。
    pub fn max_bytes_per_session(&self) -> u64 {
        self.max_bytes_per_session
    }
}

/// `validate_*` の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct TempTableLimitOutOfRange {
    value: u64,
    max: u64,
}

impl std::fmt::Display for TempTableLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "value {} is out of range 1..={}", self.value, self.max)
    }
}

impl std::error::Error for TempTableLimitOutOfRange {}

fn validate_in(value: u64, max: u64) -> Result<u64, TempTableLimitOutOfRange> {
    if (MIN_TEMP_TABLE_LIMIT..=max).contains(&value) {
        Ok(value)
    } else {
        Err(TempTableLimitOutOfRange { value, max })
    }
}

/// 件数上限の設定値を `1..=MAX_TEMP_TABLES_PER_SESSION` で検証する単一情報源。
pub fn validate_max_temp_tables(value: u64) -> Result<usize, TempTableLimitOutOfRange> {
    validate_in(value, MAX_TEMP_TABLES_PER_SESSION as u64).map(|v| v as usize)
}

/// 総バイト数上限の設定値を `1..=MAX_TEMP_BYTES_PER_SESSION` で検証する単一情報源。
pub fn validate_max_temp_bytes(value: u64) -> Result<u64, TempTableLimitOutOfRange> {
    validate_in(value, MAX_TEMP_BYTES_PER_SESSION)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limits() {
        let d = TempTableLimits::default();
        assert_eq!(d.max_tables_per_session(), MAX_TEMP_TABLES_PER_SESSION);
        assert_eq!(d.max_bytes_per_session(), MAX_TEMP_BYTES_PER_SESSION);
        assert_eq!(
            MAX_TEMP_TABLES_PER_SESSION,
            crate::sql::temp_catalog::DEFAULT_MAX_TEMP_TABLES_PER_SESSION
        );
        assert_eq!(
            MAX_TEMP_BYTES_PER_SESSION,
            crate::sql::temp_rows::DEFAULT_MAX_TEMP_BYTES_PER_SESSION
        );
    }

    #[test]
    fn validators_accept_boundaries_and_reject_outside() {
        assert_eq!(validate_max_temp_tables(1), Ok(1));
        assert_eq!(validate_max_temp_tables(64), Ok(64));
        assert!(validate_max_temp_tables(0).is_err());
        assert!(validate_max_temp_tables(65).is_err());
        assert_eq!(validate_max_temp_bytes(1), Ok(1));
        assert_eq!(
            validate_max_temp_bytes(MAX_TEMP_BYTES_PER_SESSION),
            Ok(MAX_TEMP_BYTES_PER_SESSION)
        );
        assert!(validate_max_temp_bytes(0).is_err());
        assert!(validate_max_temp_bytes(MAX_TEMP_BYTES_PER_SESSION + 1).is_err());
    }

    #[test]
    fn error_display_has_only_value_and_range() {
        let e = validate_max_temp_tables(0).unwrap_err();
        assert_eq!(e.to_string(), "value 0 is out of range 1..=64");
    }

    #[test]
    fn new_saturates_to_hard_limits() {
        assert_eq!(
            TempTableLimits::new(usize::MAX, u64::MAX),
            TempTableLimits::default()
        );
        let l = TempTableLimits::new(0, 0);
        assert_eq!(
            (l.max_tables_per_session(), l.max_bytes_per_session()),
            (1, 1)
        );
    }
}
