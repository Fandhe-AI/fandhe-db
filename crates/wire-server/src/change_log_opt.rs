//! `--change-log` 系 CLI 引数の受理判定と起動時の固定文言（Issue #3067・#3069・#2167・親 #2166／#2164・
//! RECOVER-14 ポインタ。設計は `docs/design/change-log-pitr.md` の D8・D11・D6）。
//!
//! 永続変更ログ（増分バックアップ・PITR の土台）は opt-in の値なしスイッチ `--change-log` で有効にする
//! （`main.rs` の `open_engine_core_with_change_log` が engine の `Storage::enable_change_log` へ結線）。
//! 本ファイルの責務は untrusted な CLI 文字列を検証済みの設定に変換するところまで:
//!
//! - `--change-log`: 値なしスイッチ。値は読まず・echo しない。重複指定は last-wins にせず拒否する
//! - `--change-log-archive-dir <dir>`: アーカイブ先（既存のディレクトリ）。指定するとセグメントの書き出しを
//!   常駐スレッド（`change_log_archive`）が周期的に行う。`--change-log` なしは起動拒否
//! - `--change-log-retain-records <n>`: アーカイブ済みのレコードのうち新しい `n` 件を DB に残して、より古い
//!   ものを刈り込む。指定しなければ刈り込まない（既定）。`--change-log-archive-dir` なしは起動拒否
//! - `--change-log-archive-interval-secs <n>`: 書き出しの周期（秒）。`--change-log-archive-dir` なしは起動拒否
//! - `change-log-verify` サブコマンド: アーカイブの欠落・改ざんの検査（[`parse_verify`]）
//!
//! 重複指定・不正値・組み合わせの不備は起動拒否で、エラー文は固定の英語（パス・値を含まない）。
//! 一度有効にした DB をフラグなしで起動すると、レコードが欠けたまま世代が進む（LSN の欠落）ため、
//! 起動を [`REQUIRED_MESSAGE`] で拒否する（fail-closed。engine も同じ状態の書き込みを `54000` で拒否する）。

use std::path::{Component, PathBuf};
use std::time::Duration;

/// 受理対象の CLI フラグ名（値を取らないスイッチ）。
pub const FLAG: &str = "--change-log";

/// 2 回目以降の指定を拒否する固定文言（last-wins にしない）。
pub const DUPLICATE_MESSAGE: &str = "--change-log specified more than once";

/// 一度有効にした DB をフラグなしで起動したときに拒否する固定文言（LSN の欠落を作らない）。
pub const REQUIRED_MESSAGE: &str = "this database has a change log: restart with --change-log (writing without it would leave a gap in the log)";

/// 有効化に成功したときに stderr へ出す固定文言（運用者向けの状態表示）。
pub const ENABLED_MESSAGE: &str = "change log enabled (without --change-log-archive-dir the log is not archived and grows inside the database; point-in-time recovery is not available yet)";

/// フラグ 1 回分の受理判定。`already_set` は既に 1 回指定済みか。重複指定は [`DUPLICATE_MESSAGE`]。
/// （有効化できないビルドは無い。以前あった「未提供」の拒否文言と `AVAILABLE` は #3069 で削除した。）
pub fn accept(already_set: bool) -> Result<(), &'static str> {
    if already_set {
        return Err(DUPLICATE_MESSAGE);
    }
    Ok(())
}

/// アーカイブ先ディレクトリのフラグ名。
pub const ARCHIVE_DIR_FLAG: &str = "--change-log-archive-dir";
/// 保持件数のフラグ名。
pub const RETAIN_RECORDS_FLAG: &str = "--change-log-retain-records";
/// 書き出し周期（秒）のフラグ名。
pub const INTERVAL_FLAG: &str = "--change-log-archive-interval-secs";

/// 書き出し周期の既定（秒）。実装既定値で spec 由来ではない。
pub const DEFAULT_INTERVAL_SECS: u64 = 10;
/// 書き出し周期の上限（秒。1 日）。
pub const MAX_INTERVAL_SECS: u64 = 86_400;

/// アーカイブ系フラグを `--change-log` なしで指定したときの固定文言。
pub const ARCHIVE_NEEDS_CHANGE_LOG_MESSAGE: &str =
    "--change-log-archive-dir, --change-log-retain-records and --change-log-archive-interval-secs require --change-log";
/// 保持・周期を `--change-log-archive-dir` なしで指定したときの固定文言。
pub const NEEDS_ARCHIVE_DIR_MESSAGE: &str =
    "--change-log-retain-records and --change-log-archive-interval-secs require --change-log-archive-dir";

