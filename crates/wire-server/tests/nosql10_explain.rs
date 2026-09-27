//! `POST /v1/query`（`op: "search"`・`explain: true`）の写像・実行を
//! production ルータ経由（生バイトクライアント）で検証する層 A 結合テスト
//! （Issue #765・TASK-186・対象ビヘイビア NOSQL-10。ポインタ:
//! `docs/spec/05-tasks.md` TASK-186・`docs/spec/04-behavior/nosql-surface.md`
//! NOSQL-10・`docs/spec/04-behavior/sql-surface.md` SQL-6）。
//!
//! 確定オラクルは同じ `Arc<EngineCore>` に対する `execute_sql_in_session`
//! （SQL テキスト経由の `EXPLAIN SELECT ... USING PLAN(...)`）の
//! `QUERY PLAN` 行であり、本ファイルは wire フレーミング・認証・op 許可
//! リスト・スキーマ検証込みで同じ内容へ到達することを確認する
//! （`nosql3_scan_mapping.rs`・`nosql4_aggregate.rs` と同じ流儀）。
//! `crates/engine/tests/core_explain_plan_entry.rs`（in-process。Issue #765）
//! が固定する `EngineCore::explain_bound_plan_in_session` 自体の契約は対象外。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::sync::{Arc, Mutex};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::row_codec::Value;
use engine::search_engine;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

/// 決定的スタブ `LlmClient`（実 Ollama への疎通は対象外。
/// `crates/engine/tests/sql_explain.rs::StubLlmClient` と同型）。
struct StubLlmClient {
    response: &'static str,
}

impl LlmClient for StubLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(self.response.to_string())
    }
}

/// プロンプト（辞書スナップショットを含む展開入力）を記録する決定的スタブ
/// （`crates/engine/tests/core_explain_plan_entry.rs::RecordingLlmClient` と
/// 同構成）。`StubLlmClient` は固定応答のみを返しプロンプト内容を無視する
/// ため、RLS 非漏えいの確認には「応答に他テナント語彙が現れないこと」しか
/// 固定できない（codex-review 指摘 PR #828）。本スタブは wire 経由でも
/// `EngineCore` へ実際に渡されたプロンプトそのものへ他テナント語彙が
/// 混入していないことを固定するために使う。
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

const EXPANSION_RESPONSE: &str =
    r#"{"search_terms": ["alpha", "beta"], "path_hint": "docs/", "kind_hint": "fn"}"#;

/// tenant-a に `docs/a.md` の可視行を 1 件、tenant-b に `docs/b-secret.md`
/// の Private 行を 1 件投入した `EngineCore`（既定エンジン。`EngineCore::open`
/// 経由で `search_engine_kind() == Some(ParallelBruteForce)` になることを
/// `sql_insert_explain_public_api.rs::build_explain_result_is_reachable_and_matches_sql_explain_rows`
/// と同じ理由で選ぶ）。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    new_core_with_planner(Box::new(StubLlmClient {
        response: EXPANSION_RESPONSE,
    }))
}

/// [`new_core`] と同じ行データを持ち、注入する `LlmClient` を差し替えられる版。
/// RLS 非漏えいの確認でプロンプト内容そのものを記録したい呼び出し元
/// （`RecordingLlmClient`）向け。
fn new_core_with_planner(planner: Box<dyn LlmClient>) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql10-explain-default");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    let ctx_a = ctx_for("tenant-a");
    let op_id = engine::recovery::required_op_id::OperationId::parse("nosql10-op-1")
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx_a,
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

    let ctx_b = ctx_for("tenant-b");
    let op_id_b = engine::recovery::required_op_id::OperationId::parse("nosql10-op-101")
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx_b,
        101,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 0.0, 0.0, 0.0]),
            Value::Text("xx".to_string()),
            Value::Text("docs/b-secret.md".to_string()),
            Value::Text("tenant-b only content".to_string()),
        ],
        &op_id_b,
    )
    .expect("insert tenant-b row");

    drop(storage);
    let core = EngineCore::open(&path)
        .expect("open engine core")
        .with_query_planner(planner);
    (Arc::new(core), guard)
}

