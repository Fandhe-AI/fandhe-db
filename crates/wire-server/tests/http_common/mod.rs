//! HTTP 表層（`--surface nosql`）の結合テスト共通ヘルパー（Issue #748・
//! TASK-173／HTTP-2, HTTP-3, HTTP-11）。
//!
//! `tests/common/mod.rs` は pg wire（SSLRequest／Startup）を前提にした
//! ヘルパー群であり、HTTP 表層のテスト（本ファイル・`http2_framing.rs`・
//! `http3_content_type.rs`・`http11_limits.rs`、および以後 HTTP 表層を扱う
//! テスト全般）には流用できない。本モジュールはそれらが共通して必要とする
//! 「リスナー起動・生バイト送受信・応答エンベロープ解析・`wire_code`／
//! `message` 抽出・拒否応答のアサーション」を 1 箇所へ集約する
//! （`#[path = "http_common/mod.rs"] mod http_common;` で include する）。
//!
//! 本モジュールのヘルパーが起動する入口は
//! [`wire_server::http::listener::accept_loop_with_limiter`] であり、これは
//! 内部で [`wire_server::http::conn::PlaceholderRouter`]（`pub(crate)`。
//! 全パスを `08P01`／`"unknown request target"` で拒否する固定応答）を
//! 使う。これはフレーミング層（要求行・ヘッダ・本文検証）のテストを
//! production ルータ（[`wire_server::http::router::Router`]。Issue #752・
//! #758 で 3 エンドポイント限定化まで実装済み）から意図的に切り離すための
//! 設計であり、`PlaceholderRouter` は撤去しない。production ルータの未知
//! パス応答は `conn::unknown_target_response` を共有するため
//! `PlaceholderRouter` と同一バイト列になる（Issue #758）。
//! したがって「要求が受理されパースを通ってハンドラへ到達した」ことは、
//! 本モジュール経由では常にこの固定応答としてしか観測できない。
//! [`assert_reached_router`] はその観測境界を 1 箇所へ閉じ込めている。
//!
//! 対応: TASK-173（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア HTTP-2,
//! HTTP-3, HTTP-11）。
#![allow(dead_code)]

use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use engine::core::EngineCore;
use engine::json::{parse_json, JsonValue};
use wire_server::limits::ConnectionLimiter;

/// engine 側のテスト専用一時 DB ヘルパー（`unique_db_path`。`std` のみに
/// 依存し `crate::` を一切参照しないため取り込み可能。`crates/wire-server/
/// tests/wire_aggregate.rs` 等の既存結合テストと同じ流儀。本ファイルは
/// 実ディレクトリ `tests/http_common/` の直下（`mod.rs` 相当）のため、
/// ネストしたモジュール宣言に対応する非実在ディレクトリを介さず `..` を
/// 辿れる）。`pub` にして `http_common` を取り込む側のテストファイルが
/// スローアウェイ `EngineCore` 用に再利用できるようにする（同じファイルを
/// 別の `#[path]` で二重に取り込むと `clippy::duplicate_mod` に抵触する
/// ため、取り込み元は本モジュールを再利用し、独自の `mod temp_db;` を
/// 追加で宣言しないこと）。
#[path = "../../../engine/src/test_util/temp_db.rs"]
pub mod temp_db;

/// クライアント側ソケットの読み取り／書き込みタイムアウト。
///
/// production の既定（`crate::limits::READ_TIMEOUT`＝30 秒）を待つ必要はなく、
/// テストの意図（不正フレーム・上限超過の即時拒否）に対して十分に大きい
/// 短縮値を使う。
pub const CLIENT_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// サーバー側（[`spawn_http_listener`] へ渡す `read_timeout`）の既定値。
/// [`crate::http::conn::handle_connection_with`] の `request_read_deadline`
/// としても使われる（`accept_loop_with_limiter` が同じ値を両方へ渡す契約）。
pub const SERVER_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// [`send_raw`] が読み取る応答バイト列の総量上限。無制限 `Vec` 伸長を避ける
/// （untrusted な相手からの応答を読むテストヘルパー自身も無制限確保をしない）。
const MAX_RECEIVE_BYTES: usize = 2 * 1024 * 1024;

