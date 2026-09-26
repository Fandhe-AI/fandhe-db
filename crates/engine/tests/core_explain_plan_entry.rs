//! `EngineCore::explain_bound_plan_in_session`（TASK-186・NOSQL-10。
//! Issue #765）が、単一の `Storage` を `EngineCore` が所有したまま SQL
//! テキストを経由せずに束縛済み `USING PLAN` 検索計画の `EXPLAIN` を実行
//! できること、SQL 表層の `Statement::Explain` アーム（`tests/sql_explain.rs`）
//! と行単位で完全一致すること、検索本体を実行しないこと、fail-closed に
//! 拒否すべき入力を拒否することを固定する結合テスト。
//!
//! `tests/core_bound_plan_entry.rs`（scan／aggregate。Issue #728）・
//! `tests/sql_insert_explain_public_api.rs`（insert／explain 公開 API の
//! 到達性。Issue #730）と同じ流儀で、`Storage` を `EngineCore::from_storage`
//! （または `from_storage_with_engine`）へそのまま渡して単一 `Storage`
//! 構成を保つ。決定的スタブ `LlmClient` を使い、実 Ollama への疎通は対象外
//! （`tests/sql_explain.rs::StubLlmClient` と同型）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::declarative_filter::{self, DeclarativeFilter};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::{Cell, ColumnMeta};
use engine::sql::explain::ExplainShape;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use std::sync::{Arc, Mutex};

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
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// tenant-a に `lang="ja"` の可視行を 1 件投入した `Storage` を返す。
fn seeded_storage(path: &std::path::Path) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let op_id = OperationId::parse("explain-plan-entry-op-1").expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx("tenant-a"),
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![0.1, 0.2, 0.3, 0.4]),
            Value::Text("ja".to_string()),
            Value::Text("docs/a.md".to_string()),
            Value::Text("alpha content in english".to_string()),
        ],
        &op_id,
    )
    .expect("insert tenant-a row");
    storage
}

/// tenant-b 専用の可視行（`docs/b-secret.md`）を追加で投入する（RLS 非漏えい
/// 確認用）。
fn seed_tenant_b_row(storage: &Storage) {
    let op_id = OperationId::parse("explain-plan-entry-op-101").expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx("tenant-b"),
        101,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 0.0, 0.0, 0.0]),
            Value::Text("xx".to_string()),
            Value::Text("docs/b-secret.md".to_string()),
            Value::Text("tenant-b only content".to_string()),
        ],
        &op_id,
    )
    .expect("insert tenant-b row");
}

/// `sql_explain.rs::StubLlmClient` と同型の決定的スタブ。
struct StubLlmClient {
    response: &'static str,
}

impl LlmClient for StubLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(self.response.to_string())
    }
}

/// プロンプト（辞書スナップショットを含む展開入力）を記録する決定的スタブ
/// （`tests/query_planner.rs::MockLlmClient` と同構成）。`StubLlmClient` は
/// 固定応答のみを返しプロンプト内容を無視するため、RLS 非漏えいの確認には
/// 「応答に他テナント語彙が現れないこと」しか固定できない（codex-review
/// 指摘 PR #828）。本スタブは実際に `EngineCore` から渡されたプロンプト
/// そのものへ他テナント語彙が混入していないことを固定するために使う。
struct RecordingLlmClient {
    response: &'static str,
    seen_prompts: Arc<Mutex<Vec<String>>>,
}

impl LlmClient for RecordingLlmClient {
    fn complete(&self, prompt: &str) -> Result<String, PlanError> {
        self.seen_prompts
            .lock()
            .expect("recording stub lock poisoned")
            .push(prompt.to_string());
        Ok(self.response.to_string())
    }
}

/// 呼び出し回数を記録するスタブ（`sql_explain.rs::CountingLlmClient` と同構成）。
/// 束縛失敗が LLM 呼び出しより前に完結することを直接確認するために使う。
struct CountingLlmClient {
    response: &'static str,
    calls: std::sync::atomic::AtomicUsize,
}

