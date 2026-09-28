//! HTTP-6（認証失敗の固定遅延・`28P01`／`28000` 分岐）・HTTP-7（`tenant_id`
//! 自己申告の 3 位置拒否）の受け入れ条件を 1 箇所に集約する層 A 結合テスト
//! （Issue #757・TASK-174。ポインタ: `docs/spec/05-tasks.md` TASK-174・
//! `docs/spec/04-behavior/http-transport.md` HTTP-6, HTTP-7）。
//!
//! `http4_session_issue.rs`（Issue #752）・`http5_query_bearer.rs`
//! （Issue #754）が既に固定した契約の大半を再実装せず、本ファイルは
//! それらが個別ファイルに分散して固定していない以下の契約だけを追加する:
//!
//! - 期限切れトークンの `/v1/query` 到達が `28000` へ収束すること
//!   （`http5` は欠落・不正・close 済みのみを検証しており期限切れ未検証）
//! - `/v1/session` 誤資格の `28P01` と `/v1/query` Bearer 欠落の `28000` を
//!   **同一 fixture** で対比し、分岐の取り違え退行を検出すること
//! - `AfterWrite::KeepOpen` によりクライアントが接続を切らなくてもサーバー
//!   側の判断だけで応答＋クローズへ到達すること（`http_common::send_raw`
//!   経由の直接検証）
//! - 検査順序（パス位置 tenant マーカー→認証→tenant ヘッダ→本文検証）の
//!   組み合わせを結合テストとして固定すること（`middleware.rs`・
//!   `router.rs` の module doc に記載された契約の結合テスト側の裏付け）
//!
//! 使うヘルパーは `http_common`（`spawn_router_listener`・`send_raw`・
//! `build_request`・`parse_single_response`・`wire_code_of`・
//! `error_code_of`・`error_message_of`・`assert_message_does_not_echo`）の
//! 既存実装をそのまま再利用し、本ファイル固有の要求組み立てのみ薄いラッパで
//! 追加する。
//!
//! 期限切れの再現は時刻待ち（`sleep`）ではなく `SessionStore::with_limits`
//! に `Duration::ZERO` の TTL を渡し、`SessionStore::active_sessions` の
//! 観測で決定的に確認する（Issue #1083。`store.rs` の `is_alive` は
//! `elapsed < ttl` のため TTL=0 は発行直後から常に期限切れ判定になる）。

#[path = "common/mod.rs"]
mod common;
#[path = "http_common/mod.rs"]
mod http_common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use wire_server::auth::AUTH_FAILURE_DELAY;
use wire_server::http::session::store::SessionStore;
use wire_server::http::session::token::SessionToken;

use http_common::{
    build_request, error_code_of, error_message_of, parse_single_response, send_raw,
    spawn_router_listener, wire_code_of, AfterWrite, HttpResponse,
};

/// `/v1/session` へログインしてトークン（base64url 表現）を取り出す
/// （`http5_query_bearer.rs::login` と同型）。
fn login(addr: std::net::SocketAddr, user: &str, password: &str) -> String {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);
    let request = build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    );
    let response = parse_single_response(&send_raw(addr, &request, AfterWrite::HalfClose));
    assert_eq!(response.status, 200, "login must succeed");
    let text = std::str::from_utf8(&response.body).expect("utf-8 body");
    let obj = match engine::json::parse_json(text).expect("valid json") {
        engine::json::JsonValue::Object(map) => map,
        other => panic!("expected object, got {other:?}"),
    };
    match obj.get("token") {
        Some(engine::json::JsonValue::String(s)) => s.clone(),
        other => panic!("expected string token field, got {other:?}"),
    }
}

const VALID_SCAN_BODY: &[u8] = br#"{"op":"scan","table":"docs","limit":1}"#;

