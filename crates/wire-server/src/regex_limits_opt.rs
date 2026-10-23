//! `wire-server` バイナリ（`main.rs`）が起動時に受け取る `--max-regex-pattern-len`・
//! `--max-regex-input-len`・`--max-regex-compiled-size` opt-in CLI 引数のパーサ
//! （Issue #1786。ポインタ: `docs/spec/04-behavior/` SQL-55）。
//!
//! `jsonpath_limits_opt`（`--max-jsonpath-steps` 等）と同型の「プロセス起動時にのみ明示指定
//! する注入点」。範囲（各ハード上限以下への引き下げのみ）の判定は engine 側の単一情報源
//! [`engine::regex_limits`] の `validate_*` に委ね、本モジュールは untrusted な CLI 文字列を
//! 厳密パース（先頭 `+`・空白・全角数字・空文字を弾く）して渡すだけにする。各モジュールが
//! 独立した小さい下請けを持つ既存の流儀を踏襲し、共有ヘルパーへの切り出しは行わない。
//!
//! 解決結果は `main.rs` が bind・ユーザーストア読込より前に [`resolve`] で確定し、
//! `EngineCore::with_regex_limits` へ 1 回だけ注入する（`docs/design/regex-limits-cli.md`）。
//! 範囲外・非数値・値欠落・重複指定は fail-closed で起動エラー（既定へ黙って読み替えない）。
//! 環境変数で上限を差し替える経路は設けない。

use engine::regex_limits::{
    validate_max_compiled_size, validate_max_input_len, validate_max_pattern_len, RegexLimits,
};

/// 正規表現パターン長（バイト）の上限を設定する CLI フラグ名。
pub const MAX_REGEX_PATTERN_LEN_FLAG: &str = "--max-regex-pattern-len";

/// 正規表現マッチの入力長（バイト）の上限を設定する CLI フラグ名。
pub const MAX_REGEX_INPUT_LEN_FLAG: &str = "--max-regex-input-len";

/// 正規表現のコンパイル後サイズ（バイト）の上限を設定する CLI フラグ名。
pub const MAX_REGEX_COMPILED_SIZE_FLAG: &str = "--max-regex-compiled-size";

/// 3 つの CLI フラグの生の値（未指定は `None`）。`main.rs` が引数解析で集める。
#[derive(Debug, Clone, Copy, Default)]
pub struct RegexLimitsRawFlags<'a> {
    pub max_pattern_len: Option<&'a str>,
    pub max_input_len: Option<&'a str>,
    pub max_compiled_size: Option<&'a str>,
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

/// 3 フラグの未パース値から [`RegexLimits`] を解決する純関数。未指定のフラグは既定値
/// （[`RegexLimits::default`]＝ハード上限）、範囲外・不正値は `Err`（fail-closed）。
/// エラー文にはフラグ名・入力値・範囲だけを含める。
pub fn resolve(raw: RegexLimitsRawFlags<'_>) -> Result<RegexLimits, String> {
    let base = RegexLimits::default();
    let mut pattern_len = base.max_pattern_len();
    let mut input_len = base.max_input_len();
    let mut compiled_size = base.max_compiled_size();
    if let Some(r) = raw.max_pattern_len {
        let v = parse_flag(MAX_REGEX_PATTERN_LEN_FLAG, r)?;
        pattern_len = validate_max_pattern_len(v)
            .map_err(|e| format!("{MAX_REGEX_PATTERN_LEN_FLAG}: {e}"))?;
    }
    if let Some(r) = raw.max_input_len {
        let v = parse_flag(MAX_REGEX_INPUT_LEN_FLAG, r)?;
        input_len =
            validate_max_input_len(v).map_err(|e| format!("{MAX_REGEX_INPUT_LEN_FLAG}: {e}"))?;
    }
    if let Some(r) = raw.max_compiled_size {
        let v = parse_flag(MAX_REGEX_COMPILED_SIZE_FLAG, r)?;
        compiled_size = validate_max_compiled_size(v)
            .map_err(|e| format!("{MAX_REGEX_COMPILED_SIZE_FLAG}: {e}"))?;
    }
    Ok(RegexLimits::new(pattern_len, input_len, compiled_size))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(p: Option<&str>, i: Option<&str>, c: Option<&str>) -> Result<RegexLimits, String> {
        resolve(RegexLimitsRawFlags {
            max_pattern_len: p,
            max_input_len: i,
            max_compiled_size: c,
        })
    }

    #[test]
    fn unspecified_is_default() {
        assert_eq!(one(None, None, None), Ok(RegexLimits::default()));
    }

    #[test]
    fn accepts_lower_and_upper_boundaries() {
        let l = one(Some("1"), Some("1"), Some("1")).expect("lower bound");
        assert_eq!(
            (
                l.max_pattern_len(),
                l.max_input_len(),
                l.max_compiled_size()
            ),
            (1, 1, 1)
        );
        let l = one(Some("4096"), Some("4194304"), Some("10485760")).expect("upper bound");
        assert_eq!(l, RegexLimits::default());
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
        ] {
            let e = one(Some(bad), None, None).expect_err(bad);
            assert!(e.contains(MAX_REGEX_PATTERN_LEN_FLAG), "{bad}: {e}");
            let e = one(None, Some(bad), None).expect_err(bad);
            assert!(e.contains(MAX_REGEX_INPUT_LEN_FLAG), "{bad}: {e}");
            let e = one(None, None, Some(bad)).expect_err(bad);
            assert!(e.contains(MAX_REGEX_COMPILED_SIZE_FLAG), "{bad}: {e}");
        }
        assert!(one(Some("4097"), None, None).is_err());
        assert!(one(None, Some("4194305"), None).is_err());
        assert!(one(None, None, Some("10485761")).is_err());
    }

    #[test]
    fn only_the_given_flag_changes() {
        let l = one(None, Some("8"), None).expect("input only");
        assert_eq!(l.max_input_len(), 8);
        assert_eq!(
            l.max_pattern_len(),
            RegexLimits::default().max_pattern_len()
        );
        assert_eq!(
            l.max_compiled_size(),
            RegexLimits::default().max_compiled_size()
        );
    }
}
