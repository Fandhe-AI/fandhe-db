//! `wire-server dump`／`dump-restore` サブコマンドの引数解釈と終了コードの定義
//! （Issue #2162・RECOVER-16 ポインタ。方式は `docs/design/logical-dump.md` の第 12 節）。
//!
//! 責務境界: untrusted な CLI 文字列を検証済みの [`DumpArgs`]・[`DumpRestoreArgs`] に変換するところまで。
//! ダンプの書き出しは [`crate::dump_run`]、ファイル形式は [`crate::dump_file`]、復元は
//! [`crate::dump_restore`] が担い、`main.rs` の `run_dump`・`run_dump_restore` がそれらを呼ぶ。
//! DB を開く前に引数の不正（排他違反・値の欠落・テナント名の不正・出力先の既存）を拒否する
//! （`backup_opt`・`restore_opt` と同じ流儀）。エラー文は固定の英語で、パスの内容は含めない。
//!
//! テナント境界: `--tenant` は engine の `SchemaDdlScope::shared_and_tenant` に、`--shared-only` は
//! `SchemaDdlScope::shared_only` にそのまま写る。どちらも指定しない呼び出し（＝全テナント）は
//! 受理しない（テナントの一覧を得る API が無いことと、暗黙に全テナントを書き出さないため）。

use std::path::PathBuf;

use crate::backup_opt::{validate_existing_file, validate_new_path};

/// 終了コード: 実行時の失敗（DB を開けない・書き出しや適用の途中の失敗）。
pub const EXIT_FAILURE: u8 = 1;
/// 終了コード: 引数またはパスの検証で拒否した（DB は開いていない）。
pub const EXIT_USAGE: u8 = 2;
/// 終了コード: 最終名への公開は済み中身は完全だが、後始末（一時名の削除・親ディレクトリの
/// fsync）が確かめられなかった。
pub const EXIT_PUBLISHED_UNVERIFIED: u8 = 3;
/// 終了コード（`dump-restore`）: ダンプファイルを拒否した（ヘッダ・枠組み・許可外の文・上限超過）。
pub const EXIT_INVALID_DUMP: u8 = 4;
/// 終了コード（`dump-restore`）: 流し込んだ結果の DB が整合検査に通らなかった（何も公開しない）。
pub const EXIT_NOT_CONSISTENT: u8 = 5;

/// ダンプの対象範囲。`SchemaDdlScope` への写像は [`crate::dump_run`] が行う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DumpScope {
    /// 共有の定義だけ（テナント固有のオブジェクトと行は出ない）。
    Shared,
    /// 共有の定義に、指定テナントの固有分と行を足す。
    Tenant(String),
}

/// 何を出すか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpParts {
    /// スキーマとデータ。
    All,
    /// スキーマ（`PreData` と `PostData`）だけ。
    SchemaOnly,
    /// データだけ。
    DataOnly,
}

/// データの書式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DumpFormat {
    /// `COPY ... FROM STDIN`（COPY が受理しない列型の表だけ `INSERT`）。
    Copy,
    /// 1 行 1 文の `INSERT`。
    Insert,
}

/// 検証済みの `dump` サブコマンド引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpArgs {
    /// 停止中の DB ファイル（既存の空でない通常ファイル）。
    pub db: PathBuf,
    /// 出力先（まだ存在しない新規パス）。
    pub out: PathBuf,
    /// 対象範囲。
    pub scope: DumpScope,
    /// 出す内容。
    pub parts: DumpParts,
    /// データの書式。
    pub format: DumpFormat,
}

/// 検証済みの `dump-restore` サブコマンド引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DumpRestoreArgs {
    /// ダンプファイル（既存の空でない通常ファイル）。
    pub from: PathBuf,
    /// 復元先（まだ存在しない新規パス＝空の DB だけを受け付ける）。
    pub db: PathBuf,
}

/// ダンプファイルのヘッダ行に載せられるテナント名か。ヘッダを行単位で読み戻すため、
/// 空白・制御文字・非 ASCII を含む名前は受理しない（fail-closed）。engine 側の上限
/// （空でない・長さ上限）は `PolicyContext::new` が検査する。
pub fn is_header_safe_tenant(tenant: &str) -> bool {
    tenant.bytes().all(|b| b.is_ascii_graphic())
        && engine::policy::PolicyContext::new(tenant).is_ok()
}

