//! SCHEMA（テナント内側の名前空間）のカタログと `CREATE SCHEMA`／`DROP SCHEMA` の Storage 水準の
//! 実体（TABLE-27・TABLE-36 のポインタ・Issue #1713。親: #1711。設計記録:
//! `docs/design/schema-tenant-namespace.md` D1・D2・D8・D9・D10）。
//!
//! 役割: テナントごとの SCHEMA 定義を専用カタログ表 [`SCHEMAS_TABLE`] へ永続化し、`sql::ddl` の
//! `execute_create_schema`／`execute_drop_schema` が呼ぶ [`Storage::create_schema`]／
//! [`Storage::drop_schema`] を提供する。後続の名前解決（#1714）・オブジェクトの SCHEMA 配置
//! （#1717）は [`schema_id_in_txn`] と [`RelationKey`] を土台にする。
//!
//! 既存データの不変（Issue #1713 の受け入れ条件）: `public` は**仮想 SCHEMA**で行を持たない。
//! 既存のカタログキー（裸の名前）・`user_rows/{name}`・台帳キーは書き換えず、格納形式の版も
//! 上げない。[`SCHEMAS_TABLE`]／[`SCHEMA_ID_COUNTER_TABLE`] は最初の `CREATE SCHEMA` まで作らない
//! （遅延作成。存在判定は読み取り専用オープンで行う）ため、SCHEMA を一度も使わない DB の
//! ファイル構成は変わらない。
//!
//! テナント境界（P0）: キーの先頭を `tenant_id` にし、他テナントの行は構造上引けない。存在判定は
//! `(tenant_id, name)` の 1 回 get だけで、他テナントにしか無い名前は存在しない名前と同じ
//! 経路・同じエラーになる。内部 ID はエラー文言へ出さない。
//!
//! fail-closed: 定義 blob の未知の版・長さ不一致・owner 予約領域の非零・id=0 は `CorruptSchema`
//! とし、decode で黙って補正しない。

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

use crate::storage::read_source::ReadSource;

use super::{
    validate_identifier, CatalogError, Result, Storage, CATALOG_TABLE, ENUM_TYPES_TABLE,
    INDEX_CATALOG_TABLE, MAX_LIST_TABLES, VIEWS_TABLE,
};

/// SCHEMA 定義の専用カタログ表。キーは `(tenant_id, schema_name)`（設計記録 D1）。
pub(crate) const SCHEMAS_TABLE: TableDefinition<(&str, &str), &[u8]> =
    TableDefinition::new("schemas");

/// SCHEMA 内部 ID カウンタ（固定キー 1 行の単調カウンタ。1 始まりで再利用しない）。
pub(crate) const SCHEMA_ID_COUNTER_TABLE: TableDefinition<&str, u64> =
    TableDefinition::new("schema_id_counter");

/// [`SCHEMA_ID_COUNTER_TABLE`] の唯一のキー。
const SCHEMA_ID_COUNTER_KEY: &str = "next_id";

/// 定義 blob の版。
const SCHEMA_DEF_VERSION: u8 = 1;

/// 定義 blob の固定長（版 1 + id 8 + owner_tag 1）。
const SCHEMA_DEF_LEN: usize = 1 + 8 + 1;

/// 所有者の予約領域。SQL-39（ロール）が入るまでは常に 0。
const OWNER_TAG_NONE: u8 = 0;

/// テナントあたりの SCHEMA 数の上限（[`MAX_LIST_TABLES`] と同じ既定値。DoS 対策。設計記録 D1）。
pub(crate) const MAX_SCHEMAS_PER_TENANT: usize = MAX_LIST_TABLES;

/// 利用者の SCHEMA 名として作れない組み込み名（大文字小文字を無視して比較する）。`public` は
/// 仮想 SCHEMA、他 2 つは PostgreSQL では常に存在する名前（設計記録 D1・D9）。
const BUILTIN_SCHEMA_NAMES: [&str; 3] = ["public", "pg_catalog", "information_schema"];

/// 予約名の接頭辞（`pg_`。大文字小文字を無視する）。
const RESERVED_PREFIX: &str = "pg_";

/// 予約名（`pg_` 接頭辞）か。lexer は大文字小文字を畳み込まず引用の有無も区別しないため、
/// PostgreSQL の未引用名（小文字に畳み込まれる）より狭くならないよう ASCII の大小を無視して
/// 拒否側に倒す。`sql::ddl` の `execute_create_schema` と [`Storage::create_schema`] が呼ぶ。
pub(crate) fn is_reserved_schema_name(name: &str) -> bool {
    name.len() >= RESERVED_PREFIX.len()
        && name
            .get(..RESERVED_PREFIX.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(RESERVED_PREFIX))
}

/// 組み込み名（`public`・`pg_catalog`・`information_schema`）か。ASCII の大小を無視する。
pub(crate) fn is_builtin_schema_name(name: &str) -> bool {
    BUILTIN_SCHEMA_NAMES
        .iter()
        .any(|b| name.eq_ignore_ascii_case(b))
}

/// `CREATE SCHEMA` の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreateSchemaResult {
    /// 新しく作成した。
    Created,
    /// 既に存在したため何もしなかった（`IF NOT EXISTS`）。
    AlreadyExists,
}

/// `DROP SCHEMA` の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DropSchemaResult {
    /// 削除した。
    Dropped,
    /// 存在しなかったため何もしなかった（`IF EXISTS`）。
    NotFoundSkipped,
}

fn corrupt() -> CatalogError {
    CatalogError::CorruptSchema("invalid schema definition".to_string())
}

/// 定義 blob を符号化する（固定長・版バイト先頭）。
fn encode_schema_def(id: u64) -> [u8; SCHEMA_DEF_LEN] {
    let mut out = [0u8; SCHEMA_DEF_LEN];
    out[0] = SCHEMA_DEF_VERSION;
    out[1..9].copy_from_slice(&id.to_le_bytes());
    out[9] = OWNER_TAG_NONE;
    out
}

/// 定義 blob を復号して内部 ID を返す。未知の版・長さ不一致・owner 予約領域の非零・id=0 は
/// `CorruptSchema`（固定文言。格納バイト列の断片を含めない）。
fn decode_schema_def(bytes: &[u8]) -> Result<u64> {
    if bytes.len() != SCHEMA_DEF_LEN || bytes.first() != Some(&SCHEMA_DEF_VERSION) {
        return Err(corrupt());
    }
    let id_bytes: [u8; 8] = bytes
        .get(1..9)
        .and_then(|b| b.try_into().ok())
        .ok_or_else(corrupt)?;
    let id = u64::from_le_bytes(id_bytes);
    if id == 0 || bytes.get(9) != Some(&OWNER_TAG_NONE) {
        return Err(corrupt());
    }
    Ok(id)
}

