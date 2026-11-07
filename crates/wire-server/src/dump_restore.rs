//! `wire-server dump-restore` の実行本体（Issue #2162・RECOVER-16 ポインタ。方式は
//! `docs/design/logical-dump.md` の第 12 節）。
//!
//! 責務境界: ダンプファイル（[`crate::dump_file`]）を 1 枠ずつ読み、一時名の新規 DB へ engine の
//! 通常の SQL 実行経路（`EngineCore::execute_sql_in_session`・`begin_copy`／`commit_copy_in`。
//! 第 2 の書き込み経路は作らない）で流し込み、整合検査に通ったときだけ [`crate::dump_tmp`] で
//! 復元先へ公開する。途中のどこで失敗しても何も公開せず一時名を消すので、復元は全体として
//! 原子的に成功か不成功かのどちらかになる（既存の復元先は上書きしない＝空の DB だけを受け付ける）。
//! `main.rs` の `run_dump_restore` が呼ぶ。
//!
//! 入力の扱い: ダンプファイルは untrusted。ヘッダは完全一致で検査し、本文は見出しの長さで切り出して
//! 字句解析器（`split_statements`）が 1 文と判定したものだけを、種別ごとの許可表（先頭の語句）に
//! 合う場合に限って実行する。それでもファイルは運用者が選んだ DB への書き込み SQL を運ぶ経路であり、
//! 信頼できる出典のファイルだけを渡すこと（設計書の第 12 節に危険性を記す）。エラー文は固定の英語で、
//! 文の内容・名前・行の値を含まない（失敗した枠の通し番号と SQLSTATE のみ）。

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use engine::core::{CopyPlan, EngineCore};
use engine::sql::copy::CopyInSession;
use engine::sql::mode::SessionState;
use engine::sql::statement_splitter::{split_statements, SplitOutcome};
use engine::storage::{IntegrityOptions, Storage};

use crate::dump_file::{
    is_copy_from_stdin, CopyLine, FileError, FileReader, Header, Phase, Record, StmtKind,
};
use crate::dump_opt::{
    DumpFormat, DumpParts, DumpScope, EXIT_FAILURE, EXIT_INVALID_DUMP, EXIT_NOT_CONSISTENT,
    EXIT_PUBLISHED_UNVERIFIED, EXIT_USAGE,
};
use crate::dump_tmp::{parent_of, TempFile, TmpError, WorkDir};

/// 共有分だけのダンプ（テナント固有の内容が無い）を流し込むときの実行テナント。共有の定義は
/// どのテナントの文脈で作っても同じ（行は無い）。実在のテナント名と衝突しても害は無い。
const SHARED_RESTORE_TENANT: &str = "dump-restore";
/// COPY を 1 回の commit に分けるときの 1 回あたりの目安バイト数。
const COPY_CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// 復元の件数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RestoreSummary {
    /// 実行した文（`COPY` 1 件を 1 文として数える）の数。
    pub statements: u64,
    /// `COPY` で戻した行の数（`INSERT` 形式の行は文として数える）。
    pub copy_rows: u64,
}

/// `dump-restore` の失敗（種別と固定の理由だけを持つ）。
#[derive(Debug)]
pub enum RestoreError {
    /// ダンプファイルを開けない・通常ファイルでない。
    SourceUnreadable,
    /// ダンプファイルを拒否した（ヘッダ・枠・上限・UTF-8）。
    Dump(FileError),
    /// ヘッダが宣言する内容は新規 DB へ復元できない（データのみ）。
    NotRestorable,
    /// ヘッダのテナントが `--expect-tenant` と一致しない。
    TenantMismatch,
    /// 種別の許可表に合わない・1 文でない文を含む。`ordinal` は 1 から数える枠の通し番号。
    Disallowed {
        /// 枠の通し番号。
        ordinal: u64,
    },
    /// 文の実行に失敗した。
    Apply {
        /// 枠の通し番号。
        ordinal: u64,
        /// SQLSTATE（engine の `wire_code`）。
        code: &'static str,
    },
    /// 一時 DB を開けない。
    OpenFailed,
    /// 流し込んだ DB の整合検査を始められなかった。
    VerifyFailed,
    /// 流し込んだ DB が整合検査に通らなかった。
    NotConsistent,
    /// 一時ファイルの作成・公開の失敗。
    Tmp(TmpError),
}

