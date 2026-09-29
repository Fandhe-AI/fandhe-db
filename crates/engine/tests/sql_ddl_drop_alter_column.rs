//! `ALTER TABLE <t> DROP COLUMN <c>`／`ALTER TABLE <t> ALTER COLUMN <c> TYPE <型>`
//! （TABLE-19・SQL-23・ERR-6、Issue #1167）の SQL 表層結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（構文検証 → `require_ddl_permission` →
//! `sql::ddl::execute_alter_table_drop_column`／`execute_alter_table_alter_column_type`）を
//! production 経路として検証する（`sql_ddl_add_column.rs` と同じ流儀）。engine の
//! Rust API 側の契約は `table19_drop_alter_column.rs` が確定オラクル。

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

fn new_core() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("sql-ddl-drop-alter-column");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("note", ColumnType::Text, true),
                ColumnDef::new("qty", ColumnType::Integer, true),
                ColumnDef::new(
                    "amount",
                    ColumnType::Numeric {
                        precision: 5,
                        scale: 2,
                    },
                    true,
                ),
            ],
        ))
        .expect("create table");
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

#[test]
fn drop_column_succeeds_and_column_disappears() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, 1, "note, qty", "'hello', 7");

    let out = run(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} DROP COLUMN note;"),
    )
    .expect("drop column");
    match out {
        SqlOutcome::AlterTable(o) => {
            assert_eq!(o.table_name, TABLE);
            assert_eq!(
                o.action,
                AlterTableAction::DropColumn {
                    column_name: "note".to_string()
                }
            );
        }
        other => panic!("unexpected outcome: {other:?}"),
    }
    // 削除列は参照できず、他列は既存行のまま読める。
    let err = core
        .execute_sql(&ctx(), &format!("SELECT note FROM {TABLE} LIMIT 1"))
        .expect_err("dropped column must be unknown");
    assert_eq!(err.wire_code(), "22000");
    let rows = core
        .execute_sql(&ctx(), &format!("SELECT qty FROM {TABLE} LIMIT 10"))
        .expect("select qty");
    assert_eq!(rows.rows.len(), 1);
    assert!(matches!(rows.rows[0].cells[0], Cell::SignedInteger(7)));

    // 同名で再 ADD した列は既存行で NULL。
    run(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} ADD COLUMN note TEXT"),
    )
    .expect("re-add");
    let rows = core
        .execute_sql(&ctx(), &format!("SELECT note FROM {TABLE} LIMIT 10"))
        .expect("select note");
    assert!(matches!(rows.rows[0].cells[0], Cell::Null));
}

#[test]
fn alter_type_widens_numeric_and_preserves_values() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    insert(&core, 1, "amount", "123.45");

    let out = run(
        &core,
        &mut session,
        &format!("ALTER TABLE {TABLE} ALTER COLUMN amount TYPE NUMERIC(10,2)"),
    )
    .expect("widen");
    match out {
        SqlOutcome::AlterTable(o) => assert_eq!(
            o.action,
            AlterTableAction::AlterColumnType {
                column_name: "amount".to_string()
            }
        ),
        other => panic!("unexpected outcome: {other:?}"),
    }
    // 新しい精度でしか入らない値が入り、既存値も残る。
    insert(&core, 2, "amount", "12345678.90");
    let rows = core
        .execute_sql(&ctx(), &format!("SELECT amount FROM {TABLE} LIMIT 10"))
        .expect("select");
    assert_eq!(rows.rows.len(), 2);
}

