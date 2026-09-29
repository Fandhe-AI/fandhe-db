//! 拡張クエリプロトコルの Bind（'B'）・Execute（'E'）・Sync（'S'）・
//! Close（'C'）・Flush（'H'）、および Parse（'P'）・Describe（'D'。statement・
//! portal 両対象）を受理する（Issue #933・#934・TASK-71・WIRE-11）。
//!
//! `handshake::post_auth_loop` から、engine が接続済み（`Some`）の場合にのみ
//! 呼ばれる（`engine: None` の場合は従来どおり `protocol_dispatch::
//! reject_and_close` へ流す。本モジュールドキュメント末尾参照）。接続単位の
//! [`ExtendedQueryState`]（`handshake` の接続ループが `SessionState` と並べて
//! 所有し、接続終了で破棄——接続間・テナント間で共有しない）が Parse 済みの
//! [`engine::core::ParsedSql`]（[`PreparedStatementStore`]）と portal
//! （[`PortalStore`]）を保持する。
//!
//! # エラー後の同期回復（ignore-till-sync）
//!
//! Parse／Bind／Describe／Execute／Close／Flush の処理中に起きるエラーのうち、
//! **`read_length_prefixed_body` 自体が成功した後**（＝メッセージの境界が
//! 確定した後）に判明したもの——body の構造不正・SQL 検証エラー・実行エラー・
//! 保持上限超過・未定義の statement／portal 等——はすべて回復可能として扱う。
//! [`respond_error_and_await_sync`] が ErrorResponse を送出したうえで
//! [`ExtendedQueryState::ignore_till_sync`] を立て、接続は維持したまま
//! `handshake::post_auth_loop` が次に届く Sync（'S'）まで後続メッセージを
//! 読み捨てる（'X' は通常どおり終了、COPY・FunctionCall・未知の型バイトは
//! 破棄対象にせず fail-closed に切断する。詳細は `post_auth_loop` 参照）。
//! Sync 到達時にフラグを解除し、全 portal（名前付き・無名を問わない）を
//! 破棄してから `ReadyForQuery` を返し、同期を回復する（portal の寿命は
//! 明示トランザクション〔`BEGIN`/`COMMIT`/`ROLLBACK`。SQL-31・TASK-221〕の
//! 有無とは独立な軸であり、`Active` な明示トランザクション中でも各 Sync
//! サイクルで portal は破棄される——PostgreSQL がトランザクション終了時に
//! portal を閉じる契約〔PostgreSQL 34.4「Bind」〕とは異なり、本サーバーは
//! Sync 境界そのものを portal 破棄の契機とする。名前付き prepared statement
//! は PostgreSQL と同様に保持する）。
//!
//! 一方、`read_length_prefixed_body` 自体が失敗した場合（メッセージの境界を
//! 確定できない・宣言長が上限を超える等）は [`respond_error_and_close`] が
//! ErrorResponse 送出後に有界 lingering close で接続を終了する（回復不能。
//! WIRE-4・WIRE-10 の既存契約のまま）。
//!
//! # portal のライフサイクル
//!
//! Bind が portal を構築し（`ParsedSql` の Bind 時点のスナップショットを保持
//! ——後から statement が再 Parse されても portal の実行対象は変わらない）、
//! Execute が実行・行送出（`max_rows` による分割送出。[`PortalState::
//! Suspended`]）・完了（[`PortalState::Done`]。副作用は再実行しない）を管理
//! する。名前付き・無名を問わず全 portal は Sync のたびに破棄される
//! （portal の寿命は明示トランザクション〔SQL-31・TASK-221〕の有無とは
//! 独立に Sync 境界で決まる契約。PostgreSQL のトランザクション終了時の
//! portal 破棄契約とは異なる点はモジュール冒頭「エラー後の同期回復」節
//! 参照。名前付き prepared statement は Sync を越えて
//! 残る）。Close(Statement) はその statement から作られた portal もまとめて
//! 閉じる。
//!
//! カーソル `FETCH`（WIRE-15・TASK-218）から作った portal は、Sync 境界とは
//! 別に、実行成功時点で束縛した [`CursorBinding`]（トランザクション世代・
//! カーソル個体識別子）を中断保持分の再開前に突き合わせる——`CLOSE`／
//! `COMMIT`／`ROLLBACK`（→ 世代不一致）・同名での再 `DECLARE`（→ 識別子
//! 不一致）を、名前付き portal が Sync を跨がず生き残っている間に挟んでも
//! 蓄積済みフレームを送出しない（PR #1049 レビュー指摘対応。詳細は
//! `docs/design/sql-cursor.md`「wire-server 拡張クエリの portal 束縛」節
//! 参照）。`Fetch` 以外の文から作った portal は対象外。
//!
//! 中断中の全 portal が保持するバイト数の合計（[`crate::limits::
//! MAX_SUSPENDED_PORTAL_BYTES_PER_SESSION`]）は接続（セッション）全体の
//! 上限として運用する（[`PortalStore::total_suspended_bytes_excluding`]。
//! 名前付き portal は最大 [`MAX_PORTALS_PER_SESSION`] 個まで同時に中断され
//! うるため、portal 単体への上限にすると接続全体では約
//! `MAX_PORTALS_PER_SESSION` 倍相当まで保持できてしまう。PR #1013 レビュー
//! 指摘・P0）。判定は `max_rows` で送出予定（`take`）に入らない行だけを
//! 対象に、送出（`take` 分のエンコード・`ResponseBuffer` への送出）を始める
//! 前に完了させる ―― 超過が判明した時点で 1 バイトも送らずに拒否し、1 回の
//! Execute は「送出が丸ごと成功する」か「丸ごと失敗する」かのいずれかで
//! ある契約を保つ。送出予定（`take`）分は結果セット全体を先にメモリへ
//! エンコードして保持するのではなく、1 行ずつエンコード→送出バッファへ
//! 積んで即座に送る（`max_rows<=0`／非常に大きい `max_rows` を外部 wire
//! 入力から指定されても、送出予定分のメモリ使用量が結果セット規模に応じて
//! 無制限に膨らまない。PR #1013 レビュー指摘・P0）。
//!
//! # SQLSTATE 写像（spec に専用コードが無い箇所は既存の閉じた 16 分類
//! [`engine::error_format::ErrorClass`] へ倒す。詳細は各エラー variant の
//! ドキュメント参照）
//!
//! - 未定義ステートメント名／portal 名への参照・名前付きステートメント／
//!   portal の重複作成・Bind のパラメータ数不一致・format code の個数不正
//!   → `08P01`（`ProtocolViolation`）
//! - Bind のパラメータ format code が binary（1）で対象スロットが text 系以外
//!   （WIRE-12・#1171）・結果 format code が非対応型の列を
//!   binary 指定（WIRE-14・[`result_encoder::column_binary_support`]）・
//!   Describe(Portal) 対象の受理不能・実行結果列の不整合（いずれも `0A000`
//!   の「未対応機能」区分を fail-closed なガードとして流用）
//!   → `0A000`（`FeatureNotSupported`）
//! - 結果 format code の個数不正・値不正（0/1 以外） → `08P01`
//!   （`ProtocolViolation`。[`result_encoder::BinaryFormatError`]）
//! - 件数・名前長・保持バイト上限超過 →
//!   `54000`（`PayloadTooLarge`。[`crate::limits`] の各定数）
//! - body の構造不正（NUL 終端欠落・余剰バイト・負の件数・非 UTF-8・種別バイト
//!   不正）→ `08P01`
//! - パラメータ型宣言の件数が上限（`MAX_PARAMS`）超過 → `54000`。プレースホルダ数
//!   超過 → `08P01`（WIRE-12・#1171。宣言 OID 自体は受理し値検証には使わない）
//! - Bind のパラメータ format code の値不正（0/1 以外）・値数不一致 → `08P01`
//! - SQL 検証失敗・実行時エラー → `engine::sql::allowlist::SqlSurfaceError::
//!   error_class()`（簡易クエリと同一の分類）
//!
//! # `engine: None` の経路
//!
//! `handle_connection_bounded` 経由（後方互換パス）では、'P'／'D'／'B'／'E'／
//! 'S'／'C'／'H' のいずれも従来どおり `protocol_dispatch::reject_and_close`
//! （`0A000` + 切断）のまま（WIRE-8 の契約を維持）。

use std::io;

use engine::core::{EngineCore, ParsedSql, PreparedSql};
use engine::error_format::{ClassifiedError, ErrorClass};
use engine::recovery::commit_boundary::ResponseBoundaryGuard;
use engine::sql::exec::ColumnMeta;
use engine::sql::mode::SessionState;
use engine::sql::params::{PreparedParamType, MAX_PARAMS};

use crate::error_response;
use crate::framing::{self, FrameError};
use crate::limits::{
    MAX_BOUND_PARAM_BYTES_PER_SESSION, MAX_PORTALS_PER_SESSION, MAX_PREPARED_SQL_BYTES_PER_SESSION,
    MAX_PREPARED_STATEMENTS_PER_SESSION, MAX_RESPONSE_BUFFER_BYTES, MAX_STATEMENT_NAME_LEN,
    MAX_SUSPENDED_PORTAL_BYTES_PER_SESSION,
};
use crate::result_encoder;
use crate::wire_stream::WireStream;

/// Parse 本文の最小長（空文字列ステートメント名 cstr・空クエリ cstr・
/// パラメータ件数 i16 の合計）。
const MIN_PARSE_BODY_LEN: usize = 4;
/// Describe 本文の最小長（種別 1 バイト・空文字列名 cstr）。
const MIN_DESCRIBE_BODY_LEN: usize = 2;
/// Bind 本文の最小長（空 portal 名 cstr・空 statement 名 cstr・format code 数
/// i16・パラメータ数 i16・結果 format code 数 i16 の合計）。
const MIN_BIND_BODY_LEN: usize = 8;
/// Execute 本文の最小長（空 portal 名 cstr・max_rows i32 の合計）。
const MIN_EXECUTE_BODY_LEN: usize = 5;
/// Close 本文の最小長（種別 1 バイト・空文字列名 cstr）。
const MIN_CLOSE_BODY_LEN: usize = 2;

/// `post_auth_loop` へ返す、ループを継続するか（応答送出済み。エラー後の
/// 同期回復モードへ入った場合を含む）接続を閉じたか（フレーム違反による
/// ErrorResponse＋lingering close 済み）の合図。
pub(crate) enum LoopSignal {
    Continue,
    Closed,
}

/// body 復号の構造不正。`untrusted` な生バイト列から `get()`/`try_into()` の
/// みで復号する（`unwrap`/`expect`/添字アクセス禁止。`.claude/rules/
/// coding-rust.md` P0）。Parse／Describe／Bind／Execute／Close いずれの body
/// 復号も共有する。
#[derive(Debug)]
pub(crate) enum BodyError {
    /// NUL 終端の cstring を最後まで走査したが終端が見つからなかった。
    MissingNulTerminator,
    /// cstring が UTF-8 として不正。
    InvalidUtf8,
    /// 件数フィールド（パラメータ・format code 等）が負値。
    NegativeCount,
    /// 宣言済みの構造を読み終えた後に余剰バイトが残っている、または宣言から
    /// 期待されるバイト数に満たない。
    TrailingOrTruncatedBytes,
    /// Describe／Close の種別バイトが `'S'`/`'P'` のいずれでもない。
    InvalidTargetKind,
}

impl BodyError {
    fn message(&self) -> &'static str {
        match self {
            BodyError::MissingNulTerminator => {
                "malformed extended query message: missing NUL terminator"
            }
            BodyError::InvalidUtf8 => "malformed extended query message: invalid UTF-8",
            BodyError::NegativeCount => "malformed extended query message: negative count field",
            BodyError::TrailingOrTruncatedBytes => {
                "malformed extended query message: trailing or truncated bytes"
            }
            BodyError::InvalidTargetKind => "malformed extended query message: invalid target kind",
        }
    }
}

/// body の先頭から NUL 終端 cstring を 1 個読む（`pos` を終端の次バイトへ進める）。
fn read_cstring(body: &[u8], pos: &mut usize) -> Result<String, BodyError> {
    let rest = body.get(*pos..).ok_or(BodyError::MissingNulTerminator)?;
    let nul_offset = rest
        .iter()
        .position(|&b| b == 0)
        .ok_or(BodyError::MissingNulTerminator)?;
    let bytes = rest
        .get(..nul_offset)
        .ok_or(BodyError::MissingNulTerminator)?;
    let s = std::str::from_utf8(bytes)
        .map_err(|_| BodyError::InvalidUtf8)?
        .to_string();
    *pos = pos
        .checked_add(nul_offset)
        .and_then(|p| p.checked_add(1))
        .ok_or(BodyError::TrailingOrTruncatedBytes)?;
    Ok(s)
}

/// body から `i16`（ビッグエンディアン）を 1 個読む（`pos` を 2 バイト進める）。
fn read_i16(body: &[u8], pos: &mut usize) -> Result<i16, BodyError> {
    let end = pos
        .checked_add(2)
        .ok_or(BodyError::TrailingOrTruncatedBytes)?;
    let bytes = body
        .get(*pos..end)
        .ok_or(BodyError::TrailingOrTruncatedBytes)?;
    let arr: [u8; 2] = bytes
        .try_into()
        .map_err(|_| BodyError::TrailingOrTruncatedBytes)?;
    *pos = end;
    Ok(i16::from_be_bytes(arr))
}

/// body から `i32`（ビッグエンディアン）を 1 個読む（`pos` を 4 バイト進める）。
fn read_i32(body: &[u8], pos: &mut usize) -> Result<i32, BodyError> {
    let end = pos
        .checked_add(4)
        .ok_or(BodyError::TrailingOrTruncatedBytes)?;
    let bytes = body
        .get(*pos..end)
        .ok_or(BodyError::TrailingOrTruncatedBytes)?;
    let arr: [u8; 4] = bytes
        .try_into()
        .map_err(|_| BodyError::TrailingOrTruncatedBytes)?;
    *pos = end;
    Ok(i32::from_be_bytes(arr))
}

/// 復号済みの Parse メッセージ。
struct ParseMessage {
    name: String,
    query: String,
    num_param_types: usize,
    /// 宣言型 OID 列の生バイト（`num_param_types * 4` バイト。フレーム上限で
    /// 有界）。件数上限（[`MAX_PARAMS`]）の判定を通過した後にだけ
    /// [`decode_declared_oids`] で復号する（上限検証前にアロケーションしない）。
    param_oid_bytes: Vec<u8>,
}

/// 宣言型 OID 列を復号する（呼び出し元が件数 <= [`MAX_PARAMS`] を確認済みで
/// あること）。`as_chunks::<4>` で 4 バイト単位に分割するため添字アクセスを使わない（余りは無視）。
fn decode_declared_oids(bytes: &[u8]) -> Vec<i32> {
    let (chunks, _rest) = bytes.as_chunks::<4>();
    chunks.iter().map(|c| i32::from_be_bytes(*c)).collect()
}

