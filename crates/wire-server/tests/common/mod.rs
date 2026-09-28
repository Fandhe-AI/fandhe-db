//! wire-server crate の複数の結合テスト・ベンチから `#[path = "common/mod.rs"]`
//! （または `../tests/common/mod.rs`）で include される共有ヘルパー集。
//! 元は `tests/wire_extended_query.rs`（TASK-71・WIRE-8）専用として作られたが
//! （`tests/wire_auth.rs` 側の同種ヘルパーとの統合は見送られたまま）、その後
//! 多数の wire テスト・ベンチが in-process サーバースレッド起動・pg wire
//! クライアントヘルパーとして共有するに至っている。
//!
//! 本ファイル末尾の節（Issue #736・TASK-171／HTTP-1・HTTP-9）は、実
//! `wire-server` バイナリを子プロセスとして起動し CLI 引数の受理・拒否・
//! 選択表層のリスナー分岐を外形的に検証するテスト
//! （`tests/http1_surface_select.rs`）向けの独立したヘルパー群であり、
//! 上記の in-process ヘルパーとは前提を共有しない（`#[path]` で include する
//! 各ファイルはそれぞれ独立にコンパイルされるモジュールになるため、
//! 同一ファイル内に共存させても他ファイルとのシンボル衝突は起きない）。
//!
//! 同節の [`drain_stderr`]／[`wait_for_listening`]／
//! [`wait_for_listening_addr_and_lines`] は、子プロセス（`wire-server`）の
//! stderr を listen 行到達後も EOF まで読み続ける共通ヘルパーであり
//! （Issue #1082）、`tests/wire_fault_injection_cli.rs`・`wire7_bind_guard.rs`・
//! `wire_durability_cli.rs`・`wire_search_engine_cli.rs`・
//! `wire_ddl_permission_cli.rs`・`wire_scram_mock_key_file_size.rs`・
//! `http_insert_response_boundary.rs` の 7 ファイルが個別に持っていた同型の
//! ローカル実装（listen 行到達後に受信側を破棄すると読み取りスレッドが
//! 送信失敗でループを抜け、パイプの読み口を閉じてしまう形）を集約したもの。
//! `three_client_e2e.rs::ServerGuard` と同じ理由による是正（Issue #943／
//! #1081 で先に是正済み）: #1081 以前は読み口を閉じたままサーバーが接続
//! エラー等を stderr へ複数行書くと `EPIPE` で `eprintln!` が panic し、panic
//! フック（TASK-97・RECOVER-6／TASK-99・RECOVER-8）経由で SIGABRT 終了して
//! いた。#1081 でサーバー側の診断ログが `engine::log_stderr!`（書き込み失敗を
//! 無視する）へ置き換わったため、読み口を閉じてもサーバー側は abort しなく
//! なったが、読み口を閉じると listen 後の診断行（接続エラー等）が失われる
//! ため、本節は引き続き EOF まで読み続けて診断行を tail へ保持する。
//! `wire_search_engine_cli.rs` はかつて本集約の対象外として申し送られて
//! いたが、Issue #1082 で対象に含めた。
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wire_server::auth::{argon2id, UserStore};
use wire_server::limits::ConnectionLimiter;

/// `UserStore::load_from_file` は Argon2id パラメータが
/// `argon2id::RECOMMENDED_PARAMS` と完全一致するレコードのみを受理するため、
/// フィクスチャも本番既定値をそのまま使う（`tests/wire_auth.rs` と同方針）。
const TEST_PARAMS: argon2id::Params = argon2id::RECOMMENDED_PARAMS;

/// フィクスチャ一時ディレクトリ名の一意性を pid・時刻だけに委ねないための
/// プロセス内単調カウンタ（`wire_auth.rs` と同一クラスの競合対策。Issue #172）。
static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn write_user_store_file(records: &[(&str, &str, &str)]) -> std::path::PathBuf {
    let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "wire-server-wire-extended-query-test-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos(),
        seq
    ));
    // `create_dir`（既存なら `Err`）で衝突を黙って吸収せず顕在化させる
    // （Issue #172）。
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
    path
}

