//! カタログから DDL 文を再生成して書き出す（論理ダンプのスキーマ側。RECOVER-16 (1)・Issue #2160。
//! ポインタ: `docs/spec/04-behavior/recovery.md` の RECOVER-16、設計メモ: `docs/design/logical-dump.md`）。
//!
//! 責務境界: 一貫した読み取りスナップショット 1 つの上でカタログを走査し、オブジェクトを依存順の
//! DDL 文として 1 文ずつシンクへ渡す（[`Storage::export_schema_ddl`]）。データ行の書き出し（#2161）と
//! CLI（#2162）は本モジュールの外にあり、`PreData` → データ → `PostData` の順に組み合わせて使う。
//! 書き出した文は本リポの SQL 表層（`EngineCore::execute_sql_in_session`）がそのまま受理する形に限る。
//!
//! # 出力の区分（[`DdlSection`]）
//!
//! - `PreData`: スキーマ・ロール・ENUM・SEQUENCE・表（列・既定値・PRIMARY KEY・UNIQUE・CHECK）・VIEW・関数
//! - `PostData`: 外部キー・索引・トリガ・行セキュリティ・ポリシー・権限・SEQUENCE の現在値
//!
//! データを入れる前に制約・トリガ・ポリシーが効くとデータの流し込み自体が阻害されるため、`pg_dump` と同じく
//! それらはデータの後ろ（`PostData`）に置く。
//!
//! # テナントの扱い（P0）
//!
//! 表・ビュー・索引・ENUM・SEQUENCE の定義は DB 全体で共有（`public`）される一方、SCHEMA・ロール・関数・
//! トリガ・ポリシー・行セキュリティ・権限・SEQUENCE の現在値はテナントごとに持つ（キーの先頭が
//! テナント）。そのため [`SchemaDdlScope`] は「共有分を出すか」と「どのテナント分を出すか」を取る。
//! 他テナントの SCHEMA 配下の relation は存在を示す出力・エラーを一切作らず、黙って対象外にする。
//!
//! # 書き出せないものは失敗させる（fail-closed）
//!
//! DDL へ戻す手段が無い種別・属性は省かず、[`SchemaDdlError::Unsupported`] で全体を失敗させる。
//! エラー文言は固定文字列で、オブジェクト名・テナント名・内部 ID を含めない。未知の redb 表
//! （どの種別にも分類されていないカタログ表）も同様に失敗させる（[`classify_redb_table`]）。

mod access;
mod data;
mod object;
mod quote;
mod table;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;

use redb::{ReadableDatabase, TableHandle};

use super::namespace::schema_names_for_tenant_in_txn;
use super::{CatalogError, Storage};
use quote::Namer;

pub use data::{TableDataFormat, TableDataItem, TableDataPart, TableDataSummary};

/// 書き出す範囲。共有分（`public` の表・ビュー・索引・ENUM・SEQUENCE の定義）に、指定したテナント分
/// （SCHEMA・ロール・関数・トリガ・ポリシー・権限・SEQUENCE の現在値）を足す。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaDdlScope {
    shared: bool,
    tenant: Option<String>,
}

impl SchemaDdlScope {
    /// DB 全体で共有される分だけ（テナント固有のオブジェクトは出さない）。
    pub fn shared_only() -> Self {
        Self {
            shared: true,
            tenant: None,
        }
    }

    /// 共有分に加えて `tenant` のテナント固有分を出す（1 テナントのダンプはこれ）。
    pub fn shared_and_tenant(tenant: impl Into<String>) -> Self {
        Self {
            shared: true,
            tenant: Some(tenant.into()),
        }
    }

    /// `tenant` のテナント固有分だけを出す（共有分は別に [`Self::shared_only`] で出した場合に、
    /// 複数テナントを続けて書き出すために使う）。
    pub fn tenant_only(tenant: impl Into<String>) -> Self {
        Self {
            shared: false,
            tenant: Some(tenant.into()),
        }
    }

    /// 共有分を出すか。
    pub fn includes_shared(&self) -> bool {
        self.shared
    }

    /// 出力対象のテナント（無ければ `None`）。
    pub fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }
}

/// 出力する文の区分。呼び出し側は `PreData` → データ → `PostData` の順に組み合わせる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DdlSection {
    /// データを入れる前に必要な文（オブジェクトの作成）。
    PreData,
    /// データを入れた後に実行する文（制約・索引・トリガ・ポリシー・権限・採番の現在値）。
    PostData,
}

