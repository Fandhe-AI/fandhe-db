//! 増分バックアップの出力・連鎖検証・適用（Issue #2168・ポインタ: RECOVER-13・RECOVER-14。設計は
//! `docs/design/change-log-pitr.md` の D7）。
//!
//! # 役割と責務境界
//!
//! - **出力**（[`Storage::export_incremental`](crate::storage::Storage::export_incremental)・
//!   [`ReadOnlyStorage::export_incremental`](crate::storage::ReadOnlyStorage::export_incremental)）:
//!   直前のベース（フルバックアップ、または前回の増分）が終わった commit 番号（LSN）の次から、DB の現在の
//!   最新までのログ区間を、アーカイブのセグメント形式（D6）の **1 ファイル**として書き出す。ベースの終点は
//!   セグメントの途中に落ちうるため、アーカイブの既存セグメントは流用せず、`change_log` 表から終点の次ちょうどで
//!   切り直す。1 ファイルにするのは、トレーラ（全体チェックサム）の存在がそのまま「欠けなく書き切れた」ことの
//!   証拠になるため（複数ファイルだと末尾側の欠落を受け側が検出できない）。書き出しは逐次で、区間の大きさに
//!   メモリが依存しない
//! - **連鎖検証**: ベースの [`ChainPoint`]（DB 識別子・timeline・終点 LSN）と、出力元の DB 識別子・timeline が
//!   一致し、増分の先頭 LSN がベースの終点の次であること。切れていれば専用のエラー（[`IncrementalError`]）で
//!   fail-closed に拒否する。適用側（[`apply_incremental_segments`]）も同じ検査を、ベースの DB と増分の
//!   セグメントの間、および増分同士の間で行う
//! - **適用**: ベースの DB ファイル（呼び出し側が用意した複製）へ、増分のセグメントを LSN 順に再生する。
//!   参照実装は `oracle::apply_record`・`oracle::replay`。本ファイルは redb の実表へ適用する本番版で、
//!   #2169 の PITR（指定 LSN・時刻での停止。`pitr`）が同じ再生器を共有できるよう、セグメント列の再生を
//!   1 関数（`replay_segments`）にまとめている（`pitr` が解決した停止 LSN を渡す）
//!
//! 呼び出し元は wire-server の `backup-incremental`・`restore --incremental`（`incremental_opt`・
//! `restore_opt`）。出力先・複製の公開手順（一時名 → fsync → `hard_link`）は呼び出し側と engine の
//! `archive::publish_with` が担う。復元後の世代番号の進行（RECOVER-17・#2446）はここでは行わない。
//!
//! # fail-closed
//!
//! - 出力は既存のファイルを上書きしない（`hard_link` の `EEXIST`）。他者が書けるディレクトリへは書かない
//!   （#2167 の `InsecureDirectory` と同じ検査）
//! - 適用は、全セグメントの検証（ヘッダ・レコード・全体のチェックサム、LSN の連続、同一 DB・timeline）を
//!   DB に触れる前に済ませ、再生中も各レコードの `prev_lsn == 直前に適用した LSN` とストレージ形式の版数を検査し、
//!   終了時に世代がセグメントの末尾 LSN に達していなければ失敗にする。チェックサムは改ざん検出ではなく
//!   破損検出なので、レコードが `change_log`／`change_log_meta`（記録範囲の例外）を指す場合は拒否する
//! - レコードのキー・値のバイト列は、shape の型に合うかを適用前に検証する（`table_image::apply_change`。redb の
//!   `from_bytes` は不正な入力で panic するため）。不正は `BadRecord`。適用側のメモリは 1 レコード（上限
//!   `max_payload_bytes`）の数倍になりうる（出力側は 1 レコード分）
//! - 適用先の `change_log` 表へ同じレコードを追記し、適用後の DB が最新の DB と同じ連続性
//!   （最後のレコードの LSN == 世代）を保つ。再有効化の連続性検査（D8）が拒否しない
//! - エラーの `Display` は固定の英語文言で、パス・表名・LSN・件数を含めない

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use redb::{ReadableDatabase, ReadableTable};