/// 結果を (終了コード, メッセージ) に対応づける。メッセージは固定の英語。
pub fn classify(result: &Result<RestoreSummary, RestoreError>) -> (u8, String) {
    match result {
        Ok(s) => (
            0,
            format!("statements={} copy_rows={}", s.statements, s.copy_rows),
        ),
        Err(RestoreError::SourceUnreadable) => {
            (EXIT_FAILURE, "failed to open the dump file".into())
        }
        Err(RestoreError::Dump(e)) => (EXIT_INVALID_DUMP, e.message().into()),
        Err(RestoreError::NotRestorable) => (
            EXIT_INVALID_DUMP,
            "a data-only dump cannot be restored into a new database".into(),
        ),
        Err(RestoreError::TenantMismatch) => (
            EXIT_INVALID_DUMP,
            "the dump tenant does not match --expect-tenant".into(),
        ),
        Err(RestoreError::Disallowed { ordinal }) => (
            EXIT_INVALID_DUMP,
            format!("dump item {ordinal} is not an allowed statement; nothing was published"),
        ),
        Err(RestoreError::Apply { ordinal, code }) => (
            EXIT_FAILURE,
            format!("failed to apply dump item {ordinal} (sqlstate {code}); nothing was published"),
        ),
        Err(RestoreError::OpenFailed) => (EXIT_FAILURE, "failed to create the database".into()),
        Err(RestoreError::VerifyFailed) => (
            EXIT_FAILURE,
            "failed to check the restored database; nothing was published".into(),
        ),
        Err(RestoreError::NotConsistent) => (
            EXIT_NOT_CONSISTENT,
            "the restored database failed the integrity check; nothing was published".into(),
        ),
        Err(RestoreError::Tmp(TmpError::InvalidDestination)) => (
            EXIT_USAGE,
            "--db already exists or its parent directory is missing; refusing to overwrite".into(),
        ),
        Err(RestoreError::Tmp(TmpError::PublishedButNotFinalized)) => (
            EXIT_PUBLISHED_UNVERIFIED,
            "WARNING: the database was published but its durability could not be confirmed; \
             verify it before relying on it"
                .into(),
        ),
        Err(RestoreError::Tmp(TmpError::UncheckedContentPublished)) => (
            EXIT_FAILURE,
            "WARNING: the destination holds content that was not checked; delete it manually"
                .into(),
        ),
        Err(RestoreError::Tmp(_)) => (EXIT_FAILURE, "failed to create the database file".into()),
    }
}

/// ダンプファイル `from` を一時名の新規 DB へ流し込み、検査に通ったときだけ `dest` へ公開する。
/// 一時名を消せなかった場合は `leftover` にそのパスを入れる（失敗の種別は変えない）。
pub fn restore(
    from: &Path,
    dest: &Path,
    expect_tenant: Option<&str>,
    leftover: &mut Option<PathBuf>,
) -> Result<RestoreSummary, RestoreError> {
    let parent = parent_of(dest).map_err(RestoreError::Tmp)?;
    if std::fs::symlink_metadata(dest).is_ok() {
        return Err(RestoreError::Tmp(TmpError::InvalidDestination));
    }
    let file = File::open(from).map_err(|_| RestoreError::SourceUnreadable)?;
    let meta = file
        .metadata()
        .map_err(|_| RestoreError::SourceUnreadable)?;
    if !meta.is_file() || meta.len() == 0 {
        return Err(RestoreError::SourceUnreadable);
    }
    // ヘッダを検証してから DB も一時ファイルも作る（拒否するファイルでは何も作らない）。
    let (mut reader, header) =
        FileReader::open(BufReader::new(file)).map_err(RestoreError::Dump)?;
    if header.parts == DumpParts::DataOnly {
        return Err(RestoreError::NotRestorable);
    }
    // ヘッダのテナントは信用される。取り違えを防ぎたい運用者は `--expect-tenant` で固定する。
    if let Some(t) = expect_tenant {
        if header.scope != DumpScope::Tenant(t.to_string()) {
            return Err(RestoreError::TenantMismatch);
        }
    }
    // 一時 DB は、復元先の親の中に作る 0700 の作業ディレクトリに置く。親に他者が書き込めても、
    // 一時名をパスで開く間に別のファイル（既存 DB へのリンク）へ差し替えられない。
    let mut work = WorkDir::create(parent, "fandhe-dump-restore").map_err(RestoreError::Tmp)?;
    let mut tmp = match TempFile::create(work.access_path(), "db") {
        Ok(t) => t,
        Err(e) => {
            let _ = work.remove();
            return Err(RestoreError::Tmp(e));
        }
    };
    let result = apply_verify_publish(&mut reader, &header, &mut tmp, &work, dest);
    if let Err(p) = tmp.remove() {
        *leftover = Some(p);
    }
    if let Err(p) = work.remove() {
        leftover.get_or_insert(p);
    }
    result
}

