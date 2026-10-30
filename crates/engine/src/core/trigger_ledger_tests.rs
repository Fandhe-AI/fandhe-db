//! トリガの副作用と `operation_id` 台帳の整合の結合テスト（TABLE-45・TABLE-24 ポインタ・Issue #1728）。
//!
//! 固定する性質は 3 つ。(a) 同じ `operation_id` の再送でトリガが再実行されず、副作用が重複しない
//! （`23505`）。(b) クラッシュしても「文の書き込み・トリガの副作用・台帳」が全部残るか全部残らない。
//! (c) トリガ内の書き込みが他テナントへ届かず、エラーが他テナントの情報を含まない。
//!
//! トリガ関数の実体（Issue #2228）が未整備で、呼び出し口を差し替えられるのは `cfg(test)` の
//! `EngineCore::with_trigger_invoker_for_test` だけである。このため公開 API だけを使う example や
//! 結合テストではトリガを発火できず、クラッシュ注入も `recovery::commit_boundary` と同じ
//! 「テストバイナリ自身を子プロセスとして再実行する」形で本ファイルに置く。
//! 設計記録は `docs/design/trigger-ledger-consistency.md`。

use std::io::BufRead as _;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::trigger_insert_tests::{
    add_trigger, count, ctx, read_rows, run, trg_schema, trigger_def, Behavior, FnInvoker,
};
use super::EngineCore;
use crate::catalog::trigger::{TriggerDef, TriggerEvents, TriggerLevel, TriggerTiming};
use crate::policy::PolicyContext;
use crate::recovery::required_op_id::LedgerMode;
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;
use crate::sql::SqlOutcome;
use crate::storage::Storage;
use crate::test_util::temp_db::{tempdir, unique_db_path, CleanupGuard};
use crate::trigger::call::{TriggerCall, TriggerOutcome};

const INS: u8 = TriggerEvents::INSERT;
const UPD: u8 = TriggerEvents::UPDATE;
const DEL: u8 = TriggerEvents::DELETE;
const TRUNC: u8 = TriggerEvents::TRUNCATE;

fn def(name: &str, timing: TriggerTiming, level: TriggerLevel, events: u8) -> TriggerDef {
    let mut d = trigger_def(name, timing, level);
    d.events = TriggerEvents::from_bits(events).expect("events");
    d
}

/// トリガの副作用として書く監査行（表・タイミング・粒度・操作が文字列で残る）。
fn audit_row(c: &TriggerCall<'_>) -> Vec<Value> {
    vec![
        Value::Vector(vec![0.0, 0.0]),
        Value::Text(format!(
            "{} {} {} {}",
            c.tg_when(),
            c.tg_level(),
            c.tg_op(),
            c.table
        )),
        Value::BigInt(1),
    ]
}

/// 全トリガ共通の振る舞い。`trg`・`plain` への発火は `audit` へ、`audit` への発火は `audit2` へ
/// 1 行書く（入れ子の確認用）。BEFORE ROW の INSERT／UPDATE は NEW の `a` を書き換える
/// （台帳のハッシュが書き換え前の値で取られることの確認用）。`abort_at`（`"AFTER ROW INSERT"` 形式）
/// に一致した呼び出しでは、副作用を書いた直後にプロセスを中断する（クラッシュ注入。子プロセス専用）。
fn effects(abort_at: Option<String>) -> Behavior {
    Box::new(move |c| {
        let key = format!("{} {} {}", c.tg_when(), c.tg_level(), c.tg_op());
        let dest = match c.table {
            "trg" | "plain" => Some("audit"),
            "audit" => Some("audit2"),
            _ => None,
        };
        if let Some(d) = dest {
            c.writes.insert_row(d, None, &audit_row(c))?;
        }
        if abort_at.as_deref() == Some(key.as_str()) {
            println!("CHILD_ABORTING");
            // pipe 越しの stdout はブロックバッファされるため、abort の前に親へ届くよう flush する。
            let _ = std::io::Write::flush(&mut std::io::stdout());
            std::process::abort();
        }
        let mut row = c.new_row.to_vec();
        if key == "BEFORE ROW INSERT" || key == "BEFORE ROW UPDATE" {
            if let Some(Value::Text(t)) = row.get_mut(1) {
                t.push('!');
            }
        }
        Ok(TriggerOutcome::Proceed(row))
    })
}

