//! `POST /v1/query` 前段の `Authorization: Bearer` 検証・`PolicyContext` の
//! サーバー側導出の結合テスト（Issue #754・TASK-174。対象ビヘイビア
//! HTTP-5・HTTP-6・HTTP-7。ポインタ: `docs/spec/05-tasks.md` TASK-174・
//! `docs/spec/04-behavior/http-transport.md` HTTP-5, HTTP-6, HTTP-7）。
//!
//! 層 A（本ファイル・`cargo test`・`make ci` 対象）: `http8_session_close.rs`
//! と同じ流儀（`http_common::spawn_router_listener` で production ルータを
//! in-process サーバースレッドとして起動し、生 HTTP/1.1 要求を送受信する
//! 自作クライアント）で、Bearer 有効時のゲート到達（暫定 `0A000`／501）・
//! Bearer 欠落／不正／未知／期限切れ／close 済みの `28000` への収束・
//! `tenant_id` 相当（JSON・ヘッダ・パス）の `42601` 拒否・セッション枠の
//! 非消費・テナント分離・非漏えいを検証する。

use crate::common;
use crate::http_common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use wire_server::http::session::store::SessionStore;

/// `POST /v1/session` へログインしてトークン（base64url 表現）を取り出す。
fn login(addr: std::net::SocketAddr, user: &str, password: &str) -> String {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);
    let response = send_request(addr, "/v1/session", None, body.as_bytes());
    let (status_line, resp_body) = split_response(&response);
    assert!(
        status_line.starts_with("HTTP/1.1 200 "),
        "login must succeed: got {status_line}"
    );
    let obj = json_object(&resp_body);
    match obj.get("token") {
        Some(engine::json::JsonValue::String(s)) => s.clone(),
        other => panic!("expected string token field, got {other:?}"),
    }
}

/// `target` へ `Content-Type: application/json`・任意の `Authorization` の
/// 要求を送り、EOF まで応答を読み切って返す（NoSQL 表層は 1 応答＝1 接続で
/// クローズする設計のため EOF が応答終端の合図になる）。
fn send_request(
    addr: std::net::SocketAddr,
    target: &str,
    auth: Option<&str>,
    body: &[u8],
) -> Vec<u8> {
    send_request_with_headers(addr, target, auth, &[], body)
}

/// [`send_request`] に追加ヘッダ（テナントヘッダ検証用）を差し込める版。
fn send_request_with_headers(
    addr: std::net::SocketAddr,
    target: &str,
    auth: Option<&str>,
    extra_headers: &[(&str, &str)],
    body: &[u8],
) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");

    let mut request = Vec::new();
    request.extend_from_slice(format!("POST {target} HTTP/1.1\r\n").as_bytes());
    request.extend_from_slice(b"Host: localhost\r\n");
    if let Some(auth) = auth {
        request.extend_from_slice(format!("Authorization: {auth}\r\n").as_bytes());
    }
    for (name, value) in extra_headers {
        request.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    request.extend_from_slice(b"Content-Type: application/json\r\n");
    request.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    request.extend_from_slice(b"\r\n");
    request.extend_from_slice(body);

    stream.write_all(&request).expect("send request");

    read_to_eof(&mut stream)
}

/// 生要求（任意のターゲット込み）を送り応答を読み切る（パス位置の
/// `tenant_id`・クエリ文字列付きターゲット検証向け）。
fn send_raw_request(addr: std::net::SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream.write_all(request).expect("send request");
    read_to_eof(&mut stream)
}

fn read_to_eof(stream: &mut TcpStream) -> Vec<u8> {
    let mut received = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(e) => panic!("unexpected read error: {e:?}"),
        }
    }
    received
}

fn split_response(bytes: &[u8]) -> (String, String) {
    let text = String::from_utf8_lossy(bytes).into_owned();
    let mut parts = text.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("").to_string();
    let body = parts.next().unwrap_or("").to_string();
    let status_line = head.lines().next().unwrap_or("").to_string();
    (status_line, body)
}

fn json_object(body: &str) -> std::collections::BTreeMap<String, engine::json::JsonValue> {
    match engine::json::parse_json(body).expect("response body must be valid JSON") {
        engine::json::JsonValue::Object(map) => map,
        other => panic!("expected JSON object, got {other:?}"),
    }
}

fn wire_code_of_body(body: &str) -> String {
    let obj = json_object(body);
    let engine::json::JsonValue::Object(error_obj) = obj
        .get("error")
        .unwrap_or_else(|| panic!("missing error field: {body:?}"))
        .clone()
    else {
        panic!("error field must be an object: {body:?}")
    };
    match error_obj.get("wire_code") {
        Some(engine::json::JsonValue::String(s)) => s.clone(),
        other => panic!("expected string wire_code, got {other:?}"),
    }
}

