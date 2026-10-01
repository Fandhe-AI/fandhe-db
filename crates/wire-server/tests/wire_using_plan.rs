//! `USING PLAN('<query>')`（TASK-77・SQL-5）を wire v3 の簡易クエリプロトコル
//! 経由・生バイトクライアントで実行できることを検証する結合テスト（TASK-117・
//! PLAN-9 確定化の層 A。ポインタ: `docs/spec/05-tasks.md` TASK-117、
//! `docs/spec/04-behavior/query-planning.md` PLAN-9）。
//!
//! `USING PLAN` のディスパッチ規則（許可リスト・辞書必須列・世代照合等）自体は
//! `crates/engine/tests/sql_using_plan.rs`（in-process）が既に確定オラクルとして
//! 検証済みのため、本ファイルは同じ規則を **wire フレーミング** 越しに
//! 再確認することに徹する（`wire_search_mode.rs` と同方針）。決定的スタブ
//! `LlmClient`・決定的 `Embedder` を [`common::spawn_server_with_engine`] で
//! in-process 注入し、実 Ollama・実埋め込みサービスへの疎通は行わない
//! （TASK-110 と同じくスコープ外）。
//!
//! `wire-server` バイナリ自体への `--planner-endpoint`／`--planner-model`／
//! `--embedder-hashing-dim` CLI 注入（TASK-117）は本ファイルの対象外
//! （`main.rs` の引数パース単体テストが担う）。子プロセス起動・実クライアント
//! （psql／psycopg／pg）3 種での検証は `tests/three_client_e2e.rs`（`#[ignore]`）
//! が担う。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::embedding::{EmbedError, Embedder};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::*;

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

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn insert_row(
    storage: &Storage,
    tenant: &str,
    id: u64,
    visibility: Visibility,
    embedding: Vec<f32>,
    path: &str,
    body: &str,
) {
    let op_id = OperationId::parse(&format!("wire-using-plan-op-{tenant}-{id}"))
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        storage,
        TABLE,
        &ctx(tenant),
        id,
        visibility,
        &[
            Value::Vector(embedding),
            Value::Text(path.to_string()),
            Value::Text(body.to_string()),
        ],
        &op_id,
    )
    .expect("insert row");
}

/// テキスト長だけを成分へ埋め込む決定的・ネットワーク不要な埋め込み
/// （`crates/engine/tests/sql_using_plan.rs::RecordingEmbedder` と同じ方針。
/// 本ファイルでは記録機能は不要なため最小構成にする）。
struct DeterministicEmbedder {
    dim: u32,
}

impl Embedder for DeterministicEmbedder {
    fn dim(&self) -> u32 {
        self.dim
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts
            .iter()
            .map(|t| vec![t.len() as f32 * 0.01; self.dim as usize])
            .collect())
    }
}

/// 固定の展開結果（`search_terms`）を返すスタブ `LlmClient`（実 Ollama 疎通は
/// TASK-110 と同じくスコープ外。`sql_using_plan.rs::StubLlmClient` と同方針）。
struct StubLlmClient {
    response: &'static str,
}

impl LlmClient for StubLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(self.response.to_string())
    }
}

/// 不正な JSON を返すスタブ（fail-closed 経路の検証用。`docs/spec` の期待値・
/// 実 LLM 応答の再現ではなく、`query_planner::parse_expansion` の既存 fail-closed
/// 契約を wire 越しに再確認するためだけの入力）。
struct MalformedJsonLlmClient;

impl LlmClient for MalformedJsonLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok("not valid json".to_string())
    }
}

const EXPANSION_RESPONSE: &str =
    r#"{"search_terms": ["alpha", "beta"], "path_hint": null, "kind_hint": null}"#;

fn open_storage_with_table() -> (Storage, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-using-plan");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    (storage, guard)
}

/// `tenant-a` の `Public` 行 2 件（`alpha`・`beta` 語彙）を投入した storage を返す。
fn seeded_storage() -> (Storage, temp_db::CleanupGuard) {
    let (storage, guard) = open_storage_with_table();
    insert_row(
        &storage,
        "tenant-a",
        1,
        Visibility::Public,
        vec![0.1, 0.2, 0.3, 0.4],
        "docs/a.md",
        "alpha content in english",
    );
    insert_row(
        &storage,
        "tenant-a",
        2,
        Visibility::Public,
        vec![0.4, 0.3, 0.2, 0.1],
        "docs/b.md",
        "beta content in english",
    );
    (storage, guard)
}

fn spawn_with_alice(core: Arc<EngineCore>) -> (std::net::TcpStream, common::UserStoreFile) {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    (stream, users_path)
}

