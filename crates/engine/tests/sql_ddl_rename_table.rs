//! `ALTER TABLE <t> RENAME TO <u>`（TABLE-38 のポインタ、Issue #1623）の SQL 表層結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（構文検証 → `require_ddl_permission` →
//! `sql::ddl::execute_alter_table_rename_table`）を production 経路として検証する
//! （`sql_ddl_rename_column.rs` と同じ流儀）。索引・制約・ビューに依存される表の改名は
//! Issue #1624・#1625 で追随させるまで `0A000` で拒否することも固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::ddl::AlterTableAction;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn schema(name: &str) -> TableSchema {
    TableSchema::new(
        name,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("note", ColumnType::Text, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-ddl-rename-table");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema("docs")).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn run_as(
    core: &EngineCore,
    tenant: &str,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(tenant), session, sql)
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    run_as(core, "alice", session, sql)
}

fn code_of(core: &EngineCore, session: &mut SessionState, sql: &str) -> &'static str {
    run(core, session, sql)
        .expect_err(&format!("expected {sql:?} to be rejected"))
        .wire_code()
}

fn insert(core: &EngineCore, tenant: &str, table: &str, id: u64, note: &str) {
    let mut s = SessionState::default();
    run_as(
        core,
        tenant,
        &mut s,
        &format!(
            "INSERT INTO {table} (id, embedding, note) VALUES ({id}, '[0.1,0.2]', '{note}') \
             USING OPERATION_ID 'op-{tenant}-{table}-{id}'"
        ),
    )
    .expect("insert");
}

fn notes(core: &EngineCore, tenant: &str, table: &str) -> Result<Vec<String>, SqlSurfaceError> {
    let out = core.execute_sql(
        &ctx(tenant),
        &format!("SELECT note FROM {table} ORDER BY id LIMIT 100"),
    )?;
    Ok(out
        .rows
        .into_iter()
        .map(|r| match r.cells.into_iter().next().expect("one cell") {
            Cell::Text(s) => s,
            other => panic!("unexpected cell {other:?}"),
        })
        .collect())
}

#[test]
fn rename_moves_rows_per_tenant_and_old_name_is_gone() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, "alice", "docs", 1, "a1");
    insert(&core, "alice", "docs", 2, "a2");
    insert(&core, "bob", "docs", 3, "b3");
    // キャッシュを温める。
    assert_eq!(notes(&core, "alice", "docs").expect("warm").len(), 2);

    let out = run(&core, &mut session, "ALTER TABLE docs RENAME TO papers;").expect("rename");
    match out {
        SqlOutcome::AlterTable(o) => {
            assert_eq!(o.table_name, "docs");
            assert_eq!(
                o.action,
                AlterTableAction::RenameTable {
                    new_table_name: "papers".to_string()
                }
            );
        }
        other => panic!("unexpected outcome: {other:?}"),
    }

    // テナント境界は物理キーとともに移る。
    assert_eq!(
        notes(&core, "alice", "papers").expect("alice"),
        ["a1", "a2"]
    );
    assert_eq!(notes(&core, "bob", "papers").expect("bob"), ["b3"]);
    assert_eq!(
        notes(&core, "carol", "papers").expect("carol"),
        Vec::<String>::new()
    );
    // 旧名は存在しない。
    assert_eq!(
        notes(&core, "alice", "docs").expect_err("old").wire_code(),
        "42P01"
    );
    assert_eq!(
        code_of(
            &core,
            &mut ddl_session(),
            "INSERT INTO docs (id, embedding, note) VALUES (9, '[0.1,0.2]', 'n') USING OPERATION_ID 'op-old'"
        ),
        "42P01"
    );

    // 改名後の DML と KNN 検索。
    insert(&core, "alice", "papers", 4, "a4");
    let mut s = SessionState::default();
    run(
        &core,
        &mut s,
        "UPDATE papers SET note = 'a1x' WHERE id = 1 USING OPERATION_ID 'op-u1'",
    )
    .expect("update");
    run(
        &core,
        &mut s,
        "DELETE FROM papers WHERE id = 2 USING OPERATION_ID 'op-d2'",
    )
    .expect("delete");
    assert_eq!(
        notes(&core, "alice", "papers").expect("alice"),
        ["a1x", "a4"]
    );
    let knn = core
        .execute_sql(
            &ctx("alice"),
            "SELECT id FROM papers ORDER BY embedding <=> '[0.1,0.2]' LIMIT 10",
        )
        .expect("knn");
    assert_eq!(knn.rows.len(), 2);
}

#[test]
fn rename_of_never_written_table_succeeds() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    run(&core, &mut session, "ALTER TABLE docs RENAME TO papers").expect("rename");
    assert!(notes(&core, "alice", "papers").expect("empty").is_empty());
    insert(&core, "alice", "papers", 1, "x");
    assert_eq!(notes(&core, "alice", "papers").expect("one"), ["x"]);
}

#[test]
fn old_name_recreated_is_empty_and_round_trip_keeps_rows() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, "alice", "docs", 1, "a1");
    assert_eq!(notes(&core, "alice", "docs").expect("warm"), ["a1"]);
    run(&core, &mut session, "ALTER TABLE docs RENAME TO papers").expect("rename");
    run(
        &core,
        &mut session,
        "CREATE TABLE docs (embedding VECTOR(2) NOT NULL, note TEXT)",
    )
    .expect("recreate");
    assert!(notes(&core, "alice", "docs").expect("fresh").is_empty());
    assert_eq!(notes(&core, "alice", "papers").expect("moved"), ["a1"]);
    // 往復（papers -> docs は衝突するので別名を経由する）。
    run(&core, &mut session, "DROP TABLE docs").expect("drop");
    run(&core, &mut session, "ALTER TABLE papers RENAME TO tmp").expect("to tmp");
    run(&core, &mut session, "ALTER TABLE tmp RENAME TO docs").expect("back");
    assert_eq!(notes(&core, "alice", "docs").expect("back"), ["a1"]);
}

