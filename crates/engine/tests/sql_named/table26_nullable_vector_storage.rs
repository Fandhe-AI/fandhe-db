//! NULL 許容 VECTOR 列の保存（Issue #2042、親 #2040）の結合テスト。
//! ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-26 (1)〜(4)・TABLE-1・TABLE-5・TABLE-16。
//!
//! `EngineCore::execute_sql_in_session` の production 経路で、nullable な VECTOR 列への
//! 省略・明示 NULL・`UPDATE SET v = NULL`、NOT NULL 列の `23502`、再オープン後の属性の往復一致、
//! `ADD COLUMN`・`DROP NOT NULL`・CTAS、他テナントの NULL 行が結果へ影響しないことを固定する。
//! kNN からの NULL 行除外は #2043 の担当で、ここでは対象外。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        CleanupGuard(path),
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> SqlOutcome {
    run(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    run(core, tenant, sql)
        .map(|o| panic!("{sql} must fail, got {o:?}"))
        .unwrap_err()
        .wire_code()
        .to_string()
}

fn cells(core: &EngineCore, tenant: &str, sql: &str) -> Vec<Vec<Cell>> {
    match ok(core, tenant, sql) {
        SqlOutcome::Query(r) => r.rows.into_iter().map(|row| row.cells).collect(),
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

/// `SELECT id, v` の結果のうち v が NULL の行数。`v IS NULL` 述語は VECTOR 列に未対応
/// （別担当。#2041 の棚卸し参照）のため、射影した値の `Cell::Null` で判定する。
fn null_vector_rows(core: &EngineCore, tenant: &str, table: &str) -> usize {
    cells(
        core,
        tenant,
        &format!("SELECT id, v FROM {table} LIMIT 100"),
    )
    .iter()
    .filter(|r| matches!(r.get(1), Some(Cell::Null)))
    .count()
}

fn vector_nullable(path: &std::path::Path, table: &str) -> bool {
    let storage = Storage::open(path).expect("reopen");
    storage
        .get_table_schema(table)
        .expect("schema")
        .columns
        .into_iter()
        .find(|c| c.ty.is_vector())
        .expect("vector column")
        .nullable
}

/// 既定の VECTOR 列は nullable。省略・明示 NULL・`UPDATE SET v = NULL` がどれも成功し、
/// 読み取りでは NULL として振る舞う。
#[test]
fn nullable_vector_accepts_omitted_null_and_update_to_null() {
    let (core, _g) = new_core("t26-nullable");
    ok(
        &core,
        "sys",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(3), n TEXT)",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, n) VALUES (1, 'omitted') USING OPERATION_ID 'o1'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, v, n) VALUES (2, NULL, 'null') USING OPERATION_ID 'o2'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, v, n) VALUES (3, '[1,2,3]', 'set') USING OPERATION_ID 'o3'",
    );
    ok(
        &core,
        "alice",
        "UPDATE t SET v = NULL WHERE id = 3 USING OPERATION_ID 'o4'",
    );
    let rows = vec![(); null_vector_rows(&core, "alice", "t")];
    assert_eq!(rows.len(), 3, "all three rows must read back as NULL");
    ok(
        &core,
        "alice",
        "UPDATE t SET v = '[4,5,6]' WHERE id = 1 USING OPERATION_ID 'o5'",
    );
    let rows = vec![(); null_vector_rows(&core, "alice", "t")];
    assert_eq!(rows.len(), 2);
}

/// NOT NULL の VECTOR 列は省略・明示 NULL・UPDATE のいずれも `23502`。
#[test]
fn not_null_vector_rejects_with_23502() {
    let (core, _g) = new_core("t26-notnull");
    ok(
        &core,
        "sys",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(3) NOT NULL, n TEXT)",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "INSERT INTO t (id, n) VALUES (1, 'x') USING OPERATION_ID 'o1'"
        ),
        "23502"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "INSERT INTO t (id, v) VALUES (1, NULL) USING OPERATION_ID 'o2'"
        ),
        "23502"
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, v) VALUES (1, '[1,2,3]') USING OPERATION_ID 'o3'",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "UPDATE t SET v = NULL WHERE id = 1 USING OPERATION_ID 'o4'"
        ),
        "23502"
    );
}

/// カタログの nullable 属性は再オープン後も保たれる（往復一致）。
#[test]
fn nullable_attribute_survives_reopen() {
    let path = unique_db_path("t26-reopen");
    let _g = CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open");
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        ok(
            &core,
            "sys",
            "CREATE TABLE a (id BIGINT PRIMARY KEY, v VECTOR(3))",
        );
        ok(
            &core,
            "sys",
            "CREATE TABLE b (id BIGINT PRIMARY KEY, v VECTOR(3) NOT NULL)",
        );
    }
    assert!(vector_nullable(&path, "a"));
    assert!(!vector_nullable(&path, "b"));
}

