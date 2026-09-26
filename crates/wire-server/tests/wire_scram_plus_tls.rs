//! TLS 上の SCRAM-SHA-256-PLUS（チャネルバインディング `tls-server-end-point`。
//! RFC 5929 §4・RFC 5802 §6/§7。Issue #970・親 #941・WIRE-18・TASK-222・
//! TASK-228・WIRE-9 ポインタ）の結合テスト。
//!
//! `tests/common/tls_client.rs` で実 TLS 1.3 ハンドシェイクを駆動し、以後は
//! `TlsTestChannel`（`impl Read + Write`）上で SASL 往復を行う。SCRAM の
//! クライアント側計算は `tests/wire_scram_auth.rs` と同じ方針（
//! `wire_server::auth::scram`／`base64_std` の公開 API・独立実装の proof
//! 計算）を踏襲するが、ファイルをまたいだ private ヘルパーの共有はできない
//! ため、本ファイル内に同型のヘルパーを持つ（意図的な重複）。

#[path = "common/tls_client.rs"]
mod tls_client;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use wire_server::auth::hmac_sha256::{hmac_sha256, pbkdf2_hmac_sha256_one_block};
use wire_server::auth::{base64_std, scram, UserStore};
use wire_server::limits::ConnectionLimiter;
use wire_server::tls::ed25519::SigningKey;
use wire_server::tls::server_handshake::TlsServerConfig;
use wire_server::tls::x509::ServerCertificateChain;

const SSL_REQUEST_CODE: i32 = 80_877_103;

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

fn read_message(stream: &mut impl Read) -> (u8, Vec<u8>) {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    (header[0], body)
}

fn read_authentication_sasl_mechanisms(stream: &mut impl Read) -> Vec<String> {
    let (ty, body) = read_message(stream);
    assert_eq!(ty, b'R', "expected AuthenticationSASL");
    let mut code_buf = [0u8; 4];
    code_buf.copy_from_slice(&body[..4]);
    assert_eq!(
        i32::from_be_bytes(code_buf),
        10,
        "AuthenticationSASL code must be 10"
    );
    body[4..]
        .split(|&b| b == 0)
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

fn send_sasl_initial_response(stream: &mut impl Write, mechanism: &str, client_first: &[u8]) {
    let mut body = Vec::new();
    body.extend_from_slice(mechanism.as_bytes());
    body.push(0);
    body.extend_from_slice(&(client_first.len() as i32).to_be_bytes());
    body.extend_from_slice(client_first);
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::new();
    msg.push(b'p');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    stream.write_all(&msg).expect("send SASLInitialResponse");
}

fn send_sasl_response(stream: &mut impl Write, body: &[u8]) {
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::new();
    msg.push(b'p');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(body);
    stream.write_all(&msg).expect("send SASLResponse");
}

fn read_sasl_continue(stream: &mut impl Read) -> Vec<u8> {
    let (ty, body) = read_message(stream);
    assert_eq!(ty, b'R', "expected AuthenticationSASLContinue");
    let mut code_buf = [0u8; 4];
    code_buf.copy_from_slice(&body[..4]);
    assert_eq!(
        i32::from_be_bytes(code_buf),
        11,
        "AuthenticationSASLContinue code must be 11"
    );
    body[4..].to_vec()
}

struct ServerFirst {
    server_first_body: Vec<u8>,
    nonce: String,
    salt: Vec<u8>,
    iterations: u32,
}

fn parse_server_first(body: Vec<u8>) -> ServerFirst {
    let text = std::str::from_utf8(&body).expect("server-first must be utf8");
    let mut nonce = None;
    let mut salt = None;
    let mut iterations = None;
    for field in text.split(',') {
        if let Some(v) = field.strip_prefix("r=") {
            nonce = Some(v.to_string());
        } else if let Some(v) = field.strip_prefix("s=") {
            salt = Some(base64_std::decode(v).expect("valid salt b64"));
        } else if let Some(v) = field.strip_prefix("i=") {
            iterations = Some(v.parse().expect("valid iteration count"));
        }
    }
    ServerFirst {
        server_first_body: body,
        nonce: nonce.expect("server-first must carry r="),
        salt: salt.expect("server-first must carry s="),
        iterations: iterations.expect("server-first must carry i="),
    }
}

/// RFC 5802 のクライアント側計算（独立実装。`wire_scram_auth.rs` と同型）。
fn compute_client_proof(
    password: &[u8],
    salt: &[u8],
    iterations: u32,
    auth_message: &[u8],
) -> Vec<u8> {
    let salted_password = pbkdf2_hmac_sha256_one_block(password, salt, iterations);
    let client_key = hmac_sha256(&salted_password, b"Client Key");
    let stored_key = engine::crypto::sha256::digest(&client_key);
    let client_signature = hmac_sha256(&stored_key, auth_message);
    client_key
        .iter()
        .zip(client_signature.iter())
        .map(|(a, b)| a ^ b)
        .collect()
}

/// client-final-message-without-proof を独立に組み立てる
/// （`c=base64(gs2_header ++ cbind_data)`。空 `cbind_data` は非 PLUS 相当）。
fn client_final_without_proof(gs2_header: &[u8], cbind_data: &[u8], nonce: &str) -> Vec<u8> {
    let mut cbind_input = gs2_header.to_vec();
    cbind_input.extend_from_slice(cbind_data);
    format!("c={},r={nonce}", base64_std::encode(&cbind_input)).into_bytes()
}

fn compute_auth_message(
    client_first_bare: &[u8],
    server_first: &[u8],
    client_final_no_proof: &[u8],
) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(client_first_bare);
    m.push(b',');
    m.extend_from_slice(server_first);
    m.push(b',');
    m.extend_from_slice(client_final_no_proof);
    m
}