/// `/v1/query` 宛ての要求（`auth` を与えれば `Authorization` ヘッダを付与）。
fn query_request(auth: Option<&str>, extra_headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut headers: Vec<(&str, &str)> = Vec::new();
    if let Some(auth) = auth {
        headers.push(("Authorization", auth));
    }
    headers.extend_from_slice(extra_headers);
    headers.push(("Content-Type", "application/json"));
    let content_length = body.len().to_string();
    let content_length_leaked: &'static str = Box::leak(content_length.into_boxed_str());
    headers.push(("Content-Length", content_length_leaked));
    build_request("/v1/query", &headers, body)
}

/// `/v1/session` 宛ての誤資格要求（`user`／`password` を任意の値で組み立てる）。
fn session_request(user: &str, password: &str) -> Vec<u8> {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);
    build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &body.len().to_string()),
        ],
        body.as_bytes(),
    )
}

/// `addr` へ `request` を送り、応答の解析結果と往復に要した時間を返す
/// （`AfterWrite::KeepOpen` でサーバー側判断のみによる応答＋クローズを
/// 確認する。`Instant::now()` は送信直前、経過時間は EOF 到達後に確定する）。
fn timed_send(addr: std::net::SocketAddr, request: &[u8]) -> (HttpResponse, Duration) {
    let start = Instant::now();
    let raw = send_raw(addr, request, AfterWrite::KeepOpen);
    let elapsed = start.elapsed();
    (parse_single_response(&raw), elapsed)
}

/// `Date` ヘッダを除いた応答生バイト列を返す（バイト同一性比較用。
/// `http4_session_issue.rs::strip_date_header` と同じ流儀）。
fn strip_date(response_bytes: &[u8]) -> String {
    String::from_utf8_lossy(response_bytes)
        .lines()
        .filter(|line| !line.starts_with("Date: "))
        .collect::<Vec<_>>()
        .join("\n")
}

// --- グループ A: HTTP-6 誤資格の固定遅延と 28P01 ---------------------------

#[test]
fn wrong_password_is_delayed_at_least_auth_failure_delay_and_rejects_with_28p01() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_listener(&users_path, SessionStore::new());

    let (resp, elapsed) = timed_send(addr, &session_request("alice", "wrong"));

    assert!(
        elapsed >= AUTH_FAILURE_DELAY,
        "elapsed {elapsed:?} must be at least the fixed auth failure delay"
    );
    assert_eq!(resp.status, 401);
    assert_eq!(wire_code_of(&resp), "28P01");
    assert_eq!(error_code_of(&resp), "AUTH_INVALID");
    assert_eq!(
        resp.header("WWW-Authenticate"),
        Some("Bearer"),
        "401 response must carry the auth challenge"
    );
}

#[test]
fn unknown_user_is_delayed_at_least_auth_failure_delay_and_rejects_with_28p01() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_listener(&users_path, SessionStore::new());

    let (resp, elapsed) = timed_send(addr, &session_request("bob", "whatever"));

    assert!(
        elapsed >= AUTH_FAILURE_DELAY,
        "elapsed {elapsed:?} must be at least the fixed auth failure delay"
    );
    assert_eq!(resp.status, 401);
    assert_eq!(wire_code_of(&resp), "28P01");
    assert_eq!(error_code_of(&resp), "AUTH_INVALID");
}

#[test]
fn auth_failure_responses_do_not_leak_user_or_tenant_and_use_fixed_message() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_listener(&users_path, SessionStore::new());

    let (wrong_password, _) = timed_send(addr, &session_request("alice", "wrong"));
    let (unknown_user, _) = timed_send(addr, &session_request("bob", "whatever"));

    http_common::assert_message_does_not_echo(&wrong_password, "alice");
    http_common::assert_message_does_not_echo(&wrong_password, "tenant-a");
    http_common::assert_message_does_not_echo(&unknown_user, "bob");

    // 存在オラクル非公開設計: 誤パスワード・未知ユーザーで固定文言が一致する。
    assert_eq!(
        error_message_of(&wrong_password),
        error_message_of(&unknown_user)
    );
}

// --- グループ B: HTTP-6 欠落・不正・期限切れの 28000 と分岐 ----------------