const VALID_SCAN_BODY: &[u8] = br#"{"op":"scan","table":"docs","limit":1}"#;

// --- (1) 有効 Bearer で暫定応答へ到達（非 vacuous 証跡） -------------------

#[test]
fn valid_bearer_and_valid_json_reaches_placeholder_response() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");

    let response = send_request(
        addr,
        "/v1/query",
        Some(&format!("Bearer {token}")),
        VALID_SCAN_BODY,
    );
    let (status_line, resp_body) = split_response(&response);
    // `scan` は TASK-186・NOSQL-3（Issue #766）で実行結線済みのため、
    // スローアウェイ `EngineCore`（テーブル未作成）上では `42P01`／404 が
    // 「認証 → op 許可リスト → スキーマ検証 → engine 呼び出し」到達の
    // 非 vacuous な証跡になる（`http_common::assert_reached_query_gate` と
    // 同じ判断）。
    assert!(
        status_line.starts_with("HTTP/1.1 404 "),
        "got: {status_line}"
    );
    assert_eq!(wire_code_of_body(&resp_body), "42P01");
}

// --- (2)〜(5) Bearer 欠落・不正・close 済みの収束 --------------------------

#[test]
fn missing_bearer_rejects_with_28000() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    let response = send_request(addr, "/v1/query", None, VALID_SCAN_BODY);
    let (status_line, resp_body) = split_response(&response);
    assert!(
        status_line.starts_with("HTTP/1.1 401 "),
        "got: {status_line}"
    );
    assert_eq!(wire_code_of_body(&resp_body), "28000");
}

#[test]
fn malformed_bearer_variants_reject_with_28000() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    for auth in [
        "Basic dXNlcjpwYXNz",
        "Bearer",
        "Bearer AAAA",
        "bearer not-a-real-token-of-correct-length-000000000",
    ] {
        let response = send_request(addr, "/v1/query", Some(auth), VALID_SCAN_BODY);
        let (status_line, resp_body) = split_response(&response);
        assert!(
            status_line.starts_with("HTTP/1.1 401 "),
            "auth {auth:?}: got {status_line}"
        );
        assert_eq!(wire_code_of_body(&resp_body), "28000");
    }
}

#[test]
fn closed_session_token_rejects_with_28000() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");

    let close_response = send_request(
        addr,
        "/v1/session/close",
        Some(&format!("Bearer {token}")),
        b"",
    );
    let (close_status, _) = split_response(&close_response);
    assert!(close_status.starts_with("HTTP/1.1 200 "));

    let response = send_request(
        addr,
        "/v1/query",
        Some(&format!("Bearer {token}")),
        VALID_SCAN_BODY,
    );
    let (status_line, resp_body) = split_response(&response);
    assert!(
        status_line.starts_with("HTTP/1.1 401 "),
        "got: {status_line}"
    );
    assert_eq!(wire_code_of_body(&resp_body), "28000");
}

// --- (5) 欠落・未知・close 済みの応答バイト同一性 ---------------------------

#[test]
fn missing_unknown_and_closed_responses_are_byte_identical_except_date() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    let missing = send_request(addr, "/v1/query", None, VALID_SCAN_BODY);

    let unknown_token = wire_server::http::session::token::SessionToken::generate()
        .expect("generate unrelated token");
    let unknown = send_request(
        addr,
        "/v1/query",
        Some(&format!("Bearer {}", unknown_token.encoded())),
        VALID_SCAN_BODY,
    );

    let token = login(addr, "alice", "pw-alice");
    let close_response = send_request(
        addr,
        "/v1/session/close",
        Some(&format!("Bearer {token}")),
        b"",
    );
    assert!(split_response(&close_response)
        .0
        .starts_with("HTTP/1.1 200 "));
    let closed = send_request(
        addr,
        "/v1/query",
        Some(&format!("Bearer {token}")),
        VALID_SCAN_BODY,
    );

    fn strip_date(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes)
            .lines()
            .filter(|line| !line.starts_with("Date: "))
            .collect::<Vec<_>>()
            .join("\n")
    }

    let baseline = strip_date(&missing);
    assert_eq!(baseline, strip_date(&unknown));
    assert_eq!(baseline, strip_date(&closed));
}

// --- (6) Bearer 欠落 + JSON tenant_id → 28000（順序: Bearer が先） ---------

#[test]
fn missing_bearer_with_tenant_id_json_still_rejects_with_28000() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    let body = br#"{"op":"scan","table":"docs","limit":1,"tenant_id":"evil"}"#;
    let response = send_request(addr, "/v1/query", None, body);
    let (status_line, resp_body) = split_response(&response);
    assert!(
        status_line.starts_with("HTTP/1.1 401 "),
        "got: {status_line}"
    );
    assert_eq!(wire_code_of_body(&resp_body), "28000");
}

