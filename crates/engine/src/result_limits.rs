//! 明示 `LIMIT` の範囲上限（結果行数上限）の起動時設定（Issue #1555。
//! ポインタ: `docs/spec/04-behavior/` SQL-34・SQL-46）。
//!
//! `wire-server` の `result_limits_opt` が CLI フラグ `--max-result-rows` を厳密パースし、
//! 本モジュールの [`validate_max_result_rows`] で範囲検証したうえで [`ResultLimits`] を
//! 組み立て、`EngineCore::with_result_limits` へ 1 回だけ注入する（`join_limits` と
//! 同じ流儀）。範囲検証の単一情報源は本モジュール。
//!
//! 設計上の不変条件（`docs/design/result-limits-cli.md`）:
//!
//! - 既定値は従来のコンパイル時定数と同値（10,000）で、フラグ未指定なら挙動不変。
//!   上げ下げ両方向に設定でき、ハード上限は [`MAX_RESULT_ROWS_HARD_CAP`]。
//! - 設定値は文の `LIMIT`（走査・kNN の `LIMIT k`・`FETCH n`・集合演算全体の `LIMIT`・
//!   JOIN 文の `LIMIT`・`USING PLAN` 前段検証・NoSQL の `limit`）の範囲判定にだけ効く。
//!   `OFFSET`・集合演算の枝行数・サブクエリ内側行数などの他上限とは連動させない
//!   （SQL-46 (6)）。
//! - 後続の結果行数上限（LIMIT 省略時の `54000`）も同じフラグ・同じ
//!   `EngineCore::result_limits()` を参照する想定。

/// 明示 `LIMIT` 上限の既定値（従来の `core::MAX_SEARCH_K` と同値）。
pub const DEFAULT_MAX_RESULT_ROWS: usize = 10_000;

/// 明示 `LIMIT` 上限に設定できる最大値（暫定。オーナー判断で変更可）。hybrid の
/// `MAX_POOL_DEPTH`（同値）・JOIN 出力行数のハード上限と桁を揃えた固定リテラルで、
/// 他定数への参照では連動させない。
pub const MAX_RESULT_ROWS_HARD_CAP: usize = 100_000;

/// 明示 `LIMIT` 上限に設定できる最小値。
pub const MIN_MAX_RESULT_ROWS: usize = 1;

/// 結果行数系上限の設定値。フィールドは非公開で、[`ResultLimits::new`] は範囲で飽和
/// させるため範囲外の値を保持しない。`Default` は従来定数と同値＝現行挙動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultLimits {
    max_result_rows: usize,
}

impl Default for ResultLimits {
    fn default() -> Self {
        Self {
            max_result_rows: DEFAULT_MAX_RESULT_ROWS,
        }
    }
}

impl ResultLimits {
    /// 検証済みの値から組み立てる。直接渡された場合も範囲で飽和させる（fail-closed）。
    pub fn new(max_result_rows: usize) -> Self {
        Self {
            max_result_rows: max_result_rows.clamp(MIN_MAX_RESULT_ROWS, MAX_RESULT_ROWS_HARD_CAP),
        }
    }

    /// 明示 `LIMIT` に許容する最大値。
    pub fn max_result_rows(&self) -> usize {
        self.max_result_rows
    }
}

/// [`validate_max_result_rows`] の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct ResultLimitOutOfRange {
    value: usize,
    min: usize,
    max: usize,
}

impl std::fmt::Display for ResultLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "value {} is out of range {}..={}",
            self.value, self.min, self.max
        )
    }
}

impl std::error::Error for ResultLimitOutOfRange {}

/// `--max-result-rows` の設定値を `1..=MAX_RESULT_ROWS_HARD_CAP` で検証する単一情報源。
pub fn validate_max_result_rows(value: usize) -> Result<usize, ResultLimitOutOfRange> {
    if (MIN_MAX_RESULT_ROWS..=MAX_RESULT_ROWS_HARD_CAP).contains(&value) {
        Ok(value)
    } else {
        Err(ResultLimitOutOfRange {
            value,
            min: MIN_MAX_RESULT_ROWS,
            max: MAX_RESULT_ROWS_HARD_CAP,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_legacy_constant() {
        assert_eq!(ResultLimits::default().max_result_rows(), 10_000);
        assert_eq!(DEFAULT_MAX_RESULT_ROWS, crate::core::MAX_SEARCH_K);
    }

    #[test]
    fn validator_accepts_boundaries_and_rejects_outside() {
        assert_eq!(validate_max_result_rows(1), Ok(1));
        assert_eq!(validate_max_result_rows(100_000), Ok(100_000));
        assert!(validate_max_result_rows(0).is_err());
        assert!(validate_max_result_rows(100_001).is_err());
    }

    #[test]
    fn new_saturates_into_range() {
        assert_eq!(ResultLimits::new(0).max_result_rows(), 1);
        assert_eq!(ResultLimits::new(50).max_result_rows(), 50);
        assert_eq!(ResultLimits::new(usize::MAX).max_result_rows(), 100_000);
    }

    #[test]
    fn error_display_has_value_and_range_only() {
        let msg = validate_max_result_rows(0).unwrap_err().to_string();
        assert_eq!(msg, "value 0 is out of range 1..=100000");
    }
}
