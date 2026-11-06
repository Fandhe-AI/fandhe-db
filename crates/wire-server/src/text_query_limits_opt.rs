//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-text-query-terms`・
//! `--max-text-query-prefix-expansions` opt-in CLI 引数のパーサ（Issue #2115。ポインタ:
//! `docs/spec/04-behavior/` SEARCH-12）。
//!
//! `regex_limits_opt` と同型の「プロセス起動時にのみ明示指定する注入点」。範囲（各ハード上限以下
//! への引き下げのみ）の判定は engine 側の単一情報源 [`engine::text_query_limits`] の
//! `validate_*` に委ね、本モジュールは untrusted な CLI 文字列を厳密パース（先頭 `+`・空白・
//! 全角数字・空文字を弾く）して渡すだけにする。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_text_query_limits` へ 1 回だけ注入する
//! （`docs/design/text-query-limits-cli.md`）。範囲外・非数値・値欠落・重複指定は fail-closed で
//! 起動エラー（既定へ黙って読み替えない）。環境変数で上限を差し替える経路は設けない。
//! SQL／NoSQL 表層での利用は後続 Issue（#2116）。

use engine::text_query_limits::{
    validate_max_prefix_expansions, validate_max_terms, TextQueryLimits,
};

/// 検索語の項数（グループごとの一意な語の総和）の上限を設定する CLI フラグ名。
pub const MAX_TEXT_QUERY_TERMS_FLAG: &str = "--max-text-query-terms";

/// 前方一致の展開語数の上限を設定する CLI フラグ名。
pub const MAX_TEXT_QUERY_PREFIX_EXPANSIONS_FLAG: &str = "--max-text-query-prefix-expansions";

/// 2 つの CLI フラグの生の値（未指定は `None`）。`main.rs` が引数解析で集める。
#[derive(Debug, Clone, Copy, Default)]
pub struct TextQueryLimitsRawFlags<'a> {
    pub max_terms: Option<&'a str>,
    pub max_prefix_expansions: Option<&'a str>,
}

/// ASCII 数字だけを受理する厳密な 10 進パース（`str::parse::<u64>` が受理する先頭 `+` 等を
/// 弾く）。
fn parse_strict_decimal(raw: &str) -> Option<u64> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    raw.parse().ok()
}

fn parse_flag(flag: &str, raw: &str) -> Result<u64, String> {
    parse_strict_decimal(raw)
        .ok_or_else(|| format!("{flag} expects a non-negative integer, got {raw:?}"))
}

/// 2 フラグの未パース値から [`TextQueryLimits`] を解決する純関数。未指定のフラグは既定値
/// （[`TextQueryLimits::default`]＝ハード上限）、範囲外・不正値は `Err`（fail-closed）。
/// エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: TextQueryLimitsRawFlags<'_>) -> Result<TextQueryLimits, String> {
    let base = TextQueryLimits::default();
    let mut terms = base.max_terms();
    let mut expansions = base.max_prefix_expansions();
    if let Some(r) = raw.max_terms {
        let v = parse_flag(MAX_TEXT_QUERY_TERMS_FLAG, r)?;
        terms = validate_max_terms(v).map_err(|e| format!("{MAX_TEXT_QUERY_TERMS_FLAG}: {e}"))?;
    }
    if let Some(r) = raw.max_prefix_expansions {
        let v = parse_flag(MAX_TEXT_QUERY_PREFIX_EXPANSIONS_FLAG, r)?;
        expansions = validate_max_prefix_expansions(v)
            .map_err(|e| format!("{MAX_TEXT_QUERY_PREFIX_EXPANSIONS_FLAG}: {e}"))?;
    }
    Ok(TextQueryLimits::new(terms, expansions))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(t: Option<&str>, e: Option<&str>) -> Result<TextQueryLimits, String> {
        resolve(TextQueryLimitsRawFlags {
            max_terms: t,
            max_prefix_expansions: e,
        })
    }

    #[test]
    fn unspecified_is_default() {
        assert_eq!(one(None, None), Ok(TextQueryLimits::default()));
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        let l = one(Some("1"), Some("1")).expect("lower bound");
        assert_eq!((l.max_terms(), l.max_prefix_expansions()), (1, 1));
        let l = one(Some("1024"), Some("1024")).expect("upper bound");
        assert_eq!(l, TextQueryLimits::default());
    }

    #[test]
    fn rejects_out_of_range_and_malformed_values() {
        for bad in [
            "0",
            "+5",
            " 5",
            "5 ",
            "5a",
            "",
            "１２",
            "-1",
            "99999999999999999999999",
            "1025",
        ] {
            let e = one(Some(bad), None).expect_err(bad);
            assert!(e.contains(MAX_TEXT_QUERY_TERMS_FLAG), "{bad}: {e}");
            let e = one(None, Some(bad)).expect_err(bad);
            assert!(
                e.contains(MAX_TEXT_QUERY_PREFIX_EXPANSIONS_FLAG),
                "{bad}: {e}"
            );
        }
    }

    #[test]
    fn only_the_given_flag_changes() {
        let l = one(None, Some("8")).expect("expansions only");
        assert_eq!(l.max_prefix_expansions(), 8);
        assert_eq!(l.max_terms(), TextQueryLimits::default().max_terms());
    }
}
