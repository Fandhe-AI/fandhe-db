//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-dml-affected-rows`・
//! `--max-insert-rows` opt-in CLI 引数のパーサ（Issue #997）。あわせて
//! `--batch-max-files`（Issue #1166。`batch_limits.max_files_per_batch`。
//! 優先順位は CLI 明示 > 環境変数 `VECTOR_DB_BATCH_MAX_FILES` > 既定 64）の
//! 解決 [`resolve_batch_limits`] も本モジュールが担う。
//!
//! オーナー判断の改訂（2026-09-27、前回のオーナー判断を置き換え）: 汎用 RDB
//! （PostgreSQL 等）の挙動に合わせ、述語形 UPDATE／DELETE の 1 文あたり影響行数
//! 上限・複数行 `VALUES` の 1 文あたり行数上限は**既定で無効（上限なし）**と
//! する。プロセス全体に対して起動時 CLI フラグで明示指定した場合のみ有効に
//! なる（セッション・テナント単位の設定は対象外）。資源上限は既存の
//! SQL 文長上限・1 文あたり総走査行数上限（`engine::tenant::MAX_SCANNED_ROWS`）
//! で引き続き担保し、これらは変更しない
//! （[`engine::sql::parser::DmlLimits`] ドキュメント・`docs/design/
//! predicate-dml-exec.md` §6 参照）。
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
use std::num::NonZeroUsize;

/// 述語形 UPDATE／DELETE の 1 文あたり影響行数上限を設定する CLI フラグ名。
pub const MAX_AFFECTED_ROWS_FLAG: &str = "--max-dml-affected-rows";

/// 複数行 `VALUES` の 1 文あたり行数上限を設定する CLI フラグ名。
pub const MAX_INSERT_ROWS_FLAG: &str = "--max-insert-rows";

/// `batch_limits.max_files_per_batch` を設定する CLI フラグ名（Issue #1166）。
/// 環境変数 `VECTOR_DB_BATCH_MAX_FILES` と 1 対 1 に対応し、優先順位は
/// CLI 明示 > 環境変数 > 既定 64。
pub const BATCH_MAX_FILES_FLAG: &str = "--batch-max-files";

/// `--batch-max-files` の未パース値から [`engine::batch_limits::BatchLimits`] を
/// 解決する（Issue #1166）。`base` は環境変数・既定を解決済みの値
/// （`BatchLimits::default()`）で、`raw` が `Some` のときだけ
/// `max_files_per_batch` を上書きする（CLI 明示 > 環境変数 > 既定）。範囲
/// （`1..=MAX_BATCH_MAX_FILES`）は engine 側の単一情報源
/// [`engine::batch_limits::validate_max_files_per_batch`] に委ね、不正値は
/// `Err`（fail-closed。既定へ黙って読み替えない）。
pub fn resolve_batch_limits(
    base: engine::batch_limits::BatchLimits,
    raw: Option<&str>,
) -> Result<engine::batch_limits::BatchLimits, String> {
    let Some(raw) = raw else {
        return Ok(base);
    };
    let value = parse_strict_decimal(raw).ok_or_else(|| {
        format!("{BATCH_MAX_FILES_FLAG} expects a non-negative integer, got {raw:?}")
    })?;
    let max_files_per_batch = engine::batch_limits::validate_max_files_per_batch(value)
        .map_err(|e| format!("{BATCH_MAX_FILES_FLAG}: {e}"))?;
    Ok(engine::batch_limits::BatchLimits {
        max_files_per_batch,
        ..base
    })
}

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
/// validate_dml_row_limit`] で範囲検証して `NonZeroUsize` へ変換する。`flag` は
/// エラーメッセージにのみ使う（`raw` はテナント・行内容を含まない起動時引数の
/// ため detail への混入を気にする必要はない）。
fn parse_limit(flag: &str, raw: &str) -> Result<NonZeroUsize, String> {
    let value = parse_strict_decimal(raw)
        .ok_or_else(|| format!("{flag} expects a non-negative integer, got {raw:?}"))?;
    engine::sql::parser::validate_dml_row_limit(value).map_err(|e| format!("{flag}: {e}"))
}

