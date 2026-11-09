//! `wire-server backup` サブコマンドの引数解釈と出力先パスの検証（Issue #2152・RECOVER-13 ポインタ。
//! 方式は `docs/design/backup-snapshot.md` の第 5〜8 節）。
//!
//! 責務境界: untrusted な CLI 文字列（`--db`・`--out`）を検証済みの [`BackupArgs`] に変換するまで。
//! 書き出し本体は `engine::storage::Storage::export_snapshot` で、`main.rs::run_backup` が呼ぶ
//! （停止中の DB を直接開く経路 (i)）。稼働中のサーバーへは [`crate::backup_socket`] 経由で依頼する
//! （経路 (ii)）。サーバー側も出力先を同じ [`validate_out`] で検証し、結果は [`classify`] で
//! CLI の終了コードへ対応づける。
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
/// 終了コード: 公開先に検査していない内容が残り、取り消しの削除にも失敗した（Issue #3135）。手で消す必要が
/// あるため通常の失敗（1）と分ける。`--socket` の応答でも同じコードで区別し、CLI が固定の警告を出す。
pub const EXIT_UNCHECKED_PUBLISHED: u8 = 4;

/// サーバー側のフラグ名（運用者専用のバックアップ用ソケットを opt-in で作る。既定は無効）。
pub const SOCKET_FLAG: &str = "--backup-socket";

/// 書き出しの依頼先。`--db` と `--socket` は排他。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// 停止中の DB ファイル（既存の通常ファイル）を CLI 自身が開く。
    Db(PathBuf),
    /// 稼働中のサーバーの運用者専用ソケット。
    Socket(PathBuf),
}

/// 検証済みの `backup` サブコマンド引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupArgs {
    /// 依頼先。
    pub target: Target,
    /// 出力先（まだ存在しない新規パス。`Socket` のときは絶対パスに直してある）。
    pub out: PathBuf,
}

/// 書き出し結果を (終了コード, 応答メッセージ) に対応づける。経路 (i) の終了コードと
/// 経路 (ii) の応答の結果コードの唯一の定義で、メッセージは固定の英語（パス・表名・行の値を含まない）。
pub fn classify(
    result: &Result<engine::storage::SnapshotSummary, engine::storage::SnapshotExportError>,
) -> (u8, String) {
    use engine::storage::SnapshotExportError as E;
    match result {
        Ok(s) => (0, format!("tables={} entries={}", s.tables, s.entries)),
        Err(E::PublishedButNotFinalized) => (
            EXIT_PUBLISHED_UNVERIFIED,
            "WARNING: the snapshot file was published but its durability could not be confirmed; \
             verify the output file before relying on it"
                .to_string(),
        ),
        // 出力先の親が共有か他人所有（Issue #3135）は、運用者が出力先を選び直す利用上の誤りとして 2 にする
        // （`restore` の `InsecureParent` と同じ扱い）。何も作られていない。
        Err(e @ (E::DestinationExists | E::InvalidDestination | E::InsecureParent)) => {
            (EXIT_USAGE, e.to_string())
        }
        // 差し替えを見つけたため公開しなかった（`TempReplaced`）は実行時の失敗として 1。
        Err(e @ E::TempReplaced) => (EXIT_FAILURE, e.to_string()),
        // 検査していない内容が公開先に残った（`Display` が WARNING 文言）。手で消す案内が要るので専用コード。
        Err(e @ E::UncheckedContentPublished) => (EXIT_UNCHECKED_PUBLISHED, e.to_string()),
        Err(e) => (EXIT_FAILURE, e.to_string()),
    }
}

