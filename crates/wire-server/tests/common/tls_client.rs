//! TLS 1.3 結合テスト向けの最小クライアント（公開 API `wire_server::tls::*`
//! のみを使う。本番コードには持たない test-only DER エンコーダ・鍵材料を
//! 含む）。Issue #966（`wire_tls_connection.rs`）が使う。
//!
//! `tests/tls_server_handshake.rs` の `TestClient`／`drive_client_handshake_
//! over_socket`／`test_config` と同じ構成要素を独立に持つ（意図的な重複。
//! 既存ファイルの大規模な相互依存を崩さずに新規結合テストを追加するため、
//! 本 Issue の範囲では共有モジュールへの一本化は行わない）。

#![allow(dead_code)]

use std::io::{self, Read, Write};
use std::sync::Arc;

use wire_server::tls::certificate_verify;
use wire_server::tls::ed25519::SigningKey;
use wire_server::tls::finished;
use wire_server::tls::handshake::{self, HandshakeType, RawHandshake};
use wire_server::tls::key_schedule::EarlySecret;
use wire_server::tls::record::{self, ContentType, Record, RecordKind};
use wire_server::tls::record_protection::{Opener, Sealer};
use wire_server::tls::server_handshake::TlsServerConfig;
use wire_server::tls::transcript::Transcript;
use wire_server::tls::x25519::EphemeralSecret;
use wire_server::tls::x509::ServerCertificateChain;

pub fn hex_decode(s: &str) -> Vec<u8> {
    let digits: Vec<u8> = s.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    digits
        .chunks(2)
        .map(|pair| {
            let text = std::str::from_utf8(pair).expect("ascii hex pair");
            u8::from_str_radix(text, 16).expect("valid hex byte")
        })
        .collect()
}

pub fn hex_decode32(s: &str) -> [u8; 32] {
    hex_decode(s).try_into().expect("32 bytes")
}

/// RFC 8032 §7.1 TEST 1 の Ed25519 seed／公開鍵（IETF の公開文書由来）。
pub const RFC8032_TEST1_SEED: &str =
    "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60";
pub const RFC8032_TEST1_PUBLIC_KEY: [u8; 32] = [
    0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07, 0x3a,
    0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07, 0x51, 0x1a,
];

// ---- テスト専用 DER エンコーダ（本番コードには持たない）。----

fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let len = body.len();
    if len < 0x80 {
        out.push(len as u8);
    } else if len < 0x100 {
        out.push(0x81);
        out.push(len as u8);
    } else {
        out.push(0x82);
        out.push((len >> 8) as u8);
        out.push((len & 0xff) as u8);
    }
    out.extend_from_slice(body);
    out
}

fn sequence(parts: &[&[u8]]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(part);
    }
    tlv(0x30, &body)
}

const OID_ED25519_BYTES: [u8; 3] = [0x2b, 0x65, 0x70];

fn ed25519_algorithm_identifier() -> Vec<u8> {
    sequence(&[&tlv(0x06, &OID_ED25519_BYTES)])
}

fn utc_time(text: &str) -> Vec<u8> {
    tlv(0x17, text.as_bytes())
}

fn issuer_name() -> Vec<u8> {
    let common_name = sequence(&[&tlv(0x06, &[0x55, 0x04, 0x03]), &tlv(0x0c, b"test-issuer")]);
    sequence(&[&tlv(0x31, &common_name)])
}

fn empty_name() -> Vec<u8> {
    sequence(&[])
}

fn version_v3() -> Vec<u8> {
    tlv(0xa0, &tlv(0x02, &[0x02]))
}

/// `ecdsa-with-SHA256`（1.2.840.10045.4.3.2。RFC 5929 が SHA-256 を定義する
/// 署名アルゴリズム）。Issue #1088: `--tls-scram-channel-binding enable` が
/// 受理される正例（Ed25519 鍵だが署名 OID は ECDSA-SHA256）を組むために使う。
pub const OID_ECDSA_WITH_SHA256_BYTES: [u8; 8] = [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02];

fn algorithm_identifier(oid: &[u8]) -> Vec<u8> {
    sequence(&[&tlv(0x06, oid)])
}

