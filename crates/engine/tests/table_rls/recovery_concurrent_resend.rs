//! 同一 `operation_id` の並行再送で二重適用が起きないことの結合テスト
//! （Issue #2067。ポインタ: RECOVER-3・RECOVER-10、`docs/spec/04-behavior/recovery.md`）。
//!
//! 既存の `recovery_content_hash.rs`・`recovery_ledger.rs` は再送を逐次に行う前提で固定している。
//! 本ファイルは別接続（スレッドごとに別の `SessionState`）の 2 スレッドが同じテナント・同じ
//! `operation_id` で同時に書く場合を扱う。台帳の照合と記録は書き込み txn の中で行われ、txn は
//! redb の排他ロックで直列化されるため、成功は 1 件だけで行が 2 回適用されないことを固定する。
//!
//! 方針: 開始は `Barrier` で揃え、sleep・実時間の閾値は使わない。想定外のエラー
//! （`55P03` を含む）は fail-closed ですべてテスト失敗とする。スレッド数（2）と繰り返し回数
//! （100）は定数で上限化する。pg wire 経由・NoSQL 経路の並行は対象外。

use std::sync::{Arc, Barrier};

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::{LastOperationLookup, LedgerLookup};
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{validate_write_lock_wait_ms, Storage, Visibility, MAX_WRITE_LOCK_WAIT_MS};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

/// 競合を繰り返す回数。
const ITERATIONS: usize = 100;
/// 両スレッドで共通のテナント（異なると台帳が分かれ重複にならない）。
const TENANT: &str = "tenant-a";

type SqlResult = Result<SqlOutcome, SqlSurfaceError>;

/// 台帳モードを Ledgered にした engine を開く。`operation_id` の付け忘れを `23502` で
/// 検出するため。ガードは core より先に束縛し、core が先に drop されるようにする。
fn open_engine(label: &str, ddl: &[&str]) -> (CleanupGuard, Arc<EngineCore>) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let wait = validate_write_lock_wait_ms(MAX_WRITE_LOCK_WAIT_MS).expect("valid wait");
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
    .with_ledger_mode(LedgerMode::Ledgered)
    .with_write_lock_wait(wait);
    for sql in ddl {
        let mut session = SessionState::default();
        session.allow_ddl();
        core.execute_sql_in_session(&ctx("sys"), &mut session, sql)
            .unwrap_or_else(|e| panic!("ddl {sql}: {e:?}"));
    }
    (guard, Arc::new(core))
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 呼び出しごとに新しい `SessionState`（= 別接続）で 1 文を実行する。
fn run(core: &EngineCore, sql: &str) -> SqlResult {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(TENANT), &mut session, sql)
}

/// 2 本の文を `Barrier` で開始を揃えた 2 スレッドから実行する。
fn race(core: &Arc<EngineCore>, sql_a: &str, sql_b: &str) -> (SqlResult, SqlResult) {
    let barrier = Barrier::new(2);
    std::thread::scope(|s| {
        let spawn = |sql: &str| {
            let core = Arc::clone(core);
            let barrier = &barrier;
            let sql = sql.to_string();
            s.spawn(move || {
                barrier.wait();
                run(&core, &sql)
            })
        };
        let ha = spawn(sql_a);
        let hb = spawn(sql_b);
        (
            ha.join().expect("thread A panicked"),
            hb.join().expect("thread B panicked"),
        )
    })
}

/// 勝者の位置（`true` = A が成功）と敗者のエラーを返す。成功がちょうど 1 件でなければ失敗。
fn split_one_ok(i: usize, a: SqlResult, b: SqlResult) -> (bool, SqlSurfaceError) {
    match (a, b) {
        (Ok(_), Err(e)) => (true, e),
        (Err(e), Ok(_)) => (false, e),
        (a, b) => panic!("iteration {i}: expected exactly one success, got A={a:?} B={b:?}"),
    }
}

