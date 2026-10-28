//! AES-128-GCM（`tls::aes_gcm`。TASK-228・WIRE-9・HTTP-10 ポインタ。
//! Issue #958・親 #941）の公開 API だけを使う結合テスト。単体テストの
//! 内部関数照合（GHASH の代数的性質等）とは別に、外部から見える契約
//! （`Aes128Gcm::new`／`seal`／`open`）が McGrew & Viega の GCM 仕様公開
//! テストベクタと一致することを固定する。ベクタの hex はテスト内に
//! 手打ちせず、`tls::aes` の #957 テストが既に固定した全 0 鍵の
//! `E_K(0^128)` 参照値と地続きになるよう、この結合テストでも同じ
//! Test Case 1〜4 を用いる。

use wire_server::tls::aes_gcm::{AeadError, Aes128Gcm, KEY_LEN, MAX_AAD_LEN, NONCE_LEN, TAG_LEN};

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn array16(bytes: &[u8]) -> [u8; KEY_LEN] {
    let mut out = [0u8; KEY_LEN];
    out.copy_from_slice(bytes);
    out
}

fn array12(bytes: &[u8]) -> [u8; NONCE_LEN] {
    let mut out = [0u8; NONCE_LEN];
    out.copy_from_slice(bytes);
    out
}

// GCM 仕様（McGrew & Viega, "The Galois/Counter Mode of Operation"）の
// AES-128 Test Case 1: 鍵・nonce とも全 0、P・AAD とも空。
#[test]
fn seal_matches_gcm_spec_test_case_1() {
    let gcm = Aes128Gcm::new(&[0u8; KEY_LEN]);
    let nonce = [0u8; NONCE_LEN];
    let out = gcm.seal(&nonce, &[], &[]).expect("valid inputs");
    assert_eq!(out.len(), TAG_LEN);
    assert_eq!(hex(&out), "58e2fccefa7e3061367f1d57a4e7455a");
}

// Test Case 2: 鍵・nonce とも全 0、16 バイトのゼロ平文・AAD なし。
#[test]
fn seal_matches_gcm_spec_test_case_2() {
    let gcm = Aes128Gcm::new(&[0u8; KEY_LEN]);
    let nonce = [0u8; NONCE_LEN];
    let plaintext = [0u8; 16];
    let out = gcm.seal(&nonce, &[], &plaintext).expect("valid inputs");
    let (ct, tag) = out.split_at(out.len() - TAG_LEN);
    assert_eq!(hex(ct), "0388dace60b6a392f328c2b971b2fe78");
    assert_eq!(hex(tag), "ab6e47d42cec13bdf53a67b21257bddf");
}

// Test Case 3: 専用鍵・nonce・64 バイト平文・AAD なし。
#[test]
fn seal_matches_gcm_spec_test_case_3() {
    let key = array16(&hex_decode("feffe9928665731c6d6a8f9467308308"));
    let gcm = Aes128Gcm::new(&key);
    let nonce = array12(&hex_decode("cafebabefacedbaddecaf888"));
    let plaintext = hex_decode(
        "d9313225f88406e5a55909c5aff5269a\
         86a7a9531534f7da2e4c303d8a318a72\
         1c3c0c95956809532fcf0e2449a6b525\
         b16aedf5aa0de657ba637b391aafd255",
    );
    let out = gcm.seal(&nonce, &[], &plaintext).expect("valid inputs");
    let (ct, tag) = out.split_at(out.len() - TAG_LEN);
    assert_eq!(
        hex(ct),
        "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091473f5985"
    );
    assert_eq!(hex(tag), "4d5c2af327cd64a62cf35abd2ba6fab4");
}

// Test Case 4: Test Case 3 と同じ鍵・nonce、60 バイト平文・20 バイト AAD
// （ブロック長の非整数倍）。
#[test]
fn seal_matches_gcm_spec_test_case_4() {
    let key = array16(&hex_decode("feffe9928665731c6d6a8f9467308308"));
    let gcm = Aes128Gcm::new(&key);
    let nonce = array12(&hex_decode("cafebabefacedbaddecaf888"));
    let plaintext = hex_decode(
        "d9313225f88406e5a55909c5aff5269a\
         86a7a9531534f7da2e4c303d8a318a72\
         1c3c0c95956809532fcf0e2449a6b525\
         b16aedf5aa0de657ba637b39",
    );
    let aad = hex_decode("feedfacedeadbeeffeedfacedeadbeefabaddad2");
    let out = gcm.seal(&nonce, &aad, &plaintext).expect("valid inputs");
    let (ct, tag) = out.split_at(out.len() - TAG_LEN);
    assert_eq!(
        hex(ct),
        "42831ec2217774244b7221b784d0d49ce3aa212f2c02a4e035c17e2329aca12e21d514b25466931c7d8f6a5aac84aa051ba30b396a0aac973d58e091"
    );
    assert_eq!(hex(tag), "5bc94fbc3221a5db94fae95ae7121a47");
}

// Note: Test Case 5・6（96 ビット以外の IV から GHASH で J0 を導出する経路）
// は本 API が 96 ビット nonce（`NONCE_LEN = 12`）に固定されているため対象外
// （module doc・ADR 参照）。