/// 実際に CA が署名した葉証明書の DER（16 進）。codex-review P2 指摘
/// （Issue #1088）への対応: [`build_ed25519_leaf_certificate_der_with_
/// signature_algorithm`] が組む手組み証明書は `signatureValue` が全 0 の
/// プレースホルダで、README/ADR が案内する「RSA/ECDSA CA 署名済み葉証明書」
/// を実際には代表していなかった。本定数は次のコマンドでオフラインに
/// 生成した本物の署名済み DER で、リポジトリはこの DER のみを保持し
/// CA 秘密鍵は生成後に破棄済み（本テスト資産に秘密鍵は含まれない）。
///
/// - 葉の鍵ペア: [`RFC8032_TEST1_SEED`]（Ed25519。SPKI はテスト全体で
///   共有する既存の鍵材料と同一）
/// - CA: 使い捨ての P-256 鍵で自己署名した検証専用ルート（`openssl req
///   -x509 -newkey ec:<(openssl ecparam -name prime256v1)`）
/// - 葉証明書: 上記 CA が `openssl x509 -req -sha256` で署名（P-256 鍵 +
///   SHA-256 のため signatureAlgorithm は `ecdsa-with-SHA256` になる）
/// - validity は既存の手組み証明書と同じ 2016〜2040 年の UTCTime 窓
///   （`-not_before 20160801121924Z -not_after 20401231235959Z`）
///
/// `crates/wire-server/src/tls/x509.rs` のモジュール doc が明記すると
/// おり本サーバーは `signatureValue` の暗号学的検証をスコープ外とする
/// （発行者鍵の検証はクライアントの責務）ため、この DER は「サーバーの
/// 判定を変える」ためではなく「README/ADR が示す構成の葉証明書を実際に
/// 生成し、パーサ・PLUS 交換が現実の CA 署名済み証明書の形状（実サイズの
/// ECDSA 署名・subjectKeyIdentifier/authorityKeyIdentifier 拡張・非空の
/// issuer/subject 等）でも通ることを検証する」ために使う。
const CA_SIGNED_ECDSA_SHA256_LEAF_CERTIFICATE_DER_HEX: &str = "3082013d3081e3a00302010202144455c282ee68a52c2bbec19a7df78840e079502e300a06082a8648ce3d04030230123110300e06035504030c07746573742d6361301e170d3136303830313132313932345a170d3430313233313233353935395a30163114301206035504030c0b746573742d697373756572302a300506032b6570032100d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511aa3423040301d0603551d0e041604145b27aa5589179770e47575b162a1ded97b8bfc6d301f0603551d23041830168014a3289f875b9f22d9ee38da932b312d83619a718a300a06082a8648ce3d0403020349003046022100804e11eb54191ed9cc75c71dcebe115db09caf38549d95a4ba8282f2e0ec6736022100848bf89d55fb6f4100abbd42d023259ccee9fcbdb03d7bc48fd190a9b2a73db4";

/// [`CA_SIGNED_ECDSA_SHA256_LEAF_CERTIFICATE_DER_HEX`] をデコードした
/// 実 CA 署名済み葉証明書の DER（Issue #1088）。鍵ペアは
/// [`RFC8032_TEST1_SEED`]／[`RFC8032_TEST1_PUBLIC_KEY`] と同一のため、
/// 対応する秘密鍵は [`ed25519_pkcs8_pem`] で得られる。
pub fn ca_signed_ecdsa_sha256_leaf_certificate_der() -> Vec<u8> {
    hex_decode(CA_SIGNED_ECDSA_SHA256_LEAF_CERTIFICATE_DER_HEX)
}

/// テスト専用: Ed25519 葉証明書の DER を手組みする（署名は検証対象外の
/// ため全 0 で埋める）。
fn build_ed25519_leaf_certificate_der(public_key: &[u8; 32]) -> Vec<u8> {
    build_ed25519_leaf_certificate_der_with_signature_algorithm(
        public_key,
        "160801121924Z",
        "401231235959Z",
        &OID_ED25519_BYTES,
    )
}