fn extract_sqlstate(body: &[u8]) -> String {
    let mut pos = 0usize;
    while pos < body.len() {
        let code = body[pos];
        if code == 0 {
            break;
        }
        pos += 1;
        let start = pos;
        while pos < body.len() && body[pos] != 0 {
            pos += 1;
        }
        let value = String::from_utf8_lossy(&body[start..pos]).into_owned();
        pos += 1;
        if code == b'C' {
            return value;
        }
    }
    panic!("ErrorResponse body has no 'C' (SQLSTATE) field: {body:?}");
}

fn write_scram_user_store_file(password: &[u8]) -> std::path::PathBuf {
    // `tests/wire_scram_auth.rs::write_scram_user_store_file` と同型
    // （SCRAM 検証子を持つユーザーストアを直接書く、本ファイル専用の実装）。
    let dir = std::env::temp_dir().join(format!(
        "wire-server-wire-scram-plus-tls-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos(),
    ));
    std::fs::create_dir(&dir).expect("create unique fixture dir");
    let path = dir.join("users.txt");
    let scram_salt = [7u8; scram::SALT_LEN];
    let verifier =
        scram::generate_verifier(password, &scram_salt, scram::SCRAM_ITERATIONS).expect("valid");
    let phc = wire_server::auth::argon2id::encode_phc(
        b"unused-in-scram-mode",
        b"0123456789abcdef",
        &wire_server::auth::argon2id::RECOMMENDED_PARAMS,
    )
    .expect("valid phc");
    let content = format!("alice:tenant-a:{phc}:{}\n", verifier.to_verifier_string());
    std::fs::write(&path, &content).expect("write fixture");
    path
}

const TEST_SCRAM_MOCK_KEY_SECRET: &[u8] = b"wire-scram-plus-tls-test-mock-key-secret!!";

/// TLS opt-in サーバー（`store` は SCRAM 必須）を起動する。
/// `advertise_scram_channel_binding` で PLUS 提示可否を切り替える。
fn spawn_tls_scram_server(
    users_path: &std::path::Path,
    advertise_scram_channel_binding: bool,
) -> std::net::SocketAddr {
    let tls = tls_client::test_config_with_scram_channel_binding(advertise_scram_channel_binding);
    spawn_tls_scram_server_with_config(users_path, tls)
}

