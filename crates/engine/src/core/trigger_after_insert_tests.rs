//! AFTER ROW INSERT トリガの発火の結合テスト（TABLE-45・Issue #1724）。
//!
//! トリガ関数の実体（Issue #2228）が未整備のため、定義をカタログへ直接注入し、呼び出し口を差し替えた
//! invoker が `TriggerCall::writes`（同じ write txn・呼び出しテナントの文脈への窓口）で監査表へ書く。
//! 文の終わりの発火・同一 txn での原子性・遅延 FK・テナント境界・未結線経路のガードを `EngineCore` に
//! SQL を流す水準で固定する。設計記録は `docs/design/trigger-after-insert.md`。

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::trigger_insert_tests::{
    add_trigger, before_row, count, ctx, read_rows, rows_affected, run, text_of, trg_schema,
    trigger_def, Behavior, FnInvoker,
};
use super::EngineCore;
use crate::catalog::trigger::{TriggerLevel, TriggerTiming};
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::exec::Cell;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::Storage;
use crate::test_util::temp_db::tempdir;
use crate::trigger::call::{TriggerCall, TriggerOutcome};

fn after_row(name: &str) -> crate::catalog::trigger::TriggerDef {
    trigger_def(name, TriggerTiming::After, TriggerLevel::Row)
}

/// `trg`（監査対象）・`plain`・`audit`（採番方式の監査表）・`audit_pk`（利用者指定 id の監査表）。
fn core_with(dir: &std::path::Path, behavior: Behavior) -> (EngineCore, Arc<AtomicUsize>) {
    let storage = Storage::open(dir.join("db.redb")).expect("open storage");
    storage.create_table(&trg_schema("trg")).expect("trg");
    storage.create_table(&trg_schema("plain")).expect("plain");
    storage
        .create_table(
            &trg_schema("audit").with_row_identity(crate::catalog::RowIdentity::Allocated),
        )
        .expect("audit");
    storage
        .create_table(&trg_schema("audit_pk"))
        .expect("audit_pk");
    let calls = Arc::new(AtomicUsize::new(0));
    let core = EngineCore::from_storage(storage, Box::new(crate::kernel::CpuScalarProvider))
        .with_trigger_invoker_for_test(Arc::new(FnInvoker {
            behavior,
            calls: Arc::clone(&calls),
        }));
    (core, calls)
}

/// AFTER のときだけ、受け取った NEW を監査表 `audit` へそのまま書く。
fn audit_after() -> Behavior {
    Box::new(|c| {
        if c.tg_when() == "AFTER" {
            c.writes.insert_row("audit", None, c.new_row)?;
        }
        Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
    })
}

const MULTI: &str = "INSERT INTO trg (id, embedding, a, n) VALUES \
                     (1, '[1.0,0.0]', 'x', 10), (2, '[2.0,0.0]', 'y', 20)";

/// 採番方式の監査表 `audit` の `(a, n)` を、書いた順（rowid 昇順）で返す。
fn read_audit(core: &EngineCore, c: &crate::policy::PolicyContext) -> Vec<(String, i64)> {
    let SqlOutcome::Query(q) = run(core, c, "SELECT a, n FROM audit LIMIT 100").expect("select")
    else {
        panic!("expected query");
    };
    q.rows
        .iter()
        .map(|r| {
            let a = match &r.cells[0] {
                Cell::Text(t) => t.clone(),
                o => panic!("a {o:?}"),
            };
            let n = match r.cells[1] {
                Cell::SignedInteger(v) => v,
                ref o => panic!("n {o:?}"),
            };
            (a, n)
        })
        .collect()
}

fn ddl(core: &EngineCore, c: &crate::policy::PolicyContext, sql: &str) {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(c, &mut s, sql).expect(sql);
}