/// relation（表・ビュー・索引・matview・ENUM 型）の内部キー（設計記録 D2）。
///
/// `public` は従来の裸の名前、テナントの SCHEMA に属するものは `#<schema_id>.<name>`。`#` と `.` は
/// 識別子に現れないため両者は構造上衝突しない。正規形を 1 つに固定し、別名キーを作らせない。
///
/// 本 Issue では接頭辞の組み立て（[`RelationKey::schema_prefix`]）と単体テストだけが使う。
/// 既存の decode 側 `validate_identifier` 呼び出し点の置き換えは、`#<id>.` キーを書く経路を入れる
/// #1714／#1717 で行う（書き込み経路が無い段階で受理範囲を広げると fail-closed が弱まるため）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelationKey {
    /// `public`（裸の名前）。
    Public(String),
    /// テナントの SCHEMA 配下。
    InSchema { schema_id: u64, name: String },
}

impl RelationKey {
    /// 内部キー文字列へ符号化する。
    pub(crate) fn encode(&self) -> String {
        match self {
            RelationKey::Public(name) => name.clone(),
            RelationKey::InSchema { schema_id, name } => format!("#{schema_id}.{name}"),
        }
    }

    /// 内部キー文字列を解析する。受理するのは「裸の識別子」と「`#` + 10 進数 + `.` + 識別子」
    /// だけ。先頭ゼロ・`#0`・`u64` あふれ・空名・`.` 欠落・不正な識別子は拒否する。
    pub(crate) fn parse(key: &str) -> Result<RelationKey> {
        let Some(rest) = key.strip_prefix('#') else {
            validate_identifier(key)?;
            return Ok(RelationKey::Public(key.to_string()));
        };
        let (digits, name) = rest
            .split_once('.')
            .ok_or_else(|| CatalogError::Invalid("malformed relation key".to_string()))?;
        let canonical = !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && !(digits.len() > 1 && digits.starts_with('0'));
        let schema_id: u64 = if canonical {
            digits.parse().ok().filter(|id| *id != 0)
        } else {
            None
        }
        .ok_or_else(|| CatalogError::Invalid("malformed relation key".to_string()))?;
        validate_identifier(name)?;
        Ok(RelationKey::InSchema {
            schema_id,
            name: name.to_string(),
        })
    }

    /// SCHEMA `schema_id` 配下のキーが共通して持つ接頭辞（`#<id>.`）。
    pub(crate) fn schema_prefix(schema_id: u64) -> String {
        format!("#{schema_id}.")
    }

    /// 接頭辞走査の上端（`.` の次の文字 `/` を使った排他的上端。`#1.` が `#12.` に一致しない）。
    fn schema_prefix_end(schema_id: u64) -> String {
        format!("#{schema_id}/")
    }
}

/// relation キーの名前部（`#<id>.<name>` の `<name>`。裸の名前はそのまま）。利用者向けの公開名
/// （修飾列参照の照合・エラー文言）に使い、内部 ID を出さない（ADR D2）。
pub(crate) fn relation_name_part(key: &str) -> &str {
    match key.strip_prefix('#').and_then(|rest| rest.split_once('.')) {
        Some((_, name)) => name,
        None => key,
    }
}

/// 既存の relation キー `existing` と同じ SCHEMA に属する、名前 `new_name`（裸の識別子）の
/// relation キーを返す。`ALTER ... RENAME TO <new>` は PostgreSQL と同じく SCHEMA を変えない
/// （新しい名前は修飾できない）ため、`s.t` の改名先は `#<id>.<new>` になる。`new_name` は
/// 厳格な識別子検証を通す（内部キー形式の直接指定を許さない）。
pub(crate) fn sibling_key(existing: &str, new_name: &str) -> Result<String> {
    validate_identifier(new_name)?;
    Ok(match RelationKey::parse(existing)? {
        RelationKey::Public(_) => new_name.to_string(),
        RelationKey::InSchema { schema_id, .. } => RelationKey::schema_prefix(schema_id) + new_name,
    })
}

/// `SEQUENCE` 定義のキー第 1 要素（`"#<schema_id>"`。ドットなし。設計記録 D2）。
fn sequence_schema_key(schema_id: u64) -> String {
    format!("#{schema_id}")
}

/// `(tenant_id, name)` の SCHEMA の内部 ID を返す（無ければ `None`）。表が未作成でも表を作らない。
/// 後続の名前解決（#1714）が 1 回引きで使う。`tenant_id` 以外のキーは構造上引けない。
pub(crate) fn schema_id_in_txn<R: ReadSource>(
    src: &R,
    tenant_id: &str,
    name: &str,
) -> Result<Option<u64>> {
    let table = match src.open_table(SCHEMAS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let found = table.get((tenant_id, name))?;
    match found {
        Some(v) => Ok(Some(decode_schema_def(v.value())?)),
        None => Ok(None),
    }
}

/// relation キー `key` が SCHEMA 配下形（`#<id>.<name>`）なら、その SCHEMA が write txn の時点で
/// 生存していることを確認する（Issue #1717。拡張クエリで Parse 後に `DROP SCHEMA` されると、
/// Bind 済みの内部キーが削除済み ID を指したまま作成できてしまうため。作成と同じ txn で検査して
/// TOCTOU を塞ぐ）。`public` の裸の名前は常に生存扱い。不在は `TableNotFound`（fail-closed。
/// 他テナントの SCHEMA か否かを区別しない）。SCHEMA ID は再利用されないため、全テナントの定義を
/// 走査して ID の一致を見る（DDL のみの経路。relation キーはテナントを持たないため行を直接引けず、
/// 件数上限も設けない。真偽以外は何も返さない）。他テナントの行が壊れていても無関係な作成を
/// 阻害しないよう、復号できない行は読み飛ばす（対象 SCHEMA 自身の行が壊れていれば一致せず
/// `TableNotFound` になるため fail-closed は保たれる）。
pub(crate) fn ensure_key_schema_alive_in_txn<R: ReadSource>(src: &R, key: &str) -> Result<()> {
    let RelationKey::InSchema { schema_id, .. } = RelationKey::parse(key)? else {
        return Ok(());
    };
    let table = match src.open_table(SCHEMAS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Err(CatalogError::TableNotFound(key.to_string()))
        }
        Err(e) => return Err(e.into()),
    };
    for entry in table.iter()? {
        let (_, v) = entry?;
        if decode_schema_def(v.value()).is_ok_and(|id| id == schema_id) {
            return Ok(());
        }
    }
    Err(CatalogError::TableNotFound(key.to_string()))
}

