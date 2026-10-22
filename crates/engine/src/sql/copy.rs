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
use crate::sql::copy_options::{CopyHeader, CopyOptions};
use crate::sql::parser::{
    bind_bytea_literal, bind_datetime_literal, bind_double_literal, bind_enum_literal,
    bind_integer_literal, bind_json_literal, bind_numeric_literal, bind_real_copy_field,
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
    /// 引用符付きフィールド内で `ESCAPE`（`QUOTE` と異なる場合のみ）を受けた直後
    /// （Issue #1764）。次バイトが `QUOTE`／`ESCAPE` ならそのバイトをデータとして積み、
    /// それ以外なら `ESCAPE` 自体をデータとして積んでから次バイトを通常データとして扱う
    /// （PostgreSQL の COPY CSV と同じ意味）。チャンク境界をまたいでも状態で保持する。
    QuotedEscape,
}

/// CSV 形式のレコード走査器。区切り・引用・エスケープ・NULL 文字列は `COPY ... WITH (...)` の
/// 解決済みオプション（[`CopyOptions`]。Issue #1764・WIRE-22 のポインタ）で決まる。
#[derive(Debug)]
struct CsvRecordScanner {
    state: CsvFieldState,
    quoted: bool,
    field: Vec<u8>,
    fields: Vec<Option<String>>,
    delimiter: u8,
    quote: u8,
    escape: u8,
    null_string: Vec<u8>,
}

impl CsvRecordScanner {
    /// 既定オプション（`,`・`"`・NULL は空欄）の走査器（単体テスト用）。
    #[cfg(test)]
    fn new() -> Self {
        Self::with_options(&crate::sql::copy_options::defaults_for_format(
            CopyFormat::Csv,
        ))
    }

    fn with_options(options: &CopyOptions) -> Self {
        Self {
            state: CsvFieldState::Start,
            quoted: false,
            field: Vec::new(),
            fields: Vec::new(),
            delimiter: options.delimiter(),
            quote: options.quote(),
            escape: options.escape(),
            null_string: options.null_string().as_bytes().to_vec(),
        }
    }

    fn push_field(&mut self) -> Result<(), SqlSurfaceError> {
        let bytes = std::mem::take(&mut self.field);
        let quoted = std::mem::replace(&mut self.quoted, false);
        self.fields
            .push(finish_csv_field(bytes, quoted, &self.null_string)?);
        self.state = CsvFieldState::Start;
        Ok(())
    }

