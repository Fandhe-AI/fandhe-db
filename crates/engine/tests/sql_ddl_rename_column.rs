//! `ALTER TABLE <t> RENAME [COLUMN] <old> TO <new>`（TABLE-38 のポインタ、Issue #1619）の
//! SQL 表層結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（構文検証 → `require_ddl_permission` →
//! `sql::ddl::execute_alter_table_rename_column`）を production 経路として検証する
//! （`sql_ddl_drop_alter_column.rs` と同じ流儀）。参照オブジェクト（制約・索引・ビュー）の
//! 追随は兄弟 Issue の範囲で、本 Issue は依存のある列の改名を `0A000` で拒否する契約を固定する。

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

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("note", ColumnType::Text, true),
            ColumnDef::new("qty", ColumnType::Integer, true),
            ColumnDef::new("label", ColumnType::Text, true),
        ],
    )
}

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-ddl-rename-column");
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities("owner", [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn ddl_session() -> SessionState {
    let mut session = SessionState::default();
    session.allow_ddl();
    session
}

fn run(
    core: &EngineCore,
    session: &mut SessionState,
    sql: &str,
) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), session, sql)
}

fn code_of(core: &EngineCore, session: &mut SessionState, sql: &str) -> &'static str {
    run(core, session, sql)
        .expect_err(&format!("expected {sql:?} to be rejected"))
        .wire_code()
}

fn insert(core: &EngineCore, id: u64, cols: &str, vals: &str) {
    let mut s = SessionState::default();
    run(
        core,
        &mut s,
        &format!(
            "INSERT INTO {TABLE} (id, embedding, {cols}) VALUES ({id}, '[0.1,0.2]', {vals}) USING OPERATION_ID 'op-{id}'"
        ),
    )
    .expect("insert");
}

fn select_col(core: &EngineCore, col: &str) -> Vec<Cell> {
    core.execute_sql(&ctx(), &format!("SELECT {col} FROM {TABLE} LIMIT 100"))
        .expect("select")
        .rows
        .into_iter()
        .map(|r| r.cells.into_iter().next().expect("one cell"))
        .collect()
}

#[test]
fn rename_column_keeps_existing_rows_and_works_for_dml() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, 1, "note, qty", "'hello', 7");

    let out = run(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} RENAME COLUMN note TO memo;"),
    )
    .expect("rename");
    match out {
        SqlOutcome::AlterTable(o) => {
            assert_eq!(o.table_name, TABLE);
            assert_eq!(
                o.action,
                AlterTableAction::RenameColumn {
                    column_name: "note".to_string(),
                    new_column_name: "memo".to_string()
                }
            );
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    // 既存行の値が新しい名前で読める。古い名前は 42703。
    let cells = select_col(&core, "memo");
    assert_eq!(cells.len(), 1);
    assert!(matches!(&cells[0], Cell::Text(t) if t == "hello"));
    let err = core
        .execute_sql(&ctx(), &format!("SELECT note FROM {TABLE} LIMIT 1"))
        .expect_err("old name must be unknown");
    assert_eq!(err.wire_code(), "42703");

    // 新しい名前での INSERT・UPDATE・WHERE・DELETE。
    insert(&core, 2, "memo, qty", "'world', 9");
    let mut w = SessionState::default();
    run(
        &core,
        &mut w,
        &format!("UPDATE {TABLE} SET memo = 'changed' WHERE id = 2 USING OPERATION_ID 'op-upd-2'"),
    )
    .expect("update");
    let rows = core
        .execute_sql(
            &ctx(),
            &format!("SELECT qty FROM {TABLE} WHERE memo = 'changed' LIMIT 10"),
        )
        .expect("where on new name");
    assert_eq!(rows.rows.len(), 1);
    assert!(matches!(rows.rows[0].cells[0], Cell::SignedInteger(9)));
    run(
        &core,
        &mut w,
        &format!("DELETE FROM {TABLE} WHERE memo = 'hello' USING OPERATION_ID 'op-del-1'"),
    )
    .expect("delete");
    assert_eq!(select_col(&core, "memo").len(), 1);
}

#[test]
fn column_keyword_is_optional() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, 1, "note", "'x'");
    run(&core, &mut session, "ALTER TABLE docs RENAME note TO memo").expect("rename");
    assert_eq!(select_col(&core, "memo").len(), 1);
}

#[test]
fn renamed_column_survives_reopen() {
    let path = unique_db_path("sql-ddl-rename-column-reopen");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open");
        storage.create_table(&schema()).expect("create");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let mut session = ddl_session();
        insert(&core, 1, "note", "'persist'");
        run(
            &core,
            &mut session,
            "ALTER TABLE docs RENAME COLUMN note TO memo",
        )
        .expect("rename");
    }
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let cells = select_col(&core, "memo");
    assert!(matches!(&cells[0], Cell::Text(t) if t == "persist"));
}

