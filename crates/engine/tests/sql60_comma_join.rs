//! FROM 句のカンマ区切り表参照の結合テスト（Issue #1833。ポインタ: SQL-60・SQL-28・RLS-10）。
//!
//! `tests/sql60_natural_join.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`、`EngineCore` を production 経路として使う）。表は
//! `CREATE TABLE` で作る。カンマ区切りは CROSS JOIN の left-deep 連鎖と同じ内部表現（`Cross` 段）
//! へ変換されるため、検証対象は (a) `CROSS JOIN` 表記との結果一致、(b) 別名・連鎖・混在、
//! (c) WHERE の扱いと出力行数上限、(d) 未対応形の fail-closed な拒否、(e) テナント境界。
//! 期待値は PostgreSQL 17 の意味論に基づく手計算値。

use engine::core::EngineCore;
use engine::join_limits::JoinSizeLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, QueryResult};
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

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("setup should succeed: sql={sql:?} err={e:?}"));
}

fn exec(core: &EngineCore, sql: &str) {
    exec_as(core, "tenant-a", sql);
}

fn try_run_as(
    core: &EngineCore,
    tenant: &str,
    sql: &str,
) -> Result<QueryResult, engine::sql::allowlist::SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected SqlOutcome::Query, got {other:?}"),
    }
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    try_run_as(core, "tenant-a", sql)
        .unwrap_or_else(|e| panic!("query should succeed: sql={sql:?} err={e:?}"))
}

fn assert_rejected(core: &EngineCore, sql: &str, wire_code: &str) {
    match try_run_as(core, "tenant-a", sql) {
        Ok(_) => panic!("must be rejected: sql={sql:?}"),
        Err(e) => assert_eq!(e.wire_code(), wire_code, "sql={sql:?} err={e:?}"),
    }
}

fn render(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        other => format!("{other:?}"),
    }
}

fn rows_of(result: &QueryResult) -> Vec<Vec<String>> {
    result
        .rows
        .iter()
        .map(|r| r.cells.iter().map(render).collect())
        .collect()
}

fn sorted_rows(result: &QueryResult) -> Vec<Vec<String>> {
    let mut rows = rows_of(result);
    rows.sort();
    rows
}

fn strs(rows: &[&[&str]]) -> Vec<Vec<String>> {
    rows.iter()
        .map(|r| r.iter().map(|s| s.to_string()).collect())
        .collect()
}

fn open_core(name: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        path,
    )
}

/// a: (1,a1)(2,a2)(3,a3)、b: (2,b2)(3,b3)、c: (2,c2)(4,c4)、em: 空表。
fn seeded() -> (EngineCore, std::path::PathBuf) {
    let (core, path) = open_core("comma-join");
    for sql in [
        "CREATE TABLE a (k BIGINT, v TEXT)",
        "CREATE TABLE b (k BIGINT, w TEXT)",
        "CREATE TABLE c (k BIGINT, x TEXT)",
        "CREATE TABLE em (k BIGINT, e TEXT)",
        "INSERT INTO a (k, v) VALUES (1, 'a1'), (2, 'a2'), (3, 'a3')",
        "INSERT INTO b (k, w) VALUES (2, 'b2'), (3, 'b3')",
        "INSERT INTO c (k, x) VALUES (2, 'c2'), (4, 'c4')",
    ] {
        exec(&core, sql);
    }
    (core, path)
}

