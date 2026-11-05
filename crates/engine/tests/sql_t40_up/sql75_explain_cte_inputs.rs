//! 具体化 CTE・再帰 CTE・CTE／派生表を入力にする JOIN の `EXPLAIN` 結合テスト
//! （Issue #2976・SQL-75 (1)(2)(3)(5)(6)・SQL-35・SQL-62・RLS-10 ポインタ）。
//!
//! 文を実行せず（CTE 本文の評価・再帰の反復・キャッシュ・書き込みに触れない）、段の木
//! （`cte`／`recursive_union`／`work_table_scan`／`join`／`scan` ほか）で計画を返す。出力は閉じた
//! 語彙と項目数だけで、テナント・可視性・行数・CTE 名・別名・`LIMIT` 値に依存しない。束縛エラーは
//! EXPLAIN を付けない同じ文と同じ `wire_code`。未対応の形は固定文の `42601`（fail-closed）。
//! CTE・派生表を枝に持つ集合演算は非 EXPLAIN でも未対応（`42601`）なので、EXPLAIN も同じコードで拒否する。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recursive_cte_limits::RecursiveCteLimits;
use engine::result_limits::ResultLimits;
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

/// a(k, v)・b(k, w)・c(k, x)。tenant-a にだけ行がある。
fn seeded_core(core: EngineCore) -> EngineCore {
    for sql in [
        "CREATE TABLE a (k BIGINT, v TEXT)",
        "CREATE TABLE b (k BIGINT, w TEXT)",
        "CREATE TABLE c (k BIGINT, x TEXT)",
        "INSERT INTO a (k, v) VALUES (1, 'a1'), (2, 'a2'), (3, 'a3')",
        "INSERT INTO b (k, w) VALUES (2, 'b2'), (3, 'b3')",
        "INSERT INTO c (k, x) VALUES (2, 'c2'), (4, 'c4')",
    ] {
        exec_as(&core, "tenant-a", sql);
    }
    core
}

