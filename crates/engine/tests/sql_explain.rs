//! `EXPLAIN SELECT ... USING PLAN('<query>')` の受け入れテスト（TASK-78、対象
//! ビヘイビア: SQL-6。ポインタ: `docs/spec/05-tasks.md` TASK-78・
//! `docs/spec/04-behavior/sql-surface.md` SQL-6）。
//!
//! `tests/sql_using_plan.rs`（TASK-77・SQL-5）が固定する「実行時ディスパッチ」に
//! 対し、本ファイルは `EXPLAIN` が検索本体を一切実行せず、LLM クエリ展開・モード
//! 解決の結果（検索語・ソフトヒント・実効モードと指定元）のみを `QUERY PLAN` 単一
//! 列の応答として返すことを固定する。`USING PLAN` を伴わない構文・拒否系
//! （プランナー未注入・スキーマ不備・LIMIT/モードリテラル不正）が LLM 呼び出し
//! 前に完結することも確認する（`sql_using_plan.rs` と同じ判別テストの流儀）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";
const DIM: u32 = 4;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn open_storage_with_table(path: &std::path::Path) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    storage
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(storage: &Storage, id: u64, embedding: Vec<f32>, path: &str, body: &str) {
    let op_id = OperationId::parse(&format!("explain-test-op-{id}")).expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx("tenant-a"),
        id,
        Visibility::Public,
        &[
            Value::Vector(embedding),
            Value::Text(path.to_string()),
            Value::Text(body.to_string()),
        ],
        &op_id,
    )
    .expect("insert row");
}

fn seeded_storage(path: &std::path::Path) -> Storage {
    let storage = open_storage_with_table(path);
    insert_row(
        &storage,
        1,
        vec![0.1, 0.2, 0.3, 0.4],
        "docs/a.md",
        "alpha content in english",
    );
    storage
}

/// 固定の展開結果を返すスタブ（実 Ollama 疎通は TASK-110 と同じくスコープ外。
/// `sql_using_plan.rs::StubLlmClient` と同構成）。
struct StubLlmClient {
    response: &'static str,
}

impl LlmClient for StubLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(self.response.to_string())
    }
}

/// 呼び出し回数を記録するスタブ（`sql_using_plan.rs::CountingLlmClient` と同構成。
/// LIMIT/モードリテラル不正が LLM 呼び出し前に拒否されることを直接確認するため）。
struct CountingLlmClient {
    response: String,
    calls: std::sync::atomic::AtomicUsize,
}

impl CountingLlmClient {
    fn new(response: impl Into<String>) -> Self {
        Self {
            response: response.into(),
            calls: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl LlmClient for CountingLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(self.response.clone())
    }
}

struct ArcLlmClient(std::sync::Arc<CountingLlmClient>);

impl LlmClient for ArcLlmClient {
    fn complete(&self, prompt: &str) -> Result<String, PlanError> {
        self.0.complete(prompt)
    }
}

const EXPANSION_RESPONSE: &str =
    r#"{"search_terms": ["alpha", "beta"], "path_hint": "docs/", "kind_hint": "fn"}"#;
const EXPANSION_RESPONSE_NO_HINTS: &str =
    r#"{"search_terms": [], "path_hint": null, "kind_hint": null}"#;

fn explain_result_lines(outcome: SqlOutcome) -> Vec<String> {
    match outcome {
        SqlOutcome::Explain(result) => {
            assert_eq!(
                result.columns.len(),
                1,
                "EXPLAIN must return a single column"
            );
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
fn explain_reports_search_terms_and_hints_and_mode() {
    let path = unique_db_path("sql-explain-basic");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }),
    );

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    assert_eq!(
        lines,
        vec![
            "search_terms[0]: alpha".to_string(),
            "search_terms[1]: beta".to_string(),
            "path_hint: docs/".to_string(),
            "kind_hint: fn".to_string(),
            "mode: recall".to_string(),
            "mode_source: default".to_string(),
            // Issue #411: `EngineCore::from_storage`（provider を直接注入・
            // `kind` 不明）経由のため `engine: (custom_provider)`。codex-review
            // P1 指摘対応（PR #437）: 実行方式を判別できないため `ann_plan:` も
            // `unknown_custom_provider`。
            "engine: (custom_provider)".to_string(),
            "ann_plan: unknown_custom_provider".to_string(),
            "scalar_plan: plain_scan".to_string(),
        ]
    );
}

#[test]
fn explain_uses_none_label_for_absent_hints() {
    let path = unique_db_path("sql-explain-no-hints");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE_NO_HINTS,
        }),
    );

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    assert_eq!(
        lines,
        vec![
            "path_hint: (none)".to_string(),
            "kind_hint: (none)".to_string(),
            "mode: recall".to_string(),
            "mode_source: default".to_string(),
            "engine: (custom_provider)".to_string(),
            "ann_plan: unknown_custom_provider".to_string(),
            "scalar_plan: plain_scan".to_string(),
        ]
    );
}

