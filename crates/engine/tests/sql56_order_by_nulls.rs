//! `ORDER BY ... NULLS FIRST|LAST` の結合テスト（Issue #1791、対象ビヘイビア: SQL-56。
//! ポインタ: `docs/spec/05-tasks.md` TASK-298・`docs/spec/04-behavior/sql-surface.md` SQL-56）。
//!
//! `tests/sql56_order_by_ordinal.rs` と同じ流儀（`unique_db_path`＋`CleanupGuard`、
//! `EngineCore::execute_sql` の production 経路、production の判定関数を呼ばない Rust 側の
//! 独立オラクル）で、広域取得・ウィンドウ・`GROUP BY`・`DISTINCT`・JOIN・ビューの各経路で
//! ASC/DESC × NULLS FIRST/LAST の全組合せが PostgreSQL 17 の規則どおりに並ぶこと、
//! kNN 距離順への併用や不正な指定が `42601` のままであること、別テナントの NULL 行が
//! 結果に現れないことを固定する。

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

#[derive(Clone, Debug)]
struct Row {
    id: u64,
    title: Option<&'static str>,
    lang: &'static str,
    n: Option<i32>,
}

// `title` の NULL は id 4 のみ、`n` の NULL は id 3 と id 9。同値キー（n=3 が 2 行等）を含める。
const ROWS: [Row; 9] = [
    row(1, Some("Banana"), "ja", Some(3)),
    row(2, Some("apple"), "en", Some(1)),
    row(3, Some("Cherry"), "ja", None),
    row(4, None, "en", Some(5)),
    row(5, Some("banana"), "ja", Some(2)),
    row(6, Some("Apple"), "fr", Some(4)),
    row(7, Some("cherry"), "en", Some(0)),
    row(8, Some("date"), "fr", Some(3)),
    row(9, Some("elder"), "fr", None),
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

fn insert_row(
    storage: &Storage,
    ctx: &PolicyContext,
    visibility: Visibility,
    r: &Row,
    id_offset: u64,
) {
    let text = |s: Option<&str>| s.map(|v| Value::Text(v.to_string())).unwrap_or(Value::Null);
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        ctx,
        r.id + id_offset,
        visibility,
        &[
            Value::Vector(vec![r.id as f32, 0.0]),
            text(r.title),
            text(Some(r.lang)),
            r.n.map(Value::Integer).unwrap_or(Value::Null),
        ],
        &op_id(r.id + id_offset),
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
        insert_row(&storage, &ctx, Visibility::Public, r, 0);
    }
    // 別テナントの行（NULL を含む）。tenant-a の結果・順序・件数に現れてはならない。
    let other = ctx_for("tenant-b");
    for r in &ROWS {
        let mut r = r.clone();
        r.title = None;
        r.n = None;
        insert_row(&storage, &other, Visibility::Private, &r, 1000);
    }
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

/// オラクル 1 キー: (キー抽出, 降順か, NULL 位置の明示 `Some(true)`=FIRST・`Some(false)`=LAST・
/// `None`=PostgreSQL 既定〔ASC は末尾・DESC は先頭〕)。
type KeyFn = Box<dyn Fn(&Row) -> Option<String>>;
type OKey = (KeyFn, bool, Option<bool>);

/// NULL を含む 1 キーの最終比較（NULL 位置は絶対位置で、降順反転の影響を受けない）。
fn cmp_nullable(
    a: Option<&String>,
    b: Option<&String>,
    descending: bool,
    nulls_first: Option<bool>,
) -> Ordering {
    let first = nulls_first.unwrap_or(descending);
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => {
            if first {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(_), None) => {
            if first {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Some(x), Some(y)) => {
            let o = x.cmp(y);
            if descending {
                o.reverse()
            } else {
                o
            }
        }
    }
}

fn oracle(rows: &[Row], keys: &[OKey]) -> Vec<u64> {
    let mut sorted: Vec<&Row> = rows.iter().collect();
    sorted.sort_by(|a, b| {
        for (key, descending, nulls_first) in keys {
            let ord = cmp_nullable(key(a).as_ref(), key(b).as_ref(), *descending, *nulls_first);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        a.id.cmp(&b.id)
    });
    sorted.into_iter().map(|r| r.id).collect()
}

fn n_key() -> KeyFn {
    Box::new(|r: &Row| r.n.map(|v| format!("{v:05}")))
}
fn title_key() -> KeyFn {
    Box::new(|r: &Row| r.title.map(str::to_string))
}
fn lower_title_key() -> KeyFn {
    Box::new(|r: &Row| r.title.map(str::to_lowercase))
}
fn lang_key() -> KeyFn {
    Box::new(|r: &Row| Some(r.lang.to_string()))
}

/// 方向 × NULL 位置の全組合せ（指定なしの既定を含む）: `(SQL 断片, 降順か, NULL 位置の明示)`。
const COMBOS: [(&str, bool, Option<bool>); 6] = [
    ("", false, None),
    ("ASC NULLS FIRST", false, Some(true)),
    ("ASC NULLS LAST", false, Some(false)),
    ("DESC", true, None),
    ("DESC NULLS FIRST", true, Some(true)),
    ("DESC NULLS LAST", true, Some(false)),
];

// ---------- 広域取得（単一表） ----------

#[test]
fn scan_column_key_all_combinations() {
    let (core, _g) = build("sql56n-scan-col");
    let ctx = ctx_for("tenant-a");
    for (frag, desc, nulls) in COMBOS {
        let sql = format!("SELECT id, n FROM docs ORDER BY n {frag} LIMIT 100");
        assert_eq!(
            ids(&run(&core, &ctx, &sql)),
            oracle(&ROWS, &[(n_key(), desc, nulls)]),
            "{sql}"
        );
        let sql = format!("SELECT id FROM docs ORDER BY title {frag} LIMIT 100");
        assert_eq!(
            ids(&run(&core, &ctx, &sql)),
            oracle(&ROWS, &[(title_key(), desc, nulls)]),
            "{sql}"
        );
    }
}

#[test]
fn scan_expression_ordinal_alias_and_collate_keys() {
    let (core, _g) = build("sql56n-scan-forms");
    let ctx = ctx_for("tenant-a");
    for (frag, desc, nulls) in COMBOS {
        // 式キー。
        let sql = format!("SELECT id FROM docs ORDER BY lower(title) {frag} LIMIT 100");
        assert_eq!(
            ids(&run(&core, &ctx, &sql)),
            oracle(&ROWS, &[(lower_title_key(), desc, nulls)]),
            "{sql}"
        );
        // 序数キー。
        let sql = format!("SELECT title, n FROM docs ORDER BY 2 {frag} LIMIT 100");
        assert_eq!(
            ids(&run(&core, &ctx, &sql)),
            oracle(&ROWS, &[(n_key(), desc, nulls)]),
            "{sql}"
        );
        // SELECT の別名キー（列への別名と式への別名）。
        let sql = format!("SELECT n AS k FROM docs ORDER BY k {frag} LIMIT 100");
        assert_eq!(
            ids(&run(&core, &ctx, &sql)),
            oracle(&ROWS, &[(n_key(), desc, nulls)]),
            "{sql}"
        );
        let sql = format!("SELECT lower(title) AS lt FROM docs ORDER BY lt {frag} LIMIT 100");
        assert_eq!(
            ids(&run(&core, &ctx, &sql)),
            oracle(&ROWS, &[(lower_title_key(), desc, nulls)]),
            "{sql}"
        );
        // COLLATE 付きキー（"C" 照合はバイト列順のため title_key と一致する）。
        let sql = format!("SELECT id FROM docs ORDER BY title COLLATE \"C\" {frag} LIMIT 100");
        assert_eq!(
            ids(&run(&core, &ctx, &sql)),
            oracle(&ROWS, &[(title_key(), desc, nulls)]),
            "{sql}"
        );
    }
}

#[test]
fn scan_multiple_keys_mix_null_placements() {
    let (core, _g) = build("sql56n-scan-multi");
    let ctx = ctx_for("tenant-a");
    let cases: Vec<(&str, Vec<OKey>)> = vec![
        (
            "SELECT id FROM docs ORDER BY lang, n DESC NULLS LAST LIMIT 100",
            vec![(lang_key(), false, None), (n_key(), true, Some(false))],
        ),
        (
            "SELECT id FROM docs ORDER BY n NULLS FIRST, title DESC NULLS LAST LIMIT 100",
            vec![
                (n_key(), false, Some(true)),
                (title_key(), true, Some(false)),
            ],
        ),
        (
            "SELECT n, title FROM docs ORDER BY 1 DESC NULLS LAST, 2 NULLS FIRST LIMIT 100",
            vec![
                (n_key(), true, Some(false)),
                (title_key(), false, Some(true)),
            ],
        ),
    ];
    for (sql, keys) in cases {
        assert_eq!(ids(&run(&core, &ctx, sql)), oracle(&ROWS, &keys), "{sql}");
    }
}

#[test]
fn scan_top_n_and_offset_respect_null_placement() {
    let (core, _g) = build("sql56n-scan-topn");
    let ctx = ctx_for("tenant-a");
    for (frag, desc, nulls) in COMBOS {
        let all = oracle(&ROWS, &[(n_key(), desc, nulls)]);
        let sql = format!("SELECT id FROM docs ORDER BY n {frag} LIMIT 3");
        assert_eq!(ids(&run(&core, &ctx, &sql)), all[..3].to_vec(), "{sql}");
        let sql = format!("SELECT id FROM docs ORDER BY n {frag} LIMIT 3 OFFSET 2");
        assert_eq!(ids(&run(&core, &ctx, &sql)), all[2..5].to_vec(), "{sql}");
    }
}

#[test]
fn scan_pseudo_id_key_is_unaffected_by_nulls_placement() {
    let (core, _g) = build("sql56n-scan-id");
    let ctx = ctx_for("tenant-a");
    let mut asc: Vec<u64> = ROWS.iter().map(|r| r.id).collect();
    asc.sort();
    let desc: Vec<u64> = asc.iter().rev().copied().collect();
    for frag in ["NULLS FIRST", "NULLS LAST", "ASC NULLS LAST"] {
        let sql = format!("SELECT id FROM docs ORDER BY id {frag} LIMIT 100");
        assert_eq!(ids(&run(&core, &ctx, &sql)), asc, "{sql}");
    }
    for frag in ["DESC NULLS FIRST", "DESC NULLS LAST"] {
        let sql = format!("SELECT id FROM docs ORDER BY id {frag} LIMIT 100");
        assert_eq!(ids(&run(&core, &ctx, &sql)), desc, "{sql}");
    }
}

// ---------- ウィンドウ ----------

fn int_cell(result: &QueryResult, row: usize, col: usize) -> i64 {
    match result.rows.get(row).and_then(|r| r.cells.get(col)) {
        Some(Cell::Integer(n)) => i64::try_from(*n).expect("small integer"),
        Some(Cell::Numeric(n)) => n.to_string().parse().expect("integer"),
        other => panic!("expected integer cell, got {other:?}"),
    }
}

#[test]
fn window_order_by_honors_null_placement() {
    let (core, _g) = build("sql56n-window");
    let ctx = ctx_for("tenant-a");
    for (frag, desc, nulls) in COMBOS {
        // ROW_NUMBER() の割り当て順 = OVER 内の ORDER BY（同値は id 昇順で安定）。
        let want = oracle(&ROWS, &[(n_key(), desc, nulls)]);
        let sql =
            format!("SELECT id, ROW_NUMBER() OVER (ORDER BY n {frag}) AS rn FROM docs LIMIT 100");
        let result = run(&core, &ctx, &sql);
        let mut by_rn: Vec<(i64, u64)> = (0..result.rows.len())
            .map(|i| (int_cell(&result, i, 1), result.rows[i].id))
            .collect();
        by_rn.sort();
        assert_eq!(
            by_rn.iter().map(|(_, id)| *id).collect::<Vec<_>>(),
            want,
            "{sql}"
        );
        // 文全体の ORDER BY（NULLS 付き）とウィンドウの併用。
        let sql = format!(
            "SELECT id, ROW_NUMBER() OVER (ORDER BY n DESC NULLS LAST) AS rn FROM docs \
             ORDER BY n {frag} LIMIT 100"
        );
        assert_eq!(ids(&run(&core, &ctx, &sql)), want, "{sql}");
    }
}

#[test]
fn window_partition_by_with_nulls_placement_orders_within_partition() {
    let (core, _g) = build("sql56n-window-part");
    let ctx = ctx_for("tenant-a");
    let rank_of = |result: &QueryResult, id: u64| -> i64 {
        let i = result.rows.iter().position(|r| r.id == id).expect("row");
        int_cell(result, i, 1)
    };
    let first = run(
        &core,
        &ctx,
        "SELECT id, RANK() OVER (PARTITION BY lang ORDER BY n NULLS FIRST) AS r FROM docs LIMIT 100",
    );
    // ja: id3(NULL)=1, id5(n=2)=2, id1(n=3)=3。fr: id9(NULL)=1, id8(n=3)=2, id6(n=4)=3。
    assert_eq!(
        [3, 5, 1, 9, 8, 6].map(|id| rank_of(&first, id)),
        [1, 2, 3, 1, 2, 3]
    );
    let last = run(
        &core,
        &ctx,
        "SELECT id, RANK() OVER (PARTITION BY lang ORDER BY n NULLS LAST) AS r FROM docs LIMIT 100",
    );
    // ja: id5=1, id1=2, id3(NULL)=3。
    assert_eq!([5, 1, 3].map(|id| rank_of(&last, id)), [1, 2, 3]);
}

// ---------- GROUP BY / DISTINCT ----------

fn first_cells(result: &QueryResult) -> Vec<Option<String>> {
    result
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Text(t)) => Some(t.clone()),
            Some(Cell::Null) => None,
            Some(Cell::Integer(n)) => Some(format!("{n:05}")),
            Some(Cell::SignedInteger(n)) => Some(format!("{n:05}")),
            other => panic!("unexpected cell {other:?}"),
        })
        .collect()
}

