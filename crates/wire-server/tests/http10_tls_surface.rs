//! NoSQL 表層（HTTP/1.1 最小サブセット）への TLS 層の接続（Issue #968・
//! 親 #941・TASK-228。対象ビヘイビア WIRE-9・HTTP-9・HTTP-10）。
//!
//! 層 A（本ファイル・`cargo test`・`make ci` 対象）: ephemeral port で
//! `wire_server::http::listener::accept_loop_with_router_tls` を in-process
//! サーバースレッドとして起動し、`tests/common/tls_client.rs`（公開 API
//! `wire_server::tls::*` のみを使う最小 TLS 1.3 クライアント）で実際に TLS
//! ハンドシェイクを駆動したうえで、`POST /v1/session` → `POST /v1/query` →
//! `POST /v1/session/close` の往復を検証する。実バイナリ経由の CLI 組合せ
//! 検証（`--surface nosql` × `--tls-cert`／`--tls-key`／`--tls-mode`）は
//! `tests/wire_tls_cli.rs::tls_with_nosql_surface_serves_https` が担う（本
//! ファイルは接続処理の中身、`wire_tls_cli.rs` は CLI 結線の外形。役割分担は
//! 既存の `wire_tls_connection.rs`（接続処理）／`wire_tls_cli.rs`（CLI）と
//! 同じ）。
//!
//! - TLS-1: TLS ハンドシェイク → `/v1/session`（200・token）→ `/v1/query`
//!   （スローアウェイ engine 上の `scan` で `42P01`／404 到達。TLS 上でも
//!   「認証 → op 許可リスト → engine 呼び出し」まで到達する非 vacuous な
//!   証跡）→ `/v1/session/close`（200）が完走すること
//! - TLS-2: `--tls-mode require` 相当の下で、TLS ハンドシェイクを経ない
//!   平文 HTTP 接続は要求を解釈されずに応答なしで閉じられること（H2）
//! - TLS-3: `--tls-mode allow` 相当の下で、平文 HTTP・TLS の両方が
//!   `/v1/session` を完走できること
//! - TLS-4: 不正な ClientHello（`0x16` の後にゴミバイト列）を送っても
//!   サーバーが panic せず、応答なしで閉じたうえで後続の正常な TLS 接続には
//!   波及しないこと
//! - TLS-5: `--tls-mode allow` 下で同時接続数上限を超過した平文 HTTP
//!   クライアントに、TLS 未構成時と同じ既存の 503／`53300` 応答が返る
//!   こと（codex-review 指摘・Issue #968 是正。`tls.is_some()` のみで
//!   無応答クローズしていた回帰の再発防止）
//! - TLS-6: `--tls-mode require` 下で同時接続数上限を超過した接続は
//!   （平文であっても）要求を解釈されずに応答なしで閉じられること（H4）
//! - TLS-7: TLS レコード 1 個分の暗号文を 1 バイトずつ送り続けても、要求
//!   読み取りの絶対期限が本番値の近傍で効くこと（`http::deadline_stream`
//!   の H8 対応の回帰）
//! - TLS-8: `--tls-mode allow` 下で同時接続数上限を超過した接続の先頭
//!   バイトが TLS レコードでも、ハンドシェイクを完了したうえで既存の
//!   503／`53300` 応答が TLS 上で返ること（codex-review 再指摘・Issue
//!   #968 是正。旧実装はハンドシェイクをせず無応答クローズしており、
//!   `allow` の下でも HTTPS クライアントだけがこのエラー契約から
//!   取り残されていた）

#[path = "http_common/mod.rs"]
mod http_common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use engine::core::EngineCore;
use engine::json::JsonValue;
use wire_server::http::router::Router;
use wire_server::http::session::store::SessionStore;
use wire_server::limits::ConnectionLimiter;
use wire_server::tls::server_handshake::TlsServerConfig;
use wire_server::tls_opt::TlsMode;

#[path = "common/tls_client.rs"]
mod tls_client;

