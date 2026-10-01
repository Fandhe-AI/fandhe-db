//! `POST /v1/session` の結合テスト（Issue #752・TASK-174・HTTP-4・HTTP-6。
//! 対象ポインタ: `docs/spec/05-tasks.md` TASK-174・
//! `docs/spec/04-behavior/http-transport.md` HTTP-4・HTTP-6）。
//!
//! 層 A（本ファイル・`cargo test`・`make ci` 対象）: ephemeral port
//! （`127.0.0.1:0`）で `wire_server::http::listener::accept_loop_with_router`
//! を in-process サーバースレッドとして起動し、`std::net::TcpStream` で生
//! HTTP/1.1 要求を送受信する自作クライアントで、正しい資格・誤資格・
//! 未知ユーザー・不正 JSON・未知キー・セッション上限超過・未対応パスの
//! 各ケースを検証する（§5.1）。さらに `common::SpawnedServer` で実バイナリ
//! （`wire-server --surface nosql`）を子プロセスとして起動し、`main.rs` の
//! 結線を通じて実際に `auth::verify` へ到達する非 vacuous な証跡を取る
//! （§5.2）。

#[path = "common/mod.rs"]
mod common;

// engine 側のテスト専用一時 DB ヘルパー（`std` のみに依存し `crate::` を
// 参照しないため取り込み可能。`Router::with_engine`〔Issue #766・#768〕へ
// 渡すスローアウェイ core を本ファイル固有に用意するため取り込む）。
#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use wire_server::auth::AUTH_FAILURE_DELAY;
use wire_server::http::router::Router;
use wire_server::http::session::store::SessionStore;
use wire_server::http::session::token::SessionToken;
use wire_server::limits::{ConnectionLimiter, SESSION_TTL};

/// `accept_loop_with_router` を in-process サーバースレッドで起動し、
/// 接続先アドレスを返す。`sessions` は呼び出し元が上限・TTL を制御できる
/// よう `SessionStore` をそのまま受け取る（`with_limits` でセッション上限を
/// 小さくしたケースを検証するため）。`core` は本ファイルのテストが
/// `/v1/query` を送らないため、テーブルを一切持たないスローアウェイ
/// `EngineCore` で十分（TASK-186・NOSQL-3）。
fn spawn_router_server(
    users_path: &std::path::Path,
    sessions: SessionStore,
) -> std::net::SocketAddr {
    let store = wire_server::auth::UserStore::load_from_file(users_path).expect("valid store");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(wire_server::limits::MAX_CONNECTIONS);
    let core_path = temp_db::unique_db_path("http4-session-issue-throwaway");
    let core = EngineCore::open(&core_path).expect("open throwaway engine core");
    // プロセス寿命の accept スレッドへ move するため Drop ガードを使えない。open 直後に
    // unlink して残置を防ぐ（Issue #1303。非 unix は残置する文書化された例外）。
    temp_db::unlink_open_db_file(&core_path);
    let router = Router::with_engine(
        std::sync::Arc::new(store),
        sessions,
        std::sync::Arc::new(core),
    );

    std::thread::spawn(move || {
        wire_server::http::listener::accept_loop_with_router(
            listener,
            limiter,
            wire_server::limits::READ_TIMEOUT,
            router,
        );
    });

    addr
}

/// 生 HTTP/1.1 要求を組み立てて送信し、EOF まで応答を読み切って返す
/// （NoSQL 表層は 1 応答＝1 接続でクローズする設計のため、EOF が応答終端の
/// 合図になる）。
fn send_request(addr: std::net::SocketAddr, body: &[u8]) -> (Vec<u8>, Duration) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");

    let mut request = Vec::new();
    request.extend_from_slice(b"POST /v1/session HTTP/1.1\r\n");
    request.extend_from_slice(b"Host: localhost\r\n");
    request.extend_from_slice(b"Content-Type: application/json\r\n");
    request.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    request.extend_from_slice(b"\r\n");
    request.extend_from_slice(body);

    let start = Instant::now();
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
    let elapsed = start.elapsed();
    (received, elapsed)
}

/// 生要求（任意のターゲット・メソッド込みの要求行）を送り応答を読み切る
/// （未対応パス・メソッドの検証向け。`send_request` は `/v1/session` 固定）。
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

/// レスポンス生バイト列を「ステータス行」「本文（UTF-8）」へ分解する。
fn split_response(bytes: &[u8]) -> (String, String) {
    let text = String::from_utf8_lossy(bytes).into_owned();
    let mut parts = text.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("").to_string();
    let body = parts.next().unwrap_or("").to_string();
    let status_line = head.lines().next().unwrap_or("").to_string();
    (status_line, body)
}

fn json_object(body: &str) -> std::collections::BTreeMap<String, JsonValue> {
    match parse_json(body).expect("response body must be valid JSON") {
        JsonValue::Object(map) => map,
        other => panic!("expected JSON object, got {other:?}"),
    }
}

