//! SCRAM-SHA-256（RFC 5802・RFC 7677）の SASL 往復を、`handle_connection_bounded`
//! （`--auth-method scram-sha-256`）に対する生バイトクライアントで検証する
//! 結合テスト（Issue #940・WIRE-18・TASK-222）。
//!
//! `wire_auth.rs`（cleartext・WIRE-2/WIRE-3）と同じ「ephemeral port へサーバー
//! スレッドを起動し、`std::net::TcpStream` で生バイトを送受信する自作クライアント
//! を用いる」方針を踏襲する。SCRAM のクライアント側計算（SaltedPassword→ClientKey
//! →ClientSignature→ClientProof）はサーバー側 `auth::scram` モジュールが公開する
//! 素材関数（`hmac_sha256`・`generate_verifier` 等は crate 内部限定のため、本ファイル
//! では `wire_server::auth::scram`／`base64_std` の公開 API のみで独立に計算する）。

#[path = "common/fixture_guard.rs"]
mod fixture_guard;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use wire_server::auth::hmac_sha256::{hmac_sha256, pbkdf2_hmac_sha256_one_block};
use wire_server::auth::{argon2id, base64_std, scram, UserStore};

const TEST_PARAMS: argon2id::Params = argon2id::RECOMMENDED_PARAMS;

/// テスト専用のモック鍵秘密（Issue #940 PR #1006・P0 是正）。実運用の
/// `--scram-mock-key-file` に相当し、ユーザーストアの内容から独立している
/// ことのみが契約なので値そのものに意味はない（32 バイト以上）。
const TEST_SCRAM_MOCK_KEY_SECRET: &[u8] = b"wire-scram-auth-test-mock-key-secret!!";

static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// `wire_auth.rs::write_user_store_file` と同じ設計（ユニークな一時ディレクトリ・
/// 書き込み直後の読み戻し検証）に、4 番目のフィールド（SCRAM 検証子）を追加した版。
/// `password` が `None` の行は SCRAM 検証子を持たない（cleartext 専用・混在確認用）。
fn write_scram_user_store_file(
    records: &[(&str, &str, Option<&[u8]>)],
) -> fixture_guard::UserStoreFile {
    let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "wire-server-wire-scram-test-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock")
            .as_nanos(),
        seq
    ));
    std::fs::create_dir(&dir).expect("create unique fixture dir");
    let path = dir.join("users.txt");

    let mut content = String::new();
    for (username, tenant_id, password) in records {
        let salt = b"0123456789abcdef";
        let phc = argon2id::encode_phc(b"unused-in-scram-mode", salt, &TEST_PARAMS)
            .expect("valid phc encoding");
        match password {
            Some(pw) => {
                let scram_salt = [7u8; scram::SALT_LEN];
                let verifier = scram::generate_verifier(pw, &scram_salt, scram::SCRAM_ITERATIONS)
                    .expect("valid scram verifier");
                content.push_str(&format!(
                    "{username}:{tenant_id}:{phc}:{}\n",
                    verifier.to_verifier_string()
                ));
            }
            None => content.push_str(&format!("{username}:{tenant_id}:{phc}\n")),
        }
    }
    std::fs::write(&path, &content).expect("write user store fixture");
    let readback = std::fs::read_to_string(&path).expect("read back user store fixture");
    assert_eq!(
        readback, content,
        "fixture file content must match what was just written (possible fixture race)"
    );
    fixture_guard::UserStoreFile::new(dir, path)
}

