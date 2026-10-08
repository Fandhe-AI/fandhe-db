//! AES-128 ブロック暗号（暗号化方向のみ・定数時間実装。TASK-228・WIRE-9・
//! HTTP-10 ポインタ。Issue #957・親 #941）。
//!
//! 親 Issue #941 は `TLS_AES_128_GCM_SHA256` のみを対象とする。GCM の CTR
//! 鍵ストリーム生成と `H = E_K(0^128)` の計算はどちらも AES-128 の
//! **暗号化方向**だけで足りるため、本モジュールは復号
//! （`InvSubBytes`／`InvMixColumns`／`InvCipher`）を実装しない。GCM／GHASH
//! 本体（#958）・レコード保護（#959）・接続への結線（#966 以降）はいずれも
//! 対象外。
//!
//! # 定数時間の設計
//!
//! - **SubBytes・鍵展開の SubWord**: FIPS 197 の参照 S-box テーブル
//!   （256 要素）は本番コードに一切置かず、[`sbox_bitsliced`] による
//!   ブール回路（AND／XOR／NOT のみ）で計算する。GF(2^8) の乗法逆元
//!   `x^254` を、ビットプレーン上の多項式乗算（`0x11b` で mod 還元）の
//!   固定加算連鎖（指数 254 は公開定数）で求め、その後 FIPS 197 §5.1.1 の
//!   アフィン変換（定数 `0x63`）を掛ける。秘密値のビットに依存する分岐・
//!   配列添字は存在しない（256 要素の参照テーブルはテスト専用
//!   `#[cfg(test)]` の中にのみ置き、`sbox_bitsliced` の全数照合に使う）。
//! - **ShiftRows**: 公開の固定添字だけで決まる置換
//! - **MixColumns**: `xtime` をマスク演算（`0u8.wrapping_sub(hi)`）で実装し
//!   分岐を持たない
//! - `u64` の AND／XOR／シフト演算は x86_64・aarch64 で定数時間であるとみなす
//!   （[`super::field25519`] と同じ前提）
//!
//! # 鍵スケジュールの往復
//!
//! 暗号化専用の実装であるため、本番コードには復号や逆鍵展開を追加しない。
//! 正方向の鍵展開が FIPS 197 Appendix A.1 の全ラウンド鍵と一致することに
//! 加え、テスト専用（`#[cfg(test)]`）の逆鍵展開（最終ラウンド鍵から元の鍵を
//! 復元できる。AES-128 の鍵スケジュールは可逆）で往復を検証する。
//!
//! # ゼロ化の限界
//!
//! [`Aes128`] は `Drop` でラウンド鍵を [`super::hkdf::zeroize`] で
//! best-effort にゼロ化するが、`unsafe`（`write_volatile` 等）を使わない
//! ため最適化により消去が省略されない保証はない
//! （[`super::hkdf::Secret32`] と同じ限界）。

use super::hkdf::zeroize;
use std::fmt;

/// AES-128 の鍵長（バイト）。
pub const KEY_LEN: usize = 16;
/// AES のブロック長（バイト）。
pub const BLOCK_LEN: usize = 16;

/// AES-128 のラウンド数。
const NR: usize = 10;
/// AES-128 の鍵長（32 ビットワード単位）。
const NK: usize = 4;
/// 鍵スケジュール用のラウンド定数（FIPS 197 表 5。すべて公開値）。
const RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1b, 0x36];

/// AES S-box のアフィン変換定数（FIPS 197 §5.1.1）。
const AFFINE_C: u8 = 0x63;

/// GF(2^8) の乗法単位元 `1` を、64 レーン（4 ブロック×16 バイト）すべてに
/// ブロードキャストしたビットプレーン表現。公開定数のため全レーン同一で問題ない。
const ONE_LANES: [u64; 8] = {
    let mut planes = [0u64; 8];
    planes[0] = u64::MAX;
    planes
};

/// 展開済みラウンド鍵を保持する AES-128 暗号化器。
///
/// `Clone`／`Copy` は導出しない（秘密値の不用意な複製を防ぐ。
/// [`super::key_schedule::TrafficKeys`] と同じ設計判断）。`Drop` で
/// ラウンド鍵を best-effort にゼロ化する（限界は module doc 参照）。
pub struct Aes128 {
    round_keys: [[u8; 16]; NR + 1],
}

