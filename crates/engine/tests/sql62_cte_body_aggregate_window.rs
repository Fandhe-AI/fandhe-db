//! CTE 本文の集計・`GROUP BY`・窓関数の結合テスト（SQL-62・Issue #1863、親 Issue #1859）。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-62・SQL-34・RLS-10 (b)。
//!
//! 集計・窓の本文はインライン展開すると外側の `WHERE` が集計・窓の前に掛かって誤結果になるため、
//! 常に具体化経路で評価する。本番経路（`EngineCore::execute_sql_in_session`）で
//! (a) 集計・`HAVING`・`SELECT DISTINCT`・窓の本文が正しい値を返し外側の `WHERE` が後に掛かること、
//! (b) 集計値・窓の値が参照者自身の可視行だけから計算されること（RLS-10 (b)）、
//! (c) `LIMIT` なしの本文は可視行（グループ）が `--max-result-rows` を超えたら `54000`、
//! (d) 列名リスト・Describe・JOIN の各経路、(e) 未対応の形が fail-closed に拒否されることを固定する。
//! テスト用の公開行はテナント間で共有される（`Visibility::Public`）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::result_limits::ResultLimits;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
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
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(storage: &Storage, tenant: &str, id: u64, lang: &str, vis: Visibility) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        vis,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(format!("{tenant} {id}")),
        ],
        &op_id(&format!("seed-{id}")),
    )
    .expect("insert row");
}

/// 可視行（公開行は全テナント共有）: alice は 1・2・3・4・5・7、bob は 1・2・3・5・6・7、
/// carol は 1・2・3・5・7。言語は id 3 だけ `en`。
fn open_with(limits: Option<ResultLimits>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql62-cte-body-aggregate-window");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let rows: [(&str, u64, &str, Visibility); 7] = [
        ("alice", 1, "ja", Visibility::Public),
        ("alice", 2, "ja", Visibility::Public),
        ("alice", 3, "en", Visibility::Public),
        ("alice", 4, "ja", Visibility::Private),
        ("bob", 5, "ja", Visibility::Public),
        ("bob", 6, "ja", Visibility::Private),
        ("carol", 7, "ja", Visibility::Public),
    ];
    for (tenant, id, lang, vis) in rows {
        insert_row(&storage, tenant, id, lang, vis);
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let core = match limits {
        Some(l) => core.with_result_limits(l),
        None => core,
    };
    (core, guard)
}

fn query(core: &EngineCore, tenant: &str, sql: &str) -> Result<QueryResult, SqlSurfaceError> {
    let mut session = SessionState::default();
    match core.execute_sql_in_session(&ctx(tenant), &mut session, sql)? {
        SqlOutcome::Query(result) => Ok(result),
        other => panic!("expected Query outcome for {sql}, got {other:?}"),
    }
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) -> QueryResult {
    query(core, tenant, sql).unwrap_or_else(|e| panic!("{sql}: {e:?}"))
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    query(core, tenant, sql)
        .expect_err(sql)
        .wire_code()
        .to_string()
}

/// 第 `col` 列を整数として取り出す。
fn ints(r: &QueryResult, col: usize) -> Vec<u64> {
    r.rows
        .iter()
        .map(|row| match row.cells.get(col) {
            Some(Cell::Integer(n)) => *n,
            other => panic!("expected integer cell, got {other:?}"),
        })
        .collect()
}

fn text(r: &QueryResult, col: usize) -> Vec<String> {
    r.rows
        .iter()
        .map(|row| match row.cells.get(col) {
            Some(Cell::Text(s)) => s.clone(),
            other => panic!("expected text cell, got {other:?}"),
        })
        .collect()
}

fn evals(core: &EngineCore) -> u64 {
    core.materialized_cte_eval_stats().evaluations
}

const AGG: &str = "SELECT lang, count(*) AS n FROM docs GROUP BY lang";

/// 集計本文: 外側の `WHERE` は集計の後に掛かる（インライン展開なら `count` が 1 行ずつになる）。
#[test]
fn aggregate_body_group_by_and_outer_where_after_aggregation() {
    let (core, _g) = open_with(None);
    let sql = format!("WITH c AS ({AGG}) SELECT lang, n FROM c WHERE n > 1 ORDER BY lang LIMIT 10");
    let r = ok(&core, "alice", &sql);
    assert_eq!(text(&r, 0), vec!["ja"]);
    assert_eq!(ints(&r, 1), vec![5]);
    let all = format!("WITH c AS ({AGG}) SELECT lang, n FROM c ORDER BY lang LIMIT 10");
    let r = ok(&core, "alice", &all);
    assert_eq!(text(&r, 0), vec!["en", "ja"]);
    assert_eq!(ints(&r, 1), vec![1, 5]);
}

