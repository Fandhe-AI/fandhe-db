//! `engine::core::EngineCore::plan_query` の結合テスト（TASK-110、対象ビヘイビア:
//! PLAN-1。ポインタ: `docs/spec/05-tasks.md` TASK-110・
//! `docs/spec/04-behavior/query-planning.md` PLAN-1）。
//!
//! `tests/dictionary.rs`（TASK-109）と同じ流儀（`unique_db_path` / `CleanupGuard`、
//! 実 `Storage` 上にテーブルを構築、`HashingEmbedder` による決定的埋め込み）で、
//! 固定 JSON を返すモック `LlmClient` を注入して `plan_query` の一連の流れ（辞書接頭辞が
//! 渡ること・展開結果が返ること・接頭辞のバイト同一性・planner 未注入時の fail-closed
//! 拒否・テナント境界での辞書分離）を検証する。`OllamaClient` 自体（HTTP/1.1・JSON の
//! 各経路）の単体検証は `crates/engine/src/query_planner.rs` 内の
//! `#[cfg(test)]` に併設済み（本ファイルでは扱わない）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CoreError, EngineCore};
use engine::embedding::{EmbedError, Embedder, HashingEmbedder};
use engine::incremental::IncrementalConfig;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{render_reembedding_text, LlmClient, PlanError, QueryExpansion};
use engine::storage::Storage;
use std::sync::{Arc, Mutex};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 32;

/// 固定 JSON を返すモック `LlmClient`（実 Ollama 非依存）。呼び出しごとに受け取った
/// プロンプトを記録し、テストから検証できるようにする。
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

/// 常にエラーを返すモック `LlmClient`（LLM 不達・不正応答経路の検証用）。
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
    new_core_with_documents_table_and_embedder(
        path,
        planner,
        Box::new(HashingEmbedder::new(DIM).expect("valid dim")),
    )
}

/// [`new_core_with_documents_table`] の embedder 差し替え版（TASK-114・PLAN-10 の
/// 再埋め込み検証用。スパイ `Embedder`・次元不一致検証など embedder 自体を
/// 差し替えたいテストから使う）。
fn new_core_with_documents_table_and_embedder(
    path: &std::path::Path,
    planner: Box<dyn LlmClient>,
    embedder: Box<dyn Embedder>,
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
        .with_embedder(embedder)
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

/// ファイル形 `INSERT` はチャンク行を `Visibility::Private` で書き込む
/// （`tests/dictionary.rs` と同じ理由で `Public`/`Private` 両方を許可する）。
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

const FIXED_EXPANSION_JSON: &str =
    r#"{"search_terms": ["batch", "cache"], "path_hint": "src/", "kind_hint": "fn"}"#;

// --- 基本フロー: 辞書接頭辞が渡り、展開結果が返ること -----------------------------

#[test]
fn plan_query_returns_expansion_from_mock_llm() {
    let path = unique_db_path("plan-basic");
    let _guard = CleanupGuard(path.clone());
    let seen_prompts = Arc::new(Mutex::new(Vec::new()));
    let core = new_core_with_documents_table(
        &path,
        Box::new(MockLlmClient {
            response: FIXED_EXPANSION_JSON.to_string(),
            seen_prompts: Arc::clone(&seen_prompts),
        }),
    );
    let ctx = tenant_ctx("tenant-a");

    let body = "//! module doc about caching\npub fn run_batch() {}\nstruct Wrapper {}\n";
    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", body, "op-1"),
    )
    .expect("file insert should succeed");

    let expansion = core
        .plan_query(&ctx, "documents", "how does batching work?")
        .expect("plan_query should succeed");

    // `QueryExpansion` は TASK-164（PLAN-11）で `#[non_exhaustive]` を付与したため、
    // クレート外からは `default()` ＋フィールド代入でのみ構築できる。
    let mut expected = QueryExpansion::default();
    expected.search_terms = vec!["batch".to_string(), "cache".to_string()];
    expected.path_hint = Some("src/".to_string());
    expected.kind_hint = Some("fn".to_string());
    assert_eq!(expansion, expected);

    // 辞書接頭辞（シンボル・ファイルツリー）と質問文の両方が実際に LLM へ渡っていること。
    let prompts = seen_prompts.lock().expect("mock lock poisoned");
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].contains("run_batch"));
    assert!(prompts[0].contains("src/x.rs"));
    assert!(prompts[0].contains("how does batching work?"));
    // TASK-164（PLAN-11）: 出力スキーマ指示に mode 推定フィールドが含まれること。
    assert!(prompts[0].contains("\"mode\""));
}