/// アーカイブ先が他者に書き込める（グループ・その他が書き込める。スティッキービットの有無に依らない）ときの固定文言。
pub const INSECURE_DIR_MESSAGE: &str =
    "--change-log-archive-dir must not be writable by group or other users";

/// 書き込み用のアーカイブ先として安全か（unix のみ。グループ・その他が書き込めるものは、一時名・セグメントの
/// 差し替えや同名ファイルの先置きができるため常に拒否する）。
#[cfg(unix)]
fn dir_is_private_enough(dir: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    // 存在しない・読めないパスはここでは判定しない（`accept_archive_dir` が拒否済み）。
    std::fs::metadata(dir).map_or(true, |m| {
        let mode = m.permissions().mode();
        mode & 0o022 == 0
    })
}

#[cfg(not(unix))]
fn dir_is_private_enough(_dir: &std::path::Path) -> bool {
    true
}

/// 検証済みのアーカイブ設定（`main.rs` が常駐スレッドへ渡す）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveSettings {
    /// アーカイブ先（既存のディレクトリ。絶対パスに直してある）。
    pub dir: PathBuf,
    /// 保持するアーカイブ済みレコードの件数。`None` は刈り込まない。
    pub retain_records: Option<u64>,
    /// 書き出しの周期。
    pub interval: Duration,
}

/// 同じフラグの重複指定を拒否する固定文言（フラグ名ごと）。
fn duplicate(flag: &str) -> String {
    format!("{flag} specified more than once")
}

/// `--change-log-archive-dir` の値を検証する。空・`..` 成分・既存のディレクトリでないものを拒否し、
/// サーバーの作業ディレクトリに依存しないよう絶対パスへ直す。
pub fn accept_archive_dir(already_set: bool, raw: &str) -> Result<PathBuf, String> {
    if already_set {
        return Err(duplicate(ARCHIVE_DIR_FLAG));
    }
    if raw.is_empty() {
        return Err(format!("{ARCHIVE_DIR_FLAG} must not be empty"));
    }
    let path = PathBuf::from(raw);
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!(
            "{ARCHIVE_DIR_FLAG} must not contain '..' components"
        ));
    }
    match std::fs::metadata(&path) {
        Ok(m) if m.is_dir() => {}
        _ => return Err(format!("{ARCHIVE_DIR_FLAG} must be an existing directory")),
    }
    std::path::absolute(&path)
        .map_err(|_| format!("{ARCHIVE_DIR_FLAG} cannot be resolved to an absolute path"))
}

/// 10 進の非負整数だけを受理する（符号・空白・指数・空文字を拒否する）。
fn parse_digits(flag: &str, raw: &str) -> Result<u64, String> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{flag} must be a non-negative integer"));
    }
    raw.parse::<u64>()
        .map_err(|_| format!("{flag} is out of range"))
}

/// `--change-log-retain-records` の値を検証する（0 は「アーカイブ済みのレコードをすべて刈り込む」）。
pub fn accept_retain_records(already_set: bool, raw: &str) -> Result<u64, String> {
    if already_set {
        return Err(duplicate(RETAIN_RECORDS_FLAG));
    }
    parse_digits(RETAIN_RECORDS_FLAG, raw)
}

/// `--change-log-archive-interval-secs` の値を検証する（1 以上 [`MAX_INTERVAL_SECS`] 以下）。
pub fn accept_interval_secs(already_set: bool, raw: &str) -> Result<u64, String> {
    if already_set {
        return Err(duplicate(INTERVAL_FLAG));
    }
    let n = parse_digits(INTERVAL_FLAG, raw)?;
    if !(1..=MAX_INTERVAL_SECS).contains(&n) {
        return Err(format!(
            "{INTERVAL_FLAG} must be between 1 and {MAX_INTERVAL_SECS}"
        ));
    }
    Ok(n)
}

/// フラグの組み合わせを解決する。アーカイブ系のフラグは `--change-log` が前提で、保持・周期は
/// アーカイブ先が前提（アーカイブされていない行を刈り込む設定を作らない。fail-closed）。
pub fn resolve(
    change_log: bool,
    dir: Option<PathBuf>,
    retain_records: Option<u64>,
    interval_secs: Option<u64>,
) -> Result<Option<ArchiveSettings>, &'static str> {
    let any = dir.is_some() || retain_records.is_some() || interval_secs.is_some();
    if any && !change_log {
        return Err(ARCHIVE_NEEDS_CHANGE_LOG_MESSAGE);
    }
    if dir.as_deref().is_some_and(|d| !dir_is_private_enough(d)) {
        return Err(INSECURE_DIR_MESSAGE);
    }
    let Some(dir) = dir else {
        return if any {
            Err(NEEDS_ARCHIVE_DIR_MESSAGE)
        } else {
            Ok(None)
        };
    };
    Ok(Some(ArchiveSettings {
        dir,
        retain_records,
        interval: Duration::from_secs(interval_secs.unwrap_or(DEFAULT_INTERVAL_SECS)),
    }))
}

