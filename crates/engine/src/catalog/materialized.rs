//! MATERIALIZED VIEW のカタログ定義と文脈単位の実体ストア（TABLE-42 (2) のポインタ・
//! Issue #1663。親: #1662）。
//!
//! 役割: マテリアライズドビュー（以下 matview）を、通常のビュー（`VIEWS_TABLE`）とは別の
//! 専用カタログ表 [`MATERIALIZED_VIEWS_TABLE`] に保持し、問い合わせ結果の実体を
//! **文脈（[`PolicyContext`]）ごとに** redb 上へ確保する。PostgreSQL は 1 つの実体を全読者へ
//! 返すが、本 DB は行単位のテナント境界を保つため、実体を文脈キー（[`ContextKey`]）で分けて持つ。
//!
//! 専用表にした理由: `VIEWS_TABLE` の読み手（`sql::view::resolve_from` など）は「その場で
//! 展開できるビュー」を前提にしており、matview を混ぜると未実体化なのにビューとして展開
//! される。専用表なら既存の読み手からは matview が存在しない名前（`42P01`）に見え、
//! 後続 Issue が配線するまで fail-closed のまま保てる。代わりに名前空間の衝突検査は
//! 呼び出し元（`catalog.rs` の各 DDL）が [`Storage`] の write txn 内で併記する。
//!
//! 呼び出し関係: 本モジュールの API はすべて `pub(crate)`。SQL 表層（`CREATE MATERIALIZED
//! VIEW`・`REFRESH`・`FROM <matview>` の読み取り配線と `55000` への写像）は `sql::ddl`・
//! `core.rs` が担い、本モジュールは Storage 水準の土台（定義・実体・索引宣言・DROP）を提供する。
//!
//! 索引（TABLE-42 (5)(6)・Issue #1667）: matview への `CREATE [UNIQUE] INDEX` は表と同じ
//! `INDEX_CATALOG_TABLE` へ `IndexDef { table: <matview 名> }` として宣言し、**物理構造は持たない**
//! （matview の読み取りは実体全体を外側処理に渡すため、索引の有無で結果は変わらない）。UNIQUE は
//! **文脈（[`ContextKey`]）ごと**に一意性を強制する（表の一意索引がテナント単位で判定するのと
//! 同じ規約。NULL は distinct）。強制箇所は `CREATE UNIQUE INDEX` 時の既存実体の走査と、実体の
//! 書き込みプリミティブ（[`write_entities_in_txn`]・[`write_context_entity_in_txn`]）の 2 つで、
//! REFRESH が共通プリミティブを通る限り索引との整合（同一 txn・失敗時は旧実体が残る）が保たれる。
//! 重複時のエラー（`UniqueConstraintViolation`）はテナント・値・件数を含まない固定文言。
//!
//! テナント境界（P0）: 実体の読み取りは、渡された `&PolicyContext` から導いたキーに
//! **完全一致**する実体だけを返す。「全文脈を読む」API は作らない。キーには許可可視性の
//! 集合も含め、`Public` のみの文脈が `Private` 込みで評価された実体を読む経路を構造的に塞ぐ。
//! 実体のない文脈は `Ok(None)`（未実体化）で、他テナントの有無・件数は観測できない。
//! 破損（件数不一致・ヘッダのテナント不一致など）は行を 1 件も返さず `CorruptSchema`。

use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

use super::{
    bump_table_generation_in_txn, decode_index_def, decode_schema_with_resolver, encode_schema,
    is_declarable_scalar_index_type, retain_index_defs_in_txn, validate_identifier,
    validate_index_columns, CascadedObject, CatalogError, ColumnDef, ColumnType, DropBehavior,
    IndexDef, IndexKind, Result, Storage, TableSchema, INDEX_CATALOG_TABLE, MAX_COLUMN_COUNT,
    MAX_IDENTIFIER_LEN, MAX_INDEX_COUNT, MAX_LIST_TABLES, MAX_UNIQUE_CONSTRAINTS,
    MAX_VIEW_BODY_BYTES,
};
use crate::policy::PolicyContext;
use crate::result_limits::MAX_RESULT_ROWS_HARD_CAP;
use crate::row_codec::{self, Value};
use crate::storage::read_source::ReadSource;
use crate::storage::Visibility;

/// matview 定義を格納する専用カタログ表。キーは matview 名（表・ビュー・索引と名前空間を
/// 共有する）、値は [`encode_materialized_view_def`] の blob。
pub(crate) const MATERIALIZED_VIEWS_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("materialized_views");

/// 実体のメタ表。キーは (matview 名, [`ContextKey`] のバイト列)、値は v1 の
/// `[版 u8][row_count u64 LE]`。エントリがあればその文脈は実体化済み、なければ未実体化。
const MATVIEW_ENTITY_META_TABLE: TableDefinition<(&str, &[u8]), &[u8]> =
    TableDefinition::new("matview_entities");