use super::archive::{
    check_dir_not_shared, check_row, map_decode, publish_with, read_meta_u64, verify_segment_file,
    ArchiveError, SegmentHeader, SegmentInfo, SegmentReader, SEG_MAGIC,
};
use super::{
    decode_record, is_excluded_table, peek_record, CaptureLimits, CHANGE_LOG_TABLE, DB_ID_LEN,
    META_KEY_DB_ID, META_KEY_TIMELINE, META_TABLE, STORAGE_FORMAT_VERSION,
};
use crate::crypto::sha256::Sha256;
use crate::storage::table_image;
use crate::storage::StorageError;

/// 1 回の適用で受け付けるセグメント数の上限（untrusted な引数列向けの資源上限。実装既定値）。
const MAX_APPLY_SEGMENTS: usize = 100_000;
/// 適用の途中 commit の単位（レコードのバイト数・件数の小さい方）。一時コピーへの適用で、公開前は
/// 途中の commit に durability を要しない（実装既定値）。
const BATCH_BYTES: usize = 8 * 1024 * 1024;
const BATCH_RECORDS: usize = 256;

/// 増分の連鎖上の「終点」。ベース（フルバックアップまたは前回の増分）が含む最後の commit を表す。
/// 次の増分は `end_lsn + 1` から始まり、同じ DB 識別子・timeline でなければならない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainPoint {
    /// ベースの DB 識別子（D5）。
    pub db_id: [u8; DB_ID_LEN],
    /// ベースの timeline（D5）。
    pub timeline: u64,
    /// ベースが含む最後の commit 番号（`storage_generation` の系列。D2）。
    pub end_lsn: u64,
}

/// 増分の出力・連鎖検証・適用の失敗。`Display` は固定の英語文言で、パス・表名・LSN・件数を含めない。
#[derive(Debug)]
#[non_exhaustive]
pub enum IncrementalError {
    /// ベースまたは出力元に変更ログのメタ情報（DB 識別子）が無い（変更ログを有効にしていない）。
    NotEnabled,
    /// 連鎖が切れている（増分の先頭がベースの終点の次でない、ベースが出力元より先に進んでいる、
    /// 増分同士が連続しない、適用後の世代が末尾に届かない）。
    ChainBroken,
    /// 別の DB 識別子（ベースと出力元、またはベースと増分、増分同士）。
    ForeignDatabase,
    /// 別の timeline。
    ForeignTimeline,
    /// ベースの終点以降に新しい commit が無い（出力すべき区間が空）。
    EmptyRange,
    /// 出力元の `change_log` 表がベースの終点の次から現在までを覆っていない（刈り込み済み・欠落）。
    RangeUnavailable,
    /// ベースを DB ファイルまたは増分バックアップとして読めない。
    BaseUnreadable,
    /// 出力元の DB を読み取り専用で開けない（書き込みを要する状態〔異常終了後の修復〕・DB ファイルではない・
    /// 破損）。呼び出し側（CLI）が [`ReadOnlyStorage::open`](crate::storage::ReadOnlyStorage::open) の失敗を写す。
    SourceUnreadable,
    /// 別のプロセスが DB を開いている。
    InUse,
    /// 出力先に既に何かがある。
    DestinationExists,
    /// 出力先のパスが不正（ファイル名なし・親がディレクトリでない）。
    InvalidDestination,
    /// 出力先のディレクトリが他者に書き込める。
    InsecureDirectory,
    /// レコードのストレージ形式の版数が、この実装の版数と異なる。
    UnsupportedFormat,
    /// レコードを適用できない（未知の shape・存在しない表への変更・既存の表の作成・記録範囲外の表・
    /// 型の不一致）。
    BadRecord,
    /// セグメント数が上限を超える、または 1 つも指定されていない。
    BadSegmentList,
    /// セグメントファイルの検証の失敗（途中切れ・形式不正・チェックサム不一致・LSN の飛び）。
    Segment(ArchiveError),
    /// ストレージ層のエラー。
    Storage(StorageError),
    /// ファイル操作の失敗。
    Io(std::io::ErrorKind),
    /// この環境では扱えない（unix 以外・`hard_link` 非対応）。
    Unsupported,
    /// PITR の目標（LSN・時刻）が不正（0・範囲外の値）。
    InvalidTarget,
    /// PITR の目標が、復元できる範囲の外にある（ベースの終点以前、または入力したログの末尾より後）。
    /// 黙って末尾まで適用しない。
    TargetOutOfRange,
}

