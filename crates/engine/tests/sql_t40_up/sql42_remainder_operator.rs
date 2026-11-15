//! `%`（剰余）演算子の構文・型付き評価（SQL-42 のポインタのみ。Issue #3273、親 #3272）の結合テスト。
//! 実 `Storage`＋`CpuScalarProvider` を `EngineCore::execute_sql_in_session` 経由で使い、
//! FROM なし SELECT・列投影・集計投影の値と結果型（PostgreSQL 17 準拠）、0 除算（22012）・
//! float 型被演算子／非対応型（42883）・構文不完全形（42601）の fail-closed、テナント境界を固定する。
//! WHERE 等の各文脈の網羅は兄弟 Issue #3274 の担当で、ここでは WHERE のスモークだけを置く。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
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

/// `id` 昇順で先頭列のセルを集める。
fn cells0(r: &QueryResult) -> Vec<Cell> {
    let mut rows: Vec<_> = r.rows.iter().collect();
    rows.sort_by_key(|row| row.id);
    rows.iter().map(|row| row.cells[0].clone()).collect()
}

fn ints(r: &QueryResult) -> Vec<i64> {
    cells0(r)
        .into_iter()
        .map(|c| match c {
            Cell::SignedInteger(i) => i,
            other => panic!("expected SignedInteger, got {other:?}"),
        })
        .collect()
}

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

