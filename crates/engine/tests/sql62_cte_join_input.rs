//! CTE を JOIN の入力（リレーション）として参照する結合テスト（SQL-62 (1)(3)(5)(6)・Issue #1861、
//! 親 Issue #1859）。ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-62・SQL-28・SQL-60・
//! SQL-34・RLS-10 (b)。
//!
//! 受け入れ条件は「CTE を複数回参照しても本文の実行は 1 回だけ」。本文の評価回数は結果から
//! 観測できない（結果は評価方式で変わらない）ため、観測用カウンタ
//! （`EngineCore::materialized_cte_eval_stats`）の差分で固定する。あわせて (a) 結果が CTE を
//! 使わない同値の JOIN と一致すること、(b) 参照者自身の可視行だけが入力になること（RLS-10 (b)）、
//! (c) 本文の可視行が `--max-result-rows` を超えたら黙って切り詰めず `54000`、(d) fail-closed な
//! 拒否（照合・重複列・データ変更 CTE・インライン専用の文脈）、(e) Describe が本文を実行しない
//! ことを、本番経路（`EngineCore::execute_sql_in_session`）で確認する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::result_limits::ResultLimits;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::QueryResult;
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

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(
    storage: &Storage,
    tenant_ctx: &PolicyContext,
    id: u64,
    lang: &str,
    body: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        tenant_ctx,
        id,
        visibility,
        &[
            Value::Vector(vec![id as f32, 0.0]),
            Value::Text(lang.to_string()),
            Value::Text(body.to_string()),
        ],
        &op_id(&format!("seed-{id}")),
    )
    .expect("insert row");
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

fn evals(core: &EngineCore) -> u64 {
    core.materialized_cte_eval_stats().evaluations
}

/// `sql` を実行し、CTE 本文の記憶域に対する評価回数の増分を返す。
fn eval_delta(core: &EngineCore, tenant: &str, sql: &str) -> u64 {
    let before = evals(core);
    ok(core, tenant, sql);
    evals(core) - before
}

/// alice: public 3 件（ja 2・en 1）・private 1 件（ja）。bob: public 1 件（ja）・private 1 件
/// （ja）。carol: public 1 件（ja）。`sql62_cte_materialization_path.rs` と同一の構成。
fn seed_base_fixture(storage: &Storage) {
    let rows: [(&str, u64, &str, &str, Visibility); 7] = [
        ("alice", 1, "ja", "alice public ja 1", Visibility::Public),
        ("alice", 2, "ja", "alice public ja 2", Visibility::Public),
        ("alice", 3, "en", "alice public en", Visibility::Public),
        ("alice", 4, "ja", "alice private ja", Visibility::Private),
        ("bob", 5, "ja", "bob public ja", Visibility::Public),
        ("bob", 6, "ja", "bob private ja", Visibility::Private),
        ("carol", 7, "ja", "carol public ja", Visibility::Public),
    ];
    for (tenant, id, lang, body, vis) in rows {
        insert_row(storage, &ctx(tenant), id, lang, body, vis);
    }
}

fn open_with(limits: Option<ResultLimits>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql62-cte-join-input");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    let core = new_core(storage);
    let core = match limits {
        Some(l) => core.with_result_limits(l),
        None => core,
    };
    (core, guard)
}

/// 自己結合の直接形（CTE を使わない同値の JOIN）。
const DIRECT_SELF: &str = "SELECT a.id, a.lang, b.id FROM docs a JOIN docs b ON a.lang = b.lang \
     ORDER BY a.id, b.id LIMIT 100";

fn cte_self(modifier: &str) -> String {
    format!(
        "WITH c AS {modifier}(SELECT id, lang FROM docs) \
         SELECT a.id, a.lang, b.id FROM c a JOIN c b ON a.lang = b.lang \
         ORDER BY a.id, b.id LIMIT 100"
    )
}

