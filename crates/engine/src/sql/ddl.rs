//! DDL（`CREATE TABLE`・`DROP TABLE`・`ALTER TABLE ADD COLUMN`・`CREATE INDEX` 等）の
//! 実行権限ゲートと実行本体（SQL-23・TASK-85・TASK-202・TASK-203・TASK-205・
//! TASK-206、Issue #899・#900・#902・#909・#908）。
//!
//! 責務境界: `core.rs::EngineCore::execute_parsed_in_session` の
//! `ParsedSql::CreateTable`／`ParsedSql::DropTable`／`ParsedSql::AlterTable`
//! 分岐から呼ばれる唯一の
//! 入口として、DDL 実行権限の判定（[`require_ddl_permission`]。全 DDL 文が
//! 通る単一の判定点。#900・#901・#907〜#909 の `ALTER TABLE`／`VIEW`／
//! `FOREIGN KEY` 等もここを経由する想定）と、`crate::catalog::Storage::
//! create_table`／`drop_table`（既存のカタログ反映・削除本体。いずれも
//! `PolicyContext` を取らない全テナント対象の DDL）を SQL 表層の
//! [`crate::sql::allowlist::SqlSurfaceError`] 契約へ写像する実行本体
//! （[`execute_create_table`]・[`execute_drop_table`]）、および
//! `crate::catalog::Storage::alter_table_add_column`（TABLE-5。O(1) 列追加・
//! 既存行のバイト列を変えない）へ型名解決のうえ委譲する
//! [`execute_alter_table_add_column`]（Issue #900）を担う。構文の許可
//! リスト判定は `sql::allowlist` の管轄、ディスパッチ（先頭トークンの
//! 覗き見・権限ゲートの呼び出し順序）は `core.rs` の管轄。
//!
//! **判定順序（fail-closed。決定的）**: 構文検証
//! （`sql::allowlist::validate_create_table_tokens`／
//! `validate_drop_table_tokens`／`validate_alter_table_tokens`。いずれも
//! カタログ照会を一切行わない）→
//! [`require_ddl_permission`]（`42501`。カタログ照会なし・書き込み
//! トランザクション未開始）→ 実行本体（[`execute_create_table`]／
//! [`execute_drop_table`]／[`execute_alter_table_add_column`]。書き込み
//! トランザクション内で対象テーブル・列の存在・重複を判定。`42P07`／`42P01`／
//! `42701`）。権限を持たない主体には対象
//! テーブルの有無を問わず常に `42501` を返し、DDL 権限をテーブル存在の
//! オラクルにしない（security.md「エラー・ログ経由で他テナントの
//! データ・存在情報を漏らさない」対応）。
//!
//! [`crate::policy::PolicyContext`] はテナント ID と可視性のみを運び認証主体を
//! 持たないため、DDL 権限は [`crate::sql::mode::SessionState::ddl_allowed`]
//! （接続単位）として持ち、wire-server の handshake が認証成功後に 1 回だけ
//! 付与する（`SessionState::allow_ddl` ドキュメント参照）。テナント境界とは
//! 別軸の権限であり（テーブル・カタログは全テナント共有）、RLS の判定を
//! 一切変更しない。

use crate::catalog::{
    AlterCheckError, CatalogError, ColumnDef, ColumnDefault, ColumnType, IndexDef, IndexKind,
    TableSchema, MAX_COLUMN_DEFAULT_LEN,
};
use crate::sql::allowlist::{
    InsertLiteral, SqlSurfaceError, ValidatedAlterTableAddCheck, ValidatedAlterTableAddColumn,
    ValidatedAlterTableAddUnique, ValidatedAlterTableAlterColumnType,
    ValidatedAlterTableDropColumn, ValidatedAlterTableDropConstraint, ValidatedCreateIndex,
    ValidatedCreateTable, ValidatedCreateType, ValidatedCreateView, ValidatedDropIndex,
    ValidatedDropTable, ValidatedDropType, ValidatedDropView,
};
use crate::sql::ddl_column_type::SqlColumnTypeName;
use crate::sql::mode::SessionState;
use crate::storage::Storage;

/// DDL 実行権限の唯一の判定点。`session.ddl_allowed()` が `false`（既定）の
/// 場合、カタログ照会・書き込みトランザクション開始のいずれよりも前に
/// [`SqlSurfaceError::InsufficientPrivilege`]（`42501`）へ fail-closed に落とす。
pub(crate) fn require_ddl_permission(session: &SessionState) -> Result<(), SqlSurfaceError> {
    if session.ddl_allowed() {
        Ok(())
    } else {
        Err(SqlSurfaceError::InsufficientPrivilege)
    }
}

/// `CREATE TABLE`（SQL-23・TASK-85）の成功応答。行数・件数のいずれも返さない
/// （[`DropTableOutcome`] と同じ設計。DDL はテナント横断の共有資源
///〔カタログ〕を変更するのみで、テナント固有の件数概念を持たない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CreateTableOutcome {}

/// 構造検証済みの `CREATE TABLE`（[`ValidatedCreateTable`]）をカタログへ反映する
/// （SQL-23・TASK-85・TABLE-4）。実行本体は既存の [`Storage::create_table`]
/// （単一 write トランザクション内で存在確認・挿入・世代進行・commit を行う。
/// TOCTOU なし）にそのまま委譲し、第 2 の DDL 実行器を作らない。
///
/// `CatalogError` の写像:
/// - `TableAlreadyExists` → [`SqlSurfaceError::DuplicateTable`]（`42P07`）
/// - `Invalid`（列数・`VECTOR` 列複数宣言等の意味論的不正。列数・識別子形状の
///   大半は [`crate::sql::allowlist::validate_create_table_tokens`] が構造検証
///   段階で既に拒否済みのため、ここに到達するのは主に `VECTOR` の次元範囲・
///   複数 `VECTOR` 列宣言）→ [`SqlSurfaceError::unsupported`]（`42601`）
/// - `WriteLockTimeout`（書き込みゲートの待機上限超過。SQL-31）→
///   `SqlSurfaceError::LockNotAvailable`（`55P03`）
/// - その他（`redb` I/O 等）→ `SqlSurfaceError::Internal`（`XX000`。詳細を
///   クライアントへ渡さない。security.md「情報漏えい」対応）
pub(crate) fn execute_create_table(
    storage: &Storage,
    validated: &ValidatedCreateTable,
) -> Result<CreateTableOutcome, SqlSurfaceError> {
    let mut schema = TableSchema::new(validated.table_name.clone(), validated.columns.clone());
    // `PRIMARY KEY`（TABLE-16・TASK-204、Issue #903）。`validated.primary_key` は
    // `sql::allowlist::finalize_primary_key` が `id` 単独宣言を `None` へ既に
    // 正規化済みのため、ここでは素通しするだけでよい。
    if let Some(primary_key) = validated.primary_key.clone() {
        schema = schema.with_primary_key(primary_key);
    }
    // UNIQUE 制約（TABLE-16・TASK-204、Issue #905）。参照列の実在・型適格性は
    // `sql::allowlist` が構造検証段階で判定済みで、`catalog::validate_schema`
    // （`create_table` 内）が同じ不変条件を再検証する（違反は `Invalid` として
    // 下記の `42601` へ写像される）。
    if !validated.unique_constraints.is_empty() {
        schema = schema.with_unique_constraints(validated.unique_constraints.clone());
    }
    // `CHECK` 制約（TABLE-16・TASK-204、Issue #906）。意味論検証（列の存在・型・
    // 禁止要素・正規化の往復一致）は列定義だけを持つスキーマに対して行う（他の
    // `CHECK` を参照する `CHECK` は許可しない）。カタログを参照しないため、
    // テーブル名の既存衝突（`42P07`）より先に判定しても存在オラクルにならない。
    if !validated.checks.is_empty() {
        let checks = crate::sql::check_constraint::validate_and_build(&schema, &validated.checks)?;
        schema = schema.with_checks(checks);
    }
    // `FOREIGN KEY`（TABLE-17・TASK-205、Issue #907）。参照先の名前解決・主キー／
    // UNIQUE 制約との照合・型の照合はカタログ参照を要するため、`Storage::create_table`
    // の write トランザクション内（テーブル名重複判定の後）で行う（TOCTOU 回避）。
    if !validated.foreign_keys.is_empty() {
        schema = schema.with_foreign_keys(validated.foreign_keys.clone());
    }
    storage.create_table(&schema).map_err(|e| match e {
        CatalogError::TableAlreadyExists(name) => SqlSurfaceError::duplicate_table(name),
        // `FOREIGN KEY` の参照先（TABLE-17・TASK-205、Issue #907）。`create_table` が
        // `TableNotFound` を返すのは参照先テーブルの不在のみ（作成対象自身の重複は
        // 上の `TableAlreadyExists` が先に判定する）。参照先名がビュー・索引名なら
        // 種別不一致（`42809`）、参照先列が一意キーと一致しない・型不一致は `42830`。
        CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::InvalidForeignKey(detail) => SqlSurfaceError::invalid_foreign_key(detail),
        CatalogError::Invalid(detail) => {
            SqlSurfaceError::unsupported(format!("invalid table schema: {detail}"))
        }
        // 明示トランザクション（SQL-31・TASK-221）が単一ライタを保持中で書き込み
        // ゲートの待機上限を超えた。他の書き込み入口と同じく `55P03` を返す。
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        CatalogError::Backend(_)
        | CatalogError::CorruptSchema(_)
        | CatalogError::ColumnAlreadyExists(_)
        | CatalogError::RowNotFound(_)
        | CatalogError::IncompatibleRowKeyFormat
        | CatalogError::TableGenerationCounterOverflow
        | CatalogError::TypeNotFound(_)
        | CatalogError::TypeAlreadyExists(_)
        | CatalogError::DependentObjectsStillExist(_)
        // `ColumnNotFound`／`ProtectedColumn`／`IncompatibleTypeChange` は
        // `ALTER TABLE`（Issue #901・`Storage::alter_table_drop_column` 等）
        // 専用の変種で、`Storage::create_table` からは返らない（到達不能）。
        | CatalogError::ColumnNotFound(_)
        | CatalogError::ProtectedColumn(_)
        | CatalogError::IncompatibleTypeChange { .. }
        // `ViewNotFound`／`DependentViewsExist`／`ViewLimitExceeded` は
        // `CREATE VIEW`／`DROP VIEW`（TABLE-18・SQL-23・TASK-205、Issue #909）
        // 専用の変種で、`Storage::create_table` からは返らない（到達不能）。
        | CatalogError::ViewNotFound(_)
        | CatalogError::DependentViewsExist(_)
        | CatalogError::ViewLimitExceeded(_)
        // `TooManyColumns` は `ALTER TABLE ADD COLUMN`（Issue #900・
        // `Storage::alter_table_add_column`）専用の変種で、`Storage::create_table`
        // からは返らない（列数上限は `validate_create_table_tokens` が構造検証
        // 段階で `54000` として既に拒否する。到達不能）。
        | CatalogError::TooManyColumns { .. }
        // `UniqueConstraintViolation` は `Storage::alter_table_add_unique_constraint`
        // 専用（既存行の走査結果）で、`create_table`（新規テーブル・既存行なし）
        // からは返らない（到達不能）。
        | CatalogError::UniqueConstraintViolation
        // 索引宣言（TASK-206・INDEX-7、Issue #908）専用の変種で、
        // `Storage::create_table` からは返らない（到達不能）。索引名との衝突は
        // `TableAlreadyExists` として返る。
        | CatalogError::IndexAlreadyExists(_)
        | CatalogError::IndexNotFound(_)
        | CatalogError::IndexKindMismatch(_)
        | CatalogError::IndexLimitExceeded(_)
        // 制約名の操作（`ALTER TABLE ... ADD/DROP CONSTRAINT`。Issue #1067）専用の
        // 変種で、`Storage::create_table` からは返らない（到達不能。`CREATE TABLE`
        // での明示制約名指定はスコープ外〔設計 D6〕であり、`allowlist` が常に
        // 未確定名の `UniqueConstraint` を渡す）。
        | CatalogError::ConstraintAlreadyExists(_)
        | CatalogError::ConstraintNotFound(_)
        | CatalogError::ConstraintLimitExceeded(_)
        // `ForeignKeyViolation` は `Storage::alter_table_add_foreign_key`
        // 専用（既存行の走査結果。TABLE-22・TASK-233、Issue #1069）で、
        // `create_table`（新規テーブル・既存行なし）からは返らない（到達不能）。
        | CatalogError::ForeignKeyViolation
        | CatalogError::NotNullConstraintViolation(_) => SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        },
    })?;
    Ok(CreateTableOutcome {})
}

