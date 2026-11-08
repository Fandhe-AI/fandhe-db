//! アーカイブ済み変更ログのセグメントファイルの保持と削除（Issue #3092・ポインタ: RECOVER-14 (7)。
//! 設計は `docs/design/change-log-pitr.md` の D6 の「Issue #3092 の実装判断」）。
//!
//! # 役割と責務境界
//!
//! `archive`（#2167）はセグメントを書き出して DB 側の `change_log` 表を刈り込むが、アーカイブ先の
//! ファイルは決して消さない（fail-closed）。本モジュールは、保持しているフルバックアップから PITR
//! するのに要らないセグメントだけを、運用者の明示操作で安全に消す。
//!
//! - [`retention_point_of`] は、フルバックアップがどの commit 番号までを含むか（削除の上限）を読む
//! - [`plan_archive_cleanup`] は、アーカイブ全体を検証し、削除してよい先頭からの連続区間を計画する（何も消さない）
//! - [`execute_archive_cleanup`] は、計画を昇順に 1 件ずつ消す。1 件ごとに再照合・ディレクトリ fsync する
//!
//! 呼び出し元は wire-server の `change-log-cleanup` サブコマンドだけ（テナントの SQL・wire からは
//! 到達できない）。engine の他の経路（周期アーカイブ・PITR）は本モジュールを呼ばない。
//!
//! # fail-closed
//!
//! - 削除の上限は「保持するフルバックアップの終点」の最小値。保持期間・世代数の指定は上限を**下げる**
//!   方向にしか働かない。バックアップが 0 件なら何も消さない
//! - 増分バックアップは基準に使えない（より古いフルバックアップからの PITR に要るセグメントまで消えるため）
//! - DB 識別子・timeline がバックアップ同士・アーカイブと食い違えば何も消さない
//! - アーカイブ全体の検証（連鎖・チェックサム）に失敗していれば何も消さない
//! - 先頭からの連続区間だけを昇順に消すため、途中で失敗しても LSN の飛びは生じない
//! - `Display` は固定の英語文言で、パス・LSN・件数を含めない

use std::io::Read;
use std::path::{Path, PathBuf};

use redb::{ReadableDatabase, ReadableTable};

use super::{
    check_dir, check_dir_not_shared, peek_record, verified_segments, ArchiveError,
    ArchiveExpectation, SegmentHeader, SegmentReader, CHANGE_LOG_TABLE, SEG_MAGIC, SEG_PREFIX_LEN,
};
use crate::storage::change_log::incremental::{read_identity, IncrementalError};
use crate::storage::change_log::DB_ID_LEN;

/// 保持しているフルバックアップ 1 つの位置（[`retention_point_of`] の結果）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPoint {
    /// DB 識別子（D5）。
    pub db_id: [u8; DB_ID_LEN],
    /// timeline（D5）。
    pub timeline: u64,
    /// バックアップが含む最後の commit 番号。このバックアップから PITR するのに、これ以前のセグメントは要らない。
    pub end_lsn: u64,
    /// バックアップ内の `change_log` 表の先頭行の LSN（空なら `None`）。DB 側の刈り込みが詰まらない
    /// ための下限に使う（取得後の DB の先頭行はこれ以上になる）。
    pub first_log_lsn: Option<u64>,
}

/// 削除の方針。`backups` が空なら計画は [`CleanupError::NoBackups`] で拒否する。
#[derive(Debug, Clone)]
pub struct CleanupPolicy {
    /// 保持するフルバックアップ（渡したものはすべて保持中とみなす）。
    pub backups: Vec<RetentionPoint>,
    /// 停止中の DB の位置（任意。先頭行より前までしか消さない）。
    pub db: Option<RetentionPoint>,
    /// 最後のレコードの commit 時刻がこの秒数より新しいセグメントは消さない（任意）。
    pub keep_secs: Option<u64>,
    /// 削除候補の末尾からこの数のセグメントを残す（任意）。
    pub keep_segments: Option<u64>,
    /// 現在時刻（壁時計 UTC マイクロ秒。`keep_secs` の判定用）。
    pub now_us: i64,
}

/// 削除予定のセグメント 1 つ。
#[derive(Debug, Clone)]
struct Victim {
    path: PathBuf,
    first_lsn: u64,
    last_lsn: u64,
    len: u64,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
}