/// VECTOR 列なしの表への `ADD COLUMN v VECTOR(n)` は nullable として受理し、既存行は NULL。
#[test]
fn add_column_vector_is_nullable_and_existing_rows_read_null() {
    let (core, _g) = new_core("t26-addcol");
    ok(
        &core,
        "sys",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, n TEXT)",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, n) VALUES (1, 'a') USING OPERATION_ID 'o1'",
    );
    assert_eq!(
        code(
            &core,
            "sys",
            "ALTER TABLE t ADD COLUMN w VECTOR(3) NOT NULL"
        ),
        "42601"
    );
    ok(&core, "sys", "ALTER TABLE t ADD COLUMN v VECTOR(3)");
    let rows = vec![(); null_vector_rows(&core, "alice", "t")];
    assert_eq!(rows.len(), 1);
    assert_eq!(
        code(&core, "sys", "ALTER TABLE t ADD COLUMN v2 VECTOR(3)"),
        "42601"
    );
}

/// `DROP NOT NULL` の後は NULL を書ける。`SET NOT NULL` は従来どおり拒否（対象外）。
#[test]
fn drop_not_null_allows_null_and_set_not_null_stays_rejected() {
    let (core, _g) = new_core("t26-dropnn");
    ok(
        &core,
        "sys",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(3) NOT NULL)",
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "INSERT INTO t (id, v) VALUES (1, NULL) USING OPERATION_ID 'o1'"
        ),
        "23502"
    );
    ok(&core, "sys", "ALTER TABLE t ALTER COLUMN v DROP NOT NULL");
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, v) VALUES (1, NULL) USING OPERATION_ID 'o2'",
    );
    let sqlstate = code(&core, "sys", "ALTER TABLE t ALTER COLUMN v SET NOT NULL");
    assert_eq!(sqlstate, "42601");
}

/// 他テナントの NULL 行は自テナントの結果・書き込みに現れない。
#[test]
fn other_tenant_null_rows_do_not_leak() {
    let (core, _g) = new_core("t26-rls");
    ok(
        &core,
        "sys",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(3))",
    );
    ok(
        &core,
        "bob",
        "INSERT INTO t (id, v) VALUES (1, NULL) USING OPERATION_ID 'o1'",
    );
    let rows = vec![(); null_vector_rows(&core, "alice", "t")];
    assert!(rows.is_empty());
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, v) VALUES (2, '[1,2,3]') USING OPERATION_ID 'o2'",
    );
    let rows = cells(
        &core,
        "alice",
        "SELECT id FROM t ORDER BY v <=> '[1,2,3]' LIMIT 10",
    );
    assert_eq!(rows.len(), 1);
}

/// CTAS は VECTOR 列も nullable で作り、NULL 行を写せる。
#[test]
fn ctas_copies_null_vector_rows() {
    let (core, _g) = new_core("t26-ctas");
    ok(
        &core,
        "sys",
        "CREATE TABLE src (id BIGINT PRIMARY KEY, v VECTOR(3))",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO src (id, v) VALUES (1, NULL) USING OPERATION_ID 'o1'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO src (id, v) VALUES (2, '[1,2,3]') USING OPERATION_ID 'o2'",
    );
    ok(&core, "alice", "CREATE TABLE dst AS SELECT id, v FROM src");
    let rows = vec![(); null_vector_rows(&core, "alice", "dst")];
    assert_eq!(rows.len(), 1);
}

/// #2043 までの過渡期: 自テナントの NULL 行を含む表の kNN は `0A000`（`XX000` にしない）。
/// 他テナントの NULL 行は自テナントの kNN に影響しない（`other_tenant_null_rows_do_not_leak`）。
#[test]
fn transitional_knn_over_own_null_row_code() {
    let (core, _g) = new_core("t26-knn");
    ok(
        &core,
        "sys",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(3))",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, v) VALUES (1, NULL) USING OPERATION_ID 'o1'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, v) VALUES (2, '[1,2,3]') USING OPERATION_ID 'o2'",
    );
    let r = run(
        &core,
        "alice",
        "SELECT id FROM t ORDER BY v <=> '[1,2,3]' LIMIT 10",
    );
    assert_eq!(r.unwrap_err().wire_code(), "0A000");
}

/// INSERT トリガー（BEFORE／AFTER ROW）のある表でも nullable VECTOR の省略・明示 NULL を
/// 受理する（トリガーの有無で NULL 保存契約が変わらないこと。PR #2963 レビュー指摘）。
#[test]
fn nullable_vector_accepted_on_insert_trigger_tables() {
    let (core, _g) = new_core("t26-trigger");
    ok(
        &core,
        "sys",
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v VECTOR(3), n TEXT)",
    );
    ok(
        &core,
        "sys",
        "CREATE FUNCTION trg() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$",
    );
    ok(
        &core,
        "sys",
        "CREATE TRIGGER tb BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg()",
    );
    ok(
        &core,
        "sys",
        "CREATE TRIGGER ta AFTER INSERT ON t FOR EACH ROW EXECUTE FUNCTION trg()",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, n) VALUES (1, 'omitted') USING OPERATION_ID 'o1'",
    );
    ok(
        &core,
        "alice",
        "INSERT INTO t (id, v, n) VALUES (2, NULL, 'null') USING OPERATION_ID 'o2'",
    );
    assert_eq!(null_vector_rows(&core, "alice", "t"), 2);
}
