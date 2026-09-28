//! TLS 接続での緊急応答（RECOVER-6・Issue #1080）を、実バイナリ子プロセス
//! （`CARGO_BIN_EXE_wire-server`）に対する TLS 1.3 ハンドシェイクを通じて
//! 外形的に検証する結合テスト。`tests/wire_fault_injection_cli.rs`
//! （平文接続の同種テスト。TASK-97・RECOVER-6・ERR-5）・`tests/wire_tls_cli.rs`
//! （TLS opt-in の CLI 受入テスト。Issue #967）と同じ流儀を組み合わせる。
//!
//! `--fault-inject post-commit-panic`（feature `fault-injection` 限定。
//! Issue #705）と `--tls-cert`／`--tls-key`（Issue #967）を同時に指定し、
//! commit 成功 `INSERT` の直後に送出される緊急応答が TLS レコードとして
//! 正しく復号できること・唯一の応答であること・プロセスが abort すること
//! （RECOVER-8）・commit 済み行が再オープン後も可視であることを固定する。

#![cfg(feature = "fault-injection")]

#[path = "common/tls_client.rs"]
mod tls_client;

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::storage::{Storage, Visibility};

const SSL_REQUEST_CODE: i32 = 80_877_103;

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ディレクトリ（`wire_fault_injection_cli.rs::
/// TempFixtureDir`・`wire_tls_cli.rs::TempFixtureDir` と同型。証明書・鍵の
/// 置き場も兼ねる）。
struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-fault-injection-tls-cli-{label}-{}-{}-{}",
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

    fn path(&self, name: &str) -> std::path::PathBuf {
        self.dir.join(name)
    }

    fn path_str(&self, name: &str) -> String {
        self.path(name).to_str().expect("utf-8 path").to_string()
    }

    fn db_path_str(&self) -> String {
        self.path_str("db.redb")
    }
}

impl Drop for TempFixtureDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `wire_tls_cli.rs::write_valid_tls_pair` と同じ鍵材料（`tls_client::
/// RFC8032_TEST1_SEED`／`RFC8032_TEST1_PUBLIC_KEY`）で証明書・鍵ペアを
/// 書き出す。`tls_client::drive_client_handshake_over_socket` はこの鍵材料
/// を前提にハンドシェイクを駆動する。
fn write_valid_tls_pair(fixture: &TempFixtureDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let seed = tls_client::hex_decode32(tls_client::RFC8032_TEST1_SEED);
    let cert_der = tls_client::build_ed25519_leaf_certificate_der_with_validity(
        &tls_client::RFC8032_TEST1_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
    );
    let cert_path = fixture.path("server.crt");
    let key_path = fixture.path("server.key");
    tls_client::write_pem_file(&cert_path, &tls_client::pem_wrap("CERTIFICATE", &cert_der));
    tls_client::write_pem_file(&key_path, &tls_client::ed25519_pkcs8_pem(&seed));
    (cert_path, key_path)
}

/// `wire_fault_injection_cli.rs::armed::write_user_store_with_alice` と同型
/// （`wire-server hash-password` サブコマンドを実経路で通す）。
fn write_user_store_with_alice(path: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .arg("hash-password")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn hash-password");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(b"pw-alice\n")
        .expect("write password to stdin");
    let output = child.wait_with_output().expect("wait hash-password");
    assert!(output.status.success(), "hash-password must succeed");
    let phc = String::from_utf8(output.stdout)
        .expect("utf-8 phc")
        .trim()
        .to_string();
    std::fs::write(path, format!("alice:tenant-a:{phc}\n")).expect("write user store");
}