/// `--max-dml-affected-rows`／`--max-insert-rows` の未パース値（Issue #997）から
/// [`DmlLimits`] を解決する。純関数として切り出し、`std::env::args()` を直接
/// 読まずに単体テストできるようにする（`resolve_durability`・`resolve_surface`
/// と同じ流儀）。`raw` がいずれも `None`（フラグ未指定）は
/// `DmlLimits { max_affected_rows: None, max_insert_rows_per_statement: None }`
/// （既定・上限なし。オーナー判断の改訂 2026-09-27）、範囲外・不正値は
/// いずれも `Err`（fail-closed。既定へ黙って読み替えない）。
pub fn resolve(
    max_affected_rows_raw: Option<&str>,
    max_insert_rows_raw: Option<&str>,
) -> Result<DmlLimits, String> {
    let max_affected_rows = match max_affected_rows_raw {
        None => None,
        Some(raw) => Some(parse_limit(MAX_AFFECTED_ROWS_FLAG, raw)?),
    };
    let max_insert_rows_per_statement = match max_insert_rows_raw {
        None => None,
        Some(raw) => Some(parse_limit(MAX_INSERT_ROWS_FLAG, raw)?),
    };
    Ok(DmlLimits {
        max_affected_rows,
        max_insert_rows_per_statement,
    })
}