/// `trg`・`plain`・`audit`（採番）・`audit2`（採番）を持つコア。`create` が偽なら既存 DB を開き直す。
fn build(path: &Path, create: bool, behavior: Behavior) -> (EngineCore, Arc<AtomicUsize>) {
    let storage = Storage::open(path).expect("open storage");
    if create {
        storage.create_table(&trg_schema("trg")).expect("trg");
        storage.create_table(&trg_schema("plain")).expect("plain");
        for t in ["audit", "audit2"] {
            storage
                .create_table(
                    &trg_schema(t).with_row_identity(crate::catalog::RowIdentity::Allocated),
                )
                .expect("audit table");
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let core = EngineCore::from_storage(storage, Box::new(crate::kernel::CpuScalarProvider))
        .with_trigger_invoker_for_test(Arc::new(FnInvoker {
            behavior,
            calls: Arc::clone(&calls),
        }));
    (core, calls)
}

/// `trg` に BEFORE／AFTER の行トリガと文トリガ、`plain` に TRUNCATE の文トリガを定義する。
fn add_all_triggers(core: &EngineCore, scope: &str) {
    use TriggerLevel::{Row, Statement};
    use TriggerTiming::{After, Before};
    let row_ev = INS | UPD | DEL;
    let stmt_ev = INS | UPD | DEL | TRUNC;
    let s = &core.storage;
    add_trigger(s, scope, "trg", &def("a_brow", Before, Row, row_ev));
    add_trigger(s, scope, "trg", &def("b_arow", After, Row, row_ev));
    add_trigger(s, scope, "trg", &def("c_bstmt", Before, Statement, stmt_ev));
    add_trigger(s, scope, "trg", &def("d_astmt", After, Statement, stmt_ev));
    add_trigger(s, scope, "plain", &def("e_bstmt", Before, Statement, TRUNC));
    add_trigger(s, scope, "plain", &def("f_astmt", After, Statement, TRUNC));
}

fn is_duplicate(e: &SqlSurfaceError) -> bool {
    matches!(e, SqlSurfaceError::DuplicateOperationId)
}

fn id_col(core: &EngineCore, c: &PolicyContext, t: &str) -> Vec<u64> {
    let SqlOutcome::Query(q) =
        run(core, c, &format!("SELECT id FROM {t} LIMIT 10000")).expect("ids")
    else {
        panic!("expected query");
    };
    let mut ids: Vec<u64> = q
        .rows
        .iter()
        .map(|r| match r.cells[0] {
            crate::sql::exec::Cell::Integer(v) => v,
            ref o => panic!("id {o:?}"),
        })
        .collect();
    ids.sort_unstable();
    ids
}

/// 採番方式の表の `(a, n)` を書いた順（rowid 昇順）で返す。この表は `id` 列を持たない。
fn read_an(core: &EngineCore, c: &PolicyContext, table: &str) -> Vec<(String, i64)> {
    let SqlOutcome::Query(q) =
        run(core, c, &format!("SELECT a, n FROM {table} LIMIT 1000")).expect("select")
    else {
        panic!("expected query");
    };
    q.rows
        .iter()
        .map(|r| {
            let a = match &r.cells[0] {
                crate::sql::exec::Cell::Text(t) => t.clone(),
                o => panic!("a {o:?}"),
            };
            let n = match r.cells[1] {
                crate::sql::exec::Cell::SignedInteger(v) => v,
                ref o => panic!("n {o:?}"),
            };
            (a, n)
        })
        .collect()
}

/// 採番方式の表の次の払い出し id を、write txn を commit せずに覗く（drop で破棄され状態は動かない）。
fn next_rowid(core: &EngineCore, c: &PolicyContext, table: &str) -> u64 {
    let txn = core.storage.begin_write_txn().expect("txn");
    let r = crate::catalog::rowid_alloc::allocate_in_txn(&txn, c.tenant_id(), table, 1)
        .expect("allocate");
    r.start().raw_for_state()
}

/// 採番方式の監査表に欠番が無い（払い出し済みの id が行数と一致する）。
fn audit_gapless(core: &EngineCore, c: &PolicyContext, table: &str) -> bool {
    next_rowid(core, c, table) == count(core, c, table) + 1
}

/// 副作用の全体像（再送で 1 つも変わらないことの比較用）。
#[derive(Debug, PartialEq)]
struct Snap {
    calls: usize,
    trg: Vec<(u64, String, i64)>,
    plain: Vec<(u64, String, i64)>,
    audit: Vec<(String, i64)>,
    audit2: Vec<(String, i64)>,
    /// 採番方式の監査表の次の払い出し id（再送が採番を進めていないことの確認用）。
    next_audit: u64,
    next_audit2: u64,
}

fn snap(core: &EngineCore, calls: &AtomicUsize, c: &PolicyContext) -> Snap {
    Snap {
        calls: calls.load(Ordering::SeqCst),
        trg: read_rows(core, c, "trg"),
        plain: read_rows(core, c, "plain"),
        audit: read_an(core, c, "audit"),
        audit2: read_an(core, c, "audit2"),
        next_audit: next_rowid(core, c, "audit"),
        next_audit2: next_rowid(core, c, "audit2"),
    }
}

fn is_contiguous(ids: &[u64]) -> bool {
    ids.iter().enumerate().all(|(i, id)| *id == i as u64 + 1)
}

const NEXT_OP: &str = "INSERT INTO trg (id, embedding, a, n) VALUES (900, '[1.0,0.0]', 'next', 1) \
     USING OPERATION_ID 'next-op'";

struct Case {
    label: &'static str,
    /// 事前に流す SQL（台帳なし。トリガ定義後なので発火しても構わない）。
    seed: &'static [&'static str],
    sql: &'static str,
    /// 同じ operation_id で内容だけ変えた SQL（`22023` になるはずのもの）。
    mismatch: Option<&'static str>,
}

const SEED_TRG: &str = "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 10), \
                        (2, '[2.0,0.0]', 'y', 20), (3, '[3.0,0.0]', 'z', 30)";
const SEED_PLAIN: &str = "INSERT INTO plain (id, embedding, a, n) VALUES \
                          (10, '[1.0,0.0]', 'p', 5), (11, '[1.0,0.0]', 'q', 6)";

fn cases() -> Vec<Case> {
    vec![
        Case {
            label: "insert single",
            seed: &[],
            sql: "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 10) \
                  USING OPERATION_ID 'op'",
            mismatch: Some(
                "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'CHANGED', 10) \
                 USING OPERATION_ID 'op'",
            ),
        },
        Case {
            label: "insert multi",
            seed: &[],
            sql: "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 10), \
                  (2, '[2.0,0.0]', 'y', 20) USING OPERATION_ID 'op'",
            mismatch: Some(
                "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 10), \
                 (2, '[2.0,0.0]', 'y', 21) USING OPERATION_ID 'op'",
            ),
        },
        Case {
            label: "insert select (rows)",
            seed: &[SEED_PLAIN],
            sql: "INSERT INTO trg (id, embedding, a, n) SELECT id, embedding, a, n FROM plain \
                  LIMIT 100 USING OPERATION_ID 'op'",
            mismatch: None,
        },
        Case {
            label: "insert select (zero rows)",
            seed: &[],
            sql: "INSERT INTO trg (id, embedding, a, n) SELECT id, embedding, a, n FROM plain \
                  LIMIT 100 USING OPERATION_ID 'op'",
            mismatch: None,
        },
        Case {
            label: "update by id",
            seed: &[SEED_TRG],
            sql: "UPDATE trg SET n = 5 WHERE id = 1 USING OPERATION_ID 'op'",
            mismatch: Some("UPDATE trg SET n = 6 WHERE id = 1 USING OPERATION_ID 'op'"),
        },
        Case {
            label: "update by predicate",
            seed: &[SEED_TRG],
            sql: "UPDATE trg SET a = 'q' WHERE n > 15 AND n < 1000 USING OPERATION_ID 'op'",
            mismatch: Some(
                "UPDATE trg SET a = 'r' WHERE n > 15 AND n < 1000 USING OPERATION_ID 'op'",
            ),
        },
        Case {
            label: "delete by id",
            seed: &[SEED_TRG],
            sql: "DELETE FROM trg WHERE id = 1 USING OPERATION_ID 'op'",
            mismatch: Some("DELETE FROM trg WHERE id = 2 USING OPERATION_ID 'op'"),
        },
        Case {
            label: "delete by predicate",
            seed: &[SEED_TRG],
            sql: "DELETE FROM trg WHERE n > 15 USING OPERATION_ID 'op'",
            mismatch: Some("DELETE FROM trg WHERE n > 25 USING OPERATION_ID 'op'"),
        },
        Case {
            label: "truncate single",
            seed: &[SEED_TRG],
            sql: "TRUNCATE trg USING OPERATION_ID 'op'",
            mismatch: None,
        },
        Case {
            label: "truncate multi",
            seed: &[SEED_TRG, SEED_PLAIN],
            sql: "TRUNCATE trg, plain USING OPERATION_ID 'op'",
            mismatch: None,
        },
    ]
}

// (a) 全文種の再送マトリクス。再送は `23505`（DuplicateOperationId）で、トリガの呼び出し回数・
// 行・監査行・採番のいずれも変わらない。内容違いの再送は `22023` でトリガは呼ばれない。
// BEFORE ROW が NEW を書き換えていても、同じ文の再送は `22023` ではなく `23505`（書き換え前の値で照合）。
#[test]
fn replay_is_rejected_without_firing_triggers_for_every_statement_form() {
    for case in cases() {
        let dir = tempdir();
        let (core, calls) = build(&dir.path().join("db.redb"), true, effects(None));
        add_all_triggers(&core, "tenant-a");
        let a = ctx("tenant-a");
        for s in case.seed {
            run(&core, &a, s).unwrap_or_else(|e| panic!("{}: seed {s}: {e:?}", case.label));
        }
        run(&core, &a, case.sql).unwrap_or_else(|e| panic!("{}: first: {e:?}", case.label));
        let after_first = snap(&core, &calls, &a);
        assert!(after_first.calls > 0, "{}: triggers must fire", case.label);
        assert!(
            !after_first.audit.is_empty(),
            "{}: side effects must exist",
            case.label
        );

        let err = run(&core, &a, case.sql).expect_err(case.label);
        assert!(is_duplicate(&err), "{}: {err:?}", case.label);
        assert_eq!(err.wire_code(), "23505", "{}", case.label);
        assert_eq!(snap(&core, &calls, &a), after_first, "{}", case.label);

        if let Some(m) = case.mismatch {
            let err = run(&core, &a, m).expect_err(case.label);
            assert!(
                matches!(err, SqlSurfaceError::OperationIdContentMismatch),
                "{}: {err:?}",
                case.label
            );
            assert_eq!(snap(&core, &calls, &a), after_first, "{}", case.label);
        }

        // 再送が採番を進めていない: 新しい operation_id の書き込みの後も監査行の id に欠番が無い。
        run(&core, &a, NEXT_OP).unwrap_or_else(|e| panic!("{}: next: {e:?}", case.label));
        assert!(
            audit_gapless(&core, &a, "audit") && audit_gapless(&core, &a, "audit2"),
            "{}: audit ids must be gapless",
            case.label
        );
    }
}

// トリガ内の書き込みは台帳に記録されない（台帳は外側の文の 1 エントリだけ）。トリガが書いた表へ
// 同じ operation_id で書く文は成功する。
#[test]
fn trigger_side_writes_leave_no_ledger_entry() {
    let dir = tempdir();
    let (core, _calls) = build(&dir.path().join("db.redb"), true, effects(None));
    add_all_triggers(&core, "tenant-a");
    let a = ctx("tenant-a");
    run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
         USING OPERATION_ID 'same'",
    )
    .expect("outer");
    assert!(count(&core, &a, "audit") > 0);
    run(
        &core,
        &a,
        "INSERT INTO audit (embedding, a, n) VALUES ('[1.0,0.0]', 'direct', 1) \
         USING OPERATION_ID 'same'",
    )
    .expect("audit table has no entry for the trigger's writes");
}

