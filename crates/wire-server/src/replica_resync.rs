//! レプリカの再同期（ベースバックアップからの再構築）の本体（Issue #2176・親 #2171・RECOVER-15・
//! RECOVER-13 ポインタ。方式の記録は `docs/design/replication-streaming.md` の「実装判断（Issue #2176）」）。
//!
//! 責務境界: `main.rs::run_replica_resync` から呼ばれ、稼働中のプライマリのバックアップソケットから
//! フルバックアップを取り、公開前に検査してから `--out` へ原子的に公開する。変更ログの受信と適用は
//! 公開後に `wire-server replica`（[`crate::replica`]）が行う。ベースの連続性検査は engine の
//! `replica_base_point_of`、整合検査は [`crate::restore_opt::verify_db`]、フレームの読み書きは
//! [`crate::replica`] の関数を再利用する。
//!
//! 手順（fail-closed。どの失敗でも最終名へ何も公開しない）:
//!
//! 1. `--out` と同じディレクトリの一時名へ、バックアップソケット経由でフルバックアップを書かせる
//! 2. 一時ファイルを整合検査する（矛盾・検査できない項目があれば公開しない）
//! 3. ベースが変更ログと連続していることを確かめ、切り替え点（終点 LSN）を得る
//! 4. プライマリへ確認接続する（Hello → Identity を照合 → Start(切り替え点) → Ok を受けたら Ack せず閉じる）。
//!    切り替え点がもう保持範囲の外（`RESYNC_REQUIRED`）、別系統、認証失敗などは公開しない
//! 5. 一時名から `--out` へ `hard_link` で公開する（既存は `EEXIST` で拒否）。公開先が検査したファイルと
//!    同じ inode でなければ取り消す。一時名を消して親ディレクトリを fsync する
//!
//! **復元の世代番号（`restore_generation`）は進めない**。レプリカの当該表は変更ログの記録対象外で、
//! プライマリの値に追いつけないため、進めるとプライマリと永久にずれる（`restore` を通さない理由）。
//!
//! 入力（プライマリのフレーム）は untrusted: 本体長は確保前に上限検証し、受信データの経路で
//! `unwrap`／`expect`／添字アクセスを使わない。エラー文は固定の英語で、パス・LSN・資格情報を含まない。
#![cfg(unix)]

use std::net::TcpStream;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use engine::storage::{IncrementalError, IntegrityOptions};

use crate::replica::{be_u64, classify_error, read_frame, write_frame, FrameError};
use crate::replica_resync_opt::{
    ReplicaResyncOptions, EXIT_BASE_REJECTED, EXIT_FAILURE, EXIT_INCOMPLETE, EXIT_INCONSISTENT,
    EXIT_NOT_FINALIZED, EXIT_PRIMARY_REJECTED, EXIT_USAGE,
};
use crate::replication::{code, PROTOCOL_VERSION, T_ERROR, T_HELLO, T_IDENTITY, T_OK, T_START};
use crate::{backup_opt, backup_socket, restore_opt, verify_opt};

/// 確認接続の接続・ハンドシェイク・書き込みの期限。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// ハンドシェイクのフレームの本体長の上限（Identity・Ok は 32 バイト）。
const MAX_HANDSHAKE_BODY: usize = 64;

/// 再同期の失敗。`Display` は固定の英語で、パス・LSN・資格情報を含まない。
#[derive(Debug)]
#[non_exhaustive]
pub enum ResyncError {
    /// バックアップソケットへの依頼の失敗（接続・応答の不正）。
    Request,
    /// バックアップソケットの応答が失敗（結果コード）。
    Backup(u8),
    /// バックアップ側が「公開済みだが後始末を確認できない」（結果 3）を返した。公開先は一時名で、
    /// `--out` は作られないため、終了コード 3（`--out` 公開済み）とは区別して再試行可能な失敗にする。
    BackupTempNotFinalized,
    /// 整合検査を実行できなかった・一時ファイルを検査できなかった。
    VerifyFailed,
    /// 整合検査で矛盾（4）または検査できない項目（5）。
    NotConsistent(u8),
    /// ベースが変更ログの連続性を満たさない・レプリカのベースでない。
    BaseRejected,
    /// プライマリが確認接続を拒否した（別系統・認証失敗・保持範囲の外など）。
    PrimaryRejected(&'static str),
    /// プライマリに接続できない・応答が途絶えた。
    PrimaryUnreachable,
    /// 公開先が既に存在する・不正（公開直前の競合を含む）。
    InvalidDestination,
    /// 一時ファイルが検査後に差し替えられた。公開しなかった。
    TempReplaced,
    /// 検査していない内容が公開先に残った（取り消しの削除に失敗）。
    UncheckedContentPublished,
    /// 公開済みで中身は完全だが、後始末を確かめられない。
    NotFinalized,
    /// その他の入出力の失敗。
    Io,
}

impl std::fmt::Display for ResyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Request => {
                "cannot take the base backup from the primary (is it running with --backup-socket?)"
            }
            Self::Backup(c) => backup_socket::code_message(*c),
            Self::BackupTempNotFinalized => {
                "the base backup could not be finalized; nothing was published to --out, retry"
            }
            Self::VerifyFailed => "the base could not be checked; nothing was published",
            Self::NotConsistent(_) => "the base failed the integrity check; nothing was published",
            Self::BaseRejected => {
                "the base is not a valid replica base (its change log is not continuous); \
                 nothing was published"
            }
            Self::PrimaryRejected(why) => why,
            Self::PrimaryUnreachable => {
                "cannot reach the primary replication endpoint; nothing was published"
            }
            Self::InvalidDestination => "--out already exists or is not a valid destination",
            Self::TempReplaced => "temporary base was replaced or removed; nothing was published",
            Self::UncheckedContentPublished => {
                "WARNING: the destination holds content that was not checked; delete it manually"
            }
            Self::NotFinalized => {
                "WARNING: the replica database was published but its durability could not be \
                 confirmed; verify it before relying on it"
            }
            Self::Io => "replica-resync failed due to an I/O error",
        })
    }
}