#[test]
fn missing_bearer_on_query_rejects_with_28000_and_closes_connection() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_listener(&users_path, SessionStore::new());

    let request = query_request(None, &[], VALID_SCAN_BODY);
    let raw = send_raw(addr, &request, AfterWrite::KeepOpen);
    let resp = parse_single_response(&raw);

    assert_eq!(resp.status, 401);
    assert_eq!(wire_code_of(&resp), "28000");
    assert_eq!(error_code_of(&resp), "AUTH_REQUIRED");
}

#[test]
fn malformed_bearer_variants_on_query_reject_with_28000() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_listener(&users_path, SessionStore::new());

    let unknown_token = SessionToken::generate().expect("generate unrelated token");
    let variants = [
        "Basic dXNlcjpwYXNz".to_string(),
        "Bearer".to_string(),
        "Bearer AAAA".to_string(),
        format!("Bearer {}", unknown_token.encoded()),
    ];
    for auth in variants {
        let request = query_request(Some(&auth), &[], VALID_SCAN_BODY);
        let resp = parse_single_response(&send_raw(addr, &request, AfterWrite::HalfClose));
        assert_eq!(resp.status, 401, "auth {auth:?}");
        assert_eq!(wire_code_of(&resp), "28000", "auth {auth:?}");
    }

    // `Authorization` 重複ヘッダ（`bearer.rs::BearerError::Duplicate` 経路。
    // `http5_query_bearer.rs` は未カバー）。
    let token = login(addr, "alice", "pw-alice");
    let auth_value = format!("Bearer {token}");
    let duplicate_request = build_request(
        "/v1/query",
        &[
            ("Authorization", &auth_value),
            ("Authorization", &auth_value),
            ("Content-Type", "application/json"),
            ("Content-Length", &VALID_SCAN_BODY.len().to_string()),
        ],
        VALID_SCAN_BODY,
    );
    let resp = parse_single_response(&send_raw(addr, &duplicate_request, AfterWrite::HalfClose));
    assert_eq!(resp.status, 401, "duplicate Authorization header");
    assert_eq!(
        wire_code_of(&resp),
        "28000",
        "duplicate Authorization header"
    );
}

#[test]
fn expired_token_on_query_rejects_with_28000() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);

    // 対照（long TTL）: 同じ fixture でゲートへ到達できることを示す
    // 非 vacuous な証跡。TTL の違いだけが下記の期限切れリスナーとの差分
    // であることを、この 2 リスナー構成そのものが示す。
    let long_ttl_addr = spawn_router_listener(
        &users_path,
        SessionStore::with_limits(4, Duration::from_secs(3600)),
    );
    let long_ttl_token = login(long_ttl_addr, "alice", "pw-alice");
    let long_ttl_auth = format!("Bearer {long_ttl_token}");
    let fresh_request = query_request(Some(&long_ttl_auth), &[], VALID_SCAN_BODY);
    let fresh_resp = parse_single_response(&send_raw(
        long_ttl_addr,
        &fresh_request,
        AfterWrite::HalfClose,
    ));
    assert_eq!(fresh_resp.status, 404, "token must still be valid");
    assert_eq!(wire_code_of(&fresh_resp), "42P01");

    // 期限切れ（TTL=0）: `sessions` はサーバーへ渡すハンドル、`observer` は
    // `Clone` で内部状態を共有するテスト側の観測用ハンドル
    // （`store.rs::SessionStore` は `Arc` 共有。時刻待ちをせず
    // `active_sessions()` の遷移で発行・失効・回収を直接観測する）。
    let sessions = SessionStore::with_limits(4, Duration::ZERO);
    let observer = sessions.clone();
    let addr = spawn_router_listener(&users_path, sessions);
    let token = login(addr, "alice", "pw-alice");
    assert_eq!(
        observer.active_sessions(),
        1,
        "login must issue a session even with TTL=0"
    );
    let auth = format!("Bearer {token}");

    let expired_request = query_request(Some(&auth), &[], VALID_SCAN_BODY);
    let expired_resp =
        parse_single_response(&send_raw(addr, &expired_request, AfterWrite::HalfClose));
    assert_eq!(expired_resp.status, 401);
    assert_eq!(wire_code_of(&expired_resp), "28000");
    assert_eq!(error_code_of(&expired_resp), "AUTH_REQUIRED");
    assert_eq!(
        observer.active_sessions(),
        0,
        "lookup of the expired token must sweep it and release its slot"
    );
}

