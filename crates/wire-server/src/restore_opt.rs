//! `wire-server restore` サブコマンドの引数解釈と復元本体（Issue #2156・RECOVER-13 (3)(6)(7) の
//! ポインタ。親 #2154。バックアップは `backup` が書く `Storage::export_snapshot` の出力）。
//!
//! 責務境界: untrusted な CLI 文字列（`--from`・`--db`）を検証して [`RestoreArgs`] にし、
//! バックアップを新しい DB の位置へ「コピー → 検査 → 公開」する [`restore`] と、その結果の
//! 終了コード対応 [`classify`] を持つ。検査そのものは engine の `Storage::verify_integrity`、
//! 報告の整形・Error/Incomplete の終了コードは [`crate::verify_opt`] と共有する。
//! `main.rs::run_restore` が薄く呼ぶ。
//!
//! # 公開手順（fail-closed。`export_snapshot` の公開手順にそろえる）
//!
//! 0. 前回の異常終了が残した一時名と作業ディレクトリを掃除する（[`sweep_stale_restore_temps`]。Issue #3094・#3102。
//!    形式に厳密一致し、通常ファイル（作業ディレクトリは 0700・中身が一時名の 0600 系だけ）・自分の所有・
//!    使用中でないものだけ。一時名を作る間は親ディレクトリへ共有 lock を持つ）
//!    復元先の親ディレクトリがグループ・その他から書き込める、または他ユーザーの所有なら、何も作らず拒否する
//!    （[`RestoreError::InsecureParent`]。Issue #3102。一時コピーの差し替えを防ぐ前提）
//! 1. `--from` を読み取りで開き共有ロックを試みる（稼働中の DB を誤ってコピーしない。取れなければ拒否）
//! 2. 出力先と同じディレクトリの 0700 の作業ディレクトリ（`.fandhe-restore-<pid>-<nonce>-<seq>.d`）の中の
//!    一時名（`.fandhe-restore-<pid>-<nonce>-<seq>.tmp`）へ `create_new`・0600 でコピーし、サイズ一致を確かめて
//!    fsync する。元のバックアップには一切書かない。作業ディレクトリと一時ファイルは作成時に開いた fd を保持し、
//!    Linux では `/proc/self/fd/<n>` 経由のパスで `Storage::open`・増分の適用・`hard_link` を行う（作業
//!    ディレクトリの名前を差し替えられても、アクセス先は作成したディレクトリのまま。Issue #3102）。各段の
//!    前後で、作業ディレクトリ（fd と実パスの同一性・0700・親が共有でない）と一時ファイル（(dev, ino)・長さ）を
//!    照合し、食い違えば公開せず [`RestoreError::TempReplaced`] にする
//! 3. 一時ファイルを `Storage::open` して `verify_integrity` を実行する。Error・Incomplete が 1 件でも
//!    あれば公開せず一時ファイルを消す（検査できなかったものも整合とは扱わない）
//!    通ったら、一時ファイルを開き直して復元の世代番号を進める（RECOVER-17。失敗したら公開しない）
//!    `--generation-state` 指定時は、世代を進める直前に状態ファイル（自分所有・0600・親が共有でない通常
//!    ファイル）を開いて排他ロックし、高水位より大きい値へ進め、新しい値を状態ファイルへ fsync してから
//!    次へ進む（Issue #3105。ロックは公開の後まで保持。どの失敗でも公開しない）
//! 4. 世代を進めた一時ファイルを fsync し直し、`hard_link` の直前に作業ディレクトリと一時ファイルの inode を
//!    照合してから、`hard_link` で最終名へ公開する（既存の宛先は `EEXIST` で原子的に拒否。`rename` は置換する
//!    ため使わない）。公開先の inode も照合する。一時名と作業ディレクトリを消し、親ディレクトリを fsync する
//!
//! 既存の DB を上書きする明示フラグは設けない（置換には稼働中の判定と原子的な置換の両方が要り、
//! `rename` は開いたままのサーバーの旧 inode を黙って切り離す。運用者が旧ファイルを明示的に退避する）。
//!
//! # 注意
//!
//! 検査のために一時ファイルを `Storage::open` するため、旧形式の DB の一方向移行など `open` の
//! 書き込みが一時ファイル側にだけ入りうる（バックアップは変わらない）。復元後の世代番号
//! （RECOVER-17・#2446・#2448。設計は `docs/design/restore-generation.md`）は、検査を通った一時ファイルへ
//! `Storage::advance_restore_generation` で公開の直前に 1 回だけ書く（手順 3 と 4 の間）。

use std::path::{Path, PathBuf};

use engine::storage::{IntegrityReport, Severity};

use crate::backup_opt::{
    validate_existing_file, validate_new_path, EXIT_FAILURE, EXIT_PUBLISHED_UNVERIFIED, EXIT_USAGE,
};
use crate::verify_opt;

/// 検証済みの `restore` サブコマンド引数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreArgs {
    /// 復元元のバックアップファイル（既存の空でない通常ファイル。変更しない）。
    pub from: PathBuf,
    /// 復元先の DB ファイル（まだ存在しない新規パス）。
    pub db: PathBuf,
    /// ベースの後ろへ適用する増分バックアップ（`--incremental` の指定順。古い順。既存の通常ファイル。
    /// 変更しない）。空ならベースだけを復元する。
    pub incrementals: Vec<PathBuf>,
    /// 指定点までの復元（`--until-lsn`／`--until-time`。Issue #2169・RECOVER-14 のポインタ）。`None` なら
    /// ベース（と増分）の終点まで復元する。
    pub pitr: Option<PitrSpec>,
    /// 復元をまたぐ世代の高水位を持つ状態ファイル（`--generation-state`。Issue #3105・RECOVER-17 のポインタ）。
    /// 指定すると、新しい世代がファイルの高水位を超えることを保証し、更新してから公開する。`None` なら
    /// 時計の下限だけで進める（従来どおり）。同じ DB 系列の復元では毎回同じファイルを指定する。
    pub generation_state: Option<PathBuf>,
}

/// 指定点までの復元の指定（変更ログのアーカイブと目標）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PitrSpec {
    /// 変更ログのアーカイブのディレクトリ（`--change-log-archive-dir`。既存のディレクトリ。変更しない）。
    /// 省略時は増分バックアップだけを入力にする。
    pub archive_dir: Option<PathBuf>,
    /// 停止点の目標。
    pub target: engine::storage::PitrTarget,
}

/// `--until-time` の値を UTC のマイクロ秒にする。RFC 3339 の `YYYY-MM-DDTHH:MM:SS[.f]` に、ゾーン
/// （`Z` または `±hh:mm`）を必須で付けた形だけを受理する。ゾーン省略・IANA 名・空白区切りは拒否する
/// （時刻を推測しない）。解析は engine の `TIMESTAMPTZ` リテラル解析を使う。
pub fn parse_until_time(raw: &str) -> Result<i64, String> {
    const MSG: &str = "--until-time must be an RFC 3339 timestamp with a time zone \
                       (for example 2030-01-02T03:04:05Z)";
    let bytes = raw.as_bytes();
    let has_zone = raw.is_ascii()
        && (matches!(bytes.last(), Some(b'Z' | b'z'))
            || (bytes.len() >= 6
                && matches!(bytes.get(bytes.len() - 6), Some(b'+' | b'-'))
                && bytes.get(bytes.len() - 3) == Some(&b':')));
    // 空白はどこにも許さない（ゾーンの前の空白を解析器が受理するため、ここで拒否する）。
    if !has_zone || bytes.get(10) != Some(&b'T') || raw.contains(' ') {
        return Err(MSG.into());
    }
    engine::datetime::parse_timestamptz(raw, &engine::datetime::SessionTimeZone::utc())
        .map_err(|_| MSG.to_string())
}

/// `--until-lsn` の値（1 以上の 10 進整数。符号・空白・0 を拒否する）。
fn parse_until_lsn(raw: &str) -> Result<u64, String> {
    const MSG: &str = "--until-lsn must be a positive integer";
    if raw.is_empty() || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(MSG.into());
    }
    match raw.parse::<u64>() {
        Ok(n) if n > 0 => Ok(n),
        _ => Err(MSG.into()),
    }
}

/// `restore` サブコマンドの引数（サブコマンド名より後ろ）を解釈する。未知の引数・値欠落・重複・
/// 必須欠落・復元元と復元先の衝突は拒否する（エラー文は英語で、パスの内容は含めない）。
pub fn parse(args: &[String]) -> Result<RestoreArgs, String> {
    const USAGE: &str = "unknown argument (usage: restore --from <path> --db <path> \
                         [--incremental <path>]... [--change-log-archive-dir <dir>] \
                         [--until-lsn <n> | --until-time <rfc3339>] \
                         [--generation-state <path>])";
    let mut from: Option<&str> = None;
    let mut db: Option<&str> = None;
    let mut archive_dir: Option<&str> = None;
    let mut until_lsn: Option<&str> = None;
    let mut until_time: Option<&str> = None;
    let mut generation_state: Option<&str> = None;
    let mut incrementals: Vec<&str> = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--incremental" {
            // 複数回の指定を許す唯一のフラグ。指定順（古い順）がそのまま適用順になる。
            match it.next() {
                Some(v) => incrementals.push(v.as_str()),
                None => return Err(format!("{arg} requires a value")),
            }
            continue;
        }
        let slot = match arg.as_str() {
            "--from" => &mut from,
            "--db" => &mut db,
            "--change-log-archive-dir" => &mut archive_dir,
            "--until-lsn" => &mut until_lsn,
            "--until-time" => &mut until_time,
            "--generation-state" => &mut generation_state,
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
    let (Some(from), Some(db)) = (from, db) else {
        return Err("restore requires --from and --db".into());
    };
    let from = validate_existing_file("--from", from)?;
    // 復元先は新規パスに限る（既存の DB・ファイル・ディレクトリ・リンクは書かずに拒否）。
    let db = validate_new_path("--db", db)?;
    let incrementals = incrementals
        .into_iter()
        .map(|p| validate_existing_file("--incremental", p))
        .collect::<Result<Vec<_>, _>>()?;
    // 指定点までの復元。組み合わせの不正は DB を開く前に拒否する。
    let pitr = match (until_lsn, until_time) {
        (Some(_), Some(_)) => {
            return Err("--until-lsn and --until-time cannot be combined".into());
        }
        (None, None) => {
            if archive_dir.is_some() {
                return Err("--change-log-archive-dir requires --until-lsn or --until-time".into());
            }
            None
        }
        (lsn, time) => {
            if archive_dir.is_none() && incrementals.is_empty() {
                return Err(
                    "--until-lsn and --until-time require --change-log-archive-dir or --incremental"
                        .into(),
                );
            }
            let target = match (lsn, time) {
                (Some(n), _) => engine::storage::PitrTarget::Lsn(parse_until_lsn(n)?),
                (_, Some(t)) => engine::storage::PitrTarget::TimeMicros(parse_until_time(t)?),
                (None, None) => return Err(USAGE.into()),
            };
            let archive_dir = archive_dir
                .map(|d| crate::change_log_opt::accept_archive_dir(false, d))
                .transpose()?;
            Some(PitrSpec {
                archive_dir,
                target,
            })
        }
    };
    let generation_state = generation_state
        .map(|raw| {
            let mut others: Vec<&Path> = vec![&from, &db];
            others.extend(incrementals.iter().map(PathBuf::as_path));
            others.extend(pitr.iter().filter_map(|p| p.archive_dir.as_deref()));
            validate_generation_state_path(raw, &others)
        })
        .transpose()?;
    Ok(RestoreArgs {
        from,
        db,
        incrementals,
        pitr,
        generation_state,
    })
}

