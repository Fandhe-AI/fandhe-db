//! `USING PLAN('<query>')` の実行時ディスパッチ受け入れテスト（TASK-77、対象
//! ビヘイビア: SQL-5。ポインタ: `docs/spec/05-tasks.md` TASK-77・
//! `docs/spec/04-behavior/sql-surface.md` SQL-5）。
//!
//! `tests/sql_allowlist.rs`（存在すれば構文層）とは独立に、実行時の一意ディスパッチ
//! （`sql::allowlist` の構造受理 → `core.rs::EngineCore::plan_query`（TASK-110）→
//! 展開後テキストの再埋め込み → `sql::using_plan::bind_expansion` → 既存 C4
//! ハイブリッド実行形 → `sql::exec::execute_statement`）を、スタブ `LlmClient`・
//! 決定的なテスト `Embedder` を注入して検証する。RLS 暗黙適用の検証は
//! `tests/rls_generalized.rs::using_plan_dispatch_implicitly_applies_rls` が担う
//! （本ファイルでは重複させない）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CoreError, EngineCore};
use engine::embedding::{EmbedError, Embedder};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
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
    let op_id = OperationId::parse(&format!("plan-test-op-{id}")).expect("valid operation_id");
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

/// 展開後テキストごとに異なる決定的ベクトルを返す（`text.len()` を成分へ埋め込む
/// だけの単純な写像）。「原質問の埋め込みを使い回していない」ことをテストが
/// 区別できるよう、`embed_batch` に渡された **実際のテキスト** を記録する。
struct RecordingEmbedder {
    dim: u32,
    seen: std::sync::Mutex<Vec<String>>,
}

impl RecordingEmbedder {
    fn new(dim: u32) -> Self {
        Self {
            dim,
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn seen_texts(&self) -> Vec<String> {
        self.seen.lock().expect("lock").clone()
    }
}

impl Embedder for RecordingEmbedder {
    fn dim(&self) -> u32 {
        self.dim
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut seen = self.seen.lock().expect("lock");
        let mut out = Vec::with_capacity(texts.len());
        for &t in texts {
            seen.push(t.to_string());
            let base = t.len() as f32 * 0.01;
            out.push(vec![base; self.dim as usize]);
        }
        Ok(out)
    }
}

/// 固定の展開結果を返すスタブ（実 Ollama 疎通は TASK-110 と同じくスコープ外）。
struct StubLlmClient {
    response: &'static str,
}

impl LlmClient for StubLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(self.response.to_string())
    }
}

/// 常に非有限（NaN）成分を含むベクトルを返す埋め込み（Cursor Bugbot Medium
/// 指摘の回帰用: 外部埋め込み実装の異常出力を模す）。
struct NonFiniteEmbedder {
    dim: u32,
}

impl Embedder for NonFiniteEmbedder {
    fn dim(&self) -> u32 {
        self.dim
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(vec![vec![f32::NAN; self.dim as usize]; texts.len()])
    }
}

/// 要求 1 件に対し常に 2 件のベクトルを返す埋め込み（codex-review P1 回帰用:
/// `EngineCore::plan_using_plan_expansion` が `embed_batch` の戻り値件数を
/// 検証せず `into_iter().next()` で先頭 1 件のみを黙って採用していた契約違反、
/// PR #266）。
struct TooManyVectorsEmbedder {
    dim: u32,
}

impl Embedder for TooManyVectorsEmbedder {
    fn dim(&self) -> u32 {
        self.dim
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(vec![vec![0.1; self.dim as usize]; texts.len() + 1])
    }
}

struct FailingLlmClient;

impl LlmClient for FailingLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Err(PlanError::Unavailable)
    }
}

/// 呼び出し回数を記録するスタブ（codex-review P1 指摘対応、PR #266: 無効な
/// `LIMIT` が高コストな LLM クエリ展開（`plan_using_plan_expansion`）より前に
/// 拒否されることを、呼び出し回数 0 で直接確認するために使う）。
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

/// `CountingLlmClient` を `Arc` で共有しつつ `Box<dyn LlmClient>` として注入する
/// ための薄い転送アダプタ（テスト後に呼び出し回数を読み出すため、所有権を
/// `EngineCore` へ渡し切らず `Arc` で保持する必要がある）。
struct ArcLlmClient(std::sync::Arc<CountingLlmClient>);

