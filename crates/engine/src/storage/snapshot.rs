//! 稼働中バックアップ向けの一貫スナップショット書き出し（RECOVER-13・Issue #2151 のポインタ。
//! 方式の根拠は `docs/design/backup-snapshot.md`）。
//!
//! # 役割
//!
//! [`Storage`] が開いている redb から読み取りトランザクションを **1 本だけ** 取り（MVCC により
//! 開始時点の単一スナップショット。PERSIST-4）、全表（カタログ・行・索引・台帳・メタデータ）を
//! 別ファイルへ型付きで複製する。`writer_gate`（TABLE-3 の単一ライタ）は通さないため、
//! 書き出し中も書き込みは止まらない。
//!
//! # 呼び出し元と境界
//!
//! 呼び出し元は運用者の CLI（#2152）だけで、SQL・NoSQL の表層からは到達できない（`BACKUP` 文は
//! 設けない）。出力パスの検証（`..`・ディレクトリ指定の解釈）は CLI 側の責務で、本 API は
//! 受け取った 1 つのファイルパスにだけ書く。`--users` ファイル（redb の外）は対象外。
//!
//! # 失敗時の契約（fail-closed）
//!
//! - 出力先に何かが既にあれば書かずに拒否する（公開は `hard_link` で、既存の宛先を原子的に
//!   拒否する。`rename` は置換するため使わない）
//! - 一時ファイルは出力先の親に作る 0700 の作業ディレクトリ（`work_dir`。名前は
//!   `is_snapshot_work_dir_name`）の中へ書き、完成後に最終名へ公開する（Issue #3135）。
//!   一時ファイルは保持 fd 経由のパス（Linux は `/proc/self/fd/<n>`）で開き、公開の前後に作業
//!   ディレクトリと一時ファイルの同一性（dev・ino・長さ）を照合する。食い違えば何も公開せず
//!   `TempReplaced`、公開後なら自分が作ったリンクを取り消す。親が共有か他人所有なら何も作らず
//!   `InsecureParent`。公開前の失敗は Drop ガードが一時ファイルと（差し替えられていなければ）作業
//!   ディレクトリを消すため、最終名に不完全なファイルは残らない。`SIGKILL` では作業ディレクトリが
//!   残りうるが、最終名には残らず、次の `backup` が掃除する（`sweep_stale_temps_with_work_dirs`）
//! - 未知の型の表・multimap 表があれば中止する（黙って欠落させない）
//! - エラーの `Display` は固定の英語文言で、パス・表名・行の値を含めない

use crate::storage::change_log::CaptureWriteTxn as _;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use redb::{
    Key, ReadTransaction, ReadableTable, TableDefinition, TableError, Value, WriteTransaction,
};

use super::table_image::copy_by_shape;
use super::Storage;

/// 書き出し結果の要約（行の値は含めない。CLI の表示用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotSummary {
    /// 複製した表の数。
    pub tables: usize,
    /// 複製した行（エントリ）の総数。
    pub entries: u64,
}

/// [`Storage::export_snapshot`] の失敗。`StorageError` とは別の型にして既存の網羅的な
/// `match` を壊さない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotExportError {
    /// 出力先に既に何かがある（ファイル・ディレクトリ・シンボリックリンクを含む）。
    DestinationExists,
    /// 出力先のファイル名部分が無い、または親がディレクトリとして存在しない。
    InvalidDestination,
    /// 同じ `Storage` で別の書き出しが実行中。
    InProgress,
    /// 型を解決できない表、または multimap 表がある。
    UnsupportedTable,
    /// 一時表用の `Storage` には書き出せない。
    TemporaryStorage,
    /// 非 Unix、または hard link 非対応（`ErrorKind::Unsupported`）のファイルシステム。
    /// EPERM で hard link を禁止する環境は `Io(PermissionDenied)` として報告される。
    Unsupported,
    /// redb の失敗（詳細は表名を含みうるため保持しない）。
    Backend,
    /// ファイル操作の失敗。
    Io(std::io::ErrorKind),
    /// 最終名への公開は済んだ（中身は完全）が、一時名の削除または親ディレクトリの fsync に失敗した。
    PublishedButNotFinalized,
    /// 出力先の親ディレクトリにグループ・その他が書き込める（sticky bit があっても同じ）か、親の所有者が
    /// 自分と異なる。一時ファイルの差し替えを防げないので、何も作らずに拒否した（Issue #3135）。
    InsecureParent,
    /// 作業ディレクトリまたは一時ファイルが、書き出し中に差し替えられた・消された。何も公開していない
    /// （公開の直後に食い違いを見つけた場合は、自分が作ったリンクを取り消した）。
    TempReplaced,
    /// 公開先に、書いたものと異なる内容が残った（取り消しの削除にも失敗した）。手で消すこと。
    UncheckedContentPublished,
}

