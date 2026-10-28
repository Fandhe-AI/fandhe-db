//! HTTP-2（要求行・ヘッダのフレーミング）の層 A 結合テスト（Issue #748・
//! TASK-173）。`wire_server::http::listener::accept_loop_with_limiter`
//! （production 入口。内部で `wire_server::http::conn::PlaceholderRouter` を
//! 固定で使う）をリスナーとして起動し、`std::net::TcpStream` で生バイトを
//! 送受信して外形契約を検証する。
//!
//! 各パーサの内部契約（境界値・個々の拒否理由）は
//! `crates/wire-server/src/http/request.rs`・`headers.rs` 自身の `#[cfg(test)]`
//! が固定済み（`pub(crate)` の内部 API を直接呼べる）。本ファイルはそれを
//! 「TCP 経由で 1 往復すると HTTP ステータス＋`wire_code` の JSON 応答が
//! 返り接続が閉じる」という外形まで含めて検証する（`tests/http_common/mod.rs`
//! のモジュール doc 参照）。
//!
//! 対応: TASK-173（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア HTTP-2,
//! HTTP-11）。
//!
//! ## PR #812 codex-review 指摘（P2）への対応
//!
//! - 余剰本文／パイプライン 2 本目の判定
//!   （<https://github.com/Fandhe-AI/vector-db/pull/812#discussion_r4030428050>）:
//!   production の `read_body`（`crates/wire-server/src/http/conn.rs`）は
//!   要求の頭の読み取り時点で本文の先頭が residual として同時に届いた場合に
//!   限り宣言長超過を検知する。TCP がどこで頭と本文を分割するかは外部から
//!   制御できないため、同じ入力でも「超過拒否」（08P01・独自の拒否文言）と
//!   「宣言長ぶんちょうど読み切ってルータへ到達」（`PlaceholderRouter` の
//!   固定応答も 08P01）のどちらに転ぶかが決定的ではない。両分岐とも
//!   `ErrorClass::ProtocolViolation`（`crates/wire-server/src/http/status.rs`）
//!   へ写像され status／wire_code は一致するため、本ファイルの該当 2 ケースは
//!   両方の分岐を許容する形で status／wire_code のみを固定し「単一応答で
//!   接続が閉じる」ことを主張する形へ改めた。余剰拒否そのものの決定的検証は
//!   `crates/wire-server/src/http/conn.rs` 内の単体テスト
//!   `rejects_body_longer_than_declared_content_length_within_head_buffer`
//!   （`read_body` へ超過 residual を直接与え、この分岐に限っては当該関数が
//!   I/O を行わないため split 非依存で決定的）が既に担っている
//!   （`read_body`／`residual` は `pub(crate)` ですらない private 関数・
//!   フィールドのため、本ファイル〔`tests/` 配下の結合テスト＝別クレート
//!   扱い〕からは到達できず、この既存の内部単体テストへ分離する以上のことは
//!   できない）。
//! - 要求行の異常以外は完全な要求を送る
//!   （<https://github.com/Fandhe-AI/vector-db/pull/812#discussion_r4030428059>）:
//!   `assert_malformed_request_line` の各ケースを、検証対象の異常（要求行）
//!   以外は `Content-Type: application/json`・`Content-Length: 0`・終端まで
//!   含む完全な要求へ揃えた。あわせて、拒否応答の `message` が
//!   [`REQUEST_LINE_FRAME_ERROR_MESSAGE`]（要求行パーサ自身の拒否によって
//!   のみ出る文言）であることも固定する。これにより、要求行の検証が万一
//!   緩んでも「ヘッダ未完了の EOF による偶発的な同一 wire_code」
//!   （`message` は `"invalid request"`）や「ルータへ到達してしまった」
//!   （`message` は [`ROUTER_PLACEHOLDER_MESSAGE`]）と区別できずテストが
//!   素通りする事態を防ぐ。

use std::time::Duration;

use crate::http_common;

