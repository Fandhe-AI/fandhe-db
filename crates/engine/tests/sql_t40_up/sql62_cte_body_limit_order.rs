//! CTE 本文の `ORDER BY`・`LIMIT`・`OFFSET` の結合テスト（SQL-62・Issue #1862、親 Issue #1859）。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-62・SQL-34・RLS-10 (b)。
//!
//! 本文に `LIMIT` を持つ CTE（tail CTE）はインライン展開すると `LIMIT` が黙って落ちて誤結果に
//! なるため、常に具体化経路で評価する。ここでは本番経路（`EngineCore::execute_sql_in_session`）で
//! (a) 本文の `LIMIT`／`ORDER BY`／`OFFSET` が効き外側の `WHERE` は `LIMIT` の後に掛かること、
//! (b) `LIMIT` が参照者自身の可視行にだけ掛かり他テナントの存在が漏れないこと（RLS-10 (b)）、
//! (c) `LIMIT` なしの本文は可視行が `--max-result-rows` を超えたら `54000`、(d) 単一表・JOIN・
//! 列名リスト・Describe の各経路、(e) 未対応の形が fail-closed に拒否されることを固定する。
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

use crate::temp_db;
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
    let path = unique_db_path("sql62-cte-body-limit-order");
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

fn evals(core: &EngineCore) -> u64 {
    core.materialized_cte_eval_stats().evaluations
}

const BODY: &str = "SELECT id, lang FROM docs ORDER BY id LIMIT 3";

/// 本文の `ORDER BY ... LIMIT` が先頭から効く。
#[test]
fn body_limit_and_order_apply() {
    let (core, _g) = open_with(None);
    let sql = format!("WITH c AS ({BODY}) SELECT id FROM c ORDER BY id LIMIT 10");
    assert_eq!(ints(&ok(&core, "alice", &sql), 0), vec![1, 2, 3]);
    let desc = "WITH c AS (SELECT id FROM docs ORDER BY id DESC LIMIT 2 OFFSET 1) \
                SELECT id FROM c ORDER BY id DESC LIMIT 10";
    assert_eq!(ints(&ok(&core, "alice", desc), 0), vec![5, 4]);
}

/// `FETCH FIRST n ROWS ONLY` は `LIMIT` へ正規化されて同じ結果になる。
#[test]
fn body_fetch_first_is_normalized() {
    let (core, _g) = open_with(None);
    let sql = "WITH c AS (SELECT id FROM docs ORDER BY id FETCH FIRST 2 ROWS ONLY) \
               SELECT id FROM c ORDER BY id LIMIT 10";
    assert_eq!(ints(&ok(&core, "alice", sql), 0), vec![1, 2]);
}

/// 外側の `WHERE` は本文の `LIMIT` の後に適用される（インライン展開なら 1・2・4・5・7 になる）。
#[test]
fn outer_where_applies_after_body_limit() {
    let (core, _g) = open_with(None);
    let sql = format!("WITH c AS ({BODY}) SELECT id FROM c WHERE lang = 'ja' ORDER BY id LIMIT 10");
    assert_eq!(ints(&ok(&core, "alice", &sql), 0), vec![1, 2]);
}

/// 外側の序数 `ORDER BY`（`ORDER BY 1`・ASC/DESC）は外側の投影位置の列として解決される。
/// 範囲外・0・負の序数は fail-closed で拒否する（`0A000`・`42601` のいずれか固定コード）。
#[test]
fn outer_ordinal_order_by_resolves_and_rejects_out_of_range() {
    let (core, _g) = open_with(None);
    let asc = format!("WITH c AS ({BODY}) SELECT id FROM c ORDER BY 1 LIMIT 10");
    assert_eq!(ints(&ok(&core, "alice", &asc), 0), vec![1, 2, 3]);
    let desc = format!("WITH c AS ({BODY}) SELECT id FROM c ORDER BY 1 DESC LIMIT 10");
    assert_eq!(ints(&ok(&core, "alice", &desc), 0), vec![3, 2, 1]);
    for pos in ["2", "0", "-1"] {
        let sql = format!("WITH c AS ({BODY}) SELECT id FROM c ORDER BY {pos} LIMIT 10");
        let c = code(&core, "alice", &sql);
        assert!(c == "0A000" || c == "42601", "{sql}: {c}");
    }
}

/// 集計の主クエリも本文の `LIMIT` の件数を数える。
#[test]
fn aggregate_main_query_counts_limited_rows() {
    let (core, _g) = open_with(None);
    let sql = format!("WITH c AS ({BODY}) SELECT COUNT(*) FROM c");
    assert_eq!(ints(&ok(&core, "alice", &sql), 0), vec![3]);
}

