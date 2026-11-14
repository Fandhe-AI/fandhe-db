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

// `UPDATE OF a` の BEFORE トリガは SET 列（`DEFAULT` 指定を含む）が `a` と交差しない文では発火しない。
// その文で NOT NULL 検査を延期すると行の有無で `UPDATE 0` と `23502` が分かれるため、従来どおり
// 束縛時に `23502` を返す（行の有無に依存しない）。交差する文では延期される。
#[test]
fn update_of_non_intersecting_set_keeps_bind_time_not_null_error() {
    let dir = tempdir();
    let (core, _calls) = seeded(dir.path(), passthrough());
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("b", TriggerTiming::Before, UPD, &["a"]),
    );
    for sql in [
        "UPDATE trg SET n = NULL WHERE id = 999",
        "UPDATE trg SET n = NULL WHERE id = 1",
        "UPDATE trg SET n = DEFAULT WHERE id = 999",
        "UPDATE trg SET n = DEFAULT WHERE id = 1",
        "UPDATE trg SET n = NULL WHERE n > 1000",
    ] {
        assert_eq!(code(&core, &a, sql), "23502", "{sql}");
    }
    // 交差する文では最終 NEW まで延期されるため、対象行がなければ `UPDATE 0`。
    let out = run(&core, &a, "UPDATE trg SET a = 'q', n = NULL WHERE id = 999").expect("deferred");
    assert_eq!(affected(out), 0);
}

// 自己参照 FK（CASCADE）を持つ表の UPDATE・DELETE トリガは、親側の行に発火する（Issue #3284 で子行ごとの
// 発火に結線済み。連鎖で消える別行への発火は `trigger_referential_tests`）。
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
    run(&core, &a, "UPDATE selfr SET p = NULL WHERE k = 1").expect("update");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    run(&core, &a, "DELETE FROM selfr WHERE k = 1").expect("delete");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

// AFTER だけのトリガ・値を書き換え得ない構成では、NOT NULL 違反は束縛時に 23502（行の有無で分かれない）。
#[test]
fn not_null_is_checked_at_bind_without_before_trigger() {
    let dir = tempdir();
    let (core, _calls) = seeded(dir.path(), passthrough());
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("t", TriggerTiming::After, UPD, &[]),
    );
    // 一致する行が無い述語でも 23502（UPDATE 0 にならない）。
    assert_eq!(
        code(&core, &a, "UPDATE trg SET n = NULL WHERE id = 999"),
        "23502"
    );
    assert_eq!(
        code(&core, &a, "UPDATE trg SET n = NULL WHERE id = 1"),
        "23502"
    );
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

// 分割実行 DML（Issue #3283）: 文トリガがある表は、固定文言の 0A000。
// 副作用はゼロ（トリガ呼び出し 0・ジョブ記録なし・行数不変）。行トリガだけの表は通る（次のテスト）。
// intentional: docs/design/trigger-partitioned-dml.md
// 根拠は同書 D1（文トリガは非原子実行で保証できない）。
#[test]
fn partitioned_dml_is_rejected_when_statement_triggers_exist() {
    let dir = tempdir();
    let (core, calls) = seeded(dir.path(), passthrough());
    let a = ctx("tenant-a");
    let mut before_stmt = dml_trigger("s1", TriggerTiming::Before, UPD, &[]);
    before_stmt.level = TriggerLevel::Statement;
    add_trigger(&core.storage, "tenant-a", "trg", &before_stmt);
    let mut after_stmt = dml_trigger("s2", TriggerTiming::After, DEL, &[]);
    after_stmt.level = TriggerLevel::Statement;
    add_trigger(&core.storage, "tenant-a", "trg", &after_stmt);

    for (sql, op) in [
        (
            "UPDATE trg SET n = 1 WHERE n > 0 USING OPERATION_ID 'p1' PARTITIONED CHUNK 1",
            "p1",
        ),
        (
            "DELETE FROM trg WHERE n > 0 USING OPERATION_ID 'p2' PARTITIONED CHUNK 1",
            "p2",
        ),
    ] {
        let err = run(&core, &a, sql).expect_err(sql);
        assert_eq!(err.wire_code(), "0A000");
        // 固定文言（値・表名・トリガ名を含まない）。
        let msg = format!("{err:?}");
        assert!(msg.contains("statement triggers are not supported for partitioned DML"));
        assert!(!msg.contains("s1") && !msg.contains("trg"));
        let SqlOutcome::Query(q) =
            run(&core, &a, &format!("SHOW PARTITIONED DML '{op}' ON trg")).expect("show")
        else {
            panic!("expected query");
        };
        assert!(q.rows.is_empty(), "no job record for {op}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(count(&core, &a, "trg"), 3);
    // 文トリガは通常実行では発火する（Issue #1726。分割実行だけが `0A000`）。
    run(&core, &a, "DELETE FROM trg WHERE id = 1").expect("normal delete fires statement trigger");
    assert_eq!(count(&core, &a, "trg"), 2);
}

fn timing_of<'a>(log: &'a [String], when: &str) -> Vec<&'a str> {
    log.iter()
        .filter(|l| l.starts_with(when))
        .map(String::as_str)
        .collect()
}

