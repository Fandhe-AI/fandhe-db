//! `SELECT`／`HAVING`／`ORDER BY` の式と `GROUP BY` キー式の構造一致の結合テスト
//! （Issue #1818、対象ビヘイビア: SQL-58。ポインタ: `docs/spec/04-behavior/` の SQL-58。集計の外での
//! `GROUP BY` に無い列の参照が `42803` になる改訂は ERR-6・SQL-14 の改訂注記のポインタ）。
//!
//! `tests/sql58_group_by_key_expressions.rs`（Issue #1817）と同じ流儀（`unique_db_path`＋
//! `CleanupGuard`、`EngineCore::execute_sql` の production 経路、Rust 側の独立オラクル）で、
//! 式の関数名の大文字小文字・括弧・別名・部分木の一致、一致しない式の `42803`／`42703`、
//! 従来契約（集計を持たない `GROUP BY` 文・非トップレベルは `42601`）の維持を固定する。
//! 期待値は PostgreSQL 17 の意味論で、fixture から手計算した値。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const DAY: i64 = 86_400_000_000;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("title", ColumnType::Text, true),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("ts", ColumnType::Timestamp, true),
        ],
    )
}

/// `(id, title, lang, n, ts)`。
type Row = (u64, &'static str, &'static str, Option<i32>, i64);

/// `tenant-a` の Public 行。`upper(lang)` は JA={1,3,5}・EN={2,4,7}・FR={6,8}、
/// `n / 2` は 0={2,7}・1={1,5,8}・2={4,6}・NULL={3}。
const ROWS: [Row; 8] = [
    (1, "a", "ja", Some(3), 0),
    (2, "b", "en", Some(1), DAY - 1),
    (3, "c", "JA", None, DAY),
    (4, "d", "en", Some(5), 2 * DAY - 1),
    (5, "e", "ja", Some(2), 2 * DAY),
    (6, "f", "FR", Some(4), 2 * DAY + 5),
    (7, "g", "en", Some(0), 3 * DAY - 1),
    (8, "h", "fr", Some(3), 2 * DAY + 9),
];

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn op_id(id: u64) -> OperationId {
    OperationId::parse(&format!("test-op-{id}")).expect("valid operation_id")
}

fn insert_row(storage: &Storage, ctx: &PolicyContext, visibility: Visibility, r: &Row) {
    let (id, title, lang, n, ts) = *r;
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(title.to_string()),
            Value::Text(lang.to_string()),
            n.map(Value::Integer).unwrap_or(Value::Null),
            Value::Timestamp(ts),
        ],
        &op_id(id),
    )
    .expect("insert row");
}

/// `tenant-a` に `ROWS`、`tenant-b` の Private 行（`n = 9` の 1 行だけが `10 / (n - 9)` を 0 除算にする）。
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
    insert_row(
        &storage,
        &b,
        Visibility::Private,
        &(101, "x", "zz", Some(1), 0),
    );
    insert_row(
        &storage,
        &b,
        Visibility::Private,
        &(102, "y", "zz", Some(9), 0),
    );
    (
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)),
        guard,
    )
}

fn a() -> PolicyContext {
    ctx_for("tenant-a")
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    core.execute_sql(&a(), sql)
        .unwrap_or_else(|e| panic!("query failed: {sql}: {e:?}"))
}

/// 失敗する文の `(wire_code, エラー文言)`。
fn run_err(core: &EngineCore, sql: &str) -> (&'static str, String) {
    let err = core
        .execute_sql(&a(), sql)
        .expect_err(&format!("query must fail: {sql}"));
    (err.wire_code(), format!("{err} / {err:?}"))
}

fn code(core: &EngineCore, sql: &str) -> &'static str {
    run_err(core, sql).0
}

/// セルの簡易表現（テスト比較用）。
#[derive(Debug, PartialEq)]
enum V {
    Null,
    Int(i64),
    Text(String),
    Ts(i64),
}

fn v(cell: &Cell) -> V {
    match cell {
        Cell::Null => V::Null,
        Cell::Integer(x) => V::Int(i64::try_from(*x).expect("fits")),
        Cell::SignedInteger(x) => V::Int(*x),
        Cell::Text(t) => V::Text(t.clone()),
        Cell::Timestamp(t) => V::Ts(*t),
        other => panic!("unexpected cell {other:?}"),
    }
}

fn rows(core: &EngineCore, sql: &str) -> Vec<Vec<V>> {
    run(core, sql)
        .rows
        .iter()
        .map(|r| r.cells.iter().map(v).collect())
        .collect()
}

fn t(s: &str) -> V {
    V::Text(s.to_string())
}

