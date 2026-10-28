//! `POST /v1/session/close` の結合テスト（Issue #753・TASK-174・HTTP-8。
//! 関連 HTTP-5・HTTP-6。対象ポインタ: `docs/spec/05-tasks.md` TASK-174・
//! `docs/spec/04-behavior/http-transport.md` HTTP-8）。
//!
//! 層 A（本ファイル・`cargo test`・`make ci` 対象）: `http_common::
//! spawn_router_listener`（`http4_session_issue.rs::spawn_router_server` の
//! 昇格版）で production ルータ（[`wire_server::http::router::Router`]）を
//! in-process サーバースレッドとして起動し、`std::net::TcpStream` で生
//! HTTP/1.1 要求を送受信する自作クライアントで、正常な close・Bearer 欠落・
//! 不正 Bearer・応答のバイト同一性・枠解放・テナント分離・未対応パスの
//! 各ケースを検証する。実バイナリ経由のプロセススモークは `http4_session_issue.rs`
//! （§5.2。`main.rs` の配線自体は `/v1/session` 側で既に検証済み・本ファイルは
//! 対象外）。

use crate::common;
use crate::http_common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use wire_server::http::session::store::SessionStore;
use wire_server::http::session::token::SessionToken;
use wire_server::limits::SESSION_TTL;

/// `POST /v1/session` へログインしてトークン（base64url 表現）を取り出す。
fn login(addr: std::net::SocketAddr, user: &str, password: &str) -> String {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);
    let response = send_request(addr, "/v1/session", body.as_bytes());
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

/// `target` へ `Content-Type: application/json` の要求を送り、EOF まで応答を
/// 読み切って返す（NoSQL 表層は 1 応答＝1 接続でクローズする設計のため EOF が
/// 応答終端の合図になる）。
fn send_request(addr: std::net::SocketAddr, target: &str, body: &[u8]) -> Vec<u8> {
    send_request_with_auth(addr, target, None, body)
}

/// [`send_request`] に `Authorization` ヘッダ（任意）を追加できる版。
fn send_request_with_auth(
    addr: std::net::SocketAddr,
    target: &str,
    auth: Option<&str>,
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
    request.extend_from_slice(b"Content-Type: application/json\r\n");
    request.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    request.extend_from_slice(b"\r\n");
    request.extend_from_slice(body);

    stream.write_all(&request).expect("send request");

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

/// 生要求（任意のターゲット込み）を送り応答を読み切る（未対応パス検証向け）。
fn send_raw_request(addr: std::net::SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream.write_all(request).expect("send request");

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

fn response_contains(response: &[u8], needle: &str) -> bool {
    String::from_utf8_lossy(response).contains(needle)
}

/// `Date: ...\r\n` の 1 行を取り除いた応答バイト列（時刻に依存する行だけを
/// 除外したバイト同一性比較のため）。
fn strip_date_header(response: &[u8]) -> String {
    let text = String::from_utf8_lossy(response);
    text.lines()
        .filter(|line| !line.starts_with("Date: "))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn valid_close_returns_200_and_second_close_rejects_with_28000() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");

    let auth = format!("Bearer {token}");
    let first = send_request_with_auth(addr, "/v1/session/close", Some(&auth), b"");
    let (first_status, first_body) = split_response(&first);
    assert!(
        first_status.starts_with("HTTP/1.1 200 "),
        "got: {first_status}"
    );
    assert!(first_body.contains("\"closed\":true"), "got: {first_body}");

    let second = send_request_with_auth(addr, "/v1/session/close", Some(&auth), b"");
    let (second_status, _) = split_response(&second);
    assert!(
        second_status.starts_with("HTTP/1.1 401 "),
        "got: {second_status}"
    );
    assert!(response_contains(&second, "28000"));
    assert!(response_contains(&second, "AUTH_REQUIRED"));
    assert!(response_contains(&second, "WWW-Authenticate: Bearer"));
}

#[test]
fn missing_authorization_rejects_with_28000() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    let response = send_request(addr, "/v1/session/close", b"");
    let (status_line, _) = split_response(&response);
    assert!(
        status_line.starts_with("HTTP/1.1 401 "),
        "got: {status_line}"
    );
    assert!(response_contains(&response, "28000"));
}

#[test]
fn malformed_bearer_variants_reject_with_28000() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    let unknown = SessionToken::generate()
        .expect("generate unrelated token")
        .encoded();
    let cases = [
        format!("Basic {unknown}"),
        format!("Bearer {unknown}extra"),
        "Bearer".to_string(),
        format!("Bearer {unknown}"), // 未知トークン（発行されていない）
    ];
    for auth in cases {
        let response = send_request_with_auth(addr, "/v1/session/close", Some(&auth), b"");
        let (status_line, _) = split_response(&response);
        assert!(
            status_line.starts_with("HTTP/1.1 401 "),
            "auth={auth:?}: got {status_line}"
        );
        assert!(response_contains(&response, "28000"), "auth={auth:?}");
    }
}