impl std::fmt::Display for IncrementalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::NotEnabled => "change log is not enabled for the base or the source database",
            Self::ChainBroken => "incremental backup does not continue from the base",
            Self::ForeignDatabase => "incremental backup belongs to another database",
            Self::ForeignTimeline => "incremental backup belongs to another timeline",
            Self::EmptyRange => "there are no commits after the base",
            Self::RangeUnavailable => "the change log does not cover the range after the base",
            Self::BaseUnreadable => {
                "base could not be read as a database file or an incremental backup"
            }
            Self::SourceUnreadable => {
                "source database could not be opened read-only (it may need recovery after an unclean shutdown)"
            }
            Self::InUse => "database is open for writing by another process",
            Self::DestinationExists => "incremental backup destination already exists",
            Self::InvalidDestination => "invalid incremental backup destination",
            Self::InsecureDirectory => "incremental backup directory is writable by other users",
            Self::UnsupportedFormat => "record storage format version is not supported",
            Self::BadRecord => "incremental backup holds a record that cannot be applied",
            Self::BadSegmentList => "the list of incremental backups is empty or too long",
            Self::Segment(e) => return e.fmt(f),
            Self::Storage(_) => "storage error during incremental backup",
            Self::Io(_) => "i/o error during incremental backup",
            Self::Unsupported => "incremental backup is not supported in this environment",
            Self::InvalidTarget => "point-in-time recovery target is invalid",
            Self::TargetOutOfRange => {
                "point-in-time recovery target is outside the range covered by the change log"
            }
        };
        f.write_str(text)
    }
}

impl std::error::Error for IncrementalError {}

impl From<ArchiveError> for IncrementalError {
    fn from(e: ArchiveError) -> Self {
        match e {
            ArchiveError::ForeignDatabase => Self::ForeignDatabase,
            ArchiveError::ForeignTimeline => Self::ForeignTimeline,
            ArchiveError::InsecureDirectory => Self::InsecureDirectory,
            ArchiveError::Unsupported => Self::Unsupported,
            ArchiveError::NotEnabled => Self::NotEnabled,
            other => Self::Segment(other),
        }
    }
}

impl From<StorageError> for IncrementalError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

impl<E: Into<redb::Error>> From<E> for IncrementalError {
    fn from(e: E) -> Self {
        Self::Storage(StorageError::Backend(e.into()))
    }
}

type Result<T> = std::result::Result<T, IncrementalError>;

/// [`Storage::export_incremental`](crate::storage::Storage::export_incremental) の結果（件数のみ。
/// 行の値・表名は含めない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncrementalSummary {
    /// 出力したレコード数。
    pub records: u64,
    /// 先頭の commit 番号（ベースの終点の次）。
    pub first_lsn: u64,
    /// 末尾の commit 番号（出力時点の最新）。
    pub last_lsn: u64,
}

/// [`apply_incremental_segments`] の結果（件数のみ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApplySummary {
    /// 適用したセグメント数。
    pub segments: u64,
    /// 適用したレコード数。
    pub records: u64,
    /// 適用後の DB の最新の commit 番号。
    pub last_lsn: u64,
}

// ---------------------------------------------------------------------------
// ベースの終点
// ---------------------------------------------------------------------------

/// メタ表の DB 識別子と timeline。無ければ `NotEnabled`。
fn read_identity(read: &redb::ReadTransaction) -> Result<([u8; DB_ID_LEN], u64)> {
    let id: Option<[u8; DB_ID_LEN]> = match read.open_table(META_TABLE) {
        Ok(t) => match t.get(META_KEY_DB_ID)? {
            Some(v) => Some(v.value().try_into().map_err(|_| ArchiveError::LogCorrupt)?),
            None => None,
        },
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(e.into()),
    };
    let id = id.ok_or(IncrementalError::NotEnabled)?;
    let timeline = read_meta_u64(read, META_KEY_TIMELINE)?.ok_or(ArchiveError::LogCorrupt)?;
    Ok((id, timeline))
}

