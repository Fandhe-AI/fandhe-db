//! 広域取得・`GROUP BY` 集計の `ORDER BY`／`HAVING` の式位置（スカラー関数呼び出し・
//! `CASE`・`COALESCE`・`NULLIF`・`EXTRACT`）の結合テスト（Issue #1188、対象ビヘイビア:
//! SQL-26。ポインタ: `docs/spec/05-tasks.md` TASK-210・`docs/spec/04-behavior/
//! sql-surface.md` SQL-26）。
//!
//! `tests/sql25_scalar_order_by.rs`・`tests/sql25_aggregate_order_by_scalar_keys.rs` と同じ流儀
//! （`unique_db_path`＋`CleanupGuard`、`EngineCore::execute_sql` の production 経路、
//! production の判定関数を呼ばない Rust 側の独立オラクル）で、並び順・NULL 位置・
//! `LIMIT`／`OFFSET`・`HAVING` の 3 値論理・エラー契約（`wire_code`）・RLS 境界・ビュー列
//! スコープ・可視行 0 件でのエラー非発生（Issue #353）を固定する。

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
use std::cmp::Ordering;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const JAN_1_2020: i32 = 18_262;
const JAN_1_2021: i32 = 18_628;
const JAN_1_2022: i32 = 18_993;
const JAN_1_2023: i32 = 19_358;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("title", ColumnType::Text, true),
            ColumnDef::new("lang", ColumnType::Text, true),
            ColumnDef::new("n", ColumnType::Integer, true),
            ColumnDef::new("d", ColumnType::Date, true),
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
    /// `(年, その年の 1 月 1 日からの日数オフセット)`。
    d: Option<(i32, i32)>,
}

const ROWS: [Row; 8] = [
    row(1, Some("Banana"), "ja", Some(3), Some((2021, 10))),
    row(2, Some("apple"), "en", Some(1), Some((2020, 5))),
    row(3, Some("Cherry"), "ja", None, Some((2022, 0))),
    row(4, None, "en", Some(5), Some((2021, 200))),
    row(5, Some("banana"), "ja", Some(2), None),
    row(6, Some("Apple"), "fr", Some(4), Some((2023, 1))),
    row(7, Some("cherry"), "en", Some(0), Some((2020, 100))),
    row(8, Some("date"), "fr", Some(3), Some((2022, 250))),
];

const fn row(
    id: u64,
    title: Option<&'static str>,
    lang: &'static str,
    n: Option<i32>,
    d: Option<(i32, i32)>,
) -> Row {
    Row {
        id,
        title,
        lang,
        n,
        d,
    }
}

fn jan_1(year: i32) -> i32 {
    match year {
        2020 => JAN_1_2020,
        2021 => JAN_1_2021,
        2022 => JAN_1_2022,
        2023 => JAN_1_2023,
        other => panic!("unexpected year {other}"),
    }
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

/// 他テナントの `Public` 行を可視としない ctx。
fn private_only_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Private]).expect("valid tenant")
}

fn op_id(id: u64) -> OperationId {
    OperationId::parse(&format!("test-op-{id}")).expect("valid operation_id")
}

fn insert_row(storage: &Storage, ctx: &PolicyContext, visibility: Visibility, r: &Row) {
    let text = |s: Option<&str>| s.map(|v| Value::Text(v.to_string())).unwrap_or(Value::Null);
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        ctx,
        r.id,
        visibility,
        &[
            // `vec_norm(embedding)` が id と一致するよう `[id, 0]` を入れる。
            Value::Vector(vec![r.id as f32, 0.0]),
            text(r.title),
            text(Some(r.lang)),
            r.n.map(Value::Integer).unwrap_or(Value::Null),
            r.d.map(|(y, off)| Value::Date(jan_1(y) + off))
                .unwrap_or(Value::Null),
        ],
        &op_id(r.id),
    )
    .expect("insert row");
}

