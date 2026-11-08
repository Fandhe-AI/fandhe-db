//! 異常終了（SIGKILL・電源断）で残った一時名の残骸を、次の実行で安全に掃除する共通部品
//! （Issue #3094・RECOVER-13・RECOVER-14 のポインタ）。
//!
//! 責務境界: 変更ログのアーカイブ（`change_log::archive`）と wire-server の `restore`
//! （`restore_opt`）は、公開前に一時名へ書く。一時名の中身は全テナントのデータのコピーまたは
//! セグメントなので、残骸を運用者が手で消す前提にしない。本モジュールは「名前の形式の判定」
//! を呼び出し側から受け取り、ディレクトリ単位で次の条件をすべて満たすものだけを消す。
//!
//! # fail-closed
//!
//! - 使用中の一時名を消さないため、書き込み側は一時名を作る前にディレクトリへ共有の advisory
//!   lock（[`TempDirLock`]）を取り、公開か削除までそれを保持する。書き込み側は共有 lock を取れなければ
//!   一時名を作らずエラーで中止する。掃除側は同じディレクトリに排他 lock を試し、取れなければ
//!   （書き込み中）全体を見送る。lock が使えない FS でも見送る
//! - 消す対象は「形式に厳密一致 × 通常ファイル（リンクをたどらない）× グループ・その他の権限なし
//!   × 自分の uid × 共有でないディレクトリ」の積だけ。1 つでも外れれば触らない
//! - 形式に一致しない項目は触らず、数えもしない。形式一致で型・所有者・権限が想定外のものは
//!   触らずに `skipped` へ数える
//! - 走査数・報告する名前数に上限を持つ（untrusted なディレクトリ向けの資源上限）
//!
//! 残るリスク: この lock を使わない古いバイナリと同時に動かした場合は使用中の判定ができない。
//! その場合も相手側の差し替え検出（`TempReplaced`）が公開を止める（不完全な内容は公開されない）。

use std::path::Path;

/// 走査する項目数の上限。
pub const MAX_SWEEP_ENTRIES: usize = 1_000_000;
/// 報告に載せる名前数の上限（件数は全数を数える）。
pub const MAX_REPORTED_NAMES: usize = 32;

/// 掃除の結果の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepOutcome {
    /// 走査した（0 件のこともある）。
    Swept,
    /// 他の書き込みが進行中（共有 lock が取られている）ため見送った。
    InUse,
    /// ディレクトリがグループ・その他に書き込めるため見送った。
    SharedDirectory,
    /// lock を取れない（lock 非対応の FS など）ため見送った。
    LockUnavailable,
    /// 自分の uid を確かめられなかったため見送った。
    OwnerUnknown,
    /// この OS では掃除しない。
    Unsupported,
}

/// 掃除の結果。名前は pid・nonce・通番だけでテナントのデータを含まない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleTempSweep {
    /// 結果の種別。
    pub outcome: SweepOutcome,
    /// 消した件数。
    pub removed: usize,
    /// 形式は一致したが型・所有者・権限が想定外のため残した件数。
    pub skipped: usize,
    /// 消そうとして失敗した件数。
    pub failed: usize,
    /// 消した名前（先頭から [`MAX_REPORTED_NAMES`] 件まで）。
    pub removed_names: Vec<String>,
    /// 走査数の上限で打ち切った。
    pub truncated: bool,
}

impl StaleTempSweep {
    #[cfg_attr(not(unix), allow(dead_code))]
    fn empty(outcome: SweepOutcome) -> Self {
        Self {
            outcome,
            removed: 0,
            skipped: 0,
            failed: 0,
            removed_names: Vec::new(),
            truncated: false,
        }
    }
}

/// `s` が 1〜`max_len` 桁の 10 進数だけか。
pub fn is_decimal(s: &str, max_len: usize) -> bool {
    !s.is_empty() && s.len() <= max_len && s.bytes().all(|b| b.is_ascii_digit())
}