/// Parse（'P'）の body（PostgreSQL wire v3: 文字列名 cstr・クエリ文字列 cstr・
/// パラメータ型 OID 件数 i16・OID 列 i32×件数）を復号する。OID 列は件数分の
/// 存在（長さ整合）のみ検証して生バイトのまま保持し、復号は呼び出し元が件数上限を
/// 確認した後に行う（WIRE-12）。
fn parse_parse_body(body: &[u8]) -> Result<ParseMessage, BodyError> {
    let mut pos = 0usize;
    let name = read_cstring(body, &mut pos)?;
    let query = read_cstring(body, &mut pos)?;
    let count = read_i16(body, &mut pos)?;
    if count < 0 {
        return Err(BodyError::NegativeCount);
    }
    let num_param_types = count as usize;
    let expected_oid_bytes = num_param_types
        .checked_mul(4)
        .ok_or(BodyError::TrailingOrTruncatedBytes)?;
    let remaining = body.get(pos..).ok_or(BodyError::TrailingOrTruncatedBytes)?;
    if remaining.len() != expected_oid_bytes {
        return Err(BodyError::TrailingOrTruncatedBytes);
    }
    Ok(ParseMessage {
        name,
        query,
        num_param_types,
        param_oid_bytes: remaining.to_vec(),
    })
}

/// Describe（'D'）・Close（'C'）が対象とする種別（body 先頭 1 バイトで共通）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetKind {
    Statement,
    Portal,
}

fn read_target_kind(body: &[u8], pos: &mut usize) -> Result<TargetKind, BodyError> {
    let kind = *body.get(*pos).ok_or(BodyError::TrailingOrTruncatedBytes)?;
    *pos = pos
        .checked_add(1)
        .ok_or(BodyError::TrailingOrTruncatedBytes)?;
    match kind {
        b'S' => Ok(TargetKind::Statement),
        b'P' => Ok(TargetKind::Portal),
        _ => Err(BodyError::InvalidTargetKind),
    }
}

struct DescribeMessage {
    target: TargetKind,
    name: String,
}

/// Describe（'D'）の body（種別バイト 'S'/'P'・対象名 cstr）を復号する。
fn parse_describe_body(body: &[u8]) -> Result<DescribeMessage, BodyError> {
    let mut pos = 0usize;
    let target = read_target_kind(body, &mut pos)?;
    let name = read_cstring(body, &mut pos)?;
    if pos != body.len() {
        return Err(BodyError::TrailingOrTruncatedBytes);
    }
    Ok(DescribeMessage { target, name })
}

struct CloseMessage {
    target: TargetKind,
    name: String,
}

/// Close（'C'）の body（種別バイト 'S'/'P'・対象名 cstr）を復号する
/// （[`parse_describe_body`] と同型のレイアウト）。
fn parse_close_body(body: &[u8]) -> Result<CloseMessage, BodyError> {
    let mut pos = 0usize;
    let target = read_target_kind(body, &mut pos)?;
    let name = read_cstring(body, &mut pos)?;
    if pos != body.len() {
        return Err(BodyError::TrailingOrTruncatedBytes);
    }
    Ok(CloseMessage { target, name })
}

/// 復号済みの Bind メッセージ。`param_values` は `$1` から順の生バイト列
/// （`None` は NULL）。総量は body（フレーム上限）で有界。
struct BindMessage {
    portal_name: String,
    statement_name: String,
    param_format_codes: Vec<i16>,
    param_values: Vec<Option<Vec<u8>>>,
    result_format_codes: Vec<i16>,
}

/// Bind（'B'）の body（portal 名 cstr・statement 名 cstr・パラメータ format
/// code 件数 i16＋列・パラメータ件数 i16＋各値（i32 長。-1 は NULL）・結果
/// format code 件数 i16＋列）を復号する。
fn parse_bind_body(body: &[u8]) -> Result<BindMessage, BodyError> {
    let mut pos = 0usize;
    let portal_name = read_cstring(body, &mut pos)?;
    let statement_name = read_cstring(body, &mut pos)?;

    let format_code_count = read_i16(body, &mut pos)?;
    if format_code_count < 0 {
        return Err(BodyError::NegativeCount);
    }
    let mut param_format_codes = Vec::new();
    for _ in 0..format_code_count {
        param_format_codes.push(read_i16(body, &mut pos)?);
    }

    let num_params_declared = read_i16(body, &mut pos)?;
    if num_params_declared < 0 {
        return Err(BodyError::NegativeCount);
    }
    let num_params = num_params_declared as usize;
    // 各値は最低 4 バイト（長さ）を要するため、件数は body 長で有界。
    let mut param_values: Vec<Option<Vec<u8>>> = Vec::new();
    for _ in 0..num_params {
        let len = read_i32(body, &mut pos)?;
        if len == -1 {
            // NULL: 値本体を持たない。
            param_values.push(None);
        } else if len < 0 {
            return Err(BodyError::NegativeCount);
        } else {
            let len = len as usize;
            let end = pos
                .checked_add(len)
                .ok_or(BodyError::TrailingOrTruncatedBytes)?;
            let value = body
                .get(pos..end)
                .ok_or(BodyError::TrailingOrTruncatedBytes)?;
            param_values.push(Some(value.to_vec()));
            pos = end;
        }
    }

    let result_format_count = read_i16(body, &mut pos)?;
    if result_format_count < 0 {
        return Err(BodyError::NegativeCount);
    }
    let mut result_format_codes = Vec::new();
    for _ in 0..result_format_count {
        result_format_codes.push(read_i16(body, &mut pos)?);
    }

    if pos != body.len() {
        return Err(BodyError::TrailingOrTruncatedBytes);
    }

    Ok(BindMessage {
        portal_name,
        statement_name,
        param_format_codes,
        param_values,
        result_format_codes,
    })
}

/// 復号済みの Execute メッセージ。
struct ExecuteMessage {
    portal_name: String,
    max_rows: i32,
}

/// Execute（'E'）の body（portal 名 cstr・max_rows i32）を復号する。
///
/// `max_rows` は untrusted なワイヤ入力であり、PostgreSQL プロトコルでは
/// `0` のみが「無制限取得」を意味する（PR #1013 レビュー指摘・P0）。負値は
/// `take = (max_rows as usize).min(total)` へそのまま渡すと `as usize` の
/// キャストで巨大値化し「全行送出」と等価になってしまう`0` 専用の意味を
/// 負値が僭称しないよう、ここで fail-closed に拒否して `08P01` へ写像する。
fn parse_execute_body(body: &[u8]) -> Result<ExecuteMessage, BodyError> {
    let mut pos = 0usize;
    let portal_name = read_cstring(body, &mut pos)?;
    let max_rows = read_i32(body, &mut pos)?;
    if max_rows < 0 {
        return Err(BodyError::NegativeCount);
    }
    if pos != body.len() {
        return Err(BodyError::TrailingOrTruncatedBytes);
    }
    Ok(ExecuteMessage {
        portal_name,
        max_rows,
    })
}

/// 接続単位で Parse 済みステートメントを保持する（`sql_bytes` は
/// [`MAX_PREPARED_SQL_BYTES_PER_SESSION`] の累計判定用。`Empty` は空文字列の
/// クエリテキスト——PostgreSQL は空文の Parse を構文エラーにせず受理する）。
pub(crate) enum PreparedStatement {
    Empty,
    Parsed(ParsedSql),
    /// `$n` を含む文（WIRE-12）。engine の [`PreparedSql`] テンプレートと、
    /// `$n` ごとの公告 OID・バイナリ可否（[`ParamSlot`]）を保持する。Bind が
    /// `engine.bind_prepared` で値を束縛して [`ParsedSql`] を得る。
    Parameterized {
        prepared: PreparedSql,
        slots: Vec<ParamSlot>,
    },
}

/// `$n` 1 個分の公告情報（Parse が確定し、Describe(S) の
/// `ParameterDescription` と Bind のバイナリ受理判定が参照する）。
/// 値の検証意味論には影響しない（値は常に engine のリテラル置換経路を通る）。
pub(crate) struct ParamSlot {
    /// `ParameterDescription` で公告する型 OID（宣言 OID が 0 以外ならその
    /// echo、それ以外は推論 OID）。
    oid: i32,
    /// バイナリ形式の値を受理してよいか（text 系スロットのみ。バイト列は
    /// UTF-8 の恒等表現としてそのまま engine へ渡す）。
    binary: bool,
}

/// 宣言 OID のうち、text 系スロットへバイナリ（UTF-8 バイト恒等）を許す型。
/// 0（未指定）・25（text）・1043（varchar）。
const TEXT_LIKE_DECLARED_OIDS: [i32; 3] = [0, 25, 1043];

/// 接続単位（`handshake` の接続ループが `SessionState` と並べて所有し、接続終了
/// で破棄）の名前付き／無名ステートメント保持。無名（`""`）は
/// 件数上限にカウントせず黙って置換する（PostgreSQL の既定挙動）。
pub(crate) struct PreparedStatementStore {
    statements: std::collections::HashMap<String, (PreparedStatement, usize)>,
    total_sql_bytes: usize,
}

/// 保持上限超過・構造違反。
#[derive(Debug)]
enum StoreError {
    NameTooLong,
    DuplicateName,
    TooManyStatements,
    TotalBytesExceeded,
}

impl PreparedStatementStore {
    pub(crate) fn new() -> Self {
        PreparedStatementStore {
            statements: std::collections::HashMap::new(),
            total_sql_bytes: 0,
        }
    }

    /// `name`・`sql_bytes`（Parse 対象の生クエリテキストのバイト長。保持バイト
    /// 上限の判定基準）・`statement` を保持する。上限判定はアロケーション
    /// （`HashMap::insert`）より前に行う。
    fn insert(
        &mut self,
        name: String,
        sql_bytes: usize,
        statement: PreparedStatement,
    ) -> Result<(), StoreError> {
        let is_anonymous = name.is_empty();
        if !is_anonymous {
            if name.len() > MAX_STATEMENT_NAME_LEN {
                return Err(StoreError::NameTooLong);
            }
            if self.statements.contains_key(&name) {
                return Err(StoreError::DuplicateName);
            }
            // 無名（""）エントリは件数上限にカウントしない契約（構造体コメント
            // 参照）。`statements.len()` には無名分も含まれるため、名前付き
            // （非空キー）のみを数え上げて判定する。
            let named_count = self.statements.keys().filter(|k| !k.is_empty()).count();
            if named_count >= MAX_PREPARED_STATEMENTS_PER_SESSION {
                return Err(StoreError::TooManyStatements);
            }
        }

        // 既存の同名エントリ（無名の再 Parse による黙った置換）が保持していた
        // バイト数を先に差し引いてから累計判定する。
        let existing_bytes = self.statements.get(&name).map(|(_, bytes)| *bytes);
        let base = existing_bytes
            .map(|b| self.total_sql_bytes.saturating_sub(b))
            .unwrap_or(self.total_sql_bytes);
        let new_total = base
            .checked_add(sql_bytes)
            .ok_or(StoreError::TotalBytesExceeded)?;
        if new_total > MAX_PREPARED_SQL_BYTES_PER_SESSION {
            return Err(StoreError::TotalBytesExceeded);
        }

        self.total_sql_bytes = new_total;
        self.statements.insert(name, (statement, sql_bytes));
        Ok(())
    }

    fn get(&self, name: &str) -> Option<&PreparedStatement> {
        self.statements.get(name).map(|(stmt, _)| stmt)
    }

    /// Close（'C'。statement 対象。Issue #934）が呼ぶ。存在しない名前を渡しても
    /// 何もせず成功する（PostgreSQL と同じ挙動）。累計バイト数も併せて減算する。
    fn remove(&mut self, name: &str) {
        if let Some((_, bytes)) = self.statements.get(name) {
            self.total_sql_bytes = self.total_sql_bytes.saturating_sub(*bytes);
        }
        self.statements.remove(name);
    }
}

/// Bind（Issue #934）が構築する portal の実行対象本体。`PreparedStatement` の
/// `Empty`／`Parsed` と同型（`ParsedSql: Clone` のため Bind 時点の値を複製して
/// portal 側に保持し、後から statement が再 Parse されても portal の実行対象は
/// 変わらない契約——PostgreSQL の「bind snapshots the plan」と同じ動作）。
enum PortalBody {
    Empty,
    Parsed(ParsedSql),
}

/// portal が保持する未送出行と、完了時に使う `CommandComplete` タグの組み立て
/// 方（[`crate::simple_query::TagShape`]）。
struct PortalRows {
    /// 未送出の `DataRow` フレーム（`max_rows` ごとに任意の境界で分割送出する
    /// ため、行単位で個別フレームとして保持する）。
    frames: std::collections::VecDeque<Vec<u8>>,
    shape: crate::simple_query::TagShape,
    /// この portal がこれまでの Execute 呼び出し（今回分を含まない）で
    /// 既に送出済みの行数。完了時の `CommandComplete` タグは PostgreSQL の
    /// 契約どおり「portal 全体の累計送出行数」から組み立てる必要があり、
    /// 直近 1 回の Execute で送った行数だけでは分割送出時に過小な値になる
    /// （PR #1013 レビュー指摘・P1。例: 5 行を `max_rows=2` で 3 回に分けて
    /// 取得すると完了タグは `SELECT 5` であるべきだが、直近バッチの件数
    /// だけを使うと `SELECT 1` になってしまう）。
    sent_so_far: usize,
    /// この portal の実行が書き込み系文（`INSERT`/`UPDATE`/`DELETE`/
    /// `TRUNCATE` 等の `RETURNING` 込み）を commit 済みかどうか（PR #1013
    /// レビュー指摘・codex P1）。`true` の場合、この中断保持分の続きを送出する
    /// 際のエンコード・IO 失敗は「commit は既に成功している後処理の失敗」で
    /// あり、[`HandlerError::PostCommit`] として `state=may_be_committed`
    /// detail 付きの ErrorResponse へ写像する（`RECOVER-5` (3)・ERR-5 の
    /// 既存 detail 契約を、panic 経由の緊急応答だけでなく通常の Err 経路にも
    /// 拡張適用する）。
    committed: bool,
}

/// portal の実行状態。
enum PortalState {
    /// 未実行（初回 Execute 待ち）。
    Ready,
    /// `max_rows` 制限により行送出の途中（`PortalSuspended` 済み）。
    Suspended(PortalRows),
    /// 完了済み（副作用は再実行しない。再 Execute には保持済みの `tag`
    /// （実装既定値として件数 0 のタグ）を返す契約）。
    Done { tag: String },
    /// この portal への Execute 実行を試みたが失敗した終端状態。
    /// `engine::execute_parsed_in_txn` を呼ぶ**前**に立て、成功した
    /// 場合のみ末尾で `Done`／`Suspended` へ上書きする（`execute_portal`
    /// 参照）。こうすることで、実行本体の呼び出し自体が失敗した場合・
    /// 呼び出し成功後の後処理（結果列整合検査・行エンコード・中断バイト
    /// 上限判定・応答フレーム送出）が失敗した場合のいずれも、以降の
    /// 再 Execute で `engine::execute_parsed_in_txn` を再実行して副作用を
    /// 重複させることを防ぐ（PR #1013 レビュー指摘・P1・Cursor Bugbot
    /// Medium「Failed Execute leaves portal runnable」。再 Execute は
    /// `HandlerError::PortalFailed` で拒否し、実行し直すには新しい Bind で
    /// portal を作り直す必要がある）。
    Failed,
}

