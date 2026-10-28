//! `engine::core::EngineCore::plan_query_with_mode` の結合テスト（TASK-164、対象
//! ビヘイビア: PLAN-11。ポインタ: `docs/spec/05-tasks.md` TASK-164・
//! `docs/spec/04-behavior/query-planning.md` PLAN-11）。
//!
//! `tests/query_planner.rs`（TASK-110）と同じ流儀（`unique_db_path` / `CleanupGuard`、
//! 実 `Storage` 上にテーブルを構築、固定 JSON を返すモック `LlmClient` を注入）で、
//! クエリ句・セッション変数・プランナー推定の解決結果（`sql/mode.rs::resolve_mode_with_planner`
//! の解決契約は spec のビヘイビア定義〔PLAN-11〕を参照）を検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CoreError, EngineCore};
use engine::embedding::HashingEmbedder;
use engine::incremental::IncrementalConfig;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::sql::mode::{ModeSource, SearchMode};
use engine::storage::Storage;
use std::sync::{Arc, Mutex};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 32;

/// 固定 JSON を返すモック `LlmClient`（`tests/query_planner.rs::MockLlmClient` と同型。
/// 各テストファイルは別クレートとしてコンパイルされるため、共有せず複製する）。
struct MockLlmClient {
    response: String,
    seen_prompts: Arc<Mutex<Vec<String>>>,
}

impl LlmClient for MockLlmClient {
    fn complete(&self, prompt: &str) -> Result<String, PlanError> {
        self.seen_prompts
            .lock()
            .expect("mock lock poisoned")
            .push(prompt.to_string());
        Ok(self.response.clone())
    }
}

/// 常にエラーを返すモック `LlmClient`（LLM 完了自体の失敗経路の検証用）。
struct FailingLlmClient(PlanError);

impl LlmClient for FailingLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Err(self.0.clone())
    }
}

fn new_core_with_documents_table(
    path: &std::path::Path,
    planner: Box<dyn LlmClient>,
) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")))
        .with_incremental_config(small_chunk_config())
        .with_query_planner(planner)
}

fn small_chunk_config() -> IncrementalConfig {
    IncrementalConfig {
        chunking: engine::chunking::ChunkingConfig {
            lines_per_chunk: 4,
            max_markdown_section_chars: None,
        },
        ..IncrementalConfig::default()
    }
}

fn sql_escape(s: &str) -> String {
    s.replace('\'', "''")
}

fn insert_file_sql(table: &str, path: &str, body: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO {table} (path, body) VALUES ('{}', '{}') USING OPERATION_ID '{op_id}'",
        sql_escape(path),
        sql_escape(body)
    )
}

fn tenant_ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(
        tenant,
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant")
}

/// テーブルへ最低 1 件の投入をしておく（`dictionary_snapshot` が空でも `plan_query`
/// 系は動作するが、`tests/query_planner.rs` と条件を揃えて検証する）。
fn seed_document(core: &EngineCore, ctx: &PolicyContext) {
    let body = "//! module doc about batching\npub fn run_batch() {}\n";
    core.execute_insert_sql(ctx, &insert_file_sql("documents", "src/x.rs", body, "op-1"))
        .expect("file insert should succeed");
}

fn mock_core(path: &std::path::Path, response_json: &str) -> (EngineCore, Arc<Mutex<Vec<String>>>) {
    let seen_prompts = Arc::new(Mutex::new(Vec::new()));
    let core = new_core_with_documents_table(
        path,
        Box::new(MockLlmClient {
            response: response_json.to_string(),
            seen_prompts: Arc::clone(&seen_prompts),
        }),
    );
    (core, seen_prompts)
}

// --- 明示指定なし + プランナー推定 precision: プランナー推定が採用される ----------