/// [`plan_archive_cleanup`] の結果。何も消していない。
#[derive(Debug, Clone)]
pub struct CleanupPlan {
    db_id: [u8; DB_ID_LEN],
    timeline: u64,
    cutoff: u64,
    victims: Vec<Victim>,
}

impl CleanupPlan {
    /// 削除予定のセグメント数。
    pub fn segments(&self) -> u64 {
        self.victims.len() as u64
    }

    /// 削除してよい LSN の上限（`last_lsn` がこれ以下のセグメントだけが対象）。
    pub fn cutoff(&self) -> u64 {
        self.cutoff
    }

    /// 削除予定の LSN 範囲（無ければ `None`）。
    pub fn lsn_range(&self) -> Option<(u64, u64)> {
        let a = self.victims.first()?.first_lsn;
        let b = self.victims.last()?.last_lsn;
        Some((a, b))
    }
}

/// [`execute_archive_cleanup`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupReport {
    /// 削除したセグメント数。
    pub removed: u64,
}

/// 保持・削除の失敗。`Display` は固定の英語文言で、パス・LSN・件数を含めない。
#[derive(Debug)]
pub enum CleanupError {
    /// 保持するフルバックアップが 1 つも指定されていない。
    NoBackups,
    /// フルバックアップではない（増分バックアップ）。
    NotFullBackup,
    /// バックアップを DB ファイルとして読めない。
    BackupUnreadable,
    /// 別のプロセスが DB を開いている。
    InUse,
    /// 変更ログを有効にしていない DB。
    NotEnabled,
    /// バックアップ同士、またはアーカイブと DB 識別子が食い違う。
    ForeignDatabase,
    /// バックアップ同士、またはアーカイブと timeline が食い違う。
    ForeignTimeline,
    /// アーカイブ先が他者に書き込める。
    InsecureDirectory,
    /// アーカイブの検証・走査の失敗。
    Archive(ArchiveError),
    /// 計画の後でファイルが差し替えられた（何も消さずに止まる）。
    Replaced,
    /// 一部を消した後で失敗した（先頭から `removed` 個は消えている。残りは連続した後ろ側の区間）。
    Partial { removed: u64 },
    /// ファイル操作の失敗。
    Io(std::io::ErrorKind),
}

impl std::fmt::Display for CleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::NoBackups => "no full backup was given to retain",
            Self::NotFullBackup => "the backup is not a full backup",
            Self::BackupUnreadable => "the backup cannot be read as a database file",
            Self::InUse => "the backup is in use by another process",
            Self::NotEnabled => "the backup has no change log",
            Self::ForeignDatabase => "the backups and the archive belong to different databases",
            Self::ForeignTimeline => "the backups and the archive are on different timelines",
            Self::InsecureDirectory => "archive directory is writable by other users",
            Self::Archive(_) => "archive verification failed",
            Self::Replaced => "an archive segment was replaced during cleanup",
            Self::Partial { .. } => "cleanup stopped partway (the remaining archive is contiguous)",
            Self::Io(_) => "archive i/o error",
        };
        f.write_str(text)
    }
}

impl std::error::Error for CleanupError {}

impl From<ArchiveError> for CleanupError {
    fn from(e: ArchiveError) -> Self {
        match e {
            ArchiveError::InsecureDirectory => Self::InsecureDirectory,
            other => Self::Archive(other),
        }
    }
}

impl From<std::io::Error> for CleanupError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.kind())
    }
}

/// フルバックアップ（redb の DB ファイル）`path` の位置を、読み取り専用で読む。増分バックアップ
/// （先頭が [`SEG_MAGIC`]）は [`CleanupError::NotFullBackup`] で拒否する。
pub fn retention_point_of(path: &Path) -> Result<RetentionPoint, CleanupError> {
    let mut file = std::fs::File::open(path).map_err(|_| CleanupError::BackupUnreadable)?;
    if !file
        .metadata()
        .map_err(|_| CleanupError::BackupUnreadable)?
        .is_file()
    {
        return Err(CleanupError::BackupUnreadable);
    }
    let mut magic = [0u8; 4];
    if file.read_exact(&mut magic).is_ok() && magic == SEG_MAGIC {
        return Err(CleanupError::NotFullBackup);
    }
    drop(file);
    let db = redb::ReadOnlyDatabase::open(path).map_err(|e| match e {
        redb::DatabaseError::DatabaseAlreadyOpen => CleanupError::InUse,
        _ => CleanupError::BackupUnreadable,
    })?;
    let read = db
        .begin_read()
        .map_err(|_| CleanupError::BackupUnreadable)?;
    let (db_id, timeline) = read_identity(&read).map_err(|e| match e {
        IncrementalError::NotEnabled => CleanupError::NotEnabled,
        _ => CleanupError::BackupUnreadable,
    })?;
    let end_lsn = crate::storage::current_generation_in_txn(&read)
        .map_err(|_| CleanupError::BackupUnreadable)?;
    let first_log_lsn = match read.open_table(CHANGE_LOG_TABLE) {
        Ok(t) => t
            .first()
            .map_err(|_| CleanupError::BackupUnreadable)?
            .map(|(k, _)| k.value()),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(_) => return Err(CleanupError::BackupUnreadable),
    };
    Ok(RetentionPoint {
        db_id,
        timeline,
        end_lsn,
        first_log_lsn,
    })
}