#[test]
fn valid_credentials_return_200_with_token_and_expires_in() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_server(&users_path, SessionStore::new());

    let (response, _elapsed) = send_request(addr, br#"{"user":"alice","password":"pw-alice"}"#);
    let (status_line, body) = split_response(&response);

    assert!(
        status_line.starts_with("HTTP/1.1 200 "),
        "got: {status_line}"
    );
    let obj = json_object(&body);
    let token = match obj.get("token") {
        Some(JsonValue::String(s)) => s.clone(),
        other => panic!("expected string token field, got {other:?}"),
    };
    assert!(
        SessionToken::parse(&token).is_ok(),
        "token must be a valid SessionToken encoding: {token}"
    );
    match obj.get("expires_in") {
        Some(JsonValue::Number(n)) => assert_eq!(n.as_f64(), SESSION_TTL.as_secs() as f64),
        other => panic!("expected numeric expires_in field, got {other:?}"),
    }

    // 本文はランダムなトークン（base64url。~1e-8 の確率だが "alice" 等の
    // 部分文字列を偶然含みうる）を含むため、それを除いた残りへ限定して
    // テナント ID・ユーザー名の非漏えいを検査する（応答全体からトークン値の
    // 出現箇所を除去）。
    let full_text = String::from_utf8_lossy(&response).into_owned();
    let redacted = full_text.replace(&token, "<token>");
    assert!(!redacted.contains("tenant-a"), "must not leak tenant id");
    assert!(!redacted.contains("alice"), "must not leak username");
}

#[test]
fn wrong_password_rejects_after_min_delay_with_28p01() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_server(&users_path, SessionStore::new());

    let (response, elapsed) = send_request(addr, br#"{"user":"alice","password":"wrong"}"#);
    let (status_line, body) = split_response(&response);

    assert!(
        elapsed >= AUTH_FAILURE_DELAY,
        "elapsed {elapsed:?} must be at least the fixed auth failure delay"
    );
    assert!(
        status_line.starts_with("HTTP/1.1 401 "),
        "got: {status_line}"
    );
    assert!(response_contains(&response, "28P01"));
    assert!(response_contains(&response, "AUTH_INVALID"));
    let obj = json_object(&body);
    assert!(!obj.contains_key("tenant_id"));
}

#[test]
fn unknown_user_rejects_after_min_delay_with_28p01_and_matches_wrong_password_bytes() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let sessions_a = SessionStore::new();
    let sessions_b = SessionStore::new();
    let addr_a = spawn_router_server(&users_path, sessions_a);
    let addr_b = spawn_router_server(&users_path, sessions_b);

    let (unknown_response, unknown_elapsed) =
        send_request(addr_a, br#"{"user":"bob","password":"whatever"}"#);
    let (wrong_response, _wrong_elapsed) =
        send_request(addr_b, br#"{"user":"alice","password":"wrong"}"#);

    assert!(
        unknown_elapsed >= AUTH_FAILURE_DELAY,
        "elapsed {unknown_elapsed:?} must be at least the fixed auth failure delay"
    );
    let (status_line, _body) = split_response(&unknown_response);
    assert!(
        status_line.starts_with("HTTP/1.1 401 "),
        "got: {status_line}"
    );
    assert!(response_contains(&unknown_response, "28P01"));
    assert!(response_contains(&unknown_response, "AUTH_INVALID"));

    // 未知ユーザー・誤パスワードの応答は `Date` 行を除きバイト同一になる
    // （`auth::verify` の対称性・固定文言。`docs/design` の対称性契約に
    // 対応する外形検証）。
    assert_eq!(
        strip_date_header(&unknown_response),
        strip_date_header(&wrong_response),
        "unknown-user and wrong-password responses must be identical except Date"
    );
}

#[test]
fn json_syntax_error_rejects_with_42601() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_server(&users_path, SessionStore::new());

    let (response, _elapsed) = send_request(addr, b"not json");
    let (status_line, _body) = split_response(&response);

    assert!(
        status_line.starts_with("HTTP/1.1 400 "),
        "got: {status_line}"
    );
    assert!(response_contains(&response, "42601"));
}

#[test]
fn missing_user_field_rejects_with_42601() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_server(&users_path, SessionStore::new());

    let (response, _elapsed) = send_request(addr, br#"{"password":"pw-alice"}"#);
    let (status_line, _body) = split_response(&response);

    assert!(
        status_line.starts_with("HTTP/1.1 400 "),
        "got: {status_line}"
    );
    assert!(response_contains(&response, "42601"));
}

#[test]
fn password_as_non_string_rejects_with_42601() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_server(&users_path, SessionStore::new());

    let (response, _elapsed) = send_request(addr, br#"{"user":"alice","password":123}"#);
    let (status_line, _body) = split_response(&response);

    assert!(
        status_line.starts_with("HTTP/1.1 400 "),
        "got: {status_line}"
    );
    assert!(response_contains(&response, "42601"));
}

#[test]
fn unknown_field_including_tenant_id_rejects_with_42601() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_server(&users_path, SessionStore::new());

    let (response, _elapsed) = send_request(
        addr,
        br#"{"user":"alice","password":"pw-alice","tenant_id":"other-tenant"}"#,
    );
    let (status_line, _body) = split_response(&response);

    assert!(
        status_line.starts_with("HTTP/1.1 400 "),
        "got: {status_line}"
    );
    assert!(response_contains(&response, "42601"));
}