/// [`spawn_tls_scram_server`] の一般化版（Issue #1088）。任意の
/// `TlsServerConfig` で起動できるようにし、Ed25519 署名以外の葉証明書
/// （ECDSA-SHA256 署名 OID 等）でも同じ SCRAM-PLUS 交換ヘルパー一式を
/// 再利用できるようにする。
fn spawn_tls_scram_server_with_config(
    users_path: &std::path::Path,
    tls: Arc<TlsServerConfig>,
) -> std::net::SocketAddr {
    let store = Arc::new(
        UserStore::load_from_file(users_path)
            .expect("valid user store")
            .require_scram(TEST_SCRAM_MOCK_KEY_SECRET)
            .expect("all records carry scram verifiers"),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_tls(
            listener,
            store,
            None,
            Some(tls),
            limiter,
            Duration::from_secs(5),
        );
    });

    addr
}

/// TLS ハンドシェイクを完了し、`TlsTestChannel` を返す。
fn connect_and_upgrade_to_tls(addr: std::net::SocketAddr) -> tls_client::TlsTestChannel {
    let mut socket = TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut socket);
    let mut resp = [0u8; 1];
    socket.read_exact(&mut resp).expect("read SSL response");
    assert_eq!(&resp, b"S", "TLS-enabled server must accept SSL with 'S'");
    let client = tls_client::drive_client_handshake_over_socket(&mut socket);
    tls_client::TlsTestChannel::new(client, socket)
}

/// 受入基準: TLS 接続では機構リストに PLUS が含まれる（先頭）。
#[test]
fn tls_connection_advertises_plus_mechanism_first() {
    let users_path = write_scram_user_store_file(b"pw-alice");
    let addr = spawn_tls_scram_server(&users_path, true);
    let mut channel = connect_and_upgrade_to_tls(addr);
    write_startup_message(&mut channel, "alice", "irrelevant-db");
    let mechanisms = read_authentication_sasl_mechanisms(&mut channel);
    assert_eq!(
        mechanisms,
        vec![
            scram::MECHANISM_NAME_PLUS.to_string(),
            scram::MECHANISM_NAME.to_string(),
        ]
    );
}

/// 受入基準: 提示無効化設定では TLS 接続でも PLUS を提示しない
/// （機構リストは平文接続とバイト同一）。
#[test]
fn tls_connection_with_channel_binding_disabled_does_not_advertise_plus() {
    let users_path = write_scram_user_store_file(b"pw-alice");
    let addr = spawn_tls_scram_server(&users_path, false);
    let mut channel = connect_and_upgrade_to_tls(addr);
    write_startup_message(&mut channel, "alice", "irrelevant-db");
    let mechanisms = read_authentication_sasl_mechanisms(&mut channel);
    assert_eq!(mechanisms, vec![scram::MECHANISM_NAME.to_string()]);
}