/// `wire_server::http::listener::accept_loop_with_router_tls`（production
/// 入口。Issue #968）を in-process サーバースレッドで起動し、接続先
/// アドレスを返す。`tls_client::test_config()` と同じ鍵材料
/// （RFC8032_TEST1）を使うため、`tls_client::drive_client_handshake_over_socket`
/// でそのままハンドシェイクを駆動できる（`wire_tls_connection.rs` と同じ
/// 流儀）。`engine` はテーブルを一切持たないスローアウェイ `EngineCore`
/// （TLS-1 の `scan` が `42P01`／404 へ到達することの非 vacuous な証跡用）。
fn spawn_router_listener_tls(users_path: &std::path::Path, mode: TlsMode) -> std::net::SocketAddr {
    spawn_router_listener_tls_with_limiter(
        users_path,
        mode,
        ConnectionLimiter::new(wire_server::limits::MAX_CONNECTIONS),
    )
}

/// [`spawn_router_listener_tls`] の、呼び出し元が [`ConnectionLimiter`] を
/// 構築して渡せる版。同時接続数上限超過（`limiter.try_acquire()` が
/// `None` を返す経路。TLS-5・TLS-6）を決定的に再現するため、テスト側で
/// 容量 1 の `ConnectionLimiter` を渡し、枠を保持したまま 2 本目を接続する。
fn spawn_router_listener_tls_with_limiter(
    users_path: &std::path::Path,
    mode: TlsMode,
    limiter: ConnectionLimiter,
) -> std::net::SocketAddr {
    spawn_router_listener_tls_with_limiter_and_read_timeout(
        users_path,
        mode,
        limiter,
        wire_server::limits::READ_TIMEOUT,
    )
}

/// [`spawn_router_listener_tls_with_limiter`] の、要求読み取りの絶対期限
/// （`read_timeout`）も呼び出し元が指定できる版。TLS-7（トリクル送信下でも
/// 絶対期限が本番の 30 秒より大幅に延びないことの回帰確認。`http::
/// deadline_stream` の H8 対応）を現実的な時間で検証するために使う。
fn spawn_router_listener_tls_with_limiter_and_read_timeout(
    users_path: &std::path::Path,
    mode: TlsMode,
    limiter: ConnectionLimiter,
    read_timeout: Duration,
) -> std::net::SocketAddr {
    let store = wire_server::auth::UserStore::load_from_file(users_path).expect("valid store");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let core_path = http_common::temp_db::unique_db_path("http10-tls-surface-throwaway");
    let core = EngineCore::open(&core_path).expect("open throwaway engine core");
    let sessions = SessionStore::new();
    let router = Router::with_engine(Arc::new(store), sessions, Arc::new(core));
    let tls_config: Arc<TlsServerConfig> = tls_client::test_config();

    std::thread::spawn(move || {
        wire_server::http::listener::accept_loop_with_router_tls(
            listener,
            limiter,
            read_timeout,
            router,
            tls_config,
            mode,
        );
    });

    addr
}

/// `wire-server hash-password` サブコマンドをバイナリの子プロセスとして
/// 起動せず、`wire_server::auth::argon2id::encode_phc`（公開 API）を直接
/// 呼んでユーザーストアを組み立てる（`tests/wire_auth.rs`・
/// `tests/http4_session_issue.rs` 等と同じ流儀。層 A は in-process が既定
/// 方針であり、子プロセス起動が必要な検証は `wire_tls_cli.rs` 側に任せる）。
/// `wire_tls_cli.rs::write_user_store_with_alice` と同じ資格情報
/// （`alice`／`pw-alice`／`tenant-a`）を使う。
fn write_user_store_with_alice(path: &std::path::Path) {
    let salt = b"0123456789abcdef";
    let phc = wire_server::auth::argon2id::encode_phc(
        b"pw-alice",
        salt,
        &wire_server::auth::argon2id::RECOMMENDED_PARAMS,
    )
    .expect("valid phc encoding");
    std::fs::write(path, format!("alice:tenant-a:{phc}\n")).expect("write user store");
}

