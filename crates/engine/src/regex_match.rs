//! 正規表現マッチャ（PG の `~`／`~*` 用。Issue #1786。ポインタ: `docs/spec/04-behavior/`
//! SQL-55）。
//!
//! 親 Issue #1784 の方針に従い、線形時間保証のある `regex` クレート
//! （`docs/design/regex-crate-adoption.md`）を engine 内部 API で包む。公開シグネチャに
//! `regex` の型は出さない（呼び出し側は [`CompiledRegex`]・[`RegexError`]・[`RegexCaseMode`]
//! だけを見る）。SQL 表層への配線（`~` 系は Issue #1787、`SIMILAR TO` は #1788、jsonpath の
//! `like_regex` は #1784）は本モジュールの範囲外で、ここは評価器の部品だけを提供する。
//!
//! 処理順序（`docs/design/regex-matcher.md`）:
//!
//! 1. パターン長（生のバイト長）を確保・解析の前に検査（`54000`）
//! 2. 字句変換（[`translate`]。PG ARE → `regex` 構文。非対応は `0A000`、構文誤りは `2201B`）
//! 3. `RegexBuilder` へ `size_limit`・`dfa_size_limit`・`nest_limit` を設定してコンパイル
//!    （`CompiledTooBig` は `54000`）
//! 4. マッチ時は入力のバイト長を検査してから `is_match`（真偽のみ。最長一致規則と
//!    `regex` の leftmost-first の差は結果に影響しない）
//!
//! 意味論は C 照合に限る。ICU 照合の式を拒否するのは配線側（Issue #1787）の責務。
//! エラー文言は固定の英語で、パターン・入力の断片や `regex::Error` の文面
//! （パターン文字列を含む）はクライアントへ運ばない（fail-closed・情報漏えい防止）。

mod translate;

use crate::error_format::{ClassifiedError, ErrorClass};
use crate::regex_limits::{RegexLimits, MAX_REGEX_NEST_DEPTH, REGEX_DFA_CACHE_BYTES};

/// 大文字小文字の扱い。bool にしないのは、照合依存の畳み込み（ICU）を将来足す余地を
/// 残すため。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegexCaseMode {
    /// `~`: 区別する（PG の埋め込みオプション `(?i)` はパターン側で上書きされうる）。
    Sensitive,
    /// `~*`: ASCII 英字だけを区別しない（C 照合の PG と同じ。Unicode の畳み込みはしない）。
    AsciiInsensitive,
}

/// 正規表現マッチャのエラー。`ClassifiedError` を実装し、wire 層へは固定文言で写る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegexError {
    /// パターンが PG の構文としても不正（`2201B`）。
    InvalidPattern,
    /// PG は受理するが本実装は写像しない構文（後方参照・先読み等。`0A000`）。
    Unsupported,
    /// パターン長が上限超過（`54000`）。
    PatternTooLong,
    /// 入力長が上限超過（`54000`）。
    InputTooLong,
    /// パターンが複雑すぎる（コンパイル後サイズ・入れ子の上限超過。`54000`）。
    TooComplex,
}

impl std::fmt::Display for RegexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.fixed_message())
    }
}

impl std::error::Error for RegexError {}

impl RegexError {
    /// クライアントへ返してよい固定の英語文言（パターン・入力の断片を含めない）。
    fn fixed_message(&self) -> &'static str {
        match self {
            Self::InvalidPattern => "invalid regular expression",
            Self::Unsupported => "regular expression feature is not supported",
            Self::PatternTooLong => "regular expression pattern too long",
            Self::InputTooLong => "regular expression input too long",
            Self::TooComplex => "regular expression is too complex",
        }
    }
}

impl ClassifiedError for RegexError {
    fn error_class(&self) -> ErrorClass {
        match self {
            Self::InvalidPattern => ErrorClass::InvalidRegularExpression,
            Self::Unsupported => ErrorClass::FeatureNotSupported,
            Self::PatternTooLong | Self::InputTooLong | Self::TooComplex => {
                ErrorClass::PayloadTooLarge
            }
        }
    }

    fn client_message(&self) -> String {
        self.fixed_message().to_string()
    }
}

