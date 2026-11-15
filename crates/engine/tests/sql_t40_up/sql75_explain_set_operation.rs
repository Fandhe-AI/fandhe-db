//! 集合演算の `EXPLAIN` 結合テスト（Issue #2047・SQL-75 (1)(2)(3)(5)(6)・SQL-29 (c)・RLS-10 (b)
//! ポインタ）。
//!
//! 文を実行せず、段の木（`set_operation`／`scan` ほか）で演算子・入れ子・各枝の走査分類・
//! 全体／枝内の件数制限を返す。出力は閉じた語彙と項目数だけで、テナント・可視性・行数に依存しない。
//! 束縛エラーは実行したときと同じ `wire_code`。未対応の形は従来どおり `42601`。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::result_limits::ResultLimits;
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

/// 深さ `depth`（>= 2）の葉の `scan` 節（既定の全走査）。
fn scan(depth: usize, rel: usize) -> Vec<String> {
    let pad = "  ".repeat(depth - 1);
    vec![
        format!("{pad}-> scan"),
        format!("{pad}   relation: {rel}"),
        format!("{pad}   scalar_plan: plain_scan"),
        format!("{pad}   access_path: full_scan"),
    ]
}

fn op(depth: usize, token: &str) -> Vec<String> {
    let pad = "  ".repeat(depth - 1);
    vec![
        format!("{pad}-> set_operation"),
        format!("{pad}   set_operator: {token}"),
    ]
}

fn concat(parts: Vec<Vec<String>>) -> Vec<String> {
    parts.into_iter().flatten().collect()
}

#[test]
fn each_operator_renders_its_token() {
    let (core, _g) = seeded("x75s-ops");
    for (kw, token) in [
        ("UNION", "union"),
        ("UNION ALL", "union_all"),
        ("INTERSECT", "intersect"),
        ("INTERSECT ALL", "intersect_all"),
        ("UNION DISTINCT", "union"),
        ("INTERSECT DISTINCT", "intersect"),
        ("EXCEPT", "except"),
        ("EXCEPT ALL", "except_all"),
        ("EXCEPT DISTINCT", "except"),
    ] {
        let sql = format!("EXPLAIN SELECT k FROM a {kw} SELECT k FROM b");
        let want = concat(vec![
            strs(&["set_operation", &format!("set_operator: {token}")]),
            scan(1, 1),
            scan(1, 2),
        ]);
        assert_eq!(plan(&core, &sql), want, "{sql}");
    }
}

#[test]
fn intersect_binds_tighter_than_union() {
    let (core, _g) = seeded("x75s-prec");
    let got = plan(
        &core,
        "EXPLAIN SELECT k FROM a UNION SELECT k FROM b INTERSECT SELECT k FROM c LIMIT 5",
    );
    let want = concat(vec![
        strs(&["limit", "limit: present", "offset: none"]),
        op(1, "union"),
        scan(2, 1),
        op(2, "intersect"),
        scan(3, 2),
        scan(3, 3),
    ]);
    assert_eq!(got, want);
}

#[test]
fn parentheses_regroup_and_leading_paren_is_accepted() {
    let (core, _g) = seeded("x75s-paren");
    let got = plan(
        &core,
        "EXPLAIN (SELECT k FROM a UNION SELECT k FROM b) INTERSECT SELECT k FROM c",
    );
    let want = concat(vec![
        strs(&["set_operation", "set_operator: intersect"]),
        op(1, "union"),
        scan(2, 1),
        scan(2, 2),
        scan(1, 3),
    ]);
    assert_eq!(got, want);
    // 先頭が括弧の単独枝。
    let got = plan(&core, "EXPLAIN (SELECT k FROM a) UNION SELECT k FROM b");
    let want = concat(vec![
        strs(&["set_operation", "set_operator: union"]),
        scan(1, 1),
        scan(1, 2),
    ]);
    assert_eq!(got, want);
}

#[test]
fn overall_limit_and_offset() {
    let (core, _g) = seeded("x75s-limit");
    for (suffix, l, o) in [
        ("LIMIT 3", "present", "none"),
        ("LIMIT 3 OFFSET 1", "present", "present"),
        ("LIMIT ALL OFFSET 1", "none", "present"),
    ] {
        let got = plan(
            &core,
            &format!("EXPLAIN SELECT k FROM a UNION ALL SELECT k FROM b {suffix}"),
        );
        let want = concat(vec![
            strs(&["limit", &format!("limit: {l}"), &format!("offset: {o}")]),
            op(1, "union_all"),
            scan(2, 1),
            scan(2, 2),
        ]);
        assert_eq!(got, want, "{suffix}");
    }
}