/// ベース `path`（フルバックアップの DB ファイル、または前回の増分バックアップ）の終点を読む。
/// 増分バックアップ（先頭のマジックで判別）は全体を検証してから末尾 LSN を終点にする。DB ファイルは
/// 読み取り専用で開き（書かない）、メタ表の DB 識別子・timeline と `storage_generation` を終点にする。
/// 変更ログを有効にしていない DB は `NotEnabled`。
pub fn chain_point_of(path: &Path) -> Result<ChainPoint> {
    let mut file = std::fs::File::open(path).map_err(|_| IncrementalError::BaseUnreadable)?;
    if !file
        .metadata()
        .map_err(|_| IncrementalError::BaseUnreadable)?
        .is_file()
    {
        return Err(IncrementalError::BaseUnreadable);
    }
    let mut magic = [0u8; 4];
    let is_incremental = file.read_exact(&mut magic).is_ok() && magic == SEG_MAGIC;
    drop(file);
    if is_incremental {
        let info = verify_segment_file(path, CaptureLimits::default().max_payload_bytes)?;
        return Ok(ChainPoint {
            db_id: info.header.db_id,
            timeline: info.header.timeline,
            end_lsn: info.header.last_lsn,
        });
    }
    let db = redb::ReadOnlyDatabase::open(path).map_err(|e| match e {
        redb::DatabaseError::DatabaseAlreadyOpen => IncrementalError::InUse,
        _ => IncrementalError::BaseUnreadable,
    })?;
    let read = db.begin_read()?;
    let (db_id, timeline) = read_identity(&read)?;
    let end_lsn = crate::storage::current_generation_in_txn(&read)?;
    Ok(ChainPoint {
        db_id,
        timeline,
        end_lsn,
    })
}

// ---------------------------------------------------------------------------
// 出力
// ---------------------------------------------------------------------------

/// [`Storage::export_incremental`](crate::storage::Storage::export_incremental) の本体。`read` は出力元の
/// 読み取りトランザクション（開始時点の単一スナップショット。書き込みを止めない）。
pub(in crate::storage) fn export_from_read(
    read: &redb::ReadTransaction,
    base: &ChainPoint,
    dest: &Path,
) -> Result<IncrementalSummary> {
    dest.file_name()
        .ok_or(IncrementalError::InvalidDestination)?;
    let parent: &Path = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    match std::fs::metadata(parent) {
        Ok(m) if m.is_dir() => {}
        _ => return Err(IncrementalError::InvalidDestination),
    }
    check_dir_not_shared(parent)?;
    // 早期失敗のための確認。最終判定は公開の `hard_link`（確認後の競合を残さない）。
    if std::fs::symlink_metadata(dest).is_ok() {
        return Err(IncrementalError::DestinationExists);
    }

    let (db_id, timeline) = read_identity(read)?;
    if db_id != base.db_id {
        return Err(IncrementalError::ForeignDatabase);
    }
    if timeline != base.timeline {
        return Err(IncrementalError::ForeignTimeline);
    }
    let latest = crate::storage::current_generation_in_txn(read)?;
    if base.end_lsn > latest {
        // ベースが出力元より先に進んでいる（別系統の複製・巻き戻された DB）。
        return Err(IncrementalError::ChainBroken);
    }
    if base.end_lsn == latest {
        return Err(IncrementalError::EmptyRange);
    }
    let first = base
        .end_lsn
        .checked_add(1)
        .ok_or(IncrementalError::ChainBroken)?;
    let count = latest - base.end_lsn;

    let log = match read.open_table(CHANGE_LOG_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Err(IncrementalError::RangeUnavailable)
        }
        Err(e) => return Err(e.into()),
    };
    let max_payload = CaptureLimits::default().max_payload_bytes;
    // 1 周目: 区間が欠けなく連続していることと、本体のバイト数・ストレージ形式を確かめる。
    let mut body_len = 0u64;
    let mut format: Option<u32> = None;
    let mut expect = first;
    for entry in log.range(first..=latest)? {
        let (k, v) = entry?;
        let (key, value) = (k.value(), v.value());
        if key != expect {
            return Err(IncrementalError::RangeUnavailable);
        }
        check_row(
            key,
            value,
            expect,
            &db_id,
            timeline,
            &mut format,
            max_payload,
        )?;
        body_len = body_len
            .checked_add(value.len() as u64)
            .ok_or(ArchiveError::LogCorrupt)?;
        expect = expect.checked_add(1).ok_or(ArchiveError::LogCorrupt)?;
    }
    if expect != latest.checked_add(1).ok_or(ArchiveError::LogCorrupt)? {
        return Err(IncrementalError::RangeUnavailable);
    }
    let storage_format = format.ok_or(IncrementalError::RangeUnavailable)?;
    if storage_format != STORAGE_FORMAT_VERSION {
        return Err(IncrementalError::UnsupportedFormat);
    }
    let header = SegmentHeader {
        db_id,
        timeline,
        first_lsn: first,
        last_lsn: latest,
        record_count: count,
        storage_format,
        body_len,
    };

    // 2 周目: 同じスナップショットを逐次に書く（メモリは 1 レコード分）。
    let published = publish_with(parent, dest, |file| {
        let other = |_| std::io::Error::other("change log changed while exporting");
        let mut out = std::io::BufWriter::new(file);
        let prefix = header.encode();
        let mut hasher = Sha256::new();
        hasher.update(&prefix);
        out.write_all(&prefix)?;
        let mut written = 0u64;
        for entry in log.range(first..=latest).map_err(other)? {
            let (_, v) = entry.map_err(other)?;
            let value = v.value();
            hasher.update(value);
            out.write_all(value)?;
            written += value.len() as u64;
        }
        if written != body_len {
            return Err(std::io::Error::other("change log changed while exporting"));
        }
        out.write_all(&hasher.finalize())?;
        out.flush()
    })?;
    if !published {
        return Err(IncrementalError::DestinationExists);
    }
    Ok(IncrementalSummary {
        records: count,
        first_lsn: first,
        last_lsn: latest,
    })
}

