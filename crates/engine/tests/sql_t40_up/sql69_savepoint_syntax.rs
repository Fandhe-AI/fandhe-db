//! `SAVEPOINT`／`RELEASE`／`ROLLBACK TO` の構文と savepoint スタック（Issue #1942・
//! `docs/spec/04-behavior/sql-surface.md` SQL-69 のポインタ）の結合テスト。
//!
//! `EngineCore::execute_sql_in_txn` を production 経路として、Idle での `25P01`、同名の遮蔽、
//! 存在しない名前の `3B001`、書き込み後に作った（redb で巻き戻せない）savepoint への
//! `ROLLBACK TO` が fail-closed（`0A000`＋`Failed`）で後続の `COMMIT` に書き込みを確定させない
//! こと、セッション分離・文数上限への計上を固定する。巻き戻しの成功経路は
//! `sql69_savepoint_rollback`（Issue #1943）が固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionLimits, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql69-savepoint");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
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

fn visible_rows(engine: &EngineCore, caller: &PolicyContext) -> usize {
    let outcome = engine
        .execute_sql_in_session(
            caller,
            &mut SessionState::default(),
            &format!("SELECT id FROM {TABLE} ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10"),
        )
        .expect("select");
    match outcome {
        SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn run<'e>(
    engine: &'e EngineCore,
    caller: &PolicyContext,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    engine.execute_sql_in_txn(caller, session, txn, sql)
}

#[test]
fn savepoint_statements_outside_transaction_are_25p01_and_keep_idle() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = engine.new_session_transaction();
    for sql in [
        "SAVEPOINT a",
        "RELEASE a",
        "RELEASE SAVEPOINT a",
        "ROLLBACK TO a",
        "ROLLBACK TO SAVEPOINT a",
    ] {
        let err = run(&engine, &caller, &mut s, &mut txn, sql).unwrap_err();
        assert_eq!(err.wire_code(), "25P01", "{sql}");
        assert_eq!(txn.status(), TransactionStatus::Idle, "{sql}");
    }
}