/// 受入基準: PLUS 選択・`p=tls-server-end-point`・独立算出した cbind-data で
/// 認証成功し `ReadyForQuery` へ到達する。
#[test]
fn tls_plus_channel_binding_authenticates_successfully() {
    let password = b"correct horse battery staple";
    let users_path = write_scram_user_store_file(password);
    let addr = spawn_tls_scram_server(&users_path, true);
    let mut channel = connect_and_upgrade_to_tls(addr);
    write_startup_message(&mut channel, "alice", "irrelevant-db");

    let mechanisms = read_authentication_sasl_mechanisms(&mut channel);
    assert!(mechanisms.contains(&scram::MECHANISM_NAME_PLUS.to_string()));

    // クライアント側で独立に tls-server-end-point を算出する（サーバー側の
    // 関数を再利用しない独立経路。`server_leaf_der` は実ハンドシェイクで
    // 受信した葉証明書 DER）。
    let cbind_data =
        wire_server::tls::channel_binding::tls_server_end_point(channel.client_leaf_der())
            .expect("Ed25519 is supported")
            .as_bytes()
            .to_vec();

    let gs2_header: &[u8] = b"p=tls-server-end-point,,";
    let client_nonce = "plus-test-nonce-1";
    let client_first_bare = format!("n=,r={client_nonce}").into_bytes();
    let mut client_first_body = gs2_header.to_vec();
    client_first_body.extend_from_slice(&client_first_bare);
    send_sasl_initial_response(&mut channel, scram::MECHANISM_NAME_PLUS, &client_first_body);

    let server_first = parse_server_first(read_sasl_continue(&mut channel));

    let client_final_no_proof =
        client_final_without_proof(gs2_header, &cbind_data, &server_first.nonce);
    let auth_message = compute_auth_message(
        &client_first_bare,
        &server_first.server_first_body,
        &client_final_no_proof,
    );
    let proof = compute_client_proof(
        password,
        &server_first.salt,
        server_first.iterations,
        &auth_message,
    );
    let mut client_final = client_final_no_proof.clone();
    client_final.extend_from_slice(b",p=");
    client_final.extend_from_slice(base64_std::encode(&proof).as_bytes());
    send_sasl_response(&mut channel, &client_final);

    let (ty, body) = read_message(&mut channel);
    assert_eq!(ty, b'R', "expected AuthenticationSASLFinal");
    let mut code_buf = [0u8; 4];
    code_buf.copy_from_slice(&body[..4]);
    assert_eq!(i32::from_be_bytes(code_buf), 12);

    let (ty, body) = read_message(&mut channel);
    assert_eq!(ty, b'R', "expected AuthenticationOk");
    let mut code_buf = [0u8; 4];
    code_buf.copy_from_slice(&body[..4]);
    assert_eq!(i32::from_be_bytes(code_buf), 0);

    let mut safety = 0;
    loop {
        let (ty, _) = read_message(&mut channel);
        if ty == b'Z' {
            break;
        }
        safety += 1;
        assert!(safety < 20, "too many messages before ReadyForQuery");
    }
}

