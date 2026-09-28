//! COPY プロトコル（`COPY ... FROM STDIN`／`COPY (...) TO STDOUT`）のメッセージ層
//! （Issue #939・WIRE-17・TASK-220）。
//!
//! 責務境界: 本モジュールは CopyIn／CopyOut サブプロトコルのフレーミング・
//! 状態遷移のみを担う。行のレコード分割・フィールドデコード・束縛・INDEX-4
//! 上限判定・commit はすべて `engine::sql::copy`（`EngineCore::begin_copy`／
//! `commit_copy_in`）へ委譲する（第 2 の書き込み経路を作らない設計）。
//!
//! 呼び出し文脈: `handshake::post_auth_loop` の 'Q' 分岐が、UTF-8 検証済みの
//! クエリテキストへ `engine::sql::copy::is_copy_statement` を適用して真なら
//! ここへ委譲する。本モジュールは COPY サブプロトコルの開始（CopyInResponse／
//! CopyOutResponse）から終了（`CommandComplete`＋`ReadyForQuery`）までを
//! 1 回の呼び出しで完結させる（`simple_query::execute_and_respond` と同じ
//! 「1 回の呼び出しで応答を書き切る」契約）。
//!
//! **PostgreSQL 本家との既知の相違点**（`docs/design/wire-copy-protocol.md`
//! 参照）: protocol v3 の COPY は CopyDone（'c'）で終端を表現するため、
//! protocol v2 由来の `\.` 終端行は受理・要求しない。COPY FROM STDIN の途中で
//! エラーが起きた場合、本実装は CopyDone／CopyFail を受信し終えるまで
//! ErrorResponse／ReadyForQuery の送出を遅らせる（PostgreSQL 本家は
//! ErrorResponse を即座に送るが、その後もクライアントが CopyDone／CopyFail を
//! 送ってくることを許容し続ける必要があり、次の 'Q' が先に届く可能性のある
//! 接続レベルの「読み捨て状態」を要求する。本実装は COPY サブプロトコルの
//! 開始から終了までを 1 回の関数呼び出しで完結させる単純化のため、
//! ErrorResponse は CopyDone／CopyFail 受信後にまとめて送る）。

use std::io;

use engine::core::{CopyPlan, EngineCore};
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::policy::PolicyContext;
use engine::sql::allowlist::{CopyFormat, SqlSurfaceError};
use engine::sql::copy::CopyInSession;
use engine::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use engine::sql::mode::SessionState;

use crate::framing::{self, FrameError};
use crate::result_encoder;
use crate::wire_stream::WireStream;