    /// 1 バイトを処理する。レコード終端（`\n`）に到達し `fields` が確定した
    /// 場合のみ `true` を返す（呼び出し元は直後に [`Self::take_record`] で
    /// 取り出す）。
    fn push_byte(&mut self, b: u8) -> Result<bool, SqlSurfaceError> {
        match self.state {
            CsvFieldState::Start => {
                if b == self.quote {
                    self.state = CsvFieldState::Quoted;
                    self.quoted = true;
                    Ok(false)
                } else if b == self.delimiter {
                    self.push_field()?;
                    Ok(false)
                } else if b == b'\n' {
                    self.push_field()?;
                    Ok(true)
                } else if b == b'\r' {
                    // 裸の `\r` はまだ「引用符なしフィールド中のデータ」か
                    // 「CRLF レコード終端の一部」かが確定できないため
                    // `UnquotedCr` へ移行し、次バイトで判定する（フィールド
                    // 先頭で受けた場合も、後続の `\n` と対にして扱う）。
                    self.state = CsvFieldState::UnquotedCr;
                    Ok(false)
                } else {
                    self.state = CsvFieldState::Unquoted;
                    self.field.push(b);
                    Ok(false)
                }
            }
            CsvFieldState::Unquoted => {
                if b == self.quote {
                    // フィールド先頭以外での生引用符は不正配置（`ab"cd",x` 相当）。
                    Err(SqlSurfaceError::invalid_input(
                        "misplaced double quote in COPY CSV field (quote allowed only at field start)",
                    ))
                } else if b == self.delimiter {
                    self.push_field()?;
                    Ok(false)
                } else if b == b'\n' {
                    self.push_field()?;
                    Ok(true)
                } else if b == b'\r' {
                    // `Start` と同じく、裸の `\r` は `UnquotedCr` へ移行して
                    // 次バイトで判定する（フィールドへは積まない）。
                    self.state = CsvFieldState::UnquotedCr;
                    Ok(false)
                } else {
                    // 引用符外の `ESCAPE` バイトはリテラル（PostgreSQL と同じ）。
                    self.field.push(b);
                    Ok(false)
                }
            }
            CsvFieldState::Quoted => {
                if self.escape != self.quote && b == self.escape {
                    self.state = CsvFieldState::QuotedEscape;
                } else if b == self.quote {
                    // ここではエスケープ（`ESCAPE == QUOTE` で直後にもう 1 個
                    // 引用符）か閉じ引用符かがまだ確定できないため `AfterQuote`
                    // へ移行し、次バイトで判定する。
                    self.state = CsvFieldState::AfterQuote;
                } else {
                    // 引用符内の生改行はデータ（埋め込み改行）。
                    self.field.push(b);
                }
                Ok(false)
            }
            CsvFieldState::QuotedEscape => {
                if b != self.quote && b != self.escape {
                    // `ESCAPE` は後続が `QUOTE`／`ESCAPE` のときだけ特別扱いで、
                    // それ以外では `ESCAPE` 自体がデータ。
                    self.field.push(self.escape);
                }
                self.field.push(b);
                self.state = CsvFieldState::Quoted;
                Ok(false)
            }
            CsvFieldState::AfterQuote => {
                if b == self.quote {
                    if self.escape == self.quote {
                        // 直前の引用符と合わせてエスケープされた引用符。
                        self.field.push(b);
                        self.state = CsvFieldState::Quoted;
                        Ok(false)
                    } else {
                        // `ESCAPE != QUOTE` では閉じ引用符直後の引用符は不正配置
                        // （PostgreSQL は引用の再開として受理するが、本実装は
                        // 「引用はフィールド先頭のみ」を維持して拒否側に倒す）。
                        Err(SqlSurfaceError::invalid_input(
                            "unexpected byte after closing double quote in COPY CSV field",
                        ))
                    }
                } else if b == self.delimiter {
                    self.push_field()?;
                    Ok(false)
                } else if b == b'\n' {
                    self.push_field()?;
                    Ok(true)
                } else if b == b'\r' {
                    // `\r\n` レコード終端の `\r` 側。まだ `\n` が続くかどうかが
                    // 確定していないため `AfterQuoteCr` へ移行し、次バイトで
                    // 判定する（フィールドへは積まない）。
                    self.state = CsvFieldState::AfterQuoteCr;
                    Ok(false)
                } else {
                    // 閉じ引用符の直後に区切り・レコード終端・エスケープ以外の
                    // バイトが来るのは不正配置（`"ab"junk,x` 相当）。
                    Err(SqlSurfaceError::invalid_input(
                        "unexpected byte after closing double quote in COPY CSV field",
                    ))
                }
            }
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
        if matches!(
            self.state,
            CsvFieldState::Quoted | CsvFieldState::QuotedEscape
        ) {
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
    Text {
        pending: Vec<u8>,
        pending_cr: bool,
        delimiter: u8,
        null_string: Vec<u8>,
    },
    Csv(CsvRecordScanner),
    /// binary 形式。フレーム解析と型変換は wire-server が担い、engine へはレコード単位で
    /// [`CopyInSession::feed_fields`] が届くため、バイト列の分割は行わない。
    Binary,
}

impl RecordSplitter {
    /// 既定オプションの分割器（単体テスト用）。
    #[cfg(test)]
    fn new(format: CopyFormat) -> Self {
        Self::with_options(
            format,
            &crate::sql::copy_options::defaults_for_format(format),
        )
    }

    /// `options`（区切り・NULL 文字列・引用・エスケープ）を適用する分割器。
    fn with_options(format: CopyFormat, options: &CopyOptions) -> Self {
        match format {
            CopyFormat::Text => RecordSplitter::Text {
                pending: Vec::new(),
                pending_cr: false,
                delimiter: options.delimiter(),
                null_string: options.null_string().as_bytes().to_vec(),
            },
            CopyFormat::Csv => RecordSplitter::Csv(CsvRecordScanner::with_options(options)),
            CopyFormat::Binary => RecordSplitter::Binary,
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
                delimiter,
                null_string,
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
                            on_record(decode_text_record(&record, *delimiter, null_string)?)?;
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
                            on_record(decode_text_record(&record, *delimiter, null_string)?)?;
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
            // binary セッションでは `feed` を呼ばない契約。呼ばれたら内部不整合として拒否する
            // （fail-closed。バイト列を無視して成功を返さない）。
            RecordSplitter::Binary => Err(SqlSurfaceError::Internal {
                detail: "byte feed on a binary COPY session".to_string(),
            }),
        }
    }

    /// CopyDone 到達時に呼ぶ。改行なしで終わった末尾レコード（存在する
    /// 場合）のフィールド列を返す。
    fn finish(self) -> Result<Option<Vec<Option<String>>>, SqlSurfaceError> {
        match self {
            RecordSplitter::Text {
                pending,
                pending_cr,
                delimiter,
                null_string,
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
                    Ok(Some(decode_text_record(&pending, delimiter, &null_string)?))
                }
            }
            RecordSplitter::Csv(scanner) => scanner.finish(),
            RecordSplitter::Binary => Ok(None),
        }
    }
}

/// text 形式の 1 フィールド（区切りで分割済み・エスケープ未解決の生バイト列）をデコードする。
/// 生バイト列が `null_string`（既定 `\N`）と完全一致する場合のみ NULL とする（エスケープ解決前に
/// 判定するため、`\N` が末尾以外の位置に現れた場合は通常のエスケープ解決に入り、`N` は未対応
/// エスケープとして拒否される。PostgreSQL 本家がフィールド全体一致でのみ NULL 表現を特別扱い
/// する契約と同じ）。エスケープは PostgreSQL 互換の `\\`／`\t`／`\n`／`\r`／`\b`／`\f`／`\v` と
/// `\<区切り文字>`（区切り文字そのもの）を受理し、それ以外（8 進・16 進を含む）は fail-closed に
/// `22000` で拒否する。
fn decode_text_field(
    raw: &[u8],
    delimiter: u8,
    null_string: &[u8],
) -> Result<Option<String>, SqlSurfaceError> {
    if raw == null_string {
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
            Some(c) if c == delimiter => out.push(c),
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

/// text 形式の 1 レコードを `delimiter` でフィールドへ分割する。バックスラッシュは次の 1 バイトと
/// 組で扱い、エスケープされた区切り文字（`\<区切り>`）ではフィールドを分割しない
/// （PostgreSQL の COPY TO がカスタム区切りをこの形で出力するため）。エスケープの解決自体は
/// [`decode_text_field`] の責務。バイト単位で安全なのは、区切り・バックスラッシュが ASCII で
/// UTF-8 の多バイト列中に現れないため。
fn decode_text_record(
    record: &[u8],
    delimiter: u8,
    null_string: &[u8],
) -> Result<Vec<Option<String>>, SqlSurfaceError> {
    let mut fields = Vec::new();
    let mut raw: Vec<u8> = Vec::new();
    let mut iter = record.iter().copied();
    while let Some(b) = iter.next() {
        if b == b'\\' {
            raw.push(b);
            if let Some(next) = iter.next() {
                raw.push(next);
            }
        } else if b == delimiter {
            fields.push(decode_text_field(&raw, delimiter, null_string)?);
            raw.clear();
        } else {
            raw.push(b);
        }
    }
    fields.push(decode_text_field(&raw, delimiter, null_string)?);
    Ok(fields)
}

/// [`CsvRecordScanner::push_field`] が使う、1 フィールド分の生バイト列を
/// NULL／空文字列／通常値へ写像する共通処理（引用符で囲まれておらず `NULL` 文字列と一致する
/// フィールドは NULL〔既定は空欄〕、引用符で囲まれたフィールドは決して NULL にせず、`""` は
/// 空文字列として区別する）。
fn finish_csv_field(
    bytes: Vec<u8>,
    quoted: bool,
    null_string: &[u8],
) -> Result<Option<String>, SqlSurfaceError> {
    if !quoted && bytes == null_string {
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
    clock: crate::sql::time_fn::StatementClock,
) -> Result<BoundInsert, SqlSurfaceError> {
    // 疑似列 `id` を持つのは従来方式の表だけ（TABLE-23・Issue #2199）。採番方式の表では行の識別子は
    // 書き込み段が払い出し、`BoundInsert.id` は使われない固定値 0。
    //
    // 移行済み表（`MigratedId`）は列リストからの `id` 省略を受理する（Issue #1700。採番は書き込み段が
    // 所有 SEQUENCE から行う）。列リストに `id` を含めて `\N` を渡す形は従来どおり `22000`。
    let has_pseudo_id = schema.has_pseudo_id();
    let id_omitted = has_pseudo_id && schema.is_migrated_id() && !columns.iter().any(|c| c == "id");
    let id: u64 = if id_omitted {
        0
    } else if has_pseudo_id {
        let id_pos = columns.iter().position(|c| c == "id").ok_or_else(|| {
            SqlSurfaceError::invalid_input("COPY column list must include the id pseudo-column")
        })?;
        let id_field = fields.get(id_pos).and_then(|f| f.as_ref()).ok_or_else(|| {
            SqlSurfaceError::invalid_input("id pseudo-column value must not be NULL")
        })?;
        id_field.parse().map_err(|_| {
            SqlSurfaceError::invalid_input(format!("malformed id value: {id_field}"))
        })?
    } else {
        0
    };

    let mut bound_values: Vec<Value> = vec![Value::Null; schema.columns.len()];
    let mut provided = vec![false; schema.columns.len()];

    for (name, field) in columns.iter().zip(fields.iter()) {
        if has_pseudo_id && name == "id" {
            continue;
        }
        let col_idx = schema
            .columns
            .iter()
            .position(|c| &c.name == name)
            .ok_or_else(|| SqlSurfaceError::undefined_column(name))?;
        let column = schema
            .columns
            .get(col_idx)
            .ok_or_else(|| SqlSurfaceError::undefined_column(name))?;
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
                // `CHAR(n)` は空白詰めして保存する（TABLE-31・Issue #1566）。
                ColumnType::Text | ColumnType::Varchar(_) | ColumnType::Bpchar(_) => {
                    crate::sql::parser::bind_text_literal(&column.ty, s)?
                }
                ColumnType::Boolean => Value::Bool(parse_copy_boolean(s, name)?),
                ColumnType::Array(array_ty) => Value::Array(parse_array_literal(s, array_ty)?),
                ColumnType::Bytea => bind_bytea_literal(s, name)?,
                ColumnType::Enum(def) => bind_enum_literal(def, s, name)?,
                ColumnType::Json | ColumnType::Jsonb => bind_json_literal(s, &column.ty, name)?,
                ColumnType::Date
                | ColumnType::Timestamp
                | ColumnType::Time
                | ColumnType::TimestampTz
                | ColumnType::Interval => bind_datetime_literal(name, column.ty.clone(), s)?,
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
                // UPSERT と同じエラー写像（`scalar_float.rs` の閉じた文法。非有限化・
                // 非ゼロアンダーフローは `22003`）を共有する。`REAL` のみ、`COPY TO`
                // の出力（指数表記を含む。Issue #1173）を往復できるよう指数表記を
                // 受理する `bind_real_copy_field` を使う（main マージ時点で
                // COPY 側の網羅 match が追随しておらずビルド不能だった分の
                // 追加）。
                ColumnType::Real => Value::Real(bind_real_copy_field(s)?),
                ColumnType::Double => Value::Double(bind_double_literal(s)?),
                // `INTEGER`／`BIGINT`（Issue #881）も INSERT／UPDATE／UPSERT と
                // 同じ `bind_integer_literal` を共有する。COPY のフィールドは
                // 文字列でしか届かないため `InsertLiteral::Number` として包み
                // 直し、`InsertLiteral::String` 拒否（PG 互換の暗黙変換は
                // 行わない設計判断）を経由させずに数値リテラル同等の検証
                // （範囲外 `22003`・非整数形式 `22P02`）を適用する。
                ColumnType::SmallInt | ColumnType::Integer | ColumnType::BigInt => {
                    bind_integer_literal(
                        name,
                        column.ty.clone(),
                        &InsertLiteral::Number(s.clone()),
                    )?
                }
            },
        };
        if let Some(slot) = bound_values.get_mut(col_idx) {
            *slot = value;
        }
        if let Some(flag) = provided.get_mut(col_idx) {
            *flag = true;
        }
    }

    let volatile_default_columns = crate::sql::parser::fill_omitted_columns(
        &schema.columns,
        &mut bound_values,
        &provided,
        Some(clock),
        false,
    )?;
    // `DEFAULT nextval` の列は COPY では採番しない（TABLE-24・Issue #1690。fail-closed）。
    if volatile_default_columns.iter().any(|c| {
        matches!(
            c,
            crate::sql::parser::VolatileColumn::DefaultSequence(_)
                | crate::sql::parser::VolatileColumn::Sequence(..)
        )
    }) {
        return Err(crate::sql::parser::reject_sequence_call_literal());
    }

    Ok(BoundInsert {
        table: table_name.to_string(),
        id,
        values: bound_values,
        operation_id: operation_id.clone(),
        volatile_default_columns,
        id_omitted,
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
            // TIME 値は行コーデック上 8 バイト固定（Issue #1569）。
            Value::Time(_) => 8,
            Value::TimestampTz(_) => 8,
            // INTERVAL 値は行コーデック上 16 バイト固定（Issue #1579）。
            Value::Interval(_) => 16,
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

/// ヘッダ行（最初のレコード）を処理する。`HEADER true` は内容を見ずに破棄し、`HEADER MATCH` は
/// フィールド数と各列名（NULL 不可・大小区別）が COPY の列リストと一致することを検査する
/// （PostgreSQL 17 と同じ）。エラー文言にはヘッダの内容を含めない（Issue #1764・WIRE-22）。
fn check_header_record(
    header: CopyHeader,
    columns: &[String],
    fields: &[Option<String>],
) -> Result<(), SqlSurfaceError> {
    if header != CopyHeader::Match {
        return Ok(());
    }
    let matches = fields.len() == columns.len()
        && fields
            .iter()
            .zip(columns.iter())
            .all(|(f, c)| f.as_deref() == Some(c.as_str()));
    if matches {
        Ok(())
    } else {
        Err(SqlSurfaceError::invalid_input(
            "COPY header line does not match the column list",
        ))
    }
}

/// `COPY <table> (<cols>) FROM STDIN` の逐次取り込み状態（Issue #939・
/// WIRE-17）。構築は [`crate::core::EngineCore::begin_copy`] のみが行う。
/// [`Self::feed`] を CopyData ごとに呼び、INDEX-4 の 4 上限を CopyDone より
/// 前に逐次判定する。[`Self::finish`] で末尾レコードを確定させ、実際の
/// commit（[`crate::core::EngineCore::commit_copy_in`]）へ渡す
/// [`CopyInBatch`] を返す。
#[derive(Debug)]
pub struct CopyInSession {
    // `Box` で包む: オプション（区切り・NULL 文字列）を保持して大きくなった分で `CopyPlan::From` が
    // `clippy::large_enum_variant` に抵触しないよう、公開 API の内包型を変えずにサイズを抑える。
    splitter: Box<RecordSplitter>,
    /// レコード 1 件分の束縛・上限判定・蓄積（text/csv の `feed`・`finish` と binary の
    /// [`Self::feed_fields`] が同じ束縛経路を共有するための共通部分）。
    sink: Box<RecordSink>,
    format: CopyFormat,
    running_bytes: usize,
    /// 一時表（TABLE-44・Issue #1684）を対象にする COPY か。`Some` のとき commit は一時コアが
    /// 一時 DB へ行う（`EngineCore::commit_copy_in_txn_in_session`）。永続の入口
    /// （`commit_copy_in`／`commit_copy_in_txn`）は一時表向けのバッチを `0A000` で拒否する。
    temp: Option<crate::sql::temp_rows::TempCopyTarget>,
}

/// COPY の 1 列分の種別（binary 形式の型変換を wire-server が選ぶための公開情報。
/// WIRE-23・Issue #1769）。COPY 列リストの順に並ぶ。
#[derive(Debug, Clone, PartialEq)]
pub enum CopyColumnKind {
    /// 疑似列 `id`（`bind_copy_record` が u64 の 10 進テキストとして解釈する。公告型は numeric）。
    PseudoId,
    /// 通常の列とその型。
    Column(ColumnType),
}

/// COPY のレコード単位の束縛状態。`CopyInSession` が所有し、text/csv の分割器が確定した
/// レコードと binary 形式で wire-server が変換済みのレコードを、同一の経路
/// （フィールド数照合 → ① → ④ → [`bind_copy_record`] → ②）で取り込む。
#[derive(Debug)]
struct RecordSink {
    table: String,
    columns: Vec<String>,
    operation_id: Option<OperationId>,
    // `Box`: `TableSchema` へ墓標スロット（TABLE-19・TASK-203・Issue #901）が加わりサイズが増えたため。
    schema: Box<TableSchema>,
    limits: BatchLimits,
    /// `HEADER` 指定（Issue #1764）。`False` 以外のとき最初の 1 レコードをヘッダ行として扱う。
    header: CopyHeader,
    /// 最初のレコード（ヘッダ行）をまだ処理していないか。
    header_pending: bool,
    bounds: Vec<BoundInsert>,
    /// 列 `DEFAULT now()` 系の評価時刻（SQL-37・Issue #1587）。COPY は行ごとの束縛が複数の
    /// wire メッセージにまたがり RAII のスレッドローカルスコープが届かないため、
    /// 開始時に `core.rs` が確定した値をここへ保持して全行で共有する（全行同じ時刻）。
    clock: crate::sql::time_fn::StatementClock,
    /// いずれかの行で時刻 DEFAULT が評価されたか（台帳 v3 の評価時刻を残す判定。Issue #2188）。
    clock_used: bool,
    /// `COPY` 開始時点のセッション TimeZone（TABLE-33・Issue #1575）。スレッドローカルのスコープが
    /// メッセージ間で届かないため開始時に確定した値を保持し、束縛のたびにスコープへ入れる。
    time_zone: Box<crate::datetime::SessionTimeZone>,
}

impl RecordSink {
    /// 1 レコード分のフィールドを取り込む。判定順序は ヘッダ処理 → フィールド数照合 →
    /// ①（行数）→ ④（チャンク数）→ 列型照合・束縛 → ②（行あたりのバイト量）。
    fn push_record(&mut self, fields: Vec<Option<String>>) -> Result<(), SqlSurfaceError> {
        // 最初に確定したレコードはヘッダ行。破棄（または照合）するだけで、
        // 行数①・チャンク数④・束縛の対象に含めない。
        if std::mem::take(&mut self.header_pending) {
            return check_header_record(self.header, &self.columns, &fields);
        }
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
            self.clock,
        )?;
        let row_bytes = bound_insert_byte_len(&bound)?;
        batch_limits::check_row_body_len(self.bounds.len(), row_bytes, &self.limits)
            .map_err(map_batch_err)?;
        self.bounds.push(bound);
        Ok(())
    }

    /// セッション TimeZone と DEFAULT 時刻のスコープに入って `f` を実行し、時刻 DEFAULT が
    /// 評価されたかを `clock_used` へ回収する（スコープ外のスレッド状態へ漏らさない。Issue #2188）。
    fn with_scopes<T>(&mut self, f: impl FnOnce(&mut Self) -> T) -> T {
        let _tz_scope = crate::datetime::SessionTimeZoneScope::enter((*self.time_zone).clone());
        let scope = crate::sql::time_fn::DefaultClockScope::enter(self.clock);
        let result = f(self);
        self.clock_used |= scope.was_used();
        result
    }
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
        // 従来方式の表は疑似列 `id` が必須（採番方式の表は `id` を要求しない。Issue #2199。移行済み表は
        // 省略でき、書き込み段が所有 SEQUENCE から採番する。Issue #1700）。
        if schema.has_pseudo_id() && !schema.is_migrated_id() && !columns.iter().any(|c| c == "id")
        {
            return Err(SqlSurfaceError::invalid_input(
                "COPY column list must include the id pseudo-column",
            ));
        }
        for name in columns {
            if schema.has_pseudo_id() && name == "id" {
                continue;
            }
            if !schema.columns.iter().any(|c| &c.name == name) {
                return Err(SqlSurfaceError::undefined_column(name));
            }
        }
        for column in &schema.columns {
            // `DEFAULT`（TABLE-16・TASK-204、Issue #904）を持つ列は列リストに
            // 無くても [`bind_copy_record`]（`fill_omitted_columns` 経由）が
            // 補うため、事前検証の対象から外す。
            if !column.nullable
                && column.default_expr.is_none()
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

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        table: String,
        columns: Vec<String>,
        format: CopyFormat,
        options: CopyOptions,
        operation_id: Option<OperationId>,
        schema: TableSchema,
        limits: BatchLimits,
        clock: crate::sql::time_fn::StatementClock,
        time_zone: crate::datetime::SessionTimeZone,
    ) -> Result<Self, SqlSurfaceError> {
        // csv で QUOTE／ESCAPE が改行系だとレコード終端と曖昧になるため、未対応として
        // CopyInResponse 前に拒否する（PostgreSQL は検査しないが拒否側に倒す既知の相違）。
        if format == CopyFormat::Csv
            && [options.quote(), options.escape()]
                .iter()
                .any(|b| *b == b'\n' || *b == b'\r')
        {
            return Err(SqlSurfaceError::feature_not_supported(
                "COPY quote or escape cannot be newline or carriage return",
            ));
        }
        Self::validate_columns_against_schema(&columns, &schema)?;
        Ok(Self {
            splitter: Box::new(RecordSplitter::with_options(format, &options)),
            sink: Box::new(RecordSink {
                table,
                columns,
                operation_id,
                schema: Box::new(schema),
                limits,
                header: options.header(),
                header_pending: options.header() != CopyHeader::False,
                bounds: Vec::new(),
                clock,
                clock_used: false,
                time_zone: Box::new(time_zone),
            }),
            format,
            running_bytes: 0,
            temp: None,
        })
    }

    /// 一時表向けの COPY として印を付ける（`EngineCore::begin_copy` の一時表の経路が呼ぶ）。
    #[must_use]
    pub(crate) fn with_temp(mut self, target: crate::sql::temp_rows::TempCopyTarget) -> Self {
        self.temp = Some(target);
        self
    }

    /// FROM 対象テーブル名（wire-server の CopyInResponse 送出前の分岐でも
    /// 使わないが、テスト・ログ文脈での確認用に公開する）。
    pub fn table(&self) -> &str {
        &self.sink.table
    }

    /// 列リストの列数（`id` 疑似列を含む）。`wire-server::copy` が
    /// `CopyInResponse` の列数フィールドを組み立てるために参照する。
    pub fn column_count(&self) -> usize {
        self.sink.columns.len()
    }

    /// 転送形式。`wire-server::copy::run_copy_from` が text/csv と binary の取り込み経路を
    /// 選ぶために参照する（WIRE-23・Issue #1769）。
    pub fn format(&self) -> CopyFormat {
        self.format
    }

    /// セッションの INDEX-4 上限。`wire-server::copy` が binary フレーム層のメモリ上限を
    /// 決めるために参照する（確保前の防御であり、実際の上限判定は engine が担う）。
    pub fn batch_limits(&self) -> BatchLimits {
        self.sink.limits
    }

    /// COPY 列リストの順に並べた各列の種別。`wire-server::copy` が binary フィールドの
    /// 復号方式を列型から選ぶために参照する。疑似列 `id` の判定は `bind_copy_record` と同じ規則。
    /// 列リストは `begin_copy` で検証済みのため未知の列名は到達しないが、到達した場合は
    /// `undefined_column` で拒否する（fail-closed）。
    pub fn column_kinds(&self) -> Result<Vec<CopyColumnKind>, SqlSurfaceError> {
        let schema = &self.sink.schema;
        self.sink
            .columns
            .iter()
            .map(|name| {
                if schema.has_pseudo_id() && name == "id" {
                    return Ok(CopyColumnKind::PseudoId);
                }
                schema
                    .columns
                    .iter()
                    .find(|c| &c.name == name)
                    .map(|c| CopyColumnKind::Column(c.ty.clone()))
                    .ok_or_else(|| SqlSurfaceError::undefined_column(name))
            })
            .collect()
    }

    /// ③（生 CopyData バイト量の累計）に `len` バイトを加算して判定する（`54000`）。
    /// binary 形式で wire-server がフレーム解析へ渡す直前の CopyData payload 長ごとに呼ぶ
    /// （text/csv は [`Self::feed`] が同じ判定を内部で行う）。
    pub fn account_raw_bytes(&mut self, len: usize) -> Result<(), SqlSurfaceError> {
        self.running_bytes =
            batch_limits::check_running_total(self.running_bytes, len, &self.sink.limits)
                .map_err(map_batch_err)?;
        Ok(())
    }

    /// 1 個の CopyData メッセージ本文を取り込む（text/csv 形式）。判定順序:
    /// 1. ③（生 CopyData バイト量の累計）を `pending` へ追加する前に判定
    ///    （`54000`。超過時点で拒否し、それ以降のバイトは一切保持しない）。
    /// 2. 完了したレコードごとに、①（行数）→④（生成チャンク数。1 行=1
    ///    チャンクと読み替える。既存の単文 INSERT 経路と同じ読み替え規則）→
    ///    フィールド分割・エスケープ解決・列型照合（`22000`）→②（当該行の
    ///    デコード後バイト量）の順に判定する。
    pub fn feed(&mut self, chunk: &[u8]) -> Result<(), SqlSurfaceError> {
        self.account_raw_bytes(chunk.len())?;
        let splitter = &mut self.splitter;
        self.sink
            .with_scopes(|sink| splitter.feed(chunk, |fields| sink.push_record(fields)))
    }

    /// binary 形式で型変換済みの 1 レコード分のフィールド（正規テキスト。`None` = NULL）を取り込む。
    /// text/csv の [`Self::feed`] と同じ束縛経路（①→④→束縛→②）を共有する（第 2 の書き込み経路を
    /// 作らない。WIRE-23・Issue #1769）。呼び出しは `wire-server::copy::run_copy_from` から。
    pub fn feed_fields(&mut self, fields: Vec<Option<String>>) -> Result<(), SqlSurfaceError> {
        self.sink.with_scopes(|sink| sink.push_record(fields))
    }

    /// CopyDone 到達時に呼ぶ。改行なしで終わった末尾レコード（存在する場合）を
    /// 確定させたうえで、0 行のままなら拒否する（既存の複数行 `INSERT` バッチと
    /// 同じ「空バッチ拒否」契約）。
    pub fn finish(self) -> Result<CopyInBatch, SqlSurfaceError> {
        let Self {
            splitter,
            mut sink,
            temp,
            ..
        } = self;
        if let Some(fields) = splitter.finish()? {
            sink.with_scopes(|sink| sink.push_record(fields))?;
        }

        if sink.bounds.is_empty() {
            return Err(SqlSurfaceError::invalid_input(
                "COPY FROM STDIN received no data rows",
            ));
        }

        let sink = *sink;
        Ok(CopyInBatch {
            table: sink.table,
            row_count: sink.bounds.len(),
            operation_id: sink.operation_id,
            bounds: sink.bounds,
            schema: *sink.schema,
            evaluated_at: sink.clock_used.then_some(sink.clock.txn_start_micros),
            temp,
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
    /// 時刻 DEFAULT を評価した場合の台帳 v3 評価時刻（トランザクション開始時刻。Issue #2188）。
    pub(crate) evaluated_at: Option<i64>,
    /// 一時表向けのバッチなら対象ストアと準備時の一時カタログ世代（`CopyInSession::with_temp`）。
    pub(crate) temp: Option<crate::sql::temp_rows::TempCopyTarget>,
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
            decode_text_field(b"a\\tb\\nc\\\\d", b'\t', b"\\N").unwrap(),
            Some("a\tb\nc\\d".to_string())
        );
    }

    #[test]
    fn decode_text_field_whole_field_backslash_n_is_null() {
        assert_eq!(decode_text_field(b"\\N", b'\t', b"\\N").unwrap(), None);
    }

    #[test]
    fn decode_text_field_rejects_unknown_escape() {
        assert!(decode_text_field(b"a\\xb", b'\t', b"\\N").is_err());
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

    fn test_clock() -> crate::sql::time_fn::StatementClock {
        crate::sql::time_fn::StatementClock {
            txn_start_micros: 0,
            stmt_start_micros: 0,
        }
    }

    #[test]
    fn bind_copy_record_rejects_null_for_non_nullable_column() {
        let schema = text_schema();
        let columns = vec!["id".to_string(), "body".to_string()];
        let fields = vec![Some("1".to_string()), None];
        let err =
            bind_copy_record("t", &columns, &fields, &None, &schema, test_clock()).unwrap_err();
        // TABLE-16・TASK-204、Issue #904: NOT NULL 違反は `23502`
        // （`NotNullViolation`）へ写像する（旧 `22000` から契約変更）。
        assert_eq!(err.wire_code(), "23502");
    }

    #[test]
    fn bind_copy_record_accepts_null_for_nullable_column() {
        let schema = text_schema();
        let columns = vec!["id".to_string(), "body".to_string(), "note".to_string()];
        let fields = vec![Some("1".to_string()), Some("hello".to_string()), None];
        let bound = bind_copy_record("t", &columns, &fields, &None, &schema, test_clock()).unwrap();
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
        let err =
            bind_copy_record("t", &columns, &fields, &None, &schema, test_clock()).unwrap_err();
        assert_eq!(err.wire_code(), "22000");
    }
    // ---- Issue #1764: WITH オプションの適用 ----

    fn opts(format: &str, pairs: &[(&str, &str)]) -> (CopyFormat, CopyOptions) {
        use crate::sql::copy_options::{resolve, CopyDirection, RawCopyOption, RawCopyValue};
        let mut raw = vec![RawCopyOption {
            name: "format".to_string(),
            value: Some(RawCopyValue::Ident(format.to_string())),
        }];
        for (k, v) in pairs {
            raw.push(RawCopyOption {
                name: (*k).to_string(),
                value: Some(RawCopyValue::String((*v).to_string())),
            });
        }
        resolve(raw, CopyDirection::From).expect("resolve")
    }

    fn split_all(
        format: CopyFormat,
        options: &CopyOptions,
        input: &[u8],
        bytewise: bool,
    ) -> Result<Vec<Vec<Option<String>>>, SqlSurfaceError> {
        let mut splitter = RecordSplitter::with_options(format, options);
        let mut out = Vec::new();
        let mut on = |r| {
            out.push(r);
            Ok(())
        };
        if bytewise {
            for i in 0..input.len() {
                splitter.feed(&input[i..=i], &mut on)?;
            }
        } else {
            splitter.feed(input, &mut on)?;
        }
        if let Some(r) = splitter.finish()? {
            out.push(r);
        }
        Ok(out)
    }

    #[test]
    fn csv_custom_quote_escape_null_and_delimiter() {
        let (f, o) = opts(
            "csv",
            &[
                ("delimiter", ";"),
                ("quote", "'"),
                ("escape", "\\"),
                ("null", "NULL"),
            ],
        );
        let input = b"NULL;'NULL';;'a\\'b';'c\\\\d';'x\\y'\n";
        for bytewise in [false, true] {
            let got = split_all(f, &o, input, bytewise).unwrap();
            assert_eq!(
                got,
                vec![vec![
                    None,
                    Some("NULL".to_string()),
                    Some(String::new()),
                    Some("a'b".to_string()),
                    Some("c\\d".to_string()),
                    // ESCAPE の後が QUOTE／ESCAPE 以外なら ESCAPE 自体がデータ。
                    Some("x\\y".to_string()),
                ]]
            );
        }
    }

    #[test]
    fn csv_escape_state_rejects_unterminated_and_reopened_quote() {
        let (f, o) = opts("csv", &[("escape", "\\")]);
        assert!(split_all(f, &o, b"\"abc\\", false).is_err());
        assert!(split_all(f, &o, b"\"abc\n", false).is_err());
        // ESCAPE != QUOTE では閉じ引用符直後の引用符は不正配置。
        assert!(split_all(f, &o, b"\"a\"\"b\"\n", false).is_err());
    }

    #[test]
    fn csv_unquoted_escape_byte_is_literal() {
        let (f, o) = opts("csv", &[("escape", "\\")]);
        let got = split_all(f, &o, b"a\\b,c\n", false).unwrap();
        assert_eq!(
            got,
            vec![vec![Some("a\\b".to_string()), Some("c".to_string())]]
        );
    }

    #[test]
    fn text_custom_delimiter_null_and_escaped_delimiter() {
        let (f, o) = opts("text", &[("delimiter", "|"), ("null", "NA")]);
        for bytewise in [false, true] {
            let got = split_all(f, &o, b"NA|a\\|b|\\N\n", bytewise);
            // `\N` は NULL 文字列ではないため未対応エスケープとして拒否される。
            assert!(got.is_err());
            let got = split_all(f, &o, b"NA|a\\|b|\\\\\n", bytewise).unwrap();
            assert_eq!(
                got,
                vec![vec![None, Some("a|b".to_string()), Some("\\".to_string())]]
            );
        }
    }

    #[test]
    fn text_escaped_tab_is_literal_with_default_delimiter() {
        let got = split_all(
            CopyFormat::Text,
            &crate::sql::copy_options::defaults_for_format(CopyFormat::Text),
            b"a\\\tb\tc\n",
            false,
        )
        .unwrap();
        assert_eq!(
            got,
            vec![vec![Some("a\tb".to_string()), Some("c".to_string())]]
        );
    }

    #[test]
    fn check_header_record_true_discards_and_match_compares_names() {
        let cols = vec!["id".to_string(), "body".to_string()];
        let ok = vec![Some("id".to_string()), Some("body".to_string())];
        assert!(check_header_record(CopyHeader::True, &cols, &[None]).is_ok());
        assert!(check_header_record(CopyHeader::Match, &cols, &ok).is_ok());
        for bad in [
            vec![Some("id".to_string())],
            vec![Some("id".to_string()), Some("Body".to_string())],
            vec![Some("id".to_string()), None],
        ] {
            let e = check_header_record(CopyHeader::Match, &cols, &bad).unwrap_err();
            assert_eq!(e.wire_code(), "22000");
        }
    }
}
