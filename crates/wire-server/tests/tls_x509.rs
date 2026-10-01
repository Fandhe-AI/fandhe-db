//! X.509 最小パース・`Certificate` メッセージ組み立て（`tls::x509`。
//! TASK-228・WIRE-9・HTTP-10 ポインタ。Issue #963・親 #941）の公開 API
//! だけを使う結合テスト。単体テスト（`tls::x509::tests`）の内部関数照合
//! （時刻パース・SPKI 照合の個別関数）とは別に、外部から見える契約
//! （DER 列 → 検査済みチェーン → `Certificate` メッセージ）が RFC 8410 の
//! 公開ベクタと一致すること、および fail-closed な拒否・ファイル入出力
//! エラーの分類を固定する。

use std::io::Write;
use std::path::PathBuf;

use wire_server::tls::handshake::Certificate as HandshakeCertificate;
use wire_server::tls::pem;
use wire_server::tls::x509::{
    self, CertificateChainError, ServerCertificateChain, ServerCertificateLoadError, X509Error,
};

fn unique_temp_path(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "tls-x509-e2e-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ))
}

struct TempFile {
    path: PathBuf,
}

impl TempFile {
    fn write(label: &str, contents: &[u8]) -> Self {
        let path = unique_temp_path(label);
        let mut file = std::fs::File::create(&path).expect("create temp file");
        file.write_all(contents).expect("write temp file");
        TempFile { path }
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        // 削除失敗はテストを失敗させず（Drop 内 panic は二重 panic の恐れがある）、
        // パスとエラーだけを stderr へ出す（Issue #1303）。
        if let Err(e) = std::fs::remove_file(&self.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "warning: failed to remove temp entry {}: {e}",
                    self.path.display()
                );
            }
        }
    }
}

// RFC 8410 §10.2 の X25519 自己発行証明書（PEM。66 文字幅の改行はそのまま
// RFC 原文どおり）。notBefore=2016-08-01T12:19:24Z(1470053964)・
// notAfter=2040-12-31T23:59:59Z(2240611199)。SPKI アルゴリズムは X25519
// であり Ed25519 ではないため、葉に置くと拒否される想定のフィクスチャ。
const RFC8410_10_2_X25519_CERT_PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIBLDCB36ADAgECAghWAUdKKo3DMDAFBgMrZXAwGTEXMBUGA1UEAwwOSUVURiBUZX\nN0IERlbW8wHhcNMTYwODAxMTIxOTI0WhcNNDAxMjMxMjM1OTU5WjAZMRcwFQYDVQQD\nDA5JRVRGIFRlc3QgRGVtbzAqMAUGAytlbgMhAIUg8AmJMKdUdIt93LQ+91oNvzoNJj\nga9OukqY6qm05qo0UwQzAPBgNVHRMBAf8EBTADAQEAMA4GA1UdDwEBAAQEAwIDCDAg\nBgNVHQ4BAQAEFgQUmx9e7e0EM4Xk97xiPFl1uQvIuzswBQYDK2VwA0EAryMB/t3J5v\n/BzKc9dNZIpDmAgs3babFOTQbs+BolzlDUwsPrdGxO3YNGhW7Ibz3OGhhlxXrCe1Cg\nw1AH9efZBw==\n-----END CERTIFICATE-----\n";

// RFC 8410 §10.1 の Ed25519 公開鍵（SPKI の BIT STRING 部・32 バイト）。
// `19bf4409...` は同 RFC §10.3 の秘密鍵 seed に対応する公開鍵でもある
// （#961 着地後の end-to-end 検証にも使える定数）。
const RFC8410_10_1_ED25519_PUBLIC_KEY: [u8; 32] = [
    0x19, 0xbf, 0x44, 0x09, 0x69, 0x84, 0xcd, 0xfe, 0x85, 0x41, 0xba, 0xc1, 0x67, 0xdc, 0x3b, 0x96,
    0xc8, 0x50, 0x86, 0xaa, 0x30, 0xb6, 0xb6, 0xcb, 0x0c, 0x5c, 0x38, 0xad, 0x70, 0x31, 0x66, 0xe1,
];

// RFC 8032 §7.1 TEST 1 の Ed25519 公開鍵（不一致の対照値としてのみ使う）。
const RFC8032_TEST1_ED25519_PUBLIC_KEY: [u8; 32] = [
    0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07, 0x3a,
    0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07, 0x51, 0x1a,
];

/// テスト専用の最小 DER エンコーダ（本番コードには持たない。
/// `tls::pkcs8` の結合テストと同方針）。definite・long-form（2 バイトまで
/// 十分な範囲で）のみ生成する。
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

fn empty_name() -> Vec<u8> {
    // RDNSequence ::= SEQUENCE OF RelativeDistinguishedName（0 個）。
    // subject では許容される（RFC 5280 §4.1.2.6）が、issuer では拒否される
    // （RFC 5280 §4.1.2.4）ため subject 位置専用。
    sequence(&[])
}

/// テスト専用: issuer 用の空でない Name（`CN=test-issuer` の 1 RDN）。
/// RFC 5280 §4.1.2.4 は issuer に空でない DN を要求する。
fn issuer_name() -> Vec<u8> {
    let common_name = sequence(&[&tlv(0x06, &[0x55, 0x04, 0x03]), &tlv(0x0c, b"test-issuer")]);
    sequence(&[&tlv(0x31, &common_name)])
}

/// テスト専用: `version [0] EXPLICIT INTEGER 2` を組み立てる。
fn version_v3() -> Vec<u8> {
    tlv(0xa0, &tlv(0x02, &[0x02]))
}

/// テスト専用: Ed25519 葉証明書の DER を手組みする。`not_before`／
/// `not_after` は UTCTime 文字列（`YYMMDDHHMMSSZ`）で指定する。署名は
/// 検証対象外（受入基準・スコープ外）のため全 0 の 64 バイトで埋める。
fn build_ed25519_leaf_certificate_der(
    public_key: &[u8; 32],
    not_before: &str,
    not_after: &str,
) -> Vec<u8> {
    let signature_algorithm = ed25519_algorithm_identifier();

    let mut spki_bits = vec![0x00u8]; // unused bits = 0
    spki_bits.extend_from_slice(public_key);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);

    let validity = sequence(&[&utc_time(not_before), &utc_time(not_after)]);

    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, &[0x01]), // serialNumber = 1
        &signature_algorithm,
        &issuer_name(), // issuer
        &validity,
        &empty_name(), // subject
        &spki,
    ]);

    let mut signature_bits = vec![0x00u8]; // unused bits = 0
    signature_bits.extend_from_slice(&[0u8; 64]);

    sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ])
}

/// テスト専用: `build_ed25519_leaf_certificate_der` の serialNumber
/// バイト列だけを差し替えられる版（RFC 5280 §4.1.2.2 の serialNumber
/// 検査を固定するために使う）。
fn build_ed25519_leaf_certificate_der_with_serial(
    public_key: &[u8; 32],
    not_before: &str,
    not_after: &str,
    serial: &[u8],
) -> Vec<u8> {
    let signature_algorithm = ed25519_algorithm_identifier();

    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(public_key);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);

    let validity = sequence(&[&utc_time(not_before), &utc_time(not_after)]);

    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, serial),
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