// ---------------------------------------------------------------------------
// 適用
// ---------------------------------------------------------------------------

/// 適用中の writer（途中 commit の単位で開き直す）。
struct Batch {
    txn: redb::WriteTransaction,
    bytes: usize,
    records: usize,
}

/// `db_path`（ベースの複製。呼び出し側が一時名へ作ったもの）へ、`segments`（増分バックアップのファイル。
/// 古い順）を LSN 順に再生する。`segments` の順序は呼び出し側が指定し、ここでは並べ替えない
/// （順序違反は `ChainBroken`）。成功時、`db_path` の DB は最後のセグメントの末尾 LSN の状態になり、
/// `change_log` 表にも同じレコードが追記される。失敗時、`db_path` は途中まで適用された状態になりうる
/// ので、呼び出し側は破棄する（元のバックアップには触れない）。DB は素の redb で開く（`Storage::open` は
/// 変更ログ有効化済みの DB の書き込みを拒否し、移行・修復の副作用もあるため使わない）。
///
/// 指定 LSN・時刻での停止は [`restore_to_point`](super::pitr::restore_to_point)（Issue #2169）が同じ
/// 再生器（[`replay_segments`]）に停止条件を渡して行う。復元後の世代番号の進行（#2446）は呼び出し側が、
/// 本関数の成功後・公開の前に行う。
pub fn apply_incremental_segments(db_path: &Path, segments: &[PathBuf]) -> Result<ApplySummary> {
    let replay = verify_incrementals(segments)?;
    replay_segments(db_path, &replay, None)
}

/// 再生するセグメント 1 つ（検証済み）。
#[derive(Debug, Clone)]
pub(super) struct ReplaySegment {
    pub(super) path: PathBuf,
    /// 検証時の結果。再生中に差し替えられていないことの照合に使う。
    pub(super) info: SegmentInfo,
    /// 真なら、既に適用した LSN 以下のレコードを読み飛ばす（アーカイブのセグメントは、ベースや
    /// 増分の終点をまたぎうる）。偽（増分バックアップ）なら先頭がちょうど次の LSN でなければならない。
    pub(super) overlap_ok: bool,
}

/// 増分バックアップのファイル列を検証し、再生用の列にする（DB に触れない）。各ファイルの検証、
/// 同一 DB・timeline、前の末尾 + 1 == 次の先頭、ストレージ形式の版数を確かめる。
pub(super) fn verify_incrementals(segments: &[PathBuf]) -> Result<Vec<ReplaySegment>> {
    if segments.is_empty() || segments.len() > MAX_APPLY_SEGMENTS {
        return Err(IncrementalError::BadSegmentList);
    }
    let max_payload = CaptureLimits::default().max_payload_bytes;
    let mut out: Vec<ReplaySegment> = Vec::with_capacity(segments.len());
    for path in segments {
        let info = verify_segment_file(path, max_payload)?;
        if let Some(prev) = out.last().map(|r| r.info.header) {
            if info.header.db_id != prev.db_id {
                return Err(IncrementalError::ForeignDatabase);
            }
            if info.header.timeline != prev.timeline {
                return Err(IncrementalError::ForeignTimeline);
            }
            if prev.last_lsn.checked_add(1) != Some(info.header.first_lsn) {
                return Err(IncrementalError::ChainBroken);
            }
        }
        if info.header.storage_format != STORAGE_FORMAT_VERSION {
            return Err(IncrementalError::UnsupportedFormat);
        }
        out.push(ReplaySegment {
            path: path.clone(),
            info,
            overlap_ok: false,
        });
    }
    Ok(out)
}