fn names(core: &EngineCore, sql: &str) -> Vec<String> {
    run(core, sql)
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Computed { name, .. } | ColumnMeta::Scalar { name, .. } => name.clone(),
            other => panic!("unexpected meta {other:?}"),
        })
        .collect()
}

#[test]
fn matching_select_item_is_projected_as_the_group_key() {
    let (core, _g) = build("sql58-gm-select");
    let expected = vec![
        vec![t("EN"), V::Int(3)],
        vec![t("FR"), V::Int(2)],
        vec![t("JA"), V::Int(3)],
    ];
    // 関数名の大文字小文字は構造一致の対象外（PostgreSQL と同じく関数名は大小無視）。
    for sql in [
        "SELECT upper(lang), count(*) FROM docs GROUP BY upper(lang) ORDER BY 1",
        "SELECT UPPER(lang), count(*) FROM docs GROUP BY upper(lang) ORDER BY 1",
        "SELECT upper(lang), count(*) FROM docs GROUP BY UPPER(lang) ORDER BY 1",
        "SELECT upper(lang), count(*) FROM docs GROUP BY upper(lang) ORDER BY upper(lang)",
    ] {
        assert_eq!(rows(&core, sql), expected, "{sql}");
    }
    // 既定の出力列名は PostgreSQL と同じ（関数名・`?column?`）。内部名は漏れない。
    assert_eq!(
        names(
            &core,
            "SELECT upper(lang), count(*) FROM docs GROUP BY upper(lang)"
        ),
        vec!["upper".to_string(), "count".to_string()]
    );
    assert_eq!(
        names(&core, "SELECT n / 2, count(*) FROM docs GROUP BY n / 2"),
        vec!["?column?".to_string(), "count".to_string()]
    );
}

#[test]
fn parentheses_and_aliases_do_not_affect_matching() {
    let (core, _g) = build("sql58-gm-paren");
    let expected = vec![
        vec![V::Int(0), V::Int(2)],
        vec![V::Int(1), V::Int(3)],
        vec![V::Int(2), V::Int(2)],
        vec![V::Null, V::Int(1)],
    ];
    for sql in [
        "SELECT (n / 2), count(*) FROM docs GROUP BY n / 2 ORDER BY 1",
        "SELECT n / 2, count(*) FROM docs GROUP BY (n / 2) ORDER BY 1",
        "SELECT n / 2 AS half, count(*) FROM docs GROUP BY n / 2 ORDER BY half",
        "SELECT n / 2 AS half, count(*) FROM docs GROUP BY n / 2 ORDER BY n / 2",
    ] {
        assert_eq!(rows(&core, sql), expected, "{sql}");
    }
    assert_eq!(
        names(
            &core,
            "SELECT n / 2 AS half, count(*) FROM docs GROUP BY n / 2"
        ),
        vec!["half".to_string(), "count".to_string()]
    );
}

#[test]
fn sub_expressions_of_a_select_item_may_match_the_key() {
    let (core, _g) = build("sql58-gm-sub");
    // n / 2 + 1: 整数の結果型のまま出力する（1・2・3・NULL）。
    assert_eq!(
        rows(
            &core,
            "SELECT n / 2 + 1, count(*) FROM docs GROUP BY n / 2 ORDER BY 1"
        ),
        vec![
            vec![V::Int(1), V::Int(2)],
            vec![V::Int(2), V::Int(3)],
            vec![V::Int(3), V::Int(2)],
            vec![V::Null, V::Int(1)],
        ]
    );
    assert_eq!(
        rows(
            &core,
            "SELECT concat(upper(lang), '!'), count(*) FROM docs GROUP BY upper(lang) ORDER BY 1"
        ),
        vec![
            vec![t("EN!"), V::Int(3)],
            vec![t("FR!"), V::Int(2)],
            vec![t("JA!"), V::Int(3)],
        ]
    );
    // 式キーと集計を 1 つの式に混在させる（集計の中は照合の対象外）。
    assert_eq!(
        rows(
            &core,
            "SELECT concat(upper(lang), max(title)), count(*) FROM docs GROUP BY upper(lang) ORDER BY 1"
        ),
        vec![
            vec![t("ENg"), V::Int(3)],
            vec![t("FRh"), V::Int(2)],
            vec![t("JAe"), V::Int(3)],
        ]
    );
}