/// `GROUP BY` なしの集計・`HAVING`・集計 `ORDER BY`／`LIMIT`・外側の集計。
#[test]
fn aggregate_body_variants() {
    let (core, _g) = open_with(None);
    let plain = "WITH c AS (SELECT count(*) AS n FROM docs) SELECT n FROM c LIMIT 10";
    assert_eq!(ints(&ok(&core, "alice", plain), 0), vec![6]);
    // `HAVING` の集計呼び出しは本文では未対応（`WITH` の主クエリ・ビュー本体と同じ制限。`42601`）。
    // 集計値の絞り込みは外側の `WHERE` で行える。
    let having =
        "WITH c AS (SELECT lang, count(*) AS n FROM docs GROUP BY lang HAVING count(*) > 1) \
                  SELECT lang FROM c LIMIT 10";
    assert_eq!(code(&core, "alice", having), "42601");
    let limited = "WITH c AS (SELECT lang, count(*) AS n FROM docs GROUP BY lang \
                   ORDER BY n DESC LIMIT 1) SELECT lang FROM c LIMIT 10";
    assert_eq!(text(&ok(&core, "alice", limited), 0), vec!["ja"]);
    let outer_agg = format!("WITH c AS ({AGG}) SELECT max(n) FROM c");
    let r = ok(&core, "alice", &outer_agg);
    assert!(
        matches!(
            r.rows.first().and_then(|row| row.cells.first()),
            Some(Cell::SignedInteger(5))
        ),
        "{r:?}"
    );
}

/// `SELECT DISTINCT` 本文。
#[test]
fn distinct_body() {
    let (core, _g) = open_with(None);
    let sql =
        "WITH c AS (SELECT DISTINCT lang FROM docs) SELECT lang FROM c ORDER BY lang LIMIT 10";
    assert_eq!(text(&ok(&core, "alice", sql), 0), vec!["en", "ja"]);
}

/// 窓本文: 各グループの先頭行（外側の `WHERE rn = 1` は窓の計算後に掛かる）。
#[test]
fn window_body_row_number_then_outer_filter() {
    let (core, _g) = open_with(None);
    let sql =
        "WITH c AS (SELECT id, lang, row_number() OVER (PARTITION BY lang ORDER BY id) AS rn \
               FROM docs) SELECT id FROM c WHERE rn = 1 ORDER BY id LIMIT 10";
    assert_eq!(ints(&ok(&core, "alice", sql), 0), vec![1, 3]);
    // 窓集計と、本文の `ORDER BY`／`LIMIT` の併用。
    let running =
        "WITH c AS (SELECT id, sum(id) OVER (ORDER BY id) AS s FROM docs ORDER BY id LIMIT 3) \
                   SELECT id, s FROM c ORDER BY id LIMIT 10";
    let r = ok(&core, "alice", running);
    assert_eq!(ints(&r, 0), vec![1, 2, 3]);
}

/// 集計値・窓の値は参照者自身の可視行だけから計算される（RLS-10 (b)）。
#[test]
fn aggregate_and_window_use_only_visible_rows() {
    let (core, _g) = open_with(None);
    let count = "WITH c AS (SELECT count(*) AS n FROM docs) SELECT n FROM c LIMIT 10";
    assert_eq!(ints(&ok(&core, "alice", count), 0), vec![6]);
    assert_eq!(ints(&ok(&core, "bob", count), 0), vec![6]);
    assert_eq!(ints(&ok(&core, "carol", count), 0), vec![5]);
    let win = "WITH c AS (SELECT id, row_number() OVER (ORDER BY id) AS rn FROM docs) \
               SELECT id FROM c WHERE rn = 6 LIMIT 10";
    assert_eq!(ints(&ok(&core, "alice", win), 0), vec![7]);
    assert_eq!(ints(&ok(&core, "carol", win), 0), Vec::<u64>::new());
}

/// `LIMIT` なしの本文は、可視行（グループ）数が上限を超えたら `54000`、ちょうどなら成功する。
#[test]
fn body_without_limit_is_capped_by_the_result_limit() {
    // 窓本文: alice 6 行 > 上限 5、carol 5 行 = 上限。
    let (core, _g) = open_with(Some(ResultLimits::new(5)));
    let win = "WITH c AS (SELECT id, row_number() OVER (ORDER BY id) AS rn FROM docs) \
               SELECT id FROM c ORDER BY id LIMIT 2";
    assert_eq!(code(&core, "alice", win), "54000");
    assert_eq!(ints(&ok(&core, "carol", win), 0), vec![1, 2]);
    // 集計本文: グループ数（2）が上限（1）を超える。
    let (core, _g) = open_with(Some(ResultLimits::new(1)));
    let agg = format!("WITH c AS ({AGG}) SELECT lang FROM c LIMIT 1");
    assert_eq!(code(&core, "alice", &agg), "54000");
    let one = "WITH c AS (SELECT count(*) AS n FROM docs) SELECT n FROM c LIMIT 1";
    assert_eq!(ints(&ok(&core, "alice", one), 0), vec![6]);
}

