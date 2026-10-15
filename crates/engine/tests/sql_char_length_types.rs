//! `VARCHAR(n)`／`CHAR(n)` の宣言（`ColumnType::Varchar(Some(n))`・`ColumnType::Bpchar(n)`）の
//! 結合テスト。ポインタ: TABLE-31・Issue #1563（兄弟: #1564 長さ超過 22001・#1565 wire typmod・
//! #1566 CHAR の空白詰めと比較・#1567 ALTER TYPE／FK／索引／UNIQUE）。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) 長さと種別がカタログ param として
//! 永続化され再起動後に復元されること、(2) n=0・上限超過・u32 あふれが fail-closed に拒否され副作用が
//! 残らないこと、(3) n 超過の書き込みが永続化されないこと、(4) CHAR(n) の未確定意味論が拒否側に
//! 倒れていること、を固定する。

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

fn ctx_of(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
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

fn exec_as(tenant: &str, core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx_of(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, sql: &str) {
    exec_as("alice", core, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn code(core: &EngineCore, sql: &str) -> String {
    exec_as("alice", core, sql)
        .expect_err(&format!("{sql} must fail"))
        .wire_code()
        .to_string()
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

fn table_exists(path: &std::path::Path, table: &str) -> bool {
    Storage::open(path)
        .expect("reopen")
        .get_table_schema(table)
        .is_ok()
}

fn select_count(core: &EngineCore, sql: &str) -> usize {
    core.execute_sql(&ctx_of("alice"), sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
        .rows
        .len()
}

/// 受け入れ条件 1: 再起動後も長さと種別（varying か fixed か）が復元される。
#[test]
fn length_and_kind_are_restored_after_reopen() {
    let (path, _g, core) = open("cl-persist");
    ok(
        &core,
        "CREATE TABLE t (a VARCHAR(10), b CHARACTER VARYING(3), c CHAR(5), d CHARACTER(1), \
         e VARCHAR, f TEXT)",
    );
    ok(&core, "ALTER TABLE t ADD COLUMN x VARCHAR(4)");
    ok(&core, "ALTER TABLE t ADD COLUMN y CHAR(7)");
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::Varchar(Some(10)));
    assert_eq!(column_type(&path, "t", "b"), ColumnType::Varchar(Some(3)));
    assert_eq!(column_type(&path, "t", "c"), ColumnType::Bpchar(5));
    assert_eq!(column_type(&path, "t", "d"), ColumnType::Bpchar(1));
    assert_eq!(column_type(&path, "t", "e"), ColumnType::Varchar(None));
    assert_eq!(column_type(&path, "t", "f"), ColumnType::Text);
    assert_eq!(column_type(&path, "t", "x"), ColumnType::Varchar(Some(4)));
    assert_eq!(column_type(&path, "t", "y"), ColumnType::Bpchar(7));
}

/// 受け入れ条件 2: n=0・上限超過・u32 あふれは `22023`、形式不正は `42601`、長さなしの
/// `CHAR` と配列形は `0A000`。拒否後にテーブルは作られない。
#[test]
fn invalid_lengths_are_rejected_fail_closed_without_side_effects() {
    let (path, _g, core) = open("cl-reject");
    for sql in [
        "CREATE TABLE t (a VARCHAR(0))",
        "CREATE TABLE t (a CHAR(0))",
        "CREATE TABLE t (a CHARACTER VARYING(0))",
        "CREATE TABLE t (a VARCHAR(10485761))",
        "CREATE TABLE t (a CHAR(4294967296))",
        "CREATE TABLE t (a VARCHAR(99999999999999999999))",
    ] {
        assert_eq!(code(&core, sql), "22023", "{sql}");
    }
    for sql in [
        "CREATE TABLE t (a VARCHAR())",
        "CREATE TABLE t (a VARCHAR(01))",
        "CREATE TABLE t (a CHAR(1.5))",
        "CREATE TABLE t (a CHAR(-1))",
        "CREATE TABLE t (a VARCHAR(3)",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
    for sql in [
        "CREATE TABLE t (a CHAR)",
        "CREATE TABLE t (a CHARACTER)",
        "CREATE TABLE t (a VARCHAR(3)[])",
        "CREATE TABLE t (a CHAR(3)[])",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
    ok(&core, "CREATE TABLE base (a INT)");
    assert_eq!(
        code(&core, "ALTER TABLE base ADD COLUMN b VARCHAR(0)"),
        "22023"
    );
    assert_eq!(
        code(&core, "ALTER TABLE base ADD COLUMN b CHAR(10485761)"),
        "22023"
    );
    drop(core);
    assert!(
        !table_exists(&path, "t"),
        "rejected DDL must leave no table"
    );
    assert_eq!(column_type(&path, "base", "a"), ColumnType::Integer);
    assert!(Storage::open(&path)
        .expect("reopen")
        .get_table_schema("base")
        .expect("schema")
        .columns
        .iter()
        .all(|c| c.name != "b"));
}

/// 上限ちょうど（10485760）は受理する。
#[test]
fn maximum_length_is_accepted() {
    let (path, _g, core) = open("cl-max");
    ok(
        &core,
        "CREATE TABLE t (a VARCHAR(10485760), b CHAR(10485760))",
    );
    drop(core);
    assert_eq!(
        column_type(&path, "t", "a"),
        ColumnType::Varchar(Some(10_485_760))
    );
    assert_eq!(column_type(&path, "t", "b"), ColumnType::Bpchar(10_485_760));
}

/// n 超過の書き込みは永続化されない（codec のバックストップ）。長さは文字数で数える。
/// 現状のコードは codec エラーの写像値を固定する。束縛段での `22001` 分類と末尾空白の
/// 切り詰め許容は Issue #1564 の担当で、その Issue がこの期待値を更新する。
#[test]
fn over_length_writes_are_rejected_and_not_persisted() {
    let (_p, _g, core) = open("cl-overflow");
    ok(&core, "CREATE TABLE t (v VARCHAR(3))");
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (1, 'abc') USING OPERATION_ID 'o1'",
    );
    // 3 文字（9 バイト）は受理される。
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (2, '日本語') USING OPERATION_ID 'o2'",
    );
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (3, '') USING OPERATION_ID 'o3'",
    );
    let c = code(
        &core,
        "INSERT INTO t (id, v) VALUES (4, 'abcd') USING OPERATION_ID 'o4'",
    );
    assert_eq!(c, "22000");
    assert_eq!(select_count(&core, "SELECT id FROM t LIMIT 10"), 3);
    // UPDATE の SET でも超過は拒否され、既存行は不変。
    assert!(exec_as(
        "alice",
        &core,
        "UPDATE t SET v = 'abcd' WHERE id = 1 USING OPERATION_ID 'o5'"
    )
    .is_err());
    assert_eq!(
        select_count(&core, "SELECT id FROM t WHERE v = 'abc' LIMIT 10"),
        1
    );
}

/// 一致行 0 件の UPDATE でも n 超過の SET 値は対象行探索前に拒否され、
/// `operation_id` を消費しない（同じ ID で有効な値を再送できる）。
#[test]
fn over_length_update_set_is_rejected_even_with_zero_matching_rows() {
    let (_p, _g, core) = open("cl-update-zero");
    ok(&core, "CREATE TABLE t (v VARCHAR(3))");
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (1, 'abc') USING OPERATION_ID 'o1'",
    );
    assert!(exec_as(
        "alice",
        &core,
        "UPDATE t SET v = 'abcd' WHERE id = 999 USING OPERATION_ID 'o2'"
    )
    .is_err());
    // 拒否で operation_id が消費されていなければ、同 ID で有効な更新が成功する。
    ok(
        &core,
        "UPDATE t SET v = 'xyz' WHERE id = 1 USING OPERATION_ID 'o2'",
    );
    assert_eq!(
        select_count(&core, "SELECT id FROM t WHERE v = 'xyz' LIMIT 10"),
        1
    );
}

/// `VARCHAR(n)` は TEXT と同じ述語・索引・PK 経路で扱える（値表現を共有するため）。
#[test]
fn bounded_varchar_behaves_like_text_for_predicates_and_keys() {
    let (_p, _g, core) = open("cl-varchar-text");
    ok(
        &core,
        "CREATE TABLE t (k VARCHAR(8) PRIMARY KEY, v VARCHAR(4))",
    );
    ok(
        &core,
        "INSERT INTO t (id, k, v) VALUES (1, 'apple', 'red') USING OPERATION_ID 'o1'",
    );
    ok(
        &core,
        "INSERT INTO t (id, k, v) VALUES (2, 'banana', 'yel') USING OPERATION_ID 'o2'",
    );
    assert_eq!(
        select_count(&core, "SELECT k FROM t WHERE k = 'apple' LIMIT 10"),
        1
    );
    assert_eq!(
        select_count(&core, "SELECT k FROM t WHERE k LIKE 'ban%' LIMIT 10"),
        1
    );
    assert_eq!(
        select_count(&core, "SELECT k FROM t ORDER BY k LIMIT 10"),
        2
    );
    ok(&core, "CREATE INDEX t_v ON t (v)");
}

/// `CHAR VARYING`／`CHAR VARYING(n)` は `CHARACTER VARYING` と同じ VARCHAR の綴り。
/// `VARYING` を伴わない bare `CHAR` は従来どおり `0A000`。
#[test]
fn char_varying_is_accepted_as_varchar_and_bare_char_stays_rejected() {
    let (path, _g, core) = open("cl-char-varying");
    ok(
        &core,
        "CREATE TABLE t (a CHAR VARYING(6), b char varying, c CHAR(2))",
    );
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::Varchar(Some(6)));
    assert_eq!(column_type(&path, "t", "b"), ColumnType::Varchar(None));
    assert_eq!(column_type(&path, "t", "c"), ColumnType::Bpchar(2));

    let (_p2, _g2, core) = open("cl-char-bare");
    assert_eq!(code(&core, "CREATE TABLE u (a CHAR)"), "0A000");
}

/// `CHAR(n)` は意味論（空白詰め・bpchar 比較）が未確定の間、非 NULL の書き込み・述語・索引・
/// 制約を拒否する。NULL の書き込みと `SELECT *` は成功する（Issue #1566／#1567 が解禁する）。
#[test]
fn char_columns_fail_closed_until_bpchar_semantics_land() {
    let (_p, _g, core) = open("cl-bpchar-closed");
    ok(&core, "CREATE TABLE t (c CHAR(3), n INT)");
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, c, n) VALUES (1, 'ab', 1) USING OPERATION_ID 'o1'"
        ),
        "0A000"
    );
    ok(
        &core,
        "INSERT INTO t (id, c, n) VALUES (2, NULL, 2) USING OPERATION_ID 'o2'",
    );
    ok(
        &core,
        "INSERT INTO t (id, n) VALUES (3, 3) USING OPERATION_ID 'o3'",
    );
    assert_eq!(select_count(&core, "SELECT * FROM t LIMIT 10"), 2);
    assert_eq!(select_count(&core, "SELECT id, c FROM t LIMIT 10"), 2);
    for sql in [
        "SELECT id FROM t WHERE c = 'ab' LIMIT 10",
        "SELECT id FROM t ORDER BY c LIMIT 10",
        "SELECT c, count(*) FROM t GROUP BY c LIMIT 10",
        "SELECT max(c) FROM t",
    ] {
        assert!(
            exec_as("alice", &core, sql).is_err(),
            "{sql} must be rejected while CHAR(n) semantics are unsettled"
        );
    }
    assert_eq!(
        code(
            &core,
            "UPDATE t SET c = 'ab' WHERE id = 2 USING OPERATION_ID 'o4'"
        ),
        "0A000"
    );
    for sql in [
        "CREATE TABLE p (c CHAR(3) PRIMARY KEY)",
        "CREATE TABLE p (c CHAR(3) UNIQUE)",
        "CREATE INDEX t_c ON t (c)",
    ] {
        assert!(
            exec_as("alice", &core, sql).is_err(),
            "{sql} must be rejected"
        );
    }
    assert!(exec_as("alice", &core, "CREATE TABLE d (c CHAR(3) DEFAULT 'ab')").is_err());
}