/// サーバースレッドを起動し、1 接続だけ受理してスレッドを終了する。
pub fn spawn_server_accepting_one(users_path: &std::path::Path) -> std::net::SocketAddr {
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

/// `wire_server::server::accept_loop_with_limiter` 経由でサーバースレッドを起動する
/// （接続スロット解放の確認に使う）。
pub fn spawn_server_with_accept_loop(
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

/// SSLRequest → 'N' → StartupMessage(protocol 3.0, user=...) を送るクライアント側の
/// 共通シーケンス。
pub fn send_ssl_request_and_startup(stream: &mut TcpStream, username: &str, database: &str) {
    let mut ssl_req = Vec::new();
    ssl_req.extend_from_slice(&8i32.to_be_bytes());
    ssl_req.extend_from_slice(&80_877_103i32.to_be_bytes());
    stream.write_all(&ssl_req).expect("send SSLRequest");

    let mut resp = [0u8; 1];
    stream.read_exact(&mut resp).expect("read SSL response");
    assert_eq!(&resp, b"N", "server must decline SSL");

    send_startup_message(stream, username, database);
}

/// StartupMessage（protocol 3.0, user=.../database=...）のみを送る
/// （SSLRequest フェーズを別途消費済みの接続向け。`connect_after_slot_available`
/// が SSLRequest の 'N' 応答まで進めた接続の続きとして使う）。
pub fn send_startup_message(stream: &mut TcpStream, username: &str, database: &str) {
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

/// 接続スロットが解放されるまで短い間隔でリトライし、解放後の TCP 接続を
/// 返す（SSLRequest の 'N' 応答まで到達した時点で解放済みと判定する）。
///
/// `server::accept_loop_with_limiter` は同時接続数の上限を超える接続を、ハンドシェイクへ
/// 進ませず `limits::ConnectionLimiter::try_acquire` 失敗時に `53300` の
/// ErrorResponse を返してから即座にクローズする（`limits::reject_too_many_connections`
/// 経由）。そのため SSLRequest の応答が 1 バイトも来ず EOF/タイムアウトになる
/// ことが「スロット未解放」の確実な合図になる。逆に 'N' が読めれば
/// `ConnectionLimiter` の枠を確保できたことの証拠であり、拒否された接続の
/// write-side shutdown（drain 開始時の EOF）を誤ってスロット解放の証拠として
/// 扱う競合を避けられる（`wire8_rejection_releases_connection_slot` のレビュー
/// 是正）。
pub fn connect_after_slot_available(
    addr: std::net::SocketAddr,
    total_timeout: Duration,
) -> TcpStream {
    let deadline = std::time::Instant::now() + total_timeout;
    loop {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set read timeout");

        let mut ssl_req = Vec::new();
        ssl_req.extend_from_slice(&8i32.to_be_bytes());
        ssl_req.extend_from_slice(&80_877_103i32.to_be_bytes());
        stream.write_all(&ssl_req).expect("send SSLRequest");

        let mut resp = [0u8; 1];
        match stream.read_exact(&mut resp) {
            // 'N': ハンドシェイクへ進んだ = ConnectionLimiter の枠を確保できた。
            Ok(()) if &resp == b"N" => {
                stream.set_read_timeout(None).expect("clear read timeout");
                return stream;
            }
            // 'E': limits::reject_too_many_connections が 53300 の ErrorResponse
            // を書いてから切断した = まだ枠が空いていない。リトライする。
            Ok(()) => {
                assert_eq!(
                    &resp, b"E",
                    "unexpected first response byte while polling for a free slot"
                );
                assert!(
                    std::time::Instant::now() < deadline,
                    "connection slot was not released within {total_timeout:?}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => {
                // 拒否ワーカー自体が枯渇した場合の応答なし即時クローズ、または
                // まだ枠が空くのを待っている途中のタイムアウト。どちらも
                // 「未解放」としてリトライする。
                assert!(
                    std::time::Instant::now() < deadline,
                    "connection slot was not released within {total_timeout:?}"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

pub fn read_auth_request_type(stream: &mut TcpStream) -> i32 {
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

pub fn send_password_message(stream: &mut TcpStream, password: &str) {
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

/// 次のメッセージの型バイトを返す（本文は破棄）。
pub fn read_message_type_discarding_body(stream: &mut TcpStream) -> u8 {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    header[0]
}

/// AuthenticationCleartextPassword → 正しいパスワードで認証し、後続の
/// BackendKeyData・ParameterStatus* を読み飛ばして ReadyForQuery(`'Z'`) 到達まで
/// 進める。戻り値は接続済みの `TcpStream`（呼び出し元がそのまま拡張クエリ系
/// メッセージを送信できる状態）。
pub fn authenticate_to_ready_for_query(
    addr: std::net::SocketAddr,
    username: &str,
    password: &str,
) -> TcpStream {
    let mut stream = TcpStream::connect(addr).expect("connect");
    send_ssl_request_and_startup(&mut stream, username, "irrelevant-db-name");
    let auth_code = read_auth_request_type(&mut stream);
    assert_eq!(auth_code, 3, "AuthenticationCleartextPassword expected");

    send_password_message(&mut stream, password);

    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read auth ok type");
    assert_eq!(header[0], b'R', "expected AuthenticationOk");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("read code");
    assert_eq!(i32::from_be_bytes(code_buf), 0, "AuthenticationOk code");

    let mut msg_type = read_message_type_discarding_body(&mut stream);
    let mut safety = 0;
    while msg_type != b'Z' {
        safety += 1;
        assert!(safety < 20, "too many messages before ReadyForQuery");
        msg_type = read_message_type_discarding_body(&mut stream);
    }

    stream
}

/// 型バイト + 空 body の長さプレフィックス付きメッセージを組み立てて送る
/// （拡張クエリプロトコル系メッセージは本テストでは body の中身を解釈しないため
/// 空 body で十分。長さフィールドのみ正しく検証されればよい）。
pub fn send_length_prefixed_message(stream: &mut TcpStream, type_byte: u8, body: &[u8]) {
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::new();
    msg.push(type_byte);
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(body);
    stream.write_all(&msg).expect("send message");
}

/// ErrorResponse（'E'）を読み取り、SQLSTATE が期待値と一致することを確認する。
pub fn expect_error_response_with_sqlstate(stream: &mut TcpStream, expected_sqlstate: &str) {
    expect_error_response_body(stream, expected_sqlstate, None);
}

/// ErrorResponse（'E'）を読み取り、SQLSTATE 及び（指定時）メッセージ本文の部分
/// 一致を確認する。分類ごとにメッセージ文言が分かれていること
/// （`protocol_dispatch::response_message`）の結合テスト側の回帰防止に使う。
pub fn expect_error_response_with_sqlstate_and_message(
    stream: &mut TcpStream,
    expected_sqlstate: &str,
    expected_message_substr: &str,
) {
    expect_error_response_body(stream, expected_sqlstate, Some(expected_message_substr));
}

fn expect_error_response_body(
    stream: &mut TcpStream,
    expected_sqlstate: &str,
    expected_message_substr: Option<&str>,
) {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'E', "expected ErrorResponse");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains(expected_sqlstate),
        "expected SQLSTATE {expected_sqlstate}, got: {body_str:?}"
    );
    if let Some(expected_message_substr) = expected_message_substr {
        assert!(
            body_str.contains(expected_message_substr),
            "expected message containing {expected_message_substr:?}, got: {body_str:?}"
        );
    }
}

/// 応答読み取り後に接続が閉じられている（追加の読み取りが EOF になる）ことを
/// 確認する。読み取り前に書き込み方向を閉じ、サーバー側の lingering close が
/// 即座に EOF を検出できるようにする（drain のタイムアウト分だけテストが遅く
/// なるのを避けるため）。
pub fn expect_connection_closed(stream: &mut TcpStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let mut extra = [0u8; 1];
    let n = stream.read(&mut extra).unwrap_or(0);
    assert_eq!(n, 0, "connection must be closed after rejection");
    let _ = extra;
}

// ---------------------------------------------------------------------------
// engine 接続経路のヘルパー（TASK-73・WIRE-1。`tests/wire1_simple_query.rs` と
// `tests/three_client_e2e.rs` が共有する）。
// ---------------------------------------------------------------------------

/// サーバースレッドを `wire_server::server::accept_loop_with_engine` 経由で起動する
/// （簡易クエリが `engine::core::EngineCore` へ到達する経路。TASK-73）。
pub fn spawn_server_with_engine(
    users_path: &std::path::Path,
    engine: std::sync::Arc<engine::core::EngineCore>,
) -> std::net::SocketAddr {
    let store = UserStore::load_from_file(users_path).expect("valid user store");
    let store = Arc::new(store);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            engine,
            limiter,
            Duration::from_secs(5),
        );
    });

    addr
}

/// 簡易クエリ（'Q'）1 文を送る（UTF-8 テキストのみを想定。`sql` は呼び出し側の
/// リテラル文字列を渡す前提で、NUL 混入検証は wire-server 側のテストが別途担う）。
pub fn send_simple_query(stream: &mut TcpStream, sql: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    send_length_prefixed_message(stream, b'Q', &body);
}

/// `RowDescription`（'T'）を読み、列名のリストを返す。
pub fn read_row_description(stream: &mut TcpStream) -> Vec<String> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'T', "expected RowDescription");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");

    let field_count = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2usize;
    let mut names = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let nul = body[pos..]
            .iter()
            .position(|&b| b == 0)
            .expect("nul-terminated column name");
        let name = std::str::from_utf8(&body[pos..pos + nul])
            .expect("utf8 column name")
            .to_string();
        pos += nul + 1;
        pos += 4 + 2 + 4 + 2 + 4 + 2; // table oid, attnum, type oid, typlen, typmod, format
        names.push(name);
    }
    names
}

/// `RowDescription`（'T'）を読み、列名と型 OID（`pg_type.oid`）の組のリストを
/// 返す（`read_row_description` の型情報付き版。`INTEGER`／`BIGINT` 列の
/// `int4`／`int8` 写像を wire 越しに検証する用途。Issue #903 レビュー指摘）。
pub fn read_row_description_with_oids(stream: &mut TcpStream) -> Vec<(String, i32)> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'T', "expected RowDescription");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");

    let field_count = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2usize;
    let mut columns = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let nul = body[pos..]
            .iter()
            .position(|&b| b == 0)
            .expect("nul-terminated column name");
        let name = std::str::from_utf8(&body[pos..pos + nul])
            .expect("utf8 column name")
            .to_string();
        pos += nul + 1;
        pos += 4 + 2; // table oid, attnum
        let oid_bytes: [u8; 4] = body
            .get(pos..pos + 4)
            .expect("type oid bytes")
            .try_into()
            .expect("slice is exactly 4 bytes");
        let oid = i32::from_be_bytes(oid_bytes);
        pos += 4 + 2 + 4 + 2; // type oid（読み取り済み）, typlen, typmod, format
        columns.push((name, oid));
    }
    columns
}