/// [`FrameError`] を `io::Result` の失敗へ変換する。`Truncated`（相手が既に
/// 切断）は「応答なしで終了してよい」を表すため `UnexpectedEof` へ、それ以外
/// （`TooLarge`／`Malformed`／`Io`）は接続を終了させる `io::Error` へ写像する。
/// `handshake::HandshakeError` は本モジュールでは使わない（`crate::copy::run`
/// は `simple_query::execute_and_respond` と同じ `io::Result<()>` 契約で
/// 完結させるため）。
fn frame_err_to_io(e: FrameError) -> io::Error {
    match e {
        FrameError::Truncated => io::Error::new(io::ErrorKind::UnexpectedEof, "truncated frame"),
        FrameError::Io(io_err) => io_err,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

/// [`run_copy_from`] のフレーム読み取り（'d'／'c'／'f'／'H'／'S'／'X' の
/// いずれの分岐が読む長さフィールド・本文も含む）が `FrameError::TooLarge`／
/// `Malformed` で失敗した場合、通常の簡易クエリ 'Q' 経路（`handshake::
/// respond_and_close` の `HandshakeError::Frame` 分岐）と同じ `wire_code` 付き
/// ErrorResponse を送ってから接続を終了する（Issue #939 レビュー指摘: 当初は
/// 'd' 分岐のみこの経路を通し、他の分岐は素通しで [`frame_err_to_io`] へ渡して
/// いたため `HandshakeError::Io` へ写像され、`respond_and_close` の
/// `Io(e) => Err(e)` 分岐が応答を一切送らずに切断していた——同型の後退が
/// 'c'／'f'／'H'／'S'／'X' でも起こり得たため、ループ内の全フレーム読み取りを
/// この関数へ統一した）。フレーミングが破綻した時点でストリーム上の残り
/// バイト数は不明で安全に読み進められないため、`respond_error_and_ready`
/// （CopyFail 等）とは異なり ReadyForQuery は送らず接続そのものを終了する
/// 契約にする（`TooLarge` は長さフィールドのみ読み終え本文は未読のまま、
/// `Malformed` も相手が今後どれだけ送るか分からない点は同じ）。ErrorResponse
/// の送信自体が失敗しても（相手が既に切断済み等）無視して `frame_err_to_io`
/// の結果を返す（fail-closed に接続を終える）。`Truncated`／`Io` は応答を
/// 送る意味がない（前者は相手が既に切断済み、後者はサーバー側 I/O 異常）ため
/// 従来どおり無応答で終了する。
fn respond_frame_error_and_terminate<S: WireStream>(stream: &mut S, e: FrameError) -> io::Error {
    if let Some(class) = e.error_class() {
        let _ = crate::handshake::write_error_response_io(stream, class, e.client_message());
    }
    frame_err_to_io(e)
}

/// 読み捨てたバイト数を `discarded_bytes` へ加算したうえで
/// `limits::COPY_DISCARD_MAX_BYTES` の総量チェックを行い、超過していれば
/// `08P01`（`ProtocolViolation`）の ErrorResponse を送ってから接続を終了する
/// エラーを返す（Cursor Bugbot 指摘・Issue #939 レビュー対応:
/// エラー後の read-discard ループが `CopyData`（'d'）のバイト量しかこの
/// 上限へ数えず、Flush('H')／Sync('S') のボディは無制限に読み捨てていた
/// ため、クライアントが巨大な no-op フレームを送り続けることで DoS 上限を
/// 回避し接続スロットを占有し続けられた——'d'／'H'／'S' いずれの読み捨ても
/// この単一の関数を通して同じ予算を共有させる）。旧実装は超過時に応答なし
/// の生 `io::Error` を返していた（codex-review P1 指摘・discussion_
/// r4096720869）ため、他の frame エラー経路（[`respond_frame_error_and_
/// terminate`]）と同じく ErrorResponse を送ってから切断する契約へ揃える。
///
/// `discarded_messages`（読み捨てたフレーム件数）も独立に判定する
/// （codex-review 指摘・PRRT_kwDOUAKASM6ltyY2: body が空（宣言長 4）の
/// `CopyData`／`Flush`／`Sync` だけを連送された場合、バイト予算
/// （[`crate::limits::COPY_DISCARD_MAX_BYTES`]）を使い切るまでに要する件数が
/// 非現実的に大きく実効的な上限として機能しない。件数上限
/// [`crate::limits::COPY_DISCARD_MAX_MESSAGES`] をバイト予算とは別に設けることで、
/// フレームサイズに関わらず読み捨て件数そのものを有界化する）。
fn enforce_discard_budget<S: WireStream>(
    stream: &mut S,
    discarded_bytes: usize,
    discarded_messages: usize,
) -> io::Result<()> {
    if discarded_bytes > crate::limits::COPY_DISCARD_MAX_BYTES
        || discarded_messages > crate::limits::COPY_DISCARD_MAX_MESSAGES
    {
        let _ = crate::handshake::write_error_response_io(
            stream,
            ErrorClass::ProtocolViolation,
            "COPY discard budget exceeded",
        );
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "COPY discard budget exceeded",
        ));
    }
    Ok(())
}