/// 検証済みのセグメント列 `segments` を `db_path` へ再生する（増分の適用と PITR が共有する再生器）。
/// `stop_lsn` があれば、その LSN のレコード（commit 1 つ）を適用した直後で止める（コミット境界。
/// 1 レコード = 1 commit）。止まった後も、同じセグメントの残りを読み切って全体チェックサムを照合する
/// （適用はしない）。`stop_lsn` がベースの世代以下、または列の末尾より後なら `TargetOutOfRange`。
/// `None` なら列の末尾まで適用する。
pub(super) fn replay_segments(
    db_path: &Path,
    segments: &[ReplaySegment],
    stop_lsn: Option<u64>,
) -> Result<ApplySummary> {
    let max_payload = CaptureLimits::default().max_payload_bytes;
    let first = segments
        .first()
        .ok_or(IncrementalError::BadSegmentList)?
        .clone();
    // 範囲の上限は全セグメントの最大。列は LSN の昇順とは限らない（PITR では増分の後ろに、適用済みの
    // 区間に収まるアーカイブのセグメントが続く）ので、最後の要素から取らない。
    let list_last = segments
        .iter()
        .map(|r| r.info.header.last_lsn)
        .max()
        .ok_or(IncrementalError::BadSegmentList)?;

    // 1. ベースを開いて連鎖を確かめる（DB 識別子・timeline・終点の次が先頭）。
    let db = redb::Database::open(db_path).map_err(|e| match e {
        redb::DatabaseError::DatabaseAlreadyOpen => IncrementalError::InUse,
        _ => IncrementalError::BaseUnreadable,
    })?;
    let (mut names, base_generation) = {
        let read = db.begin_read()?;
        let (db_id, timeline) = read_identity(&read)?;
        // 列のすべてのセグメントが、ベースと同じ DB・timeline であること（先頭だけでなく全体）。
        for seg in segments {
            if seg.info.header.db_id != db_id {
                return Err(IncrementalError::ForeignDatabase);
            }
            if seg.info.header.timeline != timeline {
                return Err(IncrementalError::ForeignTimeline);
            }
        }
        let generation = crate::storage::current_generation_in_txn(&read)?;
        let next = generation
            .checked_add(1)
            .ok_or(IncrementalError::ChainBroken)?;
        let fh = first.info.header;
        let connects = if first.overlap_ok {
            // 先頭が完全に適用済みの区間に収まってもよい（読み切って照合する）。続きの欠落は、
            // 以降のレコードの `prev_lsn` の連鎖と最終の世代の検査が拒否する。
            fh.first_lsn <= next
        } else {
            next == fh.first_lsn
        };
        if !connects {
            return Err(IncrementalError::ChainBroken);
        }
        let names: BTreeSet<String> = read
            .list_tables()?
            .map(|h| redb::TableHandle::name(&h).to_string())
            .filter(|n| !is_excluded_table(n))
            .collect();
        (names, generation)
    };
    if let Some(stop) = stop_lsn {
        if stop <= base_generation || stop > list_last {
            return Err(IncrementalError::TargetOutOfRange);
        }
    }

    // 2. 再生。レコードごとに連鎖・版数を検査し、途中 commit は durability を省く（公開前の一時コピー）。
    let mut current = base_generation;
    let mut batch: Option<Batch> = None;
    let mut records = 0u64;
    let mut stopped = false;
    let mut opened = 0u64;
    for seg in segments {
        let info = &seg.info;
        if stopped && (!seg.overlap_ok || info.header.first_lsn > current) {
            // 停止後は、適用済みの区間に重なるアーカイブのセグメントだけを読む（増分の途中で止まっても、
            // 停止位置以前のアーカイブの内容を照合するため）。停止位置より後ろの入力は使わない。
            continue;
        }
        let mut reader = SegmentReader::open(&seg.path, max_payload)?;
        opened = opened.saturating_add(1);
        // 検証から再生までの間に差し替えられていないこと（ヘッダが検証時と同一）。
        if *reader.header() != info.header {
            return Err(ArchiveError::ChecksumMismatch.into());
        }
        while let Some(raw) = reader.next_record()? {
            if stopped {
                // 停止点より後ろは適用しない。アーカイブのセグメントの停止位置以前のレコードは、適用済みの
                // 内容と照合する。残りは全体チェックサムのために読み切るだけ。
                if seg.overlap_ok {
                    let h = peek_record(&raw, max_payload)
                        .map_err(map_decode)
                        .map_err(IncrementalError::from)?;
                    if h.lsn <= current {
                        check_overlap(&db, batch.as_ref(), &raw, &h, &info.header)?;
                    }
                }
                continue;
            }
            if seg.overlap_ok {
                let h = peek_record(&raw, max_payload)
                    .map_err(map_decode)
                    .map_err(IncrementalError::from)?;
                if h.lsn <= current {
                    // 適用済みの LSN と重なる区間。内容が食い違う（分岐した履歴の混在）なら拒否する。
                    check_overlap(&db, batch.as_ref(), &raw, &h, &info.header)?;
                    continue;
                }
            }
            let rec = decode_record(&raw, max_payload)
                .map_err(map_decode)
                .map_err(IncrementalError::from)?;
            let h = rec.header;
            if h.db_id != info.header.db_id {
                return Err(IncrementalError::ForeignDatabase);
            }
            if h.timeline != info.header.timeline {
                return Err(IncrementalError::ForeignTimeline);
            }
            if h.storage_format != STORAGE_FORMAT_VERSION {
                return Err(IncrementalError::UnsupportedFormat);
            }
            if h.prev_lsn != current || current.checked_add(1) != Some(h.lsn) {
                return Err(IncrementalError::ChainBroken);
            }
            if batch.is_none() {
                let mut txn = db.begin_write()?;
                txn.set_durability(redb::Durability::None)?;
                batch = Some(Batch {
                    txn,
                    bytes: 0,
                    records: 0,
                });
            }
            let b = batch.as_mut().ok_or(IncrementalError::BadRecord)?;
            apply_body(&b.txn, &mut names, rec.body)?;
            append_log_row(&b.txn, h.lsn, &raw)?;
            b.bytes = b.bytes.saturating_add(raw.len());
            b.records = b.records.saturating_add(1);
            let full = b.bytes >= BATCH_BYTES || b.records >= BATCH_RECORDS;
            current = h.lsn;
            records = records.checked_add(1).ok_or(IncrementalError::BadRecord)?;
            if full {
                if let Some(b) = batch.take() {
                    b.txn.commit()?;
                }
            }
            if stop_lsn == Some(h.lsn) {
                stopped = true;
            }
        }
        // 検証時と同じ全体チェックサム（再生中に中身が変わっていない）。
        if reader.finish()? != info.trailer {
            return Err(ArchiveError::ChecksumMismatch.into());
        }
    }
    if let Some(b) = batch.take() {
        b.txn.commit()?;
    }
    let expected_last = match stop_lsn {
        Some(stop) if stopped => stop,
        Some(_) => return Err(IncrementalError::TargetOutOfRange),
        None => list_last,
    };
    // 空の commit を Immediate で 1 回行い、それまでの内容を fsync で確定させる。
    db.begin_write()?.commit()?;

    // 3. 世代が停止点（または末尾）に届いていること（レコードが世代の表を含まない・欠けた場合の fail-closed）。
    let read = db.begin_read()?;
    if crate::storage::current_generation_in_txn(&read)? != expected_last
        || current != expected_last
    {
        return Err(IncrementalError::ChainBroken);
    }
    Ok(ApplySummary {
        segments: opened,
        records,
        last_lsn: expected_last,
    })
}

