//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-dml-affected-rows`・
//! `--max-insert-rows` opt-in CLI 引数のパーサ（Issue #997）。
//!
//! オーナー判断（2026-09-27）: 述語形 UPDATE／DELETE の 1 文あたり影響行数上限・
//! 複数行 `VALUES` の 1 文あたり行数上限を、プロセス全体に対して起動時 CLI
//! フラグで設定可能にする（セッション・テナント単位の設定は対象外）。既定値は
//! いずれも現行挙動と同じ 1,000（[`engine::sql::parser::DmlLimits::default`]）。
//!
//! `--durability`（`durability_opt`）・`--search-engine`（`search_engine_opt`）と
//! 同型の「プロセス起動時にのみ明示指定する注入点」。数値の妥当な範囲
//! （`MIN_DML_ROW_LIMIT..=MAX_DML_ROW_LIMIT`）の判定は engine 側の単一情報源
//! [`engine::sql::parser::validate_dml_row_limit`] に委ね、本モジュールは
//! untrusted な CLI 文字列を厳密パースして渡すのみ（`search_engine_opt::
//! parse_strict_decimal` と同じ「先頭 `+`・空白・全角数字を弾く」方針。
//! 各モジュールが独立した小さい下請けを持つ既存の流儀を踏襲し、共有ヘルパー
//! モジュールへの切り出しは行わない）。
//!
//! `main.rs` の引数走査ループは他の閉じた語彙フラグ（`--search-engine` 等）と
//! 同じ理由で 2 回目以降の指定を fail-closed に拒否する（last-wins にしない）。

use engine::sql::parser::DmlLimits;

/// 述語形 UPDATE／DELETE の 1 文あたり影響行数上限を設定する CLI フラグ名。
pub const MAX_AFFECTED_ROWS_FLAG: &str = "--max-dml-affected-rows";

/// 複数行 `VALUES` の 1 文あたり行数上限を設定する CLI フラグ名。
pub const MAX_INSERT_ROWS_FLAG: &str = "--max-insert-rows";

/// ASCII 数字のみからなる非空文字列を厳密パースする（`search_engine_opt::
/// parse_strict_decimal` と同じ設計。`str::parse::<usize>` がそのまま受理して
/// しまう先頭 `+`・空白等を弾くための下請け。untrusted な CLI 引数からの
/// パースのため `unwrap`/`expect`/添字アクセスは使わない）。
fn parse_strict_decimal(raw: &str) -> Option<usize> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// `raw`（`flag` の値）を厳密パースし、[`engine::sql::parser::
/// validate_dml_row_limit`] で範囲検証する。`flag` はエラーメッセージにのみ
/// 使う（`raw` はテナント・行内容を含まない起動時引数のため detail への
/// 混入を気にする必要はない）。
fn parse_limit(flag: &str, raw: &str) -> Result<usize, String> {
    let value = parse_strict_decimal(raw)
        .ok_or_else(|| format!("{flag} expects a non-negative integer, got {raw:?}"))?;
    engine::sql::parser::validate_dml_row_limit(value).map_err(|e| format!("{flag}: {e}"))
}

/// `--max-dml-affected-rows`／`--max-insert-rows` の未パース値（Issue #997）から
/// [`DmlLimits`] を解決する。純関数として切り出し、`std::env::args()` を直接
/// 読まずに単体テストできるようにする（`resolve_durability`・`resolve_surface`
/// と同じ流儀）。`raw` がいずれも `None` は [`DmlLimits::default`]（既定値
/// 1,000・現行挙動を維持）、範囲外・不正値はいずれも `Err`（fail-closed。
/// 既定へ黙って読み替えない）。
pub fn resolve(
    max_affected_rows_raw: Option<&str>,
    max_insert_rows_raw: Option<&str>,
) -> Result<DmlLimits, String> {
    let defaults = DmlLimits::default();
    let max_affected_rows = match max_affected_rows_raw {
        None => defaults.max_affected_rows,
        Some(raw) => parse_limit(MAX_AFFECTED_ROWS_FLAG, raw)?,
    };
    let max_insert_rows_per_statement = match max_insert_rows_raw {
        None => defaults.max_insert_rows_per_statement,
        Some(raw) => parse_limit(MAX_INSERT_ROWS_FLAG, raw)?,
    };
    Ok(DmlLimits {
        max_affected_rows,
        max_insert_rows_per_statement,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_defaults_when_both_unset() {
        let limits = resolve(None, None).unwrap();
        let defaults = DmlLimits::default();
        assert_eq!(limits.max_affected_rows, defaults.max_affected_rows);
        assert_eq!(
            limits.max_insert_rows_per_statement,
            defaults.max_insert_rows_per_statement
        );
    }

    #[test]
    fn resolve_applies_each_flag_independently() {
        let limits = resolve(Some("5"), Some("7")).unwrap();
        assert_eq!(limits.max_affected_rows, 5);
        assert_eq!(limits.max_insert_rows_per_statement, 7);
    }

    #[test]
    fn resolve_rejects_zero() {
        let err = resolve(Some("0"), None).unwrap_err();
        assert!(
            err.contains(MAX_AFFECTED_ROWS_FLAG),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_rejects_value_over_upper_bound() {
        let over = (engine::sql::parser::MAX_DML_ROW_LIMIT + 1).to_string();
        let err = resolve(None, Some(&over)).unwrap_err();
        assert!(
            err.contains(MAX_INSERT_ROWS_FLAG),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_accepts_upper_bound_exactly() {
        let at_max = engine::sql::parser::MAX_DML_ROW_LIMIT.to_string();
        let limits = resolve(Some(&at_max), Some(&at_max)).unwrap();
        assert_eq!(
            limits.max_affected_rows,
            engine::sql::parser::MAX_DML_ROW_LIMIT
        );
        assert_eq!(
            limits.max_insert_rows_per_statement,
            engine::sql::parser::MAX_DML_ROW_LIMIT
        );
    }

    #[test]
    fn resolve_rejects_non_numeric() {
        let err = resolve(Some("abc"), None).unwrap_err();
        assert!(
            err.contains(MAX_AFFECTED_ROWS_FLAG),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_rejects_leading_plus_and_whitespace() {
        assert!(resolve(Some("+5"), None).is_err());
        assert!(resolve(Some(" 5"), None).is_err());
        assert!(resolve(Some("5 "), None).is_err());
    }
}
