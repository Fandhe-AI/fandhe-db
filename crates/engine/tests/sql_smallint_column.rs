//! `SMALLINT`／`INT2` 列（`ColumnType::SmallInt`。TABLE-30・Issue #1558）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) 型がカタログへ永続化され再オープン
//! 後も復元されること、(2) INSERT・UPDATE SET・UPSERT・COPY・列 DEFAULT の値束縛が
//! `-32768..=32767` 外を `22003` で拒否し副作用を残さないこと、(3) 兄弟 Issue（#1559・#1560・
//! #1561）が担当する機能（述語・集計・索引・PK／UNIQUE・型変更）が暫定で fail-closed に拒否される
//! こと、を固定する。(3) は兄弟 Issue で解禁する際に反転する前提の暫定挙動である。

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

fn exec(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut s, sql)
}

fn ok(core: &EngineCore, sql: &str) {
    exec(core, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn code(core: &EngineCore, sql: &str) -> String {
    exec(core, sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
}

fn rows(core: &EngineCore, sql: &str) -> Vec<String> {
    core.execute_sql(&ctx(), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
        .rows
        .iter()
        .map(|r| format!("{:?}", r.cells))
        .collect()
}

fn column_type(path: &std::path::Path, table: &str, col: &str) -> ColumnType {
    let storage = Storage::open(path).expect("reopen");
    storage
        .get_table_schema(table)
        .expect("schema")
        .columns
        .iter()
        .find(|c| c.name == col)
        .expect("column")
        .ty
        .clone()
}

#[test]
fn smallint_and_int2_are_persisted_as_the_same_type() {
    let (path, _g, core) = open("si-persist");
    ok(&core, "CREATE TABLE t (a SMALLINT, b int2, c INTEGER)");
    ok(&core, "ALTER TABLE t ADD COLUMN d SMALLINT");
    ok(&core, "ALTER TABLE t ADD COLUMN e SMALLINT DEFAULT 7");
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::SmallInt);
    assert_eq!(column_type(&path, "t", "b"), ColumnType::SmallInt);
    assert_eq!(column_type(&path, "t", "c"), ColumnType::Integer);
    assert_eq!(column_type(&path, "t", "d"), ColumnType::SmallInt);
    assert_eq!(column_type(&path, "t", "e"), ColumnType::SmallInt);
}

#[test]
fn column_default_is_range_checked() {
    let (_p, _g, core) = open("si-default");
    assert_eq!(
        code(&core, "CREATE TABLE t (a SMALLINT DEFAULT 32768)"),
        "22003"
    );
    assert_eq!(
        code(&core, "CREATE TABLE t (a SMALLINT DEFAULT -32769)"),
        "22003"
    );
    ok(&core, "CREATE TABLE t (a SMALLINT DEFAULT -32768)");
    assert_eq!(
        code(&core, "ALTER TABLE t ADD COLUMN b SMALLINT DEFAULT 99999"),
        "22003"
    );
}

#[test]
fn insert_accepts_boundaries_and_rejects_out_of_range() {
    let (_p, _g, core) = open("si-insert");
    ok(&core, "CREATE TABLE t (n SMALLINT)");
    for (i, v) in ["-32768", "32767", "0", "NULL"].iter().enumerate() {
        ok(
            &core,
            &format!(
                "INSERT INTO t (id, n) VALUES ({}, {v}) USING OPERATION_ID 'ok{i}'",
                i + 1
            ),
        );
    }
    let got = rows(&core, "SELECT id, n FROM t LIMIT 10");
    assert_eq!(got.len(), 4, "{got:?}");
    for want in ["-32768", "32767", "0"] {
        assert!(
            got.iter()
                .any(|r| r.contains(&format!("SignedInteger({want})"))),
            "missing {want} in {got:?}"
        );
    }
    for (i, (v, want)) in [
        ("32768", "22003"),
        ("-32769", "22003"),
        ("99999999999", "22003"),
        ("'1'", "22P02"),
        ("1.5", "22P02"),
        ("true", "22000"),
    ]
    .iter()
    .enumerate()
    {
        assert_eq!(
            code(
                &core,
                &format!(
                    "INSERT INTO t (id, n) VALUES ({}, {v}) USING OPERATION_ID 'bad{i}'",
                    100 + i
                )
            ),
            *want,
            "for {v}"
        );
    }
    assert_eq!(
        rows(&core, "SELECT id, n FROM t LIMIT 10").len(),
        4,
        "rejected rows must not be written"
    );
}

#[test]
fn update_set_literal_is_range_checked_without_side_effects() {
    let (_p, _g, core) = open("si-update");
    ok(&core, "CREATE TABLE t (n SMALLINT)");
    ok(
        &core,
        "INSERT INTO t (id, n) VALUES (1, 5) USING OPERATION_ID 'i1'",
    );
    assert_eq!(
        code(
            &core,
            "UPDATE t SET n = 32768 WHERE id = 1 USING OPERATION_ID 'u1'"
        ),
        "22003"
    );
    assert!(rows(&core, "SELECT id, n FROM t LIMIT 10")[0].contains("SignedInteger(5)"));
    ok(
        &core,
        "UPDATE t SET n = -32768 WHERE id = 1 USING OPERATION_ID 'u2'",
    );
    assert!(rows(&core, "SELECT id, n FROM t LIMIT 10")[0].contains("SignedInteger(-32768)"));
}

#[test]
fn upsert_is_range_checked_on_both_sides() {
    let (_p, _g, core) = open("si-upsert");
    ok(&core, "CREATE TABLE t (n SMALLINT)");
    ok(
        &core,
        "INSERT INTO t (id, n) VALUES (1, 5) USING OPERATION_ID 'i1'",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, n) VALUES (1, 40000) ON CONFLICT (id) DO UPDATE SET n = 1 USING OPERATION_ID 'x1'"
        ),
        "22003"
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, n) VALUES (1, 6) ON CONFLICT (id) DO UPDATE SET n = 40000 USING OPERATION_ID 'x2'"
        ),
        "22003"
    );
    assert!(rows(&core, "SELECT id, n FROM t LIMIT 10")[0].contains("SignedInteger(5)"));
}

