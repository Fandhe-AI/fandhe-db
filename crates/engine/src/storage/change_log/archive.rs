//! 変更ログのアーカイブと保持管理（Issue #2167・ポインタ: RECOVER-14。設計は
//! `docs/design/change-log-pitr.md` の D6）。
//!
//! # 役割と責務境界
//!
//! - `change_log` 表のレコードを、LSN の連続区間ごとに外部ファイル（セグメント）へ書き出す。
//!   セグメントは自前ヘッダ・レコード列・全体チェックサムからなり、形式は設計書 D6 に記す
//!   （#2168・#2169 の増分・復元が同じ形式を読む）
//! - 書き出し済みの LSN を「アーカイブ済み水位」としてメタ表へ永続化し、保持設定があれば水位以下の
//!   レコードだけを刈り込む。水位更新と刈り込みは同じ保守 commit（世代を進めずレコードも生まない。
//!   `recovery::commit_boundary::commit_maintenance`）で行う
//! - アーカイブの欠落（LSN の飛び・チェックサム不一致・別 DB／別 timeline）を、ファイル単体
//!   （[`verify_segment_file`]）とディレクトリ全体（[`verify_archive_dir`]）の双方で検出する。
//!   セグメントは untrusted 入力として扱い、確保の前に長さを上限検証する
//!
//! 呼び出し元は `Storage::archive_change_log`（wire-server の常駐スレッドが `EngineCore` 経由で
//! 周期的に呼ぶ）と `Storage::verify_change_log_archive`・[`verify_archive_dir`]（運用者の検証
//! サブコマンド）。復元（ログの適用）は #2168 以降で、本ファイルは書き出しと検証だけを持つ。
//!
//! # fail-closed
//!
//! - アーカイブされていない行（水位より後）は刈り込まない。削除は水位以下に構造的に限る
//! - 公開は一時名への書き出し → fsync → `hard_link` → 一時名の削除 → ディレクトリ fsync。`rename` は
//!   既存ファイルを黙って置換するため使わず、既存のセグメントは上書きも削除もしない
//! - 決定的な名前のセグメントが既に在るときは、中身が表のレコードと完全一致する場合に限り
//!   書き直さずに採用し（再試行の収束）、一致しなければ [`ArchiveError::Conflict`] で止める
//! - エラーの `Display` は固定文言で、パス・LSN・件数を含めない（運用者向けの詳細は列挙子の値）
//!
//! 変更ログの本体表・メタ表は記録範囲の例外であり、素の redb で開く（`clippy.toml` の許可箇所）。

use std::io::Read;
use std::path::{Path, PathBuf};

use redb::{ReadableDatabase, ReadableTable};

use super::{
    peek_record, ChangeLogState, DecodeError, Reader, CHANGE_LOG_TABLE, CHECKSUM_LEN, DB_ID_LEN,
    HEADER_LEN, META_KEY_ARCHIVED_LSN, META_KEY_BASE_GENERATION, META_KEY_DB_ID, META_KEY_TIMELINE,
    META_TABLE,
};
use crate::crypto::sha256::Sha256;
use crate::storage::{Storage, StorageError};

/// セグメントの先頭マジック。
const SEG_MAGIC: [u8; 4] = *b"FCLA";
/// セグメント形式の版数。未知の版数は拒否する。
pub const SEGMENT_FORMAT_VERSION: u16 = 1;
/// マジック 4 + 版数 2 + DB 識別子 16 + timeline 8 + 先頭 LSN 8 + 末尾 LSN 8 + レコード数 8 +
/// ストレージ形式の版数 4 + 本体バイト数 8。
const SEG_HEADER_LEN: usize = 4 + 2 + DB_ID_LEN + 8 + 8 + 8 + 8 + 4 + 8;
/// ヘッダ + ヘッダのチェックサム。本体（レコード列）はこの直後から始まる。
const SEG_PREFIX_LEN: usize = SEG_HEADER_LEN + CHECKSUM_LEN;
const FILE_PREFIX: &str = "fandhe-cl-";
const FILE_SUFFIX: &str = ".seg";
const TMP_PREFIX: &str = ".fandhe-seg-";
/// セグメントのバイト数の既定上限（実装既定値。spec 由来ではない）。1 件のレコードがこれを超えるときは
/// そのレコード 1 件で 1 セグメントになる。
pub const DEFAULT_SEGMENT_BYTES: usize = 16 * 1024 * 1024;
/// 1 回の保守 commit で刈り込むレコード数の既定上限（ゲート占有時間を抑える。実装既定値）。
pub const DEFAULT_PRUNE_RECORDS: usize = 4096;
/// 1 回の保守 commit で刈り込むバイト数の既定上限（実装既定値）。
pub const DEFAULT_PRUNE_BYTES: usize = 64 * 1024 * 1024;
/// 1 回の [`Storage::archive_change_log`] 呼び出しで書き出すセグメント数の上限。書き込みが続く間も
/// 呼び出しが終わるようにする（残りは次の呼び出しが引き継ぐ）。
const MAX_SEGMENTS_PER_CYCLE: usize = 256;
/// 1 回の呼び出しで行う刈り込み専用の保守 commit 数の上限。
const MAX_PRUNE_ROUNDS: usize = 1024;
/// ディレクトリ走査で扱うセグメントファイル数の上限（untrusted なディレクトリ向けの資源上限）。
const MAX_SEGMENT_FILES: usize = 1_000_000;

// ---------------------------------------------------------------------------
// 公開型
// ---------------------------------------------------------------------------

