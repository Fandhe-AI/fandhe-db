//! `GROUP BY` キー式（`GROUP BY upper(lang)`・`GROUP BY n / 2` 等）の結合テスト
//! （Issue #1817、対象ビヘイビア: SQL-58。ポインタ: `docs/spec/04-behavior/` の SQL-58）。
//!
//! `tests/sql58_having_without_group_by.rs` と同じ流儀（`unique_db_path`＋`CleanupGuard`、
//! `EngineCore::execute_sql` の production 経路、production の判定関数を呼ばない Rust 側の
//! 独立オラクル）で、式キーによるグループ化・NULL キーの 1 グループ化・列キーとの混在・
//! 評価エラーで部分結果を返さないこと・RLS 境界（不可視行を評価しない）・キー数／グループ数
//! 上限・fail-closed の境界（`wire_code`）を固定する。期待値は PostgreSQL 17 の意味論で、
//! fixture から手計算した値。式キーの値の出力と SELECT／HAVING／ORDER BY との構造一致は
//! Issue #1818 の `tests/sql58_group_key_expr_matching.rs` が固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::group_by_limits::GroupByLimits;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
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
            ColumnDef::new("f", ColumnType::Double, true),
            ColumnDef::new("ts", ColumnType::Timestamp, true),
        ],
    )
}

/// `(id, title, lang, n, f, ts)`。
type Row = (
    u64,
    Option<&'static str>,
    &'static str,
    Option<i32>,
    f64,
    i64,
);

/// `tenant-a` の Public 行（大文字小文字混在の `lang`、NULL を含む `n`・`title`）。
const ROWS: [Row; 8] = [
    (1, Some("a"), "ja", Some(3), 1.5, 0),
    (2, Some("b"), "en", Some(1), 2.5, DAY - 1),
    (3, Some("c"), "JA", None, 0.5, DAY),
    (4, Some("d"), "en", Some(5), 3.5, 2 * DAY - 1),
    (5, Some("e"), "ja", Some(2), 1.0, 2 * DAY),
    (6, Some("f"), "FR", Some(4), 2.0, 2 * DAY + 5),
    (7, None, "en", Some(0), 4.0, 3 * DAY - 1),
    (8, Some("h"), "fr", Some(3), 1.0, 2 * DAY + 9),
];

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn op_id(id: u64) -> OperationId {
    OperationId::parse(&format!("test-op-{id}")).expect("valid operation_id")
}

fn insert_row(storage: &Storage, ctx: &PolicyContext, visibility: Visibility, r: &Row) {
    let (id, title, lang, n, f, ts) = *r;
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
            Value::Double(f),
            Value::Timestamp(ts),
        ],
        &op_id(id),
    )
    .expect("insert row");
}

/// `tenant-a` に `ROWS`、`tenant-b` の Private な 6 行（5 行は n=1、1 行だけ n=9。RLS 境界の
/// 検証用。`10 / (n - 9)` は tenant-b の行でだけ 0 除算になる）を投入した core。
fn build_with(name: &str, limits: Option<GroupByLimits>) -> (EngineCore, CleanupGuard) {
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
            &(id, Some("x"), "zz", Some(1), 1.0, 0),
        );
    }
    insert_row(
        &storage,
        &b,
        Visibility::Private,
        &(106, Some("y"), "zz", Some(9), 1.0, 0),
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let core = match limits {
        Some(l) => core.with_group_by_limits(l),
        None => core,
    };
    (core, guard)
}

