//! `ALTER TABLE <t> ALTER COLUMN <c> TYPE <型> [USING <式>]` の一般変換が、依存する索引・
//! UNIQUE／PK・CHECK・FK・DEFAULT・補完値を新しい型で再構築・再検証し、ビューが参照する列だけを
//! `0A000` で拒否することの結合テスト（TABLE-40 (3)(4)・TASK-278、Issue #1637）。
//!
//! `EngineCore::execute_sql_in_session` を production 経路とし、失敗系は拒否後に「列型・行の値・
//! 制約・DEFAULT が変わっていない」ことまで確認する。再検証は全テナント・全可視性の行が対象で、
//! エラー文言に値・テナント・行 id を載せない。

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

fn reopen(path: &std::path::Path) -> EngineCore {
    EngineCore::from_storage(
        Storage::open(path).expect("reopen storage"),
        Box::new(CpuScalarProvider),
    )
}

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, sql: &str) {
    exec_as(core, "alice", sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn fail(core: &EngineCore, sql: &str) -> SqlSurfaceError {
    exec_as(core, "alice", sql).expect_err(&format!("{sql} must fail"))
}

fn code(core: &EngineCore, sql: &str) -> String {
    fail(core, sql).wire_code().to_string()
}

/// 先頭列を表示用の文字列として昇順に返す。
fn firsts_as(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let sql = format!("{sql} LIMIT 100");
    let mut v: Vec<String> = core
        .execute_sql(&ctx(tenant), &sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
        .rows
        .into_iter()
        .map(|r| match r.cells.into_iter().next().expect("cell") {
            Cell::Null => "NULL".to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    v.sort_unstable();
    v
}

fn firsts(core: &EngineCore, sql: &str) -> Vec<String> {
    firsts_as(core, "alice", sql)
}

fn insert(core: &EngineCore, table: &str, id: u64, cols: &str, vals: &str) {
    ok(
        core,
        &format!(
            "INSERT INTO {table} (id, {cols}) VALUES ({id}, {vals}) USING OPERATION_ID '{table}{id}'"
        ),
    );
}

/// (3) スカラー索引: 変換後の等値・範囲検索が新しい型の値で正しい行を返す。
#[test]
fn scalar_index_is_rebuilt_for_the_new_type() {
    let (path, _g, core) = open("dep-index");
    ok(&core, "CREATE TABLE t (n TEXT)");
    ok(&core, "CREATE INDEX t_n ON t (n)");
    for (id, v) in [(1, "5"), (2, "20"), (3, "100")] {
        insert(&core, "t", id, "n", &format!("'{v}'"));
    }
    ok(
        &core,
        "ALTER TABLE t ALTER COLUMN n TYPE INTEGER USING n::integer",
    );
    assert_eq!(
        firsts(&core, "SELECT n FROM t WHERE n = 20"),
        vec!["SignedInteger(20)"]
    );
    assert_eq!(firsts(&core, "SELECT n FROM t WHERE n > 10").len(), 2);
    drop(core);
    let core = reopen(&path);
    assert_eq!(firsts(&core, "SELECT n FROM t WHERE n = 100").len(), 1);
}

/// (3) 照合付き索引の列を照合を持てない型へ変換すると `42804` で不変。
#[test]
fn collated_index_rejects_non_collatable_type() {
    let (_p, _g, core) = open("dep-collate");
    ok(&core, "CREATE TABLE t (n TEXT)");
    ok(&core, "CREATE INDEX t_n ON t (n COLLATE \"C\")");
    insert(&core, "t", 1, "n", "'5'");
    assert_eq!(
        code(
            &core,
            "ALTER TABLE t ALTER COLUMN n TYPE INTEGER USING n::integer"
        ),
        "42804"
    );
    assert_eq!(firsts(&core, "SELECT n FROM t"), vec!["Text(\"5\")"]);
}

/// (3) 索引化に未対応の型へは `0A000` で不変。
#[test]
fn index_rejects_unindexable_type() {
    let (_p, _g, core) = open("dep-unindexable");
    ok(&core, "CREATE TABLE t (n INTEGER)");
    ok(&core, "CREATE INDEX t_n ON t (n)");
    insert(&core, "t", 1, "n", "1");
    assert_eq!(
        code(
            &core,
            "ALTER TABLE t ALTER COLUMN n TYPE BOOLEAN USING n::boolean"
        ),
        "0A000"
    );
    assert_eq!(firsts(&core, "SELECT n FROM t"), vec!["SignedInteger(1)"]);
}

/// (3) UNIQUE: 衝突する変換は `23505` で不変、衝突しない変換は受理され以後も強制される。
#[test]
fn unique_is_revalidated_and_rebuilt() {
    let (path, _g, core) = open("dep-unique");
    ok(&core, "CREATE TABLE t (k INTEGER UNIQUE)");
    insert(&core, "t", 1, "k", "11");
    insert(&core, "t", 2, "k", "12");
    assert_eq!(
        code(
            &core,
            "ALTER TABLE t ALTER COLUMN k TYPE SMALLINT USING k / 10"
        ),
        "23505"
    );
    assert_eq!(firsts(&core, "SELECT k FROM t").len(), 2);
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, k) VALUES (3, 11) USING OPERATION_ID 'x1'"
        ),
        "23505"
    );
    ok(
        &core,
        "ALTER TABLE t ALTER COLUMN k TYPE TEXT USING k::text",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, k) VALUES (4, '11') USING OPERATION_ID 'x2'"
        ),
        "23505"
    );
    drop(core);
    let core = reopen(&path);
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, k) VALUES (5, '12') USING OPERATION_ID 'x3'"
        ),
        "23505"
    );
}