impl fmt::Display for SnapshotExportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::DestinationExists => "snapshot destination already exists",
            Self::InvalidDestination => "invalid snapshot destination",
            Self::InProgress => "a snapshot export is already in progress",
            Self::UnsupportedTable => "storage contains a table that cannot be exported",
            Self::TemporaryStorage => "temporary storage cannot be exported",
            Self::Unsupported => "snapshot export is not supported on this platform",
            Self::Backend => "storage backend error during snapshot export",
            Self::Io(_) => "i/o error during snapshot export",
            Self::PublishedButNotFinalized => {
                "snapshot published but cleanup of the temporary file failed"
            }
            Self::InsecureParent => "snapshot destination directory is shared or not owned by you",
            Self::TempReplaced => {
                "temporary snapshot was replaced or removed; nothing was published"
            }
            Self::UncheckedContentPublished => {
                "WARNING: the destination holds content that was not checked; delete it manually"
            }
        })
    }
}

impl std::error::Error for SnapshotExportError {}

/// 1 つの型の組で表を複製しようとした結果。
pub(super) enum CopyProbe {
    /// この型の組ではない。
    Mismatch,
    /// 複製できた。
    Copied,
}

/// `(K, V)` 型で `name` の表を `src` から `dst` へバイト列のまま複製する。
/// `table_image` の型の組の一覧から呼ばれる。走査はストリーミングで全件をメモリに溜めない。
pub(super) fn copy_table<K: Key + 'static, V: Value + 'static>(
    src: &ReadTransaction,
    dst: &WriteTransaction,
    name: &str,
    entries: &mut u64,
) -> Result<CopyProbe, SnapshotExportError> {
    let def = TableDefinition::<K, V>::new(name);
    let st = match src.open_table(def) {
        Ok(t) => t,
        Err(TableError::TableTypeMismatch { .. }) => return Ok(CopyProbe::Mismatch),
        Err(_) => return Err(SnapshotExportError::Backend),
    };
    // 空の表も再現するため、行が無くても出力側に表を作る。
    let mut dt = dst
        .open_table_logged(def)
        .map_err(|_| SnapshotExportError::Backend)?;
    let iter = st.iter().map_err(|_| SnapshotExportError::Backend)?;
    for r in iter {
        let (k, v) = r.map_err(|_| SnapshotExportError::Backend)?;
        let (kv, vv) = (k.value(), v.value());
        let kb = K::as_bytes(&kv);
        let vb = V::as_bytes(&vv);
        dt.insert(K::from_bytes(kb.as_ref()), V::from_bytes(vb.as_ref()))
            .map_err(|_| SnapshotExportError::Backend)?;
        *entries = entries.saturating_add(1);
    }
    Ok(CopyProbe::Copied)
}

/// 実行中フラグの解放ガード（panic・早期 return でも解放する）。
pub(super) struct BusyGuard<'a>(&'a AtomicBool);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// 実行中フラグを取る（取れなければ `None`）。増分の稼働中書き出し（Issue #3098）と共有する。
pub(super) fn try_acquire_busy(flag: &AtomicBool) -> Option<BusyGuard<'_>> {
    flag.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .ok()
        .map(|_| BusyGuard(flag))
}

/// [`Storage::export_snapshot`] の本体。
pub(super) fn export_snapshot(
    storage: &Storage,
    dest: &Path,
) -> Result<SnapshotSummary, SnapshotExportError> {
    if storage.temp_scoped {
        return Err(SnapshotExportError::TemporaryStorage);
    }
    #[cfg(not(unix))]
    {
        let _ = dest;
        Err(SnapshotExportError::Unsupported)
    }
    #[cfg(unix)]
    {
        unix_impl::export(storage, dest)
    }
}

#[cfg(unix)]
mod unix_impl {
    use std::fs::File;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::{copy_by_shape, BusyGuard, SnapshotExportError, SnapshotSummary};
    use crate::storage::stale_temp::make_temp_name;
    use crate::storage::work_dir::{WorkDir, WorkDirError};
    use crate::storage::{Storage, TempDirLock};
    use redb::{ReadableDatabase, TableHandle};

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    fn io_err(e: std::io::Error) -> SnapshotExportError {
        SnapshotExportError::Io(e.kind())
    }