/// 削除の上限（`last_lsn <= cutoff` だけが対象）。次の最小値。
///
/// - 保持する全バックアップの終点の最小値（最も古いバックアップから PITR できる区間を残す）
/// - DB 側の刈り込みが詰まらないための、稼働中の DB の先頭行の手前。`db` があればその値、無ければ
///   バックアップの先頭行から見積もる。バックアップの先頭行は取得後の DB の先頭行以下なので、
///   最大のものの手前が（保守側に外れない）有効な上限になる。先頭行が空のバックアップが 1 つでも
///   あれば、その終点より後ろにしか DB の行は無いため、この項は上限を下げない
fn cutoff_of(policy: &CleanupPolicy) -> u64 {
    let mut cutoff = policy.backups.iter().map(|b| b.end_lsn).min().unwrap_or(0);
    let first_row_bound = match &policy.db {
        Some(db) => db.first_log_lsn,
        None => policy
            .backups
            .iter()
            .map(|b| b.first_log_lsn)
            .collect::<Option<Vec<u64>>>()
            .and_then(|v| v.into_iter().max()),
    };
    if let Some(first) = first_row_bound {
        cutoff = cutoff.min(first.saturating_sub(1));
    }
    cutoff
}

/// 最後のレコードの commit 時刻（壁時計 UTC マイクロ秒）。レコードを 1 件ずつ読み、最後のものだけ残す。
fn last_commit_time_us(path: &Path) -> Result<i64, CleanupError> {
    let max_payload = crate::storage::change_log::CaptureLimits::default().max_payload_bytes;
    let mut reader = SegmentReader::open(path, max_payload)?;
    let mut last = None;
    while let Some(record) = reader.next_record()? {
        let h = peek_record(&record, max_payload).map_err(super::map_decode)?;
        last = Some(h.commit_time_us);
    }
    last.ok_or(CleanupError::Archive(ArchiveError::BadFormat))
}

/// 削除を計画する。アーカイブ全体を検証し（期待値はバックアップの DB 識別子・timeline）、
/// 先頭からの連続区間で `last_lsn <= cutoff` かつ保持期間・世代数の条件を満たすものを選ぶ。何も消さない。
pub fn plan_archive_cleanup(
    dir: &Path,
    policy: &CleanupPolicy,
) -> Result<CleanupPlan, CleanupError> {
    let Some(first) = policy.backups.first() else {
        return Err(CleanupError::NoBackups);
    };
    for p in policy.backups.iter().chain(policy.db.iter()) {
        if p.db_id != first.db_id {
            return Err(CleanupError::ForeignDatabase);
        }
        if p.timeline != first.timeline {
            return Err(CleanupError::ForeignTimeline);
        }
    }
    check_dir(dir)?;
    check_dir_not_shared(dir)?;
    let expect = ArchiveExpectation {
        db_id: Some(first.db_id),
        timeline: Some(first.timeline),
        start_lsn: None,
    };
    let (_, segments) = verified_segments(dir, &expect)?;
    let cutoff = cutoff_of(policy);

    let mut candidates: Vec<(PathBuf, SegmentHeader)> = Vec::new();
    for (path, info) in segments {
        if info.header.last_lsn > cutoff {
            break;
        }
        candidates.push((path, info.header));
    }
    if let Some(keep) = policy.keep_segments {
        let keep = usize::try_from(keep).unwrap_or(usize::MAX);
        candidates.truncate(candidates.len().saturating_sub(keep));
    }
    if let Some(secs) = policy.keep_secs {
        let window = i64::try_from(secs)
            .ok()
            .and_then(|s| s.checked_mul(1_000_000))
            .ok_or(CleanupError::Archive(ArchiveError::InvalidOptions))?;
        let limit = policy.now_us.saturating_sub(window);
        let mut keep_n = 0;
        for (path, _) in &candidates {
            // 新しすぎるセグメントで止める（先頭からの連続区間を保つ。壁時計が逆行しても飛びを作らない）。
            if last_commit_time_us(path)? >= limit {
                break;
            }
            keep_n += 1;
        }
        candidates.truncate(keep_n);
    }

    let mut victims = Vec::with_capacity(candidates.len());
    for (path, h) in candidates {
        let meta = std::fs::symlink_metadata(&path)?;
        if !meta.file_type().is_file() {
            return Err(CleanupError::Replaced);
        }
        victims.push(Victim {
            path,
            first_lsn: h.first_lsn,
            last_lsn: h.last_lsn,
            len: meta.len(),
            #[cfg(unix)]
            dev: std::os::unix::fs::MetadataExt::dev(&meta),
            #[cfg(unix)]
            ino: std::os::unix::fs::MetadataExt::ino(&meta),
        });
    }
    Ok(CleanupPlan {
        db_id: first.db_id,
        timeline: first.timeline,
        cutoff,
        victims,
    })
}