impl LlmClient for ArcLlmClient {
    fn complete(&self, prompt: &str) -> Result<String, PlanError> {
        self.0.complete(prompt)
    }
}

const EXPANSION_RESPONSE: &str =
    r#"{"search_terms": ["alpha", "beta"], "path_hint": null, "kind_hint": null}"#;

fn seeded_storage(path: &std::path::Path) -> Storage {
    let storage = open_storage_with_table(path);
    insert_row(
        &storage,
        1,
        vec![0.1, 0.2, 0.3, 0.4],
        "docs/a.md",
        "alpha content in english",
    );
    insert_row(
        &storage,
        2,
        vec![0.4, 0.3, 0.2, 0.1],
        "docs/b.md",
        "beta content in english",
    );
    storage
}

#[test]
fn using_plan_dispatch_reaches_hybrid_execution_and_returns_seeded_rows() {
    let path = unique_db_path("sql-using-plan-dispatch");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)))
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));

    let result = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("USING PLAN dispatch should succeed");
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
}

#[test]
fn using_plan_reembeds_expanded_text_not_the_raw_question() {
    // PLAN-10 ポインタ: 密側の再埋め込み対象は `query_planner::
    // render_reembedding_text`（固定接頭辞 `search_query: ` ＋質問＋展開検索語の
    // 決定的結合）であり、原質問だけの埋め込みを使い回さない（codex-review P1
    // 指摘対応、PR #266。密側と疎側〔`sql::using_plan::expanded_query_text`〕は
    // 別テキスト）。`RecordingEmbedder` が実際に渡されたテキストを記録するため、
    // 原質問そのものとは異なることを直接確認する。
    let path = unique_db_path("sql-using-plan-reembed");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let embedder = std::sync::Arc::new(RecordingEmbedder::new(DIM));

    struct ArcEmbedder(std::sync::Arc<RecordingEmbedder>);
    impl Embedder for ArcEmbedder {
        fn dim(&self) -> u32 {
            self.0.dim()
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.0.embed_batch(texts)
        }
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(ArcEmbedder(embedder.clone())))
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));

    let question = "find content";
    core.execute_sql(
        &ctx("tenant-a"),
        &format!("SELECT id FROM docs USING PLAN('{question}') LIMIT 10"),
    )
    .expect("USING PLAN dispatch should succeed");

    let seen = embedder.seen_texts();
    assert_eq!(
        seen.len(),
        1,
        "expected exactly one embed_batch call, got {seen:?}"
    );
    assert_ne!(
        seen[0], question,
        "USING PLAN must not re-embed the raw question verbatim (PLAN-10)"
    );
    assert_eq!(seen[0], "search_query: find content alpha beta");
}

#[test]
fn using_plan_fails_closed_without_query_planner() {
    let path = unique_db_path("sql-using-plan-no-planner");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs USING PLAN('q') LIMIT 5",
        )
        .expect_err("missing query planner must be rejected");
    assert_eq!(err.wire_code(), "XX000");
}

#[test]
fn using_plan_fails_closed_without_embedder() {
    let path = unique_db_path("sql-using-plan-no-embedder");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }),
    );

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs USING PLAN('q') LIMIT 5",
        )
        .expect_err("missing embedder must be rejected");
    assert_eq!(err.wire_code(), "XX000");
}

#[test]
fn using_plan_fails_closed_when_llm_response_is_unavailable() {
    let path = unique_db_path("sql-using-plan-llm-down");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)))
        .with_query_planner(Box::new(FailingLlmClient));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs USING PLAN('q') LIMIT 5",
        )
        .expect_err("LLM failure must be rejected, not silently degraded");
    assert_eq!(err.wire_code(), "XX000");
    // 部分結果を返さない（呼び出し自体が Err）ことに加え、固定文言のみで構成
    // されることを確認する（プロンプト本文・LLM 応答本文を含めない、security.md P0）。
    assert_eq!(err.client_message(), "internal error");
}

