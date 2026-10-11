//! PostgreSQL 型名エイリアス（`INT`・`INT4`・`INT8`・`FLOAT4`・`FLOAT8`）の結合テスト
//! （Issue #1528。ポインタ: TABLE-29・TABLE-31）。
//!
//! 型名 → 列型の対応表は `sql::ddl_column_type` に一本化されている。ここでは
//! `EngineCore::execute_sql_in_session`（字句解析 → 構文検証 → DDL 権限ゲート →
//! 型解決 → `Storage` 反映）の production 経路で、別名が正規名と同じ列型として
//! 永続化されること、未登録の型名が従来の `42601` のまま副作用なく拒否されること、
//! 権限ゲートが型解決より先であること（`42501`）を固定する。

use engine::catalog::ColumnType;
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

fn allowed() -> SessionState {
    let mut s = SessionState::default();
    s.allow_ddl();
    s
}

fn open(label: &str) -> (std::path::PathBuf, CleanupGuard, EngineCore) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        path,
        guard,
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
    )
}

fn run(core: &EngineCore, s: &mut SessionState, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    core.execute_sql_in_session(&ctx(), s, sql)
}

fn err(core: &EngineCore, s: &mut SessionState, sql: &str) -> SqlSurfaceError {
    match run(core, s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e,
    }
}

#[test]
fn aliases_persist_as_the_canonical_column_types() {
    let (path, _g, core) = open("ta-alias-persist");
    let mut s = allowed();
    run(
        &core,
        &mut s,
        "CREATE TABLE a (c1 INT, c2 int4, c3 INT8, c4 FLOAT4, c5 float8, c6 Int4[])",
    )
    .expect("create alias table");
    run(
        &core,
        &mut s,
        "CREATE TABLE b (c1 INTEGER, c2 INTEGER, c3 BIGINT, c4 REAL, c5 DOUBLE PRECISION, \
         c6 INTEGER[])",
    )
    .expect("create canonical table");
    run(&core, &mut s, "ALTER TABLE a ADD COLUMN c7 INT8").expect("add column");
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let a = storage.get_table_schema("a").expect("schema a");
    let b = storage.get_table_schema("b").expect("schema b");
    for (x, y) in a.columns.iter().zip(b.columns.iter()) {
        assert_eq!(x.ty, y.ty, "column {}", x.name);
    }
    assert!(matches!(a.columns[0].ty, ColumnType::Integer));
    assert!(matches!(a.columns[2].ty, ColumnType::BigInt));
    assert!(matches!(a.columns[3].ty, ColumnType::Real));
    assert!(matches!(a.columns[4].ty, ColumnType::Double));
    assert!(matches!(a.columns[5].ty, ColumnType::Array(_)));
    let added = a.columns.last().expect("added column");
    assert_eq!(added.name, "c7");
    assert!(matches!(added.ty, ColumnType::BigInt));
}

#[test]
fn alias_declared_columns_round_trip_values() {
    let (_p, _g, core) = open("ta-alias-roundtrip");
    let mut s = allowed();
    run(
        &core,
        &mut s,
        "CREATE TABLE t (embedding VECTOR(2) NOT NULL, n INT4, big INT8, f FLOAT8)",
    )
    .expect("create");
    run(
        &core,
        &mut s,
        "INSERT INTO t (id, embedding, n, big, f) VALUES (1, '[0.1,0.2]', 7, 9000000000, 1.5) \
         USING OPERATION_ID 'op-1'",
    )
    .expect("insert");
    let result = core
        .execute_sql(&ctx(), "SELECT id, n, big, f FROM t LIMIT 10")
        .expect("select");
    assert_eq!(result.rows.len(), 1);
}

#[test]
fn unregistered_type_names_keep_their_rejection_without_side_effects() {
    let (path, _g, core) = open("ta-unregistered");
    let mut s = allowed();
    for sql in [
        "CREATE TABLE t (a INT2)",
        "CREATE TABLE t (a int2)",
        "CREATE TABLE t (a VARCHAR)",
        "CREATE TABLE t (a VARCHAR(10))",
        "CREATE TABLE t (a CHAR)",
        "CREATE TABLE t (a CHAR(1))",
        "CREATE TABLE t (a DOUBLE)",
    ] {
        assert_eq!(err(&core, &mut s, sql).wire_code(), "42601", "{sql}");
    }
    let unknown = err(&core, &mut s, "CREATE TABLE t (a INT2)").to_string();
    assert!(unknown.contains("unknown type name"), "{unknown}");
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    assert!(
        storage.get_table_schema("t").is_err(),
        "rejected DDL must leave no table"
    );
}