/// 本文の評価は 1 回で、外側の `LIMIT` が本文へ畳み込まれない。`[NOT] MATERIALIZED` でも結果は同じ。
#[test]
fn body_is_evaluated_once_and_modifier_does_not_change_result() {
    let (core, _g) = open_with(None);
    for modifier in ["", "MATERIALIZED ", "NOT MATERIALIZED "] {
        let sql = format!("WITH c AS {modifier}({BODY}) SELECT id FROM c ORDER BY id LIMIT 2");
        let before = evals(&core);
        assert_eq!(
            ints(&ok(&core, "alice", &sql), 0),
            vec![1, 2],
            "{modifier:?}"
        );
        assert_eq!(evals(&core) - before, 1, "{modifier:?}");
    }
}

/// `LIMIT` は参照者自身の可視行だけに掛かる（RLS-10 (b)）。他テナントの行は件数にも現れない。
#[test]
fn limit_applies_to_the_callers_visible_rows_only() {
    let (core, _g) = open_with(None);
    let sql = "WITH c AS (SELECT id FROM docs ORDER BY id DESC LIMIT 2) \
               SELECT id FROM c ORDER BY id DESC LIMIT 10";
    assert_eq!(ints(&ok(&core, "alice", sql), 0), vec![7, 5]);
    assert_eq!(ints(&ok(&core, "bob", sql), 0), vec![7, 6]);
    assert_eq!(ints(&ok(&core, "carol", sql), 0), vec![7, 5]);
    let count = "WITH c AS (SELECT id FROM docs ORDER BY id LIMIT 100) SELECT COUNT(*) FROM c";
    assert_eq!(ints(&ok(&core, "alice", count), 0), vec![6]);
    assert_eq!(ints(&ok(&core, "carol", count), 0), vec![5]);
}

/// `LIMIT` なしの本文は暗黙上限つき。可視行が上限を超えたら `54000`、ちょうどなら成功する。
#[test]
fn body_without_limit_is_capped_by_the_result_limit() {
    let (core, _g) = open_with(Some(ResultLimits::new(5)));
    let sql = "WITH c AS (SELECT id FROM docs ORDER BY id) SELECT id FROM c ORDER BY id LIMIT 2";
    assert_eq!(code(&core, "alice", sql), "54000");
    assert_eq!(ints(&ok(&core, "carol", sql), 0), vec![1, 2]);
    // `OFFSET` だけの本文も同じ暗黙上限（オフセット後の件数で判定する）。
    let offset0 = "WITH c AS (SELECT id FROM docs OFFSET 0) SELECT id FROM c LIMIT 2";
    assert_eq!(code(&core, "alice", offset0), "54000");
    let offset1 = "WITH c AS (SELECT id FROM docs OFFSET 1) SELECT id FROM c ORDER BY id LIMIT 2";
    assert_eq!(ints(&ok(&core, "alice", offset1), 0), vec![2, 3]);
}

/// JOIN の入力としての tail CTE（自己結合）。本文の評価は 1 回。
#[test]
fn tail_cte_as_join_input() {
    let (core, _g) = open_with(None);
    let sql = format!(
        "WITH c AS ({BODY}) SELECT a.id, b.id FROM c a JOIN c b ON a.lang = b.lang \
         ORDER BY a.id, b.id LIMIT 100"
    );
    let before = evals(&core);
    let r = ok(&core, "alice", &sql);
    assert_eq!(evals(&core) - before, 1);
    assert_eq!(ints(&r, 0), vec![1, 1, 2, 2, 3]);
    assert_eq!(ints(&r, 1), vec![1, 2, 1, 2, 3]);
}

/// JOIN 経路で tail CTE が先行する非 tail CTE を読む形（導出層に `ORDER BY`・`LIMIT` を載せる）。
#[test]
fn tail_cte_derived_from_a_plain_cte_in_a_join() {
    let (core, _g) = open_with(None);
    let sql = "WITH a AS (SELECT id, lang FROM docs), \
               b AS (SELECT id, lang FROM a ORDER BY id DESC LIMIT 2) \
               SELECT x.id, y.id FROM a x JOIN b y ON x.id = y.id ORDER BY x.id LIMIT 100";
    let r = ok(&core, "alice", sql);
    assert_eq!(ints(&r, 0), vec![5, 7]);
}

