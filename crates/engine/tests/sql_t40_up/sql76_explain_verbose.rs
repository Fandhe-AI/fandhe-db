//! `EXPLAIN ( VERBOSE ) <文>` と `COSTS` の結合テスト（Issue #2060・SQL-76 (4)(5) のポインタ）。
//!
//! VERBOSE は根の段へ `output:`（結果の列名）と、距離順位付けの検索では `distance_metric:` を足す。
//! 既存の行の文言・順序は変えず（VERBOSE の行を除くとオプションなしの出力と一致）、文は実行しない。
//! `ANALYZE` との併用・`INSERT`・一時表・`$n` 付き（Prepare）は fail-closed。
//! `COSTS` は推定値を出さないため ON／OFF とも出力を変えない。

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
        "CREATE TABLE d (id BIGINT PRIMARY KEY, embedding VECTOR(2) NOT NULL, kind TEXT)",
        "INSERT INTO a (k, v) VALUES (1, 'a1'), (2, 'a2'), (3, 'a3')",
        "INSERT INTO d (id, embedding, kind) VALUES (1, '[1,0]', 'x'), (2, '[0,1]', 'y')",
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
        "{:?} {:?}",
        run(core, "tenant-a", "SELECT * FROM a").expect("select"),
        run(core, "tenant-a", "SELECT * FROM d").expect("select")
    )
}

fn is_verbose_line(l: &str) -> bool {
    l.starts_with("output: ") || l.starts_with("distance_metric: ")
}

const SEARCH: &str = "SELECT id FROM d ORDER BY embedding <=> '[1.0,0.0]' LIMIT 2";

/// SQL-76 (5): 単一表の検索・集計・広域取得・JOIN・集合演算・CTE・派生表で `output:` が出て、
/// VERBOSE の行を除くとオプションなしの出力と一致する。
#[test]
fn verbose_adds_output_and_keeps_existing_lines() {
    let (core, _g) = seeded("x76v-shape");
    let cases = [
        ("SELECT k, v FROM a WHERE k = 2", "output: k, v"),
        ("SELECT count(*) AS n FROM a", "output: n"),
        ("SELECT x.k FROM a x JOIN a y ON x.k = y.k", "output: k"),
        ("SELECT k FROM a UNION SELECT k FROM a", "output: k"),
        ("WITH c AS (SELECT k FROM a) SELECT k FROM c", "output: k"),
        ("SELECT t.k FROM (SELECT k FROM a) t", "output: k"),
        (SEARCH, "output: id"),
    ];
    let before = snapshot(&core);
    for (body, want) in cases {
        let plain = lines(&core, "tenant-a", &format!("EXPLAIN {body}"));
        for head in [
            "EXPLAIN (VERBOSE)",
            "EXPLAIN VERBOSE",
            "EXPLAIN (VERBOSE ON)",
        ] {
            let got = lines(&core, "tenant-a", &format!("{head} {body}"));
            assert!(got.iter().any(|l| l == want), "{head} {body}: {got:?}");
            let kept: Vec<String> = got
                .iter()
                .filter(|l| !is_verbose_line(l))
                .cloned()
                .collect();
            assert_eq!(kept, plain, "{head} {body}");
        }
    }
    assert_eq!(snapshot(&core), before, "VERBOSE EXPLAIN must not execute");
}

/// SQL-76 (5): 距離順位付けの検索の段だけ `distance_metric:` が出て、演算子の種別が反映される。
#[test]
fn distance_metric_for_ranking_stage_only() {
    let (core, _g) = seeded("x76v-metric");
    for (op, want) in [
        ("<=>", "cosine"),
        ("<->", "l2"),
        ("<#>", "ip"),
        ("<+>", "l1"),
    ] {
        let sql =
            format!("EXPLAIN (VERBOSE) SELECT id FROM d ORDER BY embedding {op} '[1,0]' LIMIT 2");
        let got = lines(&core, "tenant-a", &sql);
        assert!(
            got.contains(&format!("distance_metric: {want}")),
            "{op}: {got:?}"
        );
    }
    let scan = lines(&core, "tenant-a", "EXPLAIN (VERBOSE) SELECT k FROM a");
    assert!(!scan.iter().any(|l| l.starts_with("distance_metric:")));
}