/// [`build_ed25519_leaf_certificate_der_with_validity`] の一般化版
/// （Issue #1088）。SPKI（鍵）は常に Ed25519 のまま、`tbsCertificate.
/// signature`・外側 `signatureAlgorithm`（両者は DER バイト列一致を要求
/// されるため同じ OID にする。`x509::validate_algorithm_identifier_
/// structure` 参照）にだけ任意の署名 OID を指定できる。既存 2 関数は
/// `OID_ED25519_BYTES` を渡してこれへ委譲する薄いラッパーとし、出力は
/// ビット同一のまま保つ。
pub fn build_ed25519_leaf_certificate_der_with_signature_algorithm(
    public_key: &[u8; 32],
    not_before: &str,
    not_after: &str,
    signature_oid: &[u8],
) -> Vec<u8> {
    let signature_algorithm = algorithm_identifier(signature_oid);
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(public_key);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time(not_before), &utc_time(not_after)]);
    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, &[0x01]),
        &signature_algorithm,
        &issuer_name(),
        &validity,
        &empty_name(),
        &spki,
    ]);
    let mut signature_bits = vec![0x00u8];
    signature_bits.extend_from_slice(&[0u8; 64]);
    sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ])
}

/// テスト用サーバー設定（RFC 8032 TEST 1 の鍵材料から作った自己署名
/// 証明書 1 枚）。`wire_tls_connection.rs` が `accept_loop_with_tls` へ渡す。
pub fn test_config() -> Arc<TlsServerConfig> {
    test_config_with_scram_channel_binding(true)
}

/// [`test_config`] と同じ証明書・鍵で、SCRAM-SHA-256-PLUS の提示可否
/// （`TlsServerConfig::with_scram_channel_binding`。Issue #970）だけを
/// 明示的に指定できる版。`tls_channel_binding.rs`・
/// `wire_scram_plus_tls.rs` が提示無効化設定の挙動を検証するために使う。
pub fn test_config_with_scram_channel_binding(enabled: bool) -> Arc<TlsServerConfig> {
    let der = build_ed25519_leaf_certificate_der(&RFC8032_TEST1_PUBLIC_KEY);
    let chain =
        ServerCertificateChain::from_der_chain(vec![der], &RFC8032_TEST1_PUBLIC_KEY, 1_600_000_000)
            .expect("valid synthetic chain");
    let key = SigningKey::from_seed_bytes(hex_decode32(RFC8032_TEST1_SEED));
    let config = TlsServerConfig::new(chain, key)
        .expect("matching leaf/key")
        .with_scram_channel_binding(enabled);
    Arc::new(config)
}

/// [`build_ed25519_leaf_certificate_der`] の validity（`notBefore`／
/// `notAfter`。UTCTime 形式 `YYMMDDHHMMSSZ`）を指定できる版（Issue #967。
/// `tests/wire_tls_cli.rs` が期限切れ証明書の fail-closed 拒否を検証するために
/// 使う）。既存の [`test_config`]／`wire_tls_connection.rs` は本関数を経由せず
/// 引き続き固定 validity（2016〜2040 年）のまま不変。
pub fn build_ed25519_leaf_certificate_der_with_validity(
    public_key: &[u8; 32],
    not_before: &str,
    not_after: &str,
) -> Vec<u8> {
    build_ed25519_leaf_certificate_der_with_signature_algorithm(
        public_key,
        not_before,
        not_after,
        &OID_ED25519_BYTES,
    )
}

/// `label`（`"CERTIFICATE"`／`"PRIVATE KEY"`）で `der` を PEM 形式（64 カラム
/// 改行）へラップする（Issue #967。`wire_server::tls::pem` の受理形状に
/// 合わせる。本番コードには PEM エンコーダを持たない ―― デコードのみ必要
/// なため ―― テスト専用ヘルパー）。
pub fn pem_wrap(label: &str, der: &[u8]) -> String {
    let b64 = wire_server::auth::base64_std::encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 output is ASCII"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
    out
}