/// テナント `tenant-a` の Public 行として `ROWS` を、追加で `extra`（Private 行）を投入した
/// core を作る。
fn build(name: &str, extra: &[Row]) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx = ctx_for("tenant-a");
    for r in &ROWS {
        insert_row(&storage, &ctx, Visibility::Public, r);
    }
    for r in extra {
        insert_row(&storage, &ctx, Visibility::Private, r);
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

/// PostgreSQL 既定の NULL 位置（ASC は末尾・DESC は先頭）で `key` 昇順／降順に並べ、
/// 同点は `id` 昇順で確定する独立オラクル。
fn oracle_order<K: Ord + Clone>(
    rows: &[Row],
    descending: bool,
    key: impl Fn(&Row) -> Option<K>,
) -> Vec<u64> {
    let mut sorted: Vec<&Row> = rows.iter().collect();
    sorted.sort_by(|a, b| {
        let ord = match (key(a), key(b)) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(x), Some(y)) => x.cmp(&y),
        };
        let ord = if descending { ord.reverse() } else { ord };
        ord.then(a.id.cmp(&b.id))
    });
    sorted.into_iter().map(|r| r.id).collect()
}

fn lower_title(r: &Row) -> Option<String> {
    r.title.map(str::to_lowercase)
}

// ---------- 広域取得の ORDER BY 式キー ----------

#[test]
fn scan_order_by_scalar_function_matches_oracle_including_null_position() {
    let (core, _g) = build("sql26-ob-fn", &[]);
    let ctx = ctx_for("tenant-a");
    let asc = run(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY lower(title) LIMIT 100",
    );
    assert_eq!(ids(&asc), oracle_order(&ROWS, false, lower_title));
    let desc = run(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY lower(title) DESC LIMIT 100",
    );
    assert_eq!(ids(&desc), oracle_order(&ROWS, true, lower_title));
    // 同一クエリの反復は決定的。
    let again = run(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY lower(title) LIMIT 100",
    );
    assert_eq!(ids(&asc), ids(&again));
}

#[test]
fn scan_order_by_extract_and_case_and_coalesce() {
    let (core, _g) = build("sql26-ob-extract", &[]);
    let ctx = ctx_for("tenant-a");
    let year = |r: &Row| r.d.map(|(y, _)| y);

    let by_year = run(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY EXTRACT(year FROM d) DESC, id LIMIT 100",
    );
    // 年降順・同点は id 昇順（`id` キーの明示と暗黙 tie-break は一致する）。
    assert_eq!(ids(&by_year), oracle_order(&ROWS, true, year));
    let by_year_asc = run(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY EXTRACT(year FROM d) LIMIT 100",
    );
    assert_eq!(ids(&by_year_asc), oracle_order(&ROWS, false, year));

    // CASE: n > 2 の行（n が NULL の行は ELSE）が先。
    let case = run(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY CASE WHEN n > 2 THEN 0 ELSE 1 END, id LIMIT 100",
    );
    assert_eq!(
        ids(&case),
        oracle_order(&ROWS, false, |r| Some(if r.n.is_some_and(|n| n > 2) {
            0
        } else {
            1
        }))
    );

    // COALESCE: NULL タイトルは空文字として最小になる。
    let coalesce = run(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY COALESCE(lower(title), '') LIMIT 100",
    );
    assert_eq!(
        ids(&coalesce),
        oracle_order(&ROWS, false, |r| Some(lower_title(r).unwrap_or_default()))
    );

    // NULLIF: 'apple' を NULL 化した結果で並べる（NULL は末尾）。
    let nullif = run(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY NULLIF(lower(title), 'apple') LIMIT 100",
    );
    assert_eq!(
        ids(&nullif),
        oracle_order(&ROWS, false, |r| lower_title(r).filter(|t| t != "apple"))
    );
}

/// 式が embedding・数値列を参照しても、デコード段階が式の参照列を含む。
#[test]
fn scan_order_by_expression_over_embedding_and_numeric_column() {
    let (core, _g) = build("sql26-ob-embedding", &[]);
    let ctx = ctx_for("tenant-a");
    let dist = run(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY abs(vec_norm(embedding) - 4), id LIMIT 100",
    );
    assert_eq!(ids(&dist), vec![4, 3, 5, 2, 6, 1, 7, 8]);
    let desc = run(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY vec_norm(embedding) DESC LIMIT 100",
    );
    assert_eq!(ids(&desc), vec![8, 7, 6, 5, 4, 3, 2, 1]);
    let numeric = run(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY abs(n - 3) DESC, id LIMIT 100",
    );
    // |n-3|: 1→0, 2→2, 3→NULL, 4→2, 5→1, 6→1, 7→3, 8→0。DESC は NULL 先頭。
    assert_eq!(ids(&numeric), vec![3, 7, 2, 4, 5, 6, 1, 8]);
}

