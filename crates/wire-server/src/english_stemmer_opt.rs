//! `--english-stemmer` CLI 引数の受理／拒否の判定（Issue #2129 Phase A・#3050 Phase B・
//! SEARCH-16 ポインタ）。
//!
//! 英語ステミングは engine の cargo feature `english-stemmer`（既定 off。依存クレートの承認は
//! `docs/design/stemming-crate-adoption.md` 第 8 節）でのみ提供する。wire-server 自身は feature を
//! 持たず、実行時に [`engine::sparse::analysis::english_stemmer_available`] で分岐する
//! （`--features engine/english-stemmer` でビルドしたときだけ受理）。feature 無効のビルドで
//! 指定されたら「未知引数」ではなく「未提供」と分かる固定文言で起動を拒否する（fail-closed）。
//! `main.rs` の引数ループから呼ばれ、値は読まず・echo しない（スイッチのみ）。受理した設定は
//! `text_analysis_opt::resolve` が `TextAnalysis::with_english_stemmer` へ渡す。

/// 拒否対象の CLI フラグ名。
pub const FLAG: &str = "--english-stemmer";

/// 起動拒否時に stderr へ出す固定文言（引数値・テナント情報を含めない）。
pub const REJECT_MESSAGE: &str = "--english-stemmer is not supported in this build: English stemming requires an approved dependency and the engine feature, which are not enabled";

/// 2 回目以降の指定を拒否する固定文言（last-wins にしない）。
pub const DUPLICATE_MESSAGE: &str = "--english-stemmer specified more than once";

/// 拒否メッセージを返す純関数。
pub fn reject() -> &'static str {
    REJECT_MESSAGE
}

/// フラグ 1 回分の受理判定。`already_set` は既に 1 回指定済みか。
/// feature 無効のビルドは常に [`REJECT_MESSAGE`]、有効でも重複指定は [`DUPLICATE_MESSAGE`]。
pub fn accept(already_set: bool) -> Result<(), &'static str> {
    if !engine::sparse::analysis::english_stemmer_available() {
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
    fn reject_message_is_fixed_and_names_flag() {
        assert!(reject().starts_with(FLAG));
    }

    #[test]
    fn accept_depends_on_feature_and_rejects_duplicates() {
        if engine::sparse::analysis::english_stemmer_available() {
            assert_eq!(accept(false), Ok(()));
            assert_eq!(accept(true), Err(DUPLICATE_MESSAGE));
        } else {
            assert_eq!(accept(false), Err(REJECT_MESSAGE));
            assert_eq!(accept(true), Err(REJECT_MESSAGE));
        }
    }
}