/// `RowDescription`（'T'）を読み、列名・型 OID・型長（`pg_type.typlen`）の
/// 3 つ組のリストを返す（`read_row_description_with_oids` の型長付き版。
/// WIRE-13・TASK-200・Issue #895 の新規スカラー型 OID を wire 越しに検証する
/// 用途）。
pub fn read_row_description_with_types(stream: &mut TcpStream) -> Vec<(String, i32, i16)> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'T', "expected RowDescription");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");

    let field_count = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2usize;
    let mut columns = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let nul = body[pos..]
            .iter()
            .position(|&b| b == 0)
            .expect("nul-terminated column name");
        let name = std::str::from_utf8(&body[pos..pos + nul])
            .expect("utf8 column name")
            .to_string();
        pos += nul + 1;
        pos += 4 + 2; // table oid, attnum
        let oid_bytes: [u8; 4] = body
            .get(pos..pos + 4)
            .expect("type oid bytes")
            .try_into()
            .expect("slice is exactly 4 bytes");
        let oid = i32::from_be_bytes(oid_bytes);
        pos += 4; // type oid（読み取り済み）
        let typlen_bytes: [u8; 2] = body
            .get(pos..pos + 2)
            .expect("typlen bytes")
            .try_into()
            .expect("slice is exactly 2 bytes");
        let typlen = i16::from_be_bytes(typlen_bytes);
        pos += 2 + 4 + 2; // typlen（読み取り済み）, typmod, format
        columns.push((name, oid, typlen));
    }
    columns
}