fn row_trigger_core(
    dir: &std::path::Path,
    log: &Log,
) -> (EngineCore, crate::policy::PolicyContext) {
    let (core, _calls) = seeded(dir, recording(log));
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
    take(log);
    (core, a)
}

// 行トリガ（BEFORE／AFTER ROW）のある表の分割実行は発火して通る。保証範囲（設計記録 D2）:
// (a) 発火回数・(b) timing ごとの行順は非分割実行と同一、(c) 1 チャンクなら全体の並びも一致。
#[test]
fn partitioned_row_triggers_fire_with_same_counts_and_order() {
    for chunk in [1usize, 2, 3, 100] {
        let dir = tempdir();
        let log: Log = Arc::default();
        let (core, a) = row_trigger_core(dir.path(), &log);

        let out = run(
            &core,
            &a,
            &format!(
                "UPDATE trg SET a = 'q' WHERE n > 0 USING OPERATION_ID 'u{chunk}' PARTITIONED CHUNK {chunk}"
            ),
        )
        .expect("partitioned update");
        assert_eq!(affected(out), 3);
        let part = take(&log);
        assert!(read_rows(&core, &a, "trg").iter().all(|r| r.1 == "q"));

        let out = run(&core, &a, "UPDATE trg SET a = 'r' WHERE n > 0").expect("plain update");
        assert_eq!(affected(out), 3);
        let plain = take(&log);
        assert_eq!(part.len(), plain.len(), "(a) chunk {chunk}");
        assert_eq!(
            timing_of(&part, "BEFORE"),
            timing_of(&plain, "BEFORE"),
            "(b) chunk {chunk}"
        );
        assert_eq!(
            timing_of(&part, "AFTER"),
            timing_of(&plain, "AFTER"),
            "(b) chunk {chunk}"
        );
        if chunk >= 3 {
            assert_eq!(part, plain, "(c) chunk {chunk}");
        }

        let out = run(
            &core,
            &a,
            &format!(
                "DELETE FROM trg WHERE n > 0 USING OPERATION_ID 'd{chunk}' PARTITIONED CHUNK {chunk}"
            ),
        )
        .expect("partitioned delete");
        assert_eq!(affected(out), 3);
        let part = take(&log);
        assert_eq!(count(&core, &a, "trg"), 0);
        assert_eq!(timing_of(&part, "BEFORE").len(), 3);
        assert_eq!(
            timing_of(&part, "AFTER"),
            vec![
                "AFTER DELETE z_after 10->-",
                "AFTER DELETE z_after 20->-",
                "AFTER DELETE z_after 30->-"
            ]
        );
    }
}

