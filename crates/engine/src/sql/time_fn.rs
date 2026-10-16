//! 現在時刻関数（`now()`・`CURRENT_TIMESTAMP` 等）の評価コンテキストと時計読み取り。
//!
//! 役割: 読み取り文の束縛時に「確定済みの時刻」（[`StatementClock`]）を式束縛
//! （`sql::udf_call` の `BindEnv`）へ渡す唯一の経路と、壁時計の読み取り・精度丸め・
//! 日付変換の純関数を提供する（対象ビヘイビア: SQL-37・TASK-248・Issue #1586。
//! 設計記録は `docs/design/now-evaluated-value-ledger.md` の D3・D7・D8）。
//!
//! 呼び出し文脈: `core.rs` の読み取り文・書き込み文（INSERT／UPDATE／DELETE の原子版。Issue #2188）の
//! 実行入口（autocommit・明示／暗黙トランザクション・Describe）が文の先頭で時刻を確定し、
//! [`ClockScope`] を保持している間だけ束縛が時刻関数を受理する。スコープ外（COPY／CHECK／
//! VIEW 本文／NoSQL 等）は既定の `None` のまま `0A000` で拒否される（許可リスト型の
//! fail-closed）。ほかに、列 `DEFAULT now()` 系の評価（[`DefaultClockScope`]。INSERT の省略列補完と
//! `VALUES` の単独時刻関数の評価。Issue #1587・#2188）と、分割実行 DML の SET・WHERE
//! （[`ClockPolicy::TxnFixedOnly`]。ジョブ開始時に確定した固定時刻。設計 D6）がある。
//! 書き込み文で時刻関数が評価されると [`ledger_evaluated_at`] が `Some` を返し、
//! `recovery::ledger` が台帳 v3 にトランザクション開始時刻を記録する。
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

/// 式束縛スコープが受理する時刻関数の範囲（SQL-37・Issue #1587）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClockPolicy {
    /// すべての時刻関数を受理する（読み取り文。既定）。
    All,
    /// トランザクション開始時刻で固定される関数（`now()`・`transaction_timestamp()`・
    /// `CURRENT_*`・`LOCALTIMESTAMP`）だけを受理する。分割実行 DML（チャンクごとに commit
    /// する非原子実行）が、ジョブ開始時に確定・永続化した時刻（設計 D6）で全チャンクを
    /// 束縛するために使う。`statement_timestamp()`・`clock_timestamp()` は実行ごとに値が
    /// 揺れて再開時に再現できないため `0A000` にする。
    TxnFixedOnly,
}

thread_local! {
    /// 現在の束縛スコープの時刻。`None` は時刻関数を受理しない文脈（既定。fail-closed）。
    static CURRENT: Cell<Option<StatementClock>> = const { Cell::new(None) };
    /// 現在の束縛スコープの受理範囲（[`ClockScope`] が設定。既定は [`ClockPolicy::All`]）。
    static POLICY: Cell<ClockPolicy> = const { Cell::new(ClockPolicy::All) };
    /// 列 `DEFAULT now()` 系の評価に使う時刻（INSERT の省略列補完専用）。式束縛用の
    /// [`CURRENT`] とは別スロットにし、`INSERT VALUES` の式が意図せず時刻関数を受理しない
    /// ようにする（INSERT VALUES 内の時刻関数は #2188 まで `42601`／`0A000` のまま）。
    static DEFAULT_CLOCK: Cell<Option<StatementClock>> = const { Cell::new(None) };
    /// 現在の [`ClockScope`] の確定時刻を束縛が実際に使ったか（`udf_call::require_clock` が立てる）。
    /// 分割実行 DML が「時刻値関数を含む文か」を AST の網羅走査なしに判定するために使う
    /// （束縛が時刻を要求した＝含む。判定漏れが fail-closed になる）。
    static CLOCK_USED: Cell<bool> = const { Cell::new(false) };
    /// 束縛と台帳記録が別スコープに分かれる経路（NoSQL insert・COPY）が、束縛時に確定した
    /// 台帳 v3 の評価時刻を書き込み区間へ引き継ぐ上書き値（[`LedgerClockScope`]）。
    /// `None` は上書きなし、`Some(x)` は `x`（`None` なら v2）を権威値として使う。
    static LEDGER_OVERRIDE: Cell<Option<Option<i64>>> = const { Cell::new(None) };
}

/// [`StatementClock`] を現スレッドの束縛スコープへ設定する RAII ガード。drop で直前の値へ戻す
/// （入れ子・panic でも他文脈へ時刻が漏れない）。
pub(crate) struct ClockScope {
    prev: Option<StatementClock>,
    prev_policy: ClockPolicy,
    prev_used: bool,
}

impl ClockScope {
    pub(crate) fn enter(clock: StatementClock) -> Self {
        let prev = CURRENT.with(|c| c.replace(Some(clock)));
        let prev_policy = POLICY.with(|p| p.replace(ClockPolicy::All));
        let prev_used = CLOCK_USED.with(|u| u.replace(false));
        ClockScope {
            prev,
            prev_policy,
            prev_used,
        }
    }