/// カーソル `FETCH`（[`engine::sql::cursor::CursorStatement::Fetch`]）を実行
/// した portal が、その実行成功時点で束縛したトランザクション世代・カーソル
/// 個体識別子（PR #1049 レビュー指摘対応: cursor Bugbot P1「終了した
/// カーソルの FETCH portal から行を送出できる」）。この portal の中断保持分
/// （[`PortalState::Suspended`]）を再送出する前に
/// [`engine::sql::transaction::SessionTransaction::active_generation`]／
/// `cursor_id` と突き合わせ、`CLOSE`／`COMMIT`／`ROLLBACK`（→ 世代不一致）や
/// 同名での再 `DECLARE`（→ 識別子不一致）を挟んでいないかを検証する。`Fetch`
/// 以外の文（通常の検索 `SELECT`・集計・広域取得等）から作られた portal は
/// 常に `None`（この検証の対象外。既存の挙動を変えない）。
#[derive(Debug, Clone)]
struct CursorBinding {
    /// 束縛先のカーソル名（`FETCH ... FROM <name>`）。再送出前の検証で
    /// `SessionTransaction::cursor_id` を引くために使う（`portal.body` から
    /// 都度再解体しない）。
    cursor_name: String,
    txn_generation: u64,
    cursor_id: u64,
}

struct Portal {
    /// この portal を作った statement 名（Close(Statement) が派生 portal も
    /// 連動して閉じるために使う）。
    source_statement: String,
    body: PortalBody,
    /// Bind 時点で `describe_parsed_in_session`（`PortalBody::Parsed`）から
    /// 求めた結果列（`PortalBody::Empty` は常に `None`）。Describe(Portal) の
    /// 応答と、Execute の結果列整合ガードの両方に使う。
    columns: Option<Vec<ColumnMeta>>,
    /// Bind の結果 format code を `columns` の列数へ解決した値（WIRE-14・
    /// `result_encoder::ResultFormats::resolve`）。`columns` が `None`
    /// （結果列なしの statement）の場合は常に空。Describe(Portal) の
    /// `RowDescription` と Execute の `DataRow` 双方が同じ値を参照し、
    /// 「Bind 時点で確定した形式を Execute まで一貫させる」契約を保つ
    /// （PostgreSQL の Bind 規則と同じ）。
    result_formats: Vec<result_encoder::FormatCode>,
    state: PortalState,
    /// [`CursorBinding`] 参照。Bind 時点では常に `None`（内側 SELECT 未実行の
    /// ため束縛先のカーソルが未確定）で、`FETCH` の初回実行が成功した直後
    /// （`execute_portal`）に一度だけ設定する。
    cursor_binding: Option<CursorBinding>,
    /// Bind が束縛した値のバイト合計（`$n` を含まない文は 0）。束縛済み
    /// `ParsedSql` が値を保持するため、接続全体の合計を
    /// [`MAX_BOUND_PARAM_BYTES_PER_SESSION`] で制限する（[`PortalStore::insert`]）。
    /// portal ごとの値から都度合算するため、除去経路での減算漏れが起きない。
    bound_bytes: usize,
}

/// 接続単位で portal を保持する（[`PreparedStatementStore`] と同型の設計）。
pub(crate) struct PortalStore {
    portals: std::collections::HashMap<String, Portal>,
}

#[derive(Debug)]
enum PortalStoreError {
    NameTooLong,
    DuplicateName,
    TooManyPortals,
    BoundBytesExceeded,
}

impl PortalStore {
    fn new() -> Self {
        PortalStore {
            portals: std::collections::HashMap::new(),
        }
    }

    fn insert(&mut self, name: String, portal: Portal) -> Result<(), PortalStoreError> {
        let is_anonymous = name.is_empty();
        if !is_anonymous {
            if name.len() > MAX_STATEMENT_NAME_LEN {
                return Err(PortalStoreError::NameTooLong);
            }
            if self.portals.contains_key(&name) {
                return Err(PortalStoreError::DuplicateName);
            }
            let named_count = self.portals.keys().filter(|k| !k.is_empty()).count();
            if named_count >= MAX_PORTALS_PER_SESSION {
                return Err(PortalStoreError::TooManyPortals);
            }
        }
        // 束縛値バイトの接続全体上限（同名 portal の置換分は差し引く）。
        let others = self
            .portals
            .iter()
            .filter(|(n, _)| n.as_str() != name)
            .fold(0usize, |acc, (_, p)| acc.saturating_add(p.bound_bytes));
        match others.checked_add(portal.bound_bytes) {
            Some(total) if total <= MAX_BOUND_PARAM_BYTES_PER_SESSION => {}
            _ => return Err(PortalStoreError::BoundBytesExceeded),
        }
        self.portals.insert(name, portal);
        Ok(())
    }

    fn get(&self, name: &str) -> Option<&Portal> {
        self.portals.get(name)
    }

    fn get_mut(&mut self, name: &str) -> Option<&mut Portal> {
        self.portals.get_mut(name)
    }

    /// Close（'C'。portal 対象）が呼ぶ。存在しない名前を渡しても何もせず成功
    /// する（PostgreSQL と同じ挙動）。
    fn remove(&mut self, name: &str) {
        self.portals.remove(name);
    }

    /// 簡易クエリ（`'Q'`）処理の直前に無名 portal のみを破棄する
    /// （[`ExtendedQueryState::discard_unnamed_for_simple_query`] が呼ぶ。
    /// PostgreSQL が simple Query の暗黙 Bind と同一視する契約）。
    fn remove_anonymous(&mut self) {
        self.portals.remove("");
    }

    /// Sync（'S'）が名前付き・無名を問わず全 portal を破棄する（[`handle_sync`]
    /// が呼ぶ）。portal の寿命は明示トランザクション（`BEGIN`/`COMMIT`/
    /// `ROLLBACK`。SQL-31・TASK-221）の有無とは独立に Sync 境界そのもので
    /// 決まる契約とし、PostgreSQL の「トランザクション終了時に portal を
    /// 閉じる」契約〔PostgreSQL 34.4「Bind」〕とは意図的に異なる（codex P1
    /// 指摘・PR #1013。従来は
    /// 無名 portal のみ破棄しており、名前付き portal が Sync を越えて次回
    /// サイクルへ誤って持ち越されていた）。名前付き prepared statement
    /// （[`PreparedStatementStore`]）はこの対象外——PostgreSQL 同様、
    /// Close(Statement) または接続終了まで保持される。
    fn clear_all(&mut self) {
        self.portals.clear();
    }

    /// Close(Statement) が対象 statement から作った portal をまとめて閉じる
    /// （PostgreSQL と同じ挙動）。
    fn remove_all_for_statement(&mut self, statement_name: &str) {
        self.portals
            .retain(|_, p| p.source_statement != statement_name);
    }

    /// `exclude` 以外の全 portal が `Suspended` 状態で保持している未送出
    /// `DataRow` フレームの合計バイト数を求める（`Ready`／`Done`／`Failed`
    /// の portal は 0 として扱う）。名前付き portal は最大
    /// [`MAX_PORTALS_PER_SESSION`] 個まで同時に中断されうるため、
    /// [`crate::limits::MAX_SUSPENDED_PORTAL_BYTES_PER_SESSION`]
    /// をセッション全体（接続全体）で守るには 1 portal 単体の保持量ではなく
    /// この合計値に対して上限を適用する必要がある（PR #1013 レビュー
    /// 指摘・P0: portal 単体判定のままだと最大 64 個の portal を上限直下まで
    /// 中断させ、接続全体では上限の約 64 倍相当を保持できてしまう）。
    /// `exclude` は呼び出し元（今まさに実行結果をエンコードしている portal）
    /// を除くためで、その portal 自身の新規分は呼び出し元が別途加算する。
    fn total_suspended_bytes_excluding(&self, exclude: &str) -> usize {
        self.portals
            .iter()
            .filter(|(name, _)| name.as_str() != exclude)
            .map(|(_, portal)| match &portal.state {
                PortalState::Suspended(rows) => rows
                    .frames
                    .iter()
                    .map(Vec::len)
                    .fold(0usize, |acc, len| acc.saturating_add(len)),
                PortalState::Ready | PortalState::Done { .. } | PortalState::Failed => 0,
            })
            .fold(0usize, |acc, n| acc.saturating_add(n))
    }
}

/// 接続単位の拡張クエリプロトコル状態（`handshake` の接続ループが
/// `SessionState` と並べて所有し、接続終了で破棄——接続間・テナント間で
/// 共有しない）。
pub(crate) struct ExtendedQueryState {
    pub(crate) statements: PreparedStatementStore,
    portals: PortalStore,
    /// エラー後の同期回復モード（モジュールドキュメント参照）。`true` の間、
    /// `handshake::post_auth_loop` は Sync（'S'）・Terminate（'X'）以外の
    /// メッセージを読み捨てる。
    pub(crate) ignore_till_sync: bool,
}

impl ExtendedQueryState {
    pub(crate) fn new() -> Self {
        ExtendedQueryState {
            statements: PreparedStatementStore::new(),
            portals: PortalStore::new(),
            ignore_till_sync: false,
        }
    }

    /// 簡易クエリ（`'Q'`）処理の直前に呼ぶ。PostgreSQL は simple Query の
    /// 実行を無名 statement／無名 portal への暗黙の Parse／Bind／Execute と
    /// 同一視し、その処理時に無名 statement・無名 portal を破棄する（Cursor
    /// Bugbot Medium 指摘・PR #1013）。これを怠ると、拡張クエリプロトコルで
    /// 確立した無名 portal を挟んで simple Query を発行した直後に
    /// `Execute("")` を送るクライアントが、本来 `34000`
    /// （`HandlerError::UnknownPortal`）になるべき再実行を、simple Query
    /// 実行前の古い portal スナップショットに対して再開・再実行してしまう
    /// （commit 済み書き込みの二重実行や既に消費済みの中断行の誤配信に
    /// つながる）。名前付き statement／portal は PostgreSQL 同様に維持する。
    pub(crate) fn discard_unnamed_for_simple_query(&mut self) {
        self.statements.remove("");
        self.portals.remove_anonymous();
    }
}

/// Parse／Describe／Bind／Execute／Close 共通の分類済みエラー。
enum HandlerError {
    Body(BodyError),
    Frame(FrameError),
    Sql(engine::sql::allowlist::SqlSurfaceError),
    Store(StoreError),
    PortalStore(PortalStoreError),
    /// Parse の宣言型件数が [`MAX_PARAMS`] を超える（`54000`。WIRE-12）。
    TooManyParams,
    /// Parse の宣言型件数が SQL 中のプレースホルダ数を超える（`08P01`。
    /// PostgreSQL は受理するが、本実装は fail-closed に拒否する逸脱）。
    DeclaredParamTypesExceedPlaceholders,
    /// Bind のパラメータ format code が 0・1 以外（`08P01`）。
    InvalidParamFormatCode,
    /// 対象が未定義のステートメント名。
    UnknownStatement,
    /// 対象が未定義の portal 名。
    UnknownPortal,
    /// Bind のパラメータ数が対象ステートメントの要求数（`$n` なしは 0、
    /// `Parameterized` は最大の `$n` 番号）と一致しない。
    ParamCountMismatch,
    /// パラメータ format code の個数が 0・1・対象数のいずれでもない
    /// （結果 format code の同種エラーは [`HandlerError::BinaryFormat`] 経由）。
    FormatCodeCountMismatch,
    /// パラメータ format code が binary（1）だが、対象スロットがバイナリ非対応
    /// （text 系以外。WIRE-12・WIRE-14。結果側は [`HandlerError::BinaryFormat`]）。
    BinaryFormatUnsupported,
    /// 結果 format code の解決・事前検査で生じたエラー（WIRE-14。
    /// `result_encoder::BinaryFormatError` をそのまま分類し直したもの）。
    BinaryFormat(result_encoder::BinaryFormatError),
    /// Execute の実行結果列が Bind 時点で確定した結果列と一致しない
    /// （「cached plan must not change result type」相当の fail-closed ガード）。
    ResultTypeChanged,
    /// 中断保持する未送出行の合計バイト数が上限を超える。
    SuspendedBytesExceeded,
    /// 応答バイト列の組み立て（エンコード）・送出中の内部エラー（`XX000`
    /// 相当）。ストリーム I/O 失敗もここへ写像する（呼び出し元がこの後の
    /// 応答送出も試みるが、壊れたストリームへの追加の書き込み失敗は
    /// `post_auth_loop` まで伝播し、最終的に接続が閉じられるだけで無害）。
    Internal(String),
    /// 書き込み系文（`INSERT`/`UPDATE`/`DELETE`/`TRUNCATE` 等）の commit が
    /// 既に成功した後で発生したエラー（結果列整合検査・`DataRow` エンコード・
    /// 中断バイト上限判定・応答フレーム送出のいずれか。PR #1013 レビュー
    /// 指摘・codex P1）。分類・メッセージは内側の `HandlerError` をそのまま
    /// 使うが、応答には ERR-5・`RECOVER-5` (3) の `state=may_be_committed`
    /// detail（[`crate::error_response::MAY_BE_COMMITTED_DETAIL`]）を追加し、
    /// クライアントに「このエラー応答は書き込みの失敗を意味しない」ことを
    /// 伝える（`respond_error_and_await_sync`／`respond_error_and_close` 参照）。
    PostCommit(Box<HandlerError>),
    /// 直前の Execute 試行が失敗し `PortalState::Failed`（終端状態）へ倒れた
    /// portal への再 Execute（PR #1013 レビュー指摘・Cursor Bugbot Medium。
    /// 「Failed Execute leaves portal runnable」の是正——実行を試みたが
    /// 失敗した portal は Sync を越えても `Ready` へは戻らず、再実行するには
    /// 新しい Bind で portal を作り直す必要がある。PostgreSQL の「エラー後は
    /// トランザクションを中断し、次の有効な操作まで拒否する」契約に相当する
    /// portal 単位版）。
    PortalFailed,
}

impl HandlerError {
    fn error_class(&self) -> ErrorClass {
        match self {
            HandlerError::Body(_) => ErrorClass::ProtocolViolation,
            HandlerError::Frame(e) => e.error_class().unwrap_or(ErrorClass::ProtocolViolation),
            HandlerError::Sql(e) => e.error_class(),
            HandlerError::Store(StoreError::DuplicateName) => ErrorClass::ProtocolViolation,
            HandlerError::Store(_) => ErrorClass::PayloadTooLarge,
            HandlerError::PortalStore(PortalStoreError::DuplicateName) => {
                ErrorClass::ProtocolViolation
            }
            HandlerError::PortalStore(_) => ErrorClass::PayloadTooLarge,
            HandlerError::TooManyParams => ErrorClass::PayloadTooLarge,
            HandlerError::DeclaredParamTypesExceedPlaceholders => ErrorClass::ProtocolViolation,
            HandlerError::InvalidParamFormatCode => ErrorClass::ProtocolViolation,
            HandlerError::UnknownStatement => ErrorClass::ProtocolViolation,
            HandlerError::UnknownPortal => ErrorClass::ProtocolViolation,
            HandlerError::ParamCountMismatch => ErrorClass::ProtocolViolation,
            HandlerError::FormatCodeCountMismatch => ErrorClass::ProtocolViolation,
            HandlerError::BinaryFormatUnsupported => ErrorClass::FeatureNotSupported,
            HandlerError::BinaryFormat(e) => e.error_class(),
            HandlerError::ResultTypeChanged => ErrorClass::FeatureNotSupported,
            HandlerError::SuspendedBytesExceeded => ErrorClass::PayloadTooLarge,
            HandlerError::Internal(_) => ErrorClass::InternalError,
            HandlerError::PostCommit(inner) => inner.error_class(),
            // 未定義 statement／portal 参照と同種の「portal をこの状態で
            // 使うことはできない」というプロトコル使用エラー
            // （`UnknownPortal` と同じ分類を再利用し、新規 SQLSTATE は
            // 追加しない）。
            HandlerError::PortalFailed => ErrorClass::ProtocolViolation,
        }
    }