/// TLS 接続 1 本を確立し、`(TestClient, TcpStream)` ではなく即座に
/// [`tls_client::TlsTestChannel`]（`Read`＋`Write`）へ包んで返す。
fn connect_tls(addr: std::net::SocketAddr) -> tls_client::TlsTestChannel {
    let mut socket = TcpStream::connect(addr).expect("connect");
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set write timeout");
    let client = tls_client::drive_client_handshake_over_socket(&mut socket);
    tls_client::TlsTestChannel::new(client, socket)
}

/// 受信済みバイト列がヘッダ終端 `\r\n\r\n`・`Content-Length` を含む完全な
/// 1 応答になっていれば、その総バイト長を返す（`channel.read` を「相手が
/// `close_notify` を送るまで」ではなく「必要な分だけ」呼ぶために使う。
/// `TlsTestChannel::read` はレコード境界で `close_notify` に到達すると
/// `read_application_data` が panic するため、応答受信後に追加で `read` を
/// 呼ばない設計にする必要がある）。
fn complete_response_len(bytes: &[u8]) -> Option<usize> {
    let sep = bytes.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(bytes.get(..sep)?).ok()?;
    let content_length: usize = head
        .split("\r\n")
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .and_then(|v| v.parse().ok())?;
    Some(sep + 4 + content_length)
}

/// [`complete_response_len`] を使い、TLS チャネル上で 1 応答ぶんちょうど
/// 読み切る（EOF・`close_notify` には依存しない）。
fn read_one_response(channel: &mut impl Read) -> Vec<u8> {
    let mut received = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Some(total) = complete_response_len(&received) {
            if received.len() >= total {
                return received;
            }
        }
        let n = channel.read(&mut buf).expect("read response chunk");
        assert!(
            n > 0,
            "connection closed before a complete response arrived"
        );
        received.extend_from_slice(&buf[..n]);
    }
}

fn build_request(target: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"POST ");
    out.extend_from_slice(target.as_bytes());
    out.extend_from_slice(b" HTTP/1.1\r\n");
    for (name, value) in headers {
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(b": ");
        out.extend_from_slice(value.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

struct ParsedResponse {
    status: u16,
    body: JsonValue,
}

fn parse_response(bytes: &[u8]) -> ParsedResponse {
    let sep = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header/body terminator present");
    let head = std::str::from_utf8(&bytes[..sep]).expect("utf-8 head");
    let mut lines = head.split("\r\n");
    let status_line = lines.next().expect("status line present");
    let status: u16 = status_line
        .split(' ')
        .nth(1)
        .expect("status code present")
        .parse()
        .expect("numeric status code");
    assert!(
        head.lines()
            .any(|l| l.eq_ignore_ascii_case("Connection: close")),
        "every NoSQL response must carry Connection: close, got head: {head:?}"
    );
    let body_str = std::str::from_utf8(&bytes[sep + 4..]).expect("utf-8 body");
    let body = engine::json::parse_json(body_str).expect("valid json body");
    ParsedResponse { status, body }
}

fn json_str<'a>(value: &'a JsonValue, key: &str) -> &'a str {
    match value {
        JsonValue::Object(map) => match map.get(key) {
            Some(JsonValue::String(s)) => s.as_str(),
            other => panic!("expected string field {key:?}, got {other:?}"),
        },
        other => panic!("expected object, got {other:?}"),
    }
}

/// エラー応答の `{"error":{"wire_code":"...", ...}}` エンベロープから
/// `wire_code` を取り出す（`crate::http::conn` の単体テストと同じ形）。
fn error_wire_code(value: &JsonValue) -> &str {
    let error = match value {
        JsonValue::Object(map) => map.get("error").expect("error key present"),
        other => panic!("expected object, got {other:?}"),
    };
    json_str(error, "wire_code")
}

// --- TLS-1: TLS 上で session → query → close が完走する -------------------