/// RFC 8032 §7.1 TEST 1 seed（[`RFC8032_TEST1_SEED`]。生の 32 バイト）から
/// Ed25519 PKCS#8 DER を組み立てる（Issue #967）。PKCS#8 の
/// `OneAsymmetricKey`（RFC 5958）は固定プレフィクス
/// `302e020100300506032b657004220420`（SEQUENCE〔version=0・
/// AlgorithmIdentifier=id-Ed25519・OCTET STRING 長 0x22 の中に
/// OCTET STRING 長 0x20〕）に続けて 32 バイトの seed を並べるだけの形
/// （公開鍵属性・追加属性を持たない最小形）で、`wire_server::tls::pkcs8::
/// parse_ed25519_pkcs8_der` が受理する形状と一致する。
pub fn ed25519_pkcs8_der(seed: &[u8; 32]) -> Vec<u8> {
    const PREFIX_HEX: &str = "302e020100300506032b657004220420";
    let mut der = hex_decode(PREFIX_HEX);
    der.extend_from_slice(seed);
    der
}

/// [`ed25519_pkcs8_der`] を PEM 化したもの（Issue #967）。
pub fn ed25519_pkcs8_pem(seed: &[u8; 32]) -> String {
    pem_wrap("PRIVATE KEY", &ed25519_pkcs8_der(seed))
}

/// `text` を `path` へ書き込む（Issue #967。`tests/wire_tls_cli.rs` が
/// 一時ディレクトリへ証明書・鍵の PEM を書き出すために使う）。
pub fn write_pem_file(path: &std::path::Path, text: &str) {
    std::fs::write(path, text).expect("write PEM file");
}

fn plaintext_handshake_record(wire_bytes: Vec<u8>) -> Record {
    Record {
        content_type: ContentType::Handshake,
        legacy_version: record::LEGACY_RECORD_VERSION,
        fragment: wire_bytes,
    }
}

/// 実ソケット越しにフルハンドシェイクを駆動した後の、application epoch まで
/// 切り替え済みのクライアント側レコード保護状態。`seal`/`open` はいずれも
/// [`wire_server::tls::record_protection`] の公開 API のみを使う。
pub struct TestClient {
    transcript: Transcript,
    pub sealer: Sealer,
    pub opener: Opener,
    /// 受信した `Certificate` メッセージの葉証明書 DER（Issue #970 の
    /// `tls_channel_binding.rs` が、サーバー側 `tls_server_end_point()`
    /// と独立にクライアント側でも `tls-server-end-point` を算出して
    /// 一致検証するために使う）。ハンドシェイク完了前は空のまま。
    pub server_leaf_der: Vec<u8>,
}

fn build_client_hello(client_pub: [u8; 32]) -> (Record, RawHandshake) {
    let supported_versions = handshake::Extension {
        extension_type: 43,
        extension_data: vec![0x02, 0x03, 0x04],
    };
    let signature_algorithms = handshake::Extension {
        extension_type: 13,
        extension_data: vec![0x00, 0x02, 0x08, 0x07],
    };
    let supported_groups = handshake::Extension {
        extension_type: 10,
        extension_data: vec![0x00, 0x02, 0x00, 0x1d],
    };
    let mut key_share_data = vec![0x00, 0x24, 0x00, 0x1d, 0x00, 0x20];
    key_share_data.extend_from_slice(&client_pub);
    let key_share = handshake::Extension {
        extension_type: 51,
        extension_data: key_share_data,
    };
    let extensions = vec![
        supported_groups,
        signature_algorithms,
        supported_versions,
        key_share,
    ];
    let ch = handshake::ClientHello {
        legacy_version: 0x0303,
        random: [0x42u8; 32],
        legacy_session_id: Vec::new(),
        cipher_suites: vec![0x1301],
        legacy_compression_methods: vec![0x00],
        extensions,
    };
    let mut body = Vec::new();
    ch.serialize_body_into(&mut body).expect("valid body");
    let raw = RawHandshake {
        msg_type: HandshakeType::ClientHello,
        body,
    };
    let record = plaintext_handshake_record(raw.to_bytes().expect("valid wire bytes"));
    (record, raw)
}

