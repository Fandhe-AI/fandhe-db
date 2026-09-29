//! COPY プロトコル（`COPY ... FROM STDIN`／`COPY (...) TO STDOUT`）の SQL 表層
//! 側実装（Issue #939・WIRE-17・TASK-220）。
//!
//! 責務境界: wire-server（`crates/wire-server/src/copy.rs`）は CopyIn／CopyOut
//! サブプロトコルのメッセージ層（フレーミング・状態遷移）のみを担い、行の
//! レコード分割・フィールドデコード・束縛・INDEX-4 上限判定・commit はすべて
//! ここへ集約する（第 2 の書き込み経路を作らない設計。TASK-190（SQL-16。
//! [`crate::core::EngineCore::execute_bound_insert_in_session`]）と同一の
//! commit 経路を [`crate::core::EngineCore::commit_copy_in`] 経由で共有する）。
//!
//! `FROM STDIN` は全 CopyData を受信し終える前（CopyDone より前）に INDEX-4 の
//! 4 上限を逐次判定する（[`crate::batch_limits`] の逐次判定ヘルパーを使う。
//! バッチ全体を揃えてから判定する既存の複数行 `INSERT`（[`crate::sql::exec::
//! execute_insert_batch`]）とは異なり、ストリーミング故に超過を CopyDone を
//! 待たずに検出できる）。`TO STDOUT` は広域取得（SQL-15・Issue #454）と同じ
//! 実行本体（[`crate::core::EngineCore::run_scan_plan`]）をそのまま使う
//! （第 2 の SELECT 実行器を持たない）。
//!
//! protocol v3 の COPY サブプロトコルは CopyDone（'c'）で終端を表現するため、
//! protocol v2 由来の `\.` 終端行は本実装では受理・要求しない（PostgreSQL
//! 本家との既知の相違点。詳細は `docs/design/wire-copy-protocol.md` 参照）。

use crate::batch_limits::{self, BatchLimits, BatchLimitsError};
use crate::catalog::{ColumnType, TableSchema};
use crate::recovery::required_op_id::OperationId;
use crate::row_codec::Value;
use crate::sql::allowlist::{CopyFormat, InsertLiteral, SqlSurfaceError};
use crate::sql::parser::{
    bind_bytea_literal, bind_datetime_literal, bind_double_literal, bind_enum_literal,
    bind_integer_literal, bind_json_literal, bind_numeric_literal, bind_real_literal,
    bind_uuid_literal, parse_array_literal, parse_vector_literal, BoundInsert,
};

/// wire 層のホットパスで `lexer::tokenize` を増やさないための安価な覗き見
/// （`core.rs::execute_sql_in_session` の `INSERT`／`TRUNCATE`／`DELETE`／
/// `UPDATE` 先頭トークン判定と同型。`handshake::post_auth_loop` の 'Q' 分岐が
/// COPY サブプロトコルへ委譲するかどうかをここで判定する）。誤って見逃した
/// 場合（コメント前置等）は通常の `validate_sql` 経路へ流れ `42601` で拒否
/// される（fail-closed。本関数は受理判定を狭める側にしか作用しない）。
pub fn is_copy_statement(sql: &str) -> bool {
    let trimmed = sql.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let rest = match trimmed.get(..4) {
        Some(prefix) if prefix.eq_ignore_ascii_case("COPY") => match trimmed.get(4..) {
            Some(r) => r,
            None => return false,
        },
        _ => return false,
    };
    match rest.chars().next() {
        None => true,
        Some(c) => c.is_ascii_whitespace() || c == '(',
    }
}

/// [`BatchLimitsError`] を SQL 表層のエラー型へ写像する（既存の
/// `validate_insert_batch_byte_and_chunk_limits` と同じ「全 variant が
/// `54000`」写像。`crate::core` も同じ写像を独自に持つが、依存を増やさず
/// ここでも同じ変換を行う）。
fn map_batch_err(e: BatchLimitsError) -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large(e.to_string())
}

/// CSV 形式（RFC4180 風）の 1 バイトずつの状態機械。フィールド先頭でのみ
/// 引用符開始を許可し、閉じ引用符の直後はエスケープ（もう 1 個の `"`）・
/// フィールド区切り（`,`）・レコード終端（`\r`／`\n`）以外を受理しない
/// （Issue #939 レビュー指摘: 旧実装は分割用 `RecordSplitter` が `"` の出現
/// 回数の偶奇だけで引用状態を判定し、デコード用 `decode_csv_record` も
/// フィールド先頭以外での引用符開始・閉じ引用符直後の非区切り文字を検査
/// していなかったため、`ab"cd",x`（フィールド途中の生引用符）や
/// `"ab"junk,x`（閉じ引用符直後に区切り以外の文字）を正常データとして
/// 受理してしまっていた。レコード分割とフィールドデコードを同じ状態機械で
/// 1 回の走査として行うことで、この種の不正な引用符配置を wire 由来の
/// untrusted 入力として `22000` で一貫して拒否する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CsvFieldState {
    /// フィールドの先頭（レコード先頭・直前がカンマ・直前のフィールドの
    /// 閉じ引用符確定後のいずれか）。ここでのみ `"` が引用符開始として
    /// 受理される。
    Start,
    /// 引用符なしフィールドの内部。ここで生の `"` が現れるのは不正配置。
    Unquoted,
    /// 引用符付きフィールドの内部。
    Quoted,
    /// 引用符なしフィールド（`Start`／`Unquoted` いずれか）で裸の `\r` を
    /// 受けた直後。CRLF レコード終端の一部としてのみ許容し、次バイトが
    /// `\n` でなければ「引用符なしフィールド中の裸の CR」として `22000` で
    /// 拒否する（PostgreSQL の `COPY` が生じる `unquoted carriage return
    /// found in data` エラーに相当。codex-review 指摘 PRRT_kwDOUAKASM6l0EjU:
    /// 直前の実装は末尾が単独 CR で終わるレコードの CR を無条件に除去して
    /// いたが、CR の後に LF が続かない場合は「行終端の一部」と確定できず、
    /// 除去は入力の無言改変になる。除去も保持もせず fail-closed に拒否する）。
    UnquotedCr,
    /// 引用符付きフィールドの閉じ引用符直後。次にもう 1 個 `"` が来れば
    /// エスケープされた引用符としてフィールドへ戻り、`,`／`\n` なら
    /// フィールド／レコードの区切り、`\r` は [`Self::AfterQuoteCr`] へ移行して
    /// 次バイトを待つ、それ以外はすべて不正配置。
    AfterQuote,
    /// 閉じ引用符直後の `\r` を受けた直後（`AfterQuote` から遷移）。CRLF
    /// レコード終端の一部としてのみ許容し、次バイトが `\n` でなければ不正配置
    /// として拒否する（codex-review 指摘・discussion PRRT_kwDOUAKASM6ltrHX,
    /// PRRT_kwDOUAKASM6ltyY9: 旧実装は `AfterQuote` で `\r` を無条件に捨てて
    /// `AfterQuote` のまま処理を続けていたため、`"a"\r,b\n`（CR の後に区切り
    /// が続く）や `"a"\r`（改行なしで終端する単独 CR）といった、CR の後に
    /// `\n` が続かない不正な CSV も正常データとして受理してしまっていた）。
    AfterQuoteCr,
}