// 入れ子（深さ 2）: `audit` の INSERT トリガが `audit2` へ書く。再送では内側のトリガも呼ばれない。
#[test]
fn nested_trigger_effects_are_not_repeated_on_replay() {
    let dir = tempdir();
    let (core, calls) = build(&dir.path().join("db.redb"), true, effects(None));
    add_all_triggers(&core, "tenant-a");
    add_trigger(
        &core.storage,
        "tenant-a",
        "audit",
        &def("g_nested", TriggerTiming::After, TriggerLevel::Row, INS),
    );
    let a = ctx("tenant-a");
    let sql = "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
               USING OPERATION_ID 'nest'";
    run(&core, &a, sql).expect("first");
    let first = snap(&core, &calls, &a);
    assert!(!first.audit2.is_empty(), "nested trigger must have fired");
    assert!(is_duplicate(&run(&core, &a, sql).expect_err("replay")));
    assert_eq!(snap(&core, &calls, &a), first);
}

// `LedgerMode::Ledgered` で operation_id を省略すると、トリガを呼ぶ前に `23502`。操作 id 付きなら成功し、
// トリガ内の書き込み（台帳なし）が `23502` にならない。
#[test]
fn required_ledger_mode_rejects_before_any_trigger() {
    let dir = tempdir();
    let (core, calls) = build(&dir.path().join("db.redb"), true, effects(None));
    let core = core.with_ledger_mode(LedgerMode::Ledgered);
    add_all_triggers(&core, "tenant-a");
    let a = ctx("tenant-a");
    let err = run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1)",
    )
    .expect_err("missing op id");
    assert_eq!(err.wire_code(), "23502");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(count(&core, &a, "trg"), 0);
    assert_eq!(count(&core, &a, "audit"), 0);
    run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
         USING OPERATION_ID 'req-1'",
    )
    .expect("with op id");
    assert!(calls.load(Ordering::SeqCst) > 0);
    assert!(audit_gapless(&core, &a, "audit"));
}

