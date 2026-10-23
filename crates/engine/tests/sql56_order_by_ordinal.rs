//! `ORDER BY` の序数指定（`ORDER BY 1, 2 DESC`）の結合テスト（Issue #1792、対象ビヘイビア:
//! SQL-56 (2)。ポインタ: `docs/spec/05-tasks.md` TASK-298・`docs/spec/04-behavior/
//! sql-surface.md` SQL-56）。
//!
//! `tests/sql26_order_by_having_expressions.rs` と同じ流儀（`unique_db_path`＋`CleanupGuard`、
//! `EngineCore::execute_sql` の production 経路、production の判定関数を呼ばない Rust 側の
//! 独立オラクル）で、序数が出力列（`*` 展開後・別名・式の項目）へ解決されること、範囲外が
//! `42P10`、序数にならない形（定数式・小数）が従来どおり `42601` であること、RLS 境界で
//! エラーの有無が可視行に依存しないことを固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use std::cmp::Ordering;

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
        ],
    )
}

/// オラクル用の行の真値（`insert_typed_row` へ渡す値と同じ）。
#[derive(Clone, Debug)]
struct Row {
    id: u64,
    title: Option<&'static str>,
    lang: &'static str,
    n: Option<i32>,
}

const ROWS: [Row; 8] = [
    row(1, Some("Banana"), "ja", Some(3)),
    row(2, Some("apple"), "en", Some(1)),
    row(3, Some("Cherry"), "ja", None),
    row(4, None, "en", Some(5)),
    row(5, Some("banana"), "ja", Some(2)),
    row(6, Some("Apple"), "fr", Some(4)),
    row(7, Some("cherry"), "en", Some(0)),
    row(8, Some("date"), "fr", Some(3)),
];

const fn row(id: u64, title: Option<&'static str>, lang: &'static str, n: Option<i32>) -> Row {
    Row { id, title, lang, n }
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn op_id(id: u64) -> OperationId {
    OperationId::parse(&format!("test-op-{id}")).expect("valid operation_id")
}

fn insert_row(storage: &Storage, ctx: &PolicyContext, r: &Row) {
    let text = |s: Option<&str>| s.map(|v| Value::Text(v.to_string())).unwrap_or(Value::Null);
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        ctx,
        r.id,
        Visibility::Public,
        &[
            Value::Vector(vec![r.id as f32, 0.0]),
            text(r.title),
            text(Some(r.lang)),
            r.n.map(Value::Integer).unwrap_or(Value::Null),
        ],
        &op_id(r.id),
    )
    .expect("insert row");
}

fn build(name: &str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    for r in &ROWS {
        insert_row(&storage, &ctx, r);
    }
    // JOIN 用の `labels(lang, label)`（言語ごとに 1 行）。
    storage
        .create_table(&TableSchema::new(
            "labels",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("label", ColumnType::Text, false),
            ],
        ))
        .expect("create labels");
    for (id, lang, label) in [
        (101u64, "ja", "Z-ja"),
        (102, "en", "Y-en"),
        (103, "fr", "X-fr"),
    ] {
        engine::tenant::insert_typed_row(
            &storage,
            "labels",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text(lang.to_string()),
                Value::Text(label.to_string()),
            ],
            &op_id(id),
        )
        .expect("insert label");
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

fn ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

/// PostgreSQL 既定の NULL 位置（ASC は末尾・DESC は先頭）で複数キーを辞書順に比較し、
/// 同点は `id` 昇順で確定する独立オラクル。`keys` は `(キー抽出, 降順か)` の並び。
type KeyFn = Box<dyn Fn(&Row) -> Option<String>>;
fn oracle(rows: &[Row], keys: &[(KeyFn, bool)]) -> Vec<u64> {
    let mut sorted: Vec<&Row> = rows.iter().collect();
    sorted.sort_by(|a, b| {
        for (key, descending) in keys {
            let ord = match (key(a), key(b)) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(x), Some(y)) => x.cmp(&y),
            };
            let ord = if *descending { ord.reverse() } else { ord };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        a.id.cmp(&b.id)
    });
    sorted.into_iter().map(|r| r.id).collect()
}

