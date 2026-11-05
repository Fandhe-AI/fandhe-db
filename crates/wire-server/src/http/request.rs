//! HTTP 要求行（`<method> SP <target> SP <version> CRLF`）の解析。
//!
//! [`crate::http`] の転送路のうち、TCP から読み取ったバイト列の先頭を
//! 「メソッド・ターゲット・バージョン」へ分解する純関数だけを提供する。
//! ストリームからの有界読み取り・EOF 判断は呼び出し元（Issue #747 の接続
//! ハンドラ）が担い、[`consumed`](RequestLineParse::Complete) が指す
//! オフセット以降を後続のヘッダパーサ（Issue #741）へ渡す想定である。
//!
//! 受理するのは `POST` メソッドと `GET /v1/openapi.json`（Issue #2244。GET は
//! パスと一体で判定する唯一の例外）・`HTTP/1.1` のみ（閉じた語彙）で、それ以外
//! （非対応メソッド・非対応バージョン・CRLF 以外の行終端・4 KiB 超過・トークン
//! 数不正等）はすべて [`FrameError::Malformed`] として拒否する（fail-closed）。
//! `FrameError` は `crate::framing` が pg wire 経路向けに定義した型をそのまま
//! 流用し、`error_class()` は `08P01`（`ErrorClass::ProtocolViolation`）へ
//! 写像される（`.claude/rules/coding-rust.md` の fail-closed 方針を HTTP 経路
//! でも共有する）。
//!
//! 受信データ経路のため `unwrap`/`expect`/添字アクセス（`[]`）を用いず、
//! `get()`・`first()`・`checked_*` で処理する。ヒープ確保も行わない
//! （[`RequestLine::target`] は入力バッファからの借用）。
//!
//! 対応: TASK-173（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア HTTP-2,
//! HTTP-11）。

use crate::framing::FrameError;

/// 要求行（CRLF を含む行全体）の上限バイト数（HTTP-11）。
///
/// 実行時設定・CLI での緩和経路を持たない固定定数（`framing::MAX_MESSAGE_LEN`
/// 等と同様、untrusted なピアに許すアロケーション量を構造的に絞る）。
pub const MAX_REQUEST_LINE_LEN: usize = 4 * 1024;

/// OpenAPI 文書の配信ターゲット（NOSQL-20、Issue #2244）。`GET` はこのターゲットに
/// バイト厳密一致する場合だけ受理する（パス判定の単一情報源。ルータも参照する）。
pub const OPENAPI_TARGET: &str = "/v1/openapi.json";

/// 受理するメソッドの閉じた語彙（HTTP-2）。`POST` と、[`OPENAPI_TARGET`] に
/// 限った `GET` だけを受理する。それ以外は [`parse_request_line`] の時点で
/// `Malformed` として拒否するため、他のバリアントは持たない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Post,
    /// 本文を持たない取得要求。`/v1/openapi.json` 以外のターゲットでは構築されない。
    Get,
}

/// 受理する HTTP バージョンの閉じた語彙（HTTP-2）。`HTTP/1.1` 以外
/// （`HTTP/1.0`・`HTTP/2` 等）は `Malformed` として拒否する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    Http11,
}

/// 解析済み要求行。`target` は入力バッファ（呼び出し元が保持するリクエスト
/// バッファ）からの借用であり、コピーしない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestLine<'a> {
    pub method: Method,
    pub target: &'a str,
    pub version: Version,
}

/// [`parse_request_line`] の結果。
#[derive(Debug, PartialEq, Eq)]
pub enum RequestLineParse<'a> {
    /// 要求行を 1 行分解析できた。`consumed` は入力先頭からの消費バイト数
    /// （終端 CRLF の直後を指す）で、呼び出し元はこの位置からヘッダの解析
    /// （Issue #741）を続ける。
    Complete {
        line: RequestLine<'a>,
        consumed: usize,
    },
    /// 上限バイト数未満の範囲内で CRLF に到達しなかった。プロトコル違反では
    /// なく「まだ全部届いていない」状態であり、呼び出し元が追加でバイトを
    /// 読み取るか、相手が切断していれば EOF として扱う（Issue #747 の責務。
    /// ここで `FrameError::Truncated` を返さないのは、本関数がストリームの
    /// 読み取り継続可否を判断できる立場にないため）。
    Incomplete,
}

