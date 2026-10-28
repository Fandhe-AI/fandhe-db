//! 投影式木内の集計呼び出しの抽出と Accumulator への束縛（SQL-42。`docs/spec/04-behavior/` の
//! ポインタのみ。Issue #1811、親 #1810）の結合テスト。関数引数・CASE・単項マイナス・先頭が
//! 集計でない式の中の集計呼び出しを受理し、GROUP BY あり・なしで値を固定する。未対応文脈の
//! fail-closed、集計数上限（`54000`）、テナント境界・公開列スコープも確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn run_opt(
    core: &EngineCore,
    ctx: &PolicyContext,
    sql: &str,
) -> Result<SqlOutcome, engine::sql::allowlist::SqlSurfaceError> {
    core.execute_sql_in_session(ctx, &mut SessionState::default(), sql)
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    match run_opt(core, ctx, sql).unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}")) {
        SqlOutcome::Query(r) => r,
        other => panic!("expected Query for {sql:?}, got {other:?}"),
    }
}

fn code(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> String {
    match run_opt(core, ctx, sql) {
        Ok(o) => panic!("{sql:?} should be rejected, got {o:?}"),
        Err(e) => e.wire_code().to_string(),
    }
}

fn row_cells(r: &QueryResult) -> Vec<Vec<Cell>> {
    r.rows.iter().map(|row| row.cells.clone()).collect()
}

/// 表 `t(g TEXT, n INTEGER, m INTEGER, r REAL, d DOUBLE)`。テナント `t1` の行:
/// `(a,1,5)`・`(a,2,6)`・`(b,10,7)`。別テナントの行（`n=1000`）は結果に現れてはならない。
fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path("sql42-aggregate-expr-tree");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("g", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::Integer, false),
                ColumnDef::new("m", ColumnType::Integer, false),
                ColumnDef::new("r", ColumnType::Real, false),
                ColumnDef::new("d", ColumnType::Double, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("t1");
    let rows: [(u64, &str, i64, i64, &str, &str); 3] = [
        (1, "a", 1, 5, "1.5", "0.5"),
        (2, "a", 2, 6, "2.5", "1.5"),
        (3, "b", 10, 7, "0.25", "2.5"),
    ];
    for (id, g, n, m, r, d) in rows {
        run_opt(
            &core,
            &ctx,
            &format!(
                "INSERT INTO t (id, embedding, g, n, m, r, d) VALUES \
                 ({id}, '[0.{id},0.1]', '{g}', {n}, {m}, {r}, {d}) USING OPERATION_ID 'seed-{id}'"
            ),
        )
        .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    }
    run_opt(
        &core,
        &ctx_for("t2"),
        "INSERT INTO t (id, embedding, g, n, m, r, d) VALUES \
         (9, '[0.9,0.1]', 'a', 1000, 1000, 1.0, 1.0) USING OPERATION_ID 'seed-other'",
    )
    .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    (core, ctx, CleanupGuard(path))
}

fn cells(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<Vec<Cell>> {
    row_cells(&run(core, ctx, sql))
}

fn text(s: &str) -> Cell {
    Cell::Text(s.to_string())
}

#[test]
fn expression_not_starting_with_aggregate_is_accepted() {
    let (core, ctx, _g) = setup();
    // sum(n)=13（別テナントの 1000 は混入しない）。
    assert_eq!(
        cells(&core, &ctx, "SELECT 1 + sum(n) FROM t"),
        vec![vec![Cell::SignedInteger(14)]]
    );
    assert_eq!(
        cells(&core, &ctx, "SELECT -sum(n) FROM t"),
        vec![vec![Cell::SignedInteger(-13)]]
    );
    assert_eq!(
        cells(&core, &ctx, "SELECT (sum(n)) + 1 FROM t"),
        vec![vec![Cell::SignedInteger(14)]]
    );
}

#[test]
fn parenthesized_single_aggregate_keeps_aggregate_name() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT (sum(n)) FROM t");
    assert_eq!(row_cells(&r), vec![vec![Cell::SignedInteger(13)]]);
    assert_eq!(r.columns.len(), 1);
    let r2 = run(&core, &ctx, "SELECT sum(n) FROM t");
    assert_eq!(format!("{:?}", r.columns), format!("{:?}", r2.columns));
}

#[test]
fn aggregate_inside_function_arguments() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        cells(
            &core,
            &ctx,
            "SELECT coalesce(sum(n), 0) FROM t WHERE n > 100"
        ),
        vec![vec![Cell::SignedInteger(0)]]
    );
    assert_eq!(
        cells(&core, &ctx, "SELECT upper(max(g)) FROM t"),
        vec![vec![text("B")]]
    );
    assert_eq!(
        cells(
            &core,
            &ctx,
            "SELECT g, upper(min(g)) FROM t GROUP BY g ORDER BY g"
        ),
        vec![vec![text("a"), text("A")], vec![text("b"), text("B")]]
    );
    assert_eq!(
        cells(&core, &ctx, "SELECT abs(min(n) - max(n)) FROM t"),
        vec![vec![Cell::SignedInteger(9)]]
    );
    assert_eq!(
        cells(&core, &ctx, "SELECT round(avg(d), 1) FROM t"),
        vec![vec![Cell::Float(1.5)]]
    );
}