#[test]
fn open_round_trips_with_seal() {
    let key = array16(&hex_decode("feffe9928665731c6d6a8f9467308308"));
    let gcm = Aes128Gcm::new(&key);
    let nonce = array12(&hex_decode("cafebabefacedbaddecaf888"));
    let aad = hex_decode("feedfacedeadbeeffeedfacedeadbeefabaddad2");
    let plaintext = hex_decode(
        "d9313225f88406e5a55909c5aff5269a\
         86a7a9531534f7da2e4c303d8a318a72\
         1c3c0c95956809532fcf0e2449a6b525\
         b16aedf5aa0de657ba637b39",
    );
    let sealed = gcm.seal(&nonce, &aad, &plaintext).expect("valid inputs");
    let opened = gcm.open(&nonce, &aad, &sealed).expect("valid tag");
    assert_eq!(opened, plaintext);
}

#[test]
fn open_rejects_bit_flip_in_ciphertext_aad_nonce_or_tag() {
    let gcm = Aes128Gcm::new(&[0x42u8; KEY_LEN]);
    let nonce = [0x24u8; NONCE_LEN];
    let aad = b"tls13-header".to_vec();
    let plaintext = b"handshake finished payload for aead round trip".to_vec();
    let sealed = gcm.seal(&nonce, &aad, &plaintext).expect("valid inputs");

    // 暗号文のビット反転。
    let mut flipped_ct = sealed.clone();
    if let Some(byte) = flipped_ct.first_mut() {
        *byte ^= 0x01;
    }
    assert_eq!(
        gcm.open(&nonce, &aad, &flipped_ct),
        Err(AeadError::TagMismatch)
    );

    // タグのビット反転。
    let mut flipped_tag = sealed.clone();
    if let Some(byte) = flipped_tag.last_mut() {
        *byte ^= 0x80;
    }
    assert_eq!(
        gcm.open(&nonce, &aad, &flipped_tag),
        Err(AeadError::TagMismatch)
    );

    // AAD の変更。
    let wrong_aad = b"tls13-header!".to_vec();
    assert_eq!(
        gcm.open(&nonce, &wrong_aad, &sealed),
        Err(AeadError::TagMismatch)
    );

    // nonce の変更。
    let mut wrong_nonce = nonce;
    if let Some(byte) = wrong_nonce.first_mut() {
        *byte ^= 0x01;
    }
    assert_eq!(
        gcm.open(&wrong_nonce, &aad, &sealed),
        Err(AeadError::TagMismatch)
    );
}

#[test]
fn open_rejects_lengths_shorter_than_tag() {
    let gcm = Aes128Gcm::new(&[0u8; KEY_LEN]);
    let nonce = [0u8; NONCE_LEN];
    for len in 0..TAG_LEN {
        let buf = vec![0u8; len];
        assert_eq!(
            gcm.open(&nonce, &[], &buf),
            Err(AeadError::CiphertextLength),
            "len={len}"
        );
    }
}

#[test]
fn seal_and_open_reject_length_limits() {
    use wire_server::tls::aes_gcm::MAX_PLAINTEXT_LEN;
    use wire_server::tls::record::MAX_CIPHERTEXT_LEN;

    let gcm = Aes128Gcm::new(&[0u8; KEY_LEN]);
    let nonce = [0u8; NONCE_LEN];

    // 平文の上限ちょうどは受理、+1 バイトは拒否。
    let ok = vec![0u8; MAX_PLAINTEXT_LEN];
    assert!(gcm.seal(&nonce, &[], &ok).is_ok());
    let too_long = vec![0u8; MAX_PLAINTEXT_LEN + 1];
    assert_eq!(
        gcm.seal(&nonce, &[], &too_long),
        Err(AeadError::PlaintextTooLong)
    );

    // AAD の上限ちょうどは受理、+1 バイトは拒否（seal・open 双方）。
    let aad_ok = vec![0u8; MAX_AAD_LEN];
    assert!(gcm.seal(&nonce, &aad_ok, &[]).is_ok());
    let aad_too_long = vec![0u8; MAX_AAD_LEN + 1];
    assert_eq!(
        gcm.seal(&nonce, &aad_too_long, &[]),
        Err(AeadError::AadTooLong)
    );
    let sealed = gcm.seal(&nonce, &[], b"x").expect("valid inputs");
    assert_eq!(
        gcm.open(&nonce, &aad_too_long, &sealed),
        Err(AeadError::AadTooLong)
    );

    // open への入力が MAX_CIPHERTEXT_LEN を超えると拒否。
    let over_ciphertext = vec![0u8; MAX_CIPHERTEXT_LEN + 1];
    assert_eq!(
        gcm.open(&nonce, &[], &over_ciphertext),
        Err(AeadError::CiphertextLength)
    );
}

#[test]
fn seal_is_deterministic_for_identical_inputs() {
    let gcm = Aes128Gcm::new(&[0x77u8; KEY_LEN]);
    let nonce = [0x11u8; NONCE_LEN];
    let plaintext = b"deterministic aead output".to_vec();
    let a = gcm.seal(&nonce, b"aad", &plaintext).expect("valid inputs");
    let b = gcm.seal(&nonce, b"aad", &plaintext).expect("valid inputs");
    assert_eq!(a, b);
}

#[test]
fn debug_output_does_not_contain_key_material() {
    let key = array16(&hex_decode("2b7e151628aed2a6abf7158809cf4f3c"));
    let gcm = Aes128Gcm::new(&key);
    let debug_str = format!("{gcm:?}");
    assert!(!debug_str.contains("2b7e15"));
    assert!(debug_str.contains("redacted"));
}

#[test]
fn alert_description_mapping_is_stable() {
    assert_eq!(AeadError::TagMismatch.alert_description(), 20);
    assert_eq!(AeadError::CiphertextLength.alert_description(), 20);
    assert_eq!(AeadError::PlaintextTooLong.alert_description(), 80);
    assert_eq!(AeadError::AadTooLong.alert_description(), 80);
}
