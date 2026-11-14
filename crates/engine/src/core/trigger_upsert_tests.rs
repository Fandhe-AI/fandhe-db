//! `INSERT ... ON CONFLICT`（UPSERT）でのトリガ発火の結合テスト（TABLE-45・Issue #3191）。
//!
//! トリガ関数の実体を注入 invoker に差し替え、呼び出しを `TG_WHEN TG_LEVEL TG_OP` と OLD／NEW の `n`
//! で記録する。競合なし・`DO UPDATE`・`DO NOTHING` の分岐ごとの発火、`BEFORE INSERT` の NEW 書き換えが
//! `EXCLUDED` に効くこと、`NULL` 返却、文トリガの順序、再送でトリガを呼ばないこと、テナント境界、
//! 未結線経路（`RETURNING`）の `0A000` を `EngineCore` に SQL を流す水準で固定する。
//! 設計記録は `docs/design/trigger-execution-model.md` の UPSERT 節。

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use super::trigger_insert_tests::{
    add_trigger, core_with, count, ctx, read_rows, run, trigger_def,
};
use super::EngineCore;
use crate::catalog::trigger::{TriggerDef, TriggerEvents, TriggerLevel, TriggerTiming};
use crate::row_codec::Value;
use crate::trigger::call::TriggerOutcome;

const INS: u8 = TriggerEvents::INSERT;
const UPD: u8 = TriggerEvents::UPDATE;

type Log = Arc<Mutex<Vec<String>>>;

fn trig(name: &str, timing: TriggerTiming, level: TriggerLevel, events: u8) -> TriggerDef {
    let mut def = trigger_def(name, timing, level);
    def.events = TriggerEvents::from_bits(events).expect("events");
    def
}

fn n_of(row: &[Value]) -> String {
    match row.get(2) {
        Some(Value::BigInt(n)) => n.to_string(),
        _ => "-".to_string(),
    }
}

/// 呼び出しを `TG_WHEN TG_LEVEL TG_OP [old=<n>] [new=<n>]` の形で記録する。`n_rewrite` が `Some(d)` なら
/// BEFORE ROW INSERT で NEW の `n` に `d` を足す。`skip_n` と一致する NEW の `n` は BEFORE ROW で `NULL` 返却。
fn behavior(
    log: &Log,
    n_rewrite: Option<i64>,
    skip_n: Option<i64>,
) -> super::trigger_insert_tests::Behavior {
    let log = Arc::clone(log);
    Box::new(move |c| {
        let mut entry = format!("{} {} {}", c.tg_when(), c.tg_level(), c.tg_op());
        if let Some(old) = c.old_row {
            entry.push_str(&format!(" old={}", n_of(old)));
        }
        if !c.new_row.is_empty() {
            entry.push_str(&format!(" new={}", n_of(c.new_row)));
        }
        log.lock().expect("log").push(entry);
        let mut row = c.new_row.to_vec();
        if c.tg_when() == "BEFORE" && c.tg_level() == "ROW" {
            if let (Some(d), "INSERT") = (n_rewrite, c.tg_op()) {
                if let Some(Value::BigInt(n)) = row.get_mut(2) {
                    *n += d;
                }
            }
            if let (Some(s), Some(Value::BigInt(n))) = (skip_n, row.get(2)) {
                if *n == s {
                    return Ok(TriggerOutcome::Skip);
                }
            }
        }
        Ok(TriggerOutcome::Proceed(row))
    })
}

fn take(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.lock().expect("log"))
}

fn new_log() -> Log {
    Arc::new(Mutex::new(Vec::new()))
}

fn code(core: &EngineCore, c: &crate::policy::PolicyContext, sql: &str) -> &'static str {
    run(core, c, sql).expect_err(sql).wire_code()
}

fn seed(core: &EngineCore) {
    run(
        core,
        &ctx("tenant-a"),
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 10)",
    )
    .expect("seed");
}