/// `DROP TABLE <table>`（SQL-23、TASK-203、Issue #902）の成功応答。
/// [`crate::sql::exec::TruncateOutcome`] と同じく削除件数（全テナント分の
/// 行数）を一切保持しないフィールドなし構造体——他テナントの行数を応答から
/// 推測できないようにする（RLS-9 と同じ設計判断。`DROP TABLE` はテナント
/// スコープの操作ではないため、そもそも「自テナント分の件数」という概念を
/// 持たない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropTableOutcome {}

/// `require_ddl_permission` を通過したセッションに限り呼ばれる実行本体。
/// `crate::catalog::Storage::drop_table`（カタログエントリ・全テナント行
/// ストア・`operation_id` 台帳エントリの単一 write txn 削除。テーブル単位
/// 世代 bump を含む）へ委譲し、`CatalogError` を SQL 表層の契約へ写像する。
///
/// 依存オブジェクト検査（`2BP01`。VIEW〔#909〕・FOREIGN KEY〔#907。TABLE-15・
/// TABLE-17〕からの参照）は `Storage::drop_table` 自身の write トランザクション内
/// （カタログエントリ削除の前）で判定し、[`map_drop_table_error`] が
/// `DependentViewsExist`／`DependentObjectsStillExist` を `2BP01` へ写像する。
pub(crate) fn execute_drop_table(
    storage: &Storage,
    validated: &ValidatedDropTable,
) -> Result<DropTableOutcome, SqlSurfaceError> {
    storage
        .drop_table(&validated.table_name)
        .map_err(map_drop_table_error)?;
    Ok(DropTableOutcome {})
}

/// `Storage::drop_table` の [`CatalogError`] を SQL 表層の契約へ写像する
/// （`catalog::table_lookup_error` と同じ役割分担だが、`TableNotFound` を
/// `Ok(false)` ではなく `42P01` 応答へ写像する点が異なるため独立させる）。
/// エラー文言にテナント・行内容・redb 内部詳細は含めない
/// （security.md「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」対応）。
fn map_drop_table_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        // `name` は字句解析済みの識別子（既に長さ上限の対象）で、`CatalogError`
        // 側もテナント・行内容を含まないため、`exec.rs::map_insert_write_error`
        // と同じくそのまま運んでよい。
        CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        // 識別子形式の不正（現状の呼び出し経路では構文検証済みの識別子しか
        // 渡らないため到達しないはずだが、`validate_identifier` の判定基準が
        // 将来変わった場合に備えて fail-closed に `42601` へ丸める）。
        CatalogError::Invalid(_) => {
            SqlSurfaceError::unsupported("malformed table reference in DROP TABLE")
        }
        // TABLE-18・SQL-23・TASK-205（Issue #909）: 対象名がビューだった場合
        // （`42809`）、対象テーブルをビューが参照している場合（`2BP01`）。
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::DependentViewsExist(name) => {
            SqlSurfaceError::DependentObjectsStillExist { name }
        }
        // 他テーブルの `FOREIGN KEY` が対象テーブルを参照している（TABLE-15・
        // TABLE-17・TASK-205、Issue #907）。カタログ情報のみによる判定で、`name` は
        // 対象テーブル名そのもの（参照元テーブル名・テナントデータは含めない）。
        CatalogError::DependentObjectsStillExist(name) => {
            SqlSurfaceError::DependentObjectsStillExist { name }
        }
        // 明示トランザクション（SQL-31・TASK-221）が単一ライタを保持中で、書き込み
        // ゲートの待機上限を超えた。他の書き込み入口と同じく `55P03` を返す
        // （`catalog::table_lookup_error` 系の写像と同じ契約）。
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        // それ以外（redb バックエンド障害・カタログ破損・世代カウンタ枯渇等）は
        // サーバー側の内部事象として `XX000` へ丸める（`Storage::drop_table` の
        // ドキュメントが一覧する他の `CatalogError` variant はいずれもこの
        // 経路には現れない設計だが、`_` 節で網羅する。detail は固定文言のみ）。
        _ => SqlSurfaceError::Internal {
            detail: "DROP TABLE failed".to_string(),
        },
    }
}

/// `ALTER TABLE ...`（SQL-23・TASK-202・TASK-204、Issue #900・#1067）の成功応答。
/// 返す情報は文に書かれたテーブル名と、確定した変更内容（[`AlterTableAction`]）に
/// 限定する（行数・テナント固有の件数は持たない。[`CreateTableOutcome`]・
/// [`DropTableOutcome`] と同じ設計）。`wire-server::simple_query` は中身を見ず
/// `CommandComplete "ALTER TABLE"` のみを返すため、本構造体の変更は wire 層に
/// 影響しない（**BREAKING CHANGE**〔Issue #1067〕: 旧 `column_name: String`
/// フィールドを `action: AlterTableAction` へ置き換えた。クレート外でこの構造体を
/// 直接構築・分解するコードは追随が必要）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlterTableOutcome {
    pub table_name: String,
    pub action: AlterTableAction,
}

/// [`AlterTableOutcome`] が運ぶ、確定した `ALTER TABLE` の変更内容
/// （Issue #900・#1067・#1068）。`AddConstraint`／`DropConstraint` の
/// `constraint_name` は確定済みの実名（`ADD` で名前省略時は設計 D2 の既定名。
/// カタログに `pg_constraint` 相当の照会手段が無いため、呼び出し元がこの応答で
/// 確定名を知る唯一の手段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlterTableAction {
    /// `ADD COLUMN`（TASK-202、Issue #900）。
    AddColumn { column_name: String },
    /// `ADD [CONSTRAINT <name>] UNIQUE (...)`（TABLE-16・TASK-204、Issue #1067）・
    /// `ADD [CONSTRAINT <name>] CHECK (<述語>)`（TABLE-16・TASK-204、
    /// Issue #1068）のいずれの成功も本 variant を共有する（新しい variant を
    /// 追加しない。呼び出し元は制約の種別を区別しない）。
    AddConstraint { constraint_name: String },
    /// `DROP CONSTRAINT <name>`（Issue #1067）。CHECK 名を指定した DROP も
    /// Issue #1068 以降は本 variant を返す（以前は `0A000` で拒否していた）。
    DropConstraint { constraint_name: String },
    /// `DROP COLUMN <col>`（TABLE-19、Issue #1167）。**BREAKING CHANGE**: variant 追加。
    DropColumn { column_name: String },
    /// `ALTER COLUMN <col> TYPE <型>`（TABLE-19、Issue #1167）。**BREAKING CHANGE**: variant 追加。
    AlterColumnType { column_name: String },
}

