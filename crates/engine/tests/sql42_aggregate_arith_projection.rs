//! 集計間算術（`sum(n)+1`・`count(*)*2` など）を集計投影で受理する機能（SQL-42 (5)。
//! `docs/spec/04-behavior/` のポインタのみ。Issue #1541、親 #1537）の結合テスト。
//! 実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql_in_session` 経由で使い、
//! 単一行集計・`GROUP BY` 付き集計での式評価、結果型と列名、`GROUP BY` 外の列・入れ子集計の
//! 拒否（SQL-14 の `42601` 契約）、内部集計項目が `HAVING`／`ORDER BY` から見えないこと、
//! 評価エラー（`22003`／`22012`）、非対応経路の fail-closed、Describe との一致、
//! テナント境界・ビューの公開列スコープを固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
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

fn col_ty(r: &QueryResult, i: usize) -> Option<ColumnType> {
    match &r.columns[i] {
        ColumnMeta::Computed { ty, .. } => ty.clone(),
        other => panic!("expected Computed, got {other:?}"),
    }
}

fn col_name(r: &QueryResult, i: usize) -> String {
    match &r.columns[i] {
        ColumnMeta::Computed { name, .. } => name.clone(),
        other => panic!("expected Computed, got {other:?}"),
    }
}

fn row_cells(r: &QueryResult) -> Vec<Vec<Cell>> {
    r.rows.iter().map(|row| row.cells.clone()).collect()
}

/// 表 `t(g TEXT, n INTEGER, m INTEGER, r REAL, d DOUBLE)`。テナント `t1` の行:
/// `(a,1,5)`・`(a,2,6)`・`(b,10,7)`。別テナントの行（`n=1000`）は結果に現れてはならない。
fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path("sql42-aggregate-arith");
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

#[test]
fn single_row_aggregate_arithmetic_is_evaluated_after_aggregation() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT sum(n)+1, count(*)*2 FROM t");
    // sum(n)=13（別テナントの 1000 は混入しない）・count(*)=3。
    assert_eq!(
        row_cells(&r),
        vec![vec![Cell::SignedInteger(14), Cell::SignedInteger(6)]]
    );
    assert_eq!(col_name(&r, 0), "?column?");
    assert_eq!(col_name(&r, 1), "?column?");
    assert_eq!(col_ty(&r, 0), Some(ColumnType::BigInt));
    assert_eq!(col_ty(&r, 1), Some(ColumnType::BigInt));
}

#[test]
fn aggregate_results_can_be_combined_with_each_other_and_aliased() {
    let (core, ctx, _g) = setup();
    let r = run(
        &core,
        &ctx,
        "SELECT max(n)-min(n) AS spread, sum(n)*2+count(*) FROM t",
    );
    assert_eq!(
        row_cells(&r),
        vec![vec![Cell::SignedInteger(9), Cell::SignedInteger(29)]]
    );
    assert_eq!(col_name(&r, 0), "spread");
    assert_eq!(col_name(&r, 1), "?column?");
    // MIN/MAX(int4) は int4 のまま（PostgreSQL と同じ）。
    assert_eq!(col_ty(&r, 0), Some(ColumnType::Integer));
    assert_eq!(col_ty(&r, 1), Some(ColumnType::BigInt));
    // 従来形（集計単独・別名付き）は列名・型とも不変。
    let r = run(&core, &ctx, "SELECT sum(n) AS s, count(*) FROM t");
    assert_eq!(col_name(&r, 0), "s");
    assert_eq!(col_name(&r, 1), "count");
}

#[test]
fn group_by_with_arithmetic_projection() {
    let (core, ctx, _g) = setup();
    let r = run(
        &core,
        &ctx,
        "SELECT g, sum(n)+1 AS s, count(*)*10 FROM t GROUP BY g ORDER BY g",
    );
    assert_eq!(
        row_cells(&r),
        vec![
            vec![
                Cell::Text("a".to_string()),
                Cell::SignedInteger(4),
                Cell::SignedInteger(20)
            ],
            vec![
                Cell::Text("b".to_string()),
                Cell::SignedInteger(11),
                Cell::SignedInteger(10)
            ],
        ]
    );
    assert_eq!(col_name(&r, 0), "g");
    assert_eq!(col_name(&r, 1), "s");
    assert_eq!(col_name(&r, 2), "?column?");
    // 集計の外で GROUP BY 列を参照する式も受理する。
    let r = run(
        &core,
        &ctx,
        "SELECT g, sum(n)+length(g) FROM t GROUP BY g ORDER BY g",
    );
    assert_eq!(r.rows.len(), 2);
    // LIMIT／OFFSET 後の出力行だけが評価される。
    let r = run(
        &core,
        &ctx,
        "SELECT g, sum(n)*2 FROM t GROUP BY g ORDER BY g LIMIT 1 OFFSET 1",
    );
    assert_eq!(
        row_cells(&r),
        vec![vec![Cell::Text("b".to_string()), Cell::SignedInteger(20)]]
    );
}

