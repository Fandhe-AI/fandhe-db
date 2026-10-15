//! TLS 上の pg wire 接続の結合テスト（Issue #966・TASK-228・WIRE-9 ポインタ。
//! `wire_server::server::accept_loop_with_tls`／`handshake::
//! handle_connection_with_options` の受入基準を検証する）。
//!
//! `tests/common/tls_client.rs`（`tests/tls_server_handshake.rs` の
//! `TestClient`／`drive_client_handshake_over_socket` と同じ構成要素を
//! 独立に持つ最小 TLS 1.3 クライアント）で実ハンドシェイクを駆動し、
//! StartupMessage・認証・簡易クエリを TLS レコード越しに送受信する。
//! 実クライアント（psql／openssl s_client）での疎通は #969 の担当。

#[path = "common/mod.rs"]
mod common;
#[path = "common/tls_client.rs"]
mod tls_client;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

const SSL_REQUEST_CODE: i32 = 80_877_103;
const GSSENC_REQUEST_CODE: i32 = 80_877_104;

fn write_ssl_request(stream: &mut TcpStream) {
    let mut msg = Vec::new();
    msg.extend_from_slice(&8i32.to_be_bytes());
    msg.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
    stream.write_all(&msg).expect("send SSLRequest");
}

fn write_startup_message(stream: &mut impl Write, username: &str, database: &str) {
    let mut params = Vec::new();
    params.extend_from_slice(b"user\0");
    params.extend_from_slice(username.as_bytes());
    params.push(0);
    params.extend_from_slice(b"database\0");
    params.extend_from_slice(database.as_bytes());
    params.push(0);
    params.push(0);
    let total_len = (4 + 4 + params.len()) as i32;
    let mut startup = Vec::new();
    startup.extend_from_slice(&total_len.to_be_bytes());
    startup.extend_from_slice(&0x0003_0000i32.to_be_bytes());
    startup.extend_from_slice(&params);
    stream.write_all(&startup).expect("send StartupMessage");
}

fn write_typed_message(stream: &mut impl Write, type_byte: u8, body: &[u8]) {
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::with_capacity(1 + 4 + body.len());
    msg.push(type_byte);
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(body);
    stream.write_all(&msg).expect("write typed message");
}

fn read_exact_n(stream: &mut impl Read, n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    stream.read_exact(&mut buf).expect("read exact");
    buf
}

/// 型バイト付きメッセージ 1 個を読む（`(type_byte, body)`）。
fn read_typed_message(stream: &mut impl Read) -> (u8, Vec<u8>) {
    let type_byte = read_exact_n(stream, 1)[0];
    let len_bytes = read_exact_n(stream, 4);
    let len = i32::from_be_bytes(len_bytes.try_into().expect("4 bytes")) as usize;
    let body_len = len.checked_sub(4).expect("length includes itself");
    let body = read_exact_n(stream, body_len);
    (type_byte, body)
}

/// AuthenticationOk 以降（BackendKeyData・ParameterStatus*）を読み飛ばし、
/// ReadyForQuery（'Z'）に到達するまで進める。
fn drain_to_ready_for_query(stream: &mut impl Read) {
    loop {
        let (type_byte, _body) = read_typed_message(stream);
        if type_byte == b'Z' {
            return;
        }
    }
}

fn spawn_tls_server(users_path: &std::path::Path) -> std::net::SocketAddr {
    spawn_tls_server_with_read_timeout(users_path, Duration::from_secs(5))
}

fn spawn_tls_server_with_read_timeout(
    users_path: &std::path::Path,
    read_timeout: Duration,
) -> std::net::SocketAddr {
    let store = Arc::new(UserStore::load_from_file(users_path).expect("valid user store"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);
    let tls = tls_client::test_config();

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_tls(
            listener,
            store,
            None,
            Some(tls),
            limiter,
            read_timeout,
        );
    });

    addr
}