/// `DataRow`（'D'）を 1 行読み、各セルを `Option<String>`（`NULL` は `None`）として
/// 返す。
pub fn read_data_row(stream: &mut TcpStream) -> Vec<Option<String>> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'D', "expected DataRow");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");

    let field_count = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2usize;
    let mut cells = Vec::with_capacity(field_count);
    for _ in 0..field_count {
        let cell_len = i32::from_be_bytes([body[pos], body[pos + 1], body[pos + 2], body[pos + 3]]);
        pos += 4;
        if cell_len < 0 {
            cells.push(None);
            continue;
        }
        let cell_len = cell_len as usize;
        let text = std::str::from_utf8(&body[pos..pos + cell_len])
            .expect("utf8 cell")
            .to_string();
        pos += cell_len;
        cells.push(Some(text));
    }
    cells
}

/// `CommandComplete`（'C'）のタグ文字列を読む。
pub fn read_command_complete(stream: &mut TcpStream) -> String {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'C', "expected CommandComplete");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    let end = body.len().saturating_sub(1);
    String::from_utf8_lossy(&body[..end]).to_string()
}

/// `EmptyQueryResponse`（'I'）を読み、body が存在しないこと（length=4）を確認する。
pub fn expect_empty_query_response(stream: &mut TcpStream) {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'I', "expected EmptyQueryResponse");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    assert_eq!(
        i32::from_be_bytes(len_buf),
        4,
        "EmptyQueryResponse has no body"
    );
}