/// SQL-76 (5): `RETURNING` を持つ述語形 DML は `output:` が出て、無ければ出ない。実行しない。
#[test]
fn dml_returning_output() {
    let (core, _g) = seeded("x76v-dml");
    let before = snapshot(&core);
    let plain = lines(&core, "tenant-a", "EXPLAIN DELETE FROM a WHERE k > 1");
    let got = lines(
        &core,
        "tenant-a",
        "EXPLAIN (VERBOSE) DELETE FROM a WHERE k > 1",
    );
    assert_eq!(got, plain, "no RETURNING: no extra lines");
    let got = lines(
        &core,
        "tenant-a",
        "EXPLAIN (VERBOSE) UPDATE a SET v = 'z' WHERE k > 1 RETURNING k, v",
    );
    assert!(got.contains(&"output: k, v".to_string()), "{got:?}");
    let plain = lines(
        &core,
        "tenant-a",
        "EXPLAIN UPDATE a SET v = 'z' WHERE k > 1 RETURNING k, v",
    );
    let kept: Vec<String> = got.into_iter().filter(|l| !is_verbose_line(l)).collect();
    assert_eq!(kept, plain);
    assert_eq!(snapshot(&core), before);
}

/// SQL-76 (5): 後の指定が勝つ。`VERBOSE OFF` はオプションなしと完全一致する。
#[test]
fn verbose_off_matches_plain_and_later_wins() {
    let (core, _g) = seeded("x76v-off");
    let q = "SELECT k FROM a";
    let plain = lines(&core, "tenant-a", &format!("EXPLAIN {q}"));
    for head in ["EXPLAIN (VERBOSE OFF)", "EXPLAIN (VERBOSE, VERBOSE OFF)"] {
        assert_eq!(lines(&core, "tenant-a", &format!("{head} {q}")), plain);
    }
    let on = lines(
        &core,
        "tenant-a",
        &format!("EXPLAIN (VERBOSE OFF, VERBOSE) {q}"),
    );
    assert!(on.iter().any(|l| l.starts_with("output: ")));
}

/// SQL-76 (4): `COSTS` の ON／OFF は推定値を出さず、出力はオプションなしと一致する。VERBOSE との併用も同じ。
#[test]
fn costs_does_not_change_output() {
    let (core, _g) = seeded("x76v-costs");
    for q in ["SELECT k FROM a WHERE k = 2", SEARCH] {
        let plain = lines(&core, "tenant-a", &format!("EXPLAIN {q}"));
        for head in [
            "EXPLAIN (COSTS ON)",
            "EXPLAIN (COSTS OFF)",
            "EXPLAIN (COSTS)",
        ] {
            assert_eq!(lines(&core, "tenant-a", &format!("{head} {q}")), plain);
        }
        let verbose = lines(&core, "tenant-a", &format!("EXPLAIN (VERBOSE) {q}"));
        assert_eq!(
            lines(
                &core,
                "tenant-a",
                &format!("EXPLAIN (VERBOSE, COSTS OFF) {q}")
            ),
            verbose
        );
        assert!(!verbose
            .iter()
            .any(|l| l.contains("cost") || l.contains("rows=")));
    }
}

/// SQL-76 (5)・テナント境界: 他テナントの行量で VERBOSE の出力が変わらない。表名・別名は出ない。
#[test]
fn tenant_boundary_output_is_independent_of_other_tenants() {
    let (core, _g) = seeded("x76v-tenant");
    let q = "EXPLAIN (VERBOSE) SELECT x.k AS key FROM a x";
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
    assert_eq!(lines(&core, "tenant-a", q), before);
    assert_eq!(lines(&core, "tenant-b", q), before);
    assert!(before.contains(&"output: key".to_string()));
    assert!(!before
        .iter()
        .any(|l| l.contains(" a") && l.contains("table")));
}

