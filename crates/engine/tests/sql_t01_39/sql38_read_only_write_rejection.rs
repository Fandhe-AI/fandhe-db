//! `BEGIN READ ONLY` 内の書き込み系の文の `25006` 拒否（SQL-38 (3)・ERR-6・Issue #1934）の
//! 結合テスト。ポインタ: `docs/spec/04-behavior/` SQL-38・error-format。
//!
//! `EngineCore::execute_sql_in_txn`／`begin_copy_in_txn` を production 経路として検証する。
//! 受け入れ条件は (a) 書き込み文の全種類が `25006`、(b) 拒否してもデータ（行・カタログ・採番）と
//! operation_id 台帳が変化しないこと。READ ONLY の Tx は writer を一度も取らないため、別セッションの
//! 書き込みを待たせないことも併せて確かめる。

use engine::core::{CopyPlan, EngineCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::LedgerMode;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionStatus};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);
    let mut s = ddl_session();
    for sql in [
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
        "CREATE VIEW v AS SELECT id, name FROM t",
        "CREATE SEQUENCE sq",
        "INSERT INTO t (id, name) VALUES (1, 'a'), (2, 'b')",
    ] {
        core.execute_sql_in_session(&ctx(), &mut s, sql)
            .unwrap_or_else(|e| panic!("{sql}: {} {e}", e.wire_code()));
    }
    (core, guard)
}

fn run<'a>(
    core: &'a EngineCore,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'a>,
    sql: &str,
) -> Result<SqlOutcome, String> {
    core.execute_sql_in_txn(&ctx(), session, txn, sql)
        .map_err(|e| e.wire_code().to_string())
}

/// 状態の指紋（行・採番・カタログ）。拒否の前後で同一であること。
fn fingerprint(core: &EngineCore) -> String {
    let mut s = SessionState::default();
    let mut parts = Vec::new();
    for sql in [
        "SELECT id, name FROM t ORDER BY id LIMIT 100",
        "SELECT * FROM sq",
        "SELECT * FROM v LIMIT 100",
        "SELECT * FROM made LIMIT 1",
    ] {
        parts.push(
            core.execute_sql_in_session(&ctx(), &mut s, sql)
                .map(|o| format!("{o:?}"))
                .unwrap_or_else(|e| format!("ERR {}", e.wire_code())),
        );
    }
    parts.join("|")
}

/// 書き込み系の文の全種類（受け入れ条件 (a)）。
const WRITES: &[&str] = &[
    "INSERT INTO t (id, name) VALUES (10, 'x')",
    "INSERT INTO t (id, name) VALUES (10, 'x'), (11, 'y')",
    "INSERT INTO t (id, name) VALUES (10, 'x') RETURNING id",
    "INSERT INTO t (id, name) VALUES (1, 'x') ON CONFLICT (id) DO UPDATE SET name = 'z'",
    "INSERT INTO t (id, name) VALUES (10, 'x') USING OPERATION_ID 'ro-op-1'",
    "INSERT INTO t (id, name) SELECT id + 100, name FROM t",
    "UPDATE t SET name = 'u' WHERE id = 1",
    "UPDATE t SET name = 'u' WHERE name = 'a' RETURNING id",
    "UPDATE t SET name = 'u' WHERE id = 1 USING OPERATION_ID 'ro-op-2'",
    "DELETE FROM t WHERE id = 1",
    "DELETE FROM t WHERE name = 'a'",
    "TRUNCATE TABLE t USING OPERATION_ID 'ro-op-3'",
    "WITH d AS (DELETE FROM t WHERE id = 1 RETURNING id) SELECT id FROM d",
    "INSERT INTO v (id, name) VALUES (10, 'x')",
    "UPDATE v SET name = 'u' WHERE id = 1",
    "DELETE FROM v WHERE id = 1",
    "SELECT setval('sq', 50)",
    "LOCK TABLE t",
    "LOCK TABLE t IN ACCESS SHARE MODE",
    "LOCK TABLE t IN ACCESS EXCLUSIVE MODE NOWAIT",
    "SELECT id FROM t WHERE id = 1 FOR UPDATE",
    "CREATE TABLE made (id BIGINT PRIMARY KEY)",
    "CREATE TEMP TABLE made (id BIGINT PRIMARY KEY)",
    "CREATE INDEX made_idx ON t (name)",
    "CREATE SEQUENCE made_seq",
    "CREATE VIEW made AS SELECT id FROM t",
    "CREATE FUNCTION made() RETURNS int LANGUAGE sql AS $$ SELECT 1 $$",
    "DROP TABLE t",
    "DROP VIEW v",
    "DROP SEQUENCE sq",
    "ALTER TABLE t ADD COLUMN extra TEXT",
    "DO $$ BEGIN INSERT INTO t (id, name) VALUES (10, 'x'); END $$",
];

