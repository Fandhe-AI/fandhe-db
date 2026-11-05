//! `EXPLAIN ( <オプション> ) <文>` の括弧付きオプション構文の結合テスト（Issue #2058・SQL-76 (1)
//! ポインタ）。
//!
//! 受理するオプション（`COSTS`・`FORMAT TEXT`・明示的な偽）はオプションなしの `EXPLAIN` と出力が
//! 完全に一致し、未実装（`ANALYZE`・`VERBOSE`・`SUMMARY`・`FORMAT JSON`）と対象外は `0A000`、
//! 未知の名前は `42601`、値の不正は `22023`。後の指定が勝つ。セッション無しの入口は従来どおり `42601`。

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
        "INSERT INTO a (k, v) VALUES (1, 'a1'), (2, 'a2'), (3, 'a3')",
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

fn lines(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    match run(core, tenant, sql).unwrap_or_else(|c| panic!("{sql} failed with {c}")) {
        SqlOutcome::Explain(r) => r
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Text(s) => s.clone(),
                other => panic!("expected text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn code(core: &EngineCore, sql: &str) -> String {
    match run(core, "tenant-a", sql) {
        Err(c) => c,
        Ok(_) => panic!("{sql} must be rejected"),
    }
}

fn snapshot(core: &EngineCore) -> String {
    format!(
        "{:?}",
        run(core, "tenant-a", "SELECT * FROM a").expect("select")
    )
}

/// SQL-76 (1): 受理するオプションはオプションなしの EXPLAIN と出力の行が完全に一致する。
#[test]
fn accepted_options_match_plain_explain() {
    let (core, _g) = seeded("x76-ok");
    let bodies = [
        "SELECT k FROM a WHERE k = 2",
        "SELECT count(*) FROM a",
        "SELECT x.k FROM a x JOIN a y ON x.k = y.k",
        "SELECT k FROM a UNION SELECT k FROM a",
        "WITH c AS (SELECT k FROM a) SELECT k FROM c",
        "UPDATE a SET v = 'z' WHERE k > 1",
        "DELETE FROM a WHERE k > 1",
    ];
    let before = snapshot(&core);
    for body in bodies {
        let plain = lines(&core, "tenant-a", &format!("EXPLAIN {body}"));
        for opts in [
            "COSTS",
            "COSTS OFF",
            "costs on",
            "FORMAT TEXT",
            "ANALYZE FALSE, VERBOSE OFF, TIMING 0, SUMMARY false",
        ] {
            let got = lines(&core, "tenant-a", &format!("EXPLAIN ({opts}) {body}"));
            assert_eq!(got, plain, "{opts} / {body}");
        }
    }
    assert_eq!(snapshot(&core), before, "DML EXPLAIN must not execute");
}

/// SQL-76 (1): 同じオプションの繰り返しは後の指定が勝つ。
#[test]
fn later_option_wins() {
    let (core, _g) = seeded("x76-last");
    let q = "SELECT k FROM a";
    lines(
        &core,
        "tenant-a",
        &format!("EXPLAIN (ANALYZE, ANALYZE OFF) {q}"),
    );
    lines(
        &core,
        "tenant-a",
        &format!("EXPLAIN (FORMAT JSON, FORMAT TEXT) {q}"),
    );
    lines(
        &core,
        "tenant-a",
        &format!("EXPLAIN (TIMING, TIMING OFF) {q}"),
    );
    assert_eq!(
        code(&core, &format!("EXPLAIN (ANALYZE OFF, ANALYZE) {q}")),
        "0A000"
    );
    assert_eq!(
        code(&core, &format!("EXPLAIN (FORMAT TEXT, FORMAT JSON) {q}")),
        "0A000"
    );
}

/// SQL-76 (1): エラーコード。未実装・対象外のオプションは DML でも文を実行しない。
#[test]
fn error_codes_and_no_side_effects() {
    let (core, _g) = seeded("x76-err");
    let before = snapshot(&core);
    for opt in ["FOO", "foo ON"] {
        assert_eq!(
            code(&core, &format!("EXPLAIN ({opt}) SELECT k FROM a")),
            "42601"
        );
    }
    for opt in ["COSTS 2", "COSTS 'maybe'", "FORMAT foo", "TIMING"] {
        assert_eq!(
            code(&core, &format!("EXPLAIN ({opt}) SELECT k FROM a")),
            "22023",
            "{opt}"
        );
    }
    for opt in [
        "BUFFERS",
        "WAL",
        "SETTINGS",
        "GENERIC_PLAN",
        "SERIALIZE",
        "MEMORY",
        "FORMAT XML",
        "FORMAT YAML",
        "ANALYZE",
        "VERBOSE",
        "SUMMARY",
        "FORMAT JSON",
    ] {
        for body in [
            "SELECT k FROM a",
            "DELETE FROM a WHERE k > 1",
            "UPDATE a SET v = 'z' WHERE k > 1",
        ] {
            assert_eq!(
                code(&core, &format!("EXPLAIN ({opt}) {body}")),
                "0A000",
                "{opt} {body}"
            );
        }
    }
    assert_eq!(snapshot(&core), before);
}

/// SQL-76 (1): 内側の文のエラーがオプションの意味検証より先に出る。
#[test]
fn inner_statement_error_comes_first() {
    let (core, _g) = seeded("x76-order");
    assert_eq!(code(&core, "EXPLAIN (FOO) SELECT * FROM missing"), "42P01");
}

/// SQL-76 (1): 括弧付きの集合演算は従来どおり。前置形は対象外のまま 42601。
#[test]
fn set_operation_and_prefix_forms_unchanged() {
    let (core, _g) = seeded("x76-reg");
    lines(
        &core,
        "tenant-a",
        "EXPLAIN (SELECT k FROM a) UNION SELECT k FROM a",
    );
    assert_eq!(code(&core, "EXPLAIN ANALYZE SELECT k FROM a"), "42601");
    assert_eq!(code(&core, "EXPLAIN VERBOSE SELECT k FROM a"), "42601");
    assert_eq!(
        code(&core, "EXPLAIN (COSTS) (COSTS) SELECT k FROM a"),
        "42601"
    );
}

/// SQL-76 (1): セッション無しの入口は従来どおり 42601。
#[test]
fn sessionless_entry_stays_rejected() {
    let (core, _g) = seeded("x76-nosess");
    let err = core
        .execute_sql(&ctx("tenant-a"), "EXPLAIN (COSTS OFF) SELECT k FROM a")
        .expect_err("rejected");
    assert_eq!(err.wire_code(), "42601");
}

/// SQL-76 (1): テナント境界。出力は行の量に依存しない。
#[test]
fn tenant_independent_output() {
    let (core, _g) = seeded("x76-tenant");
    let q = "SELECT k FROM a WHERE k = 2";
    let plain = lines(&core, "tenant-a", &format!("EXPLAIN {q}"));
    for t in ["tenant-a", "tenant-b"] {
        assert_eq!(lines(&core, t, &format!("EXPLAIN (COSTS OFF) {q}")), plain);
    }
}

/// SQL-76 (1): Prepare 経路で `$n` との併用は params のガードが拒否する（fail-closed）。
#[test]
fn prepare_with_params_rejected() {
    let (core, _g) = seeded("x76-prep");
    for sql in [
        "EXPLAIN (COSTS OFF) SELECT k FROM a WHERE k = $1",
        "EXPLAIN (COSTS OFF) SELECT x.k FROM a x JOIN a y ON x.k = y.k WHERE x.k = $1",
    ] {
        let err = core.parse_sql_prepared(sql).expect_err("rejected");
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
}
