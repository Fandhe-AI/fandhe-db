//! RFC 8448 §3（Simple 1-RTT Handshake。IETF の公開文書由来。`docs/spec` の
//! 内容ではない）のトレースを、`wire_server::tls`（transcript hash・Finished。
//! Issue #964・TASK-228・WIRE-9・HTTP-10 ポインタ）の**公開 API のみ**を
//! 通しで使って再現する結合テスト。
//!
//! `tls_key_schedule_rfc8448.rs` と同じ X25519 鍵交換（RFC 8448 記載の秘密鍵）
//! から出発し、
//! X25519 → Early secret → Handshake secret →
//! `Transcript`（ClientHello → ServerHello → EncryptedExtensions →
//! Certificate → CertificateVerify → server Finished → client Finished）の
//! 各チェックポイント → `finished::build_server_finished`／
//! `finished::verify_client_finished` → Master secret →
//! application traffic secret、という利用者視点の一連の流れを 1 本の
//! 呼び出し列として固定する（受け入れ条件 A1）。

use wire_server::tls::finished;
use wire_server::tls::handshake::{Finished, HandshakeType, RawHandshake};
use wire_server::tls::key_schedule::EarlySecret;
use wire_server::tls::transcript::Transcript;
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

/// ヘッダ込みの RFC トレースバイト列から、`RawHandshake`（本文のみ保持し
/// ヘッダは `msg_type` と `encode_into` から再構成する型）を組み立てる。
fn raw(msg_type: HandshakeType, full_with_header_hex: &str) -> RawHandshake {
    let full = hex_decode(full_with_header_hex);
    RawHandshake {
        msg_type,
        body: full[4..].to_vec(),
    }
}

// RFC 8448 §3 のメッセージバイト列（ヘッダ込み）。
const CLIENT_HELLO: &str = "010000c00303cb34ecb1e78163ba1c38c6dacb196a6dffa21a8d9912ec18a2ef6283024dece7000006130113031302010000910000000b0009000006736572766572ff01000100000a0014001200\
    1d0017001800190100010101020103010400230000003300260024001d002099381de560e4bd43d23d8e435a7dbafeb3c06e51c13cae4d5413691e529aaf2c002b0003020304000d0020001e04\
    0305030603020308040805080604010501060102010402050206020202002d000201\
    01001c00024001";
const SERVER_HELLO: &str = "020000560303a6af06a4121860dc5e6e60249cd34c95930c8ac5cb1434dac155772ed3e2692800130100002e00330024001d0020c9828876112095fe66762bdbf7c672e156d6cc253b833df1dd69b1b04e751f0f002b00020304";
const ENCRYPTED_EXTENSIONS: &str =
    "080000240022000a00140012001d00170018001901000101010201030104001c0002400100000000";
const CERTIFICATE: &str = "0b0001b9000001b50001b0308201ac30820115a003020102020102300d06092a864886f70d01010b0500300e310c300a06035504031303727361301e170d3136303733303031323335395a170d3236303733303031323335395a300e310c300a0603550403130372736130819f300d06092a864886f70d010101050003818d0030818902818100b4bb498f8279303d980836399b36c6988c0c68de55e1bdb826d3901a2461eafd2de49a91d015abbc9a95137ace6c1af19eaa6af98c7ced43120998e187a80ee0ccb0524b1b018c3e0b63264d449a6d38e22a5fda430846748030530ef0461c8ca9d9efbfae8ea6d1d03e2bd193eff0ab9a8002c47428a6d35a8d88d79f7f1e3f0203010001a31a301830090603551d1304023000300b0603551d0f0404030205a0300d06092a864886f70d01010b05000381810085aad2a0e5b9276b908c65f73a7267170618a54c5f8a7b337d2df7a594365417f2eae8f8a58c8f8172f9319cf36b7fd6c55b80f21a03015156726096fd335e5e67f2dbf102702e608ccae6bec1fc63a42a99be5c3eb7107c3c54e9b9eb2bd5203b1c3b84e0a8b2f759409ba3eac9d91d402dcc0cc8f8961229ac9187b42b4de10000";
const CERTIFICATE_VERIFY: &str = "0f000084080400805a747c5d88fa9bd2e55ab085a61015b7211f824cd484145ab3ff52f1fda8477b0b7abc90db78e2d33a5c141a078653fa6bef780c5ea248eeaaa785c4f394cab6d30bbe8d4859ee511f602957b15411ac027671459e46445c9ea58c181e818e95b8c3fb0bf3278409d3be152a3da5043e063dda65cdf5aea20d53dfacd42f74f3";
const SERVER_FINISHED: &str =
    "140000209b9b141d906337fbd2cbdce71df4deda4ab42c309572cb7fffee5454b78f0718";