#[test]
fn every_write_form_is_rejected_with_25006_and_changes_nothing() {
    let (core, _g) = new_core("sql38-ro-reject");
    let baseline = fingerprint(&core);
    for sql in WRITES {
        for ddl_allowed in [false, true] {
            let mut session = if ddl_allowed {
                ddl_session()
            } else {
                SessionState::default()
            };
            let mut txn = core.new_session_transaction();
            run(&core, &mut session, &mut txn, "BEGIN READ ONLY").expect("begin");
            assert_eq!(
                run(&core, &mut session, &mut txn, sql),
                Err("25006".to_string()),
                "{sql} (ddl_allowed={ddl_allowed})"
            );
            assert_eq!(txn.status(), TransactionStatus::Failed, "{sql}");
            assert_eq!(
                run(&core, &mut session, &mut txn, "SELECT id FROM t LIMIT 1"),
                Err("25P02".to_string()),
                "{sql}"
            );
            assert_eq!(
                run(&core, &mut session, &mut txn, "ROLLBACK"),
                Ok(SqlOutcome::Rollback)
            );
            assert_eq!(txn.status(), TransactionStatus::Idle);
            assert_eq!(fingerprint(&core), baseline, "{sql}");
        }
    }
}

/// 受け入れ条件 (b): 台帳に記録されないこと。同じ operation_id を autocommit で再実行すると
/// 新規として成功する（重複の `23505` にならない）。
#[test]
fn rejected_operation_id_statement_leaves_no_ledger_entry() {
    let path = unique_db_path("sql38-ro-ledger");
    let _g = CleanupGuard(path.clone());
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("open storage"),
        Box::new(CpuScalarProvider),
    );
    core.execute_sql_in_session(
        &ctx(),
        &mut ddl_session(),
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    )
    .expect("create");
    let insert = "INSERT INTO t (id, name) VALUES (10, 'x') USING OPERATION_ID 'ro-ledger-1'";
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    run(&core, &mut session, &mut txn, "BEGIN READ ONLY").expect("begin");
    assert_eq!(
        run(&core, &mut session, &mut txn, insert),
        Err("25006".to_string())
    );
    run(&core, &mut session, &mut txn, "ROLLBACK").expect("rollback");
    core.execute_sql_in_session(&ctx(), &mut SessionState::default(), insert)
        .expect("same operation_id succeeds as new");
}

#[test]
fn copy_from_is_rejected_before_copy_in_response() {
    let (core, _g) = new_core("sql38-ro-copy");
    let baseline = fingerprint(&core);
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    run(&core, &mut session, &mut txn, "BEGIN READ ONLY").expect("begin");
    let r = core.begin_copy_in_txn(&ctx(), &session, &mut txn, "COPY t (id, name) FROM STDIN");
    match r {
        Err(e) => assert_eq!(e.wire_code(), "25006"),
        Ok(CopyPlan::From(_)) => panic!("COPY FROM must not start in READ ONLY"),
        Ok(CopyPlan::To(..)) => panic!("unexpected COPY TO plan"),
    }
    assert_eq!(txn.status(), TransactionStatus::Failed);
    run(&core, &mut session, &mut txn, "ROLLBACK").expect("rollback");
    assert_eq!(fingerprint(&core), baseline);
}

