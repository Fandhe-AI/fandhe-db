//! AES-128 ブロック暗号（`tls::aes`。TASK-228・WIRE-9・HTTP-10 ポインタ。
//! Issue #957・親 #941）の公開 API だけを使う結合テスト。単体テストの
//! 内部関数照合（S-box 全数照合等）とは別に、外部から見える契約
//! （`Aes128::new`／`encrypt_block`／`encrypt_blocks4`）が FIPS 197・
//! NIST SP 800-38A のテストベクタと一致することを固定する。

use wire_server::tls::aes::{Aes128, BLOCK_LEN, KEY_LEN};

fn hex_decode(s: &str) -> Vec<u8> {
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn array16(bytes: &[u8]) -> [u8; BLOCK_LEN] {
    let mut out = [0u8; BLOCK_LEN];
    out.copy_from_slice(bytes);
    out
}

// FIPS 197 Appendix B（暗号化の計算例）。
#[test]
fn encrypt_block_matches_fips197_appendix_b() {
    let key: [u8; KEY_LEN] = array16(&hex_decode("2b7e151628aed2a6abf7158809cf4f3c"));
    let cipher = Aes128::new(&key);
    let mut block = array16(&hex_decode("3243f6a8885a308d313198a2e0370734"));
    cipher.encrypt_block(&mut block);
    assert_eq!(hex(&block), "3925841d02dc09fbdc118597196a0b32");
}

// FIPS 197 Appendix C.1。
#[test]
fn encrypt_block_matches_fips197_appendix_c1() {
    let key: [u8; KEY_LEN] = array16(&hex_decode("000102030405060708090a0b0c0d0e0f"));
    let cipher = Aes128::new(&key);
    let mut block = array16(&hex_decode("00112233445566778899aabbccddeeff"));
    cipher.encrypt_block(&mut block);
    assert_eq!(hex(&block), "69c4e0d86a7b0430d8cdb78070b4c55a");
}

// NIST SP 800-38A F.1.1（ECB-AES128.Encrypt）。4 並列 API を通す。
#[test]
fn encrypt_blocks4_matches_sp800_38a_ecb_vectors() {
    let key: [u8; KEY_LEN] = array16(&hex_decode("2b7e151628aed2a6abf7158809cf4f3c"));
    let cipher = Aes128::new(&key);

    let plaintexts = [
        "6bc1bee22e409f96e93d7e117393172a",
        "ae2d8a571e03ac9c9eb76fac45af8e51",
        "30c81c46a35ce411e5fbc1191a0a52ef",
        "f69f2445df4f9b17ad2b417be66c3710",
    ];
    let expected = [
        "3ad77bb40d7a3660a89ecaf32466ef97",
        "f5d3d58503b9699de785895a96fdbaaf",
        "43b1cd7f598ece23881b00e3ed030688",
        "7b0c785e27e8ad3f8223207104725dd4",
    ];

    let mut blocks: [[u8; BLOCK_LEN]; 4] = [
        array16(&hex_decode(plaintexts[0])),
        array16(&hex_decode(plaintexts[1])),
        array16(&hex_decode(plaintexts[2])),
        array16(&hex_decode(plaintexts[3])),
    ];
    cipher.encrypt_blocks4(&mut blocks);
    for (i, block) in blocks.iter().enumerate() {
        assert_eq!(hex(block), expected[i], "block {i} mismatch");
    }
}

// 参考（informational）: 全 0 鍵での E_K(0^128)（#958 の GHASH `H` 計算の
// 先行確認に使う値）。
#[test]
fn encrypt_all_zero_key_and_block_reference_value() {
    let key = [0u8; KEY_LEN];
    let cipher = Aes128::new(&key);
    let mut block = [0u8; BLOCK_LEN];
    cipher.encrypt_block(&mut block);
    assert_eq!(hex(&block), "66e94bd4ef8a2c3b884cfa59ca342b2e");
}

#[test]
fn debug_output_does_not_contain_key_bytes() {
    let key: [u8; KEY_LEN] = array16(&hex_decode("2b7e151628aed2a6abf7158809cf4f3c"));
    let cipher = Aes128::new(&key);
    let debug_str = format!("{cipher:?}");
    assert!(!debug_str.contains("2b7e15"));
    assert!(debug_str.contains("redacted"));
}
