//! `HALFVEC(N)` 列（`ColumnType::HalfVec`。TABLE-46 (1)(4)(5)・Issue #2014）の結合テスト。
//!
//! production 経路（`EngineCore::execute_sql_in_session`）で、(1) 型がカタログへ永続化され再オープン後も
//! 値ごと復元されること、(2) INSERT／UPDATE SET／UPSERT の往復が「f16 へ丸めた値」で一致し VECTOR は
//! 丸まらないこと、(3) 次元不一致・非有限は `22000`、f16 範囲外は `22003` で副作用なく拒否されること、
//! (4) 列本数の制約（VECTOR と合わせて 0〜1 本）、(5) 未解禁の機能（距離計算・索引・制約・DDL 変更・
//! COPY・CAST・並べ替え・集計）が fail-closed に拒否されること、(6) テナント境界が不変であることを固定する。
//! 距離計算と HNSW は兄弟 Issue（#2015・#2016）の担当。

use engine::catalog::ColumnType;
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

fn ctx_for(tenant: &str) -> PolicyContext {
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

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx_for(tenant), &mut s, sql)
}

fn exec(core: &EngineCore, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    exec_as(core, "alice", sql)
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

/// `SELECT id, <col> ...` の結果を `(id, 成分または None)` で返す（HALFVEC・VECTOR とも `Cell::Vector`）。
fn rows_as(core: &EngineCore, tenant: &str, sql: &str) -> Vec<(i64, Option<Vec<f32>>)> {
    let SqlOutcome::Query(q) =
        exec_as(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
    else {
        panic!("{sql} must return a query result");
    };
    q.rows
        .iter()
        .map(|r| {
            let id = match r.cells.first() {
                Some(Cell::SignedInteger(n)) => *n,
                Some(Cell::Integer(n)) => i64::try_from(*n).expect("id"),
                other => panic!("unexpected id cell {other:?}"),
            };
            let v = match r.cells.get(1) {
                Some(Cell::Vector(v)) => Some(v.clone()),
                Some(Cell::Null) => None,
                other => panic!("unexpected cell {other:?}"),
            };
            (id, v)
        })
        .collect()
}

fn rows(core: &EngineCore, sql: &str) -> Vec<(i64, Option<Vec<f32>>)> {
    rows_as(core, "alice", sql)
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

/// `0.1` を f16 へ丸めた値（f32 の最短表記 `0.099975586`）。
const HALF_0_1: f32 = 0.099_975_586;

#[test]
fn halfvec_type_is_persisted_in_the_catalog() {
    let (path, _g, core) = open("hv-persist");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, a halfvec(3), b TEXT NOT NULL)",
    );
    ok(&core, "INSERT INTO t (id, a, b) VALUES (1, '[1,2,3]', 'x')");
    drop(core);
    assert_eq!(column_type(&path, "t", "a"), ColumnType::HalfVec(3));
    assert_eq!(column_type(&path, "t", "b"), ColumnType::Text);
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    assert_eq!(
        rows(&core, "SELECT id, a FROM t"),
        vec![(1, Some(vec![1.0, 2.0, 3.0]))]
    );
}

#[test]
fn insert_select_roundtrip_rounds_to_f16_while_vector_keeps_f32() {
    let (_p, _g, core) = open("hv-roundtrip");
    ok(
        &core,
        "CREATE TABLE h (id BIGINT PRIMARY KEY, v HALFVEC(2))",
    );
    ok(&core, "CREATE TABLE f (id BIGINT PRIMARY KEY, v VECTOR(2))");
    ok(&core, "INSERT INTO h (id, v) VALUES (1, '[0.1,1]')");
    ok(&core, "INSERT INTO f (id, v) VALUES (1, '[0.1,1]')");
    assert_eq!(
        rows(&core, "SELECT id, v FROM h"),
        vec![(1, Some(vec![HALF_0_1, 1.0]))]
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM f"),
        vec![(1, Some(vec![0.1, 1.0]))]
    );
}

#[test]
fn update_and_upsert_roundtrip() {
    let (_p, _g, core) = open("hv-update");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v HALFVEC(2), n TEXT)",
    );
    ok(&core, "INSERT INTO t (id, v, n) VALUES (1, '[1,2]', 'a')");
    ok(&core, "UPDATE t SET v = '[0.1,3]' WHERE id = 1");
    assert_eq!(
        rows(&core, "SELECT id, v FROM t"),
        vec![(1, Some(vec![HALF_0_1, 3.0]))]
    );
    // 他列だけの UPDATE は HALFVEC を壊さない。
    ok(&core, "UPDATE t SET n = 'b' WHERE id = 1");
    assert_eq!(
        rows(&core, "SELECT id, v FROM t"),
        vec![(1, Some(vec![HALF_0_1, 3.0]))]
    );
    ok(
        &core,
        "INSERT INTO t (id, v, n) VALUES (1, '[9,9]', 'c') ON CONFLICT (id) DO UPDATE SET v = '[4,5]'",
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM t"),
        vec![(1, Some(vec![4.0, 5.0]))]
    );
    // 次元不一致の SET は 22000 で、値は変わらない。
    assert_eq!(
        code(&core, "UPDATE t SET v = '[1,2,3]' WHERE id = 1"),
        "22000"
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM t"),
        vec![(1, Some(vec![4.0, 5.0]))]
    );
}