/// 実体の行表の定義。キーは ([`ContextKey`] のバイト列, 結果内の順序 0..n)。
type MatviewRowsDef<'a> = TableDefinition<'a, (&'static [u8], u64), &'static [u8]>;

/// 登録可能な matview 数の上限（本リポの実装既定値。DoS 対策）。
const MAX_MATERIALIZED_VIEWS: usize = MAX_LIST_TABLES;

/// 1 回の全置換で扱える文脈数の上限（本リポの実装既定値。DoS 対策）。
pub(crate) const MAX_MATVIEW_CONTEXTS: usize = MAX_LIST_TABLES;

/// matview が直接参照できるリレーション数の上限（JOIN に備えた複数参照用。実装既定値）。
const MAX_MATVIEW_BASE_RELATIONS: usize = 64;

/// 定義 blob の版。
const DEF_FORMAT_VERSION: u8 = 1;
/// メタ値の版。
const META_FORMAT_VERSION: u8 = 1;
/// 文脈キーの版。
const CONTEXT_KEY_VERSION: u8 = 1;
/// 埋め込むスキーマ blob のバイト数上限（カタログ値の上限と同じ 2 MiB）。
const MAX_SCHEMA_BLOB_BYTES: usize = 2 * 1024 * 1024;

/// 実体の行表名を組み立てる唯一の点。`validate_identifier` は `/` を拒否するため、
/// `user_rows/*` やユーザー表の名前と衝突しない。
fn matview_rows_table_name(name: &str) -> String {
    format!("matview_rows/{name}")
}

/// 文脈ごとの実体を識別するキー。`PolicyContext` から決定的に導出し、テナント ID だけでなく
/// 許可可視性の集合も含めて完全一致で照合する（`Public` のみの文脈と `Public + Private` の
/// 文脈は別キー）。バイト列は `[版][可視性フラグ（bit0 = Public, bit1 = Private）][tenant_id]`。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ContextKey(Vec<u8>);

impl ContextKey {
    /// `ctx` から導出する。判定パスは増やさず、`PolicyContext` の読み取り専用 accessor だけを使う。
    pub(crate) fn from_context(ctx: &PolicyContext) -> Self {
        let tenant = ctx.tenant_id().as_bytes();
        let mut bytes = Vec::with_capacity(2usize.saturating_add(tenant.len()));
        bytes.push(CONTEXT_KEY_VERSION);
        bytes.push(u8::from(ctx.allows_public()) | (u8::from(ctx.allows_private()) << 1));
        bytes.extend_from_slice(tenant);
        Self(bytes)
    }

    fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// matview の定義（カタログ値）。`FROM` 解決や SQL の意味論には踏み込まず、後続 Issue が
/// 検証・正規化した値を運ぶだけの入れ物。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MaterializedViewDef {
    /// matview 名（表・ビュー・索引と名前空間を共有）。
    pub name: String,
    /// 直接参照する表・ビュー・matview（重複なし、名前順）。
    pub base_relations: Vec<String>,
    /// 後続 Issue が正規化して渡す再パース可能な本文。
    pub body_sql: String,
    /// 列別名リスト（空なら別名なし）。
    pub column_aliases: Vec<String>,
    /// 結果の列。型は後続 Issue が確定させて渡す（ENUM 列は本 Issue では受け付けない）。
    pub columns: Vec<ColumnDef>,
    /// `WITH NO DATA` のままなら false。`REFRESH ... CONCURRENTLY` の可否判定に使われる。
    pub populated: bool,
}

/// 1 文脈ぶんの書き込み入力（文脈と、その文脈で評価した結果行）。
pub(crate) type ContextEntity = (PolicyContext, Vec<Vec<Value>>);

/// 1 文脈ぶんの実体。行は書き込んだ順（結果内の順序キーの昇順）。
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MaterializedViewEntity {
    pub columns: Vec<ColumnDef>,
    pub rows: Vec<Vec<Value>>,
}

fn corrupt(msg: &str) -> CatalogError {
    CatalogError::CorruptSchema(msg.to_string())
}

fn push_str(out: &mut Vec<u8>, s: &str) -> Result<()> {
    let len = u32::try_from(s.len())
        .map_err(|_| CatalogError::Invalid("materialized view part too long".to_string()))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(s.as_bytes());
    Ok(())
}

fn push_count(out: &mut Vec<u8>, n: usize) -> Result<()> {
    let n = u32::try_from(n)
        .map_err(|_| CatalogError::Invalid("too many materialized view items".to_string()))?;
    out.extend_from_slice(&n.to_le_bytes());
    Ok(())
}

/// 定義 blob のカーソル。範囲外・長さ超過・不正 UTF-8 はすべて `CorruptSchema`。
struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(n)
            .ok_or_else(|| corrupt("materialized view definition offset overflow"))?;
        let slice = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| corrupt("materialized view definition truncated"))?;
        self.offset = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        self.take(1)?
            .first()
            .copied()
            .ok_or_else(|| corrupt("materialized view definition truncated"))
    }

    fn u32(&mut self) -> Result<u32> {
        let raw: [u8; 4] = self
            .take(4)?
            .try_into()
            .map_err(|_| corrupt("materialized view definition truncated"))?;
        Ok(u32::from_le_bytes(raw))
    }

    /// 件数を読む。確保の前に `max` で検査する。
    fn count(&mut self, max: usize) -> Result<usize> {
        let n = self.u32()? as usize;
        if n > max {
            return Err(corrupt("materialized view definition count out of range"));
        }
        Ok(n)
    }

    fn string(&mut self, max: usize) -> Result<String> {
        let len = self.u32()? as usize;
        if len > max {
            return Err(corrupt("materialized view definition part too large"));
        }
        let raw = self.take(len)?;
        std::str::from_utf8(raw)
            .map(str::to_string)
            .map_err(|_| corrupt("materialized view definition part is not valid UTF-8"))
    }
}

/// 作成前の形状検証（名前・件数・長さ・列型）。write txn に入る前に呼ぶ。
fn validate_def_shape(def: &MaterializedViewDef) -> Result<()> {
    validate_identifier(&def.name)?;
    if def.body_sql.len() > MAX_VIEW_BODY_BYTES {
        return Err(CatalogError::ViewLimitExceeded(
            "materialized view body too large".to_string(),
        ));
    }
    if def.base_relations.is_empty() || def.base_relations.len() > MAX_MATVIEW_BASE_RELATIONS {
        return Err(CatalogError::Invalid(
            "materialized view base relation count out of range".to_string(),
        ));
    }
    for (i, rel) in def.base_relations.iter().enumerate() {
        validate_identifier(rel)?;
        if def.base_relations.iter().take(i).any(|r| r == rel) {
            return Err(CatalogError::Invalid(
                "duplicate materialized view base relation".to_string(),
            ));
        }
    }
    if def.column_aliases.len() > MAX_COLUMN_COUNT {
        return Err(CatalogError::Invalid(
            "too many materialized view column aliases".to_string(),
        ));
    }
    for alias in &def.column_aliases {
        validate_identifier(alias)?;
    }
    if def.columns.is_empty() || def.columns.len() > MAX_COLUMN_COUNT {
        return Err(CatalogError::Invalid(
            "materialized view column count out of range".to_string(),
        ));
    }
    // ENUM 列・ENUM 配列列は decode 時に型の解決が要るため、列型の確定を担う後続 Issue（#1664）まで拒否する。
    if def.columns.iter().any(|c| match &c.ty {
        ColumnType::Enum(_) => true,
        // ENUM 配列（多次元を含む。次元数は ArrayType 内で保持される）も語彙解決が要る。
        ColumnType::Array(a) => a.enum_def().is_some(),
        _ => false,
    }) {
        return Err(CatalogError::Invalid(
            "ENUM columns are not supported in materialized views".to_string(),
        ));
    }
    Ok(())
}

/// [`MaterializedViewDef`] の blob 化。名前はキーが持つため含めない。
fn encode_materialized_view_def(def: &MaterializedViewDef) -> Result<Vec<u8>> {
    validate_def_shape(def)?;
    let schema_blob = encode_schema(&TableSchema::new(def.name.clone(), def.columns.clone()))?;
    if schema_blob.len() > MAX_SCHEMA_BLOB_BYTES {
        return Err(CatalogError::Invalid(
            "materialized view columns too large".to_string(),
        ));
    }
    let mut out = vec![DEF_FORMAT_VERSION, u8::from(def.populated)];
    push_str(&mut out, &def.body_sql)?;
    push_count(&mut out, def.base_relations.len())?;
    for rel in &def.base_relations {
        push_str(&mut out, rel)?;
    }
    push_count(&mut out, def.column_aliases.len())?;
    for alias in &def.column_aliases {
        push_str(&mut out, alias)?;
    }
    push_count(&mut out, schema_blob.len())?;
    out.extend_from_slice(&schema_blob);
    Ok(out)
}