fn all_triggers(core: &EngineCore, ins_events: u8, upd_events: u8) {
    for (name, timing, level, events) in [
        (
            "a_bs",
            TriggerTiming::Before,
            TriggerLevel::Statement,
            ins_events,
        ),
        ("b_br", TriggerTiming::Before, TriggerLevel::Row, ins_events),
        ("c_ar", TriggerTiming::After, TriggerLevel::Row, ins_events),
        (
            "d_as",
            TriggerTiming::After,
            TriggerLevel::Statement,
            ins_events,
        ),
        (
            "e_bs",
            TriggerTiming::Before,
            TriggerLevel::Statement,
            upd_events,
        ),
        ("f_br", TriggerTiming::Before, TriggerLevel::Row, upd_events),
        ("g_ar", TriggerTiming::After, TriggerLevel::Row, upd_events),
        (
            "h_as",
            TriggerTiming::After,
            TriggerLevel::Statement,
            upd_events,
        ),
    ] {
        if events == 0 {
            continue;
        }
        add_trigger(
            &core.storage,
            "tenant-a",
            "trg",
            &trig(name, timing, level, events),
        );
    }
}

const UPSERT_UPDATE: &str = "INSERT INTO trg (id, embedding, a, n) VALUES \
    (1, '[9.0,0.0]', 'x', 1), (2, '[2.0,0.0]', 'y', 2) \
    ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n";

// 競合なし: 行数分の BEFORE／AFTER ROW INSERT と、文トリガが 1 回ずつ（挿入側）。
#[test]
fn insert_side_fires_row_and_statement_triggers() {
    let dir = crate::test_util::temp_db::tempdir();
    let log = new_log();
    let (core, _calls) = core_with(dir.path(), behavior(&log, None, None));
    let a = ctx("tenant-a");
    all_triggers(&core, INS, 0);
    run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1), \
         (2, '[2.0,0.0]', 'y', 2) ON CONFLICT (id) DO NOTHING",
    )
    .expect("upsert");
    assert_eq!(
        take(&log),
        vec![
            "BEFORE STATEMENT INSERT",
            "BEFORE ROW INSERT new=1",
            "BEFORE ROW INSERT new=2",
            "AFTER ROW INSERT new=1",
            "AFTER ROW INSERT new=2",
            "AFTER STATEMENT INSERT",
        ]
    );
    assert_eq!(count(&core, &a, "trg"), 2);
}

// DO UPDATE: 競合行は BEFORE ROW INSERT → BEFORE ROW UPDATE → AFTER ROW UPDATE（AFTER ROW INSERT は無し）、
// 競合なし行は INSERT 側が発火する。文トリガは BEFORE INSERT → BEFORE UPDATE、AFTER UPDATE → AFTER INSERT。
#[test]
fn do_update_fires_update_triggers_for_conflicting_rows() {
    let dir = crate::test_util::temp_db::tempdir();
    let log = new_log();
    let (core, _calls) = core_with(dir.path(), behavior(&log, None, None));
    let a = ctx("tenant-a");
    seed(&core);
    all_triggers(&core, INS, UPD);
    take(&log);
    let out = run(&core, &a, UPSERT_UPDATE).expect("upsert");
    assert_eq!(super::trigger_insert_tests::rows_affected(out), 2);
    assert_eq!(
        take(&log),
        vec![
            "BEFORE STATEMENT INSERT",
            "BEFORE STATEMENT UPDATE",
            "BEFORE ROW INSERT new=1",
            "BEFORE ROW UPDATE old=10 new=1",
            "BEFORE ROW INSERT new=2",
            "AFTER ROW UPDATE old=10 new=1",
            "AFTER ROW INSERT new=2",
            "AFTER STATEMENT UPDATE",
            "AFTER STATEMENT INSERT",
        ]
    );
    assert_eq!(
        read_rows(&core, &a, "trg"),
        vec![(1, "x".to_string(), 1), (2, "y".to_string(), 2)]
    );
}

