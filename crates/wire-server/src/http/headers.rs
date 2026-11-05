//! HTTP ヘッダ部（要求行直後の `name: value` 行群と終端空行）の解析。
//!
//! [`crate::http::request::parse_request_line`] が返す `consumed` オフセット
//! 以降（要求行を除いたバイト列の先頭）を受け取り、終端空行（`\r\n\r\n` の
//! 2 個目の `\r\n`）までを 1 回で解析する純関数を提供する。ストリームからの
//! 有界読み取り・EOF 判断は呼び出し元（Issue #747 の接続ハンドラ）が担い、
//! 本モジュールが返す `consumed`（ヘッダ部先頭からの消費バイト数）は本文の
//! 開始位置を指す。後続の `Content-Type`・本文長検証（Issue #742）・
//! `Authorization` の解釈（`crate::http::session::bearer`。Issue #753・#754）・`Idempotency-Key` の取り出し
//! （`crate::http::idempotency_key`。Issue #2063）はいずれも [`Headers::get_single`] 等
//! 本モジュールの公開 API 経由でヘッダへアクセスする。
//!
//! ヘッダ部合計バイト数・個数の上限（[`MAX_HEADER_SECTION_LEN`]・
//! [`MAX_HEADER_COUNT`]）は実行時設定・CLI での緩和経路を持たない固定定数
//! （[`crate::http::request::MAX_REQUEST_LINE_LEN`] と同じ方針）。
//! `Content-Length` は必須・一意（同値の重複も拒否）・digits-only とし、
//! `Transfer-Encoding` は値を問わず拒否することで、CL.TE／TE.CL 型の
//! request smuggling を構造的に排除する（HTTP-2, HTTP-11）。
//!
//! 受信データ経路のため `unwrap`/`expect`/添字アクセス（`[]`）を用いず、
//! `get()`・`checked_*` で処理する。`Headers` は固定長配列（`Vec` を使わない）
//! でヒープ確保を行わず、名前・値は入力バッファからの借用のみを保持する。
//!
//! 対応: TASK-173（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア HTTP-2,
//! HTTP-11）。

use crate::framing::FrameError;
use crate::http::request::find_crlf_within;

/// ヘッダ部（各行の CRLF・終端空行の CRLF を含む）の上限バイト数（HTTP-11）。
///
/// 実行時設定・CLI での緩和経路を持たない固定定数。
pub const MAX_HEADER_SECTION_LEN: usize = 8 * 1024;

/// ヘッダ行数の上限（HTTP-11）。実行時設定・CLI での緩和経路を持たない固定定数。
pub const MAX_HEADER_COUNT: usize = 32;

const TRANSFER_ENCODING: &[u8] = b"transfer-encoding";
const CONTENT_LENGTH: &[u8] = b"content-length";
const IDEMPOTENCY_KEY: &[u8] = b"idempotency-key";

/// 解析済みヘッダ群。名前・値はいずれも入力バッファからの借用であり、
/// コピー・ヒープ確保を行わない（`[Option<_>; MAX_HEADER_COUNT]` は `Copy`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Headers<'a> {
    entries: [Option<(&'a [u8], &'a [u8])>; MAX_HEADER_COUNT],
    len: usize,
    content_length: usize,
}

impl<'a> Headers<'a> {
    /// 一意性検証済みの `Content-Length`（本文の宣言バイト数）。
    ///
    /// 1 MiB 等の上限判定は行わない（Issue #742 の責務。本層は「必須・一意・
    /// digits-only・`usize` に収まる」ことのみを保証する）。
    pub fn content_length(&self) -> usize {
        self.content_length
    }

    /// 受理したヘッダの件数。
    pub fn len(&self) -> usize {
        self.len
    }

