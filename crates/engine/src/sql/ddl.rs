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
    AlterApply, AlterCheckError, CascadedObject, CatalogError, ColumnDef, ColumnDefault,
    ColumnType, CreateIndexError, IndexDef, IndexKind, TableSchema, MAX_COLUMN_DEFAULT_LEN,
};
use crate::schema_limits::SchemaLimits;
use crate::sql::allowlist::{
    InsertLiteral, SqlSurfaceError, ValidatedAlterTable, ValidatedAlterTableAddCheck,
    ValidatedAlterTableAddColumn, ValidatedAlterTableAddUnique,
    ValidatedAlterTableAlterColumnDropDefault, ValidatedAlterTableAlterColumnDropNotNull,
    ValidatedAlterTableAlterColumnSetDefault, ValidatedAlterTableAlterColumnSetNotNull,
    ValidatedAlterTableAlterColumnType, ValidatedAlterTableDropColumn,
    ValidatedAlterTableDropConstraint, ValidatedAlterTableMultiple,
    ValidatedAlterTableRenameColumn, ValidatedAlterTableRenameTable, ValidatedCreateIndex,
    ValidatedCreateTable, ValidatedCreateTempTable, ValidatedCreateType, ValidatedCreateView,
    ValidatedDropIndex, ValidatedDropMaterializedView, ValidatedDropTable, ValidatedDropType,
    ValidatedDropView,
};
use crate::sql::ddl_column_type::{
    alias_spelling, to_static_column_type_with_array_default, SqlColumnTypeName, StaticColumnType,
};
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

/// `CREATE TABLE`／`CREATE TEMP TABLE` が共有する、ストレージへ書く前のスキーマ構築
/// （列数上限・配列要素数・ENUM 解決・COLLATE・既定値の正規化と束縛・各種制約）。
/// 永続表の挙動とエラーの順序は切り出し前と同一。`storage` が `None` の場合
/// （`CREATE TEMP TABLE`。TABLE-44・Issue #1683）は永続カタログを一切参照しない:
/// ENUM 列は構文段が `0A000` で拒否済みで、ここへ来た場合も `0A000`（多層防御）。
pub(crate) fn build_table_schema(
    storage: Option<&Storage>,
    validated: &ValidatedCreateTable,
    limits: &SchemaLimits,
) -> Result<TableSchema, SqlSurfaceError> {
    // 起動時設定 `--max-table-columns`（Issue #1548）。構文段の上限（ハード上限）は
    // 変えず、定義が増えるここでだけ設定値と比較する（decode 経路には入れない）。
    // カタログを参照しない判定のため存在オラクルにならず、storage 呼び出しより前に
    // 行うことで従来と同じ `54000` に揃える。
    if validated.columns.len() > limits.max_table_columns() {
        return Err(SqlSurfaceError::payload_too_large(
            "too many columns in CREATE TABLE",
        ));
    }
    // ENUM 列（構文段は未解決マーカー）を登録済みの型定義へ差し替える。型の存在確認は
    // DDL 権限ゲート（`core.rs` の `require_ddl_permission`）の後のここでのみ行い、
    // 未登録は `ADD COLUMN` と同じ `42704`（Issue #2306）。解決から `create_table` までの間に
    // `DROP TYPE` が割り込んだ場合も `create_table` 側の `TypeNotFound` を `42704` へ写す。
    if let Some(storage) = storage {
        for alias in &validated.alias_type_names {
            reject_alias_enum_collision(storage, alias)?;
        }
    }
    let mut columns = validated.columns.clone();
    for column in &mut columns {
        // 配列列の要素数上限（Issue #1548）。`[]` 省略列は設定値で組み直し、明示 `[N]` は
        // 設定値を超えたら `42601`（既存の範囲外 `[N]` と同じ分類）。
        if let ColumnType::Array(array_ty) = &column.ty {
            let max = limits.max_array_elements();
            if validated.array_len_defaulted.contains(&column.name) {
                let rebuilt = array_ty.with_max_len(max).map_err(|e| {
                    SqlSurfaceError::unsupported(format!("invalid array type: {e}"))
                })?;
                column.ty = ColumnType::Array(rebuilt);
            } else if array_ty.max_len() > max {
                return Err(array_len_over_limit(max, array_ty.max_len()));
            }
        }
        if let ColumnType::Enum(def) = &column.ty {
            column.ty = ColumnType::Enum(resolve_enum_type(
                require_enum_storage(storage)?,
                def.name(),
            )?);
        } else if let ColumnType::Array(array_ty) = &column.ty {
            // `<enum>[]` 列（Issue #1357）も同様に登録済みの語彙へ差し替える。
            if let Some(def) = array_ty.enum_def() {
                let resolved = resolve_enum_type(require_enum_storage(storage)?, def.name())?;
                let array = crate::catalog::ArrayType::new_enum(resolved, array_ty.max_len())
                    .and_then(|a| a.with_ndims(array_ty.ndims()))
                    .map_err(|e| {
                        SqlSurfaceError::unsupported(format!("invalid array type: {e}"))
                    })?;
                column.ty = ColumnType::Array(array);
            }
        }
    }
    // 解決後の列型に対して `COLLATE` の適用可否を判定する（TABLE-28・Issue #1609。
    // ENUM 解決後の型で判定するため、未登録 ENUM の `42704` が `42804` より先になる）。
    for column in &columns {
        check_column_collation(&column.ty, column.collation)?;
    }
    // TIMESTAMPTZ の TZ 無し既定値は DDL 時点のセッション TimeZone で解釈して固定する
    // （TABLE-33・Issue #1575。PG と同じ）。
    for column in &mut columns {
        normalize_timestamptz_default(column)?;
    }
    // 解決後の列型に対して DEFAULT を束縛検証する（ENUM の語彙外・DATE/NUMERIC の
    // 不正リテラル等を永続化前に SQLSTATE 付きで拒否する）。
    for column in &columns {
        if let Some(default) = &column.default_expr {
            // 時刻 DEFAULT は評価せず（型適合は構文段・`validate_schema` が検査済み）、
            // `bind_column_default` へ流さない（あちらは定数 DEFAULT 専用）。
            if !matches!(default, ColumnDefault::CurrentTime(_)) {
                crate::sql::parser::bind_column_default(column, default)?;
            }
        }
    }
    reject_time_default_set_default(&columns, &validated.foreign_keys)?;
    let mut schema = TableSchema::new(validated.table_name.clone(), columns);
    // `PRIMARY KEY`（TABLE-16・TASK-204、Issue #903）。`validated.primary_key` は
    // `sql::allowlist::finalize_primary_key` が `id` 単独宣言を `None` へ既に
    // 正規化済みのため、ここでは素通しするだけでよい。
    if let Some(primary_key) = validated.primary_key.clone() {
        schema = schema.with_primary_key(primary_key);
    }
    // 明示主キー名（TABLE-22(a)・Issue #1412）。導出名 `<table>_pkey` と同じ名前は
    // カタログ側が `None` へ正規化する。名前衝突は `validate_schema` の `Invalid`
    // として下記の `42601` へ写像される。
    schema = schema.with_primary_key_name(validated.primary_key_name.clone());
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
    Ok(schema)
}