/// 受入基準 2: TLS 未設定時は既存どおり `'N'` を返し、平文接続がそのまま
/// 使えること（`accept_loop_with_limiter` と同じ経路。回帰確認）。
#[test]
fn tls_disabled_server_still_declines_ssl_with_n() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let store = Arc::new(UserStore::load_from_file(&users_path).expect("valid user store"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);
    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_limiter(
            listener,
            store,
            limiter,
            Duration::from_secs(5),
        );
    });

    let mut stream = TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut stream);
    let resp = read_exact_n(&mut stream, 1);
    assert_eq!(&resp, b"N", "TLS-disabled server must decline SSL with 'N'");
}

/// 受入基準 1・2: TLS opt-in 時に `SSLRequest` へ `'S'` を返し、ハンドシェイク
/// → TLS 上の StartupMessage → cleartext 認証 → 簡易クエリ往復まで完走する
/// こと。`engine` は未接続（`None`）のため簡易クエリ応答は `0A000` になるが、
/// これは平文接続の `engine: None` 経路と同一の契約であり、TLS レコード上で
/// 認証後シーケンス全体が正しく動くことの確認が本テストの目的。
#[test]
fn tls_enabled_server_returns_s_and_completes_query_round_trip_over_tls() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_tls_server(&users_path);

    let mut socket = TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S", "TLS-enabled server must accept SSL with 'S'");

    let client = tls_client::drive_client_handshake_over_socket(&mut socket);
    let mut channel = tls_client::TlsTestChannel::new(client, socket);

    write_startup_message(&mut channel, "alice", "irrelevant-db-name");

    // AuthenticationCleartextPassword（'R'/3）。
    let (type_byte, body) = read_typed_message(&mut channel);
    assert_eq!(type_byte, b'R');
    let auth_code = i32::from_be_bytes(body[..4].try_into().expect("4 bytes"));
    assert_eq!(auth_code, 3, "AuthenticationCleartextPassword expected");

    let mut password_body = Vec::new();
    password_body.extend_from_slice(b"pw-alice");
    password_body.push(0);
    write_typed_message(&mut channel, b'p', &password_body);

    // AuthenticationOk（'R'/0）。
    let (type_byte, body) = read_typed_message(&mut channel);
    assert_eq!(type_byte, b'R');
    assert_eq!(
        i32::from_be_bytes(body[..4].try_into().expect("4 bytes")),
        0
    );

    drain_to_ready_for_query(&mut channel);

    // 簡易クエリ（'Q'）。engine 未接続のため `0A000` を期待する。
    let mut query_body = Vec::new();
    query_body.extend_from_slice(b"SELECT 1");
    query_body.push(0);
    write_typed_message(&mut channel, b'Q', &query_body);

    let (type_byte, body) = read_typed_message(&mut channel);
    assert_eq!(type_byte, b'E', "expected ErrorResponse");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("0A000"), "expected 0A000, got: {text}");
    drain_to_ready_for_query(&mut channel);

    // Terminate（'X'）で正常終了する。
    write_typed_message(&mut channel, b'X', &[]);
}

