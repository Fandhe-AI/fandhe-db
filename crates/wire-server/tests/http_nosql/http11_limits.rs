//! HTTP-11（固定上限。要求行長・ヘッダ部合計・ヘッダ個数・本文長）の層 A
//! 結合テスト（Issue #748・TASK-173）。
//!
//! 読み取りタイムアウト（無応答クローズ）・同時接続数上限（`503`／`53300`）
//! は `tests/http_limits.rs`（Issue #743）が既に担当済みのため、本ファイルは
//! 「固定上限そのもの・境界値・本文の読み取り前拒否」に限定する。
//!
//! 各上限は実行時設定・CLI での緩和経路を持たない固定定数
//! （`request::MAX_REQUEST_LINE_LEN`・`headers::MAX_HEADER_SECTION_LEN`・
//! `headers::MAX_HEADER_COUNT`・`body::MAX_BODY_LEN`）であることを、値の
//! ピン留めと境界値の受理／拒否の両方で固定する。
//!
//! 対応: TASK-173（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア HTTP-11）。

use crate::http_common;

use http_common::{
    assert_reached_router, assert_rejected, parse_single_response, send_raw, spawn_http_listener,
    AfterWrite, SERVER_READ_TIMEOUT,
};
use wire_server::http::body::MAX_BODY_LEN;
use wire_server::http::headers::{MAX_HEADER_COUNT, MAX_HEADER_SECTION_LEN};
use wire_server::http::request::MAX_REQUEST_LINE_LEN;

const MAX_CONNECTIONS_FOR_TEST: usize = 4;

fn listener() -> std::net::SocketAddr {
    let (addr, _limiter) = spawn_http_listener(MAX_CONNECTIONS_FOR_TEST, SERVER_READ_TIMEOUT);
    addr
}

// --- 定数ピン（CLI・実行時設定で緩和されない固定上限であることの公開側固定） --

#[test]
fn fixed_limits_match_expected_constants() {
    assert_eq!(MAX_REQUEST_LINE_LEN, 4 * 1024);
    assert_eq!(MAX_HEADER_SECTION_LEN, 8 * 1024);
    assert_eq!(MAX_HEADER_COUNT, 32);
    assert_eq!(MAX_BODY_LEN, 1024 * 1024);
}

// --- 要求行長の境界 ---------------------------------------------------------

/// `POST /` + `'a'` の連続 + ` HTTP/1.1\r\n` で要求行全体をちょうど
/// `target_len_total` バイトにする（`request.rs::accepts_request_line_at_exact_limit`
/// と同じ構築法）。
fn request_line_of_len(target_len_total: usize) -> Vec<u8> {
    let prefix = b"POST /".as_slice();
    let suffix = b" HTTP/1.1\r\n".as_slice();
    let filler_len = target_len_total - prefix.len() - suffix.len();
    let mut out = Vec::with_capacity(target_len_total);
    out.extend_from_slice(prefix);
    out.extend(std::iter::repeat_n(b'a', filler_len));
    out.extend_from_slice(suffix);
    out
}

fn minimal_valid_headers() -> &'static [u8] {
    b"Content-Type: application/json\r\nContent-Length: 0\r\n\r\n"
}

#[test]
fn accepts_request_line_at_exact_limit() {
    let addr = listener();
    let mut request = request_line_of_len(MAX_REQUEST_LINE_LEN);
    assert_eq!(request.len(), MAX_REQUEST_LINE_LEN);
    request.extend_from_slice(minimal_valid_headers());
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_reached_router(&resp);
}

#[test]
fn rejects_request_line_over_limit_with_terminator() {
    let addr = listener();
    let mut request = request_line_of_len(MAX_REQUEST_LINE_LEN + 1);
    assert_eq!(request.len(), MAX_REQUEST_LINE_LEN + 1);
    request.extend_from_slice(minimal_valid_headers());
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
}

#[test]
fn rejects_request_line_over_limit_without_terminator() {
    let addr = listener();
    // CRLF を一切含まない（上限到達の時点で即座に拒否される契約。
    // `request.rs::rejects_input_without_crlf_at_or_over_limit` の e2e 版）。
    let request = vec![b'a'; MAX_REQUEST_LINE_LEN];
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
}

// --- ヘッダ部合計バイト数の境界 ---------------------------------------------