/// `--auth-method scram-sha-256` 相当（`UserStore::require_scram`）でサーバー
/// スレッドを起動し、1 接続だけ受理する。
fn spawn_scram_server_accepting_one(users_path: &std::path::Path) -> std::net::SocketAddr {
    let store = UserStore::load_from_file(users_path)
        .expect("valid user store")
        .require_scram(TEST_SCRAM_MOCK_KEY_SECRET)
        .expect("all records must carry scram verifiers");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    std::thread::spawn(move || {
        if let Ok((stream, _)) = listener.accept() {
            let _ = wire_server::handshake::handle_connection_bounded(stream, &store);
        }
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
    assert_eq!(&resp, b"N", "server must decline SSL (TLS not yet wired)");

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

/// `AuthenticationSASL`（'R'/10）を読み、提示された機構名一覧（C 文字列の並び）を
/// 返す。
fn read_authentication_sasl_mechanisms(stream: &mut TcpStream) -> Vec<String> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    assert_eq!(header[0], b'R', "expected Authentication* message");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
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

/// `SASLInitialResponse`（型 'p'）を送る: 機構名 C 文字列＋i32 長＋client-first 本体。
fn send_sasl_initial_response(stream: &mut TcpStream, mechanism: &str, client_first: &[u8]) {
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

/// `SASLResponse`（型 'p'）を送る: raw bytes（NUL 終端なし）。
fn send_sasl_response(stream: &mut TcpStream, body: &[u8]) {
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::new();
    msg.push(b'p');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(body);
    stream.write_all(&msg).expect("send SASLResponse");
}

/// 次の 1 メッセージを丸ごと読む（型バイト・本文）。呼び出し元が型で分岐する。
fn read_message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    (header[0], body)
}

/// `AuthenticationSASLContinue`（'R'/11）本文（server-first-message）を読む。
fn read_sasl_continue(stream: &mut TcpStream) -> Vec<u8> {
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

/// server-first-message（`r=<nonce>,s=<salt_b64>,i=<iter>`）を素朴に分解する。
struct ServerFirst {
    nonce: String,
    salt: Vec<u8>,
    iterations: u32,
}

fn parse_server_first(body: &[u8]) -> ServerFirst {
    let text = std::str::from_utf8(body).expect("server-first must be utf8");
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
        nonce: nonce.expect("server-first must carry r="),
        salt: salt.expect("server-first must carry s="),
        iterations: iterations.expect("server-first must carry i="),
    }
}

/// RFC 5802 のクライアント側計算（SaltedPassword→ClientKey→ClientSignature→
/// ClientProof）。`hmac_sha256`／`pbkdf2_hmac_sha256_one_block`（いずれも
/// `wire_server::auth::hmac_sha256` の公開 API）と `engine::crypto::sha256::digest`
/// のみを使い、サーバー側 `scram::verify_client_final` の実装を経由せずに
/// 独立に proof を計算する（サーバー・クライアント双方が同じ実装バグを共有して
/// 見逃す事態を避けるため）。RFC 5802 の定義:
/// `SaltedPassword = Hi(password, salt, iterations)`、
/// `ClientKey = HMAC(SaltedPassword, "Client Key")`、
/// `StoredKey = H(ClientKey)`、
/// `ClientSignature = HMAC(StoredKey, AuthMessage)`、
/// `ClientProof = ClientKey XOR ClientSignature`。
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

/// client-final-message-without-proof（`c=<gs2-header-b64>,r=<nonce>`）を
/// クライアント側で組み立てる（サーバー実装 `scram::client_final_without_proof`
/// と同一のフォーマットだが、独立実装として本ファイル内で構築する）。
fn client_final_without_proof(gs2_header: &[u8], nonce: &str) -> Vec<u8> {
    format!("c={},r={nonce}", base64_std::encode(gs2_header)).into_bytes()
}

/// AuthMessage = client-first-bare + "," + server-first + "," +
/// client-final-without-proof（RFC 5802）。
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

/// ErrorResponse（'E'）本文から SQLSTATE（`C` フィールド値）を取り出す。
fn extract_sqlstate(body: &[u8]) -> String {
    // フィールドは `<code byte><NUL 終端文字列>` の並びで、末尾に空文字列
    // （最終 NUL のみ）で終端する。`C` フィールドの値を取り出す。
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
        pos += 1; // NUL 終端をスキップ
        if code == b'C' {
            return value;
        }
    }
    panic!("ErrorResponse body has no 'C' (SQLSTATE) field: {body:?}");
}

/// SASL 往復のクライアント状態（成功系・失敗系のテストで共有する下ごしらえ）。
struct ScramClientFirst {
    gs2_header: &'static [u8],
    client_first_bare: Vec<u8>,
    body: Vec<u8>,
}

/// gs2-flag（`"n"` または `"y"`）で client-first-message を組み立てる。
/// authzid は空（`a=` を付けない）。
fn build_client_first(gs2_flag: &'static str, client_nonce: &str) -> ScramClientFirst {
    let client_first_bare = format!("n=,r={client_nonce}").into_bytes();
    let gs2_header: &'static [u8] = match gs2_flag {
        "n" => b"n,,",
        "y" => b"y,,",
        _ => unreachable!("test only uses n/y flags"),
    };
    let mut body = Vec::new();
    body.extend_from_slice(gs2_header);
    body.extend_from_slice(&client_first_bare);
    ScramClientFirst {
        gs2_header,
        client_first_bare,
        body,
    }
}

/// 成功系・失敗系で共通の「client-first を送り server-first を受け取る」段。
fn negotiate_sasl_first(
    stream: &mut TcpStream,
    client_nonce: &str,
) -> (ScramClientFirst, ServerFirst) {
    let mechanisms = read_authentication_sasl_mechanisms(stream);
    assert_eq!(
        mechanisms,
        vec![scram::MECHANISM_NAME.to_string()],
        "server must offer exactly SCRAM-SHA-256 (no -PLUS; TLS not yet wired)"
    );
    let client_first = build_client_first("n", client_nonce);
    send_sasl_initial_response(stream, scram::MECHANISM_NAME, &client_first.body);
    let server_first_body = read_sasl_continue(stream);
    let server_first = parse_server_first(&server_first_body);
    (client_first, server_first)
}

/// 正常系: 正しいパスワードで最後まで往復し `ReadyForQuery` へ到達すること。
#[test]
fn wire_scram_successful_auth_reaches_ready_for_query() {
    let password = b"correct horse battery staple";
    let users_path =
        write_scram_user_store_file(&[("alice", "tenant-a", Some(password.as_slice()))]);
    let addr = spawn_scram_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "alice", "irrelevant-db");
    let (client_first, server_first) = negotiate_sasl_first(&mut stream, "test-client-nonce-1");

    let client_final_no_proof =
        client_final_without_proof(client_first.gs2_header, &server_first.nonce);
    let auth_message = compute_auth_message(
        &client_first.client_first_bare,
        // server-first-message は素の body（コード無し部分）そのもの。
        format!(
            "r={},s={},i={}",
            server_first.nonce,
            base64_std::encode(&server_first.salt),
            server_first.iterations
        )
        .as_bytes(),
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
    send_sasl_response(&mut stream, &client_final);

    let (ty, body) = read_message(&mut stream);
    assert_eq!(
        ty, b'R',
        "expected AuthenticationSASLFinal, got {ty} body={body:?}"
    );
    let mut code_buf = [0u8; 4];
    code_buf.copy_from_slice(&body[..4]);
    assert_eq!(
        i32::from_be_bytes(code_buf),
        12,
        "AuthenticationSASLFinal code must be 12"
    );
    assert!(
        std::str::from_utf8(&body[4..])
            .expect("utf8")
            .starts_with("v="),
        "server-final must carry v=<ServerSignature>"
    );

    // AuthenticationOk('R'/0)
    let (ty, body) = read_message(&mut stream);
    assert_eq!(ty, b'R');
    let mut code_buf = [0u8; 4];
    code_buf.copy_from_slice(&body[..4]);
    assert_eq!(
        i32::from_be_bytes(code_buf),
        0,
        "AuthenticationOk code must be 0"
    );

    // BackendKeyData・ParameterStatus* を読み飛ばして ReadyForQuery('Z') へ。
    let mut safety = 0;
    loop {
        let (ty, _) = read_message(&mut stream);
        if ty == b'Z' {
            break;
        }
        safety += 1;
        assert!(safety < 20, "too many messages before ReadyForQuery");
    }
}

/// 誤 proof: `28P01`（AuthInvalid）を固定遅延後に返し、`ReadyForQuery` へ到達
/// しないこと。
#[test]
fn wire_scram_wrong_proof_returns_28p01_after_delay() {
    let password = b"correct horse battery staple";
    let users_path =
        write_scram_user_store_file(&[("alice", "tenant-a", Some(password.as_slice()))]);
    let addr = spawn_scram_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "alice", "db");
    let (client_first, server_first) = negotiate_sasl_first(&mut stream, "nonce-wrong-proof");

    let client_final_no_proof =
        client_final_without_proof(client_first.gs2_header, &server_first.nonce);
    // わざと誤った proof（全てゼロの 32 バイト）を送る。
    let bogus_proof = [0u8; 32];
    let mut client_final = client_final_no_proof.clone();
    client_final.extend_from_slice(b",p=");
    client_final.extend_from_slice(base64_std::encode(&bogus_proof).as_bytes());

    let start = Instant::now();
    send_sasl_response(&mut stream, &client_final);
    let (ty, body) = read_message(&mut stream);
    let elapsed = start.elapsed();

    assert_eq!(ty, b'E', "expected ErrorResponse on wrong proof");
    assert_eq!(extract_sqlstate(&body), "28P01");
    assert!(
        elapsed >= Duration::from_millis(200),
        "auth failure must incur the fixed delay"
    );

    let mut extra = [0u8; 1];
    let n = stream.read(&mut extra).unwrap_or(0);
    assert_eq!(n, 0, "connection must be closed after auth failure");
}