/// 重複を除いた値の列を、指定の方向・NULL 位置で並べた期待値。
fn expected_distinct(
    values: Vec<Option<String>>,
    desc: bool,
    nulls: Option<bool>,
) -> Vec<Option<String>> {
    let mut keys = values;
    keys.sort();
    keys.dedup();
    keys.sort_by(|a, b| cmp_nullable(a.as_ref(), b.as_ref(), desc, nulls));
    keys
}

#[test]
fn group_by_key_order_honors_null_placement() {
    let (core, _g) = build("sql56n-group-key");
    let ctx = ctx_for("tenant-a");
    let titles: Vec<Option<String>> = ROWS.iter().map(|r| r.title.map(str::to_string)).collect();
    for (frag, desc, nulls) in COMBOS {
        let want = expected_distinct(titles.clone(), desc, nulls);
        let sql =
            format!("SELECT title, COUNT(*) AS c FROM docs GROUP BY title ORDER BY title {frag}");
        assert_eq!(first_cells(&run(&core, &ctx, &sql)), want, "{sql}");
        let sql = format!("SELECT title, COUNT(*) AS c FROM docs GROUP BY title ORDER BY 1 {frag}");
        assert_eq!(first_cells(&run(&core, &ctx, &sql)), want, "{sql}");
    }
}

