//! `wire-server backup-incremental` サブコマンドの引数解釈と結果の終了コード対応（Issue #2168・
//! RECOVER-13・RECOVER-14 のポインタ。設計は `docs/design/change-log-pitr.md` の D7）。
//!
//! 責務境界: untrusted な CLI 文字列（`--db`・`--base`・`--out`）を検証済みの [`IncrementalArgs`] に変換し、
//! engine の結果を (終了コード, 固定の英語文) へ対応づけるところまで。連鎖検証・書き出しは
//! `engine::storage::{chain_point_of, ReadOnlyStorage::export_incremental}`（`--change-log-archive-dir` 指定時は
//! `export_incremental_with_archive`。刈り込み済みの区間をアーカイブから補う）で、`main.rs::run_backup_incremental`
//! が薄く呼ぶ（停止中の DB を読み取り専用で開く。稼働中のサーバーからの取得は #2169 以降）。
//! 復元側（ベース＋増分の適用）は `restore --incremental`（[`crate::restore_opt`]）。
//!
//! 出力先は `backup --out` と同じ作法（既存の何かがあれば上書きせず拒否・`..` 拒否・親ディレクトリ必須）で、
//! 公開は engine が一時名 → fsync → `hard_link` で行う。他者が書けるディレクトリへは書かない
//! （#2167 のアーカイブ先と同じ検査を engine が行う）。

use std::path::PathBuf;

use engine::storage::IncrementalError;

use crate::backup_opt::{
    validate_db, validate_existing_file, validate_new_path, EXIT_FAILURE, EXIT_USAGE,
};

/// 検証済みの `backup-incremental` サブコマンド引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncrementalArgs {
    /// 出力元の DB ファイル（停止中。読み取り専用で開く）。
    pub db: PathBuf,
    /// 直前のベース（フルバックアップの DB ファイル、または前回の増分バックアップ。読むだけ）。
    pub base: PathBuf,
    /// 出力先（まだ存在しない新規パス）。
    pub out: PathBuf,
    /// 刈り込み済みの区間を補うアーカイブ先（変更ログのアーカイブ。既存のディレクトリ・絶対パス。
    /// Issue #3099・RECOVER-14）。指定がなければアーカイブを見ない。
    pub archive_dir: Option<PathBuf>,
}

/// `backup-incremental` サブコマンドの引数（サブコマンド名より後ろ）を解釈する。未知の引数・値欠落・
/// 重複指定・必須欠落は拒否する（fail-closed。エラー文は英語で、パスの内容は含めない）。
pub fn parse(args: &[String]) -> Result<IncrementalArgs, String> {
    const USAGE: &str = "unknown argument (usage: backup-incremental --db <path> --base <path> --out <path> [--change-log-archive-dir <dir>])";
    let mut db: Option<&str> = None;
    let mut base: Option<&str> = None;
    let mut out: Option<&str> = None;
    let mut archive: Option<&str> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let slot = match arg.as_str() {
            "--db" => &mut db,
            "--base" => &mut base,
            "--out" => &mut out,
            "--change-log-archive-dir" => &mut archive,
            _ => return Err(USAGE.into()),
        };
        if slot.is_some() {
            return Err(format!("{arg} specified more than once"));
        }
        match it.next() {
            Some(v) => *slot = Some(v.as_str()),
            None => return Err(format!("{arg} requires a value")),
        }
    }
    let (Some(db), Some(base), Some(out)) = (db, base, out) else {
        return Err("backup-incremental requires --db, --base and --out".into());
    };
    Ok(IncrementalArgs {
        db: validate_db(db)?,
        base: validate_existing_file("--base", base)?,
        out: validate_new_path("--out", out)?,
        archive_dir: archive
            .map(|d| crate::change_log_opt::accept_archive_dir(false, d))
            .transpose()?,
    })
}

