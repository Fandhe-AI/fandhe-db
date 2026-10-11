//! PostgreSQL 型名エイリアス（`INT`・`INT4`・`INT8`・`FLOAT4`・`FLOAT8`）の結合テスト
//! （Issue #1528。ポインタ: TABLE-29・TABLE-31）。
//!
//! 型名 → 列型の対応表は `sql::ddl_column_type` に一本化されている。ここでは
//! `EngineCore::execute_sql_in_session`（字句解析 → 構文検証 → DDL 権限ゲート →
//! 型解決 → `Storage` 反映）の production 経路で、別名が正規名と同じ列型として
//! 永続化されること、未確定型が両綴りとも同一の `0A000` で副作用なく拒否されること、
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

/// 未確定型（`INT2`／`SMALLINT`・`VARCHAR`／`CHARACTER VARYING`・`CHAR`／`CHARACTER`）は
/// `0A000` で拒否し、真に未知の型名・`DOUBLE` 単独は従来どおり `42601`（Issue #2303。
/// ポインタ: TABLE-29・TABLE-30・TABLE-31）。拒否後はテーブルが作られない。
#[test]
fn unsettled_type_names_are_rejected_with_0a000_without_side_effects() {
    let (path, _g, core) = open("ta-unsettled");
    let mut s = allowed();
    for sql in [
        "CREATE TABLE t (a INT2)",
        "CREATE TABLE t (a int2)",
        "CREATE TABLE t (a SMALLINT)",
        "CREATE TABLE t (a VARCHAR)",
        "CREATE TABLE t (a VARCHAR(10))",
        "CREATE TABLE t (a CHARACTER VARYING(10))",
        "CREATE TABLE t (a CHAR)",
        "CREATE TABLE t (a CHAR(1))",
        "CREATE TABLE t (a CHARACTER(1))",
    ] {
        assert_eq!(err(&core, &mut s, sql).wire_code(), "0A000", "{sql}");
    }
    for sql in ["CREATE TABLE t (a DOUBLE)", "CREATE TABLE t (a INT16)"] {
        assert_eq!(err(&core, &mut s, sql).wire_code(), "42601", "{sql}");
    }
    let unknown = err(&core, &mut s, "CREATE TABLE t (a INT16)").to_string();
    assert!(unknown.contains("unknown type name"), "{unknown}");
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    assert!(
        storage.get_table_schema("t").is_err(),
        "rejected DDL must leave no table"
    );
}

/// 同一型の両綴りが CREATE TABLE・ADD COLUMN・ALTER COLUMN TYPE の全経路で
/// 同じ `wire_code` とメッセージになり、拒否後にスキーマが変わらない。
#[test]
fn unsettled_spellings_match_on_every_ddl_path_and_leave_schema() {
    let (path, _g, core) = open("ta-unsettled-paths");
    let mut s = allowed();
    run(&core, &mut s, "CREATE TABLE t (a INT)").expect("create");
    let pairs = [
        ("INT2", "SMALLINT"),
        ("VARCHAR(10)", "CHARACTER VARYING(10)"),
        ("CHAR(1)", "CHARACTER(1)"),
    ];
    for (x, y) in pairs {
        let paths: [fn(&str) -> String; 3] = [
            |t| format!("CREATE TABLE u (a {t})"),
            |t| format!("ALTER TABLE t ADD COLUMN b {t}"),
            |t| format!("ALTER TABLE t ALTER COLUMN a TYPE {t}"),
        ];
        for mk in paths {
            let ex = err(&core, &mut s, &mk(x));
            let ey = err(&core, &mut s, &mk(y));
            assert_eq!(ex.wire_code(), "0A000", "{}", mk(x));
            assert_eq!(ey.wire_code(), "0A000", "{}", mk(y));
            assert_eq!(ex.to_string(), ey.to_string(), "{x} vs {y}");
        }
    }
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    assert!(storage.get_table_schema("u").is_err());
    let schema = storage.get_table_schema("t").expect("schema");
    assert!(schema.columns.iter().all(|c| c.name != "b"));
    let a = schema.columns.iter().find(|c| c.name == "a").expect("a");
    assert!(matches!(a.ty, ColumnType::Integer));
}

