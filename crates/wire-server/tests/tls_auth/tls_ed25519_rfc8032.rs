//! Ed25519 署名生成・検証（`tls::ed25519`）・`CertificateVerify`
//! （`tls::certificate_verify`。TASK-228・WIRE-9・HTTP-10 ポインタ。
//! Issue #961・親 #941）の公開 API だけを使う結合テスト。単体テスト
//! （`tls::ed25519::tests`・`tls::certificate_verify::tests`）とは別に、
//! 外部から見える契約（RFC 8032 §7.1 の公開テストベクタ・RFC 8410 の
//! 鍵読み込みからの導出・`CertificateVerify` の署名対象構造）が公開
//! ベクタと一致すること、および fail-closed な拒否を固定する。

use wire_server::tls::certificate_verify::{
    self, CertificateVerifyError, SERVER_CONTEXT, SIGNED_CONTENT_LEN,
};
use wire_server::tls::client_hello::SIG_ED25519;
use wire_server::tls::ed25519::{self, Ed25519Error, SigningKey};
use wire_server::tls::handshake::CertificateVerify;
use wire_server::tls::pkcs8;

fn hex32(s: &str) -> [u8; 32] {
    assert_eq!(s.len(), 64);
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("valid hex in fixture");
    }
    out
}

fn hex_vec(s: &str) -> Vec<u8> {
    assert!(s.len().is_multiple_of(2));
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex in fixture"))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct Vector {
    sk: &'static str,
    pk: &'static str,
    msg: &'static str,
    sig: &'static str,
}

// RFC 8032 §7.1 の公開テストベクタ（IETF 公開文書）。
const TEST_1: Vector = Vector {
    sk: "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60",
    pk: "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a",
    msg: "",
    sig: "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
};
const TEST_2: Vector = Vector {
    sk: "4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb",
    pk: "3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c",
    msg: "72",
    sig: "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
};
const TEST_3: Vector = Vector {
    sk: "c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7",
    pk: "fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025",
    msg: "af82",
    sig: "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
};
// メッセージは SHA-512("abc")（RFC 8032 §7.1 TEST SHA(abc)）。
const TEST_SHA_ABC: Vector = Vector {
    sk: "833fe62409237b9d62ec77587520911e9a759cec1d19755b7da901b96dca3d42",
    pk: "ec172b93ad5e563bf4932c70e1245034c35467ef2efd4d64ebf819683467e2bf",
    msg: "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
    sig: "dc2a4459e7369633a52b1bf277839a00201009a3efbf3ecb69bea2186c26b58909351fc9ac90b3ecfdfbc7c66431e0303dca179c138ac17ad9bef1177331a704",
};

fn check_vector(v: &Vector) {
    let key = SigningKey::from_seed_bytes(hex32(v.sk));
    assert_eq!(hex(&key.public_key()), v.pk, "public key mismatch");

    let msg = hex_vec(v.msg);
    let sig = key.sign(&msg);
    assert_eq!(hex(&sig), v.sig, "signature mismatch");

    assert!(ed25519::verify(&key.public_key(), &msg, &sig).is_ok());
}

#[test]
fn rfc8032_test_vector_1_empty_message() {
    check_vector(&TEST_1);
}

#[test]
fn rfc8032_test_vector_2() {
    check_vector(&TEST_2);
}

#[test]
fn rfc8032_test_vector_3() {
    check_vector(&TEST_3);
}

#[test]
fn rfc8032_test_vector_sha_abc() {
    check_vector(&TEST_SHA_ABC);
}

#[test]
fn signing_is_deterministic_across_independent_key_expansions() {
    let key1 = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
    let key2 = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
    let msg = b"deterministic signing";
    assert_eq!(key1.sign(msg), key2.sign(msg));
}