fn apply_verify_publish(
    reader: &mut FileReader<BufReader<File>>,
    header: &Header,
    tmp: &mut TempFile,
    work: &WorkDir,
    dest: &Path,
) -> Result<RestoreSummary, RestoreError> {
    // 長さ 0 の一時ファイルを開くと engine が新規 DB として初期化する。開く前後でパスが保持 fd の
    // ままであることを確かめる（差し替えられた場合に別のファイルへ書かない）。
    work.check().map_err(RestoreError::Tmp)?;
    tmp.check_path().map_err(RestoreError::Tmp)?;
    let summary = {
        let core = EngineCore::open(&tmp.path).map_err(|_| RestoreError::OpenFailed)?;
        let summary = apply(&core, reader, header)?;
        // `core` をここで閉じる（排他ロックと redb のクローズ書き込みを終える）。
        drop(core);
        summary
    };
    work.check().map_err(RestoreError::Tmp)?;
    tmp.check_path().map_err(RestoreError::Tmp)?;
    {
        let storage = Storage::open(&tmp.path).map_err(|_| RestoreError::VerifyFailed)?;
        let report = storage
            .verify_integrity(&IntegrityOptions::default())
            .map_err(|_| RestoreError::VerifyFailed)?;
        if !report.is_consistent() {
            return Err(RestoreError::NotConsistent);
        }
    }
    work.check().map_err(RestoreError::Tmp)?;
    tmp.publish(dest).map_err(RestoreError::Tmp)?;
    Ok(summary)
}

/// ヘッダの宣言と枠の内容が整合するか。`scope: shared` の範囲には行が無く、`parts` と `format` が
/// 排除するデータ枠・COPY 枠は受け付けない（取り違え・改ざんの検出）。`format: copy` のダンプでも、
/// COPY が受理しない列型の表は `INSERT` で出る（#2161）ため、`Insert` 枠は `copy` の下でも許す。
fn frame_matches_header(header: &Header, is_data: bool, is_copy: bool) -> bool {
    if !is_data {
        return true;
    }
    header.scope != DumpScope::Shared
        && header.parts != DumpParts::SchemaOnly
        && !(is_copy && header.format == DumpFormat::Insert)
}

/// 枠を順に実行する。ヘッダのテナントの文脈（認証済みセッションと同じ可視性集合）で実行する。
///
/// 枠は `Pre` → `Data` → `Post` の順にしか並べられない（逆行は拒否）。DDL 実行権限を持つのは
/// `Pre`・`Post` を流すセッションだけで、`Data` の枠は DDL 権限なしの別セッションで実行する。
/// トリガ・ポリシー・制約などデータ投入中に効くものは `Post` でしか受け付けないので、投入中に
/// ダンプファイル由来のトリガ関数は走らない。
fn apply(
    core: &EngineCore,
    reader: &mut FileReader<BufReader<File>>,
    header: &Header,
) -> Result<RestoreSummary, RestoreError> {
    let tenant = match &header.scope {
        DumpScope::Shared => SHARED_RESTORE_TENANT,
        DumpScope::Tenant(t) => t.as_str(),
    };
    let ctx = crate::auth::session_policy_context(tenant)
        .map_err(|_| RestoreError::Dump(FileError::BadHeader))?;
    let load_failed = |_| RestoreError::Apply {
        ordinal: 0,
        code: "XX000",
    };
    // 復元は運用者が明示して実行する CLI であり、ダンプの DDL 文を実行するには DDL 実行権限が要る。
    // 権限を与えるのはこの一時 DB 上の DDL 用セッションだけで、実行できる文は種別ごとの形で絞っている。
    let mut ddl_session = SessionState::default();
    ddl_session.allow_ddl();
    core.load_session_udfs(&ctx, &mut ddl_session)
        .map_err(load_failed)?;
    let mut data_session = SessionState::default();
    core.load_session_udfs(&ctx, &mut data_session)
        .map_err(load_failed)?;
    let mut summary = RestoreSummary::default();
    let mut ordinal: u64 = 0;
    let mut phase = Phase::Pre;
    while let Some(record) = reader.next_record().map_err(RestoreError::Dump)? {
        ordinal = ordinal.saturating_add(1);
        let disallowed = RestoreError::Disallowed { ordinal };
        match record {
            Record::Statement { kind, sql } => {
                let is_data = kind == StmtKind::Insert;
                if kind.phase() < phase
                    || !frame_matches_header(header, is_data, false)
                    || !kind.accepts(&sql)
                    || !is_single_statement(&sql)
                {
                    return Err(disallowed);
                }
                phase = kind.phase();
                let session = if is_data {
                    &mut data_session
                } else {
                    &mut ddl_session
                };
                core.refresh_session_udfs(&ctx, session);
                core.execute_sql_in_session(&ctx, session, &sql)
                    .map_err(|e| RestoreError::Apply {
                        ordinal,
                        code: e.wire_code(),
                    })?;
                summary.statements = summary.statements.saturating_add(1);
            }
            Record::CopyHeader { sql } => {
                if phase > Phase::Data
                    || !frame_matches_header(header, true, true)
                    || !is_copy_from_stdin(&sql)
                    || !is_single_statement(&sql)
                {
                    return Err(disallowed);
                }
                phase = Phase::Data;
                let rows = restore_copy(core, &ctx, &mut data_session, &sql, reader, ordinal)?;
                summary.statements = summary.statements.saturating_add(1);
                summary.copy_rows = summary.copy_rows.saturating_add(rows);
            }
        }
    }
    Ok(summary)
}

