//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--reranker`／`--rerank-window`／
//! `--rerank-model`／`--rerank-tokenizer`／`--rerank-timeout-ms` の opt-in CLI 引数のパーサ
//! （Issue #2142・#2144。ビヘイビア SEARCH-15 のポインタ。設計記録:
//! `docs/design/rerank-wiring-policy.md` D1・D2・D5）。
//!
//! `search_engine_opt`・`statement_timeout_opt` と同型の「プロセス起動時にのみ明示指定する
//! 注入点」。語彙（`off`／`lexical`／`mmr`／`cross_encoder` の閉じた 4 語）はここで厳密一致
//! だけで判定し（engine 側は `FromStr` を持たない）、窓幅・期限の範囲判定は engine の単一情報源
//! （`validate_rerank_window`・`validate_rerank_timeout_ms`）に委ねる。解決結果は `main.rs` が
//! bind・ユーザーストア読込より前に [`resolve`] で確定し、`EngineCore::with_rerank` へ 1 回だけ
//! 注入する（セッション・SQL・NoSQL からは変更できない）。
//!
//! `mmr` は Issue #2143 で解禁した（λ は `--rerank-mmr-lambda`。既定 0.5・範囲 `0..=1` は engine の
//! [`engine::rerank::validate_mmr_lambda`] で判定し、書式は `^[0-9]+(\.[0-9]+)?$` の厳密な 10 進に
//! 限る。`mmr` 以外での指定は起動エラー）。`cross_encoder`（Issue #2144）は
//! `--rerank-model`／`--rerank-tokenizer`（必須）と `--rerank-timeout-ms`（`1..=60000`、既定
//! 1000）を伴い、engine の `cross-encoder` feature 付きビルドでのみ受理する（無効ビルドでは
//! 固定文言で起動拒否）。モデルのパス検証・サイズ上限・読み込みは engine の
//! `onnx_cross_encoder_method` に一本化し、失敗は起動エラーにする。`off`（または未指定）
//! のまま `--rerank-window` を指定する、`cross_encoder` 以外でモデル系フラグを指定する、
//! 語彙外の値、範囲外・非数値・先頭 `+`・空白付きの値も fail-closed で起動エラー。
//! エラー文に含めるのはフラグ名・入力値（窓幅・期限・λ のみ）・範囲だけで、パスとファイルの
//! 中身は出さない。

use std::path::Path;

use engine::rerank::{
    cross_encoder_available, onnx_cross_encoder_method, validate_mmr_lambda,
    validate_rerank_timeout_ms, validate_rerank_window, CrossEncoderError, CrossEncoderFile,
    LexicalOverlapReranker, MmrReranker, RerankMethod, RerankSettings, DEFAULT_MMR_LAMBDA,
    DEFAULT_RERANK_TIMEOUT_MS, DEFAULT_RERANK_WINDOW,
};

/// リランカー方式を選ぶ CLI フラグ名。
pub const RERANKER_FLAG: &str = "--reranker";

/// リランク窓の幅 `W` を指定する CLI フラグ名。
pub const RERANK_WINDOW_FLAG: &str = "--rerank-window";

/// クロスエンコーダの ONNX モデルファイルを指定する CLI フラグ名（Issue #2144）。
pub const RERANK_MODEL_FLAG: &str = "--rerank-model";

/// クロスエンコーダの `tokenizer.json` を指定する CLI フラグ名（Issue #2144）。
pub const RERANK_TOKENIZER_FLAG: &str = "--rerank-tokenizer";

/// クロスエンコーダの推論期限（ミリ秒）を指定する CLI フラグ名（Issue #2144）。
pub const RERANK_TIMEOUT_FLAG: &str = "--rerank-timeout-ms";

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
    /// `--rerank-model` の値（`cross_encoder` のときだけ指定可・必須）。
    pub model: Option<&'a Path>,
    /// `--rerank-tokenizer` の値（`cross_encoder` のときだけ指定可・必須）。
    pub tokenizer: Option<&'a Path>,
    /// `--rerank-timeout-ms` の値（`cross_encoder` のときだけ指定可。既定 1000）。
    pub timeout_ms: Option<&'a str>,
    /// `--rerank-mmr-lambda` の値。
    pub mmr_lambda: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（先頭 `+`・空白・全角数字・空文字を弾く）。
