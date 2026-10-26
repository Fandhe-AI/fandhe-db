//! 集計式項目の既定列名と結果列型（SQL-42。`docs/spec/04-behavior/` のポインタのみ。Issue #1812、
//! 親 #1810）の結合テスト。式項目の `AS` 省略時の列名が PostgreSQL の `FigureColname` と一致し
//! （内部名が RowDescription に漏れない）、`COALESCE`／`CASE`／`NULLIF`／`abs` を含む式の結果列型
//! （型 OID の元）が PostgreSQL 17 と一致することを GROUP BY あり・なしで固定する。

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

/// 先頭列の (列名, 結果列型)。
fn first_col(r: &QueryResult) -> (String, Option<ColumnType>) {
    match r.columns.first() {
        Some(ColumnMeta::Computed { name, ty }) => (name.clone(), ty.clone()),
        other => panic!("expected a computed column, got {other:?}"),
    }
}

fn names(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<String> {
    run(core, ctx, sql)
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Computed { name, .. } => name.clone(),
            other => panic!("expected a computed column, got {other:?}"),
        })
        .collect()
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
                ColumnDef::new("b", ColumnType::BigInt, false),
                ColumnDef::new(
                    "c",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    false,
                ),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("t1");
    let rows: [(u64, &str, i64, i64, &str, &str, &str); 3] = [
        (1, "a", 1, 5, "1.5", "0.5", "1.50"),
        (2, "a", 2, 6, "2.5", "1.5", "2.25"),
        (3, "b", 10, 7, "0.25", "2.5", "10.00"),
    ];
    for (id, g, n, m, r, d, c) in rows {
        run_opt(
            &core,
            &ctx,
            &format!(
                "INSERT INTO t (id, embedding, g, n, m, r, d, b, c) VALUES \
                 ({id}, '[0.{id},0.1]', '{g}', {n}, {m}, {r}, {d}, {m}, {c}) \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        )
        .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    }
    run_opt(
        &core,
        &ctx_for("t2"),
        "INSERT INTO t (id, embedding, g, n, m, r, d, b, c) VALUES \
         (9, '[0.9,0.1]', 'a', 1000, 1000, 1.0, 1.0, 1000, 1000.00) USING OPERATION_ID 'seed-other'",
    )
    .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    (core, ctx, CleanupGuard(path))
}

#[test]
fn default_names_follow_figure_colname() {
    let (core, ctx, _g) = setup();
    for (sql, want) in [
        ("SELECT coalesce(sum(n), 0) FROM t", "coalesce"),
        ("SELECT upper(max(g)) FROM t", "upper"),
        (
            "SELECT CASE WHEN sum(n) > 5 THEN 1 ELSE 0 END FROM t",
            "case",
        ),
        ("SELECT nullif(count(*), 0) FROM t", "nullif"),
        ("SELECT sum(n)::text FROM t", "sum"),
        ("SELECT CAST(sum(n) AS text) FROM t", "sum"),
        ("SELECT count(*)::int FROM t", "count"),
        ("SELECT 1 + sum(n) FROM t", "?column?"),
        ("SELECT -sum(n) FROM t", "?column?"),
        ("SELECT sum(n) FILTER (WHERE n > 1) + 1 FROM t", "?column?"),
        ("SELECT coalesce(sum(n), 0) AS total FROM t", "total"),
    ] {
        assert_eq!(names(&core, &ctx, sql), vec![want.to_string()], "{sql}");
    }
}

#[test]
fn internal_placeholder_names_never_leak() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT sum(n)::text, count(*)::int FROM t",
        "SELECT g, coalesce(sum(n), 0), -min(n), abs(max(n)) FROM t GROUP BY g",
        "SELECT sum(n) + 1 FROM t HAVING sum(n) > 1",
    ] {
        for name in names(&core, &ctx, sql) {
            assert!(!name.contains('\u{1}'), "{sql}: {name:?}");
        }
    }
}