/// アーカイブ・検証の失敗。`Display` は固定文言で、パス・LSN・件数を含めない（情報漏えい防止）。
#[derive(Debug)]
pub enum ArchiveError {
    /// 変更ログを有効にしていない `Storage`（または一時表用）。
    NotEnabled,
    /// 設定値が不正（セグメントや刈り込みの上限が 0）。
    InvalidOptions,
    /// アーカイブ先が既存のディレクトリでない。
    InvalidDirectory,
    /// ファイル操作の失敗。
    Io(std::io::ErrorKind),
    /// ファイルシステムが `hard_link` を提供しない（上書き禁止の原子的な公開ができない）。
    Unsupported,
    /// セグメントが途中で切れている（ファイル長・レコード数・本体長が宣言と合わない）。
    Truncated,
    /// マジック・版数・名前・構造が不正、または未知の版数。
    BadFormat,
    /// チェックサムの不一致（ヘッダ・レコード・セグメント全体のいずれか）。改ざんまたは破損。
    ChecksumMismatch,
    /// LSN の欠落・飛び・重複（セグメント内、セグメント間、アーカイブの先頭・末尾）。
    LsnGap,
    /// 別の DB 識別子のセグメント。
    ForeignDatabase,
    /// 別の timeline のセグメント。
    ForeignTimeline,
    /// `change_log` 表の側に欠落や破損がある（水位の次のレコードが無い、レコードのチェックサム不一致等）。
    /// アーカイブを進めず止める。
    LogCorrupt,
    /// 同名のセグメントが既に在り、表のレコードと一致しない（黙って置換しない）。
    Conflict,
    /// 水位が呼び出し中に別の経路で動いた（並行するアーカイブ）。次の呼び出しで再試行する。
    WatermarkMoved,
    /// ストレージ層のエラー。
    Storage(StorageError),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::NotEnabled => "change log is not enabled",
            Self::InvalidOptions => "archive options are invalid",
            Self::InvalidDirectory => "archive directory is not an existing directory",
            Self::Io(_) => "archive i/o error",
            Self::Unsupported => "archive directory does not support hard links",
            Self::Truncated => "archive segment is truncated",
            Self::BadFormat => "archive segment is malformed",
            Self::ChecksumMismatch => "archive segment checksum mismatch",
            Self::LsnGap => "archive has a gap or overlap in commit numbers",
            Self::ForeignDatabase => "archive segment belongs to another database",
            Self::ForeignTimeline => "archive segment belongs to another timeline",
            Self::LogCorrupt => "change log table is inconsistent with the archive position",
            Self::Conflict => "an archive segment with the same name differs from the log",
            Self::WatermarkMoved => "archive position changed concurrently",
            Self::Storage(_) => "storage error during archive",
        };
        f.write_str(text)
    }
}

impl std::error::Error for ArchiveError {}

impl From<StorageError> for ArchiveError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

impl From<redb::StorageError> for ArchiveError {
    fn from(e: redb::StorageError) -> Self {
        Self::Storage(e.into())
    }
}

impl From<redb::TableError> for ArchiveError {
    fn from(e: redb::TableError) -> Self {
        Self::Storage(e.into())
    }
}

impl From<redb::TransactionError> for ArchiveError {
    fn from(e: redb::TransactionError) -> Self {
        Self::Storage(e.into())
    }
}

impl From<std::io::Error> for ArchiveError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.kind())
    }
}

type Result<T> = std::result::Result<T, ArchiveError>;

/// [`Storage::archive_change_log`] の設定。
#[derive(Debug, Clone)]
pub struct ArchiveOptions {
    /// アーカイブ先ディレクトリ（既存のディレクトリ）。
    pub dir: PathBuf,
    /// セグメントの本体バイト数の上限（1 件のレコードがこれを超えるときは 1 件で 1 セグメント）。
    pub segment_bytes: usize,
    /// 保持設定。`Some(n)` なら、アーカイブ済みのレコードのうち新しい `n` 件を DB に残して、より古い
    /// ものを刈り込む。`None`（既定）は刈り込まない（アーカイブ済みでも消さない）。
    pub retain_records: Option<u64>,
    /// 1 回の保守 commit で刈り込むレコード数の上限。
    pub max_prune_records: usize,
    /// 1 回の保守 commit で刈り込むバイト数の上限。
    pub max_prune_bytes: usize,
}

impl ArchiveOptions {
    /// 既定値（刈り込まない・上限は実装既定値）で `dir` 向けの設定を作る。
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            segment_bytes: DEFAULT_SEGMENT_BYTES,
            retain_records: None,
            max_prune_records: DEFAULT_PRUNE_RECORDS,
            max_prune_bytes: DEFAULT_PRUNE_BYTES,
        }
    }
}

/// [`Storage::archive_change_log`] の 1 回分の結果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArchiveReport {
    /// 新しく書き出したセグメント数。
    pub segments_written: u64,
    /// 既に在った同一内容のセグメントを採用した数（再試行の収束）。
    pub segments_adopted: u64,
    /// 刈り込んだレコード数。
    pub pruned_records: u64,
    /// 呼び出し後のアーカイブ済み水位（書き出し済みの最後の LSN。まだ何も無ければ有効化時の世代）。
    pub archived_through: u64,
    /// 書き込みゲートの待機で保守 commit を見送った（水位は進んでいない。次の呼び出しが再試行する）。
    pub skipped: bool,
}