/// 受入基準 3: TLS 確立後の `SSLRequest`（初回であっても）は拒否・切断される
/// こと。
#[test]
fn ssl_request_after_tls_established_is_rejected() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_tls_server(&users_path);

    let mut socket = TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S");

    let client = tls_client::drive_client_handshake_over_socket(&mut socket);
    let mut channel = tls_client::TlsTestChannel::new(client, socket);

    // TLS 確立後に SSLRequest を送る（StartupMessage 相当のフレームとして
    // TLS レコード内に含める）。
    let mut msg = Vec::new();
    msg.extend_from_slice(&8i32.to_be_bytes());
    msg.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
    channel.write_all(&msg).expect("send SSLRequest over TLS");

    let (type_byte, body) = read_typed_message(&mut channel);
    assert_eq!(
        type_byte, b'E',
        "expected ErrorResponse rejecting SSLRequest after TLS"
    );
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("08P01") || text.contains("42601") || text.contains("invalid"),
        "unexpected error body: {text}"
    );

    // 接続はその後閉じられる。サーバーは ErrorResponse 送出後に
    // `graceful_close`（best-effort な `close_notify`）を経てから TCP を
    // 閉じるため、暗号化された `close_notify` バイト列そのものが先に届き
    // うる（読み取れること自体は「まだ開いている」ことを意味しない）。
    // 有界な時間内に最終的な EOF（`Ok(0)`）へ到達することだけを確認する。
    let mut socket = channel.into_socket();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let mut probe = [0u8; 256];
    let mut reached_eof = false;
    for _ in 0..64 {
        match socket.read(&mut probe) {
            Ok(0) => {
                reached_eof = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    assert!(
        reached_eof,
        "connection must eventually close after rejecting SSLRequest post-TLS"
    );
}

/// 受入基準 3: TLS 確立後の `GSSENCRequest`（初回であっても）も同様に拒否・
/// 切断されること。
#[test]
fn gssenc_request_after_tls_established_is_rejected() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_tls_server(&users_path);

    let mut socket = TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S");

    let client = tls_client::drive_client_handshake_over_socket(&mut socket);
    let mut channel = tls_client::TlsTestChannel::new(client, socket);

    let mut msg = Vec::new();
    msg.extend_from_slice(&8i32.to_be_bytes());
    msg.extend_from_slice(&GSSENC_REQUEST_CODE.to_be_bytes());
    channel
        .write_all(&msg)
        .expect("send GSSENCRequest over TLS");

    let (type_byte, _body) = read_typed_message(&mut channel);
    assert_eq!(
        type_byte, b'E',
        "expected ErrorResponse rejecting GSSENCRequest after TLS"
    );
}

/// pipelining 対策の回帰確認（CVE-2021-23214 型）: `SSLRequest` と平文の
/// StartupMessage を同一 `write` で送っても、平文バイト列が StartupMessage
/// として処理されず、TLS ハンドシェイクの ClientHello として解釈されて
/// 失敗すること（応答なしで切断・あるいは fatal alert 後に切断）。
#[test]
fn pipelined_plaintext_after_ssl_request_is_not_processed_as_startup() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_tls_server(&users_path);

    let mut socket = TcpStream::connect(addr).expect("connect");
    let mut combined = Vec::new();
    combined.extend_from_slice(&8i32.to_be_bytes());
    combined.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
    // StartupMessage 相当の平文バイト列を SSLRequest と同じ write で送る。
    let mut params = Vec::new();
    params.extend_from_slice(b"user\0alice\0database\0db\0\0");
    let total_len = (4 + 4 + params.len()) as i32;
    combined.extend_from_slice(&total_len.to_be_bytes());
    combined.extend_from_slice(&0x0003_0000i32.to_be_bytes());
    combined.extend_from_slice(&params);
    socket.write_all(&combined).expect("send pipelined bytes");

    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S");

    // 平文の StartupMessage 相当バイト列は TLS レコードとして誤って解釈され、
    // ハンドシェイクは失敗する。届きうるのは失敗した TLS ハンドシェイクの
    // fatal alert（レコードヘッダの先頭バイトは ContentType::Alert = 0x15）
    // か EOF のみで、正規の pg wire 応答（`AuthenticationCleartextPassword`
    // の型バイト `'R'` = 0x52）が届くことは決してない。
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let mut buf = [0u8; 1];
    match socket.read(&mut buf) {
        Ok(0) => {}
        Ok(_) => assert_ne!(
            buf[0], b'R',
            "must not process pipelined plaintext as a valid pg wire AuthenticationCleartextPassword response"
        ),
        Err(_) => {}
    }
}