/// [`new_core`] と同じ行データを持つ `SearchEngineKind::Hnsw` opt-in 版。
fn new_hnsw_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql10-explain-hnsw");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx_a = ctx_for("tenant-a");
    let op_id = engine::recovery::required_op_id::OperationId::parse("nosql10-hnsw-op-1")
        .expect("valid operation_id");
    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &ctx_a,
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

    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind).with_query_planner(Box::new(
        StubLlmClient {
            response: EXPANSION_RESPONSE,
        },
    ));
    (Arc::new(core), guard)
}

fn spawn_alice_session(core: Arc<EngineCore>) -> (std::net::SocketAddr, String) {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr =
        http_common::spawn_router_listener_with_engine(&users_path, SessionStore::new(), core);
    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        login_body,
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    let token = match parse_json(&text).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    };
    (addr, token)
}

fn query(addr: std::net::SocketAddr, token: &str, body: &[u8]) -> HttpResponse {
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &format!("Bearer {token}")),
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body,
    );
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

/// 成功応答（`200`）の本文を `{"explain":[...]}` から `Vec<String>` へ解析する。
fn parse_explain_lines(resp: &HttpResponse) -> Vec<String> {
    assert_eq!(
        resp.status,
        200,
        "expected success response, got: {:?}",
        String::from_utf8_lossy(&resp.body)
    );
    let text = std::str::from_utf8(&resp.body).expect("utf-8 body");
    let JsonValue::Object(mut top) = parse_json(text).expect("valid json body") else {
        panic!("expected json object body: {text}");
    };
    let JsonValue::Array(items) = top.remove("explain").expect("missing \"explain\" key") else {
        panic!("\"explain\" must be an array: {text}");
    };
    items
        .into_iter()
        .map(|v| match v {
            JsonValue::String(s) => s,
            other => panic!("expected string element, got {other:?}"),
        })
        .collect()
}

/// SQL 表層 `EXPLAIN SELECT ... USING PLAN(...)` の行を取得する（オラクル）。
fn sql_explain_lines(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<String> {
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(ctx, &mut session, sql)
        .expect("SQL EXPLAIN should succeed");
    match outcome {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                engine::sql::exec::Cell::Text(s) => s.clone(),
                other => panic!("expected Cell::Text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected SqlOutcome::Explain, got {other:?}"),
    }
}

#[test]
fn explain_true_matches_sql_explain_rows_without_filter() {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","plan":"find content","limit":5,"explain":true}"#;
    let resp = query(addr, &token, body);
    let wire_lines = parse_explain_lines(&resp);

    let sql_lines = sql_explain_lines(
        &core,
        &ctx_for("tenant-a"),
        "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 5",
    );
    assert_eq!(wire_lines, sql_lines, "resp={resp:?}");
    assert_eq!(wire_lines.len(), 9);
    assert!(wire_lines.contains(&"engine: parallel_brute_force".to_string()));
    assert!(wire_lines.contains(&"ann_plan: plain_scan_engine".to_string()));
    assert!(wire_lines.contains(&"scalar_plan: plain_scan".to_string()));

    // 応答本文に通常の search 応答フィールド（`rows`／`columns`／
    // `row_count`）・テナント ID・行データが現れないこと。
    let raw = String::from_utf8_lossy(&resp.body);
    assert!(!raw.contains("\"rows\""));
    assert!(!raw.contains("\"row_count\""));
    assert!(!raw.contains("tenant-a"));
    assert!(!raw.contains("docs/a.md"));
}

#[test]
fn explain_true_matches_sql_explain_rows_with_filter() {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","plan":"find content","limit":5,
        "filter":[{"column":"lang","op":"eq","value":"ja"}],"explain":true}"#;
    let resp = query(addr, &token, body);
    let wire_lines = parse_explain_lines(&resp);

    let sql_lines = sql_explain_lines(
        &core,
        &ctx_for("tenant-a"),
        "EXPLAIN SELECT id FROM docs WHERE lang = 'ja' USING PLAN('find content') LIMIT 5",
    );
    assert_eq!(wire_lines, sql_lines, "resp={resp:?}");
    assert!(wire_lines.contains(&"scalar_plan: index_equality".to_string()));
}