fn setup() -> (EngineCore, PolicyContext, CleanupGuard) {
    let path = unique_db_path("sql42-remainder");
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, false),
                ColumnDef::new("b", ColumnType::BigInt, false),
                ColumnDef::new("w", ColumnType::SmallInt, false),
                ColumnDef::new("r", ColumnType::Real, false),
                ColumnDef::new("d", ColumnType::Double, false),
                ColumnDef::new("s", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = ctx_for("t1");
    // (id, n, b, w)
    for (id, n, b, w) in [
        (1u64, 1i64, 4_000_000_000i64, 7i64),
        (2, 7, 3, -7),
        (3, -7, -3, 5),
    ] {
        run_opt(
            &core,
            &ctx,
            &format!(
                "INSERT INTO t (id, embedding, n, b, w, r, d, s) VALUES \
                 ({id}, '[0.{id},0.1]', {n}, {b}, {w}, 1.5, 2.5, 'x') \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        )
        .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    }
    // 他テナントの行（`%` の結果に現れてはならない）。
    run_opt(
        &core,
        &ctx_for("t2"),
        "INSERT INTO t (id, embedding, n, b, w, r, d, s) VALUES \
         (9, '[0.9,0.1]', 100, 100, 100, 1.0, 1.0, 'y') USING OPERATION_ID 'seed-other'",
    )
    .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    (core, ctx, CleanupGuard(path))
}

#[test]
fn from_less_remainder_follows_postgres_signs_and_types() {
    let (core, ctx, _g) = setup();
    // 符号は被除数に従う（PostgreSQL 17: 7 % 3 = 1、-7 % 3 = -1、7 % -3 = 1）。
    for (sql, expected) in [
        ("SELECT 7 % 3", 1),
        ("SELECT -7 % 3", -1),
        ("SELECT 7 % -3", 1),
        ("SELECT 6 % 3", 0),
        ("SELECT -6 % 3", 0),
        ("SELECT 7 % 3;", 1),
        ("SELECT 2 + 7 % 4 * 2", 8),
        ("SELECT 10 % 4 % 3", 2),
    ] {
        let r = run(&core, &ctx, sql);
        assert_eq!(ints(&r), vec![expected], "{sql}");
        assert_eq!(first_ty(&r), Some(ColumnType::Integer), "{sql}");
        assert_eq!(first_name(&r), "?column?", "{sql}");
    }
    // NULL 伝播。
    let r = run(&core, &ctx, "SELECT CAST(NULL AS integer) % 2");
    assert_eq!(cells0(&r), vec![Cell::Null]);
}

#[test]
fn column_projection_uses_integer_result_types() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT n % 4 FROM t LIMIT 10");
    assert_eq!(ints(&r), vec![1, 3, -3]);
    assert_eq!(first_ty(&r), Some(ColumnType::Integer));
    // 4000000000 % 7 = 3。bigint どうしは bigint。
    let r = run(&core, &ctx, "SELECT b % 7 FROM t LIMIT 10");
    assert_eq!(ints(&r), vec![3, 3, -3]);
    assert_eq!(first_ty(&r), Some(ColumnType::BigInt));
    // int4 % int8 は int8。
    let r = run(&core, &ctx, "SELECT n % b FROM t LIMIT 10");
    assert_eq!(ints(&r), vec![1, 1, -1]);
    assert_eq!(first_ty(&r), Some(ColumnType::BigInt));
    // smallint % smallint は smallint。
    let r = run(&core, &ctx, "SELECT w % w FROM t LIMIT 10");
    assert_eq!(ints(&r), vec![0, 0, 0]);
    assert_eq!(first_ty(&r), Some(ColumnType::SmallInt));
    // 優先度: `%` は `+` より強い。
    let r = run(&core, &ctx, "SELECT n + n % 3 FROM t LIMIT 10");
    assert_eq!(ints(&r), vec![2, 8, -8]);
    let r = run(&core, &ctx, "SELECT (n + n) % 3 FROM t LIMIT 10");
    assert_eq!(ints(&r), vec![2, 2, -2]);
}

#[test]
fn aggregate_projection_uses_numeric_remainder() {
    let (core, ctx, _g) = setup();
    // sum(n) = 1、avg(n) = 1/3（scale 16）。他テナントの 100 は混入しない。
    let r = run(&core, &ctx, "SELECT sum(n) % 4 FROM t");
    assert_eq!(cells0(&r), vec![Cell::SignedInteger(1)]);
    let r = run(&core, &ctx, "SELECT avg(n) % 1 FROM t");
    match &cells0(&r)[0] {
        Cell::Numeric(d) => assert_eq!(d.to_string(), "0.33333333333333333333"),
        other => panic!("expected Numeric, got {other:?}"),
    }
    assert!(matches!(first_ty(&r), Some(ColumnType::Numeric { .. })));
}

#[test]
fn where_smoke_returns_expected_rows_within_tenant() {
    let (core, ctx, _g) = setup();
    // n = 1, 7, -7（PostgreSQL では -7 % 2 = -1 なので `= 1` は 2 行）。
    let r = run(&core, &ctx, "SELECT id FROM t WHERE n % 2 = 1 LIMIT 10");
    let mut ids: Vec<_> = r.rows.iter().map(|row| row.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
    // 他テナントの行（n = 100）は偶数でも現れない。
    let r = run(&core, &ctx, "SELECT id FROM t WHERE n % 2 = 0 LIMIT 10");
    assert!(r.rows.is_empty());
}

#[test]
fn division_by_zero_is_22012() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT 1 % 0",
        "SELECT n % 0 FROM t LIMIT 10",
        "SELECT b % (n - n) FROM t LIMIT 10",
        "SELECT avg(n) % 0 FROM t",
    ] {
        assert_eq!(code(&core, &ctx, sql), "22012", "{sql}");
    }
}

#[test]
fn unsupported_operand_types_are_42883() {
    let (core, ctx, _g) = setup();
    for sql in [
        // PostgreSQL 17 の `%` は整数・numeric のみ（real／double precision は未定義）。
        "SELECT r % 2 FROM t LIMIT 10",
        "SELECT d % 2 FROM t LIMIT 10",
        "SELECT 2 % d FROM t LIMIT 10",
        "SELECT 1.5::float8 % 2",
        "SELECT n + r % 2 FROM t LIMIT 10",
        "SELECT s % 2 FROM t LIMIT 10",
        "SELECT embedding % 2 FROM t LIMIT 10",
        // CASE／COALESCE／NULLIF で包んだ float8 も、直接の float8 と同じく 42883。
        "SELECT COALESCE(7::float8, 0::float8) % 2",
        "SELECT NULLIF(7::float8, 0::float8) % 2",
        "SELECT CASE WHEN 1 = 1 THEN 7::float8 ELSE 0::float8 END % 2",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42883", "{sql}");
    }
}

