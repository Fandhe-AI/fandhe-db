//! BEFORE・AFTER ROW UPDATE／DELETE トリガの発火の結合テスト（TABLE-45・SQL-32・Issue #1725）。
//!
//! トリガ関数の実体（Issue #2228）が未整備のため、定義をカタログへ直接注入し、呼び出し口を差し替えた
//! invoker が OLD・NEW・`TG_OP`・`TG_WHEN` を記録する。NEW の書き換え・`NULL` 返却による行のスキップ・
//! `UPDATE OF`・文全体の原子性・台帳・明示 Tx・分割実行と未結線経路のガード・テナント境界を
//! `EngineCore` に SQL を流す水準で固定する。設計記録は `docs/design/trigger-update-delete.md`。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::trigger_insert_tests::{
    add_trigger, core_with, count, ctx, read_rows, run, trigger_def, Behavior,
};
use super::EngineCore;
use crate::catalog::trigger::{TriggerDef, TriggerEvents, TriggerLevel, TriggerTiming};
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::test_util::temp_db::tempdir;
use crate::trigger::call::TriggerOutcome;

const UPD: u8 = TriggerEvents::UPDATE;
const DEL: u8 = TriggerEvents::DELETE;

fn dml_trigger(name: &str, timing: TriggerTiming, events: u8, update_of: &[&str]) -> TriggerDef {
    let mut def = trigger_def(name, timing, TriggerLevel::Row);
    def.events = TriggerEvents::from_bits(events).expect("events");
    def.update_of = update_of.iter().map(|s| (*s).to_string()).collect();
    def
}

fn n_of(row: &[Value]) -> i64 {
    match row.get(2) {
        Some(Value::BigInt(n)) => *n,
        other => panic!("unexpected n {other:?}"),
    }
}

fn affected(out: SqlOutcome) -> u64 {
    match out {
        SqlOutcome::Update(o) => o.rows_affected,
        SqlOutcome::Delete(o) => o.rows_affected,
        SqlOutcome::Returning(o) => o.rows_affected,
        _ => panic!("expected update or delete outcome"),
    }
}

fn code(core: &EngineCore, c: &crate::policy::PolicyContext, sql: &str) -> &'static str {
    run(core, c, sql).expect_err(sql).wire_code()
}

/// `trg` へ 3 行 `(1,'x',10)`・`(2,'y',20)`・`(3,'z',30)` を入れたコア（トリガは入れる前に無い）。
fn seeded(dir: &std::path::Path, behavior: Behavior) -> (EngineCore, Arc<AtomicUsize>) {
    let (core, calls) = core_with(dir, behavior);
    let a = ctx("tenant-a");
    run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 10), \
         (2, '[2.0,0.0]', 'y', 20), (3, '[3.0,0.0]', 'z', 30)",
    )
    .expect("seed");
    (core, calls)
}

fn passthrough() -> Behavior {
    Box::new(|c| Ok(TriggerOutcome::Proceed(c.new_row.to_vec())))
}

type Log = Arc<Mutex<Vec<String>>>;

/// 呼び出しを `TG_WHEN TG_OP name old->new` の形で記録し、行は変えない。
fn recording(log: &Log) -> Behavior {
    let log = Arc::clone(log);
    Box::new(move |c| {
        let old = c.old_row.map_or("-".to_string(), |r| n_of(r).to_string());
        let new = if c.new_row.is_empty() {
            "-".to_string()
        } else {
            n_of(c.new_row).to_string()
        };
        log.lock().expect("log").push(format!(
            "{} {} {} {}->{}",
            c.tg_when(),
            c.tg_op(),
            c.trigger_name,
            old,
            new
        ));
        Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
    })
}

fn take(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.lock().expect("log"))
}