use http_common::{assert_reached_router, SERVER_READ_TIMEOUT};
use http_common::{
    assert_rejected, build_request, error_message_of, parse_single_response, send_raw,
    spawn_http_listener, well_formed_request, wire_code_of, AfterWrite,
};

const MAX_CONNECTIONS_FOR_TEST: usize = 4;

fn listener() -> std::net::SocketAddr {
    let (addr, _limiter) = spawn_http_listener(MAX_CONNECTIONS_FOR_TEST, SERVER_READ_TIMEOUT);
    addr
}

// --- 対照（受理側） ---------------------------------------------------

#[test]
fn accepts_well_formed_post_request() {
    let addr = listener();
    let request = well_formed_request(b"{}");
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_reached_router(&resp);
}

#[test]
fn accepts_empty_body_with_content_length_zero() {
    let addr = listener();
    let request = well_formed_request(b"");
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_reached_router(&resp);
}

// --- 要求行の拒否契約（すべて 400／08P01） ------------------------------

/// 要求行パーサ（`crate::http::request::parse_request_line`）自身が
/// `FrameError::Malformed` を返した場合にのみ出る、クライアント向け固定
/// メッセージ（`FrameError::client_message()`。`Malformed` バリアント共通の
/// 汎用文言）。
///
/// `crate::http::conn::protocol_violation_bytes` が使う汎用文言
/// `"invalid request"`（ヘッダ未完了の EOF・内部境界エラー等）や、
/// `PlaceholderRouter` の固定応答文言
/// （[`http_common::ROUTER_PLACEHOLDER_MESSAGE`]）とは異なる。この 3 者を
/// 区別することで「要求行が本当にパーサ自身によって拒否されたか」を
/// 固定できる（モジュール doc の codex-review 指摘対応節を参照）。
const REQUEST_LINE_FRAME_ERROR_MESSAGE: &str = "invalid message frame";

/// `raw`（検証対象の異常を含む要求行 1 行ぶん。終端の扱いはケースごとに
/// 異なる）を送信し、要求行パーサ自身による拒否（`REQUEST_LINE_FRAME_ERROR_MESSAGE`）
/// であることまで固定する。呼び出し元は `raw` に、検証対象の異常以外の部分
/// （`Content-Type`・`Content-Length: 0`・終端）を完全な形で含めること
/// （モジュール doc 参照）。
fn assert_malformed_request_line(raw: &[u8]) {
    let addr = listener();
    let received = send_raw(addr, raw, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
    assert_eq!(
        error_message_of(&resp),
        REQUEST_LINE_FRAME_ERROR_MESSAGE,
        "expected rejection by the request-line parser itself (not an EOF-based or \
         router-reached rejection sharing the same wire_code); body: {:?}",
        String::from_utf8_lossy(&resp.body)
    );
}

#[test]
fn rejects_get_method() {
    assert_malformed_request_line(
        b"GET /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_lowercase_post_method() {
    assert_malformed_request_line(
        b"post /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_http_1_0() {
    assert_malformed_request_line(
        b"POST /v1/query HTTP/1.0\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_http_2_0() {
    assert_malformed_request_line(
        b"POST /v1/query HTTP/2.0\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_bare_lf_line_terminator() {
    // 異常（要求行の終端が bare LF）はバイト先頭から 25 バイト目で即座に
    // 検出される（`find_crlf_within` が `\r` を伴わない `\n` を見つけた時点で
    // 以降を走査せず拒否する）ため、それ以降に続く完全な要求（`Content-Type`・
    // `Content-Length: 0`・終端）は解析されず無害。
    assert_malformed_request_line(
        b"POST /v1/query HTTP/1.1\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_leading_blank_line() {
    // 異常（先頭の空行）は 1 行目（空文字列）の時点で即座に検出される
    // ため、2 行目以降に続く完全な要求は解析されず無害。
    assert_malformed_request_line(
        b"\r\nPOST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_consecutive_spaces_in_request_line() {
    assert_malformed_request_line(
        b"POST  /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_too_many_request_line_tokens() {
    assert_malformed_request_line(
        b"POST /v1/query HTTP/1.1 extra\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_too_few_request_line_tokens() {
    assert_malformed_request_line(
        b"POST /v1/query\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_absolute_form_target() {
    assert_malformed_request_line(
        b"POST http://host/v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_control_byte_in_target() {
    assert_malformed_request_line(
        b"POST /v1/\x01query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_non_ascii_byte_in_target() {
    assert_malformed_request_line(
        b"POST /v1/\xc3query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\n\r\n",
    );
}

// --- ヘッダの拒否契約（すべて 400／08P01） -----------------------------

fn assert_malformed_headers(raw: &[u8]) {
    let addr = listener();
    let received = send_raw(addr, raw, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
}

#[test]
fn rejects_missing_content_length_header() {
    assert_malformed_headers(b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\n\r\n");
}

#[test]
fn rejects_negative_content_length() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: -1\r\n\r\n",
    );
}

#[test]
fn rejects_non_numeric_content_length() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: abc\r\n\r\n",
    );
}

#[test]
fn rejects_leading_plus_content_length() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: +1\r\n\r\n",
    );
}

#[test]
fn rejects_duplicate_content_length_same_value() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 0\r\nContent-Type: application/json\r\n\r\n",
    );
}

#[test]
fn rejects_duplicate_content_length_different_value() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 1\r\nContent-Type: application/json\r\n\r\n",
    );
}

#[test]
fn rejects_transfer_encoding_chunked_even_with_content_length() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
}

#[test]
fn rejects_transfer_encoding_chunked_without_content_length() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
    );
}

#[test]
fn rejects_obs_fold_continuation_line() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\n Content-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_header_name_with_space() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nX Foo: bar\r\nContent-Length: 0\r\n\r\n",
    );
}