#[test]
fn negative_signature_mutations_are_rejected() {
    let key = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
    let msg = hex_vec(TEST_1.msg);
    let good_sig = key.sign(&msg);

    // R の 1 ビット反転。
    let mut sig = good_sig;
    sig[0] ^= 0x01;
    assert!(ed25519::verify(&key.public_key(), &msg, &sig).is_err());

    // S の 1 ビット反転。
    let mut sig = good_sig;
    sig[63] ^= 0x01;
    assert!(ed25519::verify(&key.public_key(), &msg, &sig).is_err());

    // メッセージの 1 バイト改変。
    let mut bad_msg = msg.clone();
    bad_msg.push(0x00);
    bad_msg[0] ^= 0x01;
    assert!(ed25519::verify(&key.public_key(), &bad_msg, &good_sig).is_err());

    // 別の公開鍵での検証。
    let other_key = SigningKey::from_seed_bytes(hex32(TEST_2.sk));
    assert!(ed25519::verify(&other_key.public_key(), &msg, &good_sig).is_err());

    // 長さ 0・63・65 の署名。
    assert_eq!(
        ed25519::verify(&key.public_key(), &msg, &[]),
        Err(Ed25519Error::InvalidSignatureLength)
    );
    assert_eq!(
        ed25519::verify(&key.public_key(), &msg, &good_sig[..63]),
        Err(Ed25519Error::InvalidSignatureLength)
    );
    let mut too_long = good_sig.to_vec();
    too_long.push(0);
    assert_eq!(
        ed25519::verify(&key.public_key(), &msg, &too_long),
        Err(Ed25519Error::InvalidSignatureLength)
    );

    // S ≥ L（S を L 自身へ書き換える）。
    let l_bytes = hex32("edd3f55c1a631258d69cf7a2def9de1400000000000000000000000000000010");
    let mut sig = good_sig;
    sig[32..].copy_from_slice(&l_bytes);
    assert_eq!(
        ed25519::verify(&key.public_key(), &msg, &sig),
        Err(Ed25519Error::NonCanonicalScalar)
    );

    // R が非正準（p のエンコーディング）。
    let p_bytes = hex32("edffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f");
    let mut sig = good_sig;
    sig[..32].copy_from_slice(&p_bytes);
    assert_eq!(
        ed25519::verify(&key.public_key(), &msg, &sig),
        Err(Ed25519Error::InvalidSignaturePoint)
    );

    // 公開鍵が非正準（p のエンコーディング）。
    assert!(ed25519::verify(&p_bytes, &msg, &good_sig).is_err());

    // 公開鍵に平方根が存在しない（y=2）。
    let mut bad_pk = [0u8; 32];
    bad_pk[0] = 2;
    assert!(ed25519::verify(&bad_pk, &msg, &good_sig).is_err());

    // x=0 の点に符号ビット 1 を立てた非正準表現（y=1・sign=1）。
    let mut bad_pk = [0u8; 32];
    bad_pk[0] = 1;
    bad_pk[31] = 0x80;
    assert!(ed25519::verify(&bad_pk, &msg, &good_sig).is_err());
}

// RFC 8410 §10.3 v1 の Ed25519 秘密鍵 seed（`tests/tls_pem_pkcs8.rs` の
// `ED25519_V1_SEED_HEX` と同一値。転記ミス防止のため、この値からの導出が
// `tests/tls_x509.rs` の `RFC8410_10_1_ED25519_PUBLIC_KEY`（RFC 8410 §10.1
// の公開鍵）と一致することを確認する）。
const RFC8410_10_3_V1_SEED_HEX: &str =
    "d4ee72dbf913584ad5b6d8f1f769f8ad3afe7c28cbf1d4fbe097a88f44755842";
const RFC8410_10_1_ED25519_PUBLIC_KEY: [u8; 32] = [
    0x19, 0xbf, 0x44, 0x09, 0x69, 0x84, 0xcd, 0xfe, 0x85, 0x41, 0xba, 0xc1, 0x67, 0xdc, 0x3b, 0x96,
    0xc8, 0x50, 0x86, 0xaa, 0x30, 0xb6, 0xb6, 0xcb, 0x0c, 0x5c, 0x38, 0xad, 0x70, 0x31, 0x66, 0xe1,
];

/// PEM（RFC 8410 §10.3）→ PKCS#8 デコード（`tls::pkcs8`。Issue #962）→
/// 鍵展開・公開鍵導出（`tls::ed25519`。Issue #961）の起動経路（#967 が
/// 結線する接続点そのもの）が RFC 8410 §10.1 の公開ベクタと一致することを
/// 確認する。
#[test]
fn rfc8410_pem_seed_derives_expected_public_key() {
    let pem = "-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEINTuctv5E1hK1bbY8fdp+K06/nwoy/HU++CXqI9EdVhC\n-----END PRIVATE KEY-----\n";
    let seed = pkcs8::decode_ed25519_private_key_pem(pem.as_bytes())
        .expect("RFC 8410 §10.3 v1 PEM must decode");
    assert_eq!(seed.as_bytes(), &hex32(RFC8410_10_3_V1_SEED_HEX));

    let key = SigningKey::from_seed(&seed);
    assert_eq!(key.public_key(), RFC8410_10_1_ED25519_PUBLIC_KEY);
}