/// 導出層（先行 CTE を読む tail CTE）の序数 `ORDER BY` は本文の投影位置の列へ解決される
/// （PR #2733 レビュー指摘。`plan_rows` が序数を `42601` で拒否していた）。
#[test]
fn derived_tail_cte_resolves_ordinal_order_by_in_a_join() {
    let (core, _g) = open_with(None);
    let sql = "WITH a AS (SELECT id FROM docs), \
               b AS (SELECT id FROM a ORDER BY 1 DESC LIMIT 2) \
               SELECT x.id FROM b x JOIN docs d ON x.id = d.id ORDER BY x.id LIMIT 10";
    assert_eq!(ints(&ok(&core, "alice", sql), 0), vec![5, 7]);
    // 範囲外の序数は黙って並べず拒否する（fail-closed。単一表経路と同じ `42601`）。
    let out_of_range = "WITH a AS (SELECT id FROM docs), \
               b AS (SELECT id FROM a ORDER BY 3 LIMIT 2) \
               SELECT x.id FROM b x JOIN docs d ON x.id = d.id LIMIT 10";
    assert!(query(&core, "alice", out_of_range).is_err());
}

/// 導出層の明示 `LIMIT` の上限検査は Describe でも Execute と同じ結果になる（乖離させない）。
#[test]
fn derived_tail_cte_limit_is_validated_in_describe() {
    let (core, _g) = open_with(Some(ResultLimits::new(5)));
    let sql = "WITH a AS (SELECT id FROM docs), \
               b AS (SELECT id FROM a LIMIT 6) \
               SELECT x.id FROM b x JOIN b y ON x.id = y.id LIMIT 10";
    let executed = code(&core, "carol", sql);
    let session = SessionState::default();
    let parsed = core.parse_sql(sql).expect("parse");
    let described = core
        .describe_parsed_in_session(&session, &parsed)
        .expect_err("describe must reject the over-limit LIMIT");
    assert_eq!(described.wire_code().to_string(), executed);
}

/// 列名リストつきの tail CTE と、Describe の列が Execute の結果列と一致すること。
#[test]
fn column_list_and_describe() {
    let (core, _g) = open_with(None);
    let sql = "WITH c(x, l) AS (SELECT id, lang FROM docs ORDER BY id LIMIT 2) \
               SELECT x, l FROM c ORDER BY x LIMIT 10";
    let session = SessionState::default();
    let parsed = core.parse_sql(sql).expect("parse");
    let before = evals(&core);
    let described = core
        .describe_parsed_in_session(&session, &parsed)
        .expect("describe")
        .expect("has columns");
    assert_eq!(evals(&core), before, "describe must not execute the body");
    let executed = ok(&core, "alice", sql);
    assert_eq!(described, executed.columns);
    assert_eq!(ints(&executed, 0), vec![1, 2]);
}

/// 未対応の形は固定の `0A000` で拒否する（インライン展開で `LIMIT` を落とさない）。
#[test]
fn unsupported_shapes_fail_closed_with_0a000() {
    let (core, _g) = open_with(None);
    for sql in [
        // tail CTE を別の CTE が読む連鎖。
        "WITH a AS (SELECT id FROM docs ORDER BY id LIMIT 2), b AS (SELECT id FROM a) \
         SELECT id FROM b LIMIT 10",
        // 入れ子の WITH TIES。
        "WITH c AS (SELECT id FROM docs ORDER BY id FETCH FIRST 2 ROWS WITH TIES) \
         SELECT id FROM c LIMIT 10",
    ] {
        assert_eq!(code(&core, "alice", sql), "0A000", "{sql}");
    }
}

/// ベクトル順位付けの本文は `42601` のまま。集計の本文は #1863 で受理され、`LIMIT` つきでも値が正しい。
#[test]
fn ranked_body_stays_42601_and_aggregate_body_is_accepted() {
    let (core, _g) = open_with(None);
    let ranked = "WITH c AS (SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2]' LIMIT 3) \
                  SELECT id FROM c LIMIT 10";
    assert_eq!(code(&core, "alice", ranked), "42601");
    let agg = "WITH c AS (SELECT lang, COUNT(*) AS n FROM docs GROUP BY lang \
               ORDER BY n DESC LIMIT 1) SELECT n FROM c LIMIT 10";
    assert_eq!(ints(&ok(&core, "alice", agg), 0), vec![5]);
}

