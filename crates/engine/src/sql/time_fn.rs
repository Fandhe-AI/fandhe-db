//! 現在時刻関数（`now()`・`CURRENT_TIMESTAMP` 等）の評価コンテキストと時計読み取り。
//!
//! 役割: 読み取り文の束縛時に「確定済みの時刻」（[`StatementClock`]）を式束縛
//! （`sql::udf_call` の `BindEnv`）へ渡す唯一の経路と、壁時計の読み取り・精度丸め・
//! 日付変換の純関数を提供する（対象ビヘイビア: SQL-37・TASK-248・Issue #1586。
//! 設計記録は `docs/design/now-evaluated-value-ledger.md` の D3・D7・D8）。
//!
//! 呼び出し文脈: `core.rs` の読み取り文の実行入口（autocommit・明示／暗黙トランザクション・
//! Describe）が文の先頭で時刻を確定し、[`ClockScope`] を保持している間だけ束縛が時刻関数を
//! 受理する。スコープ外（INSERT／UPDATE／DELETE／COPY／CHECK／DEFAULT／VIEW 本文／NoSQL 等）は
//! 既定の `None` のまま `0A000` で拒否される（許可リスト型の fail-closed。書き込み側は
//! 台帳 v3〔#2188〕・INSERT 系ハッシュ〔#1587〕の実装まで受理しない）。
//!
//! 時刻の運搬は `SessionState`／`UdfRegistry` に載せない（`session_at_begin` の clone・
//! ROLLBACK 復元や `UdfRegistry` の等価比較の意味を崩さないため）。束縛は呼び出しスレッドで
//! 同期的に完結するため、スレッドローカルのスコープガードで受け渡す。

use std::cell::Cell;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::datetime::{validate_date_days, validate_timestamp_micros, MICROS_PER_DAY};
use crate::sql::allowlist::SqlSurfaceError;

/// 1 文の束縛に使う確定時刻（UTC マイクロ秒。1970-01-01 起点で `crate::datetime` と同じ表現）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StatementClock {
    /// トランザクション開始時刻（`now()`・`transaction_timestamp()`・`CURRENT_*`・
    /// `LOCALTIMESTAMP` の値。autocommit では文の開始時刻と同値）。
    pub(crate) txn_start_micros: i64,
    /// 文の開始時刻（`statement_timestamp()` の値）。
    pub(crate) stmt_start_micros: i64,
}

thread_local! {
    /// 現在の束縛スコープの時刻。`None` は時刻関数を受理しない文脈（既定。fail-closed）。
    static CURRENT: Cell<Option<StatementClock>> = const { Cell::new(None) };
}

/// [`StatementClock`] を現スレッドの束縛スコープへ設定する RAII ガード。drop で直前の値へ戻す
/// （入れ子・panic でも他文脈へ時刻が漏れない）。
pub(crate) struct ClockScope {
    prev: Option<StatementClock>,
}

impl ClockScope {
    pub(crate) fn enter(clock: StatementClock) -> Self {
        let prev = CURRENT.with(|c| c.replace(Some(clock)));
        ClockScope { prev }
    }
}

impl Drop for ClockScope {
    fn drop(&mut self) {
        let prev = self.prev;
        CURRENT.with(|c| c.set(prev));
    }
}

/// 現スレッドの束縛スコープの時刻（`sql::udf_call` の `BindEnv` 構築時に読む）。
pub(crate) fn current_clock() -> Option<StatementClock> {
    CURRENT.with(|c| c.get())
}

/// 壁時計を UTC マイクロ秒で読む。1970 年以前・範囲外は fail-closed でエラーにする
/// （値を黙って丸めない）。`clock_timestamp()` の評価（`udf_call::apply_builtin`）、
/// 文・トランザクション開始時刻の確定（`core.rs`・`sql::transaction`）が呼ぶ。
pub(crate) fn now_micros() -> Result<i64, SqlSurfaceError> {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_micros()).ok())
        .filter(|m| validate_timestamp_micros(*m))
        .ok_or_else(|| SqlSurfaceError::datetime_field_overflow("system clock is out of range"))?;
    Ok(micros)
}

/// マイクロ秒を小数秒 `precision` 桁（0〜6）へ丸める（四捨五入。PostgreSQL と同じ）。
/// 範囲外の `precision`・丸め後の範囲外は `None`。
pub(crate) fn round_to_precision(micros: i64, precision: u8) -> Option<i64> {
    if precision > 6 {
        return None;
    }
    let unit = 10_i64.checked_pow(u32::from(6 - precision))?;
    let half = unit / 2;
    let rounded = micros
        .checked_add(half)?
        .div_euclid(unit)
        .checked_mul(unit)?;
    validate_timestamp_micros(rounded).then_some(rounded)
}

/// UTC マイクロ秒を 1970-01-01 起点の日数へ変換する（`CURRENT_DATE`）。範囲外は `None`。
pub(crate) fn micros_to_days(micros: i64) -> Option<i32> {
    let days = i32::try_from(micros.div_euclid(MICROS_PER_DAY)).ok()?;
    validate_date_days(days).then_some(days)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_in_range() {
        let m = now_micros().expect("clock in range");
        assert!(validate_timestamp_micros(m));
    }

    #[test]
    fn rounding_boundaries() {
        assert_eq!(round_to_precision(1_234_567, 0), Some(1_000_000));
        assert_eq!(round_to_precision(1_500_000, 0), Some(2_000_000));
        assert_eq!(round_to_precision(1_234_567, 3), Some(1_235_000));
        assert_eq!(round_to_precision(1_234_567, 6), Some(1_234_567));
        assert_eq!(round_to_precision(1, 7), None);
    }

    #[test]
    fn negative_micros_use_euclid_days() {
        assert_eq!(micros_to_days(-1), Some(-1));
        assert_eq!(micros_to_days(0), Some(0));
        assert_eq!(micros_to_days(MICROS_PER_DAY), Some(1));
    }

    #[test]
    fn scope_is_restored_on_drop() {
        assert_eq!(current_clock(), None);
        let c = StatementClock {
            txn_start_micros: 1,
            stmt_start_micros: 2,
        };
        {
            let _g = ClockScope::enter(c);
            assert_eq!(current_clock(), Some(c));
        }
        assert_eq!(current_clock(), None);
    }
}
