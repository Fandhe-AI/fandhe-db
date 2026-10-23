//! COPY プロトコル（`COPY ... FROM STDIN`／`COPY (...) TO STDOUT`）のメッセージ層
//! （Issue #939・WIRE-17・TASK-220）。
//!
//! 責務境界: 本モジュールは CopyIn／CopyOut サブプロトコルのフレーミング・
//! 状態遷移のみを担う。行のレコード分割・フィールドデコード・束縛・INDEX-4
//! 上限判定・commit はすべて `engine::sql::copy`（`EngineCore::begin_copy`／
//! `commit_copy_in`）へ委譲する（第 2 の書き込み経路を作らない設計）。
//!
//! 呼び出し文脈（拡張クエリ）: `extended_query::handle_execute` が COPY の portal を
//! `simple_query::run_copy_statement`（`Finish::Continue`）経由で本モジュールへ委譲する
//! （Issue #1760・WIRE-21 (2)）。拡張クエリでは `ReadyForQuery` を Sync が送るため、
//! 本モジュールは `CommandComplete` または ErrorResponse だけを書く。失敗後の
//! 回復（Sync まで後続メッセージを読み捨てて ReadyForQuery を返す）は
//! `extended_query`／`handshake::post_auth_loop` の同期回復が担う（Issue #1761）。
//!
//! 呼び出し文脈（簡易クエリ）: `simple_query::run_copy_statement` が、分割済みの 1 文へ
//! `engine::sql::copy::is_copy_statement` を適用して真なら（セッションが `Idle` の
//! ときに）ここへ委譲する（Issue #1175: 複数文メッセージの 2 文目以降の COPY も
//! 文単位で扱う）。本モジュールは COPY サブプロトコルの開始（CopyInResponse／
//! CopyOutResponse）から終了（`CommandComplete`。`Finish::ReadyForQuery` のときは
//! `ReadyForQuery` も）までを 1 回の呼び出しで完結させる
//! （`simple_query::run_statement` と同じ「1 文の応答を書き切る」契約）。
//!
//! binary 形式（`COPY ... FROM STDIN WITH (FORMAT binary)`。WIRE-23・Issue #1769・親 #1766）:
//! `run_copy_from` が [`BinaryCopyReader`]（フレーム解析。型非依存）で CopyData をタプルへ分解し、
//! 列型ごとのコーデック（[`copy_binary_types`]。Issue #1768）で各フィールドを正規テキストへ変換して
//! `CopyInSession::feed_fields` へ渡す。以降の束縛・INDEX-4 上限・commit は text/csv と同一の経路で、
//! 第 2 の書き込み経路は無い。非対応の列型は `CopyInResponse` を送る前に `0A000` で拒否する。
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
use std::time::{Duration, Instant};

use engine::core::{CopyPlan, EngineCore};
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::policy::PolicyContext;
use engine::sql::allowlist::{CopyFormat, SqlSurfaceError};
use engine::sql::copy::{CopyColumnKind, CopyInSession};
use engine::sql::copy_options::{CopyHeader, CopyOptions};
use engine::sql::exec::{Cell, ColumnMeta, QueryResult, ResultRow};
use engine::sql::mode::SessionState;
use engine::sql::transaction::SessionTransaction;

