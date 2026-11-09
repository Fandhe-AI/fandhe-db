//! `dump`／`dump-restore` が出力を一時名へ書き、成功したときだけ最終名へ公開するための
//! 一時ファイル（Issue #2162・RECOVER-16 ポインタ。方式は `docs/design/logical-dump.md` の第 12 節）。
//!
//! 責務境界: 一時名の排他作成（0600）・保持 fd による同一性確認・`hard_link` による公開
//! （既存の宛先は上書きしない）・失敗時の削除だけを担う。`restore_opt` の `TempCopy` と同じ手順だが、
//! あちらは `restore_opt` の内部型で公開されていないため、内部型を公開せずに済ませる目的で、
//! 本モジュールに小さく持っている（意図した重複）。`dump_run`・`dump_restore` が使う。
//! 作業ディレクトリ（[`WorkDir`]・[`check_parent_not_shared`]）は `restore_opt` の `restore`・`verify` の
//! コピー経路も共有する（Issue #3102。一時コピーをパスで開き直す窓を 0700 の作業ディレクトリと保持 fd で塞ぐ）。

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
    /// 親ディレクトリにグループ・その他が書き込める（sticky bit があっても同じ）か、親の所有者が
    /// 作業ディレクトリを作る自分と異なる。一時 DB の差し替えを防げないので使わない。
    InsecureParent,
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

/// 親ディレクトリの権限と所有者が、作業ディレクトリを安全に作れる条件を満たすか（純関数）。
/// グループ・その他の書き込み権限（sticky bit の有無によらず）があれば不可。親の所有者は、そこへ
/// 作業ディレクトリを作る自分（作成直後のディレクトリの所有者）と同じでなければ不可
/// （所有者は名前を差し替えられる。`geteuid` を使わず、mkdir 前に親の中へ作った一時ファイルの所有者である自分の uid と比べる）。
fn parent_is_acceptable(parent_mode: u32, parent_uid: u32, own_uid: u32) -> bool {
    parent_mode & 0o022 == 0 && parent_uid == own_uid
}

/// 自プロセスの実効 uid を、親ディレクトリの中へ `create_new` で作った一時ファイルの所有者として得る
/// （libc に依存しない。親は開いた fd の `fstat` で確認済みの実体であること）。作れなければ拒否する。
fn probe_own_uid(
    parent: &Path,
    parent_fd: &File,
    make_name: &mut dyn FnMut() -> String,
) -> Result<u32, TmpError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let proc_path = PathBuf::from(format!("/proc/self/fd/{}", parent_fd.as_raw_fd()));
    let base = if cfg!(target_os = "linux") && proc_path.is_dir() {
        proc_path
    } else {
        parent.to_path_buf()
    };
    for _ in 0..8 {
        let name = format!("{}.uid-probe", make_name());
        if name.contains('/') {
            return Err(TmpError::InvalidDestination);
        }
        let p = base.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&p)
        {
            Ok(f) => {
                let uid = f.metadata().map(|m| m.uid());
                drop(f);
                let _ = std::fs::remove_file(&p);
                return uid.map_err(io_err);
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err(e)),
        }
    }
    Err(TmpError::InvalidDestination)
}

/// 復元先の親ディレクトリがグループ・その他から書き込めないことを、開いた fd の `fstat` で確かめる
/// （#2167 のアーカイブ先の検査と同じ基準。所有者の照合は [`WorkDir::create`] が行う）。
/// 作業ディレクトリを作る前に呼び、他者が親に書き込めるという競合の前提そのものを無くす。
pub fn check_parent_not_shared(parent: &Path) -> Result<(), TmpError> {
    let m = File::open(parent)
        .and_then(|f| f.metadata())
        .map_err(io_err)?;
    if m.is_dir() && m.mode() & 0o022 == 0 {
        Ok(())
    } else {
        Err(TmpError::InsecureParent)
    }
}

/// 復元中の一時 DB を置く作業ディレクトリ（0700・自分が `mkdir` で新規に作ったもの）。
///
/// 前提: 親ディレクトリにグループ・その他が書き込めないこと（[`check_parent_not_shared`]・作成時にも
/// 保持 fd で再確認）。この前提のもとでは、親の名前空間を変えられるのは親の所有者（自分）だけなので、
/// 作業ディレクトリの作成・fd の取得・一時 DB のパスでの open の間に、他者が名前を差し替える窓は
/// 悪用できない。そのうえで多層防御として、作成直後に開いた fd と実パスの (dev, ino)・所有者・権限を
/// 照合し（[`WorkDir::check`]）、Linux では `/proc/self/fd/<n>` 経由で fd のディレクトリを指す。
/// `/proc/self/fd` が使えない環境や Linux 以外では実パスで開くが、親の前提があるので同様に安全である。
pub struct WorkDir {
    dir: File,
    parent: File,
    path: PathBuf,
    access: PathBuf,
    dev: u64,
    ino: u64,
    uid: u32,
    armed: bool,
}

