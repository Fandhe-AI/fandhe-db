//! 明示トランザクションの writer 遅延取得（Issue #2400）のクレート内テスト。
//!
//! `SessionTransaction::write_txn()`（`pub(crate)`）で writer の有無を直接観測する。対象ビヘイビアは
//! PERSIST-4・SQL-31・TABLE-3（ポインタ表記のみ。設計記録は `docs/design/explicit-transaction.md`）。
//! 結合側（他セッションの待機・`55P03`）は `tests/sql_t01_39/sql31_deferred_writer.rs` が担う。

use super::EngineCore;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::sql::transaction::TransactionStatus;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

fn new_core(tag: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    for name in ["documents", "docs2"] {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("n", ColumnType::BigInt, false),
                ],
            ))
            .expect("create table");
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert(id: u64, op: &str) -> String {
    format!(
        "INSERT INTO documents (id, embedding, n) VALUES ({id}, '[1.0, 0.0]', 1) \
         USING OPERATION_ID '{op}'"
    )
}

/// 読み取りだけの間は writer を持たず、最初の書き込み文で取得する（AC2）。
/// 書き込み文の種類ごとに新しい Tx で確かめる。
#[test]
fn first_write_statement_acquires_writer_for_each_dml_kind() {
    let (core, _g) = new_core("deferred-writer-kinds");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    core.execute_sql_in_session(&caller, &mut session, &insert(1, "seed"))
        .expect("seed");

    let writes = [
        insert(2, "w-insert"),
        "UPDATE documents SET n = 2 WHERE id = 1 USING OPERATION_ID 'w-update'".to_string(),
        "DELETE FROM documents WHERE id = 1 USING OPERATION_ID 'w-delete'".to_string(),
        "TRUNCATE TABLE documents USING OPERATION_ID 'w-trunc'".to_string(),
        "INSERT INTO docs2 (id, embedding, n) SELECT id, embedding, n FROM documents \
         USING OPERATION_ID 'w-insel'"
            .to_string(),
    ];
    for sql in &writes {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        assert!(txn.write_txn().is_none(), "BEGIN must not take the writer");
        core.execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "SELECT id FROM documents LIMIT 10",
        )
        .expect("select");
        assert!(txn.write_txn().is_none(), "a read must not take the writer");
        core.execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
        assert!(txn.write_txn().is_some(), "{sql} must take the writer");
        core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
            .expect("rollback");
        assert!(txn.write_txn().is_none());
    }
}

/// 読み取り・カーソルだけの Tx は write txn を 1 つも作らない（AC1 の構造的な証跡）。
#[test]
fn read_only_explicit_txn_creates_no_write_txn() {
    let (core, _g) = new_core("deferred-writer-readonly");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    core.execute_sql_in_session(&caller, &mut session, &insert(1, "seed"))
        .expect("seed");
    let before = core.storage.write_txn_creations();
    let mut txn = core.new_session_transaction();
    for sql in [
        "BEGIN",
        "SELECT id FROM documents LIMIT 10",
        "DECLARE c CURSOR FOR SELECT id FROM documents LIMIT 10",
        "FETCH 1 FROM c",
        "COMMIT",
    ] {
        core.execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    }
    assert_eq!(core.storage.write_txn_creations(), before);
    assert_eq!(txn.status(), TransactionStatus::Idle);
}

/// DDL はトランザクション内で `0A000` となり、writer を取る前に拒否される。
#[test]
fn ddl_in_explicit_txn_does_not_take_writer() {
    let (core, _g) = new_core("deferred-writer-ddl");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &caller,
            &mut session,
            &mut txn,
            "CREATE TABLE t9 (id BIGINT PRIMARY KEY)",
        )
        .expect_err("DDL is rejected");
    assert_eq!(err.wire_code(), "0A000");
    assert!(txn.write_txn().is_none());
    assert_eq!(txn.status(), TransactionStatus::Failed);
}

/// 一時表だけを触る Tx は永続側の writer を取らない。
#[test]
fn temp_table_statements_do_not_take_persistent_writer() {
    let (core, _g) = new_core("deferred-writer-temp");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &caller,
        &mut session,
        "CREATE TEMP TABLE tt (id BIGINT PRIMARY KEY, n INTEGER)",
    )
    .expect("create temp");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &caller,
        &mut session,
        &mut txn,
        "INSERT INTO tt (id, n) VALUES (1, 1)",
    )
    .expect("temp insert");
    core.execute_sql_in_txn(
        &caller,
        &mut session,
        &mut txn,
        "SELECT id FROM tt LIMIT 10",
    )
    .expect("temp select");
    assert!(txn.write_txn().is_none());
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "COMMIT")
        .expect("commit");
}

/// 暗黙トランザクションは従来どおり開始時に writer を取る（#2401 が前提とする）。
#[test]
fn implicit_txn_still_takes_writer_at_begin() {
    let (core, _g) = new_core("deferred-writer-implicit");
    let mut txn = core.new_session_transaction();
    txn.begin_implicit(&core.storage, &SessionState::default())
        .expect("begin_implicit");
    assert!(txn.write_txn().is_some());
    txn.abort_implicit();
    assert!(txn.write_txn().is_none());
}

/// テナント A の読み取りだけの Tx 中にテナント B が書いても、B の書き込みは待たされず成功する。
#[test]
fn other_tenant_write_during_read_only_txn_is_not_blocked() {
    let (core, _g) = new_core("deferred-writer-tenant");
    let a = ctx("tenant-a");
    let b = ctx("tenant-b");
    let mut session_a = SessionState::default();
    let mut session_b = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&a, &mut session_a, &mut txn, "BEGIN")
        .expect("begin");
    core.execute_sql_in_txn(
        &a,
        &mut session_a,
        &mut txn,
        "SELECT id FROM documents LIMIT 10",
    )
    .expect("select");
    core.execute_sql_in_session(&b, &mut session_b, &insert(7, "b-1"))
        .expect("tenant-b write is not blocked");
    core.execute_sql_in_txn(&a, &mut session_a, &mut txn, "COMMIT")
        .expect("commit");
}