/// 出力する文が作る・設定するオブジェクトの種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DdlObjectKind {
    /// `CREATE SCHEMA`。
    Schema,
    /// `CREATE ROLE`。
    Role,
    /// ロールのメンバーシップ（`GRANT <role> TO <member>`）。
    RoleMembership,
    /// `CREATE TYPE ... AS ENUM`。
    EnumType,
    /// `CREATE SEQUENCE`（`ALTER SEQUENCE ... RESTART` を含む）。
    Sequence,
    /// `SELECT setval(...)`（SEQUENCE の現在値）。
    SequenceValue,
    /// `CREATE TABLE`。
    Table,
    /// 外部キー制約（`ALTER TABLE ... ADD CONSTRAINT ... FOREIGN KEY`）。
    ForeignKey,
    /// `CREATE [UNIQUE] INDEX`。
    Index,
    /// `CREATE VIEW`。
    View,
    /// `CREATE FUNCTION`。
    Function,
    /// `CREATE TRIGGER`。
    Trigger,
    /// 行セキュリティの有効化（`ALTER TABLE ... ENABLE／FORCE ROW LEVEL SECURITY`）。
    RowSecurity,
    /// `CREATE POLICY`。
    Policy,
    /// `GRANT`。
    Privilege,
}

/// シンクへ渡す DDL 文 1 件。`sql` は 1 文（末尾の `;` を含まない）。
///
/// `sql` にはロールの SCRAM 検証子など秘匿値が入りうるため、`Debug` は `sql` を伏せる（ログに出さない）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct DdlStatement<'a> {
    /// 出力の区分。
    pub section: DdlSection,
    /// 作る・設定するオブジェクトの種別。
    pub kind: DdlObjectKind,
    /// 文本体（`;` なし）。
    pub sql: &'a str,
}

impl fmt::Debug for DdlStatement<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DdlStatement")
            .field("section", &self.section)
            .field("kind", &self.kind)
            .field("sql", &format_args!("<{} bytes redacted>", self.sql.len()))
            .finish()
    }
}

/// 書き出しの件数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SchemaDdlSummary {
    /// シンクへ渡した文の総数。
    pub statements: u64,
}

/// 書き出せないオブジェクト（種別と理由。いずれも固定文字列で、名前・テナント・内部 ID を含まない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnsupportedSchemaObject {
    /// オブジェクトの種別（固定文字列）。
    pub kind: &'static str,
    /// 書き出せない理由（固定文字列）。
    pub reason: &'static str,
}

/// [`Storage::export_schema_ddl`] の失敗。`E` はシンクが返すエラー型。
#[derive(Debug)]
pub enum SchemaDdlError<E> {
    /// DDL へ戻す手段が無い種別・属性があった。ダンプ全体を失敗させる（RECOVER-16 (1)）。
    Unsupported(UnsupportedSchemaObject),
    /// カタログの読み取りの失敗（破損・I/O）。
    Catalog(CatalogError),
    /// シンクが失敗を返した。
    Sink(E),
}

impl<E: fmt::Display> fmt::Display for SchemaDdlError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchemaDdlError::Unsupported(u) => {
                write!(f, "cannot dump schema: {}: {}", u.kind, u.reason)
            }
            SchemaDdlError::Catalog(e) => write!(f, "cannot read catalog: {e}"),
            SchemaDdlError::Sink(e) => write!(f, "cannot write dump: {e}"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for SchemaDdlError<E> {}

impl<E> From<CatalogError> for SchemaDdlError<E> {
    fn from(e: CatalogError) -> Self {
        SchemaDdlError::Catalog(e)
    }
}

impl<E> From<redb::Error> for SchemaDdlError<E> {
    fn from(e: redb::Error) -> Self {
        SchemaDdlError::Catalog(CatalogError::Backend(e))
    }
}

/// 書き出せないオブジェクトのエラーを作る。
fn unsupported<E>(kind: &'static str, reason: &'static str) -> SchemaDdlError<E> {
    SchemaDdlError::Unsupported(UnsupportedSchemaObject { kind, reason })
}

/// redb の表を分類した結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RedbTableClass {
    /// 本モジュールが DDL として書き出す種別のカタログ表。
    SchemaCatalog,
    /// DDL の対象ではない表（行データ・索引の物理表現・台帳・世代・カウンタ・キャッシュ等）。
    NotSchema,
}

/// 書き出す種別のカタログ表（名前の完全一致）。ここに足さない新しいカタログ表は
/// [`classify_redb_table`] が未分類として失敗させる。
const SCHEMA_CATALOG_TABLES: &[&str] = &[
    "catalog",
    "enum_types",
    "views",
    "index_catalog",
    "sequences",
    "schemas",
    "roles",
    "role_memberships",
    "functions",
    "triggers",
    "policies",
    "table_row_security",
    "acl",
    "default_acl",
    "object_owners",
    "materialized_views",
];

