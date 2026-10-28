//! production ルータ（[`wire_server::http::router::Router`]）が
//! `/v1/session`・`/v1/session/close`・`/v1/query` の 3 エンドポイントへ
//! 限定され、それ以外の未知パス・非 `POST` メソッドを `08P01` で拒否する
//! ことを検証する結合テスト（Issue #758・TASK-179／NOSQL-1・HTTP-2。
//! ポインタ: `docs/spec/05-tasks.md` TASK-179・
//! `docs/spec/04-behavior/nosql-surface.md` NOSQL-1・
//! `docs/spec/04-behavior/http-transport.md` HTTP-2）。
//!
//! `http_common::spawn_router_listener`（production ルータ経由）を使う
//! 点が `http2_framing.rs`（`PlaceholderRouter` 固定・フレーミング層専用）
//! との違い。拒否系の送受信は要求行のみで拒否される非 `POST` 要求も含む
//! ため、`http_common::send_raw` の `AfterWrite::HalfClose` を使い、
//! 読み捨てクローズの 1 秒 lingering を避ける。

use crate::common;
use crate::http_common;

use std::net::SocketAddr;

use http_common::{AfterWrite, HttpResponse};
use wire_server::http::session::store::SessionStore;

/// `POST /v1/session` へログインしてトークン（base64url 表現）を取り出す。
fn login(addr: SocketAddr, user: &str, password: &str) -> String {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);
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
    match engine::json::parse_json(&String::from_utf8_lossy(&resp.body))
        .expect("login body must be valid json")
    {
        engine::json::JsonValue::Object(mut obj) => match obj.remove("token") {
            Some(engine::json::JsonValue::String(s)) => s,
            other => panic!("expected string token field, got {other:?}"),
        },
        other => panic!("expected json object body, got {other:?}"),
    }
}

/// 正常形の `POST <target>`（`Content-Type: application/json`・宣言済み
/// `Content-Length`）を送って解析済み応答を返す便宜 API。
fn post(addr: SocketAddr, target: &str, auth: Option<&str>, body: &[u8]) -> HttpResponse {
    let mut headers: Vec<(&str, &str)> = Vec::new();
    let auth_header;
    if let Some(auth) = auth {
        auth_header = format!("Bearer {auth}");
        headers.push(("Authorization", &auth_header));
    }
    let content_length = body.len().to_string();
    headers.push(("Content-Type", "application/json"));
    headers.push(("Content-Length", &content_length));
    let request = http_common::build_request(target, &headers, body);
    http_common::parse_single_response(&http_common::send_raw(
        addr,
        &request,
        AfterWrite::HalfClose,
    ))
}

/// `method` を差し替えた生の要求行を組み立てる（[`http_common::
/// build_request`] は `POST` 固定のため、非 `POST` 検証にのみ本関数を使う）。
fn build_request_with_method(method: &str, target: &str, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(method.as_bytes());
    out.push(b' ');
    out.extend_from_slice(target.as_bytes());
    out.extend_from_slice(b" HTTP/1.1\r\n");
    out.extend_from_slice(b"Content-Type: application/json\r\n");
    out.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

const VALID_SCAN_BODY: &[u8] = br#"{"op":"scan","table":"docs","limit":1}"#;

// --- (T1) 3 エンドポイント到達性: それぞれ異なるハンドラ固有応答 ----------

#[test]
fn three_endpoints_reach_distinct_handler_specific_responses() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    // /v1/session: 200 + token フィールド。
    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let login_resp = post(addr, "/v1/session", None, login_body);
    assert_eq!(login_resp.status, 200);
    let login_text = String::from_utf8_lossy(&login_resp.body).into_owned();

    // /v1/session/close: 有効 Bearer で 200 + {"closed":true}。
    let token = login(addr, "alice", "pw-alice");
    let close_resp = post(addr, "/v1/session/close", Some(&token), b"");
    assert_eq!(close_resp.status, 200);
    let close_text = String::from_utf8_lossy(&close_resp.body).into_owned();
    assert!(close_text.contains("\"closed\":true"), "got: {close_text}");

    // /v1/query: 有効 Bearer（別トークン）+ 最小 scan 本文。`scan` は
    // TASK-186・NOSQL-3（Issue #766）で実行結線済みのため、スローアウェイ
    // `EngineCore`（テーブル未作成）上では `42P01`／404 が到達の証跡になる。
    let token2 = login(addr, "alice", "pw-alice");
    let query_resp = post(addr, "/v1/query", Some(&token2), VALID_SCAN_BODY);
    http_common::assert_reached_query_gate(&query_resp);
    let query_text = String::from_utf8_lossy(&query_resp.body).into_owned();

    // 3 応答が互いに異なることで非 vacuous（同じ固定応答へ縮退していない）。
    assert_ne!(login_text, close_text);
    assert_ne!(login_text, query_text);
    assert_ne!(close_text, query_text);
}

// --- (T2) 未知パス: 完全未知・クエリ文字列付き・末尾スラッシュ／余剰 -------
// --- セグメント・大文字小文字違い・二重スラッシュ・パーセントエンコード ---

