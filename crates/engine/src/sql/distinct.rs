//! `SELECT DISTINCT`・`COUNT(DISTINCT <expr>)`（SQL-25 (c)・TASK-209）が共有する
//! 正準キー化・予算管理。呼び出し元は 2 経路:
//!
//! - `sql::allowlist::validate_sql_tokens` が `SELECT DISTINCT` を
//!   `sql::group_by`（既存の `GROUP BY` 実行器）へ脱糖する経路（`GROUP BY` と
//!   同じ [`crate::sql::group_by::ResultBudget`] を継承するため、本モジュールの
//!   予算は対象外）。
//! - `sql::aggregate`／`sql::group_by` の `Accumulator::CountDistinct` が
//!   `COUNT(DISTINCT <expr>)` の中間状態（クエリ全体で 1 つ）として使う
//!   [`DistinctBudget`]。
//!
//! キーの数値正準化（`-0.0` を `+0.0` へ、NaN を単一表現へ正規化）は
//! [`crate::constraint::push_canonical_component`]（UNIQUE／FOREIGN KEY の
//! 正準キー）と役割は異なる。1 つの `CountDistinct` アキュムレータは束縛時に
//! 決まった単一の入力型（`AggregateInput`）しか観測しないため、
//! `push_canonical_component` のような「複数コンポーネントを連結する際の境界
//! 曖昧性」は生じず（`sql::aggregate::Accumulator::observe_distinct` は型タグ・
//! 長さ接頭辞を付けない生の正準バイト列をそのままキーにする）、本モジュールは
//! その正準化プリミティブ（`canon_f64`）と予算管理（[`DistinctBudget`]）のみを
//! 提供する。

use crate::sql::allowlist::SqlSurfaceError;

/// `COUNT(DISTINCT)` の中間状態がクエリ全体で保持してよい異なり値の件数上限
/// （実装既定値）。`SELECT DISTINCT` の結果行数そのものは、既存の `GROUP BY`
/// 予算（`sql::group_by::MAX_GROUPS`）をそのまま継承するため対象外
/// （出力行数と中間状態のエントリ数は別の量であるため上限値も異なる）。
pub(crate) const MAX_DISTINCT_VALUES: usize = 100_000;

/// `COUNT(DISTINCT)` の中間状態が保持するキーの累計バイト数上限
/// （エントリごとの固定オーバーヘッド見積り込み）。`TEXT` 列は 1 値あたり
/// 最大 [`crate::row_codec::MAX_TEXT_FIELD_LEN`] に達しうるため、件数上限
/// だけでは有界にならない。
pub(crate) const MAX_DISTINCT_TOTAL_BYTES: usize = 16 * 1024 * 1024;

/// 1 エントリあたりの固定オーバーヘッド見積り（`HashSet<Vec<u8>>` のバケット・
/// `Vec` 自体のヒープ管理コストの概算）。
const DISTINCT_ENTRY_OVERHEAD_BYTES: usize = 32;

/// [`DistinctBudget::charge`] の件数超過 detail 文言。`sql::aggregate::
/// is_aggregate_text_budget_error`／`sql::group_by::is_text_accumulator_budget_error`
/// が照合する文言とは重ならない別文字列にする（processing 順序に依存しない
/// 単調増加の集合であり、索引経路と全走査で成否が変わってはならないため。
/// 全走査へのフォールバック対象にしない）。
const DISTINCT_CARDINALITY_EXCEEDED_DETAIL: &str =
    "COUNT(DISTINCT) cardinality exceeds the allowed limit";
/// 同上。累計バイト数超過。
const DISTINCT_BYTES_EXCEEDED_DETAIL: &str =
    "COUNT(DISTINCT) accumulated key size exceeds the allowed limit";
/// 同上。`checked_add` のオーバーフロー側。
const DISTINCT_BUDGET_OVERFLOW_DETAIL: &str = "COUNT(DISTINCT) budget accounting overflowed";