/// `ALTER TABLE <table> ADD COLUMN <column> <type>` の実行本体（Issue #900）。
/// 呼び出し元（`core.rs`）は [`require_ddl_permission`] を必ず先に呼んでいる
/// 前提（対象テーブルの存在確認・ENUM 型名解決はいずれもカタログ照会のため、
/// 権限ゲートより後に置く）。
///
/// 判定順序（決定的）:
/// 1. 対象テーブルの存在確認。存在しなければ、同名のビューがあれば `42809`
///    （ビューへの DDL・書き込みは非対応。`CREATE VIEW`〔Issue #909〕の書き込み
///    系と同じ扱い）、無ければ `42P01`。型名解決より先に行い、存在しない
///    テーブルへの要求が型名の不正（`42601`／`0A000`）で答えられないようにする。
/// 2. 型名解決: `VECTOR` は常に `0A000`（`SqlSurfaceError::FeatureNotSupported`。
///    `sql::ddl_column_type` モジュールドキュメント参照）、ENUM 型名候補は
///    `storage.get_enum_type` で存在確認（未登録は `42601`）、それ以外のスカラー
///    型はそのまま `catalog::ColumnType` へ変換する。
/// 3. `catalog::Storage::alter_table_add_column`（単一 write トランザクション内で
///    テーブルの存在・列数上限・列名重複を再確認。TOCTOU なし）。1. と 3. の間に
///    テーブルが削除された場合も 1. と同じ写像（`42P01`／`42809`）になる。
///
/// `NOT NULL`／`DEFAULT` を受け付ける（Issue #1169）。`DEFAULT` は列型との整合
/// （[`add_column_default`]）・値の束縛（`22000`／`22003`）を検証してから渡し、
/// 既存行は読み出し時に既定値として見える。`DEFAULT` なしの `NOT NULL` は行の
/// 有無によらず構造検証段階で一律 `42601` 拒否される（TABLE-16。`Storage` を直接呼ぶ
/// Rust API は `CatalogError::Invalid`。行ストアを参照せず他テナントの行の存在を漏らさない）。
/// `DEFAULT`・`NOT NULL` のいずれも無い列は従来どおり nullable（TABLE-5）。
pub(crate) fn execute_alter_table_add_column(
    storage: &Storage,
    stmt: &ValidatedAlterTableAddColumn,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    match storage.get_table_schema(&stmt.table_name) {
        Ok(_) => {}
        Err(CatalogError::TableNotFound(_)) => {
            return Err(undefined_table_or_view(storage, &stmt.table_name));
        }
        Err(other) => return Err(map_add_column_error(other)),
    }
    let ty = resolve_column_type(storage, &stmt.column_type)?;
    let default = match &stmt.default {
        None => None,
        Some(literal) => Some(add_column_default(&stmt.column_name, &ty, literal)?),
    };
    let mut column = ColumnDef::new(stmt.column_name.clone(), ty, !stmt.not_null);
    if let Some(default) = default {
        // 束縛できない DEFAULT（範囲外・桁数超過等）は永続化前に SQLSTATE 付きで拒否する
        // （既存行の読み出し時補完が常に失敗する状態をカタログへ残さない）。
        crate::sql::parser::bind_column_default(&column, &default)?;
        column = column.with_default(default);
    }
    storage
        .alter_table_add_column(&stmt.table_name, column)
        .map_err(|e| match e {
            CatalogError::TableNotFound(_) => undefined_table_or_view(storage, &stmt.table_name),
            other => map_add_column_error(other),
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::AddColumn {
            column_name: stmt.column_name.clone(),
        },
    })
}

/// `ALTER TABLE <table> ADD [CONSTRAINT <name>] UNIQUE (<col>[, ...])` の実行本体
/// （TABLE-16・TASK-204、Issue #1067）。呼び出し元（`core.rs`）は
/// [`require_ddl_permission`] を必ず先に呼んでいる前提。
///
/// 判定順序（決定的。設計 D7）:
/// 1. 対象テーブルの存在確認（`execute_alter_table_add_column` と同じ
///    `undefined_table_or_view` 判定。`42P01`／`42809`）
/// 2. `catalog::Storage::alter_table_add_named_unique_constraint`
///    （単一 write トランザクション内で名前衝突・件数上限・列存在・型・
///    既存行の重複を判定。TOCTOU なし）。
pub(crate) fn execute_alter_table_add_unique(
    storage: &Storage,
    stmt: &ValidatedAlterTableAddUnique,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    match storage.get_table_schema(&stmt.table_name) {
        Ok(_) => {}
        Err(CatalogError::TableNotFound(_)) => {
            return Err(undefined_table_or_view(storage, &stmt.table_name));
        }
        Err(other) => return Err(map_alter_constraint_error(other)),
    }
    let columns: Vec<&str> = stmt.columns.iter().map(|c| c.as_str()).collect();
    let confirmed_name = storage
        .alter_table_add_named_unique_constraint(
            &stmt.table_name,
            stmt.constraint_name.as_deref(),
            &columns,
        )
        .map_err(|e| match e {
            CatalogError::TableNotFound(_) => undefined_table_or_view(storage, &stmt.table_name),
            other => map_alter_constraint_error(other),
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::AddConstraint {
            constraint_name: confirmed_name,
        },
    })
}

/// `ALTER TABLE <table> ADD PRIMARY KEY (<col>[, ...])` の実行本体（TABLE-22 (a)(d)・
/// TASK-233、Issue #1196）。呼び出し元（`core.rs`）は [`require_ddl_permission`] を
/// 必ず先に呼んでいる前提（権限の無い主体への存在オラクル化を防ぐ）。
///
/// 判定順序（決定的）: テーブル存在確認（`42P01`／`42809`）→
/// `catalog::Storage::alter_table_add_primary_key`（単一 write txn 内で PK 宣言済み・
/// 擬似名衝突・スキーマ検証・全テナント既存行の NULL／重複検査を判定。TOCTOU なし）。
/// 応答の制約名は導出擬似名 `<table>_pkey`（カタログには永続化しない）。
pub(crate) fn execute_alter_table_add_primary_key(
    storage: &Storage,
    stmt: &crate::sql::allowlist::ValidatedAlterTableAddPrimaryKey,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    ensure_table_exists(storage, &stmt.table_name)?;
    let columns: Vec<&str> = stmt.columns.iter().map(|c| c.as_str()).collect();
    let confirmed_name = storage
        .alter_table_add_primary_key(&stmt.table_name, &columns)
        .map_err(|e| match e {
            CatalogError::TableNotFound(_) => undefined_table_or_view(storage, &stmt.table_name),
            other => map_alter_constraint_error(other),
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::AddConstraint {
            constraint_name: confirmed_name,
        },
    })
}

/// `ALTER TABLE <table> DROP CONSTRAINT <name>` の実行本体（Issue #1067）。
/// 呼び出し元（`core.rs`）は [`require_ddl_permission`] を必ず先に呼んでいる
/// 前提。判定順序は [`execute_alter_table_add_unique`] と同じ
/// テーブル存在確認 → 実行本体（`catalog::Storage::alter_table_drop_constraint`。
/// 設計 D7: 名前検索 → CHECK 名なら `0A000`／未検出なら `42704` → FK 依存検査
/// 〔`2BP01`〕）。
pub(crate) fn execute_alter_table_drop_constraint(
    storage: &Storage,
    stmt: &ValidatedAlterTableDropConstraint,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    match storage.get_table_schema(&stmt.table_name) {
        Ok(_) => {}
        Err(CatalogError::TableNotFound(_)) => {
            return Err(undefined_table_or_view(storage, &stmt.table_name));
        }
        Err(other) => return Err(map_alter_constraint_error(other)),
    }
    storage
        .alter_table_drop_constraint(&stmt.table_name, &stmt.constraint_name)
        .map_err(|e| match e {
            CatalogError::TableNotFound(_) => undefined_table_or_view(storage, &stmt.table_name),
            other => map_alter_constraint_error(other),
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::DropConstraint {
            constraint_name: stmt.constraint_name.clone(),
        },
    })
}

/// `ALTER TABLE <table> ADD [CONSTRAINT <name>] CHECK (<述語>)` の実行本体
/// （TABLE-16・TASK-204、Issue #1068）。呼び出し元（`core.rs`）は
/// [`require_ddl_permission`] を必ず先に呼んでいる前提。
///
/// 判定順序（決定的。設計 D5）:
/// 1. 対象テーブルの存在確認（`execute_alter_table_add_column` と同じ
///    `undefined_table_or_view` 判定。`42P01`／`42809`）
/// 2. `catalog::Storage::alter_table_add_check_constraint`（単一 write
///    トランザクション内で名前衝突・件数上限・意味論検証・既存行の全件走査を
///    判定。TOCTOU なし）。`AlterCheckError::Sql` は評価エラー・意味論検証
///    エラーの `SqlSurfaceError` をそのまま透過し、`AlterCheckError::Catalog`
///    は [`map_add_check_or_foreign_key_error`] へ委譲する（名前衝突のみ
///    `42710`、他は UNIQUE と共有する [`map_alter_constraint_error`] の契約）。
pub(crate) fn execute_alter_table_add_check(
    storage: &Storage,
    stmt: &ValidatedAlterTableAddCheck,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    match storage.get_table_schema(&stmt.table_name) {
        Ok(_) => {}
        Err(CatalogError::TableNotFound(_)) => {
            return Err(undefined_table_or_view(storage, &stmt.table_name));
        }
        Err(other) => return Err(map_add_check_or_foreign_key_error(other)),
    }
    let confirmed_name = storage
        .alter_table_add_check_constraint(&stmt.table_name, &stmt.check)
        .map_err(|e| match e {
            AlterCheckError::Catalog(CatalogError::TableNotFound(_)) => {
                undefined_table_or_view(storage, &stmt.table_name)
            }
            AlterCheckError::Catalog(other) => map_add_check_or_foreign_key_error(other),
            AlterCheckError::Sql(sql_err) => sql_err,
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::AddConstraint {
            constraint_name: confirmed_name,
        },
    })
}