/// [`build_client_hello`] が組む正規の ClientHello を、レコード層まで直列化
/// した生バイト列として返す（`wire9_tls.rs` の切り詰めハンドシェイク負の
/// テストが、意図的に途中で止めたバイト列を組むために使う）。
pub fn build_client_hello_record_bytes(client_pub: [u8; 32]) -> Vec<u8> {
    let (record, _raw) = build_client_hello(client_pub);
    let mut buf = Vec::new();
    record
        .serialize_into(&mut buf, RecordKind::Plaintext)
        .expect("serialize ClientHello record");
    buf
}

/// TLS 1.2 以下しか提示しない ClientHello を 2 変種で組む（Issue #969・
/// WIRE-9 の負のテスト用）。`negotiate`（`client_hello.rs`）の判定順序は
/// legacy_version・`supported_versions` の TLS 1.3 有無をバージョン以外の
/// 検査より先に見るため、他フィールドは最小構成のままで
/// `protocol_version` alert に到達する。
///
/// - `variant 1`（`with_supported_versions_extension = false`）:
///   `supported_versions` 拡張自体を持たない旧クライアント相当
/// - `variant 2`（`true`）: `supported_versions` を提示するが TLS 1.2
///   （`0x0303`）のみを含む
pub fn tls12_only_client_hello_record_bytes(with_supported_versions_extension: bool) -> Vec<u8> {
    let mut extensions = Vec::new();
    if with_supported_versions_extension {
        extensions.push(handshake::Extension {
            extension_type: 43,
            // ProtocolVersion リスト長 1 バイト（0x02）+ TLS 1.2（0x0303）のみ。
            extension_data: vec![0x02, 0x03, 0x03],
        });
    }
    let ch = handshake::ClientHello {
        legacy_version: 0x0303,
        random: [0x24u8; 32],
        legacy_session_id: Vec::new(),
        // TLS 1.2 の代表的な cipher suite（ECDHE-RSA-AES128-GCM-SHA256 等）。
        // TLS 1.3 専用スイート（0x1301 等）を含めない。
        cipher_suites: vec![0xC02F, 0xC02B],
        legacy_compression_methods: vec![0x00],
        extensions,
    };
    let mut body = Vec::new();
    ch.serialize_body_into(&mut body).expect("valid body");
    let raw = RawHandshake {
        msg_type: HandshakeType::ClientHello,
        body,
    };
    let record = plaintext_handshake_record(raw.to_bytes().expect("valid wire bytes"));
    let mut buf = Vec::new();
    record
        .serialize_into(&mut buf, RecordKind::Plaintext)
        .expect("serialize ClientHello record");
    buf
}

impl TestClient {
    fn new() -> Self {
        TestClient {
            transcript: Transcript::new(),
            sealer: Sealer::new(),
            opener: Opener::new(),
            server_leaf_der: Vec::new(),
        }
    }