/// [`encode_materialized_view_def`] の逆変換。破損は `CorruptSchema`（格納値の破損として
/// 扱い、本文のリテラルをエラー文言へ含めない）。
fn decode_materialized_view_def(name: &str, bytes: &[u8]) -> Result<MaterializedViewDef> {
    let mut r = Reader { bytes, offset: 0 };
    if r.u8()? != DEF_FORMAT_VERSION {
        return Err(corrupt("unknown materialized view definition version"));
    }
    let populated = match r.u8()? {
        0 => false,
        1 => true,
        _ => return Err(corrupt("invalid materialized view populated flag")),
    };
    let body_sql = r.string(MAX_VIEW_BODY_BYTES)?;
    let base_count = r.count(MAX_MATVIEW_BASE_RELATIONS)?;
    let mut base_relations = Vec::new();
    base_relations
        .try_reserve_exact(base_count)
        .map_err(|_| corrupt("materialized view allocation failed"))?;
    for _ in 0..base_count {
        let rel = r.string(MAX_IDENTIFIER_LEN)?;
        validate_identifier(&rel).map_err(|_| corrupt("invalid base relation identifier"))?;
        base_relations.push(rel);
    }
    let alias_count = r.count(MAX_COLUMN_COUNT)?;
    let mut column_aliases = Vec::new();
    column_aliases
        .try_reserve_exact(alias_count)
        .map_err(|_| corrupt("materialized view allocation failed"))?;
    for _ in 0..alias_count {
        column_aliases.push(r.string(MAX_IDENTIFIER_LEN)?);
    }
    let schema_len = r.count(MAX_SCHEMA_BLOB_BYTES)?;
    let schema_bytes = r.take(schema_len)?;
    if r.offset != bytes.len() {
        return Err(corrupt("materialized view definition has trailing bytes"));
    }
    let mut no_enum = |_: &str| -> Result<std::sync::Arc<super::EnumTypeDef>> {
        Err(corrupt(
            "ENUM columns are not supported in materialized views",
        ))
    };
    let schema = decode_schema_with_resolver(name, schema_bytes, &mut no_enum)?;
    Ok(MaterializedViewDef {
        name: name.to_string(),
        base_relations,
        body_sql,
        column_aliases,
        columns: schema.columns,
        populated,
    })
}

fn encode_meta(row_count: u64) -> Vec<u8> {
    let mut out = vec![META_FORMAT_VERSION];
    out.extend_from_slice(&row_count.to_le_bytes());
    out
}

fn decode_meta(bytes: &[u8]) -> Result<u64> {
    if bytes.len() != 9 || bytes.first() != Some(&META_FORMAT_VERSION) {
        return Err(corrupt("invalid materialized view entity metadata"));
    }
    let raw: [u8; 8] = bytes
        .get(1..9)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| corrupt("invalid materialized view entity metadata"))?;
    Ok(u64::from_le_bytes(raw))
}

