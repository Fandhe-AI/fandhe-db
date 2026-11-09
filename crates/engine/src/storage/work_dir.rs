//! スナップショット書き出し（`snapshot`）が一時ファイルを置く 0700 の作業ディレクトリ
//! （Issue #3135・RECOVER-13 のポインタ。方式は `docs/design/backup-snapshot.md`）。
//!
//! 責務境界: wire-server の `dump_tmp::WorkDir`（restore・verify が共有）と同じ方式を、engine から
//! 使える形で小さく持つ。engine から wire-server への依存は作れないため意図した重複である。
//! `snapshot::export` だけが使う（クレート内限定）。
//!
//! # 前提と多層防御
//!
//! 親ディレクトリにグループ・その他が書き込めない（sticky bit の有無によらず）、かつ親の所有者が自分
//! であること。この前提のもとでは親の名前空間を変えられるのは自分だけなので、作業ディレクトリの作成・
//! fd の取得・一時ファイルのパスでの open の間に他者が名前を差し替える窓は悪用できない。そのうえで、
//! 作成直後に開いた fd と実パスの (dev, ino)・所有者・権限を照合し（[`WorkDir::check`]）、Linux では
//! `/proc/self/fd/<n>` 経由で fd のディレクトリを指して以降の操作をパスの差し替えから切り離す。
#![cfg(unix)]

use std::fs::File;
use std::io::ErrorKind;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

/// 作業ディレクトリ操作の失敗（種別だけを持ち、パスの内容は含めない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkDirError {
    /// 親にグループ・その他が書き込める、または親の所有者が自分と異なる。何も作らない。
    InsecureParent,
    /// 作業ディレクトリが作成後に差し替えられた。
    Replaced,
    /// 名前が 1 成分でない等、不正な指定。
    Invalid,
    /// その他の I/O 失敗。
    Io(ErrorKind),
}

fn io_err(e: std::io::Error) -> WorkDirError {
    WorkDirError::Io(e.kind())
}

/// 親の権限と所有者が、作業ディレクトリを安全に作れる条件を満たすか（純関数）。
fn parent_is_acceptable(parent_mode: u32, parent_uid: u32, own_uid: u32) -> bool {
    parent_mode & 0o022 == 0 && parent_uid == own_uid
}

/// 保持 fd を指すアクセス用パス（Linux の `/proc/self/fd/<n>`）。使えなければ実パス。
fn access_path_of(fd: &File, real: &Path) -> PathBuf {
    use std::os::fd::AsRawFd;
    let proc_path = PathBuf::from(format!("/proc/self/fd/{}", fd.as_raw_fd()));
    if cfg!(target_os = "linux") && proc_path.is_dir() {
        proc_path
    } else {
        real.to_path_buf()
    }
}

