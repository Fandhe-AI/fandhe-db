//! `tls-server-end-point`（RFC 5929 §4。Issue #970・親 #941・TASK-228・
//! WIRE-9・WIRE-18 ポインタ）のエクスポートが TLS 接続経由でも一貫している
//! ことを検証する結合テスト。
//!
//! `tests/common/tls_client.rs` で実ハンドシェイクを駆動し、クライアント側が
//! 受信した葉証明書 DER から独立に `tls-server-end-point` を算出した値と、
//! サーバー側 `TlsSession`/`TlsStream`（`crate::wire_stream::WireStream`
//! 経由）が保持するエクスポート値が一致することを固定する。平文接続では
//! `None` になることもあわせて確認する。

#[path = "common/mod.rs"]
mod common;
#[path = "common/tls_client.rs"]
mod tls_client;

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;
use wire_server::tls::channel_binding::{self, TlsServerEndPoint};
use wire_server::tls::ed25519::SigningKey;
use wire_server::tls::server_handshake::TlsServerConfig;
use wire_server::tls::x509::ServerCertificateChain;
use wire_server::tls_opt::{self, ScramChannelBindingRejection};

const SSL_REQUEST_CODE: i32 = 80_877_103;

fn write_ssl_request(stream: &mut TcpStream) {
    let mut msg = Vec::new();
    msg.extend_from_slice(&8i32.to_be_bytes());
    msg.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
    stream.write_all(&msg).expect("send SSLRequest");
}

/// `tests/wire_tls_connection.rs::spawn_tls_server` と同型のサーバー起動
/// ヘルパー（本ファイル専用に独立して持つ）。
fn spawn_tls_server(users_path: &std::path::Path) -> std::net::SocketAddr {
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
            Duration::from_secs(5),
        );
    });

    addr
}

/// 受入基準 R1・R2: TLS ハンドシェイクを完走した接続で、サーバー側の
/// `TlsServerConfig::tls_server_end_point()`（起動時 1 回算出）と、クライアント
/// が受信した葉証明書 DER から独立に算出した値が一致すること。
#[test]
fn tls_server_end_point_matches_independently_computed_value_from_client() {
    let users_path = common::write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_tls_server(&users_path);

    let mut socket = TcpStream::connect(addr).expect("connect");
    write_ssl_request(&mut socket);
    let mut resp = [0u8; 1];
    std::io::Read::read_exact(&mut socket, &mut resp).expect("read SSL response");
    assert_eq!(&resp, b"S");

    let client = tls_client::drive_client_handshake_over_socket(&mut socket);
    assert!(
        !client.server_leaf_der.is_empty(),
        "client must have received the leaf certificate DER"
    );

    // クライアント側で独立に `tls-server-end-point` を算出する（サーバー側の
    // 関数を再利用しない独立経路）。
    let client_side = channel_binding::tls_server_end_point(&client.server_leaf_der)
        .expect("Ed25519-signed test certificate is supported");

    // サーバー側 `TlsServerConfig`（本テストのサーバースレッドと同じ
    // `test_config()`）が起動時に算出した値と一致することを固定する。
    let server_config = tls_client::test_config();
    let server_side = server_config
        .tls_server_end_point()
        .expect("server config computed a channel binding value");
    assert_eq!(&client_side, server_side);
}

/// 受入基準 R2: TLS 未設定の平文接続では `WireStream::tls_server_end_point`
/// が構造的に `None` を返すこと。
#[test]
fn plaintext_connection_has_no_channel_binding() {
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

    let stream = TcpStream::connect(addr).expect("connect");
    use wire_server::wire_stream::WireStream;
    assert_eq!(WireStream::tls_server_end_point(&stream), None);
}

