//! `bit(n)` 列（固定長ビット列。TABLE-48・Issue #2021）の値表現・解析・整形。
//!
//! `catalog::ColumnType::Bit(n)` の値を保持する [`BitString`]（所有）と [`BitRef`]（行バイト列
//! からの借用）を提供する。`row_codec`（行の encode/decode・既定値束縛）、`sql::parser`・
//! `sql::copy`（文字列リテラル `'1010'` の束縛）、wire-server の結果整形・NoSQL の型付き JSON
//! から呼ばれる。距離カーネル（popcount 等）は後続 Issue の担当で、本モジュールは
//! ビット順を固定するだけに留める。
//!
//! # 表現の不変条件
//!
//! - ビット数 `len` は `1..=`[`MAX_BIT_LENGTH`]（カタログ側の宣言上限と同じ）。
//! - `bytes.len() == ceil(len / 8)`。ビット順は MSB 先頭（PostgreSQL の内部表現と同じ）。
//! - 末尾の詰め物ビット（最終バイトのうち `len` を超える下位ビット）は常に 0。
//!   行 codec は詰め物ビットが非 0 のバイト列を decode で拒否する（fail-closed）。
//!
//! # エラー文言
//!
//! 入力値・テナント・表の情報を反射しない固定の英語文字列だけを返す（untrusted 入力の
//! エコーバック防止）。長さの検証はアロケーションより先に行う。

/// `bit(n)` の `n` の上限（`catalog` の型宣言検証と共有する）。
///
/// 行 codec のスカラーペイロード上限（`row_codec::MAX_SCALAR_PAYLOAD_LEN`）に、フレーミング
/// （presence 1 + ビット長 4 バイト）込みの 1 値が収まる最大ビット数。宣言できる最大長の列に
/// 実際に INSERT／UPDATE／COPY できることを保証する（PostgreSQL の上限より小さい）。
pub const MAX_BIT_LENGTH: u32 = (crate::row_codec::MAX_SCALAR_PAYLOAD_LEN - 5) * 8;

/// `n` ビットを収めるバイト数（`ceil(n / 8)`）。`n` が `usize` に収まらない場合のみ `None`。
pub fn byte_len_for_bits(len: u32) -> Option<usize> {
    let len = usize::try_from(len).ok()?;
    len.checked_add(7).map(|v| v / 8)
}

/// ビット列の解析・検証の失敗種別。SQLSTATE への写像は呼び出し側が行う
/// （[`BitError::InvalidDigit`] は `22P02`、[`BitError::LengthMismatch`] は `22026`、
/// [`BitError::InvalidLayout`] は内部表現の破損〔行 codec が `RowCodecError` に写す〕）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitError {
    /// `0`・`1` 以外の文字を含む（`22P02`）。
    InvalidDigit,
    /// 入力のビット数が宣言長 `n` と異なる（`22026`）。
    LengthMismatch,
    /// 内部表現の不整合（長さ 0・上限超過・バイト数不一致・詰め物ビットが非 0）。
    InvalidLayout,
}

impl BitError {
    /// 固定の英語メッセージ（入力値を含まない）。
    pub const fn message(self) -> &'static str {
        match self {
            BitError::InvalidDigit => "invalid input syntax for type bit: only 0 and 1 are allowed",
            BitError::LengthMismatch => "bit string length does not match type bit(n)",
            BitError::InvalidLayout => "invalid bit string representation",
        }
    }
}

/// 末尾の詰め物ビットが 0 で、バイト数が `len` と整合しているか検証する。
fn validate_layout(len: u32, bytes: &[u8]) -> Result<(), BitError> {
    if len == 0 || len > MAX_BIT_LENGTH {
        return Err(BitError::InvalidLayout);
    }
    let expected = byte_len_for_bits(len).ok_or(BitError::InvalidLayout)?;
    if bytes.len() != expected {
        return Err(BitError::InvalidLayout);
    }
    let used = len % 8;
    if used != 0 {
        let pad_mask = 0xFFu8 >> used;
        match bytes.last() {
            Some(last) if last & pad_mask == 0 => {}
            _ => return Err(BitError::InvalidLayout),
        }
    }
    Ok(())
}

/// `bit(n)` 値（所有）。フィールドは非公開で、構築は検証付きの関数に限る。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BitString {
    len: u32,
    bytes: Vec<u8>,
}

impl BitString {
    /// テキスト表現（`0`・`1` の並び）を宣言長 `declared_len` の値として解析する。
    ///
    /// 長さの照合を先に行い、`declared_len` と一致しない入力ではバイト列を確保しない。
    /// `declared_len` は呼び出し側（カタログ）が `1..=MAX_BIT_LENGTH` を保証するが、
    /// 範囲外なら [`BitError::InvalidLayout`] で拒否する。
    pub fn parse_text(s: &str, declared_len: u32) -> Result<Self, BitError> {
        if declared_len == 0 || declared_len > MAX_BIT_LENGTH {
            return Err(BitError::InvalidLayout);
        }
        // 文字種の検査（`0`/`1` 以外は長さに関係なく 22P02）。PostgreSQL と同じく
        // 文字種エラーを長さエラーより優先する。
        if s.bytes().any(|b| b != b'0' && b != b'1') {
            return Err(BitError::InvalidDigit);
        }
        if u64::try_from(s.len()).ok() != Some(u64::from(declared_len)) {
            return Err(BitError::LengthMismatch);
        }
        let n = byte_len_for_bits(declared_len).ok_or(BitError::InvalidLayout)?;
        let mut bytes = vec![0u8; n];
        for (i, b) in s.bytes().enumerate() {
            if b == b'1' {
                let slot = bytes.get_mut(i / 8).ok_or(BitError::InvalidLayout)?;
                *slot |= 0x80 >> (i % 8);
            }
        }
        Ok(BitString {
            len: declared_len,
            bytes,
        })
    }

