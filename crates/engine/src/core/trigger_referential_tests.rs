//! FK の参照アクションが書き換える子表の ROW トリガの発火の結合テスト（TABLE-45・SQL-32・RLS-10・
//! Issue #3284）。
//!
//! 定義をカタログへ直接注入し、呼び出し口を差し替えた invoker が発火を記録する（`trigger_update_delete_tests`
//! と同じ流儀）。子表の BEFORE／AFTER の発火・イベント種別・書き換え・`NULL` 返却（`23503`）・自己参照・
//! 孫段・AFTER の順序・深さ上限・未結線経路の拒否・テナント境界を `EngineCore` に SQL を流す水準で固定する。
//! 設計記録は `docs/design/trigger-execution-model.md`（D8）と `trigger-update-delete.md`。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::trigger_insert_tests::{add_trigger, core_with, ctx, run, trigger_def, Behavior};
use super::EngineCore;
use crate::catalog::trigger::{TriggerDef, TriggerEvents, TriggerLevel, TriggerTiming};
use crate::row_codec::Value;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::test_util::temp_db::tempdir;
use crate::trigger::call::TriggerOutcome;

const UPD: u8 = TriggerEvents::UPDATE;
const DEL: u8 = TriggerEvents::DELETE;

type Log = Arc<Mutex<Vec<String>>>;

fn def(name: &str, timing: TriggerTiming, level: TriggerLevel, events: u8) -> TriggerDef {
    let mut d = trigger_def(name, timing, level);
    d.events = TriggerEvents::from_bits(events).expect("events");
    d
}

fn row(name: &str, timing: TriggerTiming, events: u8) -> TriggerDef {
    def(name, timing, TriggerLevel::Row, events)
}

/// 発火を `WHEN OP table` で記録し、行は変えない。
fn recording(log: &Log) -> Behavior {
    let log = Arc::clone(log);
    Box::new(move |c| {
        log.lock()
            .expect("log")
            .push(format!("{} {} {}", c.tg_when(), c.tg_op(), c.table));
        Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
    })
}

fn take(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.lock().expect("log"))
}

fn ddl(core: &EngineCore, sqls: &[&str]) {
    let a = ctx("tenant-a");
    let mut s = SessionState::default();
    s.allow_ddl();
    for sql in sqls {
        core.execute_sql_in_session(&a, &mut s, sql).expect(sql);
    }
}

fn rows_of(core: &EngineCore, tenant: &str, sql: &str) -> usize {
    match run(core, &ctx(tenant), sql).expect(sql) {
        SqlOutcome::Query(q) => q.rows.len(),
        _ => panic!("expected query"),
    }
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> &'static str {
    run(core, &ctx(tenant), sql).expect_err(sql).wire_code()
}

/// 親 `par(k)`（k = 1, 2）と子 `ch(v, parent_id)`（CASCADE。(a,1)・(b,1)・(c,2)）を持つコア。
fn family(dir: &std::path::Path, behavior: Behavior) -> (EngineCore, Arc<AtomicUsize>) {
    let (core, calls) = core_with(dir, behavior);
    ddl(
        &core,
        &[
            "CREATE TABLE par (k BIGINT PRIMARY KEY)",
            "CREATE TABLE ch (v TEXT, parent_id BIGINT REFERENCES par ON DELETE CASCADE ON UPDATE CASCADE)",
        ],
    );
    seed(&core, "tenant-a");
    (core, calls)
}

fn seed(core: &EngineCore, tenant: &str) {
    run(core, &ctx(tenant), "INSERT INTO par (k) VALUES (1), (2)").expect("par");
    run(
        core,
        &ctx(tenant),
        "INSERT INTO ch (v, parent_id) VALUES ('a', 1), ('b', 1), ('c', 2)",
    )
    .expect("ch");
}

