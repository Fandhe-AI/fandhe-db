//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-dml-affected-rows`・
//! `--max-insert-rows` opt-in CLI 引数のパーサ（Issue #997）。
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
/// 加え、独立した別上限 `engine::batch_limits::BatchLimits::max_files_per_batch`
/// （既定 64。Issue #860 SQL/NoSQL 機能パリティ）も通る二重ゲートである
/// （`docs/design/predicate-dml-exec.md` §6「`batch_limits.max_files_per_batch`
/// との二重ゲート」参照）。`wire-server` は現状 `EngineCore::with_batch_limits`
/// を呼ばず既定値のまま運用するため、`--max-insert-rows` で
/// `max_files_per_batch` 超の値を指定しても、複数行 `VALUES` は
/// `max_files_per_batch` 側で `54000` になり CLI の引き上げが黙って無効化
/// される（codex-review P1 指摘・PR #1122）。この状態を運用者が見落とさない
/// よう、`--max-insert-rows` を**明示指定**した場合に限り起動ログへ英語の
/// `WARNING` 行を出す（`--durability none` と同じ「明示選択した非既定値の
/// 安全上の含意を見落とさせない」設計判断。エラーにはしない——
/// `max_files_per_batch` 以下の行数しか使わない構成では正当なため
/// fail-closed で拒否する理由がない）。`limits.max_insert_rows_per_statement`
/// が `None`（`--max-insert-rows` 未指定・既定）の場合は警告しない
/// （`batch_limits.max_files_per_batch` は本 Issue 以前から常に適用されて
/// きた既存の暗黙上限であり、フラグ未指定という「何も選択していない」状態を
/// 毎回警告すると `--durability`／`--search-engine` 等の既定パターン
/// 〔非既定値を明示選択したときだけ警告する〕から外れ、通常起動のたびに
/// ノイズになる）。`max_files_per_batch` 自体を引き上げる CLI フラグは
/// Issue #997 のオーナー承認範囲（対象 2 つ）に含まれないため追加しない
/// （環境変数 `VECTOR_DB_BATCH_MAX_FILES`。`engine::batch_limits` モジュール
/// ドキュメント参照。既存の設定経路を案内するのみ）。
pub fn insert_rows_cap_warning(
    limits: &DmlLimits,
    batch_limits: &engine::batch_limits::BatchLimits,
) -> Option<String> {
    let limit = limits.max_insert_rows_per_statement?;
    if limit.get() <= batch_limits.max_files_per_batch {
        return None;
    }
    Some(format!(
        "{MAX_INSERT_ROWS_FLAG} is set to {limit} but multi-row VALUES statements are still \
         capped at {} rows by the batch_limits.max_files_per_batch default; set the \
         VECTOR_DB_BATCH_MAX_FILES environment variable to raise it",
        batch_limits.max_files_per_batch
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