/// ErrorResponse を書いてから ReadyForQuery を書く
/// （`simple_query::respond_error_and_ready` と同じ契約。COPY サブプロトコル
/// のエラーも接続を維持する簡易クエリの一部であり、切断はしない）。
/// `ReadyForQuery` の状態バイトは常に `Idle` を渡す ―― `handshake::
/// post_auth_loop` は明示トランザクションが `Idle` の場合に限り本モジュール
/// （`crate::copy::run`）へ委譲するため（SQL-31・TASK-221。`Active`／`Failed`
/// 中の COPY は本モジュールへ到達する前に `0A000`／`25P02` で拒否される）。
fn respond_error_and_ready<S: WireStream>(
    stream: &mut S,
    class: ErrorClass,
    message: &str,
) -> io::Result<()> {
    crate::handshake::write_error_response_io(stream, class, message)?;
    crate::handshake::write_ready_for_query_io(
        stream,
        engine::sql::transaction::TransactionStatus::Idle,
    )
}

fn respond_sql_error<S: WireStream>(stream: &mut S, e: &SqlSurfaceError) -> io::Result<()> {
    respond_error_and_ready(stream, e.error_class(), &e.client_message())
}

/// `CopyInResponse`（'G'）／`CopyOutResponse`（'H'）を組み立てる。いずれも
/// `Int8 format=0`（text。CSV でも overall format は 0 のまま）・
/// `Int16 num_columns`・`Int16[num_columns]`（各列 format=0）という同一構造
/// （PostgreSQL wire v3 の規範）。
fn encode_copy_response(kind: u8, column_count: usize) -> Result<Vec<u8>, ()> {
    let n = i16::try_from(column_count).map_err(|_| ())?;
    let mut body = Vec::new();
    body.push(0u8);
    body.extend_from_slice(&n.to_be_bytes());
    for _ in 0..column_count {
        body.extend_from_slice(&0i16.to_be_bytes());
    }
    let total_len = body
        .len()
        .checked_add(4)
        .and_then(|v| i32::try_from(v).ok())
        .ok_or(())?;
    let mut msg = Vec::with_capacity(1 + body.len() + 4);
    msg.push(kind);
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&body);
    Ok(msg)
}

/// `CopyDone`（'c'）。body を持たない固定 5 バイト。
fn encode_copy_done() -> [u8; 5] {
    let mut msg = [0u8; 5];
    msg[0] = b'c';
    msg[1..5].copy_from_slice(&4i32.to_be_bytes());
    msg
}

/// `Cell` を COPY の値表現（text／CSV）へエンコードする。`result_encoder::
/// cell_to_text`（通常の SELECT 応答と同じ値表現）を土台にすることで、
/// `COPY (...) TO STDOUT` の出力を同じテーブルへ `COPY ... FROM STDIN` で
/// 再投入した際に値が往復する（`sql::copy::decode_text_field`／
/// `decode_csv_record` と対称なエスケープ規則）。
fn cell_to_copy_value(
    format: CopyFormat,
    cell: &Cell,
) -> Result<Option<String>, result_encoder::EncodeError> {
    let text = result_encoder::cell_to_text(cell)?;
    Ok(text.map(|t| match format {
        CopyFormat::Text => escape_copy_text(&t),
        CopyFormat::Csv => escape_copy_csv(&t),
    }))
}

fn escape_copy_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\u{8}' => out.push_str("\\b"),
            '\u{C}' => out.push_str("\\f"),
            '\u{B}' => out.push_str("\\v"),
            other => out.push(other),
        }
    }
    out
}