// DO NOTHING: 競合行は BEFORE ROW INSERT のみで AFTER は呼ばない。UPDATE トリガは一切呼ばれない。
#[test]
fn do_nothing_conflict_fires_only_before_insert() {
    let dir = crate::test_util::temp_db::tempdir();
    let log = new_log();
    let (core, _calls) = core_with(dir.path(), behavior(&log, None, None));
    let a = ctx("tenant-a");
    seed(&core);
    all_triggers(&core, INS, UPD);
    take(&log);
    let out = run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 5), \
         (2, '[2.0,0.0]', 'y', 6) ON CONFLICT (id) DO NOTHING",
    )
    .expect("upsert");
    assert_eq!(super::trigger_insert_tests::rows_affected(out), 1);
    assert_eq!(
        take(&log),
        vec![
            "BEFORE STATEMENT INSERT",
            "BEFORE ROW INSERT new=5",
            "BEFORE ROW INSERT new=6",
            "AFTER ROW INSERT new=6",
            "AFTER STATEMENT INSERT",
        ]
    );
    assert_eq!(
        read_rows(&core, &a, "trg"),
        vec![(1, "x".to_string(), 10), (2, "y".to_string(), 6)]
    );
}

// UPDATE トリガしか無い表への DO NOTHING は UPDATE を行わないので、従来どおり成功し何も呼ばない。
#[test]
fn do_nothing_ignores_update_only_triggers() {
    let dir = crate::test_util::temp_db::tempdir();
    let log = new_log();
    let (core, calls) = core_with(dir.path(), behavior(&log, None, None));
    let a = ctx("tenant-a");
    seed(&core);
    all_triggers(&core, 0, UPD);
    run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 5) \
         ON CONFLICT (id) DO NOTHING",
    )
    .expect("upsert");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(count(&core, &a, "trg"), 1);
}

// BEFORE INSERT の NEW の書き換えが `EXCLUDED` に効く（SET n = EXCLUDED.n が書き換え後の値になる）。
#[test]
fn before_insert_rewrite_is_visible_through_excluded() {
    let dir = crate::test_util::temp_db::tempdir();
    let log = new_log();
    let (core, _calls) = core_with(dir.path(), behavior(&log, Some(100), None));
    let a = ctx("tenant-a");
    seed(&core);
    all_triggers(&core, INS, 0);
    // `seed` は BEFORE INSERT が付く前に入れたので n = 10。UPSERT では挿入側 +100 が EXCLUDED に載る。
    run(&core, &a, UPSERT_UPDATE).expect("upsert");
    assert_eq!(
        read_rows(&core, &a, "trg"),
        vec![(1, "x".to_string(), 101), (2, "y".to_string(), 102)]
    );
}

// BEFORE の `NULL` 返却: INSERT 側はその行を書かず AFTER も無し、UPDATE 側は更新せず AFTER も無し。
#[test]
fn before_null_skips_insert_and_update() {
    let dir = crate::test_util::temp_db::tempdir();
    let log = new_log();
    // 競合なしの行（n = 2）の INSERT をスキップする。
    let (core, _calls) = core_with(dir.path(), behavior(&log, None, Some(2)));
    let a = ctx("tenant-a");
    seed(&core);
    all_triggers(&core, INS, UPD);
    take(&log);
    let out = run(&core, &a, UPSERT_UPDATE).expect("upsert");
    assert_eq!(super::trigger_insert_tests::rows_affected(out), 1);
    assert_eq!(read_rows(&core, &a, "trg"), vec![(1, "x".to_string(), 1)]);
    let entries = take(&log);
    assert!(
        !entries.iter().any(|e| e == "AFTER ROW INSERT new=2"),
        "{entries:?}"
    );
    assert!(
        entries.iter().any(|e| e == "AFTER ROW UPDATE old=10 new=1"),
        "{entries:?}"
    );

    // UPDATE 側の NULL 返却（SET 後の n = 1 をスキップ）。行は更新されない。
    let dir2 = crate::test_util::temp_db::tempdir();
    let log2 = new_log();
    let (core2, _c2) = core_with(dir2.path(), behavior(&log2, None, Some(1)));
    seed(&core2);
    all_triggers(&core2, 0, UPD);
    take(&log2);
    let out = run(
        &core2,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[9.0,0.0]', 'x', 1) \
         ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n",
    )
    .expect("upsert");
    assert_eq!(super::trigger_insert_tests::rows_affected(out), 0);
    assert_eq!(read_rows(&core2, &a, "trg"), vec![(1, "x".to_string(), 10)]);
    assert!(!take(&log2).iter().any(|e| e.starts_with("AFTER ROW")));
}