/// 適用済みの LSN と重なるアーカイブのレコード `raw`（ヘッダ `h`）が、適用先に既にある内容と一致することを
/// 確かめる。適用先の `change_log` 表（書き込み中のバッチ、無ければコミット済みの状態）の同じ LSN の行と
/// バイト列で照合し、不一致は `ChainBroken`。行が無い（刈り込み済みなど）場合は照合の基準がないので、
/// ヘッダの DB 識別子・timeline・ストレージ形式と `prev_lsn == lsn - 1` だけを確かめる。
fn check_overlap(
    db: &redb::Database,
    batch: Option<&Batch>,
    raw: &[u8],
    h: &super::RecordHeader,
    seg: &SegmentHeader,
) -> Result<()> {
    let logged: Option<Vec<u8>> = match batch {
        Some(b) => {
            // 許可（clippy::disallowed_methods）: 本体表は記録範囲の例外（D3）で、読み取りだけ。
            #[allow(clippy::disallowed_methods)]
            let t = b.txn.open_table(CHANGE_LOG_TABLE)?;
            let row = t.get(h.lsn)?.map(|v| v.value().to_vec());
            row
        }
        None => {
            let read = db.begin_read()?;
            match read.open_table(CHANGE_LOG_TABLE) {
                Ok(t) => t.get(h.lsn)?.map(|v| v.value().to_vec()),
                Err(redb::TableError::TableDoesNotExist(_)) => None,
                Err(e) => return Err(e.into()),
            }
        }
    };
    match logged {
        Some(row) if row == raw => Ok(()),
        Some(_) => Err(IncrementalError::ChainBroken),
        None => {
            if h.db_id != seg.db_id
                || h.timeline != seg.timeline
                || h.storage_format != STORAGE_FORMAT_VERSION
                || h.lsn.checked_sub(1) != Some(h.prev_lsn)
            {
                return Err(IncrementalError::ChainBroken);
            }
            Ok(())
        }
    }
}