/// 誤 nonce: client-final の `r=` が combined nonce と一致しない場合も
/// 誤 proof と同じ `28P01`・同じ固定遅延で失敗すること（RFC 5802 の
/// nonce 検証は proof 検証と同じ失敗経路へ収束する契約。`handshake.rs::
/// authenticate_scram` の `nonce_and_binding_match` 参照）。
#[test]
fn wire_scram_wrong_nonce_returns_28p01_after_delay() {
    let password = b"correct horse battery staple";
    let users_path =
        write_scram_user_store_file(&[("alice", "tenant-a", Some(password.as_slice()))]);
    let addr = spawn_scram_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "alice", "db");
    let (client_first, server_first) = negotiate_sasl_first(&mut stream, "nonce-wrong-nonce");

    // 正しい nonce ではなく別の文字列を使って client-final を組み立てる
    // （proof 自体は正しい nonce で計算した結果を流用しても、サーバー側の
    // combined nonce 照合で必ず不一致になる）。
    let tampered_nonce = format!("{}-tampered", server_first.nonce);
    let client_final_no_proof =
        client_final_without_proof(client_first.gs2_header, &tampered_nonce);
    let auth_message = compute_auth_message(
        &client_first.client_first_bare,
        format!(
            "r={},s={},i={}",
            server_first.nonce,
            base64_std::encode(&server_first.salt),
            server_first.iterations
        )
        .as_bytes(),
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

    let start = Instant::now();
    send_sasl_response(&mut stream, &client_final);
    let (ty, body) = read_message(&mut stream);
    let elapsed = start.elapsed();

    assert_eq!(ty, b'E', "expected ErrorResponse on nonce mismatch");
    assert_eq!(extract_sqlstate(&body), "28P01");
    assert!(elapsed >= Duration::from_millis(200));
}