fn seeded(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = seeded_core(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    (core, guard)
}

fn run(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, String> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .map_err(|e| e.wire_code().to_string())
}

fn plan_as(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
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

fn plan(core: &EngineCore, sql: &str) -> Vec<String> {
    plan_as(core, "tenant-a", sql)
}

fn code(core: &EngineCore, sql: &str) -> String {
    match run(core, "tenant-a", sql) {
        Err(c) => c,
        Ok(_) => panic!("{sql} must be rejected"),
    }
}

fn assert_plan(core: &EngineCore, sql: &str, expected: &[&str]) {
    let got = plan(core, sql);
    let want: Vec<String> = expected.iter().map(|s| (*s).to_string()).collect();
    assert_eq!(got, want, "{sql}");
}

// ---- 具体化 CTE ----

#[test]
fn explicit_materialized_cte() {
    let (core, _g) = seeded("x75i-mat");
    assert_plan(
        &core,
        "EXPLAIN WITH w AS MATERIALIZED (SELECT k FROM a) SELECT k FROM w",
        &[
            "cte",
            "cte_source: with",
            "cte_mode: materialized",
            "-> scan",
            "   scalar_plan: plain_scan",
            "   access_path: full_scan",
        ],
    );
}

#[test]
fn tail_cte_stacks_outer_layers_above_the_cte_boundary() {
    let (core, _g) = seeded("x75i-tail");
    assert_plan(
        &core,
        "EXPLAIN WITH w AS (SELECT k FROM a ORDER BY k LIMIT 2) \
         SELECT k FROM w WHERE k > 1 ORDER BY k LIMIT 1",
        &[
            "limit",
            "limit: present",
            "offset: none",
            "-> sort",
            "   sort_keys: 1",
            "  -> filter",
            "     residual_predicates: 1",
            "    -> cte",
            "       cte_source: with",
            "       cte_mode: materialized",
            "      -> limit",
            "         limit: present",
            "         offset: none",
            "        -> sort",
            "           sort_keys: 1",
            "          -> scan",
            "             scalar_plan: plain_scan",
            "             access_path: full_scan",
        ],
    );
}

#[test]
fn derived_table_with_tail_reports_derived_source() {
    let (core, _g) = seeded("x75i-derived");
    assert_plan(
        &core,
        "EXPLAIN SELECT k FROM (SELECT k FROM a ORDER BY k LIMIT 2) t",
        &[
            "cte",
            "cte_source: derived_table",
            "cte_mode: materialized",
            "-> limit",
            "   limit: present",
            "   offset: none",
            "  -> sort",
            "     sort_keys: 1",
            "    -> scan",
            "       scalar_plan: plain_scan",
            "       access_path: full_scan",
        ],
    );
}

#[test]
fn aggregate_body_and_aggregate_outer() {
    let (core, _g) = seeded("x75i-agg");
    assert_plan(
        &core,
        "EXPLAIN WITH w AS MATERIALIZED (SELECT k, count(*) AS c FROM a GROUP BY k) \
         SELECT k FROM w WHERE c > 0 ORDER BY k",
        &[
            "sort",
            "sort_keys: 1",
            "-> filter",
            "   residual_predicates: 1",
            "  -> cte",
            "     cte_source: with",
            "     cte_mode: materialized",
            "    -> aggregate",
            "       group_keys: 1",
            "       aggregates: 1",
            "       having: none",
            "      -> scan",
            "         scalar_plan: plain_scan",
            "         access_path: full_scan",
        ],
    );
    assert_plan(
        &core,
        "EXPLAIN WITH w AS (SELECT k FROM a ORDER BY k LIMIT 2) SELECT count(*) FROM w",
        &[
            "aggregate",
            "group_keys: 0",
            "aggregates: 1",
            "having: none",
            "-> cte",
            "   cte_source: with",
            "   cte_mode: materialized",
            "  -> limit",
            "     limit: present",
            "     offset: none",
            "    -> sort",
            "       sort_keys: 1",
            "      -> scan",
            "         scalar_plan: plain_scan",
            "         access_path: full_scan",
        ],
    );
}

#[test]
fn materialized_hint_that_falls_back_to_inline_stays_inline() {
    let (core, _g) = seeded("x75i-inline");
    // 本文に ORDER BY を持つ CTE は序数指定でも具体化される。
    let got = plan(
        &core,
        "EXPLAIN WITH w AS (SELECT k FROM a ORDER BY 1) SELECT k FROM w",
    );
    assert!(got.iter().any(|l| l == "cte_mode: materialized"), "{got:?}");
    let got = plan(&core, "EXPLAIN WITH w AS (SELECT k FROM a) SELECT 1 FROM b");
    assert!(got.iter().any(|l| l == "cte_mode: inline"), "{got:?}");
    // 自己参照 0 個の `WITH RECURSIVE` は通常の CTE と同じ出力。
    let plain = plan(&core, "EXPLAIN WITH w AS (SELECT k FROM a) SELECT k FROM w");
    let recursive = plan(
        &core,
        "EXPLAIN WITH RECURSIVE w AS (SELECT k FROM a) SELECT k FROM w",
    );
    assert_eq!(plain, recursive);
}

// ---- 再帰 CTE ----

#[test]
fn recursive_cte_with_constant_anchor() {
    let (core, _g) = seeded("x75i-rec1");
    assert_plan(
        &core,
        "EXPLAIN WITH RECURSIVE r(n) AS \
         (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r",
        &[
            "cte",
            "cte_source: with",
            "cte_mode: recursive",
            "-> recursive_union",
            "   union: all",
            "   search: none",
            "   cycle: none",
            "  -> result",
            "     source: constant",
            "  -> work_table_scan",
            "     filter: present",
        ],
    );
}

#[test]
fn recursive_cte_with_join_term_search_cycle_and_outer_layers() {
    let (core, _g) = seeded("x75i-rec2");
    assert_plan(
        &core,
        "EXPLAIN WITH RECURSIVE r(n) AS (SELECT k FROM a UNION ALL \
         SELECT b.k FROM r JOIN b ON r.n = b.k) \
         SEARCH DEPTH FIRST BY n SET o CYCLE n SET m USING p \
         SELECT n FROM r ORDER BY n LIMIT 3",
        &[
            "limit",
            "limit: present",
            "offset: none",
            "-> sort",
            "   sort_keys: 1",
            "  -> cte",
            "     cte_source: with",
            "     cte_mode: recursive",
            "    -> recursive_union",
            "       union: all",
            "       search: depth_first",
            "       cycle: present",
            "      -> scan",
            "         scalar_plan: plain_scan",
            "         access_path: full_scan",
            "      -> join",
            "         join_method: hash",
            "         join_type: inner",
            "         join_keys: 1",
            "         join_filter: none",
            "        -> work_table_scan",
            "           relation: 1",
            "        -> scan",
            "           relation: 2",
            "           scalar_plan: plain_scan",
            "           access_path: full_scan",
        ],
    );
    let got = plan(
        &core,
        "EXPLAIN WITH RECURSIVE r(n) AS (SELECT k FROM a UNION SELECT b.k FROM r \
         JOIN b ON r.n = b.k) SELECT count(*) FROM r",
    );
    assert!(
        got.iter().any(|l| l.ends_with("union: distinct")),
        "{got:?}"
    );
    assert_eq!(got.first().map(String::as_str), Some("aggregate"));
}

#[test]
fn recursive_cte_with_set_operation_anchor() {
    let (core, _g) = seeded("x75i-rec3");
    let got = plan(
        &core,
        "EXPLAIN WITH RECURSIVE r(n) AS (SELECT k FROM a UNION SELECT k FROM b \
         UNION ALL SELECT n FROM r WHERE n < 3) SELECT n FROM r",
    );
    assert!(
        got.iter().any(|l| l.trim() == "-> set_operation"),
        "{got:?}"
    );
    assert!(
        got.iter().any(|l| l.trim() == "-> work_table_scan"),
        "{got:?}"
    );
}

#[test]
fn recursive_structure_violations_match_the_plain_statement() {
    let (core, _g) = seeded("x75i-rec-err");
    for body in [
        // 自己参照が 2 回。
        "WITH RECURSIVE r(n) AS (SELECT k FROM a UNION ALL SELECT r.n FROM r JOIN r r2 ON r.n = r2.n) SELECT n FROM r",
        // 再帰項が作業表を読まない。
        "WITH RECURSIVE r(n) AS (SELECT k FROM a UNION ALL SELECT k FROM b) SELECT n FROM r",
        // 自己参照が 0 個（アンカー項だけ）。
        "WITH RECURSIVE r(n) AS (SELECT 1) SELECT n FROM r",
        // 列数の不一致。
        "WITH RECURSIVE r(n) AS (SELECT k FROM a UNION ALL SELECT k, v FROM r) SELECT n FROM r",
    ] {
        let plain = code(&core, body);
        assert_eq!(code(&core, &format!("EXPLAIN {body}")), plain, "{body}");
    }
}

// ---- CTE・派生表を入力にする JOIN ----

#[test]
fn join_with_cte_input_replaces_only_the_cte_side() {
    let (core, _g) = seeded("x75i-join1");
    assert_plan(
        &core,
        "EXPLAIN WITH w AS (SELECT k FROM a) SELECT w.k FROM w JOIN b ON w.k = b.k",
        &[
            "join",
            "join_method: hash",
            "join_type: inner",
            "join_keys: 1",
            "join_filter: none",
            "-> cte",
            "   relation: 1",
            "   cte_source: with",
            "   cte_mode: materialized",
            "   cte_shared: false",
            "  -> scan",
            "     scalar_plan: plain_scan",
            "     access_path: full_scan",
            "-> scan",
            "   relation: 2",
            "   scalar_plan: plain_scan",
            "   access_path: full_scan",
        ],
    );
}

#[test]
fn join_with_derived_table_input_reports_derived_source() {
    let (core, _g) = seeded("x75i-join2");
    let got = plan(
        &core,
        "EXPLAIN SELECT t.k FROM (SELECT k FROM a) t JOIN b ON t.k = b.k",
    );
    assert!(
        got.iter().any(|l| l == "   cte_source: derived_table"),
        "{got:?}"
    );
    assert!(got.iter().any(|l| l == "   cte_shared: false"), "{got:?}");
}

#[test]
fn shared_cte_is_reported_for_each_reference() {
    let (core, _g) = seeded("x75i-join3");
    let got = plan(
        &core,
        "EXPLAIN WITH w AS (SELECT k FROM a) SELECT w.k FROM w JOIN w w2 ON w.k = w2.k",
    );
    assert_eq!(
        got.iter().filter(|l| *l == "   cte_shared: true").count(),
        2
    );
    let got = plan(
        &core,
        "EXPLAIN WITH w AS NOT MATERIALIZED (SELECT k FROM a) \
         SELECT w.k FROM w JOIN b ON w.k = b.k",
    );
    assert!(got.iter().any(|l| l == "   cte_shared: false"), "{got:?}");
}

#[test]
fn derived_slot_chain_nests_the_parent_cte() {
    let (core, _g) = seeded("x75i-join4");
    assert_plan(
        &core,
        "EXPLAIN WITH w AS (SELECT k FROM a), x AS (SELECT k FROM w ORDER BY k LIMIT 2) \
         SELECT x.k FROM x JOIN b ON x.k = b.k",
        &[
            "join",
            "join_method: hash",
            "join_type: inner",
            "join_keys: 1",
            "join_filter: none",
            "-> cte",
            "   relation: 1",
            "   cte_source: with",
            "   cte_mode: materialized",
            "   cte_shared: false",
            "  -> limit",
            "     limit: present",
            "     offset: none",
            "    -> sort",
            "       sort_keys: 1",
            "      -> cte",
            "         cte_source: with",
            "         cte_mode: materialized",
            "         cte_shared: false",
            "        -> scan",
            "           scalar_plan: plain_scan",
            "           access_path: full_scan",
            "-> scan",
            "   relation: 2",
            "   scalar_plan: plain_scan",
            "   access_path: full_scan",
        ],
    );
}

#[test]
fn unused_with_clause_before_a_plain_join_is_byte_identical() {
    let (core, _g) = seeded("x75i-join5");
    let plain = plan(&core, "EXPLAIN SELECT a.k FROM a JOIN b ON a.k = b.k");
    let with = plan(
        &core,
        "EXPLAIN WITH w AS (SELECT k FROM c) SELECT a.k FROM a JOIN b ON a.k = b.k",
    );
    assert_eq!(plain, with);
}

#[test]
fn join_input_binding_errors_match_the_plain_statement() {
    let (core, _g) = seeded("x75i-join-err");
    for sql in [
        // 型の不一致。
        "WITH w AS (SELECT k, v FROM a) SELECT w.k FROM w JOIN b ON w.v = b.k",
        // 集計本文の slot。
        "WITH w AS (SELECT count(*) AS c FROM a) SELECT w.c FROM w JOIN b ON w.c = b.k",
        // 未知列。
        "WITH w AS (SELECT k FROM a) SELECT w.zz FROM w JOIN b ON w.k = b.k",
        // 未知表。
        "WITH w AS (SELECT k FROM nope) SELECT w.k FROM w JOIN b ON w.k = b.k",
        "SELECT t.k FROM (SELECT k FROM nope) t JOIN b ON t.k = b.k",
    ] {
        let plain = code(&core, sql);
        assert_eq!(code(&core, &format!("EXPLAIN {sql}")), plain, "{sql}");
    }
}

// ---- パリティ・fail-closed ----

#[test]
fn materialized_binding_errors_match_the_plain_statement() {
    let (core, _g) = seeded("x75i-parity");
    for sql in [
        "WITH w AS (SELECT k FROM nope ORDER BY k LIMIT 2) SELECT k FROM w",
        "WITH w AS (SELECT k FROM a ORDER BY k LIMIT 2) SELECT zz FROM w",
        "WITH w AS MATERIALIZED (SELECT k FROM a) SELECT k FROM w WHERE k = 'x'",
        "WITH w(p, q, r) AS MATERIALIZED (SELECT k, v FROM a) SELECT p FROM w",
        "WITH w AS (SELECT k FROM a ORDER BY k LIMIT 2) SELECT k FROM w LIMIT 0",
    ] {
        let plain = code(&core, sql);
        assert_eq!(code(&core, &format!("EXPLAIN {sql}")), plain, "{sql}");
    }
}

#[test]
fn set_operation_inputs_are_rejected_like_the_plain_statement() {
    let (core, _g) = seeded("x75i-setop");
    for sql in [
        "WITH w AS (SELECT k FROM b) SELECT k FROM w UNION SELECT k FROM a",
        "SELECT k FROM (SELECT k FROM a) s UNION SELECT k FROM b",
    ] {
        let plain = code(&core, sql);
        assert_eq!(plain, "42601", "{sql}");
        assert_eq!(code(&core, &format!("EXPLAIN {sql}")), plain, "{sql}");
    }
}

#[test]
fn window_over_materialized_cte_and_view_forms_are_rejected_with_42601() {
    let (core, _g) = seeded("x75i-reject");
    exec_as(
        &core,
        "tenant-a",
        "CREATE VIEW lv AS SELECT k FROM a ORDER BY k LIMIT 2",
    );
    for sql in [
        "EXPLAIN WITH w AS (SELECT k FROM a ORDER BY k LIMIT 2) \
         SELECT k, row_number() OVER (ORDER BY k) FROM w",
        "EXPLAIN SELECT k FROM lv",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
}

#[test]
fn placeholders_stay_rejected_in_the_new_forms() {
    let (core, _g) = seeded("x75i-param");
    for sql in [
        "EXPLAIN WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < $1) SELECT n FROM r",
        "EXPLAIN WITH w AS MATERIALIZED (SELECT k FROM a) SELECT k FROM w WHERE k = $1",
        "EXPLAIN SELECT t.k FROM (SELECT k FROM a) t JOIN b ON t.k = b.k WHERE t.k = $1",
    ] {
        let err = core.parse_sql_prepared(sql).expect_err("must be rejected");
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
}

#[test]
fn session_less_entry_and_transactions() {
    let (core, _g) = seeded("x75i-tx");
    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "EXPLAIN WITH w AS MATERIALIZED (SELECT k FROM a) SELECT k FROM w",
        )
        .expect_err("session-less EXPLAIN must be rejected");
    assert_eq!(err.wire_code(), "42601");

    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let c = ctx("tenant-a");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    for sql in [
        "EXPLAIN WITH w AS MATERIALIZED (SELECT k FROM a) SELECT k FROM w",
        "EXPLAIN WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r",
        "EXPLAIN WITH w AS (SELECT k FROM a) SELECT w.k FROM w JOIN b ON w.k = b.k",
    ] {
        let out = core
            .execute_sql_in_txn(&c, &mut session, &mut txn, sql)
            .expect("explain in tx");
        assert!(matches!(out, SqlOutcome::Explain(_)), "{sql}");
    }
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "COMMIT")
        .expect("commit");
}

// ---- 実行しないこと・テナント非依存 ----

#[test]
fn explain_does_not_evaluate_bodies_or_hit_limits() {
    let path = unique_db_path("x75i-noexec");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = seeded_core(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_result_limits(ResultLimits::new(1))
            .with_recursive_cte_limits(RecursiveCteLimits::new(1, 1)),
    );
    let before = core.materialized_cte_eval_stats().evaluations;
    // 結果行数上限 1 では、実行すると 54000。EXPLAIN は本文を評価しないので成功する。
    assert_eq!(
        code(
            &core,
            "WITH w AS MATERIALIZED (SELECT k FROM a) SELECT k FROM w"
        ),
        "54000"
    );
    let after_plain = core.materialized_cte_eval_stats().evaluations;
    assert!(after_plain > before);
    for sql in [
        "EXPLAIN WITH w AS MATERIALIZED (SELECT k FROM a) SELECT k FROM w",
        "EXPLAIN WITH w AS (SELECT k FROM a) SELECT w.k FROM w JOIN b ON w.k = b.k",
        // 反復上限 1 回を超える再帰も計画だけを返す。
        "EXPLAIN WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 100) SELECT n FROM r",
    ] {
        plan(&core, sql);
    }
    assert_eq!(core.materialized_cte_eval_stats().evaluations, after_plain);
}

#[test]
fn data_modifying_cte_stays_0a000_and_writes_nothing() {
    let (core, _g) = seeded("x75i-dm");
    let sql = "EXPLAIN WITH d AS (DELETE FROM b RETURNING k) SELECT k FROM d";
    assert_eq!(code(&core, sql), "0A000");
    let SqlOutcome::Query(r) = run(&core, "tenant-a", "SELECT k FROM b LIMIT 10").expect("select")
    else {
        panic!("expected query");
    };
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn output_is_tenant_independent_and_names_nothing() {
    let (core, _g) = seeded("x75i-tenant");
    for sql in [
        "EXPLAIN WITH secretcte AS MATERIALIZED (SELECT k FROM a) SELECT k FROM secretcte",
        "EXPLAIN WITH RECURSIVE secretrec(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM secretrec WHERE n < 3) SELECT n FROM secretrec",
        "EXPLAIN WITH secretcte AS (SELECT k FROM a ORDER BY k LIMIT 77) SELECT secretcte.k FROM secretcte JOIN b ON secretcte.k = b.k",
        "EXPLAIN SELECT secretalias.k FROM (SELECT k FROM a ORDER BY k LIMIT 77) secretalias JOIN b ON secretalias.k = b.k",
    ] {
        let a = plan_as(&core, "tenant-a", sql);
        // 行を持たないテナントでも、出力はバイト単位で同じ。
        let b = plan_as(&core, "tenant-b", sql);
        assert_eq!(a, b, "{sql}");
        let text = a.join("\n");
        for needle in ["secret", "77", "?derived"] {
            assert!(!text.contains(needle), "{needle} leaked in {text}");
        }
    }
}

/// 再帰 CTE のアンカー項・再帰項に EXPLAIN が描画できない形（部分クエリ・窓）があるとき、EXPLAIN は
/// 段を黙って落とした計画を返さず、非 EXPLAIN と同じ `wire_code` で拒否する（fail-closed のパリティ）。
/// 受理される場合は必ず `42601`（描画できない形の固定拒否）でなければならない。
#[test]
fn unrendered_recursive_anchor_and_term_shapes_are_never_planned() {
    let (core, _g) = seeded("x75i-rec-gate");
    for body in [
        "SELECT count(*) FROM a WHERE k IN (SELECT k FROM b) \
         UNION ALL SELECT n FROM r WHERE n < 3",
        "SELECT k FROM a WHERE k IN (SELECT k FROM b) UNION SELECT k FROM c \
         UNION ALL SELECT n FROM r WHERE n < 3",
        "SELECT k FROM a UNION ALL SELECT n FROM r WHERE n IN (SELECT k FROM b)",
        "SELECT k FROM a UNION ALL SELECT row_number() OVER (ORDER BY n) FROM r WHERE n < 3",
    ] {
        let sql = format!("WITH RECURSIVE r(n) AS ({body}) SELECT n FROM r");
        let explained = code(&core, &format!("EXPLAIN {sql}"));
        match run(&core, "tenant-a", &sql) {
            Err(plain) => assert_eq!(explained, plain, "{sql}"),
            Ok(_) => assert_eq!(explained, "42601", "{sql}"),
        }
    }
}