/// DDL の対象ではない表（名前の完全一致）。行データそのもの・台帳・世代・カウンタ・導出表。
const NOT_SCHEMA_TABLES: &[&str] = &[
    "rows",
    "batch_log",
    "storage_generation",
    "table_generation",
    "index_catalog_generation",
    "schema_id_counter",
    "role_id_counter",
    "policy_id_counter",
    "trigger_id_counter",
    "function_id_counter",
    "sequence_id_counter",
    "sequence_state",
    "rowid_state",
    "role_login_names",
    "role_bootstrap_markers",
    "role_bootstrap_privileges",
    "acl_init",
    "default_acl_init",
    "function_deps_by_dependee",
    "function_deps_by_function",
    "matview_entities",
    "key_index_registry",
    "op_ledger",
    "last_op",
    "partitioned_dml_job",
    "partitioned_dml_job_active",
    "change_log",
    "change_log_meta",
    "restore_generation",
];

/// DDL の対象ではない動的な表の名前接頭辞（行ストア・索引の物理表現）。
const NOT_SCHEMA_TABLE_PREFIXES: &[&str] = &[
    "user_rows/",
    "user_uniq/",
    "user_gin/",
    "key_index/",
    "key_index_rev/",
    "matview_rows/",
];

/// redb の表名を分類する。どちらにも当てはまらない名前は `None`（未知のカタログ種別。呼び出し元は
/// 書き出しを失敗させる）。
fn classify_redb_table(name: &str) -> Option<RedbTableClass> {
    if SCHEMA_CATALOG_TABLES.contains(&name) {
        return Some(RedbTableClass::SchemaCatalog);
    }
    if NOT_SCHEMA_TABLES.contains(&name)
        || NOT_SCHEMA_TABLE_PREFIXES
            .iter()
            .any(|p| name.starts_with(p))
    {
        return Some(RedbTableClass::NotSchema);
    }
    None
}

/// シンクへ文を渡す出口（件数を数える）。
struct Emitter<'a, E> {
    sink: &'a mut dyn FnMut(&DdlStatement<'_>) -> Result<(), E>,
    count: u64,
}

impl<E> Emitter<'_, E> {
    /// 文を 1 件渡す。
    fn emit(
        &mut self,
        section: DdlSection,
        kind: DdlObjectKind,
        sql: &str,
    ) -> Result<(), SchemaDdlError<E>> {
        (self.sink)(&DdlStatement { section, kind, sql }).map_err(SchemaDdlError::Sink)?;
        self.count = self.count.saturating_add(1);
        Ok(())
    }
}

/// 書き出しの文脈（読み取りスナップショット・範囲・SCHEMA 名の対応）。
struct Ctx<'a> {
    rt: &'a redb::ReadTransaction,
    scope: &'a SchemaDdlScope,
    namer: Namer,
}