#[test]
fn unknown_targets_are_rejected_with_08p01_byte_identical_except_date() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    let unknown_targets = [
        // (a) 完全未知
        "/",
        "/definitely/unknown",
        "/v1",
        "/v1/sessions",
        // (b) 既知エンドポイント＋クエリ文字列
        "/v1/session?x=1",
        "/v1/session/close?x=1",
        "/v1/query?x=1",
        // (c) 既知エンドポイント＋末尾スラッシュ／余剰セグメント
        "/v1/session/",
        "/v1/session/close/",
        "/v1/query/",
        "/v1/query/extra",
        // 追加: 大文字小文字・スラッシュ二重化・パーセントエンコード・
        // フラグメント
        "/V1/QUERY",
        "//v1/query",
        "/v1/query%2F",
        "/v1/query#frag",
    ];

    let mut bodies_without_date: Vec<String> = Vec::new();
    for target in unknown_targets {
        let resp = post(addr, target, None, b"");
        http_common::assert_reached_router(&resp);
        http_common::assert_message_does_not_echo(&resp, target);
        let rendered = format!(
            "status={} headers={:?} body={:?}",
            resp.status,
            resp.headers
                .iter()
                .filter(|(name, _)| !name.eq_ignore_ascii_case("date"))
                .collect::<Vec<_>>(),
            resp.body
        );
        bodies_without_date.push(rendered);
    }

    let first = &bodies_without_date[0];
    for (target, rendered) in unknown_targets.iter().zip(bodies_without_date.iter()) {
        assert_eq!(
            rendered, first,
            "target {target:?} produced a different (Date-stripped) response"
        );
    }
}

// --- (T3) 非 POST × 3 エンドポイント ---------------------------------------

#[test]
fn non_post_methods_are_rejected_with_08p01_for_all_three_endpoints() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    let methods = ["GET", "HEAD", "PUT", "DELETE", "OPTIONS", "PATCH", "post"];
    let targets = ["/v1/session", "/v1/session/close", "/v1/query"];

    for target in targets {
        for method in methods {
            let request = build_request_with_method(method, target, b"");
            let resp = http_common::parse_single_response(&http_common::send_raw(
                addr,
                &request,
                AfterWrite::HalfClose,
            ));
            // 非 POST は要求行パース時点（フレーミング層）で拒否され、
            // production ルータへは到達しない。`assert_rejected` の
            // 「message != ROUTER_PLACEHOLDER_MESSAGE」断定が、ルータへの
            // 素通りが起きていないことの証跡になる。
            http_common::assert_rejected(&resp, 400, "08P01");
        }
    }
}

// --- (T4a) ルーティングは認証より前・セッション枠を消費／失効させない -----

#[test]
fn routing_precedes_authentication_and_does_not_consume_or_invalidate_session() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let sessions = SessionStore::with_limits(1, wire_server::limits::SESSION_TTL);
    let addr = http_common::spawn_router_listener(&users_path, sessions);
    let token = login(addr, "alice", "pw-alice");

    for target in ["/v1/query/extra", "/v1/session?x=1"] {
        for _ in 0..3 {
            let resp = post(addr, target, Some(&token), b"");
            // Bearer 付きでも未知パスは `08P01`（`28000` にならない ==
            // ルーティングが認証より前に判定される証跡）。
            http_common::assert_reached_router(&resp);
        }
    }

    // 同一トークンで `/v1/query` へは引き続き到達できる（未知パス要求が
    // トークンを失効させていない証跡）。
    let query_resp = post(addr, "/v1/query", Some(&token), VALID_SCAN_BODY);
    http_common::assert_reached_query_gate(&query_resp);
}

// --- (T4b) ルーティングはセッション枠を確保しない --------------------------

#[test]
fn unknown_target_requests_do_not_consume_session_slots() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let sessions = SessionStore::with_limits(1, wire_server::limits::SESSION_TTL);
    let addr = http_common::spawn_router_listener(&users_path, sessions);

    for _ in 0..5 {
        let resp = post(addr, "/v1/definitely/unknown", None, b"");
        http_common::assert_reached_router(&resp);
    }

    // 枠 1 のセッションストアでも、未知パス要求のみでは枠が消費されて
    // いないため、初回ログインは成功する。
    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let login_resp = post(addr, "/v1/session", None, login_body);
    assert_eq!(login_resp.status, 200, "got: {login_resp:?}");
}

// --- (T5) ルーティングは本文検証より前 --------------------------------------

#[test]
fn routing_precedes_body_validation() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    // tenant_id を含む JSON 本文でも未知パスは `08P01`（`42601` にならない）。
    let tenant_body = br#"{"op":"scan","table":"docs","limit":1,"tenant_id":"evil"}"#;
    let resp = post(addr, "/v1/unknown", None, tenant_body);
    http_common::assert_reached_router(&resp);

    // 不正 JSON 本文でも未知パスは `08P01`（`42601` にならない）。
    let malformed_body = b"{not valid json";
    let resp = post(addr, "/v1/unknown", None, malformed_body);
    http_common::assert_reached_router(&resp);
}

// --- (T6) 優先規則の固定: tenant マーカー（42601）> 未知パス（08P01） -----

#[test]
fn query_tenant_marker_takes_priority_over_unknown_target() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");

    let tenant_marker_resp = post(addr, "/v1/query?tenant_id=x", Some(&token), b"");
    assert_eq!(tenant_marker_resp.status, 400);
    assert_eq!(http_common::wire_code_of(&tenant_marker_resp), "42601");

    let unknown_resp = post(addr, "/v1/query?x=1", Some(&token), b"");
    http_common::assert_reached_router(&unknown_resp);
}

// --- (T7) 未知パス要求の直後に正常形の要求が成功する（他要求への非影響） --

#[test]
fn well_formed_login_succeeds_immediately_after_unknown_target_request() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = http_common::spawn_router_listener(&users_path, SessionStore::new());

    let unknown_resp = post(addr, "/v1/nope", None, b"");
    http_common::assert_reached_router(&unknown_resp);

    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let login_resp = post(addr, "/v1/session", None, login_body);
    assert_eq!(login_resp.status, 200, "got: {login_resp:?}");
}