use crate::copy_binary::{BinaryCopyReader, BinaryFrameLimits, CopyBinaryError};
use crate::copy_binary_types::{self, CopyBinaryColumn, CopyBinaryValueError};
use crate::framing::{self, FrameError};
use crate::result_encoder;
use crate::simple_query::{Finish, StatementRole};
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
pub(crate) fn enforce_discard_budget<S: WireStream>(
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

/// ErrorResponse だけを書く（`ReadyForQuery` は `CopyStep::Failed` を受けた
/// `simple_query` が書く。`simple_query::respond_error` と同じ契約。COPY サブプロトコル
/// のエラーも接続を維持する簡易クエリの一部であり、切断はしない）。
/// 後続の `ReadyForQuery` の状態バイトは接続の明示トランザクション状態（`txn.status()`）から
/// 導出する（SQL-31・TASK-221、Issue #1179。`handshake::post_auth_loop` は `Idle`・
/// `Active` のとき本モジュール〔`crate::copy::run`〕へ委譲し、`Failed` 中の COPY は
/// 本モジュールへ到達する前に `25P02` で拒否する）。
///
/// 明示トランザクション中に COPY が失敗した場合は、PostgreSQL と同じく
/// トランザクションを `Failed`（ReadyForQuery `'E'`）へ遷移させてから応答する
/// （`txn.fail()` は `Active` 以外では何もしないため、トランザクション外の挙動は不変）。
fn respond_error_and_ready<S: WireStream>(
    stream: &mut S,
    txn: &mut SessionTransaction<'_>,
    class: ErrorClass,
    message: &str,
) -> io::Result<()> {
    txn.fail();
    // `ReadyForQuery` はここでは書かない。`CopyStep::Failed` を受けた呼び出し元
    // （`simple_query`）がセッション復元の後に TimeZone 通知付きで送る（TABLE-33・Issue #1575）。
    crate::handshake::write_error_response_io(stream, class, message)
}

fn respond_sql_error<S: WireStream>(
    stream: &mut S,
    txn: &mut SessionTransaction<'_>,
    e: &SqlSurfaceError,
) -> io::Result<()> {
    respond_error_and_ready(stream, txn, e.error_class(), &e.client_message())
}

/// `COPY FROM STDIN` の取り込み中に保持するエラー（`errored`）。engine の SQL 表層エラーに加え、
/// binary 形式で wire 側に生じるフレーム不正（[`CopyBinaryError`]）と値の変換失敗を表す。
/// 文言は固定の英語文字列で、受信した値・他テナントの情報を含めない（WIRE-23・Issue #1769）。
#[derive(Debug)]
enum CopyFromError {
    Sql(SqlSurfaceError),
    Frame(CopyBinaryError),
    /// binary フィールド 1 件の変換失敗・非対応列型（分類と固定文言は [`CopyBinaryValueError`] が持つ）。
    Value(CopyBinaryValueError),
}

impl CopyFromError {
    fn error_class(&self) -> ErrorClass {
        match self {
            Self::Sql(e) => e.error_class(),
            Self::Frame(e) => e.error_class(),
            Self::Value(e) => e.error_class(),
        }
    }

    fn client_message(&self) -> String {
        match self {
            Self::Sql(e) => e.client_message(),
            Self::Frame(e) => e.client_message().to_string(),
            Self::Value(e) => e.client_message(),
        }
    }
}

impl From<SqlSurfaceError> for CopyFromError {
    fn from(e: SqlSurfaceError) -> Self {
        Self::Sql(e)
    }
}

impl From<CopyBinaryError> for CopyFromError {
    fn from(e: CopyBinaryError) -> Self {
        Self::Frame(e)
    }
}

/// `COPY FROM STDIN` の CopyData の解釈方式。text/csv は engine の分割器へ生バイトを渡し、
/// binary は [`BinaryCopyReader`] でタプルへ分解して列型ごとに正規テキストへ変換する。
enum CopyInDecoder {
    Rows,
    Binary {
        reader: BinaryCopyReader,
        /// COPY 列リストの順のコーデック（非対応列は解決時に拒否済み）。
        columns: Vec<CopyBinaryColumn>,
    },
}

/// 1 個の CopyData 本文を取り込む。③（生バイト累計）は text/csv では `CopyInSession::feed` が、
/// binary では本関数がフレーム解析より前に判定する。
fn feed_copy_data(
    decoder: &mut CopyInDecoder,
    session: &mut CopyInSession,
    body: &[u8],
) -> Result<(), CopyFromError> {
    match decoder {
        CopyInDecoder::Rows => session.feed(body).map_err(CopyFromError::Sql),
        CopyInDecoder::Binary { reader, columns } => {
            session.account_raw_bytes(body.len())?;
            reader.feed(body, |raw| {
                let fields =
                    copy_binary_types::decode_tuple(columns, raw).map_err(CopyFromError::Value)?;
                session.feed_fields(fields).map_err(CopyFromError::Sql)
            })
        }
    }
}

fn respond_copy_from_error<S: WireStream>(
    stream: &mut S,
    txn: &mut SessionTransaction<'_>,
    e: &CopyFromError,
) -> io::Result<()> {
    respond_error_and_ready(stream, txn, e.error_class(), &e.client_message())
}

/// `CopyInResponse`（'G'）／`CopyOutResponse`（'H'）を組み立てる。いずれも
/// `Int8 format`（text。CSV でも overall format は 0 のまま。`binary` のとき 1）・
/// `Int16 num_columns`・`Int16[num_columns]`（各列 format は overall と同じ）という同一構造
/// （PostgreSQL wire v3 の規範）。
fn encode_copy_response(kind: u8, column_count: usize, binary: bool) -> Result<Vec<u8>, ()> {
    let n = i16::try_from(column_count).map_err(|_| ())?;
    // binary（WIRE-23・Issue #1770）は全体・列ごとともに format=1。text／csv は 0。
    let format: i16 = if binary { 1 } else { 0 };
    let mut body = Vec::new();
    body.push(format as u8);
    body.extend_from_slice(&n.to_be_bytes());
    for _ in 0..column_count {
        body.extend_from_slice(&format.to_be_bytes());
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
/// cell_to_text_for_column_in_tz`（通常の SELECT 応答と同じ値表現。列の公告型に応じた
/// `REAL` の PostgreSQL 形式を含む。Issue #1173）を土台にすることで、
/// `COPY (...) TO STDOUT` の出力を同じテーブルへ `COPY ... FROM STDIN` で
/// 再投入した際に値が往復する（`sql::copy::decode_text_field`／
/// `decode_csv_record` と対称なエスケープ規則）。区切り・引用・ESCAPE は
/// `options`（`copy_options::resolve` で検証済み。Issue #1765・WIRE-22）から決める。
fn cell_to_copy_value(
    format: CopyFormat,
    meta: &ColumnMeta,
    cell: &Cell,
    tz: &engine::datetime::SessionTimeZone,
    options: &CopyOptions,
    single_column: bool,
) -> Result<Option<String>, result_encoder::EncodeError> {
    // COPY TO の binary は #1770 が実装する。engine が TO の binary を拒否するため到達しないが、
    // 到達した場合は text として出力せず fail-closed に拒否する。
    if format == CopyFormat::Binary {
        return Err(result_encoder::EncodeError);
    }
    let text = result_encoder::cell_to_text_for_column_in_tz(meta, cell, tz)?;
    Ok(text.map(|t| escape_copy_value(format, &t, options, single_column)))
}

/// 値 1 個を `format` の規則でエスケープ（CSV は必要時のみ引用）する。データ行と
/// ヘッダ行（列名）が同じ規則を通るよう共通化している。
fn escape_copy_value(
    format: CopyFormat,
    s: &str,
    options: &CopyOptions,
    single_column: bool,
) -> String {
    match format {
        CopyFormat::Text => escape_copy_text(s, options.delimiter()),
        CopyFormat::Csv => escape_copy_csv(s, options, single_column),
        // binary は専用経路（encode_binary_copy_data_into）を通る。text 系へ届いても
        // 無変換で返すだけで、呼び出し側が Binary を拒否する。
        CopyFormat::Binary => s.to_string(),
    }
}

/// text 形式のエスケープ（PostgreSQL の `CopyAttributeOutText` 相当）。
/// `delim` は検証済みの ASCII 1 バイト。NULL 表現はここを通さず生のまま出す。
fn escape_copy_text(s: &str, delim: u8) -> String {
    let delim = char::from(delim);
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{8}' => out.push_str("\\b"),
            '\u{C}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{B}' => out.push_str("\\v"),
            '\\' => out.push_str("\\\\"),
            c if c == delim => {
                out.push('\\');
                out.push(c);
            }
            other => out.push(other),
        }
    }
    out
}

/// CSV 形式のエスケープ（PostgreSQL の `CopyAttributeOutCSV` 相当）。NULL 表現と
/// 一致する値・区切り／QUOTE／改行を含む値・単一列の `\.` だけを引用し、引用内では
/// QUOTE と ESCAPE の前に ESCAPE を付ける（ESCAPE == QUOTE なら二重化）。
fn escape_copy_csv(s: &str, options: &CopyOptions, single_column: bool) -> String {
    let delim = char::from(options.delimiter());
    let quote = char::from(options.quote());
    let escape = char::from(options.escape());
    let needs_quoting = s == options.null_string()
        || s.contains([delim, quote, '\n', '\r'])
        || (single_column && s == "\\.");
    if !needs_quoting {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        if c == quote || c == escape {
            out.push(escape);
        }
        out.push(c);
    }
    out.push(quote);
    out
}

/// 組み立て済みの 1 行（改行込み）を `CopyData`（'d'）1 個として `out` の末尾へ追記する。
/// 失敗時は `start`（呼び出し前の長さ）へ `truncate` する。
fn push_copy_data_line(
    line: &str,
    out: &mut Vec<u8>,
    start: usize,
) -> Result<(), result_encoder::EncodeError> {
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

/// binary COPY の 1 行を `CopyData`（'d'）1 個として `out` の末尾へ追記する
/// （WIRE-23・Issue #1770）。`first` が真なら本文の先頭にストリームヘッダを置く
/// （PostgreSQL と同じくヘッダは最初の行と同じ CopyData）。tuple は
/// `Int16 フィールド数` + 各フィールド（`result_encoder::encode_binary_field_into`）で、
/// `copy_binary::encode_tuple_into` と同じバイト列。失敗時は `out` を呼び出し前の長さへ戻す。
fn encode_binary_copy_data_into(
    columns: &[ColumnMeta],
    row: &ResultRow,
    first: bool,
    out: &mut Vec<u8>,
) -> Result<(), result_encoder::EncodeError> {
    if columns.len() != row.cells.len() {
        return Err(result_encoder::EncodeError);
    }
    let start = out.len();
    let r = (|| {
        let count = i16::try_from(row.cells.len()).map_err(|_| result_encoder::EncodeError)?;
        out.push(b'd');
        out.extend_from_slice(&0i32.to_be_bytes());
        let body_start = out.len();
        if first {
            crate::copy_binary::encode_header_into(out);
        }
        out.extend_from_slice(&count.to_be_bytes());
        for (meta, cell) in columns.iter().zip(&row.cells) {
            result_encoder::encode_binary_field_into(meta, cell, out)?;
        }
        patch_copy_data_len(out, body_start)
    })();
    if r.is_err() {
        out.truncate(start);
    }
    r
}

/// `body_start` 直前の 4 バイト長プレースホルダへ、そこから末尾までの本文長を書き戻す。
fn patch_copy_data_len(
    out: &mut [u8],
    body_start: usize,
) -> Result<(), result_encoder::EncodeError> {
    let body_len = out
        .len()
        .checked_sub(body_start)
        .ok_or(result_encoder::EncodeError)?;
    let total = result_encoder::frame_len(body_len)?;
    let pos = body_start
        .checked_sub(4)
        .ok_or(result_encoder::EncodeError)?;
    let slot = out
        .get_mut(pos..body_start)
        .ok_or(result_encoder::EncodeError)?;
    slot.copy_from_slice(&total.to_be_bytes());
    Ok(())
}

/// binary COPY の終端 CopyData（トレーラ。0 行のときはヘッダ付き）を組み立てる。
fn encode_binary_copy_trailer(with_header: bool) -> Vec<u8> {
    let mut body = Vec::new();
    if with_header {
        crate::copy_binary::encode_header_into(&mut body);
    }
    crate::copy_binary::encode_trailer_into(&mut body);
    let mut msg = Vec::with_capacity(body.len() + 5);
    msg.push(b'd');
    // 本文は高々 21 バイトのため i32 に必ず収まる。
    msg.extend_from_slice(&i32::try_from(body.len() + 4).unwrap_or(4).to_be_bytes());
    msg.extend_from_slice(&body);
    msg
}

/// `ResultRow` を `CopyData`（'d'）1 個へエンコードし `out` の末尾へ追記する
/// （`result_encoder::encode_data_row_into` と同じ in-place 追記契約。失敗時は
/// 呼び出し前の長さへ `truncate` してから返す）。区切り文字・NULL 表現・引用規則は
/// 検証済みの `options` に従う（Issue #1765・WIRE-22）。
fn encode_copy_data_row_into(
    format: CopyFormat,
    columns: &[ColumnMeta],
    row: &ResultRow,
    out: &mut Vec<u8>,
    tz: &engine::datetime::SessionTimeZone,
    options: &CopyOptions,
) -> Result<(), result_encoder::EncodeError> {
    // 列メタとセル数の不一致は内部不整合（fail-closed。何も書かずに拒否する）。
    // binary はこの text 系関数の対象外（text へ黙って落とさない）。
    if columns.len() != row.cells.len() || format == CopyFormat::Binary {
        return Err(result_encoder::EncodeError);
    }
    let start = out.len();
    let single_column = columns.len() == 1;
    let sep = char::from(options.delimiter());
    let mut line = String::new();
    for (i, (meta, cell)) in columns.iter().zip(&row.cells).enumerate() {
        if i > 0 {
            line.push(sep);
        }
        match cell_to_copy_value(format, meta, cell, tz, options, single_column) {
            Ok(Some(s)) => line.push_str(&s),
            // NULL 表現は `options.null_string()` を生のまま出す（text の既定 `\N`、
            // CSV の既定は引用符なしの空フィールド。`sql::copy` のデコーダと対称）。
            Ok(None) => line.push_str(options.null_string()),
            Err(e) => {
                out.truncate(start);
                return Err(e);
            }
        }
    }
    line.push('\n');
    push_copy_data_line(&line, out, start)
}

/// `HEADER` 指定時の列名行を `CopyData`（'d'）1 個として `out` へ追記する。
/// 列名はデータと同じエスケープ規則を通す。この行は `COPY n` の n に含めない。
fn encode_copy_header_into(
    format: CopyFormat,
    columns: &[ColumnMeta],
    out: &mut Vec<u8>,
    options: &CopyOptions,
) -> Result<(), result_encoder::EncodeError> {
    let start = out.len();
    let single_column = columns.len() == 1;
    let sep = char::from(options.delimiter());
    let mut line = String::new();
    for (i, meta) in columns.iter().enumerate() {
        if i > 0 {
            line.push(sep);
        }
        line.push_str(&escape_copy_value(
            format,
            result_encoder::column_name(meta),
            options,
            single_column,
        ));
    }
    line.push('\n');
    push_copy_data_line(&line, out, start)
}

/// COPY TO の応答開始前に確定できる拒否条件を検査する。`run` が暗黙 commit より前に呼び、
/// 拒否時は commit せず ErrorResponse を返す（暗黙トランザクションの原子性。WIRE-20）。
/// `HEADER MATCH` は engine の `resolve` が TO で拒否済みで到達しないが、到達した場合は
/// 無言で `HEADER true` 扱いにせず fail-closed とする。binary は WIRE-14 の対応型のみで、
/// 非対応型は 0A000（列番号以外の情報を含めない）。
fn precheck_copy_to(
    format: CopyFormat,
    result: &QueryResult,
    options: &CopyOptions,
) -> Result<(), (ErrorClass, &'static str)> {
    if options.header() == CopyHeader::Match {
        return Err((ErrorClass::InternalError, "unsupported COPY header option"));
    }
    if format == CopyFormat::Binary {
        let formats = vec![result_encoder::FormatCode::Binary; result.columns.len()];
        if let Err(e) = result_encoder::validate_binary_formats(&result.columns, &formats) {
            return Err((
                e.error_class(),
                "COPY BINARY does not support the column type",
            ));
        }
    }
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
    conn_session: &SessionState,
    txn: &mut SessionTransaction<'_>,
    format: CopyFormat,
    result: &QueryResult,
    options: &CopyOptions,
    finish: Finish,
) -> io::Result<CopyStep> {
    // 応答開始前に拒否できる検証（HEADER MATCH・binary 非対応型）は `precheck_copy_to` が
    // 暗黙 commit より前に済ませている（呼び出し元 `run`）。
    let binary = format == CopyFormat::Binary;
    let response =
        match encode_copy_response(b'H', result.columns.len(), format == CopyFormat::Binary) {
            Ok(b) => b,
            Err(()) => {
                return respond_error_and_ready(
                    stream,
                    txn,
                    ErrorClass::InternalError,
                    "failed to encode CopyOutResponse",
                )
                .map(|()| CopyStep::Failed)
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

    if options.header() == CopyHeader::True {
        let start = buffer.frame_start();
        if encode_copy_header_into(format, &result.columns, buffer.as_mut_vec(), options).is_err() {
            buffer.truncate_to(start);
            buffer.flush(stream)?;
            return respond_error_and_ready(
                stream,
                txn,
                ErrorClass::InternalError,
                "failed to encode CopyData",
            )
            .map(|()| CopyStep::Failed);
        }
    }

    for (index, row) in result.rows.iter().enumerate() {
        let start = buffer.frame_start();
        let encoded = if binary {
            encode_binary_copy_data_into(&result.columns, row, index == 0, buffer.as_mut_vec())
        } else {
            encode_copy_data_row_into(
                format,
                &result.columns,
                row,
                buffer.as_mut_vec(),
                // COPY TO は往復契約（COPY FROM の `parse_numeric_offset` は時 0〜15 のみ受理）の
                // ため、セッション TimeZone に依らず UTC 固定で出力する（Issue #1575）。
                &engine::datetime::SessionTimeZone::utc(),
                options,
            )
        };
        if encoded.is_err() {
            buffer.truncate_to(start);
            buffer.flush(stream)?;
            return respond_error_and_ready(
                stream,
                txn,
                ErrorClass::InternalError,
                "failed to encode CopyData",
            )
            .map(|()| CopyStep::Failed);
        }
        if buffer.len() >= crate::limits::MAX_RESPONSE_BUFFER_BYTES {
            buffer.flush(stream)?;
        }
    }

    if binary {
        buffer.push_frame(stream, &encode_binary_copy_trailer(result.rows.is_empty()))?;
    }
    buffer.push_frame(stream, &encode_copy_done())?;
    let tag = format!("COPY {}", result.rows.len());
    match result_encoder::encode_command_complete(&tag) {
        Ok(msg) => {
            buffer.push_frame(stream, &msg)?;
            if finish == Finish::ReadyForQuery {
                buffer.push_frame(
                    stream,
                    &crate::handshake::ready_for_query_frames(conn_session, txn),
                )?;
            }
            buffer.flush(stream)?;
            Ok(CopyStep::Completed { tag })
        }
        Err(_) => {
            buffer.flush(stream)?;
            respond_error_and_ready(
                stream,
                txn,
                ErrorClass::InternalError,
                "failed to encode command complete response",
            )
            .map(|()| CopyStep::Failed)
        }
    }
}

/// セッションの形式から CopyData の解釈方式を決める。binary は `CopyInResponse` を送る前に、
/// 非対応の列型（[`copy_binary_types::resolve_columns`]）を含む列リストを `0A000` で拒否する
/// （copy mode へ入ってから失敗させない。fail-closed）。フレーム層のメモリ上限は、確保前の
/// 防御としてセッションの ③（バッチ合計バイト数）を使う（実際の上限判定は engine が担う）。
fn prepare_decoder(session: &CopyInSession) -> Result<CopyInDecoder, CopyFromError> {
    if session.format() != CopyFormat::Binary {
        return Ok(CopyInDecoder::Rows);
    }
    // engine の列種別を結果側の列メタへ写し、Bind・DataRow と同じ単一情報源で対応可否を決める。
    // 疑似列 `id` は結果側が numeric で公告するため `ColumnMeta::Id` に写る。
    let metas: Vec<ColumnMeta> = session
        .column_kinds()?
        .into_iter()
        .map(|k| match k {
            CopyColumnKind::PseudoId => ColumnMeta::Id,
            CopyColumnKind::Column(ty) => ColumnMeta::Scalar {
                name: String::new(),
                ty,
            },
        })
        .collect();
    let columns = copy_binary_types::resolve_columns(&metas).map_err(CopyFromError::Value)?;
    let max = session.batch_limits().max_batch_total_bytes;
    let reader = BinaryCopyReader::new(
        columns.len(),
        BinaryFrameLimits {
            max_field_bytes: max,
            max_tuple_bytes: max,
        },
    )?;
    Ok(CopyInDecoder::Binary { reader, columns })
}

/// COPY FROM STDIN の次フレームの型バイトを読む。トランザクションが `Active`（暗黙・明示とも
/// 単一ライタを保持中）の間は、型バイト待ちと続く長さ・本文の受信全体に「持続時間の残り
/// （と接続の読み取りタイムアウト）」までの期限を課す（SQL-31・Issue #1773 レビュー指摘。
/// `handshake::post_auth_loop` の期限は 'Q' の受信前に決まるため COPY のフレームには及ばない）。
/// 期限超過は `TimedOut` の I/O エラーを返し、接続は応答なしで閉じられてライタが解放される。
/// 戻り値のガードは呼び出し元が 1 フレームの受信が済むまで保持する。`Active` でなければ従来どおり。
fn read_copy_frame_header<S: WireStream>(
    stream: &mut S,
    txn: &SessionTransaction<'_>,
    base_timeout: Option<Duration>,
) -> io::Result<(Option<u8>, Option<framing::FrameDeadlineGuard>)> {
    let Some(remaining) = txn.remaining_duration() else {
        return match framing::read_typed_frame_header(stream) {
            Ok(h) => Ok((h, None)),
            Err(e) => Err(respond_frame_error_and_terminate(stream, e)),
        };
    };
    let budget = base_timeout.map_or(remaining, |b| remaining.min(b));
    let poll = budget
        .min(crate::handshake::FRAME_DEADLINE_POLL)
        .max(crate::handshake::MIN_BOUNDED_READ_WAIT);
    stream.set_read_timeout(Some(poll))?;
    let deadline = Instant::now()
        .checked_add(budget)
        .unwrap_or_else(Instant::now);
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "COPY receive deadline exceeded",
            ));
        }
        match framing::read_typed_frame_header(stream) {
            Ok(h) => {
                let body_budget = txn
                    .remaining_duration()
                    .map_or(budget, |r| base_timeout.map_or(r, |b| r.min(b)));
                let body_deadline = Instant::now()
                    .checked_add(body_budget)
                    .unwrap_or_else(Instant::now);
                return Ok((h, Some(framing::FrameDeadlineGuard::set(body_deadline))));
            }
            Err(FrameError::Io(e))
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) => {}
            Err(e) => return Err(respond_frame_error_and_terminate(stream, e)),
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
#[allow(clippy::too_many_arguments)]
/// [`run_copy_from_inner`] の呼び出しを挟み、受信期限のために変更した読み取りタイムアウトを
/// 呼び出し前の値へ戻す（`post_auth_loop` が現在値をキャッシュしているため）。
fn run_copy_from<'e, S: WireStream>(
    stream: &mut S,
    engine: &'e EngineCore,
    ctx: &PolicyContext,
    conn_session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    session: CopyInSession,
    finish: Finish,
    role: StatementRole,
) -> io::Result<CopyStep> {
    let base_timeout = stream.read_timeout()?;
    let result = run_copy_from_inner(
        stream,
        engine,
        ctx,
        conn_session,
        txn,
        session,
        finish,
        role,
        base_timeout,
    );
    if result.is_ok() {
        stream.set_read_timeout(base_timeout)?;
    }
    result
}

#[allow(clippy::too_many_arguments)]
fn run_copy_from_inner<'e, S: WireStream>(
    stream: &mut S,
    engine: &'e EngineCore,
    ctx: &PolicyContext,
    conn_session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    mut session: CopyInSession,
    finish: Finish,
    role: StatementRole,
    base_timeout: Option<Duration>,
) -> io::Result<CopyStep> {
    let mut decoder = match prepare_decoder(&session) {
        Ok(d) => d,
        Err(e) => return respond_copy_from_error(stream, txn, &e).map(|()| CopyStep::Failed),
    };
    let response = match encode_copy_response(
        b'G',
        session.column_count(),
        matches!(decoder, CopyInDecoder::Binary { .. }),
    ) {
        Ok(b) => b,
        Err(()) => {
            return respond_error_and_ready(
                stream,
                txn,
                ErrorClass::InternalError,
                "failed to encode CopyInResponse",
            )
            .map(|()| CopyStep::Failed)
        }
    };
    stream.write_all(&response)?;

    let mut errored: Option<CopyFromError> = None;
    let mut discarded_bytes: usize = 0;
    let mut discarded_messages: usize = 0;

    loop {
        let (header, _frame_deadline) = read_copy_frame_header(stream, txn, base_timeout)?;
        let type_byte = match header {
            Some(b) => b,
            None => return Ok(CopyStep::Closed),
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
                } else if let Err(e) = feed_copy_data(&mut decoder, &mut session, &body) {
                    errored = Some(e);
                }
            }
            b'c' => {
                framing::read_length_prefixed_body(stream, 4, 4)
                    .map_err(|e| respond_frame_error_and_terminate(stream, e))?;
                return finish_copy_from(
                    stream,
                    engine,
                    ctx,
                    conn_session,
                    txn,
                    session,
                    decoder,
                    errored,
                    finish,
                    role,
                );
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
                    txn,
                    ErrorClass::InvalidInput,
                    "COPY failed on the client side",
                )
                .map(|()| CopyStep::Failed);
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
                return Ok(CopyStep::Closed);
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
#[allow(clippy::too_many_arguments)]
fn finish_copy_from<'e, S: WireStream>(
    stream: &mut S,
    engine: &'e EngineCore,
    ctx: &PolicyContext,
    conn_session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    session: CopyInSession,
    decoder: CopyInDecoder,
    errored: Option<CopyFromError>,
    finish: Finish,
    role: StatementRole,
) -> io::Result<CopyStep> {
    if let Some(e) = errored {
        return respond_copy_from_error(stream, txn, &e).map(|()| CopyStep::Failed);
    }
    // binary はタプルの途中で終わった入力（CopyDone がフレームの途中に届いた）を `22P04` で拒否する。
    // タプル境界またはトレーラ後の終端のみ成功とする（PostgreSQL と同じ）。
    if let CopyInDecoder::Binary { reader, .. } = decoder {
        if let Err(e) = reader.finish() {
            return respond_copy_from_error(stream, txn, &CopyFromError::Frame(e))
                .map(|()| CopyStep::Failed);
        }
    }

    let batch = match session.finish() {
        Ok(b) => b,
        Err(e) => return respond_sql_error(stream, txn, &e).map(|()| CopyStep::Failed),
    };

    let outcome = {
        let _emergency_registration = crate::simple_query::register_emergency_response(stream);
        // セッション対応版: 一時表向けのバッチの commit と、自動コミットの COPY 後の `ON COMMIT`
        // 適用（TABLE-44・Issue #1684）に接続の `SessionState` が要る。
        // 暗黙トランザクション（複数文メッセージ。WIRE-20）の最後の文が COPY のときは、
        // 通常の文の `ImplicitFinal` と同じく commit を緊急応答の登録区間の内側で行い、
        // `CommandComplete` はその後に書く（commit 失敗時は `CommandComplete` を送らない）。
        engine
            .commit_copy_in_txn_in_session(ctx, conn_session, txn, batch)
            .and_then(|insert_outcome| {
                if role == StatementRole::ImplicitFinal {
                    txn.commit_implicit(conn_session)?;
                }
                Ok(insert_outcome)
            })
    };

    match outcome {
        Ok(insert_outcome) => {
            let tag = format!("COPY {}", insert_outcome.rows_affected);
            match result_encoder::encode_command_complete(&tag) {
                Ok(msg) => {
                    stream.write_all(&msg)?;
                    if finish == Finish::ReadyForQuery {
                        crate::handshake::write_ready_for_query_synced_io(
                            stream,
                            conn_session,
                            txn,
                        )?;
                    }
                    Ok(CopyStep::Completed { tag })
                }
                Err(_) => respond_error_and_ready(
                    stream,
                    txn,
                    ErrorClass::InternalError,
                    "failed to encode command complete response",
                )
                .map(|()| CopyStep::Failed),
            }
        }
        Err(e) => respond_sql_error(stream, txn, &e).map(|()| CopyStep::Failed),
    }
}