/// 本文の `ORDER BY` はビューが公開しない列を参照できない（並べ替えの oracle にしない）。
#[test]
fn body_order_by_cannot_use_columns_hidden_by_a_view() {
    let (core, _g) = open_with(None);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "CREATE VIEW v AS SELECT lang FROM docs",
    )
    .expect("create view");
    let sql = "WITH c AS (SELECT lang FROM v ORDER BY body LIMIT 1) SELECT lang FROM c LIMIT 10";
    assert_eq!(code(&core, "alice", sql), "42703");
}

/// インライン専用の文脈（ビュー本文の `WITH`）で tail CTE を参照すると `0A000`。
#[test]
fn inline_only_context_rejects_tail_cte() {
    let (core, _g) = open_with(None);
    let mut session = SessionState::default();
    session.allow_ddl();
    let err = core
        .execute_sql_in_session(
            &ctx("alice"),
            &mut session,
            "CREATE VIEW v AS WITH c AS (SELECT id FROM docs ORDER BY id LIMIT 1) SELECT id FROM c",
        )
        .expect_err("tail CTE in a view body");
    assert_eq!(err.wire_code(), "0A000");
}

/// 導出層（先行 CTE を読む tail CTE）の `LIMIT` 省略は、単一表経路と同じく `OFFSET` 適用後の件数で
/// 結果行数上限を判定する（PR #2733 レビュー指摘）。alice の可視行は 6 行・上限 5。
#[test]
fn derived_tail_cte_without_limit_is_capped_after_offset() {
    let (core, _g) = open_with(Some(ResultLimits::new(5)));
    let offset2 = "WITH a AS (SELECT id FROM docs), \
               b AS (SELECT id FROM a ORDER BY id OFFSET 2) \
               SELECT x.id FROM b x JOIN docs d ON x.id = d.id ORDER BY x.id LIMIT 5";
    assert_eq!(ints(&ok(&core, "alice", offset2), 0), vec![3, 4, 5, 7]);
    // `OFFSET` 後も上限（5）を超える本文は `54000`。
    let offset0 = "WITH a AS (SELECT id FROM docs), \
               b AS (SELECT id FROM a ORDER BY id OFFSET 0) \
               SELECT x.id FROM b x JOIN docs d ON x.id = d.id ORDER BY x.id LIMIT 5";
    assert_eq!(code(&core, "alice", offset0), "54000");
}

/// 導出層の `SELECT *` 本文の序数 `ORDER BY` は固定 `0A000`（親の列で `*` を展開しない制限。
/// 列名を明示した本文は序数でも並べられる）。
#[test]
fn derived_tail_cte_star_ordinal_order_by_is_unsupported() {
    let (core, _g) = open_with(None);
    let star = "WITH a AS (SELECT id FROM docs), \
               b AS (SELECT * FROM a ORDER BY 1 DESC LIMIT 2) \
               SELECT x.id FROM b x JOIN docs d ON x.id = d.id LIMIT 10";
    assert_eq!(code(&core, "alice", star), "0A000");
}

/// 導出層（先行 CTE を読む tail CTE）の明示 `LIMIT` は `ORDER BY`／`LIMIT` 適用後の件数で判定し、
/// 非共有の親が暗黙の行数上限を超えていても成功する（PR #2733 レビュー指摘。単一表経路との整合）。
/// `LIMIT` なしの本文は従来どおり `54000`。alice の可視行は 6 行・上限 5。
#[test]
fn derived_tail_cte_explicit_limit_is_not_capped_by_the_parent() {
    let (core, _g) = open_with(Some(ResultLimits::new(5)));
    let limited = "WITH a AS (SELECT id FROM docs), \
               b AS (SELECT id FROM a ORDER BY id DESC LIMIT 2) \
               SELECT x.id FROM b x JOIN docs d ON x.id = d.id ORDER BY x.id LIMIT 5";
    assert_eq!(ints(&ok(&core, "alice", limited), 0), vec![5, 7]);
    // 明示 `LIMIT` が設定値の上限を超える本文は引き続き拒否される（無制限の具体化は許さない）。
    let over = "WITH a AS (SELECT id FROM docs), \
               b AS (SELECT id FROM a ORDER BY id LIMIT 6) \
               SELECT x.id FROM b x JOIN docs d ON x.id = d.id ORDER BY x.id LIMIT 5";
    assert_eq!(code(&core, "alice", over), "22000");
    // `LIMIT` なしは従来どおり。
    let unlimited = "WITH a AS (SELECT id FROM docs), \
               b AS (SELECT id FROM a ORDER BY id) \
               SELECT x.id FROM b x JOIN docs d ON x.id = d.id ORDER BY x.id LIMIT 5";
    assert_eq!(code(&core, "alice", unlimited), "54000");
}
