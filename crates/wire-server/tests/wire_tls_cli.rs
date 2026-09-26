//! `--tls-cert`／`--tls-key`／`--tls-mode`／`--tls-scram-channel-binding`
//! opt-in（Issue #967・#970・親 #941・TASK-228。WIRE-7, WIRE-9, WIRE-18
//! ポインタ）をバイナリ子プロセスとして起動し、CLI 引数の受理・拒否
//! （fail-closed）・既定不変・平文ポリシー（`require`／`allow`）・
//! SCRAM-SHA-256-PLUS 提示可否・秘密値の非出力を外形的に検証する結合
//! テスト。
//!
//! `tests/wire_durability_cli.rs`（Issue #850）・`tests/wire_search_engine_cli.rs`
//! （Issue #656）と同じ流儀（実バイナリを
//! `Command::new(env!("CARGO_BIN_EXE_wire-server"))` で起動し、stderr の
//! `listening on` 行・非 0 終了・エラーメッセージを外形的に確認する）で、
//! `SSLRequest`/StartupMessage の送受信は `tests/common/tls_client.rs`
//! （TLS 1.3 最小クライアント）・本ファイル内のヘルパーを使う。
//!
//! - R1: フラグ未指定時は挙動不変（`SSLRequest` へ `'N'`・`TLS enabled` 行なし）
//! - R2: `--tls-cert`／`--tls-key`（`--tls-mode` 省略＝既定 `require`）で
//!   TLS ハンドシェイク→StartupMessage→cleartext 認証→簡易クエリ往復まで
//!   完走し、`TLS enabled (mode=require)` が 1 行だけ出ること
//! - R3: R2 と同じ構成で、`SSLRequest` を経ない平文 StartupMessage は
//!   `08P01` の ErrorResponse を受けて切断されること
//! - R4: `--tls-mode allow` は TLS 完走・平文完走の双方が成立し、ログは
//!   `mode=allow` になること
//! - R5: 組合せ不正・値欠落・重複指定・読み込み失敗（存在しないファイル・
//!   不正な PEM・鍵と証明書の公開鍵不一致・期限切れ証明書）はいずれも
//!   非 0 終了・フラグ名を含む説明が出ること。`--surface nosql` との併用は
//!   Issue #968 で HTTPS 終端へ置き換え済み
//!   （`tls_with_nosql_surface_serves_https`。層 A 網羅は
//!   `tests/http10_tls_surface.rs`）
//! - R6: 非ループバック bind × `allow` は非 0 終了し hint 行が出ること
//! - R7: R2・R5 の stderr に鍵・証明書の内容（seed の hex・PKCS#8 の
//!   base64 本文・証明書 PEM の本文）が含まれないこと
//! - R8（Issue #970）: `--tls-scram-channel-binding` 単独指定は組合せ不正・
//!   未知の語彙値は fail-closed 拒否。`enable` は AuthenticationSASL の
//!   機構リストへ `SCRAM-SHA-256-PLUS` を追加し起動ログへ運用上の注意行を
//!   1 行出す一方、未指定（既定 `disable`）は機構リストが `SCRAM-SHA-256`
//!   のみのまま・注意行も出ないこと

#[path = "common/mod.rs"]
mod common;
#[path = "common/tls_client.rs"]
mod tls_client;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// テストごとに衝突しない一時ディレクトリ（ユーザーストア・DB・証明書・鍵
/// ファイルの置き場）を確保し、`Drop` で確実に削除するガード
/// （`wire_durability_cli.rs::TempFixtureDir` と同型。証明書・鍵の置き場も
/// 兼ねるため任意ファイル名でパスを組み立てる [`Self::path`] を追加）。
struct TempFixtureDir {
    dir: std::path::PathBuf,
}