/// アーカイブの検証結果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArchiveSummary {
    /// 検証したセグメント数。
    pub segments: u64,
    /// レコードの合計数。
    pub records: u64,
    /// 先頭の LSN（セグメントが無ければ `None`）。
    pub first_lsn: Option<u64>,
    /// 末尾の LSN（セグメントが無ければ `None`）。
    pub last_lsn: Option<u64>,
}

/// [`verify_archive_dir`] が突き合わせる期待値。`None` の項目は検査しない（`db_id`・`timeline` は
/// 検査しない場合でも、セグメント同士が互いに一致することは常に検査する）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArchiveExpectation {
    pub db_id: Option<[u8; DB_ID_LEN]>,
    pub timeline: Option<u64>,
    /// アーカイブの先頭セグメントの先頭 LSN。
    pub start_lsn: Option<u64>,
}

// ---------------------------------------------------------------------------
// セグメント形式
// ---------------------------------------------------------------------------

/// セグメントのヘッダ。レコード列（本体）の前に置く。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SegmentHeader {
    db_id: [u8; DB_ID_LEN],
    timeline: u64,
    first_lsn: u64,
    last_lsn: u64,
    record_count: u64,
    storage_format: u32,
    body_len: u64,
}

impl SegmentHeader {
    fn encode(&self) -> [u8; SEG_PREFIX_LEN] {
        let mut head = Vec::with_capacity(SEG_PREFIX_LEN);
        head.extend_from_slice(&SEG_MAGIC);
        head.extend_from_slice(&SEGMENT_FORMAT_VERSION.to_le_bytes());
        head.extend_from_slice(&self.db_id);
        head.extend_from_slice(&self.timeline.to_le_bytes());
        head.extend_from_slice(&self.first_lsn.to_le_bytes());
        head.extend_from_slice(&self.last_lsn.to_le_bytes());
        head.extend_from_slice(&self.record_count.to_le_bytes());
        head.extend_from_slice(&self.storage_format.to_le_bytes());
        head.extend_from_slice(&self.body_len.to_le_bytes());
        let sum = crate::crypto::sha256::digest(&head);
        head.extend_from_slice(&sum);
        let mut out = [0u8; SEG_PREFIX_LEN];
        for (dst, src) in out.iter_mut().zip(head.iter()) {
            *dst = *src;
        }
        out
    }

    /// ヘッダ + チェックサムを検証して復号する。
    fn decode(prefix: &[u8; SEG_PREFIX_LEN]) -> Result<Self> {
        let (head, sum) = prefix.split_at(SEG_HEADER_LEN);
        if !crate::crypto::constant_time_eq(&crate::crypto::sha256::digest(head), sum) {
            return Err(ArchiveError::ChecksumMismatch);
        }
        let bad = |_| ArchiveError::BadFormat;
        let mut r = Reader { buf: head };
        if r.array::<4>().map_err(bad)? != SEG_MAGIC {
            return Err(ArchiveError::BadFormat);
        }
        if u16::from_le_bytes(r.array().map_err(bad)?) != SEGMENT_FORMAT_VERSION {
            return Err(ArchiveError::BadFormat);
        }
        let h = Self {
            db_id: r.array().map_err(bad)?,
            timeline: u64::from_le_bytes(r.array().map_err(bad)?),
            first_lsn: u64::from_le_bytes(r.array().map_err(bad)?),
            last_lsn: u64::from_le_bytes(r.array().map_err(bad)?),
            record_count: u64::from_le_bytes(r.array().map_err(bad)?),
            storage_format: u32::from_le_bytes(r.array().map_err(bad)?),
            body_len: u64::from_le_bytes(r.array().map_err(bad)?),
        };
        // 内部の整合: 件数と LSN の幅が一致し、空のセグメントは無い。
        let span = h
            .last_lsn
            .checked_sub(h.first_lsn)
            .and_then(|d| d.checked_add(1));
        if h.first_lsn == 0 || span != Some(h.record_count) || h.record_count == 0 {
            return Err(ArchiveError::BadFormat);
        }
        Ok(h)
    }
}

