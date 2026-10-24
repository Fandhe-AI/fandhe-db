//! 書き込み・実体化の入力 SELECT に適用する結果行数上限の算出（SQL-34・SQL-15・Issue #2212）。
//!
//! 責務境界: `INSERT ... SELECT`（`insert_select_exec`）・CTAS（`ctas_exec`）・MATERIALIZED VIEW の
//! CREATE／REFRESH（`matview_exec`）が、入力を実体化する前に「事後判定に使う `cap`」と「内側を評価する
//! `ResultLimits`」をここから受け取る。COPY のテーブル形は `core.rs::run_copy_to_scan` が設定値そのものを
//! 使い、本モジュールの注入しない形と同じ `cap` になる。上限の実体は `--max-result-rows`
//! （`EngineCore::result_limits`）で、行 SELECT と同じ値に揃える。書き込み側の `--max-insert-rows`・
//! INDEX-4 の上限は実体化の後に判定する。
//!
//! 2 方式:
//! - 注入しない形（[`EngineCore::write_input_cap`]）: LIMIT 省略を読み取り経路の暗黙上限（#1796）が
//!   受ける形。`cap` は設定値そのもの、評価には設定値をそのまま渡し、読み取り経路が `cap+1` 件で
//!   超過を検出して `54000` を返す。ハード上限でも fail-open しない。
//! - 注入する形（[`EngineCore::write_input_cap_injected`]）: 構文段が `LIMIT cap+1` のトークンを足して
//!   超過を検出する形。`ResultLimits::new` はハード上限で飽和するため、`cap+1` が `cap` に潰れて
//!   超過検出が効かなくなる fail-open を避けるべく、`cap` をハード上限の 1 つ手前へ丸める
//!   （設定がハード上限のときだけ 99,999 行まで）。

use super::EngineCore;
use crate::result_limits::{ResultLimits, MAX_RESULT_ROWS_HARD_CAP};
use crate::sql::allowlist::SqlSurfaceError;

/// 入力 SELECT の行数上限（事後判定の `cap` と評価に渡す `ResultLimits`）。
pub(super) struct WriteInputCap {
    /// 実体化した行数がこれを超えたら `54000`（書き込みの前に判定する）。
    pub(super) cap: usize,
    /// 内側 SELECT の評価に渡す上限。
    pub(super) eval_limits: ResultLimits,
    /// 注入する形で足す `LIMIT` の値（`cap+1`）。
    pub(super) fetch: usize,
}

fn internal() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "internal error".to_string(),
    }
}

fn native(limits: ResultLimits) -> Result<WriteInputCap, SqlSurfaceError> {
    let cap = limits.max_result_rows();
    let fetch = cap.checked_add(1).ok_or_else(internal)?;
    Ok(WriteInputCap {
        cap,
        eval_limits: limits,
        fetch,
    })
}

fn injected(limits: ResultLimits) -> Result<WriteInputCap, SqlSurfaceError> {
    let cap = limits
        .max_result_rows()
        .min(MAX_RESULT_ROWS_HARD_CAP.saturating_sub(1));
    let fetch = cap.checked_add(1).ok_or_else(internal)?;
    Ok(WriteInputCap {
        cap,
        eval_limits: ResultLimits::new(fetch),
        fetch,
    })
}

impl EngineCore {
    /// 注入しない形の上限。`cap` は `--max-result-rows` そのもの。
    pub(super) fn write_input_cap(&self) -> Result<WriteInputCap, SqlSurfaceError> {
        native(self.result_limits)
    }

    /// 注入する形の上限。`cap` はハード上限の 1 つ手前へ丸め、`fetch = cap+1` で評価する。
    pub(super) fn write_input_cap_injected(&self) -> Result<WriteInputCap, SqlSurfaceError> {
        injected(self.result_limits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_cap_is_the_setting_itself() {
        for set in [2usize, 10_000, MAX_RESULT_ROWS_HARD_CAP] {
            let c = native(ResultLimits::new(set)).expect("cap");
            assert_eq!(c.cap, set);
            assert_eq!(c.eval_limits.max_result_rows(), set);
            assert_eq!(c.fetch, set + 1);
        }
    }

    #[test]
    fn injected_cap_rounds_only_at_the_hard_cap() {
        let c = injected(ResultLimits::new(2)).expect("cap");
        assert_eq!((c.cap, c.fetch), (2, 3));
        assert_eq!(c.eval_limits.max_result_rows(), 3);
        let h = injected(ResultLimits::new(usize::MAX)).expect("cap");
        assert_eq!(h.cap, MAX_RESULT_ROWS_HARD_CAP - 1);
        assert_eq!(h.fetch, MAX_RESULT_ROWS_HARD_CAP);
        assert_eq!(h.eval_limits.max_result_rows(), MAX_RESULT_ROWS_HARD_CAP);
    }
}