/// `--generation-state` の検証（開く前の段階。エラー文はパスの内容を含めない）。空・`..` 成分・ファイル名なし・
/// 親ディレクトリの不在・既存の通常ファイル以外（シンボリックリンク・ディレクトリ等）、および他の引数
/// （`--from`・`--db`・`--incremental`・アーカイブ先）と同じ位置を拒否する。同じ位置かは、親を実体化した
/// パスの一致と、存在する場合の (dev, ino) の一致で判定する（別名・ハードリンクで復元元を書かせない）。
fn validate_generation_state_path(raw: &str, others: &[&Path]) -> Result<PathBuf, String> {
    const FLAG: &str = "--generation-state";
    if raw.is_empty() {
        return Err(format!("{FLAG} must not be empty"));
    }
    let path = PathBuf::from(raw);
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!("{FLAG} must not contain '..' components"));
    }
    let Some(name) = path.file_name() else {
        return Err(format!("{FLAG} must name a file"));
    };
    // 掃除（Issue #3094）が消す予約名の形式は、新規・既存を問わず拒否する。既存の状態ファイルが掃除に消され
    // 空で再作成されると高水位が失われ、世代番号の単調性（RECOVER-17）が崩れるため（fail-closed）。
    crate::backup_opt::reject_reserved_name(FLAG, &path)?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let Ok(real_parent) = std::fs::canonicalize(parent) else {
        return Err(format!("parent directory of {FLAG} does not exist"));
    };
    let existing = match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() => Some(m),
        Ok(_) => return Err(format!("{FLAG} must be a regular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(format!("{FLAG} could not be inspected")),
    };
    let own = real_parent.join(name);
    for other in others {
        let same_path = other
            .file_name()
            .zip(
                other
                    .parent()
                    .map(|p| {
                        if p.as_os_str().is_empty() {
                            Path::new(".")
                        } else {
                            p
                        }
                    })
                    .and_then(|p| std::fs::canonicalize(p).ok()),
            )
            .is_some_and(|(n, p)| p.join(n) == own);
        if same_path || existing.as_ref().is_some_and(|m| same_file(m, other)) {
            return Err(format!(
                "{FLAG} must not be the same file as another restore argument"
            ));
        }
    }
    Ok(path)
}

/// 既存のメタデータ `m` が `other`（リンクをたどる）と同じファイルか（unix のみ判定できる）。
#[cfg(unix)]
fn same_file(m: &std::fs::Metadata, other: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(other).is_ok_and(|o| o.dev() == m.dev() && o.ino() == m.ino())
}

#[cfg(not(unix))]
fn same_file(_m: &std::fs::Metadata, _other: &Path) -> bool {
    false
}

/// 復元の成功結果（行の値は含めない。CLI の表示用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestoreSummary {
    /// 検査した報告（Error・Incomplete は 0 件。Warning は含みうる）。
    pub report: IntegrityReport,
}

/// 復元の失敗。`Display` は固定の英語文言で、パス・表名・行の値を含まない。
#[derive(Debug)]
#[non_exhaustive]
pub enum RestoreError {
    /// 復元先に既に何かがある、またはパスが不正（公開直前の競合を含む）。
    InvalidDestination,
    /// 復元元が通常ファイルでない、または長さ 0（parse 後の差し替えを含む。空 DB を公開しない）。
    InvalidSource,
    /// 復元元を別のプロセスが開いている（稼働中の DB をコピーしない）。
    SourceInUse,
    /// 復元元のコピーが元のサイズと一致しなかった（コピー中に変更された等）。
    SourceChanged,
    /// コピーしたバックアップを DB として開けなかった（壊れている・DB ファイルではない）。
    OpenFailed,
    /// 検査を始められなかった。
    VerifyFailed,
    /// 復元の世代番号を進められなかった（値の破損・上限到達・書き込み失敗。RECOVER-17・Issue #2448）。
    /// 世代が進まない復元は公開しない（古い世代を返す復元先を作らない。fail-closed）。
    GenerationFailed,
    /// `--generation-state` の状態ファイルを別の復元が握っている（同じ高水位を並行して読ませない。
    /// Issue #3105）。何も公開しない。
    GenerationStateInUse,
    /// `--generation-state` の状態ファイルが、自分所有の 0600 の通常ファイルで有効な状態を持つものでは
    /// なかった（Issue #3105。別のファイル・破損・上限超過・安全でない親ディレクトリを含む）。何も公開しない。
    GenerationStateRejected,
    /// 検査用の一時コピー、またはそれを置く作業ディレクトリのパスが、作成した fd と別のものに差し替えられた・
    /// 消えた・長さが合わない（空 DB が整合扱いで公開されるのを防ぐ。公開しない）。
    TempReplaced,
    /// 復元先（verify では DB）の親ディレクトリがグループ・その他から書き込める、または他ユーザーの所有で、
    /// 一時コピーの差し替えを防げない（Issue #3102）。何も作らず何も公開しない。
    InsecureParent,
    /// 検査で矛盾または検査できない項目が見つかり、公開しなかった。
    NotConsistent(IntegrityReport),
    /// この環境では復元できない（unix 以外・hard link 非対応など）。
    Unsupported,
    /// 公開直後の確認で、公開先が検査したファイルと別だった（差し替え）うえ、公開先の削除にも失敗した。
    /// 検査していない内容が公開先に残っているので、手動で削除する（公開済みの成功ではない）。
    UncheckedContentPublished,
    /// 最終名への公開は済み中身は完全だが、後始末（一時名の削除・親ディレクトリの fsync）を確かめられなかった。
    PublishedButNotFinalized,
    /// 増分バックアップを適用できなかった（連鎖の切断・順序違反・別 DB・別 timeline・破損・形式不正。
    /// チェックサムは破損・不整合の検出であり認証ではない(増分はベースと同じ信頼境界で受け渡す)。
    /// Issue #2168・RECOVER-14 のポインタ）。何も公開しない。
    Chain(engine::storage::IncrementalError),
    /// その他の入出力の失敗。
    Io(std::io::ErrorKind),
}

impl std::fmt::Display for RestoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidDestination => "--db already exists or is not a valid destination",
            Self::InvalidSource => "--from is not a non-empty regular file",
            Self::SourceInUse => {
                "backup file is in use by another process (stop the server using it first)"
            }
            Self::SourceChanged => "backup file changed while it was being copied",
            Self::OpenFailed => {
                "backup file could not be opened as a database; nothing was published"
            }
            Self::TempReplaced => "temporary copy was replaced or removed; nothing was published",
            Self::VerifyFailed => "integrity check could not be started; nothing was published",
            Self::GenerationFailed => {
                "restore generation could not be advanced; nothing was published"
            }
            Self::GenerationStateInUse => {
                "restore generation state file is in use by another restore; nothing was published"
            }
            Self::GenerationStateRejected => {
                "restore generation state file is not a private regular file with a valid state; \
                 nothing was published"
            }
            Self::InsecureParent => {
                "the parent directory of --db is writable by group or others (or owned by another \
                 user); refusing to use it for a temporary copy"
            }
            Self::NotConsistent(_) => "backup failed the integrity check; nothing was published",
            Self::Unsupported => "restore is not supported in this environment",
            Self::UncheckedContentPublished => {
                "WARNING: the destination holds content that was not checked; delete it manually"
            }
            Self::PublishedButNotFinalized => {
                "WARNING: the database file was published but its durability could not be \
                 confirmed; verify it before relying on it"
            }
            Self::Chain(e) => return write!(f, "{e}; nothing was published"),
            Self::Io(_) => "restore failed due to an I/O error",
        })
    }
}

impl std::error::Error for RestoreError {}

/// 復元結果を (終了コード, メッセージ) に対応づける。メッセージは固定の英語（成功時は件数のみ）。
/// 終了コードは [`crate::verify_opt`] の表に従う（矛盾=4・検査できず=5・公開済みで後始末未確認=3）。
pub fn classify(result: &Result<RestoreSummary, RestoreError>) -> (u8, String) {
    match result {
        Ok(s) => (
            0,
            format!(
                "rows_scanned={} warnings={}",
                s.report.rows_scanned,
                s.report.total_by_severity(Severity::Warning)
            ),
        ),
        Err(RestoreError::PublishedButNotFinalized) => (
            EXIT_PUBLISHED_UNVERIFIED,
            RestoreError::PublishedButNotFinalized.to_string(),
        ),
        Err(
            e @ (RestoreError::InvalidDestination
            | RestoreError::InvalidSource
            | RestoreError::InsecureParent
            | RestoreError::Unsupported),
        ) => (EXIT_USAGE, e.to_string()),
        Err(e @ RestoreError::NotConsistent(r)) => (verify_opt::classify(r), e.to_string()),
        Err(e) => (EXIT_FAILURE, e.to_string()),
    }
}

/// `verify` の失敗（[`verify_via_copy`] の `Err`）を表示する固定の英語文。パス・表名・行の値を含まない。
pub fn verify_failure_message(e: &RestoreError) -> &'static str {
    match e {
        RestoreError::SourceInUse => {
            "database is in use by another process (stop the server first)"
        }
        RestoreError::InvalidSource => "--db must be an existing non-empty database file",
        RestoreError::OpenFailed => "failed to open database",
        RestoreError::SourceChanged => "database file changed while it was being copied",
        RestoreError::TempReplaced => "temporary copy was replaced or removed",
        RestoreError::InsecureParent => {
            "the parent directory of --db is writable by group or others (or owned by another \
             user); refusing to copy the database there for checking"
        }
        RestoreError::VerifyFailed => "integrity check could not be started",
        _ => {
            "failed to copy the database for checking (needs free space next to it about as \
             large as the database, and a writable directory)"
        }
    }
}

