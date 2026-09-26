//! `execute_insert`（`sql::exec`）・`build_explain_result`（`sql::explain`。
//! `ExplainEngine`・`AnnPlan`・`ScalarPlan`・`classify_ann_plan`・
//! `classify_scalar_plan` を含む）が engine クレート外から到達可能な公開 API
//! であることを固定する結合テスト（TASK-186・NOSQL-6・NOSQL-10 の前提。
//! Issue #730）。
//!
//! insert 側は `tests/sql_aggregate_public_api.rs` と同じ流儀
//! （`Storage` を直接操作し `PolicyContext::with_visibilities` で RLS 境界を
//! 確認する）。explain 側は `tests/sql_explain.rs::StubLlmClient` と同型の
//! 決定的スタブで `EngineCore::plan_query_with_mode` を経由し、`EXPLAIN`
//! （`execute_sql_in_session` の `Statement::Explain` アーム）が実際に構築する
//! 行と、engine クレート外から `ExplainEngine::new` 経由で組み立てた
//! `build_explain_result` の出力が**行単位で完全一致**することを固定する
//! （#765 の受け入れ条件の先取り）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::row_codec::Value;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{execute_insert, Cell, ColumnMeta, InsertOutcome};
use engine::sql::explain::{build_explain_result, ExplainEngine};
use engine::sql::mode::{SearchMode, SessionState};
use engine::sql::parser::BoundInsert;
use engine::sql::plan::{EvaluationOrder, ExecutionPlan};
use engine::sql::{
    classify_ann_plan, classify_scalar_plan, AnnShapeInput, ScalarShapeInput, SqlOutcome,
};
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn insert_schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str, visibilities: impl IntoIterator<Item = Visibility>) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, visibilities).expect("valid tenant ctx")
}

fn bound_insert(id: u64, op_id: &str) -> BoundInsert {
    BoundInsert {
        table: TABLE.to_string(),
        id,
        values: vec![
            Value::Vector(vec![id as f32, 0.0, 0.0, 0.0]),
            Value::Text("ja".to_string()),
        ],
        operation_id: Some(OperationId::parse(op_id).expect("valid operation_id")),
    }
}

// ---------------------------------------------------------------------
// insert 側: execute_insert の外部到達性と契約固定
// ---------------------------------------------------------------------

#[test]
fn execute_insert_is_reachable_and_writes_private_row() {
    let path = unique_db_path("sql-insert-public-api-reachable");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&insert_schema())
        .expect("create table");

    let ctx_a = ctx("tenant-a", [Visibility::Public]);
    let ctx_a_private_visible = ctx("tenant-a", [Visibility::Public, Visibility::Private]);
    let ctx_b = ctx("tenant-b", [Visibility::Public, Visibility::Private]);

    let bound = bound_insert(1, "insert-public-api-op-1");
    let outcome = execute_insert(&storage, &ctx_a, &bound, LedgerMode::Ledgered)
        .expect("execute_insert should succeed via the public API");
    assert_eq!(
        outcome,
        InsertOutcome {
            rows_affected: 1,
            incremental: None,
        }
    );

    // `execute_insert` は可視性を常に `Private` に固定する（`BoundInsert` に
    // 可視性フィールドは存在しない）ため、`Public` のみを可視とする ctx から
    // は見えない。
    let rows_public_only = engine::tenant::visible_rows(&storage, TABLE, &ctx_a).expect("scan");
    assert!(
        rows_public_only.is_empty(),
        "Private 固定のため Public のみの ctx からは不可視のはず"
    );

    let rows_a =
        engine::tenant::visible_rows(&storage, TABLE, &ctx_a_private_visible).expect("scan");
    assert_eq!(rows_a.len(), 1);
    assert_eq!(rows_a[0].id, 1);

    // 他テナント（tenant-b）からは同一 id でも一切見えない（TABLE-12・RLS-9）。
    let rows_b = engine::tenant::visible_rows(&storage, TABLE, &ctx_b).expect("scan");
    assert!(rows_b.is_empty());
}

#[test]
fn execute_insert_rejects_missing_operation_id_under_ledgered() {
    let path = unique_db_path("sql-insert-public-api-missing-op-id");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&insert_schema())
        .expect("create table");
    let ctx_a = ctx("tenant-a", [Visibility::Public]);

    let bound = BoundInsert {
        table: TABLE.to_string(),
        id: 1,
        values: vec![
            Value::Vector(vec![1.0, 0.0, 0.0, 0.0]),
            Value::Text("ja".to_string()),
        ],
        operation_id: None,
    };

    let err = execute_insert(&storage, &ctx_a, &bound, LedgerMode::Ledgered)
        .expect_err("operation_id 省略は Ledgered 構成では拒否される");
    assert!(matches!(err, SqlSurfaceError::MissingOperationId));
}