#[test]
fn missing_unknown_and_double_close_responses_are_byte_identical_except_date() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    let missing = send_request(addr, "/v1/session/close", b"");

    let unknown_token = SessionToken::generate()
        .expect("generate unrelated token")
        .encoded();
    let unknown_auth = format!("Bearer {unknown_token}");
    let unknown = send_request_with_auth(addr, "/v1/session/close", Some(&unknown_auth), b"");

    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");
    let first_close = send_request_with_auth(addr, "/v1/session/close", Some(&auth), b"");
    assert!(String::from_utf8_lossy(&first_close).starts_with("HTTP/1.1 200 "));
    let double_close = send_request_with_auth(addr, "/v1/session/close", Some(&auth), b"");

    let baseline = strip_date_header(&missing);
    assert_eq!(baseline, strip_date_header(&unknown));
    assert_eq!(baseline, strip_date_header(&double_close));
}

#[test]
fn empty_body_and_empty_object_body_are_both_accepted() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    let token_a = login(addr, "alice", "pw-alice");
    let auth_a = format!("Bearer {token_a}");
    let empty_body = send_request_with_auth(addr, "/v1/session/close", Some(&auth_a), b"");
    assert!(String::from_utf8_lossy(&empty_body).starts_with("HTTP/1.1 200 "));

    let token_b = login(addr, "alice", "pw-alice");
    let auth_b = format!("Bearer {token_b}");
    let empty_object = send_request_with_auth(addr, "/v1/session/close", Some(&auth_b), b"{}");
    assert!(String::from_utf8_lossy(&empty_object).starts_with("HTTP/1.1 200 "));
}

#[test]
fn non_empty_object_body_rejects_with_42601_without_consuming_token() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");

    let bad = send_request_with_auth(addr, "/v1/session/close", Some(&auth), br#"{"x":1}"#);
    let (status_line, _) = split_response(&bad);
    assert!(
        status_line.starts_with("HTTP/1.1 400 "),
        "got: {status_line}"
    );
    assert!(response_contains(&bad, "42601"));

    // 不正本文でトークンを消費していないことを確認する（続けて正しい close
    // が 200 になる）。
    let good = send_request_with_auth(addr, "/v1/session/close", Some(&auth), b"");
    assert!(String::from_utf8_lossy(&good).starts_with("HTTP/1.1 200 "));
}

/// 枠解放の非 vacuous 証跡: `SessionStore::with_limits(1, ...)` で上限 1 の
/// もとでも、close 済みトークンが枠を解放し次の login を 200 のまま通す
/// （close なしでは 2 回目 login が `53300` になることは
/// `http4_session_issue.rs::session_limit_exceeded_rejects_second_login_with_53300`
/// が既に固定している）。
#[test]
fn close_frees_session_slot_allowing_a_new_login() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr =
        http_common::spawn_router_listener(&users_path, SessionStore::with_limits(1, SESSION_TTL));

    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");
    let close = send_request_with_auth(addr, "/v1/session/close", Some(&auth), b"");
    assert!(String::from_utf8_lossy(&close).starts_with("HTTP/1.1 200 "));

    // close なしでは 2 回目 login が 503 になるはずだが、close 済みのため
    // 枠が解放され 200 のまま通る。
    let second_token = login(addr, "alice", "pw-alice");
    assert_ne!(token, second_token, "reissued tokens must differ");
}

/// テナント分離: A・B 2 テナントのトークンを発行し、A を close しても B は
/// 別プロセス内 `SessionStore` クローン経由で有効なまま（テナント境界は
/// 応答からは確認できないため、B の close が引き続き成功する＝有効性の
/// 直接証跡とする）。
#[test]
fn closing_one_tenant_does_not_affect_another_tenants_session() {
    let users_path = common::write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    let token_a = login(addr, "alice", "pw-alice");
    let token_b = login(addr, "bob", "pw-bob");

    let auth_a = format!("Bearer {token_a}");
    let close_a = send_request_with_auth(addr, "/v1/session/close", Some(&auth_a), b"");
    assert!(String::from_utf8_lossy(&close_a).starts_with("HTTP/1.1 200 "));

    // A の close 後も B は有効なままであることを、B 自身の close が成功する
    // ことで確認する（B の close が 401 になれば A の close が B へ波及した
    // ことになる）。
    let auth_b = format!("Bearer {token_b}");
    let close_b = send_request_with_auth(addr, "/v1/session/close", Some(&auth_b), b"");
    assert!(
        String::from_utf8_lossy(&close_b).starts_with("HTTP/1.1 200 "),
        "tenant-b session must remain valid after tenant-a close"
    );
}

#[test]
fn unsupported_variants_of_the_close_target_are_rejected_with_08p01() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    for target in ["/v1/session/close?x=1", "/v1/session/close/"] {
        let request = format!(
            "POST {target} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n"
        );
        let response = send_raw_request(addr, request.as_bytes());
        let (status_line, _) = split_response(&response);
        assert!(
            status_line.starts_with("HTTP/1.1 400 "),
            "target {target}: got {status_line}"
        );
        assert!(
            response_contains(&response, "08P01"),
            "target {target}: expected 08P01"
        );
    }
}

/// 応答本文にユーザー名・テナント ID・トークンを含めない
/// （`.claude/rules/security.md` の非漏えい方針）。
#[test]
fn response_does_not_leak_username_tenant_id_or_token() {
    let users_path = common::write_user_store_file(&[("alice", "super-secret-tenant", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");

    let response = send_request_with_auth(addr, "/v1/session/close", Some(&auth), b"");
    let text = String::from_utf8_lossy(&response);
    assert!(!text.contains("super-secret-tenant"));
    assert!(!text.contains("alice"));
    assert!(!text.contains(&token));
}
