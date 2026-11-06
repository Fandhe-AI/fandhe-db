//! 全文検索クエリ評価（[`crate::sparse`] の `score_text_query_within`）の「項数」
//! 「前方一致の展開数」上限の起動時設定（Issue #2115。ポインタ: `docs/spec/04-behavior/`
//! SEARCH-12、RLS-8）。
//!
//! `wire-server` の `text_query_limits_opt` が CLI フラグ（`--max-text-query-terms`・
//! `--max-text-query-prefix-expansions`）を厳密パースし、本モジュールの `validate_*` で範囲検証
//! したうえで [`TextQueryLimits`] を組み立て、`EngineCore::with_text_query_limits` へ 1 回だけ
//! 注入する（`regex_limits` と同じ流儀）。SQL / NoSQL 表層からの読み出し配線は後続 Issue
//! （#2116）で、本 Issue は上限型と評価 API までを担う。範囲検証の単一情報源は本モジュール。
//!
//! 設計上の不変条件（`docs/design/text-query-limits-cli.md`）:
//!
//! - **ハード上限が既定値**。設定できるのは「下げる方向」だけで、引き上げは対象外（fail-closed）。
//!   環境変数で差し替える経路は作らない。
//! - フレーズの必須トークン数上限（`MAX_PHRASE_REQUIRED_TOKENS`）は別軸の固定定数で CLI 化しない。

/// 1 クエリあたりの項数（グループごとの一意な語の総和）のハード上限（既定値）。
/// `sparse::MAX_QUERY_TERMS` と同値で、演算子を含まない入力は既定値で拒否されない。
pub const MAX_TEXT_QUERY_TERMS: usize = 1024;

/// 前方一致の展開語数（可視集合で df > 0 の語の総和）のハード上限（既定値）。
pub const MAX_TEXT_QUERY_PREFIX_EXPANSIONS: usize = 1024;

/// 2 上限に設定できる最小値。
pub const MIN_TEXT_QUERY_LIMIT: u64 = 1;

/// 全文検索クエリ評価 2 上限の設定値。フィールドは非公開で、[`TextQueryLimits::new`] は
/// ハード上限で飽和させるため範囲外の値を保持しない。`Default` はハード上限と同値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextQueryLimits {
    max_terms: usize,
    max_prefix_expansions: usize,
}

impl Default for TextQueryLimits {
    fn default() -> Self {
        Self {
            max_terms: MAX_TEXT_QUERY_TERMS,
            max_prefix_expansions: MAX_TEXT_QUERY_PREFIX_EXPANSIONS,
        }
    }
}

impl TextQueryLimits {
    /// 検証済みの 2 値から組み立てる。直接渡された場合もハード上限で飽和させる（fail-closed）。
    pub fn new(max_terms: usize, max_prefix_expansions: usize) -> Self {
        Self {
            max_terms: max_terms.clamp(1, MAX_TEXT_QUERY_TERMS),
            max_prefix_expansions: max_prefix_expansions.clamp(1, MAX_TEXT_QUERY_PREFIX_EXPANSIONS),
        }
    }

    /// 項数の上限。
    pub fn max_terms(&self) -> usize {
        self.max_terms
    }

    /// 前方一致の展開語数の上限。
    pub fn max_prefix_expansions(&self) -> usize {
        self.max_prefix_expansions
    }
}

/// `validate_*` の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct TextQueryLimitOutOfRange {
    value: u64,
    max: u64,
}

impl std::fmt::Display for TextQueryLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "value {} is out of range 1..={}", self.value, self.max)
    }
}

impl std::error::Error for TextQueryLimitOutOfRange {}

fn validate_in(value: u64, max: usize) -> Result<usize, TextQueryLimitOutOfRange> {
    let max64 = u64::try_from(max).unwrap_or(u64::MAX);
    if (MIN_TEXT_QUERY_LIMIT..=max64).contains(&value) {
        usize::try_from(value).map_err(|_| TextQueryLimitOutOfRange { value, max: max64 })
    } else {
        Err(TextQueryLimitOutOfRange { value, max: max64 })
    }
}

/// 項数上限の設定値を `1..=MAX_TEXT_QUERY_TERMS` で検証する単一情報源。
pub fn validate_max_terms(value: u64) -> Result<usize, TextQueryLimitOutOfRange> {
    validate_in(value, MAX_TEXT_QUERY_TERMS)
}

/// 展開数上限の設定値を `1..=MAX_TEXT_QUERY_PREFIX_EXPANSIONS` で検証する単一情報源。
pub fn validate_max_prefix_expansions(value: u64) -> Result<usize, TextQueryLimitOutOfRange> {
    validate_in(value, MAX_TEXT_QUERY_PREFIX_EXPANSIONS)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limits() {
        let d = TextQueryLimits::default();
        assert_eq!(d.max_terms(), MAX_TEXT_QUERY_TERMS);
        assert_eq!(d.max_prefix_expansions(), MAX_TEXT_QUERY_PREFIX_EXPANSIONS);
    }

    #[test]
    fn validators_accept_boundaries_and_reject_outside() {
        assert_eq!(validate_max_terms(1), Ok(1));
        assert_eq!(
            validate_max_terms(MAX_TEXT_QUERY_TERMS as u64),
            Ok(MAX_TEXT_QUERY_TERMS)
        );
        assert!(validate_max_terms(0).is_err());
        assert!(validate_max_terms(MAX_TEXT_QUERY_TERMS as u64 + 1).is_err());
        assert_eq!(validate_max_prefix_expansions(1), Ok(1));
        assert!(validate_max_prefix_expansions(0).is_err());
        assert!(validate_max_prefix_expansions(u64::MAX).is_err());
    }

    #[test]
    fn error_display_has_only_value_and_range() {
        let e = validate_max_terms(0).unwrap_err();
        assert_eq!(e.to_string(), "value 0 is out of range 1..=1024");
    }

    #[test]
    fn new_saturates_to_hard_limits() {
        assert_eq!(
            TextQueryLimits::new(usize::MAX, usize::MAX),
            TextQueryLimits::default()
        );
        let l = TextQueryLimits::new(0, 0);
        assert_eq!((l.max_terms(), l.max_prefix_expansions()), (1, 1));
    }
}