fn lang_key() -> KeyFn {
    Box::new(|r: &Row| Some(r.lang.to_string()))
}
/// 整数を辞書順で比較できるよう零埋めした文字列キー（`n` 列用）。
fn n_key() -> KeyFn {
    Box::new(|r: &Row| r.n.map(|v| format!("{v:05}")))
}
fn title_key() -> KeyFn {
    Box::new(|r: &Row| r.title.map(str::to_string))
}
fn lower_title_key() -> KeyFn {
    Box::new(|r: &Row| r.title.map(str::to_lowercase))
}

// ---------- 広域取得（単一表） ----------

#[test]
fn scan_ordinal_matches_column_name_order() {
    let (core, _g) = build("sql56-scan-basic");
    let ctx = ctx_for("tenant-a");
    let by_ordinal = run(
        &core,
        &ctx,
        "SELECT lang, n FROM docs ORDER BY 1, 2 DESC LIMIT 100",
    );
    let by_name = run(
        &core,
        &ctx,
        "SELECT lang, n FROM docs ORDER BY lang, n DESC LIMIT 100",
    );
    assert_eq!(ids(&by_ordinal), ids(&by_name));
    assert_eq!(
        ids(&by_ordinal),
        oracle(&ROWS, &[(lang_key(), false), (n_key(), true)])
    );
}

#[test]
fn scan_ordinal_resolves_by_position_not_by_name() {
    let (core, _g) = build("sql56-scan-swap");
    let ctx = ctx_for("tenant-a");
    // 出力 1 列目は `n AS lang`。列名 `lang` ではなく位置で `n` が使われる。
    let result = run(
        &core,
        &ctx,
        "SELECT n AS lang, lang AS n FROM docs ORDER BY 1 LIMIT 100",
    );
    assert_eq!(ids(&result), oracle(&ROWS, &[(n_key(), false)]));
}

#[test]
fn scan_ordinal_star_expands_with_pseudo_id_first() {
    let (core, _g) = build("sql56-scan-star");
    let ctx = ctx_for("tenant-a");
    let cols = run(&core, &ctx, "SELECT * FROM docs LIMIT 1");
    assert!(matches!(cols.columns.first(), Some(ColumnMeta::Id)));
    // 1 = id、2 = embedding（VECTOR は並べ替え不能 `22000`）、3 = title、4 = lang、5 = n。
    let by_id_desc = run(&core, &ctx, "SELECT * FROM docs ORDER BY 1 DESC LIMIT 100");
    let mut expected: Vec<u64> = ROWS.iter().map(|r| r.id).collect();
    expected.sort_by(|a, b| b.cmp(a));
    assert_eq!(ids(&by_id_desc), expected);
    let by_title = run(&core, &ctx, "SELECT * FROM docs ORDER BY 3 LIMIT 100");
    assert_eq!(ids(&by_title), oracle(&ROWS, &[(title_key(), false)]));
    let by_lang_n = run(
        &core,
        &ctx,
        "SELECT * FROM docs ORDER BY 4, 5 DESC LIMIT 100",
    );
    assert_eq!(
        ids(&by_lang_n),
        oracle(&ROWS, &[(lang_key(), false), (n_key(), true)])
    );
    assert_eq!(
        run_err(&core, &ctx, "SELECT * FROM docs ORDER BY 2 LIMIT 5"),
        "22000"
    );
    assert_eq!(
        run_err(&core, &ctx, "SELECT * FROM docs ORDER BY 6 LIMIT 5"),
        "42P10"
    );
}