// --- (7) 有効 Bearer + JSON tenant_id（6 op）→ 42601 ------------------------

#[test]
fn valid_bearer_with_tenant_id_json_rejects_with_42601_for_all_six_ops() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");

    let cases: [&[u8]; 6] = [
        br#"{"op":"search","table":"docs","limit":1,"tenant_id":"evil"}"#,
        br#"{"op":"scan","table":"docs","limit":1,"tenant_id":"evil"}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[],"tenant_id":"evil"}"#,
        br#"{"op":"insert","table":"docs","rows":[],"tenant_id":"evil"}"#,
        br#"{"op":"update","table":"docs","set":{"lang":"en"},"tenant_id":"evil"}"#,
        br#"{"op":"delete","table":"docs","where":{"id":1},"tenant_id":"evil"}"#,
    ];
    for body in cases {
        let response = send_request(addr, "/v1/query", Some(&auth), body);
        let (status_line, resp_body) = split_response(&response);
        assert!(
            status_line.starts_with("HTTP/1.1 400 "),
            "body {body:?}: got {status_line}"
        );
        assert_eq!(wire_code_of_body(&resp_body), "42601");
    }
}

// --- (8) 有効 Bearer + tenant ヘッダ（表記揺れ）→ 42601 ---------------------

#[test]
fn valid_bearer_with_tenant_header_variants_rejects_with_42601() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");

    for header_name in ["X-Tenant-Id", "Tenant-Id", "X-Tenant", "TenantId"] {
        let response = send_request_with_headers(
            addr,
            "/v1/query",
            Some(&auth),
            &[(header_name, "other")],
            VALID_SCAN_BODY,
        );
        let (status_line, resp_body) = split_response(&response);
        assert!(
            status_line.starts_with("HTTP/1.1 400 "),
            "header {header_name}: got {status_line}"
        );
        assert_eq!(wire_code_of_body(&resp_body), "42601");
    }
}

// --- (9) パス位置の tenant_id → 42601／それ以外の非厳密一致 → 08P01 --------

#[test]
fn tenant_id_in_path_or_query_string_rejects_with_42601() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");

    for target in ["/v1/query?tenant_id=other", "/v1/query/tenant_id/other"] {
        let request = format!(
            "POST {target} HTTP/1.1\r\nHost: localhost\r\nAuthorization: {auth}\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n"
        );
        let response = send_raw_request(addr, request.as_bytes());
        let (status_line, resp_body) = split_response(&response);
        assert!(
            status_line.starts_with("HTTP/1.1 400 "),
            "target {target}: got {status_line}"
        );
        assert_eq!(wire_code_of_body(&resp_body), "42601");
    }
}

#[test]
fn non_strict_query_target_variants_reject_with_08p01() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    for target in ["/v1/query?x=1", "/v1/query/"] {
        let request = format!(
            "POST {target} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n"
        );
        let response = send_raw_request(addr, request.as_bytes());
        let (status_line, resp_body) = split_response(&response);
        assert!(
            status_line.starts_with("HTTP/1.1 400 "),
            "target {target}: got {status_line}"
        );
        assert_eq!(wire_code_of_body(&resp_body), "08P01");
    }
}

// --- (10) HTTP-5 非 vacuous: lookup は枠を消費しない -----------------------

#[test]
fn repeated_query_requests_do_not_consume_session_slot() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let sessions = SessionStore::with_limits(1, wire_server::limits::SESSION_TTL);
    let addr = http_common::spawn_router_listener(&users_path, sessions);
    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");

    for _ in 0..5 {
        let response = send_request(addr, "/v1/query", Some(&auth), VALID_SCAN_BODY);
        let (status_line, _) = split_response(&response);
        // `scan` 実行結線後（TASK-186・NOSQL-3・Issue #766）はスローアウェイ
        // `EngineCore` 上で `42P01`／404 が到達の証跡になる。
        assert!(
            status_line.starts_with("HTTP/1.1 404 "),
            "got: {status_line}"
        );
    }

    // 上限 1 のセッションストアで、2 人目のログインは枠不足のまま拒否される
    // （`lookup` が枠を消費・解放していないことの間接証跡。close 後は成功する）。
    let second_login = send_request(
        addr,
        "/v1/session",
        None,
        br#"{"user":"alice","password":"pw-alice"}"#,
    );
    let (second_status, second_body) = split_response(&second_login);
    assert!(
        second_status.starts_with("HTTP/1.1 503 "),
        "got: {second_status}"
    );
    assert_eq!(wire_code_of_body(&second_body), "53300");

    let close_response = send_request(addr, "/v1/session/close", Some(&auth), b"");
    assert!(split_response(&close_response)
        .0
        .starts_with("HTTP/1.1 200 "));

    let third_login = send_request(
        addr,
        "/v1/session",
        None,
        br#"{"user":"alice","password":"pw-alice"}"#,
    );
    assert!(split_response(&third_login).0.starts_with("HTTP/1.1 200 "));
}

