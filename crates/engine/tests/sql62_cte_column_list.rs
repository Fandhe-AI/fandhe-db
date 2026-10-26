//! CTE 列名リスト `WITH <name>(a, b) AS (...)` の結合テスト（SQL-62 (2)・Issue #1857、
//! 親 Issue #1856）。ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-62 (2)・
//! `docs/spec/04-behavior/error-format.md`（`42P10`）・RLS-10 (b)。
//!
//! `sql29_cte.rs` と同じ流儀（実 `Storage` ＋ `CpuScalarProvider`、
//! `EngineCore::execute_sql_in_session` を production 経路として使う）。列名リストは
//! ビューの列別名（TABLE-41）と同じ「公開名 → 物理名」の対応で実現されるため、外側の
//! `WHERE`／`ORDER BY`／集計は別名で書け、元の名前は公開列の外として拒否される。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult};
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

fn query_err(core: &EngineCore, tenant: &str, sql: &str) -> SqlSurfaceError {
    query(core, tenant, sql).expect_err("must be rejected")
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    ids
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

fn open_seeded() -> (EngineCore, CleanupGuard) {
    let path = unique_db_path("sql62-cte-col");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    (new_core(storage), guard)
}

fn names(result: &QueryResult) -> Vec<String> {
    result
        .columns
        .iter()
        .map(|c| match c {
            ColumnMeta::Id => "id".to_string(),
            ColumnMeta::AliasedId { name }
            | ColumnMeta::Scalar { name, .. }
            | ColumnMeta::Computed { name, .. } => name.clone(),
        })
        .collect()
}

fn code(core: &EngineCore, tenant: &str, sql: &str) -> String {
    query_err(core, tenant, sql).wire_code().to_string()
}

#[test]
fn full_list_renames_output_and_is_usable_in_queries() {
    let (core, _g) = open_seeded();
    let r = query(
        &core,
        "alice",
        "WITH c (a, b) AS (SELECT lang, body FROM docs) SELECT a, b FROM c LIMIT 100",
    )
    .expect("full list");
    assert_eq!(names(&r), ["a", "b"]);
    assert_eq!(result_ids(&r), vec![1, 2, 3, 4, 5, 7]);

    let ja = query(
        &core,
        "alice",
        "WITH c (a, b) AS (SELECT lang, body FROM docs) \
         SELECT b FROM c WHERE a = 'ja' ORDER BY b LIMIT 100",
    )
    .expect("alias in WHERE / ORDER BY");
    assert_eq!(names(&ja), ["b"]);
    assert_eq!(result_ids(&ja), vec![1, 2, 4, 5, 7]);
    assert_eq!(ja.rows[0].cells[0], Cell::Text("alice private ja".into()));
}

#[test]
fn short_list_keeps_remaining_column_names() {
    let (core, _g) = open_seeded();
    let r = query(
        &core,
        "alice",
        "WITH c (a) AS (SELECT lang, body FROM docs) SELECT a, body FROM c LIMIT 100",
    )
    .expect("short list");
    assert_eq!(names(&r), ["a", "body"]);

    let star = query(
        &core,
        "alice",
        "WITH c (x) AS (SELECT * FROM docs) SELECT * FROM c LIMIT 100",
    )
    .expect("short list over star");
    assert_eq!(names(&star), ["x", "embedding", "lang", "body"]);
    assert_eq!(result_ids(&star), vec![1, 2, 3, 4, 5, 7]);
}

#[test]
fn star_body_with_aliased_id_is_rejected_fail_closed_in_where() {
    let (core, _g) = open_seeded();
    // 疑似列 `id` の別名を外側の述語で参照する形は、ビューの列別名と同じく `0A000`
    // （黙って別の行集合を返さない）。投影だけなら受理される。
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH c (x) AS (SELECT * FROM docs) SELECT x FROM c WHERE x = 2 LIMIT 100"
        ),
        "0A000"
    );
    let r = query(
        &core,
        "alice",
        "WITH c (x) AS (SELECT * FROM docs) SELECT x FROM c LIMIT 100",
    )
    .expect("projection of aliased id");
    assert_eq!(names(&r), ["x"]);
}

#[test]
fn list_longer_than_output_is_42p10_referenced_or_not() {
    let (core, _g) = open_seeded();
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH c (a, b, d) AS (SELECT lang, body FROM docs) SELECT a FROM c LIMIT 10"
        ),
        "42P10"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH c (a, b, d) AS (SELECT lang, body FROM docs) SELECT id FROM docs LIMIT 10"
        ),
        "42P10"
    );
    let err = query_err(
        &core,
        "alice",
        "WITH c (a, b, d) AS (SELECT lang, body FROM docs) SELECT a FROM c LIMIT 10",
    );
    assert_eq!(
        err.to_string(),
        "WITH query \"c\" has 2 columns available but 3 columns specified"
    );
}

