//! 文単位のデッドライン（Issue #1951。SQL-71 のポインタ。設計記録: `docs/design/statement-interrupt.md`）。
//!
//! 役割: 親モジュール [`crate::interrupt`] の協調的中断に「時刻による中断」を足す。
//! [`DeadlineScope::install`] で現在のスレッドへ期限を置くと、engine の走査ループが呼ぶ
//! `interrupt::check()` が [`crate::interrupt::CHECK_STRIDE`] 回に 1 回だけ時計を読み、期限を
//! 過ぎていれば `57014`（[`InterruptReason::StatementTimeout`]）で文を打ち切る。
//!
//! 呼び出し関係:
//! - 導入口は wire-server の `'Q'`／`'E'` 処理（#1954 の `statement_timeout` 結線）や将来の HTTP
//!   表層。本 Issue では engine 側の機構だけを用意し、結線は行わない。
//! - 切断検出（[`crate::interrupt::InterruptScope`] の poll スタック）とは別の thread-local
//!   スロットに持つ。poll スタックは最内スコープしか評価しないため、スタックへ積むと liveness
//!   スコープと重ねたときに片方が評価されなくなる。別スロットなら導入順・入れ子に影響されない。
//! - 入れ子は「厳しい側が勝つ」。内側のスコープで期限を緩める経路は作らない（fail-closed）。
//!
//! 安全性: スロットは thread-local なので他接続の文へ届く経路は無い。[`DeadlineGuard`] は
//! `!Send`・`!Sync` でスレッド間の移動をコンパイル時に禁止する。

use super::{clock_now, InterruptReason, CHECK_STRIDE};
use crate::sql::allowlist::SqlSurfaceError;
use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// 文の期限。無効（期限なし）か、絶対時刻 1 つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatementDeadline(Option<Instant>);

impl StatementDeadline {
    /// 期限なし。
    pub const fn disabled() -> Self {
        Self(None)
    }

    /// `start` から `timeout` 後を期限にする。`timeout` が 0 なら無効（PostgreSQL の
    /// `statement_timeout = 0` と同じ）。加算が `Instant` の表現範囲を超える場合も、到達不能な
    /// 期限として無効と同値に扱う（PostgreSQL の値域では起きない）。
    pub fn after(start: Instant, timeout: Duration) -> Self {
        if timeout.is_zero() {
            return Self(None);
        }
        Self(start.checked_add(timeout))
    }

    /// 絶対時刻で指定する（テスト・期限の引き継ぎ用）。
    pub const fn at(instant: Instant) -> Self {
        Self(Some(instant))
    }

    /// 期限が設定されているか。
    pub const fn is_enabled(&self) -> bool {
        self.0.is_some()
    }

    /// `now` の時点で期限を過ぎているか（無効なら常に `false`）。
    pub fn is_expired_at(&self, now: Instant) -> bool {
        self.0.is_some_and(|at| now >= at)
    }

    /// `now` からの残り時間。無効は `None`、期限後は `Some(ZERO)`。書き込みロック待ちを期限内に
    /// 数える結線（#1953。[`effective_lock_wait`]）が使う。
    pub fn remaining_at(&self, now: Instant) -> Option<Duration> {
        self.0.map(|at| at.saturating_duration_since(now))
    }

    /// 現在時刻からの残り時間。無効なら時計を読まず `None`。
    fn remaining_at_now(&self) -> Option<Duration> {
        if self.0.is_some() {
            self.remaining_at(clock_now())
        } else {
            None
        }
    }

    /// 即時に時計を読んで判定する（文境界などの粗い確認点用）。無効なら時計を読まない。
    pub fn check(&self) -> Result<(), SqlSurfaceError> {
        if self.0.is_some() && self.is_expired_at(clock_now()) {
            return Err(SqlSurfaceError::QueryCanceled {
                reason: InterruptReason::StatementTimeout,
            });
        }
        Ok(())
    }
}

/// thread-local のデッドラインスロット。`Copy` なのでガードが直前の値を丸ごと保存・復元できる。
#[derive(Clone, Copy)]
struct Slot {
    at: Option<Instant>,
    /// 次に時計を読むまでの残り確認回数。0（導入直後を含む）で時計を読む。
    countdown: u32,
    /// 一度期限超過を検出したら以降の確認も失敗させる（ラッチ）。
    fired: bool,
    /// テスト用プローブ: 確認回数がこの値に達したら期限超過として扱う（時計を読まない）。
    fire_after: Option<u32>,
    /// プローブ有効時の確認回数（[`checks_seen`] が返す）。
    seen: u32,
}