#[test]
fn scan_ordinal_points_to_expression_item_value() {
    let (core, _g) = build("sql56-scan-expr");
    let ctx = ctx_for("tenant-a");
    // 別名なしの式項目・別名ありの式項目のどちらも式の値で並ぶ。
    let unaliased = run(
        &core,
        &ctx,
        "SELECT lower(title) FROM docs ORDER BY 1 LIMIT 100",
    );
    assert_eq!(
        ids(&unaliased),
        oracle(&ROWS, &[(lower_title_key(), false)])
    );
    let aliased = run(
        &core,
        &ctx,
        "SELECT id, lower(title) AS t FROM docs ORDER BY 2 DESC LIMIT 100",
    );
    assert_eq!(ids(&aliased), oracle(&ROWS, &[(lower_title_key(), true)]));
}

#[test]
fn scan_ordinal_with_limit_offset() {
    let (core, _g) = build("sql56-scan-limit");
    let ctx = ctx_for("tenant-a");
    let all = oracle(&ROWS, &[(n_key(), false)]);
    let page = run(
        &core,
        &ctx,
        "SELECT id, n FROM docs ORDER BY 2 LIMIT 3 OFFSET 2",
    );
    assert_eq!(ids(&page), all[2..5].to_vec());
}

#[test]
fn scan_ordinal_with_window_function_and_window_position_rejected() {
    let (core, _g) = build("sql56-scan-window");
    let ctx = ctx_for("tenant-a");
    let result = run(
        &core,
        &ctx,
        "SELECT lang, ROW_NUMBER() OVER () AS rn, n FROM docs ORDER BY 3, 1 LIMIT 100",
    );
    assert_eq!(
        ids(&result),
        oracle(&ROWS, &[(n_key(), false), (lang_key(), false)])
    );
    assert_eq!(
        run_err(
            &core,
            &ctx,
            "SELECT lang, ROW_NUMBER() OVER () AS rn FROM docs ORDER BY 2 LIMIT 100"
        ),
        "42601"
    );
    assert_eq!(
        run_err(
            &core,
            &ctx,
            "SELECT lang, ROW_NUMBER() OVER () AS rn FROM docs ORDER BY 3 LIMIT 100"
        ),
        "42P10"
    );
}

#[test]
fn scan_ordinal_out_of_range_is_42p10() {
    let (core, _g) = build("sql56-scan-range");
    let ctx = ctx_for("tenant-a");
    for order in ["0", "-1", "3", "1, 3", "2147483647"] {
        let sql = format!("SELECT lang, n FROM docs ORDER BY {order} LIMIT 10");
        assert_eq!(run_err(&core, &ctx, &sql), "42P10", "{sql}");
    }
}

#[test]
fn scan_non_ordinal_numeric_keys_keep_syntax_error() {
    let (core, _g) = build("sql56-scan-nonord");
    let ctx = ctx_for("tenant-a");
    // 定数式・小数・int4 範囲外の整数・パラメータは序数として扱わない。
    for order in ["1 + 0", "5 + 0", "1.5", "99999999999", "-1 + 0", "$1"] {
        let sql = format!("SELECT lang, n FROM docs ORDER BY {order} LIMIT 10");
        assert_eq!(run_err(&core, &ctx, &sql), "42601", "{sql}");
    }
}

#[test]
fn scan_ordinal_error_does_not_depend_on_visible_rows() {
    let (core, _g) = build("sql56-scan-rls");
    // 可視行が 0 件のテナントでも判定は同じ（エラーの有無が他テナントの行に依存しない）。
    // 他テナントの `Public` 行は Private のみ可視の ctx からは見えない。
    let other = PolicyContext::with_visibilities("tenant-empty", [Visibility::Private])
        .expect("valid tenant");
    let sql_bad = "SELECT lang, n FROM docs ORDER BY 3 LIMIT 10";
    assert_eq!(run_err(&core, &other, sql_bad), "42P10");
    assert_eq!(run_err(&core, &ctx_for("tenant-a"), sql_bad), "42P10");
    let ok = run(
        &core,
        &other,
        "SELECT lang, n FROM docs ORDER BY 1 LIMIT 10",
    );
    assert!(ok.rows.is_empty());
}

// ---------- GROUP BY 集計・SELECT DISTINCT ----------

