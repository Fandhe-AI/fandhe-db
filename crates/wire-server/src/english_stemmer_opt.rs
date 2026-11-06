//! `--english-stemmer` CLI 引数の専用拒否入口（Issue #2129 Phase A・SEARCH-16 ポインタ）。
//!
//! 英語ステミングは依存クレートのオーナー承認（`docs/design/stemming-crate-adoption.md`
//! 第 8 節）と engine の cargo feature（既定 off）が揃うまで提供しない。承認前に指定されても
//! 「未知引数」ではなく「未提供」であることが分かる固定文言で起動を拒否する（fail-closed）。
//! `main.rs` の引数ループから呼ばれ、値の有無にかかわらず値を読まず・echo しない。
//! feature を有効にしたビルドではこの入口が受理側に置き換わる（承認後の Phase B で確定）。

/// 拒否対象の CLI フラグ名。
pub const FLAG: &str = "--english-stemmer";

/// 起動拒否時に stderr へ出す固定文言（引数値・テナント情報を含めない）。
pub const REJECT_MESSAGE: &str = "--english-stemmer is not supported in this build: English stemming requires an approved dependency and the engine feature, which are not enabled";

/// 拒否メッセージを返す純関数。
pub fn reject() -> &'static str {
    REJECT_MESSAGE
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_message_is_fixed_and_names_flag() {
        assert!(reject().starts_with(FLAG));
    }
}
