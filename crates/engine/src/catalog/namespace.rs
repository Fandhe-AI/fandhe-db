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

    /// `(tenant_id, name)` の SCHEMA の内部 ID（後続の名前解決用。読み取り専用）。
    #[allow(dead_code)]
    pub(crate) fn schema_id(&self, tenant_id: &str, name: &str) -> Result<Option<u64>> {
        let rt = self.db().begin_read()?;
        schema_id_in_txn(&rt, tenant_id, name)
    }
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
            "CREATE TABLE t (embedding VECTOR(2), n BIGINT)",
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
