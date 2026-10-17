//! `BYTEA` 列型（TABLE-13・TASK-197。関連: WIRE-13・NOSQL-17）向けの
//! テキスト表現（PostgreSQL 既定の `\x` 16 進形式）の解析・出力を集約する。
//!
//! `catalog.rs::ColumnType::Bytea`（カタログ往復）・`row_codec.rs::Value::Bytes`
//! （行バイト表現）とは独立に、SQL リテラル（`sql/parser.rs`）と wire のテキスト
//! 応答（`wire-server::result_encoder`）が共有する変換だけをここへ置く
//! （engine と wire-server の双方から到達できるよう `engine::bytea` として公開する）。
//!
//! 受理する hex 形式は PostgreSQL の `\x` 接頭辞形式の最小部分集合に限る
//! （`\x`／`\X` 接頭辞＋偶数個の 16 進数字。空白・escape 形式・接頭辞省略は拒否。
//! 曖昧さを避ける fail-closed な実装既定値）。

use std::fmt;

/// `BYTEA` 列 1 個分のバイト長上限。行バイト表現（`row_codec::Value::Bytes`）の
/// 上限と同値を維持する（片方だけの変更を防ぐため下部の const assert で強制する）。
pub const MAX_BYTEA_FIELD_LEN: u32 = 4 * 1024 * 1024;

const _: () = assert!(
    MAX_BYTEA_FIELD_LEN == crate::row_codec::MAX_TEXT_FIELD_LEN,
    "bytea::MAX_BYTEA_FIELD_LEN must stay in sync with row_codec::MAX_TEXT_FIELD_LEN"
);

/// `parse_hex_text` の失敗種別。値そのものは含めない（`.claude/rules/security.md`
/// 「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」と同じ方針で、
/// 長さ・種別のみを保持する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteaTextError {
    /// 復号後の長さが [`MAX_BYTEA_FIELD_LEN`] を超える（確保より前に入力長から
    /// 判定するため、実際に確保が発生することはない）。
    TooLong,
    /// `\x`／`\X` 接頭辞を持たない。
    MissingPrefix,
    /// 16 進本体の桁数が奇数。
    OddLength,
    /// 16 進アルファベット外の文字を含む。
    InvalidDigit,
}

impl fmt::Display for ByteaTextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ByteaTextError::TooLong => write!(f, "bytea hex literal too long"),
            ByteaTextError::MissingPrefix => write!(f, "bytea hex literal missing \\x prefix"),
            ByteaTextError::OddLength => write!(f, "bytea hex literal has odd digit count"),
            ByteaTextError::InvalidDigit => write!(f, "bytea hex literal has invalid digit"),
        }
    }
}

impl std::error::Error for ByteaTextError {}