/// `tenant_id` の SCHEMA 数を [`limit`] + 1 件で打ち切って数える。
fn count_schemas_capped<R: ReadSource>(src: &R, tenant_id: &str, limit: usize) -> Result<usize> {
    let table = match src.open_table(SCHEMAS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut count = 0usize;
    for entry in table.range((tenant_id, "")..)? {
        let (k, _) = entry?;
        if k.value().0 != tenant_id {
            break;
        }
        count += 1;
        if count > limit {
            break;
        }
    }
    Ok(count)
}

/// 内部 ID を 1 つ払い出す（1 始まり・`checked_add`・再利用しない）。
fn allocate_schema_id_in_txn(write_txn: &redb::WriteTransaction) -> Result<u64> {
    let mut counter = write_txn.open_table(SCHEMA_ID_COUNTER_TABLE)?;
    let current = counter
        .get(SCHEMA_ID_COUNTER_KEY)?
        .map(|v| v.value())
        .unwrap_or(1);
    let next = current
        .checked_add(1)
        .ok_or(CatalogError::TableGenerationCounterOverflow)?;
    counter.insert(SCHEMA_ID_COUNTER_KEY, next)?;
    Ok(current)
}

/// `&str` キーの表に `[start, end)` の範囲のキーが 1 件でもあるか（表が無ければ偽）。
fn str_key_range_nonempty<R: ReadSource>(
    src: &R,
    def: TableDefinition<&str, &[u8]>,
    start: &str,
    end: &str,
) -> Result<bool> {
    let table = match src.open_table(def) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let found = table.range(start..end)?.next().transpose()?.is_some();
    Ok(found)
}

/// SCHEMA `schema_id` に属するオブジェクトが 1 件でもあるか（`DROP SCHEMA ... RESTRICT` の
/// 空判定。設計記録 D8）。カタログ 5 表は `#<id>.` 接頭辞、SEQUENCE はキー第 1 要素
/// `#<id>`（ドットなし）の一致で見る。どの表も読み取り専用で開き、表が無ければ空と扱う。
pub(crate) fn schema_has_objects_in_txn<R: ReadSource>(src: &R, schema_id: u64) -> Result<bool> {
    let start = RelationKey::schema_prefix(schema_id);
    let end = RelationKey::schema_prefix_end(schema_id);
    for def in [
        CATALOG_TABLE,
        VIEWS_TABLE,
        INDEX_CATALOG_TABLE,
        super::materialized::MATERIALIZED_VIEWS_TABLE,
        ENUM_TYPES_TABLE,
    ] {
        if str_key_range_nonempty(src, def, &start, &end)? {
            return Ok(true);
        }
    }
    let seq_key = sequence_schema_key(schema_id);
    let seqs = match src.open_table(super::sequence::SEQUENCES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let first = seqs.range((seq_key.as_str(), "")..)?.next().transpose()?;
    Ok(first.is_some_and(|(k, _)| k.value().0 == seq_key))
}

/// SCHEMA 配下のオブジェクトの列挙結果（[`enumerate_schema_objects_in_txn`]。各リストは内部キーの
/// 昇順で決定的）。
#[derive(Debug, Default)]
pub(crate) struct SchemaObjects {
    /// 表（`#<id>.<name>`）。
    pub(crate) tables: Vec<String>,
    /// ビュー。
    pub(crate) views: Vec<String>,
    /// 索引（表と同じ SCHEMA に属する宣言。通常は表と一緒に消える）。
    pub(crate) indexes: Vec<String>,
    /// SEQUENCE（`#<id>.<name>`）。
    pub(crate) sequences: Vec<String>,
    /// 現状 SCHEMA 配下には置けない種別（matview・ENUM 型）が 1 件でもあるか。あれば
    /// `DROP SCHEMA ... CASCADE` は不整合として拒否する（fail-closed）。
    pub(crate) unsupported_kinds: bool,
}

/// `&str` キーの表の `[start, end)` のキーを上限つきで列挙する（表が無ければ空）。
fn str_keys_in_range<R: ReadSource>(
    src: &R,
    def: TableDefinition<&str, &[u8]>,
    start: &str,
    end: &str,
) -> Result<Vec<String>> {
    let table = match src.open_table(def) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.range(start..end)? {
        if out.len() >= MAX_LIST_TABLES {
            return Err(CatalogError::Invalid(format!(
                "too many objects in schema: exceeds {MAX_LIST_TABLES}"
            )));
        }
        let (k, _) = entry?;
        out.push(k.value().to_string());
    }
    Ok(out)
}

/// SCHEMA `schema_id` に属するオブジェクトを種別ごとに列挙する（`DROP SCHEMA ... CASCADE` の根集合。
/// 設計記録 D8）。走査の形は [`schema_has_objects_in_txn`] と同じ（カタログ表は `#<id>.` 接頭辞の範囲、
/// SEQUENCE はキー第 1 要素 `#<id>` のタプル範囲）。各種別は [`MAX_LIST_TABLES`] 件で打ち切る。
pub(crate) fn enumerate_schema_objects_in_txn<R: ReadSource>(
    src: &R,
    schema_id: u64,
) -> Result<SchemaObjects> {
    let start = RelationKey::schema_prefix(schema_id);
    let end = RelationKey::schema_prefix_end(schema_id);
    let mut objects = SchemaObjects {
        tables: str_keys_in_range(src, CATALOG_TABLE, &start, &end)?,
        views: str_keys_in_range(src, VIEWS_TABLE, &start, &end)?,
        indexes: str_keys_in_range(src, INDEX_CATALOG_TABLE, &start, &end)?,
        ..SchemaObjects::default()
    };
    objects.unsupported_kinds = str_key_range_nonempty(
        src,
        super::materialized::MATERIALIZED_VIEWS_TABLE,
        &start,
        &end,
    )? || str_key_range_nonempty(src, ENUM_TYPES_TABLE, &start, &end)?;
    let seq_key = sequence_schema_key(schema_id);
    match src.open_table(super::sequence::SEQUENCES_TABLE) {
        Ok(seqs) => {
            for entry in seqs.range((seq_key.as_str(), "")..)? {
                let (k, _) = entry?;
                let (schema, name) = k.value();
                if schema != seq_key {
                    break;
                }
                if objects.sequences.len() >= MAX_LIST_TABLES {
                    return Err(CatalogError::Invalid(format!(
                        "too many objects in schema: exceeds {MAX_LIST_TABLES}"
                    )));
                }
                objects
                    .sequences
                    .push(super::sequence::sequence_entry_name((schema, name)));
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(e) => return Err(e.into()),
    }
    Ok(objects)
}

impl Storage {
    /// `tenant_id` に SCHEMA `name` を作る。判定順は識別子 → 予約名（`IF NOT EXISTS` より先。
    /// `42939`）→ 組み込み名・同名の既存（`IF NOT EXISTS` なら成功の no-op、無ければ
    /// `SchemaAlreadyExists`）→ 件数上限 → ID 払い出しと保存を同一 write txn・1 回の commit で行う。
    /// `sql::ddl::execute_create_schema` から呼ばれる（権限ゲート通過後）。
    pub(crate) fn create_schema(
        &self,
        tenant_id: &str,
        name: &str,
        if_not_exists: bool,
    ) -> Result<CreateSchemaResult> {
        self.create_schema_with_limit(tenant_id, name, if_not_exists, MAX_SCHEMAS_PER_TENANT)
    }

    /// [`Self::create_schema`] の本体（件数上限を引数にしてテストで小さい値を注入する）。
    fn create_schema_with_limit(
        &self,
        tenant_id: &str,
        name: &str,
        if_not_exists: bool,
        max_schemas: usize,
    ) -> Result<CreateSchemaResult> {
        validate_tenant_id(tenant_id)?;
        validate_identifier(name)?;
        if is_reserved_schema_name(name) {
            return Err(CatalogError::ReservedSchemaName(name.to_string()));
        }
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let exists = is_builtin_schema_name(name)
            || schema_id_in_txn(&*write_txn, tenant_id, name)?.is_some();
        if exists {
            return if if_not_exists {
                Ok(CreateSchemaResult::AlreadyExists)
            } else {
                Err(CatalogError::SchemaAlreadyExists(name.to_string()))
            };
        }
        if count_schemas_capped(&*write_txn, tenant_id, max_schemas)? >= max_schemas {
            return Err(CatalogError::ViewLimitExceeded(
                "too many schemas".to_string(),
            ));
        }
        let id = allocate_schema_id_in_txn(&write_txn)?;
        {
            let mut table = write_txn.open_table(SCHEMAS_TABLE)?;
            table.insert((tenant_id, name), encode_schema_def(id).as_slice())?;
        }
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        Ok(CreateSchemaResult::Created)
    }

    /// `tenant_id` の SCHEMA `name` を削除する（`RESTRICT`）。判定順は識別子 → 組み込み名
    /// （`IF EXISTS` でも `BuiltinSchema`）→ 存在（`(tenant_id, name)` の 1 回 get。他テナントの
    /// 同名は存在しない名前と同じ `SchemaNotFound`）→ 空判定（`DependentObjectsStillExist`。
    /// 名前を含めない）→ 削除。ID カウンタは巻き戻さない。
    /// `sql::ddl::execute_drop_schema` から呼ばれる（権限ゲート通過後）。
    pub(crate) fn drop_schema(
        &self,
        tenant_id: &str,
        name: &str,
        if_exists: bool,
    ) -> Result<DropSchemaResult> {
        validate_tenant_id(tenant_id)?;
        validate_identifier(name)?;
        if is_builtin_schema_name(name) {
            return Err(CatalogError::BuiltinSchema(name.to_string()));
        }
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let Some(id) = schema_id_in_txn(&*write_txn, tenant_id, name)? else {
            return if if_exists {
                Ok(DropSchemaResult::NotFoundSkipped)
            } else {
                Err(CatalogError::SchemaNotFound(name.to_string()))
            };
        };
        if schema_has_objects_in_txn(&*write_txn, id)? {
            return Err(CatalogError::DependentObjectsStillExist(name.to_string()));
        }
        {
            let mut table = write_txn.open_table(SCHEMAS_TABLE)?;
            table.remove((tenant_id, name))?;
        }
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        Ok(DropSchemaResult::Dropped)
    }

    /// `tenant_id` の SCHEMA `name` を `CASCADE` で削除する（TABLE-27・Issue #1717、設計記録 D8）。
    /// 判定順は [`Self::drop_schema`] の存在判定まで同じ。以降は**単一の write txn・1 回の commit**で、
    /// (1) SCHEMA 配下の表・ビューを根として外部の依存（別 SCHEMA のビュー・外部キー。public からの
    /// 依存は D7 で存在しない）を既存の依存列挙で求めて先に削除 → (2) 配下のビュー（キー昇順）→
    /// (3) 配下の表（既存の `DROP TABLE ... CASCADE` 本体。索引宣言・行ストア・世代・台帳・キャッシュ
    /// 世代・所有 SEQUENCE を継承）→ (4) 残りの SEQUENCE（キー昇順）→ (5) SCHEMA 行の順に消す。
    /// 依存を正確に判定できない場合・外部の表が配下の SEQUENCE を `DEFAULT nextval` で参照している
    /// 場合は `DependentObjectsStillExist`（不可逆な連鎖削除をしない。fail-closed）。ID カウンタは
    /// 巻き戻さない。`sql::ddl::execute_drop_schema` から呼ばれる（権限ゲート通過後）。
    pub(crate) fn drop_schema_cascade(
        &self,
        tenant_id: &str,
        name: &str,
        if_exists: bool,
    ) -> Result<DropSchemaResult> {
        validate_tenant_id(tenant_id)?;
        validate_identifier(name)?;
        if is_builtin_schema_name(name) {
            return Err(CatalogError::BuiltinSchema(name.to_string()));
        }
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let Some(id) = schema_id_in_txn(&*write_txn, tenant_id, name)? else {
            return if if_exists {
                Ok(DropSchemaResult::NotFoundSkipped)
            } else {
                Err(CatalogError::SchemaNotFound(name.to_string()))
            };
        };
        let objects = enumerate_schema_objects_in_txn(&*write_txn, id)?;
        if objects.unsupported_kinds {
            return Err(CatalogError::DependentObjectsStillExist(name.to_string()));
        }
        // (1) 外部の依存（根集合に含まれない依存側）を求めて先に削除する。
        let roots: Vec<super::dependency::DropTarget> = objects
            .tables
            .iter()
            .map(|t| super::dependency::DropTarget::Table(t.clone()))
            .chain(
                objects
                    .views
                    .iter()
                    .map(|v| super::dependency::DropTarget::View(v.clone())),
            )
            .collect();
        if !roots.is_empty() {
            let closure = super::dependency::enumerate_drop_dependents_in_txn(&write_txn, &roots)?;
            if closure.conservative {
                return Err(CatalogError::DependentObjectsStillExist(name.to_string()));
            }
            super::apply_cascaded_objects_in_txn(&write_txn, closure.objects)?;
        }
        // (2) 配下のビュー。
        for view in &objects.views {
            super::drop_view_in_txn(&write_txn, view)?;
        }
        // (3) 配下の表（所有 SEQUENCE も道連れ）。
        let mut dropped_sequence_ids: Vec<u64> = Vec::new();
        if !objects.tables.is_empty() {
            let (_, ids) = self.drop_tables_collecting_sequences(
                &write_txn,
                &objects.tables,
                false,
                super::DropBehavior::Cascade,
            )?;
            dropped_sequence_ids.extend(ids);
        }
        // (4) 残りの SEQUENCE（表に所有されていないもの）。
        for seq in &objects.sequences {
            if super::sequence::sequence_exists_in_txn(&write_txn, seq)? {
                dropped_sequence_ids.push(super::sequence::drop_sequence_in_txn(&write_txn, seq)?);
            }
        }
        // 配下に残った索引宣言（表を失った孤児。通常は無い）も消す。
        let prefix = RelationKey::schema_prefix(id);
        super::retain_index_defs_in_txn(&write_txn, |def| !def.name.starts_with(&prefix))?;
        // (5) SCHEMA 行。
        {
            let mut table = write_txn.open_table(SCHEMAS_TABLE)?;
            table.remove((tenant_id, name))?;
        }
        let allocator = self.sequence_allocator();
        crate::recovery::commit_boundary::commit_and_finish(write_txn, (), |()| {
            for id in &dropped_sequence_ids {
                allocator.evict(*id);
            }
            crate::recovery::commit_boundary::PostCommitResult::Ok
        })
        .map_err(super::convert_storage_error)?;
        Ok(DropSchemaResult::Dropped)
    }

    /// `tenant_id` の SCHEMA `old` を `new` へ改名する（TABLE-27 のポインタ、Issue #1716）。
    /// オブジェクトは内部 ID 経由で格納されるため、`schemas` 表の 1 行の付け替え（内部 ID は不変）
    /// だけで中のオブジェクトが追随する。判定順は識別子 → 旧名が組み込み名（`BuiltinSchema`）→
    /// 旧名の存在（`(tenant_id, old)` の 1 回 get。他テナントの同名は不在と同じ `SchemaNotFound`）→
    /// 新名が組み込み名・既存（`SchemaAlreadyExists`。同名への改名を含む）→ 新名が予約名
    /// （`ReservedSchemaName`）→ 付け替えを 1 回の commit で行う。
    /// `sql::ddl::execute_alter_schema` から呼ばれる（権限ゲート通過後）。
    pub(crate) fn rename_schema(&self, tenant_id: &str, old: &str, new: &str) -> Result<()> {
        validate_tenant_id(tenant_id)?;
        validate_identifier(old)?;
        validate_identifier(new)?;
        if is_builtin_schema_name(old) {
            return Err(CatalogError::BuiltinSchema(old.to_string()));
        }
        let write_txn = self
            .begin_write_txn()
            .map_err(super::convert_storage_error)?;
        let Some(id) = schema_id_in_txn(&*write_txn, tenant_id, old)? else {
            return Err(CatalogError::SchemaNotFound(old.to_string()));
        };
        if is_builtin_schema_name(new) || schema_id_in_txn(&*write_txn, tenant_id, new)?.is_some() {
            return Err(CatalogError::SchemaAlreadyExists(new.to_string()));
        }
        if is_reserved_schema_name(new) {
            return Err(CatalogError::ReservedSchemaName(new.to_string()));
        }
        {
            let mut table = write_txn.open_table(SCHEMAS_TABLE)?;
            table.remove((tenant_id, old))?;
            table.insert((tenant_id, new), encode_schema_def(id).as_slice())?;
        }
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(super::convert_storage_error)?;
        Ok(())
    }

    /// `(tenant_id, name)` の SCHEMA の内部 ID（`ALTER TABLE SET SCHEMA` の移動先解決用。読み取り専用）。
    pub(crate) fn schema_id(&self, tenant_id: &str, name: &str) -> Result<Option<u64>> {
        let rt = self.db().begin_read()?;
        schema_id_in_txn(&rt, tenant_id, name)
    }

    /// `tenant_id` の SCHEMA の `内部 ID → 名前` 対応（エラー文言・EXPLAIN の表示変換用。読み取り専用。
    /// 件数は [`MAX_SCHEMAS_PER_TENANT`] で打ち切る）。逆引き表を新設しない（格納形式を増やさない）
    /// ため、冷えた経路（内部キーが利用者向けの文字列へ出る場合）だけが呼ぶ。
    pub(crate) fn schema_names_for_tenant(
        &self,
        tenant_id: &str,
    ) -> Result<std::collections::BTreeMap<u64, String>> {
        let rt = self.db().begin_read()?;
        schema_names_for_tenant_in_txn(&rt, tenant_id)
    }
}

/// [`Storage::schema_names_for_tenant`] の本体。`(tenant_id, "")..` の範囲走査で、他テナントの行には
/// 構造上触れない。
pub(crate) fn schema_names_for_tenant_in_txn<R: ReadSource>(
    src: &R,
    tenant_id: &str,
) -> Result<std::collections::BTreeMap<u64, String>> {
    let mut out = std::collections::BTreeMap::new();
    let table = match src.open_table(SCHEMAS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in table.range((tenant_id, "")..)? {
        let (k, v) = entry?;
        let (tenant, name) = k.value();
        if tenant != tenant_id || out.len() >= MAX_SCHEMAS_PER_TENANT {
            break;
        }
        out.insert(decode_schema_def(v.value())?, name.to_string());
    }
    Ok(out)
}

/// `tenant_id` の形状検査（空は拒否。形式の詳細検証は `PolicyContext` が担うため多層防御の下限）。
fn validate_tenant_id(tenant_id: &str) -> Result<()> {
    if tenant_id.is_empty() {
        return Err(CatalogError::Invalid("tenant id is empty".to_string()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use redb::ReadableDatabase;

    fn tmp_storage(label: &str) -> (Storage, crate::test_util::temp_db::CleanupGuard) {
        let path = crate::test_util::temp_db::unique_db_path(label);
        let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        (Storage::open(&path).expect("open storage"), guard)
    }

    #[test]
    fn blob_roundtrip_and_corruption_is_fail_closed() {
        let b = encode_schema_def(7);
        assert_eq!(decode_schema_def(&b).expect("decode"), 7);
        for bad in [
            vec![],
            b[..9].to_vec(),
            [b.as_slice(), &[0]].concat(),
            {
                let mut x = b;
                x[0] = 2;
                x.to_vec()
            },
            {
                let mut x = b;
                x[9] = 1;
                x.to_vec()
            },
            encode_schema_def(0).to_vec(),
        ] {
            assert!(matches!(
                decode_schema_def(&bad),
                Err(CatalogError::CorruptSchema(_))
            ));
        }
    }

    #[test]
    fn relation_key_roundtrip_and_rejections() {
        let public = RelationKey::parse("users").expect("public");
        assert_eq!(public, RelationKey::Public("users".to_string()));
        assert_eq!(public.encode(), "users");
        let k = RelationKey::parse("#12.t_1").expect("in schema");
        assert_eq!(
            k,
            RelationKey::InSchema {
                schema_id: 12,
                name: "t_1".to_string()
            }
        );
        assert_eq!(k.encode(), "#12.t_1");
        for bad in [
            "#01.t",
            "#0.t",
            "#.t",
            "#1",
            "#1.",
            "#1.1a",
            "#18446744073709551616.t",
            "#-1.t",
            "#1.a.b",
            "#+1.t",
            "",
            "1a",
            "a.b",
        ] {
            assert!(RelationKey::parse(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(RelationKey::parse("#18446744073709551615.t").is_ok());
    }

    #[test]
    fn schema_prefix_range_excludes_longer_ids() {
        let start = RelationKey::schema_prefix(1);
        let end = RelationKey::schema_prefix_end(1);
        assert!(start.as_str() <= "#1.x" && "#1.x" < end.as_str());
        assert!(!("#12.x" >= start.as_str() && "#12.x" < end.as_str()));
        assert!(!("#10.x" >= start.as_str() && "#10.x" < end.as_str()));
    }

    #[test]
    fn name_classification_is_case_insensitive() {
        for n in ["pg_x", "PG_x", "Pg_catalog", "pg_"] {
            assert!(is_reserved_schema_name(n), "{n}");
        }
        for n in ["pgx", "p", "xpg_", "é"] {
            assert!(!is_reserved_schema_name(n), "{n}");
        }
        for n in ["public", "PUBLIC", "pg_catalog", "Information_Schema"] {
            assert!(is_builtin_schema_name(n), "{n}");
        }
        assert!(!is_builtin_schema_name("publicx"));
    }

    #[test]
    fn schema_tables_are_created_lazily() {
        let (s, _g) = tmp_storage("ns-lazy");
        let names = |s: &Storage| {
            use redb::TableHandle;
            let rt = s.db().begin_read().expect("read");
            rt.list_tables()
                .expect("list")
                .map(|h| h.name().to_string())
                .collect::<Vec<_>>()
        };
        assert!(!names(&s).iter().any(|n| n.starts_with("schema")));
        // 不在の DROP IF EXISTS と重複 no-op は表を作らない。
        assert_eq!(
            s.drop_schema("alice", "x", true).expect("drop"),
            DropSchemaResult::NotFoundSkipped
        );
        assert!(!names(&s).iter().any(|n| n.starts_with("schema")));
        s.create_schema("alice", "x", false).expect("create");
        assert!(names(&s).contains(&"schemas".to_string()));
    }

    #[test]
    fn create_and_drop_are_tenant_scoped() {
        let (s, _g) = tmp_storage("ns-tenant");
        assert_eq!(
            s.create_schema("alice", "s", false).expect("a"),
            CreateSchemaResult::Created
        );
        assert_eq!(
            s.create_schema("bob", "s", false).expect("b"),
            CreateSchemaResult::Created
        );
        assert!(matches!(
            s.create_schema("alice", "s", false),
            Err(CatalogError::SchemaAlreadyExists(_))
        ));
        assert_eq!(
            s.create_schema("alice", "s", true).expect("noop"),
            CreateSchemaResult::AlreadyExists
        );
        let a = s.schema_id("alice", "s").expect("id").expect("some");
        let b = s.schema_id("bob", "s").expect("id").expect("some");
        assert_ne!(a, b);
        assert_eq!(
            s.drop_schema("bob", "s", false).expect("drop"),
            DropSchemaResult::Dropped
        );
        assert!(matches!(
            s.drop_schema("bob", "s", false),
            Err(CatalogError::SchemaNotFound(_))
        ));
        assert!(s.schema_id("alice", "s").expect("id").is_some());
        // 作り直しても ID は再利用されない。
        s.create_schema("bob", "s", false).expect("recreate");
        let b2 = s.schema_id("bob", "s").expect("id").expect("some");
        assert!(b2 > b && b2 > a);
    }

    #[test]
    fn reserved_and_builtin_names_are_rejected_before_if_not_exists() {
        let (s, _g) = tmp_storage("ns-reserved");
        for n in ["pg_x", "PG_X", "pg_catalog"] {
            for ine in [false, true] {
                assert!(matches!(
                    s.create_schema("alice", n, ine),
                    Err(CatalogError::ReservedSchemaName(_))
                ));
            }
        }
        assert!(matches!(
            s.create_schema("alice", "public", false),
            Err(CatalogError::SchemaAlreadyExists(_))
        ));
        assert_eq!(
            s.create_schema("alice", "public", true).expect("noop"),
            CreateSchemaResult::AlreadyExists
        );
        assert!(matches!(
            s.create_schema("alice", "information_schema", false),
            Err(CatalogError::SchemaAlreadyExists(_))
        ));
        for ie in [false, true] {
            for n in ["public", "pg_catalog", "information_schema"] {
                assert!(matches!(
                    s.drop_schema("alice", n, ie),
                    Err(CatalogError::BuiltinSchema(_))
                ));
            }
        }
    }

    #[test]
    fn rename_keeps_internal_id_and_objects_follow() {
        let (s, _g) = tmp_storage("ns-rename");
        s.create_schema("alice", "a", false).expect("a");
        s.create_schema("bob", "a", false).expect("bob a");
        let id = s.schema_id("alice", "a").expect("id").expect("some");
        let bob_id = s.schema_id("bob", "a").expect("id").expect("some");
        // 中身（表）を内部 ID キーで直接置き、改名後も DROP が依存で拒否されること。
        {
            let w = s.begin_write_txn().expect("w");
            {
                let mut t = w.open_table(CATALOG_TABLE).expect("t");
                t.insert(format!("#{id}.x").as_str(), [0u8].as_slice())
                    .expect("ins");
            }
            crate::recovery::commit_boundary::commit(w).expect("commit");
        }
        s.rename_schema("alice", "a", "b").expect("rename");
        assert_eq!(s.schema_id("alice", "b").expect("id"), Some(id));
        assert_eq!(s.schema_id("alice", "a").expect("id"), None);
        assert_eq!(s.schema_id("bob", "a").expect("id"), Some(bob_id));
        assert!(matches!(
            s.drop_schema("alice", "b", false),
            Err(CatalogError::DependentObjectsStillExist(_))
        ));
    }

    #[test]
    fn rename_rejections_leave_catalog_unchanged() {
        let (s, _g) = tmp_storage("ns-rename-rej");
        // SCHEMA 未使用の DB では表を作らない。
        assert!(matches!(
            s.rename_schema("alice", "a", "b"),
            Err(CatalogError::SchemaNotFound(_))
        ));
        s.create_schema("alice", "a", false).expect("a");
        s.create_schema("alice", "c", false).expect("c");
        s.create_schema("bob", "only_bob", false).expect("bob");
        let id = s.schema_id("alice", "a").expect("id");
        for old in ["public", "PUBLIC", "pg_catalog", "information_schema"] {
            assert!(matches!(
                s.rename_schema("alice", old, "z"),
                Err(CatalogError::BuiltinSchema(_))
            ));
        }
        assert!(matches!(
            s.rename_schema("alice", "only_bob", "z"),
            Err(CatalogError::SchemaNotFound(_))
        ));
        for new in ["c", "a", "public", "information_schema"] {
            assert!(matches!(
                s.rename_schema("alice", "a", new),
                Err(CatalogError::SchemaAlreadyExists(_))
            ));
        }
        for new in ["pg_x", "PG_X"] {
            assert!(matches!(
                s.rename_schema("alice", "a", new),
                Err(CatalogError::ReservedSchemaName(_))
            ));
        }
        assert_eq!(s.schema_id("alice", "a").expect("id"), id);
        assert!(s.schema_id("alice", "z").expect("id").is_none());
    }

    #[test]
    fn per_tenant_limit_is_enforced() {
        let (s, _g) = tmp_storage("ns-limit");
        s.create_schema_with_limit("alice", "a", false, 2)
            .expect("1");
        s.create_schema_with_limit("alice", "b", false, 2)
            .expect("2");
        assert!(matches!(
            s.create_schema_with_limit("alice", "c", false, 2),
            Err(CatalogError::ViewLimitExceeded(_))
        ));
        // 他テナントの件数は数えない。
        s.create_schema_with_limit("bob", "a", false, 2)
            .expect("bob");
    }

    /// 各カタログ表の `#<id>.x` キーと sequences の `("#<id>", "s")` を手で書き、DROP が拒否
    /// されること、隣接 ID の種は空と判定されることを確かめる（DDL では tenant SCHEMA の中に
    /// オブジェクトを作れないため直接書く。設計記録 D8）。
    #[test]
    fn drop_refuses_non_empty_schema_for_every_catalog() {
        for kind in 0..6 {
            let (s, _g) = tmp_storage(&format!("ns-nonempty-{kind}"));
            s.create_schema("alice", "s", false).expect("create");
            let id = s.schema_id("alice", "s").expect("id").expect("some");
            let put = |id: u64| {
                let txn = s.begin_write_txn().expect("txn");
                let key = format!("#{id}.x");
                match kind {
                    0 => {
                        txn.open_table(CATALOG_TABLE)
                            .expect("t")
                            .insert(key.as_str(), [0u8].as_slice())
                            .expect("i");
                    }
                    1 => {
                        txn.open_table(VIEWS_TABLE)
                            .expect("t")
                            .insert(key.as_str(), [0u8].as_slice())
                            .expect("i");
                    }
                    2 => {
                        txn.open_table(INDEX_CATALOG_TABLE)
                            .expect("t")
                            .insert(key.as_str(), [0u8].as_slice())
                            .expect("i");
                    }
                    3 => {
                        txn.open_table(super::super::materialized::MATERIALIZED_VIEWS_TABLE)
                            .expect("t")
                            .insert(key.as_str(), [0u8].as_slice())
                            .expect("i");
                    }
                    4 => {
                        txn.open_table(ENUM_TYPES_TABLE)
                            .expect("t")
                            .insert(key.as_str(), [0u8].as_slice())
                            .expect("i");
                    }
                    _ => {
                        txn.open_table(super::super::sequence::SEQUENCES_TABLE)
                            .expect("t")
                            .insert((format!("#{id}").as_str(), "s"), [0u8].as_slice())
                            .expect("i");
                    }
                }
                crate::recovery::commit_boundary::commit(txn).expect("commit");
            };
            // 隣接 ID（10 倍・桁違い）の種は空と判定される。
            put(id + 10);
            put(id * 10 + 1);
            assert_eq!(
                s.drop_schema("alice", "s", false).expect("empty drop"),
                DropSchemaResult::Dropped,
                "kind {kind}"
            );
            s.create_schema("alice", "s2", false).expect("create2");
            let id2 = s.schema_id("alice", "s2").expect("id").expect("some");
            put(id2);
            assert!(
                matches!(
                    s.drop_schema("alice", "s2", false),
                    Err(CatalogError::DependentObjectsStillExist(_))
                ),
                "kind {kind}"
            );
            assert!(s.schema_id("alice", "s2").expect("id").is_some());
        }
    }

    /// 削除済み SCHEMA の ID を指す内部キーでは、表・SEQUENCE のいずれも作成できない
    /// （Parse 後に `DROP SCHEMA` されても同一 txn の生存検査で拒否する。Issue #1717）。
    #[test]
    fn creating_objects_under_a_dropped_schema_is_rejected() {
        use crate::catalog::{ColumnDef, ColumnType, TableSchema};
        let (s, _g) = tmp_storage("ns-dropped");
        s.create_schema("alice", "a", false).expect("schema");
        let id = s.schema_id("alice", "a").expect("id").expect("exists");
        s.drop_schema("alice", "a", false).expect("drop");
        let key = RelationKey::InSchema {
            schema_id: id,
            name: "t".to_string(),
        }
        .encode();
        let cols = vec![ColumnDef::new("name", ColumnType::Text, true)];
        assert!(matches!(
            s.create_table(&TableSchema::new(key.clone(), cols)),
            Err(CatalogError::TableNotFound(_))
        ));
        let params = crate::catalog::sequence::SequenceParams {
            increment: 1,
            min_value: 1,
            max_value: 100,
            start: 1,
            cycle: false,
        };
        assert!(matches!(
            s.create_sequence(&key, &params),
            Err(CatalogError::TableNotFound(_))
        ));
    }

    /// 一覧 API（`list_tables`・`list_indexes`）は public の名前だけを返し、SCHEMA 配下の内部キー・内部 ID を
    /// 返さない（TABLE-27・Issue #1717。他テナントの SCHEMA の存在・名前を漏らさない）。
    #[test]
    fn list_apis_do_not_expose_schema_keys() {
        use crate::catalog::{ColumnDef, ColumnType, IndexDef, IndexKind, TableSchema};
        let (s, _g) = tmp_storage("ns-list");
        s.create_schema("alice", "a", false).expect("schema");
        let id = s.schema_id("alice", "a").expect("id").expect("exists");
        let key = RelationKey::InSchema {
            schema_id: id,
            name: "t".to_string(),
        }
        .encode();
        let cols = || vec![ColumnDef::new("name", ColumnType::Text, true)];
        s.create_table(&TableSchema::new(key.clone(), cols()))
            .expect("schema table");
        s.create_table(&TableSchema::new("pubt", cols()))
            .expect("public table");
        s.create_index(&IndexDef::new(
            RelationKey::InSchema {
                schema_id: id,
                name: "i".to_string(),
            }
            .encode(),
            key,
            IndexKind::Scalar,
            vec!["name".to_string()],
        ))
        .expect("schema index");
        assert_eq!(s.list_tables().expect("list"), vec!["pubt".to_string()]);
        assert!(s.list_indexes().expect("list").is_empty());
    }

    /// 全 redb 表の (名前, 件数) と、型付きで読める SCHEMA 以外のカタログ表の全 key/value を dump する。
    fn dump_non_schema_state(s: &Storage) -> Vec<String> {
        use redb::{ReadableTableMetadata, TableHandle};
        let rt = s.db().begin_read().expect("read");
        let mut out = Vec::new();
        for h in rt.list_tables().expect("list") {
            let name = h.name().to_string();
            if name == "schemas" || name == "schema_id_counter" {
                continue;
            }
            let len = rt
                .open_untyped_table(h)
                .expect("untyped")
                .len()
                .expect("len");
            out.push(format!("table {name} len={len}"));
        }
        for (label, def) in [
            ("catalog", CATALOG_TABLE),
            ("views", VIEWS_TABLE),
            ("index_catalog", INDEX_CATALOG_TABLE),
            ("enum_types", ENUM_TYPES_TABLE),
        ] {
            if let Ok(t) = rt.open_table(def) {
                for e in t.iter().expect("iter") {
                    let (k, v) = e.expect("entry");
                    out.push(format!("{label} {} {:?}", k.value(), v.value()));
                }
            }
        }
        if let Ok(t) = rt.open_table(super::super::sequence::SEQUENCES_TABLE) {
            for e in t.iter().expect("iter") {
                let (k, v) = e.expect("entry");
                out.push(format!("sequences {:?} {:?}", k.value(), v.value()));
            }
        }
        if let Ok(t) = rt.open_table(super::super::sequence::SEQUENCE_STATE_TABLE) {
            for e in t.iter().expect("iter") {
                let (k, v) = e.expect("entry");
                out.push(format!("seq_state {:?} {:?}", k.value(), v.value()));
            }
        }
        out
    }

    /// AC2（Issue #1713）: SCHEMA の作成・削除・再オープンを経ても、既存カタログ（表・ビュー・
    /// 索引・SEQUENCE・ENUM 型・行ストア）のバイト列と表の構成が変わらない。
    #[test]
    fn schema_ddl_leaves_existing_catalog_bytes_unchanged() {
        use crate::core::EngineCore;
        use crate::kernel::CpuScalarProvider;
        use crate::policy::PolicyContext;
        use crate::sql::mode::SessionState;
        use crate::storage::Visibility;

        let path = crate::test_util::temp_db::unique_db_path("ns-bytes");
        let _guard = crate::test_util::temp_db::CleanupGuard(path.clone());
        let ctx = |t: &str| {
            PolicyContext::with_visibilities(t, [Visibility::Public, Visibility::Private])
                .expect("ctx")
        };
        let mut session = SessionState::default();
        session.allow_ddl();
        let core = EngineCore::from_storage(
            Storage::open(&path).expect("open"),
            Box::new(CpuScalarProvider),
        );
        for sql in [
            "CREATE TABLE t (id BIGINT PRIMARY KEY, embedding VECTOR(2), n BIGINT)",
            "CREATE VIEW v1 AS SELECT n FROM t",
            "CREATE INDEX idx1 ON t (n)",
            "CREATE SEQUENCE seq1",
            "CREATE TYPE mood AS ENUM ('a', 'b')",
        ] {
            core.execute_sql_in_session(&ctx("alice"), &mut session, sql)
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        for (tenant, id) in [("alice", 1), ("bob", 2)] {
            core.execute_sql_in_session(
                &ctx(tenant),
                &mut session,
                &format!(
                    "INSERT INTO t (id, embedding, n) VALUES ({id}, '[0.1,0.2]', {id}) USING OPERATION_ID 'op-{id}'"
                ),
            )
            .expect("insert");
        }
        let before = dump_non_schema_state(core.storage_for_test());
        assert!(before.iter().any(|l| l.starts_with("table user_rows/t")));
        // SCHEMA を一度も使っていない DB に SCHEMA 用の表は無い。
        assert!(!before.iter().any(|l| l.contains("schema")));

        for (tenant, sql) in [
            ("alice", "CREATE SCHEMA s1"),
            ("bob", "CREATE SCHEMA s1"),
            ("alice", "CREATE SCHEMA IF NOT EXISTS s1"),
            ("alice", "CREATE SCHEMA s2"),
            ("alice", "DROP SCHEMA s2"),
            ("bob", "DROP SCHEMA IF EXISTS nothere"),
        ] {
            core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
                .unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        assert_eq!(before, dump_non_schema_state(core.storage_for_test()));
        drop(core);
        let reopened = Storage::open(&path).expect("reopen");
        assert_eq!(before, dump_non_schema_state(&reopened));
        // 再オープン後も SCHEMA は残る（AC1）。
        assert!(reopened.schema_id("alice", "s1").expect("id").is_some());
        assert!(reopened.schema_id("bob", "s1").expect("id").is_some());
        assert!(reopened.schema_id("alice", "s2").expect("id").is_none());
    }
}