#[test]
fn conditional_and_abs_result_types_follow_postgres() {
    let (core, ctx, _g) = setup();
    for (sql, want) in [
        // PostgreSQL 17: sum(int4) は int8、coalesce(int8, int4) は int8。
        ("SELECT coalesce(sum(n), 0) FROM t", ColumnType::BigInt),
        ("SELECT coalesce(max(n), 0) FROM t", ColumnType::Integer),
        ("SELECT abs(min(n) - max(n)) FROM t", ColumnType::Integer),
        (
            "SELECT CASE WHEN sum(n) > 5 THEN 1 ELSE 0 END FROM t",
            ColumnType::Integer,
        ),
        (
            "SELECT CASE WHEN sum(n) > 5 THEN max(r) ELSE 0 END FROM t",
            ColumnType::Real,
        ),
        ("SELECT nullif(count(*), 0) FROM t", ColumnType::BigInt),
        ("SELECT coalesce(sum(n), 0) + 1 FROM t", ColumnType::BigInt),
        ("SELECT coalesce(max(d), 0) FROM t", ColumnType::Double),
        (
            "SELECT g, coalesce(sum(n), 0) FROM t GROUP BY g",
            ColumnType::Text,
        ),
    ] {
        let (_, ty) = first_col(&run(&core, &ctx, sql));
        assert_eq!(ty, Some(want), "{sql}");
    }
    // GROUP BY あり: 2 列目が式項目。
    let r = run(
        &core,
        &ctx,
        "SELECT g, coalesce(sum(n), 0) FROM t GROUP BY g ORDER BY g",
    );
    match r.columns.get(1) {
        Some(ColumnMeta::Computed { ty, .. }) => assert_eq!(*ty, Some(ColumnType::BigInt)),
        other => panic!("unexpected column {other:?}"),
    }
    assert_eq!(r.rows[0].cells[1], Cell::SignedInteger(3));
}

#[test]
fn abs_of_int4_minimum_is_out_of_range() {
    let (core, ctx, _g) = setup();
    run_opt(
        &core,
        &ctx,
        "INSERT INTO t (id, embedding, g, n, m, r, d, b, c) VALUES \
         (7, '[0.7,0.1]', 'z', -2147483648, 0, 0.0, 0.0, 0, 0.00) USING OPERATION_ID 'seed-min'",
    )
    .expect("insert");
    assert_eq!(code(&core, &ctx, "SELECT abs(min(n)) FROM t"), "22003");
}