fn first_texts(result: &QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Text(t)) => t.clone(),
            other => panic!("expected TEXT first cell, got {other:?}"),
        })
        .collect()
}

#[test]
fn aggregate_ordinal_resolves_keys_aggregates_and_expression_items() {
    let (core, _g) = build("sql56-agg");
    let ctx = ctx_for("tenant-a");
    // c: ja=3, en=3, fr=2 / s: ja=5, en=6, fr=7（`n` の NULL は SUM から除外）。
    let by_key = run(
        &core,
        &ctx,
        "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang ORDER BY 1 DESC",
    );
    assert_eq!(first_texts(&by_key), vec!["ja", "fr", "en"]);
    let by_sum = run(
        &core,
        &ctx,
        "SELECT lang, SUM(n) AS s FROM docs GROUP BY lang ORDER BY 2 DESC",
    );
    assert_eq!(first_texts(&by_sum), vec!["fr", "en", "ja"]);
    // 序数は列名ではなく位置: 1 列目の集計 `c` の降順、同点は 2 列目のキー昇順。
    let by_count_then_key = run(
        &core,
        &ctx,
        "SELECT COUNT(*) AS c, lang FROM docs GROUP BY lang ORDER BY 1 DESC, 2",
    );
    let langs: Vec<String> = by_count_then_key
        .rows
        .iter()
        .map(|r| match r.cells.get(1) {
            Some(Cell::Text(t)) => t.clone(),
            other => panic!("expected TEXT key, got {other:?}"),
        })
        .collect();
    assert_eq!(langs, vec!["en", "ja", "fr"]);
    // 集計式の項目（Issue #1541）: SUM(n) * -1 の昇順は SUM の降順と一致する。
    let by_expr = run(
        &core,
        &ctx,
        "SELECT lang, SUM(n) * -1 AS neg FROM docs GROUP BY lang ORDER BY 2",
    );
    assert_eq!(first_texts(&by_expr), vec!["fr", "en", "ja"]);
}

#[test]
fn aggregate_ordinal_out_of_range_is_42p10() {
    let (core, _g) = build("sql56-agg-range");
    let ctx = ctx_for("tenant-a");
    for order in ["0", "-1", "3"] {
        let sql = format!("SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang ORDER BY {order}");
        assert_eq!(run_err(&core, &ctx, &sql), "42P10", "{sql}");
    }
    assert_eq!(
        run_err(
            &core,
            &ctx,
            "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang ORDER BY 1 + 0"
        ),
        "42601"
    );
}

#[test]
fn distinct_ordinal_orders_and_rejects_out_of_range() {
    let (core, _g) = build("sql56-distinct");
    let ctx = ctx_for("tenant-a");
    let sorted = run(
        &core,
        &ctx,
        "SELECT DISTINCT lang FROM docs ORDER BY 1 DESC",
    );
    assert_eq!(first_texts(&sorted), vec!["ja", "fr", "en"]);
    assert_eq!(
        run_err(&core, &ctx, "SELECT DISTINCT lang FROM docs ORDER BY 2"),
        "42P10"
    );
}

// ---------- JOIN ----------