impl std::error::Error for ResyncError {}

impl ResyncError {
    /// 固定の終了コード（[`crate::replica_resync_opt`] の表）。
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Backup(c) => *c,
            Self::NotConsistent(c) => *c,
            Self::BaseRejected => EXIT_BASE_REJECTED,
            Self::PrimaryRejected(_) => EXIT_PRIMARY_REJECTED,
            Self::InvalidDestination => EXIT_USAGE,
            Self::NotFinalized => EXIT_NOT_FINALIZED,
            Self::Request
            | Self::BackupTempNotFinalized
            | Self::VerifyFailed
            | Self::PrimaryUnreachable
            | Self::TempReplaced
            | Self::UncheckedContentPublished
            | Self::Io => EXIT_FAILURE,
        }
    }
}

// `NotConsistent` の値が表の定数と一致することの静的な固定。
const _: () = assert!(EXIT_INCONSISTENT == 4 && EXIT_INCOMPLETE == 5);

/// 一時ファイルの保持（dev・ino・長さ）。検査したファイルと公開先の同一性を確かめる。
struct TempBase {
    path: PathBuf,
    dev: u64,
    ino: u64,
    len: u64,
    /// 真の間は drop で一時名を消す（公開に成功したら偽にする）。
    armed: bool,
    /// 消せなかったときの通知用（呼び出し元の `leftover` へ移す）。
    leftover: Option<PathBuf>,
}

impl TempBase {
    /// 一時名のパスが通常ファイル（シンボリックリンクでない）であることを確かめて保持する。
    fn adopt(path: PathBuf) -> Result<Self, ResyncError> {
        let m = std::fs::symlink_metadata(&path).map_err(|_| ResyncError::Request)?;
        if !m.file_type().is_file() || m.len() == 0 {
            return Err(ResyncError::TempReplaced);
        }
        Ok(Self {
            path,
            dev: m.dev(),
            ino: m.ino(),
            len: m.len(),
            armed: true,
            leftover: None,
        })
    }

    /// `path`（リンクをたどらない）が、保持している一時ファイルと同じ通常ファイルか。
    fn is_same_file_at(&self, path: &Path) -> bool {
        match std::fs::symlink_metadata(path) {
            Ok(m) => {
                m.file_type().is_file()
                    && m.dev() == self.dev
                    && m.ino() == self.ino
                    && m.len() == self.len
            }
            Err(_) => false,
        }
    }

    fn remove(&mut self) {
        if self.armed {
            self.armed = false;
            if std::fs::remove_file(&self.path).is_err() && self.path.exists() {
                self.leftover = Some(self.path.clone());
            }
        }
    }
}

impl Drop for TempBase {
    fn drop(&mut self) {
        self.remove();
    }
}

/// `--out` と同じディレクトリの、まだ存在しない一時名を決める（0600 の作成はサーバー側）。
fn temp_name(out: &Path) -> Result<PathBuf, ResyncError> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let parent = out.parent().ok_or(ResyncError::InvalidDestination)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(
        ".fandhe-replica-resync-{}-{seq}-{nanos}.redb",
        std::process::id()
    )))
}