impl CountingLlmClient {
    fn new(response: &'static str) -> Self {
        Self {
            response,
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
        Ok(self.response.to_string())
    }
}

const EXPANSION_RESPONSE: &str =
    r#"{"search_terms": ["alpha", "beta"], "path_hint": "docs/", "kind_hint": "fn"}"#;
const EXPANSION_RESPONSE_NO_HINTS: &str =
    r#"{"search_terms": [], "path_hint": null, "kind_hint": null}"#;

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

/// 新エントリの結果を SQL `EXPLAIN` と同じ `Vec<String>` 形へ揃える。
fn explain_entry_lines(
    result: Result<engine::sql::exec::QueryResult, SqlSurfaceError>,
) -> Vec<String> {
    let result = result.expect("explain_bound_plan_in_session should succeed");
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

/// フィルタなしの binder closure（`ExplainShape::from_filters(&[], &[], &[])`）。
fn no_filter_bind(
    _schema: &engine::catalog::TableSchema,
    _udfs: &engine::sql::udf_call::UdfRegistry,
) -> Result<ExplainShape, SqlSurfaceError> {
    Ok(ExplainShape::from_filters(&[], &[], &[]))
}

#[test]
fn explain_entry_matches_sql_explain_rows_without_filter() {
    let path = unique_db_path("core-explain-plan-entry-no-filter");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    drop(storage);

    let core = EngineCore::open(&path)
        .expect("open engine core")
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));
    assert_eq!(
        core.search_engine_kind(),
        Some(search_engine::SearchEngineKind::ParallelBruteForce)
    );

    let tenant_ctx = ctx("tenant-a");
    let mut session = SessionState::default();
    let sql_outcome = core
        .execute_sql_in_session(
            &tenant_ctx,
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 5",
        )
        .expect("EXPLAIN should succeed");
    let sql_lines = explain_result_lines(sql_outcome);

    let entry_result = core.explain_bound_plan_in_session(
        &tenant_ctx,
        &SessionState::default(),
        TABLE,
        "find content",
        None,
        no_filter_bind,
    );
    let entry_lines = explain_entry_lines(entry_result);

    assert_eq!(
        sql_lines, entry_lines,
        "SQL EXPLAIN と行単位で完全一致すること"
    );
    assert_eq!(sql_lines.len(), 9);
    assert!(sql_lines.contains(&"engine: parallel_brute_force".to_string()));
    assert!(sql_lines.contains(&"ann_plan: plain_scan_engine".to_string()));
    assert!(sql_lines.contains(&"scalar_plan: plain_scan".to_string()));
}

#[test]
fn explain_entry_matches_sql_explain_rows_with_equality_filter() {
    let path = unique_db_path("core-explain-plan-entry-filter");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    drop(storage);

    let core = EngineCore::open(&path)
        .expect("open engine core")
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE_NO_HINTS,
        }));

    let tenant_ctx = ctx("tenant-a");
    let mut session = SessionState::default();
    let sql_outcome = core
        .execute_sql_in_session(
            &tenant_ctx,
            &mut session,
            "EXPLAIN SELECT id FROM docs WHERE lang = 'ja' USING PLAN('find content') LIMIT 5",
        )
        .expect("EXPLAIN should succeed");
    let sql_lines = explain_result_lines(sql_outcome);

    let entry_result = core.explain_bound_plan_in_session(
        &tenant_ctx,
        &SessionState::default(),
        TABLE,
        "find content",
        None,
        |schema, _udfs| {
            let filters =
                declarative_filter::bind_all(&[DeclarativeFilter::equals("lang", "ja")], schema)?;
            Ok(ExplainShape::from_filters(&filters, &[], &[]))
        },
    );
    let entry_lines = explain_entry_lines(entry_result);

    assert_eq!(sql_lines, entry_lines);
    assert!(sql_lines.contains(&"scalar_plan: index_equality".to_string()));
}

