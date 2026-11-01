//! 明示トランザクションの writer 遅延取得（Issue #2400）と、暗黙トランザクションが従来どおり
//! 開始時に writer を取る経路（Issue #2401）のクレート内テスト。
//!
//! `SessionTransaction::write_txn()`（`pub(crate)`）で writer の有無を直接観測する。対象ビヘイビアは
//! PERSIST-4・SQL-31・TABLE-3・WIRE-16（ポインタ表記のみ。設計記録は `docs/design/explicit-transaction.md`）。
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

/// `BEGIN READ ONLY` の各読み取り文の後も writer を持たず、スナップショットだけを保持する
/// （Issue #1933・SQL-38）。COMMIT・ROLLBACK で両方手放す。
#[test]
fn read_only_txn_holds_only_read_snapshot() {
    let (core, _g) = new_core("deferred-writer-ro-snapshot");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    core.execute_sql_in_session(&caller, &mut session, &insert(1, "seed"))
        .expect("seed");
    for end in ["COMMIT", "ROLLBACK"] {
        let mut txn = core.new_session_transaction();
        for sql in [
            "BEGIN READ ONLY",
            "SELECT id FROM documents LIMIT 10",
            "DECLARE c CURSOR FOR SELECT id FROM documents LIMIT 10",
            "FETCH 1 FROM c",
            "EXPLAIN SELECT id FROM documents LIMIT 10",
        ] {
            core.execute_sql_in_txn(&caller, &mut session, &mut txn, sql)
                .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
            assert!(txn.write_txn().is_none(), "{sql} must not take the writer");
            assert!(txn.read_snapshot().is_some(), "{sql}: snapshot is held");
        }
        core.execute_sql_in_txn(&caller, &mut session, &mut txn, end)
            .expect("end");
        assert!(txn.write_txn().is_none());
        assert!(txn.read_snapshot().is_none());
    }
}

