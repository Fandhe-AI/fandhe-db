//! `GROUP BY` 集計の `HAVING` 内の集計呼び出しの直接記述（`HAVING count(*) > 1` 等）の結合テスト
//! （Issue #1814、対象ビヘイビア: SQL-58。ポインタ: `docs/spec/04-behavior/` の SQL-58）。
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

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
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
const ROWS: [Row; 8] = [
    (1, Some("a"), "ja", Some(3), 10, 1.5),
    (2, Some("b"), "en", Some(1), 20, 2.5),
    (3, Some("c"), "ja", None, 30, 0.5),
    (4, Some("d"), "en", Some(5), 40, 3.5),
    (5, Some("e"), "ja", Some(2), 50, 1.0),
    (6, Some("f"), "fr", Some(4), 60, 2.0),
    (7, None, "en", Some(0), 70, 4.0),
    (8, Some("h"), "fr", Some(3), 80, 1.0),
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

fn check(core: &EngineCore, having: &str, expected: &[&str]) {
    let ctx = ctx_for("tenant-a");
    let sql = format!("{BASE} {having} ORDER BY lang");
    assert_eq!(langs(&run(core, &ctx, &sql)), expected, "{having}");
}

#[test]
fn having_count_family() {
    let (core, _g) = build("sql58-count");
    check(&core, "HAVING count(*) > 2", &["en", "ja"]);
    check(&core, "HAVING count(*) >= 3", &["en", "ja"]);
    check(&core, "HAVING count(*) = 2", &["fr"]);
    check(&core, "HAVING count(*) > -1", &["en", "fr", "ja"]);
    check(&core, "HAVING count(*) > 3", &[]);
    check(&core, "HAVING count(n) = 2", &["fr", "ja"]);
    check(&core, "HAVING count(DISTINCT n) = 2", &["fr", "ja"]);
    // 同じ集計を複数回書く（アキュムレータは 1 つにまとまる）。
    check(&core, "HAVING count(*) > 1 AND count(*) < 3", &["fr"]);
}

#[test]
fn having_sum_avg_min_max() {
    let (core, _g) = build("sql58-sum");
    // INTEGER の SUM は整数セル。
    check(&core, "HAVING sum(n) > 5", &["en", "fr"]);
    check(&core, "HAVING sum(n) = 5", &["ja"]);
    // BIGINT の SUM は NUMERIC セル（従来形の厳密比較へ脱糖される）。
    check(&core, "HAVING sum(b) > 100", &["en", "fr"]);
    check(&core, "HAVING sum(b) = 90", &["ja"]);
    // 整数列の AVG も NUMERIC セル。
    check(&core, "HAVING avg(n) > 2.0", &["fr", "ja"]);
    check(&core, "HAVING avg(n) = 2", &["en"]);
    check(&core, "HAVING avg(n) > 1.5", &["en", "fr", "ja"]);
    // DOUBLE の SUM／MIN／MAX。
    check(&core, "HAVING sum(f) > 3.5", &["en"]);
    check(&core, "HAVING max(n) >= 5", &["en"]);
    check(&core, "HAVING min(n) = 0", &["en"]);
    check(&core, "HAVING min(n) < 0", &[]);
    check(&core, "HAVING max(n) > -1", &["en", "fr", "ja"]);
}

#[test]
fn having_expression_forms() {
    let (core, _g) = build("sql58-expr");
    check(&core, "HAVING coalesce(max(n), 0) > 3", &["en", "fr"]);
    check(&core, "HAVING max(f) - min(f) > 1.2", &["en"]);
    check(&core, "HAVING count(*) * 2 > 5", &["en", "ja"]);
    check(&core, "HAVING sum(n) > max(n)", &["en", "fr", "ja"]);
    // 別名参照との併用。
    check(&core, "HAVING c > 2 AND sum(n) > 5", &["en"]);
    check(&core, "HAVING c > 2 AND coalesce(sum(n), 0) > 5", &["en"]);
}

#[test]
fn hidden_items_are_not_projected_and_are_shared() {
    let (core, _g) = build("sql58-projection");
    let ctx = ctx_for("tenant-a");
    // SELECT リストに無い集計で絞る: 出力列は SELECT リストどおり。
    let r = run(
        &core,
        &ctx,
        "SELECT lang, SUM(n) AS s FROM docs GROUP BY lang HAVING count(*) > 2 ORDER BY lang",
    );
    assert_eq!(r.columns.len(), 2);
    assert_eq!(langs(&r), ["en", "ja"]);
    assert!(r.rows.iter().all(|row| row.cells.len() == 2));
    assert_eq!(cell_i64(&r.rows[0].cells[1]), 6);
    assert_eq!(cell_i64(&r.rows[1].cells[1]), 5);
    // SELECT リストに集計項目が無い形。
    let r = run(
        &core,
        &ctx,
        "SELECT lang FROM docs GROUP BY lang HAVING count(*) > 2 ORDER BY lang",
    );
    assert_eq!(r.columns.len(), 1);
    assert_eq!(langs(&r), ["en", "ja"]);
    // SELECT リストと同じ集計は項目を共有する（結果が一致）。
    let r = run(
        &core,
        &ctx,
        "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang HAVING count(*) > 2 ORDER BY lang",
    );
    assert_eq!(r.columns.len(), 2);
    assert_eq!(cell_i64(&r.rows[0].cells[1]), 3);
}

#[test]
fn having_aggregates_combine_with_order_limit_offset_ties() {
    let (core, _g) = build("sql58-order");
    let ctx = ctx_for("tenant-a");
    let q = |tail: &str| {
        langs(&run(
            &core,
            &ctx,
            &format!("{BASE} HAVING sum(n) > 5 {tail}"),
        ))
    };
    assert_eq!(q("ORDER BY lang DESC LIMIT 1"), ["fr"]);
    assert_eq!(q("ORDER BY lang LIMIT 1 OFFSET 1"), ["fr"]);
    assert_eq!(
        q("ORDER BY c DESC, lang FETCH FIRST 1 ROWS WITH TIES"),
        ["en"]
    );
}

#[test]
fn rejected_forms_keep_their_sqlstate() {
    let (core, _g) = build("sql58-reject");
    let ctx = ctx_for("tenant-a");
    let cases: [(&str, &str); 9] = [
        // 入れ子の集計・GROUP BY なし・集計の外の非キー列。
        ("HAVING sum(count(*)) > 1", "42601"),
        ("HAVING lang = 'ja'", "42601"),
        // array_agg は比較不可。
        ("HAVING array_agg(n) > 1", "0A000"),
        // TEXT の MIN/MAX は数値比較できない。
        ("HAVING max(title) > 1", "22000"),
        // 未知の列。
        ("HAVING sum(zzz) > 1", "42703"),
        ("HAVING coalesce(sum(zzz), 0) > 1", "42703"),
        // ORDER BY 内の集計呼び出しは別 Issue（#1815）で扱う。
        ("ORDER BY sum(n)", "42601"),
        // 式内では NUMERIC 結果の算術は不可。
        ("HAVING sum(b) + 1 > 10", "22000"),
        // 括弧なしの列名参照は従来どおり未知名。
        ("HAVING count > 1", "42703"),
    ];
    for (tail, code) in cases {
        let sql = format!("{BASE} {tail}");
        assert_eq!(run_err(&core, &ctx, &sql), code, "{sql}");
    }
    for sql in [
        "SELECT COUNT(*) FROM docs HAVING count(*) > 1",
        "SELECT lang FROM docs GROUP BY lang",
    ] {
        assert_eq!(run_err(&core, &ctx, sql), "42601", "{sql}");
    }
    // エラー文言に内部名（制御文字）を出さない。
    for sql in [
        format!("{BASE} HAVING max(title) > 1"),
        format!("{BASE} HAVING array_agg(n) > 1"),
        format!("{BASE} HAVING sum(b) + 1 > 10"),
    ] {
        let err = core.execute_sql(&ctx, &sql).expect_err("must fail");
        assert!(!format!("{err:?} {err}").contains('\u{1}'), "{sql}");
    }
}

#[test]
fn aggregate_item_limit_counts_hidden_items() {
    let (core, _g) = build("sql58-limit");
    let ctx = ctx_for("tenant-a");
    let terms = |count: usize| {
        (0..count)
            .map(|k| format!("sum(n + {k})"))
            .collect::<Vec<_>>()
            .join(" + ")
    };
    // 隠し項目 32 個（SELECT リストに集計項目なし）は上限ちょうどで受理。
    let ok = format!(
        "SELECT lang FROM docs GROUP BY lang HAVING {} > 0",
        terms(32)
    );
    assert_eq!(langs(&run(&core, &ctx, &ok)).len(), 3);
    // 33 個は 54000。
    let over = format!(
        "SELECT lang FROM docs GROUP BY lang HAVING {} > 0",
        terms(33)
    );
    assert_eq!(run_err(&core, &ctx, &over), "54000");
    // 可視 1 ＋隠し 32（重複なし）も 54000。
    let over = format!(
        "SELECT lang, COUNT(*) FROM docs GROUP BY lang HAVING {} > 0",
        terms(32)
    );
    assert_eq!(run_err(&core, &ctx, &over), "54000");
}

#[test]
fn unsupported_contexts_fail_closed() {
    let (core, _g) = build("sql58-contexts");
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
    // ビュー本体・CTE 主クエリ・集合演算の枝・サブクエリ内部は 42601。
    for sql in [
        "CREATE VIEW vb AS SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang HAVING count(*) > 1",
        "WITH x AS (SELECT lang FROM docs) SELECT lang FROM x GROUP BY lang HAVING count(*) > 1",
        "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang HAVING count(*) > 1 \
         UNION ALL SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang",
        "SELECT id FROM docs WHERE n IN (SELECT n FROM docs GROUP BY n HAVING count(*) > 1)",
        // 評価後射影形ビューの外側集計。
        "SELECT lang, COUNT(*) AS c FROM va GROUP BY lang HAVING count(*) > 0",
    ] {
        assert_eq!(
            session_err(&core, &ctx, &mut session, sql),
            "42601",
            "{sql}"
        );
    }
    // 列別名ビューは 0A000。
    assert_eq!(
        session_err(
            &core,
            &ctx,
            &mut session,
            "SELECT l FROM vr GROUP BY l HAVING count(*) > 1"
        ),
        "0A000"
    );
    // 単純形ビュー: 公開列への集計は受理、非公開列（n）は 42703（filter oracle 防止）。
    let ok = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT lang FROM vt GROUP BY lang HAVING count(*) > 2 ORDER BY lang",
        )
        .expect("aggregate over simple view");
    match ok {
        SqlOutcome::Query(rows) => assert_eq!(langs(&rows), ["en", "ja"]),
        other => panic!("expected rows, got {other:?}"),
    }
    for sql in [
        "SELECT lang FROM vt GROUP BY lang HAVING sum(n) > 1",
        "SELECT lang FROM vt GROUP BY lang HAVING coalesce(max(n), 0) > 1",
    ] {
        assert_eq!(
            session_err(&core, &ctx, &mut session, sql),
            "42703",
            "{sql}"
        );
    }
}