#[test]
fn scan_order_by_expression_composes_with_limit_offset_and_column_keys() {
    let (core, _g) = build("sql26-ob-limit", &[]);
    let ctx = ctx_for("tenant-a");
    let all = oracle_order(&ROWS, false, lower_title);
    let page = run(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY lower(title) LIMIT 3 OFFSET 2",
    );
    assert_eq!(ids(&page), all[2..5].to_vec());

    // 列キーとの混在（先頭が列キー・後続が式キー）。
    let mixed = run(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY lang, lower(title) DESC LIMIT 100",
    );
    let mut expected: Vec<&Row> = ROWS.iter().collect();
    expected.sort_by(|a, b| {
        a.lang
            .cmp(b.lang)
            .then_with(|| match (lower_title(a), lower_title(b)) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
                (Some(x), Some(y)) => y.cmp(&x),
            })
            .then(a.id.cmp(&b.id))
    });
    assert_eq!(
        ids(&mixed),
        expected.iter().map(|r| r.id).collect::<Vec<_>>()
    );
}

#[test]
fn scan_order_by_expression_error_contract() {
    let (core, _g) = build("sql26-ob-errors", &[]);
    let ctx = ctx_for("tenant-a");
    // ベクトル順位付けとの混在・集計関数・位置指定は構文段で 42601。
    for sql in [
        "SELECT id FROM docs ORDER BY lower(title), embedding <=> '[1.0,0.0]' LIMIT 3",
        "SELECT id FROM docs ORDER BY lower(title) <=> '[1.0,0.0]' LIMIT 3",
        "SELECT id FROM docs ORDER BY count(*) LIMIT 3",
        "SELECT id FROM docs ORDER BY 1 LIMIT 3",
        "SELECT id FROM docs ORDER BY lower(title) LIMIT",
    ] {
        assert_eq!(run_err(&core, &ctx, sql), "42601", "{sql}");
    }
    // 未知の列・VECTOR 型の式は 22000。
    for sql in [
        "SELECT id FROM docs ORDER BY lower(nosuchcol) LIMIT 3",
        "SELECT id FROM docs ORDER BY vec_div(embedding, 2) LIMIT 3",
    ] {
        assert_eq!(run_err(&core, &ctx, sql), "22000", "{sql}");
    }
    // 未知の関数名は構文エラー（42601）にはならず束縛段で拒否される。
    let unknown = run_err(
        &core,
        &ctx,
        "SELECT id FROM docs ORDER BY nosuchfn(title) LIMIT 3",
    );
    assert_ne!(unknown, "42601");
    // 可視行の評価でゼロ除算（22012）が発生する。
    assert_eq!(
        run_err(
            &core,
            &ctx,
            "SELECT id FROM docs ORDER BY abs(1 / (n - n)) LIMIT 100"
        ),
        "22012"
    );
}

/// 可視行 0 件なら式は一度も評価されず、エラーにならない（Issue #353 の契約）。
#[test]
fn scan_order_by_expression_is_not_evaluated_without_visible_rows() {
    let (core, _g) = build("sql26-ob-empty", &[]);
    let nobody = private_only_ctx("tenant-z");
    let result = run(
        &core,
        &nobody,
        "SELECT id FROM docs ORDER BY abs(1 / (n - n)) LIMIT 100",
    );
    assert!(result.rows.is_empty());
}