#[test]
fn group_by_aggregate_result_null_honors_placement() {
    let (core, _g) = build("sql56n-group-agg");
    let ctx = ctx_for("tenant-a");
    // fr グループは elder(n NULL) のみ → MAX(n) が NULL。ja=3, en=5。
    let base = "SELECT lang, MAX(n) AS m FROM docs \
                WHERE lang <> 'fr' OR title = 'elder' GROUP BY lang";
    let langs = |r: &QueryResult| -> Vec<String> {
        r.rows
            .iter()
            .map(|row| match row.cells.first() {
                Some(Cell::Text(t)) => t.clone(),
                other => panic!("unexpected {other:?}"),
            })
            .collect()
    };
    let cases = [
        ("ORDER BY m NULLS FIRST", vec!["fr", "ja", "en"]),
        ("ORDER BY m ASC NULLS LAST", vec!["ja", "en", "fr"]),
        ("ORDER BY m DESC NULLS LAST", vec!["en", "ja", "fr"]),
        ("ORDER BY m DESC NULLS FIRST", vec!["fr", "en", "ja"]),
        ("ORDER BY m", vec!["ja", "en", "fr"]),
        ("ORDER BY m DESC", vec!["fr", "en", "ja"]),
    ];
    for (order, want) in cases {
        let sql = format!("{base} {order}");
        assert_eq!(langs(&run(&core, &ctx, &sql)), want, "{sql}");
    }
}

