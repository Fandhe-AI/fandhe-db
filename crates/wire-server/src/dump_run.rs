//! `wire-server dump` の実行本体（Issue #2162・RECOVER-16 ポインタ。方式は
//! `docs/design/logical-dump.md` の第 12 節）。
//!
//! 責務境界: 検証済みの [`DumpArgs`] を engine の書き出し入口（`Storage::export_schema_ddl`・
//! `export_table_data`・`export_logical_dump`）へ写し、渡された断片を [`crate::dump_file`] の形式で
//! 0600 の一時名へ書き、成功したときだけ [`crate::dump_tmp`] で最終名へ公開する。`main.rs` の
//! `run_dump` が呼ぶ。失敗時は一時ファイルを消し、何も公開しない（途中までの断片は不完全なため）。
//!
//! テナント境界: 範囲は `SchemaDdlScope` にそのまま写す（`--tenant` は `shared_and_tenant`、
//! `--shared-only` は `shared_only`）。engine は指定テナントの行だけを読み、他テナントの行・名前は
//! 出力に現れない。エラー文は固定の英語で、DDL・行・名前を含まない。

use std::io::BufWriter;
use std::path::PathBuf;

use engine::catalog::ddl_export::{
    LogicalDumpItem, SchemaDdlError, SchemaDdlScope, TableDataFormat, TableDataPart,
};
use engine::core::EngineCore;
use engine::storage::Storage;

use crate::dump_file::{FileError, FileWriter, Header, StmtKind};
use crate::dump_opt::{
    DumpArgs, DumpFormat, DumpParts, DumpScope, EXIT_FAILURE, EXIT_PUBLISHED_UNVERIFIED, EXIT_USAGE,
};
use crate::dump_tmp::{parent_of, TempFile, TmpError};

/// 書き出しの件数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DumpSummary {
    /// 書き出した DDL 文の数。
    pub statements: u64,
    /// 1 行以上を書き出した表の数。
    pub tables: u64,
    /// 書き出した行の総数。
    pub rows: u64,
}

/// `dump` の失敗（種別と固定の理由だけを持つ。名前・行・パスは含めない）。
#[derive(Debug)]
pub enum DumpError {
    /// 別プロセスが DB を開いている。
    InUse,
    /// DB を開けない。
    OpenFailed,
    /// 書き出せないオブジェクトがあった（種別と理由は engine が固定文字列で返す）。
    Unsupported(&'static str, &'static str),
    /// カタログの読み取りの失敗。
    Catalog,
    /// 復元の世代番号を読めない（値の破損・キー欠落。RECOVER-17）。
    Generation,
    /// ダンプファイルへの書き込みの失敗（上限超過・枠にできない値を含む）。
    Write(FileError),
    /// 一時ファイルの作成・公開の失敗。
    Tmp(TmpError),
}

/// 結果を (終了コード, メッセージ) に対応づける。メッセージは固定の英語。
pub fn classify(result: &Result<DumpSummary, DumpError>) -> (u8, String) {
    match result {
        Ok(s) => (
            0,
            format!(
                "statements={} tables={} rows={}",
                s.statements, s.tables, s.rows
            ),
        ),
        Err(DumpError::InUse) => (
            EXIT_FAILURE,
            "database is in use by another process (stop the server first)".into(),
        ),
        Err(DumpError::OpenFailed) => (EXIT_FAILURE, "failed to open database".into()),
        Err(DumpError::Unsupported(kind, reason)) => (
            EXIT_FAILURE,
            format!("cannot dump: unsupported object: {kind}: {reason}"),
        ),
        Err(DumpError::Catalog) => (EXIT_FAILURE, "failed to read the catalog".into()),
        Err(DumpError::Generation) => {
            (EXIT_FAILURE, "failed to read the restore generation".into())
        }
        Err(DumpError::Write(e)) => (EXIT_FAILURE, e.message().into()),
        Err(DumpError::Tmp(TmpError::InvalidDestination)) => (
            EXIT_USAGE,
            "--out already exists or its parent directory is missing; refusing to overwrite".into(),
        ),
        Err(DumpError::Tmp(TmpError::PublishedButNotFinalized)) => (
            EXIT_PUBLISHED_UNVERIFIED,
            "WARNING: the dump file was published but its durability could not be confirmed; \
             verify the output file before relying on it"
                .into(),
        ),
        Err(DumpError::Tmp(TmpError::UncheckedContentPublished)) => (
            EXIT_FAILURE,
            "WARNING: the output path holds content that was not written by this command; \
             delete it manually"
                .into(),
        ),
        Err(DumpError::Tmp(_)) => (EXIT_FAILURE, "failed to write the dump file".into()),
    }
}

fn scope_of(args: &DumpArgs) -> SchemaDdlScope {
    match &args.scope {
        DumpScope::Shared => SchemaDdlScope::shared_only(),
        DumpScope::Tenant(t) => SchemaDdlScope::shared_and_tenant(t.clone()),
    }
}

