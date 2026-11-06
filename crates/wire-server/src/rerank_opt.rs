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
//! `mmr` は Issue #2143 で解禁した（λ は `--rerank-mmr-lambda`。既定 0.5・範囲 `0..=1` は engine の
//! [`engine::rerank::validate_mmr_lambda`] で判定し、書式は `^[0-9]+(\.[0-9]+)?$` の厳密な 10 進に
//! 限る）。`cross_encoder` は語彙としては受理するが本ビルドでは未対応のため固定文言で起動を拒否する
//! （黙って `off` へ読み替えない。後続 Issue #2144 で解禁）。`off`（または未指定）のまま
//! `--rerank-window` を指定する、`mmr` 以外で `--rerank-mmr-lambda` を指定する、語彙外の値、範囲外・
//! 非数値・先頭 `+`・空白付きの値も fail-closed で起動エラー。エラー文に含めるのはフラグ名・入力値・
//! 範囲だけとする。

use engine::rerank::{
    validate_mmr_lambda, validate_rerank_window, LexicalOverlapReranker, MmrReranker, RerankMethod,
    RerankSettings, DEFAULT_MMR_LAMBDA, DEFAULT_RERANK_WINDOW,
};

/// リランカー方式を選ぶ CLI フラグ名。
pub const RERANKER_FLAG: &str = "--reranker";

/// リランク窓の幅 `W` を指定する CLI フラグ名。
pub const RERANK_WINDOW_FLAG: &str = "--rerank-window";

/// MMR の λ（関連度と多様性の釣り合い）を指定する CLI フラグ名（`--reranker mmr` 専用）。
pub const RERANK_MMR_LAMBDA_FLAG: &str = "--rerank-mmr-lambda";

/// `--reranker` が受理する閉じた語彙（ADR D2）。
pub const TOKENS: [&str; 4] = ["off", "lexical", "mmr", "cross_encoder"];

/// 未パースのフラグ値の束。
#[derive(Debug, Default, Clone, Copy)]
pub struct RerankRawFlags<'a> {
    /// `--reranker` の値。
    pub reranker: Option<&'a str>,
    /// `--rerank-window` の値。
    pub window: Option<&'a str>,
    /// `--rerank-mmr-lambda` の値。
    pub mmr_lambda: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（先頭 `+`・空白・全角数字・空文字を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<usize> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// λ の厳密な 10 進パース。受理するのは `^[0-9]+(\.[0-9]+)?$` だけ（先頭 `+`／`-`・空白・
/// 指数表記・`nan`／`inf`・`.5`／`1.`・全角・空文字を弾く）。範囲は呼び出し元が engine で検証する。
fn parse_strict_lambda(raw: &str) -> Option<f64> {
    let (int_part, frac_part) = match raw.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (raw, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(int_part) || frac_part.is_some_and(|f| !digits(f)) {
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
    // λ は `mmr` 専用。他の方式（`off` を含む）での指定は黙って無視せず拒否する。
    if token != "mmr" {
        if let Some(l) = flags.mmr_lambda {
            return Err(format!(
                "{RERANK_MMR_LAMBDA_FLAG} requires {RERANKER_FLAG} mmr, got {l:?}"
            ));
        }
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
        "mmr" => {
            let err = |raw: &str| {
                format!("{RERANK_MMR_LAMBDA_FLAG} expects a decimal in 0..=1, got {raw:?}")
            };
            let lambda = match flags.mmr_lambda {
                None => DEFAULT_MMR_LAMBDA,
                Some(raw) => {
                    let v = parse_strict_lambda(raw).ok_or_else(|| err(raw))?;
                    validate_mmr_lambda(v).map_err(|_| err(raw))?
                }
            };
            let mmr = MmrReranker::new(lambda)
                .map_err(|_| format!("{RERANK_MMR_LAMBDA_FLAG} expects a decimal in 0..=1"))?;
            RerankMethod::Mmr(mmr)
        }
        // `cross_encoder`: 語彙としては正しいが本ビルドには実装が無い。
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
        RerankRawFlags {
            reranker,
            window,
            mmr_lambda: None,
        }
    }

    fn mmr_flags<'a>(lambda: Option<&'a str>) -> RerankRawFlags<'a> {
        RerankRawFlags {
            reranker: Some("mmr"),
            window: None,
            mmr_lambda: lambda,
        }
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
        let e = resolve(flags(Some("cross_encoder"), None)).expect_err("cross_encoder");
        assert!(e.contains("not supported"), "{e}");
    }

    #[test]
    fn mmr_uses_default_lambda_and_window() {
        let s = resolve(flags(Some("mmr"), None)).unwrap();
        assert!(s.is_enabled());
        assert_eq!(s.method().kind_name(), "mmr");
        assert_eq!(s.window(), DEFAULT_RERANK_WINDOW);
        let s = resolve(flags(Some("mmr"), Some("50"))).unwrap();
        assert_eq!(s.window(), 50);
    }

    #[test]
    fn mmr_lambda_accepts_boundaries_and_decimals() {
        for ok in ["0", "1", "0.0", "1.0", "0.25", "00.5", "0.50"] {
            let s = resolve(mmr_flags(Some(ok))).expect(ok);
            assert!(matches!(s.method(), RerankMethod::Mmr(_)), "{ok}");
        }
        let RerankMethod::Mmr(m) = resolve(mmr_flags(Some("0.25"))).unwrap().method().clone()
        else {
            panic!("expected mmr");
        };
        assert_eq!(m.lambda(), 0.25);
    }

    #[test]
    fn mmr_lambda_rejects_malformed_and_out_of_range() {
        for bad in [
            "+0.5",
            "-0",
            "-0.5",
            " 0.5",
            "0.5 ",
            "1e-1",
            ".5",
            "1.",
            "nan",
            "NaN",
            "inf",
            "1.5",
            "1.0000001",
            "2",
            "１",
            "0,5",
            "",
        ] {
            let e = resolve(mmr_flags(Some(bad))).expect_err(bad);
            assert!(e.contains(RERANK_MMR_LAMBDA_FLAG), "{bad}: {e}");
        }
    }

    #[test]
    fn mmr_lambda_requires_mmr() {
        for token in [None, Some("off"), Some("lexical"), Some("cross_encoder")] {
            let f = RerankRawFlags {
                reranker: token,
                window: None,
                mmr_lambda: Some("0.5"),
            };
            let e = resolve(f).expect_err("lambda without mmr");
            assert!(e.contains(RERANK_MMR_LAMBDA_FLAG), "{token:?}: {e}");
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
