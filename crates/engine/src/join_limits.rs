//! JOIN のリレーション数・入力行数・出力行数上限の起動時設定（Issue #1551。
//! ポインタ: `docs/spec/04-behavior/` SQL-46）。
//!
//! `wire-server` の `join_limits_opt` が CLI フラグ（`--max-join-relations`・
//! `--max-join-input-rows`・`--max-join-output-rows`）を厳密パースし、本モジュールの
//! `validate_*` で範囲検証したうえで [`JoinSizeLimits`] を組み立て、
//! `EngineCore::with_join_limits` へ 1 回だけ注入する（`group_by_limits` と同じ流儀）。
//! 範囲検証の単一情報源は本モジュール。内部実行用の `sql::join::JoinLimits` とは別型。
//!
//! 設計上の不変条件（`docs/design/join-limits-cli.md`）:
//!
//! - **ハード上限は現行定数のまま**（リレーション数は `sql::relation::MAX_TABLE_REFS`、
//!   行数は本モジュールの `MAX_JOIN_*_ROWS`）。設定できるのは「下げる方向」だけで、
//!   既定値はハード上限と同値（フラグ未指定で挙動不変）。
//! - **設定値は実行段（`sql::join::execute_with_limits`）でだけ判定する**。構文段・束縛段の
//!   `MAX_TABLE_REFS` 検査はアロケーション上限として残す。

use crate::sql::relation::MAX_TABLE_REFS;

/// JOIN 1 辺（可視かつ WHERE に一致する行）の行数上限（実装既定値。Issue #925
/// §2.4）。無制限 `Vec` 確保を避ける（security.md「不安全な設計」対応）。
/// 超過検出のため走査自体は `上限 + 1` を上限に行い、超えたら `54000`。
pub const MAX_JOIN_INPUT_ROWS: usize = 100_000;

/// `LIMIT` 適用前の結合カーディナリティ（一致ペア数）の上限（実装既定値。
/// Issue #925 §2.4）。`LIMIT`／`OFFSET` の値によらず、結合の各段で判定する。超過は `54000`。
pub const MAX_JOIN_OUTPUT_ROWS: usize = 100_000;

/// リレーション数に設定できる最小値（JOIN は 2 リレーション以上）。
pub const MIN_JOIN_RELATIONS: usize = 2;

/// 行数上限に設定できる最小値。
pub const MIN_JOIN_ROW_LIMIT: usize = 1;

/// JOIN の 3 上限の設定値。フィールドは非公開で、[`JoinSizeLimits::new`] は
/// 範囲で飽和させるため範囲外の値を保持しない。`Default` はハード上限と同値＝現行挙動。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinSizeLimits {
    max_relations: usize,
    max_input_rows: usize,
    max_output_rows: usize,
}

impl Default for JoinSizeLimits {
    fn default() -> Self {
        Self {
            max_relations: MAX_TABLE_REFS,
            max_input_rows: MAX_JOIN_INPUT_ROWS,
            max_output_rows: MAX_JOIN_OUTPUT_ROWS,
        }
    }
}

impl JoinSizeLimits {
    /// 検証済みの 3 値から組み立てる。直接渡された場合も範囲で飽和させる（fail-closed）。
    pub fn new(max_relations: usize, max_input_rows: usize, max_output_rows: usize) -> Self {
        Self {
            max_relations: max_relations.clamp(MIN_JOIN_RELATIONS, MAX_TABLE_REFS),
            max_input_rows: max_input_rows.clamp(MIN_JOIN_ROW_LIMIT, MAX_JOIN_INPUT_ROWS),
            max_output_rows: max_output_rows.clamp(MIN_JOIN_ROW_LIMIT, MAX_JOIN_OUTPUT_ROWS),
        }
    }

    /// JOIN に含められるリレーション数の上限。
    pub fn max_relations(&self) -> usize {
        self.max_relations
    }

    /// JOIN 1 辺の可視行数の上限。
    pub fn max_input_rows(&self) -> usize {
        self.max_input_rows
    }

    /// 結合の出力（一致ペア）行数の上限。
    pub fn max_output_rows(&self) -> usize {
        self.max_output_rows
    }
}

/// `validate_*` の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct JoinLimitOutOfRange {
    value: usize,
    min: usize,
    max: usize,
}

impl std::fmt::Display for JoinLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "value {} is out of range {}..={}",
            self.value, self.min, self.max
        )
    }
}

impl std::error::Error for JoinLimitOutOfRange {}

fn validate_in(value: usize, min: usize, max: usize) -> Result<usize, JoinLimitOutOfRange> {
    if (min..=max).contains(&value) {
        Ok(value)
    } else {
        Err(JoinLimitOutOfRange { value, min, max })
    }
}

/// リレーション数上限の設定値を `2..=MAX_TABLE_REFS` で検証する単一情報源。
pub fn validate_max_join_relations(value: usize) -> Result<usize, JoinLimitOutOfRange> {
    validate_in(value, MIN_JOIN_RELATIONS, MAX_TABLE_REFS)
}

/// 入力行数上限の設定値を `1..=MAX_JOIN_INPUT_ROWS` で検証する単一情報源。
pub fn validate_max_join_input_rows(value: usize) -> Result<usize, JoinLimitOutOfRange> {
    validate_in(value, MIN_JOIN_ROW_LIMIT, MAX_JOIN_INPUT_ROWS)
}

/// 出力行数上限の設定値を `1..=MAX_JOIN_OUTPUT_ROWS` で検証する単一情報源。
pub fn validate_max_join_output_rows(value: usize) -> Result<usize, JoinLimitOutOfRange> {
    validate_in(value, MIN_JOIN_ROW_LIMIT, MAX_JOIN_OUTPUT_ROWS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limits() {
        let d = JoinSizeLimits::default();
        assert_eq!(d.max_relations(), MAX_TABLE_REFS);
        assert_eq!(d.max_input_rows(), 100_000);
        assert_eq!(d.max_output_rows(), 100_000);
    }

    #[test]
    fn validators_accept_boundaries_and_reject_outside() {
        assert_eq!(validate_max_join_relations(2), Ok(2));
        assert_eq!(
            validate_max_join_relations(MAX_TABLE_REFS),
            Ok(MAX_TABLE_REFS)
        );
        assert!(validate_max_join_relations(1).is_err());
        assert!(validate_max_join_relations(MAX_TABLE_REFS + 1).is_err());
        assert_eq!(validate_max_join_input_rows(1), Ok(1));
        assert_eq!(validate_max_join_input_rows(100_000), Ok(100_000));
        assert!(validate_max_join_input_rows(0).is_err());
        assert!(validate_max_join_input_rows(100_001).is_err());
        assert_eq!(validate_max_join_output_rows(1), Ok(1));
        assert_eq!(validate_max_join_output_rows(100_000), Ok(100_000));
        assert!(validate_max_join_output_rows(0).is_err());
        assert!(validate_max_join_output_rows(100_001).is_err());
    }

    #[test]
    fn error_display_has_only_value_and_range() {
        let e = validate_max_join_relations(1).unwrap_err();
        assert_eq!(e.to_string(), "value 1 is out of range 2..=8");
    }

    #[test]
    fn new_saturates_to_range() {
        assert_eq!(
            JoinSizeLimits::new(usize::MAX, usize::MAX, usize::MAX),
            JoinSizeLimits::default()
        );
        let l = JoinSizeLimits::new(0, 0, 0);
        assert_eq!(
            (l.max_relations(), l.max_input_rows(), l.max_output_rows()),
            (2, 1, 1)
        );
    }
}
