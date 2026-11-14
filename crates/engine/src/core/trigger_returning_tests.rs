//! `RETURNING` を伴う INSERT／UPDATE／DELETE でのトリガ発火の結合テスト（TABLE-45・SQL-21・Issue #3192）。
//!
//! 定義をカタログへ直接注入し、呼び出し口を差し替えた invoker で、`RETURNING` が (1) BEFORE ROW が書き換えた
//! 最終行を返す・(2) AFTER 系の影響を受けない・(3) BEFORE が `NULL` 返却でスキップした行を返さない・
//! (4) DELETE は削除前の値を返す・(5) 文トリガが 1 回発火する・(6) AFTER の失敗時に行・台帳・トリガの書き込みが
//! 残らない・(7) テナント境界を保つ、ことを `EngineCore` に SQL を流す水準で固定する。範囲外の形（UPSERT・
//! 結合形・分割実行）は引き続き `0A000`。設計記録は `docs/design/sql-returning.md` の「トリガ付き表の RETURNING」節。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use super::trigger_insert_tests::{
    add_trigger, before_row, core_with, count, ctx, run, text_of, trigger_def, Behavior,
};
use super::EngineCore;
use crate::catalog::trigger::{TriggerDef, TriggerEvents, TriggerLevel, TriggerTiming};
use crate::policy::PolicyContext;
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::test_util::temp_db::tempdir;
use crate::trigger::call::TriggerOutcome;

const INS: u8 = TriggerEvents::INSERT;
const UPD: u8 = TriggerEvents::UPDATE;
const DEL: u8 = TriggerEvents::DELETE;

fn dml_trigger(name: &str, timing: TriggerTiming, level: TriggerLevel, events: u8) -> TriggerDef {
    let mut def = trigger_def(name, timing, level);
    def.events = TriggerEvents::from_bits(events).expect("events");
    def
}

fn n_of(row: &[Value]) -> i64 {
    match row.get(2) {
        Some(Value::BigInt(n)) => *n,
        other => panic!("unexpected n {other:?}"),
    }
}

/// BEFORE ROW は `n` に 1000 を足す（`a = 'skip'` の行は `NULL` 返却でスキップ）。AFTER ROW は、INSERT・UPDATE
/// では受け取った NEW を採番方式の表 `alloc` へ監査として書く（`n` は 777 に変える。RETURNING へ影響しない
/// ことを確かめるため）。
fn behavior() -> Behavior {
    Box::new(|c| {
        if c.tg_when() == "BEFORE" {
            let probe: &[Value] = if c.new_row.is_empty() {
                c.old_row.unwrap_or(&[])
            } else {
                c.new_row
            };
            if matches!(probe.get(1), Some(Value::Text(t)) if t == "skip") {
                return Ok(TriggerOutcome::Skip);
            }
            let mut row = c.new_row.to_vec();
            if !row.is_empty() {
                let n = n_of(&row);
                if let Some(slot) = row.get_mut(2) {
                    *slot = Value::BigInt(n + 1000);
                }
            }
            return Ok(TriggerOutcome::Proceed(row));
        }
        if c.tg_op() != "DELETE" && c.tg_level() == "ROW" {
            let mut audit = c.new_row.to_vec();
            if let Some(slot) = audit.get_mut(2) {
                *slot = Value::BigInt(777);
            }
            c.writes.insert_row("alloc", None, &audit)?;
        }
        Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
    })
}

/// `RETURNING` の結果を (id, a, n) の列で返す。
fn returned(out: SqlOutcome) -> (u64, Vec<(u64, String, i64)>) {
    let SqlOutcome::Returning(o) = out else {
        panic!("expected returning outcome");
    };
    let rows = o
        .result
        .rows
        .iter()
        .map(|r| {
            let id = match r.cells[0] {
                Cell::Integer(v) => v,
                ref x => panic!("id {x:?}"),
            };
            let a = match &r.cells[1] {
                Cell::Text(t) => t.clone(),
                x => panic!("a {x:?}"),
            };
            let n = match r.cells[2] {
                Cell::SignedInteger(v) => v,
                ref x => panic!("n {x:?}"),
            };
            (id, a, n)
        })
        .collect();
    (o.rows_affected, rows)
}

fn ret(core: &EngineCore, c: &PolicyContext, sql: &str) -> (u64, Vec<(u64, String, i64)>) {
    returned(run(core, c, sql).unwrap_or_else(|e| panic!("{sql}: {e}")))
}

