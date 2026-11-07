//! `wire-server verify` サブコマンドの引数解釈・報告の整形・終了コード対応
//! （Issue #2156・RECOVER-13 (6) のポインタ。親 #2154。検査本体は
//! `engine::storage::Storage::verify_integrity`）。
//!
//! 責務境界: untrusted な CLI 文字列を検証済みの [`VerifyArgs`] に変換し、engine が返す
//! [`IntegrityReport`] を「安定したテキスト行」と終了コードへ対応づけるまで。DB を開いて検査を
//! 呼ぶのは `main.rs::run_verify`、復元後の検査は [`crate::restore_opt`] が同じ [`classify`]・
//! [`render`] を使う（終了コードと出力形式の唯一の定義）。
//!
//! # 終了コード規約
//!
//! `backup` と共有する 0〜3 の意味は変えず、検査結果には重ならない 4・5 を割り当てる。
//!
//! | コード | 意味 |
//! | ---- | ---- |
//! | 0 | 整合（Warning だけを含む場合も 0。Warning は設計上、正しさに影響しない衛生指標・遅延再構築待ちで、非 0 にすると定期検査が常時失敗するため。報告には出る） |
//! | 1 | 実行時の失敗（DB を開けない・稼働中のサーバーが使用中・検査を始められない等） |
//! | 2 | 引数・パスの拒否（DB は開いていない） |
//! | 3 | `backup`・`restore` 専用（公開済みだが後始末を確かめられなかった）。`verify` は返さない |
//! | 4 | 矛盾あり（Error が 1 件以上。Incomplete が同時にあっても 4） |
//! | 5 | 検査できなかった項目だけがある（Incomplete のみ。整合とは報告しない） |
//!
//! # 出力形式
//!
//! 標準出力に `key=value` 形式の行を出す（空白区切りの語・値は英数字と `_./:-` のみに限る）。
//! 先頭は `status=consistent|inconsistent|incomplete`、続いて走査数と重大度別の総件数、
//! `findings_truncated=true|false`、(種類, 表) ごとの `finding code=<kind.code()> severity=<..>
//! table=<表の識別または -> count=<件数>` 行。行の値・キーの値・テナント名は報告に含まれない
//! （engine の報告がもともと持たない）。表の識別は念のため許可文字以外を `?` に置換して出す。

use std::fmt::Write as _;
use std::path::PathBuf;

use engine::storage::{IntegrityOptions, IntegrityReport, Severity};

use crate::backup_opt::validate_db;

/// 終了コード: 矛盾（Error）がある。
pub const EXIT_INCONSISTENT: u8 = 4;
/// 終了コード: 検査できなかった項目（Incomplete）だけがある。
pub const EXIT_INCOMPLETE: u8 = 5;

/// 検証済みの `verify` サブコマンド引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyArgs {
    /// 検査する停止中の DB ファイル（既存の空でない通常ファイル）。
    pub db: PathBuf,
    /// 検査の資源上限。
    pub options: IntegrityOptions,
}

/// `verify` サブコマンドの引数（サブコマンド名より後ろ）を解釈する。未知の引数・値欠落・重複・
/// 必須欠落は拒否する（fail-closed。エラー文は英語で、パスの内容は含めない）。
pub fn parse(args: &[String]) -> Result<VerifyArgs, String> {
    const USAGE: &str = "unknown argument (usage: verify --db <path> [--memory-budget-bytes <n>])";
    let mut db: Option<&str> = None;
    let mut budget: Option<&str> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let slot = match arg.as_str() {
            "--db" => &mut db,
            "--memory-budget-bytes" => &mut budget,
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
    let Some(db) = db else {
        return Err("verify requires --db".into());
    };
    let db = validate_db(db)?;
    let mut options = IntegrityOptions::default();
    if let Some(raw) = budget {
        options.memory_budget_bytes = raw
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| "--memory-budget-bytes must be a positive integer".to_string())?;
    }
    Ok(VerifyArgs { db, options })
}