#[test]
fn empty_body_rejects_with_42601() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_server(&users_path, SessionStore::new());

    let (response, _elapsed) = send_request(addr, b"");
    let (status_line, _body) = split_response(&response);

    assert!(
        status_line.starts_with("HTTP/1.1 400 "),
        "got: {status_line}"
    );
    assert!(response_contains(&response, "42601"));
}

#[test]
fn session_limit_exceeded_rejects_second_login_with_53300() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_server(&users_path, SessionStore::with_limits(1, SESSION_TTL));

    let (first, _e1) = send_request(addr, br#"{"user":"alice","password":"pw-alice"}"#);
    let (first_status, _) = split_response(&first);
    assert!(
        first_status.starts_with("HTTP/1.1 200 "),
        "got: {first_status}"
    );

    let (second, _e2) = send_request(addr, br#"{"user":"alice","password":"pw-alice"}"#);
    let (second_status, _) = split_response(&second);
    assert!(
        second_status.starts_with("HTTP/1.1 503 "),
        "got: {second_status}"
    );
    assert!(response_contains(&second, "53300"));
}

#[test]
fn two_successful_logins_yield_different_tokens() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_server(&users_path, SessionStore::new());

    let (first, _) = send_request(addr, br#"{"user":"alice","password":"pw-alice"}"#);
    let (second, _) = send_request(addr, br#"{"user":"alice","password":"pw-alice"}"#);

    let (_, first_body) = split_response(&first);
    let (_, second_body) = split_response(&second);
    let first_token = json_object(&first_body).get("token").cloned();
    let second_token = json_object(&second_body).get("token").cloned();
    assert_ne!(first_token, second_token, "reissued tokens must differ");
}

#[test]
fn unsupported_paths_are_rejected_with_08p01() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_server(&users_path, SessionStore::new());

    for target in [
        "/v1/query?x=1",
        "/v1/session?x=1",
        "/v1/session/close?x=1",
        "/v1/session/close/",
        "/other",
    ] {
        let request = format!(
            "POST {target} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n"
        );
        let response = send_raw_request(addr, request.as_bytes());
        let (status_line, _body) = split_response(&response);
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

/// 応答生バイト列に固定文字列が含まれるかを見る簡易ヘルパ（`wire_code`・
/// `code` 等、本文中の固定英語文言の存在検査に使う）。
fn response_contains(response: &[u8], needle: &str) -> bool {
    String::from_utf8_lossy(response).contains(needle)
}

/// `Date: ...\r\n` の 1 行を取り除いた応答バイト列を返す（未知ユーザー・
/// 誤パスワード応答のバイト同一性比較で、時刻に依存する `Date` 行だけを
/// 除外するため）。
fn strip_date_header(response: &[u8]) -> String {
    let text = String::from_utf8_lossy(response);
    text.lines()
        .filter(|line| !line.starts_with("Date: "))
        .collect::<Vec<_>>()
        .join("\n")
}

/// プロセス経由スモーク（§5.2）: 実バイナリ `wire-server --surface nosql` を
/// 起動し、`main.rs::run_server` の結線を通じて `POST /v1/session` の正当
/// ログインが実際に `auth::verify` へ到達し 200 を返すことを確認する
/// （非 vacuous 証跡。in-process テストは `Router`／`session::issue::handle`
/// を直接呼ぶため、`main.rs` の配線自体はここでのみ検証される）。
#[test]
fn spawned_binary_accepts_valid_login_over_nosql_surface() {
    let fixture = common::TempFixtureDir::new("http4-session-issue");
    let users_path = fixture.users_path_str();
    let db_path = fixture.db_path_str();

    let mut content = String::new();
    {
        use wire_server::auth::argon2id;
        let salt = b"0123456789abcdef";
        let phc = argon2id::encode_phc(b"pw-alice", salt, &argon2id::RECOMMENDED_PARAMS)
            .expect("valid phc encoding");
        content.push_str(&format!("alice:tenant-a:{phc}\n"));
    }
    std::fs::write(&users_path, content).expect("write user store fixture");

    let mut server = common::SpawnedServer::spawn(&[
        "--users",
        &users_path,
        "--db",
        &db_path,
        "--bind",
        "127.0.0.1:0",
        "--surface",
        "nosql",
    ]);

    let deadline = Instant::now() + Duration::from_secs(10);
    let addr_str = server
        .wait_for_listening(deadline)
        .expect("server must report listening address");
    let addr: std::net::SocketAddr = addr_str.parse().expect("valid socket addr");

    let (response, _elapsed) = send_request(addr, br#"{"user":"alice","password":"pw-alice"}"#);
    let (status_line, _body) = split_response(&response);
    assert!(
        status_line.starts_with("HTTP/1.1 200 "),
        "got: {status_line}"
    );

    let seen = server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    let joined = seen.join("");
    assert!(
        !joined.contains("tenant-a"),
        "stderr must not leak tenant id"
    );
    assert!(!joined.contains("alice"), "stderr must not leak username");
}