/// 削除の直前の再照合。通常ファイルで、開いた fd とパスの (dev, ino, 長さ) が一致し、ヘッダが計画と
/// 同じであること（計画の後の差し替え・シンボリックリンク化を検出する）。
fn recheck(plan: &CleanupPlan, v: &Victim) -> Result<(), CleanupError> {
    let path_meta = std::fs::symlink_metadata(&v.path).map_err(|_| CleanupError::Replaced)?;
    if !path_meta.file_type().is_file() || path_meta.len() != v.len {
        return Err(CleanupError::Replaced);
    }
    let mut file = std::fs::File::open(&v.path).map_err(|_| CleanupError::Replaced)?;
    let fd_meta = file.metadata().map_err(|_| CleanupError::Replaced)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if fd_meta.dev() != v.dev
            || fd_meta.ino() != v.ino
            || path_meta.dev() != v.dev
            || path_meta.ino() != v.ino
        {
            return Err(CleanupError::Replaced);
        }
    }
    if fd_meta.len() != v.len {
        return Err(CleanupError::Replaced);
    }
    let mut prefix = [0u8; SEG_PREFIX_LEN];
    file.read_exact(&mut prefix)
        .map_err(|_| CleanupError::Replaced)?;
    let h = SegmentHeader::decode(&prefix).map_err(|_| CleanupError::Replaced)?;
    if h.db_id != plan.db_id
        || h.timeline != plan.timeline
        || h.first_lsn != v.first_lsn
        || h.last_lsn != v.last_lsn
    {
        return Err(CleanupError::Replaced);
    }
    Ok(())
}

/// ディレクトリのエントリ変更を永続化する。
#[cfg(unix)]
fn sync_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_dir: &Path) -> std::io::Result<()> {
    Ok(())
}

/// 計画を実行する（昇順に 1 件ずつ消す）。
pub fn execute_archive_cleanup(
    dir: &Path,
    plan: &CleanupPlan,
) -> Result<CleanupReport, CleanupError> {
    execute_with(dir, plan, |p| std::fs::remove_file(p))
}

/// [`execute_archive_cleanup`] の本体。`remove` はテストが途中失敗を注入するために差し替える。
/// 各ファイルを「再照合 → 削除 → ディレクトリ fsync」の順で処理し、最初のエラーで止める。
/// 1 件以上消した後の失敗は [`CleanupError::Partial`]（先頭から消しているので残りは連続した区間）。
fn execute_with(
    dir: &Path,
    plan: &CleanupPlan,
    mut remove: impl FnMut(&Path) -> std::io::Result<()>,
) -> Result<CleanupReport, CleanupError> {
    check_dir(dir)?;
    check_dir_not_shared(dir)?;
    let mut removed = 0u64;
    for v in &plan.victims {
        let step = (|| -> Result<(), CleanupError> {
            recheck(plan, v)?;
            remove(&v.path)?;
            sync_dir(dir)?;
            Ok(())
        })();
        match step {
            Ok(()) => removed += 1,
            Err(e) if removed == 0 => return Err(e),
            Err(_) => return Err(CleanupError::Partial { removed }),
        }
    }
    Ok(CleanupReport { removed })
}

#[cfg(test)]
mod tests;