/// (3) PK: 衝突する変換は `23505` で不変、文言に値・テナントを含めない。衝突しない変換の後は
/// 再構築された一意索引が新しい値で重複を拒否する。
#[test]
fn primary_key_collision_is_detected_across_visibility_without_leaking() {
    let (_p, _g, core) = open("dep-pk");
    ok(&core, "CREATE TABLE t (k INTEGER PRIMARY KEY)");
    insert(&core, "t", 1, "k", "11");
    insert(&core, "t", 2, "k", "12");
    let e = fail(
        &core,
        "ALTER TABLE t ALTER COLUMN k TYPE SMALLINT USING k / 10",
    );
    assert_eq!(e.wire_code(), "23505");
    let msg = e.to_string();
    assert!(!msg.contains("11") && !msg.contains("alice"), "{msg}");
    ok(
        &core,
        "ALTER TABLE t ALTER COLUMN k TYPE BIGINT USING k * 2",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, k) VALUES (3, 22) USING OPERATION_ID 'x1'"
        ),
        "23505"
    );
}

/// (3) CHECK: 新しい値が違反する変換は `23514` で不変、違反しない変換は受理され以後も強制される。
#[test]
fn check_is_revalidated() {
    let (_p, _g, core) = open("dep-check");
    ok(&core, "CREATE TABLE t (a INTEGER CHECK (a > 0))");
    insert(&core, "t", 1, "a", "5");
    assert_eq!(
        code(
            &core,
            "ALTER TABLE t ALTER COLUMN a TYPE INTEGER USING a - 10"
        ),
        "23514"
    );
    assert_eq!(firsts(&core, "SELECT a FROM t"), vec!["SignedInteger(5)"]);
    ok(
        &core,
        "ALTER TABLE t ALTER COLUMN a TYPE BIGINT USING a * 3",
    );
    assert_eq!(firsts(&core, "SELECT a FROM t"), vec!["SignedInteger(15)"]);
    assert_eq!(
        code(
            &core,
            "INSERT INTO t (id, a) VALUES (2, 0) USING OPERATION_ID 'x1'"
        ),
        "23514"
    );
}

/// (3) CHECK を新しい型で束縛できない変換は、束縛エラーの SQLSTATE（`XX000` でない）で不変。
#[test]
fn check_that_cannot_be_rebound_is_rejected_with_binding_error() {
    let (_p, _g, core) = open("dep-check-bind");
    ok(&core, "CREATE TABLE t (a INTEGER CHECK (a > 0))");
    insert(&core, "t", 1, "a", "5");
    let c = code(
        &core,
        "ALTER TABLE t ALTER COLUMN a TYPE TEXT USING a::text",
    );
    // 述語 `a > 0` は TEXT の列と数値リテラルを比較できない（`42883`）。
    assert_eq!(c, "42883");
    assert_eq!(firsts(&core, "SELECT a FROM t"), vec!["SignedInteger(5)"]);
}