/// `verify` の終了コードを決める。検査が完了した（`Ok`）うえで一時コピーを消せなかった
/// （`leftover`）場合だけ 3（主処理は完了したが後始末未確認）。それ以外は、`Ok` なら検査結果
/// （0・4・5）、`Err` なら本来の失敗（`InvalidSource`=2、他=1）。残置の有無で失敗を隠さない。
pub fn verify_exit_code(result: &Result<IntegrityReport, RestoreError>, leftover: bool) -> u8 {
    match result {
        Ok(_) if leftover => EXIT_PUBLISHED_UNVERIFIED,
        Ok(report) => verify_opt::classify(report),
        Err(RestoreError::InvalidSource | RestoreError::InsecureParent) => EXIT_USAGE,
        Err(_) => EXIT_FAILURE,
    }
}

/// バックアップ `from` を新しい DB `dest` へ復元する（モジュール doc の公開手順）。
/// 公開しなかった経路で一時コピーを消せなかったときは、残ったパスを `leftover` に入れる
/// （全テナントのデータのコピーが残るため、呼び出し元が通知する）。
#[cfg(not(unix))]
pub fn restore(
    _from: &Path,
    _dest: &Path,
    _leftover: &mut Option<PathBuf>,
) -> Result<RestoreSummary, RestoreError> {
    Err(RestoreError::Unsupported)
}

/// [`restore`] の増分つき版（非 unix では常に `Unsupported`）。
#[cfg(not(unix))]
pub fn restore_with_incrementals(
    _from: &Path,
    _dest: &Path,
    _incrementals: &[PathBuf],
    _leftover: &mut Option<PathBuf>,
) -> Result<RestoreSummary, RestoreError> {
    Err(RestoreError::Unsupported)
}

/// 検証済みの引数 `args` 一式で復元する（`--generation-state` を含む。非 unix では常に `Unsupported`）。
#[cfg(not(unix))]
pub fn restore_with_args(
    _args: &RestoreArgs,
    _leftover: &mut Option<PathBuf>,
) -> Result<RestoreSummary, RestoreError> {
    Err(RestoreError::Unsupported)
}

/// [`restore_with_incrementals`] の指定点つき版（非 unix では常に `Unsupported`）。
#[cfg(not(unix))]
pub fn restore_with_pitr(
    _from: &Path,
    _dest: &Path,
    _incrementals: &[PathBuf],
    _pitr: Option<&PitrSpec>,
    _leftover: &mut Option<PathBuf>,
) -> Result<RestoreSummary, RestoreError> {
    Err(RestoreError::Unsupported)
}

/// 前回の異常終了が `dest` の親ディレクトリに残した復元の一時名を掃除する（Issue #3094・RECOVER-13・
/// RECOVER-14 のポインタ）。`main.rs::run_restore` が [`restore_with_args`] の前に呼ぶ。消す条件は engine の
/// `storage::stale_temp`（形式に厳密一致・通常ファイル・0600 系・自分の所有・使用中でない）。宛先が既に在る・
/// 親が不正なときは `None`。結果は報告のためだけで、復元の成否には影響させない。
///
/// `protected`（`--from`・`--incremental`・`--generation-state` の明示パス）のどれかが掃除対象の全形式
/// （engine の `storage::is_reserved_temp_name`。復元の一時名・アーカイブの一時名・確認用ファイル）なら、
/// 入力の消失（復元元の変更・高水位の再作成）を避けるため掃除ごと見送って `None` を返す（fail-closed）。
#[cfg(unix)]
pub fn sweep_stale_restore_temps(
    dest: &Path,
    protected: &[&Path],
) -> Option<(PathBuf, engine::storage::StaleTempSweep)> {
    let is_temp = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(engine::storage::is_reserved_temp_name)
    };
    // 指定名が別名（シンボリックリンク）でも、実体が一時名なら掃除対象になりうるため、実体のパスでも判定する。
    // 実体を解決できないときは名前だけの判定に留まる（入力として開けないので復元自体も失敗する）。
    let reserved = protected
        .iter()
        .any(|p| is_temp(p) || std::fs::canonicalize(p).is_ok_and(|c| is_temp(&c)));
    if reserved {
        return None;
    }
    unix_impl::sweep_stale_restore_temps(dest, protected)
}

/// 非 unix では掃除しない。
#[cfg(not(unix))]
pub fn sweep_stale_restore_temps(
    _dest: &Path,
    _protected: &[&Path],
) -> Option<(PathBuf, engine::storage::StaleTempSweep)> {
    None
}

/// バックアップ `from` を新しい DB `dest` へ復元する（モジュール doc の公開手順）。
/// 公開しなかった経路で一時コピーを消せなかったときは、残ったパスを `leftover` に入れる
/// （全テナントのデータのコピーが残るため、呼び出し元が通知する）。
#[cfg(unix)]
pub fn restore(
    from: &Path,
    dest: &Path,
    leftover: &mut Option<PathBuf>,
) -> Result<RestoreSummary, RestoreError> {
    unix_impl::restore(from, dest, &[], None, None, leftover)
}

/// ベース `from` へ増分バックアップ `incrementals`（古い順）を適用した状態を、新しい DB `dest` へ復元する
/// （Issue #2168・RECOVER-13・RECOVER-14 のポインタ）。公開手順は [`restore`] と同じで、コピーの検査の前に
/// engine の `apply_incremental_segments` を一時コピーへ適用する。連鎖が切れている・順序違反・別 DB／別
/// timeline・破損・形式不正は [`RestoreError::Chain`] で、何も公開しない。
/// 細工してチェックサムを計算し直した増分は検証を通る(認証ではない)。ベースと増分のファイルは変更しない。
/// `incrementals` が空なら [`restore`] と同じ。
#[cfg(unix)]
pub fn restore_with_incrementals(
    from: &Path,
    dest: &Path,
    incrementals: &[PathBuf],
    leftover: &mut Option<PathBuf>,
) -> Result<RestoreSummary, RestoreError> {
    unix_impl::restore(from, dest, incrementals, None, None, leftover)
}

/// ベース `from` に増分 `incrementals` と変更ログのアーカイブを、`pitr` の目標のコミット境界まで適用した状態を、
/// 新しい DB `dest` へ復元する（Issue #2169・RECOVER-14 のポインタ）。`pitr` が `None` なら
/// [`restore_with_incrementals`] と同じ。公開手順は [`restore`] と同じで、検査の前に engine の
/// `restore_to_point` を一時コピーへ適用する。目標が範囲外（ベース DB の終点以前・入力したログ全体の最大 LSN より後）・
/// 欠落・順序違反・別 DB・別 timeline・破損は [`RestoreError::Chain`] で、何も公開しない（黙って末尾まで
/// 適用しない）。ベース・増分・アーカイブのファイルは変更しない。
#[cfg(unix)]
pub fn restore_with_pitr(
    from: &Path,
    dest: &Path,
    incrementals: &[PathBuf],
    pitr: Option<&PitrSpec>,
    leftover: &mut Option<PathBuf>,
) -> Result<RestoreSummary, RestoreError> {
    unix_impl::restore(from, dest, incrementals, pitr, None, leftover)
}

/// 検証済みの引数 `args` 一式で復元する（`--generation-state` を含む。Issue #3105）。状態ファイルの
/// 高水位より大きい世代へ進め、高水位を更新してから公開する。状態ファイルを開けない・不正・ロックを
/// 取れない・更新できないときは何も公開しない。`args.generation_state` が `None` なら
/// [`restore_with_pitr`] と同じ。
#[cfg(unix)]
pub fn restore_with_args(
    args: &RestoreArgs,
    leftover: &mut Option<PathBuf>,
) -> Result<RestoreSummary, RestoreError> {
    unix_impl::restore(
        &args.from,
        &args.db,
        &args.incrementals,
        args.pitr.as_ref(),
        args.generation_state.as_deref(),
        leftover,
    )
}

/// `verify` が検査に使った経路（出力の `verify_mode=` 行の値。固定語で、パス・内容を含まない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
    /// 元ファイルを読み取り専用で開いて直接検査した（コピーなし・空き容量不要）。
    ReadOnly,
    /// 読み取り専用では開けない（修復・形式更新が要る）ため、同じディレクトリの一時コピーを検査した。
    Copy,
}

impl VerifyMode {
    /// 出力用の固定語（`read_only` / `copy`）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::Copy => "copy",
        }
    }
}

/// 停止中の DB `db` を整合検査する（`verify` の本体。Issue #3074・RECOVER-13 のポインタ）。まず
/// engine の読み取り専用 open（`ReadOnlyStorage`）で元ファイルを直接検査し、コピーも書き込みもしない。
/// 読み取り専用で開けない理由が「書き込みを要する状態（異常終了後の修復・形式更新）」のときだけ、
/// [`verify_via_copy`]（一時コピーを検査。元ファイルは書かない）へ切り替える。稼働中（ロック競合）は
/// フォールバックせず [`RestoreError::SourceInUse`]。どちらの経路でも元ファイルへは書き込まない。
/// 返す [`VerifyMode`] で検査経路を呼び出し元が報告する。
pub fn verify_db(
    db: &Path,
    options: &engine::storage::IntegrityOptions,
    leftover: &mut Option<PathBuf>,
) -> Result<(IntegrityReport, VerifyMode), RestoreError> {
    use engine::storage::{ReadOnlyOpenError, ReadOnlyStorage};

    // parse 後に差し替えられた（長さ 0・通常ファイルでない）パスは開かずに拒否する。
    match std::fs::metadata(db) {
        Ok(m) if m.is_file() && m.len() > 0 => {}
        Ok(_) => return Err(RestoreError::InvalidSource),
        Err(e) => return Err(RestoreError::Io(e.kind())),
    }
    match ReadOnlyStorage::open(db) {
        Ok(ro) => {
            let report = ro
                .verify_integrity(options)
                .map_err(|_| RestoreError::VerifyFailed)?;
            Ok((report, VerifyMode::ReadOnly))
        }
        Err(ReadOnlyOpenError::NeedsWrite) => {
            verify_via_copy(db, options, leftover).map(|r| (r, VerifyMode::Copy))
        }
        Err(ReadOnlyOpenError::InUse) => Err(RestoreError::SourceInUse),
        Err(_) => Err(RestoreError::OpenFailed),
    }
}