/// RLS: 他テナントの Private 行の有無で、結果・順序・エラー内容が変わらない。
#[test]
fn scan_order_by_expression_does_not_leak_other_tenants_private_rows() {
    let secret = Row {
        id: 100,
        title: Some("AAA-secret"),
        lang: "ja",
        n: Some(0),
        d: Some((2020, 0)),
    };
    let (without, _g1) = build("sql26-ob-rls-a", &[]);
    let (with_secret, _g2) = build("sql26-ob-rls-b", std::slice::from_ref(&secret));
    let other = ctx_for("tenant-b");
    for sql in [
        "SELECT id FROM docs ORDER BY lower(title) LIMIT 100",
        "SELECT id FROM docs ORDER BY lower(title) DESC LIMIT 3",
        "SELECT id FROM docs ORDER BY EXTRACT(year FROM d), id LIMIT 100",
        "SELECT id FROM docs ORDER BY COALESCE(lower(title), '') LIMIT 2 OFFSET 1",
    ] {
        assert_eq!(
            ids(&run(&without, &other, sql)),
            ids(&run(&with_secret, &other, sql)),
            "{sql}"
        );
    }
    // 所有テナント自身からは Private 行も並べ替えに参加する。
    let owner = ctx_for("tenant-a");
    let first = run(
        &with_secret,
        &owner,
        "SELECT id FROM docs ORDER BY lower(title) LIMIT 1",
    );
    assert_eq!(ids(&first), vec![100]);
    // 他テナントのエラー応答に秘密値を含めない。
    let err = with_secret
        .execute_sql(
            &other,
            "SELECT id FROM docs ORDER BY abs(1 / (n - n)) LIMIT 100",
        )
        .expect_err("division by zero over visible rows");
    assert_eq!(err.wire_code(), "22012");
    assert!(!format!("{err:?}").contains("AAA-secret"));
}

/// ビュー経由の広域取得では、式キーもビューの公開列に限定される。
#[test]
fn scan_order_by_expression_respects_view_column_scope() {
    let (core, _g) = build("sql26-ob-view", &[]);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE VIEW v AS SELECT title, n FROM docs",
    )
    .expect("create view");
    let ok = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT title FROM v ORDER BY lower(title) LIMIT 3",
        )
        .expect("view-visible column in ORDER BY expression");
    match ok {
        SqlOutcome::Query(rows) => assert_eq!(rows.rows.len(), 3),
        other => panic!("expected rows, got {other:?}"),
    }
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT title FROM v ORDER BY lower(lang) LIMIT 3",
        )
        .expect_err("hidden column must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

// ---------- GROUP BY 集計の HAVING／ORDER BY 式 ----------

fn lang_rows(result: &QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Text(t)) => t.clone(),
            other => panic!("expected TEXT group key, got {other:?}"),
        })
        .collect()
}

const AGG: &str = "SELECT lang, COUNT(*) AS c, SUM(n) AS s FROM docs GROUP BY lang";

#[test]
fn having_accepts_scalar_function_and_case_predicates() {
    let (core, _g) = build("sql26-having", &[]);
    let ctx = ctx_for("tenant-a");
    // c: ja=3, en=3, fr=2 / s: ja=5, en=6, fr=7。
    let cases: [(&str, Vec<&str>); 6] = [
        ("HAVING abs(c - 3) < 1", vec!["en", "ja"]),
        (
            "HAVING CASE WHEN s > 5 THEN 1 ELSE 0 END = 1",
            vec!["en", "fr"],
        ),
        ("HAVING lower(lang) = 'ja'", vec!["ja"]),
        ("HAVING c > 2 AND lower(lang) = 'en'", vec!["en"]),
        ("HAVING COALESCE(s, 0) >= 6", vec!["en", "fr"]),
        ("HAVING NULLIF(c, 3) > 0", vec!["fr"]),
    ];
    for (having, expected) in cases {
        let result = run(
            &core,
            &ctx,
            &format!("{AGG} {having} ORDER BY lang LIMIT 10"),
        );
        assert_eq!(lang_rows(&result), expected, "{having}");
    }
    // 従来形（数値リテラル比較）は無変更で通る。
    let legacy = run(&core, &ctx, &format!("{AGG} HAVING c > 2 ORDER BY lang"));
    assert_eq!(lang_rows(&legacy), vec!["en", "ja"]);
}

