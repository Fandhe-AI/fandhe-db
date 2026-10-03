//! `CREATE TABLE`／`ALTER TABLE ADD COLUMN` の配列型・ENUM 型・全スカラー型の列宣言
//! （Issue #1348。ポインタ: SQL-23・TABLE-6・TABLE-13・TABLE-14）の結合テスト。
//!
//! `EngineCore::execute_sql_in_session`（字句解析 → 許可リスト構文検証 → DDL 権限
//! ゲート → `sql::ddl` の型解決・`Storage` 反映）を production 経路として検証する。
//! 未知の型名が `42601`、権限なしが型の存在有無によらず `42501` になること
//! （存在オラクルにしない）、`DROP TYPE` の依存検査が SQL 宣言の列にも効くことを固定する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
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
    core.execute_sql_in_session(&ctx("alice"), s, sql)
}

fn code(core: &EngineCore, s: &mut SessionState, sql: &str) -> &'static str {
    match run(core, s, sql) {
        Ok(o) => panic!("{sql} must fail, got {o:?}"),
        Err(e) => e.wire_code(),
    }
}

#[test]
fn create_table_declares_array_enum_and_scalar_columns_and_round_trips() {
    let (path, _g, core) = open("ct-composite-roundtrip");
    let mut s = allowed();
    run(&core, &mut s, "CREATE TYPE mood AS ENUM ('happy', 'sad')").expect("create type");
    run(
        &core,
        &mut s,
        "CREATE TABLE t (embedding VECTOR(2) NOT NULL, m mood DEFAULT 'happy', \
         tags TEXT[3], nums INTEGER[], flag BOOLEAN, born DATE, price NUMERIC(10,2))",
    )
    .expect("create table");
    run(
        &core,
        &mut s,
        "INSERT INTO t (id, embedding, tags, nums) VALUES (1, '[0.1,0.2]', '{a,b}', '{1,2,3}') \
         USING OPERATION_ID 'op-1'",
    )
    .expect("insert");
    let result = core
        .execute_sql(&ctx("alice"), "SELECT id, m, tags, nums FROM t LIMIT 10")
        .expect("select");
    let row = result.rows.first().expect("one row");
    // ENUM 列の DEFAULT が補完される。
    assert!(
        format!("{:?}", row.cells[1]).contains("happy"),
        "got {:?}",
        row.cells[1]
    );
    assert!(matches!(row.cells[2], Cell::Array(_)));
    assert!(matches!(row.cells[3], Cell::Array(_)));

    // 要素数上限（TEXT[3]）超過の書き込みは 54000。
    assert_eq!(
        code(
            &core,
            &mut s,
            "INSERT INTO t (id, embedding, tags) VALUES (2, '[0.1,0.2]', '{a,b,c,d}') \
             USING OPERATION_ID 'op-2'"
        ),
        "54000"
    );
    // 再起動後も型が往復する。
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let schema = storage.get_table_schema("t").expect("schema");
    assert_eq!(schema.columns.len(), 7);
}

#[test]
fn unknown_and_invalid_types_are_rejected_with_42601() {
    let (_p, _g, core) = open("ct-composite-unknown");
    let mut s = allowed();
    for sql in [
        "CREATE TABLE t (a no_such_type)",
        "CREATE TABLE t (a no_such_type[])",
        "CREATE TABLE t (a INTEGER[0])",
        "CREATE TABLE t (a INTEGER[1025])",
        "CREATE TABLE t (a INTEGER[][])",
        "CREATE TABLE t (a INTEGER[-1])",
        "CREATE TABLE t (a VECTOR(2)[])",
        "CREATE TABLE t (a INTEGER[ )",
        "SELECT a[1] FROM t",
    ] {
        assert_eq!(code(&core, &mut s, sql), "42601", "{sql}");
    }
    // NUMERIC の精度・位取りが範囲外の配列要素は 42601（Issue #1357 で NUMERIC 要素は受理）。
    assert_eq!(
        code(&core, &mut s, "CREATE TABLE t (a NUMERIC(5,9)[])"),
        "42601"
    );
    // 配列列の DEFAULT は 0A000。
    assert_eq!(
        code(&core, &mut s, "CREATE TABLE t (a INTEGER[] DEFAULT '{1}')"),
        "0A000"
    );
}