#[test]
fn returning_and_null_handling() {
    let (_p, _g, core) = open("hv-null");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v HALFVEC(2), w TEXT NOT NULL)",
    );
    let out = exec(
        &core,
        "INSERT INTO t (id, v, w) VALUES (1, '[0.1,2]', 'x') RETURNING v",
    )
    .expect("returning");
    let SqlOutcome::Returning(o) = out else {
        panic!("expected SqlOutcome::Returning");
    };
    assert_eq!(
        o.result.rows.first().and_then(|r| r.cells.first()),
        Some(&Cell::Vector(vec![HALF_0_1, 2.0]))
    );
    ok(&core, "INSERT INTO t (id, w) VALUES (2, 'x')");
    ok(&core, "INSERT INTO t (id, v, w) VALUES (3, NULL, 'x')");
    assert_eq!(
        rows(&core, "SELECT id, v FROM t WHERE id = 2"),
        vec![(2, None)]
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM t WHERE id = 3"),
        vec![(3, None)]
    );
    assert_eq!(code(&core, "INSERT INTO t (id) VALUES (4)"), "23502");
    assert_eq!(
        code(&core, "INSERT INTO t (id, w) VALUES (5, NULL)"),
        "23502"
    );
}

#[test]
fn invalid_values_are_rejected_without_side_effects() {
    let (_p, _g, core) = open("hv-invalid");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v HALFVEC(2))",
    );
    for lit in [
        "'[1]'",
        "'[1,2,3]'",
        "'[nan,1]'",
        "'[inf,1]'",
        "'abc'",
        "'[1,x]'",
    ] {
        assert_eq!(
            code(&core, &format!("INSERT INTO t (id, v) VALUES (1, {lit})")),
            "22000",
            "{lit}"
        );
    }
    assert_eq!(
        code(&core, "INSERT INTO t (id, v) VALUES (1, '[70000,1]')"),
        "22003"
    );
    assert_eq!(
        code(&core, "INSERT INTO t (id, v) VALUES (1, '[-70000,1]')"),
        "22003"
    );
    assert_eq!(
        code(&core, "INSERT INTO t (id, v) VALUES (1, '[65520,1]')"),
        "22003"
    );
    assert_eq!(code(&core, "INSERT INTO t (id, v) VALUES (1, 5)"), "22000");
    assert!(rows(&core, "SELECT id, v FROM t").is_empty());
}

#[test]
fn f16_range_boundary_matches_pgvector() {
    let (_p, _g, core) = open("hv-boundary");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v HALFVEC(1))",
    );
    ok(&core, "INSERT INTO t (id, v) VALUES (1, '[65504]')");
    // 65504 超〜65520 未満は 65504 へ丸まって受理される。
    ok(&core, "INSERT INTO t (id, v) VALUES (2, '[65519]')");
    assert_eq!(
        code(&core, "INSERT INTO t (id, v) VALUES (3, '[65520]')"),
        "22003"
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM t ORDER BY id"),
        vec![(1, Some(vec![65504.0])), (2, Some(vec![65504.0]))]
    );
}