/// ENUM 解決に使う永続ストレージ。一時表の定義（`storage` なし）に ENUM が残っていた場合は
/// `0A000`（一時表は永続カタログの型依存を追跡できないため。fail-closed）。
fn require_enum_storage(storage: Option<&Storage>) -> Result<&Storage, SqlSurfaceError> {
    storage.ok_or_else(|| {
        SqlSurfaceError::feature_not_supported("ENUM columns are not supported in temporary tables")
    })
}

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
    limits: &SchemaLimits,
) -> Result<CreateTableOutcome, SqlSurfaceError> {
    // `IF NOT EXISTS`（TABLE-36、Issue #1613）。同名の関係（表・ビュー・索引）が既にあれば、
    // 定義の意味検証（列数・型解決・DEFAULT 等）より前に何もせず成功を返す（定義は比較しない）。
    // 権限ゲート（`42501`）は呼び出し元で通過済みのため存在オラクルにならない。最終判定は
    // 下の `create_table` の書き込みトランザクションで、競合時の `TableAlreadyExists` も no-op に写す。
    if validated.if_not_exists
        && storage
            .relation_name_exists(&validated.table_name)
            .map_err(|_| SqlSurfaceError::Internal {
                detail: "internal error".to_string(),
            })?
    {
        return Ok(CreateTableOutcome {});
    }
    let schema = build_table_schema(Some(storage), validated, limits)?;
    // 早期判定〔上〕と書き込みの間に他セッションが同名を作った場合の TOCTOU 対策。名前衝突は
    // commit・世代 bump の前に返るため、no-op は副作用を残さない。
    match storage.create_table(&schema) {
        Err(CatalogError::TableAlreadyExists(_)) if validated.if_not_exists => {
            return Ok(CreateTableOutcome {});
        }
        result => result,
    }
    .map_err(|e| match e {
        CatalogError::TableAlreadyExists(name) => SqlSurfaceError::duplicate_table(name),
        // `FOREIGN KEY` の参照先（TABLE-17・TASK-205、Issue #907）。`create_table` が
        // `TableNotFound` を返すのは参照先テーブルの不在のみ（作成対象自身の重複は
        // 上の `TableAlreadyExists` が先に判定する）。参照先名がビュー・索引名なら
        // 種別不一致（`42809`）、参照先列が一意キーと一致しない・整数型どうし以外の型不一致は `42830`。
        CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::InvalidForeignKey(detail) => SqlSurfaceError::invalid_foreign_key(detail),
        CatalogError::Invalid(detail) => {
            SqlSurfaceError::unsupported(format!("invalid table schema: {detail}"))
        }
        // 明示トランザクション（SQL-31・TASK-221）が単一ライタを保持中で書き込み
        // ゲートの待機上限を超えた。他の書き込み入口と同じく `55P03` を返す。
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        // ENUM 列が参照する型が解決後〜書き込みトランザクションまでの間に削除された
        // 場合（Issue #1348）。事前解決の未登録と同じ `42704` に揃える（Issue #2306）。
        CatalogError::TypeNotFound(name) => SqlSurfaceError::UndefinedType { name },
        CatalogError::Backend(_)
        | CatalogError::CorruptSchema(_)
        | CatalogError::ColumnAlreadyExists(_)
        | CatalogError::RowNotFound(_)
        | CatalogError::IncompatibleRowKeyFormat
        | CatalogError::TableGenerationCounterOverflow
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
        | CatalogError::ViewDefinitionChanged(_)
        | CatalogError::ViewReferenceCycle(_)
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
        | CatalogError::IndexCollationNotSupported(_)
        | CatalogError::IndexLimitExceeded(_)
        // 制約名の操作（`ALTER TABLE ... ADD/DROP CONSTRAINT`。Issue #1067）専用の
        // 変種で、`Storage::create_table` からは返らない（到達不能。`CREATE TABLE`
        // 内の明示制約名の重複は構文段〔`check_create_table_constraint_namespace`。
        // Issue #1428〕が `42P07`／`42710` で先に拒否する。主キー重複も構造段が
        // `42P16` で先に拒否するため `MultiplePrimaryKeys` も返らない）。
        | CatalogError::ConstraintAlreadyExists(_)
        | CatalogError::MultiplePrimaryKeys(_)
        | CatalogError::ColumnInPrimaryKey(_)
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

/// 構造検証済みの `CREATE TEMP TABLE`（[`ValidatedCreateTempTable`]）をセッションの一時カタログへ
/// 登録する（TABLE-44・Issue #1683）。永続カタログ・永続ストレージには一切触れない。
///
/// **DDL 実行権限ゲート（[`require_ddl_permission`]）の対象外**: 一時表の作成は呼び出し
/// セッションの中で閉じ、他セッション・他テナントに影響しないため（TABLE-44 (6)）。呼び出し元は
/// `core.rs` の `ParsedSql::CreateTempTable` 分岐。
///
/// 判定順序（fail-closed）: `IF NOT EXISTS` の既存一時表 → `FOREIGN KEY`（一時表の制約は
/// 一時表だけを参照できるため、永続表の参照は `42P16`。一時表どうしは実行段が未実装で `0A000`）→
/// スキーマ構築・検証（永続表と共有）→ 件数上限（`54000`）と同名重複（`42P07`）→ 行ストアの
/// 遅延生成 → 登録。重い確保（行ストア）は上限・重複の判定の後に行い、拒否された文は
/// セッションを変えない。同名の永続表があっても作成できる（一時表が名前解決で優先される）。
pub(crate) fn execute_create_temp_table(
    session: &mut SessionState,
    tenant_id: &str,
    validated: &ValidatedCreateTempTable,
    schema_limits: &SchemaLimits,
    temp_limits: crate::temp_table_limits::TempTableLimits,
    template: &crate::core::EngineCore,
) -> Result<CreateTableOutcome, SqlSurfaceError> {
    let table = &validated.table;
    if table.if_not_exists && session.temp_tables().contains(&table.table_name) {
        return Ok(CreateTableOutcome {});
    }
    // 一時索引など一時カタログに載らない同名関係との衝突も、永続の `CREATE TABLE IF NOT EXISTS`
    // と同じく FK 拒否・スキーマ検証・表数上限より前に no-op 成功にする（無指定なら後段の
    // `create_table` が `42P07` を返す）。一時領域が未初期化なら一時索引も無いので素通りする。
    if table.if_not_exists {
        if let Some(store) = session.temp_rows_arc() {
            if store.relation_name_exists(&table.table_name)? {
                return Ok(CreateTableOutcome {});
            }
        }
    }
    if let Some(fk) = table.foreign_keys.first() {
        let parent = fk.parent_table();
        return Err(
            if parent == table.table_name || session.temp_tables().contains(parent) {
                SqlSurfaceError::feature_not_supported(
                    "FOREIGN KEY constraints between temporary tables are not supported",
                )
            } else {
                SqlSurfaceError::invalid_table_definition(table.table_name.clone())
            },
        );
    }
    let schema = build_table_schema(None, table, schema_limits)?;
    let schema = crate::catalog::prepare_temp_table_schema(&schema).map_err(|e| match e {
        CatalogError::Invalid(detail) => {
            SqlSurfaceError::unsupported(format!("invalid table schema: {detail}"))
        }
        _ => SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        },
    })?;
    let max_tables = temp_limits.max_tables_per_session();
    if session.temp_tables().contains(&schema.name) {
        return Err(SqlSurfaceError::duplicate_table(schema.name.clone()));
    }
    if session.temp_tables().len() >= max_tables {
        return Err(SqlSurfaceError::payload_too_large(
            "too many temporary tables in this session",
        ));
    }
    session.temp_rows_or_init(tenant_id, temp_limits.max_bytes_per_session(), template)?;
    let store = session
        .temp_rows_arc()
        .ok_or_else(|| SqlSurfaceError::Internal {
            detail: "temporary storage error".to_string(),
        })?;
    // 一時 DB のカタログへ先に実体を作る。以降の DML・索引・制約は一時コアが永続表と同じ
    // 実行器でこの定義を読む。カタログ登録に失敗した場合は作った実体を戻す（孤児を残さない）。
    store.create_table(&schema)?;
    let name = schema.name.clone();
    if let Err(e) = session
        .temp_tables_mut()
        .register(schema, validated.on_commit, max_tables)
    {
        let _ = store.drop_tables(&[name]);
        return Err(e);
    }
    Ok(CreateTableOutcome {})
}

/// 名前がすべてセッションの一時表なら `Some(true)`、一つも一時表でなければ `Some(false)`、
/// 混在していれば `None`（TABLE-44・Issue #1683）。`DROP TABLE` の振り分けに使う。自セッションの
/// 一時カタログだけで決まるため、他セッション・他テナントの存在を観測するオラクルにならない。
/// 一時表以外の名前は存在有無によらず「永続側」扱いのため、`IF EXISTS` 付きで未存在の名前が
/// 一時表と混在した場合も `None`（拒否側に倒す既知の制約）。
pub(crate) fn classify_drop_targets(session: &SessionState, names: &[String]) -> Option<bool> {
    let temp = session.temp_tables();
    let hits = names.iter().filter(|n| temp.contains(n)).count();
    if hits == names.len() {
        Some(true)
    } else if hits == 0 {
        Some(false)
    } else {
        None
    }
}

/// 一時表の `DROP TABLE`（TABLE-44・Issue #1683）。DDL 権限ゲートの対象外で、永続カタログには
/// 触れない。呼び出し元は [`classify_drop_targets`] が `Some(true)`（全名が一時表）を返した
/// 場合に限る。一時表には依存物（ビュー・FOREIGN KEY）が無いため `CASCADE`／`RESTRICT` は
/// 同じ挙動。一時 DB から実体を削除してから一時カタログから外す（実体の削除に失敗した場合は
/// カタログを変えず、固定文言のエラーを返す）。
pub(crate) fn execute_drop_temp_tables(
    session: &mut SessionState,
    validated: &ValidatedDropTable,
) -> Result<DropTableOutcome, SqlSurfaceError> {
    if let Some(store) = session.temp_rows_arc() {
        store.drop_tables(&validated.table_names)?;
    }
    for name in &validated.table_names {
        // 重複指定（`DROP TABLE t, t`）は 1 回として扱う（永続側と同じ）。
        session.temp_tables_mut().remove(name);
    }
    Ok(DropTableOutcome {
        skipped: false,
        cascaded: Vec::new(),
    })
}

/// `DROP TABLE <table>`（SQL-23、TASK-203、Issue #902）の成功応答。
/// [`crate::sql::exec::TruncateOutcome`] と同じく削除件数（全テナント分の
/// 行数）を一切保持しないフィールドなし構造体——他テナントの行数を応答から
/// 推測できないようにする（RLS-9 と同じ設計判断。`DROP TABLE` はテナント
/// スコープの操作ではないため、そもそも「自テナント分の件数」という概念を
/// 持たない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropTableOutcome {
    /// `IF EXISTS` で全対象が無く何も削除しなかったか（Issue #1614・TABLE-36 ポインタ。
    /// 複数指定〔TABLE-37 (1)〕では「1 件も削除しなかった」）。
    /// CommandComplete のタグは通常の DROP と同一のため、wire 側は参照しない。
    pub(crate) skipped: bool,
    /// `CASCADE` で連鎖削除したオブジェクト（適用順。TABLE-37 (3)、Issue #1641。NOTICE 相当の
    /// 報告用）。オブジェクト名のみで行数・テナント情報は持たない。
    /// **BREAKING CHANGE**: 本フィールドのため `Copy` を外した。
    pub(crate) cascaded: Vec<CascadedObject>,
}

impl DropTableOutcome {
    /// `IF EXISTS` により対象不在で no-op 成功した場合に真。
    pub fn skipped(&self) -> bool {
        self.skipped
    }

    /// `CASCADE` で連鎖削除した依存オブジェクト（`RESTRICT` 時は空）。
    pub fn cascaded(&self) -> &[CascadedObject] {
        &self.cascaded
    }
}

/// `require_ddl_permission` を通過したセッションに限り呼ばれる実行本体。複数テーブル指定は
/// 文全体が 1 つの write txn で原子的（TABLE-37 (1)、Issue #1639。判定順は構文 → 権限 →
/// 解決 → 依存）。
/// `crate::catalog::Storage::drop_tables`（カタログエントリ・全テナント行
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
    // 複数指定・`IF EXISTS` の判定（不在の skip を含む）はカタログ層が単一 write txn 内で行う
    // （TABLE-37 (1)。`Storage::drop_tables`）。種類違い（`42809`）・依存（`2BP01`）・
    // ロック timeout 等は読み替えず従来の写像へ流す（fail-closed。権限ゲートは呼び出し元で
    // 通過済みのため存在オラクルにならない）。
    match storage.drop_tables(
        &validated.table_names,
        validated.if_exists,
        validated.behavior,
    ) {
        Ok(report) => Ok(DropTableOutcome {
            skipped: report.dropped == 0,
            cascaded: report.cascaded,
        }),
        Err(e) => Err(map_drop_table_error(e)),
    }
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
    /// `ADD COLUMN IF NOT EXISTS` が同名の生存列を見つけて何も変えなかった（TABLE-36、
    /// Issue #1633）。**BREAKING CHANGE**: variant 追加。
    AddColumnSkipped { column_name: String },
    /// 複数アクションを単一 txn で適用した結果（TABLE-39 (5)、Issue #1633）。要素は記述順。
    /// **BREAKING CHANGE**: variant 追加。
    Multiple { actions: Vec<AlterTableAction> },
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
    /// `ALTER COLUMN <col> SET NOT NULL`（TABLE-39 (3)、Issue #1631）。**BREAKING CHANGE**: variant 追加。
    AlterColumnSetNotNull { column_name: String },
    /// `ALTER COLUMN <col> DROP NOT NULL`（TABLE-39 (4)、Issue #1632）。**BREAKING CHANGE**: variant 追加。
    AlterColumnDropNotNull { column_name: String },
    /// `ALTER COLUMN <col> SET DEFAULT <式>`（TABLE-39 (1)、Issue #1628）。**BREAKING CHANGE**: variant 追加。
    AlterColumnSetDefault { column_name: String },
    /// `ALTER COLUMN <col> DROP DEFAULT`（TABLE-39 (2)、Issue #1628）。**BREAKING CHANGE**: variant 追加。
    AlterColumnDropDefault { column_name: String },
    /// `RENAME [COLUMN] <old> TO <new>`（TABLE-38 のポインタ、Issue #1619）。
    /// **BREAKING CHANGE**: variant 追加。
    RenameColumn {
        column_name: String,
        new_column_name: String,
    },
    /// `RENAME TO <new>`（表の改名。TABLE-38 のポインタ、Issue #1623）。
    /// **BREAKING CHANGE**: variant 追加。
    RenameTable { new_table_name: String },
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
/// （[`add_column_default`]）・値の束縛（`22000`／`22003`／`22007`／`22008`／`22P02`）を検証してから渡し、
/// 既存行は読み出し時に既定値として見える。`DEFAULT` なしの `NOT NULL` は行の
/// 有無によらず構造検証段階で一律 `42601` 拒否される（TABLE-16。`Storage` を直接呼ぶ
/// Rust API は `CatalogError::Invalid`。行ストアを参照せず他テナントの行の存在を漏らさない）。
/// `DEFAULT`・`NOT NULL` のいずれも無い列は従来どおり nullable（TABLE-5）。
pub(crate) fn execute_alter_table_add_column(
    storage: &Storage,
    stmt: &ValidatedAlterTableAddColumn,
    limits: &SchemaLimits,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    let schema = match storage.get_table_schema(&stmt.table_name) {
        Ok(s) => s,
        Err(CatalogError::TableNotFound(_)) => {
            return Err(undefined_table_or_view(storage, &stmt.table_name));
        }
        Err(other) => return Err(map_add_column_error(other)),
    };
    // `IF NOT EXISTS`（TABLE-36、Issue #1633）: 同名の生存列があれば型解決・DEFAULT の束縛より
    // 前に成功扱いで返す（カタログ・世代は変えない。墓標は名前空間に入らない）。
    if stmt.if_not_exists && schema.columns.iter().any(|c| c.name == stmt.column_name) {
        return Ok(add_column_skipped_outcome(stmt));
    }
    let column = build_add_column_def(storage, stmt, limits)?;
    match storage.alter_table_add_column_with_limit(
        &stmt.table_name,
        column,
        limits.max_table_columns(),
    ) {
        Ok(()) => {}
        // 判定と書き込みの間に並行して同名列が追加された場合も `IF NOT EXISTS` なら成功扱い。
        Err(CatalogError::ColumnAlreadyExists(_)) if stmt.if_not_exists => {
            return Ok(add_column_skipped_outcome(stmt));
        }
        Err(CatalogError::TableNotFound(_)) => {
            return Err(undefined_table_or_view(storage, &stmt.table_name));
        }
        Err(other) => return Err(map_add_column_error(other)),
    }
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::AddColumn {
            column_name: stmt.column_name.clone(),
        },
    })
}

/// `ADD COLUMN IF NOT EXISTS` が何もしなかったときの応答（Issue #1633）。
fn add_column_skipped_outcome(stmt: &ValidatedAlterTableAddColumn) -> AlterTableOutcome {
    AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::AddColumnSkipped {
            column_name: stmt.column_name.clone(),
        },
    }
}

