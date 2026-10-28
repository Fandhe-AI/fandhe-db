//! FROM の派生表の実行（インライン展開／具体化）の結合テスト（SQL-62・Issue #1866、親 #1864）。
//! ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-62・RLS-10 (b)・SQL-34。
//!
//! 派生表は名前を利用者が書けない合成 CTE 1 件として既存の単一表 `WITH` 主クエリ経路で実行される。
//! 本テストは (A) 外側の述語・投影・並びが派生列に対して動く、(B) 他テナントの行が混入しない、
//! (C) CTE 版と結果が一致する、(D) 実行対象外の形は固定エラーで止まる、を固定する。
//! JOIN 入力・入れ子・深さ上限（Issue #1867）は `sql62_derived_table_join.rs`。

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

fn query_err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    query(core, tenant, sql).expect_err("must be rejected")
}

/// alice: public 3 件（ja 2・en 1）・private 1 件（ja）。bob: public 1 件
/// （ja）・private 1 件（ja）。carol: public 1 件（ja）。
/// `table18_view.rs::seed_base_fixture` と同一のフィクスチャ構成
/// （テナント境界検証を同条件で比較できるようにするため）。
fn seed_base_fixture(storage: &Storage) {
    insert_row(
        storage,
        &ctx("alice"),
        1,
        "ja",
        "alice public ja 1",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("alice"),
        2,
        "ja",
        "alice public ja 2",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("alice"),
        3,
        "en",
        "alice public en",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("alice"),
        4,
        "ja",
        "alice private ja",
        Visibility::Private,
    );
    insert_row(
        storage,
        &ctx("bob"),
        5,
        "ja",
        "bob public ja",
        Visibility::Public,
    );
    insert_row(
        storage,
        &ctx("bob"),
        6,
        "ja",
        "bob private ja",
        Visibility::Private,
    );
    insert_row(
        storage,
        &ctx("carol"),
        7,
        "ja",
        "carol public ja",
        Visibility::Public,
    );
}

