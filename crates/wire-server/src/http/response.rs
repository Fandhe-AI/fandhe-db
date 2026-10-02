//! ステータスコード＋JSON 本文 → HTTP/1.1 応答バイト列への組み立て（NoSQL 表層。
//! Issue #746。対象ビヘイビア HTTP-2・HTTP-3・ERR-4・ERR-5。ポインタ:
//! `docs/spec/05-tasks.md` TASK-173・`docs/spec/04-behavior/http-transport.md`
//! HTTP-2・HTTP-3）。
//!
//! 責務境界: [`crate::http::status`]（`ErrorClass` → HTTP ステータスの射影）・
//! [`crate::http::error_body`]（`ErrorClass` → JSON エラー本文）の合成先として、
//! 「ステータス行＋固定ヘッダ（`Content-Type`／`Content-Length`／
//! `Connection: close`）＋空行＋本文」という RFC 9112 準拠のバイト列を組み立てる
//! だけの純関数を提供する。ソケット I/O・未読データの読み捨て・接続クローズは
//! 呼び出し元（後続 Issue #747 の接続ハンドラ）の責務であり、本モジュールは
//! `Vec<u8>` を返すところまでに閉じる（`crate::result_encoder`・
//! `crate::error_response` と同じ方針）。
//!
//! NoSQL 表層は「1 応答＝1 TCP 接続」（keep-alive 非対応）の転送路であるため、
//! `Connection: close` を常に固定で付ける。`Date` ヘッダは RFC 9110 §6.6.1 で
//! 時計を持つ origin server の 2xx/3xx/4xx 応答に MUST（必須）と規定されている
//! ため常に付与する（PR #809 レビュー対応。従前の実装コメントは SHOULD と誤記
//! していた）。決定的出力の方針は「時刻を呼び出し元から受け取る」ことで維持する
//! ——本モジュール自身は `SystemTime::now()` を参照せず、[`encode`]・
//! [`encode_ok`]・[`encode_error`]・[`encode_error_may_be_committed`] はいずれも
//! `now: SystemTime` を引数に取り、同じ `now` を渡せば常に同じバイト列を返す
//! （[`crate::http::date::format_http_date`] へ委譲する純粋な変換のみ）。
//! `SystemTime::now()` の呼び出しは接続ハンドラ（後続 Issue #747）の責務。
//!
//! `Content-Length` は本文の **バイト長**（`body.len()`）。`body` は `&str`（型で
//! UTF-8 を保証）だが、それが妥当な JSON であることの保証は呼び出し元の契約
//! （`error_body::encode`／`error_body::encode_may_be_committed` の出力、または
//! 後続 Issue の成功本文シリアライザ）に委ね、本モジュールでは検証しない。
//!
//! `message` 引数の契約は [`crate::http::error_body`] と同一: 呼び出し元は固定の
//! 英語文言、または `engine::error_format::WireError` 由来の値のみを渡すこと
//! （他テナントのデータ・存在情報・内部詳細を含めない。`.claude/rules/security.md`
//! P0）。
//!
//! `401` 応答には RFC 9110 §11.6.1 が要求する認証チャレンジ（`WWW-Authenticate`）
//! を必ず 1 つ以上付与する（PR #809 レビュー対応）。本表層の認証方式は
//! `Authorization: Bearer <token>`（`crate::http::session`）のみのため、
//! `WWW-Authenticate: Bearer` を固定で用いる。`ErrorClass::AuthRequired`／
//! `AuthInvalid` の 2 分類のみが `http_status` で `401` に写像される
//! （`http::status` の網羅テーブルで固定済み）ため、[`encode_known`] は
//! ステータスコードが `401` かどうかだけを見て判定すれば分類を問わず正しい。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use engine::error_format::ErrorClass;

use crate::http::{date::format_http_date, error_body, status::http_status};

/// 応答本文の固定 `Content-Type` 値。
pub const CONTENT_TYPE_JSON_UTF8: &str = "application/json; charset=utf-8";