/// `ParsedSql::AlterTable`（[`crate::sql::allowlist::ValidatedAlterTable`]）の
/// 唯一の実行入口（Issue #1067・#1068・#1069）。`core.rs::EngineCore::
/// execute_parsed_in_session` から `require_ddl_permission` 通過後に呼ばれ、
/// 7 つの許可形状（`ADD COLUMN`／`ADD [CONSTRAINT] UNIQUE`／
/// `ADD [CONSTRAINT] CHECK`／`DROP CONSTRAINT`／`ADD [CONSTRAINT] FOREIGN KEY`／
/// `DROP COLUMN`／`ALTER COLUMN TYPE`）
/// を対応する実行本体へ振り分ける（構文の許可リスト判定は `sql::allowlist` の
/// 管轄、ディスパッチはここと `core.rs` の管轄という既存の責務分担を維持する）。
pub(crate) fn execute_alter_table(
    storage: &Storage,
    validated: &crate::sql::allowlist::ValidatedAlterTable,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    match validated {
        crate::sql::allowlist::ValidatedAlterTable::AddColumn(stmt) => {
            execute_alter_table_add_column(storage, stmt)
        }
        crate::sql::allowlist::ValidatedAlterTable::AddUnique(stmt) => {
            execute_alter_table_add_unique(storage, stmt)
        }
        crate::sql::allowlist::ValidatedAlterTable::AddCheck(stmt) => {
            execute_alter_table_add_check(storage, stmt)
        }
        crate::sql::allowlist::ValidatedAlterTable::DropConstraint(stmt) => {
            execute_alter_table_drop_constraint(storage, stmt)
        }
        crate::sql::allowlist::ValidatedAlterTable::AddForeignKey(stmt) => {
            execute_alter_table_add_foreign_key(storage, stmt)
        }
        crate::sql::allowlist::ValidatedAlterTable::DropColumn(stmt) => {
            execute_alter_table_drop_column(storage, stmt)
        }
        crate::sql::allowlist::ValidatedAlterTable::AlterColumnType(stmt) => {
            execute_alter_table_alter_column_type(storage, stmt)
        }
        crate::sql::allowlist::ValidatedAlterTable::AddPrimaryKey(stmt) => {
            execute_alter_table_add_primary_key(storage, stmt)
        }
    }
}

/// `ALTER TABLE <table> ADD [CONSTRAINT <name>] FOREIGN KEY (<col>[, ...])
/// REFERENCES ...` の実行本体（TABLE-22・TASK-233、Issue #1069）。呼び出し元
/// （`core.rs`）は [`require_ddl_permission`] を必ず先に呼んでいる前提。
///
/// 判定順序（決定的。設計 F7）:
/// 1. 対象テーブル（子）の存在確認（`execute_alter_table_add_column` と同じ
///    `undefined_table_or_view` 判定。`42P01`／`42809`）
/// 2. `catalog::Storage::alter_table_add_foreign_key`（単一 write トランザクション内で
///    名前衝突・件数上限・参照先解決・既存行の参照整合性検証を判定。TOCTOU なし）
///
/// 参照先（親）テーブルの不在・種別不一致は `TableNotFound`／`WrongObjectKind`
/// として返るが、これは**親テーブル名**についての判定であり、子テーブル名の
/// 判定（上記 1.）とは別物であるため、`map_alter_constraint_error` はエラーが
/// 運ぶ名前をそのまま使う（子表名で固定しない）。
pub(crate) fn execute_alter_table_add_foreign_key(
    storage: &Storage,
    stmt: &crate::sql::allowlist::ValidatedAlterTableAddForeignKey,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    match storage.get_table_schema(&stmt.table_name) {
        Ok(_) => {}
        Err(CatalogError::TableNotFound(_)) => {
            return Err(undefined_table_or_view(storage, &stmt.table_name));
        }
        Err(other) => return Err(map_add_check_or_foreign_key_error(other)),
    }
    let confirmed_name = storage
        .alter_table_add_foreign_key(
            &stmt.table_name,
            stmt.constraint_name.as_deref(),
            stmt.foreign_key.clone(),
        )
        .map_err(|e| match e {
            CatalogError::TableNotFound(name) if name == stmt.table_name => {
                undefined_table_or_view(storage, &stmt.table_name)
            }
            other => map_add_check_or_foreign_key_error(other),
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::AddConstraint {
            constraint_name: confirmed_name,
        },
    })
}

/// `ALTER TABLE ... ADD CONSTRAINT` の CHECK・FOREIGN KEY 経路専用の写像
/// （TABLE-22・ERR-6、Issue #1195）。制約名衝突だけを `42710`
/// （[`SqlSurfaceError::DuplicateObject`]）へ写像し、他の variant は
/// [`map_alter_constraint_error`] へ委譲する（UNIQUE は `42P07` のまま）。
fn map_add_check_or_foreign_key_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::ConstraintAlreadyExists(name) => SqlSurfaceError::duplicate_object(name),
        other => map_alter_constraint_error(other),
    }
}

/// `Storage::alter_table_add_named_unique_constraint`／
/// `Storage::alter_table_drop_constraint`／`Storage::alter_table_add_check_constraint`
/// （の `AlterCheckError::Catalog` 内側）／`Storage::alter_table_add_foreign_key`
/// の [`CatalogError`] を SQL 表層の契約へ写像する（設計 D4・F5、Issue #1067・
/// #1068・#1069）。ERR-6 の既存行のみを使い、新しい `wire_code` は追加しない——
/// `ADD UNIQUE`・`DROP CONSTRAINT` 経路の制約名衝突は索引名衝突と同じ `42P07`
/// （[`SqlSurfaceError::DuplicateTable`] を流用。CHECK・FOREIGN KEY 経路は
/// `map_add_check_or_foreign_key_error` が先に `42710` へ写像する）、未検出は `DROP INDEX` と同じ `42704`
/// （[`SqlSurfaceError::UndefinedObject`]）、`FOREIGN KEY` の宣言不正は `42830`
/// （[`SqlSurfaceError::invalid_foreign_key`]。`execute_create_table` と同じ
/// 写像）、既存行違反は `23503`（[`SqlSurfaceError::ForeignKeyViolation`]。
/// `TenantWriteError::ForeignKeyViolation` と同じ固定文言）。エラー文言に
/// テナント・行内容・redb 内部詳細は含めない（security.md P0）。
fn map_alter_constraint_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::ColumnNotFound(name) => SqlSurfaceError::UndefinedColumn { name },
        CatalogError::ConstraintAlreadyExists(name) => SqlSurfaceError::DuplicateTable { name },
        CatalogError::ConstraintNotFound(name) => SqlSurfaceError::UndefinedObject { name },
        CatalogError::ConstraintLimitExceeded(detail) => {
            SqlSurfaceError::PayloadTooLarge { detail }
        }
        CatalogError::DependentObjectsStillExist(name) => {
            SqlSurfaceError::DependentObjectsStillExist { name }
        }
        CatalogError::UniqueConstraintViolation => SqlSurfaceError::UniqueViolation,
        CatalogError::InvalidForeignKey(detail) => SqlSurfaceError::invalid_foreign_key(detail),
        CatalogError::ForeignKeyViolation => SqlSurfaceError::ForeignKeyViolation,
        // 既存行に主キー構成列の NULL がある（`ADD PRIMARY KEY`。TABLE-22 (d)、
        // Issue #1196）。文言に含めるのは列名（スキーマ情報）のみ。
        CatalogError::NotNullConstraintViolation(column) => {
            SqlSurfaceError::not_null_violation(column)
        }
        CatalogError::Invalid(detail) => SqlSurfaceError::UnsupportedSyntax { detail },
        // 明示トランザクション（SQL-31・TASK-221）が単一ライタを保持中で書き込み
        // ゲートの待機上限を超えた。他の DDL・書き込み入口と同じく `55P03`。
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        // ストレージ側の内部破損・想定外事象・本 DDL からは到達しないはずの
        // variant はいずれも詳細を露出しない `Internal`（`XX000`）へ丸める
        // （fail-closed。ワイルドカード腕を置かず全 variant を明示列挙する
        // ことで、将来の追加 variant をコンパイラに検出させる）。
        CatalogError::Backend(_)
        | CatalogError::CorruptSchema(_)
        | CatalogError::TableAlreadyExists(_)
        | CatalogError::ColumnAlreadyExists(_)
        | CatalogError::RowNotFound(_)
        | CatalogError::IncompatibleRowKeyFormat
        | CatalogError::TableGenerationCounterOverflow
        | CatalogError::TypeNotFound(_)
        | CatalogError::TypeAlreadyExists(_)
        | CatalogError::ViewNotFound(_)
        | CatalogError::DependentViewsExist(_)
        | CatalogError::ViewLimitExceeded(_)
        | CatalogError::ProtectedColumn(_)
        | CatalogError::IncompatibleTypeChange { .. }
        | CatalogError::TooManyColumns { .. }
        | CatalogError::IndexAlreadyExists(_)
        | CatalogError::IndexNotFound(_)
        | CatalogError::IndexKindMismatch(_)
        | CatalogError::IndexLimitExceeded(_) => SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        },
    }
}