/// 受け入れ条件: 明示 `MATERIALIZED` の CTE を自己結合で 2 回参照しても、本文の実行は 1 回。
#[test]
fn materialized_self_join_evaluates_body_once() {
    let (core, _g) = open_with(None);
    assert_eq!(eval_delta(&core, "alice", &cte_self("MATERIALIZED ")), 1);
}

/// 指定なしの CTE を 2 回参照すると既定で具体化され、本文の実行は 1 回（SQL-62 (3)）。
#[test]
fn default_cte_referenced_twice_is_materialized_once() {
    let (core, _g) = open_with(None);
    assert_eq!(eval_delta(&core, "alice", &cte_self("")), 1);
}

/// `NOT MATERIALIZED` は共有せず、参照ごとに評価する（インライン展開と同じ評価回数）。
#[test]
fn not_materialized_self_join_evaluates_per_reference() {
    let (core, _g) = open_with(None);
    assert_eq!(
        eval_delta(&core, "alice", &cte_self("NOT MATERIALIZED ")),
        2
    );
}

/// 1 回だけ参照される CTE（実表との JOIN）も本文の実行は 1 回。
#[test]
fn single_reference_evaluates_body_once() {
    let (core, _g) = open_with(None);
    let sql = "WITH c AS (SELECT id, lang FROM docs WHERE lang = 'ja') \
               SELECT c.id, d.lang FROM c JOIN docs d ON c.id = d.id ORDER BY c.id LIMIT 100";
    assert_eq!(eval_delta(&core, "alice", sql), 1);
}

/// CTE→CTE の連鎖: 親が `MATERIALIZED` なら、子を経由する参照を含めても親の本文は 1 回だけ実行し、
/// 子は親の評価済みの行から導出する（記憶域を読まない）。
#[test]
fn chained_materialized_parent_is_evaluated_once() {
    let (core, _g) = open_with(None);
    let sql = "WITH a AS MATERIALIZED (SELECT id, lang FROM docs), \
               b AS (SELECT id, lang FROM a WHERE lang = 'ja') \
               SELECT x.id, y.lang FROM a x JOIN b y ON x.id = y.id ORDER BY x.id LIMIT 100";
    assert_eq!(eval_delta(&core, "alice", sql), 1);
}

/// 指定なしの親を 2 つの子が参照すると（参照数 2）既定で共有され、親の本文は 1 回。
#[test]
fn parent_referenced_by_two_children_is_evaluated_once() {
    let (core, _g) = open_with(None);
    let sql = "WITH a AS (SELECT id, lang FROM docs), \
               b AS (SELECT id FROM a WHERE lang = 'ja'), \
               c AS (SELECT id, lang FROM a) \
               SELECT b.id, c.lang FROM b JOIN c ON b.id = c.id ORDER BY b.id LIMIT 100";
    assert_eq!(eval_delta(&core, "alice", sql), 1);
}

/// 単一表の主クエリ（Issue #1860 の経路）も本文の実行は 1 回で、同じカウンタに現れる。
#[test]
fn single_table_main_query_counts_one_evaluation() {
    let (core, _g) = open_with(None);
    let sql = "WITH x AS MATERIALIZED (SELECT * FROM docs) SELECT * FROM x LIMIT 100";
    assert_eq!(eval_delta(&core, "alice", sql), 1);
}

/// Describe は本文を実行せず、カウンタを増やさない。結果列は Execute と一致する。
#[test]
fn describe_does_not_evaluate_the_body_and_matches_execute() {
    let (core, _g) = open_with(None);
    let sql = "WITH c(i, l) AS MATERIALIZED (SELECT id, lang FROM docs) \
               SELECT x.i, y.l FROM c x JOIN c y ON x.l = y.l ORDER BY x.i LIMIT 100";
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
}