/// COPY 1 文の実行結果（Issue #1175）。`simple_query::run_copy_statement` が、
/// 複数文メッセージの続きの文へ進むか・打ち切るか・接続を閉じるかを判定するために使う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CopyStep {
    /// `CommandComplete "COPY n"`（と `finish` に応じた `ReadyForQuery`）を書き終えた。
    /// `tag` は送出済みの完了タグ。拡張クエリの Execute（Issue #1760）が portal を
    /// `Done` へ遷移させ、再 Execute でタグだけを再送するために保持する。
    Completed { tag: String },
    /// ErrorResponse を書き終えた（`ReadyForQuery` は呼び出し元が復元後に送る）。呼び出し元は以降の文を実行しない。
    Failed,
    /// Terminate（'X'）受信・早期 EOF。呼び出し元は接続ループを終了させる。
    Closed,
}

/// `simple_query::run_copy_statement` から、`engine::sql::copy::
/// is_copy_statement(stmt)` が真の 1 文について、セッションが `Idle` または明示
/// トランザクション中（Issue #1179）、または暗黙トランザクション中（複数文メッセージ内。
/// WIRE-20・Issue #1773。`role` で位置を渡す。`Failed` 中は呼ばれない）
/// に呼ばれる唯一の入口（`handshake::post_auth_loop` の 'Q' 分岐は `simple_query::
/// execute_and_respond` へ一本化した。Issue #1175）。
/// `EngineCore::begin_copy` の構造検証・`operation_id` 必須化ガード・
/// テーブル解決がここで失敗した場合は CopyIn／CopyOutResponse を一切送らずに
/// 通常の ErrorResponse＋ReadyForQuery を返す（PostgreSQL 互換: CopyIn/Out
/// サブプロトコルへ入ってしまってからの構文エラーは無い）。
///
/// `finish` が [`Finish::Continue`] のときは、成功時の `CommandComplete "COPY n"` の
/// 後に `ReadyForQuery` を送らず、複数文メッセージの次の文へ制御を返す
/// （エラー経路は常に ErrorResponse＋`ReadyForQuery`）。
///
/// `role` は暗黙トランザクション内での位置（[`StatementRole`]）。`ImplicitFinal` のときだけ
/// 本関数が `commit_implicit` を行う（COPY FROM は `CommandComplete` の前、COPY TO は
/// CopyOutResponse の前）。それ以外（`Standalone`・`ImplicitIntermediate`）は commit しない。
///
/// 戻り値 [`CopyStep::Closed`] は `run_copy_from` が Terminate（'X'）受信を
/// 返してきた場合で、呼び出し元はそのまま伝播して接続ループを終了させる
/// （[`run_copy_from`] のドキュメント参照）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn run<'e, S: WireStream>(
    stream: &mut S,
    engine: &'e EngineCore,
    ctx: &PolicyContext,
    session: &mut SessionState,
    txn: &mut SessionTransaction<'e>,
    sql: &str,
    finish: Finish,
    role: StatementRole,
) -> io::Result<CopyStep> {
    // commit 成功から本関数が応答を書き終えるまでの区間全体を覆う RAII ガード
    // （RECOVER-5 (3)。`simple_query::execute_and_respond` と同じ設計）。
    let _response_boundary = engine::recovery::commit_boundary::ResponseBoundaryGuard::new();

    // COPY TO の結果は `begin_copy_in_txn` の時点で実体化済みなので、暗黙トランザクションの
    // 最後の文なら CopyOutResponse を送る前に commit する（通常の SELECT の
    // `ImplicitFinal` と同じ「commit してから応答を書く」順序。WIRE-20）。commit は緊急応答の
    // 登録区間の内側で行い、失敗時は CopyOutResponse を 1 バイトも送らず ErrorResponse を返す。
    let plan = {
        let _emergency_registration = (role == StatementRole::ImplicitFinal)
            .then(|| crate::simple_query::register_emergency_response(stream));
        engine
            .begin_copy_in_txn(ctx, session, txn, sql)
            .and_then(|plan| {
                if let CopyPlan::To(format, result, options) = &plan {
                    if let Err(rejection) = precheck_copy_to(*format, result, options) {
                        return Ok(Err(rejection));
                    }
                    if role == StatementRole::ImplicitFinal {
                        txn.commit_implicit(session)?;
                    }
                }
                Ok(Ok(plan))
            })
    };
    let plan = match plan {
        Ok(Ok(plan)) => Ok(plan),
        // 応答開始前の拒否。commit しておらず、`respond_error_and_ready` が暗黙トランザクションを破棄する。
        Ok(Err((class, message))) => {
            return respond_error_and_ready(stream, txn, class, message).map(|()| CopyStep::Failed)
        }
        Err(e) => Err(e),
    };
    match plan {
        Ok(CopyPlan::To(format, result, options)) => {
            run_copy_to(stream, session, txn, format, &result, &options, finish)
        }
        Ok(CopyPlan::From(copy_session)) => run_copy_from(
            stream,
            engine,
            ctx,
            session,
            txn,
            copy_session,
            finish,
            role,
        ),
        Err(e) => respond_sql_error(stream, txn, &e).map(|()| CopyStep::Failed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_copy_text_escapes_control_characters() {
        assert_eq!(escape_copy_text("a\tb\nc\\d", b'\t'), "a\\tb\\nc\\\\d");
    }

    #[test]
    fn escape_copy_text_escapes_custom_delimiter() {
        assert_eq!(escape_copy_text("a|b\tc", b'|'), "a\\|b\\tc");
        // 既定のタブ区切りでは二重エスケープしない。
        assert_eq!(escape_copy_text("a|b\tc", b'\t'), "a|b\\tc");
        // 制御文字の区切りは前に `\` を付けた生の文字になる。
        assert_eq!(escape_copy_text("a\u{1}b", b'\x01'), "a\\\u{1}b");
        assert_eq!(escape_copy_text("a\u{2}b", b'\x01'), "a\u{2}b");
    }

    #[test]
    fn encode_copy_response_has_expected_layout_for_two_columns() {
        let msg = encode_copy_response(b'G', 2, false).expect("encode");
        assert_eq!(msg[0], b'G');
        let len = i32::from_be_bytes([msg[1], msg[2], msg[3], msg[4]]) as usize;
        assert_eq!(len, msg.len() - 1);
        assert_eq!(msg[5], 0); // overall format
        let ncols = i16::from_be_bytes([msg[6], msg[7]]);
        assert_eq!(ncols, 2);
    }

    #[test]
    fn encode_copy_response_binary_sets_overall_and_column_formats_to_one() {
        let msg = encode_copy_response(b'G', 3, true).expect("encode");
        assert_eq!(msg[5], 1);
        assert_eq!(i16::from_be_bytes([msg[6], msg[7]]), 3);
        for i in 0..3 {
            let at = 8 + i * 2;
            assert_eq!(i16::from_be_bytes([msg[at], msg[at + 1]]), 1);
        }
        assert_eq!(msg.len(), 1 + 4 + 1 + 2 + 3 * 2);
    }

    #[test]
    fn encode_copy_response_binary_sets_overall_and_per_column_format_to_one() {
        // WIRE-23・Issue #1770: binary は全体・列ごとともに format=1。
        let msg = encode_copy_response(b'H', 2, true).expect("encode");
        assert_eq!(msg[5], 1);
        assert_eq!(&msg[6..8], &2i16.to_be_bytes());
        assert_eq!(&msg[8..10], &1i16.to_be_bytes());
        assert_eq!(&msg[10..12], &1i16.to_be_bytes());
    }

    #[test]
    fn binary_copy_data_row_has_header_only_on_first_and_rejects_text_path() {
        use engine::catalog::ColumnType;
        let columns = vec![ColumnMeta::Scalar {
            name: "n".to_string(),
            ty: ColumnType::Integer,
        }];
        let row = ResultRow {
            id: 1,
            score: 0.0,
            cells: vec![Cell::Null],
        };
        let mut first = Vec::new();
        encode_binary_copy_data_into(&columns, &row, true, &mut first).expect("first");
        let mut next = Vec::new();
        encode_binary_copy_data_into(&columns, &row, false, &mut next).expect("next");
        assert_eq!(first.len(), next.len() + 19);
        // NULL は Int32 の -1。
        assert_eq!(&next[5..], &[0, 1, 0xff, 0xff, 0xff, 0xff]);
        // 長さフィールドは本文長 + 4。
        assert_eq!(
            i32::from_be_bytes([next[1], next[2], next[3], next[4]]) as usize,
            next.len() - 1
        );
    }

    #[test]
    fn encode_copy_done_is_exactly_five_bytes() {
        let msg = encode_copy_done();
        assert_eq!(msg, [b'c', 0, 0, 0, 4]);
    }
}