impl WorkDir {
    /// `parent` の中に 0700 の作業ディレクトリを新規に作る（既存の名前は使わない）。親が共有
    /// （グループ・その他が書き込める）か、所有者が自分と異なれば `InsecureParent` で拒否する。
    pub fn create(parent: &Path, prefix: &str) -> Result<Self, TmpError> {
        Self::create_named(parent, &mut || {
            let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            format!(".{prefix}-{}-{seq}.d", std::process::id())
        })
    }

    /// [`Self::create`] の名前を呼び出し側が決める版（`restore_opt` が nonce 付きの名前で使う。Issue #3102）。
    /// `make_name` は衝突のたびに呼ばれ、親直下の 1 成分（パス区切りを含まない）を返すこと。
    pub fn create_named(
        parent: &Path,
        make_name: &mut dyn FnMut() -> String,
    ) -> Result<Self, TmpError> {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::DirBuilderExt;
        let parent_fd = File::open(parent).map_err(io_err)?;
        let pm = parent_fd.metadata().map_err(io_err)?;
        if !pm.is_dir() || pm.mode() & 0o022 != 0 {
            return Err(TmpError::InsecureParent);
        }
        // 自分の実効 uid は、mkdir 前に親の中へ作った一時ファイルの所有者（カーネルが作成者の fsuid で付ける）
        // から得る。mkdir 後に開いたディレクトリの所有者は差し替えられうるので基準にしない。
        let own_uid = probe_own_uid(parent, &parent_fd, make_name)?;
        if !parent_is_acceptable(pm.mode(), pm.uid(), own_uid) {
            return Err(TmpError::InsecureParent);
        }
        let mut attempts = 0u32;
        let path = loop {
            let name = make_name();
            if name.contains('/') || name == "." || name == ".." {
                return Err(TmpError::InvalidDestination);
            }
            let p = parent.join(name);
            match std::fs::DirBuilder::new().mode(0o700).create(&p) {
                Ok(()) => break p,
                Err(e) if e.kind() == ErrorKind::AlreadyExists && attempts < 8 => {
                    attempts = attempts.saturating_add(1);
                }
                Err(e) => return Err(io_err(e)),
            }
        };
        let fail = |path: &Path, e: TmpError| {
            let _ = std::fs::remove_dir(path);
            e
        };
        let dir = File::open(&path).map_err(|e| fail(&path, io_err(e)))?;
        let m = dir.metadata().map_err(|e| fail(&path, io_err(e)))?;
        // 開いたディレクトリの所有者が mkdir 前に得た自分の uid でなければ（mkdir と open の間の差し替え）、
        // かつ親のパスが mkdir 前に確認した親と同じ実体でなければ拒否する。
        if m.uid() != own_uid || !m.is_dir() {
            return Err(fail(&path, TmpError::Replaced));
        }
        match std::fs::metadata(parent) {
            Ok(now) if now.dev() == pm.dev() && now.ino() == pm.ino() => {}
            _ => return Err(fail(&path, TmpError::Replaced)),
        }
        let proc_path = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        let access = if cfg!(target_os = "linux") && proc_path.is_dir() {
            proc_path
        } else {
            path.clone()
        };
        let this = Self {
            dir,
            parent: parent_fd,
            path,
            access,
            dev: m.dev(),
            ino: m.ino(),
            uid: m.uid(),
            armed: true,
        };
        // 想定の実体・権限・所有者でなければ（umask の異常や作成直後の差し替え）使わない。
        // 失敗時は `this` の drop が作業ディレクトリを消す。
        this.check()?;
        Ok(this)
    }

    /// 一時ファイルを作る・開くときに親として使うパス（Linux では保持 fd を指す）。
    pub fn access_path(&self) -> &Path {
        &self.access
    }

    /// 作業ディレクトリが、作成時に開いた fd と同じ実体・自分の所有・0700 の通常のディレクトリの
    /// ままで、親が共有になっていないことを確かめる（パスがリンクや別のディレクトリに差し替えられて
    /// いないこと）。
    pub fn check(&self) -> Result<(), TmpError> {
        let fd = self.dir.metadata().map_err(io_err)?;
        let by_path = std::fs::symlink_metadata(&self.path).map_err(|_| TmpError::Replaced)?;
        let parent = self.parent.metadata().map_err(io_err)?;
        if parent.mode() & 0o022 != 0 {
            return Err(TmpError::InsecureParent);
        }
        let same = by_path.is_dir()
            && by_path.dev() == self.dev
            && by_path.ino() == self.ino
            && by_path.uid() == self.uid
            && fd.is_dir()
            && fd.dev() == self.dev
            && fd.ino() == self.ino
            && fd.uid() == self.uid;
        if same && fd.mode() & 0o777 == 0o700 {
            Ok(())
        } else {
            Err(TmpError::Replaced)
        }
    }

    /// 作業ディレクトリ内のパス `p`（[`Self::access_path`] 配下。Linux では `/proc/self/fd/<n>/...`）を、
    /// 運用者が残骸を探せる実パス（親 + 作業ディレクトリ名 + ファイル名）へ直す。
    pub fn real_path_of(&self, p: &Path) -> PathBuf {
        match p.file_name() {
            Some(name) => self.path.join(name),
            None => self.path.clone(),
        }
    }