/// 長さ付き型が絡む `ALTER COLUMN TYPE` は既存行が n を超える状態を作りうるため拒否する
/// （`42804`。解禁は Issue #1567）。長さなしの `TEXT`↔`VARCHAR` は従来どおり成功する。
#[test]
fn alter_column_type_with_bounded_types_is_incompatible() {
    let (path, _g, core) = open("cl-alter");
    ok(
        &core,
        "CREATE TABLE t (a TEXT, b VARCHAR(3), c CHAR(3), d VARCHAR)",
    );
    for sql in [
        "ALTER TABLE t ALTER COLUMN a TYPE VARCHAR(3)",
        "ALTER TABLE t ALTER COLUMN b TYPE TEXT",
        "ALTER TABLE t ALTER COLUMN b TYPE VARCHAR(10)",
        "ALTER TABLE t ALTER COLUMN c TYPE TEXT",
        "ALTER TABLE t ALTER COLUMN a TYPE CHAR(3)",
    ] {
        assert_eq!(code(&core, sql), "42804", "{sql}");
    }
    ok(&core, "ALTER TABLE t ALTER COLUMN a TYPE VARCHAR");
    ok(&core, "ALTER TABLE t ALTER COLUMN d TYPE TEXT");
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::Varchar(None));
    assert_eq!(column_type(&path, "t", "b"), ColumnType::Varchar(Some(3)));
    assert_eq!(column_type(&path, "t", "c"), ColumnType::Bpchar(3));
    assert_eq!(column_type(&path, "t", "d"), ColumnType::Text);
}