#[test]
fn same_name_savepoints_shadow_and_release_in_order() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = engine.new_session_transaction();
    run(&engine, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    run(&engine, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    assert_eq!(
        run(&engine, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap(),
        SqlOutcome::Savepoint
    );
    run(&engine, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    assert_eq!(
        run(&engine, &caller, &mut s, &mut txn, "RELEASE a").unwrap(),
        SqlOutcome::ReleaseSavepoint
    );
    // 古い側がまだ残っている。
    run(&engine, &caller, &mut s, &mut txn, "RELEASE SAVEPOINT a").unwrap();
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    assert_eq!(
        run(&engine, &caller, &mut s, &mut txn, "COMMIT").unwrap(),
        SqlOutcome::Commit
    );
    assert_eq!(visible_rows(&engine, &caller), 1);
}

#[test]
fn release_unknown_name_is_3b001_and_fails_transaction() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = engine.new_session_transaction();
    run(&engine, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    run(&engine, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(&engine, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    let err = run(&engine, &caller, &mut s, &mut txn, "RELEASE b").unwrap_err();
    assert_eq!(err.wire_code(), "3B001");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    let err = run(&engine, &caller, &mut s, &mut txn, "COMMIT").unwrap_err();
    assert_eq!(err.wire_code(), "25P02");
    run(&engine, &caller, &mut s, &mut txn, "ROLLBACK").unwrap();
    assert_eq!(txn.status(), TransactionStatus::Idle);
    assert_eq!(visible_rows(&engine, &caller), 0);
}

/// Issue #2879: 書き込み後の savepoint へ戻せる。savepoint 前の行は COMMIT で残り、後の行は残らない。
#[test]
fn rollback_to_savepoint_after_writes_keeps_only_earlier_rows() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = engine.new_session_transaction();
    run(&engine, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    run(&engine, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(&engine, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    run(&engine, &caller, &mut s, &mut txn, &insert_sql(2, "op-2")).unwrap();
    run(&engine, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    run(&engine, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    assert_eq!(visible_rows(&engine, &caller), 1);
}

#[test]
fn rollback_to_unknown_name_is_3b001_and_failed_state_is_25p02() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = engine.new_session_transaction();
    run(&engine, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    let err = run(&engine, &caller, &mut s, &mut txn, "ROLLBACK TO b").unwrap_err();
    assert_eq!(err.wire_code(), "3B001");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    // savepoint が無いので Failed 中の `ROLLBACK TO` は復帰できず 25P02（Issue #1947 の保持条件外）。
    for sql in [
        "ROLLBACK TO a",
        "ROLLBACK WORK TO SAVEPOINT a",
        "SAVEPOINT a",
    ] {
        let err = run(&engine, &caller, &mut s, &mut txn, sql).unwrap_err();
        assert_eq!(err.wire_code(), "25P02", "{sql}");
        assert_eq!(txn.status(), TransactionStatus::Failed, "{sql}");
    }
    run(&engine, &caller, &mut s, &mut txn, "ROLLBACK").unwrap();
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

#[test]
fn release_pops_newer_savepoints_too() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = engine.new_session_transaction();
    run(&engine, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    run(&engine, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    run(&engine, &caller, &mut s, &mut txn, "SAVEPOINT b").unwrap();
    run(&engine, &caller, &mut s, &mut txn, "RELEASE a").unwrap();
    let err = run(&engine, &caller, &mut s, &mut txn, "RELEASE b").unwrap_err();
    assert_eq!(err.wire_code(), "3B001");
}

#[test]
fn savepoint_is_allowed_in_read_only_transaction() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = engine.new_session_transaction();
    run(&engine, &caller, &mut s, &mut txn, "BEGIN READ ONLY").unwrap();
    run(&engine, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    run(&engine, &caller, &mut s, &mut txn, "RELEASE a").unwrap();
    run(&engine, &caller, &mut s, &mut txn, "COMMIT").unwrap();
}

#[test]
fn malformed_savepoint_statements_are_42601() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = engine.new_session_transaction();
    run(&engine, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    // 構文誤りは Tx を Failed にする（既存の文拒否と同じ契約）が、状態遷移は `42601` が先。
    for sql in [
        "SAVEPOINT",
        "SAVEPOINT select",
        "SAVEPOINT a b",
        "RELEASE",
        "ROLLBACK TO",
    ] {
        let err = run(&engine, &caller, &mut s, &mut txn, sql).unwrap_err();
        assert_eq!(err.wire_code(), "42601", "{sql}");
        run(&engine, &caller, &mut s, &mut txn, "ROLLBACK").unwrap();
        run(&engine, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    }
}

#[test]
fn savepoints_are_isolated_per_session_transaction() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let (a, b) = (ctx("tenant-a"), ctx("tenant-b"));
    let (mut sa, mut sb) = (SessionState::default(), SessionState::default());
    let mut ta = engine.new_session_transaction();
    let mut tb = engine.new_session_transaction();
    run(&engine, &a, &mut sa, &mut ta, "BEGIN").unwrap();
    run(&engine, &b, &mut sb, &mut tb, "BEGIN").unwrap();
    run(&engine, &a, &mut sa, &mut ta, "SAVEPOINT a").unwrap();
    let err = run(&engine, &b, &mut sb, &mut tb, "RELEASE a").unwrap_err();
    assert_eq!(err.wire_code(), "3B001");
    run(&engine, &a, &mut sa, &mut ta, "RELEASE a").unwrap();
}

/// savepoint 時点のセッション状態（TimeZone・search_path・セッション UDF）が `ROLLBACK TO` で
/// 復元される（PR #2863 レビュー指摘の回帰。writer 未取得でも成功する経路）。
#[test]
fn rollback_to_savepoint_restores_session_state() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    s.allow_ddl();
    let tz_before = format!("{:?}", s.time_zone());
    let sp_before = s.search_path().display();
    let mut txn = engine.new_session_transaction();
    run(&engine, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    run(&engine, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    run(
        &engine,
        &caller,
        &mut s,
        &mut txn,
        "SET TIME ZONE 'Asia/Tokyo'",
    )
    .unwrap();
    run(
        &engine,
        &caller,
        &mut s,
        &mut txn,
        "SET search_path = other_schema",
    )
    .unwrap();
    // Tx ブロック内の `CREATE FUNCTION` は拒否されるため、Tx オブジェクトを介さず同一セッションへ
    // 登録する（セッション UDF が復元対象であることだけを検証する）。
    engine
        .execute_sql_in_session(&caller, &mut s, "CREATE FUNCTION f_sp(v) AS vec_norm(v)")
        .unwrap();
    assert_ne!(format!("{:?}", s.time_zone()), tz_before);
    assert_ne!(s.search_path().display(), sp_before);
    assert!(!s.udfs().is_empty());
    run(&engine, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    assert_eq!(format!("{:?}", s.time_zone()), tz_before);
    assert_eq!(s.search_path().display(), sp_before);
    assert!(s.udfs().is_empty());
    run(&engine, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    assert_eq!(format!("{:?}", s.time_zone()), tz_before);
}

#[test]
fn savepoint_statements_count_toward_statement_limit() {
    let (engine, path) = new_core();
    let _c = CleanupGuard(path);
    let engine = engine.with_transaction_limits(TransactionLimits {
        max_duration: std::time::Duration::from_secs(20),
        max_statements: 3,
    });
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = engine.new_session_transaction();
    run(&engine, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    for _ in 0..3 {
        run(&engine, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    }
    let err = run(&engine, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap_err();
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
}