/// 列名リスト: 改名後の名前で参照でき、長すぎるリストは参照の有無によらず `42P10`。
#[test]
fn column_list_applies_to_aggregate_and_window_bodies() {
    let (core, _g) = open_with(None);
    let agg = "WITH c(l, cnt) AS (SELECT lang, count(*) FROM docs GROUP BY lang) \
               SELECT l, cnt FROM c ORDER BY l LIMIT 10";
    let r = ok(&core, "alice", agg);
    assert_eq!(text(&r, 0), vec!["en", "ja"]);
    assert_eq!(ints(&r, 1), vec![1, 5]);
    let win = "WITH c(i, r) AS (SELECT id, row_number() OVER (ORDER BY id) FROM docs) \
               SELECT i, r FROM c WHERE r = 2 LIMIT 10";
    let r = ok(&core, "alice", win);
    assert_eq!(ints(&r, 0), vec![2]);
    for sql in [
        "WITH c(a, b, d) AS (SELECT lang, count(*) FROM docs GROUP BY lang) SELECT a FROM c LIMIT 1",
        "WITH c(a, b, d) AS (SELECT lang, count(*) FROM docs GROUP BY lang) SELECT id FROM docs LIMIT 1",
        "WITH c(a, b, d) AS (SELECT id, row_number() OVER (ORDER BY id) FROM docs) \
         SELECT id FROM docs LIMIT 1",
    ] {
        assert_eq!(code(&core, "alice", sql), "42P10", "{sql}");
    }
}

/// 別名なしの集計・窓項目の既定名（`count`・`row_number`）も列名リストの重複検査（`42702`）に
/// 含める（参照されない CTE にも適用）。窓項目は位置どおりに数えて列数を判定する。
#[test]
fn column_list_checks_default_names_and_star_width() {
    let (core, _g) = open_with(None);
    for sql in [
        "WITH c(count) AS (SELECT lang, count(*) FROM docs GROUP BY lang) SELECT id FROM docs LIMIT 1",
        "WITH c(row_number) AS (SELECT id, row_number() OVER (ORDER BY id) FROM docs) \
         SELECT id FROM docs LIMIT 1",
        // 窓項目が先頭でも位置どおりに改名される（`lang` は 2 列目の通常列と衝突）。
        "WITH c(lang) AS (SELECT row_number() OVER (ORDER BY id), lang FROM docs) \
         SELECT id FROM docs LIMIT 1",
    ] {
        assert_eq!(code(&core, "alice", sql), "42702", "{sql}");
    }
    // 列数ちょうどのリストは受理し、1 つ多いリストは 42P10。
    let body = "SELECT lang, row_number() OVER (ORDER BY id) AS rn FROM docs";
    ok(
        &core,
        "alice",
        &format!("WITH c(x0, x1) AS ({body}) SELECT id FROM docs LIMIT 1"),
    );
    let too_long = format!("WITH c(x0, x1, x2) AS ({body}) SELECT id FROM docs LIMIT 1");
    assert_eq!(code(&core, "alice", &too_long), "42P10");
}

/// `[NOT] MATERIALIZED` の指定によらず結果は同じで、本文の評価は 1 回。
#[test]
fn materialization_modifier_does_not_change_result() {
    let (core, _g) = open_with(None);
    for modifier in ["", "MATERIALIZED ", "NOT MATERIALIZED "] {
        let sql = format!("WITH c AS {modifier}({AGG}) SELECT lang FROM c WHERE n = 1 LIMIT 5");
        let before = evals(&core);
        assert_eq!(
            text(&ok(&core, "alice", &sql), 0),
            vec!["en"],
            "{modifier:?}"
        );
        assert_eq!(evals(&core) - before, 1, "{modifier:?}");
    }
}