// DO UPDATE ... WHERE が偽の行は UPDATE トリガを呼ばない（BEFORE ROW INSERT だけ）。
#[test]
fn do_update_where_false_does_not_fire_update_triggers() {
    let dir = crate::test_util::temp_db::tempdir();
    let log = new_log();
    let (core, _calls) = core_with(dir.path(), behavior(&log, None, None));
    let a = ctx("tenant-a");
    seed(&core);
    all_triggers(&core, INS, UPD);
    take(&log);
    let out = run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[9.0,0.0]', 'x', 1) \
         ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n WHERE trg.n > 1000",
    )
    .expect("upsert");
    assert_eq!(super::trigger_insert_tests::rows_affected(out), 0);
    let entries = take(&log);
    assert!(
        entries.iter().any(|e| e == "BEFORE ROW INSERT new=1"),
        "{entries:?}"
    );
    assert!(
        !entries.iter().any(|e| e.contains("ROW UPDATE")),
        "{entries:?}"
    );
    assert_eq!(read_rows(&core, &a, "trg"), vec![(1, "x".to_string(), 10)]);
}

// 再送冪等: 同じ operation_id の再送は `23505` で、行・トリガ呼び出し回数が増えない（挿入側・更新側・
// 全行スキップの各経路）。
#[test]
fn replay_does_not_refire_triggers() {
    for (seeded, sql) in [
        (
            false,
            "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
             ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'r1'",
        ),
        (
            true,
            "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
             ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n USING OPERATION_ID 'r2'",
        ),
        (
            true,
            "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
             ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'r3'",
        ),
    ] {
        let dir = crate::test_util::temp_db::tempdir();
        let log = new_log();
        let (core, calls) = core_with(dir.path(), behavior(&log, None, None));
        let a = ctx("tenant-a");
        if seeded {
            seed(&core);
        }
        all_triggers(&core, INS, UPD);
        run(&core, &a, sql).expect(sql);
        let after_first = calls.load(Ordering::SeqCst);
        let rows = read_rows(&core, &a, "trg");
        assert_eq!(code(&core, &a, sql), "23505", "{sql}");
        assert_eq!(calls.load(Ordering::SeqCst), after_first, "{sql}");
        assert_eq!(read_rows(&core, &a, "trg"), rows, "{sql}");
    }
}

// テナント境界: 他テナントのトリガは発火せず、他テナントの行は衝突先として見えない。
#[test]
fn other_tenants_triggers_and_rows_do_not_interfere() {
    let dir = crate::test_util::temp_db::tempdir();
    let log = new_log();
    let (core, calls) = core_with(dir.path(), behavior(&log, None, None));
    let a = ctx("tenant-a");
    let b = ctx("tenant-b");
    all_triggers(&core, INS, UPD);
    // tenant-b の書き込みでは tenant-a のトリガは呼ばれず、tenant-a の行も存在しない。
    run(
        &core,
        &b,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 5) \
         ON CONFLICT (id) DO UPDATE SET n = EXCLUDED.n",
    )
    .expect("tenant-b upsert");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(count(&core, &b, "trg"), 1);
    // tenant-a が同じ id を UPSERT しても、tenant-b の行は衝突先にならず挿入になる。
    let out = run(&core, &a, UPSERT_UPDATE).expect("tenant-a upsert");
    assert_eq!(super::trigger_insert_tests::rows_affected(out), 2);
    let entries = take(&log);
    assert!(
        !entries.iter().any(|e| e.contains("ROW UPDATE")),
        "{entries:?}"
    );
    assert_eq!(read_rows(&core, &b, "trg"), vec![(1, "x".to_string(), 5)]);
}

// 未結線経路: RETURNING 付き UPSERT はトリガのある表で `0A000`（台帳は消費されない）。
#[test]
fn returning_upsert_stays_guarded() {
    let dir = crate::test_util::temp_db::tempdir();
    let log = new_log();
    let (core, calls) = core_with(dir.path(), behavior(&log, None, None));
    let a = ctx("tenant-a");
    all_triggers(&core, INS, UPD);
    let sql = "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
               ON CONFLICT (id) DO NOTHING RETURNING id USING OPERATION_ID 'g1'";
    assert_eq!(code(&core, &a, sql), "0A000");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'g1'",
    )
    .expect("ledger not consumed");
    assert_eq!(count(&core, &a, "trg"), 1);
}