/// 未確定型の `0A000` は構文段（カタログ非参照）で決まるため、DDL 権限の有無で同じ
/// コードになる。実行段で解決される型（別名・ENUM 候補）は従来どおり権限ゲートが先
/// （`42501`）。Issue #2303 による判定順序の契約変更の明文化。
#[test]
fn ddl_permission_gate_precedes_runtime_resolution_but_not_syntax_rejection() {
    let (_p, _g, core) = open("ta-permission");
    let mut denied = SessionState::default();
    for sql in ["CREATE TABLE t (a INT4)", "CREATE TABLE t (a mood)"] {
        assert_eq!(err(&core, &mut denied, sql).wire_code(), "42501", "{sql}");
    }
    let mut ok = allowed();
    for sql in ["CREATE TABLE t (a INT2)", "CREATE TABLE t (a SMALLINT)"] {
        assert_eq!(err(&core, &mut denied, sql).wire_code(), "0A000", "{sql}");
        assert_eq!(err(&core, &mut ok, sql).wire_code(), "0A000", "{sql}");
    }
}

#[test]
fn create_type_rejects_names_that_alias_builtin_column_types() {
    let (path, _g, core) = open("ta-enum-collision");
    let mut s = allowed();
    for name in [
        "int4",
        "INT8",
        "Float4",
        "float8",
        "int2",
        "SMALLINT",
        "varchar",
        "Char",
        "character",
    ] {
        let sql = format!("CREATE TYPE {name} AS ENUM ('a')");
        let _ = err(&core, &mut s, &sql);
    }
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    for name in [
        "int4",
        "int8",
        "float4",
        "float8",
        "int2",
        "smallint",
        "varchar",
        "char",
        "character",
    ] {
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

/// 同一型・未確定型・権限なしの扱いが正規名と一致し、スキーマ不変である。
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
    for ty in ["INT2", "SMALLINT", "VARCHAR(10)"] {
        let sql = format!("ALTER TABLE t ALTER COLUMN a TYPE {ty}");
        assert_eq!(err(&core, &mut s, &sql).wire_code(), "0A000", "{sql}");
    }
    let mut denied = SessionState::default();
    assert_eq!(
        err(&core, &mut denied, "ALTER TABLE t ALTER COLUMN a TYPE INT8").wire_code(),
        "42501"
    );
    drop(core);
    assert!(matches!(column_type(&path, "t", "a"), ColumnType::Integer));
}
/// Issue #2304（TABLE-29）: `BOOL`・`FLOAT`・`FLOAT(p)`・`TIMESTAMP WITHOUT TIME ZONE` が
/// CREATE TABLE・ADD COLUMN で正規型として永続化される。
#[test]
fn issue_2304_aliases_persist_as_canonical_types() {
    let (path, _g, core) = open("ta-2304-persist");
    let mut s = allowed();
    run(
        &core,
        &mut s,
        "CREATE TABLE a (embedding VECTOR(2) NOT NULL, c1 BOOL, c2 FLOAT, c3 FLOAT(24), c4 FLOAT(25), c5 float(53), \
         c6 TIMESTAMP WITHOUT TIME ZONE, c7 bool[], c8 FLOAT(1))",
    )
    .expect("create alias table");
    run(
        &core,
        &mut s,
        "CREATE TABLE b (embedding VECTOR(2) NOT NULL, c1 BOOLEAN, c2 DOUBLE PRECISION, c3 REAL, c4 DOUBLE PRECISION, \
         c5 DOUBLE PRECISION, c6 TIMESTAMP, c7 BOOLEAN[], c8 REAL)",
    )
    .expect("create canonical table");
    run(
        &core,
        &mut s,
        "ALTER TABLE a ADD COLUMN c9 timestamp without time zone",
    )
    .expect("add column");
    run(&core, &mut s, "ALTER TABLE a ADD COLUMN c10 FLOAT(30)").expect("add float");
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let a = storage.get_table_schema("a").expect("schema a");
    let b = storage.get_table_schema("b").expect("schema b");
    for (x, y) in a.columns.iter().zip(b.columns.iter()) {
        assert_eq!(x.ty, y.ty, "column {}", x.name);
    }
    let ty_of = |n: &str| {
        a.columns
            .iter()
            .find(|c| c.name == n)
            .expect("col")
            .ty
            .clone()
    };
    assert!(matches!(ty_of("c9"), ColumnType::Timestamp));
    assert!(matches!(ty_of("c10"), ColumnType::Double));
}

/// `FLOAT`／`FLOAT(53)` への ALTER は REAL 列からの拡大として成功し、
/// `BOOL`・`FLOAT(24)`・`TIMESTAMP WITHOUT TIME ZONE` は正規名と同じ wire_code で拒否される。
#[test]
fn issue_2304_alter_column_type_matches_canonical_names() {
    let (path, _g, core) = open("ta-2304-alter");
    let mut s = allowed();
    run(
        &core,
        &mut s,
        "CREATE TABLE t (embedding VECTOR(2) NOT NULL, r1 REAL, r2 REAL, b BOOLEAN, \
         f REAL, ts TIMESTAMP)",
    )
    .expect("create");
    run(&core, &mut s, "ALTER TABLE t ALTER COLUMN r1 TYPE FLOAT").expect("float");
    run(
        &core,
        &mut s,
        "ALTER TABLE t ALTER COLUMN r2 TYPE FLOAT(53)",
    )
    .expect("float(53)");
    for (alias, canonical, col) in [
        ("BOOL", "BOOLEAN", "b"),
        ("FLOAT(24)", "REAL", "f"),
        ("TIMESTAMP WITHOUT TIME ZONE", "TIMESTAMP", "ts"),
    ] {
        let a = err(
            &core,
            &mut s,
            &format!("ALTER TABLE t ALTER COLUMN {col} TYPE {alias}"),
        );
        let c = err(
            &core,
            &mut s,
            &format!("ALTER TABLE t ALTER COLUMN {col} TYPE {canonical}"),
        );
        assert_eq!(a.wire_code(), c.wire_code(), "{alias}");
    }
    drop(core);
    assert!(matches!(column_type(&path, "t", "r1"), ColumnType::Double));
    assert!(matches!(column_type(&path, "t", "r2"), ColumnType::Double));
    assert!(matches!(column_type(&path, "t", "f"), ColumnType::Real));
}

/// `FLOAT(0)`・`FLOAT(54)` は 3 経路とも `22023`・固定文言で、副作用はなく、
/// DDL 権限の有無にかかわらず構文段で決まる。
#[test]
fn issue_2304_float_precision_out_of_range_is_22023_without_side_effects() {
    let (path, _g, core) = open("ta-2304-22023");
    let mut s = allowed();
    run(
        &core,
        &mut s,
        "CREATE TABLE t (embedding VECTOR(2) NOT NULL, a REAL)",
    )
    .expect("create");
    let cases = [
        (
            "FLOAT(0)",
            "precision for type float must be at least 1 bit",
        ),
        (
            "FLOAT(54)",
            "precision for type float must be less than 54 bits",
        ),
        (
            "FLOAT(99999999999)",
            "precision for type float must be less than 54 bits",
        ),
    ];
    for (ty, msg) in cases {
        let paths = [
            format!("CREATE TABLE u (a {ty})"),
            format!("ALTER TABLE t ADD COLUMN b {ty}"),
            format!("ALTER TABLE t ALTER COLUMN a TYPE {ty}"),
        ];
        for sql in paths {
            let e = err(&core, &mut s, &sql);
            assert_eq!(e.wire_code(), "22023", "{sql}");
            assert_eq!(e.to_string(), msg, "{sql}");
        }
    }
    let mut denied = SessionState::default();
    assert_eq!(
        err(&core, &mut denied, "CREATE TABLE u (a FLOAT(0))").wire_code(),
        "22023"
    );
    assert_eq!(
        err(&core, &mut denied, "CREATE TABLE u (a FLOAT)").wire_code(),
        "42501"
    );
    for sql in ["CREATE TABLE u (a FLOAT())", "CREATE TABLE u (a FLOAT(-1))"] {
        assert_eq!(err(&core, &mut s, sql).wire_code(), "42601", "{sql}");
    }
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    assert!(storage.get_table_schema("u").is_err());
    let schema = storage.get_table_schema("t").expect("schema");
    assert!(schema.columns.iter().all(|c| c.name != "b"));
    let a = schema.columns.iter().find(|c| c.name == "a").expect("a");
    assert!(matches!(a.ty, ColumnType::Real));
}

/// 既存 ENUM との衝突防止: `CREATE TYPE float`・`bool` は作成できない。
#[test]
fn issue_2304_create_type_rejects_float_and_bool() {
    let (path, _g, core) = open("ta-2304-enum");
    let mut s = allowed();
    for name in ["float", "FLOAT", "bool", "timestamp"] {
        let _ = err(&core, &mut s, &format!("CREATE TYPE {name} AS ENUM ('a')"));
    }
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    for name in ["float", "bool", "timestamp"] {
        assert!(storage.get_enum_type(name).is_err(), "{name}");
    }
}