/// `ADD COLUMN` 文から `ColumnDef` を組み立てる（型解決・COLLATE 適合・DEFAULT の列型整合と
/// 束縛検証）。単一アクション経路と複数アクション経路（Issue #1633）が共有する。
fn build_add_column_def(
    storage: &Storage,
    stmt: &ValidatedAlterTableAddColumn,
    limits: &SchemaLimits,
) -> Result<ColumnDef, SqlSurfaceError> {
    let ty = resolve_column_type(storage, &stmt.column_type, limits)?;
    check_column_collation(&ty, stmt.collation)?;
    let default = match &stmt.default {
        None => None,
        Some(literal) => Some(add_column_default(&stmt.column_name, &ty, literal)?),
    };
    let mut column = ColumnDef::new(stmt.column_name.clone(), ty, !stmt.not_null);
    column.collation = stmt.collation;
    if let Some(default) = default {
        column = column.with_default(default);
        // TZ 無しの TIMESTAMPTZ 既定値は DDL 時点のセッション TimeZone で固定する（#1575）。
        normalize_timestamptz_default(&mut column)?;
        // 束縛できない DEFAULT（範囲外・桁数超過等）は永続化前に SQLSTATE 付きで拒否する
        // （既存行の読み出し時補完が常に失敗する状態をカタログへ残さない）。
        if let Some(default) = &column.default_expr {
            crate::sql::parser::bind_column_default(&column, default)?;
        }
    }
    Ok(column)
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

/// `ALTER TABLE <table> ADD [CONSTRAINT <name>] PRIMARY KEY (<col>[, ...])` の実行本体
/// （TABLE-22 (a)(d)・TASK-233、Issue #1196・#1364）。呼び出し元（`core.rs`）は [`require_ddl_permission`] を
/// 必ず先に呼んでいる前提（権限の無い主体への存在オラクル化を防ぐ）。
///
/// 判定順序（決定的）: テーブル存在確認（`42P01`／`42809`）→
/// `catalog::Storage::alter_table_add_named_primary_key`（単一 write txn 内で PK 宣言済み
/// 〔`42P16`〕・名前衝突・スキーマ検証・全テナント既存行の NULL／重複検査を判定。
/// TOCTOU なし）。応答の制約名は明示名、省略時は導出擬似名 `<table>_pkey`。
pub(crate) fn execute_alter_table_add_primary_key(
    storage: &Storage,
    stmt: &crate::sql::allowlist::ValidatedAlterTableAddPrimaryKey,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    ensure_table_exists(storage, &stmt.table_name)?;
    let columns: Vec<&str> = stmt.columns.iter().map(|c| c.as_str()).collect();
    let confirmed_name = storage
        .alter_table_add_named_primary_key(
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
/// 許可形状（`ADD COLUMN`／`ADD [CONSTRAINT] UNIQUE`／
/// `ADD [CONSTRAINT] CHECK`／`DROP CONSTRAINT`／`ADD [CONSTRAINT] FOREIGN KEY`／
/// `DROP COLUMN`／`ALTER COLUMN TYPE`／`ALTER COLUMN SET NOT NULL`／
/// `ALTER COLUMN SET／DROP DEFAULT`〔Issue #1628〕／`RENAME COLUMN`）
/// を対応する実行本体へ振り分ける（構文の許可リスト判定は `sql::allowlist` の
/// 管轄、ディスパッチはここと `core.rs` の管轄という既存の責務分担を維持する）。
pub(crate) fn execute_alter_table(
    storage: &Storage,
    validated: &crate::sql::allowlist::ValidatedAlterTable,
    limits: &SchemaLimits,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    match validated {
        crate::sql::allowlist::ValidatedAlterTable::Multiple(stmt) => {
            execute_alter_table_multiple(storage, stmt, limits)
        }
        crate::sql::allowlist::ValidatedAlterTable::AddColumn(stmt) => {
            execute_alter_table_add_column(storage, stmt, limits)
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
            execute_alter_table_alter_column_type(storage, stmt, limits)
        }
        crate::sql::allowlist::ValidatedAlterTable::AddPrimaryKey(stmt) => {
            execute_alter_table_add_primary_key(storage, stmt)
        }
        crate::sql::allowlist::ValidatedAlterTable::AlterColumnSetNotNull(stmt) => {
            execute_alter_table_alter_column_set_not_null(storage, stmt)
        }
        crate::sql::allowlist::ValidatedAlterTable::AlterColumnDropNotNull(stmt) => {
            execute_alter_table_alter_column_drop_not_null(storage, stmt)
        }
        crate::sql::allowlist::ValidatedAlterTable::AlterColumnSetDefault(stmt) => {
            execute_alter_table_alter_column_set_default(storage, stmt)
        }
        crate::sql::allowlist::ValidatedAlterTable::AlterColumnDropDefault(stmt) => {
            execute_alter_table_alter_column_drop_default(storage, stmt)
        }
        crate::sql::allowlist::ValidatedAlterTable::RenameColumn(stmt) => {
            execute_alter_table_rename_column(storage, stmt)
        }
        crate::sql::allowlist::ValidatedAlterTable::RenameTable(stmt) => {
            execute_alter_table_rename_table(storage, stmt)
        }
    }
}

/// `ALTER TABLE <table> <アクション>, <アクション>, ...` の実行本体（TABLE-39 (5) のポインタ、
/// Issue #1633）。呼び出し元（[`execute_alter_table`]）は `core.rs` が
/// [`require_ddl_permission`] を先に通した後に呼ぶ。
///
/// 全アクションを `Storage::alter_table_in_single_txn` の単一 write txn 内で適用する。途中の
/// アクションが失敗したら txn ごと破棄され何も適用されない（カタログ・世代とも不変）。適用順は
/// PostgreSQL の `AlterTablePass` に倣い、pass（`DROP COLUMN` → `ADD COLUMN` →
/// 列属性変更〔SET／DROP NOT NULL・SET／DROP DEFAULT〕）の昇順で、同じ pass 内は記述順。応答の `actions` は記述順に並べ直す。各アクションは txn 内のスキーマ（前のアクションの
/// 結果を含む）に対して判定し、エラーの SQLSTATE は単一アクション経路と同じ写像を使う。
/// いずれのアクションもカタログを変えなかった場合（`IF NOT EXISTS` の skip・冪等な no-op のみ）は
/// commit せず世代も進めない。
pub(crate) fn execute_alter_table_multiple(
    storage: &Storage,
    stmt: &ValidatedAlterTableMultiple,
    limits: &SchemaLimits,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    ensure_table_exists(storage, &stmt.table_name)?;
    let mut order: Vec<usize> = (0..stmt.actions.len()).collect();
    // 安定ソート: 同じ pass 内は記述順を保つ。
    order.sort_by_key(|&i| stmt.actions.get(i).map_or(u8::MAX, alter_action_pass));
    let results = storage
        .alter_table_in_single_txn(&stmt.table_name, |txn| {
            let mut results: Vec<Option<AlterTableAction>> = vec![None; stmt.actions.len()];
            let mut changed = false;
            let mut live_checked = false;
            for &i in &order {
                let Some(action) = stmt.actions.get(i) else {
                    continue;
                };
                // 列属性変更 pass に入る前に、DROP／ADD pass の結果として生存列が残っていることを
                // 確かめる（0 本のまま属性変更が内部エラーにならないよう、ここで 42601 にする）。
                if !live_checked && alter_action_pass(action) >= ALTER_PASS_ATTRIBUTES {
                    Storage::ensure_table_has_live_column_in_txn(txn, &stmt.table_name)
                        .map_err(|e| alter_in_txn_error(e, map_drop_alter_column_error))?;
                    live_checked = true;
                }
                let (done, apply) =
                    apply_alter_action_in_txn(storage, txn, &stmt.table_name, action, limits)?;
                changed |= apply == AlterApply::Applied;
                if let Some(slot) = results.get_mut(i) {
                    *slot = Some(done);
                }
            }
            // DROP COLUMN の「生存列 1 本以上」検証は中間状態では保留しているため、全アクション
            // 適用後にここで検証する（最後の列を置き換える `ADD COLUMN b, DROP COLUMN a` 等を許す。
            // 0 本のままなら単一 DROP と同じ 42601 で txn ごと破棄）。
            Storage::ensure_table_has_live_column_in_txn(txn, &stmt.table_name)
                .map_err(|e| alter_in_txn_error(e, map_drop_alter_column_error))?;
            Ok((results, changed))
        })
        .map_err(|e| match e {
            AlterCheckError::Catalog(CatalogError::TableNotFound(_)) => {
                undefined_table_or_view(storage, &stmt.table_name)
            }
            AlterCheckError::Catalog(other) => map_drop_alter_column_error(other),
            AlterCheckError::Sql(sql_err) => sql_err,
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::Multiple {
            actions: results.into_iter().flatten().collect(),
        },
    })
}

/// 複数アクションの列属性変更 pass の番号（[`alter_action_pass`] 参照）。
const ALTER_PASS_ATTRIBUTES: u8 = 2;

/// 複数アクションの適用順（PostgreSQL の `AlterTablePass` 相当）。数値が小さいほど先に適用する。
/// 複数アクションに含められない種別は構文段で拒否済みのため最後尾（到達しない）に置く。
fn alter_action_pass(action: &ValidatedAlterTable) -> u8 {
    match action {
        ValidatedAlterTable::DropColumn(_) => 0,
        ValidatedAlterTable::AddColumn(_) => 1,
        // 列属性変更（SET／DROP NOT NULL・SET／DROP DEFAULT）は ADD COLUMN の後に記述順で適用する
        // （同一 pass 内は安定ソートで記述順を保つ）。
        ValidatedAlterTable::AlterColumnSetNotNull(_)
        | ValidatedAlterTable::AlterColumnDropNotNull(_)
        | ValidatedAlterTable::AlterColumnSetDefault(_)
        | ValidatedAlterTable::AlterColumnDropDefault(_) => ALTER_PASS_ATTRIBUTES,
        _ => u8::MAX,
    }
}

/// write txn 内の [`CatalogError`] を複数アクション経路のエラーへ写す。`TableNotFound` は
/// txn を破棄した後で `42P01`／`42809` を判別するため未変換のまま返し、それ以外は各アクション
/// 種別の写像 `map` で `SqlSurfaceError` にする。
fn alter_in_txn_error(
    e: CatalogError,
    map: fn(CatalogError) -> SqlSurfaceError,
) -> AlterCheckError {
    match e {
        CatalogError::TableNotFound(_) => AlterCheckError::Catalog(e),
        other => AlterCheckError::Sql(map(other)),
    }
}

/// 複数アクションの 1 個を write txn 内で適用する。前提・判定は単一アクション経路の
/// `execute_alter_table_*` と同じで、スキーマは txn 内のもの（前のアクションの結果を含む）を使う。
/// txn 内から `Storage` の write txn を開く API は呼ばない（書き込みゲートの再入で `55P03`
/// になるため）。`ENUM` 型名の存在確認だけは読み取り専用の `storage` 経由で行う。
fn apply_alter_action_in_txn(
    storage: &Storage,
    txn: &redb::WriteTransaction,
    table_name: &str,
    action: &ValidatedAlterTable,
    limits: &SchemaLimits,
) -> Result<(AlterTableAction, AlterApply), AlterCheckError> {
    use crate::catalog::{get_table_schema_in_txn, get_table_schema_in_txn_impl};
    match action {
        ValidatedAlterTable::AddColumn(stmt) => {
            // DROP COLUMN pass 直後は生存列 0 本の中間状態がありうる（全アクション適用後に検証する）。
            let schema = get_table_schema_in_txn_impl(txn, table_name, false)
                .map_err(|e| alter_in_txn_error(e, map_add_column_error))?;
            if stmt.if_not_exists && schema.columns.iter().any(|c| c.name == stmt.column_name) {
                return Ok((
                    AlterTableAction::AddColumnSkipped {
                        column_name: stmt.column_name.clone(),
                    },
                    AlterApply::Unchanged,
                ));
            }
            let column =
                build_add_column_def(storage, stmt, limits).map_err(AlterCheckError::Sql)?;
            let max_columns =
                Storage::prevalidate_add_column(table_name, &column, limits.max_table_columns())
                    .map_err(|e| alter_in_txn_error(e, map_add_column_error))?;
            Storage::alter_table_add_column_in_txn(txn, table_name, column, max_columns, true)
                .map_err(|e| alter_in_txn_error(e, map_add_column_error))?;
            Ok((
                AlterTableAction::AddColumn {
                    column_name: stmt.column_name.clone(),
                },
                AlterApply::Applied,
            ))
        }
        ValidatedAlterTable::DropColumn(stmt) => {
            let map = map_drop_alter_column_error;
            Storage::validate_alter_column_target(table_name, &stmt.column_name)
                .map_err(|e| alter_in_txn_error(e, map))?;
            Storage::alter_table_drop_column_in_txn(txn, table_name, &stmt.column_name, true)
                .map_err(|e| alter_in_txn_error(e, map))?;
            Ok((
                AlterTableAction::DropColumn {
                    column_name: stmt.column_name.clone(),
                },
                AlterApply::Applied,
            ))
        }
        ValidatedAlterTable::AlterColumnSetNotNull(stmt) => {
            let map = map_alter_column_nullability_error;
            Storage::validate_alter_column_target(table_name, &stmt.column_name)
                .map_err(|e| alter_in_txn_error(e, map))?;
            let apply =
                Storage::alter_table_set_not_null_in_txn(txn, table_name, &stmt.column_name)
                    .map_err(|e| alter_in_txn_error(e, map))?;
            Ok((
                AlterTableAction::AlterColumnSetNotNull {
                    column_name: stmt.column_name.clone(),
                },
                apply,
            ))
        }
        ValidatedAlterTable::AlterColumnDropNotNull(stmt) => {
            let map = map_alter_column_nullability_error;
            Storage::validate_alter_column_target(table_name, &stmt.column_name)
                .map_err(|e| alter_in_txn_error(e, map))?;
            let apply =
                Storage::alter_table_drop_not_null_in_txn(txn, table_name, &stmt.column_name)
                    .map_err(|e| alter_in_txn_error(e, map))?;
            Ok((
                AlterTableAction::AlterColumnDropNotNull {
                    column_name: stmt.column_name.clone(),
                },
                apply,
            ))
        }
        ValidatedAlterTable::AlterColumnSetDefault(stmt) => {
            let map = map_alter_column_default_error;
            let schema = get_table_schema_in_txn(txn, table_name)
                .map_err(|e| alter_in_txn_error(e, map_drop_alter_column_error))?;
            let probe = prepare_set_default_probe(&schema, stmt).map_err(AlterCheckError::Sql)?;
            Storage::validate_alter_column_target(table_name, &stmt.column_name)
                .map_err(|e| alter_in_txn_error(e, map))?;
            let apply = Storage::alter_table_set_default_expr_in_txn(
                txn,
                table_name,
                &stmt.column_name,
                &probe.ty,
                probe.default_expr.clone(),
            )
            .map_err(|e| alter_in_txn_error(e, map))?;
            Ok((
                AlterTableAction::AlterColumnSetDefault {
                    column_name: stmt.column_name.clone(),
                },
                apply,
            ))
        }
        ValidatedAlterTable::AlterColumnDropDefault(stmt) => {
            let map = map_alter_column_default_error;
            let schema = get_table_schema_in_txn(txn, table_name)
                .map_err(|e| alter_in_txn_error(e, map_drop_alter_column_error))?;
            let ty = schema
                .columns
                .iter()
                .find(|c| c.name == stmt.column_name)
                .map(|c| c.ty.clone())
                .ok_or_else(|| {
                    AlterCheckError::Sql(SqlSurfaceError::UndefinedColumn {
                        name: stmt.column_name.clone(),
                    })
                })?;
            Storage::validate_alter_column_target(table_name, &stmt.column_name)
                .map_err(|e| alter_in_txn_error(e, map))?;
            let apply = Storage::alter_table_set_default_expr_in_txn(
                txn,
                table_name,
                &stmt.column_name,
                &ty,
                None,
            )
            .map_err(|e| alter_in_txn_error(e, map))?;
            Ok((
                AlterTableAction::AlterColumnDropDefault {
                    column_name: stmt.column_name.clone(),
                },
                apply,
            ))
        }
        // 構文段（`into_validated_alter_table`）が複数アクションから除外済みの種別。到達した場合は
        // fail-closed で拒否する（何も適用しない）。
        ValidatedAlterTable::Multiple(_)
        | ValidatedAlterTable::AddUnique(_)
        | ValidatedAlterTable::AddCheck(_)
        | ValidatedAlterTable::DropConstraint(_)
        | ValidatedAlterTable::AddForeignKey(_)
        | ValidatedAlterTable::AddPrimaryKey(_)
        | ValidatedAlterTable::AlterColumnType(_)
        | ValidatedAlterTable::RenameColumn(_)
        | ValidatedAlterTable::RenameTable(_) => {
            Err(AlterCheckError::Sql(SqlSurfaceError::FeatureNotSupported {
                detail: "multiple ALTER TABLE actions including this action are not supported"
                    .to_string(),
            }))
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
        Ok(schema) => {
            reject_time_default_set_default(
                &schema.columns,
                std::slice::from_ref(&stmt.foreign_key),
            )?;
        }
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
        CatalogError::MultiplePrimaryKeys(table) => {
            SqlSurfaceError::invalid_table_definition(table)
        }
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
        | CatalogError::ViewDefinitionChanged(_)
        | CatalogError::ViewReferenceCycle(_)
        | CatalogError::ViewLimitExceeded(_)
        | CatalogError::ProtectedColumn(_)
        | CatalogError::IncompatibleTypeChange { .. }
        | CatalogError::TooManyColumns { .. }
        | CatalogError::IndexAlreadyExists(_)
        | CatalogError::IndexNotFound(_)
        | CatalogError::IndexKindMismatch(_)
        | CatalogError::IndexCollationNotSupported(_)
        | CatalogError::ColumnInPrimaryKey(_)
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

/// `ALTER TABLE <table> RENAME [COLUMN] <old> TO <new>` の実行本体（TABLE-38 の
/// ポインタ、Issue #1619）。呼び出し元（`core.rs`）は [`require_ddl_permission`] を
/// 必ず先に呼んでいる前提。
///
/// 判定順序（決定的）: テーブル存在確認（`42P01`／`42809`）→
/// `catalog::Storage::alter_table_rename_column`（単一 write txn 内。改名元の不在
/// `42703` → 改名先の衝突 `42701` → 依存オブジェクトの拒否 `0A000` → ビュー定義の追随
/// 〔追随できない形は `0A000`、本文サイズ超過は `54000`。Issue #1621〕。カタログ名のみを
/// 書き換え行ストアには触れない）。写像は [`map_rename_column_error`]。
pub(crate) fn execute_alter_table_rename_column(
    storage: &Storage,
    stmt: &ValidatedAlterTableRenameColumn,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    ensure_table_exists(storage, &stmt.table_name)?;
    storage
        .alter_table_rename_column(&stmt.table_name, &stmt.column_name, &stmt.new_column_name)
        .map_err(|e| match e {
            AlterCheckError::Catalog(CatalogError::TableNotFound(_)) => {
                undefined_table_or_view(storage, &stmt.table_name)
            }
            AlterCheckError::Catalog(other) => map_rename_column_error(other),
            AlterCheckError::Sql(sql_err) => sql_err,
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::RenameColumn {
            column_name: stmt.column_name.clone(),
            new_column_name: stmt.new_column_name.clone(),
        },
    })
}

/// `ALTER TABLE <table> RENAME TO <new>` の実行本体（TABLE-38 のポインタ、Issue #1623）。
/// 呼び出し元（`core.rs`）は [`require_ddl_permission`] を必ず先に呼んでいる前提。
///
/// 判定順序（決定的）: テーブル存在確認（`42P01`／`42809`）→
/// `catalog::Storage::alter_table_rename`（単一 write txn 内。改名先の衝突 `42P07` →
/// ビュー定義の追随〔単純形は付け替え、評価後射影形は `0A000`、本文超過は `54000`。
/// Issue #1625〕→ 制約・FK・索引宣言を含むカタログ・行ストア・世代・`operation_id` 台帳の
/// 付け替え。Issue #1624・#1625）。
pub(crate) fn execute_alter_table_rename_table(
    storage: &Storage,
    stmt: &ValidatedAlterTableRenameTable,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    ensure_table_exists(storage, &stmt.table_name)?;
    storage
        .alter_table_rename(&stmt.table_name, &stmt.new_table_name)
        .map_err(|e| match e {
            AlterCheckError::Catalog(CatalogError::TableNotFound(_)) => {
                undefined_table_or_view(storage, &stmt.table_name)
            }
            AlterCheckError::Catalog(CatalogError::TableAlreadyExists(name)) => {
                SqlSurfaceError::duplicate_table(name)
            }
            AlterCheckError::Catalog(other) => map_drop_alter_column_error(other),
            AlterCheckError::Sql(sql_err) => sql_err,
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::RenameTable {
            new_table_name: stmt.new_table_name.clone(),
        },
    })
}

/// `ALTER TABLE <table> ALTER COLUMN <column> TYPE <型>` の実行本体（TABLE-19・
/// SQL-23、Issue #1167）。前提は [`execute_alter_table_drop_column`] と同じ。
/// 判定順序: テーブル存在確認 → 目標型の解決（[`resolve_alter_target_type`]。
/// 未登録 ENUM は `42601`）→ 変換可否の事前判定（[`precheck_alter_type_conversion`]。
/// TABLE-40・Issue #1635）→ `catalog::Storage::alter_table_alter_column_type`
/// （単一 write txn 内で NUMERIC 範囲検証〔`42601`〕→ 列・型互換性判定〔`42804`〕→
/// 拡大変換なら全テナント既存行の再エンコード〔Issue #1361。PK／UNIQUE／FK 列も受理し
/// 索引を失効させる。Issue #1402〕。文字列系の縮小・`CHAR(n)` 変換は超過行を `22001`・重複を
/// `23505`・FK 違反を `23503`／`42804` で拒否し表は不変。Issue #1567））。
pub(crate) fn execute_alter_table_alter_column_type(
    storage: &Storage,
    stmt: &ValidatedAlterTableAlterColumnType,
    limits: &SchemaLimits,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    ensure_table_exists(storage, &stmt.table_name)?;
    let target = resolve_alter_target_type(storage, &stmt.column_type, limits)?;
    check_column_collation(&target, stmt.collation)?;
    precheck_alter_type_conversion(storage, stmt, &target)?;
    storage
        .alter_table_alter_column_type_collated(
            &stmt.table_name,
            &stmt.column_name,
            &target,
            stmt.collation,
            stmt.using.as_ref(),
        )
        .map_err(|e| match e {
            AlterCheckError::Catalog(CatalogError::TableNotFound(_)) => {
                undefined_table_or_view(storage, &stmt.table_name)
            }
            AlterCheckError::Catalog(other) => map_drop_alter_column_error(other),
            // 拡大変換後の CHECK 再検証失敗（`23514`・式評価エラー。Issue #1427）。
            AlterCheckError::Sql(sql_err) => sql_err,
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::AlterColumnType {
            column_name: stmt.column_name.clone(),
        },
    })
}

/// `ALTER COLUMN TYPE` の変換可否の事前判定（TABLE-40、Issue #1635）。DDL 権限ゲート・表の
/// 存在確認・目標型の解決の後に呼ばれる（権限の無い主体への存在オラクルにしない）。
///
/// - `USING` あり: 式を旧スキーマで束縛・型検査する（[`crate::sql::alter_using`]）。早期検証
///   だけで、行の書き換えはカタログが write txn 内で式を束縛し直して行う（Issue #1636）。
///   副作用は無い。
/// - `USING` 省略: 旧型から新型への代入変換が規則表（SQL-47）に無い組を `42804`（`USING` の
///   指定を促す固定文言）で拒否する。代入変換がある組は、カタログが全行を書き換えて受理する。
///   規則表の外の型（VECTOR・ENUM）は、従来どおりカタログの互換性判定（`42804`。TABLE-19 の
///   契約）に任せる。
/// - 列が見つからない場合はカタログの既存判定（`42703`）に委ねる。
fn precheck_alter_type_conversion(
    storage: &Storage,
    stmt: &ValidatedAlterTableAlterColumnType,
    target: &ColumnType,
) -> Result<(), SqlSurfaceError> {
    let schema = storage
        .get_table_schema(&stmt.table_name)
        .map_err(|e| match e {
            CatalogError::TableNotFound(_) => undefined_table_or_view(storage, &stmt.table_name),
            other => map_drop_alter_column_error(other),
        })?;
    let Some((index, column)) = schema
        .columns
        .iter()
        .enumerate()
        .find(|(_, c)| c.name == stmt.column_name)
    else {
        return Ok(());
    };
    if let Some(using) = &stmt.using {
        // 早期検証（束縛エラー・`42804`）だけを行う。式の束縛は、カタログが write txn 内で
        // 旧スキーマに対してやり直す（TOCTOU 対策）。行は読まない。
        crate::sql::alter_using::bind_alter_conversion(Some(using), &schema, index, target)?;
        return Ok(());
    }
    if crate::sql::cast::column_assignment_convertible(&column.ty, target) == Some(false) {
        return Err(SqlSurfaceError::datatype_mismatch(format!(
            "column {:?} cannot be cast automatically to type {}; specify a USING expression",
            stmt.column_name,
            crate::sql::cast::cast_type_of_column(target)
                .map(|t| t.pg_label())
                .unwrap_or_default()
        )));
    }
    Ok(())
}

/// `ALTER TABLE <table> ALTER COLUMN <column> SET NOT NULL` の実行本体（TABLE-39 (3)、
/// Issue #1631）。前提は [`execute_alter_table_drop_column`] と同じ（`core.rs` が
/// [`require_ddl_permission`] を先に通す）。判定順序: テーブル存在確認 →
/// `catalog::Storage::alter_table_set_not_null`（単一 write txn 内で全テナント・全可視性の
/// 既存行の NULL 検査〔`23502`〕と FK 宣言の整合検査〔`42830`〕を行い、拒否時は副作用ゼロ）。
fn execute_alter_table_alter_column_set_not_null(
    storage: &Storage,
    stmt: &ValidatedAlterTableAlterColumnSetNotNull,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    ensure_table_exists(storage, &stmt.table_name)?;
    storage
        .alter_table_set_not_null(&stmt.table_name, &stmt.column_name)
        .map_err(|e| match e {
            CatalogError::TableNotFound(_) => undefined_table_or_view(storage, &stmt.table_name),
            other => map_alter_column_nullability_error(other),
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::AlterColumnSetNotNull {
            column_name: stmt.column_name.clone(),
        },
    })
}

/// `ALTER TABLE <table> ALTER COLUMN <column> DROP NOT NULL` の実行本体（TABLE-39 (4)、
/// Issue #1632）。前提は [`execute_alter_table_drop_column`] と同じ（`core.rs` が
/// [`require_ddl_permission`] を先に通す）。判定順序: テーブル存在確認 →
/// `catalog::Storage::alter_table_drop_not_null`（主キー構成列〔`42P16`〕・VECTOR 列・FK 参照先側
/// の整合〔`42830`〕を検査し、拒否時は副作用ゼロ。既存行は走査しない）。
fn execute_alter_table_alter_column_drop_not_null(
    storage: &Storage,
    stmt: &ValidatedAlterTableAlterColumnDropNotNull,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    ensure_table_exists(storage, &stmt.table_name)?;
    storage
        .alter_table_drop_not_null(&stmt.table_name, &stmt.column_name)
        .map_err(|e| match e {
            CatalogError::TableNotFound(_) => undefined_table_or_view(storage, &stmt.table_name),
            other => map_alter_column_nullability_error(other),
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::AlterColumnDropNotNull {
            column_name: stmt.column_name.clone(),
        },
    })
}

/// `SET DEFAULT` の検証済み列定義（`default_expr` のみ差し替え済み）を作る。列の取得
/// （`42703`）・既定値の生成（型不一致 `42804`）・`TIMESTAMPTZ` 正規化・値の束縛検証・
/// 時刻既定値と FK `SET DEFAULT` の組の拒否（`0A000`）を行う。単一アクション経路と、
/// write txn 内のスキーマで呼ぶ複数アクション経路（Issue #1633）が共有する。
fn prepare_set_default_probe(
    schema: &TableSchema,
    stmt: &ValidatedAlterTableAlterColumnSetDefault,
) -> Result<ColumnDef, SqlSurfaceError> {
    let column = schema
        .columns
        .iter()
        .find(|c| c.name == stmt.column_name)
        .ok_or_else(|| SqlSurfaceError::UndefinedColumn {
            name: stmt.column_name.clone(),
        })?;
    let new_default = match (&stmt.time_default, &stmt.default) {
        (Some(time), _) => Some(time_column_default_for_set(
            &column.name,
            &column.ty,
            *time,
        )?),
        (None, Some(literal)) => Some(set_column_default_value(&column.name, &column.ty, literal)?),
        (None, None) => None,
    };
    let mut probe = column.clone();
    probe.set_default_expr(new_default);
    // 補完値（`fill_value`）まで書き換える `normalize_timestamptz_default` は使わず、
    // 変更対象の `default_expr` スロットだけを正規化する。
    normalize_timestamptz_slot_if_needed(&mut probe)?;
    if let Some(default) = &probe.default_expr {
        if !matches!(default, ColumnDefault::CurrentTime(_)) {
            crate::sql::parser::bind_column_default(&probe, default)?;
        }
    }
    let mut columns = schema.columns.clone();
    for c in columns.iter_mut() {
        if c.name == probe.name {
            *c = probe.clone();
        }
    }
    reject_time_default_set_default(&columns, schema.foreign_keys())?;
    Ok(probe)
}

/// `ALTER TABLE <table> ALTER COLUMN <column> SET DEFAULT <式>` の実行本体（TABLE-39 (1)、
/// Issue #1628）。前提は [`execute_alter_table_drop_column`] と同じ（`core.rs` が
/// [`require_ddl_permission`] を先に通す）。変えるのは以後の書き込みで使う INSERT 既定値
/// （`default_expr`）だけで、既存行の読み出し時補完値（`fill_value`）は変えない。
///
/// 判定順序: (1) テーブル存在確認（`42P01`／`42809`） (2) 列の取得（`42703`。保護列は
/// catalog 側で拒否） (3) 既定値の生成（リテラル→[`set_column_default_value`]、時刻値関数→
/// [`time_column_default_for_set`]。型不一致は `42804`） (4) `TIMESTAMPTZ` の正規化 (5) 値の束縛検証
/// （`22003`／`22P02` 等） (6) 時刻既定値と FK `SET DEFAULT` の組の拒否（`0A000`）
/// (7) `catalog::Storage::alter_table_set_default_expr`（単一 write txn 内で列型の一致・
/// 再検証・FK 宣言整合〔`42830`〕を行い、拒否時は副作用ゼロ）。
fn execute_alter_table_alter_column_set_default(
    storage: &Storage,
    stmt: &ValidatedAlterTableAlterColumnSetDefault,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    let schema = match storage.get_table_schema(&stmt.table_name) {
        Ok(s) => s,
        Err(CatalogError::TableNotFound(_)) => {
            return Err(undefined_table_or_view(storage, &stmt.table_name));
        }
        Err(other) => return Err(map_drop_alter_column_error(other)),
    };
    let probe = prepare_set_default_probe(&schema, stmt)?;
    storage
        .alter_table_set_default_expr(
            &stmt.table_name,
            &stmt.column_name,
            &probe.ty,
            probe.default_expr.clone(),
        )
        .map_err(|e| match e {
            CatalogError::TableNotFound(_) => undefined_table_or_view(storage, &stmt.table_name),
            other => map_alter_column_default_error(other),
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::AlterColumnSetDefault {
            column_name: stmt.column_name.clone(),
        },
    })
}

/// `ALTER TABLE <table> ALTER COLUMN <column> DROP DEFAULT` の実行本体（TABLE-39 (2)、
/// Issue #1628）。前提・判定順序の骨格は [`execute_alter_table_alter_column_set_default`] と
/// 同じ（テーブル存在確認 → 列取得 → `catalog::Storage::alter_table_set_default_expr` に
/// `None` を渡す）。既定値の無い列への DROP は冪等に成功する。`fill_value` は変えない。
fn execute_alter_table_alter_column_drop_default(
    storage: &Storage,
    stmt: &ValidatedAlterTableAlterColumnDropDefault,
) -> Result<AlterTableOutcome, SqlSurfaceError> {
    let schema = match storage.get_table_schema(&stmt.table_name) {
        Ok(s) => s,
        Err(CatalogError::TableNotFound(_)) => {
            return Err(undefined_table_or_view(storage, &stmt.table_name));
        }
        Err(other) => return Err(map_drop_alter_column_error(other)),
    };
    let column = schema
        .columns
        .iter()
        .find(|c| c.name == stmt.column_name)
        .ok_or_else(|| SqlSurfaceError::UndefinedColumn {
            name: stmt.column_name.clone(),
        })?;
    storage
        .alter_table_set_default_expr(&stmt.table_name, &stmt.column_name, &column.ty, None)
        .map_err(|e| match e {
            CatalogError::TableNotFound(_) => undefined_table_or_view(storage, &stmt.table_name),
            other => map_alter_column_default_error(other),
        })?;
    Ok(AlterTableOutcome {
        table_name: stmt.table_name.clone(),
        action: AlterTableAction::AlterColumnDropDefault {
            column_name: stmt.column_name.clone(),
        },
    })
}

/// 列 DEFAULT 変更（`SET／DROP DEFAULT`。Issue #1628）の [`CatalogError`] を SQL 表層の契約へ
/// 写像する。FK の `SET DEFAULT` 参照アクションと整合しない変更は `42830`。全 variant を
/// 明示列挙する [`map_drop_alter_column_error`] へ残りを委譲する（ワイルドカード腕を置かない。
/// fail-closed）。
fn map_alter_column_default_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::InvalidForeignKey(detail) => SqlSurfaceError::invalid_foreign_key(detail),
        other => map_drop_alter_column_error(other),
    }
}

/// 列の nullability 変更（`SET NOT NULL`。Issue #1631）の [`CatalogError`] を SQL 表層の
/// 契約へ写像する。違反行はスキーマ情報（列名）しか持ち出さない（テナント・行 id・値・件数を
/// 含めない。ERR-6）。全 variant を明示列挙しワイルドカード腕を置かない（fail-closed）。
fn map_alter_column_nullability_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::NotNullConstraintViolation(column) => {
            SqlSurfaceError::not_null_violation(column)
        }
        CatalogError::ColumnInPrimaryKey(column) => SqlSurfaceError::column_in_primary_key(column),
        CatalogError::InvalidForeignKey(detail) => SqlSurfaceError::invalid_foreign_key(detail),
        // 残りは `map_drop_alter_column_error` の契約に従う（`TableNotFound`・
        // `ColumnNotFound`・`ProtectedColumn`・`Invalid`・`WriteLockTimeout` 等）。
        other => map_drop_alter_column_error(other),
    }
}

/// 列定義の `COLLATE` が列型に付けられるかを判定する（TABLE-28・Issue #1609）。照合を
/// 持てるのは文字列系（`TEXT`／`VARCHAR`／`CHAR`）だけで、それ以外のスカラー型は `42804`、
/// 配列は `0A000`（対象外）。照合名の解決（`42704`）は構文段で済んでいるため、判定順は
/// 名前 → 型になる。`CREATE TABLE`・`ADD COLUMN`・`ALTER COLUMN TYPE` の実行段が共有する。
fn check_column_collation(
    ty: &ColumnType,
    collation: Option<crate::collation::Collation>,
) -> Result<(), SqlSurfaceError> {
    if collation.is_none() || crate::collation::is_collatable(ty) {
        return Ok(());
    }
    if matches!(ty, ColumnType::Array(_)) {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "COLLATE on an array column is not supported".to_string(),
        });
    }
    Err(SqlSurfaceError::datatype_mismatch(
        "collations are not supported by this column type",
    ))
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
    limits: &SchemaLimits,
) -> Result<ColumnType, SqlSurfaceError> {
    match ty {
        SqlColumnTypeName::Vector(dim) => Ok(ColumnType::Vector(*dim)),
        other => resolve_column_type(storage, other, limits),
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
        | CatalogError::ViewDefinitionChanged(_)
        | CatalogError::ViewReferenceCycle(_)
        | CatalogError::ViewLimitExceeded(_)
        | CatalogError::TooManyColumns { .. }
        | CatalogError::IndexAlreadyExists(_)
        | CatalogError::IndexNotFound(_)
        | CatalogError::IndexKindMismatch(_)
        | CatalogError::IndexCollationNotSupported(_)
        | CatalogError::IndexLimitExceeded(_)
        | CatalogError::UniqueConstraintViolation
        | CatalogError::InvalidForeignKey(_)
        | CatalogError::ConstraintAlreadyExists(_)
        | CatalogError::MultiplePrimaryKeys(_)
        | CatalogError::ColumnInPrimaryKey(_)
        | CatalogError::ConstraintNotFound(_)
        | CatalogError::ConstraintLimitExceeded(_)
        | CatalogError::ForeignKeyViolation
        | CatalogError::NotNullConstraintViolation(_) => SqlSurfaceError::Internal {
            detail: "internal error".to_string(),
        },
    }
}

