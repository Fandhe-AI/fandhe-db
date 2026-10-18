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
//! VIEW`・`REFRESH`・`FROM <matview>` の読み取り配線と `55000` への写像）は後続 Issue
//! （#1664〜#1667）が担い、本モジュールは Storage 水準の土台だけを提供する。
//!
//! テナント境界（P0）: 実体の読み取りは、渡された `&PolicyContext` から導いたキーに
//! **完全一致**する実体だけを返す。「全文脈を読む」API は作らない。キーには許可可視性の
//! 集合も含め、`Public` のみの文脈が `Private` 込みで評価された実体を読む経路を構造的に塞ぐ。
//! 実体のない文脈は `Ok(None)`（未実体化）で、他テナントの有無・件数は観測できない。
//! 破損（件数不一致・ヘッダのテナント不一致など）は行を 1 件も返さず `CorruptSchema`。

use redb::{ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

use super::{
    bump_table_generation_in_txn, decode_schema_with_resolver, encode_schema, validate_identifier,
    CatalogError, ColumnDef, ColumnType, Result, Storage, TableSchema, MAX_COLUMN_COUNT,
    MAX_IDENTIFIER_LEN, MAX_LIST_TABLES, MAX_VIEW_BODY_BYTES,
};
use crate::policy::PolicyContext;
use crate::result_limits::MAX_RESULT_ROWS_HARD_CAP;
use crate::row_codec::{self, Value};
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
        if super::table_exists_in_txn(&write_txn, &def.name)?
            || super::view_exists_in_txn(&write_txn, &def.name)?
            || super::index_name_exists_in_txn(&write_txn, &def.name)?
            || super::materialized_view_exists_in_txn(&write_txn, &def.name)?
        {
            return Err(CatalogError::TableAlreadyExists(def.name.clone()));
        }
        for rel in &def.base_relations {
            if !(super::table_exists_in_txn(&write_txn, rel)?
                || super::view_exists_in_txn(&write_txn, rel)?
                || super::materialized_view_exists_in_txn(&write_txn, rel)?)
            {
                return Err(CatalogError::TableNotFound(rel.clone()));
            }
        }
        {
            let mut table = write_txn.open_table(MATERIALIZED_VIEWS_TABLE)?;
            if table.len()? >= MAX_MATERIALIZED_VIEWS as u64 {
                return Err(CatalogError::ViewLimitExceeded(
                    "too many materialized views".to_string(),
                ));
            }
            table.insert(def.name.as_str(), encoded.as_slice())?;
        }
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
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let mut def = load_def_in_txn(&write_txn, name)?
            .ok_or_else(|| CatalogError::ViewNotFound(name.to_string()))?;
        let schema = TableSchema::new(name.to_string(), def.columns.clone());
        delete_all_meta_in_txn(&write_txn, name)?;
        delete_rows_table_in_txn(&write_txn, name)?;
        let table_name = matview_rows_table_name(name);
        {
            let rows_def: MatviewRowsDef = TableDefinition::new(&table_name);
            let mut rows_table = write_txn.open_table(rows_def)?;
            let mut meta_table = write_txn.open_table(MATVIEW_ENTITY_META_TABLE)?;
            for ((ctx, rows), key) in entities.iter().zip(&keys) {
                for (i, values) in rows.iter().enumerate() {
                    let encoded = row_codec::encode_row(
                        &schema,
                        ctx.tenant_id(),
                        Visibility::Private,
                        values,
                    )
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
        }
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
    /// `Ok(None)`（未実体化。SQL の `55000` への写像は #1664）。1 つの read txn で読み、
    /// 破損（件数不一致・ヘッダのテナント／可視性不一致・decode 失敗）は行を返さず
    /// `CorruptSchema`（fail-closed）。
    pub(crate) fn read_materialized_view(
        &self,
        name: &str,
        ctx: &PolicyContext,
    ) -> Result<Option<MaterializedViewEntity>> {
        validate_identifier(name)?;
        let read_txn = self.db().begin_read()?;
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

    /// matview の定義・全文脈のメタ・行表を同一 write txn で削除する（行表を残すと同名で
    /// 作り直したときに旧データが復活する。Issue #179 の教訓）。matview を参照する別の
    /// matview があれば `DependentViewsExist`。SQL 文・CASCADE は #1667。
    pub(crate) fn drop_materialized_view(&self, name: &str) -> Result<()> {
        validate_identifier(name)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        if load_def_in_txn(&write_txn, name)?.is_none() {
            return Err(CatalogError::ViewNotFound(name.to_string()));
        }
        if !materialized_views_referencing_in_txn(&write_txn, name)?.is_empty() {
            return Err(CatalogError::DependentViewsExist(name.to_string()));
        }
        {
            let mut table = write_txn.open_table(MATERIALIZED_VIEWS_TABLE)?;
            table.remove(name)?;
        }
        delete_all_meta_in_txn(&write_txn, name)?;
        delete_rows_table_in_txn(&write_txn, name)?;
        bump_table_generation_in_txn(&write_txn, name)?;
        crate::recovery::commit_boundary::commit(write_txn).map_err(super::convert_storage_error)
    }
}

#[cfg(test)]
mod tests;
