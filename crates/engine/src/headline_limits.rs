//! `ts_headline` の出力バイト長・断片数上限の起動時設定（Issue #2125。ポインタ:
//! `docs/spec/04-behavior/search.md` SEARCH-14）。
//!
//! `wire-server` の `headline_limits_opt` が CLI フラグ `--max-ts-headline-bytes`／
//! `--max-ts-headline-fragments` を厳密パースし、本モジュールの `validate_*` で範囲検証したうえで
//! [`HeadlineLimits`] を組み立て、`EngineCore::with_headline_limits` へ 1 回だけ注入する
//! （`sparsevec_limits` と同じ流儀）。SQL の束縛区間（`sql::udf_call::bind_call`）は
//! 文実行入口に張られた [`HeadlineLimitsScope`] から [`current_headline_limits`] で値を読み、
//! `BuiltinFn::TsHeadline2`／`TsHeadline3` へ焼き込む。範囲検証の単一情報源は本モジュール。
//!
//! 設計上の不変条件（`docs/design/ts-headline-limits-cli.md`）:
//!
//! - **ハード上限が既定値**。設定できるのは「下げる方向」だけで、引き上げは対象外（fail-closed）。
//!   環境変数で差し替える経路は作らない。
//! - 上限超過は切り詰めず `54000` で拒否する（`sparse::headline` が判定し、SQL 層が写像する）。

use std::cell::Cell;

/// 出力バイト長のハード上限（= 既定値）。行の `TEXT` 列値 1 個分の上限と同値。
pub const HARD_MAX_HEADLINE_OUTPUT_BYTES: usize = crate::row_codec::MAX_TEXT_FIELD_LEN as usize;

/// 断片数のハード上限（= 既定値）。
pub const HARD_MAX_HEADLINE_FRAGMENTS: usize = 1024;

/// 設定できる最小値（どちらのフラグも 1 以上）。
pub const MIN_HEADLINE_LIMIT: u64 = 1;

/// `ts_headline` の資源上限。フィールドは非公開で、[`HeadlineLimits::new`] はハード上限で
/// 飽和させるため範囲外の値を保持しない。`Default` はハード上限と同値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeadlineLimits {
    max_output_bytes: usize,
    max_fragments: usize,
}

impl Default for HeadlineLimits {
    fn default() -> Self {
        Self {
            max_output_bytes: HARD_MAX_HEADLINE_OUTPUT_BYTES,
            max_fragments: HARD_MAX_HEADLINE_FRAGMENTS,
        }
    }
}

impl HeadlineLimits {
    /// 検証済みの値から組み立てる。直接渡された場合もハード上限で飽和させる（fail-closed）。
    pub fn new(max_output_bytes: usize, max_fragments: usize) -> Self {
        Self {
            max_output_bytes: max_output_bytes.clamp(1, HARD_MAX_HEADLINE_OUTPUT_BYTES),
            max_fragments: max_fragments.clamp(1, HARD_MAX_HEADLINE_FRAGMENTS),
        }
    }

    /// 出力のバイト長の上限。
    pub fn max_output_bytes(&self) -> usize {
        self.max_output_bytes
    }

    /// `MaxFragments` オプションに許す最大値。
    pub fn max_fragments(&self) -> usize {
        self.max_fragments
    }
}

/// `validate_*` の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct HeadlineLimitOutOfRange {
    value: u64,
    max: u64,
}

impl std::fmt::Display for HeadlineLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "value {} is out of range 1..={}", self.value, self.max)
    }
}

impl std::error::Error for HeadlineLimitOutOfRange {}

fn validate(value: u64, hard: usize) -> Result<usize, HeadlineLimitOutOfRange> {
    let max = u64::try_from(hard).unwrap_or(u64::MAX);
    if (MIN_HEADLINE_LIMIT..=max).contains(&value) {
        usize::try_from(value).map_err(|_| HeadlineLimitOutOfRange { value, max })
    } else {
        Err(HeadlineLimitOutOfRange { value, max })
    }
}