fn open_with(limits: Option<ResultLimits>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql62-derived-exec");
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

fn open_seeded() -> (EngineCore, CleanupGuard) {
    open_with(None)
}

/// 行の ID・セルの並びを文字列化する（CTE 版との完全一致比較用）。
fn render(r: &QueryResult) -> String {
    format!(
        "{:?}",
        r.rows
            .iter()
            .map(|x| (x.id, format!("{:?}", x.cells)))
            .collect::<Vec<_>>()
    )
}

fn ids(r: &QueryResult) -> Vec<u64> {
    r.rows.iter().map(|x| x.id).collect()
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    query_err(core, tenant, sql).wire_code().to_string()
}

fn msg(core: &EngineCore, tenant: &str, sql: &str) -> String {
    query_err(core, tenant, sql).to_string()
}

#[test]
fn matches_cte_oracle_per_tenant() {
    let (core, _g) = open_seeded();
    for body in [
        "SELECT lang, body FROM docs",
        "SELECT lang, body FROM docs ORDER BY body LIMIT 3",
        "SELECT lang, count(*) AS n FROM docs GROUP BY lang",
        "SELECT lang, row_number() OVER (ORDER BY body) AS rn FROM docs",
    ] {
        for tenant in ["alice", "bob", "carol"] {
            let d = query(&core, tenant, &format!("SELECT * FROM ({body}) t")).expect("derived");
            let c = query(
                &core,
                tenant,
                &format!("WITH t AS ({body}) SELECT * FROM t"),
            )
            .expect("cte");
            assert_eq!(render(&d), render(&c), "{tenant}: {body}");
        }
    }
}

#[test]
fn outer_predicate_projection_and_order_use_derived_columns() {
    let (core, _g) = open_seeded();
    let r = query(
        &core,
        "alice",
        "SELECT t.lang FROM (SELECT lang FROM docs) t WHERE t.lang = 'en'",
    )
    .expect("qualified");
    assert_eq!(ids(&r), vec![3]);
    let r = query(
        &core,
        "alice",
        "SELECT a FROM (SELECT lang, body FROM docs) t(a, b) WHERE b = 'alice public en'",
    )
    .expect("column list");
    assert_eq!(ids(&r), vec![3]);
    // 列名リストで付けた名前に置き換わり、元の名前では参照できない。
    assert_eq!(
        code(
            &core,
            "alice",
            "SELECT lang FROM (SELECT lang FROM docs) t(a)"
        ),
        "42703"
    );
    // 短い列名リストは残りの列を元の名前で参照できる。
    let r = query(
        &core,
        "alice",
        "SELECT a, body FROM (SELECT lang, body FROM docs) t(a) WHERE a = 'en'",
    )
    .expect("short list");
    assert_eq!(ids(&r), vec![3]);
    let r = query(
        &core,
        "alice",
        "SELECT lang FROM (SELECT lang, body FROM docs) t ORDER BY body DESC LIMIT 2 OFFSET 1",
    )
    .expect("order");
    assert_eq!(r.rows.len(), 2);
    // 別名なしでも非修飾参照は動く。修飾参照は 42P01。
    let r = query(
        &core,
        "alice",
        "SELECT lang FROM (SELECT lang FROM docs) WHERE lang = 'en'",
    )
    .expect("no alias");
    assert_eq!(ids(&r), vec![3]);
    assert_eq!(
        code(
            &core,
            "alice",
            "SELECT docs.lang FROM (SELECT lang FROM docs) t"
        ),
        "42P01"
    );
}

#[test]
fn outer_filter_applies_after_body_limit_and_aggregate() {
    let (core, _g) = open_seeded();
    // 本文の LIMIT の後に外側の WHERE が掛かる（先頭 1 件が ja なので en は 0 件）。
    let r = query(
        &core,
        "alice",
        "SELECT lang FROM (SELECT lang FROM docs ORDER BY body LIMIT 1) t WHERE lang = 'en'",
    )
    .expect("limit then where");
    assert_eq!(r.rows.len(), 0);
    // 集計値での絞り込み。
    let r = query(
        &core,
        "alice",
        "SELECT lang FROM (SELECT lang, count(*) AS n FROM docs GROUP BY lang) t WHERE n > 1",
    )
    .expect("agg then where");
    assert_eq!(r.rows.len(), 1);
    // 外側の集計。
    let r = query(
        &core,
        "alice",
        "SELECT count(*) FROM (SELECT lang FROM docs WHERE lang = 'ja') t",
    )
    .expect("outer count");
    assert_eq!(r.rows.len(), 1);
}

#[test]
fn other_tenant_rows_never_appear() {
    let (core, _g) = open_seeded();
    for body in [
        "SELECT lang, body FROM docs",
        "SELECT lang, body FROM docs ORDER BY body LIMIT 100",
        "SELECT lang, count(*) AS n FROM docs GROUP BY lang",
    ] {
        let sql = format!("SELECT * FROM ({body}) t");
        let bob = render(&query(&core, "bob", &sql).expect("bob"));
        let carol = render(&query(&core, "carol", &sql).expect("carol"));
        assert!(!carol.contains("private"), "{carol}");
        assert!(!bob.contains("alice private"), "{bob}");
        assert!(!bob.contains("carol private"), "{bob}");
    }
    let r = query(&core, "carol", "SELECT * FROM (SELECT body FROM docs) t").expect("carol");
    assert_eq!(ids(&r), vec![1, 2, 3, 5, 7]);
    // 集計値も可視行だけから計算される（carol は ja 4・en 1）。
    let r = query(
        &core,
        "carol",
        "SELECT count(*) FROM (SELECT lang FROM docs) t",
    )
    .expect("count");
    assert_eq!(
        format!("{:?}", r.rows.first().map(|x| &x.cells)),
        "Some([Integer(5)])"
    );
}

#[test]
fn materialization_cap_counts_only_visible_rows() {
    let (core, _g) = open_with(Some(ResultLimits::new(5)));
    // alice の可視は 6 行で上限 5 を超える。LIMIT なしの tail 本文は具体化され 54000。
    let sql = "SELECT * FROM (SELECT body FROM docs ORDER BY body) t";
    assert_eq!(code(&core, "alice", sql), "54000");
    // carol の可視はちょうど 5 行（全テナント合計 7 行より少ない）で成功する。
    assert_eq!(query(&core, "carol", sql).expect("at cap").rows.len(), 5);
    // 本文に LIMIT があれば暗黙上限に掛からない。
    let r = query(
        &core,
        "alice",
        "SELECT * FROM (SELECT body FROM docs ORDER BY body LIMIT 2) t",
    )
    .expect("limited");
    assert_eq!(r.rows.len(), 2);
}

#[test]
fn unsupported_shapes_stay_fail_closed() {
    let (core, _g) = open_seeded();
    for (sql, expected) in [
        ("SELECT * FROM LATERAL (SELECT lang FROM docs) t", "0A000"),
        ("EXPLAIN SELECT * FROM (SELECT lang FROM docs) t", "42601"),
        (
            "SELECT * FROM (SELECT lang FROM docs) t WHERE lang IN (SELECT lang FROM docs)",
            "42601",
        ),
    ] {
        assert_eq!(code(&core, "alice", sql), expected, "{sql}");
    }
}

#[test]
fn alias_hides_real_table_and_cte_names() {
    let (core, _g) = open_seeded();
    let r = query(
        &core,
        "alice",
        "SELECT * FROM (SELECT lang FROM docs WHERE lang = 'en') AS docs",
    )
    .expect("alias docs");
    assert_eq!(ids(&r), vec![3]);
    let r = query(
        &core,
        "alice",
        "WITH t AS (SELECT lang FROM docs) SELECT * FROM (SELECT lang FROM docs WHERE lang = 'en') t",
    )
    .expect("alias cte");
    assert_eq!(ids(&r), vec![3]);
}

#[test]
fn errors_hide_internal_names_and_do_not_depend_on_tenant() {
    let (core, _g) = open_seeded();
    for sql in [
        "SELECT nope FROM (SELECT lang FROM docs) t",
        "SELECT * FROM (SELECT lang FROM docs) t(a, b, c)",
        "SELECT * FROM (SELECT lang FROM docs) a, docs d WHERE nope = 1",
        "SELECT x FROM (SELECT lang FROM docs ORDER BY lang) t",
        "SELECT * FROM (SELECT lang FROM docs) t ORDER BY lang <=> '[1,0]'",
    ] {
        let a = (code(&core, "alice", sql), msg(&core, "alice", sql));
        assert!(!a.1.contains("?derived"), "{sql}: {}", a.1);
        for tenant in ["bob", "carol"] {
            assert_eq!(
                a,
                (code(&core, tenant, sql), msg(&core, tenant, sql)),
                "{sql}"
            );
        }
    }
}