/// 結果が CTE を使わない同値の JOIN と一致する（INNER 自己結合）。
#[test]
fn self_join_matches_direct_join() {
    let (core, _g) = open_with(None);
    let direct = ok(&core, "alice", DIRECT_SELF);
    assert!(!direct.rows.is_empty());
    for modifier in ["MATERIALIZED ", "", "NOT MATERIALIZED "] {
        let via_cte = ok(&core, "alice", &cte_self(modifier));
        let cells =
            |r: &QueryResult| -> Vec<_> { r.rows.iter().map(|x| x.cells.clone()).collect() };
        assert_eq!(cells(&via_cte), cells(&direct), "modifier={modifier:?}");
    }
}

/// LEFT・USING・CROSS・実表との JOIN・WHERE（修飾あり・なし）が直接形と一致する。
#[test]
fn join_forms_match_direct_joins() {
    let (core, _g) = open_with(None);
    for (via_cte, direct) in [
        (
            "WITH c AS (SELECT id, lang FROM docs WHERE lang = 'en') \
             SELECT a.id, c.lang FROM docs a LEFT JOIN c ON a.id = c.id ORDER BY a.id LIMIT 100",
            "SELECT a.id, b.lang FROM docs a LEFT JOIN docs b ON a.id = b.id AND b.lang = 'en' \
             ORDER BY a.id LIMIT 100",
        ),
        (
            "WITH c AS (SELECT id, lang FROM docs) \
             SELECT lang, x.id, y.id FROM c x JOIN c y USING (lang) ORDER BY x.id, y.id LIMIT 100",
            "SELECT lang, x.id, y.id FROM docs x JOIN docs y USING (lang) \
             ORDER BY x.id, y.id LIMIT 100",
        ),
        (
            "WITH c AS (SELECT id FROM docs WHERE lang = 'en') \
             SELECT c.id, d.id FROM c CROSS JOIN docs d ORDER BY c.id, d.id LIMIT 100",
            "SELECT c.id, d.id FROM docs c CROSS JOIN docs d WHERE c.lang = 'en' \
             ORDER BY c.id, d.id LIMIT 100",
        ),
        (
            "WITH c AS (SELECT id, lang FROM docs) \
             SELECT a.id, b.id FROM c a JOIN c b ON a.lang = b.lang \
             WHERE a.lang = 'ja' AND b.lang = 'ja' ORDER BY a.id, b.id LIMIT 100",
            "SELECT a.id, b.id FROM docs a JOIN docs b ON a.lang = b.lang \
             WHERE a.lang = 'ja' AND b.lang = 'ja' ORDER BY a.id, b.id LIMIT 100",
        ),
        (
            "WITH c(i, l) AS (SELECT id, lang FROM docs) \
             SELECT c.i, d.lang FROM c JOIN docs d ON c.l = d.lang WHERE c.l = 'en' \
             ORDER BY c.i, d.id LIMIT 100",
            "SELECT c.id, d.lang FROM docs c JOIN docs d ON c.lang = d.lang WHERE c.lang = 'en' \
             ORDER BY c.id, d.id LIMIT 100",
        ),
    ] {
        let want = ok(&core, "alice", direct);
        let got = ok(&core, "alice", via_cte);
        assert!(!want.rows.is_empty(), "{direct}");
        assert_eq!(cells(&got), cells(&want), "{via_cte}");
    }
}

fn cells(r: &QueryResult) -> Vec<Vec<engine::sql::exec::Cell>> {
    r.rows.iter().map(|x| x.cells.clone()).collect()
}

/// CTE と同名の実表は隠される: 実表 `docs` の行が結合に混入せず、CTE の行だけが使われる（P0）。
#[test]
fn cte_shadows_a_table_of_the_same_name() {
    let (core, _g) = open_with(None);
    let sql = "WITH docs AS (SELECT id, lang FROM docs WHERE lang = 'en') \
               SELECT a.id, b.lang FROM docs a JOIN docs b ON a.id = b.id ORDER BY a.id LIMIT 100";
    let got = ok(&core, "alice", sql);
    assert_eq!(got.rows.len(), 1, "only the en row of the CTE: {got:?}");
    // 大文字小文字を変えた綴りでも CTE に解決される（名前の照合は大文字小文字を区別しない）。
    let upper = "WITH docs AS (SELECT id, lang FROM docs WHERE lang = 'en') \
                 SELECT a.id, b.lang FROM DOCS a JOIN docs b ON a.id = b.id ORDER BY a.id LIMIT 100";
    assert_eq!(cells(&ok(&core, "alice", upper)), cells(&got));
}

