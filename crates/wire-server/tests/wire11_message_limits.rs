//! 拡張クエリプロトコルの各メッセージ（Parse／Bind／Describe／Execute／Close／
//! Sync）に対する 1 メッセージ長上限（WIRE-4）と読み取りタイムアウト（WIRE-5）の
//! 適用確認（Issue #1178・WIRE-11）。ポインタ: `docs/spec/04-behavior/wire-protocol.md`
//! WIRE-4・WIRE-5・WIRE-11。
//!
//! 簡易クエリ側の同契約は `wire_framing.rs`・`wire_limits.rs` が固定済みのため、
//! 本ファイルは拡張クエリ経路（`extended_query` ハンドラ経由と、Sync 待ち破棄
//! 状態〔ignore_till_sync〕で `handshake::post_auth_loop` が直接検証する経路）でも
//! 「超過は `54000` を返して切断」「無通信・部分フレームは応答せず切断（枠解放）」
//! が成り立つことを、生バイトの wire クライアントで固定する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::auth::UserStore;
use wire_server::framing::MAX_MESSAGE_LEN;
use wire_server::limits::ConnectionLimiter;

use common::*;

/// 読み取りタイムアウトの検証で使う短い上限（既定 30 秒を待たないため）。
const SHORT_READ_TIMEOUT: Duration = Duration::from_millis(300);

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire11-limits");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `read_timeout` を指定して in-process サーバーを起動する
/// （`common::spawn_server_with_engine` は 5 秒固定のため本ファイルで持つ）。
fn spawn_with_read_timeout(
    core: Arc<EngineCore>,
    read_timeout: Duration,
) -> (SocketAddr, ConnectionLimiter) {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = Arc::new(UserStore::load_from_file(&users_path).expect("valid user store"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);
    let limiter_for_loop = limiter.clone();
    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            core,
            limiter_for_loop,
            read_timeout,
        );
    });
    (addr, limiter)
}

fn connect(addr: SocketAddr) -> TcpStream {
    authenticate_to_ready_for_query(addr, "alice", "correct-horse")
}

/// 型バイトと `MAX_MESSAGE_LEN + 1` の宣言長だけを送る（本文は送らない。
/// `wire_framing.rs` と同じく RST 競合を避けて決定的にするため）。
fn send_oversized_header(stream: &mut TcpStream, type_byte: u8) {
    let declared = i32::try_from(MAX_MESSAGE_LEN + 1).expect("fits i32");
    let mut msg = vec![type_byte];
    msg.extend_from_slice(&declared.to_be_bytes());
    stream.write_all(&msg).expect("send oversized header");
}

fn assert_oversized_rejected(type_byte: u8) {
    let (core, _guard) = new_core();
    let (addr, _limiter) = spawn_with_read_timeout(core, Duration::from_secs(5));
    let mut stream = connect(addr);
    send_oversized_header(&mut stream, type_byte);
    expect_error_response_with_sqlstate(&mut stream, "54000");
    expect_connection_closed(&mut stream);
}

fn send_sync(stream: &mut TcpStream) {
    send_length_prefixed_message(stream, b'S', &[]);
}

/// 未定義 statement への Bind で回復可能エラーを起こし、ErrorResponse を読み
/// 取って ignore_till_sync 状態にする。
fn enter_ignore_till_sync(stream: &mut TcpStream) {
    send_length_prefixed_message(stream, b'B', &bind_body("", "no_such_statement"));
    let (ty, _) = read_message(stream);
    assert_eq!(ty, b'E', "expected ErrorResponse for undefined statement");
}

#[test]
fn wire11_oversized_parse_frame_returns_54000_then_closes() {
    assert_oversized_rejected(b'P');
}

#[test]
fn wire11_oversized_describe_frame_returns_54000_then_closes() {
    assert_oversized_rejected(b'D');
}

#[test]
fn wire11_oversized_bind_frame_returns_54000_then_closes() {
    assert_oversized_rejected(b'B');
}

#[test]
fn wire11_oversized_execute_frame_returns_54000_then_closes() {
    assert_oversized_rejected(b'E');
}

#[test]
fn wire11_oversized_close_frame_returns_54000_then_closes() {
    assert_oversized_rejected(b'C');
}

