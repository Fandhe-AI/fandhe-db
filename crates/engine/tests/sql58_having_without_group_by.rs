//! `GROUP BY` なしの `HAVING`（`SELECT count(*) FROM t HAVING count(*) > 1` 等）の結合テスト
//! （Issue #1819、対象ビヘイビア: SQL-58 (4)。ポインタ: `docs/spec/04-behavior/` の SQL-58）。
//!
//! `tests/sql58_having_aggregate_calls.rs` と同じ流儀（`unique_db_path`＋`CleanupGuard`、
//! `EngineCore::execute_sql` の production 経路、production の判定関数を呼ばない Rust 側の
//! 独立オラクル）で、表全体を 1 グループとして `HAVING` を当てた結果（真なら 1 行・偽か
//! `NULL` なら 0 行）・空テーブルでの挙動・隠し項目が出力列に出ないこと・エラー契約
//! （`wire_code`）・fail-closed の境界・RLS 境界を固定する。期待値は PostgreSQL 17 の意味論で、
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

/// `tenant-a` の Public 行。全体の真値（手計算）: count(*)=8, count(n)=7, sum(n)=18,
/// max(n)=5, min(n)=0, sum(b)=360。
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

/// `tenant-a` に `ROWS`、`tenant-b` の Private な 5 行（各 n=1。RLS 境界の検証用）を投入した core。
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