const DUMP_USAGE: &str = "unknown argument (usage: dump --db <path> --out <path> \
     (--tenant <id> | --shared-only) [--schema-only | --data-only] [--format copy|insert])";

/// `dump` サブコマンドの引数（サブコマンド名より後ろ）を解釈する。未知の引数・値欠落・重複指定・
/// 排他違反・必須欠落を拒否する（fail-closed。エラー文は英語で、パスやテナント名の内容は含めない）。
pub fn parse_dump(args: &[String]) -> Result<DumpArgs, String> {
    let mut db: Option<&str> = None;
    let mut out: Option<&str> = None;
    let mut tenant: Option<&str> = None;
    let mut format: Option<&str> = None;
    let mut shared_only = false;
    let mut schema_only = false;
    let mut data_only = false;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let flag = match arg.as_str() {
            "--shared-only" => &mut shared_only,
            "--schema-only" => &mut schema_only,
            "--data-only" => &mut data_only,
            _ => {
                let slot = match arg.as_str() {
                    "--db" => &mut db,
                    "--out" => &mut out,
                    "--tenant" => &mut tenant,
                    "--format" => &mut format,
                    _ => return Err(DUMP_USAGE.into()),
                };
                if slot.is_some() {
                    return Err(format!("{arg} specified more than once"));
                }
                match it.next() {
                    Some(v) => *slot = Some(v.as_str()),
                    None => return Err(format!("{arg} requires a value")),
                }
                continue;
            }
        };
        if *flag {
            return Err(format!("{arg} specified more than once"));
        }
        *flag = true;
    }
    let Some(db) = db else {
        return Err("dump requires --db".into());
    };
    let Some(out) = out else {
        return Err("dump requires --out".into());
    };
    let scope = match (tenant, shared_only) {
        (Some(_), true) => return Err("--tenant and --shared-only are mutually exclusive".into()),
        (None, false) => return Err("dump requires either --tenant or --shared-only".into()),
        (None, true) => DumpScope::Shared,
        (Some(t), false) => {
            if !is_header_safe_tenant(t) {
                return Err("--tenant must be a valid tenant id (printable ASCII)".into());
            }
            DumpScope::Tenant(t.to_string())
        }
    };
    let parts = match (schema_only, data_only) {
        (true, true) => return Err("--schema-only and --data-only are mutually exclusive".into()),
        (true, false) => DumpParts::SchemaOnly,
        (false, true) => DumpParts::DataOnly,
        (false, false) => DumpParts::All,
    };
    if scope == DumpScope::Shared && parts == DumpParts::DataOnly {
        // 共有分だけの範囲には行が無い（行はすべてテナント所有）。空のダンプを成功させない。
        return Err(
            "--shared-only has no rows to dump; it cannot be combined with --data-only".into(),
        );
    }
    let format = match format {
        None | Some("copy") => DumpFormat::Copy,
        Some("insert") => DumpFormat::Insert,
        Some(_) => return Err("--format must be copy or insert".into()),
    };
    if parts == DumpParts::SchemaOnly && format == DumpFormat::Insert {
        return Err("--format has no effect with --schema-only".into());
    }
    let db = crate::backup_opt::validate_db(db)?;
    let out = validate_new_path("--out", out)?;
    Ok(DumpArgs {
        db,
        out,
        scope,
        parts,
        format,
    })
}