#[test]
fn explain_reports_mode_source_query_clause() {
    let path = unique_db_path("sql-explain-mode-query-clause");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE_NO_HINTS,
        }),
    );

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10 USING MODE 'precision'",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    assert_eq!(lines[lines.len() - 5], "mode: precision");
    assert_eq!(lines[lines.len() - 4], "mode_source: query_clause");
}

#[test]
fn explain_reports_mode_source_session_variable() {
    let path = unique_db_path("sql-explain-mode-session-variable");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE_NO_HINTS,
        }),
    );

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "SET search_mode = 'precision'",
    )
    .expect("SET search_mode should succeed");

    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    assert_eq!(lines[lines.len() - 5], "mode: precision");
    assert_eq!(lines[lines.len() - 4], "mode_source: session_variable");
}

#[test]
fn explain_reports_mode_source_planner_estimate() {
    let path = unique_db_path("sql-explain-mode-planner-estimate");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: r#"{"search_terms": [], "path_hint": null, "kind_hint": null, "mode": "precision"}"#,
        }),
    );

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    assert_eq!(lines[lines.len() - 5], "mode: precision");
    assert_eq!(lines[lines.len() - 4], "mode_source: planner_estimate");
}

#[test]
fn explain_does_not_require_an_embedder() {
    // `EXPLAIN` は検索本体を実行しないため再埋め込み（`Embedder`）を呼ばない。
    // `with_embedder` を一切呼ばずに構築した `EngineCore` でも成功することを
    // 固定する（設計方針: 「embedder 未注入でも EXPLAIN 可能」）。
    let path = unique_db_path("sql-explain-no-embedder-needed");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }),
    );

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
    )
    .expect("EXPLAIN must succeed without a configured embedder");
}

// Issue #922（SQL-27）: `EXPLAIN` の対象を通常検索・集計・広域取得へ拡大した
// ため、`USING PLAN` を伴わない検索 SELECT・集計 SELECT はもはや拒否されず
// 受理される（下記 2 ケースは拒否テストから受理テストへ反転）。`USING PLAN`
// を伴う既存の EXPLAIN 出力（本ファイル冒頭のその他のテスト）は不変。

#[test]
fn explain_accepts_plain_select_without_using_plan() {
    let path = unique_db_path("sql-explain-accepts-plain-select");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10",
        )
        .expect("EXPLAIN over a plain search SELECT (no USING PLAN) must be accepted");
    let lines = explain_result_lines(outcome);
    // `USING PLAN` を伴わない検索 EXPLAIN は `search_terms`／`path_hint`／
    // `kind_hint` を含まない（LLM I/O を行わないため）。
    assert_eq!(lines[0], "mode: recall");
    assert_eq!(lines[1], "mode_source: default");
    assert!(lines.iter().any(|l| l == "scalar_plan: plain_scan"));
    assert!(!lines.iter().any(|l| l.starts_with("search_terms")));
}

#[test]
fn explain_accepts_aggregate_select() {
    let path = unique_db_path("sql-explain-accepts-aggregate");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT COUNT(id) FROM docs",
        )
        .expect("EXPLAIN over an aggregate SELECT must be accepted");
    let lines = explain_result_lines(outcome);
    // `WHERE` なしの単一行集計は `visible_bitmap_cache`（可視ビットマップ
    // キャッシュを使える形）。
    assert_eq!(
        lines,
        vec![
            "scalar_plan: plain_scan",
            "access_path: visible_bitmap_cache"
        ]
    );
}