// ON DELETE CASCADE: 親にトリガが無くても、消える子行ごとに子表の DELETE トリガが発火する。
#[test]
fn delete_cascade_fires_child_delete_triggers_per_row() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let (core, _calls) = family(dir.path(), recording(&log));
    add_trigger(
        &core.storage,
        "tenant-a",
        "ch",
        &row("t_a", TriggerTiming::After, DEL),
    );
    add_trigger(
        &core.storage,
        "tenant-a",
        "ch",
        &row("t_b", TriggerTiming::Before, DEL),
    );
    run(&core, &ctx("tenant-a"), "DELETE FROM par WHERE k = 1").expect("delete");
    let mut got = take(&log);
    got.sort();
    assert_eq!(
        got,
        vec![
            "AFTER DELETE ch",
            "AFTER DELETE ch",
            "BEFORE DELETE ch",
            "BEFORE DELETE ch"
        ]
    );
    assert_eq!(rows_of(&core, "tenant-a", "SELECT v FROM ch"), 1);
    assert_eq!(rows_of(&core, "tenant-a", "SELECT k FROM par"), 1);
}

// ON UPDATE CASCADE: 子表の UPDATE トリガが発火し、DELETE トリガしか無い子表は発火しない。
#[test]
fn update_cascade_fires_update_trigger_not_delete_trigger() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let (core, _calls) = family(dir.path(), recording(&log));
    add_trigger(
        &core.storage,
        "tenant-a",
        "ch",
        &row("t_del", TriggerTiming::After, DEL),
    );
    run(&core, &ctx("tenant-a"), "UPDATE par SET k = 5 WHERE k = 1").expect("update");
    assert!(take(&log).is_empty());
    add_trigger(
        &core.storage,
        "tenant-a",
        "ch",
        &row("t_upd", TriggerTiming::Before, UPD),
    );
    run(&core, &ctx("tenant-a"), "UPDATE par SET k = 6 WHERE k = 2").expect("update");
    assert_eq!(take(&log), vec!["BEFORE UPDATE ch"]);
    assert_eq!(
        rows_of(&core, "tenant-a", "SELECT v FROM ch WHERE parent_id = 6"),
        1
    );
}

// BEFORE UPDATE が他列を書き換えると、子行へ反映される。
#[test]
fn before_update_on_child_rewrites_other_columns() {
    let dir = tempdir();
    let (core, _calls) = family(
        dir.path(),
        Box::new(|c| {
            let mut new = c.new_row.to_vec();
            if let Some(slot) = new.get_mut(0) {
                *slot = Value::Text("zz".to_string());
            }
            Ok(TriggerOutcome::Proceed(new))
        }),
    );
    add_trigger(
        &core.storage,
        "tenant-a",
        "ch",
        &row("t", TriggerTiming::Before, UPD),
    );
    run(&core, &ctx("tenant-a"), "UPDATE par SET k = 7 WHERE k = 1").expect("update");
    assert_eq!(
        rows_of(
            &core,
            "tenant-a",
            "SELECT v FROM ch WHERE v = 'zz' AND parent_id = 7"
        ),
        2
    );
    assert_eq!(
        rows_of(&core, "tenant-a", "SELECT v FROM ch WHERE v = 'c'"),
        1
    );
}

// BEFORE が FK 列を存在しない親キーへ書き換えると `23503`。文全体が取り消される。
#[test]
fn before_rewriting_fk_column_to_missing_parent_is_rejected() {
    let dir = tempdir();
    let (core, _calls) = family(
        dir.path(),
        Box::new(|c| {
            let mut new = c.new_row.to_vec();
            if let Some(slot) = new.get_mut(1) {
                *slot = Value::BigInt(999);
            }
            Ok(TriggerOutcome::Proceed(new))
        }),
    );
    add_trigger(
        &core.storage,
        "tenant-a",
        "ch",
        &row("t", TriggerTiming::Before, UPD),
    );
    assert_eq!(
        code(&core, "tenant-a", "UPDATE par SET k = 7 WHERE k = 1"),
        "23503"
    );
    assert_eq!(
        rows_of(&core, "tenant-a", "SELECT v FROM ch WHERE parent_id = 1"),
        2
    );
    assert_eq!(
        rows_of(&core, "tenant-a", "SELECT k FROM par WHERE k = 1"),
        1
    );
}

