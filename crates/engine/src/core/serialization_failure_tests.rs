//! writer 取得時の `40001` 判定（SQL-31・SQL-38・TASK-333・Issue #2405。設計は
//! `docs/design/explicit-transaction.md` 1a-3・1a-6・1a-7・1a-11）のクレート内テスト。
//!
//! 同一スレッドで 2 つのセッション（明示 Tx と autocommit）を順に操作し、`SessionTransaction::
//! ensure_writer_with` の判定（読み取り集合 × 後続 commit の足跡）を `EngineCore` の入口
//! 越しに検査する。記録の床の注入（`replace_commit_log_for_test`）など内部状態に触れるものは
//! ここに置く。並行セッション・3 テナントの網羅は別 Issue（#2406・#2407）の担当。

use super::EngineCore;
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;
use crate::sql::transaction::{SessionTransaction, TransactionStatus};
use crate::storage::commit_log::Limits;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

const TABLES: [&str; 3] = ["sa", "sb", "sc"];

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("n", ColumnType::BigInt, false),
        ],
    )
}

fn new_core(tag: &str, limits: Option<Limits>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(tag);
    let guard = CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open storage");
    for name in TABLES {
        storage.create_table(&schema(name)).expect("create table");
    }
    if let Some(limits) = limits {
        storage
            .replace_commit_log_for_test(limits)
            .expect("replace log");
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

fn run(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn run_ddl(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn insert(table: &str, id: u64, op: &str) -> String {
    format!(
        "INSERT INTO {table} (id, embedding, n) VALUES ({id}, '[1.0, 0.0]', 1) \
         USING OPERATION_ID '{op}'"
    )
}

fn tx<'e>(
    core: &'e EngineCore,
    session: &mut SessionState,
    tenant: &str,
    sqls: &[&str],
) -> SessionTransaction<'e> {
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&ctx(tenant), session, &mut txn, "BEGIN")
        .expect("begin");
    for sql in sqls {
        core.execute_sql_in_txn(&ctx(tenant), session, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
    }
    txn
}

fn step<'e>(
    core: &'e EngineCore,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    tenant: &str,
    sql: &str,
) -> Result<(), SqlSurfaceError> {
    core.execute_sql_in_txn(&ctx(tenant), session, txn, sql)
        .map(|_| ())
}

fn assert_serialization_failure(r: Result<(), SqlSurfaceError>) {
    assert!(
        matches!(r, Err(SqlSurfaceError::SerializationFailure)),
        "expected 40001, got {r:?}"
    );
}

fn wildcard_set() -> crate::sql::read_set::ReadSet {
    let mut s = crate::sql::read_set::ReadSet::default();
    s.set_wildcard();
    s
}

/// 読んでから書く: 読んだ表への他セッションの commit があれば最初の書き込みで `40001`。
/// 副作用は残らず、以降は拒否され、`ROLLBACK` で `Idle` に戻る（SQL-38）。
#[test]
fn read_then_write_after_conflicting_commit_fails_with_40001() {
    let (core, _g) = new_core("ssi-basic", None);
    let mut session = SessionState::default();
    let mut txn = tx(&core, &mut session, "tenant-a", &["SELECT id FROM sa"]);
    run(&core, "tenant-a", &insert("sa", 1, "own-1"));
    let gen_before = core.storage.current_generation().expect("gen");

    let r = step(
        &core,
        &mut session,
        &mut txn,
        "tenant-a",
        &insert("sb", 9, "a-1"),
    );
    assert_serialization_failure(r);
    assert_eq!(txn.status(), TransactionStatus::Failed);
    assert_eq!(core.storage.current_generation().expect("gen"), gen_before);
    let next = step(&core, &mut session, &mut txn, "tenant-a", "SELECT 1");
    assert!(next.is_err());
    step(&core, &mut session, &mut txn, "tenant-a", "ROLLBACK").expect("rollback");
    assert_eq!(txn.status(), TransactionStatus::Idle);
    // 書き込みは残っておらず、operation_id も再利用できる。
    run(&core, "tenant-a", &insert("sb", 9, "a-1"));
}

/// 読んだ表と無関係な表への commit では `40001` にならず、取得後は最新の状態を見る。
#[test]
fn unrelated_commit_does_not_conflict() {
    let (core, _g) = new_core("ssi-unrelated", None);
    let mut session = SessionState::default();
    let mut txn = tx(&core, &mut session, "tenant-a", &["SELECT id FROM sa"]);
    run(&core, "tenant-a", &insert("sb", 1, "b-1"));
    step(
        &core,
        &mut session,
        &mut txn,
        "tenant-a",
        &insert("sc", 1, "a-1"),
    )
    .expect("no conflict");
    step(
        &core,
        &mut session,
        &mut txn,
        "tenant-a",
        "SELECT id FROM sb",
    )
    .expect("read after acquisition");
    step(&core, &mut session, &mut txn, "tenant-a", "COMMIT").expect("commit");
}

/// 読まずに書く形（INSERT・`LOCK TABLE`）と読み取りだけの Tx は `40001` にならない。
#[test]
fn write_without_read_and_read_only_never_conflict() {
    let (core, _g) = new_core("ssi-noread", None);
    let mut session = SessionState::default();

    let mut txn = tx(&core, &mut session, "tenant-a", &[]);
    run(&core, "tenant-b", &insert("sa", 1, "b-1"));
    step(
        &core,
        &mut session,
        &mut txn,
        "tenant-a",
        &insert("sa", 2, "a-1"),
    )
    .expect("write without read");
    step(&core, &mut session, &mut txn, "tenant-a", "COMMIT").expect("commit");

    // LOCK TABLE の取得前の存在確認は読み取り集合へ入らない。
    let mut txn = tx(&core, &mut session, "tenant-a", &[]);
    run(&core, "tenant-b", &insert("sa", 3, "b-2"));
    step(&core, &mut session, &mut txn, "tenant-a", "LOCK TABLE sa").expect("lock without read");
    step(&core, &mut session, &mut txn, "tenant-a", "COMMIT").expect("commit");

    let mut txn = tx(&core, &mut session, "tenant-a", &["SELECT id FROM sa"]);
    run(&core, "tenant-b", &insert("sa", 4, "b-3"));
    step(&core, &mut session, &mut txn, "tenant-a", "COMMIT").expect("read only commit");
}

/// 他テナントの Private 行だけを書いた commit は原因にならない（テナント境界）。
#[test]
fn others_private_rows_are_not_a_cause() {
    let (core, _g) = new_core("ssi-tenant", None);
    let mut session = SessionState::default();
    let mut txn = tx(&core, &mut session, "tenant-a", &["SELECT id FROM sa"]);
    run(&core, "tenant-b", &insert("sa", 1, "b-1"));
    step(
        &core,
        &mut session,
        &mut txn,
        "tenant-a",
        &insert("sb", 1, "a-1"),
    )
    .expect("other tenant's private rows are not a cause");
    step(&core, &mut session, &mut txn, "tenant-a", "COMMIT").expect("commit");
}

/// 自テナントの Private 行を書いた commit は原因になる。
#[test]
fn own_private_rows_are_a_cause() {
    let (core, _g) = new_core("ssi-own", None);
    let mut session = SessionState::default();
    let mut txn = tx(&core, &mut session, "tenant-a", &["SELECT id FROM sa"]);
    run(&core, "tenant-a", &insert("sa", 1, "own-1"));
    let r = step(
        &core,
        &mut session,
        &mut txn,
        "tenant-a",
        &insert("sb", 1, "a-1"),
    );
    assert_serialization_failure(r);
}

/// 読んだ表への DDL は原因になる。
#[test]
fn ddl_on_read_table_conflicts() {
    let (core, _g) = new_core("ssi-ddl", None);
    let mut session = SessionState::default();
    let mut txn = tx(&core, &mut session, "tenant-a", &["SELECT id FROM sa"]);
    run_ddl(&core, "tenant-b", "ALTER TABLE sa ADD COLUMN extra BIGINT");
    let r = step(
        &core,
        &mut session,
        &mut txn,
        "tenant-a",
        &insert("sb", 1, "a-1"),
    );
    assert_serialization_failure(r);
}

/// 記録の床が `G_s` を超えて判定できないときは `40001`（fail-closed）。
#[test]
fn unverifiable_log_floor_fails_closed() {
    let limits = Limits {
        max_records: 1,
        max_elements: 1_000,
        max_per_commit: 64,
    };
    let (core, _g) = new_core("ssi-floor", Some(limits));
    let mut session = SessionState::default();
    let mut txn = tx(&core, &mut session, "tenant-a", &["SELECT id FROM sa"]);
    for i in 0..4u64 {
        run(&core, "tenant-b", &insert("sc", i, &format!("b-{i}")));
    }
    let r = step(
        &core,
        &mut session,
        &mut txn,
        "tenant-a",
        &insert("sb", 1, "a-1"),
    );
    assert_serialization_failure(r);
}

/// ワイルドカードの読み取り集合は、無関係な表でも可視な commit で `40001`、他テナントの
/// Private だけなら `40001` にならない。
#[test]
fn wildcard_read_set_conflicts_only_on_visible_commits() {
    let (core, _g) = new_core("ssi-wild", None);
    let mut session = SessionState::default();
    let mut txn = tx(&core, &mut session, "tenant-a", &[]);
    txn.absorb_read_set(wildcard_set());
    run(&core, "tenant-b", &insert("sc", 1, "b-1"));
    step(
        &core,
        &mut session,
        &mut txn,
        "tenant-a",
        &insert("sb", 1, "a-1"),
    )
    .expect("others' private rows only");
    step(&core, &mut session, &mut txn, "tenant-a", "COMMIT").expect("commit");

    let mut txn = tx(&core, &mut session, "tenant-a", &[]);
    txn.absorb_read_set(wildcard_set());
    run(&core, "tenant-a", &insert("sc", 2, "a-own"));
    let r = step(
        &core,
        &mut session,
        &mut txn,
        "tenant-a",
        &insert("sb", 2, "a-2"),
    );
    assert_serialization_failure(r);
}

/// ビュー経由の読み取りでも基表への後続 commit で `40001`（読み取り集合と commit 記録の
/// 表名が同じ名前空間であること）。
#[test]
fn view_read_conflicts_with_base_table_commit() {
    let (core, _g) = new_core("ssi-view", None);
    run_ddl(&core, "tenant-a", "CREATE VIEW sv AS SELECT id, n FROM sa");
    let mut session = SessionState::default();
    let mut txn = tx(&core, &mut session, "tenant-a", &["SELECT id FROM sv"]);
    run(&core, "tenant-a", &insert("sa", 1, "own-1"));
    let r = step(
        &core,
        &mut session,
        &mut txn,
        "tenant-a",
        &insert("sb", 1, "a-1"),
    );
    assert_serialization_failure(r);
}
