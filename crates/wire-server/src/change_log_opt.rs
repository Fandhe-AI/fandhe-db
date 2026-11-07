//! `--change-log` CLI 引数の受理／拒否の判定（Issue #3067・親 #2166・RECOVER-14 ポインタ。
//! 設計は `docs/design/change-log-pitr.md` の D8）。
//!
//! 永続変更ログ（増分バックアップ・PITR の土台）は opt-in の値なしスイッチで有効にする。
//! ただし現段階は engine の土台（ラッパ・capture frame・レコード形式）だけが入っており、
//! 既存の書き込み点の移行（#3068）とオラクル・有効化（#3069）が済むまでは記録が欠ける
//! 経路が残る。欠けた記録を黙って許すと復元結果が静かに食い違うため、指定された起動は
//! 「未提供」と分かる固定文言で拒否する（fail-closed。`english_stemmer_opt` と同じ流儀）。
//! `main.rs` の引数ループから呼ばれ、値は読まず・echo しない（スイッチのみ）。#3069 が
//! [`AVAILABLE`] を有効にし、engine の有効化入口へ結線する。

/// 受理対象の CLI フラグ名（値を取らないスイッチ）。
pub const FLAG: &str = "--change-log";

/// 変更ログを有効にして起動できるか。書き込み点の移行とオラクル（#3068・#3069）が済むまでは `false`。
pub const AVAILABLE: bool = false;

/// 起動拒否時に stderr へ出す固定文言（引数値・パス・テナント情報を含めない）。
pub const REJECT_MESSAGE: &str = "--change-log is not available yet: the change log is not fully wired into the write paths, so enabling it is refused";

/// 2 回目以降の指定を拒否する固定文言（last-wins にしない）。
pub const DUPLICATE_MESSAGE: &str = "--change-log specified more than once";

/// フラグ 1 回分の受理判定。`already_set` は既に 1 回指定済みか。
/// 未提供の間は常に [`REJECT_MESSAGE`]、提供後も重複指定は [`DUPLICATE_MESSAGE`]。
pub fn accept(already_set: bool) -> Result<(), &'static str> {
    if !AVAILABLE {
        return Err(REJECT_MESSAGE);
    }
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
        assert!(REJECT_MESSAGE.starts_with(FLAG));
        assert!(DUPLICATE_MESSAGE.starts_with(FLAG));
    }

    #[test]
    fn enabling_is_refused_until_the_write_paths_are_migrated() {
        assert_eq!(accept(false), Err(REJECT_MESSAGE));
        assert_eq!(accept(true), Err(REJECT_MESSAGE));
    }
}