fn pem_wrap_certificate(der: &[u8]) -> String {
    // RFC 8410 §10.2 と同じ 66 文字幅で改行する（PEM lax デコーダの
    // 「行長は強制しない」契約自体は既に固定済みのため、ここでは
    // 単に「デコード可能な PEM」を作れれば十分）。
    let encoded = pem_base64(der);
    let mut out = String::from("-----BEGIN CERTIFICATE-----\n");
    for chunk in encoded.as_bytes().chunks(66) {
        out.push_str(std::str::from_utf8(chunk).expect("ascii base64"));
        out.push('\n');
    }
    out.push_str("-----END CERTIFICATE-----\n");
    out
}

/// テスト専用の標準 base64 エンコーダ（本番の定数時間デコーダとは独立に、
/// フィクスチャ組み立てのためだけに使う）。
fn pem_base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0];
        let b1 = *chunk.get(1).unwrap_or(&0);
        let b2 = *chunk.get(2).unwrap_or(&0);
        let n = ((b0 as u32) << 16) | ((b1 as u32) << 8) | b2 as u32;
        out.push(ALPHABET[(n >> 18 & 0x3f) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6 & 0x3f) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[(n & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn decode_rfc8410_10_2_der() -> Vec<u8> {
    pem::decode_certificate_chain_pem(RFC8410_10_2_X25519_CERT_PEM.as_bytes())
        .expect("valid PEM certificate chain")
        .into_iter()
        .next()
        .expect("exactly one certificate block")
}

// notBefore/notAfter の範囲内（RFC 8410 §10.2 の証明書自身の validity と
// 重なる時刻）。
const NOW_WITHIN_RFC8410_10_2_VALIDITY: i64 = 1_600_000_000; // 2020-09-13 頃

#[test]
fn leaf_with_x25519_spki_is_rejected_as_unsupported_algorithm() {
    let der = decode_rfc8410_10_2_der();
    let expected = [0u8; 32];
    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &expected,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert!(matches!(error, X509Error::UnsupportedPublicKeyAlgorithm(_)));
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn handmade_ed25519_leaf_is_accepted_with_matching_public_key() {
    let der = build_ed25519_leaf_certificate_der(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
    );
    let chain = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .expect("Ed25519 leaf with matching public key must be accepted");
    assert_eq!(chain.leaf_public_key(), &RFC8410_10_1_ED25519_PUBLIC_KEY);
}

#[test]
fn chain_of_ed25519_leaf_and_x25519_intermediate_preserves_order_in_certificate_message() {
    let leaf_der = build_ed25519_leaf_certificate_der(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
    );
    let intermediate_der = decode_rfc8410_10_2_der();

    let chain = ServerCertificateChain::from_der_chain(
        vec![leaf_der.clone(), intermediate_der.clone()],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .expect("leaf Ed25519 + intermediate X25519-signed-by-Ed25519 chain must be accepted");

    let message = chain.certificate_message();
    assert_eq!(message.certificate_request_context, Vec::<u8>::new());
    assert_eq!(message.certificate_list.len(), 2);
    assert_eq!(message.certificate_list[0].cert_data, leaf_der);
    assert_eq!(message.certificate_list[1].cert_data, intermediate_der);
    assert!(message.certificate_list[0].extensions.is_empty());
    assert!(message.certificate_list[1].extensions.is_empty());

    // Certificate::encode_into → Certificate::parse の往復がビット一致
    // すること（RFC 8446 §4.4.2 のコーデック契約。Issue #953）。
    let mut encoded = Vec::new();
    message.encode_into(&mut encoded).expect("encode succeeds");
    // encode_into はハンドシェイクヘッダ（type + u24 長さ）を含むため、
    // 本体一致の確認は serialize_body_into 側で行う。
    let mut body = Vec::new();
    message
        .serialize_body_into(&mut body)
        .expect("serialize_body_into succeeds");
    let round_tripped = HandshakeCertificate::parse(&body).expect("parse succeeds");
    assert_eq!(round_tripped, message);
}

#[test]
fn public_key_mismatch_is_rejected() {
    let der = build_ed25519_leaf_certificate_der(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
    );
    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8032_TEST1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::PublicKeyMismatch);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn public_key_mismatch_with_single_bit_flip_is_rejected() {
    let mut flipped = RFC8410_10_1_ED25519_PUBLIC_KEY;
    flipped[0] ^= 0x01;
    let der = build_ed25519_leaf_certificate_der(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
    );
    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &flipped,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::PublicKeyMismatch);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn intermediate_not_yet_valid_and_expired_are_rejected_at_index_1() {
    // 葉証明書の validity は 1950〜2049 年（UTCTime の範囲）をカバーし、
    // 注入した now がどちらの境界でも葉自身は常に有効になるようにする
    // （葉の判定が先に発生して中間証明書の判定を隠さないようにするため）。
    let leaf_der = build_ed25519_leaf_certificate_der(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "500101000000Z",
        "491231235959Z",
    );
    let intermediate_der = decode_rfc8410_10_2_der();

    // 中間証明書の notBefore（2016-08-01T12:19:24Z）より前。
    let before_not_before = 1_000_000_000i64; // 2001-09-09 頃
    let err = ServerCertificateChain::from_der_chain(
        vec![leaf_der.clone(), intermediate_der.clone()],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        before_not_before,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 1, error } => {
            assert_eq!(error, X509Error::NotYetValid);
        }
        other => panic!("unexpected error: {other:?}"),
    }

    // 中間証明書の notAfter（2040-12-31T23:59:59Z）より後。
    let after_not_after = 2_300_000_000i64; // 2042-11-22 頃
    let err = ServerCertificateChain::from_der_chain(
        vec![leaf_der, intermediate_der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        after_not_after,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 1, error } => {
            assert_eq!(error, X509Error::Expired);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn empty_chain_is_rejected() {
    let err = ServerCertificateChain::from_der_chain(vec![], &[0u8; 32], 0).unwrap_err();
    assert_eq!(err, CertificateChainError::EmptyChain);
}

#[test]
fn chain_longer_than_limit_is_rejected() {
    let leaf_der = build_ed25519_leaf_certificate_der(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
    );
    let intermediate_der = decode_rfc8410_10_2_der();
    let mut chain = vec![leaf_der];
    for _ in 0..pem::MAX_CERTIFICATE_CHAIN_LEN {
        chain.push(intermediate_der.clone());
    }
    assert_eq!(chain.len(), pem::MAX_CERTIFICATE_CHAIN_LEN + 1);
    let err = ServerCertificateChain::from_der_chain(
        chain,
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    assert_eq!(
        err,
        CertificateChainError::ChainTooLong {
            max: pem::MAX_CERTIFICATE_CHAIN_LEN
        }
    );
}

#[test]
fn truncated_certificate_is_rejected_without_panicking() {
    let der = decode_rfc8410_10_2_der();
    let truncated = &der[..der.len() - 10];
    let err = ServerCertificateChain::from_der_chain(vec![truncated.to_vec()], &[0u8; 32], 0)
        .unwrap_err();
    assert!(matches!(
        err,
        CertificateChainError::Certificate { index: 0, .. }
    ));
}

#[test]
fn certificate_with_trailing_byte_is_rejected() {
    let mut der = decode_rfc8410_10_2_der();
    der.push(0x00);
    let err = ServerCertificateChain::from_der_chain(vec![der], &[0u8; 32], 0).unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::Malformed);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn pkcs8_der_passed_as_certificate_is_rejected_without_panicking() {
    // RFC 8410 §10.3 の PKCS#8 DER（証明書としては構造がまったく異なる）。
    let pkcs8_der: Vec<u8> = vec![
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20, 0xd4, 0xee, 0x72, 0xdb, 0xf9, 0x13, 0x58, 0x4a, 0xd5, 0xb6, 0xd8, 0xf1, 0xf7, 0x69,
        0xf8, 0xad, 0x3a, 0xfe, 0x7c, 0x28, 0xcb, 0xf1, 0xd4, 0xfb, 0xe0, 0x97, 0xa8, 0x8f, 0x44,
        0x75, 0x58, 0x42,
    ];
    let err = ServerCertificateChain::from_der_chain(vec![pkcs8_der], &[0u8; 32], 0).unwrap_err();
    assert!(matches!(
        err,
        CertificateChainError::Certificate { index: 0, .. }
    ));
}

#[test]
fn version_v1_without_explicit_tag_is_rejected() {
    // version フィールドを省略した v1 相当（tbsCertificate の残りは
    // 有効な Ed25519 葉と同じ構成）。
    let signature_algorithm = ed25519_algorithm_identifier();
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let tbs_certificate = sequence(&[
        // version を省略（v1）。
        &tlv(0x02, &[0x01]),
        &signature_algorithm,
        &issuer_name(),
        &validity,
        &empty_name(),
        &spki,
    ]);
    let mut signature_bits = vec![0x00u8];
    signature_bits.extend_from_slice(&[0u8; 64]);
    let der = sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ]);

    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::UnsupportedVersion);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn version_v2_integer_1_is_rejected() {
    let signature_algorithm = ed25519_algorithm_identifier();
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let version_v2 = tlv(0xa0, &tlv(0x02, &[0x01]));
    let tbs_certificate = sequence(&[
        &version_v2,
        &tlv(0x02, &[0x01]),
        &signature_algorithm,
        &issuer_name(),
        &validity,
        &empty_name(),
        &spki,
    ]);
    let mut signature_bits = vec![0x00u8];
    signature_bits.extend_from_slice(&[0u8; 64]);
    let der = sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ]);

    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::UnsupportedVersion);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn tbs_signature_mismatch_with_outer_signature_algorithm_is_rejected() {
    let tbs_signature_algorithm = ed25519_algorithm_identifier();
    // 外側の signatureAlgorithm だけを X25519 に差し替える
    // （RFC 5280 §4.1.1.2 の一致要求への違反）。
    let outer_signature_algorithm = sequence(&[&tlv(0x06, &[0x2b, 0x65, 0x6e])]);

    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, &[0x01]),
        &tbs_signature_algorithm,
        &issuer_name(),
        &validity,
        &empty_name(),
        &spki,
    ]);
    let mut signature_bits = vec![0x00u8];
    signature_bits.extend_from_slice(&[0u8; 64]);
    let der = sequence(&[
        &tbs_certificate,
        &outer_signature_algorithm,
        &tlv(0x03, &signature_bits),
    ]);

    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::SignatureAlgorithmMismatch);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn empty_algorithm_identifier_matching_on_both_sides_is_rejected() {
    // tbsCertificate.signature と外側 signatureAlgorithm を、いずれも
    // algorithm OID を持たない空 SEQUENCE（raw バイト列としては完全一致）
    // にした場合の拒否を固定する。DER バイト列一致検査（手順 5）だけでは
    // 両者が「等しく空」であることを見逃すため、AlgorithmIdentifier
    // としての構造検査（algorithm OID の存在）が別途必要になる。
    let empty_algorithm_identifier = sequence(&[]);

    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, &[0x01]),
        &empty_algorithm_identifier,
        &issuer_name(),
        &validity,
        &empty_name(),
        &spki,
    ]);
    let mut signature_bits = vec![0x00u8];
    signature_bits.extend_from_slice(&[0u8; 64]);
    let der = sequence(&[
        &tbs_certificate,
        &empty_algorithm_identifier,
        &tlv(0x03, &signature_bits),
    ]);

    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::Malformed);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn algorithm_identifier_with_trailing_excess_element_is_rejected() {
    // OID の後ろに parameters を超える 2 個目の要素を続けた
    // AlgorithmIdentifier（tbs・外側とも同一バイト列で raw 一致はするが、
    // 許容外の余剰要素を含むため構造検査で拒否されるべきケース）。
    let algorithm_identifier_with_excess = sequence(&[
        &tlv(0x06, &OID_ED25519_BYTES),
        &tlv(0x05, &[]), // parameters（NULL）
        &tlv(0x05, &[]), // 余剰の 2 個目の要素
    ]);

    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, &[0x01]),
        &algorithm_identifier_with_excess,
        &issuer_name(),
        &validity,
        &empty_name(),
        &spki,
    ]);
    let mut signature_bits = vec![0x00u8];
    signature_bits.extend_from_slice(&[0u8; 64]);
    let der = sequence(&[
        &tbs_certificate,
        &algorithm_identifier_with_excess,
        &tlv(0x03, &signature_bits),
    ]);

    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::Malformed);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn signature_value_with_no_signature_data_after_unused_bits_octet_is_rejected() {
    // unused-bits オクテットのみで署名データが 1 バイトも無い
    // signatureValue（実体の無い BIT STRING）を拒否する。
    let signature_algorithm = ed25519_algorithm_identifier();
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, &[0x01]),
        &signature_algorithm,
        &issuer_name(),
        &validity,
        &empty_name(),
        &spki,
    ]);
    // unused-bits オクテットのみ（0x00）で後続の署名バイト列が無い。
    let signature_bits = vec![0x00u8];
    let der = sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ]);

    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::Malformed);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn signature_value_with_nonzero_unused_bits_and_nonzero_padding_is_rejected() {
    // unused-bits が非ゼロ（4）なのに最終オクテットの下位 4 ビットが
    // ゼロでない、非正規（non-canonical）な DER エンコーディングを拒否する。
    let signature_algorithm = ed25519_algorithm_identifier();
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, &[0x01]),
        &signature_algorithm,
        &issuer_name(),
        &validity,
        &empty_name(),
        &spki,
    ]);
    let mut signature_bits = vec![0x04u8]; // unused bits = 4
    signature_bits.extend_from_slice(&[0u8; 63]);
    signature_bits.push(0xffu8); // 最終オクテットの下位 4 ビットが非ゼロ
    let der = sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ]);

    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::Malformed);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn signature_value_with_nonzero_unused_bits_and_zero_padding_is_accepted() {
    // unused_bits=4・最終オクテットの下位 4 ビットが全てゼロ（正規 DER）
    // は受理されることを固定する。unused-bits マスク検査が過剰拒否に
    // ならないことの対照ケース。
    let signature_algorithm = ed25519_algorithm_identifier();
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, &[0x01]),
        &signature_algorithm,
        &issuer_name(),
        &validity,
        &empty_name(),
        &spki,
    ]);
    let mut signature_bits = vec![0x04u8]; // unused bits = 4
    signature_bits.extend_from_slice(&[0u8; 63]);
    signature_bits.push(0xf0u8); // 下位 4 ビットが全てゼロ（正規）
    let der = sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ]);

    ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .expect("well-formed unused-bits padding must be accepted");
}