impl Aes128 {
    /// 128 ビット鍵からラウンド鍵を展開する（FIPS 197 §5.2）。
    /// [`super::key_schedule::TrafficKeys::key`] が返す `&[u8; 16]` を
    /// そのまま渡せる。
    pub fn new(key: &[u8; KEY_LEN]) -> Self {
        Self {
            round_keys: expand_key(key),
        }
    }

    /// 1 ブロックをその場で暗号化する。4 並列版（[`Self::encrypt_blocks4`]）の
    /// lane 0 だけを使うラッパーで、コードパスを 1 本にまとめる。
    pub fn encrypt_block(&self, block: &mut [u8; BLOCK_LEN]) {
        let mut blocks = [*block, [0u8; BLOCK_LEN], [0u8; BLOCK_LEN], [0u8; BLOCK_LEN]];
        self.encrypt_blocks4(&mut blocks);
        block.copy_from_slice(&blocks[0]);
    }

    /// 4 ブロックを並列に暗号化する（`#958` の GCM CTR 鍵ストリームが
    /// 使う想定）。SubBytes 段は 4 ブロック分をまとめて 1 回の
    /// ビットスライス S-box 呼び出しで処理する。
    pub fn encrypt_blocks4(&self, blocks: &mut [[u8; BLOCK_LEN]; 4]) {
        add_round_key(blocks, &self.round_keys[0]);
        for round in &self.round_keys[1..NR] {
            sub_bytes4(blocks);
            shift_rows4(blocks);
            mix_columns4(blocks);
            add_round_key(blocks, round);
        }
        sub_bytes4(blocks);
        shift_rows4(blocks);
        add_round_key(blocks, &self.round_keys[NR]);
    }

    /// テスト専用: 展開済みラウンド鍵への参照（往復検証・ゼロ化検証に使う）。
    #[cfg(test)]
    fn round_keys(&self) -> &[[u8; 16]; NR + 1] {
        &self.round_keys
    }

    /// ラウンド鍵を全ゼロ化する本体。`Drop` から呼ぶ（ゼロ化の限界は
    /// module doc を参照。`unsafe` を使わないため最適化での省略は防げない）。
    fn wipe(&mut self) {
        for rk in self.round_keys.iter_mut() {
            zeroize(rk);
        }
    }
}

impl fmt::Debug for Aes128 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Aes128")
            .field("round_keys", &"<redacted>")
            .finish()
    }
}

impl Drop for Aes128 {
    fn drop(&mut self) {
        self.wipe();
    }
}

/// 4 ブロックへ AddRoundKey（ラウンド鍵との XOR）を適用する。
fn add_round_key(blocks: &mut [[u8; BLOCK_LEN]; 4], round_key: &[u8; 16]) {
    for block in blocks.iter_mut() {
        for i in 0..BLOCK_LEN {
            block[i] ^= round_key[i];
        }
    }
}

/// 4 ブロック（64 バイト）へ SubBytes を適用する。64 バイトを 1 回だけ
/// ビットプレーンへ転置し、[`sbox_bitsliced`] を通してから元に戻す。
fn sub_bytes4(blocks: &mut [[u8; BLOCK_LEN]; 4]) {
    let mut flat = [0u8; 64];
    for (i, block) in blocks.iter().enumerate() {
        flat[i * BLOCK_LEN..(i + 1) * BLOCK_LEN].copy_from_slice(block);
    }
    sbox_apply_bytes(&mut flat);
    for (i, block) in blocks.iter_mut().enumerate() {
        block.copy_from_slice(&flat[i * BLOCK_LEN..(i + 1) * BLOCK_LEN]);
    }
}

/// 4 ブロックへ ShiftRows を適用する。状態は列優先（FIPS 197 §3.4。
/// バイト添字 `c*4+r`）で、行 `r` を `r` バイト左に巡回シフトする
/// （置換の添字は公開の固定値のみ）。
fn shift_rows4(blocks: &mut [[u8; BLOCK_LEN]; 4]) {
    for block in blocks.iter_mut() {
        let src = *block;
        for r in 0..4 {
            for c in 0..4 {
                block[c * 4 + r] = src[((c + r) % 4) * 4 + r];
            }
        }
    }
}