#[test]
fn plan_query_with_mode_no_explicit_uses_planner_estimate() {
    let path = unique_db_path("mode-planner-only");
    let _guard = CleanupGuard(path.clone());
    let (core, _prompts) = mock_core(
        &path,
        r#"{"search_terms": ["batch"], "path_hint": null, "kind_hint": null, "mode": "precision"}"#,
    );
    let ctx = tenant_ctx("tenant-a");
    seed_document(&core, &ctx);

    let planned = core
        .plan_query_with_mode(&ctx, "documents", "how does batching work?", None, None)
        .expect("plan_query_with_mode should succeed");

    assert_eq!(planned.mode().mode(), SearchMode::Precision);
    assert_eq!(planned.mode().source(), ModeSource::PlannerEstimate);
    assert_eq!(planned.expansion().mode_hint, Some(SearchMode::Precision));
}

// --- クエリ句 recall + プランナー推定 precision: クエリ句が優先される -------------

#[test]
fn plan_query_with_mode_query_clause_overrides_planner_estimate() {
    let path = unique_db_path("mode-query-wins");
    let _guard = CleanupGuard(path.clone());
    let (core, _prompts) = mock_core(
        &path,
        r#"{"search_terms": ["batch"], "path_hint": null, "kind_hint": null, "mode": "precision"}"#,
    );
    let ctx = tenant_ctx("tenant-a");
    seed_document(&core, &ctx);

    let planned = core
        .plan_query_with_mode(
            &ctx,
            "documents",
            "how does batching work?",
            Some(SearchMode::Recall),
            None,
        )
        .expect("plan_query_with_mode should succeed");

    assert_eq!(planned.mode().mode(), SearchMode::Recall);
    assert_eq!(planned.mode().source(), ModeSource::QueryClause);
}

// --- セッション変数 recall + プランナー推定 precision: セッション変数が優先される --

#[test]
fn plan_query_with_mode_session_variable_overrides_planner_estimate() {
    let path = unique_db_path("mode-session-wins");
    let _guard = CleanupGuard(path.clone());
    let (core, _prompts) = mock_core(
        &path,
        r#"{"search_terms": ["batch"], "path_hint": null, "kind_hint": null, "mode": "precision"}"#,
    );
    let ctx = tenant_ctx("tenant-a");
    seed_document(&core, &ctx);

    let planned = core
        .plan_query_with_mode(
            &ctx,
            "documents",
            "how does batching work?",
            None,
            Some(SearchMode::Recall),
        )
        .expect("plan_query_with_mode should succeed");

    assert_eq!(planned.mode().mode(), SearchMode::Recall);
    assert_eq!(planned.mode().source(), ModeSource::SessionVariable);
}

// --- クエリ句とセッション変数が両方ある場合: クエリ句が勝つ -----------------------

#[test]
fn plan_query_with_mode_query_clause_wins_over_session_variable() {
    let path = unique_db_path("mode-query-over-session");
    let _guard = CleanupGuard(path.clone());
    let (core, _prompts) = mock_core(
        &path,
        r#"{"search_terms": ["batch"], "path_hint": null, "kind_hint": null, "mode": "precision"}"#,
    );
    let ctx = tenant_ctx("tenant-a");
    seed_document(&core, &ctx);

    let planned = core
        .plan_query_with_mode(
            &ctx,
            "documents",
            "how does batching work?",
            Some(SearchMode::Precision),
            Some(SearchMode::Recall),
        )
        .expect("plan_query_with_mode should succeed");

    assert_eq!(planned.mode().mode(), SearchMode::Precision);
    assert_eq!(planned.mode().source(), ModeSource::QueryClause);
}

// --- プランナー応答の mode が null / 欠落 / 未知値: recall へ fail-safe -----------

#[test]
fn plan_query_with_mode_null_mode_falls_back_to_default_recall() {
    let path = unique_db_path("mode-null");
    let _guard = CleanupGuard(path.clone());
    let (core, _prompts) = mock_core(
        &path,
        r#"{"search_terms": ["batch"], "path_hint": null, "kind_hint": null, "mode": null}"#,
    );
    let ctx = tenant_ctx("tenant-a");
    seed_document(&core, &ctx);

    let planned = core
        .plan_query_with_mode(&ctx, "documents", "how does batching work?", None, None)
        .expect("plan_query_with_mode should succeed (mode is fail-safe, not fatal)");

    assert_eq!(planned.mode().mode(), SearchMode::Recall);
    assert_eq!(planned.mode().source(), ModeSource::Default);
    assert_eq!(planned.expansion().mode_hint, None);
}

