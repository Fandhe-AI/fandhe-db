//! 書き込みロック（writer gate）の待機を文の期限の内に数える結線（Issue #1953。SQL-71 (3)・SQL-31・
//! SQL-46 のポインタ）の結合テスト。
//!
//! 別スレッドが明示トランザクションで writer を握っている間に、本スレッドで期限つきの書き込みを
//! 実行する。待機上限（`write_lock_wait`）が長くても、文の期限が先に来れば `55P03` ではなく
//! `57014`（固定文言）を返し、期限が無い・十分先・待機上限の方が先なら従来どおり `55P03` を返す。
//! 実時間の待機に依存するため、他のテストと束ねず `tests/` 直下に単独で置く（README「engine の結合テストの
//! 束ね方と個別実行」）。経過時間の上限は緩く（10 秒）確認する。

use std::sync::Arc;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::interrupt::{DeadlineScope, StatementDeadline};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::TransactionStatus;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";
const TIMEOUT_MSG: &str = "canceling statement due to statement timeout";

fn new_core(wait: Duration) -> (Arc<EngineCore>, std::path::PathBuf) {
    let path = unique_db_path("sql1953-lock-wait");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_write_lock_wait(wait);
    (Arc::new(core), path)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_sql(id: u64, op_id: &str) -> String {
    format!(
        "INSERT INTO {TABLE} (id, embedding) VALUES ({id}, '[1.0, 0.0]') USING OPERATION_ID '{op_id}'"
    )
}

/// holder が `BEGIN` ＋最初の書き込み文で writer を握り、解放シグナル受信後に `ROLLBACK` する。
fn spawn_holder(
    engine: &Arc<EngineCore>,
) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Sender<()>) {
    let (began_tx, began_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let e = Arc::clone(engine);
    let h = std::thread::spawn(move || {
        let caller = ctx("tenant-a");
        let mut session = SessionState::default();
        let mut txn = e.new_session_transaction();
        e.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        e.execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            &insert_sql(900, "op-holder"),
        )
        .expect("first write holds writer_gate");
        began_tx.send(()).expect("notify");
        release_rx.recv().expect("release signal");
        e.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback");
    });
    began_rx.recv().expect("holder began");
    (h, release_tx)
}

/// SQL-71 (3): 待機上限（30 秒）より先に文の期限（約 250 ms）が来たら、autocommit の書き込みは
/// `55P03` ではなく `57014`（固定文言）を返す。
#[test]
fn deadline_before_write_lock_wait_returns_57014_for_autocommit_write() {
    let (engine, path) = new_core(Duration::from_secs(30));
    let _cleanup = CleanupGuard(path);
    let (holder, release) = spawn_holder(&engine);

    let started = Instant::now();
    let res = {
        let _scope = DeadlineScope::install(StatementDeadline::after(
            Instant::now(),
            Duration::from_millis(250),
        ));
        engine.execute_sql_in_session(
            &ctx("tenant-b"),
            &mut SessionState::default(),
            &insert_sql(1, "op-1"),
        )
    };
    let elapsed = started.elapsed();

    release.send(()).expect("release");
    holder.join().expect("holder");

    let err = res.expect_err("must time out by the statement deadline");
    assert_eq!(err.wire_code(), "57014");
    assert_eq!(err.to_string(), TIMEOUT_MSG);
    assert!(elapsed < Duration::from_secs(10), "elapsed={elapsed:?}");
    // 期限で止まった書き込みは何も残さない（同じ operation_id を解放後に再送できる）。
    engine
        .execute_sql_in_session(
            &ctx("tenant-b"),
            &mut SessionState::default(),
            &insert_sql(1, "op-1"),
        )
        .expect("resend after the holder released");
}

/// SQL-71 (3)・SQL-31: 明示トランザクションの最初の書き込み文での writer 取得も期限内に数える。
/// 期限切れは `57014` になり、トランザクションは `Failed` になる。
#[test]
fn deadline_before_write_lock_wait_returns_57014_for_first_write_in_explicit_transaction() {
    let (engine, path) = new_core(Duration::from_secs(30));
    let _cleanup = CleanupGuard(path);
    let (holder, release) = spawn_holder(&engine);

    let caller = ctx("tenant-b");
    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin does not wait for the writer");
    let started = Instant::now();
    let res = {
        let _scope = DeadlineScope::install(StatementDeadline::after(
            Instant::now(),
            Duration::from_millis(250),
        ));
        engine.execute_sql_in_txn(&caller, &mut session, &mut txn, &insert_sql(2, "op-2"))
    };
    let elapsed = started.elapsed();
    let status = txn.status();
    engine
        .execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");

    release.send(()).expect("release");
    holder.join().expect("holder");

    let err = res.expect_err("must time out by the statement deadline");
    assert_eq!(err.wire_code(), "57014");
    assert_eq!(err.to_string(), TIMEOUT_MSG);
    assert_eq!(status, TransactionStatus::Failed);
    assert!(elapsed < Duration::from_secs(10), "elapsed={elapsed:?}");
}

/// 期限が無い・待機上限（100 ms）より後ろ（1 時間先）の場合は、従来どおり待機上限で `55P03`。
#[test]
fn write_lock_wait_still_wins_without_a_deadline_or_with_a_far_one() {
    let (engine, path) = new_core(Duration::from_millis(100));
    let _cleanup = CleanupGuard(path);
    let (holder, release) = spawn_holder(&engine);

    let run = |op: &str| {
        engine.execute_sql_in_session(
            &ctx("tenant-b"),
            &mut SessionState::default(),
            &insert_sql(3, op),
        )
    };
    let no_deadline = run("op-3a");
    let far = {
        let _scope = DeadlineScope::install(StatementDeadline::after(
            Instant::now(),
            Duration::from_secs(3600),
        ));
        run("op-3b")
    };

    release.send(()).expect("release");
    holder.join().expect("holder");

    assert_eq!(no_deadline.expect_err("timeout").wire_code(), "55P03");
    assert_eq!(far.expect_err("timeout").wire_code(), "55P03");
}
