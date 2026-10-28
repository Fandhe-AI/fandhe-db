//! `engine::core::EngineCore::with_tiered_query_planner` / `plan_query_with_classification`
//! の結合テスト（TASK-115、対象ビヘイビア: PLAN-8。ポインタ: `docs/spec/05-tasks.md`
//! TASK-115）。
//!
//! `tests/query_planner.rs`（TASK-110）と同じ流儀（`unique_db_path` / `CleanupGuard`、
//! 実 `Storage` 上にテーブルを構築、`HashingEmbedder` による決定的埋め込み）で、呼び出し
//! 記録付きスタブ `LlmClient` を対話ティア／高精度ティアの 2 つ注入し、質問類型ごとに
//! 期待するティアのクライアントへルーティングされること・`Single` 構成（TASK-110 の既存
//! 挙動）が不変であることを検証する。`crate::tiering::classify` 自体の単体検証は
//! `crates/engine/src/tiering.rs` 内の `#[cfg(test)]` に併設済み（本ファイルでは扱わない）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CoreError, EngineCore};
use engine::embedding::HashingEmbedder;
use engine::incremental::IncrementalConfig;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::storage::Storage;
use engine::tiering::{Tier, TieringCriteria};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 32;

/// 固定 JSON を返し呼び出し回数を記録するスタブ `LlmClient`（`tests/query_planner.rs::
/// MockLlmClient` と同じ役割だが、呼び出し先クライアントの識別に呼び出し回数のみを使う）。
struct CountingLlmClient {
    calls: Arc<AtomicUsize>,
}

impl LlmClient for CountingLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(r#"{"search_terms": [], "path_hint": null, "kind_hint": null}"#.to_string())
    }
}

fn new_core_with_documents_table(path: &std::path::Path) -> EngineCore {
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

// --- ティア別ルーティング ---------------------------------------------------------

#[test]
fn abstraction_question_routes_to_high_precision_client() {
    let path = unique_db_path("tiering-abstraction");
    let _guard = CleanupGuard(path.clone());
    let dialogue_calls = Arc::new(AtomicUsize::new(0));
    let high_precision_calls = Arc::new(AtomicUsize::new(0));

    let core = new_core_with_documents_table(&path).with_tiered_query_planner(
        Box::new(CountingLlmClient {
            calls: dialogue_calls.clone(),
        }),
        Box::new(CountingLlmClient {
            calls: high_precision_calls.clone(),
        }),
        TieringCriteria::default(),
    );

    let ctx = tenant_ctx("tenant-a");
    let (_, classification) = core
        .plan_query_with_classification(&ctx, "documents", "explain the overall architecture")
        .expect("plan_query_with_classification should succeed");

    assert_eq!(dialogue_calls.load(Ordering::SeqCst), 0);
    assert_eq!(high_precision_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        classification
            .expect("tiered binding must return a classification")
            .tier,
        Tier::HighPrecision
    );
}

#[test]
fn intent_question_without_dictionary_hits_routes_to_high_precision_client() {
    let path = unique_db_path("tiering-intent");
    let _guard = CleanupGuard(path.clone());
    let dialogue_calls = Arc::new(AtomicUsize::new(0));
    let high_precision_calls = Arc::new(AtomicUsize::new(0));

    let core = new_core_with_documents_table(&path).with_tiered_query_planner(
        Box::new(CountingLlmClient {
            calls: dialogue_calls.clone(),
        }),
        Box::new(CountingLlmClient {
            calls: high_precision_calls.clone(),
        }),
        TieringCriteria::default(),
    );

    let ctx = tenant_ctx("tenant-a");
    let (_, classification) = core
        .plan_query_with_classification(&ctx, "documents", "something totally unrelated here")
        .expect("plan_query_with_classification should succeed");

    assert_eq!(dialogue_calls.load(Ordering::SeqCst), 0);
    assert_eq!(high_precision_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        classification
            .expect("tiered binding must return a classification")
            .tier,
        Tier::HighPrecision
    );
}

#[test]
fn path_like_question_routes_to_dialogue_client() {
    let path = unique_db_path("tiering-path");
    let _guard = CleanupGuard(path.clone());
    let dialogue_calls = Arc::new(AtomicUsize::new(0));
    let high_precision_calls = Arc::new(AtomicUsize::new(0));

    let core = new_core_with_documents_table(&path).with_tiered_query_planner(
        Box::new(CountingLlmClient {
            calls: dialogue_calls.clone(),
        }),
        Box::new(CountingLlmClient {
            calls: high_precision_calls.clone(),
        }),
        TieringCriteria::default(),
    );

    let ctx = tenant_ctx("tenant-a");
    let (_, classification) = core
        .plan_query_with_classification(&ctx, "documents", "open core.rs please")
        .expect("plan_query_with_classification should succeed");

    assert_eq!(dialogue_calls.load(Ordering::SeqCst), 1);
    assert_eq!(high_precision_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        classification
            .expect("tiered binding must return a classification")
            .tier,
        Tier::Dialogue
    );
}

// --- Single 構成の既存挙動不変 -----------------------------------------------------

#[test]
fn single_binding_returns_no_classification_and_still_calls_the_one_client() {
    let path = unique_db_path("tiering-single");
    let _guard = CleanupGuard(path.clone());
    let calls = Arc::new(AtomicUsize::new(0));

    let core =
        new_core_with_documents_table(&path).with_query_planner(Box::new(CountingLlmClient {
            calls: calls.clone(),
        }));

    let ctx = tenant_ctx("tenant-a");
    let (_, classification) = core
        .plan_query_with_classification(&ctx, "documents", "anything at all")
        .expect("plan_query_with_classification should succeed for a single binding");

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(classification.is_none());
}

// --- 未注入時の fail-closed 拒否は不変 ---------------------------------------------

#[test]
fn plan_query_with_classification_rejects_when_no_planner_configured() {
    let path = unique_db_path("tiering-unset");
    let _guard = CleanupGuard(path.clone());

    let core = new_core_with_documents_table(&path);
    let ctx = tenant_ctx("tenant-a");
    let err = core
        .plan_query_with_classification(&ctx, "documents", "anything")
        .expect_err("plan_query_with_classification without a configured planner must fail-closed");
    assert!(matches!(err, CoreError::QueryPlannerUnavailable));
}