#[test]
fn execute_insert_rejects_resend_and_id_conflict_within_tenant() {
    let path = unique_db_path("sql-insert-public-api-conflicts");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&insert_schema())
        .expect("create table");
    let ctx_a = ctx("tenant-a", [Visibility::Public]);

    let first = bound_insert(1, "insert-public-api-conflict-op");
    execute_insert(&storage, &ctx_a, &first, LedgerMode::Ledgered).expect("first insert succeeds");

    // 同一 operation_id の再送。
    let resend = bound_insert(1, "insert-public-api-conflict-op");
    let err = execute_insert(&storage, &ctx_a, &resend, LedgerMode::Ledgered)
        .expect_err("同一 operation_id の再送は拒否される");
    assert!(matches!(err, SqlSurfaceError::DuplicateOperationId));

    // 別 operation_id・同一 id。
    let id_conflict = bound_insert(1, "insert-public-api-different-op");
    let err = execute_insert(&storage, &ctx_a, &id_conflict, LedgerMode::Ledgered)
        .expect_err("同一 id への 2 回目の書き込みは拒否される");
    assert!(matches!(err, SqlSurfaceError::IdConflict));
}

#[test]
fn execute_insert_other_tenant_same_id_succeeds() {
    let path = unique_db_path("sql-insert-public-api-other-tenant");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&insert_schema())
        .expect("create table");

    let ctx_a = ctx("tenant-a", [Visibility::Public]);
    let ctx_b = ctx("tenant-b", [Visibility::Public]);

    let bound_a = bound_insert(1, "insert-public-api-shared-op");
    execute_insert(&storage, &ctx_a, &bound_a, LedgerMode::Ledgered)
        .expect("tenant-a insert succeeds");

    // 物理キーは (tenant_id, id) のため、他テナントが同じ id・同じ
    // operation_id 文字列を使っても成功する（存在オラクルにならない）。
    let bound_b = bound_insert(1, "insert-public-api-shared-op");
    execute_insert(&storage, &ctx_b, &bound_b, LedgerMode::Ledgered)
        .expect("tenant-b insert succeeds");
}

#[test]
fn execute_insert_compare_only_mode_skips_ledger() {
    let path = unique_db_path("sql-insert-public-api-compare-only");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&insert_schema())
        .expect("create table");
    let ctx_a = ctx("tenant-a", [Visibility::Public]);

    let bound = BoundInsert {
        table: TABLE.to_string(),
        id: 1,
        values: vec![
            Value::Vector(vec![1.0, 0.0, 0.0, 0.0]),
            Value::Text("ja".to_string()),
        ],
        operation_id: None,
    };

    // `LedgerMode::CompareOnlyWithoutLedger` は `EngineCore::with_ledger_mode`
    // 経由で既にクレート外から選択可能な既存構成。本 Issue で新規に開いた
    // バイパスではないことを固定する。
    execute_insert(
        &storage,
        &ctx_a,
        &bound,
        LedgerMode::CompareOnlyWithoutLedger,
    )
    .expect("CompareOnlyWithoutLedger では operation_id 省略でも成功する");
}

// ---------------------------------------------------------------------
// explain 側: build_explain_result の外部到達性と SQL EXPLAIN との一致
// ---------------------------------------------------------------------

fn explain_schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn seeded_explain_storage(path: &std::path::Path) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&explain_schema())
        .expect("create table");
    let ctx_a = ctx("tenant-a", [Visibility::Public, Visibility::Private]);
    let op_id = OperationId::parse("insert-explain-public-api-op-1").expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx_a,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![0.1, 0.2, 0.3, 0.4]),
            Value::Text("docs/a.md".to_string()),
            Value::Text("alpha content in english".to_string()),
        ],
        &op_id,
    )
    .expect("insert row");
    storage
}

/// `tests/sql_explain.rs::StubLlmClient` と同型の決定的スタブ（実 Ollama 疎通は
/// スコープ外）。
struct StubLlmClient {
    response: &'static str,
}

impl LlmClient for StubLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(self.response.to_string())
    }
}

const EXPANSION_RESPONSE: &str =
    r#"{"search_terms": ["alpha", "beta"], "path_hint": "docs/", "kind_hint": "fn"}"#;

fn explain_result_lines(outcome: SqlOutcome) -> Vec<String> {
    match outcome {
        SqlOutcome::Explain(result) => {
            assert_eq!(result.columns.len(), 1);
            assert_eq!(
                result.columns[0],
                ColumnMeta::Computed {
                    name: "QUERY PLAN".to_string()
                }
            );
            result
                .rows
                .iter()
                .map(|row| match &row.cells[0] {
                    Cell::Text(s) => s.clone(),
                    other => panic!("expected Cell::Text, got {other:?}"),
                })
                .collect()
        }
        other => panic!("expected SqlOutcome::Explain, got {other:?}"),
    }
}