/// 書き手へ 1 件渡す。データ断片の末尾の `;`・改行は書き手の枠が足す。
fn emit<W: std::io::Write>(
    w: &mut FileWriter<W>,
    item: &LogicalDumpItem<'_>,
) -> Result<(), FileError> {
    match item {
        LogicalDumpItem::Schema(st) => w.statement(StmtKind::from_ddl(st.kind), st.sql),
        LogicalDumpItem::Data(d) => match d.part {
            TableDataPart::CopyHeader => w.copy_header(d.text),
            TableDataPart::CopyRow => w.copy_row(d.text),
            TableDataPart::CopyEnd => w.copy_end(),
            TableDataPart::Insert => w.statement(StmtKind::Insert, d.text),
        },
    }
}

fn map_engine_error(e: SchemaDdlError<FileError>) -> DumpError {
    match e {
        SchemaDdlError::Unsupported(u) => DumpError::Unsupported(u.kind, u.reason),
        SchemaDdlError::Catalog(_) => DumpError::Catalog,
        SchemaDdlError::Sink(f) => DumpError::Write(f),
    }
}

/// 停止中の DB を開き、ダンプを一時名へ書いて、成功したときだけ `args.out` へ公開する。
/// 一時名を消せなかった場合は `leftover` にそのパスを入れる（失敗の種別は変えない）。
pub fn dump(args: &DumpArgs, leftover: &mut Option<PathBuf>) -> Result<DumpSummary, DumpError> {
    let parent = parent_of(&args.out).map_err(DumpError::Tmp)?;
    // 早期失敗のための確認。最終判定は公開時の hard_link（確認後の競合を残さない）。
    if std::fs::symlink_metadata(&args.out).is_ok() {
        return Err(DumpError::Tmp(TmpError::InvalidDestination));
    }
    let storage = Storage::open(&args.db).map_err(|e| {
        if crate::backup_opt::is_already_open(&e) {
            DumpError::InUse
        } else {
            DumpError::OpenFailed
        }
    })?;
    // 停止中の DB を排他で開いているので、世代（書くのは閉じた復元先だけ）はこの読み取りの後に
    // 変わらない。読めなければ何も作らずに失敗する（fail-closed。Issue #2451）。
    let restore_generation = storage
        .restore_generation()
        .map_err(|_| DumpError::Generation)?;
    // マテリアライズド VIEW の列構成の検査（Issue #3097）は SQL 層を持つ `EngineCore` の入口にある。
    let core = EngineCore::from_storage(storage, engine::search_engine::default_engine());
    let mut tmp = TempFile::create(parent, "fandhe-dump").map_err(DumpError::Tmp)?;
    let result = write_and_publish(&core, args, &mut tmp, restore_generation);
    if let Err(p) = tmp.remove() {
        // 公開済みなら armed は false で何もしない。公開しなかった経路で消せなかったときだけ通知する。
        *leftover = Some(p);
    }
    result
}

fn write_and_publish(
    core: &EngineCore,
    args: &DumpArgs,
    tmp: &mut TempFile,
    restore_generation: u64,
) -> Result<DumpSummary, DumpError> {
    let header = Header {
        scope: args.scope.clone(),
        parts: args.parts,
        format: args.format,
        restore_generation,
    };
    let out = BufWriter::new(&tmp.file);
    let mut writer = FileWriter::begin(out, &header).map_err(DumpError::Write)?;
    let scope = scope_of(args);
    let format = match args.format {
        DumpFormat::Copy => TableDataFormat::Copy,
        DumpFormat::Insert => TableDataFormat::Insert,
    };
    let summary = match args.parts {
        DumpParts::All => {
            let s = core
                .export_logical_dump(&scope, format, |item| emit(&mut writer, item))
                .map_err(map_engine_error)?;
            DumpSummary {
                statements: s.schema.statements,
                tables: s.data.tables,
                rows: s.data.rows,
            }
        }
        DumpParts::SchemaOnly => {
            let s = core
                .export_schema_ddl(&scope, |st| {
                    emit(&mut writer, &LogicalDumpItem::Schema(*st))
                })
                .map_err(map_engine_error)?;
            DumpSummary {
                statements: s.statements,
                ..DumpSummary::default()
            }
        }
        DumpParts::DataOnly => {
            let s = core
                .export_table_data(&scope, format, |d| {
                    emit(&mut writer, &LogicalDumpItem::Data(*d))
                })
                .map_err(map_engine_error)?;
            DumpSummary {
                statements: 0,
                tables: s.tables,
                rows: s.rows,
            }
        }
    };
    // 書き手を閉じる（終端行を書いてバッファを流す）。出力先は保持 fd 経由なので drop でよい。
    writer.finish().map_err(DumpError::Write)?;
    tmp.publish(&args.out).map_err(DumpError::Tmp)?;
    Ok(summary)
}