#[derive(Debug)]
struct CsvRecordScanner {
    state: CsvFieldState,
    quoted: bool,
    field: Vec<u8>,
    fields: Vec<Option<String>>,
}

impl CsvRecordScanner {
    fn new() -> Self {
        Self {
            state: CsvFieldState::Start,
            quoted: false,
            field: Vec::new(),
            fields: Vec::new(),
        }
    }

    fn push_field(&mut self) -> Result<(), SqlSurfaceError> {
        let bytes = std::mem::take(&mut self.field);
        let quoted = std::mem::replace(&mut self.quoted, false);
        self.fields.push(finish_csv_field(bytes, quoted)?);
        self.state = CsvFieldState::Start;
        Ok(())
    }

    /// 1 バイトを処理する。レコード終端（`\n`）に到達し `fields` が確定した
    /// 場合のみ `true` を返す（呼び出し元は直後に [`Self::take_record`] で
    /// 取り出す）。
    fn push_byte(&mut self, b: u8) -> Result<bool, SqlSurfaceError> {
        match self.state {
            CsvFieldState::Start => match b {
                b'"' => {
                    self.state = CsvFieldState::Quoted;
                    self.quoted = true;
                    Ok(false)
                }
                b',' => {
                    self.push_field()?;
                    Ok(false)
                }
                b'\n' => {
                    self.push_field()?;
                    Ok(true)
                }
                // 裸の `\r` はまだ「引用符なしフィールド中のデータ」か
                // 「CRLF レコード終端の一部」かが確定できないため
                // `UnquotedCr` へ移行し、次バイトで判定する（フィールド
                // 先頭で受けた場合も、後続の `\n` と対にして扱う）。
                b'\r' => {
                    self.state = CsvFieldState::UnquotedCr;
                    Ok(false)
                }
                _ => {
                    self.state = CsvFieldState::Unquoted;
                    self.field.push(b);
                    Ok(false)
                }
            },
            CsvFieldState::Unquoted => match b {
                // フィールド先頭以外での生引用符は不正配置（`ab"cd",x` 相当）。
                b'"' => Err(SqlSurfaceError::invalid_input(
                    "misplaced double quote in COPY CSV field (quote allowed only at field start)",
                )),
                b',' => {
                    self.push_field()?;
                    Ok(false)
                }
                b'\n' => {
                    self.push_field()?;
                    Ok(true)
                }
                // `Start` と同じく、裸の `\r` は `UnquotedCr` へ移行して
                // 次バイトで判定する（フィールドへは積まない）。
                b'\r' => {
                    self.state = CsvFieldState::UnquotedCr;
                    Ok(false)
                }
                _ => {
                    self.field.push(b);
                    Ok(false)
                }
            },
            CsvFieldState::Quoted => match b {
                // ここではエスケープ（直後にもう 1 個 `"`）か閉じ引用符かが
                // まだ確定できないため `AfterQuote` へ移行し、次バイトで
                // 判定する。
                b'"' => {
                    self.state = CsvFieldState::AfterQuote;
                    Ok(false)
                }
                // 引用符内の生改行はデータ（埋め込み改行）。
                _ => {
                    self.field.push(b);
                    Ok(false)
                }
            },
            CsvFieldState::AfterQuote => match b {
                b'"' => {
                    // 直前の `"` と合わせてエスケープされた引用符。
                    self.field.push(b'"');
                    self.state = CsvFieldState::Quoted;
                    Ok(false)
                }
                b',' => {
                    self.push_field()?;
                    Ok(false)
                }
                b'\n' => {
                    self.push_field()?;
                    Ok(true)
                }
                // `\r\n` レコード終端の `\r` 側。まだ `\n` が続くかどうかが
                // 確定していないため `AfterQuoteCr` へ移行し、次バイトで
                // 判定する（フィールドへは積まない）。
                b'\r' => {
                    self.state = CsvFieldState::AfterQuoteCr;
                    Ok(false)
                }
                // 閉じ引用符の直後に区切り・レコード終端・エスケープ以外の
                // バイトが来るのは不正配置（`"ab"junk,x` 相当）。
                _ => Err(SqlSurfaceError::invalid_input(
                    "unexpected byte after closing double quote in COPY CSV field",
                )),
            },
            CsvFieldState::AfterQuoteCr => match b {
                // `\r\n` が揃った。レコード終端として確定する。
                b'\n' => {
                    self.push_field()?;
                    Ok(true)
                }
                // CR の直後に LF が続かない不正配置（`"a"\r,x` 相当）。単独の
                // `\r` を無条件に読み飛ばす旧実装の fail-open を修正する。
                _ => Err(SqlSurfaceError::invalid_input(
                    "lone carriage return after closing double quote in COPY CSV field \
                     (CRLF record terminator requires a following line feed)",
                )),
            },
            CsvFieldState::UnquotedCr => match b {
                // `\r\n` が揃った。CR はフィールドへ含めず（元々積んでいない）
                // レコード終端として確定する。
                b'\n' => {
                    self.push_field()?;
                    Ok(true)
                }
                // CR の直後に LF が続かない不正配置（PostgreSQL の `COPY` が
                // 出す `unquoted carriage return found in data` に相当。
                // codex-review 指摘 PRRT_kwDOUAKASM6l0EjU: 「LF の無い CR は
                // レコード終端と確定できない」ため、無条件の除去（旧実装の
                // Cursor Bugbot 対応）は入力の無言改変になる。除去も保持も
                // せず、ここで fail-closed に拒否する）。
                _ => Err(SqlSurfaceError::invalid_input(
                    "unquoted carriage return found in COPY CSV data \
                     (unescaped CR must be followed by LF)",
                )),
            },
        }
    }

    fn take_record(&mut self) -> Vec<Option<String>> {
        std::mem::take(&mut self.fields)
    }