/// サーバーが一時名を残したままの失敗に備え、あれば消す（自分が決めた名前だけ。リンクはたどらない）。
/// 消せなかったときは `leftover` に入れ、呼び出し元が通知できるようにする。
fn discard_temp_name(tmp_path: PathBuf, leftover: &mut Option<PathBuf>) {
    if std::fs::symlink_metadata(&tmp_path).is_ok() && std::fs::remove_file(&tmp_path).is_err() {
        *leftover = Some(tmp_path);
    }
}

/// 一時名を `TempBase` として保持する。保持できない（差し替え・空など）ときも、全テナントの
/// スナップショットが残らないよう一時名を片付ける。
fn adopt_or_discard(
    tmp_path: PathBuf,
    leftover: &mut Option<PathBuf>,
) -> Result<TempBase, ResyncError> {
    match TempBase::adopt(tmp_path.clone()) {
        Ok(t) => Ok(t),
        Err(e) => {
            discard_temp_name(tmp_path, leftover);
            Err(e)
        }
    }
}

/// 再同期を実行して、切り替え点の LSN を返す。失敗時は何も公開しない。一時名を消せなかったときは、
/// 残ったパスを `leftover` に入れる（全テナントのデータが残るので、呼び出し元が通知する）。
pub fn resync(
    opts: &ReplicaResyncOptions,
    leftover: &mut Option<PathBuf>,
) -> Result<u64, ResyncError> {
    // 早期失敗のための確認。最終判定は公開の `hard_link`（確認後の競合を残さない）。
    if std::fs::symlink_metadata(&opts.out).is_ok() {
        return Err(ResyncError::InvalidDestination);
    }
    let tmp_path = temp_name(&opts.out)?;

    // 1. ベースの取得（プライマリのサーバー側が 0600 の一時名から原子的に公開する）。
    let result = match backup_socket::request(&opts.socket, &tmp_path) {
        Ok(r) if r.code == 0 => Ok(()),
        // 結果 3 は一時名への公開のみ。`--out` は未作成なので、終了コード 3 として転送しない。
        Ok(r) if r.code == 3 => Err(ResyncError::BackupTempNotFinalized),
        Ok(r) => Err(ResyncError::Backup(r.code)),
        Err(_) => Err(ResyncError::Request),
    };
    if let Err(e) = result {
        discard_temp_name(tmp_path, leftover);
        return Err(e);
    }
    let mut tmp = adopt_or_discard(tmp_path, leftover)?;
    let outcome = check_and_publish(opts, &mut tmp, leftover);
    tmp.remove();
    if leftover.is_none() {
        *leftover = tmp.leftover.take();
    }
    outcome
}

/// 検査（整合・連続性・プライマリへの確認）と公開。
fn check_and_publish(
    opts: &ReplicaResyncOptions,
    tmp: &mut TempBase,
    leftover: &mut Option<PathBuf>,
) -> Result<u64, ResyncError> {
    // 2. 整合検査。
    let (report, _mode) = restore_opt::verify_db(&tmp.path, &IntegrityOptions::default(), leftover)
        .map_err(|_| ResyncError::VerifyFailed)?;
    match verify_opt::classify(&report) {
        0 => {}
        c => return Err(ResyncError::NotConsistent(c)),
    }
    // 3. ベースの連続性と切り替え点。
    let point = engine::storage::replica_base_point_of(&tmp.path).map_err(|e| match e {
        IncrementalError::BaseUnreadable | IncrementalError::InUse => ResyncError::VerifyFailed,
        _ => ResyncError::BaseRejected,
    })?;
    // 4. プライマリへの確認接続。
    probe_primary(opts, &point)?;
    // 5. 公開。
    publish(tmp, &opts.out)?;
    Ok(point.end_lsn)
}

/// 確認接続。Hello → Identity（ベースと照合）→ Start(切り替え点) → Ok を受けたら Ack せず閉じる。
fn probe_primary(
    opts: &ReplicaResyncOptions,
    point: &engine::storage::ChainPoint,
) -> Result<(), ResyncError> {
    let stream = TcpStream::connect_timeout(&opts.primary, CONNECT_TIMEOUT)
        .map_err(|_| ResyncError::PrimaryUnreachable)?;
    let _ = stream.set_nodelay(true);
    stream
        .set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|_| ResyncError::PrimaryUnreachable)?;
    let role_len = u8::try_from(opts.role.len()).map_err(|_| {
        ResyncError::PrimaryRejected("the primary rejected the connection; nothing was published")
    })?;
    write_frame(
        &stream,
        T_HELLO,
        &[
            &PROTOCOL_VERSION.to_be_bytes(),
            &[role_len],
            opts.role.as_bytes(),
            &opts.password,
        ],
    )
    .map_err(|_| ResyncError::PrimaryUnreachable)?;

    let identity = expect_frame(&stream, T_IDENTITY)?;
    let same = identity.get(..16) == Some(&point.db_id[..])
        && identity.get(16..24).and_then(be_u64) == Some(point.timeline);
    if !same {
        return Err(ResyncError::PrimaryRejected(
            "the primary belongs to another database; nothing was published",
        ));
    }
    write_frame(
        &stream,
        T_START,
        &[
            &point.db_id,
            &point.timeline.to_be_bytes(),
            &point.end_lsn.to_be_bytes(),
        ],
    )
    .map_err(|_| ResyncError::PrimaryUnreachable)?;
    expect_frame(&stream, T_OK)?;
    // Ack を送らず閉じる（保持位置の登録は切断で外れる）。
    Ok(())
}

