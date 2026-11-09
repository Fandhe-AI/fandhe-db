//! `wire-server replica-promote` サブコマンドの引数解釈・終了コード・出力文言（Issue #3132・親 #2171・
//! ポインタ: RECOVER-15 (4)(5)。方式の記録は `docs/design/replication-streaming.md` の
//! 「実装判断（Issue #3132）」、運用手順は `docs/design/replication-operations.md` §5）。
//!
//! 責務境界: untrusted な CLI 文字列を検証済みの [`ReplicaPromoteOptions`] に変換し、成功時の標準出力の
//! 文言を作るところまで。DB を開いて昇格するのは `main.rs::run_replica_promote`（engine の
//! `Storage::promote_replica`）で、DB に触れる前に [`parse`] を呼ぶ。
//!
//! - `--db <path>`: 昇格する停止中のレプリカ DB（既存の空でない通常ファイル。必須・重複不可）
//!
//! 未知のフラグ・値の欠落・重複・不正値は拒否で、エラー文は固定の英語（パスを含まない）。
//! 終了コードは `replica-resync`（[`crate::replica_resync_opt`]）と揃える。

use std::path::PathBuf;

use crate::backup_opt;
use crate::replica_opt;

/// 終了コード: 成功。
pub const EXIT_OK: u8 = 0;
/// 終了コード: 実行時の失敗（DB を開けない・別のプロセスが使用中・保守 commit の失敗）。
pub const EXIT_FAILURE: u8 = backup_opt::EXIT_FAILURE;
/// 終了コード: 引数・パスの拒否（DB は開いていない）。
pub const EXIT_USAGE: u8 = backup_opt::EXIT_USAGE;
/// 終了コード: 整合検査で矛盾が見つかり、昇格しなかった。
pub const EXIT_INCONSISTENT: u8 = crate::verify_opt::EXIT_INCONSISTENT;
/// 終了コード: 整合検査で検査できない項目だけが残り、昇格しなかった。
pub const EXIT_INCOMPLETE: u8 = crate::verify_opt::EXIT_INCOMPLETE;
/// 終了コード: レプリカのベースでない・変更ログが連続しない（`replica` の適用停止と同じ値）。
pub const EXIT_NOT_REPLICA_BASE: u8 = replica_opt::EXIT_APPLY_HALTED;

/// 終了コード: 昇格は確定したが、成功の報告（標準出力）を書けなかった。DB はすでに昇格済み（timeline は
/// 分岐済み）なので、再実行してはならない。終了コード 1（DB は変わらない）とは区別する。
pub const EXIT_PROMOTED_OUTPUT_FAILED: u8 = 6;

/// 検証済みの引数。
pub struct ReplicaPromoteOptions {
    /// 昇格する停止中のレプリカ DB。
    pub db: PathBuf,
}

impl std::fmt::Debug for ReplicaPromoteOptions {
    /// パスを含めない（誤って `{:?}` で出しても漏れない）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReplicaPromoteOptions")
            .finish_non_exhaustive()
    }
}

/// `replica-promote` の引数（サブコマンド名の後ろ）を検証する。
pub fn parse(args: &[String]) -> Result<ReplicaPromoteOptions, String> {
    let mut db: Option<&str> = None;
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        if flag != "--db" {
            return Err("unknown argument (usage: replica-promote --db <path>)".to_string());
        }
        let value = it
            .next()
            .ok_or_else(|| format!("{flag} requires a value"))?;
        if db.is_some() {
            return Err(format!("{flag} specified more than once"));
        }
        db = Some(value.as_str());
    }
    let db = db.ok_or("replica-promote requires --db")?;
    Ok(ReplicaPromoteOptions {
        db: backup_opt::validate_db(db)?,
    })
}

/// 昇格後に報告を書けなかったときの標準エラーの固定文言（適用済みの commit 番号を含める。再実行しない旨を明記）。
pub fn output_failed_message(applied_lsn: u64) -> String {
    format!(
        "replica-promote: the database WAS promoted (applied_lsn={applied_lsn}) but the report could not be \
         written to stdout; do not run replica-promote again (exit code {EXIT_PROMOTED_OUTPUT_FAILED})"
    )
}

/// 成功時の標準出力（運用者向け。テナントのセッションには見せない）。確定した適用済みの commit 番号と、
/// 旧プライマリで受け取っていない（失われる）commit の範囲を固定文言で知らせる。
pub fn success_report(applied_lsn: u64) -> String {
    format!(
        "replica-promote done: applied_lsn={applied_lsn}\n\
         replica-promote: commits after lsn={applied_lsn} on the former primary were not received and are lost\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn rejects_missing_duplicate_unknown_and_missing_file() {
        assert!(parse(&args(&[])).is_err());
        assert!(parse(&args(&["--db"])).is_err());
        assert!(parse(&args(&["--bogus", "x"])).is_err());
        assert!(parse(&args(&["--db", "/nonexistent/x.redb"])).is_err());
        let err = parse(&args(&["--db", "/a", "--db", "/b"])).expect_err("dup");
        assert!(!err.contains("/a"), "{err}");
    }

    #[test]
    fn accepts_an_existing_file_and_hides_the_path_in_debug() {
        let dir = std::env::temp_dir().join(format!("rp-opt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let f = dir.join("db.redb");
        std::fs::write(&f, b"x").expect("write");
        let o = parse(&args(&["--db", f.to_str().expect("utf8")])).expect("ok");
        assert_eq!(o.db, f);
        assert!(!format!("{o:?}").contains("rp-opt"));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn the_report_carries_the_lsn_twice_in_fixed_wording() {
        let r = success_report(42);
        assert_eq!(r.lines().count(), 2);
        assert!(r.contains("applied_lsn=42"));
        assert!(r.contains("after lsn=42"));
    }
}