/// `name` を参照元とする matview の定義を write txn 内で取得する（表が未作成なら `None`）。
fn load_def_in_txn(
    write_txn: &redb::WriteTransaction,
    name: &str,
) -> Result<Option<MaterializedViewDef>> {
    let table = match write_txn.open_table(MATERIALIZED_VIEWS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let guard = table.get(name)?;
    match guard {
        Some(g) => Ok(Some(decode_materialized_view_def(name, g.value())?)),
        None => Ok(None),
    }
}

/// 定義を書き戻す。
fn store_def_in_txn(write_txn: &redb::WriteTransaction, def: &MaterializedViewDef) -> Result<()> {
    let encoded = encode_materialized_view_def(def)?;
    let mut table = write_txn.open_table(MATERIALIZED_VIEWS_TABLE)?;
    table.insert(def.name.as_str(), encoded.as_slice())?;
    Ok(())
}

/// matview `name` のメタ（全文脈ぶん）を削除する。表が未作成なら何もしない。
fn delete_all_meta_in_txn(write_txn: &redb::WriteTransaction, name: &str) -> Result<()> {
    let mut table = match write_txn.open_table(MATVIEW_ENTITY_META_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let mut keys: Vec<Vec<u8>> = Vec::new();
    for entry in table.range::<(&str, &[u8])>((name, &[][..])..)? {
        let (key, _) = entry?;
        let (n, ctx) = key.value();
        if n != name {
            break;
        }
        if keys.len() >= MAX_MATVIEW_CONTEXTS {
            return Err(corrupt("too many materialized view entities"));
        }
        keys.push(ctx.to_vec());
    }
    for ctx in &keys {
        table.remove((name, ctx.as_slice()))?;
    }
    Ok(())
}

/// matview `name` の行表を削除する（無ければ何もしない）。
fn delete_rows_table_in_txn(write_txn: &redb::WriteTransaction, name: &str) -> Result<()> {
    let table_name = matview_rows_table_name(name);
    let def: MatviewRowsDef = TableDefinition::new(&table_name);
    write_txn.delete_table(def)?;
    Ok(())
}

/// `relation` を直接の参照元とする matview 名を列挙する（暫定ガード用。上限付き・
/// decode 失敗は `Err`）。`DROP`／`RENAME` が参照元の matview を黙って壊さないために使う。
pub(crate) fn materialized_views_referencing_in_txn(
    write_txn: &redb::WriteTransaction,
    relation: &str,
) -> Result<Vec<String>> {
    let table = match write_txn.open_table(MATERIALIZED_VIEWS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    let mut seen = 0usize;
    for entry in table.iter()? {
        seen += 1;
        if seen > MAX_MATERIALIZED_VIEWS {
            return Err(corrupt("too many materialized views"));
        }
        let (key, value) = entry?;
        let def = decode_materialized_view_def(key.value(), value.value())?;
        if def.base_relations.iter().any(|r| r == relation) {
            out.push(def.name);
        }
    }
    Ok(out)
}

/// 全 matview の `(名前, 直接の参照元)` を名前順に 1 回だけ読む（DROP の依存列挙層
/// `dependency` が使う。読み取り専用で、欠落した表を作らない）。件数は
/// [`MAX_MATERIALIZED_VIEWS`] で頭打ちにし、超過・decode 失敗は `Err`（fail-closed）。
pub(crate) fn load_materialized_view_bases_in_txn(
    write_txn: &redb::WriteTransaction,
) -> Result<Vec<(String, Vec<String>)>> {
    let table = match ReadSource::open_table(write_txn, MATERIALIZED_VIEWS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.iter()? {
        if out.len() >= MAX_MATERIALIZED_VIEWS {
            return Err(corrupt("too many materialized views"));
        }
        let (key, value) = entry?;
        let def = decode_materialized_view_def(key.value(), value.value())?;
        out.push((def.name, def.base_relations));
    }
    Ok(out)
}

/// 定義 1 件の挿入（write txn 内）。判定順は名前の衝突（`TableAlreadyExists`）→ 参照先の存在
/// （`TableNotFound`）→ 件数上限（`ViewLimitExceeded`）→ 保存。世代の bump は呼び出し元が行う。
fn insert_def_in_txn(
    write_txn: &redb::WriteTransaction,
    def: &MaterializedViewDef,
    encoded: &[u8],
) -> Result<()> {
    if super::table_exists_in_txn(write_txn, &def.name)?
        || super::view_exists_in_txn(write_txn, &def.name)?
        || super::index_name_exists_in_txn(write_txn, &def.name)?
        || super::materialized_view_exists_in_txn(write_txn, &def.name)?
    {
        return Err(CatalogError::TableAlreadyExists(def.name.clone()));
    }
    for rel in &def.base_relations {
        if !(super::table_exists_in_txn(write_txn, rel)?
            || super::view_exists_in_txn(write_txn, rel)?
            || super::materialized_view_exists_in_txn(write_txn, rel)?)
        {
            return Err(CatalogError::TableNotFound(rel.clone()));
        }
    }
    let mut table = write_txn.open_table(MATERIALIZED_VIEWS_TABLE)?;
    if table.len()? >= MAX_MATERIALIZED_VIEWS as u64 {
        return Err(CatalogError::ViewLimitExceeded(
            "too many materialized views".to_string(),
        ));
    }
    table.insert(def.name.as_str(), encoded)?;
    Ok(())
}

/// matview `name` の UNIQUE 索引宣言の列リストを同一 write txn 内で返す（索引名順）。
/// 索引カタログ未作成は空（表を作らない）。走査は [`MAX_INDEX_COUNT`] で打ち切り、超過・decode
/// 失敗は `Err`（fail-closed）。`REFRESH ... CONCURRENTLY`（Issue #1666）の「WHERE を持たない
/// UNIQUE 索引の有無」判定にも使えるよう `pub(crate)` で公開する。
pub(crate) fn unique_index_columns_for_matview_in_txn(
    write_txn: &redb::WriteTransaction,
    name: &str,
) -> Result<Vec<Vec<String>>> {
    let table = match ReadSource::open_table(write_txn, INDEX_CATALOG_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for (scanned, entry) in table.iter()?.enumerate() {
        if scanned >= MAX_INDEX_COUNT {
            return Err(corrupt("index catalog exceeds its entry limit"));
        }
        let (key, value) = entry?;
        let def = decode_index_def(key.value(), value.value())?;
        if def.unique && def.table == name {
            out.push(def.columns);
        }
    }
    Ok(out)
}

/// UNIQUE 索引宣言の列名を、matview の列位置の組へ解決する。宣言が列に解決できないのは
/// カタログ破損（`CorruptSchema`）。
fn unique_column_sets_in_txn(
    write_txn: &redb::WriteTransaction,
    name: &str,
    columns: &[ColumnDef],
) -> Result<Vec<Vec<usize>>> {
    unique_index_columns_for_matview_in_txn(write_txn, name)?
        .iter()
        .map(|set| {
            set.iter()
                .map(|col| {
                    columns
                        .iter()
                        .position(|c| &c.name == col)
                        .ok_or_else(|| corrupt("unique index references an unknown column"))
                })
                .collect()
        })
        .collect()
}

/// 1 文脈ぶんの行 `rows` が、各 UNIQUE 索引（列位置の組）で重複しないことを検査する。
/// NULL を含むキーは distinct として数えない（表の一意索引と同じ正準キー）。重複は
/// `UniqueConstraintViolation`（固定文言で、テナント・値・件数を含めない）。文脈をまたいだ
/// 比較はしない（呼び出し側が文脈ごとに呼ぶ）。
fn enforce_unique_rows(sets: &[Vec<usize>], rows: &[Vec<Value>]) -> Result<()> {
    for set in sets {
        let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        for values in rows {
            match crate::constraint::unique_key_from_values(set, values) {
                Ok(Some(key)) => {
                    if !seen.insert(key) {
                        return Err(CatalogError::UniqueConstraintViolation);
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    return Err(CatalogError::Invalid(
                        "materialized view row cannot be used as a unique key".to_string(),
                    ));
                }
            }
        }
    }
    Ok(())
}

/// 既存の全文脈の実体のいずれかが、列位置 `set` の一意性に違反するか（`CREATE UNIQUE INDEX`
/// 時の走査）。文脈ごとに独立して判定する（行表は文脈キー順に並ぶためキーが変わるたびに集合を
/// リセットする）。文脈数・文脈ごとの行数は上限で頭打ちにし、超過は `CorruptSchema`。
fn existing_entities_violate_unique_in_txn(
    write_txn: &redb::WriteTransaction,
    name: &str,
    columns: &[ColumnDef],
    set: &[usize],
) -> Result<bool> {
    let schema = TableSchema::new(name.to_string(), columns.to_vec());
    let table_name = matview_rows_table_name(name);
    let rows_def: MatviewRowsDef = TableDefinition::new(&table_name);
    let rows_table = match ReadSource::open_table(write_txn, rows_def) {
        Ok(t) => t,
        // 行表が無い（WITH NO DATA か全文脈が空）なら違反しようがない。
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let mut current: Option<Vec<u8>> = None;
    let mut contexts = 0usize;
    let mut in_context = 0usize;
    let mut seen: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
    for entry in rows_table.iter()? {
        let (k, v) = entry?;
        let (ctx, _) = k.value();
        if current.as_deref() != Some(ctx) {
            contexts += 1;
            if contexts > MAX_MATVIEW_CONTEXTS {
                return Err(corrupt("too many materialized view entities"));
            }
            current = Some(ctx.to_vec());
            in_context = 0;
            seen.clear();
        }
        in_context += 1;
        if in_context > MAX_RESULT_ROWS_HARD_CAP {
            return Err(corrupt("materialized view entity row count out of range"));
        }
        let decoded = row_codec::decode_row(&schema, v.value())
            .map_err(|_| corrupt("materialized view entity row is corrupt"))?;
        match crate::constraint::unique_key_from_values(set, &decoded.values) {
            Ok(Some(key)) => {
                if !seen.insert(key) {
                    return Ok(true);
                }
            }
            Ok(None) => {}
            Err(_) => return Err(corrupt("materialized view entity row is corrupt")),
        }
    }
    Ok(false)
}

/// matview への索引宣言 1 件を登録する（`CREATE [UNIQUE] INDEX ... ON <matview>`。TABLE-42 (5)(6)・
/// INDEX-7・INDEX-9、Issue #1667）。`Storage::create_index` が、索引名の名前空間衝突を判定し終えた
/// 後・同一 write txn 内で呼ぶ。commit と世代 bump（表単位・索引カタログ）は呼び出し元が行う。
///
/// 判定順（fail-closed。データに依存するのは最後の既存実体走査だけ）: HNSW は対象外
/// （`IndexKindMismatch`。TABLE-42 (6)）→ 列の存在（matview に暗黙の `id` 列は無い。
/// `ColumnNotFound`）→ スカラー索引化できる型（`IndexKindMismatch`）→ 照合の可否
/// （`IndexCollationNotSupported`）→ UNIQUE の列型（`IndexKindMismatch`）→ 件数上限
/// （`IndexLimitExceeded`）→ UNIQUE なら既存の全文脈の実体の重複走査（`UniqueConstraintViolation`）
/// → 保存。
pub(crate) fn create_matview_index_in_txn(
    write_txn: &redb::WriteTransaction,
    def: &IndexDef,
    encoded: &[u8],
) -> Result<()> {
    let mv = load_def_in_txn(write_txn, &def.table)?
        .ok_or_else(|| CatalogError::ViewNotFound(def.table.clone()))?;
    if matches!(def.kind, IndexKind::Hnsw) {
        return Err(CatalogError::IndexKindMismatch(
            "USING hnsw is not supported on materialized views".to_string(),
        ));
    }
    let mut positions = Vec::with_capacity(def.columns.len());
    for col in &def.columns {
        let (pos, column) = mv
            .columns
            .iter()
            .enumerate()
            .find(|(_, c)| &c.name == col)
            .ok_or_else(|| CatalogError::ColumnNotFound(col.clone()))?;
        if !is_declarable_scalar_index_type(&column.ty) {
            return Err(CatalogError::IndexKindMismatch(format!(
                "column {col} cannot be declared as a scalar index target"
            )));
        }
        positions.push(pos);
    }
    // 照合の可否（照合を持てない型への COLLATE は 42804）。列の存在・型は上で確認済み。
    validate_index_columns(def, &TableSchema::new(mv.name.clone(), mv.columns.clone()))?;
    if def.unique {
        for (col, &pos) in def.columns.iter().zip(&positions) {
            let allowed = mv
                .columns
                .get(pos)
                .is_some_and(|c| c.ty.is_unique_constraint_allowed());
            if !allowed {
                return Err(CatalogError::IndexKindMismatch(format!(
                    "column {col} cannot be used in a unique index"
                )));
            }
        }
        if unique_index_columns_for_matview_in_txn(write_txn, &def.table)?.len()
            >= MAX_UNIQUE_CONSTRAINTS
        {
            return Err(CatalogError::IndexLimitExceeded(
                "too many unique indexes on the materialized view".to_string(),
            ));
        }
    }
    {
        let table = write_txn.open_table(INDEX_CATALOG_TABLE)?;
        if table.len()? >= MAX_INDEX_COUNT as u64 {
            return Err(CatalogError::IndexLimitExceeded(
                "too many indexes".to_string(),
            ));
        }
    }
    if def.unique
        && existing_entities_violate_unique_in_txn(write_txn, &def.table, &mv.columns, &positions)?
    {
        return Err(CatalogError::UniqueConstraintViolation);
    }
    let mut table = write_txn.open_table(INDEX_CATALOG_TABLE)?;
    table.insert(def.name.as_str(), encoded)?;
    Ok(())
}

/// 全文脈の実体の入力検査（文脈数上限・文脈キー重複・文脈ごとの行数上限）を行い、
/// 文脈キーを返す。確保・書き込みより前に呼ぶ。行数は文脈ごとに数え、合計では数えない。
fn check_entities(
    entities: &[(PolicyContext, Vec<Vec<Value>>)],
    max_rows_per_context: usize,
) -> Result<Vec<ContextKey>> {
    if entities.len() > MAX_MATVIEW_CONTEXTS {
        return Err(CatalogError::ViewLimitExceeded(
            "too many materialized view contexts".to_string(),
        ));
    }
    let row_limit = max_rows_per_context.min(MAX_RESULT_ROWS_HARD_CAP);
    let keys: Vec<ContextKey> = entities
        .iter()
        .map(|(ctx, _)| ContextKey::from_context(ctx))
        .collect();
    for (i, key) in keys.iter().enumerate() {
        if keys.iter().take(i).any(|k| k == key) {
            return Err(CatalogError::Invalid(
                "duplicate materialized view context".to_string(),
            ));
        }
    }
    if entities.iter().any(|(_, rows)| rows.len() > row_limit) {
        return Err(CatalogError::ViewLimitExceeded(
            "materialized view result exceeds row limit".to_string(),
        ));
    }
    Ok(keys)
}

/// matview `name` の既存実体（メタ・行表）を消して `entities` を書く（write txn 内）。
/// `keys` は [`check_entities`] が `entities` と同じ並びで返したもの。定義の `populated` と
/// 世代の更新は呼び出し元が行う。
fn write_entities_in_txn(
    write_txn: &redb::WriteTransaction,
    name: &str,
    columns: &[ColumnDef],
    entities: &[(PolicyContext, Vec<Vec<Value>>)],
    keys: &[ContextKey],
) -> Result<()> {
    let schema = TableSchema::new(name.to_string(), columns.to_vec());
    // UNIQUE 索引宣言があれば、旧実体を消す前に文脈ごとの一意性を検査する（違反は呼び出し元が
    // txn ごと破棄するため旧実体が残る。1 行以下の文脈は重複しようがないので索引を読まない）。
    if entities.iter().any(|(_, rows)| rows.len() >= 2) {
        let sets = unique_column_sets_in_txn(write_txn, name, columns)?;
        for (_, rows) in entities {
            enforce_unique_rows(&sets, rows)?;
        }
    }
    delete_all_meta_in_txn(write_txn, name)?;
    delete_rows_table_in_txn(write_txn, name)?;
    let table_name = matview_rows_table_name(name);
    let rows_def: MatviewRowsDef = TableDefinition::new(&table_name);
    let mut rows_table = write_txn.open_table(rows_def)?;
    let mut meta_table = write_txn.open_table(MATVIEW_ENTITY_META_TABLE)?;
    for ((ctx, rows), key) in entities.iter().zip(keys) {
        for (i, values) in rows.iter().enumerate() {
            let encoded =
                row_codec::encode_row(&schema, ctx.tenant_id(), Visibility::Private, values)
                    .map_err(|_| {
                        CatalogError::Invalid(
                            "materialized view row does not match its columns".to_string(),
                        )
                    })?;
            rows_table.insert((key.as_bytes(), i as u64), encoded.as_slice())?;
        }
        meta_table.insert(
            (name, key.as_bytes()),
            encode_meta(rows.len() as u64).as_slice(),
        )?;
    }
    Ok(())
}

/// 定義の登録と（`entities` が `Some` なら）全文脈の実体の書き込みを、呼び出し元の write txn
/// 内で行う（`CREATE MATERIALIZED VIEW`。TABLE-42 (2)(3)・Issue #1664）。commit は呼び出し元が
/// 1 回だけ行う。`def.populated` は `entities.is_some()` で上書きする。入力の検査
/// （文脈キー重複は `Invalid`、文脈数・行数の超過は `ViewLimitExceeded`）と符号化のいずれかが失敗
/// したら `Err` を返す。呼び出し元は txn を破棄して副作用をゼロにする。世代も本関数内で bump する。
pub(crate) fn create_materialized_view_in_txn(
    write_txn: &redb::WriteTransaction,
    def: &MaterializedViewDef,
    entities: Option<&[ContextEntity]>,
    max_rows_per_context: usize,
) -> Result<()> {
    let mut def = def.clone();
    def.populated = entities.is_some();
    let keys = match entities {
        Some(e) => check_entities(e, max_rows_per_context)?,
        None => Vec::new(),
    };
    let encoded = encode_materialized_view_def(&def)?;
    insert_def_in_txn(write_txn, &def, &encoded)?;
    if let Some(e) = entities {
        write_entities_in_txn(write_txn, &def.name, &def.columns, e, &keys)?;
    }
    bump_table_generation_in_txn(write_txn, &def.name)?;
    Ok(())
}

/// 文脈ごとに評価・保存する逐次書き込み用に、定義の登録と既存実体の消去だけを行う
/// （`CREATE MATERIALIZED VIEW ... WITH DATA` の第 1 段。Issue #1664）。
/// 続けて文脈ごとに [`write_context_entity_in_txn`] を呼び、最後に
/// [`finish_streamed_create_in_txn`] で世代を bump する。commit は呼び出し元が 1 回だけ行い、
/// 途中で失敗したら txn を破棄して副作用をゼロにする。
pub(crate) fn begin_streamed_create_in_txn(
    write_txn: &redb::WriteTransaction,
    def: &MaterializedViewDef,
) -> Result<()> {
    let encoded = encode_materialized_view_def(def)?;
    insert_def_in_txn(write_txn, def, &encoded)?;
    if def.populated {
        delete_all_meta_in_txn(write_txn, &def.name)?;
        delete_rows_table_in_txn(write_txn, &def.name)?;
    }
    Ok(())
}

/// `REFRESH MATERIALIZED VIEW` の第 1 段（TABLE-42・Issue #1665）。呼び出し元の write txn 内で
/// 定義を読み、全文脈の旧実体（メタと行表）を消去して定義を返す。続けて文脈ごとに
/// [`write_context_entity_in_txn`] を呼び、最後に [`finish_streamed_refresh_in_txn`] を呼ぶ。
/// 同じ txn 内の変更なので、commit 前に失敗して txn を破棄すれば旧実体はそのまま残る。
/// 定義が無ければ `ViewNotFound`（表・ビュー・索引の名前との区別は呼び出し元が行う）。
pub(crate) fn begin_streamed_refresh_in_txn(
    write_txn: &redb::WriteTransaction,
    name: &str,
) -> Result<MaterializedViewDef> {
    validate_identifier(name)?;
    let def = load_def_in_txn(write_txn, name)?
        .ok_or_else(|| CatalogError::ViewNotFound(name.to_string()))?;
    delete_all_meta_in_txn(write_txn, name)?;
    delete_rows_table_in_txn(write_txn, name)?;
    Ok(def)
}

/// `REFRESH` の最終段。`populated` を定義へ書き戻し、表単位の世代を同じ txn 内で bump する
/// （commit は呼び出し元が 1 回だけ行う）。
pub(crate) fn finish_streamed_refresh_in_txn(
    write_txn: &redb::WriteTransaction,
    def: &MaterializedViewDef,
    populated: bool,
) -> Result<()> {
    let mut def = def.clone();
    def.populated = populated;
    store_def_in_txn(write_txn, &def)?;
    bump_table_generation_in_txn(write_txn, &def.name)?;
    Ok(())
}

/// `write_context_entity_in_txn` の失敗が「その文脈だけの検証・符号化失敗」（書き込み前に確定し、
/// txn に何も残さない）かを返す。真なら呼び出し元は他文脈の失敗として隠してよい。偽（ストレージ
/// エラー等）は部分書き込みが txn に残りうるため、呼び出し元は txn 全体を破棄して返す（fail-closed）。
pub(crate) fn is_context_local_failure(e: &CatalogError) -> bool {
    matches!(
        e,
        CatalogError::Invalid(_)
            | CatalogError::ViewLimitExceeded(_)
            | CatalogError::UniqueConstraintViolation
    )
}

/// 1 文脈分の実体を write txn へ書く。評価結果の行値を呼び出し元が即座に解放できるよう、
/// 文脈ごとに検査（文脈数上限・キー重複・行数上限）と符号化を済ませる。`seen` は書き込み済みの
/// 文脈キーで、重複検出と文脈数上限の判定に使う。
pub(crate) fn write_context_entity_in_txn(
    write_txn: &redb::WriteTransaction,
    def: &MaterializedViewDef,
    ctx: &PolicyContext,
    rows: &[Vec<Value>],
    seen: &mut Vec<ContextKey>,
    max_rows_per_context: usize,
) -> Result<()> {
    if seen.len() >= MAX_MATVIEW_CONTEXTS {
        return Err(CatalogError::ViewLimitExceeded(
            "too many materialized view contexts".to_string(),
        ));
    }
    let key = ContextKey::from_context(ctx);
    if seen.contains(&key) {
        return Err(CatalogError::Invalid(
            "duplicate materialized view context".to_string(),
        ));
    }
    if rows.len() > max_rows_per_context.min(MAX_RESULT_ROWS_HARD_CAP) {
        return Err(CatalogError::ViewLimitExceeded(
            "materialized view result exceeds row limit".to_string(),
        ));
    }
    if rows.len() >= 2 {
        // UNIQUE 索引宣言があれば、この文脈の行だけで一意性を検査する（他文脈とは比較しない）。
        let sets = unique_column_sets_in_txn(write_txn, &def.name, &def.columns)?;
        enforce_unique_rows(&sets, rows)?;
    }
    // 符号化前提（テナント ID の非空・長さ上限）は行数に関係なく先に検査する。行ループ内だけで
    // 検査すると行 0 件の文脈で素通りし、行ありの文脈と成否が分かれる（空・非空の parity 崩れ）。
    if ctx.tenant_id().is_empty() || u8::try_from(ctx.tenant_id().len()).is_err() {
        return Err(CatalogError::Invalid(
            "materialized view row does not match its columns".to_string(),
        ));
    }
    let schema = TableSchema::new(def.name.clone(), def.columns.clone());
    // 符号化の失敗（テナント ID 長などの文脈依存の要因）で部分的な行を残さないよう、
    // 行表へ書く前に全行を符号化する。失敗時は何も書かない（呼び出し元が他文脈の失敗を隠せる）。
    let mut encoded_rows: Vec<Vec<u8>> = Vec::new();
    encoded_rows
        .try_reserve_exact(rows.len())
        .map_err(|_| CatalogError::Invalid("materialized view allocation failed".to_string()))?;
    for values in rows {
        let encoded = row_codec::encode_row(&schema, ctx.tenant_id(), Visibility::Private, values)
            .map_err(|_| {
                CatalogError::Invalid(
                    "materialized view row does not match its columns".to_string(),
                )
            })?;
        encoded_rows.push(encoded);
    }
    let table_name = matview_rows_table_name(&def.name);
    let rows_def: MatviewRowsDef = TableDefinition::new(&table_name);
    let mut rows_table = write_txn.open_table(rows_def)?;
    let mut meta_table = write_txn.open_table(MATVIEW_ENTITY_META_TABLE)?;
    for (i, encoded) in encoded_rows.iter().enumerate() {
        rows_table.insert((key.as_bytes(), i as u64), encoded.as_slice())?;
    }
    meta_table.insert(
        (def.name.as_str(), key.as_bytes()),
        encode_meta(rows.len() as u64).as_slice(),
    )?;
    seen.push(key);
    Ok(())
}

/// 逐次書き込みの最終段。表単位の世代を同じ txn 内で bump する。
pub(crate) fn finish_streamed_create_in_txn(
    write_txn: &redb::WriteTransaction,
    name: &str,
) -> Result<()> {
    bump_table_generation_in_txn(write_txn, name)?;
    Ok(())
}

/// `ctx` に対応する実体を、呼び出し元の読み取り源 `read_txn` から読む。定義が無ければ
/// `Err(ViewNotFound)`、その文脈の実体が無ければ `Ok(None)`（未実体化）。確定済みスナップ
/// ショットでも明示トランザクション／実体化中の write txn でも同じコードで読めるよう
/// [`ReadSource`] を取る。破損（件数不一致・ヘッダのテナント／可視性不一致・decode 失敗）は
/// 行を返さず `CorruptSchema`（fail-closed）。
pub(crate) fn read_materialized_view_in<R: ReadSource>(
    read_txn: &R,
    name: &str,
    ctx: &PolicyContext,
) -> Result<Option<MaterializedViewEntity>> {
    validate_identifier(name)?;
    let def = {
        let table = match read_txn.open_table(MATERIALIZED_VIEWS_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => {
                return Err(CatalogError::ViewNotFound(name.to_string()));
            }
            Err(e) => return Err(e.into()),
        };
        let guard = table
            .get(name)?
            .ok_or_else(|| CatalogError::ViewNotFound(name.to_string()))?;
        decode_materialized_view_def(name, guard.value())?
    };
    let key = ContextKey::from_context(ctx);
    let row_count = {
        let meta = match read_txn.open_table(MATVIEW_ENTITY_META_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let guard = meta.get((name, key.as_bytes()))?;
        match guard {
            Some(g) => decode_meta(g.value())?,
            None => return Ok(None),
        }
    };
    if row_count > MAX_RESULT_ROWS_HARD_CAP as u64 {
        return Err(corrupt("materialized view entity row count out of range"));
    }
    let expected = row_count as usize;
    let schema = TableSchema::new(name.to_string(), def.columns.clone());
    let rows_table_name = matview_rows_table_name(name);
    let rows_def: MatviewRowsDef = TableDefinition::new(&rows_table_name);
    let rows_table = match read_txn.open_table(rows_def) {
        Ok(t) => Some(t),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(e.into()),
    };
    let mut rows: Vec<Vec<Value>> = Vec::new();
    rows.try_reserve_exact(expected)
        .map_err(|_| corrupt("materialized view allocation failed"))?;
    if let Some(rows_table) = rows_table {
        let lo = (key.as_bytes(), 0u64);
        let hi = (key.as_bytes(), u64::MAX);
        for entry in rows_table.range::<(&[u8], u64)>(lo..=hi)? {
            let (k, v) = entry?;
            if rows.len() >= expected || k.value().1 != rows.len() as u64 {
                return Err(corrupt("materialized view entity row count mismatch"));
            }
            let decoded = row_codec::decode_row(&schema, v.value())
                .map_err(|_| corrupt("materialized view entity row is corrupt"))?;
            if !ctx.is_owner(&decoded.tenant_id) || decoded.visibility != Visibility::Private {
                return Err(corrupt("materialized view entity header mismatch"));
            }
            rows.push(decoded.values);
        }
    }
    if rows.len() != expected {
        return Err(corrupt("materialized view entity row count mismatch"));
    }
    Ok(Some(MaterializedViewEntity {
        columns: def.columns,
        rows,
    }))
}

impl Storage {
    /// matview 定義を登録する（実体なし・`populated = false`）。判定順は同一 write txn 内で
    /// 名前の衝突（`TableAlreadyExists`）→ 参照先の存在（`TableNotFound`）→ 件数上限
    /// （`ViewLimitExceeded`）→ 保存。
    pub(crate) fn create_materialized_view(&self, def: &MaterializedViewDef) -> Result<()> {
        let mut def = def.clone();
        def.populated = false;
        let encoded = encode_materialized_view_def(&def)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        insert_def_in_txn(&write_txn, &def, &encoded)?;
        bump_table_generation_in_txn(&write_txn, &def.name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(super::convert_storage_error)
    }

    /// matview 定義をスナップショット読み取りで取得する（無ければ `None`）。
    pub(crate) fn materialized_view_definition(
        &self,
        name: &str,
    ) -> Result<Option<MaterializedViewDef>> {
        validate_identifier(name)?;
        let read_txn = self.db().begin_read()?;
        let table = match read_txn.open_table(MATERIALIZED_VIEWS_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        match table.get(name)? {
            Some(g) => Ok(Some(decode_materialized_view_def(name, g.value())?)),
            None => Ok(None),
        }
    }

    /// 全文脈の実体を 1 つの write txn で置き換える（`CREATE ... WITH DATA`〔#1664〕と
    /// `REFRESH`〔#1665〕が共用する原子的プリミティブ）。入力の検査（文脈キー重複は `Invalid`、
    /// 文脈数・文脈ごとの行数超過は `ViewLimitExceeded`）と行の符号化のいずれかが失敗したら
    /// txn を破棄し、旧実体をそのまま残す。行数は文脈ごとに数え、合計では数えない。
    pub(crate) fn replace_materialized_view_entities(
        &self,
        name: &str,
        entities: &[(PolicyContext, Vec<Vec<Value>>)],
        max_rows_per_context: usize,
    ) -> Result<()> {
        validate_identifier(name)?;
        let keys = check_entities(entities, max_rows_per_context)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let mut def = load_def_in_txn(&write_txn, name)?
            .ok_or_else(|| CatalogError::ViewNotFound(name.to_string()))?;
        write_entities_in_txn(&write_txn, name, &def.columns, entities, &keys)?;
        def.populated = true;
        store_def_in_txn(&write_txn, &def)?;
        bump_table_generation_in_txn(&write_txn, name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(super::convert_storage_error)
    }

    /// 全文脈の実体を破棄して `populated = false` に戻す（`WITH NO DATA` 用）。
    pub(crate) fn clear_materialized_view_entities(&self, name: &str) -> Result<()> {
        validate_identifier(name)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let mut def = load_def_in_txn(&write_txn, name)?
            .ok_or_else(|| CatalogError::ViewNotFound(name.to_string()))?;
        delete_all_meta_in_txn(&write_txn, name)?;
        delete_rows_table_in_txn(&write_txn, name)?;
        def.populated = false;
        store_def_in_txn(&write_txn, &def)?;
        bump_table_generation_in_txn(&write_txn, name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(super::convert_storage_error)
    }

    /// `ctx` に対応する実体を読む。定義が無ければ `Err(ViewNotFound)`、その文脈の実体が無ければ
    /// `Ok(None)`（未実体化。SQL の `55000` への写像は呼び出し元 `core.rs`）。新しい read txn を
    /// 開いて [`read_materialized_view_in`] に委ねる薄いラッパー。
    #[cfg(test)]
    pub(crate) fn read_materialized_view(
        &self,
        name: &str,
        ctx: &PolicyContext,
    ) -> Result<Option<MaterializedViewEntity>> {
        let read_txn = self.db().begin_read()?;
        read_materialized_view_in(&read_txn, name, ctx)
    }

    /// matview 1 件を RESTRICT で削除する（[`Self::drop_materialized_views`] の薄いラッパー。
    /// 不在は `ViewNotFound`、matview を参照する別の matview があれば `DependentViewsExist`）。
    pub(crate) fn drop_materialized_view(&self, name: &str) -> Result<()> {
        self.drop_materialized_views(&[name.to_string()], false, DropBehavior::Restrict)
            .map(|_| ())
    }

    /// `DROP MATERIALIZED VIEW [IF EXISTS] n1[, n2 ...] [CASCADE | RESTRICT]`（TABLE-42 (5)・
    /// TABLE-37・TABLE-36 (4)、Issue #1667）。全体を 1 つの write txn・1 回の commit で適用し、
    /// 1 件でも失敗すれば何も削除しない（原子的）。`sql::ddl::execute_drop_materialized_view` が
    /// DDL 権限ゲートを通過した後に呼ぶ。
    ///
    /// 判定順: 識別子検証 → 解決（重複は除去。表・ビュー・索引の名前は `IF EXISTS` でも
    /// `WrongObjectKind`、不在は `IF EXISTS` なら skip、なければ `ViewNotFound`）→ 依存
    /// （`Restrict`: 同じ文に並べていない matview が参照元にしていれば `DependentViewsExist`。
    /// `Cascade`: 列挙層が返した依存 matview を依存する側から先に同じ txn で削除。依存を正確に
    /// 列挙できない場合は `DependentViewsExist`〔fail-closed〕）→ 削除。全件 skip なら commit せず
    /// 副作用ゼロ。報告・エラーには共有カタログの名前だけを含め、テナント・行の情報は含めない。
    pub(crate) fn drop_materialized_views(
        &self,
        names: &[String],
        if_exists: bool,
        behavior: DropBehavior,
    ) -> Result<DropMaterializedViewsReport> {
        let mut targets: Vec<&str> = Vec::new();
        for name in names {
            validate_identifier(name)?;
            if !targets.contains(&name.as_str()) {
                targets.push(name.as_str());
            }
        }
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let mut existing: Vec<&str> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        for &name in &targets {
            if super::materialized_view_exists_in_txn(&write_txn, name)? {
                existing.push(name);
            } else if super::table_exists_in_txn(&write_txn, name)?
                || super::view_exists_in_txn(&write_txn, name)?
                || super::index_name_exists_in_txn(&write_txn, name)?
            {
                return Err(CatalogError::WrongObjectKind(name.to_string()));
            } else if if_exists {
                skipped.push(name.to_string());
            } else {
                return Err(CatalogError::ViewNotFound(name.to_string()));
            }
        }
        if existing.is_empty() {
            // 副作用ゼロ。txn は commit せず破棄する。
            return Ok(DropMaterializedViewsReport {
                dropped: 0,
                skipped,
                cascaded: Vec::new(),
            });
        }
        let mut cascaded: Vec<CascadedObject> = Vec::new();
        match behavior {
            DropBehavior::Restrict => {
                for &name in &existing {
                    let referencing = materialized_views_referencing_in_txn(&write_txn, name)?;
                    if referencing.iter().any(|r| !existing.contains(&r.as_str())) {
                        return Err(CatalogError::DependentViewsExist(name.to_string()));
                    }
                }
            }
            DropBehavior::Cascade => {
                let roots: Vec<super::dependency::DropTarget> = existing
                    .iter()
                    .map(|n| super::dependency::DropTarget::MaterializedView((*n).to_string()))
                    .collect();
                let closure =
                    super::dependency::enumerate_drop_dependents_in_txn(&write_txn, &roots)?;
                if closure.conservative {
                    let first = existing.first().copied().unwrap_or_default();
                    return Err(CatalogError::DependentViewsExist(first.to_string()));
                }
                cascaded = super::apply_cascaded_objects_in_txn(&write_txn, closure.objects)?;
            }
        }
        for &name in &existing {
            drop_materialized_view_in_txn(&write_txn, name)?;
            bump_table_generation_in_txn(&write_txn, name)?;
        }
        let dropped = existing.len();
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        Ok(DropMaterializedViewsReport {
            dropped,
            skipped,
            cascaded,
        })
    }
}

/// [`Storage::drop_materialized_views`] の結果。名前以外のテナント固有の情報は持たない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DropMaterializedViewsReport {
    /// 実際に削除した matview の数（重複除去後、`CASCADE` で連鎖削除した分は含まない）。
    pub dropped: usize,
    /// `IF EXISTS` で不在のため skip した名前（記述順）。
    pub skipped: Vec<String>,
    /// `CASCADE` で連鎖削除したオブジェクト（適用順。依存する側が先）。
    pub cascaded: Vec<CascadedObject>,
}

/// matview 1 件を同一 write txn 内で削除する: 定義・全文脈のメタ・行表・索引宣言を消す。
/// 表単位の世代（`bump_table_generation_in_txn`）は呼び出し元が commit と同じ txn で進める。行表を残すと同名で作り直したときに旧データが復活し（Issue #179 の教訓）、索引
/// 宣言を残すと同名の再作成が古い宣言を引き継ぐため、必ず同じ txn で消す。依存の判定と commit は
/// 呼び出し元（[`Storage::drop_materialized_views`]・CASCADE の適用側
/// `apply_cascaded_objects_in_txn`）が行う。定義が無ければ `ViewNotFound`。
pub(crate) fn drop_materialized_view_in_txn(
    write_txn: &redb::WriteTransaction,
    name: &str,
) -> Result<()> {
    {
        let mut table = write_txn.open_table(MATERIALIZED_VIEWS_TABLE)?;
        if table.remove(name)?.is_none() {
            return Err(CatalogError::ViewNotFound(name.to_string()));
        }
    }
    delete_all_meta_in_txn(write_txn, name)?;
    delete_rows_table_in_txn(write_txn, name)?;
    retain_index_defs_in_txn(write_txn, |def| def.table != name)?;
    Ok(())
}

#[cfg(test)]
mod tests;