/// `RENAME COLUMN` の [`CatalogError`] を SQL 表層の契約へ写像する（TABLE-38 の
/// ポインタ、Issue #1619）。改名先の衝突 `ColumnAlreadyExists` は `42701`、それ以外は
/// [`map_drop_alter_column_error`] に委譲する（`ColumnAlreadyExists` は同関数では
/// Internal に落ちるため、ここで先に拾う）。
fn map_rename_column_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::ColumnAlreadyExists(name) => SqlSurfaceError::duplicate_column(name),
        // 内部列（`id`／`tenant_id`／`visibility`）は TABLE-38 に従い `42703`。
        CatalogError::ProtectedColumn(name) => SqlSurfaceError::UndefinedColumn { name },
        other => map_drop_alter_column_error(other),
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
            Ok(true) => true,
            // matview（TABLE-42・Issue #1664）も表と名前空間を共有する別種のオブジェクト。
            Ok(false) => match storage.materialized_view_definition(name) {
                Ok(def) => def.is_some(),
                Err(e) => return crate::catalog::table_lookup_error(e),
            },
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
    limits: &SchemaLimits,
) -> Result<ColumnType, SqlSurfaceError> {
    // 既存行が埋め込みバイトを持たないテーブルへの VECTOR 列追加は、
    // arena 構築・KNN・HNSW 各経路の安全性が未検証のため常に拒否する
    // （`sql::ddl_column_type` モジュールドキュメント参照）。
    if matches!(ty, SqlColumnTypeName::Vector(_)) {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "ALTER TABLE ADD COLUMN does not support VECTOR columns yet".to_string(),
        });
    }
    if let Some(alias) = alias_spelling(ty) {
        reject_alias_enum_collision(storage, alias)?;
    }
    // 明示 `[N]` が起動時設定 `--max-array-elements`（Issue #1548）を超える宣言は
    // `42601`。`[]` 省略は設定値を既定の要素数にする。
    if let SqlColumnTypeName::Array {
        max_len: Some(n), ..
    } = ty
    {
        if *n > limits.max_array_elements() {
            return Err(array_len_over_limit(limits.max_array_elements(), *n));
        }
    }
    match to_static_column_type_with_array_default(ty, limits.max_array_elements())? {
        StaticColumnType::Resolved(t) => Ok(t),
        StaticColumnType::EnumCandidate(name) => {
            Ok(ColumnType::Enum(resolve_enum_type(storage, &name)?))
        }
        // 要素が ENUM の配列（Issue #1357）。型名の存在確認（未登録は 42601）の
        // うえで登録済みの語彙を持つ配列型を組み立てる。
        StaticColumnType::ArrayOfEnumCandidate(name, max_len, ndims) => {
            let def = resolve_enum_type(storage, &name)?;
            let array = crate::catalog::ArrayType::new_enum(def, max_len)
                .and_then(|a| a.with_ndims(ndims))
                .map_err(|e| SqlSurfaceError::unsupported(format!("invalid array type: {e}")))?;
            Ok(ColumnType::Array(array))
        }
    }
}