/// ハンドシェイクのフレームを 1 つ読む。期待した型（本体 32 バイト）か `E`。それ以外はプロトコル違反。
fn expect_frame(stream: &TcpStream, expect: u8) -> Result<Vec<u8>, ResyncError> {
    match read_frame(stream, MAX_HANDSHAKE_BODY, HANDSHAKE_TIMEOUT) {
        Ok((ty, body)) if ty == expect && body.len() == 32 => Ok(body),
        Ok((T_ERROR, body)) => {
            let c = body.first().copied();
            if c == Some(code::RESYNC_REQUIRED) {
                Err(ResyncError::PrimaryRejected(
                    "the base is already outside the range retained by the primary; take a new \
                     base (increase the retention if this repeats); nothing was published",
                ))
            } else if c.and_then(classify_error).is_some() {
                Err(ResyncError::PrimaryRejected(
                    "the primary rejected the connection; nothing was published",
                ))
            } else {
                Err(ResyncError::PrimaryUnreachable)
            }
        }
        Ok(_) | Err(FrameError::TooLarge) => Err(ResyncError::PrimaryRejected(
            "protocol violation by the primary; nothing was published",
        )),
        Err(FrameError::Closed) => Err(ResyncError::PrimaryUnreachable),
    }
}

/// 一時名から `dest` へ `hard_link` で公開する。公開先が検査したファイルと別なら取り消す。
fn publish(tmp: &mut TempBase, dest: &Path) -> Result<(), ResyncError> {
    if !tmp.is_same_file_at(&tmp.path) {
        return Err(ResyncError::TempReplaced);
    }
    match std::fs::hard_link(&tmp.path, dest) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(ResyncError::InvalidDestination)
        }
        Err(_) => return Err(ResyncError::Io),
    }
    if !tmp.is_same_file_at(dest) {
        // `hard_link` は新規作成なので公開先は自分が作ったリンク。削除して公開を取り消す。
        return match std::fs::remove_file(dest) {
            Ok(()) => Err(ResyncError::TempReplaced),
            Err(_) => Err(ResyncError::UncheckedContentPublished),
        };
    }
    // 公開済みで検査したファイルと一致した。以降の失敗でも最終ファイルは完全なので消さない。
    let parent = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    tmp.armed = false;
    let finalized = std::fs::remove_file(&tmp.path)
        .and_then(|()| std::fs::File::open(parent).and_then(|d| d.sync_all()));
    if finalized.is_err() {
        if tmp.path.exists() {
            tmp.leftover = Some(tmp.path.clone());
        }
        return Err(ResyncError::NotFinalized);
    }
    Ok(())
}

// 公開した場合の `backup_opt` 定数との整合（終了コードの表の固定）。
const _: () = assert!(EXIT_USAGE == backup_opt::EXIT_USAGE);

#[cfg(test)]
mod adopt_cleanup_tests {
    use super::*;

    fn scratch(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "fandhe-resync-adopt-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&d).expect("mkdir");
        d
    }

    #[test]
    fn adopt_failure_removes_temp_file() {
        let dir = scratch("empty");
        let tmp = dir.join(".fandhe-replica-resync-test.redb");
        std::fs::write(&tmp, b"").expect("write");
        let mut leftover = None;
        let r = adopt_or_discard(tmp.clone(), &mut leftover);
        assert!(matches!(r, Err(ResyncError::TempReplaced)));
        assert!(std::fs::symlink_metadata(&tmp).is_err());
        assert!(leftover.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn adopt_failure_records_leftover_when_unremovable() {
        let dir = scratch("dir");
        let tmp = dir.join(".fandhe-replica-resync-test.redb");
        std::fs::create_dir(&tmp).expect("mkdir");
        std::fs::write(tmp.join("x"), b"x").expect("write");
        let mut leftover = None;
        let r = adopt_or_discard(tmp.clone(), &mut leftover);
        assert!(r.is_err());
        assert_eq!(leftover.as_deref(), Some(tmp.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
