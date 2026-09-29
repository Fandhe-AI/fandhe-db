//! PostgreSQL wire プロトコル v3 のハンドシェイク・簡易クエリ最小応答を担う。
//!
//! `main.rs` の接続受け付けループ（`TcpListener` + thread-per-connection）から
//! 1 接続 1 スレッドで [`handle_connection_bounded`] が呼ばれる。認証の実照合は
//! `auth::verify` に委譲し、本モジュールはメッセージのフレーミング（読み書き・
//! 長さ検証）と応答メッセージの組み立てに専念する。
//!
//! 受信データ（SSLRequest/StartupMessage/PasswordMessage/簡易クエリ）はすべて
//! untrusted 入力として扱い、`unwrap`/`expect`/添字アクセスを用いず `get()`・
//! `checked_*` で処理する（`.claude/rules/coding-rust.md` P0）。
//! メッセージ長の検証・fail-closed なエラー分類は `crate::framing` に集約する。
//!
//! 対応: TASK-67（ポインタ: `docs/spec/05-tasks.md`。対象ビヘイビア WIRE-1, WIRE-2, WIRE-3）、
//! TASK-68（正式なフレーミング上限体系。対象ビヘイビア WIRE-4, WIRE-10）。

use std::io;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::wire_stream::WireStream;
use engine::error_format::ErrorClass;

use crate::auth::{self, base64_std, scram, AuthMethod, UserStore};
use crate::framing::{self, FrameError};
use crate::tls_opt::TlsMode;

/// StartupMessage が名乗るべきプロトコルバージョン（3.0 = major 3, minor 0）。
const PROTOCOL_VERSION_3_0: i32 = 0x0003_0000;

const SSL_REQUEST_CODE: i32 = 80_877_103;
const GSSENC_REQUEST_CODE: i32 = 80_877_104;
const CANCEL_REQUEST_CODE: i32 = 80_877_102;

#[derive(Debug)]
enum HandshakeError {
    Io(io::Error),
    /// fail-closed に倒すプロトコル違反（詳細は理由をログ用途にのみ保持し、
    /// クライアントへは SQLSTATE 経由の定型メッセージのみ返す）。フレーミング外の
    /// 構文違反（パラメータ解析・型ごとの形状検証等）に用いる。
    Protocol(&'static str),
    /// `framing` モジュールが検出したフレーミング違反（WIRE-4/WIRE-10）。
    Frame(FrameError),
}

impl From<io::Error> for HandshakeError {
    fn from(e: io::Error) -> Self {
        HandshakeError::Io(e)
    }
}

impl From<FrameError> for HandshakeError {
    fn from(e: FrameError) -> Self {
        match e {
            // I/O 異常（タイムアウト等）はフレーミング分類ではなく通常の I/O エラー
            // として扱う（呼び出し元の `handle_connection_bounded` が `Err` を返す経路と
            // 揃える）。
            FrameError::Io(io_err) => HandshakeError::Io(io_err),
            other => HandshakeError::Frame(other),
        }
    }
}

/// [`write_error_response`] が [`crate::error_response::encode`]（ErrorResponse
/// バイト列組み立ての唯一の実体。TASK-153・ERR-1・codex-review P1 指摘対応・
/// PR #258）に委譲する際のエラー写像。フレーム長超過等のエンコード失敗は
/// fail-closed に倒し `Protocol` 分類として扱う（本モジュールの `class`/`message`
/// 引数は固定英語文言のみを渡す契約のため実運用では発生しない想定）。
impl From<crate::result_encoder::EncodeError> for HandshakeError {
    fn from(_: crate::result_encoder::EncodeError) -> Self {
        HandshakeError::Protocol("failed to encode error response")
    }
}

/// `handle_connection_bounded` の戻り値型（`io::Result<()>`）へ `?` で直接畳み込めるようにする
/// 変換。`Protocol`/`Frame` 側は `io::ErrorKind::InvalidData` に写像し、呼び出し元
/// （`main.rs`）にはログ用途の文字列のみを残す（詳細な違反理由をクライアントへ
/// 返すことはない）。
impl From<HandshakeError> for io::Error {
    fn from(e: HandshakeError) -> Self {
        match e {
            HandshakeError::Io(io_err) => io_err,
            HandshakeError::Protocol(msg) => io::Error::new(io::ErrorKind::InvalidData, msg),
            HandshakeError::Frame(frame_err) => {
                io::Error::new(io::ErrorKind::InvalidData, frame_err.to_string())
            }
        }
    }
}

type Result<T> = std::result::Result<T, HandshakeError>;

// ---------------------------------------------------------------------------
// 低レベル読み書きプリミティブ
// ---------------------------------------------------------------------------

fn write_all<S: WireStream>(stream: &mut S, data: &[u8]) -> Result<()> {
    stream.write_all(data)?;
    Ok(())
}

/// null 終端 C 文字列を `body[*pos..]` から読み取り、UTF-8 として検証する
/// （不正 UTF-8・未終端はいずれも `Err`。添字アクセスは行わず `get()` のみ使う）。
fn read_c_string<'a>(body: &'a [u8], pos: &mut usize) -> Result<&'a str> {
    let start = *pos;
    let rest = body
        .get(start..)
        .ok_or(HandshakeError::Protocol("truncated frame"))?;
    let nul_offset = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or(HandshakeError::Protocol("unterminated C string"))?;
    let s = std::str::from_utf8(&rest[..nul_offset])
        .map_err(|_| HandshakeError::Protocol("invalid UTF-8 in C string"))?;
    *pos = start + nul_offset + 1;
    Ok(s)
}

// ---------------------------------------------------------------------------
// 応答メッセージ組み立て
// ---------------------------------------------------------------------------

fn write_ssl_no_response(stream: &mut TcpStream) -> Result<()> {
    write_all(stream, b"N")
}

fn write_authentication_cleartext_password<S: WireStream>(stream: &mut S) -> Result<()> {
    // 'R' + length(4) + AuthenticationCleartextPassword コード(4) = 3
    let mut msg = Vec::with_capacity(9);
    msg.push(b'R');
    msg.extend_from_slice(&8i32.to_be_bytes());
    msg.extend_from_slice(&3i32.to_be_bytes());
    write_all(stream, &msg)
}

fn write_authentication_ok<S: WireStream>(stream: &mut S) -> Result<()> {
    let mut msg = Vec::with_capacity(9);
    msg.push(b'R');
    msg.extend_from_slice(&8i32.to_be_bytes());
    msg.extend_from_slice(&0i32.to_be_bytes());
    write_all(stream, &msg)
}

/// SASL メッセージ 1 個あたりの上限（Issue #940・WIRE-18・TASK-222）。
/// 妥当な SCRAM メッセージ（機構名・nonce・salt・proof を含む）は数百バイト
/// 以内に収まるため、余裕を持った固定上限とする。超過は `54000`
/// （[`FrameError::TooLarge`] と同じ分類）へ写像する。
const MAX_SASL_MESSAGE_LEN: usize = 2048;

/// `AuthenticationSASL`（'R'/10）: `offer_plus` が `false` の場合は
/// [`scram::MECHANISM_NAME`] の 1 つのみ（TLS 未接続、または TLS 接続でも
/// チャネルバインディング非提供・提示無効化設定の場合。既存挙動とビット
/// 同一）。`true` の場合は [`scram::MECHANISM_NAME_PLUS`] を先頭に加えた
/// 2 機構を提示する（TLS 接続かつ `tls-server-end-point` を算出できた場合。
/// Issue #970・WIRE-18）。
fn write_authentication_sasl<S: WireStream>(stream: &mut S, offer_plus: bool) -> Result<()> {
    let mut body = Vec::new();
    if offer_plus {
        body.extend_from_slice(scram::MECHANISM_NAME_PLUS.as_bytes());
        body.push(0);
    }
    body.extend_from_slice(scram::MECHANISM_NAME.as_bytes());
    body.push(0);
    body.push(0); // 機構リストの終端（空文字列）。
    let total_len = (8 + body.len()) as i32;
    let mut msg = Vec::with_capacity(5 + body.len());
    msg.push(b'R');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&10i32.to_be_bytes());
    msg.extend_from_slice(&body);
    write_all(stream, &msg)
}

/// `AuthenticationSASLContinue`（'R'/11）: server-first-message を運ぶ。
fn write_authentication_sasl_continue<S: WireStream>(stream: &mut S, data: &[u8]) -> Result<()> {
    let total_len = (8 + data.len()) as i32;
    let mut msg = Vec::with_capacity(5 + data.len());
    msg.push(b'R');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&11i32.to_be_bytes());
    msg.extend_from_slice(data);
    write_all(stream, &msg)
}

/// `AuthenticationSASLFinal`（'R'/12）: server-final-message（`v=...`）を運ぶ。
/// proof の検証に成功した場合にのみ送出し、これを送った直後は必ず
/// `AuthenticationOk` 以降の既存シーケンスへ進む（`v=` を送ってからエラーに
/// する経路は作らない）。
fn write_authentication_sasl_final<S: WireStream>(stream: &mut S, data: &[u8]) -> Result<()> {
    let total_len = (8 + data.len()) as i32;
    let mut msg = Vec::with_capacity(5 + data.len());
    msg.push(b'R');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&12i32.to_be_bytes());
    msg.extend_from_slice(data);
    write_all(stream, &msg)
}

/// `SASLInitialResponse`（型 'p'）を読む。本文は「機構名（C 文字列）」
/// 「int32 の長さ（`-1` は不可）」「client-first-message 本体」の順。
/// PasswordMessage と型バイトは同じだが本文形状が異なるため
/// `read_password_message` は流用しない。
///
/// `offer_plus` が `true`（[`write_authentication_sasl`] が PLUS を提示した
/// 接続）の場合のみ [`scram::MECHANISM_NAME_PLUS`] も受理し、戻り値の
/// `bool` で「クライアントが PLUS を選んだか」を返す（Issue #970）。
/// `offer_plus` が `false` の場合の受理条件・エラーはビット単位で不変。
fn read_sasl_initial_response<S: WireStream>(
    stream: &mut S,
    offer_plus: bool,
) -> Result<(bool, Vec<u8>)> {
    let type_byte = match framing::read_typed_frame_header(stream)? {
        Some(b) => b,
        None => return Err(HandshakeError::Protocol("expected SASLInitialResponse")),
    };
    if type_byte != b'p' {
        return Err(HandshakeError::Protocol("expected SASLInitialResponse"));
    }
    let body = framing::read_length_prefixed_body(
        stream,
        framing::MIN_TYPED_MESSAGE_LEN,
        MAX_SASL_MESSAGE_LEN,
    )?;

    let mut pos = 0usize;
    let mechanism = read_c_string(&body, &mut pos)?;
    let selected_plus = if offer_plus && mechanism == scram::MECHANISM_NAME_PLUS {
        true
    } else if mechanism == scram::MECHANISM_NAME {
        false
    } else {
        return Err(HandshakeError::Protocol("unsupported SASL mechanism"));
    };
    let len_bytes: [u8; 4] = body
        .get(pos..pos + 4)
        .and_then(|s| s.try_into().ok())
        .ok_or(HandshakeError::Protocol("truncated SASL message length"))?;
    let declared_len = i32::from_be_bytes(len_bytes);
    pos += 4;
    if declared_len < 0 {
        return Err(HandshakeError::Protocol("negative SASL message length"));
    }
    let rest = body
        .get(pos..)
        .ok_or(HandshakeError::Protocol("truncated SASL message body"))?;
    if rest.len() != declared_len as usize {
        return Err(HandshakeError::Protocol(
            "SASL message length does not match declared value",
        ));
    }
    Ok((selected_plus, rest.to_vec()))
}