#[test]
fn column_count_constraint_is_shared_with_vector() {
    let (_p, _g, core) = open("hv-count");
    for sql in [
        "CREATE TABLE a (id BIGINT PRIMARY KEY, x VECTOR(2), y HALFVEC(2))",
        "CREATE TABLE b (id BIGINT PRIMARY KEY, x HALFVEC(2), y HALFVEC(2))",
        "CREATE TABLE c (id BIGINT PRIMARY KEY, x HALFVEC(0))",
        "CREATE TABLE d (id BIGINT PRIMARY KEY, x HALFVEC)",
        "CREATE TABLE e (id BIGINT PRIMARY KEY, x HALFVEC(2)[])",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
}

#[test]
fn unsupported_features_fail_closed() {
    let (_p, _g, core) = open("hv-closed");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v HALFVEC(2), n INTEGER)",
    );
    ok(&core, "INSERT INTO t (id, v, n) VALUES (1, '[1,2]', 1)");
    // 列変更。
    ok(&core, "CREATE TABLE u (id BIGINT PRIMARY KEY)");
    assert_eq!(
        code(&core, "ALTER TABLE u ADD COLUMN w HALFVEC(2)"),
        "0A000"
    );
    ok(&core, "ALTER TABLE u ADD COLUMN n INTEGER");
    assert_eq!(
        code(&core, "ALTER TABLE u ALTER COLUMN n TYPE HALFVEC(2)"),
        "42804"
    );
    assert_eq!(code(&core, "ALTER TABLE t DROP COLUMN v"), "42601");
    assert_eq!(
        code(&core, "ALTER TABLE t ALTER COLUMN v TYPE TEXT"),
        "42804"
    );
    // 検索・式・並べ替え・集計・索引・制約・実体化は拒否する（距離計算と HNSW は兄弟 Issue の担当）。
    let cases: &[(&str, &str)] = &[
        ("SELECT id FROM t ORDER BY v <-> '[1,2]' LIMIT 1", "0A000"),
        ("SELECT id FROM t ORDER BY v", "22000"),
        ("SELECT id FROM t WHERE v = '[1,2]'", "22000"),
        ("SELECT DISTINCT v FROM t", "22000"),
        ("SELECT v, count(*) FROM t GROUP BY v", "22000"),
        ("SELECT max(v) FROM t", "42883"),
        ("SELECT count(v) FROM t", "42883"),
        ("SELECT v::text FROM t", "22000"),
        ("SELECT '[1,2]'::halfvec(2)", "0A000"),
        ("SELECT l2_distance(v, '[1,2]') FROM t", "22000"),
        ("SELECT v FROM t UNION SELECT v FROM t", "0A000"),
        ("SELECT v || 'x' FROM t", "22000"),
        ("CREATE INDEX i ON t (v)", "0A000"),
        ("CREATE INDEX i ON t USING hnsw (v)", "0A000"),
        ("CREATE INDEX i ON t USING gin (v)", "0A000"),
        ("CREATE TABLE c AS SELECT * FROM t", "0A000"),
        ("CREATE TABLE p (v HALFVEC(2) PRIMARY KEY)", "42601"),
        (
            "CREATE TABLE q (id BIGINT PRIMARY KEY, v HALFVEC(2) UNIQUE)",
            "42601",
        ),
        (
            "CREATE TABLE r (id BIGINT PRIMARY KEY, v HALFVEC(2) DEFAULT '[1,2]')",
            "0A000",
        ),
        (
            "CREATE TABLE r2 (id BIGINT PRIMARY KEY, v HALFVEC(2) CHECK (v IS NOT NULL))",
            "42601",
        ),
        ("UPDATE t SET v = v WHERE id = 1", "0A000"),
        ("INSERT INTO t (id, v) SELECT id + 10, v FROM t", "0A000"),
    ];
    for (sql, expected) in cases {
        assert_eq!(code(&core, sql), *expected, "{sql}");
    }
    // 副作用なし。
    assert_eq!(
        rows(&core, "SELECT id, v FROM t"),
        vec![(1, Some(vec![1.0, 2.0]))]
    );
}