/// `dump-restore` サブコマンドの引数（サブコマンド名より後ろ）を解釈する。
pub fn parse_restore(args: &[String]) -> Result<DumpRestoreArgs, String> {
    let mut from: Option<&str> = None;
    let mut db: Option<&str> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let slot = match arg.as_str() {
            "--from" => &mut from,
            "--db" => &mut db,
            _ => {
                return Err(
                    "unknown argument (usage: dump-restore --from <file> --db <path>)".into(),
                )
            }
        };
        if slot.is_some() {
            return Err(format!("{arg} specified more than once"));
        }
        match it.next() {
            Some(v) => *slot = Some(v.as_str()),
            None => return Err(format!("{arg} requires a value")),
        }
    }
    let Some(from) = from else {
        return Err("dump-restore requires --from".into());
    };
    let Some(db) = db else {
        return Err("dump-restore requires --db".into());
    };
    let from = validate_existing_file("--from", from)?;
    // 既存のパス（空でない DB を含む）は受け付けない。空の DB だけを新規に作る。
    let db = validate_new_path("--db", db)?;
    Ok(DumpRestoreArgs { from, db })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    fn exe() -> String {
        std::env::current_exe()
            .expect("current_exe")
            .to_str()
            .expect("utf-8 path")
            .to_string()
    }

    #[test]
    fn dump_rejects_malformed_arguments() {
        let exe = exe();
        assert!(parse_dump(&v(&[])).is_err());
        assert!(parse_dump(&v(&["--db"])).is_err());
        assert!(parse_dump(&v(&["--bogus"])).is_err());
        // 範囲の指定が無い（全テナントは受理しない）。
        assert!(parse_dump(&v(&["--db", &exe, "--out", "x-not-created"])).is_err());
        // 排他違反と重複。
        let base = ["--db", exe.as_str(), "--out", "x-not-created"];
        let with = |extra: &[&str]| {
            let mut a: Vec<&str> = base.to_vec();
            a.extend_from_slice(extra);
            parse_dump(&v(&a))
        };
        assert!(with(&["--tenant", "t", "--shared-only"]).is_err());
        assert!(with(&["--tenant", "t", "--tenant", "u"]).is_err());
        assert!(with(&["--shared-only", "--shared-only"]).is_err());
        assert!(with(&["--tenant", "t", "--schema-only", "--data-only"]).is_err());
        assert!(with(&["--shared-only", "--data-only"]).is_err());
        assert!(with(&["--tenant", "t", "--format", "csv"]).is_err());
        assert!(with(&["--tenant", "t", "--schema-only", "--format", "insert"]).is_err());
        assert!(with(&["--tenant", "t", "--format"]).is_err());
        // 受理する組み合わせ。
        let ok = with(&["--tenant", "t", "--data-only", "--format", "insert"]).expect("valid");
        assert_eq!(ok.scope, DumpScope::Tenant("t".into()));
        assert_eq!(ok.parts, DumpParts::DataOnly);
        assert_eq!(ok.format, DumpFormat::Insert);
        let ok = with(&["--shared-only"]).expect("valid");
        assert_eq!(ok.scope, DumpScope::Shared);
        assert_eq!(ok.parts, DumpParts::All);
        assert_eq!(ok.format, DumpFormat::Copy);
    }

    #[test]
    fn dump_rejects_invalid_tenant_ids_and_paths() {
        let exe = exe();
        for bad in ["", "a b", "a\nb", "テナント", &"x".repeat(300)] {
            let r = parse_dump(&v(&[
                "--db",
                &exe,
                "--out",
                "x-not-created",
                "--tenant",
                bad,
            ]));
            assert!(r.is_err(), "tenant {bad:?} must be rejected");
        }
        assert!(parse_dump(&v(&["--db", &exe, "--out", "../x", "--shared-only"])).is_err());
        // 出力先が既存。
        assert!(parse_dump(&v(&["--db", &exe, "--out", &exe, "--shared-only"])).is_err());
        assert!(parse_dump(&v(&[
            "--db",
            "/nonexistent-db",
            "--out",
            "x",
            "--shared-only"
        ]))
        .is_err());
    }

    #[test]
    fn restore_rejects_malformed_arguments_and_existing_destination() {
        let exe = exe();
        assert!(parse_restore(&v(&[])).is_err());
        assert!(parse_restore(&v(&["--from", &exe])).is_err());
        assert!(parse_restore(&v(&["--db", "x-not-created"])).is_err());
        assert!(parse_restore(&v(&[
            "--from",
            &exe,
            "--from",
            &exe,
            "--db",
            "x-not-created"
        ]))
        .is_err());
        assert!(parse_restore(&v(&["--from", &exe, "--db", &exe])).is_err());
        assert!(parse_restore(&v(&[
            "--from",
            "/nonexistent-dump",
            "--db",
            "x-not-created"
        ]))
        .is_err());
        assert!(parse_restore(&v(&["--from", &exe, "--db", "x-not-created", "--bogus"])).is_err());
        assert!(parse_restore(&v(&["--from", &exe, "--db", "x-not-created"])).is_ok());
    }
}