/// `SASLResponse`（型 'p'）を読む。本文は raw bytes（PasswordMessage と異なり
/// NUL 終端ではない）。
fn read_sasl_response<S: WireStream>(stream: &mut S) -> Result<Vec<u8>> {
    let type_byte = match framing::read_typed_frame_header(stream)? {
        Some(b) => b,
        None => return Err(HandshakeError::Protocol("expected SASLResponse")),
    };
    if type_byte != b'p' {
        return Err(HandshakeError::Protocol("expected SASLResponse"));
    }
    framing::read_length_prefixed_body(stream, framing::MIN_TYPED_MESSAGE_LEN, MAX_SASL_MESSAGE_LEN)
        .map_err(HandshakeError::from)
}

/// BackendKeyData（'K'）: pid・secret key を通知する。CancelRequest 経路は本タスクの
/// スコープ外だが、クライアント実装（psql 等）が本メッセージの到達を前提に
/// StartupMessage 後続シーケンスを進めるため送出する。
fn write_backend_key_data<S: WireStream>(stream: &mut S, pid: i32, secret: i32) -> Result<()> {
    let mut msg = Vec::with_capacity(13);
    msg.push(b'K');
    msg.extend_from_slice(&12i32.to_be_bytes());
    msg.extend_from_slice(&pid.to_be_bytes());
    msg.extend_from_slice(&secret.to_be_bytes());
    write_all(stream, &msg)
}

fn write_parameter_status<S: WireStream>(stream: &mut S, name: &str, value: &str) -> Result<()> {
    let mut body = Vec::with_capacity(name.len() + value.len() + 2);
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body.extend_from_slice(value.as_bytes());
    body.push(0);
    let total_len = (4 + body.len()) as i32;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(b'S');
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    write_all(stream, &msg)
}

/// `ReadyForQuery`（'Z'）を単独送出する（ハンドシェイク完了直後など、応答
/// バッファ組み立てを経由しない経路向け）。バイトレイアウトの実体は
/// `crate::result_encoder::encode_ready_for_query`（Issue #481）に一元化した
/// ―― 以前は本関数がレイアウトを個別に持っており、`crate::response_buffer::
/// ResponseBuffer` へ他フレームと同じ形で積める関数が無かった。`status` は
/// 明示トランザクション（SQL-31・TASK-221・WIRE-19）の状態バイトへそのまま
/// 写像する。呼び出し元に `SessionTransaction` が無い箇所（ハンドシェイク
/// 直後）は `TransactionStatus::Idle` を渡す。
fn write_ready_for_query<S: WireStream>(
    stream: &mut S,
    status: engine::sql::transaction::TransactionStatus,
) -> Result<()> {
    write_all(
        stream,
        &crate::result_encoder::encode_ready_for_query(status),
    )
}

/// ErrorResponse（'E'）。SQLSTATE と英語メッセージのみを含む最小フィールド構成
/// （severity 'S'・code 'C'・message 'M' のみ。他テナント・存在情報は含めない）。
/// `protocol_dispatch::reject_and_close` から呼ばれる `io::Result` 版のラッパー。
/// `HandshakeError`／`handshake::Result` は本モジュール限定の型のため、モジュール
/// 境界をまたいで直接公開せず、戻り値を `io::Result` へ写像したこの関数のみを
/// `pub(crate)` にする（`HandshakeError` 自体は private のまま維持する）。
pub(crate) fn write_error_response_io<S: WireStream>(
    stream: &mut S,
    class: ErrorClass,
    message: &str,
) -> io::Result<()> {
    write_error_response(stream, class, message).map_err(io::Error::from)
}

/// `write_ready_for_query` の `io::Result` 版ラッパー。[`crate::simple_query`] は
/// 本モジュール限定の `handshake::Result` を扱えないため、`ReadyForQuery` を
/// 送出する唯一の経路としてこの関数を `pub(crate)` にする。
pub(crate) fn write_ready_for_query_io<S: WireStream>(
    stream: &mut S,
    status: engine::sql::transaction::TransactionStatus,
) -> io::Result<()> {
    write_ready_for_query(stream, status).map_err(io::Error::from)
}

/// バイト列組み立ては [`crate::error_response::encode`] に委譲する（`ErrorClass`
/// から severity・SQLSTATE を一元的に決定し、通常応答経路がこの横断写像を必ず
/// 経由するようにする。TASK-153・ERR-1・codex-review P1 指摘対応・PR #258。
/// 以前は `crate::result_encoder::encode_error_response`〔`&str` の SQLSTATE を
/// そのまま受け取り severity は `ERROR` 固定〕を経由しており、`ErrorClass::
/// ConnectionLimitExceeded` のような `FATAL` 契約の分類でも `ERROR` に丸められる
/// 不整合があった）。
fn write_error_response<S: WireStream>(
    stream: &mut S,
    class: ErrorClass,
    message: &str,
) -> Result<()> {
    let msg = crate::error_response::encode(class, message)?;
    write_all(stream, &msg)
}

// ---------------------------------------------------------------------------
// StartupMessage 受理・認証・最小クエリループ
// ---------------------------------------------------------------------------

/// SSLRequest/GSSENCRequest には 'N'（非対応）で応答して次のパケットを待ち、
/// CancelRequest は即座に接続を閉じ、それ以外はプロトコルバージョン 3.0 の
/// StartupMessage として `user` パラメータを取り出す（WIRE-1）。
///
/// SSLRequest/GSSENCRequest への応答ループは無制限に繰り返させない
/// （各コードにつき応答は高々 1 回。2 回目以降は fail-closed で拒否し、
/// 無限ループでスレッドを占有させる経路を作らない）。
fn negotiate_startup(stream: &mut TcpStream) -> Result<String> {
    let mut ssl_seen = false;
    let mut gssenc_seen = false;
    loop {
        let body = framing::read_startup_frame(stream)?;
        let code_bytes: [u8; 4] = body
            .get(0..4)
            .and_then(|s| s.try_into().ok())
            .ok_or(HandshakeError::Protocol("truncated startup code"))?;
        let code = i32::from_be_bytes(code_bytes);

        match code {
            SSL_REQUEST_CODE if !ssl_seen => {
                ssl_seen = true;
                write_ssl_no_response(stream)?;
                continue;
            }
            GSSENC_REQUEST_CODE if !gssenc_seen => {
                gssenc_seen = true;
                write_ssl_no_response(stream)?;
                continue;
            }
            SSL_REQUEST_CODE | GSSENC_REQUEST_CODE => {
                return Err(HandshakeError::Protocol("repeated SSL/GSSENC negotiation"));
            }
            CANCEL_REQUEST_CODE => {
                return Err(HandshakeError::Protocol(
                    "cancel request (not supported on this path)",
                ));
            }
            PROTOCOL_VERSION_3_0 => {
                return parse_startup_params(&body[4..]);
            }
            _ => {
                return Err(HandshakeError::Protocol("unsupported protocol version"));
            }
        }
    }
}

/// StartupMessage のパラメータ列（null 終端キー・値ペアの繰り返し、空文字列で終端）
/// から `user` を取り出す。`user` 以外のパラメータはテナント決定に用いない
/// （ポインタ: TASK-67・WIRE-2）。
///
/// 終端（空キー）を読んだ時点で `params_body` を使い切っていること（終端後の
/// 残余バイトがないこと）・`user` キーが複数回出現しないことを検証する
/// （review 指摘: 残余バイトの無視・重複キーの後勝ち上書きは、フレーミングの
/// 曖昧さやテナント決定への不正な入力混入余地を生むため fail-closed で拒否する）。
fn parse_startup_params(params_body: &[u8]) -> Result<String> {
    let mut pos = 0usize;
    let mut user: Option<String> = None;
    loop {
        let key = read_c_string(params_body, &mut pos)?;
        if key.is_empty() {
            if pos != params_body.len() {
                return Err(HandshakeError::Protocol(
                    "trailing data after startup params",
                ));
            }
            break;
        }
        let value = read_c_string(params_body, &mut pos)?;
        if key == "user" {
            if user.is_some() {
                return Err(HandshakeError::Protocol("duplicate user parameter"));
            }
            user = Some(value.to_string());
        }
        // その他のパラメータは意図的に読み捨てる（ポインタ: WIRE-2）。
    }
    user.ok_or(HandshakeError::Protocol("missing required parameter: user"))
}

/// PasswordMessage（'p'）を読み、末尾 null を除いた生パスワードバイト列を返す。
///
/// body は null 終端 C 文字列 1 個であること（末尾以外に NUL を含む不正フレームは
/// 拒否する。review 指摘: `password\0suffix\0` のような多重 NUL フレームを
/// Argon2id 照合へそのまま渡すと、フレーミングの曖昧さがパスワード照合の意味論に
/// 混入するため fail-closed で拒否する）。
fn read_password_message<S: WireStream>(stream: &mut S) -> Result<Vec<u8>> {
    let type_byte = match framing::read_typed_frame_header(stream)? {
        Some(b) => b,
        None => return Err(HandshakeError::Protocol("expected PasswordMessage")),
    };
    if type_byte != b'p' {
        return Err(HandshakeError::Protocol("expected PasswordMessage"));
    }
    let body = framing::read_length_prefixed_body(
        stream,
        framing::MIN_TYPED_MESSAGE_LEN,
        framing::MAX_MESSAGE_LEN,
    )?;
    let end = body
        .len()
        .checked_sub(1)
        .ok_or(HandshakeError::Protocol("empty password body"))?;
    if body.get(end) != Some(&0) {
        return Err(HandshakeError::Protocol(
            "password message not null-terminated",
        ));
    }
    let password = body
        .get(..end)
        .ok_or(HandshakeError::Protocol("truncated password body"))?;
    if password.contains(&0) {
        return Err(HandshakeError::Protocol(
            "password message contains embedded NUL",
        ));
    }
    Ok(password.to_vec())
}