/// 親が共有でなく自分の所有であることを検査したうえで作る、0700 の作業ディレクトリ。
/// `armed` の間は drop で（空なら）消す。
pub(crate) struct WorkDir {
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
    /// `parent` の中に 0700 の作業ディレクトリを新規に作る。`make_name` は衝突のたびに呼ばれ、親直下の
    /// 1 成分（パス区切りを含まない）を返すこと。親が共有か他人所有なら、何も作らず `InsecureParent`。
    pub(crate) fn create(
        parent: &Path,
        make_name: &mut dyn FnMut() -> String,
    ) -> Result<Self, WorkDirError> {
        let parent_fd = File::open(parent).map_err(io_err)?;
        let pm = parent_fd.metadata().map_err(io_err)?;
        if !pm.is_dir() || pm.mode() & 0o022 != 0 {
            return Err(WorkDirError::InsecureParent);
        }
        // 自分の uid は mkdir 前に親の中へ作った確認用ファイルの所有者から得る
        // （mkdir 後に開いたディレクトリの所有者は差し替えられうるので基準にしない）。
        let own_uid = super::stale_temp::probe_own_uid(&access_path_of(&parent_fd, parent))
            .ok_or(WorkDirError::Io(ErrorKind::Other))?;
        if !parent_is_acceptable(pm.mode(), pm.uid(), own_uid) {
            return Err(WorkDirError::InsecureParent);
        }
        let mut attempts = 0u32;
        let path = loop {
            let name = make_name();
            if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                return Err(WorkDirError::Invalid);
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
        let fail = |path: &Path, e: WorkDirError| {
            let _ = std::fs::remove_dir(path);
            e
        };
        let dir = File::open(&path).map_err(|e| fail(&path, io_err(e)))?;
        let m = dir.metadata().map_err(|e| fail(&path, io_err(e)))?;
        if m.uid() != own_uid || !m.is_dir() {
            return Err(fail(&path, WorkDirError::Replaced));
        }
        // 親のパスが mkdir 前に確認した親と同じ実体であること。
        match std::fs::metadata(parent) {
            Ok(now) if now.dev() == pm.dev() && now.ino() == pm.ino() => {}
            _ => return Err(fail(&path, WorkDirError::Replaced)),
        }
        let access = access_path_of(&dir, &path);
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
        // 想定の実体・権限・所有者でなければ使わない（失敗時は drop が消す）。
        this.check()?;
        Ok(this)
    }

    /// 一時ファイルを作る・開くときに親として使うパス（Linux では保持 fd を指す）。
    pub(crate) fn access_path(&self) -> &Path {
        &self.access
    }

    /// 作業ディレクトリが、作成時に開いた fd と同じ実体・自分の所有・0700 の通常のディレクトリのまま
    /// で、親が共有になっていないことを確かめる。
    pub(crate) fn check(&self) -> Result<(), WorkDirError> {
        let fd = self.dir.metadata().map_err(io_err)?;
        let by_path = std::fs::symlink_metadata(&self.path).map_err(|_| WorkDirError::Replaced)?;
        let parent = self.parent.metadata().map_err(io_err)?;
        if parent.mode() & 0o022 != 0 {
            return Err(WorkDirError::InsecureParent);
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
            Err(WorkDirError::Replaced)
        }
    }

    /// 作業ディレクトリ内のファイル名を、運用者が探せる実パスへ直す（`/proc/self/fd` を見せない）。
    #[cfg(test)]
    pub(crate) fn real_path_of(&self, name: &Path) -> PathBuf {
        match name.file_name() {
            Some(n) => self.path.join(n),
            None => self.path.clone(),
        }
    }

    /// 実パスの作業ディレクトリ。
    #[cfg(test)]
    pub(crate) fn real_path(&self) -> &Path {
        &self.path
    }

    /// 以降の drop で消さない（差し替えを見つけたとき、別のディレクトリを消さないため）。
    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }

    /// 作業ディレクトリを消す。空でなければ消さず `Err`。呼び出し側は先に [`Self::check`] で
    /// 差し替えられていないことを確かめること。
    pub(crate) fn remove(&mut self) -> Result<(), ()> {
        if !self.armed {
            return Ok(());
        }
        self.armed = false;
        match std::fs::remove_dir(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
            Err(_) => Err(()),
        }
    }
}