/// 配列の要素数宣言が設定上限を超えたときの `42601`（Issue #1548）。文言は
/// `ArrayType::new` の範囲外エラー（`invalid array type: ...`）と同じ形に揃え、
/// 既定（設定値＝ハード上限）では従来と同一になる。
fn array_len_over_limit(max: u32, got: u32) -> SqlSurfaceError {
    SqlSurfaceError::unsupported(format!(
        "invalid array type: array max_len must be within 1..={max}, got {got}"
    ))
}

/// ENUM 型名を `Storage::get_enum_type` で解決する。未登録は `42704`
/// （`type does not exist`。Issue #2306。`ADD COLUMN`・`CREATE TABLE` 共通）。
/// 型名エイリアス（`INT4`・`INT8`・`FLOAT4`・`FLOAT8`。Issue #1528）と同じ綴りの ENUM 型が
/// 既に登録されている場合に `42601` で拒否する。エイリアス導入前は同綴りが ENUM 型名
/// として解決されていたため、組み込み型へ黙って解決し直すと既存の参照意図と食い違う
/// （fail-closed）。`CREATE TABLE`・`ADD COLUMN`・`ALTER COLUMN TYPE` が共有する。
pub(crate) fn reject_alias_enum_collision(
    storage: &Storage,
    alias: &str,
) -> Result<(), SqlSurfaceError> {
    match storage.get_enum_type(alias) {
        Ok(_) => Err(SqlSurfaceError::UnsupportedSyntax {
            detail: format!("type name {alias} collides with an existing enum type"),
        }),
        Err(CatalogError::TypeNotFound(_)) => Ok(()),
        Err(other) => Err(map_add_column_error(other)),
    }
}

fn resolve_enum_type(
    storage: &Storage,
    name: &str,
) -> Result<std::sync::Arc<crate::catalog::EnumTypeDef>, SqlSurfaceError> {
    storage.get_enum_type(name).map_err(|e| match e {
        CatalogError::TypeNotFound(_) => SqlSurfaceError::UndefinedType {
            name: name.to_string(),
        },
        other => map_add_column_error(other),
    })
}

/// 時刻 DEFAULT 列を `ON DELETE／UPDATE SET DEFAULT` の対象にする FK 宣言を `0A000` で
/// 拒否する（SQL-37・Issue #1587、設計 D9。連鎖更新は台帳へ記録する主体の文が無く、
/// 評価時刻を再送で再現できないため fail-closed）。`CREATE TABLE`・`ALTER TABLE ADD FOREIGN
/// KEY` の両方が呼ぶ。参照元列が存在しない場合は後段の検証が従来どおり判定するため無視する。
fn reject_time_default_set_default(
    columns: &[ColumnDef],
    foreign_keys: &[crate::catalog::ForeignKeyDef],
) -> Result<(), SqlSurfaceError> {
    for fk in foreign_keys {
        if !fk.uses_set_default() {
            continue;
        }
        let has_time_default = fk.columns().iter().any(|name| {
            columns.iter().any(|c| {
                &c.name == name && matches!(c.default_expr, Some(ColumnDefault::CurrentTime(_)))
            })
        });
        if has_time_default {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: "SET DEFAULT referential action on a column with a time function DEFAULT is not supported".to_string(),
            });
        }
    }
    Ok(())
}

/// 時刻値関数の `DEFAULT`（`DEFAULT now()` 系。SQL-37・TABLE-16・Issue #1587）を
/// 列型との適合だけ検査して [`ColumnDefault::CurrentTime`] にする（評価しない。評価は
/// INSERT の省略列補完のみ）。`TIMESTAMP` 系関数は `TIMESTAMP` 列、`CURRENT_DATE` は
/// `DATE` 列に限る。不一致は定数 DEFAULT の型不一致と同じ `42601`。
pub(crate) fn time_column_default(
    column_name: &str,
    ty: &ColumnType,
    time: crate::catalog::TimeDefault,
) -> Result<ColumnDefault, SqlSurfaceError> {
    time_column_default_with(column_name, ty, time, SqlSurfaceError::unsupported)
}

/// `ALTER COLUMN SET DEFAULT` 用の [`time_column_default`]（Issue #1628）。不一致は
/// `42804`（`ADD COLUMN`／`CREATE TABLE` の `42601` は既存契約のため据え置き、SET DEFAULT
/// の経路でだけ `42804` にする）。
fn time_column_default_for_set(
    column_name: &str,
    ty: &ColumnType,
    time: crate::catalog::TimeDefault,
) -> Result<ColumnDefault, SqlSurfaceError> {
    time_column_default_with(column_name, ty, time, SqlSurfaceError::datatype_mismatch)
}

/// [`time_column_default`] の本体。型不一致のエラー生成を呼び出し側が選ぶ。
fn time_column_default_with(
    column_name: &str,
    ty: &ColumnType,
    time: crate::catalog::TimeDefault,
    mismatch: fn(String) -> SqlSurfaceError,
) -> Result<ColumnDefault, SqlSurfaceError> {
    if time.compatible_with(ty) {
        Ok(ColumnDefault::CurrentTime(time))
    } else {
        Err(mismatch(format!(
            "column {column_name:?} DEFAULT time function does not match the column type"
        )))
    }
}

/// `ADD COLUMN` の `DEFAULT` リテラルを列型に応じた [`ColumnDefault`] へ変換する
/// （`CREATE TABLE` の列 DEFAULT と同じ規則。Issue #1169）。DEFAULT 非対応の列型
/// （VECTOR 等）は `0A000`、対応型でリテラル種別が合わない場合は `42601`、
/// 長さ上限超過は `54000`、BYTEA の `\x..` 形式不正は `22P02`（Issue #1373）。
pub(crate) fn add_column_default(
    column_name: &str,
    ty: &ColumnType,
    literal: &InsertLiteral,
) -> Result<ColumnDefault, SqlSurfaceError> {
    column_default_from_literal(column_name, ty, literal, SqlSurfaceError::unsupported)
}

/// `ALTER COLUMN SET DEFAULT` 用の [`add_column_default`]（Issue #1628）。変換規則は同一で、
/// リテラル種別と列型の不一致だけを `42804`（DATATYPE_MISMATCH）で返す。
fn set_column_default_value(
    column_name: &str,
    ty: &ColumnType,
    literal: &InsertLiteral,
) -> Result<ColumnDefault, SqlSurfaceError> {
    column_default_from_literal(column_name, ty, literal, SqlSurfaceError::datatype_mismatch)
}