/// `wire_server::http::listener::accept_loop_with_limiter`（production 入口。
/// 内部で [`wire_server::http::conn::PlaceholderRouter`] を固定で使う）を
/// サーバースレッドで起動し、`(接続先アドレス, リミッターのクローン)` を返す。
///
/// `tests/http_limits.rs::spawn_http_server` と同じ流儀（Issue #743）。
pub fn spawn_http_listener(
    max_connections: usize,
    read_timeout: Duration,
) -> (SocketAddr, ConnectionLimiter) {
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

/// `wire_server::http::listener::accept_loop_with_router`（production 入口。
/// [`wire_server::http::router::Router`] を使う。`/v1/session`・
/// `/v1/session/close` を実ハンドラへディスパッチする）をサーバースレッドで
/// 起動し、接続先アドレスを返す。
///
/// `http4_session_issue.rs::spawn_router_server`（Issue #752）と同じ流儀の
/// 昇格版（Issue #753。`/v1/session/close` の結合テスト・#754 以降も本関数を
/// 再利用する想定）。`sessions` は呼び出し元が上限・TTL を制御できるよう
/// `SessionStore` をそのまま受け取る。
///
/// `core`（クエリ実行）は `scan` op の束縛・実行が結線された後（TASK-186・
/// NOSQL-3・Issue #766）はテーブルを一切持たないスローアウェイ
/// `EngineCore` を内部で構築して渡す。`table: "docs"` への `scan` はこの
/// スローアウェイ core 上では `SqlSurfaceError::UndefinedTable`
/// （`42P01`／404）となるため、「認証 → op 許可リスト → スキーマ検証 →
/// engine 呼び出し」が最後まで走ったことの非 vacuous な証跡が
/// [`assert_reached_query_gate`] として得られる（データを investigate
/// したいテストは [`spawn_router_listener_with_engine`] を使うこと）。
pub fn spawn_router_listener(
    users_path: &std::path::Path,
    sessions: wire_server::http::session::store::SessionStore,
) -> SocketAddr {
    let path = temp_db::unique_db_path("query-gate-throwaway");
    let core = EngineCore::open(&path).expect("open throwaway engine core");
    // プロセス寿命の accept スレッドへ move するため Drop ガードを使えない。open 直後に
    // unlink して残置を防ぐ（Issue #1303。非 unix は残置する文書化された例外）。
    temp_db::unlink_open_db_file(&path);
    spawn_router_listener_with_engine(users_path, sessions, std::sync::Arc::new(core))
}

/// [`spawn_router_listener`] と同一の production 入口
/// （[`wire_server::http::router::Router`]）を、呼び出し元が用意した
/// `engine`（データを事前に seed 済みの `EngineCore` 等）を接続した状態
/// （`Router::with_engine` 経由。Issue #766・TASK-176・NOSQL-3・Issue #768・
/// TASK-177・NOSQL-4）で起動する。`POST /v1/query`（`op: scan`／
/// `op: aggregate`）が実行可能なリスナーを起動する。
pub fn spawn_router_listener_with_engine(
    users_path: &std::path::Path,
    sessions: wire_server::http::session::store::SessionStore,
    engine: std::sync::Arc<EngineCore>,
) -> SocketAddr {
    let store = wire_server::auth::UserStore::load_from_file(users_path).expect("valid store");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(wire_server::limits::MAX_CONNECTIONS);
    let router = wire_server::http::router::Router::with_engine(
        std::sync::Arc::new(store),
        sessions,
        engine,
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

/// 応答受信後にクライアントがソケットへ対して何をするか。
///
/// `HalfClose`（既定・推奨）は書き込み側を即座に閉じ、サーバーの
/// `drain_and_close` が即座に EOF へ到達できるようにする（drain の
/// `LINGER_DRAIN_TIMEOUT`＝1 秒待ちを避け、テストを高速に保つ）。
/// `KeepOpen` は「クライアントが接続を切らなくてもサーバー側の判断だけで
/// 応答＋クローズへ到達すること」を確認したいケース（本文読み取り前の
/// 上限拒否・パイプライン・本文長超過等）でのみ使う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterWrite {
    HalfClose,
    KeepOpen,
}

/// `addr` へ接続し `request` を書き込んでから、[`AfterWrite`] に応じて
/// 書き込み側を閉じ（またはそのまま）、EOF まで応答バイト列を読み取って返す。
///
/// 終端は `Ok(0)`（クリーンな EOF）を正とする。サーバー側が未読データを
/// 残したまま `close` する経路（drain 予算超過・タイムアウト）では OS が
/// close を RST として観測しうる（`crate::http::conn` 自身の単体テスト
/// `assert_closed_without_hanging` と同じ事情）ため、少なくとも 1 バイト
/// 受信済みであれば `ConnectionReset` も終端として許容する。`WouldBlock`／
/// `TimedOut`（クライアント側の読み取りタイムアウト超過）は許容せず
/// 明示的な失敗とする（「サーバーが応答してクローズする」契約が壊れていても
/// テストが通ってしまう事態を避けるため。`tests/http_limits.rs::assert_eof`
/// と同じ codex-review 由来の方針）。
pub fn send_raw(addr: SocketAddr, request: &[u8], after: AfterWrite) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("connect to http listener");
    stream
        .set_read_timeout(Some(CLIENT_READ_TIMEOUT))
        .expect("set client read timeout");
    stream
        .set_write_timeout(Some(CLIENT_READ_TIMEOUT))
        .expect("set client write timeout");
    stream.write_all(request).expect("write request bytes");
    if after == AfterWrite::HalfClose {
        stream
            .shutdown(Shutdown::Write)
            .expect("half-close write side after sending request");
    }

    let mut received = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                received.extend_from_slice(&buf[..n]);
                assert!(
                    received.len() <= MAX_RECEIVE_BYTES,
                    "response exceeded test receive cap of {MAX_RECEIVE_BYTES} bytes"
                );
            }
            Err(e) if e.kind() == ErrorKind::ConnectionReset && !received.is_empty() => break,
            Err(e) => panic!("unexpected read error while waiting for response/EOF: {e:?}"),
        }
    }
    received
}

