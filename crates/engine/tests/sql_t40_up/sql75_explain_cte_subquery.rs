//! CTE・派生表・WHERE の部分クエリの `EXPLAIN` 結合テスト（Issue #2048・SQL-75 (1)(2)(3)(5)(6)・
//! SQL-29・SQL-62・RLS-10 ポインタ）。
//!
//! 文を実行せず、段の木（`cte`／`subquery`／`scan` ほか）で計画を返す。出力は閉じた語彙と項目数だけで、
//! テナント・可視性・行数・CTE 名に依存しない。束縛エラーは実行したときと同じ `wire_code`。
//! 未対応の形（集合演算の入力・相関・値位置の部分クエリ等）は `42601`。再帰・具体化 CTE・
//! CTE／派生表を入力にする JOIN は Issue #2976 で対応した（`sql75_explain_cte_inputs.rs`）。

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
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

/// 走査の既定分類（全走査）の 2 行を、`pad` 分だけ字下げして返す。
fn full_scan(pad: &str) -> Vec<String> {
    vec![
        format!("{pad}scalar_plan: plain_scan"),
        format!("{pad}access_path: full_scan"),
    ]
}

fn concat(parts: Vec<Vec<String>>) -> Vec<String> {
    parts.into_iter().flatten().collect()
}

/// `subquery` 段 1 件（非相関・内側は全走査の `scan`）。`depth` は `subquery` 見出しの深さ（>= 1）。
fn sub(depth: usize, kind: &str) -> Vec<String> {
    let pad = "  ".repeat(depth - 1);
    concat(vec![
        strs(&[
            &format!("{pad}-> subquery"),
            &format!("{pad}   subquery_kind: {kind}"),
            &format!("{pad}   correlated: false"),
            &format!("{pad}  -> scan"),
        ]),
        full_scan(&format!("{pad}     ")),
    ])
}

#[test]
fn inline_cte_with_limit_offset_and_order_by() {
    let (core, _g) = seeded("x75c-inline");
    let got = plan(
        &core,
        "EXPLAIN WITH w AS (SELECT k, v FROM a) SELECT k FROM w ORDER BY k LIMIT 5 OFFSET 1",
    );
    let want = concat(vec![
        strs(&[
            "cte",
            "cte_source: with",
            "cte_mode: inline",
            "-> limit",
            "   limit: present",
            "   offset: present",
            "  -> sort",
            "     sort_keys: 1",
            "    -> scan",
        ]),
        full_scan("       "),
    ]);
    assert_eq!(got, want);
}

#[test]
fn inline_cte_plain_and_chained_and_unreferenced_definitions_are_one_stage() {
    let (core, _g) = seeded("x75c-chain");
    let want = concat(vec![
        strs(&["cte", "cte_source: with", "cte_mode: inline", "-> scan"]),
        full_scan("   "),
    ]);
    for sql in [
        "EXPLAIN WITH w AS (SELECT k, v FROM a) SELECT k FROM w",
        // 連鎖した定義も、参照されない定義があっても、展開後は 1 段。
        "EXPLAIN WITH w AS (SELECT k, v FROM a), x AS (SELECT k, v FROM w) SELECT k FROM x",
        "EXPLAIN WITH w AS (SELECT k, v FROM a), unused AS (SELECT k FROM b) SELECT k FROM w",
    ] {
        assert_eq!(plan(&core, sql), want, "{sql}");
    }
}

#[test]
fn inline_cte_aggregate_uses_the_aggregate_classification() {
    let (core, _g) = seeded("x75c-agg");
    let got = plan(
        &core,
        "EXPLAIN WITH w AS (SELECT k, v FROM a) SELECT count(*) FROM w",
    );
    let want = strs(&[
        "cte",
        "cte_source: with",
        "cte_mode: inline",
        "-> aggregate",
        "   group_keys: 0",
        "   aggregates: 1",
        "   having: none",
        "  -> scan",
        "     scalar_plan: plain_scan",
        "     access_path: visible_bitmap_cache",
    ]);
    assert_eq!(got, want);
    // 単一表の集計 EXPLAIN の 2 行と同じ分類（畳み込み後は同じ実行経路）。
    let direct = plan(&core, "EXPLAIN SELECT count(*) FROM a");
    let tail: Vec<&str> = got[got.len() - 2..].iter().map(|l| l.trim()).collect();
    let direct_tail: Vec<&str> = direct[direct.len() - 2..]
        .iter()
        .map(|l| l.trim())
        .collect();
    assert_eq!(tail, direct_tail);
}