#[test]
fn using_plan_fails_closed_without_body_column() {
    // `path` 列はあるが `body` 列を持たないテーブル。`USING PLAN` の LLM 展開
    // （`plan_query`、TASK-110）が辞書抽出の前提として `path`／`body` の両方を
    // 非 null `TEXT` 列に要求する（`core.rs::EngineCore::dictionary_snapshot`）ため、
    // `body` 欠落は本来 LLM 呼び出し前のスキーマ事前検証
    // （`core.rs::EngineCore::execute_sql_in_session` の `Statement::Select` アーム、
    // `dictionary_required_columns` 経由）で `SqlSurfaceError::InvalidInput`
    // （`22000`）として拒否されるべき通常の利用者スキーマ不備であり、LLM 呼び出し
    // 自体の失敗（`Internal`／`XX000`）ではない（codex-review P1 指摘対応、
    // PR #266: 事前検証を追加する前は `dictionary_snapshot` の失敗が一律
    // `Internal` へ丸められていた）。`bind_expansion` 側の本文列解決は多層防御
    // として `crates/engine/src/sql/using_plan.rs` の単体テストで別途固定する。
    let path = unique_db_path("sql-using-plan-no-body");
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
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)))
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM no_body USING PLAN('q') LIMIT 5",
        )
        .expect_err("missing body column must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn using_plan_respects_using_mode_precision() {
    let path = unique_db_path("sql-using-plan-mode");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)))
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));

    // `precision` モードは確信度ゲート（`crate::precision::apply_gate`）を通すため、
    // ダミーの定数ベクトル入力では 0 件へ収束しうる（fail-closed な低確信度拒否は
    // 意図した挙動。SEARCH-9 の管轄）。本テストの目的は「`USING MODE` の優先順位
    // 解決が `USING PLAN` 経路でも既存どおり効くこと」であり、`recall`（既定）
    // モードでは同じクエリが結果を返すことと対比して確認する。
    //
    // Issue #315 ポインタ: 「`USING PLAN` が実 Ollama 経由で SQL エラーなしに
    // 0 行を返す」という観測は、この `precision` 経路の契約どおりの挙動と一致する
    // （`docs/design/using-plan-precision-empty-result.md` 参照）。以下 0 件は
    // 非決定的な偶然ではなく、`RecordingEmbedder` が seed 2 行・クエリのいずれにも
    // `t.len()` 由来の定数ベクトルを返すため、密・疎の 1 位が一致せず正規化 RRF の
    // 最良スコアが確信度ゲートの下限を下回ることで決定的に生じる。
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "SELECT id FROM docs USING PLAN('find content') LIMIT 10 USING MODE 'precision'",
        )
        .expect("USING MODE should be honored alongside USING PLAN (SQL エラーにならない)");
    match outcome {
        SqlOutcome::Query(result) => {
            assert!(
                result.rows.is_empty(),
                "precision mode's confidence gate is expected to yield an empty result set \
                 (not a SQL error) for this deterministic constant-vector fixture — Issue #315"
            );
        }
        other => panic!("expected Query outcome, got {other:?}"),
    }

    let recall_outcome = core
        .execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "SELECT id FROM docs USING PLAN('find content') LIMIT 10 USING MODE 'recall'",
        )
        .expect("recall mode should succeed");
    match recall_outcome {
        SqlOutcome::Query(result) => {
            assert!(
                !result.rows.is_empty(),
                "recall mode should return the seeded rows (no confidence gate)"
            );
        }
        other => panic!("expected Query outcome, got {other:?}"),
    }
}

#[test]
fn using_plan_fails_closed_on_embedder_table_dim_mismatch() {
    // `embedding.rs` の契約: 呼び出し元が `Embedder::dim` を対象テーブルの
    // `VECTOR(N)` と突き合わせて検証する。ここではテーブルが `VECTOR(4)` なのに
    // 次元 8 を返す埋め込みを注入し、既存の `ORDER BY` 経路
    // （`sql::parser::parse_vector_literal`）と同じ不変条件が `USING PLAN` の
    // 再埋め込みベクトルにも課されることを固定する。
    let path = unique_db_path("sql-using-plan-dim-mismatch");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM * 2)))
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs USING PLAN('q') LIMIT 5",
        )
        .expect_err("embedder/table dimension mismatch must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn using_plan_dispatch_error_variant_is_query_planning_or_dispatch_related() {
    // TASK-77 は既存の `CoreError`/`SqlSurfaceError` 分類のみを使い、`PlanError`
    // 用の新規 wire_code 分類を追加しない（ERR-2、TASK-152 の単一真実源を保つ）。
    // ここでは `plan_query` 単体の契約（TASK-110）を素通しで使っていることを、
    // `CoreError::QueryPlannerUnavailable` を直接発生させて確認する。
    let path = unique_db_path("sql-using-plan-core-error-shape");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let err = core
        .plan_query(&ctx("tenant-a"), TABLE, "q")
        .expect_err("plan_query without a configured planner must fail");
    assert!(matches!(err, CoreError::QueryPlannerUnavailable));
}