/// GF(2) 上の `x` の 2 倍算（`xtime`）。分岐を持たず、最上位ビットから
/// マスク（全 0 か全 1 のいずれか）を作って還元多項式 `0x1b` を条件付きで
/// XOR する。
fn xtime(b: u8) -> u8 {
    let hi = (b >> 7) & 1;
    let mask = 0u8.wrapping_sub(hi);
    (b << 1) ^ (mask & 0x1b)
}

/// 4 ブロックへ MixColumns を適用する（最終ラウンドでは呼ばない）。
fn mix_columns4(blocks: &mut [[u8; BLOCK_LEN]; 4]) {
    for block in blocks.iter_mut() {
        for c in 0..4 {
            let s0 = block[c * 4];
            let s1 = block[c * 4 + 1];
            let s2 = block[c * 4 + 2];
            let s3 = block[c * 4 + 3];
            block[c * 4] = xtime(s0) ^ (xtime(s1) ^ s1) ^ s2 ^ s3;
            block[c * 4 + 1] = s0 ^ xtime(s1) ^ (xtime(s2) ^ s2) ^ s3;
            block[c * 4 + 2] = s0 ^ s1 ^ xtime(s2) ^ (xtime(s3) ^ s3);
            block[c * 4 + 3] = (xtime(s0) ^ s0) ^ s1 ^ s2 ^ xtime(s3);
        }
    }
}

/// バイト列（最大 64 バイト）を、S-box 適用のためのビットプレーン表現
/// （`[u64; 8]`。プレーン `j` のビット `i` はバイト `i` のビット `j`）へ
/// 転置し、[`sbox_bitsliced`] を適用してから書き戻す。
///
/// 4 ブロックの SubBytes（64 バイト全体）・鍵展開の SubWord（4 バイトのみ）
/// の双方から呼ばれ、同一の S-box 実装を必ず経由させる契約を保つ
/// （鍵展開にもテーブル参照を残さないための唯一の入口）。
fn sbox_apply_bytes(buf: &mut [u8]) {
    debug_assert!(buf.len() <= 64);
    let mut full = [0u8; 64];
    let len = buf.len().min(64);
    full[..len].copy_from_slice(&buf[..len]);

    let mut planes = bytes_to_bitplanes(&full);
    sbox_bitsliced(&mut planes);
    let full2 = bitplanes_to_bytes(&planes);

    buf[..len].copy_from_slice(&full2[..len]);
}

/// 64 バイトをビットプレーン表現（`[u64; 8]`）へ転置する。添字 `i`・`j` は
/// いずれも公開のループカウンタであり、シフト量・書き込み先はバイト値
/// （秘密値）に依存しない。ビットの取り出し自体は算術演算のみで行い分岐は
/// 使わない。
fn bytes_to_bitplanes(bytes: &[u8; 64]) -> [u64; 8] {
    let mut planes = [0u64; 8];
    for (i, &byte) in bytes.iter().enumerate() {
        for (j, plane) in planes.iter_mut().enumerate() {
            let bit = ((byte >> j) & 1) as u64;
            *plane |= bit << i;
        }
    }
    planes
}

/// [`bytes_to_bitplanes`] の逆変換。
fn bitplanes_to_bytes(planes: &[u64; 8]) -> [u8; 64] {
    let mut bytes = [0u8; 64];
    for (i, out) in bytes.iter_mut().enumerate() {
        let mut byte = 0u8;
        for (j, plane) in planes.iter().enumerate() {
            let bit = ((plane >> i) & 1) as u8;
            byte |= bit << j;
        }
        *out = byte;
    }
    bytes
}

/// GF(2^8)（AES の既約多項式 `x^8+x^4+x^3+x+1` = `0x11b`）上の乗算を、
/// ビットプレーン表現のまま 64 レーン同時に計算する。学校算術で次数 14 まで
/// の多項式積を求め、次数の高い項から固定の折り返し先（多項式の指数のみに
/// 依存する公開情報）へ XOR で還元する。値（各プレーンのビット）に依存する
/// 分岐は存在しない。
fn gf28_mul_bitsliced(a: &[u64; 8], b: &[u64; 8]) -> [u64; 8] {
    let mut r = [0u64; 15];
    for i in 0..8 {
        for j in 0..8 {
            r[i + j] ^= a[i] & b[j];
        }
    }
    // x^8 ≡ x^4 + x^3 + x + 1 (mod 0x11b) を使い、高次項を低次項へ折り返す。
    // ループは k=14..8 の降順固定で、書き込み先（base, base+1, base+3,
    // base+4）は常に現在の k より小さいため、後続の反復で正しく積算される。
    for k in (8..15).rev() {
        let c = r[k];
        let base = k - 8;
        r[base] ^= c;
        r[base + 1] ^= c;
        r[base + 3] ^= c;
        r[base + 4] ^= c;
    }
    let mut out = [0u64; 8];
    out.copy_from_slice(&r[0..8]);
    out
}