#[test]
fn closed_token_on_query_rejects_with_28000() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");

    let close_request = build_request(
        "/v1/session/close",
        &[
            ("Authorization", &auth),
            ("Content-Type", "application/json"),
            ("Content-Length", "0"),
        ],
        b"",
    );
    let close_resp = parse_single_response(&send_raw(addr, &close_request, AfterWrite::HalfClose));
    assert_eq!(close_resp.status, 200);

    let query = query_request(Some(&auth), &[], VALID_SCAN_BODY);
    let resp = parse_single_response(&send_raw(addr, &query, AfterWrite::HalfClose));
    assert_eq!(resp.status, 401);
    assert_eq!(wire_code_of(&resp), "28000");
}

#[test]
fn session_and_query_failures_map_to_distinct_wire_codes_28p01_vs_28000() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_listener(&users_path, SessionStore::new());

    let session_resp = parse_single_response(&send_raw(
        addr,
        &session_request("alice", "wrong"),
        AfterWrite::HalfClose,
    ));
    let query_resp = parse_single_response(&send_raw(
        addr,
        &query_request(None, &[], VALID_SCAN_BODY),
        AfterWrite::HalfClose,
    ));

    assert_eq!(session_resp.status, 401);
    assert_eq!(query_resp.status, 401);
    let session_code = wire_code_of(&session_resp);
    let query_code = wire_code_of(&query_resp);
    assert_eq!(session_code, "28P01");
    assert_eq!(query_code, "28000");
    assert_ne!(
        session_code, query_code,
        "session auth failure and query auth failure must map to distinct wire_code values"
    );
}

#[test]
fn missing_malformed_expired_and_closed_responses_are_byte_identical_except_date() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);

    // missing・malformed・closed は long TTL のリスナーで取得する（TTL は
    // 無関係な経路のため干渉させない）。
    let addr = spawn_router_listener(
        &users_path,
        SessionStore::with_limits(4, Duration::from_secs(3600)),
    );

    let missing = send_raw(
        addr,
        &query_request(None, &[], VALID_SCAN_BODY),
        AfterWrite::HalfClose,
    );

    let unknown_token = SessionToken::generate().expect("generate unrelated token");
    let malformed = send_raw(
        addr,
        &query_request(
            Some(&format!("Bearer {}", unknown_token.encoded())),
            &[],
            VALID_SCAN_BODY,
        ),
        AfterWrite::HalfClose,
    );

    let closing_token = login(addr, "alice", "pw-alice");
    let close_auth = format!("Bearer {closing_token}");
    let close_request = build_request(
        "/v1/session/close",
        &[
            ("Authorization", &close_auth),
            ("Content-Type", "application/json"),
            ("Content-Length", "0"),
        ],
        b"",
    );
    let close_resp = parse_single_response(&send_raw(addr, &close_request, AfterWrite::HalfClose));
    assert_eq!(close_resp.status, 200);
    let closed = send_raw(
        addr,
        &query_request(Some(&close_auth), &[], VALID_SCAN_BODY),
        AfterWrite::HalfClose,
    );

    // expired は TTL=0 の別リスナーで取得する（`expired_token_on_query_rejects_with_28000`
    // と同じ方式。時刻待ちをしない）。リスナーをまたいだ比較が成立することを
    // 担保するため、この TTL=0 側でも missing を取得し long TTL 側の missing と
    // バイト同一であることを追加で確認する。
    let expired_addr =
        spawn_router_listener(&users_path, SessionStore::with_limits(4, Duration::ZERO));
    let expiring_token = login(expired_addr, "alice", "pw-alice");
    let expired = send_raw(
        expired_addr,
        &query_request(
            Some(&format!("Bearer {expiring_token}")),
            &[],
            VALID_SCAN_BODY,
        ),
        AfterWrite::HalfClose,
    );
    let missing_on_expired_listener = send_raw(
        expired_addr,
        &query_request(None, &[], VALID_SCAN_BODY),
        AfterWrite::HalfClose,
    );

    let baseline = strip_date(&missing);
    assert_eq!(baseline, strip_date(&malformed));
    assert_eq!(baseline, strip_date(&expired));
    assert_eq!(baseline, strip_date(&closed));
    assert_eq!(
        baseline,
        strip_date(&missing_on_expired_listener),
        "missing-bearer response must be byte identical across listeners with different TTLs"
    );
}