#[test]
fn explain_entry_reports_hnsw_params_and_does_not_touch_hnsw_index_cache() {
    let path = unique_db_path("core-explain-plan-entry-hnsw");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind).with_query_planner(Box::new(
        StubLlmClient {
            response: EXPANSION_RESPONSE_NO_HINTS,
        },
    ));

    let tenant_ctx = ctx("tenant-a");
    let mut session = SessionState::default();
    let sql_outcome = core
        .execute_sql_in_session(
            &tenant_ctx,
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 5",
        )
        .expect("EXPLAIN should succeed");
    let sql_lines = explain_result_lines(sql_outcome);
    assert!(sql_lines.iter().any(|l| l.starts_with("hnsw_params:")));

    let stats_before = core.hnsw_index_cache_stats();
    let entry_result = core.explain_bound_plan_in_session(
        &tenant_ctx,
        &SessionState::default(),
        TABLE,
        "find content",
        None,
        no_filter_bind,
    );
    let entry_lines = explain_entry_lines(entry_result);
    let stats_after = core.hnsw_index_cache_stats();

    assert_eq!(sql_lines, entry_lines);
    // `EXPLAIN` は索引の `lookup`／`prepare_*` を一切呼ばない契約
    // （`run_explain_plan` モジュールドキュメント参照）。
    assert_eq!(stats_before.hits, 0);
    assert_eq!(stats_before.misses, 0);
    assert_eq!(stats_before.builds, 0);
    assert_eq!(stats_after.hits, 0);
    assert_eq!(stats_after.misses, 0);
    assert_eq!(stats_after.builds, 0);
}

#[test]
fn explain_entry_rejects_undefined_table_before_invoking_binder() {
    let path = unique_db_path("core-explain-plan-entry-undefined-table");
    let _guard = CleanupGuard(path.clone());
    // テーブルを一切作らないスローアウェイ core。
    let core = EngineCore::open(&path)
        .expect("open engine core")
        .with_query_planner(Box::new(CountingLlmClient::new(EXPANSION_RESPONSE)));

    let binder_calls = std::sync::atomic::AtomicUsize::new(0);
    let result = core.explain_bound_plan_in_session(
        &ctx("tenant-a"),
        &SessionState::default(),
        TABLE,
        "find content",
        None,
        |_schema, _udfs| -> Result<ExplainShape, SqlSurfaceError> {
            binder_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ExplainShape::from_filters(&[], &[], &[]))
        },
    );
    let err = result.expect_err("undefined table must be rejected");
    assert_eq!(err.wire_code(), "42P01");
    assert_eq!(
        binder_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "binder must not be invoked when the table does not exist"
    );
}

/// 未知テーブル＋`mode_literal` 値不正は `42P01` が `22000` より優先される
/// （Cursor Bugbot 指摘対応・PR #828 レビュー。`run_explain_plan` が
/// テーブル解決より先に `mode_literal` を解析すると、テーブル未存在＋
/// mode 値不正の要求で `42P01` より先に `22000` が確定してしまう回帰。
/// `execute_bound_plan_search_in_session`〔`run_using_plan_select`〕と
/// 同じ優先順位を `EXPLAIN` 経路でも保証する）。
#[test]
fn explain_entry_rejects_undefined_table_before_invalid_mode_literal() {
    let path = unique_db_path("core-explain-plan-entry-undefined-table-bad-mode");
    let _guard = CleanupGuard(path.clone());
    // テーブルを一切作らないスローアウェイ core。
    let core = EngineCore::open(&path)
        .expect("open engine core")
        .with_query_planner(Box::new(CountingLlmClient::new(EXPANSION_RESPONSE)));

    let binder_calls = std::sync::atomic::AtomicUsize::new(0);
    let result = core.explain_bound_plan_in_session(
        &ctx("tenant-a"),
        &SessionState::default(),
        TABLE,
        "find content",
        Some("fuzzy"),
        |_schema, _udfs| -> Result<ExplainShape, SqlSurfaceError> {
            binder_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(ExplainShape::from_filters(&[], &[], &[]))
        },
    );
    let err = result.expect_err("undefined table must be rejected before mode literal parsing");
    assert_eq!(err.wire_code(), "42P01");
    assert_eq!(
        binder_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "binder must not be invoked when the table does not exist"
    );
}