/// 行を 1 件も持たない表の core（空テーブルでの集計 1 行の検証用）。
fn build_empty(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
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

fn rows_of(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> usize {
    run(core, ctx, sql).rows.len()
}

fn cell_i64(cell: &Cell) -> i64 {
    match cell {
        Cell::Integer(v) => i64::try_from(*v).expect("fits i64"),
        Cell::SignedInteger(v) => *v,
        other => panic!("expected integer cell, got {other:?}"),
    }
}

#[test]
fn having_without_group_by_filters_the_single_row() {
    let (core, _g) = build("sql58-ungrouped");
    let ctx = ctx_for("tenant-a");
    // (HAVING 句, 期待行数)
    let cases: [(&str, usize); 18] = [
        ("HAVING count(*) > 7", 1),
        ("HAVING count(*) > 8", 0),
        ("HAVING count(*) = 8", 1),
        ("HAVING count(*) >= 9", 0),
        // SELECT リストに無い集計（隠し項目）。
        ("HAVING sum(n) = 18", 1),
        ("HAVING sum(n) > 18", 0),
        ("HAVING max(n) = 5 AND min(n) = 0", 1),
        ("HAVING max(n) = 5 AND min(n) = 1", 0),
        // 式形・集計呼び出しの直接記述。
        ("HAVING coalesce(sum(n), 0) + 1 > 18", 1),
        ("HAVING count(*) + 1 > 100", 0),
        // 集計を含まない HAVING。
        ("HAVING 1 = 1", 1),
        ("HAVING 1 = 2", 0),
        ("HAVING TRUE", 1),
        ("HAVING FALSE", 0),
        ("HAVING NULL", 0),
        ("HAVING TRUE AND count(*) > 7", 1),
        ("HAVING TRUE AND count(*) > 8", 0),
        // SELECT リストの別名の参照（GROUP BY ありと同じ解決器を共有。PostgreSQL は 42703）。
        ("HAVING c > 7", 1),
    ];
    for (tail, expected) in cases {
        let sql = format!("SELECT count(*) AS c FROM docs {tail}");
        assert_eq!(rows_of(&core, &ctx, &sql), expected, "{sql}");
    }
    // 真のとき 1 行・列は SELECT リストの項目だけ（隠し項目が出ない）。
    let result = run(
        &core,
        &ctx,
        "SELECT count(*) AS c FROM docs HAVING sum(n) = 18 AND max(b) > 0",
    );
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.columns.len(), 1);
    assert_eq!(result.rows[0].cells.len(), 1);
    assert_eq!(cell_i64(&result.rows[0].cells[0]), 8);
    // SELECT リストの集計と HAVING の集計の共有・複数項目。
    let result = run(
        &core,
        &ctx,
        "SELECT count(*), sum(n) FROM docs HAVING sum(n) > 10",
    );
    assert_eq!(result.columns.len(), 2);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(cell_i64(&result.rows[0].cells[1]), 18);
    // SELECT リストに集計式項目。
    let result = run(
        &core,
        &ctx,
        "SELECT count(*) + 1 FROM docs HAVING count(*) = 8",
    );
    assert_eq!(result.rows.len(), 1);
    assert_eq!(cell_i64(&result.rows[0].cells[0]), 9);
    // WHERE との併用: 全行除外しても 1 グループに HAVING が当たる。
    assert_eq!(
        rows_of(
            &core,
            &ctx,
            "SELECT count(*) FROM docs WHERE n > 100 HAVING count(*) = 0"
        ),
        1
    );
    assert_eq!(
        rows_of(
            &core,
            &ctx,
            "SELECT count(*) FROM docs WHERE n > 100 HAVING count(*) > 0"
        ),
        0
    );
    assert_eq!(
        rows_of(
            &core,
            &ctx,
            "SELECT count(*) FROM docs WHERE lang = 'ja' HAVING count(*) = 3"
        ),
        1
    );
}

#[test]
fn having_without_group_by_on_an_empty_table() {
    let (core, _g) = build_empty("sql58-ungrouped-empty");
    let ctx = ctx_for("tenant-a");
    // PostgreSQL 17: 可視行 0 件でも集計の 1 行（count は 0、他は NULL）が作られ、HAVING を当てる。
    let r = run(&core, &ctx, "SELECT count(*) FROM docs HAVING count(*) = 0");
    assert_eq!(r.rows.len(), 1);
    assert_eq!(cell_i64(&r.rows[0].cells[0]), 0);
    assert_eq!(
        rows_of(&core, &ctx, "SELECT count(*) FROM docs HAVING count(*) > 0"),
        0
    );
    // max(n) は NULL なので比較は NULL（0 行）。
    assert_eq!(
        rows_of(&core, &ctx, "SELECT count(*) FROM docs HAVING max(n) > 0"),
        0
    );
    let r = run(
        &core,
        &ctx,
        "SELECT sum(n) FROM docs HAVING coalesce(sum(n), -1) = -1",
    );
    assert_eq!(r.rows.len(), 1);
    assert!(matches!(r.rows[0].cells[0], Cell::Null));
    assert_eq!(
        rows_of(&core, &ctx, "SELECT sum(n) FROM docs HAVING 1 = 1"),
        1
    );
}

#[test]
fn having_without_group_by_classifies_ungrouped_columns() {
    let (core, _g) = build("sql58-ungrouped-errors");
    let ctx = ctx_for("tenant-a");
    let cases = [
        // SELECT リストの裸の列（GROUP BY なし）は集計の外の参照。
        ("SELECT lang FROM docs HAVING count(*) > 1", "42803"),
        (
            "SELECT lang, count(*) FROM docs HAVING count(*) > 1",
            "42803",
        ),
        ("SELECT id, count(*) FROM docs HAVING count(*) > 1", "42803"),
        ("SELECT count(*) + n FROM docs HAVING count(*) > 1", "42803"),
        // 存在しない列は名前解決が先に失敗する。
        (
            "SELECT zzz, count(*) FROM docs HAVING count(*) > 1",
            "42703",
        ),
        // HAVING 内の集計の外の列参照。
        ("SELECT count(*) FROM docs HAVING lang = 'ja'", "42803"),
        ("SELECT count(*) FROM docs HAVING n > 1", "42803"),
        ("SELECT count(*) FROM docs HAVING id > 1", "42803"),
        ("SELECT count(*) FROM docs HAVING count(*) > n", "42803"),
        ("SELECT count(*) FROM docs HAVING zzz > 1", "42703"),
        ("SELECT count(*) FROM docs HAVING sum(zzz) > 1", "42703"),
        // 比較できない・対応外の集計。
        ("SELECT count(*) FROM docs HAVING array_agg(n) > 1", "0A000"),
        ("SELECT count(*) FROM docs HAVING max(title) > 1", "22000"),
        ("SELECT count(*) FROM docs HAVING sum(b) + 1 > 10", "22000"),
        // 対応外の形は従来どおり 42601。
        ("SELECT count(*) FROM docs HAVING count(*) > $1", "42601"),
        ("SELECT * FROM docs HAVING count(*) > 1", "42601"),
        ("SELECT lang, count(*) FROM docs", "42601"),
        (
            "SELECT count(*) FROM docs HAVING count(*) > 1 ORDER BY 1",
            "42601",
        ),
        (
            "SELECT count(*) FROM docs HAVING count(*) > 1 LIMIT 1",
            "42601",
        ),
        (
            "SELECT count(*) FROM docs HAVING count(*) > 1 OFFSET 1",
            "42601",
        ),
        (
            "SELECT count(*) OVER () FROM docs HAVING count(*) > 1",
            "42601",
        ),
        (
            "SELECT count(*) FROM docs HAVING count(*) > 1 OR count(*) < 0",
            "42601",
        ),
        (
            "WITH x AS (SELECT lang FROM docs) SELECT count(*) FROM x HAVING count(*) > 1",
            "42601",
        ),
        (
            "SELECT count(*) FROM docs HAVING count(*) > 1 UNION ALL SELECT count(*) FROM docs",
            "42601",
        ),
        (
            "SELECT id FROM docs WHERE n IN (SELECT count(*) FROM docs HAVING count(*) > 1)",
            "42601",
        ),
    ];
    for (sql, code) in cases {
        assert_eq!(run_err(&core, &ctx, sql), code, "{sql}");
    }
    // エラー文言に内部名（制御文字）を出さない。
    for sql in [
        "SELECT lang FROM docs HAVING count(*) > 1",
        "SELECT count(*) FROM docs HAVING lang = 'ja'",
        "SELECT count(*) FROM docs HAVING max(title) > 1",
    ] {
        let err = core.execute_sql(&ctx, sql).expect_err("must fail");
        let text = format!("{err:?} {err}");
        assert!(!text.contains('\u{1}'), "{sql}");
    }
}

#[test]
fn having_without_group_by_aggregate_item_limit_counts_hidden_items() {
    let (core, _g) = build("sql58-ungrouped-limit");
    let ctx = ctx_for("tenant-a");
    let terms = |count: usize| {
        (0..count)
            .map(|k| format!("sum(n + {k})"))
            .collect::<Vec<_>>()
            .join(" + ")
    };
    // 可視 1 ＋隠し 31 で上限 32 ちょうどは受理、隠し 32（合計 33）は 54000。
    let ok = format!("SELECT count(*) FROM docs HAVING {} > 0", terms(31));
    assert_eq!(rows_of(&core, &ctx, &ok), 1);
    let over = format!("SELECT count(*) FROM docs HAVING {} > 0", terms(32));
    assert_eq!(run_err(&core, &ctx, &over), "54000");
}

#[test]
fn having_without_group_by_contexts_fail_closed_and_views() {
    let (core, _g) = build("sql58-ungrouped-contexts");
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    for ddl in [
        "CREATE VIEW vt AS SELECT title, lang FROM docs",
        "CREATE VIEW va AS SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang",
    ] {
        core.execute_sql_in_session(&ctx, &mut session, ddl)
            .unwrap_or_else(|e| panic!("{ddl}: {e:?}"));
    }
    // ビュー本体・評価後射影形ビューの外側は 42601。
    for sql in [
        "CREATE VIEW vb AS SELECT COUNT(*) AS c FROM docs HAVING count(*) > 1",
        "SELECT COUNT(*) FROM va HAVING count(*) > 0",
    ] {
        assert_eq!(
            session_err(&core, &ctx, &mut session, sql),
            "42601",
            "{sql}"
        );
    }
    // 単純形ビュー: 公開列への集計は受理、非公開列（n）は 42703（filter oracle 防止）。
    let ok = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT count(*) FROM vt HAVING count(*) = 8",
        )
        .expect("aggregate over simple view");
    match ok {
        SqlOutcome::Query(rows) => assert_eq!(rows.rows.len(), 1),
        other => panic!("expected rows, got {other:?}"),
    }
    for sql in [
        "SELECT count(*) FROM vt HAVING sum(n) > 1",
        "SELECT count(*) FROM vt HAVING coalesce(max(n), 0) > 1",
    ] {
        assert_eq!(
            session_err(&core, &ctx, &mut session, sql),
            "42703",
            "{sql}"
        );
    }
}