#[test]
fn using_plan_rejects_non_finite_reembedded_vector() {
    // codex-review P1 回帰: 再埋め込みベクトルの束縛時に次元しか検証しておらず、
    // NaN/Inf を含む値がフィルタされないまま `Ranking::Hybrid` へ渡っていた
    // （既存のベクトルリテラル経路 `parse_vector_literal`・`EngineCore::search` の
    // 非有限値拒否と非対称）。外部埋め込み実装が異常値を返す場合に fail-closed で
    // 拒否されることを固定する。
    let path = unique_db_path("sql-using-plan-non-finite");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(NonFiniteEmbedder { dim: DIM }))
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect_err("non-finite re-embedded vector must be rejected");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn using_plan_rejects_embedder_returning_multiple_vectors_for_single_query() {
    // codex-review P1 回帰（PR #266）: `plan_using_plan_expansion` は要求 1 件
    // （`dense_query_text` の 1 要素スライス）に対し `embed_batch` の戻り値が
    // 厳密に 1 件であることを確認せず、`into_iter().next()` で先頭 1 件のみを
    // 黙って採用していた。これは `query_planner::reembed_expansion`
    // （`vectors.len() != 1` を `EmbedError::InvalidResponse` で fail-closed に
    // 拒否）が課す既存の防御を迂回する契約違反応答の黙認であり、`USING PLAN`
    // 経路だけ異なる（緩い）挙動になっていた。ここでは常に 2 件のベクトルを
    // 返す `Embedder` を注入し、`USING PLAN` が成功として扱わず fail-closed に
    // 拒否することを固定する（修正前は本テストが `expect_err` の代わりに
    // `Ok` を返し fail する）。
    let path = unique_db_path("sql-using-plan-too-many-vectors");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(TooManyVectorsEmbedder { dim: DIM }))
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect_err("embedder returning multiple vectors for a single query must be rejected");
    assert_eq!(err.wire_code(), "XX000");
}

#[test]
fn using_plan_applies_planner_mode_hint_when_no_explicit_mode_is_set() {
    // codex-review P1 回帰: プランナーが推定した検索モード
    // （`expansion.mode_hint`）を `resolve_mode` へ渡さず捨てていたため、明示指定
    // （`USING MODE`／`SET search_mode`）が無い `USING PLAN` クエリではプランナーが
    // `precision` と推定しても常に既定の `recall` になっていた
    // （`resolve_mode_with_planner` 未使用が原因）。ここでは、明示指定なしで
    // プランナーが `precision` を推定したクエリの結果が、同じクエリへ明示的に
    // `USING MODE 'precision'` を付けた場合と一致する（＝プランナー推定が実際に
    // 効いている）ことを固定する。ダミーの定数ベクトル入力に対する確信度ゲートの
    // 通過件数自体は非決定的な前提を置かない（`using_plan_respects_using_mode_precision`
    // 参照）ため、絶対件数ではなく「明示 `precision` と同じ結果集合になる」ことを
    // 検証する。
    let path = unique_db_path("sql-using-plan-mode-hint");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)))
        .with_query_planner(Box::new(StubLlmClient {
            response: r#"{"search_terms": ["alpha", "beta"], "path_hint": null, "kind_hint": null, "mode": "precision"}"#,
        }));

    fn query_row_ids(outcome: SqlOutcome) -> Vec<u64> {
        match outcome {
            SqlOutcome::Query(result) => {
                let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
                ids.sort_unstable();
                ids
            }
            other => panic!("expected Query outcome, got {other:?}"),
        }
    }

    let mut session = SessionState::default();
    let implicit_ids = query_row_ids(
        core.execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("USING PLAN dispatch with an implicit planner mode hint should succeed"),
    );
    let explicit_precision_ids = query_row_ids(
        core.execute_sql_in_session(
            &ctx("tenant-a"),
            &mut session,
            "SELECT id FROM docs USING PLAN('find content') LIMIT 10 USING MODE 'precision'",
        )
        .expect("USING PLAN dispatch with an explicit precision mode should succeed"),
    );

    assert_eq!(
        implicit_ids, explicit_precision_ids,
        "a planner-estimated precision mode hint (no explicit USING MODE/SET) must resolve to \
         the same effective mode as an explicit USING MODE 'precision' clause"
    );
}