#[test]
fn having_aggregates_respect_tenant_boundary() {
    let (core, _g) = build("sql58-rls");
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    // tenant-a の ja は 3 行。tenant-b の ja は Public 3 行＋自身の Private 5 行（sum(n) は 10）。
    // tenant-a から tenant-b の Private 行は集計に入らない。
    let a_rows = run(&core, &a, &format!("{BASE} HAVING count(*) > 3"));
    assert!(a_rows.rows.is_empty());
    let b_rows = run(&core, &b, &format!("{BASE} HAVING count(*) > 7"));
    assert_eq!(langs(&b_rows), ["ja"]);
    assert_eq!(cell_i64(&b_rows.rows[0].cells[1]), 8);
    let b_none = run(&core, &b, &format!("{BASE} HAVING sum(n) > 10"));
    assert!(b_none.rows.is_empty());
}

#[test]
fn explain_error_codes_match_non_explain() {
    let (core, _g) = build("sql58-explain");
    let ctx = ctx_for("tenant-a");
    for body in [
        format!("{BASE} HAVING sum(count(*)) > 1"),
        format!("{BASE} HAVING max(title) > 1"),
        format!("{BASE} HAVING array_agg(n) > 1"),
        format!("{BASE} HAVING sum(zzz) > 1"),
        format!("{BASE} ORDER BY sum(n)"),
    ] {
        let mut s1 = SessionState::default();
        let plain = session_err(&core, &ctx, &mut s1, &body);
        let mut s2 = SessionState::default();
        let explained = session_err(&core, &ctx, &mut s2, &format!("EXPLAIN {body}"));
        assert_eq!(plain, explained, "sql={body}");
    }
    let mut s = SessionState::default();
    let outcome = core
        .execute_sql_in_session(&ctx, &mut s, &format!("EXPLAIN {BASE} HAVING count(*) > 1"))
        .expect("EXPLAIN of an accepted HAVING aggregate");
    assert!(matches!(outcome, SqlOutcome::Explain(_)));
}