#[test]
fn aggregate_order_by_accepts_expression_keys() {
    let (core, _g) = build("sql26-agg-order", &[]);
    let ctx = ctx_for("tenant-a");
    let cases: [(&str, Vec<&str>); 4] = [
        (
            "ORDER BY CASE WHEN c > 2 THEN 0 ELSE 1 END, lang",
            vec!["en", "ja", "fr"],
        ),
        ("ORDER BY lower(lang) DESC", vec!["ja", "fr", "en"]),
        ("ORDER BY abs(s - 6), lang", vec!["en", "fr", "ja"]),
        ("ORDER BY COALESCE(s, 0) DESC", vec!["fr", "en", "ja"]),
    ];
    for (order, expected) in cases {
        let result = run(&core, &ctx, &format!("{AGG} {order}"));
        assert_eq!(lang_rows(&result), expected, "{order}");
    }
    // HAVING と ORDER BY の併用・LIMIT。
    let combined = run(
        &core,
        &ctx,
        &format!("{AGG} HAVING abs(c - 3) < 1 ORDER BY lower(lang) DESC LIMIT 1"),
    );
    assert_eq!(lang_rows(&combined), vec!["ja"]);
}

#[test]
fn having_and_order_by_expression_error_contract() {
    let (core, _g) = build("sql26-having-errors", &[]);
    let ctx = ctx_for("tenant-a");
    // 構文段: 集計関数の直接記述・関数を含まない述語（従来どおり）は 42601。
    for sql in [
        format!("{AGG} HAVING count(*) > 1"),
        format!("{AGG} HAVING lang = 'ja'"),
        format!("{AGG} HAVING c + 1 > 3"),
        format!("{AGG} ORDER BY sum(n)"),
    ] {
        assert_eq!(run_err(&core, &ctx, &sql), "42601", "{sql}");
    }
    // 名前解決: 疑似列 `id`・未知名・ベースの非キー列は黙って解決せず 22000。
    for sql in [
        format!("{AGG} HAVING abs(id) > 1"),
        format!("{AGG} HAVING abs(zzz) > 1"),
        format!("{AGG} HAVING lower(title) = 'a'"),
        format!("{AGG} ORDER BY lower(title)"),
    ] {
        assert_eq!(run_err(&core, &ctx, &sql), "22000", "{sql}");
    }
    // 評価エラーはグループが存在するときのみ発生する。
    assert_eq!(
        run_err(&core, &ctx, &format!("{AGG} HAVING abs(1 / (c - c)) > 0")),
        "22012"
    );
    let nobody = private_only_ctx("tenant-z");
    let empty = run(
        &core,
        &nobody,
        &format!("{AGG} HAVING abs(1 / (c - c)) > 0"),
    );
    assert!(empty.rows.is_empty());
}

#[test]
fn having_and_order_by_expression_do_not_leak_other_tenants_private_rows() {
    let secret = Row {
        id: 100,
        title: Some("AAA-secret"),
        lang: "zz-secret",
        n: Some(999),
        d: None,
    };
    let (without, _g1) = build("sql26-having-rls-a", &[]);
    let (with_secret, _g2) = build("sql26-having-rls-b", std::slice::from_ref(&secret));
    let other = ctx_for("tenant-b");
    let render = |core: &EngineCore, sql: &str| -> Vec<Vec<Cell>> {
        run(core, &other, sql)
            .rows
            .into_iter()
            .map(|r| r.cells)
            .collect()
    };
    for sql in [
        format!("{AGG} HAVING abs(c - 3) < 5 ORDER BY lower(lang) DESC"),
        format!("{AGG} HAVING COALESCE(s, 0) >= 0 ORDER BY abs(s - 6), lang"),
    ] {
        assert_eq!(render(&without, &sql), render(&with_secret, &sql), "{sql}");
    }
    // 所有テナント自身には Private 行のグループが見える。
    let owner = ctx_for("tenant-a");
    let owned = run(
        &with_secret,
        &owner,
        &format!("{AGG} HAVING lower(lang) = 'zz-secret'"),
    );
    assert_eq!(lang_rows(&owned), vec!["zz-secret"]);
    let hidden = run(
        &with_secret,
        &other,
        &format!("{AGG} HAVING lower(lang) = 'zz-secret'"),
    );
    assert!(hidden.rows.is_empty());
}
/// `EXPLAIN` は式キー付きの広域取得・集計も従来どおり受理しない（`explain_rejects_scalar_order_by_scan`
/// と同じ分類。EXPLAIN と式 `ORDER BY`／`HAVING` の併用は本 Issue の対象外）。
#[test]
fn explain_still_rejects_expression_order_by_and_having() {
    let (core, _g) = build("sql26-explain", &[]);
    let ctx = ctx_for("tenant-a");
    for sql in [
        "EXPLAIN SELECT id FROM docs ORDER BY lower(title) LIMIT 5",
        "EXPLAIN SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang HAVING lower(lang) = 'ja'",
    ] {
        assert_eq!(run_err(&core, &ctx, sql), "42601", "{sql}");
    }
}