/// `ALTER TABLE <table> DROP COLUMN <column>` の実行本体（TABLE-19・SQL-23、
/// Issue #1167）。呼び出し元（`core.rs`）は [`require_ddl_permission`] を必ず先に
/// 呼んでいる前提。判定順序: テーブル存在確認（`42P01`／`42809`）→
/// `catalog::Storage::alter_table_drop_column`（単一 write txn 内。墓標方式の
/// O(1) 削除・世代 bump 込み）。写像は [`map_drop_alter_column_error`]。
pub(crate) fn execute_alter_table_drop_column(
    storage: &Storage,
    stmt: &ValidatedAlterTableDropColumn,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    ensure_table_exists(storage, &stmt.table_name)?;
    storage
        .alter_table_drop_column(&stmt.table_name, &stmt.column_name)
        .map_err(|e| match e {
            CatalogError::TableNotFound(_) => undefined_table_or_view(storage, &stmt.table_name),
            other => map_drop_alter_column_error(other),
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::DropColumn {
            column_name: stmt.column_name.clone(),
        },
    })
}

/// `ALTER TABLE <table> ALTER COLUMN <column> TYPE <型>` の実行本体（TABLE-19・
/// SQL-23、Issue #1167）。前提は [`execute_alter_table_drop_column`] と同じ。
/// 判定順序: テーブル存在確認 → 目標型の解決（[`resolve_alter_target_type`]。
/// 未登録 ENUM は `42601`）→ `catalog::Storage::alter_table_alter_column_type`
/// （単一 write txn 内で NUMERIC 範囲検証〔`42601`〕→ 列・型互換性判定〔`42804`〕）。
pub(crate) fn execute_alter_table_alter_column_type(
    storage: &Storage,
    stmt: &ValidatedAlterTableAlterColumnType,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    ensure_table_exists(storage, &stmt.table_name)?;
    let target = resolve_alter_target_type(storage, &stmt.column_type)?;
    storage
        .alter_table_alter_column_type(&stmt.table_name, &stmt.column_name, &target)
        .map_err(|e| match e {
            CatalogError::TableNotFound(_) => undefined_table_or_view(storage, &stmt.table_name),
            other => map_drop_alter_column_error(other),
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::AlterColumnType {
            column_name: stmt.column_name.clone(),
        },
    })
}

/// 対象テーブルの存在確認（不在なら `42P01`／`42809`）。読み出し失敗は
/// [`map_drop_alter_column_error`] で fail-closed に写す。
fn ensure_table_exists(storage: &Storage, table_name: &str) -> Result<(), SqlSurfaceError> {
    match storage.get_table_schema(table_name) {
        Ok(_) => Ok(()),
        Err(CatalogError::TableNotFound(_)) => Err(undefined_table_or_view(storage, table_name)),
        Err(other) => Err(map_drop_alter_column_error(other)),
    }
}

/// `ALTER COLUMN TYPE` の目標型解決。[`resolve_column_type`] と異なり `VECTOR` を
/// `0A000` にせず `ColumnType::Vector` へ写し、engine 側の互換性判定で
/// `IncompatibleTypeChange`（`42804`）に落とす。それ以外は [`resolve_column_type`] に委譲。
fn resolve_alter_target_type(
    storage: &Storage,
    ty: &SqlColumnTypeName,
) -> Result<ColumnType, SqlSurfaceError> {
    match ty {
        SqlColumnTypeName::Vector(dim) => Ok(ColumnType::Vector(*dim)),
        other => resolve_column_type(storage, other),
    }
}

/// `DROP COLUMN`／`ALTER COLUMN TYPE` の [`CatalogError`] を SQL 表層の契約へ写像する
/// （TABLE-19・ERR-6、Issue #1167）。`ProtectedColumn`（VECTOR 列・保護列）と
/// `Invalid` は `42601`、依存オブジェクト（PK・UNIQUE・CHECK・FK）は `2BP01`、
/// 非互換な型変更は `42804`。文言に含めるのは列名と型タグのみ（行値・テナント情報なし）。
/// 全 variant を明示列挙しワイルドカード腕を置かない（fail-closed）。
fn map_drop_alter_column_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::ColumnNotFound(name) => SqlSurfaceError::UndefinedColumn { name },
        CatalogError::ProtectedColumn(name) => {
            SqlSurfaceError::unsupported(format!("column {name:?} cannot be dropped or altered"))
        }
        CatalogError::Invalid(detail) => SqlSurfaceError::UnsupportedSyntax { detail },
        CatalogError::DependentObjectsStillExist(name) => {
            SqlSurfaceError::DependentObjectsStillExist { name }
        }
        CatalogError::IncompatibleTypeChange { column, from, to } => {
            SqlSurfaceError::datatype_mismatch(format!(
                "cannot change type of column {column:?} from {from} to {to}"
            ))
        }
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        CatalogError::Backend(_)
        | CatalogError::CorruptSchema(_)
        | CatalogError::TableAlreadyExists(_)
        | CatalogError::ColumnAlreadyExists(_)
        | CatalogError::RowNotFound(_)
        | CatalogError::IncompatibleRowKeyFormat
        | CatalogError::TableGenerationCounterOverflow
        | CatalogError::TypeNotFound(_)
        | CatalogError::TypeAlreadyExists(_)
        | CatalogError::ViewNotFound(_)
        | CatalogError::DependentViewsExist(_)
        | CatalogError::ViewLimitExceeded(_)
        | CatalogError::TooManyColumns { .. }
        | CatalogError::IndexAlreadyExists(_)
        | CatalogError::IndexNotFound(_)
        | CatalogError::IndexKindMismatch(_)
        | CatalogError::IndexLimitExceeded(_)
        | CatalogError::UniqueConstraintViolation
        | CatalogError::InvalidForeignKey(_)
        | CatalogError::ConstraintAlreadyExists(_)
        | CatalogError::ConstraintNotFound(_)
        | CatalogError::ConstraintLimitExceeded(_)
        | CatalogError::ForeignKeyViolation
        | CatalogError::NotNullConstraintViolation(_) => SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        },
    }
}

/// テーブルとして存在しない `name` について、ビューまたは索引（TASK-206・
/// INDEX-7、Issue #908。いずれもテーブルと relation 名前空間を共有する）として
/// 存在すれば `WrongObjectType`（`42809`）、しなければ `UndefinedTable`（`42P01`）を
/// 返す（`core.rs::EngineCore::reclassify_write_to_view_error` と同じ判定。
/// `catalog::Storage::view_definition`／`index_exists` を情報源とする）。読み直し
/// 自体が失敗した場合は「存在しない」と同一視せず `catalog::table_lookup_error` で
/// 写像する（fail-closed。破損したカタログを「存在しない」と誤報告しない）。
fn undefined_table_or_view(storage: &Storage, name: &str) -> SqlSurfaceError {
    let wrong_kind = match storage.view_definition(name) {
        Ok(Some(_)) => true,
        Ok(None) => match storage.index_exists(name) {
            Ok(found) => found,
            Err(e) => return crate::catalog::table_lookup_error(e),
        },
        Err(e) => return crate::catalog::table_lookup_error(e),
    };
    if wrong_kind {
        SqlSurfaceError::WrongObjectType {
            name: name.to_string(),
        }
    } else {
        SqlSurfaceError::UndefinedTable {
            name: name.to_string(),
        }
    }
}

/// [`SqlColumnTypeName`]（構文木）を `catalog::ColumnType`（意味づけ済みの型）へ
/// 解決する。ENUM 型名候補の存在確認だけをここで行い、他の型は無条件で対応する
/// `ColumnType` へ写す（範囲検証は `catalog::alter_table_add_column` 内部の
/// `validate_column` に一本化。`sql::ddl_column_type` モジュールドキュメント
/// 参照）。
fn resolve_column_type(
    storage: &Storage,
    ty: &SqlColumnTypeName,
) -> Result<ColumnType, SqlSurfaceError> {
    match ty {
        SqlColumnTypeName::Text => Ok(ColumnType::Text),
        SqlColumnTypeName::Integer => Ok(ColumnType::Integer),
        SqlColumnTypeName::BigInt => Ok(ColumnType::BigInt),
        SqlColumnTypeName::Real => Ok(ColumnType::Real),
        SqlColumnTypeName::Double => Ok(ColumnType::Double),
        SqlColumnTypeName::Boolean => Ok(ColumnType::Boolean),
        SqlColumnTypeName::Date => Ok(ColumnType::Date),
        SqlColumnTypeName::Timestamp => Ok(ColumnType::Timestamp),
        SqlColumnTypeName::Bytea => Ok(ColumnType::Bytea),
        SqlColumnTypeName::Json => Ok(ColumnType::Json),
        SqlColumnTypeName::Jsonb => Ok(ColumnType::Jsonb),
        SqlColumnTypeName::Uuid => Ok(ColumnType::Uuid),
        SqlColumnTypeName::Numeric { precision, scale } => Ok(ColumnType::Numeric {
            precision: *precision,
            scale: *scale,
        }),
        // 既存行が埋め込みバイトを持たないテーブルへの VECTOR 列追加は、
        // arena 構築・KNN・HNSW 各経路の安全性が未検証のため常に拒否する
        // （`sql::ddl_column_type` モジュールドキュメント参照）。
        SqlColumnTypeName::Vector(_) => Err(SqlSurfaceError::FeatureNotSupported {
            detail: "ALTER TABLE ADD COLUMN does not support VECTOR columns yet".to_string(),
        }),
        SqlColumnTypeName::Enum(name) => {
            let def = storage.get_enum_type(name).map_err(|e| match e {
                CatalogError::TypeNotFound(_) => SqlSurfaceError::UnsupportedSyntax {
                    detail: format!("unknown type name: {name}"),
                },
                other => map_add_column_error(other),
            })?;
            Ok(ColumnType::Enum(def))
        }
    }
}