/// セグメントのファイル名（DB 識別子・timeline・LSN 範囲から決まる決定的な名前）。
fn segment_name(db_id: &[u8; DB_ID_LEN], timeline: u64, first: u64, last: u64) -> String {
    format!(
        "{FILE_PREFIX}{}-{timeline:016x}-{first:020}-{last:020}{FILE_SUFFIX}",
        hex(db_id)
    )
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex_id(s: &str) -> Option<[u8; DB_ID_LEN]> {
    if s.len() != DB_ID_LEN * 2 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; DB_ID_LEN];
    for (i, dst) in out.iter_mut().enumerate() {
        let pair = s.get(i * 2..i * 2 + 2)?;
        *dst = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

/// ファイル名から読み取れるセグメントの識別情報。
#[derive(Debug, Clone, PartialEq, Eq)]
struct SegmentName {
    db_id: [u8; DB_ID_LEN],
    timeline: u64,
    first_lsn: u64,
    last_lsn: u64,
}

/// 名前が本ファイルの接頭辞・接尾辞を持たなければ `Ok(None)`（無関係なファイルとして無視）。
/// 持つのに解釈できなければ `BadFormat`（取り違えた名前を黙って無視しない）。
fn parse_segment_name(name: &str) -> Result<Option<SegmentName>> {
    let Some(rest) = name
        .strip_prefix(FILE_PREFIX)
        .and_then(|r| r.strip_suffix(FILE_SUFFIX))
    else {
        return Ok(None);
    };
    let parts: Vec<&str> = rest.split('-').collect();
    let [id, tl, first, last] = parts.as_slice() else {
        return Err(ArchiveError::BadFormat);
    };
    let parsed = (|| {
        Some(SegmentName {
            db_id: unhex_id(id)?,
            timeline: u64::from_str_radix(tl, 16).ok()?,
            first_lsn: first.parse().ok()?,
            last_lsn: last.parse().ok()?,
        })
    })();
    match parsed {
        Some(n) if n.first_lsn != 0 && n.first_lsn <= n.last_lsn => Ok(Some(n)),
        _ => Err(ArchiveError::BadFormat),
    }
}

/// メモリ上でセグメントを組み立てる。レコードは `change_log` 表の値（`encode_record` の出力）を
/// そのまま連結し、検査（LSN の連続・DB 識別子・timeline・ストレージ形式の一致）を 1 件ずつ行う。
struct SegmentBuilder {
    db_id: [u8; DB_ID_LEN],
    timeline: u64,
    first_lsn: u64,
    next_lsn: u64,
    storage_format: Option<u32>,
    count: u64,
    max_payload: usize,
    /// 先頭の `SEG_PREFIX_LEN` バイトはヘッダの置き場（[`Self::finish`] で埋める）。
    buf: Vec<u8>,
}

impl SegmentBuilder {
    fn new(state: &ChangeLogState, first_lsn: u64) -> Self {
        Self {
            db_id: state.db_id,
            timeline: state.timeline,
            first_lsn,
            next_lsn: first_lsn,
            storage_format: None,
            count: 0,
            max_payload: state.limits.max_payload_bytes,
            buf: vec![0u8; SEG_PREFIX_LEN],
        }
    }

    /// 次のレコードがこのセグメントの版数と同じか（先頭のレコードや読めないレコードは `true` で、
    /// 後続の [`Self::push`] の検査に委ねる）。
    fn accepts_format(&self, value: &[u8]) -> bool {
        match (self.storage_format, peek_record(value, self.max_payload)) {
            (Some(f), Ok(h)) => f == h.storage_format,
            _ => true,
        }
    }

    fn body_len(&self) -> usize {
        self.buf.len().saturating_sub(SEG_PREFIX_LEN)
    }

    /// 表の 1 行（キー = LSN、値 = レコード）を検査して追加する。表側の欠落・破損は `LogCorrupt`。
    fn push(&mut self, key: u64, value: &[u8]) -> Result<()> {
        check_row(
            key,
            value,
            self.next_lsn,
            &self.db_id,
            self.timeline,
            &mut self.storage_format,
            self.max_payload,
        )?;
        self.buf.extend_from_slice(value);
        self.count = self.count.checked_add(1).ok_or(ArchiveError::LogCorrupt)?;
        self.next_lsn = key.checked_add(1).ok_or(ArchiveError::LogCorrupt)?;
        Ok(())
    }

    /// ヘッダ・ヘッダのチェックサム・トレーラ（全体チェックサム）を付けて完成させる。
    fn finish(mut self) -> Result<(SegmentHeader, Vec<u8>)> {
        let last_lsn = self
            .next_lsn
            .checked_sub(1)
            .ok_or(ArchiveError::LogCorrupt)?;
        let header = SegmentHeader {
            db_id: self.db_id,
            timeline: self.timeline,
            first_lsn: self.first_lsn,
            last_lsn,
            record_count: self.count,
            storage_format: self.storage_format.ok_or(ArchiveError::LogCorrupt)?,
            body_len: u64::try_from(self.body_len()).map_err(|_| ArchiveError::LogCorrupt)?,
        };
        for (dst, src) in self.buf.iter_mut().zip(header.encode().iter()) {
            *dst = *src;
        }
        let mut h = Sha256::new();
        h.update(&self.buf);
        let trailer = h.finalize();
        self.buf.extend_from_slice(&trailer);
        Ok((header, self.buf))
    }
}

/// 表の 1 行の検査（組み立てと採用の照合で共通）。
fn check_row(
    key: u64,
    value: &[u8],
    expect_lsn: u64,
    db_id: &[u8; DB_ID_LEN],
    timeline: u64,
    storage_format: &mut Option<u32>,
    max_payload: usize,
) -> Result<()> {
    if key != expect_lsn {
        return Err(ArchiveError::LogCorrupt);
    }
    let h = peek_record(value, max_payload).map_err(|_| ArchiveError::LogCorrupt)?;
    if h.lsn != key
        || key.checked_sub(1) != Some(h.prev_lsn)
        || &h.db_id != db_id
        || h.timeline != timeline
    {
        return Err(ArchiveError::LogCorrupt);
    }
    match storage_format {
        Some(f) if *f != h.storage_format => return Err(ArchiveError::LogCorrupt),
        Some(_) => {}
        None => *storage_format = Some(h.storage_format),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 検証（untrusted 入力）
// ---------------------------------------------------------------------------

/// `read_exact` の失敗を、ファイルの途中切れなら `Truncated`、他は `Io` に写す。
fn read_err(e: std::io::Error) -> ArchiveError {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        ArchiveError::Truncated
    } else {
        ArchiveError::from(e)
    }
}

fn map_decode(e: DecodeError) -> ArchiveError {
    match e {
        DecodeError::Truncated => ArchiveError::Truncated,
        DecodeError::ChecksumMismatch => ArchiveError::ChecksumMismatch,
        _ => ArchiveError::BadFormat,
    }
}

/// 検証済みセグメントの情報。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SegmentInfo {
    header: SegmentHeader,
    /// トレーラ（全体チェックサム）。採用時の照合に使う。
    trailer: [u8; CHECKSUM_LEN],
}

/// セグメントファイル 1 つを検証する。ファイル全体を一度に読まず、レコード単位に
/// 「ヘッダ → ペイロード長の上限検証 → 読み取り → チェックサム」の順で進める。検出するもの:
/// ヘッダ・レコード・全体のチェックサム不一致、セグメント内の LSN の飛び、別 DB／別 timeline の
/// レコードの混入、途中切れ・余り。通常ファイルでない（シンボリックリンク等）ものは拒否する。
fn verify_segment_file(path: &Path, max_payload: usize) -> Result<SegmentInfo> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(ArchiveError::BadFormat);
    }
    let mut file = std::fs::File::open(path)?;
    let mut prefix = [0u8; SEG_PREFIX_LEN];
    file.read_exact(&mut prefix).map_err(read_err)?;
    let header = SegmentHeader::decode(&prefix)?;
    let expected_len = u64::try_from(SEG_PREFIX_LEN)
        .ok()
        .and_then(|p| p.checked_add(header.body_len))
        .and_then(|n| n.checked_add(CHECKSUM_LEN as u64))
        .ok_or(ArchiveError::BadFormat)?;
    match meta.len().cmp(&expected_len) {
        std::cmp::Ordering::Less => return Err(ArchiveError::Truncated),
        std::cmp::Ordering::Greater => return Err(ArchiveError::BadFormat),
        std::cmp::Ordering::Equal => {}
    }
    let mut hasher = Sha256::new();
    hasher.update(&prefix);
    let mut remaining = header.body_len;
    let mut expect_lsn = header.first_lsn;
    let mut count = 0u64;
    let frame_head = HEADER_LEN + CHECKSUM_LEN;
    let mut format: Option<u32> = None;
    while remaining > 0 {
        let mut record = vec![0u8; frame_head];
        let head_len = frame_head as u64;
        if remaining < head_len {
            return Err(ArchiveError::Truncated);
        }
        file.read_exact(&mut record).map_err(read_err)?;
        // ペイロード長はヘッダの末尾 4 バイト（`encode_record_with` の配置）。確保の前に上限と残量で検証する。
        let len_bytes: [u8; 4] = record
            .get(HEADER_LEN - 4..HEADER_LEN)
            .and_then(|b| b.try_into().ok())
            .ok_or(ArchiveError::BadFormat)?;
        let payload_len =
            usize::try_from(u32::from_le_bytes(len_bytes)).map_err(|_| ArchiveError::BadFormat)?;
        if payload_len > max_payload {
            return Err(ArchiveError::BadFormat);
        }
        let payload_u64 = payload_len as u64;
        let after_head = remaining - head_len;
        if payload_u64 > after_head {
            return Err(ArchiveError::Truncated);
        }
        record.resize(frame_head + payload_len, 0);
        file.read_exact(
            record
                .get_mut(frame_head..)
                .ok_or(ArchiveError::BadFormat)?,
        )
        .map_err(read_err)?;
        remaining = after_head - payload_u64;
        let h = peek_record(&record, max_payload).map_err(map_decode)?;
        if h.db_id != header.db_id {
            return Err(ArchiveError::ForeignDatabase);
        }
        if h.timeline != header.timeline {
            return Err(ArchiveError::ForeignTimeline);
        }
        if h.lsn != expect_lsn || expect_lsn.checked_sub(1) != Some(h.prev_lsn) {
            return Err(ArchiveError::LsnGap);
        }
        match format {
            Some(f) if f != h.storage_format => return Err(ArchiveError::BadFormat),
            Some(_) => {}
            None => format = Some(h.storage_format),
        }
        hasher.update(&record);
        expect_lsn = expect_lsn.checked_add(1).ok_or(ArchiveError::LsnGap)?;
        count = count.checked_add(1).ok_or(ArchiveError::BadFormat)?;
    }
    if count != header.record_count {
        return Err(ArchiveError::LsnGap);
    }
    if expect_lsn.checked_sub(1) != Some(header.last_lsn) || format != Some(header.storage_format) {
        return Err(ArchiveError::BadFormat);
    }
    let mut trailer = [0u8; CHECKSUM_LEN];
    file.read_exact(&mut trailer).map_err(read_err)?;
    if !crate::crypto::constant_time_eq(&hasher.finalize(), &trailer) {
        return Err(ArchiveError::ChecksumMismatch);
    }
    Ok(SegmentInfo { header, trailer })
}