#[test]
fn explain_entry_propagates_binder_error_and_skips_llm_call() {
    let path = unique_db_path("core-explain-plan-entry-binder-error");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    drop(storage);

    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    struct ArcLlmClient(std::sync::Arc<CountingLlmClient>);
    impl LlmClient for ArcLlmClient {
        fn complete(&self, prompt: &str) -> Result<String, PlanError> {
            self.0.complete(prompt)
        }
    }
    let core = EngineCore::open(&path)
        .expect("open engine core")
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let result = core.explain_bound_plan_in_session(
        &ctx("tenant-a"),
        &SessionState::default(),
        TABLE,
        "find content",
        None,
        |_schema, _udfs| -> Result<ExplainShape, SqlSurfaceError> {
            Err(SqlSurfaceError::InvalidInput {
                detail: "unknown column: nope".to_string(),
            })
        },
    );
    let err = result.expect_err("binder error must propagate unchanged");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        planner.call_count(),
        0,
        "binder failure must be rejected before invoking the LLM (I/O amplification防止)"
    );
}

#[test]
fn explain_entry_fails_closed_without_query_planner() {
    let path = unique_db_path("core-explain-plan-entry-no-planner");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    drop(storage);

    // `with_query_planner` を呼ばない core（プランナー未注入）。
    let core = EngineCore::open(&path).expect("open engine core");

    let result = core.explain_bound_plan_in_session(
        &ctx("tenant-a"),
        &SessionState::default(),
        TABLE,
        "find content",
        None,
        no_filter_bind,
    );
    let err = result.expect_err("must fail closed without a query planner");
    assert_eq!(err.wire_code(), "XX000");
}

#[test]
fn explain_entry_rejects_table_missing_dictionary_columns() {
    let path = unique_db_path("core-explain-plan-entry-no-body-column");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    // `path`/`body` 列を欠くスキーマ（辞書必須列検証の対象）。
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new("embedding", ColumnType::Vector(DIM), false)],
        ))
        .expect("create table");
    drop(storage);

    let core = EngineCore::open(&path)
        .expect("open engine core")
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));

    let result = core.explain_bound_plan_in_session(
        &ctx("tenant-a"),
        &SessionState::default(),
        TABLE,
        "find content",
        None,
        no_filter_bind,
    );
    let err = result.expect_err("must reject tables without path/body columns");
    assert_eq!(err.wire_code(), "22000");
}

/// `plan` 欠落（binder closure が `42601` 系エラーを返す状況）は、辞書必須列
/// （`path`/`body`）を欠くテーブルに対しても `22000`（辞書必須列検証）より
/// 優先される（codex-review P1 指摘対応・PR #828。`run_explain_plan` が
/// 辞書必須列検証を `bind` 呼び出しより先に行うと、`crates/wire-server/src/
/// http/query/explain.rs::execute` の binder closure（`question.ok_or(..)?`）
/// が返す `plan` 欠落エラーより先に `22000` が確定してしまう回帰。通常検索
/// （`search::bind_search` の `VectorAndPlanBothMissing`＝`42601`）と同じ
/// `wire_code` を、辞書必須列を欠くテーブルに対しても保証する）。
#[test]
fn explain_entry_prioritizes_binder_plan_missing_error_over_dictionary_columns() {
    let path = unique_db_path("core-explain-plan-entry-plan-missing-no-dict");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    // `path`/`body` 列を欠くスキーマ（辞書必須列検証の対象）。
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![ColumnDef::new("embedding", ColumnType::Vector(DIM), false)],
        ))
        .expect("create table");
    drop(storage);

    let core = EngineCore::open(&path)
        .expect("open engine core")
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));

    // `wire-server::http::query::explain::execute` の binder closure が
    // `plan` 欠落時に返す `ExplainRequiresPlan`（`42601`）を模した closure。
    let result = core.explain_bound_plan_in_session(
        &ctx("tenant-a"),
        &SessionState::default(),
        TABLE,
        "", // `plan` 欠落（`explain::execute` のプレースホルダ空文字列と同型）
        None,
        |_schema, _udfs| -> Result<ExplainShape, SqlSurfaceError> {
            Err(SqlSurfaceError::UnsupportedSyntax {
                detail: "explain is only supported for search requests with \"plan\"".to_string(),
            })
        },
    );
    let err = result.expect_err("plan-missing binder error must win over dictionary column check");
    assert_eq!(err.wire_code(), "42601");
}