impl Drop for WorkDir {
    /// 差し替えを検出できた場合に限り、作業ディレクトリ（空のはず）を消す。
    fn drop(&mut self) {
        if self.armed && self.check().is_ok() {
            let _ = std::fs::remove_dir(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn parent_with(mode: u32) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "engine-work-dir-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&p).expect("mkdir");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).expect("chmod");
        p
    }

    fn name() -> String {
        super::super::stale_temp::make_temp_name(
            "fandhe-snapshot",
            SEQ.load(Ordering::Relaxed),
            "d",
        )
    }

    #[test]
    fn creates_private_dir_and_removes_it() {
        let parent = parent_with(0o700);
        let mut w = WorkDir::create(&parent, &mut name).expect("create");
        let mode = std::fs::metadata(w.real_path()).expect("meta").mode() & 0o777;
        assert_eq!(mode, 0o700);
        assert!(w.check().is_ok());
        assert!(w.remove().is_ok());
        assert_eq!(std::fs::read_dir(&parent).expect("read").count(), 0);
        std::fs::remove_dir(&parent).expect("cleanup");
    }

    #[test]
    fn rejects_shared_parents_without_creating_anything() {
        for mode in [0o777, 0o775, 0o757, 0o1777] {
            let parent = parent_with(mode);
            assert!(matches!(
                WorkDir::create(&parent, &mut name),
                Err(WorkDirError::InsecureParent)
            ));
            assert_eq!(std::fs::read_dir(&parent).expect("read").count(), 0);
            std::fs::remove_dir(&parent).expect("cleanup");
        }
    }

    #[test]
    fn acceptance_rules_cover_owner_and_mode() {
        assert!(parent_is_acceptable(0o755, 1000, 1000));
        assert!(!parent_is_acceptable(0o775, 1000, 1000));
        assert!(!parent_is_acceptable(0o1777, 1000, 1000));
        assert!(!parent_is_acceptable(0o755, 0, 1000));
    }

    #[test]
    fn rejects_names_that_are_not_one_component() {
        let parent = parent_with(0o700);
        for bad in ["a/b", "..", ".", ""] {
            let mut f = || bad.to_string();
            assert!(matches!(
                WorkDir::create(&parent, &mut f),
                Err(WorkDirError::Invalid)
            ));
        }
        std::fs::remove_dir(&parent).expect("cleanup");
    }

    #[test]
    fn detects_rename_and_symlink_replacement() {
        let parent = parent_with(0o700);
        let w = WorkDir::create(&parent, &mut name).expect("create");
        let real = w.real_path().to_path_buf();
        let moved = parent.join("moved");
        std::fs::rename(&real, &moved).expect("rename away");
        // 別のディレクトリへの置き換え。
        std::fs::create_dir(&real).expect("decoy");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        assert_eq!(w.check(), Err(WorkDirError::Replaced));
        std::fs::remove_dir(&real).expect("rm decoy");
        // シンボリックリンクへの置き換え。
        std::os::unix::fs::symlink(&moved, &real).expect("symlink");
        assert_eq!(w.check(), Err(WorkDirError::Replaced));
        std::fs::remove_file(&real).expect("rm link");
        std::fs::rename(&moved, &real).expect("restore");
        assert!(w.check().is_ok());
        drop(w);
        std::fs::remove_dir(&parent).expect("cleanup");
    }

    #[test]
    fn drop_does_not_remove_a_replaced_directory() {
        let parent = parent_with(0o700);
        let w = WorkDir::create(&parent, &mut name).expect("create");
        let real = w.real_path().to_path_buf();
        let moved = parent.join("moved");
        std::fs::rename(&real, &moved).expect("rename away");
        std::fs::create_dir(&real).expect("decoy");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        drop(w);
        // 差し替え後のディレクトリは消されない。
        assert!(real.is_dir());
        std::fs::remove_dir(&real).expect("rm decoy");
        std::fs::remove_dir(&moved).expect("rm moved");
        std::fs::remove_dir(&parent).expect("cleanup");
    }

    #[test]
    fn disarm_keeps_the_directory() {
        let parent = parent_with(0o700);
        let mut w = WorkDir::create(&parent, &mut name).expect("create");
        let real = w.real_path().to_path_buf();
        w.disarm();
        drop(w);
        assert!(real.is_dir());
        std::fs::remove_dir(&real).expect("rm");
        std::fs::remove_dir(&parent).expect("cleanup");
    }

    #[test]
    fn real_path_report_never_uses_proc() {
        let parent = parent_with(0o700);
        let w = WorkDir::create(&parent, &mut name).expect("create");
        let shown = w.real_path_of(&w.access_path().join("x.tmp"));
        assert!(!shown.to_string_lossy().contains("/proc/self/fd"));
        drop(w);
        std::fs::remove_dir(&parent).expect("cleanup");
    }
}