/// 書き出しの結果を (終了コード, メッセージ) に対応づける。メッセージは固定の英語で、パス・表名・
/// 行の値・commit 番号を含まない（成功時はレコード数のみ）。引数・出力先の不備と環境の非対応は 2、
/// 連鎖の切断・別 DB・空の区間・入出力の失敗などの実行時の失敗は 1。
pub fn classify(
    result: &Result<engine::storage::IncrementalSummary, IncrementalError>,
) -> (u8, String) {
    match result {
        Ok(s) => (0, format!("records={}", s.records)),
        Err(
            e @ (IncrementalError::DestinationExists
            | IncrementalError::InvalidDestination
            | IncrementalError::InsecureDirectory
            | IncrementalError::Unsupported),
        ) => (EXIT_USAGE, e.to_string()),
        Err(e) => (EXIT_FAILURE, e.to_string()),
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
        assert!(parse(&v(&["--db", "a", "--db", "b", "--base", "c", "--out", "d"])).is_err());
        assert!(parse(&v(&["--db", "a", "--base", "b"])).is_err());
        assert!(parse(&v(&["--db", "a", "--out", "b"])).is_err());
    }

    #[test]
    fn archive_dir_is_optional_validated_and_not_repeatable() {
        let exe = std::env::current_exe().expect("current_exe");
        let exe = exe.to_str().expect("utf-8 path");
        let tmp = std::env::temp_dir();
        let tmp = tmp.to_str().expect("utf-8 path");
        let out = format!("{tmp}/incremental-opt-test-not-existing-out");
        let base = ["--db", exe, "--base", exe, "--out", out.as_str()];
        let with = |extra: &[&str]| {
            let mut a: Vec<&str> = base.to_vec();
            a.extend_from_slice(extra);
            parse(&v(&a))
        };
        assert_eq!(with(&[]).expect("no archive").archive_dir, None);
        let ok = with(&["--change-log-archive-dir", tmp]).expect("archive");
        assert!(ok.archive_dir.as_deref().is_some_and(|d| d.is_absolute()));
        // 値欠落・重複・存在しない・ディレクトリでない・`..` 成分は拒否する。
        assert!(with(&["--change-log-archive-dir"]).is_err());
        assert!(with(&[
            "--change-log-archive-dir",
            tmp,
            "--change-log-archive-dir",
            tmp
        ])
        .is_err());
        assert!(with(&["--change-log-archive-dir", "/nonexistent-archive-dir-test"]).is_err());
        assert!(with(&["--change-log-archive-dir", exe]).is_err());
        assert!(with(&["--change-log-archive-dir", "../x"]).is_err());
    }

    #[test]
    fn rejects_missing_inputs_and_existing_or_escaping_outputs() {
        let exe = std::env::current_exe().expect("current_exe");
        let exe = exe.to_str().expect("utf-8 path");
        // 出力先が既存（実行ファイル自身）
        assert!(parse(&v(&["--db", exe, "--base", exe, "--out", exe])).is_err());
        // 出力先に `..`
        assert!(parse(&v(&["--db", exe, "--base", exe, "--out", "../x"])).is_err());
        // ベースが無い
        assert!(parse(&v(&[
            "--db",
            exe,
            "--base",
            "/nonexistent-base-for-incremental-test",
            "--out",
            "/nonexistent-dir-for-incremental-test/x"
        ]))
        .is_err());
    }

    #[test]
    fn classify_separates_usage_from_runtime_failures() {
        let ok = Ok(engine::storage::IncrementalSummary {
            records: 3,
            first_lsn: 1,
            last_lsn: 3,
        });
        assert_eq!(classify(&ok), (0, "records=3".to_string()));
        for e in [
            IncrementalError::DestinationExists,
            IncrementalError::InvalidDestination,
            IncrementalError::InsecureDirectory,
        ] {
            assert_eq!(classify(&Err(e)).0, EXIT_USAGE);
        }
        for e in [
            IncrementalError::ChainBroken,
            IncrementalError::ForeignDatabase,
            IncrementalError::EmptyRange,
            IncrementalError::RangeUnavailable,
            IncrementalError::NotEnabled,
        ] {
            assert_eq!(classify(&Err(e)).0, EXIT_FAILURE);
        }
    }
}