    fn process_server_flight(
        &mut self,
        flight_records: &[Record],
        expected_leaf_public_key: &[u8; 32],
        server_hs_traffic: &wire_server::tls::key_schedule::TrafficSecret,
    ) -> [u8; 32] {
        let mut buffer = handshake::HandshakeBuffer::new();
        for record in flight_records {
            let inner = self
                .opener
                .open(record)
                .expect("valid server flight record");
            assert_eq!(inner.content_type, ContentType::Handshake);
            buffer.feed(&inner.content).expect("buffer capacity");
        }

        let ee = buffer
            .next_message()
            .expect("valid message")
            .expect("EncryptedExtensions present");
        assert_eq!(ee.msg_type, HandshakeType::EncryptedExtensions);
        self.transcript
            .append_encrypted_extensions(&ee)
            .expect("valid EncryptedExtensions");

        let cert_raw = buffer
            .next_message()
            .expect("valid message")
            .expect("Certificate present");
        assert_eq!(cert_raw.msg_type, HandshakeType::Certificate);
        let certificate =
            handshake::Certificate::parse(&cert_raw.body).expect("valid Certificate body");
        self.server_leaf_der = certificate
            .certificate_list
            .first()
            .expect("Certificate message carries at least the leaf")
            .cert_data
            .clone();
        self.transcript
            .append_certificate(&cert_raw)
            .expect("valid Certificate");
        let th_ch_cert = self
            .transcript
            .hash_through_certificate()
            .expect("checkpoint reached");

        let cv_raw = buffer
            .next_message()
            .expect("valid message")
            .expect("CertificateVerify present");
        assert_eq!(cv_raw.msg_type, HandshakeType::CertificateVerify);
        let cv =
            handshake::CertificateVerify::parse(&cv_raw.body).expect("valid CertificateVerify");
        certificate_verify::verify_server_certificate_verify(
            expected_leaf_public_key,
            &th_ch_cert,
            &cv,
        )
        .expect("server CertificateVerify must verify");
        self.transcript
            .append_certificate_verify(&cv_raw)
            .expect("valid CertificateVerify");
        let th_ch_cv = self
            .transcript
            .hash_through_certificate_verify()
            .expect("checkpoint reached");

        let sf_raw = buffer
            .next_message()
            .expect("valid message")
            .expect("server Finished present");
        assert_eq!(sf_raw.msg_type, HandshakeType::Finished);
        let server_finished =
            handshake::Finished::parse(&sf_raw.body).expect("valid server Finished");
        let expected_server_finished_key = server_hs_traffic
            .finished_key()
            .expect("valid HKDF parameters");
        let expected_verify_data =
            finished::compute_verify_data(&expected_server_finished_key, &th_ch_cv);
        assert_eq!(
            expected_verify_data.as_bytes().as_slice(),
            server_finished.verify_data.as_slice(),
            "server Finished verify_data must match independently-derived value"
        );
        self.transcript
            .append_server_finished(&sf_raw)
            .expect("valid server Finished");
        let th_ch_sf = self
            .transcript
            .hash_through_server_finished()
            .expect("checkpoint reached");

        assert!(
            buffer.next_message().expect("valid message").is_none() && !buffer.has_partial(),
            "server flight must not contain trailing bytes"
        );

        th_ch_sf
    }
}

/// server flight 受信・ハンドシェイク鍵導入までの共通手順（[`TestClient`]
/// の `sealer`／`opener` には handshake epoch の鍵のみが入った状態で返す）。
/// [`drive_client_handshake_over_socket`]（client Finished まで送出して
/// application epoch へ切り替える）と
/// [`drive_client_handshake_stop_before_finished`]（`wire9_tls.rs` の負の
/// テストが「client Finished を送らずに諦める」挙動を確認するために使う。
/// client Finished を送らないまま止める）の共通前半部分を 1 箇所に持つ。
struct HandshakeUpToServerFlight {
    client: TestClient,
    handshake_secret: wire_server::tls::key_schedule::HandshakeSecret,
    th_ch_sf: [u8; 32],
    client_hs_traffic: wire_server::tls::key_schedule::TrafficSecret,
}