const DIRECT_SELF_IDS: &str = "SELECT a.id, b.id FROM docs a JOIN docs b ON a.lang = b.lang \
     ORDER BY a.id, b.id LIMIT 100";

/// 3 テナントで同じ文を発行しても、各テナントの結果は自テナントの可視行だけから成る
/// （RLS-10 (b)）。他テナントの行は結合の中間結果にも現れない。
#[test]
fn each_tenant_sees_only_its_own_rows() {
    let (core, _g) = open_with(None);
    let via_cte = "WITH c AS MATERIALIZED (SELECT id, lang FROM docs) \
                   SELECT a.id, b.id FROM c a JOIN c b ON a.lang = b.lang \
                   ORDER BY a.id, b.id LIMIT 100";
    for tenant in ["alice", "bob", "carol"] {
        let via = ok(&core, tenant, via_cte);
        let direct = ok(&core, tenant, DIRECT_SELF_IDS);
        assert!(!via.rows.is_empty(), "tenant={tenant}");
        assert_eq!(cells(&via), cells(&direct), "tenant={tenant}");
    }
    // 結合の入力は各テナントの可視行だけ（他テナントの private 行 id 4・6 は現れない）。
    let ids_sql = "WITH c AS MATERIALIZED (SELECT id, lang FROM docs) \
                   SELECT c.id, d.lang FROM c JOIN docs d ON c.id = d.id ORDER BY c.id LIMIT 100";
    for (tenant, want) in [
        ("alice", vec![1u64, 2, 3, 4, 5, 7]),
        ("bob", vec![1, 2, 3, 5, 6, 7]),
        ("carol", vec![1, 2, 3, 5, 7]),
    ] {
        let got: Vec<u64> = ok(&core, tenant, ids_sql)
            .rows
            .iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(got, want, "tenant={tenant}");
    }
}

/// 本文の可視行が `--max-result-rows` を超えたら、外側の `LIMIT` が小さくても `54000`
/// （黙って切り詰めない。SQL-62 (5)・SQL-34）。上限ちょうどは成功し、判定は可視行だけで行う。
#[test]
fn body_visible_rows_are_capped_by_the_result_limit() {
    let (core, _g) = open_with(Some(ResultLimits::new(5)));
    let sql = "WITH c AS MATERIALIZED (SELECT id, lang FROM docs) \
               SELECT a.id FROM c a JOIN c b ON a.id = b.id LIMIT 2";
    // alice の可視行は 6 件（上限 5 を超える）。
    assert_eq!(code(&core, "alice", sql), "54000");
    // carol の可視行は 5 件（ちょうど上限）。全テナント合計 7 行は判定に影響しない。
    assert_eq!(ok(&core, "carol", sql).rows.len(), 2);
}

/// 本文が公開しない列・`id` は外側（JOIN の WHERE・投影・ON）から参照できない
/// （filter oracle を作らない）。
#[test]
fn columns_not_exposed_by_the_body_are_rejected() {
    let (core, _g) = open_with(None);
    for sql in [
        "WITH c AS (SELECT lang FROM docs) \
         SELECT c.body FROM c JOIN docs d ON c.lang = d.lang LIMIT 10",
        "WITH c AS (SELECT lang FROM docs) \
         SELECT c.lang FROM c JOIN docs d ON c.id = d.id LIMIT 10",
        "WITH c AS (SELECT lang FROM docs) \
         SELECT d.id FROM c JOIN docs d ON c.lang = d.lang WHERE c.body = 'x' LIMIT 10",
    ] {
        let err = query(&core, "alice", sql).expect_err(sql);
        assert!(
            matches!(err.wire_code(), "42703" | "22000" | "42P01"),
            "{sql}: {err:?}"
        );
    }
}