#[test]
fn tls_session_query_close_round_trip_completes_over_tls() {
    let fixture_dir = std::env::temp_dir().join(format!(
        "wire-server-http10-tls-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir(&fixture_dir).expect("create fixture dir");
    let users_path = fixture_dir.join("users.txt");
    write_user_store_with_alice(&users_path);

    let addr = spawn_router_listener_tls(&users_path, TlsMode::Require);

    // 1 本目の TLS 接続: ログインしてトークンを取得する。
    let mut channel = connect_tls(addr);
    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let request = build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        login_body,
    );
    channel.write_all(&request).expect("write session request");
    let response = read_one_response(&mut channel);
    let parsed = parse_response(&response);
    assert_eq!(parsed.status, 200, "login must succeed over TLS");
    let token = json_str(&parsed.body, "token").to_string();

    // 2 本目の TLS 接続（NoSQL は 1 応答＝1 接続）: `/v1/query` で
    // 「認証 → op 許可リスト → engine 呼び出し」に到達した非 vacuous な
    // 証跡（スローアウェイ engine 上の scan は `42P01`／404）を確認する。
    let mut channel = connect_tls(addr);
    let query_body = br#"{"op":"scan","table":"docs","limit":1}"#;
    let request = build_request(
        "/v1/query",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &query_body.len().to_string()),
            ("Authorization", &format!("Bearer {token}")),
        ],
        query_body,
    );
    channel.write_all(&request).expect("write query request");
    let response = read_one_response(&mut channel);
    let parsed = parse_response(&response);
    assert_eq!(
        parsed.status, 404,
        "query against throwaway engine must reach the engine (42P01)"
    );
    assert_eq!(error_wire_code(&parsed.body), "42P01");

    // 3 本目の TLS 接続: セッションを明示的に失効させる。
    let mut channel = connect_tls(addr);
    let request = build_request(
        "/v1/session/close",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", "0"),
            ("Authorization", &format!("Bearer {token}")),
        ],
        b"",
    );
    channel.write_all(&request).expect("write close request");
    let response = read_one_response(&mut channel);
    let parsed = parse_response(&response);
    assert_eq!(parsed.status, 200, "session close must succeed over TLS");

    let _ = std::fs::remove_dir_all(&fixture_dir);
}

// --- TLS-2: require 下で平文 HTTP は応答なしで閉じられる ------------------

#[test]
fn plaintext_http_is_closed_without_response_when_tls_required() {
    let fixture_dir = std::env::temp_dir().join(format!(
        "wire-server-http10-tls-r2-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir(&fixture_dir).expect("create fixture dir");
    let users_path = fixture_dir.join("users.txt");
    write_user_store_with_alice(&users_path);

    let addr = spawn_router_listener_tls(&users_path, TlsMode::Require);

    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let request = build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        login_body,
    );
    // 書き込み自体は成功しうる（サーバーは先頭 1 バイトを peek した時点で
    // 平文と判定し、要求本体を読まずに閉じるため）。
    let _ = stream.write_all(&request);

    let mut buf = [0u8; 8];
    match stream.read(&mut buf) {
        Ok(0) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => {
            panic!("expected no HTTP response over plaintext when tls-mode=require, got {other:?}")
        }
    }

    let _ = std::fs::remove_dir_all(&fixture_dir);
}

// --- TLS-3: allow 下では平文・TLS のいずれでも完走する --------------------

#[test]
fn allow_mode_accepts_both_plaintext_and_tls() {
    let fixture_dir = std::env::temp_dir().join(format!(
        "wire-server-http10-tls-r3-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir(&fixture_dir).expect("create fixture dir");
    let users_path = fixture_dir.join("users.txt");
    write_user_store_with_alice(&users_path);

    let addr = spawn_router_listener_tls(&users_path, TlsMode::Allow);
    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let request = build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        login_body,
    );

    // TLS 経路。
    let mut channel = connect_tls(addr);
    channel.write_all(&request).expect("write over tls");
    let response = read_one_response(&mut channel);
    assert_eq!(
        parse_response(&response).status,
        200,
        "tls login must succeed under allow"
    );

    // 平文経路。
    let mut stream = TcpStream::connect(addr).expect("connect plaintext");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream.write_all(&request).expect("write plaintext request");
    let mut received = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(e) => panic!("unexpected read error on plaintext path: {e:?}"),
        }
    }
    assert_eq!(
        parse_response(&received).status,
        200,
        "plaintext login must also succeed under allow"
    );

    let _ = std::fs::remove_dir_all(&fixture_dir);
}