#[test]
fn derived_table_is_a_synthetic_cte() {
    let (core, _g) = seeded("x75c-derived");
    let want = concat(vec![
        strs(&[
            "cte",
            "cte_source: derived_table",
            "cte_mode: inline",
            "-> scan",
        ]),
        full_scan("   "),
    ]);
    for sql in [
        "EXPLAIN SELECT k FROM (SELECT k FROM a) t WHERE k = 2",
        "EXPLAIN SELECT k FROM (SELECT k FROM (SELECT k FROM a) u) t",
    ] {
        assert_eq!(plan(&core, sql), want, "{sql}");
    }
}

#[test]
fn where_subqueries_render_one_stage_each() {
    let (core, _g) = seeded("x75c-sub");
    for (pred, kind) in [
        ("k IN (SELECT k FROM b)", "in"),
        ("k NOT IN (SELECT k FROM b)", "not_in"),
        ("EXISTS (SELECT 1 FROM b)", "exists"),
        ("NOT EXISTS (SELECT 1 FROM b)", "not_exists"),
        ("k > ALL (SELECT k FROM b)", "all"),
        ("k = ANY (SELECT k FROM b)", "any"),
    ] {
        let sql = format!("EXPLAIN SELECT k FROM a WHERE {pred}");
        let want = concat(vec![full_scan(""), sub(1, kind)]);
        assert_eq!(plan(&core, &sql), want, "{sql}");
    }
}

#[test]
fn scalar_subquery_with_aggregate_inner_uses_the_fixed_full_scan() {
    let (core, _g) = seeded("x75c-scalar");
    let got = plan(
        &core,
        "EXPLAIN SELECT k FROM a WHERE k = (SELECT max(k) FROM b)",
    );
    let want = concat(vec![
        full_scan(""),
        strs(&[
            "-> subquery",
            "   subquery_kind: scalar",
            "   correlated: false",
            "  -> aggregate",
            "     group_keys: 0",
            "     aggregates: 1",
            "     having: none",
            "    -> scan",
        ]),
        full_scan("       "),
    ]);
    assert_eq!(got, want);
}

#[test]
fn nested_and_multiple_subqueries_keep_source_order() {
    let (core, _g) = seeded("x75c-nested");
    let got = plan(
        &core,
        "EXPLAIN SELECT k FROM a WHERE k NOT IN (SELECT k FROM b WHERE k IN (SELECT k FROM c))",
    );
    let want = concat(vec![full_scan(""), {
        let mut v = sub(1, "not_in");
        v.extend(sub(3, "in"));
        v
    }]);
    assert_eq!(got, want);
    let got = plan(
        &core,
        "EXPLAIN SELECT k FROM a WHERE k IN (SELECT k FROM b) OR EXISTS (SELECT 1 FROM c)",
    );
    let want = concat(vec![full_scan(""), sub(1, "in"), sub(1, "exists")]);
    assert_eq!(got, want);
}

#[test]
fn outer_classification_is_conservative_for_subquery_predicates() {
    let (core, _g) = seeded("x75c-conservative");
    let pk = strs(&[
        "scalar_plan: index_primary_key",
        "access_path: primary_key_lookup",
    ]);
    // 部分クエリを含まない連言の PK 点検索は保つ。
    let got = plan(
        &core,
        "EXPLAIN SELECT id FROM pt WHERE id = 1 AND k IN (SELECT k FROM b)",
    );
    assert_eq!(&got[..2], &pk[..]);
    // OR の内側に部分クエリがあれば PK 点検索とは主張しない。
    let got = plan(
        &core,
        "EXPLAIN SELECT id FROM pt WHERE id = 1 OR k IN (SELECT k FROM b)",
    );
    assert_eq!(&got[..2], &full_scan("")[..]);
    // 集計でも同様に、部分クエリ述語を索引・キャッシュに使えないフィルタとして扱う。
    let got = plan(
        &core,
        "EXPLAIN SELECT count(*) FROM a WHERE k IN (SELECT k FROM b)",
    );
    assert_eq!(&got[..2], &full_scan("")[..]);
}

