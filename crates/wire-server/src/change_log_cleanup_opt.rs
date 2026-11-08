//! `change-log-cleanup` サブコマンドの引数解釈（Issue #3092・RECOVER-14 (7) のポインタ。設計は
//! `docs/design/change-log-pitr.md` の D6 の「Issue #3092 の実装判断」）。
//!
//! アーカイブ先に溜まった変更ログのセグメントのうち、保持しているフルバックアップからの PITR に要らない
//! ものだけを運用者が明示的に消す。本ファイルの責務は untrusted な CLI 文字列を検証済みの設定へ変換する
//! ところまで（削除の計画・実行は engine の `plan_archive_cleanup`／`execute_archive_cleanup`。
//! `main.rs` の `run_change_log_cleanup` が呼ぶ）。
//!
//! - 既定は dry-run（消す予定を出力するだけ）。`--execute` を付けたときだけ消す
//! - `--backup` は繰り返し指定する（保持するフルバックアップ。1 つも無ければ拒否する。fail-closed）
//! - `--keep-secs`・`--keep-segments` は削除を減らす方向にしか働かない
//!
//! エラー文は固定の英語で、パス・値を含めない。

use std::path::PathBuf;

use crate::change_log_opt::accept_archive_dir;

/// 終了コード: 失敗した（検証の失敗・途中で止まった場合を含む）。
pub const EXIT_FAILED: u8 = 1;
/// 終了コード: 引数の不備（何も開いていない）。
pub const EXIT_USAGE: u8 = 2;

const USAGE: &str = "usage: change-log-cleanup --archive-dir <dir> --backup <path>... \
[--db <path>] [--keep-secs <n>] [--keep-segments <n>] [--execute]";

/// 検証済みの `change-log-cleanup` 引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CleanupArgs {
    /// アーカイブ先（既存のディレクトリ）。
    pub dir: PathBuf,
    /// 保持するフルバックアップ（1 件以上）。
    pub backups: Vec<PathBuf>,
    /// 停止中の DB（任意。先頭行より前までしか消さない）。
    pub db: Option<PathBuf>,
    /// 最後のレコードがこの秒数より新しいセグメントを残す（任意）。
    pub keep_secs: Option<u64>,
    /// 削除候補の末尾から残すセグメント数（任意）。
    pub keep_segments: Option<u64>,
    /// 真なら実際に消す。偽（既定）は dry-run。
    pub execute: bool,
}

fn duplicate(flag: &str) -> String {
    format!("{flag} specified more than once")
}

fn parse_digits(flag: &str, raw: &str) -> Result<u64, String> {
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("{flag} must be a non-negative integer"));
    }
    raw.parse::<u64>()
        .map_err(|_| format!("{flag} is out of range"))
}

fn set_once<'a>(slot: &mut Option<&'a str>, flag: &str, value: &'a str) -> Result<(), String> {
    if slot.is_some() {
        return Err(duplicate(flag));
    }
    *slot = Some(value);
    Ok(())
}