/// 非対応機構名: `SASLInitialResponse` に `SCRAM-SHA-256` 以外の機構名を
/// 名乗った場合は `08P01`（ProtocolViolation）で即座に拒否されること
/// （固定遅延は課さない構文違反系。`handshake.rs::read_sasl_initial_response`
/// 参照）。
#[test]
fn wire_scram_unsupported_mechanism_returns_08p01() {
    let password = b"correct horse battery staple";
    let users_path =
        write_scram_user_store_file(&[("alice", "tenant-a", Some(password.as_slice()))]);
    let addr = spawn_scram_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "alice", "db");
    let _ = read_authentication_sasl_mechanisms(&mut stream);

    let client_first = build_client_first("n", "nonce-unsupported-mechanism");
    send_sasl_initial_response(&mut stream, "SCRAM-SHA-1", &client_first.body);

    let (ty, body) = read_message(&mut stream);
    assert_eq!(ty, b'E', "expected ErrorResponse on unsupported mechanism");
    assert_eq!(extract_sqlstate(&body), "08P01");
}

/// チャネルバインディング要求（`p=<cb-name>`）: TLS 未実装のため `08P01` で
/// 拒否されること（Issue #941・TASK-228 へ引き継ぐ制約）。
#[test]
fn wire_scram_channel_binding_request_returns_08p01() {
    let password = b"correct horse battery staple";
    let users_path =
        write_scram_user_store_file(&[("alice", "tenant-a", Some(password.as_slice()))]);
    let addr = spawn_scram_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "alice", "db");
    let _ = read_authentication_sasl_mechanisms(&mut stream);

    let body = b"p=tls-server-end-point,,n=,r=nonce-cbind".to_vec();
    send_sasl_initial_response(&mut stream, scram::MECHANISM_NAME, &body);

    let (ty, body) = read_message(&mut stream);
    assert_eq!(
        ty, b'E',
        "expected ErrorResponse on channel binding request"
    );
    assert_eq!(extract_sqlstate(&body), "08P01");
}