/// `wire_fault_injection_cli.rs::armed::create_empty_docs_table` と同型。
fn create_empty_docs_table(db_path: &str) {
    let storage = Storage::open(db_path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    drop(storage);
}

fn insert_sql(id: u64, op_id: &str) -> String {
    format!(
        "INSERT INTO docs (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]') USING OPERATION_ID '{op_id}'"
    )
}

/// `wire_fault_injection_cli.rs::armed::wait_for_listening_addr_and_lines`
/// と同型。
fn wait_for_listening_addr_and_lines(
    child: &mut Child,
    timeout: Duration,
) -> (std::net::SocketAddr, Vec<String>) {
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        loop {
            line.clear();
            let n = reader.read_line(&mut line).unwrap_or(0);
            if n == 0 || tx.send(std::mem::take(&mut line)).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + timeout;
    let mut lines = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            panic!("did not observe listening address within {timeout:?}; lines so far: {lines:?}");
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                let trimmed = line.trim_end().to_string();
                if let Some(idx) = line.find("listening on ") {
                    let addr_str = line[idx + "listening on ".len()..].trim();
                    let addr: std::net::SocketAddr = addr_str.parse().expect("parse listen addr");
                    lines.push(trimmed);
                    return (addr, lines);
                }
                lines.push(trimmed);
            }
            Err(_) => panic!(
                "stderr channel closed before observing listening address; lines so far: {lines:?}"
            ),
        }
    }
}