#[test]
fn original_name_is_outside_the_public_columns() {
    let (core, _g) = open_seeded();
    for sql in [
        "WITH c (a) AS (SELECT lang FROM docs) SELECT lang FROM c LIMIT 10",
        "WITH c (a) AS (SELECT lang FROM docs) SELECT a FROM c WHERE lang = 'ja' LIMIT 10",
        "WITH c (a, b) AS (SELECT * FROM docs) SELECT a FROM c WHERE tenant_id = 'bob' LIMIT 10",
    ] {
        let c = code(&core, "alice", sql);
        assert!(c == "42703" || c == "22000", "sql={sql} code={c}");
    }
}

#[test]
fn aggregate_main_query_uses_aliases() {
    let (core, _g) = open_seeded();
    let r = query(
        &core,
        "alice",
        "WITH c (a) AS (SELECT lang FROM docs) \
         SELECT a, COUNT(*) FROM c GROUP BY a ORDER BY a",
    )
    .expect("aggregate over aliased CTE");
    assert_eq!(names(&r)[0], "a");
    assert_eq!(r.rows.len(), 2);
    assert_eq!(r.rows[0].cells[0], Cell::Text("en".into()));
    assert_eq!(r.rows[0].cells[1], Cell::Integer(1));
    assert_eq!(r.rows[1].cells[0], Cell::Text("ja".into()));
    assert_eq!(r.rows[1].cells[1], Cell::Integer(5));
}

#[test]
fn cte_chain_propagates_aliases() {
    let (core, _g) = open_seeded();
    let r = query(
        &core,
        "alice",
        "WITH a (x) AS (SELECT lang FROM docs), \
         b AS (SELECT x FROM a WHERE x = 'ja') SELECT x FROM b LIMIT 100",
    )
    .expect("chain");
    assert_eq!(names(&r), ["x"]);
    assert_eq!(result_ids(&r), vec![1, 2, 4, 5, 7]);

    let renamed = query(
        &core,
        "alice",
        "WITH a (x, y) AS (SELECT lang, body FROM docs), \
         b (p) AS (SELECT y, x FROM a WHERE x = 'en') SELECT p, x FROM b LIMIT 100",
    )
    .expect("chain with second alias list");
    assert_eq!(names(&renamed), ["p", "x"]);
    assert_eq!(
        renamed.rows[0].cells[0],
        Cell::Text("alice public en".into())
    );
}

#[test]
fn syntax_errors_in_list() {
    let (core, _g) = open_seeded();
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH c () AS (SELECT lang FROM docs) SELECT lang FROM c LIMIT 1"
        ),
        "42601"
    );
    assert_eq!(
        code(
            &core,
            "alice",
            "WITH c (a, a) AS (SELECT lang, body FROM docs) SELECT a FROM c LIMIT 1"
        ),
        "42701"
    );
}

#[test]
fn rls_still_applies_through_aliased_cte() {
    let (core, _g) = open_seeded();
    let sql = "WITH c (a) AS (SELECT lang FROM docs) SELECT a FROM c LIMIT 100";
    // 列名リスト付き CTE でも、各セッション自身の可視範囲（RLS-10 (b)）だけが返る。
    // 直接クエリと同じ集合で、他テナントの private 行（alice の 4・bob の 6）は混ざらない。
    for tenant in ["alice", "bob", "carol"] {
        let via = result_ids(&query(&core, tenant, sql).unwrap());
        let direct = result_ids(&query(&core, tenant, "SELECT id FROM docs LIMIT 100").unwrap());
        assert_eq!(via, direct, "tenant={tenant}");
    }
    let bob = result_ids(&query(&core, "bob", sql).unwrap());
    assert!(!bob.contains(&4) && bob.contains(&6));
    let carol = result_ids(&query(&core, "carol", sql).unwrap());
    assert!(!carol.contains(&4) && !carol.contains(&6));
}

#[test]
fn definition_count_limit_still_applies_with_lists() {
    let (core, _g) = open_seeded();
    let defs: Vec<String> = (0..17)
        .map(|i| format!("c{i} (a) AS (SELECT lang FROM docs)"))
        .collect();
    let sql = format!("WITH {} SELECT a FROM c0 LIMIT 10", defs.join(", "));
    assert_eq!(code(&core, "alice", &sql), "54000");
}