#[test]
fn case_and_timestamp_keys_are_projected_with_their_types() {
    let (core, _g) = build("sql58-gm-types");
    assert_eq!(
        rows(
            &core,
            "SELECT CASE WHEN n > 2 THEN 'hi' ELSE 'lo' END AS b, count(*) FROM docs \
             GROUP BY CASE WHEN n > 2 THEN 'hi' ELSE 'lo' END ORDER BY b"
        ),
        vec![vec![t("hi"), V::Int(4)], vec![t("lo"), V::Int(4)]]
    );
    assert_eq!(
        rows(
            &core,
            "SELECT date_trunc('day', ts), count(*) FROM docs GROUP BY date_trunc('day', ts) ORDER BY 1"
        ),
        vec![
            vec![V::Ts(0), V::Int(2)],
            vec![V::Ts(DAY), V::Int(2)],
            vec![V::Ts(2 * DAY), V::Int(4)],
        ]
    );
}

#[test]
fn column_and_expression_keys_can_be_projected_together() {
    let (core, _g) = build("sql58-gm-mixed");
    assert_eq!(
        rows(
            &core,
            "SELECT lang, n / 2, count(*) FROM docs WHERE n IS NOT NULL AND n < 3 \
             GROUP BY lang, n / 2 ORDER BY lang, 2"
        ),
        vec![
            vec![t("en"), V::Int(0), V::Int(2)],
            vec![t("en"), V::Int(1), V::Int(0)]
                .into_iter()
                .take(0)
                .collect(),
        ]
        .into_iter()
        .filter(|r: &Vec<V>| !r.is_empty())
        .chain([vec![t("ja"), V::Int(1), V::Int(1)]])
        .collect::<Vec<_>>()
    );
}

#[test]
fn having_and_order_by_accept_matching_expressions() {
    let (core, _g) = build("sql58-gm-having");
    assert_eq!(
        rows(
            &core,
            "SELECT upper(lang), count(*) FROM docs GROUP BY upper(lang) \
             HAVING upper(lang) > 'EN' ORDER BY 1"
        ),
        vec![vec![t("FR"), V::Int(2)], vec![t("JA"), V::Int(3)]]
    );
    // 算術式の HAVING（関数・CASE を含まない形）。n / 2 = 2 のグループだけ。
    assert_eq!(
        rows(
            &core,
            "SELECT n / 2, count(*) FROM docs GROUP BY n / 2 HAVING n / 2 > 1"
        ),
        vec![vec![V::Int(2), V::Int(2)]]
    );
    // ORDER BY: 降順（NULL は先頭）・序数・キー式そのもの。
    assert_eq!(
        rows(
            &core,
            "SELECT n / 2, count(*) FROM docs GROUP BY n / 2 ORDER BY n / 2 DESC"
        ),
        vec![
            vec![V::Null, V::Int(1)],
            vec![V::Int(2), V::Int(2)],
            vec![V::Int(1), V::Int(3)],
            vec![V::Int(0), V::Int(2)],
        ]
    );
    assert_eq!(
        rows(
            &core,
            "SELECT upper(lang), count(*) FROM docs GROUP BY upper(lang) ORDER BY upper(lang) DESC"
        ),
        vec![
            vec![t("JA"), V::Int(3)],
            vec![t("FR"), V::Int(2)],
            vec![t("EN"), V::Int(3)],
        ]
    );
    // ORDER BY の式の中の部分木が一致する形（キーを含む式）。
    assert_eq!(
        rows(
            &core,
            "SELECT upper(lang), count(*) FROM docs GROUP BY upper(lang) ORDER BY lower(upper(lang)) DESC"
        ),
        vec![
            vec![t("JA"), V::Int(3)],
            vec![t("FR"), V::Int(2)],
            vec![t("EN"), V::Int(3)],
        ]
    );
}

#[test]
fn null_key_forms_one_group_with_a_null_output_value() {
    let (core, _g) = build("sql58-gm-null");
    let r = rows(
        &core,
        "SELECT n / 2, count(*) FROM docs GROUP BY n / 2 ORDER BY 1",
    );
    assert_eq!(r.last(), Some(&vec![V::Null, V::Int(1)]));
    assert_eq!(r.len(), 4);
}