/// 1 桁の 16 進文字を 4 bit 値へ変換する。ASCII の大小文字を両方受理する。
fn hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// PostgreSQL `\x` 16 進形式の文字列をバイト列へ解析する（B4）。
///
/// 受理: `\x`／`\X` 接頭辞＋偶数個の 16 進数字（大小文字混在可）。`\x` のみ
/// （本体 0 桁）は空バイト列として受理する。
/// 拒否: 接頭辞なし・奇数桁・非 16 進文字・空白混在（`MissingPrefix`／
/// `OddLength`／`InvalidDigit`）。
///
/// untrusted 入力を扱うため、確保（`Vec::with_capacity`）より前に入力長から
/// 上限超過を判定する（`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。
pub fn parse_hex_text(s: &str) -> Result<Vec<u8>, ByteaTextError> {
    // 復号後の長さは高々 `(s.len() - 2) / 2` であり、`s.len()` の上限判定だけで
    // 確保前に MAX_BYTEA_FIELD_LEN 超過を検出できる（`2 * MAX + 2` 超なら
    // 復号後も必ず MAX を超える）。`checked_*` がオーバーフローした場合も
    // fail-closed に拒否する（黙って上限検査を素通りさせない）。
    let max_input_len = (MAX_BYTEA_FIELD_LEN as usize)
        .checked_mul(2)
        .and_then(|v| v.checked_add(2))
        .ok_or(ByteaTextError::TooLong)?;
    if s.len() > max_input_len {
        return Err(ByteaTextError::TooLong);
    }

    // 添字アクセス（`[]`）を使わず `get()` とスライスパターンのみで判定する
    // （受信データ経路。`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。
    let bytes = s.as_bytes();
    let prefix_ok = matches!(bytes.get(..2), Some([b'\\', b'x' | b'X']));
    if !prefix_ok {
        return Err(ByteaTextError::MissingPrefix);
    }
    let hex_body = bytes.get(2..).unwrap_or(&[]);
    if hex_body.len() % 2 != 0 {
        return Err(ByteaTextError::OddLength);
    }

    let out_len = hex_body.len() / 2;
    let mut out = Vec::new();
    out.try_reserve_exact(out_len)
        .map_err(|_| ByteaTextError::TooLong)?;
    // `as_chunks::<2>().0`（`row_codec.rs` の f32 デコードと同じ書き方）は
    // 固定長配列パターンで境界を静的に保証し、添字アクセスを使わない。
    for [hi_byte, lo_byte] in hex_body.as_chunks::<2>().0 {
        let hi = hex_digit(*hi_byte).ok_or(ByteaTextError::InvalidDigit)?;
        let lo = hex_digit(*lo_byte).ok_or(ByteaTextError::InvalidDigit)?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

/// [`parse_hex_text`] の逆変換。`\x` ＋ 小文字 16 進（PostgreSQL の
/// `bytea_output=hex` 既定と同じ表現。B5）。
pub fn format_hex_text(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("\\x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// [`parse_cast_text`] の失敗種別。値は含めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteaCastError {
    /// 復号後の長さが [`MAX_BYTEA_FIELD_LEN`] を超える。
    TooLong,
    /// `\x` 形式の本体が奇数桁（PostgreSQL は `22023`）。
    OddLength,
    /// `\x` 形式の本体に 16 進以外の文字（空白を含む）がある（PostgreSQL は `22023`）。
    InvalidDigit,
    /// escape 形式の不正（孤立した `\`、`\` の後が `\` でも 3 桁の 8 進でもない。`22P02`）。
    InvalidSyntax,
}

/// `CAST(<文字列> AS bytea)` の入力関数（Issue #1592・SQL-47。`sql::cast_value` が呼ぶ）。
///
/// PostgreSQL の `byteain` に従い、`\x` 始まりは 16 進形式、それ以外は escape 形式
/// （生バイト・`\\`・`\ooo`〔先頭桁 0〜3 の 8 進 3 桁〕）として読む。`'abc'` は `\x616263`。
/// 16 進本体の空白は PostgreSQL では許容されるが、本エンジンは [`parse_hex_text`] と揃えて
/// 拒否する（fail-closed。既知の差分）。INSERT のリテラル束縛（[`parse_hex_text`]）は変えない。
///
/// 復号後の長さは入力長以下のため、上限は確保前に入力長から判定する。
pub fn parse_cast_text(s: &str) -> Result<Vec<u8>, ByteaCastError> {
    let bytes = s.as_bytes();
    let max = MAX_BYTEA_FIELD_LEN as usize;
    if matches!(bytes.get(..2), Some([b'\\', b'x'])) {
        let body = bytes.get(2..).unwrap_or(&[]);
        if body.len() / 2 > max {
            return Err(ByteaCastError::TooLong);
        }
        if body.len() % 2 != 0 {
            return Err(ByteaCastError::OddLength);
        }
        let mut out = Vec::new();
        out.try_reserve_exact(body.len() / 2)
            .map_err(|_| ByteaCastError::TooLong)?;
        for [hi, lo] in body.as_chunks::<2>().0 {
            let hi = hex_digit(*hi).ok_or(ByteaCastError::InvalidDigit)?;
            let lo = hex_digit(*lo).ok_or(ByteaCastError::InvalidDigit)?;
            out.push((hi << 4) | lo);
        }
        return Ok(out);
    }
    // escape 形式。出力は入力以下の長さのため、入力長で確保前に上限を判定する
    // （入力が上限を超える場合は `\\`・`\ooo` で縮んでも拒否する。fail-closed の既知の差分）。
    if bytes.len() > max {
        return Err(ByteaCastError::TooLong);
    }
    let mut out = Vec::new();
    out.try_reserve_exact(bytes.len())
        .map_err(|_| ByteaCastError::TooLong)?;
    let mut i = 0usize;
    while let Some(&c) = bytes.get(i) {
        if c != b'\\' {
            out.push(c);
            i += 1;
            continue;
        }
        match (bytes.get(i + 1), bytes.get(i + 2), bytes.get(i + 3)) {
            (Some(b'\\'), _, _) => {
                out.push(b'\\');
                i += 2;
            }
            (Some(&a @ b'0'..=b'3'), Some(&b @ b'0'..=b'7'), Some(&d @ b'0'..=b'7')) => {
                out.push(((a - b'0') << 6) | ((b - b'0') << 3) | (d - b'0'));
                i += 4;
            }
            _ => return Err(ByteaCastError::InvalidSyntax),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cast_text_reads_escape_form_like_postgres() {
        assert_eq!(parse_cast_text("abc").unwrap(), b"abc".to_vec());
        assert_eq!(parse_cast_text("").unwrap(), Vec::<u8>::new());
        assert_eq!(parse_cast_text("a\\\\b").unwrap(), b"a\\b".to_vec());
        assert_eq!(parse_cast_text("\\101\\000").unwrap(), vec![0x41, 0x00]);
        assert_eq!(parse_cast_text("\\377").unwrap(), vec![0xff]);
    }

    #[test]
    fn cast_text_hex_form_and_errors() {
        assert_eq!(parse_cast_text("\\x616263").unwrap(), b"abc".to_vec());
        assert_eq!(parse_cast_text("\\x").unwrap(), Vec::<u8>::new());
        assert_eq!(parse_cast_text("\\xabc"), Err(ByteaCastError::OddLength));
        assert_eq!(parse_cast_text("\\xzz"), Err(ByteaCastError::InvalidDigit));
        assert_eq!(parse_cast_text("\\x a"), Err(ByteaCastError::InvalidDigit));
    }

    #[test]
    fn cast_text_rejects_malformed_escapes() {
        for bad in ["\\", "a\\", "\\9", "\\400", "\\1", "\\18x", "\\q"] {
            assert_eq!(
                parse_cast_text(bad),
                Err(ByteaCastError::InvalidSyntax),
                "{bad}"
            );
        }
    }

    #[test]
    fn parse_accepts_lower_upper_and_mixed_case() {
        assert_eq!(
            parse_hex_text("\\xdeadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(
            parse_hex_text("\\XDEADBEEF").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
        assert_eq!(
            parse_hex_text("\\xDeAdBeEf").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
    }

    #[test]
    fn parse_accepts_empty_body() {
        assert_eq!(parse_hex_text("\\x").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn parse_rejects_missing_prefix() {
        assert_eq!(
            parse_hex_text("deadbeef"),
            Err(ByteaTextError::MissingPrefix)
        );
        assert_eq!(parse_hex_text(""), Err(ByteaTextError::MissingPrefix));
    }

    #[test]
    fn parse_rejects_odd_length() {
        assert_eq!(parse_hex_text("\\xabc"), Err(ByteaTextError::OddLength));
    }

    #[test]
    fn parse_rejects_invalid_digit() {
        assert_eq!(parse_hex_text("\\xzz"), Err(ByteaTextError::InvalidDigit));
        // 偶数桁（4 文字）のまま非 16 進文字（空白）を混入させる。
        assert_eq!(parse_hex_text("\\xa cd"), Err(ByteaTextError::InvalidDigit));
    }

    #[test]
    fn parse_rejects_over_limit_by_length_before_allocation() {
        let huge = "\\x".to_string() + &"a".repeat((MAX_BYTEA_FIELD_LEN as usize) * 2 + 1);
        assert_eq!(parse_hex_text(&huge), Err(ByteaTextError::TooLong));
    }

    #[test]
    fn parse_accepts_exact_limit() {
        let body_len = (MAX_BYTEA_FIELD_LEN as usize) * 2;
        let exact = "\\x".to_string() + &"ab".repeat(MAX_BYTEA_FIELD_LEN as usize);
        assert_eq!(exact.len(), 2 + body_len);
        let decoded = parse_hex_text(&exact).unwrap();
        assert_eq!(decoded.len(), MAX_BYTEA_FIELD_LEN as usize);
    }

    #[test]
    fn format_outputs_lowercase_hex_with_prefix() {
        assert_eq!(format_hex_text(&[0xde, 0xad, 0xbe, 0xef]), "\\xdeadbeef");
        assert_eq!(format_hex_text(&[]), "\\x");
    }

    #[test]
    fn round_trip_parse_then_format() {
        let bytes: Vec<u8> = (0..=255u8).collect();
        let text = format_hex_text(&bytes);
        assert_eq!(parse_hex_text(&text).unwrap(), bytes);
    }
}