#[test]
fn ddl_permission_gate_precedes_alias_resolution() {
    let (_p, _g, core) = open("ta-permission");
    let mut denied = SessionState::default();
    for sql in ["CREATE TABLE t (a INT4)", "CREATE TABLE t (a INT2)"] {
        assert_eq!(err(&core, &mut denied, sql).wire_code(), "42501", "{sql}");
    }
}

#[test]
fn create_type_rejects_names_that_alias_builtin_column_types() {
    let (path, _g, core) = open("ta-enum-collision");
    let mut s = allowed();
    for name in ["int4", "INT8", "Float4", "float8"] {
        let sql = format!("CREATE TYPE {name} AS ENUM ('a')");
        let _ = err(&core, &mut s, &sql);
    }
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    for name in ["int4", "int8", "float4", "float8"] {
        assert!(
            storage.get_enum_type(name).is_err(),
            "enum type {name} must not be created"
        );
    }
}

fn column_type(path: &std::path::Path, table: &str, col: &str) -> ColumnType {
    let storage = Storage::open(path).expect("reopen");
    let schema = storage.get_table_schema(table).expect("schema");
    schema
        .columns
        .iter()
        .find(|c| c.name == col)
        .expect("column")
        .ty
        .clone()
}

/// `ALTER COLUMN ... TYPE <別名>` が正規名と同じ拡張変換として通る（Issue #1529・TABLE-29）。
#[test]
fn alter_column_type_accepts_aliases_like_canonical_names() {
    let (path, _g, core) = open("ta-alter-type");
    let mut s = allowed();
    run(
        &core,
        &mut s,
        "CREATE TABLE t (embedding VECTOR(2) NOT NULL, a INT, b INTEGER, c FLOAT4, d REAL)",
    )
    .expect("create");
    run(
        &core,
        &mut s,
        "INSERT INTO t (id, embedding, a, b, c, d) VALUES (1, '[0.1,0.2]', 7, 8, 1.5, 2.5) \
         USING OPERATION_ID 'op-1'",
    )
    .expect("insert");
    run(&core, &mut s, "ALTER TABLE t ALTER COLUMN a TYPE INT8").expect("alias int8");
    run(&core, &mut s, "ALTER TABLE t ALTER COLUMN b TYPE int8").expect("lowercase alias");
    run(&core, &mut s, "ALTER TABLE t ALTER COLUMN c TYPE FLOAT8").expect("alias float8");
    run(
        &core,
        &mut s,
        "ALTER TABLE t ALTER COLUMN d TYPE DOUBLE PRECISION",
    )
    .expect("canonical");
    let result = core
        .execute_sql(&ctx(), "SELECT id, a, b FROM t LIMIT 10")
        .expect("select");
    assert_eq!(result.rows.len(), 1);
    drop(core);
    assert!(matches!(column_type(&path, "t", "a"), ColumnType::BigInt));
    assert!(matches!(column_type(&path, "t", "b"), ColumnType::BigInt));
    assert!(matches!(column_type(&path, "t", "c"), ColumnType::Double));
    assert!(matches!(column_type(&path, "t", "d"), ColumnType::Double));
}

/// 同一型・未登録型・権限なしの扱いが正規名と一致し、スキーマ不変である。
#[test]
fn alter_column_type_alias_failures_match_canonical_and_leave_schema() {
    let (path, _g, core) = open("ta-alter-type-fail");
    let mut s = allowed();
    run(
        &core,
        &mut s,
        "CREATE TABLE t (embedding VECTOR(2) NOT NULL, a INT)",
    )
    .expect("create");
    let canon = err(&core, &mut s, "ALTER TABLE t ALTER COLUMN a TYPE INTEGER").wire_code();
    let alias = err(&core, &mut s, "ALTER TABLE t ALTER COLUMN a TYPE INT4").wire_code();
    assert_eq!(canon, alias);
    assert_eq!(
        err(&core, &mut s, "ALTER TABLE t ALTER COLUMN a TYPE INT2").wire_code(),
        "42601"
    );
    let mut denied = SessionState::default();
    assert_eq!(
        err(&core, &mut denied, "ALTER TABLE t ALTER COLUMN a TYPE INT8").wire_code(),
        "42501"
    );
    drop(core);
    assert!(matches!(column_type(&path, "t", "a"), ColumnType::Integer));
}