// --- TLS-4: 不正な ClientHello で他接続へ波及しない ------------------------

#[test]
fn malformed_client_hello_does_not_crash_or_affect_later_connections() {
    let fixture_dir = std::env::temp_dir().join(format!(
        "wire-server-http10-tls-r4-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir(&fixture_dir).expect("create fixture dir");
    let users_path = fixture_dir.join("users.txt");
    write_user_store_with_alice(&users_path);

    let addr = spawn_router_listener_tls(&users_path, TlsMode::Require);

    // `0x16`（TLS ハンドシェイクレコード）の後にゴミバイト列を送る。
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let mut garbage = vec![0x16u8, 0x03, 0x03, 0x00, 0x05];
    garbage.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF, 0x00]);
    let _ = stream.write_all(&garbage);
    // ハンドシェイク driver は不正なレコードに対し alert を送ってから
    // 切断しうる（`docs/design/tls-wire-connection.md` 参照）。ここでは
    // panic せず有限時間内に読み取りが終わる（0 バイト・alert・
    // `ConnectionReset` のいずれか）ことだけを確認する。HTTP 応答（明示的な
    // 平文の `08P01` 等）が返らないことが要点。
    let mut buf = [0u8; 64];
    let _ = stream.read(&mut buf);
    drop(stream);

    // 後続の正常な TLS 接続は影響を受けずに完走できること。
    let mut channel = connect_tls(addr);
    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let request = build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        login_body,
    );
    channel.write_all(&request).expect("write over tls");
    let response = read_one_response(&mut channel);
    assert_eq!(
        parse_response(&response).status,
        200,
        "a later well-formed TLS connection must still succeed"
    );

    let _ = std::fs::remove_dir_all(&fixture_dir);
}

// --- TLS-5/6: 同時接続数上限超過時のエラー契約（Issue #968 codex-review
//     P1 是正）---------------------------------------------------------------

/// 容量 1 の `ConnectionLimiter` を渡し、1 本目の接続で枠を保持したまま
/// 2 本目を接続することで `limiter.try_acquire()` が `None` を返す経路を
/// 決定的に再現する。`listener.rs::accept_loop_with_handler` の
/// `active() >= 1` を待ってから 2 本目を送るため、フレーク要因（受理前に
/// 2 本目が先着する）を排除する。
fn wait_for_permit_active(limiter: &ConnectionLimiter) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while limiter.active() < 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for permit"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// TLS-5: `--tls-mode allow` の下で同時接続数上限を超過した平文 HTTP
/// クライアントには、TLS 未構成時と同じ既存の 503／`wire_code` 53300
/// 応答が返ること（`tls.is_some()` のみで無応答クローズしていた回帰の
/// 再発防止）。
#[test]
fn allow_mode_still_returns_503_for_plaintext_over_capacity() {
    let fixture_dir = std::env::temp_dir().join(format!(
        "wire-server-http10-tls-r5-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir(&fixture_dir).expect("create fixture dir");
    let users_path = fixture_dir.join("users.txt");
    write_user_store_with_alice(&users_path);

    let limiter = ConnectionLimiter::new(1);
    let addr = spawn_router_listener_tls_with_limiter(&users_path, TlsMode::Allow, limiter.clone());

    // 1 本目: 枠を保持し続ける（TLS 接続を確立し、何も送らない）。
    let _holder = connect_tls(addr);
    wait_for_permit_active(&limiter);

    // 2 本目（平文）: 上限超過で拒否されるはず。`allow` 下の判定は先頭
    // バイトの `peek` に依存するため、request-line を送ってから読む
    // （`accept_loop_with_limiter_rejects_connection_over_capacity_with_503`
    // の TLS 未構成版と異なり、何も送らない接続は判定に必要なバイトが
    // 届かず `peek` がタイムアウトして無応答クローズ側に倒れる）。
    let mut rejected = TcpStream::connect(addr).expect("connect rejected");
    rejected
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let request = build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        login_body,
    );
    let _ = rejected.write_all(&request);
    let mut received = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        match rejected.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&received);
    assert!(
        text.starts_with("HTTP/1.1 503 "),
        "allow mode must still return 503 for plaintext over capacity, got: {text:?}"
    );
    assert!(text.contains("53300"), "got: {text:?}");

    let _ = std::fs::remove_dir_all(&fixture_dir);
}

