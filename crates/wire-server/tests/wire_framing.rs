//! wire-server の結合テスト（TASK-68・対象ビヘイビア WIRE-4, WIRE-10）。
//!
//! `wire_auth.rs`（WIRE-1〜3。ヘッダコメント・責務は変更しない）と同様、
//! ephemeral port（`127.0.0.1:0`）でサーバースレッドを起動し、`std::net::TcpStream`
//! で生バイトを送受信する自作クライアントを用いる。ヘルパーは `wire_auth.rs` から
//! 複製している（結合テストファイル間でのヘルパー共有機構を持たないため）。

#[path = "common/fixture_guard.rs"]
mod fixture_guard;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use wire_server::auth::{argon2id, UserStore};
use wire_server::framing::MAX_STARTUP_LEN;
use wire_server::limits::ConnectionLimiter;

const TEST_PARAMS: argon2id::Params = argon2id::RECOMMENDED_PARAMS;

/// フィクスチャ一時ディレクトリ名の一意性を pid・時刻だけに委ねないための
/// プロセス内単調カウンタ（`wire_auth.rs` と同一クラスの競合対策。Issue #172）。
static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

fn write_user_store_file(records: &[(&str, &str, &str)]) -> fixture_guard::UserStoreFile {
    let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "wire-server-wire-framing-test-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos(),
        seq
    ));
    // `create_dir`（既存なら `Err`）にして、万一の衝突を黙って吸収せず顕在化させる
    // （Issue #172: フィクスチャ一時ディレクトリ名衝突によるテストフレーク対策）。
    std::fs::create_dir(&dir).expect("create unique fixture dir");
    let path = dir.join("users.txt");

    let mut content = String::new();
    for (username, tenant_id, password) in records {
        let salt = b"0123456789abcdef";
        let phc = argon2id::encode_phc(password.as_bytes(), salt, &TEST_PARAMS)
            .expect("valid phc encoding");
        content.push_str(&format!("{username}:{tenant_id}:{phc}\n"));
    }
    std::fs::write(&path, content).expect("write user store fixture");
    fixture_guard::UserStoreFile::new(dir, path)
}

fn spawn_server_accepting_one(users_path: &std::path::Path) -> std::net::SocketAddr {
    let store = UserStore::load_from_file(users_path).expect("valid user store");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = wire_server::handshake::handle_connection_bounded(stream, &store);
        }
    });

    addr
}

/// `spawn_server_accepting_one` と異なり、接続ハンドラを別スレッドで `join` まで
/// 待てるようにハンドルを返す（途中切断で panic しないことの回帰確認に使う）。
fn spawn_server_accepting_one_joinable(
    users_path: &std::path::Path,
) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
    let store = UserStore::load_from_file(users_path).expect("valid user store");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    let handle = std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = wire_server::handshake::handle_connection_bounded(stream, &store);
        }
    });

    (addr, handle)
}

fn spawn_server_with_accept_loop(
    users_path: &std::path::Path,
    max_connections: usize,
    io_timeout: Duration,
) -> std::net::SocketAddr {
    let store = Arc::new(UserStore::load_from_file(users_path).expect("valid user store"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(max_connections);

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_limiter(listener, store, limiter, io_timeout);
    });

    addr
}

fn send_ssl_request_and_startup(stream: &mut TcpStream, username: &str, database: &str) {
    let mut ssl_req = Vec::new();
    ssl_req.extend_from_slice(&8i32.to_be_bytes());
    ssl_req.extend_from_slice(&80_877_103i32.to_be_bytes());
    stream.write_all(&ssl_req).expect("send SSLRequest");

    let mut resp = [0u8; 1];
    stream.read_exact(&mut resp).expect("read SSL response");
    assert_eq!(&resp, b"N");

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

fn read_auth_request_type(stream: &mut TcpStream) -> i32 {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    assert_eq!(header[0], b'R', "expected Authentication* message");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf);
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("read auth code");
    assert_eq!(len, 8);
    i32::from_be_bytes(code_buf)
}

fn send_password_message(stream: &mut TcpStream, password: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(password.as_bytes());
    body.push(0);
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::new();
    msg.push(b'p');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    stream.write_all(&msg).expect("send PasswordMessage");
}