/// 別セッション（同テナント・他テナント）が書き込み・commit を繰り返す間も、`BEGIN READ ONLY` の Tx は
/// writer を取らず、`write_txn` の生成数（choke point の証跡）に寄与しない（Issue #1935・SQL-38）。
/// 書き込み文の `25006` 拒否の後も同様。対照として通常の `BEGIN` は最初の書き込み文で生成数を増やす。
/// 結合側の並行・スナップショット一貫性は `tests/sql_t01_39/sql38_read_only_concurrency.rs` が担う。
#[test]
fn read_only_txn_never_takes_writer_while_others_commit() {
    let (core, _g) = new_core("deferred-writer-ro-others");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    core.execute_sql_in_session(&caller, &mut session, &insert(1, "seed"))
        .expect("seed");
    let mut txn = core.new_session_transaction();
    let mut ro_session = SessionState::default();
    core.execute_sql_in_txn(&caller, &mut ro_session, &mut txn, "BEGIN READ ONLY")
        .expect("begin read only");
    let others = [
        ("tenant-a", 10u64),
        ("tenant-b", 11),
        ("tenant-a", 12),
        ("tenant-b", 13),
    ];
    for (tenant, id) in others {
        // 他セッションの書き込みは成功し、生成数を増やす。
        let before = core.storage.write_txn_creations();
        core.execute_sql_in_session(&ctx(tenant), &mut session, &insert(id, &format!("o-{id}")))
            .expect("other session write");
        assert!(core.storage.write_txn_creations() > before);

        // READ ONLY 側の読み取り・カーソル・EXPLAIN は生成数に寄与しない。
        let before = core.storage.write_txn_creations();
        for sql in [
            "SELECT id FROM documents LIMIT 10",
            "DECLARE c CURSOR FOR SELECT id FROM documents LIMIT 10",
            "FETCH 1 FROM c",
            "CLOSE c",
            "EXPLAIN SELECT id FROM documents LIMIT 10",
        ] {
            core.execute_sql_in_txn(&caller, &mut ro_session, &mut txn, sql)
                .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
            assert!(txn.write_txn().is_none(), "{sql} must not take the writer");
            assert!(txn.read_snapshot().is_some(), "{sql}: snapshot is held");
        }
        assert_eq!(core.storage.write_txn_creations(), before);
    }

    // 書き込み文は 25006 で拒否され、writer を取らない。
    let before = core.storage.write_txn_creations();
    let err = core
        .execute_sql_in_txn(&caller, &mut ro_session, &mut txn, &insert(99, "ro-w"))
        .expect_err("25006");
    assert_eq!(err.wire_code(), "25006");
    assert!(txn.write_txn().is_none());
    assert_eq!(core.storage.write_txn_creations(), before);

    core.execute_sql_in_txn(&caller, &mut ro_session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert!(txn.write_txn().is_none());
    assert!(txn.read_snapshot().is_none());

    // 対照: 通常の BEGIN は最初の書き込み文で writer を取る。
    let mut plain = core.new_session_transaction();
    let mut plain_session = SessionState::default();
    core.execute_sql_in_txn(&caller, &mut plain_session, &mut plain, "BEGIN")
        .expect("begin");
    let before = core.storage.write_txn_creations();
    core.execute_sql_in_txn(
        &caller,
        &mut plain_session,
        &mut plain,
        &insert(98, "plain-w"),
    )
    .expect("plain write");
    assert!(plain.write_txn().is_some());
    assert!(core.storage.write_txn_creations() > before);
    core.execute_sql_in_txn(&caller, &mut plain_session, &mut plain, "ROLLBACK")
        .expect("rollback");
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

/// 暗黙 Tx の writer は開始時にちょうど 1 回だけ取得し、最初の書き込み文で取り直さない（Issue #2401。
/// SQL-31・WIRE-16）。明示 Tx は `BEGIN` で 0 回、最初の書き込み文で 1 回となる対照も同じ場所で示す。
#[test]
fn implicit_txn_acquires_writer_once_at_begin_and_not_at_first_write() {
    let (core, _g) = new_core("deferred-writer-implicit-once");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    core.execute_sql_in_session(&caller, &mut session, &insert(1, "seed"))
        .expect("seed");

    let before = core.storage.write_txn_creations();
    let mut txn = core.new_session_transaction();
    core.begin_implicit_transaction(&mut txn, &session)
        .expect("begin implicit");
    assert_eq!(core.storage.write_txn_creations(), before + 1);
    assert!(
        txn.write_txn().is_some(),
        "writer is held before any statement"
    );

    for sql in [
        "SELECT id FROM documents LIMIT 10".to_string(),
        insert(2, "imp-insert"),
        "UPDATE documents SET n = 3 WHERE id = 1 USING OPERATION_ID 'imp-update'".to_string(),
        "DELETE FROM documents WHERE id = 2 USING OPERATION_ID 'imp-delete'".to_string(),
    ] {
        core.execute_sql_in_txn(&caller, &mut session, &mut txn, &sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
        assert_eq!(
            core.storage.write_txn_creations(),
            before + 1,
            "{sql} must not re-acquire the writer"
        );
        assert!(txn.write_txn().is_some());
    }
    txn.commit_implicit(&mut session).expect("commit implicit");
    assert!(txn.write_txn().is_none());
    assert_eq!(txn.status(), TransactionStatus::Idle);

    // 対照: 明示 Tx は BEGIN で 0 回、最初の書き込み文で 1 回。
    let before = core.storage.write_txn_creations();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    assert_eq!(core.storage.write_txn_creations(), before);
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, &insert(5, "exp-insert"))
        .expect("first write");
    assert_eq!(core.storage.write_txn_creations(), before + 1);
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
}

/// 暗黙 Tx の開始直後の読み取りスナップショットは、新たに開いた確定済み読み取りと同じ世代を指す
/// （Issue #2401。snapshot は writer 取得後に開かれる）。
#[test]
fn implicit_txn_snapshot_matches_committed_state_at_begin() {
    use redb::ReadableDatabase;
    let (core, _g) = new_core("deferred-writer-implicit-gen");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    core.execute_sql_in_session(&caller, &mut session, &insert(1, "seed"))
        .expect("seed");
    let mut txn = core.new_session_transaction();
    core.begin_implicit_transaction(&mut txn, &session)
        .expect("begin implicit");
    let read_gen = |r: &redb::ReadTransaction| {
        r.open_table(crate::storage::GENERATION_TABLE)
            .expect("open generation table")
            .get("generation")
            .expect("get")
            .map(|v| v.value())
    };
    let fresh = core.storage.db().begin_read().expect("begin_read");
    let snapshot = txn.read_snapshot().expect("snapshot");
    assert_eq!(read_gen(snapshot), read_gen(&fresh));
    txn.abort_implicit();
}

/// 読み取り源の三分岐（SQL-19・SQL-31・Issue #2403）。取得前は `BEGIN` 時点のスナップショット、
/// 書き込み後は共有 write txn、`Idle` は fail-closed（`XX000`）。
#[test]
fn read_source_switches_between_begin_snapshot_and_writer() {
    use crate::sql::transaction::TxnReadSource;
    let (core, _g) = new_core("deferred-writer-read-source");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    assert_eq!(
        txn.read_source().err().map(|e| e.wire_code().to_string()),
        Some("XX000".to_string()),
        "Idle must fail closed"
    );
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    assert!(matches!(
        txn.read_source(),
        Ok(TxnReadSource::BeginSnapshot(_))
    ));
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, &insert(1, "w1"))
        .expect("insert");
    assert!(matches!(txn.read_source(), Ok(TxnReadSource::Writer(_))));
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
    assert!(txn.read_source().is_err());
}