// --- グループ C: HTTP-7 tenant_id 3 位置の 42601 と検査順序 -----------------

#[test]
fn tenant_id_in_json_body_rejects_with_42601_for_all_ops() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");

    let cases: [&[u8]; 6] = [
        br#"{"op":"search","table":"docs","limit":1,"tenant_id":"evil"}"#,
        br#"{"op":"scan","table":"docs","limit":1,"tenant_id":"evil"}"#,
        br#"{"op":"aggregate","table":"docs","aggregates":[],"tenant_id":"evil"}"#,
        br#"{"op":"insert","table":"docs","rows":[],"tenant_id":"evil"}"#,
        br#"{"op":"scan","table":"docs","limit":1,"tenantId":"evil"}"#,
        br#"{"op":"scan","table":"docs","limit":1,"TENANT_ID":"evil"}"#,
    ];
    for body in cases {
        let request = query_request(Some(&auth), &[], body);
        let resp = parse_single_response(&send_raw(addr, &request, AfterWrite::HalfClose));
        assert_eq!(resp.status, 400, "body {body:?}");
        assert_eq!(wire_code_of(&resp), "42601", "body {body:?}");
    }
}

#[test]
fn tenant_id_in_headers_rejects_with_42601() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");

    for header_name in [
        "X-Tenant-Id",
        "Tenant-Id",
        "X-Tenant",
        "TenantId",
        "x-tenant_id",
    ] {
        let request = query_request(Some(&auth), &[(header_name, "other")], VALID_SCAN_BODY);
        let resp = parse_single_response(&send_raw(addr, &request, AfterWrite::HalfClose));
        assert_eq!(resp.status, 400, "header {header_name}");
        assert_eq!(wire_code_of(&resp), "42601", "header {header_name}");
        http_common::assert_message_does_not_echo(&resp, header_name);
        http_common::assert_message_does_not_echo(&resp, "other");
    }
}

#[test]
fn tenant_id_in_path_rejects_with_42601() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");

    for target in [
        "/v1/query?tenant_id=other",
        "/v1/query/tenant_id/other",
        "/v1/query?TENANT-ID=x",
        "/v1/query/tenantid/x",
    ] {
        let request = build_request(
            target,
            &[
                ("Authorization", &auth),
                ("Content-Type", "application/json"),
                ("Content-Length", "0"),
            ],
            b"",
        );
        let resp = parse_single_response(&send_raw(addr, &request, AfterWrite::HalfClose));
        assert_eq!(resp.status, 400, "target {target}");
        assert_eq!(wire_code_of(&resp), "42601", "target {target}");
        http_common::assert_message_does_not_echo(&resp, "other");
        http_common::assert_message_does_not_echo(&resp, "/tenant_id/");
    }
}

#[test]
fn tenant_marker_in_path_is_classified_before_authentication() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_listener(&users_path, SessionStore::new());

    // Bearer 欠落でもパス上の tenant_id マーカーは 42601 として分類される
    // （ルータのパス分類が認証より前。`router.rs::query_target_kind`）。
    let request = build_request(
        "/v1/query?tenant_id=other",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", "0"),
        ],
        b"",
    );
    let resp = parse_single_response(&send_raw(addr, &request, AfterWrite::HalfClose));
    assert_eq!(resp.status, 400);
    assert_eq!(wire_code_of(&resp), "42601");
}

