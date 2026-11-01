//! 明示トランザクションの読み取り集合（SQL-38・TASK-333・Issue #2404）のクレート内テスト。
//!
//! `SessionTransaction::read_set()`（`pub(crate)`）で writer 取得前に読んだ基底表の集合を直接観測する。
//! 設計記録は `docs/design/explicit-transaction.md` 1a-4。受け入れ条件 (A)「結合・部分クエリ・ビュー経由の
//! 読み取りが集合に入る」と、writer 取得後・一時表・暗黙トランザクションが集合に入らないことを固定する。
//! 判定そのもの（`40001`）は #2405 の担当で、ここでは集合の内容だけを検査する。

use super::EngineCore;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::mode::SessionState;
use crate::sql::read_set::ReadSet;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

const TABLES: [&str; 6] = ["ra", "rb", "rc", "rd", "re", "rf"];

fn new_core(tag: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    for name in TABLES {
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

fn seed(core: &EngineCore, session: &mut SessionState) {
    let caller = ctx("tenant-a");
    for t in TABLES {
        core.execute_sql_in_session(
            &caller,
            session,
            &format!(
                "INSERT INTO {t} (id, embedding, n) VALUES (1, '[1.0, 0.0]', 1) \
                 USING OPERATION_ID 'seed-{t}'"
            ),
        )
        .expect("seed");
    }
}

/// `BEGIN` → `sqls` を順に実行 → その時点の読み取り集合を返す（Tx は `ROLLBACK` する）。
fn read_set_after(core: &EngineCore, session: &mut SessionState, sqls: &[&str]) -> ReadSet {
    let caller = ctx("tenant-a");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, session, &mut txn, "BEGIN")
        .expect("begin");
    for sql in sqls {
        core.execute_sql_in_txn(&caller, session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    }
    let set = txn.read_set().expect("active").clone();
    core.execute_sql_in_txn(&caller, session, &mut txn, "ROLLBACK")
        .expect("rollback");
    set
}

fn assert_reads(set: &ReadSet, expected: &[&str]) {
    for t in expected {
        assert!(set.contains(t), "{t} must be in the read set: {set:?}");
    }
    assert!(!set.is_wildcard(), "unexpected wildcard: {set:?}");
}

fn assert_not_reads(set: &ReadSet, absent: &[&str]) {
    assert!(!set.is_wildcard(), "unexpected wildcard: {set:?}");
    for t in absent {
        assert!(!set.contains(t), "{t} must not be in the read set: {set:?}");
    }
}

#[test]
fn begin_alone_leaves_the_read_set_empty() {
    let (core, _g) = new_core("readset-empty");
    let mut session = SessionState::default();
    let set = read_set_after(&core, &mut session, &[]);
    assert!(set.is_empty());
}

#[test]
fn plain_select_records_the_table() {
    let (core, _g) = new_core("readset-plain");
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let set = read_set_after(&core, &mut session, &["SELECT id FROM ra LIMIT 10"]);
    assert_reads(&set, &["ra"]);
    assert_not_reads(&set, &["rb", "rc"]);
}

#[test]
fn join_records_both_tables() {
    let (core, _g) = new_core("readset-join");
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let set = read_set_after(
        &core,
        &mut session,
        &["SELECT ra.id FROM ra JOIN rb ON ra.id = rb.id LIMIT 10"],
    );
    assert_reads(&set, &["ra", "rb"]);
    assert_not_reads(&set, &["rc"]);
}

#[test]
fn subqueries_record_the_inner_tables() {
    let (core, _g) = new_core("readset-subquery");
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let set = read_set_after(
        &core,
        &mut session,
        &["SELECT id FROM ra WHERE id IN (SELECT id FROM rc) LIMIT 10"],
    );
    assert_reads(&set, &["ra", "rc"]);
    let set = read_set_after(
        &core,
        &mut session,
        &["SELECT id FROM ra WHERE EXISTS (SELECT 1 FROM rd WHERE rd.id = ra.id) LIMIT 10"],
    );
    assert_reads(&set, &["ra", "rd"]);
}

#[test]
fn cte_and_set_operations_record_every_branch() {
    let (core, _g) = new_core("readset-cte-setop");
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let set = read_set_after(
        &core,
        &mut session,
        &["WITH x AS (SELECT id FROM ra) SELECT id FROM x LIMIT 10"],
    );
    assert_reads(&set, &["ra"]);
    let set = read_set_after(
        &core,
        &mut session,
        &["SELECT id FROM rb UNION SELECT id FROM rc"],
    );
    assert_reads(&set, &["rb", "rc"]);
}

#[test]
fn view_reads_record_the_base_tables_including_nested_views() {
    let (core, _g) = new_core("readset-view");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    seed(&core, &mut session);
    for ddl in [
        "CREATE VIEW v_d AS SELECT id, n FROM rd",
        "CREATE VIEW v_vd AS SELECT id FROM v_d",
    ] {
        core.execute_sql_in_session(&caller, &mut session, ddl)
            .unwrap_or_else(|e| panic!("{ddl}: {e:?}"));
    }
    let set = read_set_after(&core, &mut session, &["SELECT id FROM v_d LIMIT 10"]);
    assert_reads(&set, &["rd"]);
    let set = read_set_after(&core, &mut session, &["SELECT id FROM v_vd LIMIT 10"]);
    assert_reads(&set, &["rd"]);
    assert_not_reads(&set, &["ra"]);
}

#[test]
fn explain_records_the_table_without_reading_rows() {
    let (core, _g) = new_core("readset-explain");
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let set = read_set_after(&core, &mut session, &["EXPLAIN SELECT id FROM re LIMIT 10"]);
    assert_reads(&set, &["re"]);
}

#[test]
fn cursor_declare_records_the_inner_select() {
    let (core, _g) = new_core("readset-cursor");
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let set = read_set_after(
        &core,
        &mut session,
        &["DECLARE c CURSOR FOR SELECT id FROM rf LIMIT 10"],
    );
    assert_reads(&set, &["rf"]);
}

#[test]
fn copy_to_stdout_records_the_inner_select() {
    let (core, _g) = new_core("readset-copy");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    core.begin_copy_in_txn(
        &caller,
        &session,
        &mut txn,
        "COPY (SELECT id FROM rb LIMIT 10) TO STDOUT",
    )
    .expect("copy to");
    let set = txn.read_set().expect("active").clone();
    assert_reads(&set, &["rb"]);
    core.execute_sql_in_txn(&caller, &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
}

#[test]
fn reads_after_the_writer_is_taken_are_not_recorded() {
    let (core, _g) = new_core("readset-after-writer");
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let set = read_set_after(
        &core,
        &mut session,
        &[
            "SELECT id FROM ra LIMIT 10",
            "INSERT INTO rb (id, embedding, n) VALUES (9, '[1.0, 0.0]', 1) \
             USING OPERATION_ID 'after-1'",
            "SELECT id FROM rc LIMIT 10",
        ],
    );
    assert_reads(&set, &["ra"]);
    assert_not_reads(&set, &["rb", "rc"]);
}

#[test]
fn insert_select_source_is_not_recorded_because_the_writer_is_taken_first() {
    let (core, _g) = new_core("readset-insert-select");
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let set = read_set_after(
        &core,
        &mut session,
        &[
            "INSERT INTO rb (id, embedding, n) SELECT id, embedding, n FROM rc WHERE id = 99 \
           USING OPERATION_ID 'insel'",
        ],
    );
    assert_not_reads(&set, &["rb", "rc"]);
}

#[test]
fn temp_table_reads_are_not_recorded() {
    let (core, _g) = new_core("readset-temp");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &caller,
        &mut session,
        "CREATE TEMP TABLE tt (id BIGINT PRIMARY KEY, n INTEGER)",
    )
    .expect("create temp");
    let set = read_set_after(
        &core,
        &mut session,
        &[
            "INSERT INTO tt (id, n) VALUES (1, 1)",
            "SELECT id FROM tt LIMIT 10",
        ],
    );
    assert!(!set.contains("tt"), "{set:?}");
    assert!(set.is_empty(), "{set:?}");
}

#[test]
fn catalog_enumeration_becomes_a_wildcard() {
    let (core, _g) = new_core("readset-wildcard");
    let scope = crate::sql::read_set::ReadSetScope::enter();
    let _ = core.storage.list_tables().expect("list");
    let set = scope.finish();
    assert!(set.is_wildcard());
    assert!(set.contains("anything"));
}

#[test]
fn implicit_transaction_has_no_read_set_scope() {
    let (core, _g) = new_core("readset-implicit");
    let mut session = SessionState::default();
    seed(&core, &mut session);
    let caller = ctx("tenant-a");
    let mut txn = core.new_session_transaction();
    core.begin_implicit_transaction(&mut txn, &session)
        .expect("begin implicit");
    assert!(txn.read_set_scope().is_none());
    core.execute_sql_in_txn(
        &caller,
        &mut session,
        &mut txn,
        "SELECT id FROM ra LIMIT 10",
    )
    .expect("select");
    assert!(txn.read_set().expect("active").is_empty());
    txn.abort_implicit();
}

#[test]
fn autocommit_statements_do_not_leak_into_a_later_transaction() {
    let (core, _g) = new_core("readset-autocommit");
    let caller = ctx("tenant-a");
    let mut session = SessionState::default();
    seed(&core, &mut session);
    core.execute_sql_in_session(&caller, &mut session, "SELECT id FROM rf LIMIT 10")
        .expect("autocommit select");
    let set = read_set_after(&core, &mut session, &["SELECT id FROM ra LIMIT 10"]);
    assert_reads(&set, &["ra"]);
    assert_not_reads(&set, &["rf"]);
}
