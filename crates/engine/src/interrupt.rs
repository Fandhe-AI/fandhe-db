//! 文の協調的中断（Issue #1798。設計記録: `docs/design/statement-interrupt.md`）。
//!
//! 役割: wire-server の接続スレッドが「この文の実行を止めたい」状況（クライアント切断など）
//! を検出する `poll` を [`InterruptScope::install`] で thread-local に積み、engine の長い
//! 走査ループが [`check`] を呼ぶたびに（[`CHECK_STRIDE`] 回に 1 回）その `poll` を評価して
//! `57014`（[`crate::error_format::ErrorClass::QueryCanceled`]）で打ち切る。
//!
//! 呼び出し関係:
//! - wire-server の `post_auth_loop` が `'Q'`／`'E'` のメッセージ処理ごとにスコープを入れる。
//! - engine 側の確認点（`sql::scan`・`window`・`group_by`・`aggregate`・`join`・
//!   `relation_snapshot`）は #1797 の `cap + 1` 件目の早期中断と同じ行ループに `check()?` を置く。
//! - 文単位のデッドライン（#1951。[`DeadlineScope`]・[`StatementDeadline`]）は同じ [`check`] に
//!   時刻比較を足して実現し、並行する第 2 の仕組みは作らない。poll スタックとは別の
//!   thread-local スロットに持つ（`deadline.rs` の `//!` 参照）。導入は wire-server（#1954）が行う。
//! - #1953 で書き込み（INSERT／UPDATE／DELETE／UPSERT／TRUNCATE）・COPY・ベクトル検索（arena
//!   走査・DISTANCE 段）・GPU バッチの確認点と、書き込みロック待ちの期限内計上
//!   （[`effective_lock_wait`]・[`lock_wait_error`]）を足した。分割実行 DML は [`suppressed`] を
//!   維持しつつ、期限だけ注入時計で明示評価する（`tenant/partitioned_dml.rs`）。
//! - 集計の 3 つの行ループ（#3208）は [`RowCheck`] 経由で確認し、行ごとのコストをループ局所の
//!   カウンタだけにする（[`ROW_CHECK_INTERVAL`] 行ごとに即時評価）。
//!
//! 安全性: 中断は commit 前の `Err` としてしか起きない（確認点を commit 処理・台帳・成功境界
//! より後ろに置かない）。このため autocommit は副作用ゼロ、明示トランザクションは `Failed`
//! （`25P02`）という既存の契約がそのまま成り立つ。スコープは thread-local なので、他接続の
//! 文へ届く経路は存在しない。

mod deadline;
mod statement_timeout;

pub use deadline::{
    checks_seen, current_deadline, install_fire_after_checks, DeadlineGuard, DeadlineScope,
    StatementDeadline,
};
pub(crate) use deadline::{effective_lock_wait, is_expired_now, lock_wait_error};
pub use statement_timeout::{
    format_statement_timeout, parse_statement_timeout_text, validate_statement_timeout_ms,
    StatementTimeoutOutOfRange, DEFAULT_STATEMENT_TIMEOUT_MS, INVALID_STATEMENT_TIMEOUT_DETAIL,
    MAX_STATEMENT_TIMEOUT_MS,
};

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::Instant;

/// 中断の理由。エラー文言は理由ごとの固定文言のみで、件数・テナント情報を含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptReason {
    /// クライアントが切断した（wire-server の生存確認が検出）。
    ClientGone,
    /// 利用者による取り消し（将来の CancelRequest 受理用の拡張点。現状は発生しない）。
    UserCancel,
    /// 文のデッドライン超過（#1951 の [`DeadlineScope`] が [`check`] で検出する）。
    StatementTimeout,
}

/// 中断判定を [`check`] 何回に 1 回行うか。判定本体（ソケット probe 等）は高価なため、
/// 通常の行ごとのコストは thread-local のカウンタ加算だけにする。
pub const CHECK_STRIDE: u32 = 1024;

/// 時計の読み出し口。デッドライン判定の唯一の時計読み出し点で、テストでは読み出し回数を数えて
/// 「無効時は時計を読まない」「stride ごとに 1 回」を決定的に検証する。
pub(crate) fn clock_now() -> Instant {
    #[cfg(test)]
    CLOCK_READS.with(|c| c.set(c.get() + 1));
    Instant::now()
}

#[cfg(test)]
thread_local! {
    static CLOCK_READS: Cell<u64> = const { Cell::new(0) };
}

