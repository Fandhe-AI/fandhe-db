//! 同期レプリケーション（オプション）の確認待ちの調停役（Issue #2245・親 #2171・RECOVER-15 ポインタ。
//! 方式の記録は `docs/design/replication-streaming.md` の「実装判断（Issue #2245）」）。
//!
//! 責務境界: wire-server の送信側（`replication.rs`）が同期対象のレプリカの受信・永続化の位置を報告し、
//! wire-server の応答境界（`sync_commit_wait.rs`）が commit した LSN の確認を待つ。engine 側は位置の
//! 勘定・待機・上限超過の扱い（`reject`／`degrade`）だけを持ち、ネットワークもトランザクションも知らない。
//! 待機は commit の書き込みゲートの permit を解放した後に呼び出し元が行う契約で、本型は commit 経路
//! （`recovery::commit_boundary`）から呼ばれない（engine の commit の `Err` = durable でない、という契約を
//! 変えないため）。
//!
//! - 同期対象はレプリケーションのロール名で指定し、指定した**全ロール**の確認が揃ったら完了とする
//! - 同期ロール 1 つにつき接続は 1 本まで（2 本目は [`AttachError::Busy`]）
//! - 切断したロールの位置は外す（再接続後の attach と ack で戻る）。待機は打ち切らず、上限まで待つ
//! - 上限超過は [`SyncTimeoutAction::Reject`]（呼び出し元が may_be_committed を返す）か
//!   [`SyncTimeoutAction::Degrade`]（成功を返し、縮退中は待たない。全ロールが要求済みの最大 LSN に
//!   追いついたら自動で解除する）
//!
//! 運用者向けの標準エラーには固定文言だけを出し、ロール名・LSN・テナント情報は載せない。

use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// 待機中に `interrupted` を確認する刻み。
const WAIT_STEP: Duration = Duration::from_millis(100);

/// 待つ確認の段階（PostgreSQL の `synchronous_commit` の値に対応する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncCommitStage {
    /// レプリカが受け取った（`R`）時点で満たす。
    RemoteWrite,
    /// レプリカが永続化した（`K`）時点で満たす。
    On,
    /// 適用が済んだ時点で満たす。受信側の実装上、永続化と適用は同じ commit のため [`Self::On`] と同じ事象。
    RemoteApply,
}

/// 確認が上限までに揃わなかったときの扱い。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncTimeoutAction {
    /// 成功を返し、縮退状態にする（運用者が明示的に選んだときだけ）。
    Degrade,
    /// may_be_committed のエラーにする（成功には倒さない）。
    Reject,
}

/// 同期レプリケーションの設定（`main.rs` が検証済みの CLI 値から作る）。
#[derive(Debug, Clone)]
pub struct SyncCommitConfig {
    /// 同期対象のレプリケーションのロール名（重複なし・1 件以上）。
    pub standbys: Vec<String>,
    /// 待つ確認の段階。
    pub stage: SyncCommitStage,
    /// 待機の上限。`None` は無期限。
    pub timeout: Option<Duration>,
    /// 上限超過時の扱い（`timeout` が `Some` のときだけ使う）。
    pub on_timeout: SyncTimeoutAction,
}

/// [`SyncCommitCoordinator::wait`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncWaitOutcome {
    /// 全同期ロールが確認した。
    Confirmed,
    /// 縮退中、または上限超過で縮退に入った（成功を返してよい）。
    Degraded,
    /// 上限を超えた（`Reject`）。
    TimedOut,
    /// 呼び出し元が中断を指示した（切断・停止）。成功を返してはならない。
    Interrupted,
}

/// [`SyncCommitCoordinator::attach_standby`] の失敗。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachError {
    /// 同じロールが既に接続している。
    Busy,
}

#[derive(Debug, Default, Clone, Copy)]
struct Standby {
    attached: bool,
    received: u64,
    durable: u64,
}

#[derive(Debug)]
struct State {
    standbys: HashMap<String, Standby>,
    /// これまでに待機を求められた最大の LSN（縮退の解除判定に使う）。
    max_required: u64,
    degraded: bool,
    /// 待機の開始を送信側へ知らせる連番（送信側が commit のポーリングを早める）。
    wake_seq: u64,
}

/// 同期レプリケーションの確認待ちの調停役。`EngineCore` が `Arc` で保持する。
#[derive(Debug)]
pub struct SyncCommitCoordinator {
    stage: SyncCommitStage,
    timeout: Option<Duration>,
    on_timeout: SyncTimeoutAction,
    state: Mutex<State>,
    /// 位置の更新・縮退の変化を待機側へ知らせる。
    changed: Condvar,
    /// 待機の開始を送信側へ知らせる。
    wake: Condvar,
}

/// 同期ロールの接続を表すハンドル。`Drop` でそのロールの位置を外して待機側を起こす。
#[derive(Debug)]
pub struct StandbyHandle {
    coordinator: Arc<SyncCommitCoordinator>,
    role: String,
}

