//! AES-128-GCM（NIST SP 800-38D。AEAD 暗号化／復号・タグ検証。TASK-228・
//! WIRE-9・HTTP-10 ポインタ。Issue #958・親 #941）。
//!
//! 親 Issue #941 は `TLS_AES_128_GCM_SHA256` のみを対象とする。本モジュールは
//! [`super::aes::Aes128`]（暗号化方向のみ）を使って CTR 鍵ストリームと
//! `H = E_K(0^128)` を計算し、GHASH による認証タグの生成・検証まで単独で
//! 完結させる。per-record nonce（iv XOR seq）の導出・
//! `TLSInnerPlaintext` の内容型パディング・レコード保護本体は
//! [`super::key_schedule`]・#959 の担当で、本モジュールは対象外。
//!
//! # ビット順の注意
//!
//! GCM の GF(2^128) 表現は反射順（reflected order）で、ブロックの先頭バイト
//! （バイト 0）の最上位ビットが多項式の定数項（x^0）の係数になる。本実装は
//! ブロックをビッグエンディアンで `(hi, lo)` の 2 語へ読み込み、GHASH の
//! ビット直列乗算は `hi` の最上位ビットから消費する（[`gf128_mul`] 参照）。
//! ここを誤っても [`gf128_mul`] 単体の代数的性質（可換性・単位元）を検証する
//! テストは通ってしまい、公開テストベクタとの照合だけが失敗する。実測が
//! 合わない場合はまずここを疑う。
//!
//! # 定数時間の設計
//!
//! - GHASH の GF(2^128) 乗算（[`gf128_mul`]）はビット直列の shift-and-add
//!   （SP 800-38D Algorithm 1）で、秘密値（`H`・暗号文由来のブロック）に依存
//!   するテーブル参照・分岐・添字は一切使わない。4 ビット／8 ビットの
//!   テーブル方式（Shoup 法）は添字が秘密値 `H` に依存しキャッシュタイミング
//!   攻撃を受けるため不採用。BearSSL の `ctmul64` 型（穴あき整数乗算・
//!   Karatsuba）は性能面の後続候補として記憶に留めるに留め、今回は導出・
//!   検証が容易な直列方式を選んだ
//! - タグ検証は [`super::hkdf::ct_eq`]（定数時間比較）で行う。**復号（CTR
//!   XOR）は検証に成功した場合のみ実行**し、失敗時は平文バッファを一切
//!   確保・生成しない（[`Aes128Gcm::open`] 参照。復号オラクルを作らない）
//! - `u64` の AND／XOR／シフト演算は x86_64・aarch64 で定数時間であるとみなす
//!   （[`super::aes`]・[`super::field25519`] と同じ前提）
//!
//! # 対象外の範囲
//!
//! - 96 ビット以外の nonce（GHASH から J0 を導出する経路）。TLS 1.3 は常に
//!   12 バイト nonce のため実装しない
//! - in-place／detached API（`C || tag` を返す形のみ提供する）
//! - **nonce の一意性は呼び出し側（#959 のシーケンス番号管理）の契約**で
//!   あり、本モジュールは強制しない
//!
//! # ゼロ化の限界
//!
//! [`Aes128Gcm`] は `Drop` で `H` を [`super::hkdf::zeroize`] により
//! best-effort にゼロ化する（内包する [`super::aes::Aes128`] 自身も Drop で
//! ラウンド鍵をゼロ化する）。CTR 鍵ストリーム・`E_K(J0)` の一時値も使用後に
//! ゼロ化するが、いずれも `unsafe`（`write_volatile` 等）を使わないため
//! 最適化により消去が省略されない保証はない（[`super::aes::Aes128`]・
//! [`super::hkdf::Secret32`] と同じ限界）。

use super::aes::{Aes128, BLOCK_LEN};
use super::hkdf::{ct_eq, zeroize};
use super::record;
use std::fmt;

/// AES-128-GCM の鍵長（バイト）。
pub const KEY_LEN: usize = 16;
/// nonce 長（バイト）。96 ビット固定（TLS 1.3 は常にこの長さ）。
pub const NONCE_LEN: usize = 12;
/// 認証タグ長（バイト）。短縮タグは受理しない。
pub const TAG_LEN: usize = 16;