/// 要求行を解析する。入力 `input` は要求行の後ろにヘッダ等が続いていてよい
/// （その部分は無視し、`consumed` で境界だけを返す）。
pub fn parse_request_line(input: &[u8]) -> Result<RequestLineParse<'_>, FrameError> {
    let lf_idx = match find_crlf_within(input, MAX_REQUEST_LINE_LEN)? {
        Some(idx) => idx,
        None => {
            if input.len() >= MAX_REQUEST_LINE_LEN {
                return Err(FrameError::Malformed("request line exceeds limit"));
            }
            return Ok(RequestLineParse::Incomplete);
        }
    };
    // find_crlf_within は「\r の直後に \n がある」位置でのみ Some(lf_idx) を
    // 返す契約のため、\r は必ず lf_idx-1 に存在する（lf_idx >= 1）。万一の
    // 不変条件破れも panic ではなく Malformed で fail-closed に倒す。
    let cr_idx = lf_idx
        .checked_sub(1)
        .ok_or(FrameError::Malformed("request line missing CR"))?;
    let line = input
        .get(..cr_idx)
        .ok_or(FrameError::Malformed("request line slice out of range"))?;
    let consumed = lf_idx
        .checked_add(1)
        .ok_or(FrameError::Malformed("request line consumed overflow"))?;

    let mut tokens = line.split(|&b| b == b' ');
    let method_bytes = tokens
        .next()
        .ok_or(FrameError::Malformed("missing method token"))?;
    let target_bytes = tokens
        .next()
        .ok_or(FrameError::Malformed("missing target token"))?;
    let version_bytes = tokens
        .next()
        .ok_or(FrameError::Malformed("missing version token"))?;
    if tokens.next().is_some() {
        return Err(FrameError::Malformed("too many request line tokens"));
    }

    let target = validate_target(target_bytes)?;
    let method = parse_method(method_bytes, target)?;
    let version = parse_version(version_bytes)?;

    Ok(RequestLineParse::Complete {
        line: RequestLine {
            method,
            target,
            version,
        },
        consumed,
    })
}

/// メソッドトークンを検証する（大文字小文字を区別。HTTP-2）。`POST` は常に受理し、
/// `GET` は [`OPENAPI_TARGET`] とバイト厳密一致のときだけ受理する。これにより
/// 「非 POST は要求行パーサで拒否されルータへ到達しない」契約を、当該パス以外の
/// すべてで維持する（GET 用の緩和が及ぶ範囲を構造的に 1 パスへ限定する）。
fn parse_method(bytes: &[u8], target: &str) -> Result<Method, FrameError> {
    if bytes == b"POST" {
        Ok(Method::Post)
    } else if bytes == b"GET" && target == OPENAPI_TARGET {
        Ok(Method::Get)
    } else {
        Err(FrameError::Malformed("unsupported request method"))
    }
}

/// バージョントークンが `HTTP/1.1` と完全一致するかを検証する（HTTP-2）。
fn parse_version(bytes: &[u8]) -> Result<Version, FrameError> {
    if bytes == b"HTTP/1.1" {
        Ok(Version::Http11)
    } else {
        Err(FrameError::Malformed("unsupported HTTP version"))
    }
}

/// ターゲットトークンを検証する。origin-form（`/` 始まり）かつ非空・全バイト
/// 可視 ASCII（`0x21..=0x7E`）のみを受理する。absolute-form（`http://...`）・
/// asterisk-form（`*`）は先頭バイト検査で自動的に排除される。パス自体の
/// 妥当性（許可リストとの一致等）はルータ層（Issue #758）の責務であり、本層
/// では判定しない。
fn validate_target(bytes: &[u8]) -> Result<&str, FrameError> {
    if bytes.is_empty() {
        return Err(FrameError::Malformed("empty request target"));
    }
    if bytes.first() != Some(&b'/') {
        return Err(FrameError::Malformed("request target must be origin-form"));
    }
    if !bytes.iter().all(|&b| (0x21..=0x7e).contains(&b)) {
        return Err(FrameError::Malformed(
            "request target contains invalid byte",
        ));
    }
    std::str::from_utf8(bytes)
        .map_err(|_| FrameError::Malformed("request target is not valid utf-8"))
}