    /// CopyDone 到達時に呼ぶ。改行なしで終わった末尾レコード（存在する
    /// 場合）のフィールド列を返す。引用符が閉じられないまま終わった場合は
    /// `22000` で拒否する（旧 `decode_csv_record` の未終端引用符検査を踏襲）。
    fn finish(mut self) -> Result<Option<Vec<Option<String>>>, SqlSurfaceError> {
        if self.state == CsvFieldState::Quoted {
            return Err(SqlSurfaceError::invalid_input(
                "unterminated quoted CSV field in COPY record",
            ));
        }
        // CopyDone に到達した時点で「閉じ引用符直後の CR」が確定しないまま
        // 残っている（後続バイトが無いままストリームが終わった）場合も、
        // CRLF が完成しない不正配置として拒否する（codex-review 指摘:
        // `push_byte` 側の検査だけでは、レコードが CR で終わってそのまま
        // CopyDone を迎える経路が検査対象から漏れていた）。
        if self.state == CsvFieldState::AfterQuoteCr {
            return Err(SqlSurfaceError::invalid_input(
                "COPY stream ended with a lone carriage return after closing double quote \
                 (CRLF record terminator requires a following line feed)",
            ));
        }
        // 「新規レコードとして何も消費していない」を判定する条件は
        // `Start` 状態（かつ既に確定済みフィールドが無い）ことであって、
        // `field` が空であることではない。`""`（引用符で囲まれた空文字列）
        // が改行なしで終わった場合、閉じ引用符の直後（`AfterQuote` 状態）で
        // は `field` が空のまま・`fields` も空のままだが、これはレコードが
        // 存在する（`Some("")` になるべき）ケースであり、旧
        // `field.is_empty() && fields.is_empty()` 判定では誤って「レコード
        // なし」（`None`）に落ち、行が無言で消失していた（advisor 指摘。
        // 旧 `decode_csv_record(b"\"\"")` は `[Some("")]` を返していたため、
        // これは本モジュール導入時の回帰だった）。
        // CopyDone に到達した時点で「引用符なしフィールド中の裸の CR」が
        // 確定しないまま残っている場合（`value\r` のまま `\n` が来ずに
        // ストリームが終わった）、`AfterQuoteCr` と同じ理由で CRLF が完成
        // しない不正配置として拒否する（codex-review 指摘
        // PRRT_kwDOUAKASM6l0EjU: LF の無い CR は行終端と確定できないため、
        // 除去も保持もせず fail-closed に拒否する。除去する旧実装は Cursor
        // Bugbot PRRT_kwDOUAKASM6lzwN7 が指摘した値不一致——`value\r\n` と
        // `value\r` の格納値が異なる——の原因でもあった）。
        if self.state == CsvFieldState::UnquotedCr {
            return Err(SqlSurfaceError::invalid_input(
                "COPY stream ended with an unquoted carriage return in CSV data \
                 (unescaped CR must be followed by LF)",
            ));
        }
        if self.state == CsvFieldState::Start && self.fields.is_empty() {
            return Ok(None);
        }
        self.push_field()?;
        Ok(Some(self.take_record()))
    }
}

/// レコード（1 行分）をチャンク境界をまたいで切り出し、フィールドへ
/// デコードするまでを 1 つの状態機械で行う（[`CsvRecordScanner`] の
/// モジュールドキュメント参照）。text 形式は埋め込み改行を `\n`（2 文字の
/// エスケープ）としてしか表現できず引用符機構を持たないため、生の LF を
/// 単純に行区切りとして扱う（バックスラッシュエスケープの解決自体は
/// [`decode_text_field`] の責務）。`pending`／`CsvRecordScanner` 双方の
/// 総容量は呼び出し元（[`CopyInSession::feed`]）が CopyData の生バイト量
/// そのものを③（`max_batch_total_bytes`）で先に上限判定しているため、
/// 本型自体は追加の容量上限を持たない（すでに有界な入力を受け取る契約）。
///
/// `pending_cr`: 直前バイトが裸の `\r`（`pending` へは積んでいない）で、
/// 次バイトが `\n` かどうかで CRLF レコード終端の一部か・fail-closed に
/// 拒否すべき不正配置かが未確定の状態を、チャンク境界をまたいで保持する
/// （PostgreSQL の `COPY` が text 形式のデータ中の裸 CR を `literal
/// carriage return found in data` として拒否する契約に合わせる。
/// codex-review 指摘 PRRT_kwDOUAKASM6l0EjU: 末尾が単独 `\r` で終わる
/// レコードの CR を無条件に除去する旧実装は、LF が続かない裸 CR を
/// 「行終端の一部」と決め打つ入力の無言改変だった）。
#[derive(Debug)]
enum RecordSplitter {
    Text { pending: Vec<u8>, pending_cr: bool },
    Csv(CsvRecordScanner),
}

impl RecordSplitter {
    fn new(format: CopyFormat) -> Self {
        match format {
            CopyFormat::Text => RecordSplitter::Text {
                pending: Vec::new(),
                pending_cr: false,
            },
            CopyFormat::Csv => RecordSplitter::Csv(CsvRecordScanner::new()),
        }
    }

    /// `chunk`（1 個の CopyData メッセージ本文）を走査し、完了したレコード
    /// （デコード済みフィールド列）ごとに `on_record` を呼ぶ。
    fn feed(
        &mut self,
        chunk: &[u8],
        mut on_record: impl FnMut(Vec<Option<String>>) -> Result<(), SqlSurfaceError>,
    ) -> Result<(), SqlSurfaceError> {
        match self {
            RecordSplitter::Text {
                pending,
                pending_cr,
            } => {
                for &b in chunk {
                    if *pending_cr {
                        // 直前バイトが裸の `\r`。今回のバイトが `\n` なら
                        // CRLF が完成しレコード終端（CR はフィールドへ含め
                        // ない。そもそも `pending` へ積んでいない）。それ
                        // 以外は LF の続かない裸 CR として fail-closed に
                        // 拒否する。
                        *pending_cr = false;
                        if b == b'\n' {
                            let record = std::mem::take(pending);
                            on_record(decode_text_record(&record)?)?;
                            continue;
                        } else {
                            return Err(SqlSurfaceError::invalid_input(
                                "literal carriage return found in COPY text data \
                                 (unescaped CR must be followed by LF)",
                            ));
                        }
                    }
                    match b {
                        b'\r' => {
                            *pending_cr = true;
                        }
                        b'\n' => {
                            let record = std::mem::take(pending);
                            on_record(decode_text_record(&record)?)?;
                        }
                        _ => pending.push(b),
                    }
                }
                Ok(())
            }
            RecordSplitter::Csv(scanner) => {
                for &b in chunk {
                    if scanner.push_byte(b)? {
                        on_record(scanner.take_record())?;
                    }
                }
                Ok(())
            }
        }
    }

