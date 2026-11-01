//! 書き込みロック待機上限の CLI 注入（`EngineCore::with_write_lock_wait`。Issue #1554・
//! SQL-46 ポインタ）の結合テスト。wire-server の `--write-lock-wait-ms` が通る経路
//! （`validate_write_lock_wait_ms` → `EngineCore::with_write_lock_wait`）が `Storage` の
//! 待機上限まで届き、超過時に従来どおり `55P03` を返すことを固定する。
//!
//! 境界: 上限ちょうど（60,000 ms）は実時間で待てないため `validate_*` のユニットテスト
//! （`storage.rs`）で固定し、挙動側は下限ちょうど（100 ms）で固定する。
//! `sql31_transaction.rs` と同じ流儀（実 `Storage`・チャネル同期・holder を先に解放して join）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{validate_write_lock_wait_ms, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn new_core(wait_ms: u64) -> (Arc<EngineCore>, std::path::PathBuf) {
    let path = unique_db_path("write-lock-wait");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_write_lock_wait(validate_write_lock_wait_ms(wait_ms).expect("in range"));
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

/// holder が `BEGIN` ＋最初の書き込み文（Issue #2400）で writer を握り、解放シグナル受信後に
/// `ROLLBACK` する。明示 Tx の writer は `BEGIN` ではなく最初の書き込み文で取得される。
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

/// T1: 下限ちょうど（100 ms）。既定 30 秒のままなら満たせない短時間で `55P03`。
#[test]
fn lowered_wait_times_out_quickly_with_55p03() {
    let (engine, path) = new_core(100);
    let _cleanup = CleanupGuard(path);
    let (holder, release) = spawn_holder(&engine);

    let started = Instant::now();
    let res = engine.execute_sql_in_session(
        &ctx("tenant-b"),
        &mut SessionState::default(),
        &insert_sql(1, "op-1"),
    );
    let elapsed = started.elapsed();

    release.send(()).expect("release");
    holder.join().expect("holder");

    let err = res.expect_err("must time out");
    assert_eq!(err.wire_code(), "55P03");
    assert!(elapsed >= Duration::from_millis(100), "elapsed={elapsed:?}");
    assert!(elapsed < Duration::from_secs(10), "elapsed={elapsed:?}");
}

/// T2: 上げた待機上限の範囲内で holder が解放されれば待っていた INSERT は成功する。
#[test]
fn raised_wait_lets_blocked_insert_succeed_after_release() {
    let (engine, path) = new_core(5_000);
    let _cleanup = CleanupGuard(path);
    let (holder, release) = spawn_holder(&engine);

    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        release.send(()).expect("release");
    });
    let res = engine.execute_sql_in_session(
        &ctx("tenant-b"),
        &mut SessionState::default(),
        &insert_sql(1, "op-1"),
    );
    releaser.join().expect("releaser");
    holder.join().expect("holder");
    res.expect("insert succeeds once writer is released within the wait");
}

/// T3: 待つ側が明示トランザクションなら `BEGIN` は待たず成功し、最初の書き込み文が `55P03`
/// になる（Issue #2400）。`Failed` 後は `25P02`、`ROLLBACK` だけが成功する。
#[test]
fn explicit_first_write_times_out_with_55p03() {
    let (engine, path) = new_core(100);
    let _cleanup = CleanupGuard(path);
    let (holder, release) = spawn_holder(&engine);

    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    let begin_started = Instant::now();
    let begin = engine.execute_sql_in_txn(&ctx("tenant-b"), &mut session, &mut txn, "BEGIN");
    let begin_elapsed = begin_started.elapsed();
    let write = engine.execute_sql_in_txn(
        &ctx("tenant-b"),
        &mut session,
        &mut txn,
        &insert_sql(1, "op-t3"),
    );
    let after = engine.execute_sql_in_txn(&ctx("tenant-b"), &mut session, &mut txn, "SELECT 1");
    let rollback = engine.execute_sql_in_txn(&ctx("tenant-b"), &mut session, &mut txn, "ROLLBACK");

    release.send(()).expect("release");
    holder.join().expect("holder");
    begin.expect("BEGIN does not wait for the writer");
    assert!(
        begin_elapsed < Duration::from_millis(100),
        "begin_elapsed={begin_elapsed:?}"
    );
    assert_eq!(write.expect_err("must time out").wire_code(), "55P03");
    assert_eq!(after.expect_err("failed txn").wire_code(), "25P02");
    rollback.expect("rollback");
}

/// T3b: 待つ側が暗黙トランザクションなら、writer は開始時に取るため `begin_implicit_transaction`
/// 自体が待機上限で `55P03` になる（Issue #2401。SQL-31・WIRE-16）。`Idle` のまま 1 文も実行されない。
#[test]
fn implicit_begin_times_out_with_55p03_at_start() {
    let (engine, path) = new_core(100);
    let _cleanup = CleanupGuard(path);
    let (holder, release) = spawn_holder(&engine);

    let mut session = SessionState::default();
    let mut txn = engine.new_session_transaction();
    let started = Instant::now();
    let begin = engine.begin_implicit_transaction(&mut txn, &session);
    let elapsed = started.elapsed();
    let status = txn.status();

    release.send(()).expect("release");
    holder.join().expect("holder");
    assert_eq!(begin.expect_err("must time out").wire_code(), "55P03");
    assert!(elapsed >= Duration::from_millis(100), "elapsed={elapsed:?}");
    assert_eq!(status, engine::sql::transaction::TransactionStatus::Idle);

    engine
        .begin_implicit_transaction(&mut txn, &session)
        .expect("begin implicit after release");
    txn.commit_implicit(&mut session).expect("commit");
}

/// T4: 構築経路（`open`）に関係なく設定が効き、検索エンジン種別は変わらない。
#[test]
fn builder_applies_to_opened_core_without_changing_engine_kind() {
    let path = unique_db_path("write-lock-wait-open");
    let _cleanup = CleanupGuard(path.clone());
    let core = EngineCore::open(&path).expect("open");
    let kind = core.search_engine_kind();
    assert_eq!(
        core.write_lock_wait(),
        engine::storage::DEFAULT_WRITE_LOCK_WAIT
    );
    let core = core.with_write_lock_wait(Duration::from_millis(250));
    assert_eq!(core.write_lock_wait(), Duration::from_millis(250));
    assert_eq!(core.search_engine_kind(), kind);
}
