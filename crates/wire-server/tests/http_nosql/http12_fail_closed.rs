//! NoSQL 表層（`--surface nosql`）の複数接続結合テスト（Issue #749・
//! TASK-173／HTTP-12。関連: `tests/wire_framing.rs` の WIRE-10 相当テスト
//! （SQL wire 版）の HTTP 版）。
//!
//! `http::conn::handle_connection_with`（Issue #747・PR #810）は不正フレーム
//! でも応答を書いてから未読データを有界に読み捨ててクローズする
//! （lingering close）。本ファイルは
//! [`wire_server::http::listener::accept_loop_with_limiter`]（`PlaceholderRouter`
//! 固定。フレーミング層専用のテストヘルパーであり production ルータ経由
//! ではない。モジュール doc「フレーミング層テスト用」節参照）を経由し、
//! 1 本の接続（A）が 3 種の不正フレームのいずれかを送っている最中・直後でも、
//! 別の接続（B）が正常形の要求を完了できることを固定する。
//!
//! ## 「B が成功」の定義（観測境界）
//!
//! 本ファイルは [`wire_server::http::conn::PlaceholderRouter`]（全パス
//! `08P01` 固定）を意図的に使い続ける（production ルータ経由の同型検証は
//! `tests/nosql1_endpoint_routing.rs`〔Issue #758〕が担う）。そのため
//! ここでの「B が成功」は「要求行・ヘッダ・本文の全段を読み切りルータへ
//! 到達した証跡として `message == "unknown request target"` を完全受信し、
//! 最終 read が `Ok(0)`（クリーンな EOF）で終わること」で定義する。
//! production ルータでは `/v1/session`・`/v1/session/close`・`/v1/query`
//! 以外の未知パスも同じく `08P01`（`ROUTER_PLACEHOLDER_MESSAGE` と同一
//! バイト列）のままであり、本ファイルの前提はそのまま成立する
//! （Issue #758 実装記録参照）。
//!
//! ## EOF 判定規約
//!
//! `ConnectionReset`／`BrokenPipe` は「読み捨てずに RST した」失敗として
//! 扱い、`Ok(0)` のみを合格とする（弱体化しない。`tests/http_limits.rs` と
//! 同じ `assert_eof` 方針）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use wire_server::http::headers::MAX_HEADER_SECTION_LEN;
use wire_server::http::request::MAX_REQUEST_LINE_LEN;
use wire_server::limits::ConnectionLimiter;

/// `http::conn::read_head` の内部読み取りバッファ長（`MAX_REQUEST_LINE_LEN`
/// ＋ `MAX_HEADER_SECTION_LEN`。非公開の `conn::HEAD_BUF_LEN` と同じ計算式）。
/// 「読み捨てクローズ」の回帰検出力を保証するため、末尾ゴミバイト列の長さは
/// これを超える必要がある（下記 `http12_malformed_request_line_with_pending_bytes_still_delivers_response`
/// の doc 参照）。
const HEAD_BUF_LEN_FOR_TEST: usize = MAX_REQUEST_LINE_LEN + MAX_HEADER_SECTION_LEN;

/// `accept_loop_with_limiter` をサーバースレッドで起動し、
/// `(接続先アドレス, リミッターのクローン)` を返す（`tests/http_limits.rs`
/// と同じ流儀）。
fn spawn_http_server(
    max_connections: usize,
    read_timeout: Duration,
) -> (std::net::SocketAddr, ConnectionLimiter) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(max_connections);
    let limiter_for_loop = limiter.clone();

    std::thread::spawn(move || {
        wire_server::http::listener::accept_loop_with_limiter(
            listener,
            limiter_for_loop,
            read_timeout,
        );
    });

    (addr, limiter)
}

fn connect(addr: std::net::SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("set write timeout");
    stream
}

/// `stream.read` を EOF（`Ok(0)`）まで読み切り、受信した全バイト列を返す。
/// 途中の `Err`（`WouldBlock`／`TimedOut`／`ConnectionReset` を含む）は
/// すべて panic として顕在化させる（RST を「読み捨て成功」と誤認しない。
/// §「EOF 判定規約」参照）。
fn read_to_clean_eof(stream: &mut TcpStream) -> Vec<u8> {
    let mut received = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => return received,
            Ok(n) => received.extend_from_slice(&buf[..n]),
            Err(e) => panic!(
                "expected clean EOF after reading {} bytes, got read error: {e:?}",
                received.len()
            ),
        }
    }
}