    fn map_work(e: WorkDirError) -> SnapshotExportError {
        match e {
            WorkDirError::InsecureParent => SnapshotExportError::InsecureParent,
            WorkDirError::Replaced => SnapshotExportError::TempReplaced,
            WorkDirError::Invalid => SnapshotExportError::InvalidDestination,
            WorkDirError::Io(k) => SnapshotExportError::Io(k),
        }
    }

    /// 作業ディレクトリの中の一時ファイル。作成時に開いた fd を保持し、パスが同じ通常ファイルのままか
    /// を照合する。公開前の drop で一時ファイルを消す。
    struct TempFile {
        /// 作業ディレクトリのアクセス用パス（Linux では保持 fd 経由）の下のパス。
        path: PathBuf,
        /// 書き込みに使った fd と同じ inode を指す保持 fd（長さの照合用）。
        fd: File,
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
        /// `access`（作業ディレクトリのアクセス用パス）に 0600 の一時ファイルを排他的に作り、書き込み用の
        /// `File` と合わせて返す。一時名は最終名に依存させない（最終名が NAME_MAX 近くても収まる）。
        fn create(access: &Path) -> Result<(Self, File), SnapshotExportError> {
            let mut attempts = 0u32;
            loop {
                let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
                let p = access.join(make_temp_name("fandhe-snapshot", seq, "tmp"));
                match std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&p)
                {
                    Ok(file) => {
                        let m = file.metadata().map_err(io_err)?;
                        let fd = file.try_clone().map_err(io_err)?;
                        return Ok((
                            Self {
                                path: p,
                                fd,
                                dev: m.dev(),
                                ino: m.ino(),
                                armed: true,
                            },
                            file,
                        ));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < 8 => {
                        attempts += 1;
                    }
                    Err(e) => return Err(io_err(e)),
                }
            }
        }

        /// `path`（リンクをたどらない）が保持 fd と同じ (dev, ino)・同じ長さの通常ファイルか。
        fn is_same_file_at(&self, path: &Path) -> bool {
            let (Ok(m), Ok(fd)) = (std::fs::symlink_metadata(path), self.fd.metadata()) else {
                return false;
            };
            m.is_file() && m.dev() == self.dev && m.ino() == self.ino && m.len() == fd.len()
        }
    }

    /// 親ディレクトリがグループ・その他から書き込めないことを、開いた fd の `fstat` で確かめる。
    /// 何かを作る前に呼び、共有の親を最初に拒否する（所有者の照合は [`WorkDir::create`]）。
    fn check_parent_not_shared(parent: &Path) -> Result<(), SnapshotExportError> {
        let m = File::open(parent)
            .and_then(|f| f.metadata())
            .map_err(io_err)?;
        if m.is_dir() && m.mode() & 0o022 == 0 {
            Ok(())
        } else {
            Err(SnapshotExportError::InsecureParent)
        }
    }

    /// テスト専用の差し替え注入点（状態を変えない読み取りだけの hook。本番のビルドには含めない）。
    #[cfg(test)]
    #[allow(clippy::items_after_test_module)]
    pub(super) mod hook {
        use std::cell::RefCell;
        use std::path::Path;

        /// 引数は (実パスの作業ディレクトリ, 実パスの一時ファイル)。公開直前の照合の前に呼ばれる。
        pub(in crate::storage::snapshot) type Hook = Box<dyn Fn(&Path, &Path)>;

        thread_local! {
            pub(in crate::storage::snapshot) static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
        }
    }

    pub(super) fn export(
        storage: &Storage,
        dest: &Path,
    ) -> Result<SnapshotSummary, SnapshotExportError> {
        // 出力先の解釈（親ディレクトリは作らない）。
        dest.file_name()
            .ok_or(SnapshotExportError::InvalidDestination)?;
        let parent: &Path = match dest.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        match std::fs::metadata(parent) {
            Ok(m) if m.is_dir() => {}
            _ => return Err(SnapshotExportError::InvalidDestination),
        }
        // 共有の親は、何も作らずに拒否する。
        check_parent_not_shared(parent)?;
        // 早期失敗のための確認。最終判定は後段の hard_link（確認後の競合を残さない）。
        if std::fs::symlink_metadata(dest).is_ok() {
            return Err(SnapshotExportError::DestinationExists);
        }

        // 同時実行は 1 本まで。
        if storage
            .snapshot_export_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(SnapshotExportError::InProgress);
        }
        let _busy = BusyGuard(&storage.snapshot_export_busy);