    /// ヘッダが 1 件も無いか（終端行のみだった場合。`Content-Length` 必須の
    /// 規則では到達せず、GET の省略許可規則でのみ起こりうる。`len` との対で提供する）。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 宣言順で (名前, 値) を反復する。
    pub fn iter(&self) -> impl Iterator<Item = (&'a [u8], &'a [u8])> + '_ {
        self.entries
            .iter()
            .take(self.len)
            .filter_map(|entry| *entry)
    }

    /// `name` を ASCII 大文字小文字非区別で照合し、ちょうど 1 件だけ一致すれば
    /// その値（前後の OWS はトリム済み）を返す。
    ///
    /// 0 件は `Ok(None)`、2 件以上（重複ヘッダ）は `Malformed`（fail-closed）。
    /// `Content-Type`（Issue #742）・`Authorization`（`crate::http::session::bearer`。
    /// Issue #753・#754）が消費する
    /// 想定の共通 API。
    pub fn get_single(&self, name: &[u8]) -> Result<Option<&'a [u8]>, FrameError> {
        let mut found: Option<&'a [u8]> = None;
        for (entry_name, entry_value) in self.iter() {
            if entry_name.eq_ignore_ascii_case(name) {
                if found.is_some() {
                    return Err(FrameError::Malformed("duplicate header"));
                }
                found = Some(entry_value);
            }
        }
        Ok(found)
    }

    /// `Idempotency-Key` ヘッダの生の値（OWS 除去済みの借用）を取り出す薄い包み
    /// （Issue #2063・HTTP-14 (2) ポインタ）。
    ///
    /// 0 件は `Ok(None)`、2 件以上は `08P01`（[`Self::get_single`] の一般則）。
    /// 長さ・文字の検証は本層では行わず、`crate::http::idempotency_key` が
    /// `OperationId::parse` で担う（本モジュールは engine に依存しない）。
    pub fn idempotency_key(&self) -> Result<Option<&'a [u8]>, FrameError> {
        self.get_single(IDEMPOTENCY_KEY)
    }
}

/// [`parse_headers`] の結果。
///
/// `Complete` と `Incomplete` の間でサイズ差が大きい（`Headers` が固定長配列
/// のため）が、`Headers` をヒープへ逃がすと本モジュールの「ヒープ確保なし」
/// 方針（[`crate::http::request`] と同じ）に反するため、意図して
/// `large_enum_variant` を許容する。
#[derive(Debug, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum HeaderParse<'a> {
    /// 終端空行まで解析できた。`consumed` はヘッダ部先頭（`input[0]`）からの
    /// 消費バイト数（終端空行の CRLF 直後＝本文先頭）。
    Complete {
        headers: Headers<'a>,
        consumed: usize,
    },
    /// 上限バイト数未満の範囲で終端空行に到達していない（まだ全部届いて
    /// いない）。読み取り継続・EOF 判断は呼び出し元（Issue #747）の責務。
    Incomplete,
}

/// ヘッダ部を解析する。`input` はヘッダ部の先頭（要求行の
/// [`RequestLineParse::Complete::consumed`](crate::http::request::RequestLineParse::Complete)
/// 以降）を指すスライスで、本文が続いていてもよい（無視し `consumed` で
/// 境界だけを返す）。
pub fn parse_headers(input: &[u8]) -> Result<HeaderParse<'_>, FrameError> {
    parse_headers_with(input, ContentLengthRule::Required)
}

/// `Content-Length` 欠落の扱い。本文を持たない `GET /v1/openapi.json`（Issue #2244）
/// だけが [`ContentLengthRule::OptionalDefaultZero`] を使い、`POST` は常に `Required`。
/// 重複・非数値・`Transfer-Encoding` の拒否は規則によらず不変（request smuggling 面を増やさない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentLengthRule {
    /// 欠落は `08P01`（従来挙動）。
    Required,
    /// 欠落は宣言長 0 として扱う。
    OptionalDefaultZero,
}