fn build(name: &str) -> (EngineCore, CleanupGuard) {
    build_with(name, None)
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

fn ddl(core: &EngineCore, ctx: &PolicyContext, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(ctx, &mut session, sql)
        .unwrap_or_else(|e| panic!("ddl failed: {sql}: {e:?}"));
}

fn ddl_err(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> &'static str {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(ctx, &mut session, sql)
        .expect_err(&format!("statement must fail: {sql}"))
        .wire_code()
}

fn cell_i64(cell: &Cell) -> Option<i64> {
    match cell {
        Cell::Integer(v) => Some(i64::try_from(*v).expect("fits i64")),
        Cell::SignedInteger(v) => Some(*v),
        Cell::Float(f) => Some(*f as i64),
        Cell::Null => None,
        other => panic!("expected integer cell, got {other:?}"),
    }
}

fn cell_text(cell: &Cell) -> Option<String> {
    match cell {
        Cell::Text(t) => Some(t.clone()),
        Cell::Null => None,
        other => panic!("expected text cell, got {other:?}"),
    }
}

/// 各行の整数セル列（NULL は `None`）。
fn int_rows(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<Vec<Option<i64>>> {
    run(core, ctx, sql)
        .rows
        .iter()
        .map(|r| r.cells.iter().map(cell_i64).collect())
        .collect()
}

fn a() -> PolicyContext {
    ctx_for("tenant-a")
}

#[test]
fn function_key_folds_case_into_one_group() {
    let (core, _g) = build("sql58-gk-func");
    // upper(lang): JA={1,3,5} EN={2,4,7} FR={6,8}。(count, sum(n)) を sum(n) 昇順で並べる。
    let rows = int_rows(
        &core,
        &a(),
        "SELECT count(*), sum(n) FROM docs GROUP BY upper(lang) ORDER BY sum(n)",
    );
    assert_eq!(
        rows,
        vec![
            vec![Some(3), Some(5)],
            vec![Some(3), Some(6)],
            vec![Some(2), Some(7)]
        ]
    );
    let counts = int_rows(
        &core,
        &a(),
        "SELECT count(*) FROM docs GROUP BY upper(lang) ORDER BY count(*) DESC, sum(n)",
    );
    assert_eq!(counts, vec![vec![Some(3)], vec![Some(3)], vec![Some(2)]]);
}

#[test]
fn arithmetic_keys_follow_postgresql_result_types() {
    let (core, _g) = build("sql58-gk-arith");
    // n / 2（整数除算）: 0={r2,r7} 1={r1,r5,r8} 2={r4,r6} NULL={r3}。キー昇順・NULL 末尾。
    assert_eq!(
        int_rows(
            &core,
            &a(),
            "SELECT count(*), sum(n) FROM docs GROUP BY n / 2"
        ),
        vec![
            vec![Some(2), Some(1)],
            vec![Some(3), Some(8)],
            vec![Some(2), Some(9)],
            vec![Some(1), None],
        ]
    );
    // n / 3: 0={r2,r5,r7} 1={r1,r4,r6,r8} NULL={r3}。
    assert_eq!(
        int_rows(
            &core,
            &a(),
            "SELECT count(*), sum(n) FROM docs GROUP BY n / 3"
        ),
        vec![
            vec![Some(3), Some(3)],
            vec![Some(4), Some(15)],
            vec![Some(1), None],
        ]
    );
    // f * 2（double）: 1,2,3,4,5,7,8 のうち 2 だけ 2 行（r5・r8）。
    assert_eq!(
        int_rows(&core, &a(), "SELECT count(*) FROM docs GROUP BY f * 2"),
        vec![
            vec![Some(1)],
            vec![Some(2)],
            vec![Some(1)],
            vec![Some(1)],
            vec![Some(1)],
            vec![Some(1)],
            vec![Some(1)],
        ]
    );
}

#[test]
fn case_and_timestamp_keys() {
    let (core, _g) = build("sql58-gk-case");
    // n > 2 は hi={r1,r4,r6,r8}（sum 15）、それ以外（NULL を含む）は lo={r2,r3,r5,r7}（sum 3）。
    assert_eq!(
        int_rows(
            &core,
            &a(),
            "SELECT count(*), sum(n) FROM docs GROUP BY CASE WHEN n > 2 THEN 'hi' ELSE 'lo' END"
        ),
        vec![vec![Some(4), Some(15)], vec![Some(4), Some(3)]]
    );
    // date_trunc('day', ts): day0={r1,r2} day1={r3,r4} day2={r5,r6,r7,r8}。
    assert_eq!(
        int_rows(
            &core,
            &a(),
            "SELECT count(*) FROM docs GROUP BY date_trunc('day', ts)"
        ),
        vec![vec![Some(2)], vec![Some(2)], vec![Some(4)]]
    );
}

#[test]
fn column_and_expression_keys_can_be_mixed() {
    let (core, _g) = build("sql58-gk-mixed");
    let result = run(
        &core,
        &a(),
        "SELECT lang, count(*) FROM docs GROUP BY lang, n / 2 ORDER BY lang, count(*)",
    );
    let got: Vec<(Option<String>, Option<i64>)> = result
        .rows
        .iter()
        .map(|r| (cell_text(&r.cells[0]), cell_i64(&r.cells[1])))
        .collect();
    let s = |v: &str| Some(v.to_string());
    assert_eq!(
        got,
        vec![
            (s("FR"), Some(1)),
            (s("JA"), Some(1)),
            (s("en"), Some(1)),
            (s("en"), Some(2)),
            (s("fr"), Some(1)),
            (s("ja"), Some(2)),
        ]
    );
}

#[test]
fn null_keys_form_one_group_at_the_end() {
    let (core, _g) = build("sql58-gk-null");
    // nullif(lang, 'en') は en（3 行）で NULL。FR・JA・fr が 1 行ずつ、ja が 2 行、NULL が末尾。
    assert_eq!(
        int_rows(
            &core,
            &a(),
            "SELECT count(*) FROM docs GROUP BY nullif(lang, 'en')"
        ),
        vec![
            vec![Some(1)],
            vec![Some(1)],
            vec![Some(1)],
            vec![Some(2)],
            vec![Some(3)],
        ]
    );
    // -0.0 と 0.0 は PostgreSQL と同じく 1 グループ。
    assert_eq!(
        int_rows(
            &core,
            &a(),
            "SELECT count(*) FROM docs GROUP BY CASE WHEN n > 2 THEN f * 0.0 ELSE f * -0.0 END"
        ),
        vec![vec![Some(8)]]
    );
}

#[test]
fn works_with_having_order_by_limit_offset() {
    let (core, _g) = build("sql58-gk-clauses");
    // HAVING count(*) > 2 は JA(5)・EN(6) が残り、sum(n) 昇順の OFFSET 1 LIMIT 1 は EN。
    assert_eq!(
        int_rows(
            &core,
            &a(),
            "SELECT count(*), sum(n) FROM docs GROUP BY upper(lang) \
             HAVING count(*) > 2 ORDER BY sum(n) LIMIT 1 OFFSET 1"
        ),
        vec![vec![Some(3), Some(6)]]
    );
}

#[test]
fn evaluation_error_fails_the_statement_without_partial_rows() {
    let (core, _g) = build("sql58-gk-error");
    // r1（n=3）で 0 除算。文全体が失敗し、行は返らない。
    assert_eq!(
        run_err(
            &core,
            &a(),
            "SELECT count(*) FROM docs GROUP BY 10 / (n - 3)"
        ),
        "22012"
    );
}

#[test]
fn key_expression_is_not_evaluated_on_invisible_rows() {
    let (core, _g) = build("sql58-gk-rls");
    // tenant-b にだけ n=9 の行がある。tenant-a から `10 / (n - 9)` は成功し、tenant-b の行の
    // 存在（0 除算）を漏らさない。-2={r4,r6} -1={r1,r2,r5,r7,r8} NULL={r3}。
    assert_eq!(
        int_rows(
            &core,
            &a(),
            "SELECT count(*) FROM docs GROUP BY 10 / (n - 9)"
        ),
        vec![vec![Some(2)], vec![Some(5)], vec![Some(1)]]
    );
    // tenant-b 自身の可視行では 0 除算になる。
    assert_eq!(
        run_err(
            &core,
            &ctx_for("tenant-b"),
            "SELECT count(*) FROM docs GROUP BY 10 / (n - 9)"
        ),
        "22012"
    );
    // WHERE で除外された行でも評価しない（n = 3 の r1・r8 と NULL の r3 を除けば
    // 10 / (n - 3) は成功し、n = 1, 5, 2, 4, 0 の 5 グループになる）。
    assert_eq!(
        int_rows(
            &core,
            &a(),
            "SELECT count(*) FROM docs WHERE n <> 3 GROUP BY 10 / (n - 3)"
        )
        .len(),
        5
    );
    // tenant-a の n/2 のキー集合に tenant-b のキー（9 / 2 = 4）は現れない。
    assert_eq!(
        int_rows(&core, &a(), "SELECT count(*) FROM docs GROUP BY n / 2").len(),
        4
    );
}

#[test]
fn key_count_and_group_count_limits_apply_to_expression_keys() {
    let (core, _g) = build("sql58-gk-limits");
    // 列キーと式キーの合算で 9 個は 54000。
    assert_eq!(
        run_err(
            &core,
            &a(),
            "SELECT count(*) FROM docs GROUP BY lang, upper(lang), lower(lang), n, n + 1, \
             n + 2, n + 3, n + 4, n + 5"
        ),
        "54000"
    );
    // 8 個ちょうどは受理される。
    run(
        &core,
        &a(),
        "SELECT count(*) FROM docs GROUP BY lang, upper(lang), lower(lang), n, n + 1, \
         n + 2, n + 3, n + 4",
    );
    // 起動時設定のキー数上限・グループ数上限。
    let (core, _g) = build_with(
        "sql58-gk-limits-col",
        Some(GroupByLimits::new(1, engine::sql::group_by::MAX_GROUPS)),
    );
    assert_eq!(
        run_err(
            &core,
            &a(),
            "SELECT count(*) FROM docs GROUP BY lang, n / 2"
        ),
        "54000"
    );
    let (core, _g) = build_with(
        "sql58-gk-limits-grp",
        Some(GroupByLimits::new(
            engine::sql::allowlist::MAX_GROUP_BY_COLUMNS,
            3,
        )),
    );
    // n / 2 は 4 グループ（0・1・2・NULL）で上限 3 を超える。
    assert_eq!(
        run_err(&core, &a(), "SELECT count(*) FROM docs GROUP BY n / 2"),
        "54000"
    );
    // upper(lang) は 3 グループで上限ちょうど。
    run(
        &core,
        &a(),
        "SELECT count(*) FROM docs GROUP BY upper(lang)",
    );
}

#[test]
fn unsupported_forms_are_rejected_fail_closed() {
    let (core, _g) = build("sql58-gk-reject");
    let cases: [(&str, &str); 9] = [
        ("GROUP BY count(*)", "42803"),
        // `OVER` を含む文は文全体がウィンドウ文として先に分類され、GROUP BY との併用は 42601
        // （式キー内の分類 42P20 は構文段の単体テストで固定する）。
        ("GROUP BY row_number() OVER ()", "42601"),
        ("GROUP BY 1", "42601"),
        ("GROUP BY upper(nope)", "42703"),
        ("GROUP BY n / 2, n / 2", "42601"),
        ("GROUP BY n + (SELECT 1)", "0A000"),
        ("GROUP BY n + $1", "42601"),
        ("GROUP BY n / 2 n", "42601"),
        ("GROUP BY CAST(n AS numeric)", "0A000"),
    ];
    for (tail, code) in cases {
        let sql = format!("SELECT count(*) FROM docs {tail}");
        assert_eq!(run_err(&core, &a(), &sql), code, "{sql}");
    }
    // 式キーの値の出力・構造一致は #1818（tests/sql58_group_key_expr_matching.rs）。一致する式は
    // 受理し、一致しない列参照は集計の外の参照として 42803。
    run(
        &core,
        &a(),
        "SELECT upper(lang), count(*) FROM docs GROUP BY upper(lang)",
    );
    assert_eq!(
        run_err(
            &core,
            &a(),
            "SELECT lang, count(*) FROM docs GROUP BY upper(lang)"
        ),
        "42803"
    );
}

#[test]
fn expression_keys_are_rejected_outside_the_top_level_aggregate() {
    let (core, _g) = build("sql58-gk-context");
    assert_eq!(
        run_err(
            &core,
            &a(),
            "SELECT count(*) FROM docs GROUP BY upper(lang) \
             UNION ALL SELECT count(*) FROM docs"
        ),
        "42601"
    );
    assert_eq!(
        ddl_err(
            &core,
            &a(),
            "CREATE VIEW gk_body AS SELECT count(*) AS c FROM docs GROUP BY upper(lang)"
        ),
        "42601"
    );
    ddl(
        &core,
        &a(),
        "CREATE VIEW gk_simple AS SELECT lang, n FROM docs",
    );
    assert_eq!(
        run_err(
            &core,
            &a(),
            "SELECT count(*) FROM gk_simple GROUP BY upper(lang)"
        ),
        "0A000"
    );
}

#[test]
fn expression_keys_bypass_the_text_index_path() {
    let (core, _g) = build("sql58-gk-index");
    ddl(&core, &a(), "CREATE INDEX docs_lang ON docs (lang)");
    // 単一 TEXT 列に索引があっても、式キーは全走査で正しく集計される。
    assert_eq!(
        int_rows(
            &core,
            &a(),
            "SELECT count(*), sum(n) FROM docs GROUP BY upper(lang) ORDER BY sum(n)"
        ),
        vec![
            vec![Some(3), Some(5)],
            vec![Some(3), Some(6)],
            vec![Some(2), Some(7)]
        ]
    );
}