#[test]
fn having_without_group_by_respects_tenant_boundary() {
    let (core, _g) = build("sql58-ungrouped-rls");
    let a = ctx_for("tenant-a");
    let b = ctx_for("tenant-b");
    // tenant-a が見えるのは Public の 8 行（sum(n)=18）。tenant-b の Private 5 行（各 n=1）は
    // tenant-a の集計にも判定にも入らない。
    assert_eq!(
        rows_of(&core, &a, "SELECT count(*) FROM docs HAVING count(*) = 8"),
        1
    );
    assert_eq!(
        rows_of(&core, &a, "SELECT count(*) FROM docs HAVING count(*) > 8"),
        0
    );
    assert_eq!(
        rows_of(&core, &a, "SELECT count(*) FROM docs HAVING sum(n) = 18"),
        1
    );
    // tenant-b は Public 8 行＋自身の Private 5 行を見る（13 行・sum(n)=23）。
    assert_eq!(
        rows_of(&core, &b, "SELECT count(*) FROM docs HAVING count(*) = 13"),
        1
    );
    assert_eq!(
        rows_of(&core, &b, "SELECT count(*) FROM docs HAVING sum(n) = 23"),
        1
    );
    assert_eq!(
        rows_of(&core, &b, "SELECT count(*) FROM docs HAVING count(*) = 8"),
        0
    );
}