/// このスレッドでの [`clock_now`] の累計呼び出し回数（テスト専用）。
#[cfg(test)]
pub(crate) fn clock_reads() -> u64 {
    CLOCK_READS.with(Cell::get)
}

type PollFn = Box<dyn FnMut() -> Option<InterruptReason>>;

struct Scope {
    poll: PollFn,
    /// 次に `poll` を呼ぶまでの残り回数。0 のとき（初回を含む）に必ず `poll` を呼ぶ。
    countdown: u32,
    /// 一度中断が確定したら以降の [`check`] も同じ理由で失敗させる（ラッチ）。
    fired: Option<InterruptReason>,
}

thread_local! {
    static STACK: RefCell<Vec<Scope>> = const { RefCell::new(Vec::new()) };
    static SUPPRESSED: Cell<u32> = const { Cell::new(0) };
}

/// [`InterruptScope::install`] が返す RAII ガード。drop でスコープを外す（入れ子対応）。
#[must_use = "dropping the guard immediately uninstalls the interrupt scope"]
///
/// drop は「drop したスレッド」の thread-local スタックを切り詰めるため、別スレッドへ移すと
/// 作成元のスコープが残り後続の文までキャンセルされる。`PhantomData<Rc<()>>` で `!Send`・`!Sync`
/// にして、スレッド間の移動をコンパイル時に禁止する（下記 `compile_fail` doc test が保証）。
///
/// ```compile_fail
/// fn assert_send<T: Send>() {}
/// assert_send::<engine::interrupt::InterruptGuard>();
/// ```
///
/// ```compile_fail
/// fn assert_sync<T: Sync>() {}
/// assert_sync::<engine::interrupt::InterruptGuard>();
/// ```
pub struct InterruptGuard {
    depth: usize,
    _not_send: PhantomData<Rc<()>>,
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        // panic 中の drop でも二重 panic を起こさないよう `try_with`／`try_borrow_mut` を使う。
        let depth = self.depth;
        let _ = STACK.try_with(|s| {
            if let Ok(mut s) = s.try_borrow_mut() {
                s.truncate(depth);
            }
        });
    }
}

/// 中断スコープの導入口。
pub struct InterruptScope;

impl InterruptScope {
    /// 現在のスレッドに `poll` を積み、drop で外すガードを返す。
    ///
    /// `poll` は [`check`] から [`CHECK_STRIDE`] 回に 1 回（導入後の最初の呼び出しでは必ず）
    /// 呼ばれ、`Some(reason)` を返すと文は `57014` で打ち切られる。`poll` の内側から
    /// [`check`] を呼んではならない（再入は無視され何もしない）。
    pub fn install(poll: Box<dyn FnMut() -> Option<InterruptReason>>) -> InterruptGuard {
        let depth = STACK.with(|s| {
            let mut s = s.borrow_mut();
            let depth = s.len();
            s.push(Scope {
                poll,
                countdown: 0,
                fired: None,
            });
            depth
        });
        InterruptGuard {
            depth,
            _not_send: PhantomData,
        }
    }
}

/// 走査ループの確認点。スコープが無ければ何もしない。中断が確定していれば
/// [`crate::sql::allowlist::SqlSurfaceError::QueryCanceled`] を返す。
///
/// 呼び出し元は #1797 の早期中断と同じ行ループ（commit 前の読み取り経路）に限る。
pub(crate) fn check() -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    check_reason()
        .map_err(|reason| crate::sql::allowlist::SqlSurfaceError::QueryCanceled { reason })
}

/// [`check`] の理由だけを返す版。`SqlSurfaceError` を持たない層（`ArenaError`・
/// `BatchSearchError` など。#1953）が自分のエラー型へ写すための確認点で、判定は [`check`] と
/// 同一（ビット同一の挙動）。
pub(crate) fn check_reason() -> Result<(), InterruptReason> {
    check_reason_inner(false)
}

/// [`check_reason`] の本体。`force` が真なら内部の [`CHECK_STRIDE`] 間引きを飛ばして、
/// 抑止 → 期限 → poll スタックの順に即時評価する（Issue #3208。[`RowCheck`] 専用）。
fn check_reason_inner(force: bool) -> Result<(), InterruptReason> {
    if SUPPRESSED.with(Cell::get) > 0 {
        return Ok(());
    }
    // デッドラインは poll スタックと独立に評価する（無効時は時計を読まない）。
    if let Some(reason) = deadline::poll(force) {
        return Err(reason);
    }
    let fired = STACK.with(|s| {
        // 再入（`poll` 内からの `check`）は借用失敗として無視する。
        let Ok(mut s) = s.try_borrow_mut() else {
            return None;
        };
        let scope = s.last_mut()?;
        if let Some(r) = scope.fired {
            return Some(r);
        }
        if !force && scope.countdown > 0 {
            scope.countdown -= 1;
            return None;
        }
        scope.countdown = CHECK_STRIDE - 1;
        let reason = (scope.poll)();
        scope.fired = reason;
        reason
    });
    match fired {
        Some(reason) => Err(reason),
        None => Ok(()),
    }
}