#[test]
fn limited_branch_shows_limit_and_sort_stages() {
    let (core, _g) = seeded("x75s-limited");
    let got = plan(
        &core,
        "EXPLAIN (SELECT k FROM a ORDER BY k LIMIT 2 OFFSET 1) UNION SELECT k FROM b",
    );
    let want = concat(vec![
        strs(&["set_operation", "set_operator: union"]),
        strs(&[
            "-> limit",
            "   limit: present",
            "   offset: present",
            "  -> sort",
            "     sort_keys: 1",
            "    -> scan",
            "       relation: 1",
            "       scalar_plan: plain_scan",
            "       access_path: full_scan",
        ]),
        scan(1, 2),
    ]);
    assert_eq!(got, want);
}

#[test]
fn aggregate_branch_shows_aggregate_stage_with_full_scan() {
    let (core, _g) = seeded("x75s-agg");
    let got = plan(
        &core,
        "EXPLAIN SELECT k, COUNT(*) FROM a GROUP BY k UNION ALL SELECT k, COUNT(*) FROM b GROUP BY k",
    );
    let agg = |rel: usize| {
        strs(&[
            "-> aggregate",
            "   group_keys: 1",
            "   aggregates: 1",
            "   having: none",
        ])
        .into_iter()
        .chain(scan(2, rel))
        .collect::<Vec<_>>()
    };
    let want = concat(vec![
        strs(&["set_operation", "set_operator: union_all"]),
        agg(1),
        agg(2),
    ]);
    assert_eq!(got, want);
}

#[test]
fn primary_key_branch_is_classified_per_branch() {
    let (core, _g) = seeded("x75s-pk");
    let got = plan(
        &core,
        "EXPLAIN SELECT k FROM pt WHERE id = 1 UNION SELECT k FROM a",
    );
    let want = concat(vec![
        strs(&["set_operation", "set_operator: union"]),
        strs(&[
            "-> scan",
            "   relation: 1",
            "   scalar_plan: index_primary_key",
            "   access_path: primary_key_lookup",
        ]),
        scan(1, 2),
    ]);
    assert_eq!(got, want);
}

#[test]
fn output_is_tenant_and_data_independent() {
    let (core, _g) = seeded("x75s-tenant");
    let sql = "EXPLAIN SELECT k FROM a UNION SELECT k FROM b LIMIT 10";
    let with_rows = lines(&core, "tenant-a", sql);
    let no_rows = lines(&core, "tenant-b", sql);
    assert_eq!(with_rows, no_rows);
    let empty = lines(
        &core,
        "tenant-a",
        "EXPLAIN SELECT k FROM em UNION SELECT k FROM em LIMIT 10",
    );
    assert_eq!(empty.len(), with_rows.len());
}

#[test]
fn explain_does_not_execute_the_set_operation() {
    let (core, _g) = seeded("x75s-noexec");
    // 結果行数の上限を極小にすると LIMIT 省略の SELECT は 54000 だが、EXPLAIN は実行しないので成功する。
    let core = core.with_result_limits(ResultLimits::new(1));
    assert_eq!(
        code(&core, "SELECT k FROM a UNION ALL SELECT k FROM b"),
        "54000"
    );
    assert!(!plan(&core, "EXPLAIN SELECT k FROM a UNION ALL SELECT k FROM b").is_empty());
}

#[test]
fn binding_errors_match_execution() {
    let (core, _g) = seeded("x75s-errors");
    exec_as(&core, "tenant-a", "CREATE TABLE tt (k TEXT)");
    exec_as(&core, "tenant-a", "CREATE TABLE vv (k BIGINT, e VECTOR(3))");
    let many = (0..17)
        .map(|_| "SELECT k FROM a".to_string())
        .collect::<Vec<_>>()
        .join(" UNION ALL ");
    let bodies = vec![
        "SELECT k FROM a UNION SELECT k FROM nope".to_string(),
        "SELECT zzz FROM a UNION SELECT k FROM b".to_string(),
        "SELECT k FROM a UNION SELECT k FROM tt".to_string(),
        "SELECT k FROM a UNION SELECT k, v FROM a".to_string(),
        "SELECT k FROM a UNION SELECT k FROM b LIMIT 99999999999".to_string(),
        "SELECT e FROM vv UNION SELECT e FROM vv".to_string(),
        "SELECT e FROM vv INTERSECT ALL SELECT e FROM vv".to_string(),
        many,
    ];
    for body in &bodies {
        let plain = code(&core, body);
        let explained = code(&core, &format!("EXPLAIN {body}"));
        assert_eq!(plain, explained, "{body}");
    }
    assert_eq!(code(&core, &bodies[0]), "42P01");
    assert_eq!(code(&core, &bodies[1]), "42703");
    assert_eq!(code(&core, &bodies[2]), "42804");
}