/// `backup` サブコマンドの引数（サブコマンド名より後ろ）を解釈する。
/// 未知の引数・値欠落・重複指定・必須欠落は拒否する（fail-closed。エラー文は英語で、パスの
/// 内容は含めない）。
pub fn parse(args: &[String]) -> Result<BackupArgs, String> {
    let mut db: Option<&str> = None;
    let mut socket: Option<&str> = None;
    let mut out: Option<&str> = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let slot =
            match arg.as_str() {
                "--db" => &mut db,
                "--socket" => &mut socket,
                "--out" => &mut out,
                _ => return Err(
                    "unknown argument (usage: backup (--db <path> | --socket <path>) --out <path>)"
                        .into(),
                ),
            };
        if slot.is_some() {
            return Err(format!("{arg} specified more than once"));
        }
        match it.next() {
            Some(v) => *slot = Some(v.as_str()),
            None => return Err(format!("{arg} requires a value")),
        }
    }
    let Some(out) = out else {
        return Err("backup requires --out".into());
    };
    let target = match (db, socket) {
        (Some(_), Some(_)) => return Err("--db and --socket are mutually exclusive".into()),
        (None, None) => return Err("backup requires either --db or --socket".into()),
        (Some(db), None) => Target::Db(validate_db(db)?),
        (None, Some(sock)) => {
            if sock.is_empty() {
                return Err("--socket must not be empty".into());
            }
            Target::Socket(PathBuf::from(sock))
        }
    };
    let mut out = validate_out(out)?;
    if matches!(target, Target::Socket(_)) {
        out = absolutize_for_socket(out)?;
    }
    Ok(BackupArgs { target, out })
}

/// ソケットへ渡す出力先を絶対パスに直す（サーバーの作業ディレクトリは CLI と異なるため）。行プロトコルの
/// 区切りと衝突する改行を含むパス・UTF-8 でないパスは拒否する。`backup`・`backup-incremental` が使う。
pub fn absolutize_for_socket(out: PathBuf) -> Result<PathBuf, String> {
    let out = std::path::absolute(&out)
        .map_err(|_| "cannot resolve --out to an absolute path".to_string())?;
    if out.to_string_lossy().contains('\n') || out.to_str().is_none() {
        return Err("--out must be valid UTF-8 without line breaks".into());
    }
    Ok(out)
}

/// `backup` の出力先 `out` の親に残った、前回の異常終了のスナップショットの作業ディレクトリ・一時ファイル
/// （Issue #3135・RECOVER-13 のポインタ）を、自分の書き出しの前に掃除する。`main.rs::run_backup` が `--db`・
/// `--socket` の両方で呼ぶ。宛先が既に在る・親が不正なときは何もしない。掃除できなかった場合も本処理は続ける
/// （呼び出し元が結果だけ報告する）。`protected` は消してはならない入力（`--db`・ソケット）のパス。書き込み中の
/// 作業ディレクトリは書き込み側の共有 lock により `InUse` で見送られる。
pub fn sweep_stale_backup_temps(
    out: &Path,
    protected: &[&Path],
) -> Option<(PathBuf, engine::storage::StaleTempSweep)> {
    out.file_name()?;
    let parent: &Path = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if std::fs::symlink_metadata(out).is_ok() {
        return None;
    }
    let s = engine::storage::sweep_stale_temps_with_work_dirs(
        parent,
        engine::storage::is_snapshot_temp_name,
        engine::storage::is_snapshot_work_dir_name,
        protected,
    )
    .ok()?;
    Some((parent.to_path_buf(), s))
}

/// `Storage::open` の失敗が「別プロセスが開いている」ことによるかを判定する（`backup`・`verify` が
/// 「stop the server first」を案内するために使う）。redb のエラー種別は wire-server から名前で
/// 参照できない（redb 非依存）ため、`DatabaseAlreadyOpen` の表示文言に依存している。文言が変わると
/// 案内が一般的な「failed to open」に落ちるだけで、終了コード（1）と fail-closed は変わらない。
pub fn is_already_open(e: &engine::storage::StorageError) -> bool {
    e.to_string().contains("already open")
}

