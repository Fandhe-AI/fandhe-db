//! HTTP 本文（`Content-Length` バイト分の要求本体）を読み取る **前** に行う、
//! `Content-Type` の限定と本文長の絶対上限判定。
//!
//! [`crate::http::headers::Headers`]（Issue #741）が保証する「`Content-Length`
//! 必須・一意・digits-only・`usize` に収まる」宣言長を消費し、[`plan_body`] を
//! 通過した場合にのみ得られる [`BodyPlan`] からしか本文バッファの確保サイズを
//! 取り出せない構造にすることで、未検証の宣言長がそのままアロケーションへ渡る
//! 経路（DoS）を構造的に閉じる。接続ハンドラ本体（Issue #747）は本モジュールを
//! 「ヘッダ→本文サイズ決定」の唯一の入口として使い、実際のストリーム読み取り・
//! `Expect: 100-continue`・宣言長と実送信バイト数の不一致処理はその責務のまま
//! 残す（本モジュールは純関数＋型のみで、`TcpStream` にも本文バイト列にも
//! 触れない）。
//!
//! 読み取り後の本文バイト列を UTF-8 文字列へ昇格する [`body_as_utf8`] も本
//! モジュールが提供する（JSON としての妥当性は後段の `engine::json` が判定し、
//! 本モジュールは UTF-8 か否かのみを見る）。
//!
//! 受信データ経路のため `unwrap`/`expect`/添字アクセス（`[]`）を用いない。
//!
//! 対応: TASK-173（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア HTTP-3,
//! HTTP-11）。

use crate::framing::FrameError;
use crate::http::headers::Headers;
use engine::error_format::ErrorClass;

/// 本文長の絶対上限（HTTP-11）。実行時設定・CLI での緩和経路を持たない固定
/// 定数（[`crate::http::request::MAX_REQUEST_LINE_LEN`]・
/// [`crate::http::headers::MAX_HEADER_SECTION_LEN`] と同じ方針）。
///
/// pg wire 経路の [`crate::framing::MAX_MESSAGE_LEN`] と現時点で同値だが、
/// 別表層・別契約のためエイリアスにはしない。
pub const MAX_BODY_LEN: usize = 1024 * 1024;

const CONTENT_TYPE: &[u8] = b"content-type";
const MEDIA_TYPE_JSON: &[u8] = b"application/json";
const CHARSET_PARAM: &[u8] = b"charset";
const CHARSET_UTF8: &[u8] = b"utf-8";

/// `Content-Type` 検証・本文長上限判定の両方を通過した宣言長の証明。
///
/// フィールド非公開のため [`plan_body`] 経由でしか構築できない。接続ハンドラ
/// （Issue #747）は本文バッファの確保サイズを必ずここから取り出す運用にし、
/// [`Headers::content_length`] を直接アロケーションへ渡さない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyPlan {
    content_length: usize,
}

impl BodyPlan {
    /// 検証済みの本文バイト数（`MAX_BODY_LEN` 以下であることが保証されている）。
    pub fn content_length(&self) -> usize {
        self.content_length
    }
}

/// `headers` から `Content-Type` と `Content-Length` を検証し、両方を満たせば
/// [`BodyPlan`] を返す。本文バイト列にもストリームにも触れない（本文を読む
/// 前に完結する）。
///
/// 検証順序は `Content-Type` → 本文長で固定する（両方不正な要求は `08P01`）。
pub fn plan_body(headers: &Headers<'_>) -> Result<BodyPlan, FrameError> {
    let content_type = headers.get_single(CONTENT_TYPE)?;
    validate_content_type(content_type)?;
    check_body_length(headers.content_length())?;
    Ok(BodyPlan {
        content_length: headers.content_length(),
    })
}