/// `ADD COLUMN` の `DEFAULT` リテラルを列型に応じた [`ColumnDefault`] へ変換する
/// （`CREATE TABLE` の列 DEFAULT と同じ規則。Issue #1169）。DEFAULT 非対応の列型
/// （日時・BYTEA・JSON・UUID・ENUM・配列等）は `0A000`、対応型でリテラル種別が
/// 合わない場合は `42601`、長さ上限超過は `54000`。
fn add_column_default(
    column_name: &str,
    ty: &ColumnType,
    literal: &InsertLiteral,
) -> Result<ColumnDefault, SqlSurfaceError> {
    let kind = match ty {
        ColumnType::Text => "text",
        ColumnType::Integer
        | ColumnType::BigInt
        | ColumnType::Real
        | ColumnType::Double
        | ColumnType::Numeric { .. } => "numeric",
        ColumnType::Boolean => "boolean",
        _ => {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: format!("column {column_name:?}: DEFAULT is not supported for this type"),
            })
        }
    };
    let mismatch = || {
        SqlSurfaceError::unsupported(format!(
            "column {column_name:?} DEFAULT expects a {kind} literal"
        ))
    };
    match literal {
        InsertLiteral::String(s) if kind == "text" => {
            if s.len() > MAX_COLUMN_DEFAULT_LEN {
                return Err(default_too_long(column_name));
            }
            Ok(ColumnDefault::Text(s.clone()))
        }
        InsertLiteral::Number(n) if kind == "numeric" => {
            if n.len() > MAX_COLUMN_DEFAULT_LEN {
                return Err(default_too_long(column_name));
            }
            Ok(ColumnDefault::Number(n.clone()))
        }
        InsertLiteral::Bool(b) if kind == "boolean" => Ok(ColumnDefault::Bool(*b)),
        _ => Err(mismatch()),
    }
}

fn default_too_long(column_name: &str) -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large(format!(
        "column {column_name:?} DEFAULT literal exceeds length limit"
    ))
}

/// `Storage::alter_table_add_column`（および ENUM 型名解決）の [`CatalogError`] を
/// SQL 表層の契約へ写像する（ERR-2。Issue #900）。`catalog::table_lookup_error`
/// （読み取り専用経路向け）とは意図的に共有しない——`ColumnAlreadyExists`・
/// `TooManyColumns` は読み取り経路には現れない DDL 固有の意味を持つため。
/// エラー文言にテナント・行内容・redb 内部詳細は含めない（security.md）。
fn map_add_column_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        CatalogError::ColumnAlreadyExists(name) => SqlSurfaceError::duplicate_column(name),
        // 列数上限超過は `CREATE TABLE` の列数上限（`validate_create_table_tokens`）
        // と同じく `54000` へ写像する。
        CatalogError::TooManyColumns { count } => {
            SqlSurfaceError::payload_too_large(format!("too many columns: {count}"))
        }
        CatalogError::Invalid(detail) => SqlSurfaceError::UnsupportedSyntax { detail },
        CatalogError::TypeNotFound(name) => SqlSurfaceError::UnsupportedSyntax {
            detail: format!("unknown type name: {name}"),
        },
        // 明示トランザクション（SQL-31・TASK-221）が単一ライタを保持中で書き込み
        // ゲートの待機上限を超えた。他の DDL・書き込み入口と同じく `55P03`。
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        // 対象名がビュー（Issue #909）。現行の `alter_table_add_column` はテーブル
        // 名前空間のみを引くため到達しないが、将来ビュー判別を内包した場合も
        // `42809` に揃える。
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        // ストレージ側の内部破損・想定外事象・本 DDL からは到達しないはずの
        // variant はいずれも詳細を露出しない `Internal`（`XX000`）へ丸める
        // （fail-closed。ワイルドカード腕を置かず全 variant を明示列挙する
        // ことで、将来の追加 variant をコンパイラに検出させる）。
        CatalogError::Backend(_)
        | CatalogError::CorruptSchema(_)
        | CatalogError::TableAlreadyExists(_)
        | CatalogError::RowNotFound(_)
        | CatalogError::IncompatibleRowKeyFormat
        | CatalogError::TableGenerationCounterOverflow
        | CatalogError::TypeAlreadyExists(_)
        | CatalogError::DependentObjectsStillExist(_)
        | CatalogError::ColumnNotFound(_)
        | CatalogError::ProtectedColumn(_)
        | CatalogError::IncompatibleTypeChange { .. }
        | CatalogError::ViewNotFound(_)
        | CatalogError::DependentViewsExist(_)
        | CatalogError::ViewLimitExceeded(_)
        // `UniqueConstraintViolation` は `Storage::alter_table_add_unique_constraint`
        // 専用（TABLE-16・TASK-204、Issue #905）で、`alter_table_add_column` からは
        // 返らない（到達不能）。
        | CatalogError::UniqueConstraintViolation
        // 索引宣言（TASK-206・INDEX-7、Issue #908）専用の変種で、
        // `alter_table_add_column` からは返らない（到達不能）。
        | CatalogError::IndexAlreadyExists(_)
        | CatalogError::IndexNotFound(_)
        | CatalogError::IndexKindMismatch(_)
        | CatalogError::IndexLimitExceeded(_)
        // `FOREIGN KEY` 宣言の照合（`create_table` 専用。TABLE-17・TASK-205、
        // Issue #907）は `alter_table_add_column` からは返らない（到達不能）。
        | CatalogError::InvalidForeignKey(_)
        // 制約名の操作（`ALTER TABLE ... ADD/DROP CONSTRAINT`。Issue #1067）専用の
        // 変種で、`alter_table_add_column` からは返らない（到達不能）。
        | CatalogError::ConstraintAlreadyExists(_)
        | CatalogError::ConstraintNotFound(_)
        | CatalogError::ConstraintLimitExceeded(_)
        // `ALTER TABLE ... ADD FOREIGN KEY` の既存行検証違反（TABLE-22・
        // TASK-233、Issue #1069）専用の変種で、`alter_table_add_column` からは
        // 返らない（到達不能）。
        | CatalogError::ForeignKeyViolation
        | CatalogError::NotNullConstraintViolation(_) => SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        },
    }
}

/// `CREATE VIEW <name> AS <body>`（TABLE-18・SQL-23・TASK-205、Issue #909）の
/// 成功応答。[`DropTableOutcome`] と同じくフィールドを持たない（作成した
/// ビューの内容・他テナントの存在情報を応答から推測できないようにする必要は
/// ないが、`Insert`／`Truncate` と異なり返す値自体がないため設計を揃える）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateViewOutcome {}

/// `DROP VIEW <name>`（TABLE-18・SQL-23・TASK-205、Issue #909）の成功応答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropViewOutcome {}

/// `require_ddl_permission` を通過したセッションに限り呼ばれる実行本体。
/// `crate::catalog::Storage::create_view` へ委譲する（構文検証段〔
/// `validate_create_view_tokens`〕はカタログを一切照会していないため、
/// 参照先の存在確認・ネスト深さ判定・名前衝突判定はすべてこの呼び出しの
/// 中で初めて行われる）。
pub(crate) fn execute_create_view(
    storage: &Storage,
    validated: &ValidatedCreateView,
) -> Result<CreateViewOutcome, SqlSurfaceError> {
    storage
        .create_view(
            validated.name(),
            validated.base_relation(),
            validated.body_sql(),
        )
        .map_err(map_create_view_error)?;
    Ok(CreateViewOutcome {})
}

/// `require_ddl_permission` を通過したセッションに限り呼ばれる実行本体。
/// `crate::catalog::Storage::drop_view` へ委譲する。
pub(crate) fn execute_drop_view(
    storage: &Storage,
    validated: &ValidatedDropView,
) -> Result<DropViewOutcome, SqlSurfaceError> {
    storage
        .drop_view(validated.name())
        .map_err(map_drop_view_error)?;
    Ok(DropViewOutcome {})
}

