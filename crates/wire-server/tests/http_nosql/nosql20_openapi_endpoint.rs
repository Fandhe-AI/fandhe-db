//! `GET /v1/openapi.json` が静的な OpenAPI 文書を Bearer 認証の後にだけ返すことを、production
//! ルータ（生バイトクライアント）経由で固定する層 A 結合テスト（Issue #2244。対象ビヘイビア:
//! NOSQL-20 (3)・HTTP-5・HTTP-6。ポインタ: `docs/spec/04-behavior/nosql-surface.md`・
//! `docs/spec/04-behavior/http-transport.md`）。
//!
//! 固定する契約: 本文は埋め込み文書・ゴールデンとバイト同一／主体・テナントによらず同一で
//! テナントのデータを含まない／認証失敗は `28000`／本文あり・他メソッド・他パスは認証より前に
//! `08P01`（セッションを消費しない）／スコープ付きセッションでも取得できる。

use crate::common;
use crate::http_common;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::http::openapi_endpoint::OPENAPI_DOCUMENT;
use wire_server::http::session::store::SessionStore;

use http_common::temp_db;
use http_common::{AfterWrite, HttpResponse};

fn raw(addr: SocketAddr, request: &[u8]) -> HttpResponse {
    http_common::parse_single_response(&http_common::send_raw(addr, request, AfterWrite::HalfClose))
}

