//! 投影式の結果型推論と列名（SQL-42。`docs/spec/04-behavior/` のポインタのみ。
//! Issue #1539、親 #1537）の結合テスト。
//! 実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql_in_session` 経由で使い、
//! 整数／単精度／倍精度の型規則、列名 `?column?`、NULL 伝播、`22003`／`22012`／`42883` の
//! fail-closed、Describe と実行時の列メタの一致、WHERE 意味論の不変、テナント境界を固定する。

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

/// 先頭列の型（`Computed` のみ）。
fn first_ty(r: &QueryResult) -> Option<ColumnType> {
    match &r.columns[0] {
        ColumnMeta::Computed { ty, .. } => ty.clone(),
        other => panic!("expected Computed, got {other:?}"),
    }
}

fn first_name(r: &QueryResult) -> String {
    match &r.columns[0] {
        ColumnMeta::Computed { name, .. } => name.clone(),
        other => panic!("expected Computed, got {other:?}"),
    }
}

/// `id` 昇順の先頭列セル。
fn cells0(r: &QueryResult) -> Vec<Cell> {
    let mut rows: Vec<_> = r.rows.iter().collect();
    rows.sort_by_key(|row| row.id);
    rows.iter().map(|row| row.cells[0].clone()).collect()
}

fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path("sql42-result-types");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, false),
                ColumnDef::new("m", ColumnType::Integer, true),
                ColumnDef::new("b", ColumnType::BigInt, false),
                ColumnDef::new("r", ColumnType::Real, false),
                ColumnDef::new("d", ColumnType::Double, false),
                ColumnDef::new("s", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("t1");
    // (id, n, m, b, r, d)
    let rows: [(u64, i64, &str, i64, &str, &str); 3] = [
        (1, 1, "10", 4_000_000_000, "1.5", "0.5"),
        (2, 7, "NULL", 3, "2.5", "1.5"),
        (3, -7, "5", -3, "0.25", "2.5"),
    ];
    for (id, n, m, b, r, d) in rows {
        run_opt(
            &core,
            &ctx,
            &format!(
                "INSERT INTO t (id, embedding, n, m, b, r, d, s) VALUES \
                 ({id}, '[0.{id},0.1]', {n}, {m}, {b}, {r}, {d}, 'x') \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        )
        .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    }
    // 他テナントの行（式投影の結果に現れてはならない）。
    run_opt(
        &core,
        &ctx_for("t2"),
        "INSERT INTO t (id, embedding, n, m, b, r, d, s) VALUES \
         (9, '[0.9,0.1]', 100, 100, 100, 1.0, 1.0, 'y') USING OPERATION_ID 'seed-other'",
    )
    .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    (core, ctx, CleanupGuard(path))
}

fn assert_type(core: &EngineCore, ctx: &PolicyContext, sql: &str, ty: ColumnType) {
    let r = run(core, ctx, sql);
    assert_eq!(first_ty(&r), Some(ty), "{sql}");
}

#[test]
fn integer_arithmetic_yields_int4_and_int8() {
    let (core, ctx, _g) = setup();
    assert_type(
        &core,
        &ctx,
        "SELECT n+1 FROM t LIMIT 10",
        ColumnType::Integer,
    );
    assert_type(
        &core,
        &ctx,
        "SELECT n+b FROM t LIMIT 10",
        ColumnType::BigInt,
    );
    assert_type(
        &core,
        &ctx,
        "SELECT b*2 FROM t LIMIT 10",
        ColumnType::BigInt,
    );
    assert_type(
        &core,
        &ctx,
        "SELECT (n+1)*2 FROM t LIMIT 10",
        ColumnType::Integer,
    );
    assert_type(
        &core,
        &ctx,
        "SELECT -n FROM t LIMIT 10",
        ColumnType::Integer,
    );
    assert_type(
        &core,
        &ctx,
        "SELECT n+3000000000 FROM t LIMIT 10",
        ColumnType::BigInt,
    );
    let r = run(&core, &ctx, "SELECT n+1 FROM t LIMIT 10");
    assert_eq!(
        cells0(&r),
        vec![
            Cell::SignedInteger(2),
            Cell::SignedInteger(8),
            Cell::SignedInteger(-6)
        ]
    );
    let r = run(&core, &ctx, "SELECT n+b FROM t LIMIT 10");
    assert_eq!(cells0(&r)[0], Cell::SignedInteger(4_000_000_001));
}

#[test]
fn float_arithmetic_keeps_float_types() {
    let (core, ctx, _g) = setup();
    assert_type(&core, &ctx, "SELECT r+r FROM t LIMIT 10", ColumnType::Real);
    assert_type(
        &core,
        &ctx,
        "SELECT r*2 FROM t LIMIT 10",
        ColumnType::Double,
    );
    assert_type(
        &core,
        &ctx,
        "SELECT n+d FROM t LIMIT 10",
        ColumnType::Double,
    );
    assert_type(
        &core,
        &ctx,
        "SELECT n*1.5 FROM t LIMIT 10",
        ColumnType::Double,
    );
    assert_type(
        &core,
        &ctx,
        "SELECT abs(n)+1 FROM t LIMIT 10",
        ColumnType::Double,
    );
    assert_type(
        &core,
        &ctx,
        "SELECT id*2 FROM t LIMIT 10",
        ColumnType::Double,
    );
    let r = run(&core, &ctx, "SELECT r+r FROM t LIMIT 10");
    assert_eq!(
        cells0(&r),
        vec![Cell::Float(3.0), Cell::Float(5.0), Cell::Float(0.5)]
    );
}

#[test]
fn integer_literal_projection_follows_literal_size() {
    let (core, ctx, _g) = setup();
    assert_type(&core, &ctx, "SELECT 1 FROM t LIMIT 1", ColumnType::Integer);
    assert_type(
        &core,
        &ctx,
        "SELECT 3000000000 FROM t LIMIT 1",
        ColumnType::BigInt,
    );
}

#[test]
fn integer_division_truncates_toward_zero_per_node() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT n/2 FROM t LIMIT 10");
    assert_eq!(
        cells0(&r),
        vec![
            Cell::SignedInteger(0),
            Cell::SignedInteger(3),
            Cell::SignedInteger(-3)
        ]
    );
    // PostgreSQL と同じく (7/2)*2 = 6。
    let r = run(&core, &ctx, "SELECT (n/2)*2 FROM t LIMIT 10");
    assert_eq!(
        cells0(&r),
        vec![
            Cell::SignedInteger(0),
            Cell::SignedInteger(6),
            Cell::SignedInteger(-6)
        ]
    );
}

#[test]
fn anonymous_expression_column_is_question_column() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        first_name(&run(&core, &ctx, "SELECT n+1 FROM t LIMIT 1")),
        "?column?"
    );
    assert_eq!(
        first_name(&run(&core, &ctx, "SELECT n+1 AS k FROM t LIMIT 1")),
        "k"
    );
    assert_eq!(
        first_name(&run(&core, &ctx, "SELECT abs(n) FROM t LIMIT 1")),
        "abs"
    );
}

