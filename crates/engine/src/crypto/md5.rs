//! 自作 MD5（RFC 1321）実装。
//!
//! 役割: SQL 組み込み関数 `md5(text)`（Issue #1905・SQL-65・TASK-308 のポインタ）の
//! ダイジェスト計算だけを担う。呼び出し元は `sql::string_fn::md5`（16 進化）で、
//! それを `sql::udf_call::apply_builtin` が呼ぶ。engine 内専用（`pub(crate)`）で、
//! 公開 API は増やさない。
//!
//! 注意: MD5 は衝突耐性が破られており、PostgreSQL 互換の SQL 関数のためだけに
//! 使う。認証（SCRAM）・TLS channel binding・台帳の内容ハッシュ（SHA-256）には使わない。
//!
//! 依存追加なし。`unsafe` なし。添字 `[]` を使わず、パディングは最大 128 バイトの
//! スタック固定バッファで組み立てる（入力長に比例するヒープ確保はしない）。

/// 各ラウンドのシフト量（RFC 1321 3.4 節）。
const S: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9,
    14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15,
    21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// 加算定数 `floor(2^32 * abs(sin(i + 1)))`（テストで再計算して照合する）。
const K: [u32; 64] = [
    0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501,
    0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821,
    0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8,
    0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, 0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a,
    0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70,
    0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665,
    0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
    0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
];

/// 1 ブロック（64 バイト）ぶんの圧縮関数。
fn compress(state: &mut [u32; 4], block: &[u8; 64]) {
    let mut m = [0u32; 16];
    for (slot, word) in m.iter_mut().zip(block.as_chunks::<4>().0.iter()) {
        *slot = u32::from_le_bytes(*word);
    }
    let [mut a, mut b, mut c, mut d] = *state;
    for i in 0..64usize {
        let (f, g) = match i / 16 {
            0 => ((b & c) | (!b & d), i),
            1 => ((d & b) | (!d & c), (5 * i + 1) & 15),
            2 => (b ^ c ^ d, (3 * i + 5) & 15),
            _ => (c ^ (b | !d), (7 * i) & 15),
        };
        let sum = a
            .wrapping_add(f)
            .wrapping_add(K.get(i).copied().unwrap_or(0))
            .wrapping_add(m.get(g).copied().unwrap_or(0));
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(sum.rotate_left(S.get(i).copied().unwrap_or(0)));
    }
    let [s0, s1, s2, s3] = state;
    *s0 = s0.wrapping_add(a);
    *s1 = s1.wrapping_add(b);
    *s2 = s2.wrapping_add(c);
    *s3 = s3.wrapping_add(d);
}

/// `data` の MD5 ダイジェスト（16 バイト）を返す。
pub(crate) fn digest(data: &[u8]) -> [u8; 16] {
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let (blocks, rest) = data.as_chunks::<64>();
    for block in blocks {
        compress(&mut state, block);
    }
    // 末尾: 残り + 0x80 + 0 埋め + ビット長（LE 8 バイト）。最大 2 ブロック。
    let mut tail = [0u8; 128];
    for (dst, src) in tail.iter_mut().zip(rest.iter()) {
        *dst = *src;
    }
    let rlen = rest.len();
    if let Some(b) = tail.get_mut(rlen) {
        *b = 0x80;
    }
    let total = if rlen < 56 { 64 } else { 128 };
    let bits = (data.len() as u64).wrapping_mul(8).to_le_bytes();
    if let Some(dst) = tail.get_mut(total - 8..total) {
        dst.copy_from_slice(&bits);
    }
    for block in tail.as_chunks::<64>().0.iter().take(total / 64) {
        compress(&mut state, block);
    }
    let mut out = [0u8; 16];
    for (dst, w) in out.as_chunks_mut::<4>().0.iter_mut().zip(state.iter()) {
        *dst = w.to_le_bytes();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(d: [u8; 16]) -> String {
        d.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn k_table_matches_sine_definition() {
        for (i, k) in K.iter().enumerate() {
            let expect = ((i as f64 + 1.0).sin().abs() * 4_294_967_296.0) as u64 as u32;
            assert_eq!(*k, expect, "K[{i}]");
        }
    }

    #[test]
    fn rfc1321_test_suite() {
        let cases: [(&str, &str); 7] = [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "c3fcd3d76192e4007dfb496cca67e13b",
            ),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ];
        for (input, expect) in cases {
            assert_eq!(hex(digest(input.as_bytes())), expect, "input={input:?}");
        }
    }

    #[test]
    fn padding_boundaries_and_multibyte() {
        assert_eq!(hex(digest(&[b'a'; 55])), "ef1772b6dff9a122358552954ad0df65");
        assert_eq!(hex(digest(&[b'a'; 56])), "3b0c8ac703f828b04c6c197006d17218");
        assert_eq!(hex(digest(&[b'a'; 63])), "b06521f39153d618550606be297466d5");
        assert_eq!(hex(digest(&[b'a'; 64])), "014842d480b571495a4a0363793f7367");
        assert_eq!(hex(digest(&[b'a'; 65])), "c743a45e0d2e6a95cb859adae0248435");
        assert_eq!(
            hex(digest(&[b'a'; 119])),
            "8a7bd0732ed6a28ce75f6dabc90e1613"
        );
        assert_eq!(
            hex(digest(&[b'a'; 120])),
            "5f61c0ccad4cac44c75ff505e1f1e537"
        );
        assert_eq!(
            hex(digest(&[b'a'; 128])),
            "e510683b3f5ffe4093d021808bc6ff70"
        );
        assert_eq!(
            hex(digest(&[b'a'; 1000])),
            "cabe45dcc9ae5b66ba86600cca6b8ba8"
        );
        assert_eq!(
            hex(digest("日本語テキスト".as_bytes())),
            "291a033f45232d75458716bcc03d90a0"
        );
        assert_eq!(
            hex(digest(&vec![b'x'; 1 << 20])),
            "b561f87202d04959e37588ee05cf5b10"
        );
    }
}