const EMPTY: Slot = Slot {
    at: None,
    countdown: 0,
    fired: false,
    fire_after: None,
    seen: 0,
};

thread_local! {
    static DEADLINE: Cell<Slot> = const { Cell::new(EMPTY) };
}

/// [`DeadlineScope::install`] が返す RAII ガード。drop で直前のスロットへ戻す（入れ子対応）。
///
/// drop は「drop したスレッド」のスロットを書き換えるため、別スレッドへ移すと作成元の期限が
/// 残り後続の文まで打ち切られる。`PhantomData<Rc<()>>` で `!Send`・`!Sync` にする。
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<engine::interrupt::DeadlineGuard>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<engine::interrupt::DeadlineGuard>();
/// ```
#[must_use = "dropping the guard immediately uninstalls the statement deadline"]
pub struct DeadlineGuard {
    prev: Slot,
    _not_send: PhantomData<Rc<()>>,
}

impl Drop for DeadlineGuard {
    fn drop(&mut self) {
        // panic 中の drop でも二重 panic を起こさないよう `try_with` を使う。
        let prev = self.prev;
        let _ = DEADLINE.try_with(|c| c.set(prev));
    }
}

/// デッドラインの導入口。
pub struct DeadlineScope;

impl DeadlineScope {
    /// 現在のスレッドへ `deadline` を置き、drop で元に戻すガードを返す。既に期限がある場合は
    /// 早い方を採る（内側で緩められない）。無効な期限を入れても外側の期限は維持される。
    pub fn install(deadline: StatementDeadline) -> DeadlineGuard {
        let prev = DEADLINE.with(Cell::get);
        let at = match (prev.at, deadline.0) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, None) => a,
            (None, b) => b,
        };
        DEADLINE.with(|c| {
            c.set(Slot {
                at,
                countdown: 0,
                fired: prev.fired,
                fire_after: None,
                seen: 0,
            })
        });
        DeadlineGuard {
            prev,
            _not_send: PhantomData,
        }
    }
}

/// テスト専用プローブ（`#[doc(hidden)]`。本番経路からは呼ばない）。時計に依存せず、`check()` が
/// `n` 回通過した後の `n + 1` 回目で `57014` にする。走査完了後の確認点（JOIN・集約・集合演算等）
/// だけを決定的に発火させる結合テスト（Issue #1952）が使う。`u32::MAX` なら発火せず確認回数だけ
/// 数える（[`checks_seen`]）。drop で直前のスロットへ戻る。
#[doc(hidden)]
pub fn install_fire_after_checks(n: u32) -> DeadlineGuard {
    let prev = DEADLINE.with(Cell::get);
    DEADLINE.with(|c| {
        c.set(Slot {
            // 実時刻は参照されない（プローブ有効時は時計を読まない）。有効判定のための印。
            at: Some(Instant::now()),
            countdown: 0,
            fired: false,
            fire_after: Some(n),
            seen: 0,
        })
    });
    DeadlineGuard {
        prev,
        _not_send: PhantomData,
    }
}

/// [`install_fire_after_checks`] 有効中の確認回数（テスト専用。無効時は 0）。
#[doc(hidden)]
pub fn checks_seen() -> u32 {
    DEADLINE.with(Cell::get).seen
}

/// 現在のスレッドの実効期限（無効なら [`StatementDeadline::disabled`]）。
pub fn current_deadline() -> StatementDeadline {
    StatementDeadline(DEADLINE.with(Cell::get).at)
}

/// 書き込みロック（writer gate）の実効待機時間（#1953。SQL-71 (3) のポインタ）。期限が無効なら
/// 時計を読まず `configured` をそのまま返し、有効なら `min(configured, 残り時間)` にする。
/// 待機時間を減らす方向にしか働かないため、`write_lock_wait` の上限は緩まない。`Duration::ZERO`
/// （NOWAIT）は常に `ZERO` のまま。storage の writer 取得点から呼ばれる。
pub(crate) fn effective_lock_wait(configured: Duration) -> Duration {
    let current = current_deadline();
    match current.remaining_at_now() {
        Some(remaining) => configured.min(remaining),
        None => configured,
    }
}