/// 表外（[`reason_phrase`] が `None` を返す）ステータスコードを [`encode`] へ
/// 渡した場合の fail-closed エラー。理由句を捏造して送出しない
/// （`crate::result_encoder::EncodeError` と同型の unit struct）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResponseEncodeError;

/// ステータス行の固定オーバーヘッド見積り（ステータス行＋ヘッダ行〔`Date`・
/// 401 時の `WWW-Authenticate` を含む最大構成〕＋空行の定数部分。`body`／
/// `Content-Length` の桁数を除く上限バイト数）。事前確保の目安値であり超過
/// しても正しさには影響しない。
const FIXED_OVERHEAD: usize = 220;

/// クローズドな理由句表。`200` と [`http_status`] の全値域（`ErrorClass::ALL`
/// が写像しうる `{400, 401, 403, 404, 409, 413, 500, 501, 503}`）のみを持つ。
/// 表外のステータスコードは `None`（[`encode`] の fail-closed 判定の唯一の
/// 情報源）。値はいずれも RFC 9110 の標準句・ASCII のみ・CR/LF を含まない
/// （ヘッダインジェクション面がこの定数表のみであることをテストで固定する）。
pub const fn reason_phrase(status: u16) -> Option<&'static str> {
    match status {
        200 => Some("OK"),
        400 => Some("Bad Request"),
        401 => Some("Unauthorized"),
        403 => Some("Forbidden"),
        404 => Some("Not Found"),
        409 => Some("Conflict"),
        413 => Some("Content Too Large"),
        500 => Some("Internal Server Error"),
        501 => Some("Not Implemented"),
        503 => Some("Service Unavailable"),
        _ => None,
    }
}

/// `now`（`SystemTime`）→ `Date` ヘッダ値。クロックが `UNIX_EPOCH` より前を
/// 指す異常系（NTP 未同期のシステムクロック等）は `panic` させず `Duration::
/// ZERO`（`UNIX_EPOCH` 自体の日時）へ fail-closed に縮退する
/// （`.claude/rules/coding-rust.md` の fail-closed 方針。応答自体の送出を
/// 妨げないことを優先し、縮退時の `Date` 値の正確性より可用性を取る）。
fn date_header_value(now: SystemTime) -> String {
    let elapsed = now.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    format_http_date(elapsed)
}