#[test]
fn float_results_use_postgres_like_types() {
    let (core, ctx, _g) = setup();
    let r = run(
        &core,
        &ctx,
        "SELECT avg(d)*2, sum(r)+1, sum(r)+sum(r), sum(d)+1, max(n)+1 FROM t",
    );
    assert_eq!(col_ty(&r, 0), Some(ColumnType::Double));
    // real + integer は PostgreSQL でも double precision。
    assert_eq!(col_ty(&r, 1), Some(ColumnType::Double));
    // real + real は real。
    assert_eq!(col_ty(&r, 2), Some(ColumnType::Real));
    assert_eq!(col_ty(&r, 3), Some(ColumnType::Double));
    assert_eq!(col_ty(&r, 4), Some(ColumnType::Integer));
    let cells = &r.rows[0].cells;
    assert_eq!(cells[1], Cell::Float(5.25));
    assert_eq!(cells[2], Cell::Float(8.5));
    assert_eq!(cells[3], Cell::Float(5.5));
    assert_eq!(cells[4], Cell::SignedInteger(11));
    match cells[0] {
        // avg(d) = (0.5 + 1.5 + 2.5) / 3 = 1.5 → *2 = 3.0。
        Cell::Float(v) => assert!((v - 3.0).abs() < 1e-9, "{v}"),
        ref other => panic!("expected Float, got {other:?}"),
    }
}

/// Issue #2270（TABLE-13）: 整数の AVG は numeric を返すため、式の中（numeric 算術は
/// 別 Issue）では fail-closed の 22000 で拒否する。Describe と実行の結果も一致する。
#[test]
fn avg_of_integer_in_expression_is_rejected_with_22000() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT avg(n)*2 FROM t",
        "SELECT g, avg(n)+1 FROM t GROUP BY g",
        "SELECT avg(id)+1 FROM t",
    ] {
        assert_eq!(code(&core, &ctx, sql), "22000", "{sql}");
        let parsed = core.parse_sql(sql).expect("parse");
        let described = core.describe_parsed_in_session(&SessionState::default(), &parsed);
        assert_eq!(
            described.expect_err("describe must reject").wire_code(),
            "22000",
            "{sql}"
        );
    }
}

#[test]
fn bare_column_outside_aggregate_is_rejected_with_existing_contract() {
    let (core, ctx, _g) = setup();
    // 現行契約（SQL-14）の 42601 を維持する。
    for sql in [
        "SELECT sum(n)+m FROM t",
        "SELECT n+sum(n) FROM t",
        "SELECT g, sum(n)+m FROM t GROUP BY g",
        "SELECT g, sum(n)+n FROM t GROUP BY g",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42601", "{sql}");
    }
}

#[test]
fn nested_aggregates_and_aggregate_free_expressions_are_rejected() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT sum(count(*))+1 FROM t",
        "SELECT sum(n+count(*))+1 FROM t",
        "SELECT g, g+g FROM t GROUP BY g",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42601", "{sql}");
    }
    // 集計関数は WHERE に置けない（従来どおり）。
    assert_eq!(
        code(&core, &ctx, "SELECT sum(n)+1 FROM t WHERE sum(n) > 1"),
        "42601"
    );
}

#[test]
fn evaluation_errors_are_fail_closed() {
    let (core, ctx, _g) = setup();
    assert_eq!(code(&core, &ctx, "SELECT sum(n)/0 FROM t"), "22012");
    // max(n)=10 は int4。10*1000000000 は int4 の範囲外。
    assert_eq!(
        code(&core, &ctx, "SELECT max(n)*1000000000 FROM t"),
        "22003"
    );
}