/// 兄弟 Issue（#1559・#1560）が解禁するまでの暫定挙動: INTEGER 扱いで黙って通さず拒否する。
#[test]
fn features_owned_by_sibling_issues_are_rejected_fail_closed() {
    let (_p, _g, core) = open("si-failclosed");
    ok(&core, "CREATE TABLE t (n SMALLINT, m INTEGER)");
    ok(
        &core,
        "INSERT INTO t (id, n, m) VALUES (1, 5, 5) USING OPERATION_ID 'i1'",
    );
    // 同形の INTEGER 列 `m` への問い合わせは通る（拒否理由が構文ではなく SMALLINT であること）。
    for sql in [
        "SELECT id, m FROM t WHERE m = 5 LIMIT 10",
        "SELECT SUM(m) FROM t",
        "SELECT m, COUNT(*) FROM t GROUP BY m LIMIT 10",
        "SELECT id, m FROM t ORDER BY m LIMIT 10",
    ] {
        ok(&core, sql);
    }
    for (sql, want) in [
        ("SELECT id, n FROM t WHERE n = 1 LIMIT 10", "22000"),
        ("SELECT SUM(n) FROM t", "42883"),
        ("SELECT n, COUNT(*) FROM t GROUP BY n LIMIT 10", "22000"),
        ("SELECT id, n FROM t ORDER BY n LIMIT 10", "22000"),
        (
            "UPDATE t SET n = n + 1 WHERE id = 1 USING OPERATION_ID 'u1'",
            "0A000",
        ),
        ("CREATE TABLE p (n SMALLINT PRIMARY KEY)", "42601"),
        ("CREATE TABLE u (n SMALLINT UNIQUE)", "42601"),
        ("ALTER TABLE t ALTER COLUMN n TYPE INTEGER", "42804"),
        ("CREATE TABLE a (n SMALLINT[])", "0A000"),
        ("CREATE TABLE a (n INT2[3])", "0A000"),
    ] {
        assert_eq!(code(&core, sql), want, "{sql}");
    }
}

#[test]
fn smallint_type_names_stay_reserved_against_enum_names() {
    let (_p, _g, core) = open("si-enum");
    for name in ["smallint", "int2"] {
        assert!(
            exec(&core, &format!("CREATE TYPE {name} AS ENUM ('a')")).is_err(),
            "{name} must stay reserved"
        );
    }
}
