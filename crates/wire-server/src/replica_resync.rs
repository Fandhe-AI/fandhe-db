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
//! 0. `--out` の親が共有（グループ・その他が書き込める）か他人所有なら、何も作らずに拒否する。そうでなければ
//!    親に 0700 の作業ディレクトリ（[`WorkDir`]。Issue #3135）を作り、以降の一時ベースへのアクセスは作業
//!    ディレクトリの保持 fd 経由のパスで行い、各段の前後で作業ディレクトリと一時ベースの同一性を照合する
//! 1. 作業ディレクトリの中の固定名 `base.redb` へ、バックアップソケット経由でフルバックアップを書かせる
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

use std::fs::File;
use std::net::TcpStream;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use engine::storage::{IncrementalError, IntegrityOptions};

use crate::dump_tmp::{self, TmpError, WorkDir};
use crate::replica::{be_u64, classify_error, read_frame, write_frame, FrameError};
use crate::replica_resync_opt::{
    ReplicaResyncOptions, EXIT_BASE_REJECTED, EXIT_FAILURE, EXIT_INCOMPLETE, EXIT_INCONSISTENT,
    EXIT_NOT_FINALIZED, EXIT_PRIMARY_REJECTED, EXIT_USAGE,
};
use crate::replication::{code, PROTOCOL_VERSION, T_ERROR, T_HELLO, T_IDENTITY, T_OK, T_START};
use crate::replication_conn::{self, ConnReader};
use crate::tls::client_handshake::perform_client_handshake;
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
    /// バックアップ側が「公開先に検査していない内容が残った」（結果 4）を返した。公開先は作業ディレクトリ内の
    /// 一時ベースで、整合検査には到達していない。`NotConsistent(4)`（整合検査の矛盾）と衝突させないため、
    /// 終了コード 1 の実行時エラーとして扱う（`--out` は作られない）。
    BackupTempUnchecked,
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
    /// `--out` の親ディレクトリにグループ・その他が書き込める（sticky bit があっても同じ）か、親の所有者が
    /// 自分と異なる。一時ベースの差し替えを防げないので、ソケットにもプライマリにも触れず何も作らずに拒否した
    /// （Issue #3135）。
    InsecureParent,
    /// 作業ディレクトリまたは一時ベースが検査後に差し替えられた。公開しなかった。
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
            Self::BackupTempUnchecked => {
                "the base backup could not be taken cleanly; nothing was published to --out, retry"
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
            Self::InsecureParent => {
                "the directory of --out is writable by group or others, or not owned by you; \
                 nothing was created"
            }
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
            Self::InvalidDestination | Self::InsecureParent => EXIT_USAGE,
            Self::NotFinalized => EXIT_NOT_FINALIZED,
            Self::Request
            | Self::BackupTempNotFinalized
            | Self::BackupTempUnchecked
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

/// 作業ディレクトリの中の一時ベースの固定名。サーバーへ依頼する出力先（実パス）の末尾で、
/// 親直下の予約名には含めない（サーバーは出力先を予約名で拒否するため。`engine::storage::is_replica_resync_base_name`）。
const BASE_NAME: &str = "base.redb";

/// `dump_tmp` の失敗を再同期の失敗へ対応づける（種別だけ。パスの内容は持ち越さない）。
fn map_tmp(e: TmpError) -> ResyncError {
    match e {
        TmpError::InsecureParent => ResyncError::InsecureParent,
        TmpError::Replaced => ResyncError::TempReplaced,
        TmpError::InvalidDestination => ResyncError::InvalidDestination,
        TmpError::UncheckedContentPublished => ResyncError::UncheckedContentPublished,
        TmpError::PublishedButNotFinalized => ResyncError::NotFinalized,
        TmpError::Unsupported | TmpError::Io(_) => ResyncError::Io,
    }
}

/// 一時ベースの保持。作業ディレクトリの中の `base.redb` を開いた fd（読み取り）と、その (dev, ino, 長さ) を
/// 覚え、以降のパスでの開き直し（検査・連続性の確認・`hard_link`）の前後でパスが同じ通常ファイルのままかを
/// 照合する（Issue #3135・RECOVER-13・RECOVER-15 のポインタ）。パスは作業ディレクトリの保持 fd 経由の
/// アクセス用パス（Linux は `/proc/self/fd/<n>/base.redb`）で、通知用に実パスを別に持つ。
struct TempBase {
    /// 保持 fd（長さの照合用。内容は読まない）。
    file: File,
    /// 作業ディレクトリのアクセス用パスの下の一時ベース。
    path: PathBuf,
    /// 運用者が残骸を探せる実パス（`/proc/self/fd` を見せない）。
    real: PathBuf,
    dev: u64,
    ino: u64,
    len: u64,
    /// 真の間は drop で一時ベースを消す（公開に成功したら偽にする）。
    armed: bool,
    /// 消せなかったときの通知用（実パス。呼び出し元の `leftover` へ移す）。
    leftover: Option<PathBuf>,
}

impl TempBase {
    /// 作業ディレクトリの `base.redb` が通常ファイル（シンボリックリンクでない）で長さ 1 以上であることを
    /// 確かめ、開いた fd が lstat した実体と同じであることを確かめて保持する。
    fn adopt(work: &WorkDir) -> Result<Self, ResyncError> {
        let path = work.access_path().join(BASE_NAME);
        let m = std::fs::symlink_metadata(&path).map_err(|_| ResyncError::Request)?;
        if !m.file_type().is_file() || m.len() == 0 {
            return Err(ResyncError::TempReplaced);
        }
        let file = File::open(&path).map_err(|_| ResyncError::Io)?;
        let fm = file.metadata().map_err(|_| ResyncError::Io)?;
        if !fm.is_file() || fm.dev() != m.dev() || fm.ino() != m.ino() || fm.len() != m.len() {
            return Err(ResyncError::TempReplaced);
        }
        Ok(Self {
            file,
            path,
            real: work.real_path_of(Path::new(BASE_NAME)),
            dev: fm.dev(),
            ino: fm.ino(),
            len: fm.len(),
            armed: true,
            leftover: None,
        })
    }

    /// `path`（リンクをたどらない）が、保持している一時ベースと同じ通常ファイルか。
    fn is_same_file_at(&self, path: &Path) -> bool {
        // 保持 fd が生きていること（長さは adopt 時の値で固定。検査中に伸びたら別物として扱う）。
        if !self.file.metadata().is_ok_and(|m| m.len() == self.len) {
            return false;
        }
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

    /// 一時ベースのパスが保持 fd のままであることを確かめる。
    fn check(&self) -> Result<(), ResyncError> {
        if self.is_same_file_at(&self.path) {
            Ok(())
        } else {
            Err(ResyncError::TempReplaced)
        }
    }

    fn remove(&mut self) {
        if self.armed {
            self.armed = false;
            if std::fs::remove_file(&self.path).is_err()
                && std::fs::symlink_metadata(&self.path).is_ok()
            {
                self.leftover = Some(self.real.clone());
            }
        }
    }
}

impl Drop for TempBase {
    fn drop(&mut self) {
        self.remove();
    }
}

/// 作業ディレクトリと一時ベースが作成時のままであることを確かめる。検査・確認接続・公開の各段の前後で
/// 呼ぶ（差し替えを見つけたら以降へ進まず、何も公開しない）。
fn check_staging(work: &WorkDir, tmp: &TempBase) -> Result<(), ResyncError> {
    work.check().map_err(map_tmp)?;
    tmp.check()
}

/// アクセス用パス配下のパスを、運用者が探せる実パスへ直す（`/proc/self/fd/<n>` を見せない）。
fn to_real(work: &WorkDir, p: &Path) -> PathBuf {
    match p.strip_prefix(work.access_path()) {
        Ok(rest) => work.real_path_of(Path::new("")).join(rest),
        Err(_) => p.to_path_buf(),
    }
}

/// テスト専用の差し替え注入点（状態を変えない読み取りだけの hook。本番のビルドには含めない）。
#[cfg(test)]
mod hook {
    use std::cell::RefCell;
    use std::path::Path;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Stage {
        /// 整合検査（`verify_db`）の直前。
        Verify,
        /// 連続性の確認（`replica_base_point_of`）の直前。
        BasePoint,
        /// 公開（`hard_link`）の直前（直前の照合の前）。
        Publish,
    }

    /// 引数は (段階, 実パスの作業ディレクトリ, 実パスの一時ベース)。
    pub(super) type Hook = Box<dyn Fn(Stage, &Path, &Path)>;

    thread_local! {
        pub(super) static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }
}

#[cfg(test)]
fn run_hook(stage: hook::Stage, work: &WorkDir, tmp: &TempBase) {
    hook::HOOK.with(|h| {
        if let Some(f) = h.borrow().as_ref() {
            f(stage, &work.real_path_of(Path::new("")), &tmp.real);
        }
    });
}

/// `--out` の親に残った、前回の異常終了の作業ディレクトリ（`.fandhe-replica-resync-*.d`。中身は `base.redb` と、
/// サーバーが残した入れ子のスナップショット一時ファイルだけ）を掃除する（Issue #3135）。`main.rs::run_replica_resync`
/// が再同期の前に呼ぶ。親直下の項目は何も消さず（旧形式の `.fandhe-replica-resync-*.redb` は予約名にできないため
/// 手動で消す前提）、使用中の作業ディレクトリは書き込み側の共有 lock により `InUse` で見送られる。宛先が既に在る・
/// 親が不正なときは何もしない。`protected` は消してはならない入力（`--credentials`・`--socket`）のパス。
pub fn sweep_stale_resync_temps(
    out: &Path,
    protected: &[&Path],
) -> Option<(PathBuf, engine::storage::StaleTempSweep)> {
    fn never(_: &str) -> bool {
        false
    }
    fn is_entry(name: &str) -> bool {
        engine::storage::is_replica_resync_base_name(name)
            || engine::storage::is_snapshot_temp_name(name)
    }
    let parent = dump_tmp::parent_of(out).ok()?;
    if std::fs::symlink_metadata(out).is_ok() {
        return None;
    }
    let s = engine::storage::sweep_stale_temps_with_work_dir_entries(
        parent,
        never,
        engine::storage::is_replica_resync_work_dir_name,
        is_entry,
        protected,
    )
    .ok()?;
    Some((parent.to_path_buf(), s))
}

/// 再同期を実行して、切り替え点の LSN を返す。失敗時は何も公開しない。一時ベースや作業ディレクトリを消せ
/// なかったときは、残った実パスを `leftover` に入れる（全テナントのデータが残るので、呼び出し元が通知する）。
///
/// 一時ベースは `--out` の親に作る 0700 の作業ディレクトリ（[`WorkDir`]。名前は
/// `engine::storage::is_replica_resync_work_dir_name`）の中に置く。プライマリのサーバーには作業ディレクトリ
/// の**実パス**を出力先として渡す（`/proc/self/fd/<n>` はこの CLI の fd なのでサーバーには渡せない）。
/// サーバーの `export_snapshot` は、この作業ディレクトリ（同じ uid の 0700）の中に自分のスナップショット用
/// 作業ディレクトリを入れ子に作って公開する。親が共有か他人所有なら、ソケットにもプライマリにも触れず、
/// 何も作らずに拒否する。
pub fn resync(
    opts: &ReplicaResyncOptions,
    leftover: &mut Vec<PathBuf>,
) -> Result<u64, ResyncError> {
    // 早期失敗のための確認。最終判定は公開の `hard_link`（確認後の競合を残さない）。
    if std::fs::symlink_metadata(&opts.out).is_ok() {
        return Err(ResyncError::InvalidDestination);
    }
    let parent = dump_tmp::parent_of(&opts.out).map_err(map_tmp)?;
    dump_tmp::check_parent_not_shared(parent).map_err(map_tmp)?;
    // 掃除（排他 lock）との相互排他。取れなければ何も作らずに中止する（fail-closed）。
    // drop の順序は宣言の逆（作業ディレクトリ → 親の共有 lock）。
    let _lock = engine::storage::TempDirLock::shared(parent).map_err(|_| ResyncError::Io)?;
    let mut work = WorkDir::create_named(parent, &mut || {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        engine::storage::make_temp_name(
            "fandhe-replica-resync",
            SEQ.fetch_add(1, Ordering::Relaxed),
            "d",
        )
    })
    .map_err(map_tmp)?;
    let outcome = run(opts, &mut work, leftover);
    match &outcome {
        // 公開の確定まで済んだ。作業ディレクトリは `publish` が消している。
        Ok(_) => {}
        // 公開済みで後始末だけ失敗した。残したものは `publish` が `leftover` に入れて disarm 済み。
        Err(ResyncError::NotFinalized) => {}
        Err(_) => cleanup_failed(&mut work, leftover),
    }
    leftover.extend(work.take_probe_leftover());
    outcome
}

/// 失敗後の後始末。一時ベース（アクセス用パス経由）と、サーバーが異常終了で残した入れ子のスナップショット用
/// 作業ディレクトリを消し、作業ディレクトリが作成時のままなら作業ディレクトリも消す。消せなかったもの
/// （差し替えを見つけて残した作業ディレクトリを含む）は実パスを `leftover` に入れる。
fn cleanup_failed(work: &mut WorkDir, leftover: &mut Vec<PathBuf>) {
    let base = work.access_path().join(BASE_NAME);
    if std::fs::symlink_metadata(&base).is_ok() && std::fs::remove_file(&base).is_err() {
        leftover.push(work.real_path_of(Path::new(BASE_NAME)));
    }
    // サーバーが書いている間は作業ディレクトリに共有 lock があり `InUse` で触れない。使用中でなく、
    // 形式・権限・所有者が想定どおりの残骸だけが消える。
    let _ = engine::storage::sweep_stale_temps_with_work_dirs(
        work.access_path(),
        engine::storage::is_snapshot_temp_name,
        engine::storage::is_snapshot_work_dir_name,
        &[],
    );
    match work.check() {
        Ok(()) => {
            if let Err(p) = work.remove() {
                leftover.push(p);
            }
        }
        Err(_) => {
            work.disarm();
            leftover.push(work.real_path_of(Path::new("")));
        }
    }
}

/// ベースの取得と、検査・公開。失敗時の後始末は呼び出し元（[`resync`]）の [`cleanup_failed`]。
fn run(
    opts: &ReplicaResyncOptions,
    work: &mut WorkDir,
    leftover: &mut Vec<PathBuf>,
) -> Result<u64, ResyncError> {
    // 1. ベースの取得（プライマリのサーバー側が作業ディレクトリの中の 0600 の一時名から原子的に公開する）。
    let real_base = work.real_path_of(Path::new(BASE_NAME));
    match backup_socket::request(&opts.socket, &real_base) {
        Ok(r) if r.code == 0 => {}
        // 結果 3 は一時名への公開のみ。`--out` は未作成なので、終了コード 3 として転送しない。
        Ok(r) if r.code == 3 => return Err(ResyncError::BackupTempNotFinalized),
        // 結果 4 も一時ベースの取得失敗。整合検査の矛盾（4）と混同しないよう実行時エラー（1）にする。
        Ok(r) if r.code == crate::backup_opt::EXIT_UNCHECKED_PUBLISHED => {
            return Err(ResyncError::BackupTempUnchecked);
        }
        Ok(r) => return Err(ResyncError::Backup(r.code)),
        Err(_) => return Err(ResyncError::Request),
    }
    let mut tmp = TempBase::adopt(work)?;
    let outcome = check_and_publish(opts, work, &mut tmp, leftover);
    tmp.remove();
    // 検査用コピーの残置と一時ベースの残置は別物なので、上書きせず両方を報告に残す。
    leftover.extend(tmp.leftover.take());
    outcome
}

/// 検査（整合・連続性・プライマリへの確認）と公開。各段の前後で作業ディレクトリと一時ベースの同一性を
/// 照合する。
fn check_and_publish(
    opts: &ReplicaResyncOptions,
    work: &mut WorkDir,
    tmp: &mut TempBase,
    leftover: &mut Vec<PathBuf>,
) -> Result<u64, ResyncError> {
    // 2. 整合検査。コピー経路に入ると作業ディレクトリの中に `.fandhe-verify-*.d` が入れ子で作られて消える
    //    （親は 0700 の自分の作業ディレクトリなので許容する）。
    check_staging(work, tmp)?;
    #[cfg(test)]
    run_hook(hook::Stage::Verify, work, tmp);
    let mut verify_leftover = None;
    let verified = restore_opt::verify_db(
        &tmp.path,
        &IntegrityOptions::default(),
        &mut verify_leftover,
    );
    leftover.extend(verify_leftover.map(|p| to_real(work, &p)));
    let after_verify = check_staging(work, tmp);
    // 差し替えの検出を検査の失敗より優先する（差し替えられたファイルの検査結果は信用しない）。
    after_verify?;
    let (report, _mode) = verified.map_err(|_| ResyncError::VerifyFailed)?;
    match verify_opt::classify(&report) {
        0 => {}
        c => return Err(ResyncError::NotConsistent(c)),
    }
    // 3. ベースの連続性と切り替え点。
    check_staging(work, tmp)?;
    #[cfg(test)]
    run_hook(hook::Stage::BasePoint, work, tmp);
    let point_result = engine::storage::replica_base_point_of(&tmp.path);
    check_staging(work, tmp)?;
    let point = point_result.map_err(|e| match e {
        IncrementalError::BaseUnreadable | IncrementalError::InUse => ResyncError::VerifyFailed,
        _ => ResyncError::BaseRejected,
    })?;
    // 4. プライマリへの確認接続。
    probe_primary(opts, &point)?;
    // 5. 公開。
    #[cfg(test)]
    run_hook(hook::Stage::Publish, work, tmp);
    publish(work, tmp, &opts.out, leftover)?;
    Ok(point.end_lsn)
}

/// 確認接続。Hello → Identity（ベースと照合）→ Start(切り替え点) → Ok を受けたら Ack せず閉じる。
fn probe_primary(
    opts: &ReplicaResyncOptions,
    point: &engine::storage::ChainPoint,
) -> Result<(), ResyncError> {
    let mut stream = TcpStream::connect_timeout(&opts.primary, CONNECT_TIMEOUT)
        .map_err(|_| ResyncError::PrimaryUnreachable)?;
    let _ = stream.set_nodelay(true);
    // ピンがあれば TLS を確立し、証明書の一致を確かめる（資格情報は一致の後にだけ送る。Issue #2177）。
    let (mut rd, mut wr) = match opts.primary_tls.as_deref() {
        None => replication_conn::plain(stream).map_err(|_| ResyncError::PrimaryUnreachable)?,
        Some(pin) => match perform_client_handshake(&mut stream, pin) {
            Ok(session) => replication_conn::tls(stream, session)
                .map_err(|_| ResyncError::PrimaryUnreachable)?,
            Err(e) if e.is_rejection() => {
                return Err(ResyncError::PrimaryRejected(
                    "the primary's TLS handshake was rejected (check --primary-tls-cert); nothing was published",
                ))
            }
            Err(_) => return Err(ResyncError::PrimaryUnreachable),
        },
    };
    wr.set_write_timeout(Some(WRITE_TIMEOUT))
        .map_err(|_| ResyncError::PrimaryUnreachable)?;
    let role_len = u8::try_from(opts.role.len()).map_err(|_| {
        ResyncError::PrimaryRejected("the primary rejected the connection; nothing was published")
    })?;
    write_frame(
        &mut wr,
        T_HELLO,
        &[
            &PROTOCOL_VERSION.to_be_bytes(),
            &[role_len],
            opts.role.as_bytes(),
            &opts.password,
        ],
    )
    .map_err(|_| ResyncError::PrimaryUnreachable)?;

    let identity = expect_frame(&mut rd, T_IDENTITY)?;
    let same = identity.get(..16) == Some(&point.db_id[..])
        && identity.get(16..24).and_then(be_u64) == Some(point.timeline);
    if !same {
        return Err(ResyncError::PrimaryRejected(
            "the primary belongs to another database; nothing was published",
        ));
    }
    write_frame(
        &mut wr,
        T_START,
        &[
            &point.db_id,
            &point.timeline.to_be_bytes(),
            &point.end_lsn.to_be_bytes(),
        ],
    )
    .map_err(|_| ResyncError::PrimaryUnreachable)?;
    expect_frame(&mut rd, T_OK)?;
    // Ack を送らず閉じる（保持位置の登録は切断で外れる）。
    Ok(())
}

/// ハンドシェイクのフレームを 1 つ読む。期待した型（本体 32 バイト）か `E`。それ以外はプロトコル違反。
fn expect_frame(stream: &mut ConnReader, expect: u8) -> Result<Vec<u8>, ResyncError> {
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

/// 一時ベースから `dest` へ `hard_link` で公開する。公開の前に作業ディレクトリと一時ベースを照合し、公開の
/// 直後に公開先が検査したファイルと別なら取り消す。成功したら一時ベースと作業ディレクトリを消して親ディレクトリ
/// を fsync する（失敗は `NotFinalized`。残したものの実パスを `leftover` に入れ、作業ディレクトリは disarm する）。
fn publish(
    work: &mut WorkDir,
    tmp: &mut TempBase,
    dest: &Path,
    leftover: &mut Vec<PathBuf>,
) -> Result<(), ResyncError> {
    check_staging(work, tmp)?;
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
        .map_err(|_| ())
        .and_then(|()| work.check().map_err(|_| ()))
        .and_then(|()| work.remove().map_err(|_| ()))
        .and_then(|()| {
            std::fs::File::open(parent)
                .and_then(|d| d.sync_all())
                .map_err(|_| ())
        });
    if finalized.is_err() {
        work.disarm();
        if std::fs::symlink_metadata(&tmp.path).is_ok() {
            leftover.push(tmp.real.clone());
        }
        let dir = work.real_path_of(Path::new(""));
        if std::fs::symlink_metadata(&dir).is_ok() {
            leftover.push(dir);
        }
        return Err(ResyncError::NotFinalized);
    }
    Ok(())
}

// 公開した場合の `backup_opt` 定数との整合（終了コードの表の固定）。
const _: () = assert!(EXIT_USAGE == backup_opt::EXIT_USAGE);

#[cfg(test)]
mod work_dir_tests {
    //! 作業ディレクトリ方式（Issue #3135）の単体テスト。プライマリへの接続を要する段（検査・確認接続）は
    //! CLI の結合テスト（`tests/wire_cli/replica_resync_cli.rs`）で確かめ、ここでは一時ベースの保持・
    //! 差し替えの検出・公開の取り消し・失敗後の後始末を確かめる。
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn parent(label: &str, mode: u32) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "fandhe-resync-wd-{label}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&d).expect("mkdir");
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(mode)).expect("chmod");
        d
    }

    fn new_work(parent: &Path) -> WorkDir {
        WorkDir::create_named(parent, &mut || {
            engine::storage::make_temp_name("fandhe-replica-resync", 1, "d")
        })
        .expect("work dir")
    }

    /// 作業ディレクトリに `base.redb` を置いて保持する。
    fn base_in(work: &WorkDir, body: &[u8]) -> TempBase {
        let p = work.access_path().join(BASE_NAME);
        std::fs::write(&p, body).expect("write base");
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        TempBase::adopt(work).expect("adopt")
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .expect("read")
            .map(|e| e.expect("e").file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn work_dir_name_is_a_recognized_work_dir_and_base_name_is_not_reserved() {
        let p = parent("names", 0o700);
        let w = new_work(&p);
        let n = names(&p);
        assert_eq!(n.len(), 1);
        assert!(engine::storage::is_replica_resync_work_dir_name(&n[0]));
        assert!(engine::storage::is_replica_resync_base_name(BASE_NAME));
        // サーバーの出力先検査（予約名）に当たらない。
        assert!(!engine::storage::is_reserved_temp_name(BASE_NAME));
        drop(w);
        std::fs::remove_dir(&p).expect("cleanup");
    }

    #[test]
    fn adopt_rejects_missing_empty_and_symlinked_base() {
        let p = parent("adopt", 0o700);
        let w = new_work(&p);
        assert!(matches!(TempBase::adopt(&w), Err(ResyncError::Request)));
        let base = w.access_path().join(BASE_NAME);
        std::fs::write(&base, b"").expect("write");
        assert!(matches!(
            TempBase::adopt(&w),
            Err(ResyncError::TempReplaced)
        ));
        std::fs::remove_file(&base).expect("rm");
        let other = p.join("other");
        std::fs::write(&other, b"x").expect("write");
        std::os::unix::fs::symlink(&other, &base).expect("symlink");
        assert!(matches!(
            TempBase::adopt(&w),
            Err(ResyncError::TempReplaced)
        ));
        std::fs::remove_file(&base).expect("rm link");
        std::fs::remove_file(&other).expect("rm other");
        drop(w);
        std::fs::remove_dir(&p).expect("cleanup");
    }

    #[test]
    fn publish_succeeds_and_leaves_only_the_destination() {
        let p = parent("ok", 0o700);
        let mut w = new_work(&p);
        let mut tmp = base_in(&w, b"base-bytes");
        let dest = p.join("replica.redb");
        let mut leftover = Vec::new();
        publish(&mut w, &mut tmp, &dest, &mut leftover).expect("publish");
        assert!(leftover.is_empty());
        assert_eq!(std::fs::read(&dest).expect("read"), b"base-bytes");
        assert_eq!(names(&p), vec!["replica.redb".to_string()]);
        std::fs::remove_file(&dest).expect("cleanup");
        std::fs::remove_dir(&p).expect("cleanup");
    }

    #[test]
    fn publish_refuses_a_replaced_base_and_publishes_nothing() {
        let p = parent("swap-base", 0o700);
        let mut w = new_work(&p);
        let mut tmp = base_in(&w, b"checked");
        // 検査したファイルを別の inode に置き換える（同じ長さ）。
        let base = w.access_path().join(BASE_NAME);
        let decoy = w.access_path().join("decoy");
        std::fs::write(&decoy, b"swapped").expect("write");
        std::fs::rename(&decoy, &base).expect("swap");
        let dest = p.join("replica.redb");
        let mut leftover = Vec::new();
        let r = publish(&mut w, &mut tmp, &dest, &mut leftover);
        assert!(matches!(r, Err(ResyncError::TempReplaced)));
        assert!(!dest.exists());
        cleanup_failed(&mut w, &mut leftover);
        drop(tmp);
        assert!(leftover.is_empty(), "{leftover:?}");
        assert!(names(&p).is_empty());
        std::fs::remove_dir(&p).expect("cleanup");
    }

    #[test]
    fn publish_refuses_a_renamed_work_dir_and_cleanup_keeps_the_foreign_dir() {
        let p = parent("swap-dir", 0o700);
        let mut w = new_work(&p);
        let mut tmp = base_in(&w, b"checked");
        let real = w.real_path_of(Path::new(""));
        let moved = p.join("moved-away");
        std::fs::rename(&real, &moved).expect("rename away");
        // 同名の別のディレクトリ（0700）に差し替える。
        std::fs::create_dir(&real).expect("decoy");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let dest = p.join("replica.redb");
        let mut leftover = Vec::new();
        let r = publish(&mut w, &mut tmp, &dest, &mut leftover);
        assert!(matches!(r, Err(ResyncError::TempReplaced)));
        assert!(!dest.exists());
        cleanup_failed(&mut w, &mut leftover);
        tmp.remove();
        // 差し替え後のディレクトリは消さず、実パスを通知する（`/proc/self/fd` は出さない）。
        assert!(real.is_dir());
        assert_eq!(leftover, vec![real.clone()]);
        assert!(leftover
            .iter()
            .all(|p| !p.to_string_lossy().contains("/proc/self/fd")));
        drop(tmp);
        drop(w);
        assert!(real.is_dir());
        std::fs::remove_dir(&real).expect("rm decoy");
        std::fs::remove_dir_all(&moved).expect("rm moved");
        std::fs::remove_dir(&p).expect("cleanup");
    }

    #[test]
    fn publish_never_overwrites_an_existing_destination() {
        let p = parent("exists", 0o700);
        let mut w = new_work(&p);
        let mut tmp = base_in(&w, b"checked");
        let dest = p.join("replica.redb");
        std::fs::write(&dest, b"keep").expect("write");
        let mut leftover = Vec::new();
        let r = publish(&mut w, &mut tmp, &dest, &mut leftover);
        assert!(matches!(r, Err(ResyncError::InvalidDestination)));
        assert_eq!(std::fs::read(&dest).expect("read"), b"keep");
        cleanup_failed(&mut w, &mut leftover);
        drop(tmp);
        assert!(leftover.is_empty());
        assert_eq!(names(&p), vec!["replica.redb".to_string()]);
        std::fs::remove_file(&dest).expect("cleanup");
        std::fs::remove_dir(&p).expect("cleanup");
    }

    #[test]
    fn cleanup_failed_removes_base_and_swept_nested_work_dir() {
        let p = parent("cleanup", 0o700);
        let mut w = new_work(&p);
        let _tmp = base_in(&w, b"x");
        // サーバーが異常終了で残した入れ子のスナップショット用作業ディレクトリ（0700・中は 0600 の一時名）。
        let nested =
            w.access_path()
                .join(engine::storage::make_temp_name("fandhe-snapshot", 3, "d"));
        std::fs::create_dir(&nested).expect("mkdir");
        std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let inner = nested.join(engine::storage::make_temp_name("fandhe-snapshot", 4, "tmp"));
        std::fs::write(&inner, b"y").expect("write");
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let mut leftover = Vec::new();
        cleanup_failed(&mut w, &mut leftover);
        assert!(leftover.is_empty(), "{leftover:?}");
        assert!(names(&p).is_empty());
        std::fs::remove_dir(&p).expect("cleanup");
    }

    #[test]
    fn shared_parent_is_rejected_before_anything_is_created() {
        for mode in [0o777, 0o775, 0o1777] {
            let p = parent("shared", mode);
            assert_eq!(
                dump_tmp::check_parent_not_shared(&p),
                Err(TmpError::InsecureParent)
            );
            assert!(matches!(
                map_tmp(TmpError::InsecureParent),
                ResyncError::InsecureParent
            ));
            assert!(names(&p).is_empty());
            std::fs::remove_dir(&p).expect("cleanup");
        }
        assert_eq!(ResyncError::InsecureParent.exit_code(), EXIT_USAGE);
        // 回帰: バックアップ結果 4 は整合検査矛盾（4）として転送しない。
        assert_eq!(ResyncError::BackupTempUnchecked.exit_code(), EXIT_FAILURE);
    }

    #[test]
    fn hook_stage_swaps_are_detected_by_the_check() {
        let p = parent("hook", 0o700);
        let w = new_work(&p);
        let tmp = base_in(&w, b"checked");
        assert!(check_staging(&w, &tmp).is_ok());
        // 作業ディレクトリは同じまま、一時ベースだけ置き換える。
        let base = w.access_path().join(BASE_NAME);
        let decoy = w.access_path().join("decoy");
        std::fs::write(&decoy, b"checked").expect("write");
        std::fs::rename(&decoy, &base).expect("swap");
        assert!(matches!(
            check_staging(&w, &tmp),
            Err(ResyncError::TempReplaced)
        ));
        drop(tmp);
        drop(w);
        let _ = std::fs::remove_dir_all(&p);
    }

    struct HookReset;
    impl Drop for HookReset {
        fn drop(&mut self) {
            hook::HOOK.with(|h| *h.borrow_mut() = None);
        }
    }

    fn opts_for(out: PathBuf) -> ReplicaResyncOptions {
        ReplicaResyncOptions {
            socket: PathBuf::from("/nonexistent/backup.sock"),
            out,
            primary: "127.0.0.1:1".parse().expect("addr"),
            primary_tls: None,
            role: "r".to_string(),
            password: Vec::new(),
        }
    }

    /// 空の redb を作業ディレクトリの `base.redb` へ置いて保持する（整合検査を通る正常なベースの代用）。
    fn valid_base_in(work: &WorkDir, scratch: &Path) -> TempBase {
        let src = scratch.join("src.redb");
        drop(engine::storage::Storage::open(&src).expect("open"));
        let body = std::fs::read(&src).expect("read");
        std::fs::remove_file(&src).expect("rm");
        base_in(work, &body)
    }

    /// 段階ごとに、hook が一時ベースを別の inode へ置き換えると、公開前に `TempReplaced` で止まる。
    #[test]
    fn swaps_at_each_stage_stop_before_publishing() {
        for stage in [hook::Stage::Verify, hook::Stage::BasePoint] {
            let p = parent("stages", 0o700);
            let scratch = parent("stages-src", 0o700);
            let mut w = new_work(&p);
            let mut tmp = valid_base_in(&w, &scratch);
            let body = std::fs::read(&tmp.path).expect("read");
            let opts = opts_for(p.join("o.redb"));
            hook::HOOK.with(|h| {
                *h.borrow_mut() = Some(Box::new(move |s, _work, base| {
                    if s == stage {
                        let other = base.with_extension("other");
                        std::fs::write(&other, &body).expect("write");
                        std::fs::rename(&other, base).expect("swap");
                    }
                }));
            });
            let _reset = HookReset;
            let mut leftover = Vec::new();
            let r = check_and_publish(&opts, &mut w, &mut tmp, &mut leftover);
            assert!(
                matches!(r, Err(ResyncError::TempReplaced)),
                "{stage:?}: {r:?}"
            );
            assert!(!opts.out.exists());
            cleanup_failed(&mut w, &mut leftover);
            drop(tmp);
            assert!(names(&p).is_empty(), "{stage:?}: {:?}", names(&p));
            std::fs::remove_dir(&p).expect("cleanup");
            let _ = std::fs::remove_dir_all(&scratch);
        }
    }

    #[test]
    fn to_real_maps_nested_paths_without_proc() {
        let p = parent("real", 0o700);
        let w = new_work(&p);
        let nested = w.access_path().join(".fandhe-verify-1-2-3.d").join("x");
        let shown = to_real(&w, &nested);
        assert!(!shown.to_string_lossy().contains("/proc/self/fd"));
        assert!(shown.starts_with(w.real_path_of(Path::new(""))));
        assert!(shown.ends_with(".fandhe-verify-1-2-3.d/x"));
        drop(w);
        std::fs::remove_dir(&p).expect("cleanup");
    }
}