/// 照合（ICU）の列を持つ表を読む CTE は JOIN の入力にできない（`0A000`）。照合が黙って落ちる
/// fail-open を作らない。CTE を入力にしない JOIN は従来どおり成功する。
#[test]
fn collated_table_cte_is_rejected_as_a_join_input() {
    let path = unique_db_path("sql62-cte-join-collation");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = new_core(storage);
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut session,
        "CREATE TABLE ct (id BIGINT PRIMARY KEY, s TEXT COLLATE \"und-x-icu\")",
    )
    .expect("create table");
    core.execute_sql_in_session(
        &ctx("alice"),
        &mut SessionState::default(),
        "INSERT INTO ct (id, s) VALUES (1, 'v1') USING OPERATION_ID 'ins-1'",
    )
    .expect("insert");
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH c AS (SELECT id, s FROM ct) SELECT a.id FROM c a JOIN ct b ON a.id = b.id LIMIT 5"
        ),
        "0A000"
    );
    ok(
        &core,
        "alice",
        "WITH c AS (SELECT id FROM ct) SELECT a.id FROM ct a JOIN ct b ON a.id = b.id LIMIT 5",
    );
}

/// 重複した列名を返す本文・データ変更 CTE を含む文は `0A000`（固定文言）。
#[test]
fn duplicate_columns_and_data_modifying_cte_are_rejected() {
    let (core, _g) = open_with(None);
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH c AS (SELECT lang, lang FROM docs) \
             SELECT a.lang FROM c a JOIN docs d ON a.lang = d.lang LIMIT 5"
        ),
        "0A000"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH c AS (DELETE FROM docs WHERE lang = 'xx' RETURNING id) \
             SELECT a.id FROM docs a JOIN c ON a.id = c.id LIMIT 5"
        ),
        "0A000"
    );
}

/// インライン専用の文脈（CREATE VIEW の本文・`EXPLAIN`）では、CTE を入力に持つ JOIN は
/// 従来どおり `42601` で拒否される。
#[test]
fn inline_only_contexts_still_reject_cte_join() {
    let (core, _g) = open_with(None);
    let mut session = SessionState::default();
    session.allow_ddl();
    let err = core
        .execute_sql_in_session(
            &ctx("alice"),
            &mut session,
            "CREATE VIEW v AS WITH c AS (SELECT id, lang FROM docs) \
             SELECT a.id, b.id FROM c a JOIN c b ON a.lang = b.lang LIMIT 5",
        )
        .expect_err("view body");
    assert_eq!(err.wire_code(), "42601", "{err:?}");
    assert_eq!(
        code(
            &core,
            "alice",
            "EXPLAIN WITH c AS (SELECT id, lang FROM docs) \
             SELECT a.id, b.id FROM c a JOIN c b ON a.lang = b.lang LIMIT 5"
        ),
        "42601"
    );
}

/// VECTOR 列を素通しする CTE も JOIN の入力にできる（`SELECT *` は疑似列 `id` と列を展開する）。
#[test]
fn star_body_with_vector_column_passes_through() {
    let (core, _g) = open_with(None);
    let sql = "WITH c AS (SELECT * FROM docs) \
               SELECT c.id, c.embedding, d.lang FROM c JOIN docs d ON c.id = d.id \
               ORDER BY c.id LIMIT 100";
    let got = ok(&core, "alice", sql);
    assert_eq!(got.rows.len(), 6);
}