/// 受入基準（Issue #1088 codex-review P2 指摘への対応）: 署名アルゴリズム
/// OID が `ecdsa-with-SHA256`（鍵種別は SPKI 上 Ed25519 のまま）の葉証明書
/// でも `--tls-scram-channel-binding enable` の accept 側が実クライアント
/// 接続で機能し、PLUS 交換が最後まで成功して `ReadyForQuery` へ到達する
/// こと。`tls_channel_binding.rs::ecdsa_signed_ed25519_key_leaf_is_rfc5929_
/// defined_and_enable_is_accepted` は判定関数の単体確認に留まるため、本
/// テストは実際にサーバーへ証明書を載せ、クライアント接続・SCRAM-PLUS
/// 交換の最後まで駆動する。証明書は
/// [`tls_client::ca_signed_ecdsa_sha256_leaf_certificate_der`] が返す
/// 実際に CA が署名した DER（README/ADR が案内する「RSA/ECDSA CA 署名済み
/// 葉証明書」を代表する fixture。codex-review 再指摘で全 0 埋め
/// `signatureValue` から差し替えた）を使う。`signatureValue` 自体の
/// 暗号学的検証は `x509.rs` モジュール doc が明記するとおり本実装の
/// スコープ外（発行者鍵の検証はクライアントの責務）で、実 CA 署名か
/// どうかでサーバー側の受理判定は変わらないが、本テストは判定を
/// 変えるためではなく、現実の CA 署名済み証明書が持つ形状（実サイズの
/// ECDSA 署名・拡張・非空 issuer/subject）でもパーサと PLUS 交換一式が
/// 通ることを検証する。
#[test]
fn tls_plus_channel_binding_authenticates_successfully_with_ecdsa_signed_leaf() {
    let password = b"correct horse battery staple";
    let users_path = write_scram_user_store_file(password);

    let der = tls_client::ca_signed_ecdsa_sha256_leaf_certificate_der();
    let chain = ServerCertificateChain::from_der_chain(
        vec![der],
        &tls_client::RFC8032_TEST1_PUBLIC_KEY,
        1_600_000_000,
    )
    .expect("valid synthetic chain");
    let key = SigningKey::from_seed_bytes(tls_client::hex_decode32(tls_client::RFC8032_TEST1_SEED));
    let tls = Arc::new(
        TlsServerConfig::new(chain, key)
            .expect("matching leaf/key")
            .with_scram_channel_binding(true),
    );

    let addr = spawn_tls_scram_server_with_config(&users_path, tls);
    let mut channel = connect_and_upgrade_to_tls(addr);
    write_startup_message(&mut channel, "alice", "irrelevant-db");

    let mechanisms = read_authentication_sasl_mechanisms(&mut channel);
    assert!(mechanisms.contains(&scram::MECHANISM_NAME_PLUS.to_string()));

    let cbind_data =
        wire_server::tls::channel_binding::tls_server_end_point(channel.client_leaf_der())
            .expect("ecdsa-with-SHA256 signature OID is RFC 5929 defined")
            .as_bytes()
            .to_vec();

    let gs2_header: &[u8] = b"p=tls-server-end-point,,";
    let client_nonce = "plus-test-nonce-ecdsa-leaf";
    let client_first_bare = format!("n=,r={client_nonce}").into_bytes();
    let mut client_first_body = gs2_header.to_vec();
    client_first_body.extend_from_slice(&client_first_bare);
    send_sasl_initial_response(&mut channel, scram::MECHANISM_NAME_PLUS, &client_first_body);

    let server_first = parse_server_first(read_sasl_continue(&mut channel));

    let client_final_no_proof =
        client_final_without_proof(gs2_header, &cbind_data, &server_first.nonce);
    let auth_message = compute_auth_message(
        &client_first_bare,
        &server_first.server_first_body,
        &client_final_no_proof,
    );
    let proof = compute_client_proof(
        password,
        &server_first.salt,
        server_first.iterations,
        &auth_message,
    );
    let mut client_final = client_final_no_proof.clone();
    client_final.extend_from_slice(b",p=");
    client_final.extend_from_slice(base64_std::encode(&proof).as_bytes());
    send_sasl_response(&mut channel, &client_final);

    let (ty, body) = read_message(&mut channel);
    assert_eq!(ty, b'R', "expected AuthenticationSASLFinal");
    let mut code_buf = [0u8; 4];
    code_buf.copy_from_slice(&body[..4]);
    assert_eq!(i32::from_be_bytes(code_buf), 12);

    let (ty, body) = read_message(&mut channel);
    assert_eq!(ty, b'R', "expected AuthenticationOk");
    let mut code_buf = [0u8; 4];
    code_buf.copy_from_slice(&body[..4]);
    assert_eq!(i32::from_be_bytes(code_buf), 0);

    let mut safety = 0;
    loop {
        let (ty, _) = read_message(&mut channel);
        if ty == b'Z' {
            break;
        }
        safety += 1;
        assert!(safety < 20, "too many messages before ReadyForQuery");
    }
}