/// レコード本体を writer へ適用する（`dropped` の削除 → `created` の作成 → `tables` の反映。参照実装は
/// `oracle::apply_record`）。表の有無の整合（無い表の削除・既存の表の作成・無い表への変更）と、記録範囲外の
/// 表（`change_log`・`change_log_meta`）への言及は `BadRecord` で拒否する。
fn apply_body(
    txn: &redb::WriteTransaction,
    names: &mut BTreeSet<String>,
    body: super::RecordBody,
) -> Result<()> {
    let bad = |_| IncrementalError::BadRecord;
    let shapes = table_image::shapes_len();
    for n in &body.dropped {
        if !table_name_ok(n) || !names.remove(n) {
            return Err(IncrementalError::BadRecord);
        }
        if !table_image::drop_table_named(txn, n).map_err(bad)? {
            return Err(IncrementalError::BadRecord);
        }
    }
    for (n, shape) in &body.created {
        let shape = usize::try_from(*shape).map_err(bad_conv)?;
        if !table_name_ok(n) || shape >= shapes || !names.insert(n.clone()) {
            return Err(IncrementalError::BadRecord);
        }
        table_image::apply_change(txn, n, shape, Vec::new(), Vec::new()).map_err(bad)?;
    }
    for t in body.tables {
        let shape = usize::try_from(t.shape).map_err(bad_conv)?;
        if !table_name_ok(&t.name) || shape >= shapes || !names.contains(&t.name) {
            return Err(IncrementalError::BadRecord);
        }
        table_image::apply_change(txn, &t.name, shape, t.upserts, t.removes).map_err(bad)?;
    }
    Ok(())
}

/// 適用してよい表名か。空の名前は redb の `TableDefinition::new` が panic するため拒否し(`dropped`・
/// `created`・`tables` のすべての経路で共通)、記録範囲外の表(`change_log`・`change_log_meta`)も拒否する。
fn table_name_ok(name: &str) -> bool {
    !name.is_empty() && !is_excluded_table(name)
}

fn bad_conv(_: std::num::TryFromIntError) -> IncrementalError {
    IncrementalError::BadRecord
}

/// 適用したレコードを、適用先の `change_log` 表へ（キー = LSN）そのまま追記する。最新の DB と同じ
/// 連続性（最後のレコードの LSN == 世代）を保つため。同じ LSN の行が既にあれば `ChainBroken`。
fn append_log_row(txn: &redb::WriteTransaction, lsn: u64, raw: &[u8]) -> Result<()> {
    // 許可（clippy::disallowed_methods）: 本体表は記録範囲の例外（D3）で、捕捉の対象外。
    #[allow(clippy::disallowed_methods)]
    let mut log = txn.open_table(CHANGE_LOG_TABLE)?;
    if log.insert(lsn, raw)?.is_some() {
        return Err(IncrementalError::ChainBroken);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