fn drive_up_to_server_flight(client_socket: &mut std::net::TcpStream) -> HandshakeUpToServerFlight {
    let client_priv = [0x33u8; 32];
    let client_ephemeral = EphemeralSecret::from_bytes(client_priv);
    let client_pub = *client_ephemeral.public_key().as_bytes();

    let mut client = TestClient::new();
    let (ch_record, ch_raw) = build_client_hello(client_pub);
    client
        .transcript
        .append_client_hello(&ch_raw)
        .expect("valid ClientHello");

    let mut ch_buf = Vec::new();
    ch_record
        .serialize_into(&mut ch_buf, RecordKind::Plaintext)
        .expect("serialize ClientHello record");
    client_socket
        .write_all(&ch_buf)
        .expect("write ClientHello to loopback stream");

    let sh_record = record::read_record(client_socket, RecordKind::Ciphertext)
        .expect("read ServerHello record")
        .expect("ServerHello record present");
    let sh = handshake::ServerHello::parse(&sh_record.fragment[4..]).expect("valid ServerHello");
    let mut sh_body = Vec::new();
    sh.serialize_body_into(&mut sh_body).expect("valid body");
    let sh_raw = RawHandshake {
        msg_type: HandshakeType::ServerHello,
        body: sh_body,
    };
    client
        .transcript
        .append_server_hello(&sh_raw)
        .expect("valid ServerHello");
    let th_ch_sh = client
        .transcript
        .hash_through_server_hello()
        .expect("checkpoint reached");

    let server_pub = sh
        .extensions
        .iter()
        .find(|e| e.extension_type == 0x0033)
        .and_then(|e| e.extension_data.get(4..36))
        .and_then(|s| <[u8; 32]>::try_from(s).ok())
        .expect("server key_share public key present");

    let shared = client_ephemeral
        .diffie_hellman(&server_pub)
        .expect("valid non-zero shared secret");
    let early = EarlySecret::new_without_psk();
    let handshake_secret = early
        .into_handshake(&shared)
        .expect("valid HKDF parameters");
    let traffic = handshake_secret
        .traffic_secrets(&th_ch_sh)
        .expect("valid HKDF parameters");
    let client_hs_keys = traffic
        .client
        .traffic_keys()
        .expect("valid HKDF parameters");
    let server_hs_keys = traffic
        .server
        .traffic_keys()
        .expect("valid HKDF parameters");
    client
        .sealer
        .install_handshake_keys(&client_hs_keys)
        .expect("valid transition");
    client
        .opener
        .install_handshake_keys(&server_hs_keys)
        .expect("valid transition");

    // server flight（EE・Certificate・CertificateVerify・server Finished）は
    // 本テストの構成（最小の自己署名 ed25519 証明書 1 枚）では 1 ciphertext
    // レコードに収まる。
    let flight_record = record::read_record(client_socket, RecordKind::Ciphertext)
        .expect("read server flight record")
        .expect("server flight record present");
    let th_ch_sf = client.process_server_flight(
        std::slice::from_ref(&flight_record),
        &RFC8032_TEST1_PUBLIC_KEY,
        &traffic.server,
    );

    HandshakeUpToServerFlight {
        client,
        handshake_secret,
        th_ch_sf,
        client_hs_traffic: traffic.client,
    }
}

/// server flight 受信・ハンドシェイク鍵導入までを行い、client Finished は
/// 送らずに返す（Issue #969・WIRE-9 の負のテスト
/// `handshake_abandoned_before_client_finished_closes_without_wire_data`
/// 専用。返る [`TestClient`] は application epoch へ遷移していない）。
pub fn drive_client_handshake_stop_before_finished(
    client_socket: &mut std::net::TcpStream,
) -> TestClient {
    drive_up_to_server_flight(client_socket).client
}

/// 実ソケット越しにフルハンドシェイクを駆動し、application epoch まで
/// 切り替え済みの [`TestClient`] を返す（`tests/tls_server_handshake.rs::
/// drive_client_handshake_over_socket` と同じ手順）。以後は
/// `client.sealer.seal_fragmented(ContentType::ApplicationData, ..)`／
/// `client.opener.open(&record)` で pg wire バイト列を直接やり取りできる。
pub fn drive_client_handshake_over_socket(client_socket: &mut std::net::TcpStream) -> TestClient {
    let HandshakeUpToServerFlight {
        mut client,
        handshake_secret,
        th_ch_sf,
        client_hs_traffic,
    } = drive_up_to_server_flight(client_socket);

    let master = handshake_secret
        .into_master()
        .expect("valid HKDF parameters");
    let app = master
        .application_traffic_secrets(&th_ch_sf)
        .expect("valid HKDF parameters");
    let client_ap_keys = app.client.traffic_keys().expect("valid HKDF parameters");
    let server_ap_keys = app.server.traffic_keys().expect("valid HKDF parameters");

    // client Finished を構成し、handshake epoch のまま送る。
    let client_finished_key = client_hs_traffic
        .finished_key()
        .expect("valid HKDF parameters");
    let verify_data = finished::compute_verify_data(&client_finished_key, &th_ch_sf);
    let client_finished = handshake::Finished {
        verify_data: verify_data.as_bytes().to_vec(),
    };
    let mut cf_body = Vec::new();
    client_finished
        .serialize_body_into(&mut cf_body)
        .expect("valid body");
    let cf_raw = RawHandshake {
        msg_type: HandshakeType::Finished,
        body: cf_body,
    };
    let cf_wire = cf_raw.to_bytes().expect("valid wire bytes");
    let cf_records = client
        .sealer
        .seal_fragmented(ContentType::Handshake, &cf_wire)
        .expect("valid seal");
    let mut cf_buf = Vec::new();
    for record in &cf_records {
        record
            .serialize_into(&mut cf_buf, RecordKind::Ciphertext)
            .expect("serialize client Finished record");
    }
    client_socket
        .write_all(&cf_buf)
        .expect("write client Finished to loopback stream");

    // application epoch へ切り替える（client 側）。
    client
        .sealer
        .install_application_keys(&client_ap_keys)
        .expect("valid transition");
    client
        .opener
        .install_application_keys(&server_ap_keys)
        .expect("valid transition");
    client
}