#[test]
fn internal_aggregates_are_invisible_to_having_and_order_by() {
    let (core, ctx, _g) = setup();
    // 式中の `sum(n)` は HAVING から名前では解決できない（未知名は 22000。曖昧 42702 ではない）。
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT sum(n)+1 FROM t GROUP BY g HAVING sum > 1"
        ),
        "42703"
    );
    // 可視の `sum(n)` と併存しても、HAVING／ORDER BY は可視項目へ一意に解決する。
    let r = run(
        &core,
        &ctx,
        "SELECT g, sum(n), sum(n)+1 FROM t GROUP BY g HAVING sum > 5 ORDER BY g",
    );
    assert_eq!(
        row_cells(&r),
        vec![vec![
            Cell::Text("b".to_string()),
            Cell::SignedInteger(10),
            Cell::SignedInteger(11)
        ]]
    );
    // 別名を持つ式項目の別名は他の式から参照できない。
    assert_eq!(code(&core, &ctx, "SELECT sum(n) AS s, s+1 FROM t"), "42601");
}

#[test]
fn numeric_like_aggregate_results_are_rejected_in_expressions() {
    let (core, ctx, _g) = setup();
    // 疑似列 `id` の SUM は NUMERIC(20,0)。式での近似はせず fail-closed。
    assert_eq!(code(&core, &ctx, "SELECT sum(id)+1 FROM t"), "22000");
}

#[test]
fn too_many_aggregates_in_one_item_are_rejected() {
    let (core, ctx, _g) = setup();
    let many = (0..40).map(|_| "count(*)").collect::<Vec<_>>().join("+");
    assert_eq!(code(&core, &ctx, &format!("SELECT {many} FROM t")), "54000");
}

#[test]
fn count_star_fast_paths_evaluate_the_expression() {
    let (core, ctx, _g) = setup();
    // 2 回目以降は可視 id キャッシュ経路を通りうる。いずれも終端で式を評価する。
    for _ in 0..3 {
        let r = run(&core, &ctx, "SELECT count(*)*2 FROM t");
        assert_eq!(row_cells(&r), vec![vec![Cell::SignedInteger(6)]]);
    }
    run_opt(&core, &ctx, "CREATE INDEX t_g ON t (g)").ok();
    let r = run(&core, &ctx, "SELECT count(*)*2 FROM t WHERE g = 'a'");
    assert_eq!(row_cells(&r), vec![vec![Cell::SignedInteger(4)]]);
}

#[test]
fn describe_matches_execution() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT sum(n)+1, count(*)*2 FROM t",
        "SELECT g, sum(n)+1 AS s FROM t GROUP BY g",
        "SELECT max(n)-min(n) FROM t",
        "SELECT avg(d)*2 FROM t",
    ] {
        let parsed = core.parse_sql(sql).expect("parse");
        let described = core
            .describe_parsed_in_session(&SessionState::default(), &parsed)
            .expect("describe");
        let executed = run(&core, &ctx, sql);
        assert_eq!(described, Some(executed.columns), "{sql}");
    }
}

#[test]
fn unsupported_contexts_fail_closed() {
    let (core, ctx, _g) = setup();
    // ビュー本体・集合演算の枝・CTE 本体では集計式項目を受理しない。
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut ddl,
            "CREATE VIEW v_expr AS SELECT sum(n)+1 FROM t",
        )
        .expect_err("view body must reject an aggregate expression item");
    assert_eq!(err.wire_code(), "42601");
    for sql in [
        "SELECT count(*) FROM t UNION ALL SELECT count(*)+1 FROM t",
        "WITH w AS (SELECT n FROM t LIMIT 5) SELECT sum(n)+1 FROM w",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42601", "{sql}");
    }
}

#[test]
fn view_public_columns_scope_covers_expression_items() {
    let (core, ctx, _g) = setup();
    let mut ddl = SessionState::default();
    ddl.allow_ddl();
    core.execute_sql_in_session(&ctx, &mut ddl, "CREATE VIEW v_pub AS SELECT g, n FROM t")
        .expect("create view");
    // 公開列 (g, n) だけを使う式は通る。
    let r = run(&core, &ctx, "SELECT sum(n)+1 FROM v_pub");
    assert_eq!(row_cells(&r), vec![vec![Cell::SignedInteger(14)]]);
    // 非公開列 `m` を式内の集計引数・集計の外の参照に使うことは公開列スコープ違反（22000）。
    assert_eq!(code(&core, &ctx, "SELECT sum(m)+1 FROM v_pub"), "42703");
    assert_eq!(
        code(&core, &ctx, "SELECT m, sum(n)+m FROM v_pub GROUP BY m"),
        "42703"
    );
}