/// [`add_column_default`]・[`set_column_default_value`] の共通本体。種別不一致のエラー生成
/// （`mismatch_error`）だけを呼び出し側が選び、その他の SQLSTATE（`0A000`・`54000`・
/// `22001`・`22P02` 等）は両経路で同一にする。
fn column_default_from_literal(
    column_name: &str,
    ty: &ColumnType,
    literal: &InsertLiteral,
    mismatch_error: fn(String) -> SqlSurfaceError,
) -> Result<ColumnDefault, SqlSurfaceError> {
    let kind = match ty {
        ColumnType::Text | ColumnType::Varchar(_) | ColumnType::Bpchar(_) => "text",
        ColumnType::SmallInt
        | ColumnType::Integer
        | ColumnType::BigInt
        | ColumnType::Real
        | ColumnType::Double
        | ColumnType::Numeric { .. } => "numeric",
        ColumnType::Boolean => "boolean",
        ColumnType::Date => "date",
        ColumnType::Enum(_) => "enum",
        ColumnType::Uuid => "uuid",
        ColumnType::Timestamp => "timestamp",
        ColumnType::Time => "time",
        // TIMESTAMPTZ の既定値は文字列リテラルの原文を保持し、解釈は束縛時
        // （`parser::bind_column_default`）に行う（Issue #1574）。
        ColumnType::TimestampTz => "timestamptz",
        // INTERVAL の既定値も文字列リテラルの原文を保持し、解釈は束縛時
        // （`parser::bind_column_default`）に行う（Issue #1579）。
        ColumnType::Interval => "interval",
        ColumnType::Json => "json",
        ColumnType::Jsonb => "jsonb",
        ColumnType::Bytea => "bytea",
        // 配列の `{..}` 原文はここでは種別と長さだけを検査し、要素の解析はしない
        // （Issue #1374）。`CREATE TABLE` の構文段（`allowlist::parse_create_table_column`）は
        // ENUM 要素の語彙が未解決の配列型でこの関数を呼ぶため、ここで解析すると
        // `mood[] DEFAULT '{happy}'` を誤って拒否する。要素の検証は語彙解決後の
        // `parser::bind_column_default`（`execute_create_table`・
        // `execute_alter_table_add_column` が永続化前に必ず通す）が INSERT と同じ
        // `parse_array_literal` で行う。BYTEA の形に揃えて解析をここへ移さないこと。
        ColumnType::Array(_) => "array",
        _ => {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: format!("column {column_name:?}: DEFAULT is not supported for this type"),
            })
        }
    };
    let mismatch = || {
        mismatch_error(format!(
            "column {column_name:?} DEFAULT expects a {kind} literal"
        ))
    };
    match literal {
        // 定数式（`TIMESTAMPTZ '...'`・`timezone(...)`／`AT TIME ZONE` の結果）が畳み込まれた
        // 絶対時刻（Issue #1576・TABLE-33）。日時列の保持形（テキスト）へ DDL 時点の
        // セッション TimeZone で変換する。TIMESTAMPTZ は UTC の明示 offset 付き表記へ
        // 正規化し、読み出し時補完がセッションによらず同じ値になるようにする。
        InsertLiteral::TimestampTz(micros)
            if matches!(
                ty,
                ColumnType::TimestampTz | ColumnType::Timestamp | ColumnType::Date
            ) =>
        {
            let overflow = || {
                SqlSurfaceError::datetime_field_overflow(format!(
                    "column {column_name:?} DEFAULT is out of range for its type"
                ))
            };
            let text = match ty {
                ColumnType::TimestampTz => {
                    if !crate::datetime::validate_timestamptz_micros(*micros) {
                        return Err(overflow());
                    }
                    crate::datetime::format_timestamptz(
                        *micros,
                        &crate::datetime::SessionTimeZone::utc(),
                    )
                }
                ColumnType::Timestamp => crate::datetime::with_current_session_time_zone(|tz| {
                    tz.timestamptz_to_timestamp(*micros)
                })
                .map(crate::datetime::format_timestamp)
                .ok_or_else(overflow)?,
                _ => crate::datetime::with_current_session_time_zone(|tz| {
                    tz.timestamptz_to_date(*micros)
                })
                .map(crate::datetime::format_date)
                .ok_or_else(overflow)?,
            };
            Ok(ColumnDefault::Text(text))
        }
        // `DATE`／`TIMESTAMP`／`UUID`／`ENUM` は文字列リテラルの原文を保持し、解釈（ENUM は語彙照合）は
        // 束縛時（`parser::bind_column_default`）に行う（Issue #1279・#1280・#1281・#1282）。
        // ENUM の型名解決（未登録は 42601）は DEFAULT 評価より先に済んでいる。
        InsertLiteral::String(s)
            if kind == "text"
                || kind == "array"
                || kind == "date"
                || kind == "timestamp"
                || kind == "time"
                || kind == "timestamptz"
                || kind == "interval"
                || kind == "uuid"
                || kind == "enum"
                || kind == "json" =>
        {
            if s.len() > MAX_COLUMN_DEFAULT_LEN {
                return Err(default_too_long(column_name));
            }
            // `VARCHAR(n)` の既定値は定義時に n 文字へ収める（超過は `22001` で DDL を拒否。
            // 超過部分が半角空白のみなら切り詰めて保存する。TABLE-31・Issue #1564）。
            // PostgreSQL は INSERT 時に判定するが、ADD COLUMN では既存行が既定値を読むため
            // 格納不変条件を保つ目的で定義時に fail-closed とする。
            // `CHAR(n)` の既定値は n 文字へ詰めた形で保持する（読み出し時補完が借用で返せる
            // よう、保持値＝格納形とする。詰め後の長さも上限で検査し、n が確保量を決めない。
            // TABLE-31・Issue #1566）。
            let stored = crate::row_codec::fit_pad_char_length(ty, s, MAX_COLUMN_DEFAULT_LEN)?;
            Ok(ColumnDefault::Text(stored.into_owned()))
        }
        // `JSONB` は DDL 時に正規化した形で保持する（読み出し時補完が借用で返せるよう、
        // 保持値＝格納形とする。Issue #1337）。エラー文言に値は含めない。
        InsertLiteral::String(s) if kind == "jsonb" => {
            if s.len() > MAX_COLUMN_DEFAULT_LEN {
                return Err(default_too_long(column_name));
            }
            let canonical = match crate::json::canonicalize_jsonb_text(s) {
                Ok(c) => c,
                Err(crate::json::JsonColumnError::TooLong) => {
                    return Err(default_too_long(column_name))
                }
                Err(crate::json::JsonColumnError::Invalid) => {
                    return Err(SqlSurfaceError::invalid_text_representation(format!(
                        "column {column_name:?} DEFAULT has an invalid input syntax for its type"
                    )))
                }
            };
            if canonical.len() > MAX_COLUMN_DEFAULT_LEN {
                return Err(default_too_long(column_name));
            }
            Ok(ColumnDefault::Text(canonical))
        }
        // `BYTEA` は `\x..` 原文を保持する（正規化しない）。形式の検証はここで
        // `bind_bytea_literal`（INSERT と同じ写像）に任せて 22P02 を返し、読み出し時補完は
        // `TableSchema::bytea_default_bytes` が原文をデコードして借用で渡す（Issue #1373）。
        InsertLiteral::String(s) if kind == "bytea" => {
            if s.len() > MAX_COLUMN_DEFAULT_LEN {
                return Err(default_too_long(column_name));
            }
            crate::sql::parser::bind_bytea_literal(s, column_name)?;
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

/// `TIMESTAMPTZ` 列の既定値リテラルを、DDL 実行時点のセッション TimeZone
/// （[`crate::datetime::current_session_time_zone`]。engine の各セッション入口が張るスコープ）で
/// 解釈し、UTC の明示 offset 付き表記（`...+00`）へ正規化して保持する（TABLE-33・Issue #1575）。
///
/// 呼び出し元は `execute_create_table`／`execute_alter_table_add_column`（永続化前）。
/// 読み出し時補完（`row_codec::default_scalar`）は常に UTC で解釈するが、正規化後の値は
/// 明示 offset を持つためセッションによらず同じ値になる（既存カタログの TZ 無し既定値は
/// 当時の DDL セッション＝常に UTC と同じ解釈のまま）。TZ 付きの既定値も同じ UTC 表記へ揃う。
/// 解釈できない値は DDL を拒否する（エラー文言に値を含めない）。
fn normalize_timestamptz_default(column: &mut ColumnDef) -> Result<(), SqlSurfaceError> {
    if column.ty != ColumnType::TimestampTz {
        return Ok(());
    }
    // `default_expr` と `fill_value` の両方を同じ規則で正規化する（Issue #1627。
    // ミラー状態なら同じ値になる）。
    normalize_timestamptz_slot(&column.name, &mut column.default_expr)?;
    normalize_timestamptz_slot(&column.name, &mut column.fill_value)
}

/// `default_expr` スロットだけを `TIMESTAMPTZ` 向けに正規化する（`ALTER COLUMN SET DEFAULT`。
/// Issue #1628）。[`normalize_timestamptz_default`] は `fill_value` まで書き換えるため、補完値を
/// 変えてはならない本経路では使わない。
fn normalize_timestamptz_slot_if_needed(column: &mut ColumnDef) -> Result<(), SqlSurfaceError> {
    if column.ty != ColumnType::TimestampTz {
        return Ok(());
    }
    normalize_timestamptz_slot(&column.name, &mut column.default_expr)
}

/// [`normalize_timestamptz_default`] の 1 スロット分（INSERT 既定値または補完値）。
fn normalize_timestamptz_slot(
    column_name: &str,
    slot: &mut Option<ColumnDefault>,
) -> Result<(), SqlSurfaceError> {
    use crate::datetime::{DateTimeLiteralError, TimestampTzLiteralError};
    let Some(ColumnDefault::Text(text)) = &*slot else {
        return Ok(());
    };
    let tz = crate::datetime::current_session_time_zone();
    match crate::datetime::parse_timestamptz(text, &tz) {
        Ok(micros) => {
            *slot = Some(ColumnDefault::Text(crate::datetime::format_timestamptz(
                micros,
                &crate::datetime::SessionTimeZone::utc(),
            )));
            Ok(())
        }
        Err(TimestampTzLiteralError::Literal(DateTimeLiteralError::Format(_))) => {
            Err(SqlSurfaceError::invalid_datetime_format(format!(
                "column {:?} DEFAULT has an invalid datetime format for its type",
                column_name
            )))
        }
        Err(TimestampTzLiteralError::Literal(DateTimeLiteralError::Overflow(_))) => {
            Err(SqlSurfaceError::datetime_field_overflow(format!(
                "column {:?} DEFAULT is out of range for its type",
                column_name
            )))
        }
        Err(TimestampTzLiteralError::UnknownTimeZone) => {
            Err(SqlSurfaceError::InvalidParameterValue {
                detail: "time zone not recognized",
            })
        }
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
        CatalogError::TypeNotFound(name) => SqlSurfaceError::UndefinedType { name },
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
        | CatalogError::ViewDefinitionChanged(_)
        | CatalogError::ViewReferenceCycle(_)
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
        | CatalogError::IndexCollationNotSupported(_)
        | CatalogError::IndexLimitExceeded(_)
        // `FOREIGN KEY` 宣言の照合（`create_table` 専用。TABLE-17・TASK-205、
        // Issue #907）は `alter_table_add_column` からは返らない（到達不能）。
        | CatalogError::InvalidForeignKey(_)
        // 制約名の操作（`ALTER TABLE ... ADD/DROP CONSTRAINT`。Issue #1067）専用の
        // 変種で、`alter_table_add_column` からは返らない（到達不能）。
        | CatalogError::ConstraintAlreadyExists(_)
        | CatalogError::MultiplePrimaryKeys(_)
        | CatalogError::ColumnInPrimaryKey(_)
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

/// `CREATE MATERIALIZED VIEW ...`（TABLE-42・Issue #1664）の成功応答。
///
/// テナント境界（P0）: 行数は **作成したセッション自身の文脈の実体の行数だけ**を運ぶ。他の文脈
/// （他テナント・別の可視性集合）の件数を合算すると、DDL 権限を持つ主体に他テナントの件数が
/// 漏れるため、合算は構造的にできない（フィールドを 1 つの `Option<u64>` に限る）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CreateMaterializedViewOutcome {
    /// `IF NOT EXISTS` により同名の relation があって何もしなかったか。
    pub(crate) skipped: bool,
    /// `WITH DATA` で初回実体化したか（`WITH NO DATA`・`skipped` では偽）。
    pub(crate) populated: bool,
    /// 作成者の文脈の実体の行数（`WITH DATA` で作成者の文脈が実体化対象だったときだけ `Some`）。
    pub(crate) own_rows: Option<u64>,
}

impl CreateMaterializedViewOutcome {
    /// `IF NOT EXISTS` の no-op か。
    pub fn skipped(&self) -> bool {
        self.skipped
    }

    /// 初回実体化を行ったか。
    pub fn populated(&self) -> bool {
        self.populated
    }

    /// 作成者自身の文脈の実体の行数（`SELECT <n>` タグ用）。他文脈の件数は含まない。
    /// 実体化していない場合・作成者の文脈が実体化対象外の場合は `None`。
    pub fn own_rows(&self) -> Option<u64> {
        self.own_rows
    }
}

/// `CatalogError` を `CREATE MATERIALIZED VIEW`（TABLE-42・Issue #1664）の SQL 表層の契約へ写像する。
/// 名前衝突 `42P07`、参照先不存在 `42P01`、件数・行数・本文サイズ上限 `54000`、書き込みゲート待機の
/// 超過 `55P03`。文言に他テナントの情報・内部詳細を含めない。
pub(crate) fn map_create_matview_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::TableAlreadyExists(name) => SqlSurfaceError::DuplicateTable { name },
        CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        CatalogError::ViewLimitExceeded(detail) => SqlSurfaceError::PayloadTooLarge { detail },
        // 一意索引（Issue #1667）の宣言がある matview の実体書き込みで文脈内に重複があった。
        // 固定文言で、テナント・値・件数を含めない。
        CatalogError::UniqueConstraintViolation => SqlSurfaceError::UniqueViolation,
        CatalogError::Invalid(_) => SqlSurfaceError::feature_not_supported(
            "materialized view definition or data cannot be stored",
        ),
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        _ => SqlSurfaceError::Internal {
            detail: "CREATE MATERIALIZED VIEW failed".to_string(),
        },
    }
}

/// `REFRESH MATERIALIZED VIEW ...`（TABLE-42・Issue #1665）の成功応答。
///
/// テナント境界（P0）: 件数を運ばない（pg の固定タグ `REFRESH MATERIALIZED VIEW` に件数は無く、
/// 他文脈の件数を DDL 権限を持つ主体へ返せない構造にする）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefreshMaterializedViewOutcome {
    /// `WITH DATA` で再実体化したか（`WITH NO DATA` では偽）。
    pub(crate) populated: bool,
}