fn query_cells(core: &EngineCore, sql: &str) -> Vec<Cell> {
    let SqlOutcome::Query(q) = run(core, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}")) else {
        panic!("{sql}: expected Query outcome");
    };
    q.rows.into_iter().map(|r| r.cells[0].clone()).collect()
}

fn rows_for_k(core: &EngineCore, i: usize) -> Vec<String> {
    query_cells(
        core,
        &format!("SELECT payload FROM ev WHERE k = {i} LIMIT 10"),
    )
    .into_iter()
    .map(|c| match c {
        Cell::Text(s) => s,
        other => panic!("unexpected cell {other:?}"),
    })
    .collect()
}

fn n_for_k(core: &EngineCore, i: usize) -> i64 {
    let cells = query_cells(core, &format!("SELECT n FROM ctr WHERE k = {i} LIMIT 10"));
    assert_eq!(cells.len(), 1, "iteration {i}: expected one row: {cells:?}");
    match &cells[0] {
        Cell::SignedInteger(n) => *n,
        other => panic!("unexpected cell {other:?}"),
    }
}

/// 台帳が当該 op を記録済みで、直前に成功した書き込みがその op であること。
/// 各イテレーションは逐次なので last_operation_id が直前の op と一致する。
fn assert_ledger_once(core: &EngineCore, table: &str, op: &str) {
    let id = OperationId::parse(op).expect("valid operation_id");
    assert_eq!(
        core.operation_recorded(&ctx(TENANT), table, &id)
            .expect("lookup"),
        LedgerLookup::Recorded,
        "{op}"
    );
    assert_eq!(
        core.last_operation_id(&ctx(TENANT), table).expect("lookup"),
        LastOperationLookup::Committed(id),
        "{op}"
    );
}

fn assert_duplicate(r: SqlResult, what: &str) {
    match r {
        Err(e @ SqlSurfaceError::DuplicateOperationId) => assert_eq!(e.wire_code(), "23505"),
        other => panic!("{what}: expected DuplicateOperationId, got {other:?}"),
    }
}

fn assert_mismatch(r: SqlResult, what: &str) {
    match r {
        Err(e @ SqlSurfaceError::OperationIdContentMismatch) => assert_eq!(e.wire_code(), "22023"),
        other => panic!("{what}: expected OperationIdContentMismatch, got {other:?}"),
    }
}

const EV_DDL: &[&str] = &["CREATE TABLE ev (k BIGINT, payload TEXT)"];
const CTR_DDL: &[&str] = &["CREATE TABLE ctr (k BIGINT PRIMARY KEY, n BIGINT)"];

fn ins(i: usize, payload: &str, op: &str) -> String {
    format!("INSERT INTO ev (k, payload) VALUES ({i}, '{payload}') USING OPERATION_ID '{op}'")
}

fn upd(i: usize, add: i64, op: &str) -> String {
    format!("UPDATE ctr SET n = n + {add} WHERE k = {i} USING OPERATION_ID '{op}'")
}

fn seed_ctr(core: &EngineCore) {
    let mut start = 0;
    while start < ITERATIONS {
        let end = (start + 64).min(ITERATIONS);
        let values: Vec<String> = (start..end).map(|k| format!("({k}, 0)")).collect();
        run(
            core,
            &format!(
                "INSERT INTO ctr (k, n) VALUES {} USING OPERATION_ID 'seed-{start}'",
                values.join(", ")
            ),
        )
        .expect("seed");
        start = end;
    }
}

#[test]
fn concurrent_insert_same_content_applies_once() {
    let (_g, core) = open_engine("recover-concurrent-ins-same", EV_DDL);
    for i in 0..ITERATIONS {
        let op = format!("ins-same-{i}");
        let sql = ins(i, "same", &op);
        let (a, b) = race(&core, &sql, &sql);
        let (_, err) = split_one_ok(i, a, b);
        assert!(
            matches!(err, SqlSurfaceError::DuplicateOperationId),
            "iteration {i}: loser must be a ledger duplicate, got {err:?}"
        );
        assert_eq!(err.wire_code(), "23505");
        assert_eq!(
            rows_for_k(&core, i),
            vec!["same".to_string()],
            "iteration {i}"
        );
        assert_ledger_once(&core, "ev", &op);
        assert_duplicate(run(&core, &sql), &op);
        assert_eq!(rows_for_k(&core, i).len(), 1, "iteration {i}");
    }
    let total = query_cells(&core, "SELECT k FROM ev LIMIT 1000").len();
    assert_eq!(total, ITERATIONS);
}

