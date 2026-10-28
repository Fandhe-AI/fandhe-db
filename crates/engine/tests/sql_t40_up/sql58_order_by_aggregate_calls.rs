//! `GROUP BY` 集計の `ORDER BY` 内の集計呼び出しの直接記述（`ORDER BY sum(n) DESC` 等）の結合テスト
//! （Issue #1815、対象ビヘイビア: SQL-58。ポインタ: `docs/spec/04-behavior/` の SQL-58）。
//!
//! `tests/sql26_order_by_having_expressions.rs` と同じ流儀（`unique_db_path`＋`CleanupGuard`、
//! `EngineCore::execute_sql` の production 経路、production の判定関数を呼ばない Rust 側の
//! 独立オラクル）で、結果・隠し項目が出力列に出ないこと・重複排除・上限・エラー契約
//! （`wire_code`）・ビューの列公開範囲・RLS 境界を固定する。期待値は PostgreSQL 17 の意味論で、
//! fixture から手計算した値。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("title", ColumnType::Text, true),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("b", ColumnType::BigInt, true),
            ColumnDef::new("f", ColumnType::Double, true),
        ],
    )
}

/// `(id, title, lang, n, b, f)`。
type Row = (
    u64,
    Option<&'static str>,
    &'static str,
    Option<i32>,
    i64,
    f64,
);

/// グループ別の真値（手計算）:
///
/// | lang | 行 | count(*) | count(n) | sum(n) | avg(n) | min(n) | max(n) | sum(b) | max(f)-min(f) |
/// | ---- | -- | -------- | -------- | ------ | ------ | ------ | ------ | ------ | ------------- |
/// | ja | 1,3,5 | 3 | 2 | 5 | 2.5 | 2 | 3 | 90 | 1.0 |
/// | en | 2,4,7 | 3 | 3 | 6 | 2.0 | 0 | 5 | 130 | 1.5 |
/// | fr | 6,8 | 2 | 2 | 7 | 3.5 | 3 | 4 | 140 | 1.0 |
/// | de | 9,10 | 2 | 0 | NULL | NULL | NULL | NULL | 11 | 2.0 |
const ROWS: [Row; 10] = [
    (1, Some("a"), "ja", Some(3), 10, 1.5),
    (2, Some("b"), "en", Some(1), 20, 2.5),
    (3, Some("c"), "ja", None, 30, 0.5),
    (4, Some("d"), "en", Some(5), 40, 3.5),
    (5, Some("e"), "ja", Some(2), 50, 1.0),
    (6, Some("f"), "fr", Some(4), 60, 2.0),
    (7, None, "en", Some(0), 70, 4.0),
    (8, Some("h"), "fr", Some(3), 80, 1.0),
    // de: n が全て NULL（sum(n) 等が NULL になるグループ。NULL の並び位置の検証用）。
    (9, None, "de", None, 5, 1.0),
    (10, None, "de", None, 6, 3.0),
];

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn op_id(id: u64) -> OperationId {
    OperationId::parse(&format!("test-op-{id}")).expect("valid operation_id")
}

fn insert_row(storage: &Storage, ctx: &PolicyContext, visibility: Visibility, r: &Row) {
    let (id, title, lang, n, b, f) = *r;
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            title
                .map(|t| Value::Text(t.to_string()))
                .unwrap_or(Value::Null),
            Value::Text(lang.to_string()),
            n.map(Value::Integer).unwrap_or(Value::Null),
            Value::BigInt(b),
            Value::Double(f),
        ],
        &op_id(id),
    )
    .expect("insert row");
}

/// `tenant-a` に `ROWS`、`tenant-b` の Private な `ja` 5 行（RLS 境界の検証用）を投入した core。
fn build(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let a = ctx_for("tenant-a");
    for r in &ROWS {
        insert_row(&storage, &a, Visibility::Public, r);
    }
    let b = ctx_for("tenant-b");
    for id in 101..106u64 {
        insert_row(
            &storage,
            &b,
            Visibility::Private,
            &(id, Some("x"), "ja", Some(1), 1, 1.0),
        );
    }
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn run(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> QueryResult {
    core.execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e:?}"))
}

fn run_err(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> &'static str {
    core.execute_sql(ctx, sql)
        .expect_err(&format!("query must fail: {sql}"))
        .wire_code()
}

fn session_err(
    core: &EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
    sql: &str,
) -> &'static str {
    core.execute_sql_in_session(ctx, session, sql)
        .expect_err(&format!("query must fail: {sql}"))
        .wire_code()
}

