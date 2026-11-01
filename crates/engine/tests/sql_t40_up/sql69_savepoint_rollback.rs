//! `ROLLBACK TO` による永続データ表の巻き戻し（Issue #1943・
//! `docs/spec/04-behavior/sql-surface.md` SQL-69 のポインタ）の結合テスト。
//!
//! `EngineCore::execute_sql_in_txn` を production 経路として、redb の ephemeral savepoint への
//! 結線（writer 取得前に作った savepoint への巻き戻し）を固定する。受け入れ条件は
//! (a) `ROLLBACK TO` の後にその savepoint 以降の書き込みが見えないこと、(b) `COMMIT` の後も
//! 巻き戻した書き込みが永続化されないこと。writer が dirty になった後に作った savepoint は
//! redb では戻せないため `0A000` で fail-closed になることと、`ROLLBACK TO` の後も writer を
//! Tx の終了まで保持すること（SQL-69 (3)）も合わせて固定する。時間計測には依存しない
//! （`55P03` は待機上限で判定する）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility, WriteDurability};

use crate::temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";

fn new_core_with(tag: &str, durability: Option<WriteDurability>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = match durability {
        Some(d) => Storage::open_with_durability(&path, d).expect("open storage"),
        None => Storage::open(&path).expect("open storage"),
    };
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_write_lock_wait(std::time::Duration::from_millis(100));
    (core, guard)
}