// 明示 Tx: 記録済みの operation_id の再送は `23505` で Tx が Failed（`25P02`）、トリガは呼ばれない。
// ROLLBACK した Tx の operation_id は台帳に残らず、再実行できる。
#[test]
fn explicit_transaction_replay_and_rollback() {
    let dir = tempdir();
    let (core, calls) = build(&dir.path().join("db.redb"), true, effects(None));
    add_all_triggers(&core, "tenant-a");
    let a = ctx("tenant-a");
    let sql1 = "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
                USING OPERATION_ID 'tx-1'";
    run(&core, &a, sql1).expect("autocommit");
    let first = snap(&core, &calls, &a);

    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&a, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(&a, &mut s, &mut txn, sql1)
        .expect_err("replay in tx");
    assert!(is_duplicate(&err), "{err:?}");
    let err = core
        .execute_sql_in_txn(&a, &mut s, &mut txn, "SELECT COUNT(*) FROM trg")
        .expect_err("failed tx");
    assert_eq!(err.wire_code(), "25P02");
    core.execute_sql_in_txn(&a, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(snap(&core, &calls, &a), first);

    // ROLLBACK した Tx の op は残らない。
    let sql2 = "INSERT INTO trg (id, embedding, a, n) VALUES (2, '[2.0,0.0]', 'y', 2) \
                USING OPERATION_ID 'tx-2'";
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&a, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&a, &mut s, &mut txn, sql2)
        .expect("in tx");
    core.execute_sql_in_txn(&a, &mut s, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert_eq!(snap(&core, &calls, &a).trg, first.trg);
    assert_eq!(snap(&core, &calls, &a).audit, first.audit);
    run(&core, &a, sql2).expect("retry after rollback");

    // COMMIT した Tx の op は自動コミットの再送でも `23505`。
    let sql3 = "INSERT INTO trg (id, embedding, a, n) VALUES (3, '[3.0,0.0]', 'z', 3) \
                USING OPERATION_ID 'tx-3'";
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&a, &mut s, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(&a, &mut s, &mut txn, sql3)
        .expect("in tx");
    core.execute_sql_in_txn(&a, &mut s, &mut txn, "COMMIT")
        .expect("commit");
    let committed = snap(&core, &calls, &a);
    assert!(is_duplicate(&run(&core, &a, sql3).expect_err("replay")));
    assert_eq!(snap(&core, &calls, &a), committed);
    assert!(audit_gapless(&core, &a, "audit"));
}

// 再オープン後の再送も `23505`（台帳は永続化されている）。トリガは呼ばれず、副作用は変わらない。
#[test]
fn replay_after_reopen_is_rejected() {
    let dir = tempdir();
    let path = dir.path().join("db.redb");
    let sql = "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1), \
               (2, '[2.0,0.0]', 'y', 2) USING OPERATION_ID 'reopen'";
    let a = ctx("tenant-a");
    let before = {
        let (core, calls) = build(&path, true, effects(None));
        add_all_triggers(&core, "tenant-a");
        run(&core, &a, sql).expect("first");
        snap(&core, &calls, &a)
    };
    let (core, calls) = build(&path, false, effects(None));
    assert!(is_duplicate(&run(&core, &a, sql).expect_err("replay")));
    let after = snap(&core, &calls, &a);
    assert_eq!(after.calls, 0);
    assert_eq!(after.trg, before.trg);
    assert_eq!(after.audit, before.audit);
}

// (c) 3 テナントの境界。同じ operation_id 文字列を使っても台帳は衝突せず、a の再送だけが `23505`。
// b・c の副作用は a の再送の影響を受けず、エラー文言は他テナントの有無で変わらない（存在のオラクルにならない）。
#[test]
fn tenant_boundary_for_trigger_side_effects_and_ledger() {
    let insert = "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
                  USING OPERATION_ID 'shared'";
    let (a, b, c) = (ctx("tenant-a"), ctx("tenant-b"), ctx("tenant-c"));

    // 他テナントも同じ op を使った世界。
    let dir = tempdir();
    let (core, calls) = build(&dir.path().join("db.redb"), true, effects(None));
    add_all_triggers(&core, "tenant-a");
    add_all_triggers(&core, "tenant-b");
    run(&core, &a, insert).expect("a");
    let calls_after_a = calls.load(Ordering::SeqCst);
    assert!(calls_after_a > 0);
    // a のトリガは b・c の表へ書かない。
    assert_eq!(count(&core, &b, "audit"), 0);
    assert_eq!(count(&core, &c, "audit"), 0);
    run(&core, &b, insert).expect("b uses the same op string");
    run(&core, &c, insert).expect("c (no triggers) uses the same op string");
    assert_eq!(calls.load(Ordering::SeqCst), calls_after_a * 2);
    let (sb, sc) = (snap(&core, &calls, &b), snap(&core, &calls, &c));
    assert_eq!(count(&core, &c, "audit"), 0, "c has no triggers");
    assert_eq!(count(&core, &a, "audit"), count(&core, &b, "audit"));

    let sa = snap(&core, &calls, &a);
    let err_with_others = run(&core, &a, insert).expect_err("replay");
    assert!(is_duplicate(&err_with_others));
    assert_eq!(snap(&core, &calls, &a), sa);
    assert_eq!(snap(&core, &calls, &b), sb);
    assert_eq!(snap(&core, &calls, &c), sc);

    // 他テナントが何もしていない世界。
    let dir2 = tempdir();
    let (core2, _calls2) = build(&dir2.path().join("db.redb"), true, effects(None));
    add_all_triggers(&core2, "tenant-a");
    run(&core2, &a, insert).expect("a only");
    let err_alone = run(&core2, &a, insert).expect_err("replay");

    for e in [&err_with_others, &err_alone] {
        let text = format!("{e} | {e:?}");
        assert!(
            !text.contains("tenant-b") && !text.contains("tenant-c"),
            "{text}"
        );
    }
    assert_eq!(err_with_others.to_string(), err_alone.to_string());
    assert_eq!(err_with_others.wire_code(), err_alone.wire_code());

    // 内容違いの `22023` も他テナントの有無で同一。
    let changed = insert.replace("'x'", "'CHANGED'");
    let m1 = run(&core, &a, &changed).expect_err("mismatch");
    let m2 = run(&core2, &a, &changed).expect_err("mismatch");
    assert!(matches!(m1, SqlSurfaceError::OperationIdContentMismatch));
    assert_eq!(m1.to_string(), m2.to_string());
}

// トリガ内の書き込みが失敗したとき（内部キーの指定・制約違反・自前のエラー）、エラーは他テナントの
// 識別子を含まず、文全体が取り消される（行・監査行・台帳が残らない）。内部キーは不在と同じ `42P01`。
#[test]
fn trigger_write_failures_do_not_leak_or_leave_side_effects() {
    let behaviors: Vec<(&str, Behavior)> = vec![
        (
            "42P01",
            Box::new(|c| {
                c.writes.insert_row("#1.audit", None, &audit_row(c))?;
                Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
            }),
        ),
        (
            "22000",
            Box::new(|c| {
                let mut r = audit_row(c);
                r[2] = Value::Null;
                c.writes.insert_row("audit", None, &r)?;
                Ok(TriggerOutcome::Proceed(c.new_row.to_vec()))
            }),
        ),
        (
            "22000",
            Box::new(|c| {
                c.writes.insert_row("audit", None, &audit_row(c))?;
                Err(SqlSurfaceError::InvalidInput {
                    detail: "rejected".to_string(),
                })
            }),
        ),
    ];
    for (code, behavior) in behaviors {
        let dir = tempdir();
        let (core, _calls) = build(&dir.path().join("db.redb"), true, behavior);
        add_all_triggers(&core, "tenant-a");
        let (a, b) = (ctx("tenant-a"), ctx("tenant-b"));
        run(
            &core,
            &b,
            "INSERT INTO audit (embedding, a, n) VALUES ('[1.0,0.0]', 'b-secret', 77)",
        )
        .expect("b data");
        let sql = "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1) \
                   USING OPERATION_ID 'fail'";
        let err = run(&core, &a, sql).expect_err(code);
        assert_eq!(err.wire_code(), code);
        let text = format!("{err} | {err:?}");
        for needle in ["tenant-b", "b-secret", "77"] {
            assert!(!text.contains(needle), "{code}: leaked {needle}: {text}");
        }
        assert_eq!(count(&core, &a, "trg"), 0, "{code}");
        assert_eq!(count(&core, &a, "audit"), 0, "{code}");
        assert_eq!(count(&core, &b, "audit"), 1, "{code}");
    }
}

// ---------------------------------------------------------------------------------------------
// (b) クラッシュ注入（自己再帰サブプロセス。`recovery::commit_boundary` と同じ方式）。
// ---------------------------------------------------------------------------------------------

const ABORT_DB_ENV: &str = "ENGINE_TRIGGER_LEDGER_CRASH_ABORT_DB";
const ABORT_AT_ENV: &str = "ENGINE_TRIGGER_LEDGER_CRASH_ABORT_AT";
const KILL_DB_ENV: &str = "ENGINE_TRIGGER_LEDGER_CRASH_KILL_DB";

fn test_path(name: &str) -> String {
    {
        // libtest の名前は crate 名を含まない（`core::...`）ため、先頭の crate 名を外す。
        let m = module_path!();
        let rest = m.split_once("::").map_or(m, |x| x.1);
        format!("{rest}::{name}")
    }
}

fn spawn_child(name: &str, env: &[(&str, &str)]) -> std::process::Child {
    let exe = std::env::current_exe().expect("current_exe");
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--exact")
        .arg(test_path(name))
        .arg("--nocapture")
        .arg("--test-threads=1")
        .stdout(std::process::Stdio::piped())
        // stderr は読まない。pipe のまま放置すると満杯で子がブロックしうるので捨てる。
        .stderr(std::process::Stdio::null());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.spawn().expect("spawn child")
}

fn wait_with_timeout(child: &mut std::process::Child, secs: u64) -> std::process::ExitStatus {
    let start = std::time::Instant::now();
    loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            return s;
        }
        if start.elapsed() > std::time::Duration::from_secs(secs) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not terminate within {secs}s");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// 中断位置ごとの子プロセスが流す SQL と、そのときの最終的な副作用の期待値。
struct AbortCase {
    at: &'static str,
    sql: &'static str,
    /// 親が事前に入れておく行数（トリガ定義前に入れる）。
    seeded: u64,
    /// 再送が成功したときの `trg` の行数。
    trg_after_retry: u64,
    /// 再送でトリガが呼ばれる回数（= 書かれる監査行数）。
    fired: u64,
}

const ABORT_CASES: &[AbortCase] = &[
    // 行ごとの AFTER の中（2 行 → AFTER ROW 2 回の 1 回目）。
    AbortCase {
        at: "AFTER ROW INSERT",
        sql: "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1), \
              (2, '[2.0,0.0]', 'y', 2) USING OPERATION_ID 'crash-1'",
        seeded: 0,
        trg_after_retry: 2,
        fired: 6, // BEFORE ROW 2 + AFTER ROW 2 + BEFORE STATEMENT 1 + AFTER STATEMENT 1
    },
    // 台帳の記録の後・行の書き込みの前（BEFORE STATEMENT の中）。
    AbortCase {
        at: "BEFORE STATEMENT INSERT",
        sql: "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1), \
              (2, '[2.0,0.0]', 'y', 2) USING OPERATION_ID 'crash-2'",
        seeded: 0,
        trg_after_retry: 2,
        fired: 6,
    },
    // TRUNCATE の AFTER STATEMENT の中（行は削除済みの txn 内）。
    AbortCase {
        at: "AFTER STATEMENT TRUNCATE",
        sql: "TRUNCATE trg USING OPERATION_ID 'crash-3'",
        seeded: 2,
        trg_after_retry: 0,
        fired: 2,
    },
];

/// 子の定義: `abort_at` で中断する invoker を持つコアで `sql` を流す。
fn abort_child(path: &str, at: &str) {
    let case = ABORT_CASES.iter().find(|c| c.at == at).expect("case");
    let (core, _calls) = build(Path::new(path), false, effects(Some(at.to_string())));
    let _ = run(&core, &ctx("tenant-a"), case.sql);
    // ここへ到達したのは中断が起きなかった不具合。親が判定できる固定マーカーを出す。
    println!("CHILD_REACHED_AFTER_ABORT");
    std::process::exit(1);
}

#[test]
fn crash_during_trigger_leaves_no_partial_effects() {
    if let Ok(path) = std::env::var(ABORT_DB_ENV) {
        let at = std::env::var(ABORT_AT_ENV).expect("abort at");
        abort_child(&path, &at);
    }
    for case in ABORT_CASES {
        let path = unique_db_path("trigger-ledger-abort");
        let _cleanup = CleanupGuard(path.clone());
        let a = ctx("tenant-a");
        {
            let (core, _calls) = build(&path, true, effects(None));
            if case.seeded > 0 {
                run(&core, &a, "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1), (2, '[2.0,0.0]', 'y', 2)")
                    .expect("seed");
            }
            add_all_triggers(&core, "tenant-a");
        }
        let path_str = path.to_str().expect("utf8 path");
        let mut child = spawn_child(
            "crash_during_trigger_leaves_no_partial_effects",
            &[(ABORT_DB_ENV, path_str), (ABORT_AT_ENV, case.at)],
        );
        let status = wait_with_timeout(&mut child, 60);
        let mut out = String::new();
        if let Some(mut o) = child.stdout.take() {
            let _ = std::io::Read::read_to_string(&mut o, &mut out);
        }
        assert!(
            !out.contains("CHILD_REACHED_AFTER_ABORT"),
            "{}: {out}",
            case.at
        );
        assert!(
            out.contains("CHILD_ABORTING"),
            "{}: abort not reached: {out}",
            case.at
        );
        assert!(!status.success(), "{}: {status:?}", case.at);
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            assert_eq!(status.signal(), Some(6), "{}: {status:?}", case.at);
        }

        // 再オープン後: 行・監査行・台帳のどれも残っていない。
        let (core, calls) = build(&path, false, effects(None));
        assert_eq!(count(&core, &a, "trg"), case.seeded, "{}", case.at);
        assert_eq!(count(&core, &a, "audit"), 0, "{}", case.at);
        // 台帳が残っていれば `23505` になる。残っていないので同じ SQL が成功し、トリガが 1 回分だけ発火する。
        run(&core, &a, case.sql).unwrap_or_else(|e| panic!("{}: retry: {e:?}", case.at));
        assert_eq!(
            calls.load(Ordering::SeqCst) as u64,
            case.fired,
            "{}",
            case.at
        );
        assert_eq!(count(&core, &a, "audit"), case.fired, "{}", case.at);
        assert_eq!(count(&core, &a, "trg"), case.trg_after_retry, "{}", case.at);
        assert!(is_duplicate(&run(&core, &a, case.sql).expect_err("replay")));
    }
}