/// `Storage::create_view` の [`CatalogError`] を SQL 表層の契約へ写像する
/// （ERR-6 の管轄表: 名前衝突 `42P07`、参照先不存在 `42P01`、ネスト深さ・
/// 登録件数上限超過 `54000`）。エラー文言にテナント・行内容・redb 内部詳細は
/// 含めない（security.md P0）。
fn map_create_view_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::TableAlreadyExists(name) => SqlSurfaceError::DuplicateTable { name },
        CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        CatalogError::ViewLimitExceeded(detail) => SqlSurfaceError::PayloadTooLarge { detail },
        CatalogError::Invalid(_) => {
            SqlSurfaceError::unsupported("malformed view definition in CREATE VIEW")
        }
        // 明示トランザクション（SQL-31・TASK-221）が単一ライタを保持中で書き込み
        // ゲートの待機上限を超えた。他の書き込み入口（`CREATE TABLE`／
        // `DROP TABLE`）と同じく `55P03` を返す（`XX000` へ丸めない）。
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        _ => SqlSurfaceError::Internal {
            detail: "CREATE VIEW failed".to_string(),
        },
    }
}

/// `Storage::drop_view` の [`CatalogError`] を SQL 表層の契約へ写像する
/// （ERR-6 の管轄表: 対象不存在 `42P01`、テーブル名を指定 `42809`、依存する
/// ビューが残存 `2BP01`）。
fn map_drop_view_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::ViewNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::DependentViewsExist(name) => {
            SqlSurfaceError::DependentObjectsStillExist { name }
        }
        CatalogError::Invalid(_) => {
            SqlSurfaceError::unsupported("malformed view reference in DROP VIEW")
        }
        // 明示トランザクション（SQL-31・TASK-221）が単一ライタを保持中で書き込み
        // ゲートの待機上限を超えた。他の書き込み入口（`CREATE TABLE`／
        // `DROP TABLE`）と同じく `55P03` を返す（`XX000` へ丸めない）。
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        _ => SqlSurfaceError::Internal {
            detail: "DROP VIEW failed".to_string(),
        },
    }
}

/// `CREATE INDEX`（TASK-206・INDEX-7・SQL-23、Issue #908）の成功応答。
/// [`CreateTableOutcome`] と同じくフィールドを持たない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateIndexOutcome {}

/// `DROP INDEX`（TASK-206・INDEX-7・SQL-23、Issue #908）の成功応答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropIndexOutcome {}

/// `require_ddl_permission` を通過したセッションに限り呼ばれる実行本体
/// （TASK-206・INDEX-7、Issue #908）。`crate::catalog::Storage::create_index`
/// （単一 write txn 内で名前衝突・対象の種別と存在・列整合・件数上限を判定して
/// 保存し、対象テーブルの世代を進める）へ委譲する。構文検証段
/// （`validate_create_index_tokens`）はカタログを一切照会していない。
pub(crate) fn execute_create_index(
    storage: &Storage,
    validated: &ValidatedCreateIndex,
) -> Result<CreateIndexOutcome, SqlSurfaceError> {
    let kind = if validated.is_hnsw() {
        IndexKind::Hnsw
    } else {
        IndexKind::Scalar
    };
    let def = IndexDef::new(
        validated.name().to_string(),
        validated.table().to_string(),
        kind,
        validated.columns().to_vec(),
    );
    storage.create_index(&def).map_err(map_create_index_error)?;
    Ok(CreateIndexOutcome {})
}

/// `require_ddl_permission` を通過したセッションに限り呼ばれる実行本体
/// （TASK-206・INDEX-7、Issue #908）。`crate::catalog::Storage::drop_index` へ委譲する。
pub(crate) fn execute_drop_index(
    storage: &Storage,
    validated: &ValidatedDropIndex,
) -> Result<DropIndexOutcome, SqlSurfaceError> {
    storage
        .drop_index(validated.name())
        .map_err(map_drop_index_error)?;
    Ok(DropIndexOutcome {})
}

/// `Storage::create_index` の [`CatalogError`] を SQL 表層の契約へ写像する
/// （ERR-6: 名前衝突 `42P07`、対象テーブル不存在 `42P01`、対象がビュー `42809`、
/// 列不存在 `42703`、種別と列型の不整合 `0A000`、件数上限 `54000`）。エラー文言に
/// テナント・行内容・redb 内部詳細は含めない（security.md P0）。
fn map_create_index_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::IndexAlreadyExists(name) => SqlSurfaceError::DuplicateTable { name },
        CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::ColumnNotFound(name) => SqlSurfaceError::UndefinedColumn { name },
        CatalogError::IndexKindMismatch(detail) => SqlSurfaceError::FeatureNotSupported { detail },
        CatalogError::IndexLimitExceeded(detail) => SqlSurfaceError::PayloadTooLarge { detail },
        // 識別子形状・列数（構文検証段で既に拒否済みのため通常は到達しない）。
        CatalogError::Invalid(_) => {
            SqlSurfaceError::unsupported("malformed index definition in CREATE INDEX")
        }
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        _ => SqlSurfaceError::Internal {
            detail: "CREATE INDEX failed".to_string(),
        },
    }
}

/// `Storage::drop_index` の [`CatalogError`] を SQL 表層の契約へ写像する
/// （ERR-6: 索引不存在 `42704`、テーブル・ビュー名を指定 `42809`）。
fn map_drop_index_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::IndexNotFound(name) => SqlSurfaceError::UndefinedObject { name },
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::Invalid(_) => {
            SqlSurfaceError::unsupported("malformed index reference in DROP INDEX")
        }
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        _ => SqlSurfaceError::Internal {
            detail: "DROP INDEX failed".to_string(),
        },
    }
}

/// `CREATE TYPE ... AS ENUM`（TABLE-14・SQL-23・TASK-198、Issue #1194）の成功応答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateTypeOutcome {}

/// `DROP TYPE`（TABLE-14・SQL-23・TASK-198、Issue #1194）の成功応答。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DropTypeOutcome {}

/// `require_ddl_permission` を通過したセッションに限り呼ばれる実行本体
/// （Issue #1194）。`Storage::create_enum_type`（単一 write txn 内で名前検証・
/// 重複判定・件数上限を行う）へ委譲する。ENUM 型は全テナント共有カタログのため
/// `PolicyContext` は取らない。
pub(crate) fn execute_create_type(
    storage: &Storage,
    validated: &ValidatedCreateType,
) -> Result<CreateTypeOutcome, SqlSurfaceError> {
    storage
        .create_enum_type(validated.name(), validated.labels().to_vec())
        .map_err(map_create_type_error)?;
    Ok(CreateTypeOutcome {})
}

/// `require_ddl_permission` を通過したセッションに限り呼ばれる実行本体
/// （Issue #1194）。`Storage::drop_enum_type`（同一 write txn 内で依存列を判定し、
/// 残っていれば `DependentObjectsStillExist`）へ委譲する。
pub(crate) fn execute_drop_type(
    storage: &Storage,
    validated: &ValidatedDropType,
) -> Result<DropTypeOutcome, SqlSurfaceError> {
    storage
        .drop_enum_type(validated.name())
        .map_err(map_drop_type_error)?;
    Ok(DropTypeOutcome {})
}

/// `Storage::create_enum_type` の [`CatalogError`] を SQL 表層の契約へ写像する
/// （型名重複は ERR-6 に専用行が無いため `42P07`。定義不正・型数上限は `42601`）。
/// エラー文言にはラベル・テナント・redb 内部詳細を含めない（security.md P0）。
fn map_create_type_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::TypeAlreadyExists(name) => SqlSurfaceError::DuplicateTable { name },
        CatalogError::Invalid(_) => {
            SqlSurfaceError::unsupported("invalid enum type definition in CREATE TYPE")
        }
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        _ => SqlSurfaceError::Internal {
            detail: "CREATE TYPE failed".to_string(),
        },
    }
}