#[test]
fn error_contract_and_decision_order() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    let cases = [
        // 存在しない表
        ("ALTER TABLE nope RENAME COLUMN a TO b", "42P01"),
        // 存在しない改名元
        ("ALTER TABLE docs RENAME COLUMN missing TO b", "42703"),
        // 既存列との衝突・同名への変更
        ("ALTER TABLE docs RENAME COLUMN note TO qty", "42701"),
        ("ALTER TABLE docs RENAME COLUMN note TO note", "42701"),
        // 予約名（構文段。表の存在より先に判定される）
        ("ALTER TABLE docs RENAME COLUMN id TO x", "42601"),
        ("ALTER TABLE docs RENAME COLUMN tenant_id TO x", "42601"),
        ("ALTER TABLE docs RENAME COLUMN note TO id", "42601"),
        ("ALTER TABLE docs RENAME COLUMN note TO visibility", "42601"),
        ("ALTER TABLE docs RENAME COLUMN note TO \"check\"", "42601"),
        ("ALTER TABLE nope RENAME COLUMN id TO x", "42601"),
        // 未対応の形
        ("ALTER TABLE docs RENAME TO other", "42601"),
        ("ALTER TABLE docs RENAME CONSTRAINT a TO b", "42601"),
        (
            "ALTER TABLE IF EXISTS docs RENAME COLUMN note TO memo",
            "42601",
        ),
        (
            "ALTER TABLE docs RENAME COLUMN note TO memo, ADD COLUMN z TEXT",
            "42601",
        ),
        ("ALTER TABLE docs RENAME COLUMN note", "42601"),
    ];
    for (sql, code) in cases {
        assert_eq!(code_of(&core, &mut session, sql), code, "{sql}");
    }
    // 拒否後もスキーマは不変。
    core.execute_sql(&ctx(), "SELECT note FROM docs LIMIT 1")
        .expect("note still exists");
}

#[test]
fn view_name_is_42809() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    run(
        &core,
        &mut session,
        "CREATE VIEW v_docs AS SELECT * FROM docs",
    )
    .expect("view");
    assert_eq!(
        code_of(
            &core,
            &mut session,
            "ALTER TABLE v_docs RENAME COLUMN note TO m"
        ),
        "42809"
    );
}

#[test]
fn permission_denial_is_identical_regardless_of_existence() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = SessionState::default();
    let mut messages = Vec::new();
    for sql in [
        "ALTER TABLE docs RENAME COLUMN note TO memo",
        "ALTER TABLE nope RENAME COLUMN note TO memo",
        "ALTER TABLE docs RENAME COLUMN missing TO memo",
        "ALTER TABLE docs RENAME COLUMN note TO qty",
    ] {
        let err = run(&core, &mut session, sql).expect_err("must be denied");
        assert_eq!(err.wire_code(), "42501", "{sql}");
        messages.push(err.to_string());
    }
    assert!(messages.windows(2).all(|w| w[0] == w[1]));
    core.execute_sql(&ctx(), "SELECT note FROM docs LIMIT 1")
        .expect("note still exists");
}

#[test]
fn dependent_columns_are_rejected_with_0a000_and_schema_is_unchanged() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    for sql in [
        "CREATE TABLE parents (code TEXT PRIMARY KEY, u TEXT UNIQUE, c INTEGER, free TEXT, CHECK (c > 0))",
        "CREATE TABLE children (pcode TEXT, y TEXT, FOREIGN KEY (pcode) REFERENCES parents (code))",
        "CREATE INDEX idx_note ON docs (note)",
    ] {
        run(&core, &mut session, sql).expect(sql);
    }
    for sql in [
        "ALTER TABLE parents RENAME COLUMN code TO k",
        "ALTER TABLE parents RENAME COLUMN u TO k",
        "ALTER TABLE parents RENAME COLUMN c TO k",
        "ALTER TABLE children RENAME COLUMN pcode TO k",
        "ALTER TABLE docs RENAME COLUMN note TO memo",
    ] {
        assert_eq!(code_of(&core, &mut session, sql), "0A000", "{sql}");
    }
    // 古い名前で引き続き動く。
    core.execute_sql(&ctx(), "SELECT code FROM parents LIMIT 1")
        .expect("code still exists");
    core.execute_sql(&ctx(), "SELECT note FROM docs LIMIT 1")
        .expect("note still exists");
    // 依存のない列は改名できる。
    run(
        &core,
        &mut session,
        "ALTER TABLE parents RENAME COLUMN free TO free2",
    )
    .expect("independent column");
}

#[test]
fn table_with_view_rejects_rename_conservatively() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    run(
        &core,
        &mut session,
        "CREATE VIEW v_docs AS SELECT note FROM docs",
    )
    .expect("view");
    assert_eq!(
        code_of(
            &core,
            &mut session,
            "ALTER TABLE docs RENAME COLUMN qty TO q"
        ),
        "0A000"
    );
}

#[test]
fn vector_column_can_be_renamed_and_searched() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, 1, "note", "'x'");
    run(
        &core,
        &mut session,
        "ALTER TABLE docs RENAME COLUMN embedding TO vec",
    )
    .expect("rename vector column");
    let rows = core
        .execute_sql(
            &ctx(),
            "SELECT id FROM docs ORDER BY vec <=> '[0.1,0.2]' LIMIT 5",
        )
        .expect("search on renamed vector column");
    assert_eq!(rows.rows.len(), 1);
}

#[test]
fn rename_to_dropped_column_name_and_readd_old_name() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, 1, "note, qty", "'keep', 3");
    run(&core, &mut session, "ALTER TABLE docs DROP COLUMN label").expect("drop");
    // 墓標と同じ名前への改名は許可され、墓標の値は復活しない。
    run(
        &core,
        &mut session,
        "ALTER TABLE docs RENAME COLUMN note TO label",
    )
    .expect("rename");
    let cells = select_col(&core, "label");
    assert!(matches!(&cells[0], Cell::Text(t) if t == "keep"));
    // 改名後、新しい名前での ADD は 42701、古い名前での ADD は成功し NULL。
    assert_eq!(
        code_of(
            &core,
            &mut session,
            "ALTER TABLE docs ADD COLUMN label TEXT"
        ),
        "42701"
    );
    run(&core, &mut session, "ALTER TABLE docs ADD COLUMN note TEXT").expect("re-add old name");
    assert!(matches!(select_col(&core, "note")[0], Cell::Null));
}