#[test]
fn distinct_order_by_honors_null_placement() {
    let (core, _g) = build("sql56n-distinct");
    let ctx = ctx_for("tenant-a");
    let ns: Vec<Option<String>> = ROWS
        .iter()
        .map(|r| r.n.map(|v| format!("{v:05}")))
        .collect();
    for (frag, desc, nulls) in COMBOS {
        let want = expected_distinct(ns.clone(), desc, nulls);
        let sql = format!("SELECT DISTINCT n FROM docs ORDER BY n {frag}");
        assert_eq!(first_cells(&run(&core, &ctx, &sql)), want, "{sql}");
        let sql = format!("SELECT DISTINCT n FROM docs ORDER BY 1 {frag}");
        assert_eq!(first_cells(&run(&core, &ctx, &sql)), want, "{sql}");
    }
}

// ---------- JOIN ----------

fn first_ids(result: &QueryResult) -> Vec<u64> {
    result
        .rows
        .iter()
        .map(|r| match r.cells.first() {
            Some(Cell::Numeric(n)) => n.to_string().parse().expect("id"),
            Some(Cell::Integer(n)) => *n,
            other => panic!("unexpected id cell {other:?}"),
        })
        .collect()
}

#[test]
fn join_plain_and_aggregate_honor_null_placement() {
    let (core, _g) = build("sql56n-join");
    let ctx = ctx_for("tenant-a");
    let titles: Vec<Option<String>> = ROWS.iter().map(|r| r.title.map(str::to_string)).collect();
    for (frag, desc, nulls) in COMBOS {
        let sql = format!(
            "SELECT docs.id, docs.n FROM docs JOIN labels ON docs.lang = labels.lang \
             ORDER BY docs.n {frag}, docs.id LIMIT 100"
        );
        assert_eq!(
            first_ids(&run(&core, &ctx, &sql)),
            oracle(&ROWS, &[(n_key(), desc, nulls)]),
            "{sql}"
        );
        // 序数キー。
        let sql = format!(
            "SELECT docs.id, docs.n FROM docs JOIN labels ON docs.lang = labels.lang \
             ORDER BY 2 {frag}, 1 LIMIT 100"
        );
        assert_eq!(
            first_ids(&run(&core, &ctx, &sql)),
            oracle(&ROWS, &[(n_key(), desc, nulls)]),
            "{sql}"
        );
        // 集計形。
        let sql = format!(
            "SELECT docs.title, COUNT(*) AS c FROM docs JOIN labels ON docs.lang = labels.lang \
             GROUP BY docs.title ORDER BY docs.title {frag} LIMIT 100"
        );
        assert_eq!(
            first_cells(&run(&core, &ctx, &sql)),
            expected_distinct(titles.clone(), desc, nulls),
            "{sql}"
        );
    }
}