/// 受入基準 4: TLS ハンドシェイク完了後、接続の読み取りタイムアウトが
/// ハンドシェイク driver 側の定数（`HANDSHAKE_READ_TIMEOUT`）のまま
/// 残らず、接続設定値（`accept_loop_with_tls` に渡した値）へ復元される
/// こと。ハンドシェイク完了後にクライアントが何も送らない場合、接続
/// 設定値（短い値を注入）で応答なしに切断されることを確認する。
#[test]
fn read_timeout_is_restored_to_connection_setting_after_tls_handshake() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_tls_server_with_read_timeout(&users_path, Duration::from_millis(300));

    let mut socket = TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S");

    let _client = tls_client::drive_client_handshake_over_socket(&mut socket);

    // ハンドシェイク完了後、何も送らずに待つ。接続設定値（300ms）へ正しく
    // 復元されていれば、`HANDSHAKE_READ_TIMEOUT`（数秒〜のオーダー）より
    // 十分に短い時間で応答なしに切断される。
    // WIRE-5 の「応答なしで切断」は pg wire 応答（ErrorResponse 等）を
    // 送らないという契約であり、TLS 層の `close_notify`（`graceful_close`。
    // ペイロードを持たない小さな alert レコード）自体は正常に送られうる。
    // そのためここでは「有界な時間内に最終的な EOF へ到達すること」だけを
    // 検証し、`close_notify` に相当する少量のバイト列が先に届くことは許容
    // する（生ソケット読み取りのため復号はしない）。
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set client read timeout");
    let start = std::time::Instant::now();
    let mut buf = [0u8; 256];
    let mut reached_eof = false;
    for _ in 0..64 {
        match socket.read(&mut buf) {
            Ok(0) => {
                reached_eof = true;
                break;
            }
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    let elapsed = start.elapsed();
    assert!(
        reached_eof,
        "server must eventually close without a pg-wire response on idle timeout"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "connection read timeout was not restored to the short connection setting: took {elapsed:?}"
    );
}

/// SQL-TLS: ハンドシェイク途中（TLS レコードヘッダの先頭だけ送って黙る）のクライアントが、
/// 固定の長い期限ではなく**設定値**の読み取りタイムアウト（300ms）で切断されること
/// （Issue #2455。ポインタ: WIRE-25・WIRE-5。`perform_server_handshake_with_timeout` への
/// 接続設定値の受け渡し）。
#[test]
fn stalled_tls_handshake_is_closed_at_configured_read_timeout() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_tls_server_with_read_timeout(&users_path, Duration::from_millis(300));

    let mut socket = TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut socket);
    assert_eq!(read_exact_n(&mut socket, 1), b"S");

    // TLS レコードヘッダ（handshake・TLS1.2 互換版・長さ）の先頭 3 バイトだけ送って黙る。
    socket
        .write_all(&[0x16, 0x03, 0x01])
        .expect("send partial record header");
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set client read timeout");
    let start = std::time::Instant::now();
    let mut buf = [0u8; 256];
    let mut closed = false;
    for _ in 0..64 {
        match socket.read(&mut buf) {
            Ok(0) => {
                closed = true;
                break;
            }
            // 切断前の TLS alert 等は許容し、最終的な EOF/RST を待つ。
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {
                closed = true;
                break;
            }
            Err(_) => break,
        }
    }
    assert!(closed, "stalled handshake must be closed by the server");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "handshake stall was not bounded by the configured read timeout: {:?}",
        start.elapsed()
    );
}

/// 1 接続だけを受け付け、`handle_connection_with_options`（TLS opt-in）の
/// 戻り値を `JoinHandle` で返す。上位 wire 層が切断を静かに扱う（panic・
/// エラー終了しない）ことを戻り値で直接検証するためのヘルパー。
fn spawn_single_tls_connection(
    users_path: &std::path::Path,
) -> (
    std::net::SocketAddr,
    std::thread::JoinHandle<std::io::Result<()>>,
) {
    let store = UserStore::load_from_file(users_path).expect("valid user store");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("set read timeout");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("set write timeout");
        wire_server::handshake::handle_connection_with_options(
            stream,
            &store,
            None,
            Some(tls_client::test_config()),
        )
    });
    (addr, handle)
}

