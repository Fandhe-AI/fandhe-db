//! `restore --generation-state <path>` の状態ファイル（復元をまたぐ世代の高水位。Issue #3105・
//! RECOVER-17 のポインタ。設計は `docs/design/restore-generation.md`）の入出力。
//!
//! 責務境界: パスの検査（通常ファイル・所有者・権限・親ディレクトリ・差し替え）、排他ロック、
//! 固定長の読み書きと fsync を持つ。形式（スロット・checksum・単調性）は engine の
//! `decode_high_water`・`encode_next_slot` が担う。`restore_opt` の物理の復元（`restore_from_copy`）から、
//! 検査を通った一時コピーの世代を進める直前に開かれ、公開（`hard_link`）の後まで保持される。
//!
//! 失敗はすべて [`RestoreError`] の固定文言で返し、パス・値を含めない（fail-closed。どの失敗でも
//! 呼び出し元は何も公開しない）。復元元のファイルには一切触れない。

use std::fs::{File, OpenOptions, TryLockError};
use std::io::ErrorKind;
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use engine::storage::{decode_high_water, encode_next_slot, HighWaterState, HIGH_WATER_FILE_LEN};

use crate::restore_opt::RestoreError;

/// 開いてロックした状態ファイル。drop でロックを解放する（公開の後まで保持する）。
pub(crate) struct GenerationState {
    file: File,
    parent: PathBuf,
    created: bool,
}

impl GenerationState {
    /// 状態ファイルを検査して開き、排他ロックを取る。`own_uid` は一時コピーの所有者（自分）の uid。
    /// 取れなければ [`RestoreError::GenerationStateInUse`]、検査に通らなければ
    /// [`RestoreError::GenerationStateRejected`]。
    pub(crate) fn open(path: &Path, own_uid: u32) -> Result<Self, RestoreError> {
        let rejected = || RestoreError::GenerationStateRejected;
        let parent: &Path = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        // グループ・その他が書き込める親では、他者による削除・差し替えで黙って保証を失う。
        crate::dump_tmp::check_parent_not_shared(parent).map_err(|_| rejected())?;

        // 開く前にリンクをたどらずに種別を確かめる（シンボリックリンク・ディレクトリは拒否）。
        let (file, created) = match std::fs::symlink_metadata(path) {
            Ok(m) if m.is_file() => (open_existing(path)?, false),
            Ok(_) => return Err(rejected()),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)
                {
                    Ok(f) => (f, true),
                    // 作成の競合は既存のファイルとして開き直す（以降の検査は同じ）。
                    Err(e) if e.kind() == ErrorKind::AlreadyExists => (open_existing(path)?, false),
                    Err(e) => return Err(RestoreError::Io(e.kind())),
                }
            }
            Err(_) => return Err(rejected()),
        };

        // 開いた fd の属性と、再度のパスの属性を照合する（開く間の差し替えを拒否する）。
        let fd_meta = file.metadata().map_err(|_| rejected())?;
        let path_meta = std::fs::symlink_metadata(path).map_err(|_| rejected())?;
        if !fd_meta.is_file()
            || !path_meta.is_file()
            || fd_meta.dev() != path_meta.dev()
            || fd_meta.ino() != path_meta.ino()
            || fd_meta.uid() != own_uid
            || fd_meta.mode() & 0o077 != 0
        {
            return Err(rejected());
        }
        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(RestoreError::GenerationStateInUse),
            Err(TryLockError::Error(e)) => return Err(RestoreError::Io(e.kind())),
        }
        if created {
            // 作成したディレクトリエントリを永続化する。
            File::open(parent)
                .and_then(|d| d.sync_all())
                .map_err(|e| RestoreError::Io(e.kind()))?;
        }
        Ok(Self {
            file,
            parent: parent.to_path_buf(),
            created,
        })
    }

    /// 高水位を読む。長さは固定の上限（[`HIGH_WATER_FILE_LEN`]）で検証してから読み、不正なら拒否する。
    pub(crate) fn read(&self) -> Result<HighWaterState, RestoreError> {
        let rejected = || RestoreError::GenerationStateRejected;
        let len = self.file.metadata().map_err(|_| rejected())?.len();
        if len == 0 {
            return decode_high_water(&[]).map_err(|_| rejected());
        }
        if len != HIGH_WATER_FILE_LEN as u64 {
            return Err(rejected());
        }
        let mut buf = [0u8; HIGH_WATER_FILE_LEN];
        self.file
            .read_exact_at(&mut buf, 0)
            .map_err(|_| rejected())?;
        decode_high_water(&buf).map_err(|_| rejected())
    }

    /// `next` を新しい高水位として書いて fsync する。直前の値を保つ側のスロットには触れない。
    /// 失敗したら呼び出し元は公開しない（高水位が進まないまま世代だけが進んだ復元先を作らない）。
    pub(crate) fn record(&self, state: &HighWaterState, next: u64) -> Result<(), RestoreError> {
        let io = |e: std::io::Error| RestoreError::Io(e.kind());
        let (offset, slot) =
            encode_next_slot(state, next).map_err(|_| RestoreError::GenerationFailed)?;
        // 初回は長さ 0。全 0 の 64 バイトは新規として読めるので、書きかけで止まっても安全。
        if self.file.metadata().map_err(io)?.len() < HIGH_WATER_FILE_LEN as u64 {
            self.file.set_len(HIGH_WATER_FILE_LEN as u64).map_err(io)?;
        }
        self.file.write_all_at(&slot, offset).map_err(io)?;
        self.file.sync_all().map_err(io)?;
        if self.created {
            File::open(&self.parent)
                .and_then(|d| d.sync_all())
                .map_err(io)?;
        }
        Ok(())
    }
}

fn open_existing(path: &Path) -> Result<File, RestoreError> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|_| RestoreError::GenerationStateRejected)
}