fn escape_copy_csv(s: &str) -> String {
    let needs_quoting = s.is_empty() || s.contains([',', '"', '\n', '\r']);
    if !needs_quoting {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// `ResultRow` を `CopyData`（'d'）1 個へエンコードし `out` の末尾へ追記する
/// （`result_encoder::encode_data_row_into` と同じ in-place 追記契約。失敗時は
/// 呼び出し前の長さへ `truncate` してから返す）。COPY の値区切りは text 形式
/// タブ・CSV 形式カンマの固定（クライアント指定の区切り文字は許可リスト外。
/// `sql::allowlist::validate_copy` の `FORMAT` 以外のオプション拒否と対応）。
fn encode_copy_data_row_into(
    format: CopyFormat,
    row: &ResultRow,
    out: &mut Vec<u8>,
) -> Result<(), result_encoder::EncodeError> {
    let start = out.len();
    let mut fields: Vec<Option<String>> = Vec::with_capacity(row.cells.len());
    for cell in &row.cells {
        match cell_to_copy_value(format, cell) {
            Ok(v) => fields.push(v),
            Err(e) => {
                out.truncate(start);
                return Err(e);
            }
        }
    }
    let sep = match format {
        CopyFormat::Text => '\t',
        CopyFormat::Csv => ',',
    };
    let mut line = String::new();
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            line.push(sep);
        }
        match (format, field) {
            // text 形式の NULL 表現はリテラル `\N`（`sql::copy::decode_text_field`
            // がフィールド全体一致で NULL 扱いする対称）。CSV 形式は
            // `sql::copy::decode_csv_record`／`finish_csv_field` が「引用符なしの
            // 空フィールド」を NULL、`""` を空文字列として区別する契約のため、
            // ここで `\N` を出力すると再投入時に文字列 "\N" として読まれてしまい
            // 往復が壊れる（Issue #939 レビュー指摘）。CSV の NULL は引用符なしの
            // 空文字列で表現する。
            (CopyFormat::Text, None) => line.push_str("\\N"),
            (CopyFormat::Csv, None) => {}
            (_, Some(s)) => line.push_str(s),
        }
    }
    line.push('\n');
    let bytes = line.as_bytes();
    let len = match i32::try_from(bytes.len().saturating_add(4)) {
        Ok(v) => v,
        Err(_) => {
            out.truncate(start);
            return Err(result_encoder::EncodeError);
        }
    };
    out.push(b'd');
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(bytes);
    Ok(())
}

/// `COPY (...) TO STDOUT`: CopyOutResponse → 各行 CopyData → CopyDone →
/// `CommandComplete "COPY n"` → `ReadyForQuery`。実行本体
/// （`EngineCore::begin_copy` の `CopyPlan::To` 分岐。広域取得と同じ RLS
/// 暗黙適用・`LIMIT` 有界の走査）はこの関数の呼び出し前に完了済みであり、
/// 本関数はエンコードと送出のみを担う（実行エラーは `CopyOutResponse` より
/// 前に確定しているため、この経路には到達しない）。
fn run_copy_to<S: WireStream>(
    stream: &mut S,
    format: CopyFormat,
    result: &QueryResult,
) -> io::Result<()> {
    let response = match encode_copy_response(b'H', result.columns.len()) {
        Ok(b) => b,
        Err(()) => {
            return respond_error_and_ready(
                stream,
                ErrorClass::InternalError,
                "failed to encode CopyOutResponse",
            )
        }
    };

    let hint = response
        .len()
        .saturating_add(result.rows.len().saturating_mul(64));
    let mut buffer = crate::response_buffer::ResponseBuffer::with_capacity_hint(
        crate::limits::MAX_RESPONSE_BUFFER_BYTES,
        hint,
    );
    buffer.push_frame(stream, &response)?;

    for row in &result.rows {
        let start = buffer.frame_start();
        if encode_copy_data_row_into(format, row, buffer.as_mut_vec()).is_err() {
            buffer.truncate_to(start);
            buffer.flush(stream)?;
            return respond_error_and_ready(
                stream,
                ErrorClass::InternalError,
                "failed to encode CopyData",
            );
        }
        if buffer.len() >= crate::limits::MAX_RESPONSE_BUFFER_BYTES {
            buffer.flush(stream)?;
        }
    }

    buffer.push_frame(stream, &encode_copy_done())?;
    let tag = format!("COPY {}", result.rows.len());
    match result_encoder::encode_command_complete(&tag) {
        Ok(msg) => {
            buffer.push_frame(stream, &msg)?;
            buffer.push_frame(
                stream,
                &result_encoder::encode_ready_for_query(
                    engine::sql::transaction::TransactionStatus::Idle,
                ),
            )?;
            buffer.flush(stream)
        }
        Err(_) => {
            buffer.flush(stream)?;
            respond_error_and_ready(
                stream,
                ErrorClass::InternalError,
                "failed to encode command complete response",
            )
        }
    }
}