/// Sync は固定長 4 の本文経路で読まれるが、宣言長の超過は `08P01` ではなく
/// `54000` になる。
#[test]
fn wire11_oversized_sync_frame_returns_54000_then_closes() {
    assert_oversized_rejected(b'S');
}

/// ignore_till_sync 中の超過は `post_auth_loop` が直接検証する別経路。
#[test]
fn wire11_oversized_frame_during_ignore_till_sync_returns_54000_then_closes() {
    let (core, _guard) = new_core();
    let (addr, _limiter) = spawn_with_read_timeout(core, Duration::from_secs(5));
    let mut stream = connect(addr);
    enter_ignore_till_sync(&mut stream);
    send_oversized_header(&mut stream, b'B');
    expect_error_response_with_sqlstate(&mut stream, "54000");
    expect_connection_closed(&mut stream);
}

/// 宣言長がちょうど `MAX_MESSAGE_LEN` の Parse はフレーム超過として切断され
/// ない。SQL 層のガードで回復可能エラーになりうるため、判定基準は SQLSTATE
/// ではなく「接続が維持され Sync に ReadyForQuery が返ること」。
#[test]
fn wire11_parse_frame_at_exact_limit_is_not_rejected_as_frame_too_large() {
    let (core, _guard) = new_core();
    let (addr, _limiter) = spawn_with_read_timeout(core, Duration::from_secs(10));
    let mut stream = connect(addr);

    // 本文 = 名前 "" + NUL + クエリ + NUL + パラメータ数(2 バイト)。
    // クエリはトークン間の空白で長さを合わせる。
    let body_len = MAX_MESSAGE_LEN - 4;
    let head = "SELECT id FROM docs";
    let tail = " LIMIT 1";
    let fixed = 1 + head.len() + tail.len() + 1 + 2;
    let pad = body_len - fixed;
    let query = format!("{head}{}{tail}", " ".repeat(pad));
    let body = parse_body("", &query, 0);
    assert_eq!(body.len(), body_len);
    send_length_prefixed_message(&mut stream, b'P', &body);
    send_sync(&mut stream);

    loop {
        let (ty, _) = read_message(&mut stream);
        if ty == b'Z' {
            break;
        }
        assert!(
            ty == b'1' || ty == b'E',
            "unexpected message type {ty} before ReadyForQuery"
        );
    }
}

/// 応答バイトが 1 つも無いまま EOF になり、接続枠が解放されることを確認する。
/// 退行時に無限待ちにならないようクライアント側にも保険のタイムアウトを置く。
fn assert_closed_silently(stream: &mut TcpStream, limiter: &ConnectionLimiter) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set client read timeout");
    std::thread::sleep(SHORT_READ_TIMEOUT * 3);
    let mut buf = [0u8; 16];
    let n = stream
        .read(&mut buf)
        .expect("read must observe EOF, not time out");
    assert_eq!(n, 0, "no response bytes expected before close");
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(limiter.active(), 0, "connection slot must be released");
}

#[test]
fn wire11_partial_parse_frame_is_closed_without_response() {
    let (core, _guard) = new_core();
    let (addr, limiter) = spawn_with_read_timeout(core, SHORT_READ_TIMEOUT);
    let mut stream = connect(addr);
    // 宣言長 64 に対して本文の先頭数バイトだけで停止する。
    let mut msg = vec![b'P'];
    msg.extend_from_slice(&64i32.to_be_bytes());
    msg.extend_from_slice(b"\0SEL");
    stream.write_all(&msg).expect("send partial frame");
    assert_closed_silently(&mut stream, &limiter);
}

#[test]
fn wire11_idle_after_parse_before_sync_is_closed_without_response() {
    let (core, _guard) = new_core();
    let (addr, limiter) = spawn_with_read_timeout(core, SHORT_READ_TIMEOUT);
    let mut stream = connect(addr);
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("", "SELECT id FROM docs LIMIT 1", 0),
    );
    let (ty, _) = read_message(&mut stream);
    assert_eq!(ty, b'1', "ParseComplete expected");
    assert_closed_silently(&mut stream, &limiter);
}

#[test]
fn wire11_idle_during_ignore_till_sync_is_closed_without_response() {
    let (core, _guard) = new_core();
    let (addr, limiter) = spawn_with_read_timeout(core, SHORT_READ_TIMEOUT);
    let mut stream = connect(addr);
    enter_ignore_till_sync(&mut stream);
    assert_closed_silently(&mut stream, &limiter);
}