/// 検証サブコマンドの終了コード: 検証に失敗した（欠落・改ざん・別 DB 等。または DB を開けない）。
pub const EXIT_VERIFY_FAILED: u8 = 1;
/// 検証サブコマンドの終了コード: 引数の不備（DB は開いていない）。
pub const EXIT_VERIFY_USAGE: u8 = 2;

/// 検証済みの `change-log-verify` サブコマンド引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyArgs {
    /// アーカイブ先（既存のディレクトリ）。
    pub dir: PathBuf,
    /// 突き合わせる停止中の DB（任意）。指定すると DB の識別子・timeline・有効化時の世代・水位と照合し、
    /// 先頭と末尾の欠落も検出する。省略するとディレクトリ内の整合（連鎖・チェックサム）だけを検査する。
    pub db: Option<PathBuf>,
}

/// `change-log-verify` サブコマンドの引数（サブコマンド名より後ろ）を解釈する。未知の引数・値欠落・
/// 重複・必須欠落は拒否する（エラー文は英語の固定文言で、パスを含まない）。
pub fn parse_verify(args: &[String]) -> Result<VerifyArgs, String> {
    let mut dir: Option<&str> = None;
    let mut db: Option<&str> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let slot =
            match arg.as_str() {
                "--archive-dir" => &mut dir,
                "--db" => &mut db,
                _ => return Err(
                    "unknown argument (usage: change-log-verify --archive-dir <dir> [--db <path>])"
                        .into(),
                ),
            };
        if slot.is_some() {
            return Err(duplicate(arg));
        }
        match it.next() {
            Some(v) => *slot = Some(v.as_str()),
            None => return Err(format!("{arg} requires a value")),
        }
    }
    let Some(dir) = dir else {
        return Err("change-log-verify requires --archive-dir".into());
    };
    let dir =
        accept_archive_dir(false, dir).map_err(|m| m.replace(ARCHIVE_DIR_FLAG, "--archive-dir"))?;
    let db = db.map(crate::backup_opt::validate_db).transpose()?;
    Ok(VerifyArgs { dir, db })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_is_a_bare_switch_name() {
        assert!(FLAG.starts_with("--"));
        assert!(!FLAG.contains('='));
        assert!(DUPLICATE_MESSAGE.starts_with(FLAG));
    }

    #[test]
    fn the_flag_is_accepted_once() {
        assert_eq!(accept(false), Ok(()));
        assert_eq!(accept(true), Err(DUPLICATE_MESSAGE));
    }

    #[test]
    fn startup_messages_carry_no_variable_content() {
        for m in [
            REQUIRED_MESSAGE,
            ENABLED_MESSAGE,
            ARCHIVE_NEEDS_CHANGE_LOG_MESSAGE,
            NEEDS_ARCHIVE_DIR_MESSAGE,
            INSECURE_DIR_MESSAGE,
        ] {
            assert!(!m.contains('{') && !m.contains('\n'));
        }
        assert!(REQUIRED_MESSAGE.contains(FLAG));
    }

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn tmp_dir() -> String {
        std::env::temp_dir().to_string_lossy().into_owned()
    }

    #[test]
    fn archive_dir_must_be_an_existing_directory_without_parent_components() {
        let dir = tmp_dir();
        let ok = accept_archive_dir(false, &dir).expect("existing dir");
        assert!(ok.is_absolute());
        assert!(accept_archive_dir(true, &dir).is_err(), "duplicate");
        assert!(accept_archive_dir(false, "").is_err());
        assert!(accept_archive_dir(false, "/nonexistent-dir-for-change-log-test").is_err());
        assert!(accept_archive_dir(false, &format!("{dir}/../x")).is_err());
        let exe = std::env::current_exe().expect("exe");
        assert!(
            accept_archive_dir(false, exe.to_str().expect("utf8")).is_err(),
            "a file is not a directory"
        );
    }

    #[test]
    fn numeric_flags_reject_malformed_and_out_of_range_values() {
        assert_eq!(accept_retain_records(false, "0"), Ok(0));
        assert_eq!(accept_retain_records(false, "12345"), Ok(12345));
        for bad in [
            "",
            "-1",
            "+1",
            " 1",
            "1 ",
            "1e3",
            "0x10",
            "abc",
            "99999999999999999999999",
        ] {
            assert!(accept_retain_records(false, bad).is_err(), "{bad:?}");
        }
        assert!(accept_retain_records(true, "1").is_err(), "duplicate");
        assert_eq!(accept_interval_secs(false, "1"), Ok(1));
        assert_eq!(accept_interval_secs(false, "86400"), Ok(86_400));
        for bad in ["0", "86401", "", "x", "-5"] {
            assert!(accept_interval_secs(false, bad).is_err(), "{bad:?}");
        }
        assert!(accept_interval_secs(true, "5").is_err(), "duplicate");
        // エラー文に入力値を含めない。
        let e = accept_retain_records(false, "secret-value").unwrap_err();
        assert!(!e.contains("secret-value"));
    }

    #[test]
    fn resolve_requires_change_log_and_archive_dir() {
        let dir = PathBuf::from("/archive");
        assert_eq!(resolve(false, None, None, None), Ok(None));
        assert_eq!(resolve(true, None, None, None), Ok(None));
        assert_eq!(
            resolve(false, Some(dir.clone()), None, None),
            Err(ARCHIVE_NEEDS_CHANGE_LOG_MESSAGE)
        );
        assert_eq!(
            resolve(false, None, Some(1), None),
            Err(ARCHIVE_NEEDS_CHANGE_LOG_MESSAGE)
        );
        // 保持はアーカイブ先なしでは指定できない（アーカイブされていない行を刈り込む設定を作らない）。
        assert_eq!(
            resolve(true, None, Some(1), None),
            Err(NEEDS_ARCHIVE_DIR_MESSAGE)
        );
        assert_eq!(
            resolve(true, None, None, Some(5)),
            Err(NEEDS_ARCHIVE_DIR_MESSAGE)
        );
        let s = resolve(true, Some(dir.clone()), None, None)
            .expect("ok")
            .expect("some");
        assert_eq!(s.retain_records, None, "retention is off by default");
        assert_eq!(s.interval, Duration::from_secs(DEFAULT_INTERVAL_SECS));
        let s = resolve(true, Some(dir), Some(7), Some(3))
            .expect("ok")
            .expect("some");
        assert_eq!(s.retain_records, Some(7));
        assert_eq!(s.interval, Duration::from_secs(3));
    }

    #[test]
    fn verify_arguments_are_validated() {
        let dir = tmp_dir();
        let exe = std::env::current_exe().expect("exe");
        let exe = exe.to_str().expect("utf8");
        assert!(parse_verify(&v(&[])).is_err());
        assert!(parse_verify(&v(&["--archive-dir"])).is_err());
        assert!(parse_verify(&v(&["--bogus", "x"])).is_err());
        assert!(parse_verify(&v(&["--archive-dir", &dir, "--archive-dir", &dir])).is_err());
        assert!(parse_verify(&v(&["--archive-dir", &dir, "--db", exe, "--db", exe])).is_err());
        assert!(parse_verify(&v(&[
            "--archive-dir",
            &dir,
            "--db",
            "/nonexistent-db-for-test"
        ]))
        .is_err());
        assert!(parse_verify(&v(&["--archive-dir", "/nonexistent-dir-for-test"])).is_err());
        let ok = parse_verify(&v(&["--archive-dir", &dir])).expect("dir only");
        assert!(ok.db.is_none());
        let ok = parse_verify(&v(&["--archive-dir", &dir, "--db", exe])).expect("with db");
        assert_eq!(ok.db.as_deref(), Some(std::path::Path::new(exe)));
    }

    #[cfg(unix)]
    #[test]
    fn a_shared_writable_archive_dir_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("fandhe-opt-shared-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).expect("chmod");
        assert_eq!(
            resolve(true, Some(dir.clone()), None, None),
            Err(INSECURE_DIR_MESSAGE)
        );
        // スティッキービットが付いていても拒否する。他者が書けないものは受理する。
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o1777)).expect("chmod");
        assert_eq!(
            resolve(true, Some(dir.clone()), None, None),
            Err(INSECURE_DIR_MESSAGE)
        );
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert!(resolve(true, Some(dir.clone()), None, None).is_ok());
        std::fs::remove_dir(&dir).expect("rmdir");
    }
}