fn wait_for_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!("subprocess did not terminate within {timeout:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(unix)]
fn assert_aborted(status: std::process::ExitStatus) {
    use std::os::unix::process::ExitStatusExt as _;
    assert!(
        !status.success(),
        "child must not exit successfully; status={status:?}"
    );
    assert_eq!(
        status.signal(),
        Some(6),
        "child must be terminated by SIGABRT (std::process::abort); status={status:?}"
    );
}

fn write_ssl_request(stream: &mut std::net::TcpStream) {
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

fn write_password_message(stream: &mut impl Write, password: &str) {
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

fn write_simple_query(stream: &mut impl Write, sql: &str) {
    let mut body = Vec::new();
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::new();
    msg.push(b'Q');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    stream.write_all(&msg).expect("send simple query");
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

fn cleartext_auth_over(mut channel: &mut (impl Read + Write), username: &str, password: &str) {
    write_startup_message(&mut channel, username, "irrelevant-db-name");
    let (type_byte, body) = read_typed_message(&mut channel);
    assert_eq!(type_byte, b'R', "expected Authentication* message");
    let auth_code = i32::from_be_bytes(body[..4].try_into().expect("4 bytes"));
    assert_eq!(auth_code, 3, "AuthenticationCleartextPassword expected");

    write_password_message(&mut channel, password);
    let (type_byte, body) = read_typed_message(&mut channel);
    assert_eq!(type_byte, b'R', "expected AuthenticationOk");
    let code = i32::from_be_bytes(body[..4].try_into().expect("4 bytes"));
    assert_eq!(code, 0, "AuthenticationOk code");

    // ParameterStatus*・BackendKeyData を読み飛ばし ReadyForQuery まで進む。
    loop {
        let (type_byte, _body) = read_typed_message(&mut channel);
        if type_byte == b'Z' {
            return;
        }
    }
}

/// body 中の 1 フィールド（タグ 1 バイト＋NUL 終端文字列）を機械的に抽出
/// する（`wire_fault_injection_cli.rs::armed::find_field` と同型）。
fn find_field(body: &[u8], tag: u8) -> Option<String> {
    let mut idx = 0;
    while idx < body.len() {
        let this_tag = *body.get(idx)?;
        if this_tag == 0 {
            return None;
        }
        let value_start = idx + 1;
        let nul_offset = body.get(value_start..)?.iter().position(|&b| b == 0)?;
        let value_end = value_start + nul_offset;
        if this_tag == tag {
            let bytes = body.get(value_start..value_end)?;
            return std::str::from_utf8(bytes).ok().map(str::to_string);
        }
        idx = value_end + 1;
    }
    None
}

/// TLS で復号した `E`（ErrorResponse）フレームが緊急応答
/// （`C`=`XX000`・`D`=`state=may_be_committed`）であることを確認する
/// （`wire_fault_injection_cli.rs::armed::assert_emergency_response` の
/// TLS 版。`channel` は復号済み平文を返す `impl Read`）。
fn assert_emergency_response_over_tls(channel: &mut impl Read) {
    let (type_byte, body) = read_typed_message(channel);
    assert_eq!(type_byte, b'E', "expected ErrorResponse type byte");

    assert_eq!(
        find_field(&body, b'S').as_deref(),
        Some("ERROR"),
        "severity"
    );
    assert_eq!(
        find_field(&body, b'C').as_deref(),
        Some("XX000"),
        "sqlstate"
    );
    assert_eq!(
        find_field(&body, b'M').as_deref(),
        Some("internal error"),
        "message"
    );
    assert_eq!(
        find_field(&body, b'D').as_deref(),
        Some("state=may_be_committed"),
        "detail (ERR-5)"
    );
    assert_eq!(
        body.iter().filter(|&&b| b == b'D').count(),
        1,
        "D field must appear exactly once"
    );
}

/// 受入基準 1: arm 済み・TLS 接続・commit 成功 `INSERT` の直後に、緊急応答
/// が正しく暗号化された TLS レコードとしてクライアントへ届き、復号すると
/// 平文接続と同じ `ErrorResponse` として観測できること。プロセスは abort
/// し（RECOVER-8）、close_notify を含む追加バイトは届かない
/// （sole-response 契約。生ソケット側で検査する。`TlsTestChannel::read` は
/// EOF で panic するため使わない）。commit 自体は成功しているため再オープン
/// 後も行は可視のまま。
#[test]
fn tls_armed_post_commit_panic_sends_emergency_response_as_ciphertext_then_aborts() {
    let fixture = TempFixtureDir::new("tls-armed-fire");
    write_user_store_with_alice(&fixture.path_str("users.txt"));
    create_empty_docs_table(&fixture.db_path_str());
    let (cert_path, key_path) = write_valid_tls_pair(&fixture);
    let db_path = fixture.db_path_str();

    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            &fixture.path_str("users.txt"),
            "--db",
            &db_path,
            "--bind",
            "127.0.0.1:0",
            "--tls-cert",
            cert_path.to_str().expect("utf-8 path"),
            "--tls-key",
            key_path.to_str().expect("utf-8 path"),
            "--fault-inject",
            "post-commit-panic",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server");

    let (addr, lines) = wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));
    assert!(
        lines.iter().any(|l| l.contains("fault injection armed")),
        "expected 'fault injection armed' line; lines={lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("TLS enabled")),
        "expected 'TLS enabled' line; lines={lines:?}"
    );

    let mut socket = std::net::TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S", "TLS-enabled server must accept SSL with 'S'");

    let client = tls_client::drive_client_handshake_over_socket(&mut socket);
    let mut channel = tls_client::TlsTestChannel::new(client, socket);

    cleartext_auth_over(&mut channel, "alice", "pw-alice");

    write_simple_query(&mut channel, &insert_sql(1, "fi-tls-armed-op-1"));
    assert_emergency_response_over_tls(&mut channel);

    // 唯一の応答であることを生ソケット側で確認する（`into_parts` で
    // `TestClient`・生ソケットを取り出す。`TlsTestChannel::read` は EOF で
    // panic するため sole-response 検査には使わない。プランに記載の
    // 「`Ok(0)` または `Err`。close_notify も届かない」契約の確認）。
    let (_client, mut raw_socket) = channel.into_parts();
    raw_socket
        .set_read_timeout(Some(Duration::from_millis(300)))
        .expect("set short read timeout");
    let mut extra = [0u8; 16];
    let n = raw_socket.read(&mut extra).unwrap_or(0);
    assert_eq!(
        n, 0,
        "no additional bytes must follow the emergency response (sole-response contract)"
    );

    let status = wait_for_exit(&mut child, Duration::from_secs(30));
    assert_aborted(status);

    // commit 自体は成功しているため、再オープン後も行が可視であること
    // （`wire_fault_injection_cli.rs::armed` と同じ確認方法）。
    let storage = Storage::open(&db_path).expect("reopen storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id FROM docs ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    assert_eq!(
        result.rows.len(),
        1,
        "the committed row must remain visible after the emergency-abort path"
    );
}