/// 追加データ（AAD）の上限。TLS 1.3 の AAD は 5 バイトのレコードヘッダ
/// （[`record::RECORD_HEADER_LEN`]）だが、公開テストベクタには 20 バイトの
/// AAD を持つものがあるため、それを収める最小限の余裕として 64 とする。
pub const MAX_AAD_LEN: usize = 64;
/// [`Aes128Gcm::seal`] が受理する平文の上限。レコード 1 件に収まる AEAD
/// 平文（`TLSInnerPlaintext`）の上限で、暗号文＋タグが
/// [`record::MAX_CIPHERTEXT_LEN`] を超えないように定める。
pub const MAX_PLAINTEXT_LEN: usize = record::MAX_CIPHERTEXT_LEN - TAG_LEN;

const _: () = assert!(TAG_LEN <= record::MAX_CIPHERTEXT_LEN);
// ブロック数が 32 ビットカウンタ（inc32）の上限（2^32 - 2）に対して十分小さい
// ことを静的に固定する。MAX_PLAINTEXT_LEN は現状 2^14 + 240 程度であり、
// ブロック数は高々 1032 個で 2^32 に遠く及ばない。
const _: () = assert!((MAX_PLAINTEXT_LEN / BLOCK_LEN + 1) < (u32::MAX as usize - 1));
// バイト長からビット長への換算（×8）が u64 で溢れないことを固定する。
const _: () = assert!((MAX_PLAINTEXT_LEN as u64).checked_mul(8).is_some());
const _: () = assert!((MAX_AAD_LEN as u64).checked_mul(8).is_some());

/// [`Aes128Gcm::seal`]／[`Aes128Gcm::open`] の失敗理由。詳細な長さ・内容は
/// `Display` に含めない（エラー応答経由の情報漏えい防止）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AeadError {
    /// タグ検証に失敗した（改ざん・誤った鍵／nonce・鍵/nonce の不一致）。
    TagMismatch,
    /// `open` への入力長がタグ長未満、または上限を超えた。
    CiphertextLength,
    /// `seal` への平文が [`MAX_PLAINTEXT_LEN`] を超えた。
    PlaintextTooLong,
    /// AAD が [`MAX_AAD_LEN`] を超えた。
    AadTooLong,
}

impl fmt::Display for AeadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AeadError::TagMismatch => write!(f, "AEAD authentication failed"),
            AeadError::CiphertextLength => write!(f, "AEAD ciphertext length out of range"),
            AeadError::PlaintextTooLong => write!(f, "AEAD plaintext exceeds maximum length"),
            AeadError::AadTooLong => write!(f, "AEAD additional data exceeds maximum length"),
        }
    }
}

impl std::error::Error for AeadError {}

impl AeadError {
    /// TLS 1.3 alert の `AlertDescription` 値への写像。実送出は #965 の担当。
    /// `TagMismatch`／`CiphertextLength`（復号できない）はいずれも
    /// `bad_record_mac`（20）へ収束させ、詳細を漏らさない。呼び出し契約違反
    /// （`seal` 側の長さ超過）は `internal_error`（80）とする。
    pub const fn alert_description(self) -> u8 {
        match self {
            AeadError::TagMismatch => 20,
            AeadError::CiphertextLength => 20,
            AeadError::PlaintextTooLong => 80,
            AeadError::AadTooLong => 80,
        }
    }
}

/// AES-128-GCM の暗号化器・復号器。`H = E_K(0^128)` を構築時に 1 度だけ
/// 計算して保持する。
///
/// `Clone`／`Copy` は導出しない（秘密値の不用意な複製を防ぐ。[`Aes128`]・
/// [`super::key_schedule::TrafficKeys`] と同じ設計判断）。`Debug` は内容を
/// 出さず、`Drop` で `H` を best-effort ゼロ化する（限界は module doc 参照）。
pub struct Aes128Gcm {
    cipher: Aes128,
    /// `H = E_K(0^128)` をビッグエンディアンで上位・下位の 2 語に分けた
    /// GHASH 用の鍵。
    h: (u64, u64),
}

impl Aes128Gcm {
    /// 128 ビット鍵から構築する。[`super::key_schedule::TrafficKeys::key`]
    /// が返す `&[u8; 16]` をそのまま渡せる。
    pub fn new(key: &[u8; KEY_LEN]) -> Self {
        let cipher = Aes128::new(key);
        let h = compute_h(&cipher);
        Self { cipher, h }
    }