/// `dir` のセグメント名を列挙する（接頭辞・接尾辞が本形式のものだけ。それ以外は無視）。
fn list_segments(dir: &Path) -> Result<Vec<(PathBuf, SegmentName)>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if let Some(parsed) = parse_segment_name(name)? {
            if out.len() >= MAX_SEGMENT_FILES {
                return Err(ArchiveError::BadFormat);
            }
            out.push((entry.path(), parsed));
        }
    }
    out.sort_by_key(|(_, n)| (n.timeline, n.first_lsn, n.last_lsn));
    Ok(out)
}

/// アーカイブ先ディレクトリ全体を検証する。各セグメントを [`verify_segment_file`] で検証し、名前と
/// ヘッダの一致、DB 識別子・timeline の一貫、LSN の連鎖（前の末尾 + 1 == 次の先頭。飛びも重複も拒否）、
/// `expect` との一致を確かめる。欠落・順序違反・改ざん・別 DB／別 timeline は非 `Ok`。
/// 運用者の検証サブコマンドと [`Storage::verify_change_log_archive`] から呼ばれる。
pub fn verify_archive_dir(dir: &Path, expect: &ArchiveExpectation) -> Result<ArchiveSummary> {
    check_dir(dir)?;
    verify_listed(dir, expect, None)
}