#[test]
fn explain_rejects_set_search_mode() {
    let path = unique_db_path("sql-explain-rejects-set");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SET search_mode = 'recall'",
        )
        .expect_err("EXPLAIN over SET must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn explain_via_session_less_entry_point_is_rejected() {
    // `EngineCore::execute_sql`（セッションなし後方互換 API）は `EXPLAIN` を
    // 受理しない（`SET`・`CREATE FUNCTION` と同じ「セッション対応エントリ
    // ポイントを使う」契約）。
    let path = unique_db_path("sql-explain-rejects-sessionless-entry");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }),
    );

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect_err("EXPLAIN must require a session-aware entry point");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn explain_fails_closed_without_query_planner() {
    let path = unique_db_path("sql-explain-no-planner");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect_err("missing query planner must be rejected");
    assert_eq!(err.wire_code(), "XX000");
}

#[test]
fn explain_fails_closed_without_body_column() {
    let path = unique_db_path("sql-explain-no-body");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "no_body",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
                ColumnDef::new("path", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }),
    );

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM no_body USING PLAN('find content') LIMIT 10",
        )
        .expect_err("missing body column must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn explain_rejects_invalid_limit_before_invoking_query_planner() {
    let path = unique_db_path("sql-explain-invalid-limit");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 0",
        )
        .expect_err("LIMIT 0 must be rejected");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        planner.call_count(),
        0,
        "invalid LIMIT must be rejected before the high-cost query planner call"
    );
}

#[test]
fn explain_rejects_invalid_using_mode_before_invoking_query_planner() {
    let path = unique_db_path("sql-explain-invalid-mode");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10 USING MODE 'invalid'",
        )
        .expect_err("unknown USING MODE literal must be rejected");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        planner.call_count(),
        0,
        "invalid USING MODE must be rejected before the high-cost query planner call"
    );
}

#[test]
fn explain_rejects_unknown_projection_column_before_invoking_query_planner() {
    // codex-review P1・Cursor Bugbot 指摘（PR #267）の判別テスト:
    // `EXPLAIN` 経路も `Statement::Select` の `USING PLAN` 経路（PR #266・
    // `sql_using_plan.rs::using_plan_rejects_unknown_projection_column_before_invoking_query_planner`）
    // と同じく、投影列の束縛（`sql::parser::bind_projection`、`22000`）を
    // I/O 前の `pre_check_schema` で検証すべき。未知列は構文上は受理されるが
    // 束縛不能で必ず拒否される。
    let path = unique_db_path("sql-explain-unknown-projection");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT no_such_column FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect_err("unknown projected column must be rejected");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        planner.call_count(),
        0,
        "unknown projection column must be rejected before the high-cost query planner call"
    );
}

#[test]
fn explain_rejects_unknown_where_column_before_invoking_query_planner() {
    // 上のテストの対（`WHERE` 述語側）。`sql::parser::bind_where_predicates`
    // の未知列拒否（`22000`）も同じく I/O 前の `pre_check_schema` で検証される
    // べき（PR #267 の是正対応）。
    let path = unique_db_path("sql-explain-unknown-where");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs WHERE no_such_column = 'x' USING PLAN('find content') LIMIT 10",
        )
        .expect_err("unknown WHERE column must be rejected");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        planner.call_count(),
        0,
        "unknown WHERE column must be rejected before the high-cost query planner call"
    );
}

#[test]
fn explain_rejects_unregistered_udf_call_before_invoking_query_planner() {
    // codex-review P1・Cursor Bugbot 指摘（PR #267）の判別テスト:
    // 投影列・`WHERE` 述語内の未登録 UDF 呼び出し（`sql::udf_call`、`22000`）も
    // `EXPLAIN` 経路で I/O 前に拒否されるべき（`pre_check_bindable` が
    // `session.udfs()` を渡して同じ検証を再利用する）。
    let path = unique_db_path("sql-explain-unregistered-udf");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT no_such_udf(id) FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect_err("unregistered UDF call must be rejected");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        planner.call_count(),
        0,
        "unregistered UDF call must be rejected before the high-cost query planner call"
    );
}