impl RefreshMaterializedViewOutcome {
    /// 再実体化を行ったか（`WITH NO DATA` なら偽）。
    pub fn populated(&self) -> bool {
        self.populated
    }
}

/// `CatalogError` を `REFRESH MATERIALIZED VIEW`（TABLE-42・Issue #1665）の SQL 表層の契約へ写像する。
/// 不存在 `42P01`、種別違い `42809`、行数上限 `54000`、書き込みゲート待機の超過 `55P03`。
/// 文言に他テナントの情報・内部詳細を含めない。
pub(crate) fn map_refresh_matview_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::ViewNotFound(name) | CatalogError::TableNotFound(name) => {
            SqlSurfaceError::UndefinedTable { name }
        }
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::ViewLimitExceeded(detail) => SqlSurfaceError::PayloadTooLarge { detail },
        CatalogError::UniqueConstraintViolation => SqlSurfaceError::UniqueViolation,
        CatalogError::Invalid(_) => SqlSurfaceError::feature_not_supported(
            "materialized view definition or data cannot be stored",
        ),
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        _ => SqlSurfaceError::Internal {
            detail: "REFRESH MATERIALIZED VIEW failed".to_string(),
        },
    }
}

/// `DROP VIEW <name>`（TABLE-18・SQL-23・TASK-205、Issue #909）の成功応答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropViewOutcome {
    /// `IF EXISTS` で全対象が無く何も削除しなかったか（Issue #1614・TABLE-36 ポインタ。
    /// 複数指定〔TABLE-37 (1)〕では「1 件も削除しなかった」）。
    /// CommandComplete のタグは通常の DROP と同一のため、wire 側は参照しない。
    pub(crate) skipped: bool,
    /// `CASCADE` で連鎖削除した依存ビュー（適用順。TABLE-37、Issue #1641）。
    /// **BREAKING CHANGE**: 本フィールドのため `Copy` を外した。
    pub(crate) cascaded: Vec<CascadedObject>,
}

impl DropViewOutcome {
    /// `IF EXISTS` により対象不在で no-op 成功した場合に真。
    pub fn skipped(&self) -> bool {
        self.skipped
    }

    /// `CASCADE` で連鎖削除した依存オブジェクト（`RESTRICT` 時は空）。
    pub fn cascaded(&self) -> &[CascadedObject] {
        &self.cascaded
    }
}

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
        .create_view_with_column_aliases(
            validated.name(),
            validated.base_relation(),
            validated.body_sql(),
            validated.column_aliases(),
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
    // `IF EXISTS` かつ対象不在（`ViewNotFound`）だけを成功へ読み替える。`Storage::drop_view` は
    // `ViewNotFound` を commit 前に返し txn が破棄されるため、カタログ・台帳・世代は変わらない。
    // 種類違い（`42809`）・依存（`2BP01`）・ロック timeout 等は読み替えず従来の写像へ流す
    // （fail-closed。権限ゲートは呼び出し元で通過済みのため存在オラクルにならない）。
    match storage.drop_view_with_behavior(validated.name(), validated.behavior()) {
        Ok(cascaded) => Ok(DropViewOutcome {
            skipped: false,
            cascaded,
        }),
        Err(CatalogError::ViewNotFound(_)) if validated.if_exists() => Ok(DropViewOutcome {
            skipped: true,
            cascaded: Vec::new(),
        }),
        Err(e) => Err(map_drop_view_error(e)),
    }
}

/// `DROP MATERIALIZED VIEW ...`（TABLE-42 (5)・TABLE-37、Issue #1667）の成功応答。
/// 連鎖削除の報告には共有カタログのオブジェクト名だけを含み、テナント・行数の情報は持たない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropMaterializedViewOutcome {
    /// `IF EXISTS` で 1 件も削除しなかったか（全対象が不在）。
    pub(crate) skipped: bool,
    /// `CASCADE` で連鎖削除した依存オブジェクト（適用順。依存する側が先）。
    pub(crate) cascaded: Vec<CascadedObject>,
}

impl DropMaterializedViewOutcome {
    /// `IF EXISTS` により全対象が不在で no-op 成功した場合に真。
    pub fn skipped(&self) -> bool {
        self.skipped
    }

    /// `CASCADE` で連鎖削除した依存オブジェクト（`RESTRICT` 時は空）。
    pub fn cascaded(&self) -> &[CascadedObject] {
        &self.cascaded
    }
}

/// `require_ddl_permission` を通過したセッションに限り呼ばれる実行本体。
/// `Storage::drop_materialized_views` へ委譲する（全体で単一 write txn・単一 commit）。
pub(crate) fn execute_drop_materialized_view(
    storage: &Storage,
    validated: &ValidatedDropMaterializedView,
) -> Result<DropMaterializedViewOutcome, SqlSurfaceError> {
    let report = storage
        .drop_materialized_views(
            validated.names(),
            validated.if_exists(),
            validated.behavior(),
        )
        .map_err(map_drop_matview_error)?;
    Ok(DropMaterializedViewOutcome {
        skipped: report.dropped == 0,
        cascaded: report.cascaded,
    })
}

/// `Storage::drop_materialized_views` の [`CatalogError`] を SQL 表層の契約へ写像する
/// （対象不存在 `42P01`、表・ビュー・索引名を指定 `42809`、依存が残存 `2BP01`、書き込みゲート
/// 待機の超過 `55P03`）。文言に他テナントの情報・内部詳細を含めない。
pub(crate) fn map_drop_matview_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::ViewNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::DependentViewsExist(name)
        | CatalogError::DependentObjectsStillExist(name) => {
            SqlSurfaceError::DependentObjectsStillExist { name }
        }
        CatalogError::Invalid(_) => {
            SqlSurfaceError::unsupported("malformed relation reference in DROP MATERIALIZED VIEW")
        }
        CatalogError::ViewLimitExceeded(detail) => SqlSurfaceError::PayloadTooLarge { detail },
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        _ => SqlSurfaceError::Internal {
            detail: "DROP MATERIALIZED VIEW failed".to_string(),
        },
    }
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

/// `CREATE OR REPLACE VIEW`（TABLE-41 (2)・Issue #1660）の実行本体。`require_ddl_permission` と
/// 互換性検査（`core`）を通過したセッションに限り呼ばれ、`expected_old`（検査に使った旧定義）を
/// `Storage::create_or_replace_view_checked` の compare-and-swap へ渡す（旧定義と、検査が参照した
/// スキーマの指紋）と、新定義が列を末尾に追加するか（`appends_columns`。依存ビューの保守的ゲート用。
/// Issue #1661）。旧定義または参照スキーマが検査後に変わっていた
/// 場合は [`CatalogError::ViewDefinitionChanged`] が返り、呼び出し元が検査からやり直す。
pub(crate) fn execute_create_or_replace_view(
    storage: &Storage,
    validated: &ValidatedCreateView,
    expected_old: Option<&crate::catalog::ViewDef>,
    expected_fingerprint: Option<&[u8]>,
    appends_columns: bool,
) -> Result<CreateViewOutcome, CatalogError> {
    storage.create_or_replace_view_checked(
        validated.name(),
        validated.base_relation(),
        validated.body_sql(),
        validated.column_aliases(),
        expected_old,
        expected_fingerprint,
        appends_columns,
    )?;
    Ok(CreateViewOutcome {})
}

/// [`execute_create_or_replace_view`] の [`CatalogError`] を SQL 表層の契約へ写像する
/// （種別違い `42809`、自己参照・循環 `42P17`、依存ビューを無効化する置換 `0A000`〔Issue #1661〕、
/// 競合による旧定義の変化 `55P03`。他は [`map_create_view_error`] と同じ）。
pub(crate) fn map_replace_view_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::ViewReferenceCycle(_) => SqlSurfaceError::InvalidObjectDefinition {
            detail: "view definition must not reference the view itself",
        },
        CatalogError::DependentViewsExist(_) => SqlSurfaceError::feature_not_supported(
            "cannot replace view because a dependent view would become invalid",
        ),
        CatalogError::ViewDefinitionChanged(_) => SqlSurfaceError::LockNotAvailable,
        other => map_create_view_error(other),
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
pub struct DropIndexOutcome {
    /// `IF EXISTS` で全対象が無く何も削除しなかったか（Issue #1614・TABLE-36 ポインタ。
    /// 複数指定〔TABLE-37 (1)〕では「1 件も削除しなかった」）。
    /// CommandComplete のタグは通常の DROP と同一のため、wire 側は参照しない。
    pub(crate) skipped: bool,
}

impl DropIndexOutcome {
    /// `IF EXISTS` により対象不在で no-op 成功した場合に真。
    pub fn skipped(&self) -> bool {
        self.skipped
    }
}

/// `require_ddl_permission` を通過したセッションに限り呼ばれる実行本体
/// （TASK-206・INDEX-7、Issue #908）。`crate::catalog::Storage::create_index`
/// （単一 write txn 内で名前衝突・対象の種別と存在・列整合・件数上限を判定して
/// 保存し、対象テーブルの世代を進める）へ委譲する。構文検証段
/// （`validate_create_index_tokens`）はカタログを一切照会していない。
pub(crate) fn execute_create_index(
    storage: &Storage,
    validated: &ValidatedCreateIndex,
) -> Result<CreateIndexOutcome, SqlSurfaceError> {
    // 防御ゲート（INDEX-10、Issue #1648）: hnsw と式要素の組み合わせは構文段の最終ゲートで拒否
    // 済みだが、将来の経路変更で届いても、式を黙って落として列だけで保存しない（fail-closed）。
    // 式要素そのものは `create_index_bound` が書き込み txn 内で束縛する。一意索引と式要素の組
    // （Issue #1651）は受理し、`create_index_bound` が強制の前提を同一 txn で整える。
    let has_expressions = validated.expression_keys().iter().any(Option::is_some);
    if has_expressions && validated.is_hnsw() {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "expression indexes are not supported for this index kind yet".to_string(),
        });
    }
    // 防御ゲート（INDEX-11、Issue #1653）: 述語付きは構文段の最終ゲートで拒否済みだが、将来の
    // 経路変更で届いても、述語を黙って落として列だけの索引を保存しない（fail-closed）。
    // Issue #1654 が述語の格納を実装する際に撤去する。
    if validated.predicate().is_some() {
        return Err(SqlSurfaceError::FeatureNotSupported {
            detail: "partial indexes are not supported yet".to_string(),
        });
    }
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
    )
    .with_column_collations(validated.column_collations().to_vec())
    // `CREATE UNIQUE INDEX`（Issue #1616・INDEX-9）。UNIQUE 制約への展開と既存行の重複検査は
    // `Storage::create_index` が同一 write txn で行う。
    .with_unique(validated.is_unique());
    // `IF NOT EXISTS`（TABLE-36、Issue #1613）。`create_index` の write txn は名前衝突を最初に
    // 判定し commit・世代 bump の前に返るため、同名があれば定義を比較せず副作用なしで成功にする。
    // 式要素（INDEX-10、Issue #1648）の束縛エラーは `Sql` としてそのまま透過し、カタログ側の
    // 失敗だけを `map_create_index_error` で写像する。
    match storage.create_index_bound(&def, validated.expression_keys()) {
        Ok(()) => {}
        Err(CreateIndexError::Catalog(CatalogError::IndexAlreadyExists(_)))
            if validated.if_not_exists() => {}
        Err(CreateIndexError::Catalog(e)) => return Err(map_create_index_error(e)),
        Err(CreateIndexError::Sql(e)) => return Err(e),
    }
    Ok(CreateIndexOutcome {})
}

/// `require_ddl_permission` を通過したセッションに限り呼ばれる実行本体
/// （TASK-206・INDEX-7、Issue #908）。`crate::catalog::Storage::drop_index` へ委譲する。
pub(crate) fn execute_drop_index(
    storage: &Storage,
    validated: &ValidatedDropIndex,
) -> Result<DropIndexOutcome, SqlSurfaceError> {
    // `IF EXISTS` かつ対象不在（`IndexNotFound`）だけを成功へ読み替える。`Storage::drop_index` は
    // `IndexNotFound` を commit 前に返し txn が破棄されるため、カタログ・台帳・世代は変わらない。
    // 種類違い（`42809`）・依存（`2BP01`）・ロック timeout 等は読み替えず従来の写像へ流す
    // （fail-closed。権限ゲートは呼び出し元で通過済みのため存在オラクルにならない）。
    match storage.drop_index(validated.name()) {
        Ok(()) => Ok(DropIndexOutcome { skipped: false }),
        Err(CatalogError::IndexNotFound(_)) if validated.if_exists() => {
            Ok(DropIndexOutcome { skipped: true })
        }
        Err(e) => Err(map_drop_index_error(e)),
    }
}

/// `Storage::create_index` の [`CatalogError`] を SQL 表層の契約へ写像する
/// （ERR-6: 名前衝突 `42P07`、対象テーブル不存在 `42P01`、対象がビュー `42809`、
/// 列不存在 `42703`、種別と列型の不整合 `0A000`、照合を持てない列への `COLLATE` `42804`、
/// 件数上限 `54000`）。エラー文言に
/// テナント・行内容・redb 内部詳細は含めない（security.md P0）。
fn map_create_index_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::IndexAlreadyExists(name) => SqlSurfaceError::DuplicateTable { name },
        CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable { name },
        CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType { name },
        CatalogError::ColumnNotFound(name) => SqlSurfaceError::UndefinedColumn { name },
        CatalogError::IndexKindMismatch(detail) => SqlSurfaceError::FeatureNotSupported { detail },
        // 照合を持てない列（`id`・数値・`VECTOR` 等）・`USING hnsw` への `COLLATE`（Issue #2191。
        // PostgreSQL と同じ `42804`）。
        CatalogError::IndexCollationNotSupported(detail) => {
            SqlSurfaceError::datatype_mismatch(detail)
        }
        CatalogError::IndexLimitExceeded(detail) => SqlSurfaceError::PayloadTooLarge { detail },
        // 一意索引（`CREATE UNIQUE INDEX`。Issue #1616・INDEX-9）: 既存行の重複は `23505`
        // （固定文言。テナント・値・件数を含めない）、制約名前空間の衝突は `42P07`、
        // 制約数上限は `54000`。
        CatalogError::UniqueConstraintViolation => SqlSurfaceError::UniqueViolation,
        CatalogError::ConstraintAlreadyExists(name) => SqlSurfaceError::DuplicateTable { name },
        CatalogError::ConstraintLimitExceeded(detail) => {
            SqlSurfaceError::PayloadTooLarge { detail }
        }
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
        // 一意索引が他の FOREIGN KEY の参照先になっている（Issue #1616・INDEX-9）。
        CatalogError::DependentObjectsStillExist(name) => {
            SqlSurfaceError::DependentObjectsStillExist { name }
        }
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
pub struct DropTypeOutcome {
    /// `IF EXISTS` で全対象が無く何も削除しなかったか（Issue #1614・TABLE-36 ポインタ。
    /// 複数指定〔TABLE-37 (1)〕では「1 件も削除しなかった」）。
    /// CommandComplete のタグは通常の DROP と同一のため、wire 側は参照しない。
    pub(crate) skipped: bool,
}