/// GF(2^8) の乗法逆元 `a^254`（`a=0` のときは `0`。AES S-box の慣例と一致）を
/// 求める。指数 254（`0b11111110`）は公開定数のため、左から右への
/// バイナリべき乗法（二乗と乗算の固定連鎖）で計算しても分岐は秘密値に
/// 依存しない。
fn gf28_inv_bitsliced(a: &[u64; 8]) -> [u64; 8] {
    // 指数 254 の各ビット（MSB→LSB）。公開定数のため定数時間性を損なわない。
    const BITS: [bool; 8] = [true, true, true, true, true, true, true, false];
    let mut result = ONE_LANES;
    for &bit in BITS.iter() {
        result = gf28_mul_bitsliced(&result, &result);
        if bit {
            result = gf28_mul_bitsliced(&result, a);
        }
    }
    result
}

/// FIPS 197 §5.1.1 のアフィン変換（定数 `0x63`）をビットプレーン表現へ適用する。
fn affine_transform(inv: &[u64; 8]) -> [u64; 8] {
    let mut out = [0u64; 8];
    for (i, slot) in out.iter_mut().enumerate() {
        let mut v =
            inv[i] ^ inv[(i + 4) % 8] ^ inv[(i + 5) % 8] ^ inv[(i + 6) % 8] ^ inv[(i + 7) % 8];
        if (AFFINE_C >> i) & 1 == 1 {
            v ^= u64::MAX;
        }
        *slot = v;
    }
    out
}

/// AES S-box をビットプレーン表現へ適用する（GF(2^8) 逆元 → アフィン変換）。
/// 本番コードの SubBytes・SubWord のいずれもがこの関数を経由し、
/// 256 要素の参照テーブルは持たない。
fn sbox_bitsliced(planes: &mut [u64; 8]) {
    let inv = gf28_inv_bitsliced(planes);
    *planes = affine_transform(&inv);
}

/// 鍵スケジュールの `RotWord`（FIPS 197 §5.2）。
fn rot_word(w: [u8; 4]) -> [u8; 4] {
    [w[1], w[2], w[3], w[0]]
}