// ---------- 後続機能との併用境界（main 取り込み時に追加した拒否・検査経路） ----------

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

/// 式キーの `ORDER BY` は、ウィンドウ関数（Issue #1189 で併用可能になったのは列名キーのみ）・
/// 集合演算の枝内の `ORDER BY`（Issue #1191）とは併用できず `42601`。黙って並べ替えを
/// 落として `LIMIT` を適用しない。
#[test]
fn expression_order_by_rejects_window_and_set_branch_combinations() {
    let (core, _g) = build("sql26-ob-combos", &[]);
    let ctx = ctx_for("tenant-a");
    for sql in [
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM docs ORDER BY lower(title) LIMIT 5",
        "(SELECT lang FROM docs ORDER BY lower(title) LIMIT 2) \
         UNION ALL (SELECT lang FROM docs ORDER BY lang LIMIT 1)",
    ] {
        assert_eq!(run_err(&core, &ctx, sql), "42601", "{sql}");
    }
    // 列名キーのみなら従来どおり併用できる（上の拒否が式キーに限られることの対照）。
    let ok = run(
        &core,
        &ctx,
        "SELECT id, ROW_NUMBER() OVER (ORDER BY id) AS rn FROM docs ORDER BY lang, id LIMIT 3",
    );
    assert_eq!(ok.rows.len(), 3);
}

/// 評価後射影形ビュー（集計本文。Issue #1192）への外側 `ORDER BY` の式キーは `42601`
/// （列名キーと同じく外側は `LIMIT`／`OFFSET` のみ受理。式キーを黙って落とさない）。
/// 列を絞ったビューを基にした集計では、式 `HAVING`／`ORDER BY` を公開列・項目名の
/// 範囲で受理し、非公開列への参照は拒否する。
#[test]
fn expression_order_by_and_having_over_views() {
    let (core, _g) = build("sql26-ob-views", &[]);
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    for ddl in [
        "CREATE VIEW va AS SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang",
        "CREATE VIEW vt AS SELECT title, lang FROM docs",
    ] {
        core.execute_sql_in_session(&ctx, &mut session, ddl)
            .unwrap_or_else(|e| panic!("{ddl}: {e:?}"));
    }
    assert_eq!(
        session_err(
            &core,
            &ctx,
            &mut session,
            "SELECT * FROM va ORDER BY lower(lang) LIMIT 3",
        ),
        "42601"
    );
    let ok = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT lang, COUNT(*) AS c FROM vt GROUP BY lang \
             HAVING abs(c - 3) < 1 ORDER BY CASE WHEN c > 2 THEN 0 ELSE 1 END, lower(lang) DESC",
        )
        .expect("aggregate over view with expression HAVING/ORDER BY");
    match ok {
        SqlOutcome::Query(rows) => assert_eq!(lang_rows(&rows), vec!["ja", "en"]),
        other => panic!("expected rows, got {other:?}"),
    }
    for sql in [
        "SELECT lang, COUNT(*) AS c FROM vt GROUP BY lang ORDER BY abs(n)",
        "SELECT lang, COUNT(*) AS c FROM vt GROUP BY lang HAVING abs(n) > 0",
    ] {
        assert_eq!(
            session_err(&core, &ctx, &mut session, sql),
            "22000",
            "{sql}"
        );
    }
}
// ---------- 整数セル（Cell::Integer）の 2^53 境界（Issue #1277） ----------
//
// `COUNT`・`SUM(id)`・`MIN(id)`・`MAX(id)` の結果（`Cell::Integer(u64)`）を式形 `HAVING`／
// `ORDER BY` が参照するとき、`sql/group_by.rs` の `cell_to_scalar_ref` が `WHERE`／投影式で
// `id` を扱う場合と同じ 2^53 境界で f64 へ変換する（`2^53` ちょうどは受理・超過と `i64`
// 範囲外は `22000`）。単体テスト `group_row_integer_cell_uses_exact_f64_boundary` と対になる
// SQL 表層（`EngineCore::execute_sql`）側の固定。2^53 を超える数値リテラルは束縛段で
// `22000` になるため、境界超過はリテラルでなく大きな id の投入（セル値側）で作る。