/// 停止中の DB `db` を、元ファイルを書き込み用に開かずに一時コピーで整合検査する（`verify` のコピー経路。
/// Issue #2156。通常は [`verify_db`] の読み取り専用経路が使われ、本関数はそのフォールバック）。`db` を読み取りで開いて
/// 排他のアドバイザリロックを取り（redb が `Database::create` で使うロックと同じ仕組みで競合するため、
/// 稼働中なら [`RestoreError::SourceInUse`]）、ロックを保持したまま同じディレクトリの 0600 の
/// 一時名へコピーし、コピーを開いて `verify_integrity` を実行して一時名を消す。元ファイルへは
/// 書き込まない（open 時の移行・ヘッダ更新はコピーにだけ入る）。DB と同程度の空き容量が要る。
/// 一時名を消せなかったときは、残ったパスを `leftover` に入れる（検査結果とは別に呼び出し元が通知する）。
#[cfg(unix)]
pub fn verify_via_copy(
    db: &Path,
    options: &engine::storage::IntegrityOptions,
    leftover: &mut Option<PathBuf>,
) -> Result<IntegrityReport, RestoreError> {
    unix_impl::verify_via_copy(db, options, leftover)
}

/// [`verify_via_copy`] の非 unix スタブ。
#[cfg(not(unix))]
pub fn verify_via_copy(
    _db: &Path,
    _options: &engine::storage::IntegrityOptions,
    _leftover: &mut Option<PathBuf>,
) -> Result<IntegrityReport, RestoreError> {
    Err(RestoreError::Unsupported)
}

#[cfg(unix)]
mod unix_impl {
    use std::fs::{File, OpenOptions, TryLockError};
    use std::io::ErrorKind;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use engine::storage::{IntegrityOptions, IntegrityReport, Storage};

    use crate::restore_generation_state::GenerationState;

    use crate::dump_tmp::{check_parent_not_shared, TmpError, WorkDir};

    use super::{PitrSpec, RestoreError, RestoreSummary};

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    /// `dump_tmp` の失敗を復元の失敗へ対応づける（種別だけ。パスの内容は持ち越さない）。
    fn map_tmp(e: TmpError) -> RestoreError {
        match e {
            TmpError::InsecureParent => RestoreError::InsecureParent,
            TmpError::Replaced => RestoreError::TempReplaced,
            TmpError::InvalidDestination => RestoreError::InvalidDestination,
            TmpError::UncheckedContentPublished => RestoreError::UncheckedContentPublished,
            TmpError::PublishedButNotFinalized => RestoreError::PublishedButNotFinalized,
            TmpError::Unsupported => RestoreError::Unsupported,
            TmpError::Io(k) => RestoreError::Io(k),
        }
    }

    /// 名前 `.{prefix}-<pid>-<nonce 16桁hex>-<通番>.{suffix}`（一時ファイルは `tmp`・作業ディレクトリは `d`）を
    /// 作る。nonce はプロセスごとにランダムな鍵を持つ `RandomState` と時刻・通番を混ぜた値で、PID の再利用
    /// （PID 1 のコンテナ等）や再起動で通番が 0 に戻っても、残骸の数に依らず名前が衝突しない。
    fn unique_name(prefix: &str, suffix: &str) -> String {
        use std::hash::{BuildHasher, Hasher};
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(nanos);
        h.write_u64(seq);
        format!(
            ".{prefix}-{}-{:016x}-{seq}.{suffix}",
            std::process::id(),
            h.finish()
        )
    }

    fn io_err(e: std::io::Error) -> RestoreError {
        RestoreError::Io(e.kind())
    }

    /// 一時コピー。作成時に開いた fd を保持し続け、パスが差し替え・unlink されていないことを
    /// [`TempCopy::check_path`] で確かめる（`Storage::open` はパスで開くため、その間に一時パスが
    /// unlink されると redb が空 DB を新規作成し、空の DB が整合扱いになりうる）。
    /// 公開前の drop で一時名を消す（公開後は `armed = false`）。
    pub(super) struct TempCopy {
        pub(super) file: File,
        pub(super) path: PathBuf,
        dev: u64,
        ino: u64,
        pub(super) armed: bool,
    }

    impl Drop for TempCopy {
        fn drop(&mut self) {
            if self.armed {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }

    impl TempCopy {
        /// `parent`（[`Staging`] では作業ディレクトリの保持 fd を指すアクセス用パス）に、所有者のみ読み書きの
        /// 一時ファイルを排他的に作る。親ディレクトリの共有 lock は呼び出し側（[`Staging::create`]）が持つ。
        pub(super) fn create(parent: &Path, prefix: &str) -> Result<Self, RestoreError> {
            let mut attempts = 0u32;
            loop {
                let p = parent.join(unique_name(prefix, "tmp"));
                match OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&p)
                {
                    Ok(file) => {
                        let m = file.metadata().map_err(io_err)?;
                        return Ok(Self {
                            file,
                            path: p,
                            dev: m.dev(),
                            ino: m.ino(),
                            armed: true,
                        });
                    }
                    Err(e) if e.kind() == ErrorKind::AlreadyExists && attempts < 8 => {
                        attempts += 1;
                    }
                    Err(e) => return Err(io_err(e)),
                }
            }
        }

        /// 一時名を明示的に消す。消せなければ残ったパスを返す（`Drop` は失敗を無視するため、
        /// 正常な経路ではこちらで確かめて呼び出し元に通知する。既に無い場合は成功扱い）。
        pub(super) fn remove(&mut self) -> Result<(), PathBuf> {
            if !self.armed {
                return Ok(());
            }
            self.armed = false;
            match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
                Err(_) => Err(self.path.clone()),
            }
        }

        /// `path`（リンクをたどらない）が保持 fd と同じ (dev, ino) の通常ファイルで、長さも保持 fd と
        /// 一致するか。公開先が検査したファイルそのものかの確認に使う。
        pub(super) fn is_same_file_at(&self, path: &Path) -> bool {
            let (Ok(m), Ok(fd)) = (std::fs::symlink_metadata(path), self.file.metadata()) else {
                return false;
            };
            m.is_file() && m.dev() == self.dev && m.ino() == self.ino && m.len() == fd.len()
        }

        /// パス（リンクをたどらない）が保持 fd と同じ (dev, ino) の通常ファイルで、長さが保持 fd の
        /// 現在の長さと一致することを確かめる。`expected_len` があればその長さとも一致させる
        /// （`Storage::open` の前はコピーしたバイト数。open 後は正当に伸びうるため fd の長さと比べる）。
        pub(super) fn check_path(&self, expected_len: Option<u64>) -> Result<(), RestoreError> {
            let path_meta =
                std::fs::symlink_metadata(&self.path).map_err(|_| RestoreError::TempReplaced)?;
            let fd_meta = self.file.metadata().map_err(io_err)?;
            if !path_meta.is_file()
                || path_meta.dev() != self.dev
                || path_meta.ino() != self.ino
                || path_meta.len() != fd_meta.len()
                || expected_len.is_some_and(|n| n != path_meta.len())
            {
                return Err(RestoreError::TempReplaced);
            }
            Ok(())
        }
    }

    /// 検査対象の一時コピーと、それを置く 0700 の作業ディレクトリ・親ディレクトリの共有 lock の組
    /// （Issue #3102・RECOVER-13 のポインタ）。`restore` と `verify` のコピー経路が使う。
    ///
    /// 一時コピーをパスで開き直す箇所（`Storage::open`・増分の適用・`hard_link`）は、作業ディレクトリの保持 fd
    /// 経由のパス（[`WorkDir::access_path`]）を使い、さらに [`Staging::check`] で前後を照合する。親ディレクトリが
    /// 共有でない前提（[`WorkDir::create`] が検査）のもとで、作業ディレクトリは自分だけが変えられる。
    ///
    /// フィールドの宣言順が drop の順序（一時ファイル → 作業ディレクトリ → 親の共有 lock）。
    pub(super) struct Staging {
        pub(super) tmp: TempCopy,
        work: WorkDir,
        /// 親ディレクトリの共有 lock（Issue #3094。掃除が使用中の一時名・作業ディレクトリを消さないための
        /// 相互排他）。作業ディレクトリを作る前に取り、消した後まで保持する。
        _parent_lock: engine::storage::TempDirLock,
    }

    impl Staging {
        /// `parent` に作業ディレクトリ（0700）と、その中の一時ファイル（0600）を作る。共有 lock を取れない・
        /// 親が共有または他人所有なら、何も作らず失敗する（fail-closed）。
        pub(super) fn create(parent: &Path, prefix: &str) -> Result<Self, RestoreError> {
            let lock = engine::storage::TempDirLock::shared(parent)
                .map_err(|k| io_err(std::io::Error::from(k)))?;
            let work =
                WorkDir::create_named(parent, &mut || unique_name(prefix, "d")).map_err(map_tmp)?;
            let tmp = TempCopy::create(work.access_path(), prefix)?;
            Ok(Self {
                tmp,
                work,
                _parent_lock: lock,
            })
        }

        /// 作業ディレクトリと一時ファイルが作成時のままであることを確かめる（差し替え・unlink・長さ違いは
        /// `TempReplaced`、親が共有になっていれば `InsecureParent`）。
        pub(super) fn check(&self, expected_len: Option<u64>) -> Result<(), RestoreError> {
            self.work.check().map_err(map_tmp)?;
            self.tmp.check_path(expected_len)
        }

        /// 利用者が残骸を探せる実パス（`/proc/self/fd` を見せない）の一時ファイル。
        #[cfg(test)]
        pub(super) fn real_tmp_path(&self) -> PathBuf {
            self.work.real_path_of(&self.tmp.path)
        }

        /// 実パスの作業ディレクトリ。
        #[cfg(test)]
        pub(super) fn real_work_path(&self) -> PathBuf {
            self.work.real_path_of(Path::new(""))
        }

        /// 一時ファイルを消し、作業ディレクトリが作成時のままなら作業ディレクトリも消す。消せなかった
        /// ものの実パスを返す（作業ディレクトリが差し替えられていれば、別のディレクトリを消さずに残す）。
        pub(super) fn cleanup(&mut self) -> Option<PathBuf> {
            if let Err(p) = self.tmp.remove() {
                return Some(self.work.real_path_of(&p));
            }
            match self.work.check() {
                Ok(()) => self.work.remove().err(),
                Err(_) => {
                    self.work.disarm();
                    Some(self.work.real_path_of(Path::new("")))
                }
            }
        }

