//! 明示トランザクション内の DDL と持続時間の上限（SQL-80・SQL-31・SQL-71・TABLE-22 のポインタ、Issue #3314。
//! 設計記録: `docs/design/explicit-transaction.md`「#3314 の実装記録」・`docs/design/transaction-limits-cli.md`）の
//! 時間依存テスト。
//!
//! `TransactionLimits::max_duration` を小さく取り、(1) 上限を過ぎた後の DDL が `54000` になること、
//! (2) 既存行を走査する DDL が走査の途中で Tx の期限に当たって `54000`・復帰不可の `Failed` になり痕跡を
//! 残さないこと、(3) 共有 writer の待機が Tx の残り時間で打ち切られることを固定する。時間計測に依存する
//! ため、他のテストと束ねない単独のバイナリにしている（`.claude/rules/coding-rust.md`）。判定は SQLSTATE と
//! 「何も残らない」ことだけにし、時間の揺らぎでは結果が変わらない形（どの段で期限に当たっても `54000`）にする。

use std::time::{Duration, Instant};

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionLimits, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str, max_duration: Duration) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_transaction_limits(TransactionLimits {
            max_duration,
            max_statements: 1000,
        });
    (core, CleanupGuard(path))
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn tx<'a>(
    core: &'a EngineCore,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_txn(&ctx(), session, txn, sql)
}

fn auto(core: &EngineCore, session: &mut SessionState, sql: &str) {
    core.execute_sql_in_session(&ctx(), session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
}

#[test]
fn ddl_after_the_duration_limit_is_54000_and_leaves_nothing() {
    let (core, _g) = new_core("sql80-deadline-after", Duration::from_millis(150));
    let mut session = ddl_session();
    let mut txn = core.new_session_transaction();
    tx(&core, &mut session, &mut txn, "BEGIN").expect("begin");
    std::thread::sleep(Duration::from_millis(400));
    let err = tx(
        &core,
        &mut session,
        &mut txn,
        "CREATE TABLE t (id BIGINT PRIMARY KEY)",
    )
    .expect_err("duration limit");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    let _ = tx(&core, &mut session, &mut txn, "ROLLBACK");
    let err = core
        .execute_sql_in_session(&ctx(), &mut session, "SELECT id FROM t")
        .expect_err("table was never created");
    assert_eq!(err.wire_code(), "42P01");
}

#[test]
fn scan_ddl_hits_the_transaction_deadline_mid_scan() {
    // BEGIN の直後に上限が来るよう極小にし、行数の多い表への一意索引の走査が確認点で打ち切られるようにする。
    // 文の開始時点の検査・走査中の確認点のどちらで当たっても `54000`・復帰不可の `Failed`・痕跡なしになる。
    let (core, _g) = new_core("sql80-deadline-scan", Duration::from_millis(3));
    let mut session = ddl_session();
    auto(
        &core,
        &mut session,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, k BIGINT)",
    );
    for chunk in 0..20u64 {
        let values: Vec<String> = (0..1000u64)
            .map(|i| {
                let n = chunk * 1000 + i + 1;
                format!("({n}, {n})")
            })
            .collect();
        auto(
            &core,
            &mut session,
            &format!(
                "INSERT INTO t (id, k) VALUES {} USING OPERATION_ID 'seed-{chunk}'",
                values.join(", ")
            ),
        );
    }
    let mut txn = core.new_session_transaction();
    tx(&core, &mut session, &mut txn, "BEGIN").expect("begin");
    let err = tx(
        &core,
        &mut session,
        &mut txn,
        "CREATE UNIQUE INDEX t_k_u ON t (k)",
    )
    .expect_err("deadline during the scan");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    // 復帰不可の失敗状態は ROLLBACK で戻り、何も確定しない。
    tx(&core, &mut session, &mut txn, "ROLLBACK").expect("rollback");
    assert_eq!(txn.status(), TransactionStatus::Idle);
    // 索引は残っていない（同じ値の挿入が通る）。
    auto(
        &core,
        &mut session,
        "INSERT INTO t (id, k) VALUES (999999, 1) USING OPERATION_ID 'after-scan'",
    );
}

#[test]
fn writer_wait_is_cut_by_the_transaction_remaining_time() {
    let (core, _g) = new_core("sql80-deadline-writer", Duration::from_millis(400));
    let mut holder_session = ddl_session();
    let mut holder = core.new_session_transaction();
    tx(&core, &mut holder_session, &mut holder, "BEGIN").expect("begin");
    tx(
        &core,
        &mut holder_session,
        &mut holder,
        "CREATE TABLE held (id BIGINT PRIMARY KEY)",
    )
    .expect("holder takes the writer");
    // 別スレッド（同一スレッドの再取得は待たずに `55P03` になる）の Tx の DDL は writer を待ち、
    // 既定の待機上限ではなく Tx の残り時間で打ち切られて `54000` になる。
    let (code, status, elapsed) = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let mut session = ddl_session();
                let mut txn = core.new_session_transaction();
                tx(&core, &mut session, &mut txn, "BEGIN").expect("begin");
                let started = Instant::now();
                let err = tx(
                    &core,
                    &mut session,
                    &mut txn,
                    "CREATE TABLE waiting (id BIGINT PRIMARY KEY)",
                )
                .expect_err("writer wait");
                (err.wire_code(), txn.status(), started.elapsed())
            })
            .join()
            .expect("thread")
    });
    assert_eq!(code, "54000");
    assert_eq!(status, TransactionStatus::Failed);
    assert!(
        elapsed < Duration::from_secs(3),
        "wait was bounded by the Tx limit: {elapsed:?}"
    );
    tx(&core, &mut holder_session, &mut holder, "ROLLBACK").expect("rollback holder");
}