    /// CopyDone 到達時に呼ぶ。改行なしで終わった末尾レコード（存在する
    /// 場合）のフィールド列を返す。
    fn finish(self) -> Result<Option<Vec<Option<String>>>, SqlSurfaceError> {
        match self {
            RecordSplitter::Text {
                pending,
                pending_cr,
            } => {
                // CopyDone に到達した時点で「LF の続かない裸の CR」が
                // 確定しないまま残っている場合、CRLF が完成しない不正配置
                // として拒否する（`feed` 内の対応する分岐と同じ理由。
                // codex-review 指摘 PRRT_kwDOUAKASM6l0EjU）。
                if pending_cr {
                    return Err(SqlSurfaceError::invalid_input(
                        "COPY stream ended with a literal carriage return in text data \
                         (unescaped CR must be followed by LF)",
                    ));
                }
                if pending.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(decode_text_record(&pending)?))
                }
            }
            RecordSplitter::Csv(scanner) => scanner.finish(),
        }
    }
}

/// text 形式のバックスラッシュエスケープを解決する（PostgreSQL 互換の
/// `\\`／`\t`／`\n`／`\r`／`\b`／`\f`／`\v`。それ以外のエスケープ（8 進・
/// 16 進を含む）は fail-closed に `22000` で拒否する。フィールド全体が
/// リテラル `\N` の場合のみ NULL とする（`\N` が末尾以外の位置に現れた
/// 場合は通常のエスケープ解決に入り、`N` は未対応エスケープとして拒否
/// される。これは PostgreSQL 本家がフィールド全体一致でのみ `\N` を特別
/// 扱いする契約と同じ）。
fn decode_text_field(raw: &[u8]) -> Result<Option<String>, SqlSurfaceError> {
    if raw == b"\\N" {
        return Ok(None);
    }
    let mut out: Vec<u8> = Vec::with_capacity(raw.len());
    let mut iter = raw.iter().copied();
    while let Some(b) = iter.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match iter.next() {
            Some(b'\\') => out.push(b'\\'),
            Some(b't') => out.push(b'\t'),
            Some(b'n') => out.push(b'\n'),
            Some(b'r') => out.push(b'\r'),
            Some(b'b') => out.push(0x08),
            Some(b'f') => out.push(0x0C),
            Some(b'v') => out.push(0x0B),
            _ => {
                return Err(SqlSurfaceError::invalid_input(
                    "unsupported backslash escape in COPY text field",
                ))
            }
        }
    }
    String::from_utf8(out)
        .map(Some)
        .map_err(|_| SqlSurfaceError::invalid_input("COPY text field is not valid UTF-8"))
}

/// text 形式の 1 レコードをタブ区切りでフィールドへ分割する。生のタブ
/// バイトは常にフィールド区切り（埋め込みタブはクライアント側で `\t` へ
/// エスケープ済みである契約。[`RecordSplitter`] の LF と同じ考え方）。
fn decode_text_record(record: &[u8]) -> Result<Vec<Option<String>>, SqlSurfaceError> {
    record
        .split(|&b| b == b'\t')
        .map(decode_text_field)
        .collect()
}

/// [`CsvRecordScanner::push_field`] が使う、1 フィールド分の生バイト列を
/// NULL／空文字列／通常値へ写像する共通処理（引用符で囲まれていない空
/// フィールドは NULL、引用符で囲まれた空フィールド（`""`）は空文字列として
/// 区別する）。
fn finish_csv_field(bytes: Vec<u8>, quoted: bool) -> Result<Option<String>, SqlSurfaceError> {
    if bytes.is_empty() && !quoted {
        return Ok(None);
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| SqlSurfaceError::invalid_input("COPY CSV field is not valid UTF-8"))
}

