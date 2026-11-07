//! `wire-server backup` サブコマンドの引数解釈と出力先パスの検証（Issue #2152・RECOVER-13 ポインタ。
//! 方式は `docs/design/backup-snapshot.md` の第 5〜8 節）。
//!
//! 責務境界: untrusted な CLI 文字列（`--db`・`--out`）を検証済みの [`BackupArgs`] に変換するまで。
//! 書き出し本体は `engine::storage::Storage::export_snapshot` で、`main.rs::run_backup` が呼ぶ。
//! 出力先に既に何かがある場合の原子的な拒否は engine 側（`hard_link` の `EEXIST`）が担い、
//! ここでの事前検査は利用者へ早く分かりやすく返すためのもので、競合の防止は engine に任せる。

use std::path::{Component, Path, PathBuf};

/// 終了コード: 書き出しの実行時失敗（DB を開けない・書き出し中の失敗など）。
pub const EXIT_FAILURE: u8 = 1;
/// 終了コード: 引数または出力先パスの検証で拒否した（DB は開いていない）。
pub const EXIT_USAGE: u8 = 2;
/// 終了コード: 最終名への公開は済み中身は完全だが、後始末（一時名の削除・親ディレクトリの
/// fsync）が確かめられなかった。バックアップは存在するので検証を促す（再実行は
/// 出力先の重複で拒否される）。
pub const EXIT_PUBLISHED_UNVERIFIED: u8 = 3;

/// 検証済みの `backup` サブコマンド引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupArgs {
    /// 停止中の DB ファイル（既存の通常ファイルであること）。
    pub db: PathBuf,
    /// 出力先（まだ存在しない新規パス）。
    pub out: PathBuf,
}

/// `backup` サブコマンドの引数（サブコマンド名より後ろ）を解釈する。
/// 未知の引数・値欠落・重複指定・必須欠落は拒否する（fail-closed。エラー文は英語で、パスの
/// 内容は含めない）。
pub fn parse(args: &[String]) -> Result<BackupArgs, String> {
    let mut db: Option<&str> = None;
    let mut out: Option<&str> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let slot = match arg.as_str() {
            "--db" => &mut db,
            "--out" => &mut out,
            _ => return Err("unknown argument (usage: backup --db <path> --out <path>)".into()),
        };
        if slot.is_some() {
            return Err(format!("{arg} specified more than once"));
        }
        match it.next() {
            Some(v) => *slot = Some(v.as_str()),
            None => return Err(format!("{arg} requires a value")),
        }
    }
    let (Some(db), Some(out)) = (db, out) else {
        return Err("backup requires --db and --out".into());
    };
    let db = validate_db(db)?;
    let out = validate_out(out)?;
    Ok(BackupArgs { db, out })
}

/// `--db` の検証。`Storage::open` は存在しないパスに空の DB を作るため、既存の通常ファイルに限る
/// （typo で空 DB が生成され「成功」するのを防ぐ）。
fn validate_db(raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("--db must not be empty".into());
    }
    let path = PathBuf::from(raw);
    match std::fs::metadata(&path) {
        Ok(m) if m.is_file() => Ok(path),
        _ => Err("--db must be an existing database file".into()),
    }
}

/// `--out` の検証。空・`..` 成分・ファイル名なし・既存の何か（シンボリックリンクを含む）・
/// 親ディレクトリの不在を拒否する。
fn validate_out(raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err("--out must not be empty".into());
    }
    let path = PathBuf::from(raw);
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("--out must not contain '..' components".into());
    }
    if path.file_name().is_none() {
        return Err("--out must name a file".into());
    }
    // 存在確認はリンクをたどらない（壊れたリンク経由の書き出しも拒否する）。
    if std::fs::symlink_metadata(&path).is_ok() {
        return Err("--out already exists; refusing to overwrite".into());
    }
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    match std::fs::metadata(parent) {
        Ok(m) if m.is_dir() => Ok(path),
        _ => Err("parent directory of --out does not exist".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rejects_malformed_arguments() {
        assert!(parse(&v(&[])).is_err());
        assert!(parse(&v(&["--db"])).is_err());
        assert!(parse(&v(&["--bogus", "x"])).is_err());
        assert!(parse(&v(&["--db", "a", "--db", "b", "--out", "c"])).is_err());
        assert!(parse(&v(&["--db", "", "--out", "x"])).is_err());
    }

    #[test]
    fn rejects_parent_dir_and_missing_parent() {
        let exe = std::env::current_exe().expect("current_exe");
        let db = exe.to_str().expect("utf-8 path");
        assert!(validate_out("../x").is_err());
        assert!(validate_out("a/../x").is_err());
        assert!(validate_out("/nonexistent-dir-for-backup-test/x").is_err());
        assert!(validate_out("/").is_err());
        assert!(parse(&v(&["--db", db, "--out", "../x"])).is_err());
    }

    #[test]
    fn rejects_existing_out_and_missing_db() {
        let exe = std::env::current_exe().expect("current_exe");
        let exe = exe.to_str().expect("utf-8 path");
        assert!(validate_out(exe).is_err());
        assert!(validate_db("/nonexistent-db-for-backup-test").is_err());
        assert!(validate_db("/").is_err());
    }
}