    /// 平文を暗号化し、`C || tag`（`plaintext.len() + TAG_LEN` バイト）を
    /// 返す。`nonce` の一意性は呼び出し側の契約（module doc 参照）。
    pub fn seal(
        &self,
        nonce: &[u8; NONCE_LEN],
        aad: &[u8],
        plaintext: &[u8],
    ) -> Result<Vec<u8>, AeadError> {
        if aad.len() > MAX_AAD_LEN {
            return Err(AeadError::AadTooLong);
        }
        if plaintext.len() > MAX_PLAINTEXT_LEN {
            return Err(AeadError::PlaintextTooLong);
        }

        let j0 = build_j0(nonce);
        let mut out = Vec::with_capacity(plaintext.len() + TAG_LEN);
        out.extend_from_slice(plaintext);

        let mut counter = j0;
        inc32(&mut counter);
        ctr_xor(&self.cipher, counter, &mut out);

        let tag = self.compute_tag(&j0, aad, &out);
        out.extend_from_slice(&tag);
        Ok(out)
    }

    /// `ciphertext_and_tag`（`C || tag`）を検証・復号する。**タグを検証して
    /// から復号する**順序を守り、検証に失敗した場合は平文バッファを一切
    /// 確保・生成しない（module doc 参照）。
    pub fn open(
        &self,
        nonce: &[u8; NONCE_LEN],
        aad: &[u8],
        ciphertext_and_tag: &[u8],
    ) -> Result<Vec<u8>, AeadError> {
        if aad.len() > MAX_AAD_LEN {
            return Err(AeadError::AadTooLong);
        }
        if ciphertext_and_tag.len() < TAG_LEN
            || ciphertext_and_tag.len() > record::MAX_CIPHERTEXT_LEN
        {
            return Err(AeadError::CiphertextLength);
        }

        let split_at = ciphertext_and_tag.len() - TAG_LEN;
        let (ct, tag) = match ciphertext_and_tag.split_at_checked(split_at) {
            Some(pair) => pair,
            None => return Err(AeadError::CiphertextLength),
        };

        let j0 = build_j0(nonce);
        let expected_tag = self.compute_tag(&j0, aad, ct);
        // 復号（CTR XOR）はここより先では一切行わない。定数時間比較の結果
        // だけを見て分岐する（平文はまだ確保も生成もしていない）。
        if !ct_eq(&expected_tag, tag) {
            return Err(AeadError::TagMismatch);
        }

        let mut out = Vec::with_capacity(ct.len());
        out.extend_from_slice(ct);
        let mut counter = j0;
        inc32(&mut counter);
        ctr_xor(&self.cipher, counter, &mut out);
        Ok(out)
    }

    /// `tag = GHASH(H, A, C) ^ E_K(J0)` を計算する。
    fn compute_tag(&self, j0: &[u8; 16], aad: &[u8], ciphertext: &[u8]) -> [u8; TAG_LEN] {
        let s = ghash(self.h, aad, ciphertext);
        let mut ek_j0 = *j0;
        self.cipher.encrypt_block(&mut ek_j0);
        let ek_pair = block_to_pair(&ek_j0);
        zeroize(&mut ek_j0);
        pair_to_block(xor128(s, ek_pair))
    }

    /// テスト専用: `H` への参照（GHASH 単体テスト・ゼロ化検証に使う）。
    #[cfg(test)]
    fn h(&self) -> (u64, u64) {
        self.h
    }

    /// `H` を強制的にゼロ化する（`Drop` から呼ぶ本体。テストから直接
    /// 呼んでゼロ化を確認する）。
    fn wipe(&mut self) {
        // `H` をバイト列へ写してから `zeroize`（black_box ヒント付き）で
        // ゼロ化し、その結果を書き戻す。`self.h = (0, 0)` という素朴な代入は
        // オプティマイザに dead store として除去されうるため避ける
        // （module doc「ゼロ化の限界」参照。それでも最適化により消去が
        // 省略されない保証はない）。
        let mut bytes = pair_to_block(self.h);
        zeroize(&mut bytes);
        self.h = block_to_pair(&bytes);
    }
}

impl fmt::Debug for Aes128Gcm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Aes128Gcm")
            .field("h", &"<redacted>")
            .finish()
    }
}