/// 本文を持たない要求（`GET /v1/openapi.json`、Issue #2244）の本文計画。宣言長が
/// 0 のときだけ [`BodyPlan`] を返し、それ以外は `08P01`。`Content-Type` は本文が
/// 無いため検査しない（付いていても無視）。未検証の宣言長をアロケーションへ渡さない
/// 不変条件を保つため、`BodyPlan` は [`plan_body`] かここでしか作れない。
pub fn plan_bodyless(headers: &Headers<'_>) -> Result<BodyPlan, FrameError> {
    if headers.content_length() != 0 {
        return Err(FrameError::Malformed("request body is not allowed"));
    }
    Ok(BodyPlan { content_length: 0 })
}

/// `Content-Type` 値が `application/json`（パラメータ無し、または
/// `charset=utf-8` 1 個のみ）であることを検証する。
///
/// 欠落・重複（`Headers::get_single` が `Malformed` を返す）・メディアタイプ
/// 不一致・許可されないパラメータはすべて `08P01` へ落ちる（fail-closed）。
fn validate_content_type(value: Option<&[u8]>) -> Result<(), FrameError> {
    let value = value.ok_or(FrameError::Malformed("missing content-type"))?;

    let mut parts = value.split(|&b| b == b';');
    let media_type = parts
        .next()
        .ok_or(FrameError::Malformed("empty content-type"))?;
    if !trim_ows(media_type).eq_ignore_ascii_case(MEDIA_TYPE_JSON) {
        return Err(FrameError::Malformed("unsupported content-type"));
    }

    let mut charset_seen = false;
    for raw_param in parts {
        let param = trim_ows(raw_param);
        if param.is_empty() {
            return Err(FrameError::Malformed("empty content-type parameter"));
        }
        let eq_idx = param
            .iter()
            .position(|&b| b == b'=')
            .ok_or(FrameError::Malformed("content-type parameter missing ="))?;
        let name = param
            .get(..eq_idx)
            .ok_or(FrameError::Malformed("content-type parameter name range"))?;
        let after_eq = eq_idx
            .checked_add(1)
            .ok_or(FrameError::Malformed("content-type parameter overflow"))?;
        let raw_param_value = param
            .get(after_eq..)
            .ok_or(FrameError::Malformed("content-type parameter value range"))?;

        if !trim_ows(name).eq_ignore_ascii_case(CHARSET_PARAM) {
            return Err(FrameError::Malformed("unsupported content-type parameter"));
        }
        if charset_seen {
            return Err(FrameError::Malformed("duplicate charset parameter"));
        }

        let param_value = unquote(raw_param_value)?;
        if param_value.is_empty() || !param_value.eq_ignore_ascii_case(CHARSET_UTF8) {
            return Err(FrameError::Malformed("unsupported charset"));
        }
        charset_seen = true;
    }

    Ok(())
}

/// 先頭・末尾の SP／HTAB（OWS）をトリムする（`headers.rs::trim_ows` と同じ規則）。
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

/// パラメータ値を OWS トリムしたうえで、RFC 9110 の quoted-string 形
/// （`"..."`）であれば囲みの二重引用符を除去する。引用符の対応が取れない
/// 形（開始のみ・終端のみ）は拒否する。
fn unquote(raw: &[u8]) -> Result<&[u8], FrameError> {
    let trimmed = trim_ows(raw);
    let starts_with_quote = trimmed.first().copied() == Some(b'"');
    let ends_with_quote = trimmed.len() >= 2 && trimmed.last().copied() == Some(b'"');
    if starts_with_quote && ends_with_quote {
        let inner_end = trimmed
            .len()
            .checked_sub(1)
            .ok_or(FrameError::Malformed("quoted content-type value range"))?;
        trimmed
            .get(1..inner_end)
            .ok_or(FrameError::Malformed("quoted content-type value range"))
    } else if starts_with_quote || ends_with_quote {
        Err(FrameError::Malformed("unbalanced content-type quote"))
    } else {
        Ok(trimmed)
    }
}