const CLIENT_FINISHED: &str =
    "14000020a8ec436d677634ae525ac1fcebe11a039ec17694fac6e98527b642f2edd5ce61";

#[test]
fn rfc8448_simple_1rtt_transcript_and_finished_match_trace() {
    // {client}/{server} X25519 鍵交換（RFC 8448 §3 記載の秘密鍵。
    // `tls_key_schedule_rfc8448.rs` と同じ）。
    let client_priv =
        hex_decode32("49af42ba7f7994852d713ef2784bcbcaa7911de26adc5642cb634540e7ea5005");
    let server_priv =
        hex_decode32("b1580eeadf6dd589b8ef4f2d5652578cc810e9980191ec8d058308cea216a21e");
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

    // ClientHello → ServerHello を投入し、ClientHello..ServerHello の
    // transcript hash（`th_ch_sh`）を取得する。
    let mut transcript = Transcript::new();
    transcript
        .append_client_hello(&raw(HandshakeType::ClientHello, CLIENT_HELLO))
        .expect("valid ClientHello");
    transcript
        .append_server_hello(&raw(HandshakeType::ServerHello, SERVER_HELLO))
        .expect("valid ServerHello");
    let th_ch_sh = transcript
        .hash_through_server_hello()
        .expect("checkpoint reached");
    assert_eq!(
        hex(&th_ch_sh),
        "860c06edc07858ee8e78f0e7428c58edd6b43f2ca3e6e95f02ed063cf0e1cad8"
    );

    let traffic = handshake
        .traffic_secrets(&th_ch_sh)
        .expect("valid HKDF parameters");

    // EncryptedExtensions → Certificate → CertificateVerify を投入し、
    // ClientHello..CertificateVerify の transcript hash（`th_ch_cv`。
    // server Finished の verify_data 算出対象）を取得する。
    transcript
        .append_encrypted_extensions(&raw(
            HandshakeType::EncryptedExtensions,
            ENCRYPTED_EXTENSIONS,
        ))
        .expect("valid EncryptedExtensions");
    transcript
        .append_certificate(&raw(HandshakeType::Certificate, CERTIFICATE))
        .expect("valid Certificate");
    transcript
        .append_certificate_verify(&raw(HandshakeType::CertificateVerify, CERTIFICATE_VERIFY))
        .expect("valid CertificateVerify");
    let th_ch_cv = transcript
        .hash_through_certificate_verify()
        .expect("checkpoint reached");

    // server Finished を構成し、RFC トレースの verify_data と一致することを
    // 確認する。
    let server_finished =
        finished::build_server_finished(&traffic.server, &th_ch_cv).expect("valid finished key");
    assert_eq!(
        hex(&server_finished.verify_data),
        "9b9b141d906337fbd2cbdce71df4deda4ab42c309572cb7fffee5454b78f0718"
    );
    // 送信バイト列（ヘッダ込み）もトレースと一致することを確認する。
    let mut encoded = Vec::new();
    server_finished
        .encode_into(&mut encoded)
        .expect("valid Finished encoding");
    assert_eq!(hex(&encoded), SERVER_FINISHED);

    // server Finished を transcript へ投入し、ClientHello..server Finished の
    // transcript hash（`th_ch_sf`。application traffic secret・client
    // Finished 検証対象）を取得する。
    transcript
        .append_server_finished(&raw(HandshakeType::Finished, SERVER_FINISHED))
        .expect("valid server Finished");
    let th_ch_sf = transcript
        .hash_through_server_finished()
        .expect("checkpoint reached");
    assert_eq!(
        hex(&th_ch_sf),
        "9608102a0f1ccc6db6250b7b7e417b1a000eaada3daae4777a7686c9ff83df13"
    );

    // client Finished（トレース記載の生バイト列）を検証する。
    let received_client_finished =
        Finished::parse(&hex_decode(CLIENT_FINISHED)[4..]).expect("valid Finished body");
    finished::verify_client_finished(&traffic.client, &th_ch_sf, &received_client_finished)
        .expect("client Finished must verify against RFC 8448 trace");

    // 1 ビット反転した client Finished は拒否される（fail-closed。alert は
    // decrypt_error へ写像される）。
    let mut tampered = received_client_finished.clone();
    tampered.verify_data[0] ^= 0x01;
    let err = finished::verify_client_finished(&traffic.client, &th_ch_sf, &tampered)
        .expect_err("tampered verify_data must be rejected");
    assert_eq!(err, finished::FinishedError::VerifyDataMismatch);
    assert_eq!(
        err.alert_description(),
        wire_server::tls::record::AlertDescription::DecryptError
    );

    transcript
        .append_client_finished(&raw(HandshakeType::Finished, CLIENT_FINISHED))
        .expect("valid client Finished");

    // Master secret → application traffic secret/key/iv（RFC 8448 §3。
    // `tls_key_schedule_rfc8448.rs` と同じ最終値）。
    let master = handshake.into_master().expect("valid HKDF parameters");
    let app = master
        .application_traffic_secrets(&th_ch_sf)
        .expect("valid HKDF parameters");
    let server_ap_keys = app.server.traffic_keys().expect("valid HKDF parameters");
    assert_eq!(
        hex(server_ap_keys.key()),
        "9f02283b6c9c07efc26bb9f2ac92e356"
    );
    assert_eq!(hex(server_ap_keys.iv()), "cf782b88dd83549aadf1e984");
}