/// `COPY <table> (<cols>) FROM STDIN`: CopyInResponse → CopyData*／CopyDone／
/// CopyFail のサブプロトコルを 1 回の呼び出しで完結させる。行のデコード・
/// 束縛・INDEX-4 逐次判定は [`CopyInSession::feed`] が担う（`sql::copy`
/// モジュールドキュメント参照）。
///
/// エラー処理は本モジュールドキュメントに記載の簡略化方針に従う: `feed` が
/// 失敗した時点では応答を送らず「以降の CopyData を読み捨てる」状態へ移り、
/// CopyDone／CopyFail を受信してからまとめて ErrorResponse＋ReadyForQuery を
/// 送る（副作用は一切残さない——commit は CopyDone 到達かつエラー無しの場合
/// のみ行う）。
///
/// 戻り値は `handshake::post_auth_loop` へそのまま返す
/// [`crate::extended_query::LoopSignal`]（Issue #939 レビュー指摘・
/// discussion_r4096720859: 以前は常に `Ok(())` を返しており、COPY 中に
/// Terminate（'X'）を受信した事実が呼び出し元へ伝播せず、`post_auth_loop`
/// が通常のクエリループへ戻ってクライアントが実際にソケットを閉じるまで
/// 接続スロットを保持し続けていた。`Closed` はループを終了させるべき
/// 場合——Terminate 受信・ストリーム側の早期 EOF——にのみ返す）。
fn run_copy_from<S: WireStream>(
    stream: &mut S,
    engine: &EngineCore,
    ctx: &PolicyContext,
    mut session: CopyInSession,
) -> io::Result<crate::extended_query::LoopSignal> {
    let response = match encode_copy_response(b'G', session.column_count()) {
        Ok(b) => b,
        Err(()) => {
            return respond_error_and_ready(
                stream,
                ErrorClass::InternalError,
                "failed to encode CopyInResponse",
            )
            .map(|()| crate::extended_query::LoopSignal::Continue)
        }
    };
    stream.write_all(&response)?;

    let mut errored: Option<SqlSurfaceError> = None;
    let mut discarded_bytes: usize = 0;
    let mut discarded_messages: usize = 0;

    loop {
        let type_byte = match framing::read_typed_frame_header(stream) {
            Ok(Some(b)) => b,
            Ok(None) => return Ok(crate::extended_query::LoopSignal::Closed),
            Err(e) => return Err(respond_frame_error_and_terminate(stream, e)),
        };
        match type_byte {
            b'd' => {
                let body = framing::read_length_prefixed_body(stream, 4, framing::MAX_MESSAGE_LEN)
                    .map_err(|e| respond_frame_error_and_terminate(stream, e))?;
                if errored.is_some() {
                    // body が空（宣言長 4）の CopyData を連送しても予算を
                    // 消費できてしまわないよう、フレームヘッダー分の固定
                    // オーバーヘッドも必ず加算する（codex-review 指摘・
                    // `limits::COPY_DISCARD_FRAME_OVERHEAD_BYTES` 参照）。
                    discarded_bytes = discarded_bytes
                        .saturating_add(body.len())
                        .saturating_add(crate::limits::COPY_DISCARD_FRAME_OVERHEAD_BYTES);
                    discarded_messages = discarded_messages.saturating_add(1);
                    enforce_discard_budget(stream, discarded_bytes, discarded_messages)?;
                } else if body.is_empty() {
                    // codex-review P0 指摘（PRRT_kwDOUAKASM6luiS5）: `errored`
                    // が `None` の間（＝まだ正常に取り込み処理中）は
                    // `session.feed` が実際の進捗（`running_bytes` の増加・
                    // 行確定）を伴わない限りどの上限も加算しないため、body が
                    // 空（宣言長 4）の CopyData を連送すると `feed` を無限に
                    // 呼び続けても `COPY_DISCARD_MAX_BYTES`／
                    // `COPY_DISCARD_MAX_MESSAGES` のいずれも消費できず接続
                    // スロットを無期限に占有できてしまう。`feed` へ渡しても
                    // 意味のある処理（0 バイト分の `check_running_total` は
                    // 何も変えない）が起きないことは判明しているため、
                    // エラー後の読み捨てと同じ discard 予算へこの空フレーム
                    // 自体を数え、進捗ゼロの空フレーム連送を有界化する
                    // （`feed` は呼ばない——空 body を渡しても no-op のため
                    // 呼んでも呼ばなくても取り込み結果は変わらない）。
                    discarded_bytes = discarded_bytes
                        .saturating_add(crate::limits::COPY_DISCARD_FRAME_OVERHEAD_BYTES);
                    discarded_messages = discarded_messages.saturating_add(1);
                    enforce_discard_budget(stream, discarded_bytes, discarded_messages)?;
                } else if let Err(e) = session.feed(&body) {
                    errored = Some(e);
                }
            }
            b'c' => {
                framing::read_length_prefixed_body(stream, 4, 4)
                    .map_err(|e| respond_frame_error_and_terminate(stream, e))?;
                return finish_copy_from(stream, engine, ctx, session, errored)
                    .map(|()| crate::extended_query::LoopSignal::Continue);
            }
            b'f' => {
                let body = framing::read_length_prefixed_body(
                    stream,
                    framing::MIN_TYPED_MESSAGE_LEN,
                    framing::MAX_MESSAGE_LEN,
                )
                .map_err(|e| respond_frame_error_and_terminate(stream, e))?;
                // CopyFail の理由文字列は PostgreSQL wire v3 の `String` 型
                // （末尾 NUL 終端・埋め込み NUL 不可の C 文字列）として送られる
                // 契約である。旧実装は宣言長のみ検証し本文を無条件に読み捨てて
                // いたため、長さ 4（本文なし）や NUL 終端を欠く本文も正常な
                // CopyFail として受理し接続を維持していた（codex-review 指摘・
                // PRRT_kwDOUAKASM6lugH7）。ここでは構造（末尾が単一の NUL で
                // 終わり、それより前に NUL を含まない）だけを検査し、違反時は
                // 他の frame エラー経路と同じ `08P01`
                // （[`respond_frame_error_and_terminate`] と同型の応答）で
                // 拒否して接続を終了する（fail-closed。宣言長の時点で本文が
                // 空の可能性を許していた WIRE-10 の固定形状メッセージ判定とは
                // 別に、`String` 型の中身の形状はここでしか検査できない）。
                // 文字列の中身自体は依然として応答・ログへ一切エコーしない
                // （security.md「エラー・ログ経由で他テナントのデータ・存在
                // 情報を漏らさない」。解釈するのは NUL 終端という構造のみ）。
                let is_valid_c_string =
                    matches!(body.split_last(), Some((0, rest)) if !rest.contains(&0));
                if !is_valid_c_string {
                    let _ = crate::handshake::write_error_response_io(
                        stream,
                        ErrorClass::ProtocolViolation,
                        "malformed CopyFail message",
                    );
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "malformed CopyFail message",
                    ));
                }
                return respond_error_and_ready(
                    stream,
                    ErrorClass::InvalidInput,
                    "COPY failed on the client side",
                )
                .map(|()| crate::extended_query::LoopSignal::Continue);
            }
            b'H' | b'S' => {
                // Flush('H')／Sync('S') は PostgreSQL wire v3 上 length=4
                // （body 厳密に空）以外を持たない固定形状のメッセージである。
                // 旧実装は `validate_typed_message_length_prefix` で最大長
                // のみを検証し、宣言長が 4 バイトを超える場合は任意の本文を
                // 読み捨てて正常受理していたため、`framing.rs`（WIRE-10）が
                // 他の固定形状メッセージ（CopyDone・Terminate）に課している
                // 「本文付きは `08P01` で fail-closed に拒否」という契約から
                // この 2 種類だけ逸脱していた（codex-review 指摘・
                // discussion PRRT_kwDOUAKASM6ltrHb）。`c`／`X` 分岐と同じ
                // `read_length_prefixed_body(stream, 4, 4)` を使い、本文付き
                // メッセージは `respond_frame_error_and_terminate` により
                // `Malformed`（`08P01`）として拒否し接続を終了する。
                framing::read_length_prefixed_body(stream, 4, 4)
                    .map_err(|e| respond_frame_error_and_terminate(stream, e))?;
                // 上記検証を通過した時点で body は必ず空だが、フレーム
                // ヘッダー分の固定オーバーヘッドは `d` の読み捨てと同じ
                // `discarded_bytes` 予算へ加算する（Cursor Bugbot 指摘。
                // 以前は本文長のみを数えていたため、body が空のフレームを
                // 連送すると予算を一切消費できず DoS 上限を無制限に回避
                // できた。`limits::COPY_DISCARD_FRAME_OVERHEAD_BYTES` 参照）。
                discarded_bytes = discarded_bytes
                    .saturating_add(crate::limits::COPY_DISCARD_FRAME_OVERHEAD_BYTES);
                discarded_messages = discarded_messages.saturating_add(1);
                enforce_discard_budget(stream, discarded_bytes, discarded_messages)?;
            }
            b'X' => {
                // Terminate は `handshake::post_auth_loop` の通常の 'X' 分岐
                // （`read_length_prefixed_body(stream, 4, 4)`）と対称に、長さ
                // フィールド（body 厳密に空・4 バイト固定）を確実に消費してから
                // 抜ける。読み捨てないと `post_auth_loop` がこの 4 バイトを
                // 次のメッセージ種別バイトとして誤読する（Issue #939 レビュー
                // 指摘）。`Closed` を返し、接続を終了すべきことを呼び出し元へ
                // 伝播する（上記関数ドキュメント参照）。
                framing::read_length_prefixed_body(stream, 4, 4)
                    .map_err(|e| respond_frame_error_and_terminate(stream, e))?;
                return Ok(crate::extended_query::LoopSignal::Closed);
            }
            _ => {
                let _ = framing::validate_typed_message_length_prefix(
                    stream,
                    framing::MIN_TYPED_MESSAGE_LEN,
                    framing::MAX_MESSAGE_LEN,
                );
                let _ = crate::handshake::write_error_response_io(
                    stream,
                    ErrorClass::ProtocolViolation,
                    "unexpected message type during COPY FROM STDIN",
                );
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "protocol violation during COPY FROM STDIN",
                ));
            }
        }
    }
}