/// 行ループ局所の間引き間隔（Issue #3208）。[`CHECK_STRIDE`] と同じ値にして、中断が効くまでの
/// 最大遅延（最も重い 1 反復 × この行数）を従来の契約から変えない。根拠は
/// `docs/design/aggregate-loop-regression.md`。
pub(crate) const ROW_CHECK_INTERVAL: u32 = CHECK_STRIDE;

/// 行ループ局所の中断確認ティッカー（Issue #3208。#2888 の是正）。
///
/// 役割: 集計の行ループが行ごとに [`check`] を呼ぶと thread-local を毎行触るため、間引きカウンタを
/// ループ側の局所値に持ち、[`ROW_CHECK_INTERVAL`] 行に 1 回だけ即時評価（[`check_due`]）する。
/// 最初の `tick` は必ず評価する（先頭行での確認に依存する既存契約を維持）。抑止・ラッチ・
/// 再入無視の意味は [`check`] と同一で、エラー型・文言も同じ。
pub(crate) struct RowCheck {
    left: u32,
}

impl RowCheck {
    /// 最初の `tick` で必ず評価する状態で作る。
    pub(crate) fn new() -> Self {
        Self { left: 0 }
    }

    /// 1 行分の確認。通常は局所カウンタの減算と分岐だけで thread-local に触らない。
    #[inline(always)]
    pub(crate) fn tick(&mut self) -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
        if self.left > 0 {
            self.left -= 1;
            return Ok(());
        }
        self.left = ROW_CHECK_INTERVAL - 1;
        check_due()
    }
}

/// [`RowCheck::tick`] の評価点。間引きを通さず即時に判定する。
#[cold]
#[inline(never)]
fn check_due() -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    check_reason_inner(true)
        .map_err(|reason| crate::sql::allowlist::SqlSurfaceError::QueryCanceled { reason })
}

/// 粗い境界（点更新の入口・GPU の group／tile／chunk 境界など、通過回数が [`CHECK_STRIDE`] に
/// 届かない確認点）用の即時確認（#1953。SQL-71 のポインタ）。stride 間引きをせず毎回時計を
/// 読む。`suppressed` 区間では no-op、デッドライン無効時は時計を読まない。
pub(crate) fn check_now_reason() -> Result<(), InterruptReason> {
    if SUPPRESSED.with(Cell::get) > 0 {
        return Ok(());
    }
    current_deadline()
        .check()
        .map_err(|_| InterruptReason::StatementTimeout)
}