/// `ReadyForQuery`（'Z'）を読み切る（`status` バイトの中身は検証しない呼び出し側
/// 向けの簡便ヘルパー）。トランザクション状態バイトを検証したい呼び出し側は
/// [`read_ready_for_query_status`] を使う。
pub fn read_ready_for_query(stream: &mut TcpStream) {
    let _status = read_ready_for_query_status(stream);
}

/// `ReadyForQuery`（'Z'）を読み切り、トランザクション状態バイト
/// （`'I'`／`'T'`／`'E'`）を返す（WIRE-19・SQL-31・TASK-221）。
pub fn read_ready_for_query_status(stream: &mut TcpStream) -> u8 {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'Z', "expected ReadyForQuery");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    *body.last().expect("ReadyForQuery body has a status byte")
}

// ---------------------------------------------------------------------------
// 子プロセス起動ヘルパー（Issue #736・TASK-171／HTTP-1・HTTP-9）。
//
// `tests/http1_surface_select.rs` から使われる。実バイナリ
// （`env!("CARGO_BIN_EXE_wire-server")`）を子プロセスとして起動し、CLI 引数の
// 受理・拒否（fail-closed）・選択表層に応じたリスナー分岐を stderr の外形
// 観測だけで検証する結合テストの土台。旧 `tests/wire_surface_cli.rs` に
// 個別実装されていた同型ヘルパーをここへ集約する（PR #795 で本ファイルへの
// 共通化が「対象外」として申し送られた分）。stderr の EOF までの読み取り
// （`StderrDrain`・`drain_stderr`・`wait_for_listening`・
// `wait_for_listening_addr_and_lines`）は Issue #1082 で 7 ファイル分を
// 本節へ集約した（`wire_search_engine_cli.rs` もこの集約に含む。旧版の本
// コメントでは対象外としていたが、Issue #1082 で解消済み）。
// ---------------------------------------------------------------------------

/// 子プロセス用フィクスチャの一時ディレクトリ名が他テスト・他プロセスと
/// 衝突しないためのプロセス内単調カウンタ（上記 `FIXTURE_SEQ` とは独立の
/// カウンタ空間。子プロセス起動ヘルパー専用に分ける）。
static CHILD_PROCESS_FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// 子プロセス（`wire-server` バイナリ）に渡すユーザーストア・DB ファイルの
/// 置き場となる一時ディレクトリ。`Drop` で確実に削除する
/// （`wire_search_engine_cli.rs::TempFixtureDir` と同型）。
pub struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    pub fn new(label: &str) -> Self {
        let seq = CHILD_PROCESS_FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-child-process-{label}-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos(),
            seq
        ));
        std::fs::create_dir(&dir).expect("create unique fixture dir");
        Self { dir }
    }

    pub fn users_path_str(&self) -> String {
        self.dir
            .join("users.txt")
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }

    pub fn db_path_str(&self) -> String {
        self.dir
            .join("db.redb")
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }
}