// 対象ビヘイビア: TABLE-45（ポインタ）。書いた行ごとに監査行が 1 つずつ、同じ順で入る（VALUES 単一行・
// 複数行・INSERT ... SELECT）。
#[test]
fn audit_rows_are_written_for_every_insert_form() {
    let dir = tempdir();
    let (core, calls) = core_with(dir.path(), audit_after());
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &after_row("audit_t"));

    let out = run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1)",
    )
    .expect("single");
    assert_eq!(rows_affected(out), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(read_audit(&core, &a), vec![("x".to_string(), 1)]);

    let out = run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES \
         (2, '[2.0,0.0]', 'y', 2), (3, '[3.0,0.0]', 'z', 3)",
    )
    .expect("multi");
    assert_eq!(rows_affected(out), 2);
    let audit = read_audit(&core, &a);
    assert_eq!(
        audit
            .iter()
            .map(|r| (r.0.as_str(), r.1))
            .collect::<Vec<_>>(),
        vec![("x", 1), ("y", 2), ("z", 3)]
    );

    run(
        &core,
        &a,
        "INSERT INTO plain (id, embedding, a, n) VALUES (10, '[1.0,0.0]', 'p', 5), \
         (11, '[1.0,0.0]', 'q', 6)",
    )
    .expect("seed");
    let out = run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) SELECT id, embedding, a, n FROM plain LIMIT 100 \
         USING OPERATION_ID 'op-sel'",
    )
    .expect("insert select");
    assert_eq!(rows_affected(out), 2);
    assert_eq!(count(&core, &a, "audit"), 5);
    assert_eq!(count(&core, &a, "trg"), 5);
}