/// `POST <target> HTTP/1.1` の要求バイト列を組み立てる。`headers` は宣言順で
/// そのまま出力する（大文字小文字・重複の検証はしない。テストが意図的に
/// 不正なヘッダ列を組み立てられるようにするため）。
pub fn build_request(target: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
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

/// `/v1/query` 宛ての整形済み要求（`Content-Type: application/json`・
/// 実際の `body` 長に一致する `Content-Length`）を組み立てる便宜 API。
pub fn well_formed_request(body: &[u8]) -> Vec<u8> {
    let content_length = body.len().to_string();
    build_request(
        "/v1/query",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &content_length),
        ],
        body,
    )
}

/// 解析済み HTTP 応答（1 個）。`Date` ヘッダは応答ごとに変わるため golden
/// bytes 比較ではなく、本構造体へ解析したうえで個別フィールドを検証する。
#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub reason: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    /// ヘッダ名を ASCII 大文字小文字非区別で照合し、最初に一致した値を返す。
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// `bytes` の中から `needle` の最初の出現位置を返す（ヘッダ／本文の境界
/// `\r\n\r\n` を探すための小さな純関数。テストコードのため `unwrap`/`expect`
/// は許容するが、対象がユーザー入力ではなくテスト自身が受信したバイト列
/// であり、境界が見つからないのは応答の不整合＝テスト失敗として扱ってよい）。
fn find_subslice(bytes: &[u8], needle: &[u8]) -> Option<usize> {
    bytes.windows(needle.len()).position(|w| w == needle)
}

