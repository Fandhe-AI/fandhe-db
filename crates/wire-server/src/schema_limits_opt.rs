//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-table-columns`・
//! `--max-array-elements`・`--max-enum-labels` opt-in CLI 引数のパーサ
//! （Issue #1548。ポインタ: `docs/spec/04-behavior/` SQL-46）。
//!
//! `dml_limits_opt`（`--max-dml-affected-rows` 等）と同型の「プロセス起動時にのみ
//! 明示指定する注入点」。範囲（各ハード上限以下への引き下げのみ）の判定は engine 側の
//! 単一情報源 [`engine::schema_limits`] の `validate_*` に委ね、本モジュールは untrusted な
//! CLI 文字列を厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して渡すだけにする。
//! 各モジュールが独立した小さい下請けを持つ既存の流儀を踏襲し、共有ヘルパーへの
//! 切り出しは行わない。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_schema_limits` へ 1 回だけ注入する。設定値は定義が増える DDL の
//! 実行段（`engine::sql::ddl`）にだけ効き、永続化済みスキーマの読み出しには効かない
//! （`docs/design/schema-limits-cli.md`）。範囲外・非数値・値欠落・重複指定は
//! fail-closed で起動エラー（既定へ黙って読み替えない）。

use engine::schema_limits::{
    validate_max_array_elements, validate_max_enum_labels, validate_max_table_columns, SchemaLimits,
};

/// 1 テーブルの列数上限を設定する CLI フラグ名。
pub const MAX_TABLE_COLUMNS_FLAG: &str = "--max-table-columns";

/// 配列列 1 個の要素数上限を設定する CLI フラグ名。
pub const MAX_ARRAY_ELEMENTS_FLAG: &str = "--max-array-elements";

/// ENUM 型 1 個のラベル数上限を設定する CLI フラグ名。
pub const MAX_ENUM_LABELS_FLAG: &str = "--max-enum-labels";

/// 3 つの CLI フラグの生の値（未指定は `None`）。`main.rs` が引数解析で集める。
#[derive(Debug, Clone, Copy, Default)]
pub struct SchemaLimitsRawFlags<'a> {
    pub max_table_columns: Option<&'a str>,
    pub max_array_elements: Option<&'a str>,
    pub max_enum_labels: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（`dml_limits_opt::parse_strict_decimal`
/// と同じ方針。`str::parse::<usize>` が受理する先頭 `+` 等を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<usize> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

fn parse_flag(flag: &str, raw: &str) -> Result<usize, String> {
    parse_strict_decimal(raw)
        .ok_or_else(|| format!("{flag} expects a non-negative integer, got {raw:?}"))
}

/// 3 フラグの未パース値から [`SchemaLimits`] を解決する純関数。未指定のフラグは
/// 既定値（[`SchemaLimits::default`]＝ハード上限＝現行挙動）、範囲外・不正値は
/// `Err`（fail-closed）。エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: SchemaLimitsRawFlags<'_>) -> Result<SchemaLimits, String> {
    let base = SchemaLimits::default();
    let mut max_table_columns = base.max_table_columns();
    let mut max_array_elements = base.max_array_elements();
    let mut max_enum_labels = base.max_enum_labels();
    if let Some(r) = raw.max_table_columns {
        let v = parse_flag(MAX_TABLE_COLUMNS_FLAG, r)?;
        max_table_columns =
            validate_max_table_columns(v).map_err(|e| format!("{MAX_TABLE_COLUMNS_FLAG}: {e}"))?;
    }
    if let Some(r) = raw.max_array_elements {
        let v = parse_flag(MAX_ARRAY_ELEMENTS_FLAG, r)?;
        max_array_elements = validate_max_array_elements(v)
            .map_err(|e| format!("{MAX_ARRAY_ELEMENTS_FLAG}: {e}"))?;
    }
    if let Some(r) = raw.max_enum_labels {
        let v = parse_flag(MAX_ENUM_LABELS_FLAG, r)?;
        max_enum_labels =
            validate_max_enum_labels(v).map_err(|e| format!("{MAX_ENUM_LABELS_FLAG}: {e}"))?;
    }
    Ok(SchemaLimits::new(
        max_table_columns,
        max_array_elements,
        max_enum_labels,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(
        cols: Option<&str>,
        arr: Option<&str>,
        labels: Option<&str>,
    ) -> Result<SchemaLimits, String> {
        resolve(SchemaLimitsRawFlags {
            max_table_columns: cols,
            max_array_elements: arr,
            max_enum_labels: labels,
        })
    }

    #[test]
    fn unspecified_is_default() {
        assert_eq!(one(None, None, None), Ok(SchemaLimits::default()));
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        let l = one(Some("1"), Some("1"), Some("1")).expect("lower bound");
        assert_eq!(
            (
                l.max_table_columns(),
                l.max_array_elements(),
                l.max_enum_labels()
            ),
            (1, 1, 1)
        );
        let l = one(Some("256"), Some("1024"), Some("256")).expect("upper bound");
        assert_eq!(l, SchemaLimits::default());
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        for bad in [
            "0",
            "257",
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "-1",
            "99999999999999999999999",
        ] {
            let e = one(Some(bad), None, None).expect_err(bad);
            assert!(e.contains(MAX_TABLE_COLUMNS_FLAG), "{bad}: {e}");
        }
        for bad in ["0", "1025", "+5", ""] {
            let e = one(None, Some(bad), None).expect_err(bad);
            assert!(e.contains(MAX_ARRAY_ELEMENTS_FLAG), "{bad}: {e}");
        }
        for bad in ["0", "257", "+5", ""] {
            let e = one(None, None, Some(bad)).expect_err(bad);
            assert!(e.contains(MAX_ENUM_LABELS_FLAG), "{bad}: {e}");
        }
    }

    #[test]
    fn only_the_given_flag_changes() {
        let l = one(None, Some("8"), None).expect("array only");
        assert_eq!(l.max_array_elements(), 8);
        assert_eq!(
            l.max_table_columns(),
            SchemaLimits::default().max_table_columns()
        );
    }
}