impl DropTypeOutcome {
    /// `IF EXISTS` により対象不在で no-op 成功した場合に真。
    pub fn skipped(&self) -> bool {
        self.skipped
    }
}

/// `require_ddl_permission` を通過したセッションに限り呼ばれる実行本体
/// （Issue #1194）。`Storage::create_enum_type`（単一 write txn 内で名前検証・
/// 重複判定・件数上限を行う）へ委譲する。ENUM 型は全テナント共有カタログのため
/// `PolicyContext` は取らない。
pub(crate) fn execute_create_type(
    storage: &Storage,
    validated: &ValidatedCreateType,
    limits: &SchemaLimits,
) -> Result<CreateTypeOutcome, SqlSurfaceError> {
    // 起動時設定 `--max-enum-labels`（Issue #1548）。storage 側の `CatalogError::Invalid`
    // は `42601` に写像されるため、構文段と同じ `54000` を保つにはここで先に判定する。
    if validated.labels().len() > limits.max_enum_labels() {
        return Err(SqlSurfaceError::payload_too_large("too many enum labels"));
    }
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
    // `IF EXISTS` かつ対象不在（`TypeNotFound`）だけを成功へ読み替える。`Storage::drop_enum_type` は
    // `TypeNotFound` を commit 前に返し txn が破棄されるため、カタログ・台帳・世代は変わらない。
    // 種類違い（`42809`）・依存（`2BP01`）・ロック timeout 等は読み替えず従来の写像へ流す
    // （fail-closed。権限ゲートは呼び出し元で通過済みのため存在オラクルにならない）。
    match storage.drop_enum_type(validated.name()) {
        Ok(()) => Ok(DropTypeOutcome { skipped: false }),
        Err(CatalogError::TypeNotFound(_)) if validated.if_exists() => {
            Ok(DropTypeOutcome { skipped: true })
        }
        Err(e) => Err(map_drop_type_error(e)),
    }
}

/// `Storage::create_enum_type` の [`CatalogError`] を SQL 表層の契約へ写像する
/// （型名重複は `42710`（Issue #1405・TABLE-14・ERR-6）。定義不正・型数上限は `42601`）。
/// エラー文言にはラベル・テナント・redb 内部詳細を含めない（security.md P0）。
fn map_create_type_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::TypeAlreadyExists(name) => SqlSurfaceError::duplicate_type(name),
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

    /// `IF EXISTS` の対象不在 no-op は全体世代・テーブル単位世代・索引カタログ世代を
    /// 進めない（Issue #1614・TABLE-36 ポインタ）。対照として実在する索引の DROP は
    /// 索引カタログ世代を進める（検査が何も見ていない状態を防ぐ）。
    #[test]
    fn if_exists_noop_leaves_all_generations_untouched() {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};
        use crate::sql::allowlist::{
            ValidatedDropIndex, ValidatedDropTable, ValidatedDropType, ValidatedDropView,
        };
        use redb::ReadableDatabase;

        let path = crate::test_util::temp_db::unique_db_path("ddl-drop-ie-gen");
        let _guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");
        let snapshot = |st: &Storage| {
            let read = st.db().begin_read().expect("read txn");
            (
                st.current_generation().expect("generation"),
                st.table_generation("docs").expect("table generation"),
                crate::catalog::index_catalog_generation_in_txn(&read).expect("index generation"),
            )
        };
        let before = snapshot(&storage);

        let t = ValidatedDropTable {
            table_names: vec!["ghost".into()],
            if_exists: true,
            behavior: crate::catalog::DropBehavior::Restrict,
        };
        assert!(execute_drop_table(&storage, &t).expect("table").skipped());
        let v = ValidatedDropView {
            name: "ghost".into(),
            if_exists: true,
            behavior: crate::catalog::DropBehavior::Restrict,
        };
        assert!(execute_drop_view(&storage, &v).expect("view").skipped());
        let i = ValidatedDropIndex {
            name: "ghost".into(),
            if_exists: true,
        };
        assert!(execute_drop_index(&storage, &i).expect("index").skipped());
        let y = ValidatedDropType {
            name: "ghost".into(),
            if_exists: true,
        };
        assert!(execute_drop_type(&storage, &y).expect("type").skipped());
        assert_eq!(
            snapshot(&storage),
            before,
            "no-op must not bump generations"
        );

        // IF EXISTS なしでは従来どおり不在エラー。
        let strict = ValidatedDropIndex {
            name: "ghost".into(),
            if_exists: false,
        };
        assert_eq!(
            execute_drop_index(&storage, &strict)
                .expect_err("missing")
                .wire_code(),
            "42704"
        );
        assert_eq!(snapshot(&storage), before);
    }

    /// 未登録の型名は解決前後（race を含む）とも `42704` に写す（Issue #2306）。
    #[test]
    fn missing_type_maps_to_42704() {
        let e = map_add_column_error(CatalogError::TypeNotFound("t".to_string()));
        assert_eq!(e.wire_code(), "42704");
        assert_eq!(e.to_string(), "type does not exist: t");
    }

    /// 型 DDL のエラー写像（Issue #1194）。
    #[test]
    fn type_ddl_error_mapping() {
        let code = |e: SqlSurfaceError| e.wire_code();
        assert_eq!(
            code(map_create_type_error(CatalogError::TypeAlreadyExists(
                "t".into()
            ))),
            "42710"
        );
        // 制約用 variant への退行を防ぐ（Issue #1405）。
        let dup = map_create_type_error(CatalogError::TypeAlreadyExists("t".into()));
        assert!(matches!(dup, SqlSurfaceError::DuplicateType { .. }));
        assert!(dup.to_string().starts_with("type already exists"));
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

    /// 型名エイリアスと同綴りの ENUM 型が登録済みなら `42601` で拒否し、未登録なら通す
    /// （Issue #1528。判定は綴り非依存のため任意の ENUM 名で検証する）。
    #[test]
    fn alias_enum_collision_is_rejected_only_when_enum_exists() {
        let (storage, _guard) = tmp_storage("alias-enum-collision");
        assert!(reject_alias_enum_collision(&storage, "mood").is_ok());
        storage
            .create_enum_type("mood", vec!["a".to_string()])
            .expect("create enum");
        let e = reject_alias_enum_collision(&storage, "mood").expect_err("collision");
        assert_eq!(e.wire_code(), "42601");
    }

    /// テーブルとして存在しない名前は、ビューなら `42809`、どちらでもなければ
    /// `42P01`（`execute_alter_table_add_column` の事前確認・書き込み txn 内の
    /// 再確認〔競合でテーブルが消えた場合〕の双方が共有する判定。Issue #900）。
    #[test]
    fn undefined_table_or_view_distinguishes_view_and_missing() {
        let (storage, _guard) = tmp_storage("undefined-or-view");
        let validated = ValidatedCreateTable {
            if_not_exists: false,
            primary_key_name: None,
            alias_type_names: Vec::new(),
            array_len_defaulted: Vec::new(),
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
        execute_create_table(&storage, &validated, &SchemaLimits::default()).expect("create table");
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

    /// `RENAME COLUMN` の写像（Issue #1619）: 衝突は `42701`、不在は `42703`、
    /// 内部列は `42703`（TABLE-38）、ロック競合は既存写像に従う。
    #[test]
    fn rename_column_error_mapping() {
        assert_eq!(
            map_rename_column_error(CatalogError::ColumnAlreadyExists("c".to_string())).wire_code(),
            "42701"
        );
        assert_eq!(
            map_rename_column_error(CatalogError::ColumnNotFound("c".to_string())).wire_code(),
            "42703"
        );
        assert_eq!(
            map_rename_column_error(CatalogError::ProtectedColumn("id".to_string())).wire_code(),
            "42703"
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
        let cases: [(SqlSurfaceError, &str); 14] = [
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
            // 一意索引（Issue #1616・INDEX-9）。
            (
                map_create_index_error(CatalogError::UniqueConstraintViolation),
                "23505",
            ),
            (
                map_create_index_error(CatalogError::ConstraintAlreadyExists("c".into())),
                "42P07",
            ),
            (
                map_create_index_error(CatalogError::ConstraintLimitExceeded("n".into())),
                "54000",
            ),
            (
                map_drop_index_error(CatalogError::DependentObjectsStillExist("i".into())),
                "2BP01",
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
            if_not_exists: false,
            primary_key_name: None,
            alias_type_names: Vec::new(),
            array_len_defaulted: Vec::new(),
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
        execute_create_table(&storage, &validated, &SchemaLimits::default())
            .expect("create table must succeed");
        let schema = storage.get_table_schema("docs").expect("schema must exist");
        assert_eq!(schema.columns.len(), 2);
    }

    /// `IF NOT EXISTS`（TABLE-36）の no-op は表・索引カタログの世代を変えない。
    #[test]
    fn if_not_exists_noop_leaves_generations_unchanged() {
        let (storage, _guard) = tmp_storage("if-not-exists-gen");
        let mut validated = ValidatedCreateTable {
            if_not_exists: true,
            primary_key_name: None,
            alias_type_names: Vec::new(),
            array_len_defaulted: Vec::new(),
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
        execute_create_table(&storage, &validated, &SchemaLimits::default()).expect("create");
        let index = ValidatedCreateIndex {
            name: "i".to_string(),
            if_not_exists: true,
            table: "docs".to_string(),
            hnsw: false,
            columns: vec!["body".to_string()],
            expressions: vec![None],
            collations: vec![None],
            unique: false,
            predicate: None,
        };
        execute_create_index(&storage, &index).expect("create index");
        let snapshot = |storage: &Storage| {
            use redb::ReadableDatabase;
            let read = storage.db().begin_read().expect("read");
            (
                crate::catalog::table_generation_in_txn(&read, "docs").expect("table gen"),
                crate::catalog::index_catalog_generation_in_txn(&read).expect("index gen"),
            )
        };
        let before = snapshot(&storage);
        execute_create_table(&storage, &validated, &SchemaLimits::default()).expect("noop table");
        validated.table_name = "i".to_string();
        execute_create_table(&storage, &validated, &SchemaLimits::default()).expect("noop name");
        execute_create_index(&storage, &index).expect("noop index");
        assert_eq!(before, snapshot(&storage));
    }

    #[test]
    fn execute_create_table_rejects_duplicate_name() {
        let (storage, _guard) = tmp_storage("duplicate-name");
        let validated = ValidatedCreateTable {
            if_not_exists: false,
            primary_key_name: None,
            alias_type_names: Vec::new(),
            array_len_defaulted: Vec::new(),
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
        execute_create_table(&storage, &validated, &SchemaLimits::default())
            .expect("first create must succeed");
        let err = execute_create_table(&storage, &validated, &SchemaLimits::default())
            .expect_err("second create with same name must fail");
        assert!(matches!(err, SqlSurfaceError::DuplicateTable { .. }));
        assert_eq!(err.wire_code(), "42P07");
    }

    #[test]
    fn execute_create_table_rejects_two_vector_columns() {
        let (storage, _guard) = tmp_storage("two-vector");
        let validated = ValidatedCreateTable {
            if_not_exists: false,
            primary_key_name: None,
            alias_type_names: Vec::new(),
            array_len_defaulted: Vec::new(),
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
        let err = execute_create_table(&storage, &validated, &SchemaLimits::default())
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

    /// `ADD COLUMN IF NOT EXISTS` の skip と複数アクションの全 no-op は世代を進めず、途中失敗は
    /// カタログも世代も変えない。成功時だけ世代が 1 だけ進む（TABLE-36・TABLE-39 (5)、
    /// Issue #1633）。
    #[test]
    fn alter_table_if_not_exists_and_multi_action_generation_contract() {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};
        use crate::sql::allowlist::validate_alter_table;

        let path = crate::test_util::temp_db::unique_db_path("ddl-alter-multi-gen");
        let _guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("a", ColumnType::Text, true)],
            ))
            .expect("create table");
        let limits = SchemaLimits::default();
        let run = |sql: &str| {
            let v = validate_alter_table(sql).expect("valid shape");
            execute_alter_table(&storage, &v, &limits)
        };
        let generation = || storage.table_generation("docs").expect("generation");
        let schema = || storage.get_table_schema("docs").expect("schema");

        // 既存列への IF NOT EXISTS は skip（世代・スキーマ不変）。
        let (g0, s0) = (generation(), schema());
        let out = run("ALTER TABLE docs ADD COLUMN IF NOT EXISTS a TEXT").expect("skip");
        assert_eq!(
            out.action,
            AlterTableAction::AddColumnSkipped {
                column_name: "a".to_string()
            }
        );
        assert_eq!(generation(), g0);
        assert_eq!(schema(), s0);

        // 全アクションが no-op（skip と冪等な DROP DEFAULT）なら commit しない。
        run("ALTER TABLE docs ADD COLUMN IF NOT EXISTS a TEXT, ALTER COLUMN a DROP DEFAULT")
            .expect("all no-op");
        assert_eq!(generation(), g0);

        // 途中失敗（3 個目が存在しない列）は何も適用せず世代も進めない。
        let err = run("ALTER TABLE docs ADD COLUMN b TEXT, ADD COLUMN c INTEGER, DROP COLUMN zz")
            .expect_err("must fail");
        assert_eq!(err.wire_code(), "42703");
        assert_eq!(generation(), g0);
        assert_eq!(schema(), s0);

        // 成功は世代を 1 だけ進める（複数アクションでも bump は 1 回）。
        run("ALTER TABLE docs ADD COLUMN b TEXT, ADD COLUMN c INTEGER DEFAULT 1")
            .expect("multi success");
        assert_eq!(generation(), g0 + 1);
        assert_eq!(schema().columns.len(), 3);
    }
}