/// `send_raw` が返した応答バイト列（ちょうど 1 応答ぶんであること・
/// エンベロープの妥当性を含む）を解析する。
///
/// 検証する不変条件（いずれか 1 つでも破れれば panic でテスト失敗にする）:
/// - `HTTP/1.1 <3桁> <reason>\r\n` で始まる
/// - ヘッダ部が `\r\n\r\n` で終端する
/// - `Content-Length` ヘッダが存在し、本文の残余バイト長と一致する
///   （＝本文の後に余剰バイトが無い。1 応答＝1 接続の証跡）
/// - `Connection: close` が存在する
/// - `Content-Type` が [`wire_server::http::response::CONTENT_TYPE_JSON_UTF8`]
///   と一致する
pub fn parse_single_response(bytes: &[u8]) -> HttpResponse {
    let sep = find_subslice(bytes, b"\r\n\r\n")
        .unwrap_or_else(|| panic!("response missing header/body terminator: {bytes:?}"));
    let head = bytes.get(..sep).expect("head slice in range");
    let body = bytes.get(sep + 4..).expect("body slice in range");

    let head_str = std::str::from_utf8(head).expect("response head must be valid utf-8");
    let mut lines = head_str.split("\r\n");
    let status_line = lines.next().expect("response missing status line");
    let mut parts = status_line.splitn(3, ' ');
    let http_version = parts.next().expect("status line missing http version");
    assert_eq!(
        http_version, "HTTP/1.1",
        "unexpected http version in status line: {status_line:?}"
    );
    let status: u16 = parts
        .next()
        .expect("status line missing status code")
        .parse()
        .expect("status code must be a 3-digit number");
    let reason = parts
        .next()
        .expect("status line missing reason phrase")
        .to_string();

    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .unwrap_or_else(|| panic!("header line missing colon: {line:?}"));
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }

    let response = HttpResponse {
        status,
        reason,
        headers,
        body: body.to_vec(),
    };

    let content_length: usize = response
        .header("content-length")
        .expect("response missing content-length header")
        .parse()
        .expect("content-length header must be numeric");
    assert_eq!(
        content_length,
        response.body.len(),
        "content-length does not match trailing byte count (no surplus bytes after body)"
    );
    assert_eq!(
        response.header("connection"),
        Some("close"),
        "response missing Connection: close"
    );
    assert_eq!(
        response.header("content-type"),
        Some(wire_server::http::response::CONTENT_TYPE_JSON_UTF8),
        "unexpected content-type header"
    );

    response
}

/// `resp.body` を `{"error":{"wire_code":...,"code":...,"message":...}}` として
/// 解析し `wire_code` を返す。
pub fn wire_code_of(resp: &HttpResponse) -> String {
    error_field(resp, "wire_code")
}

/// [`wire_code_of`] と同じ枠組みで `message` を取り出す。
pub fn error_message_of(resp: &HttpResponse) -> String {
    error_field(resp, "message")
}

/// [`wire_code_of`] と同じ枠組みで `code`（`AUTH_INVALID`／`AUTH_REQUIRED` 等の
/// アプリケーション側分類名）を取り出す（Issue #757・HTTP-6）。
pub fn error_code_of(resp: &HttpResponse) -> String {
    error_field(resp, "code")
}

fn error_field(resp: &HttpResponse, field: &str) -> String {
    let body_str = std::str::from_utf8(&resp.body).expect("error body must be valid utf-8");
    let parsed = parse_json(body_str)
        .unwrap_or_else(|_| panic!("error body must be valid json: {body_str:?}"));
    let JsonValue::Object(mut top) = parsed else {
        panic!("error body top level must be an object: {body_str:?}");
    };
    let error_value = top
        .remove("error")
        .unwrap_or_else(|| panic!("error body missing \"error\" key: {body_str:?}"));
    let JsonValue::Object(error_obj) = error_value else {
        panic!("error body \"error\" value must be an object: {body_str:?}");
    };
    match error_obj.get(field) {
        Some(JsonValue::String(s)) => s.clone(),
        other => panic!("error body \"error.{field}\" must be a string, got {other:?}"),
    }
}

/// `expected_status`／`expected_wire_code` の応答本文であることだけを検証
/// する（[`parse_single_response`] の不変条件を含む）。[`PlaceholderRouter`]
/// の固定応答（`400`／`08P01`）と偶然一致する場合も通してしまうため、
/// フレーミング層の拒否を主張したいテストからは直接呼ばず
/// [`assert_rejected`] を使うこと（[`PlaceholderRouter`]: `wire_server::
/// http::conn::PlaceholderRouter`）。
fn assert_status_and_wire_code(
    resp: &HttpResponse,
    expected_status: u16,
    expected_wire_code: &str,
) {
    assert_eq!(
        resp.status,
        expected_status,
        "unexpected status (body: {:?})",
        String::from_utf8_lossy(&resp.body)
    );
    let wire_code = wire_code_of(resp);
    assert_eq!(
        wire_code,
        expected_wire_code,
        "unexpected wire_code (body: {:?})",
        String::from_utf8_lossy(&resp.body)
    );
}