/// 現在のスレッドの期限が既に切れているか（無効なら時計を読まず `false`）。
pub(crate) fn is_expired_now() -> bool {
    let current = current_deadline();
    current.is_enabled() && current.is_expired_at(clock_now())
}

/// writer gate 待ちの失敗（`55P03` 相当）を表層エラーへ写す共通点。待機中に文の期限が来て
/// いたなら `57014`（StatementTimeout）、そうでなければ `55P03`（`LockNotAvailable`）を返す。
/// エラー文言は固定で、件数・テナント・期限値を含めない。
pub(crate) fn lock_wait_error() -> SqlSurfaceError {
    if is_expired_now() {
        SqlSurfaceError::QueryCanceled {
            reason: InterruptReason::StatementTimeout,
        }
    } else {
        SqlSurfaceError::LockNotAvailable
    }
}

/// `interrupt::check()` から呼ばれる確認点本体。無効時のコストは thread-local の読み出しと
/// 分岐 1 つだけで、時計は読まない。有効時は [`CHECK_STRIDE`] 回に 1 回だけ時計を読む。
pub(super) fn poll() -> Option<InterruptReason> {
    DEADLINE.with(|c| {
        let mut s = c.get();
        let at = s.at?;
        if s.fired {
            return Some(InterruptReason::StatementTimeout);
        }
        if let Some(limit) = s.fire_after {
            if s.seen >= limit {
                s.fired = true;
                c.set(s);
                return Some(InterruptReason::StatementTimeout);
            }
            s.seen += 1;
            c.set(s);
            return None;
        }
        if s.countdown > 0 {
            s.countdown -= 1;
            c.set(s);
            return None;
        }
        s.countdown = CHECK_STRIDE - 1;
        let expired = clock_now() >= at;
        s.fired = expired;
        c.set(s);
        expired.then_some(InterruptReason::StatementTimeout)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::interrupt::{check, clock_reads, suppressed, InterruptScope};

    const HOUR: Duration = Duration::from_secs(3600);

    fn past() -> StatementDeadline {
        StatementDeadline::at(Instant::now())
    }

    fn future() -> StatementDeadline {
        StatementDeadline::after(Instant::now(), HOUR)
    }

    #[test]
    fn zero_timeout_is_disabled() {
        let d = StatementDeadline::after(Instant::now(), Duration::ZERO);
        assert!(!d.is_enabled());
        assert_eq!(d, StatementDeadline::disabled());
        assert_eq!(StatementDeadline::default(), d);
        assert!(!d.is_expired_at(Instant::now()));
        assert_eq!(d.remaining_at(Instant::now()), None);
        assert!(d.check().is_ok());
    }

    #[test]
    fn overflowing_timeout_is_disabled() {
        let d = StatementDeadline::after(Instant::now(), Duration::MAX);
        assert!(!d.is_enabled());
    }

    #[test]
    fn expiry_and_remaining() {
        let now = Instant::now();
        let d = StatementDeadline::at(now);
        assert!(d.is_enabled());
        assert!(d.is_expired_at(now));
        assert_eq!(d.remaining_at(now + HOUR), Some(Duration::ZERO));
        let f = StatementDeadline::after(now, HOUR);
        assert!(!f.is_expired_at(now));
        assert_eq!(f.remaining_at(now), Some(HOUR));
    }

    #[test]
    fn immediate_check_maps_to_57014_with_timeout_message() {
        let err = past().check().unwrap_err();
        assert_eq!(err.wire_code(), "57014");
        assert_eq!(
            err.to_string(),
            "canceling statement due to statement timeout"
        );
        assert!(future().check().is_ok());
    }

    #[test]
    fn scope_detects_expiry_on_first_check_and_latches() {
        let _g = DeadlineScope::install(past());
        let err = check().unwrap_err();
        assert!(matches!(
            err,
            SqlSurfaceError::QueryCanceled {
                reason: InterruptReason::StatementTimeout
            }
        ));
        let reads = clock_reads();
        assert!(check().is_err());
        assert_eq!(clock_reads(), reads, "latched: no further clock read");
    }

    #[test]
    fn disabled_never_reads_the_clock() {
        let before = clock_reads();
        for _ in 0..(CHECK_STRIDE * 3) {
            assert!(check().is_ok());
        }
        assert_eq!(clock_reads(), before);
        let _g = DeadlineScope::install(StatementDeadline::disabled());
        for _ in 0..(CHECK_STRIDE * 3) {
            assert!(check().is_ok());
        }
        assert_eq!(clock_reads(), before);
    }

    #[test]
    fn enabled_reads_the_clock_once_per_stride() {
        let _g = DeadlineScope::install(future());
        let before = clock_reads();
        for _ in 0..(CHECK_STRIDE * 3) {
            assert!(check().is_ok());
        }
        assert_eq!(clock_reads() - before, 3);
    }

    #[test]
    fn guard_drop_restores_previous_state() {
        {
            let _g = DeadlineScope::install(past());
            assert!(current_deadline().is_enabled());
            assert!(check().is_err());
        }
        assert!(!current_deadline().is_enabled());
        assert!(check().is_ok());
    }

    #[test]
    fn nested_scope_cannot_loosen_the_outer_deadline() {
        let outer = DeadlineScope::install(past());
        {
            let _inner = DeadlineScope::install(future());
            assert!(check().is_err(), "outer deadline still wins");
        }
        drop(outer);
        assert!(check().is_ok());
    }

    #[test]
    fn inner_tighter_deadline_is_dropped_with_its_guard() {
        let _outer = DeadlineScope::install(future());
        {
            let _inner = DeadlineScope::install(past());
            assert!(check().is_err());
        }
        assert!(check().is_ok(), "outer slot restored");
    }

    #[test]
    fn coexists_with_poll_scope_in_either_order() {
        // 期限が先・poll が後。
        {
            let _d = DeadlineScope::install(past());
            let _p = InterruptScope::install(Box::new(|| Some(InterruptReason::ClientGone)));
            assert!(check().is_err());
        }
        // 期限が切れていなくても poll の ClientGone が効く。
        {
            let _d = DeadlineScope::install(future());
            let _p = InterruptScope::install(Box::new(|| Some(InterruptReason::ClientGone)));
            assert!(check().is_err());
        }
        // poll が先・期限が後。poll は発火しないので期限で止まる。
        {
            let _p = InterruptScope::install(Box::new(|| None));
            let _d = DeadlineScope::install(past());
            assert!(check().is_err());
        }
        {
            let _p = InterruptScope::install(Box::new(|| None));
            let _d = DeadlineScope::install(future());
            assert!(check().is_ok());
        }
    }

    #[test]
    fn suppressed_skips_the_deadline() {
        let _g = DeadlineScope::install(past());
        assert!(suppressed(|| check().is_ok()));
        assert!(check().is_err());
    }

    #[test]
    fn lock_wait_helpers() {
        // 無効: 時計を読まず configured のまま、写像は 55P03。
        let before = clock_reads();
        assert_eq!(
            effective_lock_wait(Duration::from_secs(30)),
            Duration::from_secs(30)
        );
        assert!(!is_expired_now());
        assert_eq!(clock_reads(), before);
        assert!(matches!(
            lock_wait_error(),
            SqlSurfaceError::LockNotAvailable
        ));
        // 有効（十分先）: min は configured 側。NOWAIT は ZERO のまま。
        {
            let _g = DeadlineScope::install(StatementDeadline::after(Instant::now(), HOUR));
            assert_eq!(
                effective_lock_wait(Duration::from_secs(30)),
                Duration::from_secs(30)
            );
            assert_eq!(effective_lock_wait(Duration::ZERO), Duration::ZERO);
            assert!(matches!(
                lock_wait_error(),
                SqlSurfaceError::LockNotAvailable
            ));
        }
        // 有効（残りが configured より短い）: 残り側。
        {
            let _g = DeadlineScope::install(StatementDeadline::after(
                Instant::now(),
                Duration::from_secs(5),
            ));
            let w = effective_lock_wait(Duration::from_secs(30));
            assert!(w <= Duration::from_secs(5));
        }
        // 期限切れ: ZERO で 57014。
        {
            let _g = DeadlineScope::install(past());
            assert_eq!(effective_lock_wait(Duration::from_secs(30)), Duration::ZERO);
            assert!(is_expired_now());
            assert!(matches!(
                lock_wait_error(),
                SqlSurfaceError::QueryCanceled {
                    reason: InterruptReason::StatementTimeout
                }
            ));
        }
    }
}
