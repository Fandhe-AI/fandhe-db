//! RFC 8448 §3（Simple 1-RTT Handshake）のトレースを、`wire_server::tls`
//! （HKDF・鍵スケジュール。Issue #956・TASK-228・WIRE-9・HTTP-10 ポインタ）の
//! **公開 API のみ**を通しで使って再現する結合テスト。
//!
//! `crates/wire-server/src/tls/{hkdf.rs,key_schedule.rs}` の単体テストは
//! 各段を個別に検証するが、本テストは
//! X25519 鍵交換 → Early secret → Handshake secret → traffic secret/key/iv →
//! Master secret → application traffic secret/key/iv という利用者視点の
//! 一連の流れを 1 本の呼び出し列として固定する。あわせて、
//! ClientHello‖ServerHello（RFC 8448 記載バイト列）の SHA-256 が
//! トレース記載の transcript hash と一致することも確認し、
//! `engine::crypto::sha256` の公開 API がブロック境界をまたぐ実データでも
//! 正しく動作することを固定する。

use wire_server::tls::hkdf::HASH_LEN;
use wire_server::tls::key_schedule::EarlySecret;
use wire_server::tls::x25519::EphemeralSecret;

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex in RFC fixture"))
        .collect()
}

fn hex_decode32(s: &str) -> [u8; 32] {
    let v = hex_decode(s);
    v.try_into().expect("32 bytes")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn rfc8448_simple_1rtt_handshake_key_schedule_matches_trace() {
    // {client} create an ephemeral x25519 key pair
    let client_priv =
        hex_decode32("49af42ba7f7994852d713ef2784bcbcaa7911de26adc5642cb634540e7ea5005");
    // {server} create an ephemeral x25519 key pair
    let server_priv =
        hex_decode32("b1580eeadf6dd589b8ef4f2d5652578cc810e9980191ec8d058308cea216a21e");

    let server_secret = EphemeralSecret::from_bytes(server_priv);
    let client_secret = EphemeralSecret::from_bytes(client_priv);
    let client_pub = *client_secret.public_key().as_bytes();

    // {server} extract secret "early" と ECDHE 共有秘密の算出。
    let shared = server_secret
        .diffie_hellman(&client_pub)
        .expect("valid non-zero shared secret (RFC 8448 fixture)");
    assert_eq!(
        hex(shared.as_bytes()),
        "8bd4054fb55b9d63fdfbacf9f04b9f0d35e6d63f537563efd46272900f89492d"
    );

    // Early → Handshake secret。
    let early = EarlySecret::new_without_psk();
    let handshake = early
        .into_handshake(&shared)
        .expect("valid HKDF parameters");

    // ClientHello‖ServerHello（RFC 8448 記載バイト列）の transcript hash。
    let client_hello = hex_decode(
        "010000c00303cb34ecb1e78163ba1c38c6dacb196a6dffa21a8d9912ec18a2ef6283024dece7000006\
         130113031302010000910000000b0009000006736572766572ff01000100000a0014001200\
         1d0017001800190100010101020103010400230000003300260024001d002099381de560e4\
         bd43d23d8e435a7dbafeb3c06e51c13cae4d5413691e529aaf2c002b0003020304000d0020\
         001e040305030603020308040805080604010501060102010402050206020202002d000201\
         01001c00024001",
    );
    let server_hello = hex_decode(
        "020000560303a6af06a4121860dc5e6e60249cd34c95930c8ac5cb1434dac155772ed3e26928\
         00130100002e00330024001d0020c9828876112095fe66762bdbf7c672e156d6cc253b833df\
         1dd69b1b04e751f0f002b00020304",
    );
    let mut transcript = client_hello;
    transcript.extend_from_slice(&server_hello);
    let th_ch_sh = engine::crypto::sha256::digest(&transcript);
    assert_eq!(
        hex(&th_ch_sh),
        "860c06edc07858ee8e78f0e7428c58edd6b43f2ca3e6e95f02ed063cf0e1cad8"
    );

    let traffic = handshake
        .traffic_secrets(&th_ch_sh)
        .expect("valid HKDF parameters");
    let server_hs_keys = traffic
        .server
        .traffic_keys()
        .expect("valid HKDF parameters");
    assert_eq!(
        hex(server_hs_keys.key()),
        "3fce516009c21727d0f2e4e86ee403bc"
    );
    assert_eq!(hex(server_hs_keys.iv()), "5d313eb2671276ee13000b30");

    let client_hs_keys = traffic
        .client
        .traffic_keys()
        .expect("valid HKDF parameters");
    assert_eq!(
        hex(client_hs_keys.key()),
        "dbfaa693d1762c5b666af5d950258d01"
    );
    assert_eq!(hex(client_hs_keys.iv()), "5bd3c71b836e0b76bb73265f");

    // Handshake → Master secret → application traffic secret/key/iv。
    let master = handshake.into_master().expect("valid HKDF parameters");
    let th_ch_sf = hex_decode32("9608102a0f1ccc6db6250b7b7e417b1a000eaada3daae4777a7686c9ff83df13");
    let app = master
        .application_traffic_secrets(&th_ch_sf)
        .expect("valid HKDF parameters");

    let server_ap_keys = app.server.traffic_keys().expect("valid HKDF parameters");
    assert_eq!(
        hex(server_ap_keys.key()),
        "9f02283b6c9c07efc26bb9f2ac92e356"
    );
    assert_eq!(hex(server_ap_keys.iv()), "cf782b88dd83549aadf1e984");

    // HASH_LEN が SHA-256 の 32 バイトで固定されていることも合わせて固定する
    // （鍵スケジュール全体が SHA-256 決め打ちであるという設計の外形テスト）。
    assert_eq!(HASH_LEN, 32);
}