#[test]
fn join_ordinal_plain_star_and_aggregate() {
    let (core, _g) = build("sql56-join");
    let ctx = ctx_for("tenant-a");
    // 出力: docs.id(1), docs.title(2), labels.label(3)。label 降順は ja(Z)・en(Y)・fr(X)。
    let plain = run(
        &core,
        &ctx,
        "SELECT docs.id, docs.title, labels.label FROM docs \
         JOIN labels ON docs.lang = labels.lang ORDER BY 3 DESC, 1 LIMIT 100",
    );
    let label_rank = |lang: &str| match lang {
        "ja" => 0,
        "en" => 1,
        _ => 2,
    };
    let mut expected: Vec<&Row> = ROWS.iter().collect();
    expected.sort_by_key(|r| (label_rank(r.lang), r.id));
    assert_eq!(
        plain.rows.iter().map(|r| r.id).collect::<Vec<_>>().len(),
        expected.len()
    );
    let first_ids: Vec<u64> = plain
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Numeric(n)) => n.to_string().parse().expect("id"),
            Some(Cell::Integer(n)) => *n,
            other => panic!("unexpected id cell {other:?}"),
        })
        .collect();
    assert_eq!(first_ids, expected.iter().map(|r| r.id).collect::<Vec<_>>());
    // `*`: docs(id, embedding, title, lang, n) の 5 列の後に labels(id, embedding, lang, label)。
    // 3 = docs.title。
    let star = run(
        &core,
        &ctx,
        "SELECT * FROM docs JOIN labels ON docs.lang = labels.lang ORDER BY 3, 1 LIMIT 100",
    );
    let star_first: Vec<u64> = star
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Numeric(n)) => n.to_string().parse().expect("id"),
            Some(Cell::Integer(n)) => *n,
            other => panic!("unexpected id cell {other:?}"),
        })
        .collect();
    assert_eq!(star_first, oracle(&ROWS, &[(title_key(), false)]));
    // 集計形。
    let agg = run(
        &core,
        &ctx,
        "SELECT docs.lang, COUNT(*) AS c FROM docs \
         JOIN labels ON docs.lang = labels.lang GROUP BY docs.lang ORDER BY 2 DESC, 1 LIMIT 10",
    );
    assert_eq!(first_texts(&agg), vec!["en", "ja", "fr"]);
}

#[test]
fn join_ordinal_out_of_range_is_42p10() {
    let (core, _g) = build("sql56-join-range");
    let ctx = ctx_for("tenant-a");
    for order in ["0", "4"] {
        let sql = format!(
            "SELECT docs.id, docs.title, labels.label FROM docs \
             JOIN labels ON docs.lang = labels.lang ORDER BY {order} LIMIT 10"
        );
        assert_eq!(run_err(&core, &ctx, &sql), "42P10", "{sql}");
    }
}

// ---------- ビュー・INSERT ... SELECT・EXPLAIN ----------

#[test]
fn view_ordinal_follows_public_column_order() {
    let (core, _g) = build("sql56-view");
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE VIEW v AS SELECT n, lang FROM docs",
    )
    .expect("create view");
    // 公開列の 2 列目は `lang`（基表の 2 列目 `embedding` や `title` ではない）。
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT * FROM v ORDER BY 2, 1 LIMIT 100",
        )
        .expect("ordinal over view");
    let SqlOutcome::Query(rows) = outcome else {
        panic!("expected rows");
    };
    assert_eq!(
        rows.rows.iter().map(|r| r.id).collect::<Vec<_>>(),
        oracle(&ROWS, &[(lang_key(), false), (n_key(), false)])
    );
    let err = core
        .execute_sql_in_session(&ctx, &mut session, "SELECT * FROM v ORDER BY 3 LIMIT 100")
        .expect_err("out of range over view");
    assert_eq!(err.wire_code(), "42P10");
}

#[test]
fn explain_matches_non_explain_for_ordinal() {
    let (core, _g) = build("sql56-explain");
    let ctx = ctx_for("tenant-a");
    for (body, code) in [
        ("SELECT lang, n FROM docs ORDER BY 1, 2 DESC LIMIT 5", None),
        ("SELECT lang, n FROM docs ORDER BY 3 LIMIT 5", Some("42P10")),
        (
            "SELECT lang, n FROM docs ORDER BY 1 + 0 LIMIT 5",
            Some("42601"),
        ),
    ] {
        let mut session = SessionState::default();
        let result = core.execute_sql_in_session(&ctx, &mut session, &format!("EXPLAIN {body}"));
        match (result, code) {
            (Ok(SqlOutcome::Explain(_)), None) => {}
            (Err(e), Some(code)) => assert_eq!(e.wire_code(), code, "{body}"),
            (other, _) => panic!("unexpected EXPLAIN result for {body}: {other:?}"),
        }
    }
}