#[test]
fn null_propagates_without_changing_column_type() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT m+1 FROM t LIMIT 10");
    assert_eq!(first_ty(&r), Some(ColumnType::Integer));
    assert_eq!(
        cells0(&r),
        vec![Cell::SignedInteger(11), Cell::Null, Cell::SignedInteger(6)]
    );
}

#[test]
fn fails_closed_on_overflow_division_and_text_operands() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        code(&core, &ctx, "SELECT n+2147483647 FROM t LIMIT 10"),
        "22003"
    );
    assert_eq!(
        code(&core, &ctx, "SELECT n*2147483647*2 FROM t LIMIT 10"),
        "22003"
    );
    assert_eq!(code(&core, &ctx, "SELECT n/0 FROM t LIMIT 10"), "22012");
    assert_eq!(code(&core, &ctx, "SELECT s+1 FROM t LIMIT 10"), "42883");
    assert_eq!(code(&core, &ctx, "SELECT 1*s FROM t LIMIT 10"), "42883");
    assert_eq!(code(&core, &ctx, "SELECT s/s FROM t LIMIT 10"), "42883");
    assert_eq!(
        code(&core, &ctx, "SELECT -embedding FROM t LIMIT 10"),
        "42804"
    );
    // 他テナントの値・列名を含まない固定文言。
    let err = run_opt(&core, &ctx, "SELECT s+1 FROM t LIMIT 10").unwrap_err();
    let text = format!("{err:?}");
    assert!(!text.contains("100"), "{text}");
}

#[test]
fn describe_matches_execution_for_expression_projections() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT n+1 FROM t LIMIT 10",
        "SELECT n+b FROM t LIMIT 10",
        "SELECT r+r FROM t LIMIT 10",
        "SELECT n*d AS k FROM t LIMIT 10",
        "SELECT 1 FROM t LIMIT 10",
        "SELECT n+1 FROM t ORDER BY embedding <=> '[0.1,0.1]' LIMIT 10",
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
fn where_keeps_float_semantics_and_tenant_boundary_holds() {
    let (core, ctx, _g) = setup();
    // WHERE の算術は従来の f64 意味論のまま（n/2 は 3.5 になるため 7/2 > 3 が真）。
    let r = run(&core, &ctx, "SELECT n FROM t WHERE n/2 > 3 LIMIT 10");
    assert_eq!(r.rows.len(), 1);
    let r = run(&core, &ctx, "SELECT n+1 FROM t LIMIT 10");
    assert_eq!(r.rows.len(), 3);
    assert!(cells0(&r)
        .iter()
        .all(|c| matches!(c, Cell::SignedInteger(v) if *v < 100)));
}
