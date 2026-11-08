//! [`super`] の単体テスト（RECOVER-15 ポインタ。実時間は数十 ms の短い上限だけを使う）。

use super::*;

fn coord(
    standbys: &[&str],
    stage: SyncCommitStage,
    timeout: Option<Duration>,
    on_timeout: SyncTimeoutAction,
) -> Arc<SyncCommitCoordinator> {
    Arc::new(SyncCommitCoordinator::new(SyncCommitConfig {
        standbys: standbys.iter().map(|s| (*s).to_string()).collect(),
        stage,
        timeout,
        on_timeout,
    }))
}

const SHORT: Option<Duration> = Some(Duration::from_millis(40));
const NEVER: fn() -> bool = || false;

#[test]
fn all_roles_must_confirm() {
    let c = coord(
        &["a", "b"],
        SyncCommitStage::On,
        SHORT,
        SyncTimeoutAction::Reject,
    );
    let a = c.attach_standby("a", 0).expect("a").expect("tracked");
    let b = c.attach_standby("b", 0).expect("b").expect("tracked");
    a.report_durable(5);
    assert_eq!(c.wait(5, NEVER), SyncWaitOutcome::TimedOut);
    b.report_durable(5);
    assert_eq!(c.wait(5, NEVER), SyncWaitOutcome::Confirmed);
}

#[test]
fn stage_decides_which_report_counts() {
    let c = coord(
        &["a"],
        SyncCommitStage::On,
        SHORT,
        SyncTimeoutAction::Reject,
    );
    let a = c.attach_standby("a", 0).expect("a").expect("tracked");
    a.report_received(3);
    assert_eq!(c.wait(3, NEVER), SyncWaitOutcome::TimedOut);
    a.report_durable(3);
    assert_eq!(c.wait(3, NEVER), SyncWaitOutcome::Confirmed);

    let w = coord(
        &["a"],
        SyncCommitStage::RemoteWrite,
        SHORT,
        SyncTimeoutAction::Reject,
    );
    let a = w.attach_standby("a", 0).expect("a").expect("tracked");
    a.report_received(3);
    assert_eq!(w.wait(3, NEVER), SyncWaitOutcome::Confirmed);

    let r = coord(
        &["a"],
        SyncCommitStage::RemoteApply,
        SHORT,
        SyncTimeoutAction::Reject,
    );
    let a = r.attach_standby("a", 0).expect("a").expect("tracked");
    a.report_received(3);
    assert_eq!(r.wait(3, NEVER), SyncWaitOutcome::TimedOut);
    a.report_durable(3);
    assert_eq!(r.wait(3, NEVER), SyncWaitOutcome::Confirmed);
}

#[test]
fn start_position_counts_and_reports_are_monotonic() {
    let c = coord(
        &["a"],
        SyncCommitStage::On,
        SHORT,
        SyncTimeoutAction::Reject,
    );
    let a = c.attach_standby("a", 7).expect("a").expect("tracked");
    assert_eq!(c.wait(7, NEVER), SyncWaitOutcome::Confirmed);
    a.report_durable(9);
    a.report_durable(2);
    assert_eq!(c.wait(9, NEVER), SyncWaitOutcome::Confirmed);
}

#[test]
fn second_connection_is_busy_and_unlisted_roles_are_untracked() {
    let c = coord(
        &["a"],
        SyncCommitStage::On,
        SHORT,
        SyncTimeoutAction::Reject,
    );
    let first = c.attach_standby("a", 0).expect("first");
    assert!(first.is_some());
    assert_eq!(
        c.attach_standby("a", 0).expect_err("busy"),
        AttachError::Busy
    );
    assert!(c.attach_standby("other", 0).expect("untracked").is_none());
    drop(first);
    assert!(c.attach_standby("a", 0).expect("reattach").is_some());
}

#[test]
fn detach_removes_the_position_but_not_other_roles() {
    let c = coord(
        &["a", "b"],
        SyncCommitStage::On,
        SHORT,
        SyncTimeoutAction::Reject,
    );
    let a = c.attach_standby("a", 0).expect("a").expect("tracked");
    let b = c.attach_standby("b", 0).expect("b").expect("tracked");
    a.report_durable(4);
    b.report_durable(4);
    assert_eq!(c.wait(4, NEVER), SyncWaitOutcome::Confirmed);
    drop(a);
    assert_eq!(c.wait(5, NEVER), SyncWaitOutcome::TimedOut);
    b.report_durable(5);
    assert_eq!(c.wait(5, NEVER), SyncWaitOutcome::TimedOut, "a is gone");
    let a2 = c.attach_standby("a", 5).expect("a2").expect("tracked");
    assert_eq!(c.wait(5, NEVER), SyncWaitOutcome::Confirmed);
    drop(a2);
}

#[test]
fn degrade_passes_without_waiting_and_recovers_when_caught_up() {
    let c = coord(
        &["a"],
        SyncCommitStage::On,
        SHORT,
        SyncTimeoutAction::Degrade,
    );
    let a = c.attach_standby("a", 0).expect("a").expect("tracked");
    assert_eq!(c.wait(3, NEVER), SyncWaitOutcome::Degraded);
    assert!(c.is_degraded());
    let t = Instant::now();
    assert_eq!(c.wait(4, NEVER), SyncWaitOutcome::Degraded);
    assert!(
        t.elapsed() < Duration::from_millis(30),
        "degraded does not wait"
    );
    a.report_durable(3);
    assert!(c.is_degraded(), "max required is 4");
    a.report_durable(4);
    assert!(!c.is_degraded());
}

#[test]
fn report_wakes_a_waiter() {
    let c = coord(&["a"], SyncCommitStage::On, None, SyncTimeoutAction::Reject);
    let a = c.attach_standby("a", 0).expect("a").expect("tracked");
    let c2 = Arc::clone(&c);
    let t = std::thread::spawn(move || c2.wait(2, NEVER));
    std::thread::sleep(Duration::from_millis(30));
    a.report_durable(2);
    assert_eq!(t.join().expect("join"), SyncWaitOutcome::Confirmed);
}

#[test]
fn interrupt_stops_an_unbounded_wait() {
    let c = coord(&["a"], SyncCommitStage::On, None, SyncTimeoutAction::Reject);
    let _a = c.attach_standby("a", 0).expect("a").expect("tracked");
    let flag = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|s| {
        let h = s.spawn(|| c.wait(1, || flag.load(std::sync::atomic::Ordering::Acquire)));
        std::thread::sleep(Duration::from_millis(30));
        flag.store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(h.join().expect("join"), SyncWaitOutcome::Interrupted);
    });
}

#[test]
fn waiting_bumps_the_wake_sequence() {
    let c = coord(
        &["a"],
        SyncCommitStage::On,
        SHORT,
        SyncTimeoutAction::Reject,
    );
    let _a = c.attach_standby("a", 0).expect("a").expect("tracked");
    let seen = c.wake_seq();
    let t = Instant::now();
    assert_eq!(c.wait(1, NEVER), SyncWaitOutcome::TimedOut);
    assert_ne!(c.wake_seq(), seen);
    // 連番が進んでいれば park はすぐ戻る。
    let t2 = Instant::now();
    c.park_until_commit_waiting(seen, Duration::from_secs(5));
    assert!(t2.elapsed() < Duration::from_secs(1));
    assert!(t.elapsed() >= Duration::from_millis(40));
}