    fn message(&self) -> String {
        match self {
            HandlerError::Body(e) => e.message().to_string(),
            HandlerError::Frame(e) => e.client_message().to_string(),
            HandlerError::Sql(e) => e.client_message(),
            HandlerError::Store(StoreError::NameTooLong) => {
                "statement name exceeds the maximum length".to_string()
            }
            HandlerError::Store(StoreError::DuplicateName) => {
                "a prepared statement with this name already exists".to_string()
            }
            HandlerError::Store(StoreError::TooManyStatements) => {
                "too many prepared statements on this connection".to_string()
            }
            HandlerError::Store(StoreError::TotalBytesExceeded) => {
                "total prepared statement text exceeds the per-connection limit".to_string()
            }
            HandlerError::PortalStore(PortalStoreError::NameTooLong) => {
                "portal name exceeds the maximum length".to_string()
            }
            HandlerError::PortalStore(PortalStoreError::DuplicateName) => {
                "a portal with this name already exists".to_string()
            }
            HandlerError::PortalStore(PortalStoreError::TooManyPortals) => {
                "too many portals on this connection".to_string()
            }
            HandlerError::PortalStore(PortalStoreError::BoundBytesExceeded) => {
                "bound parameter values exceed the per-connection limit".to_string()
            }
            HandlerError::TooManyParams => {
                "too many parameter types declared in Parse".to_string()
            }
            HandlerError::DeclaredParamTypesExceedPlaceholders => {
                "Parse declares more parameter types than the statement has placeholders"
                    .to_string()
            }
            HandlerError::InvalidParamFormatCode => {
                "parameter format code must be 0 (text) or 1 (binary)".to_string()
            }
            HandlerError::UnknownStatement => "no such prepared statement".to_string(),
            HandlerError::UnknownPortal => "no such portal".to_string(),
            HandlerError::ParamCountMismatch => {
                "bind message supplies a wrong number of parameters".to_string()
            }
            HandlerError::FormatCodeCountMismatch => {
                "format code count does not match the parameter or result count".to_string()
            }
            HandlerError::BinaryFormatUnsupported => {
                "binary parameter format is not supported for this parameter".to_string()
            }
            HandlerError::BinaryFormat(result_encoder::BinaryFormatError::FormatCountMismatch) => {
                "format code count does not match the parameter or result count".to_string()
            }
            HandlerError::BinaryFormat(result_encoder::BinaryFormatError::InvalidFormatCode) => {
                "format code must be 0 (text) or 1 (binary)".to_string()
            }
            // 列番号のみを含め、テーブル名・値そのものは含めない
            // （`.claude/rules/security.md`。他テナントの存在情報を漏らさない）。
            HandlerError::BinaryFormat(result_encoder::BinaryFormatError::UnsupportedType {
                column_index,
            }) => {
                format!("column {column_index} does not support binary format")
            }
            HandlerError::ResultTypeChanged => {
                "cached plan must not change result type".to_string()
            }
            HandlerError::SuspendedBytesExceeded => {
                "suspended portal row buffer exceeds the per-connection limit".to_string()
            }
            HandlerError::Internal(detail) => detail.clone(),
            HandlerError::PostCommit(inner) => inner.message(),
            HandlerError::PortalFailed => {
                "portal is in a failed state after a prior execution error; re-bind before executing again"
                    .to_string()
            }
        }
    }
}

impl From<BodyError> for HandlerError {
    fn from(e: BodyError) -> Self {
        HandlerError::Body(e)
    }
}

fn internal_error(detail: &str) -> HandlerError {
    HandlerError::Internal(detail.to_string())
}

fn io_to_handler(e: io::Error) -> HandlerError {
    internal_error(&format!("stream I/O error: {e}"))
}

/// `err` から `ErrorResponse`（'E'）フレームを組み立てる。`err` が
/// [`HandlerError::PostCommit`] を再帰的に含む場合、ERR-5・`RECOVER-5` (3)
/// の `state=may_be_committed` detail を付ける（[`error_response::
/// encode_with_detail`]・既存の panic 緊急応答経路が使う定数・関数をそのまま
/// 再利用し、新規 SQLSTATE を追加しない）。それ以外は通常応答
/// （`result_encoder::encode_error_response`。本モジュールの既存経路）。
fn build_error_response_body(err: &HandlerError) -> Result<Vec<u8>, result_encoder::EncodeError> {
    if let HandlerError::PostCommit(inner) = err {
        return error_response::encode_with_detail(
            inner.error_class(),
            &inner.message(),
            error_response::MAY_BE_COMMITTED_DETAIL,
        );
    }
    result_encoder::encode_error_response(err.error_class().wire_code(), &err.message())
}

/// ErrorResponse を送出し、フレーム自体が壊れており同期を回復できない
/// （モジュールドキュメント「エラー後の同期回復」節）場合に、有界
/// lingering close で接続を終える。
fn respond_error_and_close<S: WireStream>(stream: &mut S, err: &HandlerError) -> io::Result<()> {
    engine::log_stderr!(
        "wire-server: extended query rejecting message ({})",
        err.error_class().wire_code()
    );
    let body = build_error_response_body(err).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "failed to encode ErrorResponse")
    })?;
    stream.write_all(&body)?;
    stream.flush()?;
    crate::protocol_dispatch::drain_and_close(
        stream,
        crate::protocol_dispatch::LINGER_DRAIN_TIMEOUT,
        crate::protocol_dispatch::LINGER_DRAIN_MAX_BYTES,
    );
    Ok(())
}

/// ErrorResponse を送出したうえで [`ExtendedQueryState::ignore_till_sync`] を
/// 立てる（モジュールドキュメント「エラー後の同期回復」節）。接続は維持する。
///
/// 拡張クエリプロトコルのエラー応答（Parse・Bind・Describe・Execute・Close 等）は
/// すべて本関数を通る。`handshake::post_auth_loop` は各メッセージの処理後（と次の
/// メッセージの受信直後）に `ignore_till_sync` を見て、明示トランザクションが
/// `Active` なら `Failed` へ遷移させる（SQL-31・TASK-221。PR #1041 レビュー指摘）。
/// 各ハンドラは `SessionTransaction` を受け取らないため、この旗が唯一の受け渡し
/// 経路になる。エラー応答を本関数以外の方法で返す経路を追加してはならない
/// （切断する経路は `respond_error_and_close`。切断で `SessionTransaction` が
/// drop され、書き込みトランザクションは abort される）。
fn respond_error_and_await_sync<S: WireStream>(
    stream: &mut S,
    err: &HandlerError,
    state: &mut ExtendedQueryState,
) -> io::Result<()> {
    engine::log_stderr!(
        "wire-server: extended query error, awaiting Sync ({})",
        err.error_class().wire_code()
    );
    let body = build_error_response_body(err).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "failed to encode ErrorResponse")
    })?;
    stream.write_all(&body)?;
    stream.flush()?;
    state.ignore_till_sync = true;
    Ok(())
}

fn io_error_from_frame(e: FrameError) -> io::Error {
    match e {
        FrameError::Io(io_err) => io_err,
        other => io::Error::new(io::ErrorKind::InvalidData, other.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Parse（'P'）
// ---------------------------------------------------------------------------

/// Parse（'P'）を処理する（`engine` が接続済みの場合のみ呼ばれる。`handshake::
/// post_auth_loop` 参照）。SQL は簡易クエリと同一の許可リスト
/// （[`EngineCore::parse_sql`]）で検証してから保持する（検証失敗なら保持しない）。
pub(crate) fn handle_parse<S: WireStream>(
    stream: &mut S,
    engine: &EngineCore,
    txn: &mut engine::sql::transaction::SessionTransaction<'_>,
    state: &mut ExtendedQueryState,
) -> io::Result<LoopSignal> {
    let body = match framing::read_length_prefixed_body(
        stream,
        MIN_PARSE_BODY_LEN,
        framing::MAX_MESSAGE_LEN,
    ) {
        Ok(b) => b,
        Err(FrameError::Truncated) => return Ok(LoopSignal::Closed),
        Err(e @ FrameError::Io(_)) => return Err(io_error_from_frame(e)),
        Err(e) => {
            respond_error_and_close(stream, &HandlerError::Frame(e))?;
            return Ok(LoopSignal::Closed);
        }
    };

    match handle_parse_body(engine, txn, &mut state.statements, &body) {
        Ok(()) => {
            stream.write_all(&result_encoder::encode_parse_complete())?;
            stream.flush()?;
            Ok(LoopSignal::Continue)
        }
        Err(err) => {
            respond_error_and_await_sync(stream, &err, state)?;
            Ok(LoopSignal::Continue)
        }
    }
}

fn handle_parse_body(
    engine: &EngineCore,
    txn: &mut engine::sql::transaction::SessionTransaction<'_>,
    store: &mut PreparedStatementStore,
    body: &[u8],
) -> Result<(), HandlerError> {
    // フレーム本体の構造検証（`08P01`）だけを先に行う。
    let msg = parse_parse_body(body)?;
    // 明示トランザクションが `Failed` の間は、`ROLLBACK` と空文字列以外を、
    // パラメータ型 OID 指定の未対応（`0A000`）等の機能検証や parse より前に
    // `25P02` で拒否する（SQL-31・TASK-221。簡易クエリの
    // `EngineCore::execute_sql_in_txn` と同じ判定順序。PR #1041 レビュー指摘）。
    if txn.status() == engine::sql::transaction::TransactionStatus::Failed
        && !msg.query.trim().is_empty()
        && !engine::sql::transaction::is_rollback_statement(&msg.query)
    {
        return Err(HandlerError::Sql(txn.take_failed_error()));
    }
    // 宣言型件数の上限検証は OID 列の復号・アロケーションより前に行う。
    if msg.num_param_types > usize::from(MAX_PARAMS) {
        return Err(HandlerError::TooManyParams);
    }

    let statement = if msg.query.trim().is_empty() {
        PreparedStatement::Empty
    } else {
        let prepared = engine
            .parse_sql_prepared(&msg.query)
            .map_err(HandlerError::Sql)?;
        match prepared.parsed_if_unparameterized() {
            // `$n` を含まない文は従来どおり Parsed 経路のまま保持する
            // （二重 parse なし・挙動不変）。宣言型があれば過剰宣言。
            Some(parsed) => {
                if msg.num_param_types > 0 {
                    return Err(HandlerError::DeclaredParamTypesExceedPlaceholders);
                }
                PreparedStatement::Parsed(parsed.clone())
            }
            None => {
                let declared = decode_declared_oids(&msg.param_oid_bytes);
                let slots = build_param_slots(&prepared, &declared)?;
                PreparedStatement::Parameterized { prepared, slots }
            }
        }
    };

    store
        .insert(msg.name, msg.query.len(), statement)
        .map_err(HandlerError::Store)
}

/// `$n` ごとの公告 OID・バイナリ可否を組み立てる（WIRE-12）。宣言 OID は
/// 値の検証には一切使わない（値は常にリテラル置換経路を通るため、宣言型で
/// 検証を迂回できない）。0 以外はそのまま echo し、0・未宣言は推論値で補う。
/// バイナリは、推論がバイナリ対応かつ宣言 OID が text 系（0・25・1043）の
/// ときだけ許す。
fn build_param_slots(
    prepared: &PreparedSql,
    declared: &[i32],
) -> Result<Vec<ParamSlot>, HandlerError> {
    let types = prepared.param_types();
    if declared.len() > types.len() {
        return Err(HandlerError::DeclaredParamTypesExceedPlaceholders);
    }
    let slots = types
        .iter()
        .enumerate()
        .map(|(i, ty)| {
            let (inferred_oid, inferred_binary) = match ty {
                PreparedParamType::Text => (result_encoder::WireType::Text.oid(), true),
                PreparedParamType::VectorText => (result_encoder::WireType::Text.oid(), false),
                PreparedParamType::Column(meta) => (
                    result_encoder::column_wire_type(meta).oid(),
                    result_encoder::column_binary_support(meta),
                ),
            };
            let declared_oid = declared.get(i).copied().unwrap_or(0);
            ParamSlot {
                oid: if declared_oid != 0 {
                    declared_oid
                } else {
                    inferred_oid
                },
                binary: inferred_binary && TEXT_LIKE_DECLARED_OIDS.contains(&declared_oid),
            }
        })
        .collect();
    Ok(slots)
}

// ---------------------------------------------------------------------------
// Describe（'D'。statement・portal 両対象）
// ---------------------------------------------------------------------------

enum DescribeResult {
    Statement(Option<Vec<ColumnMeta>>, Vec<i32>),
    /// portal 対象。`Bind` が確定した結果 format code（WIRE-14）を
    /// `RowDescription` の各列の format code フィールドへそのまま反映する
    /// （Execute の `DataRow` と同じ値を参照——Bind 時点で確定した形式を
    /// Execute まで一貫させる契約）。
    Portal(Option<Vec<ColumnMeta>>, Vec<result_encoder::FormatCode>),
}

/// Describe（'D'）を処理する（`engine` が接続済みの場合のみ呼ばれる）。
/// statement 対象は `ParameterDescription`（`$n` の公告 OID。WIRE-12。
/// `$n` なしは 0 件）と `RowDescription`（結果列なしは `NoData`）を返す。portal 対象は
/// `ParameterDescription` を返さず `RowDescription`／`NoData` のみ返す
/// （PostgreSQL の規約）。
pub(crate) fn handle_describe<S: WireStream>(
    stream: &mut S,
    engine: &EngineCore,
    session: &SessionState,
    txn: &engine::sql::transaction::SessionTransaction<'_>,
    state: &mut ExtendedQueryState,
) -> io::Result<LoopSignal> {
    let body = match framing::read_length_prefixed_body(
        stream,
        MIN_DESCRIBE_BODY_LEN,
        framing::MAX_MESSAGE_LEN,
    ) {
        Ok(b) => b,
        Err(FrameError::Truncated) => return Ok(LoopSignal::Closed),
        Err(e @ FrameError::Io(_)) => return Err(io_error_from_frame(e)),
        Err(e) => {
            respond_error_and_close(stream, &HandlerError::Frame(e))?;
            return Ok(LoopSignal::Closed);
        }
    };

    match handle_describe_body(engine, session, txn, state, &body) {
        Ok(DescribeResult::Statement(columns, param_oids)) => {
            write_describe_response(stream, &param_oids, columns)?;
            Ok(LoopSignal::Continue)
        }
        Ok(DescribeResult::Portal(columns, formats)) => {
            write_describe_response_portal(stream, columns, &formats)?;
            Ok(LoopSignal::Continue)
        }
        Err(err) => {
            respond_error_and_await_sync(stream, &err, state)?;
            Ok(LoopSignal::Continue)
        }
    }
}

fn write_describe_response<S: WireStream>(
    stream: &mut S,
    param_oids: &[i32],
    columns: Option<Vec<ColumnMeta>>,
) -> io::Result<()> {
    stream.write_all(
        &result_encoder::encode_parameter_description(param_oids).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "failed to encode ParameterDescription",
            )
        })?,
    )?;
    match columns {
        Some(columns) => {
            let row_description =
                result_encoder::encode_row_description(&columns).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "failed to encode RowDescription",
                    )
                })?;
            stream.write_all(&row_description)?;
        }
        None => {
            stream.write_all(&result_encoder::encode_no_data())?;
        }
    }
    stream.flush()
}

