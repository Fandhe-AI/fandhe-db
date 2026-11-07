//! `dump`／`dump-restore` が出力を一時名へ書き、成功したときだけ最終名へ公開するための
//! 一時ファイル（Issue #2162・RECOVER-16 ポインタ。方式は `docs/design/logical-dump.md` の第 12 節）。
//!
//! 責務境界: 一時名の排他作成（0600）・保持 fd による同一性確認・`hard_link` による公開
//! （既存の宛先は上書きしない）・失敗時の削除だけを担う。`restore_opt` の `TempCopy` と同じ手順だが、
//! あちらは `restore_opt` の内部型で公開されていないため、内部型を公開せずに済ませる目的で、
//! 本モジュールに小さく持っている（意図した重複）。`dump_run`・`dump_restore` が使う。

use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 一時ファイル操作の失敗（種別だけを持ち、パスの内容は含めない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmpError {
    /// 宛先が不正（親が無い・既に存在する）。
    InvalidDestination,
    /// 一時名が検査後に差し替えられた。
    Replaced,
    /// 公開先に、書いたものと異なる内容が残った（取り消しの削除にも失敗した）。
    UncheckedContentPublished,
    /// 公開は済み内容も完全だが、後始末（一時名の削除・親ディレクトリの fsync）を確かめられなかった。
    PublishedButNotFinalized,
    /// ファイルシステムが `hard_link` を持たない。
    Unsupported,
    /// その他の I/O 失敗。
    Io(ErrorKind),
}

fn io_err(e: std::io::Error) -> TmpError {
    TmpError::Io(e.kind())
}

/// 親ディレクトリの存在を確かめて返す（`dest` の親。空なら `.`）。
pub fn parent_of(dest: &Path) -> Result<&Path, TmpError> {
    dest.file_name().ok_or(TmpError::InvalidDestination)?;
    let parent: &Path = match dest.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    match std::fs::metadata(parent) {
        Ok(m) if m.is_dir() => Ok(parent),
        _ => Err(TmpError::InvalidDestination),
    }
}

/// 公開前の一時ファイル。作成時に開いた fd を保持し、公開前の drop で一時名を消す。
pub struct TempFile {
    /// 書き込みに使う保持 fd。
    pub file: File,
    /// 一時名のパス。
    pub path: PathBuf,
    dev: u64,
    ino: u64,
    armed: bool,
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl TempFile {
    /// `parent` に所有者のみ読み書きの一時ファイルを排他的に作る。
    pub fn create(parent: &Path, prefix: &str) -> Result<Self, TmpError> {
        let mut attempts = 0u32;
        loop {
            let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let p = parent.join(format!(".{prefix}-{}-{seq}.tmp", std::process::id()));
            match OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&p)
            {
                Ok(file) => {
                    let m = file.metadata().map_err(io_err)?;
                    return Ok(Self {
                        file,
                        path: p,
                        dev: m.dev(),
                        ino: m.ino(),
                        armed: true,
                    });
                }
                Err(e) if e.kind() == ErrorKind::AlreadyExists && attempts < 8 => {
                    attempts = attempts.saturating_add(1);
                }
                Err(e) => return Err(io_err(e)),
            }
        }
    }

    /// 一時名のパス（リンクをたどらない）が保持 fd と同じ (dev, ino) の通常ファイルで、長さも
    /// 保持 fd と一致するか。
    fn is_same_file_at(&self, path: &Path) -> bool {
        let (Ok(m), Ok(fd)) = (std::fs::symlink_metadata(path), self.file.metadata()) else {
            return false;
        };
        m.is_file() && m.dev() == self.dev && m.ino() == self.ino && m.len() == fd.len()
    }

    /// 一時名が保持 fd のままであることを確かめる（`EngineCore::open` はパスで開くため、
    /// 開く前後に呼んで差し替え・unlink を検出する）。
    pub fn check_path(&self) -> Result<(), TmpError> {
        if self.is_same_file_at(&self.path) {
            Ok(())
        } else {
            Err(TmpError::Replaced)
        }
    }

    /// 一時名を明示的に消す。消せなければ残ったパスを返す（既に無ければ成功扱い）。
    pub fn remove(&mut self) -> Result<(), PathBuf> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(_) => Err(self.path.clone()),
        }
    }

    /// fsync し直し、`dest` へ `hard_link` で公開する（既存の宛先は上書きしない）。公開先が保持 fd と
    /// 同じファイルでなければ公開を取り消す。公開後に一時名を消して親ディレクトリを fsync する。
    pub fn publish(&mut self, dest: &Path) -> Result<(), TmpError> {
        let parent = parent_of(dest)?;
        self.check_path()?;
        self.file.sync_all().map_err(io_err)?;
        self.check_path()?;
        match std::fs::hard_link(&self.path, dest) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                return Err(TmpError::InvalidDestination);
            }
            Err(e) if e.kind() == ErrorKind::Unsupported => return Err(TmpError::Unsupported),
            Err(e) => return Err(io_err(e)),
        }
        if !self.is_same_file_at(dest) {
            return match std::fs::remove_file(dest) {
                Ok(()) => Err(TmpError::Replaced),
                Err(_) => Err(TmpError::UncheckedContentPublished),
            };
        }
        // 公開済みで書いたファイルと一致した。以降の失敗でも最終ファイルは完全なので消さない。
        self.armed = false;
        let finalized = std::fs::remove_file(&self.path)
            .and_then(|()| File::open(parent).and_then(|d| d.sync_all()));
        if finalized.is_err() {
            return Err(TmpError::PublishedButNotFinalized);
        }
        Ok(())
    }
}
