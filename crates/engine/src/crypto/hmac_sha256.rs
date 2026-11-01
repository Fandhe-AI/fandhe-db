//! HMAC-SHA-256（RFC 2104）と PBKDF2-HMAC-SHA-256 の 1 ブロック版（RFC 8018
//! `Hi` 関数。SCRAM の `SaltedPassword` 導出専用・出力長は常に 32 バイト固定）。
//!
//! `engine::crypto::sha256::Sha256` を下請けに使う（依存追加なし。
//! `.claude/rules/dependency-policy.md`）。`auth::scram`（SCRAM-SHA-256 認証。
//! Issue #940・WIRE-18・TASK-222）から呼ばれる。将来の TLS 実装（Issue #941・
//! TASK-228）が channel binding や鍵導出（HKDF）を必要とする際もこのモジュールの
//! `hmac_sha256` を下請けに再利用できる。

use super::sha256::{digest, Sha256};

const BLOCK_SIZE: usize = 64;

/// RFC 2104 の HMAC を SHA-256 で計算する。鍵長は問わない（`BLOCK_SIZE` 超過は
/// 先に `digest` で 32 バイトへ縮める、RFC 2104 §2 の規定どおり）。
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut key_block = [0u8; BLOCK_SIZE];
    if key.len() > BLOCK_SIZE {
        let hashed = digest(key);
        if let Some(slot) = key_block.get_mut(..hashed.len()) {
            slot.copy_from_slice(&hashed);
        }
    } else if let Some(slot) = key_block.get_mut(..key.len()) {
        slot.copy_from_slice(key);
    }

    let mut ipad = [0u8; BLOCK_SIZE];
    let mut opad = [0u8; BLOCK_SIZE];
    for i in 0..BLOCK_SIZE {
        let Some(k) = key_block.get(i).copied() else {
            continue;
        };
        if let Some(slot) = ipad.get_mut(i) {
            *slot = k ^ 0x36;
        }
        if let Some(slot) = opad.get_mut(i) {
            *slot = k ^ 0x5c;
        }
    }

    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(message);
    let inner_digest = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(&inner_digest);
    outer.finalize()
}

/// PBKDF2-HMAC-SHA256 の `Hi(str, salt, i)` 関数（RFC 8018 §5.2、SCRAM
/// （RFC 5802 §2.2）が `SaltedPassword` の導出に使う 1 ブロック版。出力長は常に
/// SHA-256 の出力長（32 バイト）に固定される）。
///
/// `iterations == 0` は呼び出し元（`scram::generate_verifier`）が拒否する契約
/// のため、ここでは `iterations >= 1` を前提とする（`iterations == 0` を渡すと
/// `u[..]` が全く更新されないまま `u1` を返す fail-safe な自明値になる）。
pub fn pbkdf2_hmac_sha256_one_block(password: &[u8], salt: &[u8], iterations: u32) -> [u8; 32] {
    let mut salt_block = Vec::with_capacity(salt.len() + 4);
    salt_block.extend_from_slice(salt);
    salt_block.extend_from_slice(&1u32.to_be_bytes());

    let mut u = hmac_sha256(password, &salt_block);
    let mut result = u;
    for _ in 1..iterations {
        u = hmac_sha256(password, &u);
        for (r, ui) in result.iter_mut().zip(u.iter()) {
            *r ^= ui;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    // RFC 4231 §4.2: Test Case 1
    #[test]
    fn hmac_matches_rfc4231_case1() {
        let key = [0x0bu8; 20];
        let data = b"Hi There";
        let mac = hmac_sha256(&key, data);
        assert_eq!(
            hex(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    // RFC 4231 §4.3: Test Case 2 ("Jefe"/"what do ya want for nothing?")
    #[test]
    fn hmac_matches_rfc4231_case2() {
        let key = b"Jefe";
        let data = b"what do ya want for nothing?";
        let mac = hmac_sha256(key, data);
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    // RFC 4231 §4.4: Test Case 3（0xaa 20 バイト鍵・0xdd 50 バイトデータ）
    #[test]
    fn hmac_matches_rfc4231_case3() {
        let key = [0xaau8; 20];
        let data = [0xddu8; 50];
        let mac = hmac_sha256(&key, &data);
        assert_eq!(
            hex(&mac),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
    }

    // RFC 4231 §4.6: Test Case 6（131 バイト鍵。BLOCK_SIZE 超過の鍵縮約経路）
    #[test]
    fn hmac_matches_rfc4231_case6_key_longer_than_block_size() {
        let key = [0xaau8; 131];
        let data = b"Test Using Larger Than Block-Size Key - Hash Key First";
        let mac = hmac_sha256(&key, data);
        assert_eq!(
            hex(&mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    // RFC 4231 §4.7: Test Case 7（131 バイト鍵・152 バイトデータ）
    #[test]
    fn hmac_matches_rfc4231_case7_key_and_data_longer_than_block_size() {
        let key = [0xaau8; 131];
        let data = b"This is a test using a larger than block-size key and a larger \
                      than block-size data. The key needs to be hashed before being \
                      used by the HMAC algorithm.";
        let mac = hmac_sha256(&key, data);
        assert_eq!(
            hex(&mac),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
        );
    }

    // PBKDF2-HMAC-SHA256("password","salt",1,32)（RFC 7914 系で広く参照される
    // 既知ベクタ。1 反復）。
    #[test]
    fn pbkdf2_matches_known_vector_one_iteration() {
        let out = pbkdf2_hmac_sha256_one_block(b"password", b"salt", 1);
        assert_eq!(
            hex(&out),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
    }

    // PBKDF2-HMAC-SHA256("password","salt",4096,32) の既知ベクタ。
    #[test]
    fn pbkdf2_matches_known_vector_4096_iterations() {
        let out = pbkdf2_hmac_sha256_one_block(b"password", b"salt", 4096);
        assert_eq!(
            hex(&out),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }
}