// --- 固定接頭辞の使い回し: 同一辞書世代での連続呼び出しはバイト同一 -----------------

#[test]
fn plan_query_prefix_is_byte_identical_across_calls_for_same_generation() {
    let path = unique_db_path("plan-prefix-stable");
    let _guard = CleanupGuard(path.clone());
    let seen_prompts = Arc::new(Mutex::new(Vec::new()));
    let core = new_core_with_documents_table(
        &path,
        Box::new(MockLlmClient {
            response: FIXED_EXPANSION_JSON.to_string(),
            seen_prompts: Arc::clone(&seen_prompts),
        }),
    );
    let ctx = tenant_ctx("tenant-a");

    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", "pub fn hello() {}\n", "op-1"),
    )
    .expect("file insert should succeed");

    core.plan_query(&ctx, "documents", "question one")
        .expect("first plan_query should succeed");
    core.plan_query(&ctx, "documents", "question two")
        .expect("second plan_query should succeed");

    let prompts = seen_prompts.lock().expect("mock lock poisoned");
    assert_eq!(prompts.len(), 2);
    let prefix_a = prompts[0].split("# Question\n").next().unwrap();
    let prefix_b = prompts[1].split("# Question\n").next().unwrap();
    assert_eq!(
        prefix_a, prefix_b,
        "dictionary prefix must be byte-identical across calls within the same generation"
    );
}

// --- fail-closed: planner 未注入は拒否 ---------------------------------------------

#[test]
fn plan_query_rejects_when_no_planner_configured() {
    let path = unique_db_path("plan-no-planner");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
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
    // `with_query_planner` を呼ばない EngineCore（未注入）。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")));
    let ctx = tenant_ctx("tenant-a");

    let err = core
        .plan_query(&ctx, "documents", "anything")
        .expect_err("plan_query without a configured planner must fail-closed");
    assert!(matches!(err, CoreError::QueryPlannerUnavailable));
}

// --- fail-closed: LLM 不達・不正応答はエラーを伝播する -----------------------------

#[test]
fn plan_query_propagates_llm_unavailable_error() {
    let path = unique_db_path("plan-llm-unavailable");
    let _guard = CleanupGuard(path.clone());
    let core =
        new_core_with_documents_table(&path, Box::new(FailingLlmClient(PlanError::Unavailable)));
    let ctx = tenant_ctx("tenant-a");

    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", "pub fn hello() {}\n", "op-1"),
    )
    .expect("file insert should succeed");

    let err = core
        .plan_query(&ctx, "documents", "anything")
        .expect_err("plan_query must propagate llm client errors");
    assert!(matches!(
        err,
        CoreError::QueryPlanning(PlanError::Unavailable)
    ));
}