#[test]
fn algorithm_identifier_with_malformed_oid_is_rejected() {
    // OID の値部分が「最後のサブ識別子が継続ビット付きのまま終端」して
    // いる、切り詰められた不正な OID（`0x2b 0x80`）。タグ・長さは
    // 整形式のため `validate_structure` は通すが、OID としては無効。
    let malformed_oid_algorithm = sequence(&[&tlv(0x06, &[0x2b, 0x80])]);

    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, &[0x01]),
        &malformed_oid_algorithm,
        &issuer_name(),
        &validity,
        &empty_name(),
        &spki,
    ]);
    let mut signature_bits = vec![0x00u8];
    signature_bits.extend_from_slice(&[0u8; 64]);
    let der = sequence(&[
        &tbs_certificate,
        &malformed_oid_algorithm,
        &tlv(0x03, &signature_bits),
    ]);

    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::Malformed);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn spki_with_parameters_is_rejected() {
    let signature_algorithm = ed25519_algorithm_identifier();
    // Ed25519 の OID の後ろに NULL parameters を付けた不正な SPKI。
    let spki_algorithm_with_params = sequence(&[&tlv(0x06, &OID_ED25519_BYTES), &tlv(0x05, &[])]);
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&spki_algorithm_with_params, &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
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
    let der = sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ]);

    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::InvalidPublicKey);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn spki_bit_string_unused_bits_nonzero_is_rejected() {
    // 未使用ビット数 1 でも DER としては正当（最終オクテットの未使用ビットが
    // 0）な BIT STRING にし、RFC 8410 §3 の「Ed25519 鍵の未使用ビット数は
    // 0」という葉の SPKI 検査（InvalidPublicKey）だけを観測する。
    // RFC 8410 §10.1 の鍵は最終オクテットが奇数のため、その最下位ビットを
    // 落とす（非 0 パディングは DER 違反として Malformed になる。別テスト）。
    let signature_algorithm = ed25519_algorithm_identifier();
    let mut spki_bits = vec![0x01u8]; // unused bits != 0
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    if let Some(last) = spki_bits.last_mut() {
        *last &= 0xfe;
    }
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
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
    let der = sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ]);

    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::InvalidPublicKey);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn spki_key_length_31_and_33_are_rejected() {
    for bad_len in [31usize, 33usize] {
        let signature_algorithm = ed25519_algorithm_identifier();
        let mut spki_bits = vec![0x00u8];
        spki_bits.extend_from_slice(&vec![0u8; bad_len]);
        let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
        let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
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
        let der = sequence(&[
            &tbs_certificate,
            &signature_algorithm,
            &tlv(0x03, &signature_bits),
        ]);

        let err = ServerCertificateChain::from_der_chain(
            vec![der],
            &RFC8410_10_1_ED25519_PUBLIC_KEY,
            NOW_WITHIN_RFC8410_10_2_VALIDITY,
        )
        .unwrap_err();
        match err {
            CertificateChainError::Certificate { index: 0, error } => {
                assert_eq!(error, X509Error::InvalidPublicKey);
            }
            other => panic!("unexpected error ({bad_len} bytes): {other:?}"),
        }
    }
}