#[test]
fn comma_join_matches_cross_join() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    for (comma, cross) in [
        (
            "SELECT * FROM a, b LIMIT 100",
            "SELECT * FROM a CROSS JOIN b LIMIT 100",
        ),
        (
            "SELECT * FROM a, b, c LIMIT 100",
            "SELECT * FROM a CROSS JOIN b CROSS JOIN c LIMIT 100",
        ),
        (
            "SELECT a.k, b.w FROM a, b ORDER BY a.k DESC, b.w LIMIT 100",
            "SELECT a.k, b.w FROM a CROSS JOIN b ORDER BY a.k DESC, b.w LIMIT 100",
        ),
        (
            "SELECT * FROM a, b CROSS JOIN c LIMIT 100",
            "SELECT * FROM a CROSS JOIN b CROSS JOIN c LIMIT 100",
        ),
        (
            "SELECT * FROM a JOIN b ON a.k = b.k, c LIMIT 100",
            "SELECT * FROM a JOIN b ON a.k = b.k CROSS JOIN c LIMIT 100",
        ),
        (
            "SELECT * FROM a NATURAL JOIN b, c LIMIT 100",
            "SELECT * FROM a NATURAL JOIN b CROSS JOIN c LIMIT 100",
        ),
    ] {
        let l = run(&core, comma);
        let r = run(&core, cross);
        assert_eq!(l.columns.len(), r.columns.len(), "sql={comma:?}");
        assert_eq!(rows_of(&l), rows_of(&r), "sql={comma:?}");
    }
    let r = run(&core, "SELECT * FROM a, b LIMIT 100");
    assert_eq!(r.columns.len(), 4);
    assert_eq!(r.rows.len(), 6);
}

#[test]
fn comma_join_with_aliases_and_self_join() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    let r = run(
        &core,
        "SELECT x.v, y.w FROM a x, b AS y WHERE x.v = 'a1' LIMIT 100",
    );
    assert_eq!(sorted_rows(&r), strs(&[&["a1", "b2"], &["a1", "b3"]]));
    let r = run(&core, "SELECT x.k, y.k FROM a x, a y LIMIT 100");
    assert_eq!(r.rows.len(), 9);
    assert_rejected(&core, "SELECT * FROM a, a LIMIT 10", "42601");
    assert_rejected(&core, "SELECT * FROM a x, b x LIMIT 10", "42601");
}

#[test]
fn comma_join_where_pushdown_and_cross_side_residual() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    let eq_comma = run(&core, "SELECT a.v, b.w FROM a, b WHERE a.k = b.k LIMIT 100");
    let eq_inner = run(
        &core,
        "SELECT a.v, b.w FROM a JOIN b ON a.k = b.k LIMIT 100",
    );
    assert_eq!(sorted_rows(&eq_comma), sorted_rows(&eq_inner));
    let lt = run(&core, "SELECT a.k, b.k FROM a, b WHERE a.k < b.k LIMIT 100");
    assert_eq!(
        sorted_rows(&lt),
        strs(&[&["1", "2"], &["1", "3"], &["2", "3"]])
    );
}

#[test]
fn comma_join_output_limit_is_judged_on_the_product() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    let core = core.with_join_limits(JoinSizeLimits::new(8, 100, 5));
    assert_rejected(&core, "SELECT * FROM a, b LIMIT 100", "54000");
    assert_rejected(&core, "SELECT * FROM a, b LIMIT 1", "54000");
    // 側を跨ぐ等価 WHERE は結合キーへ昇格する（Issue #1834）ため、直積（6 行）が上限 5 を
    // 超えても一致件数（2 行）で判定され成功する。昇格できない形（非等価・OR 内の等価）は
    // 直積の件数で判定され 54000 のまま。
    assert_eq!(
        run(&core, "SELECT a.v FROM a, b WHERE a.k = b.k LIMIT 100")
            .rows
            .len(),
        2
    );
    assert_rejected(
        &core,
        "SELECT a.v FROM a, b WHERE a.k < b.k LIMIT 100",
        "54000",
    );
    assert_rejected(
        &core,
        "SELECT a.v FROM a, b WHERE (a.k = b.k OR a.k = b.k) LIMIT 100",
        "54000",
    );
    let core = core.with_join_limits(JoinSizeLimits::new(8, 100, 6));
    assert_eq!(run(&core, "SELECT * FROM a, b LIMIT 100").rows.len(), 6);
}

