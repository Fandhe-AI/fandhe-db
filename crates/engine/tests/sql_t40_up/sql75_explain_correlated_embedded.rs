//! WHERE の相関部分クエリの `EXPLAIN` 結合テスト（Issue #2977・SQL-75 (1)(2)(3)(5)(6)・
//! SQL-61・RLS-10 ポインタ）。
//!
//! 文を実行せず、`subquery` 段に `correlated: true`・`correlation_keys`・`correlated_residual` を
//! 並べて返す。出力は件数と有無だけで、テナント・行数・列名に依存しない。束縛エラーは実行したときと
//! 同じ `wire_code`。実行が拒否する形（相関 ANY/ALL・外側スコープ越え等）は同じコードで拒否する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
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

/// a(k, v)・b(k, w)・c(k, x)・pt(id PK, k)・em（空）。tenant-a にだけ行がある。
fn seeded(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for sql in [
        "CREATE TABLE a (k BIGINT, v TEXT)",
        "CREATE TABLE b (k BIGINT, w TEXT)",
        "CREATE TABLE c (k BIGINT, x TEXT)",
        "CREATE TABLE pt (id BIGINT PRIMARY KEY, k BIGINT)",
        "CREATE TABLE em (k BIGINT, e TEXT)",
        "INSERT INTO a (k, v) VALUES (1, 'a1'), (2, 'a2'), (3, 'a3')",
        "INSERT INTO b (k, w) VALUES (2, 'b2'), (3, 'b3')",
        "INSERT INTO c (k, x) VALUES (2, 'c2'), (4, 'c4')",
        "INSERT INTO pt (id, k) VALUES (1, 2), (2, 3)",
    ] {
        exec_as(&core, "tenant-a", sql);
    }
    (core, guard)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn lines(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    match run(core, tenant, sql).unwrap_or_else(|c| panic!("{sql} failed with {c}")) {
        SqlOutcome::Explain(r) => r
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn code(core: &EngineCore, sql: &str) -> String {
    match run(core, "tenant-a", sql) {
        Err(c) => c,
        Ok(_) => panic!("{sql} must be rejected"),
    }
}

fn plan(core: &EngineCore, sql: &str) -> Vec<String> {
    lines(core, "tenant-a", sql)
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// 走査の既定分類（全走査）の 2 行を、`pad` 分だけ字下げして返す。
fn full_scan(pad: &str) -> Vec<String> {
    vec![
        format!("{pad}scalar_plan: plain_scan"),
        format!("{pad}access_path: full_scan"),
    ]
}

fn concat(parts: Vec<Vec<String>>) -> Vec<String> {
    parts.into_iter().flatten().collect()
}

fn corr(depth: usize, kind: &str, keys: usize, residual: &str, inner: Vec<String>) -> Vec<String> {
    let pad = "  ".repeat(depth - 1);
    concat(vec![
        strs(&[
            &format!("{pad}-> subquery"),
            &format!("{pad}   subquery_kind: {kind}"),
            &format!("{pad}   correlated: true"),
            &format!("{pad}   correlation_keys: {keys}"),
            &format!("{pad}   correlated_residual: {residual}"),
            &format!("{pad}  -> scan"),
        ]),
        inner,
    ])
}

fn subquery_lines(got: &[String]) -> Vec<String> {
    got.iter()
        .skip_while(|l| !l.starts_with("-> subquery"))
        .cloned()
        .collect()
}

#[test]
fn correlated_exists_reports_keys_and_no_residual() {
    let (core, _g) = seeded("x75r-exists");
    for (sql, kind) in [
        (
            "EXPLAIN SELECT k FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)",
            "exists",
        ),
        (
            "EXPLAIN SELECT k FROM a WHERE NOT EXISTS (SELECT 1 FROM b WHERE b.k = a.k)",
            "not_exists",
        ),
    ] {
        let got = plan(&core, sql);
        assert_eq!(
            subquery_lines(&got),
            corr(1, kind, 1, "none", full_scan("     ")),
            "{sql}"
        );
    }
}

#[test]
fn correlated_exists_with_non_equality_reports_residual() {
    let (core, _g) = seeded("x75r-resid");
    let got = plan(
        &core,
        "EXPLAIN SELECT k FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k AND b.k > a.k)",
    );
    assert_eq!(
        subquery_lines(&got),
        corr(1, "exists", 1, "present", full_scan("     "))
    );
}

#[test]
fn correlated_in_and_not_in() {
    let (core, _g) = seeded("x75r-in");
    let got = plan(
        &core,
        "EXPLAIN SELECT k FROM a WHERE k IN (SELECT k FROM b WHERE b.k = a.k)",
    );
    // 対象列の等価が相関キーに加わる。
    assert_eq!(
        subquery_lines(&got),
        corr(1, "in", 2, "none", full_scan("     "))
    );
    let got = plan(
        &core,
        "EXPLAIN SELECT k FROM a WHERE k NOT IN (SELECT k FROM b WHERE b.k = a.k)",
    );
    assert_eq!(
        subquery_lines(&got),
        corr(1, "not_in", 1, "none", full_scan("     "))
    );
}

#[test]
fn correlated_scalar_aggregate_and_plain() {
    let (core, _g) = seeded("x75r-scalar");
    let got = plan(
        &core,
        "EXPLAIN SELECT k FROM a WHERE k > (SELECT count(*) FROM b WHERE b.k = a.k)",
    );
    let sub = subquery_lines(&got);
    assert!(sub.contains(&"   correlated: true".to_string()), "{sub:?}");
    assert!(
        sub.contains(&"   correlation_keys: 1".to_string()),
        "{sub:?}"
    );
    assert!(sub.iter().any(|l| l.contains("-> aggregate")), "{sub:?}");
    assert!(
        sub.contains(&"     group_keys: 1".to_string())
            || sub.iter().any(|l| l.contains("group_keys: 1"))
    );
    let got = plan(
        &core,
        "EXPLAIN SELECT k FROM a WHERE k = (SELECT k FROM b WHERE b.k = a.k)",
    );
    assert_eq!(
        subquery_lines(&got),
        corr(1, "scalar", 1, "none", full_scan("     "))
    );
}

#[test]
fn nested_non_correlated_subquery_hangs_under_the_correlated_stage() {
    let (core, _g) = seeded("x75r-nested");
    let got = plan(
        &core,
        "EXPLAIN SELECT k FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k AND b.k IN (SELECT k FROM c))",
    );
    let sub = subquery_lines(&got);
    assert_eq!(
        sub.iter()
            .filter(|l| l.trim_start() == "-> subquery")
            .count(),
        2,
        "{sub:?}"
    );
    // 入れ子は相関段の内側の scan の下（外側の直下ではない）。
    assert_eq!(
        got.iter().filter(|l| l.starts_with("-> subquery")).count(),
        1,
        "{got:?}"
    );
}

#[test]
fn rejected_shapes_match_execution_codes() {
    let (core, _g) = seeded("x75r-reject");
    for sql in [
        // 相関 ANY/ALL。
        "SELECT k FROM a WHERE k = ANY (SELECT k FROM b WHERE b.k = a.k)",
        // ORDER BY と LIMIT を伴う相関スカラー。
        "SELECT k FROM a WHERE k = (SELECT k FROM b WHERE b.k = a.k ORDER BY k LIMIT 1)",
        // 未知列・曖昧な参照。
        "SELECT k FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.zz)",
        "SELECT k FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.zz = a.k)",
    ] {
        let want = code(&core, sql);
        let got = code(&core, &format!("EXPLAIN {sql}"));
        assert_eq!(got, want, "{sql}");
    }
}

#[test]
fn explain_is_tenant_independent_and_side_effect_free() {
    let (core, _g) = seeded("x75r-tenant");
    let sql = "EXPLAIN SELECT k FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)";
    assert_eq!(lines(&core, "tenant-a", sql), lines(&core, "tenant-b", sql));
}

#[test]
fn cte_and_derived_table_with_subqueries_match_execution_codes() {
    // 実行が CTE・派生表の主クエリの部分クエリを拒否する間は、EXPLAIN も同じコードで拒否する（パリティ）。
    let (core, _g) = seeded("x75r-cte");
    for sql in [
        "WITH w AS (SELECT k FROM a) SELECT k FROM w WHERE k IN (SELECT k FROM b)",
        "SELECT k FROM (SELECT k FROM a) t WHERE EXISTS (SELECT 1 FROM b WHERE b.k = t.k)",
    ] {
        let want = code(&core, sql);
        assert_eq!(code(&core, &format!("EXPLAIN {sql}")), want, "{sql}");
    }
}

#[test]
fn ranked_search_with_subquery_matches_execution_codes() {
    let (core, _g) = seeded("x75r-ranked");
    exec_as(
        &core,
        "tenant-a",
        "CREATE TABLE hv (id BIGINT PRIMARY KEY, h HALFVEC(2))",
    );
    exec_as(
        &core,
        "tenant-a",
        "CREATE TABLE rv (id BIGINT PRIMARY KEY, v VECTOR(2))",
    );
    // HALFVEC の kNN は実行が受理するので EXPLAIN も計画を返す。
    let got = plan(
        &core,
        "EXPLAIN SELECT id FROM hv WHERE id IN (SELECT k FROM b) ORDER BY h <=> '[1,0]' LIMIT 3",
    );
    assert!(
        got.iter().any(|l| l.trim_start() == "-> subquery"),
        "{got:?}"
    );
    // VECTOR の kNN は実行が未対応なので EXPLAIN も同じコードで拒否する。
    let sql = "SELECT id FROM rv WHERE id IN (SELECT k FROM b) ORDER BY v <=> '[1,0]' LIMIT 3";
    let want = code(&core, sql);
    assert_eq!(want, "42601");
    assert_eq!(code(&core, &format!("EXPLAIN {sql}")), want);
}