#[test]
fn certificate_der_exceeding_max_len_is_rejected() {
    let oversized = vec![0u8; x509::MAX_CERTIFICATE_DER_LEN + 1];
    let err = ServerCertificateChain::from_der_chain(vec![oversized], &[0u8; 32], 0).unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::CertificateTooLarge);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn error_display_does_not_leak_certificate_content() {
    let der = decode_rfc8410_10_2_der();
    let err = ServerCertificateChain::from_der_chain(vec![der], &[0u8; 32], 0).unwrap_err();
    let text = format!("{err}");
    assert!(!text.contains("IETF Test Demo"));
    assert!(!text.contains("85520f0"));
}

#[test]
fn load_server_certificate_chain_file_reaches_chain_check_via_file_path() {
    let file = TempFile::write("chain-ok", RFC8410_10_2_X25519_CERT_PEM.as_bytes());
    // X25519 の葉は Ed25519 以外として拒否されるが、ファイル読み込み経路
    // （PEM → DER 列 → チェーン検査）が正しく合成されていることは
    // 「チェーン検査まで到達して index 0 の理由で拒否される」ことで確認できる。
    let err = x509::load_server_certificate_chain_file(&file.path, &[0u8; 32], 0).unwrap_err();
    assert!(matches!(
        err,
        ServerCertificateLoadError::Chain(CertificateChainError::Certificate { index: 0, .. })
    ));
}

#[test]
fn load_server_certificate_chain_file_reports_missing_file() {
    let path = unique_temp_path("missing");
    let err = x509::load_server_certificate_chain_file(&path, &[0u8; 32], 0).unwrap_err();
    assert!(matches!(err, ServerCertificateLoadError::Load(_)));
}