/// `input` の先頭から `min(input.len(), max_len)` バイトの窓の中で、行終端
/// CRLF を探す共通ヘルパ（ヘッダパーサ Issue #741 からの再利用を想定）。
///
/// - 窓の中に bare LF（直前が `\r` でない `\n`）・bare CR（直後が `\n` でない
///   `\r`）を見つけた場合は `Malformed` を返す。
/// - 窓の中に CRLF が見つかれば、`\n` のインデックスを `Some` で返す。
/// - 窓の中に見つからなければ `Ok(None)`（呼び出し元が `input.len()` と
///   `max_len` を比べて「上限超過」か「まだ届いていない」かを判断する）。
///
/// 走査は `max_len` を超えて読み進めない（untrusted 入力に対する上限検証を
/// 実際の探索より先に効かせるための境界。coding-rust.md 準拠）。
pub fn find_crlf_within(input: &[u8], max_len: usize) -> Result<Option<usize>, FrameError> {
    let limit = input.len().min(max_len);
    let mut i = 0usize;
    while i < limit {
        let byte = *input
            .get(i)
            .ok_or(FrameError::Malformed("crlf scan index out of range"))?;
        match byte {
            b'\r' => {
                // 周辺コード（下の checked_add・呼び出し元の checked_sub 等）
                // との様式統一のため checked_add(1) を使う。i < limit ≤
                // input.len() ≤ isize::MAX なのでオーバーフローは実質発生
                // しないが、万一の不変条件破れも panic ではなく Malformed で
                // fail-closed に倒す。
                let next = i
                    .checked_add(1)
                    .ok_or(FrameError::Malformed("crlf scan index overflow"))?;
                if next >= limit {
                    // \r が窓の末尾にある。直後のバイトが未着（本当に途中）か、
                    // 上限に達していて読み進められないかのいずれかであり、
                    // ここでは判別できない。呼び出し元の Incomplete / 上限超過
                    // 判定に委ねるため「見つからなかった」として返す。
                    return Ok(None);
                }
                return match input.get(next) {
                    Some(b'\n') => Ok(Some(next)),
                    _ => Err(FrameError::Malformed("bare CR in request line")),
                };
            }
            b'\n' => {
                // \r 直後の \n は上の分岐で既に消費されているため、ここへ
                // 到達する \n は必ず bare LF（直前が \r ではない）。
                return Err(FrameError::Malformed("bare LF in request line"));
            }
            _ => {}
        }
        i = i
            .checked_add(1)
            .ok_or(FrameError::Malformed("crlf scan index overflow"))?;
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::error_format::ErrorClass;

    fn assert_protocol_violation(err: &FrameError) {
        assert!(matches!(err, FrameError::Malformed(_)));
        assert_eq!(err.sqlstate(), Some("08P01"));
        assert_eq!(err.error_class(), Some(ErrorClass::ProtocolViolation));
    }

    #[test]
    fn accepts_minimal_post_request_line() {
        let input = b"POST /v1/query HTTP/1.1\r\nHost: x\r\n\r\n";
        match parse_request_line(input).expect("parse should succeed") {
            RequestLineParse::Complete { line, consumed } => {
                assert_eq!(line.method, Method::Post);
                assert_eq!(line.target, "/v1/query");
                assert_eq!(line.version, Version::Http11);
                assert_eq!(&input[consumed..], b"Host: x\r\n\r\n");
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn accepts_request_line_with_no_trailing_headers() {
        let input = b"POST /v1/session/close HTTP/1.1\r\n";
        match parse_request_line(input).expect("parse should succeed") {
            RequestLineParse::Complete { line, consumed } => {
                assert_eq!(line.target, "/v1/session/close");
                assert_eq!(consumed, input.len());
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn accepts_request_line_at_exact_limit() {
        // ターゲットを `/` + `a` 連続で埋め、CRLF を含む行全体をちょうど
        // MAX_REQUEST_LINE_LEN バイトにする（fail-closed 側の境界＝受理側）。
        let prefix = b"POST /";
        let suffix = b" HTTP/1.1\r\n";
        let filler_len = MAX_REQUEST_LINE_LEN - prefix.len() - suffix.len();
        let mut input = Vec::with_capacity(MAX_REQUEST_LINE_LEN);
        input.extend_from_slice(prefix);
        input.extend(std::iter::repeat_n(b'a', filler_len));
        input.extend_from_slice(suffix);
        assert_eq!(input.len(), MAX_REQUEST_LINE_LEN);

        match parse_request_line(&input).expect("parse should succeed") {
            RequestLineParse::Complete { consumed, .. } => {
                assert_eq!(consumed, MAX_REQUEST_LINE_LEN);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn accepts_target_with_query_string() {
        // クエリ文字列の可否・パスそのものの妥当性はルータ層（Issue #758）の
        // 責務であり、本層は可視 ASCII かどうかのみを見る。
        let input = b"POST /v1/query?x=1 HTTP/1.1\r\n";
        match parse_request_line(input).expect("parse should succeed") {
            RequestLineParse::Complete { line, .. } => {
                assert_eq!(line.target, "/v1/query?x=1");
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn rejects_non_post_method() {
        let err = parse_request_line(b"GET /v1/query HTTP/1.1\r\n").unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn accepts_get_only_for_exact_openapi_target() {
        let input = b"GET /v1/openapi.json HTTP/1.1\r\n";
        match parse_request_line(input).expect("parse should succeed") {
            RequestLineParse::Complete { line, .. } => {
                assert_eq!(line.method, Method::Get);
                assert_eq!(line.target, OPENAPI_TARGET);
            }
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    #[test]
    fn rejects_get_and_other_methods_for_non_openapi_targets() {
        for raw in [
            &b"GET / HTTP/1.1\r\n"[..],
            b"GET /v1/openapi.json?x=1 HTTP/1.1\r\n",
            b"GET /v1/openapi.json/ HTTP/1.1\r\n",
            b"GET /V1/OPENAPI.JSON HTTP/1.1\r\n",
            b"get /v1/openapi.json HTTP/1.1\r\n",
            b"HEAD /v1/openapi.json HTTP/1.1\r\n",
            b"PUT /v1/openapi.json HTTP/1.1\r\n",
        ] {
            let err = parse_request_line(raw).unwrap_err();
            assert_protocol_violation(&err);
        }
    }

    #[test]
    fn rejects_lowercase_method() {
        let err = parse_request_line(b"post /v1/query HTTP/1.1\r\n").unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_http_1_0() {
        let err = parse_request_line(b"POST /v1/query HTTP/1.0\r\n").unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_http_2() {
        let err = parse_request_line(b"POST /v1/query HTTP/2\r\n").unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_bare_lf() {
        let err = parse_request_line(b"POST /v1/query HTTP/1.1\n").unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_bare_cr() {
        let err = parse_request_line(b"POST /v1/query\rHTTP/1.1\r\n").unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_request_line_over_limit_with_crlf() {
        let prefix = b"POST /";
        let suffix = b" HTTP/1.1\r\n";
        // ちょうど 1 バイト超過（4097 バイト）させる。
        let filler_len = MAX_REQUEST_LINE_LEN - prefix.len() - suffix.len() + 1;
        let mut input = Vec::with_capacity(MAX_REQUEST_LINE_LEN + 1);
        input.extend_from_slice(prefix);
        input.extend(std::iter::repeat_n(b'a', filler_len));
        input.extend_from_slice(suffix);
        assert_eq!(input.len(), MAX_REQUEST_LINE_LEN + 1);

        let err = parse_request_line(&input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_input_without_crlf_at_or_over_limit() {
        let input = vec![b'a'; MAX_REQUEST_LINE_LEN];
        let err = parse_request_line(&input).unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn incomplete_when_trailing_cr_is_at_window_end_under_limit() {
        // 上限未満の入力が `\r` で途切れている（`\n` がまだ届いていない）
        // ケース。find_crlf_within の「\r が窓の末尾にある」分岐（上限超過
        // 側は rejects_request_line_over_limit_with_crlf が経由）を、
        // 上限未満側から通す。
        let input = b"POST /x HTTP/1.1\r";
        assert!(input.len() < MAX_REQUEST_LINE_LEN);
        assert_eq!(
            parse_request_line(input).expect("should not error while incomplete"),
            RequestLineParse::Incomplete
        );
    }

    #[test]
    fn rejects_too_few_tokens() {
        let err = parse_request_line(b"POST /v1/query\r\n").unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_too_many_tokens() {
        let err = parse_request_line(b"POST /v1/query HTTP/1.1 extra\r\n").unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_consecutive_spaces() {
        let err = parse_request_line(b"POST  /v1/query HTTP/1.1\r\n").unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_tab_separated_tokens() {
        let err = parse_request_line(b"POST\t/v1/query\tHTTP/1.1\r\n").unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_absolute_form_target() {
        let err = parse_request_line(b"POST http://h/v1/query HTTP/1.1\r\n").unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_asterisk_form_target() {
        let err = parse_request_line(b"POST * HTTP/1.1\r\n").unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_control_byte_in_target() {
        let err = parse_request_line(b"POST /v1/\x01query HTTP/1.1\r\n").unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_non_ascii_byte_in_target() {
        let err = parse_request_line(b"POST /v1/\xc3query HTTP/1.1\r\n").unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn rejects_leading_blank_line() {
        let err = parse_request_line(b"\r\nPOST /v1/query HTTP/1.1\r\n").unwrap_err();
        assert_protocol_violation(&err);
    }

    #[test]
    fn incomplete_on_partial_request_line() {
        assert_eq!(
            parse_request_line(b"POST /v1").unwrap(),
            RequestLineParse::Incomplete
        );
    }

    #[test]
    fn incomplete_on_empty_input() {
        assert_eq!(
            parse_request_line(b"").unwrap(),
            RequestLineParse::Incomplete
        );
    }

    #[test]
    fn find_crlf_within_truncates_scan_at_max_len() {
        // 上限を超える入力でも max_len を超えて読み進めない。
        let mut input = vec![b'a'; 10];
        input.extend_from_slice(b"\r\n");
        assert_eq!(find_crlf_within(&input, 5).unwrap(), None);
    }

    #[test]
    fn find_crlf_within_detects_bare_lf() {
        let err = find_crlf_within(b"abc\ndef", 4096).unwrap_err();
        assert!(matches!(err, FrameError::Malformed(_)));
    }

    #[test]
    fn find_crlf_within_detects_bare_cr() {
        let err = find_crlf_within(b"abc\rdef", 4096).unwrap_err();
        assert!(matches!(err, FrameError::Malformed(_)));
    }
}