/// 中断しない区間で `f` を実行する。分割実行 DML（PARTITIONED）のように、チャンク commit 後に
/// `57014` を返すと SQL-32／SQL-71 (4) の契約（commit 済みなら `VD001`）とずれる経路を包む。
pub(crate) fn suppressed<R>(f: impl FnOnce() -> R) -> R {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = SUPPRESSED.try_with(|c| c.set(c.get().saturating_sub(1)));
        }
    }
    SUPPRESSED.with(|c| c.set(c.get().saturating_add(1)));
    let _restore = Restore;
    f()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn counting_poll(
        calls: Rc<Cell<u32>>,
        fire_on: u32,
    ) -> Box<dyn FnMut() -> Option<InterruptReason>> {
        Box::new(move || {
            calls.set(calls.get() + 1);
            (calls.get() >= fire_on).then_some(InterruptReason::ClientGone)
        })
    }

    /// #1953: stride 間引きの `check_reason` は少数回の通過では期限後も気付かないが、
    /// `check_now_reason` は毎回時計を読んで即座に失敗する（GPU の tile／chunk 境界・点更新の入口用）。
    #[test]
    fn check_now_reason_detects_expiry_without_stride() {
        let _g = DeadlineScope::install(StatementDeadline::after(
            Instant::now(),
            std::time::Duration::from_millis(30),
        ));
        assert!(check_reason().is_ok());
        std::thread::sleep(std::time::Duration::from_millis(60));
        // stride 内の 2 回目は時計を読まないため通ってしまう（これが GPU 境界で不十分だった理由）。
        assert!(check_reason().is_ok());
        assert_eq!(check_now_reason(), Err(InterruptReason::StatementTimeout));
        // suppressed 区間では no-op。
        assert!(suppressed(check_now_reason).is_ok());
    }

    /// #3208: `RowCheck` は最初の tick で poll を 1 回呼び、以後は `ROW_CHECK_INTERVAL` 行ごとに 1 回。
    #[test]
    fn row_check_polls_first_then_once_per_interval() {
        let calls = Rc::new(Cell::new(0));
        let _g = InterruptScope::install(counting_poll(Rc::clone(&calls), u32::MAX));
        let mut rc = RowCheck::new();
        assert!(rc.tick().is_ok());
        assert_eq!(calls.get(), 1);
        for _ in 0..(ROW_CHECK_INTERVAL - 1) {
            assert!(rc.tick().is_ok());
        }
        assert_eq!(calls.get(), 1);
        assert!(rc.tick().is_ok());
        assert_eq!(calls.get(), 2);
        for k in 3..=4u32 {
            for _ in 0..ROW_CHECK_INTERVAL {
                assert!(rc.tick().is_ok());
            }
            assert_eq!(calls.get(), k);
        }
    }

    /// #3208: 発火後はラッチされ、`tick` も `check` も同じ理由で失敗し続ける。
    #[test]
    fn row_check_latches_and_shares_state_with_check() {
        let calls = Rc::new(Cell::new(0));
        let _g = InterruptScope::install(counting_poll(Rc::clone(&calls), 1));
        let mut rc = RowCheck::new();
        assert!(rc.tick().is_err());
        assert!(check().is_err());
        assert_eq!(calls.get(), 1);
    }

    /// #3208: `suppressed` 区間では発火済みでも期限切れでも `tick` は通る。スコープ無しは no-op。
    #[test]
    fn row_check_respects_suppressed_and_no_scope() {
        let mut rc = RowCheck::new();
        for _ in 0..(ROW_CHECK_INTERVAL * 2) {
            assert!(rc.tick().is_ok());
        }
        let calls = Rc::new(Cell::new(0));
        let _g = InterruptScope::install(counting_poll(Rc::clone(&calls), 1));
        let mut rc = RowCheck::new();
        assert!(suppressed(|| rc.tick()).is_ok());
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn no_scope_is_noop() {
        for _ in 0..(CHECK_STRIDE * 3) {
            assert!(check().is_ok());
        }
    }

    #[test]
    fn first_check_polls_immediately() {
        let calls = Rc::new(Cell::new(0));
        let _g = InterruptScope::install(counting_poll(Rc::clone(&calls), 1));
        assert!(check().is_err());
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn polls_once_per_stride_and_latches() {
        let calls = Rc::new(Cell::new(0));
        let _g = InterruptScope::install(counting_poll(Rc::clone(&calls), 3));
        for _ in 0..(CHECK_STRIDE * 2) {
            assert!(check().is_ok());
        }
        assert_eq!(calls.get(), 2);
        assert!(check().is_err());
        assert_eq!(calls.get(), 3);
        // ラッチ: 以降は poll を呼ばずに失敗し続ける。
        assert!(check().is_err());
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn guard_drop_uninstalls_and_nesting_restores_outer() {
        let outer = Rc::new(Cell::new(0));
        let inner = Rc::new(Cell::new(0));
        let g1 = InterruptScope::install(counting_poll(Rc::clone(&outer), u32::MAX));
        {
            let _g2 = InterruptScope::install(counting_poll(Rc::clone(&inner), 1));
            assert!(check().is_err());
        }
        // 内側が外れたら外側のスコープが有効（外側の初回 poll は未消費）。
        assert!(check().is_ok());
        assert_eq!(outer.get(), 1);
        drop(g1);
        assert!(check().is_ok());
        assert_eq!(outer.get(), 1);
    }

    #[test]
    fn suppressed_skips_checks_and_restores() {
        let calls = Rc::new(Cell::new(0));
        let _g = InterruptScope::install(counting_poll(Rc::clone(&calls), 1));
        let r = suppressed(|| check().is_ok());
        assert!(r);
        assert_eq!(calls.get(), 0);
        assert!(check().is_err());
    }

    #[test]
    fn query_canceled_maps_to_57014() {
        let _g = InterruptScope::install(Box::new(|| Some(InterruptReason::ClientGone)));
        let err = check().unwrap_err();
        assert_eq!(err.wire_code(), "57014");
    }
}