// HelloRetryRequest（RFC 8448 §5。IETF の公開文書由来）を経た場合の
// transcript 再構成（`message_hash` 置換）が、実際の HRR トレースの
// server/client Finished 値と整合することを確認する（受け入れ条件 A2）。
//
// RFC 8448 §5 は traffic secret を導出する ECDHE 鍵交換に P-256 を使うが、
// 本リポの X25519 実装では再現できないため、finished_key を
// `Secret32::from_bytes`（pub(crate)）で直接構成できる `finished.rs` の
// crate 内単体テスト（`compute_verify_data_matches_rfc8448_*`）と役割分担し、
// 本テストは公開 API のみで完結する `Transcript` の message_hash 置換
// （`hash_through_server_hello` チェックポイント）の正しさに範囲を絞る。
#[test]
fn rfc8448_hello_retry_request_transcript_message_hash_matches_trace() {
    let ch1 = "010000b00303b0b1c5a5aa37c5919f2ed1d5c6fff7fcb7849716945a2b8cee9258a346677b6f000006130113031302010000810000000b0009000006736572766572ff01000100000a00080006001d00170018003300260024001d0020e8e8e3f3b93a25ed97a14a7dcacb8a272c6288e585c6484d05262fcad062ad1f002b0003020304000d0020001e040305030603020308040805080604010501060102010402050206020202002d00020101001c00024001";
    let hrr = "020000ac0303cf21ad74e59a6111be1d8c021e65b891c2a211167abb8c5e079e09e2c8a8339c001301000084003300020017002c0074007271dcd04bb88bc3189119398a00000000eefafc76c146b823b096f8aacad365dd0030953f4edf625636e5f21bb2e23fcc654b1b5b40318d10d137abcbb87574e36e8a1f025f7dfa5d6e50781b5eda4aa15b0c8be778257d16aa3030e9e7841dd9e4c0342267e8ca0caf571fb2b7cff0f934b0002b00020304";
    let ch2 = "010001fc0303b0b1c5a5aa37c5919f2ed1d5c6fff7fcb7849716945a2b8cee9258a346677b6f000006130113031302010001cd0000000b0009000006736572766572ff01000100000a00080006001d001700180033004700450017004104a6da7392ec591e17abfd535964b99894d13befb221b3def2ebe3830eac8f0151812677c4d6d2237e85cf01d6910cfb83954e76ba7352830534159897e8065780002b0003020304000d0020001e040305030603020308040805080604010501060102010402050206020202002c0074007271dcd04bb88bc3189119398a00000000eefafc76c146b823b096f8aacad365dd0030953f4edf625636e5f21bb2e23fcc654b1b5b40318d10d137abcbb87574e36e8a1f025f7dfa5d6e50781b5eda4aa15b0c8be778257d16aa3030e9e7841dd9e4c0342267e8ca0caf571fb2b7cff0f934b0002d00020101001c00024001001500af00000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000";
    let sh2 = "020000770303bb341d847fd789c47c387172dc0c9bf147fccacb5043d86ca4c598d3ff571b9800130100004f003300450017004104583e054b7a66672ae020ad9d2686fcc85b5ad41a134a0f03ee72b893052bd85b4c8de6776f5b04ac07d83540eab3e3d9c547bc6528c4317d294686093a6cad7d002b00020304";

    let mut transcript = Transcript::new();
    transcript
        .append_client_hello(&raw(HandshakeType::ClientHello, ch1))
        .expect("valid ClientHello1");
    transcript
        .append_hello_retry_request(&raw(HandshakeType::ServerHello, hrr))
        .expect("valid HelloRetryRequest");
    transcript
        .append_client_hello(&raw(HandshakeType::ClientHello, ch2))
        .expect("valid ClientHello2");
    transcript
        .append_server_hello(&raw(HandshakeType::ServerHello, sh2))
        .expect("valid ServerHello");

    let th = transcript
        .hash_through_server_hello()
        .expect("checkpoint reached");
    assert_eq!(
        hex(&th),
        "8aa8e828ec2f8a884fec95a3139de01c15a3daa7ff5bfc3f4bfcc21b438d7bf8"
    );
}