// --- (11) テナント分離: a の close が b の /v1/query に波及しない ----------

#[test]
fn closing_one_tenant_session_does_not_affect_another_tenants_query_access() {
    let users_path = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());
    let token_a = login(addr, "alice", "pw-alice");
    let token_b = login(addr, "bob", "pw-bob");

    let close_a = send_request(
        addr,
        "/v1/session/close",
        Some(&format!("Bearer {token_a}")),
        b"",
    );
    assert!(split_response(&close_a).0.starts_with("HTTP/1.1 200 "));

    // bob のセッションはまだ有効で /v1/query に到達できる（`scan` 実行結線後
    // 〔TASK-186・NOSQL-3・Issue #766〕はスローアウェイ `EngineCore` 上で
    // `42P01`／404 が到達の証跡になる）。
    let query_b = send_request(
        addr,
        "/v1/query",
        Some(&format!("Bearer {token_b}")),
        VALID_SCAN_BODY,
    );
    assert!(split_response(&query_b).0.starts_with("HTTP/1.1 404 "));

    // alice のセッションは close 済みで 28000。
    let query_a = send_request(
        addr,
        "/v1/query",
        Some(&format!("Bearer {token_a}")),
        VALID_SCAN_BODY,
    );
    let (status_a, body_a) = split_response(&query_a);
    assert!(status_a.starts_with("HTTP/1.1 401 "));
    assert_eq!(wire_code_of_body(&body_a), "28000");
}

// --- (12) 非漏えい: 応答本文にユーザー名・テナント ID・トークンを含まない --

#[test]
fn responses_do_not_leak_username_tenant_id_or_token() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");

    let ok_response = send_request(addr, "/v1/query", Some(&auth), VALID_SCAN_BODY);
    let ok_text = String::from_utf8_lossy(&ok_response);
    assert!(!ok_text.contains("alice"));
    assert!(!ok_text.contains("tenant-a"));
    assert!(!ok_text.contains(&token));

    let unauthorized = send_request(addr, "/v1/query", None, VALID_SCAN_BODY);
    let unauthorized_text = String::from_utf8_lossy(&unauthorized);
    assert!(!unauthorized_text.contains("alice"));
    assert!(!unauthorized_text.contains("tenant-a"));
}

// --- (13) PolicyContext の非 vacuous 証跡（単体テスト側は middleware.rs に
// 既にあるが、結合テストとして到達可能性を追加で固定する） ------------------

#[test]
fn valid_bearer_reaches_gate_for_every_op_schema_minimal_form() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");

    // `scan`（TASK-186・NOSQL-3・Issue #766）・`aggregate`（Issue #768）・
    // `insert`（Issue #772）・`search`（TASK-186・NOSQL-2・Issue #764）は
    // いずれも実行結線済みのため、暫定 `0A000`／501 へ到達する op は
    // 存在しない（`placeholder_cases` は空のまま維持し、将来 op が増えた
    // 際の回帰検知点として残す）。
    let placeholder_cases: [&[u8]; 0] = [];
    for body in placeholder_cases {
        let response = send_request(addr, "/v1/query", Some(&auth), body);
        let (status_line, resp_body) = split_response(&response);
        assert!(
            status_line.starts_with("HTTP/1.1 501 "),
            "body {body:?}: got {status_line}"
        );
        assert_eq!(wire_code_of_body(&resp_body), "0A000");
    }

    let executed_cases: [&[u8]; 4] = [
        VALID_SCAN_BODY,
        br#"{"op":"aggregate","table":"docs","aggregates":[{"fn":"count","column":"id"}]}"#,
        br#"{"op":"insert","table":"docs","rows":[{"id":1,"embedding":[0.1,0.2,0.3]}],"operation_id":"http5-op-1"}"#,
        br#"{"op":"search","table":"docs","limit":1}"#,
    ];
    for body in executed_cases {
        let response = send_request(addr, "/v1/query", Some(&auth), body);
        let (status_line, resp_body) = split_response(&response);
        assert!(
            status_line.starts_with("HTTP/1.1 404 "),
            "body {body:?}: got {status_line}"
        );
        assert_eq!(wire_code_of_body(&resp_body), "42P01");
    }
}