    /// 以降の drop で作業ディレクトリを消さない（差し替えを検出して別のディレクトリを消さないため。
    /// Issue #3102。呼び出し側が残ったパスを通知する）。
    pub fn disarm(&mut self) {
        self.armed = false;
    }

    /// 作業ディレクトリを消す（空でなければ消さず、残ったパスを返す）。
    pub fn remove(&mut self) -> Result<(), PathBuf> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        match std::fs::remove_dir(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(_) => Err(self.path.clone()),
        }
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_parent(label: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "dump-tmp-{label}-{}-{}",
            std::process::id(),
            TMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&p).expect("create parent");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        }
        p
    }

    #[test]
    fn work_dir_is_private_and_removed() {
        let parent = unique_parent("priv");
        let mut w = WorkDir::create(&parent, "t").expect("create");
        let mode = std::fs::metadata(&w.path).expect("meta").mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert!(w.check().is_ok());
        // 中に一時ファイルを作れて、消せば作業ディレクトリも消せる。
        let mut t = TempFile::create(w.access_path(), "f").expect("tmp");
        assert!(w.remove().is_err(), "non-empty dir must not be removed");
        w.armed = true;
        assert!(t.remove().is_ok());
        assert!(w.remove().is_ok());
        assert_eq!(std::fs::read_dir(&parent).expect("read").count(), 0);
        std::fs::remove_dir(&parent).expect("cleanup");
    }

    #[test]
    fn parent_acceptance_rules() {
        assert!(parent_is_acceptable(0o755, 1000, 1000));
        assert!(parent_is_acceptable(0o700, 1000, 1000));
        assert!(!parent_is_acceptable(0o775, 1000, 1000));
        assert!(!parent_is_acceptable(0o777, 1000, 1000));
        assert!(!parent_is_acceptable(0o1777, 1000, 1000));
        assert!(!parent_is_acceptable(0o757, 1000, 1000));
        // 所有者が異なる親（root 所有など）は、書き込めても拒否する。
        assert!(!parent_is_acceptable(0o755, 0, 1000));
        assert!(!parent_is_acceptable(0o700, 1000, 0));
    }

    #[test]
    fn probed_uid_matches_created_dir_owner() {
        let parent = unique_parent("uid");
        let mut n = 0u32;
        let f = File::open(&parent).expect("open");
        let uid = probe_own_uid(&parent, &f, &mut || {
            n += 1;
            format!(".p-{n}")
        })
        .expect("probe");
        let w = WorkDir::create(&parent, "t").expect("create");
        assert_eq!(w.uid, uid);
        assert_eq!(
            std::fs::read_dir(&parent).expect("read").count(),
            1,
            "probe removed"
        );
        drop(w);
        std::fs::remove_dir(&parent).expect("cleanup");
    }

    #[test]
    fn rejects_shared_parents() {
        use std::os::unix::fs::PermissionsExt;
        for mode in [0o777, 0o775, 0o757, 0o1777] {
            let parent = unique_parent("shared");
            std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(mode))
                .expect("chmod");
            assert_eq!(
                check_parent_not_shared(&parent),
                Err(TmpError::InsecureParent)
            );
            assert!(matches!(
                WorkDir::create(&parent, "t"),
                Err(TmpError::InsecureParent)
            ));
            assert_eq!(std::fs::read_dir(&parent).expect("read").count(), 0);
            std::fs::remove_dir(&parent).expect("cleanup");
        }
        let parent = unique_parent("ok");
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(check_parent_not_shared(&parent).is_ok());
        assert!(WorkDir::create(&parent, "t").is_ok());
        std::fs::remove_dir(&parent).expect("cleanup");
    }

    #[test]
    fn real_path_report_never_uses_proc() {
        let parent = unique_parent("real");
        let w = WorkDir::create(&parent, "t").expect("create");
        let t = TempFile::create(w.access_path(), "f").expect("tmp");
        let shown = w.real_path_of(&t.path);
        assert!(!shown.to_string_lossy().contains("/proc/self/fd"));
        assert_eq!(shown.parent(), Some(w.path.as_path()));
        assert_eq!(shown.file_name(), t.path.file_name());
        drop(t);
        drop(w);
        std::fs::remove_dir(&parent).expect("cleanup");
    }

    #[test]
    fn work_dir_detects_replacement_by_symlink() {
        let parent = unique_parent("swap");
        let w = WorkDir::create(&parent, "t").expect("create");
        let moved = parent.join("moved");
        std::fs::rename(&w.path, &moved).expect("rename away");
        let victim = parent.join("victim");
        std::fs::create_dir(&victim).expect("victim");
        std::os::unix::fs::symlink(&victim, &w.path).expect("symlink");
        assert_eq!(w.check(), Err(TmpError::Replaced));
        std::fs::remove_file(&w.path).expect("rm link");
        std::fs::rename(&moved, &w.path).expect("restore");
        assert!(w.check().is_ok());
        drop(w);
        std::fs::remove_dir(&victim).expect("cleanup victim");
        std::fs::remove_dir(&parent).expect("cleanup");
    }
}