/// 完全な認証シーケンスを実行し、`ReadyForQuery` まで到達させる（並行接続の非影響
/// 確認テストで「正常系が生き続けている」ことを示すために使う）。
fn authenticate_and_reach_ready_for_query(stream: &mut TcpStream, username: &str, password: &str) {
    send_ssl_request_and_startup(stream, username, "db");
    let _ = read_auth_request_type(stream);
    send_password_message(stream, password);

    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read auth ok type");
    assert_eq!(header[0], b'R', "expected AuthenticationOk");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("read code");
    assert_eq!(i32::from_be_bytes(code_buf), 0);

    let mut msg_type = read_message_type_discarding_body(stream);
    let mut safety = 0;
    while msg_type != b'Z' {
        safety += 1;
        assert!(safety < 20, "too many messages before ReadyForQuery");
        msg_type = read_message_type_discarding_body(stream);
    }
}

fn read_message_type_discarding_body(stream: &mut TcpStream) -> u8 {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    header[0]
}

/// ErrorResponse を読み取り、'E' であること・SQLSTATE を body に含むことを確認する。
fn expect_error_response_with_sqlstate(stream: &mut TcpStream, sqlstate: &str) {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read response type");
    assert_eq!(header[0], b'E', "expected ErrorResponse");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains(sqlstate),
        "ErrorResponse must carry SQLSTATE {sqlstate}, got: {body_str:?}"
    );
}

fn expect_connection_closed(stream: &mut TcpStream) {
    let mut extra = [0u8; 1];
    let n = stream.read(&mut extra).unwrap_or(0);
    assert_eq!(n, 0, "connection must be closed");
}

/// WIRE-4: 認証後の簡易クエリ（'Q'）が length で 1 MiB 超過を宣言した場合、
/// `54000` で応答してから切断すること。length のみ送信することで、未読バイトの
/// 有無に起因する RST 競合を避け、応答観測を決定的にする。
#[test]
fn wire4_oversized_query_frame_returns_54000_then_closes() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");
    authenticate_and_reach_ready_for_query(&mut stream, "alice", "correct-horse");

    let declared: i32 = 1024 * 1024 + 1; // MAX_MESSAGE_LEN + 1
    let mut msg = Vec::new();
    msg.push(b'Q');
    msg.extend_from_slice(&declared.to_be_bytes());
    stream.write_all(&msg).expect("send oversized query header");

    expect_error_response_with_sqlstate(&mut stream, "54000");
    expect_connection_closed(&mut stream);
}

/// WIRE-4: 認証前の PasswordMessage が上限超過を宣言した場合も同様に `54000` で
/// 応答してから切断すること。
#[test]
fn wire4_oversized_password_message_returns_54000_then_closes() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "alice", "db");
    let _ = read_auth_request_type(&mut stream);

    let declared: i32 = 1024 * 1024 + 1;
    let mut msg = Vec::new();
    msg.push(b'p');
    msg.extend_from_slice(&declared.to_be_bytes());
    stream
        .write_all(&msg)
        .expect("send oversized password header");

    expect_error_response_with_sqlstate(&mut stream, "54000");
    expect_connection_closed(&mut stream);
}

/// WIRE-4 境界値: length が `MAX_MESSAGE_LEN`（1 MiB）ちょうどの簡易クエリは
/// サイズ超過として拒否されないこと（`54000` を返さないことのみ確認。簡易クエリ
/// 実行自体は TASK-71 未実装のため `0A000` 等それ以外の応答であればよい）。
#[test]
fn wire4_frame_at_exact_limit_is_not_rejected_as_too_large() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");
    authenticate_and_reach_ready_for_query(&mut stream, "alice", "correct-horse");

    let max_message_len = 1024 * 1024usize;
    let body_len = max_message_len - 4;
    let mut body = vec![b'a'; body_len - 1];
    body.push(0); // 終端 NUL（簡易クエリの形状検証を満たすため）

    let mut msg = Vec::with_capacity(1 + max_message_len);
    msg.push(b'Q');
    msg.extend_from_slice(&(max_message_len as i32).to_be_bytes());
    msg.extend_from_slice(&body);
    stream.write_all(&msg).expect("send exactly-at-limit query");

    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read response type");
    if header[0] == b'E' {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).expect("read len");
        let len = i32::from_be_bytes(len_buf) as usize;
        let mut resp_body = vec![0u8; len - 4];
        stream.read_exact(&mut resp_body).expect("read body");
        let body_str = String::from_utf8_lossy(&resp_body);
        assert!(
            !body_str.contains("54000"),
            "boundary-sized frame must not be rejected as too large, got: {body_str:?}"
        );
    }
}