/// TLS-6: `--tls-mode require` の下で同時接続数上限を超過した接続は
/// （平文であっても）要求を解釈されずに応答なしで閉じられること（H4）。
#[test]
fn require_mode_closes_over_capacity_connection_without_response() {
    let fixture_dir = std::env::temp_dir().join(format!(
        "wire-server-http10-tls-r6-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir(&fixture_dir).expect("create fixture dir");
    let users_path = fixture_dir.join("users.txt");
    write_user_store_with_alice(&users_path);

    let limiter = ConnectionLimiter::new(1);
    let addr =
        spawn_router_listener_tls_with_limiter(&users_path, TlsMode::Require, limiter.clone());

    // 1 本目: 枠を保持し続ける（TLS 接続を確立し、何も送らない）。
    let _holder = connect_tls(addr);
    wait_for_permit_active(&limiter);

    // 2 本目（平文）: `require` 下では上限超過時も応答なしで閉じられる。
    let mut rejected = TcpStream::connect(addr).expect("connect rejected");
    rejected
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let mut buf = [0u8; 8];
    match rejected.read(&mut buf) {
        Ok(0) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => {
            panic!("expected no HTTP response over capacity when tls-mode=require, got {other:?}")
        }
    }

    let _ = std::fs::remove_dir_all(&fixture_dir);
}

/// TLS-8: `--tls-mode allow` の下で同時接続数上限を超過した接続の先頭
/// バイトが TLS レコードでも、ハンドシェイクを完了したうえで既存の
/// 503／`wire_code` 53300 応答が TLS 上で返ること（codex-review 再指摘・
/// Issue #968 是正。`reject_or_close_over_limit` が旧実装のまま TLS
/// レコード判定時にハンドシェイクをせず無応答クローズしていると、
/// `connect_tls`（クライアント側ハンドシェイク駆動）がハンドシェイク
/// 完了を待てず失敗し、本テストは fail する）。
#[test]
fn allow_mode_returns_503_over_tls_for_tls_over_capacity() {
    let fixture_dir = std::env::temp_dir().join(format!(
        "wire-server-http10-tls-r8-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir(&fixture_dir).expect("create fixture dir");
    let users_path = fixture_dir.join("users.txt");
    write_user_store_with_alice(&users_path);

    let limiter = ConnectionLimiter::new(1);
    let addr = spawn_router_listener_tls_with_limiter(&users_path, TlsMode::Allow, limiter.clone());

    // 1 本目: 枠を保持し続ける（TLS 接続を確立し、何も送らない）。
    let _holder = connect_tls(addr);
    wait_for_permit_active(&limiter);

    // 2 本目（TLS）: 上限超過だが先頭バイトが TLS レコードのため、
    // `reject_or_close_over_limit` がハンドシェイクを完了してから 503 を
    // 返すはず（`connect_tls` 自体がクライアント側ハンドシェイクを完走
    // させるため、ここまで到達した時点でサーバー側ハンドシェイクの完了は
    // 既に確認済み）。
    let mut channel = connect_tls(addr);
    let request = build_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", "0"),
        ],
        b"",
    );
    channel.write_all(&request).expect("write request over tls");
    let response = read_one_response(&mut channel);
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.starts_with("HTTP/1.1 503 "),
        "allow mode must return 503 over TLS for TLS-record over capacity, got: {text:?}"
    );
    assert!(text.contains("53300"), "got: {text:?}");

    let _ = std::fs::remove_dir_all(&fixture_dir);
}

/// TLS-7: TLS レコード 1 個分の暗号文を 1 バイトずつ小さな間隔で送り続けて
/// も、要求読み取りの絶対期限（Slowloris 対策）が本番の値の近傍で正しく
/// 効くこと（`http::deadline_stream::DeadlineStream` の H8 対応の回帰。
/// 単体テスト `http::deadline_stream::tests::
/// repeated_reads_are_bounded_by_absolute_deadline_despite_trickle` の
/// TLS 実プロトコル版）。`DeadlineStream` が無ければ
/// `TlsStream::fill_from_inner` の内部ループが `read_timeout` を使い回すため、
/// 接続は絶対期限を大きく超えて（トリクル間隔 × レコードバイト数）保持
/// されてしまう。
#[test]
fn tls_trickle_within_one_record_is_bounded_by_absolute_read_deadline() {
    let fixture_dir = std::env::temp_dir().join(format!(
        "wire-server-http10-tls-r7-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos()
    ));
    std::fs::create_dir(&fixture_dir).expect("create fixture dir");
    let users_path = fixture_dir.join("users.txt");
    write_user_store_with_alice(&users_path);

    let read_deadline = Duration::from_millis(150);
    let addr = spawn_router_listener_tls_with_limiter_and_read_timeout(
        &users_path,
        TlsMode::Allow,
        ConnectionLimiter::new(wire_server::limits::MAX_CONNECTIONS),
        read_deadline,
    );

    let mut socket = TcpStream::connect(addr).expect("connect");
    socket
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set write timeout");
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    let mut client = tls_client::drive_client_handshake_over_socket(&mut socket);

    // request-line 相当のアプリケーションデータを 1 個の TLS レコードへ
    // 封をした生バイト列を組み立てる（`tls_client::send_application_data`
    // は `write_all` で一括送出するため使わず、ここでは意図的に 1 バイトずつ
    // 送る）。中身は正規のリクエストである必要はない（サーバーはヘッダを
    // 読み切る前に期限切れで閉じるはずなので、パース結果は検証しない）。
    let payload = b"POST /v1/session HTTP/1.1\r\n";
    let records = client
        .sealer
        .seal_fragmented(
            wire_server::tls::record::ContentType::ApplicationData,
            payload,
        )
        .expect("valid seal");
    let mut wire_bytes = Vec::new();
    for record in &records {
        record
            .serialize_into(
                &mut wire_bytes,
                wire_server::tls::record::RecordKind::Ciphertext,
            )
            .expect("serialize application data record");
    }

    let started = std::time::Instant::now();
    let sender = std::thread::spawn(move || {
        for byte in wire_bytes {
            if socket.write_all(&[byte]).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        // 送信し切った後も socket の生死判定は呼び出し元に委ねる（サーバーが
        // 途中で閉じていれば `write_all` は途中で失敗するため無視する）。
        socket
    });

    // サーバー側が期限切れで接続を閉じたことは、最終的に read が `Ok(0)`
    // （TCP FIN）または接続エラーになることで確認する。`CloseSilently`
    // 経路は `graceful_close`（TLS では `close_notify` 送出。H7）を経て
    // から `shutdown_both` するため、閉じる直前に `close_notify` アラート
    // レコードの生バイト列が 1 回分届きうる（本テストは復号しない生読み
    // のため、これも「まだデータが届いた」に見える）。したがって単発の
    // `read` では判定せず、`Ok(0)`／エラーに到達するまで読み進める。
    let mut probe_socket = sender.join().expect("sender thread");
    let mut buf = [0u8; 16];
    let closed = loop {
        match probe_socket.read(&mut buf) {
            Ok(0) => break true,
            Ok(_) => continue,
            Err(_) => break true,
        }
    };
    let elapsed = started.elapsed();

    assert!(
        closed,
        "connection should be closed once the absolute read deadline is exceeded"
    );
    // トリクル間隔（60ms）× レコードのバイト数（約 49 バイト）では
    // 3 秒近くになるが、`DeadlineStream` があれば絶対期限（150ms）+
    // 十分な許容誤差以内で閉じられているはず。`DeadlineStream` が無い
    // 回帰では、レコード全体を送り切るまで（3 秒近く）閉じられない。
    assert!(
        elapsed < read_deadline + Duration::from_secs(1),
        "absolute read deadline was not enforced over TLS trickle: elapsed={elapsed:?}"
    );

    let _ = std::fs::remove_dir_all(&fixture_dir);
}
