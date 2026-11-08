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

/// 復元（`restore`）の一時名 `.fandhe-restore-<pid>-<seq>.tmp`（旧形式）または
/// `.fandhe-restore-<pid>-<nonce 16桁小文字hex>-<seq>.tmp` の厳密な判定。wire-server の `restore_opt`
/// （一時名の生成・掃除）と [`is_reserved_temp_name`] が使う単一の真実源。名前は untrusted として
/// 添字を使わずに解析する。
pub fn is_restore_temp_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(".fandhe-restore-")
        .and_then(|r| r.strip_suffix(".tmp"))
    else {
        return false;
    };
    let mut parts = rest.split('-');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(pid), Some(seq), None, None) => is_decimal(pid, 10) && is_decimal(seq, 20),
        (Some(pid), Some(nonce), Some(seq), None) => {
            is_decimal(pid, 10) && is_lower_hex(nonce, 16) && is_decimal(seq, 20)
        }
        _ => false,
    }
}

/// 掃除の uid 確認用ファイルの名前の接頭辞。復元・アーカイブの一時名とは別の形式にして、
/// 呼び出し側の「一時名の残骸」の数え方と混ざらないようにする（掃除自身は [`is_probe_name`] で回収する）。
const PROBE_PREFIX: &str = ".fandhe-sweep-probe-";

/// 掃除の uid 確認用ファイルの名前 `.fandhe-sweep-probe-<pid>-<nonce 16桁hex>-<試行番号>.tmp` を作る。
#[cfg(unix)]
fn make_probe_name(attempt: u64) -> String {
    use std::hash::{BuildHasher, Hasher};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(nanos);
    h.write_u64(attempt);
    format!(
        "{PROBE_PREFIX}{}-{:016x}-{attempt}.tmp",
        std::process::id(),
        h.finish()
    )
}

/// `name` が掃除の uid 確認用ファイルの厳密な形式か。確認用ファイルは作成後すぐ消すが、その間に
/// 異常終了すると残るため、掃除は一時名と同じ安全条件で回収する。
fn is_probe_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(PROBE_PREFIX)
        .and_then(|r| r.strip_suffix(".tmp"))
    else {
        return false;
    };
    let mut parts = rest.split('-');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(pid), Some(nonce), Some(attempt), None) => {
            is_decimal(pid, 10) && is_lower_hex(nonce, 16) && is_decimal(attempt, 20)
        }
        _ => false,
    }
}

/// `name` が掃除の対象になりうる予約名（復元の一時名・アーカイブの一時名・掃除の確認用ファイル）の形式か。
/// 永続ファイル（`restore --db`・`backup --out` 等の出力先）がこの形式だと、同じ親ディレクトリでの
/// 掃除に完成済みのファイルが消されうるため、wire-server の出力先の検証がこの判定で拒否する（fail-closed）。
pub fn is_reserved_temp_name(name: &str) -> bool {
    is_restore_temp_name(name)
        || super::change_log::archive::is_segment_temp_name(name)
        || is_probe_name(name)
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

/// 掃除が消してはならないファイルの識別子（dev・inode）の集合を作る。`protected` の各パスについて、
/// 指定そのもの（リンクをたどらない）と実体（リンクをたどる）の両方を登録する。存在しないパスは
/// 登録されない（その名前が予約名なら呼び出し側の引数検査が拒否する）。
#[cfg(unix)]
fn protected_identities(protected: &[&Path]) -> Vec<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let mut ids = Vec::new();
    for p in protected {
        for meta in [std::fs::symlink_metadata(p), std::fs::metadata(p)]
            .into_iter()
            .flatten()
        {
            ids.push((meta.dev(), meta.ino()));
        }
    }
    ids
}

/// `dir` の残骸を掃除する。`is_temp_name` は一時名の形式の厳密な判定。自分の uid の確認用に
/// 作る probe は専用の形式（[`is_probe_name`]）で、`is_temp_name` と一致しないため呼び出し側の
/// 残骸の数え方に混ざり、異常終了で残っても次の掃除が回収する。
///
/// `protected` は消してはならない既存のファイル（起動中の DB・入力ファイル等）のパス。名前が一時名の形式でも、
/// 指定のパス・シンボリックリンクを解決した実体・同じ dev/inode のどれかに一致する候補は消さず `skipped` に数える
/// （予約名の検査をすり抜けた永続ファイルを守る多層防御。Issue #3094）。
#[cfg(unix)]
pub fn sweep_stale_temps(
    dir: &Path,
    is_temp_name: fn(&str) -> bool,
    protected: &[&Path],
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

    let protected_ids = protected_identities(protected);
    let protected_real: Vec<std::path::PathBuf> = protected
        .iter()
        .filter_map(|p| std::fs::canonicalize(p).ok())
        .collect();
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
        if !is_temp_name(name) && !is_probe_name(name) {
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
        let is_protected = protected_ids.contains(&(meta.dev(), meta.ino()))
            || std::fs::canonicalize(&path).is_ok_and(|c| protected_real.contains(&c));
        if is_protected {
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
    _protected: &[&Path],
) -> Result<StaleTempSweep, std::io::ErrorKind> {
    Ok(StaleTempSweep::empty(SweepOutcome::Unsupported))
}

#[cfg(all(test, unix))]
mod tests;