    /// トランザクション固定の関数だけを受理するスコープ（[`ClockPolicy::TxnFixedOnly`]）。
    pub(crate) fn enter_txn_fixed_only(clock: StatementClock) -> Self {
        let prev = CURRENT.with(|c| c.replace(Some(clock)));
        let prev_policy = POLICY.with(|p| p.replace(ClockPolicy::TxnFixedOnly));
        let prev_used = CLOCK_USED.with(|u| u.replace(false));
        ClockScope {
            prev,
            prev_policy,
            prev_used,
        }
    }

    /// このスコープの間に、束縛が確定時刻を実際に要求したか（時刻値関数を含む文か）。
    pub(crate) fn was_used(&self) -> bool {
        CLOCK_USED.with(|u| u.get())
    }
}

impl Drop for ClockScope {
    fn drop(&mut self) {
        let prev = self.prev;
        CURRENT.with(|c| c.set(prev));
        let prev_policy = self.prev_policy;
        POLICY.with(|p| p.set(prev_policy));
        let prev_used = self.prev_used;
        CLOCK_USED.with(|u| u.set(prev_used));
    }
}

/// 束縛が確定時刻を使ったことを記録する（`sql::udf_call` の時刻関数束縛が呼ぶ）。
pub(crate) fn mark_clock_used() {
    CLOCK_USED.with(|u| u.set(true));
}

/// 現スレッドの束縛スコープの受理範囲（`sql::udf_call` の `statement_timestamp()`・
/// `clock_timestamp()` 束縛が読む）。
pub(crate) fn current_clock_policy() -> ClockPolicy {
    POLICY.with(|p| p.get())
}

/// 列 `DEFAULT` の評価時刻を現スレッドへ設定する RAII ガード（INSERT の束縛区間専用。
/// `core.rs` の INSERT 入口・NoSQL insert の束縛クロージャが保持する。COPY は
/// 行ごとの束縛が複数の wire メッセージにまたがるため、`CopyInSession` が時刻を保持して
/// `fill_omitted_columns` へ明示的に渡す）。drop で直前の値へ戻す。
pub(crate) struct DefaultClockScope {
    prev: Option<StatementClock>,
    prev_used: bool,
}

impl DefaultClockScope {
    pub(crate) fn enter(clock: StatementClock) -> Self {
        let prev = DEFAULT_CLOCK.with(|c| c.replace(Some(clock)));
        // 式スコープと同じ「時刻関数を評価したか」の印を入れ子で保存・復元する。
        // 式スコープ（`ClockScope`）の内側で時刻を共有する場合はその印を引き継ぎ、スコープ外
        // （COPY の feed 等）の評価で残った印は文ごとに false へ初期化する（Issue #2188。
        // 時刻関数を使わない INSERT が v3 を記録しないようにする）。
        let in_expr_scope = CURRENT.with(|c| c.get()).is_some();
        let prev_used = CLOCK_USED.with(|u| {
            let prev = u.get();
            if !in_expr_scope {
                u.set(false);
            }
            prev
        });
        DefaultClockScope { prev, prev_used }
    }

    /// このスコープの間に時刻 DEFAULT（または式）が時刻を評価したか。
    pub(crate) fn was_used(&self) -> bool {
        CLOCK_USED.with(|u| u.get())
    }
}

/// 束縛時に確定した台帳 v3 の評価時刻を書き込み区間へ引き継ぐ RAII ガード（NoSQL insert・
/// COPY。束縛スコープは書き込み前に破棄されるため）。drop で直前の値へ戻す。
pub(crate) struct LedgerClockScope {
    prev: Option<Option<i64>>,
}

impl LedgerClockScope {
    pub(crate) fn enter(evaluated_at: Option<i64>) -> Self {
        let prev = LEDGER_OVERRIDE.with(|o| o.replace(Some(evaluated_at)));
        LedgerClockScope { prev }
    }
}

impl Drop for LedgerClockScope {
    fn drop(&mut self) {
        let prev = self.prev;
        LEDGER_OVERRIDE.with(|o| o.set(prev));
    }
}

impl Drop for DefaultClockScope {
    fn drop(&mut self) {
        let prev = self.prev;
        DEFAULT_CLOCK.with(|c| c.set(prev));
        let prev_used = self.prev_used;
        CLOCK_USED.with(|u| u.set(prev_used));
    }
}

/// 現スレッドの列 `DEFAULT` 評価時刻（`sql::parser::bind_insert_row` が読む）。
/// `None` は評価時刻が無い文脈で、時刻 DEFAULT は `0A000`（fail-closed）になる。
pub(crate) fn current_default_clock() -> Option<StatementClock> {
    DEFAULT_CLOCK.with(|c| c.get())
}