/// `s` が `len` 桁の小文字 16 進数だけか。
pub fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// 一時名を作る書き込み側が親ディレクトリに持つ共有 lock。drop で解放する。
/// 掃除（排他 lock）との相互排他に使う。
#[derive(Debug)]
pub struct TempDirLock {
    _dir: std::fs::File,
}

impl TempDirLock {
    /// `dir` へ共有 lock を取る。取れない（lock 非対応の FS・資源不足など）ときはエラーを返す。
    /// 呼び出し側は一時名を作らずに中止すること（fail-closed。lock なしで書くと、並行する掃除が
    /// 排他 lock を取って使用中の一時名を消しうる）。
    pub fn shared(dir: &Path) -> Result<Self, std::io::ErrorKind> {
        let f = std::fs::File::open(dir).map_err(|e| e.kind())?;
        f.lock_shared().map_err(|e| e.kind())?;
        Ok(Self { _dir: f })
    }
}

/// `dir` の残骸を掃除する。`is_temp_name` は一時名の形式の厳密な判定、`make_probe_name` は
/// 形式に一致する新しい名前の生成（自分の uid の確認用の probe。引数は試行番号）。
#[cfg(unix)]
pub fn sweep_stale_temps(
    dir: &Path,
    is_temp_name: fn(&str) -> bool,
    make_probe_name: &dyn Fn(u64) -> String,
) -> Result<StaleTempSweep, std::io::ErrorKind> {
    use std::fs::TryLockError;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let dir_file = std::fs::File::open(dir).map_err(|e| e.kind())?;
    match dir_file.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => return Ok(StaleTempSweep::empty(SweepOutcome::InUse)),
        Err(TryLockError::Error(_)) => {
            return Ok(StaleTempSweep::empty(SweepOutcome::LockUnavailable))
        }
    }
    let dir_meta = dir_file.metadata().map_err(|e| e.kind())?;
    if dir_meta.mode() & 0o022 != 0 {
        return Ok(StaleTempSweep::empty(SweepOutcome::SharedDirectory));
    }

    // 自分の uid は geteuid ではなく「自分が今作ったファイルの所有者」で得る（libc に依存しない）。
    let mut own_uid = None;
    for attempt in 0..8u64 {
        let p = dir.join(make_probe_name(attempt));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&p)
        {
            Ok(f) => {
                own_uid = f.metadata().ok().map(|m| m.uid());
                drop(f);
                let _ = std::fs::remove_file(&p);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => break,
        }
    }
    let Some(own_uid) = own_uid else {
        return Ok(StaleTempSweep::empty(SweepOutcome::OwnerUnknown));
    };

    let mut sweep = StaleTempSweep::empty(SweepOutcome::Swept);
    let mut scanned = 0usize;
    let entries = std::fs::read_dir(dir).map_err(|e| e.kind())?;
    for entry in entries {
        scanned = scanned.saturating_add(1);
        if scanned > MAX_SWEEP_ENTRIES {
            sweep.truncated = true;
            break;
        }
        let Ok(entry) = entry else {
            sweep.failed = sweep.failed.saturating_add(1);
            continue;
        };
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        if !is_temp_name(name) {
            continue;
        }
        let path = dir.join(name);
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.mode() & 0o7077 != 0 || meta.uid() != own_uid {
            sweep.skipped = sweep.skipped.saturating_add(1);
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                sweep.removed = sweep.removed.saturating_add(1);
                if sweep.removed_names.len() < MAX_REPORTED_NAMES {
                    sweep.removed_names.push(name.to_string());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => sweep.failed = sweep.failed.saturating_add(1),
        }
    }
    if sweep.removed > 0 {
        let _ = dir_file.sync_all();
    }
    Ok(sweep)
}

/// 非 unix では何も消さない。
#[cfg(not(unix))]
pub fn sweep_stale_temps(
    _dir: &Path,
    _is_temp_name: fn(&str) -> bool,
    _make_probe_name: &dyn Fn(u64) -> String,
) -> Result<StaleTempSweep, std::io::ErrorKind> {
    Ok(StaleTempSweep::empty(SweepOutcome::Unsupported))
}

#[cfg(all(test, unix))]
mod tests;