#[test]
fn load_server_certificate_chain_file_accepts_handmade_ed25519_leaf() {
    let der = build_ed25519_leaf_certificate_der(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
    );
    let file = TempFile::write("leaf-ok", pem_wrap_certificate(&der).as_bytes());
    let chain = x509::load_server_certificate_chain_file(
        &file.path,
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .expect("valid Ed25519 leaf file must be accepted");
    assert_eq!(chain.leaf_public_key(), &RFC8410_10_1_ED25519_PUBLIC_KEY);
}

#[test]
fn serial_number_negative_zero_oversized_and_non_minimal_are_rejected() {
    // 負数（最上位ビットが立つ 1 オクテット）。
    let negative = build_ed25519_leaf_certificate_der_with_serial(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
        &[0x80],
    );
    // ゼロ。
    let zero = build_ed25519_leaf_certificate_der_with_serial(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
        &[0x00],
    );
    // 21 オクテット（RFC 5280 の上限 20 を超える）。
    let oversized = build_ed25519_leaf_certificate_der_with_serial(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
        &[0x01; 21],
    );
    // 非最小符号化（次オクテットの最上位ビットが立っていないのに不要な
    // 先頭 0x00 を付けている）。
    let non_minimal = build_ed25519_leaf_certificate_der_with_serial(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
        &[0x00, 0x01],
    );

    for der in [negative, zero, oversized, non_minimal] {
        let err = ServerCertificateChain::from_der_chain(
            vec![der],
            &RFC8410_10_1_ED25519_PUBLIC_KEY,
            NOW_WITHIN_RFC8410_10_2_VALIDITY,
        )
        .unwrap_err();
        match err {
            CertificateChainError::Certificate { index: 0, error } => {
                assert_eq!(error, X509Error::Malformed);
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }
}

#[test]
fn serial_number_20_octets_and_minimal_high_bit_encoding_are_accepted() {
    // 20 オクテットちょうど（上限）は受理される。
    let max_len = build_ed25519_leaf_certificate_der_with_serial(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
        &[0x7f; 20],
    );
    ServerCertificateChain::from_der_chain(
        vec![max_len],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .expect("20-octet serial number must be accepted");

    // 最上位ビットが立つ正の値に対する、符号ビット確保のための単一の
    // 0x00 接頭辞は最小符号化として受理される。
    let minimal_high_bit = build_ed25519_leaf_certificate_der_with_serial(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
        &[0x00, 0x80],
    );
    ServerCertificateChain::from_der_chain(
        vec![minimal_high_bit],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .expect("minimal 0x00-prefixed high-bit serial number must be accepted");
}

#[test]
fn intermediate_spki_algorithm_identifier_with_trailing_excess_element_is_rejected() {
    // 中間証明書（チェーン index 1）の SPKI AlgorithmIdentifier に、
    // parameters を超える余剰の 2 個目の要素を付けた不正な形。葉証明書側の
    // Ed25519 判定（OID 完全一致・parameters 不在）は index 0 にしか
    // 適用されないため、この不正な形が中間証明書側でも構造検査自体で
    // 拒否されることを固定する（Issue #963 レビュー指摘）。
    let leaf_der = build_ed25519_leaf_certificate_der(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
    );

    let signature_algorithm = ed25519_algorithm_identifier();
    let spki_algorithm_with_excess = sequence(&[
        &tlv(0x06, &OID_ED25519_BYTES),
        &tlv(0x05, &[]), // parameters（NULL）
        &tlv(0x05, &[]), // 余剰の 2 個目の要素
    ]);
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&spki_algorithm_with_excess, &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
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
    let intermediate_der = sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ]);

    let err = ServerCertificateChain::from_der_chain(
        vec![leaf_der, intermediate_der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 1, error } => {
            assert_eq!(error, X509Error::Malformed);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn intermediate_spki_algorithm_identifier_with_truncated_oid_is_rejected() {
    // 中間証明書（index 1）の SPKI OID が「最後のサブ識別子が継続ビット付き
    // のまま終端」している、切り詰められた不正な OID（`0x2b 0x80`）。
    let leaf_der = build_ed25519_leaf_certificate_der(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
    );

    let signature_algorithm = ed25519_algorithm_identifier();
    let spki_algorithm_truncated = sequence(&[&tlv(0x06, &[0x2b, 0x80])]);
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&spki_algorithm_truncated, &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
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
    let intermediate_der = sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ]);

    let err = ServerCertificateChain::from_der_chain(
        vec![leaf_der, intermediate_der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 1, error } => {
            assert_eq!(error, X509Error::Malformed);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn issuer_name_with_primitive_set_is_rejected() {
    // issuer Name 内の RDN を表す SET（0x31）を primitive の 0x11 へ
    // 置き換えた不正 DER（PR #1036 codex-review P1 指摘その 1）。
    // DER は SEQUENCE／SET を常に constructed で符号化することを要求する。
    let issuer_with_primitive_set = sequence(&[&tlv(0x11, &[])]);

    let signature_algorithm = ed25519_algorithm_identifier();
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, &[0x01]),
        &signature_algorithm,
        &issuer_with_primitive_set,
        &validity,
        &empty_name(),
        &spki,
    ]);
    let mut signature_bits = vec![0x00u8];
    signature_bits.extend_from_slice(&[0u8; 64]);
    let der = sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ]);

    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::Malformed);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn subject_name_with_rdn_encoded_as_sequence_instead_of_set_is_rejected() {
    // RDNSequence の要素（RelativeDistinguishedName）が SET（0x31）ではなく
    // SEQUENCE（0x30）で符号化された不正な subject Name
    // （PR #1036 codex-review P1 指摘その 2。generic な DER 構造検証だけでは
    // universal 型の primitive/constructed 制約しか見ないため、この
    // タグ固有の構造違反は見逃されていた）。
    let attribute_type_and_value = sequence(&[&tlv(0x06, &OID_ED25519_BYTES), &tlv(0x05, &[])]);
    let subject_with_rdn_as_sequence = sequence(&[&attribute_type_and_value]);

    let signature_algorithm = ed25519_algorithm_identifier();
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, &[0x01]),
        &signature_algorithm,
        &issuer_name(),
        &validity,
        &subject_with_rdn_as_sequence,
        &spki,
    ]);
    let mut signature_bits = vec![0x00u8];
    signature_bits.extend_from_slice(&[0u8; 64]);
    let der = sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ]);

    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::Malformed);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn extensions_wrapper_with_trailing_data_is_rejected() {
    // extensions [3] EXPLICIT の中身が Extensions（SEQUENCE）1 個の後に
    // 余剰バイトを持つ不正 DER（PR #1036 codex-review P1 指摘その 2）。
    let extension = sequence(&[&tlv(0x06, &[0x55, 0x1d, 0x0f]), &tlv(0x04, &[0x00])]);
    let extensions_seq = sequence(&[&extension]);
    let mut extensions_wrapper_value = extensions_seq.clone();
    extensions_wrapper_value.extend_from_slice(&tlv(0x05, &[]));
    let extensions_field = tlv(0xa3, &extensions_wrapper_value);

    let signature_algorithm = ed25519_algorithm_identifier();
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, &[0x01]),
        &signature_algorithm,
        &issuer_name(),
        &validity,
        &empty_name(),
        &spki,
        &extensions_field,
    ]);
    let mut signature_bits = vec![0x00u8];
    signature_bits.extend_from_slice(&[0u8; 64]);
    let der = sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ]);

    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::Malformed);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

/// テスト専用: issuer／subject Name と任意の extensions `[3]` フィールドを
/// 差し替えた Ed25519 葉証明書の DER を手組みする（RDN の SET OF 順序・
/// Extension 構文の回帰テスト用。PR #1036 codex-review P1 指摘）。
fn build_ed25519_leaf_with_names_and_extensions(
    issuer: &[u8],
    subject: &[u8],
    extensions_field: Option<&[u8]>,
) -> Vec<u8> {
    let signature_algorithm = ed25519_algorithm_identifier();
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let serial = tlv(0x02, &[0x01]);
    let version = version_v3();
    let mut tbs_parts: Vec<&[u8]> = vec![
        &version,
        &serial,
        &signature_algorithm,
        issuer,
        &validity,
        subject,
        &spki,
    ];
    if let Some(field) = extensions_field {
        tbs_parts.push(field);
    }
    let tbs_certificate = sequence(&tbs_parts);
    let mut signature_bits = vec![0x00u8];
    signature_bits.extend_from_slice(&[0u8; 64]);
    sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ])
}

/// テスト専用: 1 個以上の Extension 本体（各 `Extension` の SEQUENCE の
/// 値部分）から `extensions [3] EXPLICIT` フィールドを組み立てる。
fn extensions_field_from_bodies(extension_bodies: &[&[u8]]) -> Vec<u8> {
    let extensions: Vec<Vec<u8>> = extension_bodies
        .iter()
        .map(|body| tlv(0x30, body))
        .collect();
    let parts: Vec<&[u8]> = extensions.iter().map(Vec::as_slice).collect();
    tlv(0xa3, &sequence(&parts))
}