#[test]
fn explain_does_not_write_or_execute_search() {
    // `EXPLAIN` はテーブル行データを一切返さない・書き込まないことを、実行前後で
    // 行数（`SELECT COUNT(*)`）が変化しないことにより確認する（副作用なしの回帰）。
    let path = unique_db_path("sql-explain-no-side-effects");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }),
    );

    let mut session = SessionState::default();
    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut session,
        "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
    )
    .expect("EXPLAIN should succeed");

    let count_before = match core
        .execute_sql(&ctx("tenant-a"), "SELECT COUNT(id) FROM docs")
        .expect("count query should succeed")
        .rows
        .into_iter()
        .next()
        .expect("count row")
        .cells
        .into_iter()
        .next()
        .expect("count cell")
    {
        Cell::Integer(n) => n,
        other => panic!("expected Cell::Integer, got {other:?}"),
    };
    assert_eq!(count_before, 1, "EXPLAIN must not write or delete rows");
}

// --- Issue #411: `engine:`／`hnsw_params:`／`ann_plan:` 行の受け入れテスト --------

/// `SearchEngineKind::Hnsw` opt-in で構築した `EngineCore` を返す（`from_storage`
/// 経由の各テストと同様、`seeded_storage` の 1 行を種にする）。
fn seeded_hnsw_core(path: &std::path::Path) -> EngineCore {
    let storage = seeded_storage(path);
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    EngineCore::from_storage_with_engine(storage, kind)
}

#[test]
fn explain_reports_custom_provider_engine_and_unknown_custom_provider_ann_plan() {
    // `EngineCore::from_storage`（provider 直接注入・`kind` 不明）は
    // `search_engine_kind() == None` となり `engine: (custom_provider)` を返す。
    // codex-review P1 指摘対応（PR #437）: 実際に ANN か brute-force かを
    // `EngineCore` 側から判別できないため、`ann_plan:` も
    // `unknown_custom_provider`（`plain_scan_engine` へ丸めない）。
    let path = unique_db_path("sql-explain-engine-custom-provider");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE_NO_HINTS,
        }),
    );

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    assert_eq!(lines[lines.len() - 3], "engine: (custom_provider)");
    assert_eq!(lines[lines.len() - 2], "ann_plan: unknown_custom_provider");
    assert_eq!(lines[lines.len() - 1], "scalar_plan: plain_scan");
}

#[test]
fn explain_reports_hnsw_engine_params_and_full_visible_ann_plan_without_filter() {
    // HNSW opt-in・`WHERE` なし → `ann_plan: hnsw_full_visible`
    // （`sql::hnsw_cache::classify_ann_plan` の `filters_empty` 分岐）。
    let path = unique_db_path("sql-explain-hnsw-full-visible");
    let _guard = CleanupGuard(path.clone());
    let core = seeded_hnsw_core(&path).with_query_planner(Box::new(StubLlmClient {
        response: EXPANSION_RESPONSE_NO_HINTS,
    }));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    assert_eq!(lines[lines.len() - 4], "engine: hnsw");
    assert_eq!(
        lines[lines.len() - 3],
        "hnsw_params: m=16,ef_construction=100,ef_search=64,resident=f32,sparse_visited_max=0"
    );
    assert_eq!(lines[lines.len() - 2], "ann_plan: hnsw_full_visible");
    assert_eq!(lines[lines.len() - 1], "scalar_plan: plain_scan");
}

#[test]
fn explain_reports_hnsw_subset_ann_plan_with_default_scalar_first_where() {
    // HNSW opt-in・等価 `WHERE`・既定 HINT ORDER（SCALAR 先行）
    // → `ann_plan: hnsw_subset`。
    let path = unique_db_path("sql-explain-hnsw-subset");
    let _guard = CleanupGuard(path.clone());
    let core = seeded_hnsw_core(&path).with_query_planner(Box::new(StubLlmClient {
        response: EXPANSION_RESPONSE_NO_HINTS,
    }));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs WHERE path = 'docs/a.md' USING PLAN('find content') LIMIT 10",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    assert_eq!(lines[lines.len() - 2], "ann_plan: hnsw_subset");
    assert_eq!(lines[lines.len() - 1], "scalar_plan: index_equality");
}