/// 2^53（f64 で連続して表現できる整数の上限）。
const P53: u64 = 1u64 << 53;

/// 大きな値のグループ `"big"` を作る Private 行（`MAX(id)`／`SUM(id)` のセル値になる）。
fn big_row(id: u64) -> Row {
    row(id, None, "big", None, None)
}

const AGG_ID: &str =
    "SELECT lang, COUNT(*) AS c, MAX(id) AS m, SUM(id) AS s FROM docs GROUP BY lang";

/// `lang` が `group` の行の `idx` 番目のセルを返す（0: lang, 1: c, 2: m, 3: s）。
fn group_cell(result: &QueryResult, group: &str, idx: usize) -> Cell {
    result
        .rows
        .iter()
        .find(|r| matches!(r.cells.first(), Some(Cell::Text(t)) if t == group))
        .and_then(|r| r.cells.get(idx).cloned())
        .unwrap_or_else(|| panic!("group {group} / cell {idx} not found"))
}

/// `2^53` ちょうどは受理され、`HAVING`／`ORDER BY` の両方で丸めず比較・並べ替えできる。
#[test]
fn integer_cell_exactly_2p53_is_accepted_and_compared_exactly() {
    let (core, _g) = build("sql26-p53-exact", &[big_row(P53)]);
    let ctx = ctx_for("tenant-a");
    for having in [
        "HAVING abs(m - 9007199254740992) < 1",
        "HAVING abs(m - 9007199254740991) = 1",
        "HAVING CASE WHEN m >= 9007199254740992 THEN 1 ELSE 0 END = 1",
    ] {
        let r = run(&core, &ctx, &format!("{AGG_ID} {having} ORDER BY lang"));
        assert_eq!(lang_rows(&r), vec!["big"], "{having}");
        assert_eq!(group_cell(&r, "big", 2), Cell::Integer(P53), "{having}");
    }
    for (order, expected) in [
        ("ORDER BY abs(m) DESC", vec!["big", "fr", "en", "ja"]),
        ("ORDER BY abs(m)", vec!["ja", "en", "fr", "big"]),
        (
            "ORDER BY CASE WHEN m = 9007199254740992 THEN 0 ELSE 1 END, lang",
            vec!["big", "en", "fr", "ja"],
        ),
    ] {
        let r = run(&core, &ctx, &format!("{AGG_ID} {order}"));
        assert_eq!(lang_rows(&r), expected, "{order}");
    }
}

/// `2^53` 超過（`2^53 + 1`）は、式が値を参照すると `HAVING`／`ORDER BY` の両方で `22000`
/// （黙った丸めをしない fail-closed）。参照しない式・従来形の厳密比較は影響を受けない。
#[test]
fn integer_cell_over_2p53_is_rejected_when_referenced_by_expression() {
    let (core, _g) = build("sql26-p53-over", &[big_row(P53 + 1)]);
    let ctx = ctx_for("tenant-a");
    for sql in [
        format!("{AGG_ID} HAVING abs(m) > 0"),
        format!("{AGG_ID} ORDER BY abs(m)"),
        format!("{AGG_ID} ORDER BY CASE WHEN m > 0 THEN 0 ELSE 1 END"),
    ] {
        assert_eq!(run_err(&core, &ctx, &sql), "22000", "{sql}");
    }
    // 対照 (a): 範囲外のセルを参照しない式は成功し、投影値は丸められない。
    let r = run(
        &core,
        &ctx,
        &format!("{AGG_ID} HAVING abs(c - 1) < 1 ORDER BY lower(lang)"),
    );
    assert_eq!(lang_rows(&r), vec!["big"]);
    assert_eq!(group_cell(&r, "big", 2), Cell::Integer(P53 + 1));
    let r = run(&core, &ctx, &format!("{AGG_ID} ORDER BY lower(lang) DESC"));
    assert_eq!(lang_rows(&r), vec!["ja", "fr", "en", "big"]);
    // 対照 (b): 従来形（リテラル比較）の HAVING は厳密比較の経路で成功する。
    let r = run(
        &core,
        &ctx,
        &format!("{AGG_ID} HAVING m > 9007199254740992 ORDER BY lang"),
    );
    assert_eq!(lang_rows(&r), vec!["big"]);
}