/// `Storage::drop_enum_type` の [`CatalogError`] を SQL 表層の契約へ写像する
/// （型不在 `42704`、依存列が残っている場合 `2BP01`）。文言に含めるのは削除対象の
/// 型名のみで、依存テーブル名は含めない。
fn map_drop_type_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::TypeNotFound(name) => SqlSurfaceError::UndefinedObject { name },
        CatalogError::DependentObjectsStillExist(name) => {
            SqlSurfaceError::DependentObjectsStillExist { name }
        }
        CatalogError::Invalid(_) => {
            SqlSurfaceError::unsupported("malformed type reference in DROP TYPE")
        }
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        _ => SqlSurfaceError::Internal {
            detail: "DROP TYPE failed".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 型 DDL のエラー写像（Issue #1194）。
    #[test]
    fn type_ddl_error_mapping() {
        let code = |e: SqlSurfaceError| e.wire_code();
        assert_eq!(
            code(map_create_type_error(CatalogError::TypeAlreadyExists(
                "t".into()
            ))),
            "42P07"
        );
        assert_eq!(
            code(map_create_type_error(CatalogError::Invalid("x".into()))),
            "42601"
        );
        assert_eq!(
            code(map_create_type_error(CatalogError::WriteLockTimeout)),
            "55P03"
        );
        assert_eq!(
            code(map_drop_type_error(CatalogError::TypeNotFound("t".into()))),
            "42704"
        );
        assert_eq!(
            code(map_drop_type_error(
                CatalogError::DependentObjectsStillExist("t".into())
            )),
            "2BP01"
        );
        assert_eq!(
            code(map_drop_type_error(CatalogError::CorruptSchema("x".into()))),
            "XX000"
        );
    }

    /// 書き込みゲートの待機上限超過は `ALTER TABLE ADD COLUMN` でも `55P03`
    /// （SQL-31・TASK-221。他の DDL 入口と同じ契約。Issue #900）。
    #[test]
    fn add_column_write_lock_timeout_maps_to_lock_not_available() {
        let err = map_add_column_error(CatalogError::WriteLockTimeout);
        assert_eq!(err.wire_code(), "55P03");
    }

    /// テーブルとして存在しない名前は、ビューなら `42809`、どちらでもなければ
    /// `42P01`（`execute_alter_table_add_column` の事前確認・書き込み txn 内の
    /// 再確認〔競合でテーブルが消えた場合〕の双方が共有する判定。Issue #900）。
    #[test]
    fn undefined_table_or_view_distinguishes_view_and_missing() {
        let (storage, _guard) = tmp_storage("undefined-or-view");
        let validated = ValidatedCreateTable {
            table_name: "docs".to_string(),
            columns: vec![crate::catalog::ColumnDef::new(
                "body",
                crate::catalog::ColumnType::Text,
                true,
            )],
            primary_key: None,
            unique_constraints: Vec::new(),
            checks: Vec::new(),
            foreign_keys: Vec::new(),
        };
        execute_create_table(&storage, &validated).expect("create table");
        storage
            .create_view("v_docs", "docs", "SELECT * FROM docs")
            .expect("create view");

        assert_eq!(
            undefined_table_or_view(&storage, "v_docs").wire_code(),
            "42809"
        );
        assert_eq!(
            undefined_table_or_view(&storage, "missing").wire_code(),
            "42P01"
        );
    }

    /// 列数上限超過は `54000`、列名重複は `42701` へ写像する（Issue #900）。
    #[test]
    fn add_column_error_mapping_distinguishes_limit_and_duplicate() {
        assert_eq!(
            map_add_column_error(CatalogError::TooManyColumns { count: 257 }).wire_code(),
            "54000"
        );
        assert_eq!(
            map_add_column_error(CatalogError::ColumnAlreadyExists("c".to_string())).wire_code(),
            "42701"
        );
    }

    /// 書き込みゲートの待機上限超過は `XX000` ではなく `55P03` へ写像する
    /// （SQL-31・TASK-221。DROP TABLE も他の書き込み入口と同じ契約）。
    #[test]
    fn drop_table_write_lock_timeout_maps_to_lock_not_available() {
        let err = map_drop_table_error(CatalogError::WriteLockTimeout);
        assert_eq!(err.wire_code(), "55P03");
    }

    /// `CREATE VIEW`／`DROP VIEW`（TABLE-18・SQL-23・TASK-205、Issue #909）も
    /// `CREATE TABLE`／`DROP TABLE` と同じ書き込み入口の契約を守り、書き込み
    /// ゲートの待機上限超過を `XX000`（内部エラー）へ丸めず `55P03` として
    /// 返すことを固定する（SQL-31・TASK-221 との base 取り込みマージ統合で
    /// 見落としやすい写像の一つ）。
    #[test]
    fn create_view_write_lock_timeout_maps_to_lock_not_available() {
        let err = map_create_view_error(CatalogError::WriteLockTimeout);
        assert_eq!(err.wire_code(), "55P03");
    }

    #[test]
    fn drop_view_write_lock_timeout_maps_to_lock_not_available() {
        let err = map_drop_view_error(CatalogError::WriteLockTimeout);
        assert_eq!(err.wire_code(), "55P03");
    }

    /// `CREATE INDEX`／`DROP INDEX`（TASK-206・INDEX-7、Issue #908）の
    /// `CatalogError` 写像（ERR-6。書き込みゲートの待機上限超過は他の DDL と
    /// 同じく `55P03`、索引件数上限は `54000`、未知の内部事象は `XX000`）。
    #[test]
    fn index_ddl_catalog_errors_map_to_err6_codes() {
        let cases: [(SqlSurfaceError, &str); 10] = [
            (
                map_create_index_error(CatalogError::IndexAlreadyExists("i".into())),
                "42P07",
            ),
            (
                map_create_index_error(CatalogError::TableNotFound("t".into())),
                "42P01",
            ),
            (
                map_create_index_error(CatalogError::WrongObjectKind("v".into())),
                "42809",
            ),
            (
                map_create_index_error(CatalogError::ColumnNotFound("c".into())),
                "42703",
            ),
            (
                map_create_index_error(CatalogError::IndexKindMismatch("k".into())),
                "0A000",
            ),
            (
                map_create_index_error(CatalogError::IndexLimitExceeded("n".into())),
                "54000",
            ),
            (
                map_create_index_error(CatalogError::WriteLockTimeout),
                "55P03",
            ),
            (
                map_drop_index_error(CatalogError::IndexNotFound("i".into())),
                "42704",
            ),
            (
                map_drop_index_error(CatalogError::WrongObjectKind("t".into())),
                "42809",
            ),
            (
                map_drop_index_error(CatalogError::WriteLockTimeout),
                "55P03",
            ),
        ];
        for (err, code) in cases {
            assert_eq!(err.wire_code(), code, "{err:?}");
        }
        let internal = map_drop_index_error(CatalogError::CorruptSchema("x".into()));
        assert_eq!(internal.wire_code(), "XX000");
        assert_eq!(internal.client_message(), "internal error");
    }

    #[test]
    fn require_ddl_permission_rejects_default_session() {
        let session = SessionState::default();
        let err = require_ddl_permission(&session).expect_err("default session must be denied");
        assert!(matches!(err, SqlSurfaceError::InsufficientPrivilege));
        assert_eq!(err.wire_code(), "42501");
    }

    #[test]
    fn require_ddl_permission_accepts_after_allow_ddl() {
        let mut session = SessionState::default();
        session.allow_ddl();
        assert!(require_ddl_permission(&session).is_ok());
    }

    #[test]
    fn insufficient_privilege_client_message_has_no_identifying_detail() {
        let msg = SqlSurfaceError::InsufficientPrivilege.client_message();
        assert_eq!(msg, "permission denied for DDL statement");
    }

    /// 一時 DB の払い出しは Issue #173 の共通ヘルパー（`crate::test_util::
    /// temp_db`）へ一本化する（重複実装を作らない）。
    fn tmp_storage(label: &str) -> (Storage, crate::test_util::temp_db::CleanupGuard) {
        let path = crate::test_util::temp_db::unique_db_path(label);
        let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        (storage, guard)
    }

    #[test]
    fn execute_create_table_succeeds_for_new_table() {
        let (storage, _guard) = tmp_storage("succeeds");
        let validated = ValidatedCreateTable {
            table_name: "docs".to_string(),
            columns: vec![
                crate::catalog::ColumnDef::new(
                    "embedding",
                    crate::catalog::ColumnType::Vector(4),
                    false,
                ),
                crate::catalog::ColumnDef::new("body", crate::catalog::ColumnType::Text, true),
            ],
            primary_key: None,
            unique_constraints: Vec::new(),
            checks: Vec::new(),
            foreign_keys: Vec::new(),
        };
        execute_create_table(&storage, &validated).expect("create table must succeed");
        let schema = storage.get_table_schema("docs").expect("schema must exist");
        assert_eq!(schema.columns.len(), 2);
    }

    #[test]
    fn execute_create_table_rejects_duplicate_name() {
        let (storage, _guard) = tmp_storage("duplicate-name");
        let validated = ValidatedCreateTable {
            table_name: "docs".to_string(),
            columns: vec![crate::catalog::ColumnDef::new(
                "body",
                crate::catalog::ColumnType::Text,
                true,
            )],
            primary_key: None,
            unique_constraints: Vec::new(),
            checks: Vec::new(),
            foreign_keys: Vec::new(),
        };
        execute_create_table(&storage, &validated).expect("first create must succeed");
        let err = execute_create_table(&storage, &validated)
            .expect_err("second create with same name must fail");
        assert!(matches!(err, SqlSurfaceError::DuplicateTable { .. }));
        assert_eq!(err.wire_code(), "42P07");
    }

    #[test]
    fn execute_create_table_rejects_two_vector_columns() {
        let (storage, _guard) = tmp_storage("two-vector");
        let validated = ValidatedCreateTable {
            table_name: "docs".to_string(),
            columns: vec![
                crate::catalog::ColumnDef::new("a", crate::catalog::ColumnType::Vector(4), false),
                crate::catalog::ColumnDef::new("b", crate::catalog::ColumnType::Vector(4), false),
            ],
            primary_key: None,
            unique_constraints: Vec::new(),
            checks: Vec::new(),
            foreign_keys: Vec::new(),
        };
        let err = execute_create_table(&storage, &validated)
            .expect_err("two VECTOR columns must be rejected");
        assert_eq!(err.wire_code(), "42601");
    }

    /// Issue #1195: 制約名衝突は UNIQUE・DROP 経路で `42P07`、CHECK・FK 経路で
    /// `42710`。他の variant は両写像で同一。
    #[test]
    fn constraint_name_collision_maps_by_constraint_kind() {
        let dup = || CatalogError::ConstraintAlreadyExists("c".to_string());
        assert_eq!(map_alter_constraint_error(dup()).wire_code(), "42P07");
        let err = map_add_check_or_foreign_key_error(dup());
        assert!(matches!(err, SqlSurfaceError::DuplicateObject { .. }));
        assert_eq!(err.wire_code(), "42710");
        let others = || {
            vec![
                CatalogError::TableNotFound("t".to_string()),
                CatalogError::ConstraintNotFound("c".to_string()),
                CatalogError::ConstraintLimitExceeded("limit".to_string()),
            ]
        };
        for (a, b) in others().into_iter().zip(others()) {
            assert_eq!(
                map_alter_constraint_error(a).wire_code(),
                map_add_check_or_foreign_key_error(b).wire_code()
            );
        }
    }
}