/// 上限超過の SASL メッセージ（`MAX_SASL_MESSAGE_LEN`=2048 超・かつ
/// `framing::MAX_MESSAGE_LEN`=1MiB 未満）: `framing::read_length_prefixed_body`
/// は型固有の上限超過を `FrameError::Malformed`（`08P01`）として分類し、
/// `FrameError::TooLarge`（`54000`）は `framing::MAX_MESSAGE_LEN` 自体を
/// 超えた場合にのみ生じる（`framing.rs::read_length_prefixed_body` 参照）。
#[test]
fn wire_scram_oversized_sasl_message_returns_08p01() {
    let password = b"correct horse battery staple";
    let users_path =
        write_scram_user_store_file(&[("alice", "tenant-a", Some(password.as_slice()))]);
    let addr = spawn_scram_server_accepting_one(&users_path);
    let mut stream = TcpStream::connect(addr).expect("connect");

    send_ssl_request_and_startup(&mut stream, "alice", "db");
    let _ = read_authentication_sasl_mechanisms(&mut stream);

    // 機構名 + 巨大な client-first 本体（2048 バイト上限を超える）。
    let oversized_nonce = "x".repeat(4096);
    let client_first_bare = format!("n=,r={oversized_nonce}").into_bytes();
    let mut body = Vec::new();
    body.extend_from_slice(b"n,,");
    body.extend_from_slice(&client_first_bare);
    send_sasl_initial_response(&mut stream, scram::MECHANISM_NAME, &body);

    let (ty, body) = read_message(&mut stream);
    assert_eq!(ty, b'E', "expected ErrorResponse on oversized SASL message");
    assert_eq!(extract_sqlstate(&body), "08P01");
}

