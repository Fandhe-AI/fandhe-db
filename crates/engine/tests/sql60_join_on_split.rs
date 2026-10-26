//! `JOIN ... ON` の結合キー／残余述語への分解の結合テスト（Issue #1829。ポインタ: SQL-60）。
//!
//! 分類器そのものの単体テストは `sql::join::on_split`。ここでは production 経路
//! （`EngineCore`）で、(a) 等価キーのみの ON が従来どおり動くこと、(b) キー無しは `42601`、
//! (c) 残余述語（左のみ・右のみ・混在）付きは評価が #1830 の責務のため全結合種別で `0A000`
//! の fail-closed、(d) 未知列は分類より前に `42703`、(e) 拒否がテナントに依存しないこと
//! を確認する。

use engine::core::EngineCore;
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

/// DDL を許可したセッションで文を実行する（`CREATE TABLE`・`INSERT` の準備用）。
fn exec(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
        .unwrap_or_else(|e| panic!("setup should succeed: sql={sql:?} err={e:?}"));
}

/// l(k,tag,n)・r(k,tag,n): 結合キー `k`・単側述語用の `tag`・混在述語用の `n`。
fn seeded() -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path("on-split");
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for sql in [
        "CREATE TABLE l (k BIGINT, tag TEXT, n BIGINT)",
        "CREATE TABLE r (k BIGINT, tag TEXT, n BIGINT)",
        "INSERT INTO l (k, tag, n) VALUES (1, 'x', 1), (2, 'y', 5)",
        "INSERT INTO r (k, tag, n) VALUES (1, 'x', 2), (2, 'x', 1), (3, 'z', 9)",
    ] {
        exec(&core, sql);
    }
    (core, path)
}

fn try_run(
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

fn render(cell: &Cell) -> String {
    match cell {
        Cell::Null => "NULL".to_string(),
        Cell::Text(s) => s.clone(),
        Cell::Integer(v) => v.to_string(),
        Cell::SignedInteger(v) => v.to_string(),
        other => format!("{other:?}"),
    }
}

fn err_of(core: &EngineCore, tenant: &str, sql: &str) -> (String, String) {
    match try_run(core, tenant, sql) {
        Ok(_) => panic!("must be rejected: sql={sql:?}"),
        Err(e) => (e.wire_code().to_string(), format!("{e:?}")),
    }
}

fn assert_rejected(core: &EngineCore, sql: &str, wire_code: &str) {
    let (code, detail) = err_of(core, "tenant-a", sql);
    assert_eq!(code, wire_code, "sql={sql:?} err={detail}");
}

#[test]
fn equi_key_only_on_is_unchanged() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    let res = try_run(
        &core,
        "tenant-a",
        "SELECT l.k, r.n FROM l JOIN r ON l.k = r.k LIMIT 10",
    )
    .expect("equi join");
    let mut rows: Vec<Vec<String>> = res
        .rows
        .iter()
        .map(|row| row.cells.iter().map(render).collect())
        .collect();
    rows.sort();
    assert_eq!(rows, vec![vec!["1", "2"], vec!["2", "1"]]);
    // 複数キー。
    let res = try_run(
        &core,
        "tenant-a",
        "SELECT l.k FROM l JOIN r ON l.k = r.k AND l.tag = r.tag LIMIT 10",
    )
    .expect("multi-key equi join");
    assert_eq!(res.rows.len(), 1);
}

#[test]
fn residual_predicates_are_rejected_for_every_join_kind() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    for kind in ["JOIN", "LEFT JOIN", "RIGHT JOIN", "FULL JOIN"] {
        for residual in ["r.tag = 'x'", "l.tag = 'x'", "l.n > r.n"] {
            let sql = format!("SELECT l.k FROM l {kind} r ON l.k = r.k AND {residual} LIMIT 10");
            assert_rejected(&core, &sql, "0A000");
        }
    }
}

#[test]
fn on_without_equi_key_is_rejected() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    assert_rejected(
        &core,
        "SELECT l.k FROM l JOIN r ON l.n > r.n LIMIT 10",
        "42601",
    );
    assert_rejected(
        &core,
        "SELECT l.k FROM l JOIN r ON l.k = l.n LIMIT 10",
        "42601",
    );
    assert_rejected(
        &core,
        "SELECT l.k FROM l JOIN r ON r.tag = 'x' LIMIT 10",
        "42601",
    );
}

#[test]
fn unknown_column_in_on_is_resolved_before_classification() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    assert_rejected(
        &core,
        "SELECT l.k FROM l JOIN r ON l.k = r.k AND r.nonexistent = 'x' LIMIT 10",
        "42703",
    );
}

#[test]
fn rejection_does_not_depend_on_tenant() {
    let (core, path) = seeded();
    let _guard = CleanupGuard(path);
    let sql = "SELECT l.k FROM l JOIN r ON l.k = r.k AND r.tag = 'x' LIMIT 10";
    assert_eq!(
        err_of(&core, "tenant-a", sql),
        err_of(&core, "tenant-b", sql)
    );
}