impl Drop for Aes128Gcm {
    fn drop(&mut self) {
        self.wipe();
    }
}

/// `H = E_K(0^128)` を計算する。一時ブロックは使用後にゼロ化する。
fn compute_h(cipher: &Aes128) -> (u64, u64) {
    let mut block = [0u8; BLOCK_LEN];
    cipher.encrypt_block(&mut block);
    let h = block_to_pair(&block);
    zeroize(&mut block);
    h
}

/// `J0 = nonce || 0x00000001`（SP 800-38D §7.1。96 ビット nonce 専用の
/// 固定形。GHASH から J0 を導出する経路は実装しない）。
fn build_j0(nonce: &[u8; NONCE_LEN]) -> [u8; BLOCK_LEN] {
    let mut j0 = [0u8; BLOCK_LEN];
    if let Some(slot) = j0.get_mut(0..NONCE_LEN) {
        slot.copy_from_slice(nonce);
    }
    if let Some(slot) = j0.get_mut(NONCE_LEN..BLOCK_LEN) {
        slot.copy_from_slice(&1u32.to_be_bytes());
    }
    j0
}

/// SP 800-38D の `inc_32`: 128 ビットカウンタブロックの下位 32 ビットだけを
/// `wrapping_add(1)` する（上位 96 ビット＝nonce は変更しない）。呼び出し側
/// は [`MAX_PLAINTEXT_LEN`] の静的上限によりブロック数が
/// `2^32 - 2` を大きく下回ることを保証しており、実際に wrap することはない。
fn inc32(counter: &mut [u8; BLOCK_LEN]) {
    let low = counter
        .get(NONCE_LEN..BLOCK_LEN)
        .map(be_bytes_to_u32)
        .unwrap_or(0);
    let next = low.wrapping_add(1);
    if let Some(slot) = counter.get_mut(NONCE_LEN..BLOCK_LEN) {
        slot.copy_from_slice(&next.to_be_bytes());
    }
}

/// バイト列（先頭最大 4 バイトのみ使用）をビッグエンディアン `u32` へ変換する。
/// 添字直指定を避けるための組み立てで、`bytes.len() != 4` でもパニックしない。
fn be_bytes_to_u32(bytes: &[u8]) -> u32 {
    let mut v = 0u32;
    for &byte in bytes.iter().take(4) {
        v = (v << 8) | u32::from(byte);
    }
    v
}

/// バイト列（先頭最大 8 バイトのみ使用）をビッグエンディアン `u64` へ変換する。
/// 添字直指定を避けるための組み立てで、`bytes.len() != 8` でもパニックしない。
fn be_bytes_to_u64(bytes: &[u8]) -> u64 {
    let mut v = 0u64;
    for &byte in bytes.iter().take(8) {
        v = (v << 8) | u64::from(byte);
    }
    v
}

/// 16 バイトブロックをビッグエンディアンの `(hi, lo)` 2 語表現へ変換する。
/// GCM の反射順ビット表現に対応させるため、`hi` の最上位ビットがバイト 0 の
/// 最上位ビット（多項式の最高次項の係数）に対応する（module doc 参照）。
fn block_to_pair(b: &[u8; BLOCK_LEN]) -> (u64, u64) {
    let hi = b.get(0..8).map(be_bytes_to_u64).unwrap_or(0);
    let lo = b.get(8..BLOCK_LEN).map(be_bytes_to_u64).unwrap_or(0);
    (hi, lo)
}

/// [`block_to_pair`] の逆変換。
fn pair_to_block(pair: (u64, u64)) -> [u8; BLOCK_LEN] {
    let mut out = [0u8; BLOCK_LEN];
    if let Some(slot) = out.get_mut(0..8) {
        slot.copy_from_slice(&pair.0.to_be_bytes());
    }
    if let Some(slot) = out.get_mut(8..BLOCK_LEN) {
        slot.copy_from_slice(&pair.1.to_be_bytes());
    }
    out
}

/// 2 つの 128 ビット値（`(hi, lo)` 表現）の XOR。
fn xor128(a: (u64, u64), b: (u64, u64)) -> (u64, u64) {
    (a.0 ^ b.0, a.1 ^ b.1)
}