/// (3) FK: 子の列の型を親と比較できない型へ変換すると `42804`、親を変換して参照が切れると
/// `23503`。両立する変換は受理され以後の孤児 INSERT は `23503`。
#[test]
fn foreign_key_is_revalidated_both_sides() {
    let (_p, _g, core) = open("dep-fk");
    ok(&core, "CREATE TABLE p (k INTEGER PRIMARY KEY)");
    ok(&core, "CREATE TABLE c (r INTEGER REFERENCES p (k))");
    insert(&core, "p", 1, "k", "1");
    insert(&core, "c", 1, "r", "1");
    assert_eq!(
        code(
            &core,
            "ALTER TABLE c ALTER COLUMN r TYPE TEXT USING r::text"
        ),
        "42804"
    );
    assert_eq!(
        code(
            &core,
            "ALTER TABLE p ALTER COLUMN k TYPE INTEGER USING k + 1"
        ),
        "23503"
    );
    assert_eq!(firsts(&core, "SELECT k FROM p"), vec!["SignedInteger(1)"]);
    // 両側を同じ値のまま BIGINT へ。
    ok(
        &core,
        "ALTER TABLE p ALTER COLUMN k TYPE BIGINT USING k::bigint",
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO c (id, r) VALUES (2, 99) USING OPERATION_ID 'x1'"
        ),
        "23503"
    );
}

/// (3) DEFAULT・補完値: 旧型から新型への代入変換（PostgreSQL と同じく USING 式は使わない）で
/// 変換される。INSERT で列を省略すると変換後の値が入り、既存行は USING の結果になる。
/// 代入変換できない DEFAULT（`TEXT`→`INTEGER` など）は `42804` で不変。
#[test]
fn default_and_fill_value_are_converted_or_rejected() {
    let (_p, _g, core) = open("dep-default");
    ok(
        &core,
        "CREATE TABLE t (n BIGINT DEFAULT 300, m TEXT DEFAULT '5', big BIGINT DEFAULT 3000000000)",
    );
    insert(&core, "t", 1, "n, m, big", "1, '1', 1");
    ok(
        &core,
        "ALTER TABLE t ALTER COLUMN n TYPE INTEGER USING n::integer",
    );
    ok(
        &core,
        "INSERT INTO t (id) VALUES (2) USING OPERATION_ID 'd2'",
    );
    assert_eq!(
        firsts(&core, "SELECT n FROM t"),
        vec!["SignedInteger(1)", "SignedInteger(300)"]
    );
    for sql in [
        "ALTER TABLE t ALTER COLUMN m TYPE INTEGER USING m::integer",
        "ALTER TABLE t ALTER COLUMN big TYPE INTEGER USING big::integer",
    ] {
        assert_eq!(code(&core, sql), "42804", "{sql}");
    }
    ok(
        &core,
        "INSERT INTO t (id) VALUES (3) USING OPERATION_ID 'd3'",
    );
    assert_eq!(
        firsts(&core, "SELECT m FROM t WHERE id = 3"),
        vec!["Text(\"5\")"]
    );
}

/// (3) DEFAULT: 変換後の正準 text 表現が元と異なりうる型（REAL・DOUBLE PRECISION・NUMERIC の scale・
/// TIMESTAMPTZ）でも、代入変換が成功する限り `42804` にならず DEFAULT が新しい型へ変換される。
#[test]
fn default_converts_to_inexact_text_types() {
    let (_p, _g, core) = open("dep-default-inexact");
    ok(
        &core,
        "CREATE TABLE t (r DOUBLE PRECISION DEFAULT 0.1, d REAL DEFAULT 0.1, \
         n DOUBLE PRECISION DEFAULT 1.005, ts DATE DEFAULT '2024-01-02')",
    );
    for sql in [
        "ALTER TABLE t ALTER COLUMN r TYPE REAL",
        "ALTER TABLE t ALTER COLUMN d TYPE DOUBLE PRECISION",
        "ALTER TABLE t ALTER COLUMN n TYPE NUMERIC(10, 2)",
        "ALTER TABLE t ALTER COLUMN ts TYPE TIMESTAMPTZ",
    ] {
        ok(&core, sql);
    }
    ok(
        &core,
        "INSERT INTO t (id) VALUES (1) USING OPERATION_ID 'x1'",
    );
    let rows = core
        .execute_sql(
            &ctx("alice"),
            "SELECT r, d, n, ts FROM t WHERE id = 1 LIMIT 10",
        )
        .expect("select")
        .rows;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].cells.iter().all(|c| !matches!(c, Cell::Null)));
}

/// (3) 補完値: ADD COLUMN DEFAULT より前の既存行は変換後の値、以後の省略は変換後の DEFAULT。
#[test]
fn fill_value_is_materialized_into_existing_rows() {
    let (_p, _g, core) = open("dep-fill");
    ok(&core, "CREATE TABLE t (a INTEGER)");
    insert(&core, "t", 1, "a", "1");
    ok(&core, "ALTER TABLE t ADD COLUMN x BIGINT DEFAULT 7");
    ok(
        &core,
        "ALTER TABLE t ALTER COLUMN x TYPE INTEGER USING x::integer * 2",
    );
    ok(
        &core,
        "INSERT INTO t (id, a) VALUES (2, 2) USING OPERATION_ID 'f2'",
    );
    assert_eq!(
        firsts(&core, "SELECT x FROM t"),
        vec!["SignedInteger(14)", "SignedInteger(7)"]
    );
}