// BEFORE UPDATE は OLD を受け取り、返した NEW が保存される。id 指定形・述語形の両方。
#[test]
fn before_update_rewrites_new_and_receives_old() {
    let dir = tempdir();
    let (core, calls) = seeded(
        dir.path(),
        Box::new(|c| {
            let old = c.old_row.expect("OLD");
            let mut new = c.new_row.to_vec();
            if let Some(slot) = new.get_mut(2) {
                *slot = Value::BigInt(n_of(old) + 1000);
            }
            Ok(TriggerOutcome::Proceed(new))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("b", TriggerTiming::Before, UPD, &[]),
    );

    let out = run(&core, &a, "UPDATE trg SET n = 5 WHERE id = 1").expect("single");
    assert_eq!(affected(out), 1);
    assert_eq!(read_rows(&core, &a, "trg")[0].2, 1010);
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    let out = run(
        &core,
        &a,
        "UPDATE trg SET a = 'q' WHERE n > 15 AND n < 1000",
    )
    .expect("predicate");
    assert_eq!(affected(out), 2);
    let rows = read_rows(&core, &a, "trg");
    assert_eq!(rows[1], (2, "q".to_string(), 1020));
    assert_eq!(rows[2], (3, "q".to_string(), 1030));
}

// BEFORE UPDATE の前に NEW 候補の NOT NULL 検査をしない。SET が NOT NULL 列へ NULL を入れても、BEFORE が
// 有効な値へ補正すれば成功する。補正しなければ最終 NEW の検査で `23502` になる。
#[test]
fn before_update_can_repair_null_in_not_null_column() {
    let dir = tempdir();
    let (core, _calls) = seeded(
        dir.path(),
        Box::new(|c| {
            let mut new = c.new_row.to_vec();
            if let Some(slot) = new.get_mut(2) {
                *slot = Value::BigInt(77);
            }
            Ok(TriggerOutcome::Proceed(new))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("b", TriggerTiming::Before, UPD, &[]),
    );
    let out = run(&core, &a, "UPDATE trg SET n = NULLIF(n, n) WHERE id = 1").expect("repaired");
    assert_eq!(affected(out), 1);
    assert_eq!(read_rows(&core, &a, "trg")[0].2, 77);

    let dir2 = tempdir();
    let (core2, _) = seeded(dir2.path(), passthrough());
    add_trigger(
        &core2.storage,
        "tenant-a",
        "trg",
        &dml_trigger("b", TriggerTiming::Before, UPD, &[]),
    );
    assert_eq!(
        code(&core2, &a, "UPDATE trg SET n = NULLIF(n, n) WHERE id = 1"),
        "23502"
    );
}

// 明示的な `SET n = NULL`・既定値の無い NOT NULL 列への `SET n = DEFAULT` も、束縛時に `23502` にせず
// BEFORE UPDATE が補正できる（最終 NEW だけを検査する）。補正しなければ `23502`。
#[test]
fn before_update_can_repair_explicit_null_and_default() {
    let dir = tempdir();
    let (core, _calls) = seeded(
        dir.path(),
        Box::new(|c| {
            let mut new = c.new_row.to_vec();
            if let Some(slot) = new.get_mut(2) {
                *slot = Value::BigInt(88);
            }
            Ok(TriggerOutcome::Proceed(new))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("b", TriggerTiming::Before, UPD, &[]),
    );
    for sql in [
        "UPDATE trg SET n = NULL WHERE id = 1",
        "UPDATE trg SET n = DEFAULT WHERE id = 1",
        "UPDATE trg SET n = NULL WHERE n > 0",
    ] {
        run(&core, &a, sql).expect(sql);
    }
    assert_eq!(read_rows(&core, &a, "trg")[0].2, 88);

    let dir2 = tempdir();
    let (core2, _) = seeded(dir2.path(), passthrough());
    add_trigger(
        &core2.storage,
        "tenant-a",
        "trg",
        &dml_trigger("b", TriggerTiming::Before, UPD, &[]),
    );
    assert_eq!(
        code(&core2, &a, "UPDATE trg SET n = NULL WHERE id = 1"),
        "23502"
    );
    assert_eq!(
        code(&core2, &a, "UPDATE trg SET n = DEFAULT WHERE id = 1"),
        "23502"
    );
    assert_eq!(
        code(&core2, &a, "UPDATE trg SET n = NULL WHERE n > 0"),
        "23502"
    );
    // トリガの無い表では従来どおり束縛時に `23502`。
    let dir3 = tempdir();
    let (core3, _) = seeded(dir3.path(), passthrough());
    assert_eq!(
        code(&core3, &a, "UPDATE trg SET n = NULL WHERE id = 1"),
        "23502"
    );
}

// 自己参照 FK（CASCADE）を持つ表自身の UPDATE・DELETE トリガは、子表扱いで 0A000 にならず発火する。
#[test]
fn self_referencing_table_triggers_fire() {
    let dir = tempdir();
    let (core, calls) = seeded(dir.path(), passthrough());
    let a = ctx("tenant-a");
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(
        &a,
        &mut s,
        "CREATE TABLE selfr (k BIGINT PRIMARY KEY, p BIGINT REFERENCES selfr(k) ON DELETE CASCADE ON UPDATE CASCADE)",
    )
    .expect("ddl");
    add_trigger(
        &core.storage,
        "tenant-a",
        "selfr",
        &dml_trigger("t", TriggerTiming::Before, UPD | DEL, &[]),
    );
    run(&core, &a, "INSERT INTO selfr (k) VALUES (1)").expect("insert");
    let out = run(&core, &a, "UPDATE selfr SET p = NULL WHERE k = 1").expect("update");
    assert_eq!(affected(out), 1);
    let out = run(&core, &a, "DELETE FROM selfr WHERE k = 1").expect("delete");
    assert_eq!(affected(out), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

// BEFORE が NULL を返した行は更新・削除されず、影響行数にも AFTER にも数えない。
#[test]
fn before_skip_excludes_row_from_update_and_delete() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let log2 = Arc::clone(&log);
    let (core, _calls) = seeded(
        dir.path(),
        Box::new(move |c| {
            if c.tg_when() == "BEFORE" && c.old_row.is_some_and(|r| n_of(r) == 20) {
                return Ok(TriggerOutcome::Skip);
            }
            log2.lock()
                .expect("log")
                .push(format!("{} {}", c.tg_when(), c.tg_op()));
            Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("b", TriggerTiming::Before, UPD | DEL, &[]),
    );
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("z_after", TriggerTiming::After, UPD | DEL, &[]),
    );

    let out = run(&core, &a, "UPDATE trg SET a = 'q' WHERE n > 0").expect("update");
    assert_eq!(affected(out), 2);
    let rows = read_rows(&core, &a, "trg");
    assert_eq!(rows[0].1, "q");
    assert_eq!(rows[1].1, "y");
    assert_eq!(rows[2].1, "q");
    assert_eq!(
        take(&log),
        vec![
            "BEFORE UPDATE",
            "BEFORE UPDATE",
            "AFTER UPDATE",
            "AFTER UPDATE"
        ]
    );

    let out = run(&core, &a, "DELETE FROM trg WHERE n > 0").expect("delete");
    assert_eq!(affected(out), 2);
    assert_eq!(read_rows(&core, &a, "trg").len(), 1);
    assert_eq!(count(&core, &a, "trg"), 1);
    assert_eq!(
        take(&log),
        vec![
            "BEFORE DELETE",
            "BEFORE DELETE",
            "AFTER DELETE",
            "AFTER DELETE"
        ]
    );
}

// AFTER は最終 NEW と OLD を、変更の順 × トリガ名のバイト順で受け取る。DELETE は OLD だけ。
#[test]
fn after_receives_old_and_final_new_in_order() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let (core, _calls) = seeded(dir.path(), recording(&log));
    let a = ctx("tenant-a");
    for (name, timing) in [
        ("a2", TriggerTiming::After),
        ("a1", TriggerTiming::After),
        ("b1", TriggerTiming::Before),
    ] {
        add_trigger(
            &core.storage,
            "tenant-a",
            "trg",
            &dml_trigger(name, timing, UPD | DEL, &[]),
        );
    }

    run(&core, &a, "UPDATE trg SET n = 99 WHERE n < 25").expect("update");
    assert_eq!(
        take(&log),
        vec![
            "BEFORE UPDATE b1 10->99",
            "BEFORE UPDATE b1 20->99",
            "AFTER UPDATE a1 10->99",
            "AFTER UPDATE a2 10->99",
            "AFTER UPDATE a1 20->99",
            "AFTER UPDATE a2 20->99",
        ]
    );

    run(&core, &a, "DELETE FROM trg WHERE id = 3").expect("delete");
    assert_eq!(
        take(&log),
        vec![
            "BEFORE DELETE b1 30->-",
            "AFTER DELETE a1 30->-",
            "AFTER DELETE a2 30->-"
        ]
    );
}

// `UPDATE OF` は、その列が SET に書かれた文でだけ発火する（BEFORE による書き換えは判定に含めない）。
#[test]
fn update_of_fires_only_when_column_is_set() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let (core, _calls) = seeded(dir.path(), recording(&log));
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("of_a", TriggerTiming::After, UPD, &["a"]),
    );

    run(&core, &a, "UPDATE trg SET n = 1 WHERE id = 1").expect("n only");
    run(&core, &a, "UPDATE trg SET n = 1 WHERE n > 0").expect("n only predicate");
    assert!(take(&log).is_empty());

    run(&core, &a, "UPDATE trg SET a = 'k' WHERE id = 1").expect("a");
    assert_eq!(take(&log), vec!["AFTER UPDATE of_a 1->1"]);
    run(&core, &a, "UPDATE trg SET a = 'k', n = 2 WHERE n > 0").expect("a predicate");
    assert_eq!(take(&log).len(), 3);
}

// トリガのエラーで文全体が取り消され、同じ operation_id を再利用できる。成功後の再送は 23505 で
// トリガを呼ばない。
#[test]
fn failure_rolls_back_statement_and_resend_skips_triggers() {
    let dir = tempdir();
    let calls_seen = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&calls_seen);
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let f = Arc::clone(&fail);
    let (core, calls) = seeded(
        dir.path(),
        Box::new(move |c| {
            if c.tg_when() == "AFTER"
                && f.load(Ordering::SeqCst)
                && seen.fetch_add(1, Ordering::SeqCst) == 1
            {
                return Err(SqlSurfaceError::InvalidInput {
                    detail: "rejected".to_string(),
                });
            }
            Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("a", TriggerTiming::After, UPD | DEL, &[]),
    );

    assert_eq!(
        code(
            &core,
            &a,
            "UPDATE trg SET n = 7 WHERE n > 0 USING OPERATION_ID 'u1'"
        ),
        "22000"
    );
    assert!(read_rows(&core, &a, "trg").iter().all(|r| r.2 != 7));
    calls_seen.store(0, Ordering::SeqCst);
    assert_eq!(
        code(
            &core,
            &a,
            "DELETE FROM trg WHERE n > 0 USING OPERATION_ID 'd1'"
        ),
        "22000"
    );
    assert_eq!(count(&core, &a, "trg"), 3);

    fail.store(false, Ordering::SeqCst);
    let before = calls.load(Ordering::SeqCst);
    run(
        &core,
        &a,
        "UPDATE trg SET n = 7 WHERE n > 0 USING OPERATION_ID 'u1'",
    )
    .expect("same op id is reusable after a rolled back statement");
    let after_first = calls.load(Ordering::SeqCst);
    assert_eq!(after_first - before, 3);
    assert_eq!(
        code(
            &core,
            &a,
            "UPDATE trg SET n = 7 WHERE n > 0 USING OPERATION_ID 'u1'"
        ),
        "23505"
    );
    assert_eq!(calls.load(Ordering::SeqCst), after_first);
}

// 明示 Tx: AFTER のエラーで Failed（25P02）。成功時は COMMIT で UPDATE・DELETE が確定する。
#[test]
fn explicit_transaction_fires_and_commits() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let (core, _calls) = seeded(dir.path(), recording(&log));
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("t", TriggerTiming::After, UPD | DEL, &[]),
    );

    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        "UPDATE trg SET n = 11 WHERE id = 1",
        "DELETE FROM trg WHERE n > 25",
        "COMMIT",
    ] {
        core.execute_sql_in_txn(&a, &mut s, &mut txn, sql)
            .expect(sql);
    }
    assert_eq!(
        take(&log),
        vec!["AFTER UPDATE t 10->11", "AFTER DELETE t 30->-"]
    );
    assert_eq!(count(&core, &a, "trg"), 2);
}

// 分割実行 DML は、該当イベントのトリガ（行・文を問わない）があれば 0A000。副作用はゼロ。
#[test]
fn partitioned_dml_is_rejected_when_triggers_exist() {
    let dir = tempdir();
    let (core, calls) = seeded(dir.path(), passthrough());
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("r", TriggerTiming::Before, UPD, &[]),
    );
    let mut stmt_level = dml_trigger("s", TriggerTiming::After, DEL, &[]);
    stmt_level.level = TriggerLevel::Statement;
    add_trigger(&core.storage, "tenant-a", "trg", &stmt_level);

    assert_eq!(
        code(
            &core,
            &a,
            "UPDATE trg SET n = 1 WHERE n > 0 USING OPERATION_ID 'p1' PARTITIONED CHUNK 1"
        ),
        "0A000"
    );
    assert_eq!(
        code(
            &core,
            &a,
            "DELETE FROM trg WHERE n > 0 USING OPERATION_ID 'p2' PARTITIONED CHUNK 1"
        ),
        "0A000"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(count(&core, &a, "trg"), 3);
    for op in ["p1", "p2"] {
        let SqlOutcome::Query(q) =
            run(&core, &a, &format!("SHOW PARTITIONED DML '{op}' ON trg")).expect("show")
        else {
            panic!("expected query");
        };
        assert!(q.rows.is_empty(), "no job record for {op}");
    }
    // 文トリガは通常実行でも未結線（#1726）のため 0A000。
    assert_eq!(code(&core, &a, "DELETE FROM trg WHERE id = 1"), "0A000");
}

// 発火に未対応の経路（RETURNING・UPSERT・生の行 API）は 0A000。台帳は消費されない。
#[test]
fn unwired_paths_are_guarded() {
    let dir = tempdir();
    let (core, calls) = seeded(dir.path(), passthrough());
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("t", TriggerTiming::Before, UPD | DEL, &[]),
    );

    for sql in [
        "UPDATE trg SET n = 1 WHERE id = 1 RETURNING id USING OPERATION_ID 'g1'",
        "UPDATE trg SET n = 1 WHERE n > 0 RETURNING id USING OPERATION_ID 'g2'",
        "DELETE FROM trg WHERE id = 1 RETURNING id USING OPERATION_ID 'g3'",
        "DELETE FROM trg WHERE n > 0 RETURNING id USING OPERATION_ID 'g4'",
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
         ON CONFLICT (id) DO UPDATE SET n = 2 USING OPERATION_ID 'g5'",
    ] {
        assert_eq!(code(&core, &a, sql), "0A000", "{sql}");
    }
    let op = crate::recovery::required_op_id::OperationId::parse("op-raw").expect("op");
    let err = crate::tenant::delete_row(&core.storage, "trg", &a, 1, &op).unwrap_err();
    assert_eq!(
        crate::error_format::ClassifiedError::error_class(&err),
        crate::error_format::ErrorClass::FeatureNotSupported
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(count(&core, &a, "trg"), 3);
    // 台帳が消費されていないので、同じ operation_id を発火する経路で使える。
    run(
        &core,
        &a,
        "UPDATE trg SET n = 1 WHERE id = 1 USING OPERATION_ID 'g1'",
    )
    .expect("ledger not consumed");
}

// 参照アクションが及ぶ子表に該当イベントのトリガがあれば、親にトリガが無くても 0A000。
#[test]
fn referential_action_to_triggered_child_is_rejected() {
    let dir = tempdir();
    let (core, calls) = seeded(dir.path(), passthrough());
    let a = ctx("tenant-a");
    let mut s = SessionState::default();
    s.allow_ddl();
    for ddl in [
        "CREATE TABLE par (k BIGINT PRIMARY KEY)",
        "CREATE TABLE ch (parent_id BIGINT REFERENCES par ON DELETE CASCADE ON UPDATE CASCADE)",
    ] {
        core.execute_sql_in_session(&a, &mut s, ddl).expect(ddl);
    }
    add_trigger(
        &core.storage,
        "tenant-a",
        "ch",
        &dml_trigger("t", TriggerTiming::After, DEL, &[]),
    );
    assert_eq!(
        code(
            &core,
            &a,
            "DELETE FROM par WHERE k = 1 USING OPERATION_ID 'f1'"
        ),
        "0A000"
    );
    // DELETE トリガしか無い子表へ及ぶのは UPDATE の CASCADE だが、UPDATE イベントのトリガは無いので通る。
    run(
        &core,
        &a,
        "UPDATE par SET k = 2 WHERE k = 1 USING OPERATION_ID 'f2'",
    )
    .expect("update cascade without update trigger on child");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

// テナント境界: 別テナントが同じ表へ作ったトリガは発火せず、応答にも現れない。
#[test]
fn other_tenants_triggers_do_not_fire() {
    let dir = tempdir();
    let log: Log = Arc::default();
    let (core, calls) = seeded(dir.path(), recording(&log));
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-b",
        "trg",
        &dml_trigger("b_only", TriggerTiming::Before, UPD | DEL, &[]),
    );
    let out = run(&core, &a, "UPDATE trg SET n = 1 WHERE n > 0").expect("update");
    assert_eq!(affected(out), 3);
    let out = run(&core, &a, "DELETE FROM trg WHERE id = 1").expect("delete");
    assert_eq!(affected(out), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(take(&log).is_empty());
}

// トリガの無い表では従来どおり（結果と件数）。
#[test]
fn tables_without_triggers_are_unchanged() {
    let dir = tempdir();
    let (core, calls) = seeded(dir.path(), passthrough());
    let a = ctx("tenant-a");
    let out = run(&core, &a, "UPDATE trg SET n = 5 WHERE n > 15").expect("update");
    assert_eq!(affected(out), 2);
    let out = run(&core, &a, "DELETE FROM trg WHERE n = 5").expect("delete");
    assert_eq!(affected(out), 2);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(count(&core, &a, "trg"), 1);
}