/// `explain_bound_plan_in_session` を NoSQL 表層の事前検証
/// （`http/query/explain.rs::execute` の `validate_using_plan_question`
/// 呼び出し）を経由せず直接呼ぶ経路（多層防御の対象）で、`bind`（`plan`
/// 欠落判定を含む）が成功したにもかかわらず `question` が空文字列の場合、
/// LLM 呼び出しより前に `22000` で拒否されることを固定する（codex-review
/// P1 指摘対応・PR #828 追加分）。
#[test]
fn explain_entry_rejects_empty_question_after_successful_bind_before_llm_call() {
    let path = unique_db_path("core-explain-plan-entry-empty-question");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    drop(storage);

    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    struct ArcLlmClient(std::sync::Arc<CountingLlmClient>);
    impl LlmClient for ArcLlmClient {
        fn complete(&self, prompt: &str) -> Result<String, PlanError> {
            self.0.complete(prompt)
        }
    }
    let core = EngineCore::open(&path)
        .expect("open engine core")
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    // `bind` は `plan` 欠落時とは異なり成功する（`no_filter_bind` は
    // `question` の中身を一切見ない）——NoSQL 表層のプレースホルダ空文字列
    // 規約に頼らず、engine 単独で空文字列を拒否できることを確認する。
    let result = core.explain_bound_plan_in_session(
        &ctx("tenant-a"),
        &SessionState::default(),
        TABLE,
        "",
        None,
        no_filter_bind,
    );
    let err = result.expect_err("empty question must be rejected even when bind succeeds");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        planner.call_count(),
        0,
        "empty question must be rejected before invoking the LLM (I/O amplification防止)"
    );
}

/// [`MAX_USING_PLAN_LEN`]（`sql::allowlist`）を超える `question` も、`bind`
/// 成功後・LLM 呼び出し前に `54000` で拒否されることを固定する（同上）。
#[test]
fn explain_entry_rejects_oversized_question_before_llm_call() {
    let path = unique_db_path("core-explain-plan-entry-oversized-question");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    drop(storage);

    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    struct ArcLlmClient(std::sync::Arc<CountingLlmClient>);
    impl LlmClient for ArcLlmClient {
        fn complete(&self, prompt: &str) -> Result<String, PlanError> {
            self.0.complete(prompt)
        }
    }
    let core = EngineCore::open(&path)
        .expect("open engine core")
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let oversized_question = "a".repeat(64 * 1024 + 1);
    let result = core.explain_bound_plan_in_session(
        &ctx("tenant-a"),
        &SessionState::default(),
        TABLE,
        &oversized_question,
        None,
        no_filter_bind,
    );
    let err = result.expect_err("oversized question must be rejected even when bind succeeds");
    assert_eq!(err.wire_code(), "54000");
    assert_eq!(
        planner.call_count(),
        0,
        "oversized question must be rejected before invoking the LLM (I/O amplification防止)"
    );
}

