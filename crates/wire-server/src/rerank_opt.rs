//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--reranker`／`--rerank-window` の
//! opt-in CLI 引数のパーサ（Issue #2142。ビヘイビア SEARCH-15 のポインタ。設計記録:
//! `docs/design/rerank-wiring-policy.md` D1・D2）。
//!
//! `search_engine_opt`・`statement_timeout_opt` と同型の「プロセス起動時にのみ明示指定する
//! 注入点」。語彙（`off`／`lexical`／`mmr`／`cross_encoder` の閉じた 4 語）はここで厳密一致
//! だけで判定し（engine 側は `FromStr` を持たない）、窓幅の範囲判定は engine の単一情報源
//! [`engine::rerank::validate_rerank_window`] に委ねる。解決結果は `main.rs` が bind・ユーザー
//! ストア読込より前に [`resolve`] で確定し、`EngineCore::with_rerank` へ 1 回だけ注入する
//! （セッション・SQL・NoSQL からは変更できない）。
//!
//! `mmr`・`cross_encoder` は語彙としては受理するが、本ビルドでは未対応のため固定文言で起動を
//! 拒否する（黙って `off` へ読み替えない。後続 Issue #2143・#2144 で解禁）。`off`（または未指定）
//! のまま `--rerank-window` を指定する、語彙外の値、範囲外・非数値・先頭 `+`・空白付きの値も
//! fail-closed で起動エラー。エラー文に含めるのはフラグ名・入力値・範囲だけとする。

use engine::rerank::{
    validate_rerank_window, LexicalOverlapReranker, RerankMethod, RerankSettings,
    DEFAULT_RERANK_WINDOW,
};

/// リランカー方式を選ぶ CLI フラグ名。
pub const RERANKER_FLAG: &str = "--reranker";

/// リランク窓の幅 `W` を指定する CLI フラグ名。
pub const RERANK_WINDOW_FLAG: &str = "--rerank-window";

/// `--reranker` が受理する閉じた語彙（ADR D2）。
pub const TOKENS: [&str; 4] = ["off", "lexical", "mmr", "cross_encoder"];

/// 未パースのフラグ値の束。
#[derive(Debug, Default, Clone, Copy)]
pub struct RerankRawFlags<'a> {
    /// `--reranker` の値。
    pub reranker: Option<&'a str>,
    /// `--rerank-window` の値。
    pub window: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（先頭 `+`・空白・全角数字・空文字を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<usize> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// 未パース値からリランク設定を解決する純関数。未指定は off（従来経路）。
pub fn resolve(flags: RerankRawFlags<'_>) -> Result<RerankSettings, String> {
    let token = flags.reranker.unwrap_or("off");
    if !TOKENS.contains(&token) {
        return Err(format!(
            "{RERANKER_FLAG} expects one of off, lexical, mmr, cross_encoder, got {token:?}"
        ));
    }
    let method = match token {
        "off" => {
            if let Some(w) = flags.window {
                return Err(format!(
                    "{RERANK_WINDOW_FLAG} requires {RERANKER_FLAG} other than off, got {w:?}"
                ));
            }
            return Ok(RerankSettings::off());
        }
        "lexical" => RerankMethod::Lexical(LexicalOverlapReranker::default()),
        // `mmr`・`cross_encoder`: 語彙としては正しいが本ビルドには実装が無い。
        _ => {
            return Err(format!(
                "{RERANKER_FLAG} {token} is not supported in this build"
            ));
        }
    };
    let window = match flags.window {
        None => DEFAULT_RERANK_WINDOW,
        Some(raw) => {
            let v = parse_strict_decimal(raw).ok_or_else(|| {
                format!("{RERANK_WINDOW_FLAG} expects an integer in 1..=10000, got {raw:?}")
            })?;
            validate_rerank_window(v).map_err(|_| {
                format!("{RERANK_WINDOW_FLAG} expects an integer in 1..=10000, got {raw:?}")
            })?
        }
    };
    RerankSettings::new(method, window)
        .map_err(|_| format!("{RERANK_WINDOW_FLAG} expects an integer in 1..=10000"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags<'a>(reranker: Option<&'a str>, window: Option<&'a str>) -> RerankRawFlags<'a> {
        RerankRawFlags { reranker, window }
    }

    #[test]
    fn unspecified_and_off_are_disabled() {
        assert!(!resolve(flags(None, None)).unwrap().is_enabled());
        assert!(!resolve(flags(Some("off"), None)).unwrap().is_enabled());
    }

    #[test]
    fn lexical_uses_default_or_explicit_window() {
        let s = resolve(flags(Some("lexical"), None)).unwrap();
        assert!(s.is_enabled());
        assert_eq!(s.window(), DEFAULT_RERANK_WINDOW);
        assert_eq!(s.method().kind_name(), "lexical");
        let s = resolve(flags(Some("lexical"), Some("50"))).unwrap();
        assert_eq!(s.window(), 50);
        assert_eq!(
            resolve(flags(Some("lexical"), Some("1"))).unwrap().window(),
            1
        );
        assert_eq!(
            resolve(flags(Some("lexical"), Some("10000")))
                .unwrap()
                .window(),
            10_000
        );
    }

    #[test]
    fn rejects_unknown_and_unsupported_tokens() {
        for bad in [
            "", "Lexical", " lexical", "lexical ", "identity", "none", "ON",
        ] {
            let e = resolve(flags(Some(bad), None)).expect_err(bad);
            assert!(e.contains(RERANKER_FLAG), "{bad}: {e}");
        }
        for unsupported in ["mmr", "cross_encoder"] {
            let e = resolve(flags(Some(unsupported), None)).expect_err(unsupported);
            assert!(e.contains("not supported"), "{unsupported}: {e}");
        }
    }

    #[test]
    fn rejects_malformed_window() {
        for bad in [
            "0",
            "10001",
            "-1",
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "99999999999999999999999",
        ] {
            let e = resolve(flags(Some("lexical"), Some(bad))).expect_err(bad);
            assert!(e.contains(RERANK_WINDOW_FLAG), "{bad}: {e}");
        }
    }

    #[test]
    fn window_without_active_reranker_is_rejected() {
        assert!(resolve(flags(None, Some("10"))).is_err());
        assert!(resolve(flags(Some("off"), Some("10"))).is_err());
    }
}