/// `i64` 範囲外（`i64::MAX + 1`・`u64::MAX`）も `HAVING`／`ORDER BY` の両方で `22000`。
#[test]
fn integer_cell_beyond_i64_is_rejected_when_referenced_by_expression() {
    let ctx = ctx_for("tenant-a");
    for (name, id) in [
        ("sql26-p53-i64over", i64::MAX as u64 + 1),
        ("sql26-p53-u64max", u64::MAX),
    ] {
        let (core, _g) = build(name, &[big_row(id)]);
        for sql in [
            format!("{AGG_ID} HAVING abs(m) > 0"),
            format!("{AGG_ID} ORDER BY abs(m)"),
        ] {
            assert_eq!(run_err(&core, &ctx, &sql), "22000", "id={id}: {sql}");
        }
        let r = run(
            &core,
            &ctx,
            &format!("{AGG_ID} HAVING m > 9007199254740992 ORDER BY lang"),
        );
        assert_eq!(lang_rows(&r), vec!["big"], "id={id}");
        assert_eq!(group_cell(&r, "big", 2), Cell::Integer(id), "id={id}");
    }
}

/// 集計値 `SUM(id)` が境界をまたぐ場合（各入力 id は表現可能）。ちょうど `2^53` は受理、
/// `2^53 + 1` は `s` を参照する式のみ `22000`（`m` は範囲内なので受理）。
#[test]
fn integer_cell_sum_crossing_2p53_follows_same_boundary() {
    let ctx = ctx_for("tenant-a");
    let (core, _g) = build(
        "sql26-p53-sum-exact",
        &[big_row((1 << 52) - 1), big_row((1 << 52) + 1)],
    );
    let r = run(
        &core,
        &ctx,
        &format!("{AGG_ID} HAVING abs(s - 9007199254740992) < 1"),
    );
    assert_eq!(lang_rows(&r), vec!["big"]);
    assert_eq!(group_cell(&r, "big", 3), Cell::Integer(P53));
    let r = run(&core, &ctx, &format!("{AGG_ID} ORDER BY abs(s) DESC"));
    assert_eq!(lang_rows(&r), vec!["big", "fr", "en", "ja"]);

    let (core, _g2) = build(
        "sql26-p53-sum-over",
        &[big_row(1 << 52), big_row((1 << 52) + 1)],
    );
    for sql in [
        format!("{AGG_ID} HAVING abs(s) > 0"),
        format!("{AGG_ID} ORDER BY abs(s)"),
    ] {
        assert_eq!(run_err(&core, &ctx, &sql), "22000", "{sql}");
    }
    let r = run(
        &core,
        &ctx,
        &format!("{AGG_ID} HAVING abs(m - 4503599627370497) < 1"),
    );
    assert_eq!(lang_rows(&r), vec!["big"]);
}

/// 他テナントの Private 行にある範囲外の値は、エラーにも結果にも現れない（RLS・Issue #353）。
/// 所有テナントでは同じクエリが `22000`（境界検査が実際に効いていることの対照）。
#[test]
fn integer_cell_boundary_does_not_leak_through_other_tenants() {
    let (without, _g1) = build("sql26-p53-rls-a", &[]);
    let (with, _g2) = build("sql26-p53-rls-b", &[big_row(u64::MAX)]);
    let other = ctx_for("tenant-b");
    let owner = ctx_for("tenant-a");
    for sql in [
        format!("{AGG_ID} HAVING abs(m) > 0 ORDER BY abs(m) DESC"),
        format!("{AGG_ID} HAVING abs(s) >= 0 ORDER BY abs(s), lang"),
    ] {
        let a = run(&without, &other, &sql);
        let b = run(&with, &other, &sql);
        let cells = |r: &QueryResult| r.rows.iter().map(|x| x.cells.clone()).collect::<Vec<_>>();
        assert_eq!(cells(&a), cells(&b), "{sql}");
        assert_eq!(run_err(&with, &owner, &sql), "22000", "{sql}");
        let none = run(&with, &private_only_ctx("tenant-z"), &sql);
        assert!(none.rows.is_empty(), "{sql}");
    }
}