/// 複数行 `VALUES`（`BoundInsertForm::RowBatch`）は本モジュールの
/// `max_insert_rows_per_statement`（構文解析段の上限。`Some` のときのみ判定）に
/// 加え、独立した別上限 `engine::batch_limits::BatchLimits` の
/// `max_files_per_batch`（既定 64。Issue #860）と `max_batch_chunks`
/// （既定 4096）も通る多重ゲートであり、実効行数上限は両者の小さい方になる
/// （`docs/design/predicate-dml-exec.md` §6 参照）。`--max-insert-rows` が
/// この実効上限を超えると引き上げが黙って無効化されるため（codex-review P1
/// 指摘・PR #1122）、`--max-insert-rows` を**明示指定**した場合に限り起動ログへ
/// 英語の `WARNING` 行を出す（`--durability none` と同じ「明示選択した非既定値の
/// 安全上の含意を見落とさせない」設計判断。エラーにはしない）。
/// `max_insert_rows_per_statement` が `None`（未指定・既定）の場合は警告しない。
/// `batch_limits` には main.rs が `EngineCore::with_batch_limits` へ渡すのと
/// **同じ解決済みの値**（`--batch-max-files` > 環境変数 > 既定。Issue #1166）を
/// 渡すこと。メッセージには効いている側の上限と引き上げ手段を含める。
pub fn insert_rows_cap_warning(
    limits: &DmlLimits,
    batch_limits: &engine::batch_limits::BatchLimits,
) -> Option<String> {
    let limit = limits.max_insert_rows_per_statement?;
    let files_cap = batch_limits.max_files_per_batch;
    let chunks_cap = batch_limits.max_batch_chunks;
    let effective = files_cap.min(chunks_cap);
    if limit.get() <= effective {
        return None;
    }
    let remedy = if files_cap <= chunks_cap {
        format!(
            "batch_limits.max_files_per_batch; set {BATCH_MAX_FILES_FLAG} (or the \
             VECTOR_DB_BATCH_MAX_FILES environment variable) to raise it"
        )
    } else {
        "batch_limits.max_batch_chunks; set the VECTOR_DB_BATCH_MAX_CHUNKS \
         environment variable to raise it"
            .to_string()
    };
    Some(format!(
        "{MAX_INSERT_ROWS_FLAG} is set to {limit} but multi-row VALUES statements are still \
         capped at {effective} rows by {remedy}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_defaults_when_both_unset() {
        let limits = resolve(None, None).unwrap();
        assert_eq!(limits.max_affected_rows, None);
        assert_eq!(limits.max_insert_rows_per_statement, None);
    }

    #[test]
    fn resolve_applies_each_flag_independently() {
        let limits = resolve(Some("5"), Some("7")).unwrap();
        assert_eq!(limits.max_affected_rows.map(NonZeroUsize::get), Some(5));
        assert_eq!(
            limits.max_insert_rows_per_statement.map(NonZeroUsize::get),
            Some(7)
        );
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
            limits.max_affected_rows.map(NonZeroUsize::get),
            Some(engine::sql::parser::MAX_DML_ROW_LIMIT)
        );
        assert_eq!(
            limits.max_insert_rows_per_statement.map(NonZeroUsize::get),
            Some(engine::sql::parser::MAX_DML_ROW_LIMIT)
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

    #[test]
    fn insert_rows_cap_warning_fires_when_configured_limit_exceeds_batch_limits() {
        let limits = DmlLimits {
            max_affected_rows: None,
            max_insert_rows_per_statement: Some(NonZeroUsize::new(100).unwrap()),
        };
        let batch_limits = engine::batch_limits::BatchLimits {
            max_files_per_batch: 64,
            ..engine::batch_limits::BatchLimits::default()
        };
        let warning = insert_rows_cap_warning(&limits, &batch_limits)
            .expect("expected a warning when max_insert_rows_per_statement > max_files_per_batch");
        assert!(
            warning.contains(MAX_INSERT_ROWS_FLAG),
            "unexpected: {warning}"
        );
        assert!(warning.contains("100"), "unexpected: {warning}");
        assert!(warning.contains("64"), "unexpected: {warning}");
        assert!(
            warning.contains("VECTOR_DB_BATCH_MAX_FILES"),
            "unexpected: {warning}"
        );
        assert!(
            warning.contains(BATCH_MAX_FILES_FLAG),
            "unexpected: {warning}"
        );
    }

    #[test]
    fn insert_rows_cap_warning_names_chunks_when_chunks_cap_is_effective() {
        let limits = DmlLimits {
            max_affected_rows: None,
            max_insert_rows_per_statement: Some(NonZeroUsize::new(5000).unwrap()),
        };
        let batch_limits = engine::batch_limits::BatchLimits {
            max_files_per_batch: 10000,
            max_batch_chunks: 4096,
            ..engine::batch_limits::BatchLimits::default()
        };
        let warning = insert_rows_cap_warning(&limits, &batch_limits).expect("warning expected");
        assert!(warning.contains("4096"), "unexpected: {warning}");
        assert!(
            warning.contains("VECTOR_DB_BATCH_MAX_CHUNKS"),
            "unexpected: {warning}"
        );
    }

    #[test]
    fn resolve_batch_limits_none_keeps_base() {
        let base = engine::batch_limits::BatchLimits {
            max_files_per_batch: 77,
            ..engine::batch_limits::BatchLimits::default()
        };
        let got = resolve_batch_limits(base, None).unwrap();
        assert_eq!(got.max_files_per_batch, 77);
    }

    #[test]
    fn resolve_batch_limits_overrides_only_max_files() {
        let base = engine::batch_limits::BatchLimits {
            max_files_per_batch: 77,
            ..engine::batch_limits::BatchLimits::default()
        };
        let got = resolve_batch_limits(base, Some("3")).unwrap();
        assert_eq!(got.max_files_per_batch, 3);
        assert_eq!(got.max_batch_chunks, base.max_batch_chunks);
        assert_eq!(got.max_batch_total_bytes, base.max_batch_total_bytes);
    }

    #[test]
    fn resolve_batch_limits_accepts_upper_bound_and_rejects_invalid() {
        let base = engine::batch_limits::BatchLimits::default();
        let max = engine::batch_limits::MAX_BATCH_MAX_FILES;
        assert_eq!(
            resolve_batch_limits(base, Some(&max.to_string()))
                .unwrap()
                .max_files_per_batch,
            max
        );
        for bad in ["0", "abc", "+5", " 5", "5 ", ""] {
            let err = resolve_batch_limits(base, Some(bad)).unwrap_err();
            assert!(err.contains(BATCH_MAX_FILES_FLAG), "unexpected: {err}");
        }
        let over = (max + 1).to_string();
        let err = resolve_batch_limits(base, Some(&over)).unwrap_err();
        assert!(err.contains(BATCH_MAX_FILES_FLAG), "unexpected: {err}");
    }

    #[test]
    fn insert_rows_cap_warning_is_silent_when_unset() {
        // 既定（`--max-insert-rows` 未指定＝`None`）では警告しない
        // （`--durability`／`--search-engine` と同じ「非既定値を明示選択した
        // ときだけ警告する」既定パターンを踏襲する）。
        let limits = DmlLimits {
            max_affected_rows: None,
            max_insert_rows_per_statement: None,
        };
        let batch_limits = engine::batch_limits::BatchLimits::default();
        assert!(insert_rows_cap_warning(&limits, &batch_limits).is_none());
    }

    #[test]
    fn insert_rows_cap_warning_is_silent_within_batch_limits() {
        let limits = DmlLimits {
            max_affected_rows: None,
            max_insert_rows_per_statement: Some(NonZeroUsize::new(64).unwrap()),
        };
        let batch_limits = engine::batch_limits::BatchLimits {
            max_files_per_batch: 64,
            ..engine::batch_limits::BatchLimits::default()
        };
        assert!(insert_rows_cap_warning(&limits, &batch_limits).is_none());
    }
}