#[test]
fn build_explain_result_is_reachable_and_matches_sql_explain_rows() {
    let path = unique_db_path("sql-insert-explain-public-api-match");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_explain_storage(&path);
    // `Storage` を drop してから `EngineCore::open` で同じパスを再オープンする
    // （`redb::Database` は同時に複数ハンドルを開けないため。`sql_scan_public_api.rs`
    // の drop パターンと同じ理由）。
    drop(storage);

    // `EngineCore::open`（既定エンジン `ParallelBruteForce`）経由で構築する。
    // `from_storage` はカスタム provider 注入経路であり
    // `search_engine_kind() == None` になってしまうため、`engine:` 行の値が
    // 既知になる `open` を使う（`sql_explain.rs` はあえて `from_storage` を
    // 使って `(custom_provider)` 経路を固定しており、本テストは対照的に
    // 既知エンジン経路を固定する）。
    let core = EngineCore::open(&path)
        .expect("open engine core")
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));
    assert_eq!(
        core.search_engine_kind(),
        Some(engine::search_engine::SearchEngineKind::ParallelBruteForce)
    );

    let tenant_ctx = ctx("tenant-a", [Visibility::Public, Visibility::Private]);

    // (a) SQL 表層の EXPLAIN 経路（`core.rs::EngineCore::execute_sql_in_session`
    // の `Statement::Explain` アーム）。
    let mut session = SessionState::default();
    let sql_outcome = core
        .execute_sql_in_session(
            &tenant_ctx,
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 5",
        )
        .expect("EXPLAIN should succeed");
    let sql_lines = explain_result_lines(sql_outcome);

    // (b) engine クレート外から `plan_query_with_mode` → `classify_ann_plan`／
    // `classify_scalar_plan` → `ExplainEngine::new` → `build_explain_result`
    // の経路を単一情報源として組み立てる（#765 が写像する経路そのもの）。
    let planned = core
        .plan_query_with_mode(
            &tenant_ctx,
            TABLE,
            "find content",
            None,
            session.search_mode(),
        )
        .expect("plan_query_with_mode should succeed");

    let ann_plan = classify_ann_plan(AnnShapeInput {
        hnsw_enabled: false,
        engine_kind_unknown: core.search_engine_kind().is_none(),
        is_hybrid: true,
        is_precision: planned.mode().mode() == SearchMode::Precision,
        filters_empty: true,
        scalar_prefilter: ExecutionPlan::from_evaluation_order(EvaluationOrder::default())
            .scalar_prefilter,
    });
    let scalar_plan = classify_scalar_plan(&ScalarShapeInput {
        scalar_prefilter: ExecutionPlan::from_evaluation_order(EvaluationOrder::default())
            .scalar_prefilter,
        metadata_filters: &[],
        expr_filters: &[],
        or_filters: &[],
    });
    let explain_engine = ExplainEngine::new(core.search_engine_kind(), ann_plan, scalar_plan);
    let external_result = build_explain_result(&planned, &explain_engine);

    assert_eq!(
        external_result.columns[0],
        ColumnMeta::Computed {
            name: "QUERY PLAN".to_string()
        }
    );
    let external_lines: Vec<String> = external_result
        .rows
        .iter()
        .map(|row| match &row.cells[0] {
            Cell::Text(s) => s.clone(),
            other => panic!("expected Cell::Text, got {other:?}"),
        })
        .collect();

    // (c) 行単位で完全一致（#765 の受け入れ条件の先取り）。
    assert_eq!(sql_lines, external_lines);

    // vacuous 防止: search_terms 2 件 + 既存 6 行 + engine/ann_plan/scalar_plan
    // の 3 行（既定エンジンでは hnsw_params 行は出ない）で 9 行のはず。
    assert_eq!(sql_lines.len(), 9);
    assert!(sql_lines.contains(&"engine: parallel_brute_force".to_string()));
    assert!(sql_lines.contains(&"ann_plan: plain_scan_engine".to_string()));
    assert!(sql_lines.contains(&"scalar_plan: plain_scan".to_string()));
}

#[test]
fn explain_engine_accessors_round_trip() {
    let ann_plan = classify_ann_plan(AnnShapeInput {
        hnsw_enabled: false,
        engine_kind_unknown: false,
        is_hybrid: true,
        is_precision: false,
        filters_empty: true,
        scalar_prefilter: true,
    });
    let scalar_plan = classify_scalar_plan(&ScalarShapeInput {
        scalar_prefilter: true,
        metadata_filters: &[],
        expr_filters: &[],
        or_filters: &[],
    });
    let engine = ExplainEngine::new(
        Some(engine::search_engine::SearchEngineKind::ParallelBruteForce),
        ann_plan,
        scalar_plan,
    );

    assert_eq!(
        engine.kind(),
        Some(engine::search_engine::SearchEngineKind::ParallelBruteForce)
    );
    assert_eq!(engine.ann_plan(), ann_plan);
    assert_eq!(engine.scalar_plan(), scalar_plan);
}