        // 掃除（排他 lock）との相互排他。取れなければ何も作らずに中止する（fail-closed）。
        // drop の順序は宣言の逆（一時ファイル → 作業ディレクトリ → lock）。
        let _lock = TempDirLock::shared(parent).map_err(SnapshotExportError::Io)?;
        let mut work = WorkDir::create(parent, &mut || {
            make_temp_name(
                "fandhe-snapshot",
                TMP_SEQ.fetch_add(1, Ordering::Relaxed),
                "d",
            )
        })
        .map_err(map_work)?;
        let (mut tmp, file) = TempFile::create(work.access_path())?;

        let summary = write_all_tables(storage, file)?;

        // 公開の直前に、作業ディレクトリと一時ファイルが作成時のままであることを照合する。
        #[cfg(test)]
        hook::HOOK.with(|h| {
            if let Some(f) = h.borrow().as_ref() {
                f(work.real_path(), &work.real_path_of(&tmp.path));
            }
        });
        work.check().map_err(map_work)?;
        if !tmp.is_same_file_at(&tmp.path) {
            return Err(SnapshotExportError::TempReplaced);
        }

        // 公開。宛先が存在すれば原子的に失敗する（上書きしない）。
        match std::fs::hard_link(&tmp.path, dest) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(SnapshotExportError::DestinationExists);
            }
            Err(e) => {
                // `Unsupported` は ErrorKind::Unsupported（EOPNOTSUPP 等）に限る。EPERM は
                // 権限不足と hard link 禁止（一部のファイルシステムやカーネル設定が EPERM で
                // 返す）を区別できないため、`Io(PermissionDenied)` のまま返す。
                return Err(match e.kind() {
                    std::io::ErrorKind::Unsupported => SnapshotExportError::Unsupported,
                    _ => io_err(e),
                });
            }
        }
        // 公開先が書いたファイルそのものでなければ（作業ディレクトリの差し替え等）、自分が作った
        // リンクを取り消す。取り消せなければ警告用の種別で知らせる。
        if !tmp.is_same_file_at(dest) {
            return match std::fs::remove_file(dest) {
                Ok(()) => Err(SnapshotExportError::TempReplaced),
                Err(_) => Err(SnapshotExportError::UncheckedContentPublished),
            };
        }
        // 公開済みで書いたファイルと一致した。以降の失敗でも最終ファイルは完全なので消さない。
        tmp.armed = false;
        let finalized = std::fs::remove_file(&tmp.path)
            .map_err(|_| ())
            .and_then(|()| work.check().map_err(|_| ()))
            .and_then(|()| work.remove())
            .and_then(|()| {
                File::open(parent)
                    .and_then(|d| d.sync_all())
                    .map_err(|_| ())
            });
        if finalized.is_err() {
            work.disarm();
            return Err(SnapshotExportError::PublishedButNotFinalized);
        }
        Ok(summary)
    }

    /// 読み取り Tx を 1 本取り、全表を `file`（新規の空ファイル）上の redb へ複製する。
    fn write_all_tables(
        storage: &Storage,
        file: std::fs::File,
    ) -> Result<SnapshotSummary, SnapshotExportError> {
        // writer_gate・commit_log の登録簿は通さない（書き込みを止めない）。
        let src = storage
            .db
            .begin_read()
            .map_err(|_| SnapshotExportError::Backend)?;
        let out = redb::Builder::new()
            .create_file(file)
            .map_err(|_| SnapshotExportError::Backend)?;

        let multimaps = src
            .list_multimap_tables()
            .map_err(|_| SnapshotExportError::Backend)?;
        if multimaps.count() != 0 {
            return Err(SnapshotExportError::UnsupportedTable);
        }
        let names: Vec<String> = src
            .list_tables()
            .map_err(|_| SnapshotExportError::Backend)?
            .map(|h| h.name().to_string())
            .collect();

        let mut entries = 0u64;
        for name in &names {
            let mut dst = out
                .begin_write()
                .map_err(|_| SnapshotExportError::Backend)?;
            // 出力は公開前の未公開ファイルなので、途中の commit は fsync を省く。
            dst.set_durability(redb::Durability::None)
                .map_err(|_| SnapshotExportError::Backend)?;
            if !copy_by_shape(&src, &dst, name, &mut entries)? {
                return Err(SnapshotExportError::UnsupportedTable);
            }
            dst.commit().map_err(|_| SnapshotExportError::Backend)?;
        }
        // 最後に Immediate の空 commit を 1 回行い、それまでの内容を fsync で確定させてから
        // 公開する（案 D の「Immediate で commit → fsync」）。
        let fin = out
            .begin_write()
            .map_err(|_| SnapshotExportError::Backend)?;
        fin.commit().map_err(|_| SnapshotExportError::Backend)?;
        drop(out);
        drop(src);
        Ok(SnapshotSummary {
            tables: names.len(),
            entries,
        })
    }
}