/// 本文を持たない `GET <target>`（`Content-Length`・`Content-Type` は付けない）。
fn get_bytes(method: &str, target: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = format!("{method} {target} HTTP/1.1\r\nHost: x\r\n").into_bytes();
    for (k, v) in headers {
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

fn get_openapi(addr: SocketAddr, token: Option<&str>) -> HttpResponse {
    let auth = token.map(|t| format!("Bearer {t}"));
    let mut headers: Vec<(&str, &str)> = Vec::new();
    if let Some(a) = auth.as_deref() {
        headers.push(("Authorization", a));
    }
    raw(addr, &get_bytes("GET", "/v1/openapi.json", &headers, b""))
}

fn post(addr: SocketAddr, target: &str, token: Option<&str>, body: &str) -> HttpResponse {
    let len = body.len().to_string();
    let auth = token.map(|t| format!("Bearer {t}"));
    let mut headers = vec![
        ("Content-Type", "application/json"),
        ("Content-Length", len.as_str()),
    ];
    if let Some(a) = auth.as_deref() {
        headers.push(("Authorization", a));
    }
    raw(
        addr,
        &http_common::build_request(target, &headers, body.as_bytes()),
    )
}

fn login_with(addr: SocketAddr, user: &str, extra: &str) -> String {
    let body = format!(r#"{{"user":"{user}","password":"pw-{user}"{extra}}}"#);
    let resp = post(addr, "/v1/session", None, &body);
    assert_eq!(resp.status, 200, "login must succeed: {resp:?}");
    match parse_json(&String::from_utf8_lossy(&resp.body)).expect("login json") {
        JsonValue::Object(mut o) => match o.remove("token") {
            Some(JsonValue::String(s)) => s,
            other => panic!("expected token, got {other:?}"),
        },
        other => panic!("expected object, got {other:?}"),
    }
}

fn login(addr: SocketAddr, user: &str) -> String {
    login_with(addr, user, "")
}

fn spawn_default() -> SocketAddr {
    let users = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    http_common::spawn_router_listener(&users, SessionStore::new())
}

fn assert_unauthenticated(resp: &HttpResponse) {
    http_common::assert_rejected(resp, 401, "28000");
    assert_eq!(resp.header("www-authenticate"), Some("Bearer"));
    let text = String::from_utf8_lossy(&resp.body);
    assert!(
        !text.contains("\"openapi\""),
        "document must not leak: {text}"
    );
}

/// `Date` を除いた比較用の表現。
fn without_date(resp: &HttpResponse) -> String {
    format!(
        "{} {:?} {:?}",
        resp.status,
        resp.headers
            .iter()
            .filter(|(n, _)| !n.eq_ignore_ascii_case("date"))
            .collect::<Vec<_>>(),
        resp.body
    )
}

#[test]
fn nosql20_openapi_endpoint_serves_embedded_document() {
    let addr = spawn_default();
    let token = login(addr, "alice");
    let resp = get_openapi(addr, Some(&token));
    assert_eq!(resp.status, 200, "{resp:?}");
    assert_eq!(resp.body, OPENAPI_DOCUMENT.as_bytes());
    let golden = std::fs::read(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/openapi.json"),
    )
    .expect("golden readable");
    assert_eq!(resp.body, golden);
    assert_eq!(
        resp.header("content-type"),
        Some("application/json; charset=utf-8")
    );
    assert_eq!(
        resp.header("content-length").map(str::to_owned),
        Some(resp.body.len().to_string())
    );
    assert_eq!(resp.header("cache-control"), Some("no-store"));
    let JsonValue::Object(doc) = parse_json(&String::from_utf8_lossy(&resp.body)).expect("json")
    else {
        panic!("root must be an object");
    };
    match doc.get("openapi") {
        Some(JsonValue::String(s)) => assert!(s.starts_with("3.1."), "got {s}"),
        other => panic!("openapi member missing: {other:?}"),
    }
    assert!(doc.contains_key("info") && doc.contains_key("paths"));
}

#[test]
fn nosql20_openapi_endpoint_accepts_explicit_zero_content_length() {
    let addr = spawn_default();
    let token = login(addr, "alice");
    let auth = format!("Bearer {token}");
    let req = get_bytes(
        "GET",
        "/v1/openapi.json",
        &[("Authorization", &auth), ("Content-Length", "0")],
        b"",
    );
    let resp = raw(addr, &req);
    assert_eq!(resp.status, 200, "{resp:?}");
    assert_eq!(resp.body, OPENAPI_DOCUMENT.as_bytes());
}

#[test]
fn nosql20_openapi_endpoint_rejects_unauthenticated_with_28000() {
    let users = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);

    // 欠落・不正形式・未知トークン
    let addr = http_common::spawn_router_listener(&users, SessionStore::new());
    assert_unauthenticated(&get_openapi(addr, None));
    let malformed = raw(
        addr,
        &get_bytes(
            "GET",
            "/v1/openapi.json",
            &[("Authorization", "Basic abc")],
            b"",
        ),
    );
    assert_unauthenticated(&malformed);
    assert_unauthenticated(&get_openapi(
        addr,
        Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
    ));

    // close 済みトークン
    let token = login(addr, "alice");
    let closed = post(addr, "/v1/session/close", Some(&token), "");
    assert_eq!(closed.status, 200);
    assert_unauthenticated(&get_openapi(addr, Some(&token)));

    // 期限切れトークン（TTL=0 は発行直後から期限切れ）
    let expired_addr =
        http_common::spawn_router_listener(&users, SessionStore::with_limits(4, Duration::ZERO));
    let expired = login(expired_addr, "alice");
    assert_unauthenticated(&get_openapi(expired_addr, Some(&expired)));
}

#[test]
fn nosql20_openapi_endpoint_is_identical_across_tenants() {
    let addr = spawn_default();
    let a = get_openapi(addr, Some(&login(addr, "alice")));
    let b = get_openapi(addr, Some(&login(addr, "bob")));
    assert_eq!(a.status, 200);
    assert_eq!(without_date(&a), without_date(&b));
}

#[test]
fn nosql20_openapi_endpoint_does_not_leak_tenant_data() {
    let path = temp_db::unique_db_path("nosql20-openapi-endpoint");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "secret_tbl_2244",
            vec![ColumnDef::new("secret_col_2244", ColumnType::Text, true)],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener_with_engine(&users, SessionStore::new(), core);
    let resp = get_openapi(addr, Some(&login(addr, "alice")));
    assert_eq!(resp.status, 200);
    let text = String::from_utf8_lossy(&resp.body);
    assert!(!text.contains("secret_tbl_2244") && !text.contains("secret_col_2244"));
}

#[test]
fn nosql20_openapi_endpoint_rejects_framing_violations_before_auth() {
    let addr = spawn_default();
    let token = login(addr, "alice");
    let auth = format!("Bearer {token}");

    // (f) 本文あり GET（有効トークン付きでも 08P01）
    let with_body = raw(
        addr,
        &get_bytes(
            "GET",
            "/v1/openapi.json",
            &[("Authorization", &auth), ("Content-Length", "3")],
            b"abc",
        ),
    );
    http_common::assert_rejected(&with_body, 400, "08P01");
    // 本文あり GET（トークンなしでも 28000 ではなく 08P01）
    let anon_body = raw(
        addr,
        &get_bytes(
            "GET",
            "/v1/openapi.json",
            &[("Content-Length", "3")],
            b"abc",
        ),
    );
    http_common::assert_rejected(&anon_body, 400, "08P01");

    // (h) 他パス・正規化違い・他メソッド（Bearer なしでも 08P01）
    for (method, target) in [
        ("GET", "/v1/openapi.json?x=1"),
        ("GET", "/v1/openapi.json/"),
        ("GET", "/V1/OPENAPI.JSON"),
        ("GET", "/v1/query"),
        ("GET", "/"),
        ("HEAD", "/v1/openapi.json"),
        ("PUT", "/v1/openapi.json"),
    ] {
        let anon = raw(addr, &get_bytes(method, target, &[], b""));
        http_common::assert_rejected(&anon, 400, "08P01");
        let authed = raw(
            addr,
            &get_bytes(method, target, &[("Authorization", &auth)], b""),
        );
        http_common::assert_rejected(&authed, 400, "08P01");
    }

    // (g) POST /v1/openapi.json は未知パス（認証しない 08P01）
    let post_authed = post(addr, "/v1/openapi.json", Some(&token), r#"{"op":"scan"}"#);
    http_common::assert_reached_router(&post_authed);
    let post_anon = post(addr, "/v1/openapi.json", None, "");
    http_common::assert_reached_router(&post_anon);

    // (i) 上記はいずれもセッションを消費せず、同じトークンが引き続き有効
    let gate = post(
        addr,
        "/v1/query",
        Some(&token),
        r#"{"op":"scan","table":"docs","limit":1}"#,
    );
    http_common::assert_reached_query_gate(&gate);
    assert_eq!(get_openapi(addr, Some(&token)).status, 200);
}

#[test]
fn nosql20_openapi_endpoint_is_available_to_scoped_sessions() {
    let addr = spawn_default();
    let token = login_with(addr, "alice", r#","scope":{"ops":["scan"]}"#);
    let resp = get_openapi(addr, Some(&token));
    assert_eq!(resp.status, 200, "{resp:?}");
    assert_eq!(resp.body, OPENAPI_DOCUMENT.as_bytes());
}
