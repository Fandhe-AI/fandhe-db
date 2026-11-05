//! DML の `EXPLAIN ANALYZE` の結合テスト（Issue #2055・SQL-76 (2)(3) ポインタ）。
//!
//! (2) 内側の `UPDATE`／`DELETE`／`INSERT` を実際に実行する（autocommit では commit・台帳記録が残り、
//! 自動では巻き戻さない。明示トランザクションでは `ROLLBACK` で巻き戻る）。根の段の `actual_rows` は
//! `RETURNING` の可視行数で、結果の行は返さない。(3) 計測値から他テナントの行量が漏れない。
//! 未対応の形（分割実行・`VERBOSE`・`FORMAT JSON` 等）は従来どおり fail-closed で拒否する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
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

fn seeded(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for sql in [
        "CREATE TABLE a (k BIGINT, v TEXT)",
        "CREATE TABLE b (k BIGINT, v TEXT)",
        "INSERT INTO a (k, v) VALUES (1, 'a1'), (2, 'a2'), (3, 'a3'), (4, 'a4')",
        "INSERT INTO b (k, v) VALUES (7, 'b7')",
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

fn snapshot(core: &EngineCore, tenant: &str, table: &str) -> String {
    match run(core, tenant, &format!("SELECT * FROM {table}")) {
        Ok(o) => format!("{o:?}"),
        Err(c) => c,
    }
}

fn actual_rows(plan: &[String]) -> usize {
    plan.iter()
        .find_map(|l| l.strip_prefix("actual_rows: "))
        .unwrap_or_else(|| panic!("actual_rows missing in {plan:?}"))
        .parse()
        .expect("actual_rows is an integer")
}

const OPTS: &str = "EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF)";

/// 同じ DML を通常に実行した別 DB と、ANALYZE で実行した DB の最終状態が一致する。
/// 出力は `QUERY PLAN` 1 列で、静的な EXPLAIN の根の段へ 2 行（actual_rows／actual_loops）を足した形。
#[test]
fn analyze_executes_like_plain_dml() {
    for (i, body) in [
        "UPDATE a SET v = 'z' WHERE k > 1",
        "DELETE FROM a WHERE k > 1",
        "INSERT INTO a (k, v) VALUES (8, 'x'), (9, 'y')",
        "INSERT INTO a (k, v) SELECT k, v FROM b",
    ]
    .iter()
    .enumerate()
    {
        let (plain, _g1) = seeded(&format!("x76d-plain{i}"));
        let (analyzed, _g2) = seeded(&format!("x76d-an{i}"));
        run(&plain, "tenant-a", body).expect("plain");
        let static_plan = lines(&analyzed, "tenant-a", &format!("EXPLAIN {body}"));
        let plan = lines(&analyzed, "tenant-a", &format!("{OPTS} {body}"));
        assert_eq!(
            snapshot(&plain, "tenant-a", "a"),
            snapshot(&analyzed, "tenant-a", "a"),
            "{body}"
        );
        assert_eq!(plan.len(), static_plan.len() + 2, "{body}");
        assert_eq!(actual_rows(&plan), 0, "{plan:?}");
        assert!(plan.contains(&"actual_loops: 1".to_string()), "{plan:?}");
        // TIMING／SUMMARY の既定は両方有効（別 DB で実行する）。
        let (full_db, _g3) = seeded(&format!("x76d-full{i}"));
        let full = lines(&full_db, "tenant-a", &format!("EXPLAIN ANALYZE {body}"));
        assert!(full.iter().any(|l| l.starts_with("actual_total_time_ms: ")));
        assert!(full.iter().any(|l| l.starts_with("execution_time_ms: ")));
    }
}

/// `RETURNING` の可視行数が `actual_rows` になり、行の値は出力に現れない。
#[test]
fn actual_rows_counts_returning_rows_only() {
    let (core, _g) = seeded("x76d-ret");
    let plan = lines(
        &core,
        "tenant-a",
        &format!("{OPTS} DELETE FROM a WHERE k > 2 RETURNING v"),
    );
    assert_eq!(actual_rows(&plan), 2);
    assert!(!plan.iter().any(|l| l.contains("a3") || l.contains("a4")));
    let plan = lines(
        &core,
        "tenant-a",
        &format!("{OPTS} UPDATE a SET v = 'zz' WHERE k = 1 RETURNING k, v"),
    );
    assert_eq!(actual_rows(&plan), 1);
    assert!(!plan.iter().any(|l| l.contains("zz")));
}

/// `USING OPERATION_ID` の台帳に通常の実行と同じく記録される（静的な EXPLAIN と逆向き）。
#[test]
fn ledger_is_recorded() {
    let (core, _g) = seeded("x76d-ledger");
    lines(
        &core,
        "tenant-a",
        &format!("{OPTS} DELETE FROM a WHERE k = 1 USING OPERATION_ID 'op1'"),
    );
    // 通常の実行で 1 回目を送った別 DB と、2 回目（本文違い）の結果・最終状態が一致する。
    let (plain, _g2) = seeded("x76d-ledger-plain");
    run(
        &plain,
        "tenant-a",
        "DELETE FROM a WHERE k = 1 USING OPERATION_ID 'op1'",
    )
    .expect("plain first");
    let second = "DELETE FROM a WHERE k = 2 USING OPERATION_ID 'op1'";
    let expected = run(&plain, "tenant-a", second).map(|_| ());
    assert!(expected.is_err(), "baseline: id reuse with another body");
    let actual = run(&core, "tenant-a", second).map(|_| ());
    assert_eq!(expected, actual);
    assert_eq!(
        snapshot(&core, "tenant-a", "a"),
        snapshot(&plain, "tenant-a", "a")
    );
}

/// 明示トランザクション: 未 commit の変更が見え、`ROLLBACK` で戻り、`COMMIT` で残る。
#[test]
fn explicit_transaction_rolls_back() {
    let (core, _g) = seeded("x76d-txn");
    let before = snapshot(&core, "tenant-a", "a");
    let c = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    let mut exec = |s: &mut SessionState, sql: &str| {
        core.execute_sql_in_txn(&c, s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    };
    exec(&mut s, "BEGIN");
    exec(&mut s, &format!("{OPTS} DELETE FROM a WHERE k > 1"));
    match exec(&mut s, "SELECT k FROM a") {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 1),
        other => panic!("{other:?}"),
    }
    exec(&mut s, "ROLLBACK");
    assert_eq!(snapshot(&core, "tenant-a", "a"), before);

    let mut txn = core.new_session_transaction();
    let mut exec = |s: &mut SessionState, sql: &str| {
        core.execute_sql_in_txn(&c, s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    };
    exec(&mut s, "BEGIN");
    exec(&mut s, &format!("{OPTS} UPDATE a SET v = 'c' WHERE k = 1"));
    exec(&mut s, "COMMIT");
    assert_ne!(snapshot(&core, "tenant-a", "a"), before);
}

/// READ ONLY のトランザクションは `25006`。行は不変。
#[test]
fn read_only_transaction_is_rejected() {
    let (core, _g) = seeded("x76d-ro");
    let before = snapshot(&core, "tenant-a", "a");
    let c = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = core.new_session_transaction();
    core.execute_sql_in_txn(&c, &mut s, &mut txn, "BEGIN READ ONLY")
        .unwrap();
    let err = core
        .execute_sql_in_txn(&c, &mut s, &mut txn, &format!("{OPTS} DELETE FROM a"))
        .expect_err("read only");
    assert_eq!(err.wire_code(), "25006");
    let _ = core.execute_sql_in_txn(&c, &mut s, &mut txn, "ROLLBACK");
    assert_eq!(snapshot(&core, "tenant-a", "a"), before);
}

/// 実行時エラーは通常の DML と同じ `wire_code` で、計画は返さず、台帳にも残らない。
#[test]
fn runtime_error_leaves_nothing() {
    let (core, _g) = seeded("x76d-err");
    let before = snapshot(&core, "tenant-a", "a");
    let plain = code(
        &core,
        "UPDATE a SET k = 1 / 0 WHERE k > 0 USING OPERATION_ID 'e0'",
    );
    let sql = format!("{OPTS} UPDATE a SET k = 1 / 0 WHERE k > 0 USING OPERATION_ID 'e1'");
    assert_eq!(code(&core, &sql), plain);
    assert_eq!(snapshot(&core, "tenant-a", "a"), before);
    run(
        &core,
        "tenant-a",
        "UPDATE a SET v = 'ok' WHERE k = 1 USING OPERATION_ID 'e1'",
    )
    .expect("id not consumed");
}

/// SQL-76 (3): 他テナントの行量で出力が変わらず、他テナントの行は変更されない。
#[test]
fn tenant_boundary() {
    let (core, _g) = seeded("x76d-tenant");
    let q = format!("{OPTS} DELETE FROM a WHERE k > 100 RETURNING k");
    let base = lines(&core, "tenant-a", &q);
    for k in 101..140 {
        let _ = run(
            &core,
            "tenant-b",
            &format!("INSERT INTO a (k, v) VALUES ({k}, 'b')"),
        );
    }
    assert_eq!(lines(&core, "tenant-a", &q), base);
    let before_b = snapshot(&core, "tenant-b", "a");
    lines(
        &core,
        "tenant-a",
        &format!("{OPTS} DELETE FROM a WHERE k > 0"),
    );
    assert_eq!(snapshot(&core, "tenant-b", "a"), before_b);
}

/// 未対応の形は fail-closed で拒否し、行を変えない。
#[test]
fn unsupported_forms_are_rejected() {
    let (core, _g) = seeded("x76d-reject");
    let before = snapshot(&core, "tenant-a", "a");
    for sql in [
        "EXPLAIN ANALYZE DELETE FROM a WHERE k > 1 USING OPERATION_ID 'p-del' PARTITIONED CHUNK 1",
        "EXPLAIN (ANALYZE, VERBOSE) DELETE FROM a WHERE k > 1",
        "EXPLAIN (ANALYZE, FORMAT JSON) DELETE FROM a WHERE k > 1",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
    for (analyze, plain) in [
        (
            "EXPLAIN ANALYZE DELETE FROM a WHERE k = 1",
            "EXPLAIN DELETE FROM a WHERE k = 1",
        ),
        (
            "EXPLAIN ANALYZE DELETE FROM a WHERE k IN (SELECT k FROM b)",
            "EXPLAIN DELETE FROM a WHERE k IN (SELECT k FROM b)",
        ),
    ] {
        // 静的な EXPLAIN が拒否する形は ANALYZE も同じ code で拒否する（受理される形ならスキップ）。
        if let Err(static_code) = run(&core, "tenant-a", plain) {
            assert_eq!(code(&core, analyze), static_code, "{analyze}");
        }
    }
    assert_eq!(snapshot(&core, "tenant-a", "a"), before);
}

/// セッション無しの入口は 42601。
#[test]
fn sessionless_entry_stays_rejected() {
    let (core, _g) = seeded("x76d-nosess");
    let before = snapshot(&core, "tenant-a", "a");
    let err = core
        .execute_sql(&ctx("tenant-a"), "EXPLAIN ANALYZE DELETE FROM a")
        .expect_err("rejected");
    assert_eq!(err.wire_code(), "42601");
    assert_eq!(snapshot(&core, "tenant-a", "a"), before);
}