/// 宣言済み本文長が [`MAX_BODY_LEN`] を超えないことを検証する。本文バイト
/// 列は一切参照しない（読み取り前拒否）。
fn check_body_length(declared: usize) -> Result<(), FrameError> {
    if declared > MAX_BODY_LEN {
        return Err(FrameError::TooLarge {
            declared,
            max: MAX_BODY_LEN,
        });
    }
    Ok(())
}

/// UTF-8 として不正な本文バイト列を表すエラー（`42601`）。
///
/// `engine::json::JsonError` は使わず本モジュール専用の小さな型として持つ
/// （転送路の型を engine の JSON パーサへ結合させないため）。本文バイト列・
/// 不正位置は含めない（クライアントへ内部詳細を反映しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyEncodingError;

impl BodyEncodingError {
    /// `engine::error_format::ErrorClass` への写像（`42601`＝
    /// `UnsupportedSqlSyntax`。`error_body.rs`／`status.rs` が既に持つ
    /// `ErrorClass` → HTTP 応答の横断写像へ、追加 `wire_code` を作らずに乗る）。
    pub const fn error_class(&self) -> ErrorClass {
        ErrorClass::UnsupportedSqlSyntax
    }

    /// SQLSTATE（`42601`）。
    pub fn sqlstate(&self) -> &'static str {
        self.error_class().wire_code()
    }

    /// クライアントへ返す固定英語文言。本文バイトを一切反映しない。
    pub const fn client_message(&self) -> &'static str {
        "request body is not valid utf-8"
    }
}