fn verify_listed(
    dir: &Path,
    expect: &ArchiveExpectation,
    max_payload: Option<usize>,
) -> Result<ArchiveSummary> {
    let max_payload = max_payload.unwrap_or(super::CaptureLimits::default().max_payload_bytes);
    let mut summary = ArchiveSummary::default();
    let mut db_id = expect.db_id;
    let mut timeline = expect.timeline;
    let mut prev_last: Option<u64> = None;
    for (path, name) in list_segments(dir)? {
        let info = verify_segment_file(&path, max_payload)?;
        let h = info.header;
        if h.db_id != name.db_id || h.timeline != name.timeline {
            // 名前とヘッダが食い違う（取り違え・改名）。
            return Err(ArchiveError::BadFormat);
        }
        if h.first_lsn != name.first_lsn || h.last_lsn != name.last_lsn {
            return Err(ArchiveError::BadFormat);
        }
        if *db_id.get_or_insert(h.db_id) != h.db_id {
            return Err(ArchiveError::ForeignDatabase);
        }
        if *timeline.get_or_insert(h.timeline) != h.timeline {
            return Err(ArchiveError::ForeignTimeline);
        }
        let expected_first = match (prev_last, expect.start_lsn) {
            (Some(p), _) => Some(p.checked_add(1).ok_or(ArchiveError::LsnGap)?),
            (None, start) => start,
        };
        if expected_first.is_some_and(|e| e != h.first_lsn) {
            return Err(ArchiveError::LsnGap);
        }
        prev_last = Some(h.last_lsn);
        summary.segments = summary.segments.saturating_add(1);
        summary.records = summary.records.saturating_add(h.record_count);
        summary.first_lsn.get_or_insert(h.first_lsn);
        summary.last_lsn = Some(h.last_lsn);
    }
    Ok(summary)
}

fn check_dir(dir: &Path) -> Result<()> {
    match std::fs::metadata(dir) {
        Ok(m) if m.is_dir() => Ok(()),
        _ => Err(ArchiveError::InvalidDirectory),
    }
}

// ---------------------------------------------------------------------------
// 書き出し
// ---------------------------------------------------------------------------

/// 一時名から最終名へ、既存を上書きせずに公開する。公開は `hard_link`（宛先が在れば原子的に失敗する）。
/// 戻り値が `Ok(true)` なら新規に公開し、`Ok(false)` なら競合で既に在った（呼び出し側が内容を照合する）。
#[cfg(unix)]
fn publish(dir: &Path, final_path: &Path, bytes: &[u8]) -> Result<bool> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    let mut attempts = 0u32;
    let (mut file, tmp) = loop {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let p = dir.join(format!("{TMP_PREFIX}{}-{seq}.tmp", std::process::id()));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&p)
        {
            Ok(f) => break (f, p),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < 8 => {
                attempts += 1;
            }
            Err(e) => return Err(e.into()),
        }
    };
    let written = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.into());
    }
    let linked = std::fs::hard_link(&tmp, final_path);
    // 公開の成否にかかわらず一時名は不要（公開済みなら最終名が完全な中身を持つ）。
    let removed = std::fs::remove_file(&tmp);
    let created = match linked {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
        Err(e) => {
            return Err(match e.kind() {
                std::io::ErrorKind::Unsupported => ArchiveError::Unsupported,
                _ => e.into(),
            })
        }
    };
    removed?;
    std::fs::File::open(dir).and_then(|d| d.sync_all())?;
    Ok(created)
}

#[cfg(not(unix))]
fn publish(_dir: &Path, _final_path: &Path, _bytes: &[u8]) -> Result<bool> {
    Err(ArchiveError::Unsupported)
}

/// 競合で先に在った同名ファイルの中身が `bytes` と同一か。通常ファイルで長さが一致するときだけ
/// 読む（FIFO・デバイス・巨大ファイルを読まない。読み取りも `bytes` の長さ + 1 で打ち切る）。
fn existing_equals(path: &Path, bytes: &[u8]) -> Result<bool> {
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() || meta.len() != bytes.len() as u64 {
        return Ok(false);
    }
    let mut existing = Vec::with_capacity(bytes.len());
    std::fs::File::open(path)?
        .take(bytes.len() as u64 + 1)
        .read_to_end(&mut existing)?;
    Ok(existing == bytes)
}

/// 次に書き出す区間の計画。
enum Plan {
    /// 書くものが無い（水位より後のレコードが無い）。
    Nothing,
    /// 新規に書き出す。
    Write {
        header: SegmentHeader,
        bytes: Vec<u8>,
    },
    /// 同じ先頭 LSN の検証済みセグメントが既に在り、表のレコードと一致した（書き直さない）。
    Adopt { last_lsn: u64 },
}