/// `SSLRequest` → TLS ハンドシェイク → StartupMessage → cleartext 認証 →
/// ReadyForQuery までを進めた TLS チャネルを返す。
fn authenticate_over_tls(addr: std::net::SocketAddr) -> tls_client::TlsTestChannel {
    let mut socket = TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut socket);
    assert_eq!(&read_exact_n(&mut socket, 1), b"S");
    let client = tls_client::drive_client_handshake_over_socket(&mut socket);
    let mut channel = tls_client::TlsTestChannel::new(client, socket);

    write_startup_message(&mut channel, "alice", "irrelevant-db-name");
    let (type_byte, _) = read_typed_message(&mut channel);
    assert_eq!(type_byte, b'R');
    write_typed_message(&mut channel, b'p', b"pw-alice\0");
    let (type_byte, body) = read_typed_message(&mut channel);
    assert_eq!(type_byte, b'R');
    assert_eq!(
        i32::from_be_bytes(body[..4].try_into().expect("4 bytes")),
        0
    );
    drain_to_ready_for_query(&mut channel);
    channel
}

/// 生ソケットから EOF まで TLS レコードを読み、クライアント鍵で復号した
/// 内側の content type 列を返す。
fn collect_inner_content_types(
    client: &mut tls_client::TestClient,
    socket: &mut TcpStream,
) -> Vec<wire_server::tls::record::ContentType> {
    use wire_server::tls::record::{self, RecordKind};
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    let mut types = Vec::new();
    while let Ok(Some(rec)) = record::read_record(socket, RecordKind::Ciphertext) {
        let inner = client
            .opener
            .open(&rec)
            .expect("server record must decrypt");
        types.push(inner.content_type);
    }
    types
}

/// PR #1056 レビュー指摘（close_notify なしの EOF をエラー扱いへ変更）後も、
/// 認証後にクライアントが close_notify も Terminate も送らず TCP を閉じた
/// 場合、上位の pg wire 層は通常のクライアント切断として静かに終了する
/// こと（panic せず `Ok(())`、ErrorResponse も close_notify も送らない。
/// ストリームは失敗状態に固定されるため以後の送出は行われない）。
#[test]
fn client_disconnect_without_close_notify_is_handled_as_quiet_disconnect() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let (addr, handle) = spawn_single_tls_connection(&users_path);

    let channel = authenticate_over_tls(addr);
    let (mut client, mut socket) = channel.into_parts();
    socket
        .shutdown(std::net::Shutdown::Write)
        .expect("shutdown write");

    let types = collect_inner_content_types(&mut client, &mut socket);
    assert!(
        types.is_empty(),
        "server must not send anything after an unclean client disconnect, got {types:?}"
    );

    let result = handle.join().expect("connection handler must not panic");
    assert!(
        result.is_ok(),
        "unclean client disconnect must end the connection quietly: {result:?}"
    );
}

/// Terminate（'X'）送信後にクライアントが close_notify なしで TCP を閉じても、
/// サーバーは pg wire の応答（ApplicationData）を一切送らず、best-effort の
/// close_notify（alert）のみを送って `Ok(())` で終了すること。
#[test]
fn terminate_then_disconnect_without_close_notify_ends_quietly() {
    use wire_server::tls::record::ContentType;

    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let (addr, handle) = spawn_single_tls_connection(&users_path);

    let mut channel = authenticate_over_tls(addr);
    write_typed_message(&mut channel, b'X', &[]);
    let (mut client, mut socket) = channel.into_parts();
    socket
        .shutdown(std::net::Shutdown::Write)
        .expect("shutdown write");

    let types = collect_inner_content_types(&mut client, &mut socket);
    assert!(
        types.iter().all(|t| *t == ContentType::Alert),
        "only a close_notify alert may follow Terminate, got {types:?}"
    );

    let result = handle.join().expect("connection handler must not panic");
    assert!(
        result.is_ok(),
        "Terminate followed by disconnect must end quietly: {result:?}"
    );
}