#[test]
fn statements_without_subqueries_are_unchanged() {
    let (core, _g) = seeded("x75c-compat");
    assert_eq!(
        plan(&core, "EXPLAIN SELECT k FROM a WHERE k = 1"),
        full_scan("")
    );
    assert_eq!(
        plan(&core, "EXPLAIN SELECT id FROM pt WHERE id = 1"),
        strs(&[
            "scalar_plan: index_primary_key",
            "access_path: primary_key_lookup"
        ])
    );
}

#[test]
fn output_is_tenant_and_data_independent() {
    let (core, _g) = seeded("x75c-tenant");
    for sql in [
        "EXPLAIN WITH w AS (SELECT k, v FROM a) SELECT k FROM w ORDER BY k LIMIT 3",
        "EXPLAIN SELECT k FROM (SELECT k FROM a) t",
        "EXPLAIN SELECT k FROM a WHERE k IN (SELECT k FROM b)",
    ] {
        let with_rows = lines(&core, "tenant-a", sql);
        let no_rows = lines(&core, "tenant-b", sql);
        assert_eq!(with_rows, no_rows, "{sql}");
    }
}

#[test]
fn explain_does_not_execute_the_statement() {
    let (core, _g) = seeded("x75c-noexec");
    // 結果行数の上限を極小にすると LIMIT 省略の SELECT は 54000 だが、EXPLAIN は実行しないので成功する。
    let core = core.with_result_limits(ResultLimits::new(1));
    let body = "SELECT k FROM a WHERE k IN (SELECT k FROM b)";
    assert_eq!(code(&core, body), "54000");
    assert!(!plan(&core, &format!("EXPLAIN {body}")).is_empty());
}

#[test]
fn explain_skips_runtime_cardinality_errors() {
    let (core, _g) = seeded("x75c-card");
    // 内側が複数行を返すスカラー比較は実行すると 21000 だが、EXPLAIN は静的に妥当なら成功する。
    let scalar = "SELECT k FROM a WHERE k = (SELECT k FROM b)";
    assert_eq!(code(&core, scalar), "21000");
    assert!(!plan(&core, &format!("EXPLAIN {scalar}")).is_empty());
}

#[test]
fn binding_errors_match_execution() {
    let (core, _g) = seeded("x75c-errors");
    exec_as(&core, "tenant-a", "CREATE TABLE tt (k TEXT)");
    let bodies = [
        // 内側の未知表・未知列・投影列数・値族の不一致。
        "SELECT k FROM a WHERE k IN (SELECT k FROM nope)",
        "SELECT k FROM a WHERE k IN (SELECT zzz FROM b)",
        "SELECT k FROM a WHERE k IN (SELECT k, w FROM b)",
        "SELECT k FROM a WHERE k IN (SELECT k FROM tt)",
        "SELECT k FROM a WHERE k = (SELECT k, w FROM b LIMIT 1)",
        // 外側の未知列。
        "SELECT zzz FROM a WHERE k IN (SELECT k FROM b)",
        "SELECT k FROM a WHERE zzz IN (SELECT k FROM b)",
        // CTE 本文・主クエリ・派生表。
        "WITH w AS (SELECT k FROM nope) SELECT k FROM w",
        "WITH w AS (SELECT k FROM a) SELECT zzz FROM w",
        "SELECT zzz FROM (SELECT k FROM a) t",
        "SELECT k FROM (SELECT k FROM nope) t",
    ];
    for body in bodies {
        let plain = code(&core, body);
        let explained = code(&core, &format!("EXPLAIN {body}"));
        assert_eq!(plain, explained, "{body}");
    }
    assert_eq!(code(&core, bodies[0]), "42P01");
    assert_eq!(code(&core, bodies[1]), "42703");
}