/// 時刻 DEFAULT を `clock`（トランザクション開始時刻）で値へ評価する（INSERT の省略列補完用。
/// SQL-37・TABLE-16・Issue #1587）。`CURRENT_DATE` は日付、それ以外は `TIMESTAMP`
/// （UTC マイクロ秒。精度指定は四捨五入。範囲外は `22008`）。
pub(crate) fn eval_time_default(
    time: crate::catalog::TimeDefault,
    clock: &StatementClock,
) -> Result<crate::row_codec::Value, SqlSurfaceError> {
    use crate::catalog::TimeDefault;
    use crate::row_codec::Value;
    let timestamp = |precision: Option<u8>| -> Result<Value, SqlSurfaceError> {
        let micros = match precision {
            None => clock.txn_start_micros,
            Some(p) => round_to_precision(clock.txn_start_micros, p).ok_or(
                SqlSurfaceError::InvalidParameterValue {
                    detail: "timestamp precision must be between 0 and 6",
                },
            )?,
        };
        Ok(Value::Timestamp(micros))
    };
    // 時刻関数を評価した印（台帳 v3 に評価時刻を残す判定。Issue #2188）。
    mark_clock_used();
    match time {
        TimeDefault::StatementTimestamp => Ok(Value::Timestamp(clock.stmt_start_micros)),
        // 行ごと・呼び出しごとに壁時計を読む（トランザクション・文の時刻には固定しない）。
        TimeDefault::ClockTimestamp => Ok(Value::Timestamp(now_micros()?)),
        TimeDefault::LocalTime(p) => {
            let micros = match p {
                None => clock.txn_start_micros,
                Some(p) => round_to_precision(clock.txn_start_micros, p).ok_or(
                    SqlSurfaceError::InvalidParameterValue {
                        detail: "time precision must be between 0 and 6",
                    },
                )?,
            };
            Ok(Value::Time(micros_to_time_of_day(micros)))
        }
        TimeDefault::Now | TimeDefault::TransactionTimestamp => timestamp(None),
        TimeDefault::CurrentTimestamp(p) | TimeDefault::LocalTimestamp(p) => timestamp(p),
        TimeDefault::CurrentDate => {
            let days = micros_to_days(clock.txn_start_micros).ok_or_else(|| {
                SqlSurfaceError::datetime_field_overflow("current date is out of range")
            })?;
            Ok(Value::Date(days))
        }
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

/// UTC マイクロ秒を 1 日の中の時刻（0 以上 1 日未満のマイクロ秒）へ変換する（`LOCALTIME`。
/// TimeZone 未実装の間は UTC 扱い。設計 D8）。
pub(crate) fn micros_to_time_of_day(micros: i64) -> i64 {
    micros.rem_euclid(MICROS_PER_DAY)
}

/// 評価に時刻関数を使った書き込みの台帳 v3 に残す評価時刻（トランザクション開始時刻）。
/// 現スレッドの [`ClockScope`]／[`DefaultClockScope`] の間に時刻関数が評価されていれば
/// `Some`。`recovery::ledger::record_in_txn_accepting` が参照する（Issue #2188・SQL-37）。
pub(crate) fn ledger_evaluated_at() -> Option<i64> {
    if let Some(overridden) = LEDGER_OVERRIDE.with(|o| o.get()) {
        return overridden;
    }
    if !CLOCK_USED.with(|u| u.get()) {
        return None;
    }
    CURRENT
        .with(|c| c.get())
        .or_else(|| DEFAULT_CLOCK.with(|c| c.get()))
        .map(|c| c.txn_start_micros)
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
    fn default_clock_scope_is_separate_and_restored() {
        let c = StatementClock {
            txn_start_micros: 5,
            stmt_start_micros: 6,
        };
        assert_eq!(current_default_clock(), None);
        {
            let _g = DefaultClockScope::enter(c);
            assert_eq!(current_default_clock(), Some(c));
            // 式束縛用スロットには漏れない。
            assert_eq!(current_clock(), None);
        }
        assert_eq!(current_default_clock(), None);
    }

    #[test]
    fn txn_fixed_only_policy_is_restored() {
        let c = StatementClock {
            txn_start_micros: 1,
            stmt_start_micros: 1,
        };
        assert_eq!(current_clock_policy(), ClockPolicy::All);
        {
            let _g = ClockScope::enter_txn_fixed_only(c);
            assert_eq!(current_clock_policy(), ClockPolicy::TxnFixedOnly);
            {
                let _inner = ClockScope::enter(c);
                assert_eq!(current_clock_policy(), ClockPolicy::All);
            }
            assert_eq!(current_clock_policy(), ClockPolicy::TxnFixedOnly);
        }
        assert_eq!(current_clock_policy(), ClockPolicy::All);
    }

    #[test]
    fn eval_time_default_values() {
        use crate::catalog::TimeDefault;
        use crate::row_codec::Value;
        let c = StatementClock {
            txn_start_micros: 1_234_567,
            stmt_start_micros: 9,
        };
        assert_eq!(
            eval_time_default(TimeDefault::Now, &c).unwrap(),
            Value::Timestamp(1_234_567)
        );
        assert_eq!(
            eval_time_default(TimeDefault::CurrentTimestamp(Some(3)), &c).unwrap(),
            Value::Timestamp(1_235_000)
        );
        assert_eq!(
            eval_time_default(TimeDefault::CurrentDate, &c).unwrap(),
            Value::Date(0)
        );
        assert!(eval_time_default(TimeDefault::LocalTimestamp(Some(7)), &c).is_err());
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
