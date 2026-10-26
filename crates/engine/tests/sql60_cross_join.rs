//! `CROSS JOIN` の結合テスト（Issue #1831。ポインタ: SQL-60・SQL-28・RLS-10）。
//!
//! `tests/sql60_natural_join.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `unique_db_path`／`CleanupGuard`、`EngineCore` を production 経路として使う）。表は
//! `CREATE TABLE`（採番方式。疑似列 `id` を持たない）で作る。検証対象は (a) 直積の行数・列順・
//! 決定的順序、(b) 連鎖・他の結合種別との混在、(c) 出力行数上限の事前判定（`54000`・部分結果なし・
//! `LIMIT` 非依存）、(d) 未対応形の fail-closed な拒否、(e) テナント境界。
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
    let (core, path) = open_core("cross-join");
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
fn cross_join_returns_cartesian_product_in_star_order() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    let r = run(&core, "SELECT * FROM a CROSS JOIN b LIMIT 100");
    // 列順は左の全列 → 右の全列（統合列は無い）。
    assert_eq!(r.columns.len(), 4);
    assert_eq!(
        sorted_rows(&r),
        strs(&[
            &["1", "a1", "2", "b2"],
            &["1", "a1", "3", "b3"],
            &["2", "a2", "2", "b2"],
            &["2", "a2", "3", "b3"],
            &["3", "a3", "2", "b2"],
            &["3", "a3", "3", "b3"],
        ])
    );
}

#[test]
fn cross_join_order_is_deterministic_left_major() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    let sql = "SELECT a.k, b.k FROM a CROSS JOIN b LIMIT 100";
    let first = rows_of(&run(&core, sql));
    let second = rows_of(&run(&core, sql));
    assert_eq!(first, second);
    assert_eq!(first.len(), 6);
    let ordered = run(
        &core,
        "SELECT a.k, b.w FROM a CROSS JOIN b ORDER BY a.k DESC, b.w LIMIT 100",
    );
    assert_eq!(
        rows_of(&ordered),
        strs(&[
            &["3", "b2"],
            &["3", "b3"],
            &["2", "b2"],
            &["2", "b3"],
            &["1", "b2"],
            &["1", "b3"],
        ])
    );
}

#[test]
fn cross_join_with_empty_side_returns_no_rows() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    assert!(run(&core, "SELECT * FROM a CROSS JOIN em LIMIT 10")
        .rows
        .is_empty());
    assert!(run(&core, "SELECT * FROM em CROSS JOIN a LIMIT 10")
        .rows
        .is_empty());
}

#[test]
fn three_way_cross_join_chain() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    let r = run(&core, "SELECT * FROM a CROSS JOIN b CROSS JOIN c LIMIT 100");
    assert_eq!(r.columns.len(), 6);
    assert_eq!(r.rows.len(), 3 * 2 * 2);
    let proj = run(
        &core,
        "SELECT a.v, b.w, c.x FROM a CROSS JOIN b CROSS JOIN c WHERE a.v = 'a1' AND b.w = 'b2' LIMIT 100",
    );
    assert_eq!(
        sorted_rows(&proj),
        strs(&[&["a1", "b2", "c2"], &["a1", "b2", "c4"]])
    );
}

#[test]
fn cross_join_mixed_with_on_using_natural() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    // CROSS の後に ON 結合: (a x b) の各組に c を c.k = a.k で結合する。
    let r = run(
        &core,
        "SELECT a.v, b.w, c.x FROM a CROSS JOIN b JOIN c ON c.k = a.k LIMIT 100",
    );
    assert_eq!(
        sorted_rows(&r),
        strs(&[&["a2", "b2", "c2"], &["a2", "b3", "c2"]])
    );
    // ON 結合の後に CROSS。
    let r = run(
        &core,
        "SELECT a.v, b.w, c.x FROM a JOIN b ON a.k = b.k CROSS JOIN c LIMIT 100",
    );
    assert_eq!(
        sorted_rows(&r),
        strs(&[
            &["a2", "b2", "c2"],
            &["a2", "b2", "c4"],
            &["a3", "b3", "c2"],
            &["a3", "b3", "c4"],
        ])
    );
    // NATURAL の後に CROSS（共通列 k は 1 列に統合される）。
    let r = run(
        &core,
        "SELECT * FROM a NATURAL JOIN b CROSS JOIN c LIMIT 100",
    );
    assert_eq!(
        sorted_rows(&r),
        strs(&[
            &["2", "a2", "b2", "2", "c2"],
            &["2", "a2", "b2", "4", "c4"],
            &["3", "a3", "b3", "2", "c2"],
            &["3", "a3", "b3", "4", "c4"],
        ])
    );
    // CROSS の後に LEFT JOIN（NULL 補完）。
    let r = run(
        &core,
        "SELECT b.w, c.x FROM a CROSS JOIN b LEFT JOIN c ON c.k = b.k WHERE a.v = 'a1' LIMIT 100",
    );
    assert_eq!(sorted_rows(&r), strs(&[&["b2", "c2"], &["b3", "NULL"]]));
}