#[test]
fn explain_rejects_hint_order_combined_with_using_plan() {
    // `sql::allowlist::parse_select_shape` は `USING PLAN` 経路で `HINT ORDER(...)`
    // を構造上受理しない（ランキング自体を展開結果が決めるため。上記コメント
    // 参照）。`EXPLAIN` も同じ許可リスト検証を通るため、`stmt.evaluation_order()`
    // は `USING PLAN` を伴う限り常に `EvaluationOrder::DEFAULT`（SCALAR 先行）
    // であり、`ann_plan:` の `scalar_prefilter` 分岐で `false`（DISTANCE 先行）
    // 側には到達しない。この判別テストで両者の前提が食い違わないことを固定する。
    let path = unique_db_path("sql-explain-hnsw-hint-order-rejected");
    let _guard = CleanupGuard(path.clone());
    let core = seeded_hnsw_core(&path).with_query_planner(Box::new(StubLlmClient {
        response: EXPANSION_RESPONSE_NO_HINTS,
    }));

    let mut session = SessionState::default();
    let err = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs WHERE path = 'docs/a.md' USING PLAN('find content') \
             LIMIT 10 HINT ORDER(DISTANCE, SCALAR, RLS)",
        )
        .expect_err("HINT ORDER combined with USING PLAN must be rejected");
    assert_eq!(err.wire_code(), "42601");
}

#[test]
fn explain_reports_hnsw_full_visible_ann_plan_with_only_visible_predicate() {
    // `WHERE visible()`（`WherePredicate::PredicateCall`）は `metadata_filters`／
    // `expr_filters` を増やさないため、`stmt.where_predicates()` が非空でも
    // `filters_empty == true` になる（`sql::using_plan::PreCheckShape` ドキュメント
    // 参照）。`stmt.where_predicates().is_empty()` へ代替していないことの固定。
    let path = unique_db_path("sql-explain-hnsw-visible-predicate");
    let _guard = CleanupGuard(path.clone());
    let core = seeded_hnsw_core(&path).with_query_planner(Box::new(StubLlmClient {
        response: EXPANSION_RESPONSE_NO_HINTS,
    }));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs WHERE visible() USING PLAN('find content') LIMIT 10",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    assert_eq!(lines[lines.len() - 2], "ann_plan: hnsw_full_visible");
    assert_eq!(lines[lines.len() - 1], "scalar_plan: plain_scan");
}

#[test]
fn explain_reports_plain_scan_precision_ann_plan_for_precision_mode() {
    // HNSW opt-in でも `precision` モードは常に厳密 brute-force
    // （TASK-162・SEARCH-9。`docs/design/explain-search-engine-exposure.md` 参照）。
    let path = unique_db_path("sql-explain-hnsw-precision");
    let _guard = CleanupGuard(path.clone());
    let core = seeded_hnsw_core(&path).with_query_planner(Box::new(StubLlmClient {
        response: EXPANSION_RESPONSE_NO_HINTS,
    }));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10 USING MODE 'precision'",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    assert_eq!(lines[lines.len() - 2], "ann_plan: plain_scan_precision");
    assert_eq!(lines[lines.len() - 1], "scalar_plan: plain_scan");
}