#[test]
fn plan_query_rejects_invalid_llm_json_response() {
    let path = unique_db_path("plan-invalid-json");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(
        &path,
        Box::new(MockLlmClient {
            response: "not a json object at all".to_string(),
            seen_prompts: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let ctx = tenant_ctx("tenant-a");

    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", "pub fn hello() {}\n", "op-1"),
    )
    .expect("file insert should succeed");

    let err = core
        .plan_query(&ctx, "documents", "anything")
        .expect_err("plan_query must reject an invalid llm response");
    assert!(matches!(
        err,
        CoreError::QueryPlanning(PlanError::InvalidResponse)
    ));
}

// --- テナント境界: 別テナントは辞書接頭辞（プロンプト内容）が分離される ------------

#[test]
fn plan_query_prefix_is_isolated_per_tenant() {
    let path = unique_db_path("plan-tenant-isolation");
    let _guard = CleanupGuard(path.clone());
    let seen_prompts = Arc::new(Mutex::new(Vec::new()));
    let core = new_core_with_documents_table(
        &path,
        Box::new(MockLlmClient {
            response: FIXED_EXPANSION_JSON.to_string(),
            seen_prompts: Arc::clone(&seen_prompts),
        }),
    );
    let ctx_a = tenant_ctx("tenant-a");
    let ctx_b = tenant_ctx("tenant-b");

    core.execute_insert_sql(
        &ctx_a,
        &insert_file_sql(
            "documents",
            "src/only_in_a.rs",
            "pub fn only_in_tenant_a() {}\n",
            "op-a",
        ),
    )
    .expect("tenant-a file insert should succeed");
    core.execute_insert_sql(
        &ctx_b,
        &insert_file_sql(
            "documents",
            "src/only_in_b.rs",
            "pub fn only_in_tenant_b() {}\n",
            "op-b",
        ),
    )
    .expect("tenant-b file insert should succeed");

    core.plan_query(&ctx_a, "documents", "q")
        .expect("plan_query for tenant-a should succeed");
    core.plan_query(&ctx_b, "documents", "q")
        .expect("plan_query for tenant-b should succeed");

    let prompts = seen_prompts.lock().expect("mock lock poisoned");
    assert_eq!(prompts.len(), 2);
    let prompt_a = &prompts[0];
    let prompt_b = &prompts[1];

    assert!(prompt_a.contains("only_in_tenant_a"));
    assert!(!prompt_a.contains("only_in_tenant_b"));
    assert!(prompt_b.contains("only_in_tenant_b"));
    assert!(!prompt_b.contains("only_in_tenant_a"));
}

// =====================================================================================
// TASK-114・PLAN-10: 再埋め込み規則（`EngineCore::plan_and_embed_query`）
// =====================================================================================

/// 埋め込み呼び出しを記録しつつ、実ベクトルは決定的な [`HashingEmbedder`] へ委譲する
/// スパイ `Embedder`。`embed_batch` へ渡されたテキストを記録し、TASK-114・PLAN-10 の
/// 回帰テストから参照する。
struct SpyEmbedder {
    inner: HashingEmbedder,
    seen_texts: Arc<Mutex<Vec<String>>>,
}

impl Embedder for SpyEmbedder {
    fn dim(&self) -> u32 {
        self.inner.dim()
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        for t in texts {
            self.seen_texts
                .lock()
                .expect("spy lock poisoned")
                .push((*t).to_string());
        }
        self.inner.embed_batch(texts)
    }
}

#[test]
fn plan_and_embed_query_reembeds_expanded_text_with_search_query_prefix() {
    let path = unique_db_path("plan-embed-basic");
    let _guard = CleanupGuard(path.clone());
    let seen_texts = Arc::new(Mutex::new(Vec::new()));
    let core = new_core_with_documents_table_and_embedder(
        &path,
        Box::new(MockLlmClient {
            response: FIXED_EXPANSION_JSON.to_string(),
            seen_prompts: Arc::new(Mutex::new(Vec::new())),
        }),
        Box::new(SpyEmbedder {
            inner: HashingEmbedder::new(DIM).expect("valid dim"),
            seen_texts: Arc::clone(&seen_texts),
        }),
    );
    let ctx = tenant_ctx("tenant-a");

    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", "pub fn run_batch() {}\n", "op-1"),
    )
    .expect("file insert should succeed");

    let question = "how does batching work?";
    let planned = core
        .plan_and_embed_query(&ctx, "documents", question)
        .expect("plan_and_embed_query should succeed");

    // LLM 展開結果（TASK-110）が引き続き返ること。
    assert_eq!(planned.expansion.search_terms, vec!["batch", "cache"]);

    // 再埋め込みへ渡された最後のテキストが `render_reembedding_text` の出力と
    // バイト一致し、`search_query: ` で始まり、モック LLM が返した展開語を含むこと。
    let seen = seen_texts.lock().expect("spy lock poisoned");
    let last = seen.last().expect("embed_batch should have been called");
    assert_eq!(*last, render_reembedding_text(question, &planned.expansion));
    assert!(last.starts_with("search_query: "));
    assert!(last.contains("batch"));
    assert!(last.contains("cache"));
}

#[test]
fn plan_and_embed_query_does_not_reuse_raw_question_embedding() {
    let path = unique_db_path("plan-embed-no-reuse");
    let _guard = CleanupGuard(path.clone());
    let core = new_core_with_documents_table(
        &path,
        Box::new(MockLlmClient {
            response: FIXED_EXPANSION_JSON.to_string(),
            seen_prompts: Arc::new(Mutex::new(Vec::new())),
        }),
    );
    let ctx = tenant_ctx("tenant-a");

    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", "pub fn run_batch() {}\n", "op-1"),
    )
    .expect("file insert should succeed");

    let question = "how does batching work?";
    let planned = core
        .plan_and_embed_query(&ctx, "documents", question)
        .expect("plan_and_embed_query should succeed");

    let reference = HashingEmbedder::new(DIM).expect("valid dim");
    let raw_question_vec = reference
        .embed_batch(&[question])
        .expect("embed_batch should succeed")
        .pop()
        .expect("one vector");
    let expected_vec = reference
        .embed_batch(&[render_reembedding_text(question, &planned.expansion).as_str()])
        .expect("embed_batch should succeed")
        .pop()
        .expect("one vector");

    assert_ne!(
        planned.embedding, raw_question_vec,
        "plan_and_embed_query must not reuse the raw question's embedding"
    );
    assert_eq!(
        planned.embedding, expected_vec,
        "plan_and_embed_query must embed the search_query:-prefixed expanded text"
    );
}