const USING_PLAN_SELECT: &str = "SELECT id FROM docs USING PLAN('find content') LIMIT 10";

/// TASK-117・PLAN-9: `USING PLAN` が wire 経由（生バイトクライアント）で
/// `RowDescription`→`DataRow`→`CommandComplete("SELECT <n>")`→`ReadyForQuery`
/// を返し、展開後の語彙に対応する行（`crates/engine/tests/sql_using_plan.rs::
/// using_plan_dispatch_reaches_hybrid_execution_and_returns_seeded_rows` と同じ
/// コーパス・期待値）へ到達することを確認する。
#[test]
fn using_plan_wire_dispatch_returns_expected_rows() {
    let (storage, _guard) = seeded_storage();
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(DeterministicEmbedder { dim: DIM }))
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));
    let (mut stream, _users_path) = spawn_with_alice(Arc::new(core));

    send_simple_query(&mut stream, USING_PLAN_SELECT);
    let _columns = read_row_description(&mut stream);
    let mut ids: Vec<String> = Vec::new();
    loop {
        // `RowDescription` の直後に来るメッセージが `CommandComplete`（'C'）か
        // `DataRow`（'D'）かは行数に依存するため、`read_command_complete` を
        // 先に試さず、まず 1 バイト先読みして分岐する（`peek` は使わず、代わりに
        // 既知の期待行数〔2 行〕で単純にループする）。
        if ids.len() == 2 {
            break;
        }
        ids.push(read_data_row(&mut stream)[0].clone().expect("id"));
    }
    ids.sort_unstable();
    assert_eq!(ids, vec!["1", "2"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 2");
    read_ready_for_query(&mut stream);
}

/// TASK-117・PLAN-9・RLS 不変（wire 経由の代表 1 ケース。網羅は
/// `crates/engine/tests/rls_generalized.rs::
/// using_plan_dispatch_implicitly_applies_rls` が in-process で担う）:
/// 別テナント（`tenant-b`）の `Private` 行が、`tenant-a` として認証した接続の
/// `USING PLAN` 結果に混入しない（wire 認証経路の `PolicyContext` は `Public`
/// のみ許可可視性とする既定。モジュール冒頭コメント・`crate::simple_query`
/// モジュールコメント参照。`Public` は全テナントに共有される可視性であり
/// テナント境界の対照にならないため、境界を突く行は意図的に `Private` にする）。
#[test]
fn using_plan_wire_dispatch_does_not_leak_other_tenant_rows() {
    let (storage, _guard) = seeded_storage();
    insert_row(
        &storage,
        "tenant-b",
        99,
        Visibility::Private,
        vec![0.2, 0.2, 0.2, 0.2],
        "docs/other-tenant.md",
        "alpha content belonging to another tenant",
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(DeterministicEmbedder { dim: DIM }))
        .with_query_planner(Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }));
    let (mut stream, _users_path) = spawn_with_alice(Arc::new(core));

    send_simple_query(&mut stream, USING_PLAN_SELECT);
    let _columns = read_row_description(&mut stream);
    let mut ids: Vec<String> = Vec::new();
    for _ in 0..2 {
        ids.push(read_data_row(&mut stream)[0].clone().expect("id"));
    }
    ids.sort_unstable();
    assert_eq!(
        ids,
        vec!["1", "2"],
        "tenant-b's row (id=99) must not appear in tenant-a's USING PLAN result"
    );
    assert_eq!(read_command_complete(&mut stream), "SELECT 2");
    read_ready_for_query(&mut stream);
}

/// TASK-117・PLAN-9・fail-closed: `embedder` のみ注入し `query_planner` を
/// 未注入のままにすると、`CoreError::QueryPlannerUnavailable` が
/// `SqlSurfaceError::Internal` 経由で `XX000`・固定の一般化メッセージ
/// （`"internal error"`。`SqlSurfaceError::client_message` の既存契約。
/// `crates/engine/src/sql/allowlist.rs` 参照）として wire 応答され、
/// 接続は `ReadyForQuery` で継続することを確認する。
#[test]
fn using_plan_wire_dispatch_rejects_when_query_planner_unconfigured() {
    let (storage, _guard) = seeded_storage();
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(DeterministicEmbedder { dim: DIM }));
    let (mut stream, _users_path) = spawn_with_alice(Arc::new(core));

    send_simple_query(&mut stream, USING_PLAN_SELECT);
    expect_error_response_with_sqlstate_and_message(&mut stream, "XX000", "internal error");
    read_ready_for_query(&mut stream);

    // 接続が生きていること自体を、以降の平易な SELECT（`USING PLAN` を含まない
    // ベクトル検索。リテラルベクトルを直接使うため embedder/query_planner
    // 未設定でも成功する）が成功することで確認する。
    send_simple_query(
        &mut stream,
        "SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 10",
    );
    let _columns = read_row_description(&mut stream);
    let mut ids: Vec<String> = Vec::new();
    for _ in 0..2 {
        ids.push(read_data_row(&mut stream)[0].clone().expect("id"));
    }
    ids.sort_unstable();
    assert_eq!(ids, vec!["1", "2"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 2");
    read_ready_for_query(&mut stream);
}

/// TASK-117・PLAN-9・fail-closed: `query_planner` のみ注入し `embedder` を
/// 未注入のままにすると、`CoreError::EmbedderUnavailable` が同様に `XX000`
/// （固定の一般化メッセージ）として拒否され、接続は継続する。
#[test]
fn using_plan_wire_dispatch_rejects_when_embedder_unconfigured() {
    let (storage, _guard) = seeded_storage();
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }),
    );
    let (mut stream, _users_path) = spawn_with_alice(Arc::new(core));

    send_simple_query(&mut stream, USING_PLAN_SELECT);
    expect_error_response_with_sqlstate_and_message(&mut stream, "XX000", "internal error");
    read_ready_for_query(&mut stream);
}