#[test]
fn explain_does_not_touch_hnsw_index_cache() {
    // `EXPLAIN` は検索本体を実行しない契約（`explain_does_not_write_or_execute_search`
    // と同じ方針）を HNSW opt-in エンジンでも維持することを、`hnsw_index_cache_stats`
    // が実行前後で全 0 のまま（`lookup`／`prepare_*` を一切呼ばない）で固定する。
    let path = unique_db_path("sql-explain-hnsw-no-index-touch");
    let _guard = CleanupGuard(path.clone());
    let core = seeded_hnsw_core(&path).with_query_planner(Box::new(StubLlmClient {
        response: EXPANSION_RESPONSE,
    }));

    let before = format!("{:?}", core.hnsw_index_cache_stats());

    core.execute_sql_in_session(
        &ctx("tenant-a"),
        &mut SessionState::default(),
        "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
    )
    .expect("EXPLAIN should succeed");

    let after = core.hnsw_index_cache_stats();
    assert_eq!(
        before,
        format!("{after:?}"),
        "EXPLAIN must not touch HnswIndexCache (no build/lookup side effects)"
    );
    // `sql::hnsw_cache` は `pub(crate)`（`HnswIndexCacheStats::default()` を
    // 外部クレートからは名指しできない）ため、フィールド和がゼロであることで
    // 「全 0」を確認する（全フィールドが `u64`／`usize` のため和 0 は全 0 と同値）。
    let total = after.hits
        + after.misses
        + after.builds
        + after.build_failures
        + after.rebuilds
        + after.delta_searches
        + after.fallbacks
        + after.plain_scans
        + after.masked_short
        + after.mask_splits_graph
        + after.subset_searches
        + after.ef_cap_fallbacks
        + after.hybrid_dense_searches
        + after.hybrid_queries
        + after.hybrid_rounds_max
        + after.hybrid_resumed_rounds
        + after.acorn_guard_fallbacks
        + after.entries as u64;
    assert_eq!(
        total, 0,
        "HnswIndexCache must remain untouched (all-zero stats) after EXPLAIN: {after:?}"
    );
}

#[test]
fn explain_new_rows_use_closed_vocabulary_and_default_hnsw_params() {
    // Issue #411 要件 3: 新規行（`engine:`／`hnsw_params:`／`ann_plan:`）が閉じた
    // 語彙集合の要素のみで構成され、可視カーディナリティ・行数等データ由来の値を
    // 含まないことを既定エンジン・HNSW opt-in の両方で機械的に固定する。
    const ENGINE_TOKENS: &[&str] = &[
        "cpu_scalar_brute_force",
        "parallel_brute_force",
        "hnsw",
        "(custom_provider)",
    ];
    const ANN_PLAN_TOKENS: &[&str] = &[
        "plain_scan_engine",
        "plain_scan_precision",
        "hnsw_full_visible",
        "hnsw_subset",
        "unknown_custom_provider",
    ];
    // Issue #474: `scalar_plan:` の閉じた語彙。
    const SCALAR_PLAN_TOKENS: &[&str] = &[
        "plain_scan",
        "index_equality",
        "index_prefix",
        "index_id_range",
        "index_conjunction",
    ];

    let path = unique_db_path("sql-explain-hnsw-closed-vocab");
    let _guard = CleanupGuard(path.clone());
    let core = seeded_hnsw_core(&path).with_query_planner(Box::new(StubLlmClient {
        response: EXPANSION_RESPONSE_NO_HINTS,
    }));

    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("EXPLAIN should succeed");

    let lines = explain_result_lines(outcome);
    let engine_line = lines
        .iter()
        .find(|l| l.starts_with("engine: "))
        .expect("engine line present");
    assert!(
        ENGINE_TOKENS
            .iter()
            .any(|t| engine_line == &format!("engine: {t}")),
        "unexpected engine token: {engine_line}"
    );
    let ann_plan_line = lines
        .iter()
        .find(|l| l.starts_with("ann_plan: "))
        .expect("ann_plan line present");
    assert!(
        ANN_PLAN_TOKENS
            .iter()
            .any(|t| ann_plan_line == &format!("ann_plan: {t}")),
        "unexpected ann_plan token: {ann_plan_line}"
    );
    let hnsw_params_line = lines.iter().find(|l| l.starts_with("hnsw_params: "));
    assert_eq!(
        hnsw_params_line,
        Some(
            &"hnsw_params: m=16,ef_construction=100,ef_search=64,resident=f32,sparse_visited_max=0"
                .to_string()
        ),
        "default HnswParams must round-trip exactly through EXPLAIN"
    );
    let scalar_plan_line = lines
        .iter()
        .find(|l| l.starts_with("scalar_plan: "))
        .expect("scalar_plan line present");
    assert!(
        SCALAR_PLAN_TOKENS
            .iter()
            .any(|t| scalar_plan_line == &format!("scalar_plan: {t}")),
        "unexpected scalar_plan token: {scalar_plan_line}"
    );
    assert_eq!(
        lines.last().map(String::as_str),
        Some("scalar_plan: plain_scan"),
        "scalar_plan is the last EXPLAIN row (no WHERE clause in this query)"
    );
}