        /// 公開後の後始末（一時名と作業ディレクトリの削除・親ディレクトリの fsync）。公開は済み中身は完全なので、
        /// 失敗は `PublishedButNotFinalized`。
        fn finalize_published(&mut self, parent: &Path) -> Result<(), RestoreError> {
            self.tmp.armed = false;
            let finalized = std::fs::remove_file(&self.tmp.path)
                .map_err(|_| ())
                .and_then(|()| self.work.check().map_err(|_| ()))
                .and_then(|()| self.work.remove().map_err(|_| ()))
                .and_then(|()| {
                    File::open(parent)
                        .and_then(|d| d.sync_all())
                        .map_err(|_| ())
                });
            if finalized.is_err() {
                self.work.disarm();
                return Err(RestoreError::PublishedButNotFinalized);
            }
            Ok(())
        }
    }

    /// `path` を読み取りで開き、`exclusive` なら排他、でなければ共有のロックを試みて、通常ファイルかつ
    /// 長さ 1 以上であることを開いた fd で確かめる。ロックは返す `File` を保持する間有効。
    fn open_locked_source(path: &Path, exclusive: bool) -> Result<(File, u64), RestoreError> {
        let src = File::open(path).map_err(io_err)?;
        let locked = if exclusive {
            src.try_lock()
        } else {
            src.try_lock_shared()
        };
        match locked {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => return Err(RestoreError::SourceInUse),
            Err(TryLockError::Error(e)) => return Err(io_err(e)),
        }
        // parse 後に差し替えられた（長さ 0・FIFO 等）復元元を、開いた fd の属性で再確認する。
        // 長さ 0 のままだと空の DB が「復元」として公開されてしまう。
        let meta = src.metadata().map_err(io_err)?;
        if !meta.is_file() || meta.len() == 0 {
            return Err(RestoreError::InvalidSource);
        }
        Ok((src, meta.len()))
    }

    /// ロック済みの `src`（長さ `src_len`）を一時コピーへ書き、サイズ一致を確かめて fsync する。
    fn copy_into(src: &mut File, src_len: u64, tmp: &mut TempCopy) -> Result<(), RestoreError> {
        let copied = std::io::copy(src, &mut tmp.file).map_err(io_err)?;
        if copied != src_len || src.metadata().map_err(io_err)?.len() != src_len {
            return Err(RestoreError::SourceChanged);
        }
        tmp.file.sync_all().map_err(io_err)
    }

    /// 一時コピーを開いて検査する。開く直前と検査後に、パスが保持 fd のままであることを確かめる。
    /// `Storage` は検査後に必ず drop する（排他ロックと redb のクローズ書き込みを終える）。
    fn open_and_verify(
        staging: &Staging,
        len: u64,
        options: &IntegrityOptions,
    ) -> Result<IntegrityReport, RestoreError> {
        staging.check(Some(len))?;
        #[cfg(test)]
        run_hook(hook::Stage::BeforeVerifyOpen, staging);
        let report = {
            let storage = Storage::open(&staging.tmp.path).map_err(|_| RestoreError::OpenFailed)?;
            storage
                .verify_integrity(options)
                .map_err(|_| RestoreError::VerifyFailed)?
        };
        staging.check(None)?;
        Ok(report)
    }

    /// 検査を通った一時コピーを開き直して復元の世代番号を進める（開いた `Storage` は閉じてから返す。
    /// 書き込みで長さが変わるため、パスの同一性だけを確かめる）。
    ///
    /// `state_path`（`--generation-state`。Issue #3105）があれば、状態ファイルを開いて排他ロックを取り、
    /// 高水位より大きい世代へ進め、新しい世代を状態ファイルへ書いて fsync する。失敗したら公開しない。
    /// 返すガードは、並行の復元に同じ高水位を読ませないため、公開（`hard_link`）の後まで保持する。
    fn advance_generation(
        staging: &Staging,
        state_path: Option<&Path>,
    ) -> Result<Option<GenerationState>, RestoreError> {
        staging.check(None)?;
        // 状態ファイルの所有者は一時コピーの所有者（自分）と一致させる（`geteuid` を使わない）。
        let own_uid = staging.tmp.file.metadata().map_err(io_err)?.uid();
        let gs = state_path
            .map(|p| GenerationState::open(p, own_uid))
            .transpose()?;
        let high_water = gs.as_ref().map(GenerationState::read).transpose()?;
        let next = {
            let mut storage =
                Storage::open(&staging.tmp.path).map_err(|_| RestoreError::OpenFailed)?;
            match &high_water {
                Some(h) => storage.advance_restore_generation_above(h.value),
                None => storage.advance_restore_generation(),
            }
            .map_err(|_| RestoreError::GenerationFailed)?
        };
        staging.check(None)?;
        if let (Some(g), Some(h)) = (&gs, &high_water) {
            g.record(h, next)?;
        }
        Ok(gs)
    }

    /// 復元の一時名の判定は engine の `storage::is_restore_temp_name`（単一の真実源。出力先の予約名検査と共有）。
    pub(super) use engine::storage::is_restore_temp_name;

    /// `dest` の親ディレクトリに残った復元の一時名を掃除する（Issue #3094）。宛先が既に在る・親が
    /// 不正なときは何もしない。掃除できなかった場合も復元は続ける（呼び出し元が結果だけ報告する）。
    pub(super) fn sweep_stale_restore_temps(
        dest: &Path,
        protected: &[&Path],
    ) -> Option<(PathBuf, engine::storage::StaleTempSweep)> {
        let parent = parent_of(dest).ok()?;
        if std::fs::symlink_metadata(dest).is_ok() {
            return None;
        }
        let s = engine::storage::sweep_stale_temps_with_work_dirs(
            parent,
            is_restore_temp_name,
            engine::storage::is_restore_work_dir_name,
            protected,
        )
        .ok()?;
        Some((parent.to_path_buf(), s))
    }

    fn parent_of(path: &Path) -> Result<&Path, RestoreError> {
        path.file_name().ok_or(RestoreError::InvalidDestination)?;
        let parent: &Path = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        match std::fs::metadata(parent) {
            Ok(m) if m.is_dir() => Ok(parent),
            _ => Err(RestoreError::InvalidDestination),
        }
    }

    pub(super) fn verify_via_copy(
        db: &Path,
        options: &IntegrityOptions,
        leftover: &mut Option<PathBuf>,
    ) -> Result<IntegrityReport, RestoreError> {
        let parent = parent_of(db)?;
        // 排他ロックを検査の間保持する（redb と同じ flock。取れなければ稼働中）。
        let (mut src, src_len) = open_locked_source(db, true)?;
        let mut staging = Staging::create(parent, "fandhe-verify")?;
        let result = copy_into(&mut src, src_len, &mut staging.tmp)
            .and_then(|()| open_and_verify(&staging, src_len, options));
        drop(src);
        // 成功・失敗のどちらでも一時コピーと作業ディレクトリを明示的に消し、消せなければ（差し替えを
        // 検出して残した場合を含む）実パスを通知できるようにする。
        if let Some(p) = staging.cleanup() {
            *leftover = Some(p);
        }
        result
    }

    pub(super) fn restore(
        from: &Path,
        dest: &Path,
        incrementals: &[PathBuf],
        pitr: Option<&PitrSpec>,
        generation_state: Option<&Path>,
        leftover: &mut Option<PathBuf>,
    ) -> Result<RestoreSummary, RestoreError> {
        let parent = parent_of(dest)?;
        // 早期失敗のための確認。最終判定は後段の hard_link（確認後の競合を残さない）。
        if std::fs::symlink_metadata(dest).is_ok() {
            return Err(RestoreError::InvalidDestination);
        }

        // 親ディレクトリが共有（グループ・その他が書き込める）なら、何も開かず何も作らずに拒否する
        // （Issue #3102。一時コピーの差し替えを防ぐ前提。`dump-restore` と同じ基準）。
        check_parent_not_shared(parent).map_err(map_tmp)?;

        // 1. 復元元は読み取りだけで開き、共有ロックを試みる。稼働中の DB（redb が排他ロックを
        //    保持）なら取れず、書き込み中のファイルをコピーしない。ロックは copy の間保持する。
        let (mut src, src_len) = open_locked_source(from, false)?;

        // 2. 作業ディレクトリ（0700）の中へ、一時ファイルを 0600 の排他作成でコピーする。fd は公開まで保持する。
        let mut staging = Staging::create(parent, "fandhe-restore")?;
        let result = restore_from_copy(
            &mut src,
            src_len,
            &mut staging,
            (dest, parent),
            incrementals,
            pitr,
            generation_state,
        );
        drop(src);
        // 公開しなかった経路（公開済みなら armed は false）では、一時コピーと作業ディレクトリを明示的に消し、
        // 消せなければ通知できるようにする。
        if staging.tmp.armed {
            if let Some(p) = staging.cleanup() {
                *leftover = Some(p);
            }
        }
        result
    }