/// デコード済み 1 行分のフィールド（`None` = NULL）を [`BoundInsert`] へ束縛
/// する。`sql::parser::bind_insert_row`（`INSERT ... VALUES` 専用）とは意図的に
/// 別実装とする理由: `InsertLiteral` は文字列／数値リテラルのみを表現でき
/// 明示的な NULL 値を持てない（`INSERT` の許可形状自体が VALUES 内の `NULL`
/// キーワードを受理しない）のに対し、COPY のフィールドは `\N`／引用符なし
/// 空欄という独自の NULL 表現を持つため、共有すると `InsertLiteral` へ
/// 破壊的変更（`Null` variant 追加）を要する。列名解決・非 nullable 列欠落・
/// 型照合の検査内容は `bind_insert_row` と同じ規則に揃えている。
fn bind_copy_record(
    table_name: &str,
    columns: &[String],
    fields: &[Option<String>],
    operation_id: &Option<OperationId>,
    schema: &TableSchema,
) -> Result<BoundInsert, SqlSurfaceError> {
    let id_pos = columns.iter().position(|c| c == "id").ok_or_else(|| {
        SqlSurfaceError::invalid_input("COPY column list must include the id pseudo-column")
    })?;
    let id_field = fields
        .get(id_pos)
        .and_then(|f| f.as_ref())
        .ok_or_else(|| SqlSurfaceError::invalid_input("id pseudo-column value must not be NULL"))?;
    let id: u64 = id_field
        .parse()
        .map_err(|_| SqlSurfaceError::invalid_input(format!("malformed id value: {id_field}")))?;

    let mut bound_values: Vec<Value> = vec![Value::Null; schema.columns.len()];
    let mut provided = vec![false; schema.columns.len()];

    for (name, field) in columns.iter().zip(fields.iter()) {
        if name == "id" {
            continue;
        }
        let col_idx = schema
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))?;
        let column = schema
            .columns
            .get(col_idx)
            .ok_or_else(|| SqlSurfaceError::invalid_input(format!("unknown column: {name}")))?;
        let value = match field {
            None => {
                // 明示的な `\N`（COPY テキスト形式の NULL マーカー）は
                // `DEFAULT` を適用しない（TABLE-16・TASK-204、Issue #904。
                // 列を丸ごと省略した場合〔下の省略列補完ループ〕とは異なる
                // 経路として区別する）。
                if !column.nullable {
                    return Err(SqlSurfaceError::not_null_violation(name.clone()));
                }
                Value::Null
            }
            Some(s) => match &column.ty {
                ColumnType::Vector(dim) => Value::Vector(parse_vector_literal(s, *dim)?),
                ColumnType::Text => Value::Text(s.clone()),
                ColumnType::Boolean => Value::Bool(parse_copy_boolean(s, name)?),
                ColumnType::Array(array_ty) => Value::Array(parse_array_literal(s, *array_ty)?),
                ColumnType::Bytea => bind_bytea_literal(s, name)?,
                ColumnType::Enum(def) => bind_enum_literal(def, s, name)?,
                ColumnType::Json | ColumnType::Jsonb => bind_json_literal(s, &column.ty, name)?,
                ColumnType::Date | ColumnType::Timestamp => {
                    bind_datetime_literal(name, column.ty.clone(), s)?
                }
                ColumnType::Uuid => bind_uuid_literal(s, name)?,
                // COPY のフィールドは常に文字列のためリテラル種別を持たず、
                // `InsertLiteral::String` として包んでから INSERT／UPDATE／
                // UPSERT と共有する `bind_numeric_literal` へ渡す（Issue
                // #939 のマージ時点で `ColumnType::Numeric`／`Uuid` への対応が
                // 漏れていた分の追加）。
                ColumnType::Numeric { precision, scale } => bind_numeric_literal(
                    &InsertLiteral::String(s.clone()),
                    name,
                    *precision,
                    *scale,
                )?,
                // `REAL`／`DOUBLE PRECISION`（Issue #882）も INSERT／UPDATE／
                // UPSERT と同じ `bind_real_literal`／`bind_double_literal`
                // （`scalar_float.rs` の閉じた文法。非有限化・非ゼロ
                // アンダーフローは `22003`）を共有する（main マージ時点で
                // COPY 側の網羅 match が追随しておらずビルド不能だった分の
                // 追加）。
                ColumnType::Real => Value::Real(bind_real_literal(s)?),
                ColumnType::Double => Value::Double(bind_double_literal(s)?),
                // `INTEGER`／`BIGINT`（Issue #881）も INSERT／UPDATE／UPSERT と
                // 同じ `bind_integer_literal` を共有する。COPY のフィールドは
                // 文字列でしか届かないため `InsertLiteral::Number` として包み
                // 直し、`InsertLiteral::String` 拒否（PG 互換の暗黙変換は
                // 行わない設計判断）を経由させずに数値リテラル同等の検証
                // （範囲外 `22003`・非整数形式 `22P02`）を適用する。
                ColumnType::Integer | ColumnType::BigInt => bind_integer_literal(
                    name,
                    column.ty.clone(),
                    &InsertLiteral::Number(s.clone()),
                )?,
            },
        };
        if let Some(slot) = bound_values.get_mut(col_idx) {
            *slot = value;
        }
        if let Some(flag) = provided.get_mut(col_idx) {
            *flag = true;
        }
    }

    crate::sql::parser::fill_omitted_columns(&schema.columns, &mut bound_values, &provided)?;

    Ok(BoundInsert {
        table: table_name.to_string(),
        id,
        values: bound_values,
        operation_id: operation_id.clone(),
    })
}

/// COPY テキスト／CSV 形式の `BOOLEAN` フィールドを解釈する。配列リテラルの
/// 要素解釈（`parse_array_literal` 内、`t|f|true|false` を大小無視で受理）と
/// 同じ語彙に揃え、`INSERT ... VALUES` のブール識別子（`sql::allowlist` の
/// 字句判定）とは別経路のまま維持する（COPY のフィールドは常に文字列であり
/// SQL 識別子ではないため）。
fn parse_copy_boolean(raw: &str, column_name: &str) -> Result<bool, SqlSurfaceError> {
    if raw.eq_ignore_ascii_case("true") || raw.eq_ignore_ascii_case("t") {
        Ok(true)
    } else if raw.eq_ignore_ascii_case("false") || raw.eq_ignore_ascii_case("f") {
        Ok(false)
    } else {
        Err(SqlSurfaceError::invalid_text_representation(format!(
            "column {column_name:?} expects a boolean value (t/f/true/false)"
        )))
    }
}

/// `Σ` 各値のバイト長（既存の `EngineCore::validate_insert_batch_byte_and_
/// chunk_limits`・`core.rs` の複数行 `INSERT` バッチ判定と同一の②③判定対象量
/// 定義を COPY 側でも使う。`BOOLEAN`/`ARRAY`/`BYTEA`/`ENUM` の各定義は
/// Issue #883・#888・#886・#890 の判断をそのまま踏襲する）。
fn bound_insert_byte_len(bound: &BoundInsert) -> Result<usize, SqlSurfaceError> {
    let mut total: usize = 0;
    for v in &bound.values {
        let value_len = match v {
            Value::Null => 0,
            Value::Text(s) => s.len(),
            Value::Vector(vec) => vec.len().saturating_mul(std::mem::size_of::<f32>()),
            Value::Bool(_) => 1,
            Value::Array(array_value) => {
                let entry_len =
                    crate::row_codec::scalar_array_entry_len(array_value.elem(), array_value)
                        .map_err(|e| SqlSurfaceError::payload_too_large(e.to_string()))?;
                usize::try_from(entry_len).map_err(|_| {
                    SqlSurfaceError::payload_too_large("COPY row array entry length overflow")
                })?
            }
            Value::Bytes(b) => b.len(),
            Value::Enum(s) => s.len(),
            Value::Json(s) => s.len(),
            // DATE／TIMESTAMP 値は行コーデック上それぞれ 4／8 バイト固定
            // （Issue #884・D-3。`core.rs::validate_insert_batch_byte_and_
            // chunk_limits` と同一の判定対象量定義）。
            Value::Date(_) => 4,
            Value::Timestamp(_) => 8,
            // NUMERIC／UUID 値は行コーデック上いずれも 16 バイト固定
            // （`core.rs::validate_insert_batch_byte_and_chunk_limits` と
            // 同一の判定対象量定義。Issue #885・#887）。
            Value::Numeric(_) => 16,
            Value::Uuid(_) => 16,
            // REAL／DOUBLE PRECISION は `core.rs::validate_insert_batch_
            // byte_and_chunk_limits` と同一の判定対象量定義（f32／f64 の
            // ネイティブ幅。Issue #882）。
            Value::Real(_) => std::mem::size_of::<f32>(),
            Value::Double(_) => std::mem::size_of::<f64>(),
            // INTEGER／BIGINT は `core.rs::validate_insert_batch_byte_and_
            // chunk_limits` と同一の判定対象量定義（i32／i64 のネイティブ幅。
            // Issue #881）。
            Value::Integer(_) => std::mem::size_of::<i32>(),
            Value::BigInt(_) => std::mem::size_of::<i64>(),
        };
        total = total
            .checked_add(value_len)
            .ok_or_else(|| SqlSurfaceError::payload_too_large("COPY row byte size overflow"))?;
    }
    Ok(total)
}