#[test]
fn non_matching_columns_outside_an_aggregate_are_grouping_errors() {
    let (core, _g) = build("sql58-gm-reject");
    for sql in [
        "SELECT lang, count(*) FROM docs GROUP BY upper(lang)",
        "SELECT n + 1, count(*) FROM docs GROUP BY n / 2",
        "SELECT upper(lang), title, count(*) FROM docs GROUP BY upper(lang)",
        "SELECT upper(lang), count(*) FROM docs GROUP BY upper(lang) HAVING lang = 'en'",
        "SELECT upper(lang), count(*) FROM docs GROUP BY upper(lang) ORDER BY title",
        "SELECT upper(lang), count(*) FROM docs GROUP BY upper(lang) ORDER BY lower(lang)",
        "SELECT title, count(*) FROM docs GROUP BY lang",
        "SELECT lang, count(*) FROM docs",
        "SELECT id, count(*) FROM docs",
        "SELECT count(*), lang FROM docs",
        "SELECT n / 2, count(*) FROM docs",
    ] {
        let (c, msg) = run_err(&core, sql);
        assert_eq!(c, "42803", "{sql}");
        assert!(
            !msg.contains('\u{1}'),
            "internal name leaked: {sql}: {msg:?}"
        );
    }
    // 存在しない列は 42703（名前解決が先）。
    for sql in [
        "SELECT ghost, count(*) FROM docs GROUP BY upper(lang)",
        "SELECT ghost + 1, count(*) FROM docs GROUP BY n / 2",
        "SELECT upper(lang), count(*) FROM docs GROUP BY upper(lang) ORDER BY ghost",
    ] {
        let (c, msg) = run_err(&core, sql);
        assert_eq!(c, "42703", "{sql}");
        assert!(
            !msg.contains('\u{1}'),
            "internal name leaked: {sql}: {msg:?}"
        );
    }
}

#[test]
fn a_column_inside_an_aggregate_is_not_matched_against_the_keys() {
    let (core, _g) = build("sql58-gm-agg");
    // 集計の中では GROUP BY に無い列を参照できる（PostgreSQL と同じ）。
    assert_eq!(
        rows(
            &core,
            "SELECT upper(lang), max(title) FROM docs GROUP BY upper(lang) ORDER BY 1"
        ),
        vec![
            vec![t("EN"), t("g")],
            vec![t("FR"), t("h")],
            vec![t("JA"), t("e")],
        ]
    );
}

#[test]
fn legacy_contracts_are_kept() {
    let (core, _g) = build("sql58-gm-legacy");
    // 集計呼び出しを 1 つも持たない GROUP BY 文は従来どおり 42601（式キーでも同じ）。
    for sql in [
        "SELECT upper(lang) FROM docs GROUP BY upper(lang)",
        "SELECT lang FROM docs GROUP BY lang",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
    // 非トップレベル（CTE 主クエリ・集合演算の枝）は従来どおり構文段が 42601。
    for sql in [
        "WITH x AS (SELECT lang, n FROM docs) SELECT lang, count(*) FROM x",
        "WITH x AS (SELECT lang, n FROM docs) SELECT upper(lang), count(*) FROM x GROUP BY upper(lang)",
        "SELECT count(*) FROM docs UNION ALL SELECT upper(lang), count(*) FROM docs GROUP BY upper(lang)",
    ] {
        assert_eq!(code(&core, sql), "42601", "{sql}");
    }
}

#[test]
fn key_expression_is_not_evaluated_on_invisible_rows_when_projected() {
    let (core, _g) = build("sql58-gm-rls");
    // tenant-b の Private 行（n = 9）は tenant-a から不可視。10 / (n - 9) の 0 除算は起きず、
    // tenant-a の行（n - 9 は常に非 0）だけで評価される。
    let r = rows(
        &core,
        "SELECT 10 / (n - 9), count(*) FROM docs WHERE n IS NOT NULL GROUP BY 10 / (n - 9) ORDER BY 1",
    );
    // n = 0,1,2,3,3,4,5 → 10/(n-9) = -1,-1,-1,-1,-1,-2,-2（整数除算は 0 方向への丸め）。
    assert_eq!(
        r,
        vec![vec![V::Int(-2), V::Int(2)], vec![V::Int(-1), V::Int(5)]]
    );
}

#[test]
fn evaluation_error_fails_without_partial_rows() {
    let (core, _g) = build("sql58-gm-evalerr");
    // n = 0 の行（id 7）で 10 / n が 0 除算（22012）。部分結果を返さずに文全体が失敗する。
    assert_eq!(
        code(&core, "SELECT 10 / n, count(*) FROM docs GROUP BY 10 / n"),
        "22012"
    );
}

/// 括弧付き `ORDER BY` キーは、一致すれば受理し、一致しなければ fail-closed で拒否する（Issue #1818）。
#[test]
fn parenthesized_order_by_keys_are_matched_against_group_keys() {
    let (core, _g) = build("sql58-gm-order-paren");
    let expected = vec![
        vec![V::Null, V::Int(1)],
        vec![V::Int(2), V::Int(2)],
        vec![V::Int(1), V::Int(3)],
        vec![V::Int(0), V::Int(2)],
    ];
    assert_eq!(
        rows(
            &core,
            "SELECT n / 2, count(*) FROM docs GROUP BY n / 2 ORDER BY (n / 2) DESC"
        ),
        expected
    );
    let (c, _) = run_err(
        &core,
        "SELECT n / 2, count(*) FROM docs GROUP BY n / 2 ORDER BY (n + 1)",
    );
    assert_eq!(c, "42803");
}
