//! `wire-server restore` サブコマンドの引数解釈と復元本体（Issue #2156・RECOVER-13 (3)(6)(7) の
//! ポインタ。親 #2154。バックアップは `backup` が書く `Storage::export_snapshot` の出力）。
//!
//! 責務境界: untrusted な CLI 文字列（`--from`・`--db`）を検証して [`RestoreArgs`] にし、
//! バックアップを新しい DB の位置へ「コピー → 検査 → 公開」する [`restore`] と、その結果の
//! 終了コード対応 [`classify`] を持つ。検査そのものは engine の `Storage::verify_integrity`、
//! 報告の整形・Error/Incomplete の終了コードは [`crate::verify_opt`] と共有する。
//! `main.rs::run_restore` が薄く呼ぶ。
//!
//! # 公開手順（fail-closed。`export_snapshot` の公開手順にそろえる）
//!
//! 1. `--from` を読み取りで開き共有ロックを試みる（稼働中の DB を誤ってコピーしない。取れなければ拒否）
//! 2. 出力先と同じディレクトリの一時名（`.fandhe-restore-<pid>-<seq>.tmp`）へ `create_new`・0600 で
//!    コピーし、サイズ一致を確かめて fsync する。元のバックアップには一切書かない
//! 3. 一時ファイルを `Storage::open` して `verify_integrity` を実行する。Error・Incomplete が 1 件でも
//!    あれば公開せず一時ファイルを消す（検査できなかったものも整合とは扱わない）
//! 4. 検査が通ったら一時ファイルを fsync し直し、`hard_link` で最終名へ公開する（既存の宛先は
//!    `EEXIST` で原子的に拒否。`rename` は置換するため使わない）。一時名を消し、親ディレクトリを fsync する
//!
//! 既存の DB を上書きする明示フラグは設けない（置換には稼働中の判定と原子的な置換の両方が要り、
//! `rename` は開いたままのサーバーの旧 inode を黙って切り離す。運用者が旧ファイルを明示的に退避する）。
//!
//! # 注意
//!
//! 検査のために一時ファイルを `Storage::open` するため、旧形式の DB の一方向移行など `open` の
//! 書き込みが一時ファイル側にだけ入りうる（バックアップは変わらない）。復元後の世代番号を進める
//! 処理（RECOVER-17・#2446。設計は `docs/design/restore-generation.md`）は未実装で、呼び出し位置を
//! [`restore`] の公開直前に示してある。

use std::path::{Path, PathBuf};

use engine::storage::{IntegrityReport, Severity};

use crate::backup_opt::{
    validate_existing_file, validate_new_path, EXIT_FAILURE, EXIT_PUBLISHED_UNVERIFIED, EXIT_USAGE,
};
use crate::verify_opt;

/// 検証済みの `restore` サブコマンド引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreArgs {
    /// 復元元のバックアップファイル（既存の空でない通常ファイル。変更しない）。
    pub from: PathBuf,
    /// 復元先の DB ファイル（まだ存在しない新規パス）。
    pub db: PathBuf,
}

/// `restore` サブコマンドの引数（サブコマンド名より後ろ）を解釈する。未知の引数・値欠落・重複・
/// 必須欠落・復元元と復元先の衝突は拒否する（エラー文は英語で、パスの内容は含めない）。
pub fn parse(args: &[String]) -> Result<RestoreArgs, String> {
    let mut from: Option<&str> = None;
    let mut db: Option<&str> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let slot = match arg.as_str() {
            "--from" => &mut from,
            "--db" => &mut db,
            _ => return Err("unknown argument (usage: restore --from <path> --db <path>)".into()),
        };
        if slot.is_some() {
            return Err(format!("{arg} specified more than once"));
        }
        match it.next() {
            Some(v) => *slot = Some(v.as_str()),
            None => return Err(format!("{arg} requires a value")),
        }
    }
    let (Some(from), Some(db)) = (from, db) else {
        return Err("restore requires --from and --db".into());
    };
    let from = validate_existing_file("--from", from)?;
    // 復元先は新規パスに限る（既存の DB・ファイル・ディレクトリ・リンクは書かずに拒否）。
    let db = validate_new_path("--db", db)?;
    Ok(RestoreArgs { from, db })
}

/// 復元の成功結果（行の値は含めない。CLI の表示用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreSummary {
    /// 検査した報告（Error・Incomplete は 0 件。Warning は含みうる）。
    pub report: IntegrityReport,
}