/// 字句解析器の規則（文字列リテラル・ドル引用・コメント）で、本文が 1 文であることを確かめる。
fn is_single_statement(sql: &str) -> bool {
    matches!(split_statements(sql), Ok(SplitOutcome::Single))
}

/// 1 つの `COPY` の全データ行を、engine のバッチ上限に収まる単位に分けて commit する。
/// COPY text の行内の改行・CR はエスケープ済みなので、生の改行は行境界だけである。戻り値は戻した行数。
fn restore_copy(
    core: &EngineCore,
    ctx: &engine::policy::PolicyContext,
    session: &mut SessionState,
    sql: &str,
    reader: &mut FileReader<BufReader<File>>,
    ordinal: u64,
) -> Result<u64, RestoreError> {
    let apply_err = |e: engine::sql::allowlist::SqlSurfaceError| RestoreError::Apply {
        ordinal,
        code: e.wire_code(),
    };
    let begin = |session: &mut SessionState| -> Result<CopyInSession, RestoreError> {
        core.refresh_session_udfs(ctx, session);
        match core.begin_copy(ctx, session, sql).map_err(apply_err)? {
            CopyPlan::From(c) => Ok(c),
            CopyPlan::To(..) => Err(RestoreError::Disallowed { ordinal }),
        }
    };
    let commit = |copy: CopyInSession| -> Result<(), RestoreError> {
        let batch = copy.finish().map_err(apply_err)?;
        core.commit_copy_in(ctx, batch).map_err(apply_err)?;
        Ok(())
    };
    let mut total: u64 = 0;
    // 現在のバッチ（セッション・行数・合計バイト）と、engine のバッチ上限から求めた 1 バッチの上限。
    // 上限は行数（①）・生成チャンク数（④。1 行 1 チャンク）・合計バイト（③）のすべてを見る。
    let mut current: Option<(CopyInSession, usize, usize)> = None;
    let mut max_rows: usize = 1;
    let mut max_bytes: usize = COPY_CHUNK_BYTES;
    loop {
        match reader.next_copy_line().map_err(RestoreError::Dump)? {
            CopyLine::End => break,
            CopyLine::Row(line) => {
                // 次の行を足すと上限を超えるなら、足す前に今のバッチを commit して新しく始める。
                // 1 行だけで上限を超える場合は新しいバッチの `feed` が拒否する（fail-closed）。
                let full = current.as_ref().is_some_and(|(_, rows, bytes)| {
                    rows.saturating_add(1) > max_rows
                        || bytes.saturating_add(line.len()) > max_bytes
                });
                if full {
                    if let Some((copy, rows, _)) = current.take() {
                        commit(copy)?;
                        total = total.saturating_add(rows as u64);
                    }
                }
                if current.is_none() {
                    let copy = begin(session)?;
                    let l = copy.batch_limits();
                    max_rows = l.max_files_per_batch.min(l.max_batch_chunks).max(1);
                    max_bytes = l.max_batch_total_bytes.clamp(1, COPY_CHUNK_BYTES);
                    current = Some((copy, 0, 0));
                }
                if let Some((copy, rows, bytes)) = current.as_mut() {
                    copy.feed(line).map_err(apply_err)?;
                    *rows = rows.saturating_add(1);
                    *bytes = bytes.saturating_add(line.len());
                }
            }
        }
    }
    if let Some((copy, rows, _)) = current.take() {
        commit(copy)?;
        total = total.saturating_add(rows as u64);
    }
    if total == 0 {
        // 書き手は行の無い表の COPY を出さない。空の COPY は枠の破損として拒否する。
        return Err(RestoreError::Dump(FileError::BadFrame));
    }
    Ok(total)
}