/// `COPY <table> (<cols>) FROM STDIN` の逐次取り込み状態（Issue #939・
/// WIRE-17）。構築は [`crate::core::EngineCore::begin_copy`] のみが行う。
/// [`Self::feed`] を CopyData ごとに呼び、INDEX-4 の 4 上限を CopyDone より
/// 前に逐次判定する。[`Self::finish`] で末尾レコードを確定させ、実際の
/// commit（[`crate::core::EngineCore::commit_copy_in`]）へ渡す
/// [`CopyInBatch`] を返す。
#[derive(Debug)]
pub struct CopyInSession {
    table: String,
    columns: Vec<String>,
    operation_id: Option<OperationId>,
    // `Box` で包む: `TableSchema` へ墓標スロット（TABLE-19・TASK-203・Issue #901）
    // が加わったことで `CopyInSession` 自体のサイズが増え、`CopyPlan::From` が
    // 包む列挙子として `clippy::large_enum_variant` に抵触するようになった。
    // ここで内側の非公開フィールドを Box 化してサイズを抑えることで、
    // `CopyPlan::From` の内包型（公開 API）を変更せずに済ませる
    // （`CopyPlan::From(CopyInSession)` のまま。外部クレートへの破壊的変更なし）。
    schema: Box<TableSchema>,
    limits: BatchLimits,
    splitter: RecordSplitter,
    bounds: Vec<BoundInsert>,
    running_bytes: usize,
}

impl CopyInSession {
    /// `columns`（`id` 疑似列を含む列リスト）が `schema` に対して構文的に
    /// 自明な誤りを持たないかを、`CopyInResponse`（`G`）送出前に検証する
    /// （codex-review 指摘・Issue #939 レビュー対応）。列名の typo や
    /// 非 nullable 列の欠落は、修正前は各行のデコード時（1 行目の CopyData
    /// 受信後）に初めて [`bind_copy_record`] が検出していたため、クライアントは
    /// 既に copy mode（`CopyInResponse` 受領後）に入ってからデータ送信を
    /// 開始してしまい、PostgreSQL 本家の「copy mode へ入る前に列リストを
    /// テーブル定義と照合して拒否する」契約と乖離していた。本関数はその
    /// 列リストの網羅性（列名の実在性・非 nullable 列の被覆）だけを検証し、
    /// 個々の値の型・NULL 可否は引き続き [`bind_copy_record`] が行単位で担う
    /// （第 2 の検証経路を増やさず、責務を「文レベル」と「行レベル」で分ける）。
    fn validate_columns_against_schema(
        columns: &[String],
        schema: &TableSchema,
    ) -> Result<(), SqlSurfaceError> {
        for name in columns {
            if name == "id" {
                continue;
            }
            if !schema.columns.iter().any(|c| &c.name == name) {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "unknown column: {name}"
                )));
            }
        }
        for column in &schema.columns {
            // `DEFAULT`（TABLE-16・TASK-204、Issue #904）を持つ列は列リストに
            // 無くても [`bind_copy_record`]（`fill_omitted_columns` 経由）が
            // 補うため、事前検証の対象から外す。
            if !column.nullable
                && column.default.is_none()
                && !columns.iter().any(|c| c == &column.name)
            {
                // `bind_copy_record`（`fill_omitted_columns` 経由）・INSERT・行内
                // `\N` 処理と同じ NOT NULL 違反として `23502` へ統一する
                // （PR #1051 codex-review 指摘。列リストでの事前検出とデコード時
                // 検出とでエラー契約が経路依存で揺れないようにする）。
                return Err(SqlSurfaceError::not_null_violation(column.name.clone()));
            }
        }
        Ok(())
    }

    pub(crate) fn new(
        table: String,
        columns: Vec<String>,
        format: CopyFormat,
        operation_id: Option<OperationId>,
        schema: TableSchema,
        limits: BatchLimits,
    ) -> Result<Self, SqlSurfaceError> {
        Self::validate_columns_against_schema(&columns, &schema)?;
        Ok(Self {
            table,
            columns,
            splitter: RecordSplitter::new(format),
            operation_id,
            schema: Box::new(schema),
            limits,
            bounds: Vec::new(),
            running_bytes: 0,
        })
    }

    /// FROM 対象テーブル名（wire-server の CopyInResponse 送出前の分岐でも
    /// 使わないが、テスト・ログ文脈での確認用に公開する）。
    pub fn table(&self) -> &str {
        &self.table
    }

    /// 列リストの列数（`id` 疑似列を含む）。`wire-server::copy` が
    /// `CopyInResponse` の列数フィールドを組み立てるために参照する。
    pub fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// 1 個の CopyData メッセージ本文を取り込む。判定順序:
    /// 1. ③（生 CopyData バイト量の累計）を `pending` へ追加する前に判定
    ///    （`54000`。超過時点で拒否し、それ以降のバイトは一切保持しない）。
    /// 2. 完了したレコードごとに、①（行数）→④（生成チャンク数。1 行=1
    ///    チャンクと読み替える。既存の単文 INSERT 経路と同じ読み替え規則）→
    ///    フィールド分割・エスケープ解決・列型照合（`22000`）→②（当該行の
    ///    デコード後バイト量）の順に判定する。
    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), SqlSurfaceError> {
        self.running_bytes =
            batch_limits::check_running_total(self.running_bytes, chunk.len(), &self.limits)
                .map_err(map_batch_err)?;

        let table = self.table.clone();
        let columns = self.columns.clone();
        let operation_id = self.operation_id.clone();
        let limits = self.limits;
        let schema = &self.schema;
        let bounds = &mut self.bounds;
        let splitter = &mut self.splitter;

        splitter.feed(chunk, |fields| {
            if fields.len() != columns.len() {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "COPY record field count {} does not match column count {}",
                    fields.len(),
                    columns.len()
                )));
            }
            let next_count = bounds.len().saturating_add(1);
            batch_limits::check_row_count(next_count, &limits).map_err(map_batch_err)?;
            batch_limits::validate_chunk_total(next_count, &limits).map_err(map_batch_err)?;
            let bound = bind_copy_record(&table, &columns, &fields, &operation_id, schema)?;
            let row_bytes = bound_insert_byte_len(&bound)?;
            batch_limits::check_row_body_len(bounds.len(), row_bytes, &limits)
                .map_err(map_batch_err)?;
            bounds.push(bound);
            Ok(())
        })
    }

    /// CopyDone 到達時に呼ぶ。改行なしで終わった末尾レコード（存在する場合）を
    /// 確定させたうえで、0 行のままなら拒否する（既存の複数行 `INSERT` バッチと
    /// 同じ「空バッチ拒否」契約）。
    pub fn finish(mut self) -> Result<CopyInBatch, SqlSurfaceError> {
        let tail = self.splitter.finish()?;
        if let Some(fields) = tail {
            if fields.len() != self.columns.len() {
                return Err(SqlSurfaceError::invalid_input(format!(
                    "COPY record field count {} does not match column count {}",
                    fields.len(),
                    self.columns.len()
                )));
            }
            let next_count = self.bounds.len().saturating_add(1);
            batch_limits::check_row_count(next_count, &self.limits).map_err(map_batch_err)?;
            batch_limits::validate_chunk_total(next_count, &self.limits).map_err(map_batch_err)?;
            let bound = bind_copy_record(
                &self.table,
                &self.columns,
                &fields,
                &self.operation_id,
                &self.schema,
            )?;
            let row_bytes = bound_insert_byte_len(&bound)?;
            batch_limits::check_row_body_len(self.bounds.len(), row_bytes, &self.limits)
                .map_err(map_batch_err)?;
            self.bounds.push(bound);
        }

        if self.bounds.is_empty() {
            return Err(SqlSurfaceError::invalid_input(
                "COPY FROM STDIN received no data rows",
            ));
        }

        Ok(CopyInBatch {
            table: self.table,
            row_count: self.bounds.len(),
            operation_id: self.operation_id,
            bounds: self.bounds,
            schema: *self.schema,
        })
    }
}

