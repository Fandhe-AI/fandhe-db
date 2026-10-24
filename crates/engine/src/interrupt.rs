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
//! - 将来の `statement_timeout`（#1951）は同じ [`check`] にデッドラインを足すだけで済ませ、
//!   並行する第 2 の仕組みを作らない（[`InterruptReason`] が拡張点）。
//!
//! 安全性: 中断は commit 前の `Err` としてしか起きない（確認点を commit 処理・台帳・成功境界
//! より後ろに置かない）。このため autocommit は副作用ゼロ、明示トランザクションは `Failed`
//! （`25P02`）という既存の契約がそのまま成り立つ。スコープは thread-local なので、他接続の
//! 文へ届く経路は存在しない。

use std::cell::{Cell, RefCell};

/// 中断の理由。エラー文言には出さない（固定文言。件数・テナント情報を含めない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptReason {
    /// クライアントが切断した（wire-server の生存確認が検出）。
    ClientGone,
    /// 利用者による取り消し（将来の CancelRequest 受理用の拡張点。現状は発生しない）。
    UserCancel,
    /// 文のデッドライン超過（#1951／#1950 の statement_timeout 用の拡張点。現状は発生しない）。
    StatementTimeout,
}

/// 中断判定を [`check`] 何回に 1 回行うか。判定本体（ソケット probe 等）は高価なため、
/// 通常の行ごとのコストは thread-local のカウンタ加算だけにする。
pub const CHECK_STRIDE: u32 = 1024;

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
pub struct InterruptGuard {
    depth: usize,
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
        InterruptGuard { depth }
    }
}

/// 走査ループの確認点。スコープが無ければ何もしない。中断が確定していれば
/// [`crate::sql::allowlist::SqlSurfaceError::QueryCanceled`] を返す。
///
/// 呼び出し元は #1797 の早期中断と同じ行ループ（commit 前の読み取り経路）に限る。
pub(crate) fn check() -> Result<(), crate::sql::allowlist::SqlSurfaceError> {
    if SUPPRESSED.with(Cell::get) > 0 {
        return Ok(());
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
        if scope.countdown > 0 {
            scope.countdown -= 1;
            return None;
        }
        scope.countdown = CHECK_STRIDE - 1;
        let reason = (scope.poll)();
        scope.fired = reason;
        reason
    });
    match fired {
        Some(reason) => Err(crate::sql::allowlist::SqlSurfaceError::QueryCanceled { reason }),
        None => Ok(()),
    }
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