#[test]
fn unsupported_forms_stay_rejected() {
    let (core, _g) = seeded("x75s-reject");
    for sql in [
        "EXPLAIN SELECT k FROM (SELECT k FROM a) s UNION SELECT k FROM b",
        "EXPLAIN WITH w AS (SELECT k FROM b) SELECT k FROM w UNION SELECT k FROM a",
        "EXPLAIN (SELECT k FROM a)",
        "EXPLAIN SELECT k FROM a WHERE k = $1 UNION SELECT k FROM b",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
    // セッションの無い入口は従来どおり EXPLAIN を 42601 で拒否する。
    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "EXPLAIN SELECT k FROM a UNION SELECT k FROM b",
        )
        .expect_err("session-less EXPLAIN must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn works_inside_explicit_transaction() {
    let (core, _g) = seeded("x75s-tx");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let c = ctx("tenant-a");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let out = core
        .execute_sql_in_txn(
            &c,
            &mut session,
            &mut txn,
            "EXPLAIN SELECT k FROM a UNION SELECT k FROM b",
        )
        .expect("explain in tx");
    assert!(matches!(out, SqlOutcome::Explain(_)));
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "COMMIT")
        .expect("commit");
}

/// 括弧なし末尾 `ORDER BY`（集合全体のキー。Issue #3266・#3267）は `EXPLAIN`・`EXPLAIN ANALYZE` でも
/// 実行と同じ分類になる。成功する形は全体 `sort` 段（キー数のみ）を `limit` 段と `set_operation`
/// 段の間に出し、解決できないキーは `42703`／`42P10`、式キーは `0A000`（意図的な拒否）。
// intentional: docs/design/set-operations.md
#[test]
fn trailing_order_by_is_classified_like_execution() {
    let (core, _g) = seeded("x75s-order-by");
    for prefix in ["EXPLAIN", "EXPLAIN ANALYZE"] {
        for (tail, want) in [
            ("ORDER BY nope", "42703"),
            ("ORDER BY 2", "42P10"),
            ("ORDER BY lower(k)", "0A000"),
        ] {
            let sql = format!("{prefix} SELECT k FROM a UNION SELECT k FROM b {tail}");
            assert_eq!(code(&core, &sql), want, "{sql}");
        }
        for tail in ["ORDER BY k", "ORDER BY 1 DESC NULLS FIRST LIMIT 1"] {
            let sql = format!("{prefix} SELECT k FROM a UNION SELECT k FROM b {tail}");
            assert!(run(&core, "tenant-a", &sql).is_ok(), "{sql}");
        }
    }
    let plan_lines = plan(
        &core,
        "EXPLAIN SELECT k FROM a UNION SELECT k FROM b ORDER BY 1 DESC LIMIT 1",
    );
    let text = plan_lines.join("\n");
    let limit = text.find("limit").expect("limit stage");
    let sort = text.find("sort_keys: 1").expect("sort stage");
    let set_op = text.find("set_operation").expect("set_operation stage");
    assert!(limit < sort && sort < set_op, "{text}");
}
/// 定数枝の葉 `result`（Issue #3271）。`relation:` 行は持たない。
fn result_leaf(depth: usize) -> Vec<String> {
    let pad = "  ".repeat(depth - 1);
    vec![
        format!("{pad}-> result"),
        format!("{pad}   source: constant"),
    ]
}

#[test]
fn expression_branch_renders_like_a_plain_scan() {
    let (core, _g) = seeded("x75s-expr");
    let got = plan(&core, "EXPLAIN SELECT k+1 FROM a UNION SELECT k FROM b");
    let want = concat(vec![
        strs(&["set_operation", "set_operator: union"]),
        scan(1, 1),
        scan(1, 2),
    ]);
    assert_eq!(got, want);
}

#[test]
fn const_branch_renders_a_result_leaf_and_consumes_an_ordinal() {
    let (core, _g) = seeded("x75s-const");
    let got = plan(&core, "EXPLAIN SELECT 7 UNION SELECT k FROM a");
    assert_eq!(
        got,
        concat(vec![
            strs(&["set_operation", "set_operator: union"]),
            result_leaf(1),
            scan(1, 2)
        ])
    );
    let got = plan(&core, "EXPLAIN SELECT k FROM a UNION SELECT 7");
    assert_eq!(
        got,
        concat(vec![
            strs(&["set_operation", "set_operator: union"]),
            scan(1, 1),
            result_leaf(1)
        ])
    );
}