/// 認証成功後の最小メッセージループ。`engine` が `Some` の場合、簡易クエリ
/// （'Q'）は UTF-8 検証後 [`crate::simple_query::execute_and_respond`] へ委譲し、
/// engine の SQL 表層で実行した結果（または SQL エラー）を wire メッセージへ
/// 整形して返す（TASK-73・WIRE-1）。`engine` が `None`（`handle_connection_bounded`
/// 経由の後方互換パス）の場合は従来どおり未実装エラー（SQLSTATE `0A000`）を返す。
/// いずれの経路も Terminate（'X'）で正常終了する。それ以外の型（拡張クエリ
/// プロトコル等）は `protocol_dispatch` へ委譲し、fail-closed でエラー応答後に
/// 接続を切断する（TASK-71・WIRE-8 で正式化済み）。
///
/// `Q`・`X` いずれも構造検証を行う（review 指摘: 構造を検証せず読み捨てるだけでは
/// フレーミングの曖昧さが残る）。`Q` は単一の NUL 終端文字列（空 body・終端 NUL
/// なし・末尾以外の埋め込み NUL はいずれも拒否）に加え、本文が UTF-8 として
/// 妥当であることを要求する（`simple_query`／engine の SQL 表層はいずれも `&str`
/// のみを受け取る契約のため、境界はここで確定させる。不正 UTF-8 は `08P01` で
/// fail-closed に拒否する）。`X` は length=4・body が厳密に空であることを要求し、
/// 違反は protocol violation として fail-closed で扱う。
///
/// `ctx` は認証成功時に導出された `engine::policy::PolicyContext`（テナント境界・
/// 可視性判定の唯一の入力経路）、`session` は接続単位の
/// `engine::sql::mode::SessionState`（取得モード・宣言的 UDF レジストリ）で、
/// いずれも本ループの全クエリを通じて 1 個の値を使い回す（`EngineCore` 自体は
/// セッション状態を保持しない設計。`sql::mode` モジュールドキュメント参照）。
fn post_auth_loop<'e, S: WireStream>(
    stream: &mut S,
    ctx: &engine::policy::PolicyContext,
    engine: Option<&'e engine::core::EngineCore>,
    session: &mut engine::sql::mode::SessionState,
    txn: &mut Option<engine::sql::transaction::SessionTransaction<'e>>,
    extended: &mut crate::extended_query::ExtendedQueryState,
) -> Result<()> {
    // 接続全体に設定済みの読み取りタイムアウト（WIRE-5。`server::accept_loop_*`
    // が受理直後に設定する）。明示トランザクション中に一時的に切り詰めた後は
    // 必ずこの値へ戻す（[`read_next_frame_header`] 参照）。
    let base_timeout = stream.read_timeout()?;
    let mut applied_timeout = base_timeout;
    loop {
        let type_byte =
            match read_next_frame_header(stream, txn, base_timeout, &mut applied_timeout)? {
                Some(b) => b,
                None => return Ok(()),
            };

        // SQL-31・TASK-221（PR #1041 レビュー指摘）: 明示トランザクションの持続時間
        // 上限を、要求の種類（Sync・Flush 等 SQL を伴わない要求を含む）を問わず
        // 受信のたびに検査する。期限を過ぎていれば共有書き込みトランザクションを
        // abort してライタを解放し `Failed` へ遷移させる（最初の文／`COMMIT` には
        // `54000` が返る）。無通信の間の期限到達は [`read_next_frame_header`] が
        // 受信待ちを期限で打ち切って同じ遷移を行う。
        if let Some(txn) = txn.as_mut() {
            txn.release_if_expired();
            // 拡張クエリプロトコルのエラー後（`ignore_till_sync`）は、ループ末尾の
            // 同じ検査に加えてここでも `Failed` へ遷移させる（ループ末尾へ到達しない
            // `continue` 経路が将来追加されても取りこぼさないための多層防御）。
            if extended.ignore_till_sync {
                txn.fail();
            }
        }

        // SQL-31・TASK-221（PR #1041 レビュー指摘）: `Active` の間は、型バイトに
        // 続く長さ・本文の受信全体にも持続時間上限（と接続の読み取りタイムアウト）
        // までの期限を課す。本文を少しずつ送り続けて上限後もライタを保持させる
        // 経路を塞ぐためで、期限を過ぎたら応答を送らずに接続を閉じ（I/O エラー）、
        // `SessionTransaction` の drop でライタを解放する。読み取り 1 回あたりの
        // 待機は短い読み取りタイムアウトで区切り、期限の確認を
        // `framing::FrameDeadlineGuard` が行う。次の反復の
        // [`read_next_frame_header`] が読み取りタイムアウトを戻す。
        let _frame_deadline = match txn.as_ref().and_then(|t| t.remaining_duration()) {
            Some(remaining) => {
                let budget = base_timeout.map_or(remaining, |b| remaining.min(b));
                let poll = budget.min(FRAME_DEADLINE_POLL).max(MIN_BOUNDED_READ_WAIT);
                if applied_timeout != Some(poll) {
                    stream.set_read_timeout(Some(poll))?;
                    applied_timeout = Some(poll);
                }
                Instant::now()
                    .checked_add(budget)
                    .map(framing::FrameDeadlineGuard::set)
            }
            None => None,
        };

        // WIRE-11 確定化（Issue #934）: 拡張クエリプロトコルのエラー後は
        // Sync（'S'）まで後続メッセージを破棄する「同期回復」モードに入る
        // （`extended_query` モジュールドキュメント「エラー後の同期回復」節）。
        // 'S' はここでは処理せず下の通常分岐へフォールスルーさせてフラグを
        // 解除する。'X' は通常の分岐（559 行目付近）と同じく length=4・body
        // 厳密に空であることを検証してから終了する（PR #1013 レビュー指摘・
        // codex P1: 検証を素通りする経路があると、長さフィールド欠落・不正長・
        // 余剰 body を持つ malformed Terminate が「エラー後」という条件だけで
        // 正規の Terminate として受理されてしまい、フレーミング検証契約が
        // ignore_till_sync モードでだけ回避可能になる）。長さ検証自体が失敗
        // した場合は通常の `X` 分岐と同じく `?` で fail-closed に伝播する。
        // COPY・FunctionCall・未知の型バイトは破棄対象にせず fail-closed に
        // `reject_and_close`（既存の WIRE-8 契約のまま）。それ以外
        // （'Q'/'P'/'D'/'B'/'E'/'C'/'H'）は長さフィールドのみ検証して本文を
        // 読み捨て、応答を一切送らない。
        if extended.ignore_till_sync && type_byte != b'S' {
            if type_byte == b'X' {
                let _body = framing::read_length_prefixed_body(stream, 4, 4)?;
                return Ok(());
            }
            let kind = crate::protocol_dispatch::classify(type_byte);
            match kind {
                crate::protocol_dispatch::FrontendMessageKind::UnsupportedFeature(_)
                | crate::protocol_dispatch::FrontendMessageKind::Unknown(_) => {
                    framing::validate_typed_message_length_prefix(
                        stream,
                        framing::MIN_TYPED_MESSAGE_LEN,
                        framing::MAX_MESSAGE_LEN,
                    )?;
                    crate::protocol_dispatch::reject_and_close(
                        stream,
                        kind,
                        write_error_response_io,
                    )?;
                    return Ok(());
                }
                _ => {
                    let total_len = framing::validate_typed_message_length_prefix(
                        stream,
                        framing::MIN_TYPED_MESSAGE_LEN,
                        framing::MAX_MESSAGE_LEN,
                    )?;
                    let body_len = total_len
                        .checked_sub(4)
                        .ok_or(FrameError::Malformed("message length below header size"))?;
                    framing::discard_body(stream, body_len)?;
                    continue;
                }
            }
        }

        match type_byte {
            b'Q' => {
                // 最小 5（length 4 バイト + 終端 NUL 1 バイト）。空 body は拒否する。
                let body = framing::read_length_prefixed_body(stream, 5, framing::MAX_MESSAGE_LEN)?;
                let end = body
                    .len()
                    .checked_sub(1)
                    .ok_or(HandshakeError::Protocol("empty query body"))?;
                if body.get(end) != Some(&0) {
                    return Err(HandshakeError::Protocol(
                        "query message not null-terminated",
                    ));
                }
                let text = body
                    .get(..end)
                    .ok_or(HandshakeError::Protocol("truncated query body"))?;
                if text.contains(&0) {
                    return Err(HandshakeError::Protocol(
                        "query message contains embedded NUL",
                    ));
                }
                let text = std::str::from_utf8(text)
                    .map_err(|_| HandshakeError::Protocol("query text is not valid UTF-8"))?;

                // PostgreSQL は simple Query の処理を無名 statement／無名 portal
                // への暗黙の Parse／Bind／Execute と同一視し、その処理時に両方を
                // 破棄する。拡張クエリプロトコルで確立した無名 portal を残した
                // まま simple Query を発行すると、後続の `Execute("")` が
                // simple Query 実行前の古い portal を誤って再開してしまう
                // （Cursor Bugbot Medium 指摘・PR #1013）。名前付き
                // statement／portal は維持する（PostgreSQL と同じ挙動）。
                extended.discard_unnamed_for_simple_query();

                // SQL-31・TASK-221: `engine` が `Some` の間は `txn` も `Some`
                // （`post_auth_loop` 呼び出し元で対で構築する）。受信経路で
                // `expect` に頼らず、万一片方だけ `Some` の場合は engine 未接続と
                // 同じ fail-closed 分岐（`0A000`）へ倒す。
                match (engine, txn.as_mut()) {
                    (Some(engine), Some(txn)) => {
                        // Issue #1175: `COPY`（WIRE-17）を含むすべての 'Q' 本文を
                        // `execute_and_respond` へ委譲する。COPY の判定・状態検査・
                        // サブプロトコルは文単位で `simple_query::run_copy_statement`
                        // が担う（複数文メッセージの 2 文目以降の COPY を扱うため、
                        // 従来のメッセージ全文の覗き見はここから移した）。
                        // Issue #939 レビュー指摘（discussion_r4096720859）: COPY
                        // サブプロトコル中に Terminate（'X'）を受信した場合は
                        // `LoopSignal::Closed` が返る。ここで判定せず捨てると、
                        // クライアントは切断済みのつもりで応答を待たなくなる一方
                        // サーバー側は接続スロットを保持し続けてしまう
                        // （`P`／`D` 分岐と同じ判定作法）。
                        match crate::simple_query::execute_and_respond(
                            stream, engine, ctx, session, txn, text,
                        )? {
                            crate::extended_query::LoopSignal::Continue => {}
                            crate::extended_query::LoopSignal::Closed => return Ok(()),
                        }
                    }
                    _ => {
                        write_error_response(
                            stream,
                            ErrorClass::FeatureNotSupported,
                            "simple query execution is not yet implemented",
                        )?;
                        write_ready_for_query(
                            stream,
                            engine::sql::transaction::TransactionStatus::Idle,
                        )?;
                    }
                }
            }
            b'X' => {
                // Terminate は length=4（body 厳密に空）以外を fail-closed で拒否する。
                let _body = framing::read_length_prefixed_body(stream, 4, 4)?;
                return Ok(());
            }
            b'P' => {
                // WIRE-11（Issue #933）: engine が接続済みの場合のみ
                // `crate::extended_query::handle_parse` へ委譲する。`engine: None`
                // （`handle_connection_bounded` 経由の後方互換パス）では SQL 表層
                // 自体が存在しないため、従来どおり `protocol_dispatch::
                // reject_and_close`（`0A000` + 切断）へ倒す（WIRE-8 が Parse に
                // 適用していた契約をそのまま維持）。
                match (engine, txn.as_mut()) {
                    (Some(engine), Some(txn)) => {
                        match crate::extended_query::handle_parse(stream, engine, txn, extended)? {
                            crate::extended_query::LoopSignal::Continue => {}
                            crate::extended_query::LoopSignal::Closed => return Ok(()),
                        }
                    }
                    _ => {
                        // `other` 分岐（WIRE-8）と同じく、`0A000` を返す前に長さ
                        // フィールド自体を検証する（malformed frame を正規の
                        // 未対応機能扱いにしない。レビュー指摘の回帰防止）。
                        framing::validate_typed_message_length_prefix(
                            stream,
                            framing::MIN_TYPED_MESSAGE_LEN,
                            framing::MAX_MESSAGE_LEN,
                        )?;
                        let kind = crate::protocol_dispatch::classify(type_byte);
                        crate::protocol_dispatch::reject_and_close(
                            stream,
                            kind,
                            write_error_response_io,
                        )?;
                        return Ok(());
                    }
                }
            }
            b'D' => {
                // WIRE-11（Issue #933）: Describe。`b'P'` と同じ engine 有無の分岐。
                // WIRE-15・TASK-218: `txn`（`engine` と Some/None が一致する—— 上の
                // `let mut txn = engine.map(...)` 参照）も渡し、`FETCH` の Describe が
                // 開いているカーソルの列メタデータを参照できるようにする。
                match (engine, txn.as_mut()) {
                    (Some(engine), Some(txn)) => {
                        match crate::extended_query::handle_describe(
                            stream, engine, session, txn, extended,
                        )? {
                            crate::extended_query::LoopSignal::Continue => {}
                            crate::extended_query::LoopSignal::Closed => return Ok(()),
                        }
                    }
                    _ => {
                        framing::validate_typed_message_length_prefix(
                            stream,
                            framing::MIN_TYPED_MESSAGE_LEN,
                            framing::MAX_MESSAGE_LEN,
                        )?;
                        let kind = crate::protocol_dispatch::classify(type_byte);
                        crate::protocol_dispatch::reject_and_close(
                            stream,
                            kind,
                            write_error_response_io,
                        )?;
                        return Ok(());
                    }
                }
            }
            b'B' => match (engine, txn.as_mut()) {
                (Some(engine), Some(txn)) => {
                    match crate::extended_query::handle_bind(
                        stream, engine, session, txn, extended,
                    )? {
                        crate::extended_query::LoopSignal::Continue => {}
                        crate::extended_query::LoopSignal::Closed => return Ok(()),
                    }
                }
                _ => {
                    framing::validate_typed_message_length_prefix(
                        stream,
                        framing::MIN_TYPED_MESSAGE_LEN,
                        framing::MAX_MESSAGE_LEN,
                    )?;
                    let kind = crate::protocol_dispatch::classify(type_byte);
                    crate::protocol_dispatch::reject_and_close(
                        stream,
                        kind,
                        write_error_response_io,
                    )?;
                    return Ok(());
                }
            },
            // SQL-31・TASK-221（Issue #942 codex-review 指摘対応）: `engine` が
            // `Some` の間は `txn` も `Some`（`'Q'` 分岐と同じ不変条件。
            // `post_auth_loop` 呼び出し元で対で構築する）。拡張クエリプロトコル
            // 経由の Execute も簡易クエリと同一の `SessionTransaction` を共有し、
            // `BEGIN`/`COMMIT`/`ROLLBACK` を受理できるようにする。片方だけ
            // `Some` の場合は engine 未接続と同じ fail-closed 分岐へ倒す。
            b'E' => match (engine, txn.as_mut()) {
                (Some(engine), Some(txn)) => {
                    match crate::extended_query::handle_execute(
                        stream, engine, ctx, session, txn, extended,
                    )? {
                        crate::extended_query::LoopSignal::Continue => {}
                        crate::extended_query::LoopSignal::Closed => return Ok(()),
                    }
                }
                _ => {
                    framing::validate_typed_message_length_prefix(
                        stream,
                        framing::MIN_TYPED_MESSAGE_LEN,
                        framing::MAX_MESSAGE_LEN,
                    )?;
                    let kind = crate::protocol_dispatch::classify(type_byte);
                    crate::protocol_dispatch::reject_and_close(
                        stream,
                        kind,
                        write_error_response_io,
                    )?;
                    return Ok(());
                }
            },
            b'S' => match engine {
                Some(_engine) => {
                    // `engine` が `Some` の間は `txn` も `Some`（他の拡張クエリ
                    // 分岐と同じ不変条件）。万一片方だけの場合は fail-closed に
                    // `Idle` を送出する（`TransactionStatus::Idle` は既存の
                    // 固定 `'I'` 送出と同じ安全側の既定値）。
                    let txn_status = txn
                        .as_ref()
                        .map(|t| t.status())
                        .unwrap_or(engine::sql::transaction::TransactionStatus::Idle);
                    match crate::extended_query::handle_sync(stream, extended, txn_status)? {
                        crate::extended_query::LoopSignal::Continue => {}
                        crate::extended_query::LoopSignal::Closed => return Ok(()),
                    }
                }
                None => {
                    framing::validate_typed_message_length_prefix(
                        stream,
                        framing::MIN_TYPED_MESSAGE_LEN,
                        framing::MAX_MESSAGE_LEN,
                    )?;
                    let kind = crate::protocol_dispatch::classify(type_byte);
                    crate::protocol_dispatch::reject_and_close(
                        stream,
                        kind,
                        write_error_response_io,
                    )?;
                    return Ok(());
                }
            },
            b'C' => match engine {
                Some(_engine) => match crate::extended_query::handle_close(stream, extended)? {
                    crate::extended_query::LoopSignal::Continue => {}
                    crate::extended_query::LoopSignal::Closed => return Ok(()),
                },
                None => {
                    framing::validate_typed_message_length_prefix(
                        stream,
                        framing::MIN_TYPED_MESSAGE_LEN,
                        framing::MAX_MESSAGE_LEN,
                    )?;
                    let kind = crate::protocol_dispatch::classify(type_byte);
                    crate::protocol_dispatch::reject_and_close(
                        stream,
                        kind,
                        write_error_response_io,
                    )?;
                    return Ok(());
                }
            },
            b'H' => match engine {
                Some(_engine) => match crate::extended_query::handle_flush(stream)? {
                    crate::extended_query::LoopSignal::Continue => {}
                    crate::extended_query::LoopSignal::Closed => return Ok(()),
                },
                None => {
                    framing::validate_typed_message_length_prefix(
                        stream,
                        framing::MIN_TYPED_MESSAGE_LEN,
                        framing::MAX_MESSAGE_LEN,
                    )?;
                    let kind = crate::protocol_dispatch::classify(type_byte);
                    crate::protocol_dispatch::reject_and_close(
                        stream,
                        kind,
                        write_error_response_io,
                    )?;
                    return Ok(());
                }
            },
            other => {
                // 拡張クエリプロトコル等の未対応メッセージ。長さフィールド
                // （最低 4 バイト、MIN_TYPED_MESSAGE_LEN..=MAX_MESSAGE_LEN）だけは
                // 他の型付きメッセージと同じ基準で検証する。未検証のまま
                // `0A000`（未対応機能）を返すと、長さフィールド欠落・範囲外の
                // malformed frame まで正規の未対応機能扱いにしてしまい、既存の
                // framing/protocol error 契約（`54000`/`08P01`）を迂回してしまう
                // ため（レビュー指摘の回帰防止）。本文自体は読まない
                // （`protocol_dispatch` 側が型バイトのみで分類し、ErrorResponse
                // 送出後は有界 lingering close で未読データを読み捨てる。
                // ポインタ: TASK-71・WIRE-8）。
                framing::validate_typed_message_length_prefix(
                    stream,
                    framing::MIN_TYPED_MESSAGE_LEN,
                    framing::MAX_MESSAGE_LEN,
                )?;
                let kind = crate::protocol_dispatch::classify(other);
                crate::protocol_dispatch::reject_and_close(stream, kind, write_error_response_io)?;
                return Ok(());
            }
        }

        // SQL-31・TASK-221（PR #1041 レビュー指摘）: 拡張クエリプロトコルの
        // Parse／Bind／Describe／Execute 等でエラー応答を返した場合
        // （`ignore_till_sync` が立つ）、明示トランザクション中なら種類を問わず
        // `Failed` へ遷移させる（PostgreSQL と同じ）。`Active` のまま残すと後続の
        // `COMMIT` が先行する書き込みを永続化してしまう。`fail` は `Active` 以外
        // では何もしないため、トランザクション外・既に `Failed` の挙動は不変。
        if extended.ignore_till_sync {
            if let Some(txn) = txn.as_mut() {
                txn.fail();
            }
        }
    }
}