#[test]
fn explain_true_matches_sql_explain_rows_with_mode() {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","plan":"find content","limit":5,
        "mode":"precision","explain":true}"#;
    let resp = query(addr, &token, body);
    let wire_lines = parse_explain_lines(&resp);

    let sql_lines = sql_explain_lines(
        &core,
        &ctx_for("tenant-a"),
        "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 5 USING MODE 'precision'",
    );
    assert_eq!(wire_lines, sql_lines, "resp={resp:?}");
    assert!(wire_lines.contains(&"mode: precision".to_string()));
    assert!(wire_lines.contains(&"mode_source: query_clause".to_string()));
}

/// 未知テーブル＋`mode` 値不正は `explain: true` 経路でも `42P01` が
/// `22000` より優先される（Cursor Bugbot 指摘対応・PR #828 レビュー。
/// `explain_bound_plan_in_session` がテーブル解決より先に `mode` リテラルを
/// 解析すると、テーブル未存在＋ mode 値不正の要求で `42P01` より先に
/// `22000` が確定してしまう回帰。通常の `search`（`nosql2_search.rs::
/// undefined_table_rejects_with_42p01_before_invalid_mode_on_plan_path`）
/// と同じ優先順位を `explain: true` 経路でも保証する）。
#[test]
fn undefined_table_rejects_with_42p01_before_invalid_mode_with_explain_true() {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let body = br#"{"op":"search","table":"missing","plan":"find content","limit":1,
        "mode":"fuzzy","explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(http_common::wire_code_of(&resp), "42P01", "resp={resp:?}");
}

/// 未知テーブル＋`mode` の**識別子形状**不正（先頭が ASCII 英字／`_` 以外）
/// は `explain: true` 経路でも `42P01` が形状違反の `42601` より優先される
/// （codex-review P1 指摘・PR #828。上の
/// `undefined_table_rejects_with_42p01_before_invalid_mode_with_explain_true`
/// が使う `"fuzzy"` は識別子として正しい形状のため語彙エラー（`SearchMode::
/// parse_literal` 由来の `22000`）の優先順位しか固定できていなかった。本
/// テストは `ident::check_identifier` 自身が拒否する形状（数字始まり）を
/// 使い、`explain::execute` がテーブル解決より前に `mode` の識別子形状検査
/// を行っていないことを固定する）。
#[test]
fn undefined_table_rejects_with_42p01_before_malformed_mode_shape_with_explain_true() {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let body = br#"{"op":"search","table":"missing","plan":"find content","limit":1,
        "mode":"1bad","explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(http_common::wire_code_of(&resp), "42P01", "resp={resp:?}");
}

/// 未知テーブル＋`plan` 欠落は `explain: true` 経路でも `42P01` が `plan`
/// 欠落の `42601` より優先される（codex-review P1 指摘・PR #828。
/// `explain::execute` が `plan` 欠落判定をテーブル解決前に行うと、未知
/// テーブル＋ `plan` 欠落の要求で `42P01` より先に `42601` が確定して
/// しまう回帰。`super::search::execute` が `vector`／`plan` 両方欠落の
/// 判定をテーブル解決後の `bind_search` へ委ねるのと同じ優先順位を
/// `explain: true` 経路でも保証する）。
#[test]
fn undefined_table_rejects_with_42p01_before_missing_plan_with_explain_true() {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let body = br#"{"op":"search","table":"missing","limit":1,"explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(http_common::wire_code_of(&resp), "42P01", "resp={resp:?}");
}