/// CopyDone 到達後（`errored` が `None` の場合のみ）、[`CopyInSession::finish`]
/// で末尾レコードを確定させてから [`EngineCore::commit_copy_in`] を呼ぶ。
/// commit 前後の RECOVER-5 (3)／RECOVER-6 の保護区間は
/// `simple_query::execute_and_respond` と同じ設計（`ResponseBoundaryGuard` は
/// 呼び出し元 [`run`] が関数全体を覆い、`EmergencyResponseRegistration` は
/// commit 呼び出しだけをブロックスコープで覆う）。
fn finish_copy_from<S: WireStream>(
    stream: &mut S,
    engine: &EngineCore,
    ctx: &PolicyContext,
    session: CopyInSession,
    errored: Option<SqlSurfaceError>,
) -> io::Result<()> {
    if let Some(e) = errored {
        return respond_sql_error(stream, &e);
    }

    let batch = match session.finish() {
        Ok(b) => b,
        Err(e) => return respond_sql_error(stream, &e),
    };

    let outcome = {
        let _emergency_registration = crate::simple_query::register_emergency_response(stream);
        engine.commit_copy_in(ctx, batch)
    };

    match outcome {
        Ok(insert_outcome) => {
            match result_encoder::encode_command_complete(&format!(
                "COPY {}",
                insert_outcome.rows_affected
            )) {
                Ok(msg) => {
                    stream.write_all(&msg)?;
                    crate::handshake::write_ready_for_query_io(
                        stream,
                        engine::sql::transaction::TransactionStatus::Idle,
                    )
                }
                Err(_) => respond_error_and_ready(
                    stream,
                    ErrorClass::InternalError,
                    "failed to encode command complete response",
                ),
            }
        }
        Err(e) => respond_sql_error(stream, &e),
    }
}