fn parse_strict_decimal(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

/// 窓幅の書式・範囲を検証する。
fn parse_window(raw: Option<&str>) -> Result<usize, String> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_RERANK_WINDOW);
    };
    let err = || format!("{RERANK_WINDOW_FLAG} expects an integer in 1..=10000, got {raw:?}");
    let v = parse_strict_decimal(raw)
        .and_then(|v| usize::try_from(v).ok())
        .ok_or_else(err)?;
    validate_rerank_window(v).map_err(|_| err())
}

/// 推論期限の書式・範囲を検証する。
fn parse_timeout(raw: Option<&str>) -> Result<std::time::Duration, String> {
    let err =
        |got: &str| format!("{RERANK_TIMEOUT_FLAG} expects an integer in 1..=60000, got {got:?}");
    let ms = match raw {
        None => DEFAULT_RERANK_TIMEOUT_MS,
        Some(raw) => parse_strict_decimal(raw).ok_or_else(|| err(raw))?,
    };
    validate_rerank_timeout_ms(ms).map_err(|_| err(&ms.to_string()))
}

/// モデル構築失敗を起動エラー文へ変換する。パスとファイル内容は出さない
/// （`ModelFile` は閉じた固定語、それ以外は engine の固定文言）。
fn load_error(e: &CrossEncoderError) -> String {
    match e {
        CrossEncoderError::ModelFile { file, reason } => {
            let flag = match file {
                CrossEncoderFile::Model => RERANK_MODEL_FLAG,
                CrossEncoderFile::Tokenizer => RERANK_TOKENIZER_FLAG,
            };
            format!("{flag} rejected ({reason})")
        }
        // `Backend(String)` はバックエンド由来の文言（入力名・ライブラリのパス等）を含み得るため
        // 出力しない。他の variant も含め固定文言だけを返す。
        CrossEncoderError::Backend(_) => {
            format!("{RERANKER_FLAG} cross_encoder: failed to initialize model backend")
        }
        _ => format!("{RERANKER_FLAG} cross_encoder: failed to load model"),
    }
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

/// 未パース値からリランク設定を解決する。未指定は off（従来経路）。`cross_encoder` のときだけ
/// モデル／トークナイザの読み込み（ファイル I/O）を行う。
///
/// 判定順: 語彙 → フラグの組み合わせ → 窓幅・期限の書式 → ビルドの対応可否 → 読み込み。
/// 前半は純粋な検証で、feature の有無によらず同じ結果になる。
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
    if token == "off" {
        if let Some(w) = flags.window {
            return Err(format!(
                "{RERANK_WINDOW_FLAG} requires {RERANKER_FLAG} other than off, got {w:?}"
            ));
        }
    }
    if token != "cross_encoder" {
        let ce_flags = [
            (RERANK_MODEL_FLAG, flags.model.is_some()),
            (RERANK_TOKENIZER_FLAG, flags.tokenizer.is_some()),
            (RERANK_TIMEOUT_FLAG, flags.timeout_ms.is_some()),
        ];
        if let Some((flag, _)) = ce_flags.iter().find(|(_, present)| *present) {
            return Err(format!(
                "{flag} requires {RERANKER_FLAG} cross_encoder, got {token:?}"
            ));
        }
    }
    if token == "off" {
        return Ok(RerankSettings::off());
    }
    let window = parse_window(flags.window)?;
    let method = match token {
        "lexical" => RerankMethod::Lexical(LexicalOverlapReranker::default()),
        "cross_encoder" => {
            let Some(model) = flags.model else {
                return Err(format!(
                    "{RERANKER_FLAG} cross_encoder requires {RERANK_MODEL_FLAG}"
                ));
            };
            let Some(tokenizer) = flags.tokenizer else {
                return Err(format!(
                    "{RERANKER_FLAG} cross_encoder requires {RERANK_TOKENIZER_FLAG}"
                ));
            };
            let timeout = parse_timeout(flags.timeout_ms)?;
            if !cross_encoder_available() {
                return Err(format!(
                    "{RERANKER_FLAG} cross_encoder is not supported in this build"
                ));
            }
            onnx_cross_encoder_method(model, tokenizer, window, timeout)
                .map_err(|e| load_error(&e))?
        }
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
        _ => {
            return Err(format!(
                "{RERANKER_FLAG} {token} is not supported in this build"
            ));
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
            ..Default::default()
        }
    }

    fn ce<'a>(
        model: Option<&'a Path>,
        tokenizer: Option<&'a Path>,
        timeout_ms: Option<&'a str>,
    ) -> RerankRawFlags<'a> {
        RerankRawFlags {
            reranker: Some("cross_encoder"),
            model,
            tokenizer,
            timeout_ms,
            ..Default::default()
        }
    }

    fn mmr_flags<'a>(lambda: Option<&'a str>) -> RerankRawFlags<'a> {
        RerankRawFlags {
            reranker: Some("mmr"),
            mmr_lambda: lambda,
            ..Default::default()
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
                mmr_lambda: Some("0.5"),
                ..Default::default()
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

    #[test]
    fn cross_encoder_flags_require_cross_encoder() {
        let p = Path::new("/nonexistent/secret-model-path");
        for reranker in [None, Some("off"), Some("lexical"), Some("mmr")] {
            for (flag, raw) in [
                (
                    RERANK_MODEL_FLAG,
                    RerankRawFlags {
                        reranker,
                        model: Some(p),
                        ..Default::default()
                    },
                ),
                (
                    RERANK_TOKENIZER_FLAG,
                    RerankRawFlags {
                        reranker,
                        tokenizer: Some(p),
                        ..Default::default()
                    },
                ),
                (
                    RERANK_TIMEOUT_FLAG,
                    RerankRawFlags {
                        reranker,
                        timeout_ms: Some("10"),
                        ..Default::default()
                    },
                ),
            ] {
                let e = resolve(raw).expect_err(flag);
                assert!(e.contains(flag), "{reranker:?} {flag}: {e}");
                assert!(!e.contains("secret-model-path"), "{e}");
            }
        }
    }

    #[test]
    fn cross_encoder_requires_model_and_tokenizer() {
        let p = Path::new("/nonexistent/secret-model-path");
        let e = resolve(ce(None, Some(p), None)).expect_err("no model");
        assert!(e.contains(RERANK_MODEL_FLAG), "{e}");
        let e = resolve(ce(Some(p), None, None)).expect_err("no tokenizer");
        assert!(e.contains(RERANK_TOKENIZER_FLAG), "{e}");
    }

    #[test]
    fn cross_encoder_rejects_malformed_timeout_regardless_of_build() {
        let p = Path::new("/nonexistent/secret-model-path");
        for bad in [
            "0",
            "60001",
            "+5",
            " 5",
            "",
            "abc",
            "-1",
            "99999999999999999999999",
        ] {
            let e = resolve(ce(Some(p), Some(p), Some(bad))).expect_err(bad);
            assert!(e.contains(RERANK_TIMEOUT_FLAG), "{bad}: {e}");
        }
    }

    #[test]
    fn cross_encoder_rejects_malformed_window_regardless_of_build() {
        let p = Path::new("/nonexistent/secret-model-path");
        let mut f = ce(Some(p), Some(p), None);
        f.window = Some("0");
        let e = resolve(f).expect_err("window");
        assert!(e.contains(RERANK_WINDOW_FLAG), "{e}");
    }

    /// 書式が正しくても、feature 無効なら「未対応」、有効ならモデル読み込み失敗で拒否される。
    /// どちらの場合もエラー文にパスを含めない。
    #[test]
    fn cross_encoder_with_missing_files_fails_closed_without_leaking_paths() {
        let dir = Path::new("/nonexistent/secret-model-path");
        let model = dir.join("model.onnx");
        let tok = dir.join("tokenizer.json");
        let e = resolve(ce(Some(&model), Some(&tok), Some("500"))).expect_err("must fail");
        assert!(!e.contains("secret-model-path"), "{e}");
        if cross_encoder_available() {
            assert!(e.contains(RERANK_MODEL_FLAG), "{e}");
        } else {
            assert!(e.contains("not supported in this build"), "{e}");
        }
    }
}