#[test]
fn rename_survives_reopen() {
    let path = unique_db_path("sql-ddl-rename-table-reopen");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open");
        storage.create_table(&schema("docs")).expect("create");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        insert(&core, "alice", "docs", 1, "a1");
        insert(&core, "bob", "docs", 2, "b2");
        run(
            &core,
            &mut ddl_session(),
            "ALTER TABLE docs RENAME TO papers",
        )
        .expect("rename");
    }
    let core = EngineCore::from_storage(
        Storage::open(&path).expect("reopen"),
        Box::new(CpuScalarProvider),
    );
    assert_eq!(notes(&core, "alice", "papers").expect("alice"), ["a1"]);
    assert_eq!(notes(&core, "bob", "papers").expect("bob"), ["b2"]);
    assert_eq!(
        notes(&core, "alice", "docs").expect_err("old").wire_code(),
        "42P01"
    );
}

#[test]
fn rename_error_codes() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    run(&core, &mut session, "CREATE TABLE other (note TEXT)").expect("other");
    run(
        &core,
        &mut session,
        "CREATE VIEW v_other AS SELECT note FROM other",
    )
    .expect("view");
    run(
        &core,
        &mut session,
        "CREATE INDEX idx_other ON other (note)",
    )
    .expect("index");
    let cases = [
        // 既存の表・ビュー・索引名、同名への改名
        ("ALTER TABLE docs RENAME TO other", "42P07"),
        ("ALTER TABLE docs RENAME TO v_other", "42P07"),
        ("ALTER TABLE docs RENAME TO idx_other", "42P07"),
        ("ALTER TABLE docs RENAME TO docs", "42P07"),
        // 不在の表、改名元がビュー・索引
        ("ALTER TABLE nope RENAME TO x", "42P01"),
        ("ALTER TABLE v_other RENAME TO x", "42809"),
        ("ALTER TABLE idx_other RENAME TO x", "42809"),
        // 構文
        ("ALTER TABLE docs RENAME TO \"bad name\"", "42601"),
        ("ALTER TABLE docs RENAME TO s.u", "42601"),
        ("ALTER TABLE docs RENAME TO", "42601"),
        ("ALTER TABLE docs RENAME TO x, ADD COLUMN z TEXT", "42601"),
        ("ALTER TABLE docs ADD COLUMN z TEXT, RENAME TO x", "42601"),
        ("ALTER TABLE docs RENAME CONSTRAINT a TO b", "42601"),
    ];
    for (sql, code) in cases {
        assert_eq!(code_of(&core, &mut session, sql), code, "{sql}");
    }
    // 拒否の後も表は旧名のまま。
    assert!(notes(&core, "alice", "docs")
        .expect("still docs")
        .is_empty());
}

#[test]
fn rename_requires_ddl_permission_without_existence_oracle() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut denied = SessionState::default();
    let existing = code_of(&core, &mut denied, "ALTER TABLE docs RENAME TO x");
    let missing = code_of(&core, &mut denied, "ALTER TABLE nope RENAME TO x");
    assert_eq!(existing, "42501");
    assert_eq!(existing, missing);
    assert!(notes(&core, "alice", "docs").is_ok());
}

#[test]
fn rename_inside_explicit_transaction_is_rejected() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    let owner = ctx("alice");
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&owner, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let err = core
        .execute_sql_in_txn(
            &owner,
            &mut session,
            &mut txn,
            "ALTER TABLE docs RENAME TO x",
        )
        .expect_err("DDL inside an explicit transaction must be rejected");
    assert_eq!(err.wire_code(), "0A000");
    assert!(notes(&core, "alice", "docs").is_ok());
}

#[test]
fn rename_with_dependents_is_refused_without_changes() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    for ddl in [
        "CREATE TABLE pk_t (code TEXT PRIMARY KEY, note TEXT)",
        "CREATE TABLE uq_t (code TEXT UNIQUE, note TEXT)",
        "CREATE TABLE ck_t (c INTEGER, note TEXT, CHECK (c > 0))",
        "CREATE TABLE parent_t (code TEXT PRIMARY KEY, note TEXT)",
        "CREATE TABLE child_t (p TEXT, note TEXT, FOREIGN KEY (p) REFERENCES parent_t (code))",
        "CREATE TABLE idx_t (note TEXT)",
        "CREATE INDEX idx_t_note ON idx_t (note)",
        "CREATE TABLE view_t (note TEXT)",
        "CREATE VIEW v_view_t AS SELECT note FROM view_t",
    ] {
        run(&core, &mut session, ddl).unwrap_or_else(|e| panic!("{ddl}: {e:?}"));
    }
    for table in [
        "pk_t", "uq_t", "ck_t", "parent_t", "child_t", "idx_t", "view_t",
    ] {
        let sql = format!("ALTER TABLE {table} RENAME TO renamed_x");
        assert_eq!(code_of(&core, &mut session, &sql), "0A000", "{sql}");
        // 何も変わっていない: 旧名は存在し、新名は存在しない。
        assert!(notes(&core, "alice", table).is_ok(), "{table} must remain");
        assert_eq!(
            notes(&core, "alice", "renamed_x")
                .expect_err("no new name")
                .wire_code(),
            "42P01"
        );
    }
}