#[test]
fn concurrent_insert_different_content_applies_winner_only() {
    let (_g, core) = open_engine("recover-concurrent-ins-diff", EV_DDL);
    for i in 0..ITERATIONS {
        let op = format!("ins-diff-{i}");
        let (sa, sb) = (ins(i, "left", &op), ins(i, "right", &op));
        let (a, b) = race(&core, &sa, &sb);
        let (a_won, err) = split_one_ok(i, a, b);
        assert!(
            matches!(err, SqlSurfaceError::OperationIdContentMismatch),
            "iteration {i}: loser must be a content mismatch, got {err:?}"
        );
        assert_eq!(err.wire_code(), "22023");
        let msg = err.client_message();
        for leak in ["left", "right", TENANT] {
            assert!(!msg.contains(leak), "iteration {i}: message leaks {leak}");
        }
        let (win, lose, win_payload) = if a_won {
            (&sa, &sb, "left")
        } else {
            (&sb, &sa, "right")
        };
        assert_eq!(
            rows_for_k(&core, i),
            vec![win_payload.to_string()],
            "iteration {i}"
        );
        assert_ledger_once(&core, "ev", &op);
        assert_duplicate(run(&core, win), &op);
        assert_mismatch(run(&core, lose), &op);
        assert_eq!(
            rows_for_k(&core, i),
            vec![win_payload.to_string()],
            "iteration {i}"
        );
    }
}

#[test]
fn concurrent_update_same_content_applies_once() {
    let (_g, core) = open_engine("recover-concurrent-upd-same", CTR_DDL);
    seed_ctr(&core);
    for i in 0..ITERATIONS {
        let op = format!("upd-same-{i}");
        let sql = upd(i, 1, &op);
        let (a, b) = race(&core, &sql, &sql);
        let (_, err) = split_one_ok(i, a, b);
        assert!(
            matches!(err, SqlSurfaceError::DuplicateOperationId),
            "iteration {i}: loser must be a ledger duplicate, got {err:?}"
        );
        assert_eq!(err.wire_code(), "23505");
        assert_eq!(n_for_k(&core, i), 1, "iteration {i}: double applied");
        assert_ledger_once(&core, "ctr", &op);
        assert_duplicate(run(&core, &sql), &op);
        assert_eq!(n_for_k(&core, i), 1, "iteration {i}");
    }
}

#[test]
fn concurrent_update_different_content_applies_winner_only() {
    let (_g, core) = open_engine("recover-concurrent-upd-diff", CTR_DDL);
    seed_ctr(&core);
    for i in 0..ITERATIONS {
        let op = format!("upd-diff-{i}");
        let (sa, sb) = (upd(i, 1, &op), upd(i, 2, &op));
        let (a, b) = race(&core, &sa, &sb);
        let (a_won, err) = split_one_ok(i, a, b);
        assert!(
            matches!(err, SqlSurfaceError::OperationIdContentMismatch),
            "iteration {i}: loser must be a content mismatch, got {err:?}"
        );
        assert_eq!(err.wire_code(), "22023");
        let (win, lose, expect) = if a_won { (&sa, &sb, 1) } else { (&sb, &sa, 2) };
        assert_eq!(n_for_k(&core, i), expect, "iteration {i}");
        assert_ledger_once(&core, "ctr", &op);
        assert_duplicate(run(&core, win), &op);
        assert_mismatch(run(&core, lose), &op);
        assert_eq!(n_for_k(&core, i), expect, "iteration {i}");
    }
}