#[test]
fn unsupported_forms_stay_rejected_with_42601() {
    let (core, _g) = seeded("x75c-reject");
    for sql in [
        // 集合演算の入力になる CTE・派生表（非 EXPLAIN でも未対応。再帰・具体化 CTE・JOIN 入力は
        // Issue #2976 で対応済み。`sql75_explain_cte_inputs.rs`）。
        "EXPLAIN WITH w AS (SELECT k FROM b) SELECT k FROM w UNION SELECT k FROM a",
        // 相関部分クエリ（黙って受理しない）。
        "EXPLAIN SELECT k FROM a WHERE EXISTS (SELECT 1 FROM b WHERE b.k = a.k)",
        "EXPLAIN SELECT k FROM a WHERE k IN (SELECT k FROM b WHERE b.k = a.k)",
        // 投影位置の値の部分クエリ。
        "EXPLAIN SELECT (SELECT max(k) FROM b) FROM a",
        // 集計の FILTER の部分クエリ。
        "EXPLAIN SELECT count(*) FILTER (WHERE k IN (SELECT k FROM b)) FROM a",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
}

#[test]
fn data_modifying_cte_stays_0a000_and_writes_nothing() {
    let (core, _g) = seeded("x75c-dm");
    let sql = "EXPLAIN WITH d AS (DELETE FROM b RETURNING k) SELECT k FROM d";
    assert_eq!(code(&core, sql), "0A000");
    // 書き込みは発生しない。
    let SqlOutcome::Query(r) = run(&core, "tenant-a", "SELECT k FROM b LIMIT 10").expect("select")
    else {
        panic!("expected query");
    };
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn placeholders_are_rejected_in_explain_of_cte_and_subqueries() {
    let (core, _g) = seeded("x75c-param");
    for sql in [
        "EXPLAIN WITH w AS (SELECT k, v FROM a) SELECT k FROM w WHERE k = $1",
        "EXPLAIN SELECT k FROM (SELECT k FROM a) t WHERE k = $1",
        "EXPLAIN SELECT k FROM a WHERE k IN (SELECT k FROM b) AND k = $1",
    ] {
        let err = core.parse_sql_prepared(sql).expect_err("must be rejected");
        assert_eq!(err.wire_code(), "42601", "{sql}");
    }
    // 部分クエリも CTE も無い EXPLAIN の `$n` は従来どおり受理される。
    assert!(core
        .parse_sql_prepared("EXPLAIN SELECT k FROM a WHERE k = $1")
        .is_ok());
}

#[test]
fn session_less_entry_still_rejects_explain() {
    let (core, _g) = seeded("x75c-nosession");
    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "EXPLAIN SELECT k FROM a WHERE k IN (SELECT k FROM b)",
        )
        .expect_err("session-less EXPLAIN must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn works_inside_explicit_transaction() {
    let (core, _g) = seeded("x75c-tx");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let c = ctx("tenant-a");
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    for sql in [
        "EXPLAIN WITH w AS (SELECT k, v FROM a) SELECT k FROM w",
        "EXPLAIN SELECT k FROM a WHERE k IN (SELECT k FROM b)",
    ] {
        let out = core
            .execute_sql_in_txn(&c, &mut session, &mut txn, sql)
            .expect("explain in tx");
        assert!(matches!(out, SqlOutcome::Explain(_)), "{sql}");
    }
    core.execute_sql_in_txn(&c, &mut session, &mut txn, "COMMIT")
        .expect("commit");
}

/// OR 内の通常述語は表示用の置換で落ちるが、束縛エラー（未知列 `42703`）は実行時と揃える。
#[test]
fn plain_predicate_inside_or_with_subquery_is_still_bound() {
    let (core, _g) = seeded("explain_or_plain_bind");
    assert_eq!(
        code(
            &core,
            "EXPLAIN SELECT k FROM a WHERE zzz = 1 OR k IN (SELECT k FROM b)"
        ),
        "42703"
    );
}

/// 内側の OR は実行時に `id = 1` 以外にも一致し得るため、PK 点検索と過大報告しない。
#[test]
fn inner_or_with_subquery_is_not_reported_as_pk_lookup() {
    let (core, _g) = seeded("explain_inner_or_no_pk");
    let out = lines(
        &core,
        "tenant-a",
        "EXPLAIN SELECT k FROM a WHERE k IN \
         (SELECT k FROM pt WHERE id = 1 OR k IN (SELECT k FROM b))",
    );
    assert!(
        out.iter().all(|l| !l.contains("primary_key_lookup")),
        "{out:?}"
    );
}