#[cfg(test)]
mod tests {
    //! 書き出しの網羅・fail-closed・並行書き込みの検証（RECOVER-13 のポインタ）。

    use super::*;
    use crate::storage::{RowInput, Visibility};
    use crate::test_util::temp_db::TempDir;
    use redb::{Database, ReadableDatabase, ReadableTableMetadata, TableHandle};

    /// 値の redb 上のバイト列表現（比較用）。
    fn bytes_of<T: Value + 'static>(v: &T::SelfType<'_>) -> Vec<u8> {
        T::as_bytes(v).as_ref().to_vec()
    }

    /// 型の組ごとに 1 表・2 行を作る表と、複製後の内容比較を同じ一覧から生成する。
    /// 一覧の長さは `table_image` の型の組の件数と一致を検査し、型の追加漏れを検知する。
    macro_rules! shape_cases {
        ($(($name:literal, $k:ty, $v:ty, $k1:expr, $v1:expr, $k2:expr, $v2:expr)),+ $(,)?) => {
            const CASE_COUNT: usize = [$($name),+].len();

            fn populate(db: &Database) {
                let w = db.begin_write().unwrap();
                $({
                    let mut t = w.open_table(TableDefinition::<$k, $v>::new($name)).unwrap();
                    t.insert($k1, $v1).unwrap();
                    t.insert($k2, $v2).unwrap();
                })+
                w.commit().unwrap();
            }

            fn dump_all(db: &Database) -> Vec<(String, Vec<(Vec<u8>, Vec<u8>)>)> {
                let r = db.begin_read().unwrap();
                let mut out = Vec::new();
                $({
                    let t = r.open_table(TableDefinition::<$k, $v>::new($name)).unwrap();
                    let mut rows = Vec::new();
                    for e in t.iter().unwrap() {
                        let (k, v) = e.unwrap();
                        let (kv, vv) = (k.value(), v.value());
                        rows.push((
                            bytes_of::<$k>(&kv),
                            bytes_of::<$v>(&vv),
                        ));
                    }
                    out.push(($name.to_string(), rows));
                })+
                out
            }
        };
    }

    shape_cases![
        ("s00", &'static str, u64, "a", 1, "b", 2),
        ("s01", &'static str, &'static [u8], "a", &[1u8][..], "b", &[2u8, 3][..]),
        ("s02", (&'static str, &'static str), &'static [u8], ("a", "x"), &[1u8][..], ("a", "y"), &[][..]),
        ("s03", (&'static str, &'static str), u64, ("a", "x"), 1, ("b", "x"), 2),
        ("s04", (&'static str, &'static str, &'static str), &'static [u8], ("a", "b", "c"), &[1u8][..], ("a", "b", "d"), &[2u8][..]),
        ("s05", (&'static str, &'static str, &'static str), (), ("a", "b", "c"), (), ("a", "b", "d"), ()),
        ("s06", (&'static str, u64), &'static [u8], ("a", 1), &[1u8][..], ("a", 2), &[2u8][..]),
        ("s07", (&'static str, u64, u64), &'static [u8], ("a", 1, 2), &[1u8][..], ("a", 1, 3), &[2u8][..]),
        ("s08", (&'static str, &'static [u8]), &'static [u8], ("a", &[1u8][..]), &[1u8][..], ("a", &[2u8][..]), &[2u8][..]),
        ("s09", (&'static str, &'static [u8], u64), (), ("a", &[1u8][..], 1), (), ("a", &[1u8][..], 2), ()),
        ("s10", (&'static [u8], u64), &'static [u8], (&[1u8][..], 1), &[1u8][..], (&[2u8][..], 2), &[2u8][..]),
        ("s11", u64, u64, 1, 10, 2, 20),
        ("s12", u64, &'static [u8], 1, &[1u8][..], 2, &[2u8][..]),
        ("s13", (u8, &'static str, &'static str), &'static [u8], (1, "a", "b"), &[1u8][..], (2, "a", "b"), &[2u8][..]),
        ("s14", (u8, &'static str, &'static str, &'static str, &'static str), (), (1, "a", "b", "c", "d"), (), (2, "a", "b", "c", "d"), ()),
        ("s15", (&'static str, &'static str, &'static str, u8, &'static str), (), ("a", "b", "c", 1, "d"), (), ("a", "b", "c", 2, "d"), ()),
        ("s16", (&'static str, u64, u64, u8, u64), &'static [u8], ("a", 1, 2, 3, 4), &[1u8][..], ("a", 1, 2, 3, 5), &[2u8][..]),
        ("s17", (&'static str, u64, u64, u8), &'static [u8], ("a", 1, 2, 3), &[1u8][..], ("a", 1, 2, 4), &[2u8][..]),
        ("s18", (&'static str, u8, &'static str, &'static str, u64, u64), &'static [u8], ("a", 1, "b", "c", 2, 3), &[1u8][..], ("a", 1, "b", "c", 2, 4), &[2u8][..]),
        ("s19", (&'static str, u8, &'static str), &'static [u8], ("a", 1, "b"), &[1u8][..], ("a", 2, "b"), &[2u8][..]),
    ];

    /// 空表も再現されることを見るための、行が 0 件の表。
    const EMPTY_TABLE: TableDefinition<&str, u64> = TableDefinition::new("empty_table");

    fn row_input<'a>(tenant: &'a str, emb: &'a [f32], meta: &'a [u8]) -> RowInput<'a> {
        RowInput {
            tenant_id: tenant,
            visibility: Visibility::Public,
            embedding: emb,
            metadata: meta,
        }
    }

    fn open_storage(dir: &TempDir) -> Storage {
        let s = Storage::open(dir.db_path()).unwrap();
        populate(s.db());
        let w = s.db().begin_write().unwrap();
        w.open_table(EMPTY_TABLE).unwrap();
        w.commit().unwrap();
        s.put(1, &row_input("tenant-a", &[1.0, 2.0], b"m1"))
            .unwrap();
        s.put(2, &row_input("tenant-b", &[3.0, 4.0], b"m2"))
            .unwrap();
        s
    }

    fn table_names(db: &Database) -> Vec<String> {
        let r = db.begin_read().unwrap();
        let mut v: Vec<String> = r
            .list_tables()
            .unwrap()
            .map(|h| h.name().to_string())
            .collect();
        v.sort();
        v
    }

    fn dir_entries(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn shape_list_matches_table_image_shapes() {
        assert_eq!(CASE_COUNT, crate::storage::table_image::shape_count());
    }

    #[test]
    fn exported_file_opens_and_matches_every_table() {
        let dir = TempDir::new("snap-all");
        let storage = open_storage(&dir);
        let out = dir.path().join("backup.redb");
        let summary = storage.export_snapshot(&out).unwrap();

        // 元 DB が持つ全表（rows・世代・各型の表・空表）が複製対象に数えられる。
        let src_names = table_names(storage.db());
        assert_eq!(summary.tables, src_names.len());
        assert!(src_names.len() >= CASE_COUNT + 2);

        let copy = Storage::open(&out).unwrap();
        assert_eq!(table_names(copy.db()), src_names);
        assert_eq!(dump_all(copy.db()), dump_all(storage.db()));
        assert_eq!(copy.scan().unwrap().len(), 2);
        assert_eq!(copy.get("tenant-a", 1).unwrap().embedding, vec![1.0, 2.0]);
        let r = copy.db().begin_read().unwrap();
        assert_eq!(r.open_table(EMPTY_TABLE).unwrap().len().unwrap(), 0);
        drop(r);

        // 一時名は残らず、成果物は所有者のみ読み書き。
        assert_eq!(
            dir_entries(dir.path()),
            ["backup.redb", "db.redb"].map(String::from)
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&out).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn existing_destination_is_rejected_and_left_untouched() {
        let dir = TempDir::new("snap-exists");
        let storage = open_storage(&dir);

        let file = dir.path().join("exists.redb");
        std::fs::write(&file, b"keep").unwrap();
        assert_eq!(
            storage.export_snapshot(&file),
            Err(SnapshotExportError::DestinationExists)
        );
        assert_eq!(std::fs::read(&file).unwrap(), b"keep");

        let sub = dir.path().join("sub");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("x"), b"x").unwrap();
        assert_eq!(
            storage.export_snapshot(&sub),
            Err(SnapshotExportError::DestinationExists)
        );
        // 稼働中の DB 自身は上書きされない。
        assert_eq!(
            storage.export_snapshot(dir.db_path()),
            Err(SnapshotExportError::DestinationExists)
        );
        assert_eq!(storage.scan().unwrap().len(), 2);
        assert_eq!(
            dir_entries(dir.path()),
            ["db.redb", "exists.redb", "sub"].map(String::from)
        );
    }

    #[test]
    fn long_destination_name_does_not_overflow_temp_name() {
        let dir = TempDir::new("snap-long");
        let storage = open_storage(&dir);
        // NAME_MAX（255 バイト）ちょうどの最終名でも、一時名は最終名に依存しないため作れる。
        let out = dir.path().join("a".repeat(255));
        storage.export_snapshot(&out).unwrap();
        assert!(Storage::open(&out).is_ok());
        assert_eq!(dir_entries(dir.path()).len(), 2);
    }

    #[test]
    fn invalid_destination_is_rejected() {
        let dir = TempDir::new("snap-invalid");
        let storage = open_storage(&dir);
        let missing_parent = dir.path().join("no-such-dir").join("b.redb");
        assert_eq!(
            storage.export_snapshot(&missing_parent),
            Err(SnapshotExportError::InvalidDestination)
        );
        assert_eq!(
            storage.export_snapshot(std::path::Path::new("/")),
            Err(SnapshotExportError::InvalidDestination)
        );
    }

    #[test]
    fn unsupported_table_aborts_without_leaving_files() {
        let dir = TempDir::new("snap-unknown");
        let storage = open_storage(&dir);
        let w = storage.db().begin_write().unwrap();
        w.open_table(TableDefinition::<u8, u8>::new("unknown_probe"))
            .unwrap();
        w.commit().unwrap();

        let out = dir.path().join("backup.redb");
        assert_eq!(
            storage.export_snapshot(&out),
            Err(SnapshotExportError::UnsupportedTable)
        );
        assert_eq!(dir_entries(dir.path()), ["db.redb"].map(String::from));
        // 失敗後に同じ Storage で再実行できる（実行中フラグが解放されている）。
        assert_eq!(
            storage.export_snapshot(&out),
            Err(SnapshotExportError::UnsupportedTable)
        );
    }

    #[test]
    fn export_does_not_block_writers_and_sees_start_state() {
        let dir = TempDir::new("snap-concurrent");
        let storage = open_storage(&dir);
        // 書き込みゲートを握ったままでも書き出しは進み、握っている Tx の未 commit 分は含まない。
        let gated = storage.begin_write_txn().unwrap();
        let out = dir.path().join("backup.redb");
        storage.export_snapshot(&out).unwrap();
        drop(gated);
        // 書き出し後の書き込みは出力に現れない。
        storage
            .put(3, &row_input("tenant-a", &[5.0], b"m3"))
            .unwrap();
        let copy = Storage::open(&out).unwrap();
        assert_eq!(copy.scan().unwrap().len(), 2);
        assert_eq!(storage.scan().unwrap().len(), 3);
    }

    #[test]
    fn concurrent_export_is_rejected() {
        let dir = TempDir::new("snap-busy");
        let storage = open_storage(&dir);
        storage
            .snapshot_export_busy
            .store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(
            storage.export_snapshot(dir.path().join("b.redb")),
            Err(SnapshotExportError::InProgress)
        );
        assert_eq!(dir_entries(dir.path()), ["db.redb"].map(String::from));
    }

    #[test]
    fn temporary_storage_is_rejected() {
        let storage =
            Storage::open_with_backend(redb::backends::InMemoryBackend::new(), 1 << 20).unwrap();
        let dir = TempDir::new("snap-temp");
        assert_eq!(
            storage.export_snapshot(dir.path().join("b.redb")),
            Err(SnapshotExportError::TemporaryStorage)
        );
    }

    /// 親ディレクトリを `mode` の新しい一時ディレクトリにして、その中の `out.redb` を返す。
    fn dir_with_mode(label: &str, mode: u32) -> (TempDir, std::path::PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new(label);
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode)).unwrap();
        let out = dir.path().join("out.redb");
        (dir, out)
    }

    struct HookReset;
    impl Drop for HookReset {
        fn drop(&mut self) {
            unix_impl::hook::HOOK.with(|h| *h.borrow_mut() = None);
        }
    }
    fn set_hook(f: impl Fn(&std::path::Path, &std::path::Path) + 'static) -> HookReset {
        unix_impl::hook::HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
        HookReset
    }

    #[test]
    fn shared_parent_is_rejected_without_creating_anything() {
        // 別のディレクトリに DB を置き、出力先の親だけを共有にする。
        let src = TempDir::new("snap-shared-src");
        let storage = open_storage(&src);
        for mode in [0o777, 0o775, 0o757, 0o1777] {
            let (dir, out) = dir_with_mode("snap-shared", mode);
            assert_eq!(
                storage.export_snapshot(&out),
                Err(SnapshotExportError::InsecureParent),
                "{mode:o}"
            );
            assert!(dir_entries(dir.path()).is_empty(), "{mode:o}");
        }
        // 共有でなければ成功し、親に最終名以外が残らない。
        let (dir, out) = dir_with_mode("snap-private", 0o755);
        storage.export_snapshot(&out).unwrap();
        assert_eq!(dir_entries(dir.path()), ["out.redb"].map(String::from));
    }

    #[test]
    fn replaced_temp_file_is_never_published() {
        let src = TempDir::new("snap-swap-tmp-src");
        let storage = open_storage(&src);
        let (dir, out) = dir_with_mode("snap-swap-tmp", 0o700);
        let _reset = set_hook(|_work, tmp| {
            // 一時ファイルを別の inode に置き換える（名前の付け替え）。
            let other = tmp.with_extension("other");
            std::fs::write(&other, b"decoy").unwrap();
            std::fs::rename(&other, tmp).unwrap();
        });
        assert_eq!(
            storage.export_snapshot(&out),
            Err(SnapshotExportError::TempReplaced)
        );
        assert!(!out.exists());
        // 失敗後に一時ファイルも作業ディレクトリも残らない。
        assert!(dir_entries(dir.path()).is_empty());
    }

    #[test]
    fn replaced_work_dir_is_never_published_and_not_removed() {
        let src = TempDir::new("snap-swap-dir-src");
        let storage = open_storage(&src);
        let (dir, out) = dir_with_mode("snap-swap-dir", 0o700);
        let moved = dir.path().join("moved-away");
        let moved2 = moved.clone();
        let _reset = set_hook(move |work, _tmp| {
            // 作業ディレクトリを名前ごと別の場所へ移し、同名の空ディレクトリ（別の実体）に差し替える。
            use std::os::unix::fs::PermissionsExt;
            std::fs::rename(work, &moved2).unwrap();
            std::fs::create_dir(work).unwrap();
            std::fs::set_permissions(work, std::fs::Permissions::from_mode(0o700)).unwrap();
        });
        assert_eq!(
            storage.export_snapshot(&out),
            Err(SnapshotExportError::TempReplaced)
        );
        assert!(!out.exists());
        // 差し替え後の別のディレクトリは消さない（移した本物は一時ファイルごと残る）。
        let names = dir_entries(dir.path());
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(names
            .iter()
            .any(|n| crate::storage::is_snapshot_work_dir_name(n)));
        assert!(moved.is_dir());
    }

    #[test]
    fn success_and_failure_leave_no_work_dir() {
        let src = TempDir::new("snap-clean-src");
        let storage = open_storage(&src);
        let (dir, out) = dir_with_mode("snap-clean", 0o700);
        storage.export_snapshot(&out).unwrap();
        assert_eq!(dir_entries(dir.path()), ["out.redb"].map(String::from));
        // 既存の宛先で拒否された後も何も残らない。
        assert_eq!(
            storage.export_snapshot(&out),
            Err(SnapshotExportError::DestinationExists)
        );
        assert_eq!(dir_entries(dir.path()), ["out.redb"].map(String::from));
    }

    #[test]
    fn leftover_work_dir_is_swept_by_the_shared_cleanup() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, _out) = dir_with_mode("snap-sweep", 0o700);
        let work = dir
            .path()
            .join(crate::storage::make_temp_name("fandhe-snapshot", 7, "d"));
        let tmp = work.join(crate::storage::make_temp_name("fandhe-snapshot", 8, "tmp"));
        std::fs::create_dir(&work).unwrap();
        std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&tmp, b"x").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600)).unwrap();
        let sweep = crate::storage::sweep_stale_temps_with_work_dirs(
            dir.path(),
            crate::storage::is_snapshot_temp_name,
            crate::storage::is_snapshot_work_dir_name,
            &[],
        )
        .unwrap();
        assert_eq!(sweep.removed, 1);
        assert!(dir_entries(dir.path()).is_empty());
    }
}