fn seed(core: &EngineCore, c: &PolicyContext) {
    run(
        core,
        c,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 10), \
         (2, '[2.0,0.0]', 'y', 20), (3, '[3.0,0.0]', 'z', 30)",
    )
    .expect("seed");
}

fn code(core: &EngineCore, c: &PolicyContext, sql: &str) -> &'static str {
    run(core, c, sql).expect_err(sql).wire_code()
}

const INS_RET: &str = "RETURNING id, a, n";

// INSERT（単一行・複数行）: BEFORE ROW が書き換えた最終行が返り、`NULL` 返却の行は影響行数にも返却にも出ない。
// AFTER ROW が別表へ書いても（NEW を書き換えても）返却は変わらない。
#[test]
fn insert_returning_reflects_before_rewrite_and_skips() {
    let dir = tempdir();
    let (core, calls) = core_with(dir.path(), behavior());
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("b", TriggerTiming::Before, TriggerLevel::Row, INS),
    );
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("t", TriggerTiming::After, TriggerLevel::Row, INS),
    );

    let (n, rows) = ret(
        &core,
        &a,
        &format!("INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) {INS_RET}"),
    );
    assert_eq!((n, rows), (1, vec![(1, "x".to_string(), 1001)]));
    assert_eq!(count(&core, &a, "alloc"), 1);

    let (n, rows) = ret(
        &core,
        &a,
        &format!(
            "INSERT INTO trg (id, embedding, a, n) VALUES (2, '[2.0,0.0]', 'keep', 2), \
             (3, '[3.0,0.0]', 'skip', 3), (4, '[4.0,0.0]', 'keep', 4) {INS_RET}"
        ),
    );
    assert_eq!(n, 2);
    assert_eq!(
        rows,
        vec![(2, "keep".to_string(), 1002), (4, "keep".to_string(), 1004)]
    );
    // 返却の値と保存された値が一致し、AFTER の監査表は 3 行（INSERT 3 行分）。
    assert_eq!(count(&core, &a, "trg"), 3);
    assert_eq!(count(&core, &a, "alloc"), 3);
    assert!(calls.load(Ordering::SeqCst) >= 6);
}

// 全行が `NULL` 返却でスキップされると、影響 0 行・返却 0 行で、同じ operation_id の再送は `23505`。
#[test]
fn insert_returning_all_skipped_returns_nothing_and_records_ledger() {
    let dir = tempdir();
    let (core, calls) = core_with(dir.path(), behavior());
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("b"));
    let sql = format!(
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'skip', 1) {INS_RET} \
         USING OPERATION_ID 'op-skip'"
    );
    assert_eq!(ret(&core, &a, &sql), (0, vec![]));
    let before = calls.load(Ordering::SeqCst);
    assert_eq!(code(&core, &a, &sql), "23505");
    assert_eq!(calls.load(Ordering::SeqCst), before);
    assert_eq!(count(&core, &a, "trg"), 0);
}

// 採番方式の表（TABLE-23）の INSERT でもトリガ経路で最終行が返る（`id` 疑似列は持たないため列だけを返す）。
#[test]
fn insert_returning_on_allocated_table_returns_final_row() {
    let dir = tempdir();
    let (core, _calls) = core_with(dir.path(), behavior());
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "alloc",
        &dml_trigger("b", TriggerTiming::Before, TriggerLevel::Row, INS),
    );
    let SqlOutcome::Returning(o) = run(
        &core,
        &a,
        "INSERT INTO alloc (embedding, a, n) VALUES ('[1.0,0.0]', 'p', 5) RETURNING a, n",
    )
    .expect("insert") else {
        panic!("expected returning");
    };
    assert_eq!(o.rows_affected, 1);
    assert_eq!(o.result.rows.len(), 1);
    assert_eq!(
        o.result.rows[0].cells,
        vec![Cell::Text("p".to_string()), Cell::SignedInteger(1005)]
    );
}