/// `status`／`reason`（呼び出し元が確定済みの組）＋`body`＋`now` から応答バイト列を
/// 組み立てる共有実体（`now` を除き infallible）。[`encode`]・[`encode_ok`] の
/// 双方が委譲する。
///
/// 出力レイアウト（固定順。RFC 9110 上ヘッダ間の順序に意味はないが、
/// `Content-Length` 算出対象を安定させるため golden bytes テストで固定する）:
///
/// ```text
/// HTTP/1.1 <status> <reason>\r\n
/// Content-Type: application/json; charset=utf-8\r\n
/// Content-Length: <body.len() の 10 進>\r\n
/// Connection: close\r\n
/// Date: <now の HTTP-date>\r\n
/// WWW-Authenticate: Bearer\r\n  (status == 401 のときのみ)
/// \r\n
/// <body>
/// ```
fn encode_known(status: u16, reason: &'static str, body: &str, now: SystemTime) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len().saturating_add(FIXED_OVERHEAD));
    out.extend_from_slice(b"HTTP/1.1 ");
    // u16 は高々 3 桁。`itoa` 相当の追加依存を避け `to_string` で十分
    // （依存追加なし方針・`.claude/rules/dependency-policy.md`）。
    out.extend_from_slice(status.to_string().as_bytes());
    out.push(b' ');
    out.extend_from_slice(reason.as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(b"Content-Type: ");
    out.extend_from_slice(CONTENT_TYPE_JSON_UTF8.as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(b"Content-Length: ");
    out.extend_from_slice(body.len().to_string().as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(b"Connection: close\r\n");
    out.extend_from_slice(b"Date: ");
    out.extend_from_slice(date_header_value(now).as_bytes());
    out.extend_from_slice(b"\r\n");
    if status == 401 {
        // RFC 9110 §11.6.1: 401 応答は認証チャレンジを最低 1 つ含める MUST。
        // 本表層の認証方式は Bearer トークンのみ（`crate::http::session`）。
        out.extend_from_slice(b"WWW-Authenticate: Bearer\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body.as_bytes());
    out
}

/// ステータスコード＋JSON 本文 → 応答バイト列（汎用形）。
///
/// `status` が [`reason_phrase`] の表外（`None`）の場合は `Err`（捏造した理由句
/// や空理由句で応答を送出しない。fail-closed）。
pub fn encode(status: u16, body: &str, now: SystemTime) -> Result<Vec<u8>, ResponseEncodeError> {
    match reason_phrase(status) {
        Some(reason) => Ok(encode_known(status, reason, body, now)),
        None => Err(ResponseEncodeError),
    }
}

/// 成功応答（`200 OK`）。理由句は表に常在するため `Result` を経由しない
/// infallible な便宜 API。
pub fn encode_ok(body: &str, now: SystemTime) -> Vec<u8> {
    encode_known(200, "OK", body, now)
}

/// 通常エラー応答: [`http_status`] のステータス ＋ [`error_body::encode`] の本文
/// を合成する。
///
/// `reason_phrase(http_status(class))` が `None` になることは `http_status` の
/// 値域（`{400, 401, 403, 404, 409, 413, 500, 501, 503}`）が [`reason_phrase`] の
/// 表に全て含まれるため構造上到達不能だが、`ErrorClass` へ将来 variant が
/// 追加され `http_status` の値域が広がった場合に理由句を捏造して送出しない
/// よう、fail-closed の縮退（`500 Internal Server Error` ＋ `error_body::encode`
/// の固定文言）を用意する（`ErrorClass::ALL` 全走査テストで非到達を固定する）。
pub fn encode_error(class: ErrorClass, message: &str, now: SystemTime) -> Vec<u8> {
    let status = http_status(class);
    let body = error_body::encode(class, message);
    encode(status, &body, now).unwrap_or_else(|ResponseEncodeError| {
        encode_known(
            500,
            "Internal Server Error",
            &error_body::encode(ErrorClass::InternalError, "internal error"),
            now,
        )
    })
}

/// 緊急応答（`RECOVER-5` (3)・ERR-5 ポインタ。commit 後 panic 時に限り呼び出す
/// 契約。該当判定は呼び出し元＝接続ハンドラ〔#747〕の責務）: [`http_status`] の
/// ステータス ＋ [`error_body::encode_may_be_committed`] の本文（`data` 付き）を
/// 合成する。誤って通常応答へ `data` が混入しないよう [`encode_error`] とは
/// 構造的に分離した専用 API とする（`error_body` の設計方針を踏襲）。
///
/// 縮退時の扱いは [`encode_error`] と同一（`500` ＋ 通常本文の固定文言。
/// 緊急応答であっても表外ステータスを捏造した理由句で送出しない）。
pub fn encode_error_may_be_committed(class: ErrorClass, message: &str, now: SystemTime) -> Vec<u8> {
    let status = http_status(class);
    let body = error_body::encode_may_be_committed(class, message);
    encode(status, &body, now).unwrap_or_else(|ResponseEncodeError| {
        encode_known(
            500,
            "Internal Server Error",
            &error_body::encode(ErrorClass::InternalError, "internal error"),
            now,
        )
    })
}

/// 分割実行 DML の部分完了（`VD001`）・取り消し（`VD002`）の応答（Issue #1130・
/// NOSQL-12・ERR-4）: [`http_status`]（いずれも 409）＋
/// [`error_body::encode_partitioned`] の本文（`data.committed`・`data.operation_id` 付き）。
/// 呼び出しは `SqlSurfaceError::PartialCompletion`／`PartitionedDmlCancelled` の 2 variant に
/// 限る（`wire-server::http::query::partitioned`）。縮退時は [`encode_error`] と同一。
pub fn encode_error_partitioned(
    class: ErrorClass,
    message: &str,
    committed: u64,
    operation_id: &str,
    now: SystemTime,
) -> Vec<u8> {
    let status = http_status(class);
    let body = error_body::encode_partitioned(class, message, committed, operation_id);
    encode(status, &body, now).unwrap_or_else(|ResponseEncodeError| {
        encode_known(
            500,
            "Internal Server Error",
            &error_body::encode(ErrorClass::InternalError, "internal error"),
            now,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の固定時刻（`2023-11-14T22:13:20Z`）。golden bytes テストの
    /// `Date` ヘッダ値の根拠。決定的出力の契約（同じ `now` → 同じバイト列）を
    /// 検証するため、`SystemTime::now()` はテストから一切呼ばない。
    fn test_now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_700_000_000)
    }

    /// 応答バイト列をヘッダ部（行ごとの `名前: 値`）と本文へ分割するテスト専用
    /// ミニパーサ。`unwrap`/`expect` はテストコードでは許容する
    /// （`.claude/rules/coding-rust.md` の添字アクセス禁止は受信入力経路が対象。
    /// `error_body.rs` の tests と同じ方針）。
    struct ParsedResponse<'a> {
        status_line: &'a str,
        headers: Vec<(&'a str, &'a str)>,
        body: &'a [u8],
    }

    fn parse(bytes: &[u8]) -> ParsedResponse<'_> {
        let text_prefix_end = bytes
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("response must contain a blank line separator");
        let head = std::str::from_utf8(&bytes[..text_prefix_end]).expect("head must be ASCII");
        let body = &bytes[text_prefix_end + 4..];

        let mut lines = head.split("\r\n");
        let status_line = lines.next().expect("status line present");
        let headers = lines
            .map(|line| {
                let (name, value) = line.split_once(": ").expect("header must be name: value");
                assert!(
                    !name.contains(['\r', '\n']) && !value.contains(['\r', '\n']),
                    "header must not contain embedded CR/LF: {line:?}"
                );
                (name, value)
            })
            .collect();

        ParsedResponse {
            status_line,
            headers,
            body,
        }
    }

    fn assert_well_formed(bytes: &[u8]) -> ParsedResponse<'_> {
        let parsed = parse(bytes);
        let tokens: Vec<&str> = parsed.status_line.splitn(3, ' ').collect();
        assert_eq!(tokens.len(), 3, "status line: {:?}", parsed.status_line);
        assert_eq!(tokens[0], "HTTP/1.1");
        assert!(
            tokens[1].len() == 3 && tokens[1].bytes().all(|b| b.is_ascii_digit()),
            "status code must be 3 digits: {:?}",
            tokens[1]
        );
        assert!(!tokens[2].is_empty(), "reason phrase must not be empty");

        let connection_close_count = parsed
            .headers
            .iter()
            .filter(|(name, value)| *name == "Connection" && *value == "close")
            .count();
        assert_eq!(
            connection_close_count, 1,
            "Connection: close must appear exactly once"
        );

        let content_length: usize = parsed
            .headers
            .iter()
            .find(|(name, _)| *name == "Content-Length")
            .map(|(_, value)| {
                value
                    .parse()
                    .expect("Content-Length must be a decimal number")
            })
            .expect("Content-Length header must be present");
        assert_eq!(
            content_length,
            parsed.body.len(),
            "Content-Length must match remaining byte count"
        );

        let content_type_count = parsed
            .headers
            .iter()
            .filter(|(name, value)| *name == "Content-Type" && *value == CONTENT_TYPE_JSON_UTF8)
            .count();
        assert_eq!(
            content_type_count, 1,
            "Content-Type header must appear exactly once"
        );

        let date_values: Vec<&&str> = parsed
            .headers
            .iter()
            .filter(|(name, _)| *name == "Date")
            .map(|(_, value)| value)
            .collect();
        assert_eq!(
            date_values.len(),
            1,
            "Date header must appear exactly once (RFC 9110 6.6.1)"
        );
        let date_value = *date_values[0];
        assert_eq!(
            date_value.len(),
            29,
            "Date header must be IMF-fixdate (29 bytes): {date_value:?}"
        );
        assert!(date_value.is_ascii());

        let www_authenticate_count = parsed
            .headers
            .iter()
            .filter(|(name, _)| *name == "WWW-Authenticate")
            .count();
        if tokens[1] == "401" {
            assert_eq!(
                www_authenticate_count, 1,
                "401 response must carry exactly one WWW-Authenticate challenge (RFC 9110 11.6.1)"
            );
        } else {
            assert_eq!(
                www_authenticate_count, 0,
                "non-401 response must not carry WWW-Authenticate"
            );
        }

        parsed
    }

    /// golden bytes: `encode_ok` の全バイト列を厳密一致で固定する。
    #[test]
    fn encode_ok_golden_bytes() {
        assert_eq!(
            encode_ok("{}", test_now()),
            b"HTTP/1.1 200 OK\r\n\
Content-Type: application/json; charset=utf-8\r\n\
Content-Length: 2\r\n\
Connection: close\r\n\
Date: Tue, 14 Nov 2023 22:13:20 GMT\r\n\
\r\n\
{}"
            .to_vec()
        );
    }

    /// golden bytes: `encode_error` の全バイト列を厳密一致で固定する。
    #[test]
    fn encode_error_golden_bytes() {
        let bytes = encode_error(ErrorClass::InternalError, "internal error", test_now());
        assert_eq!(
            bytes,
            b"HTTP/1.1 500 Internal Server Error\r\n\
Content-Type: application/json; charset=utf-8\r\n\
Content-Length: 82\r\n\
Connection: close\r\n\
Date: Tue, 14 Nov 2023 22:13:20 GMT\r\n\
\r\n\
{\"error\":{\"wire_code\":\"XX000\",\"code\":\"INTERNAL_ERROR\",\"message\":\"internal error\"}}"
                .to_vec()
        );
    }

    /// golden bytes: 401（`AuthRequired`）の `WWW-Authenticate` 付与を厳密一致で
    /// 固定する（PR #809 レビュー対応・RFC 9110 §11.6.1）。
    #[test]
    fn encode_error_401_golden_bytes_includes_www_authenticate() {
        let bytes = encode_error(
            ErrorClass::AuthRequired,
            "authentication required",
            test_now(),
        );
        assert_eq!(
            bytes,
            b"HTTP/1.1 401 Unauthorized\r\n\
Content-Type: application/json; charset=utf-8\r\n\
Content-Length: 90\r\n\
Connection: close\r\n\
Date: Tue, 14 Nov 2023 22:13:20 GMT\r\n\
WWW-Authenticate: Bearer\r\n\
\r\n\
{\"error\":{\"wire_code\":\"28000\",\"code\":\"AUTH_REQUIRED\",\"message\":\"authentication required\"}}"
                .to_vec()
        );
    }

    /// 全 `ErrorClass` について `encode_error`／`encode_error_may_be_committed`
    /// の応答が RFC 9112 準拠の構文（ステータス行・ヘッダ・空行・本文）を
    /// 満たすことを固定する。
    #[test]
    fn all_error_classes_produce_well_formed_responses() {
        for class in ErrorClass::ALL {
            let normal = encode_error(class, "boom", test_now());
            assert_well_formed(&normal);

            let emergency = encode_error_may_be_committed(class, "boom", test_now());
            assert_well_formed(&emergency);
        }
    }

    /// `AuthRequired`／`AuthInvalid`（401 に写像される 2 分類のみ）だけが
    /// `WWW-Authenticate` を持ち、他の分類は持たないことを非 vacuous に固定する
    /// （`assert_well_formed` の判定条件がステータス値のみに依存することの
    /// 直接的な裏付け）。
    #[test]
    fn only_401_classes_carry_www_authenticate() {
        let classes_with_401: Vec<ErrorClass> = ErrorClass::ALL
            .into_iter()
            .filter(|c| http_status(*c) == 401)
            .collect();
        assert_eq!(
            classes_with_401,
            vec![ErrorClass::AuthInvalid, ErrorClass::AuthRequired],
            "401 に写像される分類の集合が変化した場合はこのテストを更新すること"
        );
        for class in classes_with_401 {
            let bytes = encode_error(class, "boom", test_now());
            let parsed = parse(&bytes);
            let count = parsed
                .headers
                .iter()
                .filter(|(name, _)| *name == "WWW-Authenticate")
                .count();
            assert_eq!(count, 1, "class={class:?}");
        }
    }

    /// 200 と [`http_status`] の全値域に [`reason_phrase`] が `Some` で存在する
    /// （非 vacuous リンク: 表の欠落を検出する）。
    #[test]
    fn reason_phrase_covers_200_and_all_error_class_statuses() {
        assert!(reason_phrase(200).is_some());
        for class in ErrorClass::ALL {
            let status = http_status(class);
            assert!(
                reason_phrase(status).is_some(),
                "status {status} (class={class:?}) must have a reason phrase"
            );
        }
    }

    /// `encode_error` が `encode(http_status(class), &error_body::encode(...))`
    /// と一致する（fail-closed 縮退分岐が発火しないことの固定）。
    /// `encode_error_may_be_committed` も同様に `error_body::
    /// encode_may_be_committed` と一致する。
    #[test]
    fn encode_error_matches_manual_composition_for_all_classes() {
        for class in ErrorClass::ALL {
            let status = http_status(class);

            let expected_normal_body = error_body::encode(class, "msg");
            let expected_normal = encode(status, &expected_normal_body, test_now())
                .expect("status must be within reason_phrase table");
            assert_eq!(
                encode_error(class, "msg", test_now()),
                expected_normal,
                "class={class:?}"
            );

            let expected_emergency_body = error_body::encode_may_be_committed(class, "msg");
            let expected_emergency = encode(status, &expected_emergency_body, test_now())
                .expect("status must be within reason_phrase table");
            assert_eq!(
                encode_error_may_be_committed(class, "msg", test_now()),
                expected_emergency,
                "class={class:?}"
            );

            // 通常応答の本文には緊急応答専用の `data` が現れない。
            let normal_body_bytes = &encode_error(class, "msg", test_now())[..];
            let normal_text =
                std::str::from_utf8(normal_body_bytes).expect("ascii head + utf8 body");
            assert!(
                !normal_text.contains("\"data\""),
                "class={class:?} normal response must not contain data"
            );
        }
    }

    /// `Content-Length` はバイト長: 非 ASCII 本文（CJK 3 文字 → 9 バイト）。
    #[test]
    fn content_length_counts_bytes_not_chars_for_non_ascii_body() {
        let body = "日本語";
        assert_eq!(body.len(), 9);
        let bytes = encode_ok(body, test_now());
        let parsed = assert_well_formed(&bytes);
        assert_eq!(parsed.body, body.as_bytes());
    }

    /// `Content-Length` は空本文で 0。
    #[test]
    fn content_length_is_zero_for_empty_body() {
        let bytes = encode_ok("", test_now());
        let parsed = assert_well_formed(&bytes);
        assert_eq!(parsed.body.len(), 0);
    }

    /// 本文中に改行を含んでいても `Content-Length` は本文の総バイト長のみに
    /// 依存する（ヘッダ部との境界は先頭の空行のみで決まる）。
    #[test]
    fn content_length_depends_only_on_body_with_embedded_newlines() {
        let body = "a\r\nb\nc";
        let bytes = encode_ok(body, test_now());
        let parsed = assert_well_formed(&bytes);
        assert_eq!(parsed.body, body.as_bytes());
    }

    /// fail-closed: 表外のステータスコード（999／0／418）は `Err` を返す。
    #[test]
    fn encode_rejects_unknown_status_codes() {
        assert_eq!(encode(999, "{}", test_now()), Err(ResponseEncodeError));
        assert_eq!(encode(0, "{}", test_now()), Err(ResponseEncodeError));
        assert_eq!(encode(418, "{}", test_now()), Err(ResponseEncodeError));
    }

    /// 理由句表の全値が ASCII かつ CR/LF を含まない（ヘッダインジェクション面が
    /// 定数表のみであることの固定）。
    #[test]
    fn all_reason_phrases_are_ascii_without_crlf() {
        let candidates = [
            200, 400, 401, 403, 404, 409, 413, 500, 501, 503, 999, 0, 418,
        ];
        for status in candidates {
            if let Some(reason) = reason_phrase(status) {
                assert!(
                    reason.is_ascii(),
                    "status {status}: {reason:?} must be ASCII"
                );
                assert!(
                    !reason.contains(['\r', '\n']),
                    "status {status}: {reason:?} must not contain CR/LF"
                );
            }
        }
    }

    /// 同一入力（同一 `now` を含む）からは常に同一出力（外部状態・乱数への
    /// 非依存の確認。`now` 自体は引数として明示的に渡すため、ここでは
    /// 同じ `now` を 2 回使うことで「時刻を除く外部状態非依存」を固定する）。
    #[test]
    fn encode_is_deterministic() {
        for class in ErrorClass::ALL {
            assert_eq!(
                encode_error(class, "x", test_now()),
                encode_error(class, "x", test_now()),
                "class={class:?}"
            );
            assert_eq!(
                encode_error_may_be_committed(class, "x", test_now()),
                encode_error_may_be_committed(class, "x", test_now()),
                "class={class:?}"
            );
        }
        assert_eq!(encode_ok("{}", test_now()), encode_ok("{}", test_now()));
    }

    /// `now` を変えると `Date` ヘッダのみが変わる（決定的出力の契約が「時刻を
    /// 呼び出し元から受け取る」設計で成立していることの直接確認。PR #809
    /// レビュー対応）。
    #[test]
    fn date_header_reflects_now_argument() {
        let now_a = UNIX_EPOCH + Duration::from_secs(0);
        let now_b = UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        let bytes_a = encode_ok("{}", now_a);
        let bytes_b = encode_ok("{}", now_b);
        assert_ne!(bytes_a, bytes_b);

        let date_a = parse(&bytes_a)
            .headers
            .into_iter()
            .find(|(name, _)| *name == "Date")
            .map(|(_, value)| value.to_string())
            .expect("Date header present");
        let date_b = parse(&bytes_b)
            .headers
            .into_iter()
            .find(|(name, _)| *name == "Date")
            .map(|(_, value)| value.to_string())
            .expect("Date header present");
        assert_eq!(date_a, "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(date_b, "Tue, 14 Nov 2023 22:13:20 GMT");
    }

    /// クロックが `UNIX_EPOCH` より前を指す異常系（システムクロック異常）でも
    /// `panic` せず `UNIX_EPOCH` の日時へ fail-closed に縮退する
    /// （`date_header_value` の `unwrap_or(Duration::ZERO)` の固定）。
    #[test]
    fn date_header_falls_back_to_epoch_when_clock_before_unix_epoch() {
        let before_epoch = UNIX_EPOCH - Duration::from_secs(1);
        let bytes = encode_ok("{}", before_epoch);
        let parsed = assert_well_formed(&bytes);
        let date_value = parsed
            .headers
            .iter()
            .find(|(name, _)| *name == "Date")
            .map(|(_, value)| *value)
            .expect("Date header present");
        assert_eq!(date_value, "Thu, 01 Jan 1970 00:00:00 GMT");
    }

    /// `const fn` 固定: `reason_phrase` が定数文脈から利用できる形態を先取りして
    /// コンパイル時に検証する（`status`／`error_body` と同じ方針）。
    const _: Option<&str> = reason_phrase(200);
}
