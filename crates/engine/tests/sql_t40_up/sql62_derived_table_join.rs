//! FROM の派生表を JOIN／カンマ区切りの入力にする形と、入れ子の深さ上限の結合テスト
//! （SQL-62・Issue #1867、親 #1864）。ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-62・
//! RLS-10 (b)・SQL-34。
//!
//! 派生表は名前を利用者が書けない合成 CTE として `WITH`＋JOIN 主クエリ経路の slot で評価される。
//! 本テストは (A) CTE 版と結果が一致する、(B) JOIN 経由でも他テナントの行が混入しない、
//! (C) 入れ子の深さ上限（`54000`）、(D) 未対応の形は固定エラーで止まり内部名が漏れない、(E) EXPLAIN は
//! 対象外（`42601`）、を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::nesting_limits::NestingLimits;
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
    open_full(limits, None)
}

fn open_full(
    limits: Option<ResultLimits>,
    nesting: Option<NestingLimits>,
) -> (EngineCore, CleanupGuard) {
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
    let core = match nesting {
        Some(n) => core.with_nesting_limits(n),
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

/// 深さ `n` の入れ子（最内側が実表）。例: n=2 は `(SELECT lang FROM (SELECT lang FROM docs) a1) a0`。
fn nested(n: usize) -> String {
    let mut inner = "docs".to_string();
    for k in (0..n).rev() {
        inner = format!("(SELECT lang FROM {inner}) a{k}");
    }
    format!("SELECT * FROM {inner}")
}

#[test]
fn join_matches_cte_oracle_per_tenant() {
    let (core, _g) = open_seeded();
    for (derived, cte) in [
        (
            "SELECT d.id, t.lang FROM docs d JOIN (SELECT lang FROM docs WHERE lang = 'en') t \
             ON d.lang = t.lang ORDER BY d.id LIMIT 100",
            "WITH t AS (SELECT lang FROM docs WHERE lang = 'en') SELECT d.id, t.lang FROM docs d \
             JOIN t ON d.lang = t.lang ORDER BY d.id LIMIT 100",
        ),
        (
            "SELECT a.id, b.id FROM (SELECT id, lang FROM docs) a JOIN (SELECT id, body FROM docs) b \
             ON a.id = b.id ORDER BY a.id LIMIT 100",
            "WITH a AS (SELECT id, lang FROM docs), b AS (SELECT id, body FROM docs) SELECT a.id, b.id \
             FROM a JOIN b ON a.id = b.id ORDER BY a.id LIMIT 100",
        ),
        (
            "SELECT d.id, t.lang FROM docs d, (SELECT lang FROM docs WHERE lang = 'en') t \
             ORDER BY d.id LIMIT 100",
            "WITH t AS (SELECT lang FROM docs WHERE lang = 'en') SELECT d.id, t.lang FROM docs d, t \
             ORDER BY d.id LIMIT 100",
        ),
        (
            "WITH c AS (SELECT id, lang FROM docs) SELECT x.id, d.id FROM (SELECT * FROM c) x \
             JOIN docs d ON x.id = d.id ORDER BY x.id LIMIT 100",
            "WITH c AS (SELECT id, lang FROM docs), x AS (SELECT * FROM c) SELECT x.id, d.id FROM x \
             JOIN docs d ON x.id = d.id ORDER BY x.id LIMIT 100",
        ),
        (
            "SELECT d.id, t.lang FROM docs d JOIN (SELECT id, lang FROM docs ORDER BY body LIMIT 3) t \
             ON d.id = t.id ORDER BY d.id LIMIT 100",
            "WITH t AS (SELECT id, lang FROM docs ORDER BY body LIMIT 3) SELECT d.id, t.lang FROM docs d \
             JOIN t ON d.id = t.id ORDER BY d.id LIMIT 100",
        ),
    ] {
        for tenant in ["alice", "bob", "carol"] {
            let d = query(&core, tenant, derived).expect("derived");
            let c = query(&core, tenant, cte).expect("cte");
            assert_eq!(render(&d), render(&c), "{tenant}: {derived}");
        }
    }
}

#[test]
fn join_never_exposes_other_tenant_rows() {
    let (core, _g) = open_seeded();
    // id 6 は bob の private 行。両側が派生表でも alice・carol の結果に現れない。
    let sql = "SELECT a.id FROM (SELECT id, lang FROM docs) a JOIN (SELECT id, body FROM docs) b \
               ON a.id = b.id ORDER BY a.id LIMIT 100";
    assert!(!ids(&query(&core, "alice", sql).expect("alice")).contains(&6));
    assert!(!ids(&query(&core, "carol", sql).expect("carol")).contains(&6));
    assert!(ids(&query(&core, "bob", sql).expect("bob")).contains(&6));
    // 実表側が派生表の別名と同名でも実表は読まれない（可視行だけ）。
    let sql = "SELECT docs.id, d.id FROM (SELECT id, lang FROM docs WHERE lang = 'en') docs \
               JOIN docs d ON docs.id = d.id ORDER BY docs.id LIMIT 100";
    assert_eq!(ids(&query(&core, "alice", sql).expect("alias")), vec![3]);
    // 公開行（id 3）は他テナントからも可視。非公開行は alias に隠れた実表経由でも混ざらない。
    assert_eq!(ids(&query(&core, "bob", sql).expect("bob alias")), vec![3]);
}

#[test]
fn column_list_applies_to_join_columns_and_on() {
    let (core, _g) = open_seeded();
    let r = query(
        &core,
        "alice",
        "SELECT d.id, t.k FROM docs d JOIN (SELECT lang FROM docs WHERE lang = 'en') t(k) \
         ON d.lang = t.k ORDER BY d.id LIMIT 100",
    )
    .expect("column list");
    assert_eq!(ids(&r), vec![3]);
    // 列名リストで隠れた元の列名は ON で使えない。
    assert!(query(
        &core,
        "alice",
        "SELECT d.id FROM docs d JOIN (SELECT lang FROM docs) t(k) ON d.lang = t.lang"
    )
    .is_err());
}

#[test]
fn materialized_join_input_counts_only_visible_rows() {
    let (core, _g) = open_with(Some(ResultLimits::new(5)));
    // alice の可視は 6 行で上限 5 を超える（LIMIT なしの tail 本文は具体化され 54000）。
    let sql = "SELECT d.id FROM docs d JOIN (SELECT id, body FROM docs ORDER BY body) t \
               ON d.id = t.id LIMIT 5";
    assert_eq!(code(&core, "alice", sql), "54000");
    // carol の可視はちょうど 5 行で成功する。
    let r = query(&core, "carol", sql);
    assert!(r.is_ok(), "{:?}", r.err());
}

#[test]
fn nesting_depth_boundary_matches_subquery_limit() {
    let (core, _g) = open_seeded();
    for n in 1..=4 {
        let r = query(&core, "alice", &nested(n)).expect("within limit");
        assert_eq!(r.rows.len(), 6, "depth {n}");
    }
    for n in 5..=7 {
        assert_eq!(code(&core, "alice", &nested(n)), "54000", "depth {n}");
    }
    // 深さ超過の判定は内側本文の構文エラーより先に行う。
    let sql = "SELECT * FROM (SELECT a FROM (SELECT a FROM (SELECT a FROM (SELECT a FROM \
               (SELECT FROM docs) e) d) c) b) a0";
    assert_eq!(code(&core, "alice", sql), "54000");
    // 上限内なら構文エラーは 42601。
    assert_eq!(
        code(
            &core,
            "alice",
            "SELECT * FROM (SELECT a FROM (SELECT FROM docs) b) a0"
        ),
        "42601"
    );
}

#[test]
fn lowered_operator_limit_applies_to_nesting() {
    let defaults = NestingLimits::default();
    let lowered = NestingLimits::new(
        2,
        defaults.max_subquery_executions(),
        defaults.max_cte_definitions(),
        defaults.max_cte_nesting_depth(),
    );
    let (core, _g) = open_full(None, Some(lowered));
    assert!(query(&core, "alice", &nested(2)).is_ok());
    assert_eq!(code(&core, "alice", &nested(3)), "54000");
}

#[test]
fn nested_derived_tables_feed_a_join() {
    let (core, _g) = open_seeded();
    let r = query(
        &core,
        "alice",
        "SELECT d.id, t.lang FROM docs d JOIN (SELECT lang FROM (SELECT lang FROM docs \
         WHERE lang = 'en') x) t ON d.lang = t.lang ORDER BY d.id LIMIT 100",
    )
    .expect("nested in join");
    assert_eq!(ids(&r), vec![3]);
}

#[test]
fn too_many_derived_tables_is_54000() {
    let (core, _g) = open_seeded();
    let list: Vec<String> = (0..9)
        .map(|i| format!("(SELECT lang FROM docs) t{i}"))
        .collect();
    let sql = format!("SELECT * FROM {}", list.join(", "));
    assert_eq!(code(&core, "alice", &sql), "54000");
}

#[test]
fn inherited_join_input_restrictions_stay_fail_closed() {
    let (core, _g) = open_seeded();
    for (sql, expected) in [
        // 集計本文は JOIN の入力にできない。
        (
            "SELECT d.id FROM docs d JOIN (SELECT lang, count(*) AS n FROM docs GROUP BY lang) t \
             ON d.lang = t.lang",
            "0A000",
        ),
        // 入れ子の内側が tail（LIMIT）だと外側の合成 CTE から参照できない。
        (
            "SELECT d.id FROM docs d JOIN (SELECT lang FROM (SELECT lang FROM docs LIMIT 2) x) t \
             ON d.lang = t.lang",
            "0A000",
        ),
        // 本文内で内側の別名による修飾は未対応。
        (
            "SELECT d.id FROM docs d JOIN (SELECT x.lang FROM (SELECT lang FROM docs) x) t \
             ON d.lang = t.lang",
            "42601",
        ),
        // 別名なしの派生表は複数リレーションの FROM では受け付けない。
        (
            "SELECT * FROM docs d JOIN (SELECT lang FROM docs) ON d.lang = lang",
            "0A000",
        ),
        ("SELECT * FROM docs d, (SELECT lang FROM docs)", "0A000"),
        // EXPLAIN は対象外（CTE の EXPLAIN 非対応と同じ扱い）。
        (
            "EXPLAIN SELECT * FROM docs d JOIN (SELECT lang FROM docs) t ON d.lang = t.lang",
            "42601",
        ),
        (
            "EXPLAIN SELECT * FROM (SELECT lang FROM (SELECT lang FROM docs) x) t",
            "42601",
        ),
        // LATERAL は対象外。
        (
            "SELECT * FROM docs d JOIN LATERAL (SELECT lang FROM docs) t ON true",
            "0A000",
        ),
    ] {
        assert_eq!(code(&core, "alice", sql), expected, "{sql}");
    }
}

#[test]
fn data_modifying_cte_with_derived_join_writes_nothing() {
    let (core, _g) = open_seeded();
    let before = query(&core, "alice", "SELECT lang FROM docs")
        .expect("select")
        .rows
        .len();
    let sql = "WITH w AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) \
               SELECT d.id FROM docs d JOIN (SELECT lang FROM docs) t ON d.lang = t.lang";
    assert_eq!(code(&core, "alice", sql), "0A000");
    let after = query(&core, "alice", "SELECT lang FROM docs").expect("select");
    assert_eq!(after.rows.len(), before);
}

#[test]
fn internal_names_never_leak_and_errors_are_tenant_independent() {
    let (core, _g) = open_seeded();
    for sql in [
        "SELECT * FROM docs d JOIN (SELECT lang FROM docs) ON d.lang = lang",
        "SELECT d.id FROM docs d JOIN (SELECT lang FROM docs) t ON d.lang = t.nope",
        "SELECT * FROM docs d JOIN (SELECT lang FROM docs) t(a, b) ON d.lang = t.a",
        "SELECT d.id FROM docs d JOIN (SELECT lang FROM (SELECT lang FROM docs LIMIT 2) x) t ON d.lang = t.lang",
        "SELECT d.id FROM docs d JOIN (SELECT lang, count(*) AS n FROM docs GROUP BY lang) t ON d.lang = t.lang",
        "SELECT * FROM (SELECT lang FROM (SELECT lang FROM (SELECT lang FROM (SELECT lang FROM (SELECT lang FROM docs) a) b) c) d) e",
        "SELECT * FROM (SELECT lang FROM docs) a, (SELECT lang FROM nope) b",
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
    // 成功した JOIN の結果列名にも内部名は出ない。
    let r = query(
        &core,
        "alice",
        "SELECT * FROM docs d JOIN (SELECT lang FROM docs) t ON d.lang = t.lang LIMIT 1",
    )
    .expect("join");
    assert!(!format!("{:?}", r.columns).contains("?derived"));
}
