//! スキーマ定義系 3 上限（1 テーブルの列数・配列列 1 個の要素数・ENUM 型 1 個の
//! ラベル数）の起動時設定（Issue #1548。ポインタ: `docs/spec/04-behavior/` SQL-46）。
//!
//! `wire-server` の `schema_limits_opt` が CLI フラグ（`--max-table-columns`・
//! `--max-array-elements`・`--max-enum-labels`）を厳密パースし、本モジュールの
//! `validate_*` で範囲検証したうえで [`SchemaLimits`] を組み立て、
//! `EngineCore::with_schema_limits` へ 1 回だけ注入する（`batch_limits`・
//! `sql::parser::DmlLimits` と同じ流儀）。範囲検証の単一情報源は本モジュール。
//!
//! 設計上の不変条件（`docs/design/schema-limits-cli.md`）:
//!
//! - **ハード上限は現行定数のまま**（`catalog::MAX_COLUMN_COUNT`・
//!   `catalog::MAX_ARRAY_ELEMENTS`・`catalog::MAX_ENUM_LABELS`）。設定できるのは
//!   「下げる方向」だけで、既定値はハード上限と同じ（フラグ未指定で挙動不変）。
//!   これらは decode 時のアロケーション上限やカタログ値長の静的アサーションの
//!   前提でもあるため、引き上げは本機能の対象外。
//! - **設定値は「新しく定義が増える操作」（CREATE TABLE・ALTER TABLE ADD COLUMN・
//!   CREATE TYPE AS ENUM・新規宣言の配列 `[N]`）の実行段（`sql::ddl`）にだけ効く**。
//!   永続化済みデータの読み出し（`catalog` の decode・`validate_schema`・
//!   `row_codec`）には一切入れない。入れると上限を下げて再起動した際に既存の
//!   テーブル・型が `CorruptSchema` で読めなくなる（可用性喪失）ため。

use crate::catalog::{MAX_ARRAY_ELEMENTS, MAX_COLUMN_COUNT, MAX_ENUM_LABELS};

/// 3 上限に設定できる最小値。
pub const MIN_SCHEMA_LIMIT: usize = 1;

/// スキーマ系 3 上限の設定値。フィールドは非公開で、[`SchemaLimits::new`] は
/// ハード上限で飽和させるため範囲外の値を保持しない。`Default` は 3 つとも
/// ハード上限と同値＝現行挙動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchemaLimits {
    max_table_columns: usize,
    max_array_elements: u32,
    max_enum_labels: usize,
}

impl Default for SchemaLimits {
    fn default() -> Self {
        Self {
            max_table_columns: MAX_COLUMN_COUNT,
            max_array_elements: MAX_ARRAY_ELEMENTS,
            max_enum_labels: MAX_ENUM_LABELS,
        }
    }
}

impl SchemaLimits {
    /// 検証済みの 3 値から組み立てる。引数は `validate_*` の戻り値を想定するが、
    /// 直接渡された場合もハード上限で飽和させ、範囲外の値を保持しない（fail-closed）。
    pub fn new(max_table_columns: usize, max_array_elements: u32, max_enum_labels: usize) -> Self {
        Self {
            max_table_columns: max_table_columns.clamp(MIN_SCHEMA_LIMIT, MAX_COLUMN_COUNT),
            max_array_elements: max_array_elements.clamp(1, MAX_ARRAY_ELEMENTS),
            max_enum_labels: max_enum_labels.clamp(MIN_SCHEMA_LIMIT, MAX_ENUM_LABELS),
        }
    }

    /// 1 テーブルの列数（ADD COLUMN では物理スロット数＝生存列＋墓標）の上限。
    pub fn max_table_columns(&self) -> usize {
        self.max_table_columns
    }

    /// 新規宣言する配列列 1 個の要素数上限（`[]` 省略時の既定値も兼ねる）。
    pub fn max_array_elements(&self) -> u32 {
        self.max_array_elements
    }