impl Drop for TempFixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 空のユーザーストア（子プロセス起動ヘルパーを使うテストの多くは認証まで
/// 到達する必要がない）。
pub fn write_empty_user_store(path: &str) {
    std::fs::write(path, "").expect("write empty user store");
}

/// 診断用に保持する listen 後の stderr 行数の上限（無制限に溜めない。
/// `three_client_e2e.rs::STDERR_TAIL_MAX_LINES` と同値）。
const STDERR_TAIL_MAX_LINES: usize = 256;

/// listen 後の stderr 行を上限付きで `tail` へ追加する（古い行から捨てる。
/// `three_client_e2e.rs::push_tail` と同型）。
fn push_tail(tail: &Mutex<VecDeque<String>>, line: String) {
    let mut guard = match tail.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.len() >= STDERR_TAIL_MAX_LINES {
        guard.pop_front();
    }
    guard.push_back(line.trim_end().to_string());
}

/// 子プロセスの stderr を 1 行ずつ読み、送信側（`tx`）が生きている間は
/// そちらへ転送し、送信失敗後（受信側が破棄された後）は `tail`（`Some` の
/// 場合のみ）へ上限付きで蓄積し続ける。いずれの場合も EOF（子プロセス終了）
/// まで読み取りを止めない。
///
/// `read_until` + `from_utf8_lossy` を使い、`BufRead::read_line` の
/// `unwrap_or(0)` とは異なり非 UTF-8 バイト列が混じってもループを抜けない
/// （Issue #1082。旧実装は送信失敗・非 UTF-8 のいずれでもループを抜けて
/// パイプの読み口を閉じていた。読み口を閉じたまま以降にサーバーが stderr へ
/// 書くと、#1081 以前は `EPIPE` で `eprintln!` が panic し panic フック
/// 〔TASK-97・RECOVER-6／TASK-99・RECOVER-8〕経由で SIGABRT 終了しうる問題が
/// あった。#1081 でサーバー側の診断ログが `engine::log_stderr!`〔書き込み
/// 失敗を無視する〕へ置き換わったため、現在は読み口を閉じてもサーバー側は
/// abort しないが、読み口を閉じると listen 後の診断行が失われる。EOF まで
/// 読み続けることで診断行の欠落を防ぐ）。
fn spawn_stderr_reader(
    stderr: ChildStderr,
    tx: mpsc::Sender<String>,
    tail: Option<Arc<Mutex<VecDeque<String>>>>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut buf: Vec<u8> = Vec::new();
        let mut forwarding = true;
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let line = String::from_utf8_lossy(&buf).into_owned();
            if forwarding {
                if let Err(mpsc::SendError(unsent)) = tx.send(line) {
                    forwarding = false;
                    if let Some(tail) = &tail {
                        push_tail(tail, unsent);
                    }
                }
            } else if let Some(tail) = &tail {
                push_tail(tail, line);
            }
        }
    })
}

/// 子プロセス（`wire-server`）の stderr を EOF まで読み続ける読み取り
/// ハンドル（Issue #1082）。[`drain_stderr`] で生成し、
/// [`Self::wait_for_listening`] で listen 行を待ち受ける。待ち受け後も
/// 読み取りスレッドは止まらず、以降の行は `tail` へ回る（[`Self::tail_lines`]
/// で後から参照できる）。`Drop` では読み取りスレッドを join しない
/// （子プロセスが生存中に join するとブロックしうるため。子プロセスの
/// kill でパイプが閉じれば読み取りスレッドは自然に終了する。
/// `three_client_e2e.rs::ServerGuard` は kill 後に明示 join するが、本型は
/// 生存確認・診断目的の直接利用〔回帰テスト〕を除き使い捨てのラッパー
/// （[`wait_for_listening`]／[`wait_for_listening_addr_and_lines`]）経由で
/// 使われるため、その利用形に合わせて join を必須にしない）。
pub struct StderrDrain {
    rx: Option<mpsc::Receiver<String>>,
    tail: Arc<Mutex<VecDeque<String>>>,
    _reader: std::thread::JoinHandle<()>,
}