/// (4) ビュー: 参照する列・`SELECT *` ビュー・連鎖は `0A000` で不変、参照しない列は受理する。
#[test]
fn views_block_the_conversion_unless_unreferenced() {
    let (_p, _g, core) = open("dep-view");
    ok(&core, "CREATE TABLE t (a TEXT, b TEXT)");
    insert(&core, "t", 1, "a, b", "'1', '2'");
    ok(&core, "CREATE VIEW v AS SELECT a FROM t");
    ok(&core, "CREATE VIEW w AS SELECT a FROM v");
    assert_eq!(
        code(
            &core,
            "ALTER TABLE t ALTER COLUMN a TYPE INTEGER USING a::integer"
        ),
        "0A000"
    );
    ok(
        &core,
        "ALTER TABLE t ALTER COLUMN b TYPE INTEGER USING b::integer",
    );
    assert_eq!(firsts(&core, "SELECT b FROM t"), vec!["SignedInteger(2)"]);
    ok(&core, "CREATE TABLE s (a TEXT)");
    ok(&core, "CREATE VIEW sv AS SELECT * FROM s");
    assert_eq!(
        code(
            &core,
            "ALTER TABLE s ALTER COLUMN a TYPE INTEGER USING a::integer"
        ),
        "0A000"
    );
}

/// テナント境界: 他テナントの行が違反する場合も拒否され、文言に値・テナントを含めない。
#[test]
fn other_tenant_violations_are_rejected_without_leaking() {
    let (_p, _g, core) = open("dep-tenant");
    ok(&core, "CREATE TABLE t (a INTEGER CHECK (a > 0))");
    exec_as(
        &core,
        "tenant-b",
        "INSERT INTO t (id, a) VALUES (1, 5) USING OPERATION_ID 'b1'",
    )
    .expect("insert as other tenant");
    let e = fail(
        &core,
        "ALTER TABLE t ALTER COLUMN a TYPE INTEGER USING a - 10",
    );
    assert_eq!(e.wire_code(), "23514");
    assert!(!e.to_string().contains("tenant-b"), "{e}");
    assert_eq!(
        firsts_as(&core, "tenant-b", "SELECT a FROM t"),
        vec!["SignedInteger(5)"]
    );
}

/// 回帰: 拡大経路（INTEGER→BIGINT。PK・FK・ビュー依存あり）は従来どおり受理される。
#[test]
fn widening_path_is_unchanged() {
    let (_p, _g, core) = open("dep-widen");
    ok(&core, "CREATE TABLE t (k INTEGER PRIMARY KEY)");
    insert(&core, "t", 1, "k", "1");
    ok(&core, "CREATE VIEW v AS SELECT k FROM t");
    ok(&core, "ALTER TABLE t ALTER COLUMN k TYPE BIGINT");
}

/// PK 列（FK の参照先あり）の縮小変換は受理され、以後の参照整合性と一意性が新しい型で働く。
/// 自己参照 FK も同じ。
#[test]
fn narrowing_referenced_primary_key_keeps_integrity() {
    let (_p, _g, core) = open("dep-narrow");
    ok(&core, "CREATE TABLE p (k BIGINT PRIMARY KEY)");
    ok(&core, "CREATE TABLE c (r BIGINT REFERENCES p (k))");
    ok(
        &core,
        "CREATE TABLE s (k BIGINT PRIMARY KEY, up BIGINT REFERENCES s (k))",
    );
    insert(&core, "p", 1, "k", "7");
    insert(&core, "c", 1, "r", "7");
    insert(&core, "s", 1, "k, up", "3, 3");
    ok(&core, "ALTER TABLE p ALTER COLUMN k TYPE INTEGER");
    ok(&core, "ALTER TABLE s ALTER COLUMN k TYPE INTEGER");
    assert_eq!(
        code(
            &core,
            "INSERT INTO c (id, r) VALUES (2, 8) USING OPERATION_ID 'n1'"
        ),
        "23503"
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO p (id, k) VALUES (2, 7) USING OPERATION_ID 'n2'"
        ),
        "23505"
    );
    assert_eq!(
        code(
            &core,
            "INSERT INTO s (id, k, up) VALUES (2, 4, 9) USING OPERATION_ID 'n3'"
        ),
        "23503"
    );
}