/// サブコマンド名より後ろの引数を解釈する。未知の引数・値欠落・（`--backup` 以外の）重複・必須欠落・
/// 不正な数値・存在しないパスを拒否する。
pub fn parse(args: &[String]) -> Result<CleanupArgs, String> {
    let mut dir: Option<&str> = None;
    let mut db: Option<&str> = None;
    let mut backups: Vec<&str> = Vec::new();
    let mut keep_secs: Option<u64> = None;
    let mut keep_segments: Option<u64> = None;
    let mut execute = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let flag = arg.as_str();
        if flag == "--execute" {
            if execute {
                return Err(duplicate(flag));
            }
            execute = true;
            continue;
        }
        if !matches!(
            flag,
            "--archive-dir" | "--backup" | "--db" | "--keep-secs" | "--keep-segments"
        ) {
            return Err(format!("unknown argument ({USAGE})"));
        }
        let Some(value) = it.next() else {
            return Err(format!("{flag} requires a value"));
        };
        let value = value.as_str();
        match flag {
            "--backup" => backups.push(value),
            "--archive-dir" => set_once(&mut dir, flag, value)?,
            "--db" => set_once(&mut db, flag, value)?,
            "--keep-secs" => {
                if keep_secs.is_some() {
                    return Err(duplicate(flag));
                }
                keep_secs = Some(parse_digits(flag, value)?);
            }
            _ => {
                if keep_segments.is_some() {
                    return Err(duplicate(flag));
                }
                keep_segments = Some(parse_digits(flag, value)?);
            }
        }
    }
    let Some(dir) = dir else {
        return Err("change-log-cleanup requires --archive-dir".into());
    };
    if backups.is_empty() {
        return Err("change-log-cleanup requires at least one --backup".into());
    }
    let dir = accept_archive_dir(false, dir)
        .map_err(|m| m.replace(crate::change_log_opt::ARCHIVE_DIR_FLAG, "--archive-dir"))?;
    let backups = backups
        .into_iter()
        .map(|b| crate::backup_opt::validate_existing_file("--backup", b))
        .collect::<Result<Vec<_>, _>>()?;
    let db = db.map(crate::backup_opt::validate_db).transpose()?;
    Ok(CleanupArgs {
        dir,
        backups,
        db,
        keep_secs,
        keep_segments,
        execute,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| (*s).to_string()).collect()
    }

    fn tmp_dir() -> String {
        std::env::temp_dir().to_str().expect("utf8").to_string()
    }

    #[test]
    fn requires_a_backup_and_an_archive_dir() {
        let dir = tmp_dir();
        let exe = std::env::current_exe().expect("exe");
        let exe = exe.to_str().expect("utf8");
        assert!(parse(&v(&[])).is_err());
        assert!(parse(&v(&["--archive-dir", &dir])).is_err());
        assert!(parse(&v(&["--backup", exe])).is_err());
        let ok = parse(&v(&["--archive-dir", &dir, "--backup", exe])).expect("ok");
        assert!(!ok.execute, "dry-run by default");
        assert_eq!(ok.backups.len(), 1);
        assert_eq!(ok.keep_secs, None);
    }

    #[test]
    fn options_are_validated() {
        let dir = tmp_dir();
        let exe = std::env::current_exe().expect("exe");
        let exe = exe.to_str().expect("utf8");
        let with = |extra: &[&str]| {
            let mut a = vec!["--archive-dir", dir.as_str(), "--backup", exe];
            a.extend_from_slice(extra);
            parse(&v(&a))
        };
        let ok = with(&[
            "--backup",
            exe,
            "--db",
            exe,
            "--keep-secs",
            "60",
            "--keep-segments",
            "2",
            "--execute",
        ])
        .expect("ok");
        assert_eq!(ok.backups.len(), 2);
        assert_eq!(ok.keep_secs, Some(60));
        assert_eq!(ok.keep_segments, Some(2));
        assert!(ok.execute);
        assert!(with(&["--keep-secs", "-1"]).is_err());
        assert!(with(&["--keep-secs", ""]).is_err());
        assert!(with(&["--keep-secs", "1", "--keep-secs", "2"]).is_err());
        assert!(with(&["--keep-segments", "x"]).is_err());
        assert!(with(&["--execute", "--execute"]).is_err());
        assert!(with(&["--db", exe, "--db", exe]).is_err());
        assert!(with(&["--bogus"]).is_err());
        assert!(with(&["--db"]).is_err());
        assert!(with(&["--backup", "/nonexistent-backup-for-test"]).is_err());
    }

    #[test]
    fn error_messages_do_not_echo_values() {
        let dir = tmp_dir();
        let e = parse(&v(&[
            "--archive-dir",
            &dir,
            "--backup",
            "/nonexistent-secret-path",
        ]))
        .expect_err("rejected");
        assert!(!e.contains("secret"));
    }
}