/// 小数リテラルの剰余は f64 の fmod（`0.3 % 0.1` が約 0.1）へ流さず fail-closed で拒否する（0A000）。
#[test]
fn decimal_literal_remainder_is_rejected_not_approximated() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT 0.3 % 0.1",
        "SELECT 7.5 % 2",
        "SELECT n FROM t WHERE 0.3 % 0.1 = 0",
    ] {
        assert_eq!(code(&core, &ctx, sql), "0A000", "{sql}");
    }
}

#[test]
fn dangling_percent_is_a_syntax_error() {
    let (core, ctx, _g) = setup();
    for sql in [
        "SELECT n % FROM t LIMIT 10",
        "SELECT % 2 FROM t LIMIT 10",
        "SELECT n # 2 FROM t LIMIT 10",
    ] {
        assert_eq!(code(&core, &ctx, sql), "42601", "{sql}");
    }
}

#[test]
fn tenant_boundary_holds() {
    let (core, ctx, _g) = setup();
    let r = run(&core, &ctx, "SELECT n % 1000 FROM t LIMIT 10");
    assert_eq!(r.rows.len(), 3);
    assert!(ints(&r).iter().all(|v| v.abs() < 100));
}

/// f64 レーン（WHERE）で `b * b` が 2^53 を超える丸めた中間値を剰余へ流さない（22003 で拒否）。
#[test]
fn where_remainder_rejects_inexact_intermediate() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        code(&core, &ctx, "SELECT id FROM t WHERE (b * b) % 2 = 0"),
        "22003"
    );
    // 最終値は小さくても中間の `b * b + 1` が丸められる式は誤一致させず 22003 で拒否する。
    assert_eq!(
        code(
            &core,
            &ctx,
            "SELECT id FROM t WHERE ((b * b + 1) - (b * b)) % 2 = 0"
        ),
        "22003"
    );
    // 2^53 未満の厳密な中間値は従来どおり評価する。
    let r = run(&core, &ctx, "SELECT id FROM t WHERE (n * n) % 2 = 1");
    assert_eq!(r.rows.len(), 3);
}

/// 整数 CAST の内部の f64 算術も精度検査（22003）の対象にし、丸めによる誤一致を許さない。
#[test]
fn where_remainder_hardens_integer_cast_interior() {
    let (core, ctx, _g) = setup();
    assert_eq!(
        code(&core, &ctx, "SELECT CAST(b * b AS bigint) % 2 FROM t"),
        "22003"
    );
}

/// |v| > 2^53 の BIGINT 値を含む被演算子は、不変条件違反（XX000）でなく 22003 にする。
#[test]
fn where_remainder_large_bigint_operand_is_22003() {
    let (core, ctx, _g) = setup();
    run_opt(
        &core,
        &ctx,
        "INSERT INTO t (id, embedding, n, b, w, r, d, s) VALUES \
         (4, '[0.4,0.1]', 1, 1152921504606846976, 1, 1.5, 2.5, 'x') USING OPERATION_ID 'seed-big'",
    )
    .unwrap_or_else(|e| panic!("insert should succeed: {e:?}"));
    assert_eq!(
        code(&core, &ctx, "SELECT id FROM t WHERE (b + 1) % 2 = 0"),
        "22003"
    );
}

/// 型付き UDF の引数への `%` は、定義時・呼び出し時とも整数引数なら受理する。
#[test]
fn typed_udf_param_remainder_is_accepted() {
    let (core, ctx, _g) = setup();
    let mut s = SessionState::default();
    s.allow_ddl();
    for sql in [
        "CREATE FUNCTION odd(x integer) RETURNS integer AS x % 2",
        "CREATE FUNCTION odd8(x bigint) RETURNS bigint AS x % 2",
    ] {
        core.execute_sql_in_session(&ctx, &mut s, sql)
            .unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"));
    }
    for (sql, expected) in [("SELECT odd(7)", 1), ("SELECT odd8(8)", 0)] {
        match core
            .execute_sql_in_session(&ctx, &mut s, sql)
            .unwrap_or_else(|e| panic!("{sql:?} should succeed: {e:?}"))
        {
            SqlOutcome::Query(r) => assert_eq!(ints(&r), vec![expected], "{sql}"),
            other => panic!("expected Query, got {other:?}"),
        }
    }
}