/// Describe(Portal) 応答（WIRE-14）。portal 対象は `ParameterDescription` を
/// 返さず（[`write_describe_response`] と異なり常に `false` 相当）、
/// `RowDescription` の format code は Bind 時点で確定した `formats`
/// （[`Portal::result_formats`]）をそのまま反映する。
fn write_describe_response_portal<S: WireStream>(
    stream: &mut S,
    columns: Option<Vec<ColumnMeta>>,
    formats: &[result_encoder::FormatCode],
) -> io::Result<()> {
    match columns {
        Some(columns) => {
            let row_description = result_encoder::encode_row_description_with_formats(
                &columns, formats,
            )
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "failed to encode RowDescription",
                )
            })?;
            stream.write_all(&row_description)?;
        }
        None => {
            stream.write_all(&result_encoder::encode_no_data())?;
        }
    }
    stream.flush()
}

fn handle_describe_body(
    engine: &EngineCore,
    session: &SessionState,
    txn: &engine::sql::transaction::SessionTransaction<'_>,
    state: &ExtendedQueryState,
    body: &[u8],
) -> Result<DescribeResult, HandlerError> {
    let msg = parse_describe_body(body)?;
    match msg.target {
        TargetKind::Statement => {
            let statement = state
                .statements
                .get(&msg.name)
                .ok_or(HandlerError::UnknownStatement)?;
            let mut param_oids: Vec<i32> = Vec::new();
            let columns = match statement {
                PreparedStatement::Empty => None,
                // WIRE-12: 値未確定のままダミー束縛済みの結果列を返し、
                // `ParameterDescription` には Parse が確定した公告 OID を返す。
                PreparedStatement::Parameterized { prepared, slots } => {
                    param_oids = slots.iter().map(|s| s.oid).collect();
                    engine
                        .describe_prepared_in_session(session, prepared)
                        .map_err(HandlerError::Sql)?
                }
                // WIRE-15・TASK-218: `describe_parsed_in_txn` へ切り替え、
                // `ParsedSql::Cursor(CursorStatement::Fetch { .. })` に限り
                // `txn` が保持する開いているカーソルの列メタデータを返す
                // （それ以外の `ParsedSql` は `describe_parsed_in_session` と
                // 完全に同一の判定へ委譲する）。
                PreparedStatement::Parsed(parsed) => engine
                    .describe_parsed_in_txn(session, txn, parsed)
                    .map_err(HandlerError::Sql)?,
            };
            Ok(DescribeResult::Statement(columns, param_oids))
        }
        TargetKind::Portal => {
            let portal = state
                .portals
                .get(&msg.name)
                .ok_or(HandlerError::UnknownPortal)?;
            Ok(DescribeResult::Portal(
                portal.columns.clone(),
                portal.result_formats.clone(),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Bind（'B'）
// ---------------------------------------------------------------------------

/// Bind のパラメータ format code 列を検証する（PostgreSQL の規約: 件数は
/// 0（すべて既定＝text）・1（すべてこの 1 個の値に従う）・対象数と同数の
/// いずれかでなければならない）。各値は 0（text）・1（binary）のみ許可し、
/// それ以外は `08P01`。binary のスロット別受理可否は呼び出し元が
/// [`ParamSlot`] で判定する（結果 format code は WIRE-14・
/// [`result_encoder::ResultFormats::resolve`]／[`result_encoder::
/// validate_binary_formats`] が別途扱う）。
fn validate_format_codes(codes: &[i16], target_count: usize) -> Result<(), HandlerError> {
    if !codes.is_empty() && codes.len() != 1 && codes.len() != target_count {
        return Err(HandlerError::FormatCodeCountMismatch);
    }
    if codes.iter().any(|&c| c != 0 && c != 1) {
        return Err(HandlerError::InvalidParamFormatCode);
    }
    Ok(())
}

/// Bind（'B'）を処理する（`engine` が接続済みの場合のみ呼ばれる）。対象
/// ステートメントを Describe 相当（`describe_parsed_in_session`）して結果列を
/// 確定し、portal として保持する（Bind 時点のスナップショット。モジュール
/// ドキュメント「portal のライフサイクル」節参照）。
pub(crate) fn handle_bind<S: WireStream>(
    stream: &mut S,
    engine: &EngineCore,
    session: &SessionState,
    txn: &mut engine::sql::transaction::SessionTransaction<'_>,
    state: &mut ExtendedQueryState,
) -> io::Result<LoopSignal> {
    let body = match framing::read_length_prefixed_body(
        stream,
        MIN_BIND_BODY_LEN,
        framing::MAX_MESSAGE_LEN,
    ) {
        Ok(b) => b,
        Err(FrameError::Truncated) => return Ok(LoopSignal::Closed),
        Err(e @ FrameError::Io(_)) => return Err(io_error_from_frame(e)),
        Err(e) => {
            respond_error_and_close(stream, &HandlerError::Frame(e))?;
            return Ok(LoopSignal::Closed);
        }
    };

    match handle_bind_body(engine, session, txn, state, &body) {
        Ok(()) => {
            stream.write_all(&result_encoder::encode_bind_complete())?;
            stream.flush()?;
            Ok(LoopSignal::Continue)
        }
        Err(err) => {
            respond_error_and_await_sync(stream, &err, state)?;
            Ok(LoopSignal::Continue)
        }
    }
}

fn handle_bind_body(
    engine: &EngineCore,
    session: &SessionState,
    txn: &mut engine::sql::transaction::SessionTransaction<'_>,
    state: &mut ExtendedQueryState,
    body: &[u8],
) -> Result<(), HandlerError> {
    // フレーム本体の構造検証（`08P01`）だけを先に行う。
    let msg = parse_bind_body(body)?;

    // 明示トランザクションが `Failed` の間は、`ROLLBACK` と空文字列以外の
    // ステートメントの Bind を `25P02` で拒否する（`Failed` になる前に Parse 済みの
    // ステートメントを含む。SQL-31・TASK-221）。format code の検証や未登録
    // ステートメントの判定より先に行う（PR #1041 レビュー指摘:
    // 機能・対象の検証が先行すると `Failed` 中でも `25P02` 以外が返る）。
    if txn.status() == engine::sql::transaction::TransactionStatus::Failed {
        let is_exit_or_empty = matches!(
            state.statements.get(&msg.statement_name),
            Some(
                PreparedStatement::Empty
                    | PreparedStatement::Parsed(ParsedSql::Transaction(
                        engine::sql::transaction::TxnControl::Rollback
                    ))
            )
        );
        if !is_exit_or_empty {
            return Err(HandlerError::Sql(txn.take_failed_error()));
        }
    }

    let num_params = msg.param_values.len();
    validate_format_codes(&msg.param_format_codes, num_params)?;

    let statement = state
        .statements
        .get(&msg.statement_name)
        .ok_or(HandlerError::UnknownStatement)?;

    // Bind が送ってきた実パラメータ数は、ステートメントが要求する数
    // （`$n` なしは 0、`Parameterized` は最大の `$n` 番号）と一致しなければ
    // ならない（`08P01`）。
    let required = match statement {
        PreparedStatement::Parameterized { slots, .. } => slots.len(),
        PreparedStatement::Empty | PreparedStatement::Parsed(_) => 0,
    };
    if num_params != required {
        return Err(HandlerError::ParamCountMismatch);
    }

    let mut bound_bytes = 0usize;
    let (portal_body, columns) = match statement {
        PreparedStatement::Empty => (PortalBody::Empty, None),
        PreparedStatement::Parameterized { prepared, slots } => {
            // 値ごとの format を解決する（0 件は全 text・1 件は全値へ適用・
            // n 件は個別。件数の整合は `validate_format_codes` が検証済み）。
            // binary は text 系スロットのみ受理し、UTF-8 バイト恒等でそのまま
            // engine へ渡す（それ以外は `0A000`）。
            for (i, slot) in slots.iter().enumerate() {
                let code = match msg.param_format_codes.as_slice() {
                    [] => 0,
                    [one] => *one,
                    many => many.get(i).copied().unwrap_or(0),
                };
                if code == 1 && !slot.binary {
                    return Err(HandlerError::BinaryFormatUnsupported);
                }
            }
            for v in msg.param_values.iter().flatten() {
                bound_bytes = bound_bytes.saturating_add(v.len());
            }
            let bound = engine
                .bind_prepared(prepared, &msg.param_values)
                .map_err(HandlerError::Sql)?;
            let columns = engine
                .describe_parsed_in_txn(session, txn, &bound)
                .map_err(HandlerError::Sql)?;
            (PortalBody::Parsed(bound), columns)
        }
        PreparedStatement::Parsed(parsed) => {
            let parsed = parsed.clone();
            // WIRE-15・TASK-218: `describe_parsed_in_txn` へ切り替える
            // （`handle_describe_body` と同じ理由。`FETCH` の portal 列メタは
            // Bind 時点で開いているカーソルの列を反映する）。
            let columns = engine
                .describe_parsed_in_txn(session, txn, &parsed)
                .map_err(HandlerError::Sql)?;
            (PortalBody::Parsed(parsed), columns)
        }
    };

    // 結果 format code の解決・事前検査（WIRE-14）。列ごとの
    // `FormatCode`（`Text`／`Binary`）へ解決したうえで、binary 指定列が
    // すべて対応型（`TEXT`）であることを `RowDescription` 送出前に確定する
    // （`result_encoder` モジュールドキュメント参照）。結果列なしの
    // statement（`columns` が `None`。`expected_cols == 0`）では
    // `validate_binary_formats` を呼ばない——列が 0 なので形式指定は常に
    // 無害（PostgreSQL も同様に無視する）。
    let expected_cols = columns.as_ref().map(Vec::len).unwrap_or(0);
    let result_formats = result_encoder::ResultFormats::new(&msg.result_format_codes)
        .resolve(expected_cols)
        .map_err(HandlerError::BinaryFormat)?;
    if let Some(cols) = columns.as_ref() {
        result_encoder::validate_binary_formats(cols, &result_formats)
            .map_err(HandlerError::BinaryFormat)?;
    }

    let portal = Portal {
        source_statement: msg.statement_name,
        body: portal_body,
        columns,
        result_formats,
        state: PortalState::Ready,
        cursor_binding: None,
        bound_bytes,
    };

    state
        .portals
        .insert(msg.portal_name, portal)
        .map_err(HandlerError::PortalStore)
}

// ---------------------------------------------------------------------------
// Execute（'E'）
// ---------------------------------------------------------------------------

/// Execute（'E'）を処理する（`engine` が接続済みの場合のみ呼ばれる）。portal
/// の状態（[`PortalState`]）に応じて実行・分割送出・再利用を行う
/// （モジュールドキュメント「portal のライフサイクル」節参照）。`txn` は
/// 接続単位の [`engine::sql::transaction::SessionTransaction`]（`handshake::
/// post_auth_loop` が簡易クエリと共有して保持する同一の値。SQL-31・
/// TASK-221・Issue #942 codex-review 指摘対応: 以前は本関数が
/// `execute_parsed_in_session`（autocommit 専用）を呼んでいたため、拡張
/// クエリプロトコル経由の `BEGIN` は `Idle` から進めず常に `0A000`
/// （`transaction_feature_not_supported`）で拒否されていた。`execute_parsed_
/// in_txn` へ切り替え、簡易クエリと同じ状態機械を共有する）。
pub(crate) fn handle_execute<'e, S: WireStream>(
    stream: &mut S,
    engine: &'e EngineCore,
    ctx: &engine::policy::PolicyContext,
    session: &mut SessionState,
    txn: &mut engine::sql::transaction::SessionTransaction<'e>,
    state: &mut ExtendedQueryState,
) -> io::Result<LoopSignal> {
    // commit 成功から本関数が応答を書き終えるまでの区間全体を覆う RAII ガード
    // （RECOVER-5 (3)・`simple_query::execute_and_respond` と同じ保護区間の
    // 取り方）。Execute は `engine::execute_parsed_in_txn` を通じて書き込み系
    // 文（`INSERT`/`UPDATE`/`DELETE`/`TRUNCATE`/`COMMIT` 等）の commit も
    // 実行しうるため、簡易クエリと同様にこのガードが無いと commit 成功後
    // panic した際の緊急応答（`recovery::panic_hook`・TASK-97・RECOVER-6）が
    // 発火しない（PR #1013 レビュー指摘・cursor High: ResponseBoundaryGuard
    // 欠落）。`must_use` のため名前付き束縛のまま関数末尾まで保持する。
    let _response_boundary = ResponseBoundaryGuard::new();

    let body = match framing::read_length_prefixed_body(
        stream,
        MIN_EXECUTE_BODY_LEN,
        framing::MAX_MESSAGE_LEN,
    ) {
        Ok(b) => b,
        Err(FrameError::Truncated) => return Ok(LoopSignal::Closed),
        Err(e @ FrameError::Io(_)) => return Err(io_error_from_frame(e)),
        Err(e) => {
            respond_error_and_close(stream, &HandlerError::Frame(e))?;
            return Ok(LoopSignal::Closed);
        }
    };

    let msg = match parse_execute_body(&body) {
        Ok(m) => m,
        Err(e) => {
            respond_error_and_await_sync(stream, &HandlerError::Body(e), state)?;
            return Ok(LoopSignal::Continue);
        }
    };

    match execute_portal(
        stream,
        engine,
        ctx,
        session,
        txn,
        state,
        &msg.portal_name,
        msg.max_rows,
    ) {
        Ok(()) => Ok(LoopSignal::Continue),
        Err(err) => {
            respond_error_and_await_sync(stream, &err, state)?;
            Ok(LoopSignal::Continue)
        }
    }
}

fn write_command_complete<S: WireStream>(stream: &mut S, tag: &str) -> Result<(), HandlerError> {
    let msg = result_encoder::encode_command_complete(tag)
        .map_err(|_| internal_error("failed to encode command complete"))?;
    stream.write_all(&msg).map_err(io_to_handler)?;
    stream.flush().map_err(io_to_handler)
}

/// 明示トランザクションが `Failed` の間も Execute を受理する portal か
/// （SQL-31・TASK-221）。空文字列の portal（副作用なし）と、未実行（`Ready`）の
/// `ROLLBACK` の portal だけが対象で、未登録の portal は含まない。
fn portal_is_exempt_while_failed(state: &ExtendedQueryState, portal_name: &str) -> bool {
    match state.portals.get(portal_name) {
        Some(portal) => match &portal.body {
            PortalBody::Empty => true,
            PortalBody::Parsed(ParsedSql::Transaction(
                engine::sql::transaction::TxnControl::Rollback,
            )) => matches!(portal.state, PortalState::Ready),
            PortalBody::Parsed(_) => false,
        },
        None => false,
    }
}

/// Execute 本体（`crate::simple_query::execute_with_emergency_registration` と
/// `map_outcome`/`TagShape` を再利用し、簡易クエリの `run_statement` と同じ
/// 緊急応答登録位置・タグ組み立て規則を共有する。WIRE-11: 第 2 の実行器を
/// 作らない）。`txn` は [`handle_execute`] が受け取った接続単位の
/// `SessionTransaction` をそのまま引き継ぎ、`engine::execute_parsed_in_txn`
/// （SQL-31・TASK-221）へ渡す。
///
/// `txn` 追加で `clippy::too_many_arguments`（閾値 7）を超える
/// （`sql/exec.rs`・`tenant.rs` 等、既存の同種箇所と同じ対応）。
#[allow(clippy::too_many_arguments)]
fn execute_portal<'e, S: WireStream>(
    stream: &mut S,
    engine: &'e EngineCore,
    ctx: &engine::policy::PolicyContext,
    session: &mut SessionState,
    txn: &mut engine::sql::transaction::SessionTransaction<'e>,
    state: &mut ExtendedQueryState,
    portal_name: &str,
    max_rows: i32,
) -> Result<(), HandlerError> {
    // 明示トランザクションが `Failed`（エラーによる abort・持続時間上限による
    // 解放後）の間は、`ROLLBACK`（未実行の `Ready`）と空文字列の portal 以外への
    // Execute を、portal の存在確認や実行状態に依らず最初に `25P02`（期限切れの
    // 未報告分があれば 1 回だけ `54000`）で拒否する（SQL-31・TASK-221。PR #1041
    // レビュー指摘: 未登録 portal の判定や、実行を開始済みの portal
    // 〔`Suspended`・`Done`〕の残り行送出・タグ再送が状態検査より先に行われると、
    // abort 後の要求が成功に見え期限切れも報告されない）。PostgreSQL が
    // トランザクション終了時に portal を破棄するのに合わせ、拒否した portal は
    // 終端状態 `Failed` へ倒し、`ROLLBACK` 後も再開させない。
    if txn.status() == engine::sql::transaction::TransactionStatus::Failed
        && !portal_is_exempt_while_failed(state, portal_name)
    {
        if let Some(portal) = state.portals.get_mut(portal_name) {
            portal.state = PortalState::Failed;
        }
        return Err(HandlerError::Sql(txn.take_failed_error()));
    }

    // Empty body（空文字列に対する Bind から作られた portal）は状態遷移を
    // 持たず、常に `EmptyQueryResponse` を返す（副作用が無いため何度
    // Execute しても安全に冪等。PostgreSQL と同じ扱い）。
    let is_empty = matches!(
        state
            .portals
            .get(portal_name)
            .ok_or(HandlerError::UnknownPortal)?
            .body,
        PortalBody::Empty
    );
    if is_empty {
        stream
            .write_all(&result_encoder::encode_empty_query_response())
            .map_err(io_to_handler)?;
        stream.flush().map_err(io_to_handler)?;
        return Ok(());
    }

    let needs_execution = matches!(
        state
            .portals
            .get(portal_name)
            .ok_or(HandlerError::UnknownPortal)?
            .state,
        PortalState::Ready
    );

    if needs_execution {
        let (parsed, expected_columns, result_formats) = {
            let portal = state
                .portals
                .get(portal_name)
                .ok_or(HandlerError::UnknownPortal)?;
            let parsed = match &portal.body {
                PortalBody::Parsed(parsed) => parsed.clone(),
                // 呼び出し元が関数冒頭で `Empty` を判定・早期 return 済みの
                // はずだが、内部状態機械の不変条件が将来の変更で崩れた場合に
                // 備えて panic ではなく fail-closed な内部エラーへ倒す
                // （wire 入力経路で panic を避ける方針。`.claude/rules/
                // coding-rust.md`）。
                PortalBody::Empty => {
                    return Err(internal_error(
                        "portal state machine invariant violated: Empty body reached execution",
                    ))
                }
            };
            (
                parsed,
                portal.columns.clone(),
                portal.result_formats.clone(),
            )
        };

        // `engine::sql::allowlist::Statement`（`ParsedSql::Statement`）は
        // `SELECT`／`SET search_mode`／`CREATE FUNCTION`／`EXPLAIN` のみで、
        // いずれも redb への書き込み commit を伴わない（`CREATE FUNCTION`・
        // `SET` はセッションローカルな状態変更のみ）。`ParsedSql::Transaction`
        // （SQL-31・TASK-221。`BEGIN`/`COMMIT`/`ROLLBACK`）は `COMMIT` のみが
        // 実際に redb commit を伴い、`BEGIN`/`ROLLBACK` は伴わない。それ以外の
        // `ParsedSql`（`Insert`/`Truncate`/`Delete`/`Update`）は、明示
        // トランザクションが `Active`（`txn.is_active()`）でない限り
        // `execute_parsed_in_txn` が `Ok` を返した時点で commit 成功が確定
        // している（`RECOVER-5`「commit 成功境界」契約——`Err` を返す経路は
        // commit 未到達のまま失敗する設計のため、`Ok` は必ず commit 成功を
        // 意味する）。`Active` の間は書き込みが `write_txn` へ溜まるだけで
        // `COMMIT` まで commit されない（`sql::transaction` モジュール
        // ドキュメント参照）ため、この場合は false とする。判定は呼び出し前の
        // `txn.is_active()`（`execute_parsed_in_txn` 自体が `Active`/`Idle`
        // 間で状態遷移するため、呼び出し後の値を使うと `BEGIN`/`COMMIT`
        // 自身の判定が壊れる）を使う。この判定は Execute 呼び出しをまたいで
        // 使うため（`PortalRows::committed` 経由で中断保持継続時にも
        // 引き継ぐ）、ここで一度だけ確定する（codex-review 指摘・Issue #942:
        // 拡張クエリプロトコルを `execute_parsed_in_txn` へ接続した際に
        // 追加した分岐。以前は `ParsedSql::Statement` 以外を一律 `true` と
        // 判定していたが、当時は明示トランザクションが無かったため
        // `Insert`/`Truncate`/`Delete`/`Update` は常に単独 commit で成立し
        // 問題にならなかった）。
        let was_active_before_execution = txn.is_active();
        let is_write_statement = match &parsed {
            ParsedSql::Statement(_) => false,
            ParsedSql::Transaction(engine::sql::transaction::TxnControl::Commit) => true,
            ParsedSql::Transaction(_) => false,
            _ => !was_active_before_execution,
        };

        // 実行を試みる時点で portal を `Failed`（終端状態）へ倒しておく
        // （成功時のみ末尾で正しい状態へ上書きする）。`execute_parsed_in_txn`
        // 自体がエラーを返す経路も含め、実行を試みた portal は Sync を越えても
        // `Ready` に留まらず、再実行するには新しい Bind で portal を作り直す
        // 必要がある——PostgreSQL の「エラー後はトランザクションを中断する」
        // 契約に相当する portal 単位版（PR #1013 レビュー指摘・Cursor Bugbot
        // Medium「Failed Execute leaves portal runnable」の是正。以前は
        // `execute_parsed_in_session` 成功後にのみ `Failed` へ倒していたため、
        // その呼び出し自体が失敗した場合は portal が `Ready` のまま残っていた）。
        {
            let portal = state
                .portals
                .get_mut(portal_name)
                .ok_or(HandlerError::UnknownPortal)?;
            portal.state = PortalState::Failed;
        }

        let outcome = crate::simple_query::execute_with_emergency_registration(stream, || {
            engine.execute_parsed_in_txn(ctx, session, txn, &parsed)
        })
        .map_err(HandlerError::Sql)?;

        // `parsed` がカーソル `FETCH` なら、実行が成功したこの時点（`txn` は
        // 実行後の状態）のトランザクション世代・カーソル個体識別子を portal へ
        // 束縛する（PR #1049 レビュー指摘対応。cursor Bugbot P1「終了した
        // カーソルの FETCH portal から行を送出できる」の是正）。`FETCH` 以外は
        // 常に `None`（この検証の対象外。既存の挙動を変えない）。中断保持
        // （`PortalState::Suspended`）へ回った場合はこの束縛を再送出前の
        // 検証に使い、`CLOSE`／`COMMIT`／`ROLLBACK`／再 `DECLARE` を挟んで
        // いれば以降の再 Execute を拒否する（本関数冒頭の検証ブロック参照）。
        let cursor_binding = match &parsed {
            ParsedSql::Cursor(engine::sql::cursor::CursorStatement::Fetch { name, .. }) => {
                txn.active_generation().and_then(|txn_generation| {
                    txn.cursor_id(name).map(|cursor_id| CursorBinding {
                        cursor_name: name.clone(),
                        txn_generation,
                        cursor_id,
                    })
                })
            }
            _ => None,
        };

        // ここに到達した時点で `is_write_statement` なら commit は既に成功
        // している（上のコメント参照）。これ以降（結果列整合検査・行
        // エンコード・中断バイト上限判定・応答フレーム送出）で発生する失敗は
        // すべて「commit 成功後の後処理失敗」であり、`state=may_be_committed`
        // detail 付きの `HandlerError::PostCommit` へ包んでクライアントへ
        // 誤解を与えない（PR #1013 レビュー指摘・codex P1。ERR-5・
        // `RECOVER-5` (3) の既存 detail 契約を、panic 経由の緊急応答だけで
        // なく通常の Err 経路にも拡張適用する）。
        let commit_wrap = |e: HandlerError| -> HandlerError {
            if is_write_statement {
                HandlerError::PostCommit(Box::new(e))
            } else {
                e
            }
        };

        // PR #1049 レビュー指摘 codex P1 対応: カーソル `FETCH` はエンジン実行が
        // 成功した時点（上の `execute_parsed_in_txn`）で取得位置を進める。以降の
        // 応答生成（行エンコード・中断保持バイト上限・送出）が失敗した場合、
        // 位置だけが進んで行がクライアントへ届かないまま次の `FETCH` が続きを
        // 返すことがあってはならない。ここで明示トランザクションを直ちに
        // `Failed` へ遷移させ（カーソルは `ActiveTxn` とともに破棄される）、
        // 以降の `FETCH` は `25P02` になる——PostgreSQL の「エラーでトランザク
        // ションを中断する」契約（SQL-31）と同じで、行を飛ばした状態から再開
        // する経路を持たない。`handshake::post_auth_loop` もエラー応答後に同じ
        // 遷移を行う（`respond_error_and_await_sync` 参照）が、その受け渡しに
        // 依存せずこの失敗箇所で局所的に保証する。`fail` は `Active` 以外では
        // 何もしない（冪等）。
        let is_cursor_fetch = cursor_binding.is_some();
        let response = (|| -> Result<(), HandlerError> {
            match crate::simple_query::map_outcome(outcome) {
                crate::simple_query::OutcomeResponse::Command { tag } => {
                    let portal = state
                        .portals
                        .get_mut(portal_name)
                        .ok_or(HandlerError::UnknownPortal)?;
                    portal.state = PortalState::Done { tag: tag.clone() };
                    portal.cursor_binding = cursor_binding;
                    write_command_complete(stream, &tag).map_err(commit_wrap)
                }
                crate::simple_query::OutcomeResponse::Rows { result, shape } => {
                    if Some(&result.columns) != expected_columns.as_ref() {
                        return Err(commit_wrap(HandlerError::ResultTypeChanged));
                    }
                    let total = result.rows.len();
                    let take = if max_rows <= 0 {
                        total
                    } else {
                        (max_rows as usize).min(total)
                    };

                    // まず中断保持へ回る行（`take` 以降）だけを対象に、1 行
                    // エンコードするたびに合計バイト数をセッション全体
                    // （他の全 portal の中断保持分を含む。PR #1013 レビュー
                    // 指摘・P0: portal 単体判定だと名前付き portal 最大 64 個で
                    // 接続全体では上限の約 64 倍相当を保持できてしまう）で
                    // 判定し、超過が判明した時点で残りの行を一切エンコードせず
                    // 即座に拒否する。この判定は `take` 分の送出を始める前に
                    // 完了させる ―― 1 回の Execute は「`take` 行の送出（＋
                    // 完了／中断マーカー）が丸ごと成功する」か「1 バイトも送らず
                    // 失敗する」かのいずれかである契約を保つ（中断バイト上限
                    // 超過を理由に、既に一部の行を送ってしまった後で
                    // ErrorResponse を返す事態を避ける）。
                    let other_suspended_bytes =
                        state.portals.total_suspended_bytes_excluding(portal_name);
                    let mut remaining_bytes: usize = 0;
                    let mut frames = std::collections::VecDeque::new();
                    for row in result.rows.iter().skip(take) {
                        // Bind 時点で確定した結果 format code（WIRE-14。
                        // `Portal::result_formats`）を反映する。`validate_binary_
                        // formats` が Bind で事前検査済みのため、ここでの
                        // `EncodeError` は呼び出し元（本モジュール）内部の不整合
                        // のみを意味する（`XX000` 相当。`result_encoder`
                        // モジュールドキュメント参照）。
                        let mut frame = Vec::new();
                        result_encoder::encode_data_row_into_with_formats(
                            row,
                            &result_formats,
                            &mut frame,
                        )
                        .map_err(|_| commit_wrap(internal_error("failed to encode data row")))?;
                        remaining_bytes = remaining_bytes.saturating_add(frame.len());
                        let session_total = other_suspended_bytes.saturating_add(remaining_bytes);
                        if session_total > MAX_SUSPENDED_PORTAL_BYTES_PER_SESSION {
                            return Err(commit_wrap(HandlerError::SuspendedBytesExceeded));
                        }
                        frames.push_back(frame);
                    }

                    // 中断保持分の検査を通過した後で初めて `take` 分の送出へ移る。
                    // `take` 分は 1 行ずつエンコード→`ResponseBuffer` へ積んで
                    // その場で送出し、結果セット全体（`take` 分を含む）を先に
                    // メモリへ確保しない（PR #1013 レビュー指摘・P0: `max_rows`
                    // が非常に大きい／`max_rows<=0` の場合、`take` は `total` と
                    // 一致し得るため、`take` 分もここで先にすべてエンコードして
                    // 保持すると外部 wire 入力の結果セット規模で無制限にアロケー
                    // ションが発生してしまう）。
                    let mut buffer = crate::response_buffer::ResponseBuffer::with_capacity_hint(
                        MAX_RESPONSE_BUFFER_BYTES,
                        0,
                    );
                    for row in result.rows.iter().take(take) {
                        let mut frame = Vec::new();
                        result_encoder::encode_data_row_into_with_formats(
                            row,
                            &result_formats,
                            &mut frame,
                        )
                        .map_err(|_| commit_wrap(internal_error("failed to encode data row")))?;
                        buffer
                            .push_frame(stream, &frame)
                            .map_err(|e| commit_wrap(io_to_handler(e)))?;
                        if buffer.len() >= MAX_RESPONSE_BUFFER_BYTES {
                            buffer
                                .flush(stream)
                                .map_err(|e| commit_wrap(io_to_handler(e)))?;
                        }
                    }

                    if frames.is_empty() {
                        // 全行を今回の Execute で送出済み。`CommandComplete` の
                        // タグは portal 全体の累計送出行数（この場合は `take`
                        // そのもの）から組み立てる（PR #1013 レビュー指摘・P1）。
                        let tag = shape.render(take);
                        let msg = result_encoder::encode_command_complete(&tag).map_err(|_| {
                            commit_wrap(internal_error("failed to encode command complete"))
                        })?;
                        buffer
                            .push_frame(stream, &msg)
                            .map_err(|e| commit_wrap(io_to_handler(e)))?;
                        buffer
                            .flush(stream)
                            .map_err(|e| commit_wrap(io_to_handler(e)))?;

                        let portal = state
                            .portals
                            .get_mut(portal_name)
                            .ok_or(HandlerError::UnknownPortal)?;
                        portal.state = PortalState::Done { tag };
                        portal.cursor_binding = cursor_binding;
                        return Ok(());
                    }

                    buffer
                        .push_frame(stream, &result_encoder::encode_portal_suspended())
                        .map_err(|e| commit_wrap(io_to_handler(e)))?;
                    buffer
                        .flush(stream)
                        .map_err(|e| commit_wrap(io_to_handler(e)))?;

                    let portal = state
                        .portals
                        .get_mut(portal_name)
                        .ok_or(HandlerError::UnknownPortal)?;
                    portal.state = PortalState::Suspended(PortalRows {
                        frames,
                        shape,
                        sent_so_far: take,
                        committed: is_write_statement,
                    });
                    portal.cursor_binding = cursor_binding;
                    Ok(())
                }
            }
        })();
        if response.is_err() && is_cursor_fetch {
            txn.fail();
        }
        return response;
    }

    let portal = state
        .portals
        .get_mut(portal_name)
        .ok_or(HandlerError::UnknownPortal)?;

    // 完了済み portal（`Done`）の再 Execute は保持済みの完了タグを再送する
    // だけで行を一切送出しないため、カーソル束縛の検証より先に処理する
    // （PR #1049 レビュー指摘 Cursor Bugbot Low 対応: `CLOSE`／`COMMIT`／
    // `ROLLBACK` 後に完了済み `FETCH` portal を再 Execute すると `34000` を
    // 返していた。`Done` の扱いを `FETCH` 以外の portal と揃える）。
    if let PortalState::Done { tag } = &portal.state {
        let tag = tag.clone();
        return write_command_complete(stream, &tag);
    }

    // カーソル `FETCH` 由来 portal の中断保持分（`PortalState::Suspended`）を
    // 再送出する前に、実行成功時点で束縛した世代・カーソル個体識別子が現在も
    // 有効かを検証する（PR #1049 レビュー指摘対応。cursor Bugbot P1「終了した
    // カーソルの FETCH portal から行を送出できる」の是正）。`CLOSE`（同名の
    // 再 `DECLARE` を含む）・`COMMIT`／`ROLLBACK`（→ 新しい世代／`Idle`）の
    // いずれを挟んでいても不一致となり fail-closed に拒否する。検証対象は
    // 保持行を送出し得る `Suspended` のみ（`Done` は直前で処理済み、`Failed`
    // は下の分岐で `PortalFailed` になる）。`Fetch` 以外から作った portal は
    // `cursor_binding` が常に `None` のため対象外（既存の挙動を変えない）。
    // `Ready`（初回実行）はこの分岐に到達しない（`needs_execution` 判定で
    // 上のブロックへ流れ、この時点では既に `return` 済みのため）。
    if let (PortalState::Suspended(_), Some(binding)) = (&portal.state, &portal.cursor_binding) {
        let still_valid = txn.active_generation() == Some(binding.txn_generation)
            && txn.cursor_id(&binding.cursor_name) == Some(binding.cursor_id);
        if !still_valid {
            portal.state = PortalState::Failed;
            return Err(HandlerError::Sql(
                engine::sql::allowlist::SqlSurfaceError::invalid_cursor_name(),
            ));
        }
    }

    let finished_tag = {
        let rows = match &mut portal.state {
            PortalState::Suspended(rows) => rows,
            // `Ready` はここに至る前に実行済みへ遷移し、`Done` は直前で
            // 早期 return 済みのはず。`Failed` は実行を試みたが（実行自体・
            // その後処理いずれかの理由で）失敗した終端状態で、再実行を許さず
            // `HandlerError::PortalFailed` を返す（PR #1013 レビュー指摘・
            // Cursor Bugbot Medium。この分岐は正常なクライアント操作
            // （失敗した portal への再 Execute）として到達しうるため、
            // 「内部エラー」ではなくクライアントに意味の伝わるエラーへ写像
            // する）。内部状態機械の不変条件が将来の変更で崩れた場合に備え、
            // それ以外の到達（到達しないはずの `Ready`/`Done`）はここでは
            // panic ではなく fail-closed な内部エラーへ倒す（wire 入力経路で
            // panic を避ける方針）。
            PortalState::Failed => return Err(HandlerError::PortalFailed),
            PortalState::Ready | PortalState::Done { .. } => {
                return Err(internal_error(
                    "portal state machine invariant violated: expected Suspended state",
                ))
            }
        };

        // この中断保持分の送出継続が、先行する Execute で既に commit 済みの
        // 書き込み系文の結果を運んでいるか（`PortalRows::committed`。
        // `PortalRows` を構築した Execute 呼び出し内で確定済みの値をそのまま
        // 引き継ぐ）。以降のエンコード・IO 失敗はこの値に応じて
        // `HandlerError::PostCommit` へ包む。
        let committed = rows.committed;
        let commit_wrap = |e: HandlerError| -> HandlerError {
            if committed {
                HandlerError::PostCommit(Box::new(e))
            } else {
                e
            }
        };

        let total = rows.frames.len();
        let take = if max_rows <= 0 {
            total
        } else {
            (max_rows as usize).min(total)
        };

        let hint: usize = rows.frames.iter().take(take).map(Vec::len).sum();
        let mut buffer = crate::response_buffer::ResponseBuffer::with_capacity_hint(
            MAX_RESPONSE_BUFFER_BYTES,
            hint,
        );
        for _ in 0..take {
            if let Some(frame) = rows.frames.pop_front() {
                buffer
                    .push_frame(stream, &frame)
                    .map_err(|e| commit_wrap(io_to_handler(e)))?;
            }
            if buffer.len() >= MAX_RESPONSE_BUFFER_BYTES {
                buffer
                    .flush(stream)
                    .map_err(|e| commit_wrap(io_to_handler(e)))?;
            }
        }
        rows.sent_so_far = rows.sent_so_far.saturating_add(take);

        if rows.frames.is_empty() {
            // `CommandComplete` のタグは今回のバッチ件数（`take`）ではなく
            // portal 全体の累計送出行数（`sent_so_far`）から組み立てる
            // ―― PostgreSQL の契約では分割送出（`PortalRun`）を跨いだ
            // 累計件数を返す（PR #1013 レビュー指摘・P1。例:
            // 5 行を `max_rows=2` で 3 回に分けて取得した場合の完了タグは
            // `SELECT 5` であるべきで、直近バッチの `1` 件だけではない）。
            let tag = rows.shape.render(rows.sent_so_far);
            let msg = result_encoder::encode_command_complete(&tag)
                .map_err(|_| commit_wrap(internal_error("failed to encode command complete")))?;
            buffer
                .push_frame(stream, &msg)
                .map_err(|e| commit_wrap(io_to_handler(e)))?;
            buffer
                .flush(stream)
                .map_err(|e| commit_wrap(io_to_handler(e)))?;
            Some(tag)
        } else {
            buffer
                .push_frame(stream, &result_encoder::encode_portal_suspended())
                .map_err(|e| commit_wrap(io_to_handler(e)))?;
            buffer
                .flush(stream)
                .map_err(|e| commit_wrap(io_to_handler(e)))?;
            None
        }
    };

    if let Some(tag) = finished_tag {
        portal.state = PortalState::Done { tag };
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Sync（'S'）
// ---------------------------------------------------------------------------

/// Sync（'S'）を処理する。body は厳密に空（length=4）以外を fail-closed で
/// 拒否する（フレーム違反であり回復しない。`'X'` と同じ扱い）。
/// [`ExtendedQueryState::ignore_till_sync`] を解除し、名前付き・無名を問わず
/// 全 portal を破棄したうえで `ReadyForQuery` を返す（モジュールドキュメント
/// 「エラー後の同期回復」節・「portal のライフサイクル」節参照。portal の
/// 寿命が明示トランザクション〔SQL-31・TASK-221〕の有無とは独立に Sync
/// 境界だけで決まる契約は [`PortalStore::clear_all`] 参照。codex P1 指摘・
/// PR #1013——名前付き prepared statement〔[`PreparedStatementStore`]〕は
/// この対象外のまま Sync を越えて残る）。トランザクション状態機械
/// （`SessionTransaction`）自体は本関数の対象外で、`handshake::
/// post_auth_loop` が接続単位で保持したまま Sync を越えて生き続ける
/// （SQL-31・TASK-221・Issue #942）。
pub(crate) fn handle_sync<S: WireStream>(
    stream: &mut S,
    state: &mut ExtendedQueryState,
    txn_status: engine::sql::transaction::TransactionStatus,
) -> io::Result<LoopSignal> {
    let _body = match framing::read_length_prefixed_body(stream, 4, 4) {
        Ok(b) => b,
        Err(FrameError::Truncated) => return Ok(LoopSignal::Closed),
        Err(e @ FrameError::Io(_)) => return Err(io_error_from_frame(e)),
        Err(e) => {
            respond_error_and_close(stream, &HandlerError::Frame(e))?;
            return Ok(LoopSignal::Closed);
        }
    };

    state.ignore_till_sync = false;
    state.portals.clear_all();
    // `txn_status` は `handshake::post_auth_loop` が接続単位で保持する
    // `SessionTransaction::status()` をそのまま渡す（WIRE-19・SQL-31・
    // TASK-221・PR #1041 レビュー指摘 P1: 以前は明示トランザクションの状態を
    // 無視して常に `Idle`（'I'）を送出しており、`BEGIN` 後もクライアントから
    // トランザクションが終了したように見えていた）。
    stream.write_all(&result_encoder::encode_ready_for_query(txn_status))?;
    stream.flush()?;
    Ok(LoopSignal::Continue)
}

// ---------------------------------------------------------------------------
// Close（'C'）
// ---------------------------------------------------------------------------

/// Close（'C'）を処理する。statement 対象はその statement から作られた
/// portal もまとめて閉じる。存在しない名前を渡しても失敗させない
/// （PostgreSQL と同じ挙動）。
pub(crate) fn handle_close<S: WireStream>(
    stream: &mut S,
    state: &mut ExtendedQueryState,
) -> io::Result<LoopSignal> {
    let body = match framing::read_length_prefixed_body(
        stream,
        MIN_CLOSE_BODY_LEN,
        framing::MAX_MESSAGE_LEN,
    ) {
        Ok(b) => b,
        Err(FrameError::Truncated) => return Ok(LoopSignal::Closed),
        Err(e @ FrameError::Io(_)) => return Err(io_error_from_frame(e)),
        Err(e) => {
            respond_error_and_close(stream, &HandlerError::Frame(e))?;
            return Ok(LoopSignal::Closed);
        }
    };

    match parse_close_body(&body) {
        Ok(msg) => {
            match msg.target {
                TargetKind::Statement => {
                    state.statements.remove(&msg.name);
                    state.portals.remove_all_for_statement(&msg.name);
                }
                TargetKind::Portal => {
                    state.portals.remove(&msg.name);
                }
            }
            stream.write_all(&result_encoder::encode_close_complete())?;
            stream.flush()?;
            Ok(LoopSignal::Continue)
        }
        Err(e) => {
            respond_error_and_await_sync(stream, &HandlerError::Body(e), state)?;
            Ok(LoopSignal::Continue)
        }
    }
}

// ---------------------------------------------------------------------------
// Flush（'H'）
// ---------------------------------------------------------------------------

/// Flush（'H'）を処理する。body は厳密に空（length=4）以外を fail-closed で
/// 拒否する（`'S'`/`'X'` と同じ扱い）。出力を flush するのみで `ReadyForQuery`
/// は送らない（PostgreSQL の規約）。
pub(crate) fn handle_flush<S: WireStream>(stream: &mut S) -> io::Result<LoopSignal> {
    let _body = match framing::read_length_prefixed_body(stream, 4, 4) {
        Ok(b) => b,
        Err(FrameError::Truncated) => return Ok(LoopSignal::Closed),
        Err(e @ FrameError::Io(_)) => return Err(io_error_from_frame(e)),
        Err(e) => {
            respond_error_and_close(stream, &HandlerError::Frame(e))?;
            return Ok(LoopSignal::Closed);
        }
    };
    stream.flush()?;
    Ok(LoopSignal::Continue)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_parse_body_decodes_name_query_and_zero_params() {
        let mut body = Vec::new();
        body.extend_from_slice(b"stmt1\0");
        body.extend_from_slice(b"SELECT 1\0");
        body.extend_from_slice(&0i16.to_be_bytes());

        let msg = parse_parse_body(&body).expect("valid Parse body");
        assert_eq!(msg.name, "stmt1");
        assert_eq!(msg.query, "SELECT 1");
        assert_eq!(msg.num_param_types, 0);
    }

    #[test]
    fn parse_parse_body_decodes_anonymous_statement() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0");
        body.extend_from_slice(b"SELECT 1\0");
        body.extend_from_slice(&0i16.to_be_bytes());

        let msg = parse_parse_body(&body).expect("valid Parse body");
        assert_eq!(msg.name, "");
        assert_eq!(msg.query, "SELECT 1");
    }

    #[test]
    fn parse_parse_body_rejects_missing_nul_terminator() {
        let body = b"stmt1".to_vec();
        assert!(matches!(
            parse_parse_body(&body),
            Err(BodyError::MissingNulTerminator)
        ));
    }

    #[test]
    fn parse_parse_body_rejects_negative_param_count() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0");
        body.extend_from_slice(b"SELECT 1\0");
        body.extend_from_slice(&(-1i16).to_be_bytes());

        assert!(matches!(
            parse_parse_body(&body),
            Err(BodyError::NegativeCount)
        ));
    }

    #[test]
    fn parse_parse_body_rejects_trailing_bytes() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0");
        body.extend_from_slice(b"SELECT 1\0");
        body.extend_from_slice(&0i16.to_be_bytes());
        body.push(0xff); // 余剰バイト

        assert!(matches!(
            parse_parse_body(&body),
            Err(BodyError::TrailingOrTruncatedBytes)
        ));
    }

    #[test]
    fn parse_parse_body_rejects_truncated_param_oids() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0");
        body.extend_from_slice(b"SELECT 1\0");
        body.extend_from_slice(&1i16.to_be_bytes()); // 1 件宣言だが OID 本体なし

        assert!(matches!(
            parse_parse_body(&body),
            Err(BodyError::TrailingOrTruncatedBytes)
        ));
    }

    #[test]
    fn parse_describe_body_decodes_statement_target() {
        let mut body = Vec::new();
        body.push(b'S');
        body.extend_from_slice(b"stmt1\0");

        let msg = parse_describe_body(&body).expect("valid Describe body");
        assert_eq!(msg.target, TargetKind::Statement);
        assert_eq!(msg.name, "stmt1");
    }

    #[test]
    fn parse_describe_body_decodes_portal_target() {
        let mut body = Vec::new();
        body.push(b'P');
        body.extend_from_slice(b"\0");

        let msg = parse_describe_body(&body).expect("valid Describe body");
        assert_eq!(msg.target, TargetKind::Portal);
    }

    #[test]
    fn parse_describe_body_rejects_invalid_kind() {
        let mut body = Vec::new();
        body.push(b'X');
        body.extend_from_slice(b"\0");

        assert!(matches!(
            parse_describe_body(&body),
            Err(BodyError::InvalidTargetKind)
        ));
    }

    #[test]
    fn parse_describe_body_rejects_trailing_bytes() {
        let mut body = Vec::new();
        body.push(b'S');
        body.extend_from_slice(b"stmt1\0");
        body.push(0xff);

        assert!(matches!(
            parse_describe_body(&body),
            Err(BodyError::TrailingOrTruncatedBytes)
        ));
    }

    #[test]
    fn parse_close_body_decodes_statement_and_portal() {
        let mut body = Vec::new();
        body.push(b'S');
        body.extend_from_slice(b"stmt1\0");
        let msg = parse_close_body(&body).expect("valid Close body");
        assert_eq!(msg.target, TargetKind::Statement);
        assert_eq!(msg.name, "stmt1");

        let mut body = Vec::new();
        body.push(b'P');
        body.extend_from_slice(b"portal1\0");
        let msg = parse_close_body(&body).expect("valid Close body");
        assert_eq!(msg.target, TargetKind::Portal);
        assert_eq!(msg.name, "portal1");
    }

    #[test]
    fn parse_execute_body_decodes_portal_and_max_rows() {
        let mut body = Vec::new();
        body.extend_from_slice(b"portal1\0");
        body.extend_from_slice(&5i32.to_be_bytes());
        let msg = parse_execute_body(&body).expect("valid Execute body");
        assert_eq!(msg.portal_name, "portal1");
        assert_eq!(msg.max_rows, 5);
    }

    #[test]
    fn parse_execute_body_accepts_zero_max_rows_as_unbounded() {
        let mut body = Vec::new();
        body.extend_from_slice(b"portal1\0");
        body.extend_from_slice(&0i32.to_be_bytes());
        let msg = parse_execute_body(&body).expect("max_rows=0 means unbounded fetch");
        assert_eq!(msg.max_rows, 0);
    }

    #[test]
    fn parse_execute_body_rejects_negative_max_rows() {
        // untrusted wire 入力の負値 `max_rows` を `as usize` へキャストすると
        // 巨大値化し「無制限取得（0 専用の意味）」を僭称してしまうため、
        // fail-closed に拒否することを固定する（PR #1013 レビュー指摘・P0）。
        let mut body = Vec::new();
        body.extend_from_slice(b"portal1\0");
        body.extend_from_slice(&(-1i32).to_be_bytes());
        assert!(matches!(
            parse_execute_body(&body),
            Err(BodyError::NegativeCount)
        ));
    }

    #[test]
    fn parse_execute_body_rejects_trailing_bytes() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0");
        body.extend_from_slice(&0i32.to_be_bytes());
        body.push(0xff);
        assert!(matches!(
            parse_execute_body(&body),
            Err(BodyError::TrailingOrTruncatedBytes)
        ));
    }

    #[test]
    fn parse_bind_body_decodes_minimal_message() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0"); // portal name
        body.extend_from_slice(b"\0"); // statement name
        body.extend_from_slice(&0i16.to_be_bytes()); // param format code count
        body.extend_from_slice(&0i16.to_be_bytes()); // param count
        body.extend_from_slice(&0i16.to_be_bytes()); // result format code count

        let msg = parse_bind_body(&body).expect("valid Bind body");
        assert_eq!(msg.portal_name, "");
        assert_eq!(msg.statement_name, "");
        assert!(msg.param_format_codes.is_empty());
        assert!(msg.param_values.is_empty());
        assert!(msg.result_format_codes.is_empty());
    }

    #[test]
    fn parse_bind_body_decodes_null_and_non_null_params() {
        let mut body = Vec::new();
        body.extend_from_slice(b"p\0");
        body.extend_from_slice(b"s\0");
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&2i16.to_be_bytes()); // 2 params
        body.extend_from_slice(&(-1i32).to_be_bytes()); // NULL
        body.extend_from_slice(&3i32.to_be_bytes());
        body.extend_from_slice(b"abc");
        body.extend_from_slice(&0i16.to_be_bytes());

        let msg = parse_bind_body(&body).expect("valid Bind body");
        assert_eq!(msg.param_values, vec![None, Some(b"abc".to_vec())]);
    }

    #[test]
    fn parse_bind_body_rejects_negative_param_value_length() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0");
        body.extend_from_slice(b"\0");
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&1i16.to_be_bytes());
        body.extend_from_slice(&(-2i32).to_be_bytes()); // -1 以外の負値は不正

        assert!(matches!(
            parse_bind_body(&body),
            Err(BodyError::NegativeCount)
        ));
    }

    #[test]
    fn parse_bind_body_rejects_value_length_exceeding_body() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0");
        body.extend_from_slice(b"\0");
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&1i16.to_be_bytes());
        body.extend_from_slice(&100i32.to_be_bytes()); // 宣言長に対し本体が無い

        assert!(matches!(
            parse_bind_body(&body),
            Err(BodyError::TrailingOrTruncatedBytes)
        ));
    }

    #[test]
    fn parse_bind_body_rejects_trailing_bytes() {
        let mut body = Vec::new();
        body.extend_from_slice(b"\0");
        body.extend_from_slice(b"\0");
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.extend_from_slice(&0i16.to_be_bytes());
        body.push(0xff);

        assert!(matches!(
            parse_bind_body(&body),
            Err(BodyError::TrailingOrTruncatedBytes)
        ));
    }

    #[test]
    fn validate_format_codes_accepts_zero_one_or_target_count() {
        assert!(validate_format_codes(&[], 5).is_ok());
        assert!(validate_format_codes(&[0], 5).is_ok());
        assert!(validate_format_codes(&[0, 0, 0, 0, 0], 5).is_ok());
    }

    #[test]
    fn validate_format_codes_rejects_mismatched_count() {
        assert!(matches!(
            validate_format_codes(&[0, 0], 5),
            Err(HandlerError::FormatCodeCountMismatch)
        ));
    }

    #[test]
    fn validate_format_codes_accepts_binary_and_rejects_unknown_codes() {
        // binary（1）のスロット別受理可否は Bind 本体が `ParamSlot` で判定する。
        assert!(validate_format_codes(&[1], 1).is_ok());
        assert!(matches!(
            validate_format_codes(&[2], 1),
            Err(HandlerError::InvalidParamFormatCode)
        ));
        assert!(matches!(
            validate_format_codes(&[0, -1], 2),
            Err(HandlerError::InvalidParamFormatCode)
        ));
    }

    #[test]
    fn store_rejects_name_too_long() {
        let mut store = PreparedStatementStore::new();
        let long_name = "a".repeat(MAX_STATEMENT_NAME_LEN + 1);
        let result = store.insert(long_name, 10, PreparedStatement::Empty);
        assert!(matches!(result, Err(StoreError::NameTooLong)));
    }

    #[test]
    fn store_rejects_duplicate_named_statement() {
        let mut store = PreparedStatementStore::new();
        store
            .insert("stmt1".to_string(), 10, PreparedStatement::Empty)
            .expect("first insert succeeds");
        let result = store.insert("stmt1".to_string(), 10, PreparedStatement::Empty);
        assert!(matches!(result, Err(StoreError::DuplicateName)));
    }

    #[test]
    fn store_allows_repeated_anonymous_inserts_without_counting_toward_limit() {
        let mut store = PreparedStatementStore::new();
        for _ in 0..(MAX_PREPARED_STATEMENTS_PER_SESSION * 2) {
            store
                .insert(String::new(), 10, PreparedStatement::Empty)
                .expect("anonymous insert always succeeds within byte budget");
        }
        assert_eq!(store.statements.len(), 1);
    }

    #[test]
    fn store_rejects_too_many_named_statements() {
        let mut store = PreparedStatementStore::new();
        for i in 0..MAX_PREPARED_STATEMENTS_PER_SESSION {
            store
                .insert(format!("stmt{i}"), 10, PreparedStatement::Empty)
                .expect("insert within limit succeeds");
        }
        let result = store.insert(
            format!("stmt{MAX_PREPARED_STATEMENTS_PER_SESSION}"),
            10,
            PreparedStatement::Empty,
        );
        assert!(matches!(result, Err(StoreError::TooManyStatements)));
    }

    /// 無名 statement が先に存在していても、名前付き statement は規定の
    /// `MAX_PREPARED_STATEMENTS_PER_SESSION` 件をすべて保持できる（codex-review
    /// ・Cursor Bugbot 指摘・Issue #933: 無名エントリを件数上限へ誤って算入する
    /// 境界バグの回帰）。
    #[test]
    fn store_allows_full_named_quota_alongside_anonymous_statement() {
        let mut store = PreparedStatementStore::new();
        store
            .insert(String::new(), 10, PreparedStatement::Empty)
            .expect("anonymous insert succeeds");
        for i in 0..MAX_PREPARED_STATEMENTS_PER_SESSION {
            store
                .insert(format!("stmt{i}"), 10, PreparedStatement::Empty)
                .expect("named insert within limit succeeds despite anonymous entry present");
        }
        let result = store.insert(
            format!("stmt{MAX_PREPARED_STATEMENTS_PER_SESSION}"),
            10,
            PreparedStatement::Empty,
        );
        assert!(matches!(result, Err(StoreError::TooManyStatements)));
    }

    #[test]
    fn store_rejects_total_bytes_exceeded() {
        let mut store = PreparedStatementStore::new();
        let result = store.insert(
            "big".to_string(),
            MAX_PREPARED_SQL_BYTES_PER_SESSION + 1,
            PreparedStatement::Empty,
        );
        assert!(matches!(result, Err(StoreError::TotalBytesExceeded)));
    }

    #[test]
    fn store_remove_clears_entry_and_byte_accounting() {
        let mut store = PreparedStatementStore::new();
        store
            .insert("stmt1".to_string(), 100, PreparedStatement::Empty)
            .expect("insert succeeds");
        store.remove("stmt1");
        assert!(store.get("stmt1").is_none());
        // 累計バイト数が正しく戻っていることを、上限ぎりぎりの別ステートメント
        // が挿入できることで確認する。
        store
            .insert(
                "stmt2".to_string(),
                MAX_PREPARED_SQL_BYTES_PER_SESSION,
                PreparedStatement::Empty,
            )
            .expect("insert succeeds after removal frees the byte budget");
    }

    #[test]
    fn store_remove_of_unknown_name_is_a_no_op() {
        let mut store = PreparedStatementStore::new();
        store.remove("does-not-exist");
    }

    fn portal_with_bound_bytes(bound_bytes: usize) -> Portal {
        Portal {
            source_statement: String::new(),
            body: PortalBody::Empty,
            columns: None,
            result_formats: Vec::new(),
            state: PortalState::Ready,
            cursor_binding: None,
            bound_bytes,
        }
    }

    #[test]
    fn portal_store_enforces_bound_bytes_limit_and_replacement_credit() {
        let mut store = PortalStore::new();
        let half = MAX_BOUND_PARAM_BYTES_PER_SESSION / 2;
        store
            .insert("a".to_string(), portal_with_bound_bytes(half))
            .expect("first fits");
        store
            .insert("b".to_string(), portal_with_bound_bytes(half))
            .expect("second fills the limit exactly");
        // 上限超過は拒否される。
        assert!(matches!(
            store.insert("c".to_string(), portal_with_bound_bytes(1)),
            Err(PortalStoreError::BoundBytesExceeded)
        ));
        // 無名 portal は同名置換のため、既存の無名分を差し引いて判定する。
        store
            .insert(String::new(), portal_with_bound_bytes(0))
            .expect("zero bytes fits at the limit");
        assert!(matches!(
            store.insert(String::new(), portal_with_bound_bytes(1)),
            Err(PortalStoreError::BoundBytesExceeded)
        ));
        // 除去後は再び受理される。
        store.remove("a");
        store
            .insert("c".to_string(), portal_with_bound_bytes(half))
            .expect("credit returns after removal");
        store.clear_all();
        store
            .insert(
                "d".to_string(),
                portal_with_bound_bytes(MAX_BOUND_PARAM_BYTES_PER_SESSION),
            )
            .expect("credit returns after clear_all");
    }

    #[test]
    fn decode_declared_oids_reads_big_endian_pairs() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&25i32.to_be_bytes());
        bytes.extend_from_slice(&1043i32.to_be_bytes());
        assert_eq!(decode_declared_oids(&bytes), vec![25, 1043]);
    }

    #[test]
    fn portal_store_rejects_too_many_named_portals() {
        let mut store = PortalStore::new();
        for i in 0..MAX_PORTALS_PER_SESSION {
            store
                .insert(
                    format!("p{i}"),
                    Portal {
                        source_statement: String::new(),
                        body: PortalBody::Empty,
                        columns: None,
                        result_formats: Vec::new(),
                        state: PortalState::Ready,
                        cursor_binding: None,
                        bound_bytes: 0,
                    },
                )
                .expect("insert within limit succeeds");
        }
        let result = store.insert(
            format!("p{MAX_PORTALS_PER_SESSION}"),
            Portal {
                source_statement: String::new(),
                body: PortalBody::Empty,
                columns: None,
                result_formats: Vec::new(),
                state: PortalState::Ready,
                cursor_binding: None,
                bound_bytes: 0,
            },
        );
        assert!(matches!(result, Err(PortalStoreError::TooManyPortals)));
    }

    #[test]
    fn portal_store_remove_anonymous_only_removes_unnamed() {
        let mut store = PortalStore::new();
        store
            .insert(
                String::new(),
                Portal {
                    source_statement: String::new(),
                    body: PortalBody::Empty,
                    columns: None,
                    result_formats: Vec::new(),
                    state: PortalState::Ready,
                    cursor_binding: None,
                    bound_bytes: 0,
                },
            )
            .expect("insert succeeds");
        store
            .insert(
                "named".to_string(),
                Portal {
                    source_statement: String::new(),
                    body: PortalBody::Empty,
                    columns: None,
                    result_formats: Vec::new(),
                    state: PortalState::Ready,
                    cursor_binding: None,
                    bound_bytes: 0,
                },
            )
            .expect("insert succeeds");

        store.remove_anonymous();
        assert!(store.get("").is_none());
        assert!(store.get("named").is_some());
    }

    #[test]
    fn portal_store_remove_all_for_statement_closes_derived_portals_only() {
        let mut store = PortalStore::new();
        store
            .insert(
                "p1".to_string(),
                Portal {
                    source_statement: "stmt1".to_string(),
                    body: PortalBody::Empty,
                    columns: None,
                    result_formats: Vec::new(),
                    state: PortalState::Ready,
                    cursor_binding: None,
                    bound_bytes: 0,
                },
            )
            .expect("insert succeeds");
        store
            .insert(
                "p2".to_string(),
                Portal {
                    source_statement: "stmt2".to_string(),
                    body: PortalBody::Empty,
                    columns: None,
                    result_formats: Vec::new(),
                    state: PortalState::Ready,
                    cursor_binding: None,
                    bound_bytes: 0,
                },
            )
            .expect("insert succeeds");

        store.remove_all_for_statement("stmt1");
        assert!(store.get("p1").is_none());
        assert!(store.get("p2").is_some());
    }
}