/// バイト列の 1 チャンク（最大 16 バイト）を 16 バイト境界までゼロ詰めする。
fn pad_block(chunk: &[u8]) -> [u8; BLOCK_LEN] {
    let mut buf = [0u8; BLOCK_LEN];
    let len = chunk.len().min(BLOCK_LEN);
    if let (Some(slot), Some(src)) = (buf.get_mut(..len), chunk.get(..len)) {
        slot.copy_from_slice(src);
    }
    buf
}

/// GF(2^128)（既約多項式 `x^128 + x^7 + x^2 + x + 1`）上の乗算を、
/// SP 800-38D Algorithm 1 のビット直列 shift-and-add で計算する。
///
/// `a` を MSB から 1 ビットずつ取り出し、対応するビットが立っていれば
/// アキュムレータ `z` へ `v`（初期値は `b`。反射表現のまま毎回右へ 1 ビット
/// シフトし、シフトで押し出されたビットが 1 なら還元多項式
/// `R = 0xE1 || 0^120` を条件付き XOR する）を XOR する。ループは常に 128 回
/// 固定で、`a`・`b` の値（秘密値）に依存する分岐・添字は存在しない
/// （分岐条件はすべてマスク演算 `0u64.wrapping_sub(bit)` に置き換えている）。
fn gf128_mul(a: (u64, u64), b: (u64, u64)) -> (u64, u64) {
    let mut z_hi = 0u64;
    let mut z_lo = 0u64;
    let mut v_hi = b.0;
    let mut v_lo = b.1;

    for i in 0..128u32 {
        // i は公開のループカウンタであり、この分岐は秘密値に依存しない。
        let bit = if i < 64 {
            (a.0 >> (63 - i)) & 1
        } else {
            (a.1 >> (127 - i)) & 1
        };
        let mask = 0u64.wrapping_sub(bit);
        z_hi ^= v_hi & mask;
        z_lo ^= v_lo & mask;

        let lsb = v_lo & 1;
        let carry = (v_hi & 1) << 63;
        v_lo = (v_lo >> 1) | carry;
        v_hi >>= 1;
        let rmask = 0u64.wrapping_sub(lsb);
        v_hi ^= (0xE1u64 << 56) & rmask;
    }

    (z_hi, z_lo)
}

/// AAD・暗号文・長さブロックから GHASH（`Y`）を計算する（SP 800-38D §6.4）。
/// `Y = 0` から始め、各ブロック `X` について `Y = (Y ^ X) · H` を繰り返す。
/// AAD・暗号文とも 16 バイト境界までゼロ詰めしたうえで処理し、最後に
/// `[len(A) を bit 単位で u64 BE] || [len(C) を bit 単位で u64 BE]` の
/// 長さブロックを 1 つ処理する。
fn ghash(h: (u64, u64), aad: &[u8], ciphertext: &[u8]) -> (u64, u64) {
    let mut y = (0u64, 0u64);

    for chunk in aad.chunks(BLOCK_LEN) {
        let block = block_to_pair(&pad_block(chunk));
        y = gf128_mul(xor128(y, block), h);
    }
    for chunk in ciphertext.chunks(BLOCK_LEN) {
        let block = block_to_pair(&pad_block(chunk));
        y = gf128_mul(xor128(y, block), h);
    }

    let aad_bits = (aad.len() as u64).saturating_mul(8);
    let ct_bits = (ciphertext.len() as u64).saturating_mul(8);
    y = gf128_mul(xor128(y, (aad_bits, ct_bits)), h);

    y
}