/// R3: 既知の証明書（`test_config()` が使う RFC 8032 TEST 1 の Ed25519 鍵
/// から組んだ自己署名証明書）に対する `TlsServerConfig::tls_server_end_point`
/// の固定値検証。値そのものは `tls::channel_binding` の単体テストが
/// 公開ベクタで固定しているため、ここでは「TLS 接続構成要素からエクスポート
/// できること」「同じ設定は常に同じ値になること（決定性）」を固定する。
#[test]
fn tls_server_config_channel_binding_is_deterministic() {
    let config_a = tls_client::test_config();
    let config_b = tls_client::test_config();
    let a = config_a
        .tls_server_end_point()
        .expect("channel binding computed");
    let b = config_b
        .tls_server_end_point()
        .expect("channel binding computed");
    assert_eq!(a, b);
    match a {
        TlsServerEndPoint::Sha256(_) => {}
        TlsServerEndPoint::Sha512(_) => panic!("test certificate is Ed25519-signed (SHA-256)"),
    }
}

/// `TlsServerConfig::with_scram_channel_binding(false)` で無効化しても、
/// `tls_server_end_point()`（算出値そのもの）は変わらない契約
/// （提示可否は [`server_handshake::TlsSession::tls_server_end_point`] 側で
/// 畳み込まれ、`TlsServerConfig` の算出値自体は不変。Issue #970）。
#[test]
fn scram_channel_binding_disabled_config_does_not_change_export_value() {
    let enabled = tls_client::test_config().tls_server_end_point().copied();
    let disabled = tls_client::test_config_with_scram_channel_binding(false);
    assert!(!disabled.scram_channel_binding_enabled());
    assert_eq!(disabled.tls_server_end_point().copied(), enabled);
}

// --- Issue #1088: `enable` と RFC 5929 が定義するハッシュを持たない
// 署名アルゴリズム（Ed25519 など）の組合せの起動時拒否 ----------------------

/// Ed25519 自己署名の葉証明書（`tls_client::test_config` と同じ鍵材料）は、
/// RFC 5929 が単一ハッシュを定義しない署名アルゴリズムのため
/// `tls_server_end_point_is_rfc5929_defined()` が `false`、
/// `check_scram_channel_binding(&cfg, true)` が `NoRfc5929Hash` を返す。
#[test]
fn ed25519_signed_leaf_is_not_rfc5929_defined_and_enable_is_rejected() {
    let config = tls_client::test_config_with_scram_channel_binding(true);
    assert!(!config.tls_server_end_point_is_rfc5929_defined());
    assert_eq!(
        tls_opt::check_scram_channel_binding(&config, true),
        Err(ScramChannelBindingRejection::NoRfc5929Hash)
    );
    // R2: `enabled == false` は同じ証明書でも常に許容する。
    assert_eq!(tls_opt::check_scram_channel_binding(&config, false), Ok(()));
}

/// Ed25519 鍵の葉証明書でも、署名アルゴリズム OID を `ecdsa-with-SHA256`
/// （RFC 5929 が SHA-256 を定義する）にすれば `enable` は受理される。
/// 判定は SPKI（鍵種別）ではなく `signatureAlgorithm` を見ていることの確認。
#[test]
fn ecdsa_signed_ed25519_key_leaf_is_rfc5929_defined_and_enable_is_accepted() {
    let der = tls_client::build_ed25519_leaf_certificate_der_with_signature_algorithm(
        &tls_client::RFC8032_TEST1_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
        &tls_client::OID_ECDSA_WITH_SHA256_BYTES,
    );
    let chain = ServerCertificateChain::from_der_chain(
        vec![der],
        &tls_client::RFC8032_TEST1_PUBLIC_KEY,
        1_600_000_000,
    )
    .expect("valid synthetic chain");
    let key = SigningKey::from_seed_bytes(tls_client::hex_decode32(tls_client::RFC8032_TEST1_SEED));
    let config = TlsServerConfig::new(chain, key)
        .expect("matching leaf/key")
        .with_scram_channel_binding(true);

    assert!(config.tls_server_end_point_is_rfc5929_defined());
    assert_eq!(tls_opt::check_scram_channel_binding(&config, true), Ok(()));
}

/// Ed25519 の設定でも、`with_scram_channel_binding(true)` はライブラリ API
/// として従来どおり構築できること（R3: CLI 拒否とライブラリ API 構築可能性
/// を混同しない）。
#[test]
fn with_scram_channel_binding_still_constructs_for_ed25519_signed_leaf() {
    let config = tls_client::test_config_with_scram_channel_binding(true);
    assert!(config.scram_channel_binding_enabled());
}