/// [`parse_headers`] の規則指定版。接続ハンドラがメソッドから規則を決めて呼ぶ。
pub fn parse_headers_with(
    input: &[u8],
    rule: ContentLengthRule,
) -> Result<HeaderParse<'_>, FrameError> {
    let mut entries: [Option<(&[u8], &[u8])>; MAX_HEADER_COUNT] = [None; MAX_HEADER_COUNT];
    let mut count = 0usize;
    let mut pos = 0usize;
    let mut content_length: Option<usize> = None;

    loop {
        let remaining = input
            .get(pos..)
            .ok_or(FrameError::Malformed("header scan position out of range"))?;
        let window = MAX_HEADER_SECTION_LEN
            .checked_sub(pos)
            .ok_or(FrameError::Malformed("header section position overflow"))?;
        let lf_idx = match find_crlf_within(remaining, window)? {
            Some(idx) => idx,
            None => {
                if input.len() >= MAX_HEADER_SECTION_LEN {
                    return Err(FrameError::Malformed("header section exceeds limit"));
                }
                return Ok(HeaderParse::Incomplete);
            }
        };
        let cr_idx = lf_idx
            .checked_sub(1)
            .ok_or(FrameError::Malformed("header line missing CR"))?;
        let line = remaining
            .get(..cr_idx)
            .ok_or(FrameError::Malformed("header line slice out of range"))?;
        let line_end = pos
            .checked_add(lf_idx)
            .and_then(|v| v.checked_add(1))
            .ok_or(FrameError::Malformed("header line end overflow"))?;

        if line.is_empty() {
            // 終端空行（CRLF のみ）に到達。
            let content_length = match (content_length, rule) {
                (Some(v), _) => v,
                (None, ContentLengthRule::OptionalDefaultZero) => 0,
                (None, ContentLengthRule::Required) => {
                    return Err(FrameError::Malformed("missing content-length"));
                }
            };
            let consumed = line_end;
            if consumed > MAX_HEADER_SECTION_LEN {
                return Err(FrameError::Malformed("header section exceeds limit"));
            }
            return Ok(HeaderParse::Complete {
                headers: Headers {
                    entries,
                    len: count,
                    content_length,
                },
                consumed,
            });
        }

        let next_count = count
            .checked_add(1)
            .ok_or(FrameError::Malformed("header count overflow"))?;
        if next_count > MAX_HEADER_COUNT {
            return Err(FrameError::Malformed("too many headers"));
        }

        let (name, value) = split_header_line(line)?;

        if name.eq_ignore_ascii_case(TRANSFER_ENCODING) {
            return Err(FrameError::Malformed("transfer-encoding not supported"));
        }

        if name.eq_ignore_ascii_case(CONTENT_LENGTH) {
            if content_length.is_some() {
                return Err(FrameError::Malformed("duplicate content-length"));
            }
            content_length = Some(parse_content_length(value)?);
        }

        let slot = entries
            .get_mut(count)
            .ok_or(FrameError::Malformed("header slot out of range"))?;
        *slot = Some((name, value));
        count = next_count;

        pos = line_end;
    }
}

/// ヘッダ 1 行（CRLF を除いた `name: value` 部分）を `name`／`value` へ分解する。
///
/// - 行頭が SP／HTAB（obs-fold・継続行）の場合は拒否する
/// - `:` を含まない場合は拒否する
/// - `name` は非空・全バイトが RFC 9110 tchar（`!#$%&'*+-.^_\`|~` と英数字）で
///   なければ拒否する（SP・HTAB・制御バイト・非 ASCII を含め拒否）
/// - `value` は前後の OWS（SP／HTAB）をトリムした残りが全て可視 ASCII
///   （`0x20..=0x7E`）または HTAB でなければ拒否する（空値は許可）
fn split_header_line(line: &[u8]) -> Result<(&[u8], &[u8]), FrameError> {
    let first = line
        .first()
        .copied()
        .ok_or(FrameError::Malformed("empty header line"))?;
    if first == b' ' || first == b'\t' {
        return Err(FrameError::Malformed("header line starts with obs-fold"));
    }
    let colon_idx = line
        .iter()
        .position(|&b| b == b':')
        .ok_or(FrameError::Malformed("header line missing colon"))?;
    let name = line
        .get(..colon_idx)
        .ok_or(FrameError::Malformed("header name slice out of range"))?;
    let after_colon = colon_idx
        .checked_add(1)
        .ok_or(FrameError::Malformed("header name length overflow"))?;
    let raw_value = line
        .get(after_colon..)
        .ok_or(FrameError::Malformed("header value slice out of range"))?;

    validate_header_name(name)?;
    let value = trim_ows(raw_value);
    validate_header_value(value)?;

    Ok((name, value))
}

