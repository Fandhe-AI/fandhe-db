//! 正規表現マッチャ（[`crate::regex_match`]）の「パターン長」「入力長」「コンパイル後サイズ」
//! 上限の起動時設定（Issue #1786。ポインタ: `docs/spec/04-behavior/` SQL-55）。
//!
//! `wire-server` の `regex_limits_opt` が CLI フラグ（`--max-regex-pattern-len`・
//! `--max-regex-input-len`・`--max-regex-compiled-size`）を厳密パースし、本モジュールの
//! `validate_*` で範囲検証したうえで [`RegexLimits`] を組み立て、
//! `EngineCore::with_regex_limits` へ 1 回だけ注入する（`jsonpath_limits` と同じ流儀）。
//! 述語評価（`~` 系は Issue #1787 で配線済みで `sql::regex_op` が文の束縛区間へ運ぶ。`SIMILAR TO`・
//! jsonpath `like_regex` は #1788・#1784）は `EngineCore::regex_limits` から取り出して
//! [`crate::regex_match::CompiledRegex::compile`] へ渡す。範囲検証の単一情報源は本モジュール。
//!
//! 設計上の不変条件（`docs/design/regex-limits-cli.md`）:
//!
//! - **ハード上限が既定値**。設定できるのは「下げる方向」だけで、フラグ未指定ならハード
//!   上限で動く。引き上げは対象外（fail-closed）。環境変数で差し替える経路は作らない。
//! - 「ステップ上限」は持たない。`regex` クレートが最悪 `O(m * n)` の終了を保証し、
//!   コンパイル後サイズ上限がパターン側の `m` を、入力長上限が `n` を有界にするため。

/// パターン長（生の PG パターンのバイト長）のハード上限（既定値）。
/// `declarative_filter::MAX_LIKE_PATTERN_LEN` と同じ値に揃える。
pub const MAX_REGEX_PATTERN_LEN: usize = 4096;

/// 入力長（バイト長）のハード上限（既定値）。1 値の最大ペイロード長
/// （`row_codec::MAX_SCALAR_PAYLOAD_LEN`）と同値で、これより長い入力は表層に存在しない。
pub const MAX_REGEX_INPUT_LEN: usize = 4 * 1024 * 1024;

/// コンパイル後サイズ（`regex` の `size_limit`）のハード上限（既定値）。`regex` 自身の既定
/// （10 MiB）と同値。超過は `CompiledTooBig` として拒否される。
pub const MAX_REGEX_COMPILED_SIZE: usize = 10 * 1024 * 1024;

/// 入れ子の深さ（括弧）のハード上限。固定値（CLI では変えない）。字句走査は再帰しない
/// ため、この値はスタックではなく `regex` 側の AST 深さの上限として働く。
pub const MAX_REGEX_NEST_DEPTH: usize = 64;

/// 遅延 DFA のキャッシュ上限（`regex` の `dfa_size_limit`）。固定値（CLI では変えない）。
/// 超過してもエラーにはならず低速なエンジンへ縮退する。メモリの上限であって拒否の根拠ではない。
pub const REGEX_DFA_CACHE_BYTES: usize = 2 * 1024 * 1024;

/// 3 上限に設定できる最小値。
pub const MIN_REGEX_LIMIT: u64 = 1;

/// 正規表現マッチャ 3 上限の設定値。フィールドは非公開で、[`RegexLimits::new`] は
/// ハード上限で飽和させるため範囲外の値を保持しない。`Default` はハード上限と同値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegexLimits {
    max_pattern_len: usize,
    max_input_len: usize,
    max_compiled_size: usize,
}

impl Default for RegexLimits {
    fn default() -> Self {
        Self {
            max_pattern_len: MAX_REGEX_PATTERN_LEN,
            max_input_len: MAX_REGEX_INPUT_LEN,
            max_compiled_size: MAX_REGEX_COMPILED_SIZE,
        }
    }
}

impl RegexLimits {
    /// 検証済みの 3 値から組み立てる。直接渡された場合もハード上限で飽和させる（fail-closed）。
    pub fn new(max_pattern_len: usize, max_input_len: usize, max_compiled_size: usize) -> Self {
        Self {
            max_pattern_len: max_pattern_len.clamp(1, MAX_REGEX_PATTERN_LEN),
            max_input_len: max_input_len.clamp(1, MAX_REGEX_INPUT_LEN),
            max_compiled_size: max_compiled_size.clamp(1, MAX_REGEX_COMPILED_SIZE),
        }
    }