#[test]
fn enum_column_default_outside_vocabulary_is_rejected() {
    let (_p, _g, core) = open("ct-composite-enum-default");
    let mut s = allowed();
    run(&core, &mut s, "CREATE TYPE mood AS ENUM ('happy')").expect("create type");
    assert_eq!(
        code(&core, &mut s, "CREATE TABLE t (m mood DEFAULT 'furious')"),
        "22P02"
    );
}

#[test]
fn drop_type_is_blocked_while_sql_declared_enum_column_exists() {
    let (_p, _g, core) = open("ct-composite-drop-type");
    let mut s = allowed();
    run(&core, &mut s, "CREATE TYPE mood AS ENUM ('a')").expect("create type");
    run(&core, &mut s, "CREATE TABLE t (m mood)").expect("create table");
    assert_eq!(code(&core, &mut s, "DROP TYPE mood"), "2BP01");
}

#[test]
fn ddl_permission_gate_precedes_type_resolution() {
    let (_p, _g, core) = open("ct-composite-perm");
    let mut denied = SessionState::default();
    // 型が存在しても存在しなくても同じ 42501（存在オラクルにならない）。
    run(&core, &mut allowed(), "CREATE TYPE mood AS ENUM ('a')").expect("create type");
    for sql in [
        "CREATE TABLE t (m mood)",
        "CREATE TABLE t (m no_such_type)",
        "CREATE TABLE t (m no_such_type[])",
        "CREATE TABLE t (m INTEGER[])",
    ] {
        assert_eq!(code(&core, &mut denied, sql), "42501", "{sql}");
    }
}

#[test]
fn add_column_accepts_array_columns_and_rejects_invalid_ones() {
    let (_p, _g, core) = open("ct-composite-add-column");
    let mut s = allowed();
    run(
        &core,
        &mut s,
        "CREATE TABLE t (embedding VECTOR(2) NOT NULL)",
    )
    .expect("create table");
    run(&core, &mut s, "ALTER TABLE t ADD COLUMN tags TEXT[3]").expect("add array column");
    run(&core, &mut s, "ALTER TABLE t ADD COLUMN nums INTEGER[]").expect("add array column");
    // Issue #1357: NUMERIC・BYTEA・JSON・JSONB 要素の配列列も追加できる。
    for sql in [
        "ALTER TABLE t ADD COLUMN n NUMERIC(5,2)[]",
        "ALTER TABLE t ADD COLUMN b BYTEA[]",
        "ALTER TABLE t ADD COLUMN j JSON[2]",
        "ALTER TABLE t ADD COLUMN jb JSONB[]",
    ] {
        run(&core, &mut s, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }
    for (sql, expected) in [
        ("ALTER TABLE t ADD COLUMN x no_such[]", "42601"),
        ("ALTER TABLE t ADD COLUMN x VECTOR(2)[]", "42601"),
        ("ALTER TABLE t ADD COLUMN x INTEGER[0]", "42601"),
        ("ALTER TABLE t ADD COLUMN x INTEGER[2000]", "42601"),
        ("ALTER TABLE t ADD COLUMN x NUMERIC(5,9)[]", "42601"),
        (
            "ALTER TABLE t ADD COLUMN x INTEGER[] DEFAULT '{1}'",
            "0A000",
        ),
        // 配列への型変更は互換性判定で拒否される。
        ("ALTER TABLE t ALTER COLUMN tags TYPE INTEGER[]", "42804"),
    ] {
        assert_eq!(code(&core, &mut s, sql), expected, "{sql}");
    }
}