#[test]
fn having_without_group_by_explain_matches_non_explain() {
    let (core, _g) = build("sql58-ungrouped-explain");
    let ctx = ctx_for("tenant-a");
    for body in [
        "SELECT lang FROM docs HAVING count(*) > 1",
        "SELECT count(*) FROM docs HAVING lang = 'ja'",
        "SELECT count(*) FROM docs HAVING array_agg(n) > 1",
        "SELECT count(*) FROM docs HAVING sum(zzz) > 1",
    ] {
        let mut s1 = SessionState::default();
        let plain = session_err(&core, &ctx, &mut s1, body);
        let mut s2 = SessionState::default();
        let explained = session_err(&core, &ctx, &mut s2, &format!("EXPLAIN {body}"));
        assert_eq!(plain, explained, "sql={body}");
    }
    let mut s = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut s,
            "EXPLAIN SELECT count(*) FROM docs HAVING count(*) > 1",
        )
        .expect("EXPLAIN of an accepted HAVING without GROUP BY");
    assert!(matches!(outcome, SqlOutcome::Explain(_)));
}

/// 引用識別子 `"having"` を別名に使う既存の文は、`HAVING` 句として誤って振り分けられない。
#[test]
fn quoted_having_identifier_is_not_a_clause() {
    let (core, _g) = build("sql58-ungrouped-quoted");
    let ctx = ctx_for("tenant-a");
    for sql in [
        "SELECT count(*) AS \"having\" FROM docs",
        "SELECT lang AS \"having\" FROM docs LIMIT 3",
    ] {
        assert!(core.execute_sql(&ctx, sql).is_ok(), "{sql}");
    }
}
