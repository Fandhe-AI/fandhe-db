//! 非再帰 `WITH` 句（CTE）の結合テスト（SQL-29 (b)・RLS-10 (b)、TASK-213、
//! Issue #928）。ポインタ: `docs/spec/05-tasks.md` TASK-213・
//! `docs/spec/04-behavior/sql-surface.md` SQL-29 (b)・`docs/spec/04-behavior/
//! rls.md` RLS-10 (b)・RLS-7・RLS-8。
//!
//! `table18_view.rs`（TABLE-18・SQL-23・TASK-205、Issue #909）と同じ流儀
//! （実 `Storage` ＋ `CpuScalarProvider`、`unique_db_path`／`CleanupGuard`、
//! `engine::tenant::insert_typed_row` による投入、`EngineCore::
//! execute_sql_in_session` を production 経路として使う）。CTE は「クエリの
//! 中だけで有効な名前なしビュー」としてインライン展開されるため、`CREATE
//! VIEW` のような DDL 実行権限ゲートは持たない（`WITH` はどのセッションでも
//! 使える通常の `SELECT` 文の一部）。
//!
//! 検証する契約（本計画の受入基準 1〜4）:
//! 1. 非再帰 CTE を受理する
//! 2. `WITH RECURSIVE` を拒否する
//! 3. CTE の定義数・連鎖の深さに上限を設け、超過は `54000`
//! 4. CTE 経由でも RLS が暗黙に適用される

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
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
    let path = unique_db_path("sql29-cte");
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    seed_base_fixture(&storage);
    (new_core(storage), guard)
}

// --- 受入基準 1: 非再帰 CTE を受理する -----------------------------------------

#[test]
fn accepts_single_cte_and_matches_direct_query() {
    let (core, _guard) = open_seeded();
    let via_cte = query(
        &core,
        "alice",
        "WITH ja AS (SELECT id FROM docs WHERE lang = 'ja') SELECT id FROM ja LIMIT 100",
    )
    .expect("CTE query");
    let direct = query(
        &core,
        "alice",
        "SELECT id FROM docs WHERE lang = 'ja' LIMIT 100",
    )
    .expect("direct query");
    assert_eq!(result_ids(&via_cte), result_ids(&direct));
}

#[test]
fn accepts_cte_chain() {
    let (core, _guard) = open_seeded();
    // 2 段の CTE 連鎖（`b` は `a` を参照する）が、それぞれの述語を合成した
    // 直接クエリと同じ結果になることを固定する（`sql::view::resolve_from` の
    // 「内側から外側へ畳み込む」設計を CTE でも踏襲していることの確認）。
    let via_chain = query(
        &core,
        "alice",
        "WITH a AS (SELECT id, body FROM docs WHERE lang = 'ja'), \
         b AS (SELECT id FROM a WHERE body = 'alice public ja 2') \
         SELECT id FROM b LIMIT 100",
    )
    .expect("CTE chain query");
    let direct = query(
        &core,
        "alice",
        "SELECT id FROM docs WHERE lang = 'ja' AND body = 'alice public ja 2' LIMIT 100",
    )
    .expect("direct query");
    assert_eq!(result_ids(&via_chain), result_ids(&direct));
    assert_eq!(result_ids(&via_chain), vec![2]);
}

#[test]
fn accepts_unreferenced_cte() {
    let (core, _guard) = open_seeded();
    let result = query(
        &core,
        "alice",
        "WITH unused AS (SELECT id FROM docs WHERE lang = 'en') \
         SELECT id FROM docs WHERE lang = 'ja' LIMIT 100",
    )
    .expect("unreferenced CTE must not block the main query");
    assert!(result_ids(&result).contains(&1));
}

// --- 受入基準 2: `WITH RECURSIVE` を拒否する -----------------------------------

#[test]
fn rejects_with_recursive() {
    let (core, _guard) = open_seeded();
    let err = query_err(
        &core,
        "alice",
        "WITH RECURSIVE r AS (SELECT id FROM docs) SELECT id FROM r LIMIT 10",
    );
    assert_eq!(err.wire_code(), "42601");
}

// --- 受入基準 3: 定義数・連鎖の深さの上限 --------------------------------------

#[test]
fn cte_definition_count_over_limit_is_rejected_with_54000() {
    let (core, _guard) = open_seeded();
    // `sql::cte::MAX_CTE_DEFINITIONS` は 16（TASK-213）。1 件多い 17 件は
    // `54000` になる。
    let over = 17;
    let defs: Vec<String> = (0..over)
        .map(|i| format!("c{i} AS (SELECT id FROM docs)"))
        .collect();
    let sql = format!("WITH {} SELECT id FROM c0 LIMIT 10", defs.join(", "));
    let err = query_err(&core, "alice", &sql);
    assert_eq!(err.wire_code(), "54000");
}