fn langs(result: &QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Text(t)) => t.clone(),
            other => panic!("expected TEXT group key, got {other:?}"),
        })
        .collect()
}

fn cell_i64(cell: &Cell) -> i64 {
    match cell {
        Cell::Integer(v) => i64::try_from(*v).expect("fits i64"),
        Cell::SignedInteger(v) => *v,
        other => panic!("expected integer cell, got {other:?}"),
    }
}

const BASE: &str = "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang";

fn order(core: &EngineCore, tail: &str) -> Vec<String> {
    let ctx = ctx_for("tenant-a");
    langs(&run(core, &ctx, &format!("{BASE} {tail}")))
}

#[test]
fn order_by_sum_desc_and_nulls_placement() {
    let (core, _g) = build("sql58o-sum");
    // 受け入れ条件: NULL（de）は DESC の既定で先頭。
    assert_eq!(
        order(&core, "ORDER BY sum(n) DESC"),
        ["de", "fr", "en", "ja"]
    );
    assert_eq!(order(&core, "ORDER BY sum(n)"), ["ja", "en", "fr", "de"]);
    assert_eq!(
        order(&core, "ORDER BY sum(n) DESC NULLS LAST"),
        ["fr", "en", "ja", "de"]
    );
    assert_eq!(
        order(&core, "ORDER BY sum(n) ASC NULLS FIRST"),
        ["de", "ja", "en", "fr"]
    );
}

#[test]
fn order_by_aggregate_families() {
    let (core, _g) = build("sql58o-family");
    assert_eq!(
        order(&core, "ORDER BY count(*), lang"),
        ["de", "fr", "en", "ja"]
    );
    assert_eq!(
        order(&core, "ORDER BY count(*) DESC, lang"),
        ["en", "ja", "de", "fr"]
    );
    // 整数列の AVG は NUMERIC セル（厳密比較）。
    assert_eq!(
        order(&core, "ORDER BY avg(n) DESC NULLS LAST"),
        ["fr", "ja", "en", "de"]
    );
    assert_eq!(order(&core, "ORDER BY min(n)"), ["en", "ja", "fr", "de"]);
    assert_eq!(
        order(&core, "ORDER BY max(n) DESC NULLS LAST"),
        ["en", "fr", "ja", "de"]
    );
    // BIGINT の SUM は NUMERIC セル。
    assert_eq!(
        order(&core, "ORDER BY sum(b) DESC"),
        ["fr", "en", "ja", "de"]
    );
    // 式キー。
    assert_eq!(
        order(&core, "ORDER BY max(f) - min(f) DESC, lang"),
        ["de", "en", "fr", "ja"]
    );
    assert_eq!(
        order(&core, "ORDER BY coalesce(sum(n), 0), lang"),
        ["de", "ja", "en", "fr"]
    );
}

#[test]
fn order_by_aggregate_with_ordinals_and_limits() {
    let (core, _g) = build("sql58o-ordinal");
    assert_eq!(
        order(&core, "ORDER BY 2 DESC, sum(n)"),
        ["ja", "en", "fr", "de"]
    );
    assert_eq!(
        order(&core, "ORDER BY count(*) DESC, 1"),
        ["en", "ja", "de", "fr"]
    );
    assert_eq!(order(&core, "ORDER BY sum(n) DESC LIMIT 2"), ["de", "fr"]);
    assert_eq!(
        order(&core, "ORDER BY sum(n) DESC LIMIT 2 OFFSET 1"),
        ["fr", "en"]
    );
    let mut ties = order(&core, "ORDER BY count(*) FETCH FIRST 1 ROWS WITH TIES");
    ties.sort();
    assert_eq!(ties, ["de", "fr"]);
}