/// WIRE-10: StartupMessage の length が最小値（8）未満の場合 `08P01` で応答して
/// から切断すること。
#[test]
fn wire10_startup_length_below_min_returns_08p01_then_closes() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    let mut msg = Vec::new();
    msg.extend_from_slice(&4i32.to_be_bytes()); // MIN_STARTUP_LEN(8) 未満
    stream.write_all(&msg).expect("send undersized startup");

    expect_error_response_with_sqlstate(&mut stream, "08P01");
    expect_connection_closed(&mut stream);
}

/// WIRE-10: StartupMessage の length が `MAX_STARTUP_LEN` を超える場合、length の
/// 4 バイトのみ送信して `08P01` で応答してから切断すること。
#[test]
fn wire10_startup_length_over_limit_returns_08p01_then_closes() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    let declared = (MAX_STARTUP_LEN + 1) as i32;
    let mut msg = Vec::new();
    msg.extend_from_slice(&declared.to_be_bytes());
    stream
        .write_all(&msg)
        .expect("send oversized startup header");

    expect_error_response_with_sqlstate(&mut stream, "08P01");
    expect_connection_closed(&mut stream);
}

/// WIRE-10: StartupMessage の length が負の場合も `08P01` で応答してから切断する
/// こと。
#[test]
fn wire10_negative_startup_length_returns_08p01() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    let mut msg = Vec::new();
    msg.extend_from_slice(&(-1i32).to_be_bytes());
    stream
        .write_all(&msg)
        .expect("send negative-length startup");

    expect_error_response_with_sqlstate(&mut stream, "08P01");
    expect_connection_closed(&mut stream);
}

/// WIRE-10: 宣言長より実送信が短い（途中切断）StartupMessage を受けても panic
/// せず、応答なしで正常にクローズすること（`handle_connection_bounded` を包むスレッドの
/// `join()` が `Ok` であることで確認する）。
#[test]
fn wire10_truncated_startup_closes_without_panic() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, server) = spawn_server_accepting_one_joinable(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    let mut msg = Vec::new();
    msg.extend_from_slice(&64i32.to_be_bytes()); // 宣言 64 バイトだが 10 バイトしか送らない
    msg.extend_from_slice(&[0u8; 6]);
    stream.write_all(&msg).expect("send truncated startup");
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("shutdown write");

    expect_connection_closed(&mut stream);
    server.join().expect("server thread must not panic");
}

/// WIRE-10: 認証後の簡易クエリで途中切断が起きても panic せず、応答なしで正常に
/// クローズすること。
#[test]
fn wire10_truncated_query_after_auth_closes_without_panic() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, server) = spawn_server_accepting_one_joinable(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");
    authenticate_and_reach_ready_for_query(&mut stream, "alice", "correct-horse");

    let mut msg = Vec::new();
    msg.push(b'Q');
    msg.extend_from_slice(&64i32.to_be_bytes()); // 宣言 64 バイトだが 10 バイトしか送らない
    msg.extend_from_slice(&[0u8; 6]);
    stream.write_all(&msg).expect("send truncated query");
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("shutdown write");

    expect_connection_closed(&mut stream);
    server.join().expect("server thread must not panic");
}

/// WIRE-10: 1 接続が不正フレーム（08P01）や途中切断で異常終了しても、他の接続が
/// 正常に認証を完了して `ReadyForQuery` へ到達できること（他接続への非影響）。
#[test]
fn wire10_malformed_connection_does_not_affect_other_connections() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_accept_loop(&users_path, 4, Duration::from_secs(5));

    // A: 不正な StartupMessage（負の長さ）。
    {
        let mut a = TcpStream::connect(addr).expect("connect A");
        let mut msg = Vec::new();
        msg.extend_from_slice(&(-1i32).to_be_bytes());
        a.write_all(&msg).expect("send negative-length startup");
        expect_error_response_with_sqlstate(&mut a, "08P01");
        expect_connection_closed(&mut a);
    }

    // B: 途中切断。
    {
        let mut b = TcpStream::connect(addr).expect("connect B");
        let mut msg = Vec::new();
        msg.extend_from_slice(&64i32.to_be_bytes());
        msg.extend_from_slice(&[0u8; 6]);
        b.write_all(&msg).expect("send truncated startup");
        b.shutdown(std::net::Shutdown::Write)
            .expect("shutdown write");
        expect_connection_closed(&mut b);
    }

    // C: 正常系。A・B の異常が尾を引いていないことを確認する。
    let mut c = TcpStream::connect(addr).expect("connect C");
    authenticate_and_reach_ready_for_query(&mut c, "alice", "correct-horse");
}