fn assert_leaf_rejected_as_malformed(der: Vec<u8>) {
    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    match err {
        CertificateChainError::Certificate { index: 0, error } => {
            assert_eq!(error, X509Error::Malformed);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

fn assert_leaf_accepted(der: Vec<u8>) {
    let chain = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .expect("well-formed leaf certificate must be accepted");
    assert_eq!(chain.leaf_public_key(), &RFC8410_10_1_ED25519_PUBLIC_KEY);
}

// id-ce-keyUsage（2.5.29.15）・id-ce-basicConstraints（2.5.29.19）。
const OID_KEY_USAGE_TLV: [u8; 5] = [0x06, 0x03, 0x55, 0x1d, 0x0f];
const OID_BASIC_CONSTRAINTS_TLV: [u8; 5] = [0x06, 0x03, 0x55, 0x1d, 0x13];
const CRITICAL_TRUE_TLV: [u8; 3] = [0x01, 0x01, 0xff];
const CRITICAL_FALSE_TLV: [u8; 3] = [0x01, 0x01, 0x00];

fn octet_string_extn_value() -> Vec<u8> {
    // extnValue OCTET STRING（中身は keyUsage 相当の BIT STRING。意味は
    // パーサが解釈しない）。
    tlv(0x04, &tlv(0x03, &[0x07, 0x80]))
}

#[test]
fn extensions_with_valid_extension_syntax_are_accepted() {
    // critical 省略・critical TRUE・critical FALSE 明示（RFC 8410 §10.2 の
    // 公開ベクタと同じ形）の 3 形状がいずれも受理されること。
    let extn_value = octet_string_extn_value();
    let omitted = [OID_KEY_USAGE_TLV.as_slice(), &extn_value].concat();
    let critical_true = [
        OID_BASIC_CONSTRAINTS_TLV.as_slice(),
        &CRITICAL_TRUE_TLV,
        &tlv(0x04, &sequence(&[])),
    ]
    .concat();
    assert_leaf_accepted(build_ed25519_leaf_with_names_and_extensions(
        &issuer_name(),
        &issuer_name(),
        Some(&extensions_field_from_bodies(&[&omitted, &critical_true])),
    ));

    let critical_false = [
        OID_KEY_USAGE_TLV.as_slice(),
        &CRITICAL_FALSE_TLV,
        &extn_value,
    ]
    .concat();
    assert_leaf_accepted(build_ed25519_leaf_with_names_and_extensions(
        &issuer_name(),
        &issuer_name(),
        Some(&extensions_field_from_bodies(&[&critical_false])),
    ));
}

#[test]
fn extension_with_invalid_internal_syntax_is_rejected() {
    // Extension ::= SEQUENCE { extnID OID, critical BOOLEAN DEFAULT FALSE,
    // extnValue OCTET STRING } に反する各形状（PR #1036 codex-review P1
    // 指摘: 従来は各要素が SEQUENCE であることしか見ていなかった）。
    let extn_value = octet_string_extn_value();
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("empty extension", Vec::new()),
        (
            "missing extnID",
            [CRITICAL_TRUE_TLV.as_slice(), &extn_value].concat(),
        ),
        ("missing extnValue", OID_KEY_USAGE_TLV.to_vec()),
        (
            "critical after extnValue",
            [
                OID_KEY_USAGE_TLV.as_slice(),
                &extn_value,
                &CRITICAL_TRUE_TLV,
            ]
            .concat(),
        ),
        (
            "extnValue is BIT STRING instead of OCTET STRING",
            [OID_KEY_USAGE_TLV.as_slice(), &tlv(0x03, &[0x07, 0x80])].concat(),
        ),
        (
            "trailing element after extnValue",
            [OID_KEY_USAGE_TLV.as_slice(), &extn_value, &tlv(0x05, &[])].concat(),
        ),
        (
            "critical encoded as INTEGER",
            [
                OID_KEY_USAGE_TLV.as_slice(),
                &tlv(0x02, &[0x01]),
                &extn_value,
            ]
            .concat(),
        ),
        (
            "truncated extnID",
            [tlv(0x06, &[0x55, 0x1d, 0x8f]).as_slice(), &extn_value].concat(),
        ),
    ];
    let valid_first = [OID_BASIC_CONSTRAINTS_TLV.as_slice(), &extn_value].concat();
    for (label, body) in cases {
        // 不正な Extension が単独でも、正当な Extension の後ろに続いても拒否される。
        for bodies in [vec![body.as_slice()], vec![valid_first.as_slice(), &body]] {
            let der = build_ed25519_leaf_with_names_and_extensions(
                &issuer_name(),
                &issuer_name(),
                Some(&extensions_field_from_bodies(&bodies)),
            );
            let err = ServerCertificateChain::from_der_chain(
                vec![der],
                &RFC8410_10_1_ED25519_PUBLIC_KEY,
                NOW_WITHIN_RFC8410_10_2_VALIDITY,
            )
            .expect_err(label);
            assert_eq!(
                err,
                CertificateChainError::Certificate {
                    index: 0,
                    error: X509Error::Malformed
                },
                "{label}"
            );
        }
    }
}

/// テスト専用: `AttributeTypeAndValue ::= SEQUENCE { type OID, value }`
/// を組み立てる（値は UTF8String）。
fn attribute_type_and_value(oid_last: u8, value: &str) -> Vec<u8> {
    sequence(&[
        &tlv(0x06, &[0x55, 0x04, oid_last]),
        &tlv(0x0c, value.as_bytes()),
    ])
}

#[test]
fn multi_valued_rdn_in_der_ascending_order_is_accepted() {
    // commonName（2.5.4.3）と organizationName（2.5.4.10）を 1 個の RDN
    // （SET OF）にまとめ、符号化バイト列の昇順（0x03 < 0x0a）で並べた
    // 正規 DER。issuer・subject の双方で受理されること。
    let cn = attribute_type_and_value(0x03, "a");
    let org = attribute_type_and_value(0x0a, "a");
    let name = sequence(&[&tlv(0x31, &[cn.as_slice(), &org].concat())]);
    assert_leaf_accepted(build_ed25519_leaf_with_names_and_extensions(
        &name, &name, None,
    ));

    // 同一符号化の重複要素も昇順（非減少）の定義上は許容する。
    let duplicated = sequence(&[&tlv(0x31, &[cn.as_slice(), &cn].concat())]);
    assert_leaf_accepted(build_ed25519_leaf_with_names_and_extensions(
        &duplicated,
        &empty_name(),
        None,
    ));
}

#[test]
fn multi_valued_rdn_in_descending_order_is_rejected() {
    // 上と同じ 2 要素を逆順（降順）に並べた非正規 BER（PR #1036
    // codex-review P1 指摘: SET OF の DER 順序を検査していなかった）。
    let cn = attribute_type_and_value(0x03, "a");
    let org = attribute_type_and_value(0x0a, "a");
    let reversed = sequence(&[&tlv(0x31, &[org.as_slice(), &cn].concat())]);
    assert_leaf_rejected_as_malformed(build_ed25519_leaf_with_names_and_extensions(
        &reversed,
        &empty_name(),
        None,
    ));
    assert_leaf_rejected_as_malformed(build_ed25519_leaf_with_names_and_extensions(
        &issuer_name(),
        &reversed,
        None,
    ));

    // 値の長さが異なる場合も符号化バイト列（長さオクテットを含む）で比較する:
    // 長さ 0x0d の要素は長さ 0x08 の要素より後ろでなければならない。
    let short = attribute_type_and_value(0x0a, "a");
    let long = attribute_type_and_value(0x03, "abcdef");
    let ascending = sequence(&[&tlv(0x31, &[short.as_slice(), &long].concat())]);
    assert_leaf_accepted(build_ed25519_leaf_with_names_and_extensions(
        &ascending,
        &empty_name(),
        None,
    ));
    let descending = sequence(&[&tlv(0x31, &[long.as_slice(), &short].concat())]);
    assert_leaf_rejected_as_malformed(build_ed25519_leaf_with_names_and_extensions(
        &descending,
        &empty_name(),
        None,
    ));
}

// X25519（1.3.101.110）。中間証明書の SPKI に置く（中間は SPKI アルゴリズムを
// 制限しないため、葉限定の Ed25519 検査に依存せず BIT STRING 形状検査だけを
// 観測できる）。
const OID_X25519_BYTES: [u8; 3] = [0x2b, 0x65, 0x6e];

/// テスト専用: SPKI の subjectPublicKey BIT STRING の値部分（未使用ビット数
/// オクテット込み）を差し替えた X25519 中間証明書の DER を手組みする。
fn build_x25519_intermediate_with_spki_bit_string(spki_bit_string_value: &[u8]) -> Vec<u8> {
    let signature_algorithm = ed25519_algorithm_identifier();
    let spki = sequence(&[
        &sequence(&[&tlv(0x06, &OID_X25519_BYTES)]),
        &tlv(0x03, spki_bit_string_value),
    ]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, &[0x02]),
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

fn leaf_and_intermediate_result(
    intermediate: Vec<u8>,
) -> Result<ServerCertificateChain, CertificateChainError> {
    let leaf = build_ed25519_leaf_certificate_der(
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        "160801121924Z",
        "401231235959Z",
    );
    ServerCertificateChain::from_der_chain(
        vec![leaf, intermediate],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
}

#[test]
fn intermediate_spki_bit_string_with_invalid_shape_is_rejected() {
    // 中間証明書（index 1）の SPKI BIT STRING の形状違反（PR #1036
    // codex-review P1 指摘: 従来は葉の check_leaf_public_key だけが検査して
    // いたため、中間ではこれらが from_der_chain を通過していた）。DER 形状の
    // 違反は構造検証（全階層の BIT STRING 正規形検査）で Malformed、DER と
    // しては正当だが鍵の内容が 0 バイトのものは InvalidPublicKey になる。
    let cases: [(&str, Vec<u8>, X509Error); 5] = [
        ("empty value", Vec::new(), X509Error::Malformed),
        (
            "unused bits octet only",
            vec![0x00],
            X509Error::InvalidPublicKey,
        ),
        (
            "unused bits 8",
            [&[0x08u8][..], &[0u8; 32]].concat(),
            X509Error::Malformed,
        ),
        (
            "unused bits 3 with nonzero padding",
            vec![0x03, 0xaa, 0xf9],
            X509Error::Malformed,
        ),
        (
            "unused bits without content",
            vec![0x05],
            X509Error::Malformed,
        ),
    ];
    for (label, value, expected) in cases {
        let err =
            leaf_and_intermediate_result(build_x25519_intermediate_with_spki_bit_string(&value))
                .expect_err(label);
        assert_eq!(
            err,
            CertificateChainError::Certificate {
                index: 1,
                error: expected
            },
            "{label}"
        );
    }
}

#[test]
fn intermediate_spki_bit_string_with_valid_shape_is_accepted() {
    // 形状が正しい BIT STRING は、未使用ビット数 0（32 バイト鍵）でも、
    // 未使用ビット数が非 0 で 0 埋めされていても中間では受理される
    // （中間の SPKI アルゴリズム・鍵長は制限しない）。
    let mut unused_zero = vec![0x00u8];
    unused_zero.extend_from_slice(&[0x5a; 32]);
    let unused_three_zero_padded = vec![0x03, 0xaa, 0xf8];
    for value in [unused_zero, unused_three_zero_padded] {
        let chain =
            leaf_and_intermediate_result(build_x25519_intermediate_with_spki_bit_string(&value))
                .expect("intermediate with well-formed SPKI BIT STRING must be accepted");
        assert_eq!(chain.leaf_public_key(), &RFC8410_10_1_ED25519_PUBLIC_KEY);
    }
}

#[test]
fn leaf_spki_bit_string_with_invalid_shape_is_rejected_as_invalid_public_key() {
    // 葉でも中間と同じ共通検査を通る。DER 形状の違反は Malformed、DER として
    // 正当だが内容 0 バイトの鍵は InvalidPublicKey。
    for (value, expected) in [
        (vec![0x08u8, 0x00], X509Error::Malformed),
        (vec![0x00], X509Error::InvalidPublicKey),
        (Vec::new(), X509Error::Malformed),
    ] {
        let signature_algorithm = ed25519_algorithm_identifier();
        let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &value)]);
        let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
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
        let der = sequence(&[
            &tbs_certificate,
            &signature_algorithm,
            &tlv(0x03, &signature_bits),
        ]);
        let err = ServerCertificateChain::from_der_chain(
            vec![der],
            &RFC8410_10_1_ED25519_PUBLIC_KEY,
            NOW_WITHIN_RFC8410_10_2_VALIDITY,
        )
        .unwrap_err();
        assert_eq!(
            err,
            CertificateChainError::Certificate {
                index: 0,
                error: expected
            },
            "{value:02x?}"
        );
    }
}

#[test]
fn empty_issuer_name_is_rejected_and_empty_subject_is_accepted() {
    // RFC 5280 §4.1.2.4: issuer は空でない DN を含まなければならない
    // （PR #1036 codex-review P1 指摘: 従来は RDN 0 個の issuer を受理していた）。
    assert_leaf_rejected_as_malformed(build_ed25519_leaf_with_names_and_extensions(
        &empty_name(),
        &empty_name(),
        None,
    ));
    // 中間証明書（index 1）でも同じく拒否される。
    let intermediate_with_empty_issuer = {
        let signature_algorithm = ed25519_algorithm_identifier();
        let mut spki_bits = vec![0x00u8];
        spki_bits.extend_from_slice(&[0x5a; 32]);
        let spki = sequence(&[
            &sequence(&[&tlv(0x06, &OID_X25519_BYTES)]),
            &tlv(0x03, &spki_bits),
        ]);
        let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
        let tbs_certificate = sequence(&[
            &version_v3(),
            &tlv(0x02, &[0x02]),
            &signature_algorithm,
            &empty_name(), // issuer（空）
            &validity,
            &issuer_name(), // subject
            &spki,
        ]);
        let mut signature_bits = vec![0x00u8];
        signature_bits.extend_from_slice(&[0u8; 64]);
        sequence(&[
            &tbs_certificate,
            &signature_algorithm,
            &tlv(0x03, &signature_bits),
        ])
    };
    let err = leaf_and_intermediate_result(intermediate_with_empty_issuer).unwrap_err();
    assert_eq!(
        err,
        CertificateChainError::Certificate {
            index: 1,
            error: X509Error::Malformed
        }
    );
    // subject は SAN のみで主体を表す場合に空を許される（RFC 5280 §4.1.2.6）
    // ため、空の subject と空でない issuer の組は受理する。
    assert_leaf_accepted(build_ed25519_leaf_with_names_and_extensions(
        &issuer_name(),
        &empty_name(),
        None,
    ));
}

#[test]
fn duplicate_extension_ids_are_rejected() {
    // RFC 5280 §4.2: 同一 extnID の拡張を複数含めてはならない。critical の
    // 有無・extnValue が異なっても extnID が同じなら重複として拒否する。
    let first = [OID_KEY_USAGE_TLV.as_slice(), &octet_string_extn_value()].concat();
    let second = [
        OID_KEY_USAGE_TLV.as_slice(),
        &CRITICAL_TRUE_TLV,
        &tlv(0x04, &[0x05, 0x00]),
    ]
    .concat();
    let other = [
        OID_BASIC_CONSTRAINTS_TLV.as_slice(),
        &tlv(0x04, &sequence(&[])),
    ]
    .concat();
    assert_leaf_rejected_as_malformed(build_ed25519_leaf_with_names_and_extensions(
        &issuer_name(),
        &empty_name(),
        Some(&extensions_field_from_bodies(&[&first, &other, &second])),
    ));
    // 異なる extnID の組は受理する。
    assert_leaf_accepted(build_ed25519_leaf_with_names_and_extensions(
        &issuer_name(),
        &empty_name(),
        Some(&extensions_field_from_bodies(&[&first, &other])),
    ));
}

#[test]
fn constructed_only_universal_type_in_name_value_follows_x690() {
    // AttributeTypeAndValue の value（ANY）に EXTERNAL（tag 8）を置く。
    // X.690 上 EXTERNAL は constructed 必須のため、constructed（0x28）は
    // 受理し primitive（0x08）は拒否する（PR #1036 codex-review P2 指摘）。
    let name_with_value = |value: &[u8]| {
        let atv = sequence(&[&tlv(0x06, &[0x55, 0x04, 0x03]), value]);
        sequence(&[&tlv(0x31, &atv)])
    };
    let constructed_external = tlv(0x28, &tlv(0x02, &[0x01]));
    assert_leaf_accepted(build_ed25519_leaf_with_names_and_extensions(
        &issuer_name(),
        &name_with_value(&constructed_external),
        None,
    ));
    let primitive_external = tlv(0x08, &[0x02, 0x01, 0x01]);
    assert_leaf_rejected_as_malformed(build_ed25519_leaf_with_names_and_extensions(
        &issuer_name(),
        &name_with_value(&primitive_external),
        None,
    ));
    // 文字列型の constructed（BER の断片化形）は引き続き拒否する。
    let constructed_utf8 = tlv(0x2c, &tlv(0x0c, b"a"));
    assert_leaf_rejected_as_malformed(build_ed25519_leaf_with_names_and_extensions(
        &issuer_name(),
        &name_with_value(&constructed_utf8),
        None,
    ));
}

#[test]
fn non_canonical_integer_in_algorithm_parameters_is_rejected() {
    // AlgorithmIdentifier の parameters（ANY）内の INTEGER も DER の最小
    // 符号化を満たさなければならない（意味を解釈しない値の内側にも DER
    // 制約を及ぼす）。ここでは外側・tbs 両方の signature に同じ値を置く。
    let with_params =
        |integer: &[u8]| sequence(&[&tlv(0x06, &OID_ED25519_BYTES), &tlv(0x02, integer)]);
    let build = |algorithm: &[u8]| {
        let mut spki_bits = vec![0x00u8];
        spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
        let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
        let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
        let tbs_certificate = sequence(&[
            &version_v3(),
            &tlv(0x02, &[0x01]),
            algorithm,
            &issuer_name(),
            &validity,
            &empty_name(),
            &spki,
        ]);
        let mut signature_bits = vec![0x00u8];
        signature_bits.extend_from_slice(&[0u8; 64]);
        sequence(&[&tbs_certificate, algorithm, &tlv(0x03, &signature_bits)])
    };
    assert_leaf_accepted(build(&with_params(&[0x00, 0x80])));
    assert_leaf_rejected_as_malformed(build(&with_params(&[0x00, 0x7f])));
    assert_leaf_rejected_as_malformed(build(&with_params(&[])));
}

/// テスト専用: tbsCertificate.signature と外側 signatureAlgorithm の双方に
/// 同じ AlgorithmIdentifier を置いた Ed25519 葉証明書の DER を手組みする。
fn build_leaf_with_signature_algorithm(algorithm: &[u8]) -> Vec<u8> {
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &utc_time("401231235959Z")]);
    let tbs_certificate = sequence(&[
        &version_v3(),
        &tlv(0x02, &[0x01]),
        algorithm,
        &issuer_name(),
        &validity,
        &empty_name(),
        &spki,
    ]);
    let mut signature_bits = vec![0x00u8];
    signature_bits.extend_from_slice(&[0u8; 64]);
    sequence(&[&tbs_certificate, algorithm, &tlv(0x03, &signature_bits)])
}