impl SyncCommitCoordinator {
    /// 設定から作る。
    pub fn new(cfg: SyncCommitConfig) -> Self {
        let standbys = cfg
            .standbys
            .into_iter()
            .map(|r| (r, Standby::default()))
            .collect();
        Self {
            stage: cfg.stage,
            timeout: cfg.timeout,
            on_timeout: cfg.on_timeout,
            state: Mutex::new(State {
                standbys,
                max_required: 0,
                degraded: false,
                wake_seq: 0,
            }),
            changed: Condvar::new(),
            wake: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 接続したレプリカを追跡する。同期対象外のロールは `Ok(None)`（追跡しない）。同じロールが既に
    /// 接続していれば [`AttachError::Busy`]。`start_lsn` はレプリカが申告した適用済みの LSN（永続化の初期位置）。
    pub fn attach_standby(
        self: &Arc<Self>,
        role: &str,
        start_lsn: u64,
    ) -> Result<Option<StandbyHandle>, AttachError> {
        let mut g = self.lock();
        let Some(s) = g.standbys.get_mut(role) else {
            return Ok(None);
        };
        if s.attached {
            return Err(AttachError::Busy);
        }
        *s = Standby {
            attached: true,
            received: start_lsn,
            durable: start_lsn,
        };
        self.after_progress(&mut g);
        Ok(Some(StandbyHandle {
            coordinator: Arc::clone(self),
            role: role.to_string(),
        }))
    }

    fn position(&self, s: &Standby) -> u64 {
        match self.stage {
            SyncCommitStage::RemoteWrite => s.received.max(s.durable),
            SyncCommitStage::On | SyncCommitStage::RemoteApply => s.durable,
        }
    }

    fn satisfied(&self, g: &State, lsn: u64) -> bool {
        g.standbys
            .values()
            .all(|s| s.attached && self.position(s) >= lsn)
    }

    /// 位置が進んだ後の共通処理（縮退の自動解除と待機側の起床）。
    fn after_progress(&self, g: &mut State) {
        if g.degraded && self.satisfied(g, g.max_required) {
            g.degraded = false;
            crate::log_stderr!("engine: synchronous replication restored after degradation");
        }
        self.changed.notify_all();
    }

    /// `lsn` までの確認を待つ。呼び出し元は書き込みゲートの permit を持っていない契約。`interrupted` は
    /// 約 100ms ごとに確認する。期限は `Instant::checked_add` で計算する。
    pub fn wait(&self, lsn: u64, interrupted: impl Fn() -> bool) -> SyncWaitOutcome {
        let deadline = self.timeout.and_then(|t| Instant::now().checked_add(t));
        let mut g = self.lock();
        g.max_required = g.max_required.max(lsn);
        if self.satisfied(&g, lsn) {
            return SyncWaitOutcome::Confirmed;
        }
        if g.degraded {
            return SyncWaitOutcome::Degraded;
        }
        g.wake_seq = g.wake_seq.wrapping_add(1);
        self.wake.notify_all();
        loop {
            if self.satisfied(&g, lsn) {
                return SyncWaitOutcome::Confirmed;
            }
            if g.degraded {
                return SyncWaitOutcome::Degraded;
            }
            if interrupted() {
                return SyncWaitOutcome::Interrupted;
            }
            let step = match deadline {
                Some(d) => {
                    let rem = d.saturating_duration_since(Instant::now());
                    if rem.is_zero() {
                        return match self.on_timeout {
                            SyncTimeoutAction::Reject => SyncWaitOutcome::TimedOut,
                            SyncTimeoutAction::Degrade => {
                                g.degraded = true;
                                crate::log_stderr!(
                                    "engine: synchronous replication degraded after a timeout"
                                );
                                SyncWaitOutcome::Degraded
                            }
                        };
                    }
                    rem.min(WAIT_STEP)
                }
                None => WAIT_STEP,
            };
            g = self
                .changed
                .wait_timeout(g, step)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
    }

    /// 送信側が commit の待機開始を検知するための連番。ポーリングの前に取る。
    pub fn wake_seq(&self) -> u64 {
        self.lock().wake_seq
    }

    /// 連番が `seen` から進む（待機の開始）か `max` が経つまで待つ。送信側が追いついている間の
    /// `sleep` の代わりに使い、同期 commit の遅延を下げる。
    pub fn park_until_commit_waiting(&self, seen: u64, max: Duration) {
        let g = self.lock();
        if g.wake_seq != seen {
            return;
        }
        let _ = self
            .wake
            .wait_timeout_while(g, max, |s| s.wake_seq == seen)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
    }

    /// 縮退中か（テスト・運用の観測用）。
    pub fn is_degraded(&self) -> bool {
        self.lock().degraded
    }
}

impl StandbyHandle {
    fn update(&self, f: impl FnOnce(&mut Standby)) {
        let c = &self.coordinator;
        let mut g = c.lock();
        if let Some(s) = g.standbys.get_mut(&self.role) {
            f(s);
        }
        c.after_progress(&mut g);
    }

    /// レプリカが受け取った最後の LSN（`R`）を報告する。単調でなければ無視する。
    pub fn report_received(&self, lsn: u64) {
        self.update(|s| s.received = s.received.max(lsn));
    }

    /// レプリカが永続化した最後の LSN（`K`）を報告する（受信も含意する）。単調でなければ無視する。
    pub fn report_durable(&self, lsn: u64) {
        self.update(|s| {
            s.durable = s.durable.max(lsn);
            s.received = s.received.max(lsn);
        });
    }
}

impl Drop for StandbyHandle {
    fn drop(&mut self) {
        let c = &self.coordinator;
        let mut g = c.lock();
        if let Some(s) = g.standbys.get_mut(&self.role) {
            *s = Standby::default();
        }
        c.changed.notify_all();
    }
}

#[cfg(test)]
mod tests;