#[test]
fn hidden_order_items_are_not_projected_and_are_shared() {
    let (core, _g) = build("sql58o-projection");
    let ctx = ctx_for("tenant-a");
    let r = run(
        &core,
        &ctx,
        "SELECT lang, SUM(n) AS s FROM docs GROUP BY lang ORDER BY sum(n) DESC NULLS LAST",
    );
    assert_eq!(r.columns.len(), 2);
    assert!(r.rows.iter().all(|row| row.cells.len() == 2));
    assert_eq!(langs(&r), ["fr", "en", "ja", "de"]);
    assert_eq!(cell_i64(&r.rows[0].cells[1]), 7);
    // SELECT リストに集計が無い形。
    let r = run(
        &core,
        &ctx,
        "SELECT lang FROM docs GROUP BY lang ORDER BY count(*) DESC, lang",
    );
    assert_eq!(r.columns.len(), 1);
    assert_eq!(langs(&r), ["en", "ja", "de", "fr"]);
    // HAVING と同じ集計を共有する。
    let r = run(
        &core,
        &ctx,
        &format!("{BASE} HAVING sum(n) > 5 ORDER BY sum(n) DESC"),
    );
    assert_eq!(langs(&r), ["fr", "en"]);
    assert_eq!(r.columns.len(), 2);
}

#[test]
fn aggregate_item_limit_counts_order_by_items() {
    let (core, _g) = build("sql58o-limit");
    let ctx = ctx_for("tenant-a");
    let terms = |count: usize| {
        (0..count)
            .map(|k| format!("sum(n + {k})"))
            .collect::<Vec<_>>()
            .join(" + ")
    };
    let sel = "SELECT lang FROM docs GROUP BY lang";
    assert_eq!(
        langs(&run(&core, &ctx, &format!("{sel} ORDER BY {}", terms(32)))).len(),
        4
    );
    assert_eq!(
        run_err(&core, &ctx, &format!("{sel} ORDER BY {}", terms(33))),
        "54000"
    );
    // 可視 1 ＋隠し 32 は上限超過。
    assert_eq!(
        run_err(
            &core,
            &ctx,
            &format!(
                "SELECT lang, COUNT(*) FROM docs GROUP BY lang ORDER BY {}",
                terms(32)
            )
        ),
        "54000"
    );
    // HAVING の集計と ORDER BY の集計は添字空間を共有し重複排除される。
    assert_eq!(
        langs(&run(
            &core,
            &ctx,
            &format!("{sel} HAVING sum(n + 0) > 0 ORDER BY {}", terms(32))
        ))
        .len(),
        3
    );
}

#[test]
fn rejected_order_by_forms_keep_their_sqlstate() {
    let (core, _g) = build("sql58o-reject");
    let ctx = ctx_for("tenant-a");
    let cases: [(&str, &str); 4] = [
        ("ORDER BY sum(count(*))", "42601"),
        ("ORDER BY array_agg(n)", "0A000"),
        ("ORDER BY sum(zzz)", "42703"),
        ("ORDER BY sum(b) + 1", "22000"),
    ];
    for (tail, code) in cases {
        let sql = format!("{BASE} {tail}");
        assert_eq!(run_err(&core, &ctx, &sql), code, "{sql}");
    }
    for sql in [
        "SELECT lang FROM docs GROUP BY lang ORDER BY lang",
        "SELECT DISTINCT lang FROM docs ORDER BY count(*)",
        "SELECT COUNT(*) FROM docs ORDER BY count(*)",
    ] {
        assert_eq!(run_err(&core, &ctx, sql), "42601", "{sql}");
    }
    for tail in ["ORDER BY array_agg(n)", "ORDER BY sum(b) + 1"] {
        let err = core
            .execute_sql(&ctx, &format!("{BASE} {tail}"))
            .expect_err("must fail");
        assert!(!format!("{err:?} {err}").contains('\u{1}'), "{tail}");
    }
}