/// Issue #315 ポインタ:「`USING PLAN` が実 Ollama 経由で SQL エラーなしに
/// 0 行を返す」観測を wire フレーミング越しに再確認する。プランナーが
/// `mode_hint: "precision"` を推定すると確信度ゲート（SEARCH-9）により
/// `RowDescription`→`CommandComplete("SELECT 0")`→`ReadyForQuery`（エラー
/// 応答なし・接続継続）となり、続けて同一クエリへ `USING MODE 'recall'` を
/// 付けるとゲートを経由せず seed 2 行が返る（`crates/engine/tests/
/// sql_using_plan.rs::using_plan_respects_using_mode_precision` の
/// in-process オラクルと同じ決定的フィクスチャ。判断根拠は
/// `docs/design/using-plan-precision-empty-result.md` 参照）。
#[test]
fn using_plan_wire_precision_hint_returns_zero_rows_then_recall_override_returns_rows() {
    let (storage, _guard) = seeded_storage();
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(DeterministicEmbedder { dim: DIM }))
        .with_query_planner(Box::new(StubLlmClient {
            response: r#"{"search_terms": ["alpha", "beta"], "path_hint": null, "kind_hint": null, "mode": "precision"}"#,
        }));
    let (mut stream, _users_path) = spawn_with_alice(Arc::new(core));

    // プランナー推定 `precision`（明示 `USING MODE`/`SET search_mode` なし）:
    // エラー応答を経由せず `RowDescription` の直後に `CommandComplete("SELECT 0")`
    // が届く（`DataRow` は 0 件のため待たない）。
    send_simple_query(&mut stream, USING_PLAN_SELECT);
    let _columns = read_row_description(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 0");
    read_ready_for_query(&mut stream);

    // 同一接続・同一クエリへ明示 `USING MODE 'recall'` を付けると確信度ゲートを
    // 経由せず seed 2 行が返る（positive control。空集合が embedder/query_planner
    // の設定不備やテナント境界の取りこぼしではないことを示す）。
    send_simple_query(
        &mut stream,
        "SELECT id FROM docs USING PLAN('find content') LIMIT 10 USING MODE 'recall'",
    );
    let _columns = read_row_description(&mut stream);
    let mut ids: Vec<String> = Vec::new();
    for _ in 0..2 {
        ids.push(read_data_row(&mut stream)[0].clone().expect("id"));
    }
    ids.sort_unstable();
    assert_eq!(ids, vec!["1", "2"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 2");
    read_ready_for_query(&mut stream);
}

/// TASK-117・PLAN-9・fail-closed: 展開クライアントが不正な JSON を返した場合も
/// `query_planner::parse_expansion` の既存 fail-closed 契約により `XX000`
/// （固定の一般化メッセージ）で拒否される。実 LLM の応答内容は untrusted 入力
/// として扱われ、SQL 文字列へそのまま連結されない契約（`coding-rust.md`
/// 「untrusted 入力の扱い」）を wire 越しに再確認する。
#[test]
fn using_plan_wire_dispatch_rejects_when_llm_response_is_malformed_json() {
    let (storage, _guard) = seeded_storage();
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(DeterministicEmbedder { dim: DIM }))
        .with_query_planner(Box::new(MalformedJsonLlmClient));
    let (mut stream, _users_path) = spawn_with_alice(Arc::new(core));

    send_simple_query(&mut stream, USING_PLAN_SELECT);
    expect_error_response_with_sqlstate_and_message(&mut stream, "XX000", "internal error");
    read_ready_for_query(&mut stream);
}