/// 明示トランザクション内では、同じトランザクションの未 commit の書き込みを CTE 本文が読める。
#[test]
fn cte_join_reads_uncommitted_rows_in_the_same_transaction() {
    let (core, _g) = open_with(None);
    let c = ctx("alice");
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let mut tx = |sql: &str| core.execute_sql_in_txn(&c, &mut session, &mut txn, sql);
    tx("BEGIN").expect("begin");
    tx(
        "INSERT INTO docs (id, embedding, lang, body) VALUES (900, '[9, 0]', 'zz', 'txn row') \
        USING OPERATION_ID 'txn-ins-1'",
    )
    .expect("insert");
    let sql = "WITH c AS MATERIALIZED (SELECT id, lang FROM docs WHERE lang = 'zz') \
               SELECT a.id, b.lang FROM c a JOIN c b ON a.lang = b.lang LIMIT 10";
    match tx(sql).expect("select in txn") {
        SqlOutcome::Query(r) => assert_eq!(r.rows.len(), 1, "{r:?}"),
        other => panic!("{other:?}"),
    }
    tx("ROLLBACK").expect("rollback");
    assert!(ok(&core, "alice", sql).rows.is_empty());
}

/// VECTOR 列は結合キー・述語に使えない（既存の JOIN 束縛の規則）。CTE 経由でも同じコードで拒否し、
/// `Internal` にならない。
#[test]
fn vector_column_cannot_be_a_join_key_or_filter() {
    let (core, _g) = open_with(None);
    for sql in [
        "WITH c AS (SELECT id, embedding FROM docs) \
         SELECT c.id FROM c JOIN docs d ON c.embedding = d.embedding LIMIT 5",
        "WITH c AS (SELECT id, embedding FROM docs) \
         SELECT c.id FROM c JOIN docs d ON c.id = d.id WHERE c.embedding = '[1, 0]' LIMIT 5",
    ] {
        let err = query(&core, "alice", sql).expect_err(sql);
        assert_ne!(err.wire_code(), "XX000", "{sql}: {err:?}");
    }
}

/// `SELECT *` は CTE の公開列だけを展開する（本文が公開しない列・暗黙の行 id を足さない）。
#[test]
fn star_expands_only_the_cte_public_columns() {
    let (core, _g) = open_with(None);
    let sql = "WITH c AS (SELECT lang FROM docs) \
               SELECT * FROM c JOIN docs d ON c.lang = d.lang LIMIT 100";
    let got = ok(&core, "alice", sql);
    let names: Vec<String> = got.columns.iter().map(|c| format!("{c:?}")).collect();
    // CTE の `lang` 1 列 ＋ 実表 `docs` の全列（疑似列 `id`・embedding・lang・body）。
    assert_eq!(names.len(), 1 + 4, "{names:?}");
}

/// 一時表を読む CTE の JOIN は一時コアで実行され、永続表と混在する文は拒否される（`0A000`）。
#[test]
fn temp_table_cte_join_runs_on_the_temp_core_and_mixed_is_rejected() {
    let (core, _g) = open_with(None);
    let c = ctx("alice");
    let mut session = SessionState::default();
    session.allow_ddl();
    let mut run = |sql: &str| core.execute_sql_in_session(&c, &mut session, sql);
    run("CREATE TEMP TABLE tmp (id BIGINT PRIMARY KEY, k TEXT)").expect("create temp");
    run("INSERT INTO tmp (id, k) VALUES (1, 'a')").expect("insert");
    let sql = "WITH c AS MATERIALIZED (SELECT id, k FROM tmp) \
               SELECT a.id, b.k FROM c a JOIN c b ON a.k = b.k LIMIT 5";
    match run(sql) {
        Ok(SqlOutcome::Query(r)) => assert_eq!(r.rows.len(), 1),
        other => panic!("{other:?}"),
    }
    let mixed = "WITH c AS (SELECT id, k FROM tmp) \
                 SELECT a.id FROM c a JOIN docs d ON a.id = d.id LIMIT 5";
    let err = run(mixed).expect_err("mixed");
    assert_eq!(err.wire_code(), "0A000", "{err:?}");
}