#[test]
fn non_canonical_bit_string_in_algorithm_parameters_is_rejected() {
    // AlgorithmIdentifier の parameters（ANY。意味は解釈しない）に置いた
    // BIT STRING も DER の正規形で検査する（PR #1036 codex-review P1 指摘:
    // `03 02 08 00`〔未使用ビット数 8〕が受理されていた）。
    let with_params = |parameters: &[u8]| sequence(&[&tlv(0x06, &OID_ED25519_BYTES), parameters]);
    for rejected in [
        tlv(0x03, &[0x08, 0x00]),
        tlv(0x03, &[0x03, 0xf9]),
        tlv(0x03, &[0x01]),
        tlv(0x03, &[]),
    ] {
        assert_leaf_rejected_as_malformed(build_leaf_with_signature_algorithm(&with_params(
            &rejected,
        )));
    }
    for accepted in [
        tlv(0x03, &[0x00]),
        tlv(0x03, &[0x03, 0xf8]),
        tlv(0x04, &[0x08, 0x00]),
    ] {
        assert_leaf_accepted(build_leaf_with_signature_algorithm(&with_params(&accepted)));
    }
}

#[test]
fn name_attribute_values_follow_der_primitive_constraints() {
    // Name の属性値（ANY）に置いた文字列型・時刻型も、フィールドの意味を
    // 解釈しないまま DER の正規形・文字集合で検査する。
    let subject_with_value = |value: &[u8]| {
        let atv = sequence(&[&tlv(0x06, &[0x55, 0x04, 0x03]), value]);
        sequence(&[&tlv(0x31, &atv)])
    };
    let build = |value: &[u8]| {
        build_ed25519_leaf_with_names_and_extensions(
            &issuer_name(),
            &subject_with_value(value),
            None,
        )
    };
    for accepted in [
        tlv(0x0c, "caf\u{e9}".as_bytes()),
        tlv(0x13, b"Example Org."),
        tlv(0x16, b"user@example.com"),
        tlv(0x1e, &[0x00, 0x41]),
        tlv(0x18, b"20501231235959.5Z"),
    ] {
        assert_leaf_accepted(build(&accepted));
    }
    for rejected in [
        tlv(0x0c, &[0x63, 0xc3]),
        tlv(0x13, b"a*b"),
        tlv(0x16, &[0x80]),
        tlv(0x1e, &[0x41]),
        tlv(0x09, &[]),
    ] {
        assert_leaf_rejected_as_malformed(build(&rejected));
    }
    // 時刻型の DER 違反は validity と同じ InvalidTime に写像される。
    let err = ServerCertificateChain::from_der_chain(
        vec![build(&tlv(0x18, b"20501231235959.50Z"))],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    assert_eq!(
        err,
        CertificateChainError::Certificate {
            index: 0,
            error: X509Error::InvalidTime
        }
    );
}

#[test]
fn validity_with_der_valid_fractional_generalized_time_is_rejected() {
    // DER としては正当な小数秒付き GeneralizedTime でも、RFC 5280 §4.1.2.5.2
    // が validity に小数秒を禁じるため InvalidTime として拒否する。
    let signature_algorithm = ed25519_algorithm_identifier();
    let mut spki_bits = vec![0x00u8];
    spki_bits.extend_from_slice(&RFC8410_10_1_ED25519_PUBLIC_KEY);
    let spki = sequence(&[&ed25519_algorithm_identifier(), &tlv(0x03, &spki_bits)]);
    let validity = sequence(&[&utc_time("160801121924Z"), &tlv(0x18, b"20501231235959.5Z")]);
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
    let der = sequence(&[
        &tbs_certificate,
        &signature_algorithm,
        &tlv(0x03, &signature_bits),
    ]);
    let err = ServerCertificateChain::from_der_chain(
        vec![der],
        &RFC8410_10_1_ED25519_PUBLIC_KEY,
        NOW_WITHIN_RFC8410_10_2_VALIDITY,
    )
    .unwrap_err();
    assert_eq!(
        err,
        CertificateChainError::Certificate {
            index: 0,
            error: X509Error::InvalidTime
        }
    );
}

#[test]
fn current_unix_secs_returns_a_plausible_recent_value() {
    // このリポジトリが書かれた時点（2020 年以降）より新しい値であることの
    // ゆるい健全性チェック。厳密な現在時刻検証はできないため、明らかに
    // 過去・エラーでないことのみ確認する。
    let now = x509::current_unix_secs().expect("clock must be readable");
    assert!(now > 1_600_000_000);
}