#[test]
fn explain_entry_does_not_leak_other_tenant_row_content() {
    let path = unique_db_path("core-explain-plan-entry-rls");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    seed_tenant_b_row(&storage);
    drop(storage);

    let seen_prompts = Arc::new(Mutex::new(Vec::new()));
    let core = EngineCore::open(&path)
        .expect("open engine core")
        .with_query_planner(Box::new(RecordingLlmClient {
            response: EXPANSION_RESPONSE,
            seen_prompts: Arc::clone(&seen_prompts),
        }));

    let entry_result = core.explain_bound_plan_in_session(
        &ctx("tenant-a"),
        &SessionState::default(),
        TABLE,
        "find content",
        None,
        no_filter_bind,
    );
    let entry_lines = explain_entry_lines(entry_result);
    let joined = entry_lines.join("\n");
    assert!(
        !joined.contains("docs/b-secret.md"),
        "EXPLAIN の展開結果はスタブ固定値のため他テナント語彙が混入しないこと"
    );
    assert!(!joined.contains("tenant-b"));

    // `RecordingLlmClient` はスタブ応答を無視せず実際に渡されたプロンプトを
    // 記録するため、辞書スナップショットを含む展開入力そのものに他テナント
    // 語彙が混入していないことも固定する（codex-review 指摘 PR #828:
    // 固定応答スタブでは応答側しか検証できず、プロンプト側の混入は見逃せる）。
    let prompts = seen_prompts.lock().expect("recording stub lock poisoned");
    assert!(
        !prompts.is_empty(),
        "LLM 呼び出しが発生していない（テストが vacuous）"
    );
    for prompt in prompts.iter() {
        assert!(
            !prompt.contains("docs/b-secret.md"),
            "辞書スナップショットに他テナントの path が混入している: {prompt}"
        );
        assert!(
            !prompt.contains("tenant-b"),
            "プロンプトに他テナント語彙が混入している: {prompt}"
        );
        assert!(
            !prompt.contains("tenant-b only content"),
            "辞書スナップショットに他テナントの body 内容が混入している: {prompt}"
        );
    }
    // 上記の非混入アサーションだけでは、辞書スナップショット自体が空
    // （＝そもそも何も渡していない）場合にも同じく green になってしまい
    // 非漏えい検証として vacuous になる（advisor 指摘）。tenant-a 自身の
    // `path`（`render_prompt_prefix` の `# Files` 節に決定的にそのまま
    // 現れる。`crates/engine/src/query_planner.rs::render_prompt_prefix`）
    // が実際にプロンプトへ含まれていることを固定し、辞書内容そのものが
    // 渡っていることを非 vacuous に確認する。
    assert!(
        prompts.iter().any(|prompt| prompt.contains("docs/a.md")),
        "自テナント（tenant-a）の辞書内容がプロンプトに含まれていない\
         （非漏えい検証が vacuous）"
    );
}

/// クレート外（本テストファイル）からの直接呼び出しが `EngineCore::
/// execute_sql_in_session`（セッション必須のエントリ）を経由せずに使える
/// ことの非 vacuous 確認: `Storage` を単一に保ったまま `EngineCore::open`
/// 直後（`SessionState::default()`）でも成功する。
#[test]
fn explain_entry_works_without_a_prior_sql_session() {
    let path = unique_db_path("core-explain-plan-entry-fresh-session");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    drop(storage);

    let core = EngineCore::open(&path)
        .expect("open engine core")
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE_NO_HINTS,
        }));

    let result = core.explain_bound_plan_in_session(
        &ctx("tenant-a"),
        &SessionState::default(),
        TABLE,
        "find content",
        None,
        no_filter_bind,
    );
    let _ = explain_entry_lines(result);
    // 呼び出し自体が `CpuScalarProvider` の直接依存を持たないことも確認
    // （import が unused にならないよう明示的に参照する）。
    let _ = CpuScalarProvider;
}