#[test]
fn rejects_header_value_with_control_byte() {
    assert_malformed_headers(
        b"POST /v1/query HTTP/1.1\r\nContent-Type: application/json\r\nX-Foo: b\x00r\r\nContent-Length: 0\r\n\r\n",
    );
}

// --- 本文長不一致（`Content-Length` との齟齬） --------------------------

/// 宣言長より短い本文を送って半クローズ（クライアントが書き込み側を閉じ、
/// サーバーが本文読み取り中に EOF を検知する）。半クローズ必須の理由:
/// クライアントが接続を開いたままだと、サーバーはタイムアウトまで応答を
/// 書かずに待ち続ける（`fill_remaining_body` の deadline 分岐）ため、
/// 検証したい「本文不一致を 08P01 として即時検知する」契約を確認できない。
#[test]
fn rejects_body_shorter_than_declared_content_length() {
    let addr = listener();
    let mut request = build_request(
        "/v1/query",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", "10"),
        ],
        b"",
    );
    request.extend_from_slice(b"abc"); // 宣言 10 バイトに対し 3 バイトのみ送信
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 400, "08P01");
}

/// 宣言長より長い本文（余剰バイト）を送る。
///
/// production の `read_body`（`crate::http::conn`）は、要求の頭の読み取り
/// 時点で本文の先頭（residual）として宣言長を超えるバイト列が同時に届いた
/// 場合に限って超過を検知する。頭と本文がどこで分割されて `read` されるかは
/// OS の TCP 実装に委ねられ外部から制御できないため、この 2 分岐
/// （「超過拒否」か「宣言長ぶんちょうど読み切ってルータへ到達」か）は
/// 決定的ではない。いずれの分岐でも `ErrorClass::ProtocolViolation` へ
/// 写像され status／wire_code は一致する（モジュール doc の codex-review
/// 指摘対応節を参照。余剰拒否そのものの決定的検証は
/// `crate::http::conn` 内の単体テストへ分離済み）ため、ここでは
/// status／wire_code の一致と、`parse_single_response` が保証する「応答が
/// ちょうど 1 個で、その後 EOF になる」ことのみを固定する。keep-open でも、
/// サーバー側の判断だけで応答＋クローズへ到達することを確認する。
#[test]
fn rejects_body_longer_than_declared_content_length() {
    let addr = listener();
    let mut request = build_request(
        "/v1/query",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", "2"),
        ],
        b"",
    );
    request.extend_from_slice(b"abcdef"); // 宣言 2 バイトに対し 6 バイト送信
    let received = send_raw(addr, &request, AfterWrite::KeepOpen);
    let resp = parse_single_response(&received);
    assert_eq!(
        resp.status,
        400,
        "unexpected status (body: {:?})",
        String::from_utf8_lossy(&resp.body)
    );
    assert_eq!(wire_code_of(&resp), "08P01");
}