// ---------- ビュー ----------

#[test]
fn view_outer_order_by_honors_null_placement() {
    let (core, _g) = build("sql56n-view");
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE VIEW v AS SELECT n, title FROM docs",
    )
    .expect("create view");
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "CREATE VIEW vs AS SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang",
    )
    .expect("create buffered view");
    for (frag, desc, nulls) in COMBOS {
        let sql = format!("SELECT * FROM v ORDER BY n {frag} LIMIT 100");
        let SqlOutcome::Query(rows) = core
            .execute_sql_in_session(&ctx, &mut session, &sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        else {
            panic!("expected rows");
        };
        assert_eq!(
            ids(&rows),
            oracle(&ROWS, &[(n_key(), desc, nulls)]),
            "{sql}"
        );
    }
    // 評価後射影形ビュー（集計本文）への外側の列キー + NULLS は受理される。
    // c は全言語 3 で同点のため、`lang NULLS FIRST` の昇順で en, fr, ja。
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT lang, c FROM vs ORDER BY c DESC NULLS LAST, lang NULLS FIRST LIMIT 10",
        )
        .expect("buffered view outer order by");
    let SqlOutcome::Query(rows) = outcome else {
        panic!("expected rows");
    };
    assert_eq!(
        first_cells(&rows),
        vec![Some("en".into()), Some("fr".into()), Some("ja".into())]
    );
}

// ---------- サブクエリ ----------

#[test]
fn subquery_order_by_with_nulls_is_accepted() {
    let (core, _g) = build("sql56n-subquery");
    let ctx = ctx_for("tenant-a");
    let result = run(
        &core,
        &ctx,
        "SELECT id FROM docs WHERE n = (SELECT n FROM docs ORDER BY n NULLS LAST LIMIT 1) LIMIT 100",
    );
    // n の最小は 0（id 7）。NULLS LAST なので NULL ではなく 0 が選ばれる。
    assert_eq!(ids(&result), vec![7]);
}

// ---------- fail-closed ----------

