//! `SPARSEVEC(N)` の非ゼロ要素数上限の起動時設定（Issue #2027。ポインタ:
//! `docs/spec/04-behavior/` TABLE-47 (2)・SQL-46）。
//!
//! `wire-server` の `sparsevec_limits_opt` が CLI フラグ `--max-sparsevec-nnz` を厳密パースし、
//! 本モジュールの [`validate_max_nnz`] で範囲検証したうえで [`SparseVecLimits`] を組み立て、
//! `EngineCore::with_sparsevec_limits` へ 1 回だけ注入する（`regex_limits` と同じ流儀）。
//! 書き込み経路（`sql::parser::parse_sparsevec_literal`）は文の束縛区間に張られた
//! [`SparseVecLimitsScope`] から [`current_sparsevec_limits`] で値を読む。範囲検証の単一情報源は本モジュール。
//!
//! 設計上の不変条件（`docs/design/sparsevec-limits-cli.md`）:
//!
//! - **ハード上限が既定値**。設定できるのは「下げる方向」だけで、フラグ未指定ならハード上限で
//!   動く。引き上げは対象外（fail-closed）。環境変数で差し替える経路は作らない。
//! - 検査はテキスト入力から値を作る時点だけ。行 codec の decode／encode は
//!   [`HARD_MAX_SPARSEVEC_NNZ`] のままなので、上限を下げても既存行は読め、再 encode も拒否されない。

use std::cell::Cell;

use crate::sparse_vector::HARD_MAX_SPARSEVEC_NNZ;

/// 設定できる最小値。
pub const MIN_SPARSEVEC_NNZ: u64 = 1;

/// 非ゼロ数上限の設定値。フィールドは非公開で、[`SparseVecLimits::new`] はハード上限で
/// 飽和させるため範囲外の値を保持しない。`Default` はハード上限と同値。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SparseVecLimits {
    max_nnz: usize,
}

impl Default for SparseVecLimits {
    fn default() -> Self {
        Self {
            max_nnz: HARD_MAX_SPARSEVEC_NNZ,
        }
    }
}

impl SparseVecLimits {
    /// 検証済みの値から組み立てる。直接渡された場合もハード上限で飽和させる（fail-closed）。
    pub fn new(max_nnz: usize) -> Self {
        Self {
            max_nnz: max_nnz.clamp(1, HARD_MAX_SPARSEVEC_NNZ),
        }
    }

    /// 書き込み経路の非ゼロ要素数の上限。
    pub fn max_nnz(&self) -> usize {
        self.max_nnz
    }
}

/// [`validate_max_nnz`] の範囲外エラー。`Display` は値と範囲のみを含む。
#[derive(Debug, PartialEq, Eq)]
pub struct SparseVecLimitOutOfRange {
    value: u64,
    max: u64,
}

impl std::fmt::Display for SparseVecLimitOutOfRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "value {} is out of range 1..={}", self.value, self.max)
    }
}

impl std::error::Error for SparseVecLimitOutOfRange {}

/// 非ゼロ数上限の設定値を `1..=HARD_MAX_SPARSEVEC_NNZ` で検証する単一情報源。
pub fn validate_max_nnz(value: u64) -> Result<usize, SparseVecLimitOutOfRange> {
    let max = u64::try_from(HARD_MAX_SPARSEVEC_NNZ).unwrap_or(u64::MAX);
    if (MIN_SPARSEVEC_NNZ..=max).contains(&value) {
        usize::try_from(value).map_err(|_| SparseVecLimitOutOfRange { value, max })
    } else {
        Err(SparseVecLimitOutOfRange { value, max })
    }
}

thread_local! {
    /// 現在の束縛スコープの上限。未設定は `None`（ハード上限へフォールバック）。
    static CURRENT_LIMITS: Cell<Option<SparseVecLimits>> = const { Cell::new(None) };
}

/// `EngineCore::sparsevec_limits` を現スレッドの束縛スコープへ設定する RAII ガード。
/// `core.rs` の文実行入口・束縛クロージャ呼び出し区間が保持している間だけ有効で、drop で
/// 直前の値へ戻す（入れ子・panic でも他文脈へ漏れない）。プロセス大域の状態ではない。
pub(crate) struct SparseVecLimitsScope {
    prev: Option<SparseVecLimits>,
}

impl SparseVecLimitsScope {
    pub(crate) fn enter(limits: SparseVecLimits) -> Self {
        let prev = CURRENT_LIMITS.with(|c| c.replace(Some(limits)));
        SparseVecLimitsScope { prev }
    }
}

impl Drop for SparseVecLimitsScope {
    fn drop(&mut self) {
        CURRENT_LIMITS.with(|c| c.set(self.prev));
    }
}

/// 現在の束縛スコープの上限。スコープが無い経路はハード上限（CLI は下げることしかできない）。
pub(crate) fn current_sparsevec_limits() -> SparseVecLimits {
    CURRENT_LIMITS.with(|c| c.get()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_equals_hard_limit() {
        assert_eq!(SparseVecLimits::default().max_nnz(), HARD_MAX_SPARSEVEC_NNZ);
    }

    #[test]
    fn validator_accepts_boundaries_and_rejects_outside() {
        assert_eq!(validate_max_nnz(1), Ok(1));
        assert_eq!(validate_max_nnz(16_000), Ok(16_000));
        assert!(validate_max_nnz(0).is_err());
        assert!(validate_max_nnz(16_001).is_err());
        assert!(validate_max_nnz(u64::MAX).is_err());
        assert_eq!(
            validate_max_nnz(0).unwrap_err().to_string(),
            "value 0 is out of range 1..=16000"
        );
    }

    #[test]
    fn new_saturates_to_hard_limit() {
        assert_eq!(SparseVecLimits::new(usize::MAX), SparseVecLimits::default());
        assert_eq!(SparseVecLimits::new(0).max_nnz(), 1);
    }

    #[test]
    fn scope_nests_and_restores() {
        let default = current_sparsevec_limits();
        assert_eq!(default, SparseVecLimits::default());
        {
            let _outer = SparseVecLimitsScope::enter(SparseVecLimits::new(5));
            assert_eq!(current_sparsevec_limits().max_nnz(), 5);
            {
                let _inner = SparseVecLimitsScope::enter(SparseVecLimits::new(2));
                assert_eq!(current_sparsevec_limits().max_nnz(), 2);
            }
            assert_eq!(current_sparsevec_limits().max_nnz(), 5);
        }
        assert_eq!(current_sparsevec_limits(), default);
    }
}