    /// 一時コピーへのコピー・検査・公開（`restore` の本体。失敗時の一時名の後始末は呼び出し元）。
    fn restore_from_copy(
        src: &mut File,
        src_len: u64,
        staging: &mut Staging,
        (dest, parent): (&Path, &Path),
        incrementals: &[PathBuf],
        pitr: Option<&PitrSpec>,
        generation_state: Option<&Path>,
    ) -> Result<RestoreSummary, RestoreError> {
        copy_into(src, src_len, &mut staging.tmp)?;

        // 2b. 増分バックアップを一時コピーへ適用する（元のバックアップには触れない。連鎖の検証・破損の
        //     拒否は engine。失敗すれば公開せずに一時コピーを消す）。適用で DB ファイルは伸びるため、
        //     以降の長さの確認は適用後の長さを基準にする。
        //     指定点までの復元（`--until-*`）なら、同じ再生器に停止条件を渡す engine の `restore_to_point` を
        //     使う（増分とアーカイブの選択・連鎖の検証・範囲外の拒否は engine。Issue #2169）。
        let mut verified_len = src_len;
        if pitr.is_some() || !incrementals.is_empty() {
            staging.check(Some(src_len))?;
            match pitr {
                Some(spec) => engine::storage::restore_to_point(
                    &staging.tmp.path,
                    incrementals,
                    spec.archive_dir.as_deref(),
                    spec.target,
                )
                .map(|_| ()),
                None => {
                    engine::storage::apply_incremental_segments(&staging.tmp.path, incrementals)
                        .map(|_| ())
                }
            }
            .map_err(RestoreError::Chain)?;
            staging.check(None)?;
            verified_len = staging.tmp.file.metadata().map_err(io_err)?.len();
        }

        // 3. コピーを開いて検査する（パスの同一性を前後で確認）。
        let report = open_and_verify(staging, verified_len, &IntegrityOptions::default())?;
        if !report.is_consistent() {
            return Err(RestoreError::NotConsistent(report));
        }

        // 復元の世代番号を進める（RECOVER-17・Issue #2448。設計は docs/design/restore-generation.md）。
        // 物理・増分・PITR（#2450）の全経路がここを通る。
        // 検査を通った一時ファイルへ、公開の前に 1 回だけ書く。公開後に古い世代の復元先が見える
        // 時間帯を作らない。失敗したら何も公開しない（一時ファイルは破棄される）。
        // 進めた後に整合検査を再実行しない: 書き込みは世代の 1 表のみで、同じ inode のまま公開する。
        // 状態ファイルを指定したときは、そのガード（排他ロック）を関数の終わり（公開・後始末の後）まで保持する。
        let _generation_guard = advance_generation(staging, generation_state)?;

        // 4. open・クローズ後の最終内容を保持 fd で fsync し直す。fsync は長くかかりうるため、
        //    パスの同一性は fsync の「後」、hard_link の直前に確かめる（hard_link は fd ではなく
        //    パスを使うので、直前の確認が差し替えの窓を最小にする）。
        staging.check(None)?;
        staging.tmp.file.sync_all().map_err(io_err)?;
        #[cfg(test)]
        run_hook(hook::Stage::BeforePublish, staging);
        // 作業ディレクトリ・一時ファイルの inode を公開の直前に照合する（差し替えは公開しない）。
        staging.check(None)?;
        match std::fs::hard_link(&staging.tmp.path, dest) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                return Err(RestoreError::InvalidDestination);
            }
            Err(e) if e.kind() == ErrorKind::Unsupported => return Err(RestoreError::Unsupported),
            Err(e) => return Err(io_err(e)),
        }
        // 公開先が保持 fd（検査したファイル）と同じでなければ、確認直後の差し替えで別の内容を
        // 公開してしまっている。`hard_link` は新規作成なので公開先は自分が作ったリンクであり、
        // 削除して公開を取り消す。削除にも失敗したら、検査していない内容が公開先に残る。
        if !staging.tmp.is_same_file_at(dest) {
            return match std::fs::remove_file(dest) {
                Ok(()) => Err(RestoreError::TempReplaced),
                Err(_) => Err(RestoreError::UncheckedContentPublished),
            };
        }
        // 公開済みで検査したファイルと一致した。以降の失敗でも最終ファイルは完全なので消さない。
        staging.finalize_published(parent)?;
        Ok(RestoreSummary { report })
    }

    /// テスト専用の差し替え注入点（状態を変えない読み取りだけの hook。本番のビルドには含めない）。
    #[cfg(test)]
    #[allow(clippy::items_after_test_module)]
    pub(super) mod hook {
        use std::cell::RefCell;
        use std::path::Path;

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(in crate::restore_opt) enum Stage {
            /// 検査のための open の直前（直前の照合の後）。
            BeforeVerifyOpen,
            /// `hard_link` の直前（直前の照合の前）。
            BeforePublish,
        }

        /// 引数は (段階, 実パスの作業ディレクトリ, 実パスの一時ファイル)。
        pub(in crate::restore_opt) type Hook = Box<dyn Fn(Stage, &Path, &Path)>;

        thread_local! {
            pub(in crate::restore_opt) static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
        }
    }

    #[cfg(test)]
    fn run_hook(stage: hook::Stage, staging: &Staging) {
        hook::HOOK.with(|h| {
            if let Some(f) = h.borrow().as_ref() {
                f(stage, &staging.real_work_path(), &staging.real_tmp_path());
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    /// 入力が一時名の実体へのシンボリックリンクなら、掃除を見送って実体を残す（Issue #3094 の指摘対応）。
    #[cfg(unix)]
    #[test]
    fn sweep_is_skipped_when_protected_input_aliases_a_temp_name() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("fandhe-sweep-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("mkdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let real = dir.join(".fandhe-restore-4242-7.tmp");
        std::fs::write(&real, b"x").expect("write");
        std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let alias = dir.join("backup.redb");
        std::os::unix::fs::symlink(&real, &alias).expect("symlink");
        let dest = dir.join("restored.redb");
        assert!(sweep_stale_restore_temps(&dest, &[alias.as_path()]).is_none());
        assert!(real.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 入力の保護判定は掃除対象の全形式（確認用ファイル・アーカイブの一時名を含む）に及ぶ（Issue #3094）。
    #[cfg(unix)]
    #[test]
    fn sweep_is_skipped_for_every_reserved_protected_name() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("fandhe-sweep-prot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("mkdir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let dest = dir.join("restored.redb");
        for name in [
            ".fandhe-sweep-probe-4242-0123456789abcdef-0.tmp",
            ".fandhe-seg-4242-0123456789abcdef-7.tmp",
            ".fandhe-restore-4242-7.tmp",
        ] {
            let p = dir.join(name);
            assert!(
                sweep_stale_restore_temps(&dest, &[p.as_path()]).is_none(),
                "{name}"
            );
        }
        assert!(sweep_stale_restore_temps(&dest, &[dir.join("plain.bin").as_path()]).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_malformed_arguments() {
        assert!(parse(&v(&[])).is_err());
        assert!(parse(&v(&["--from"])).is_err());
        assert!(parse(&v(&["--bogus", "x"])).is_err());
        assert!(parse(&v(&["--from", "a", "--from", "b", "--db", "c"])).is_err());
        assert!(parse(&v(&["--from", "a"])).is_err());
        assert!(parse(&v(&["--db", "c"])).is_err());
    }

    #[test]
    fn incremental_flag_repeats_in_order_and_requires_existing_files() {
        let exe = std::env::current_exe().expect("current_exe");
        let exe = exe.to_str().expect("utf-8 path");
        let parsed = parse(&v(&[
            "--from",
            exe,
            "--incremental",
            exe,
            "--db",
            "/nonexistent-dir-for-restore-test/x",
        ]));
        // 復元先の親が無いので拒否されるが、増分の検証より先に `--db` で落ちる（順序に依らず拒否）。
        assert!(parsed.is_err());
        // 値欠落・存在しない増分は拒否する。
        assert!(parse(&v(&["--from", exe, "--db", "x", "--incremental"])).is_err());
        assert!(parse(&v(&[
            "--from",
            exe,
            "--db",
            "x-not-created",
            "--incremental",
            "/nonexistent-incremental-for-restore-test"
        ]))
        .is_err());
        // `--incremental` だけが複数回の指定を許す（他のフラグの重複は従来どおり拒否）。
        let ok = parse(&v(&[
            "--from",
            exe,
            "--db",
            "x-not-created",
            "--incremental",
            exe,
            "--incremental",
            exe,
        ]))
        .expect("repeatable");
        assert_eq!(ok.incrementals.len(), 2);
        assert!(parse(&v(&["--from", exe, "--from", exe, "--db", "x-not-created"])).is_err());
    }

    #[test]
    fn generation_state_flag_is_validated_at_parse_time() {
        let exe = std::env::current_exe().expect("current_exe");
        let exe = exe.to_str().expect("utf-8 path");
        let base = ["--from", exe, "--db", "gen-state-test-not-created"];
        let with = |extra: &[&str]| {
            let mut a = base.to_vec();
            a.extend_from_slice(extra);
            parse(&v(&a))
        };
        // 値の欠落・重複・`..`・親の不在・復元元と同じファイルは拒否する。
        assert!(with(&["--generation-state"]).is_err());
        assert!(with(&["--generation-state", "a", "--generation-state", "b"]).is_err());
        assert!(with(&["--generation-state", "../x"]).is_err());
        assert!(with(&["--generation-state", "/nonexistent-dir-for-gen-state/x"]).is_err());
        assert!(with(&["--generation-state", exe]).is_err());
        // 存在しない新規パス（親は存在）は受理され、そのまま保持される。
        let ok = with(&["--generation-state", "gen-state-test-state.bin"]).expect("accepted");
        assert_eq!(
            ok.generation_state.as_deref(),
            Some(Path::new("gen-state-test-state.bin"))
        );
        assert!(parse(&v(&base)).expect("plain").generation_state.is_none());
    }

    #[test]
    fn until_time_requires_an_explicit_zone_and_rfc3339_shape() {
        // 2030-01-02T03:04:05Z = 1893553445 秒（UTC）。
        assert_eq!(
            parse_until_time("2030-01-02T03:04:05Z"),
            Ok(1_893_553_445_000_000)
        );
        assert_eq!(
            parse_until_time("2030-01-02T03:04:05.250000Z"),
            Ok(1_893_553_445_250_000)
        );
        // オフセットは UTC へ正規化される。
        assert_eq!(
            parse_until_time("2030-01-02T12:04:05+09:00"),
            Ok(1_893_553_445_000_000)
        );
        for bad in [
            "",
            "2030-01-02T03:04:05",
            "2030-01-02 03:04:05Z",
            "2030-01-02T03:04:05 UTC",
            "2030-01-02T03:04:05 Z",
            "2030-01-02T03:04:05 +09:00",
            "2030-01-02T03:04:05+09",
            "2030-01-02T03:04:05Asia/Tokyo",
            "2030-01-02Z",
            "yesterday",
            "2030-13-02T03:04:05Z",
            "2030-01-02T03:04:05Z\u{3042}",
        ] {
            assert!(parse_until_time(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn pitr_flags_are_validated_before_anything_is_opened() {
        let exe = std::env::current_exe().expect("current_exe");
        let exe = exe.to_str().expect("utf-8 path");
        let dir = std::env::temp_dir();
        let dir = dir.to_str().expect("utf-8 path");
        let base = ["--from", exe, "--db", "pitr-flag-test-not-created"];
        let with = |extra: &[&str]| {
            let mut a: Vec<&str> = base.to_vec();
            a.extend_from_slice(extra);
            parse(&v(&a))
        };
        // 正しい指定（アーカイブ + LSN、増分 + 時刻）。
        let ok = with(&["--change-log-archive-dir", dir, "--until-lsn", "7"]).expect("lsn");
        assert_eq!(
            ok.pitr.map(|p| p.target),
            Some(engine::storage::PitrTarget::Lsn(7))
        );
        let ok =
            with(&["--incremental", exe, "--until-time", "2030-01-02T03:04:05Z"]).expect("time");
        assert!(ok.pitr.is_some_and(|p| p.archive_dir.is_none()));
        // 併用・片方だけ・ログ源なし・不正な値は拒否する。
        let t = "2030-01-02T03:04:05Z";
        assert!(with(&[
            "--change-log-archive-dir",
            dir,
            "--until-lsn",
            "7",
            "--until-time",
            t
        ])
        .is_err());
        assert!(with(&["--change-log-archive-dir", dir]).is_err());
        assert!(with(&["--until-lsn", "7"]).is_err());
        assert!(with(&["--until-time", t]).is_err());
        for bad in [
            "0",
            "-1",
            "+1",
            " 1",
            "1 ",
            "",
            "1e3",
            "18446744073709551616",
        ] {
            assert!(
                with(&["--change-log-archive-dir", dir, "--until-lsn", bad]).is_err(),
                "{bad:?}"
            );
        }
        assert!(with(&[
            "--change-log-archive-dir",
            dir,
            "--until-time",
            "2030-01-02T03:04:05"
        ])
        .is_err());
        assert!(with(&[
            "--change-log-archive-dir",
            "/nonexistent-archive-dir-for-test",
            "--until-lsn",
            "1"
        ])
        .is_err());
        // 重複の指定。
        assert!(with(&[
            "--change-log-archive-dir",
            dir,
            "--change-log-archive-dir",
            dir,
            "--until-lsn",
            "1"
        ])
        .is_err());
        assert!(with(&[
            "--change-log-archive-dir",
            dir,
            "--until-lsn",
            "1",
            "--until-lsn",
            "2"
        ])
        .is_err());
        // 指定なしなら従来どおり。
        assert_eq!(with(&[]).expect("plain").pitr, None);
    }

    #[test]
    fn rejects_existing_destination_and_missing_source() {
        let exe = std::env::current_exe().expect("current_exe");
        let exe = exe.to_str().expect("utf-8 path");
        // 復元先が既存（実行ファイル自身）
        assert!(parse(&v(&["--from", exe, "--db", exe])).is_err());
        // 復元元が無い
        assert!(parse(&v(&[
            "--from",
            "/nonexistent-for-restore-test",
            "--db",
            "../x"
        ]))
        .is_err());
        assert!(parse(&v(&["--from", "/", "--db", "x"])).is_err());
    }

    /// 一時コピーのパスが unlink・差し替え・リンク化・長さ不一致のとき `TempReplaced` になる
    /// （`Storage::open` がパスで開く間の差し替えで空 DB が整合扱いになるのを防ぐ検査）。
    #[cfg(unix)]
    #[test]
    fn temp_copy_detects_unlink_replacement_and_length_mismatch() {
        use std::io::Write as _;
        let dir = std::env::temp_dir().join(format!("restore-opt-tempcopy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let is_replaced =
            |r: Result<(), RestoreError>| matches!(r, Err(RestoreError::TempReplaced));

        // 一致: 作成直後（長さ 0）と、書いた後の長さ
        let mut t = unix_impl::TempCopy::create(&dir, "fandhe-test").expect("create");
        assert!(t.check_path(Some(0)).is_ok());
        t.file.write_all(b"abcd").expect("write");
        assert!(t.check_path(Some(4)).is_ok());
        assert!(t.check_path(None).is_ok());
        // 期待長と違う
        assert!(is_replaced(t.check_path(Some(5))));
        // unlink された
        let path = t.path.clone();
        std::fs::remove_file(&path).expect("unlink");
        assert!(is_replaced(t.check_path(None)));
        // 別 inode の同名ファイル（空 DB を新規作成された状況）
        std::fs::write(&path, b"abcd").expect("recreate");
        assert!(is_replaced(t.check_path(None)));
        // リンクへの差し替え
        std::fs::remove_file(&path).expect("unlink");
        let other = dir.join("other");
        std::fs::write(&other, b"abcd").expect("other");
        std::os::unix::fs::symlink(&other, &path).expect("symlink");
        assert!(is_replaced(t.check_path(None)));
        std::fs::remove_file(&path).expect("unlink link");
        t.armed = false;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一時名を消せないとき `remove` は残ったパスを返す（`Drop` は失敗を無視するため、呼び出し元が
    /// 通知できる）。権限の剥奪は root では効かないため、パスを空でないディレクトリに差し替えて
    /// `remove_file` を（権限に依らず）失敗させる。既に無い場合・解除済みの場合は成功。
    #[cfg(unix)]
    #[test]
    fn temp_copy_remove_reports_leftover_path_on_failure() {
        let dir = std::env::temp_dir().join(format!("restore-opt-remove-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");

        // 通常の削除は成功し、二重の削除も成功扱い
        let mut t = unix_impl::TempCopy::create(&dir, "fandhe-test").expect("create");
        let path = t.path.clone();
        assert!(t.remove().is_ok());
        assert!(!path.exists());
        assert!(t.remove().is_ok());

        // 既に消えている場合は成功
        let mut t = unix_impl::TempCopy::create(&dir, "fandhe-test").expect("create");
        std::fs::remove_file(&t.path).expect("unlink");
        assert!(t.remove().is_ok());

        // 消せない場合は残ったパスを返す
        let mut t = unix_impl::TempCopy::create(&dir, "fandhe-test").expect("create");
        let path = t.path.clone();
        std::fs::remove_file(&path).expect("unlink");
        std::fs::create_dir(&path).expect("dir in place of file");
        std::fs::write(path.join("child"), b"x").expect("child");
        assert_eq!(t.remove(), Err(path.clone()));
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// verify の終了コード: 3 は検査が完了した（Ok）うえで消せなかったときだけ。失敗は残置があっても
    /// 本来の終了コードのまま。
    #[test]
    fn verify_exit_code_keeps_real_failures_and_uses_3_only_after_completion() {
        let ok = Ok(IntegrityReport::default());
        assert_eq!(verify_exit_code(&ok, false), 0);
        assert_eq!(verify_exit_code(&ok, true), EXIT_PUBLISHED_UNVERIFIED);
        let cases = [
            (RestoreError::SourceInUse, EXIT_FAILURE),
            (RestoreError::OpenFailed, EXIT_FAILURE),
            (RestoreError::TempReplaced, EXIT_FAILURE),
            (RestoreError::Io(std::io::ErrorKind::Other), EXIT_FAILURE),
            (RestoreError::InvalidSource, EXIT_USAGE),
        ];
        for (e, want) in cases {
            let err = Err(e);
            assert_eq!(verify_exit_code(&err, false), want);
            assert_eq!(verify_exit_code(&err, true), want);
        }
    }

    /// 公開先が保持 fd と同じファイル（hard link）のときだけ `is_same_file_at` が真になる
    /// （公開直後の差し替え検出の判定部分）。
    #[cfg(unix)]
    #[test]
    fn temp_copy_identifies_published_destination() {
        let dir = std::env::temp_dir().join(format!("restore-opt-same-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let mut t = unix_impl::TempCopy::create(&dir, "fandhe-test").expect("create");
        let linked = dir.join("linked");
        std::fs::hard_link(&t.path, &linked).expect("link");
        assert!(t.is_same_file_at(&linked));
        // 別 inode の同名ファイル・存在しないパス・リンクは偽
        let other = dir.join("other");
        std::fs::write(&other, b"").expect("other");
        assert!(!t.is_same_file_at(&other));
        assert!(!t.is_same_file_at(&dir.join("missing")));
        let sym = dir.join("sym");
        std::os::unix::fs::symlink(&linked, &sym).expect("symlink");
        assert!(!t.is_same_file_at(&sym));
        // 長さが保持 fd と違えば偽
        std::fs::write(&other, b"x").expect("write");
        std::fs::remove_file(&linked).expect("unlink");
        std::fs::write(&linked, b"abc").expect("recreate");
        assert!(!t.is_same_file_at(&linked));
        t.armed = false;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_temp_names_are_matched_strictly() {
        let dir = std::env::temp_dir().join(format!("restore-opt-name-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let mut t = unix_impl::TempCopy::create(&dir, "fandhe-restore").expect("create");
        let name = t
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("name")
            .to_string();
        assert!(unix_impl::is_restore_temp_name(&name), "{name}");
        t.armed = false;
        let _ = std::fs::remove_dir_all(&dir);
        for bad in [
            ".fandhe-restore-decoy.tmp",
            ".fandhe-restore-1-2.tmp.bak",
            ".fandhe-restore-1-2-3.tmp",
            ".fandhe-restore--2.tmp",
            ".fandhe-restore-1-.tmp",
            ".fandhe-verify-1-2.tmp",
            "fandhe-restore-1-2.tmp",
            ".fandhe-restore-12345678901-2.tmp",
        ] {
            assert!(!unix_impl::is_restore_temp_name(bad), "{bad}");
        }
    }

    #[test]
    fn sweep_is_skipped_when_an_input_has_a_reserved_name() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("restore-opt-protect-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let input = dir.join(".fandhe-restore-1-2.tmp");
        std::fs::write(&input, b"input").expect("write");
        std::fs::set_permissions(&input, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        let dest = dir.join("restored.db");
        // 予約名の入力を明示すると掃除は見送られ、入力は残る。
        assert!(sweep_stale_restore_temps(&dest, &[input.as_path()]).is_none());
        assert!(input.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn classify_maps_failures_to_distinct_codes() {
        let r = IntegrityReport::default();
        let ok = Ok(RestoreSummary { report: r.clone() });
        assert_eq!(classify(&ok).0, 0);
        assert_eq!(
            classify(&Err(RestoreError::InvalidDestination)).0,
            EXIT_USAGE
        );
        assert_eq!(classify(&Err(RestoreError::OpenFailed)).0, EXIT_FAILURE);
        assert_eq!(classify(&Err(RestoreError::TempReplaced)).0, EXIT_FAILURE);
        assert_eq!(
            classify(&Err(RestoreError::UncheckedContentPublished)).0,
            EXIT_FAILURE
        );
        assert_eq!(classify(&Err(RestoreError::SourceInUse)).0, EXIT_FAILURE);
        assert_eq!(
            classify(&Err(RestoreError::PublishedButNotFinalized)).0,
            EXIT_PUBLISHED_UNVERIFIED
        );
    }
    // ---- Issue #3102: 作業ディレクトリと保持 fd による一時コピーの差し替え防止 ----

    #[cfg(unix)]
    mod staging {
        use super::super::unix_impl::hook::{Stage, HOOK};
        use super::super::unix_impl::{self, Staging};
        use super::super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);

        /// pid・通番・時刻で一意な 0700 のディレクトリ（#2254 の衝突を避ける）。
        fn dir(label: &str) -> PathBuf {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let d = std::env::temp_dir().join(format!(
                "restore-staging-{label}-{}-{}-{nanos}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&d).expect("mkdir");
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).expect("chmod");
            d
        }

        /// 空の正常な DB ファイルを作る。
        fn make_db(path: &Path) {
            drop(engine::storage::Storage::open(path).expect("open db"));
        }

        fn names(dir: &Path) -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(dir)
                .expect("rd")
                .map(|e| e.expect("e").file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        }

        /// hook を設定し、drop で必ず外す。
        struct HookGuard;
        impl HookGuard {
            fn set(f: impl Fn(Stage, &Path, &Path) + 'static) -> Self {
                HOOK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
                Self
            }
        }
        impl Drop for HookGuard {
            fn drop(&mut self) {
                HOOK.with(|h| *h.borrow_mut() = None);
            }
        }

        /// 実パスの作業ディレクトリを別名へ退避し、同じ名前の 0700 のディレクトリへ、同じ一時名の別の
        /// 正常な DB（decoy）を置く。
        fn swap_work_dir(work: &Path, tmp: &Path, decoy_src: &Path) {
            let moved = work.with_extension("moved");
            std::fs::rename(work, &moved).expect("rename work");
            std::fs::create_dir(work).expect("mkdir work");
            std::fs::set_permissions(work, std::fs::Permissions::from_mode(0o700)).expect("chmod");
            std::fs::copy(decoy_src, work.join(tmp.file_name().expect("name"))).expect("decoy");
        }

        #[test]
        fn restore_never_checks_a_swapped_work_dir() {
            let d = dir("swapwork");
            let from = d.join("backup.redb");
            make_db(&from);
            let decoy_src = d.join("decoy-src.redb");
            make_db(&decoy_src);
            let decoy_bytes = std::fs::read(&decoy_src).expect("read");
            let out = dir("swapwork-out");
            let dest = out.join("restored.redb");
            let seen = std::rc::Rc::new(std::cell::RefCell::new(None::<PathBuf>));
            let seen2 = seen.clone();
            let _g = HookGuard::set(move |stage, work, tmp| {
                if stage == Stage::BeforeVerifyOpen && seen2.borrow().is_none() {
                    swap_work_dir(work, tmp, &decoy_src);
                    *seen2.borrow_mut() = Some(work.join(tmp.file_name().expect("name")));
                }
            });
            let mut leftover = None;
            let r = restore(&from, &dest, &mut leftover);
            assert!(matches!(r, Err(RestoreError::TempReplaced)), "{r:?}");
            assert!(!dest.exists(), "swapped copy must not be published");
            // 差し替え後の decoy は開かれても書かれてもいない（検査されていない）。
            let decoy = seen.borrow().clone().expect("hook ran");
            assert_eq!(std::fs::read(&decoy).expect("read decoy"), decoy_bytes);
            // 差し替えを検出した作業ディレクトリは消さず、実パスで通知する。
            let left = leftover.expect("leftover reported");
            assert!(!left.to_string_lossy().contains("/proc/self/fd"));
            let _ = std::fs::remove_dir_all(&d);
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn restore_never_publishes_a_replaced_temp_file() {
            let d = dir("swaptmp");
            let from = d.join("backup.redb");
            make_db(&from);
            let decoy_src = d.join("decoy-src.redb");
            make_db(&decoy_src);
            let out = dir("swaptmp-out");
            let dest = out.join("restored.redb");
            let _g = HookGuard::set(move |stage, work, tmp| {
                if stage == Stage::BeforePublish {
                    let staged = work.join("decoy.tmp");
                    std::fs::copy(&decoy_src, &staged).expect("decoy");
                    std::fs::rename(&staged, tmp).expect("replace tmp");
                }
            });
            let mut leftover = None;
            let r = restore(&from, &dest, &mut leftover);
            assert!(matches!(r, Err(RestoreError::TempReplaced)), "{r:?}");
            assert!(!dest.exists(), "decoy must not be published");
            let _ = std::fs::remove_dir_all(&d);
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn verify_copy_never_checks_a_swapped_work_dir() {
            let d = dir("verify-swap");
            let db = d.join("db.redb");
            make_db(&db);
            let decoy_src = d.join("decoy-src.redb");
            make_db(&decoy_src);
            let decoy_bytes = std::fs::read(&decoy_src).expect("read");
            let seen = std::rc::Rc::new(std::cell::RefCell::new(None::<PathBuf>));
            let seen2 = seen.clone();
            let _g = HookGuard::set(move |stage, work, tmp| {
                if stage == Stage::BeforeVerifyOpen && seen2.borrow().is_none() {
                    swap_work_dir(work, tmp, &decoy_src);
                    *seen2.borrow_mut() = Some(work.join(tmp.file_name().expect("name")));
                }
            });
            let mut leftover = None;
            let r = verify_via_copy(
                &db,
                &engine::storage::IntegrityOptions::default(),
                &mut leftover,
            );
            assert!(matches!(r, Err(RestoreError::TempReplaced)), "{r:?}");
            let decoy = seen.borrow().clone().expect("hook ran");
            assert_eq!(std::fs::read(&decoy).expect("read decoy"), decoy_bytes);
            assert!(leftover.is_some());
            let _ = std::fs::remove_dir_all(&d);
        }

        #[test]
        fn success_leaves_nothing_and_work_dir_is_private() {
            let d = dir("ok");
            let from = d.join("backup.redb");
            make_db(&from);
            let out = dir("ok-out");
            let dest = out.join("restored.redb");
            let modes = std::rc::Rc::new(std::cell::RefCell::new(Vec::<u32>::new()));
            let modes2 = modes.clone();
            let _g = HookGuard::set(move |stage, work, tmp| {
                if stage == Stage::BeforePublish {
                    let m = std::fs::metadata(work).expect("meta");
                    modes2.borrow_mut().push(m.permissions().mode() & 0o7777);
                    assert!(!work.to_string_lossy().contains("/proc/self/fd"));
                    assert!(!tmp.to_string_lossy().contains("/proc/self/fd"));
                    assert_eq!(tmp.parent(), Some(work));
                }
            });
            let mut leftover = None;
            restore(&from, &dest, &mut leftover).expect("restore");
            assert!(leftover.is_none());
            assert_eq!(modes.borrow().as_slice(), &[0o700]);
            assert_eq!(names(&out), vec!["restored.redb".to_string()]);
            // verify のコピー経路も何も残さない。
            let mut leftover = None;
            verify_via_copy(
                &dest,
                &engine::storage::IntegrityOptions::default(),
                &mut leftover,
            )
            .expect("verify");
            assert!(leftover.is_none());
            assert_eq!(names(&out), vec!["restored.redb".to_string()]);
            let _ = std::fs::remove_dir_all(&d);
            let _ = std::fs::remove_dir_all(&out);
        }

        #[test]
        fn shared_parents_are_rejected_without_creating_anything() {
            let d = dir("shared-src");
            let from = d.join("backup.redb");
            make_db(&from);
            for mode in [0o775, 0o777, 0o757, 0o1777] {
                let out = dir("shared");
                let db = out.join("db.redb");
                make_db(&db);
                std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode))
                    .expect("chmod");
                let mut leftover = None;
                let r = restore(&from, &out.join("restored.redb"), &mut leftover);
                assert!(
                    matches!(r, Err(RestoreError::InsecureParent)),
                    "{mode:o} {r:?}"
                );
                let mut leftover = None;
                let r = verify_via_copy(
                    &db,
                    &engine::storage::IntegrityOptions::default(),
                    &mut leftover,
                );
                assert!(
                    matches!(r, Err(RestoreError::InsecureParent)),
                    "{mode:o} {r:?}"
                );
                assert_eq!(names(&out), vec!["db.redb".to_string()], "{mode:o}");
                std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o700))
                    .expect("chmod");
                let _ = std::fs::remove_dir_all(&out);
            }
            assert_eq!(classify(&Err(RestoreError::InsecureParent)).0, EXIT_USAGE);
            assert_eq!(
                verify_exit_code(&Err(RestoreError::InsecureParent), false),
                EXIT_USAGE
            );
            let _ = std::fs::remove_dir_all(&d);
        }

        /// 作業ディレクトリの差し替え・一時ファイルの unlink・別 inode 化は `check` が拒否する。
        #[test]
        fn staging_check_detects_work_dir_and_temp_replacement() {
            use std::io::Write as _;
            let d = dir("check");
            let mut s = Staging::create(&d, "fandhe-test").expect("create");
            assert!(s.check(Some(0)).is_ok());
            s.tmp.file.write_all(b"abcd").expect("write");
            assert!(s.check(Some(4)).is_ok());
            assert!(matches!(s.check(Some(5)), Err(RestoreError::TempReplaced)));
            // 一時ファイルの別 inode 化
            let t = s.real_tmp_path();
            std::fs::remove_file(&t).expect("unlink");
            assert!(matches!(s.check(None), Err(RestoreError::TempReplaced)));
            std::fs::write(&t, b"abcd").expect("recreate");
            assert!(matches!(s.check(None), Err(RestoreError::TempReplaced)));
            std::fs::remove_file(&t).expect("unlink");
            // 作業ディレクトリのリンクへの差し替え
            let w = s.real_work_path();
            let moved = d.join("moved");
            std::fs::rename(&w, &moved).expect("rename");
            let victim = d.join("victim");
            std::fs::create_dir(&victim).expect("victim");
            std::os::unix::fs::symlink(&victim, &w).expect("symlink");
            assert!(matches!(s.check(None), Err(RestoreError::TempReplaced)));
            // 差し替えられた作業ディレクトリは消さず、実パスを通知する。
            let left = s.cleanup().expect("leftover");
            assert_eq!(left, w);
            assert!(victim.is_dir());
            std::fs::remove_file(&w).expect("rm link");
            std::fs::rename(&moved, &w).expect("restore");
            let _ = std::fs::remove_dir_all(&d);
        }

        #[test]
        fn staging_cleanup_removes_both_and_names_use_the_work_dir_format() {
            let d = dir("cleanup");
            let mut s = Staging::create(&d, "fandhe-restore").expect("create");
            let w = s.real_work_path();
            let wn = w.file_name().and_then(|n| n.to_str()).expect("name");
            assert!(engine::storage::is_restore_work_dir_name(wn), "{wn}");
            let tn = s.real_tmp_path();
            let tn = tn.file_name().and_then(|n| n.to_str()).expect("name");
            assert!(unix_impl::is_restore_temp_name(tn), "{tn}");
            assert!(s.cleanup().is_none());
            assert!(names(&d).is_empty());
            let _ = std::fs::remove_dir_all(&d);
        }
    }
}