#[test]
fn using_plan_rejects_invalid_limit_before_invoking_query_planner() {
    // codex-review P1（PR #266）指摘の判別テスト: `LIMIT` の範囲検証（`22000`）が
    // `plan_using_plan_expansion`（辞書スナップショット構築＋LLM クエリ展開）より
    // 前に完結する契約（`core.rs::EngineCore::execute_sql_in_session` の
    // `Statement::Select` アーム・`USING PLAN` 分岐ドキュメント参照）を、スタブ
    // プランナーの呼び出し回数が 0 のままであることで直接確認する。`LIMIT 0` は
    // 構文（`sql::allowlist`）上は受理されるが意味論検証で必ず拒否される値であり、
    // 修正前はこの拒否より前に `plan_query`（プランナー呼び出し）が実行されて
    // いた。
    let path = unique_db_path("sql-using-plan-invalid-limit-zero");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs USING PLAN('find content') LIMIT 0",
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
fn using_plan_rejects_out_of_range_limit_before_invoking_query_planner() {
    // 上のテストの対（`LIMIT` 上限超過側）。構文上は `u32::MAX` まで受理されるが、
    // `MAX_SEARCH_K` 超過は必ず拒否される値であり、こちらも `plan_query` 実行前に
    // 拒否されることを確認する。
    let path = unique_db_path("sql-using-plan-invalid-limit-huge");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs USING PLAN('find content') LIMIT 4294967295",
        )
        .expect_err("LIMIT far beyond MAX_SEARCH_K must be rejected");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        planner.call_count(),
        0,
        "invalid LIMIT must be rejected before the high-cost query planner call"
    );
}

#[test]
fn using_plan_rejects_invalid_using_mode_before_invoking_query_planner() {
    // codex-review P1（PR #266）指摘の判別テスト: `USING MODE` リテラルの解析
    // （`SearchMode::parse_literal`、`22000`）は構文上受理されるが必ず拒否される
    // 値（`recall`／`precision` 以外）であり、`plan_using_plan_expansion`（辞書
    // スナップショット構築＋LLM クエリ展開＋再埋め込み）より前に完結すべき
    // （`core.rs::EngineCore::execute_sql_in_session` の `USING PLAN` 分岐が
    // `sql::using_plan::pre_check_schema` を I/O 前に呼ぶ契約）。修正前は
    // `plan_using_plan_expansion` 内でのみモードリテラルを解析していたため、
    // 無効な `USING MODE` でも LLM 呼び出し・再埋め込みが先に実行されていた。
    let path = unique_db_path("sql-using-plan-invalid-mode");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs USING PLAN('find content') LIMIT 10 USING MODE 'invalid'",
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
fn using_plan_rejects_unknown_projection_column_before_invoking_query_planner() {
    // codex-review P1（PR #266）指摘の判別テスト: 投影列（`SELECT` リスト）の
    // 束縛（`sql::parser::bind_projection`、`22000`）も、`USING MODE` と同じく
    // I/O 前の `pre_check_schema` で検証されるべき。未知列は構文上は受理される
    // が束縛不能で必ず拒否される。
    let path = unique_db_path("sql-using-plan-unknown-projection");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT no_such_column FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect_err("unknown projected column must be rejected");
    assert_eq!(err.wire_code(), "42703");
    assert_eq!(
        planner.call_count(),
        0,
        "unknown projection column must be rejected before the high-cost query planner call"
    );
}

#[test]
fn using_plan_rejects_unknown_where_column_before_invoking_query_planner() {
    // 上のテストの対（`WHERE` 述語側）。`sql::parser::bind_where_predicates` の
    // 未知列拒否（`22000`）も同じく I/O 前の `pre_check_schema` で検証される
    // べき。
    let path = unique_db_path("sql-using-plan-unknown-where");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            "SELECT id FROM docs WHERE no_such_column = 'x' USING PLAN('find content') LIMIT 10",
        )
        .expect_err("unknown WHERE column must be rejected");
    assert_eq!(err.wire_code(), "42703");
    assert_eq!(
        planner.call_count(),
        0,
        "unknown WHERE column must be rejected before the high-cost query planner call"
    );
}

