//! 投影項目のトップレベル中置式（`+ - * /`・括弧・単項マイナス）の受理
//! （SQL-42。`docs/spec/04-behavior/` のポインタのみ。Issue #1538、親 #1537）の結合テスト。
//! 実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql_in_session` 経由で使い、
//! 式投影の値、既存投影の不変、型不一致・0 除算・深さ／ノード超過の拒否、
//! 非対応形の fail-closed、テナント境界を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
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

/// `id` 昇順で `idx` 番目のセルを f64 として集める。
fn floats(r: &QueryResult, idx: usize) -> Vec<f64> {
    let mut rows: Vec<_> = r.rows.iter().collect();
    rows.sort_by_key(|row| row.id);
    rows.iter()
        .map(|row| match &row.cells[idx] {
            Cell::Float(f) => *f,
            other => panic!("expected Float, got {other:?}"),
        })
        .collect()
}

/// `id` 昇順で `idx` 番目のセルを整数として集める（Issue #1539: 整数算術の結果は
/// `Cell::SignedInteger`）。
fn ints(r: &QueryResult, idx: usize) -> Vec<i64> {
    let mut rows: Vec<_> = r.rows.iter().collect();
    rows.sort_by_key(|row| row.id);
    rows.iter()
        .map(|row| match &row.cells[idx] {
            Cell::SignedInteger(i) => *i,
            other => panic!("expected SignedInteger, got {other:?}"),
        })
        .collect()
}

fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path("sql42-projection-infix");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("t1");
    for (id, n) in [(1u64, 1i64), (2, 2), (3, 3)] {
        run_opt(
            &core,
            &ctx,
            &format!(
                "INSERT INTO t (id, embedding, n) VALUES ({id}, '[0.{id},0.1]', {n}) \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        )
        .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    }
    // 他テナントの行（式投影の結果に現れてはならない）。
    run_opt(
        &core,
        &ctx_for("t2"),
        "INSERT INTO t (id, embedding, n) VALUES (9, '[0.9,0.1]', 100) \
         USING OPERATION_ID 'seed-other'",
    )
    .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    (core, ctx, CleanupGuard(path))
}

#[test]
fn accepts_top_level_arithmetic() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT n+1 AS m FROM t LIMIT 10");
    assert_eq!(ints(&r, 0), vec![2, 3, 4]);
    let r = run(&core, &ctx, "SELECT (n+1)*2 AS m FROM t LIMIT 10");
    assert_eq!(ints(&r, 0), vec![4, 6, 8]);
    let r = run(&core, &ctx, "SELECT -n AS m FROM t LIMIT 10");
    assert_eq!(ints(&r, 0), vec![-1, -2, -3]);
    let r = run(&core, &ctx, "SELECT -n*2 AS m FROM t LIMIT 10");
    assert_eq!(ints(&r, 0), vec![-2, -4, -6]);
    let r = run(&core, &ctx, "SELECT abs(n)+1 AS m FROM t LIMIT 10");
    assert_eq!(floats(&r, 0), vec![2.0, 3.0, 4.0]);
    let r = run(&core, &ctx, "SELECT id * 2 AS m FROM t LIMIT 10");
    assert_eq!(floats(&r, 0), vec![2.0, 4.0, 6.0]);
    let r = run(&core, &ctx, "SELECT t.n + 1 AS m FROM t LIMIT 10");
    assert_eq!(ints(&r, 0), vec![2, 3, 4]);
    let r = run(&core, &ctx, "SELECT id, n+1 AS m FROM t LIMIT 10");
    assert_eq!(ints(&r, 1), vec![2, 3, 4]);
}

#[test]
fn accepts_in_search_select() {
    let (core, ctx, _g) = setup();
    let r = run(
        &core,
        &ctx,
        "SELECT n+1 AS m FROM t ORDER BY embedding <=> '[0.1,0.1]' LIMIT 10",
    );
    let mut v = ints(&r, 0);
    v.sort_unstable();
    assert_eq!(v, vec![2, 3, 4]);
}

#[test]
fn existing_projections_unchanged() {
    let (core, ctx, _g) = setup();
    assert_eq!(run(&core, &ctx, "SELECT * FROM t LIMIT 10").rows.len(), 3);
    assert_eq!(run(&core, &ctx, "SELECT t.* FROM t LIMIT 10").rows.len(), 3);
    let r = run(&core, &ctx, "SELECT n FROM t LIMIT 10");
    assert_eq!(r.columns.len(), 1);
    assert_eq!(r.rows.len(), 3);
    let r = run(&core, &ctx, "SELECT (n) FROM t LIMIT 10");
    assert_eq!(r.columns.len(), 1);
    assert_eq!(r.rows.len(), 3);
}

#[test]
fn rejects_runtime_and_type_errors() {
    let (core, ctx, _g) = setup();
    assert_eq!(code(&core, &ctx, "SELECT n / 0 FROM t LIMIT 10"), "22012");
    assert_eq!(
        code(&core, &ctx, "SELECT -embedding FROM t LIMIT 10"),
        "42804"
    );
    let deep = format!(
        "SELECT {}n{} FROM t LIMIT 10",
        "(".repeat(40),
        ")".repeat(40)
    );
    assert_eq!(code(&core, &ctx, &deep), "54000");
    let wide = format!("SELECT n{} FROM t LIMIT 10", "+n".repeat(1100));
    assert_eq!(code(&core, &ctx, &wide), "54000");
}

#[test]
fn unsupported_forms_stay_rejected() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT n % 2 FROM t LIMIT 10",
        "SELECT 'a' + n FROM t LIMIT 10",
        "SELECT n + FROM t LIMIT 10",
        "SELECT * FROM t WHERE n = -n LIMIT 10",
        "CREATE VIEW v AS SELECT n+1 FROM t",
        "SELECT n+1 FROM t UNION SELECT n FROM t",
        "SELECT n+1 FROM t GROUP BY n",
        "SELECT n + sum(n) FROM t",
    ] {
        assert!(
            run_opt(&core, &ctx, sql).is_err(),
            "{sql:?} must be rejected"
        );
    }
    assert_eq!(
        code(&core, &ctx, "SELECT * FROM t WHERE n = -n LIMIT 10"),
        "42601"
    );
}

#[test]
fn tenant_boundary_holds() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT n+1 AS m FROM t LIMIT 10");
    assert!(ints(&r, 0).iter().all(|v| *v < 100));
    assert_eq!(r.rows.len(), 3);
}
