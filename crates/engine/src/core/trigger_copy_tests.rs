//! `COPY ... FROM STDIN` の確定処理でのトリガ発火の結合テスト（TABLE-45・SQL-32・RECOVER-1。Issue #3281）。
//!
//! 定義をカタログへ直接注入し、呼び出し口を差し替えた invoker が `TG_WHEN`・`TG_LEVEL`・`TG_OP` を
//! 記録する。COPY の発火列が同じ行数の複数行 `INSERT` と一致すること、再送でトリガを呼ばないこと、
//! AFTER ROW の失敗で本表・台帳が残らないこと、セッションなしの入口と NoSQL 表層が `0A000` のまま
//! 副作用を残さないことを固定する。本番 invoker での結果・越境は
//! `crates/engine/tests/table_rls/table45_trigger_copy_from.rs` が担う。

use std::sync::{Arc, Mutex};

use super::trigger_insert_tests::{add_trigger, core_with, count, ctx, run, trigger_def, Behavior};
use super::{CopyPlan, EngineCore};
use crate::catalog::trigger::{TriggerLevel, TriggerTiming};
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;
use crate::test_util::temp_db::tempdir;
use crate::trigger::call::TriggerOutcome;

type Log = Arc<Mutex<Vec<String>>>;

/// 呼び出しを `TG_WHEN TG_LEVEL TG_OP` の形で記録し、行は変えない。`fail_after_row` が真なら AFTER ROW で失敗する。
fn logging(log: &Log, fail_after_row: bool) -> Behavior {
    let log = Arc::clone(log);
    Box::new(move |c| {
        log.lock()
            .expect("log")
            .push(format!("{} {} {}", c.tg_when(), c.tg_level(), c.tg_op()));
        if fail_after_row && c.tg_when() == "AFTER" && c.tg_level() == "ROW" {
            return Err(SqlSurfaceError::invalid_input("injected trigger failure"));
        }
        Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
    })
}

fn take(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.lock().expect("log"))
}

fn four_triggers(core: &EngineCore) {
    for (name, timing, level) in [
        ("a_bs", TriggerTiming::Before, TriggerLevel::Statement),
        ("b_br", TriggerTiming::Before, TriggerLevel::Row),
        ("c_ar", TriggerTiming::After, TriggerLevel::Row),
        ("d_as", TriggerTiming::After, TriggerLevel::Statement),
    ] {
        add_trigger(
            &core.storage,
            "tenant-a",
            "trg",
            &trigger_def(name, timing, level),
        );
    }
}

/// `trg (id, embedding, a, n)` へ 3 行を流す COPY を、セッション入口で確定する。
fn copy_three(
    core: &EngineCore,
    first_id: u32,
    op: &str,
) -> Result<crate::sql::exec::InsertOutcome, SqlSurfaceError> {
    let c = ctx("tenant-a");
    let mut session = SessionState::default();
    let sql = format!("COPY trg (id, embedding, a, n) FROM STDIN USING OPERATION_ID '{op}'");
    let CopyPlan::From(mut cs) = core.begin_copy(&c, &session, &sql)? else {
        panic!("from plan");
    };
    let data: String = (0..3)
        .map(|i| format!("{}\t[1.0,0.0]\tx\t{}\n", first_id + i, 10 + i))
        .collect();
    cs.feed(data.as_bytes())?;
    let batch = cs.finish()?;
    let mut txn = core.new_session_transaction();
    core.commit_copy_in_txn_in_session(&c, &mut session, &mut txn, batch)
}

// COPY の発火列は、同じ行数の複数行 INSERT の発火列と完全に一致する。
#[test]
fn copy_firing_sequence_equals_multi_row_insert() {
    let dir = tempdir();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let (core, _calls) = core_with(dir.path(), logging(&log, false));
    four_triggers(&core);
    run(
        &core,
        &ctx("tenant-a"),
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 10), \
         (2, '[1.0,0.0]', 'x', 11), (3, '[1.0,0.0]', 'x', 12)",
    )
    .expect("insert");
    let by_insert = take(&log);
    assert_eq!(by_insert.len(), 8, "{by_insert:?}");
    let out = copy_three(&core, 11, "op-order").expect("copy");
    assert_eq!(out.rows_affected, 3);
    assert_eq!(take(&log), by_insert);
    assert_eq!(count(&core, &ctx("tenant-a"), "trg"), 6);
}

// 同じ operation_id の再送は `23505` で、トリガを再び呼ばない。
#[test]
fn copy_resend_does_not_refire() {
    let dir = tempdir();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let (core, calls) = core_with(dir.path(), logging(&log, false));
    four_triggers(&core);
    copy_three(&core, 1, "op-resend").expect("first");
    let fired = calls.load(std::sync::atomic::Ordering::SeqCst);
    assert!(fired > 0);
    let e = copy_three(&core, 1, "op-resend").expect_err("resend");
    assert_eq!(e.wire_code(), "23505");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), fired);
}

// AFTER ROW の失敗で COPY 全体が失敗し、本表の行も台帳も残らない（同じ operation_id の再送が成功する）。
#[test]
fn copy_after_row_failure_leaves_no_rows_or_ledger() {
    let dir = tempdir();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let flag = Arc::clone(&fail);
    let inner = logging(&log, false);
    let behavior: Behavior = Box::new(move |c| {
        if flag.load(std::sync::atomic::Ordering::SeqCst)
            && c.tg_when() == "AFTER"
            && c.tg_level() == "ROW"
        {
            return Err(SqlSurfaceError::invalid_input("injected trigger failure"));
        }
        inner(c)
    });
    let (core, _calls) = core_with(dir.path(), behavior);
    four_triggers(&core);
    copy_three(&core, 1, "op-fail").expect_err("injected failure");
    assert_eq!(count(&core, &ctx("tenant-a"), "trg"), 0);
    fail.store(false, std::sync::atomic::Ordering::SeqCst);
    copy_three(&core, 1, "op-fail").expect("resend after failure succeeds");
    assert_eq!(count(&core, &ctx("tenant-a"), "trg"), 3);
}

// セッションなしの公開入口は、トリガのある表を副作用の前に `0A000` で拒否する（fail-closed）。
//
// intentional: docs/design/trigger-execution-model.md
// 主体の写しを張れない入口で本番の invoker を呼ばない。トリガを発火させる COPY はセッション入口が担う。
#[test]
fn sessionless_copy_entry_rejects_trigger_tables() {
    let dir = tempdir();
    let log: Log = Arc::new(Mutex::new(Vec::new()));
    let (core, calls) = core_with(dir.path(), logging(&log, false));
    four_triggers(&core);
    let c = ctx("tenant-a");
    let session = SessionState::default();
    let CopyPlan::From(mut cs) = core
        .begin_copy(&c, &session, "COPY trg (id, embedding, a, n) FROM STDIN")
        .expect("plan")
    else {
        panic!("from plan");
    };
    cs.feed(b"1\t[1.0,0.0]\tx\t10\n").expect("feed");
    let batch = cs.finish().expect("finish");
    let e = core.commit_copy_in(&c, batch).expect_err("0A000");
    assert_eq!(e.wire_code(), "0A000");
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(count(&core, &ctx("tenant-a"), "trg"), 0);
}