#[test]
fn aggregate_inside_case_and_cast() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        cells(
            &core,
            &ctx,
            "SELECT CASE WHEN count(*) > 2 THEN 'many' ELSE 'few' END FROM t"
        ),
        vec![vec![text("many")]]
    );
    assert_eq!(
        cells(
            &core,
            &ctx,
            "SELECT g, CASE WHEN sum(n) > 5 THEN 1 ELSE 0 END AS big \
             FROM t GROUP BY g ORDER BY g"
        ),
        vec![
            vec![text("a"), Cell::SignedInteger(0)],
            vec![text("b"), Cell::SignedInteger(1)]
        ]
    );
    assert_eq!(
        cells(&core, &ctx, "SELECT sum(n)::text FROM t"),
        vec![vec![text("13")]]
    );
}

#[test]
fn filter_clause_inside_expression() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        cells(
            &core,
            &ctx,
            "SELECT count(*) FILTER (WHERE n > 1) + 1 FROM t"
        ),
        vec![vec![Cell::SignedInteger(3)]]
    );
    assert_eq!(
        cells(
            &core,
            &ctx,
            "SELECT coalesce(sum(n) FILTER (WHERE g = 'z'), 0) FROM t"
        ),
        vec![vec![Cell::SignedInteger(0)]]
    );
}

#[test]
fn duplicate_aggregates_give_consistent_values() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        cells(&core, &ctx, "SELECT sum(n) + sum(n), sum(n) FROM t"),
        vec![vec![Cell::SignedInteger(26), Cell::SignedInteger(13)]]
    );
    // FILTER が異なる同一集計は共有されず、それぞれの条件で集計される。
    assert_eq!(
        cells(
            &core,
            &ctx,
            "SELECT count(*) + 0, count(*) FILTER (WHERE n > 1) + 0 FROM t"
        ),
        vec![vec![Cell::SignedInteger(3), Cell::SignedInteger(2)]]
    );
}

#[test]
fn evaluation_errors_do_not_return_partial_results() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        code(&core, &ctx, "SELECT coalesce(sum(n) / 0, 0) FROM t"),
        "22012"
    );
    assert_eq!(
        code(&core, &ctx, "SELECT abs(max(n) * 1000000000) FROM t"),
        "22003"
    );
}

#[test]
fn aggregate_count_limit_is_fail_closed() {
    let (core, ctx, _g) = setup();
    let many = vec!["count(*)"; 40].join(" + ");
    assert_eq!(
        code(&core, &ctx, &format!("SELECT coalesce({many}, 0) FROM t")),
        "54000"
    );
}

#[test]
fn unsupported_shapes_stay_rejected() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT coalesce(sum(count(*)), 0) FROM t",
        "SELECT upper(g) FROM t GROUP BY g",
        "SELECT coalesce(sum(n), 0) FROM t WHERE coalesce(sum(n), 0) > 1",
        "WITH w AS (SELECT * FROM t) SELECT coalesce(sum(n), 0) FROM w",
        "SELECT coalesce(sum(n), 0) FROM t UNION SELECT 1 FROM t",
        "SELECT n FROM t WHERE n IN (SELECT coalesce(max(n), 0) FROM t)",
        "CREATE VIEW v AS SELECT coalesce(sum(n), 0) FROM t",
        "SELECT coalesce(sum(n), $1) FROM t",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42601", "{sql}");
    }
    // 集計の外の `GROUP BY` に無い列は 42803（Issue #1818・SQL-58）。
    for sql in [
        "SELECT coalesce(sum(n), n) FROM t",
        "SELECT g, coalesce(sum(n), m) FROM t GROUP BY g",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42803", "{sql}");
    }
    // 式の被演算子にできない結果は 0A000（成功させない）。
    assert_eq!(
        code(&core, &ctx, "SELECT cardinality(array_agg(n)) FROM t"),
        "0A000"
    );
}

#[test]
fn scalar_subquery_item_dispatch_is_unchanged() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        cells(&core, &ctx, "SELECT (SELECT count(*) FROM t) FROM t").len(),
        3
    );
}

#[test]
fn tenant_and_visibility_columns_are_not_exposed() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        code(&core, &ctx, "SELECT coalesce(max(tenant_id), '') FROM t"),
        "42703"
    );
    assert_eq!(
        code(&core, &ctx, "SELECT upper(max(visibility)) FROM t"),
        "42703"
    );
}

#[test]
fn explain_and_describe_accept_new_shape() {
    let (core, ctx, _g) = setup();
    let o = run_opt(&core, &ctx, "EXPLAIN SELECT coalesce(sum(n), 0) FROM t").expect("explain");
    assert!(matches!(o, SqlOutcome::Explain(_)));
}