impl StderrDrain {
    /// `wire-server: listening on <addr>` 行に到達するまで `timeout` 内で
    /// 待ち受け、到達アドレス（無ければ `None`）とそこまでに観測した全行
    /// （トリム済み）を返す。呼び出し後は受信側チャネルを手放し、以降の行は
    /// `tail` へ回る。
    pub fn wait_for_listening(&mut self, timeout: Duration) -> (Option<SocketAddr>, Vec<String>) {
        let deadline = Instant::now() + timeout;
        let mut lines = Vec::new();
        let mut found = None;
        if let Some(rx) = self.rx.take() {
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match rx.recv_timeout(remaining) {
                    Ok(line) => {
                        let trimmed = line.trim_end().to_string();
                        if let Some(idx) = trimmed.find("listening on ") {
                            let addr_str = trimmed[idx + "listening on ".len()..].trim();
                            if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                                lines.push(trimmed);
                                found = Some(addr);
                                break;
                            }
                        }
                        lines.push(trimmed);
                    }
                    Err(_) => break,
                }
            }
            // `rx` はここで drop され、以降の行は読み取りスレッド側で送信
            // 失敗を検知して `tail` へ切り替わる。
        }
        (found, lines)
    }

    /// 診断用に、これまで `tail` へ蓄積された行のスナップショットを返す
    /// （Mutex が poison していても `into_inner` で読む）。
    pub fn tail_lines(&self) -> Vec<String> {
        match self.tail.lock() {
            Ok(guard) => guard.iter().cloned().collect(),
            Err(poisoned) => poisoned.into_inner().iter().cloned().collect(),
        }
    }
}

/// [`StderrDrain`] を生成し、`child` の stderr を専用スレッドで EOF まで
/// 読み続ける（呼び出し元はこの呼び出しで `child.stderr` の所有権を失う）。
pub fn drain_stderr(child: &mut Child) -> StderrDrain {
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel::<String>();
    let tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    let reader = spawn_stderr_reader(stderr, tx, Some(Arc::clone(&tail)));
    StderrDrain {
        rx: Some(rx),
        tail,
        _reader: reader,
    }
}

/// `wire-server` 子プロセスの stderr から `listening on` 行への到達可否
/// だけを見る薄いラッパー（Issue #1082。7 件の CLI 系結合テストが個別に
/// 持っていた同型のローカル関数を集約する）。到達すれば `true`、`timeout`
/// または子プロセスの早期終了（パイプの EOF）では `false` を返す。
pub fn wait_for_listening(child: &mut Child, timeout: Duration) -> bool {
    let mut drain = drain_stderr(child);
    let (addr, _lines) = drain.wait_for_listening(timeout);
    addr.is_some()
}

/// `wire-server` 子プロセスの stderr から listen アドレスと、そこまでに
/// 観測した全行（トリム済み）を返す薄いラッパー（Issue #1082）。到達
/// しなければ、それまでに観測した行を含めて panic する（fail-closed。
/// 曖昧な結果で処理を続けない）。
pub fn wait_for_listening_addr_and_lines(
    child: &mut Child,
    timeout: Duration,
) -> (SocketAddr, Vec<String>) {
    let mut drain = drain_stderr(child);
    let (addr, lines) = drain.wait_for_listening(timeout);
    match addr {
        Some(addr) => (addr, lines),
        None => {
            panic!("did not observe listening address within {timeout:?}; lines so far: {lines:?}")
        }
    }
}

/// 子プロセス（`wire-server`）を起動し、stderr の行読み取りスレッド・
/// これまでに読んだ行の蓄積を束ねたハンドル。`Drop` で未 stop なら
/// kill＋wait する（呼び出し元テストが途中で panic してもゾンビ・ポート
/// 占有を残さない。既存の子プロセステストにはこの保護が無かった）。
pub struct SpawnedServer {
    child: Child,
    rx: mpsc::Receiver<String>,
    seen: Vec<String>,
    stopped: bool,
}

