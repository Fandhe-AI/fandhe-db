//! NoSQL（HTTP）`filter` の同じ列への等価 `or` が SQL 表層の `OR` と同じ
//! `IN` 経路（`scalar_plan: index_in_list`）へ載ること、対象外の形は従来どおり
//! `plain_scan` に残ること、他テナントの行が現れないことを固定する層 A 結合テスト
//! （Issue #1306・NOSQL-12・NOSQL-14・SQL-24・TASK-208 ポインタ。SQL 側の書き換えは
//! Issue #1305。オラクルは同じ `EngineCore` の SQL `EXPLAIN` 行）。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::row_codec::Value;
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

fn assert_matches_sql(or_filter: &str, sql_where: &str, expected_plan: &str) {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));
    let body = format!(
        r#"{{"op":"search","table":"docs","vector":[0.1,0.2,0.3,0.4],"limit":5,"filter":[{or_filter}],"explain":true}}"#
    );
    let resp = query(addr, &token, body.as_bytes());
    let got = parse_explain_lines(&resp);
    let expected = sql_explain_lines(
        &core,
        &ctx_for("tenant-a"),
        &format!(
            "EXPLAIN SELECT id FROM docs WHERE {sql_where} ORDER BY embedding <=> '[0.1,0.2,0.3,0.4]' LIMIT 5"
        ),
    );
    assert_eq!(got, expected, "resp={resp:?}");
    assert!(
        got.contains(&format!("scalar_plan: {expected_plan}")),
        "unexpected plan: {got:?}"
    );
}

#[test]
fn same_column_eq_or_uses_index_in_list_like_sql() {
    assert_matches_sql(
        r#"{"or":[{"column":"lang","op":"eq","value":"ja"},{"column":"lang","op":"eq","value":"en"}]}"#,
        "lang = 'ja' OR lang = 'en'",
        "index_in_list",
    );
}

#[test]
fn same_column_eq_and_in_or_uses_index_in_list_like_sql() {
    assert_matches_sql(
        r#"{"or":[{"column":"lang","op":"eq","value":"ja"},{"column":"lang","op":"in","value":["en","fr"]}]}"#,
        "lang = 'ja' OR lang IN ('en','fr')",
        "index_in_list",
    );
}

#[test]
fn different_column_or_stays_plain_scan_like_sql() {
    assert_matches_sql(
        r#"{"or":[{"column":"lang","op":"eq","value":"ja"},{"column":"path","op":"eq","value":"docs/a.md"}]}"#,
        "lang = 'ja' OR path = 'docs/a.md'",
        "plain_scan",
    );
}

#[test]
fn nested_or_stays_plain_scan_like_sql() {
    assert_matches_sql(
        r#"{"or":[{"column":"lang","op":"eq","value":"ja"},{"or":[{"column":"lang","op":"eq","value":"en"},{"column":"lang","op":"eq","value":"fr"}]}]}"#,
        "lang = 'ja' OR (lang = 'en' OR lang = 'fr')",
        "plain_scan",
    );
}

/// 畳んだ経路でも tenant-b の Private 行（`lang = 'xx'`）が結果へ現れない。
#[test]
fn folded_or_search_does_not_return_other_tenant_rows() {
    let (core, _guard) = new_core();
    let (addr, token) = spawn_alice_session(Arc::clone(&core));
    let body = br#"{"op":"search","table":"docs","vector":[1.0,0.0,0.0,0.0],"limit":5,
        "filter":[{"or":[{"column":"lang","op":"eq","value":"ja"},{"column":"lang","op":"eq","value":"xx"}]}]}"#;
    let resp = query(addr, &token, body);
    assert_eq!(resp.status, 200, "resp={resp:?}");
    let raw = String::from_utf8_lossy(&resp.body);
    assert!(!raw.contains("b-secret"), "other tenant leaked: {raw}");
    assert!(!raw.contains("tenant-b only"), "other tenant leaked: {raw}");
}