/// 出力バイト長上限の設定値を `1..=HARD_MAX_HEADLINE_OUTPUT_BYTES` で検証する単一情報源。
pub fn validate_max_output_bytes(value: u64) -> Result<usize, HeadlineLimitOutOfRange> {
    validate(value, HARD_MAX_HEADLINE_OUTPUT_BYTES)
}

/// 断片数上限の設定値を `1..=HARD_MAX_HEADLINE_FRAGMENTS` で検証する単一情報源。
pub fn validate_max_fragments(value: u64) -> Result<usize, HeadlineLimitOutOfRange> {
    validate(value, HARD_MAX_HEADLINE_FRAGMENTS)
}

thread_local! {
    /// 現在の束縛スコープの上限。未設定は `None`（ハード上限へフォールバック）。
    static CURRENT_LIMITS: Cell<Option<HeadlineLimits>> = const { Cell::new(None) };
}

/// `EngineCore::headline_limits` を現スレッドの束縛スコープへ設定する RAII ガード。
/// `core.rs` の `sparsevec_limits_scope` と同じ区間で張られ、drop で直前の値へ戻す
/// （入れ子・panic でも他文脈へ漏れない）。プロセス大域の状態ではない。
pub(crate) struct HeadlineLimitsScope {
    prev: Option<HeadlineLimits>,
}

impl HeadlineLimitsScope {
    pub(crate) fn enter(limits: HeadlineLimits) -> Self {
        let prev = CURRENT_LIMITS.with(|c| c.replace(Some(limits)));
        HeadlineLimitsScope { prev }
    }
}

impl Drop for HeadlineLimitsScope {
    fn drop(&mut self) {
        CURRENT_LIMITS.with(|c| c.set(self.prev));
    }
}

/// 現在の束縛スコープの上限。スコープが無い経路はハード上限（CLI は下げることしかできない）。
pub(crate) fn current_headline_limits() -> HeadlineLimits {
    CURRENT_LIMITS.with(|c| c.get()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limits() {
        let d = HeadlineLimits::default();
        assert_eq!(d.max_output_bytes(), HARD_MAX_HEADLINE_OUTPUT_BYTES);
        assert_eq!(d.max_fragments(), HARD_MAX_HEADLINE_FRAGMENTS);
    }

    #[test]
    fn validators_accept_boundaries_and_reject_outside() {
        assert_eq!(validate_max_fragments(1), Ok(1));
        assert_eq!(validate_max_fragments(1024), Ok(1024));
        assert!(validate_max_fragments(0).is_err());
        assert!(validate_max_fragments(1025).is_err());
        assert_eq!(
            validate_max_output_bytes(4 * 1024 * 1024),
            Ok(4 * 1024 * 1024)
        );
        assert!(validate_max_output_bytes(0).is_err());
        assert!(validate_max_output_bytes(u64::MAX).is_err());
        assert_eq!(
            validate_max_fragments(0).unwrap_err().to_string(),
            "value 0 is out of range 1..=1024"
        );
    }

    #[test]
    fn new_saturates_to_hard_limits() {
        assert_eq!(
            HeadlineLimits::new(usize::MAX, usize::MAX),
            HeadlineLimits::default()
        );
        assert_eq!(HeadlineLimits::new(0, 0).max_output_bytes(), 1);
    }

    #[test]
    fn scope_nests_and_restores() {
        let default = current_headline_limits();
        assert_eq!(default, HeadlineLimits::default());
        {
            let _outer = HeadlineLimitsScope::enter(HeadlineLimits::new(50, 5));
            assert_eq!(current_headline_limits().max_output_bytes(), 50);
            {
                let _inner = HeadlineLimitsScope::enter(HeadlineLimits::new(20, 2));
                assert_eq!(current_headline_limits().max_fragments(), 2);
            }
            assert_eq!(current_headline_limits().max_fragments(), 5);
        }
        assert_eq!(current_headline_limits(), default);
    }
}