/// 未知テーブル＋`limit` 範囲外は `explain: true` 経路でも `limit` の
/// 範囲検証（`22000`）がテーブル解決（`42P01`）より優先される（codex-review
/// P1 指摘・PR #828。対応する SQL `EXPLAIN`〔`core.rs` の `Statement::
/// Explain` アーム〕が `run_explain_plan` 呼び出し前〔テーブル解決前〕に
/// `validate_search_limit` を呼ぶのと同一の優先順位を、通常の `plan` 検索
/// 〔`search.rs::execute`〕と同様に `explain: true` 経路でも保証する）。
#[test]
fn limit_out_of_range_rejects_with_22000_before_undefined_table_with_explain_true() {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let body = br#"{"op":"search","table":"missing","plan":"find content","limit":0,
        "explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(http_common::wire_code_of(&resp), "22000", "resp={resp:?}");
}

/// 既存テーブル＋`plan` 欠落＋`mode` 値不正が同時に揃う要求では `plan`
/// 欠落（`42601`）が `mode` 値不正（`22000`）より優先される（Cursor
/// Bugbot 指摘対応・PR #828 レビュー。`core.rs::run_explain_plan` が
/// `mode_literal` を `bind`（`plan` 欠落判定を含む束縛 closure）より後で
/// 解析するよう順序を修正。`vector`／`plan` の排他判定を `mode` 解析より
/// 先に行う `search::bind_search`〔`vector` 指定検索〕と同一の優先順位）。
#[test]
fn missing_plan_rejects_with_42601_before_invalid_mode_on_existing_table_with_explain_true() {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","limit":1,"mode":"fuzzy","explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(http_common::wire_code_of(&resp), "42601", "resp={resp:?}");
}

#[test]
fn explain_true_reports_hnsw_params_and_does_not_touch_hnsw_index_cache() {
    let (core, _guard) = new_hnsw_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let stats_before = core.hnsw_index_cache_stats();
    let body = br#"{"op":"search","table":"docs","plan":"find content","limit":5,"explain":true}"#;
    let resp = query(addr, &token, body);
    let wire_lines = parse_explain_lines(&resp);
    let stats_after = core.hnsw_index_cache_stats();

    assert!(
        wire_lines.iter().any(|l| l.starts_with("hnsw_params:")),
        "wire_lines={wire_lines:?}"
    );
    assert_eq!(stats_before.hits, 0);
    assert_eq!(stats_before.builds, 0);
    assert_eq!(stats_after.hits, 0);
    assert_eq!(stats_after.builds, 0);
}

#[test]
fn vector_with_explain_true_matches_sql_explain_rows() {
    // Issue #948（NOSQL-16・SQL-27）で `vector` 指定 `search` への `explain:
    // true` を受理するよう拡大した。旧来は `42601` で拒否していた
    // （`crates/wire-server/tests/nosql16_explain_targets.rs` が新経路の
    // 網羅的な行一致・非実行・非露出を固定する。本テストは既存ファイルの
    // 回帰確認として引き続き 1 ケースだけ残す）。
    let (core, _guard) = new_core();
    let ctx_a = ctx_for("tenant-a");
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let sql = "SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 5";
    let expected = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {sql}"));

    let body = br#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,
        "explain":true}"#;
    let resp = query(addr, &token, body);
    let got = parse_explain_lines(&resp);
    assert_eq!(got, expected, "resp={resp:?}");
}

#[test]
fn vector_and_plan_both_present_with_explain_true_rejects_with_42601() {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],
        "plan":"find content","limit":5,"explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(resp.status, 400, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "42601");
}

#[test]
fn explain_true_without_planner_fails_closed_with_xx000() {
    let path = temp_db::unique_db_path("nosql10-explain-no-planner");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    drop(storage);
    // `with_query_planner` を呼ばない core（プランナー未注入）。
    let core = Arc::new(EngineCore::open(&path).expect("open engine core"));
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","plan":"find content","limit":5,"explain":true}"#;
    let resp = query(addr, &token, body);
    assert_eq!(resp.status, 500, "resp={resp:?}");
    assert_eq!(http_common::wire_code_of(&resp), "XX000");
    assert!(!parse_explain_lines_allow_error(&resp));
    drop(guard);
}