/// 読み取りタイムアウトの切り詰め後に設定する最小値（`set_read_timeout` は
/// `Duration::ZERO` を受け付けないため、残り時間が 0 でもこの値で待つ）。
const MIN_BOUNDED_READ_WAIT: Duration = Duration::from_millis(1);

/// `Active` な明示トランザクション中にフレームの長さ・本文を受信する間の、
/// 読み取り 1 回あたりの待機上限（この間隔で `framing::FrameDeadlineGuard` の
/// 期限を確認する。期限超過の検出遅れの上限でもある）。
const FRAME_DEADLINE_POLL: Duration = Duration::from_millis(100);

/// [`post_auth_loop`] が次の要求の型バイトを待つ（SQL-31・TASK-221。PR #1041
/// レビュー指摘）。
///
/// 明示トランザクションが `Active`（共有書き込みトランザクション＝単一ライタを
/// 保持中）の間は、受信待ちの読み取りタイムアウトを「持続時間上限までの残り
/// 時間」と接続の読み取りタイムアウト（`base_timeout`。WIRE-5）の小さい方へ
/// 切り詰める。期限到達でタイムアウトした場合は
/// `engine::sql::transaction::SessionTransaction::release_if_expired` で abort
/// してライタを解放し（応答は送らない。受信のたびの検査と同じく、次の文／
/// `COMMIT` に `54000` が返る）、接続は維持したまま受信待ちを続ける。これにより
/// 無通信のクライアントが上限を超えてライタを占有し続けることはない。
///
/// - `Failed` は既にライタを解放済みのため切り詰めの対象外（`Idle` と同じく
///   `base_timeout` のまま待つ。この経路はシステムコールを追加しない）。
/// - WIRE-5 の無通信上限は不変: 期限到達後の再待機は、この関数へ入った時点から
///   の経過時間を差し引いた `base_timeout` の残りで待ち、尽きたら通常の読み取り
///   タイムアウトと同じ I/O エラー（応答なしで切断）を返す。
/// - 型バイトを受信できたら `base_timeout` へ戻す（ここでの打ち切りはフレームの
///   境界でだけ行う）。`Active` の間の長さ・本文の受信は、[`post_auth_loop`] が
///   `framing::FrameDeadlineGuard` でフレーム全体の期限を課す（期限超過は接続を
///   閉じてライタを解放する）。`applied` は現在ソケットに設定中の値で、変更が
///   必要なときだけ `set_read_timeout` を呼ぶ。
fn read_next_frame_header<S: WireStream>(
    stream: &mut S,
    txn: &mut Option<engine::sql::transaction::SessionTransaction<'_>>,
    base_timeout: Option<Duration>,
    applied: &mut Option<Duration>,
) -> Result<Option<u8>> {
    let waiting_since = Instant::now();
    let mut deadline_reached = false;
    loop {
        if let Some(t) = txn.as_mut() {
            t.release_if_expired();
        }
        let txn_remaining = txn.as_ref().and_then(|t| t.remaining_duration());
        let base_part = if deadline_reached {
            match base_timeout {
                Some(t) => {
                    let rest = t.saturating_sub(waiting_since.elapsed());
                    if rest.is_zero() {
                        return Err(HandshakeError::Io(io::Error::from(io::ErrorKind::TimedOut)));
                    }
                    Some(rest)
                }
                None => None,
            }
        } else {
            base_timeout
        };
        let (desired, bounded_by_txn) = match (base_part, txn_remaining) {
            (Some(b), Some(r)) if r < b => (Some(r.max(MIN_BOUNDED_READ_WAIT)), true),
            (None, Some(r)) => (Some(r.max(MIN_BOUNDED_READ_WAIT)), true),
            (b, _) => (b, false),
        };
        if *applied != desired {
            stream.set_read_timeout(desired)?;
            *applied = desired;
        }
        match framing::read_typed_frame_header(stream) {
            Ok(header) => {
                if *applied != base_timeout {
                    stream.set_read_timeout(base_timeout)?;
                    *applied = base_timeout;
                }
                return Ok(header);
            }
            Err(FrameError::Io(e))
                if bounded_by_txn
                    && matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
            {
                // 期限到達（またはタイマ粒度による直前の早期起床）。次の反復の
                // 先頭で `release_if_expired` が abort・ライタ解放を行う。
                deadline_reached = true;
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// `negotiate_startup`・`read_password_message`・`post_auth_loop` いずれのエラーも
/// ここへ集約して応答を分岐する（WIRE-4/WIRE-10）。
///
/// - `sqlstate()` が `Some`（`Protocol`・`Frame(TooLarge)`・`Frame(Malformed)`）:
///   固定の英語メッセージで ErrorResponse を送ってから切断する（送信失敗は無視。
///   相手が既に切断していれば送信自体が失敗しうるが、その場合も fail-closed に
///   切断で終わる）。`Frame` 由来のメッセージは `FrameError::client_message()` を、
///   `Protocol` 由来は呼び出し元が渡す `fallback_message` を用いる（`Protocol` は
///   フレーミング外の構文違反であり、`FrameError` に対応するメッセージを持たない
///   ため）。
/// - `Frame(Truncated)`: 相手が既に切断しているため応答を送らずに `Ok(())`。
/// - `Io`: サーバー側の異常として `Err` をそのまま返す（呼び出し元の
///   `server::accept_loop` がログに残す）。
fn respond_and_close<S: WireStream>(
    stream: &mut S,
    err: HandshakeError,
    fallback_message: &str,
) -> io::Result<()> {
    match err {
        HandshakeError::Io(e) => Err(e),
        HandshakeError::Frame(FrameError::Truncated) => Ok(()),
        HandshakeError::Frame(frame_err) => {
            if let Some(class) = frame_err.error_class() {
                let _ = write_error_response(stream, class, frame_err.client_message());
            }
            Ok(())
        }
        HandshakeError::Protocol(_) => {
            let _ = write_error_response(stream, ErrorClass::ProtocolViolation, fallback_message);
            Ok(())
        }
    }
}

/// 1 接続ぶんのハンドシェイク・認証・最小クエリループ全体。
/// `server::accept_loop_with_limiter` の接続受け付けスレッドから呼ばれる。
/// 戻り値の `Err` はネットワーク I/O 異常（クライアント切断・読み取り
/// タイムアウト等）を表し、呼び出し元はログのみでスレッドを終了してよい
/// （他接続には影響させない）。
///
/// 読み取りタイムアウト（`limits::READ_TIMEOUT`）は呼び出し元の
/// `server::accept_loop_with_limiter` が受理直後に一度だけソケットへ設定済み
/// であり、本関数はそれを認証前後で変更しない（WIRE-5: 接続全体に同一の期限を
/// 適用する）。例外は明示トランザクションが `Active` の間の要求待ちで、持続時間
/// 上限の残り時間へ一時的に切り詰める（無通信の上限そのものは不変。
/// [`read_next_frame_header`] 参照。SQL-31・TASK-221）。タイムアウト由来の `io::Error`（`TimedOut` / `WouldBlock`）は
/// ここで捕捉して ErrorResponse を書くことはせず、そのまま `Err` として
/// 呼び出し元へ返す（応答なしでクローズすることが WIRE-5 の契約）。
///
/// 対応: TASK-69（WIRE-5）。旧 3 引数シグネチャ（`post_auth_idle_timeout` を
/// 直接受け取る形）は [`handle_connection`]（deprecated 互換ラッパー）として
/// 維持する（codex-review / Cursor Bugbot 再指摘: 別名ラッパーでは後方互換に
/// ならず、旧名・旧シグネチャをそのまま残す必要がある）。
///
/// `engine` が `None` の場合、簡易クエリには従来どおり `0A000`（未実装）を返す
/// （本関数の既存呼び出し元・既存テストの契約を変えない）。`Some` を渡す新規
/// エントリポイントは [`handle_connection_with_engine`]（TASK-73・WIRE-1）。
pub fn handle_connection_bounded(stream: TcpStream, store: &UserStore) -> io::Result<()> {
    handle_connection_inner(stream, store, None)
}

/// engine（SQL 表層）を接続した簡易クエリ実行経路（TASK-73・WIRE-1）。
/// `server::accept_loop_with_engine` の接続受け付けスレッドから呼ばれる。
/// 認証・ハンドシェイクの契約は [`handle_connection_bounded`] と同一で、簡易
/// クエリ（'Q'）の実処理だけが `engine::core::EngineCore` へ委譲される点が異なる。
pub fn handle_connection_with_engine(
    stream: TcpStream,
    store: &UserStore,
    engine: &engine::core::EngineCore,
) -> io::Result<()> {
    handle_connection_inner(stream, store, Some(engine))
}

/// TLS opt-in を含む公開入口（Issue #966）。`tls` が `Some` の場合に限り
/// `SSLRequest` へ `'S'` を返し、`crate::tls::server_handshake::
/// perform_server_handshake` を実行してから以後の pg wire メッセージを
/// `crate::tls::stream::TlsStream` 上で処理する。`None` の場合は
/// [`handle_connection_bounded`]／[`handle_connection_with_engine`] と
/// ビット単位で同一の平文経路になる（受入基準 2）。
///
/// **`--tls-mode` opt-in（Issue #967）**: 本関数は常に
/// [`TlsMode::Allow`]（`SSLRequest` を経ない平文 StartupMessage も受理する。
/// #966 時点の既存挙動）で [`handle_connection_with_tls_mode`] へ委譲する
/// 後方互換ラッパーとして維持する（AGENTS.md 公開 API 互換方針）。CLI から
/// `--tls-mode require` を選んだ場合の平文拒否経路は新規エントリポイント
/// [`handle_connection_with_tls_mode`] が担う。
pub fn handle_connection_with_options(
    stream: TcpStream,
    store: &UserStore,
    engine: Option<&engine::core::EngineCore>,
    tls: Option<Arc<crate::tls::server_handshake::TlsServerConfig>>,
) -> io::Result<()> {
    handle_connection_with_tls_mode(stream, store, engine, tls, TlsMode::Allow)
}

/// TLS opt-in と平文接続ポリシー（[`TlsMode`]）の双方を含む新しい公開入口
/// （Issue #967。`server::accept_loop_with_tls_mode` から呼ばれる想定）。
///
/// `tls` が `None` の場合、`mode` は無視され [`handle_connection_bounded`]／
/// [`handle_connection_with_engine`] とビット単位で同一の平文経路になる
/// （TLS を CLI で構成していない構成は `--tls-mode` の意味を持たない）。
/// `tls` が `Some` の場合、`mode` が [`TlsMode::Require`] なら `SSLRequest`
/// を経ない平文 StartupMessage を `08P01` で拒否する（受入基準 3。
/// startup パラメータを解釈する前に拒否する。D8）。
pub fn handle_connection_with_tls_mode(
    stream: TcpStream,
    store: &UserStore,
    engine: Option<&engine::core::EngineCore>,
    tls: Option<Arc<crate::tls::server_handshake::TlsServerConfig>>,
    mode: TlsMode,
) -> io::Result<()> {
    handle_connection_inner_with_tls(stream, store, engine, tls, mode)
}

/// 認証の結果（成功時の `PolicyContext`、または失敗〔`ErrorResponse` は
/// [`authenticate`] 内で既に送出済みで、呼び出し元は接続を閉じるだけでよい〕）。
enum AuthOutcome {
    Success(engine::policy::PolicyContext),
    Failure,
}

/// `store.auth_method()` に応じて cleartext／SCRAM-SHA-256 のいずれかの認証
/// フローを実行する（Issue #940・WIRE-18・TASK-222。サーバー全体で 1 方式に
/// 固定し、ユーザーごとには切り替えない設計）。`AuthenticationOk` 以降の
/// 共通シーケンスは呼び出し元（[`handle_connection_inner`]）が担う。
fn authenticate<S: WireStream>(
    stream: &mut S,
    store: &UserStore,
    username: &str,
) -> Result<AuthOutcome> {
    match store.auth_method() {
        AuthMethod::Cleartext => authenticate_cleartext(stream, store, username),
        AuthMethod::ScramSha256 => authenticate_scram(stream, store, username),
    }
}

/// ポインタ: TASK-67・WIRE-3。既存の cleartext password フロー。
fn authenticate_cleartext<S: WireStream>(
    stream: &mut S,
    store: &UserStore,
    username: &str,
) -> Result<AuthOutcome> {
    write_authentication_cleartext_password(stream)?;
    let password = read_password_message(stream)?;
    match auth::verify(store, username, &password) {
        Err(_failure) => {
            write_error_response(stream, ErrorClass::AuthInvalid, auth::AuthFailure::MESSAGE)?;
            Ok(AuthOutcome::Failure)
        }
        Ok(ctx) => Ok(AuthOutcome::Success(ctx)),
    }
}

/// SCRAM-SHA-256（RFC 5802／RFC 7677）の SASL 往復（Issue #940・WIRE-18・
/// TASK-222）。未知ユーザーはモック検証子（`scram::mock_verifier`）で
/// 同一手順を最後まで実行し、固定遅延（`client-final` 検証開始時点から
/// [`auth::AUTH_FAILURE_DELAY`]）・同一のエラー応答で列挙攻撃を防ぐ。
/// TLS 接続かつ `tls-server-end-point`（[`crate::wire_stream::WireStream::
/// tls_server_end_point`]）を算出できた場合に限り SCRAM-SHA-256-PLUS
/// （`p=tls-server-end-point`）も提示・受理する（Issue #970）。それ以外
/// （TLS 未接続・非対応の署名アルゴリズム・提示無効化設定）では従来どおり
/// `-PLUS`／`p=<cb-name>` を提示・受理しない（`08P01`）。
fn authenticate_scram<S: WireStream>(
    stream: &mut S,
    store: &UserStore,
    username: &str,
) -> Result<AuthOutcome> {
    // この接続でチャネルバインディングを提供できるか（TLS 接続かつ
    // `tls-server-end-point` を算出できた場合のみ `Some`。TLS 未接続・
    // 非対応の署名アルゴリズム・提示無効化設定はいずれも `None` に畳み込み
    // 済み。Issue #970・WIRE-18）。
    let cb_end_point: Option<Vec<u8>> = stream.tls_server_end_point().map(|b| b.to_vec());
    let server_offers_plus = cb_end_point.is_some();

    write_authentication_sasl(stream, server_offers_plus)?;

    let (selected_plus, client_first_body) =
        read_sasl_initial_response(stream, server_offers_plus)?;
    let parse_client_first_fn = if server_offers_plus {
        scram::parse_client_first_with_cbind
    } else {
        scram::parse_client_first
    };
    let client_first = match parse_client_first_fn(&client_first_body) {
        Ok(c) => c,
        Err(scram::ScramError::ChannelBindingRequested) => {
            write_error_response(
                stream,
                ErrorClass::ProtocolViolation,
                "channel binding is not supported on this connection",
            )?;
            return Ok(AuthOutcome::Failure);
        }
        Err(_) => {
            write_error_response(
                stream,
                ErrorClass::ProtocolViolation,
                "malformed SASL message",
            )?;
            return Ok(AuthOutcome::Failure);
        }
    };

    // チャネルバインディングの交渉（RFC 5802 §6・§7。ユーザーの存在に
    // 依存しない純粋関数のため、mock 検証子の計算前・server-first 送出前に
    // 判定してよい。ダウングレード検出（PLUS 提示時に `y` を選ぶ）を含め、
    // 失敗はすべて同一の 08P01 応答へ収束させる）。
    let channel_binding = match scram::negotiate_channel_binding(
        selected_plus,
        &client_first.cbind_flag,
        server_offers_plus,
    ) {
        Ok(cb) => cb,
        Err(scram::ScramError::ChannelBindingRequested) => {
            write_error_response(
                stream,
                ErrorClass::ProtocolViolation,
                "channel binding is not supported on this connection",
            )?;
            return Ok(AuthOutcome::Failure);
        }
        Err(_) => {
            write_error_response(
                stream,
                ErrorClass::ProtocolViolation,
                "channel binding negotiation failed",
            )?;
            return Ok(AuthOutcome::Failure);
        }
    };
    // `negotiate_channel_binding` の判定表上、`TlsServerEndPoint` は
    // `server_offers_plus`（＝`cb_end_point.is_some()`）が真の場合にしか
    // 返らない。
    let cbind_data: Vec<u8> = match channel_binding {
        scram::ChannelBinding::None => Vec::new(),
        scram::ChannelBinding::TlsServerEndPoint => cb_end_point.clone().unwrap_or_default(),
    };

    // P0 review 指摘（Issue #940 PR #1006）: モック検証子の生成コストが
    // 既知/未知ユーザー間で非対称だと、`AuthenticationSASLContinue` 到着まで
    // の時間差でユーザー存在を列挙されうる（既知ユーザーは保存済み検証子の
    // `clone` のみ、未知ユーザーは `mock_verifier` が HMAC-SHA256 を 3 回
    // 実行してから決まる）。`scram_lookup` の分岐に関わらず必ず
    // `mock_verifier` を計算してから分岐することで、既知/未知いずれの経路も
    // 同じ計算量（`mock_verifier` 相当のコスト＋高々 1 回の `clone`）を
    // `server-first` 送出前に必ず消費させる（列挙攻撃対策。
    // ポインタ: WIRE-2, WIRE-3）。
    let Some(mock_key) = store.scram_mock_key() else {
        // 構造的に到達しないはずの分岐（`ScramSha256` は `UserStore::require_scram`
        // 経由でのみ選ばれ、その関数は必ずモック鍵を設定する）だが、
        // fail-closed に認証失敗として扱う（`unwrap`/`expect` でパニックさせない）。
        write_error_response(stream, ErrorClass::AuthInvalid, auth::AuthFailure::MESSAGE)?;
        return Ok(AuthOutcome::Failure);
    };
    let mock = scram::mock_verifier(mock_key, username, scram::SCRAM_ITERATIONS);
    let (tenant_id, verifier) = match store.scram_lookup(username) {
        Some((tenant, v)) => (Some(tenant.to_string()), v.clone()),
        None => (None, mock),
    };

    let server_nonce_raw = auth::read_urandom(18)?;
    let server_nonce_b64 = base64_std::encode(&server_nonce_raw);
    let combined_nonce = format!("{}{server_nonce_b64}", client_first.client_nonce);

    let server_first =
        scram::build_server_first(&combined_nonce, &verifier.salt, verifier.iterations);
    write_authentication_sasl_continue(stream, &server_first)?;

    let client_final_body = read_sasl_response(stream)?;
    let client_final = match scram::parse_client_final(&client_final_body) {
        Ok(c) => c,
        Err(_) => {
            write_error_response(
                stream,
                ErrorClass::ProtocolViolation,
                "malformed SASL message",
            )?;
            return Ok(AuthOutcome::Failure);
        }
    };

    // 固定遅延の起点は client-final の検証開始時点（構文違反〔上記〕は
    // 遅延の対象外。意味上の不一致〔以下〕はすべて同一の失敗応答・遅延へ
    // 収束させる）。
    let verify_start = std::time::Instant::now();

    // `c=` の期待値は `cbind-input = gs2-header ‖ cbind-data`（RFC 5802 §5・
    // RFC 5929 §4）。`cbind_data` はサーバー側が独立に算出した
    // `tls-server-end-point`（クライアントが送った `c=` から逆算しない。
    // Issue #970）であり、`channel_binding == None` の場合は空（既存
    // 契約とビット同一）。
    let mut expected_cbind_input =
        Vec::with_capacity(client_first.gs2_header.len() + cbind_data.len());
    expected_cbind_input.extend_from_slice(&client_first.gs2_header);
    expected_cbind_input.extend_from_slice(&cbind_data);
    let expected_channel_binding_b64 = base64_std::encode(&expected_cbind_input);
    let nonce_and_binding_match = client_final.channel_binding_b64 == expected_channel_binding_b64
        && client_final.nonce == combined_nonce;

    let client_final_no_proof = scram::client_final_without_proof_with_cbind(
        &client_first.gs2_header,
        &cbind_data,
        &combined_nonce,
    );
    let auth_message = scram::compute_auth_message(
        &client_first.client_first_bare,
        &server_first,
        &client_final_no_proof,
    );
    let verification = scram::verify_client_final(&verifier, &auth_message, &client_final.proof);

    let success = nonce_and_binding_match && verification.ok && tenant_id.is_some();

    let elapsed = verify_start.elapsed();
    if !success && elapsed < auth::AUTH_FAILURE_DELAY {
        std::thread::sleep(auth::AUTH_FAILURE_DELAY - elapsed);
    }

    if !success {
        write_error_response(stream, ErrorClass::AuthInvalid, auth::AuthFailure::MESSAGE)?;
        return Ok(AuthOutcome::Failure);
    }

    write_authentication_sasl_final(
        stream,
        &scram::build_server_final_success(&verification.server_signature),
    )?;

    // `success` が真の時点で `tenant_id` は必ず `Some`（上の `&&` 条件）。
    let tenant_id = tenant_id.ok_or(HandshakeError::Protocol("unreachable: missing tenant_id"))?;
    let ctx = auth::session_policy_context(&tenant_id)
        .map_err(|_| HandshakeError::Protocol("policy context rejected tenant_id"))?;
    Ok(AuthOutcome::Success(ctx))
}

fn handle_connection_inner(
    stream: TcpStream,
    store: &UserStore,
    engine: Option<&engine::core::EngineCore>,
) -> io::Result<()> {
    // `tls: None` のため `mode` は意味を持たない（`handle_connection_inner_with_tls`
    // ドキュメント参照）。既存呼び出し元とのビット単位互換のため `Allow` を渡す。
    handle_connection_inner_with_tls(stream, store, engine, None, TlsMode::Allow)
}

/// [`negotiate_startup_or_upgrade`] の戻り値。
enum PreTlsOutcome {
    /// StartupMessage を受理した（TLS へ昇格しない。`SSLRequest` を送らない
    /// クライアント、または `tls` 未設定時の従来経路）。
    Ready(String),
    /// `SSLRequest` を受理し、TLS へ昇格すべき（Issue #966）。
    UpgradeTls,
    /// `mode == TlsMode::Require` の下で、`SSLRequest` を経ない平文
    /// StartupMessage を受けた（Issue #967・受入基準 3・D8）。startup
    /// パラメータは解釈しない（`parse_startup_params` を呼ばない）。
    PlaintextRejected,
}

/// `tls` opt-in 時の `SSLRequest`/`GSSENCRequest`/StartupMessage 受理
/// （Issue #966・#967）。既存の [`negotiate_startup`]（TLS 未設定時に使う。
/// ビット単位で不変）と受理判定は同じだが、`SSLRequest` を受けた時点で
/// `'N'` を返さず [`PreTlsOutcome::UpgradeTls`] を返す点、および
/// `mode == TlsMode::Require` の下で平文 StartupMessage を
/// [`PreTlsOutcome::PlaintextRejected`] として拒否する点が異なる（応答は
/// 呼び出し元が組み立てる）。GSSENC は TLS 昇格の対象外のまま `'N'`
/// （既存契約を維持。`mode` に関わらず同一）。
fn negotiate_startup_or_upgrade(stream: &mut TcpStream, mode: TlsMode) -> Result<PreTlsOutcome> {
    // `SSLRequest` を受けた時点で即座に `UpgradeTls` を返す（呼び出し元が
    // 制御を引き継ぐ）ため、平文経路の `negotiate_startup` と異なり
    // `ssl_seen` フラグは不要 ―― この関数の同一呼び出し内で `SSLRequest` を
    // 2 回受け取ることは構造的に起こらない。
    let mut gssenc_seen = false;
    loop {
        let body = framing::read_startup_frame(stream)?;
        let code_bytes: [u8; 4] = body
            .get(0..4)
            .and_then(|s| s.try_into().ok())
            .ok_or(HandshakeError::Protocol("truncated startup code"))?;
        let code = i32::from_be_bytes(code_bytes);

        match code {
            SSL_REQUEST_CODE => {
                return Ok(PreTlsOutcome::UpgradeTls);
            }
            GSSENC_REQUEST_CODE if !gssenc_seen => {
                gssenc_seen = true;
                write_ssl_no_response(stream)?;
                continue;
            }
            GSSENC_REQUEST_CODE => {
                return Err(HandshakeError::Protocol("repeated SSL/GSSENC negotiation"));
            }
            CANCEL_REQUEST_CODE => {
                return Err(HandshakeError::Protocol(
                    "cancel request (not supported on this path)",
                ));
            }
            PROTOCOL_VERSION_3_0 => {
                // D8（Issue #967）: `require` では平文 StartupMessage の
                // パラメータを一切解釈せずに拒否する（ユーザー名等の
                // untrusted な値を無駄に処理しない）。
                if mode == TlsMode::Require {
                    return Ok(PreTlsOutcome::PlaintextRejected);
                }
                return parse_startup_params(&body[4..]).map(PreTlsOutcome::Ready);
            }
            _ => {
                return Err(HandshakeError::Protocol("unsupported protocol version"));
            }
        }
    }
}

/// TLS 確立後（`TlsStream` 上）の StartupMessage 受理（Issue #966・受入
/// 基準 3）。`SSLRequest`／`GSSENCRequest` はいずれも（初回であっても）
/// `Protocol` エラーとし、平文経路の「2 回目の SSLRequest」と同じ応答契約
/// （[`respond_and_close`] 経由の ErrorResponse・切断）へ倒す。PostgreSQL
/// 本体も TLS 確立後の再ネゴシエーション要求は拒否する。
fn negotiate_after_tls<S: WireStream>(stream: &mut S) -> Result<String> {
    let body = framing::read_startup_frame(stream)?;
    let code_bytes: [u8; 4] = body
        .get(0..4)
        .and_then(|s| s.try_into().ok())
        .ok_or(HandshakeError::Protocol("truncated startup code"))?;
    let code = i32::from_be_bytes(code_bytes);
    match code {
        SSL_REQUEST_CODE | GSSENC_REQUEST_CODE => Err(HandshakeError::Protocol(
            "SSL/GSSENC negotiation is not allowed once TLS is established",
        )),
        CANCEL_REQUEST_CODE => Err(HandshakeError::Protocol(
            "cancel request (not supported on this path)",
        )),
        PROTOCOL_VERSION_3_0 => parse_startup_params(&body[4..]),
        _ => Err(HandshakeError::Protocol("unsupported protocol version")),
    }
}

/// 認証成功後の共通シーケンス（`AuthenticationOk` 以降・[`post_auth_loop`]）。
/// 平文接続（`tls` 未設定分岐）・TLS 確立後の接続
/// （[`handle_tls_upgrade`]）の双方が共有する（Issue #966）。ロジック
/// （分岐・応答内容・順序）は従来の `handle_connection_inner` から一切
/// 変更せず、ストリーム型を `WireStream` へ一般化しただけ。
fn run_authenticated_session<S: WireStream>(
    stream: &mut S,
    store: &UserStore,
    engine: Option<&engine::core::EngineCore>,
    username: String,
) -> io::Result<()> {
    let outcome = match authenticate(stream, store, &username) {
        Ok(o) => o,
        Err(e) => {
            // Cursor Bugbot 指摘（Issue #940 PR #1006）: `authenticate` の
            // `HandshakeError::Protocol` は `respond_and_close` の
            // `fallback_message` をそのまま ErrorResponse の M フィールドへ
            // 使う（`Frame` エラーは `frame_err.client_message()` を優先する
            // ため本分岐の対象外）。cleartext フローの
            // `read_password_message` が返す `Protocol` エラー（不正な
            // PasswordMessage）は本 Issue 以前 `"invalid password message"`
            // だったが、cleartext/SCRAM 共通の `authenticate()` へ統合した際に
            // 単一の `"invalid message frame"` へ潰れ既定 cleartext 経路の
            // ErrorResponse がビット同一でなくなっていた。認証方式ごとに
            // fallback を分けて既定 cleartext 経路の応答を復元する
            // （SCRAM 経路は本 Issue で新設のため従来メッセージを持たず
            // `"invalid message frame"` のまま）。
            let fallback = match store.auth_method() {
                AuthMethod::Cleartext => "invalid password message",
                AuthMethod::ScramSha256 => "invalid message frame",
            };
            return respond_and_close(stream, e, fallback);
        }
    };
    let ctx = match outcome {
        AuthOutcome::Failure => return Ok(()),
        AuthOutcome::Success(ctx) => ctx,
    };

    // 読み取りタイムアウトは `server::accept_loop_with_limiter` が接続全体に
    // 一度だけ設定済み（WIRE-5）であり、ここで切り替えない（TLS 昇格時は
    // `handle_tls_upgrade` がハンドシェイク前後で退避・復元する）。
    write_authentication_ok(stream)?;
    // BackendKeyData の値そのものはキャンセル要求の照合以外に使わないため、
    // 暗号学的な強さは要求しない。プロセス ID とプロセス内カウンタで十分。
    let pid = std::process::id() as i32;
    let secret = connection_counter();
    write_backend_key_data(stream, pid, secret)?;
    write_parameter_status(stream, "server_version", "14.0")?;
    write_parameter_status(stream, "client_encoding", "UTF8")?;
    write_ready_for_query(stream, engine::sql::transaction::TransactionStatus::Idle)?;

    // 接続単位のセッション状態（取得モード・宣言的 UDF レジストリ）。
    // `EngineCore` 自体は保持しない（`sql::mode` モジュールドキュメント参照）。
    let mut session = engine::sql::mode::SessionState::default();
    // 接続単位の明示トランザクション状態（SQL-31・TASK-221）。`session` と同じく
    // 接続終了（drop）で破棄する ―― 未 commit のまま接続が切れた場合、保持中の
    // 共有 `redb::WriteTransaction` は commit されずに abort され、単一ライタの
    // 占有（`writer_gate::WriterPermit`）も解放される。
    let mut txn = engine.map(|e| e.new_session_transaction());
    // SQL-23・TASK-202・TASK-203（Issue #899・#902）: DDL 実行権限
    // （`CREATE TABLE`・`DROP TABLE` 共通）は認証成功後（＝`username` が
    // 確定した時点）に 1 回だけ付与する。`PolicyContext`（`ctx`）はテナント ID・
    // 可視性のみを運び認証主体を持たないため、この判定は `ctx` ではなく
    // `store`（`--ddl-allowed-users`）に基づく。SQL 文経由でセッションが自身の
    // 権限を昇格させる経路は構造的に存在しない。
    if store.is_ddl_allowed(&username) {
        session.allow_ddl();
    }
    // 接続単位の Parse 済みステートメント・portal 保持（Issue #933・#934・
    // TASK-71・WIRE-11）。`session` と同じく接続終了で破棄し、接続間・
    // テナント間で共有しない（`extended_query` モジュールドキュメント参照）。
    let mut extended = crate::extended_query::ExtendedQueryState::new();

    match post_auth_loop(stream, &ctx, engine, &mut session, &mut txn, &mut extended) {
        Ok(()) => Ok(()),
        Err(e) => respond_and_close(stream, e, "invalid message frame"),
    }
}

/// TLS opt-in と平文接続ポリシー（[`TlsMode`]）を含む接続処理本体
/// （Issue #966・#967）。`tls` が `None` の場合は `mode` を無視し
/// [`negotiate_startup`]（`'N'` 応答。ビット単位で不変）へそのまま委譲し
/// （受入基準 2。TLS を CLI で構成していない構成に `--tls-mode` の意味は
/// 無い）、`Some` の場合のみ [`negotiate_startup_or_upgrade`] へ `mode` を
/// 渡し、`SSLRequest` を検出して `'S'` を返し TLS ハンドシェイクへ進める。
/// `mode == TlsMode::Require` の下で平文 StartupMessage を受けた場合は
/// startup パラメータを解釈せず `08P01` で拒否する（受入基準 3・D8）。
fn handle_connection_inner_with_tls(
    mut stream: TcpStream,
    store: &UserStore,
    engine: Option<&engine::core::EngineCore>,
    tls: Option<Arc<crate::tls::server_handshake::TlsServerConfig>>,
    mode: TlsMode,
) -> io::Result<()> {
    let Some(tls_config) = tls else {
        let username = match negotiate_startup(&mut stream) {
            Ok(u) => u,
            Err(e) => return respond_and_close(&mut stream, e, "invalid startup packet"),
        };
        return run_authenticated_session(&mut stream, store, engine, username);
    };

    match negotiate_startup_or_upgrade(&mut stream, mode) {
        Ok(PreTlsOutcome::Ready(username)) => {
            run_authenticated_session(&mut stream, store, engine, username)
        }
        Ok(PreTlsOutcome::UpgradeTls) => handle_tls_upgrade(stream, store, engine, tls_config),
        Ok(PreTlsOutcome::PlaintextRejected) => respond_and_close(
            &mut stream,
            HandshakeError::Protocol("plaintext startup rejected (tls-mode=require)"),
            "TLS is required by this server; plaintext connections are not accepted",
        ),
        Err(e) => respond_and_close(&mut stream, e, "invalid startup packet"),
    }
}

/// `SSLRequest` へ `'S'` を返した直後の TLS ハンドシェイク実行と、以後の
/// pg wire メッセージを [`crate::tls::stream::TlsStream`] 上で処理する
/// 経路（Issue #966）。
fn handle_tls_upgrade(
    mut stream: TcpStream,
    store: &UserStore,
    engine: Option<&engine::core::EngineCore>,
    tls_config: Arc<crate::tls::server_handshake::TlsServerConfig>,
) -> io::Result<()> {
    // `'S'`（受理）を返す。以後のバイト列は TLS レコードとして扱われる。
    write_all(&mut stream, b"S")?;

    // ハンドシェイク driver（`perform_server_handshake_with`）は
    // `DeadlineReader`/`DeadlineWriter` でソケットの読み書きタイムアウトを
    // 都度上書きし、成功時も `HANDSHAKE_READ_TIMEOUT` 定数のまま残す
    // （#965 の既存設計。呼び出し元での補正が前提）。ここで接続設定値
    // （`server::accept_loop_*` が受理直後に設定した値。WIRE-5）を退避し、
    // ハンドシェイク完了後に再適用することで、受入基準 4（TLS 上でも同じ
    // タイムアウト値が働く）を満たす。
    let saved_read_timeout = stream.read_timeout()?;
    let saved_write_timeout = stream.write_timeout()?;

    let session =
        match crate::tls::server_handshake::perform_server_handshake(&mut stream, tls_config) {
            Ok(session) => session,
            Err(_e) => {
                // alert の送出・切断は driver 側が既に行っている
                // （`perform_server_handshake_with` のドキュメント参照）。
                // ErrorResponse は送らない（TLS ハンドシェイクの失敗は
                // ERR-1/2/4 の wire_code 写像の対象外。
                // `docs/design/tls-wire-connection.md` 参照）。
                return Err(io::Error::other("TLS handshake failed"));
            }
        };

    if stream.set_read_timeout(saved_read_timeout).is_err()
        || stream.set_write_timeout(saved_write_timeout).is_err()
    {
        return Err(io::Error::other(
            "failed to restore connection timeouts after TLS handshake",
        ));
    }

    let mut tls_stream = crate::tls::stream::TlsStream::new(stream, session);

    let username = match negotiate_after_tls(&mut tls_stream) {
        Ok(u) => u,
        Err(e) => {
            let result = respond_and_close(&mut tls_stream, e, "invalid startup packet");
            tls_stream.graceful_close();
            return result;
        }
    };

    let result = run_authenticated_session(&mut tls_stream, store, engine, username);
    tls_stream.graceful_close();
    result
}

/// 旧 `(stream, store, post_auth_idle_timeout)` 3 引数シグネチャとの後方互換
/// ラッパー（**旧名・旧シグネチャをそのまま維持**。codex-review / Cursor
/// Bugbot 再指摘: 別名のラッパーを追加するだけでは呼び出し元のコンパイルが
/// 通らず後方互換にならないため、新実装は [`handle_connection_bounded`]
/// という別名へ移し、この名前・シグネチャの方を互換層として残す）。
///
/// TASK-69（WIRE-5）で読み取りタイムアウトは認証前後で切り替えない単一値方式
/// （`server::accept_loop_with_limiter` が受理直後に一度だけ設定する）へ統一
/// され、本体の実装はタイムアウトを受け取らない 2 引数シグネチャへ移行した。
/// 本関数はすでに公開 API として利用側に届いている可能性のある旧シグネチャを
/// 維持しつつ、内部では新実装（[`handle_connection_bounded`]）へ委譲する
/// （`server::bind_loopback` と同じ後方互換方針）。
///
/// `post_auth_idle_timeout` は WIRE-5 の単一タイムアウト契約により**無視**する
/// （認証前後で値を切り替える経路自体が存在しないため）。新規コードは
/// `handle_connection_bounded` を直接呼ぶこと。
#[deprecated(
    since = "0.1.0",
    note = "use handle_connection_bounded(stream, store) instead; post_auth_idle_timeout is ignored (WIRE-5 uses a single read_timeout for the whole connection)"
)]
pub fn handle_connection(
    stream: TcpStream,
    store: &UserStore,
    _post_auth_idle_timeout: Duration,
) -> io::Result<()> {
    handle_connection_bounded(stream, store)
}

/// BackendKeyData の secret フィールド用のプロセス内カウンタ（接続ごとに異なる値を
/// 割り当てるだけの用途で、暗号学的強度は不要）。
fn connection_counter() -> i32 {
    use std::sync::atomic::{AtomicI32, Ordering};
    static COUNTER: AtomicI32 = AtomicI32::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::sync::atomic::{AtomicU64, Ordering};

    /// フィクスチャ一時ディレクトリ名の一意性を pid・時刻だけに委ねないための
    /// プロセス内単調カウンタ（`tests/wire_auth.rs` と同一クラスの競合対策。
    /// Issue #172）。
    static FIXTURE_SEQ: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn parse_startup_params_extracts_user_and_ignores_database() {
        let mut body = Vec::new();
        body.extend_from_slice(b"user\0alice\0");
        body.extend_from_slice(b"database\0other-tenant\0");
        body.push(0);
        let user = parse_startup_params(&body).expect("valid params");
        assert_eq!(user, "alice");
    }

    #[test]
    fn parse_startup_params_rejects_missing_user() {
        let mut body = Vec::new();
        body.extend_from_slice(b"database\0d\0");
        body.push(0);
        assert!(parse_startup_params(&body).is_err());
    }

    #[test]
    fn parse_startup_params_rejects_unterminated_string() {
        let body = b"user\0alice".to_vec(); // 値が null 終端されていない
        assert!(parse_startup_params(&body).is_err());
    }

    #[test]
    fn parse_startup_params_rejects_invalid_utf8() {
        let mut body = Vec::new();
        body.extend_from_slice(b"user\0");
        body.push(0xFF);
        body.push(0);
        body.push(0);
        assert!(parse_startup_params(&body).is_err());
    }

    /// review 指摘の再現ケース: 終端（空キー）後に残余バイトがあれば拒否すること。
    #[test]
    fn parse_startup_params_rejects_trailing_data_after_terminator() {
        let mut body = Vec::new();
        body.extend_from_slice(b"user\0alice\0");
        body.push(0); // 終端
        body.extend_from_slice(b"trailing garbage");
        assert!(parse_startup_params(&body).is_err());
    }

    /// review 指摘の再現ケース: `user` キーが複数回出現する場合は後勝ちで上書きせず
    /// 拒否すること（テナント決定の唯一の入力経路への曖昧な混入余地を作らない）。
    #[test]
    fn parse_startup_params_rejects_duplicate_user() {
        let mut body = Vec::new();
        body.extend_from_slice(b"user\0alice\0");
        body.extend_from_slice(b"user\0mallory\0");
        body.push(0);
        assert!(parse_startup_params(&body).is_err());
    }

    fn write_ssl_request(stream: &mut TcpStream) {
        let mut msg = Vec::new();
        msg.extend_from_slice(&8i32.to_be_bytes());
        msg.extend_from_slice(&SSL_REQUEST_CODE.to_be_bytes());
        stream.write_all(&msg).expect("send SSLRequest");
    }

    /// SSLRequest への応答は各コードにつき高々 1 回。2 回目は無限ループへ入らず
    /// fail-closed で拒否されること（無応答・スレッド占有の回帰確認）。
    #[test]
    fn negotiate_startup_rejects_repeated_ssl_request() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = negotiate_startup(&mut stream);
            assert!(result.is_err(), "second SSLRequest must be rejected");
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        write_ssl_request(&mut client);
        let mut resp = [0u8; 1];
        client.read_exact(&mut resp).expect("read first N");
        assert_eq!(&resp, b"N");

        // 2 回目の SSLRequest: 応答を待たず、サーバー側が拒否して接続を閉じる。
        write_ssl_request(&mut client);
        let mut extra = [0u8; 1];
        let n = client.read(&mut extra).unwrap_or(0);
        assert_eq!(
            n, 0,
            "server must close rather than answer a second SSLRequest"
        );

        server.join().expect("server thread must not panic");
    }

    /// 長さプレフィックス付きメッセージ（type byte + length + body）をそのまま
    /// クライアント側から送るテスト用ヘルパー。
    fn write_length_prefixed_message(stream: &mut TcpStream, type_byte: u8, body: &[u8]) {
        let total_len = (4 + body.len()) as i32;
        let mut msg = Vec::with_capacity(1 + body.len() + 4);
        msg.push(type_byte);
        msg.extend_from_slice(&total_len.to_be_bytes());
        msg.extend_from_slice(body);
        stream.write_all(&msg).expect("send message");
    }

    /// review 指摘の再現ケース: 末尾以外に NUL を含む PasswordMessage
    /// （`password\0suffix\0`）を fail-closed で拒否すること。
    #[test]
    fn read_password_message_rejects_embedded_nul() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = read_password_message(&mut stream);
            assert!(result.is_err(), "embedded NUL must be rejected");
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        write_length_prefixed_message(&mut client, b'p', b"password\0suffix\0");

        server.join().expect("server thread must not panic");
    }

    /// 正常系の対照確認: 内部 NUL を含まない PasswordMessage は受理されること。
    #[test]
    fn read_password_message_accepts_well_formed_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = read_password_message(&mut stream);
            assert_eq!(result.expect("valid password"), b"correct-horse".to_vec());
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        write_length_prefixed_message(&mut client, b'p', b"correct-horse\0");

        server.join().expect("server thread must not panic");
    }

    fn dummy_policy_context() -> engine::policy::PolicyContext {
        engine::policy::PolicyContext::new("tenant-a").expect("valid tenant id")
    }

    /// review 指摘の再現ケース: 簡易クエリ（'Q'）の body が空（終端 NUL すら
    /// 無い）場合は fail-closed で拒否すること。
    #[test]
    fn post_auth_loop_rejects_empty_query_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let ctx = dummy_policy_context();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = post_auth_loop(
                &mut stream,
                &ctx,
                None,
                &mut engine::sql::mode::SessionState::default(),
                &mut None,
                &mut crate::extended_query::ExtendedQueryState::new(),
            );
            assert!(result.is_err(), "empty query body must be rejected");
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        write_length_prefixed_message(&mut client, b'Q', b"");

        server.join().expect("server thread must not panic");
    }

    /// review 指摘の再現ケース: 簡易クエリの body に末尾以外の埋め込み NUL が
    /// あれば fail-closed で拒否すること。
    #[test]
    fn post_auth_loop_rejects_query_with_embedded_nul() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let ctx = dummy_policy_context();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = post_auth_loop(
                &mut stream,
                &ctx,
                None,
                &mut engine::sql::mode::SessionState::default(),
                &mut None,
                &mut crate::extended_query::ExtendedQueryState::new(),
            );
            assert!(result.is_err(), "embedded NUL in query must be rejected");
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        write_length_prefixed_message(&mut client, b'Q', b"select\0 1\0");

        server.join().expect("server thread must not panic");
    }

    /// review 指摘の再現ケース: Terminate（'X'）は length=4（body 厳密に空）以外を
    /// fail-closed で拒否すること。
    #[test]
    fn post_auth_loop_rejects_terminate_with_nonempty_body() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let ctx = dummy_policy_context();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = post_auth_loop(
                &mut stream,
                &ctx,
                None,
                &mut engine::sql::mode::SessionState::default(),
                &mut None,
                &mut crate::extended_query::ExtendedQueryState::new(),
            );
            assert!(
                result.is_err(),
                "Terminate with a non-empty body must be rejected"
            );
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        write_length_prefixed_message(&mut client, b'X', b"unexpected");

        server.join().expect("server thread must not panic");
    }

    /// 正常系の対照確認: length=4・body 厳密に空の Terminate は正常終了すること。
    #[test]
    fn post_auth_loop_accepts_well_formed_terminate() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let ctx = dummy_policy_context();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = post_auth_loop(
                &mut stream,
                &ctx,
                None,
                &mut engine::sql::mode::SessionState::default(),
                &mut None,
                &mut crate::extended_query::ExtendedQueryState::new(),
            );
            assert!(result.is_ok(), "well-formed Terminate must succeed");
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        write_length_prefixed_message(&mut client, b'X', b"");

        server.join().expect("server thread must not panic");
    }

    /// レビュー指摘の再現ケース: 未対応メッセージ（例: Parse 'P'）であっても、
    /// 宣言長が `MIN_TYPED_MESSAGE_LEN`（4）未満の malformed frame は
    /// `0A000`（未対応機能）としてではなく、既存の `FrameError` 経路
    /// （`08P01` 相当）で fail-closed に拒否されること（`post_auth_loop` は
    /// `Err` を返し、呼び出し元の `respond_and_close` が応答を分岐する）。
    #[test]
    fn post_auth_loop_rejects_unsupported_message_with_length_below_minimum() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let ctx = dummy_policy_context();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = post_auth_loop(
                &mut stream,
                &ctx,
                None,
                &mut engine::sql::mode::SessionState::default(),
                &mut None,
                &mut crate::extended_query::ExtendedQueryState::new(),
            );
            match result {
                Err(HandshakeError::Frame(_)) => {}
                other => panic!(
                    "malformed length prefix on an unsupported message must surface as a                      FrameError, got {other:?}"
                ),
            }
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        // 型バイト 'P'（Parse・未対応）の直後に、body を含めない宣言長 3
        // （MIN_TYPED_MESSAGE_LEN=4 未満）だけを送る。
        client.write_all(b"P").expect("send type byte");
        client
            .write_all(&3i32.to_be_bytes())
            .expect("send malformed length");

        server.join().expect("server thread must not panic");
    }

    /// レビュー指摘の再現ケース: 未対応メッセージの宣言長が `MAX_MESSAGE_LEN` を
    /// 超える場合も同様に `FrameError`（`TooLarge`・`54000` 相当）経路へ送られ、
    /// `0A000` を返さないこと。
    #[test]
    fn post_auth_loop_rejects_unsupported_message_with_length_too_large() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let ctx = dummy_policy_context();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = post_auth_loop(
                &mut stream,
                &ctx,
                None,
                &mut engine::sql::mode::SessionState::default(),
                &mut None,
                &mut crate::extended_query::ExtendedQueryState::new(),
            );
            match result {
                Err(HandshakeError::Frame(FrameError::TooLarge { .. })) => {}
                other => panic!(
                    "oversized length prefix on an unsupported message must surface as                      FrameError::TooLarge, got {other:?}"
                ),
            }
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        let declared = (framing::MAX_MESSAGE_LEN + 1) as i32;
        client.write_all(b"P").expect("send type byte");
        client
            .write_all(&declared.to_be_bytes())
            .expect("send oversized length");

        server.join().expect("server thread must not panic");
    }

    /// 正常系の対照確認: 未対応メッセージでも宣言長が妥当な範囲内であれば、
    /// 従来どおり `reject_and_close`（`0A000` 応答＋切断）へ到達し、
    /// `post_auth_loop` は `Ok(())` を返すこと。
    #[test]
    fn post_auth_loop_accepts_well_formed_unsupported_message_length() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let ctx = dummy_policy_context();

        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let result = post_auth_loop(
                &mut stream,
                &ctx,
                None,
                &mut engine::sql::mode::SessionState::default(),
                &mut None,
                &mut crate::extended_query::ExtendedQueryState::new(),
            );
            assert!(
                result.is_ok(),
                "well-formed length prefix on an unsupported message must still be rejected                  via 0A000 and return Ok(())"
            );
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        // 型バイト 'P'（Parse・未対応）+ 妥当な宣言長のみ（body は付けない。
        // `reject_and_close` 側は body を読まず lingering close で読み捨てる）。
        write_length_prefixed_message(&mut client, b'P', b"");

        let mut resp_type = [0u8; 1];
        client
            .read_exact(&mut resp_type)
            .expect("read response type");
        assert_eq!(resp_type[0], b'E', "expected ErrorResponse ('E')");

        server.join().expect("server thread must not panic");
    }

    /// P1 review 再指摘（codex-review / Cursor Bugbot: 別名ラッパーでは
    /// 後方互換にならない）の再現ケース: 旧名・旧 3 引数シグネチャの
    /// [`handle_connection`]（deprecated）を呼んでも panic せず、新実装
    /// （[`handle_connection_bounded`]）へ委譲されること。無効な
    /// StartupMessage（負の長さ）を送り、応答なしで正常にクローズすることまで
    /// 確認する（`post_auth_idle_timeout` は無視される契約のため、値そのものは
    /// 検証しない）。
    #[test]
    #[allow(deprecated)]
    fn handle_connection_compat_wrapper_delegates_without_panic() {
        let seq = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-server-handshake-test-legacy-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos(),
            seq
        ));
        // `create_dir`（既存なら `Err`）で衝突を黙って吸収せず顕在化させる
        // （Issue #172）。
        std::fs::create_dir(&dir).expect("create unique fixture dir");
        let path = dir.join("users.txt");
        std::fs::write(&path, "").expect("write empty user store");
        let store = UserStore::load_from_file(&path).expect("valid empty store");

        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");

        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept");
            // 旧名・旧シグネチャをそのまま呼ぶ（互換性の実体はここで検証する）。
            let result = handle_connection(stream, &store, Duration::from_secs(300));
            assert!(
                result.is_ok(),
                "malformed startup must close without I/O error"
            );
        });

        let mut client = TcpStream::connect(addr).expect("connect");
        let mut msg = Vec::new();
        msg.extend_from_slice(&(-1i32).to_be_bytes());
        client
            .write_all(&msg)
            .expect("send negative-length startup");

        let mut extra = [0u8; 1];
        let n = client.read(&mut extra).unwrap_or(0);
        // ErrorResponse の 'E' か、応答なし EOF のいずれか（フレーミング拒否経路の
        // 詳細は本テストの関心事ではない）。ここでは委譲先が呼ばれて panic せずに
        // クローズすることのみを確認する。
        let _ = n;

        server.join().expect("server thread must not panic");
        let _ = std::fs::remove_file(&path);
    }
}