#[test]
fn drop_column_error_contract() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    run(
        &core,
        &mut session,
        "CREATE TABLE keyed (code TEXT PRIMARY KEY, a TEXT, b INTEGER, UNIQUE (a), CHECK (b > 0))",
    )
    .expect("create keyed");

    for (sql, code) in [
        ("ALTER TABLE nope DROP COLUMN note", "42P01"),
        ("ALTER TABLE docs DROP COLUMN missing", "42703"),
        // VECTOR 列（保護列）。
        ("ALTER TABLE docs DROP COLUMN embedding", "42601"),
        // 予約名（大文字小文字の変種を含む）。
        ("ALTER TABLE docs DROP COLUMN id", "42601"),
        ("ALTER TABLE docs DROP COLUMN Tenant_Id", "42601"),
        ("ALTER TABLE docs DROP COLUMN VISIBILITY", "42601"),
        // PK・UNIQUE・CHECK が参照する列。
        ("ALTER TABLE keyed DROP COLUMN code", "2BP01"),
        ("ALTER TABLE keyed DROP COLUMN a", "2BP01"),
        ("ALTER TABLE keyed DROP COLUMN b", "2BP01"),
    ] {
        assert_eq!(code_of(&core, &mut session, sql), code, "{sql}");
    }
}

#[test]
fn alter_type_error_contract() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    run(
        &core,
        &mut session,
        "CREATE TABLE checked (a INTEGER, CHECK (a > 0))",
    )
    .expect("create checked");

    for (sql, code) in [
        ("ALTER TABLE nope ALTER COLUMN note TYPE TEXT", "42P01"),
        ("ALTER TABLE docs ALTER COLUMN missing TYPE TEXT", "42703"),
        ("ALTER TABLE docs ALTER COLUMN note TYPE INTEGER", "42804"),
        ("ALTER TABLE docs ALTER COLUMN qty TYPE BIGINT", "42804"),
        ("ALTER TABLE docs ALTER COLUMN note TYPE TEXT", "42804"),
        (
            "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(4,2)",
            "42804",
        ),
        (
            "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(10,3)",
            "42804",
        ),
        (
            "ALTER TABLE docs ALTER COLUMN embedding TYPE VECTOR(3)",
            "42804",
        ),
        ("ALTER TABLE docs ALTER COLUMN embedding TYPE TEXT", "42804"),
        // NUMERIC の範囲不正は互換性判定より先に 42601。
        (
            "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(5,6)",
            "42601",
        ),
        (
            "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(39,2)",
            "42601",
        ),
        // 未登録 ENUM 型名。
        (
            "ALTER TABLE docs ALTER COLUMN note TYPE no_such_enum",
            "42601",
        ),
        // 予約名。
        ("ALTER TABLE docs ALTER COLUMN id TYPE TEXT", "42601"),
        ("ALTER TABLE docs ALTER COLUMN TENANT_ID TYPE TEXT", "42601"),
        // CHECK が参照する列。
        ("ALTER TABLE checked ALTER COLUMN a TYPE BIGINT", "2BP01"),
    ] {
        assert_eq!(code_of(&core, &mut session, sql), code, "{sql}");
    }
}

#[test]
fn view_names_are_rejected_with_42809() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    run(
        &core,
        &mut session,
        "CREATE VIEW v_docs AS SELECT * FROM docs",
    )
    .expect("create view");
    for sql in [
        "ALTER TABLE v_docs DROP COLUMN note",
        "ALTER TABLE v_docs ALTER COLUMN amount TYPE NUMERIC(10,2)",
    ] {
        assert_eq!(code_of(&core, &mut session, sql), "42809", "{sql}");
    }
}

#[test]
fn reserved_check_and_constraint_names_and_last_column_are_42601() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    run(&core, &mut session, "CREATE TABLE solo (only_col TEXT)").expect("create solo");
    for sql in [
        "ALTER TABLE docs DROP COLUMN check",
        "ALTER TABLE docs ALTER COLUMN constraint TYPE TEXT",
        // 最後の 1 列（`Invalid` -> 42601）。
        "ALTER TABLE solo DROP COLUMN only_col",
    ] {
        assert_eq!(code_of(&core, &mut session, sql), "42601", "{sql}");
    }
}