/// 4 ブロック（64 バイト）単位で CTR 鍵ストリームを生成し `data` に
/// その場で XOR する。開始カウンタは呼び出し側が [`inc32`] 済みの値
/// （`J0` の次）を渡す。鍵ストリームのブロックは使い終わったら
/// [`zeroize`] する。
fn ctr_xor(cipher: &Aes128, mut counter: [u8; BLOCK_LEN], data: &mut [u8]) {
    let mut offset = 0usize;
    while offset < data.len() {
        let c0 = counter;
        let mut c1 = c0;
        inc32(&mut c1);
        let mut c2 = c1;
        inc32(&mut c2);
        let mut c3 = c2;
        inc32(&mut c3);
        let mut blocks = [c0, c1, c2, c3];
        cipher.encrypt_blocks4(&mut blocks);

        let mut next_counter = c3;
        inc32(&mut next_counter);
        counter = next_counter;

        for lane in blocks.iter_mut() {
            if offset < data.len() {
                let take = (data.len() - offset).min(BLOCK_LEN);
                if let Some(chunk) = data.get_mut(offset..offset + take) {
                    for (d, k) in chunk.iter_mut().zip(lane.iter()) {
                        *d ^= *k;
                    }
                }
                offset += take;
            }
            zeroize(lane);
        }
    }
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

    fn array12(bytes: &[u8]) -> [u8; 12] {
        let mut out = [0u8; 12];
        out.copy_from_slice(bytes);
        out
    }

    // --- GHASH・GF(2^128) 乗算の代数的性質（テスト専用の独立参照実装との
    // 照合。分岐ありの素朴な shift-and-add を別ロジックで書き、決定的
    // PRNG で作った入力多数について本番実装と一致することを確認する）。

    fn gf128_mul_reference(a: (u64, u64), b: (u64, u64)) -> (u64, u64) {
        let mut z = (0u64, 0u64);
        let mut v = b;
        for i in 0..128 {
            let bit = if i < 64 {
                (a.0 >> (63 - i)) & 1
            } else {
                (a.1 >> (127 - i)) & 1
            };
            if bit == 1 {
                z.0 ^= v.0;
                z.1 ^= v.1;
            }
            let lsb = v.1 & 1;
            v.1 = (v.1 >> 1) | ((v.0 & 1) << 63);
            v.0 >>= 1;
            if lsb == 1 {
                v.0 ^= 0xE1u64 << 56;
            }
        }
        z
    }

    // 決定的 xorshift64（テスト専用。乱数源のためだけに使う）。
    fn xorshift64(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *state = x;
        x
    }

    #[test]
    fn gf128_mul_matches_independent_reference_for_many_inputs() {
        let mut state = 0x9e3779b97f4a7c15u64;
        for _ in 0..200 {
            let a = (xorshift64(&mut state), xorshift64(&mut state));
            let b = (xorshift64(&mut state), xorshift64(&mut state));
            assert_eq!(gf128_mul(a, b), gf128_mul_reference(a, b));
        }
    }

    #[test]
    fn gf128_mul_identity_element_leaves_value_unchanged() {
        // GCM の反射表現での乗法単位元は先頭バイトが 0x80 の block
        // （多項式の x^0 項の係数が 1、他はすべて 0）。
        let one = block_to_pair(&array16(&hex_decode("80000000000000000000000000000000")));
        let mut state = 0x1234_5678_9abc_def0u64;
        for _ in 0..50 {
            let a = (xorshift64(&mut state), xorshift64(&mut state));
            assert_eq!(gf128_mul(a, one), a);
            assert_eq!(gf128_mul(one, a), a);
        }
    }

    #[test]
    fn gf128_mul_is_commutative() {
        let mut state = 0xabcdef0123456789u64;
        for _ in 0..50 {
            let a = (xorshift64(&mut state), xorshift64(&mut state));
            let b = (xorshift64(&mut state), xorshift64(&mut state));
            assert_eq!(gf128_mul(a, b), gf128_mul(b, a));
        }
    }

    #[test]
    fn gf128_mul_distributes_over_xor() {
        let mut state = 0x0f0f0f0f0f0f0f0fu64;
        for _ in 0..50 {
            let a = (xorshift64(&mut state), xorshift64(&mut state));
            let b = (xorshift64(&mut state), xorshift64(&mut state));
            let c = (xorshift64(&mut state), xorshift64(&mut state));
            let lhs = gf128_mul(a, xor128(b, c));
            let rhs = xor128(gf128_mul(a, b), gf128_mul(a, c));
            assert_eq!(lhs, rhs);
        }
    }

    #[test]
    fn inc32_wraps_only_low_32_bits() {
        // nonce（上位 96 ビット）は不変で、下位 32 ビットが 0xffffffff から
        // 0x00000000 へラップすることを確認する。
        let mut counter = array16(&hex_decode("00112233445566778899aabbffffffff"));
        inc32(&mut counter);
        assert_eq!(hex(&counter), "00112233445566778899aabb00000000");
    }

    // McGrew & Viega, "The Galois/Counter Mode of Operation", Appendix B
    // （AES-128 の Test Case 1〜4。key は全 4 ケース共通）。
    fn test_key() -> [u8; 16] {
        array16(&hex_decode("00000000000000000000000000000000"))
    }

    // Test Case 1: P・AAD ともに空。
    #[test]
    fn seal_matches_gcm_spec_test_case_1() {
        let gcm = Aes128Gcm::new(&test_key());
        let nonce = array12(&hex_decode("000000000000000000000000"));
        let out = gcm.seal(&nonce, &[], &[]).expect("valid inputs");
        assert_eq!(out.len(), TAG_LEN);
        assert_eq!(hex(&out), "58e2fccefa7e3061367f1d57a4e7455a");
    }

    // Test Case 2: 16 バイトのゼロ平文・AAD なし。
    #[test]
    fn seal_matches_gcm_spec_test_case_2() {
        let gcm = Aes128Gcm::new(&test_key());
        let nonce = array12(&hex_decode("000000000000000000000000"));
        let plaintext = hex_decode("00000000000000000000000000000000");
        let out = gcm.seal(&nonce, &[], &plaintext).expect("valid inputs");
        let (ct, tag) = out.split_at(out.len() - TAG_LEN);
        assert_eq!(hex(ct), "0388dace60b6a392f328c2b971b2fe78");
        assert_eq!(hex(tag), "ab6e47d42cec13bdf53a67b21257bddf");
    }

    // Test Case 3: 64 バイト平文（鍵・nonce も専用の値）・AAD なし。
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

    // Test Case 4: 60 バイト平文・20 バイト AAD（ブロック長の非整数倍）。
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
        let ciphertext = gcm.seal(&nonce, &aad, &plaintext).expect("valid inputs");
        let recovered = gcm.open(&nonce, &aad, &ciphertext).expect("valid tag");
        assert_eq!(recovered, plaintext);
    }

    #[test]
    fn open_rejects_bit_flip_in_ciphertext() {
        let key = [0x11u8; 16];
        let gcm = Aes128Gcm::new(&key);
        let nonce = [0x22u8; 12];
        let plaintext = b"hello, tls 1.3 aead world!!".to_vec();
        let mut sealed = gcm.seal(&nonce, b"aad", &plaintext).expect("valid inputs");
        if let Some(byte) = sealed.first_mut() {
            *byte ^= 0x01;
        }
        assert_eq!(
            gcm.open(&nonce, b"aad", &sealed),
            Err(AeadError::TagMismatch)
        );
    }

    #[test]
    fn open_rejects_bit_flip_in_tag() {
        let key = [0x33u8; 16];
        let gcm = Aes128Gcm::new(&key);
        let nonce = [0x44u8; 12];
        let plaintext = b"another aead payload".to_vec();
        let mut sealed = gcm.seal(&nonce, b"", &plaintext).expect("valid inputs");
        if let Some(byte) = sealed.last_mut() {
            *byte ^= 0x80;
        }
        assert_eq!(gcm.open(&nonce, b"", &sealed), Err(AeadError::TagMismatch));
    }

    #[test]
    fn open_rejects_wrong_aad() {
        let key = [0x55u8; 16];
        let gcm = Aes128Gcm::new(&key);
        let nonce = [0x66u8; 12];
        let plaintext = b"payload with aad binding".to_vec();
        let sealed = gcm
            .seal(&nonce, b"correct-aad", &plaintext)
            .expect("valid inputs");
        assert_eq!(
            gcm.open(&nonce, b"wrong-aad!!!", &sealed),
            Err(AeadError::TagMismatch)
        );
    }

    #[test]
    fn open_rejects_wrong_nonce() {
        let key = [0x77u8; 16];
        let gcm = Aes128Gcm::new(&key);
        let nonce = [0x88u8; 12];
        let mut other_nonce = nonce;
        if let Some(b) = other_nonce.first_mut() {
            *b ^= 0x01;
        }
        let plaintext = b"nonce binding check".to_vec();
        let sealed = gcm.seal(&nonce, b"", &plaintext).expect("valid inputs");
        assert_eq!(
            gcm.open(&other_nonce, b"", &sealed),
            Err(AeadError::TagMismatch)
        );
    }

    #[test]
    fn open_rejects_lengths_shorter_than_tag() {
        let gcm = Aes128Gcm::new(&[0u8; 16]);
        let nonce = [0u8; 12];
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
    fn seal_accepts_max_plaintext_and_rejects_one_more() {
        let gcm = Aes128Gcm::new(&[0u8; 16]);
        let nonce = [0u8; 12];
        let ok = vec![0u8; MAX_PLAINTEXT_LEN];
        assert!(gcm.seal(&nonce, &[], &ok).is_ok());

        let too_long = vec![0u8; MAX_PLAINTEXT_LEN + 1];
        assert_eq!(
            gcm.seal(&nonce, &[], &too_long),
            Err(AeadError::PlaintextTooLong)
        );
    }

    #[test]
    fn seal_and_open_reject_aad_too_long() {
        let gcm = Aes128Gcm::new(&[0u8; 16]);
        let nonce = [0u8; 12];
        let aad = vec![0u8; MAX_AAD_LEN + 1];
        assert_eq!(gcm.seal(&nonce, &aad, &[]), Err(AeadError::AadTooLong));

        let sealed = gcm.seal(&nonce, &[], b"x").expect("valid inputs");
        assert_eq!(gcm.open(&nonce, &aad, &sealed), Err(AeadError::AadTooLong));
    }

    #[test]
    fn open_rejects_ciphertext_longer_than_max_record() {
        let gcm = Aes128Gcm::new(&[0u8; 16]);
        let nonce = [0u8; 12];
        let buf = vec![0u8; record::MAX_CIPHERTEXT_LEN + 1];
        assert_eq!(
            gcm.open(&nonce, &[], &buf),
            Err(AeadError::CiphertextLength)
        );
    }

    #[test]
    fn seal_and_open_are_deterministic_for_same_inputs() {
        let gcm = Aes128Gcm::new(&[0x99u8; 16]);
        let nonce = [0x01u8; 12];
        let plaintext = b"deterministic check".to_vec();
        let a = gcm.seal(&nonce, b"aad", &plaintext).expect("valid inputs");
        let b = gcm.seal(&nonce, b"aad", &plaintext).expect("valid inputs");
        assert_eq!(a, b);
    }

    #[test]
    fn debug_output_does_not_expose_h() {
        let gcm = Aes128Gcm::new(&[0xaau8; 16]);
        let rendered = format!("{gcm:?}");
        assert!(rendered.contains("redacted"));
    }

    #[test]
    fn wipe_zeroes_h() {
        let mut gcm = Aes128Gcm::new(&[0xbbu8; 16]);
        assert_ne!(gcm.h(), (0, 0));
        gcm.wipe();
        assert_eq!(gcm.h(), (0, 0));
    }

    #[test]
    fn alert_description_mapping() {
        assert_eq!(AeadError::TagMismatch.alert_description(), 20);
        assert_eq!(AeadError::CiphertextLength.alert_description(), 20);
        assert_eq!(AeadError::PlaintextTooLong.alert_description(), 80);
        assert_eq!(AeadError::AadTooLong.alert_description(), 80);
    }

    // 手動専用のスループット参考値計測（`aes.rs::tests::aes128_throughput_reference`
    // と同じ流儀）。固定値のアサートはせず、`docs/design/tls-aes-gcm.md` へ
    // 手動で記録する参考値を出力する。採否の根拠にはしない
    // （`docs/design/benchmark-judgement-policy.md` 参照）。
    //
    // 実行: cargo test -p fandhe-db-wire-server --release \
    //   aes128_gcm_throughput_reference -- --ignored --nocapture
    #[test]
    #[ignore]
    fn aes128_gcm_throughput_reference() {
        let gcm = Aes128Gcm::new(&[0x5au8; 16]);
        let nonce = [0x5bu8; NONCE_LEN];
        let plaintext = vec![0x5cu8; MAX_PLAINTEXT_LEN];

        const ITERATIONS: u32 = 2_000;
        let start = std::time::Instant::now();
        for _ in 0..ITERATIONS {
            let _ = gcm.seal(&nonce, b"aad", &plaintext).expect("valid inputs");
        }
        let elapsed = start.elapsed();
        let bytes_processed = (ITERATIONS as u64) * (plaintext.len() as u64);
        let mb_per_sec = (bytes_processed as f64 / 1_000_000.0) / elapsed.as_secs_f64();
        println!(
            "aes128_gcm_throughput_reference: {bytes_processed} bytes in {elapsed:?} ({mb_per_sec:.2} MB/s, 共有環境の参考値・採否根拠にしない)"
        );
    }
}