/// 受入基準: `c=` の cbind-data を改ざんすると proof 誤りと同一の
/// 28P01・固定遅延経路へ収束する（他テナント・存在情報を漏らさない）。
#[test]
fn tls_plus_channel_binding_tampered_cbind_data_is_rejected_as_auth_invalid() {
    let password = b"correct horse battery staple";
    let users_path = write_scram_user_store_file(password);
    let addr = spawn_tls_scram_server(&users_path, true);
    let mut channel = connect_and_upgrade_to_tls(addr);
    write_startup_message(&mut channel, "alice", "irrelevant-db");
    let _ = read_authentication_sasl_mechanisms(&mut channel);

    let gs2_header: &[u8] = b"p=tls-server-end-point,,";
    let client_nonce = "plus-test-nonce-tamper";
    let client_first_bare = format!("n=,r={client_nonce}").into_bytes();
    let mut client_first_body = gs2_header.to_vec();
    client_first_body.extend_from_slice(&client_first_bare);
    send_sasl_initial_response(&mut channel, scram::MECHANISM_NAME_PLUS, &client_first_body);
    let server_first = parse_server_first(read_sasl_continue(&mut channel));

    // 実際の cbind-data とは異なる値（改ざん）で `c=` を組み立てる。
    let tampered_cbind_data = [0xffu8; 32];
    let client_final_no_proof =
        client_final_without_proof(gs2_header, &tampered_cbind_data, &server_first.nonce);
    let auth_message = compute_auth_message(
        &client_first_bare,
        &server_first.server_first_body,
        &client_final_no_proof,
    );
    let proof = compute_client_proof(
        password,
        &server_first.salt,
        server_first.iterations,
        &auth_message,
    );
    let mut client_final = client_final_no_proof.clone();
    client_final.extend_from_slice(b",p=");
    client_final.extend_from_slice(base64_std::encode(&proof).as_bytes());

    let verify_start = std::time::Instant::now();
    send_sasl_response(&mut channel, &client_final);
    let (ty, body) = read_message(&mut channel);
    let elapsed = verify_start.elapsed();
    assert_eq!(ty, b'E', "expected ErrorResponse");
    assert_eq!(extract_sqlstate(&body), "28P01");
    assert!(
        elapsed >= wire_server::auth::AUTH_FAILURE_DELAY,
        "tampered channel binding must incur the same fixed delay as a wrong proof"
    );
}

/// 受入基準: PLUS 提示時に非 PLUS 機構＋`y`（ダウングレード）は拒否する。
#[test]
fn tls_downgrade_attempt_with_y_flag_is_rejected_with_08p01() {
    let users_path = write_scram_user_store_file(b"pw-alice");
    let addr = spawn_tls_scram_server(&users_path, true);
    let mut channel = connect_and_upgrade_to_tls(addr);
    write_startup_message(&mut channel, "alice", "irrelevant-db");
    let _ = read_authentication_sasl_mechanisms(&mut channel);

    let client_first_body = b"y,,n=,r=downgrade-test-nonce".to_vec();
    send_sasl_initial_response(&mut channel, scram::MECHANISM_NAME, &client_first_body);

    let (ty, body) = read_message(&mut channel);
    assert_eq!(ty, b'E', "expected ErrorResponse");
    assert_eq!(extract_sqlstate(&body), "08P01");
}

/// 受入基準: PLUS 提示時に非 PLUS 機構＋`n` は受理する。
#[test]
fn tls_non_plus_selection_with_n_flag_still_succeeds() {
    let password = b"correct horse battery staple";
    let users_path = write_scram_user_store_file(password);
    let addr = spawn_tls_scram_server(&users_path, true);
    let mut channel = connect_and_upgrade_to_tls(addr);
    write_startup_message(&mut channel, "alice", "irrelevant-db");
    let _ = read_authentication_sasl_mechanisms(&mut channel);

    let client_nonce = "non-plus-over-tls-nonce";
    let gs2_header: &[u8] = b"n,,";
    let client_first_bare = format!("n=,r={client_nonce}").into_bytes();
    let mut client_first_body = gs2_header.to_vec();
    client_first_body.extend_from_slice(&client_first_bare);
    send_sasl_initial_response(&mut channel, scram::MECHANISM_NAME, &client_first_body);
    let server_first = parse_server_first(read_sasl_continue(&mut channel));

    let client_final_no_proof = client_final_without_proof(gs2_header, &[], &server_first.nonce);
    let auth_message = compute_auth_message(
        &client_first_bare,
        &server_first.server_first_body,
        &client_final_no_proof,
    );
    let proof = compute_client_proof(
        password,
        &server_first.salt,
        server_first.iterations,
        &auth_message,
    );
    let mut client_final = client_final_no_proof.clone();
    client_final.extend_from_slice(b",p=");
    client_final.extend_from_slice(base64_std::encode(&proof).as_bytes());
    send_sasl_response(&mut channel, &client_final);

    let (ty, body) = read_message(&mut channel);
    assert_eq!(ty, b'R', "expected AuthenticationSASLFinal");
    let mut code_buf = [0u8; 4];
    code_buf.copy_from_slice(&body[..4]);
    assert_eq!(i32::from_be_bytes(code_buf), 12);
}