impl Storage {
    /// カタログから DDL 文を依存順に再生成し、`sink` へ 1 文ずつ渡す（RECOVER-16 (1)）。
    ///
    /// 読み取りトランザクション 1 つの上で実行するため、書き込みを止めず、全文が単一の commit 点の
    /// 内容に対応する（方式: `docs/design/backup-snapshot.md`）。文は `PreData` をすべて、続けて
    /// `PostData` をすべて渡す。書き出せないオブジェクトがあれば [`SchemaDdlError::Unsupported`] を返す
    /// （それまでに渡した文は有効だが、呼び出し側は失敗として扱い出力を破棄すること）。
    /// `sink` が `Err` を返した場合はそこで中止して [`SchemaDdlError::Sink`] を返す。
    ///
    /// 運用者の CLI（#2162）専用の入口で、テナントの認可は呼び出し側（運用者の操作）が担う。
    pub fn export_schema_ddl<E>(
        &self,
        scope: &SchemaDdlScope,
        mut sink: impl FnMut(&DdlStatement<'_>) -> Result<(), E>,
    ) -> Result<SchemaDdlSummary, SchemaDdlError<E>> {
        if let Some(t) = scope.tenant() {
            super::role::validate_tenant(t).map_err(SchemaDdlError::Catalog)?;
        }
        let rt = self.db().begin_read().map_err(CatalogError::from)?;
        ensure_all_tables_classified(&rt)?;
        let namer = Namer {
            schemas: match scope.tenant() {
                Some(t) => schema_names_for_tenant_in_txn(&rt, t)?,
                None => BTreeMap::new(),
            },
        };
        let ctx = Ctx {
            rt: &rt,
            scope,
            namer,
        };
        let mut out = Emitter {
            sink: &mut sink,
            count: 0,
        };
        object::export_pre_data(&ctx, &mut out)?;
        object::export_post_data(&ctx, &mut out)?;
        Ok(SchemaDdlSummary {
            statements: out.count,
        })
    }
}

/// [`Storage::export_logical_dump`] がシンクへ渡す断片（スキーマの文またはデータ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalDumpItem<'a> {
    /// DDL 文 1 件（`PreData` の全件、データの後に `PostData` の全件の順）。
    Schema(DdlStatement<'a>),
    /// 表データの断片 1 件（`PreData` と `PostData` の間）。
    Data(TableDataItem<'a>),
}

/// [`Storage::export_logical_dump`] の件数。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogicalDumpSummary {
    /// 書き出した DDL 文の数。
    pub schema: SchemaDdlSummary,
    /// 書き出したデータの件数。
    pub data: TableDataSummary,
}

impl Storage {
    /// スキーマ（`PreData`）・表データ・スキーマ（`PostData`）を、読み取りトランザクション 1 つの上で
    /// この順に `sink` へ渡す（RECOVER-16 (1)(2)）。全体が単一の commit 点の内容に対応するため、
    /// 定義とデータが食い違わない。契約・失敗時の扱いは [`Storage::export_schema_ddl`]・
    /// [`Storage::export_table_data`] と同じ（一部でも失敗したら出力全体を破棄すること）。
    ///
    /// 運用者の CLI（#2162）専用の入口で、テナントの認可は呼び出し側が担う。
    pub fn export_logical_dump<E>(
        &self,
        scope: &SchemaDdlScope,
        format: TableDataFormat,
        mut sink: impl FnMut(&LogicalDumpItem<'_>) -> Result<(), E>,
    ) -> Result<LogicalDumpSummary, SchemaDdlError<E>> {
        if let Some(t) = scope.tenant() {
            super::role::validate_tenant(t).map_err(SchemaDdlError::Catalog)?;
        }
        let rt = self.db().begin_read().map_err(CatalogError::from)?;
        ensure_all_tables_classified(&rt)?;
        let namer = Namer {
            schemas: match scope.tenant() {
                Some(t) => schema_names_for_tenant_in_txn(&rt, t)?,
                None => BTreeMap::new(),
            },
        };
        let ctx = Ctx {
            rt: &rt,
            scope,
            namer,
        };
        let mut schema_count: u64 = 0;
        {
            let mut schema_sink = |st: &DdlStatement<'_>| sink(&LogicalDumpItem::Schema(*st));
            let mut out = Emitter {
                sink: &mut schema_sink,
                count: 0,
            };
            object::export_pre_data(&ctx, &mut out)?;
            schema_count = schema_count.saturating_add(out.count);
        }
        let mut data_sink = |it: &TableDataItem<'_>| sink(&LogicalDumpItem::Data(*it));
        let data = data::export_table_data_in(&rt, scope, format, &mut data_sink)?;
        {
            let mut schema_sink = |st: &DdlStatement<'_>| sink(&LogicalDumpItem::Schema(*st));
            let mut out = Emitter {
                sink: &mut schema_sink,
                count: 0,
            };
            object::export_post_data(&ctx, &mut out)?;
            schema_count = schema_count.saturating_add(out.count);
        }
        Ok(LogicalDumpSummary {
            schema: SchemaDdlSummary {
                statements: schema_count,
            },
            data,
        })
    }
}

/// スナップショット内のすべての redb 表が分類済みであることを確かめる（未知の表・multimap 表があれば
/// 失敗）。新しいカタログ表を足した変更が、書き出しの対応を忘れたまま黙ってダンプから欠落する
/// ことを防ぐ。
fn ensure_all_tables_classified<E>(rt: &redb::ReadTransaction) -> Result<(), SchemaDdlError<E>> {
    for handle in rt.list_tables().map_err(CatalogError::from)? {
        if classify_redb_table(handle.name()).is_none() {
            return Err(unsupported(
                "catalog",
                "unclassified storage table (schema dump does not know this kind)",
            ));
        }
    }
    let mut multimaps = rt.list_multimap_tables().map_err(CatalogError::from)?;
    if multimaps.next().is_some() {
        return Err(unsupported("catalog", "multimap storage tables"));
    }
    Ok(())
}