    /// パターン長（バイト）の上限。
    pub fn max_pattern_len(&self) -> usize {
        self.max_pattern_len
    }

    /// 入力長（バイト）の上限。
    pub fn max_input_len(&self) -> usize {
        self.max_input_len
    }

    /// コンパイル後サイズ（`regex` の `size_limit`）の上限。
    pub fn max_compiled_size(&self) -> usize {
        self.max_compiled_size
    }
}

/// `validate_*` の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct RegexLimitOutOfRange {
    value: u64,
    max: u64,
}

impl std::fmt::Display for RegexLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "value {} is out of range 1..={}", self.value, self.max)
    }
}

impl std::error::Error for RegexLimitOutOfRange {}

fn validate_in(value: u64, max: usize) -> Result<usize, RegexLimitOutOfRange> {
    let max64 = u64::try_from(max).unwrap_or(u64::MAX);
    if (MIN_REGEX_LIMIT..=max64).contains(&value) {
        // max は usize 由来のため、範囲内の値は usize へ変換できる。失敗時も範囲外として扱う。
        usize::try_from(value).map_err(|_| RegexLimitOutOfRange { value, max: max64 })
    } else {
        Err(RegexLimitOutOfRange { value, max: max64 })
    }
}

/// パターン長上限の設定値を `1..=MAX_REGEX_PATTERN_LEN` で検証する単一情報源。
pub fn validate_max_pattern_len(value: u64) -> Result<usize, RegexLimitOutOfRange> {
    validate_in(value, MAX_REGEX_PATTERN_LEN)
}

/// 入力長上限の設定値を `1..=MAX_REGEX_INPUT_LEN` で検証する単一情報源。
pub fn validate_max_input_len(value: u64) -> Result<usize, RegexLimitOutOfRange> {
    validate_in(value, MAX_REGEX_INPUT_LEN)
}

/// コンパイル後サイズ上限の設定値を `1..=MAX_REGEX_COMPILED_SIZE` で検証する単一情報源。
pub fn validate_max_compiled_size(value: u64) -> Result<usize, RegexLimitOutOfRange> {
    validate_in(value, MAX_REGEX_COMPILED_SIZE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limits() {
        let d = RegexLimits::default();
        assert_eq!(d.max_pattern_len(), MAX_REGEX_PATTERN_LEN);
        assert_eq!(d.max_input_len(), MAX_REGEX_INPUT_LEN);
        assert_eq!(d.max_compiled_size(), MAX_REGEX_COMPILED_SIZE);
    }

    #[test]
    fn validators_accept_boundaries_and_reject_outside() {
        assert_eq!(validate_max_pattern_len(1), Ok(1));
        assert_eq!(
            validate_max_pattern_len(MAX_REGEX_PATTERN_LEN as u64),
            Ok(MAX_REGEX_PATTERN_LEN)
        );
        assert!(validate_max_pattern_len(0).is_err());
        assert!(validate_max_pattern_len(MAX_REGEX_PATTERN_LEN as u64 + 1).is_err());
        assert_eq!(
            validate_max_input_len(MAX_REGEX_INPUT_LEN as u64),
            Ok(MAX_REGEX_INPUT_LEN)
        );
        assert!(validate_max_input_len(0).is_err());
        assert!(validate_max_input_len(MAX_REGEX_INPUT_LEN as u64 + 1).is_err());
        assert_eq!(
            validate_max_compiled_size(MAX_REGEX_COMPILED_SIZE as u64),
            Ok(MAX_REGEX_COMPILED_SIZE)
        );
        assert!(validate_max_compiled_size(0).is_err());
        assert!(validate_max_compiled_size(u64::MAX).is_err());
    }

    #[test]
    fn error_display_has_only_value_and_range() {
        let e = validate_max_pattern_len(0).unwrap_err();
        assert_eq!(e.to_string(), "value 0 is out of range 1..=4096");
    }

    #[test]
    fn new_saturates_to_hard_limits() {
        assert_eq!(
            RegexLimits::new(usize::MAX, usize::MAX, usize::MAX),
            RegexLimits::default()
        );
        let l = RegexLimits::new(0, 0, 0);
        assert_eq!(
            (
                l.max_pattern_len(),
                l.max_input_len(),
                l.max_compiled_size()
            ),
            (1, 1, 1)
        );
    }
}