impl TempFixtureDir {
    fn new(label: &str) -> Self {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-tls-cli-{label}-{}-{}-{}",
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

/// [`tls_client::RFC8032_TEST1_SEED`]／`RFC8032_TEST1_PUBLIC_KEY` の鍵材料から
/// 有効な証明書・鍵 PEM の組を書き出す（validity 2016〜2040 年。
/// `tls_client::test_config` と同じ鍵材料を使うため、TLS ハンドシェイクは
/// `tls_client::drive_client_handshake_over_socket` でそのまま駆動できる）。
fn write_valid_tls_pair(fixture: &TempFixtureDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let seed = tls_client::hex_decode32(tls_client::RFC8032_TEST1_SEED);
    let cert_der = tls_client::build_ed25519_leaf_certificate_der_with_validity(
        &tls_client::RFC8032_TEST1_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
    );
    let cert_path = fixture.path("cert.pem");
    tls_client::write_pem_file(&cert_path, &tls_client::pem_wrap("CERTIFICATE", &cert_der));
    let key_path = fixture.path("key.pem");
    tls_client::write_pem_file(&key_path, &tls_client::ed25519_pkcs8_pem(&seed));
    (cert_path, key_path)
}

/// 有効な鍵（RFC8032 TEST1）だが、証明書の埋め込み公開鍵が異なる（鍵と
/// 証明書が不一致な）組を書き出す（R5: 鍵/証明書不一致は起動時 fail-closed
/// で拒否されることの確認用）。
fn write_mismatched_tls_pair(fixture: &TempFixtureDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let seed = tls_client::hex_decode32(tls_client::RFC8032_TEST1_SEED);
    // 証明書は別の（無関係の）公開鍵を埋め込む。
    let other_public_key = [0x11u8; 32];
    let cert_der = tls_client::build_ed25519_leaf_certificate_der_with_validity(
        &other_public_key,
        "160801121924Z",
        "401231235959Z",
    );
    let cert_path = fixture.path("mismatched-cert.pem");
    tls_client::write_pem_file(&cert_path, &tls_client::pem_wrap("CERTIFICATE", &cert_der));
    let key_path = fixture.path("mismatched-key.pem");
    tls_client::write_pem_file(&key_path, &tls_client::ed25519_pkcs8_pem(&seed));
    (cert_path, key_path)
}

/// 期限切れ（`notAfter` が過去）の証明書・有効な鍵の組を書き出す
/// （R5: 期限切れ証明書は起動時 fail-closed で拒否されることの確認用）。
fn write_expired_tls_pair(fixture: &TempFixtureDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let seed = tls_client::hex_decode32(tls_client::RFC8032_TEST1_SEED);
    let cert_der = tls_client::build_ed25519_leaf_certificate_der_with_validity(
        &tls_client::RFC8032_TEST1_PUBLIC_KEY,
        "180101000000Z",
        "190101000000Z",
    );
    let cert_path = fixture.path("expired-cert.pem");
    tls_client::write_pem_file(&cert_path, &tls_client::pem_wrap("CERTIFICATE", &cert_der));
    let key_path = fixture.path("expired-key.pem");
    tls_client::write_pem_file(&key_path, &tls_client::ed25519_pkcs8_pem(&seed));
    (cert_path, key_path)
}

/// `alice:tenant-a:<phc>` の 1 行を持つユーザーストアを書く（PHC は
/// `wire-server hash-password` を子プロセスとして呼び、平文をテストコード
/// 内に決め打ちしない。`wire_durability_cli.rs::write_user_store_with_alice`
/// と同型）。
fn write_user_store_with_alice(path: &std::path::Path) {
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

const SSL_REQUEST_CODE: i32 = 80_877_103;
const GSSENC_REQUEST_CODE: i32 = 80_877_104;

fn write_ssl_request(stream: &mut TcpStream) {
    let mut msg = Vec::new();
    msg.extend_from_slice(&8i32.to_be_bytes());
    msg.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
    stream.write_all(&msg).expect("send SSLRequest");
}

fn write_gssenc_request(stream: &mut TcpStream) {
    let mut msg = Vec::new();
    msg.extend_from_slice(&8i32.to_be_bytes());
    msg.extend_from_slice(&GSSENC_REQUEST_CODE.to_be_bytes());
    stream.write_all(&msg).expect("send GSSENCRequest");
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

/// ErrorResponse（'E'）本体から `C`（sqlstate）フィールドを取り出す。
fn read_error_sqlstate(stream: &mut impl Read) -> String {
    let (type_byte, body) = read_typed_message(stream);
    assert_eq!(type_byte, b'E', "expected ErrorResponse");
    let mut idx = 0usize;
    while idx < body.len() {
        let tag = body[idx];
        if tag == 0 {
            break;
        }
        let value_start = idx + 1;
        let nul = body[value_start..]
            .iter()
            .position(|&b| b == 0)
            .expect("null-terminated field value");
        let value_end = value_start + nul;
        let value = String::from_utf8(body[value_start..value_end].to_vec()).expect("utf-8 value");
        if tag == b'C' {
            return value;
        }
        idx = value_end + 1;
    }
    panic!("ErrorResponse did not contain a C (sqlstate) field");
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

    drain_to_ready_for_query(&mut channel);
}

/// TLS 完走: `SSLRequest` → `'S'` → ハンドシェイク → StartupMessage →
/// cleartext 認証 → 簡易クエリ往復まで完走することを確認する
/// （R2・R4 共通本体）。
fn complete_tls_round_trip(addr: std::net::SocketAddr) {
    let mut socket = TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S", "TLS-enabled server must accept SSL with 'S'");

    let client = tls_client::drive_client_handshake_over_socket(&mut socket);
    let mut channel = tls_client::TlsTestChannel::new(client, socket);

    cleartext_auth_over(&mut channel, "alice", "pw-alice");

    write_simple_query(&mut channel, "SELECT 1");
    drain_to_ready_for_query(&mut channel);
}

/// 平文完走: `SSLRequest` を送らずに StartupMessage → cleartext 認証 →
/// 簡易クエリ往復まで完走することを確認する（R4: `allow` モードの受理確認）。
fn complete_plaintext_round_trip(addr: std::net::SocketAddr) {
    let mut stream = TcpStream::connect(addr).expect("connect");
    cleartext_auth_over(&mut stream, "alice", "pw-alice");
    write_simple_query(&mut stream, "SELECT 1");
    drain_to_ready_for_query(&mut stream);
}

// --- R1: 既定不変 -----------------------------------------------------

#[test]
fn tls_flags_absent_keeps_default_plaintext_behavior() {
    let fixture = TempFixtureDir::new("r1");
    write_user_store_with_alice(&fixture.path("users.txt"));

    let mut server = common::SpawnedServer::spawn(&[
        "--users",
        &fixture.path_str("users.txt"),
        "--db",
        &fixture.db_path_str(),
        "--bind",
        "127.0.0.1:0",
    ]);
    let addr = server
        .wait_for_listening(Instant::now() + Duration::from_secs(10))
        .expect("server must reach listening state");

    let mut stream = TcpStream::connect(addr.parse::<std::net::SocketAddr>().expect("valid addr"))
        .expect("connect");
    write_ssl_request(&mut stream);
    let resp = read_exact_n(&mut stream, 1);
    assert_eq!(
        &resp, b"N",
        "TLS-disabled server must decline SSL with 'N' (unchanged behavior)"
    );

    let lines = server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    assert!(
        !lines.iter().any(|l| l.contains("TLS enabled")),
        "unexpected TLS enabled line with TLS flags absent: {lines:?}"
    );
}

// --- R2: require の完走・起動ログ ---------------------------------------

#[test]
fn tls_require_completes_handshake_and_query_round_trip() {
    let fixture = TempFixtureDir::new("r2");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (cert_path, key_path) = write_valid_tls_pair(&fixture);

    let mut server = common::SpawnedServer::spawn(&[
        "--users",
        &fixture.path_str("users.txt"),
        "--db",
        &fixture.db_path_str(),
        "--bind",
        "127.0.0.1:0",
        "--tls-cert",
        cert_path.to_str().expect("utf-8 path"),
        "--tls-key",
        key_path.to_str().expect("utf-8 path"),
    ]);
    let addr = server
        .wait_for_listening(Instant::now() + Duration::from_secs(10))
        .expect("server must reach listening state");

    complete_tls_round_trip(addr.parse().expect("valid addr"));

    let lines = server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    let enabled_lines: Vec<&String> = lines.iter().filter(|l| l.contains("TLS enabled")).collect();
    assert_eq!(
        enabled_lines.len(),
        1,
        "expected exactly one 'TLS enabled' line: {lines:?}"
    );
    assert!(
        enabled_lines[0].contains("mode=require"),
        "expected mode=require: {enabled_lines:?}"
    );
    assert_no_secret_leak(&lines, &fixture);
}

// --- R3: require の平文拒否 ---------------------------------------------

#[test]
fn tls_require_rejects_plaintext_startup_with_08p01() {
    let fixture = TempFixtureDir::new("r3");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (cert_path, key_path) = write_valid_tls_pair(&fixture);

    let mut server = common::SpawnedServer::spawn(&[
        "--users",
        &fixture.path_str("users.txt"),
        "--db",
        &fixture.db_path_str(),
        "--bind",
        "127.0.0.1:0",
        "--tls-cert",
        cert_path.to_str().expect("utf-8 path"),
        "--tls-key",
        key_path.to_str().expect("utf-8 path"),
        "--tls-mode",
        "require",
    ]);
    let addr = server
        .wait_for_listening(Instant::now() + Duration::from_secs(10))
        .expect("server must reach listening state");

    let mut stream = TcpStream::connect(addr.parse::<std::net::SocketAddr>().expect("valid addr"))
        .expect("connect");
    // `SSLRequest` を送らず、いきなり平文 StartupMessage を送る。
    write_startup_message(&mut stream, "alice", "irrelevant-db-name");

    let sqlstate = read_error_sqlstate(&mut stream);
    assert_eq!(
        sqlstate, "08P01",
        "plaintext startup under --tls-mode require must be rejected with 08P01"
    );

    // 切断されており ReadyForQuery には到達しない（読み取りが EOF/エラーに
    // なることを確認する）。
    let mut buf = [0u8; 1];
    let read_result = stream.read(&mut buf);
    assert!(
        matches!(read_result, Ok(0) | Err(_)),
        "connection must be closed after the 08P01 rejection"
    );

    server.stop_and_drain(Instant::now() + Duration::from_secs(5));
}

/// R3 の変形: `GSSENCRequest` を先に送って `'N'` を受け取ってから、平文
/// StartupMessage を送っても同じく `08P01` で拒否されることを確認する
/// （`negotiate_startup_or_upgrade` の GSSENC 分岐は `mode` を無視して
/// ループを続け、続く `PROTOCOL_VERSION_3_0` 分岐で `mode` を判定する）。
#[test]
fn tls_require_rejects_plaintext_startup_after_gssenc_with_08p01() {
    let fixture = TempFixtureDir::new("r3-gssenc");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (cert_path, key_path) = write_valid_tls_pair(&fixture);

    let mut server = common::SpawnedServer::spawn(&[
        "--users",
        &fixture.path_str("users.txt"),
        "--db",
        &fixture.db_path_str(),
        "--bind",
        "127.0.0.1:0",
        "--tls-cert",
        cert_path.to_str().expect("utf-8 path"),
        "--tls-key",
        key_path.to_str().expect("utf-8 path"),
    ]);
    let addr = server
        .wait_for_listening(Instant::now() + Duration::from_secs(10))
        .expect("server must reach listening state");

    let mut stream = TcpStream::connect(addr.parse::<std::net::SocketAddr>().expect("valid addr"))
        .expect("connect");
    write_gssenc_request(&mut stream);
    let resp = read_exact_n(&mut stream, 1);
    assert_eq!(
        &resp, b"N",
        "GSSENC must still be declined with 'N' under require"
    );

    write_startup_message(&mut stream, "alice", "irrelevant-db-name");
    let sqlstate = read_error_sqlstate(&mut stream);
    assert_eq!(
        sqlstate, "08P01",
        "plaintext startup after GSSENC under --tls-mode require must be rejected with 08P01"
    );

    server.stop_and_drain(Instant::now() + Duration::from_secs(5));
}

// --- R4: allow の受理（TLS・平文の双方） --------------------------------

#[test]
fn tls_allow_accepts_both_tls_and_plaintext() {
    let fixture = TempFixtureDir::new("r4");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (cert_path, key_path) = write_valid_tls_pair(&fixture);

    let mut server = common::SpawnedServer::spawn(&[
        "--users",
        &fixture.path_str("users.txt"),
        "--db",
        &fixture.db_path_str(),
        "--bind",
        "127.0.0.1:0",
        "--tls-cert",
        cert_path.to_str().expect("utf-8 path"),
        "--tls-key",
        key_path.to_str().expect("utf-8 path"),
        "--tls-mode",
        "allow",
    ]);
    let addr = server
        .wait_for_listening(Instant::now() + Duration::from_secs(10))
        .expect("server must reach listening state");
    let socket_addr: std::net::SocketAddr = addr.parse().expect("valid addr");

    complete_tls_round_trip(socket_addr);
    complete_plaintext_round_trip(socket_addr);

    let lines = server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    let enabled_lines: Vec<&String> = lines.iter().filter(|l| l.contains("TLS enabled")).collect();
    assert_eq!(
        enabled_lines.len(),
        1,
        "expected exactly one 'TLS enabled' line: {lines:?}"
    );
    assert!(
        enabled_lines[0].contains("mode=allow"),
        "expected mode=allow: {enabled_lines:?}"
    );
}

// --- R5: 起動失敗（非 0 終了） -------------------------------------------

fn assert_startup_rejected(extra_args: &[&str], expected_substring: &str) {
    let output = common::run_wire_server_to_exit(extra_args);
    assert!(
        !output.status.success(),
        "expected non-zero exit for args {extra_args:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(expected_substring),
        "expected stderr to contain {expected_substring:?}, got: {stderr}"
    );
}

#[test]
fn tls_cert_alone_is_rejected() {
    let fixture = TempFixtureDir::new("r5-cert-alone");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (cert_path, _key_path) = write_valid_tls_pair(&fixture);
    assert_startup_rejected(
        &[
            "--users",
            &fixture.path_str("users.txt"),
            "--db",
            &fixture.db_path_str(),
            "--bind",
            "127.0.0.1:0",
            "--tls-cert",
            cert_path.to_str().expect("utf-8 path"),
        ],
        "--tls-key",
    );
}

#[test]
fn tls_key_alone_is_rejected() {
    let fixture = TempFixtureDir::new("r5-key-alone");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (_cert_path, key_path) = write_valid_tls_pair(&fixture);
    assert_startup_rejected(
        &[
            "--users",
            &fixture.path_str("users.txt"),
            "--db",
            &fixture.db_path_str(),
            "--bind",
            "127.0.0.1:0",
            "--tls-key",
            key_path.to_str().expect("utf-8 path"),
        ],
        "--tls-cert",
    );
}

#[test]
fn tls_mode_alone_is_rejected() {
    let fixture = TempFixtureDir::new("r5-mode-alone");
    write_user_store_with_alice(&fixture.path("users.txt"));
    assert_startup_rejected(
        &[
            "--users",
            &fixture.path_str("users.txt"),
            "--db",
            &fixture.db_path_str(),
            "--bind",
            "127.0.0.1:0",
            "--tls-mode",
            "require",
        ],
        "--tls-mode",
    );
}

#[test]
fn tls_mode_rejects_unknown_value() {
    let fixture = TempFixtureDir::new("r5-mode-bad");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (cert_path, key_path) = write_valid_tls_pair(&fixture);
    assert_startup_rejected(
        &[
            "--users",
            &fixture.path_str("users.txt"),
            "--db",
            &fixture.db_path_str(),
            "--bind",
            "127.0.0.1:0",
            "--tls-cert",
            cert_path.to_str().expect("utf-8 path"),
            "--tls-key",
            key_path.to_str().expect("utf-8 path"),
            "--tls-mode",
            "prefer",
        ],
        "--tls-mode",
    );
}

#[test]
fn tls_cert_missing_value_is_rejected() {
    let fixture = TempFixtureDir::new("r5-cert-no-value");
    write_user_store_with_alice(&fixture.path("users.txt"));
    assert_startup_rejected(
        &[
            "--users",
            &fixture.path_str("users.txt"),
            "--db",
            &fixture.db_path_str(),
            "--tls-cert",
        ],
        "--tls-cert",
    );
}

#[test]
fn tls_cert_duplicate_flag_is_rejected() {
    let fixture = TempFixtureDir::new("r5-cert-dup");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (cert_path, key_path) = write_valid_tls_pair(&fixture);
    let cert_str = cert_path.to_str().expect("utf-8 path");
    assert_startup_rejected(
        &[
            "--users",
            &fixture.path_str("users.txt"),
            "--db",
            &fixture.db_path_str(),
            "--tls-cert",
            cert_str,
            "--tls-cert",
            cert_str,
            "--tls-key",
            key_path.to_str().expect("utf-8 path"),
        ],
        "specified more than once",
    );
}

#[test]
fn tls_nonexistent_files_are_rejected() {
    let fixture = TempFixtureDir::new("r5-nonexistent");
    write_user_store_with_alice(&fixture.path("users.txt"));
    assert_startup_rejected(
        &[
            "--users",
            &fixture.path_str("users.txt"),
            "--db",
            &fixture.db_path_str(),
            "--tls-cert",
            "/nonexistent/cert.pem",
            "--tls-key",
            "/nonexistent/key.pem",
        ],
        "failed to load TLS configuration",
    );
}

#[test]
fn tls_garbage_key_pem_is_rejected() {
    let fixture = TempFixtureDir::new("r5-garbage-key");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (cert_path, _) = write_valid_tls_pair(&fixture);
    let key_path = fixture.path("garbage-key.pem");
    tls_client::write_pem_file(
        &key_path,
        "-----BEGIN PRIVATE KEY-----\nAA==\n-----END PRIVATE KEY-----\n",
    );
    assert_startup_rejected(
        &[
            "--users",
            &fixture.path_str("users.txt"),
            "--db",
            &fixture.db_path_str(),
            "--tls-cert",
            cert_path.to_str().expect("utf-8 path"),
            "--tls-key",
            key_path.to_str().expect("utf-8 path"),
        ],
        "failed to load TLS configuration",
    );
}

#[test]
fn tls_mismatched_key_and_cert_are_rejected() {
    let fixture = TempFixtureDir::new("r5-mismatch");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (cert_path, key_path) = write_mismatched_tls_pair(&fixture);
    assert_startup_rejected(
        &[
            "--users",
            &fixture.path_str("users.txt"),
            "--db",
            &fixture.db_path_str(),
            "--tls-cert",
            cert_path.to_str().expect("utf-8 path"),
            "--tls-key",
            key_path.to_str().expect("utf-8 path"),
        ],
        "failed to load TLS configuration",
    );
}

#[test]
fn tls_expired_certificate_is_rejected() {
    let fixture = TempFixtureDir::new("r5-expired");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (cert_path, key_path) = write_expired_tls_pair(&fixture);
    assert_startup_rejected(
        &[
            "--users",
            &fixture.path_str("users.txt"),
            "--db",
            &fixture.db_path_str(),
            "--tls-cert",
            cert_path.to_str().expect("utf-8 path"),
            "--tls-key",
            key_path.to_str().expect("utf-8 path"),
        ],
        "failed to load TLS configuration",
    );
}

/// `POST <target>` の HTTP/1.1 要求バイト列を組み立てる（`http_common::
/// build_request` の最小限のコピー。本ファイルは `http_common` を取り込む
/// 構成ではないため、CLI 結合テストの流儀（自己完結したヘルパー）に合わせて
/// 複製する）。
fn build_http_request(target: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
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

/// 受信済みバイト列が完全な 1 応答（ヘッダ終端＋`Content-Length` ぶんの
/// 本文）になっていれば、その総バイト長を返す（`TlsTestChannel::read` が
/// `close_notify` レコードに到達すると `read_application_data` が panic
/// するため、応答受信後に追加で `read` を呼ばない設計が必要。
/// `http10_tls_surface.rs::complete_response_len` と同じ実装）。
fn complete_http_response_len(bytes: &[u8]) -> Option<usize> {
    let sep = bytes.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = std::str::from_utf8(bytes.get(..sep)?).ok()?;
    let content_length: usize = head
        .split("\r\n")
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .and_then(|v| v.parse().ok())?;
    Some(sep + 4 + content_length)
}

/// TLS チャネル上で 1 応答ぶんちょうど読み切り、`HTTP/1.1 <status>` 行を返す。
fn read_one_http_status_line(channel: &mut (impl Read + Write)) -> String {
    let mut received = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Some(total) = complete_http_response_len(&received) {
            if received.len() >= total {
                break;
            }
        }
        let n = channel.read(&mut buf).expect("read response chunk");
        assert!(
            n > 0,
            "connection closed before a complete response arrived"
        );
        received.extend_from_slice(&buf[..n]);
    }
    let text = String::from_utf8_lossy(&received);
    text.lines()
        .next()
        .expect("status line present")
        .to_string()
}

// --- #968: NoSQL 表層も同じ TLS opt-in で HTTPS 終端する -------------------

/// R5 の旧テスト（`tls_with_nosql_surface_is_rejected`）は Issue #968 で
/// 置き換え済み: `--surface nosql` は `--tls-cert`／`--tls-key`／
/// `--tls-mode` を SQL 表層と同じ意味で受理し、HTTP リスナーを HTTPS として
/// 終端する（`wire_server::http::tls_transport`）。接続処理の中身（TLS／
/// 平文判定・ハンドシェイク・上限超過時の挙動等）の層 A 網羅は
/// `tests/http10_tls_surface.rs` が担い、本テストは CLI 結線（起動ログ・
/// 実バイナリ経由での TLS 完走・平文拒否）の外形のみを確認する。
#[test]
fn tls_with_nosql_surface_serves_https() {
    let fixture = TempFixtureDir::new("r5-nosql");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (cert_path, key_path) = write_valid_tls_pair(&fixture);

    let mut server = common::SpawnedServer::spawn(&[
        "--users",
        &fixture.path_str("users.txt"),
        "--db",
        &fixture.db_path_str(),
        "--bind",
        "127.0.0.1:0",
        "--surface",
        "nosql",
        "--tls-cert",
        cert_path.to_str().expect("utf-8 path"),
        "--tls-key",
        key_path.to_str().expect("utf-8 path"),
    ]);
    let addr = server
        .wait_for_listening(Instant::now() + Duration::from_secs(10))
        .expect("server must reach listening state")
        .parse::<std::net::SocketAddr>()
        .expect("valid addr");

    // TLS 経路: ハンドシェイク → `/v1/session` が 200 で完走すること。
    let mut socket = TcpStream::connect(addr).expect("connect");
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    let client = tls_client::drive_client_handshake_over_socket(&mut socket);
    let mut channel = tls_client::TlsTestChannel::new(client, socket);
    let login_body = br#"{"user":"alice","password":"pw-alice"}"#;
    let request = build_http_request(
        "/v1/session",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", &login_body.len().to_string()),
        ],
        login_body,
    );
    channel
        .write_all(&request)
        .expect("write session request over tls");
    let status_line = read_one_http_status_line(&mut channel);
    assert_eq!(
        status_line, "HTTP/1.1 200 OK",
        "nosql surface must terminate TLS and complete /v1/session"
    );
    drop(channel);

    // 平文経路: `--tls-mode` 未指定＝既定 `require` の下では、TLS
    // ハンドシェイクを経ない平文 HTTP 接続へ一切応答しない。
    let mut plaintext = TcpStream::connect(addr).expect("connect plaintext");
    plaintext
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("set read timeout");
    let _ = plaintext.write_all(&request);
    let mut buf = [0u8; 8];
    match plaintext.read(&mut buf) {
        Ok(0) => {}
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => {}
        other => {
            panic!("expected no HTTP response over plaintext when tls-mode=require, got {other:?}")
        }
    }

    let lines = server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    assert!(
        lines.iter().any(|l| l.contains("surface nosql")),
        "expected a 'surface nosql' line, got: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.contains("TLS enabled (mode=require)")),
        "expected a 'TLS enabled (mode=require)' line, got: {lines:?}"
    );
}

// --- R6: 非ループバック × allow は起動拒否 ------------------------------

#[test]
fn tls_allow_rejects_non_loopback_bind() {
    let fixture = TempFixtureDir::new("r6");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (cert_path, key_path) = write_valid_tls_pair(&fixture);
    let output = common::run_wire_server_to_exit(&[
        "--users",
        &fixture.path_str("users.txt"),
        "--db",
        &fixture.db_path_str(),
        "--bind",
        "0.0.0.0:0",
        "--tls-cert",
        cert_path.to_str().expect("utf-8 path"),
        "--tls-key",
        key_path.to_str().expect("utf-8 path"),
        "--tls-mode",
        "allow",
    ]);
    assert!(
        !output.status.success(),
        "non-loopback bind under --tls-mode allow must be rejected"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--tls-mode allow"),
        "expected hint mentioning --tls-mode allow: {stderr}"
    );
    assert!(
        stderr.contains("require"),
        "expected hint suggesting --tls-mode require: {stderr}"
    );
}

// --- R7: 秘密値の非出力 ---------------------------------------------------

/// 起動ログ・エラーメッセージに鍵の seed（hex）・PKCS#8 の base64 本文・
/// 証明書 PEM の本文が含まれないことを確認する（R2・R5 共通の検証）。
fn assert_no_secret_leak(lines: &[String], fixture: &TempFixtureDir) {
    let joined = lines.join("\n");
    assert!(
        !joined.contains(tls_client::RFC8032_TEST1_SEED),
        "stderr must not contain the raw key seed hex: {joined}"
    );
    // 証明書・鍵ファイルが存在する場合、PEM 本文の一部（base64 行）が
    // stderr へそのまま出ていないことも確認する。
    for name in ["cert.pem", "key.pem"] {
        let path = fixture.path(name);
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Some(first_b64_line) = content.lines().nth(1) {
                if !first_b64_line.is_empty() {
                    assert!(
                        !joined.contains(first_b64_line),
                        "stderr must not contain PEM body content: {joined}"
                    );
                }
            }
        }
    }
}

#[test]
fn tls_error_messages_do_not_leak_secret_material() {
    let fixture = TempFixtureDir::new("r7");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (cert_path, key_path) = write_mismatched_tls_pair(&fixture);
    let output = common::run_wire_server_to_exit(&[
        "--users",
        &fixture.path_str("users.txt"),
        "--db",
        &fixture.db_path_str(),
        "--tls-cert",
        cert_path.to_str().expect("utf-8 path"),
        "--tls-key",
        key_path.to_str().expect("utf-8 path"),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains(tls_client::RFC8032_TEST1_SEED),
        "stderr must not contain the raw key seed hex: {stderr}"
    );
}

// --- R8: --tls-scram-channel-binding（Issue #970） -----------------------

#[test]
fn tls_scram_channel_binding_alone_is_rejected() {
    let fixture = TempFixtureDir::new("r8-scram-cb-alone");
    write_user_store_with_alice(&fixture.path("users.txt"));
    assert_startup_rejected(
        &[
            "--users",
            &fixture.path_str("users.txt"),
            "--db",
            &fixture.db_path_str(),
            "--bind",
            "127.0.0.1:0",
            "--tls-scram-channel-binding",
            "enable",
        ],
        "--tls-scram-channel-binding",
    );
}

#[test]
fn tls_scram_channel_binding_rejects_unknown_value() {
    let fixture = TempFixtureDir::new("r8-scram-cb-bad");
    write_user_store_with_alice(&fixture.path("users.txt"));
    let (cert_path, key_path) = write_valid_tls_pair(&fixture);
    assert_startup_rejected(
        &[
            "--users",
            &fixture.path_str("users.txt"),
            "--db",
            &fixture.db_path_str(),
            "--bind",
            "127.0.0.1:0",
            "--tls-cert",
            cert_path.to_str().expect("utf-8 path"),
            "--tls-key",
            key_path.to_str().expect("utf-8 path"),
            "--tls-scram-channel-binding",
            "true",
        ],
        "--tls-scram-channel-binding",
    );
}

/// SCRAM 検証子を持つユーザーストア（`alice`）と、`--scram-mock-key-file`
/// 用のモック鍵ファイルを書き出す（`wire_scram_plus_psql_interop.rs::
/// write_scram_user_store_file` と同じ構成要素。ファイルをまたいだ
/// private ヘルパーの共有はできないため意図的に重複させる）。
fn write_scram_user_store_and_mock_key(
    fixture: &TempFixtureDir,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let scram_salt = [7u8; wire_server::auth::scram::SALT_LEN];
    let verifier = wire_server::auth::scram::generate_verifier(
        b"pw-alice",
        &scram_salt,
        wire_server::auth::scram::SCRAM_ITERATIONS,
    )
    .expect("valid verifier");
    let phc = wire_server::auth::argon2id::encode_phc(
        b"unused-in-scram-mode",
        b"0123456789abcdef",
        &wire_server::auth::argon2id::RECOMMENDED_PARAMS,
    )
    .expect("valid phc");
    let users_path = fixture.path("users.txt");
    std::fs::write(
        &users_path,
        format!("alice:tenant-a:{phc}:{}\n", verifier.to_verifier_string()),
    )
    .expect("write scram user store");

    let mock_key_path = fixture.path("scram-mock-key.bin");
    std::fs::write(
        &mock_key_path,
        vec![9u8; wire_server::auth_method_opt::SCRAM_MOCK_KEY_FILE_MIN_LEN],
    )
    .expect("write scram mock key file");
    (users_path, mock_key_path)
}

/// AuthenticationSASL（コード 10）の機構リストを読む（`wire_scram_plus_tls.rs::
/// read_authentication_sasl_mechanisms` と同じ構成要素。意図的な重複）。
fn read_authentication_sasl_mechanisms(stream: &mut impl Read) -> Vec<String> {
    let (ty, body) = read_typed_message(stream);
    assert_eq!(ty, b'R', "expected AuthenticationSASL");
    let code = i32::from_be_bytes(body[..4].try_into().expect("4 bytes"));
    assert_eq!(code, 10, "AuthenticationSASL code must be 10");
    body[4..]
        .split(|&b| b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// TLS ハンドシェイク完走後に StartupMessage を送り、AuthenticationSASL の
/// 機構リストを読むところまでを共通化する（SCRAM 交換本体はこのテストの
/// 対象外。機構リストの内容だけが `--tls-scram-channel-binding` の効果を
/// 判定する対象）。
fn connect_tls_and_read_sasl_mechanisms(addr: std::net::SocketAddr) -> Vec<String> {
    let mut socket = TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut socket);
    let resp = read_exact_n(&mut socket, 1);
    assert_eq!(&resp, b"S", "TLS-enabled server must accept SSL with 'S'");

    let client = tls_client::drive_client_handshake_over_socket(&mut socket);
    let mut channel = tls_client::TlsTestChannel::new(client, socket);

    write_startup_message(&mut channel, "alice", "irrelevant-db-name");
    read_authentication_sasl_mechanisms(&mut channel)
}

#[test]
fn tls_scram_channel_binding_enable_advertises_plus_mechanism() {
    let fixture = TempFixtureDir::new("r8-scram-cb-enable");
    let (users_path, mock_key_path) = write_scram_user_store_and_mock_key(&fixture);
    let (cert_path, key_path) = write_valid_tls_pair(&fixture);

    let mut server = common::SpawnedServer::spawn(&[
        "--users",
        users_path.to_str().expect("utf-8 path"),
        "--db",
        &fixture.db_path_str(),
        "--bind",
        "127.0.0.1:0",
        "--auth-method",
        "scram-sha-256",
        "--scram-mock-key-file",
        mock_key_path.to_str().expect("utf-8 path"),
        "--tls-cert",
        cert_path.to_str().expect("utf-8 path"),
        "--tls-key",
        key_path.to_str().expect("utf-8 path"),
        "--tls-scram-channel-binding",
        "enable",
    ]);
    let addr = server
        .wait_for_listening(Instant::now() + Duration::from_secs(10))
        .expect("server must reach listening state");

    let mechanisms = connect_tls_and_read_sasl_mechanisms(addr.parse().expect("valid addr"));
    assert!(
        mechanisms.contains(&wire_server::auth::scram::MECHANISM_NAME_PLUS.to_string()),
        "expected SCRAM-SHA-256-PLUS to be advertised: {mechanisms:?}"
    );

    let lines = server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    assert!(
        lines
            .iter()
            .any(|l| l.contains("SCRAM-SHA-256-PLUS advertisement enabled")),
        "expected an advisory line for --tls-scram-channel-binding enable: {lines:?}"
    );
    assert_no_secret_leak(&lines, &fixture);
}

#[test]
fn tls_scram_channel_binding_unset_does_not_advertise_plus_mechanism() {
    let fixture = TempFixtureDir::new("r8-scram-cb-default");
    let (users_path, mock_key_path) = write_scram_user_store_and_mock_key(&fixture);
    let (cert_path, key_path) = write_valid_tls_pair(&fixture);

    let mut server = common::SpawnedServer::spawn(&[
        "--users",
        users_path.to_str().expect("utf-8 path"),
        "--db",
        &fixture.db_path_str(),
        "--bind",
        "127.0.0.1:0",
        "--auth-method",
        "scram-sha-256",
        "--scram-mock-key-file",
        mock_key_path.to_str().expect("utf-8 path"),
        "--tls-cert",
        cert_path.to_str().expect("utf-8 path"),
        "--tls-key",
        key_path.to_str().expect("utf-8 path"),
    ]);
    let addr = server
        .wait_for_listening(Instant::now() + Duration::from_secs(10))
        .expect("server must reach listening state");

    let mechanisms = connect_tls_and_read_sasl_mechanisms(addr.parse().expect("valid addr"));
    assert_eq!(
        mechanisms,
        vec![wire_server::auth::scram::MECHANISM_NAME.to_string()],
        "expected only SCRAM-SHA-256 (no PLUS) with the default (disable): {mechanisms:?}"
    );

    let lines = server.stop_and_drain(Instant::now() + Duration::from_secs(5));
    assert!(
        !lines
            .iter()
            .any(|l| l.contains("SCRAM-SHA-256-PLUS advertisement enabled")),
        "unexpected PLUS advisory line with the default (disable): {lines:?}"
    );
}