/// AES-128 の鍵展開（FIPS 197 §5.2）。`SubWord` は [`sbox_apply_bytes`] を
/// 通し、SubBytes と同じビットスライス S-box を必ず経由させる。
fn expand_key(key: &[u8; KEY_LEN]) -> [[u8; 16]; NR + 1] {
    const TOTAL_WORDS: usize = 4 * (NR + 1);
    let mut w = [[0u8; 4]; TOTAL_WORDS];
    for i in 0..NK {
        w[i] = [key[4 * i], key[4 * i + 1], key[4 * i + 2], key[4 * i + 3]];
    }
    for i in NK..TOTAL_WORDS {
        let mut temp = w[i - 1];
        if i % NK == 0 {
            temp = rot_word(temp);
            sbox_apply_bytes(&mut temp);
            temp[0] ^= RCON[i / NK - 1];
        }
        let mut prev = w[i - NK];
        for j in 0..4 {
            temp[j] ^= prev[j];
        }
        w[i] = temp;
        // 鍵由来の一時値（w[i-1]・w[i-NK] のコピー）はここで用済みになる。
        // 次の反復でスタック上のバッファが再利用される前に best-effort で
        // ゼロ化し、残存コピーの生存期間を縮める（PR #1023 codex-review 指摘）。
        zeroize(&mut temp);
        zeroize(&mut prev);
    }

    let mut round_keys = [[0u8; 16]; NR + 1];
    for (r, rk) in round_keys.iter_mut().enumerate() {
        for c in 0..4 {
            rk[4 * c..4 * c + 4].copy_from_slice(&w[r * 4 + c]);
        }
    }
    // `w` は元鍵（w[0..NK]）を含む全鍵展開ワード列であり、round_keys へ
    // コピーした後もスタック上に値が残り得る。Aes128::Drop が round_keys を
    // ゼロ化しても w 自体は別バッファのため対象外だった（PR #1023
    // codex-review 指摘）。呼び出し元へ返す前に全ワードを best-effort で
    // ゼロ化する。
    for word in w.iter_mut() {
        zeroize(word);
    }
    round_keys
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("valid hex"))
            .collect()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn array16(bytes: &[u8]) -> [u8; 16] {
        let mut out = [0u8; 16];
        out.copy_from_slice(bytes);
        out
    }

    // --- テスト専用の参照実装（総当たりの GF(2^8) 逆元 + アフィン変換）。
    // 本番の `sbox_bitsliced` と全数照合するための独立導出であり、記憶からの
    // 表転記に頼らない。

    fn gf28_mul_scalar(mut a: u8, mut b: u8) -> u8 {
        let mut p = 0u8;
        for _ in 0..8 {
            if b & 1 != 0 {
                p ^= a;
            }
            let hi = a & 0x80;
            a <<= 1;
            if hi != 0 {
                a ^= 0x1b;
            }
            b >>= 1;
        }
        p
    }

    fn gf28_inv_scalar(a: u8) -> u8 {
        if a == 0 {
            return 0;
        }
        // a^254 を a^1 から 253 回の乗算で総当たりに求める（テスト専用・
        // 速度は問わない。本番の加算連鎖とは独立した導出にする）。
        let mut result = a;
        for _ in 0..253 {
            result = gf28_mul_scalar(result, a);
        }
        result
    }

    fn reference_sbox(x: u8) -> u8 {
        let inv = gf28_inv_scalar(x);
        let mut out = 0u8;
        for i in 0..8 {
            let bit = ((inv >> i) & 1)
                ^ ((inv >> ((i + 4) % 8)) & 1)
                ^ ((inv >> ((i + 5) % 8)) & 1)
                ^ ((inv >> ((i + 6) % 8)) & 1)
                ^ ((inv >> ((i + 7) % 8)) & 1)
                ^ ((AFFINE_C >> i) & 1);
            out |= bit << i;
        }
        out
    }

    fn bitsliced_sbox_single(x: u8) -> u8 {
        let mut buf = [x];
        sbox_apply_bytes(&mut buf);
        buf[0]
    }

    #[test]
    fn reference_sbox_matches_known_values() {
        // FIPS 197 §5.1.1 で言及される既知の対応値。
        assert_eq!(reference_sbox(0x00), 0x63);
        assert_eq!(reference_sbox(0x53), 0xed);
    }

    #[test]
    fn sbox_bitsliced_matches_reference_for_all_256_inputs() {
        for x in 0u16..256 {
            let x = x as u8;
            assert_eq!(
                bitsliced_sbox_single(x),
                reference_sbox(x),
                "sbox mismatch at input {x:#04x}"
            );
        }
    }

    #[test]
    fn bitplane_transpose_roundtrips() {
        let mut bytes = [0u8; 64];
        for (i, b) in bytes.iter_mut().enumerate() {
            // 決定的だが多様なパターン（全 0/全 1 を含む）。
            *b = ((i as u32).wrapping_mul(2654435761) >> 24) as u8;
        }
        let planes = bytes_to_bitplanes(&bytes);
        let roundtrip = bitplanes_to_bytes(&planes);
        assert_eq!(bytes, roundtrip);
    }

    // FIPS 197 Appendix A.1（AES-128 鍵展開）。
    #[test]
    fn key_expansion_matches_fips197_appendix_a1() {
        let key = array16(&hex_decode("2b7e151628aed2a6abf7158809cf4f3c"));
        let cipher = Aes128::new(&key);
        let rk = cipher.round_keys();

        let expected_words: [&str; 44] = [
            "2b7e1516", "28aed2a6", "abf71588", "09cf4f3c", "a0fafe17", "88542cb1", "23a33939",
            "2a6c7605", "f2c295f2", "7a96b943", "5935807a", "7359f67f", "3d80477d", "4716fe3e",
            "1e237e44", "6d7a883b", "ef44a541", "a8525b7f", "b671253b", "db0bad00", "d4d1c6f8",
            "7c839d87", "caf2b8bc", "11f915bc", "6d88a37a", "110b3efd", "dbf98641", "ca0093fd",
            "4e54f70e", "5f5fc9f3", "84a64fb2", "4ea6dc4f", "ead27321", "b58dbad2", "312bf560",
            "7f8d292f", "ac7766f3", "19fadc21", "28d12941", "575c006e", "d014f9a8", "c9ee2589",
            "e13f0cc8", "b6630ca6",
        ];

        for (i, expected) in expected_words.iter().enumerate() {
            let round = i / 4;
            let col = i % 4;
            let word = &rk[round][4 * col..4 * col + 4];
            assert_eq!(hex(word), *expected, "word w[{i}] mismatch");
        }
    }

    // FIPS 197 Appendix B（暗号化の計算例。鍵は Appendix A.1 と同一）。
    #[test]
    fn encrypt_block_matches_fips197_appendix_b() {
        let key = array16(&hex_decode("2b7e151628aed2a6abf7158809cf4f3c"));
        let cipher = Aes128::new(&key);
        let mut block = array16(&hex_decode("3243f6a8885a308d313198a2e0370734"));
        cipher.encrypt_block(&mut block);
        assert_eq!(hex(&block), "3925841d02dc09fbdc118597196a0b32");
    }

    // FIPS 197 Appendix C.1（鍵 000102...0f、平文 00112233...ff）。
    #[test]
    fn encrypt_block_matches_fips197_appendix_c1() {
        let key = array16(&hex_decode("000102030405060708090a0b0c0d0e0f"));
        let cipher = Aes128::new(&key);
        let mut block = array16(&hex_decode("00112233445566778899aabbccddeeff"));
        cipher.encrypt_block(&mut block);
        assert_eq!(hex(&block), "69c4e0d86a7b0430d8cdb78070b4c55a");
    }

    // NIST SP 800-38A F.1.1（ECB-AES128.Encrypt）。鍵は Appendix B と同一。
    #[test]
    fn encrypt_blocks4_matches_sp800_38a_ecb_vectors() {
        let key = array16(&hex_decode("2b7e151628aed2a6abf7158809cf4f3c"));
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

        let mut blocks = [
            array16(&hex_decode(plaintexts[0])),
            array16(&hex_decode(plaintexts[1])),
            array16(&hex_decode(plaintexts[2])),
            array16(&hex_decode(plaintexts[3])),
        ];
        cipher.encrypt_blocks4(&mut blocks);
        for (i, block) in blocks.iter().enumerate() {
            assert_eq!(hex(block), expected[i], "block {i} mismatch");
        }

        // 単発 `encrypt_block` でも同じ結果になることを確認する
        // （4 並列版のコードパスと分岐しないことの機械検証）。
        for (i, pt) in plaintexts.iter().enumerate() {
            let mut single = array16(&hex_decode(pt));
            cipher.encrypt_block(&mut single);
            assert_eq!(hex(&single), expected[i], "single block {i} mismatch");
        }
    }

    // lane 1〜3 に任意の値を入れても lane 0 の結果が変わらない（4 並列の
    // 各レーンが独立していることの確認）。
    #[test]
    fn encrypt_blocks4_lanes_are_independent() {
        let key = array16(&hex_decode("000102030405060708090a0b0c0d0e0f"));
        let cipher = Aes128::new(&key);
        let lane0 = array16(&hex_decode("00112233445566778899aabbccddeeff"));

        let mut blocks_a = [lane0, [0u8; 16], [0u8; 16], [0u8; 16]];
        let mut blocks_b = [
            lane0,
            [0xffu8; 16],
            array16(&hex_decode("2b7e151628aed2a6abf7158809cf4f3c")),
            array16(&hex_decode("6bc1bee22e409f96e93d7e117393172a")),
        ];
        cipher.encrypt_blocks4(&mut blocks_a);
        cipher.encrypt_blocks4(&mut blocks_b);
        assert_eq!(blocks_a[0], blocks_b[0]);
    }

    // 参考（informational）: 全 0 鍵での E_K(0^128)。#958 の GHASH `H` 計算の
    // 先行確認に使う値であり、本テストの合否自体は本 Issue の受け入れ基準。
    #[test]
    fn encrypt_all_zero_key_and_block_reference_value() {
        let key = [0u8; 16];
        let cipher = Aes128::new(&key);
        let mut block = [0u8; 16];
        cipher.encrypt_block(&mut block);
        assert_eq!(hex(&block), "66e94bd4ef8a2c3b884cfa59ca342b2e");
    }

    // --- 鍵スケジュールの往復（テスト専用の逆展開）。

    /// テスト専用: 最終ラウンド鍵から元の 128 ビット鍵を復元する
    /// （AES-128 の鍵スケジュールは可逆）。本番コードには追加しない
    /// （暗号化専用の実装方針）。
    fn invert_key_schedule(round_keys: &[[u8; 16]; NR + 1]) -> [u8; KEY_LEN] {
        const TOTAL_WORDS: usize = 4 * (NR + 1);
        let mut w = [[0u8; 4]; TOTAL_WORDS];
        for (r, rk) in round_keys.iter().enumerate() {
            for c in 0..4 {
                w[r * 4 + c].copy_from_slice(&rk[4 * c..4 * c + 4]);
            }
        }
        // w[i] = w[i-NK] ^ f(w[i-1]) より w[i-NK] = w[i] ^ f(w[i-1])。
        // 末尾から先頭へ向かって復元する。
        for i in (NK..TOTAL_WORDS).rev() {
            let mut temp = w[i - 1];
            if i % NK == 0 {
                temp = rot_word(temp);
                sbox_apply_bytes(&mut temp);
                temp[0] ^= RCON[i / NK - 1];
            }
            let cur = w[i];
            let mut restored = [0u8; 4];
            for j in 0..4 {
                restored[j] = cur[j] ^ temp[j];
            }
            w[i - NK] = restored;
        }
        let mut key = [0u8; KEY_LEN];
        for i in 0..NK {
            key[4 * i..4 * i + 4].copy_from_slice(&w[i]);
        }
        key
    }

    #[test]
    fn key_schedule_round_trips_via_test_only_inverse_expansion() {
        let key = array16(&hex_decode("000102030405060708090a0b0c0d0e0f"));
        let cipher = Aes128::new(&key);
        let restored = invert_key_schedule(cipher.round_keys());
        assert_eq!(restored, key);
    }

    #[test]
    fn wipe_zeroes_all_round_key_bytes() {
        let key = array16(&hex_decode("2b7e151628aed2a6abf7158809cf4f3c"));
        let mut cipher = Aes128::new(&key);
        // wipe 前は非ゼロのラウンド鍵が含まれることを確認してから wipe する。
        assert!(cipher
            .round_keys()
            .iter()
            .any(|rk| rk.iter().any(|&b| b != 0)));
        cipher.wipe();
        for rk in cipher.round_keys().iter() {
            assert_eq!(*rk, [0u8; 16]);
        }
    }

    #[test]
    fn debug_output_does_not_contain_key_bytes() {
        let key = array16(&hex_decode("2b7e151628aed2a6abf7158809cf4f3c"));
        let cipher = Aes128::new(&key);
        let debug_str = format!("{cipher:?}");
        assert!(!debug_str.contains("2b7e15"));
        assert!(debug_str.contains("redacted"));
    }

    // 手動専用のスループット参考値計測（`x25519` の `#[ignore]` テストと同じ
    // 流儀）。固定値のアサートはせず、`docs/design/tls-aes.md` へ手動で
    // 記録する参考値を出力する。採否の根拠にはしない
    // （`docs/design/benchmark-judgement-policy.md` 参照）。
    //
    // 実行: cargo test -p fandhe-db-wire-server --release \
    //   aes128_throughput_reference -- --ignored --nocapture
    #[test]
    #[ignore]
    fn aes128_throughput_reference() {
        let key = array16(&hex_decode("000102030405060708090a0b0c0d0e0f"));
        let cipher = Aes128::new(&key);
        let mut blocks = [
            array16(&hex_decode("00112233445566778899aabbccddeeff")),
            [0x11u8; 16],
            [0x22u8; 16],
            [0x33u8; 16],
        ];

        const ITERATIONS: u32 = 200_000;
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            cipher.encrypt_blocks4(&mut blocks);
        }
        let elapsed = start.elapsed();
        let bytes_processed = (ITERATIONS as u64) * 4 * (BLOCK_LEN as u64);
        let mb_per_sec = (bytes_processed as f64 / 1_000_000.0) / elapsed.as_secs_f64();
        println!(
            "aes128_throughput_reference: {bytes_processed} bytes in {elapsed:?} ({mb_per_sec:.2} MB/s, 共有環境の参考値・採否根拠にしない)"
        );
    }
}