/// 再オープン後の整合のオラクル。`trg` の id が 1..=K で連続し、監査行がちょうど K × 2
/// （BEFORE STATEMENT と AFTER ROW が 1 行ずつ）であることを確かめ、K を返す。
/// 「台帳あり・副作用なし」「副作用あり・行なし」のような半端な状態は `Err`。
fn oracle(core: &EngineCore, c: &PolicyContext) -> Result<u64, String> {
    let ids = id_col(core, c, "trg");
    if !is_contiguous(&ids) {
        return Err(format!("trg ids not contiguous: {ids:?}"));
    }
    let k = ids.len() as u64;
    let audit = count(core, c, "audit");
    if !audit_gapless(core, c, "audit") {
        return Err("audit ids have a gap".to_string());
    }
    if audit != k * 2 {
        return Err(format!("audit rows {audit} != 2 * trg rows {k}"));
    }
    Ok(k)
}

// オラクル自体が不整合を検出できること（空振り防止）。トリガ無しで片側だけを書いた状態を作る。
#[test]
fn oracle_detects_partial_states() {
    let a = ctx("tenant-a");
    let dir = tempdir();
    let (core, _calls) = build(&dir.path().join("db.redb"), true, effects(None));
    run(
        &core,
        &a,
        "INSERT INTO audit (embedding, a, n) VALUES ('[1.0,0.0]', 'orphan', 1)",
    )
    .expect("audit only");
    assert!(
        oracle(&core, &a).is_err(),
        "audit without row must be detected"
    );

    let dir = tempdir();
    let (core, _calls) = build(&dir.path().join("db.redb"), true, effects(None));
    run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1)",
    )
    .expect("row only");
    assert!(
        oracle(&core, &a).is_err(),
        "row without audit must be detected"
    );

    // 整合状態（トリガ経由）は通る。
    let dir = tempdir();
    let (core, _calls) = build(&dir.path().join("db.redb"), true, effects(None));
    use TriggerLevel::{Row, Statement};
    use TriggerTiming::{After, Before};
    add_trigger(&core.storage, "tenant-a", "trg", &def("a", After, Row, INS));
    add_trigger(
        &core.storage,
        "tenant-a",
        "trg",
        &def("b", Before, Statement, INS),
    );
    run(
        &core,
        &a,
        "INSERT INTO trg (id, embedding, a, n) VALUES (1, '[1.0,0.0]', 'x', 1)",
    )
    .expect("both");
    assert_eq!(oracle(&core, &a), Ok(1));
}