/// 報告を終了コードへ対応づける（0・4・5。モジュール doc の表）。Error を Incomplete より優先する。
pub fn classify(report: &IntegrityReport) -> u8 {
    if report.total_by_severity(Severity::Error) > 0 {
        EXIT_INCONSISTENT
    } else if report.total_by_severity(Severity::Incomplete) > 0 {
        EXIT_INCOMPLETE
    } else {
        0
    }
}

fn severity_word(s: Severity) -> &'static str {
    match s {
        Severity::Error => "error",
        Severity::Warning => "warning",
        Severity::Incomplete => "incomplete",
        // `Severity` は `#[non_exhaustive]`。未知の重大度は整合と誤読されない語にする。
        _ => "unknown",
    }
}

/// 表の識別を出力用に無害化する（許可文字以外は `?`。制御文字・空白・改行で行構造を壊さない）。
fn sanitize(s: &str) -> String {
    s.chars()
        .take(256)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | ':' | '-') {
                c
            } else {
                '?'
            }
        })
        .collect()
}

/// 報告を安定したテキスト行へ整形する（モジュール doc の出力形式。末尾は改行）。
pub fn render(report: &IntegrityReport) -> String {
    let status = match classify(report) {
        0 => "consistent",
        EXIT_INCONSISTENT => "inconsistent",
        _ => "incomplete",
    };
    let mut out = String::new();
    // String への書き込みは失敗しない。
    let _ = writeln!(out, "status={status}");
    let _ = writeln!(
        out,
        "storage_tables_scanned={}",
        report.storage_tables_scanned
    );
    let _ = writeln!(
        out,
        "catalog_tables_checked={}",
        report.catalog_tables_checked
    );
    let _ = writeln!(out, "rows_scanned={}", report.rows_scanned);
    let _ = writeln!(
        out,
        "error_total={}",
        report.total_by_severity(Severity::Error)
    );
    let _ = writeln!(
        out,
        "warning_total={}",
        report.total_by_severity(Severity::Warning)
    );
    let _ = writeln!(
        out,
        "incomplete_total={}",
        report.total_by_severity(Severity::Incomplete)
    );
    let _ = writeln!(out, "findings_truncated={}", report.findings_truncated);
    for f in &report.findings {
        let table = f.table.as_deref().map_or_else(|| "-".to_string(), sanitize);
        let _ = writeln!(
            out,
            "finding code={} severity={} table={} count={}",
            f.kind.code(),
            severity_word(f.kind.severity()),
            table,
            f.count
        );
    }
    out
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
        assert!(parse(&v(&["--db", "a", "--db", "b"])).is_err());
        assert!(parse(&v(&["--db", ""])).is_err());
        assert!(parse(&v(&["--db", "/nonexistent-db-for-verify-test"])).is_err());
    }

    #[test]
    fn budget_must_be_a_positive_integer() {
        let exe = std::env::current_exe().expect("current_exe");
        let db = exe.to_str().expect("utf-8 path");
        for bad in ["0", "-1", "x", ""] {
            assert!(parse(&v(&["--db", db, "--memory-budget-bytes", bad])).is_err());
        }
        let ok = parse(&v(&["--db", db, "--memory-budget-bytes", "123"])).expect("valid");
        assert_eq!(ok.options.memory_budget_bytes, 123);
    }

    #[test]
    fn empty_report_is_consistent_and_renders_status_first() {
        let r = IntegrityReport::default();
        assert_eq!(classify(&r), 0);
        assert!(render(&r).starts_with("status=consistent\n"));
    }

    #[test]
    fn sanitize_replaces_disallowed_characters() {
        assert_eq!(sanitize("a b\nc=d"), "a?b?c?d");
        assert_eq!(sanitize("kids/x.y:z-1"), "kids/x.y:z-1");
    }
}