#[test]
fn cross_join_with_where_pushdown_and_cross_side_residual() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    let single = run(
        &core,
        "SELECT a.v, b.w FROM a CROSS JOIN b WHERE a.v = 'a1' LIMIT 100",
    );
    assert_eq!(sorted_rows(&single), strs(&[&["a1", "b2"], &["a1", "b3"]]));
    let lt = run(
        &core,
        "SELECT a.k, b.k FROM a CROSS JOIN b WHERE a.k < b.k LIMIT 100",
    );
    assert_eq!(
        sorted_rows(&lt),
        strs(&[&["1", "2"], &["1", "3"], &["2", "3"]])
    );
    let eq_cross = run(
        &core,
        "SELECT a.v, b.w FROM a CROSS JOIN b WHERE a.k = b.k LIMIT 100",
    );
    let eq_inner = run(
        &core,
        "SELECT a.v, b.w FROM a JOIN b ON a.k = b.k LIMIT 100",
    );
    assert_eq!(sorted_rows(&eq_cross), sorted_rows(&eq_inner));
}

#[test]
fn cross_join_limit_offset_and_order_by() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    let r = run(
        &core,
        "SELECT a.k, b.k FROM a CROSS JOIN b ORDER BY a.k, b.k LIMIT 2 OFFSET 1",
    );
    assert_eq!(rows_of(&r), strs(&[&["1", "3"], &["2", "2"]]));
}

#[test]
fn cross_join_aggregate() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    let r = run(&core, "SELECT COUNT(*) FROM a CROSS JOIN b");
    assert_eq!(rows_of(&r), strs(&[&["6"]]));
    let g = run(
        &core,
        "SELECT a.k, COUNT(*) FROM a CROSS JOIN b GROUP BY a.k ORDER BY a.k",
    );
    assert_eq!(rows_of(&g), strs(&[&["1", "2"], &["2", "2"], &["3", "2"]]));
}

#[test]
fn self_cross_join_with_aliases() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    let r = run(&core, "SELECT x.k, y.k FROM a x CROSS JOIN a y LIMIT 100");
    assert_eq!(r.rows.len(), 9);
    assert_rejected(&core, "SELECT * FROM a CROSS JOIN a LIMIT 10", "42601");
}

#[test]
fn cross_join_output_limit_is_rejected_before_partial_result() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    // a x b = 6 行。上限 5 なら `54000`（部分結果は返さない）、上限 6 なら成功する。
    let core = core.with_join_limits(JoinSizeLimits::new(8, 100, 5));
    assert_rejected(&core, "SELECT * FROM a CROSS JOIN b LIMIT 100", "54000");
    // `LIMIT` の値には依存しない。
    assert_rejected(&core, "SELECT * FROM a CROSS JOIN b LIMIT 1", "54000");
    let core = core.with_join_limits(JoinSizeLimits::new(8, 100, 6));
    assert_eq!(
        run(&core, "SELECT * FROM a CROSS JOIN b LIMIT 100")
            .rows
            .len(),
        6
    );
    // 3 段連鎖: 空表を挟めば最終 0 行で成功し、中間段（a x b = 12 > 10）が超過するなら拒否される。
    let core = core.with_join_limits(JoinSizeLimits::new(8, 100, 10));
    assert_rejected(
        &core,
        "SELECT * FROM a CROSS JOIN b CROSS JOIN c LIMIT 100",
        "54000",
    );
}

#[test]
fn cross_join_syntax_misuse_is_rejected() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    for sql in [
        "SELECT * FROM a CROSS JOIN b ON a.k = b.k LIMIT 10",
        "SELECT * FROM a CROSS JOIN b USING (k) LIMIT 10",
        "SELECT * FROM a NATURAL CROSS JOIN b LIMIT 10",
        "SELECT * FROM a CROSS OUTER JOIN b LIMIT 10",
        "SELECT * FROM a CROSS JOIN LIMIT 10",
    ] {
        assert_rejected(&core, sql, "42601");
    }
    exec(&core, "CREATE VIEW vw AS SELECT k FROM b");
    assert_rejected(&core, "SELECT * FROM a CROSS JOIN vw LIMIT 10", "42601");
}

#[test]
fn other_tenant_rows_never_surface_through_cross_join() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    // tenant-b の行を大量に足しても、tenant-a の結果・上限判定は変わらない。
    for chunk in 0..4 {
        let values: Vec<String> = (0..64)
            .map(|i| format!("({}, 'other-b{}')", 1000 + chunk * 64 + i, i))
            .collect();
        exec_as(
            &core,
            "tenant-b",
            &format!("INSERT INTO b (k, w) VALUES {}", values.join(", ")),
        );
    }
    let r = run(&core, "SELECT * FROM a CROSS JOIN b LIMIT 100");
    assert_eq!(r.rows.len(), 6);
    assert!(rows_of(&r)
        .iter()
        .flatten()
        .all(|cell| !cell.contains("other-b")));
    let core = core.with_join_limits(JoinSizeLimits::new(8, 1000, 6));
    assert_eq!(
        run(&core, "SELECT * FROM a CROSS JOIN b LIMIT 100")
            .rows
            .len(),
        6
    );
}