fn new_core(tag: &str) -> (EngineCore, CleanupGuard) {
    new_core_with(tag, None)
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

const SELECT: &str = "SELECT id FROM documents ORDER BY embedding <=> '[1.0, 0.0]' LIMIT 10";

fn rows_of(outcome: SqlOutcome) -> usize {
    match outcome {
        SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// 別セッション（Tx なし）から見える確定済みの行数。
fn committed_rows(engine: &EngineCore, caller: &PolicyContext) -> usize {
    rows_of(
        engine
            .execute_sql_in_session(caller, &mut SessionState::default(), SELECT)
            .expect("select"),
    )
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

fn rows_in_txn<'e>(
    engine: &'e EngineCore,
    caller: &PolicyContext,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
) -> usize {
    rows_of(run(engine, caller, session, txn, SELECT).expect("select in txn"))
}

/// 受け入れ条件 (a)(b) の本体（durability 構成ごとに再利用する）。
fn assert_rollback_to_hides_and_commit_does_not_persist(core: &EngineCore) {
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    run(core, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    run(core, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    run(core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(core, &caller, &mut s, &mut txn, &insert_sql(2, "op-2")).unwrap();
    assert_eq!(rows_in_txn(core, &caller, &mut s, &mut txn), 2);
    assert_eq!(
        run(core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap(),
        SqlOutcome::Rollback
    );
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    // (a) savepoint 以降の書き込みが見えない。
    assert_eq!(rows_in_txn(core, &caller, &mut s, &mut txn), 0);
    // 巻き戻し後の書き込みは通常どおり行える。
    run(core, &caller, &mut s, &mut txn, &insert_sql(3, "op-3")).unwrap();
    run(core, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    // (b) 巻き戻した書き込みは永続化されず、後の書き込みだけが残る。
    assert_eq!(committed_rows(core, &caller), 1);
}

#[test]
fn rollback_to_hides_writes_and_commit_does_not_persist_them() {
    let (core, _g) = new_core("sql69-rb-basic");
    assert_rollback_to_hides_and_commit_does_not_persist(&core);
}

#[test]
fn rollback_to_works_without_durability() {
    let (core, _g) = new_core_with("sql69-rb-nodur", Some(WriteDurability::None));
    assert_rollback_to_hides_and_commit_does_not_persist(&core);
}

#[test]
fn rolling_back_to_same_savepoint_twice_succeeds() {
    let (core, _g) = new_core("sql69-rb-twice");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    run(&core, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    run(&core, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    run(&core, &caller, &mut s, &mut txn, &insert_sql(2, "op-2")).unwrap();
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    assert_eq!(rows_in_txn(&core, &caller, &mut s, &mut txn), 0);
    run(&core, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    assert_eq!(committed_rows(&core, &caller), 0);
}

#[test]
fn nested_savepoints_before_writes_restore_and_drop_newer_ones() {
    let (core, _g) = new_core("sql69-rb-nested");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    run(&core, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    run(&core, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    run(&core, &caller, &mut s, &mut txn, "SAVEPOINT b").unwrap();
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO b").unwrap();
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    // `ROLLBACK TO a` で b は除かれている。
    let err = run(&core, &caller, &mut s, &mut txn, "RELEASE b").unwrap_err();
    assert_eq!(err.wire_code(), "3B001");
}

#[test]
fn savepoint_established_after_writes_is_0a000_and_rollback_leaves_no_rows() {
    let (core, _g) = new_core("sql69-rb-after-writes");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    run(&core, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(&core, &caller, &mut s, &mut txn, "SAVEPOINT c").unwrap();
    let err = run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO c").unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK").unwrap();
    assert_eq!(committed_rows(&core, &caller), 0);
}

#[test]
fn mixed_restorable_and_unrestorable_savepoints() {
    let (core, _g) = new_core("sql69-rb-mixed");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    run(&core, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    run(&core, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(&core, &caller, &mut s, &mut txn, "SAVEPOINT b").unwrap();
    let err = run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO b").unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK").unwrap();

    // 別の Tx では a へ戻せる。
    run(&core, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    run(&core, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-2")).unwrap();
    run(&core, &caller, &mut s, &mut txn, "SAVEPOINT b").unwrap();
    // b は Unrestorable。a へ直接戻すのは可能（b は除かれる）。
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    assert_eq!(rows_in_txn(&core, &caller, &mut s, &mut txn), 0);
    run(&core, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    assert_eq!(committed_rows(&core, &caller), 0);
}

#[test]
fn writer_is_held_until_transaction_end_after_rollback_to() {
    let (core, _g) = new_core("sql69-rb-writer-held");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    run(&core, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    run(&core, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    // writer は手放さない: 別セッションの書き込みは待機上限で 55P03。
    let mut other = SessionState::default();
    let err = core
        .execute_sql_in_session(&ctx("tenant-b"), &mut other, &insert_sql(9, "other-1"))
        .expect_err("writer is still held");
    assert_eq!(err.wire_code(), "55P03");
    run(&core, &caller, &mut s, &mut txn, "COMMIT").unwrap();
    core.execute_sql_in_session(&ctx("tenant-b"), &mut other, &insert_sql(9, "other-2"))
        .expect("writer released after commit");
}

/// writer をテーブル操作で先に取った場合、redb 上は dirty になるため probe が `Unrestorable` と
/// 判定し、`ROLLBACK TO` は `0A000` で fail-closed になる（engine の記録に頼らない判定の確認）。
#[test]
fn savepoint_taken_after_lock_table_is_unrestorable_and_fails_closed() {
    let (core, _g) = new_core("sql69-rb-lock-first");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    run(&core, &caller, &mut s, &mut txn, "BEGIN").unwrap();
    run(
        &core,
        &caller,
        &mut s,
        &mut txn,
        &format!("LOCK TABLE {TABLE}"),
    )
    .unwrap();
    run(&core, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    run(&core, &caller, &mut s, &mut txn, &insert_sql(1, "op-1")).unwrap();
    let err = run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap_err();
    assert_eq!(err.wire_code(), "0A000");
    assert_eq!(txn.status(), TransactionStatus::Failed);
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK").unwrap();
    assert_eq!(committed_rows(&core, &caller), 0);
}

#[test]
fn rollback_to_in_read_only_transaction_succeeds() {
    let (core, _g) = new_core("sql69-rb-readonly");
    let caller = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    run(&core, &caller, &mut s, &mut txn, "BEGIN READ ONLY").unwrap();
    run(&core, &caller, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    run(&core, &caller, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    run(&core, &caller, &mut s, &mut txn, "COMMIT").unwrap();
}

#[test]
fn rollback_to_does_not_touch_other_tenants_committed_rows() {
    let (core, _g) = new_core("sql69-rb-tenant");
    let (a, b) = (ctx("tenant-a"), ctx("tenant-b"));
    let mut sb = SessionState::default();
    core.execute_sql_in_session(&b, &mut sb, &insert_sql(100, "b-1"))
        .expect("tenant-b write");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    run(&core, &a, &mut s, &mut txn, "BEGIN").unwrap();
    run(&core, &a, &mut s, &mut txn, "SAVEPOINT a").unwrap();
    run(&core, &a, &mut s, &mut txn, &insert_sql(1, "a-1")).unwrap();
    run(&core, &a, &mut s, &mut txn, "ROLLBACK TO a").unwrap();
    // tenant-a の Tx からは自テナントの行も他テナントの行も見えない。
    assert_eq!(rows_in_txn(&core, &a, &mut s, &mut txn), 0);
    run(&core, &a, &mut s, &mut txn, "COMMIT").unwrap();
    // tenant-b の確定済みの行は消えず、tenant-a の巻き戻した行は見えない。
    assert_eq!(committed_rows(&core, &b), 1);
    assert_eq!(committed_rows(&core, &a), 0);
}