/// 子: トリガ付きの `INSERT ... USING OPERATION_ID 'trg-<n>'` を書き続け、成功のたびに印を出す。
fn kill_child(path: &str) {
    use std::io::Write as _;
    let (core, _calls) = build(Path::new(path), false, effects(None));
    let a = ctx("tenant-a");
    // 上限を置かず親の SIGKILL まで書き続ける（完走後の待機中に kill される空振り合格を防ぐ）。
    for n in 1..=u64::MAX {
        let sql = format!(
            "INSERT INTO trg (id, embedding, a, n) VALUES ({n}, '[1.0,0.0]', 'x', 1) \
             USING OPERATION_ID 'trg-{n}'"
        );
        if run(&core, &a, &sql).is_err() {
            std::process::exit(2);
        }
        println!("COMMITTED op=trg-{n}");
        let _ = std::io::stdout().flush();
    }
    // 到達不能（u64 を使い切るまで書き続ける）。万一到達したら親が検知できるよう異常終了する。
    std::process::exit(3);
}

// SIGKILL による中断。何件目で止まっても、再オープン後は行・監査行・台帳が整合し、確認済みの commit は
// すべて残る。再送は `23505` でトリガを呼ばず、次の operation_id は成功する。
#[test]
fn sigkill_during_trigger_writes_keeps_ledger_rows_and_side_effects_consistent() {
    if let Ok(path) = std::env::var(KILL_DB_ENV) {
        kill_child(&path);
    }
    let a = ctx("tenant-a");
    for round in 0..2u64 {
        let path = unique_db_path("trigger-ledger-kill");
        let _cleanup = CleanupGuard(path.clone());
        {
            let (core, _calls) = build(&path, true, effects(None));
            use TriggerLevel::{Row, Statement};
            use TriggerTiming::{After, Before};
            add_trigger(&core.storage, "tenant-a", "trg", &def("a", After, Row, INS));
            add_trigger(
                &core.storage,
                "tenant-a",
                "trg",
                &def("b", Before, Statement, INS),
            );
        }
        let mut child = spawn_child(
            "sigkill_during_trigger_writes_keeps_ledger_rows_and_side_effects_consistent",
            &[(KILL_DB_ENV, path.to_str().expect("utf8 path"))],
        );
        let stdout = child.stdout.take().expect("stdout");
        let (tx, rx) = std::sync::mpsc::channel::<u64>();
        let reader = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(Result::ok)
            {
                if let Some(n) = line.strip_prefix("COMMITTED op=trg-") {
                    if let Ok(n) = n.trim().parse::<u64>() {
                        let _ = tx.send(n);
                    }
                }
            }
        });
        // 開始同期点: 1 件以上 commit を確認できるまで待つ（できなければ fail-closed）。
        let first = rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .unwrap_or_else(|_| {
                let _ = child.kill();
                let _ = child.wait();
                panic!("child never reported a commit");
            });
        // 固定の待機の後に kill する（round ごとに待機を変えて中断位置をずらす）。
        std::thread::sleep(std::time::Duration::from_millis(100 + 150 * round));
        // kill 前に子が自然終了していたら（子が書き込みを続けられず終了した等）空振りの合格になるため fail-closed。
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "round {round}: child exited before being killed"
        );
        child.kill().expect("kill child");
        let status = child.wait().expect("wait child");
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt as _;
            assert_eq!(status.signal(), Some(9), "round {round}: not SIGKILLed");
        }
        #[cfg(not(unix))]
        assert!(!status.success(), "round {round}: child exited cleanly");
        let _ = reader.join();
        let last_seen = std::iter::once(first)
            .chain(rx.try_iter())
            .max()
            .expect("max");

        let (core, calls) = build(&path, false, effects(None));
        let k = oracle(&core, &a).unwrap_or_else(|e| panic!("round {round}: {e}"));
        assert!(
            k >= last_seen,
            "round {round}: lost commit: K={k} last_seen={last_seen}"
        );
        // 確認済みの op はすべて再送が `23505` で、トリガは呼ばれない。
        for n in 1..=k {
            let sql = format!(
                "INSERT INTO trg (id, embedding, a, n) VALUES ({n}, '[1.0,0.0]', 'x', 1) \
                 USING OPERATION_ID 'trg-{n}'"
            );
            let err = run(&core, &a, &sql).expect_err("replay");
            assert!(is_duplicate(&err), "round {round}: n={n}: {err:?}");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "round {round}");
        assert_eq!(oracle(&core, &a), Ok(k));
        // 中断された次の op（K+1）は成功し、副作用がちょうど 2 行増える。
        let next = k + 1;
        run(
            &core,
            &a,
            &format!(
                "INSERT INTO trg (id, embedding, a, n) VALUES ({next}, '[1.0,0.0]', 'x', 1) \
                 USING OPERATION_ID 'trg-{next}'"
            ),
        )
        .unwrap_or_else(|e| panic!("round {round}: next op: {e:?}"));
        assert_eq!(oracle(&core, &a), Ok(next));
    }
}

// 内部キーが `SqlOutcome` に漏れないことの型確認用（未使用警告の回避）。
#[allow(dead_code)]
fn _assert_outcome_used(_: SqlOutcome) {}