// BEFORE が `NULL` を返して子行が残ると、孤児を作らず `23503` で文全体を取り消す（fail-closed）。
#[test]
fn before_null_on_child_delete_is_foreign_key_violation_with_no_side_effects() {
    let dir = tempdir();
    let (core, calls) = family(dir.path(), Box::new(|_| Ok(TriggerOutcome::Skip)));
    add_trigger(
        &core.storage,
        "tenant-a",
        "ch",
        &row("t", TriggerTiming::Before, DEL),
    );
    assert_eq!(
        code(&core, "tenant-a", "DELETE FROM par WHERE k = 1"),
        "23503"
    );
    assert!(calls.load(Ordering::SeqCst) > 0);
    assert_eq!(rows_of(&core, "tenant-a", "SELECT k FROM par"), 2);
    assert_eq!(rows_of(&core, "tenant-a", "SELECT v FROM ch"), 3);
}

// SET NULL: 子表の UPDATE トリガが発火する（子行は消えない）。
#[test]
fn set_null_fires_child_update_trigger() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let (core, _calls) = core_with(dir.path(), recording(&log));
    ddl(
        &core,
        &[
            "CREATE TABLE par (k BIGINT PRIMARY KEY)",
            "CREATE TABLE sn (v TEXT, parent_id BIGINT REFERENCES par ON DELETE SET NULL)",
        ],
    );
    run(&core, &ctx("tenant-a"), "INSERT INTO par (k) VALUES (1)").expect("par");
    run(
        &core,
        &ctx("tenant-a"),
        "INSERT INTO sn (v, parent_id) VALUES ('a', 1), ('b', 1)",
    )
    .expect("sn");
    add_trigger(
        &core.storage,
        "tenant-a",
        "sn",
        &row("t", TriggerTiming::After, UPD),
    );
    run(&core, &ctx("tenant-a"), "DELETE FROM par WHERE k = 1").expect("delete");
    assert_eq!(take(&log), vec!["AFTER UPDATE sn", "AFTER UPDATE sn"]);
    assert_eq!(
        rows_of(
            &core,
            "tenant-a",
            "SELECT v FROM sn WHERE parent_id IS NULL"
        ),
        2
    );
}

// 子の AFTER は、親の AFTER より先に発火する。
#[test]
fn child_after_fires_before_parent_after() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let (core, _calls) = family(dir.path(), recording(&log));
    add_trigger(
        &core.storage,
        "tenant-a",
        "par",
        &row("p", TriggerTiming::After, DEL),
    );
    add_trigger(
        &core.storage,
        "tenant-a",
        "ch",
        &row("c", TriggerTiming::After, DEL),
    );
    run(&core, &ctx("tenant-a"), "DELETE FROM par WHERE k = 2").expect("delete");
    assert_eq!(take(&log), vec!["AFTER DELETE ch", "AFTER DELETE par"]);
}

// 孫段: 親・子にトリガが無くても、孫の DELETE トリガが発火する。
#[test]
fn grandchild_triggers_fire() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let (core, _calls) = core_with(dir.path(), recording(&log));
    ddl(
        &core,
        &[
            "CREATE TABLE a1 (k BIGINT PRIMARY KEY)",
            "CREATE TABLE b1 (k BIGINT PRIMARY KEY, pa BIGINT REFERENCES a1 ON DELETE CASCADE)",
            "CREATE TABLE c1 (w TEXT, pb BIGINT REFERENCES b1 ON DELETE CASCADE)",
        ],
    );
    let a = ctx("tenant-a");
    run(&core, &a, "INSERT INTO a1 (k) VALUES (1)").expect("a1");
    run(&core, &a, "INSERT INTO b1 (k, pa) VALUES (10, 1)").expect("b1");
    run(
        &core,
        &a,
        "INSERT INTO c1 (w, pb) VALUES ('x', 10), ('y', 10)",
    )
    .expect("c1");
    add_trigger(
        &core.storage,
        "tenant-a",
        "c1",
        &row("g", TriggerTiming::After, DEL),
    );
    run(&core, &a, "DELETE FROM a1 WHERE k = 1").expect("delete");
    assert_eq!(take(&log), vec!["AFTER DELETE c1"; 2]);
    assert_eq!(rows_of(&core, "tenant-a", "SELECT w FROM c1"), 0);
}