/// エラー応答が `explain` キーを持たないことの補助（`parse_explain_lines` は
/// 成功応答専用で `200` を要求するため、エラー応答用に別関数を用意する）。
fn parse_explain_lines_allow_error(resp: &HttpResponse) -> bool {
    let text = std::str::from_utf8(&resp.body).expect("utf-8 body");
    let Ok(JsonValue::Object(top)) = parse_json(text) else {
        return false;
    };
    top.contains_key("explain")
}

#[test]
fn aggregate_and_scan_explain_true_match_sql_explain_rows() {
    // Issue #948（NOSQL-16・SQL-27）で `aggregate`／`scan` op への `explain:
    // true` を受理するよう拡大した（旧来は `42601` で拒否）。網羅的な
    // カバレッジは `crates/wire-server/tests/nosql16_explain_targets.rs`
    // が持つ。本テストは既存ファイルの回帰確認として最小ケースを残す。
    let (core, _guard) = new_core();
    let ctx_a = ctx_for("tenant-a");
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let aggregate_sql = "SELECT COUNT(*) FROM docs";
    let expected_aggregate = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {aggregate_sql}"));
    let aggregate_body =
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"*"}],
            "explain":true}"#;
    let resp = query(addr, &token, aggregate_body);
    let got_aggregate = parse_explain_lines(&resp);
    assert_eq!(got_aggregate, expected_aggregate, "resp={resp:?}");

    let scan_sql = "SELECT id FROM docs LIMIT 5";
    let expected_scan = sql_explain_lines(&core, &ctx_a, &format!("EXPLAIN {scan_sql}"));
    let scan_body = br#"{"op":"scan","table":"docs","limit":5,"explain":true}"#;
    let resp = query(addr, &token, scan_body);
    let got_scan = parse_explain_lines(&resp);
    assert_eq!(got_scan, expected_scan, "resp={resp:?}");
}

#[test]
fn other_tenant_row_content_does_not_leak_into_explain_response() {
    let seen_prompts = Arc::new(Mutex::new(Vec::new()));
    let (core, _guard) = new_core_with_planner(Box::new(RecordingLlmClient {
        response: EXPANSION_RESPONSE,
        seen_prompts: Arc::clone(&seen_prompts),
    }));
    let (addr, token) = spawn_alice_session(Arc::clone(&core));

    let body = br#"{"op":"search","table":"docs","plan":"find content","limit":5,"explain":true}"#;
    let resp = query(addr, &token, body);
    let wire_lines = parse_explain_lines(&resp);
    let joined = wire_lines.join("\n");
    // 展開結果はスタブ固定値（`EXPANSION_RESPONSE`）のため、tenant-b 専用の
    // 語彙（辞書スナップショットが RLS 可視行のみから作られる既存契約の
    // 再確認）は現れない。
    assert!(!joined.contains("docs/b-secret.md"));
    assert!(!joined.contains("tenant-b"));

    // `RecordingLlmClient` は固定応答を無視せず実際に wire 経由で
    // `EngineCore` へ渡されたプロンプトを記録するため、辞書スナップショット
    // を含む展開入力そのものに他テナント語彙が混入していないことも固定する
    // （codex-review 指摘 PR #828: 固定応答スタブでは応答側しか検証できず、
    // プロンプト側の混入は見逃せる）。
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
    // が実際に wire 経由のプロンプトへ含まれていることを固定し、辞書内容
    // そのものが渡っていることを非 vacuous に確認する。
    assert!(
        prompts.iter().any(|prompt| prompt.contains("docs/a.md")),
        "自テナント（tenant-a）の辞書内容がプロンプトに含まれていない\
         （非漏えい検証が vacuous）"
    );
}
