//! `Accumulator::observe_value` のインライン化の復元（Issue #3209・#2761 の是正。SQL-25・SQL-57 の
//! ポインタ。設計記録: `docs/design/aggregate-loop-regression.md`）の結合テスト。
//!
//! 本体を `observe_value_inline` へ分けても、式入力の集計・ウィンドウ集計（`observe_value` の
//! 入口経由）・テナント境界の結果が変わらないことを固定する。期待値は手計算値。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx_of(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn open(label: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(label);
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx_of("alice"),
        &mut session,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, g INTEGER, i INTEGER, s SMALLINT, b BIGINT, \
         r REAL, d DOUBLE PRECISION, n NUMERIC(10,2), t TEXT, f BOOLEAN, dt DATE, \
         ts TIMESTAMP, tz TIMESTAMPTZ, tm TIME, iv INTERVAL, u UUID, y BYTEA)",
    )
    .expect("ddl");
    (core, CleanupGuard(path))
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<String, SqlSurfaceError> {
    let out = core.execute_sql_in_session(&ctx_of(tenant), &mut SessionState::default(), sql)?;
    match out {
        SqlOutcome::Query(r) => Ok(r
            .rows
            .iter()
            .map(|row| format!("{:?}", row.cells))
            .collect::<Vec<_>>()
            .join("|")),
        other => Ok(format!("{other:?}")),
    }
}

/// 3 行（g=1,1,2）。2 行目は全列 NULL（id と g を除く）。
fn seed(core: &EngineCore, tenant: &str, base: u64) {
    let sql = format!(
        "INSERT INTO t (id, g, i, s, b, r, d, n, t, f, dt, ts, tz, tm, iv, u, y) VALUES \
         ({a}, 1, 10, 2, 100, 1.5, 2.5, 3.25, 'b', true, '2024-01-02', '2024-01-02 03:04:05', \
          '2024-01-02 03:04:05+00', '01:02:03', '1 day', \
          '00000000-0000-0000-0000-000000000001', '\\x01'), \
         ({b}, 1, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, \
          NULL, NULL), \
         ({c}, 2, 20, 3, 200, 2.5, 3.5, 4.75, 'a', false, '2024-02-03', '2024-02-03 04:05:06', \
          '2024-02-03 04:05:06+00', '02:03:04', '2 days', \
          '00000000-0000-0000-0000-000000000002', '\\x02')",
        a = base,
        b = base + 1,
        c = base + 2
    );
    core.execute_insert_sql(&ctx_of(tenant), &sql)
        .expect("seed");
}

#[test]
fn sql3209_expression_inputs_match_column_inputs() {
    let (core, _g) = open("sql3209-expr");
    seed(&core, "alice", 1);
    let all = |sql: &str| run(&core, "alice", sql).expect(sql);
    // i = {10, NULL, 20}。式入力は NULL 行を飛ばし、拡幅後の Float で集計される。
    assert_eq!(
        all("SELECT SUM(i + 1), COUNT(i + 1), MIN(i + 1), MAX(i + 1) FROM t"),
        "[Float(32.0), Integer(2), Float(11.0), Float(21.0)]"
    );
    assert_eq!(all("SELECT AVG(d * 2) FROM t"), "[Float(6.0)]");
    // 空集合: COUNT は 0、SUM は NULL。
    let e = all("SELECT COUNT(i + 1), SUM(i + 1) FROM t WHERE id < 0");
    assert!(e.contains("Integer(0)") && e.contains("Null"), "{e}");
}

#[test]
fn sql3209_window_aggregate_through_entry_point() {
    let (core, _g) = open("sql3209-window");
    seed(&core, "alice", 1);
    let w = run(
        &core,
        "alice",
        "SELECT SUM(i + 1) OVER () FROM t ORDER BY id",
    )
    .expect("w");
    assert_eq!(w.matches("Float(32.0)").count(), 3, "{w}");
    let w = run(&core, "alice", "SELECT SUM(i) OVER () FROM t ORDER BY id").expect("w");
    assert!(w.contains("30"), "{w}");
}

#[test]
fn sql3209_wide_integer_sums_widen_without_wrapping() {
    let (core, _g) = open("sql3209-overflow");
    for id in 1..=2u64 {
        core.execute_insert_sql(
            &ctx_of("alice"),
            &format!(
                "INSERT INTO t (id, g, i, b) VALUES ({id}, 1, 2147483647, 9223372036854775807)"
            ),
        )
        .expect("seed");
    }
    assert!(run(&core, "alice", "SELECT SUM(i + 1) FROM t").is_ok());
    // BIGINT 最大値 2 行の SUM は i64 を超えるが、エラーにも wrap にもならず Numeric へ拡幅される。
    assert_eq!(
        run(&core, "alice", "SELECT SUM(b) FROM t").expect("sum b"),
        "[Numeric(Decimal { unscaled: 18446744073709551614, scale: 0 })]"
    );
    assert!(run(&core, "alice", "SELECT SUM(i) FROM t")
        .expect("sum")
        .contains("4294967294"));
}

#[test]
fn sql3209_tenant_boundary_holds_for_expression_and_window() {
    let (core, _g) = open("sql3209-rls");
    seed(&core, "alice", 1);
    seed(&core, "bob", 100);
    let a = run(&core, "alice", "SELECT COUNT(i + 1), SUM(i + 1) FROM t").expect("a");
    assert_eq!(a, "[Integer(2), Float(32.0)]");
    let w = run(&core, "alice", "SELECT SUM(i + 1) OVER () FROM t").expect("w");
    assert_eq!(w.matches("Float(32.0)").count(), 3, "{w}");
    let b = run(&core, "bob", "SELECT MAX(id) FROM t").expect("b");
    assert!(b.contains("102"), "{b}");
    assert!(!a.contains("102"));
}