/// [`TestClient`] を使い、平文 `payload` を 1 個以上の ciphertext レコード
/// として `socket` へ書く（pg wire の `write_all` に相当）。
pub fn send_application_data(
    client: &mut TestClient,
    socket: &mut std::net::TcpStream,
    payload: &[u8],
) {
    let records = client
        .sealer
        .seal_fragmented(ContentType::ApplicationData, payload)
        .expect("valid seal");
    let mut buf = Vec::new();
    for record in &records {
        record
            .serialize_into(&mut buf, RecordKind::Ciphertext)
            .expect("serialize application data record");
    }
    socket.write_all(&buf).expect("write application data");
}

/// [`TestClient`] を使い、`socket` から 1 レコード分の平文アプリケーション
/// データを読む（`ApplicationData` 以外・空フラグメントは読み飛ばす）。
pub fn read_application_data(client: &mut TestClient, socket: &mut std::net::TcpStream) -> Vec<u8> {
    loop {
        let record = record::read_record(socket, RecordKind::Ciphertext)
            .expect("read record")
            .expect("record present");
        let inner = client.opener.open(&record).expect("valid record");
        if inner.content_type == ContentType::ApplicationData && !inner.content.is_empty() {
            return inner.content;
        }
    }
}

/// `std::io::Read`／`Write` を実装する TLS テストチャネル（[`TestClient`]・
/// 生ソケットのラッパー）。`wire_tls_connection.rs` の pg wire メッセージ
/// 送受信ヘルパーを、平文接続用ヘルパーと同じ形（`impl Read + Write`）で
/// 再利用できるようにする。1 回の `write` を 1 回の seal（複数レコードに
/// 分割されうる）として即座に送出し、`read` は復号済み平文を読み切るまで
/// 追加のレコードを読む（production の `TlsStream` と同じ枠組みの簡易版。
/// タイムアウト・fail-closed 状態は持たない test-only 実装）。
pub struct TlsTestChannel {
    client: TestClient,
    socket: std::net::TcpStream,
    plaintext: Vec<u8>,
    pos: usize,
}

impl TlsTestChannel {
    pub fn new(client: TestClient, socket: std::net::TcpStream) -> Self {
        Self {
            client,
            socket,
            plaintext: Vec::new(),
            pos: 0,
        }
    }

    pub fn into_socket(self) -> std::net::TcpStream {
        self.socket
    }

    /// 切断後にサーバーから届くレコードを復号して検査するため、
    /// [`TestClient`]（鍵材料）と生ソケットの両方を取り出す。
    pub fn into_parts(self) -> (TestClient, std::net::TcpStream) {
        (self.client, self.socket)
    }

    /// 受信した葉証明書 DER（[`TestClient::server_leaf_der`]）。
    /// `wire_scram_plus_tls.rs` がクライアント側で独立に
    /// `tls-server-end-point` を算出するために使う（Issue #970）。
    pub fn client_leaf_der(&self) -> &[u8] {
        &self.client.server_leaf_der
    }
}

impl Read for TlsTestChannel {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.plaintext.len() {
            self.plaintext = read_application_data(&mut self.client, &mut self.socket);
            self.pos = 0;
        }
        let remaining = &self.plaintext[self.pos..];
        let n = remaining.len().min(buf.len());
        buf.get_mut(..n)
            .expect("buf at least n bytes")
            .copy_from_slice(&remaining[..n]);
        self.pos += n;
        Ok(n)
    }
}

impl Write for TlsTestChannel {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        send_application_data(&mut self.client, &mut self.socket, buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
