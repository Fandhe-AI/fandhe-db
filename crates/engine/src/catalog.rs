//! スキーマカタログ層（TASK-85、対象ビヘイビア: TABLE-1, TABLE-4, TABLE-5, TABLE-6。
//! ポインタ: `docs/spec/05-tasks.md` TASK-85・`docs/spec/04-behavior/data-model.md`）。
//!
//! 責務境界: `VECTOR(N)` 列型を含むテーブル定義（[`TableSchema`]）の DDL
//! （`CREATE TABLE`・`ALTER TABLE ADD COLUMN`・`DROP TABLE` 相当の
//! [`Storage::drop_table`]）と、その永続化（`storage.rs` の `redb::Database` を
//! 共有する専用テーブル）を担う。行データそのもの（`ROWS_TABLE`）には一切
//! アクセスしない設計上の境界とする（TABLE-4/TABLE-5）。[`Storage::drop_table`] は
//! 例外的にテーブルスコープ行ストア（`user_rows/{table}`）をカタログエントリと
//! 同一トランザクションで削除するが、これは DDL のライフサイクル管理としての
//! 削除であり、行の中身（値）を読み書きするものではない。
//! 行エンコーダーの列対応・NULL 解決（TASK-86）・
//! アリーナデコード（TASK-87）・テナント境界統合（TASK-89）は本モジュールの
//! 責務外で、後続タスクが本モジュールの API に依存する。SQL 表層からの `CREATE
//! TABLE` 受理は SQL-23・TASK-202（Issue #899）で `sql::ddl`（DDL 実行権限
//! ゲート・`SqlOutcome` への写像）から配線済み（本モジュールは実行本体
//! [`Storage::create_table`] を提供するのみで、権限判定・SQL 構文の許可リスト
//! 判定は担わない）。`DROP TABLE`（[`Storage::drop_table`]）は Issue #902 で
//! `EngineCore::parse_tokens` → `sql::ddl::execute_drop_table` →
//! [`Storage::drop_table`] として配線済み（`docs/design/drop-table.md` 参照）。
//! `ALTER TABLE` は引き続き未配線のまま。
//!
//! `storage.rs` との関係: `Storage::db()`（`pub(crate)`）を経由して同一
//! `redb::Database` ハンドルを共有し、カタログ専用のテーブル（[`CATALOG_TABLE`]）に
//! 書き込む。`ROWS_TABLE` の行エンコーディング（v2, RLS フィールド同居）とは
//! 独立したフォーマットを持つ。
//!
//! テーブルスコープ行 API（TASK-146、対象ビヘイビア: EXT-1, EXT-2。ポインタ:
//! `docs/spec/05-tasks.md` TASK-146・`docs/spec/04-behavior/extensions.md`）:
//! テーブルごとに動的な redb テーブル（`user_rows/{table_name}`）へ行を分離し、
//! 挿入時に本モジュールの [`TableSchema::validate_embedding_dim`] で宣言次元との
//! 完全一致を検証する。次元検証はここで完結し、RLS ポリシー評価（可視性判定）は
//! 従来どおり呼び出し元（TASK-133 以降）の責務のまま変えない。
//!
//! `sql::allowlist` との関係（TASK-74、対象ビヘイビア: SQL-8）: `impl TableLookup for
//! Storage`（本ファイル下部）が SQL 表層の FROM テーブル存在確認を橋渡しする。

use std::fmt;
use std::sync::Arc;

use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

// `row_codec::{self, Value as RowCodecValue}` は `insert_typed_row`
// （`#[cfg(test)]` 限定。Issue #1078）とユニットテストのみが使う。
#[cfg(test)]
use crate::row_codec::{self, Value as RowCodecValue};
use crate::sql::allowlist::{parse_view_body, SqlSurfaceError, TableLookup};
// `RowInput` / `Visibility` は `insert_row_into_table` / `insert_rows_into_table` /
// `insert_typed_row`（いずれも `#[cfg(test)]` 限定。Issue #1078）とユニットテストのみが
// 使う。
use crate::storage::{Row as StorageRow, Storage, StorageError};
#[cfg(test)]
use crate::storage::{RowInput, Visibility};

/// カタログ値を格納するテーブル。キーはテーブル名、値は [`encode_schema`] で
/// エンコードしたバイト列。`ROWS_TABLE`（`storage.rs`）とは別テーブルとし、
/// カタログの読み書き（TABLE-4/TABLE-5）が行データに触れないようにする。
const CATALOG_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("catalog");

/// ENUM 型定義（TABLE-14・TASK-198、Issue #890）を格納するテーブル。キーは
/// 型名（`validate_identifier` で検証済み）、値は [`encode_enum_type_def`] で
/// エンコードした語彙 blob。`CATALOG_TABLE`（列定義）とは独立したライフサイクルを
/// 持つ名前空間で、列は型名の参照（[`ColumnType::Enum`]）のみを保持する。
const ENUM_TYPES_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("enum_types");

/// 非マテリアライズド `VIEW` 定義（TABLE-18・SQL-23・TASK-205、Issue #909）を
/// 格納するテーブル。キーはビュー名（テーブルと名前空間を共有する。
/// [`Storage::create_view`]／[`Storage::create_table`] のいずれも [`CATALOG_TABLE`]・
/// 本テーブルの双方を同一 write txn で確認する）。値は [`encode_view_def`] で
/// エンコードした「直接参照するリレーション名＋正規化 body SQL」の blob。
/// 参照時の展開（`sql::view::resolve_from`）は本テーブルから取得した定義を
/// 許可リストパーサー（`sql::allowlist::parse_view_body`）で再検証してから使う
/// （第 2 の SQL パーサー・実行器を作らない設計。spec-confidentiality に配慮し
/// 本コメントには TABLE-18・SQL-23 のポインタのみを記す）。
pub(crate) const VIEWS_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("views");

/// 登録可能なビューの総数上限（[`MAX_LIST_TABLES`] と同じ既定値。本リポの
/// 実装既定値であり DoS 対策）。
const MAX_VIEWS: usize = MAX_LIST_TABLES;

/// ビュー本文（正規化 SQL テキスト）1 件あたりのバイト数上限（本リポの実装
/// 既定値）。untrusted 入力から無制限 `String`／redb 値を確保しないための
/// アロケーション前の上限（security.md「無制限リソース確保（DoS）」対応）。
pub(crate) const MAX_VIEW_BODY_BYTES: usize = 64 * 1024;

/// ビューのネスト深さ上限（本リポの実装既定値。テーブル自身を深さ 0 とし、
/// それを直接参照するビューを深さ 1、以降 1 段ごとに +1 する）。
pub(crate) const MAX_VIEW_NESTING_DEPTH: u32 = 4;

/// カタログ破損時の連鎖走査を打ち切る安全上限（[`MAX_VIEWS`] と同じ値。
/// 正常経路では循環を構造的に構築できない（[`Storage::create_view`] が
/// 参照先の存在を作成前に要求するため）が、破損したカタログ値に対しても
/// 無限ループにならないことを保証する防御的上限）。
const MAX_VIEW_CHAIN_WALK: usize = MAX_VIEWS;

/// 索引宣言（`CREATE INDEX`／`DROP INDEX`。TASK-206・INDEX-7、Issue #908）を格納する
/// テーブル。キーは索引名（テーブル・ビューと同じ relation 名前空間を共有する。
/// PostgreSQL と同じ設計。[`Storage::create_table`]／[`Storage::create_view`]／
/// [`Storage::create_index`] はいずれも [`CATALOG_TABLE`]・[`VIEWS_TABLE`]・本テーブルの
/// 3 者を同一 write txn で確認する）、値は [`encode_index_def`] でエンコードした
/// バイト列。対象テーブルが [`Storage::drop_table`] で削除された場合は同一 write
/// トランザクションで該当エントリも削除する（[`delete_indexes_for_table_in_txn`]）。
///
/// 責務境界: 本テーブルは宣言（索引の存在・種別・対象列）のみを保持し、索引の
/// 物理表現（`sql::scalar_index::ScalarIndexCache`・`sql::hnsw_cache::HnswIndexCache`
/// 等のテーブル世代整合キャッシュ。いずれも `(table, PolicyContext)` 可視
/// スナップショットから構築する）は一切構築しない（宣言の書き込みは行数に依存
/// しない）。宣言が既存キャッシュの構築対象へ与える効果は本 Issue の対象外
/// （`docs/design/index-ddl-declaration.md`「対象外」節参照）。
const INDEX_CATALOG_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("index_catalog");

/// [`INDEX_CATALOG_TABLE`] に宣言できる索引の総数上限（[`MAX_LIST_TABLES`] と
/// 同じ既定値。無制限 `Vec`／走査コストを避ける。本リポの実装既定値）。
const MAX_INDEX_COUNT: usize = MAX_LIST_TABLES;

/// 索引宣言 1 件が持てる列数の上限（[`MAX_COLUMN_COUNT`] と同値。本リポの実装
/// 既定値）。
const MAX_INDEX_DEF_COLUMNS: usize = MAX_COLUMN_COUNT;

/// 索引種別（TASK-206・INDEX-7、Issue #908）。`Scalar` はスカラー列（1 列以上）への
/// 宣言、`Hnsw` は単一の `VECTOR` 列に対する ANN 索引の宣言を表す。疎索引（BM25）は
/// 宣言の対象外（hybrid 実行のたびに自動構築する既存契約のまま）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexKind {
    Scalar,
    Hnsw,
}

impl IndexKind {
    fn as_str(self) -> &'static str {
        match self {
            IndexKind::Scalar => "scalar",
            IndexKind::Hnsw => "hnsw",
        }
    }
}

/// 索引宣言 1 件（TASK-206・INDEX-7、Issue #908）。[`Storage::create_index`] の入力・
/// [`decode_index_def`] の出力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef {
    pub name: String,
    pub table: String,
    pub kind: IndexKind,
    pub columns: Vec<String>,
}

impl IndexDef {
    pub fn new(name: String, table: String, kind: IndexKind, columns: Vec<String>) -> Self {
        Self {
            name,
            table,
            kind,
            columns,
        }
    }
}

/// [`IndexDef`] のテキスト形式エンコード（`v1\n<table>\n<kind>\n<col1>,<col2>,...`）。
/// 列名・テーブル名は [`validate_identifier`] を通過済みの前提（`,`・改行のいずれも
/// 含み得ない）のため、区切り文字として安全に使える。
fn encode_index_def(def: &IndexDef) -> Result<Vec<u8>> {
    let columns_csv = def.columns.join(",");
    let text = format!("v1\n{}\n{}\n{}", def.table, def.kind.as_str(), columns_csv);
    if text.len() > MAX_CATALOG_VALUE_LEN {
        return Err(CatalogError::Invalid(
            "index definition exceeds catalog value size limit".to_string(),
        ));
    }
    Ok(text.into_bytes())
}

/// [`encode_index_def`] の逆写像。永続化済みバイト列の破損は fail-closed に
/// [`CatalogError::CorruptSchema`] として拒否する（`decode_schema` と同じ方針）。
fn decode_index_def(name: &str, bytes: &[u8]) -> Result<IndexDef> {
    if bytes.len() > MAX_CATALOG_VALUE_LEN {
        return Err(CatalogError::CorruptSchema(format!(
            "index catalog value too large for index {name}"
        )));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| {
        CatalogError::CorruptSchema(format!("index catalog value is not utf-8 for index {name}"))
    })?;
    let mut lines = text.split('\n');
    let version = lines.next().ok_or_else(|| {
        CatalogError::CorruptSchema(format!("missing format version for index {name}"))
    })?;
    if version != "v1" {
        return Err(CatalogError::CorruptSchema(format!(
            "unknown index catalog format version for index {name}"
        )));
    }
    let table = lines
        .next()
        .ok_or_else(|| CatalogError::CorruptSchema(format!("missing table for index {name}")))?
        .to_string();
    validate_identifier(&table).map_err(|_| {
        CatalogError::CorruptSchema(format!("invalid table identifier for index {name}"))
    })?;
    let kind_tag = lines
        .next()
        .ok_or_else(|| CatalogError::CorruptSchema(format!("missing kind for index {name}")))?;
    let kind = match kind_tag {
        "scalar" => IndexKind::Scalar,
        "hnsw" => IndexKind::Hnsw,
        _ => {
            return Err(CatalogError::CorruptSchema(format!(
                "unknown index kind for index {name}"
            )))
        }
    };
    let columns_csv = lines
        .next()
        .ok_or_else(|| CatalogError::CorruptSchema(format!("missing columns for index {name}")))?;
    if lines.next().is_some() {
        return Err(CatalogError::CorruptSchema(format!(
            "unexpected trailing data for index {name}"
        )));
    }
    let mut columns: Vec<String> = Vec::new();
    for c in columns_csv.split(',') {
        if columns.len() >= MAX_INDEX_DEF_COLUMNS {
            return Err(CatalogError::CorruptSchema(format!(
                "index column count exceeds limit for index {name}"
            )));
        }
        validate_identifier(c).map_err(|_| {
            CatalogError::CorruptSchema(format!("invalid column identifier for index {name}"))
        })?;
        columns.push(c.to_string());
    }
    // [`Storage::create_index`] は列の重複を永続化しないため、重複を含む値は
    // 破損として fail-closed に拒否する。
    if first_duplicate_column(&columns).is_some() {
        return Err(CatalogError::CorruptSchema(format!(
            "duplicate column in index {name}"
        )));
    }
    Ok(IndexDef {
        name: name.to_string(),
        table,
        kind,
        columns,
    })
}

/// 列名リストの中で最初に重複した列名を返す（[`Storage::create_index`] の入力検証と
/// [`decode_index_def`] の破損検出が共有する。要素数は呼び出し元が
/// [`MAX_INDEX_DEF_COLUMNS`] 以下に制限済み）。
fn first_duplicate_column(columns: &[String]) -> Option<&str> {
    let mut seen = std::collections::HashSet::new();
    columns
        .iter()
        .find(|c| !seen.insert(c.as_str()))
        .map(|c| c.as_str())
}

/// [`IndexDef::kind`]・[`IndexDef::columns`] とテーブル定義（[`TableSchema`]）との
/// 整合を検証する（TASK-206・INDEX-7、Issue #908）。`id`（暗黙の行キー。
/// `schema.columns` には現れない）はスカラー宣言の対象列として常に有効とする。
///
/// - `Hnsw`: ちょうど 1 列を対象とし、その列は `VECTOR` 型でなければならない。
/// - `Scalar`: 各列は索引化対応済みの型（[`is_declarable_scalar_index_type`]）で
///   なければならない。
fn validate_index_columns(def: &IndexDef, schema: &TableSchema) -> Result<()> {
    match def.kind {
        IndexKind::Hnsw => {
            let col = match def.columns.as_slice() {
                [only] => only,
                _ => {
                    return Err(CatalogError::IndexKindMismatch(
                        "USING hnsw requires exactly one column".to_string(),
                    ))
                }
            };
            if col == "id" {
                // `id` は暗黙の行キーで常に「存在する列」だが `VECTOR` 型ではないため、
                // 列不在（`ColumnNotFound`）ではなく種別・列型の不整合へ倒す。
                return Err(CatalogError::IndexKindMismatch(
                    "USING hnsw requires a VECTOR column, column id is not VECTOR".to_string(),
                ));
            }
            let column = schema
                .columns
                .iter()
                .find(|c| &c.name == col)
                .ok_or_else(|| CatalogError::ColumnNotFound(col.clone()))?;
            if !column.ty.is_vector() {
                return Err(CatalogError::IndexKindMismatch(format!(
                    "USING hnsw requires a VECTOR column, column {col} is not VECTOR"
                )));
            }
        }
        IndexKind::Scalar => {
            for col in &def.columns {
                if col == "id" {
                    continue;
                }
                let column = schema
                    .columns
                    .iter()
                    .find(|c| &c.name == col)
                    .ok_or_else(|| CatalogError::ColumnNotFound(col.clone()))?;
                if !is_declarable_scalar_index_type(&column.ty) {
                    return Err(CatalogError::IndexKindMismatch(format!(
                        "column {col} cannot be declared as a scalar index target"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// スカラー索引宣言が対象にできる列型（`sql::scalar_index::ScalarIndex` が現に扱う
/// 型に限る。`VECTOR`・`BOOLEAN`／`BYTEA`／`JSON(B)`／`ARRAY`・未結線の数値型
/// 〔`INTEGER`／`BIGINT`／`REAL`／`DOUBLE`〕は、宣言しても索引化に一切効かない
/// 状態を作らないため fail-closed に拒否する）。
fn is_declarable_scalar_index_type(ty: &ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Text
            | ColumnType::Enum(_)
            | ColumnType::Date
            | ColumnType::Timestamp
            | ColumnType::Numeric { .. }
            | ColumnType::Uuid
    )
}

/// [`declared_index_targets_in_txn`] の出力（Issue #1065・TASK-206・INDEX-7）。
/// 呼び出し元（`sql::scalar_index::resolve_scalar_index_target_in_txn`）は
/// ここから `ScalarIndex::build` の構築対象列を導出する。宣言は起動時 opt-in
/// （`SearchEngineKind::Hnsw`）が有効な場合にのみ構築対象へ効く
/// （`docs/design/index-declaration-effects.md`）。HNSW 宣言の判定は
/// [`hnsw_targeted_in_txn`]（[`IndexCatalogGateCache`] 経由でカタログ全体の
/// 要約を世代単位に再利用する）が担うため、ここには HNSW 宣言の有無を持たない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeclaredIndexTargets {
    /// 対象テーブルのスカラー宣言列（複数宣言の和集合・重複排除）。宣言が
    /// 1 件も無ければ `None`（呼び出し元は `Auto` 〔現行の全列自動〕へ倒す）。
    pub(crate) scalar_columns: Option<Vec<String>>,
}

/// 対象テーブルの索引宣言を、クエリが使っているのと同一の `read_txn`
/// （アリーナ・世代と同一スナップショット）から 1 パスで要約する
/// （Issue #1065）。`Storage::list_indexes` は独自に read txn を開くため
/// クエリ経路からは呼ばない。カタログ未作成（宣言 0 件）は空の `Ok`。
/// 走査件数が [`MAX_INDEX_COUNT`] を超える・値のデコードに失敗する場合は
/// `Err` とし、呼び出し元は「宣言なし→自動」へは倒さず索引を使わない側
/// （スカラー: 構築失敗→plain scan）へ fail-closed に倒す。
pub(crate) fn declared_index_targets_in_txn(
    read_txn: &redb::ReadTransaction,
    table: &str,
) -> Result<DeclaredIndexTargets> {
    let index_table = match read_txn.open_table(INDEX_CATALOG_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Ok(DeclaredIndexTargets {
                scalar_columns: None,
            })
        }
        Err(e) => return Err(e.into()),
    };
    let mut scalar_columns: Vec<String> = Vec::new();
    for (scanned, entry) in index_table.iter()?.enumerate() {
        if scanned >= MAX_INDEX_COUNT {
            return Err(CatalogError::CorruptSchema(format!(
                "index catalog exceeds {MAX_INDEX_COUNT} entries"
            )));
        }
        let (key, value) = entry?;
        let def = decode_index_def(key.value(), value.value())?;
        if matches!(def.kind, IndexKind::Scalar) && def.table == table {
            for col in def.columns {
                if !scalar_columns.iter().any(|c| c == &col) {
                    scalar_columns.push(col);
                }
            }
        }
    }
    Ok(DeclaredIndexTargets {
        scalar_columns: if scalar_columns.is_empty() {
            None
        } else {
            Some(scalar_columns)
        },
    })
}

/// [`hnsw_targeted_in_txn`] が索引カタログ全件走査から要約する内容（Issue #1065・
/// PR #1124）。`IndexKind::Hnsw` 宣言を持つテーブル名の集合だけを保持する
/// （[`IndexCatalogGateCache`] が 1 世代あたり 1 回の走査で全テーブルの問い合わせを
/// 賄えるようにするため）。`HnswScope::All` では集合の中身を使わず、走査が成功した
/// こと（カタログが読み取り可能であること）だけを使う。
#[derive(Debug, Default)]
struct HnswCatalogSummary {
    /// `IndexKind::Hnsw` 宣言を持つテーブル名の集合。
    hnsw_tables: std::collections::HashSet<String>,
}

/// [`HnswCatalogSummary`] を索引カタログ全件走査で構築する（[`hnsw_targeted_in_txn`]
/// の判定本体）。カタログ未作成（宣言 0 件）は空の `Ok`。走査件数が
/// [`MAX_INDEX_COUNT`] を超える・デコードに失敗する場合は `Err`（呼び出し元が
/// brute-force へ fail-closed に倒す）。
fn hnsw_catalog_summary_in_txn(read_txn: &redb::ReadTransaction) -> Result<HnswCatalogSummary> {
    let index_table = match read_txn.open_table(INDEX_CATALOG_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(HnswCatalogSummary::default()),
        Err(e) => return Err(e.into()),
    };
    let mut summary = HnswCatalogSummary::default();
    for (scanned, entry) in index_table.iter()?.enumerate() {
        if scanned >= MAX_INDEX_COUNT {
            return Err(CatalogError::CorruptSchema(format!(
                "index catalog exceeds {MAX_INDEX_COUNT} entries"
            )));
        }
        let (key, value) = entry?;
        let def = decode_index_def(key.value(), value.value())?;
        if matches!(def.kind, IndexKind::Hnsw) {
            summary.hnsw_tables.insert(def.table);
        }
    }
    Ok(summary)
}

/// [`hnsw_targeted_in_txn`] の索引カタログ全件走査結果（[`HnswCatalogSummary`]）を
/// ストレージ全体の単一世代カウンタ（`crate::storage::current_generation_in_txn`）
/// 単位で再利用するキャッシュ（codex-review P2 対応・Issue #1065 PR #1124）。
/// 宣言が [`MAX_INDEX_COUNT`]（最大 10,000 件）に達する構成では、キャッシュ
/// 無しだと検索・`EXPLAIN` のたびに宣言数に比例するデコードが検索ホットパスへ
/// 乗る。
///
/// キー選定: 索引カタログを変更する経路（`Storage::create_index`・`drop_index`・
/// `drop_table`・`alter_table_drop_column`）はいずれも
/// `crate::recovery::commit_boundary::commit` を経由し、commit 前に必ず
/// `crate::storage::prepare_generation_bump`（ストレージ全体の単一世代
/// カウンタ）を通る。索引カタログはテーブル横断の単一 redb テーブルで、要約
/// （HNSW 宣言テーブル集合・読み取り可否）はどのテーブルへの索引 DDL の commit
/// でも変わり得るため、対象テーブルの世代（`bump_table_generation_in_txn`）では
/// なく全 commit で進むストレージ全体世代をキーにする（取りこぼさない）。
/// 通常の行 DML でも過剰に無効化されるが、キャッシュ不一致時のコストは
/// キャッシュ導入前と同じフルスキャン 1 回に留まる（悪化しない）。
///
/// `EngineCore` が唯一のインスタンスを保持し、`core.rs`（Rust API 検索・
/// `EXPLAIN`）・`sql::exec`（SQL 検索、`sql::hnsw_cache::HnswCacheAccess`
/// 経由）の全呼び出し元が共有する。
pub(crate) struct IndexCatalogGateCache {
    state: std::sync::Mutex<Option<(u64, std::sync::Arc<HnswCatalogSummary>)>>,
    /// [`hnsw_targeted_in_txn`] が索引カタログ全件走査（世代取得・走査上限超過・
    /// デコード失敗のいずれか）に失敗し `false`（brute-force へ fail-closed 縮退）
    /// を返した回数（codex-review Low 指摘対応・Issue #1065）。挙動自体は
    /// 常に安全側（厳密結果）だが、カタログ破損等の異常が運用上観測できるよう
    /// `scalar_index::ScalarIndexCache::build_failures` と同方針で計上する。
    gate_read_failures: std::sync::atomic::AtomicU64,
}

impl IndexCatalogGateCache {
    pub(crate) fn new() -> Self {
        Self {
            state: std::sync::Mutex::new(None),
            gate_read_failures: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 現在の観測用統計を返す（`EngineCore::index_catalog_gate_cache_stats` の
    /// 唯一の呼び出し元）。テナント ID・行データ等の機微情報は含まない。
    pub(crate) fn stats(&self) -> IndexCatalogGateCacheStats {
        IndexCatalogGateCacheStats {
            gate_read_failures: self
                .gate_read_failures
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

/// [`IndexCatalogGateCache`] の現在の統計（codex-review Low 指摘対応・
/// Issue #1065）。テスト・運用観測用であり、`VectorCore` trait には載せない
/// 固有 API として `EngineCore::index_catalog_gate_cache_stats` からのみ公開する
/// （`ScalarIndexCacheStats`・`HnswIndexCacheStats` と同方針）。
#[derive(Debug, Clone, Copy, Default)]
pub struct IndexCatalogGateCacheStats {
    /// [`hnsw_targeted_in_txn`] がカタログ読み取り失敗により brute-force へ
    /// fail-closed 縮退した回数。
    pub gate_read_failures: u64,
}

/// [`IndexCatalogGateCache`] を経由して [`HnswCatalogSummary`] を取得する。
/// 世代の読み取りと本体の走査を同一の `read_txn`（クエリが使っているのと
/// 同一スナップショット）から行う（キーと内容が別スナップショット由来になる
/// TOCTOU を避ける）。`Err`（世代取得・カタログ走査いずれかの失敗）はキャッシュ
/// へ書き込まず呼び出し元へそのまま返す（fail-closed の縮退を汚さない）。
/// ロック毒化はキャッシュミス相当として扱う（`unwrap` しない）。新しい世代の
/// 結果だけを保存し、遅延したミスがより新しいエントリを上書きしないようにする。
fn cached_hnsw_catalog_summary_in_txn(
    read_txn: &redb::ReadTransaction,
    gate_cache: &IndexCatalogGateCache,
) -> Result<std::sync::Arc<HnswCatalogSummary>> {
    let generation =
        crate::storage::current_generation_in_txn(read_txn).map_err(convert_storage_error)?;
    if let Ok(guard) = gate_cache.state.lock() {
        if let Some((cached_generation, summary)) = guard.as_ref() {
            if *cached_generation == generation {
                return Ok(summary.clone());
            }
        }
    }
    let summary = std::sync::Arc::new(hnsw_catalog_summary_in_txn(read_txn)?);
    if let Ok(mut guard) = gate_cache.state.lock() {
        let should_store = match guard.as_ref() {
            Some((cached_generation, _)) => generation > *cached_generation,
            None => true,
        };
        if should_store {
            *guard = Some((generation, summary.clone()));
        }
    }
    Ok(summary)
}

/// HNSW opt-in（`SearchEngineKind::Hnsw`。呼び出し元が `hnsw_available` で
/// 渡す）がクエリ対象テーブル `table` で実際に有効かを判定する（Issue #1065・
/// TASK-206・INDEX-7・`docs/design/index-declaration-effects.md`「HNSW
/// （テーブル単位）」）。判定はテーブル単位で、`table` 以外のテーブルの
/// `USING hnsw` 宣言は結果に影響しない（他テーブルへの宣言で経路・Top-k が
/// 変わらない）:
///
/// - `hnsw_available == false`: 常に `false`（カタログに触れない）
/// - `scope == HnswScope::All`（既定）: 索引カタログを読み取れれば `true`
///   （宣言の有無によらず全テーブル HNSW。宣言は記録のみで経路を変えない）
/// - `scope == HnswScope::Declared`: `table` に `USING hnsw` 宣言があれば
///   `true`、無ければ `false`（厳密 brute-force）
/// - カタログ読み取り失敗（走査上限超過・デコード破損・世代取得失敗）:
///   いずれの `scope` でも `false`（fail-closed。宣言状態を確定できない場合は
///   索引を使わない側＝厳密 brute-force へ倒す。スカラー側の
///   `resolve_scalar_index_target_in_txn` と同方針）。失敗回数は
///   [`IndexCatalogGateCacheStats`] に計上する。
///
/// `sql::exec`（`AnnShapeInput.hnsw_enabled`）・`core.rs`（Rust API
/// `search_with_snapshot`・`EXPLAIN` の `ann_plan:` 行）が同一のこの関数を
/// 呼ぶことで、実行時判定と `EXPLAIN` 表示の乖離を作らない。走査結果は
/// `gate_cache`（[`IndexCatalogGateCache`]）を経由してストレージ世代単位で
/// 再利用する（codex-review P2 対応・PR #1124）。
pub(crate) fn hnsw_targeted_in_txn(
    read_txn: &redb::ReadTransaction,
    gate_cache: &IndexCatalogGateCache,
    table: &str,
    scope: crate::search_engine::HnswScope,
    hnsw_available: bool,
) -> bool {
    if !hnsw_available {
        return false;
    }
    match cached_hnsw_catalog_summary_in_txn(read_txn, gate_cache) {
        Ok(summary) => match scope {
            crate::search_engine::HnswScope::All => true,
            crate::search_engine::HnswScope::Declared => summary.hnsw_tables.contains(table),
        },
        Err(_) => {
            gate_cache
                .gate_read_failures
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            false
        }
    }
}

/// `name` が [`INDEX_CATALOG_TABLE`] に索引名として登録済みかを write txn 内で
/// 判定する（relation 名前空間の衝突・種別判定用）。索引カタログ自体が未作成
/// （宣言 0 件）の場合は `false`。
fn index_name_exists_in_txn(write_txn: &redb::WriteTransaction, name: &str) -> Result<bool> {
    match write_txn.open_table(INDEX_CATALOG_TABLE) {
        Ok(t) => Ok(t.get(name)?.is_some()),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// [`INDEX_CATALOG_TABLE`] の全エントリのうち `keep` が `false` を返す宣言を同一
/// write txn 内で削除する（[`delete_indexes_for_table_in_txn`]・
/// [`delete_indexes_referencing_column_in_txn`] の共通本体）。索引カタログ未作成は
/// 何もせず `Ok(())`。走査件数は [`MAX_INDEX_COUNT`] で打ち切る（fail-closed）。
fn retain_index_defs_in_txn(
    write_txn: &redb::WriteTransaction,
    mut keep: impl FnMut(&IndexDef) -> bool,
) -> Result<()> {
    let mut index_table = match write_txn.open_table(INDEX_CATALOG_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let mut to_remove = Vec::new();
    for (scanned, entry) in index_table.iter()?.enumerate() {
        if scanned >= MAX_INDEX_COUNT {
            return Err(CatalogError::CorruptSchema(format!(
                "index catalog exceeds {MAX_INDEX_COUNT} entries"
            )));
        }
        let (key, value) = entry?;
        let name = key.value().to_string();
        let def = decode_index_def(&name, value.value())?;
        if !keep(&def) {
            to_remove.push(name);
        }
    }
    for name in to_remove {
        index_table.remove(name.as_str())?;
    }
    Ok(())
}

/// [`Storage::drop_table`] が同一 write txn から呼ぶ。対象テーブルの索引宣言を
/// すべて削除する（残置した宣言名が drop 後の同名テーブル再作成に伴う
/// `CREATE INDEX` を無関係に `42P07` で塞ぐ事故を防ぐ。
/// `recovery::ledger::delete_table_in_txn` と同じ「テーブルのライフサイクルに
/// 追随して掃除する」設計判断）。
fn delete_indexes_for_table_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
) -> Result<()> {
    retain_index_defs_in_txn(write_txn, |def| def.table != table_name)
}

/// [`Storage::alter_table_drop_column`] が同一 write txn から呼ぶ。削除する列を
/// 対象に含む索引宣言を削除する（PostgreSQL の `DROP COLUMN` が当該列を含む索引を
/// 削除するのと同じ扱い。宣言が消えた列名を指したまま残り、後の同名列の再追加で
/// 意図せず復活する事故を防ぐ）。
fn delete_indexes_referencing_column_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
    column_name: &str,
) -> Result<()> {
    retain_index_defs_in_txn(write_txn, |def| {
        def.table != table_name || !def.columns.iter().any(|c| c == column_name)
    })
}

/// カタログのテキスト形式フォーマットバージョン識別子。値の追加・変更は
/// 破壊的変更として扱い、この値を更新する。旧バージョンの読み出しは
/// マイグレーションを提供せず fail-closed に拒否する
/// （`storage.rs` の `ROW_FORMAT_VERSION` と同じ方針）。
///
/// `v2`（Issue #880）: 列の 4 フィールド構文（`name:tag:param:nullable`）自体は
/// `v1` と同一のまま、`param` フィールドの意味を「パラメータなし型は `-`、
/// それ以外は型ごとの文法」へ汎用化した。`TEXT`/`VECTOR` の列行バイト列は
/// `v1` と完全に同一で、変わるのは 1 行目のバージョン識別子のみ。`v1` の
/// カタログ値はマイグレーションを提供せず fail-closed に拒否する
/// （`docs/design/column-type-extension.md` 参照）。
const CATALOG_FORMAT_VERSION_LINE: &str = "v2";

/// カタログ v3（TABLE-19・TASK-203、Issue #901）: `ALTER TABLE ... DROP COLUMN`
/// により削除された列（墓標。[`DroppedSlot`]）を 1 つ以上持つスキーマ専用の
/// フォーマット。墓標を持たないスキーマは従来どおり v2 で書き（バイト列不変。
/// 既存のゴールデンテストに影響しない）、墓標が 1 つでもあるスキーマだけが
/// このバージョンで書かれる（[`encode_schema`]）。列 1 行あたり
/// `name:tag:param:nullable:state`（`state` は `L`=生存／`D`=削除済み）の
/// 5 フィールドで、物理スロット順（[`TableSchema::physical_slots`]）に並ぶ。
const CATALOG_FORMAT_VERSION_V3: &str = "v3";

/// カタログ v4（TABLE-16・TASK-204、Issue #903）: `PRIMARY KEY` を宣言した
/// スキーマ専用のフォーマット。`cols:` 行の直後に `pk:<col>[,<col>]*` 行を
/// 1 行追加する点のみが v3 と異なり、列行の形（5 フィールド・`state`）は
/// v3 と共有する（[`TableSchema::dropped_slots`] が空でも v4 で書ける）。
/// 主キーを宣言しないスキーマは（墓標の有無に関わらず）従来どおり v2／v3 で
/// 書き、v4 は「主キーを持つが DEFAULT は持たない」スキーマにのみ使う
/// （既存のゴールデンテストに影響しない）。
const CATALOG_FORMAT_VERSION_V4: &str = "v4";

/// カタログ v5（TABLE-16・TASK-204、Issue #904）: `DEFAULT` 句を 1 つ以上持つ
/// スキーマ専用のフォーマット。`DEFAULT` を持たないスキーマは引き続き
/// v2／v3／v4（墓標・主キーの有無で選択）のいずれかで書き、バイト列を
/// 変えない（既存ゴールデンテストへ影響しない）。v5 は v4 の上位集合で、
/// `cols:` 行の直後に `pk:` 行を必ず 1 行持つ（主キー宣言が無ければ
/// `pk:` の後を空のまま書き、`decode_schema_body` は空を「主キーなし」と
/// 解釈する。v4 の `pk:` 行は非空必須のまま変えない）。列行は墓標の有無に
/// かかわらず必ず `name:tag:param:nullable:state:default` の 6 フィールド
/// （`state` は v3／v4 と同じ `L`／`D`。墓標行の `default` は常に `-`）で
/// 書く。`default` フィールドの符号化は [`ColumnDefault::
/// encode_catalog_field`] 参照。
const CATALOG_FORMAT_VERSION_V5: &str = "v5";

/// カタログ v6（TABLE-16・TASK-204、Issue #905）: UNIQUE 制約
/// （[`UniqueConstraint`]）を 1 つ以上持つスキーマ専用のフォーマット。v5 の
/// 上位集合で、`cols:` 行の直後に `pk:` 行（主キー宣言が無ければ空）を必ず
/// 1 行持ち、列行は墓標・`DEFAULT` の有無に関わらず 6 フィールド
/// （`name:tag:param:nullable:state:default`）で書く。列行の直後に
/// `uniq:<n>` 行（`n >= 1`）と `n` 個の `U:<col>[,<col>]*` 行を追記する。
/// UNIQUE 制約を 1 つでも持つスキーマは（主キー・`DEFAULT`・墓標の有無に
/// 関わらず）必ず v6 で書き、持たないスキーマは従来どおり v2〜v5 のまま
/// バイト列を変えない（既存ゴールデンテストへ影響しない。v2〜v6 は互いに
/// 排他な正規形）。
const CATALOG_FORMAT_VERSION_V6: &str = "v6";

/// カタログ v7（TABLE-16・TASK-204、Issue #906）: `CHECK` 制約
/// （[`CheckConstraint`]）を 1 つ以上持つスキーマ専用のフォーマット。v6 の
/// 上位集合で、`cols:` 行の直後に `pk:` 行（主キー宣言が無ければ空）を必ず
/// 1 行持ち、列行は 6 フィールド（`name:tag:param:nullable:state:default`）で
/// 書く。列行の直後に `uniq:<n>` 行（v7 に限り `n == 0` を許容する）と `n` 個の
/// `U:` 行、続けて `checks:<m>` 行（`m >= 1`）と `m` 個の
/// `check:<name>:<col1,col2,...>:<hex(predicate_sql)>` 行を追記する。`CHECK` 制約を
/// 1 つでも持つスキーマは（主キー・`DEFAULT`・UNIQUE・墓標の有無に関わらず）
/// 必ず v7 で書き、持たないスキーマは従来どおり v2〜v6 のままバイト列を変えない
/// （v2〜v7 は互いに排他な正規形）。
const CATALOG_FORMAT_VERSION_V7: &str = "v7";

/// カタログ v8（TABLE-17・TASK-205、Issue #907）: `FOREIGN KEY` 制約
/// （[`ForeignKeyDef`]）を 1 つ以上持つスキーマ専用のフォーマット。v7 の
/// 上位集合で、`cols:` 行の直後に `pk:` 行（主キー宣言が無ければ空）を必ず
/// 1 行持ち、列行は 6 フィールドで書く。列行の直後に `uniq:<n>` 行（0 件可）と
/// `n` 個の `U:` 行、`checks:<m>` 行（v8 に限り `m == 0` を許容する）と `m` 個の
/// `check:` 行、続けて `fks:<k>` 行（`k >= 1`）と `k` 個の
/// `fk:<col1,col2,...>:<parent_table>:<pcol1,pcol2,...>` 行を追記する。
/// `FOREIGN KEY` を 1 つでも持つスキーマは（他の宣言の有無に関わらず）必ず v8 で
/// 書き、持たないスキーマは従来どおり v2〜v7 のままバイト列を変えない
/// （v2〜v8 は互いに排他な正規形）。
const CATALOG_FORMAT_VERSION_V8: &str = "v8";

/// カタログ v9（TABLE-17・TASK-205、Issue #1076／#1077）: `FOREIGN KEY` 制約の
/// うち 1 件以上が既定以外の参照アクション（`NO ACTION` 以外）・`MATCH`
/// （`Full`）・遅延属性（`NotDeferrable` 以外）を持つスキーマ専用のフォーマット。
/// v8 の上位集合で、本体（`pk:` 行・6 フィールド列行・`uniq:`／`checks:`
/// セクション）は同一のまま、`fks:` セクションの `fk:` 行を
/// `<col1,...>:<parent_table>:<pcol1,...>:<on_delete>:<on_update>:<match>:<deferral>`
/// の 7 フィールドに拡張する（`on_delete`／`on_update` は `noaction`／`cascade`／
/// `setnull`／`setdefault`、`match` は `simple`／`full`、`deferral` は `immediate`
/// 〔`NotDeferrable`〕／`deferrable`〔`DeferrableInitiallyImmediate`〕／`deferred`
/// 〔`DeferrableInitiallyDeferred`〕）。正規形の一意性を保つため、全 `FOREIGN KEY`
/// が既定オプションのスキーマは v9 では書かず引き続き v8 のバイト列のまま
/// 変えない（v2〜v9 は互いに排他な正規形。旧バイナリは v9 を未知の版として
/// 拒否する。前方互換は持たない）。
///
/// **互換性契約（Issue #1147・codex-review／Cursor Bugbot 指摘）**: この `v9` の
/// 意味は #1077 でリリース済みであり、main 上に既に `v9` バイト列を持つカタログが
/// 存在しうる。UNIQUE 制約名の永続化（Issue #1067）は `v9` を再定義せず、
/// 未使用の [`CATALOG_FORMAT_VERSION_V10`]／[`CATALOG_FORMAT_VERSION_V11`] を
/// 新設して読み書きする。
const CATALOG_FORMAT_VERSION_V9: &str = "v9";

/// カタログ v10（TABLE-16・TASK-204、Issue #1067）: 制約名（[`UniqueConstraint::name`]）
/// を永続化するフォーマット。v8 の上位集合で、`cols:` 行の直後に `pk:` 行
/// （主キー宣言が無ければ空）を必ず 1 行持ち、列行は 6 フィールドで書く。
/// `uniq:<n>` 行（v10 に限り `n >= 1` 必須）の後ろに `n` 個の
/// `U:<name>:<col1,col2,...>` 行（`U:` 行の 2 番目のフィールドに制約名を持つ点が
/// v6〜v8 と異なる唯一の差分）、続けて `checks:<m>` 行（0 件可）と `m` 個の
/// `check:` 行、`fks:<k>` 行（**v10 に限り `k == 0` を許容する**。v8 は
/// `k >= 1` 必須）と `k` 個の 3 フィールド `fk:` 行（`MATCH`・遅延属性を持たない
/// 既定オプション固定）を追記する。`FOREIGN KEY` が既定以外の `MATCH`・遅延属性を
/// 1 件でも持つ場合は [`CATALOG_FORMAT_VERSION_V11`] で書く（v9 の意味は #1077 の
/// ままとし、UNIQUE の実名を理由に v9 を上書きしない）。
///
/// v10 で書くのはスキーマが少なくとも 1 つの UNIQUE 制約を持ち、かつその実名が
/// 「全 UNIQUE を名前未指定とみなして [`derive_unique_constraint_names`] で
/// 導出した既定名」と 1 つでも一致せず（[`encode_schema`]）、かつ `FOREIGN KEY`
/// が既定以外の `MATCH`・遅延属性を 1 件も持たない場合だけとする。名前を指定せずに
/// 宣言された UNIQUE 制約は既定名と一致するため、`CREATE TABLE`・名前省略の
/// `ALTER TABLE ADD UNIQUE` はバイト列を変えず v6〜v8 のまま書かれる（既存
/// ゴールデンテストに影響しない）。名前の無い旧 v6〜v8 値は decode 時に既定名を
/// 導出する（[`assign_unique_constraint_names`]）。v10 を知らない旧バイナリは
/// 「未知のフォーマットバージョン」として fail-closed に拒否する。
const CATALOG_FORMAT_VERSION_V10: &str = "v10";

/// カタログ v11（TABLE-16/17・TASK-204/205、Issue #1067・#1077 の統合）: v10 の
/// 上位集合で、UNIQUE の実名（Issue #1067）と `FOREIGN KEY` の既定以外の
/// `MATCH`（`Full`）・遅延属性（`NotDeferrable` 以外。Issue #1077）の両方を
/// 1 つのスキーマが同時に持つ場合に使う。本体（`pk:` 行・6 フィールド列行・
/// 名前付き `uniq:` セクション）は v10 と同一のまま、`fks:` の `fk:` 行のみ
/// `<col1,...>:<parent_table>:<pcol1,...>:<on_delete>:<on_update>:<match>:<deferral>`
/// の 7 フィールドへ拡張する（`encode_foreign_key_section` が実際に書く形。
/// フィールドの意味は v9 の新形式と同じ）。5 フィールド
/// （`<col1,...>:<parent_table>:<pcol1,...>:<match>:<deferral>`。参照アクション
/// フィールドを持たない）は Issue #1077 時点で書かれた**旧** v9 バイト列を
/// 読むための読み取り専用の後方互換形であり（`parse_foreign_key_section`
/// 参照）、v11 の書き込み形式ではない（codex-review P2 指摘・PR #1147）。
/// `fks:<k>` は **v11 に限り `k >= 1`
/// 必須**（既定オプションのみの FK しか無ければ UNIQUE の実名だけを理由に
/// v10 のバイト列のまま書く。正規形の一意性）。`uniq:<n>` は v10 と同じく
/// `n >= 1` 必須（この分岐に入るのは UNIQUE の実名を持つ場合のみのため）。
/// v11 を知らない旧バイナリは「未知のフォーマットバージョン」として fail-closed に
/// 拒否する（前方互換は持たない）。
const CATALOG_FORMAT_VERSION_V11: &str = "v11";

/// カタログ v12（TABLE-22・TASK-233、Issue #1069。設計 F4）: v11 の上位集合。
/// `FOREIGN KEY` の実名が設計 F2 の既定名導出と 1 件でも食い違う場合のみこの
/// 版で書く（UNIQUE の実名の有無・FK オプションの有無は問わない。v12 は
/// `uniq:`／`checks:` セクションを 0 件で許し、`fks:` セクションのみ `n >= 1`
/// 必須〔この分岐に入るのは名前付き FK を持つ場合のみのため〕。`fk:` 行は
/// `fk:<name>:<cols>:<parent>:<pcols>:<on_delete>:<on_update>:<match>:<deferral>`
/// の 8 フィールド固定——v8〜v11 の後方互換 5/7 フィールド判別を持たない
/// 書き込み専用の新形式（[`encode_foreign_key_section_v12`]・
/// [`parse_foreign_key_section_v12`]）。v12 を知らない旧バイナリは「未知の
/// フォーマットバージョン」として fail-closed に拒否する（前方互換は持たない）。
const CATALOG_FORMAT_VERSION_V12: &str = "v12";

/// `FOREIGN KEY` を持ちうるカタログフォーマット版の一覧（TABLE-17・TASK-205・
/// TABLE-22・TASK-233、Issue #907・#1069）。[`referencing_foreign_keys_in_txn`]
/// が候補行の絞り込みに使う唯一の情報源。**P0**: 新しい FK 保持版を追加する際は
/// 必ずここへ追記すること——見落とすと参照先側の書き込み検査
/// （`constraint::enforce_referencing_rows_in_txn`）と `DROP TABLE` の依存検査
/// （`2BP01`）の双方が fail-open になる（advisor 指摘・codex-review 指摘）。
const FK_BEARING_FORMAT_VERSIONS: [&str; 5] = [
    CATALOG_FORMAT_VERSION_V8,
    CATALOG_FORMAT_VERSION_V9,
    CATALOG_FORMAT_VERSION_V10,
    CATALOG_FORMAT_VERSION_V11,
    CATALOG_FORMAT_VERSION_V12,
];

/// 1 テーブルが持てる `CHECK` 制約数の上限（TABLE-16・TASK-204、Issue #906。
/// 実装既定値）。デコード時、この値を超える宣言件数はアロケーション前に拒否する
/// （.claude/rules/coding-rust.md「untrusted 入力の扱い」）。
pub(crate) const MAX_CHECK_CONSTRAINTS_PER_TABLE: usize = 32;

/// `CHECK` 制約 1 件あたりの正規化済み述語テキストのバイト長上限（実装既定値）。
/// 個々の制約の評価コストを書き込み経路で有界にするための上限（式ノード数・
/// 深さは `sql::udf_call::MAX_EXPR_NODES`／`MAX_EXPR_DEPTH` を別途適用する）。
pub(crate) const MAX_CHECK_PREDICATE_SQL_LEN: usize = 4096;

/// `CHECK` 制約 1 件が参照する列名の件数上限（実装既定値）。
pub(crate) const MAX_CHECK_REFERENCED_COLUMNS: usize = 32;

/// 1 テーブルが宣言できる UNIQUE 制約数の上限（TABLE-16・TASK-204、
/// Issue #905）。本リポの実装既定値。デコード時、この値を超える宣言数は
/// アロケーション前に拒否する（.claude/rules/coding-rust.md「untrusted 入力の
/// 扱い」）。
pub(crate) const MAX_UNIQUE_CONSTRAINTS: usize = 32;

/// UNIQUE 制約 1 個が参照できる列数の上限（TABLE-16・TASK-204、Issue #905）。
/// [`MAX_PRIMARY_KEY_COLUMNS`] と同じ PostgreSQL の索引キー列数慣習（32）に
/// 合わせた実装既定値。
pub(crate) const MAX_UNIQUE_CONSTRAINT_COLUMNS: usize = 32;

/// 1 テーブルが宣言できる `FOREIGN KEY` 制約数の上限（TABLE-17・TASK-205、
/// Issue #907）。[`MAX_UNIQUE_CONSTRAINTS`] と同じ本リポの実装既定値。デコード時、
/// この値を超える宣言数はアロケーション前に拒否する（.claude/rules/coding-rust.md
/// 「untrusted 入力の扱い」）。
pub(crate) const MAX_FOREIGN_KEYS_PER_TABLE: usize = 32;

/// `FOREIGN KEY` 制約 1 個が参照できる列数の上限（TABLE-17・TASK-205、
/// Issue #907）。参照先は主キー・UNIQUE 制約と列集合が一致する必要があるため、
/// [`MAX_UNIQUE_CONSTRAINT_COLUMNS`] と同値とする。
pub(crate) const MAX_FOREIGN_KEY_COLUMNS: usize = MAX_UNIQUE_CONSTRAINT_COLUMNS;

/// `FOREIGN KEY` の参照先列として `id` 疑似列（物理キー `(tenant_id, id)` の
/// `id` 側。テナント内で常に一意な暗黙主キー。TABLE-12）を表す列名。
/// [`ForeignKeyDef::parent_columns`] がこの 1 要素だけを持つ場合に限り、参照先は
/// 行の物理キーそのものを意味する（`id` は予約列名のため通常列と衝突しない）。
pub(crate) const FOREIGN_KEY_PARENT_ID_COLUMN: &str = "id";
/// カタログ v2 の `param` フィールドに許容する文字集合（TABLE-6・Issue #880）。
/// パラメータなし型を表す `-` は本集合の外だが、[`validate_catalog_param`] で
/// 別途特別扱いする。`:`・改行を含まないため、encode 側の `:` 区切りと
/// 衝突しない（区切り文字注入の防止）。
fn is_valid_catalog_param_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == ','
}

/// `param` フィールドの文字集合検証（TABLE-6）。空文字列・許容外文字（`:`・改行・
/// 非 ASCII 等）を fail-closed に拒否する。`-`（パラメータなし型）はここで許容する。
fn validate_catalog_param(param: &str) -> Result<()> {
    if param.is_empty() {
        return Err(CatalogError::Invalid(
            "catalog param field must not be empty".to_string(),
        ));
    }
    if param == "-" {
        return Ok(());
    }
    if !param.chars().all(is_valid_catalog_param_char) {
        return Err(CatalogError::Invalid(format!(
            "catalog param field contains invalid character: {param:?}"
        )));
    }
    Ok(())
}

/// 識別子（テーブル名・列名）のバイト長上限。PostgreSQL の識別子長慣習に整合させた
/// 実装ローカルな値（対象ビヘイビア: TABLE-6）。
const MAX_IDENTIFIER_LEN: usize = 63;

/// `VECTOR(N)` の次元数上限。`storage.rs::MAX_EMBEDDING_DIM` と同値を維持する
/// （カタログで宣言可能な次元が永続化層で扱える上限を超えないようにするため）。
/// 下限は 1 とする（TABLE-1）。
const MAX_VECTOR_DIM: u32 = 65_536;

// 上記コメントの「同値を維持する」という前提を、単なる複製定数のコメントに留めず
// コンパイル時に強制する。片方だけを変更するとここでビルドが失敗し、ドリフトを防ぐ。
const _: () = assert!(
    MAX_VECTOR_DIM == crate::storage::MAX_EMBEDDING_DIM,
    "catalog::MAX_VECTOR_DIM must stay in sync with storage::MAX_EMBEDDING_DIM"
);

/// 1 テーブルが持てる列数の上限。カタログ値のデコード時、この値を超える宣言列数は
/// アロケーション前に拒否する（.claude/rules/coding-rust.md「untrusted 入力の扱い」）。
const MAX_COLUMN_COUNT: usize = 256;

/// `PRIMARY KEY` に宣言できる列数の上限（TABLE-16・TASK-204、Issue #903）。
/// PostgreSQL の索引キー列数慣習（32）に合わせた本リポの実装既定値。デコード時、
/// この値を超える宣言列数は `Vec` を確保する前に拒否する
/// （.claude/rules/coding-rust.md「untrusted 入力の扱い」）。
pub(crate) const MAX_PRIMARY_KEY_COLUMNS: usize = 32;

/// カタログ値（エンコード済みバイト列）のバイト長上限。デコード前に検証し、
/// 無制限な文字列アロケーションを防ぐ。
const MAX_CATALOG_VALUE_LEN: usize = 1024 * 1024;

/// [`Storage::list_tables`] が返せるテーブル数の上限（無制限 `Vec` 確保を避ける。
/// security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
const MAX_LIST_TABLES: usize = 10_000;

/// ENUM 型（TABLE-14・TASK-198）1 個が持てるラベル数の上限。デコード・DDL
/// いずれもアロケーション前にこの上限で拒否する。
pub const MAX_ENUM_LABELS: usize = 256;

/// ENUM ラベル 1 個の UTF-8 バイト長上限。
pub const MAX_ENUM_LABEL_LEN: usize = 63;

/// 登録可能な ENUM 型の総数上限（[`MAX_LIST_TABLES`] と同じ既定値）。
const MAX_ENUM_TYPES: usize = 10_000;

/// ENUM 型定義 blob（[`encode_enum_type_def`]）のバイト長上限。
const MAX_ENUM_TYPE_VALUE_LEN: usize = 64 * 1024;

/// 組み込み型名との衝突防止（Issue #890 D1）。将来 SQL-23 で `CREATE TABLE`
/// の型名解決を実装した際に、ENUM 型名が組み込み型と曖昧になることを防ぐ。
/// 大文字小文字を区別しない。
const RESERVED_TYPE_NAMES: &[&str] = &[
    "text",
    "vector",
    "boolean",
    "bool",
    "bytea",
    "integer",
    "int",
    "bigint",
    "real",
    "double",
    "numeric",
    // `DECIMAL` は `NUMERIC` の別名（TABLE-13〔検討中〕・TASK-197、Issue #885。
    // カタログの型タグは `numeric` の 1 つに固定するが、SQL-23 の型名解決での
    // 曖昧さを避けるため別名も予約する）。
    "decimal",
    "date",
    "timestamp",
    "uuid",
    "json",
    "jsonb",
    // Cursor Bugbot 指摘（PR #1015）: 将来 SQL-23 の `CREATE TABLE` 型名解決で
    // `ENUM`／`ARRAY` は列型構文のキーワードとして扱われる想定であり、
    // ユーザー定義 ENUM 型名との曖昧さを避けるため予約する。
    "enum",
    "array",
];

/// カタログ層の公開エラー型。`redb` 操作由来のエラーは `Backend` に一本化し、
/// それ以外はすべて fail-closed な明示的な拒否理由を持つ。
///
/// `StorageError`（`storage.rs`）とは独立した型として定義する。`StorageError` は
/// `redb::Error` への変換元を一括で受ける blanket `From` 実装を持つため、
/// coherence 制約によりそこへ `CatalogError` からの変換を個別追加できない
/// （`storage.rs` の設計メモ参照）。
#[derive(Debug)]
pub enum CatalogError {
    /// `redb` 側で発生したエラー（I/O・トランザクション競合等）。
    Backend(redb::Error),
    /// 識別子・型・次元数のフォーマットが不正（TABLE-6）。呼び出し側（ユーザー入力の
    /// 識別子・スキーマ定義）が渡した値そのものの検証失敗であり、`detail` は
    /// 呼び出し元が把握済みの情報のみを含む。
    Invalid(String),
    /// redb に格納済みのカタログ値（[`decode_schema`]）、または格納済み行の
    /// スカラーペイロード（`tenant::update_row_columns_unchecked` が
    /// `row_codec::decode_scalar_columns` を呼ぶ経路。Issue #865）のデコードに
    /// 失敗した。ユーザーが今回渡した入力の構文エラーではなく、ストレージ側の
    /// 破損・想定外の格納状態を示す。`detail` には格納済みバイト列由来の断片
    /// （`cols_line` 等）が含まれ得るため、`Invalid` と区別し、wire クライアントへは
    /// detail を渡さず汎用メッセージへ丸める（Issue #55 レビュー指摘。
    /// `.claude/rules/security.md`「不安全な設計」「エラー・ログ経由で他テナントの
    /// データ・存在情報を漏らさない」対応）。
    CorruptSchema(String),
    /// 指定したテーブルがカタログに存在しない。
    TableNotFound(String),
    /// `CREATE TABLE` で同名テーブルが既に存在する（上書きしない。TABLE-4 前提）。
    TableAlreadyExists(String),
    /// `ALTER TABLE ADD COLUMN` で追加しようとした列名が既存列と重複する。
    ColumnAlreadyExists(String),
    /// テーブルスコープ行 API（TASK-146）で、指定した行 ID がそのテーブル内に
    /// 存在しない。他テーブルの同一 ID は無関係（テーブル帰属した独立ストア）。
    RowNotFound(u64),
    /// 既存 DB の行テーブルが旧フォーマット（物理キーが `id` のみ）で、現行の
    /// `(tenant_id, id)` 複合キー（対象ビヘイビア: TABLE-12）と互換でない。
    /// 旧データを別テナントの行として読み出す fail-open を避けるため、
    /// マイグレーションは提供せず fail-closed に拒否する（`redb` の
    /// `TableError::TableTypeMismatch` を本 variant へ写像する）。エラー文言には
    /// テーブル名・テナント ID を含めない（存在情報を漏らさない。security.md P0）。
    IncompatibleRowKeyFormat,
    /// テーブル単位の世代カウンタ（[`bump_table_generation_in_txn`]）が `u64` の
    /// 上限に達した。現実的には到達しないが、`checked_add` の網羅性のため扱う
    /// （`storage.rs::StorageError::GenerationCounterOverflow` と同じ方針）。
    TableGenerationCounterOverflow,
    /// ENUM 型（TABLE-14・TASK-198）DDL が参照した型名がカタログに存在しない。
    TypeNotFound(String),
    /// ENUM 型の `CREATE TYPE` 相当 API で同名の型が既に存在する（上書きしない）。
    TypeAlreadyExists(String),
    /// `DROP TYPE` 相当 API で、依存列（当該型を使う `ColumnType::Enum` 列）が
    /// 1 つ以上残っているため削除を拒否する（SQL-23 結線時は `2BP01` へ写像する
    /// 想定だが、本 variant は Rust API 専用であり wire への送出経路を持たない
    /// ため `ErrorClass` には追加しない）。
    DependentObjectsStillExist(String),
    /// `DROP VIEW` の対象名がカタログ・ビュー双方のいずれにも存在しない
    /// （TABLE-18・SQL-23・TASK-205、Issue #909）。テーブルの `TableNotFound` とは
    /// 別 variant とし、SQL 表層側で同じ `42P01` へ写像しつつ「ビュー専用の
    /// 検索だった」ことを型で残す。
    ViewNotFound(String),
    /// 名前は存在するが、要求された操作が期待するオブジェクト種別
    /// （テーブル／ビュー）と一致しない（`DROP TABLE` にビュー名、`DROP VIEW` に
    /// テーブル名、ビューへの書き込み系文。TABLE-18・SQL-23・TASK-205、
    /// Issue #909。ERR-6: `42809`）。
    WrongObjectKind(String),
    /// `DROP TABLE`／`DROP VIEW` の対象を参照するビューが 1 つ以上残っている
    /// （TABLE-18・SQL-23・TASK-205、Issue #909。ERR-6: `2BP01`）。ENUM 型専用の
    /// [`CatalogError::DependentObjectsStillExist`] とは独立させ、依存元・
    /// 依存先の意味論が混ざらないようにする。
    DependentViewsExist(String),
    /// ビュー関連の DoS 対策上限超過（ビュー総数・本文バイト数・ネスト深さ。
    /// TABLE-18・SQL-23・TASK-205、Issue #909。ERR-6: `54000`）。`detail` は
    /// 固定文言のみ（テナント・行内容を含まない）。
    ViewLimitExceeded(String),
    /// 明示トランザクション（SQL-31・TASK-221）の単一ライタ占有により、書き込み
    /// トランザクションの取得がロック待ちの上限を超過した（`storage::StorageError`
    /// から写像。[`convert_storage_error`] 参照）。
    WriteLockTimeout,
    /// `ALTER TABLE ... DROP COLUMN`／`ALTER COLUMN ... TYPE` が参照した列名が
    /// 対象テーブルに存在しない（TABLE-19・TASK-203、Issue #901）。
    ColumnNotFound(String),
    /// `ALTER TABLE ... DROP COLUMN`／`ALTER COLUMN ... TYPE` の対象列が
    /// 予約列（`id`／`tenant_id`／`visibility`）または `VECTOR` 列であり、
    /// 削除・型変更を許可しない（TABLE-19 D1・D3、Issue #901）。
    ProtectedColumn(String),
    /// `ALTER COLUMN ... TYPE` に指定した型変更が受理する拡大変換の一覧
    /// （TABLE-19 D3）に含まれない（縮小変換・異種変換・同一型を含む）。
    IncompatibleTypeChange {
        column: String,
        from: String,
        to: String,
    },
    /// `ALTER TABLE ADD COLUMN`（TABLE-5）で列を追加すると `MAX_COLUMN_COUNT`
    /// を超える（Issue #900・SQL-23）。`count` は追加後の物理スロット数（生存列＋
    /// 削除済み列の墓標。TABLE-19 D1。上限超過後の値）。
    /// `sql::ddl::execute_alter_table_add_column` はこの分類のみ `54000`
    /// （`SqlSurfaceError::PayloadTooLarge`）へ写像し、他の `Invalid` 系
    /// （識別子・型不正）とは区別する。
    TooManyColumns { count: usize },
    /// [`Storage::alter_table_add_unique_constraint`]（Rust API。TABLE-16・
    /// TASK-204、Issue #905）が、既存行の中にテナント内で重複する値の組を
    /// 検出したため制約追加を拒否した。文言・variant 自体にテナント名・値を
    /// 含めない（security.md P0。`TenantWriteError::UniqueViolation` と同じ
    /// 秘匿方針）。
    UniqueConstraintViolation,
    /// `CREATE INDEX` で指定した索引名が、既存の索引・テーブル・ビューの名前と
    /// 衝突する（TASK-206・INDEX-7、Issue #908。索引名はテーブル・ビューと同じ
    /// relation 名前空間を共有する。PostgreSQL と同じ設計。ERR-6: `42P07`）。
    IndexAlreadyExists(String),
    /// `DROP INDEX` で指定した名前が索引・テーブル・ビューのいずれとしても存在
    /// しない（TASK-206・INDEX-7、Issue #908。ERR-6: `42704`）。名前がテーブル・
    /// ビューとして存在する場合は [`CatalogError::WrongObjectKind`]（`42809`）。
    IndexNotFound(String),
    /// 索引種別と対象列の型が整合しない（`USING hnsw` に `VECTOR` 以外の列、
    /// スカラー宣言に `VECTOR` 列または索引化非対応の型を指定した等。
    /// TASK-206・INDEX-7、Issue #908。SQL 表層では `0A000`）。
    IndexKindMismatch(String),
    /// 索引宣言の登録件数上限超過（TASK-206・INDEX-7、Issue #908。本リポの実装
    /// 既定値による DoS 対策。SQL 表層では `54000`）。`detail` は固定文言のみ。
    IndexLimitExceeded(String),
    /// `FOREIGN KEY` 宣言（TABLE-17・TASK-205、Issue #907）が参照先の主キー・
    /// UNIQUE 制約（または `id` 疑似列）と列集合が一致しない、あるいは参照元列と
    /// 参照先列の型が一致しない（ERR-6: `42830`）。`detail` はカタログ情報
    /// （列名・テーブル名）のみでテナントデータを含まない。
    InvalidForeignKey(String),
    /// `ALTER TABLE ... ADD [CONSTRAINT <name>] UNIQUE` で指定した制約名が、
    /// 同一テーブルの既存 UNIQUE 制約名・CHECK 制約名（テーブル単位で名前空間を
    /// 共有する。設計 D1）のいずれかと衝突する（Issue #1067。ERR-6: `42P07`。
    /// 索引名衝突〔`IndexAlreadyExists`〕と同じ SQLSTATE を流用する）。
    ConstraintAlreadyExists(String),
    /// `ALTER TABLE ... DROP CONSTRAINT <name>` の対象名が、UNIQUE・CHECK
    /// いずれの制約としても存在しない（Issue #1067。ERR-6: `42704`）。
    ConstraintNotFound(String),
    /// `ALTER TABLE ... DROP CONSTRAINT <name>` の対象名が CHECK 制約を指す
    /// （CHECK の DROP は本 Issue のスコープ外。Issue #1067。ERR-6: `0A000`。
    /// fail-closed に拒否し、暗黙の CHECK 削除を許さない）。
    ConstraintDropNotSupported(String),
    /// テーブルあたりの制約数上限（[`MAX_UNIQUE_CONSTRAINTS`] と同じ本リポの
    /// 実装既定値）を超える `ALTER TABLE ... ADD UNIQUE`（Issue #1067。
    /// ERR-6: `54000`）。
    ConstraintLimitExceeded(String),
    /// [`Storage::alter_table_add_foreign_key`]（TABLE-22・TASK-233、Issue #1069）
    /// が、既存行の中に新しい `FOREIGN KEY` を満たさない参照元行を検出したため
    /// 制約追加を拒否した。文言・variant 自体にテナント名・値・行を含めない
    /// （security.md P0。`TenantWriteError::ForeignKeyViolation` と同じ秘匿方針）。
    ForeignKeyViolation,
}

impl fmt::Display for CatalogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CatalogError::Backend(e) => write!(f, "catalog backend error: {e}"),
            CatalogError::Invalid(msg) => write!(f, "invalid catalog data: {msg}"),
            CatalogError::CorruptSchema(msg) => write!(f, "corrupt catalog schema: {msg}"),
            CatalogError::TableNotFound(name) => write!(f, "table not found: {name}"),
            CatalogError::TableAlreadyExists(name) => write!(f, "table already exists: {name}"),
            CatalogError::ColumnAlreadyExists(name) => {
                write!(f, "column already exists: {name}")
            }
            CatalogError::RowNotFound(id) => write!(f, "row not found: id={id}"),
            CatalogError::IncompatibleRowKeyFormat => {
                write!(f, "incompatible row store key format: rebuild required")
            }
            CatalogError::TableGenerationCounterOverflow => {
                write!(f, "table generation counter overflow")
            }
            CatalogError::UniqueConstraintViolation => {
                write!(f, "duplicate key value violates unique constraint")
            }
            CatalogError::TypeNotFound(name) => write!(f, "type not found: {name}"),
            CatalogError::TypeAlreadyExists(name) => write!(f, "type already exists: {name}"),
            CatalogError::DependentObjectsStillExist(name) => {
                write!(f, "dependent objects still exist for type: {name}")
            }
            CatalogError::ViewNotFound(name) => write!(f, "view not found: {name}"),
            CatalogError::WrongObjectKind(name) => {
                write!(f, "wrong object kind for: {name}")
            }
            CatalogError::DependentViewsExist(name) => {
                write!(f, "dependent views still exist for: {name}")
            }
            CatalogError::ViewLimitExceeded(detail) => {
                write!(f, "view limit exceeded: {detail}")
            }
            CatalogError::WriteLockTimeout => {
                write!(f, "write lock not available: timed out waiting for writer")
            }
            CatalogError::ColumnNotFound(name) => write!(f, "column not found: {name}"),
            CatalogError::ProtectedColumn(name) => {
                write!(
                    f,
                    "column is protected and cannot be dropped or altered: {name}"
                )
            }
            CatalogError::IncompatibleTypeChange { column, from, to } => write!(
                f,
                "incompatible type change for column {column:?}: {from} -> {to}"
            ),
            CatalogError::TooManyColumns { count } => {
                write!(f, "too many columns: {count}")
            }
            CatalogError::IndexAlreadyExists(name) => write!(f, "index already exists: {name}"),
            CatalogError::IndexNotFound(name) => write!(f, "index not found: {name}"),
            CatalogError::IndexKindMismatch(detail) => {
                write!(f, "index kind mismatch: {detail}")
            }
            CatalogError::IndexLimitExceeded(detail) => {
                write!(f, "index limit exceeded: {detail}")
            }
            CatalogError::InvalidForeignKey(detail) => {
                write!(f, "invalid foreign key declaration: {detail}")
            }
            CatalogError::ConstraintAlreadyExists(name) => {
                write!(f, "constraint already exists: {name}")
            }
            CatalogError::ConstraintNotFound(name) => {
                write!(f, "constraint not found: {name}")
            }
            CatalogError::ConstraintDropNotSupported(name) => {
                write!(f, "dropping this constraint is not supported: {name}")
            }
            CatalogError::ConstraintLimitExceeded(detail) => {
                write!(f, "constraint limit exceeded: {detail}")
            }
            CatalogError::ForeignKeyViolation => {
                write!(
                    f,
                    "insert or update on table violates foreign key constraint"
                )
            }
        }
    }
}

impl std::error::Error for CatalogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CatalogError::Backend(e) => Some(e),
            CatalogError::Invalid(_)
            | CatalogError::CorruptSchema(_)
            | CatalogError::TableNotFound(_)
            | CatalogError::TableAlreadyExists(_)
            | CatalogError::ColumnAlreadyExists(_)
            | CatalogError::RowNotFound(_)
            | CatalogError::IncompatibleRowKeyFormat
            | CatalogError::TableGenerationCounterOverflow
            | CatalogError::TypeNotFound(_)
            | CatalogError::TypeAlreadyExists(_)
            | CatalogError::DependentObjectsStillExist(_)
            | CatalogError::ViewNotFound(_)
            | CatalogError::WrongObjectKind(_)
            | CatalogError::DependentViewsExist(_)
            | CatalogError::ViewLimitExceeded(_)
            | CatalogError::ColumnNotFound(_)
            | CatalogError::ProtectedColumn(_)
            | CatalogError::IncompatibleTypeChange { .. }
            | CatalogError::UniqueConstraintViolation
            | CatalogError::WriteLockTimeout
            | CatalogError::TooManyColumns { .. }
            | CatalogError::IndexAlreadyExists(_)
            | CatalogError::IndexNotFound(_)
            | CatalogError::IndexKindMismatch(_)
            | CatalogError::IndexLimitExceeded(_)
            | CatalogError::InvalidForeignKey(_)
            | CatalogError::ConstraintAlreadyExists(_)
            | CatalogError::ConstraintNotFound(_)
            | CatalogError::ConstraintDropNotSupported(_)
            | CatalogError::ConstraintLimitExceeded(_)
            | CatalogError::ForeignKeyViolation => None,
        }
    }
}

// `storage.rs` の `StorageError` と同じ橋渡し方針。`redb` の各操作が返す複数のエラー型を
// 一括して `CatalogError::Backend` へ変換する。
impl<E> From<E> for CatalogError
where
    E: Into<redb::Error>,
{
    fn from(e: E) -> Self {
        CatalogError::Backend(e.into())
    }
}

pub type Result<T> = std::result::Result<T, CatalogError>;

/// 列のデータ型（閉じた集合）。デコード時に未知の型名を検出した場合は
/// 既知の型へ黙殺フォールバックせず `CatalogError::Invalid` で拒否する（TABLE-6）。
///
/// variant を追加する際は `#[non_exhaustive]`・ワイルドカード腕（`_ =>`）を
/// 導入しない（Issue #880 D1）。コンパイラに全ディスパッチ地点を列挙させることで、
/// 新型が既存分岐へ黙って流れる fail-open を構造的に防ぐ設計とする。
///
/// `Copy` は付けない（Issue #890）: [`ColumnType::Enum`] が型定義（[`EnumTypeDef`]）
/// を指す `Arc` を保持するため、`Vector(u32)` までは可能だった値コピーは表現できない。
/// 呼び出し元は `column.ty.clone()` または `&column.ty` を使う（`!=`／`matches!` は
/// 参照を暗黙に取るため無変更で動く）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnType {
    /// 可変長テキスト列。
    Text,
    /// 固定次元の埋め込み列（`VECTOR(N)`、TABLE-1）。0 と `MAX_VECTOR_DIM` 超過は
    /// encode・decode 両側で拒否する。
    Vector(u32),
    /// 符号付き 32 ビット整数列（`INTEGER`、TABLE-13・TASK-196。Issue #881）。
    Integer,
    /// 符号付き 64 ビット整数列（`BIGINT`、TABLE-13・TASK-196。Issue #881）。
    BigInt,
    /// 単精度浮動小数点列（`REAL`、TABLE-13・TASK-196）。
    Real,
    /// 倍精度浮動小数点列（`DOUBLE PRECISION`、TABLE-13・TASK-196）。
    Double,
    /// 真偽値列（TABLE-13・TASK-196、Issue #883）。NULL と false は行バイト列・
    /// 投影・述語評価のいずれでも区別する（[`crate::row_codec::Value::Bool`] 参照）。
    Boolean,
    /// 日付列（TABLE-13・TASK-197、Issue #884）。内部表現は 1970-01-01 起点の
    /// 日数（`i32`）。値の解析・整形は [`crate::datetime`] へ委譲する。
    Date,
    /// 日時列（TABLE-13・TASK-197、Issue #884）。タイムゾーンを持たない
    /// （naive）値で、内部表現は 1970-01-01 00:00:00 起点のマイクロ秒（`i64`）。
    /// 値の解析・整形は [`crate::datetime`] へ委譲する。
    Timestamp,
    /// 可変長の同型スカラー配列列（`<スカラー型>[]`、TABLE-14・TASK-198、Issue #888）。
    /// 要素型・要素数上限は [`ArrayType`] が保持する。検索経路（KNN・hybrid・ANN・
    /// 二次索引・`EXPLAIN`）からは一貫して非対象として除外する（`VECTOR` 列との
    /// 責務境界。`docs/design/array-column-type.md` 参照）。
    Array(ArrayType),
    /// 可変長バイナリ列（TABLE-13・TASK-197、Issue #886）。NULL と空バイト列は
    /// 行バイト列上も区別する（[`crate::row_codec::Value::Bytes`] 参照）。
    Bytea,
    /// JSON テキスト列（TABLE-14・TASK-198、Issue #889）。格納時に共有パーサー
    /// [`crate::json::parse_json`] で検証するが、入力テキスト（空白・キー順を含む）を
    /// そのまま保持する（[`crate::row_codec::Value::Json`] 参照）。JSONB との違いは
    /// 正規化の有無のみで、値表現は共有する。
    Json,
    /// JSONB 列（TABLE-14・TASK-198、Issue #889）。格納時に正規化再シリアライズ
    /// した文字列を保持する（キー順は辞書順・空白なし。詳細は
    /// `docs/design/column-type-extension.md`「#889 追記」節参照）。
    Jsonb,
    /// 名前付き ENUM 型を参照する列（TABLE-14・TASK-198、Issue #890）。
    /// カタログには型名のみを保持し（[`ColumnType::catalog_fields`]）、
    /// デコード時に [`ENUM_TYPES_TABLE`] から語彙を解決した [`EnumTypeDef`] を
    /// `Arc` で持ち回る。値は行バイト列上 TEXT と同じフレーム（[`crate::row_codec::
    /// Value::Enum`]）で格納し、語彙外の値は書き込み前に拒否する
    /// （fail-closed。`ALTER TYPE ... ADD VALUE` による末尾追記のみ許可）。
    Enum(Arc<EnumTypeDef>),
    /// 十進固定小数列 `NUMERIC(precision, scale)`（TABLE-13〔検討中〕・
    /// TASK-197、Issue #885）。値の内部表現・丸め規則は
    /// [`crate::numeric::Decimal`] 参照。`1 <= precision <= 38`・
    /// `0 <= scale <= precision` を encode・decode 両側で検証する。
    Numeric { precision: u8, scale: u8 },
    /// 128bit 識別子列 `UUID`（TABLE-13〔検討中〕・TASK-197、Issue #887）。
    /// 値の内部表現・テキスト規範形は [`crate::uuid::Uuid`] 参照。version／
    /// variant ビットは検証しない（nil・全 1 も有効値）。
    Uuid,
}

impl ColumnType {
    /// `VECTOR` 列かどうか（`matches!(ty, ColumnType::Vector(_))` の言い換え。
    /// Issue #880 D9）。
    pub fn is_vector(&self) -> bool {
        matches!(self, ColumnType::Vector(_))
    }

    /// `PRIMARY KEY`（Issue #903）の構成列・`FOREIGN KEY`（Issue #907）の参照元列
    /// として宣言できる型かどうか（TABLE-16・TASK-204。PK・FK が共有する許可
    /// リスト。UNIQUE 制約は [`Self::is_unique_constraint_allowed`] という別の
    /// 上位集合を持つ——理由は同メソッドの doc 参照）。行バイト列上の値表現が
    /// バイト単位で一意に決まる型のみを許可する fail-closed な許可リストであり、
    /// `constraint::enforce_unique_keys_in_txn` が構築する正準キーバイト列の
    /// 一意性が値の一意性と一致することの前提になる。`VECTOR`（検索対象・
    /// 等価比較の対象外）・`REAL`／`DOUBLE`／`NUMERIC`／`JSON`／`JSONB`／`ARRAY`
    /// は対象外とする（Issue #1073 で UNIQUE のみへ拡張。PK・FK 側は据え置き。
    /// `docs/design/foreign-key.md` D3「参照元は PK 許可型であること」が前提に
    /// しているため、この共有リストを広げると FK・PK の対象型が意図せず広がる）。
    /// `sql::allowlist::validate_create_table_tokens` が SQL 表層の構造検証段階
    /// でも同じ判定を行う（第 2 の許可リストを作らず、ここへ委譲する）。
    pub(crate) fn is_primary_key_allowed(&self) -> bool {
        matches!(
            self,
            ColumnType::Text
                | ColumnType::Integer
                | ColumnType::BigInt
                | ColumnType::Boolean
                | ColumnType::Date
                | ColumnType::Timestamp
                | ColumnType::Uuid
                | ColumnType::Bytea
                | ColumnType::Enum(_)
        )
    }

    /// UNIQUE 制約（Issue #905・#1073）の参照列として宣言できる型かどうか
    /// （TABLE-16・TASK-204）。[`Self::is_primary_key_allowed`] の上位集合
    /// （PK 許可型はすべて UNIQUE でも許可する）で、REAL・DOUBLE PRECISION・
    /// NUMERIC・JSON・JSONB・配列型を追加で許可する。PK・FK 側の許可リストは
    /// 意図的に据え置く（`is_primary_key_allowed` の doc 参照）。正準キーの
    /// 型ごとの正規化（`-0.0` 正規化・NUMERIC の末尾ゼロ除去・JSON の値として
    /// の等価正規化・配列要素の生ペイロード連結）は
    /// [`crate::constraint::push_canonical_component`] が担う
    /// （`docs/design/unique-constraint.md` D7 参照）。`VECTOR`（検索対象・
    /// 等価比較の対象外）は引き続き対象外。
    pub(crate) fn is_unique_constraint_allowed(&self) -> bool {
        self.is_primary_key_allowed()
            || matches!(
                self,
                ColumnType::Real
                    | ColumnType::Double
                    | ColumnType::Numeric { .. }
                    | ColumnType::Json
                    | ColumnType::Jsonb
                    | ColumnType::Array(_)
            )
    }

    /// [`crate::constraint::enforce_unique_keys_in_txn`] が正準キーバイト列を
    /// 組み立てる際に使う、一意キー許可型（[`Self::is_unique_constraint_allowed`]。
    /// PK・FK は狭い方の [`Self::is_primary_key_allowed`] のみを使う）ごとの
    /// 固定タグ（TABLE-16・TASK-204、Issue #903・#1073）。
    /// [`Self::is_unique_constraint_allowed`] が `true` を返す型にのみ呼び出す
    /// 契約（呼び出し元は非許可型ではこのメソッドを呼ばない）。
    ///
    /// Issue #1070（永続一意索引化）により、この値は `user_uniq/{table}`
    /// テーブルの正引きキーの一部として**永続化される**（`constraint::
    /// unique_index` 参照）。カタログの `catalog_fields` タグとは独立に採番
    /// してよい点は変わらないが、値そのものはディスク形式の一部になったため、
    /// 既存タグの意味を変更してはならない（変更すると索引の
    /// フォーマットバージョンを上げ再構築を強制する必要がある）。新しい
    /// 一意キー許可型を追加する際は、未使用の値を新規に採番すること。
    /// [`Self::unique_key_tag`] の `ColumnType::Array` に対応する値。
    /// [`crate::constraint::push_canonical_component`] は要素値（借用結果
    /// [`crate::row_codec::ArrayRef`]）だけを持ち、宣言時の `max_len` を含む
    /// 完全な `ArrayType` を構築できないため、単独の定数として公開する
    /// （`unique_key_tag` の `match` 側の値と同一であることを機械的に保証する）。
    pub(crate) const ARRAY_UNIQUE_KEY_TAG: u8 = 14;
    /// [`Self::unique_key_tag`] の `ColumnType::Numeric` に対応する値。
    /// [`crate::constraint::push_canonical_component`] は借用結果
    /// [`crate::numeric::Decimal`] だけを持ち、宣言時の `precision`／`scale`
    /// を含む `ColumnType::Numeric` を構築する必要がないため、[`Self::
    /// ARRAY_UNIQUE_KEY_TAG`] と同じ理由で単独の定数として公開する。
    pub(crate) const NUMERIC_UNIQUE_KEY_TAG: u8 = 12;

    pub(crate) fn unique_key_tag(&self) -> u8 {
        match self {
            ColumnType::Text => 1,
            ColumnType::Integer => 2,
            ColumnType::BigInt => 3,
            ColumnType::Boolean => 4,
            ColumnType::Date => 5,
            ColumnType::Timestamp => 6,
            ColumnType::Bytea => 7,
            ColumnType::Uuid => 8,
            ColumnType::Enum(_) => 9,
            ColumnType::Real => 10,
            ColumnType::Double => 11,
            ColumnType::Numeric { .. } => Self::NUMERIC_UNIQUE_KEY_TAG,
            ColumnType::Json | ColumnType::Jsonb => 13,
            ColumnType::Array(_) => Self::ARRAY_UNIQUE_KEY_TAG,
            ColumnType::Vector(_) => {
                // 呼び出し元が `is_unique_constraint_allowed` の契約を破っている
                // （非許可型からタグを取得しようとした）。永続化しない内部
                // スクラッチ用の値のため panic ではなく判別可能な番兵を返し、
                // 呼び出し元（`constraint.rs`）が別途 fail-closed に拒否する。
                0
            }
        }
    }

    /// カタログのテキスト形式（v2）における型タグと `param` フィールドを返す
    /// （Issue #880 D2）。[`encode_schema`] はこの 1 対だけを呼び、型を 1 つ
    /// 追加する際にカタログ側で触る箇所をここへ集約する。ENUM 列の `param` は
    /// 型名そのもの（語彙は含めない。語彙は [`ENUM_TYPES_TABLE`] 側の SSOT）。
    fn catalog_fields(&self) -> (&'static str, String) {
        match self {
            ColumnType::Text => ("text", "-".to_string()),
            ColumnType::Vector(dim) => ("vector", dim.to_string()),
            ColumnType::Integer => ("integer", "-".to_string()),
            ColumnType::BigInt => ("bigint", "-".to_string()),
            ColumnType::Real => ("real", "-".to_string()),
            ColumnType::Double => ("double", "-".to_string()),
            ColumnType::Boolean => ("boolean", "-".to_string()),
            ColumnType::Date => ("date", "-".to_string()),
            ColumnType::Timestamp => ("timestamp", "-".to_string()),
            ColumnType::Array(array_ty) => (
                "array",
                format!("{},{}", array_ty.elem().catalog_tag(), array_ty.max_len()),
            ),
            ColumnType::Bytea => ("bytea", "-".to_string()),
            ColumnType::Json => ("json", "-".to_string()),
            ColumnType::Jsonb => ("jsonb", "-".to_string()),
            ColumnType::Enum(def) => ("enum", def.name.clone()),
            ColumnType::Numeric { precision, scale } => ("numeric", format!("{precision},{scale}")),
            ColumnType::Uuid => ("uuid", "-".to_string()),
        }
    }

    /// [`ColumnType::catalog_fields`] の逆変換。未知の型タグ・`param` の型別文法
    /// 違反はすべて `CatalogError::Invalid` で拒否する（TABLE-6。呼び出し元の
    /// [`decode_schema_body`] が `CorruptSchema` へ読み替える）。`param` の文字集合
    /// 検証（[`validate_catalog_param`]）は呼び出し元が先に行う契約とする。
    ///
    /// `resolve_enum`: `enum` タグの型名解決コールバック。呼び出し元
    /// （[`decode_schema_with_resolver`]）が現在の write/read トランザクション
    /// から [`ENUM_TYPES_TABLE`] を引く実装（[`get_enum_type_in_read_txn`]／
    /// [`get_enum_type_in_write_txn`]）を渡す。未登録の型名は
    /// `CatalogError::TypeNotFound` を返す（`decode_schema_with_resolver` は
    /// `Invalid` 以外はそのまま透過するため `CorruptSchema` へは読み替わらない
    /// が、`table_lookup_error` で `Invalid`／`TypeNotFound` いずれも
    /// `SqlSurfaceError::Internal` へ同じく丸まる。ENUM 列を持たないテーブルの
    /// デコードでは一度も呼ばれない）。
    fn from_catalog_fields(
        tag: &str,
        param: &str,
        resolve_enum: &mut dyn FnMut(&str) -> Result<Arc<EnumTypeDef>>,
    ) -> Result<ColumnType> {
        match tag {
            "text" => {
                if param != "-" {
                    return Err(CatalogError::Invalid(format!(
                        "text column must not declare a parameter: {param:?}"
                    )));
                }
                Ok(ColumnType::Text)
            }
            "vector" => {
                let dim: u32 = param.parse().map_err(|_| {
                    CatalogError::Invalid(format!("malformed vector dimension: {param:?}"))
                })?;
                validate_vector_dim(dim)?;
                Ok(ColumnType::Vector(dim))
            }
            "integer" => {
                if param != "-" {
                    return Err(CatalogError::Invalid(format!(
                        "integer column must not declare a parameter: {param:?}"
                    )));
                }
                Ok(ColumnType::Integer)
            }
            "bigint" => {
                if param != "-" {
                    return Err(CatalogError::Invalid(format!(
                        "bigint column must not declare a parameter: {param:?}"
                    )));
                }
                Ok(ColumnType::BigInt)
            }
            "real" => {
                if param != "-" {
                    return Err(CatalogError::Invalid(format!(
                        "real column must not declare a parameter: {param:?}"
                    )));
                }
                Ok(ColumnType::Real)
            }
            "double" => {
                if param != "-" {
                    return Err(CatalogError::Invalid(format!(
                        "double column must not declare a parameter: {param:?}"
                    )));
                }
                Ok(ColumnType::Double)
            }
            "boolean" => {
                if param != "-" {
                    return Err(CatalogError::Invalid(format!(
                        "boolean column must not declare a parameter: {param:?}"
                    )));
                }
                Ok(ColumnType::Boolean)
            }
            "date" => {
                if param != "-" {
                    return Err(CatalogError::Invalid(format!(
                        "date column must not declare a parameter: {param:?}"
                    )));
                }
                Ok(ColumnType::Date)
            }
            "timestamp" => {
                if param != "-" {
                    return Err(CatalogError::Invalid(format!(
                        "timestamp column must not declare a parameter: {param:?}"
                    )));
                }
                Ok(ColumnType::Timestamp)
            }
            "array" => {
                // `<elem_tag>,<max_len>` のちょうど 2 要素（Issue #888 D-A2）。
                // カンマの数が違う場合は要素・上限のいずれかが欠落・過多であり
                // fail-closed に拒否する。
                let mut parts = param.splitn(3, ',');
                let elem_tag = parts
                    .next()
                    .ok_or_else(|| CatalogError::Invalid("array param is empty".to_string()))?;
                let max_len_field = parts.next().ok_or_else(|| {
                    CatalogError::Invalid(format!("array param missing max_len: {param:?}"))
                })?;
                if parts.next().is_some() {
                    return Err(CatalogError::Invalid(format!(
                        "array param has too many fields: {param:?}"
                    )));
                }
                let elem = ArrayElemType::from_catalog_tag(elem_tag)?;
                // 先頭ゼロ等の非正準表現を拒否する（`s != n.to_string()` 比較）。
                let max_len: u32 = max_len_field.parse().map_err(|_| {
                    CatalogError::Invalid(format!("malformed array max_len: {max_len_field:?}"))
                })?;
                if max_len.to_string() != max_len_field {
                    return Err(CatalogError::Invalid(format!(
                        "array max_len is not in canonical form: {max_len_field:?}"
                    )));
                }
                let array_ty = ArrayType::new(elem, max_len)?;
                Ok(ColumnType::Array(array_ty))
            }
            "bytea" => {
                if param != "-" {
                    return Err(CatalogError::Invalid(format!(
                        "bytea column must not declare a parameter: {param:?}"
                    )));
                }
                Ok(ColumnType::Bytea)
            }
            "json" => {
                if param != "-" {
                    return Err(CatalogError::Invalid(format!(
                        "json column must not declare a parameter: {param:?}"
                    )));
                }
                Ok(ColumnType::Json)
            }
            "jsonb" => {
                if param != "-" {
                    return Err(CatalogError::Invalid(format!(
                        "jsonb column must not declare a parameter: {param:?}"
                    )));
                }
                Ok(ColumnType::Jsonb)
            }
            "enum" => {
                validate_identifier(param)?;
                let def = resolve_enum(param)?;
                Ok(ColumnType::Enum(def))
            }
            "numeric" => {
                let (precision, scale) = parse_numeric_param(param)?;
                Ok(ColumnType::Numeric { precision, scale })
            }
            "uuid" => {
                if param != "-" {
                    return Err(CatalogError::Invalid(format!(
                        "uuid column must not declare a parameter: {param:?}"
                    )));
                }
                Ok(ColumnType::Uuid)
            }
            other => Err(CatalogError::Invalid(format!(
                "unknown column type: {other:?}"
            ))),
        }
    }
}

/// [`ENUM_TYPES_TABLE`] を read トランザクションから引く（[`decode_schema_with_resolver`]
/// のリゾルバ実装。[`get_table_schema_in_txn`] から使う）。
fn get_enum_type_in_read_txn(
    read_txn: &redb::ReadTransaction,
    name: &str,
) -> Result<Arc<EnumTypeDef>> {
    let table = match read_txn.open_table(ENUM_TYPES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Err(CatalogError::TypeNotFound(name.to_string()))
        }
        Err(e) => return Err(e.into()),
    };
    let guard = table
        .get(name)?
        .ok_or_else(|| CatalogError::TypeNotFound(name.to_string()))?;
    Ok(Arc::new(decode_enum_type_def(name, guard.value())?))
}

/// [`ENUM_TYPES_TABLE`] を write トランザクションから引く（同上。DDL 系 API・
/// [`require_table_schema_write`]・[`Storage::alter_table_add_column`] から使う）。
fn get_enum_type_in_write_txn(
    write_txn: &redb::WriteTransaction,
    name: &str,
) -> Result<Arc<EnumTypeDef>> {
    let table = match write_txn.open_table(ENUM_TYPES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Err(CatalogError::TypeNotFound(name.to_string()))
        }
        Err(e) => return Err(e.into()),
    };
    let guard = table
        .get(name)?
        .ok_or_else(|| CatalogError::TypeNotFound(name.to_string()))?;
    Ok(Arc::new(decode_enum_type_def(name, guard.value())?))
}

/// 常に未解決を返す [`ColumnType::from_catalog_fields`] 用リゾルバ。txn を
/// 持たない文脈（単体テスト・列挙ヘルパの一部）で ENUM 列を含まないと分かって
/// いる場合にのみ使う。ENUM 列に遭遇した場合は fail-closed に拒否する。
/// production 経路は [`get_table_schema_in_txn`]・[`require_table_schema_write`]
/// が txn 由来のリゾルバを個別に渡すため、本関数を経由しない（`#[cfg(test)]`
/// の [`decode_schema`] 経由でのみ使う）。
#[cfg(test)]
fn no_enum_resolver(name: &str) -> Result<Arc<EnumTypeDef>> {
    Err(CatalogError::Invalid(format!(
        "enum type resolution is not available in this context: {name:?}"
    )))
}

/// ENUM 型の語彙違反（TABLE-14・TASK-198、Issue #890）。`sql::allowlist::
/// SqlSurfaceError::InvalidTextRepresentation`（`22P02`）へ写像される。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnumLabelError {
    /// 語彙に存在しないラベル。
    NotInVocabulary,
}

impl fmt::Display for EnumLabelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EnumLabelError::NotInVocabulary => write!(f, "label is not a member of the enum type"),
        }
    }
}

/// 名前付き ENUM 型の定義（TABLE-14・TASK-198、Issue #890）。宣言順を保持した
/// ラベル列を持つ。フィールドは private とし、`name()`／`labels()`／`contains()`／
/// `validate_label()` の 4 メソッドのみを公開する（engine・wire-server の双方が
/// 語彙検証をこの実装 1 つに委譲する単一情報源とするため）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumTypeDef {
    name: String,
    labels: Vec<String>,
}

impl EnumTypeDef {
    /// 型名。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 宣言順のラベル列。
    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    /// `label` が語彙に含まれるか（バイト単位の完全一致）。
    pub fn contains(&self, label: &str) -> bool {
        self.labels.iter().any(|l| l == label)
    }

    /// `label` を語彙に対して検証する。engine・wire-server が共有する唯一の
    /// 検証実装（[`crate::sql::parser::bind_enum_literal`]・NoSQL 表層の
    /// insert/update/filter 経路がいずれもこれを呼ぶ）。
    pub fn validate_label(&self, label: &str) -> std::result::Result<(), EnumLabelError> {
        if self.contains(label) {
            Ok(())
        } else {
            Err(EnumLabelError::NotInVocabulary)
        }
    }
}

/// ラベル 1 個の制約検証（DDL・ADD VALUE 共通）。空文字・[`MAX_ENUM_LABEL_LEN`]
/// 超過・制御文字（NUL 含む）を拒否する。
fn validate_enum_label(label: &str) -> Result<()> {
    if label.is_empty() {
        return Err(CatalogError::Invalid(
            "enum label must not be empty".to_string(),
        ));
    }
    if label.len() > MAX_ENUM_LABEL_LEN {
        return Err(CatalogError::Invalid(format!(
            "enum label too long: {} bytes",
            label.len()
        )));
    }
    if label.chars().any(|c| c.is_control()) {
        return Err(CatalogError::Invalid(
            "enum label must not contain control characters".to_string(),
        ));
    }
    Ok(())
}

/// ラベル列全体の制約検証（個数上限・重複・各ラベルの形式）。
fn validate_enum_labels(labels: &[String]) -> Result<()> {
    if labels.is_empty() {
        return Err(CatalogError::Invalid(
            "enum type must declare at least one label".to_string(),
        ));
    }
    if labels.len() > MAX_ENUM_LABELS {
        return Err(CatalogError::Invalid(format!(
            "too many enum labels: {}",
            labels.len()
        )));
    }
    let mut seen: Vec<&str> = Vec::with_capacity(labels.len());
    for label in labels {
        validate_enum_label(label)?;
        if seen.contains(&label.as_str()) {
            return Err(CatalogError::Invalid(format!(
                "duplicate enum label: {label:?}"
            )));
        }
        seen.push(label.as_str());
    }
    Ok(())
}

/// ENUM 型名の検証。識別子形式（[`validate_identifier`]）に加え、組み込み型名
/// （[`RESERVED_TYPE_NAMES`]。大文字小文字を区別しない）との衝突を拒否する
/// （Issue #890 D1。将来 SQL-23 の `CREATE TABLE` 型名解決での曖昧さを防ぐ）。
fn validate_enum_type_name(name: &str) -> Result<()> {
    validate_identifier(name)?;
    let lower = name.to_ascii_lowercase();
    if RESERVED_TYPE_NAMES.contains(&lower.as_str()) {
        return Err(CatalogError::Invalid(format!(
            "enum type name collides with a built-in type name: {name:?}"
        )));
    }
    Ok(())
}

/// [`EnumTypeDef`] のバイト表現（TABLE-14）。`u8` バージョン・`u16` ラベル数
/// （LE）・各ラベルは `u8` 長さ＋UTF-8 本体。宣言数の上限（[`MAX_ENUM_LABELS`]）は
/// デコード側がアロケーション前に検証する（.claude/rules/coding-rust.md
/// 「untrusted 入力の扱い」。ただし本 blob は untrusted クライアント入力の
/// 直接デコード対象ではなく、格納済みカタログ値のデコードであり、破損は
/// `CorruptSchema` として扱う）。
fn encode_enum_type_def(name: &str, labels: &[String]) -> Result<Vec<u8>> {
    validate_enum_type_name(name)?;
    validate_enum_labels(labels)?;
    let mut out = Vec::new();
    out.push(1u8); // バージョン
    let count = u16::try_from(labels.len())
        .map_err(|_| CatalogError::Invalid("too many enum labels to encode".to_string()))?;
    out.extend_from_slice(&count.to_le_bytes());
    for label in labels {
        let len = u8::try_from(label.len())
            .map_err(|_| CatalogError::Invalid("enum label too long to encode".to_string()))?;
        out.push(len);
        out.extend_from_slice(label.as_bytes());
    }
    if out.len() > MAX_ENUM_TYPE_VALUE_LEN {
        return Err(CatalogError::Invalid(
            "encoded enum type value too large".to_string(),
        ));
    }
    Ok(out)
}

/// [`encode_enum_type_def`] の逆変換。バージョン不一致・宣言数超過・余剰バイト・
/// 不正 UTF-8 はすべて `CatalogError::CorruptSchema` として拒否する（格納済み
/// データの破損。ユーザー入力の構文エラーとは区別する。Issue #55 の既存方針を
/// 踏襲）。
fn decode_enum_type_def(name: &str, bytes: &[u8]) -> Result<EnumTypeDef> {
    decode_enum_type_def_body(name, bytes).map_err(|e| match e {
        CatalogError::Invalid(msg) => CatalogError::CorruptSchema(msg),
        other => other,
    })
}

fn decode_enum_type_def_body(name: &str, bytes: &[u8]) -> Result<EnumTypeDef> {
    if bytes.len() > MAX_ENUM_TYPE_VALUE_LEN {
        return Err(CatalogError::Invalid(
            "enum type value too large".to_string(),
        ));
    }
    let version = *bytes
        .first()
        .ok_or_else(|| CatalogError::Invalid("enum type value is empty".to_string()))?;
    if version != 1 {
        return Err(CatalogError::Invalid(format!(
            "unknown enum type format version: {version}"
        )));
    }
    let count_bytes: [u8; 2] = bytes
        .get(1..3)
        .ok_or_else(|| CatalogError::Invalid("enum type value truncated".to_string()))?
        .try_into()
        .map_err(|_| CatalogError::Invalid("enum type value truncated".to_string()))?;
    let count = u16::from_le_bytes(count_bytes) as usize;
    // 宣言数の上限をアロケーション（`Vec::with_capacity`）の前に検証する
    // （Issue #880 D7 と同じ方針）。
    if count == 0 || count > MAX_ENUM_LABELS {
        return Err(CatalogError::Invalid(format!(
            "enum type declares an invalid label count: {count}"
        )));
    }
    let mut labels = Vec::with_capacity(count);
    let mut offset = 3usize;
    for _ in 0..count {
        let len = *bytes.get(offset).ok_or_else(|| {
            CatalogError::Invalid("enum type value truncated at label length".to_string())
        })? as usize;
        offset = offset
            .checked_add(1)
            .ok_or_else(|| CatalogError::Invalid("enum type value offset overflow".to_string()))?;
        let end = offset
            .checked_add(len)
            .ok_or_else(|| CatalogError::Invalid("enum type value offset overflow".to_string()))?;
        let label_bytes = bytes.get(offset..end).ok_or_else(|| {
            CatalogError::Invalid("enum type value truncated at label body".to_string())
        })?;
        let label = std::str::from_utf8(label_bytes)
            .map_err(|_| CatalogError::Invalid("enum label is not valid UTF-8".to_string()))?
            .to_string();
        labels.push(label);
        offset = end;
    }
    if offset != bytes.len() {
        return Err(CatalogError::Invalid(
            "enum type value has trailing bytes".to_string(),
        ));
    }
    validate_enum_labels(&labels)?;
    Ok(EnumTypeDef {
        name: name.to_string(),
        labels,
    })
}

/// 永続化済み `VIEW` 定義（TABLE-18・SQL-23・TASK-205、Issue #909）。`sql::view`
/// モジュールが `body_sql` を許可リストパーサーで再検証・再展開する（本モジュールは
/// 「直接参照するリレーション名」と「正規化 body SQL」を運ぶだけで、SQL の意味論
/// には一切踏み込まない）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewDef {
    /// `CREATE VIEW ... AS SELECT ... FROM <base_relation> ...` の
    /// `<base_relation>`（テーブルまたは別のビューの名前。存在確認・
    /// ネスト深さの計算は本モジュールが担い、値そのものはカタログ照会前の
    /// 識別子として保持する）。
    pub base_relation: String,
    /// [`crate::sql::allowlist::validate_create_view_tokens`] が構築した、
    /// 再パース可能な正規化 SQL（`SELECT ... FROM ... [WHERE ...]`。`sql::view`
    /// の round-trip テストが「描画 → 再トークン化 → 再パース」で元の AST と
    /// 一致することを固定する）。
    pub body_sql: String,
}

/// [`ViewDef`] のバイト表現（`u8` バージョン・`base_relation`／`body_sql` それぞれ
/// `u32`（LE）長 + UTF-8 本体）。[`encode_enum_type_def`] と同じく untrusted
/// クライアント入力の直接デコード対象ではなく、格納済みカタログ値のデコード
/// （破損は `CorruptSchema`）。
fn encode_view_def(def: &ViewDef) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.push(1u8); // バージョン
    for part in [def.base_relation.as_str(), def.body_sql.as_str()] {
        let len = u32::try_from(part.len()).map_err(|_| {
            CatalogError::Invalid("view definition part too long to encode".to_string())
        })?;
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(part.as_bytes());
    }
    if out.len() > MAX_VIEW_BODY_BYTES.saturating_add(64) {
        return Err(CatalogError::ViewLimitExceeded(
            "view body too large".to_string(),
        ));
    }
    Ok(out)
}

/// [`encode_view_def`] の逆変換。バージョン不一致・長さ不整合・不正 UTF-8・
/// 余剰バイトはすべて `CatalogError::CorruptSchema` として拒否する（格納済み
/// データの破損。呼び出し元〔`sql::view::resolve_from`〕はこれを固定文言
/// `XX000` へ丸め、body のリテラル値をエラーへ含めない。security.md P0）。
fn decode_view_def(bytes: &[u8]) -> Result<ViewDef> {
    decode_view_def_body(bytes).map_err(|e| match e {
        CatalogError::Invalid(msg) => CatalogError::CorruptSchema(msg),
        other => other,
    })
}

fn decode_view_def_body(bytes: &[u8]) -> Result<ViewDef> {
    let version = *bytes
        .first()
        .ok_or_else(|| CatalogError::Invalid("view definition value is empty".to_string()))?;
    if version != 1 {
        return Err(CatalogError::Invalid(format!(
            "unknown view definition format version: {version}"
        )));
    }
    let mut offset = 1usize;
    let mut parts: Vec<String> = Vec::with_capacity(2);
    for _ in 0..2 {
        let len_bytes: [u8; 4] = bytes
            .get(offset..offset + 4)
            .ok_or_else(|| CatalogError::Invalid("view definition value truncated".to_string()))?
            .try_into()
            .map_err(|_| CatalogError::Invalid("view definition value truncated".to_string()))?;
        offset = offset
            .checked_add(4)
            .ok_or_else(|| CatalogError::Invalid("view definition offset overflow".to_string()))?;
        let len = u32::from_le_bytes(len_bytes) as usize;
        if len > MAX_VIEW_BODY_BYTES {
            return Err(CatalogError::Invalid(
                "view definition part too large".to_string(),
            ));
        }
        let end = offset
            .checked_add(len)
            .ok_or_else(|| CatalogError::Invalid("view definition offset overflow".to_string()))?;
        let part_bytes = bytes.get(offset..end).ok_or_else(|| {
            CatalogError::Invalid("view definition value truncated at part body".to_string())
        })?;
        let part = std::str::from_utf8(part_bytes)
            .map_err(|_| {
                CatalogError::Invalid("view definition part is not valid UTF-8".to_string())
            })?
            .to_string();
        parts.push(part);
        offset = end;
    }
    if offset != bytes.len() {
        return Err(CatalogError::Invalid(
            "view definition value has trailing bytes".to_string(),
        ));
    }
    // `parts` はループで必ず 2 要素を push 済みだが、`expect` による panic 経路を
    // 作らず（coding-rust.md: engine ライブラリコードで panic させない）
    // スライスパターンで直接分解する。要素数不一致は構造的に到達不能なため
    // `CorruptSchema` 経由の防御的フォールバックとして扱う。
    let [base_relation, body_sql] = <[String; 2]>::try_from(parts).map_err(|_| {
        CatalogError::Invalid("view definition value has unexpected part count".to_string())
    })?;
    Ok(ViewDef {
        base_relation,
        body_sql,
    })
}

/// [`Storage::create_view`] が保存前に行う `body_sql` の自己検証（codex-review
/// 指摘・PR #1048）。`sql::allowlist::parse_view_body`（`CREATE VIEW` 構文検証・
/// `sql::view::resolve_from` の格納値再検証と同一実装。第 2 のパーサーを
/// 作らない）で `body_sql` を再トークン化・再パースし、(1) 許可リスト形状
/// （`SELECT <* | 列名> FROM <relation> [WHERE <単純述語>]`。式項目・UDF 述語は
/// `42601` 相当として拒否）を満たすこと、(2) パース結果が示す `FROM` の参照先が
/// 呼び出し元の主張する `base_relation` と一致することを検証する。
///
/// `sql::allowlist::validate_create_view_tokens` を経由する正規の SQL 表層経路
/// では `body_sql` は常に `render_view_body(parsed)`（`parsed.table_name ==
/// base_relation`）として構築されるためこの検証は常に通るが、それ以外の
/// `pub fn create_view` 呼び出し元（本メソッドは engine の公開 Rust API）が
/// 独自に組み立てた `body_sql`／`base_relation` の組を渡した場合、両者が
/// 食い違う定義や許可リスト外の形状が永続化されてしまうと、参照時
/// （`sql::view::resolve_from`）の列スコープ検査
/// （`sql::view::check_columns_within_view`）が「`body_sql` の投影は
/// `base_relation` に対して検証済み」という前提の上に成り立たなくなる
/// （テナント境界そのものは崩さないが、ビューが宣言する列公開契約が破れる）。
fn validate_view_body_matches_base_relation(body_sql: &str, base_relation: &str) -> Result<()> {
    let tokens = crate::sql::lexer::tokenize(body_sql).map_err(|_| {
        CatalogError::Invalid("view body is not valid SQL for a view definition".to_string())
    })?;
    let parsed = parse_view_body(&tokens).map_err(|_| {
        CatalogError::Invalid(
            "view body does not match the allowed view definition shape".to_string(),
        )
    })?;
    if parsed.table_name != base_relation {
        return Err(CatalogError::Invalid(
            "view body FROM target does not match base_relation".to_string(),
        ));
    }
    Ok(())
}

/// `start` から始めてテーブルへ到達するまでの参照段数（テーブル自身が深さ 0、
/// それを直接参照するビューが深さ 1）を計算する（[`Storage::create_view`] が
/// 新規ビューのネスト深さ判定に使う。同一 write txn 内で完結させ TOCTOU を
/// 避ける）。`start` がテーブル・ビューのいずれにも存在しない場合は
/// `CatalogError::TableNotFound`。循環・異常に長い連鎖はカタログ破損として
/// `CorruptSchema`（正常経路では発生しない。[`Storage::create_view`] が新規
/// ビュー名の被参照を作成前に拒否するため自己参照は構造的に作れない）。
fn resolve_reference_depth_in_txn(
    catalog_table: &redb::Table<'_, &str, &[u8]>,
    views_table: &redb::Table<'_, &str, &[u8]>,
    start: &str,
) -> Result<u32> {
    let mut current = start.to_string();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut depth = 0u32;
    loop {
        if !visited.insert(current.clone()) {
            return Err(CatalogError::CorruptSchema(
                "view reference cycle detected".to_string(),
            ));
        }
        if visited.len() > MAX_VIEW_CHAIN_WALK {
            return Err(CatalogError::CorruptSchema(
                "view reference chain too long".to_string(),
            ));
        }
        if catalog_table.get(current.as_str())?.is_some() {
            return Ok(depth);
        }
        match views_table.get(current.as_str())? {
            Some(guard) => {
                let def = decode_view_def(guard.value())?;
                depth = depth.checked_add(1).ok_or_else(|| {
                    CatalogError::CorruptSchema("view nesting depth overflow".to_string())
                })?;
                current = def.base_relation;
            }
            None => return Err(CatalogError::TableNotFound(start.to_string())),
        }
    }
}

/// `target`（テーブルまたはビュー名）を直接参照している既存ビュー名の一覧
/// （[`Storage::drop_table`]／[`Storage::drop_view`] の依存検査が使う。
/// [`dependent_tables_in_txn`] と同じ「上限超過は無制限 `Vec` 確保を避けて
/// `Err`」方針）。
fn views_depending_on_in_txn(
    views_table: &redb::Table<'_, &str, &[u8]>,
    target: &str,
) -> Result<Vec<String>> {
    let mut dependents = Vec::new();
    for entry in views_table.iter()? {
        let (key, value) = entry?;
        if dependents.len() >= MAX_VIEWS {
            return Err(CatalogError::ViewLimitExceeded(
                "too many views".to_string(),
            ));
        }
        let def = decode_view_def(value.value())?;
        if def.base_relation == target {
            dependents.push(key.value().to_string());
        }
    }
    Ok(dependents)
}

/// 配列列（`ColumnType::Array`）の要素型（TABLE-14・Issue #888）。`VECTOR`・`ARRAY`
/// （入れ子・多次元配列）を構造的に除外し、`VECTOR` 列との責務境界を型で保証する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArrayElemType {
    Text,
    Bool,
}

impl ArrayElemType {
    /// カタログ v2 の `param` フィールド内で使う要素型タグ（Issue #888 D-A2）。
    fn catalog_tag(&self) -> &'static str {
        match self {
            ArrayElemType::Text => "text",
            ArrayElemType::Bool => "boolean",
        }
    }

    /// [`ArrayElemType::catalog_tag`] の逆変換。`vector`・`array`・未知タグは
    /// fail-closed に拒否する（配列の入れ子・`VECTOR` 要素は非対応。TABLE-14）。
    fn from_catalog_tag(tag: &str) -> Result<ArrayElemType> {
        match tag {
            "text" => Ok(ArrayElemType::Text),
            "boolean" => Ok(ArrayElemType::Bool),
            other => Err(CatalogError::Invalid(format!(
                "unsupported array element type: {other:?}"
            ))),
        }
    }
}

/// 配列列 1 個の宣言（要素型＋要素数上限。TABLE-14・TASK-198、Issue #888）。
/// フィールドを private にし [`ArrayType::new`] のみを構築経路とすることで、
/// 上限の範囲検証（`1..=MAX_ARRAY_ELEMENTS`）を経ない値を作れないようにする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArrayType {
    elem: ArrayElemType,
    max_len: u32,
}

/// 配列列 1 個が宣言できる要素数の実装上限（本リポの実装既定値。TASK-198）。
/// [`crate::row_codec`] の decode 側もこの値を要素数の上限検証に用いる。
pub const MAX_ARRAY_ELEMENTS: u32 = 1_024;

impl ArrayType {
    /// `max_len` が `1..=MAX_ARRAY_ELEMENTS` の範囲外なら `Err`（TABLE-14）。
    pub fn new(elem: ArrayElemType, max_len: u32) -> Result<Self> {
        if max_len == 0 || max_len > MAX_ARRAY_ELEMENTS {
            return Err(CatalogError::Invalid(format!(
                "array max_len must be within 1..={MAX_ARRAY_ELEMENTS}, got {max_len}"
            )));
        }
        Ok(Self { elem, max_len })
    }

    pub fn elem(&self) -> ArrayElemType {
        self.elem
    }

    pub fn max_len(&self) -> u32 {
        self.max_len
    }
}

/// `DEFAULT <literal>` 句（TABLE-16・TASK-204、Issue #904）が宣言する既定値。
/// [`crate::sql::allowlist::InsertLiteral`] の対応する 3 variant
/// （`String`／`Number`／`Bool`。`Vector`・`Null` は `DEFAULT` の文法上
/// 構造的に構築されない）を写した軽量表現で、`row_codec::Value` を直接
/// 持たない（`Value` は `Real`／`Double` に `f64`/`f32` を持ち `Eq` を
/// 実装できないため、`ColumnDef` の `Eq` 導出を維持できなくなる）。
///
/// `sql::parser::bind_literal_for_column`（省略列への補完・`INSERT`／
/// `UPDATE`／`UPSERT`・COPY が共有する単一の束縛点）がこの値を実際の列型へ
/// 束縛する際の型不一致・数値範囲外は通常の `INSERT` リテラルと同じ
/// エラー分類（`22000`/`22003` 等）で拒否する。カタログ層（本モジュール）は
/// [`column_default_compatible`] で列型の大分類（テキスト系／数値系／真偽値）
/// との整合のみを検証し、`sql` 層に依存しない自己完結の防御として持つ
/// （decode 時・Rust API 直接構築時にも効く多層防御）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ColumnDefault {
    Text(String),
    Number(String),
    Bool(bool),
}

/// [`ColumnDefault`] 1 個ぶんのカタログテキスト表現（未加工の値本体）の
/// バイト長上限。カタログ decode 時・CREATE TABLE 構文検証時の両方で、
/// 16 進符号化・アロケーションの前に検証する
/// （`.claude/rules/coding-rust.md`「untrusted 入力の扱い」）。
pub const MAX_COLUMN_DEFAULT_LEN: usize = 1024;

// `MAX_COLUMN_DEFAULT_LEN` を 16 進符号化（最大 2 倍）したうえで
// `MAX_COLUMN_COUNT` 列ぶん連結しても、カタログ値全体の上限
// （`MAX_CATALOG_VALUE_LEN`）に十分収まることをコンパイル時に固定する
// （プレフィックス 1 バイト・区切り文字・列名等の余地として 500,000 バイトの
// 余裕を残す）。
const _: () = assert!(
    MAX_COLUMN_DEFAULT_LEN * 2 * MAX_COLUMN_COUNT + 500_000 <= MAX_CATALOG_VALUE_LEN,
    "MAX_COLUMN_DEFAULT_LEN * 2 * MAX_COLUMN_COUNT must leave slack under MAX_CATALOG_VALUE_LEN"
);

impl ColumnDefault {
    /// この既定値が列型の大分類と整合するか（TABLE-16 D2）。`VECTOR` は
    /// 常に不可（DEFAULT 自体が禁止）。ここでは型の大分類のみを見る粗い
    /// フィルタで、数値の桁数・範囲・ENUM 語彙といった細かな整合性は
    /// `sql::parser::bind_literal_for_column` が実際の束縛時に検証する。
    fn compatible_with(&self, ty: &ColumnType) -> bool {
        matches!(
            (self, ty),
            (ColumnDefault::Text(_), ColumnType::Text)
                | (
                    ColumnDefault::Number(_),
                    ColumnType::Integer
                        | ColumnType::BigInt
                        | ColumnType::Real
                        | ColumnType::Double
                        | ColumnType::Numeric { .. },
                )
                | (ColumnDefault::Bool(_), ColumnType::Boolean)
        )
    }

    /// カタログテキスト形式（v5）の `default` フィールドへ符号化する。
    /// `-`（なし）は [`encode_column_line_v5`] 側が扱うため本関数は
    /// `Some` の場合のみ呼ばれる。値本体を 16 進化するのは、`TEXT` 既定値が
    /// `:`・改行等のカタログの区切り文字を含み得るため（区切り文字注入の
    /// 防止。TABLE-6 と同じ設計判断）。
    fn encode_catalog_field(&self) -> Result<String> {
        let (tag, raw): (char, String) = match self {
            ColumnDefault::Text(s) => ('s', s.clone()),
            ColumnDefault::Number(s) => ('n', s.clone()),
            ColumnDefault::Bool(b) => {
                return Ok(if *b { "t".to_string() } else { "f".to_string() })
            }
        };
        if raw.len() > MAX_COLUMN_DEFAULT_LEN {
            return Err(CatalogError::Invalid(format!(
                "column default literal exceeds length limit: {} bytes",
                raw.len()
            )));
        }
        let mut out = String::with_capacity(1 + raw.len() * 2);
        out.push(tag);
        for byte in raw.as_bytes() {
            out.push_str(&hex_encode_byte(*byte));
        }
        Ok(out)
    }

    /// [`ColumnDefault::encode_catalog_field`] の逆変換。未知タグ・不正 16 進・
    /// 不正 UTF-8・長さ上限超過はいずれも `Err`（fail-closed。TABLE-6 と同じ
    /// 「デコード不能なカタログ値を許さない」方針）。
    fn decode_catalog_field(field: &str) -> Result<Option<ColumnDefault>> {
        if field == "-" {
            return Ok(None);
        }
        if field == "t" {
            return Ok(Some(ColumnDefault::Bool(true)));
        }
        if field == "f" {
            return Ok(Some(ColumnDefault::Bool(false)));
        }
        let mut chars = field.chars();
        let tag = chars
            .next()
            .ok_or_else(|| CatalogError::Invalid("empty column default field".to_string()))?;
        let hex_body = chars.as_str();
        // 16 進復号前に長さ上限を検証する（1 バイトは 16 進 2 文字。奇数長は
        // 復号側で拒否されるが、上限判定はアロケーション前に済ませる）。
        if hex_body.len() > MAX_COLUMN_DEFAULT_LEN * 2 {
            return Err(CatalogError::Invalid(
                "column default field exceeds length limit".to_string(),
            ));
        }
        let bytes = hex_decode(hex_body)?;
        let text = String::from_utf8(bytes).map_err(|_| {
            CatalogError::Invalid("column default field is not valid UTF-8".to_string())
        })?;
        match tag {
            's' => Ok(Some(ColumnDefault::Text(text))),
            'n' => Ok(Some(ColumnDefault::Number(text))),
            other => Err(CatalogError::Invalid(format!(
                "unknown column default tag: {other:?}"
            ))),
        }
    }
}

/// 1 バイトを小文字 16 進 2 文字へ変換する（依存追加なしの自作。
/// dependency-policy.md 準拠）。
fn hex_encode_byte(b: u8) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let hi = HEX[(b >> 4) as usize] as char;
    let lo = HEX[(b & 0x0f) as usize] as char;
    let mut s = String::with_capacity(2);
    s.push(hi);
    s.push(lo);
    s
}

/// 小文字 16 進文字列をバイト列へ復号する。奇数長・非 16 進文字は `Err`
/// （fail-closed。untrusted なカタログ値を復号する経路のため添字直接
/// アクセスを避け `get`／`from_digit` 相当の明示判定を使う）。
fn hex_decode(s: &str) -> Result<Vec<u8>> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return Err(CatalogError::Invalid(
            "column default hex field has odd length".to_string(),
        ));
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    // 上のコメントが述べる「添字直接アクセスを避け明示判定を使う」方針を、
    // 実際に `[]` を書かない形で徹底する（Issue #904 レビュー指摘）。
    // 直前の `is_multiple_of(2)` 検査により `i + 1 < bytes.len()` の間は
    // `get(i)`／`get(i + 1)` が必ず `Some` になるが、`get` を使うことで
    // この不変条件が崩れても panic ではなく `Err` へ倒れる（fail-closed）。
    while i + 1 < bytes.len() {
        let (Some(&hi_byte), Some(&lo_byte)) = (bytes.get(i), bytes.get(i + 1)) else {
            return Err(CatalogError::Invalid(
                "column default hex field has odd length".to_string(),
            ));
        };
        let hi = hex_nibble(hi_byte)?;
        let lo = hex_nibble(lo_byte)?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => Err(CatalogError::Invalid(
            "column default field contains invalid hex digit".to_string(),
        )),
    }
}

/// テーブル定義中の 1 列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnDef {
    pub name: String,
    pub ty: ColumnType,
    /// `ALTER TABLE ADD COLUMN` で追加された列は暗黙 nullable とする（TABLE-5）。
    /// 実際の行デコード時の NULL 解決は行エンコーダー（TASK-86）の責務であり、
    /// 本モジュールはこのフラグを保持・往復させるのみ。
    pub nullable: bool,
    /// `DEFAULT <literal>` 句（TABLE-16・TASK-204、Issue #904）。`INSERT` で
    /// この列が省略された場合に補われる値。明示的な `NULL` には適用しない
    /// （TABLE-16。`sql::parser::bind_literal_for_column`／
    /// `fill_omitted_columns` が唯一の適用点）。
    pub default: Option<ColumnDefault>,
}

impl ColumnDef {
    pub fn new(name: impl Into<String>, ty: ColumnType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            ty,
            nullable,
            default: None,
        }
    }

    /// [`ColumnDef::new`] に `DEFAULT` を追加した版（TABLE-16・TASK-204、
    /// Issue #904）。`ColumnDef::new` の呼び出し元（約 1,200 箇所）を変えずに
    /// 済むよう、既定値の付与だけを別メソッドへ切り出す。
    pub fn with_default(mut self, default: ColumnDefault) -> Self {
        self.default = Some(default);
        self
    }
}

/// `ALTER TABLE ... DROP COLUMN` で削除された列の墓標（TABLE-19・TASK-203、
/// Issue #901）。物理行ペイロード（`row_codec::encode_scalar_columns` の出力）は
/// 列の宣言順に位置依存する固定形式のため、削除後も後続の生存列の物理位置を
/// ずらさないよう、削除列の位置・型だけをカタログに残す（値は残さない。
/// 削除後の新規書き込みは常に NULL・削除前からの既存行は構造検証のみ行い
/// 読み捨てる。`row_codec.rs` 参照）。
///
/// `ty` は削除前の型をフレーム等価型へ正規化した型を保持する: `ENUM`／`JSON`／
/// `JSONB` は行バイト列上 `TEXT` と同一フレーム（presence + u32 長 + 本体）の
/// ため `TEXT` へ正規化し、`DROP TYPE` が削除済み列の残存参照を理由に永久に
/// ブロックされ続ける結合を断つ（他の型は元の型をそのまま保持し、物理フレーム幅
/// を保つ）。`VECTOR` は削除不可のため現れない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedSlot {
    /// 物理スロット位置（0 始まり、昇順で保持する。[`TableSchema::physical_slots`]
    /// が生存列とこの位置でマージする）。
    physical_index: u16,
    /// 削除前の列名。生存列の列名重複検査の対象外（同名列の再追加は新しい
    /// 物理スロットを得る独立の列として扱う。旧値は復活しない）。
    name: String,
    /// 削除前の型（フレーム等価型への正規化後）。
    ty: ColumnType,
}

impl DroppedSlot {
    /// 削除前の列名（デバッグ・カタログエンコード専用）。
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// 削除前の型（フレーム等価型への正規化後）。行の物理走査でのみ使う。
    pub(crate) fn ty(&self) -> &ColumnType {
        &self.ty
    }
}

/// [`Storage::alter_table_drop_column`] が削除前の型を墓標へ格納する前に通す
/// 正規化（TABLE-19 D1・Issue #901）。`ENUM`／`JSON`／`JSONB` は行バイト列上
/// `TEXT` と同一フレーム（presence + u32 長 + 本体）のため `TEXT` へ正規化し、
/// `DROP TYPE`（[`Storage::drop_enum_type`]）が削除済み列の残存参照を理由に
/// 永久にブロックされ続ける結合を断つ。他の型は元の型をそのまま保持する
/// （物理フレーム幅を保つ必要があるため）。`VECTOR` は呼び出し元
/// （[`Storage::alter_table_drop_column`]）が先に拒否するためここには来ない。
fn normalize_dropped_column_type(ty: ColumnType) -> ColumnType {
    match ty {
        ColumnType::Enum(_) | ColumnType::Json | ColumnType::Jsonb => ColumnType::Text,
        other => other,
    }
}

/// [`TableSchema::physical_slots`] が返す 1 物理スロット。生存列は論理インデックス
/// （`schema.columns` への添字）付き、削除済み列は墓標そのものを返す。
pub(crate) enum PhysicalSlot<'a> {
    Live(usize, &'a ColumnDef),
    Dropped(&'a DroppedSlot),
}

/// [`TableSchema::physical_slots`] の実装。`dropped` は `physical_index` 昇順で
/// 保持されている前提（[`TableSchema::from_parts`]・[`Storage::alter_table_drop_column`]
/// の両方がこの不変条件を維持する）で、生存列（`columns` の宣言順）と墓標を
/// 物理位置の昇順にマージする。
pub(crate) struct PhysicalSlots<'a> {
    columns: std::iter::Enumerate<std::slice::Iter<'a, ColumnDef>>,
    dropped: std::iter::Peekable<std::slice::Iter<'a, DroppedSlot>>,
    next_physical_index: usize,
}

impl<'a> Iterator for PhysicalSlots<'a> {
    type Item = PhysicalSlot<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(d) = self.dropped.peek() {
            if d.physical_index as usize == self.next_physical_index {
                let d = self.dropped.next()?;
                self.next_physical_index += 1;
                return Some(PhysicalSlot::Dropped(d));
            }
        }
        let (logical_index, column) = self.columns.next()?;
        self.next_physical_index += 1;
        Some(PhysicalSlot::Live(logical_index, column))
    }
}

/// UNIQUE 制約（単一列・複数列）の宣言（TABLE-16・TASK-204、Issue #905）。
/// 一意性のスコープはテナント内に閉じる（[`crate::constraint`] の単一検査点が
/// テナント所有の全行——`Public`／`Private` を問わない——を母集合として
/// 検査する。RLS 可視集合ではない）。列は宣言順を保持する。
///
/// `columns()` が返す各列名は、この制約を保持する [`TableSchema`] の**生存列**
/// （`schema.columns`）に存在し、かつ [`ColumnType::is_unique_constraint_allowed`]
/// （PK・FK が共有する許可リストの上位集合。Issue #1073）を満たす型であることを
/// [`validate_schema`] が保証する契約とする（構築時点では検証しない。
/// [`ColumnDef::new`] と同じ「検証は呼び出し元が別途通す」設計）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniqueConstraint {
    /// 制約名（Issue #1067）。テーブル単位の名前空間を CHECK 制約名と共有する
    /// （PRIMARY KEY・FOREIGN KEY は名前を持たない）。空文字列は「名前未確定」を
    /// 表す一時状態で、[`assign_unique_constraint_names`] が
    /// `encode_schema`／`validate_schema` より前に確定する（TOCTOU 回避）。
    /// 空名のまま `validate_schema` へ到達した場合は fail-closed に拒否される
    /// （[`validate_unique_constraints`]）。
    name: String,
    columns: Vec<String>,
}

impl UniqueConstraint {
    /// `pub(crate)`: 構築元は `sql::allowlist`（`CREATE TABLE` の列制約・表制約。
    /// 名前は未確定のまま組み立てる）・[`decode_schema_body`]（v6〜v9 カタログ値の
    /// 復元。decode 後に名前を導出する）に限る。
    pub(crate) fn new(columns: Vec<String>) -> Self {
        Self {
            name: String::new(),
            columns,
        }
    }

    /// `pub(crate)`: 名前確定済みの UNIQUE 制約を構築する。構築元は
    /// [`assign_unique_constraint_names`]（既定名の確定）・[`decode_schema_body`]
    /// （v10／v11 カタログ値の復元。実名を永続化済み）・`sql::allowlist`（明示
    /// `CONSTRAINT <name> UNIQUE` の `ALTER TABLE ADD` 構文）に限る。
    pub(crate) fn with_name(name: String, columns: Vec<String>) -> Self {
        Self { name, columns }
    }

    /// 制約名（確定済みのスキーマでは常に非空）。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// 制約が参照する列名（宣言順）。
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
}

/// `CHECK` 制約 1 件分（TABLE-16・TASK-204、Issue #906）。catalog 層は SQL を
/// 解釈しない（`predicate_sql` は `sql::check_constraint` が正規化レンダリングした
/// テキストであり、本モジュールは中身を解釈せず不透明な文字列として持ち回す）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckConstraint {
    /// 制約名（識別子として妥当。同一テーブル内で一意）。
    pub(crate) name: String,
    /// 述語が参照するテーブル列名（疑似列 `id` は含めない）。
    /// [`Storage::alter_table_drop_column`]／`alter_table_widen_numeric_precision`
    /// の依存検査が使う（参照列は変更・削除しない。fail-closed）。
    pub(crate) columns: Vec<String>,
    /// `sql::check_constraint::render_predicates` が正規化レンダリングした
    /// SQL 述語テキスト（AND 連結の `WHERE` 述語文法）。
    pub(crate) predicate_sql: String,
}

/// `FOREIGN KEY` の参照アクション（Issue #1076・TABLE-17・TASK-205）。参照先の
/// 行が削除・更新（キー変更）されたとき、参照元（このスキーマ側）の行へ何を
/// 行うかを表す。`ON DELETE`・`ON UPDATE` それぞれ独立に宣言する
/// （[`ForeignKeyDef::on_delete`]／[`ForeignKeyDef::on_update`]）。
///
/// 適用順序・上限・テナント境界は `crate::constraint` モジュールドキュメント
/// 参照（連鎖の適用 → 変更対象テーブル全体の事後検証、の 2 段構え）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferentialAction {
    /// 既定。参照先の変更は、文の最後に行う事後検証（`23503`）でのみ扱う
    /// （`RESTRICT` 宣言もここへ正規化し、区別して永続化しない）。
    NoAction,
    /// 参照元の行を連鎖的に削除・更新する。
    Cascade,
    /// 参照元の FK 列をすべて `NULL` にする。
    SetNull,
    /// 参照元の FK 列をそれぞれの列 `DEFAULT`（無ければ `NULL`）にする。
    SetDefault,
}

/// `FOREIGN KEY` 制約 1 件分の宣言（TABLE-17・TASK-205、Issue #907。参照
/// アクションは Issue #1076）。参照元（この宣言を保持する [`TableSchema`]）の列
/// `columns` の値の組が、参照先テーブル `parent_table` の列 `parent_columns` の
/// 値の組として**同一テナント内**に存在することを要求する（`constraint` モジュール
/// の単一検査点が判定する。母集合はテナント所有の全行で、RLS 可視集合ではない。
/// RLS-10 (c)）。
///
/// `parent_columns` は `columns` と位置で対応し（`columns[i]` ↔ `parent_columns[i]`）、
/// カタログへ永続化される値では常に解決済み（非空）: 参照先の主キー・UNIQUE 制約の
/// いずれかと列集合が一致するか、`["id"]`（[`FOREIGN_KEY_PARENT_ID_COLUMN`]。
/// 物理キーの `id` 側）のいずれか。`REFERENCES <table>` のように参照先列を省略した
/// 宣言は、`CREATE TABLE` の write トランザクション内で参照先の主キー（未宣言なら
/// `id`）へ解決してから永続化する（[`Storage::create_table`]）。解決前の空
/// `parent_columns` は `sql::allowlist` が組み立てる中間表現でのみ現れる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignKeyDef {
    /// 制約名（Issue #1069）。テーブル単位の名前空間を UNIQUE・CHECK 制約名と
    /// 共有する（PRIMARY KEY は引き続き無名）。空文字列は「名前未確定」を表す
    /// 一時状態で、[`assign_foreign_key_constraint_names`] が `encode_schema`／
    /// `validate_schema` より前に確定する（TOCTOU 回避。`UniqueConstraint.name`
    /// と同じ設計。設計 D2 系）。空名のまま `validate_schema` へ到達した場合は
    /// fail-closed に拒否される（[`validate_foreign_keys`]）。
    name: String,
    columns: Vec<String>,
    parent_table: String,
    parent_columns: Vec<String>,
    on_delete: ReferentialAction,
    on_update: ReferentialAction,
    match_type: ForeignKeyMatch,
    deferrability: ForeignKeyDeferrability,
}

/// `FOREIGN KEY` の `MATCH` 句（TABLE-17・TASK-205、Issue #1077）。既定は
/// `Simple`。`constraint::push_required_key` が参照元側の NULL 混在判定に使う。
///
/// - `Simple`（PostgreSQL の既定）: 構成列のいずれかが NULL の組は検査対象外。
/// - `Full`: 構成列がすべて NULL の組のみ検査対象外とし、NULL と非 NULL が
///   混在する組は違反（`23503`）にする。単一列の `FOREIGN KEY` では `Simple`
///   と同じ挙動になる（NULL は 0 個か全部＝1 個のいずれかしかあり得ないため）。
///
/// `MATCH PARTIAL` は非対応のまま（`sql::allowlist` が `42601` で拒否する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ForeignKeyMatch {
    Simple,
    Full,
}

/// `FOREIGN KEY` の `[NOT] DEFERRABLE`／`INITIALLY {DEFERRED|IMMEDIATE}` 句
/// （TABLE-17・TASK-205、Issue #1077）。既定は `NotDeferrable`（従来どおり文単位で
/// 即時検査）。検査タイミングへの効き方は `constraint::FkCheckMode` 参照——
/// `DeferrableInitiallyDeferred` の宣言だけが、明示トランザクション
/// （`tenant::WriteTarget::InTxn`）中の文単位検査を COMMIT まで遅延できる
/// （autocommit では区別なく文単位で検査する）。`SET CONSTRAINTS` は非対応
/// （`DeferrableInitiallyImmediate` を実行時に遅延へ切り替える経路はない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ForeignKeyDeferrability {
    NotDeferrable,
    DeferrableInitiallyImmediate,
    DeferrableInitiallyDeferred,
}

impl ForeignKeyDef {
    /// `pub(crate)`: 構築元は `sql::allowlist`（`CREATE TABLE` の列制約・表制約）・
    /// [`decode_schema_body`]（v8〜v11 カタログ値の復元）・参照先解決
    /// （[`resolve_foreign_key_target`]）に限る。検証は [`validate_schema`] が担う。
    /// `MATCH`／遅延属性は既定値（`Simple`／`NotDeferrable`）になる
    /// （[`Self::with_options`] で明示指定を上書きする）。
    pub(crate) fn new(
        columns: Vec<String>,
        parent_table: String,
        parent_columns: Vec<String>,
        on_delete: ReferentialAction,
        on_update: ReferentialAction,
    ) -> Self {
        Self {
            name: String::new(),
            columns,
            parent_table,
            parent_columns,
            on_delete,
            on_update,
            match_type: ForeignKeyMatch::Simple,
            deferrability: ForeignKeyDeferrability::NotDeferrable,
        }
    }

    /// `pub(crate)`: 名前確定済みの `ForeignKeyDef` を返すビルダー（Issue #1069）。
    /// 構築元は [`assign_foreign_key_constraint_names`]（既定名の確定）・
    /// `sql::allowlist`（明示 `CONSTRAINT <name> FOREIGN KEY` の構文）・
    /// [`decode_schema_body`]（v12 カタログ値の復元。実名を永続化済み）・
    /// [`resolve_foreign_key_target`]（参照先解決時に元の宣言の名前を引き継ぐ）
    /// に限る。
    pub(crate) fn with_name(mut self, name: String) -> Self {
        self.name = name;
        self
    }

    /// 制約名（確定済みのスキーマでは常に非空）。
    pub fn name(&self) -> &str {
        &self.name
    }

    /// [`Self::new`] に `MATCH`・遅延属性を明示指定したコピーを返すビルダー
    /// （TABLE-17・TASK-205、Issue #1077）。`sql::allowlist` の `REFERENCES` 句
    /// 解析・[`decode_schema_body`]（v9／v11 カタログ値の復元）・
    /// [`resolve_foreign_key_target`]（参照先列の解決時にオプションを引き継ぐ）が
    /// 呼ぶ。
    pub(crate) fn with_options(
        mut self,
        match_type: ForeignKeyMatch,
        deferrability: ForeignKeyDeferrability,
    ) -> Self {
        self.match_type = match_type;
        self.deferrability = deferrability;
        self
    }

    /// 参照元（このスキーマ側）の列名（宣言順）。
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// 参照先テーブル名（自己参照ではこのスキーマ自身の名前）。
    pub fn parent_table(&self) -> &str {
        &self.parent_table
    }

    /// 参照先の列名（[`Self::columns`] と位置で対応）。永続化済みの宣言では常に
    /// 非空で、`["id"]` は物理キーの `id` 側を参照することを表す。
    pub fn parent_columns(&self) -> &[String] {
        &self.parent_columns
    }

    /// `ON DELETE` の参照アクション（Issue #1076）。
    pub fn on_delete(&self) -> ReferentialAction {
        self.on_delete
    }

    /// `ON UPDATE` の参照アクション（Issue #1076）。
    pub fn on_update(&self) -> ReferentialAction {
        self.on_update
    }

    /// `MATCH` 句（TABLE-17・TASK-205、Issue #1077）。
    pub fn match_type(&self) -> ForeignKeyMatch {
        self.match_type
    }

    /// 遅延属性（TABLE-17・TASK-205、Issue #1077）。
    pub fn deferrability(&self) -> ForeignKeyDeferrability {
        self.deferrability
    }

    /// `INITIALLY DEFERRED` で宣言されているか（`constraint::FkCheckMode` が
    /// 参照する。COMMIT まで検査を遅延できる唯一の区分）。
    pub(crate) fn is_initially_deferred(&self) -> bool {
        matches!(
            self.deferrability,
            ForeignKeyDeferrability::DeferrableInitiallyDeferred
        )
    }

    /// 参照先が `id` 疑似列（物理キー）であるか。
    pub(crate) fn references_parent_id(&self) -> bool {
        matches!(self.parent_columns.as_slice(), [only] if only == FOREIGN_KEY_PARENT_ID_COLUMN)
    }

    /// `MATCH`・遅延属性・参照アクションを無視した参照形状（参照元列・参照先
    /// テーブル・参照先列）の一致判定。重複宣言の検査（[`validate_foreign_keys`]・
    /// [`parse_foreign_key_section`]）が使う——同じ列の組についてオプション
    /// （アクション・`MATCH`・遅延属性）違いだけの宣言も重複として拒否する
    /// （fail-closed。TABLE-17・TASK-205、Issue #1076 A14／Issue #1077）。
    pub(crate) fn shares_reference_shape(&self, other: &Self) -> bool {
        self.columns == other.columns
            && self.parent_table == other.parent_table
            && self.parent_columns == other.parent_columns
    }
}

/// テーブル定義。列の宣言順を保持する（`ALTER TABLE ADD COLUMN` は末尾追記のみを
/// 許可する。TABLE-5）。`columns` は常に**論理列（生存列のみ）**を宣言順で持つ。
/// `SELECT *`・投影・`WHERE` 解決・`RowDescription` など、行の物理配置を
/// 意識する必要のないほぼすべての呼び出し元はこのフィールドだけを見ればよい。
///
/// 削除済み列（[`DroppedSlot`]）は非公開フィールド `dropped` に持ち、
/// [`TableSchema::physical_slots`] が両者を物理位置でマージするビューを提供する。
/// 物理配置を意識する必要があるのは行の物理ペイロード（`row_codec.rs`）と
/// カタログの encode/decode（本モジュール）だけである（TABLE-19・TASK-203、
/// Issue #901）。
///
/// **破壊的変更（Issue #901）**: 従来 `pub name`／`pub columns` のみで構成
/// されていた本型に非公開フィールド `dropped` を追加したため、外部クレート
/// からの `TableSchema { name, columns }` という構造体リテラル構築はコンパ
/// イル不能になった。移行先は [`TableSchema::new`]（クレート内の呼び出し元
/// は移行済み）。詳細・spec 側の扱いは
/// `docs/design/alter-table-drop-modify-column.md`「D5」参照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableSchema {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    dropped: Vec<DroppedSlot>,
    /// `PRIMARY KEY` 宣言（TABLE-16・TASK-204、Issue #903）。`id` 暗黙主キーの
    /// テーブルは `None`（既存挙動・カタログバイト列とも完全不変。
    /// `docs/design/sql-primary-key.md` 参照）。`Some` のときは列**名**の宣言順
    /// 列挙（列の物理位置がずれても安全なように名前で持つ。`DROP COLUMN` は
    /// 主キー構成列を拒否するため、主キー宣言後にここへ現れる名前は常に生存列を
    /// 指す）。空 `Vec` は許さない（[`validate_schema`] が拒否する）。
    primary_key: Option<Vec<String>>,
    /// UNIQUE 制約（TABLE-16・TASK-204、Issue #905）。空が既定（カタログ
    /// v2〜v5 のバイト列不変）。1 件以上持つスキーマは v6 で永続化される。
    unique_constraints: Vec<UniqueConstraint>,
    /// `CHECK` 制約（TABLE-16・TASK-204、Issue #906）。空が既定（カタログ
    /// v2〜v6 のバイト列不変）。1 件以上持つスキーマは v7 で永続化される。
    /// SQL DDL（`sql::ddl::execute_create_table`）以外から任意の述語テキストを
    /// 差し込めないよう [`TableSchema::with_checks`] は `pub(crate)` に留める。
    checks: Vec<CheckConstraint>,
    /// `FOREIGN KEY` 制約（TABLE-17・TASK-205、Issue #907）。空が既定（カタログ
    /// v2〜v7 のバイト列不変）。1 件以上持つスキーマは v8 で永続化される。
    /// 参照先の解決（主キー・UNIQUE 制約との照合）はカタログ参照を要するため
    /// [`Storage::create_table`] の write トランザクション内で行う。
    foreign_keys: Vec<ForeignKeyDef>,
}

impl TableSchema {
    pub fn new(name: impl Into<String>, columns: Vec<ColumnDef>) -> Self {
        Self {
            name: name.into(),
            columns,
            dropped: Vec::new(),
            primary_key: None,
            unique_constraints: Vec::new(),
            checks: Vec::new(),
            foreign_keys: Vec::new(),
        }
    }

    /// [`TableSchema::new`] の削除済み列（墓標）・主キー付き版。呼び出し元
    /// （本モジュールのカタログ decode）は `dropped` を `physical_index` 昇順で
    /// 渡す契約とする（[`PhysicalSlots`] の前提）。バリデーションは行わない
    /// （呼び出し元が [`validate_schema`] を別途通す）。
    pub(crate) fn from_parts(
        name: impl Into<String>,
        columns: Vec<ColumnDef>,
        dropped: Vec<DroppedSlot>,
        primary_key: Option<Vec<String>>,
        unique_constraints: Vec<UniqueConstraint>,
    ) -> Self {
        Self {
            name: name.into(),
            columns,
            dropped,
            primary_key,
            unique_constraints,
            checks: Vec::new(),
            foreign_keys: Vec::new(),
        }
    }

    /// `CHECK` 制約（TABLE-16・TASK-204、Issue #906）を設定したコピーを返す
    /// ビルダー。`sql::ddl::execute_create_table`（`CREATE TABLE` の検証結果）・
    /// [`decode_schema_body`]（v7 カタログ値の復元）が使う。バリデーションは
    /// 行わない（[`encode_schema`] 内の [`validate_schema`] が別途通す）。
    pub(crate) fn with_checks(mut self, checks: Vec<CheckConstraint>) -> Self {
        self.checks = checks;
        self
    }

    /// 宣言済みの `CHECK` 制約一覧（宣言順。TABLE-16・TASK-204、Issue #906）。
    pub(crate) fn checks(&self) -> &[CheckConstraint] {
        &self.checks
    }

    /// `FOREIGN KEY` 制約（TABLE-17・TASK-205、Issue #907）を設定したコピーを
    /// 返すビルダー。`sql::ddl::execute_create_table`（`CREATE TABLE` の検証結果）・
    /// [`decode_schema_body`]（v8 カタログ値の復元）・参照先解決
    /// （[`resolve_foreign_keys_in_txn`]）が使う。バリデーションは行わない
    /// （[`encode_schema`] 内の [`validate_schema`] が別途通す）。Rust API から任意の
    /// 参照先を差し込めないよう `pub(crate)` に留める（宣言面は SQL 表層の
    /// `CREATE TABLE` のみ）。
    pub(crate) fn with_foreign_keys(mut self, foreign_keys: Vec<ForeignKeyDef>) -> Self {
        self.foreign_keys = foreign_keys;
        self
    }

    /// 宣言済みの `FOREIGN KEY` 制約一覧（宣言順。TABLE-17・TASK-205、Issue #907）。
    pub fn foreign_keys(&self) -> &[ForeignKeyDef] {
        &self.foreign_keys
    }

    /// `PRIMARY KEY` 宣言列名（宣言順）。未宣言（`id` 暗黙主キー）は `None`
    /// （TABLE-16・TASK-204、Issue #903）。
    pub fn primary_key(&self) -> Option<&[String]> {
        self.primary_key.as_deref()
    }

    /// [`Self::primary_key`] を設定したコピーを返すビルダー（Rust API 用。
    /// SQL 表層は [`Self::from_parts`] を経由するカタログ decode のみが
    /// `primary_key` を持つ `TableSchema` を組み立てる）。バリデーションは
    /// 行わない（呼び出し元が [`Storage::create_table`] を通し `validate_schema`
    /// で検証させる契約とする）。
    pub fn with_primary_key(mut self, columns: Vec<String>) -> Self {
        self.primary_key = Some(columns);
        self
    }

    /// UNIQUE 制約（TABLE-16・TASK-204、Issue #905）を設定したコピーを返す
    /// ビルダー。`sql::ddl::execute_create_table`・
    /// [`Storage::alter_table_add_unique_constraint`] が使う。バリデーションは
    /// 行わない（[`encode_schema`] 内の [`validate_schema`] が別途通す）。
    pub(crate) fn with_unique_constraints(
        mut self,
        unique_constraints: Vec<UniqueConstraint>,
    ) -> Self {
        self.unique_constraints = unique_constraints;
        self
    }

    /// 宣言済み UNIQUE 制約の一覧（宣言順。TABLE-16・TASK-204、Issue #905）。
    pub fn unique_constraints(&self) -> &[UniqueConstraint] {
        &self.unique_constraints
    }

    /// 削除済み列（墓標）の一覧。`physical_index` 昇順。
    pub(crate) fn dropped_slots(&self) -> &[DroppedSlot] {
        &self.dropped
    }

    /// 物理スロット総数（生存列 + 墓標）。列数上限 [`MAX_COLUMN_COUNT`] は
    /// この値に適用する（墓標も物理容量を消費する。TABLE-19 D1）。
    pub(crate) fn physical_slot_count(&self) -> usize {
        self.columns.len() + self.dropped.len()
    }

    /// 生存列と墓標を物理位置の昇順でマージした走査（`row_codec.rs` の行
    /// ペイロード走査・カタログ encode が共有する唯一の物理配置ビュー）。
    pub(crate) fn physical_slots(&self) -> PhysicalSlots<'_> {
        PhysicalSlots {
            columns: self.columns.iter().enumerate(),
            dropped: self.dropped.iter().peekable(),
            next_physical_index: 0,
        }
    }

    /// 宣言済みの埋め込み次元（`VECTOR(N)` 列のうち最初に見つかったもの、TABLE-1）。
    pub fn vector_dim(&self) -> Option<u32> {
        self.columns.iter().find_map(|c| match &c.ty {
            ColumnType::Vector(dim) => Some(*dim),
            ColumnType::Text
            | ColumnType::Integer
            | ColumnType::BigInt
            | ColumnType::Real
            | ColumnType::Double
            | ColumnType::Boolean
            | ColumnType::Date
            | ColumnType::Timestamp
            | ColumnType::Bytea
            | ColumnType::Json
            | ColumnType::Jsonb
            | ColumnType::Enum(_)
            | ColumnType::Array(_)
            | ColumnType::Numeric { .. }
            | ColumnType::Uuid => None,
        })
    }

    /// 挿入経路（TASK-86 以降）が、宣言済み次元と一致しない埋め込みを拒否するための
    /// 検証ヘルパ（TABLE-1）。`VECTOR` 列を持たないテーブルへの呼び出しも
    /// fail-closed に拒否する。
    ///
    /// `VECTOR` 列の SET／値が実際にある場合の次元検証（単一行 UPDATE・述語つき
    /// UPDATE・UPSERT の `DO UPDATE`）はこのメソッドを直接使う。これらの呼び出し元は
    /// `VECTOR` 列への SET があった場合のみ本メソッドを呼ぶガード（`vector_assigned`
    /// 等）で囲っているため、`VECTOR` 列を持たないテーブルでは通常到達しない。
    /// 行全体を書き込む経路（`RowInput` による INSERT・行全体置換 UPDATE）は
    /// [`Self::validate_row_embedding_dim`] を使う（TABLE-1・Issue #995・#1079）。
    pub fn validate_embedding_dim(&self, dim: usize) -> Result<()> {
        let expected = self
            .vector_dim()
            .ok_or_else(|| CatalogError::Invalid("table has no VECTOR column".to_string()))?;
        if dim as u64 != expected as u64 {
            return Err(CatalogError::Invalid(format!(
                "embedding dim mismatch: expected {expected}, got {dim}"
            )));
        }
        Ok(())
    }

    /// 行全体を書き込む経路（INSERT 系、および `RowInput` による行全体置換 UPDATE。
    /// TABLE-1・Issue #995・#1079）向けの embedding 次元検証。
    ///
    /// `VECTOR` 列を持つスキーマでは [`Self::validate_embedding_dim`] へそのまま
    /// 委譲し、エラー分類・文言は完全に同一のまま変えない（Issue #995 受け入れ
    /// 基準「`VECTOR` 列を持つスキーマの次元検証・エラー分類は変わらない」）。
    ///
    /// `VECTOR` 列を持たないスキーマでは、読み取り経路（`sql/scan.rs`・
    /// `sql/aggregate.rs` の `expected_dim: Option<u32>`・`storage::decode_row*`）
    /// が既に採用している「dim 0 の行として扱う」モデルに合わせ、`dim == 0` の
    /// ときのみ受理する。非空 embedding（`dim > 0`）は
    /// `validate_embedding_dim` と同じ文言・`CatalogError::Invalid` で
    /// fail-closed に拒否する（`tests/extensions.rs` の既存拒否テストが固定）。
    pub(crate) fn validate_row_embedding_dim(&self, dim: usize) -> Result<()> {
        if self.vector_dim().is_none() {
            if dim == 0 {
                return Ok(());
            }
            return Err(CatalogError::Invalid(
                "table has no VECTOR column".to_string(),
            ));
        }
        self.validate_embedding_dim(dim)
    }
}

/// 識別子（テーブル名・列名）の検証（TABLE-6）。`[A-Za-z_][A-Za-z0-9_]*` のみを許容し、
/// 空文字列・非 ASCII・区切り文字混入・長さ上限超過を fail-closed に拒否する。
/// encode 側・decode 側の両方から呼ばれ、永続データが手で書き換えられた場合も
/// 同じ検証を通す。
///
/// `pub(crate)`: `tenant.rs`（TASK-95・対象ビヘイビア: RECOVER-4）が書き込みガード API
/// （`insert_row`/`update_row`/`delete_row`）内の同一 write トランザクションから、
/// テーブル名検証をここへ委譲する（重複実装を作らない。クレート外へは公開しない）。
pub(crate) fn validate_identifier(s: &str) -> Result<()> {
    if s.is_empty() {
        return Err(CatalogError::Invalid("identifier is empty".to_string()));
    }
    if s.len() > MAX_IDENTIFIER_LEN {
        return Err(CatalogError::Invalid(format!(
            "identifier too long: {} bytes",
            s.len()
        )));
    }
    let mut chars = s.chars();
    // 上記 is_empty チェック済みのため先頭文字は必ず存在する。
    let first = chars
        .next()
        .ok_or_else(|| CatalogError::Invalid("identifier is empty".to_string()))?;
    if !(first.is_ascii_alphabetic() || first == '_') {
        return Err(CatalogError::Invalid(format!(
            "identifier must start with [A-Za-z_]: {s:?}"
        )));
    }
    for c in chars {
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return Err(CatalogError::Invalid(format!(
                "identifier contains invalid character: {s:?}"
            )));
        }
    }
    Ok(())
}

/// `VECTOR(N)` の次元数検証（TABLE-6）。0 と `MAX_VECTOR_DIM` 超過を拒否する。
fn validate_vector_dim(dim: u32) -> Result<()> {
    if dim == 0 {
        return Err(CatalogError::Invalid(
            "VECTOR dimension must not be zero".to_string(),
        ));
    }
    if dim > MAX_VECTOR_DIM {
        return Err(CatalogError::Invalid(format!(
            "VECTOR dimension too large: {dim}"
        )));
    }
    Ok(())
}

/// `NUMERIC(precision, scale)` の宣言制約検証（TABLE-13〔検討中〕・TASK-197、
/// Issue #885・D1）。`1 <= precision <= MAX_PRECISION`・`0 <= scale <= precision`
/// を満たさない宣言は encode・decode 両側で fail-closed に拒否する。
fn validate_numeric_precision_scale(precision: u8, scale: u8) -> Result<()> {
    if precision == 0 || precision > crate::numeric::MAX_PRECISION {
        return Err(CatalogError::Invalid(format!(
            "NUMERIC precision must be between 1 and {}: {precision}",
            crate::numeric::MAX_PRECISION
        )));
    }
    if scale > precision {
        return Err(CatalogError::Invalid(format!(
            "NUMERIC scale must not exceed precision: scale={scale} precision={precision}"
        )));
    }
    Ok(())
}

/// カタログ `param` フィールド（`"p,s"`）を `(precision, scale)` へ厳格パースする
/// （Issue #885・D1）。カンマはちょうど 1 個、各要素は ASCII 数字のみからなる
/// `u8`、範囲は [`validate_numeric_precision_scale`] で検証する。再 encode
/// した結果が入力と一致しない非正規形（先頭ゼロ等。例: `"010,2"`）も
/// fail-closed に拒否する。
fn parse_numeric_param(param: &str) -> Result<(u8, u8)> {
    let mut parts = param.split(',');
    let precision_str = parts
        .next()
        .ok_or_else(|| CatalogError::Invalid(format!("malformed NUMERIC parameter: {param:?}")))?;
    let scale_str = parts
        .next()
        .ok_or_else(|| CatalogError::Invalid(format!("malformed NUMERIC parameter: {param:?}")))?;
    if parts.next().is_some() {
        return Err(CatalogError::Invalid(format!(
            "malformed NUMERIC parameter: {param:?}"
        )));
    }
    let precision: u8 = precision_str.parse().map_err(|_| {
        CatalogError::Invalid(format!("malformed NUMERIC precision: {precision_str:?}"))
    })?;
    let scale: u8 = scale_str
        .parse()
        .map_err(|_| CatalogError::Invalid(format!("malformed NUMERIC scale: {scale_str:?}")))?;
    // 非正規形（先頭ゼロ等）の拒否: 再 encode した文字列が入力と一致するかで
    // 判定する（`u8::to_string()` は正規形しか生成しないため、`"010"` の
    // ような入力は不一致になる）。
    if precision.to_string() != precision_str || scale.to_string() != scale_str {
        return Err(CatalogError::Invalid(format!(
            "non-canonical NUMERIC parameter: {param:?}"
        )));
    }
    validate_numeric_precision_scale(precision, scale)?;
    Ok((precision, scale))
}

fn validate_column(column: &ColumnDef) -> Result<()> {
    validate_identifier(&column.name)?;
    if let ColumnType::Vector(dim) = &column.ty {
        validate_vector_dim(*dim)?;
    }
    if let ColumnType::Numeric { precision, scale } = column.ty {
        validate_numeric_precision_scale(precision, scale)?;
    }
    Ok(())
}

/// スキーマ全体の検証（テーブル名・列定義・列数上限・列名重複・`VECTOR` 列数）。
/// `create_table`・`alter_table_add_column`（追加後のスキーマ）の両方から呼ばれる。
///
/// `VECTOR` 列は高々 1 つに制限する（TABLE-1）。複数の `VECTOR` 列を許すと
/// [`TableSchema::vector_dim`] が先頭列のみを見て後続列を黙殺する fail-open な
/// 状態になり得るため、ここで拒否する（.claude/rules/security.md「不安全な設計」）。
fn validate_schema(schema: &TableSchema) -> Result<()> {
    validate_identifier(&schema.name)?;
    if schema.columns.is_empty() {
        return Err(CatalogError::Invalid(
            "table must have at least one column".to_string(),
        ));
    }
    // 列数上限は物理スロット総数（生存列 + 墓標）に適用する。墓標も
    // 物理容量（行ペイロードの位置空間）を消費するため（TABLE-19 D1・
    // Issue #901）。
    if schema.physical_slot_count() > MAX_COLUMN_COUNT {
        return Err(CatalogError::Invalid(format!(
            "too many columns: {}",
            schema.physical_slot_count()
        )));
    }
    let mut seen: Vec<&str> = Vec::with_capacity(schema.columns.len());
    let mut vector_column_count = 0u32;
    for column in &schema.columns {
        validate_column(column)?;
        if seen.contains(&column.name.as_str()) {
            return Err(CatalogError::Invalid(format!(
                "duplicate column name: {}",
                column.name
            )));
        }
        seen.push(column.name.as_str());
        if column.ty.is_vector() {
            vector_column_count += 1;
        }
        // `DEFAULT`（TABLE-16・TASK-204、Issue #904）は列型の大分類と整合する
        // ものだけを許可する（`VECTOR` は常に不可）。SQL 表層の構文検証
        // （`sql::allowlist::parse_create_table_column`）をすり抜けた場合も、
        // Rust API 直接構築の場合も、ここで fail-closed に拒否する。
        if let Some(default) = &column.default {
            if !default.compatible_with(&column.ty) {
                return Err(CatalogError::Invalid(format!(
                    "column {:?} has a DEFAULT that is not compatible with its type",
                    column.name
                )));
            }
        }
    }
    if vector_column_count > 1 {
        return Err(CatalogError::Invalid(format!(
            "table must declare at most one VECTOR column, got {vector_column_count}"
        )));
    }
    // 墓標側の不変条件（TABLE-19 D1・D2、Issue #901）: 削除前の物理位置が
    // 一意・昇順であること（[`PhysicalSlots`] の前提）、削除前の型が
    // フレーム等価型へ正規化済み（`VECTOR`／`ENUM`／`JSON`／`JSONB` を含まない）
    // であること、削除前の列名が識別子として妥当であること（重複検査は
    // 生存列同士のみで、墓標は対象外）。
    let mut last_physical_index: Option<u16> = None;
    for dropped in &schema.dropped {
        validate_identifier(&dropped.name)?;
        if matches!(
            dropped.ty,
            ColumnType::Vector(_) | ColumnType::Enum(_) | ColumnType::Json | ColumnType::Jsonb
        ) {
            return Err(CatalogError::Invalid(format!(
                "dropped column {:?} must be normalized to a frame-equivalent type",
                dropped.name
            )));
        }
        if let ColumnType::Numeric { precision, scale } = dropped.ty {
            validate_numeric_precision_scale(precision, scale)?;
        }
        match last_physical_index {
            Some(prev) if prev >= dropped.physical_index => {
                return Err(CatalogError::Invalid(
                    "dropped column slots are not in strictly ascending physical order".to_string(),
                ));
            }
            _ => {}
        }
        last_physical_index = Some(dropped.physical_index);
    }
    if let Some(last) = last_physical_index {
        if last as usize >= schema.physical_slot_count() {
            return Err(CatalogError::Invalid(
                "dropped column physical index out of range".to_string(),
            ));
        }
    }
    if let Some(pk_cols) = &schema.primary_key {
        validate_primary_key(schema, pk_cols)?;
    }
    validate_unique_constraints(schema)?;
    validate_check_constraints(schema)?;
    validate_foreign_keys(schema, false)?;
    // UNIQUE 制約名と CHECK 制約名はテーブル単位の名前空間を共有する（設計 D1・
    // Issue #1067）。個々の検査（`validate_unique_constraints`・
    // `validate_check_constraints`）はそれぞれの集合内の重複しか見ないため、
    // ここで両者にまたがる衝突を追加で検査する。
    for uc in &schema.unique_constraints {
        if schema.checks.iter().any(|c| c.name == uc.name) {
            return Err(CatalogError::Invalid(format!(
                "constraint name {} is already used by a CHECK constraint on this table",
                uc.name
            )));
        }
    }
    // `FOREIGN KEY` 名も同じ名前空間に加わる（設計 F1。TABLE-22・TASK-233、
    // Issue #1069）。FK 同士の重複は `validate_foreign_keys` が検査済みのため、
    // ここでは UNIQUE・CHECK との衝突のみを追加検査する。
    for fk in &schema.foreign_keys {
        if fk.name.is_empty() {
            continue;
        }
        if schema.checks.iter().any(|c| c.name == fk.name) {
            return Err(CatalogError::Invalid(format!(
                "constraint name {} is already used by a CHECK constraint on this table",
                fk.name
            )));
        }
        if schema
            .unique_constraints
            .iter()
            .any(|u| u.name() == fk.name)
        {
            return Err(CatalogError::Invalid(format!(
                "constraint name {} is already used by a UNIQUE constraint on this table",
                fk.name
            )));
        }
    }
    Ok(())
}

/// `FOREIGN KEY` 制約（[`ForeignKeyDef`]。TABLE-17・TASK-205、Issue #907）の
/// 不変条件検査のうち、参照先カタログを参照せずに判定できるもの:
/// 件数上限・列数上限・列名の識別子妥当性・参照元列の実在（生存列）と重複なし・
/// 参照元列の型適格性（一意キー許可型。`id` 参照は `INTEGER`／`BIGINT`）・
/// 参照先列数と参照元列数の一致・`id` 参照は単一列のみ・同一宣言の重複なし。
/// 自己参照（参照先＝このスキーマ）は参照先もこのスキーマ自身のため、参照先の
/// 主キー・UNIQUE 制約との照合（[`resolve_foreign_key_target`]）までここで行う
/// （カタログ decode 時にも自己参照の整合を再検証する多層防御）。
///
/// `allow_unresolved == true` は `CREATE TABLE` の write トランザクション前の
/// 事前検証専用で、参照先列の省略（空の `parent_columns`）を許容する。永続化値・
/// decode 結果は常に `false` で検証する（未解決の宣言を永続化しない）。
fn validate_foreign_keys(schema: &TableSchema, allow_unresolved: bool) -> Result<()> {
    if schema.foreign_keys.len() > MAX_FOREIGN_KEYS_PER_TABLE {
        return Err(CatalogError::Invalid(format!(
            "too many foreign keys: {}",
            schema.foreign_keys.len()
        )));
    }
    for (i, fk) in schema.foreign_keys.iter().enumerate() {
        // 制約名（TABLE-22・TASK-233、Issue #1069。設計 F2）: `allow_unresolved`
        // （`create_table` の write トランザクション前の事前検証専用）は
        // `sql::allowlist` が組み立てる名前未確定（空名）の宣言を許容する。
        // 永続化値・decode 結果・`encode_schema` の最終検証（`allow_unresolved
        // == false`）では、`assign_foreign_key_constraint_names` が
        // `validate_schema` より前に必ず確定するため、空名のまま到達した場合は
        // fail-closed に拒否する（`UniqueConstraint` と同じ設計）。
        if !allow_unresolved {
            if fk.name.is_empty() {
                return Err(CatalogError::Invalid(
                    "foreign key constraint name is not resolved".to_string(),
                ));
            }
            validate_identifier(&fk.name)?;
            if schema
                .foreign_keys
                .iter()
                .take(i)
                .any(|other| other.name == fk.name)
            {
                return Err(CatalogError::Invalid(format!(
                    "duplicate foreign key constraint name: {}",
                    fk.name
                )));
            }
        }
        validate_identifier(&fk.parent_table)?;
        if fk.columns.is_empty() {
            return Err(CatalogError::Invalid(
                "foreign key must reference at least one column".to_string(),
            ));
        }
        if fk.columns.len() > MAX_FOREIGN_KEY_COLUMNS
            || fk.parent_columns.len() > MAX_FOREIGN_KEY_COLUMNS
        {
            return Err(CatalogError::Invalid(
                "foreign key references too many columns".to_string(),
            ));
        }
        let mut seen: Vec<&str> = Vec::with_capacity(fk.columns.len());
        for name in &fk.columns {
            validate_identifier(name)?;
            if seen.contains(&name.as_str()) {
                return Err(CatalogError::Invalid(format!(
                    "foreign key references column {name} more than once"
                )));
            }
            seen.push(name.as_str());
            let column = schema
                .columns
                .iter()
                .find(|c| &c.name == name)
                .ok_or_else(|| {
                    CatalogError::Invalid(format!("foreign key references unknown column: {name}"))
                })?;
            if !column.ty.is_primary_key_allowed() {
                return Err(CatalogError::InvalidForeignKey(format!(
                    "column {name} has a type that cannot be used in a foreign key"
                )));
            }
        }
        if fk.parent_columns.is_empty() {
            if !allow_unresolved {
                return Err(CatalogError::Invalid(
                    "foreign key referenced columns are not resolved".to_string(),
                ));
            }
        } else {
            let mut seen_parent: Vec<&str> = Vec::with_capacity(fk.parent_columns.len());
            for name in &fk.parent_columns {
                validate_identifier(name)?;
                if seen_parent.contains(&name.as_str()) {
                    return Err(CatalogError::InvalidForeignKey(format!(
                        "foreign key references parent column {name} more than once"
                    )));
                }
                seen_parent.push(name.as_str());
            }
            if fk.parent_columns.len() != fk.columns.len() {
                return Err(CatalogError::InvalidForeignKey(
                    "number of referencing and referenced columns for foreign key disagree"
                        .to_string(),
                ));
            }
            if fk
                .parent_columns
                .iter()
                .any(|c| c == FOREIGN_KEY_PARENT_ID_COLUMN)
                && !fk.references_parent_id()
            {
                return Err(CatalogError::InvalidForeignKey(
                    "the id column can only be referenced alone".to_string(),
                ));
            }
            // `id` 参照の参照元列の型は参照先スキーマを要さずに判定できる
            // （カタログ decode 時にも再検証する多層防御）。
            if fk.references_parent_id() {
                for name in &fk.columns {
                    let is_integer = schema.columns.iter().any(|c| {
                        &c.name == name && matches!(c.ty, ColumnType::Integer | ColumnType::BigInt)
                    });
                    if !is_integer {
                        return Err(CatalogError::InvalidForeignKey(format!(
                            "column {name} must be INTEGER or BIGINT to reference id"
                        )));
                    }
                }
            }
        }
        if schema
            .foreign_keys
            .iter()
            .take(i)
            .any(|other| other.shares_reference_shape(fk))
        {
            return Err(CatalogError::Invalid(
                "duplicate foreign key declaration".to_string(),
            ));
        }
        // 参照アクションの宣言時検査（Issue #1076 A8）。ALTER で FK 列の
        // nullability・DEFAULT を後から変える経路が無い（DROP COLUMN は FK 構成列を
        // 拒否する）ため、この検査は宣言後も恒久的に有効であり続ける。
        validate_referential_action_declaration(schema, fk)?;
        // 自己参照は参照先の宣言がこのスキーマ自身にあるため、参照先の照合まで
        // 静的に検証できる（解決済みの宣言のみ。未解決は `create_table` の
        // write トランザクション内で解決・照合する）。
        if fk.parent_table == schema.name && !fk.parent_columns.is_empty() {
            resolve_foreign_key_target(schema, fk, schema)?;
        }
    }
    Ok(())
}

/// `FOREIGN KEY` の参照アクション（Issue #1076 A8）が常に失敗する宣言を拒否する。
/// (a) `SET NULL`（`ON DELETE`／`ON UPDATE` いずれか）で参照元列に `NOT NULL` の
/// 列がある。(b) `SET DEFAULT` で参照元列に「`DEFAULT` が無く、かつ `NOT NULL`」の
/// 列がある。参照元スキーマだけで判定できる（カタログ decode 時にも再検証する
/// 多層防御）。参照先列の nullable が必要な (c) は [`resolve_foreign_key_target`]
/// （参照先が確定する `CREATE TABLE` の write トランザクション内）が担う。
fn validate_referential_action_declaration(schema: &TableSchema, fk: &ForeignKeyDef) -> Result<()> {
    let uses_set_null = matches!(fk.on_delete, ReferentialAction::SetNull)
        || matches!(fk.on_update, ReferentialAction::SetNull);
    let uses_set_default = matches!(fk.on_delete, ReferentialAction::SetDefault)
        || matches!(fk.on_update, ReferentialAction::SetDefault);
    if !uses_set_null && !uses_set_default {
        return Ok(());
    }
    for name in &fk.columns {
        let column = schema
            .columns
            .iter()
            .find(|c| &c.name == name)
            .ok_or_else(|| {
                CatalogError::Invalid(format!("foreign key references unknown column: {name}"))
            })?;
        if uses_set_null && !column.nullable {
            return Err(CatalogError::InvalidForeignKey(format!(
                "column {name} is NOT NULL and cannot use the SET NULL referential action"
            )));
        }
        if uses_set_default && !column.nullable && column.default.is_none() {
            return Err(CatalogError::InvalidForeignKey(format!(
                "column {name} is NOT NULL without a DEFAULT and cannot use the SET DEFAULT referential action"
            )));
        }
    }
    Ok(())
}

/// `FOREIGN KEY` 宣言 `fk`（参照元 `child`）を参照先スキーマ `parent` に照らして
/// 解決・検証する（TABLE-17・TASK-205、Issue #907）。参照先列の省略は `parent` の
/// 主キー（未宣言なら `id` 疑似列）へ解決する。解決後の参照先列は、`id` 単独・
/// `parent` の主キー・UNIQUE 制約のいずれかと**列集合**が一致しなければならず
/// （順序は問わない。対応は `columns` との位置で決まる）、参照元列と参照先列の
/// 型は位置ごとに一致しなければならない（`id` 参照は参照元列が `INTEGER`／
/// `BIGINT`）。いずれの違反も [`CatalogError::InvalidForeignKey`]（`42830`）。
/// 一意性を保証しない列集合を参照先にすると、参照先の同値行が 1 行削除されても
/// 残りの行が参照を満たし続ける等、NO ACTION の意味論が定まらないため拒否する。
fn resolve_foreign_key_target(
    child: &TableSchema,
    fk: &ForeignKeyDef,
    parent: &TableSchema,
) -> Result<ForeignKeyDef> {
    let parent_columns: Vec<String> = if fk.parent_columns.is_empty() {
        match parent.primary_key() {
            Some(pk) => pk.to_vec(),
            None => vec![FOREIGN_KEY_PARENT_ID_COLUMN.to_string()],
        }
    } else {
        fk.parent_columns.clone()
    };
    if parent_columns.len() != fk.columns.len() {
        return Err(CatalogError::InvalidForeignKey(
            "number of referencing and referenced columns for foreign key disagree".to_string(),
        ));
    }
    let child_type = |name: &str| -> Result<&ColumnType> {
        child
            .columns
            .iter()
            .find(|c| c.name == name)
            .map(|c| &c.ty)
            .ok_or_else(|| {
                CatalogError::Invalid(format!("foreign key references unknown column: {name}"))
            })
    };
    let resolved = ForeignKeyDef::new(
        fk.columns.clone(),
        fk.parent_table.clone(),
        parent_columns,
        fk.on_delete,
        fk.on_update,
    )
    .with_options(fk.match_type(), fk.deferrability())
    .with_name(fk.name.clone());
    if resolved.references_parent_id() {
        for name in &resolved.columns {
            if !matches!(child_type(name)?, ColumnType::Integer | ColumnType::BigInt) {
                return Err(CatalogError::InvalidForeignKey(format!(
                    "column {name} must be INTEGER or BIGINT to reference id"
                )));
            }
        }
        // `id` 疑似列（物理キー）は常に非 NULL のため、`ON UPDATE CASCADE` の
        // 参照先 nullable 検査（(c)）は不要（`id` 自体は更新されず、A7 により
        // ON UPDATE アクションはそもそも発火しない）。
        return Ok(resolved);
    }
    let same_set = |key: &[String]| -> bool {
        key.len() == resolved.parent_columns.len()
            && key.iter().all(|k| resolved.parent_columns.contains(k))
    };
    let is_unique_target = parent.primary_key().is_some_and(&same_set)
        || parent
            .unique_constraints()
            .iter()
            .any(|u| same_set(u.columns()));
    if !is_unique_target {
        return Err(CatalogError::InvalidForeignKey(format!(
            "there is no unique constraint matching the referenced columns of table {}",
            parent.name
        )));
    }
    for (child_name, parent_name) in resolved.columns.iter().zip(&resolved.parent_columns) {
        let parent_ty = parent
            .columns
            .iter()
            .find(|c| &c.name == parent_name)
            .map(|c| &c.ty)
            .ok_or_else(|| {
                CatalogError::InvalidForeignKey(format!(
                    "referenced column {parent_name} does not exist"
                ))
            })?;
        // 型の同一性は型タグ＋パラメータ（`catalog_fields`。ENUM は型名を含む）で
        // 判定する。一意性検査と同じ正準キー（型タグ付き）で参照先を照合するため、
        // 型が異なる組は値が「等しく」見えても一致しない（黙って常に違反になる
        // 宣言を受理しない）。
        if child_type(child_name)?.catalog_fields() != parent_ty.catalog_fields() {
            return Err(CatalogError::InvalidForeignKey(format!(
                "foreign key column {child_name} and referenced column {parent_name} are of incompatible types"
            )));
        }
    }
    // 参照アクションの宣言時検査（Issue #1076 A8 (c)）: `ON UPDATE CASCADE` は
    // 参照先列の新しい値をそのまま参照元列へ書き込む。参照元列が `NOT NULL` なのに
    // 参照先列が nullable（NULL にできる UNIQUE 列）だと、参照先が NULL へ更新
    // された瞬間に必ず失敗する宣言になるため拒否する。
    if matches!(resolved.on_update, ReferentialAction::Cascade) {
        for (child_name, parent_name) in resolved.columns.iter().zip(&resolved.parent_columns) {
            let child_nullable = child
                .columns
                .iter()
                .find(|c| &c.name == child_name)
                .is_some_and(|c| c.nullable);
            let parent_nullable = parent
                .columns
                .iter()
                .find(|c| &c.name == parent_name)
                .is_some_and(|c| c.nullable);
            if !child_nullable && parent_nullable {
                return Err(CatalogError::InvalidForeignKey(format!(
                    "column {child_name} is NOT NULL but referenced column {parent_name} is nullable, which is incompatible with ON UPDATE CASCADE"
                )));
            }
        }
    }
    Ok(resolved)
}

/// `CHECK` 制約（[`CheckConstraint`]。TABLE-16・TASK-204、Issue #906）の不変条件
/// 検査: 件数上限・制約名の識別子妥当性・一意性、参照列名の識別子妥当性・
/// 生存列内での存在、述語テキストの長さ上限。述語の構文的妥当性
/// （`WherePredicate` としてのパース可能性・型検査）は catalog 層の管轄外
/// （`sql::check_constraint` が DDL 時・書き込み時の双方で担う）。
fn validate_check_constraints(schema: &TableSchema) -> Result<()> {
    if schema.checks.len() > MAX_CHECK_CONSTRAINTS_PER_TABLE {
        return Err(CatalogError::Invalid(format!(
            "too many CHECK constraints: {}",
            schema.checks.len()
        )));
    }
    let mut seen_names: Vec<&str> = Vec::with_capacity(schema.checks.len());
    for check in &schema.checks {
        validate_identifier(&check.name)?;
        if seen_names.contains(&check.name.as_str()) {
            return Err(CatalogError::Invalid(format!(
                "duplicate CHECK constraint name: {}",
                check.name
            )));
        }
        seen_names.push(check.name.as_str());
        if check.predicate_sql.is_empty() || check.predicate_sql.len() > MAX_CHECK_PREDICATE_SQL_LEN
        {
            return Err(CatalogError::Invalid(format!(
                "CHECK constraint {:?} predicate is empty or exceeds {} bytes",
                check.name, MAX_CHECK_PREDICATE_SQL_LEN
            )));
        }
        if check.columns.len() > MAX_CHECK_REFERENCED_COLUMNS {
            return Err(CatalogError::Invalid(format!(
                "CHECK constraint {:?} references too many columns",
                check.name
            )));
        }
        for column_name in &check.columns {
            validate_identifier(column_name)?;
            if !schema.columns.iter().any(|c| &c.name == column_name) {
                return Err(CatalogError::Invalid(format!(
                    "CHECK constraint {:?} references unknown column {:?}",
                    check.name, column_name
                )));
            }
        }
    }
    Ok(())
}

/// 制約名（UNIQUE・CHECK）の既定名接尾辞衝突解決の共通ヘルパー（設計 D2。
/// Issue #1067）。`candidate` が識別子として妥当（[`validate_identifier`]）かつ
/// `used` に未登録なら採用し、そうでなければ `_2`・`_3`… の接尾辞を試す
/// （PostgreSQL の暗黙制約名衝突解決に倣う）。識別子長超過等でどの接尾辞候補も
/// 妥当にならない場合は `<fallback_prefix><N>`（`N` は `used.len()` 起点の連番）へ
/// フォールバックする。`sql::check_constraint::resolve_unique_name`（CHECK の
/// 既定名。フォールバック接頭辞 `"check"`）と
/// [`derive_unique_constraint_names`]（UNIQUE の既定名。接頭辞 `"key"`）の
/// 両方から呼ばれる唯一の実装で、挙動を 1 か所に揃える。
pub(crate) fn resolve_constraint_name(
    candidate: &str,
    used: &[String],
    fallback_prefix: &str,
) -> String {
    if validate_identifier(candidate).is_ok() && !used.iter().any(|u| u == candidate) {
        return candidate.to_string();
    }
    for suffix in 2..=used.len() + 2 {
        let attempt = format!("{candidate}_{suffix}");
        if validate_identifier(&attempt).is_ok() && !used.iter().any(|u| u == &attempt) {
            return attempt;
        }
    }
    let mut fallback_index = used.len();
    loop {
        let attempt = format!("{fallback_prefix}{fallback_index}");
        if !used.iter().any(|u| u == &attempt) {
            return attempt;
        }
        fallback_index += 1;
    }
}

/// 名前未指定（宣言順）の UNIQUE 制約群へ既定名を割り当てる（設計 D2。
/// Issue #1067）。テーブル名・宣言順の列リスト・使用済み名集合（呼び出し元が
/// 渡す。CHECK 名 ∪ 明示 UNIQUE 名を想定）のみで決まる純関数。候補名は
/// `<table>_<col1>_<col2>..._key`（PostgreSQL 風）。衝突解決は
/// [`resolve_constraint_name`]（フォールバック接頭辞 `"key"`）に委譲する。
fn derive_unique_constraint_names(
    table: &str,
    columns_per_constraint: &[Vec<String>],
    used: &[String],
) -> Vec<String> {
    let mut used: Vec<String> = used.to_vec();
    let mut names = Vec::with_capacity(columns_per_constraint.len());
    for cols in columns_per_constraint {
        let candidate = format!("{table}_{}_key", cols.join("_"));
        let name = resolve_constraint_name(&candidate, &used, "key");
        used.push(name.clone());
        names.push(name);
    }
    names
}

/// スキーマの UNIQUE 制約のうち名前未確定（空名。[`UniqueConstraint::new`] で
/// 構築されたもの）に既定名を割り当てた新しい `TableSchema` を返す
/// （Issue #1067）。すでに実名を持つ制約（明示 `CONSTRAINT <name>` を伴う
/// `ALTER TABLE ADD`）はそのまま保持し、「使用済み」集合に加える。
///
/// 呼び出し元は [`Storage::create_table`]・
/// [`Storage::alter_table_add_named_unique_constraint`] の write トランザクション
/// 内で、`encode_schema`（内部の `validate_schema`）より前に必ず呼ぶこと
/// （TOCTOU 回避。FK の `parent_columns` 未解決中間表現と同じ流儀）。
fn assign_unique_constraint_names(schema: TableSchema) -> TableSchema {
    let mut used: Vec<String> = schema.checks.iter().map(|c| c.name.clone()).collect();
    // 明示 FK 名（TABLE-22・TASK-233、Issue #1069。設計 F1）を衝突回避の対象に
    // 加える。`encode_schema` は UNIQUE 名を FK 名より先に確定するため、この
    // 時点で見える FK 名は「まだ空文字列の既定名待ち」を除いた明示指定分のみ
    // （空文字列は素通しになるだけで害はない）。
    for fk in &schema.foreign_keys {
        if !fk.name.is_empty() {
            used.push(fk.name.clone());
        }
    }
    for uc in &schema.unique_constraints {
        if !uc.name.is_empty() {
            used.push(uc.name.clone());
        }
    }
    let table = schema.name.clone();
    let mut new_unique = Vec::with_capacity(schema.unique_constraints.len());
    for uc in schema.unique_constraints.clone() {
        if uc.name.is_empty() {
            let candidate = format!("{table}_{}_key", uc.columns.join("_"));
            let name = resolve_constraint_name(&candidate, &used, "key");
            used.push(name.clone());
            new_unique.push(UniqueConstraint::with_name(name, uc.columns));
        } else {
            new_unique.push(uc);
        }
    }
    schema.with_unique_constraints(new_unique)
}

/// 名前未指定（宣言順）の `FOREIGN KEY` 制約群へ既定名を割り当てる（設計 F2。
/// TABLE-22・TASK-233、Issue #1069）。テーブル名・宣言順の列リスト・使用済み
/// 名集合のみで決まる純関数。候補名は `<table>_<col1>_<col2>..._fkey`
/// （PostgreSQL 風）。衝突解決は [`resolve_constraint_name`]（フォールバック
/// 接頭辞 `"fkey"`）に委譲する（[`derive_unique_constraint_names`] と同じ形）。
fn derive_foreign_key_constraint_names(
    table: &str,
    columns_per_constraint: &[Vec<String>],
    used: &[String],
) -> Vec<String> {
    let mut used: Vec<String> = used.to_vec();
    let mut names = Vec::with_capacity(columns_per_constraint.len());
    for cols in columns_per_constraint {
        let candidate = format!("{table}_{}_fkey", cols.join("_"));
        let name = resolve_constraint_name(&candidate, &used, "fkey");
        used.push(name.clone());
        names.push(name);
    }
    names
}

/// スキーマの `FOREIGN KEY` 制約のうち名前未確定（空名）に既定名を割り当てた
/// 新しい `TableSchema` を返す（設計 F2。TABLE-22・TASK-233、Issue #1069。
/// [`assign_unique_constraint_names`] と同じ設計）。すでに実名を持つ制約
/// （明示 `CONSTRAINT <name> FOREIGN KEY` を伴う `CREATE TABLE`／
/// `ALTER TABLE ADD`）はそのまま保持する。
///
/// 呼び出し元は `encode_schema`（内部の `validate_schema` より前。TOCTOU
/// 回避）で `assign_unique_constraint_names` の**直後**に必ず呼ぶこと
/// （`used` に確定済みの UNIQUE 名を含めるため。encode_schema ドキュメント
/// 参照）。
fn assign_foreign_key_constraint_names(schema: TableSchema) -> TableSchema {
    let mut used: Vec<String> = schema.checks.iter().map(|c| c.name.clone()).collect();
    used.extend(
        schema
            .unique_constraints
            .iter()
            .map(|u| u.name().to_string()),
    );
    for fk in &schema.foreign_keys {
        if !fk.name.is_empty() {
            used.push(fk.name.clone());
        }
    }
    let table = schema.name.clone();
    let mut new_fks = Vec::with_capacity(schema.foreign_keys.len());
    for fk in schema.foreign_keys.clone() {
        if fk.name.is_empty() {
            let candidate = format!("{table}_{}_fkey", fk.columns.join("_"));
            let name = resolve_constraint_name(&candidate, &used, "fkey");
            used.push(name.clone());
            new_fks.push(fk.with_name(name));
        } else {
            new_fks.push(fk);
        }
    }
    schema.with_foreign_keys(new_fks)
}

/// UNIQUE 制約（[`UniqueConstraint`]。TABLE-16・TASK-204、Issue #905。
/// 制約名は Issue #1067）の不変条件検査。`create_table`・
/// `alter_table_add_unique_constraint`（追加後のスキーマ）・カタログ decode
/// （v6〜v9）のいずれからも `validate_schema` 経由で呼ばれる。
///
/// 検査項目（いずれも fail-closed・`CatalogError::Invalid`。主キーの
/// [`validate_primary_key`] と同じ分類）:
/// - 制約数が [`MAX_UNIQUE_CONSTRAINTS`] 以下
/// - 各制約の名前が非空・識別子として妥当（[`validate_identifier`]）
/// - 各制約が非空・[`MAX_UNIQUE_CONSTRAINT_COLUMNS`] 以下・制約内の列名重複なし
/// - 各列名が**生存列**に存在し、[`ColumnType::is_unique_constraint_allowed`]
///   （PK・FK が共有する許可リストの上位集合。Issue #1073）を満たす型
/// - 同一列リスト（宣言順そのままの比較）の制約が重複しない
///
/// UNIQUE 制約名と CHECK 制約名の名前空間共有（テーブル単位）の検査は
/// [`validate_schema`] が両方の検証後にまとめて行う。
///
/// 主キーと異なり NULL 許容列を参照できる（NULLS DISTINCT。
/// [`crate::constraint`] が NULL を含む行を当該制約の検査対象外とする）。
fn validate_unique_constraints(schema: &TableSchema) -> Result<()> {
    if schema.unique_constraints.len() > MAX_UNIQUE_CONSTRAINTS {
        return Err(CatalogError::Invalid(format!(
            "too many unique constraints: {}",
            schema.unique_constraints.len()
        )));
    }
    let mut seen_names: Vec<&str> = Vec::with_capacity(schema.unique_constraints.len());
    let mut seen_lists: Vec<&[String]> = Vec::with_capacity(schema.unique_constraints.len());
    for constraint in &schema.unique_constraints {
        if constraint.name.is_empty() {
            return Err(CatalogError::Invalid(
                "unique constraint name must not be empty".to_string(),
            ));
        }
        validate_identifier(&constraint.name)?;
        if seen_names.contains(&constraint.name.as_str()) {
            return Err(CatalogError::Invalid(format!(
                "duplicate unique constraint name: {}",
                constraint.name
            )));
        }
        seen_names.push(constraint.name.as_str());
        let columns = constraint.columns();
        if columns.is_empty() {
            return Err(CatalogError::Invalid(
                "unique constraint must reference at least one column".to_string(),
            ));
        }
        if columns.len() > MAX_UNIQUE_CONSTRAINT_COLUMNS {
            return Err(CatalogError::Invalid(format!(
                "unique constraint references too many columns: {}",
                columns.len()
            )));
        }
        let mut seen: Vec<&str> = Vec::with_capacity(columns.len());
        for name in columns {
            if seen.contains(&name.as_str()) {
                return Err(CatalogError::Invalid(format!(
                    "unique constraint references column {name} more than once"
                )));
            }
            seen.push(name.as_str());
            let column = schema
                .columns
                .iter()
                .find(|c| &c.name == name)
                .ok_or_else(|| {
                    CatalogError::Invalid(format!(
                        "unique constraint references unknown column: {name}"
                    ))
                })?;
            if !column.ty.is_unique_constraint_allowed() {
                return Err(CatalogError::Invalid(format!(
                    "column {name} has a type that cannot be used in a unique constraint"
                )));
            }
        }
        if seen_lists.contains(&columns) {
            return Err(CatalogError::Invalid(
                "duplicate unique constraint over the same column list".to_string(),
            ));
        }
        seen_lists.push(columns);
    }
    Ok(())
}

/// `PRIMARY KEY` 宣言（TABLE-16・TASK-204、Issue #903）の不変条件検査。
/// `create_table`・カタログ decode（[`decode_schema_body`]）の両方から
/// `validate_schema` 経由で呼ばれる。
///
/// 検査項目（いずれも fail-closed）:
/// - 非空・[`MAX_PRIMARY_KEY_COLUMNS`] 以下
/// - 列名の重複なし
/// - 各列名が**生存列**（`schema.columns`）に存在する（削除済み列・`id` 疑似列は
///   対象外。`id` は SQL 表層が `PRIMARY KEY (id)` を構文段階で正規化して除去する
///   ため、ここへ到達する `"id"` は常に「存在しない列」として拒否される）
/// - 各列が [`ColumnType::is_primary_key_allowed`] を満たす型
/// - 各列が `nullable == false`（呼び出し元が事前に `false` へ設定する契約。
///   本関数は黙って書き換えない）
fn validate_primary_key(schema: &TableSchema, pk_cols: &[String]) -> Result<()> {
    if pk_cols.is_empty() {
        return Err(CatalogError::Invalid(
            "PRIMARY KEY must declare at least one column".to_string(),
        ));
    }
    if pk_cols.len() > MAX_PRIMARY_KEY_COLUMNS {
        return Err(CatalogError::Invalid(format!(
            "too many primary key columns: {}",
            pk_cols.len()
        )));
    }
    let mut seen: Vec<&str> = Vec::with_capacity(pk_cols.len());
    for name in pk_cols {
        if seen.contains(&name.as_str()) {
            return Err(CatalogError::Invalid(format!(
                "duplicate primary key column: {name}"
            )));
        }
        seen.push(name.as_str());
        let column = schema
            .columns
            .iter()
            .find(|c| &c.name == name)
            .ok_or_else(|| {
                CatalogError::Invalid(format!("primary key references unknown column: {name}"))
            })?;
        if !column.ty.is_primary_key_allowed() {
            return Err(CatalogError::Invalid(format!(
                "column {name} has a type that cannot be a primary key column"
            )));
        }
        if column.nullable {
            return Err(CatalogError::Invalid(format!(
                "primary key column {name} must be declared non-nullable"
            )));
        }
    }
    Ok(())
}

/// 1 列ぶんのカタログ行（`name:tag:param:nullable\n`）を組み立てる。`param` は
/// [`validate_catalog_param`] を通してから連結する（encode 側の fail-closed。
/// TABLE-6・codex-review 指摘 PR #999。`catalog_fields` はここまで型定義側の
/// 自己申告であり、decode 側（`validate_catalog_param`・`from_catalog_fields`）が
/// 要求する文字集合・`:` 非混入を encode 側でも検証してから連結する。将来
/// `catalog_fields` が区切り文字や空文字を返す型を追加しても、ここで検知して
/// fail-closed に拒否し、デコード不能なカタログ値を永続化しない）。
/// [`encode_schema`] のループ本体であり、不正な `param` を直接与えて encode 側の
/// 拒否を固定する単体テストの seam も兼ねる。
fn encode_column_line(
    name: &str,
    type_name: &str,
    param_field: &str,
    nullable: bool,
) -> Result<String> {
    validate_catalog_param(param_field)?;
    let nullable_field = if nullable { "1" } else { "0" };
    Ok(format!(
        "{name}:{type_name}:{param_field}:{nullable_field}\n"
    ))
}

/// [`encode_column_line`] の v3 版（5 フィールド。`state` は `L`=生存／`D`=削除済み。
/// TABLE-19・TASK-203、Issue #901）。
fn encode_column_line_v3(
    name: &str,
    type_name: &str,
    param_field: &str,
    nullable: bool,
    state: char,
) -> Result<String> {
    validate_catalog_param(param_field)?;
    let nullable_field = if nullable { "1" } else { "0" };
    Ok(format!(
        "{name}:{type_name}:{param_field}:{nullable_field}:{state}\n"
    ))
}

/// [`encode_column_line_v3`] の v4 版（6 フィールド。末尾に `default`。
/// TABLE-16・TASK-204、Issue #904）。`default` は `None` を `-` として書く
/// （墓標行・`DEFAULT` 未宣言の生存列の両方がこの経路を通る）。
fn encode_column_line_v5(
    name: &str,
    type_name: &str,
    param_field: &str,
    nullable: bool,
    state: char,
    default: Option<&ColumnDefault>,
) -> Result<String> {
    validate_catalog_param(param_field)?;
    let nullable_field = if nullable { "1" } else { "0" };
    let default_field = match default {
        None => "-".to_string(),
        Some(d) => d.encode_catalog_field()?,
    };
    Ok(format!(
        "{name}:{type_name}:{param_field}:{nullable_field}:{state}:{default_field}\n"
    ))
}

/// `schema` のいずれかの `CHECK` 制約（TABLE-16・TASK-204、Issue #906）が列
/// `column_name` を参照しているか（`ALTER TABLE` の依存オブジェクト検査用）。
///
/// カタログに記録された依存列（`CheckConstraint::columns`）に加え、述語テキストから
/// 依存列を再計算して照合する（`sql::check_constraint::recompute_referenced_columns`。
/// 式述語内の `VECTOR` 列参照など、記録が欠けていた場合でも依存を取りこぼさない
/// 多層防御）。再計算に失敗した制約は「依存あり」とみなす（fail-closed。制約を
/// 黙って壊す `ALTER` を通さない）。
fn schema_check_references_column(schema: &TableSchema, column_name: &str) -> bool {
    schema.checks.iter().any(|c| {
        c.columns.iter().any(|col| col == column_name)
            || match crate::sql::check_constraint::recompute_referenced_columns(schema, c) {
                Ok(columns) => columns.iter().any(|col| col == column_name),
                Err(_) => true,
            }
    })
}

/// `CHECK` 制約セクション（`checks:<N>` 行 + N 行の
/// `check:<name>:<col1,col2,...>:<hex(predicate_sql)>`）を `out` へ追記する
/// （[`encode_schema`] の v7 分岐が呼ぶ。TABLE-16・TASK-204、Issue #906）。
/// 述語テキストはカタログの `:`／改行区切りと衝突しないよう小文字 16 進で
/// 符号化する（`DEFAULT` の `x` 形式と同じ [`hex_encode_byte`] を共有）。
/// 名前・列名は `validate_identifier`（[`validate_schema`] 経由で検証済み）に
/// より `:`／`,`／改行を含み得ない。
fn encode_check_section(out: &mut String, checks: &[CheckConstraint]) -> Result<()> {
    out.push_str(&format!("checks:{}\n", checks.len()));
    for check in checks {
        validate_identifier(&check.name)?;
        for column_name in &check.columns {
            validate_identifier(column_name)?;
        }
        out.push_str("check:");
        out.push_str(&check.name);
        out.push(':');
        out.push_str(&check.columns.join(","));
        out.push(':');
        for byte in check.predicate_sql.as_bytes() {
            out.push_str(&hex_encode_byte(*byte));
        }
        out.push('\n');
    }
    Ok(())
}

/// カタログ v7 の `CHECK` 制約セクション（[`encode_check_section`] の逆変換）を
/// 構造検証しつつ読み取る共有パーサー。[`decode_schema_body`] と軽量パーサー
/// [`catalog_value_references_enum_type`] の両方が使う（[`parse_unique_section`]
/// と同じく、両者の fail-closed 判定を 1 か所に揃える）。
///
/// 検証順序: `checks:` 行の存在・件数の数値形式・`1..=MAX_CHECK_CONSTRAINTS_PER_TABLE`
/// （0 件は「`CHECK` を持たないスキーマは v2〜v6 で書く」形式の一意性契約に
/// 反する）→ 各行の `check:` 接頭辞・フィールド数 → 識別子形状 → 参照列数上限
/// （`Vec` へ積む前）→ hex → 長さ上限 → UTF-8。参照列の実在・制約名の一意性は
/// 呼び出し元の [`validate_schema`] が判定する。確保は宣言件数（上限検査済み）の
/// 範囲に限る。エラーは呼び出し元が自身の分類へ包む文言のみを返す。
fn parse_check_section<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    allow_empty: bool,
) -> std::result::Result<Vec<CheckConstraint>, String> {
    let checks_line = lines
        .next()
        .ok_or_else(|| "catalog value truncated: missing checks line".to_string())?;
    let count_str = checks_line
        .strip_prefix("checks:")
        .ok_or_else(|| format!("malformed checks line: {checks_line:?}"))?;
    let count: usize = count_str
        .parse()
        .map_err(|_| format!("malformed check constraint count: {count_str:?}"))?;
    // `allow_empty` は v8（`FOREIGN KEY` を持つスキーマ。`CHECK` の有無とは独立。
    // TABLE-17・TASK-205、Issue #907）のみ `true`。v7 の 0 件は形式の一意性契約
    // 違反として拒否する。
    if count == 0 && !allow_empty {
        return Err("v7 catalog format requires at least one CHECK constraint".to_string());
    }
    if count > MAX_CHECK_CONSTRAINTS_PER_TABLE {
        return Err(format!("too many CHECK constraints: {count}"));
    }
    let mut checks = Vec::with_capacity(count);
    for _ in 0..count {
        let line = lines
            .next()
            .ok_or_else(|| "catalog value truncated: missing check line".to_string())?;
        let body = line
            .strip_prefix("check:")
            .ok_or_else(|| format!("malformed check line: {line:?}"))?;
        let mut fields = body.split(':');
        let (Some(name), Some(columns_field), Some(hex_predicate), None) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(format!("malformed check line: {line:?}"));
        };
        validate_identifier(name).map_err(|_| format!("malformed check line: {line:?}"))?;
        let mut columns: Vec<String> = Vec::new();
        if !columns_field.is_empty() {
            for (i, column_name) in columns_field.split(',').enumerate() {
                if i >= MAX_CHECK_REFERENCED_COLUMNS {
                    return Err(format!(
                        "CHECK constraint {name:?} references too many columns"
                    ));
                }
                validate_identifier(column_name)
                    .map_err(|_| format!("malformed check line: {line:?}"))?;
                columns.push(column_name.to_string());
            }
        }
        if hex_predicate.len() > MAX_CHECK_PREDICATE_SQL_LEN.saturating_mul(2) {
            return Err(format!(
                "CHECK constraint {name:?} predicate exceeds {MAX_CHECK_PREDICATE_SQL_LEN} bytes"
            ));
        }
        let predicate_bytes = hex_decode(hex_predicate)
            .map_err(|_| format!("CHECK constraint {name:?} predicate is not valid hex"))?;
        let predicate_sql = String::from_utf8(predicate_bytes)
            .map_err(|_| format!("CHECK constraint {name:?} predicate is not valid UTF-8"))?;
        // 空述語は encode 側が生成しない（`validate_check_constraints` と同じ
        // 不変条件）。`DROP TYPE` の依存判定が decode より緩くならないよう、
        // 共有パーサーで先に拒否する。
        if predicate_sql.is_empty() {
            return Err(format!("CHECK constraint {name:?} predicate is empty"));
        }
        checks.push(CheckConstraint {
            name: name.to_string(),
            columns,
            predicate_sql,
        });
    }
    Ok(checks)
}

/// `FOREIGN KEY` セクション（`fks:<N>` 行 + N 行の
/// `fk:<col1,col2,...>:<parent_table>:<pcol1,pcol2,...>`）を `out` へ追記する
/// （[`encode_schema`] の v8 分岐が呼ぶ。TABLE-17・TASK-205、Issue #907）。
/// 列名・テーブル名は `validate_identifier`（[`validate_schema`] 経由で検証済み）に
/// より `:`／`,`／改行を含み得ないため、区切り文字と衝突しない。
/// `include_options` は v9・v11（[`CATALOG_FORMAT_VERSION_V9`]・
/// [`CATALOG_FORMAT_VERSION_V11`]）選択時のみ `true` で、`fk:` 行を 7 フィールド
/// （参照アクション・`MATCH`・遅延属性を含む）で書く。v8・v10 は 3 フィールドの
/// まま（既存のバイト列を変えない。TABLE-17・TASK-205、Issue #1076／#1077）。
fn encode_foreign_key_section(
    out: &mut String,
    foreign_keys: &[ForeignKeyDef],
    include_options: bool,
) -> Result<()> {
    out.push_str(&format!("fks:{}\n", foreign_keys.len()));
    for fk in foreign_keys {
        validate_identifier(&fk.parent_table)?;
        for name in fk.columns.iter().chain(&fk.parent_columns) {
            validate_identifier(name)?;
        }
        out.push_str("fk:");
        out.push_str(&fk.columns.join(","));
        out.push(':');
        out.push_str(&fk.parent_table);
        out.push(':');
        out.push_str(&fk.parent_columns.join(","));
        // 参照アクション（Issue #1076）・`MATCH`／遅延属性（Issue #1077）は
        // すべて既定値（`NO ACTION`／`NO ACTION`／`SIMPLE`／`NOT DEFERRABLE`）の
        // 場合、v8 のバイト列を一切変えない 3 フィールド形のまま書く（既存
        // ゴールデンテスト・カタログの後方互換を保つ）。いずれか 1 つでも
        // 既定値以外の場合のみ 7 フィールド形（v9）に拡張する（正規形の一意性は
        // decode 側（[`parse_foreign_key_section`]）が「7 フィールドで全既定値」を
        // 拒否することで保つ）。
        if include_options {
            out.push(':');
            out.push_str(referential_action_token(fk.on_delete));
            out.push(':');
            out.push_str(referential_action_token(fk.on_update));
            out.push(':');
            out.push_str(match fk.match_type {
                ForeignKeyMatch::Simple => "simple",
                ForeignKeyMatch::Full => "full",
            });
            out.push(':');
            out.push_str(match fk.deferrability {
                ForeignKeyDeferrability::NotDeferrable => "immediate",
                ForeignKeyDeferrability::DeferrableInitiallyImmediate => "deferrable",
                ForeignKeyDeferrability::DeferrableInitiallyDeferred => "deferred",
            });
        }
        out.push('\n');
    }
    Ok(())
}

/// [`ReferentialAction`] のカタログ v8 `fk:` 行トークン（閉じた語彙）。
/// [`parse_referential_action_token`] と対を成す（往復を保証する多層防御。
/// Issue #1076）。
fn referential_action_token(action: ReferentialAction) -> &'static str {
    match action {
        ReferentialAction::NoAction => "noaction",
        ReferentialAction::Cascade => "cascade",
        ReferentialAction::SetNull => "setnull",
        ReferentialAction::SetDefault => "setdefault",
    }
}

/// [`referential_action_token`] の逆変換。語彙外のトークンは fail-closed に
/// `Err` とする（decode 経路・軽量パーサーの双方が使う共有パーサー）。
fn parse_referential_action_token(token: &str) -> std::result::Result<ReferentialAction, String> {
    match token {
        "noaction" => Ok(ReferentialAction::NoAction),
        "cascade" => Ok(ReferentialAction::Cascade),
        "setnull" => Ok(ReferentialAction::SetNull),
        "setdefault" => Ok(ReferentialAction::SetDefault),
        other => Err(format!(
            "malformed foreign key referential action: {other:?}"
        )),
    }
}

/// カンマ区切りの識別子リスト（`fk:` 行の列リスト）を構造検証しつつ読み取る。
/// 空要素・識別子違反・[`MAX_FOREIGN_KEY_COLUMNS`] 超過（`Vec` へ積む前に判定）は
/// `Err`。重複・実在の判定は呼び出し元（[`validate_foreign_keys`]）が担う。
fn parse_foreign_key_column_list(
    field: &str,
    line: &str,
) -> std::result::Result<Vec<String>, String> {
    let mut names: Vec<String> = Vec::new();
    for (i, name) in field.split(',').enumerate() {
        if i >= MAX_FOREIGN_KEY_COLUMNS {
            return Err(format!(
                "foreign key references too many columns: exceeds {MAX_FOREIGN_KEY_COLUMNS}"
            ));
        }
        if name.is_empty() {
            return Err(format!("malformed foreign key line: {line:?}"));
        }
        validate_identifier(name).map_err(|_| format!("malformed foreign key line: {line:?}"))?;
        names.push(name.to_string());
    }
    Ok(names)
}

/// カタログ v8 の `FOREIGN KEY` セクション（[`encode_foreign_key_section`] の
/// 逆変換）を構造検証しつつ読み取る共有パーサー。[`decode_schema_body`] と軽量
/// パーサー [`catalog_value_references_enum_type`] の両方が使う
/// （[`parse_unique_section`] と同じく、両者の fail-closed 判定を 1 か所に揃える）。
///
/// 検証順序: `fks:` 行の存在・件数の数値形式・`1..=MAX_FOREIGN_KEYS_PER_TABLE`
/// （0 件は「`FOREIGN KEY` を持たないスキーマは v2〜v7 で書く」形式の一意性契約に
/// 反する）→ 各行の `fk:` 接頭辞・フィールド数（`has_options` なら 7 または
/// （後方互換の）5、そうでなければ 3）→ 列リスト・テーブル名の識別子形状・
/// 参照アクション・`MATCH`／遅延属性トークン（TABLE-17・TASK-205、
/// Issue #1076／#1077）。`has_options` の 5 フィールド形は Issue #1077 時点の
/// v9（参照アクション追加前）が書いた既存カタログ値の読み取り専用互換パス
/// （codex-review 指摘対応。`parse_foreign_key_section` 本体のコメント参照）。
/// 参照元列の実在・参照先との照合は呼び出し元の [`validate_schema`] が判定する。
/// 確保は宣言件数（上限検査済み）の範囲に限る。`allow_empty` はカタログ v10
/// （[`CATALOG_FORMAT_VERSION_V10`]。UNIQUE の実名の有無で選ばれる形式。FK の
/// 有無とは独立。Issue #1067）選択時のみ `true`。`has_options` はカタログ v9・
/// v11（[`CATALOG_FORMAT_VERSION_V9`]・[`CATALOG_FORMAT_VERSION_V11`]）選択時
/// のみ `true`。
fn parse_foreign_key_section<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    allow_empty: bool,
    has_options: bool,
) -> std::result::Result<Vec<ForeignKeyDef>, String> {
    let fks_line = lines
        .next()
        .ok_or_else(|| "catalog value truncated: missing fks line".to_string())?;
    let count_str = fks_line
        .strip_prefix("fks:")
        .ok_or_else(|| format!("malformed fks line: {fks_line:?}"))?;
    let count: usize = count_str
        .parse()
        .map_err(|_| format!("malformed foreign key count: {count_str:?}"))?;
    // `allow_empty` は v10（UNIQUE の実名の有無で選ばれる形式。FK の有無とは
    // 独立。Issue #1067）のみ `true`。v8／v9／v11 の 0 件は形式の一意性契約違反
    // として拒否する。
    if count == 0 && !allow_empty {
        return Err(
            "v8/v9/v11 catalog format requires at least one FOREIGN KEY constraint".to_string(),
        );
    }
    if count > MAX_FOREIGN_KEYS_PER_TABLE {
        return Err(format!("too many FOREIGN KEY constraints: {count}"));
    }
    let mut foreign_keys = Vec::with_capacity(count);
    for _ in 0..count {
        let line = lines
            .next()
            .ok_or_else(|| "catalog value truncated: missing fk line".to_string())?;
        let body = line
            .strip_prefix("fk:")
            .ok_or_else(|| format!("malformed foreign key line: {line:?}"))?;
        let mut fields = body.split(':');
        let (Some(columns_field), Some(parent_table), Some(parent_columns_field)) =
            (fields.next(), fields.next(), fields.next())
        else {
            return Err(format!("malformed foreign key line: {line:?}"));
        };
        // 参照アクション・`MATCH`／遅延属性フィールド（v9 のみ。TABLE-17・
        // TASK-205、Issue #1076／#1077）。v8 は 3 フィールド固定のため、ここで
        // 余剰フィールドが無いことを検証する。
        //
        // v9 は `fk:` 行として 2 世代の形式を許容する（後方互換。Issue #1076・
        // codex-review 指摘対応）: (a) 旧形式（Issue #1077 時点の 5 フィールド。
        // `MATCH`／遅延属性のみで参照アクションは持たない）、(b) 新形式（本
        // Issue #1076 が参照アクションを追加した 7 フィールド）。バージョン
        // 文字列 `v9` を変えずに新形式へ拡張したため、旧形式のまま既に
        // 永続化済みのカタログ値（本 Issue のマージ前に作成された、既定以外の
        // `MATCH`／遅延属性を持つスキーマ）を新形式専用のパーサーで読むと
        // 構造検証エラーで拒否してしまう（fail-closed だが可用性を損なう）。
        // フィールド数で判別する: 5 番目のフィールドが有るかどうかで、新形式
        // （`on_delete`・`on_update` を含む）か旧形式（`MATCH`／遅延属性のみ、
        // 参照アクションは記録されておらず既定の `NoAction`）かが確定する。
        // `encode_foreign_key_section` は常に新形式（7 フィールド）でのみ書く
        // ため、旧形式は読み取り専用の互換パスであり正規形の一意性を損なわない。
        let (on_delete, on_update, match_type, deferrability) = if has_options {
            let field1 = fields
                .next()
                .ok_or_else(|| format!("malformed foreign key line: {line:?}"))?;
            let field2 = fields
                .next()
                .ok_or_else(|| format!("malformed foreign key line: {line:?}"))?;
            let (on_delete, on_update, match_field, deferral_field) = match fields.next() {
                Some(field3) => {
                    // 新形式（7 フィールド）: field1/field2 が on_delete/on_update。
                    let field4 = fields
                        .next()
                        .ok_or_else(|| format!("malformed foreign key line: {line:?}"))?;
                    if fields.next().is_some() {
                        return Err(format!("malformed foreign key line: {line:?}"));
                    }
                    (
                        parse_referential_action_token(field1)?,
                        parse_referential_action_token(field2)?,
                        field3,
                        field4,
                    )
                }
                None => {
                    // 旧形式（5 フィールド。Issue #1077 時点の v9）: field1/field2 が
                    // match/deferral。参照アクションは既定値へ補完する。
                    (
                        ReferentialAction::NoAction,
                        ReferentialAction::NoAction,
                        field1,
                        field2,
                    )
                }
            };
            let match_type = match match_field {
                "simple" => ForeignKeyMatch::Simple,
                "full" => ForeignKeyMatch::Full,
                other => return Err(format!("malformed foreign key MATCH field: {other:?}")),
            };
            let deferrability = match deferral_field {
                "immediate" => ForeignKeyDeferrability::NotDeferrable,
                "deferrable" => ForeignKeyDeferrability::DeferrableInitiallyImmediate,
                "deferred" => ForeignKeyDeferrability::DeferrableInitiallyDeferred,
                other => {
                    return Err(format!(
                        "malformed foreign key deferrability field: {other:?}"
                    ))
                }
            };
            (on_delete, on_update, match_type, deferrability)
        } else {
            if fields.next().is_some() {
                return Err(format!("malformed foreign key line: {line:?}"));
            }
            (
                ReferentialAction::NoAction,
                ReferentialAction::NoAction,
                ForeignKeyMatch::Simple,
                ForeignKeyDeferrability::NotDeferrable,
            )
        };
        let columns = parse_foreign_key_column_list(columns_field, line)?;
        validate_identifier(parent_table)
            .map_err(|_| format!("malformed foreign key line: {line:?}"))?;
        let parent_columns = parse_foreign_key_column_list(parent_columns_field, line)?;
        // 参照先を要さない構造上の不変条件（`validate_foreign_keys` と同じ契約）を
        // 共有パーサー側でも検証し、軽量パーサーが decode より緩くならないようにする。
        let has_duplicate = |names: &[String]| {
            names
                .iter()
                .enumerate()
                .any(|(i, n)| names.get(..i).is_some_and(|prev| prev.contains(n)))
        };
        if has_duplicate(&columns) || has_duplicate(&parent_columns) {
            return Err(format!(
                "foreign key references a column more than once: {line:?}"
            ));
        }
        if columns.len() != parent_columns.len() {
            return Err(format!("foreign key column count mismatch: {line:?}"));
        }
        let fk = ForeignKeyDef::new(
            columns,
            parent_table.to_string(),
            parent_columns,
            on_delete,
            on_update,
        )
        .with_options(match_type, deferrability);
        if fk
            .parent_columns
            .iter()
            .any(|c| c == FOREIGN_KEY_PARENT_ID_COLUMN)
            && !fk.references_parent_id()
        {
            return Err(format!(
                "the id column can only be referenced alone: {line:?}"
            ));
        }
        if foreign_keys
            .iter()
            .any(|other: &ForeignKeyDef| other.shares_reference_shape(&fk))
        {
            return Err("duplicate foreign key declaration".to_string());
        }
        foreign_keys.push(fk);
    }
    // 正規形の一意性（TABLE-17・TASK-205、Issue #1076／#1077）: v9／v11 選択の
    // 判断材料は「既定以外の参照アクション・MATCH・遅延属性を持つ FK が 1 件以上
    // ある」ことのみであり、全 FK が既定オプションのスキーマは v8／v10 のバイト
    // 列のまま書く契約（`encode_schema` 参照）。この不変条件に反する v9／v11
    // カタログ値（全既定値なのに v9／v11 で書かれた手書きデータ）を「依存なし」
    // 等へ丸めず
    // fail-closed に拒否する。
    if has_options
        && !foreign_keys.iter().any(|fk| {
            fk.on_delete() != ReferentialAction::NoAction
                || fk.on_update() != ReferentialAction::NoAction
                || fk.match_type() != ForeignKeyMatch::Simple
                || fk.deferrability() != ForeignKeyDeferrability::NotDeferrable
        })
    {
        return Err(
            "v9/v11 catalog format requires at least one FOREIGN KEY constraint with a non-default referential action, MATCH, or deferrability option"
                .to_string(),
        );
    }
    Ok(foreign_keys)
}

/// カタログ v12（[`CATALOG_FORMAT_VERSION_V12`]。設計 F4。TABLE-22・TASK-233、
/// Issue #1069）の `FOREIGN KEY` セクションを追記する。名前付き FK を永続化
/// するために新設する書き込み専用の実装で、v8〜v11 の 5/7 フィールド後方互換
/// 判別を持つ [`encode_foreign_key_section`] とは独立させる（名前フィールドの
/// 追加だけが差分のため、既存の後方互換ロジックを複雑化させない）。
fn encode_foreign_key_section_v12(out: &mut String, foreign_keys: &[ForeignKeyDef]) -> Result<()> {
    out.push_str(&format!("fks:{}\n", foreign_keys.len()));
    for fk in foreign_keys {
        validate_identifier(&fk.name)?;
        validate_identifier(&fk.parent_table)?;
        for name in fk.columns.iter().chain(&fk.parent_columns) {
            validate_identifier(name)?;
        }
        out.push_str("fk:");
        out.push_str(&fk.name);
        out.push(':');
        out.push_str(&fk.columns.join(","));
        out.push(':');
        out.push_str(&fk.parent_table);
        out.push(':');
        out.push_str(&fk.parent_columns.join(","));
        out.push(':');
        out.push_str(referential_action_token(fk.on_delete));
        out.push(':');
        out.push_str(referential_action_token(fk.on_update));
        out.push(':');
        out.push_str(match fk.match_type {
            ForeignKeyMatch::Simple => "simple",
            ForeignKeyMatch::Full => "full",
        });
        out.push(':');
        out.push_str(match fk.deferrability {
            ForeignKeyDeferrability::NotDeferrable => "immediate",
            ForeignKeyDeferrability::DeferrableInitiallyImmediate => "deferrable",
            ForeignKeyDeferrability::DeferrableInitiallyDeferred => "deferred",
        });
        out.push('\n');
    }
    Ok(())
}

/// [`encode_foreign_key_section_v12`] の逆変換（v12 専用。名前フィールドを
/// 持つ 8 フィールド固定の `fk:` 行）。`decode_schema_body` と軽量パーサー
/// [`catalog_value_references_enum_type`] の両方が使う。`count == 0` は形式の
/// 一意性契約違反として拒否する（v12 を選ぶのは名前付き FK を持つ場合のみの
/// ため）。
fn parse_foreign_key_section_v12<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
) -> std::result::Result<Vec<ForeignKeyDef>, String> {
    let fks_line = lines
        .next()
        .ok_or_else(|| "catalog value truncated: missing fks line".to_string())?;
    let count_str = fks_line
        .strip_prefix("fks:")
        .ok_or_else(|| format!("malformed fks line: {fks_line:?}"))?;
    let count: usize = count_str
        .parse()
        .map_err(|_| format!("malformed foreign key count: {count_str:?}"))?;
    if count == 0 {
        return Err("v12 catalog format requires at least one FOREIGN KEY constraint".to_string());
    }
    if count > MAX_FOREIGN_KEYS_PER_TABLE {
        return Err(format!("too many FOREIGN KEY constraints: {count}"));
    }
    let mut foreign_keys = Vec::with_capacity(count);
    let mut seen_names: Vec<String> = Vec::with_capacity(count);
    for _ in 0..count {
        let line = lines
            .next()
            .ok_or_else(|| "catalog value truncated: missing fk line".to_string())?;
        let body = line
            .strip_prefix("fk:")
            .ok_or_else(|| format!("malformed foreign key line: {line:?}"))?;
        let mut fields = body.split(':');
        let (
            Some(name),
            Some(columns_field),
            Some(parent_table),
            Some(parent_columns_field),
            Some(on_delete_field),
            Some(on_update_field),
            Some(match_field),
            Some(deferral_field),
        ) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        )
        else {
            return Err(format!("malformed foreign key line: {line:?}"));
        };
        if fields.next().is_some() {
            return Err(format!("malformed foreign key line: {line:?}"));
        }
        validate_identifier(name).map_err(|_| format!("malformed foreign key line: {line:?}"))?;
        if seen_names.iter().any(|n| n == name) {
            return Err(format!("duplicate foreign key constraint name: {name:?}"));
        }
        seen_names.push(name.to_string());
        let columns = parse_foreign_key_column_list(columns_field, line)?;
        validate_identifier(parent_table)
            .map_err(|_| format!("malformed foreign key line: {line:?}"))?;
        let parent_columns = parse_foreign_key_column_list(parent_columns_field, line)?;
        let has_duplicate = |names: &[String]| {
            names
                .iter()
                .enumerate()
                .any(|(i, n)| names.get(..i).is_some_and(|prev| prev.contains(n)))
        };
        if has_duplicate(&columns) || has_duplicate(&parent_columns) {
            return Err(format!(
                "foreign key references a column more than once: {line:?}"
            ));
        }
        if columns.len() != parent_columns.len() {
            return Err(format!("foreign key column count mismatch: {line:?}"));
        }
        let on_delete = parse_referential_action_token(on_delete_field)?;
        let on_update = parse_referential_action_token(on_update_field)?;
        let match_type = match match_field {
            "simple" => ForeignKeyMatch::Simple,
            "full" => ForeignKeyMatch::Full,
            other => return Err(format!("malformed foreign key MATCH field: {other:?}")),
        };
        let deferrability = match deferral_field {
            "immediate" => ForeignKeyDeferrability::NotDeferrable,
            "deferrable" => ForeignKeyDeferrability::DeferrableInitiallyImmediate,
            "deferred" => ForeignKeyDeferrability::DeferrableInitiallyDeferred,
            other => {
                return Err(format!(
                    "malformed foreign key deferrability field: {other:?}"
                ))
            }
        };
        let fk = ForeignKeyDef::new(
            columns,
            parent_table.to_string(),
            parent_columns,
            on_delete,
            on_update,
        )
        .with_options(match_type, deferrability)
        .with_name(name.to_string());
        if fk
            .parent_columns
            .iter()
            .any(|c| c == FOREIGN_KEY_PARENT_ID_COLUMN)
            && !fk.references_parent_id()
        {
            return Err(format!(
                "the id column can only be referenced alone: {line:?}"
            ));
        }
        if foreign_keys
            .iter()
            .any(|other: &ForeignKeyDef| other.shares_reference_shape(&fk))
        {
            return Err("duplicate foreign key declaration".to_string());
        }
        foreign_keys.push(fk);
    }
    Ok(foreign_keys)
}

/// [`TableSchema`] をカタログのテキスト形式へエンコードする。1 行目に
/// フォーマットバージョン、2 行目に列数、以降 1 行 1 列（`name:type:dim:nullable`
/// の 4 フィールドを `:` 区切り。識別子は `validate_identifier` により `:` を
/// 含み得ないため、区切り文字との衝突は起きない）。エンコード時にも
/// `validate_schema` を通し、不正なスキーマを永続化しない（fail-closed）。
///
/// バージョン選択: `CHECK` 制約を 1 つでも持つスキーマは（他の宣言の有無を
/// 問わず）v7 で書く（TABLE-16・TASK-204、Issue #906）。それ以外で
/// UNIQUE 制約を 1 つでも持つスキーマは（`PRIMARY KEY`・
/// `DEFAULT`・墓標の有無を問わず）v6 で書く（TABLE-16・TASK-204、Issue #905）。
/// それ以外で `DEFAULT` を 1 つでも持つスキーマは（`PRIMARY KEY`・墓標の
/// 有無を問わず）v5 で書く（TABLE-16・TASK-204、Issue #904）。`DEFAULT` を
/// 持たず `PRIMARY KEY` を持つスキーマは（墓標の有無を問わず）v4 で書く
/// （TABLE-16・TASK-204、Issue #903）。どちらも持たないスキーマは従来どおり
/// v2／v3（墓標の有無で選択）のまま、バイト列を変えない（既存ゴールデン
/// テストへ影響しない）。
fn encode_schema(schema: &TableSchema) -> Result<Vec<u8>> {
    // UNIQUE 制約の名前未確定（空名）分を確定する（設計 D2・Issue #1067）。
    // `encode_schema` は「スキーマ検証を集約する唯一の choke point」（本関数
    // ドキュメント参照）であるため、名前確定もここへ集約し、`Storage::
    // create_table`／`alter_table_add_named_unique_constraint` が確定済み
    // スキーマを渡す場合はもちろん、`encode_schema` を直接呼ぶ単体テスト・
    // 将来の呼び出し元が空名の `UniqueConstraint::new` を渡す場合も一貫して
    // 動く（冪等: 既に実名を持つ制約には作用しない）。
    let schema = assign_unique_constraint_names(schema.clone());
    // `FOREIGN KEY` の名前未確定（空名）分を確定する（設計 F2・Issue #1069）。
    // UNIQUE 名の確定より後に呼ぶ（`used` に確定済みの UNIQUE 名を含めるため。
    // `assign_foreign_key_constraint_names` ドキュメント参照）。
    let schema = assign_foreign_key_constraint_names(schema);
    let schema = &schema;
    validate_schema(schema)?;
    let has_default = schema.columns.iter().any(|c| c.default.is_some());
    let has_unique = !schema.unique_constraints.is_empty();
    let has_check = !schema.checks.is_empty();
    let has_fk = !schema.foreign_keys.is_empty();
    // v10／v11 選択条件（Issue #1067）: 実名が既定名導出と 1 つでも食い違う
    // UNIQUE 制約を持つスキーマのみ v10（以上）で書く。名前を明示しない
    // `CREATE TABLE`・`ALTER TABLE ADD UNIQUE` は常に既定名と一致するため
    // v6〜v9 のバイト列は変わらない（既存ゴールデンテスト不変。**v9 は #1077 で
    // 既にリリース済みの意味を持つため、UNIQUE の実名を理由に v9 を再定義しない**。
    // codex-review／Cursor Bugbot 指摘・Issue #1147）。
    let has_named_unique = has_unique && {
        let columns_per_constraint: Vec<Vec<String>> = schema
            .unique_constraints
            .iter()
            .map(|c| c.columns().to_vec())
            .collect();
        let check_names: Vec<String> = schema.checks.iter().map(|c| c.name.clone()).collect();
        let derived =
            derive_unique_constraint_names(&schema.name, &columns_per_constraint, &check_names);
        schema
            .unique_constraints
            .iter()
            .zip(derived.iter())
            .any(|(uc, derived_name)| uc.name() != derived_name.as_str())
    };
    // v9 選択の判断材料（TABLE-17・TASK-205、Issue #1076／#1077）: 既定以外の
    // 参照アクション（`ON DELETE`／`ON UPDATE` が `NO ACTION` 以外）・`MATCH`
    // （`Full`）・遅延属性（`NotDeferrable` 以外）のいずれかを持つ `FOREIGN KEY`
    // が 1 件でもあるか。全 FK が既定オプションのスキーマは（`has_fk` であっても）
    // 引き続き v8 のバイト列のまま書く（正規形の一意性。既存ゴールデンテストへ
    // 影響しない）。
    let has_fk_options = schema.foreign_keys.iter().any(|fk| {
        fk.on_delete() != ReferentialAction::NoAction
            || fk.on_update() != ReferentialAction::NoAction
            || fk.match_type() != ForeignKeyMatch::Simple
            || fk.deferrability() != ForeignKeyDeferrability::NotDeferrable
    });
    // v12 選択条件（設計 F4・Issue #1069）: 実名が既定名導出と 1 つでも食い違う
    // `FOREIGN KEY` を持つスキーマのみ v12 で書く。名前を明示しない
    // `CREATE TABLE`・`ALTER TABLE ADD FOREIGN KEY` は常に既定名と一致するため
    // v8〜v11 のバイト列は変わらない（既存ゴールデンテスト不変）。
    let has_named_fk = has_fk && {
        let columns_per_constraint: Vec<Vec<String>> = schema
            .foreign_keys
            .iter()
            .map(|f| f.columns().to_vec())
            .collect();
        let mut used: Vec<String> = schema.checks.iter().map(|c| c.name.clone()).collect();
        used.extend(
            schema
                .unique_constraints
                .iter()
                .map(|u| u.name().to_string()),
        );
        let derived =
            derive_foreign_key_constraint_names(&schema.name, &columns_per_constraint, &used);
        schema
            .foreign_keys
            .iter()
            .zip(derived.iter())
            .any(|(fk, derived_name)| fk.name() != derived_name.as_str())
    };
    let mut out = String::new();
    if has_named_fk {
        // カタログ v12（設計 F4。TABLE-22・TASK-233、Issue #1069）: v11 の
        // 上位集合で、`uniq:`／`checks:` セクションは 0 件を許し（この枝へ来る
        // 判断材料は FK の実名のみで UNIQUE・CHECK の有無とは独立なため）、
        // `fks:` セクションのみ名前付き 8 フィールドの `fk:` 行
        // （[`encode_foreign_key_section_v12`]）で書く。
        out.push_str(CATALOG_FORMAT_VERSION_V12);
        out.push('\n');
        out.push_str(&format!("cols:{}\n", schema.physical_slot_count()));
        let pk_field = schema
            .primary_key
            .as_ref()
            .map(|cols| cols.join(","))
            .unwrap_or_default();
        out.push_str(&format!("pk:{pk_field}\n"));
        for slot in schema.physical_slots() {
            match slot {
                PhysicalSlot::Live(_, column) => {
                    let (type_name, param_field) = column.ty.catalog_fields();
                    out.push_str(&encode_column_line_v5(
                        &column.name,
                        type_name,
                        &param_field,
                        column.nullable,
                        'L',
                        column.default.as_ref(),
                    )?);
                }
                PhysicalSlot::Dropped(dropped) => {
                    let (type_name, param_field) = dropped.ty().catalog_fields();
                    out.push_str(&encode_column_line_v5(
                        dropped.name(),
                        type_name,
                        &param_field,
                        true,
                        'D',
                        None,
                    )?);
                }
            }
        }
        out.push_str(&format!("uniq:{}\n", schema.unique_constraints.len()));
        for constraint in &schema.unique_constraints {
            out.push_str("U:");
            out.push_str(constraint.name());
            out.push(':');
            out.push_str(&constraint.columns().join(","));
            out.push('\n');
        }
        encode_check_section(&mut out, &schema.checks)?;
        encode_foreign_key_section_v12(&mut out, &schema.foreign_keys)?;
    } else if has_named_unique {
        // カタログ v10／v11（TABLE-16/17・TASK-204/205、Issue #1067・#1077）:
        // v8 の上位集合で `uniq:` セクションの `U:` 行が `U:<name>:<cols>` の
        // 形になる。この枝に入るのは UNIQUE の実名を持つ場合のみ（`has_fk_options`
        // だけを理由にここへは入らない。**v9 は #1077 でリリース済みの意味を
        // 保つため、`has_named_unique` を伴わない `has_fk_options` は下の
        // フォールバック枝で従来どおり v9 のまま書く**。Issue #1147）。
        // `FOREIGN KEY` が既定以外の `MATCH`・遅延属性を 1 件でも持つ場合のみ
        // v11（[`CATALOG_FORMAT_VERSION_V11`]。`fk:` 行を 7 フィールドへ拡張。
        // 5 フィールド形は Issue #1077 時点の旧 v9 バイト列を読むための読み取り
        // 専用の後方互換形であり、書き込みには使わない。codex-review P2 指摘・
        // PR #1147）で書き、それ以外は v10 のまま（[`CATALOG_FORMAT_VERSION_V10`]）。
        out.push_str(if has_fk_options {
            CATALOG_FORMAT_VERSION_V11
        } else {
            CATALOG_FORMAT_VERSION_V10
        });
        out.push('\n');
        out.push_str(&format!("cols:{}\n", schema.physical_slot_count()));
        let pk_field = schema
            .primary_key
            .as_ref()
            .map(|cols| cols.join(","))
            .unwrap_or_default();
        out.push_str(&format!("pk:{pk_field}\n"));
        for slot in schema.physical_slots() {
            match slot {
                PhysicalSlot::Live(_, column) => {
                    let (type_name, param_field) = column.ty.catalog_fields();
                    out.push_str(&encode_column_line_v5(
                        &column.name,
                        type_name,
                        &param_field,
                        column.nullable,
                        'L',
                        column.default.as_ref(),
                    )?);
                }
                PhysicalSlot::Dropped(dropped) => {
                    let (type_name, param_field) = dropped.ty().catalog_fields();
                    out.push_str(&encode_column_line_v5(
                        dropped.name(),
                        type_name,
                        &param_field,
                        true,
                        'D',
                        None,
                    )?);
                }
            }
        }
        out.push_str(&format!("uniq:{}\n", schema.unique_constraints.len()));
        for constraint in &schema.unique_constraints {
            out.push_str("U:");
            out.push_str(constraint.name());
            out.push(':');
            out.push_str(&constraint.columns().join(","));
            out.push('\n');
        }
        encode_check_section(&mut out, &schema.checks)?;
        encode_foreign_key_section(&mut out, &schema.foreign_keys, has_fk_options)?;
    } else if has_default || has_unique || has_check || has_fk {
        // UNIQUE 制約を持つスキーマは v6、それ以外で `DEFAULT` を持つスキーマは
        // v5 で書く（`PRIMARY KEY` の有無に関わらず）。v6 は v5 と同じ本体
        // （`pk:` 行・6 フィールドの列行）の後ろに `uniq:` セクションを追記
        // するだけの上位集合（TABLE-16・TASK-204、Issue #905）。`pk:` 行は `cols:` 行の直後、列行より前に置き、主キー
        // 宣言が無ければ空のまま書く（`decode_schema_body` はこれを「主キー
        // なし」と解釈する。v4 の `pk:` 行は非空必須のまま変えない）。
        // 識別子は `validate_identifier`（`validate_schema` が経由済み）により
        // `,` を含み得ないため、`,` 区切りとの衝突は起きない。
        // `CHECK` 制約を持つスキーマは v7（TABLE-16・TASK-204、Issue #906）。
        // v7 は v6 と同じ本体・`uniq:` セクション（0 件可）の後ろに `checks:`
        // セクションを追記するだけの上位集合。
        // `FOREIGN KEY` を持つスキーマは v8（TABLE-17・TASK-205、Issue #907）。
        // v8 は v7 と同じ本体・`uniq:`／`checks:` セクション（いずれも 0 件可）の
        // 後ろに `fks:` セクションを追記するだけの上位集合。既定以外の参照
        // アクション・`MATCH`・遅延属性を 1 件でも持つスキーマは v9
        // （[`CATALOG_FORMAT_VERSION_V9`]。TABLE-17・TASK-205、Issue #1076／
        // #1077）——`fks:` セクションの `fk:` 行のみ 7 フィールドへ拡張する
        // 上位集合。この枝は `!has_named_unique` の場合のみ到達するため
        // （UNIQUE の実名を持つ場合は上の v10／v11 分岐で処理済み）、
        // `has_fk_options` だけを理由にここへ来た場合も v9 のバイト列は
        // #1077／#1076 のまま変えない（Issue #1147）。
        out.push_str(if has_fk_options {
            CATALOG_FORMAT_VERSION_V9
        } else if has_fk {
            CATALOG_FORMAT_VERSION_V8
        } else if has_check {
            CATALOG_FORMAT_VERSION_V7
        } else if has_unique {
            CATALOG_FORMAT_VERSION_V6
        } else {
            CATALOG_FORMAT_VERSION_V5
        });
        out.push('\n');
        out.push_str(&format!("cols:{}\n", schema.physical_slot_count()));
        let pk_field = schema
            .primary_key
            .as_ref()
            .map(|cols| cols.join(","))
            .unwrap_or_default();
        out.push_str(&format!("pk:{pk_field}\n"));
        for slot in schema.physical_slots() {
            match slot {
                PhysicalSlot::Live(_, column) => {
                    let (type_name, param_field) = column.ty.catalog_fields();
                    out.push_str(&encode_column_line_v5(
                        &column.name,
                        type_name,
                        &param_field,
                        column.nullable,
                        'L',
                        column.default.as_ref(),
                    )?);
                }
                PhysicalSlot::Dropped(dropped) => {
                    let (type_name, param_field) = dropped.ty().catalog_fields();
                    out.push_str(&encode_column_line_v5(
                        dropped.name(),
                        type_name,
                        &param_field,
                        true,
                        'D',
                        None,
                    )?);
                }
            }
        }
        if has_unique || has_check || has_fk {
            // 識別子は `validate_identifier`（列名は `validate_schema` 経由で
            // 生存列名と一致することを検証済み）により `,`／`:`／改行を含み
            // 得ないため、カンマ区切りで連結しても区切り文字と衝突しない。
            out.push_str(&format!("uniq:{}\n", schema.unique_constraints.len()));
            for constraint in &schema.unique_constraints {
                out.push_str("U:");
                out.push_str(&constraint.columns().join(","));
                out.push('\n');
            }
        }
        if has_check || has_fk {
            encode_check_section(&mut out, &schema.checks)?;
        }
        if has_fk {
            encode_foreign_key_section(&mut out, &schema.foreign_keys, has_fk_options)?;
        }
    } else if let Some(pk_cols) = &schema.primary_key {
        // 主キーを宣言したが DEFAULT は持たないスキーマは（墓標の有無に
        // 関わらず）常に v4 で書く（TABLE-16・TASK-204、Issue #903）。`pk:`
        // 行は `cols:` 行の直後、列行より前に置く。
        out.push_str(CATALOG_FORMAT_VERSION_V4);
        out.push('\n');
        out.push_str(&format!("cols:{}\n", schema.physical_slot_count()));
        out.push_str(&format!("pk:{}\n", pk_cols.join(",")));
        for slot in schema.physical_slots() {
            match slot {
                PhysicalSlot::Live(_, column) => {
                    let (type_name, param_field) = column.ty.catalog_fields();
                    out.push_str(&encode_column_line_v3(
                        &column.name,
                        type_name,
                        &param_field,
                        column.nullable,
                        'L',
                    )?);
                }
                PhysicalSlot::Dropped(dropped) => {
                    let (type_name, param_field) = dropped.ty().catalog_fields();
                    out.push_str(&encode_column_line_v3(
                        dropped.name(),
                        type_name,
                        &param_field,
                        true,
                        'D',
                    )?);
                }
            }
        }
    } else if schema.dropped_slots().is_empty() {
        // 主キー・DEFAULT・墓標のいずれも持たないスキーマは常に v2 で書く
        // （バイト列不変。既存のゴールデンテストに影響しない。TABLE-19・
        // Issue #901）。
        out.push_str(CATALOG_FORMAT_VERSION_LINE);
        out.push('\n');
        out.push_str(&format!("cols:{}\n", schema.columns.len()));
        for column in &schema.columns {
            // 型タグ・`param` の往復は ColumnType::catalog_fields に集約する
            // （Issue #880 D2。型を追加する際にここを個別に触らずに済む）。
            let (type_name, param_field) = column.ty.catalog_fields();
            out.push_str(&encode_column_line(
                &column.name,
                type_name,
                &param_field,
                column.nullable,
            )?);
        }
    } else {
        // 墓標が 1 つでもあるが DEFAULT を持たないスキーマは v3 で書く。
        // 物理位置の昇順（[`TableSchema::physical_slots`]）で生存列・墓標を
        // 交互に列挙する。
        out.push_str(CATALOG_FORMAT_VERSION_V3);
        out.push('\n');
        out.push_str(&format!("cols:{}\n", schema.physical_slot_count()));
        for slot in schema.physical_slots() {
            match slot {
                PhysicalSlot::Live(_, column) => {
                    let (type_name, param_field) = column.ty.catalog_fields();
                    out.push_str(&encode_column_line_v3(
                        &column.name,
                        type_name,
                        &param_field,
                        column.nullable,
                        'L',
                    )?);
                }
                PhysicalSlot::Dropped(dropped) => {
                    let (type_name, param_field) = dropped.ty().catalog_fields();
                    out.push_str(&encode_column_line_v3(
                        dropped.name(),
                        type_name,
                        &param_field,
                        true,
                        'D',
                    )?);
                }
            }
        }
    }
    if out.len() > MAX_CATALOG_VALUE_LEN {
        return Err(CatalogError::Invalid(format!(
            "encoded catalog value too large: {} bytes",
            out.len()
        )));
    }
    Ok(out.into_bytes())
}

/// カタログのテキスト形式から [`TableSchema`] をデコードする。`table_name` は
/// redb のキー（呼び出し元が既知）から渡され、値バイト列には含まれない。
/// 欠落フィールド・余剰フィールド・未知バージョン・不正 UTF-8・不正次元・
/// 切り詰め・識別子違反はすべて `Err`（黙殺フォールバックしない。TABLE-6）。
///
/// 返すエラーは常に [`CatalogError::CorruptSchema`]（[`decode_schema_body`] が
/// 内部で使う `validate_identifier`・`validate_vector_dim`・`validate_schema` は
/// 汎用の `CatalogError::Invalid` を返すため、ここで格納済みデータのデコード失敗
/// として明示的に読み替える）。呼び出し元（[`TableLookup for Storage`](Storage)）は
/// この変換を前提に `Invalid`（ユーザー入力の識別子形式不正）と区別して wire_code を
/// 割り当てる（Issue #55 レビュー指摘）。ENUM 列を含まないカタログ値専用の
/// 簡易ラッパー（[`no_enum_resolver`]）であり、単体テスト（`#[cfg(test)]`）
/// 専用。production 経路は [`decode_schema_with_resolver`] を txn 由来の
/// リゾルバ付きで直接呼ぶ。
#[cfg(test)]
fn decode_schema(table_name: &str, bytes: &[u8]) -> Result<TableSchema> {
    decode_schema_with_resolver(table_name, bytes, &mut no_enum_resolver)
}

/// [`decode_schema`] の一般化版。ENUM 列（[`ColumnType::Enum`]）を含むテーブルは、
/// カタログに型名しか持たないため、デコード時に `resolve_enum` を通じて
/// [`ENUM_TYPES_TABLE`] から語彙を解決する必要がある。production の 2 呼び出し口
/// （[`get_table_schema_in_txn`]・[`Storage::alter_table_add_column`]）はいずれも
/// 既に read/write トランザクションを保持しているため、そこから
/// [`ENUM_TYPES_TABLE`] を開くクロージャを渡す。ENUM 列を持たないテーブルの
/// デコードでは `resolve_enum` は一度も呼ばれない。
fn decode_schema_with_resolver(
    table_name: &str,
    bytes: &[u8],
    resolve_enum: &mut dyn FnMut(&str) -> Result<Arc<EnumTypeDef>>,
) -> Result<TableSchema> {
    decode_schema_body(table_name, bytes, resolve_enum).map_err(|e| match e {
        // `FOREIGN KEY` の自己参照照合（`validate_foreign_keys`）が返す
        // `InvalidForeignKey` も、格納済み値の破損として同じく読み替える
        // （TABLE-17・TASK-205、Issue #907）。
        CatalogError::Invalid(msg) | CatalogError::InvalidForeignKey(msg) => {
            CatalogError::CorruptSchema(msg)
        }
        other => other,
    })
}

fn decode_schema_body(
    table_name: &str,
    bytes: &[u8],
    resolve_enum: &mut dyn FnMut(&str) -> Result<Arc<EnumTypeDef>>,
) -> Result<TableSchema> {
    if bytes.len() > MAX_CATALOG_VALUE_LEN {
        return Err(CatalogError::Invalid(format!(
            "catalog value too large: {} bytes",
            bytes.len()
        )));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| CatalogError::Invalid("catalog value is not valid UTF-8".to_string()))?;

    let mut lines = text.split('\n');

    let version_line = lines
        .next()
        .ok_or_else(|| CatalogError::Invalid("catalog value is empty".to_string()))?;
    // v3（TABLE-19・Issue #901）は 1 行あたり 5 フィールド（末尾に `state`
    // `L`／`D`）を持つ以外は v2 と同じ枠組みを共有する。v4（TABLE-16・
    // TASK-204、Issue #903）はさらに `cols:` 行の直後に `pk:` 行を 1 行持つ
    // （非空必須）。v5（TABLE-16・TASK-204、Issue #904）は v4 の上位集合で、
    // `pk:` 行は主キー宣言が無ければ空を許し（`pk:` のみの行で「主キー
    // なし」を表す）、列行は 6 番目に `default` フィールドを必須で持つ。
    // `cols:` は物理スロット総数（v2 では常に生存列数と一致）を表す。
    // v6（TABLE-16・TASK-204、Issue #905）は v5 と同じ本体の後ろ（列行の
    // 直後）に `uniq:` セクションを持つ。
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum FormatVersion {
        V2,
        V3,
        V4,
        V5,
        V6,
        V7,
        V8,
        V9,
        V10,
        V11,
        V12,
    }
    let format_version = match version_line {
        CATALOG_FORMAT_VERSION_LINE => FormatVersion::V2,
        CATALOG_FORMAT_VERSION_V3 => FormatVersion::V3,
        CATALOG_FORMAT_VERSION_V4 => FormatVersion::V4,
        CATALOG_FORMAT_VERSION_V5 => FormatVersion::V5,
        CATALOG_FORMAT_VERSION_V6 => FormatVersion::V6,
        CATALOG_FORMAT_VERSION_V7 => FormatVersion::V7,
        CATALOG_FORMAT_VERSION_V8 => FormatVersion::V8,
        CATALOG_FORMAT_VERSION_V9 => FormatVersion::V9,
        CATALOG_FORMAT_VERSION_V10 => FormatVersion::V10,
        CATALOG_FORMAT_VERSION_V11 => FormatVersion::V11,
        CATALOG_FORMAT_VERSION_V12 => FormatVersion::V12,
        other => {
            return Err(CatalogError::Invalid(format!(
                "unknown catalog format version: {other:?}"
            )))
        }
    };
    let has_state_field = format_version != FormatVersion::V2;
    // v7（TABLE-16・TASK-204、Issue #906）は v6 の上位集合（`pk:` 行・6 フィールド
    // 列行・`uniq:` セクション〔0 件可〕の後ろに `checks:` セクション）。
    // v8（TABLE-17・TASK-205、Issue #907）は v7 の上位集合（`checks:` セクション
    // 〔0 件可〕の後ろに `fks:` セクション）。v9（TABLE-17・TASK-205、
    // Issue #1076／#1077）は v8 の上位集合（`fks:` セクションの `fk:` 行のみ
    // 7 フィールドへ拡張）。v10（Issue #1067）は v8 の上位集合（`U:` 行に制約名を
    // 持つ点のみ差分。v9 とは独立）。v11（Issue #1067・#1076／#1077 の統合）は
    // v10 の上位集合（`fks:` セクションの `fk:` 行のみ 7 フィールドへ拡張）。
    let has_default_field = matches!(
        format_version,
        FormatVersion::V5
            | FormatVersion::V6
            | FormatVersion::V7
            | FormatVersion::V8
            | FormatVersion::V9
            | FormatVersion::V10
            | FormatVersion::V11
            | FormatVersion::V12
    );
    // `pk:` 行を持つのは v4〜v12（v4 は非空必須、v5〜v12 は空を「主キー
    // なし」として許容する）。
    let has_pk_line = matches!(
        format_version,
        FormatVersion::V4
            | FormatVersion::V5
            | FormatVersion::V6
            | FormatVersion::V7
            | FormatVersion::V8
            | FormatVersion::V9
            | FormatVersion::V10
            | FormatVersion::V11
            | FormatVersion::V12
    );

    let cols_line = lines.next().ok_or_else(|| {
        CatalogError::Invalid("catalog value truncated: missing cols line".to_string())
    })?;
    let count_str = cols_line
        .strip_prefix("cols:")
        .ok_or_else(|| CatalogError::Invalid(format!("malformed cols line: {cols_line:?}")))?;
    let slot_count: usize = count_str
        .parse()
        .map_err(|_| CatalogError::Invalid(format!("malformed column count: {count_str:?}")))?;
    if slot_count > MAX_COLUMN_COUNT {
        return Err(CatalogError::Invalid(format!(
            "too many columns: {slot_count}"
        )));
    }

    // v4／v5 専用: `pk:` 行を `cols:` 行の直後・列行より前に置く（TABLE-16・
    // TASK-204、Issue #903／#904）。要素数は `Vec` を確保する前に
    // `MAX_PRIMARY_KEY_COLUMNS` で上限検証する（untrusted 入力の扱い）。v4 は
    // 非空必須（`primary_key.is_some()` 自体が v4 か v2/v3 かを分ける唯一の
    // 判断材料）、v5 は空を「主キーなし」として許容する（DEFAULT の有無が
    // v5 選択の判断材料であり、主キーの有無とは独立なため）。
    let primary_key: Option<Vec<String>> = if has_pk_line {
        let pk_line = lines.next().ok_or_else(|| {
            CatalogError::Invalid("catalog value truncated: missing pk line".to_string())
        })?;
        let pk_body = pk_line
            .strip_prefix("pk:")
            .ok_or_else(|| CatalogError::Invalid(format!("malformed pk line: {pk_line:?}")))?;
        if pk_body.is_empty() {
            if format_version == FormatVersion::V4 {
                return Err(CatalogError::Invalid(
                    "v4 catalog format requires at least one primary key column".to_string(),
                ));
            }
            None
        } else {
            let mut cols: Vec<String> = Vec::new();
            for (i, name) in pk_body.split(',').enumerate() {
                if i >= MAX_PRIMARY_KEY_COLUMNS {
                    return Err(CatalogError::Invalid(format!(
                        "too many primary key columns: exceeds {MAX_PRIMARY_KEY_COLUMNS}"
                    )));
                }
                if name.is_empty() {
                    return Err(CatalogError::Invalid(format!(
                        "malformed pk line: {pk_line:?}"
                    )));
                }
                validate_identifier(name)?;
                cols.push(name.to_string());
            }
            Some(cols)
        }
    } else {
        None
    };

    // 残り行を、宣言スロット数（slot_count。上で MAX_COLUMN_COUNT 以下と検証済み）を
    // 超えない範囲でのみ構築する。旧実装の `lines.collect()` は残り行数が
    // 宣言列数と無関係に無制限へ膨らむ攻撃入力（大量の短い行）に対して行数比例の
    // アロケーションを先に行っていたが（.claude/rules/coding-rust.md「untrusted
    // 入力の扱い」）、本実装は `slot_count` 件目までしか構築しない単一走査へ変更し、
    // 未知の型タグ・不正な `param` はその列を構築する前に拒否する（Issue #880 D7）。
    // `slot_count` を超える行は「末尾の空行（トレーリング改行）1 行のみ」を
    // 許容し、それ以外は余剰行として拒否する。
    //
    // 列行はちょうど `slot_count` 行だけ読む（v6 はその直後に `uniq:`
    // セクションが続くため、残り行の扱いは列行・`uniq:` セクションを読み
    // 終えた後で一括して判定する。TABLE-16・TASK-204、Issue #905）。行が
    // 不足した場合は後続の件数照合で拒否する。
    let mut columns = Vec::with_capacity(slot_count);
    let mut dropped: Vec<DroppedSlot> = Vec::new();
    for line_index in 0..slot_count {
        let Some(line) = lines.next() else {
            break;
        };

        let mut fields = line.split(':');
        let name = fields
            .next()
            .ok_or_else(|| CatalogError::Invalid(format!("malformed column line: {line:?}")))?;
        let type_name = fields
            .next()
            .ok_or_else(|| CatalogError::Invalid(format!("malformed column line: {line:?}")))?;
        let param_field = fields
            .next()
            .ok_or_else(|| CatalogError::Invalid(format!("malformed column line: {line:?}")))?;
        let nullable_field = fields
            .next()
            .ok_or_else(|| CatalogError::Invalid(format!("malformed column line: {line:?}")))?;
        // v2 は 4 フィールド固定（state 相当は常に「生存」）。v3／v4 は
        // 5 番目に `state` フィールドを必須で持ち、v5 はさらに 6 番目に
        // `default` を必須で持つ。
        let state_field =
            if has_state_field {
                Some(fields.next().ok_or_else(|| {
                    CatalogError::Invalid(format!("malformed column line: {line:?}"))
                })?)
            } else {
                None
            };
        let default_field =
            if has_default_field {
                Some(fields.next().ok_or_else(|| {
                    CatalogError::Invalid(format!("malformed column line: {line:?}"))
                })?)
            } else {
                None
            };
        if fields.next().is_some() {
            return Err(CatalogError::Invalid(format!(
                "malformed column line: {line:?}"
            )));
        }

        validate_identifier(name)?;
        validate_catalog_param(param_field)?;
        // 未知の型タグ・型別の `param` 文法違反は、ここで `ColumnDef`（`name` の
        // 所有 `String` を確保する）を構築する前に拒否する（アロケーション前拒否。
        // Issue #880 D7）。
        let ty = ColumnType::from_catalog_fields(type_name, param_field, resolve_enum)?;

        let nullable = match nullable_field {
            "0" => false,
            "1" => true,
            other => {
                return Err(CatalogError::Invalid(format!(
                    "malformed nullable field: {other:?}"
                )))
            }
        };

        match state_field {
            None | Some("L") => {
                let mut column = ColumnDef::new(name, ty, nullable);
                if let Some(field) = default_field {
                    // decode 側は「fail-closed（不正な default はスキーマ自体を
                    // 読み込み不能にする）」方針。`ColumnDefault::compatible_with`
                    // による型整合は後続の `validate_schema` が担う。
                    column.default = ColumnDefault::decode_catalog_field(field)?;
                }
                columns.push(column);
            }
            Some("D") => {
                // 墓標は default を持たない（encode 側は常に `-` を書く。
                // 手書きの不正データによる持ち込みも fail-closed に拒否する）。
                if let Some(field) = default_field {
                    if field != "-" {
                        return Err(CatalogError::Invalid(format!(
                            "dropped column {name:?} must not declare a DEFAULT"
                        )));
                    }
                }
                // 墓標の型は必ずフレーム等価型（TABLE-19 D1・D2）でなければ
                // ならない。手書きの不正データが `VECTOR`／`ENUM`／`JSON`／
                // `JSONB` を削除済み状態で持ち込むのを拒否する。
                if matches!(
                    ty,
                    ColumnType::Vector(_)
                        | ColumnType::Enum(_)
                        | ColumnType::Json
                        | ColumnType::Jsonb
                ) {
                    return Err(CatalogError::Invalid(format!(
                        "dropped column {name:?} has a non frame-equivalent type"
                    )));
                }
                let physical_index = u16::try_from(line_index).map_err(|_| {
                    CatalogError::Invalid("dropped column physical index overflow".to_string())
                })?;
                dropped.push(DroppedSlot {
                    physical_index,
                    name: name.to_string(),
                    ty,
                });
            }
            Some(other) => {
                return Err(CatalogError::Invalid(format!(
                    "malformed column state field: {other:?}"
                )))
            }
        }
    }
    if columns.len() + dropped.len() != slot_count {
        return Err(CatalogError::Invalid(format!(
            "catalog value line count mismatch: expected {slot_count} columns, got {} lines",
            columns.len() + dropped.len()
        )));
    }
    // v3（墓標を持つが主キー・DEFAULT を持たない）なのに墓標 0 件は形式の
    // 一意性に反する（同一スキーマが 2 通りにエンコードされ得る状態を
    // 許さない。TABLE-19 D2）。v4／v5（主キー・DEFAULT のいずれかを持つ）は
    // 墓標が 0 件でも一意な唯一のエンコードであるため、この不変条件の対象外と
    // する（`format_version` 自体が v4/v5 か v2/v3 かを分ける唯一の判断材料
    // であり、`pk:` 行の必須非空〔v4〕・DEFAULT 0 件拒否〔v5、下記〕は
    // それぞれ別途検証済み）。
    if format_version == FormatVersion::V3 && dropped.is_empty() {
        return Err(CatalogError::Invalid(
            "v3 catalog format requires at least one dropped column".to_string(),
        ));
    }
    // v5 なのに DEFAULT 0 件は形式の一意性に反する（同一スキーマが v2／v3／v4
    // と v5 の 2 通りにエンコードされ得る状態を許さない。TABLE-16・TASK-204、
    // Issue #904。TABLE-19 D2 と同じ設計判断）。
    if format_version == FormatVersion::V5 && !columns.iter().any(|c| c.default.is_some()) {
        return Err(CatalogError::Invalid(
            "v5 catalog format requires at least one column with a DEFAULT".to_string(),
        ));
    }

    // v6 専用の `uniq:` セクション（TABLE-16・TASK-204、Issue #905）。構造
    // （件数・`U:` 行・識別子形状・制約内重複・同一列リスト重複）は共有
    // パーサー [`parse_unique_section`] が検証し、参照列の実在・型適格性は
    // 後続の `validate_schema`（[`validate_unique_constraints`]）が担う。
    // v7／v8 も同じ `uniq:` セクションを持つが、UNIQUE 制約 0 件を許容する（v7 の
    // 選択材料は `CHECK` の有無であり UNIQUE の有無とは独立なため。TABLE-16・
    // TASK-204、Issue #906）。v9（Issue #1077）は `FOREIGN KEY` のオプションの
    // 有無で選ばれる形式であり UNIQUE の有無とは独立なため、v7／v8 と同じく
    // 0 件を許容する（`named = false`。**v9 の意味は既にリリース済みであり
    // UNIQUE の実名は持たない**。Issue #1147）。v10（Issue #1067）は `U:` 行に
    // 制約名を持ち（`named = true`）、v6 と同じく 0 件を許容しない。v11
    // （Issue #1067・#1077 の統合）も同じく `named = true`・0 件不可。名前の無い
    // v6〜v9 値は decode 後に既定名を導出する（[`assign_unique_constraint_names`]）。
    let unique_constraints: Vec<UniqueConstraint> = match format_version {
        FormatVersion::V6 | FormatVersion::V7 | FormatVersion::V8 | FormatVersion::V9 => {
            parse_unique_section(&mut lines, format_version != FormatVersion::V6, false)
                .map_err(CatalogError::Invalid)?
                .into_iter()
                .map(|(_, cols)| UniqueConstraint::new(cols))
                .collect()
        }
        FormatVersion::V10 | FormatVersion::V11 => parse_unique_section(&mut lines, false, true)
            .map_err(CatalogError::Invalid)?
            .into_iter()
            .map(|(name, cols)| UniqueConstraint::with_name(name.unwrap_or_default(), cols))
            .collect(),
        // v12（設計 F4・Issue #1069）は名前付き FK の有無のみで選ばれる形式で
        // あり UNIQUE の有無とは独立なため、0 件を許容する（`allow_empty = true`）。
        FormatVersion::V12 => parse_unique_section(&mut lines, true, true)
            .map_err(CatalogError::Invalid)?
            .into_iter()
            .map(|(name, cols)| UniqueConstraint::with_name(name.unwrap_or_default(), cols))
            .collect(),
        _ => Vec::new(),
    };
    // v7 専用の `checks:` セクション（TABLE-16・TASK-204、Issue #906）。構造は
    // 共有パーサー [`parse_check_section`] が検証し、参照列の実在・制約名の
    // 一意性は後続の `validate_schema` が担う。
    let checks: Vec<CheckConstraint> = match format_version {
        FormatVersion::V7
        | FormatVersion::V8
        | FormatVersion::V9
        | FormatVersion::V10
        | FormatVersion::V11
        | FormatVersion::V12 => {
            parse_check_section(&mut lines, format_version != FormatVersion::V7)
                .map_err(CatalogError::Invalid)?
        }
        _ => Vec::new(),
    };
    // v8 専用の `fks:` セクション（TABLE-17・TASK-205、Issue #907）。構造は共有
    // パーサー [`parse_foreign_key_section`] が検証し、参照元列の実在・自己参照の
    // 照合は後続の `validate_schema`（[`validate_foreign_keys`]）が担う。v9
    // （Issue #1077）は `MATCH`／遅延属性オプションを持つことが選択理由のため
    // `k >= 1` 必須のまま、`fk:` 行を 5 フィールド（`has_options = true`）で読む。
    // v10（Issue #1067）は UNIQUE の実名の有無で選ばれる形式であり FK の有無とは
    // 独立なため、`k == 0` を許容する（v8 は `k >= 1` 必須のまま変えない）。v11
    // （Issue #1067・#1077 の統合）は v9 と同じくオプション付き FK を必須とする。
    let foreign_keys: Vec<ForeignKeyDef> = match format_version {
        FormatVersion::V8 => {
            parse_foreign_key_section(&mut lines, false, false).map_err(CatalogError::Invalid)?
        }
        FormatVersion::V9 => {
            parse_foreign_key_section(&mut lines, false, true).map_err(CatalogError::Invalid)?
        }
        FormatVersion::V10 => {
            parse_foreign_key_section(&mut lines, true, false).map_err(CatalogError::Invalid)?
        }
        FormatVersion::V11 => {
            parse_foreign_key_section(&mut lines, false, true).map_err(CatalogError::Invalid)?
        }
        FormatVersion::V12 => {
            parse_foreign_key_section_v12(&mut lines).map_err(CatalogError::Invalid)?
        }
        _ => Vec::new(),
    };

    // 宣言スロット数（v6 は `uniq:` セクションも）を超える残り行は、「末尾の
    // 空行（トレーリング改行）1 行のみ」を許容し、それ以外は余剰行として
    // 拒否する。
    let mut trailing_seen = false;
    for line in lines {
        if trailing_seen || !line.is_empty() {
            return Err(CatalogError::Invalid(format!(
                "catalog value line count mismatch: expected {slot_count} columns, got more than {slot_count} lines"
            )));
        }
        trailing_seen = true;
    }

    let schema = TableSchema::from_parts(
        table_name,
        columns,
        dropped,
        primary_key,
        unique_constraints,
    )
    .with_checks(checks)
    .with_foreign_keys(foreign_keys);
    // v6〜v9（名前の無い UNIQUE 制約）は decode 時に既定名を導出する
    // （設計 D2・Issue #1067）。v10／v11 はすでに実名を持つため素通しする
    // （`assign_unique_constraint_names` は空名の制約にのみ作用するため
    // 呼んでも安全だが、意図を明示するため分岐する）。
    let schema = if matches!(
        format_version,
        FormatVersion::V10 | FormatVersion::V11 | FormatVersion::V12
    ) {
        schema
    } else {
        assign_unique_constraint_names(schema)
    };
    // v8〜v11（名前の無い FOREIGN KEY）は decode 時に既定名を導出する（設計
    // F3・Issue #1069）。v12 はすでに実名を持つため素通しする
    // （`assign_foreign_key_constraint_names` は空名の制約にのみ作用するため
    // 呼んでも安全だが、意図を明示するため分岐する）。
    let schema = if format_version == FormatVersion::V12 {
        schema
    } else {
        assign_foreign_key_constraint_names(schema)
    };
    // デコード結果を再度検証する（列数上限・列名重複・識別子・墓標の不変条件）。
    // 手書きの不正データがフィールドごとの検証をすり抜けても、スキーマ全体の
    // 不変条件はここで担保する。
    validate_schema(&schema)?;
    Ok(schema)
}

/// [`parse_unique_section`] が返す 1 制約ぶんの中間表現（制約名〔`named` が
/// `false` のときは常に `None`〕・列リスト）。呼び出し元ごとに異なる型を
/// 書かないための共有エイリアス（clippy `type_complexity` 対応）。
type ParsedUniqueConstraints = Vec<(Option<String>, Vec<String>)>;

/// カタログ v6 の `uniq:` セクション（`uniq:<n>` 行と `n` 個の
/// `U:<col>[,<col>]*` 行。TABLE-16・TASK-204、Issue #905）を構造検証しつつ
/// 読み取る共有パーサー。[`decode_schema_body`] と軽量パーサー
/// [`catalog_value_references_enum_type`] の両方が使い、両者の fail-closed
/// 判定を 1 か所に揃える（片方だけが緩いと、`decode_schema_body` が拒否する
/// 壊れた値を `DROP TYPE` の依存判定だけが「依存なし」に丸めてしまう）。
///
/// 検証項目: `uniq:` 行の存在・件数の数値形式・`1..=MAX_UNIQUE_CONSTRAINTS`
/// （0 件は「UNIQUE 制約を持たないスキーマは v2〜v5 で書く」形式の一意性
/// 契約に反する。ただし v7／v8／v9〔`CHECK`／`FOREIGN KEY` を持つスキーマ〕は
/// UNIQUE の有無と独立に選ばれるため、呼び出し元が `allow_empty = true` を渡して
/// `0..=MAX_UNIQUE_CONSTRAINTS` を許容する）・各 `U:` 行の接頭辞・空要素なし・要素数
/// `MAX_UNIQUE_CONSTRAINT_COLUMNS` 以下（`Vec` へ積む前に判定）・識別子形状・
/// 制約内の列名重複なし・同一列リストの制約重複なし。`named = true`（v10／v11。
/// Issue #1067）のときは `U:` 行が `U:<name>:<cols>` の形になり、制約名の
/// 識別子形状・制約名同士の重複も追加で検証する（CHECK 名との重複は列行の
/// 集合を要さないため、呼び出し元が `validate_schema` で追加検証する）。
/// 参照列の実在・型適格性は呼び出し元が判定する（列行の集合が必要なため）。
/// エラーは呼び出し元が自身の分類（`Invalid`／`CorruptSchema`）へ包む文言のみを
/// 返す。
fn parse_unique_section<'a>(
    lines: &mut impl Iterator<Item = &'a str>,
    allow_empty: bool,
    named: bool,
) -> std::result::Result<ParsedUniqueConstraints, String> {
    let uniq_line = lines
        .next()
        .ok_or_else(|| "catalog value truncated: missing uniq line".to_string())?;
    let count_str = uniq_line
        .strip_prefix("uniq:")
        .ok_or_else(|| format!("malformed uniq line: {uniq_line:?}"))?;
    let count: usize = count_str
        .parse()
        .map_err(|_| format!("malformed unique constraint count: {count_str:?}"))?;
    // `allow_empty` は v7／v8／v9（`CHECK`／`FOREIGN KEY` を持つスキーマ。UNIQUE
    // の有無とは独立）のみ `true`。v6／v10／v11 の 0 件は形式の一意性契約違反
    // として拒否する。
    if count == 0 && !allow_empty {
        return Err("catalog format requires at least one unique constraint".to_string());
    }
    if count > MAX_UNIQUE_CONSTRAINTS {
        return Err(format!("too many unique constraints: {count}"));
    }
    let mut constraints: ParsedUniqueConstraints = Vec::with_capacity(count);
    let mut seen_names: Vec<String> = Vec::with_capacity(count);
    for _ in 0..count {
        let line = lines
            .next()
            .ok_or_else(|| "catalog value truncated: missing unique constraint line".to_string())?;
        let body = line
            .strip_prefix("U:")
            .ok_or_else(|| format!("malformed unique constraint line: {line:?}"))?;
        let (name, cols_body) = if named {
            let (name, rest) = body
                .split_once(':')
                .ok_or_else(|| format!("malformed unique constraint line: {line:?}"))?;
            if name.is_empty() {
                return Err(format!("malformed unique constraint line: {line:?}"));
            }
            validate_identifier(name)
                .map_err(|_| format!("malformed unique constraint line: {line:?}"))?;
            if seen_names.iter().any(|n| n == name) {
                return Err(format!("duplicate unique constraint name: {name:?}"));
            }
            seen_names.push(name.to_string());
            (Some(name.to_string()), rest)
        } else {
            (None, body)
        };
        let mut names: Vec<String> = Vec::new();
        for (i, col_name) in cols_body.split(',').enumerate() {
            if i >= MAX_UNIQUE_CONSTRAINT_COLUMNS {
                return Err(format!(
                    "unique constraint references too many columns: exceeds {MAX_UNIQUE_CONSTRAINT_COLUMNS}"
                ));
            }
            if col_name.is_empty() {
                return Err(format!("malformed unique constraint line: {line:?}"));
            }
            validate_identifier(col_name)
                .map_err(|_| format!("malformed unique constraint line: {line:?}"))?;
            if names.iter().any(|n| n == col_name) {
                return Err(format!(
                    "unique constraint references column {col_name:?} more than once"
                ));
            }
            names.push(col_name.to_string());
        }
        if constraints.iter().any(|(_, cols)| cols == &names) {
            return Err("duplicate unique constraint over the same column list".to_string());
        }
        constraints.push((name, names));
    }
    Ok(constraints)
}

/// ユーザーテーブル `table_name` に対応する行ストア用の動的 redb テーブル名を組み立てる
/// （TASK-146・EXT-2）。`validate_identifier` が `/` を含む文字列を許容しないため、
/// 固定テーブル（`rows`／`catalog`）ともユーザーテーブル同士とも名前衝突しない。
/// 呼び出し元は必ず先に `validate_identifier(table_name)` を通してから呼ぶこと
/// （本関数自身は検証を行わない）。
///
/// この動的テーブルは [`CATALOG_TABLE`] のエントリとは別ライフサイクルで管理されている
/// （`create_table` は `CATALOG_TABLE` のみ書き込み、本関数が指す行テーブルは初回挿入まで
/// 未作成のまま）。[`Storage::drop_table`] が `CATALOG_TABLE` のエントリ削除と同一 write
/// トランザクション内で本関数が返す行テーブルも削除する。残留を許すとテーブル再作成時に
/// 旧次元の行データが残り、EXT-2 の次元固定の不変条件を静かに破るため（Issue #179・
/// PR #151 レビュー据え置き事項）。
///
/// `pub(crate)` で公開する: `arena.rs`（TASK-87、対象ビヘイビア: TABLE-8）が
/// コールドスタート・アリーナ構築時に、対象テーブルの行テーブルだけを単一の
/// `read_txn` 上で直接開くために必要（クレート外へは公開しない）。
pub(crate) fn user_rows_table_name(table_name: &str) -> String {
    format!("user_rows/{table_name}")
}

/// 行ストア（`user_rows/{table_name}`）の物理キー型（対象ビヘイビア: TABLE-12・RLS-9。
/// ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-12・`rls.md` RLS-9）。
///
/// キーはサーバー側導出テナント（`policy.rs::PolicyContext::tenant_id`）と行 `id` の
/// 複合キーで、行 `id` の一意性スコープをテナント内に閉じる。異なるテナントは同一の
/// `id` を独立に保持でき、`crate::tenant::insert_row` の重複検出は自テナントの
/// 名前空間内だけを見るため、他テナント行の存在有無が `23505` の有無として観測される
/// 経路が構造的に消える（codex-review P0 指摘・PR #194）。`redb` のタプルキーは
/// 要素順（tenant_id 昇順 → id 昇順）で全順序を持つため、全件走査は従来どおり
/// 単一の range 走査で列挙できる。
///
/// `storage.rs::RowStoreTableDef` へのエイリアス（Issue #206。旧 `rows` テーブル
/// （`storage.rs::ROWS_TABLE`）とテーブル名だけが異なる同一契約のため、キー型定義を
/// `storage.rs` 側へ一元化しドリフトを防ぐ）。
pub(crate) type UserRowsTableDef<'a> = crate::storage::RowStoreTableDef<'a>;

/// [`Storage::scan_table_page`] のページングカーソル（行ストアの物理キーと同形の
/// `(tenant_id, id)`。対象ビヘイビア: TABLE-12。`id` 単独では再開位置を表現できない）。
///
/// `storage.rs::RowCursor` の re-export（生成点は `storage.rs` に一元化。Issue #206）。
pub use crate::storage::RowCursor;

/// [`Storage::scan_table_page`] の戻り値（1 ページ分の行と、続きがある場合の
/// [`RowCursor`]）。
pub type RowPage = (Vec<StorageRow>, Option<RowCursor>);

/// 行テーブル定義を組み立てる（[`UserRowsTableDef`] の唯一の生成点）。
/// 呼び出し元（`catalog.rs`・`tenant.rs`・`arena.rs`・`rls.rs`）がキー型を各所で
/// 書き下すとドリフトするため、ここへ集約する。
pub(crate) fn user_rows_table_def(row_table_name: &str) -> UserRowsTableDef<'_> {
    TableDefinition::new(row_table_name)
}

/// 永続一意索引（TABLE-16・TASK-204、Issue #1070）のテーブル名を組み立てる。
/// `user_rows/{table_name}`（[`user_rows_table_name`]）と同じ命名規則を共有し、
/// `validate_identifier` を通った `table_name` は `/` を含まないため既存の
/// 固定テーブル・行テーブル・他のユーザーテーブルの索引テーブルのいずれとも
/// 衝突しない。呼び出し元は本関数を呼ぶ前に必ず `validate_identifier` を
/// 通すこと（本関数自身は検証を行わない。[`user_rows_table_name`] と同じ契約）。
///
/// `PRIMARY KEY`・`UNIQUE` のいずれも宣言しないテーブルではこのテーブルは
/// 生成されない（`constraint::enforce_unique_keys_in_txn` が検査対象キーが
/// 0 個の場合に即座に成功する既存の契約を維持する。索引テーブルは初回の
/// 一意キー検査まで物理的に未作成）。
pub(crate) fn user_uniq_table_name(table_name: &str) -> String {
    format!("user_uniq/{table_name}")
}

/// 永続一意索引テーブルの物理キー型。第 1 要素はサーバー側導出テナント
/// （`ctx.tenant_id()` 由来。RLS-9・TABLE-12 と同じ名前空間化）、第 2 要素は
/// `constraint::unique_index` が組み立てる名前空間化済みサブキー（マーカー・
/// 正引き・逆引きの 3 種）。値は名前空間ごとに異なるバイト列（`constraint::
/// unique_index` の各エンコード関数のドキュメント参照）。
pub(crate) type UniqueIndexTableDef<'a> =
    TableDefinition<'a, (&'static str, &'static [u8]), &'static [u8]>;

/// 永続一意索引テーブル定義を組み立てる（[`UniqueIndexTableDef`] の唯一の
/// 生成点。[`user_rows_table_def`] と同じ集約方針）。
pub(crate) fn user_uniq_table_def(index_table_name: &str) -> UniqueIndexTableDef<'_> {
    TableDefinition::new(index_table_name)
}

/// 行テーブル `open_table` のエラー写像（[`UserRowsTableDef`] 専用）。
///
/// 旧フォーマット（物理キーが `id` のみ）の DB を開くと `redb` は
/// `TableError::TableTypeMismatch` を返す。これを黙って握りつぶすと旧行を
/// 別テナントの行として扱う fail-open になりうるため、
/// [`CatalogError::IncompatibleRowKeyFormat`] へ明示的に写像して拒否する
/// （マイグレーションは提供しない。TABLE-12 の物理キー変更に伴う恒久契約）。
pub(crate) fn map_row_table_error(e: redb::TableError) -> CatalogError {
    match e {
        redb::TableError::TableTypeMismatch { .. } => CatalogError::IncompatibleRowKeyFormat,
        other => CatalogError::from(other),
    }
}

/// `storage.rs::StorageError` を `CatalogError` へ明示変換する。`CatalogError` は
/// `redb::Error` への blanket `From` 実装を持つため（`storage.rs` の設計メモと同じ
/// coherence 制約）、`redb::Error` そのものではない複合エラー型 `StorageError` からの
/// 変換はここで個別に定義する。
pub(crate) fn convert_storage_error(e: StorageError) -> CatalogError {
    match e {
        StorageError::Backend(err) => CatalogError::Backend(err),
        StorageError::Codec(msg) => CatalogError::Invalid(msg),
        StorageError::NotFound(id) => CatalogError::RowNotFound(id),
        // `StorageError::ScanLimitExceeded` は `Storage::scan`（無制限走査）と
        // `Storage::scan_batch_log`（バッチ台帳）の 2 経路で共有される単一 variant
        // （Issue #131・PR #193 codex レビュー PRRT_kwDOUAKASM6cCITT 対応。バッチ台帳専用の
        // variant を新設する案は公開 enum への破壊的変更にあたるとして差し戻された。詳細は
        // `storage.rs::StorageError::ScanLimitExceeded` のドキュメンテーションコメント参照）。
        // `convert_storage_error` はカタログ層（テーブルスコープの `scan_table_page`）
        // からのみ呼ばれ `Storage::scan_batch_log` を経由しないため、ここでは
        // 「呼び出し元が自分の経路を知っている（= 内部コンテキスト）」という前提の下、
        // テーブルスコープの正確な代替手段 `scan_table_page` を案内してよい。
        // なお `scan_table_page` は `MAX_SCAN_PAGE_LIMIT` で事前にクランプしているため
        // 通常この分岐自体には到達しない（`StorageError` の網羅性のためにここで扱う）。
        StorageError::ScanLimitExceeded => {
            CatalogError::Invalid("scan limit exceeded: use scan_table_page".to_string())
        }
        // `log_batch`（バッチ台帳）専用のエラーだが、カタログ層は行テーブル
        // （`user_rows_table_name`）しか扱わずバッチ台帳を経由しない。到達しない
        // 分岐だが `StorageError` の網羅性のためここでも扱い、Invalid へ一般化する。
        StorageError::DuplicateBatchSeq(seq) => {
            CatalogError::Invalid(format!("duplicate batch seq: seq={seq}"))
        }
        // `WriteTxn` の内部カウンタ（バッチ台帳専用）のエラーで、カタログ層の
        // 行テーブル操作からは到達しない。`StorageError` の網羅性のためここでも扱う。
        StorageError::PendingRowCountOverflow => {
            CatalogError::Invalid("pending row count overflow".to_string())
        }
        // `log_batch`/`commit` の未台帳行チェック・空バッチ拒否も同様にバッチ台帳
        // 専用のエラーで、カタログ層（`db().begin_write()` による生 redb トランザクション
        // 経由）からは到達しない。`StorageError` の網羅性のためここでも扱う。
        StorageError::UnloggedRows(count) => {
            CatalogError::Invalid(format!("unlogged rows before commit: count={count}"))
        }
        StorageError::EmptyBatch => {
            CatalogError::Invalid("empty batch: no rows put since last log_batch".to_string())
        }
        // `bump_generation_and_commit`（TASK-133 P1 対応）はカタログ層の DDL/DML commit
        // からも呼ばれるため到達しうる。u64 の枯渇は現実的に起こらないが網羅性のため扱う。
        StorageError::GenerationCounterOverflow => {
            CatalogError::Invalid("storage generation counter overflow".to_string())
        }
        // カタログ層は `catalog.rs::ROWS_TABLE`（`user_rows/{table}`）を経由し
        // `storage.rs::ROWS_TABLE`（旧 `rows` テーブル）は経由しないため通常到達
        // しないが、`StorageError` の網羅性のためここでも扱う。両テーブルは
        // `CatalogError::IncompatibleRowKeyFormat` と完全に同一の文言を持つため、
        // 単純な写像で挙動が揃う（Issue #206）。
        StorageError::IncompatibleRowKeyFormat => CatalogError::IncompatibleRowKeyFormat,
        // 明示トランザクション（SQL-31・TASK-221）の単一ライタ占有による
        // `Storage::begin_write_txn` のロック待ち上限超過（`55P03`）。
        StorageError::WriteLockTimeout | StorageError::WriteTxnHeldByCurrentSession => {
            CatalogError::WriteLockTimeout
        }
    }
}

/// write トランザクション内でカタログテーブルから `table_name` のスキーマを取得する
/// （TASK-146）。`insert_row_into_table` / `insert_rows_into_table` の共通前段処理。
/// カタログテーブル自体が未作成の場合・該当エントリが存在しない場合のいずれも
/// `CatalogError::TableNotFound` に一本化する（他テーブルの存在情報を漏らさない
/// fail-closed な扱い。security.md「アクセス制御の不備」）。
///
/// `pub(crate)`: `tenant.rs`（TASK-95・対象ビヘイビア: RECOVER-4）の書き込みガード API が、
/// 同一 write トランザクション内で「スキーマ取得 → 所有権判定 → 書き込み」を行うために
/// ここへ委譲する（クレート外へは公開しない）。
pub(crate) fn require_table_schema_write(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
) -> Result<TableSchema> {
    let catalog_table = match write_txn.open_table(CATALOG_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Err(CatalogError::TableNotFound(table_name.to_string()))
        }
        Err(e) => return Err(e.into()),
    };
    let guard = catalog_table
        .get(table_name)?
        .ok_or_else(|| CatalogError::TableNotFound(table_name.to_string()))?;
    let bytes = guard.value().to_vec();
    drop(guard);
    let mut resolve = |name: &str| get_enum_type_in_write_txn(write_txn, name);
    decode_schema_with_resolver(table_name, &bytes, &mut resolve)
}

/// `#[cfg(test)]` 限定の生書き込み API（`insert_row_into_table` /
/// `insert_rows_into_table` / `insert_typed_row`。Issue #1078）の共通ガード。
/// これら 3 API は [`crate::constraint::enforce_row_constraints_in_txn`]
/// （TABLE-16・TABLE-17 の単一検査点）を経由せず redb へ直接書き込むため、主キー・
/// UNIQUE・CHECK・FOREIGN KEY のいずれかを宣言したテーブルに対しては
/// `CatalogError::Invalid` で fail-closed に拒否する（テストが意図せず制約違反状態を
/// 作れてしまう経路を塞ぐ。制約付きテーブルへの投入は検査点を経由する
/// `crate::tenant::insert_*` を使うこと）。
///
/// FOREIGN KEY の参照先（親テーブル）側について: 親テーブルが PK／UNIQUE で参照
/// される場合は親テーブル自体が制約付きとして本ガードに拒否される。`id` 疑似列
/// （制約なしの親でも参照可能。`docs/design/foreign-key.md` D1）で参照される場合でも、
/// 生 API は `(tenant_id, id)` キーへの追加または同一キーの置換のみを行い既存の `id`
/// を削除しないため、参照元行が参照先を失う（孤児化する）ことはない。
#[cfg(test)]
fn reject_constrained_table_for_raw_write(schema: &TableSchema) -> Result<()> {
    if schema.primary_key().is_some()
        || !schema.unique_constraints().is_empty()
        || !schema.checks().is_empty()
        || !schema.foreign_keys().is_empty()
    {
        return Err(CatalogError::Invalid(
            "raw test-only write API does not support tables with declared constraints; use crate::tenant write APIs".to_string(),
        ));
    }
    Ok(())
}

/// read トランザクション内でカタログテーブルに `table_name` が定義済みかを確認する
/// （TASK-146）。`get_row_from_table` / `scan_table_page` の共通前段処理。スキーマ本体は
/// 呼び出し元が使わないため取得・デコードしない（[`require_table_schema_write`] と異なり
/// 存在確認のみ）。判定方針は同様に fail-closed。
fn require_table_exists_read(read_txn: &redb::ReadTransaction, table_name: &str) -> Result<()> {
    let catalog_table = match read_txn.open_table(CATALOG_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Err(CatalogError::TableNotFound(table_name.to_string()))
        }
        Err(e) => return Err(e.into()),
    };
    if catalog_table.get(table_name)?.is_none() {
        return Err(CatalogError::TableNotFound(table_name.to_string()));
    }
    Ok(())
}

/// テーブル単位の世代カウンタ（codex-review P1 指摘対応、PR #266）。キーは論理
/// テーブル名（[`validate_identifier`] 通過済み）、値は当該テーブルへの書き込み
/// commit 回数を表す単調増加カウンタ。`crate::storage::GENERATION_TABLE`
/// （ストレージ全体で任意の write commit ごとに増える世代）とは別テーブルで、
/// こちらは対象テーブル（カタログ定義の DDL・`user_rows/{table_name}` への
/// 行書き込み）に限定して増加する。無関係な他テーブルへの書き込みが本カウンタへ
/// 影響しないことが `USING PLAN` の I/O 前後世代照合（`core.rs`
/// `EngineCore::execute_sql_in_session` の `Statement::Select` アーム参照）の
/// 可用性契約（「テナント境界を跨いだ通常の書き込みトラフィックで USING PLAN が
/// 恒常的に拒否されない」）の土台になる。粒度がテーブル単位・全テナント共通
/// （テナント単位・可視性境界単位への細分化は行わない）であることの設計判断は
/// Issue #285 で現状維持として確定した。根拠・移行トリガーは
/// `docs/design/table-generation-rejection-granularity.md` を参照。
const TABLE_GENERATION_TABLE: TableDefinition<&str, u64> = TableDefinition::new("table_generation");

/// [`TABLE_GENERATION_TABLE`] を 1 つ進める（`write_txn.commit()` 前に呼ぶ）。
///
/// 呼び出し元は、当該 `table_name` の `CATALOG_TABLE` エントリ（DDL）または
/// `user_rows/{table_name}`（DML）のいずれかを同一 `write_txn` 内で変更した
/// すべての箇所（`Storage::create_table`・`drop_table`・`alter_table_add_column`・
/// `insert_row_into_table`・`insert_rows_into_table`・`insert_typed_row`、および
/// `tenant.rs` の `insert_row_unchecked`・`insert_rows_unchecked`・
/// `insert_typed_row_unchecked`・`update_row_unchecked`・`delete_row_unchecked`・
/// `replace_typed_rows_by_text_key`）。新たに対象テーブルの行・スキーマを変更する
/// 書き込み経路を追加する場合は、その commit 前にも本関数を呼ぶこと（呼び忘れは
/// `USING PLAN` の世代照合が対象テーブルの実変更を見逃す fail-open に直結する）。
/// 「変更なしの early return（空バッチ・削除 0 件等）で commit 自体を行わない」
/// 経路は本関数を呼ばない（world 全体の [`crate::storage::bump_generation_and_commit`]
/// と同じく、commit しない = 世代を進めない、が既存契約）。
///
/// `DROP TABLE`→同名再作成の場合もカウンタは単純に増加し続ける（drop 時にリセット
/// しない）。前後比較で「変化したか」だけを見る呼び出し元にとっては、リセットの
/// 有無は無関係（drop→再作成でも必ず値が変わることが重要）。
pub(crate) fn bump_table_generation_in_txn(
    write_txn: &redb::WriteTransaction,
    table_name: &str,
) -> Result<()> {
    let mut gen_table = write_txn.open_table(TABLE_GENERATION_TABLE)?;
    let current = gen_table.get(table_name)?.map(|v| v.value()).unwrap_or(0);
    let next = current
        .checked_add(1)
        .ok_or(CatalogError::TableGenerationCounterOverflow)?;
    gen_table.insert(table_name, next)?;
    Ok(())
}

/// [`bump_table_generation_in_txn`] の読み取り側。テーブルが未作成（1 度も
/// 書き込まれていない）場合は `0` を返す（`crate::storage::current_generation_in_txn`
/// と同じ「未作成 = 世代 0」の方針）。
pub(crate) fn table_generation_in_txn(
    read_txn: &redb::ReadTransaction,
    table_name: &str,
) -> Result<u64> {
    match read_txn.open_table(TABLE_GENERATION_TABLE) {
        Ok(t) => Ok(t.get(table_name)?.map(|v| v.value()).unwrap_or(0)),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(0),
        Err(e) => Err(e.into()),
    }
}

/// カタログ DDL API。`Storage`（`storage.rs`）の拡張として実装し、
/// `Storage::db()` を経由して `ROWS_TABLE` とは別のテーブル（[`CATALOG_TABLE`]）
/// のみを読み書きする。行データへは一切アクセスしない（TABLE-4/TABLE-5）。
impl Storage {
    /// 新規テーブルを定義する（TABLE-4）。同名テーブルが既に存在する場合は
    /// 上書きせず `Err` を返す。カタログテーブルのみを触る単一 write txn で完結する。
    ///
    /// `FOREIGN KEY`（TABLE-17・TASK-205、Issue #907）を宣言するスキーマは、同じ
    /// write txn 内で参照先を解決・照合してから永続化する（TOCTOU 回避。判定順序:
    /// 事前検証〔参照先を要さない不変条件〕→ テーブル名重複 `TableAlreadyExists` →
    /// 参照先の名前解決〔ビュー・索引名は `WrongObjectKind`、不在は
    /// `TableNotFound`〕→ 主キー・UNIQUE 制約・型の照合〔`InvalidForeignKey`〕）。
    /// 自己参照は参照先を作成中のスキーマ自身として解決する。参照先を持たない
    /// スキーマの挙動・カタログバイト列は従来と同一。
    pub fn create_table(&self, schema: &TableSchema) -> Result<()> {
        // UNIQUE 制約の名前を確定する（設計 D2・Issue #1067）。`CREATE TABLE` の
        // 構文段（`sql::allowlist`）は常に名前未確定（空名）の `UniqueConstraint`
        // を組み立てるため、`validate_schema`（`encode_schema` 内）が空名を
        // 拒否する前にここで確定する（TOCTOU 回避。FK の `parent_columns`
        // 未解決中間表現と同じ流儀）。
        let schema = &assign_unique_constraint_names(schema.clone());
        // スキーマ検証は `encode_schema` 内の `validate_schema` に集約する（write txn を
        // 開く前に fail-closed に拒否される。ここで別途 `validate_schema` を呼ぶ必要はない）。
        // `FOREIGN KEY` を持つスキーマは参照先列の解決を write txn 内で行うため、
        // ここでは参照先を要さない不変条件のみを先に検証し、エンコードは解決後に行う。
        let pre_encoded = if schema.foreign_keys.is_empty() {
            Some(encode_schema(schema)?)
        } else {
            validate_schema(&schema.clone().with_foreign_keys(Vec::new()))?;
            validate_foreign_keys(schema, true)?;
            None
        };
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        {
            // ENUM 列（[`ColumnType::Enum`]）が参照する型は、この write txn の
            // 時点でカタログに登録済みであることを検証する（Issue #890 D2。
            // `encoded` は型名のみを持つため、ここで未登録の型名を通すと
            // 存在しない型を参照する列が作成されてしまう）。カタログ側は
            // 型名だけを永続化するため、呼び出し元が渡した `Arc<EnumTypeDef>`
            // の中身（語彙）自体は検証結果として使わず捨てる。
            for column in &schema.columns {
                if let ColumnType::Enum(def) = &column.ty {
                    get_enum_type_in_write_txn(&write_txn, def.name())?;
                }
            }
            let table = write_txn.open_table(CATALOG_TABLE)?;
            if table.get(schema.name.as_str())?.is_some() {
                return Err(CatalogError::TableAlreadyExists(schema.name.clone()));
            }
            // ビューとテーブルは名前空間を共有する（TABLE-18・SQL-23・TASK-205、
            // Issue #909）。`VIEWS_TABLE` 側の衝突も同じ `TableAlreadyExists` へ
            // 写像する。
            match write_txn.open_table(VIEWS_TABLE) {
                Ok(views_table) => {
                    if views_table.get(schema.name.as_str())?.is_some() {
                        return Err(CatalogError::TableAlreadyExists(schema.name.clone()));
                    }
                }
                Err(redb::TableError::TableDoesNotExist(_)) => {}
                Err(e) => return Err(CatalogError::from(e)),
            }
            // 索引名とも名前空間を共有する（TASK-206・INDEX-7、Issue #908）。
            // 既存の索引宣言と同名のテーブルは同じ `TableAlreadyExists` で拒否する。
            if index_name_exists_in_txn(&write_txn, schema.name.as_str())? {
                return Err(CatalogError::TableAlreadyExists(schema.name.clone()));
            }
        }
        // `FOREIGN KEY` の参照先解決は `CATALOG_TABLE` を別途開く（参照先スキーマの
        // decode）ため、上のブロックでハンドルを解放してから行う（redb の
        // `TableAlreadyOpen` 回避）。
        let encoded = match pre_encoded {
            Some(encoded) => encoded,
            None => encode_schema(&resolve_foreign_keys_in_txn(&write_txn, schema)?)?,
        };
        {
            let mut table = write_txn.open_table(CATALOG_TABLE)?;
            table.insert(schema.name.as_str(), encoded.as_slice())?;
        }
        bump_table_generation_in_txn(&write_txn, &schema.name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// テーブル定義（`CATALOG_TABLE` エントリ）と、対応する行ストア
    /// （`user_rows/{table_name}`）を同一 write txn で削除する `DROP TABLE` 相当の DDL
    /// （Issue #179。PR #151 レビューで据え置かれた stale `PrefilterIndex` 対策）。
    ///
    /// 失効契約: `bump_generation_and_commit` 経由で commit するため、drop 前に構築された
    /// [`crate::rls::PrefilterSnapshot`]／[`crate::core`] の `PrefilterCache` エントリは
    /// 以後の世代照合でいずれも stale（`RlsError::IndexStale`／キャッシュ破棄）になり、
    /// 削除済み行・旧スキーマの行に基づく結果を返す経路がない。drop 専用の新たな失効機構は
    /// 追加しない（世代カウンタへの一本化）。
    ///
    /// 安全性: `create_table`／`alter_table_add_column` と同じく
    /// [`crate::policy::PolicyContext`] を取らない生の DDL であり、全テナントの行を
    /// 不可逆に削除する。SQL 表層（`DROP TABLE <table>`。SQL-23・TASK-203、
    /// Issue #902）は `crate::sql::ddl::require_ddl_permission`（接続単位の
    /// DDL 実行権限ゲート。既定拒否・`42501`。`CREATE TABLE`〔SQL-23・
    /// TASK-202、Issue #899〕と共有する単一の判定点）を通過したセッションに
    /// 限り `crate::sql::ddl::execute_drop_table` 経由で本メソッドへ到達する
    /// （wire-server 側の付与経路は `crate::sql::mode::SessionState::allow_ddl`
    /// ドキュメント参照）。
    ///
    /// 存在しないテーブル名は `Err(CatalogError::TableNotFound)`、識別子として不正な
    /// 名前は `Err(CatalogError::Invalid)`（fail-closed。冪等に `Ok` へ丸めない）。
    /// 行ストア（`user_rows/{table_name}`）は初回挿入まで物理的に未作成のことがあり、
    /// その場合の `delete_table` は「元々存在しなかった」ことを表す `Ok(false)` を返すため
    /// エラーにしない。`ROWS_TABLE`（旧・非テーブルスコープ API）・世代テーブルには
    /// 一切触れない。`operation_id` 台帳（`op_ledger`）は同一トランザクション内で
    /// [`crate::recovery::ledger::delete_table_in_txn`] により当該テーブル名分を
    /// 削除する（Issue #226 レビュー対応: drop 後の同名テーブル再作成で旧台帳
    /// エントリが引き継がれ、正当な書き込みを誤って重複拒否する事故を防ぐ）。
    pub fn drop_table(&self, table_name: &str) -> Result<()> {
        validate_identifier(table_name)?;
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        {
            // ビュー名前空間との判別・依存検査（TABLE-18・SQL-23・TASK-205、
            // Issue #909）。テーブル削除より前の同一 write txn 内で判定する
            // （TOCTOU 回避）。`name` がビューなら `WrongObjectKind`（`42809`）、
            // このテーブルを参照するビューが 1 つでも残っていれば
            // `DependentViewsExist`（`2BP01`）。
            match write_txn.open_table(VIEWS_TABLE) {
                Ok(views_table) => {
                    if views_table.get(table_name)?.is_some() {
                        return Err(CatalogError::WrongObjectKind(table_name.to_string()));
                    }
                    let dependents = views_depending_on_in_txn(&views_table, table_name)?;
                    if !dependents.is_empty() {
                        return Err(CatalogError::DependentViewsExist(table_name.to_string()));
                    }
                }
                Err(redb::TableError::TableDoesNotExist(_)) => {}
                Err(e) => return Err(CatalogError::from(e)),
            }
            // 索引名を指定された場合もビューと同じく種別不一致（`42809`）とする
            // （TASK-206・INDEX-7、Issue #908）。
            if index_name_exists_in_txn(&write_txn, table_name)? {
                return Err(CatalogError::WrongObjectKind(table_name.to_string()));
            }
        }
        // `FOREIGN KEY` からの参照（TABLE-17・TASK-205、Issue #907。TABLE-15）:
        // 他テーブルがこのテーブルを参照先にしている場合はデータの有無を問わず
        // カタログ情報のみで `DependentObjectsStillExist`（`2BP01`）として拒否する
        // （テナントデータを一切参照しないため、他テナントの行の有無は結果に
        // 影響しない）。このテーブル自身の自己参照は、参照元ごと削除されるため
        // 依存に数えない。
        if referencing_foreign_keys_in_txn(&write_txn, table_name)?
            .iter()
            .any(|(child, _)| child.name != table_name)
        {
            return Err(CatalogError::DependentObjectsStillExist(
                table_name.to_string(),
            ));
        }
        {
            let mut table = match write_txn.open_table(CATALOG_TABLE) {
                Ok(table) => table,
                Err(redb::TableError::TableDoesNotExist(_)) => {
                    return Err(CatalogError::TableNotFound(table_name.to_string()));
                }
                Err(e) => return Err(CatalogError::from(e)),
            };
            if table.remove(table_name)?.is_none() {
                return Err(CatalogError::TableNotFound(table_name.to_string()));
            }
        }
        // `CATALOG_TABLE` のハンドルは上のブロックを抜けた時点で解放済み。ここで
        // 行ストアを同一 txn 内で `open_table` すると `TableAlreadyOpen` になるため、
        // `delete_table` は既存ハンドルを介さず直接呼ぶ。
        write_txn.delete_table(user_rows_table_def(&user_rows_table_name(table_name)))?;
        // 永続一意索引（TABLE-16・TASK-204、Issue #1070）も行ストアと同一 txn・
        // 同一 commit で削除する（存在しない場合は `delete_table` が `Ok(false)`
        // を返すだけで、`PRIMARY KEY`・`UNIQUE` のいずれも宣言しないテーブルでも
        // エラーにならない）。放置すると同名テーブルを再作成した際に旧索引の
        // マーカー・エントリが残り、新テーブルの行を旧テナントの索引と誤って
        // 突き合わせてしまう（[`user_rows_table_def`] ドキュメントの「残留を
        // 許すと～」と同種の事故）。
        write_txn.delete_table(user_uniq_table_def(&user_uniq_table_name(table_name)))?;
        // op_ledger も同一 txn・同一 commit で整合させる（上記ドキュメンテーション
        // コメント参照）。行ストア削除と異なりテーブル自体は残す（他テーブル分の
        // エントリが同居するため）。
        crate::recovery::ledger::delete_table_in_txn(&write_txn, table_name)
            .map_err(convert_storage_error)?;
        // 索引宣言（TASK-206・INDEX-7、Issue #908）も同一 txn・同一 commit で整合
        // させる（[`delete_indexes_for_table_in_txn`] 参照）。
        delete_indexes_for_table_in_txn(&write_txn, table_name)?;
        // 永続キー索引（`key_index.rs`、Issue #1071）も同一 txn・同一 commit で
        // 削除する。残置すると同名テーブルの再作成後に旧テナント・旧スキーマ
        // 由来のキーが混入し、参照整合性検査が fail-open に誤判定しうる
        // （`key_index::drop_indexes_for_table_in_txn` ドキュメント参照）。
        crate::key_index::drop_indexes_for_table_in_txn(&write_txn, table_name)?;
        bump_table_generation_in_txn(&write_txn, table_name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// 既存テーブルへ列を末尾追記する（TABLE-5）。追加列は暗黙 nullable として
    /// 保持され、既存行のバイト列には一切触れない（`ROWS_TABLE` 非アクセス）。
    /// `column.nullable == false` は fail-closed に拒否する
    /// （security.md「不安全な設計」）。対象テーブル不存在・列名重複も `Err`。
    pub fn alter_table_add_column(&self, table_name: &str, mut column: ColumnDef) -> Result<()> {
        validate_identifier(table_name)?;
        validate_column(&column)?;
        if !column.nullable {
            return Err(CatalogError::Invalid(
                "column added via ALTER TABLE ADD COLUMN must be nullable".to_string(),
            ));
        }
        // `DEFAULT` を伴う ADD COLUMN は未対応（TABLE-16・TASK-204、Issue #904
        // D7）。既存行に対する読み出し時の DEFAULT 補完（PostgreSQL の
        // `ALTER TABLE ... ADD COLUMN ... DEFAULT ...` 相当）を実装していない
        // ため、受理すると既存行が常に NULL で読める一方、新規行だけ既定値を
        // 持つという意味論の食い違いが生じる。fail-closed に拒否する。
        if column.default.is_some() {
            return Err(CatalogError::Invalid(
                "column added via ALTER TABLE ADD COLUMN must not declare a DEFAULT".to_string(),
            ));
        }
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;

        {
            // 呼び出し元が渡した `ColumnType::Enum` の `Arc<EnumTypeDef>` は信頼せず、
            // この write txn から見えるカタログ登録済みの定義で必ず置き換える
            // （未登録の語彙を呼び出し元が持ち込めないようにする。Issue #890 D2）。
            if let ColumnType::Enum(def) = &column.ty {
                column.ty = ColumnType::Enum(get_enum_type_in_write_txn(&write_txn, def.name())?);
            }
            let mut table = write_txn.open_table(CATALOG_TABLE)?;
            let existing: Vec<u8> = {
                let guard = table
                    .get(table_name)?
                    .ok_or_else(|| CatalogError::TableNotFound(table_name.to_string()))?;
                guard.value().to_vec()
            };
            let mut resolve = |name: &str| get_enum_type_in_write_txn(&write_txn, name);
            let mut schema = decode_schema_with_resolver(table_name, &existing, &mut resolve)?;
            if schema.columns.iter().any(|c| c.name == column.name) {
                return Err(CatalogError::ColumnAlreadyExists(column.name.clone()));
            }
            // 列数上限は物理スロット総数（生存列 + 墓標）に適用する（TABLE-19 D1・
            // Issue #901。墓標も物理容量を消費するため、ADD 前に既存の墓標数も
            // 合算して判定する）。
            // 上限超過は `Invalid` と区別した `TooManyColumns` で返す（Issue #900。
            // SQL 表層 `ALTER TABLE ADD COLUMN` が `54000` へ写像するため）。
            if schema.physical_slot_count() >= MAX_COLUMN_COUNT {
                return Err(CatalogError::TooManyColumns {
                    count: schema.physical_slot_count().saturating_add(1),
                });
            }
            schema.columns.push(column);
            let encoded = encode_schema(&schema)?;
            table.insert(table_name, encoded.as_slice())?;
        }
        bump_table_generation_in_txn(&write_txn, table_name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// 既存列を削除する（`ALTER TABLE ... DROP COLUMN`。TABLE-19・TASK-203、
    /// Issue #901）。カタログのみを書き換える O(1) 操作で、行ストア
    /// （`user_rows/{table_name}`）には一切アクセスしない（TABLE-4/TABLE-5 と
    /// 同じ責務境界）。
    ///
    /// 行の物理ペイロード（`row_codec.rs`）は列の宣言順に位置依存する固定形式
    /// のため、削除後も後続の生存列の物理位置をずらさないよう、削除列の物理
    /// 位置・型（フレーム等価型へ正規化済み）だけを [`DroppedSlot`]（墓標）として
    /// カタログに残す。既存行の削除列の値は読み捨てられ（構造検証のみ行い
    /// 出力しない）、削除後に書き込まれる行は当該位置に常に NULL を書く
    /// （`row_codec::encode_scalar_columns`／`merge_encode_scalar_columns` 参照）。
    ///
    /// 予約列（`id`／`tenant_id`／`visibility`）・`VECTOR` 列・最後の 1 列は
    /// `Err`（`CatalogError::ProtectedColumn`／`CatalogError::Invalid`）で拒否する
    /// （fail-closed。TABLE-19 D1）。対象列が存在しない場合は
    /// `Err(CatalogError::ColumnNotFound)`。
    ///
    /// 失効契約は他の DDL（`alter_table_add_column` 等）と同じくテーブル単位
    /// 世代カウンタへ一本化する（`bump_table_generation_in_txn`）。
    pub fn alter_table_drop_column(&self, table_name: &str, column_name: &str) -> Result<()> {
        validate_identifier(table_name)?;
        validate_identifier(column_name)?;
        if column_name == "id" || column_name == "tenant_id" || column_name == "visibility" {
            return Err(CatalogError::ProtectedColumn(column_name.to_string()));
        }
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        {
            let mut table = write_txn.open_table(CATALOG_TABLE)?;
            let existing: Vec<u8> = {
                let guard = table
                    .get(table_name)?
                    .ok_or_else(|| CatalogError::TableNotFound(table_name.to_string()))?;
                guard.value().to_vec()
            };
            let mut resolve = |name: &str| get_enum_type_in_write_txn(&write_txn, name);
            let mut schema = decode_schema_with_resolver(table_name, &existing, &mut resolve)?;

            // 対象列の物理位置は、削除前のスキーマの物理走査（`.enumerate()`
            // の添字がそのまま物理位置に一致する。[`TableSchema::physical_slots`]
            // は物理位置の昇順で生存列・墓標を列挙するため）で求める。
            let target = schema
                .physical_slots()
                .enumerate()
                .find_map(|(physical_index, slot)| match slot {
                    PhysicalSlot::Live(logical_index, column) if column.name == column_name => {
                        Some((logical_index, physical_index, column.ty.clone()))
                    }
                    _ => None,
                });
            let (logical_index, physical_index, ty) =
                target.ok_or_else(|| CatalogError::ColumnNotFound(column_name.to_string()))?;
            if ty.is_vector() {
                return Err(CatalogError::ProtectedColumn(column_name.to_string()));
            }
            // `PRIMARY KEY`（TABLE-16・TASK-204、Issue #903）の構成列は
            // `constraint::enforce_unique_keys_in_txn` がテナント全行を走査する
            // 前提として生存し続けなければならない。ここで拒否せず
            // `schema.columns.remove` へ進むと、`validate_schema` の
            // 「主キー列が生存列に存在すること」検査に引っかかり
            // `CatalogError::Invalid`（意味論的エラー）として現れてしまい、
            // `ALTER TABLE DROP COLUMN` の依存オブジェクト検査として一貫しない
            // 分類になる（`drop_enum_type` の `DependentObjectsStillExist` と
            // 同じ分類へ揃える）。
            //
            // UNIQUE 制約（TABLE-16・TASK-204、Issue #905）が参照する列も同様に
            // 暗黙 cascade で制約ごと消さず、明示的に拒否する。`CHECK` 制約
            // （TABLE-16・TASK-204、Issue #906）が参照する列も同じく拒否する
            // （制約を黙って弱める・無効化する経路を作らない。fail-closed）。
            if schema
                .primary_key
                .as_ref()
                .is_some_and(|pk| pk.iter().any(|c| c == column_name))
                || schema
                    .unique_constraints
                    .iter()
                    .any(|u| u.columns().iter().any(|c| c == column_name))
                || schema_check_references_column(&schema, column_name)
                // `FOREIGN KEY`（TABLE-17・TASK-205、Issue #907）の参照元列も同様に
                // 拒否する（`DROP CONSTRAINT` を持たないため、制約を黙って消す
                // 暗黙 cascade を作らない）。参照先側の列は主キー・UNIQUE 制約の
                // 構成列（上で拒否済み）か `id`（予約列）に限られる。
                || schema
                    .foreign_keys
                    .iter()
                    .any(|fk| fk.columns.iter().any(|c| c == column_name))
            {
                return Err(CatalogError::DependentObjectsStillExist(
                    column_name.to_string(),
                ));
            }
            let physical_index = u16::try_from(physical_index).map_err(|_| {
                CatalogError::Invalid("dropped column physical index overflow".to_string())
            })?;

            schema.columns.remove(logical_index);
            schema.dropped.push(DroppedSlot {
                physical_index,
                name: column_name.to_string(),
                ty: normalize_dropped_column_type(ty),
            });
            // `physical_slots()`（[`PhysicalSlots`]）は `dropped` が物理位置の
            // 昇順であることを前提とする。
            schema.dropped.sort_by_key(|d| d.physical_index);

            // `validate_schema`（`encode_schema` 内で呼ばれる）が「生存列 1 本以上」
            // を含む全ての不変条件を検証する。ここでの追加検証は不要。
            let encoded = encode_schema(&schema)?;
            table.insert(table_name, encoded.as_slice())?;
        }
        // 削除した列を含む索引宣言（TASK-206・INDEX-7、Issue #908）も同一 txn で
        // 削除する（[`delete_indexes_referencing_column_in_txn`] 参照）。
        delete_indexes_referencing_column_in_txn(&write_txn, table_name, column_name)?;
        bump_table_generation_in_txn(&write_txn, table_name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// 既存テーブルへ UNIQUE 制約を追加する（Rust API 専用。TABLE-16・TASK-204、
    /// Issue #905）。名前は常に既定名で確定する薄いラッパーで、実体は
    /// [`Self::alter_table_add_named_unique_constraint`]（Issue #1067。SQL 表層
    /// `ALTER TABLE ... ADD [CONSTRAINT <name>] UNIQUE` と共有する唯一の実装）に
    /// 委譲する。呼び出し元互換のためシグネチャ・挙動（`Ok(())`）を変えない。
    pub fn alter_table_add_unique_constraint(
        &self,
        table_name: &str,
        columns: &[&str],
    ) -> Result<()> {
        self.alter_table_add_named_unique_constraint(table_name, None, columns)?;
        Ok(())
    }

    /// 既存テーブルへ（任意で名前付きの）UNIQUE 制約を追加する（TABLE-16・
    /// TASK-204、Issue #905／制約名は Issue #1067）。`name` が `None` のときは
    /// 設計 D2 の規則で既定名を確定する（[`assign_unique_constraint_names`]）。
    /// SQL 表層 `ALTER TABLE ... ADD [CONSTRAINT <name>] UNIQUE`
    /// （`sql::ddl::execute_alter_table_add_unique`）と Rust API
    /// （[`Self::alter_table_add_unique_constraint`]）の唯一の実装。
    ///
    /// 判定順序（設計 D7。fail-closed）: (1) 明示名の識別子妥当性・列リストの
    /// 構造（空・重複・上限超過）は呼び出し元（構文段）が検証済みの前提 (2)
    /// テーブル取得（`TableNotFound`） (3) 明示名の衝突（既存 UNIQUE・CHECK
    /// 制約名との重複。`ConstraintAlreadyExists`） (4) 制約数上限
    /// （`ConstraintLimitExceeded`） (5) 名前確定後のスキーマとして
    /// [`validate_schema`]（未宣言列・対象外型・同一列リスト重複は
    /// `CatalogError::Invalid`） (6) 対象テーブルの**全行**（`Public`／
    /// `Private` を問わない。DDL は `PolicyContext` を取らないテーブル単位の
    /// 共有資源操作であるため）をテナントごとに独立して走査し、いずれかの
    /// テナント内で新しい制約列の値の組が重複する行が 1 件でもあれば
    /// `Err(CatalogError::UniqueConstraintViolation)` で拒否する（副作用ゼロ。
    /// write トランザクションを commit せず破棄する）。テナントを跨いだ同値は
    /// 許容する。判定は書き込み時の検査点と同じ正準キーで行う
    /// （[`crate::constraint::table_has_duplicate_unique_key`]）。
    ///
    /// 成功時は確定した制約名を返す（呼び出し元が省略時の既定名を知る唯一の
    /// 手段。SQL 表層の `AlterTableAction::AddConstraint` 応答に使う）。
    ///
    /// `sql::scalar_index` 等の `(table, PolicyContext)` 可視スナップショット
    /// 由来の索引は一切流用しない（可視集合はテナント内の部分集合に過ぎず、
    /// 不可視行の重複を見逃す fail-open になるため）。
    pub fn alter_table_add_named_unique_constraint(
        &self,
        table_name: &str,
        name: Option<&str>,
        columns: &[&str],
    ) -> Result<String> {
        validate_identifier(table_name)?;
        if let Some(n) = name {
            validate_identifier(n)?;
        }
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        let confirmed_name;
        {
            let schema = require_table_schema_write(&write_txn, table_name)?;
            let new_columns: Vec<String> = columns.iter().map(|c| c.to_string()).collect();

            // 明示名の衝突は既定名導出より前に判定する（同名の UNIQUE・CHECK が
            // 既にあるテーブルへ、その名前を明示指定して追加しようとした場合を
            // 確実に拒否するため）。
            if let Some(n) = name {
                let collides = schema.unique_constraints.iter().any(|u| u.name() == n)
                    || schema.checks.iter().any(|c| c.name == n);
                if collides {
                    return Err(CatalogError::ConstraintAlreadyExists(n.to_string()));
                }
            }
            if schema.unique_constraints.len() >= MAX_UNIQUE_CONSTRAINTS {
                return Err(CatalogError::ConstraintLimitExceeded(format!(
                    "table {table_name} already has {MAX_UNIQUE_CONSTRAINTS} unique constraints"
                )));
            }

            let new_constraint = match name {
                Some(n) => UniqueConstraint::with_name(n.to_string(), new_columns.clone()),
                None => UniqueConstraint::new(new_columns.clone()),
            };
            let mut constraints = schema.unique_constraints.clone();
            constraints.push(new_constraint);
            let updated =
                assign_unique_constraint_names(schema.clone().with_unique_constraints(constraints));
            // 既存行の走査より前に、追加後のスキーマとして検証する。
            validate_schema(&updated)?;
            confirmed_name = updated
                .unique_constraints
                .last()
                .map(|u| u.name().to_string())
                .ok_or_else(|| {
                    CatalogError::Invalid("unique constraint was not appended".to_string())
                })?;

            let row_table_name = user_rows_table_name(table_name);
            match write_txn.open_table(user_rows_table_def(&row_table_name)) {
                Ok(row_table) => {
                    if crate::constraint::table_has_duplicate_unique_key(
                        &row_table,
                        &updated,
                        &new_columns,
                    )? {
                        return Err(CatalogError::UniqueConstraintViolation);
                    }
                }
                Err(redb::TableError::TableDoesNotExist(_)) => {
                    // 初回挿入前で行ストアが物理的に未作成（既存行 0 件）。
                }
                Err(e) => return Err(map_row_table_error(e)),
            }

            let encoded = encode_schema(&updated)?;
            let mut catalog_table = write_txn.open_table(CATALOG_TABLE)?;
            catalog_table.insert(table_name, encoded.as_slice())?;
        }
        // 永続一意索引（TABLE-16・TASK-204、Issue #1070）を無効化する。索引の
        // マーカーは列宣言のシグネチャを保持しており本来は自動で不一致検出
        // されるが、`ADD UNIQUE` の直後に索引テーブルごと削除しておくことで
        // 二重の安全策とする（各テナントは次回の書き込み時に遅延再構築される。
        // `row_table` の借用は上のブロックを抜けた時点で解放済みのため、
        // 別テーブルである索引テーブルの削除はここで安全に行える）。
        write_txn.delete_table(user_uniq_table_def(&user_uniq_table_name(table_name)))?;
        bump_table_generation_in_txn(&write_txn, table_name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)?;
        Ok(confirmed_name)
    }

    /// 既存テーブルへ（任意で名前付きの）`FOREIGN KEY` 制約を追加する
    /// （TABLE-22・TASK-233、Issue #1069。SQL 表層
    /// `ALTER TABLE ... ADD [CONSTRAINT <name>] FOREIGN KEY (...) REFERENCES ...`
    /// の実行本体。`pub(crate)`: 宣言面は SQL 表層のみで、`ForeignKeyDef::new`・
    /// `with_foreign_keys` が crate 内限定のため。設計 F11）。`name` が `None`
    /// のときは設計 F2 の規則で既定名を確定する
    /// （[`assign_foreign_key_constraint_names`]）。
    ///
    /// 判定順序（設計 F7。fail-closed）:
    /// 1. 明示名の識別子妥当性・宣言の構造は呼び出し元（構文段）が検証済みの
    ///    前提
    /// 2. 対象テーブル（子）の取得（`TableNotFound`）
    /// 3. 明示名の衝突（既存 UNIQUE・CHECK・FOREIGN KEY 制約名との重複。
    ///    設計 F1。`ConstraintAlreadyExists`）
    /// 4. FK 件数上限（`ConstraintLimitExceeded`）
    /// 5. 参照先の解決（ビュー・索引なら `WrongObjectKind`、不在なら
    ///    `TableNotFound`。自己参照は変更前の子スキーマ自身を親として解決する
    ///    ——親の列・主キー・UNIQUE 制約は今回の FK 追加で変わらないため）・
    ///    `resolve_foreign_key_target` の照合（`InvalidForeignKey`）
    /// 6. 名前確定後の更新後スキーマとして `validate_schema`
    /// 7. 索引衛生（子表・親表の stale 索引を検証**前**に捨てる。§3.4・
    ///    P0。`key_index::prune_unneeded_indexes_in_txn`）
    /// 8. 対象テーブルの**全テナント**の既存行を、各テナント内に閉じた判定で
    ///    検証する（[`crate::constraint::verify_new_foreign_key_all_tenants_in_txn`]。
    ///    1 行でも違反があれば `Err(CatalogError::ForeignKeyViolation)` で
    ///    副作用ゼロに拒否する。RLS-9・RLS-10 (c)）
    /// 9. カタログを書き換えて子テーブルの世代を bump する。
    ///
    /// 成功時は確定した制約名を返す（`alter_table_add_named_unique_constraint`
    /// と同じ契約。SQL 表層の `AlterTableAction::AddConstraint` 応答に使う）。
    pub(crate) fn alter_table_add_foreign_key(
        &self,
        table_name: &str,
        name: Option<&str>,
        foreign_key: ForeignKeyDef,
    ) -> Result<String> {
        validate_identifier(table_name)?;
        if let Some(n) = name {
            validate_identifier(n)?;
        }
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        let confirmed_name;
        let parent_table_name;
        {
            let schema = require_table_schema_write(&write_txn, table_name)?;

            if let Some(n) = name {
                let collides = schema.unique_constraints.iter().any(|u| u.name() == n)
                    || schema.checks.iter().any(|c| c.name == n)
                    || schema.foreign_keys.iter().any(|f| f.name() == n);
                if collides {
                    return Err(CatalogError::ConstraintAlreadyExists(n.to_string()));
                }
            }
            if schema.foreign_keys.len() >= MAX_FOREIGN_KEYS_PER_TABLE {
                return Err(CatalogError::ConstraintLimitExceeded(format!(
                    "table {table_name} already has {MAX_FOREIGN_KEYS_PER_TABLE} foreign keys"
                )));
            }

            let fk_declared = match name {
                Some(n) => foreign_key.with_name(n.to_string()),
                None => foreign_key,
            };
            parent_table_name = fk_declared.parent_table().to_string();

            // 参照先の解決（[`resolve_foreign_keys_in_txn`] と同じ判定だが、
            // 単一の新 FK に限定して適用する）。自己参照は変更前の `schema`
            // 自身を親として解決してよい——`resolve_foreign_key_target` は親の
            // 列・主キー・UNIQUE 制約しか参照せず、親の `foreign_keys` は
            // 見ないため（今回追加する FK 自体が親の列定義に影響しない）。
            let owned_parent;
            let parent: &TableSchema = if parent_table_name == schema.name {
                &schema
            } else {
                match write_txn.open_table(VIEWS_TABLE) {
                    Ok(views_table) => {
                        if views_table.get(parent_table_name.as_str())?.is_some() {
                            return Err(CatalogError::WrongObjectKind(parent_table_name.clone()));
                        }
                    }
                    Err(redb::TableError::TableDoesNotExist(_)) => {}
                    Err(e) => return Err(CatalogError::from(e)),
                }
                if index_name_exists_in_txn(&write_txn, parent_table_name.as_str())? {
                    return Err(CatalogError::WrongObjectKind(parent_table_name.clone()));
                }
                owned_parent = require_table_schema_write(&write_txn, &parent_table_name)?;
                &owned_parent
            };
            let resolved = resolve_foreign_key_target(&schema, &fk_declared, parent)?;

            let mut fks = schema.foreign_keys.clone();
            fks.push(resolved);
            let updated =
                assign_foreign_key_constraint_names(schema.clone().with_foreign_keys(fks));
            validate_schema(&updated)?;
            let final_fk =
                updated.foreign_keys.last().cloned().ok_or_else(|| {
                    CatalogError::Invalid("foreign key was not appended".to_string())
                })?;
            confirmed_name = final_fk.name().to_string();

            // 索引衛生（P0・設計 §3.4）: 検証に使う索引を信用する前に、子表・
            // 親表それぞれの stale 索引（他の DROP で登録簿に残ったまま以後
            // 同期されなくなった索引）を、まだ新 FK を含まない現在のカタログ
            // から求めた「本当に必要な索引名」で刈り込む。
            let required_child = required_parent_key_index_names_in_txn(&write_txn, table_name)?;
            crate::key_index::prune_unneeded_indexes_in_txn(
                &write_txn,
                table_name,
                &required_child,
            )?;
            if parent_table_name != table_name {
                let required_parent =
                    required_parent_key_index_names_in_txn(&write_txn, &parent_table_name)?;
                crate::key_index::prune_unneeded_indexes_in_txn(
                    &write_txn,
                    &parent_table_name,
                    &required_parent,
                )?;
            }

            // 全テナントの既存行検証（RLS-9・RLS-10 (c)）。違反があれば副作用
            // ゼロで拒否する（write_txn は commit しない）。
            crate::constraint::verify_new_foreign_key_all_tenants_in_txn(
                &write_txn,
                &parent_table_name,
                parent,
                &updated,
                &final_fk,
            )
            .map_err(|e| match e {
                crate::tenant::TenantWriteError::ForeignKeyViolation => {
                    CatalogError::ForeignKeyViolation
                }
                crate::tenant::TenantWriteError::Catalog(e) => e,
                _ => CatalogError::Invalid(
                    "foreign key verification failed unexpectedly".to_string(),
                ),
            })?;

            let encoded = encode_schema(&updated)?;
            let mut catalog_table = write_txn.open_table(CATALOG_TABLE)?;
            catalog_table.insert(table_name, encoded.as_slice())?;
        }
        bump_table_generation_in_txn(&write_txn, table_name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)?;
        Ok(confirmed_name)
    }

    /// 既存テーブルの UNIQUE・FOREIGN KEY 制約を名前で削除する（SQL-23・
    /// TASK-204・TABLE-22・TASK-233、Issue #1067・#1069。
    /// `ALTER TABLE ... DROP CONSTRAINT <name>`）。CHECK 制約名を指定した場合は
    /// `ConstraintDropNotSupported`（`0A000`。CHECK の DROP はスコープ外。
    /// 設計 D6）で拒否し、暗黙の CHECK 削除を許さない。
    ///
    /// 判定順序（設計 D7・F8。fail-closed）: (1) テーブル取得（`TableNotFound`）
    /// (2) 名前の検索——UNIQUE → FOREIGN KEY → CHECK の順（設計 F8）。CHECK に
    /// あれば `ConstraintDropNotSupported`、どれにも無ければ `ConstraintNotFound`
    /// (3) UNIQUE 削除時のみ: 削除対象の列集合を、他の `FOREIGN KEY` 宣言
    /// （自己参照を含む。[`referencing_foreign_keys_in_txn`]）の
    /// `parent_columns` の**集合**が覆っていれば `DependentObjectsStillExist`
    /// （`2BP01`）。主キーや他の UNIQUE が同じ集合を覆っていても救済せず拒否
    /// する（fail-closed。設計 D7）。FOREIGN KEY 削除は既存行を変更しないため
    /// 依存検査は不要
    /// (4) カタログを書き換える
    /// (5) 索引衛生（P0・設計 §3.4）: 削除後のカタログから求めた「本当に
    /// 必要な索引名」で、子表（UNIQUE 削除時はこの表自身も対象。他表の FK が
    /// この表を参照しうるため）・FOREIGN KEY 削除時は親表の stale 索引を刈り込む
    /// (6) 世代を bump する。
    pub fn alter_table_drop_constraint(&self, table_name: &str, name: &str) -> Result<()> {
        validate_identifier(table_name)?;
        validate_identifier(name)?;
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        let mut dropped_fk_parent_table: Option<String> = None;
        {
            let schema = require_table_schema_write(&write_txn, table_name)?;
            let unique_index = schema
                .unique_constraints
                .iter()
                .position(|u| u.name() == name);
            let updated = if let Some(target_index) = unique_index {
                // `.position()` の戻り値をそのまま添字アクセスせず `.get()` を使う
                // （coding-rust.md「受信データ経路では添字アクセス禁止」の文言
                // 遵守。同一 Vec を同一トランザクション内で即座に参照するため
                // 実際には常に Some だが、fail-closed の作法として明示的に
                // 処理する）。
                let target_columns: Vec<String> = schema
                    .unique_constraints
                    .get(target_index)
                    .ok_or_else(|| {
                        CatalogError::CorruptSchema(format!(
                            "unique constraint index out of range for table {table_name}"
                        ))
                    })?
                    .columns()
                    .to_vec();

                // FK 依存検査（自己参照を含む）: `parent_columns` の集合が削除対象の
                // 列集合と一致する宣言が 1 件でもあれば拒否する。
                let referencing = referencing_foreign_keys_in_txn(&write_txn, table_name)?;
                let target_set: std::collections::HashSet<&str> =
                    target_columns.iter().map(|s| s.as_str()).collect();
                for (_referencing_schema, fk) in &referencing {
                    let parent_set: std::collections::HashSet<&str> =
                        fk.parent_columns().iter().map(|s| s.as_str()).collect();
                    if parent_set == target_set {
                        return Err(CatalogError::DependentObjectsStillExist(name.to_string()));
                    }
                }

                let mut constraints = schema.unique_constraints.clone();
                constraints.remove(target_index);
                schema.clone().with_unique_constraints(constraints)
            } else if let Some(fk_index) = schema.foreign_keys.iter().position(|f| f.name() == name)
            {
                // FOREIGN KEY の削除（設計 F8）: 既存行は変更しない。索引衛生は
                // カタログ書き換え後にまとめて行う（下記）。
                let fk = schema.foreign_keys.get(fk_index).ok_or_else(|| {
                    CatalogError::CorruptSchema(format!(
                        "foreign key index out of range for table {table_name}"
                    ))
                })?;
                dropped_fk_parent_table = Some(fk.parent_table().to_string());
                let mut fks = schema.foreign_keys.clone();
                fks.remove(fk_index);
                schema.clone().with_foreign_keys(fks)
            } else {
                if schema.checks.iter().any(|c| c.name == name) {
                    return Err(CatalogError::ConstraintDropNotSupported(name.to_string()));
                }
                return Err(CatalogError::ConstraintNotFound(name.to_string()));
            };

            let encoded = encode_schema(&updated)?;
            let mut catalog_table = write_txn.open_table(CATALOG_TABLE)?;
            catalog_table.insert(table_name, encoded.as_slice())?;
        }
        // 索引衛生（P0・設計 §3.4）: カタログ書き換え後（＝索引の要否判定が
        // 「削除後」の状態を見る）に、影響を受けうる表の stale 索引を刈り込む。
        // UNIQUE 削除は「この表を親とする索引」（他表の FK が UNIQUE 列を参照
        // しうる）を、FOREIGN KEY 削除は「削除した FK の参照先（親）表を親と
        // する索引」を対象にする（`key_index.rs` の索引は常に親テーブル基準で
        // 構築される契約。`required_parent_key_index_names_in_txn` 参照）。
        let required_self = required_parent_key_index_names_in_txn(&write_txn, table_name)?;
        crate::key_index::prune_unneeded_indexes_in_txn(&write_txn, table_name, &required_self)?;
        if let Some(parent_table) = &dropped_fk_parent_table {
            if parent_table != table_name {
                let required_parent =
                    required_parent_key_index_names_in_txn(&write_txn, parent_table)?;
                crate::key_index::prune_unneeded_indexes_in_txn(
                    &write_txn,
                    parent_table,
                    &required_parent,
                )?;
            }
        }
        bump_table_generation_in_txn(&write_txn, table_name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// `NUMERIC(p, s)` 列の精度 `p` を拡大する（`ALTER TABLE ... ALTER COLUMN
    /// ... TYPE NUMERIC(new_precision, s)`。`s` は変更しない。TABLE-19 D3・
    /// TASK-203、Issue #901）。
    ///
    /// この変更は行の物理フレーム幅（presence(1) + unscaled i128(16) = 17 バイト
    /// 固定）に一切影響しないため、カタログのみを書き換える O(1) 操作であり、
    /// 既存行の書き換えは不要（[`crate::numeric::Decimal`] は unscaled 値を
    /// `precision` に対して都度検証するため、`new_precision > 旧 precision`
    /// であれば既存の全テナントの既存値は新しい宣言の下でも必ず有効な値として
    /// 読み出せる）。
    ///
    /// 受理するのはこの 1 パターン（`NUMERIC` → より大きい `precision`・同一
    /// `scale` の `NUMERIC`）のみで、それ以外の型変更（`INTEGER`→`BIGINT`・
    /// `REAL`→`DOUBLE PRECISION` を含む、行の書き換えを要する拡大変換や、
    /// 縮小変換・異種変換・同一型への変更）はすべて
    /// `Err(CatalogError::IncompatibleTypeChange)` で拒否する（行の書き換えを
    /// 伴う変換は本 Issue のスコープ外。`docs/design/alter-table-drop-modify-column.md`
    /// 参照）。
    pub fn alter_table_widen_numeric_precision(
        &self,
        table_name: &str,
        column_name: &str,
        new_precision: u8,
    ) -> Result<()> {
        validate_identifier(table_name)?;
        validate_identifier(column_name)?;
        if column_name == "id" || column_name == "tenant_id" || column_name == "visibility" {
            return Err(CatalogError::ProtectedColumn(column_name.to_string()));
        }
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        {
            let mut table = write_txn.open_table(CATALOG_TABLE)?;
            let existing: Vec<u8> = {
                let guard = table
                    .get(table_name)?
                    .ok_or_else(|| CatalogError::TableNotFound(table_name.to_string()))?;
                guard.value().to_vec()
            };
            let mut resolve = |name: &str| get_enum_type_in_write_txn(&write_txn, name);
            let mut schema = decode_schema_with_resolver(table_name, &existing, &mut resolve)?;
            // `CHECK` 制約（TABLE-16・TASK-204、Issue #906）が参照する列の型変更は
            // 安全側で拒否する（`alter_table_drop_column` と同じ判断。述語の
            // 意味を黙って変えない）。
            if schema_check_references_column(&schema, column_name) {
                return Err(CatalogError::DependentObjectsStillExist(
                    column_name.to_string(),
                ));
            }
            let column = schema
                .columns
                .iter_mut()
                .find(|c| c.name == column_name)
                .ok_or_else(|| CatalogError::ColumnNotFound(column_name.to_string()))?;
            let (old_precision, old_scale) = match column.ty {
                ColumnType::Numeric { precision, scale } => (precision, scale),
                ref other => {
                    return Err(CatalogError::IncompatibleTypeChange {
                        column: column_name.to_string(),
                        from: other.catalog_fields().0.to_string(),
                        to: format!("numeric,{new_precision}"),
                    })
                }
            };
            if new_precision <= old_precision {
                return Err(CatalogError::IncompatibleTypeChange {
                    column: column_name.to_string(),
                    from: format!("numeric,{old_precision},{old_scale}"),
                    to: format!("numeric,{new_precision},{old_scale}"),
                });
            }
            // 新しい (precision, scale) の組が有効であること（1..=MAX_PRECISION・
            // scale <= precision）を検証してから確定する。
            validate_numeric_precision_scale(new_precision, old_scale)?;
            column.ty = ColumnType::Numeric {
                precision: new_precision,
                scale: old_scale,
            };
            let encoded = encode_schema(&schema)?;
            table.insert(table_name, encoded.as_slice())?;
        }
        bump_table_generation_in_txn(&write_txn, table_name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// 新規 ENUM 型を定義する（TABLE-14・TASK-198、Issue #890）。SQL-23（`CREATE
    /// TYPE ... AS ENUM`）未実装のため Rust API 専用の DDL。同名の型が既に
    /// 存在する場合は上書きせず `Err(CatalogError::TypeAlreadyExists)`。
    pub fn create_enum_type(&self, name: &str, labels: Vec<String>) -> Result<Arc<EnumTypeDef>> {
        let encoded = encode_enum_type_def(name, &labels)?;
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        {
            let mut table = write_txn.open_table(ENUM_TYPES_TABLE)?;
            if table.get(name)?.is_some() {
                return Err(CatalogError::TypeAlreadyExists(name.to_string()));
            }
            let count = table.len()?;
            if count >= MAX_ENUM_TYPES as u64 {
                return Err(CatalogError::Invalid(format!(
                    "too many enum types registered: {count}"
                )));
            }
            table.insert(name, encoded.as_slice())?;
        }
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)?;
        Ok(Arc::new(EnumTypeDef {
            name: name.to_string(),
            labels,
        }))
    }

    /// 定義済みの ENUM 型をスナップショット読み取りで取得する。
    pub fn get_enum_type(&self, name: &str) -> Result<Arc<EnumTypeDef>> {
        validate_identifier(name)?;
        let read_txn = self.db().begin_read()?;
        let table = match read_txn.open_table(ENUM_TYPES_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(CatalogError::TypeNotFound(name.to_string()))
            }
            Err(e) => return Err(e.into()),
        };
        let guard = table
            .get(name)?
            .ok_or_else(|| CatalogError::TypeNotFound(name.to_string()))?;
        Ok(Arc::new(decode_enum_type_def(name, guard.value())?))
    }

    /// `ALTER TYPE <name> ADD VALUE '<label>'` 相当（TABLE-14・TASK-198）。既存
    /// ラベルの末尾へ 1 個だけ追記する。並べ替え・`BEFORE`/`AFTER` 指定・
    /// 削除・改名はいずれも提供しない（Issue #890 D4。既存行はラベル文字列を
    /// そのまま格納するため、語彙の単調増加さえ守れば古いスナップショットで
    /// 有効だった値は将来にわたって有効であり続ける契約を維持できる）。
    /// 依存テーブルが存在する場合、この write txn 内で該当テーブルすべての
    /// 世代を進行させる（多層防御。同一クエリ内でスキーマが再取得されない
    /// キャッシュ経路が新ラベルを見落とす可能性を保守的に潰す）。
    pub fn alter_enum_type_add_value(&self, name: &str, label: String) -> Result<Arc<EnumTypeDef>> {
        validate_identifier(name)?;
        validate_enum_label(&label)?;
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        let updated = {
            let mut table = write_txn.open_table(ENUM_TYPES_TABLE)?;
            let existing = table
                .get(name)?
                .ok_or_else(|| CatalogError::TypeNotFound(name.to_string()))?
                .value()
                .to_vec();
            let mut def = decode_enum_type_def(name, &existing)?;
            if def.labels.iter().any(|l| l == &label) {
                return Err(CatalogError::Invalid(format!(
                    "enum label already exists: {label:?}"
                )));
            }
            def.labels.push(label);
            validate_enum_labels(&def.labels)?;
            let encoded = encode_enum_type_def(name, &def.labels)?;
            table.insert(name, encoded.as_slice())?;
            def
        };
        for table_name in dependent_tables_in_txn(&write_txn, name)? {
            bump_table_generation_in_txn(&write_txn, &table_name)?;
        }
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)?;
        Ok(Arc::new(updated))
    }

    /// `DROP TYPE <name>` 相当。依存列（[`ColumnType::Enum`] でこの型を参照する
    /// 列）が 1 つでも残っている場合は
    /// `Err(CatalogError::DependentObjectsStillExist)` で拒否する（SQL-23 結線時は
    /// `2BP01` へ写像する想定。Issue #890 D5）。
    pub fn drop_enum_type(&self, name: &str) -> Result<()> {
        validate_identifier(name)?;
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        {
            let dependents = dependent_tables_in_txn(&write_txn, name)?;
            if !dependents.is_empty() {
                return Err(CatalogError::DependentObjectsStillExist(name.to_string()));
            }
            let mut table = write_txn.open_table(ENUM_TYPES_TABLE)?;
            if table.remove(name)?.is_none() {
                return Err(CatalogError::TypeNotFound(name.to_string()));
            }
        }
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// `CREATE VIEW <name> AS <body>`（TABLE-18・SQL-23・TASK-205、Issue #909）の
    /// 永続化。呼び出し元（`sql::ddl::execute_create_view`）は許可リスト検証
    /// （`sql::allowlist::validate_create_view_tokens`）を通過した `name`・
    /// `base_relation`・`body_sql` を渡す（カタログ照会自体はここで初めて行う。
    /// 構文検証段はカタログを一切照会しない契約）。
    ///
    /// `body_sql` はこのメソッド自身も [`parse_view_body`]（`CREATE VIEW` 構文
    /// 検証・`sql::view::resolve_from` の再検証と同一実装）で再検証し、`FROM`
    /// が指す名前が `base_relation` と一致することを保存前に確認する
    /// （codex-review 指摘・PR #1048: 本メソッドは `pub fn` の Rust API であり、
    /// SQL 表層の `validate_create_view_tokens`〔`body_sql` を必ず
    /// `render_view_body(parsed)` として `base_relation` と対応づけて生成する〕
    /// を経由しない呼び出し元が、`body_sql` の実際の `FROM` と食い違う
    /// `base_relation`・許可リスト外の形状〔式項目・UDF 述語〕を持つ定義を
    /// そのまま永続化できてしまっていた。`sql::view::resolve_from` は列公開
    /// 判定を `body_sql` の投影から、連鎖の探索先を `base_relation` から
    /// それぞれ独立に読むため、この食い違いは「`body_sql` が宣言する列は
    /// 実際には別の関係に対して検証されたものではない」という列スコープ契約
    /// 〔`sql::view::check_columns_within_view`〕の前提を静かに破る）。
    ///
    /// 判定順序（同一 write txn 内。TOCTOU 回避）: `body_sql` の構文・
    /// 参照先一致検証（`Err(Invalid)`）→ 名前空間の衝突
    /// （[`CATALOG_TABLE`]／[`VIEWS_TABLE`] のいずれか。`Err(TableAlreadyExists)`）
    /// → 参照先の存在・ネスト深さ（[`resolve_reference_depth_in_txn`]。参照先
    /// 不存在は `Err(TableNotFound)`、深さ超過は `Err(ViewLimitExceeded)`）→
    /// 登録件数上限（`Err(ViewLimitExceeded)`）→ 保存。
    ///
    /// 自己参照（`CREATE VIEW v AS ... FROM v`）は `base_relation == name` が
    /// [`CATALOG_TABLE`]・[`VIEWS_TABLE`] のいずれにも存在しない（`name` は
    /// この時点でまだ登録されていない）ことから構造的に `TableNotFound` へ
    /// 落ち、何も永続化されない。行ストア・世代カウンタは持たないため
    /// [`bump_table_generation_in_txn`] は呼ばない。
    pub fn create_view(&self, name: &str, base_relation: &str, body_sql: &str) -> Result<()> {
        validate_identifier(name)?;
        if body_sql.len() > MAX_VIEW_BODY_BYTES {
            return Err(CatalogError::ViewLimitExceeded(
                "view body too large".to_string(),
            ));
        }
        validate_view_body_matches_base_relation(body_sql, base_relation)?;
        let encoded = encode_view_def(&ViewDef {
            base_relation: base_relation.to_string(),
            body_sql: body_sql.to_string(),
        })?;
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        {
            let catalog_table = write_txn.open_table(CATALOG_TABLE)?;
            if catalog_table.get(name)?.is_some() {
                return Err(CatalogError::TableAlreadyExists(name.to_string()));
            }
            let mut views_table = write_txn.open_table(VIEWS_TABLE)?;
            if views_table.get(name)?.is_some() {
                return Err(CatalogError::TableAlreadyExists(name.to_string()));
            }
            // 索引名とも名前空間を共有する（TASK-206・INDEX-7、Issue #908）。
            if index_name_exists_in_txn(&write_txn, name)? {
                return Err(CatalogError::TableAlreadyExists(name.to_string()));
            }
            let depth =
                resolve_reference_depth_in_txn(&catalog_table, &views_table, base_relation)?;
            // テーブル自身が深さ 0 のため、それを直接参照する新規ビューの深さは
            // `depth`（参照先の深さ）+ 1。
            let new_depth = depth.checked_add(1).ok_or_else(|| {
                CatalogError::ViewLimitExceeded("view nesting depth overflow".to_string())
            })?;
            if new_depth > MAX_VIEW_NESTING_DEPTH {
                return Err(CatalogError::ViewLimitExceeded(
                    "view nesting depth exceeds limit".to_string(),
                ));
            }
            let count = views_table.len()?;
            if count >= MAX_VIEWS as u64 {
                return Err(CatalogError::ViewLimitExceeded(
                    "too many views".to_string(),
                ));
            }
            views_table.insert(name, encoded.as_slice())?;
        }
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// `DROP VIEW <name>`（TABLE-18・SQL-23・TASK-205、Issue #909）。他のビューが
    /// `name` を参照している場合は `Err(DependentViewsExist)`（作り直しによる
    /// 循環構築を阻止する）。`name` がテーブルとして存在する場合は
    /// `Err(WrongObjectKind)`。いずれの名前空間にも存在しない場合は
    /// `Err(ViewNotFound)`。行ストア・世代カウンタは持たないため
    /// テーブル単位の世代 bump は行わない。
    pub fn drop_view(&self, name: &str) -> Result<()> {
        validate_identifier(name)?;
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        {
            let catalog_table = write_txn.open_table(CATALOG_TABLE)?;
            if catalog_table.get(name)?.is_some() {
                return Err(CatalogError::WrongObjectKind(name.to_string()));
            }
            // 索引名を指定された場合も種別不一致（`42809`。TASK-206・INDEX-7、
            // Issue #908）。
            if index_name_exists_in_txn(&write_txn, name)? {
                return Err(CatalogError::WrongObjectKind(name.to_string()));
            }
        }
        {
            let mut views_table = write_txn.open_table(VIEWS_TABLE)?;
            let dependents = views_depending_on_in_txn(&views_table, name)?;
            if !dependents.is_empty() {
                return Err(CatalogError::DependentViewsExist(name.to_string()));
            }
            if views_table.remove(name)?.is_none() {
                return Err(CatalogError::ViewNotFound(name.to_string()));
            }
        }
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// 索引を宣言する（`CREATE INDEX`。TASK-206・INDEX-7、Issue #908）。カタログ
    /// （[`INDEX_CATALOG_TABLE`]）のみを書き換え、行ストアには一切触れない（実行
    /// 時間は行数に依存しない）。`PolicyContext` を取らない全テナント共有の DDL で、
    /// SQL 表層からは `crate::sql::ddl::require_ddl_permission` を通過したセッション
    /// に限り `crate::sql::ddl::execute_create_index` 経由で到達する。
    ///
    /// 判定順序（同一 write txn 内。TOCTOU 回避。[`Storage::create_view`] と同じ
    /// 流儀）: 識別子・列数の形状検証（`Err(Invalid)`。txn 開始前）→ 索引名の
    /// relation 名前空間衝突（テーブル・ビュー・既存索引のいずれか。
    /// `Err(IndexAlreadyExists)`）→ 対象がビュー（`Err(WrongObjectKind)`）→ 対象
    /// テーブルの存在（`Err(TableNotFound)`）→ 列の存在・種別整合
    /// （[`validate_index_columns`]。`Err(ColumnNotFound)`／`Err(IndexKindMismatch)`）
    /// → 登録件数上限（`Err(IndexLimitExceeded)`）→ 保存。
    ///
    /// 成功時は対象テーブルの世代（[`bump_table_generation_in_txn`]）を進める。
    /// テーブル単位世代整合キャッシュ（`sql::scalar_index::ScalarIndexCache`・
    /// `sql::hnsw_cache::HnswIndexCache` 等）は次のクエリで自然に失効・再構築され、
    /// 宣言変更前のキャッシュが残り続ける経路を作らない（宣言列をキャッシュの
    /// 構築対象へ反映する結線自体は本メソッドの管轄外）。
    pub fn create_index(&self, def: &IndexDef) -> Result<()> {
        validate_identifier(&def.name)?;
        validate_identifier(&def.table)?;
        if def.columns.is_empty() {
            return Err(CatalogError::Invalid(
                "index must reference at least one column".to_string(),
            ));
        }
        if def.columns.len() > MAX_INDEX_DEF_COLUMNS {
            return Err(CatalogError::Invalid(format!(
                "index column count {} exceeds limit {MAX_INDEX_DEF_COLUMNS}",
                def.columns.len()
            )));
        }
        for c in &def.columns {
            validate_identifier(c)?;
        }
        // 同一列の重複指定は SQL 表層の構文検証（`validate_create_index_tokens`）と
        // 同じく `Invalid`（SQL 表層では `42601`）で拒否する。Rust API から直接
        // 渡された `IndexDef` も同じ不変条件を満たさない限り永続化しない
        // （[`decode_index_def`] は重複を破損として拒否するため、ここで通すと
        // 以後そのカタログ値を読めなくなる）。
        if let Some(dup) = first_duplicate_column(&def.columns) {
            return Err(CatalogError::Invalid(format!(
                "duplicate index column: {dup}"
            )));
        }
        let encoded = encode_index_def(def)?;
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        {
            let views_contains = |name: &str| -> Result<bool> {
                match write_txn.open_table(VIEWS_TABLE) {
                    Ok(t) => Ok(t.get(name)?.is_some()),
                    Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
                    Err(e) => Err(e.into()),
                }
            };
            let table_contains = |name: &str| -> Result<bool> {
                match write_txn.open_table(CATALOG_TABLE) {
                    Ok(t) => Ok(t.get(name)?.is_some()),
                    Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
                    Err(e) => Err(e.into()),
                }
            };
            if table_contains(&def.name)?
                || views_contains(&def.name)?
                || index_name_exists_in_txn(&write_txn, &def.name)?
            {
                return Err(CatalogError::IndexAlreadyExists(def.name.clone()));
            }
            // 対象がビュー・既存の索引（relation 名前空間を共有するが行を持たない）
            // なら種別不一致（`42809`）。テーブル不在（`42P01`）へ落とさない。
            if views_contains(&def.table)? || index_name_exists_in_txn(&write_txn, &def.table)? {
                return Err(CatalogError::WrongObjectKind(def.table.clone()));
            }
            let schema = require_table_schema_write(&write_txn, &def.table)?;
            validate_index_columns(def, &schema)?;
            let mut index_table = write_txn.open_table(INDEX_CATALOG_TABLE)?;
            if index_table.len()? >= MAX_INDEX_COUNT as u64 {
                return Err(CatalogError::IndexLimitExceeded(
                    "too many indexes".to_string(),
                ));
            }
            index_table.insert(def.name.as_str(), encoded.as_slice())?;
        }
        bump_table_generation_in_txn(&write_txn, &def.table)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// 索引宣言を削除する（`DROP INDEX`。TASK-206・INDEX-7、Issue #908）。名前が
    /// テーブル・ビューとして存在する場合は `Err(WrongObjectKind)`（`drop_view` に
    /// テーブル名を渡した場合と同じ扱い）、いずれとしても存在しない場合は
    /// `Err(IndexNotFound)`（冪等に `Ok` へ丸めない。fail-closed）。成功時は
    /// 対象テーブルの世代を進める（[`Storage::create_index`] と同じ理由）。
    pub fn drop_index(&self, name: &str) -> Result<()> {
        validate_identifier(name)?;
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        let removed = {
            match write_txn.open_table(INDEX_CATALOG_TABLE) {
                Ok(mut index_table) => {
                    let bytes = index_table
                        .remove(name)?
                        .map(|guard| guard.value().to_vec());
                    match bytes {
                        Some(bytes) => Some(decode_index_def(name, &bytes)?),
                        None => None,
                    }
                }
                Err(redb::TableError::TableDoesNotExist(_)) => None,
                Err(e) => return Err(e.into()),
            }
        };
        let Some(def) = removed else {
            for relation_table in [CATALOG_TABLE, VIEWS_TABLE] {
                match write_txn.open_table(relation_table) {
                    Ok(t) => {
                        if t.get(name)?.is_some() {
                            return Err(CatalogError::WrongObjectKind(name.to_string()));
                        }
                    }
                    Err(redb::TableError::TableDoesNotExist(_)) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            return Err(CatalogError::IndexNotFound(name.to_string()));
        };
        bump_table_generation_in_txn(&write_txn, &def.table)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// `name` が索引宣言として存在するかをスナップショット読み取りで判定する
    /// （TASK-206・INDEX-7、Issue #908）。テーブルとして存在しない名前について、
    /// SQL 表層が `42P01` ではなく種別不一致（`42809`）を返すべきかを判定する
    /// 呼び出し元（`sql::ddl` の `ALTER TABLE`・`core.rs` の書き込み系 DML）専用。
    pub(crate) fn index_exists(&self, name: &str) -> Result<bool> {
        let read_txn = self.db().begin_read()?;
        match read_txn.open_table(INDEX_CATALOG_TABLE) {
            Ok(t) => Ok(t.get(name)?.is_some()),
            Err(redb::TableError::TableDoesNotExist(_)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// 索引宣言を名前順に列挙する（TASK-206・INDEX-7、Issue #908。宣言の確認・
    /// テスト用の読み取り API。スナップショット読み取りで、件数は
    /// [`MAX_INDEX_COUNT`] で打ち切る）。
    pub fn list_indexes(&self) -> Result<Vec<IndexDef>> {
        let read_txn = self.db().begin_read()?;
        let table = match read_txn.open_table(INDEX_CATALOG_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in table.iter()? {
            if out.len() >= MAX_INDEX_COUNT {
                return Err(CatalogError::CorruptSchema(format!(
                    "index catalog exceeds {MAX_INDEX_COUNT} entries"
                )));
            }
            let (key, value) = entry?;
            out.push(decode_index_def(key.value(), value.value())?);
        }
        Ok(out)
    }

    /// ビュー定義をスナップショット読み取りで取得する（[`TableLookup::
    /// view_definition`] のバックエンド実装。存在しない場合は `Ok(None)`——
    /// テーブル名かどうかを問わず「ビューとしては見つからない」ことだけを表す
    /// fail-closed な戻り値で、呼び出し元〔`sql::view::resolve_from`〕が
    /// テーブルとしての存在確認を別途行う）。
    pub fn view_definition(&self, name: &str) -> Result<Option<ViewDef>> {
        let read_txn = self.db().begin_read()?;
        let table = match read_txn.open_table(VIEWS_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match table.get(name)? {
            Some(guard) => Ok(Some(decode_view_def(guard.value())?)),
            None => Ok(None),
        }
    }

    /// テーブル定義を読み出す（スナップショット読み取り）。存在しない場合は
    /// `Err(CatalogError::TableNotFound)`。
    pub fn get_table_schema(&self, table_name: &str) -> Result<TableSchema> {
        let read_txn = self.db().begin_read()?;
        get_table_schema_in_txn(&read_txn, table_name)
    }

    /// テーブル単位の世代（[`table_generation_in_txn`]）を新規 read トランザクション
    /// で読み直す（Issue #357・`sql/sparse_cache.rs::SparseIndexCache::insert`
    /// 専用の呼び出し口）。呼び出し元のクエリが使う `read_txn` とは別スナップショットを
    /// 意図的に開く: 挿入直前に他スレッドがコミットした書き込みを見逃さないため
    /// （`core.rs::PrefilterCache::insert` が `storage.current_generation()` を
    /// 同じ理由で挿入時に再読取するのと同じ方針）。
    pub(crate) fn table_generation(&self, table_name: &str) -> Result<u64> {
        let read_txn = self.db().begin_read()?;
        table_generation_in_txn(&read_txn, table_name)
    }

    /// 定義済みテーブル名の一覧をスナップショット読み取りで返す。件数上限
    /// （[`MAX_LIST_TABLES`]）を超える場合は `Err`（無制限 `Vec` 確保を防ぐ）。
    pub fn list_tables(&self) -> Result<Vec<String>> {
        let read_txn = self.db().begin_read()?;
        list_tables_in_txn(&read_txn)
    }

    /// テーブルスコープで 1 行挿入する（TASK-146、対象ビヘイビア: EXT-1, EXT-2）。
    ///
    /// カタログからのスキーマ取得・次元検証・行書き込みを単一の write トランザクション内で
    /// 行うことで、並行する DDL（`alter_table_add_column` 等）との整合を確保する。
    /// テーブル不存在・次元不一致は fail-closed に `Err` で拒否する（security.md
    /// 「不安全な設計」）。`VECTOR` 列を持たないテーブルは embedding が空（dim 0）の
    /// ときのみ受理し、非空 embedding は同様に `Err`（Issue #995・
    /// [`TableSchema::validate_row_embedding_dim`] 参照）。
    ///
    /// `pub(crate)`: 本メソッドはテナント境界チェック（`PolicyContext::is_owner`）を
    /// 一切行わない生の書き込み経路であり、クレート外（wire-server・結合テスト等）へ
    /// 公開するとテナント境界を完全に迂回できてしまう（codex-review P0 指摘・PR #194。
    /// security.md P0「テナント分離の検査を外す/緩める/バイパス経路を作らない」）。
    /// クレート外・テストからの新規行投入は [`crate::tenant::insert_row`]（テナント境界付き
    /// 書き込みガード。TASK-95・RECOVER-4）を経由すること。
    ///
    /// `#[cfg(test)]`: 現状の呼び出し元はすべて各モジュールの `#[cfg(test)]`
    /// ユニットテスト（`arena.rs`・`core.rs`・`rls.rs`・本ファイルの `tenant.rs`）のみ
    /// （Issue #1078。以前は `#[cfg_attr(not(test), allow(dead_code))]` で dead_code lint
    /// を黙殺していたが、`#[cfg(test)]` によりテスト専用であることをコンパイル構成で
    /// 保証する）。本メソッドは [`crate::constraint::enforce_row_constraints_in_txn`]
    /// （TABLE-16・TABLE-17 の検査点）を経由しないため、
    /// `reject_constrained_table_for_raw_write` が主キー・UNIQUE・CHECK・FOREIGN KEY
    /// のいずれかを宣言したテーブルへの書き込みを fail-closed に拒否する（制約違反状態を
    /// 意図せず作れてしまう経路を塞ぐ）。制約付きテーブルへの投入は
    /// [`crate::tenant::insert_row`]（検査点経由）を使うこと。
    #[cfg(test)]
    pub(crate) fn insert_row_into_table(
        &self,
        table_name: &str,
        id: u64,
        row: &RowInput<'_>,
    ) -> Result<()> {
        validate_identifier(table_name)?;
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        {
            let schema = require_table_schema_write(&write_txn, table_name)?;
            reject_constrained_table_for_raw_write(&schema)?;
            schema.validate_row_embedding_dim(row.embedding.len())?;
            let encoded = crate::storage::encode_row(row).map_err(convert_storage_error)?;
            let row_table_name = user_rows_table_name(table_name);
            let mut row_table = write_txn
                .open_table(user_rows_table_def(&row_table_name))
                .map_err(map_row_table_error)?;
            // 物理キーは `(tenant_id, id)`（TABLE-12）。テナント境界チェックを行わない
            // 生の経路のため、キーの名前空間は入力 `row.tenant_id` に従う
            // （認可済みの名前空間で書くのは `crate::tenant::insert_row` の責務）。
            row_table.insert((row.tenant_id, id), encoded.as_slice())?;
        }
        bump_table_generation_in_txn(&write_txn, table_name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// テーブルスコープで複数行を単一トランザクションで挿入する（TASK-146、対象ビヘイビア:
    /// EXT-1, EXT-2）。`insert_row_into_table` のバッチ版。
    /// 1 行でもスキーマ取得・次元検証・エンコードに失敗した場合、write トランザクションは
    /// commit されずに破棄されるため（`redb::WriteTransaction` の drop 契約）、全体が
    /// 未反映のまま拒否される。空スライスの場合も write トランザクション内でカタログ上の
    /// テーブル存在を確認してから成功を返す（レビュー指摘対応: `rows.is_empty()` を
    /// 存在確認より先に判定すると、存在しないテーブルへの空バッチ挿入が `Ok(())` になり
    /// 「テーブル不存在は fail-closed に `Err`」という契約を空バッチで迂回できてしまう）。
    ///
    /// `pub(crate)`（codex-review P0 指摘・PR #194 対応）: 本メソッドはテナント境界
    /// チェックを一切行わない生の書き込み経路で、クレート外へ公開すると任意の
    /// `tenant_id` 名義での書き込み・既存行の上書きが可能になる
    /// （security.md P0「テナント分離の検査を外す/緩める/バイパス経路を作らない」）。
    /// クレート外・テストからのバッチ投入は [`crate::tenant::insert_rows`]
    /// （`PolicyContext` 必須のガード付きバッチ API）を経由すること。
    ///
    /// `#[cfg(test)]`（Issue #1078）: [`Self::insert_row_into_table`] と同じ理由でテスト専用に
    /// 限定する。`reject_constrained_table_for_raw_write` による制約付きテーブルの
    /// fail-closed 拒否は、空バッチ（`rows.is_empty()` の早期 return）より前に行う
    /// （制約付きテーブルへの空バッチ投入だけを検査迂回の抜け道にしない）。
    #[cfg(test)]
    pub(crate) fn insert_rows_into_table(
        &self,
        table_name: &str,
        rows: &[(u64, RowInput<'_>)],
    ) -> Result<()> {
        validate_identifier(table_name)?;
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        {
            let schema = require_table_schema_write(&write_txn, table_name)?;
            reject_constrained_table_for_raw_write(&schema)?;
            if rows.is_empty() {
                // 存在確認以外に何も変更しないため、commit（＝世代を進める）せず
                // write txn を破棄する（`redb::WriteTransaction` は commit/abort の
                // どちらも呼ばずに drop すると自動的に abort される契約。TASK-133 P2
                // 対応: 空バッチだけで既存 `PrefilterIndex` を不要に失効させない）。
                // `storage.rs::Storage::put_batch` と同様、空バッチは行データに
                // 触れず即座に成功として扱う。
                drop(write_txn);
                return Ok(());
            }
            let row_table_name = user_rows_table_name(table_name);
            let mut row_table = write_txn
                .open_table(user_rows_table_def(&row_table_name))
                .map_err(map_row_table_error)?;
            // clear 再利用の 1 面スクラッチ（Issue #398。`tenant.rs`・
            // `storage.rs::Storage::put_batch` の同パターンと揃える）。
            let mut scratch: Vec<u8> = Vec::new();
            for (id, row) in rows {
                schema.validate_row_embedding_dim(row.embedding.len())?;
                scratch.clear();
                crate::storage::encode_row_into(&mut scratch, row)
                    .map_err(convert_storage_error)?;
                // 物理キーは `(tenant_id, id)`（TABLE-12。`insert_row_into_table` と同じ）。
                row_table.insert((row.tenant_id, *id), scratch.as_slice())?;
            }
        }
        bump_table_generation_in_txn(&write_txn, table_name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// スキーマ列順の型付き値列（[`row_codec::Value`]）から 1 行挿入する（TASK-75、
    /// 対象ビヘイビア: SQL-1〜4 の実行経路が `INSERT` 文を持たない本タスクで、
    /// 結合テスト（`tests/sql_surface.rs`）が決定的なコーパスを投入するための共通入口）。
    /// `values` は `schema.columns` の列順に対応させる。
    ///
    /// スキーマ取得・`VECTOR` 列の抽出・スカラーペイロード生成
    /// （[`row_codec::encode_scalar_columns`]）・行書き込みを単一の write トランザクション
    /// 内で行う（`insert_row_into_table` と同じ理由で並行 DDL との整合を確保する）。
    /// `VECTOR` 列を持つスキーマで対応する位置が `Value::Vector` でない場合は
    /// fail-closed に `Err`。`VECTOR` 列を持たないスキーマでは embedding を空
    /// として扱う（Issue #995）。
    ///
    /// `pub(crate)`（codex-review P0 指摘・PR #194 対応）: [`Self::insert_rows_into_table`]
    /// と同じ理由でクレート外へは公開しない（`tenant_id` を引数で受け取る生の経路）。
    /// クレート外・テストからの型付き行投入は [`crate::tenant::insert_typed_row`]
    /// （`PolicyContext` から `tenant_id` を導出するガード付き API）を経由すること。
    ///
    /// `#[cfg(test)]`（Issue #1078）: [`Self::insert_row_into_table`] と同じ理由でテスト
    /// 専用に限定し、`reject_constrained_table_for_raw_write` で制約付きテーブルへの
    /// 書き込みを fail-closed に拒否する。
    #[cfg(test)]
    pub(crate) fn insert_typed_row(
        &self,
        table_name: &str,
        id: u64,
        tenant_id: &str,
        visibility: Visibility,
        values: &[RowCodecValue],
    ) -> Result<()> {
        validate_identifier(table_name)?;
        let write_txn = self.begin_write_txn().map_err(convert_storage_error)?;
        {
            let schema = require_table_schema_write(&write_txn, table_name)?;
            reject_constrained_table_for_raw_write(&schema)?;
            let vector_idx = schema.columns.iter().position(|c| c.ty.is_vector());
            // Issue #995: `VECTOR` 列を持たないスキーマでは embedding を空のまま
            // 扱う（読み取り側の dim==0 モデルと整合）。列がある場合の「値が
            // 欠落・非 Vector なら拒否」という fail-closed 判定は変えない。
            let embedding = match vector_idx {
                Some(idx) => match values.get(idx) {
                    Some(RowCodecValue::Vector(v)) => v.clone(),
                    _ => {
                        return Err(CatalogError::Invalid(
                            "VECTOR column value missing or not a Vector".to_string(),
                        ))
                    }
                },
                None => Vec::new(),
            };
            schema.validate_row_embedding_dim(embedding.len())?;
            let metadata = row_codec::encode_scalar_columns(&schema, values)
                .map_err(|e| CatalogError::Invalid(e.to_string()))?;
            let row_input = RowInput {
                tenant_id,
                visibility,
                embedding: &embedding,
                metadata: &metadata,
            };
            let encoded = crate::storage::encode_row(&row_input).map_err(convert_storage_error)?;
            let row_table_name = user_rows_table_name(table_name);
            let mut row_table = write_txn
                .open_table(user_rows_table_def(&row_table_name))
                .map_err(map_row_table_error)?;
            // 物理キーは `(tenant_id, id)`（TABLE-12）。`tenant_id` は本 API の引数
            // （呼び出し元がテナントを明示する契約）。
            row_table.insert((tenant_id, id), encoded.as_slice())?;
        }
        bump_table_generation_in_txn(&write_txn, table_name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(convert_storage_error)
    }

    /// テーブルスコープで、指定テナントの名前空間から 1 行取得する（スナップショット
    /// 読み取り。TASK-146、対象ビヘイビア: EXT-1, EXT-2。物理キーは TABLE-12 の
    /// `(tenant_id, id)`）。他テーブル・他テナントの同一 `id` は見えない。
    ///
    /// `tenant_id` を引数で必須化しているのは TABLE-12 で行 `id` の一意性スコープが
    /// テナント内に閉じたためで、`id` 単独ではもはや行を一意に指せない。本メソッド自体は
    /// 認可を行わない生の取得経路であり（`tenant_id` は「どの名前空間を引くか」の指定に
    /// すぎない）、可視性判定は呼び出し元（`core.rs::EngineCore::get_row` →
    /// `PolicyContext::is_visible`）が行う。呼び出し元は不存在と不可視を区別せず
    /// `NotFound` に統一する契約のため、本 API 経由で他テナント行の存在を観測できる
    /// 公開経路は生まれない（fail-closed。RLS-9）。
    /// テーブル不存在・行不存在はいずれも fail-closed に `Err` を返す
    /// （エラー内容に他テーブル・他テナントの存在情報を含めない）。
    pub fn get_row_from_table(
        &self,
        table_name: &str,
        tenant_id: &str,
        id: u64,
    ) -> Result<StorageRow> {
        validate_identifier(table_name)?;
        let read_txn = self.db().begin_read()?;
        require_table_exists_read(&read_txn, table_name)?;
        let row_table_name = user_rows_table_name(table_name);
        let row_table = match read_txn.open_table(user_rows_table_def(&row_table_name)) {
            Ok(t) => t,
            // テーブルは定義済みだが 1 行も挿入していない（行テーブル自体が未作成）。
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(CatalogError::RowNotFound(id))
            }
            Err(e) => return Err(map_row_table_error(e)),
        };
        let guard = row_table
            .get(&(tenant_id, id))?
            .ok_or(CatalogError::RowNotFound(id))?;
        crate::storage::decode_row(id, guard.value()).map_err(convert_storage_error)
    }

    /// テーブルスコープで物理キー昇順（`(tenant_id, id)`。TABLE-12）に最大 `limit` 件を
    /// 走査する上限付きページング API（TASK-146、対象ビヘイビア: EXT-1, EXT-2）。
    /// `storage.rs::Storage::scan_page` と同じ行数上限（`MAX_SCAN_PAGE_LIMIT`）・
    /// バイト量上限（`MAX_SCAN_PAGE_BYTES`）を適用し、自テーブルの行のみを返す
    /// （他テーブルとの混線なし）。
    ///
    /// **走査範囲はテーブル全行**（テナントで絞らない）: 可視性判定は呼び出し元
    /// （`crate::tenant::visible_rows` → `PolicyContext::is_visible`）の単一照合パスに
    /// 委譲する契約を維持するため、本 API はテナント境界を判断しない。他テナントの
    /// `Public` 行も列挙対象に含まれる（`(tenant_id, id)` 順では連続範囲にならないため、
    /// テナント絞り込みでは可視集合を構成できない）。
    ///
    /// カーソルは物理キーと同じ `(tenant_id, id)` 形（`id` 単独では再開位置を表現できず、
    /// 行の取りこぼしになる）。打ち切り契約は `scan_page` と同一。
    pub fn scan_table_page(
        &self,
        table_name: &str,
        after: Option<(&str, u64)>,
        limit: u32,
    ) -> Result<RowPage> {
        validate_identifier(table_name)?;
        let limit = limit.min(crate::storage::MAX_SCAN_PAGE_LIMIT) as usize;

        let read_txn = self.db().begin_read()?;
        require_table_exists_read(&read_txn, table_name)?;
        // `limit == 0` の早期 return は存在確認より後に置く（レビュー指摘対応: 先に
        // 判定すると、存在しないテーブルへの limit=0 走査が空ページで成功してしまい、
        // 「テーブル不存在は fail-closed に `Err`」という契約を迂回できてしまう）。
        if limit == 0 {
            return Ok((Vec::new(), None));
        }
        let row_table_name = user_rows_table_name(table_name);
        let row_table = match read_txn.open_table(user_rows_table_def(&row_table_name)) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok((Vec::new(), None)),
            Err(e) => return Err(map_row_table_error(e)),
        };

        // カーソルの直後から走査を開始する（`Bound::Excluded`）。複合キーでは
        // 「次のキー」を算術で導出できないため、除外境界付き range で表現する。
        let range_start = match after {
            Some(cursor) => std::ops::Bound::Excluded(cursor),
            None => std::ops::Bound::Unbounded,
        };

        let mut out = Vec::new();
        let mut bytes_used: usize = 0;
        let mut capped = false;
        let mut last_key: Option<(String, u64)> = None;
        for entry in row_table.range::<(&str, u64)>((range_start, std::ops::Bound::Unbounded))? {
            if out.len() == limit {
                capped = true;
                break;
            }
            let (k, v) = entry?;
            let (tenant_id, id) = k.value();
            let raw = v.value();
            if !out.is_empty()
                && bytes_used.saturating_add(raw.len()) > crate::storage::MAX_SCAN_PAGE_BYTES
            {
                capped = true;
                break;
            }
            out.push(crate::storage::decode_row(id, raw).map_err(convert_storage_error)?);
            last_key = Some((tenant_id.to_string(), id));
            bytes_used = bytes_used.saturating_add(raw.len());
        }

        let cursor_for_next = if capped { last_key } else { None };
        Ok((out, cursor_for_next))
    }
}

/// `sql::allowlist::validate_statement`（TASK-74・SQL-8 参照）から FROM テーブルの
/// カタログ存在確認に使われる橋渡し実装。`get_table_schema` が返す `CatalogError`
/// を SQL 表層のエラー契約へ分類し直し、識別子形式不正のみ拒否側（構文エラー）へ
/// 倒す。それ以外（カタログ照会自体の失敗を含む）は受理側へ倒さず fail-closed に
/// エラー伝播する（`.claude/rules/security.md`「不安全な設計」対応）。格納済み
/// スキーマのデコード失敗は識別子形式不正と区別し、内部データ断片がエラー
/// メッセージ経由で漏れないよう汎用メッセージへ丸める（security.md「情報漏えい」対応）。
impl TableLookup for Storage {
    fn table_exists(&self, name: &str) -> std::result::Result<bool, SqlSurfaceError> {
        match self.get_table_schema(name) {
            Ok(_) => Ok(true),
            Err(CatalogError::TableNotFound(_)) => Ok(false),
            Err(other) => Err(table_lookup_error(other)),
        }
    }

    /// [`Storage::view_definition`]（本ファイル上部の inherent メソッド）へ
    /// 委譲し、`CatalogError` は [`table_lookup_error`] で SQL 表層の契約へ
    /// 写像する（TABLE-18・SQL-23・TASK-205、Issue #909）。
    fn view_definition(&self, name: &str) -> std::result::Result<Option<ViewDef>, SqlSurfaceError> {
        Storage::view_definition(self, name).map_err(table_lookup_error)
    }

    /// `sql::allowlist` の WITH 事前検証（どこからも参照されない leaf CTE）が
    /// 実テーブル直下の列存在を検査するために呼ぶ（Issue #928 レビュー指摘。
    /// `TableLookup::table_columns` のドキュメント参照）。`sql::parser::
    /// bind_projection` が受理する投影列名の集合と一致させる: 生存列（`ALTER
    /// TABLE DROP COLUMN` 済みの列は `get_table_schema` の時点で既に除外
    /// 済み）の実カラム名に加え、スキーマが実カラム `id` を宣言していなければ
    /// 疑似列 `id` を追加する（`bind_projection` は実カラム `id` を疑似列より
    /// 優先するため、二重に加えない）。テーブル不存在は `Ok(None)`（検査省略。
    /// 呼び出し元の事前検証はこの後 FROM 解決で `UndefinedTable` を返すため、
    /// ここで `None` を返しても fail-open にはならない）。
    fn table_columns(
        &self,
        name: &str,
    ) -> std::result::Result<Option<Vec<String>>, SqlSurfaceError> {
        match self.get_table_schema(name) {
            Ok(schema) => {
                let mut columns: Vec<String> =
                    schema.columns.iter().map(|c| c.name.clone()).collect();
                if !columns.iter().any(|c| c == "id") {
                    columns.push("id".to_string());
                }
                Ok(Some(columns))
            }
            Err(CatalogError::TableNotFound(_)) => Ok(None),
            Err(other) => Err(table_lookup_error(other)),
        }
    }
}

/// [`TableLookup::table_exists`]（`impl TableLookup for Storage`）と
/// `core.rs::InsertSchemaLookup`（Issue #485・単文 INSERT 経路のスキーマ取得
/// 一本化。1 read txn・1 decode で済ませる私的キャッシュ）が共有する
/// `CatalogError → SqlSurfaceError` の写像本体。両者は同じ `get_table_schema`
/// 系の呼び出しに対して同一のエラー文言・`wire_code` を返す契約を持つため、
/// ここへ抽出することで機械的に一致させる（写像がずれると security.md
/// P0「エラー経由で内部情報・存在情報を漏らさない」契約の検査対象が
/// 呼び出し箇所ごとに分裂してしまう）。`TableNotFound` はこの関数の対象外
/// （呼び出し元が `Ok(false)`／`UndefinedTable` へ個別に振り分ける）。
pub(crate) fn table_lookup_error(e: CatalogError) -> SqlSurfaceError {
    match e {
        CatalogError::Invalid(detail) => {
            SqlSurfaceError::unsupported(format!("malformed table reference: {detail}"))
        }
        CatalogError::TableNotFound(_)
        | CatalogError::Backend(_)
        | CatalogError::CorruptSchema(_)
        | CatalogError::TableAlreadyExists(_)
        | CatalogError::ColumnAlreadyExists(_)
        | CatalogError::RowNotFound(_)
        | CatalogError::IncompatibleRowKeyFormat
        | CatalogError::TableGenerationCounterOverflow
        | CatalogError::TypeNotFound(_)
        | CatalogError::TypeAlreadyExists(_)
        | CatalogError::DependentObjectsStillExist(_)
        // ビュー関連の variant は `table_exists`（テーブルカタログのみを引く）
        // からは構造的に到達しない（`view_definition` 経由のみで発生する）が、
        // `CatalogError` の網羅 `match` を保つため他の未接続 variant と同じ
        // `Internal` へ丸める（security.md「不安全な設計」対応）。
        | CatalogError::ViewNotFound(_)
        | CatalogError::WrongObjectKind(_)
        | CatalogError::DependentViewsExist(_)
        | CatalogError::ViewLimitExceeded(_)
        | CatalogError::ColumnNotFound(_)
        | CatalogError::ProtectedColumn(_)
        | CatalogError::IncompatibleTypeChange { .. }
        | CatalogError::UniqueConstraintViolation
        | CatalogError::TooManyColumns { .. }
        // 索引宣言（TASK-206・INDEX-7、Issue #908）の variant は
        // `Storage::create_index`／`drop_index` 専用で、テーブル存在確認からは
        // 到達しない（網羅性のため `Internal` へ丸める）。
        | CatalogError::IndexAlreadyExists(_)
        | CatalogError::IndexNotFound(_)
        | CatalogError::IndexKindMismatch(_)
        | CatalogError::IndexLimitExceeded(_)
        // `FOREIGN KEY` 宣言の照合（`create_table` 専用）はテーブル存在確認からは
        // 到達しない（網羅性のため `Internal` へ丸める）。
        | CatalogError::InvalidForeignKey(_)
        // 制約名の操作（`ALTER TABLE ... ADD/DROP CONSTRAINT`。Issue #1067）は
        // テーブル存在確認からは到達しない（網羅性のため `Internal` へ丸める）。
        | CatalogError::ConstraintAlreadyExists(_)
        | CatalogError::ConstraintNotFound(_)
        | CatalogError::ConstraintDropNotSupported(_)
        | CatalogError::ConstraintLimitExceeded(_)
        // `ALTER TABLE ... ADD FOREIGN KEY` の既存行検証違反（TABLE-22・
        // TASK-233、Issue #1069）はテーブル存在確認からは到達しない（網羅性の
        // ため `Internal` へ丸める）。
        | CatalogError::ForeignKeyViolation => SqlSurfaceError::Internal {
            detail: "catalog lookup failed".to_string(),
        },
        // 読み取り専用の存在確認（`table_exists`）は書き込みトランザクションを
        // 取得しないため通常は到達しないが、`CatalogError` の網羅性のためここでも
        // 扱う（SQL-31・TASK-221）。
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
    }
}

/// [`Storage::get_table_schema`]・[`crate::arena::VectorArena::build`]（TASK-87、
/// 対象ビヘイビア: TABLE-8）が共有するトランザクションスコープの実装本体。
/// `pub(crate)` で公開し、`arena.rs` が単一の `read_txn` 上でスキーマ取得と
/// テーブルスコープ行テーブル（[`user_rows_table_name`]）のオープンを同一
/// スナップショットで行えるようにする（TOCTOU 対策）。
pub(crate) fn get_table_schema_in_txn(
    read_txn: &redb::ReadTransaction,
    table_name: &str,
) -> Result<TableSchema> {
    // `alter_table_add_column` と同様、redb キーとして引く前に識別子を検証する。
    // 不正形式の名前は `TableNotFound`（存在しない）ではなく `Invalid`（形式不正）で
    // 拒否し、両 API 間でエラーバリアントを揃える。
    validate_identifier(table_name)?;
    let table = match read_txn.open_table(CATALOG_TABLE) {
        Ok(t) => t,
        // カタログテーブル未作成（1 テーブルも定義していない）は「存在しない」として扱う。
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Err(CatalogError::TableNotFound(table_name.to_string()))
        }
        Err(e) => return Err(e.into()),
    };
    let guard = table
        .get(table_name)?
        .ok_or_else(|| CatalogError::TableNotFound(table_name.to_string()))?;
    let bytes = guard.value().to_vec();
    drop(guard);
    let mut resolve = |name: &str| get_enum_type_in_read_txn(read_txn, name);
    decode_schema_with_resolver(table_name, &bytes, &mut resolve)
}

/// [`Storage::list_tables`] が共有するトランザクションスコープの実装本体。
fn list_tables_in_txn(read_txn: &redb::ReadTransaction) -> Result<Vec<String>> {
    let table = match read_txn.open_table(CATALOG_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut names = Vec::new();
    for entry in table.iter()? {
        let (key, _value) = entry?;
        if names.len() >= MAX_LIST_TABLES {
            return Err(CatalogError::Invalid(format!(
                "too many tables: exceeds {MAX_LIST_TABLES}"
            )));
        }
        let name = key.value();
        // `get_table_schema_in_txn` と同じ検証をここでも通す。通常経路で書かれるキーは
        // すべて `create_table` の `validate_schema` を経ているため常に合法だが、
        // 手書きの不正データが直接 redb へ書き込まれていた場合に、そのまま
        // 一覧へ紛れ込ませない（`decode_schema` と同じ fail-closed 方針）。
        validate_identifier(name)?;
        names.push(name.to_string());
    }
    Ok(names)
}

/// カタログ値（1 テーブル分のエンコード済みバイト列）が ENUM 型 `type_name` を
/// 参照する列を含むかどうかを、完全デコード（[`decode_schema_with_resolver`]）
/// を経由せずテキスト走査だけで判定する。`dependent_tables_in_txn` が
/// `DROP TYPE`／`ALTER TYPE ... ADD VALUE`（テーブル世代の保守的な同時進行）の
/// 双方から使う軽量な判定であり、完全なスキーマ復元・語彙解決は不要（`param`
/// の識別子形式検証・`nullable` フィールドの値検証までは行わない）。
/// ヘッダ行（フォーマットバージョン・列数）の形は [`decode_schema_body`] と
/// 同じ想定で読み飛ばし、宣言列数ぶんの列行のみを検査する。
/// 破損したカタログ値（UTF-8 でない・ヘッダ不正・列行のフィールド数不整合・
/// 宣言列数に満たない等）は fail-closed で `Err(CatalogError::CorruptSchema)`
/// として伝播する（codex-review P1 指摘・Issue #890: 破損を「依存なし」に
/// 丸めると `drop_enum_type` が実際には参照されている ENUM 型を削除できて
/// しまう。`decode_schema_body` と同じ fail-closed 方針をここでも徹底する）。
fn catalog_value_references_enum_type(bytes: &[u8], type_name: &str) -> Result<bool> {
    if bytes.len() > MAX_CATALOG_VALUE_LEN {
        return Err(CatalogError::CorruptSchema(format!(
            "catalog value too large: {} bytes",
            bytes.len()
        )));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|_| CatalogError::CorruptSchema("catalog value is not valid UTF-8".to_string()))?;
    let mut lines = text.split('\n');

    let version_line = lines
        .next()
        .ok_or_else(|| CatalogError::CorruptSchema("catalog value is empty".to_string()))?;
    // v3（TABLE-19・Issue #901）・v4（TABLE-16・TASK-204、Issue #903）・v5
    // （TABLE-16・TASK-204、Issue #904）は列行が 5 フィールド（末尾に
    // `state`。v5 はさらに 6 番目の `default` を持つ）になる以外は同じ枠組み。
    // 生存・削除済みいずれの行も保守的に依存判定の対象に含める
    // （`decode_schema_body` は削除済み列の型を必ず TEXT へ正規化するが、
    // ここは decode の完全な検証を経由しない軽量パーサーであり、fail-closed
    // に倒して依存を見落とさない）。v4／v5 は `cols:` 行の直後に `pk:` 行を
    // 追加で持つため、列行を読み始める前に 1 行読み飛ばす（内容の検証は
    // `decode_schema_body` に委ね、ここでは行の存在のみを要求する。
    // 存在しなければ他のフィールド数不整合と同じく `CorruptSchema` で
    // 拒否する）。
    let (has_state_field, has_default_field) = match version_line {
        CATALOG_FORMAT_VERSION_LINE => (false, false),
        CATALOG_FORMAT_VERSION_V3 => (true, false),
        CATALOG_FORMAT_VERSION_V4 => (true, false),
        CATALOG_FORMAT_VERSION_V5 => (true, true),
        // v6（TABLE-16・TASK-204、Issue #905）は v5 と同じ列行の後ろに
        // `uniq:` セクションを持つ（下記で検証する）。
        CATALOG_FORMAT_VERSION_V6 => (true, true),
        // v7（TABLE-16・TASK-204、Issue #906）は v6 の上位集合で、`uniq:`
        // セクション（0 件可）の後ろに `checks:` セクションを持つ。
        CATALOG_FORMAT_VERSION_V7 => (true, true),
        // v8（TABLE-17・TASK-205、Issue #907）は v7 の上位集合で、`checks:`
        // セクション（0 件可）の後ろに `fks:` セクションを持つ。
        CATALOG_FORMAT_VERSION_V8 => (true, true),
        // v9（TABLE-17・TASK-205、Issue #1076／#1077）は v8 の上位集合で、
        // `fks:` セクションの `fk:` 行のみ 7 フィールドへ拡張する（下記で検証する）。
        CATALOG_FORMAT_VERSION_V9 => (true, true),
        // v10（Issue #1067）は v8 の上位集合で、`U:` 行に制約名を持つ点のみが
        // 異なる（v9 とは独立）。
        CATALOG_FORMAT_VERSION_V10 => (true, true),
        // v11（Issue #1067・#1077 の統合）は v10 の上位集合で、`fks:` セクションの
        // `fk:` 行のみ 5 フィールドへ拡張する（下記で検証する）。
        CATALOG_FORMAT_VERSION_V11 => (true, true),
        // v12（設計 F4・Issue #1069）は v11 の上位集合で、`uniq:`／`checks:`
        // セクションが 0 件を許すようになり、`fks:` セクションの `fk:` 行が
        // 名前付き 8 フィールドへ拡張する（下記で検証する）。
        CATALOG_FORMAT_VERSION_V12 => (true, true),
        other => {
            return Err(CatalogError::CorruptSchema(format!(
                "unknown catalog format version: {other:?}"
            )))
        }
    };
    let is_v4 = version_line == CATALOG_FORMAT_VERSION_V4;
    let is_v5 = version_line == CATALOG_FORMAT_VERSION_V5;
    let is_v6 = version_line == CATALOG_FORMAT_VERSION_V6;
    let is_v7 = version_line == CATALOG_FORMAT_VERSION_V7;
    let is_v8 = version_line == CATALOG_FORMAT_VERSION_V8;
    let is_v9 = version_line == CATALOG_FORMAT_VERSION_V9;
    let is_v10 = version_line == CATALOG_FORMAT_VERSION_V10;
    let is_v11 = version_line == CATALOG_FORMAT_VERSION_V11;
    let is_v12 = version_line == CATALOG_FORMAT_VERSION_V12;
    let has_pk_line =
        is_v4 || is_v5 || is_v6 || is_v7 || is_v8 || is_v9 || is_v10 || is_v11 || is_v12;

    let cols_line = lines.next().ok_or_else(|| {
        CatalogError::CorruptSchema("catalog value truncated: missing cols line".to_string())
    })?;
    let count_str = cols_line.strip_prefix("cols:").ok_or_else(|| {
        CatalogError::CorruptSchema(format!("malformed cols line: {cols_line:?}"))
    })?;
    let col_count: usize = count_str.parse().map_err(|_| {
        CatalogError::CorruptSchema(format!("malformed column count: {count_str:?}"))
    })?;
    if col_count > MAX_COLUMN_COUNT {
        return Err(CatalogError::CorruptSchema(format!(
            "too many columns: {col_count}"
        )));
    }
    // v4／v5 の pk 行は `decode_schema_body`（構造検証）→ `validate_primary_key`
    // （重複列・未知列の検証。`validate_schema` 経由）の 2 段で検証される。
    // この軽量パーサーは列行を読む前に pk 行の構造だけを検証し（要素数上限・
    // 各要素の非空・識別子妥当性）、重複列・未知列の判定は列行を読み終えて
    // 実在する列名の集合が判明してから行う（codex-review P1 指摘・PR #1050:
    // 接頭辞と非空だけの軽量チェックでは、重複列・未知列・空要素等を含み
    // `decode_schema_body`／`validate_schema` なら `CorruptSchema` で拒否する
    // はずの壊れた v4 カタログでも、ENUM 参照列が無ければ「依存なし」に
    // 丸められ `drop_enum_type` の fail-closed 依存関係検査をすり抜けてしまう）。
    // v4 は非空必須、v5 は空を「主キーなし」として許容する
    // （`decode_schema_body` と同じ契約。TABLE-16・TASK-204、Issue #904）。
    let pk_cols: Option<Vec<String>> = if has_pk_line {
        let pk_line = lines.next().ok_or_else(|| {
            CatalogError::CorruptSchema("catalog value truncated: missing pk line".to_string())
        })?;
        let pk_body = pk_line.strip_prefix("pk:").ok_or_else(|| {
            CatalogError::CorruptSchema(format!("malformed pk line: {pk_line:?}"))
        })?;
        if pk_body.is_empty() {
            if is_v4 {
                return Err(CatalogError::CorruptSchema(
                    "v4 catalog format requires at least one primary key column".to_string(),
                ));
            }
            None
        } else {
            let mut cols: Vec<String> = Vec::new();
            for (i, name) in pk_body.split(',').enumerate() {
                if i >= MAX_PRIMARY_KEY_COLUMNS {
                    return Err(CatalogError::CorruptSchema(format!(
                        "too many primary key columns: exceeds {MAX_PRIMARY_KEY_COLUMNS}"
                    )));
                }
                if name.is_empty() {
                    return Err(CatalogError::CorruptSchema(format!(
                        "malformed pk line: {pk_line:?}"
                    )));
                }
                validate_identifier(name).map_err(|_| {
                    CatalogError::CorruptSchema(format!("malformed pk line: {pk_line:?}"))
                })?;
                cols.push(name.to_string());
            }
            Some(cols)
        }
    } else {
        None
    };

    let mut found = false;
    // v5 の「DEFAULT 0 件のカタログを拒否する」不変条件（`decode_schema_body`
    // 側。TABLE-16・TASK-204、Issue #904）をこの軽量パーサーでも敷く
    // （codex-review P1 指摘・Issue #904・PR #1051: v5 分岐が `default`
    // フィールドの存在確認のみで内容を検証しないと、DEFAULT を 1 件も
    // 持たない・不正なタグ／16 進文字列／型不一致の default を持つ壊れた
    // v5 カタログでも ENUM 参照だけを見て「依存なし」に丸められ、
    // `decode_schema_body` が `CorruptSchema` で拒否するはずの値に対して
    // `DROP TYPE`／`ALTER TYPE` の fail-closed 依存関係検査だけがすり抜けて
    // しまう）。
    let mut seen_default = false;
    let mut seen_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for _ in 0..col_count {
        let line = lines.next().ok_or_else(|| {
            CatalogError::CorruptSchema("catalog value truncated: missing column line".to_string())
        })?;
        let mut fields = line.split(':');
        let (Some(name), Some(tag), Some(param), Some(_nullable)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(CatalogError::CorruptSchema(format!(
                "malformed column line: {line:?}"
            )));
        };
        // state（v2 は常に生存扱い）を先に確定させてから重複検査を行う。
        // `validate_schema` の一意性検査は生存列同士のみが対象で墓標は対象外
        // （TABLE-19: 削除→再作成で同名列が墓標と生存列に共存しうる）ため、
        // ここでも生存列名だけを `seen_names` へ積む（対象を広げると正当な
        // v3/v4 カタログを誤って壊れた値扱いしてしまう）。
        let is_live = if has_state_field {
            // v3 の 5 番目フィールドは `state`（"L"＝生存／"D"＝削除済み）。
            // `decode_schema_body` の state 検証（"L"／"D" 以外は拒否）と同じ
            // 契約をここでも徹底する（codex-review P1 指摘・PR #1045: フィールドの
            // 存在だけを見て値を検証しないと、不正な state 値を持つ破損カタログ値が
            // 「依存なし」に丸められ `drop_enum_type` が破損カタログを残したまま
            // ENUM 型を削除できてしまう）。
            match fields.next() {
                Some("L") => true,
                Some("D") => false,
                Some(other) => {
                    return Err(CatalogError::CorruptSchema(format!(
                        "malformed column state field: {other:?}"
                    )))
                }
                None => {
                    return Err(CatalogError::CorruptSchema(format!(
                        "malformed column line: {line:?}"
                    )))
                }
            }
        } else {
            true
        };
        // v5 の 6 番目フィールドは `default`。この軽量パーサーは依存判定に
        // 値を直接使わないが、`decode_schema_body` と同じ fail-closed 方針を
        // 徹底するため内容も検証する（codex-review P1 指摘・Issue #904・
        // PR #1051）: 墓標（`is_live == false`）は必ず `-`、生存列は
        // `ColumnDefault::decode_catalog_field`（未知タグ・不正 16 進・不正
        // UTF-8 を拒否する既存デコーダ）でデコードしたうえ、列の型タグと
        // 大分類が整合すること（`ColumnDefault::compatible_with` の判定基準を
        // タグ文字列ベースで再現）を確認する。
        if has_default_field {
            let default_field = fields.next().ok_or_else(|| {
                CatalogError::CorruptSchema(format!("malformed column line: {line:?}"))
            })?;
            if is_live {
                let decoded = ColumnDefault::decode_catalog_field(default_field)
                    .map_err(|e| CatalogError::CorruptSchema(e.to_string()))?;
                if let Some(default) = decoded {
                    if !column_default_compatible_with_tag(&default, tag) {
                        return Err(CatalogError::CorruptSchema(format!(
                            "column {name:?} has a DEFAULT that is not compatible with its type"
                        )));
                    }
                    seen_default = true;
                }
            } else if default_field != "-" {
                return Err(CatalogError::CorruptSchema(format!(
                    "dropped column {name:?} must not declare a DEFAULT"
                )));
            }
        }
        if fields.next().is_some() {
            return Err(CatalogError::CorruptSchema(format!(
                "malformed column line: {line:?}"
            )));
        }
        // 列名の重複は `validate_schema`（生存列同士のみを対象とする一意性
        // 検証）が拒否する不変条件であり、この軽量パーサーでも同じ契約を
        // 敷いておかないと、ENUM 未参照列を重複させただけの壊れた v4/v3
        // カタログが「依存なし」に丸められうる（同種の fail-closed 徹底。
        // codex-review P1 指摘・PR #1050）。
        if is_live && !seen_names.insert(name) {
            return Err(CatalogError::CorruptSchema(format!(
                "duplicate column name: {name:?}"
            )));
        }
        if tag == "enum" && param == type_name {
            found = true;
        }
    }
    // v6 の `uniq:` セクション（TABLE-16・TASK-204、Issue #905）。
    // `decode_schema_body` と同じ共有パーサー [`parse_unique_section`] で
    // 構造を検証し（行数だけを読み飛ばすと、壊れた `uniq:` セクションを持つ
    // カタログが本関数だけ「依存なし」に丸められる）、参照列の実在は列行を
    // 読み終えた後に `pk:` 行と同じ手順で検証する。
    let unique_constraints: ParsedUniqueConstraints = if is_v6 || is_v7 || is_v8 || is_v9 {
        // v9（Issue #1077）は `FOREIGN KEY` のオプションの有無で選ばれる形式で
        // あり UNIQUE の有無とは独立なため、v7／v8 と同じく `n == 0` を許容する
        // （`named = false`。v9 の意味は既にリリース済みであり UNIQUE の実名は
        // 持たない。Issue #1147）。
        parse_unique_section(&mut lines, is_v7 || is_v8 || is_v9, false)
            .map_err(CatalogError::CorruptSchema)?
    } else if is_v10 || is_v11 {
        // v10（Issue #1067）・v11（Issue #1067・#1077 の統合）は `U:` 行に
        // 制約名を持つ（`named = true`）。この軽量パーサーは ENUM 依存判定には
        // 名前自体を使わないが、`validate_schema`（`decode_schema_body` 経由）が
        // 課す「UNIQUE 名と CHECK 名はテーブル単位の名前空間を共有する」不変条件
        // （設計 D1・Issue #1067）をここでも徹底するため名前を保持する
        // （codex-review P2 指摘・PR #1147: 名前を捨てると、`decode_schema_body`／
        // `validate_schema` が同名衝突として拒否するはずの壊れた v10／v11
        // カタログが本関数だけ「依存なし」に丸められ、ENUM 依存判定の
        // fail-closed 判定が `validate_schema` 側と食い違う）。
        parse_unique_section(&mut lines, false, true).map_err(CatalogError::CorruptSchema)?
    } else if is_v12 {
        // v12（設計 F4・Issue #1069）は名前付き FK の有無のみで選ばれる形式で
        // あり UNIQUE の有無とは独立なため、0 件を許容する（`allow_empty = true`）。
        parse_unique_section(&mut lines, true, true).map_err(CatalogError::CorruptSchema)?
    } else {
        Vec::new()
    };
    // v7 の `checks:` セクション（TABLE-16・TASK-204、Issue #906）。`CHECK` 述語は
    // 既存列の参照のみで ENUM 型への新たな依存を作らないが、行数だけを読み
    // 飛ばすと壊れたセクションを持つカタログが本関数だけ「依存なし」に丸め
    // られるため、`decode_schema_body` と同じ共有パーサーで構造を検証し、
    // 参照列の実在・制約名の一意性も下で検証する。
    let checks: Vec<CheckConstraint> = if is_v7 || is_v8 || is_v9 || is_v10 || is_v11 || is_v12 {
        parse_check_section(&mut lines, is_v8 || is_v9 || is_v10 || is_v11 || is_v12)
            .map_err(CatalogError::CorruptSchema)?
    } else {
        Vec::new()
    };
    // v8／v9／v10／v11 の `fks:` セクション（TABLE-17・TASK-205、Issue #907／
    // #1077／#1067）。`FOREIGN KEY` は ENUM 型への新たな依存を作らないが、
    // `checks:` と同じ理由（壊れたセクションを本関数だけが「依存なし」に丸め
    // ない）で共有パーサーの構造検証を通し、参照元列の実在も下で検証する。v9・
    // v11 は `MATCH`／遅延属性オプションが選択理由のため `k >= 1` 必須・
    // `has_options = true`（`fk:` 行 5 フィールド）で読み、v10 は UNIQUE の実名の
    // 有無で選ばれる形式のため FK 0 件を許容する。全 FK が既定オプションの
    // v9／v11 カタログ値（正規形の一意性違反）も同じ共有パーサーが fail-closed に
    // 拒否する。
    let foreign_keys: Vec<ForeignKeyDef> = if is_v8 {
        parse_foreign_key_section(&mut lines, false, false).map_err(CatalogError::CorruptSchema)?
    } else if is_v9 {
        parse_foreign_key_section(&mut lines, false, true).map_err(CatalogError::CorruptSchema)?
    } else if is_v10 {
        parse_foreign_key_section(&mut lines, true, false).map_err(CatalogError::CorruptSchema)?
    } else if is_v11 {
        parse_foreign_key_section(&mut lines, false, true).map_err(CatalogError::CorruptSchema)?
    } else if is_v12 {
        parse_foreign_key_section_v12(&mut lines).map_err(CatalogError::CorruptSchema)?
    } else {
        Vec::new()
    };

    // 宣言列数を超える残り行は、`decode_schema_body` と同じく「末尾の空行
    // （トレーリング改行）1 行のみ」を許容しそれ以外は余剰行として拒否する。
    // ここを緩めると、宣言列数を過小に偽装した破損値（例: `cols:1` の後に
    // 実在の ENUM 参照列をもう 1 行追加する）が「依存なし」に丸められ、
    // `decode_schema_body` が `CorruptSchema` で拒否する同じバイト列に対して
    // `drop_enum_type` だけが削除を許してしまう fail-open 経路になる。
    let mut trailing_seen = false;
    for line in lines {
        if trailing_seen || !line.is_empty() {
            return Err(CatalogError::CorruptSchema(format!(
                "catalog value line count mismatch: expected {col_count} columns, got more than {col_count} lines"
            )));
        }
        trailing_seen = true;
    }

    // pk 行の参照整合性は生存列名の集合が確定した後でしか判定できないため、
    // 列行を読み終えた後にまとめて検証する（`validate_primary_key` の
    // 「重複なし・生存列に存在する」契約と同じ。codex-review P1 指摘・
    // PR #1050）。
    if let Some(pk_cols) = pk_cols {
        let mut pk_seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for name in &pk_cols {
            if !pk_seen.insert(name.as_str()) {
                return Err(CatalogError::CorruptSchema(format!(
                    "duplicate primary key column: {name:?}"
                )));
            }
            if !seen_names.contains(name.as_str()) {
                return Err(CatalogError::CorruptSchema(format!(
                    "primary key references unknown column: {name:?}"
                )));
            }
        }
    }

    // v5 なのに DEFAULT 0 件は形式の一意性に反する（`decode_schema_body` と
    // 同じ不変条件。TABLE-16・TASK-204、Issue #904）。ここで拒否しないと、
    // この不変条件に違反する壊れた v5 カタログが本関数だけ「依存なし」を
    // 返し続ける（codex-review P1 指摘・Issue #904・PR #1051）。
    // v6 は UNIQUE 制約の有無で選ばれる形式であり DEFAULT 0 件でも正当なため、
    // この不変条件は v5 に限る（`decode_schema_body` と同じ）。
    if is_v5 && !seen_default {
        return Err(CatalogError::CorruptSchema(
            "v5 catalog format requires at least one column with a DEFAULT".to_string(),
        ));
    }

    // UNIQUE 制約の参照整合性（`validate_unique_constraints` の「生存列に
    // 存在する」契約と同じ）。構造（件数・重複）は `parse_unique_section` で
    // 検証済み。
    for (_, constraint) in &unique_constraints {
        for name in constraint {
            if !seen_names.contains(name.as_str()) {
                return Err(CatalogError::CorruptSchema(format!(
                    "unique constraint references unknown column: {name:?}"
                )));
            }
        }
    }

    // `CHECK` 制約の参照整合性・制約名の一意性（`validate_check_constraints` と
    // 同じ契約。TABLE-16・TASK-204、Issue #906）。
    let mut check_names: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for check in &checks {
        if !check_names.insert(check.name.as_str()) {
            return Err(CatalogError::CorruptSchema(format!(
                "duplicate CHECK constraint name: {:?}",
                check.name
            )));
        }
        for name in &check.columns {
            if !seen_names.contains(name.as_str()) {
                return Err(CatalogError::CorruptSchema(format!(
                    "CHECK constraint references unknown column: {name:?}"
                )));
            }
        }
    }

    // UNIQUE 制約名と CHECK 制約名はテーブル単位の名前空間を共有する
    // （`validate_schema` と同じ契約。設計 D1・Issue #1067）。v10／v11 の
    // `unique_constraints` は名前を持つため、ここで CHECK 名との衝突も検査する
    // （codex-review P2 指摘・PR #1147: この検査を欠くと、`decode_schema_body`／
    // `validate_schema` が同名衝突として拒否するはずの壊れた v10／v11 カタログが
    // 本関数だけ「依存なし」に丸められ、ENUM 依存判定の fail-closed 判定が
    // `validate_schema` 側と食い違う）。
    for (name, _) in &unique_constraints {
        if let Some(name) = name {
            if check_names.contains(name.as_str()) {
                return Err(CatalogError::CorruptSchema(format!(
                    "constraint name {name:?} is already used by a CHECK constraint on this table"
                )));
            }
        }
    }

    // `FOREIGN KEY` の参照元列の参照整合性（`validate_foreign_keys` の「生存列に
    // 存在する」契約と同じ。TABLE-17・TASK-205、Issue #907）。
    for fk in &foreign_keys {
        for name in fk.columns() {
            if !seen_names.contains(name.as_str()) {
                return Err(CatalogError::CorruptSchema(format!(
                    "foreign key references unknown column: {name:?}"
                )));
            }
        }
    }

    // `FOREIGN KEY` 名も UNIQUE・CHECK と同じ名前空間を共有する（設計 F1・
    // Issue #1069）。v12 の `foreign_keys` は名前を持つため、ここで FK 同士・
    // CHECK・UNIQUE との衝突も検査する（`validate_schema` と同じ fail-closed
    // 判定を本関数でも徹底する。codex-review P2 指摘・PR #1147 と同じ理由）。
    let mut fk_names_seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for fk in &foreign_keys {
        if fk.name().is_empty() {
            continue;
        }
        if !fk_names_seen.insert(fk.name()) {
            return Err(CatalogError::CorruptSchema(format!(
                "duplicate foreign key constraint name: {:?}",
                fk.name()
            )));
        }
        if check_names.contains(fk.name()) {
            return Err(CatalogError::CorruptSchema(format!(
                "constraint name {:?} is already used by a CHECK constraint on this table",
                fk.name()
            )));
        }
        if unique_constraints
            .iter()
            .any(|(name, _)| name.as_deref() == Some(fk.name()))
        {
            return Err(CatalogError::CorruptSchema(format!(
                "constraint name {:?} is already used by a UNIQUE constraint on this table",
                fk.name()
            )));
        }
    }

    Ok(found)
}

/// [`catalog_value_references_enum_type`] が `default` フィールドの内容検証に
/// 使う軽量な型整合判定。`ColumnDefault::compatible_with` と同じ大分類の
/// 組み合わせを、完全な `ColumnType` を構築せず列タグ文字列（`catalog_fields`
/// が生成する型タグ。TABLE-16 D2）だけで再現する（ENUM 解決・数値パラメータの
/// パースを要しない軽量パーサーとしての設計を維持するため）。
fn column_default_compatible_with_tag(default: &ColumnDefault, tag: &str) -> bool {
    matches!(
        (default, tag),
        (ColumnDefault::Text(_), "text")
            | (
                ColumnDefault::Number(_),
                "integer" | "bigint" | "real" | "double" | "numeric"
            )
            | (ColumnDefault::Bool(_), "boolean")
    )
}

/// ENUM 型 `type_name` を参照する列を持つテーブル名の一覧を列挙する
/// （`DROP TYPE`・`ALTER TYPE ... ADD VALUE` が共有する。Issue #890 D4/D5）。
/// [`MAX_LIST_TABLES`] を超える場合は無制限 `Vec` 確保を避けて `Err`。
fn dependent_tables_in_txn(
    write_txn: &redb::WriteTransaction,
    type_name: &str,
) -> Result<Vec<String>> {
    let table = match write_txn.open_table(CATALOG_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut dependents = Vec::new();
    for entry in table.iter()? {
        let (key, value) = entry?;
        if dependents.len() >= MAX_LIST_TABLES {
            return Err(CatalogError::Invalid(format!(
                "too many tables: exceeds {MAX_LIST_TABLES}"
            )));
        }
        if catalog_value_references_enum_type(value.value(), type_name)? {
            dependents.push(key.value().to_string());
        }
    }
    Ok(dependents)
}

/// `create_table` の write txn 内で、`schema` の各 `FOREIGN KEY` 宣言の参照先を
/// 解決・照合した結果のスキーマを返す（TABLE-17・TASK-205、Issue #907）。
/// 自己参照は `schema` 自身を参照先とする。参照先名がビュー・索引名なら
/// `WrongObjectKind`（`42809`。テーブル・ビュー・索引は名前空間を共有する）、
/// どれでもなければ `TableNotFound`（`42P01`）。呼び出し元は `CATALOG_TABLE` の
/// ハンドルを保持していない状態で呼ぶこと（参照先スキーマの decode で開き直す）。
fn resolve_foreign_keys_in_txn(
    write_txn: &redb::WriteTransaction,
    schema: &TableSchema,
) -> Result<TableSchema> {
    let mut resolved = Vec::with_capacity(schema.foreign_keys.len());
    for fk in &schema.foreign_keys {
        let owned_parent;
        let parent: &TableSchema = if fk.parent_table == schema.name {
            schema
        } else {
            match write_txn.open_table(VIEWS_TABLE) {
                Ok(views_table) => {
                    if views_table.get(fk.parent_table.as_str())?.is_some() {
                        return Err(CatalogError::WrongObjectKind(fk.parent_table.clone()));
                    }
                }
                Err(redb::TableError::TableDoesNotExist(_)) => {}
                Err(e) => return Err(CatalogError::from(e)),
            }
            if index_name_exists_in_txn(write_txn, fk.parent_table.as_str())? {
                return Err(CatalogError::WrongObjectKind(fk.parent_table.clone()));
            }
            owned_parent = require_table_schema_write(write_txn, &fk.parent_table)?;
            &owned_parent
        };
        resolved.push(resolve_foreign_key_target(schema, fk, parent)?);
    }
    Ok(schema.clone().with_foreign_keys(resolved))
}

/// テーブル `parent_table` を参照先とする `FOREIGN KEY` 宣言を、宣言を持つ
/// スキーマ（参照元。自己参照ではこのテーブル自身）と組にして列挙する
/// （TABLE-17・TASK-205、Issue #907）。`DROP TABLE` の依存検査（`2BP01`）と、
/// 参照先側の書き込み後検査（`constraint::enforce_referencing_rows_in_txn`）が使う。
///
/// `CATALOG_TABLE` を全走査するが、`FOREIGN KEY` を持つスキーマは必ず v8・v9
/// （TABLE-17・TASK-205、Issue #1077。既定以外の `MATCH`・遅延属性を 1 件でも
/// 持つ場合）・v10（TABLE-16・TASK-204、Issue #1067。UNIQUE の実名を持つ場合）・
/// v11（Issue #1067・#1077 の統合）のいずれかで永続化される（v2〜v11 は互いに
/// 排他な正規形）ため、値の 1 行目が v8・v9・v10・v11 のいずれかのエントリだけを
/// decode する（大多数のテーブルは先頭バイトの比較のみで済む）。**P0**:
/// v9／v10／v11 をここで見落とすと、それらの子テーブルに対して参照先側の検査
/// （`constraint::enforce_referencing_rows_in_txn` 経由の DELETE／TRUNCATE／
/// キー UPDATE）と `DROP TABLE` の依存検査（`2BP01`）が効かなくなる fail-open
/// 経路になる（advisor 指摘・codex-review 指摘・Issue #1077）。
/// [`MAX_LIST_TABLES`] を超える v8／v9／v10／v11 エントリは無制限 `Vec` 確保を
/// 避けて `Err`。呼び出し元は `CATALOG_TABLE` のハンドルを保持していない状態で
/// 呼ぶこと。
pub(crate) fn referencing_foreign_keys_in_txn(
    write_txn: &redb::WriteTransaction,
    parent_table: &str,
) -> Result<Vec<(TableSchema, ForeignKeyDef)>> {
    // FK を持ちうる全版の接頭辞（[`FK_BEARING_FORMAT_VERSIONS`]）を候補にする。
    // 新しい FK 保持版（v12 等）を追加した際にここを見落とすと、`DROP TABLE` の
    // `2BP01` 判定・参照先側の書き込み検査（`constraint::enforce_referencing_rows_in_txn`）
    // の双方が fail-open になる（advisor 指摘・codex-review 指摘）。
    let prefixes: Vec<Vec<u8>> = FK_BEARING_FORMAT_VERSIONS
        .iter()
        .map(|v| format!("{v}\n").into_bytes())
        .collect();
    let candidates: Vec<(String, Vec<u8>)> = {
        let table = match write_txn.open_table(CATALOG_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut candidates = Vec::new();
        for entry in table.iter()? {
            let (key, value) = entry?;
            if !prefixes.iter().any(|p| value.value().starts_with(p)) {
                continue;
            }
            if candidates.len() >= MAX_LIST_TABLES {
                return Err(CatalogError::Invalid(format!(
                    "too many tables: exceeds {MAX_LIST_TABLES}"
                )));
            }
            candidates.push((key.value().to_string(), value.value().to_vec()));
        }
        candidates
    };
    let mut referencing = Vec::new();
    for (name, bytes) in candidates {
        let mut resolve = |type_name: &str| get_enum_type_in_write_txn(write_txn, type_name);
        let schema = decode_schema_with_resolver(&name, &bytes, &mut resolve)?;
        for fk in &schema.foreign_keys {
            if fk.parent_table == parent_table {
                referencing.push((schema.clone(), fk.clone()));
            }
        }
    }
    Ok(referencing)
}

/// テーブル `table` を親とする各 `FOREIGN KEY`（自己参照を含む。`id` 参照は
/// 除く——`key_index.rs` は `id` 参照に索引を使わず物理キーの点照会で検査する
/// ため対象外）の参照先列集合から、`table` が本当に持つべき永続キー索引名の
/// 集合を求める（P0・Issue #1069。設計 §3.4「索引衛生」）。
///
/// `key_index.rs` の索引は常に「参照先（親）テーブル・参照先列」を基準に
/// 構築される（`constraint::prepare_referenced_key_indexes_in_txn`・
/// `constraint::verify_required_parent_keys` がいずれも `ensure_index_in_txn` を
/// `table = fk.parent_table()` で呼ぶ契約）ため、この関数は
/// [`referencing_foreign_keys_in_txn`] が返す「`table` を参照する FK」だけを
/// 見れば済む（`table` 自身の FK が持つ参照元列は索引の対象にならない）。
///
/// [`Storage::alter_table_drop_constraint`]（UNIQUE・FOREIGN KEY いずれの削除
/// でも、削除後のカタログに対して呼ぶ）・[`Storage::alter_table_add_foreign_key`]
/// （新 FK の検証**前**に、まだ新 FK を含まない現在のカタログに対して呼ぶ）が
/// 使う。呼び出し元は `CATALOG_TABLE` のハンドルを保持していない状態で呼ぶこと
/// （[`referencing_foreign_keys_in_txn`] と同じ契約）。
pub(crate) fn required_parent_key_index_names_in_txn(
    write_txn: &redb::WriteTransaction,
    table: &str,
) -> Result<std::collections::BTreeSet<String>> {
    let referencing = referencing_foreign_keys_in_txn(write_txn, table)?;
    Ok(referencing
        .iter()
        .filter(|(_, fk)| !fk.references_parent_id())
        .map(|(_, fk)| crate::key_index::index_name_for_columns(fk.parent_columns()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
    // `crate::test_util::temp_db` へ一本化した（旧: このモジュール内の複製）。
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    // --- 索引宣言（TASK-206・INDEX-7、Issue #908） --------------------------

    fn index_fixture_storage(label: &str) -> (Storage, CleanupGuard) {
        let path = unique_db_path(label);
        let guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("lang", ColumnType::Text, false),
                    ColumnDef::new("flag", ColumnType::Boolean, true),
                ],
            ))
            .expect("create docs");
        storage
            .create_table(&TableSchema::new(
                "sibling",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create sibling");
        (storage, guard)
    }

    fn scalar_def(name: &str, table: &str, cols: &[&str]) -> IndexDef {
        IndexDef::new(
            name.to_string(),
            table.to_string(),
            IndexKind::Scalar,
            cols.iter().map(|c| c.to_string()).collect(),
        )
    }

    /// 索引宣言の作成・削除は対象テーブルの世代だけを進め（テーブル単位世代整合
    /// キャッシュの失効源泉）、無関係なテーブルの世代は変えない。失敗した DDL は
    /// commit しないため世代を進めない。
    #[test]
    fn create_and_drop_index_bump_only_target_table_generation() {
        let (storage, _guard) = index_fixture_storage("index-ddl-generation");
        let read_gen = |name: &str| -> u64 {
            let read_txn = storage.db().begin_read().expect("begin read");
            table_generation_in_txn(&read_txn, name).expect("read table generation")
        };
        let (docs0, sibling0) = (read_gen("docs"), read_gen("sibling"));

        storage
            .create_index(&scalar_def("idx_lang", "docs", &["lang"]))
            .expect("create index");
        let docs1 = read_gen("docs");
        assert!(
            docs1 > docs0,
            "create_index must bump the target table generation"
        );
        assert_eq!(read_gen("sibling"), sibling0);

        // 失敗（重複名）は commit せず世代を進めない。
        assert!(matches!(
            storage.create_index(&scalar_def("idx_lang", "docs", &["lang"])),
            Err(CatalogError::IndexAlreadyExists(_))
        ));
        assert_eq!(read_gen("docs"), docs1);

        storage.drop_index("idx_lang").expect("drop index");
        assert!(
            read_gen("docs") > docs1,
            "drop_index must bump the target table generation"
        );
        assert_eq!(read_gen("sibling"), sibling0);
    }

    fn hnsw_def(name: &str, table: &str) -> IndexDef {
        IndexDef::new(
            name.to_string(),
            table.to_string(),
            IndexKind::Hnsw,
            vec!["embedding".to_string()],
        )
    }

    /// デコード不能な値を索引カタログへ直接挿入し、HNSW 適格性ゲートの
    /// カタログ走査失敗を再現するテスト補助（Issue #1065。`decode_index_def`
    /// の破損検出はテスト `index_def_round_trips_and_rejects_corruption` で
    /// 別途固定済み）。
    fn insert_corrupt_index_entry(storage: &Storage) {
        let write_txn = storage.db().begin_write().expect("begin write");
        {
            let mut index_table = write_txn
                .open_table(INDEX_CATALOG_TABLE)
                .expect("open index catalog table");
            index_table
                .insert("idx_corrupt", &b"\xff\xfe"[..])
                .expect("insert corrupt entry");
        }
        // 本番の書き込み経路と同じくストレージ全体世代を進める（進めないと
        // `IndexCatalogGateCache` が破損前の検証成功を同一世代として再利用する。
        // 本番の索引カタログ変更経路はいずれも commit 前に世代を進める）。
        crate::storage::prepare_generation_bump(&write_txn).expect("bump generation");
        write_txn.commit().expect("commit corrupt entry");
    }

    use crate::search_engine::HnswScope;

    /// `storage` の最新スナップショットで `hnsw_targeted_in_txn` を 1 回評価する
    /// テスト補助（HNSW opt-in 有効〔`hnsw_available == true`〕前提）。
    fn targeted_now(
        storage: &Storage,
        gate_cache: &IndexCatalogGateCache,
        table: &str,
        scope: HnswScope,
    ) -> bool {
        let read_txn = storage.db().begin_read().expect("begin read");
        hnsw_targeted_in_txn(&read_txn, gate_cache, table, scope, true)
    }

    /// `HnswScope::All`（既定）の HNSW 適格性ゲートが、`USING hnsw` 宣言の追加・
    /// 削除で判定を変えない（宣言テーブル・他テーブルとも常に `true`）ことを
    /// 固定する（Issue #1065 の結果不変の受け入れ条件。旧カタログ全体単位
    /// ゲートでは `docs` への宣言で未宣言の `sibling` が `false` へ切り替わって
    /// いた回帰の防止）。同一の [`IndexCatalogGateCache`] を複数世代で使い回しても、
    /// 新しいキャッシュで評価した結果と一致することも併せて固定する。
    #[test]
    fn hnsw_targeted_in_txn_scope_all_is_unaffected_by_declarations() {
        let (storage, _guard) = index_fixture_storage("index-gate-scope-all");
        let gate_cache = IndexCatalogGateCache::new();
        let check_all_true = |label: &str| {
            for table in ["docs", "sibling"] {
                assert!(
                    targeted_now(&storage, &gate_cache, table, HnswScope::All),
                    "{label}: {table} must stay on hnsw under scope all"
                );
                assert!(
                    targeted_now(
                        &storage,
                        &IndexCatalogGateCache::new(),
                        table,
                        HnswScope::All
                    ),
                    "{label}: uncached result must match for {table}"
                );
            }
        };

        check_all_true("no declarations");
        storage
            .create_index(&hnsw_def("idx_hnsw_docs", "docs"))
            .expect("create hnsw index");
        check_all_true("after declaring docs");
        storage.drop_index("idx_hnsw_docs").expect("drop index");
        check_all_true("after dropping the declaration");

        // opt-in 無効ならカタログの状態・scope によらず常に `false`。
        let read_txn = storage.db().begin_read().expect("begin read");
        for scope in [HnswScope::All, HnswScope::Declared] {
            assert!(!hnsw_targeted_in_txn(
                &read_txn,
                &gate_cache,
                "docs",
                scope,
                false
            ));
        }
        assert_eq!(gate_cache.stats().gate_read_failures, 0);
    }

    /// `HnswScope::Declared` の HNSW 適格性ゲートが、`USING hnsw` を宣言した
    /// テーブルだけ `true` にし、未宣言テーブルは `false`（厳密）のまま、
    /// `DROP INDEX` で `false` へ戻ることを固定する（Issue #1065・オーナー判断
    /// 2026-09-28）。`docs` への宣言が `sibling` の判定を変えないこと（テーブル
    /// 単位）と、同一キャッシュを世代を跨いで使い回しても新しい世代の宣言を
    /// 取りこぼさないことも併せて固定する。
    #[test]
    fn hnsw_targeted_in_txn_scope_declared_follows_only_own_table_declaration() {
        let (storage, _guard) = index_fixture_storage("index-gate-scope-declared");
        let gate_cache = IndexCatalogGateCache::new();
        let t = |table: &str| targeted_now(&storage, &gate_cache, table, HnswScope::Declared);

        assert!(!t("docs"));
        assert!(!t("sibling"));

        storage
            .create_index(&hnsw_def("idx_hnsw_docs", "docs"))
            .expect("create hnsw index");
        assert!(
            t("docs"),
            "declared table must use hnsw under scope declared"
        );
        assert!(
            !t("sibling"),
            "a declaration on docs must not affect sibling"
        );

        storage
            .create_index(&hnsw_def("idx_hnsw_sibling", "sibling"))
            .expect("create hnsw index on sibling");
        assert!(t("docs"));
        assert!(t("sibling"));

        storage.drop_index("idx_hnsw_docs").expect("drop index");
        assert!(!t("docs"), "drop index must return docs to exact search");
        assert!(
            t("sibling"),
            "dropping docs' declaration must not affect sibling"
        );
        assert_eq!(gate_cache.stats().gate_read_failures, 0);
    }

    /// [`hnsw_targeted_in_txn`] がカタログ読み取り失敗（デコード失敗）で
    /// いずれの `scope` でも fail-closed 縮退（`false`）し、その回数を
    /// [`IndexCatalogGateCache::stats`] の `gate_read_failures` に計上することを
    /// 固定する（codex-review Low 指摘対応・Issue #1065）。
    #[test]
    fn hnsw_targeted_in_txn_records_gate_read_failure_on_corrupt_catalog_entry() {
        let (storage, _guard) = index_fixture_storage("index-gate-cache-read-failure");
        let gate_cache = IndexCatalogGateCache::new();
        storage
            .create_index(&hnsw_def("idx_hnsw_docs", "docs"))
            .expect("create hnsw index");

        // 破損前に一度成功をキャッシュさせ、破損後の世代でキャッシュが誤って
        // 再利用されない（新しい世代で再走査して失敗する）ことも確認する。
        assert!(targeted_now(
            &storage,
            &gate_cache,
            "docs",
            HnswScope::Declared
        ));
        insert_corrupt_index_entry(&storage);

        assert_eq!(gate_cache.stats().gate_read_failures, 0);
        assert!(!targeted_now(
            &storage,
            &gate_cache,
            "docs",
            HnswScope::Declared
        ));
        assert_eq!(gate_cache.stats().gate_read_failures, 1);

        // 縮退は fail-closed のたびに計上される（キャッシュへは書き込まれないため
        // 毎回走査を再試行し、そのたびに失敗が増える）。`All` でも同様に縮退する。
        assert!(!targeted_now(
            &storage,
            &gate_cache,
            "sibling",
            HnswScope::All
        ));
        assert_eq!(gate_cache.stats().gate_read_failures, 2);
    }

    /// クエリが使っているのと同一の `read_txn`（スナップショット）を先に開いてから
    /// 別の書き込み（`CREATE INDEX`）が commit されても、その `read_txn` 経由の
    /// 判定は書き込み前のスナップショットのまま変わらないこと（TOCTOU 対策・
    /// fail-closed の前提）を固定する。`IndexCatalogGateCache` は世代をキーに
    /// するだけで、`read_txn` 自体のスナップショット隔離は `redb` の MVCC に
    /// 委ねる（本テストはその前提が本キャッシュ導入後も崩れていないことの
    /// 回帰確認）。判定が変わる書き込みとして `HnswScope::Declared` での
    /// `docs` への宣言を使う。
    #[test]
    fn hnsw_targeted_in_txn_cache_respects_read_txn_snapshot_taken_before_the_write() {
        let (storage, _guard) = index_fixture_storage("index-gate-cache-snapshot");
        let gate_cache = IndexCatalogGateCache::new();
        let scope = HnswScope::Declared;

        // クエリ開始時点の read_txn（`docs` への HNSW 宣言より前のスナップショット）。
        let read_txn_before = storage.db().begin_read().expect("begin read (before)");

        storage
            .create_index(&hnsw_def("idx_hnsw_docs", "docs"))
            .expect("create hnsw index");

        // 新しい read_txn（宣言後）では `docs` が `true`。
        let read_txn_after = storage.db().begin_read().expect("begin read (after)");
        assert!(hnsw_targeted_in_txn(
            &read_txn_after,
            &gate_cache,
            "docs",
            scope,
            true
        ));

        // 旧スナップショットからは宣言が見えないため、依然 `false`（より新しい
        // 世代のキャッシュ済み要約を古いスナップショットへ流用しない）。
        assert!(!hnsw_targeted_in_txn(
            &read_txn_before,
            &gate_cache,
            "docs",
            scope,
            true
        ));
        // 古い世代の要約が、より新しい世代の判定をキャッシュ経由で `false` へ
        // 誤って倒さない。
        assert!(hnsw_targeted_in_txn(
            &read_txn_after,
            &gate_cache,
            "docs",
            scope,
            true
        ));
    }

    #[test]
    fn index_def_round_trips_and_rejects_corruption() {
        let def = IndexDef::new(
            "idx".to_string(),
            "docs".to_string(),
            IndexKind::Hnsw,
            vec!["embedding".to_string()],
        );
        let bytes = encode_index_def(&def).expect("encode");
        assert_eq!(decode_index_def("idx", &bytes).expect("decode"), def);

        for corrupt in [
            &b"v2\ndocs\nscalar\nlang"[..],
            b"v1\ndocs\nbtree\nlang",
            b"v1\ndocs\nscalar\n",
            b"v1\ndocs\nscalar\nlang,,x",
            b"v1\n1docs\nscalar\nlang",
            b"v1\ndocs\nscalar\nlang\nextra",
            b"v1\ndocs\nscalar\nlang,id,lang",
            b"v1\ndocs\nscalar",
            b"\xff\xfe",
        ] {
            assert!(
                matches!(
                    decode_index_def("idx", corrupt),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "corrupt value must be rejected: {corrupt:?}"
            );
        }
    }

    /// 列の存在・種別整合の判定（`id` は暗黙列としてスカラー宣言に使えるが
    /// `VECTOR` ではないため `USING hnsw` には使えない）。
    #[test]
    fn create_index_validates_columns_against_schema() {
        let (storage, _guard) = index_fixture_storage("index-ddl-columns");
        assert!(matches!(
            storage.create_index(&scalar_def("i1", "docs", &["missing"])),
            Err(CatalogError::ColumnNotFound(c)) if c == "missing"
        ));
        assert!(matches!(
            storage.create_index(&scalar_def("i2", "docs", &["embedding"])),
            Err(CatalogError::IndexKindMismatch(_))
        ));
        assert!(matches!(
            storage.create_index(&scalar_def("i3", "docs", &["flag"])),
            Err(CatalogError::IndexKindMismatch(_))
        ));
        let hnsw = |col: &str| {
            IndexDef::new(
                "i4".to_string(),
                "docs".to_string(),
                IndexKind::Hnsw,
                vec![col.to_string()],
            )
        };
        assert!(matches!(
            storage.create_index(&hnsw("id")),
            Err(CatalogError::IndexKindMismatch(_))
        ));
        assert!(matches!(
            storage.create_index(&hnsw("lang")),
            Err(CatalogError::IndexKindMismatch(_))
        ));
        storage
            .create_index(&scalar_def("i5", "docs", &["id", "lang"]))
            .expect("id and TEXT columns are declarable");
        storage
            .create_index(&hnsw("embedding"))
            .expect("hnsw on VECTOR");
        let names: Vec<String> = storage
            .list_indexes()
            .expect("list")
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["i4".to_string(), "i5".to_string()]);
    }

    /// 公開 Rust API（[`Storage::create_index`]）から同一列を重複指定した
    /// `IndexDef` は SQL 表層と同じ `Invalid`（`42601`）で拒否し、何も永続化しない
    /// （PR #1054 レビュー指摘の回帰防止）。
    #[test]
    fn create_index_rejects_duplicate_columns_from_rust_api() {
        let (storage, _guard) = index_fixture_storage("index-ddl-dup-columns");
        for cols in [&["lang", "lang"][..], &["id", "lang", "id"][..]] {
            assert!(matches!(
                storage.create_index(&scalar_def("dup", "docs", cols)),
                Err(CatalogError::Invalid(_))
            ));
        }
        assert!(storage.list_indexes().expect("list").is_empty());
    }

    /// UNIQUE 制約（TABLE-16、Issue #905）の構成列は `DROP COLUMN` 自体が拒否
    /// されるため、その列を含む索引宣言も削除されずに残る（拒否時は何も変更しない）。
    #[test]
    fn drop_column_rejected_by_unique_constraint_keeps_index_declarations() {
        let (storage, _guard) = index_fixture_storage("index-ddl-unique-drop");
        storage
            .alter_table_add_unique_constraint("docs", &["lang"])
            .expect("add unique constraint");
        storage
            .create_index(&scalar_def("docs_lang", "docs", &["lang"]))
            .expect("create index");
        assert!(matches!(
            storage.alter_table_drop_column("docs", "lang"),
            Err(CatalogError::DependentObjectsStillExist(_))
        ));
        let names: Vec<String> = storage
            .list_indexes()
            .expect("list")
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["docs_lang".to_string()]);
    }

    /// `CREATE INDEX ... ON <既存索引名>` は索引もテーブル・ビューと同じ
    /// relation 名前空間に属するため、テーブル不在ではなく種別不一致
    /// （`WrongObjectKind`）で拒否する（PR #1054 レビュー指摘の回帰防止）。
    #[test]
    fn create_index_on_existing_index_name_is_wrong_object_kind() {
        let (storage, _guard) = index_fixture_storage("index-ddl-on-index");
        storage
            .create_index(&scalar_def("docs_lang", "docs", &["lang"]))
            .expect("create index");
        assert!(matches!(
            storage.create_index(&scalar_def("other", "docs_lang", &["lang"])),
            Err(CatalogError::WrongObjectKind(name)) if name == "docs_lang"
        ));
        assert!(storage.index_exists("docs_lang").expect("lookup"));
        assert!(!storage.index_exists("docs").expect("lookup"));
        assert_eq!(storage.list_indexes().expect("list").len(), 1);
    }

    /// `DROP TABLE` は対象テーブルの索引宣言を同一 txn で一掃し、他テーブルの
    /// 宣言は残す。`DROP COLUMN` は当該列を含む宣言のみを削除する。
    #[test]
    fn drop_table_and_drop_column_clean_up_index_declarations() {
        let (storage, _guard) = index_fixture_storage("index-ddl-cleanup");
        storage
            .create_index(&scalar_def("docs_lang", "docs", &["lang"]))
            .expect("create");
        storage
            .create_index(&scalar_def("docs_id", "docs", &["id"]))
            .expect("create");
        storage
            .create_index(&IndexDef::new(
                "sib_hnsw".to_string(),
                "sibling".to_string(),
                IndexKind::Hnsw,
                vec!["embedding".to_string()],
            ))
            .expect("create");

        storage
            .alter_table_drop_column("docs", "lang")
            .expect("drop column");
        let names = |s: &Storage| -> Vec<String> {
            s.list_indexes()
                .expect("list")
                .into_iter()
                .map(|d| d.name)
                .collect()
        };
        assert_eq!(
            names(&storage),
            vec!["docs_id".to_string(), "sib_hnsw".to_string()]
        );

        storage.drop_table("docs").expect("drop table");
        assert_eq!(names(&storage), vec!["sib_hnsw".to_string()]);
    }

    #[test]
    fn validate_schema_rejects_more_than_one_vector_column() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(384), false),
                ColumnDef::new("other", ColumnType::Vector(8), false),
            ],
        );
        assert!(matches!(
            validate_schema(&schema),
            Err(CatalogError::Invalid(_))
        ));
    }

    #[test]
    fn validate_identifier_accepts_valid_forms() {
        assert!(validate_identifier("a").is_ok());
        assert!(validate_identifier("_foo").is_ok());
        assert!(validate_identifier("foo_bar123").is_ok());
        assert!(validate_identifier("A1").is_ok());
    }

    #[test]
    fn validate_identifier_rejects_invalid_forms() {
        assert!(validate_identifier("").is_err());
        assert!(validate_identifier("1abc").is_err());
        assert!(validate_identifier("-abc").is_err());
        assert!(validate_identifier("a b").is_err());
        assert!(validate_identifier("a:b").is_err());
        assert!(validate_identifier("a\nb").is_err());
        assert!(validate_identifier("héllo").is_err());
        assert!(validate_identifier(&"a".repeat(MAX_IDENTIFIER_LEN + 1)).is_err());
    }

    #[test]
    fn validate_vector_dim_rejects_zero_and_overflow() {
        assert!(validate_vector_dim(0).is_err());
        assert!(validate_vector_dim(MAX_VECTOR_DIM + 1).is_err());
        assert!(validate_vector_dim(1).is_ok());
        assert!(validate_vector_dim(MAX_VECTOR_DIM).is_ok());
    }

    #[test]
    fn encode_decode_roundtrip_preserves_schema() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(384), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("tag", ColumnType::Text, true),
            ],
        );
        let encoded = encode_schema(&schema).expect("encode should succeed");
        let decoded = decode_schema("docs", &encoded).expect("decode should succeed");
        assert_eq!(decoded, schema);
    }

    /// `PRIMARY KEY`（TABLE-16・TASK-204、Issue #903）を持つスキーマは v4 で
    /// 往復する。主キーを持たないスキーマのゴールデンバイト列
    /// （`encode_schema_golden_v2_layout`）には一切影響しない。
    #[test]
    fn encode_decode_roundtrip_preserves_primary_key_v4() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("code", ColumnType::Text, false),
                ColumnDef::new("region", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        )
        .with_primary_key(vec!["code".to_string(), "region".to_string()]);
        let encoded = encode_schema(&schema).expect("encode should succeed");
        let text = std::str::from_utf8(&encoded).expect("utf8");
        assert!(text.starts_with("v4\n"));
        assert!(text.contains("pk:code,region\n"));
        let decoded = decode_schema("docs", &encoded).expect("decode should succeed");
        assert_eq!(decoded, schema);
        assert_eq!(
            decoded.primary_key(),
            Some(&["code".to_string(), "region".to_string()][..])
        );
    }

    /// `PRIMARY KEY`（Issue #903・v4）と `DEFAULT`（Issue #904・v5）の両方を
    /// 持つスキーマは v5 で書かれ、`pk:` 行（非空）・`default` フィールドの
    /// 両方が往復すること（base 取り込みマージで両フォーマットを統合した際の
    /// 回帰。`encode_decode_roundtrip_preserves_primary_key_v4`〔PK のみ〕・
    /// `sql_not_null_default.rs::catalog_v5_roundtrips_default_value_across_reopen`
    /// 〔DEFAULT のみ〕とは異なる、両方を同時に持つ組み合わせを固定する）。
    #[test]
    fn encode_decode_roundtrip_preserves_primary_key_and_default_v5() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("code", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, false)
                    .with_default(ColumnDefault::Text("ja".to_string())),
            ],
        )
        .with_primary_key(vec!["code".to_string()]);
        let encoded = encode_schema(&schema).expect("encode should succeed");
        let text = std::str::from_utf8(&encoded).expect("utf8");
        assert!(text.starts_with("v5\n"));
        assert!(text.contains("pk:code\n"));
        let decoded = decode_schema("docs", &encoded).expect("decode should succeed");
        assert_eq!(decoded, schema);
        assert_eq!(decoded.primary_key(), Some(&["code".to_string()][..]));
        let lang = decoded
            .columns
            .iter()
            .find(|c| c.name == "lang")
            .expect("lang column present");
        assert_eq!(lang.default, Some(ColumnDefault::Text("ja".to_string())));
    }

    /// 主キー宣言なしのスキーマはこれまでどおり v2 のまま（`PRIMARY KEY` 追加が
    /// 既存カタログ値のバイト列に一切影響しないことの回帰）。
    #[test]
    fn schema_without_primary_key_still_encodes_as_v2() {
        let schema = TableSchema::new("docs", vec![ColumnDef::new("body", ColumnType::Text, true)]);
        let encoded = encode_schema(&schema).expect("encode should succeed");
        let text = std::str::from_utf8(&encoded).expect("utf8");
        assert!(text.starts_with("v2\n"));
        assert!(schema.primary_key().is_none());
    }

    /// 主キー未宣言を表す `Invalid` 各種の拒否理由を固定する。
    #[test]
    fn validate_primary_key_rejects_invalid_declarations() {
        let base = || {
            TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(4), false),
                    ColumnDef::new("code", ColumnType::Text, false),
                ],
            )
        };

        // 空リスト。
        let empty = base().with_primary_key(vec![]);
        assert!(matches!(
            encode_schema(&empty),
            Err(CatalogError::Invalid(_))
        ));

        // 未知列。
        let unknown = base().with_primary_key(vec!["missing".to_string()]);
        assert!(matches!(
            encode_schema(&unknown),
            Err(CatalogError::Invalid(_))
        ));

        // `VECTOR` 列は主キー対象外。
        let vector_pk = base().with_primary_key(vec!["embedding".to_string()]);
        assert!(matches!(
            encode_schema(&vector_pk),
            Err(CatalogError::Invalid(_))
        ));

        // nullable な列は主キー対象外。
        let nullable =
            TableSchema::new("docs", vec![ColumnDef::new("code", ColumnType::Text, true)])
                .with_primary_key(vec!["code".to_string()]);
        assert!(matches!(
            encode_schema(&nullable),
            Err(CatalogError::Invalid(_))
        ));

        // 列名重複。
        let dup = base().with_primary_key(vec!["code".to_string(), "code".to_string()]);
        assert!(matches!(encode_schema(&dup), Err(CatalogError::Invalid(_))));

        // 上限超過。
        let too_many = TableSchema::new(
            "docs",
            (0..MAX_PRIMARY_KEY_COLUMNS + 1)
                .map(|i| ColumnDef::new(format!("c{i}"), ColumnType::Text, false))
                .collect(),
        )
        .with_primary_key(
            (0..MAX_PRIMARY_KEY_COLUMNS + 1)
                .map(|i| format!("c{i}"))
                .collect(),
        );
        assert!(matches!(
            encode_schema(&too_many),
            Err(CatalogError::Invalid(_))
        ));
    }

    /// `id` を主キー列として渡す（Rust API 経由）と「未知列」として拒否される
    /// （`id` はカタログ上の疑似列であり `schema.columns` には現れないため。
    /// TABLE-16・TASK-204、Issue #903）。
    #[test]
    fn validate_primary_key_rejects_id_pseudo_column() {
        let schema = TableSchema::new("docs", vec![ColumnDef::new("body", ColumnType::Text, true)])
            .with_primary_key(vec!["id".to_string()]);
        assert!(matches!(
            encode_schema(&schema),
            Err(CatalogError::Invalid(_))
        ));
    }

    /// 主キー構成列は `ALTER TABLE ... DROP COLUMN` で拒否される
    /// （`DependentObjectsStillExist`。TABLE-16・TASK-204、Issue #903）。
    #[test]
    fn alter_table_drop_column_rejects_primary_key_column() {
        let path = unique_db_path("drop-pk-column");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("code", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        )
        .with_primary_key(vec!["code".to_string()]);
        storage.create_table(&schema).expect("create table");
        let err = storage
            .alter_table_drop_column("docs", "code")
            .expect_err("dropping a primary key column must be rejected");
        assert!(matches!(err, CatalogError::DependentObjectsStillExist(name) if name == "code"));
    }

    /// 配列列（TABLE-14・TASK-198、Issue #888）のカタログ往復。TEXT/BOOLEAN
    /// 双方の要素型・複数の `max_len` で確認する。
    #[test]
    fn encode_decode_roundtrip_preserves_array_column() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new(
                    "tags",
                    ColumnType::Array(ArrayType::new(ArrayElemType::Text, 64).expect("array ty")),
                    false,
                ),
                ColumnDef::new(
                    "flags",
                    ColumnType::Array(
                        ArrayType::new(ArrayElemType::Bool, MAX_ARRAY_ELEMENTS).expect("array ty"),
                    ),
                    true,
                ),
            ],
        );
        let encoded = encode_schema(&schema).expect("encode should succeed");
        let decoded = decode_schema("docs", &encoded).expect("decode should succeed");
        assert_eq!(decoded, schema);
    }

    #[test]
    fn array_column_rejects_malformed_param() {
        // param が 2 要素ちょうどでない（要素・上限のいずれかが欠落／過多）。
        let bytes = b"v2\ncols:1\ntags:array:text:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    #[test]
    fn array_column_rejects_non_canonical_max_len() {
        // 先頭ゼロは正準形でないため拒否する（D-A2）。
        let bytes = b"v2\ncols:1\ntags:array:text,008:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    #[test]
    fn array_column_rejects_max_len_exceeding_limit() {
        let bytes =
            format!("v2\ncols:1\ntags:array:text,{}:0\n", MAX_ARRAY_ELEMENTS + 1).into_bytes();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    #[test]
    fn array_column_rejects_nested_array_element_type() {
        // 配列要素として vector/array は構造的に非対応（TABLE-14）。
        let bytes = b"v2\ncols:1\ntags:array:vector,4:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    #[test]
    fn array_type_max_len_accessor_matches_construction() {
        let ty = ArrayType::new(ArrayElemType::Bool, 10).expect("array ty");
        assert_eq!(ty.max_len(), 10);
        assert_eq!(ty.elem(), ArrayElemType::Bool);
    }

    /// `NUMERIC(p, s)` 列のカタログ往復（TABLE-13〔検討中〕・TASK-197、
    /// Issue #885・D1）。`catalog_fields` の `param` が `"p,s"` 形式であり、
    /// decode 後も往復することを固定する。
    #[test]
    fn numeric_column_roundtrips_through_catalog() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new(
                    "price",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    true,
                ),
            ],
        );
        assert_eq!(
            schema.columns[1].ty.catalog_fields(),
            ("numeric", "10,2".to_string())
        );
        let encoded = encode_schema(&schema).expect("encode should succeed");
        let decoded = decode_schema("docs", &encoded).expect("decode should succeed");
        assert_eq!(decoded, schema);
    }

    /// 不正な `NUMERIC` param（区切り不正・非正規形・範囲外）は encode・decode
    /// いずれの経路でも fail-closed に拒否する。
    #[test]
    fn numeric_param_rejects_malformed_and_out_of_range_forms() {
        for bad in [
            "10", "10,", ",2", "39,0", "0,0", "3,4", "010,2", "10,2,3", "a,2", "10,a",
        ] {
            assert!(
                parse_numeric_param(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        assert!(parse_numeric_param("10,2").is_ok());
        assert!(parse_numeric_param("38,38").is_ok());
        assert!(parse_numeric_param("1,0").is_ok());
    }

    #[test]
    fn decode_rejects_unknown_version() {
        let bytes = b"v99\ncols:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    /// カタログ v1（Issue #880 で v2 へ更新される前の正当な形式）は、
    /// マイグレーションを提供せず fail-closed に拒否する（D6）。
    #[test]
    fn decode_rejects_legacy_v1_format() {
        let bytes = b"v1\ncols:1\nfoo:text:-:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    /// `encode_schema` が出力する v2 の 1 行目がバージョン識別子であることを
    /// バイト列で固定する（golden。Issue #880 のカタログ v1→v2 移行で変わるのは
    /// この 1 行目のみで、列行のバイト表現は不変であることの根拠とする）。
    #[test]
    fn encode_schema_golden_v2_layout() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("tag", ColumnType::Text, true),
            ],
        );
        let encoded = encode_schema(&schema).expect("encode should succeed");
        assert_eq!(
            encoded,
            b"v2\ncols:3\nembedding:vector:3:0\nbody:text:-:0\ntag:text:-:1\n".to_vec()
        );
    }

    /// `REAL`／`DOUBLE PRECISION` 列（TABLE-13・TASK-196）のカタログ v2 往復を
    /// 固定する golden テスト。`param` は他のパラメータなし型（`Text`）と同じ
    /// `-` 固定であることを含む。
    #[test]
    fn encode_decode_roundtrips_real_and_double_columns() {
        let schema = TableSchema::new(
            "metrics",
            vec![
                ColumnDef::new("score", ColumnType::Real, false),
                ColumnDef::new("weight", ColumnType::Double, true),
            ],
        );
        let encoded = encode_schema(&schema).expect("encode should succeed");
        assert_eq!(
            encoded,
            b"v2\ncols:2\nscore:real:-:0\nweight:double:-:1\n".to_vec()
        );
        let decoded = decode_schema("metrics", &encoded).expect("decode should succeed");
        assert_eq!(decoded, schema);
    }

    /// `real`／`double` タグに `-` 以外の `param` を付けた場合は拒否する
    /// （`Text` と同じ「パラメータなし型」の契約。TABLE-13）。
    #[test]
    fn decode_rejects_real_and_double_with_non_dash_param() {
        for bytes in [
            b"v2\ncols:1\nscore:real:1:0\n".to_vec(),
            b"v2\ncols:1\nweight:double:1:0\n".to_vec(),
        ] {
            assert!(
                matches!(
                    decode_schema("t", &bytes),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "must reject: {bytes:?}"
            );
        }
    }

    /// `param` フィールドの文字集合違反（`:`・改行相当の区切り注入、非 ASCII）を
    /// fail-closed に拒否する（TABLE-6・Issue #880 D5）。
    #[test]
    fn decode_rejects_invalid_param_charset() {
        for bytes in [
            b"v2\ncols:1\nfoo:vector:1:2:0\n".to_vec(), // ':' 混入で param が余剰フィールドを生む
            "v2\ncols:1\nfoo:text:あ:0\n".as_bytes().to_vec(), // 非 ASCII
        ] {
            assert!(
                matches!(
                    decode_schema("t", &bytes),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "must reject: {bytes:?}"
            );
        }
    }

    /// `encode_schema`（実体は [`encode_column_line`]）が `param` フィールドの
    /// 文字集合違反を decode 側（[`decode_rejects_invalid_param_charset`]）と
    /// 対称に fail-closed 拒否することを固定する（TABLE-6・Issue #880 D5・
    /// codex-review 指摘 PR #999）。`ColumnType` は現状 `Text`/`Vector` のみで
    /// `catalog_fields` が不正な `param` を返すことはないため、将来型追加時の
    /// 回帰を検知できるよう encode の実処理関数を直接不正 `param` で呼ぶ。
    #[test]
    fn encode_column_line_rejects_invalid_param_charset() {
        for invalid_param in ["", "1:2", "a\nb", "あ"] {
            assert!(
                matches!(
                    encode_column_line("foo", "text", invalid_param, false),
                    Err(CatalogError::Invalid(_))
                ),
                "must reject: {invalid_param:?}"
            );
        }
    }

    /// `encode_column_line` は `param` が許容文字集合内であれば
    /// `encode_schema_golden_v2_layout` と同じ行文字列を組み立てる（非退行）。
    #[test]
    fn encode_column_line_accepts_valid_param_charset() {
        assert_eq!(
            encode_column_line("tag", "text", "-", true).expect("valid param must be accepted"),
            "tag:text:-:1\n"
        );
        assert_eq!(
            encode_column_line("embedding", "vector", "384", false)
                .expect("valid param must be accepted"),
            "embedding:vector:384:0\n"
        );
    }

    // --- INTEGER / BIGINT（Issue #881・TABLE-13・TASK-196） -----------------

    /// `ColumnType::Integer`／`BigInt` のカタログタグ・往復（encode → decode）が
    /// 一致することを固定する。
    #[test]
    fn integer_bigint_column_type_roundtrips_through_catalog_schema() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("n", ColumnType::Integer, false),
                ColumnDef::new("b", ColumnType::BigInt, true),
            ],
        );
        let encoded = encode_schema(&schema).expect("encode should succeed");
        assert_eq!(
            encoded,
            b"v2\ncols:3\nembedding:vector:3:0\nn:integer:-:0\nb:bigint:-:1\n".to_vec()
        );
        let decoded = decode_schema("docs", &encoded).expect("decode should succeed");
        assert_eq!(decoded, schema);
    }

    /// `integer`／`bigint` タグへ `param` を付与した場合は `CatalogError::CorruptSchema`
    /// で fail-closed に拒否する（`text` 型と同じ「パラメータなし型」の契約）。
    #[test]
    fn integer_and_bigint_column_reject_declared_parameter() {
        for bytes in [
            b"v2\ncols:1\nn:integer:4:0\n".to_vec(),
            b"v2\ncols:1\nb:bigint:8:0\n".to_vec(),
        ] {
            assert!(
                matches!(
                    decode_schema("t", &bytes),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "must reject: {bytes:?}"
            );
        }
    }

    /// 宣言列数 (`cols:`) を超える余剰行（トレーリング空行 1 行を除く）を
    /// 拒否する。未知の型タグを含む余剰行であっても、宣言列数を超えた時点で
    /// 拒否され、余剰分の `ColumnDef` は構築されない（Issue #880 D7）。
    #[test]
    fn decode_rejects_excess_lines_beyond_declared_column_count() {
        let bytes = b"v2\ncols:1\nfoo:text:-:0\nbar:unknowntype:-:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    #[test]
    fn decode_rejects_truncated_column_lines() {
        let bytes = b"v2\ncols:2\nfoo:text:-:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    #[test]
    fn decode_rejects_unknown_type() {
        let bytes = b"v2\ncols:1\nfoo:blob:-:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    #[test]
    fn decode_rejects_bad_dimension() {
        let bytes = b"v2\ncols:1\nfoo:vector:0:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
        let bytes = b"v2\ncols:1\nfoo:vector:not-a-number:0\n".to_vec();
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    #[test]
    fn decode_rejects_invalid_utf8() {
        let bytes = vec![0xff, 0xfe, 0xfd];
        assert!(matches!(
            decode_schema("t", &bytes),
            Err(CatalogError::CorruptSchema(_))
        ));
    }

    /// `drop_enum_type` は破損カタログ値を「依存なし」に丸めず `CorruptSchema`
    /// として拒否する（PR #1015 レビュー指摘・codex-review P1。Issue #890）。
    /// `decode_rejects_invalid_utf8` と同じ破損データ（不正 UTF-8）を、実際に
    /// 依存判定が走る `CATALOG_TABLE` エントリへ直接書き込み、それを検証する。
    /// 破損に丸めて削除を通してしまう fail-open 経路だと、実際には参照されて
    /// いる ENUM 型が消えてしまう（本テストは削除が起きないことも確認する）。
    #[test]
    fn drop_enum_type_rejects_corrupt_catalog_value_instead_of_treating_it_as_no_dependents() {
        let path = unique_db_path("catalog-drop-enum-corrupt");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let type_name = "mood";
        storage
            .create_enum_type(type_name, vec!["happy".to_string()])
            .expect("create enum type");

        // 依存判定（`dependent_tables_in_txn`）は `CATALOG_TABLE` の全エントリを
        // 走査するため、実在テーブルを経由せず不正 UTF-8 の値を直接書き込む
        // （`decode_rejects_invalid_utf8` と同じ破損データ）。
        let write_txn = storage.begin_write_txn().expect("begin write txn");
        {
            let mut table = write_txn
                .open_table(CATALOG_TABLE)
                .expect("open catalog table");
            table
                .insert("docs", [0xff_u8, 0xfe, 0xfd].as_slice())
                .expect("insert corrupt catalog value");
        }
        // テスト専用のセットアップ書き込みのため `commit_boundary` を経由せず
        // 直接 `redb::WriteTransaction::commit` を呼ぶ（既存テスト
        // `catalog.rs` 内の他のセットアップ commit と同じ流儀。
        // `table_generation_bump_coverage.rs` の悉皆走査は
        // `commit_boundary::commit*` 呼び出しのみを対象とするため、これを
        // 経由すると本テスト自身がアローリスト追記を要求されてしまう）。
        write_txn
            .commit_raw_for_test()
            .expect("commit corrupt catalog value");

        let err = storage.drop_enum_type(type_name).unwrap_err();
        assert!(
            matches!(err, CatalogError::CorruptSchema(_)),
            "drop_enum_type must fail-closed on corrupt catalog values, got: {err:?}"
        );

        // 型は削除されず残っていること（fail-open だとここが消えてしまう）。
        storage
            .get_enum_type(type_name)
            .expect("enum type must still exist after the rejected drop");
    }

    /// `catalog_value_references_enum_type`（`drop_enum_type` 等が使う軽量な
    /// 依存判定）は v3 カタログ値の 5 番目フィールド（`state`）が「フィールドの
    /// 存在」だけでなく値そのものが `"L"`／`"D"` であることも検証しなければ
    /// ならない（codex-review P1 指摘・PR #1045）。値がその他の不正な文字列
    /// （破損カタログ）である場合に「依存なし」へ丸めてしまうと、
    /// `drop_enum_type` が破損カタログを残したまま実際には参照されている
    /// ENUM 型を削除できてしまう。
    #[test]
    fn drop_enum_type_rejects_v3_catalog_value_with_invalid_state_field() {
        let path = unique_db_path("catalog-drop-enum-v3-bad-state");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let type_name = "mood";
        storage
            .create_enum_type(type_name, vec!["happy".to_string()])
            .expect("create enum type");

        // v3 形式のカタログ値で、ENUM 型 `mood` を参照する列の state フィールドを
        // "L"／"D" のいずれでもない不正値にする。値検証を欠くと
        // `catalog_value_references_enum_type` はフィールドが 5 個存在すること
        // だけで通過させ「依存なし」と誤判定しうる。
        let corrupt_value =
            format!("{CATALOG_FORMAT_VERSION_V3}\ncols:1\nmood_col:enum:{type_name}:0:X\n");
        let write_txn = storage.begin_write_txn().expect("begin write txn");
        {
            let mut table = write_txn
                .open_table(CATALOG_TABLE)
                .expect("open catalog table");
            table
                .insert("docs", corrupt_value.as_bytes())
                .expect("insert corrupt catalog value");
        }
        write_txn
            .commit_raw_for_test()
            .expect("commit corrupt catalog value");

        let err = storage.drop_enum_type(type_name).unwrap_err();
        assert!(
            matches!(err, CatalogError::CorruptSchema(_)),
            "drop_enum_type must fail-closed on an invalid v3 state field, got: {err:?}"
        );

        // 型は削除されず残っていること（fail-open だとここが消えてしまう）。
        storage
            .get_enum_type(type_name)
            .expect("enum type must still exist after the rejected drop");
    }

    /// `catalog_value_references_enum_type` は v4 カタログの `pk:` 行が
    /// 実在しない列を参照している場合も `decode_schema_body`／
    /// `validate_schema`（`validate_primary_key`）と同じく拒否しなければ
    /// ならない（codex-review P1 指摘・PR #1050）。この値は ENUM 参照列を
    /// 一切持たないため、pk 行を接頭辞と非空だけで検証する軽量チェックだと
    /// 「依存なし」に丸められ、実際には `decode_schema_body` なら拒否する
    /// 壊れたカタログを残したまま `drop_enum_type` が進んでしまう。
    #[test]
    fn drop_enum_type_rejects_v4_catalog_value_with_unknown_primary_key_column() {
        let path = unique_db_path("catalog-drop-enum-v4-unknown-pk");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let type_name = "mood";
        storage
            .create_enum_type(type_name, vec!["happy".to_string()])
            .expect("create enum type");

        // v4 形式で `pk:` 行が宣言列に存在しない `missing` を参照する。ENUM
        // 参照列は含まない。
        let corrupt_value =
            format!("{CATALOG_FORMAT_VERSION_V4}\ncols:1\npk:missing\ncode:text:-:0:L\n");
        let write_txn = storage.begin_write_txn().expect("begin write txn");
        {
            let mut table = write_txn
                .open_table(CATALOG_TABLE)
                .expect("open catalog table");
            table
                .insert("docs", corrupt_value.as_bytes())
                .expect("insert corrupt catalog value");
        }
        write_txn
            .commit_raw_for_test()
            .expect("commit corrupt catalog value");

        let err = storage.drop_enum_type(type_name).unwrap_err();
        assert!(
            matches!(err, CatalogError::CorruptSchema(_)),
            "drop_enum_type must fail-closed on a v4 pk line referencing an unknown column, got: {err:?}"
        );

        // 型は削除されず残っていること（fail-open だとここが消えてしまう）。
        storage
            .get_enum_type(type_name)
            .expect("enum type must still exist after the rejected drop");
    }

    /// 上記と対の検証: `pk:` 行が同一列を重複して参照する壊れた v4 カタログも
    /// 同じく `CorruptSchema` で拒否される（`validate_primary_key` の
    /// 「主キー列の重複なし」契約と同じ。codex-review P1 指摘・PR #1050）。
    #[test]
    fn drop_enum_type_rejects_v4_catalog_value_with_duplicate_primary_key_column() {
        let path = unique_db_path("catalog-drop-enum-v4-dup-pk");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let type_name = "mood";
        storage
            .create_enum_type(type_name, vec!["happy".to_string()])
            .expect("create enum type");

        let corrupt_value =
            format!("{CATALOG_FORMAT_VERSION_V4}\ncols:1\npk:code,code\ncode:text:-:0:L\n");
        let write_txn = storage.begin_write_txn().expect("begin write txn");
        {
            let mut table = write_txn
                .open_table(CATALOG_TABLE)
                .expect("open catalog table");
            table
                .insert("docs", corrupt_value.as_bytes())
                .expect("insert corrupt catalog value");
        }
        write_txn
            .commit_raw_for_test()
            .expect("commit corrupt catalog value");

        let err = storage.drop_enum_type(type_name).unwrap_err();
        assert!(
            matches!(err, CatalogError::CorruptSchema(_)),
            "drop_enum_type must fail-closed on a v4 pk line with a duplicate column, got: {err:?}"
        );

        storage
            .get_enum_type(type_name)
            .expect("enum type must still exist after the rejected drop");
    }

    /// `catalog_value_references_enum_type` は v5 カタログの `default`
    /// フィールドが「フィールドの存在」だけでなく内容（未知タグ）も検証しな
    /// ければならない（codex-review P1 指摘・Issue #904・PR #1051）。値検証を
    /// 欠くと、`decode_schema_body`（`ColumnDefault::decode_catalog_field`）
    /// なら拒否する未知タグの破損 default を「依存なし」に丸めてしまう
    /// （このテーブルは対象 ENUM 型を参照しないため、値検証が無いと素通り
    /// してしまう点が v3/v4 の既存回帰テストと同じ構図）。
    #[test]
    fn drop_enum_type_rejects_v5_catalog_value_with_unknown_default_tag() {
        let path = unique_db_path("catalog-drop-enum-v5-bad-default-tag");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let type_name = "mood";
        storage
            .create_enum_type(type_name, vec!["happy".to_string()])
            .expect("create enum type");

        // v5 形式。ENUM 参照列は持たず、`default` フィールドに
        // `ColumnDefault::decode_catalog_field` が拒否する未知タグ `z` を置く。
        let corrupt_value =
            format!("{CATALOG_FORMAT_VERSION_V5}\ncols:1\npk:\ncode:text:-:0:L:zff\n");
        let write_txn = storage.begin_write_txn().expect("begin write txn");
        {
            let mut table = write_txn
                .open_table(CATALOG_TABLE)
                .expect("open catalog table");
            table
                .insert("docs", corrupt_value.as_bytes())
                .expect("insert corrupt catalog value");
        }
        write_txn
            .commit_raw_for_test()
            .expect("commit corrupt catalog value");

        let err = storage.drop_enum_type(type_name).unwrap_err();
        assert!(
            matches!(err, CatalogError::CorruptSchema(_)),
            "drop_enum_type must fail-closed on an unknown v5 DEFAULT tag, got: {err:?}"
        );

        storage
            .get_enum_type(type_name)
            .expect("enum type must still exist after the rejected drop");
    }

    /// 上と対の検証: `default` フィールドのデコード自体は成功しても、列の型
    /// タグと大分類が一致しない（`integer` 列に `TEXT` の DEFAULT）場合も
    /// `validate_schema`（`ColumnDefault::compatible_with`）と同じく拒否する
    /// （codex-review P1 指摘・Issue #904・PR #1051）。
    #[test]
    fn drop_enum_type_rejects_v5_catalog_value_with_type_incompatible_default() {
        let path = unique_db_path("catalog-drop-enum-v5-bad-default-type");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let type_name = "mood";
        storage
            .create_enum_type(type_name, vec!["happy".to_string()])
            .expect("create enum type");

        // `s6a61` は `ColumnDefault::Text("ja")` に正しくデコードできるが、
        // 列の型タグは `integer`（数値のみ許容）のため型不一致となる。
        let corrupt_value =
            format!("{CATALOG_FORMAT_VERSION_V5}\ncols:1\npk:\ncount:integer:-:0:L:s6a61\n");
        let write_txn = storage.begin_write_txn().expect("begin write txn");
        {
            let mut table = write_txn
                .open_table(CATALOG_TABLE)
                .expect("open catalog table");
            table
                .insert("docs", corrupt_value.as_bytes())
                .expect("insert corrupt catalog value");
        }
        write_txn
            .commit_raw_for_test()
            .expect("commit corrupt catalog value");

        let err = storage.drop_enum_type(type_name).unwrap_err();
        assert!(
            matches!(err, CatalogError::CorruptSchema(_)),
            "drop_enum_type must fail-closed on a type-incompatible v5 DEFAULT, got: {err:?}"
        );

        storage
            .get_enum_type(type_name)
            .expect("enum type must still exist after the rejected drop");
    }

    /// 上と対の検証: v5 カタログなのに `DEFAULT` を宣言する列が 1 つも無い
    /// （全列 `default` が `-`）場合も、`decode_schema_body` の「v5 は
    /// DEFAULT 0 件を許容しない」不変条件と同じく拒否する（codex-review P1
    /// 指摘・Issue #904・PR #1051）。
    #[test]
    fn drop_enum_type_rejects_v5_catalog_value_with_zero_defaults() {
        let path = unique_db_path("catalog-drop-enum-v5-zero-default");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let type_name = "mood";
        storage
            .create_enum_type(type_name, vec!["happy".to_string()])
            .expect("create enum type");

        let corrupt_value =
            format!("{CATALOG_FORMAT_VERSION_V5}\ncols:1\npk:\ncode:text:-:0:L:-\n");
        let write_txn = storage.begin_write_txn().expect("begin write txn");
        {
            let mut table = write_txn
                .open_table(CATALOG_TABLE)
                .expect("open catalog table");
            table
                .insert("docs", corrupt_value.as_bytes())
                .expect("insert corrupt catalog value");
        }
        write_txn
            .commit_raw_for_test()
            .expect("commit corrupt catalog value");

        let err = storage.drop_enum_type(type_name).unwrap_err();
        assert!(
            matches!(err, CatalogError::CorruptSchema(_)),
            "drop_enum_type must fail-closed on a v5 catalog value with zero DEFAULTs, got: {err:?}"
        );

        storage
            .get_enum_type(type_name)
            .expect("enum type must still exist after the rejected drop");
    }

    // --- カタログ v6（UNIQUE 制約。TABLE-16・TASK-204、Issue #905） ---------

    /// UNIQUE 制約のみを持つスキーマは v6 で往復し、`pk:` 行は空・列行は
    /// 6 フィールド（`default` は `-`）になる。
    #[test]
    fn encode_decode_roundtrip_preserves_unique_constraints_v6() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("a", ColumnType::Text, true),
                ColumnDef::new("b", ColumnType::Text, true),
            ],
        )
        .with_unique_constraints(vec![
            UniqueConstraint::new(vec!["a".to_string()]),
            UniqueConstraint::new(vec!["b".to_string(), "a".to_string()]),
        ]);
        let encoded = encode_schema(&schema).expect("encode should succeed");
        let text = std::str::from_utf8(&encoded).expect("utf8");
        assert_eq!(
            text,
            "v6\ncols:3\npk:\nembedding:vector:4:0:L:-\na:text:-:1:L:-\nb:text:-:1:L:-\nuniq:2\nU:a\nU:b,a\n"
        );
        let decoded = decode_schema("docs", &encoded).expect("decode should succeed");
        // 名前未指定の UNIQUE 制約は decode 時に既定名を導出する（設計 D2・
        // Issue #1067）。バイト列（上のアサーション）は v6 のまま不変だが、
        // decode 結果のオブジェクトは実名を持つため、比較対象も
        // `assign_unique_constraint_names` で同じ既定名を確定させる。
        assert_eq!(decoded, assign_unique_constraint_names(schema));
    }

    /// 主キー・`DEFAULT`・墓標・UNIQUE 制約をすべて持つスキーマも v6 で往復する
    /// （v4／v5／v3 の各要素が v6 の中で失われないことの回帰）。
    #[test]
    fn encode_decode_roundtrip_v6_with_primary_key_default_and_dropped_slot() {
        let schema = TableSchema::from_parts(
            "docs",
            vec![
                ColumnDef::new("code", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, true)
                    .with_default(ColumnDefault::Text("ja".to_string())),
            ],
            vec![DroppedSlot {
                physical_index: 1,
                name: "old".to_string(),
                ty: ColumnType::Text,
            }],
            Some(vec!["code".to_string()]),
            vec![UniqueConstraint::new(vec!["lang".to_string()])],
        );
        let encoded = encode_schema(&schema).expect("encode should succeed");
        let text = std::str::from_utf8(&encoded).expect("utf8");
        assert!(text.starts_with("v6\ncols:3\npk:code\n"), "{text}");
        assert!(text.ends_with("uniq:1\nU:lang\n"), "{text}");
        let decoded = decode_schema("docs", &encoded).expect("decode should succeed");
        // 名前未指定の UNIQUE 制約は decode 時に既定名を導出する（設計 D2・
        // Issue #1067。上記コメント参照）。
        assert_eq!(decoded, assign_unique_constraint_names(schema));
    }

    /// UNIQUE 制約を持たないスキーマのバイト列は v2〜v5 のまま不変（v6 導入が
    /// 既存カタログ値へ影響しないことの回帰）。
    #[test]
    fn schema_without_unique_constraints_keeps_previous_versions() {
        let v2 = TableSchema::new("docs", vec![ColumnDef::new("body", ColumnType::Text, true)]);
        assert!(std::str::from_utf8(&encode_schema(&v2).expect("encode"))
            .expect("utf8")
            .starts_with("v2\n"));
        let v5 = TableSchema::new(
            "docs",
            vec![ColumnDef::new("body", ColumnType::Text, true)
                .with_default(ColumnDefault::Text("x".to_string()))],
        );
        assert!(std::str::from_utf8(&encode_schema(&v5).expect("encode"))
            .expect("utf8")
            .starts_with("v5\n"));
    }

    /// `validate_unique_constraints` の拒否理由（未知列・対象外型・制約内重複・
    /// 同一列リストの制約重複・空リスト・上限超過）を固定する。
    #[test]
    fn validate_unique_constraints_rejects_invalid_declarations() {
        let base = || {
            TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(4), false),
                    ColumnDef::new("a", ColumnType::Text, true),
                ],
            )
        };
        let cases: Vec<Vec<Vec<&str>>> = vec![
            vec![vec!["missing"]],
            // `VECTOR` は UNIQUE 制約でも引き続き拒否される
            // （`is_unique_constraint_allowed` は許可型を広げない。Issue #1073）。
            vec![vec!["embedding"]],
            vec![vec!["a", "a"]],
            vec![vec!["a"], vec!["a"]],
            vec![vec![]],
        ];
        for case in cases {
            let schema = base().with_unique_constraints(
                case.iter()
                    .map(|cols| UniqueConstraint::new(cols.iter().map(|c| c.to_string()).collect()))
                    .collect(),
            );
            assert!(
                matches!(validate_schema(&schema), Err(CatalogError::Invalid(_))),
                "expected {case:?} to be rejected"
            );
        }
        let too_many = base().with_unique_constraints(
            (0..=MAX_UNIQUE_CONSTRAINTS)
                .map(|_| UniqueConstraint::new(vec!["a".to_string()]))
                .collect(),
        );
        assert!(matches!(
            validate_schema(&too_many),
            Err(CatalogError::Invalid(_))
        ));
    }

    /// UNIQUE 制約の対象型拡張（TABLE-16・TASK-204、Issue #1073）: REAL・
    /// DOUBLE PRECISION・NUMERIC・JSON・JSONB・配列型のいずれも単一 UNIQUE 制約
    /// の対象として受理する。`VECTOR` は引き続き拒否される
    /// （直前の `validate_unique_constraints_rejects_invalid_declarations` 参照）。
    #[test]
    fn validate_unique_constraints_accepts_extended_types() {
        let extended_types = [
            ColumnType::Real,
            ColumnType::Double,
            ColumnType::Numeric {
                precision: 10,
                scale: 2,
            },
            ColumnType::Json,
            ColumnType::Jsonb,
            ColumnType::Array(ArrayType::new(ArrayElemType::Text, 8).expect("valid array type")),
            ColumnType::Array(ArrayType::new(ArrayElemType::Bool, 8).expect("valid array type")),
        ];
        for ty in extended_types {
            let schema = TableSchema::new("docs", vec![ColumnDef::new("v", ty.clone(), true)])
                .with_unique_constraints(vec![UniqueConstraint::new(vec!["v".to_string()])]);
            // `validate_schema` は制約名が確定済み（既定名導出後）であることを
            // 前提とする（`encode_schema`／`decode_schema_body` は
            // `validate_schema` を呼ぶ前に必ず `assign_unique_constraint_names`
            // を経由する。設計 D2・Issue #1067）。直接呼ぶこのテストでも同じ
            // 前処理を経てから検証する。
            let schema = assign_unique_constraint_names(schema);
            assert!(
                validate_schema(&schema).is_ok(),
                "expected column type {ty:?} to be accepted in a UNIQUE constraint"
            );
        }
    }

    /// PRIMARY KEY・FOREIGN KEY の対象型は Issue #1073 で拡張しない
    /// （`is_primary_key_allowed` は据え置き。`docs/design/foreign-key.md` D3 が
    /// 前提にする「参照元は PK 許可型」を崩さないための固定回帰）。
    #[test]
    fn validate_primary_key_still_rejects_extended_unique_only_types() {
        for ty in [
            ColumnType::Real,
            ColumnType::Double,
            ColumnType::Numeric {
                precision: 10,
                scale: 2,
            },
            ColumnType::Json,
            ColumnType::Jsonb,
            ColumnType::Array(ArrayType::new(ArrayElemType::Text, 8).expect("valid array type")),
        ] {
            let schema = TableSchema::new("docs", vec![ColumnDef::new("v", ty.clone(), false)])
                .with_primary_key(vec!["v".to_string()]);
            assert!(
                matches!(validate_schema(&schema), Err(CatalogError::Invalid(_))),
                "expected column type {ty:?} to still be rejected as a PRIMARY KEY column"
            );
        }
    }

    /// UNIQUE 制約の対象型拡張後も v6 カタログ値との往復が保たれる（新しい型を
    /// 含む UNIQUE 宣言を持つスキーマの encode/decode 往復。Issue #1073）。
    #[test]
    fn encode_decode_roundtrip_preserves_extended_type_unique_constraints_v6() {
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("r", ColumnType::Real, true),
                ColumnDef::new(
                    "n",
                    ColumnType::Numeric {
                        precision: 10,
                        scale: 2,
                    },
                    true,
                ),
                ColumnDef::new("j", ColumnType::Json, true),
                ColumnDef::new(
                    "arr",
                    ColumnType::Array(
                        ArrayType::new(ArrayElemType::Text, 8).expect("valid array type"),
                    ),
                    true,
                ),
            ],
        )
        .with_unique_constraints(vec![
            UniqueConstraint::new(vec!["r".to_string()]),
            UniqueConstraint::new(vec!["n".to_string()]),
            UniqueConstraint::new(vec!["j".to_string()]),
            UniqueConstraint::new(vec!["arr".to_string()]),
        ]);
        let encoded = encode_schema(&schema).expect("encode should succeed");
        let decoded = decode_schema("docs", &encoded).expect("decode should succeed");
        // 名前を明示しない UNIQUE 制約は encode／decode のいずれでも既定名
        // （`derive_unique_constraint_names`）を導出するため（設計 D2・
        // Issue #1067）、比較対象の `schema` にも同じ既定名付与を適用してから
        // 比較する（`UniqueConstraint` の等価性は名前も含むため）。
        assert_eq!(decoded, assign_unique_constraint_names(schema));
    }

    /// FK の参照元列許可型は Issue #1073 の UNIQUE 拡張から独立して据え置かれる
    /// （`docs/design/foreign-key.md` D3 の「参照元は PK 許可型」という前提を
    /// 維持する固定回帰）。参照先が `UNIQUE(r REAL)` を宣言できても
    /// （Issue #1073 で UNIQUE の対象へ追加）、REAL 列から `FOREIGN KEY` を
    /// 宣言することはできない。
    #[test]
    fn validate_foreign_keys_still_rejects_real_referencing_column_after_unique_extension() {
        let parent = TableSchema::new("parents", vec![ColumnDef::new("r", ColumnType::Real, true)])
            .with_unique_constraints(vec![UniqueConstraint::new(vec!["r".to_string()])]);
        // 直接 `validate_schema` を呼ぶため、`encode_schema`／
        // `decode_schema_body` と同じ前提（制約名は検証前に確定済み）を
        // 自前で満たす（設計 D2・Issue #1067）。
        let parent = assign_unique_constraint_names(parent);
        assert!(
            validate_schema(&parent).is_ok(),
            "parent schema with UNIQUE(r REAL) must validate (Issue #1073)"
        );

        let child = TableSchema::new(
            "children",
            vec![ColumnDef::new("r", ColumnType::Real, true)],
        )
        .with_foreign_keys(vec![fk(&["r"], "parents", &["r"])]);
        // 直接 `validate_schema` を呼ぶため、`encode_schema` が担う FK 名の
        // 確定（設計 F2・Issue #1069）を自前で満たす（UNIQUE と同じ前提）。
        let child = assign_foreign_key_constraint_names(child);
        assert!(matches!(
            validate_schema(&child),
            Err(CatalogError::InvalidForeignKey(_))
        ));
    }

    /// v6 の `uniq:` セクションの破損値デコードを `CorruptSchema` で拒否する
    /// （`uniq:` 行の欠落・不正件数・0 件・上限超過・`U:` 行の空要素／接頭辞
    /// 欠落・件数不足・末尾余剰行・未知列参照・制約内重複・制約重複）。
    #[test]
    fn decode_v6_rejects_corrupt_unique_section() {
        let head = "v6\ncols:1\npk:\na:text:-:1:L:-\n";
        let over = MAX_UNIQUE_CONSTRAINTS + 1;
        let corrupt: Vec<String> = vec![
            String::new(),
            "uniq:x\n".to_string(),
            "U:a\n".to_string(),
            "uniq:0\n".to_string(),
            format!("uniq:{over}\n"),
            "uniq:1\nU:\n".to_string(),
            "uniq:1\nU:a,,a\n".to_string(),
            "uniq:1\nU:,a\n".to_string(),
            "uniq:1\nU:a,\n".to_string(),
            "uniq:1\na\n".to_string(),
            "uniq:2\nU:a\n".to_string(),
            "uniq:1\nU:a\nsurplus\n".to_string(),
            "uniq:1\nU:missing\n".to_string(),
            "uniq:1\nU:a,a\n".to_string(),
            "uniq:2\nU:a\nU:a\n".to_string(),
        ];
        for tail in corrupt {
            let bytes = format!("{head}{tail}").into_bytes();
            assert!(
                matches!(
                    decode_schema("t", &bytes),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "expected tail {tail:?} to be rejected"
            );
        }
        // 対照: 有効な v6 値は受理される。
        let valid = format!("{head}uniq:1\nU:a\n").into_bytes();
        let schema = decode_schema("t", &valid).expect("valid v6 value must decode");
        assert_eq!(schema.unique_constraints()[0].columns(), &["a".to_string()]);
    }

    /// `catalog_value_references_enum_type`（`DROP TYPE` の依存判定）も v6 の
    /// `uniq:` セクションを `decode_schema_body` と同じ基準で検証する。有効な
    /// v6 値は ENUM 参照を正しく検出し、`U:` 行が未知列を参照する等の壊れた値は
    /// 「依存なし」に丸めず `CorruptSchema` で拒否する。
    #[test]
    fn catalog_value_references_enum_type_validates_v6_unique_section() {
        let valid =
            "v6\ncols:2\npk:\nmood_col:enum:mood:1:L:-\na:text:-:1:L:-\nuniq:1\nU:mood_col,a\n";
        assert!(catalog_value_references_enum_type(valid.as_bytes(), "mood").expect("valid v6"));
        assert!(!catalog_value_references_enum_type(valid.as_bytes(), "other").expect("valid v6"));
        for corrupt in [
            "v6\ncols:1\npk:\na:text:-:1:L:-\nuniq:1\nU:missing\n",
            "v6\ncols:1\npk:\na:text:-:1:L:-\nuniq:0\n",
            "v6\ncols:1\npk:\na:text:-:1:L:-\n",
            "v6\ncols:1\npk:\na:text:-:1:L:-\nuniq:1\nU:a,a\n",
            "v6\ncols:1\npk:\na:text:-:1:L:-\nuniq:1\nU:a\nsurplus\n",
        ] {
            assert!(
                matches!(
                    catalog_value_references_enum_type(corrupt.as_bytes(), "mood"),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "expected {corrupt:?} to be rejected"
            );
        }
    }

    #[test]
    fn table_schema_validate_embedding_dim() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(384), false)],
        );
        assert!(schema.validate_embedding_dim(384).is_ok());
        assert!(schema.validate_embedding_dim(128).is_err());

        let no_vector = TableSchema::new(
            "docs2",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        assert!(no_vector.validate_embedding_dim(384).is_err());
    }

    // Issue #995・#1079: 行全体を書き込む経路（INSERT 系、および `RowInput` に
    // よる行全体置換 UPDATE）向けの次元検証。`VECTOR` 列ありスキーマでは
    // `validate_embedding_dim` と完全に同一の判定・文言になり
    // （受け入れ基準「`VECTOR` 列を持つスキーマの次元検証・エラー分類は変わらない」）、
    // `VECTOR` 列なしスキーマでは dim 0 のみ受理する。
    #[test]
    fn table_schema_validate_row_embedding_dim_with_vector_column_matches_validate_embedding_dim() {
        let schema = TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(384), false)],
        );
        assert!(schema.validate_row_embedding_dim(384).is_ok());
        let err = schema.validate_row_embedding_dim(128).unwrap_err();
        assert_eq!(
            err.to_string(),
            schema.validate_embedding_dim(128).unwrap_err().to_string()
        );
    }

    #[test]
    fn table_schema_validate_row_embedding_dim_without_vector_column_accepts_only_empty() {
        let no_vector = TableSchema::new(
            "docs2",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        assert!(no_vector.validate_row_embedding_dim(0).is_ok());
        let err = no_vector.validate_row_embedding_dim(1).unwrap_err();
        assert!(
            matches!(&err, CatalogError::Invalid(msg) if msg == "table has no VECTOR column"),
            "non-empty embedding on a table without a VECTOR column must be rejected \
             fail-closed with the same message as validate_embedding_dim, got: {err:?}"
        );
    }

    // --- Storage::drop_table -----------------------------------------------
    // `drop_table` の内部不変条件（世代 +1・`ROWS_TABLE` 非接触）をこの unit test
    // モジュールに置き、公開契約（未存在・識別子不正・再作成別次元）の固定は
    // `tests/catalog.rs`（クレート外の統合テスト）側に委譲する（Issue #179）。

    #[test]
    fn drop_table_removes_catalog_entry() {
        let path = unique_db_path("drop-table-removes-entry");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");

        storage.drop_table("docs").expect("drop table");

        assert!(matches!(
            storage.get_table_schema("docs"),
            Err(CatalogError::TableNotFound(_))
        ));
        assert!(!storage
            .list_tables()
            .expect("list tables")
            .iter()
            .any(|t| t == "docs"));
    }

    // codex-review P1 再指摘（PR #266）「新設する場合は書き込み経路での更新漏れが
    // ないことをテストで担保」対応: `bump_table_generation_in_txn` を呼ぶすべての
    // カタログ層 API（`create_table`・`alter_table_add_column`・
    // `insert_row_into_table`・`insert_rows_into_table`・`Storage::insert_typed_row`・
    // `drop_table`）が対象テーブル（`docs`）の世代を実際に進めること、かつ無関係な
    // 別テーブル（`sibling`）の世代には一切影響しないことを固定する。空バッチ
    // （`insert_rows_into_table` の `rows.is_empty()` 早期 return）は commit 自体を
    // 行わない既存契約のとおり世代を進めないことも合わせて固定する
    // （`tenant.rs` 側の書き込み API は
    // `write_apis_bump_only_the_written_tables_generation` で別途カバーする）。
    #[test]
    fn catalog_write_apis_bump_only_the_written_tables_generation() {
        let path = unique_db_path("table-generation-bump-coverage-catalog");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let read_gen = |name: &str| -> u64 {
            let read_txn = storage.db().begin_read().expect("begin read");
            table_generation_in_txn(&read_txn, name).expect("read table generation")
        };

        // 無関係な「sibling」テーブルを先に作る。以降の全操作を通じて
        // `sibling` の世代が一切変化しないことを都度確認する。
        storage
            .create_table(&TableSchema::new(
                "sibling",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create sibling");
        let sibling_gen = read_gen("sibling");
        assert_eq!(
            sibling_gen, 1,
            "create_table must bump its own table's generation"
        );

        assert_eq!(read_gen("docs"), 0, "未作成テーブルの世代は 0");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create docs");
        let mut prev = read_gen("docs");
        assert!(prev > 0, "create_table must bump docs' generation");
        assert_eq!(read_gen("sibling"), sibling_gen);

        storage
            .alter_table_add_column("docs", ColumnDef::new("path", ColumnType::Text, true))
            .expect("alter_table_add_column");
        let next = read_gen("docs");
        assert!(
            next > prev,
            "alter_table_add_column must bump docs' generation"
        );
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.1, 0.2],
                    metadata: &[],
                },
            )
            .expect("insert_row_into_table");
        let next = read_gen("docs");
        assert!(
            next > prev,
            "insert_row_into_table must bump docs' generation"
        );
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        storage
            .insert_rows_into_table(
                "docs",
                &[(
                    2,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[0.3, 0.4],
                        metadata: &[],
                    },
                )],
            )
            .expect("insert_rows_into_table (non-empty)");
        let next = read_gen("docs");
        assert!(
            next > prev,
            "insert_rows_into_table (non-empty) must bump docs' generation"
        );
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        // 空バッチは commit 自体を行わない既存契約（`insert_rows_into_table` の
        // ドキュメントコメント参照）のとおり、世代を進めない。
        storage
            .insert_rows_into_table("docs", &[])
            .expect("insert_rows_into_table (empty)");
        assert_eq!(
            read_gen("docs"),
            prev,
            "insert_rows_into_table with an empty batch must not bump the generation"
        );
        assert_eq!(read_gen("sibling"), sibling_gen);

        storage
            .insert_typed_row(
                "docs",
                3,
                "tenant-a",
                Visibility::Public,
                &[
                    RowCodecValue::Vector(vec![0.5, 0.6]),
                    RowCodecValue::Text("typed-path".to_string()),
                ],
            )
            .expect("insert_typed_row");
        let next = read_gen("docs");
        assert!(next > prev, "insert_typed_row must bump docs' generation");
        prev = next;
        assert_eq!(read_gen("sibling"), sibling_gen);

        storage.drop_table("docs").expect("drop_table");
        let next = read_gen("docs");
        assert!(next > prev, "drop_table must bump docs' generation");
        assert_eq!(read_gen("sibling"), sibling_gen);
    }

    #[test]
    fn drop_table_rejects_missing_table_and_invalid_identifier() {
        let path = unique_db_path("drop-table-rejects-missing");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // カタログテーブル自体が未作成（1 テーブルも定義していない DB）でも
        // `TableNotFound` に丸め込む（`Backend` 等の内部エラー種別を露出しない）。
        assert!(matches!(
            storage.drop_table("docs"),
            Err(CatalogError::TableNotFound(_))
        ));

        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");

        assert!(matches!(
            storage.drop_table("missing"),
            Err(CatalogError::TableNotFound(_))
        ));
        assert!(matches!(
            storage.drop_table("bad/name"),
            Err(CatalogError::Invalid(_))
        ));
    }

    #[test]
    fn drop_table_then_recreate_with_different_dim_starts_empty() {
        let path = unique_db_path("drop-table-recreate-dim");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");
        storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row");

        storage.drop_table("docs").expect("drop table");

        // drop 直後（再作成前）はカタログ・行の双方が不存在扱いになる。
        assert!(matches!(
            storage.get_row_from_table("docs", "tenant-a", 1),
            Err(CatalogError::TableNotFound(_))
        ));
        assert!(matches!(
            storage.scan_table_page("docs", None, 10),
            Err(CatalogError::TableNotFound(_))
        ));

        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(4), false)],
            ))
            .expect("recreate table with different dim");
        storage
            .insert_row_into_table(
                "docs",
                2,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0, 0.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert row with new dim");

        let (rows, _cursor) = storage
            .scan_table_page("docs", None, 10)
            .expect("scan after recreate");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, 2);
    }

    #[test]
    fn drop_table_bumps_generation_exactly_once() {
        let path = unique_db_path("drop-table-generation");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");

        let before = storage.current_generation().expect("read generation");
        storage.drop_table("docs").expect("drop table");
        let after = storage.current_generation().expect("read generation");

        assert_eq!(after, before + 1);
    }

    // drop 後の同名テーブル再作成で旧 `op_ledger` エントリが引き継がれ、正当な
    // 書き込みを誤って重複拒否させない（Issue #226 レビュー対応: TASK-93/RECOVER-2）。
    #[test]
    fn drop_table_removes_op_ledger_entries_for_that_table() {
        use crate::recovery::content_hash::ContentHash;
        use crate::recovery::ledger::{contains_in_read_txn, record_in_txn, LedgerWrite};
        use crate::recovery::required_op_id::OperationId;

        let path = unique_db_path("drop-table-op-ledger");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");

        let op_id = OperationId::parse("op-drop-1").expect("valid operation_id");
        let write_txn = storage.db().begin_write().expect("begin write");
        record_in_txn(
            &write_txn,
            "tenant-a",
            "docs",
            LedgerWrite::Record(&op_id),
            &ContentHash::for_test(b"content"),
        )
        .expect("record op ledger entry");
        write_txn.commit().expect("commit ledger record");

        storage.drop_table("docs").expect("drop table");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("recreate table with same name");

        let read_txn = storage.db().begin_read().expect("begin read");
        let found = contains_in_read_txn(&read_txn, "tenant-a", "docs", &op_id)
            .expect("contains after recreate");
        assert!(
            !found,
            "op_ledger entry from the dropped table must not survive into the recreated table"
        );
    }

    #[test]
    fn drop_table_does_not_touch_legacy_rows_table() {
        let path = unique_db_path("drop-table-legacy-rows");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");
        storage
            .put(
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("put legacy row");

        storage.drop_table("docs").expect("drop table");

        let row = storage
            .get("tenant-a", 1)
            .expect("legacy row still readable");
        assert_eq!(row.embedding, vec![1.0, 0.0]);
    }

    // --- Storage::list_tables ---------------------------------------------
    // MAX_LIST_TABLES 上限超過時の Err 分岐（security.md「無制限リソース確保」対応）と、
    // カタログテーブル未作成（空 DB）時の Ok(Vec::new()) 分岐を検証する。
    // `MAX_LIST_TABLES` / `CATALOG_TABLE` が非公開のため、`tests/catalog.rs`
    // （クレート外の統合テスト）ではなくこの unit test モジュールに置く。

    #[test]
    fn list_tables_returns_empty_vec_when_catalog_table_not_yet_created() {
        let path = unique_db_path("list-tables-empty");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // 1 テーブルも create_table していない状態（catalog テーブル自体が未作成）。
        let tables = storage.list_tables().expect("list_tables on empty db");
        assert!(tables.is_empty());
    }

    #[test]
    fn list_tables_rejects_when_exceeding_max_list_tables() {
        let path = unique_db_path("list-tables-exceeds-max");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // MAX_LIST_TABLES を超える件数を用意する。create_table を MAX_LIST_TABLES+1 回
        // 呼ぶと write txn ごとのコミットコストでテストが極端に遅くなるため、
        // 単一の write txn へ直接まとめて挿入する（create_table 自体の性能特性検証は
        // table4 系の統合テストの責務であり、ここでの目的は list_tables 自体の
        // DoS 対策（security.md「無制限リソース確保」）の検証）。
        let schema = TableSchema::new(
            "seed",
            vec![ColumnDef::new("body", ColumnType::Text, false)],
        );
        let encoded = encode_schema(&schema).expect("encode seed schema");
        {
            let write_txn = storage.db().begin_write().expect("begin_write");
            {
                let mut table = write_txn.open_table(CATALOG_TABLE).expect("open_table");
                for i in 0..=MAX_LIST_TABLES {
                    let name = format!("t{i}");
                    table
                        .insert(name.as_str(), encoded.as_slice())
                        .expect("insert seed row");
                }
            }
            write_txn.commit().expect("commit");
        }

        let result = storage.list_tables();
        assert!(
            matches!(result, Err(CatalogError::Invalid(_))),
            "expected Err(Invalid) once table count exceeds MAX_LIST_TABLES, got {result:?}"
        );
    }

    // --- Storage::insert_rows_into_table（空バッチ） -------------------------
    // TASK-133 P2 対応: 空バッチは既存行・スキーマを一切変更しないため、世代カウンタ
    // （`crate::storage::bump_generation_and_commit` が管理）を進めてはならない
    // （進めると空バッチだけで既存 `PrefilterIndex` を不要に失効させてしまう）。

    #[test]
    fn insert_rows_into_table_with_empty_batch_does_not_bump_generation() {
        let path = unique_db_path("insert-rows-empty-batch-generation");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table");

        // `create_table` 自体の世代（DDL も commit のたびに世代を進める）を基準にする。
        let generation_before = storage.current_generation().expect("generation before");

        storage
            .insert_rows_into_table("docs", &[])
            .expect("empty batch insert must succeed as a no-op");

        assert_eq!(
            storage.current_generation().expect("generation after"),
            generation_before,
            "empty batch insert must not bump the storage generation counter"
        );

        // 対称性の確認: 実際に行を書き込むバッチは引き続き世代を進める。
        storage
            .insert_rows_into_table(
                "docs",
                &[(
                    1,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: crate::storage::Visibility::Public,
                        embedding: &[1.0, 0.0],
                        metadata: &[],
                    },
                )],
            )
            .expect("non-empty batch insert");
        assert_eq!(
            storage
                .current_generation()
                .expect("generation after non-empty batch"),
            generation_before + 1,
            "a non-empty batch insert must still bump the storage generation counter"
        );
    }

    // --- insert_typed_row（TASK-75、SQL-1〜4 の結合テスト共通入口） -----------------

    #[test]
    fn insert_typed_row_round_trips_embedding_and_scalar_columns() {
        let path = unique_db_path("insert-typed-row-roundtrip");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(3), false),
                    ColumnDef::new("body", ColumnType::Text, false),
                    ColumnDef::new("lang", ColumnType::Text, true),
                ],
            ))
            .expect("create table");

        storage
            .insert_typed_row(
                "docs",
                1,
                "tenant-a",
                Visibility::Public,
                &[
                    RowCodecValue::Vector(vec![1.0, 2.0, 3.0]),
                    RowCodecValue::Text("hello".to_string()),
                    RowCodecValue::Text("ja".to_string()),
                ],
            )
            .expect("insert typed row");

        let row = storage
            .get_row_from_table("docs", "tenant-a", 1)
            .expect("get row");
        assert_eq!(row.embedding, vec![1.0, 2.0, 3.0]);
        let schema = storage.get_table_schema("docs").expect("get schema");
        let decoded =
            row_codec::decode_scalar_columns(&schema, &row.metadata).expect("decode scalar");
        assert_eq!(decoded[1], RowCodecValue::Text("hello".to_string()));
        assert_eq!(decoded[2], RowCodecValue::Text("ja".to_string()));
    }

    #[test]
    fn insert_typed_row_rejects_embedding_dim_mismatch() {
        let path = unique_db_path("insert-typed-row-dim-mismatch");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
            ))
            .expect("create table");

        let result = storage.insert_typed_row(
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[RowCodecValue::Vector(vec![1.0, 2.0])],
        );
        assert!(matches!(result, Err(CatalogError::Invalid(_))));
    }

    #[test]
    fn insert_typed_row_rejects_missing_vector_column_value() {
        let path = unique_db_path("insert-typed-row-missing-vector");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
            ))
            .expect("create table");

        let result = storage.insert_typed_row(
            "docs",
            1,
            "tenant-a",
            Visibility::Public,
            &[RowCodecValue::Null],
        );
        assert!(matches!(result, Err(CatalogError::Invalid(_))));
    }

    // Issue #131: convert_storage_error はカタログ層（テーブルスコープ）専用の呼び出し元
    // であるという内部コンテキストを前提に、`scan_table_page` への正確な代替手段案内を
    // 生成することを固定する（`Storage::scan_batch_log` はこの経路を通らない）。

    #[test]
    fn convert_storage_error_maps_scan_limit_to_table_page_guidance() {
        let err = convert_storage_error(crate::storage::StorageError::ScanLimitExceeded);
        match err {
            CatalogError::Invalid(msg) => assert!(msg.contains("scan_table_page"), "{msg}"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    /// 取りこぼし検査（Issue #849）: `catalog.rs` が提供する DDL・生行挿入経路
    /// （`create_table`・`drop_table`・`alter_table_add_column`・
    /// `insert_row_into_table`・`insert_rows_into_table`・`insert_typed_row`）が
    /// [`Storage::begin_write_txn`] choke point を 1 回ずつ経由することを、呼び出し
    /// 前後の `write_txn_creations()` の差分で非 vacuous に確認する
    /// （`tenant.rs::tests::all_tenant_write_paths_go_through_storage_choke_point` の
    /// catalog 層対応）。
    #[test]
    fn all_catalog_write_paths_go_through_storage_choke_point() {
        let path = unique_db_path("durability-choke-point-catalog");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let mut expected: u64 = 0;

        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create_table");
        expected += 1;
        assert_eq!(storage.write_txn_creations(), expected, "create_table");

        storage
            .alter_table_add_column("docs", ColumnDef::new("note", ColumnType::Text, true))
            .expect("alter_table_add_column");
        expected += 1;
        assert_eq!(
            storage.write_txn_creations(),
            expected,
            "alter_table_add_column"
        );

        storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0, 0.0],
                    metadata: &[],
                },
            )
            .expect("insert_row_into_table");
        expected += 1;
        assert_eq!(
            storage.write_txn_creations(),
            expected,
            "insert_row_into_table"
        );

        storage
            .insert_rows_into_table(
                "docs",
                &[(
                    2,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[0.0, 1.0],
                        metadata: &[],
                    },
                )],
            )
            .expect("insert_rows_into_table");
        expected += 1;
        assert_eq!(
            storage.write_txn_creations(),
            expected,
            "insert_rows_into_table"
        );

        storage
            .insert_typed_row(
                "docs",
                3,
                "tenant-a",
                Visibility::Public,
                &[RowCodecValue::Vector(vec![1.0, 1.0])],
            )
            .expect("insert_typed_row");
        expected += 1;
        assert_eq!(storage.write_txn_creations(), expected, "insert_typed_row");

        storage.drop_table("docs").expect("drop_table");
        expected += 1;
        assert_eq!(storage.write_txn_creations(), expected, "drop_table");
    }

    // --- ALTER TABLE DROP COLUMN / ALTER COLUMN TYPE（TABLE-19・TASK-203、
    // Issue #901）------------------------------------------------------------

    /// DROP COLUMN はカタログのみを書き換える O(1) 操作であり、既存行の物理
    /// バイト列には一切触れないこと（TABLE-19 D1 の核心）を、削除前後で
    /// 生バイト列が完全一致することにより固定する。
    #[test]
    fn alter_table_drop_column_preserves_existing_row_bytes_and_reads_correctly() {
        let path = unique_db_path("drop-column-preserves-bytes");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("body", ColumnType::Text, false),
                    ColumnDef::new("kind", ColumnType::Text, true),
                ],
            ))
            .expect("create table");
        storage
            .insert_typed_row(
                "docs",
                1,
                "tenant-a",
                Visibility::Public,
                &[
                    RowCodecValue::Vector(vec![1.0, 2.0]),
                    RowCodecValue::Text("hello".to_string()),
                    RowCodecValue::Text("ja".to_string()),
                ],
            )
            .expect("insert typed row");

        let before = storage
            .get_row_from_table("docs", "tenant-a", 1)
            .expect("get row before drop");

        storage
            .alter_table_drop_column("docs", "kind")
            .expect("drop column");

        let after = storage
            .get_row_from_table("docs", "tenant-a", 1)
            .expect("get row after drop");
        assert_eq!(
            before.metadata, after.metadata,
            "DROP COLUMN must not rewrite existing row bytes"
        );
        assert_eq!(before.embedding, after.embedding);

        let schema = storage.get_table_schema("docs").expect("get schema");
        assert_eq!(
            schema.columns.len(),
            2,
            "kind must be removed from logical columns"
        );
        assert!(schema.columns.iter().all(|c| c.name != "kind"));
        assert_eq!(schema.dropped_slots().len(), 1);

        let decoded =
            row_codec::decode_scalar_columns(&schema, &after.metadata).expect("decode scalar");
        assert_eq!(decoded.len(), 2);
        assert_eq!(decoded[1], RowCodecValue::Text("hello".to_string()));
    }

    /// 予約列（`id`／`tenant_id`／`visibility`）・`VECTOR` 列・不存在列・最後の
    /// 1 列はいずれも fail-closed に拒否し、カタログを一切変更しない（TABLE-19
    /// D1）。
    #[test]
    fn alter_table_drop_column_rejects_protected_missing_and_last_column() {
        let path = unique_db_path("drop-column-rejections");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            ))
            .expect("create table");

        assert!(matches!(
            storage.alter_table_drop_column("docs", "id"),
            Err(CatalogError::ProtectedColumn(_))
        ));
        assert!(matches!(
            storage.alter_table_drop_column("docs", "tenant_id"),
            Err(CatalogError::ProtectedColumn(_))
        ));
        assert!(matches!(
            storage.alter_table_drop_column("docs", "visibility"),
            Err(CatalogError::ProtectedColumn(_))
        ));
        assert!(matches!(
            storage.alter_table_drop_column("docs", "embedding"),
            Err(CatalogError::ProtectedColumn(_))
        ));
        assert!(matches!(
            storage.alter_table_drop_column("docs", "missing"),
            Err(CatalogError::ColumnNotFound(_))
        ));
        assert!(matches!(
            storage.alter_table_drop_column("missing_table", "body"),
            Err(CatalogError::TableNotFound(_))
        ));

        // カタログは一切変わっていないことを確認する。
        let schema = storage.get_table_schema("docs").expect("get schema");
        assert_eq!(schema.columns.len(), 2);
        assert!(schema.dropped_slots().is_empty());

        // `VECTOR` 列を持たないテーブルで唯一の列を消すと生存列が 0 本になる
        // ため拒否する。
        storage
            .create_table(&TableSchema::new(
                "scalar_only",
                vec![ColumnDef::new("body", ColumnType::Text, false)],
            ))
            .expect("create scalar-only table");
        assert!(matches!(
            storage.alter_table_drop_column("scalar_only", "body"),
            Err(CatalogError::Invalid(_))
        ));
    }

    /// 削除後に同名列を再追加すると独立した新しい物理スロットを得て、削除前の
    /// 行の値は復活しない（TABLE-19 D1）。削除後に書き込む行の墓標位置には
    /// 常に NULL が書かれる（`row_codec::encode_scalar_columns`）ことも併せて
    /// 確認する。
    #[test]
    fn alter_table_drop_column_then_readd_same_name_does_not_resurrect_old_value() {
        let path = unique_db_path("drop-column-readd");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("kind", ColumnType::Text, true),
                ],
            ))
            .expect("create table");
        storage
            .insert_typed_row(
                "docs",
                1,
                "tenant-a",
                Visibility::Public,
                &[
                    RowCodecValue::Vector(vec![1.0, 2.0]),
                    RowCodecValue::Text("ja".to_string()),
                ],
            )
            .expect("insert row1 before drop");

        storage
            .alter_table_drop_column("docs", "kind")
            .expect("drop column");
        storage
            .alter_table_add_column("docs", ColumnDef::new("kind", ColumnType::Text, true))
            .expect("re-add column with the same name");

        storage
            .insert_typed_row(
                "docs",
                2,
                "tenant-a",
                Visibility::Public,
                &[
                    RowCodecValue::Vector(vec![3.0, 4.0]),
                    RowCodecValue::Text("re-added".to_string()),
                ],
            )
            .expect("insert row2 after re-add");

        let schema = storage.get_table_schema("docs").expect("get schema");
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.dropped_slots().len(), 1);

        let row1 = storage
            .get_row_from_table("docs", "tenant-a", 1)
            .expect("get row1");
        let decoded1 =
            row_codec::decode_scalar_columns(&schema, &row1.metadata).expect("decode row1");
        assert_eq!(
            decoded1[1],
            RowCodecValue::Null,
            "old kind value must not resurrect under the re-added column"
        );

        let row2 = storage
            .get_row_from_table("docs", "tenant-a", 2)
            .expect("get row2");
        let decoded2 =
            row_codec::decode_scalar_columns(&schema, &row2.metadata).expect("decode row2");
        assert_eq!(decoded2[1], RowCodecValue::Text("re-added".to_string()));
    }

    /// カタログ v3（墓標あり）は DB の再オープンを跨いで正しく往復する
    /// （TABLE-19 D2）。
    #[test]
    fn alter_table_drop_column_schema_survives_reopen() {
        let path = unique_db_path("drop-column-reopen");
        let _guard = CleanupGuard(path.clone());
        {
            let storage = Storage::open(&path).expect("open storage");
            storage
                .create_table(&TableSchema::new(
                    "docs",
                    vec![
                        ColumnDef::new("embedding", ColumnType::Vector(2), false),
                        ColumnDef::new("body", ColumnType::Text, false),
                        ColumnDef::new("kind", ColumnType::Text, true),
                    ],
                ))
                .expect("create table");
            storage
                .insert_typed_row(
                    "docs",
                    1,
                    "tenant-a",
                    Visibility::Public,
                    &[
                        RowCodecValue::Vector(vec![1.0, 2.0]),
                        RowCodecValue::Text("hello".to_string()),
                        RowCodecValue::Text("ja".to_string()),
                    ],
                )
                .expect("insert typed row");
            storage
                .alter_table_drop_column("docs", "kind")
                .expect("drop column");
        }

        let storage = Storage::open(&path).expect("reopen storage");
        let schema = storage.get_table_schema("docs").expect("get schema");
        assert_eq!(schema.columns.len(), 2);
        assert_eq!(schema.dropped_slots().len(), 1);
        let row = storage
            .get_row_from_table("docs", "tenant-a", 1)
            .expect("get row after reopen");
        let decoded =
            row_codec::decode_scalar_columns(&schema, &row.metadata).expect("decode after reopen");
        assert_eq!(decoded[1], RowCodecValue::Text("hello".to_string()));
    }

    /// `merge_encode_scalar_columns`（UPDATE の read-merge-write 経路）も、
    /// 墓標位置には既存値（`existing`）・SET 対象（`overrides`）のいずれに
    /// 関わらず常に NULL を書く（TABLE-19 D1）。
    #[test]
    fn merge_encode_scalar_columns_always_nulls_dropped_slots() {
        let path = unique_db_path("drop-column-merge-encode");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("body", ColumnType::Text, false),
                    ColumnDef::new("kind", ColumnType::Text, true),
                ],
            ))
            .expect("create table");
        storage
            .insert_typed_row(
                "docs",
                1,
                "tenant-a",
                Visibility::Public,
                &[
                    RowCodecValue::Vector(vec![1.0, 2.0]),
                    RowCodecValue::Text("hello".to_string()),
                    RowCodecValue::Text("ja".to_string()),
                ],
            )
            .expect("insert typed row");
        storage
            .alter_table_drop_column("docs", "kind")
            .expect("drop column");

        let schema = storage.get_table_schema("docs").expect("get schema");
        let row = storage
            .get_row_from_table("docs", "tenant-a", 1)
            .expect("get row");
        let existing = row_codec::scan_scalar_columns(&schema, &row.metadata).expect("scan");
        let updated_value = RowCodecValue::Text("updated".to_string());
        let overrides: Vec<(usize, &RowCodecValue)> = vec![(1, &updated_value)];
        let merged = row_codec::merge_encode_scalar_columns(&schema, &existing, &overrides)
            .expect("merge encode");
        let decoded = row_codec::decode_scalar_columns(&schema, &merged).expect("decode merged");
        assert_eq!(decoded[1], RowCodecValue::Text("updated".to_string()));
    }

    /// `NUMERIC(p, s)` の精度拡大（`ALTER COLUMN ... TYPE NUMERIC(p', s)`。
    /// TABLE-19 D3）はカタログのみを書き換え、既存値は不変のまま新しい精度の
    /// 下でも有効に読み出せる。
    #[test]
    fn alter_table_widen_numeric_precision_preserves_existing_values() {
        let path = unique_db_path("widen-numeric-precision");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new(
                        "amount",
                        ColumnType::Numeric {
                            precision: 5,
                            scale: 2,
                        },
                        true,
                    ),
                ],
            ))
            .expect("create table");
        let decimal = crate::numeric::Decimal::from_parts(12345, 2).expect("valid decimal");
        storage
            .insert_typed_row(
                "docs",
                1,
                "tenant-a",
                Visibility::Public,
                &[
                    RowCodecValue::Vector(vec![1.0, 2.0]),
                    RowCodecValue::Numeric(decimal),
                ],
            )
            .expect("insert typed row");

        storage
            .alter_table_widen_numeric_precision("docs", "amount", 10)
            .expect("widen precision");

        let schema = storage.get_table_schema("docs").expect("get schema");
        assert_eq!(
            schema.columns[1].ty,
            ColumnType::Numeric {
                precision: 10,
                scale: 2
            }
        );
        let row = storage
            .get_row_from_table("docs", "tenant-a", 1)
            .expect("get row");
        let decoded = row_codec::decode_scalar_columns(&schema, &row.metadata).expect("decode");
        assert_eq!(decoded[1], RowCodecValue::Numeric(decimal));

        // 拡大後の精度でしか表現できない値も新規に書き込めることを確認する。
        let large = crate::numeric::Decimal::from_parts(1_234_567_890, 2).expect("valid decimal");
        storage
            .insert_typed_row(
                "docs",
                2,
                "tenant-a",
                Visibility::Public,
                &[
                    RowCodecValue::Vector(vec![3.0, 4.0]),
                    RowCodecValue::Numeric(large),
                ],
            )
            .expect("insert typed row within widened precision");
    }

    /// 縮小・同一精度・異種型（`NUMERIC` 以外）への変更はいずれも
    /// `Err(CatalogError::IncompatibleTypeChange)` で拒否し、カタログを変更
    /// しない（TABLE-19 D3）。
    #[test]
    fn alter_table_widen_numeric_precision_rejects_non_widening_changes() {
        let path = unique_db_path("widen-numeric-rejections");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new(
                        "amount",
                        ColumnType::Numeric {
                            precision: 5,
                            scale: 2,
                        },
                        true,
                    ),
                    ColumnDef::new("body", ColumnType::Text, true),
                ],
            ))
            .expect("create table");

        assert!(matches!(
            storage.alter_table_widen_numeric_precision("docs", "amount", 5),
            Err(CatalogError::IncompatibleTypeChange { .. })
        ));
        assert!(matches!(
            storage.alter_table_widen_numeric_precision("docs", "amount", 3),
            Err(CatalogError::IncompatibleTypeChange { .. })
        ));
        assert!(matches!(
            storage.alter_table_widen_numeric_precision("docs", "body", 10),
            Err(CatalogError::IncompatibleTypeChange { .. })
        ));
        assert!(matches!(
            storage.alter_table_widen_numeric_precision("docs", "missing", 10),
            Err(CatalogError::ColumnNotFound(_))
        ));
        assert!(matches!(
            storage.alter_table_widen_numeric_precision("docs", "id", 10),
            Err(CatalogError::ProtectedColumn(_))
        ));

        let schema = storage.get_table_schema("docs").expect("get schema");
        assert_eq!(
            schema.columns[1].ty,
            ColumnType::Numeric {
                precision: 5,
                scale: 2
            },
            "rejected ALTER COLUMN TYPE must not mutate the catalog"
        );
    }

    /// [`Storage::create_view`] は `pub fn` の Rust API であり、SQL 表層の
    /// `validate_create_view_tokens`（`body_sql` を必ず `base_relation` と
    /// 対応づけて生成する）を経由しない呼び出し元も存在しうる（codex-review
    /// 指摘・PR #1048）。`body_sql` の `FROM` が主張する参照先と `base_relation`
    /// 引数が食い違う場合は `CatalogError::Invalid` で拒否し、何も永続化しない。
    #[test]
    fn create_view_rejects_body_sql_from_target_mismatching_base_relation() {
        let path = unique_db_path("create-view-mismatched-base-relation");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table docs");
        storage
            .create_table(&TableSchema::new(
                "other",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table other");

        // `base_relation` は "docs" だが、`body_sql` 自身の FROM は "other" を
        // 指す。SQL 表層経由（`render_view_body`）では構造的に発生しない食い違い。
        let err = storage
            .create_view("v", "docs", "SELECT id FROM other")
            .expect_err("body_sql FROM target must match base_relation");
        assert!(matches!(err, CatalogError::Invalid(_)));
        assert!(
            storage.view_definition("v").expect("view lookup").is_none(),
            "mismatched view definition must not be persisted"
        );
    }

    /// [`Storage::create_view`] は `body_sql` が許可リスト形状（式項目・UDF
    /// 述語を含まない `SELECT <* | 列名> FROM <relation> [WHERE <単純述語>]`）
    /// を満たさない場合も `CatalogError::Invalid` で拒否する（同上。`sql::view::
    /// resolve_from` が再パースする際に想定していない構文が紛れ込むのを
    /// 保存前に防ぐ）。
    #[test]
    fn create_view_rejects_body_sql_outside_allowed_shape() {
        let path = unique_db_path("create-view-disallowed-shape");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create table docs");

        let err = storage
            .create_view("v", "docs", "SELECT vec_norm(embedding) FROM docs")
            .expect_err("expression projection items are outside the allowed view body shape");
        assert!(matches!(err, CatalogError::Invalid(_)));
        assert!(
            storage.view_definition("v").expect("view lookup").is_none(),
            "invalid view definition must not be persisted"
        );
    }

    // --- CHECK 制約・カタログ v7（TABLE-16・TASK-204、Issue #906） ---------

    fn check(name: &str, columns: &[&str], predicate_sql: &str) -> CheckConstraint {
        CheckConstraint {
            name: name.to_string(),
            columns: columns.iter().map(|c| c.to_string()).collect(),
            predicate_sql: predicate_sql.to_string(),
        }
    }

    /// `CHECK` 制約を持たないスキーマは v2〜v6 のバイト列を変えない（`checks`
    /// フィールド追加による既存形式の変化が無いことの固定）。
    #[test]
    fn encode_schema_without_checks_keeps_existing_formats() {
        let plain = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        );
        assert_eq!(
            encode_schema(&plain).expect("encode"),
            b"v2\ncols:2\nembedding:vector:3:0\nbody:text:-:1\n".to_vec()
        );
        let unique = plain
            .clone()
            .with_unique_constraints(vec![UniqueConstraint::new(vec!["body".to_string()])]);
        assert!(encode_schema(&unique).expect("encode").starts_with(b"v6\n"));
    }

    /// `CHECK` を 1 つ以上持つスキーマは v7 で書かれ、往復でビット同一のスキーマへ
    /// 戻る（UNIQUE 0 件・主キーあり・`DEFAULT`・墓標の各組み合わせ）。
    #[test]
    fn encode_decode_roundtrips_v7_with_check_constraints() {
        let base = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("kind", ColumnType::Text, true),
                ColumnDef::new("status", ColumnType::Text, false),
            ],
        )
        .with_checks(vec![
            check("docs_kind_check", &["kind"], "kind = 'a'"),
            check(
                "multi",
                &["kind", "status"],
                "kind = 'a:b' AND status LIKE 'x%'",
            ),
            check("docs_check", &[], "vec_norm(embedding) < 100"),
        ]);
        let encoded = encode_schema(&base).expect("encode");
        let text = std::str::from_utf8(&encoded).expect("utf8");
        assert!(text.starts_with("v7\ncols:3\npk:\n"), "{text}");
        assert!(
            text.contains("\nuniq:0\nchecks:3\ncheck:docs_kind_check:kind:"),
            "{text}"
        );
        assert_eq!(decode_schema("docs", &encoded).expect("decode"), base);

        let with_pk_unique = base
            .clone()
            .with_primary_key(vec!["status".to_string()])
            .with_unique_constraints(vec![UniqueConstraint::new(vec!["kind".to_string()])]);
        let encoded = encode_schema(&with_pk_unique).expect("encode");
        assert!(encoded.starts_with(b"v7\ncols:3\npk:status\n"));
        // 名前未指定の UNIQUE 制約は decode 時に既定名を導出する（設計 D2・
        // Issue #1067）。
        assert_eq!(
            decode_schema("docs", &encoded).expect("decode"),
            assign_unique_constraint_names(with_pk_unique)
        );

        let with_dropped = TableSchema::from_parts(
            "docs",
            vec![ColumnDef::new("kind", ColumnType::Text, true)
                .with_default(ColumnDefault::Text("a".to_string()))],
            vec![DroppedSlot {
                physical_index: 0,
                name: "old".to_string(),
                ty: ColumnType::Text,
            }],
            None,
            Vec::new(),
        )
        .with_checks(vec![check("k", &["kind"], "kind = 'a'")]);
        let encoded = encode_schema(&with_dropped).expect("encode");
        assert_eq!(
            decode_schema("docs", &encoded).expect("decode"),
            with_dropped
        );
    }

    /// v7 の破損入力（`checks:0`・件数不足・不正 hex・未知参照列・制約名重複・
    /// 余剰行・`checks:` 行欠落）を fail-closed に拒否する。`DROP TYPE` の依存判定
    /// （`catalog_value_references_enum_type`）も同じ値を「依存なし」に丸めない。
    #[test]
    fn decode_v7_rejects_corrupt_check_section() {
        let head = "v7\ncols:2\npk:\nmood_col:enum:mood:1:L:-\nkind:text:-:1:L:-\nuniq:0\n";
        let hex = |s: &str| -> String { s.bytes().map(hex_encode_byte).collect() };
        let pred = hex("kind = 'a'");
        let valid = format!("{head}checks:1\ncheck:c1:kind:{pred}\n");
        assert!(catalog_value_references_enum_type(valid.as_bytes(), "mood").expect("valid v7"));
        let corrupt_values = [
            format!("{head}checks:0\n"),
            format!("{head}checks:1\n"),
            head.to_string(),
            format!("{head}checks:1\ncheck:c1:kind:abc\n"),
            format!("{head}checks:1\ncheck:c1:kind:zz\n"),
            format!("{head}checks:1\ncheck:c1:missing:{pred}\n"),
            format!("{head}checks:2\ncheck:c1:kind:{pred}\ncheck:c1:kind:{pred}\n"),
            format!("{head}checks:1\ncheck:c1:kind:{pred}\nsurplus\n"),
            format!("{head}checks:1\ncheck:c1:kind:{pred}:extra\n"),
            format!("{head}checks:1\nchk:c1:kind:{pred}\n"),
            format!("{head}checks:1\ncheck:c1:kind:\n"),
            format!("{head}checks:33\n"),
        ];
        for corrupt in &corrupt_values {
            let resolve = &mut |name: &str| -> Result<Arc<EnumTypeDef>> {
                Ok(Arc::new(EnumTypeDef {
                    name: name.to_string(),
                    labels: vec!["x".to_string()],
                }))
            };
            assert!(
                matches!(
                    decode_schema_with_resolver("docs", corrupt.as_bytes(), resolve),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "decode must reject {corrupt:?}"
            );
        }
        // `DROP TYPE` 側の軽量パーサーも同じ破損値をすべて拒否する（decode より
        // 緩くならない）。
        for corrupt in &corrupt_values {
            assert!(
                matches!(
                    catalog_value_references_enum_type(corrupt.as_bytes(), "mood"),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "enum dependency parser must reject {corrupt:?}"
            );
        }
    }

    fn fk(columns: &[&str], parent: &str, parent_columns: &[&str]) -> ForeignKeyDef {
        ForeignKeyDef::new(
            columns.iter().map(|c| c.to_string()).collect(),
            parent.to_string(),
            parent_columns.iter().map(|c| c.to_string()).collect(),
            ReferentialAction::NoAction,
            ReferentialAction::NoAction,
        )
    }

    fn fk_with_actions(
        columns: &[&str],
        parent: &str,
        parent_columns: &[&str],
        on_delete: ReferentialAction,
        on_update: ReferentialAction,
    ) -> ForeignKeyDef {
        ForeignKeyDef::new(
            columns.iter().map(|c| c.to_string()).collect(),
            parent.to_string(),
            parent_columns.iter().map(|c| c.to_string()).collect(),
            on_delete,
            on_update,
        )
    }

    /// `FOREIGN KEY` を 1 つ以上持つスキーマは v8 で書かれ、往復でビット同一の
    /// スキーマへ戻る（TABLE-17・TASK-205、Issue #907）。`FOREIGN KEY` を持たない
    /// 同じスキーマは従来の版のまま（バイト列不変）。
    #[test]
    fn encode_decode_roundtrips_v8_with_foreign_keys() {
        let plain = TableSchema::new(
            "children",
            vec![
                ColumnDef::new("parent_id", ColumnType::BigInt, true),
                ColumnDef::new("code", ColumnType::Text, true),
            ],
        );
        let plain_bytes = encode_schema(&plain).expect("encode");
        assert!(plain_bytes.starts_with(b"v2\n"));

        let with_fk = plain.clone().with_foreign_keys(vec![
            fk(&["parent_id"], "parents", &["id"]),
            fk(&["code"], "countries", &["code"]),
        ]);
        let encoded = encode_schema(&with_fk).expect("encode");
        let text = std::str::from_utf8(&encoded).expect("utf8");
        assert_eq!(
            text,
            "v8\ncols:2\npk:\nparent_id:bigint:-:1:L:-\ncode:text:-:1:L:-\nuniq:0\nchecks:0\n\
             fks:2\nfk:parent_id:parents:id\nfk:code:countries:code\n"
        );
        // 名前未指定の FOREIGN KEY は decode 時に既定名を導出する（設計 F3・
        // Issue #1069）。
        assert_eq!(
            decode_schema("children", &encoded).expect("decode"),
            assign_foreign_key_constraint_names(with_fk)
        );

        // 主キー・UNIQUE・CHECK・自己参照（主キーを参照）との共存。
        let full = TableSchema::new(
            "nodes",
            vec![
                ColumnDef::new("k", ColumnType::Text, false),
                ColumnDef::new("parent_k", ColumnType::Text, true),
            ],
        )
        .with_primary_key(vec!["k".to_string()])
        .with_unique_constraints(vec![UniqueConstraint::new(vec!["parent_k".to_string()])])
        .with_checks(vec![check("k_ck", &["k"], "k = 'a'")])
        .with_foreign_keys(vec![fk(&["parent_k"], "nodes", &["k"])]);
        let encoded = encode_schema(&full).expect("encode");
        assert!(encoded.starts_with(b"v8\ncols:2\npk:k\n"));
        // 名前未指定の UNIQUE・FOREIGN KEY 制約は decode 時に既定名を導出する
        // （設計 D2・F3・Issue #1067・#1069）。
        assert_eq!(
            decode_schema("nodes", &encoded).expect("decode"),
            assign_foreign_key_constraint_names(assign_unique_constraint_names(full))
        );

        // 未解決（参照先列が空）の宣言は永続化しない。
        let unresolved = plain.with_foreign_keys(vec![fk(&["parent_id"], "parents", &[])]);
        assert!(matches!(
            encode_schema(&unresolved),
            Err(CatalogError::Invalid(_))
        ));
    }

    /// 参照アクション（Issue #1076）: `NO ACTION` だけの FK は v8 の 3 フィールド
    /// 形のままバイト列が不変（既存ゴールデンテストとの互換）。どちらかが
    /// `NO ACTION` 以外なら（`MATCH`・遅延属性の統合フォーマット。Issue #1077）
    /// v9 の 7 フィールド形で永続化し、往復でビット同一に戻る。
    #[test]
    fn encode_decode_roundtrips_v8_with_referential_actions() {
        let schema = TableSchema::new(
            "children",
            vec![ColumnDef::new("parent_id", ColumnType::BigInt, true)],
        )
        .with_foreign_keys(vec![fk_with_actions(
            &["parent_id"],
            "parents",
            &["id"],
            ReferentialAction::Cascade,
            ReferentialAction::SetNull,
        )]);
        let encoded = encode_schema(&schema).expect("encode");
        let text = std::str::from_utf8(&encoded).expect("utf8");
        assert_eq!(
            text,
            "v9\ncols:1\npk:\nparent_id:bigint:-:1:L:-\nuniq:0\nchecks:0\n\
             fks:1\nfk:parent_id:parents:id:cascade:setnull:simple:immediate\n"
        );
        assert_eq!(
            decode_schema("children", &encoded).expect("decode"),
            assign_foreign_key_constraint_names(schema)
        );

        // 両方 `NO ACTION` は 3 フィールド形のまま（バイト列不変）。
        let no_action = TableSchema::new(
            "children",
            vec![ColumnDef::new("parent_id", ColumnType::BigInt, true)],
        )
        .with_foreign_keys(vec![fk(&["parent_id"], "parents", &["id"])]);
        let encoded_no_action = encode_schema(&no_action).expect("encode");
        assert_eq!(
            std::str::from_utf8(&encoded_no_action).expect("utf8"),
            "v8\ncols:1\npk:\nparent_id:bigint:-:1:L:-\nuniq:0\nchecks:0\nfks:1\nfk:parent_id:parents:id\n"
        );

        // v8 は 3 フィールド固定のため、5・7 フィールド形（オプション統合フォーマット。
        // Issue #1077）は v8 のバージョン行では常に拒否する。全既定値の v9 は
        // 別途 `decode_v9_rejects_corrupt_or_all_default_foreign_key_section` が
        // 正規形の一意性違反として検証する。
        let malformed = "v8\ncols:1\npk:\nparent_id:bigint:-:1:L:-\nuniq:0\nchecks:0\n\
                          fks:1\nfk:parent_id:parents:id:noaction:noaction\n";
        assert!(decode_schema("children", malformed.as_bytes()).is_err());

        // 語彙外トークン・不正フィールド数は fail-closed に拒否する。
        for corrupt in [
            "v8\ncols:1\npk:\nparent_id:bigint:-:1:L:-\nuniq:0\nchecks:0\n\
             fks:1\nfk:parent_id:parents:id:bogus:noaction\n",
            "v8\ncols:1\npk:\nparent_id:bigint:-:1:L:-\nuniq:0\nchecks:0\n\
             fks:1\nfk:parent_id:parents:id:cascade\n",
            "v9\ncols:1\npk:\nparent_id:bigint:-:1:L:-\nuniq:0\nchecks:0\n\
             fks:1\nfk:parent_id:parents:id:cascade:setnull:simple:immediate:extra\n",
        ] {
            assert!(
                decode_schema("children", corrupt.as_bytes()).is_err(),
                "must reject {corrupt:?}"
            );
        }

        // 構造は同じでアクションだけが異なる重複宣言は拒否する（A14）。
        let dup_actions = TableSchema::new(
            "children",
            vec![ColumnDef::new("parent_id", ColumnType::BigInt, true)],
        )
        .with_foreign_keys(vec![
            fk(&["parent_id"], "parents", &["id"]),
            fk_with_actions(
                &["parent_id"],
                "parents",
                &["id"],
                ReferentialAction::Cascade,
                ReferentialAction::NoAction,
            ),
        ]);
        assert!(matches!(
            encode_schema(&dup_actions),
            Err(CatalogError::Invalid(_))
        ));
    }

    /// 参照アクション宣言時検査（Issue #1076 A8）: 常に失敗する宣言を拒否する。
    #[test]
    fn validate_foreign_keys_rejects_impossible_referential_action_declarations() {
        // (a) SET NULL だが参照元列が NOT NULL。
        let set_null_not_null = TableSchema::new(
            "children",
            vec![ColumnDef::new("parent_id", ColumnType::BigInt, false)],
        )
        .with_foreign_keys(vec![fk_with_actions(
            &["parent_id"],
            "parents",
            &["id"],
            ReferentialAction::SetNull,
            ReferentialAction::NoAction,
        )]);
        assert!(matches!(
            encode_schema(&set_null_not_null),
            Err(CatalogError::InvalidForeignKey(_))
        ));

        // (b) SET DEFAULT だが参照元列が NOT NULL かつ DEFAULT 無し。
        let set_default_no_default = TableSchema::new(
            "children",
            vec![ColumnDef::new("parent_id", ColumnType::BigInt, false)],
        )
        .with_foreign_keys(vec![fk_with_actions(
            &["parent_id"],
            "parents",
            &["id"],
            ReferentialAction::NoAction,
            ReferentialAction::SetDefault,
        )]);
        assert!(matches!(
            encode_schema(&set_default_no_default),
            Err(CatalogError::InvalidForeignKey(_))
        ));
    }

    /// v8 の破損入力を fail-closed に拒否する。`DROP TYPE` の依存判定
    /// （`catalog_value_references_enum_type`）も同じ値を「依存なし」に丸めない。
    #[test]
    fn decode_v8_rejects_corrupt_foreign_key_section() {
        let head = "v8\ncols:2\npk:\nmood_col:enum:mood:1:L:-\npid:bigint:-:1:L:-\n\
                    uniq:0\nchecks:0\n";
        let valid = format!("{head}fks:1\nfk:pid:parents:id\n");
        assert!(catalog_value_references_enum_type(valid.as_bytes(), "mood").expect("valid v8"));
        let resolve = &mut |name: &str| -> Result<Arc<EnumTypeDef>> {
            Ok(Arc::new(EnumTypeDef {
                name: name.to_string(),
                labels: vec!["x".to_string()],
            }))
        };
        assert!(decode_schema_with_resolver("docs", valid.as_bytes(), resolve).is_ok());
        let corrupt_values = [
            format!("{head}fks:0\n"),
            format!("{head}fks:1\n"),
            head.to_string(),
            format!("{head}fks:33\n"),
            format!("{head}fks:1\nfk:pid:parents\n"),
            format!("{head}fks:1\nfk:pid:parents:id:extra\n"),
            format!("{head}fks:1\nfkey:pid:parents:id\n"),
            format!("{head}fks:1\nfk::parents:id\n"),
            format!("{head}fks:1\nfk:pid:parents:\n"),
            format!("{head}fks:1\nfk:missing:parents:id\n"),
            format!("{head}fks:1\nfk:pid,pid:parents:a,b\n"),
            format!("{head}fks:1\nfk:pid,mood_col:parents:id,a\n"),
            format!("{head}fks:1\nfk:pid:parents:a,b\n"),
            format!("{head}fks:2\nfk:pid:parents:id\nfk:pid:parents:id\n"),
            format!("{head}fks:1\nfk:pid:parents:id\nsurplus\n"),
            format!("{head}fks:1\nfk:pid:bad-name:id\n"),
        ];
        for corrupt in &corrupt_values {
            assert!(
                matches!(
                    decode_schema_with_resolver("docs", corrupt.as_bytes(), resolve),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "decode must reject {corrupt:?}"
            );
            assert!(
                matches!(
                    catalog_value_references_enum_type(corrupt.as_bytes(), "mood"),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "enum dependency parser must reject {corrupt:?}"
            );
        }
        // 参照先の照合を要する不変条件（自己参照の一意キー不一致・`id` 参照の型）は
        // 完全 decode（`validate_schema`）が拒否する。
        for corrupt in [
            format!("{head}fks:1\nfk:pid:docs:pid\n"),
            "v8\ncols:1\npk:\nname:text:-:1:L:-\nuniq:0\nchecks:0\nfks:1\nfk:name:p:id\n"
                .to_string(),
        ] {
            assert!(
                matches!(
                    decode_schema_with_resolver("docs", corrupt.as_bytes(), resolve),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "decode must reject {corrupt:?}"
            );
        }
    }

    /// 既定以外の `MATCH`・遅延属性を 1 件でも持つ FK は v9 で書かれ、往復で
    /// ビット同一のスキーマへ戻る（TABLE-17・TASK-205、Issue #1077）。全 FK が
    /// 既定オプションのスキーマは（`with_options` を明示的に既定値で呼んでも）
    /// v8 のバイト列のまま変わらない（正規形の一意性）。**この v9 の意味は
    /// #1077 でリリース済みであり、UNIQUE の実名（Issue #1067）を理由に
    /// 再定義しない（Issue #1147・codex-review／Cursor Bugbot 指摘）**。
    #[test]
    fn encode_decode_roundtrips_v9_with_foreign_key_options() {
        let plain = TableSchema::new(
            "children",
            vec![
                ColumnDef::new("parent_id", ColumnType::BigInt, true),
                ColumnDef::new("code", ColumnType::Text, true),
            ],
        );

        // 全 FK が既定オプション（`Simple`／`NotDeferrable`）なら v8 のまま。
        let all_default =
            plain
                .clone()
                .with_foreign_keys(vec![fk(&["parent_id"], "parents", &["id"]).with_options(
                    ForeignKeyMatch::Simple,
                    ForeignKeyDeferrability::NotDeferrable,
                )]);
        let encoded = encode_schema(&all_default).expect("encode");
        assert!(encoded.starts_with(b"v8\n"));
        assert_eq!(
            decode_schema("children", &encoded).expect("decode"),
            assign_foreign_key_constraint_names(all_default)
        );

        // 1 件でも既定以外のオプションを持てば v9。
        let with_options = plain.with_foreign_keys(vec![
            fk(&["parent_id"], "parents", &["id"]).with_options(
                ForeignKeyMatch::Full,
                ForeignKeyDeferrability::DeferrableInitiallyDeferred,
            ),
            fk(&["code"], "countries", &["code"]).with_options(
                ForeignKeyMatch::Simple,
                ForeignKeyDeferrability::DeferrableInitiallyImmediate,
            ),
        ]);
        let encoded = encode_schema(&with_options).expect("encode");
        let text = std::str::from_utf8(&encoded).expect("utf8");
        assert_eq!(
            text,
            "v9\ncols:2\npk:\nparent_id:bigint:-:1:L:-\ncode:text:-:1:L:-\nuniq:0\nchecks:0\n\
             fks:2\nfk:parent_id:parents:id:noaction:noaction:full:deferred\n\
             fk:code:countries:code:noaction:noaction:simple:deferrable\n"
        );
        assert_eq!(
            decode_schema("children", &encoded).expect("decode"),
            assign_foreign_key_constraint_names(with_options)
        );
    }

    /// 後方互換: Issue #1076 が参照アクションを追加する前の v9（Issue #1077
    /// 時点。`MATCH`／遅延属性のみの 5 フィールド `fk:` 行）で永続化済みの
    /// カタログ値を、参照アクション追加後の本バイナリでも読み取れる
    /// （codex-review 指摘対応: バージョン文字列 `v9` を変えずに 7 フィールドへ
    /// 拡張したため、旧形式のまま既存 DB に残るカタログ値を新形式専用の
    /// パーサーで読むと構造検証エラーで拒否してしまっていた）。参照アクションは
    /// 記録されていないため `NoAction` へ既定値補完する。
    #[test]
    fn decode_v9_accepts_legacy_five_field_foreign_key_line() {
        let legacy = "v9\ncols:2\npk:\nparent_id:bigint:-:1:L:-\ncode:text:-:1:L:-\n\
                      uniq:0\nchecks:0\nfks:1\nfk:parent_id:parents:id:full:deferred\n";
        let decoded = decode_schema("children", legacy.as_bytes()).expect("decode legacy v9");
        let fk = decoded.foreign_keys().first().expect("one fk");
        assert_eq!(fk.on_delete(), ReferentialAction::NoAction);
        assert_eq!(fk.on_update(), ReferentialAction::NoAction);
        assert_eq!(fk.match_type(), ForeignKeyMatch::Full);
        assert_eq!(
            fk.deferrability(),
            ForeignKeyDeferrability::DeferrableInitiallyDeferred
        );
        // 再エンコードは常に新形式（7 フィールド）で書く（正規形の一意性を
        // 損なわない。読み取り専用の互換パス）。
        let reencoded = encode_schema(&decoded).expect("re-encode");
        let text = std::str::from_utf8(&reencoded).expect("utf8");
        assert!(text.contains("fk:parent_id:parents:id:noaction:noaction:full:deferred\n"));
    }

    /// v9 の破損入力を fail-closed に拒否する（v8 と同じ共有パーサーを通す。
    /// TABLE-17・TASK-205、Issue #1076／#1077）。全 FK が既定オプションの v9
    /// （正規形の一意性違反）・未知の参照アクション／`MATCH`／遅延属性トークン・
    /// フィールド数不正はいずれも拒否し、`DROP TYPE` の依存判定
    /// （`catalog_value_references_enum_type`）も同じ値を「依存なし」に丸めない。
    #[test]
    fn decode_v9_rejects_corrupt_or_all_default_foreign_key_section() {
        let head = "v9\ncols:2\npk:\nmood_col:enum:mood:1:L:-\npid:bigint:-:1:L:-\n\
                    uniq:0\nchecks:0\n";
        let valid = format!("{head}fks:1\nfk:pid:parents:id:noaction:noaction:full:immediate\n");
        assert!(catalog_value_references_enum_type(valid.as_bytes(), "mood").expect("valid v9"));
        let resolve = &mut |name: &str| -> Result<Arc<EnumTypeDef>> {
            Ok(Arc::new(EnumTypeDef {
                name: name.to_string(),
                labels: vec!["x".to_string()],
            }))
        };
        assert!(decode_schema_with_resolver("docs", valid.as_bytes(), resolve).is_ok());
        let corrupt_values = [
            // 全 FK が既定オプション（正規形は v8 のはず）。
            format!("{head}fks:1\nfk:pid:parents:id:noaction:noaction:simple:immediate\n"),
            // 未知の参照アクション／MATCH／遅延属性トークン。
            format!("{head}fks:1\nfk:pid:parents:id:bogus:noaction:full:immediate\n"),
            format!("{head}fks:1\nfk:pid:parents:id:noaction:noaction:partial:immediate\n"),
            format!("{head}fks:1\nfk:pid:parents:id:noaction:noaction:full:later\n"),
            // フィールド数不正（v9 は 7 フィールド固定）。
            format!("{head}fks:1\nfk:pid:parents:id\n"),
            format!("{head}fks:1\nfk:pid:parents:id:noaction:noaction\n"),
            format!("{head}fks:1\nfk:pid:parents:id:noaction:noaction:full\n"),
            format!("{head}fks:1\nfk:pid:parents:id:noaction:noaction:full:immediate:extra\n"),
        ];
        for corrupt in &corrupt_values {
            assert!(
                matches!(
                    decode_schema_with_resolver("docs", corrupt.as_bytes(), resolve),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "decode must reject {corrupt:?}"
            );
            assert!(
                matches!(
                    catalog_value_references_enum_type(corrupt.as_bytes(), "mood"),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "enum dependency parser must reject {corrupt:?}"
            );
        }
    }

    /// カタログ v10（制約名付き UNIQUE。`U:<name>:<cols>` 形式。TABLE-16・
    /// TASK-204、Issue #1067）の往復と正規形の一意性を検証する
    /// （`encode_decode_roundtrips_v9_with_foreign_key_options` の UNIQUE 側
    /// 対応。レビュー指摘: Issue #1067 PR 差し戻し・Issue #1147）。既定名（
    /// `derive_unique_constraint_names` が導出する名前）と一致する UNIQUE は
    /// v6 のバイト列のまま変わらず（正規形の一意性）、既定名と異なる実名を
    /// 1 つでも持てば v10 で書かれてビット同一のスキーマへ戻る（**v9 は #1077 で
    /// リリース済みの意味を持つため、UNIQUE の実名を理由に v9 は使わない**）。
    #[test]
    fn encode_decode_roundtrips_v10_with_named_unique_constraint() {
        let plain = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("a", ColumnType::Text, true),
                ColumnDef::new("b", ColumnType::Text, true),
            ],
        );

        let default_named =
            plain
                .clone()
                .with_unique_constraints(vec![UniqueConstraint::with_name(
                    "docs_a_key".to_string(),
                    vec!["a".to_string()],
                )]);
        let encoded = encode_schema(&default_named).expect("encode");
        assert!(encoded.starts_with(b"v6\n"));
        assert_eq!(
            decode_schema("docs", &encoded).expect("decode"),
            default_named
        );

        let named = plain.with_unique_constraints(vec![
            UniqueConstraint::with_name("custom_a_unique".to_string(), vec!["a".to_string()]),
            UniqueConstraint::with_name("docs_b_key".to_string(), vec!["b".to_string()]),
        ]);
        let encoded = encode_schema(&named).expect("encode");
        let text = std::str::from_utf8(&encoded).expect("utf8");
        assert_eq!(
            text,
            "v10\ncols:2\npk:\na:text:-:1:L:-\nb:text:-:1:L:-\nuniq:2\n\
             U:custom_a_unique:a\nU:docs_b_key:b\nchecks:0\nfks:0\n"
        );
        assert_eq!(decode_schema("docs", &encoded).expect("decode"), named);
    }

    /// UNIQUE 制約名と CHECK 制約名はテーブル単位の名前空間を共有する
    /// （`validate_schema` の契約。設計 D1・Issue #1067）。ENUM 依存判定の軽量
    /// パーサー（[`catalog_value_references_enum_type`]）は v10／v11 で UNIQUE の
    /// 制約名を読み取るだけで CHECK 名との衝突検査を行わないと、
    /// `decode_schema_with_resolver`／`validate_schema` が `CorruptSchema` として
    /// 拒否する壊れた v10 カタログ値でも「依存なし」に丸められてしまい、両者の
    /// fail-closed 判定が食い違う（codex-review P2 指摘・PR #1147）。
    ///
    /// 手組みの v10 カタログ値は 2 通り用意する（Cursor Bugbot 指摘・PR #1147）:
    /// (1) `decode_schema` 側は ENUM 列を含めない（`no_enum_resolver` が
    /// enum 型解決不可で先に `CorruptSchema` を返し、衝突検査に到達しないまま
    /// テストが green になっていた）。(2) いずれも `fks:0` セクションを明示的に
    /// 補う（v7 エンコードには `fks:` セクションが無く、`checks:` セクションを
    /// そのまま流用すると v10／v11 が要求する `fks:` セクション欠落で構造検証が
    /// 先に落ち、同じく衝突検査に到達しない）。衝突箇所のエラーメッセージ
    /// （制約名・「CHECK constraint」の文言）も具体的にアサートし、無関係な
    /// 理由で `CorruptSchema` になっていないことを確認する。
    #[test]
    fn catalog_value_references_enum_type_rejects_unique_name_colliding_with_check_name() {
        // CHECK セクションのバイト表現（述語の 16 進エンコード）は
        // `encode_schema` に生成させ、そのまま手組みのカタログへ流用する
        // （述語エンコードの内部形式に依存しないため）。
        let check_only = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("mood_col", ColumnType::Text, true),
                ColumnDef::new("pid", ColumnType::BigInt, true),
            ],
        )
        .with_checks(vec![check("dup", &["pid"], "pid > 0")]);
        let encoded = encode_schema(&check_only).expect("encode check-only schema");
        let text = std::str::from_utf8(&encoded).expect("utf8");
        let checks_section = text
            .split_once("\nchecks:")
            .map(|(_, rest)| format!("checks:{rest}"))
            .expect("checks section present");
        // `check_only` は FK を持たないため v7 で符号化され `fks:` セクションを
        // 持たない。v10／v11 の `decode_schema_body` は `fks:` セクションを
        // 必須で読むため、空の `fks:0` を手組みで補う（空 FK 列の
        // `encode_foreign_key_section` が書く形と同一）。
        let checks_and_fks_section = format!("{checks_section}fks:0\n");

        // UNIQUE 制約名（`dup`）が CHECK 制約名（同じく `dup`）と衝突する、
        // 手組みの壊れた v10 カタログ値。`decode_schema`（テスト専用の
        // `no_enum_resolver` 版）で検証するため ENUM 列は使わない
        // （ENUM 列を含めると列デコード段階で `no_enum_resolver` が先に
        // 拒否し、意図した衝突検査に到達しない）。
        let plain_head =
            "v10\ncols:2\npk:\nmood_col:text:-:1:L:-\npid:bigint:-:1:L:-\n".to_string();
        let plain_colliding =
            format!("{plain_head}uniq:1\nU:dup:mood_col\n{checks_and_fks_section}");
        match decode_schema("docs", plain_colliding.as_bytes()) {
            Err(CatalogError::CorruptSchema(msg)) => {
                assert!(
                    msg.contains("dup") && msg.contains("CHECK constraint"),
                    "decode_schema must reject with the UNIQUE/CHECK name collision message, got {msg:?}"
                );
            }
            other => panic!(
                "decode_schema must reject a UNIQUE/CHECK constraint name collision: \
                 {plain_colliding:?}, got {other:?}"
            ),
        }

        // `catalog_value_references_enum_type`（ENUM 依存判定の軽量パーサー）は
        // 実際の enum 型解決を行わずテキスト走査のみで判定するため、ENUM 列
        // （`mood_col:enum:mood`）を含む同型の v10 カタログ値でも同じ衝突検査に
        // 到達することを確認する。
        let enum_head =
            "v10\ncols:2\npk:\nmood_col:enum:mood:1:L:-\npid:bigint:-:1:L:-\n".to_string();
        let enum_colliding = format!("{enum_head}uniq:1\nU:dup:mood_col\n{checks_and_fks_section}");
        match catalog_value_references_enum_type(enum_colliding.as_bytes(), "mood") {
            Err(CatalogError::CorruptSchema(msg)) => {
                assert!(
                    msg.contains("dup") && msg.contains("CHECK constraint"),
                    "enum dependency parser must reject with the collision message, got {msg:?}"
                );
            }
            other => panic!(
                "enum dependency parser must reject the same collision instead of \
                 silently reporting \"no dependents\": {enum_colliding:?}, got {other:?}"
            ),
        }
    }

    /// v10 の破損した名前付き UNIQUE セクション（`U:<name>:<cols>` 形式）を
    /// fail-closed に拒否する（`decode_v9_rejects_corrupt_or_all_default_foreign_key_section`
    /// の UNIQUE 側対応。TABLE-16・TASK-204、Issue #1067）。名前が空・識別子
    /// 形状違反・重複、および名前フィールドを持たない旧 v6〜v8 形式の `U:` 行
    /// （コロン区切りが 1 つ足りない）はいずれも拒否する。
    ///
    /// 注記: v9 は「全 FK が既定オプション」という非正規形を decode 側でも
    /// 拒否するが、v10 は「全 UNIQUE が既定名」の非正規形を decode 側では
    /// 拒否しない（`encode_schema` がその形を生成しないだけで、手組みの v10
    /// 値の decode 自体は許容する。既存の設計上の非対称であり、本テストは
    /// この既定挙動を変更しない）。
    #[test]
    fn decode_v10_rejects_corrupt_named_unique_section() {
        let head = "v10\ncols:2\npk:\nmood_col:enum:mood:1:L:-\npid:bigint:-:1:L:-\n".to_string();
        let valid = format!("{head}uniq:1\nU:custom_key:mood_col\nchecks:0\nfks:0\n");
        assert!(catalog_value_references_enum_type(valid.as_bytes(), "mood").expect("valid v10"));
        let resolve = &mut |name: &str| -> Result<Arc<EnumTypeDef>> {
            Ok(Arc::new(EnumTypeDef {
                name: name.to_string(),
                labels: vec!["x".to_string()],
            }))
        };
        assert!(decode_schema_with_resolver("docs", valid.as_bytes(), resolve).is_ok());
        let corrupt_values = [
            // 名前が空。
            format!("{head}uniq:1\nU::mood_col\nchecks:0\nfks:0\n"),
            // 名前が識別子として不正（数字始まり）。
            format!("{head}uniq:1\nU:1bad:mood_col\nchecks:0\nfks:0\n"),
            // 制約名の重複。
            format!("{head}uniq:2\nU:dup:mood_col\nU:dup:pid\nchecks:0\nfks:0\n"),
            // 名前フィールドを持たない旧 v6〜v8 形式の `U:` 行（コロン不足）。
            format!("{head}uniq:1\nU:mood_col\nchecks:0\nfks:0\n"),
        ];
        for corrupt in &corrupt_values {
            assert!(
                matches!(
                    decode_schema_with_resolver("docs", corrupt.as_bytes(), resolve),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "decode must reject {corrupt:?}"
            );
            assert!(
                matches!(
                    catalog_value_references_enum_type(corrupt.as_bytes(), "mood"),
                    Err(CatalogError::CorruptSchema(_))
                ),
                "enum dependency parser must reject {corrupt:?}"
            );
        }
    }

    /// 互換性回帰テスト（Issue #1147・codex-review／Cursor Bugbot 指摘）: #1077
    /// でリリース済みの `v9`（FK オプション形式。無名の `U:` 行・`uniq:0` 許容・
    /// 5 フィールドの `fk:` 行）バイト列は、UNIQUE 制約名の永続化（Issue #1067）
    /// 実装後も引き続き decode できる。本テストは main に既に存在する `v9` の
    /// バイト列（旧バイナリが書き込み済みの想定）を手組みで再現し、新バイナリが
    /// これを「未知のフォーマットバージョン」等で拒否しないことを固定する。
    #[test]
    fn decode_pre_existing_v9_foreign_key_options_bytes_stays_readable() {
        // `uniq:0`・無名の `U:` 行が無い最小形（UNIQUE 制約を持たない子テーブル）。
        let minimal = "v9\ncols:2\npk:\nparent_id:bigint:-:1:L:-\ncode:text:-:1:L:-\n\
                        uniq:0\nchecks:0\nfks:1\nfk:parent_id:parents:id:full:deferred\n";
        let decoded =
            decode_schema("children", minimal.as_bytes()).expect("decode v9 (FK options)");
        assert_eq!(decoded.foreign_keys.len(), 1);
        assert_eq!(decoded.foreign_keys[0].match_type(), ForeignKeyMatch::Full);
        assert_eq!(
            decoded.foreign_keys[0].deferrability(),
            ForeignKeyDeferrability::DeferrableInitiallyDeferred
        );
        assert!(decoded.unique_constraints.is_empty());

        // 無名の `U:` 行を持つ v9（UNIQUE 制約もある子テーブル。MATCH FULL・
        // 非既定の遅延属性を持つ FOREIGN KEY と共存する main 上の実際のバイト列
        // 形状）。
        let with_unique = "v9\ncols:2\npk:\nparent_id:bigint:-:1:L:-\ncode:text:-:1:L:-\n\
                            uniq:1\nU:code\nchecks:0\nfks:1\n\
                            fk:parent_id:parents:id:full:immediate\n";
        let decoded =
            decode_schema("children", with_unique.as_bytes()).expect("decode v9 with UNIQUE");
        assert_eq!(decoded.unique_constraints.len(), 1);
        // 名前フィールドを持たない旧 v9 値は decode 時に既定名を導出する
        // （v6〜v9 共通の設計 D2）。
        assert_eq!(decoded.unique_constraints[0].name(), "children_code_key");
    }

    /// UNIQUE の実名（Issue #1067）と `FOREIGN KEY` の既定以外の `MATCH`・
    /// 遅延属性（Issue #1077）を同時に持つスキーマは v11 で書かれ、往復で
    /// ビット同一のスキーマへ戻る（Issue #1147: v9／v10 のどちらの正規形にも
    /// 収まらない組み合わせを別バージョンとして扱う）。
    #[test]
    fn encode_decode_roundtrips_v11_with_named_unique_and_foreign_key_options() {
        let plain = TableSchema::new(
            "children",
            vec![
                ColumnDef::new("parent_id", ColumnType::BigInt, true),
                ColumnDef::new("code", ColumnType::Text, true),
            ],
        );
        let schema = plain
            .with_unique_constraints(vec![UniqueConstraint::with_name(
                "custom_code_unique".to_string(),
                vec!["code".to_string()],
            )])
            .with_foreign_keys(vec![fk(&["parent_id"], "parents", &["id"]).with_options(
                ForeignKeyMatch::Full,
                ForeignKeyDeferrability::DeferrableInitiallyDeferred,
            )]);
        let encoded = encode_schema(&schema).expect("encode");
        let text = std::str::from_utf8(&encoded).expect("utf8");
        assert_eq!(
            text,
            "v11\ncols:2\npk:\nparent_id:bigint:-:1:L:-\ncode:text:-:1:L:-\nuniq:1\n\
             U:custom_code_unique:code\nchecks:0\nfks:1\n\
             fk:parent_id:parents:id:noaction:noaction:full:deferred\n"
        );
        assert_eq!(
            decode_schema("children", &encoded).expect("decode"),
            assign_foreign_key_constraint_names(schema)
        );
    }

    /// `CHECK` が参照する列の `DROP COLUMN`・型変更は `DependentObjectsStillExist`
    /// で拒否し、参照しない列の削除は従来どおり成功する（制約を黙って弱めない）。
    #[test]
    fn alter_table_rejects_changes_to_check_referenced_columns() {
        let path = unique_db_path("check-dependent-column");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("kind", ColumnType::Text, true),
                ColumnDef::new("body", ColumnType::Text, true),
                ColumnDef::new(
                    "amount",
                    ColumnType::Numeric {
                        precision: 5,
                        scale: 2,
                    },
                    true,
                ),
            ],
        )
        .with_checks(vec![
            check("kind_ck", &["kind"], "kind = 'a'"),
            check("amount_ck", &["amount"], "amount > '1.00'"),
        ]);
        storage.create_table(&schema).expect("create table");
        let err = storage
            .alter_table_drop_column("docs", "kind")
            .expect_err("dropping a CHECK-referenced column must be rejected");
        assert!(matches!(err, CatalogError::DependentObjectsStillExist(name) if name == "kind"));
        let err = storage
            .alter_table_widen_numeric_precision("docs", "amount", 9)
            .expect_err("changing a CHECK-referenced column type must be rejected");
        assert!(matches!(err, CatalogError::DependentObjectsStillExist(name) if name == "amount"));
        storage
            .alter_table_drop_column("docs", "body")
            .expect("dropping an unreferenced column must succeed");
        let reloaded = storage.get_table_schema("docs").expect("schema");
        assert_eq!(reloaded.checks().len(), 2);
    }

    /// 回帰（PR #1055 codex P1）: 依存列の記録が欠けた `CHECK` でも、述語テキスト
    /// からの再計算で依存を検出し `DROP COLUMN` を拒否する（式述語内の `VECTOR`
    /// 列参照も依存として扱う）。
    #[test]
    fn check_dependency_is_recomputed_from_predicate_text() {
        let path = unique_db_path("check-recompute-dependency");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("kind", ColumnType::Text, true),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        )
        .with_checks(vec![
            check("no_record", &[], "kind = 'a'"),
            check("vec_expr", &[], "vec_norm(embedding) < 100"),
        ]);
        assert!(schema_check_references_column(&schema, "embedding"));
        assert!(schema_check_references_column(&schema, "kind"));
        assert!(!schema_check_references_column(&schema, "body"));
        storage.create_table(&schema).expect("create table");
        let err = storage
            .alter_table_drop_column("docs", "kind")
            .expect_err("recomputed dependency must block DROP COLUMN");
        assert!(matches!(err, CatalogError::DependentObjectsStillExist(name) if name == "kind"));
        storage
            .alter_table_drop_column("docs", "body")
            .expect("unreferenced column can still be dropped");

        // 再計算できない（破損した）述語は「依存あり」とみなす（fail-closed）。
        let broken = TableSchema::new(
            "docs2",
            vec![
                ColumnDef::new("kind", ColumnType::Text, true),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        )
        .with_checks(vec![check("broken", &["kind"], "kind = = 'a'")]);
        assert!(schema_check_references_column(&broken, "body"));
    }

    // --- 生書き込み API（`#[cfg(test)]` 限定）の制約付きテーブル拒否
    // （TABLE-16・TABLE-17、Issue #1078） -----------------------------------

    /// 制約付きテーブルへの生書き込みを `reject_constrained_table_for_raw_write` が
    /// 拒否した場合、行が書かれず（`RowNotFound` のまま）テーブル世代も進まないこと
    /// を確認する共通アサーション。
    fn assert_raw_write_rejected_without_side_effect(
        storage: &Storage,
        table_name: &str,
        tenant_id: &str,
        id: u64,
        prev_generation: u64,
    ) {
        assert_eq!(
            storage
                .table_generation(table_name)
                .expect("read generation"),
            prev_generation,
            "rejected raw write must not bump the table generation"
        );
        assert!(
            matches!(
                storage.get_row_from_table(table_name, tenant_id, id),
                Err(CatalogError::RowNotFound(_))
            ),
            "rejected raw write must not persist the row"
        );
    }

    /// (a) 主キー付きテーブルへの `insert_row_into_table` は拒否される。
    #[test]
    fn insert_row_into_table_rejects_primary_key_table() {
        let path = unique_db_path("raw-write-guard-insert-row-pk");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("k", ColumnType::Text, false),
            ],
        )
        .with_primary_key(vec!["k".to_string()]);
        storage.create_table(&schema).expect("create table");
        let prev_generation = storage.table_generation("docs").expect("read generation");

        let err = storage
            .insert_row_into_table(
                "docs",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.1, 0.2],
                    metadata: &[],
                },
            )
            .expect_err("primary-key table must reject raw write");
        assert!(matches!(err, CatalogError::Invalid(_)));
        assert_raw_write_rejected_without_side_effect(
            &storage,
            "docs",
            "tenant-a",
            1,
            prev_generation,
        );
    }

    /// (b) UNIQUE 制約付きテーブルへの `insert_rows_into_table` は非空・空バッチの
    /// いずれも拒否される（`UniqueConstraint` は事後の `alter_table_add_unique_constraint`
    /// で追加。既存行の走査で違反がないことを確認してから制約が付くため、制約が付いた
    /// 後は生 API 経由の新規投入・空バッチ投入がいずれも拒否されることを確認する）。
    #[test]
    fn insert_rows_into_table_rejects_unique_constraint_table_including_empty_batch() {
        let path = unique_db_path("raw-write-guard-insert-rows-unique");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("k", ColumnType::Text, false),
            ],
        );
        storage.create_table(&schema).expect("create table");
        // 制約が付く前は生 API での投入に成功する（回帰: 無制約テーブルは従来どおり）。
        // `alter_table_add_unique_constraint` は既存行を `k` 列としてスキーマ準拠デコード
        // するため、ここではスキーマ準拠の metadata を作る `insert_typed_row`（この時点
        // ではまだ無制約なので受理される）で投入する。
        storage
            .insert_typed_row(
                "docs",
                1,
                "tenant-a",
                Visibility::Public,
                &[
                    RowCodecValue::Vector(vec![0.1, 0.2]),
                    RowCodecValue::Text("k1".to_string()),
                ],
            )
            .expect("unconstrained table must still accept raw write");
        storage
            .alter_table_add_unique_constraint("docs", &["k"])
            .expect("add unique constraint");
        let prev_generation = storage.table_generation("docs").expect("read generation");

        let err = storage
            .insert_rows_into_table(
                "docs",
                &[(
                    2,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[0.3, 0.4],
                        metadata: &[],
                    },
                )],
            )
            .expect_err("unique-constrained table must reject non-empty raw batch write");
        assert!(matches!(err, CatalogError::Invalid(_)));
        assert_raw_write_rejected_without_side_effect(
            &storage,
            "docs",
            "tenant-a",
            2,
            prev_generation,
        );

        // 空バッチも同じガードで拒否される（`rows.is_empty()` の早期 return より前に
        // ガードを呼ぶ契約。制約付きテーブルへの空バッチだけが検査を迂回しない）。
        let err = storage
            .insert_rows_into_table("docs", &[])
            .expect_err("unique-constrained table must reject empty raw batch write too");
        assert!(matches!(err, CatalogError::Invalid(_)));
        assert_eq!(
            storage.table_generation("docs").expect("read generation"),
            prev_generation,
            "rejected empty raw batch write must not bump the table generation"
        );
    }

    /// (c) CHECK 制約付きテーブルへの `insert_typed_row` は拒否される。
    #[test]
    fn insert_typed_row_rejects_check_constraint_table() {
        let path = unique_db_path("raw-write-guard-insert-typed-check");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let schema = TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("kind", ColumnType::Text, true),
            ],
        )
        .with_checks(vec![check("docs_kind_check", &["kind"], "kind = 'a'")]);
        storage.create_table(&schema).expect("create table");
        let prev_generation = storage.table_generation("docs").expect("read generation");

        let err = storage
            .insert_typed_row(
                "docs",
                1,
                "tenant-a",
                Visibility::Public,
                &[
                    RowCodecValue::Vector(vec![0.1, 0.2]),
                    RowCodecValue::Text("a".to_string()),
                ],
            )
            .expect_err("check-constrained table must reject raw write");
        assert!(matches!(err, CatalogError::Invalid(_)));
        assert_raw_write_rejected_without_side_effect(
            &storage,
            "docs",
            "tenant-a",
            1,
            prev_generation,
        );
    }

    /// (d) FOREIGN KEY を宣言した子テーブルへの生書き込みは拒否される
    /// （参照元列側の検査。参照先の根拠は `reject_constrained_table_for_raw_write` の
    /// ドキュメンテーションコメント参照）。
    #[test]
    fn insert_row_into_table_rejects_foreign_key_table() {
        let path = unique_db_path("raw-write-guard-insert-row-fk");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                "parents",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            ))
            .expect("create parents");
        let child_schema = TableSchema::new(
            "children",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("parent_id", ColumnType::BigInt, true),
            ],
        )
        .with_foreign_keys(vec![fk(&["parent_id"], "parents", &["id"])]);
        storage
            .create_table(&child_schema)
            .expect("create children");
        let prev_generation = storage
            .table_generation("children")
            .expect("read generation");

        let err = storage
            .insert_row_into_table(
                "children",
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[0.1, 0.2],
                    metadata: &[],
                },
            )
            .expect_err("foreign-key table must reject raw write");
        assert!(matches!(err, CatalogError::Invalid(_)));
        assert_raw_write_rejected_without_side_effect(
            &storage,
            "children",
            "tenant-a",
            1,
            prev_generation,
        );
    }
}
