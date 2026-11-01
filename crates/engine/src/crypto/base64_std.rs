//! RFC 4648 標準アルファベット・padding 必須の base64 エンコード／厳格デコード。
//!
//! wire-server の `auth::scram`（Issue #940・WIRE-18・TASK-222）と engine の `scram_verifier` の
//! salt・StoredKey／ServerKey・nonce・proof のエンコードに使う。依存追加なし
//! （`.claude/rules/dependency-policy.md`）。デコードは untrusted 入力
//! （wire 経由の client-first/client-final メッセージ）を扱うため、
//! 非正規のパディング・末尾ビット非ゼロ・不正文字はすべて `Err` とする
//! fail-closed 設計（`.claude/rules/coding-rust.md`）。

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// `ALPHABET` は 64 要素固定であり、呼び出し元はすべて `0..64` に収まる値
/// しか渡さない（6 ビット値の符号化）。それでも untrusted 入力の扱い規約
/// （`.claude/rules/coding-rust.md`）に合わせ添字アクセスを避け、範囲外は
/// 到達不能な `unwrap_or` フォールバックとして扱う。
fn alphabet_char(sextet: u8) -> char {
    ALPHABET.get(sextet as usize).copied().unwrap_or(b'A') as char
}

pub fn encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let Some(b0) = chunk.first().copied() else {
            continue;
        };
        let b1 = chunk.get(1).copied();
        let b2 = chunk.get(2).copied();

        let c0 = b0 >> 2;
        let c1 = ((b0 & 0x03) << 4) | (b1.unwrap_or(0) >> 4);
        out.push(alphabet_char(c0));
        out.push(alphabet_char(c1));

        match (b1, b2) {
            (Some(b1), Some(b2)) => {
                let c2 = ((b1 & 0x0f) << 2) | (b2 >> 6);
                let c3 = b2 & 0x3f;
                out.push(alphabet_char(c2));
                out.push(alphabet_char(c3));
            }
            (Some(b1), None) => {
                let c2 = (b1 & 0x0f) << 2;
                out.push(alphabet_char(c2));
                out.push('=');
            }
            (None, _) => {
                out.push('=');
                out.push('=');
            }
        }
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
pub struct DecodeError;

fn decode_symbol(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// 厳格デコード: 4 文字単位・padding 必須・非正規のパディング配置や
/// 末尾の非ゼロビットを拒否する（RFC 4648 §3.5 の canonical encoding 要求）。
pub fn decode(input: &str) -> Result<Vec<u8>, DecodeError> {
    let bytes = input.as_bytes();
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if !bytes.len().is_multiple_of(4) {
        return Err(DecodeError);
    }

    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let chunk_count = bytes.len() / 4;
    for (i, chunk) in bytes.chunks(4).enumerate() {
        let is_last = i == chunk_count - 1;
        let c0 = chunk.first().copied().ok_or(DecodeError)?;
        let c1 = chunk.get(1).copied().ok_or(DecodeError)?;
        let c2 = chunk.get(2).copied().ok_or(DecodeError)?;
        let c3 = chunk.get(3).copied().ok_or(DecodeError)?;

        if !is_last && (c0 == b'=' || c1 == b'=' || c2 == b'=' || c3 == b'=') {
            return Err(DecodeError);
        }

        let v0 = decode_symbol(c0).ok_or(DecodeError)?;
        let v1 = decode_symbol(c1).ok_or(DecodeError)?;

        if c2 == b'=' {
            // 末尾チャンクが `XX==` の形（1 出力バイト）であること。
            if c3 != b'=' || !is_last {
                return Err(DecodeError);
            }
            // 末尾 4 ビットが非ゼロなら非正規（デコード結果が一意でない）。
            if v1 & 0x0f != 0 {
                return Err(DecodeError);
            }
            out.push((v0 << 2) | (v1 >> 4));
            continue;
        }

        let v2 = decode_symbol(c2).ok_or(DecodeError)?;
        out.push((v0 << 2) | (v1 >> 4));

        if c3 == b'=' {
            if !is_last {
                return Err(DecodeError);
            }
            // 末尾 2 ビットが非ゼロなら非正規。
            if v2 & 0x03 != 0 {
                return Err(DecodeError);
            }
            out.push(((v1 & 0x0f) << 4) | (v2 >> 2));
            continue;
        }

        let v3 = decode_symbol(c3).ok_or(DecodeError)?;
        out.push(((v1 & 0x0f) << 4) | (v2 >> 2));
        out.push(((v2 & 0x03) << 6) | v3);
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_various_lengths() {
        for len in 0..40 {
            let input: Vec<u8> = (0..len).map(|i| (i * 7 % 256) as u8).collect();
            let encoded = encode(&input);
            assert_eq!(decode(&encoded).expect("decode"), input, "len={len}");
        }
    }

    #[test]
    fn matches_known_vectors() {
        assert_eq!(encode(b"f"), "Zg==");
        assert_eq!(encode(b"fo"), "Zm8=");
        assert_eq!(encode(b"foo"), "Zm9v");
        assert_eq!(encode(b"foob"), "Zm9vYg==");
        assert_eq!(encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(encode(b"foobar"), "Zm9vYmFy");

        assert_eq!(decode("Zg==").expect("decode"), b"f");
        assert_eq!(decode("Zm8=").expect("decode"), b"fo");
        assert_eq!(decode("Zm9v").expect("decode"), b"foo");
        assert_eq!(decode("Zm9vYg==").expect("decode"), b"foob");
        assert_eq!(decode("Zm9vYmE=").expect("decode"), b"fooba");
        assert_eq!(decode("Zm9vYmFy").expect("decode"), b"foobar");
    }

    #[test]
    fn rejects_missing_padding() {
        assert_eq!(decode("Zg"), Err(DecodeError));
        assert_eq!(decode("Zg="), Err(DecodeError));
    }

    #[test]
    fn rejects_invalid_characters() {
        assert_eq!(decode("Zg!="), Err(DecodeError));
        assert_eq!(decode("ああああ"), Err(DecodeError));
    }

    #[test]
    fn rejects_padding_in_middle_chunk() {
        assert_eq!(decode("Zg==Zm9v"), Err(DecodeError));
    }

    #[test]
    fn rejects_non_canonical_trailing_bits() {
        // "Zh==" decodes b0='Z'(25) c1='h'(33) -> v1 & 0x0f = 33 & 0x0f = 1 != 0
        assert_eq!(decode("Zh=="), Err(DecodeError));
    }
}
