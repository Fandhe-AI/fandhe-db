//! SELECT の `EXPLAIN ANALYZE` の結合テスト（Issue #2054・SQL-76 (1)(2)(3) ポインタ）。
//!
//! (1) 構文: 括弧形・前置形（`ANALYZE`／`ANALYSE`）。(2) ANALYZE: 文を実際に実行し、計画の根の段へ
//! 結果の行数を併記する（結果の行は返さない）。(3) 計測値のテナント境界: 併記する行数は可視行に限り、
//! 他テナントの行量で出力が変わらない。DML・`USING PLAN`・一時表・`VERBOSE`・`FORMAT JSON` は `0A000`。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::transaction::{SessionTransaction, TransactionLimits};
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn seeded(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for sql in [
        "CREATE TABLE a (k BIGINT, v TEXT)",
        "CREATE TABLE b (k BIGINT, w TEXT)",
        "INSERT INTO a (k, v) VALUES (1, 'a1'), (2, 'a2'), (3, 'a3'), (4, 'a4')",
        "INSERT INTO b (k, w) VALUES (2, 'b2'), (3, 'b3'), (9, 'b9')",
    ] {
        let mut session = SessionState::default();
        session.allow_ddl();
        core.execute_sql_in_session(&ctx("tenant-a"), &mut session, sql)
            .unwrap_or_else(|e| panic!("setup: {sql:?} {e:?}"));
    }
    (core, guard)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn plan_lines(outcome: SqlOutcome) -> Vec<String> {
    match outcome {
        SqlOutcome::Explain(r) => {
            assert_eq!(r.columns.len(), 1, "QUERY PLAN single column");
            r.rows
                .iter()
                .map(|row| match &row.cells[0] {
                    Cell::Text(s) => s.clone(),
                    other => panic!("expected text, got {other:?}"),
                })
                .collect()
        }
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn lines(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    plan_lines(run(core, tenant, sql).unwrap_or_else(|c| panic!("{sql} failed with {c}")))
}

fn code(core: &EngineCore, sql: &str) -> String {
    match run(core, "tenant-a", sql) {
        Err(c) => c,
        Ok(_) => panic!("{sql} must be rejected"),
    }
}

fn row_count(core: &EngineCore, tenant: &str, sql: &str) -> usize {
    match run(core, tenant, sql).unwrap_or_else(|c| panic!("{sql} failed with {c}")) {
        SqlOutcome::Query(r) => r.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn actual_rows(plan: &[String]) -> usize {
    plan.iter()
        .find_map(|l| l.strip_prefix("actual_rows: "))
        .unwrap_or_else(|| panic!("actual_rows missing in {plan:?}"))
        .parse()
        .expect("actual_rows is an integer")
}

fn snapshot(core: &EngineCore) -> String {
    format!(
        "{:?}",
        run(core, "tenant-a", "SELECT * FROM a").expect("select")
    )
}

const BODIES: &[&str] = &[
    "SELECT k, v FROM a WHERE k > 1",
    "SELECT k, v FROM a",
    "SELECT k, v FROM a WHERE k > 100",
    "SELECT k, v FROM a ORDER BY k LIMIT 2",
    "SELECT k, v FROM a ORDER BY k LIMIT 2 OFFSET 1",
    "SELECT count(*) FROM a",
    "SELECT k, count(*) FROM a GROUP BY k",
    "SELECT x.k FROM a x JOIN b y ON x.k = y.k",
    "SELECT k FROM a UNION SELECT k FROM b",
    "SELECT k FROM a INTERSECT SELECT k FROM b",
    "WITH w AS (SELECT k FROM a) SELECT k FROM w WHERE k > 1",
    "WITH w AS MATERIALIZED (SELECT k FROM a ORDER BY k LIMIT 3) SELECT k FROM w",
    "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 5) SELECT n FROM r",
    "SELECT k FROM (SELECT k FROM a ORDER BY k LIMIT 3) t",
    "SELECT k FROM a WHERE k IN (SELECT k FROM b)",
];

/// SQL-76 (2): 各形で `actual_rows` が同じ SELECT の結果の行数と一致する（括弧形・前置形とも）。
#[test]
fn actual_rows_match_plain_select() {
    let (core, _g) = seeded("x76a-rows");
    for body in BODIES {
        let expected = row_count(&core, "tenant-a", body);
        for head in [
            "EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF)",
            "EXPLAIN ANALYZE",
            "EXPLAIN ANALYSE",
        ] {
            let plan = lines(&core, "tenant-a", &format!("{head} {body}"));
            assert_eq!(actual_rows(&plan), expected, "{head} {body}");
        }
    }
}

/// SQL-76 (2): `TIMING OFF, SUMMARY OFF` の出力は、オプションなしの EXPLAIN に根の段の末尾で 2 行を
/// 挿入した形と一致する。結果の行は返さない。
#[test]
fn output_shape_without_timing_and_summary() {
    let (core, _g) = seeded("x76a-shape");
    for body in BODIES {
        let plain = lines(&core, "tenant-a", &format!("EXPLAIN {body}"));
        let got = lines(
            &core,
            "tenant-a",
            &format!("EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF) {body}"),
        );
        assert_eq!(got.len(), plain.len() + 2, "{body}");
        let at = got
            .iter()
            .position(|l| l.starts_with("actual_rows: "))
            .expect("actual_rows");
        assert_eq!(got[at + 1], "actual_loops: 1", "{body}");
        let mut removed = got.clone();
        removed.drain(at..at + 2);
        assert_eq!(removed, plain, "{body}");
        // 根の段の末尾 = 直前の行は前置なし、直後の行（あれば）は子の段（`-> ` か空白で始まる）。
        if let Some(prev) = at.checked_sub(1).and_then(|i| got.get(i)) {
            assert!(!prev.starts_with("-> ") && !prev.starts_with(' '), "{body}");
        }
        if let Some(next) = got.get(at + 2) {
            assert!(next.starts_with("-> ") || next.starts_with(' '), "{body}");
        }
    }
}

fn parse_ms(line: &str, key: &str) -> f64 {
    let v: f64 = line
        .strip_prefix(key)
        .unwrap_or_else(|| panic!("{line} lacks {key}"))
        .parse()
        .expect("number");
    assert!(v.is_finite() && v >= 0.0, "{line}");
    v
}

/// SQL-76 (2): 既定（TIMING・SUMMARY とも真）は時間の行を出し、OFF で消える。
#[test]
fn timing_and_summary_lines() {
    let (core, _g) = seeded("x76a-time");
    let q = "SELECT k FROM a";
    let full = lines(&core, "tenant-a", &format!("EXPLAIN ANALYZE {q}"));
    let t = full
        .iter()
        .find(|l| l.starts_with("actual_total_time_ms: "))
        .expect("actual_total_time_ms");
    parse_ms(t, "actual_total_time_ms: ");
    let n = full.len();
    parse_ms(&full[n - 2], "planning_time_ms: ");
    parse_ms(&full[n - 1], "execution_time_ms: ");

    let no_timing = lines(
        &core,
        "tenant-a",
        &format!("EXPLAIN (ANALYZE, TIMING OFF) {q}"),
    );
    assert!(!no_timing
        .iter()
        .any(|l| l.starts_with("actual_total_time_ms")));
    assert!(no_timing.iter().any(|l| l.starts_with("planning_time_ms")));

    let no_summary = lines(
        &core,
        "tenant-a",
        &format!("EXPLAIN (ANALYZE, SUMMARY OFF) {q}"),
    );
    assert!(no_summary
        .iter()
        .any(|l| l.starts_with("actual_total_time_ms")));
    assert!(!no_summary
        .iter()
        .any(|l| l.contains("_time_ms") && !l.starts_with("actual_")));
}

/// SQL-76 (2): 文を実際に実行している（静的な EXPLAIN は成功する式エラーが、ANALYZE では普通の SELECT と
/// 同じ 22012 になる）。
#[test]
fn statement_is_actually_executed() {
    let (core, _g) = seeded("x76a-exec");
    let q = "SELECT 1 / (k - 2) FROM a";
    lines(&core, "tenant-a", &format!("EXPLAIN {q}"));
    let plain = code(&core, q);
    assert_eq!(plain, "22012");
    assert_eq!(code(&core, &format!("EXPLAIN ANALYZE {q}")), plain);
}

/// SQL-76 (3): 他テナントの行量で出力が変わらず、`actual_rows` は自テナントの可視行数に等しい。
#[test]
fn tenant_boundary_output_is_independent_of_other_tenants() {
    let (core, _g) = seeded("x76a-tenant");
    let q = "EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF) SELECT k, v FROM a";
    let before = lines(&core, "tenant-a", q);
    let mut s = SessionState::default();
    for i in 0..40 {
        core.execute_sql_in_session(
            &ctx("tenant-b"),
            &mut s,
            &format!("INSERT INTO a (k, v) VALUES ({}, 'b')", 1000 + i),
        )
        .expect("tenant-b insert");
    }
    let after = lines(&core, "tenant-a", q);
    assert_eq!(before, after);
    assert_eq!(
        actual_rows(&after),
        row_count(&core, "tenant-a", "SELECT k, v FROM a")
    );
    assert_eq!(actual_rows(&after), 4);
    assert_eq!(
        actual_rows(&lines(&core, "tenant-b", q)),
        row_count(&core, "tenant-b", "SELECT k, v FROM a")
    );
}

/// SQL-76 (2): 明示トランザクション内は未 commit の変更が見え、ROLLBACK で元に戻る。READ ONLY でも可。
#[test]
fn explicit_transaction() {
    let (core, _g) = seeded("x76a-txn");
    let q = "EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF) SELECT k FROM a";
    let c = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = SessionTransaction::new(TransactionLimits::default());
    let mut exec = |s: &mut SessionState, sql: &str| {
        core.execute_sql_in_txn(&c, s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    };
    exec(&mut s, "BEGIN");
    exec(&mut s, "INSERT INTO a (k, v) VALUES (50, 'x')");
    assert_eq!(actual_rows(&plan_lines(exec(&mut s, q))), 5);
    exec(&mut s, "ROLLBACK");
    assert_eq!(actual_rows(&lines(&core, "tenant-a", q)), 4);

    let mut txn = SessionTransaction::new(TransactionLimits::default());
    let mut exec = |s: &mut SessionState, sql: &str| {
        core.execute_sql_in_txn(&c, s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    };
    exec(&mut s, "BEGIN READ ONLY");
    assert_eq!(actual_rows(&plan_lines(exec(&mut s, q))), 4);
    exec(&mut s, "COMMIT");
}

/// SQL-76 (2): 対応しない形は固定のエラーで拒否し、行を変えない。
#[test]
fn unsupported_forms_are_rejected() {
    let (core, _g) = seeded("x76a-reject");
    let before = snapshot(&core);
    for sql in [
        "EXPLAIN ANALYZE DELETE FROM a WHERE k > 1",
        "EXPLAIN ANALYZE UPDATE a SET v = 'z' WHERE k > 1",
        "EXPLAIN ANALYZE INSERT INTO a (k, v) VALUES (9, 'z')",
        "EXPLAIN ANALYZE DELETE FROM a WHERE k > 1 USING OPERATION_ID 'p-del' PARTITIONED CHUNK 1",
        "EXPLAIN ANALYZE VERBOSE SELECT k FROM a",
        "EXPLAIN (ANALYZE, VERBOSE) SELECT k FROM a",
        "EXPLAIN (ANALYZE, FORMAT JSON) SELECT k FROM a",
        "EXPLAIN (SUMMARY) SELECT k FROM a",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
    assert_eq!(code(&core, "EXPLAIN (TIMING) SELECT k FROM a"), "22023");
    assert_eq!(
        code(&core, "EXPLAIN ANALYZE (COSTS) SELECT k FROM a"),
        "42601"
    );
    assert_eq!(code(&core, "EXPLAIN ANALYZE"), "42601");
    assert_eq!(
        code(&core, "EXPLAIN ANALYZE SELECT k FROM missing"),
        "42P01"
    );
    assert_eq!(snapshot(&core), before);
}

/// SQL-76 (1): セッション無しの入口は従来どおり 42601。
#[test]
fn sessionless_entry_stays_rejected() {
    let (core, _g) = seeded("x76a-nosess");
    let err = core
        .execute_sql(&ctx("tenant-a"), "EXPLAIN ANALYZE SELECT k FROM a")
        .expect_err("rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// SQL-76 (2): 一時表を参照する文は 0A000。
#[test]
fn temp_table_is_rejected() {
    let (core, _g) = seeded("x76a-temp");
    let mut s = SessionState::default();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut s, "CREATE TEMP TABLE t (k BIGINT)")
        .expect("create temp");
    assert_eq!(
        core.execute_sql_in_session(&ctx("tenant-a"), &mut s, "EXPLAIN ANALYZE SELECT k FROM t")
            .unwrap_err()
            .wire_code(),
        "0A000"
    );
}

/// SQL-76 (2): 拡張クエリの Parse は `$n` を含む `EXPLAIN ANALYZE` を 42601 で拒否する。
#[test]
fn prepared_parse_rejects_parameters() {
    let (core, _g) = seeded("x76a-param");
    let err = core
        .parse_sql_prepared("EXPLAIN ANALYZE SELECT k FROM a WHERE k = $1")
        .map(|_| ())
        .expect_err("rejected");
    assert_eq!(err.wire_code(), "42601");
}