/// `handshake::post_auth_loop` の 'Q' 分岐から、`engine::sql::copy::
/// is_copy_statement(text)` が真の場合にのみ呼ばれる唯一の入口。
/// `EngineCore::begin_copy` の構造検証・`operation_id` 必須化ガード・
/// テーブル解決がここで失敗した場合は CopyIn／CopyOutResponse を一切送らずに
/// 通常の ErrorResponse＋ReadyForQuery を返す（PostgreSQL 互換: CopyIn/Out
/// サブプロトコルへ入ってしまってからの構文エラーは無い）。
///
/// 戻り値は `handshake::post_auth_loop` の 'Q' 分岐が Parse／Describe
/// （'P'／'D'）と同じ作法で判定する [`crate::extended_query::LoopSignal`]。
/// `run_copy_from` が Terminate（'X'）受信を `Closed` として返してきた場合、
/// 呼び出し元はここで新たに応答を送らずそのまま伝播し、接続ループを
/// 終了させる（[`run_copy_from`] のドキュメント参照）。
pub(crate) fn run<S: WireStream>(
    stream: &mut S,
    engine: &EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
    sql: &str,
) -> io::Result<crate::extended_query::LoopSignal> {
    // commit 成功から本関数が応答を書き終えるまでの区間全体を覆う RAII ガード
    // （RECOVER-5 (3)。`simple_query::execute_and_respond` と同じ設計）。
    let _response_boundary = engine::recovery::commit_boundary::ResponseBoundaryGuard::new();

    match engine.begin_copy(ctx, session, sql) {
        Ok(CopyPlan::To(format, result)) => run_copy_to(stream, format, &result)
            .map(|()| crate::extended_query::LoopSignal::Continue),
        Ok(CopyPlan::From(copy_session)) => run_copy_from(stream, engine, ctx, copy_session),
        Err(e) => {
            respond_sql_error(stream, &e).map(|()| crate::extended_query::LoopSignal::Continue)
        }
    }
}

