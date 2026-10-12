//! `GROUP BY` の列数上限・グループ数上限の起動時設定（Issue #1550。
//! ポインタ: `docs/spec/04-behavior/` SQL-46）。
//!
//! `wire-server` の `group_by_limits_opt` が CLI フラグ（`--max-group-by-columns`・
//! `--max-groups`）を厳密パースし、本モジュールの `validate_*` で範囲検証したうえで
//! [`GroupByLimits`] を組み立て、`EngineCore::with_group_by_limits` へ 1 回だけ注入する
//! （`schema_limits`・`sql::parser::DmlLimits` と同じ流儀）。範囲検証の単一情報源は本モジュール。
//!
//! 設計上の不変条件（`docs/design/group-by-limits-cli.md`）:
//!
//! - **ハード上限は現行定数のまま**（`sql::allowlist::MAX_GROUP_BY_COLUMNS`・
//!   `sql::group_by::MAX_GROUPS`）。設定できるのは「下げる方向」だけで、既定値は
//!   ハード上限と同じ（フラグ未指定で挙動不変）。列数は実行段の固定長スタック配列長、
//!   グループ数はウィンドウ・集計 `LIMIT` 範囲・結果バイト見積りの前提でもあるため、
//!   引き上げは本機能の対象外。
//! - **設定値は実行段（`sql::group_by`・JOIN 集計）でだけ判定する**。構文段・束縛段の
//!   ハード上限検査は残す。

use crate::sql::allowlist::MAX_GROUP_BY_COLUMNS;
use crate::sql::group_by::MAX_GROUPS;

/// 2 上限に設定できる最小値。
pub const MIN_GROUP_BY_LIMIT: usize = 1;

/// `GROUP BY` 2 上限の設定値。フィールドは非公開で、[`GroupByLimits::new`] は
/// ハード上限で飽和させるため範囲外の値を保持しない。`Default` はハード上限と同値＝現行挙動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GroupByLimits {
    max_group_by_columns: usize,
    max_groups: usize,
}

impl Default for GroupByLimits {
    fn default() -> Self {
        Self {
            max_group_by_columns: MAX_GROUP_BY_COLUMNS,
            max_groups: MAX_GROUPS,
        }
    }
}

impl GroupByLimits {
    /// 検証済みの 2 値から組み立てる。直接渡された場合もハード上限で飽和させる（fail-closed）。
    pub fn new(max_group_by_columns: usize, max_groups: usize) -> Self {
        Self {
            max_group_by_columns: max_group_by_columns
                .clamp(MIN_GROUP_BY_LIMIT, MAX_GROUP_BY_COLUMNS),
            max_groups: max_groups.clamp(MIN_GROUP_BY_LIMIT, MAX_GROUPS),
        }
    }

    /// `GROUP BY` キー列数の上限。
    pub fn max_group_by_columns(&self) -> usize {
        self.max_group_by_columns
    }

    /// 1 クエリが生成できるグループ数の上限。
    pub fn max_groups(&self) -> usize {
        self.max_groups
    }
}

/// `validate_*` の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct GroupByLimitOutOfRange {
    value: usize,
    max: usize,
}

impl std::fmt::Display for GroupByLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "value {} is out of range 1..={}", self.value, self.max)
    }
}

impl std::error::Error for GroupByLimitOutOfRange {}

fn validate_in(value: usize, max: usize) -> Result<usize, GroupByLimitOutOfRange> {
    if (MIN_GROUP_BY_LIMIT..=max).contains(&value) {
        Ok(value)
    } else {
        Err(GroupByLimitOutOfRange { value, max })
    }
}

/// 列数上限の設定値を `1..=MAX_GROUP_BY_COLUMNS` で検証する単一情報源。
pub fn validate_max_group_by_columns(value: usize) -> Result<usize, GroupByLimitOutOfRange> {
    validate_in(value, MAX_GROUP_BY_COLUMNS)
}

/// グループ数上限の設定値を `1..=MAX_GROUPS` で検証する単一情報源。
pub fn validate_max_groups(value: usize) -> Result<usize, GroupByLimitOutOfRange> {
    validate_in(value, MAX_GROUPS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limits() {
        let d = GroupByLimits::default();
        assert_eq!(d.max_group_by_columns(), MAX_GROUP_BY_COLUMNS);
        assert_eq!(d.max_groups(), MAX_GROUPS);
    }

    #[test]
    fn validators_accept_boundaries_and_reject_outside() {
        assert_eq!(validate_max_group_by_columns(1), Ok(1));
        assert_eq!(
            validate_max_group_by_columns(MAX_GROUP_BY_COLUMNS),
            Ok(MAX_GROUP_BY_COLUMNS)
        );
        assert!(validate_max_group_by_columns(0).is_err());
        assert!(validate_max_group_by_columns(MAX_GROUP_BY_COLUMNS + 1).is_err());
        assert_eq!(validate_max_groups(1), Ok(1));
        assert_eq!(validate_max_groups(MAX_GROUPS), Ok(MAX_GROUPS));
        assert!(validate_max_groups(0).is_err());
        assert!(validate_max_groups(MAX_GROUPS + 1).is_err());
    }

    #[test]
    fn error_display_has_only_value_and_range() {
        let e = validate_max_group_by_columns(0).unwrap_err();
        assert_eq!(e.to_string(), "value 0 is out of range 1..=8");
    }

    #[test]
    fn new_saturates_to_hard_limits() {
        assert_eq!(
            GroupByLimits::new(usize::MAX, usize::MAX),
            GroupByLimits::default()
        );
        let l = GroupByLimits::new(0, 0);
        assert_eq!((l.max_group_by_columns(), l.max_groups()), (1, 1));
    }
}