#[test]
fn comma_join_syntax_and_table_ref_limits() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    for sql in [
        "SELECT * FROM a, LIMIT 1",
        "SELECT * FROM a, b, LIMIT 1",
        "SELECT * FROM a, b ON a.k = b.k LIMIT 10",
        "SELECT * FROM a, b USING (k) LIMIT 10",
        "SELECT * FROM a, (SELECT k FROM b) s LIMIT 10",
        "SELECT * FROM a, unnest(ARRAY[1]) LIMIT 10",
    ] {
        assert_rejected(&core, sql, "42601");
    }
    // カンマの後の CROSS 以外の明示 JOIN は fail-closed（0A000）。
    for sql in [
        "SELECT * FROM a, b JOIN c ON b.k = c.k LIMIT 10",
        "SELECT * FROM a, b LEFT JOIN c ON b.k = c.k LIMIT 10",
        "SELECT * FROM a, b RIGHT JOIN c ON b.k = c.k LIMIT 10",
        "SELECT * FROM a, b FULL JOIN c ON b.k = c.k LIMIT 10",
        "SELECT * FROM a, b NATURAL JOIN c LIMIT 10",
        "SELECT * FROM a, b JOIN c USING (k) LIMIT 10",
    ] {
        assert_rejected(&core, sql, "0A000");
    }
    exec(&core, "CREATE VIEW vw AS SELECT k FROM b");
    assert_rejected(&core, "SELECT * FROM a, vw LIMIT 10", "42601");
    assert_rejected(&core, "SELECT * FROM a, nosuch LIMIT 10", "42P01");
    // 表参照数の上限（8 表は受理、9 表は 54000）。
    for i in 0..9 {
        exec(&core, &format!("CREATE TABLE t{i} (k BIGINT)"));
    }
    let eight: Vec<String> = (0..8).map(|i| format!("t{i}")).collect();
    let nine: Vec<String> = (0..9).map(|i| format!("t{i}")).collect();
    assert!(run(
        &core,
        &format!("SELECT * FROM {} LIMIT 10", eight.join(", "))
    )
    .rows
    .is_empty());
    assert_rejected(
        &core,
        &format!("SELECT * FROM {} LIMIT 10", nine.join(", ")),
        "54000",
    );
}

#[test]
fn comma_join_context_parity_with_cross_join() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    for (comma, cross) in [
        (
            "EXPLAIN SELECT * FROM a, b LIMIT 10",
            "EXPLAIN SELECT * FROM a CROSS JOIN b LIMIT 10",
        ),
        (
            "SELECT k FROM a WHERE k IN (SELECT a.k FROM a, b) LIMIT 10",
            "SELECT k FROM a WHERE k IN (SELECT a.k FROM a CROSS JOIN b) LIMIT 10",
        ),
        (
            "CREATE VIEW v_comma AS SELECT a.k FROM a, b",
            "CREATE VIEW v_cross AS SELECT a.k FROM a CROSS JOIN b",
        ),
    ] {
        let l = try_run_as(&core, "tenant-a", comma).map(|r| r.rows.len());
        let r = try_run_as(&core, "tenant-a", cross).map(|r| r.rows.len());
        match (l, r) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "sql={comma:?}"),
            (Err(a), Err(b)) => assert_eq!(a.wire_code(), b.wire_code(), "sql={comma:?}"),
            (l, r) => panic!("parity broken: comma={l:?} cross={r:?} sql={comma:?}"),
        }
    }
}

#[test]
fn other_tenant_rows_never_surface_through_comma_join() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    exec_as(
        &core,
        "tenant-b",
        "INSERT INTO b (k, w) VALUES (1000, 'other-b0'), (1001, 'other-b1')",
    );
    let r = run(&core, "SELECT * FROM a, b LIMIT 100");
    assert_eq!(r.rows.len(), 6);
    assert!(rows_of(&r)
        .iter()
        .flatten()
        .all(|cell| !cell.contains("other-b")));
    let core = core.with_join_limits(JoinSizeLimits::new(8, 1000, 6));
    assert_eq!(run(&core, "SELECT * FROM a, b LIMIT 100").rows.len(), 6);
}