fn transcript_hash_for_test(byte: u8) -> [u8; 32] {
    [byte; 32]
}

#[test]
fn certificate_verify_signed_content_layout_is_fixed() {
    let hash = transcript_hash_for_test(0x11);
    let content = certificate_verify::server_signed_content(&hash);
    assert_eq!(content.len(), SIGNED_CONTENT_LEN);
    assert_eq!(content.len(), 130);
    assert!(content[..64].iter().all(|&b| b == 0x20));
    assert_eq!(&content[64..64 + SERVER_CONTEXT.len()], SERVER_CONTEXT);
    assert_eq!(content[64 + SERVER_CONTEXT.len()], 0x00);
    assert_eq!(&content[64 + SERVER_CONTEXT.len() + 1..], &hash);
}

#[test]
fn certificate_verify_round_trips_and_matches_direct_sign() {
    let key = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
    let hash = transcript_hash_for_test(0x22);

    let cv = certificate_verify::build_server_certificate_verify(&key, &hash);
    assert_eq!(cv.algorithm, SIG_ED25519);
    assert_eq!(cv.signature.len(), 64);

    let content = certificate_verify::server_signed_content(&hash);
    assert_eq!(cv.signature, key.sign(&content).to_vec());

    assert!(
        certificate_verify::verify_server_certificate_verify(&key.public_key(), &hash, &cv).is_ok()
    );
}

#[test]
fn certificate_verify_rejects_hash_mutation_algorithm_mismatch_and_bad_length() {
    let key = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
    let hash = transcript_hash_for_test(0x33);
    let cv = certificate_verify::build_server_certificate_verify(&key, &hash);

    // transcript hash の 1 ビット改変。
    let mut wrong_hash = hash;
    wrong_hash[0] ^= 0x01;
    assert!(certificate_verify::verify_server_certificate_verify(
        &key.public_key(),
        &wrong_hash,
        &cv
    )
    .is_err());

    // algorithm の書き換え（rsa_pss_rsae_sha256 = 0x0804）。
    let mut wrong_alg = cv.clone();
    wrong_alg.algorithm = 0x0804;
    assert_eq!(
        certificate_verify::verify_server_certificate_verify(&key.public_key(), &hash, &wrong_alg),
        Err(CertificateVerifyError::UnsupportedAlgorithm)
    );

    // 署名長の不正（65 バイトへ水増し）。
    let mut wrong_len = cv;
    wrong_len.signature.push(0);
    assert_eq!(
        certificate_verify::verify_server_certificate_verify(&key.public_key(), &hash, &wrong_len),
        Err(CertificateVerifyError::InvalidSignatureLength)
    );
}

#[test]
fn signing_key_debug_does_not_expose_secret_bytes() {
    let key = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
    let debug = format!("{key:?}");
    assert!(!debug.contains(TEST_1.sk));
    assert!(debug.contains("redacted"));
}

// `CertificateVerify::parse`／`encode_into`（`tls::handshake`。Issue #953）
// との往復が #961 の生成物に対しても成立することを確認する（層をまたいだ
// 契約の整合性）。
#[test]
fn certificate_verify_message_round_trips_through_handshake_wire_format() {
    let key = SigningKey::from_seed_bytes(hex32(TEST_1.sk));
    let hash = transcript_hash_for_test(0x44);
    let cv = certificate_verify::build_server_certificate_verify(&key, &hash);

    let mut wire = Vec::new();
    cv.encode_into(&mut wire).expect("encode CertificateVerify");

    // handshake ヘッダ（type 1 byte + u24 length）を取り除いて body を得る。
    let body = &wire[4..];
    let parsed = CertificateVerify::parse(body).expect("parse CertificateVerify body");
    assert_eq!(parsed, cv);
    assert!(certificate_verify::verify_server_certificate_verify(
        &key.public_key(),
        &hash,
        &parsed
    )
    .is_ok());
}
