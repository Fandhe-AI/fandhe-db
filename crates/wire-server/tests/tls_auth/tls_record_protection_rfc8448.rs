//! RFC 8448 §3（Simple 1-RTT Handshake）のトレースを、`wire_server::tls`
//! （レコード保護層。Issue #959・TASK-228・WIRE-9・HTTP-10 ポインタ）の
//! **公開 API のみ**を通しで使って再現する結合テスト。
//!
//! `crates/wire-server/src/tls/record_protection.rs` の単体テストは
//! 決定的なダミー鍵で各契約（nonce・パディング・エラー分類・上限）を
//! 個別に検証するが、本テストは
//! X25519 鍵交換 → 鍵スケジュール（`tls::key_schedule`） →
//! client handshake traffic key/iv → `Opener::install_handshake_keys` →
//! 実際のワイヤ上のレコード（[`record::read_record`]）を `Opener::open`
//! で復号、という利用者視点の一連の流れを 1 本の呼び出し列として固定する。
//!
//! `crates/wire-server/src/tls/record.rs` の単体テストが既に固定している
//! RFC 8448 の client Finished 暗号化レコード（58 octets）を用いる。この
//! レコードの鍵・iv は `tls_key_schedule_rfc8448.rs` が固定した client
//! handshake traffic key/iv（`dbfaa693…`／`5bd3c71b…`）と同じ導出経路で
//! 得られる。verify_data（Finished メッセージの中身）そのものは
//! `docs/spec`（private）の内容ではなく単に本テストでは決め打ちしないが、
//! 復号結果が RFC 8446 §4.4.4 の Finished メッセージ構造
//! （`type(1) || length(3) || verify_data(32)` = 36 オクテット、
//! `type == 0x14`）と一致することを確認することで、AEAD タグ検証・
//! 復号・`TLSInnerPlaintext` の内容型復元がすべて正しく機能していることを
//! 固定する。

use std::io::Cursor;
use wire_server::tls::key_schedule::EarlySecret;
use wire_server::tls::record::{self, ContentType};
use wire_server::tls::record_protection::{Opener, ProtectionError};
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

/// RFC 8448 §3 のクライアント側 Finished を運ぶ暗号化レコード（58 octets）。
/// `crates/wire-server/src/tls/record.rs` のテストが固定している値と同一
/// （IETF の公開文書 RFC 8448 由来。`docs/spec` の内容ではない）。
fn rfc8448_client_finished_record_bytes() -> Vec<u8> {
    hex_decode(
        "170303003575ec4dc238cce60b298044a71e219c56cc77b0517fe9b93c7a4bfc44d87f38f80338ac98fc46\
         deb384bd1caeacab6867d726c40546",
    )
}

/// RFC 8448 §3 の ECDHE 入力から client handshake traffic key/iv を導出する
/// （`tls_key_schedule_rfc8448.rs` と同じ手順）。
fn client_handshake_traffic_key_iv() -> wire_server::tls::key_schedule::TrafficKeys {
    let server_priv =
        hex_decode32("b1580eeadf6dd589b8ef4f2d5652578cc810e9980191ec8d058308cea216a21e");
    let client_priv =
        hex_decode32("49af42ba7f7994852d713ef2784bcbcaa7911de26adc5642cb634540e7ea5005");
    let server_secret = EphemeralSecret::from_bytes(server_priv);
    let client_secret = EphemeralSecret::from_bytes(client_priv);
    let client_pub = *client_secret.public_key().as_bytes();
    let shared = server_secret
        .diffie_hellman(&client_pub)
        .expect("valid non-zero shared secret (RFC 8448 fixture)");

    let early = EarlySecret::new_without_psk();
    let handshake = early
        .into_handshake(&shared)
        .expect("valid HKDF parameters");

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

    let traffic = handshake
        .traffic_secrets(&th_ch_sh)
        .expect("valid HKDF parameters");
    traffic
        .client
        .traffic_keys()
        .expect("valid HKDF parameters")
}

#[test]
fn opener_decrypts_rfc8448_client_finished_record_via_public_api() {
    let keys = client_handshake_traffic_key_iv();
    // 導出した鍵が RFC 8448 トレース記載値と一致することを固定する
    // （`tls_key_schedule_rfc8448.rs` の固定値と同一の再確認）。
    let key_hex: String = keys.key().iter().map(|b| format!("{b:02x}")).collect();
    let iv_hex: String = keys.iv().iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(key_hex, "dbfaa693d1762c5b666af5d950258d01");
    assert_eq!(iv_hex, "5bd3c71b836e0b76bb73265f");

    let raw = rfc8448_client_finished_record_bytes();
    assert_eq!(raw.len(), 58);

    let mut opener = Opener::new();
    opener
        .install_handshake_keys(&keys)
        .expect("Plaintext -> Handshake transition must succeed");
    assert_eq!(opener.record_kind(), record::RecordKind::Ciphertext);

    let mut cursor = Cursor::new(raw);
    let record = record::read_record(&mut cursor, opener.record_kind())
        .expect("must parse via record layer")
        .expect("must be Some");
    assert_eq!(record.content_type, ContentType::ApplicationData);

    let inner = opener.open(&record).expect("AEAD tag must verify");
    assert_eq!(inner.content_type, ContentType::Handshake);
    // Finished メッセージ（RFC 8446 §4.4.4）: type(1) || length(3) ||
    // verify_data(Hash.length=32) = 36 オクテット、type は Finished(20=0x14)。
    assert_eq!(inner.content.len(), 36);
    assert_eq!(inner.content.first().copied(), Some(0x14));
    // length フィールド（24 ビットビッグエンディアン）は verify_data 長 32
    // と一致する。
    assert_eq!(&inner.content[1..4], &[0x00, 0x00, 0x20]);
}

#[test]
fn reopening_the_same_record_after_seq_advanced_fails_closed() {
    let keys = client_handshake_traffic_key_iv();
    let raw = rfc8448_client_finished_record_bytes();

    let mut opener = Opener::new();
    opener.install_handshake_keys(&keys).expect("install");
    let mut cursor = Cursor::new(raw.clone());
    let record = record::read_record(&mut cursor, opener.record_kind())
        .expect("must parse")
        .expect("must be Some");

    opener
        .open(&record)
        .expect("first open at seq=0 must succeed");

    // seq は 1 へ進んでいるため、同じ暗号文（seq=0 で seal された）を
    // 再度 open すると nonce が食い違いタグ検証に失敗する。
    let err = opener.open(&record).expect_err("must fail at seq=1");
    assert_eq!(err, ProtectionError::BadRecordMac);
    assert_eq!(
        err.alert_description(),
        Some(record::AlertDescription::BadRecordMac)
    );
}

#[test]
fn opener_rejects_bit_flipped_rfc8448_record() {
    let keys = client_handshake_traffic_key_iv();
    let mut raw = rfc8448_client_finished_record_bytes();
    // レコードヘッダ直後（fragment 先頭）の 1 ビットを反転する。
    if let Some(byte) = raw.get_mut(record::RECORD_HEADER_LEN) {
        *byte ^= 0x01;
    }

    let mut opener = Opener::new();
    opener.install_handshake_keys(&keys).expect("install");
    let mut cursor = Cursor::new(raw);
    let record = record::read_record(&mut cursor, opener.record_kind())
        .expect("header must still parse")
        .expect("must be Some");

    assert_eq!(opener.open(&record), Err(ProtectionError::BadRecordMac));
}