/// `VARCHAR(n)` の DEFAULT は文字数が n 以下であること。
#[test]
fn varchar_default_longer_than_length_is_rejected() {
    let (path, _g, core) = open("cl-default");
    ok(&core, "CREATE TABLE ok_t (v VARCHAR(3) DEFAULT 'abc')");
    assert!(exec_as(
        "alice",
        &core,
        "CREATE TABLE bad_t (v VARCHAR(3) DEFAULT 'abcd')"
    )
    .is_err());
    drop(core);
    assert!(table_exists(&path, "ok_t"));
    assert!(!table_exists(&path, "bad_t"));
}

/// テナント境界: 別テナントのセッションでも同じ DDL 結果になり、エラーに他テナントの
/// 情報（テナント名・表の存在）が含まれない。
#[test]
fn errors_do_not_leak_tenant_information() {
    let (_p, _g, core) = open("cl-tenant");
    ok(&core, "CREATE TABLE secret_t (v VARCHAR(3))");
    let e = exec_as("bob", &core, "CREATE TABLE u (a VARCHAR(0))").expect_err("must fail");
    assert_eq!(e.wire_code(), "22023");
    let msg = e.to_string();
    assert!(
        !msg.contains("alice") && !msg.contains("bob") && !msg.contains("secret_t"),
        "{msg}"
    );
}
