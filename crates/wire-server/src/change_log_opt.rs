//! `--change-log` CLI 引数の受理判定と起動時の固定文言（Issue #3067・#3069・親 #2166・RECOVER-14
//! ポインタ。設計は `docs/design/change-log-pitr.md` の D8・D11）。
//!
//! 永続変更ログ（増分バックアップ・PITR の土台）は opt-in の値なしスイッチで有効にする。engine の
//! 書き込み点の移行（#3068）とオラクル試験・性能計測（#3069）が済んだため、指定すると engine の
//! `Storage::enable_change_log` へ結線される（`main.rs` の `open_engine_core_with_change_log`）。
//! `main.rs` の引数ループから呼ばれ、値は読まず・echo しない（スイッチのみ。重複指定は
//! last-wins にせず拒否する）。
//!
//! 一度有効にした DB をフラグなしで起動すると、レコードが欠けたまま世代が進む（LSN の欠落）ため、
//! 起動を [`REQUIRED_MESSAGE`] で拒否する（fail-closed。engine も同じ状態の書き込みを `54000` で
//! 拒否する）。アーカイブ・保持・PITR は後続（#2167 以降）で、現時点の変更ログは `change_log` 表へ
//! 溜まるだけである。

/// 受理対象の CLI フラグ名（値を取らないスイッチ）。
pub const FLAG: &str = "--change-log";

/// 2 回目以降の指定を拒否する固定文言（last-wins にしない）。
pub const DUPLICATE_MESSAGE: &str = "--change-log specified more than once";

/// 一度有効にした DB をフラグなしで起動したときに拒否する固定文言（LSN の欠落を作らない）。
pub const REQUIRED_MESSAGE: &str = "this database has a change log: restart with --change-log (writing without it would leave a gap in the log)";

/// 有効化に成功したときに stderr へ出す固定文言（運用者向けの状態表示）。
pub const ENABLED_MESSAGE: &str = "change log enabled (archive, retention and point-in-time recovery are not available yet; the log grows inside the database)";

/// フラグ 1 回分の受理判定。`already_set` は既に 1 回指定済みか。重複指定は [`DUPLICATE_MESSAGE`]。
/// （有効化できないビルドは無い。以前あった「未提供」の拒否文言と `AVAILABLE` は #3069 で削除した。）
pub fn accept(already_set: bool) -> Result<(), &'static str> {
    if already_set {
        return Err(DUPLICATE_MESSAGE);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_is_a_bare_switch_name() {
        assert!(FLAG.starts_with("--"));
        assert!(!FLAG.contains('='));
        assert!(DUPLICATE_MESSAGE.starts_with(FLAG));
    }

    #[test]
    fn the_flag_is_accepted_once() {
        assert_eq!(accept(false), Ok(()));
        assert_eq!(accept(true), Err(DUPLICATE_MESSAGE));
    }

    #[test]
    fn startup_messages_carry_no_variable_content() {
        for m in [REQUIRED_MESSAGE, ENABLED_MESSAGE] {
            assert!(!m.contains('{') && !m.contains('\n'));
        }
        assert!(REQUIRED_MESSAGE.contains(FLAG));
    }
}