#[test]
fn cte_nesting_depth_over_limit_is_rejected_with_54000() {
    let (core, _guard) = open_seeded();
    // `sql::cte::MAX_CTE_NESTING_DEPTH` は 4（TASK-213）。c0..c4 の 5 件連鎖
    // （深さ 5 相当）は `54000` になる。
    let depth: usize = 4;
    let mut defs = vec!["c0 AS (SELECT id FROM docs)".to_string()];
    for i in 1..=depth {
        defs.push(format!("c{i} AS (SELECT id FROM c{})", i - 1));
    }
    let sql = format!("WITH {} SELECT id FROM c{depth} LIMIT 10", defs.join(", "));
    let err = query_err(&core, "alice", &sql);
    assert_eq!(err.wire_code(), "54000");
}

// --- 受入基準 4: CTE 経由でも RLS が暗黙適用される（RLS-10 (b)・RLS-7・RLS-8） ---

#[test]
fn cte_read_applies_referencing_session_rls_not_creator_visibility() {
    let (core, _guard) = open_seeded();
    let sql = "WITH ja AS (SELECT id FROM docs WHERE lang = 'ja') SELECT id FROM ja LIMIT 100";

    for tenant in ["alice", "bob", "carol"] {
        let via_cte = query(&core, tenant, sql).expect("CTE scan");
        let direct = query(
            &core,
            tenant,
            "SELECT id FROM docs WHERE lang = 'ja' LIMIT 100",
        )
        .expect("direct scan");
        assert_eq!(
            result_ids(&via_cte),
            result_ids(&direct),
            "tenant={tenant}: CTE result must match direct query under the SAME session ctx"
        );
    }

    // alice の private 行（id 4）・bob の private 行（id 6）が他テナントへ
    // 混入しない。
    let bob_via_cte = query(&core, "bob", sql).expect("bob CTE scan");
    assert!(!result_ids(&bob_via_cte).contains(&4));
    let carol_via_cte = query(&core, "carol", sql).expect("carol CTE scan");
    assert!(!result_ids(&carol_via_cte).contains(&4));
    assert!(!result_ids(&carol_via_cte).contains(&6));
}

#[test]
fn cte_without_predicate_does_not_leak_other_tenant_rows() {
    let (core, _guard) = open_seeded();
    // 述語なしの CTE（`SELECT * FROM docs`）でも、参照したセッションの RLS が
    // そのまま効くため、他テナントの private 行は混入しない。
    let sql = "WITH all_docs AS (SELECT * FROM docs) SELECT id FROM all_docs LIMIT 100";
    let bob_ids = result_ids(&query(&core, "bob", sql).expect("bob CTE scan"));
    assert!(!bob_ids.contains(&4)); // alice の private 行
    let carol_ids = result_ids(&query(&core, "carol", sql).expect("carol CTE scan"));
    assert!(!carol_ids.contains(&4));
    assert!(!carol_ids.contains(&6)); // bob の private 行
}

// --- 名前解決・カーソル・COPY との併用 ------------------------------------------

#[test]
fn cte_name_hides_real_table_of_the_same_name() {
    let (core, _guard) = open_seeded();
    // `docs` という名前の CTE を定義すると、主クエリの `FROM docs` は
    // 実テーブルではなく CTE を指す（PostgreSQL と同じ名前解決の意味論）。
    let result = query(
        &core,
        "alice",
        "WITH docs AS (SELECT id FROM docs WHERE lang = 'en') SELECT id FROM docs LIMIT 100",
    )
    .expect("CTE name shadowing query");
    assert_eq!(result_ids(&result), vec![3]);
}

#[test]
fn declare_cursor_for_with_query_works() {
    let (core, _guard) = open_seeded();
    let mut session = SessionState::default();
    let mut txn = core.new_session_transaction();
    let alice = ctx("alice");
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "BEGIN")
        .expect("begin");
    let outcome = core
        .execute_sql_in_txn(
            &alice,
            &mut session,
            &mut txn,
            "DECLARE cur CURSOR FOR WITH ja AS (SELECT id FROM docs WHERE lang = 'ja') \
             SELECT id FROM ja LIMIT 100",
        )
        .expect("declare cursor over WITH query");
    assert_eq!(outcome, SqlOutcome::DeclareCursor);

    let outcome = core
        .execute_sql_in_txn(&alice, &mut session, &mut txn, "FETCH 100 FROM cur")
        .expect("fetch");
    let SqlOutcome::Fetch(result) = outcome else {
        panic!("expected Fetch outcome, got {outcome:?}");
    };
    assert!(result_ids(&result).contains(&1));

    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "CLOSE cur")
        .expect("close");
    core.execute_sql_in_txn(&alice, &mut session, &mut txn, "COMMIT")
        .expect("commit");
}