/// `COUNT(DISTINCT <expr>)` の中間状態（クエリ全体で 1 つ。`GROUP BY` を伴う
/// 場合は全グループ・全項目の合計を管理する）。新規キー 1 件の挿入が確定した
/// 時点でのみ [`Self::charge`] を呼ぶ（既存キーへの再ヒットでは呼ばない）。
pub(crate) struct DistinctBudget {
    entries: usize,
    bytes: usize,
}

impl DistinctBudget {
    pub(crate) fn new() -> Self {
        Self {
            entries: 0,
            bytes: 0,
        }
    }

    /// 新規キー 1 件（バイト長 `key_len`）を計上し、上限超過なら `54000` で
    /// fail-closed に拒否する。`checked_*` を使い、加算のオーバーフローも
    /// `54000` として扱う（`.claude/rules/coding-rust.md`「整数演算は
    /// `checked_*`／`saturating_*` を使う」）。
    pub(crate) fn charge(&mut self, key_len: usize) -> Result<(), SqlSurfaceError> {
        let entries = self
            .entries
            .checked_add(1)
            .ok_or_else(|| SqlSurfaceError::payload_too_large(DISTINCT_BUDGET_OVERFLOW_DETAIL))?;
        let added = key_len
            .checked_add(DISTINCT_ENTRY_OVERHEAD_BYTES)
            .ok_or_else(|| SqlSurfaceError::payload_too_large(DISTINCT_BUDGET_OVERFLOW_DETAIL))?;
        let bytes = self
            .bytes
            .checked_add(added)
            .ok_or_else(|| SqlSurfaceError::payload_too_large(DISTINCT_BUDGET_OVERFLOW_DETAIL))?;
        if entries > MAX_DISTINCT_VALUES {
            return Err(SqlSurfaceError::payload_too_large(
                DISTINCT_CARDINALITY_EXCEEDED_DETAIL,
            ));
        }
        if bytes > MAX_DISTINCT_TOTAL_BYTES {
            return Err(SqlSurfaceError::payload_too_large(
                DISTINCT_BYTES_EXCEEDED_DETAIL,
            ));
        }
        self.entries = entries;
        self.bytes = bytes;
        Ok(())
    }
}

/// `f64` の正準化。`-0.0` を `+0.0` へ、NaN をすべて単一の正準表現へ正規化する
/// （PostgreSQL の `DISTINCT`／`GROUP BY` は NaN 同士・±0 同士をそれぞれ同一視
/// する契約に合わせる）。
pub(crate) fn canon_f64(v: f64) -> [u8; 8] {
    let canon = if v.is_nan() {
        f64::NAN
    } else if v == 0.0 {
        0.0_f64
    } else {
        v
    };
    canon.to_bits().to_be_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canon_f64_normalizes_negative_zero() {
        assert_eq!(canon_f64(0.0), canon_f64(-0.0));
    }

    #[test]
    fn canon_f64_normalizes_nan_payloads() {
        let a = f64::from_bits(0x7ff8000000000001);
        let b = f64::from_bits(0x7ff8000000000002);
        assert!(a.is_nan() && b.is_nan());
        assert_eq!(canon_f64(a), canon_f64(b));
    }

    #[test]
    fn canon_f64_keeps_distinct_finite_values_distinct() {
        assert_ne!(canon_f64(1.0), canon_f64(2.0));
    }

    #[test]
    fn distinct_budget_rejects_cardinality_overflow() {
        let mut budget = DistinctBudget::new();
        for _ in 0..MAX_DISTINCT_VALUES {
            budget.charge(4).expect("within limit");
        }
        let err = budget.charge(4).expect_err("must exceed cardinality limit");
        assert_eq!(err.wire_code(), "54000");
    }

    #[test]
    fn distinct_budget_rejects_byte_overflow() {
        let mut budget = DistinctBudget::new();
        let big = MAX_DISTINCT_TOTAL_BYTES;
        let err = budget.charge(big).expect_err("must exceed byte limit");
        assert_eq!(err.wire_code(), "54000");
    }
}