/// RFC 9110 の tchar 集合（トークン文字）かを判定する。
fn is_tchar(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// ヘッダ名が非空・全バイト tchar であることを検証する。
fn validate_header_name(name: &[u8]) -> Result<(), FrameError> {
    if name.is_empty() {
        return Err(FrameError::Malformed("empty header name"));
    }
    if !name.iter().all(|&b| is_tchar(b)) {
        return Err(FrameError::Malformed("header name contains invalid byte"));
    }
    Ok(())
}

/// 先頭・末尾の SP／HTAB（OWS）をトリムする。
fn trim_ows(value: &[u8]) -> &[u8] {
    let start = value
        .iter()
        .position(|&b| b != b' ' && b != b'\t')
        .unwrap_or(value.len());
    let trimmed_start = value.get(start..).unwrap_or(&[]);
    let end = trimmed_start
        .iter()
        .rposition(|&b| b != b' ' && b != b'\t')
        .map(|idx| idx + 1)
        .unwrap_or(0);
    trimmed_start.get(..end).unwrap_or(&[])
}

/// OWS トリム済みのヘッダ値が全バイト可視 ASCII（`0x20..=0x7E`）または HTAB
/// であることを検証する（制御バイト・非 ASCII を拒否）。空値は許可する。
fn validate_header_value(value: &[u8]) -> Result<(), FrameError> {
    if value
        .iter()
        .all(|&b| b == b'\t' || (0x20..=0x7e).contains(&b))
    {
        Ok(())
    } else {
        Err(FrameError::Malformed("header value contains invalid byte"))
    }
}

/// `Content-Length` の値を検証・数値化する。非空・ASCII 数字のみ（先頭の
/// `+`/`-`・空白混在・空文字列は拒否）を要求し、`u64` へ桁ごとに
/// `checked_mul`/`checked_add` で累積したうえで `usize` へ収まることを
/// 確認する（オーバーフローは fail-closed に拒否）。
///
/// 1 MiB 等の絶対上限判定は行わない（Issue #742 の責務）。
fn parse_content_length(value: &[u8]) -> Result<usize, FrameError> {
    if value.is_empty() {
        return Err(FrameError::Malformed("empty content-length"));
    }
    let mut acc: u64 = 0;
    for &byte in value {
        if !byte.is_ascii_digit() {
            return Err(FrameError::Malformed("content-length is not numeric"));
        }
        let digit = u64::from(byte - b'0');
        acc = acc
            .checked_mul(10)
            .and_then(|v| v.checked_add(digit))
            .ok_or(FrameError::Malformed("content-length overflow"))?;
    }
    usize::try_from(acc).map_err(|_| FrameError::Malformed("content-length overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::request::{parse_request_line, RequestLineParse};
    use engine::error_format::ErrorClass;

    fn assert_protocol_violation(err: &FrameError) {
        assert!(matches!(err, FrameError::Malformed(_)));
        assert_eq!(err.sqlstate(), Some("08P01"));
        assert_eq!(err.error_class(), Some(ErrorClass::ProtocolViolation));
    }

    fn body_after(headers: &[u8]) -> Vec<u8> {
        let mut input = headers.to_vec();
        input.extend_from_slice(b"BODY");
        input
    }

    #[test]
    fn accepts_minimal_headers_with_content_length_only() {
        let input = body_after(b"Content-Length: 0\r\n\r\n");
        match parse_headers(&input).expect("parse should succeed") {
            HeaderParse::Complete { headers, consumed } => {
                assert_eq!(headers.content_length(), 0);
                assert_eq!(headers.len(), 1);
                assert_eq!(&input[consumed..], b"BODY");
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn accepts_content_length_with_surrounding_ows() {
        let input = body_after(b"Content-Length:  42  \r\n\r\n");
        match parse_headers(&input).expect("parse should succeed") {
            HeaderParse::Complete { headers, .. } => {
                assert_eq!(headers.content_length(), 42);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn matches_header_name_case_insensitively() {
        for name in ["content-length", "CONTENT-LENGTH", "Content-Length"] {
            let mut input = Vec::new();
            input.extend_from_slice(name.as_bytes());
            input.extend_from_slice(b": 7\r\n\r\n");
            match parse_headers(&input).expect("parse should succeed") {
                HeaderParse::Complete { headers, .. } => {
                    assert_eq!(headers.content_length(), 7);
                }
                other => panic!("expected Complete, got {other:?}"),
            }
        }
    }

    #[test]
    fn allows_empty_value_for_non_reserved_header() {
        let input = body_after(b"X-Foo:\r\nContent-Length: 0\r\n\r\n");
        match parse_headers(&input).expect("parse should succeed") {
            HeaderParse::Complete { headers, .. } => {
                assert_eq!(
                    headers.get_single(b"x-foo").expect("single lookup"),
                    Some(&b""[..])
                );
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn get_single_returns_none_for_absent_header() {
        let input = body_after(b"Content-Length: 0\r\n\r\n");
        match parse_headers(&input).expect("parse should succeed") {
            HeaderParse::Complete { headers, .. } => {
                assert_eq!(headers.get_single(b"host").expect("single lookup"), None);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn get_single_rejects_duplicate_non_reserved_header() {
        let input = body_after(b"X-Foo: a\r\nX-Foo: b\r\nContent-Length: 0\r\n\r\n");
        match parse_headers(&input).expect("parse should succeed") {
            HeaderParse::Complete { headers, .. } => {
                let err = headers.get_single(b"x-foo").unwrap_err();
                assert_protocol_violation(&err);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn idempotency_key_absent_is_none_and_name_is_case_insensitive() {
        let input = body_after(b"Content-Length: 0\r\n\r\n");
        match parse_headers(&input).expect("parse should succeed") {
            HeaderParse::Complete { headers, .. } => {
                assert_eq!(headers.idempotency_key().expect("lookup"), None);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
        for name in ["IDEMPOTENCY-KEY", "idempotency-key", "Idempotency-Key"] {
            let raw = format!("{name}: k1\r\nContent-Length: 0\r\n\r\n");
            match parse_headers(raw.as_bytes()).expect("parse should succeed") {
                HeaderParse::Complete { headers, .. } => {
                    assert_eq!(headers.idempotency_key().expect("lookup"), Some(&b"k1"[..]));
                }
                other => panic!("expected Complete, got {other:?}"),
            }
        }
    }

    #[test]
    fn idempotency_key_duplicate_is_protocol_violation() {
        for raw in [
            &b"Idempotency-Key: a\r\nIdempotency-Key: a\r\nContent-Length: 0\r\n\r\n"[..],
            &b"Idempotency-Key: a\r\nidempotency-key: b\r\nContent-Length: 0\r\n\r\n"[..],
        ] {
            match parse_headers(raw).expect("parse should succeed") {
                HeaderParse::Complete { headers, .. } => {
                    let err = headers.idempotency_key().unwrap_err();
                    assert_protocol_violation(&err);
                }
                other => panic!("expected Complete, got {other:?}"),
            }
        }
    }

    #[test]
    fn iter_yields_headers_in_declaration_order() {
        let input = body_after(b"A: 1\r\nB: 2\r\nContent-Length: 0\r\n\r\n");
        match parse_headers(&input).expect("parse should succeed") {
            HeaderParse::Complete { headers, .. } => {
                let collected: Vec<(&[u8], &[u8])> = headers.iter().collect();
                assert_eq!(
                    collected,
                    vec![
                        (&b"A"[..], &b"1"[..]),
                        (&b"B"[..], &b"2"[..]),
                        (&b"Content-Length"[..], &b"0"[..]),
                    ]
                );
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn consumed_offset_lines_up_with_request_line_parser() {
        let input = b"POST /v1/query HTTP/1.1\r\nContent-Length: 4\r\n\r\nBODY";
        let (line_consumed, _line) = match parse_request_line(input).expect("line parse ok") {
            RequestLineParse::Complete { line, consumed } => (consumed, line),
            other => panic!("expected Complete, got {other:?}"),
        };
        let header_input = &input[line_consumed..];
        match parse_headers(header_input).expect("header parse ok") {
            HeaderParse::Complete { headers, consumed } => {
                assert_eq!(headers.content_length(), 4);
                assert_eq!(&header_input[consumed..], b"BODY");
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn accepts_header_section_at_exact_limit() {
        // 固定ヘッダ（Content-Length + 終端空行）を除いた残りを 1 本の長い
        // X-Pad 値で埋め、ヘッダ部合計をちょうど MAX_HEADER_SECTION_LEN
        // バイトにする（受理側の境界）。
        let fixed_prefix = b"Content-Length: 0\r\nX-Pad: ".as_slice();
        let fixed_suffix = b"\r\n\r\n".as_slice();
        let filler_len = MAX_HEADER_SECTION_LEN - fixed_prefix.len() - fixed_suffix.len();
        let mut input = Vec::with_capacity(MAX_HEADER_SECTION_LEN);
        input.extend_from_slice(fixed_prefix);
        input.extend(std::iter::repeat_n(b'a', filler_len));
        input.extend_from_slice(fixed_suffix);
        assert_eq!(input.len(), MAX_HEADER_SECTION_LEN);

        match parse_headers(&input).expect("parse should succeed") {
            HeaderParse::Complete { consumed, .. } => {
                assert_eq!(consumed, MAX_HEADER_SECTION_LEN);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn rejects_header_section_over_limit() {
        let fixed_prefix = b"Content-Length: 0\r\nX-Pad: ".as_slice();
        let fixed_suffix = b"\r\n\r\n".as_slice();
        // ちょうど 1 バイト超過させる。
        let filler_len = MAX_HEADER_SECTION_LEN - fixed_prefix.len() - fixed_suffix.len() + 1;
        let mut input = Vec::with_capacity(MAX_HEADER_SECTION_LEN + 1);
        input.extend_from_slice(fixed_prefix);
        input.extend(std::iter::repeat_n(b'a', filler_len));
        input.extend_from_slice(fixed_suffix);
        assert_eq!(input.len(), MAX_HEADER_SECTION_LEN + 1);

        let err = parse_headers(&input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn incomplete_when_header_section_under_limit_without_terminator() {
        let input = vec![b'a'; MAX_HEADER_SECTION_LEN - 1];
        assert_eq!(
            parse_headers(&input).expect("should not error while incomplete"),
            HeaderParse::Incomplete
        );
    }

    #[test]
    fn rejects_header_section_at_or_over_limit_without_terminator() {
        let input = vec![b'a'; MAX_HEADER_SECTION_LEN];
        let err = parse_headers(&input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn incomplete_on_empty_input() {
        assert_eq!(
            parse_headers(b"").expect("should not error"),
            HeaderParse::Incomplete
        );
    }

    #[test]
    fn incomplete_when_trailing_cr_is_at_window_end_under_limit() {
        let input = b"Content-Length: 0\r";
        assert_eq!(
            parse_headers(input).expect("should not error while incomplete"),
            HeaderParse::Incomplete
        );
    }

    #[test]
    fn accepts_header_count_at_exact_limit() {
        let mut input = Vec::new();
        for i in 0..(MAX_HEADER_COUNT - 1) {
            input.extend_from_slice(format!("X-H{i}: v\r\n").as_bytes());
        }
        input.extend_from_slice(b"Content-Length: 0\r\n\r\n");
        match parse_headers(&input).expect("parse should succeed") {
            HeaderParse::Complete { headers, .. } => {
                assert_eq!(headers.len(), MAX_HEADER_COUNT);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn rejects_header_count_over_limit() {
        let mut input = Vec::new();
        for i in 0..MAX_HEADER_COUNT {
            input.extend_from_slice(format!("X-H{i}: v\r\n").as_bytes());
        }
        input.extend_from_slice(b"Content-Length: 0\r\n\r\n");
        let err = parse_headers(&input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_missing_content_length() {
        let input = b"X-Foo: bar\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_content_length_non_numeric() {
        let input = b"Content-Length: abc\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_content_length_negative() {
        let input = b"Content-Length: -1\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_content_length_leading_plus() {
        let input = b"Content-Length: +1\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_content_length_comma_list() {
        let input = b"Content-Length: 5, 5\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_content_length_empty_value() {
        let input = b"Content-Length:\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_content_length_internal_space() {
        let input = b"Content-Length: 1 0\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_content_length_u64_overflow() {
        // 21 桁は u64（最大 20 桁）を超える。
        let input = b"Content-Length: 999999999999999999999\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn accepts_content_length_at_u64_max() {
        // u64::MAX は digits-only 検証・u64 累積のいずれも通過し、64bit 環境
        // では usize::MAX と一致するため受理される（オーバーフロー処理の
        // 境界を u64 側から確認する）。
        let input = b"Content-Length: 18446744073709551615\r\n\r\n";
        match parse_headers(input).expect("parse should succeed on 64bit") {
            HeaderParse::Complete { headers, .. } => {
                assert_eq!(headers.content_length(), u64::MAX as usize);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn rejects_duplicate_content_length_same_value() {
        let input = b"Content-Length: 5\r\nContent-Length: 5\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_duplicate_content_length_different_value() {
        let input = b"Content-Length: 5\r\nContent-Length: 6\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_transfer_encoding_chunked() {
        let input = b"Content-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_transfer_encoding_identity() {
        let input = b"Content-Length: 0\r\nTransfer-Encoding: identity\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_transfer_encoding_empty_value() {
        let input = b"Content-Length: 0\r\nTransfer-Encoding:\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_transfer_encoding_lowercase_name() {
        let input = b"Content-Length: 0\r\ntransfer-encoding: chunked\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_transfer_encoding_without_content_length() {
        let input = b"Transfer-Encoding: chunked\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_obs_fold_leading_space() {
        let input = b"Content-Length: 0\r\n X-Foo: bar\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_obs_fold_leading_tab() {
        let input = b"Content-Length: 0\r\n\tX-Foo: bar\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_missing_colon() {
        let input = b"Content-Length 0\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_empty_header_name() {
        let input = b"Content-Length: 0\r\n: bar\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_header_name_with_space() {
        let input = b"Content-Length: 0\r\nX Foo: bar\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_header_name_with_non_tchar() {
        let input = b"Content-Length: 0\r\nX\"Foo: bar\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_header_value_with_control_byte() {
        let input = b"Content-Length: 0\r\nX-Foo: b\x00r\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_header_value_with_del_byte() {
        let input = b"Content-Length: 0\r\nX-Foo: b\x7Fr\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_header_value_with_non_ascii_byte() {
        let input = b"Content-Length: 0\r\nX-Foo: b\xC3r\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_bare_lf() {
        let input = b"Content-Length: 0\nX-Foo: bar\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_bare_cr() {
        let input = b"Content-Length: 0\rX-Foo: bar\r\n\r\n";
        let err = parse_headers(input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn optional_rule_defaults_missing_content_length_to_zero() {
        let input = b"Host: x\r\n\r\n";
        match parse_headers_with(input, ContentLengthRule::OptionalDefaultZero).unwrap() {
            HeaderParse::Complete { headers, .. } => assert_eq!(headers.content_length(), 0),
            other => panic!("expected Complete, got {other:?}"),
        }
        assert!(parse_headers_with(input, ContentLengthRule::Required).is_err());
        assert!(parse_headers(input).is_err());
    }

    #[test]
    fn optional_rule_still_rejects_duplicates_and_transfer_encoding() {
        for raw in [
            &b"Content-Length: 0\r\nContent-Length: 0\r\n\r\n"[..],
            b"Content-Length: abc\r\n\r\n",
            b"Transfer-Encoding: chunked\r\n\r\n",
        ] {
            assert!(parse_headers_with(raw, ContentLengthRule::OptionalDefaultZero).is_err());
        }
    }
}