/// 受入基準: 未知の cb-name（例 `tls-unique`）は 08P01。
#[test]
fn tls_unknown_cb_name_is_rejected_with_08p01() {
    let users_path = write_scram_user_store_file(b"pw-alice");
    let addr = spawn_tls_scram_server(&users_path, true);
    let mut channel = connect_and_upgrade_to_tls(addr);
    write_startup_message(&mut channel, "alice", "irrelevant-db");
    let _ = read_authentication_sasl_mechanisms(&mut channel);

    let client_first_body = b"p=tls-unique,,n=,r=unknown-cb-name-nonce".to_vec();
    send_sasl_initial_response(&mut channel, scram::MECHANISM_NAME_PLUS, &client_first_body);

    let (ty, body) = read_message(&mut channel);
    assert_eq!(ty, b'E', "expected ErrorResponse");
    assert_eq!(extract_sqlstate(&body), "08P01");
}

/// 受入基準: 未知ユーザーでも PLUS 経路で既知ユーザーの誤 proof と同一の
/// 応答・遅延になる（列挙攻撃対策の既存性質を PLUS 経路でも維持）。
#[test]
fn tls_plus_unknown_user_error_is_byte_identical_to_wrong_proof() {
    let password = b"correct horse battery staple";
    let users_path = write_scram_user_store_file(password);
    let addr_known = spawn_tls_scram_server(&users_path, true);
    let addr_unknown = spawn_tls_scram_server(&users_path, true);

    let run = |addr: std::net::SocketAddr, username: &str, use_correct_password: bool| -> Vec<u8> {
        let mut channel = connect_and_upgrade_to_tls(addr);
        write_startup_message(&mut channel, username, "irrelevant-db");
        let _ = read_authentication_sasl_mechanisms(&mut channel);

        let cbind_data =
            wire_server::tls::channel_binding::tls_server_end_point(channel.client_leaf_der())
                .expect("Ed25519 is supported")
                .as_bytes()
                .to_vec();
        let gs2_header: &[u8] = b"p=tls-server-end-point,,";
        let client_nonce = "enum-test-nonce";
        let client_first_bare = format!("n=,r={client_nonce}").into_bytes();
        let mut client_first_body = gs2_header.to_vec();
        client_first_body.extend_from_slice(&client_first_bare);
        send_sasl_initial_response(&mut channel, scram::MECHANISM_NAME_PLUS, &client_first_body);
        let server_first = parse_server_first(read_sasl_continue(&mut channel));

        let client_final_no_proof =
            client_final_without_proof(gs2_header, &cbind_data, &server_first.nonce);
        let auth_message = compute_auth_message(
            &client_first_bare,
            &server_first.server_first_body,
            &client_final_no_proof,
        );
        let used_password: &[u8] = if use_correct_password {
            password
        } else {
            b"wrong-password"
        };
        let proof = compute_client_proof(
            used_password,
            &server_first.salt,
            server_first.iterations,
            &auth_message,
        );
        let mut client_final = client_final_no_proof.clone();
        client_final.extend_from_slice(b",p=");
        client_final.extend_from_slice(base64_std::encode(&proof).as_bytes());
        send_sasl_response(&mut channel, &client_final);

        let (ty, body) = read_message(&mut channel);
        assert_eq!(ty, b'E');
        body
    };

    let known_wrong_proof = run(addr_known, "alice", false);
    let unknown_user = run(addr_unknown, "someone-else", true);
    assert_eq!(known_wrong_proof, unknown_user);
}