#[test]
fn tenant_header_without_bearer_rejects_with_28000_before_header_check() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_listener(&users_path, SessionStore::new());

    // Bearer 欠落 + tenant ヘッダ: 認証がヘッダ検査より前のため 28000。
    // 未認証クライアントにヘッダ受理挙動を探索させない設計
    // （`middleware.rs` module doc の処理順序契約）。
    let request = query_request(None, &[("X-Tenant-Id", "other")], VALID_SCAN_BODY);
    let resp = parse_single_response(&send_raw(addr, &request, AfterWrite::HalfClose));
    assert_eq!(resp.status, 401);
    assert_eq!(wire_code_of(&resp), "28000");
}

#[test]
fn tenant_header_is_checked_before_body_validation() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");

    let malformed_body = b"not json";
    let request = query_request(Some(&auth), &[("X-Tenant-Id", "other")], malformed_body);
    let resp = parse_single_response(&send_raw(addr, &request, AfterWrite::HalfClose));

    assert_eq!(resp.status, 400);
    assert_eq!(wire_code_of(&resp), "42601");
    assert_eq!(
        error_message_of(&resp),
        wire_server::http::session::middleware::TENANT_HEADER_MESSAGE,
        "tenant header rejection must win over body validation failure"
    );
}

#[test]
fn tenant_id_rejection_does_not_leak_session_tenant() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_router_listener(&users_path, SessionStore::new());
    let token = login(addr, "alice", "pw-alice");
    let auth = format!("Bearer {token}");

    let json_resp = parse_single_response(&send_raw(
        addr,
        &query_request(
            Some(&auth),
            &[],
            br#"{"op":"scan","table":"docs","limit":1,"tenant_id":"evil"}"#,
        ),
        AfterWrite::HalfClose,
    ));
    let header_resp = parse_single_response(&send_raw(
        addr,
        &query_request(Some(&auth), &[("X-Tenant-Id", "other")], VALID_SCAN_BODY),
        AfterWrite::HalfClose,
    ));

    for resp in [&json_resp, &header_resp] {
        http_common::assert_message_does_not_echo(resp, "tenant-a");
        http_common::assert_message_does_not_echo(resp, "alice");
        http_common::assert_message_does_not_echo(resp, &token);
    }
}

// --- グループ D: 実バイナリ経由の非 vacuous 証跡 ---------------------------

/// 実バイナリ `wire-server --surface nosql` を起動し、`main.rs::run_server`
/// の結線を通じて誤資格の固定遅延・`28P01`、続く Bearer 欠落の `28000` が
/// 実際に到達することを確認する（`http4_session_issue.rs` §5.2 と同じ流儀）。
#[test]
fn spawned_binary_enforces_delay_and_28000_over_nosql_surface() {
    let fixture = common::TempFixtureDir::new("http6-auth-failure");
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

    let start = Instant::now();
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream
        .write_all(&session_request("alice", "wrong"))
        .expect("send request");
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
    let session_resp = parse_single_response(&received);

    assert!(
        elapsed >= AUTH_FAILURE_DELAY,
        "elapsed {elapsed:?} must be at least the fixed auth failure delay"
    );
    assert_eq!(session_resp.status, 401);
    assert_eq!(wire_code_of(&session_resp), "28P01");

    let query_resp = parse_single_response(&send_raw(
        addr,
        &query_request(None, &[], VALID_SCAN_BODY),
        AfterWrite::HalfClose,
    ));
    assert_eq!(query_resp.status, 401);
    assert_eq!(wire_code_of(&query_resp), "28000");

    let seen = server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    let joined = seen.join("");
    assert!(
        !joined.contains("tenant-a"),
        "stderr must not leak tenant id"
    );
    assert!(!joined.contains("alice"), "stderr must not leak username");
}