/// `Content-Type` + `Content-Length: 0` + `X-Pad: <filler>` + 終端空行で
/// ヘッダ部合計をちょうど `target_len` バイトにする
/// （`headers.rs::accepts_header_section_at_exact_limit` と同じ構築法。
/// 要求行は別の固定上限が扱うため、ここでは通常の短い要求行を前置する）。
fn headers_section_of_len(target_len: usize) -> Vec<u8> {
    let fixed_prefix = b"Content-Type: application/json\r\nContent-Length: 0\r\nX-Pad: ".as_slice();
    let fixed_suffix = b"\r\n\r\n".as_slice();
    let filler_len = target_len - fixed_prefix.len() - fixed_suffix.len();
    let mut out = Vec::with_capacity(target_len);
    out.extend_from_slice(fixed_prefix);
    out.extend(std::iter::repeat_n(b'a', filler_len));
    out.extend_from_slice(fixed_suffix);
    out
}

#[test]
fn accepts_header_section_at_exact_limit() {
    let addr = listener();
    let headers_section = headers_section_of_len(MAX_HEADER_SECTION_LEN);
    assert_eq!(headers_section.len(), MAX_HEADER_SECTION_LEN);
    let mut request = b"POST /v1/query HTTP/1.1\r\n".to_vec();
    request.extend_from_slice(&headers_section);
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_reached_router(&resp);
}

#[test]
fn rejects_header_section_over_limit() {
    let addr = listener();
    let headers_section = headers_section_of_len(MAX_HEADER_SECTION_LEN + 1);
    assert_eq!(headers_section.len(), MAX_HEADER_SECTION_LEN + 1);
    let mut request = b"POST /v1/query HTTP/1.1\r\n".to_vec();
    request.extend_from_slice(&headers_section);
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
}

// --- ヘッダ個数の境界 -------------------------------------------------------

/// `x_header_count` 本の `X-H<i>: v` に `Content-Type`・`Content-Length: 0`
/// を加えた合計 `x_header_count + 2` 本のヘッダ部を組み立てる。
fn headers_with_extra_count(x_header_count: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..x_header_count {
        out.extend_from_slice(format!("X-H{i}: v\r\n").as_bytes());
    }
    out.extend_from_slice(b"Content-Type: application/json\r\nContent-Length: 0\r\n\r\n");
    out
}

#[test]
fn accepts_header_count_at_exact_limit() {
    let addr = listener();
    // X-H * 30 + Content-Type + Content-Length = ちょうど MAX_HEADER_COUNT。
    let headers_section = headers_with_extra_count(MAX_HEADER_COUNT - 2);
    let mut request = b"POST /v1/query HTTP/1.1\r\n".to_vec();
    request.extend_from_slice(&headers_section);
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_reached_router(&resp);
}

#[test]
fn rejects_header_count_over_limit() {
    let addr = listener();
    let headers_section = headers_with_extra_count(MAX_HEADER_COUNT - 1);
    let mut request = b"POST /v1/query HTTP/1.1\r\n".to_vec();
    request.extend_from_slice(&headers_section);
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
}

// --- 本文長上限の境界 -------------------------------------------------------

#[test]
fn accepts_content_length_at_max_body_len() {
    let addr = listener();
    let body = vec![b'a'; MAX_BODY_LEN];
    let mut request = format!(
        "POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {MAX_BODY_LEN}\r\n\r\n"
    )
    .into_bytes();
    request.extend_from_slice(&body);
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_reached_router(&resp);
}

/// 宣言長が `MAX_BODY_LEN` を 1 バイト超過する要求は本文を 1 バイトも送らず
/// （keep-open）拒否される（読み取り前拒否の外形証跡。本文経路であれば
/// `08P01` になるところ、`54000` が返ることが判別子になる）。
#[test]
fn rejects_content_length_over_max_body_len_without_sending_body() {
    let addr = listener();
    let over_limit = MAX_BODY_LEN + 1;
    let request = format!(
        "POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {over_limit}\r\n\r\n"
    )
    .into_bytes();
    let received = send_raw(addr, &request, AfterWrite::KeepOpen);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 413, "54000");
}

/// `Content-Length: u64::MAX`（`headers.rs` は digits-only・u64 累積のいずれも
/// 通過させるため `usize` の値として受理する）は `54000`（本文長超過）へ
/// 落ちる。
#[test]
fn rejects_content_length_at_u64_max() {
    let addr = listener();
    let request = b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 18446744073709551615\r\n\r\n".to_vec();
    let received = send_raw(addr, &request, AfterWrite::KeepOpen);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 413, "54000");
}

/// `u64::MAX + 1`（20 桁。digits-only 検証自体は通るが `u64` 累積の途中で
/// オーバーフローする）は `headers.rs` の時点で `08P01` へ拒否される
/// （`54000` ではない）。
#[test]
fn rejects_content_length_overflowing_u64() {
    let addr = listener();
    let request = b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 18446744073709551616\r\n\r\n".to_vec();
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
}