// 自己参照 FK: 連鎖で消える別行にも、対象表自身のトリガが発火する。
#[test]
fn self_reference_cascade_fires_triggers_for_every_row() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let (core, _calls) = core_with(dir.path(), recording(&log));
    ddl(
        &core,
        &["CREATE TABLE tr (k BIGINT PRIMARY KEY, p BIGINT REFERENCES tr(k) ON DELETE CASCADE)"],
    );
    run(
        &core,
        &ctx("tenant-a"),
        "INSERT INTO tr (k, p) VALUES (1, NULL)",
    )
    .expect("root");
    run(
        &core,
        &ctx("tenant-a"),
        "INSERT INTO tr (k, p) VALUES (2, 1)",
    )
    .expect("c2");
    run(
        &core,
        &ctx("tenant-a"),
        "INSERT INTO tr (k, p) VALUES (3, 2)",
    )
    .expect("c3");
    add_trigger(
        &core.storage,
        "tenant-a",
        "tr",
        &row("t", TriggerTiming::After, DEL),
    );
    run(&core, &ctx("tenant-a"), "DELETE FROM tr WHERE k = 1").expect("delete");
    assert_eq!(take(&log), vec!["AFTER DELETE tr"; 3]);
    assert_eq!(rows_of(&core, "tenant-a", "SELECT k FROM tr"), 0);
}

// 深さ: 連鎖が BEFORE トリガの入れ子の深さ上限を超えたら `54001`（副作用なし）。
#[test]
fn chained_triggers_respect_trigger_depth_limit() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let (core, _calls) = core_with(dir.path(), recording(&log));
    let core = core.with_trigger_limits(crate::trigger_limits::TriggerLimits::new(2));
    ddl(
        &core,
        &["CREATE TABLE tr (k BIGINT PRIMARY KEY, p BIGINT REFERENCES tr(k) ON DELETE CASCADE)"],
    );
    run(
        &core,
        &ctx("tenant-a"),
        "INSERT INTO tr (k, p) VALUES (1, NULL)",
    )
    .expect("root");
    for (k, p) in [(2, 1), (3, 2), (4, 3)] {
        run(
            &core,
            &ctx("tenant-a"),
            &format!("INSERT INTO tr (k, p) VALUES ({k}, {p})"),
        )
        .expect("child");
    }
    add_trigger(
        &core.storage,
        "tenant-a",
        "tr",
        &row("t", TriggerTiming::Before, DEL),
    );
    assert_eq!(
        code(&core, "tenant-a", "DELETE FROM tr WHERE k = 1"),
        "54001"
    );
    assert_eq!(rows_of(&core, "tenant-a", "SELECT k FROM tr"), 4);
}

// 深さ上限 1: 親にトリガが無く子表の BEFORE／AFTER が 1 段だけ発火する連鎖は、段の計数と
// `TriggerRuntime::invoke` の計数が二重にならず成功する（最小値 1 の契約）。
#[test]
fn single_stage_child_trigger_succeeds_at_depth_limit_one() {
    for timing in [TriggerTiming::Before, TriggerTiming::After] {
        let dir = tempdir();
        let log: Log = Arc::default();
        let (core, _calls) = family(dir.path(), recording(&log));
        let core = core.with_trigger_limits(crate::trigger_limits::TriggerLimits::new(1));
        add_trigger(&core.storage, "tenant-a", "ch", &row("t", timing, DEL));
        run(&core, &ctx("tenant-a"), "DELETE FROM par WHERE k = 2").expect("delete");
        assert_eq!(take(&log).len(), 1);
        assert_eq!(rows_of(&core, "tenant-a", "SELECT v FROM ch"), 2);
    }
}