#[test]
fn every_operator_accepts_const_and_expression_branches() {
    let (core, _g) = seeded("x75s-expr-ops");
    for (kw, token) in [
        ("UNION", "union"),
        ("UNION ALL", "union_all"),
        ("INTERSECT", "intersect"),
        ("INTERSECT ALL", "intersect_all"),
        ("EXCEPT", "except"),
        ("EXCEPT ALL", "except_all"),
    ] {
        let sql = format!("EXPLAIN SELECT 7 {kw} SELECT k+0 FROM b");
        let want = concat(vec![
            strs(&["set_operation", &format!("set_operator: {token}")]),
            result_leaf(1),
            scan(1, 2),
        ]);
        assert_eq!(plan(&core, &sql), want, "{sql}");
        let sql = format!("EXPLAIN SELECT k*1 FROM b {kw} SELECT 7");
        let want = concat(vec![
            strs(&["set_operation", &format!("set_operator: {token}")]),
            scan(1, 1),
            result_leaf(1),
        ]);
        assert_eq!(plan(&core, &sql), want, "{sql}");
    }
}

#[test]
fn nested_expression_branches_render() {
    let (core, _g) = seeded("x75s-expr-nest");
    let got = plan(
        &core,
        "EXPLAIN (SELECT 7 UNION SELECT k FROM a) INTERSECT SELECT k+0 FROM b",
    );
    let want = concat(vec![
        strs(&["set_operation", "set_operator: intersect"]),
        op(1, "union"),
        result_leaf(2),
        scan(2, 2),
        scan(1, 3),
    ]);
    assert_eq!(got, want);
}

#[test]
fn limited_expression_branch_and_overall_order_by_render() {
    let (core, _g) = seeded("x75s-expr-limited");
    let got = plan(
        &core,
        "EXPLAIN (SELECT k+1 AS m FROM a ORDER BY m LIMIT 2) UNION SELECT 0 ORDER BY 1 LIMIT 1",
    );
    let want = concat(vec![strs(&[
        "limit",
        "limit: present",
        "offset: none",
        "-> sort",
        "   sort_keys: 1",
        "  -> set_operation",
        "     set_operator: union",
        "    -> limit",
        "       limit: present",
        "       offset: none",
        "      -> sort",
        "         sort_keys: 1",
        "        -> scan",
        "           relation: 1",
        "           scalar_plan: plain_scan",
        "           access_path: full_scan",
        "    -> result",
        "       source: constant",
    ])]);
    assert_eq!(got, want);
}

#[test]
fn expression_and_const_explain_parity_and_independence() {
    let (core, _g) = seeded("x75s-expr-parity");
    for body in [
        "SELECT 7 UNION SELECT v FROM a",
        "SELECT 1 UNION SELECT 2",
        "SELECT 7 WHERE true UNION SELECT k FROM a",
        "SELECT zzz+1 FROM a UNION SELECT k FROM b",
        "SELECT k FROM a UNION SELECT 7 UNION SELECT k FROM nope",
    ] {
        assert_eq!(
            code(&core, body),
            code(&core, &format!("EXPLAIN {body}")),
            "{body}"
        );
    }
    assert_eq!(code(&core, "EXPLAIN SELECT 1 UNION SELECT 2"), "0A000");
    // テナント・データに依存しない。
    let sql = "EXPLAIN SELECT k+1 FROM a UNION SELECT 7";
    assert_eq!(lines(&core, "tenant-a", sql), lines(&core, "tenant-b", sql));
    // EXPLAIN は式を評価しない（実行は 0 除算、EXPLAIN は成功）。
    assert!(run(&core, "tenant-a", "SELECT 10/(k-2) FROM a UNION SELECT 0").is_err());
    assert!(!plan(&core, "EXPLAIN SELECT 10/(k-2) FROM a UNION SELECT 0").is_empty());
}

#[test]
fn explain_analyze_runs_expression_and_const_branches() {
    let (core, _g) = seeded("x75s-expr-analyze");
    let out = lines(
        &core,
        "tenant-a",
        "EXPLAIN ANALYZE SELECT 7 UNION SELECT k+1 FROM a",
    );
    assert!(out.iter().any(|l| l.contains("result")), "{out:?}");
}

#[test]
fn recursive_anchor_with_const_or_expression_set_operation_is_fail_closed_or_rendered() {
    let (core, _g) = seeded("x75s-expr-anchor");
    for sql in [
        "EXPLAIN WITH RECURSIVE r(n) AS (SELECT 1 UNION SELECT k FROM a UNION ALL SELECT n+1 FROM r WHERE n < 3) SELECT n FROM r",
        "EXPLAIN WITH RECURSIVE r(n) AS (SELECT k+1 FROM a UNION SELECT k FROM b UNION ALL SELECT n+1 FROM r WHERE n < 3) SELECT n FROM r",
    ] {
        if let Err(c) = run(&core, "tenant-a", sql) {
            assert!(
                matches!(c.as_str(), "42601" | "0A000"),
                "{sql}: unexpected {c}"
            );
        }
    }
}