// UPDATE（id 指定形・述語形）: BEFORE が書き換えた最終 NEW が返り、スキップ行は返らない。AFTER は影響しない。
#[test]
fn update_returning_reflects_before_rewrite_and_skips() {
    let dir = tempdir();
    let (core, _calls) = core_with(dir.path(), behavior());
    let a = ctx("tenant-a");
    seed(&core, &a);
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("b", TriggerTiming::Before, TriggerLevel::Row, UPD),
    );
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("t", TriggerTiming::After, TriggerLevel::Row, UPD),
    );

    let (n, rows) = ret(
        &core,
        &a,
        &format!("UPDATE trg SET n = 5 WHERE id = 1 {INS_RET}"),
    );
    assert_eq!((n, rows), (1, vec![(1, "x".to_string(), 1005)]));
    assert_eq!(count(&core, &a, "alloc"), 1);

    // 述語形。`y` の行は `skip` へ SET されて BEFORE がスキップする（返却にも影響行数にも出ない）。
    let (n, rows) = ret(
        &core,
        &a,
        &format!("UPDATE trg SET a = 'skip' WHERE id = 2 {INS_RET}"),
    );
    assert_eq!((n, rows), (0, vec![]));
    let (n, rows) = ret(
        &core,
        &a,
        &format!("UPDATE trg SET a = 'q' WHERE n > 15 AND n < 1000 {INS_RET}"),
    );
    assert_eq!(n, 2);
    assert_eq!(
        rows,
        vec![(2, "q".to_string(), 1020), (3, "q".to_string(), 1030)]
    );
}

// DELETE: BEFORE を通って削除した行の削除前の値を返す。スキップ行は返らず、行も残る。
#[test]
fn delete_returning_returns_pre_image_and_skips() {
    let dir = tempdir();
    let (core, _calls) = core_with(dir.path(), behavior());
    let a = ctx("tenant-a");
    seed(&core, &a);
    run(&core, &a, "UPDATE trg SET a = 'skip' WHERE id = 3").expect("mark skip");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("b", TriggerTiming::Before, TriggerLevel::Row, DEL),
    );

    let (n, rows) = ret(
        &core,
        &a,
        &format!("DELETE FROM trg WHERE id = 1 {INS_RET}"),
    );
    assert_eq!((n, rows), (1, vec![(1, "x".to_string(), 10)]));
    // 述語形: id 3 は BEFORE がスキップするので残り、id 2 だけ削除される。
    let (n, rows) = ret(&core, &a, &format!("DELETE FROM trg WHERE n > 0 {INS_RET}"));
    assert_eq!((n, rows), (1, vec![(2, "y".to_string(), 20)]));
    assert_eq!(count(&core, &a, "trg"), 1);
    // 不在の id は 0 行（存在差を出さない）。
    let (n, rows) = ret(
        &core,
        &a,
        &format!("DELETE FROM trg WHERE id = 99 {INS_RET}"),
    );
    assert_eq!((n, rows), (0, vec![]));
}