/// [`ColumnMeta`] を参照する箇所が本モジュールに存在することを型検査するための
/// マーカー（`result_encoder::cell_to_text` は `Cell` のみを取るため、
/// `ColumnMeta` は `QueryResult::columns` 経由でのみ使う。未使用 import 警告を
/// 避けるための明示的な no-op）。
#[allow(dead_code)]
fn _assert_column_meta_type(_c: &ColumnMeta) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_copy_text_escapes_control_characters() {
        assert_eq!(escape_copy_text("a\tb\nc\\d"), "a\\tb\\nc\\\\d");
    }

    #[test]
    fn escape_copy_csv_quotes_when_needed() {
        assert_eq!(escape_copy_csv("plain"), "plain");
        assert_eq!(escape_copy_csv(""), "\"\"");
        assert_eq!(escape_copy_csv("a,b"), "\"a,b\"");
        assert_eq!(escape_copy_csv("a\"b"), "\"a\"\"b\"");
    }

    #[test]
    fn encode_copy_response_has_expected_layout_for_two_columns() {
        let msg = encode_copy_response(b'G', 2).expect("encode");
        assert_eq!(msg[0], b'G');
        let len = i32::from_be_bytes([msg[1], msg[2], msg[3], msg[4]]) as usize;
        assert_eq!(len, msg.len() - 1);
        assert_eq!(msg[5], 0); // overall format
        let ncols = i16::from_be_bytes([msg[6], msg[7]]);
        assert_eq!(ncols, 2);
    }

    #[test]
    fn encode_copy_done_is_exactly_five_bytes() {
        let msg = encode_copy_done();
        assert_eq!(msg, [b'c', 0, 0, 0, 4]);
    }
}
