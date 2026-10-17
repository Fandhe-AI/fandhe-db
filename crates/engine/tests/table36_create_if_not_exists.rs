//! `CREATE TABLE IF NOT EXISTS`・名前付き `CREATE INDEX IF NOT EXISTS` の結合テスト
//! （Issue #1613）。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-36・
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `sql_index_ddl.rs` と同じ流儀（実 `Storage` と `EngineCore::execute_sql_in_session`）。
//! 検証する契約（`docs/design/sql-create-table.md` 参照）:
//!
//! - 同名の関係（表・ビュー・索引）があれば定義を比較せず成功し、カタログを変えない
//! - 句なしの重複は従来どおり `42P07`
//! - `CREATE VIEW`／`CREATE TYPE`／名前なし `CREATE INDEX` の `IF NOT EXISTS` は `42601`
//! - 権限ゲート（`42501`）・明示トランザクション（`0A000`）は句の有無で変わらない

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("alice", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn allowed_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn core_at(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("open storage"),
        Box::new(CpuScalarProvider),
    )
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), session, sql)
}

fn run_ok(core: &EngineCore, session: &mut SessionState, sql: &str) -> SqlOutcome {
    run(core, session, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn run_code(core: &EngineCore, session: &mut SessionState, sql: &str) -> &'static str {
    match run(core, session, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

fn fixture(label: &str) -> (std::path::PathBuf, CleanupGuard) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    (path, guard)
}

#[test]
fn creates_when_absent_and_is_noop_when_present() {
    let (path, _guard) = fixture("t36-table");
    let core = core_at(&path);
    let mut s = allowed_session();
    run_ok(&core, &mut s, "CREATE TABLE IF NOT EXISTS t (a TEXT)");
    // 定義が異なっても何もしない（既存のスキーマが残る）。
    run_ok(
        &core,
        &mut s,
        "CREATE TABLE IF NOT EXISTS t (b TEXT, c TEXT)",
    );
    run_ok(&core, &mut s, "create table if not exists T2 (a TEXT)");
    run_ok(&core, &mut s, "Create Table If Not Exists t2 (z TEXT)");
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("t").expect("schema");
    assert_eq!(schema.columns.len(), 1);
    assert_eq!(schema.columns[0].name, "a");
}

#[test]
fn table_noop_skips_definition_validation_and_shares_namespace() {
    let (path, _guard) = fixture("t36-ns");
    let core = core_at(&path);
    let mut s = allowed_session();
    // 未登録 ENUM 型の列でも、同名があれば意味検証より前に no-op。
    run_ok(
        &core,
        &mut s,
        "CREATE TABLE IF NOT EXISTS docs (a no_such_enum)",
    );
    run_ok(&core, &mut s, "CREATE VIEW v AS SELECT id FROM docs");
    run_ok(&core, &mut s, "CREATE INDEX i ON docs (lang)");
    run_ok(&core, &mut s, "CREATE TABLE IF NOT EXISTS v (a TEXT)");
    run_ok(&core, &mut s, "CREATE TABLE IF NOT EXISTS i (a TEXT)");
    // 未存在なら意味検証は走る。
    assert_eq!(
        run_code(
            &core,
            &mut s,
            "CREATE TABLE IF NOT EXISTS fresh (a no_such_enum)"
        ),
        "42704"
    );
    // 句なしの重複は従来どおり。
    for sql in [
        "CREATE TABLE docs (a TEXT)",
        "CREATE TABLE v (a TEXT)",
        "CREATE TABLE i (a TEXT)",
    ] {
        assert_eq!(run_code(&core, &mut s, sql), "42P07", "{sql}");
    }
}

#[test]
fn index_if_not_exists_is_noop_for_existing_name() {
    let (path, _guard) = fixture("t36-index");
    {
        let core = core_at(&path);
        let mut s = allowed_session();
        run_ok(&core, &mut s, "CREATE INDEX IF NOT EXISTS i ON docs (lang)");
        run_ok(&core, &mut s, "CREATE INDEX IF NOT EXISTS i ON docs (body)");
        run_ok(&core, &mut s, "create index if not exists i on docs (body)");
        run_ok(&core, &mut s, "CREATE VIEW v AS SELECT id FROM docs");
        run_ok(
            &core,
            &mut s,
            "CREATE INDEX IF NOT EXISTS docs ON docs (lang)",
        );
        run_ok(&core, &mut s, "CREATE INDEX IF NOT EXISTS v ON docs (lang)");
        assert_eq!(
            run_code(&core, &mut s, "CREATE INDEX i ON docs (lang)"),
            "42P07"
        );
    }
    let indexes = Storage::open(&path)
        .expect("reopen")
        .list_indexes()
        .expect("list");
    assert_eq!(indexes.len(), 1);
    assert_eq!(indexes[0].name, "i");
    assert_eq!(indexes[0].columns, vec!["lang".to_string()]);
}

#[test]
fn unsupported_shapes_are_rejected_with_42601() {
    let (path, _guard) = fixture("t36-reject");
    let core = core_at(&path);
    for sql in [
        "CREATE VIEW IF NOT EXISTS v AS SELECT id FROM docs",
        "CREATE TYPE IF NOT EXISTS e AS ENUM ('a')",
        "CREATE INDEX IF NOT EXISTS ON docs (lang)",
        "CREATE TABLE IF NOT EXISTS (a TEXT)",
        "CREATE TABLE IF EXISTS t (a TEXT)",
        "CREATE TABLE IF NOT t (a TEXT)",
    ] {
        for s in [&mut allowed_session(), &mut SessionState::default()] {
            assert_eq!(run_code(&core, s, sql), "42601", "{sql}");
        }
    }
    drop(core);
    assert!(Storage::open(&path)
        .expect("reopen")
        .list_indexes()
        .expect("list")
        .is_empty());
}

#[test]
fn permission_and_transaction_gates_are_unchanged() {
    let (path, _guard) = fixture("t36-gates");
    let core = core_at(&path);
    for sql in [
        "CREATE TABLE IF NOT EXISTS docs (a TEXT)",
        "CREATE TABLE IF NOT EXISTS fresh (a TEXT)",
        "CREATE INDEX IF NOT EXISTS i ON docs (lang)",
    ] {
        assert_eq!(
            run_code(&core, &mut SessionState::default(), sql),
            "42501",
            "{sql}"
        );
    }
    let mut s = allowed_session();
    for sql in [
        "CREATE TABLE IF NOT EXISTS docs (a TEXT)",
        "CREATE TABLE IF NOT EXISTS fresh (a TEXT)",
    ] {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&ctx(), &mut s, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(&ctx(), &mut s, &mut txn, sql)
            .expect_err("DDL inside a transaction must be rejected");
        assert_eq!(err.wire_code(), "0A000", "{sql}");
        core.execute_sql_in_txn(&ctx(), &mut s, &mut txn, "ROLLBACK")
            .expect("rollback");
    }
}

#[test]
fn if_as_table_name_still_works() {
    let (path, _guard) = fixture("t36-ifname");
    let core = core_at(&path);
    let mut s = allowed_session();
    run_ok(&core, &mut s, "CREATE TABLE if (a TEXT)");
}