#[test]
fn malformed_and_vector_ranking_nulls_forms_are_rejected() {
    let (core, _g) = build("sql56n-reject");
    let ctx = ctx_for("tenant-a");
    for sql in [
        "SELECT id FROM docs ORDER BY n NULLS LIMIT 5",
        "SELECT id FROM docs ORDER BY n NULLS MIDDLE LIMIT 5",
        "SELECT id FROM docs ORDER BY n NULLS FIRST DESC LIMIT 5",
        "SELECT id FROM docs ORDER BY n NULLS FIRST NULLS LAST LIMIT 5",
        "SELECT id FROM docs ORDER BY n NULLS FIRST, LIMIT 5",
        "SELECT id FROM docs ORDER BY embedding <=> [1.0, 2.0] NULLS LAST LIMIT 5",
        "SELECT id FROM docs ORDER BY HYBRID(embedding, 'q') NULLS FIRST LIMIT 5",
        "SELECT lang, COUNT(*) AS c FROM docs GROUP BY lang ORDER BY c NULLS SOMETIMES",
        "SELECT id, ROW_NUMBER() OVER (ORDER BY n NULLS) FROM docs LIMIT 5",
        "SELECT DISTINCT n FROM docs ORDER BY n NULLS FIRST NULLS LAST",
    ] {
        assert_eq!(run_err(&core, &ctx, sql), "42601", "{sql}");
    }
}

#[test]
fn qualified_column_named_like_nulls_clause_is_not_reinterpreted() {
    let (core, _g) = build("sql56n-qualified");
    let ctx = ctx_for("tenant-a");
    for sql in [
        "SELECT docs.id FROM docs ORDER BY n NULLS docs.first LIMIT 5",
        "SELECT docs.id FROM docs ORDER BY n docs.nulls FIRST LIMIT 5",
    ] {
        let code = run_err(&core, &ctx, sql);
        assert!(["42601", "42703"].contains(&code), "{sql}: {code}");
    }
}

// ---------- RLS ----------

#[test]
fn other_tenant_null_rows_never_appear() {
    let (core, _g) = build("sql56n-rls");
    let ctx = ctx_for("tenant-a");
    for (frag, desc, nulls) in COMBOS {
        let sql = format!("SELECT id FROM docs ORDER BY n {frag} LIMIT 100");
        let got = ids(&run(&core, &ctx, &sql));
        assert_eq!(got.len(), ROWS.len(), "{sql}");
        assert!(got.iter().all(|id| *id < 1000), "{sql}");
        assert_eq!(got, oracle(&ROWS, &[(n_key(), desc, nulls)]), "{sql}");
    }
}

// ---------- EXPLAIN ----------

#[test]
fn explain_accepts_nulls_placement() {
    let (core, _g) = build("sql56n-explain");
    let ctx = ctx_for("tenant-a");
    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx,
        &mut session,
        "EXPLAIN SELECT id FROM docs ORDER BY n DESC NULLS LAST LIMIT 5",
    )
    .expect("explain with NULLS LAST");
    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "EXPLAIN SELECT id FROM docs ORDER BY n NULLS MIDDLE LIMIT 5",
        )
        .expect_err("malformed");
    assert_eq!(err.wire_code(), "42601");
}

// ---------- 後方互換: 列名が NULLS／FIRST／LAST の表 ----------

#[test]
fn columns_named_nulls_first_last_remain_usable_as_order_keys() {
    let path = unique_db_path("sql56n-keyword-columns");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "kw",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("nulls", ColumnType::Integer, true),
                ColumnDef::new("first", ColumnType::Integer, true),
                ColumnDef::new("last", ColumnType::Integer, true),
            ],
        ))
        .expect("create kw");
    let ctx = ctx_for("tenant-a");
    // (id, nulls, first, last)
    for (id, a, b, c) in [
        (1u64, Some(2), Some(1), None),
        (2, None, Some(2), Some(1)),
        (3, Some(1), None, Some(2)),
    ] {
        let v = |x: Option<i32>| x.map(Value::Integer).unwrap_or(Value::Null);
        engine::tenant::insert_typed_row(
            &storage,
            "kw",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec![id as f32, 0.0]), v(a), v(b), v(c)],
            &op_id(id),
        )
        .expect("insert kw");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    // 列名 `nulls`（NULLS 句なし・あり）。nulls 列の値: id1=2, id2=NULL, id3=1。
    assert_eq!(
        ids(&run(
            &core,
            &ctx,
            "SELECT id FROM kw ORDER BY nulls LIMIT 10"
        )),
        vec![3, 1, 2]
    );
    assert_eq!(
        ids(&run(
            &core,
            &ctx,
            "SELECT id FROM kw ORDER BY nulls NULLS FIRST LIMIT 10"
        )),
        vec![2, 3, 1]
    );
    // 列名 `first`／`last` を複数キーで使い、NULLS 句と併用する。
    assert_eq!(
        ids(&run(
            &core,
            &ctx,
            "SELECT id FROM kw ORDER BY first DESC NULLS LAST, last NULLS FIRST LIMIT 10"
        )),
        vec![2, 1, 3]
    );
}