impl From<translate::TranslateError> for RegexError {
    fn from(e: translate::TranslateError) -> Self {
        match e {
            translate::TranslateError::Syntax => Self::InvalidPattern,
            translate::TranslateError::Unsupported => Self::Unsupported,
            translate::TranslateError::TooDeep => Self::TooComplex,
        }
    }
}

/// コンパイル済みの正規表現。`Clone` は内部の `Arc` 共有で安価。
#[derive(Debug, Clone)]
pub struct CompiledRegex {
    re: regex::Regex,
    max_input_len: usize,
}

impl CompiledRegex {
    /// PG のパターンをコンパイルする。パターン長は確保・解析より前に検査する。
    pub fn compile(
        pattern: &str,
        case: RegexCaseMode,
        limits: RegexLimits,
    ) -> Result<Self, RegexError> {
        if pattern.len() > limits.max_pattern_len() {
            return Err(RegexError::PatternTooLong);
        }
        let translated = translate::translate(pattern, case == RegexCaseMode::AsciiInsensitive)?;
        // 自前の深さ上限に、変換が足すラッパー（量指定・クラス）の分の余裕を持たせた多重防御。
        let nest_limit = u32::try_from(MAX_REGEX_NEST_DEPTH.saturating_mul(4).saturating_add(16))
            .unwrap_or(u32::MAX);
        let re = regex::RegexBuilder::new(&translated)
            .size_limit(limits.max_compiled_size())
            .dfa_size_limit(REGEX_DFA_CACHE_BYTES)
            .nest_limit(nest_limit)
            .build()
            .map_err(|e| match e {
                regex::Error::CompiledTooBig(_) => RegexError::TooComplex,
                // 変換が正しければ `Syntax` は起きない。起きた場合も拒否側（構文誤り）に倒す。
                _ => RegexError::InvalidPattern,
            })?;
        Ok(Self {
            re,
            max_input_len: limits.max_input_len(),
        })
    }

    /// 入力にパターンがマッチするか。入力長は走査の前に検査する。
    pub fn is_match(&self, input: &str) -> Result<bool, RegexError> {
        if input.len() > self.max_input_len {
            return Err(RegexError::InputTooLong);
        }
        Ok(self.re.is_match(input))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(p: &str, input: &str) -> Result<bool, RegexError> {
        CompiledRegex::compile(p, RegexCaseMode::Sensitive, RegexLimits::default())?.is_match(input)
    }

    #[test]
    fn basic_matching() {
        assert_eq!(m("^a+b$", "aaab"), Ok(true));
        assert_eq!(m("^a+b$", "aaa"), Ok(false));
        assert_eq!(m("a.c", "a\nc"), Ok(true));
    }

    #[test]
    fn case_insensitive_is_ascii_only() {
        let c = |p: &str, i: &str| {
            CompiledRegex::compile(p, RegexCaseMode::AsciiInsensitive, RegexLimits::default())
                .unwrap()
                .is_match(i)
        };
        assert_eq!(c("abc", "ABC"), Ok(true));
        assert_eq!(c("é", "É"), Ok(false));
        assert_eq!(c("k", "\u{212a}"), Ok(false));
    }

    #[test]
    fn error_messages_are_fixed() {
        for e in [
            RegexError::InvalidPattern,
            RegexError::Unsupported,
            RegexError::PatternTooLong,
            RegexError::InputTooLong,
            RegexError::TooComplex,
        ] {
            assert_eq!(e.client_message(), e.to_string());
        }
        assert_eq!(m("(a)\\1", "aa").unwrap_err().wire_code(), "0A000");
        assert_eq!(m("(", "a").unwrap_err().wire_code(), "2201B");
    }

    #[test]
    fn limits_apply_at_boundaries() {
        let l = RegexLimits::new(3, 4, 1_000_000);
        assert!(CompiledRegex::compile("abc", RegexCaseMode::Sensitive, l).is_ok());
        assert_eq!(
            CompiledRegex::compile("abcd", RegexCaseMode::Sensitive, l).unwrap_err(),
            RegexError::PatternTooLong
        );
        let re = CompiledRegex::compile("a", RegexCaseMode::Sensitive, l).unwrap();
        assert_eq!(re.is_match("aaaa"), Ok(true));
        assert_eq!(re.is_match("aaaaa"), Err(RegexError::InputTooLong));
    }
}