fn wait_for_active_permits(limiter: &ConnectionLimiter, expected: usize, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let active = limiter.active();
        if active >= expected {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {expected} active permits, got {active}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn wait_for_permits_to_drop_to(limiter: &ConnectionLimiter, expected: usize, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let active = limiter.active();
        if active <= expected {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for active permits to drop to {expected}, still {active}"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// 応答バイト列をステータス行・ヘッダ・本文へ分割するテスト専用ミニ
/// パーサ（`crate::http::response` の golden bytes テストと同型。テスト
/// コードのため `unwrap`／`expect` は許容する）。
#[allow(dead_code)]
struct ParsedResponse {
    status_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

fn parse_response(bytes: &[u8]) -> ParsedResponse {
    let sep = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response must contain a blank line separator");
    let head = std::str::from_utf8(&bytes[..sep]).expect("head must be ASCII");
    let body_bytes = &bytes[sep + 4..];
    let body = std::str::from_utf8(body_bytes)
        .expect("body must be utf-8")
        .to_string();

    let mut lines = head.split("\r\n");
    let status_line = lines.next().expect("status line present").to_string();
    let headers: Vec<(String, String)> = lines
        .map(|line| {
            let (name, value) = line.split_once(": ").expect("header must be name: value");
            (name.to_string(), value.to_string())
        })
        .collect();

    let content_length: usize = headers
        .iter()
        .find(|(name, _)| name == "Content-Length")
        .map(|(_, value)| value.parse().expect("Content-Length must be decimal"))
        .expect("Content-Length header present");
    assert_eq!(
        content_length,
        body.len(),
        "Content-Length must match body byte length"
    );

    let connection_close_count = headers
        .iter()
        .filter(|(name, value)| name == "Connection" && value == "close")
        .count();
    assert_eq!(
        connection_close_count, 1,
        "Connection: close must appear exactly once, got: {headers:?}"
    );

    ParsedResponse {
        status_line,
        headers,
        body,
    }
}

/// 本文（JSON）から `error.wire_code`／`error.message` を取り出す。
fn error_fields(body: &str) -> (String, String) {
    let parsed = engine::json::parse_json(body).expect("body must be valid JSON");
    let engine::json::JsonValue::Object(top) = parsed else {
        panic!("body must be a JSON object, got: {parsed:?}");
    };
    let Some(engine::json::JsonValue::Object(error)) = top.get("error") else {
        panic!("body must have an \"error\" object, got: {top:?}");
    };
    let Some(engine::json::JsonValue::String(wire_code)) = error.get("wire_code") else {
        panic!("error object must have a string \"wire_code\", got: {error:?}");
    };
    let Some(engine::json::JsonValue::String(message)) = error.get("message") else {
        panic!("error object must have a string \"message\", got: {error:?}");
    };
    (wire_code.clone(), message.clone())
}

/// A 側（不正フレーム）の応答: `400 Bad Request`・`wire_code == 08P01`・
/// 指定した `message` と一致することを確認する。
fn assert_08p01(bytes: &[u8], expected_message: &str) {
    let parsed = parse_response(bytes);
    assert_eq!(
        parsed.status_line, "HTTP/1.1 400 Bad Request",
        "unexpected status line: {:?} (body: {:?})",
        parsed.status_line, parsed.body
    );
    let (wire_code, message) = error_fields(&parsed.body);
    assert_eq!(
        wire_code, "08P01",
        "unexpected wire_code, body: {:?}",
        parsed.body
    );
    assert_eq!(
        message, expected_message,
        "unexpected message, body: {:?}",
        parsed.body
    );
}

/// B 側（正常形）の応答: `PlaceholderRouter` に到達した証跡として
/// `400`／`08P01`／`"unknown request target"` を確認する（§「B が成功」の
/// 定義」参照。本ファイルは `PlaceholderRouter` 固定のフレーミング層テスト
/// のため、production ルータ導入〔Issue #758〕後もこの期待は反転しない）。
fn assert_router_success(bytes: &[u8]) {
    assert_08p01(bytes, "unknown request target");
}

/// 正常形の要求（`POST /v1/query`・`Content-Type: application/json`）バイト列
/// を組み立てる。
fn well_formed_request(path: &str, body: &str) -> Vec<u8> {
    format!(
        "POST {path} HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A が正常形の要求を送信し、応答を最後まで読み切って `assert_router_success`
/// で検証する（B の役割を担う共通ヘルパ）。
fn run_well_formed_request(addr: std::net::SocketAddr) {
    let mut stream = connect(addr);
    stream
        .write_all(&well_formed_request("/v1/query", "{}"))
        .expect("write well-formed request");
    let response = read_to_clean_eof(&mut stream);
    assert_router_success(&response);
}

/// ケース 1: `Content-Length` 宣言より実送信が少ない状態で A が接続を
/// 保持し続けている最中に、B が接続・送信・完全応答・EOF まで完了する。
/// B の EOF 後も A がまだ進行中（`limiter.active() >= 1`）であることを
/// assert してから A に `shutdown(Write)` し、A も `08P01`（不足方向の
/// `Content-Length` 不一致）を完全受信して `Ok(0)` になることを確認する。
#[test]
fn http12_content_length_shortfall_in_flight_does_not_block_other_connection() {
    let (addr, limiter) = spawn_http_server(4, Duration::from_secs(10));

    // A: 頭で Content-Length: 64 を宣言し、本文 8 バイトだけ送って保持する。
    let mut a = connect(addr);
    let head =
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 64\r\n\r\n";
    a.write_all(head).expect("write A head");
    a.write_all(b"12345678").expect("write A partial body");

    // A のハンドラがブロック中であること（枠を保持している）を観測する。
    wait_for_active_permits(&limiter, 1, Duration::from_secs(5));

    // B: A が進行中の間に完全に完了させる。
    run_well_formed_request(addr);

    // B が完了した後もなお A は進行中であること（同時性の直接的な裏付け）。
    assert!(
        limiter.active() >= 1,
        "connection A must still be in flight after B completed"
    );

    // A を完結させる: 書き込み側を閉じ、宣言長との不一致（不足）による
    // 08P01 応答を完全受信する。
    a.shutdown(std::net::Shutdown::Write)
        .expect("shutdown A write half");
    let a_response = read_to_clean_eof(&mut a);
    assert_08p01(&a_response, "invalid request");

    wait_for_permits_to_drop_to(&limiter, 0, Duration::from_secs(5));
}

/// ケース 2: 要求行が構文不正（メソッド・バージョンとも非対応）かつ末尾に
/// ゴミバイト列（`HEAD_BUF_LEN_FOR_TEST` 超過長）を同一書き込みで付加し
/// `shutdown(Write)` しない状態で応答を待つ。未読データが残ったまま
/// 応答→drain→close する経路（読み捨てクローズ。PoC-15）を非 vacuous に
/// 踏ませ、A が応答全体を `Ok(0)` まで受信できること（RST が発生しない
/// こと）を確認する。A の送信直後に B を完了させ、その後 A を読む
/// （逐次ではない到達順序）。
///
/// ゴミバイト列の長さを `read_head` の内部バッファ長（`HEAD_BUF_LEN`＝
/// `HEAD_BUF_LEN_FOR_TEST`）より確実に大きくしている（数バイトの
/// マージンではなく 1 バッファ分丸ごと超過させる）。`read_head` は
/// 要求行が `Malformed` と判定でき次第ループを抜けるが、判定前の
/// `stream.read` 呼び出し自体は「バッファの空き全体」を読み取り範囲として
/// 渡すため、ゴミが `HEAD_BUF_LEN_FOR_TEST` 以下だと最初の 1 回の read で
/// カーネル受信バッファの中身をすべて読み切ってしまい得る（実装は正しい
/// まま、未読データが 1 バイトも残らない）。その場合 drain-and-close の
/// 読み捨てループは空ソケットを待つだけになり、drain 処理自体を省略・
/// 破損させる回帰が起きてもこのテストは変わらず pass してしまう
/// （検出力が無い）。ゴミを `HEAD_BUF_LEN_FOR_TEST` 超過長にすることで、
/// `read_head` 側の read がどれだけ多くのバイトを一括で読めても
/// バッファ容量（`HEAD_BUF_LEN_FOR_TEST`）を超えては読めない以上、
/// 超過分は構造的にカーネル受信バッファに残り続け、drain が実際に
/// 未読バイトを読み捨てる経路を毎回確実に踏む。
#[test]
fn http12_malformed_request_line_with_pending_bytes_still_delivers_response() {
    let (addr, limiter) = spawn_http_server(4, Duration::from_secs(10));

    let mut a = connect(addr);
    // メソッド `GET`（`POST` 以外）・バージョン `HTTP/1.0`（非対応）の
    // いずれも `parse_request_line` を `Malformed` にする形状。CRLF は
    // 含むため `Incomplete` へは倒れない。
    let mut payload = b"GET / HTTP/1.0\r\n".to_vec();
    // `read_head` の読み取りバッファ（`HEAD_BUF_LEN_FOR_TEST`）を
    // 丸ごと超過させ、未読データが drain 対象として必ず残ることを保証する
    // （doc 参照）。
    payload.extend(std::iter::repeat_n(b'x', HEAD_BUF_LEN_FOR_TEST + 4096));
    a.write_all(&payload)
        .expect("write malformed request line with trailing garbage");
    // `shutdown(Write)` しない: 未読データが残った状態のまま応答を待つ。

    // A の送信直後に B を完了させる（同時性）。
    run_well_formed_request(addr);

    let a_response = read_to_clean_eof(&mut a);
    assert_08p01(&a_response, "invalid message frame");

    wait_for_permits_to_drop_to(&limiter, 0, Duration::from_secs(5));
}

/// ケース 3: 未知パスへの `POST`（要求行・ヘッダ・本文は正常形。末尾ゴミ
/// なし）が `PlaceholderRouter` の固定応答（`08P01`／`unknown request
/// target`）で拒否される。A の送信 → B 完了 → A 読み取りの順序で、A の
/// 応答待ちが B を妨げないことを確認する。
#[test]
fn http12_unknown_path_post_is_rejected_while_other_connection_completes() {
    let (addr, limiter) = spawn_http_server(4, Duration::from_secs(10));

    let mut a = connect(addr);
    a.write_all(&well_formed_request("/definitely/unknown", "{}"))
        .expect("write unknown-path request");

    run_well_formed_request(addr);

    let a_response = read_to_clean_eof(&mut a);
    // `PlaceholderRouter` はパスを問わず一律 `unknown request target`。
    assert_router_success(&a_response);

    wait_for_permits_to_drop_to(&limiter, 0, Duration::from_secs(5));
}

/// ケース 4: ケース 1〜3 相当の不正フレームに加え「何も送らず切断」を
/// 逐次で流し、最後に正常形の要求（B 役）が成功することを確認する
/// （`tests/wire_framing.rs` の WIRE-10 複数接続テストと同型の逐次回帰）。
#[test]
fn http12_sequential_malformed_connections_leave_server_healthy() {
    let (addr, limiter) = spawn_http_server(4, Duration::from_secs(10));

    // 1) Content-Length 不足。
    {
        let mut a = connect(addr);
        let head = b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 16\r\n\r\n";
        a.write_all(head).expect("write head");
        a.write_all(b"ab").expect("write partial body");
        a.shutdown(std::net::Shutdown::Write)
            .expect("shutdown write half");
        let response = read_to_clean_eof(&mut a);
        assert_08p01(&response, "invalid request");
    }

    // 2) 要求行の構文不正。
    {
        let mut a = connect(addr);
        a.write_all(b"BOGUS / HTTP/9.9\r\n\r\n")
            .expect("write malformed request line");
        let response = read_to_clean_eof(&mut a);
        assert_08p01(&response, "invalid message frame");
    }

    // 3) 未知パスへの POST。
    {
        let mut a = connect(addr);
        a.write_all(&well_formed_request("/nope", "{}"))
            .expect("write unknown-path request");
        let response = read_to_clean_eof(&mut a);
        assert_router_success(&response);
    }

    // 4) 何も送らず切断（HTTP-11: 無応答で良い）。
    {
        let a = connect(addr);
        drop(a);
    }

    // 最後に正常形の要求が成功すること。
    run_well_formed_request(addr);

    wait_for_permits_to_drop_to(&limiter, 0, Duration::from_secs(5));
}

/// ケース 5: ケース 1〜3 それぞれの終了後に `limiter.active()` が 0 へ戻る
/// こと（枠が確実に解放されること）を個別に固定する。
#[test]
fn http12_permits_are_released_after_malformed_connections() {
    let (addr, limiter) = spawn_http_server(4, Duration::from_secs(10));

    // Content-Length 不足。
    {
        let mut a = connect(addr);
        let head = b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 16\r\n\r\n";
        a.write_all(head).expect("write head");
        a.write_all(b"ab").expect("write partial body");
        wait_for_active_permits(&limiter, 1, Duration::from_secs(5));
        a.shutdown(std::net::Shutdown::Write)
            .expect("shutdown write half");
        let _ = read_to_clean_eof(&mut a);
    }
    wait_for_permits_to_drop_to(&limiter, 0, Duration::from_secs(5));

    // 要求行の構文不正。
    {
        let mut a = connect(addr);
        a.write_all(b"BOGUS / HTTP/9.9\r\n\r\n")
            .expect("write malformed request line");
        let _ = read_to_clean_eof(&mut a);
    }
    wait_for_permits_to_drop_to(&limiter, 0, Duration::from_secs(5));

    // 未知パスへの POST。
    {
        let mut a = connect(addr);
        a.write_all(&well_formed_request("/nope", "{}"))
            .expect("write unknown-path request");
        let _ = read_to_clean_eof(&mut a);
    }
    wait_for_permits_to_drop_to(&limiter, 0, Duration::from_secs(5));
}