// BEFORE の NULL 返却（スキップ）はカーソルを進め、再走査・再発火が無い。rows_affected は書いた行数。
// 全行スキップでも完了する（StateConflict にならない）。
#[test]
fn partitioned_before_skip_advances_cursor_without_refire() {
    for chunk in [1usize, 2] {
        let dir = tempdir();
        let log: Log = Arc::default();
        let log2 = Arc::clone(&log);
        let skip_all = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let sa = Arc::clone(&skip_all);
        let (core, _calls) = seeded(
            dir.path(),
            Box::new(move |c| {
                if c.tg_when() == "BEFORE"
                    && (sa.load(Ordering::SeqCst) || c.old_row.is_some_and(|r| n_of(r) == 20))
                {
                    log2.lock().expect("log").push("SKIP".to_string());
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

        let out = run(
            &core,
            &a,
            &format!(
                "UPDATE trg SET a = 'q' WHERE n > 0 USING OPERATION_ID 'u1' PARTITIONED CHUNK {chunk}"
            ),
        )
        .expect("update");
        assert_eq!(affected(out), 2);
        let seen = take(&log);
        assert_eq!(seen.iter().filter(|l| *l == "SKIP").count(), 1);
        assert_eq!(seen.iter().filter(|l| *l == "BEFORE UPDATE").count(), 2);
        assert_eq!(seen.iter().filter(|l| *l == "AFTER UPDATE").count(), 2);
        let rows = read_rows(&core, &a, "trg");
        assert_eq!((rows[0].1.as_str(), rows[1].1.as_str()), ("q", "y"));

        // 全行スキップ。
        skip_all.store(true, Ordering::SeqCst);
        let out = run(
            &core,
            &a,
            &format!(
                "DELETE FROM trg WHERE n > 0 USING OPERATION_ID 'd1' PARTITIONED CHUNK {chunk}"
            ),
        )
        .expect("all skipped delete completes");
        assert_eq!(affected(out), 0);
        assert_eq!(count(&core, &a, "trg"), 3);
        assert_eq!(take(&log).iter().filter(|l| *l == "SKIP").count(), 3);
    }
}

// BEFORE が別表へ書いて行をスキップするチャンクでも、別表への書き込みは commit される
// （変更 0 件のチャンクとして破棄されない）。
#[test]
fn partitioned_before_skip_with_side_write_is_committed() {
    let dir = tempdir();
    let (core, _calls) = seeded(
        dir.path(),
        Box::new(|c| {
            if c.tg_when() == "BEFORE" {
                c.writes
                    .insert_row("alloc", None, c.old_row.unwrap_or(&[]))?;
                return Ok(TriggerOutcome::Skip);
            }
            Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("b", TriggerTiming::Before, DEL, &[]),
    );
    let out = run(
        &core,
        &a,
        "DELETE FROM trg WHERE n > 0 USING OPERATION_ID 'd1' PARTITIONED CHUNK 1",
    )
    .expect("delete");
    assert_eq!(affected(out), 0);
    assert_eq!(count(&core, &a, "trg"), 3);
    assert_eq!(count(&core, &a, "alloc"), 3);
}

// トリガの失敗は当該チャンクだけを abort し、commit 済みチャンクは残る（VD001）。同じ operation_id の
// 再送は未処理分から再開し、完了済みの行のトリガは再発火しない。
#[test]
fn partitioned_trigger_failure_aborts_only_that_chunk_and_resend_resumes() {
    let dir = tempdir();
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let f = Arc::clone(&fail);
    let seen = Arc::new(AtomicUsize::new(0));
    let s2 = Arc::clone(&seen);
    let (core, _calls) = seeded(
        dir.path(),
        Box::new(move |c| {
            if c.tg_when() == "AFTER" {
                let n = s2.fetch_add(1, Ordering::SeqCst);
                if f.load(Ordering::SeqCst) && n == 1 {
                    return Err(SqlSurfaceError::InvalidInput {
                        detail: "rejected".to_string(),
                    });
                }
            }
            Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("a", TriggerTiming::After, UPD, &[]),
    );
    let sql = "UPDATE trg SET n = 7 WHERE n > 0 USING OPERATION_ID 'u1' PARTITIONED CHUNK 1";
    assert_eq!(code(&core, &a, sql), "VD001");
    let rows = read_rows(&core, &a, "trg");
    assert_eq!(rows.iter().filter(|r| r.2 == 7).count(), 1, "chunk 1 stays");
    assert_eq!(rows[0].2, 7);

    fail.store(false, Ordering::SeqCst);
    seen.store(0, Ordering::SeqCst);
    let out = run(&core, &a, sql).expect("resend resumes");
    assert_eq!(affected(out), 3);
    assert_eq!(seen.load(Ordering::SeqCst), 2, "finished row not refired");
    assert!(read_rows(&core, &a, "trg").iter().all(|r| r.2 == 7));
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

// 参照アクションが及ぶ子表に該当イベントのトリガがあれば、親にトリガが無くても子行ごとに発火する
// （Issue #3284。発火の詳細は `trigger_referential_tests`）。結線の無い経路（RETURNING 付き）は 0A000。
#[test]
fn referential_action_to_triggered_child_fires_and_unwired_path_is_rejected() {
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
    run(&core, &a, "INSERT INTO par (k) VALUES (1)").expect("par");
    run(&core, &a, "INSERT INTO ch (parent_id) VALUES (1), (1)").expect("ch");
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
            "DELETE FROM par WHERE k = 1 RETURNING k USING OPERATION_ID 'f1'"
        ),
        "0A000"
    );
    // DELETE トリガしか無い子表へ及ぶのは UPDATE の CASCADE だが、UPDATE イベントのトリガは無いので発火しない。
    run(
        &core,
        &a,
        "UPDATE par SET k = 2 WHERE k = 1 USING OPERATION_ID 'f2'",
    )
    .expect("update cascade without update trigger on child");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    // DELETE の CASCADE は子の DELETE トリガを子行ごとに発火する。
    run(
        &core,
        &a,
        "DELETE FROM par WHERE k = 2 USING OPERATION_ID 'f3'",
    )
    .expect("delete cascade fires child trigger");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
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