#[test]
fn passthrough_paths_return_rounded_values() {
    let (_p, _g, core) = open("hv-passthrough");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v HALFVEC(2))",
    );
    ok(&core, "INSERT INTO t (id, v) VALUES (1, '[0.1,2]')");
    ok(&core, "INSERT INTO t (id) VALUES (2)");
    // IS NULL は行の有無を正しく判定する。
    assert_eq!(
        rows(&core, "SELECT id, v FROM t WHERE v IS NULL"),
        vec![(2, None)]
    );
    // ビュー経由の素通し。
    ok(&core, "CREATE VIEW vw AS SELECT id, v FROM t");
    assert_eq!(
        rows(&core, "SELECT id, v FROM vw WHERE id = 1"),
        vec![(1, Some(vec![HALF_0_1, 2.0]))]
    );
    // EXCLUDED の参照も f16 のまま運ぶ。
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (2, '[0.1,3]') ON CONFLICT (id) DO UPDATE SET v = EXCLUDED.v",
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM t WHERE id = 2"),
        vec![(2, Some(vec![HALF_0_1, 3.0]))]
    );
}

#[test]
fn tenant_boundary_is_unchanged() {
    let (_p, _g, core) = open("hv-tenant");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v HALFVEC(2))",
    );
    ok(&core, "INSERT INTO t (id, v) VALUES (1, '[1,2]')");
    exec_as(&core, "bob", "INSERT INTO t (id, v) VALUES (2, '[3,4]')").expect("bob insert");
    assert_eq!(
        rows_as(&core, "alice", "SELECT id, v FROM t"),
        vec![(1, Some(vec![1.0, 2.0]))]
    );
    assert_eq!(
        rows_as(&core, "bob", "SELECT id, v FROM t"),
        vec![(2, Some(vec![3.0, 4.0]))]
    );
}

/// nullable HALFVEC 列への `SET v = NULL`（UPDATE／UPSERT）は成功し、NOT NULL 列では 23502 で拒否される。
#[test]
fn set_null_on_nullable_halfvec_column() {
    let (_p, _g, core) = open("hv-setnull");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v HALFVEC(2))",
    );
    ok(&core, "INSERT INTO t (id, v) VALUES (1, '[1,2]')");
    ok(&core, "UPDATE t SET v = NULL WHERE id = 1");
    assert_eq!(
        rows(&core, "SELECT id, v FROM t WHERE id = 1"),
        vec![(1, None)]
    );
    ok(&core, "UPDATE t SET v = '[5,6]' WHERE id = 1");
    ok(
        &core,
        "INSERT INTO t (id, v) VALUES (1, '[9,9]') ON CONFLICT (id) DO UPDATE SET v = NULL",
    );
    assert_eq!(
        rows(&core, "SELECT id, v FROM t WHERE id = 1"),
        vec![(1, None)]
    );
    ok(
        &core,
        "CREATE TABLE n (id BIGINT PRIMARY KEY, r HALFVEC(2) NOT NULL)",
    );
    ok(&core, "INSERT INTO n (id, r) VALUES (1, '[3,4]')");
    assert_eq!(code(&core, "UPDATE n SET r = NULL WHERE id = 1"), "23502");
}

/// HALFVEC 列は行の内容によらず、マテリアライズドビューの出力に使えない（0A000）。
#[test]
fn halfvec_is_rejected_in_materialized_view_regardless_of_rows() {
    let (_p, _g, core) = open("hv-matview");
    ok(
        &core,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, h HALFVEC(2))",
    );
    let q = "CREATE MATERIALIZED VIEW mv AS SELECT h FROM t";
    assert_eq!(code(&core, q), "0A000");
    ok(&core, "INSERT INTO t (id, h) VALUES (1, NULL)");
    assert_eq!(code(&core, q), "0A000");
    ok(&core, "UPDATE t SET h = '[1,2]' WHERE id = 1");
    assert_eq!(code(&core, q), "0A000");
}