impl SpawnedServer {
    /// `wire-server` バイナリを `extra_args` 付きで起動する。stdout は
    /// 捨て、stderr はパイプして専用スレッド（[`spawn_stderr_reader`]。
    /// `tail` は使わず送信専用モードで動かす。呼び出し元は受信側チャネルを
    /// 持ったまま `stop_and_drain` で kill→wait するため、送信失敗後に
    /// 読み取りスレッドが読み捨てるだけの形で実害はない）で読み続ける
    /// （`BufReader::read_line` 相当はデッドラインを持たないブロッキング
    /// 呼び出しのため、呼び出し元は `recv_timeout` で確実に打ち切れる
    /// ようにする）。
    pub fn spawn(extra_args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(extra_args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server");

        let stderr = child.stderr.take().expect("piped stderr");
        let (tx, rx) = mpsc::channel::<String>();
        spawn_stderr_reader(stderr, tx, None);

        Self {
            child,
            rx,
            seen: Vec::new(),
            stopped: false,
        }
    }

    /// `wire-server: listening on <addr>` 行が来るまで待ち、その `<addr>`
    /// を返す（到達しなければ `None`）。読み取った行はすべて内部へ蓄積し、
    /// `stop_and_drain` の戻り値と合算できるようにする（`listening on` の
    /// 行数検証・表層表示行の有無検証に必要）。
    pub fn wait_for_listening(&mut self, deadline: Instant) -> Option<String> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(line) => {
                    let addr = line
                        .trim_end()
                        .strip_prefix("wire-server: listening on ")
                        .map(str::to_string);
                    self.seen.push(line);
                    if addr.is_some() {
                        return addr;
                    }
                }
                Err(_) => return None,
            }
        }
    }

    /// 子プロセスを kill＋wait したうえで、stderr 読み取りスレッドが送信
    /// 済みの残り行を回収し、蓄積済みの行と合算して返す（送信側スレッドは
    /// パイプが閉じれば終了し `recv`/`recv_timeout` は有限時間内に必ず
    /// `Err` を返す）。
    pub fn stop_and_drain(mut self, drain_deadline: Instant) -> Vec<String> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.stopped = true;
        loop {
            let remaining = drain_deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match self.rx.recv_timeout(remaining) {
                Ok(line) => self.seen.push(line),
                Err(_) => break,
            }
        }
        std::mem::take(&mut self.seen)
    }
}

impl Drop for SpawnedServer {
    fn drop(&mut self) {
        if !self.stopped {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// `wire-server` バイナリを `extra_args` 付きで起動し、終了まで待って
/// `Output` を返す（起動 CLI 引数の拒否ケース向け。listen へ到達しない
/// ことを前提に `Command::output()` で完了同期する）。
pub fn run_wire_server_to_exit(extra_args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(extra_args)
        .output()
        .expect("spawn wire-server")
}

/// 拡張クエリプロトコルの `Parse`（'P'）メッセージ本体を組み立てる
/// （`wire942_extended_transaction.rs`・`wire19_ready_for_query_status.rs`
/// で共有。`num_param_types` は事前指定するパラメータ型数）。
pub fn parse_body(name: &str, query: &str, num_param_types: i16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body.extend_from_slice(query.as_bytes());
    body.push(0);
    body.extend_from_slice(&num_param_types.to_be_bytes());
    body
}

/// 拡張クエリプロトコルの `Bind`（'B'）メッセージ本体を組み立てる
/// （パラメータなしの固定形。上記 `parse_body` と同様に共有）。
pub fn bind_body(portal: &str, statement: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes()); // param format code count
    body.extend_from_slice(&0i16.to_be_bytes()); // param count
    body.extend_from_slice(&0i16.to_be_bytes()); // result format code count
    body
}

/// 拡張クエリプロトコルの `Execute`（'E'）メッセージ本体を組み立てる
/// （上記 `parse_body`／`bind_body` と同様に共有）。
pub fn execute_body(portal: &str, max_rows: i32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(&max_rows.to_be_bytes());
    body
}

/// 型バイト＋長さプレフィックス付きの 1 メッセージを読み取り
/// `(型バイト, 本体)` を返す（拡張クエリプロトコルの応答読み取りに共有）。
pub fn read_message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut type_byte = [0u8; 1];
    stream.read_exact(&mut type_byte).expect("read type byte");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let body_len = len.checked_sub(4).expect("length must be >= 4");
    let mut body = vec![0u8; body_len];
    stream.read_exact(&mut body).expect("read body");
    (type_byte[0], body)
}