/// `path` の名前（シンボリックリンクを解決した実体の名前を含む）が、掃除（Issue #3094）が消す予約名の形式なら
/// エラーを返す。永続ファイル・入力ファイルを指す全フラグの検証がこの関数を通る（fail-closed。新規・既存を問わない）。
/// 同じ親での掃除に、完成済みのファイルや開いている DB が消されるのを防ぐ。パスの内容はエラー文に出さない。
pub fn reject_reserved_name(flag: &str, path: &Path) -> Result<(), String> {
    let reserved = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(engine::storage::is_reserved_temp_name)
    };
    if reserved(path) || std::fs::canonicalize(path).is_ok_and(|c| reserved(&c)) {
        return Err(format!(
            "{flag} must not use a reserved temporary file name"
        ));
    }
    Ok(())
}

/// `--db` の検証。`Storage::open` は存在しない・長さ 0 のパスに空の DB を作るため、既存の
/// 空でない通常ファイルに限る（typo で空 DB が生成され「成功」するのを防ぐ）。
/// `verify`・`restore` からも使う（[`crate::verify_opt`]・[`crate::restore_opt`]）。
pub fn validate_db(raw: &str) -> Result<PathBuf, String> {
    validate_existing_file("--db", raw)
}

/// 既存の空でない通常ファイルだけを受け入れる共通検査（フラグ名はエラー文に使う。パスは出さない）。
/// 長さ 0 のファイルは `Storage::open` が空 DB として初期化してしまうため拒否する。
pub fn validate_existing_file(flag: &str, raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err(format!("{flag} must not be empty"));
    }
    let path = PathBuf::from(raw);
    reject_reserved_name(flag, &path)?;
    match std::fs::metadata(&path) {
        Ok(m) if m.is_file() && m.len() > 0 => Ok(path),
        _ => Err(format!(
            "{flag} must be an existing non-empty database file"
        )),
    }
}

/// `--out` の検証。空・`..` 成分・ファイル名なし・既存の何か（シンボリックリンクを含む）・
/// 親ディレクトリの不在を拒否する。
pub fn validate_out(raw: &str) -> Result<PathBuf, String> {
    validate_new_path("--out", raw)
}

/// まだ存在しない新規パスの共通検査（`backup --out`・`restore --db`・dump・incremental 等が使う。フラグ名はエラー文に
/// 使い、パスの内容は出さない）。確認後の競合は公開側の `hard_link`（`EEXIST`）が原子的に拒否する。
pub fn validate_new_path(flag: &str, raw: &str) -> Result<PathBuf, String> {
    if raw.is_empty() {
        return Err(format!("{flag} must not be empty"));
    }
    let path = PathBuf::from(raw);
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("{flag} must not contain '..' components"));
    }
    if path.file_name().is_none() {
        return Err(format!("{flag} must name a file"));
    }
    // 掃除（Issue #3094）が消す一時名の形式の出力先は拒否する（fail-closed。non-UTF-8 は形式に一致しない）。
    reject_reserved_name(flag, &path)?;
    // 存在確認はリンクをたどらない（壊れたリンク経由の書き出しも拒否する）。
    if std::fs::symlink_metadata(&path).is_ok() {
        return Err(format!("{flag} already exists; refusing to overwrite"));
    }
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    match std::fs::metadata(parent) {
        Ok(m) if m.is_dir() => Ok(path),
        _ => Err(format!("parent directory of {flag} does not exist")),
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
        assert!(parse(&v(&["--db", "a", "--socket", "b", "--out", "c"])).is_err());
        assert!(parse(&v(&["--out", "c"])).is_err());
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
    fn unchecked_content_published_has_its_own_exit_code() {
        let (code, msg) = classify(&Err(
            engine::storage::SnapshotExportError::UncheckedContentPublished,
        ));
        assert_eq!(code, EXIT_UNCHECKED_PUBLISHED);
        assert!(msg.starts_with("WARNING:"));
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