#[test]
fn plan_and_embed_query_rejects_when_no_embedder_configured() {
    let path = unique_db_path("plan-embed-no-embedder");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
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
    let seen_prompts = Arc::new(Mutex::new(Vec::new()));
    // `with_embedder` を呼ばない EngineCore（未注入）。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(MockLlmClient {
            response: FIXED_EXPANSION_JSON.to_string(),
            seen_prompts: Arc::clone(&seen_prompts),
        }),
    );
    let ctx = tenant_ctx("tenant-a");

    let err = core
        .plan_and_embed_query(&ctx, "documents", "anything")
        .expect_err("plan_and_embed_query without a configured embedder must fail-closed");
    assert!(matches!(err, CoreError::EmbedderUnavailable));
    // embedder 未構成は LLM 呼び出しより前に検出され、LLM は一度も呼ばれない
    // （`incremental.rs::chunk_phase` と同じ「高コスト I/O の前に構成不備を検出する」流儀）。
    assert!(seen_prompts.lock().expect("mock lock poisoned").is_empty());
}

#[test]
fn plan_and_embed_query_rejects_when_no_planner_configured() {
    let path = unique_db_path("plan-embed-no-planner");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
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
    // `with_query_planner` を呼ばない EngineCore（未注入）。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(HashingEmbedder::new(DIM).expect("valid dim")));
    let ctx = tenant_ctx("tenant-a");

    let err = core
        .plan_and_embed_query(&ctx, "documents", "anything")
        .expect_err("plan_and_embed_query without a configured planner must fail-closed");
    assert!(matches!(err, CoreError::QueryPlannerUnavailable));
}

#[test]
fn plan_and_embed_query_rejects_embedder_table_dim_mismatch() {
    let path = unique_db_path("plan-embed-dim-mismatch");
    let _guard = CleanupGuard(path.clone());
    // テーブル宣言次元 (DIM=32) と embedder 次元 (16) を意図的にずらす。
    let mismatched_dim: u32 = 16;
    let core = new_core_with_documents_table_and_embedder(
        &path,
        Box::new(MockLlmClient {
            response: FIXED_EXPANSION_JSON.to_string(),
            seen_prompts: Arc::new(Mutex::new(Vec::new())),
        }),
        Box::new(HashingEmbedder::new(mismatched_dim).expect("valid dim")),
    );
    let ctx = tenant_ctx("tenant-a");

    let err = core
        .plan_and_embed_query(&ctx, "documents", "anything")
        .expect_err("plan_and_embed_query must reject an embedder/table dim mismatch");
    assert!(matches!(
        err,
        CoreError::QueryEmbedding(EmbedError::DimMismatch {
            expected: DIM,
            got: 16,
        })
    ));
}

#[test]
fn plan_and_embed_query_does_not_embed_when_llm_expansion_fails() {
    let path = unique_db_path("plan-embed-llm-fails");
    let _guard = CleanupGuard(path.clone());
    let seen_texts = Arc::new(Mutex::new(Vec::new()));
    let core = new_core_with_documents_table_and_embedder(
        &path,
        Box::new(FailingLlmClient(PlanError::Unavailable)),
        Box::new(SpyEmbedder {
            inner: HashingEmbedder::new(DIM).expect("valid dim"),
            seen_texts: Arc::clone(&seen_texts),
        }),
    );
    let ctx = tenant_ctx("tenant-a");

    core.execute_insert_sql(
        &ctx,
        &insert_file_sql("documents", "src/x.rs", "pub fn hello() {}\n", "op-1"),
    )
    .expect("file insert should succeed");
    let calls_after_insert = seen_texts.lock().expect("spy lock poisoned").len();

    let err = core
        .plan_and_embed_query(&ctx, "documents", "anything")
        .expect_err("plan_and_embed_query must propagate llm expansion failures");
    assert!(matches!(
        err,
        CoreError::QueryPlanning(PlanError::Unavailable)
    ));

    // LLM 展開失敗時は再埋め込み（`embed_batch` の追加呼び出し）を一切行わない。
    let calls_after_plan = seen_texts.lock().expect("spy lock poisoned").len();
    assert_eq!(
        calls_after_insert, calls_after_plan,
        "embedder must not be called again after llm expansion fails"
    );
}