fn text_cells(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<Vec<Cell>> {
    run(core, ctx, sql)
        .rows
        .iter()
        .map(|row| row.cells.clone())
        .collect()
}

fn dec(unscaled: i128, scale: u8) -> Cell {
    Cell::Numeric(engine::numeric::Decimal::from_parts(unscaled, scale).expect("decimal"))
}

/// Issue #1812: NUMERIC 結果（`sum(bigint)`・整数の `avg`・NUMERIC 列）を式の中で算術に使う。
/// 期待値は PostgreSQL 17 の結果（値と scale）。b = (5, 6, 7)、n = (1, 2, 10)、c = (1.50, 2.25, 10.00)。
#[test]
fn numeric_aggregates_work_in_arithmetic() {
    let (core, ctx, _g) = setup();
    // sum(bigint) は numeric、+ 整数は scale 0 のまま。
    assert_eq!(
        text_cells(&core, &ctx, "SELECT sum(b) + 1 FROM t"),
        vec![vec![dec(19, 0)]]
    );
    // avg(integer) = 13/3 = 4.3333333333333333（scale 16）。 * 2 は scale 16 のまま。
    assert_eq!(
        text_cells(&core, &ctx, "SELECT avg(n) * 2 FROM t"),
        vec![vec![dec(86_666_666_666_666_666, 16)]]
    );
    // sum(c) = 13.75（scale 2）。 + 整数 / 乗算は scale の和。
    assert_eq!(
        text_cells(&core, &ctx, "SELECT sum(c) * 2 FROM t"),
        vec![vec![dec(2750, 2)]]
    );
    assert_eq!(
        text_cells(&core, &ctx, "SELECT sum(c) + sum(b) FROM t"),
        vec![vec![dec(3175, 2)]]
    );
    // 数値リテラルは numeric（scale 1）。
    assert_eq!(
        text_cells(&core, &ctx, "SELECT sum(c) * 1.5 FROM t"),
        vec![vec![dec(20625, 3)]]
    );
    assert_eq!(
        text_cells(&core, &ctx, "SELECT 0 - sum(c) FROM t"),
        vec![vec![dec(-1375, 2)]]
    );
    // 結果列型は numeric（OID 1700 の元）。
    let (_, ty) = first_col(&run(&core, &ctx, "SELECT sum(b) + 1 FROM t"));
    assert!(matches!(ty, Some(ColumnType::Numeric { .. })), "{ty:?}");
    // GROUP BY あり。
    assert_eq!(
        text_cells(
            &core,
            &ctx,
            "SELECT g, sum(b) + 1 FROM t GROUP BY g ORDER BY g"
        ),
        vec![
            vec![Cell::Text("a".to_string()), dec(12, 0)],
            vec![Cell::Text("b".to_string()), dec(8, 0)],
        ]
    );
}

#[test]
fn numeric_division_follows_postgres_scale() {
    let (core, ctx, _g) = setup();
    // 13.75 / 4 = 3.4375（PostgreSQL 17: 3.4375000000000000。select_div_scale で scale 16）。
    assert_eq!(
        text_cells(&core, &ctx, "SELECT sum(c) / 4 FROM t"),
        vec![vec![dec(34_375_000_000_000_000, 16)]]
    );
}

#[test]
fn numeric_in_conditional_and_cast() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        text_cells(
            &core,
            &ctx,
            "SELECT coalesce(sum(b), 0) FROM t WHERE n > 100"
        ),
        vec![vec![dec(0, 0)]]
    );
    assert_eq!(
        text_cells(&core, &ctx, "SELECT coalesce(sum(b), 0) FROM t"),
        vec![vec![dec(18, 0)]]
    );
    assert_eq!(
        text_cells(&core, &ctx, "SELECT sum(c)::text FROM t"),
        vec![vec![Cell::Text("13.75".to_string())]]
    );
}

#[test]
fn numeric_arithmetic_errors_are_fail_closed() {
    let (core, ctx, _g) = setup();
    assert_eq!(code(&core, &ctx, "SELECT sum(c) / 0 FROM t"), "22012");
    // float との混在は近似せず拒否する。
    assert_eq!(code(&core, &ctx, "SELECT sum(c) + max(d) FROM t"), "0A000");
    // HAVING／ORDER BY 内の NUMERIC 算術は従来どおり 22000。
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT g, count(*) FROM t GROUP BY g HAVING sum(b) + 1 > 10"
        ),
        "22000"
    );
}

#[test]
fn numeric_arithmetic_does_not_leak_other_tenants() {
    let (core, ctx, _g) = setup();
    // 別テナント（b = 1000）の行が合計に混ざらない。
    assert_eq!(
        text_cells(&core, &ctx, "SELECT sum(b) + 0 FROM t"),
        vec![vec![dec(18, 0)]]
    );
}

/// NUMERIC の GROUP BY キーも式の被演算子として numeric 算術に乗る（内部エラーにならない）。
#[test]
fn numeric_group_key_in_arithmetic() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        text_cells(
            &core,
            &ctx,
            "SELECT c, sum(n) + c FROM t GROUP BY c ORDER BY c"
        ),
        vec![
            vec![dec(150, 2), dec(250, 2)],
            vec![dec(225, 2), dec(425, 2)],
            vec![dec(1000, 2), dec(2000, 2)],
        ]
    );
}