/// 本モジュール経由（`accept_loop_with_limiter`＋[`wire_server::http::conn::
/// PlaceholderRouter`]）で唯一到達できるハンドラの固定応答文言。production
/// ルータ（[`wire_server::http::router::Router`]）の未知パス応答も
/// `conn::unknown_target_response` を共有するため同一バイト列になる
/// （Issue #758）。
pub const ROUTER_PLACEHOLDER_MESSAGE: &str = "unknown request target";

/// 応答が `expected_status`／`expected_wire_code` の拒否応答であることを
/// 検証する（[`parse_single_response`] の不変条件を含む）。
///
/// `PlaceholderRouter` の固定応答も `400`／`08P01` を返すため、本文だけの
/// 一致では「フレーミング層で本当に拒否された」ことと「フレーミング層を
/// 素通りしてルータへ到達し、たまたま同じ応答形状になった」ことを区別
/// できない（後者は検証対象の入力検証が抜け落ちる退行を検出できなくする）。
/// そのため `message` が [`ROUTER_PLACEHOLDER_MESSAGE`] と異なることも
/// あわせて固定し、フレーミング層の拒否であることを保証する。
pub fn assert_rejected(resp: &HttpResponse, expected_status: u16, expected_wire_code: &str) {
    assert_status_and_wire_code(resp, expected_status, expected_wire_code);
    let message = error_message_of(resp);
    assert_ne!(
        message,
        ROUTER_PLACEHOLDER_MESSAGE,
        "response reached PlaceholderRouter instead of being rejected by the framing layer          (body: {:?})",
        String::from_utf8_lossy(&resp.body)
    );
}

/// 「要求がパースを通ってハンドラへ到達した」ことを検証する（本 Issue 時点は
/// [`ROUTER_PLACEHOLDER_MESSAGE`] を返す `PlaceholderRouter` のみが production
/// ハンドラのため、到達＝この固定応答になる。モジュール doc 参照）。
pub fn assert_reached_router(resp: &HttpResponse) {
    assert_status_and_wire_code(resp, 400, "08P01");
    assert_eq!(error_message_of(resp), ROUTER_PLACEHOLDER_MESSAGE);
}

/// `op: "scan"` の最小要求本文。[`spawn_router_listener`] のスローアウェイ
/// `EngineCore`（テーブル未作成）へ送ると「認証 → op 許可リスト → スキーマ
/// 検証 → engine 呼び出し」が最後まで走ったうえで `table: "docs"` が
/// 未存在と判定される（TASK-186・NOSQL-3・Issue #766 で `scan` が実行結線
/// されたため、他 3 op と異なりもう暫定 `0A000`／501 を返さない）。
pub const GATE_PROBE_BODY: &[u8] = br#"{"op":"scan","table":"docs","limit":1}"#;

/// [`GATE_PROBE_BODY`] を [`spawn_router_listener`] のスローアウェイ
/// `EngineCore` へ送った応答が「認証・op 許可リスト・スキーマ検証を通過し
/// engine まで到達したこと」の非 vacuous な証跡（`42P01`／404。SQL 経路の
/// 未存在テーブル判定と同一分類）であることを検証する。`aggregate`／
/// `search`（実行結線済み）・`insert`（後続 Issue で結線予定）が
/// [`GATE_PROBE_BODY`] とは別の要求本文で呼び出されても、本アサーションは
/// `42P01`／404 という結果の形のみを見るため影響を受けない。
pub fn assert_reached_query_gate(resp: &HttpResponse) {
    assert_status_and_wire_code(resp, 404, "42P01");
}

/// 応答本文（`message` フィールド・生バイト列の双方）に `marker` が含まれて
/// いないことを確認する。要求内容・内部詳細がそのままクライアントへ
/// 反映されないこと（`.claude/rules/security.md` の非漏えい方針）を機械的に
/// 断定するための補助。
pub fn assert_message_does_not_echo(resp: &HttpResponse, marker: &str) {
    let message = error_message_of(resp);
    assert!(
        !message.contains(marker),
        "response message unexpectedly echoed marker {marker:?}: {message:?}"
    );
    let body_str = String::from_utf8_lossy(&resp.body);
    assert!(
        !body_str.contains(marker),
        "response body unexpectedly echoed marker {marker:?}: {body_str:?}"
    );
}