// 深さ上限 1: 実際に 2 段になる連鎖は引き続き `54001`（副作用なし）。
#[test]
fn two_stage_chain_still_rejected_at_depth_limit_one() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let (core, _calls) = core_with(dir.path(), recording(&log));
    let core = core.with_trigger_limits(crate::trigger_limits::TriggerLimits::new(1));
    ddl(
        &core,
        &["CREATE TABLE tr (k BIGINT PRIMARY KEY, p BIGINT REFERENCES tr(k) ON DELETE CASCADE)"],
    );
    for (k, p) in [("1", "NULL"), ("2", "1"), ("3", "2")] {
        run(
            &core,
            &ctx("tenant-a"),
            &format!("INSERT INTO tr (k, p) VALUES ({k}, {p})"),
        )
        .expect("seed");
    }
    add_trigger(
        &core.storage,
        "tenant-a",
        "tr",
        &row("t", TriggerTiming::Before, DEL),
    );
    assert_eq!(
        code(&core, "tenant-a", "DELETE FROM tr WHERE k = 1"),
        "54001"
    );
    assert_eq!(rows_of(&core, "tenant-a", "SELECT k FROM tr"), 3);
}

// 子表の文トリガ・結線の無い経路（RETURNING 付き）は、固定文言の `0A000` のまま。
#[test]
fn statement_trigger_on_child_and_unwired_paths_are_rejected() {
    let dir = tempdir();
    let (core, calls) = family(
        dir.path(),
        Box::new(|c| Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))),
    );
    add_trigger(
        &core.storage,
        "tenant-a",
        "ch",
        &row("r", TriggerTiming::After, DEL),
    );
    assert_eq!(
        code(
            &core,
            "tenant-a",
            "DELETE FROM par WHERE k = 1 RETURNING k USING OPERATION_ID 'u1'"
        ),
        "0A000"
    );
    add_trigger(
        &core.storage,
        "tenant-a",
        "ch",
        &def("s", TriggerTiming::After, TriggerLevel::Statement, DEL),
    );
    assert_eq!(
        code(&core, "tenant-a", "DELETE FROM par WHERE k = 1"),
        "0A000"
    );
    assert_eq!(rows_of(&core, "tenant-a", "SELECT k FROM par"), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// テナント境界: 他テナントにだけあるトリガは発火せず、他テナントの子行・親行は変わらない。
#[test]
fn other_tenants_child_triggers_do_not_fire_and_rows_stay() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let (core, calls) = family(dir.path(), recording(&log));
    seed(&core, "tenant-b");
    add_trigger(
        &core.storage,
        "tenant-b",
        "ch",
        &row("b_only", TriggerTiming::After, DEL),
    );
    run(&core, &ctx("tenant-a"), "DELETE FROM par WHERE k = 1").expect("delete");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(take(&log).is_empty());
    assert_eq!(rows_of(&core, "tenant-a", "SELECT v FROM ch"), 1);
    assert_eq!(rows_of(&core, "tenant-b", "SELECT v FROM ch"), 3);
    assert_eq!(rows_of(&core, "tenant-b", "SELECT k FROM par"), 2);
    // tenant-b 自身の削除では発火し、a の行は変わらない。
    run(&core, &ctx("tenant-b"), "DELETE FROM par WHERE k = 1").expect("delete b");
    assert_eq!(take(&log), vec!["AFTER DELETE ch"; 2]);
    assert_eq!(rows_of(&core, "tenant-a", "SELECT v FROM ch"), 1);
}