// 文トリガ（BEFORE・AFTER STATEMENT）は RETURNING 付きでも 1 回ずつ発火し、対象 0 件でも発火する。
#[test]
fn statement_triggers_fire_once_with_returning() {
    let dir = tempdir();
    let calls_log = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::clone(&calls_log);
    let (core, _calls) = core_with(
        dir.path(),
        Box::new(move |c| {
            if c.tg_level() == "STATEMENT" {
                c2.fetch_add(1, Ordering::SeqCst);
            }
            Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
        }),
    );
    let a = ctx("tenant-a");
    seed(&core, &a);
    for (name, timing) in [("sb", TriggerTiming::Before), ("sa", TriggerTiming::After)] {
        add_trigger(
            &core.storage,
            "tenant-a",
            "trg",
            &dml_trigger(name, timing, TriggerLevel::Statement, INS | UPD | DEL),
        );
    }
    for sql in [
        format!("UPDATE trg SET n = 1 WHERE n > 0 {INS_RET}"),
        format!("UPDATE trg SET n = 1 WHERE id = 99 {INS_RET}"),
        format!("DELETE FROM trg WHERE id = 99 {INS_RET}"),
        format!("INSERT INTO trg (id, embedding, a, n) VALUES (9, '[1.0,0.0]', 'x', 1) {INS_RET}"),
    ] {
        calls_log.store(0, Ordering::SeqCst);
        run(&core, &a, &sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        assert_eq!(calls_log.load(Ordering::SeqCst), 2, "{sql}");
    }
}

// 明示トランザクション内でも同じ結果になり、ROLLBACK で行・監査行とも残らない。
#[test]
fn returning_in_explicit_transaction() {
    let dir = tempdir();
    let (core, _calls) = core_with(dir.path(), behavior());
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger("b", TriggerTiming::Before, TriggerLevel::Row, INS | UPD),
    );
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&a, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let out = core
        .execute_sql_in_txn(
            &a,
            &mut s,
            &mut txn,
            &format!(
                "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) {INS_RET}"
            ),
        )
        .expect("insert");
    assert_eq!(returned(out), (1, vec![(1, "x".to_string(), 1001)]));
    let out = core
        .execute_sql_in_txn(
            &a,
            &mut s,
            &mut txn,
            &format!("UPDATE trg SET n = 2 WHERE id = 1 {INS_RET}"),
        )
        .expect("update");
    assert_eq!(returned(out), (1, vec![(1, "x".to_string(), 1002)]));
    core.execute_sql_in_txn(&a, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(count(&core, &a, "trg"), 0);
}

// AFTER の失敗は文全体を巻き戻す（行・台帳。トリガが書いた行も）。同じ operation_id の再送は成功する。
#[test]
fn after_failure_rolls_back_returning_statement() {
    let dir = tempdir();
    let fail = Arc::new(AtomicBool::new(true));
    let f = Arc::clone(&fail);
    let (core, _calls) = core_with(
        dir.path(),
        Box::new(move |c| {
            if c.tg_when() == "AFTER" {
                c.writes.insert_row("alloc", None, c.new_row)?;
                if f.load(Ordering::SeqCst) && text_of(c.new_row, 1) == "y" {
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
        &dml_trigger("t", TriggerTiming::After, TriggerLevel::Row, INS),
    );
    let sql = format!(
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1), \
         (2, '[2.0,0.0]', 'y', 2) {INS_RET} USING OPERATION_ID 'op-after'"
    );
    assert_eq!(code(&core, &a, &sql), "22000");
    assert_eq!(count(&core, &a, "trg"), 0);
    assert_eq!(count(&core, &a, "alloc"), 0);
    fail.store(false, Ordering::SeqCst);
    let (n, rows) = ret(&core, &a, &sql);
    assert_eq!(n, 2);
    assert_eq!(rows.len(), 2);
}

// テナント境界: テナント A のトリガは、テナント B の RETURNING 付き書き込みでは発火せず、B は自分の行だけを
// 受け取る（A のトリガの存在で拒否もされない）。
#[test]
fn returning_respects_tenant_boundary() {
    let dir = tempdir();
    let (core, calls) = core_with(dir.path(), behavior());
    let a = ctx("tenant-a");
    let b = ctx("tenant-b");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger(
            "b",
            TriggerTiming::Before,
            TriggerLevel::Row,
            INS | UPD | DEL,
        ),
    );
    run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1)",
    )
    .expect("a insert");
    let before = calls.load(Ordering::SeqCst);

    let (n, rows) = ret(
        &core,
        &b,
        &format!(
            "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'bx', 5) {INS_RET}"
        ),
    );
    assert_eq!((n, rows), (1, vec![(1, "bx".to_string(), 5)]));
    let (n, rows) = ret(
        &core,
        &b,
        &format!("UPDATE trg SET n = 6 WHERE n > 0 {INS_RET}"),
    );
    assert_eq!((n, rows), (1, vec![(1, "bx".to_string(), 6)]));
    let (n, rows) = ret(&core, &b, &format!("DELETE FROM trg WHERE n > 0 {INS_RET}"));
    assert_eq!((n, rows), (1, vec![(1, "bx".to_string(), 6)]));
    assert_eq!(calls.load(Ordering::SeqCst), before);
    // A の行は B の操作で影響を受けない。
    assert_eq!(count(&core, &a, "trg"), 1);
}

// 範囲外の形は引き続き `0A000`（UPSERT・結合形）。台帳は消費されない。
#[test]
fn unsupported_forms_stay_guarded_with_returning() {
    let dir = tempdir();
    let (core, calls) = core_with(dir.path(), behavior());
    let a = ctx("tenant-a");
    seed(&core, &a);
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &dml_trigger(
            "b",
            TriggerTiming::Before,
            TriggerLevel::Row,
            INS | UPD | DEL,
        ),
    );
    let before = calls.load(Ordering::SeqCst);
    for sql in [
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
         ON CONFLICT (id) DO NOTHING RETURNING id USING OPERATION_ID 'u1'",
        "UPDATE trg SET n = 1 FROM plain WHERE trg.id = plain.id RETURNING trg.id \
         USING OPERATION_ID 'u2'",
        "DELETE FROM trg USING plain WHERE trg.id = plain.id RETURNING trg.id \
         USING OPERATION_ID 'u3'",
    ] {
        assert_eq!(code(&core, &a, sql), "0A000", "{sql}");
    }
    assert_eq!(calls.load(Ordering::SeqCst), before);
    assert_eq!(count(&core, &a, "trg"), 3);
}