/// 復元の失敗。`Display` は固定の英語文言で、パス・表名・行の値を含まない。
#[derive(Debug)]
#[non_exhaustive]
pub enum RestoreError {
    /// 復元先に既に何かがある、またはパスが不正（公開直前の競合を含む）。
    InvalidDestination,
    /// 復元元が通常ファイルでない、または長さ 0（parse 後の差し替えを含む。空 DB を公開しない）。
    InvalidSource,
    /// 復元元を別のプロセスが開いている（稼働中の DB をコピーしない）。
    SourceInUse,
    /// 復元元のコピーが元のサイズと一致しなかった（コピー中に変更された等）。
    SourceChanged,
    /// コピーしたバックアップを DB として開けなかった（壊れている・DB ファイルではない）。
    OpenFailed,
    /// 検査を始められなかった。
    VerifyFailed,
    /// 検査で矛盾または検査できない項目が見つかり、公開しなかった。
    NotConsistent(IntegrityReport),
    /// この環境では復元できない（unix 以外・hard link 非対応など）。
    Unsupported,
    /// 最終名への公開は済み中身は完全だが、後始末（一時名の削除・親ディレクトリの fsync）を確かめられなかった。
    PublishedButNotFinalized,
    /// その他の入出力の失敗。
    Io(std::io::ErrorKind),
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidDestination => "--db already exists or is not a valid destination",
            Self::InvalidSource => "--from is not a non-empty regular file",
            Self::SourceInUse => {
                "backup file is in use by another process (stop the server using it first)"
            }
            Self::SourceChanged => "backup file changed while it was being copied",
            Self::OpenFailed => {
                "backup file could not be opened as a database; nothing was published"
            }
            Self::VerifyFailed => "integrity check could not be started; nothing was published",
            Self::NotConsistent(_) => "backup failed the integrity check; nothing was published",
            Self::Unsupported => "restore is not supported in this environment",
            Self::PublishedButNotFinalized => {
                "WARNING: the database file was published but its durability could not be \
                 confirmed; verify it before relying on it"
            }
            Self::Io(_) => "restore failed due to an I/O error",
        })
    }
}

impl std::error::Error for RestoreError {}

/// 復元結果を (終了コード, メッセージ) に対応づける。メッセージは固定の英語（成功時は件数のみ）。
/// 終了コードは [`crate::verify_opt`] の表に従う（矛盾=4・検査できず=5・公開済みで後始末未確認=3）。
pub fn classify(result: &Result<RestoreSummary, RestoreError>) -> (u8, String) {
    match result {
        Ok(s) => (
            0,
            format!(
                "rows_scanned={} warnings={}",
                s.report.rows_scanned,
                s.report.total_by_severity(Severity::Warning)
            ),
        ),
        Err(RestoreError::PublishedButNotFinalized) => (
            EXIT_PUBLISHED_UNVERIFIED,
            RestoreError::PublishedButNotFinalized.to_string(),
        ),
        Err(
            e @ (RestoreError::InvalidDestination
            | RestoreError::InvalidSource
            | RestoreError::Unsupported),
        ) => (EXIT_USAGE, e.to_string()),
        Err(e @ RestoreError::NotConsistent(r)) => (verify_opt::classify(r), e.to_string()),
        Err(e) => (EXIT_FAILURE, e.to_string()),
    }
}

/// バックアップ `from` を新しい DB `dest` へ復元する（モジュール doc の公開手順）。
#[cfg(not(unix))]
pub fn restore(_from: &Path, _dest: &Path) -> Result<RestoreSummary, RestoreError> {
    Err(RestoreError::Unsupported)
}

/// バックアップ `from` を新しい DB `dest` へ復元する（モジュール doc の公開手順）。
#[cfg(unix)]
pub fn restore(from: &Path, dest: &Path) -> Result<RestoreSummary, RestoreError> {
    unix_impl::restore(from, dest)
}

#[cfg(unix)]
mod unix_impl {
    use std::fs::{File, OpenOptions, TryLockError};
    use std::io::ErrorKind;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use engine::storage::{IntegrityOptions, Storage};

    use super::{RestoreError, RestoreSummary};

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    /// 一時ファイルの削除ガード。公開（`hard_link`）前の失敗で一時名を消す。
    struct TempFileGuard {
        path: PathBuf,
        armed: bool,
    }

