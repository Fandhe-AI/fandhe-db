//! HTTP-3（`Content-Type` 検証・本文の UTF-8 昇格）の層 A 結合テスト
//! （Issue #748・TASK-173）。`crates/wire-server/src/http/body.rs` の
//! `plan_body`／検証順序の内部契約は同ファイルの `#[cfg(test)]` が固定済み。
//! 本ファイルは TCP 経由での外形（HTTP ステータス＋`wire_code`）まで含めて
//! 検証する（`tests/http_common/mod.rs` 参照）。
//!
//! ## `body_as_utf8` の e2e 結線について
//!
//! `wire_server::http::body::body_as_utf8`（Issue #742・`42601`）は
//! `crates/wire-server/src/` 内に呼び出し元を持たない（`build_outcome` は
//! 生 `&[u8]` を `RequestHandler` へ渡すのみ）。したがって不正 UTF-8 本文を
//! **リスナー経由**で送っても現状は `42601` に到達しない（`Content-Type`／
//! `Content-Length` の検証を通れば `PlaceholderRouter` の `08P01` に到達する）。
//! 本ファイルは `body_as_utf8` の契約を公開 API への直接呼び出しで固定し
//! （下記「直接 API」節）、リスナー経由での結線は `Request.body` を消費する
//! 実ルータ／op ハンドラ（Issue #758・#763）への申し送りとする。
//!
//! 対応: TASK-173（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア HTTP-3,
//! HTTP-11）。

use crate::http_common;

use http_common::{
    assert_reached_router, assert_rejected, build_request, parse_single_response, send_raw,
    spawn_http_listener, AfterWrite, SERVER_READ_TIMEOUT,
};

const MAX_CONNECTIONS_FOR_TEST: usize = 4;

fn listener() -> std::net::SocketAddr {
    let (addr, _limiter) = spawn_http_listener(MAX_CONNECTIONS_FOR_TEST, SERVER_READ_TIMEOUT);
    addr
}

fn request_with_content_type(content_type: &str, body: &[u8]) -> Vec<u8> {
    let content_length = body.len().to_string();
    build_request(
        "/v1/query",
        &[
            ("Content-Type", content_type),
            ("Content-Length", &content_length),
        ],
        body,
    )
}

// --- 受理側 ---------------------------------------------------------------

#[test]
fn accepts_application_json_without_parameters() {
    let addr = listener();
    let request = request_with_content_type("application/json", b"{}");
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_reached_router(&resp);
}

#[test]
fn accepts_application_json_charset_utf8_case_insensitive_quoted() {
    let addr = listener();
    let request = request_with_content_type("Application/JSON; Charset=\"UTF-8\"", b"{}");
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_reached_router(&resp);
}

// --- 拒否側（すべて 400／08P01） ------------------------------------------

fn assert_content_type_rejected(content_type_header: Option<&str>) {
    let addr = listener();
    let headers: Vec<(&str, &str)> = match content_type_header {
        Some(ct) => vec![("Content-Type", ct), ("Content-Length", "2")],
        None => vec![("Content-Length", "2")],
    };
    let request = build_request("/v1/query", &headers, b"{}");
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
}

#[test]
fn rejects_missing_content_type() {
    assert_content_type_rejected(None);
}

#[test]
fn rejects_text_plain() {
    assert_content_type_rejected(Some("text/plain"));
}

#[test]
fn rejects_charset_utf16() {
    assert_content_type_rejected(Some("application/json; charset=utf-16"));
}

#[test]
fn rejects_unsupported_parameter_boundary() {
    assert_content_type_rejected(Some("application/json; boundary=x"));
}

#[test]
fn rejects_trailing_semicolon_without_parameter() {
    assert_content_type_rejected(Some("application/json;"));
}

#[test]
fn rejects_duplicate_content_type_header() {
    let addr = listener();
    let request = build_request(
        "/v1/query",
        &[
            ("Content-Type", "application/json"),
            ("Content-Type", "application/json"),
            ("Content-Length", "2"),
        ],
        b"{}",
    );
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
}

#[test]
fn rejects_wildcard_media_type() {
    assert_content_type_rejected(Some("application/*"));
}

/// 検証順序: `Content-Type` → 本文長（`body.rs::plan_body` の doc・
/// `content_type_violation_takes_priority_over_body_length` 単体テストの
/// e2e 版）。`Content-Type` が不正 **かつ** `Content-Length` が
/// `MAX_BODY_LEN` を超える要求は `54000`（本文長超過）ではなく `08P01`
/// （`Content-Type` 不正）として拒否される。
#[test]
fn content_type_violation_takes_priority_over_body_length() {
    let addr = listener();
    let over_limit = wire_server::http::body::MAX_BODY_LEN + 1;
    let request = build_request(
        "/v1/query",
        &[
            ("Content-Type", "text/plain"),
            ("Content-Length", &over_limit.to_string()),
        ],
        b"",
    );
    // 本文は 1 バイトも送らない（読み取り前拒否の外形証跡。本文を待たずに
    // 応答へ到達することを keep-open で確認する）。
    let received = send_raw(addr, &request, AfterWrite::KeepOpen);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
}

// --- 直接 API（`body_as_utf8`。e2e 結線は未実装。モジュール doc 参照） ----

#[test]
fn body_as_utf8_direct_api_rejects_invalid_utf8() {
    let err = wire_server::http::body::body_as_utf8(&[0xFFu8]).expect_err("invalid utf-8");
    assert_eq!(err.sqlstate(), "42601");
    assert_eq!(
        wire_server::http::status::http_status(err.error_class()),
        400
    );
    assert_eq!(err.client_message(), "request body is not valid utf-8");
}

#[test]
fn body_as_utf8_direct_api_accepts_multibyte_bom_and_empty() {
    let text = "日本語のボディ";
    assert_eq!(
        wire_server::http::body::body_as_utf8(text.as_bytes()).expect("valid utf-8"),
        text
    );

    let mut bom_prefixed = vec![0xEF, 0xBB, 0xBF];
    bom_prefixed.extend_from_slice(b"{}");
    assert_eq!(
        wire_server::http::body::body_as_utf8(&bom_prefixed).expect("bom prefixed"),
        "\u{feff}{}"
    );

    assert_eq!(
        wire_server::http::body::body_as_utf8(&[]).expect("empty body"),
        ""
    );
}