fn read_meta_u64(read: &redb::ReadTransaction, key: &str) -> Result<Option<u64>> {
    match read.open_table(META_TABLE) {
        Ok(t) => match t.get(key)? {
            Some(v) => {
                let b: [u8; 8] = v.value().try_into().map_err(|_| ArchiveError::LogCorrupt)?;
                Ok(Some(u64::from_le_bytes(b)))
            }
            None => Ok(None),
        },
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// 現在のアーカイブ済み水位（無ければ初回有効化時の世代）。
fn watermark(read: &redb::ReadTransaction) -> Result<u64> {
    if let Some(w) = read_meta_u64(read, META_KEY_ARCHIVED_LSN)? {
        return Ok(w);
    }
    read_meta_u64(read, META_KEY_BASE_GENERATION)?.ok_or(ArchiveError::NotEnabled)
}

/// 水位の次から、バイト上限までレコードを集めて新規セグメントを組み立てる。既に同じ先頭 LSN の
/// セグメントがあれば、それを検証して表と照合し採用する（上限の再計算に頼らない。設定を変えた
/// 再起動でも同名衝突や重複区間を作らない）。
fn plan_next(
    storage: &Storage,
    state: &ChangeLogState,
    opts: &ArchiveOptions,
    w: u64,
) -> Result<Plan> {
    let first = w.checked_add(1).ok_or(ArchiveError::LogCorrupt)?;
    let read = storage.db.begin_read()?;
    let table = match read.open_table(CHANGE_LOG_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Plan::Nothing),
        Err(e) => return Err(e.into()),
    };
    // 水位の次の行が無ければ未アーカイブは無い。在るのに LSN が合わなければ表側の欠落。
    let head = table.range(first..)?.next().transpose()?;
    let Some((k, _)) = head else {
        return Ok(Plan::Nothing);
    };
    if k.value() != first {
        return Err(ArchiveError::LogCorrupt);
    }
    drop(k);

    let existing = list_segments(&opts.dir)?.into_iter().find(|(_, n)| {
        n.db_id == state.db_id && n.timeline == state.timeline && n.first_lsn == first
    });
    if let Some((path, name)) = existing {
        let info = verify_segment_file(&path, state.limits.max_payload_bytes)
            .map_err(|_| ArchiveError::Conflict)?;
        let h = info.header;
        if h.db_id != name.db_id
            || h.timeline != name.timeline
            || h.first_lsn != name.first_lsn
            || h.last_lsn != name.last_lsn
        {
            return Err(ArchiveError::Conflict);
        }
        // 表から同じ区間を流して、全体チェックサムが一致するか確かめる（メモリは 1 レコード分）。
        let mut hasher = Sha256::new();
        hasher.update(&h_prefix(&h));
        let mut next = first;
        let mut format: Option<u32> = None;
        let mut body = 0u64;
        for row in table.range(first..=h.last_lsn)? {
            let (k, v) = row?;
            let (key, value) = (k.value(), v.value());
            check_row(
                key,
                value,
                next,
                &state.db_id,
                state.timeline,
                &mut format,
                state.limits.max_payload_bytes,
            )?;
            hasher.update(value);
            body = body.saturating_add(value.len() as u64);
            next = key.checked_add(1).ok_or(ArchiveError::LogCorrupt)?;
        }
        let matches = next.checked_sub(1) == Some(h.last_lsn)
            && body == h.body_len
            && format == Some(h.storage_format)
            && crate::crypto::constant_time_eq(&hasher.finalize(), &info.trailer);
        return if matches {
            Ok(Plan::Adopt {
                last_lsn: h.last_lsn,
            })
        } else {
            Err(ArchiveError::Conflict)
        };
    }

    let mut builder = SegmentBuilder::new(state, first);
    for row in table.range(first..)? {
        let (k, v) = row?;
        // ストレージ形式の版数が変わる境界でセグメントを切る（1 セグメントは 1 つの版数。残りは次の周期）。
        if !builder.accepts_format(v.value()) {
            break;
        }
        builder.push(k.value(), v.value())?;
        if builder.body_len() >= opts.segment_bytes {
            break;
        }
    }
    let (header, bytes) = builder.finish()?;
    Ok(Plan::Write { header, bytes })
}

fn h_prefix(h: &SegmentHeader) -> [u8; SEG_PREFIX_LEN] {
    h.encode()
}

/// 保守 commit の結果。
enum Step {
    /// 書き込みゲートの待機を譲って見送った。
    Skipped,
    Done {
        pruned: u64,
    },
}

/// 水位を `new_w` へ進め（`expect_w` から変わっていなければ）、保持設定があれば水位以下を上限つきで
/// 刈り込む保守 commit。世代を進めずレコードも生まない（D6）。通常の書き込みが待機している間は
/// ゲートを譲り、見送ったら [`Step::Skipped`]。
fn maintenance_commit(
    storage: &Storage,
    expect_w: u64,
    new_w: u64,
    opts: &ArchiveOptions,
) -> Result<Step> {
    let Some(txn) = storage.begin_maintenance_txn()? else {
        return Ok(Step::Skipped);
    };
    let mut deleted: u64 = 0;
    {
        // 許可（clippy::disallowed_methods）: メタ表と本体表は記録範囲の例外（D3）で、捕捉の対象外。
        #[allow(clippy::disallowed_methods)]
        let mut meta = txn.open_table(META_TABLE)?;
        let current = match meta.get(META_KEY_ARCHIVED_LSN)? {
            Some(v) => {
                let b: [u8; 8] = v.value().try_into().map_err(|_| ArchiveError::LogCorrupt)?;
                Some(u64::from_le_bytes(b))
            }
            None => None,
        };
        let base = match meta.get(META_KEY_BASE_GENERATION)? {
            Some(v) => {
                let b: [u8; 8] = v.value().try_into().map_err(|_| ArchiveError::LogCorrupt)?;
                u64::from_le_bytes(b)
            }
            None => return Err(ArchiveError::NotEnabled),
        };
        if current.unwrap_or(base) != expect_w || new_w < expect_w {
            return Err(ArchiveError::WatermarkMoved);
        }
        if new_w != expect_w {
            meta.insert(META_KEY_ARCHIVED_LSN, new_w.to_le_bytes().as_slice())?;
        }
        if let Some(retain) = opts.retain_records {
            // 削除は水位以下に構造的に限る（水位より後は未アーカイブで、決して消さない）。
            let upto = new_w.saturating_sub(retain);
            #[allow(clippy::disallowed_methods)]
            let mut log = txn.open_table(CHANGE_LOG_TABLE)?;
            let mut bytes = 0usize;
            let max_records = u64::try_from(opts.max_prune_records).unwrap_or(u64::MAX);
            while deleted < max_records && bytes < opts.max_prune_bytes {
                let (key, len) = match log.first()? {
                    Some((k, v)) => (k.value(), v.value().len()),
                    None => break,
                };
                if key > upto {
                    break;
                }
                log.remove(key)?;
                deleted += 1;
                bytes = bytes.saturating_add(len);
            }
        }
    }
    if new_w == expect_w && deleted == 0 {
        txn.abort()?;
        return Ok(Step::Done { pruned: 0 });
    }
    crate::recovery::commit_boundary::commit_maintenance(txn)?;
    Ok(Step::Done { pruned: deleted })
}

/// [`Storage::archive_change_log`] の本体。未アーカイブのレコードをセグメントへ書き出して水位を進め
/// （上限つきで複数セグメント）、保持設定があれば水位以下を刈り込む。
pub(in crate::storage) fn run_cycle(
    storage: &Storage,
    opts: &ArchiveOptions,
) -> Result<ArchiveReport> {
    let state = storage.change_log.clone().ok_or(ArchiveError::NotEnabled)?;
    if opts.segment_bytes == 0 || opts.max_prune_records == 0 || opts.max_prune_bytes == 0 {
        return Err(ArchiveError::InvalidOptions);
    }
    check_dir(&opts.dir)?;
    let mut report = ArchiveReport::default();
    for _ in 0..MAX_SEGMENTS_PER_CYCLE {
        let w = watermark(&storage.db.begin_read()?)?;
        report.archived_through = w;
        let (last_lsn, adopted) = match plan_next(storage, &state, opts, w)? {
            Plan::Nothing => break,
            Plan::Adopt { last_lsn } => (last_lsn, true),
            Plan::Write { header, bytes } => {
                let name = segment_name(
                    &header.db_id,
                    header.timeline,
                    header.first_lsn,
                    header.last_lsn,
                );
                let path = opts.dir.join(name);
                if !publish(&opts.dir, &path, &bytes)? {
                    // 競合で同名が先に在った。中身が同一なら採用し、違えば置換せずに止める。
                    if !existing_equals(&path, &bytes)? {
                        return Err(ArchiveError::Conflict);
                    }
                    (header.last_lsn, true)
                } else {
                    (header.last_lsn, false)
                }
            }
        };
        match maintenance_commit(storage, w, last_lsn, opts)? {
            Step::Skipped => {
                report.skipped = true;
                return Ok(report);
            }
            Step::Done { pruned } => {
                report.pruned_records = report.pruned_records.saturating_add(pruned);
                report.archived_through = last_lsn;
                if adopted {
                    report.segments_adopted += 1;
                } else {
                    report.segments_written += 1;
                }
            }
        }
    }
    if opts.retain_records.is_some() {
        for _ in 0..MAX_PRUNE_ROUNDS {
            let w = watermark(&storage.db.begin_read()?)?;
            match maintenance_commit(storage, w, w, opts)? {
                Step::Skipped => {
                    report.skipped = true;
                    break;
                }
                Step::Done { pruned: 0 } => break,
                Step::Done { pruned } => {
                    report.pruned_records = report.pruned_records.saturating_add(pruned);
                }
            }
        }
    }
    Ok(report)
}

/// [`Storage::verify_change_log_archive`] の本体。DB のメタ表（DB 識別子・timeline・有効化時の世代・
/// 水位）を期待値として、アーカイブ先の連鎖を検証する。先頭は有効化時の世代の次、末尾は水位以上で
/// なければならない（末尾のセグメントの紛失を検出する）。
pub(in crate::storage) fn verify_against_db(
    storage: &Storage,
    dir: &Path,
) -> Result<ArchiveSummary> {
    check_dir(dir)?;
    let read = storage.db.begin_read()?;
    let id: Option<[u8; DB_ID_LEN]> = match read.open_table(META_TABLE) {
        Ok(t) => match t.get(META_KEY_DB_ID)? {
            Some(v) => Some(v.value().try_into().map_err(|_| ArchiveError::LogCorrupt)?),
            None => None,
        },
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(e.into()),
    };
    let id = id.ok_or(ArchiveError::NotEnabled)?;
    let timeline = read_meta_u64(&read, META_KEY_TIMELINE)?.ok_or(ArchiveError::LogCorrupt)?;
    let base = read_meta_u64(&read, META_KEY_BASE_GENERATION)?.ok_or(ArchiveError::LogCorrupt)?;
    let archived = read_meta_u64(&read, META_KEY_ARCHIVED_LSN)?.unwrap_or(base);
    drop(read);
    let expect = ArchiveExpectation {
        db_id: Some(id),
        timeline: Some(timeline),
        start_lsn: base.checked_add(1),
    };
    let summary = verify_listed(dir, &expect, None)?;
    // 水位が進んでいるのに、その LSN まで届くセグメントが無い（末尾の紛失）。
    if archived > base && summary.last_lsn.is_none_or(|l| l < archived) {
        return Err(ArchiveError::LsnGap);
    }
    Ok(summary)
}

#[cfg(test)]
mod tests;
