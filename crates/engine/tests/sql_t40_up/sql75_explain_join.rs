//! JOIN の `EXPLAIN` 結合テスト（Issue #2046・SQL-75 (2)(3)(5)(6)・SQL-27・SQL-40 (6)(b)・RLS-10
//! ポインタ）。
//!
//! 文を実行せず、段の木（`join`／`scan` ほか）で各段の結合方式・結合の順・各入力の走査分類を返す。
//! 出力は閉じた語彙と項目数だけで、テナント・可視性・行数に依存しない。束縛エラーは実行したとき
//! と同じ `wire_code`。未対応の形（派生表・CTE・セッション無し）は従来どおり `42601`。

use engine::core::EngineCore;
use engine::join_limits::JoinSizeLimits;
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

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .unwrap_or_else(|e| panic!("setup should succeed: sql={sql:?} err={e:?}"));
}

/// a(k, v)・b(k, w)・c(k, x)・pt(id PK, k)・em（空）。tenant-a にだけ行がある。
fn seeded(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for sql in [
        "CREATE TABLE a (k BIGINT, v TEXT)",
        "CREATE TABLE b (k BIGINT, w TEXT)",
        "CREATE TABLE c (k BIGINT, x TEXT)",
        "CREATE TABLE pt (id BIGINT PRIMARY KEY, k BIGINT)",
        "CREATE TABLE em (k BIGINT, e TEXT)",
        "INSERT INTO a (k, v) VALUES (1, 'a1'), (2, 'a2'), (3, 'a3')",
        "INSERT INTO b (k, w) VALUES (2, 'b2'), (3, 'b3')",
        "INSERT INTO c (k, x) VALUES (2, 'c2'), (4, 'c4')",
        "INSERT INTO pt (id, k) VALUES (1, 2), (2, 3)",
    ] {
        exec_as(&core, "tenant-a", sql);
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

fn plan(core: &EngineCore, sql: &str) -> Vec<String> {
    lines(core, "tenant-a", sql)
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

#[test]
fn inner_hash_join_with_sort_and_limit() {
    let (core, _g) = seeded("x75-inner");
    // 根（深さ 0）は前置なし、子は深さに応じて `"  " x (d-1) + "-> "` で字下げされる。
    assert_eq!(
        plan(
            &core,
            "EXPLAIN SELECT a.k, b.w FROM a JOIN b ON a.k = b.k ORDER BY a.k LIMIT 10",
        ),
        strs(&[
            "limit",
            "limit: present",
            "offset: none",
            "-> sort",
            "   sort_keys: 1",
            "  -> join",
            "     join_method: hash",
            "     join_type: inner",
            "     join_keys: 1",
            "     join_filter: none",
            "    -> scan",
            "       relation: 1",
            "       scalar_plan: plain_scan",
            "       access_path: full_scan",
            "    -> scan",
            "       relation: 2",
            "       scalar_plan: plain_scan",
            "       access_path: full_scan",
        ])
    );
}

#[test]
fn join_types_and_reduction() {
    let (core, _g) = seeded("x75-types");
    let ty = |sql: &str| -> String {
        plan(&core, sql)
            .into_iter()
            .find(|l| l.trim_start().starts_with("join_type:"))
            .unwrap_or_else(|| panic!("no join_type for {sql}"))
            .trim()
            .to_string()
    };
    assert_eq!(
        ty("EXPLAIN SELECT * FROM a LEFT JOIN b ON a.k = b.k LIMIT 10"),
        "join_type: left"
    );
    assert_eq!(
        ty("EXPLAIN SELECT * FROM a RIGHT JOIN b ON a.k = b.k LIMIT 10"),
        "join_type: right"
    );
    assert_eq!(
        ty("EXPLAIN SELECT * FROM a FULL OUTER JOIN b ON a.k = b.k LIMIT 10"),
        "join_type: full"
    );
    // null を拒否する WHERE で LEFT は INNER に簡約される（実効値を出す）。
    assert_eq!(
        ty("EXPLAIN SELECT * FROM a LEFT JOIN b ON a.k = b.k WHERE b.w = 'b2' LIMIT 10"),
        "join_type: inner"
    );
}

#[test]
fn cross_and_comma_are_identical() {
    let (core, _g) = seeded("x75-cross");
    let cross = plan(&core, "EXPLAIN SELECT * FROM a CROSS JOIN b LIMIT 10");
    let comma = plan(&core, "EXPLAIN SELECT * FROM a, b LIMIT 10");
    assert_eq!(cross, comma);
    for want in ["join_method: cross", "join_keys: 0", "join_filter: none"] {
        assert!(
            cross.iter().any(|l| l.trim() == want),
            "{want} in {cross:#?}"
        );
    }
}

#[test]
fn nested_loop_and_residual_filters() {
    let (core, _g) = seeded("x75-nlj");
    let nl = plan(
        &core,
        "EXPLAIN SELECT * FROM a JOIN b ON a.k < b.k LIMIT 10",
    );
    for want in [
        "join_method: nested_loop",
        "join_keys: 0",
        "join_filter: present",
    ] {
        assert!(nl.iter().any(|l| l.trim() == want), "{want} in {nl:#?}");
    }
    let mixed = plan(
        &core,
        "EXPLAIN SELECT * FROM a JOIN b ON a.k = b.k AND a.k < b.k LIMIT 10",
    );
    for want in ["join_method: hash", "join_keys: 1", "join_filter: present"] {
        assert!(
            mixed.iter().any(|l| l.trim() == want),
            "{want} in {mixed:#?}"
        );
    }
    for sql in [
        "EXPLAIN SELECT * FROM a JOIN b USING (k) LIMIT 10",
        "EXPLAIN SELECT * FROM a NATURAL JOIN b LIMIT 10",
    ] {
        let got = plan(&core, sql);
        assert!(
            got.iter().any(|l| l.trim() == "join_method: hash"),
            "{sql}: {got:#?}"
        );
    }
}

#[test]
fn three_way_join_nests_left_deep() {
    let (core, _g) = seeded("x75-three");
    let got = plan(
        &core,
        "EXPLAIN SELECT * FROM a JOIN b ON a.k = b.k JOIN c ON b.k = c.k LIMIT 10",
    );
    let heads: Vec<&str> = got
        .iter()
        .filter(|l| l.trim_start().starts_with("->") || *l == "limit")
        .map(String::as_str)
        .collect();
    assert_eq!(
        heads,
        vec![
            "limit",
            "-> join",
            "  -> join",
            "    -> scan",
            "    -> scan",
            "  -> scan",
        ],
        "{got:#?}"
    );
    let rels: Vec<&str> = got
        .iter()
        .map(|l| l.trim())
        .filter(|l| l.starts_with("relation:"))
        .collect();
    assert_eq!(rels, vec!["relation: 1", "relation: 2", "relation: 3"]);
}

#[test]
fn aggregate_shape_stages() {
    let (core, _g) = seeded("x75-agg");
    let got = plan(
        &core,
        "EXPLAIN SELECT a.k, COUNT(b.k) AS n, SUM(b.k) AS s FROM a JOIN b ON a.k = b.k \
         GROUP BY a.k HAVING n > 0 ORDER BY a.k LIMIT 5 OFFSET 1",
    );
    assert_eq!(
        got[..9],
        strs(&[
            "limit",
            "limit: present",
            "offset: present",
            "-> sort",
            "   sort_keys: 1",
            "  -> aggregate",
            "     group_keys: 1",
            "     aggregates: 2",
            "     having: present",
        ]),
        "{got:#?}"
    );
}

#[test]
fn cross_relation_where_adds_filter_stage() {
    let (core, _g) = seeded("x75-filter");
    let got = plan(
        &core,
        "EXPLAIN SELECT * FROM a JOIN b ON a.k = b.k WHERE a.k < b.k LIMIT 10",
    );
    assert!(got.contains(&"-> filter".to_string()), "{got:#?}");
    assert!(
        got.iter().any(|l| l.trim() == "residual_predicates: 1"),
        "{got:#?}"
    );
}

#[test]
fn primary_key_pushdown_classifies_only_that_side() {
    let (core, _g) = seeded("x75-pk");
    let got = plan(
        &core,
        "EXPLAIN SELECT * FROM a JOIN pt ON a.k = pt.k WHERE pt.id = '1' LIMIT 10",
    );
    let sp: Vec<&str> = got
        .iter()
        .map(|l| l.trim())
        .filter(|l| l.starts_with("scalar_plan:") || l.starts_with("access_path:"))
        .collect();
    assert_eq!(
        sp,
        vec![
            "scalar_plan: plain_scan",
            "access_path: full_scan",
            "scalar_plan: index_primary_key",
            "access_path: primary_key_lookup",
        ],
        "{got:#?}"
    );
}

#[test]
fn output_is_tenant_and_data_independent() {
    let (core, _g) = seeded("x75-tenant");
    let sql = "EXPLAIN SELECT a.k FROM a JOIN b ON a.k = b.k ORDER BY a.k LIMIT 10";
    let with_rows = lines(&core, "tenant-a", sql);
    let no_rows = lines(&core, "tenant-b", sql);
    assert_eq!(with_rows, no_rows);
    // 空表同士でも同一の木（行数に依存しない）。
    let empty = lines(
        &core,
        "tenant-a",
        "EXPLAIN SELECT em.k FROM em JOIN em e2 ON em.k = e2.k ORDER BY em.k LIMIT 10",
    );
    assert_eq!(empty.len(), with_rows.len());
}

#[test]
fn explain_does_not_execute_the_join() {
    let (core, _g) = seeded("x75-noexec");
    // 出力行数の上限を極小にすると SELECT は 54000 だが、EXPLAIN は実行しないので成功する。
    let core = core.with_join_limits(JoinSizeLimits::new(8, 1000, 1));
    assert_eq!(code(&core, "SELECT * FROM a, b LIMIT 100"), "54000");
    assert!(!plan(&core, "EXPLAIN SELECT * FROM a, b LIMIT 100").is_empty());
}

#[test]
fn relation_count_limit_applies_to_explain_too() {
    let (core, _g) = seeded("x75-rels");
    for i in 0..9 {
        exec_as(&core, "tenant-a", &format!("CREATE TABLE t{i} (k BIGINT)"));
    }
    let nine: Vec<String> = (0..9).map(|i| format!("t{i}")).collect();
    let from = nine.join(", ");
    assert_eq!(
        code(&core, &format!("SELECT * FROM {from} LIMIT 10")),
        "54000"
    );
    assert_eq!(
        code(&core, &format!("EXPLAIN SELECT * FROM {from} LIMIT 10")),
        "54000"
    );
}

#[test]
fn binding_errors_match_execution() {
    let (core, _g) = seeded("x75-errors");
    exec_as(&core, "tenant-a", "CREATE TABLE tt (k TEXT)");
    for body in [
        "SELECT * FROM a JOIN nope ON a.k = nope.k LIMIT 10",
        "SELECT a.zzz FROM a JOIN b ON a.k = b.k LIMIT 10",
        "SELECT * FROM a JOIN tt ON a.k = tt.k LIMIT 10",
        "SELECT * FROM a JOIN b ON a.k = b.k LIMIT 99999999999",
    ] {
        let plain = code(&core, body);
        let explained = code(&core, &format!("EXPLAIN {body}"));
        assert_eq!(plain, explained, "{body}");
    }
    assert_eq!(
        code(&core, "SELECT * FROM a JOIN nope ON a.k = nope.k LIMIT 10"),
        "42P01"
    );
}

#[test]
fn unsupported_forms_stay_rejected() {
    let (core, _g) = seeded("x75-reject");
    for sql in [
        "EXPLAIN SELECT * FROM a JOIN (SELECT k FROM b) s ON a.k = s.k LIMIT 10",
        "EXPLAIN WITH w AS (SELECT k FROM b) SELECT * FROM a JOIN w ON a.k = w.k LIMIT 10",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
    // セッションの無い入口は従来どおり EXPLAIN を 42601 で拒否する。
    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "EXPLAIN SELECT * FROM a JOIN b ON a.k = b.k LIMIT 10",
        )
        .expect_err("session-less EXPLAIN must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn works_inside_explicit_transaction() {
    let (core, _g) = seeded("x75-tx");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let c = ctx("tenant-a");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let out = core
        .execute_sql_in_txn(
            &c,
            &mut session,
            &mut txn,
            "EXPLAIN SELECT * FROM a JOIN b ON a.k = b.k LIMIT 10",
        )
        .expect("explain in tx");
    assert!(matches!(out, SqlOutcome::Explain(_)));
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "COMMIT")
        .expect("commit");
}