#[test]
fn using_plan_rejects_expanded_query_text_exceeding_sparse_limits_before_reembedding() {
    // codex-review P1（PR #266）指摘の判別テスト: `sql::using_plan::
    // expanded_query_text`（原質問＋展開検索語の決定的結合）は疎側
    // （`hybrid_search` の全文検索側入力）専用の文字列だが、疎側
    // （`sparse::SparseIndex::search`/`search_within`）が課す `MAX_QUERY_BYTES`
    // （16 KiB）を考慮せずに構成すると、CJK のような多バイト文字を多用する展開結果
    // では受理されたクエリが再埋め込み後の `hybrid_search` でのみ失敗しうる
    // （拒否自体は既存の `22000` 契約〔`map_hybrid_error`〕で fail-closed だが、
    // 再埋め込みという高コスト I/O を消費した後段でしか検出できていなかった）。
    //
    // 原質問（[`engine::query_planner::MAX_QUESTION_CHARS`] ちょうどの CJK）と
    // 展開検索語（[`engine::query_planner::MAX_SEARCH_TERMS`] 件 ×
    // [`engine::query_planner::MAX_TERM_LEN`] 文字の CJK）を両方ともそれぞれの
    // 文字数上限ちょうどに構成し、UTF-8 での多バイト化により結合後のバイト長が
    // （sparse 側の）バイト長上限を超えるケースを固定する（上限定数の変化に
    // 追随できるよう、リテラルの決め打ちではなく実際の公開定数から生成する）。
    // `CountingLlmClient` の呼び出し回数（1 回・展開自体は成功）と
    // `RecordingEmbedder` の記録件数（0 回）を確認することで、拒否が再埋め込み
    // より前で完結することを直接確認する。
    let path = unique_db_path("sql-using-plan-expanded-text-too-long");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);

    let question = "あ".repeat(engine::query_planner::MAX_QUESTION_CHARS);
    let term = "い".repeat(engine::query_planner::MAX_TERM_LEN);
    let terms_json = std::iter::repeat_n(
        format!("\"{term}\""),
        engine::query_planner::MAX_SEARCH_TERMS,
    )
    .collect::<Vec<_>>()
    .join(", ");
    let response =
        format!("{{\"search_terms\": [{terms_json}], \"path_hint\": null, \"kind_hint\": null}}");

    let planner = std::sync::Arc::new(CountingLlmClient::new(response));
    let embedder = std::sync::Arc::new(RecordingEmbedder::new(DIM));
    struct ArcEmbedder(std::sync::Arc<RecordingEmbedder>);
    impl Embedder for ArcEmbedder {
        fn dim(&self) -> u32 {
            self.0.dim()
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.0.embed_batch(texts)
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(ArcEmbedder(embedder.clone())))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            &format!("SELECT id FROM docs USING PLAN('{question}') LIMIT 10"),
        )
        .expect_err("expanded query text exceeding sparse limits must be rejected");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        embedder.seen_texts().len(),
        0,
        "over-long expanded query text must be rejected before the high-cost re-embedding call"
    );
}

#[test]
fn using_plan_accepts_cjk_question_within_sparse_limits() {
    // 上のテスト（`using_plan_rejects_expanded_query_text_exceeding_sparse_limits_
    // before_reembedding`）の対: sparse 側のバイト長上限（`sparse::
    // validate_query_bounds`）の事前検証は、限度内の CJK クエリを新たに拒否しては
    // ならない。展開後テキストが十分短い CJK 質問で `USING PLAN` 経路が従来どおり
    // 成功し、再埋め込み（`embedder.embed_batch`）が実際に 1 回実行されることを
    // 固定する。
    let path = unique_db_path("sql-using-plan-cjk-within-limits");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);
    let embedder = std::sync::Arc::new(RecordingEmbedder::new(DIM));
    struct ArcEmbedder(std::sync::Arc<RecordingEmbedder>);
    impl Embedder for ArcEmbedder {
        fn dim(&self) -> u32 {
            self.0.dim()
        }
        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
            self.0.embed_batch(texts)
        }
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(ArcEmbedder(embedder.clone())))
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));

    let question = "日本語のクエリ".repeat(10);
    core.execute_sql(
        &ctx("tenant-a"),
        &format!("SELECT id FROM docs USING PLAN('{question}') LIMIT 10"),
    )
    .expect("CJK question within sparse limits must still succeed");

    assert_eq!(
        embedder.seen_texts().len(),
        1,
        "a CJK question within limits must still reach the re-embedding call"
    );
}