    /// 内部表現（ビット数 + MSB 先頭のバイト列）から構築する。不変条件を検証する。
    pub fn from_parts(len: u32, bytes: Vec<u8>) -> Result<Self, BitError> {
        validate_layout(len, &bytes)?;
        Ok(BitString { len, bytes })
    }

    /// ビット数。
    pub fn bit_len(&self) -> u32 {
        self.len
    }

    /// MSB 先頭のバイト列（末尾の詰め物ビットは 0）。
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// 借用形へ変換する。
    pub fn as_bit_ref(&self) -> BitRef<'_> {
        BitRef {
            len: self.len,
            bytes: &self.bytes,
        }
    }

    /// `0`・`1` の文字列へ整形する。
    pub fn format_text(&self) -> String {
        self.as_bit_ref().format_text()
    }
}

/// `bit(n)` 値の借用形（行バイト列を指す）。`row_codec::ScalarRef::Bit` が保持する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitRef<'a> {
    len: u32,
    bytes: &'a [u8],
}

impl<'a> BitRef<'a> {
    /// 検証付きで借用形を作る（`row_codec` の decode が呼ぶ。不変条件は [`BitString`] と同じ）。
    pub fn new(len: u32, bytes: &'a [u8]) -> Result<Self, BitError> {
        validate_layout(len, bytes)?;
        Ok(BitRef { len, bytes })
    }

    /// ビット数。
    pub fn bit_len(&self) -> u32 {
        self.len
    }

    /// MSB 先頭のバイト列。
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// 所有形へ複製する（検証済みのため失敗しない前提だが、不整合は `Err`）。
    pub fn to_owned_value(&self) -> Result<BitString, BitError> {
        BitString::from_parts(self.len, self.bytes.to_vec())
    }

    /// `0`・`1` の文字列へ整形する。
    pub fn format_text(&self) -> String {
        let mut out = String::new();
        let total = usize::try_from(self.len).unwrap_or(0);
        out.reserve(total);
        for i in 0..total {
            let set = self
                .bytes
                .get(i / 8)
                .is_some_and(|b| b & (0x80 >> (i % 8)) != 0);
            out.push(if set { '1' } else { '0' });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_format_round_trip() {
        for s in [
            "1",
            "0",
            "1010",
            "10101010",
            "101010101",
            "0000000011111111",
        ] {
            let n = u32::try_from(s.len()).expect("len");
            let v = BitString::parse_text(s, n).expect("parse");
            assert_eq!(v.format_text(), s);
            assert_eq!(v.bit_len(), n);
            assert_eq!(v.as_bytes().len(), byte_len_for_bits(n).expect("bytes"));
        }
    }

    #[test]
    fn msb_first_with_zero_padding() {
        let v = BitString::parse_text("1010", 4).expect("parse");
        assert_eq!(v.as_bytes(), &[0b1010_0000]);
        let v = BitString::parse_text("101010101", 9).expect("parse");
        assert_eq!(v.as_bytes(), &[0b1010_1010, 0b1000_0000]);
    }

    #[test]
    fn rejects_bad_digit_before_length() {
        assert_eq!(
            BitString::parse_text("10a1", 4),
            Err(BitError::InvalidDigit)
        );
        assert_eq!(BitString::parse_text("1a", 4), Err(BitError::InvalidDigit));
        assert_eq!(
            BitString::parse_text("１０", 2),
            Err(BitError::InvalidDigit)
        );
    }

    #[test]
    fn rejects_length_mismatch() {
        assert_eq!(
            BitString::parse_text("101", 4),
            Err(BitError::LengthMismatch)
        );
        assert_eq!(
            BitString::parse_text("10101", 4),
            Err(BitError::LengthMismatch)
        );
        assert_eq!(BitString::parse_text("", 1), Err(BitError::LengthMismatch));
    }

    #[test]
    fn rejects_out_of_range_declared_length() {
        assert_eq!(BitString::parse_text("", 0), Err(BitError::InvalidLayout));
        assert_eq!(
            BitString::parse_text("1", MAX_BIT_LENGTH + 1),
            Err(BitError::InvalidLayout)
        );
    }

    #[test]
    fn layout_validation_rejects_nonzero_padding_and_bad_length() {
        assert!(BitString::from_parts(4, vec![0b1010_0000]).is_ok());
        assert_eq!(
            BitString::from_parts(4, vec![0b1010_0001]),
            Err(BitError::InvalidLayout)
        );
        assert_eq!(
            BitString::from_parts(9, vec![0xFF]),
            Err(BitError::InvalidLayout)
        );
        assert_eq!(
            BitString::from_parts(8, vec![0xFF, 0x00]),
            Err(BitError::InvalidLayout)
        );
        assert_eq!(
            BitString::from_parts(0, Vec::new()),
            Err(BitError::InvalidLayout)
        );
        assert!(BitRef::new(8, &[0xFF]).is_ok());
        assert!(BitRef::new(MAX_BIT_LENGTH + 1, &[]).is_err());
    }

    #[test]
    fn error_messages_do_not_echo_input() {
        for e in [
            BitError::InvalidDigit,
            BitError::LengthMismatch,
            BitError::InvalidLayout,
        ] {
            assert!(!e.message().is_empty());
        }
    }
}