/// SQL-76 (5): 対応しない形は固定のエラーで拒否し、行を変えない。
#[test]
fn unsupported_forms_are_rejected() {
    let (core, _g) = seeded("x76v-reject");
    let before = snapshot(&core);
    for sql in [
        "EXPLAIN ANALYZE VERBOSE SELECT k FROM a",
        "EXPLAIN (ANALYZE, VERBOSE) SELECT k FROM a",
        "EXPLAIN (VERBOSE, FORMAT JSON) SELECT k FROM a",
        "EXPLAIN (VERBOSE) INSERT INTO a (k, v) VALUES (9, 'z')",
    ] {
        assert_eq!(code(&core, sql), "0A000", "{sql}");
    }
    assert_eq!(
        code(&core, "EXPLAIN (VERBOSE) SELECT * FROM missing"),
        "42P01"
    );
    assert_eq!(snapshot(&core), before);
}

/// SQL-76 (5): 一時表を参照する VERBOSE は黙って縮退せず `0A000`。
#[test]
fn temp_table_is_rejected() {
    let (core, _g) = seeded("x76v-temp");
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut s, "CREATE TEMP TABLE t (k BIGINT)")
        .expect("temp table");
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut s,
            "EXPLAIN (VERBOSE) SELECT k FROM t",
        )
        .expect_err("temp VERBOSE");
    assert_eq!(err.wire_code(), "0A000");
}

/// SQL-76 (5): 無関係な一時表があっても、永続表への VERBOSE は `output:` つきで成功する
/// （粗いセッションゲートを列導出で再適用しない回帰）。
#[test]
fn persistent_table_with_unrelated_temp_table_succeeds() {
    let (core, _g) = seeded("x76v-temp-unrelated");
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx("tenant-a"), &mut s, "CREATE TEMP TABLE t (k BIGINT)")
        .expect("temp table");
    let out = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut s,
            "EXPLAIN (VERBOSE) SELECT k FROM a",
        )
        .expect("persistent VERBOSE with unrelated temp table");
    match out {
        SqlOutcome::Explain(r) => {
            let texts: Vec<String> = r
                .rows
                .iter()
                .filter_map(|row| match &row.cells[0] {
                    Cell::Text(s) => Some(s.clone()),
                    _ => None,
                })
                .collect();
            assert!(texts.iter().any(|l| l.contains("output:")), "{texts:?}");
        }
        other => panic!("expected Explain, got {other:?}"),
    }
}

/// SQL-76 (5): 明示トランザクション内でも計画できる（READ ONLY でも可）。
#[test]
fn works_inside_transaction() {
    use engine::sql::transaction::{SessionTransaction, TransactionLimits};
    let (core, _g) = seeded("x76v-txn");
    let c = ctx("tenant-a");
    let mut s = SessionState::default();
    let mut txn = SessionTransaction::new(TransactionLimits::default());
    let mut exec = |s: &mut SessionState, sql: &str| {
        core.execute_sql_in_txn(&c, s, &mut txn, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
    };
    exec(&mut s, "BEGIN READ ONLY");
    for q in [
        "EXPLAIN (VERBOSE) SELECT k FROM a",
        "EXPLAIN (VERBOSE) DELETE FROM a WHERE k > 1",
    ] {
        match exec(&mut s, q) {
            SqlOutcome::Explain(_) => {}
            other => panic!("expected Explain, got {other:?}"),
        }
    }
    exec(&mut s, "COMMIT");
}

/// SQL-76 (5): セッション無しの入口は従来どおり `42601`。
#[test]
fn sessionless_entry_stays_syntax_error() {
    let (core, _g) = seeded("x76v-nosession");
    let err = core
        .execute_sql(&ctx("tenant-a"), "EXPLAIN (VERBOSE) SELECT k FROM a")
        .expect_err("sessionless");
    assert_eq!(err.wire_code(), "42601");
}

/// SQL-76 (5): 未解決の部分クエリを含む WHERE でも VERBOSE は受理され、既存の行は変わらない
/// （この形は `output:` を省く）。
#[test]
fn subquery_where_is_accepted() {
    let (core, _g) = seeded("x76v-subq");
    let body = "SELECT k FROM a WHERE k IN (SELECT id FROM d)";
    let plain = lines(&core, "tenant-a", &format!("EXPLAIN {body}"));
    let got = lines(&core, "tenant-a", &format!("EXPLAIN (VERBOSE) {body}"));
    let kept: Vec<String> = got.into_iter().filter(|l| !is_verbose_line(l)).collect();
    assert_eq!(kept, plain);
}
