//! 式を含まない集計入力の 1 段観測（Issue #3211・#2684 の是正。SQL-57・SQL-71 のポインタ。
//! 設計記録: `docs/design/aggregate-loop-regression.md`）の結合テスト。
//!
//! 列参照の集計（1 段観測）と、同じ値を式経由（従来の 2 段経路）で集計した結果が一致すること、
//! NULL・空集合・整数 SUM の拡幅・GROUP BY・FILTER・window・RLS 境界で挙動が
//! 変わらないことを固定する。期待値は fixture の規則から独立に導いた手計算値。

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

/// 列参照（1 段観測）の結果型・値を手計算値で固定し、式経由（2 段経路。結果は Float）とも
/// 数値が一致することを確かめる。
#[test]
fn sql3211_column_refs_match_expression_path() {
    let (core, _g) = open("sql3211-pairs");
    seed(&core, "alice", 1);
    let direct = run(&core, "alice", "SELECT SUM(i), SUM(s), SUM(b) FROM t").expect("direct");
    assert_eq!(
        direct,
        "[SignedInteger(30), SignedInteger(5), Numeric(Decimal { unscaled: 300, scale: 0 })]"
    );
    let via = run(
        &core,
        "alice",
        "SELECT SUM(i + 0), SUM(s + 0), SUM(b + 0) FROM t",
    )
    .expect("via");
    assert_eq!(via, "[Float(30.0), Float(5.0), Float(300.0)]");
    let avg = run(&core, "alice", "SELECT AVG(i), MIN(i), MAX(i) FROM t").expect("avg");
    assert_eq!(
        avg,
        "[Numeric(Decimal { unscaled: 150000000000000000, scale: 16 }), SignedInteger(10), SignedInteger(20)]"
    );
    let via = run(
        &core,
        "alice",
        "SELECT AVG(i + 0), MIN(i + 0), MAX(i + 0) FROM t",
    )
    .expect("via");
    assert_eq!(via, "[Float(15.0), Float(10.0), Float(20.0)]");
}

#[test]
fn sql3211_typed_columns_aggregate_to_hand_derived_values() {
    let (core, _g) = open("sql3211-values");
    seed(&core, "alice", 1);
    let all = |sql: &str| run(&core, "alice", sql).expect(sql);
    // COUNT(col) は非 NULL のみ（2 行）、COUNT(*) は 3 行。
    for col in [
        "i", "s", "b", "r", "d", "n", "t", "f", "dt", "ts", "tz", "tm", "iv", "u", "y",
    ] {
        let out = all(&format!("SELECT COUNT({col}), COUNT(*) FROM t"));
        assert!(
            out.contains("Integer(2)") && out.contains("Integer(3)"),
            "{col}: {out}"
        );
    }
    assert!(all("SELECT SUM(i) FROM t").contains("30"));
    assert!(all("SELECT SUM(b) FROM t").contains("300"));
    assert!(all("SELECT MIN(t), MAX(t) FROM t").contains("\"a\""));
    assert!(all("SELECT bool_and(f), bool_or(f) FROM t").contains("Bool(false)"));
    // GROUP BY（g=1 は 1 行だけ非 NULL、g=2 は 1 行）。
    let g = all("SELECT g, COUNT(i), SUM(i) FROM t GROUP BY g ORDER BY g");
    assert!(g.contains("10") && g.contains("20"), "{g}");
    // FILTER。
    let f = all("SELECT COUNT(*) FILTER (WHERE g = 1) FROM t");
    assert!(f.contains("Integer(2)"), "{f}");
    // 空集合: COUNT は 0、それ以外は NULL。
    let e = all("SELECT COUNT(i), SUM(i), AVG(d), MIN(t), MAX(dt) FROM t WHERE id < 0");
    assert!(e.contains("Integer(0)") && e.contains("Null"), "{e}");
    // window。
    let w = all("SELECT SUM(i) OVER () FROM t ORDER BY id");
    assert!(w.contains("30"), "{w}");
}

#[test]
fn sql3211_integer_sums_widen_without_overflow() {
    let (core, _g) = open("sql3211-overflow");
    let sql = |id: u64| {
        format!("INSERT INTO t (id, g, i, b) VALUES ({id}, 1, 2147483647, 9223372036854775807)")
    };
    for id in 1..=2 {
        core.execute_insert_sql(&ctx_of("alice"), &sql(id))
            .expect("seed");
    }
    // INTEGER の SUM は i64 に収まる（従来どおり成功）。
    assert!(run(&core, "alice", "SELECT SUM(i) FROM t")
        .expect("int sum")
        .contains("4294967294"));
    // BIGINT の SUM は numeric 相当で成功する。
    assert!(run(&core, "alice", "SELECT SUM(b) FROM t").is_ok());
}

#[test]
fn sql3211_tenant_boundary_is_unchanged() {
    let (core, _g) = open("sql3211-rls");
    seed(&core, "alice", 1);
    seed(&core, "bob", 100);
    let a = run(
        &core,
        "alice",
        "SELECT COUNT(*), COUNT(i), SUM(i), MAX(id) FROM t",
    )
    .expect("a");
    assert!(
        a.contains("Integer(3)") && a.contains("Integer(2)") && a.contains("30"),
        "{a}"
    );
    let b = run(&core, "bob", "SELECT COUNT(*), MAX(id) FROM t").expect("b");
    assert!(b.contains("Integer(3)") && b.contains("102"), "{b}");
    assert!(!a.contains("102"), "{a}");
}