#[test]
fn plan_query_with_mode_missing_mode_key_falls_back_to_default_recall() {
    let path = unique_db_path("mode-missing");
    let _guard = CleanupGuard(path.clone());
    // `mode` キー自体が存在しない応答（既存の PLAN-1 期モックと同型の JSON）。
    let (core, _prompts) = mock_core(
        &path,
        r#"{"search_terms": ["batch"], "path_hint": null, "kind_hint": null}"#,
    );
    let ctx = tenant_ctx("tenant-a");
    seed_document(&core, &ctx);

    let planned = core
        .plan_query_with_mode(&ctx, "documents", "how does batching work?", None, None)
        .expect("plan_query_with_mode should succeed when mode key is absent");

    assert_eq!(planned.mode().mode(), SearchMode::Recall);
    assert_eq!(planned.mode().source(), ModeSource::Default);
}

#[test]
fn plan_query_with_mode_unknown_mode_value_falls_back_to_default_recall() {
    let path = unique_db_path("mode-unknown");
    let _guard = CleanupGuard(path.clone());
    let (core, _prompts) = mock_core(
        &path,
        r#"{"search_terms": ["batch"], "path_hint": null, "kind_hint": null, "mode": "fuzzy"}"#,
    );
    let ctx = tenant_ctx("tenant-a");
    seed_document(&core, &ctx);

    let planned = core
        .plan_query_with_mode(&ctx, "documents", "how does batching work?", None, None)
        .expect("plan_query_with_mode should succeed on an unknown mode value");

    assert_eq!(planned.mode().mode(), SearchMode::Recall);
    assert_eq!(planned.mode().source(), ModeSource::Default);
}

// --- LLM 完了自体の失敗: モード以前の失敗として `Err`（既存 PLAN-1 契約どおり） -----

#[test]
fn plan_query_with_mode_propagates_llm_completion_failure() {
    let path = unique_db_path("mode-llm-failure");
    let _guard = CleanupGuard(path.clone());
    let core =
        new_core_with_documents_table(&path, Box::new(FailingLlmClient(PlanError::Unavailable)));
    let ctx = tenant_ctx("tenant-a");
    seed_document(&core, &ctx);

    let result =
        core.plan_query_with_mode(&ctx, "documents", "how does batching work?", None, None);
    assert!(matches!(
        result,
        Err(CoreError::QueryPlanning(PlanError::Unavailable))
    ));
}

// --- 既存 `plan_query` の挙動が不変であること（後方互換） -------------------------

#[test]
fn plan_query_backward_compatible_ignores_mode_field() {
    let path = unique_db_path("mode-backward-compat");
    let _guard = CleanupGuard(path.clone());
    let (core, prompts) = mock_core(
        &path,
        r#"{"search_terms": ["batch"], "path_hint": "src/", "kind_hint": "fn", "mode": "precision"}"#,
    );
    let ctx = tenant_ctx("tenant-a");
    seed_document(&core, &ctx);

    let expansion = core
        .plan_query(&ctx, "documents", "how does batching work?")
        .expect("plan_query should succeed");

    // `mode` は展開結果のフィールドとして保持されるが、`plan_query`（2 系）自体は
    // 優先順位解決を行わず `QueryExpansion` を素通しする（TASK-164 で追加した経路は
    // `plan_query_with_mode` のみ）。
    assert_eq!(expansion.search_terms, vec!["batch".to_string()]);
    assert_eq!(expansion.path_hint, Some("src/".to_string()));
    assert_eq!(expansion.kind_hint, Some("fn".to_string()));
    assert_eq!(expansion.mode_hint, Some(SearchMode::Precision));

    let seen = prompts.lock().expect("mock lock poisoned");
    assert_eq!(seen.len(), 1);
}