    /// `CREATE TYPE ... AS ENUM` 1 個のラベル数上限。
    pub fn max_enum_labels(&self) -> usize {
        self.max_enum_labels
    }
}

/// `validate_*` の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct SchemaLimitOutOfRange {
    value: usize,
    max: usize,
}

impl std::fmt::Display for SchemaLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "value {} is out of range 1..={}", self.value, self.max)
    }
}

impl std::error::Error for SchemaLimitOutOfRange {}

fn validate_in(value: usize, max: usize) -> Result<usize, SchemaLimitOutOfRange> {
    if (MIN_SCHEMA_LIMIT..=max).contains(&value) {
        Ok(value)
    } else {
        Err(SchemaLimitOutOfRange { value, max })
    }
}

/// 列数上限の設定値を `1..=MAX_COLUMN_COUNT` で検証する単一情報源。
pub fn validate_max_table_columns(value: usize) -> Result<usize, SchemaLimitOutOfRange> {
    validate_in(value, MAX_COLUMN_COUNT)
}

/// 配列要素数上限の設定値を `1..=MAX_ARRAY_ELEMENTS` で検証する単一情報源。
pub fn validate_max_array_elements(value: usize) -> Result<u32, SchemaLimitOutOfRange> {
    let max = MAX_ARRAY_ELEMENTS as usize;
    let v = validate_in(value, max)?;
    // `validate_in` が `max`（u32 範囲）以下を保証済み。失敗は範囲外と同じ扱い。
    u32::try_from(v).map_err(|_| SchemaLimitOutOfRange { value, max })
}

/// ENUM ラベル数上限の設定値を `1..=MAX_ENUM_LABELS` で検証する単一情報源。
pub fn validate_max_enum_labels(value: usize) -> Result<usize, SchemaLimitOutOfRange> {
    validate_in(value, MAX_ENUM_LABELS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limits() {
        let d = SchemaLimits::default();
        assert_eq!(d.max_table_columns(), MAX_COLUMN_COUNT);
        assert_eq!(d.max_array_elements(), MAX_ARRAY_ELEMENTS);
        assert_eq!(d.max_enum_labels(), MAX_ENUM_LABELS);
    }

    #[test]
    fn validators_accept_boundaries_and_reject_outside() {
        assert_eq!(validate_max_table_columns(1), Ok(1));
        assert_eq!(
            validate_max_table_columns(MAX_COLUMN_COUNT),
            Ok(MAX_COLUMN_COUNT)
        );
        assert!(validate_max_table_columns(0).is_err());
        assert!(validate_max_table_columns(MAX_COLUMN_COUNT + 1).is_err());
        assert_eq!(validate_max_array_elements(1), Ok(1));
        assert_eq!(
            validate_max_array_elements(MAX_ARRAY_ELEMENTS as usize),
            Ok(MAX_ARRAY_ELEMENTS)
        );
        assert!(validate_max_array_elements(0).is_err());
        assert!(validate_max_array_elements(MAX_ARRAY_ELEMENTS as usize + 1).is_err());
        assert!(validate_max_enum_labels(0).is_err());
        assert!(validate_max_enum_labels(MAX_ENUM_LABELS + 1).is_err());
        assert_eq!(
            validate_max_enum_labels(MAX_ENUM_LABELS),
            Ok(MAX_ENUM_LABELS)
        );
    }

    #[test]
    fn error_display_has_only_value_and_range() {
        let e = validate_max_table_columns(0).unwrap_err();
        assert_eq!(e.to_string(), "value 0 is out of range 1..=256");
    }

    #[test]
    fn new_saturates_to_hard_limits() {
        let l = SchemaLimits::new(usize::MAX, u32::MAX, usize::MAX);
        assert_eq!(l, SchemaLimits::default());
        let l = SchemaLimits::new(0, 0, 0);
        assert_eq!(l.max_table_columns(), 1);
        assert_eq!(l.max_array_elements(), 1);
        assert_eq!(l.max_enum_labels(), 1);
    }
}