/// 列挙耐性: 未知ユーザーへの任意 proof と、既知ユーザーへの誤 proof が
/// バイト完全一致の `ErrorResponse` を返すこと（`AuthFailure::MESSAGE` 固定
/// 文言・同一 SQLSTATE の対称性。RLS-9 と同じ設計様式の SCRAM 版）。
#[test]
fn wire_scram_unknown_user_error_is_byte_identical_to_wrong_proof() {
    let password = b"correct horse battery staple";
    let users_path =
        write_scram_user_store_file(&[("alice", "tenant-a", Some(password.as_slice()))]);

    // 既知ユーザー・誤 proof の応答バイト列を採取する。
    let known_wrong_proof_bytes = {
        let addr = spawn_scram_server_accepting_one(&users_path);
        let mut stream = TcpStream::connect(addr).expect("connect");
        send_ssl_request_and_startup(&mut stream, "alice", "db");
        let (client_first, server_first) = negotiate_sasl_first(&mut stream, "nonce-enum-known");
        let client_final_no_proof =
            client_final_without_proof(client_first.gs2_header, &server_first.nonce);
        let mut client_final = client_final_no_proof;
        client_final.extend_from_slice(b",p=");
        client_final.extend_from_slice(base64_std::encode(&[0u8; 32]).as_bytes());
        send_sasl_response(&mut stream, &client_final);
        let (ty, body) = read_message(&mut stream);
        assert_eq!(ty, b'E');
        body
    };

    // 未知ユーザー・任意 proof の応答バイト列を採取する。
    let unknown_user_bytes = {
        let addr = spawn_scram_server_accepting_one(&users_path);
        let mut stream = TcpStream::connect(addr).expect("connect");
        send_ssl_request_and_startup(&mut stream, "no-such-user", "db");
        let (client_first, server_first) = negotiate_sasl_first(&mut stream, "nonce-enum-unknown");
        let client_final_no_proof =
            client_final_without_proof(client_first.gs2_header, &server_first.nonce);
        let mut client_final = client_final_no_proof;
        client_final.extend_from_slice(b",p=");
        client_final.extend_from_slice(base64_std::encode(&[0u8; 32]).as_bytes());
        send_sasl_response(&mut stream, &client_final);
        let (ty, body) = read_message(&mut stream);
        assert_eq!(ty, b'E');
        body
    };

    assert_eq!(
        known_wrong_proof_bytes, unknown_user_bytes,
        "unknown-user and known-user-wrong-proof ErrorResponse bodies must be byte-identical \
         (enumeration resistance)"
    );
}

/// 列挙耐性（salt の決定性）: 同じ未知ユーザー名に対する server-first の
/// salt／iterations は、2 回の別接続でも変わらないこと（`mock_verifier` が
/// ユーザー名から決定的に導出される契約。ユーザーストア再読み込み無しに
/// salt が接続ごとに変わると、それ自体が「このユーザー名は未登録」という
/// 弁別信号になってしまう）。
#[test]
fn wire_scram_unknown_user_mock_salt_is_stable_across_connections() {
    let password = b"correct horse battery staple";
    let users_path =
        write_scram_user_store_file(&[("alice", "tenant-a", Some(password.as_slice()))]);
    let store = UserStore::load_from_file(&users_path)
        .expect("valid user store")
        .require_scram(TEST_SCRAM_MOCK_KEY_SECRET)
        .expect("all records must carry scram verifiers");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            if let Ok((stream, _)) = listener.accept() {
                let _ = wire_server::handshake::handle_connection_bounded(stream, &store);
            }
        }
    });

    let mut server_firsts = Vec::new();
    for i in 0..2 {
        let mut stream = TcpStream::connect(addr).expect("connect");
        send_ssl_request_and_startup(&mut stream, "no-such-user", "db");
        let (_client_first, server_first) =
            negotiate_sasl_first(&mut stream, &format!("nonce-mock-salt-{i}"));
        server_firsts.push((server_first.salt, server_first.iterations));
        // 未検証のまま接続を切る（proof を送らず単に切断してよい。
        // サーバー側は次の接続の accept へ進む）。
    }

    assert_eq!(
        server_firsts[0], server_firsts[1],
        "mock verifier's salt/iterations must be deterministic per username across connections"
    );

    server.join().expect("server thread must not panic");
}