// --- 接続契約 ------------------------------------------------------------

#[test]
fn response_is_connection_close_even_with_keep_alive_request_header() {
    let addr = listener();
    let request = build_request(
        "/v1/query",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", "0"),
            ("Connection", "keep-alive"),
        ],
        b"",
    );
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    // parse_single_response 自体が Connection: close を検証する。
    let resp = parse_single_response(&received);
    assert_reached_router(&resp);
}

/// 1 回の書き込みへ整形済み要求を 2 本連結（パイプライン）しても、応答は
/// ちょうど 1 個（2 本目は解釈されない。非パイプライン設計の外形証跡）。
///
/// 2 本目の要求バイト列は 1 本目の `Content-Length` から見れば境界の無い
/// 余剰バイトそのものであり、`rejects_body_longer_than_declared_content_length`
/// と同じ「読み取りの分割が OS 依存で決定不能」な事情を持つ（1 本目の本文
/// ちょうどの直後に 2 本目の要求行が続くため、サーバーは高い確率で「本文が
/// 宣言長を超えている」と即時検知するが、分割によっては 1 本目の本文
/// ちょうどまでしか同時に届かず `PlaceholderRouter` へ到達する場合もある。
/// いずれの分岐でも status／wire_code は 400／08P01 で一致する〔モジュール
/// doc の codex-review 指摘対応節を参照〕）。「応答はちょうど 1 個（2 本目は
/// 解釈されない）」ことは `parse_single_response` の不変条件
/// （`Content-Length` と本文の一致・`Connection: close`）そのものが保証する
/// ため、ここでは単一応答＋接続終了のみを固定する。
#[test]
fn pipelined_second_request_is_not_interpreted() {
    let addr = listener();
    let mut request = well_formed_request(b"{}");
    request.extend_from_slice(&well_formed_request(b"{}"));
    let received = send_raw(addr, &request, AfterWrite::KeepOpen);
    let resp = parse_single_response(&received);
    assert_eq!(
        resp.status,
        400,
        "unexpected status (body: {:?})",
        String::from_utf8_lossy(&resp.body)
    );
    assert_eq!(wire_code_of(&resp), "08P01");
}

/// `Expect` ヘッダ（値を問わず）は暫定応答を送る経路が無いため、最終エラー
/// 応答（`FeatureNotSupported`／`0A000`／501）へ倒れる（PR #810 レビュー
/// 対応の回帰固定）。
#[test]
fn rejects_expect_100_continue() {
    let addr = listener();
    let request = build_request(
        "/v1/query",
        &[
            ("Content-Type", "application/json"),
            ("Content-Length", "0"),
            ("Expect", "100-continue"),
        ],
        b"",
    );
    let received = send_raw(addr, &request, AfterWrite::HalfClose);
    let resp = parse_single_response(&received);
    assert_rejected(&resp, 501, "0A000");
}

/// 起動ヘルパー自体が `SERVER_READ_TIMEOUT` を使っていることの健全性確認
/// （テストが恒久的にハングしない設計であることの明示）。
#[test]
fn server_read_timeout_constant_is_bounded() {
    assert!(SERVER_READ_TIMEOUT <= Duration::from_secs(30));
}
