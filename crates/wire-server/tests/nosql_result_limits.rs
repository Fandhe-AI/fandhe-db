//! `--max-result-rows`（Issue #1555。ポインタ: SQL-34・SQL-46）の設定値が NoSQL（HTTP）表層の
//! `search`／`scan` の `limit` 範囲判定へ届くことを検証する層 A 結合テスト。
//!
//! `EngineCore::with_result_limits` を注入した core を HTTP ルータへ載せ、
//!
//! - 下げた構成: 設定値ちょうどは受理・+1 は `22000`（`explain: true` も同一の分類）
//! - 上げた構成: 既定の上限（10,000）を超える `limit` を受理し、ハード上限+1 は `22000`
//! - 未注入（既定）: `limit: 10000` は受理・`10001` は `22000`（現行回帰）
//!
//! を確認する。CLI フラグの受理・拒否は `wire_result_limits_cli.rs` の担当。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;
use http_common::temp_db;

use std::net::SocketAddr;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::result_limits::ResultLimits;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};
use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn new_core(limit: Option<usize>) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("nosql-result-limits");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for id in 1..=3u64 {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text("ja".to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("nosql-rl-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let core = match limit {
        Some(n) => core.with_result_limits(ResultLimits::new(n)),
        None => core,
    };
    (Arc::new(core), guard)
}

fn spawn(core: Arc<EngineCore>) -> SocketAddr {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    http_common::spawn_router_listener_with_engine(&users_path, SessionStore::new(), core)
}

fn login(addr: SocketAddr) -> String {
    let body = r#"{"user":"alice","password":"pw-alice"}"#;
    let request = http_common::build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    );
    let resp = http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    match parse_json(&String::from_utf8_lossy(&resp.body)).expect("login body must be valid json") {
        JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    }
}

fn post(addr: SocketAddr, token: &str, body: &str) -> HttpResponse {
    let auth_header = format!("Bearer {token}");
    let content_length = body.len().to_string();
    let request = http_common::build_request(
        "/v1/query",
        &[
            ("Authorization", &auth_header),
            ("Content-Type", "application/json"),
            ("Content-Length", &content_length),
        ],
        body.as_bytes(),
    );
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

fn scan_body(limit: usize) -> String {
    format!(r#"{{"op":"scan","table":"docs","limit":{limit}}}"#)
}

fn search_body(limit: usize) -> String {
    format!(r#"{{"op":"search","table":"docs","vector":[1.0,0.0],"limit":{limit}}}"#)
}

fn explain_scan_body(limit: usize) -> String {
    format!(r#"{{"op":"scan","table":"docs","limit":{limit},"explain":true}}"#)
}

/// `limit == max` は 200、`max + 1` は `22000`（400）になることを `scan`／`search`／`explain` で確認する。
fn assert_boundary(addr: SocketAddr, max: usize) {
    let token = login(addr);
    for make in [scan_body, search_body, explain_scan_body] {
        let at = post(addr, &token, &make(max));
        assert_eq!(at.status, 200, "limit={max}: {at:?}");
        let over = post(addr, &token, &make(max + 1));
        http_common::assert_rejected(&over, 400, "22000");
    }
}

#[test]
fn default_keeps_ten_thousand() {
    let (core, _guard) = new_core(None);
    assert_boundary(spawn(core), 10_000);
}

#[test]
fn lowered_limit_applies_to_scan_search_and_explain() {
    let (core, _guard) = new_core(Some(50));
    assert_boundary(spawn(core), 50);
}

#[test]
fn raised_limit_accepts_values_above_the_legacy_ten_thousand() {
    let (core, _guard) = new_core(Some(100_000));
    assert_boundary(spawn(core), 100_000);
}