/// [`CopyInSession::finish`] が返す、commit 直前の束縛済みバッチ
/// （[`crate::core::EngineCore::commit_copy_in`] のみが分解して使う）。
pub struct CopyInBatch {
    pub(crate) table: String,
    pub(crate) row_count: usize,
    pub(crate) operation_id: Option<OperationId>,
    pub(crate) bounds: Vec<BoundInsert>,
    pub(crate) schema: TableSchema,
}

impl CopyInBatch {
    pub fn table(&self) -> &str {
        &self.table
    }

    pub fn row_count(&self) -> usize {
        self.row_count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::ColumnDef;

    fn text_schema() -> TableSchema {
        TableSchema::new(
            "t",
            vec![
                ColumnDef::new("body".to_string(), ColumnType::Text, false),
                ColumnDef::new("note".to_string(), ColumnType::Text, true),
            ],
        )
    }

    #[test]
    fn is_copy_statement_matches_case_insensitively_with_leading_whitespace() {
        assert!(is_copy_statement("  copy t (id) FROM STDIN"));
        assert!(is_copy_statement("COPY (SELECT 1) TO STDOUT"));
        assert!(is_copy_statement("COPY"));
    }

    #[test]
    fn is_copy_statement_rejects_identifier_prefix() {
        assert!(!is_copy_statement("COPYFOO t (id) FROM STDIN"));
        assert!(!is_copy_statement("SELECT 1"));
    }

    /// CSV レコードを丸ごと最後まで供給し、確定したフィールド列（複数
    /// レコードにまたがる場合は最後のレコードのみ）を返すテスト用ヘルパー。
    fn scan_csv_record(input: &[u8]) -> Result<Vec<Option<String>>, SqlSurfaceError> {
        let mut scanner = CsvRecordScanner::new();
        let mut last: Option<Vec<Option<String>>> = None;
        for &b in input {
            if scanner.push_byte(b)? {
                last = Some(scanner.take_record());
            }
        }
        if let Some(fields) = last {
            return Ok(fields);
        }
        scanner
            .finish()?
            .ok_or_else(|| SqlSurfaceError::invalid_input("no CSV record produced"))
    }

    #[test]
    fn record_splitter_splits_on_raw_newline_and_strips_cr() {
        let mut splitter = RecordSplitter::new(CopyFormat::Text);
        let mut records: Vec<Vec<Option<String>>> = Vec::new();
        splitter
            .feed(b"a\tb\r\nc\td\n", |r| {
                records.push(r);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            records,
            vec![
                vec![Some("a".to_string()), Some("b".to_string())],
                vec![Some("c".to_string()), Some("d".to_string())],
            ]
        );
    }

    #[test]
    fn record_splitter_resumes_across_chunk_boundaries() {
        let mut splitter = RecordSplitter::new(CopyFormat::Text);
        let mut records: Vec<Vec<Option<String>>> = Vec::new();
        for i in 0..b"a\tbc\n".len() {
            let byte = b"a\tbc\n"[i..=i].to_vec();
            splitter
                .feed(&byte, |r| {
                    records.push(r);
                    Ok(())
                })
                .unwrap();
        }
        assert_eq!(
            records,
            vec![vec![Some("a".to_string()), Some("bc".to_string())]]
        );
    }

    #[test]
    fn record_splitter_keeps_newline_inside_csv_quotes() {
        let mut splitter = RecordSplitter::new(CopyFormat::Csv);
        let mut records: Vec<Vec<Option<String>>> = Vec::new();
        splitter
            .feed(b"1,\"a\nb\"\n2,c\n", |r| {
                records.push(r);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            records,
            vec![
                vec![Some("1".to_string()), Some("a\nb".to_string())],
                vec![Some("2".to_string()), Some("c".to_string())],
            ]
        );
    }

    #[test]
    fn record_splitter_handles_escaped_quote_pair_without_ending_quoted_region() {
        let mut splitter = RecordSplitter::new(CopyFormat::Csv);
        let mut records: Vec<Vec<Option<String>>> = Vec::new();
        // `"a""b\nc"` は 1 個の引用符フィールド内に `""`（エスケープされた
        // 引用符）と改行を含む。エスケープ後もまだ引用符内であることを
        // 状態機械が正しく維持できるかを確認する。
        splitter
            .feed(b"\"a\"\"b\nc\"\n", |r| {
                records.push(r);
                Ok(())
            })
            .unwrap();
        assert_eq!(records, vec![vec![Some("a\"b\nc".to_string())]]);
    }

    #[test]
    fn decode_text_field_resolves_standard_escapes() {
        assert_eq!(
            decode_text_field(b"a\\tb\\nc\\\\d").unwrap(),
            Some("a\tb\nc\\d".to_string())
        );
    }

    #[test]
    fn decode_text_field_whole_field_backslash_n_is_null() {
        assert_eq!(decode_text_field(b"\\N").unwrap(), None);
    }

    #[test]
    fn decode_text_field_rejects_unknown_escape() {
        assert!(decode_text_field(b"a\\xb").is_err());
    }

    #[test]
    fn csv_record_scanner_distinguishes_null_and_empty_string() {
        let fields = scan_csv_record(b",\"\",value\n").unwrap();
        assert_eq!(
            fields,
            vec![None, Some(String::new()), Some("value".to_string())]
        );
    }

    #[test]
    fn csv_record_scanner_unescapes_doubled_quotes() {
        let fields = scan_csv_record(b"\"a\"\"b\"\n").unwrap();
        assert_eq!(fields, vec![Some("a\"b".to_string())]);
    }

    // advisor 指摘の回帰防止: 改行なしで終わった末尾レコードが `""`
    // （引用符で囲まれた空文字列）だけの場合、`finish` の「新規レコードを
    // 何も消費していない」判定が誤って `field.is_empty()` を見ていたため、
    // `AfterQuote` 状態で `field` が空のまま行が無言で消失していた。
    #[test]
    fn csv_record_scanner_finish_without_trailing_newline_keeps_quoted_empty_string() {
        let mut scanner = CsvRecordScanner::new();
        for &b in b"\"\"" {
            assert!(!scanner.push_byte(b).unwrap());
        }
        let fields = scanner
            .finish()
            .unwrap()
            .expect("record must not be dropped");
        assert_eq!(fields, vec![Some(String::new())]);
    }

    #[test]
    fn csv_record_scanner_finish_without_trailing_newline_keeps_trailing_quoted_empty_field() {
        let mut scanner = CsvRecordScanner::new();
        for &b in b"a,\"\"" {
            assert!(!scanner.push_byte(b).unwrap());
        }
        let fields = scanner
            .finish()
            .unwrap()
            .expect("record must not be dropped");
        assert_eq!(fields, vec![Some("a".to_string()), Some(String::new())]);
    }

    #[test]
    fn csv_record_scanner_rejects_unterminated_quote() {
        let mut scanner = CsvRecordScanner::new();
        for &b in b"\"abc" {
            scanner.push_byte(b).unwrap();
        }
        let err = scanner.finish().unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // Issue #939 codex-review 指摘（discussion_r4096720883）: フィールド先頭
    // 以外で生の `"` が現れる不正配置（`ab"cd",x`）を正常データとして受理して
    // いた回帰の防止。
    #[test]
    fn csv_record_scanner_rejects_quote_mid_unquoted_field() {
        let mut scanner = CsvRecordScanner::new();
        let mut err = None;
        for &b in b"ab\"cd\",x\n" {
            if let Err(e) = scanner.push_byte(b) {
                err = Some(e);
                break;
            }
        }
        let err = err.expect("misplaced quote must be rejected");
        assert_eq!(err.wire_code(), "22000");
    }

    // 同上（discussion_r4096720883）: 閉じ引用符の直後に区切り・レコード
    // 終端・エスケープ以外の文字が続く不正配置（`"ab"junk,x`）を正常データ
    // として受理していた回帰の防止。
    #[test]
    fn csv_record_scanner_rejects_data_immediately_after_closing_quote() {
        let mut scanner = CsvRecordScanner::new();
        let mut err = None;
        for &b in b"\"ab\"junk,x\n" {
            if let Err(e) = scanner.push_byte(b) {
                err = Some(e);
                break;
            }
        }
        let err = err.expect("data right after closing quote must be rejected");
        assert_eq!(err.wire_code(), "22000");
    }

    // codex-review 指摘（PRRT_kwDOUAKASM6ltrHX・PRRT_kwDOUAKASM6ltyY9）:
    // 閉じ引用符直後の CR は CRLF の一部としてのみ許容し、直後に区切り文字が
    // 続く（LF が続かない）場合は不正配置として `22000` で拒否する回帰防止。
    #[test]
    fn csv_record_scanner_rejects_cr_followed_by_comma_after_closing_quote() {
        let mut scanner = CsvRecordScanner::new();
        let mut err = None;
        for &b in b"\"a\"\r,b\n" {
            if let Err(e) = scanner.push_byte(b) {
                err = Some(e);
                break;
            }
        }
        let err = err.expect("lone CR before comma must be rejected");
        assert_eq!(err.wire_code(), "22000");
    }

    // 同上: 改行なしで単独 CR のまま CopyDone（`finish`）へ到達した場合も、
    // CRLF が完成しない不正配置として拒否する回帰防止。
    #[test]
    fn csv_record_scanner_rejects_lone_trailing_cr_at_finish() {
        let mut scanner = CsvRecordScanner::new();
        for &b in b"\"a\"\r" {
            assert!(!scanner.push_byte(b).unwrap());
        }
        let err = scanner.finish().unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }

    // 正常系: `\r\n` が揃った場合は従来どおりレコード終端として受理される。
    #[test]
    fn csv_record_scanner_accepts_proper_crlf_after_closing_quote() {
        let fields = scan_csv_record(b"\"a\"\r\n").unwrap();
        assert_eq!(fields, vec![Some("a".to_string())]);
    }

    #[test]
    fn bind_copy_record_rejects_null_for_non_nullable_column() {
        let schema = text_schema();
        let columns = vec!["id".to_string(), "body".to_string()];
        let fields = vec![Some("1".to_string()), None];
        let err = bind_copy_record("t", &columns, &fields, &None, &schema).unwrap_err();
        // TABLE-16・TASK-204、Issue #904: NOT NULL 違反は `23502`
        // （`NotNullViolation`）へ写像する（旧 `22000` から契約変更）。
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn bind_copy_record_accepts_null_for_nullable_column() {
        let schema = text_schema();
        let columns = vec!["id".to_string(), "body".to_string(), "note".to_string()];
        let fields = vec![Some("1".to_string()), Some("hello".to_string()), None];
        let bound = bind_copy_record("t", &columns, &fields, &None, &schema).unwrap();
        assert_eq!(bound.id, 1);
        assert_eq!(
            bound.values,
            vec![Value::Text("hello".to_string()), Value::Null]
        );
    }

    #[test]
    fn bind_copy_record_rejects_missing_id_column() {
        let schema = text_schema();
        let columns = vec!["body".to_string()];
        let fields = vec![Some("hello".to_string())];
        let err = bind_copy_record("t", &columns, &fields, &None, &schema).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }
}