// AFTER は BEFORE が書き換えた後の最終 NEW を受け取る。
#[test]
fn after_receives_final_new_from_before() {
    let dir = tempdir();
    let (core, _calls) = core_with(
        dir.path(),
        Box::new(|c| {
            let mut row = c.new_row.to_vec();
            if c.tg_when() == "BEFORE" {
                row[1] = Value::Text(format!("{}!", text_of(&row, 1)));
            } else {
                c.writes.insert_row("audit", None, c.new_row)?;
            }
            Ok(TriggerOutcome::Proceed(row))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("b_bang"));
    add_trigger(&core.storage, "tenant-a", "trg", &after_row("a_audit"));
    run(&core, &a, MULTI).expect("insert");
    assert_eq!(
        read_audit(&core, &a),
        vec![("x!".to_string(), 10), ("y!".to_string(), 20)]
    );
}

// 順序: 全行の BEFORE（と書き込み）の後に、AFTER が「行の順 × トリガ名のバイト順」で呼ばれる。
#[test]
fn after_runs_at_statement_end_in_row_then_name_order() {
    let dir = tempdir();
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let l = Arc::clone(&log);
    let (core, _calls) = core_with(
        dir.path(),
        Box::new(move |c: &TriggerCall<'_>| {
            l.lock().expect("lock").push(format!(
                "{}:{}:{}",
                c.tg_when(),
                c.trigger_name,
                text_of(c.new_row, 1)
            ));
            Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &after_row("b_after"));
    add_trigger(&core.storage, "tenant-a", "trg", &after_row("a_after"));
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("z_before"));
    run(&core, &a, MULTI).expect("insert");
    assert_eq!(
        *log.lock().expect("lock"),
        vec![
            "BEFORE:z_before:x",
            "BEFORE:z_before:y",
            "AFTER:a_after:x",
            "AFTER:b_after:x",
            "AFTER:a_after:y",
            "AFTER:b_after:y",
        ]
    );
}

// BEFORE が NULL を返してスキップした行は、AFTER を呼ばない。
#[test]
fn skipped_rows_do_not_fire_after() {
    let dir = tempdir();
    let (core, _calls) = core_with(
        dir.path(),
        Box::new(|c| {
            if c.tg_when() == "BEFORE" && text_of(c.new_row, 1) == "y" {
                return Ok(TriggerOutcome::Skip);
            }
            if c.tg_when() == "AFTER" {
                c.writes.insert_row("audit", None, c.new_row)?;
            }
            Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &before_row("b"));
    add_trigger(&core.storage, "tenant-a", "trg", &after_row("a"));
    let out = run(&core, &a, MULTI).expect("insert");
    assert_eq!(rows_affected(out), 1);
    assert_eq!(read_audit(&core, &a), vec![("x".to_string(), 10)]);
}

// AFTER の戻り値（NULL・構造違いの行）は無視され、外側の行・`rows_affected` は変わらない。
#[test]
fn after_outcome_is_ignored() {
    for outcome in [
        TriggerOutcome::Skip,
        TriggerOutcome::Proceed(vec![Value::Null]),
    ] {
        let dir = tempdir();
        let o = outcome.clone();
        let (core, _calls) = core_with(dir.path(), Box::new(move |_| Ok(o.clone())));
        let a = ctx("tenant-a");
        add_trigger(&core.storage, "tenant-a", "trg", &after_row("a"));
        let out = run(&core, &a, MULTI).expect("insert");
        assert_eq!(rows_affected(out), 2);
        assert_eq!(
            read_rows(&core, &a, "trg"),
            vec![(1, "x".to_string(), 10), (2, "y".to_string(), 20)]
        );
    }
}

// AFTER のエラーで文全体がロールバックされる（行・監査行・台帳）。同じ operation_id の再送は `23505`
// ではなく成功する。
#[test]
fn after_error_rolls_back_rows_audit_and_ledger() {
    let dir = tempdir();
    let fail = Arc::new(AtomicBool::new(true));
    let f = Arc::clone(&fail);
    let (core, _calls) = core_with(
        dir.path(),
        Box::new(move |c| {
            c.writes.insert_row("audit", None, c.new_row)?;
            if f.load(Ordering::SeqCst) && text_of(c.new_row, 1) == "y" {
                return Err(SqlSurfaceError::InvalidInput {
                    detail: "rejected".to_string(),
                });
            }
            Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &after_row("a"));
    let sql = format!("{MULTI} USING OPERATION_ID 'op-after'");
    let err = run(&core, &a, &sql).unwrap_err();
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(count(&core, &a, "trg"), 0);
    assert_eq!(count(&core, &a, "audit"), 0);
    fail.store(false, Ordering::SeqCst);
    run(&core, &a, &sql).expect("resend after rollback is not a replay");
    assert_eq!(count(&core, &a, "trg"), 2);
    assert_eq!(count(&core, &a, "audit"), 2);
}

// 監査行の制約違反（利用者指定 id の重複）は従来の SQLSTATE で文全体を巻き戻す。
#[test]
fn constraint_violation_in_after_rolls_back_statement() {
    let dir = tempdir();
    let (core, _calls) = core_with(
        dir.path(),
        Box::new(|c| {
            c.writes.insert_row("audit_pk", Some(1), c.new_row)?;
            Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &after_row("a"));
    let err = run(&core, &a, MULTI).unwrap_err();
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(count(&core, &a, "trg"), 0);
    assert_eq!(count(&core, &a, "audit_pk"), 0);
}

// 明示 Tx: AFTER のエラーで Failed（`25P02`）、ROLLBACK 後は何も残らない。成功時は COMMIT で確定する。
#[test]
fn explicit_transaction_semantics() {
    let dir = tempdir();
    let fail = Arc::new(AtomicBool::new(true));
    let f = Arc::clone(&fail);
    let (core, _calls) = core_with(
        dir.path(),
        Box::new(move |c| {
            c.writes.insert_row("audit", None, c.new_row)?;
            if f.load(Ordering::SeqCst) {
                return Err(SqlSurfaceError::InvalidInput {
                    detail: "rejected".to_string(),
                });
            }
            Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
        }),
    );
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &after_row("a"));

    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&a, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&a, &mut s, &mut txn, INSERT_A1_N)
        .unwrap_err();
    assert_eq!(err.wire_code(), "22000");
    let err = core
        .execute_sql_in_txn(&a, &mut s, &mut txn, "SELECT COUNT(*) FROM trg")
        .unwrap_err();
    assert_eq!(err.wire_code(), "25P02");
    core.execute_sql_in_txn(&a, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(count(&core, &a, "trg"), 0);
    assert_eq!(count(&core, &a, "audit"), 0);

    fail.store(false, Ordering::SeqCst);
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&a, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&a, &mut s, &mut txn, MULTI)
        .expect("insert");
    core.execute_sql_in_txn(&a, &mut s, &mut txn, "COMMIT")
        .expect("commit");
    assert_eq!(count(&core, &a, "trg"), 2);
    assert_eq!(count(&core, &a, "audit"), 2);
}

/// 親 `p`・遅延 FK の子 `c`（`parent_id`）を作り、AFTER が子へ `parent_id = 999`（親なし）を書くコア。
fn deferred_fk_core(dir: &std::path::Path) -> EngineCore {
    let (core, _calls) = core_with(
        dir,
        Box::new(|c| {
            if c.tg_when() == "AFTER" {
                c.writes.insert_row("c", None, &[Value::BigInt(999)])?;
            }
            Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
        }),
    );
    let sys = ctx("tenant-a");
    ddl(&core, &sys, "CREATE TABLE p (k BIGINT PRIMARY KEY)");
    ddl(
        &core,
        &sys,
        "CREATE TABLE c (parent_id BIGINT REFERENCES p DEFERRABLE INITIALLY DEFERRED)",
    );
    add_trigger(&core.storage, "tenant-a", "trg", &after_row("a"));
    core
}

// P0: トリガが書いた表の遅延 FK が素通りしない。autocommit は文の終わりに `23503`、明示 Tx は COMMIT で
// `23503`（`mark_written` の漏れが無いことの回帰）。
#[test]
fn deferred_fk_written_by_trigger_is_checked() {
    let dir = tempdir();
    let core = deferred_fk_core(dir.path());
    let a = ctx("tenant-a");

    let err = run(&core, &a, INSERT_A1_N).unwrap_err();
    assert_eq!(err.wire_code(), "23503");
    assert_eq!(count(&core, &a, "trg"), 0);
    assert_eq!(count(&core, &a, "c"), 0);

    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&a, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&a, &mut s, &mut txn, INSERT_A1_N)
        .expect("deferred: the statement itself passes");
    let err = core
        .execute_sql_in_txn(&a, &mut s, &mut txn, "COMMIT")
        .unwrap_err();
    assert_eq!(err.wire_code(), "23503");
    assert_eq!(count(&core, &a, "trg"), 0);
    assert_eq!(count(&core, &a, "c"), 0);

    // 対照: 親があれば通る。
    run(&core, &a, "INSERT INTO p (k) VALUES (999)").expect("parent");
    run(&core, &a, INSERT_A1_N).expect("with parent");
    assert_eq!(count(&core, &a, "c"), 1);
}

const INSERT_A1_N: &str = "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1)";

// テナント境界: 監査行は書き込みテナントの行として入り、別テナントからは見えない。トリガは書き込みテナント
// 自身の scope のものだけが発火する。
#[test]
fn audit_rows_stay_in_the_writing_tenant() {
    let dir = tempdir();
    let (core, calls) = core_with(dir.path(), audit_after());
    let a = ctx("tenant-a");
    let b = ctx("tenant-b");
    add_trigger(&core.storage, "tenant-a", "trg", &after_row("audit_t"));
    run(&core, &a, INSERT_A1_N).expect("a");
    assert_eq!(count(&core, &a, "audit"), 1);
    assert_eq!(count(&core, &b, "audit"), 0);
    // B の書き込みでは A のトリガは発火しない。
    run(&core, &b, INSERT_A1_N).expect("b");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(count(&core, &b, "audit"), 0);
}

// 入れ子: トリガ内の書き込み先に INSERT トリガがあると `0A000`（再帰しない）。文全体は巻き戻る。
#[test]
fn nested_trigger_target_is_0a000() {
    let dir = tempdir();
    let (core, _calls) = core_with(dir.path(), audit_after());
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &after_row("audit_t"));
    add_trigger(&core.storage, "tenant-a", "audit", &before_row("nested"));
    let err = run(&core, &a, INSERT_A1_N).unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(count(&core, &a, "trg"), 0);
    assert_eq!(count(&core, &a, "audit"), 0);
}

// 文トリガは引き続き `0A000`。`WHEN` 付き AFTER も `0A000`（発火時に二重化）。副作用なし。
#[test]
fn statement_level_and_when_are_0a000() {
    let dir = tempdir();
    let (core, calls) = core_with(dir.path(), audit_after());
    let a = ctx("tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &trigger_def("s", TriggerTiming::After, TriggerLevel::Statement),
    );
    let err = run(&core, &a, INSERT_A1_N).unwrap_err();
    assert_eq!(err.wire_code(), "0A000");

    let dir2 = tempdir();
    let (core2, calls2) = core_with(dir2.path(), audit_after());
    let mut def = after_row("w");
    def.when_sql = Some("n > 0".to_string());
    core2
        .storage
        .create_function("tenant-a", &super::trigger_insert_tests::func("f"))
        .ok();
    core2
        .storage
        .create_trigger_for_test("tenant-a", "trg", &def)
        .expect("create");
    let err = run(&core2, &a, INSERT_A1_N).unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(
        calls.load(Ordering::SeqCst) + calls2.load(Ordering::SeqCst),
        0
    );
    assert_eq!(count(&core, &a, "trg"), 0);
    assert_eq!(count(&core2, &a, "trg"), 0);
}

// 発火に未対応の経路（RETURNING・UPSERT）は AFTER ROW のある表でも `0A000`。台帳は消費されない。
#[test]
fn unwired_paths_stay_guarded_for_after_triggers() {
    let dir = tempdir();
    let (core, calls) = core_with(dir.path(), audit_after());
    let a = ctx("tenant-a");
    add_trigger(&core.storage, "tenant-a", "trg", &after_row("audit_t"));
    let err = run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
         RETURNING id USING OPERATION_ID 'op-g'",
    )
    .unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
    let err = run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-g'",
    )
    .unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
         USING OPERATION_ID 'op-g'",
    )
    .expect("operation id is not consumed by the guarded path");
    assert_eq!(count(&core, &a, "audit"), 1);
}