/// READ ONLY の Tx は writer を取らないため、拒否の前後とも別セッションが書き込める。
#[test]
fn read_only_txn_never_blocks_another_writer() {
    let (core, _g) = new_core("sql38-ro-nowriter");
    let mut s1 = SessionState::default();
    let mut t1 = core.new_session_transaction();
    run(&core, &mut s1, &mut t1, "BEGIN READ ONLY").expect("begin ro");
    run(
        &core,
        &mut s1,
        &mut t1,
        "SELECT id FROM t ORDER BY id LIMIT 10",
    )
    .expect("read");

    let mut s2 = SessionState::default();
    let mut t2 = core.new_session_transaction();
    run(&core, &mut s2, &mut t2, "BEGIN").expect("begin rw");
    run(
        &core,
        &mut s2,
        &mut t2,
        "INSERT INTO t (id, name) VALUES (20, 'w')",
    )
    .expect("write");
    run(&core, &mut s2, &mut t2, "COMMIT").expect("commit");

    assert_eq!(
        run(&core, &mut s1, &mut t1, "DELETE FROM t WHERE id = 1"),
        Err("25006".to_string())
    );
    let mut s3 = SessionState::default();
    let mut t3 = core.new_session_transaction();
    run(&core, &mut s3, &mut t3, "BEGIN").expect("begin rw 2");
    run(
        &core,
        &mut s3,
        &mut t3,
        "INSERT INTO t (id, name) VALUES (21, 'w')",
    )
    .expect("write 2");
    run(&core, &mut s3, &mut t3, "COMMIT").expect("commit 2");
}

/// 優先順位: 分割実行の拒否は `25001` が先に動く（既存契約を変えない）。
#[test]
fn partitioned_write_keeps_25001() {
    let (core, _g) = new_core("sql38-ro-precedence");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    run(&core, &mut session, &mut txn, "BEGIN READ ONLY").expect("begin");
    assert_eq!(
        run(
            &core,
            &mut session,
            &mut txn,
            "UPDATE t SET name = 'u' WHERE id > 0 USING OPERATION_ID 'ro-p' PARTITIONED CHUNK 1"
        ),
        Err("25001".to_string())
    );
    run(&core, &mut session, &mut txn, "ROLLBACK").expect("rollback");
}

/// `lastval()` は読み取りのみで READ ONLY 内でも許可される。writer を要求せず（`25006` にならず）、
/// トランザクションが `Failed` へ遷移しない（writer を取らないまま COMMIT できる）。
#[test]
fn lastval_is_allowed_without_writer_and_does_not_fail_txn() {
    let (core, _g) = new_core("sql38-ro-lastval");
    let mut session = ddl_session();
    for sql in [
        "CREATE TABLE lv (id BIGINT PRIMARY KEY, n BIGINT)",
        "INSERT INTO lv (id, n) VALUES (1, nextval('sq')) USING OPERATION_ID 'lv-op-1'",
    ] {
        core.execute_sql_in_session(&ctx(), &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql}: {} {e}", e.wire_code()));
    }
    let mut txn = core.new_session_transaction();
    run(&core, &mut session, &mut txn, "BEGIN READ ONLY").expect("begin");
    run(&core, &mut session, &mut txn, "SELECT lastval()").expect("lastval in read only");
    assert_eq!(txn.status(), TransactionStatus::InTransaction);
    assert_eq!(
        run(&core, &mut session, &mut txn, "COMMIT"),
        Ok(SqlOutcome::Commit)
    );
}

/// 許可側の回帰: 読み取り・セッション設定は READ ONLY 内で成功し、COMMIT できる。
#[test]
fn read_statements_are_allowed_and_commit() {
    let (core, _g) = new_core("sql38-ro-allowed");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    run(&core, &mut session, &mut txn, "BEGIN READ ONLY").expect("begin");
    for sql in [
        "SELECT id, name FROM t ORDER BY id LIMIT 10",
        "SELECT count(*) FROM t",
        "SELECT id FROM t UNION SELECT id FROM t",
        "SELECT 1",
        "SET TIME ZONE 'UTC'",
        "SHOW TIME ZONE",
    ] {
        run(&core, &mut session, &mut txn, sql).unwrap_or_else(|c| panic!("{sql}: {c}"));
        assert_eq!(txn.status(), TransactionStatus::InTransaction, "{sql}");
    }
    assert_eq!(
        run(&core, &mut session, &mut txn, "COMMIT"),
        Ok(SqlOutcome::Commit)
    );
}