#[test]
fn using_plan_rejects_raw_question_exceeding_sparse_query_terms_before_invoking_query_planner() {
    // codex-review P1 + Cursor Bugbot Medium（PR #266）指摘の判別テスト:
    // `plan_using_plan_expansion` は `plan_query`（辞書スナップショット構築＋LLM
    // 呼び出し、高コスト I/O）を実行した後にしか疎側の入力上限
    // （`sparse::validate_query_bounds`）を検証していなかった。展開は検索語を
    // 追加するだけで既存の語を減らさないため、展開前の原質問
    // （`query_planner::sanitize_question` 通過後）の時点で既に `MAX_QUERY_TERMS`
    // を超える場合、`plan_query` を呼んでも絶対に成功しえない。
    //
    // `MAX_QUESTION_CHARS` 以内（切り詰めなし）の CJK 質問で、`sparse::tokenize`
    // が CJK 文字ごとに生成する unigram のみで一意語数が `MAX_QUERY_TERMS`
    // （1024）を超えるケースを固定する（相異なる CJK 文字を `MAX_QUERY_TERMS + 1`
    // 個並べる。バイト長は `MAX_QUERY_BYTES`〔16 KiB〕を十分下回るため、この
    // ケースはバイト長超過ではなく一意語数超過のみで拒否されることも確認する）。
    // `CountingLlmClient` の呼び出し回数が 0 のままであることを直接確認することで、
    // 拒否が `plan_query`（辞書スナップショット構築・LLM 呼び出し）より前で
    // 完結することを固定する。
    let path = unique_db_path("sql-using-plan-raw-question-too-many-terms");
    let _guard = CleanupGuard(path.clone());
    let storage = seeded_storage(&path);

    // U+4E00 (一) から連番で相異なる CJK 文字を並べる。区切り文字を挟まないため
    // 隣接文字同士の bigram も生成されるが、unigram だけで既に上限を超える。
    // `sparse::MAX_QUERY_TERMS`（1024・`crates/engine/src/sparse.rs`）は
    // クレート内部定数で本テスト（統合テストクレート）からは参照できないため、
    // 上限超過を確実にする件数として `MAX_QUESTION_CHARS`（2000）以下かつ
    // `MAX_QUERY_TERMS + 1` 以上である 1025 文字を直接指定する。
    const RAW_QUESTION_TERM_COUNT: usize = 1025;
    let question: String = (0..RAW_QUESTION_TERM_COUNT)
        .map(|i| char::from_u32(0x4E00 + i as u32).expect("valid CJK code point"))
        .collect();
    assert!(
        question.chars().count() <= engine::query_planner::MAX_QUESTION_CHARS,
        "fixture question must stay within MAX_QUESTION_CHARS so sanitize_question does not truncate it"
    );
    assert!(
        question.len() <= 16 * 1024,
        "fixture question must stay within MAX_QUERY_BYTES so the rejection is term-count-driven"
    );

    let planner = std::sync::Arc::new(CountingLlmClient::new(EXPANSION_RESPONSE));
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(RecordingEmbedder::new(DIM)))
        .with_query_planner(Box::new(ArcLlmClient(planner.clone())));

    let err = core
        .execute_sql(
            &ctx("tenant-a"),
            &format!("SELECT id FROM docs USING PLAN('{question}') LIMIT 10"),
        )
        .expect_err("raw question exceeding sparse query term limits must be rejected");
    assert_eq!(err.wire_code(), "22000");
    assert_eq!(
        planner.call_count(),
        0,
        "raw question exceeding sparse bounds must be rejected before the high-cost query planner call"
    );
}