#[test]
fn foreign_key_columns_cannot_be_dropped() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    run(
        &core,
        &mut session,
        "CREATE TABLE parents (code TEXT PRIMARY KEY, x TEXT)",
    )
    .expect("create parents");
    run(
        &core,
        &mut session,
        "CREATE TABLE children (pcode TEXT, y TEXT, FOREIGN KEY (pcode) REFERENCES parents (code))",
    )
    .expect("create children");
    // 子側の参照元列・親側の被参照列はどちらも 2BP01。
    assert_eq!(
        code_of(
            &core,
            &mut session,
            "ALTER TABLE children DROP COLUMN pcode"
        ),
        "2BP01"
    );
    assert_eq!(
        code_of(&core, &mut session, "ALTER TABLE parents DROP COLUMN code"),
        "2BP01"
    );
}

#[test]
fn unsupported_qualifiers_are_rejected_with_42601() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    for sql in [
        "ALTER TABLE docs DROP note",
        "ALTER TABLE docs DROP COLUMN IF EXISTS note",
        "ALTER TABLE docs DROP COLUMN note CASCADE",
        "ALTER TABLE docs DROP COLUMN note RESTRICT",
        "ALTER TABLE docs DROP COLUMN note, DROP COLUMN qty",
        "ALTER TABLE docs DROP COLUMN note USING OPERATION_ID 'op-1'",
        "ALTER TABLE docs DROP COLUMN note RETURNING id",
        "ALTER TABLE docs DROP COLUMN note; SELECT 1",
        "ALTER TABLE IF EXISTS docs DROP COLUMN note",
        "ALTER TABLE ONLY docs DROP COLUMN note",
        "ALTER TABLE docs ALTER amount TYPE NUMERIC(9,2)",
        "ALTER TABLE docs ALTER COLUMN amount SET DATA TYPE NUMERIC(9,2)",
        "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(9,2) USING amount",
        "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(9,2) COLLATE x",
        "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(9,2), ALTER COLUMN qty TYPE BIGINT",
        "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(9,2) RETURNING id",
        "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(9,2); SELECT 1",
    ] {
        assert_eq!(code_of(&core, &mut session, sql), "42601", "{sql}");
    }
}

#[test]
fn permission_denial_is_identical_regardless_of_existence_and_has_no_side_effects() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = SessionState::default();
    let mut messages = Vec::new();
    for sql in [
        "ALTER TABLE docs DROP COLUMN note",
        "ALTER TABLE nope DROP COLUMN note",
        "ALTER TABLE docs DROP COLUMN missing",
        "ALTER TABLE docs DROP COLUMN embedding",
        "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(10,2)",
        "ALTER TABLE nope ALTER COLUMN x TYPE TEXT",
    ] {
        let err = run(&core, &mut session, sql).expect_err("must be denied");
        assert_eq!(err.wire_code(), "42501", "{sql}");
        messages.push(err.to_string());
    }
    assert!(messages.windows(2).all(|w| w[0] == w[1]));
    // 拒否後もスキーマは不変（note 列は参照できる）。
    core.execute_sql(&ctx(), &format!("SELECT note FROM {TABLE} LIMIT 1"))
        .expect("note still exists");
}

#[test]
fn ddl_inside_explicit_transaction_is_rejected_with_0a000() {
    let (core, path) = new_core();
    let _guard = CleanupGuard(path);
    let mut session = ddl_session();
    let owner = ctx();
    for sql in [
        "ALTER TABLE docs DROP COLUMN note",
        "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(10,2)",
    ] {
        let mut txn = core.new_session_transaction();
        core.execute_sql_in_txn(&owner, &mut session, &mut txn, "BEGIN")
            .expect("begin");
        let err = core
            .execute_sql_in_txn(&owner, &mut session, &mut txn, sql)
            .expect_err("DDL inside an explicit transaction must be rejected");
        assert_eq!(err.wire_code(), "0A000", "{sql}");
    }
}