    impl Drop for TempFileGuard {
        fn drop(&mut self) {
            if self.armed {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }

    fn io_err(e: std::io::Error) -> RestoreError {
        RestoreError::Io(e.kind())
    }

    pub(super) fn restore(from: &Path, dest: &Path) -> Result<RestoreSummary, RestoreError> {
        dest.file_name().ok_or(RestoreError::InvalidDestination)?;
        let parent: &Path = match dest.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        match std::fs::metadata(parent) {
            Ok(m) if m.is_dir() => {}
            _ => return Err(RestoreError::InvalidDestination),
        }
        // 早期失敗のための確認。最終判定は後段の hard_link（確認後の競合を残さない）。
        if std::fs::symlink_metadata(dest).is_ok() {
            return Err(RestoreError::InvalidDestination);
        }

        // 1. 復元元は読み取りだけで開き、共有ロックを試みる。稼働中の DB（redb が排他ロックを
        //    保持）なら取れず、書き込み中のファイルをコピーしない。ロックは copy の間保持する。
        let mut src = File::open(from).map_err(io_err)?;
        match src.try_lock_shared() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(RestoreError::SourceInUse),
            Err(TryLockError::Error(e)) => return Err(io_err(e)),
        }
        // parse 後に差し替えられた（長さ 0・FIFO 等）復元元を、開いた fd の属性で再確認する。
        // 長さ 0 のままだと空の DB が「復元」として公開されてしまう。
        let src_meta = src.metadata().map_err(io_err)?;
        if !src_meta.is_file() || src_meta.len() == 0 {
            return Err(RestoreError::InvalidSource);
        }
        let src_len = src_meta.len();

        // 2. 一時ファイルへ 0600 の排他作成でコピーする。
        let (mut tmp, tmp_path) = create_temp(parent)?;
        let mut guard = TempFileGuard {
            path: tmp_path.clone(),
            armed: true,
        };
        let copied = std::io::copy(&mut src, &mut tmp).map_err(io_err)?;
        if copied != src_len || src.metadata().map_err(io_err)?.len() != src_len {
            return Err(RestoreError::SourceChanged);
        }
        tmp.sync_all().map_err(io_err)?;
        drop(tmp);
        drop(src);

        // 3. コピーを開いて検査する。`Storage` は検査後に必ず drop（排他ロックと redb の
        //    クリーンクローズ書き込みを公開前に終える）。
        let report = {
            let storage = Storage::open(&tmp_path).map_err(|_| RestoreError::OpenFailed)?;
            storage
                .verify_integrity(&IntegrityOptions::default())
                .map_err(|_| RestoreError::VerifyFailed)?
        };
        if !report.is_consistent() {
            return Err(RestoreError::NotConsistent(report));
        }

        // 復元の世代番号を進める処理の呼び出し位置（RECOVER-17・#2446。設計は
        // docs/design/restore-generation.md、実装は #2448）。公開前の一時ファイルへ書く必要がある
        // ため、API ができたらここ（検査の通過後・fsync と公開の前）で呼ぶ。現在は未実装。

        // 4. 公開前に、open・クローズ後の最終内容を fsync し直す。
        OpenOptions::new()
            .read(true)
            .open(&tmp_path)
            .and_then(|f| f.sync_all())
            .map_err(io_err)?;
        match std::fs::hard_link(&tmp_path, dest) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                return Err(RestoreError::InvalidDestination);
            }
            Err(e) if e.kind() == ErrorKind::Unsupported => return Err(RestoreError::Unsupported),
            Err(e) => return Err(io_err(e)),
        }
        // 公開済み。以降の失敗でも最終ファイルは完全なので消さない。
        guard.armed = false;
        let finalized = std::fs::remove_file(&tmp_path)
            .and_then(|()| File::open(parent).and_then(|d| d.sync_all()));
        if finalized.is_err() {
            return Err(RestoreError::PublishedButNotFinalized);
        }
        Ok(RestoreSummary { report })
    }

    /// 出力先と同じディレクトリに、所有者のみ読み書きの一時ファイルを排他的に作る。
    fn create_temp(parent: &Path) -> Result<(File, PathBuf), RestoreError> {
        let mut attempts = 0u32;
        loop {
            let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let p = parent.join(format!(".fandhe-restore-{}-{seq}.tmp", std::process::id()));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&p)
            {
                Ok(f) => return Ok((f, p)),
                Err(e) if e.kind() == ErrorKind::AlreadyExists && attempts < 8 => attempts += 1,
                Err(e) => return Err(io_err(e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rejects_malformed_arguments() {
        assert!(parse(&v(&[])).is_err());
        assert!(parse(&v(&["--from"])).is_err());
        assert!(parse(&v(&["--bogus", "x"])).is_err());
        assert!(parse(&v(&["--from", "a", "--from", "b", "--db", "c"])).is_err());
        assert!(parse(&v(&["--from", "a"])).is_err());
        assert!(parse(&v(&["--db", "c"])).is_err());
    }

    #[test]
    fn rejects_existing_destination_and_missing_source() {
        let exe = std::env::current_exe().expect("current_exe");
        let exe = exe.to_str().expect("utf-8 path");
        // 復元先が既存（実行ファイル自身）
        assert!(parse(&v(&["--from", exe, "--db", exe])).is_err());
        // 復元元が無い
        assert!(parse(&v(&[
            "--from",
            "/nonexistent-for-restore-test",
            "--db",
            "../x"
        ]))
        .is_err());
        assert!(parse(&v(&["--from", "/", "--db", "x"])).is_err());
    }

    #[test]
    fn classify_maps_failures_to_distinct_codes() {
        let r = IntegrityReport::default();
        let ok = Ok(RestoreSummary { report: r.clone() });
        assert_eq!(classify(&ok).0, 0);
        assert_eq!(
            classify(&Err(RestoreError::InvalidDestination)).0,
            EXIT_USAGE
        );
        assert_eq!(classify(&Err(RestoreError::OpenFailed)).0, EXIT_FAILURE);
        assert_eq!(classify(&Err(RestoreError::SourceInUse)).0, EXIT_FAILURE);
        assert_eq!(
            classify(&Err(RestoreError::PublishedButNotFinalized)).0,
            EXIT_PUBLISHED_UNVERIFIED
        );
    }
}