/// 窓本文の CTE は JOIN の入力にできる（本文の評価は 1 回）。集計本文は固定 `0A000`。
#[test]
fn window_cte_as_join_input_and_aggregate_is_rejected() {
    let (core, _g) = open_with(None);
    let sql =
        "WITH c AS (SELECT id, lang, row_number() OVER (PARTITION BY lang ORDER BY id) AS rn \
               FROM docs) SELECT a.id, b.id FROM c a JOIN c b ON a.rn = b.rn \
               ORDER BY a.id, b.id LIMIT 100";
    let before = evals(&core);
    let r = ok(&core, "alice", sql);
    assert_eq!(evals(&core) - before, 1);
    assert_eq!(ints(&r, 0), vec![1, 1, 2, 3, 3, 4, 5, 7]);
    let agg = format!("WITH c AS ({AGG}) SELECT a.lang FROM c a JOIN c b ON a.n = b.n LIMIT 10");
    assert_eq!(code(&core, "alice", &agg), "0A000");
}

/// Describe の列は Execute の結果列と一致し、本文を評価しない。
#[test]
fn describe_matches_execute() {
    let (core, _g) = open_with(None);
    for sql in [
        format!("WITH c AS ({AGG}) SELECT lang, n FROM c ORDER BY lang LIMIT 10"),
        "WITH c AS (SELECT id, row_number() OVER (ORDER BY id) AS rn FROM docs) \
         SELECT id, rn FROM c ORDER BY id LIMIT 10"
            .to_string(),
    ] {
        let session = SessionState::default();
        let parsed = core.parse_sql(&sql).expect("parse");
        let before = evals(&core);
        let described = core
            .describe_parsed_in_session(&session, &parsed)
            .expect("describe")
            .expect("has columns");
        assert_eq!(evals(&core), before, "describe must not execute the body");
        let executed = ok(&core, "alice", &sql);
        assert_eq!(described, executed.columns, "{sql}");
    }
}

/// 未対応の形は fail-closed（`0A000`／`42601`）。インラインへ戻して誤結果にしない。
#[test]
fn unsupported_shapes_fail_closed() {
    let (core, _g) = open_with(None);
    for (sql, want) in [
        // 集計 CTE を別の CTE が読む連鎖。
        (
            format!("WITH a AS ({AGG}), b AS (SELECT lang FROM a) SELECT lang FROM b LIMIT 10"),
            "0A000",
        ),
        // 入れ子の WITH TIES。
        (
            "WITH c AS (SELECT lang, count(*) AS n FROM docs GROUP BY lang \
             ORDER BY n FETCH FIRST 1 ROWS WITH TIES) SELECT lang FROM c LIMIT 10"
                .to_string(),
            "0A000",
        ),
        // ROLLUP・窓と GROUP BY の併用・ベクトル順位付けは 42601。
        (
            "WITH c AS (SELECT lang, count(*) FROM docs GROUP BY ROLLUP (lang)) \
             SELECT lang FROM c LIMIT 10"
                .to_string(),
            "42601",
        ),
        (
            "WITH c AS (SELECT lang, row_number() OVER (ORDER BY id) FROM docs GROUP BY lang) \
             SELECT lang FROM c LIMIT 10"
                .to_string(),
            "42601",
        ),
        (
            "WITH c AS (SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2]' LIMIT 3) \
             SELECT id FROM c LIMIT 10"
                .to_string(),
            "42601",
        ),
    ] {
        assert_eq!(code(&core, "alice", &sql), want, "{sql}");
    }
}

/// インライン専用の文脈（ビュー本文）や、公開しない列の参照を拒否する。
#[test]
fn inline_only_contexts_and_hidden_columns_are_rejected() {
    let (core, _g) = open_with(None);
    let mut session = SessionState::default();
    session.allow_ddl();
    let err = core
        .execute_sql_in_session(
            &ctx("alice"),
            &mut session,
            &format!("CREATE VIEW v AS WITH c AS ({AGG}) SELECT lang FROM c LIMIT 10"),
        )
        .expect_err("aggregate CTE in a view body");
    assert_eq!(err.wire_code(), "0A000");
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "CREATE VIEW pub AS SELECT lang FROM docs",
    )
    .expect("create view");
    // 非公開列（`body`）を集計引数・グループキー・窓の `PARTITION BY` に使う本文は、参照の有無によらず拒否。
    for sql in [
        "WITH c AS (SELECT count(body) AS n FROM pub) SELECT n FROM c LIMIT 1",
        "WITH c AS (SELECT body, count(*) AS n FROM pub GROUP BY body) SELECT lang FROM docs LIMIT 1",
        "WITH c AS (SELECT lang, row_number() OVER (PARTITION BY body) AS r FROM pub) \
         SELECT lang FROM docs LIMIT 1",
    ] {
        let mut s = SessionState::default();
        let r = core.execute_sql_in_session(&ctx("alice"), &mut s, sql);
        let c = r.expect_err(sql).wire_code().to_string();
        assert!(c == "22000" || c == "42703", "{sql}: {c}");
    }
}