/// 本文バイト列を UTF-8 文字列へ昇格する。空本文（`Content-Length: 0`）や
/// BOM 付き本文もそのまま受理する（JSON としての妥当性判定は後段の責務）。
pub fn body_as_utf8(body: &[u8]) -> Result<&str, BodyEncodingError> {
    std::str::from_utf8(body).map_err(|_| BodyEncodingError)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::headers::{parse_headers, HeaderParse};

    fn parse(input: &[u8]) -> Headers<'_> {
        match parse_headers(input).expect("header parse should succeed") {
            HeaderParse::Complete { headers, .. } => headers,
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    fn assert_protocol_violation(err: &FrameError) {
        assert!(matches!(err, FrameError::Malformed(_)));
        assert_eq!(err.sqlstate(), Some("08P01"));
        assert_eq!(err.error_class(), Some(ErrorClass::ProtocolViolation));
    }

    fn assert_payload_too_large(err: &FrameError, declared: usize) {
        assert!(matches!(
            err,
            FrameError::TooLarge { declared: d, max } if *d == declared && *max == MAX_BODY_LEN
        ));
        assert_eq!(err.sqlstate(), Some("54000"));
        assert_eq!(err.error_class(), Some(ErrorClass::PayloadTooLarge));
    }

    #[test]
    fn max_body_len_is_one_mebibyte() {
        assert_eq!(MAX_BODY_LEN, 1_048_576);
    }

    #[test]
    fn accepts_application_json_without_parameters() {
        let headers = parse(b"Content-Type: application/json\r\nContent-Length: 0\r\n\r\n");
        let plan = plan_body(&headers).expect("plan should succeed");
        assert_eq!(plan.content_length(), 0);
    }

    #[test]
    fn accepts_application_json_case_insensitive() {
        let headers = parse(b"Content-Type: APPLICATION/JSON\r\nContent-Length: 0\r\n\r\n");
        plan_body(&headers).expect("plan should succeed");
    }

    #[test]
    fn accepts_charset_utf8_lowercase() {
        let headers =
            parse(b"Content-Type: application/json; charset=utf-8\r\nContent-Length: 0\r\n\r\n");
        plan_body(&headers).expect("plan should succeed");
    }

    #[test]
    fn accepts_charset_utf8_uppercase_no_space() {
        let headers =
            parse(b"Content-Type: application/json;charset=UTF-8\r\nContent-Length: 0\r\n\r\n");
        plan_body(&headers).expect("plan should succeed");
    }

    #[test]
    fn accepts_charset_utf8_quoted_with_spacing() {
        let headers = parse(
            b"Content-Type: application/json ; charset=\"utf-8\"\r\nContent-Length: 0\r\n\r\n",
        );
        plan_body(&headers).expect("plan should succeed");
    }

    #[test]
    fn accepts_content_length_at_exact_limit() {
        let mut input = Vec::new();
        input.extend_from_slice(
            b"Content-Type: application/json\r\nContent-Length: 1048576\r\n\r\n",
        );
        let headers = parse(&input);
        let plan = plan_body(&headers).expect("plan should succeed");
        assert_eq!(plan.content_length(), MAX_BODY_LEN);
    }

    #[test]
    fn rejects_missing_content_type() {
        let headers = parse(b"Content-Length: 0\r\n\r\n");
        let err = plan_body(&headers).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_empty_content_type_value() {
        let headers = parse(b"Content-Type:\r\nContent-Length: 0\r\n\r\n");
        let err = plan_body(&headers).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_text_plain() {
        let headers = parse(b"Content-Type: text/plain\r\nContent-Length: 0\r\n\r\n");
        let err = plan_body(&headers).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_form_urlencoded() {
        let headers =
            parse(b"Content-Type: application/x-www-form-urlencoded\r\nContent-Length: 0\r\n\r\n");
        let err = plan_body(&headers).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_wildcard_media_type() {
        let headers = parse(b"Content-Type: application/*\r\nContent-Length: 0\r\n\r\n");
        let err = plan_body(&headers).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_json_patch_media_type() {
        let headers =
            parse(b"Content-Type: application/json-patch+json\r\nContent-Length: 0\r\n\r\n");
        let err = plan_body(&headers).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_boundary_parameter() {
        let headers =
            parse(b"Content-Type: application/json; boundary=x\r\nContent-Length: 0\r\n\r\n");
        let err = plan_body(&headers).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_trailing_semicolon_without_parameter() {
        let headers = parse(b"Content-Type: application/json;\r\nContent-Length: 0\r\n\r\n");
        let err = plan_body(&headers).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_charset_without_value() {
        let headers =
            parse(b"Content-Type: application/json; charset=\r\nContent-Length: 0\r\n\r\n");
        let err = plan_body(&headers).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_duplicate_charset_parameter() {
        let headers = parse(
            b"Content-Type: application/json; charset=utf-8; charset=utf-8\r\nContent-Length: 0\r\n\r\n",
        );
        let err = plan_body(&headers).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_charset_utf16() {
        let headers =
            parse(b"Content-Type: application/json; charset=utf-16\r\nContent-Length: 0\r\n\r\n");
        let err = plan_body(&headers).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_duplicate_content_type_header() {
        let headers = parse(
            b"Content-Type: application/json\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
        );
        let err = plan_body(&headers).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_body_over_limit() {
        let mut input = Vec::new();
        input.extend_from_slice(
            b"Content-Type: application/json\r\nContent-Length: 1048577\r\n\r\n",
        );
        let headers = parse(&input);
        let err = plan_body(&headers).unwrap_err();
        assert_payload_too_large(&err, 1_048_577);
    }

    #[test]
    fn rejects_body_over_limit_without_reading_body_bytes() {
        // 本文バイトを一切含まない入力（ヘッダ部のみ）で判定されることを示す。
        let mut input = Vec::new();
        input.extend_from_slice(
            b"Content-Type: application/json\r\nContent-Length: 99999999\r\n\r\n",
        );
        let headers = parse(&input);
        let err = plan_body(&headers).unwrap_err();
        assert_payload_too_large(&err, 99_999_999);
    }

    #[test]
    fn content_type_violation_takes_priority_over_body_length() {
        let mut input = Vec::new();
        input.extend_from_slice(b"Content-Type: text/plain\r\nContent-Length: 1048577\r\n\r\n");
        let headers = parse(&input);
        let err = plan_body(&headers).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_invalid_utf8_leading_byte() {
        let body = [0xFFu8];
        body_as_utf8(&body).expect_err("invalid leading byte should be rejected");
    }

    #[test]
    fn rejects_truncated_multibyte_sequence() {
        let body = [0xE3u8, 0x81];
        body_as_utf8(&body).expect_err("truncated multibyte sequence should be rejected");
    }

    #[test]
    fn rejects_overlong_encoding() {
        let body = [0xC0u8, 0x80];
        body_as_utf8(&body).expect_err("overlong encoding should be rejected");
    }

    #[test]
    fn rejects_surrogate_encoding() {
        let body = [0xEDu8, 0xA0, 0x80];
        body_as_utf8(&body).expect_err("surrogate encoding should be rejected");
    }

    #[test]
    fn body_encoding_error_has_fixed_sqlstate_and_message() {
        let err = body_as_utf8(&[0xFFu8]).unwrap_err();
        assert_eq!(err.sqlstate(), "42601");
        assert_eq!(err.error_class(), ErrorClass::UnsupportedSqlSyntax);
        assert_eq!(err.client_message(), "request body is not valid utf-8");
    }

    #[test]
    fn accepts_valid_multibyte_utf8() {
        let text = "日本語のボディ";
        let decoded = body_as_utf8(text.as_bytes()).expect("valid utf-8 should be accepted");
        assert_eq!(decoded, text);
    }

    #[test]
    fn accepts_utf8_bom_prefixed_body() {
        let mut body = vec![0xEF, 0xBB, 0xBF];
        body.extend_from_slice(b"{}");
        let decoded = body_as_utf8(&body).expect("bom prefixed body should be accepted");
        assert_eq!(decoded, "\u{feff}{}");
    }

    #[test]
    fn accepts_empty_body() {
        let decoded = body_as_utf8(&[]).expect("empty body should be accepted");
        assert_eq!(decoded, "");
    }

    #[test]
    fn request_line_headers_body_plan_offsets_line_up() {
        use crate::http::request::{parse_request_line, RequestLineParse};

        let content_type = b"Content-Type: application/json\r\n".as_slice();
        let content_length = b"Content-Length: 4\r\n".as_slice();
        let mut input = Vec::new();
        input.extend_from_slice(b"POST /v1/query HTTP/1.1\r\n");
        input.extend_from_slice(content_type);
        input.extend_from_slice(content_length);
        input.extend_from_slice(b"\r\n");
        input.extend_from_slice(b"BODY");

        let line_consumed = match parse_request_line(&input).expect("line parse ok") {
            RequestLineParse::Complete { consumed, .. } => consumed,
            other => panic!("expected Complete, got {other:?}"),
        };
        let header_input = &input[line_consumed..];
        let (headers, header_consumed) = match parse_headers(header_input).expect("header parse ok")
        {
            HeaderParse::Complete { headers, consumed } => (headers, consumed),
            other => panic!("expected Complete, got {other:?}"),
        };
        let plan = plan_body(&headers).expect("plan should succeed");
        assert_eq!(plan.content_length(), 4);
        let body = &header_input[header_consumed..];
        assert_eq!(body.len(), plan.content_length());
        assert_eq!(body, b"BODY");
    }

    #[test]
    fn plan_bodyless_accepts_zero_and_ignores_content_type() {
        for raw in [
            &b"Content-Length: 0\r\n\r\n"[..],
            b"Content-Length: 0\r\nContent-Type: text/plain\r\n\r\n",
        ] {
            let plan = plan_bodyless(&parse(raw)).expect("zero length is accepted");
            assert_eq!(plan.content_length(), 0);
        }
    }

    #[test]
    fn plan_bodyless_rejects_nonzero_length() {
        let headers = parse(b"Content-Length: 3\r\nContent-Type: application/json\r\n\r\n");
        assert_protocol_violation(&plan_bodyless(&headers).unwrap_err());
    }
}