#[test]
fn unsupported_contexts_fail_closed() {
    let (core, _g) = build("sql58o-contexts");
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    for ddl in [
        "CREATE VIEW vt AS SELECT title, lang FROM docs",
        "CREATE VIEW va AS SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang",
        "CREATE VIEW vr (l, t) AS SELECT lang, title FROM docs",
    ] {
        core.execute_sql_in_session(&ctx, &mut session, ddl)
            .unwrap_or_else(|e| panic!("{ddl}: {e:?}"));
    }
    for sql in [
        "CREATE VIEW vb AS SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang ORDER BY count(*)",
        "WITH x AS (SELECT lang FROM docs) SELECT lang FROM x GROUP BY lang ORDER BY count(*)",
        "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang ORDER BY count(*) \
         UNION ALL SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang",
        "SELECT id FROM docs WHERE n IN (SELECT n FROM docs GROUP BY n ORDER BY count(*))",
        "SELECT lang, COUNT(*) AS c FROM va GROUP BY lang ORDER BY count(*)",
    ] {
        assert_eq!(
            session_err(&core, &ctx, &mut session, sql),
            "42601",
            "{sql}"
        );
    }
    assert_eq!(
        session_err(
            &core,
            &ctx,
            &mut session,
            "SELECT l FROM vr GROUP BY l ORDER BY count(*)"
        ),
        "0A000"
    );
    // 単純形ビュー: 公開列への集計は受理、非公開列（n）は 42703。
    let ok = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT lang FROM vt GROUP BY lang ORDER BY count(*) DESC, lang",
        )
        .expect("aggregate over simple view");
    match ok {
        SqlOutcome::Query(rows) => assert_eq!(langs(&rows), ["en", "ja", "de", "fr"]),
        other => panic!("expected rows, got {other:?}"),
    }
    for sql in [
        "SELECT lang FROM vt GROUP BY lang ORDER BY sum(n)",
        "SELECT lang FROM vt GROUP BY lang ORDER BY coalesce(max(n), 0)",
    ] {
        let err = core
            .execute_sql_in_session(&ctx, &mut session, sql)
            .expect_err("must fail");
        assert_eq!(err.wire_code(), "42703", "{sql}");
        assert!(!format!("{err:?} {err}").contains('\u{1}'), "{sql}");
    }
}

#[test]
fn order_by_aggregates_respect_tenant_boundary() {
    let (core, _g) = build("sql58o-rls");
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    // tenant-a: ja の sum(n)=5 は en(6) より小さい。tenant-b は自身の Private 5 行（n=1）が
    // ja に加わるため ja が先頭に来る。他テナントの行は tenant-a の順位に影響しない。
    let q = format!("{BASE} ORDER BY sum(n) DESC NULLS LAST");
    assert_eq!(langs(&run(&core, &a, &q)), ["fr", "en", "ja", "de"]);
    assert_eq!(langs(&run(&core, &b, &q))[0], "ja");
}

#[test]
fn explain_of_order_by_aggregates() {
    let (core, _g) = build("sql58o-explain");
    let ctx = ctx_for("tenant-a");
    for body in [
        format!("{BASE} ORDER BY sum(count(*))"),
        format!("{BASE} ORDER BY array_agg(n)"),
        format!("{BASE} ORDER BY sum(zzz)"),
    ] {
        let mut s1 = SessionState::default();
        let plain = session_err(&core, &ctx, &mut s1, &body);
        let mut s2 = SessionState::default();
        let explained = session_err(&core, &ctx, &mut s2, &format!("EXPLAIN {body}"));
        assert_eq!(plain, explained, "sql={body}");
    }
    let mut s = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut s,
            &format!("EXPLAIN {BASE} ORDER BY sum(n) DESC"),
        )
        .expect("EXPLAIN of an accepted ORDER BY aggregate");
    match outcome {
        SqlOutcome::Explain(plan) => {
            assert!(!format!("{plan:?}").contains('\u{1}'));
        }
        other => panic!("expected explain, got {other:?}"),
    }
}
