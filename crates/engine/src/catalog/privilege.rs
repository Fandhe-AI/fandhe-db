//! 権限（ACL）カタログのスキーマと保存形式、および Storage 水準の読み書き（SQL-40 のポインタ、Issue #1961。
//! 設計記録: `docs/design/role-privilege-model.md` の D5・D12）。
//!
//! 役割: 後続の `GRANT`／`REVOKE`（#1965・#2234・#2235）が書き、権限検査ゲート（#1966〜#1969）が読む付与の
//! 永続化層だけを担う。本ファイルは検査をしない。`catalog.rs` の relation（表・ビュー・matview）の DROP・
//! 改名・列の DROP・改名は、同じ write txn の中で本ファイルの `*_in_txn` を呼び、名前をキーにした付与が
//! 同名の再作成へ引き継がれる fail-open を作らない。
//!
//! キー設計: [`ACL_TABLE`] の `(tenant_id, object_kind, object_key, column, grantee_id, grantor_id)`。先頭が
//! `tenant_id` なので他テナントの行は構造上引けない。`object_kind` は 1=表（ビュー・matview を含む relation）・
//! 2=列・3=スキーマ・4=シーケンス・5=関数。`object_key` は表・列では relation キー、スキーマ・シーケンス・
//! 関数では内部 ID の 10 進正規形（ID は再利用されないため、作り直しても古い付与を拾わない）。`column` は列の
//! 種別だけが識別子を持ち、他では空文字列。`grantee_id` の 0 は PUBLIC（同じテナントのロール全体）の予約 ID、
//! `grantor_id` の 0 は破損（ロール blob・メンバーシップのキーと違い、ID 0 を受理するのは `grantee_id` だけ）。
//!
//! 既知の制約: relation 単位の掃除は名前をキーにした行が全テナントに散るため、ACL 表を全件走査する（計算量は
//! ACL の総件数に比例）。逆引き索引の追加は #1965 以降の判断とする。
//!
//! 既存データの不変: [`ACL_TABLE`] は最初の書き込みまで作らない（遅延作成。読み取りの `TableDoesNotExist` は
//! 0 件と扱い、掃除も表が無ければ何もしない）。
//!
//! fail-closed: 許可集合の外のビット・`grant_options` が `privileges` の部分集合でない値・`privileges` が 0 の値・
//! 未知の版やタグ・切断・末尾の余りは固定文言の `CorruptSchema`。エラーに blob の断片・tenant_id を含めない。

use redb::{ReadableTable, TableDefinition};

use crate::storage::read_source::ReadSource;

use super::role::validate_tenant;
use super::{validate_identifier, validate_relation_key, CatalogError, Result, Storage};

/// ACL の専用カタログ表。キーは `(tenant_id, object_kind, object_key, column, grantee_id, grantor_id)`。
#[allow(clippy::type_complexity)] // 6 成分の複合キー（先頭 tenant_id）。型別名にすると redb の寿命推論が崩れる。
pub(crate) const ACL_TABLE: TableDefinition<(&str, u8, &str, &str, u64, u64), &[u8]> =
    TableDefinition::new("acl");

/// ACL blob の版。
pub(crate) const ACL_BLOB_VERSION: u8 = 1;

/// 1 テナントあたりの ACL 行数の上限（実装既定値）。
pub(crate) const MAX_ACL_ENTRIES_PER_TENANT: usize = 100_000;

/// PUBLIC を表す予約 `grantee_id`。
pub(crate) const PUBLIC_GRANTEE_ID: u64 = 0;

/// 件数上限超過の `ViewLimitExceeded` 文言（`54000`）。
pub(crate) const ACL_LIMIT_MESSAGE: &str = "too many privilege entries";

/// 権限ビット（PostgreSQL 17 の公開の権限種別）。
pub(crate) const PRIV_SELECT: u16 = 1 << 0;
pub(crate) const PRIV_INSERT: u16 = 1 << 1;
pub(crate) const PRIV_UPDATE: u16 = 1 << 2;
pub(crate) const PRIV_DELETE: u16 = 1 << 3;
pub(crate) const PRIV_TRUNCATE: u16 = 1 << 4;
pub(crate) const PRIV_REFERENCES: u16 = 1 << 5;
pub(crate) const PRIV_TRIGGER: u16 = 1 << 6;
pub(crate) const PRIV_MAINTAIN: u16 = 1 << 7;
pub(crate) const PRIV_USAGE: u16 = 1 << 8;
pub(crate) const PRIV_CREATE: u16 = 1 << 9;
pub(crate) const PRIV_EXECUTE: u16 = 1 << 10;

const KIND_TABLE: u8 = 1;
const KIND_COLUMN: u8 = 2;
const KIND_SCHEMA: u8 = 3;
const KIND_SEQUENCE: u8 = 4;
const KIND_FUNCTION: u8 = 5;

/// 1 バッチの件数（掃除の走査）。
const SWEEP_BATCH: usize = 256;

fn corrupt() -> CatalogError {
    CatalogError::CorruptSchema("invalid privilege catalog entry".to_string())
}

fn invalid(msg: &str) -> CatalogError {
    CatalogError::Invalid(msg.to_string())
}

/// 権限の対象。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PrivilegeObject {
    /// 表・ビュー・matview（relation キー）。
    Table(String),
    /// 表の列。
    Column { table: String, column: String },
    /// スキーマ（内部 ID）。
    Schema(u64),
    /// シーケンス（内部 ID）。
    Sequence(u64),
    /// 関数（内部 ID）。
    Function(u64),
}

impl PrivilegeObject {
    /// 種別ごとに許す権限ビットの集合。
    pub(crate) fn allowed_privileges(&self) -> u16 {
        match self {
            Self::Table(_) => {
                PRIV_SELECT
                    | PRIV_INSERT
                    | PRIV_UPDATE
                    | PRIV_DELETE
                    | PRIV_TRUNCATE
                    | PRIV_REFERENCES
                    | PRIV_TRIGGER
                    | PRIV_MAINTAIN
            }
            Self::Column { .. } => PRIV_SELECT | PRIV_INSERT | PRIV_UPDATE | PRIV_REFERENCES,
            Self::Schema(_) => PRIV_USAGE | PRIV_CREATE,
            Self::Sequence(_) => PRIV_USAGE | PRIV_SELECT | PRIV_UPDATE,
            Self::Function(_) => PRIV_EXECUTE,
        }
    }

    /// 形状の検証（キー成分へ分解する前に通す）。
    fn validate(&self) -> Result<()> {
        match self {
            Self::Table(t) => validate_relation_key(t),
            Self::Column { table, column } => {
                validate_relation_key(table)?;
                validate_identifier(column)
            }
            Self::Schema(id) | Self::Sequence(id) | Self::Function(id) => {
                if *id == 0 {
                    return Err(invalid("invalid privilege object"));
                }
                Ok(())
            }
        }
    }

    /// キー成分 `(object_kind, object_key, column)` へ分解する。
    fn to_components(&self) -> Result<(u8, String, String)> {
        self.validate()?;
        Ok(match self {
            Self::Table(t) => (KIND_TABLE, t.clone(), String::new()),
            Self::Column { table, column } => (KIND_COLUMN, table.clone(), column.clone()),
            Self::Schema(id) => (KIND_SCHEMA, id.to_string(), String::new()),
            Self::Sequence(id) => (KIND_SEQUENCE, id.to_string(), String::new()),
            Self::Function(id) => (KIND_FUNCTION, id.to_string(), String::new()),
        })
    }

    /// キー成分から復元する。未知のタグ・形状の不整合・`column` の不整合・ID の正規形でない値は破損。
    fn from_components(kind: u8, key: &str, column: &str) -> Result<Self> {
        let id_of = |key: &str| -> Result<u64> {
            let id: u64 = key.parse().map_err(|_| corrupt())?;
            if id == 0 || id.to_string() != key {
                return Err(corrupt());
            }
            Ok(id)
        };
        let object = match kind {
            KIND_TABLE => Self::Table(key.to_string()),
            KIND_COLUMN => Self::Column {
                table: key.to_string(),
                column: column.to_string(),
            },
            KIND_SCHEMA => Self::Schema(id_of(key)?),
            KIND_SEQUENCE => Self::Sequence(id_of(key)?),
            KIND_FUNCTION => Self::Function(id_of(key)?),
            _ => return Err(corrupt()),
        };
        if kind != KIND_COLUMN && !column.is_empty() {
            return Err(corrupt());
        }
        object.validate().map_err(|_| corrupt())?;
        Ok(object)
    }
}

/// ACL の 1 行（対象はキー成分で、本構造体には持たない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AclEntry {
    /// 付与先のロール ID（0 は PUBLIC）。
    pub(crate) grantee_id: u64,
    /// 付与者のロール ID（0 は破損）。
    pub(crate) grantor_id: u64,
    pub(crate) privileges: u16,
    /// `privileges` の部分集合（GRANT OPTION）。
    pub(crate) grant_options: u16,
}

/// 権限集合の検証（0・許可集合の外・部分集合違反）。
fn validate_privileges(allowed: u16, privileges: u16, grant_options: u16) -> bool {
    privileges != 0 && privileges & !allowed == 0 && grant_options & !privileges == 0
}

/// ACL blob へ符号化する（版 1・`privileges(u16)`・`grant_options(u16)`。リトルエンディアン）。
pub(crate) fn encode_acl_blob(
    object: &PrivilegeObject,
    privileges: u16,
    grant_options: u16,
) -> Result<Vec<u8>> {
    if !validate_privileges(object.allowed_privileges(), privileges, grant_options) {
        return Err(invalid("invalid privilege set"));
    }
    let mut out = vec![ACL_BLOB_VERSION];
    out.extend_from_slice(&privileges.to_le_bytes());
    out.extend_from_slice(&grant_options.to_le_bytes());
    Ok(out)
}

/// ACL blob を復号する（`object` はキー成分から復元した対象。許可集合の判定に使う）。
pub(crate) fn decode_acl_blob(object: &PrivilegeObject, bytes: &[u8]) -> Result<(u16, u16)> {
    let [version, p0, p1, g0, g1] = *bytes else {
        return Err(corrupt());
    };
    if version != ACL_BLOB_VERSION {
        return Err(corrupt());
    }
    let privileges = u16::from_le_bytes([p0, p1]);
    let grant_options = u16::from_le_bytes([g0, g1]);
    if !validate_privileges(object.allowed_privileges(), privileges, grant_options) {
        return Err(corrupt());
    }
    Ok((privileges, grant_options))
}

type AclKey = (String, u8, String, String, u64, u64);

fn decode_row(
    key: (&str, u8, &str, &str, u64, u64),
    value: &[u8],
) -> Result<(PrivilegeObject, AclEntry)> {
    let (_, kind, object_key, column, grantee_id, grantor_id) = key;
    let object = PrivilegeObject::from_components(kind, object_key, column)?;
    if grantor_id == 0 {
        return Err(corrupt());
    }
    let (privileges, grant_options) = decode_acl_blob(&object, value)?;
    Ok((
        object,
        AclEntry {
            grantee_id,
            grantor_id,
            privileges,
            grant_options,
        },
    ))
}

/// 付与を書く（同じ `(対象, grantee, grantor)` は置き換え）。`privileges` が 0 なら行を消す。`grantor_id` 0・
/// 許可集合の外のビット・部分集合違反は拒否する。テナントあたりの上限は [`MAX_ACL_ENTRIES_PER_TENANT`]。
pub(crate) fn upsert_acl_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    object: &PrivilegeObject,
    grantee_id: u64,
    grantor_id: u64,
    privileges: u16,
    grant_options: u16,
) -> Result<()> {
    upsert_acl_limited(
        write_txn,
        tenant,
        object,
        grantee_id,
        grantor_id,
        privileges,
        grant_options,
        MAX_ACL_ENTRIES_PER_TENANT,
    )
}

#[allow(clippy::too_many_arguments)]
fn upsert_acl_limited(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    object: &PrivilegeObject,
    grantee_id: u64,
    grantor_id: u64,
    privileges: u16,
    grant_options: u16,
    max_entries: usize,
) -> Result<()> {
    validate_tenant(tenant)?;
    let (kind, key, column) = object.to_components()?;
    if grantor_id == 0 {
        return Err(invalid("invalid privilege grantor"));
    }
    if privileges == 0 {
        if grant_options != 0 {
            return Err(invalid("invalid privilege set"));
        }
        remove_acl_in_txn(write_txn, tenant, object, grantee_id, grantor_id)?;
        return Ok(());
    }
    let blob = encode_acl_blob(object, privileges, grant_options)?;
    let k = (
        tenant,
        kind,
        key.as_str(),
        column.as_str(),
        grantee_id,
        grantor_id,
    );
    let is_new = match ReadSource::open_table(write_txn, ACL_TABLE) {
        Ok(t) => t.get(k)?.is_none(),
        Err(redb::TableError::TableDoesNotExist(_)) => true,
        Err(e) => return Err(e.into()),
    };
    if is_new && count_tenant_entries(write_txn, tenant, max_entries)? >= max_entries {
        return Err(CatalogError::ViewLimitExceeded(
            ACL_LIMIT_MESSAGE.to_string(),
        ));
    }
    let mut table = write_txn.open_table(ACL_TABLE)?;
    table.insert(k, blob.as_slice())?;
    Ok(())
}

/// テナントの行数を `limit` まで数える（上限判定用。無制限に走査しない）。
fn count_tenant_entries<R: ReadSource>(src: &R, tenant: &str, limit: usize) -> Result<usize> {
    let table = match src.open_table(ACL_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut n = 0usize;
    for entry in table.range((tenant, 0u8, "", "", 0u64, 0u64)..)? {
        let (k, _) = entry?;
        if k.value().0 != tenant || n >= limit {
            break;
        }
        n += 1;
    }
    Ok(n)
}

/// 付与 1 行を削除し、存在して消したら `true`。
pub(crate) fn remove_acl_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    object: &PrivilegeObject,
    grantee_id: u64,
    grantor_id: u64,
) -> Result<bool> {
    validate_tenant(tenant)?;
    let (kind, key, column) = object.to_components()?;
    let k = (
        tenant,
        kind,
        key.as_str(),
        column.as_str(),
        grantee_id,
        grantor_id,
    );
    let exists = match ReadSource::open_table(write_txn, ACL_TABLE) {
        Ok(t) => t.get(k)?.is_some(),
        Err(redb::TableError::TableDoesNotExist(_)) => false,
        Err(e) => return Err(e.into()),
    };
    if !exists {
        return Ok(false);
    }
    let mut table = write_txn.open_table(ACL_TABLE)?;
    table.remove(k)?;
    Ok(true)
}

/// 対象 1 つの付与を `(grantee_id, grantor_id)` の昇順で返す（表が無ければ空）。破損した行が 1 件でもあれば
/// `Err`（読み飛ばして「付与なし」「付与あり」のどちらにも倒さない）。他テナントの行は構造上引けない。
pub(crate) fn list_acl_for_object_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    object: &PrivilegeObject,
) -> Result<Vec<AclEntry>> {
    validate_tenant(tenant)?;
    let (kind, key, column) = object.to_components()?;
    let table = match src.open_table(ACL_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.range((tenant, kind, key.as_str(), column.as_str(), 0u64, 0u64)..)? {
        let (k, v) = entry?;
        let kv = k.value();
        if kv.0 != tenant || kv.1 != kind || kv.2 != key || kv.3 != column {
            break;
        }
        if out.len() >= MAX_ACL_ENTRIES_PER_TENANT {
            return Err(corrupt());
        }
        let (_, e) = decode_row(kv, v.value())?;
        out.push(e);
    }
    Ok(out)
}

/// `collect_matching` の戻り値（集めた行と、続きがありうるときの再開キー）。
type MatchBatch = (Vec<(AclKey, Vec<u8>)>, Option<AclKey>);

/// relation（表・列）に属する行で `pred(kind, object_key, column)` が真のものを、全テナントから最大 `limit`
/// 件集める（バッチ走査。ACL 表が無ければ空）。`after` が `Some` ならそのキーの直後から走査を再開し、
/// 毎回表の先頭から走り直す O(N×バッチ数) の再走査を避ける。戻り値の 2 要素目は、上限に達して続きが
/// ありうるときの再開キー（最後に集めたキー）で、`None` は走査完了を表す。
fn collect_matching<R: ReadSource>(
    src: &R,
    limit: usize,
    after: Option<&AclKey>,
    pred: &dyn Fn(u8, &str, &str) -> bool,
) -> Result<MatchBatch> {
    let table = match src.open_table(ACL_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok((Vec::new(), None)),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    let iter = match after {
        Some(a) => table.range::<(&str, u8, &str, &str, u64, u64)>((
            std::ops::Bound::Excluded((a.0.as_str(), a.1, a.2.as_str(), a.3.as_str(), a.4, a.5)),
            std::ops::Bound::Unbounded,
        ))?,
        None => table.iter()?,
    };
    let mut cursor = None;
    for entry in iter {
        let (k, v) = entry?;
        let (t, kind, key, col, grantee, grantor) = k.value();
        if pred(kind, key, col) {
            out.push((
                (
                    t.to_string(),
                    kind,
                    key.to_string(),
                    col.to_string(),
                    grantee,
                    grantor,
                ),
                v.value().to_vec(),
            ));
            if out.len() >= limit {
                cursor = out.last().map(|(key, _)| key.clone());
                break;
            }
        }
    }
    Ok((out, cursor))
}

fn remove_matching(
    write_txn: &redb::WriteTransaction,
    pred: &dyn Fn(u8, &str, &str) -> bool,
) -> Result<()> {
    let mut cursor: Option<AclKey> = None;
    loop {
        let (batch, next) = collect_matching(write_txn, SWEEP_BATCH, cursor.as_ref(), pred)?;
        if batch.is_empty() {
            return Ok(());
        }
        let mut table = write_txn.open_table(ACL_TABLE)?;
        for ((t, kind, key, col, grantee, grantor), _) in &batch {
            table.remove((
                t.as_str(),
                *kind,
                key.as_str(),
                col.as_str(),
                *grantee,
                *grantor,
            ))?;
        }
        match next {
            Some(c) => cursor = Some(c),
            None => return Ok(()),
        }
    }
}

fn move_matching(
    write_txn: &redb::WriteTransaction,
    pred: &dyn Fn(u8, &str, &str) -> bool,
    rewrite: &dyn Fn(&AclKey) -> AclKey,
) -> Result<()> {
    let mut cursor: Option<AclKey> = None;
    loop {
        let (batch, next) = collect_matching(write_txn, SWEEP_BATCH, cursor.as_ref(), pred)?;
        if batch.is_empty() {
            return Ok(());
        }
        let mut table = write_txn.open_table(ACL_TABLE)?;
        for (old, blob) in &batch {
            let new = rewrite(old);
            table.remove((
                old.0.as_str(),
                old.1,
                old.2.as_str(),
                old.3.as_str(),
                old.4,
                old.5,
            ))?;
            table.insert(
                (
                    new.0.as_str(),
                    new.1,
                    new.2.as_str(),
                    new.3.as_str(),
                    new.4,
                    new.5,
                ),
                blob.as_slice(),
            )?;
        }
        match next {
            Some(c) => cursor = Some(c),
            None => return Ok(()),
        }
    }
}

fn is_relation_row(kind: u8) -> bool {
    kind == KIND_TABLE || kind == KIND_COLUMN
}

/// relation `relation_key` の全テナント分の表・列の付与を削除する（`DROP TABLE`・`DROP VIEW`・
/// `DROP MATERIALIZED VIEW` から同じ write txn で呼ぶ。同名の再作成へ付与を引き継がせない）。
/// ID をキーにした種別（スキーマ・シーケンス・関数）には触れない。表が無ければ何もしない。
pub(crate) fn remove_relation_acl_in_txn(
    write_txn: &redb::WriteTransaction,
    relation_key: &str,
) -> Result<()> {
    remove_matching(write_txn, &|kind, key, _| {
        is_relation_row(kind) && key == relation_key
    })
}

/// relation の改名に追随して全テナント分の表・列の付与を新しいキーへ移す。改名先に残っていた孤児の行は先に
/// 消す。
pub(crate) fn rename_relation_acl_in_txn(
    write_txn: &redb::WriteTransaction,
    old_key: &str,
    new_key: &str,
) -> Result<()> {
    if old_key == new_key {
        return Ok(());
    }
    remove_relation_acl_in_txn(write_txn, new_key)?;
    move_matching(
        write_txn,
        &|kind, key, _| is_relation_row(kind) && key == old_key,
        &|old| {
            (
                old.0.clone(),
                old.1,
                new_key.to_string(),
                old.3.clone(),
                old.4,
                old.5,
            )
        },
    )
}

/// 列 `column` の全テナント分の付与を削除する（`ALTER TABLE DROP COLUMN` から同じ write txn で呼ぶ）。
pub(crate) fn remove_column_acl_in_txn(
    write_txn: &redb::WriteTransaction,
    relation_key: &str,
    column: &str,
) -> Result<()> {
    remove_matching(write_txn, &|kind, key, col| {
        kind == KIND_COLUMN && key == relation_key && col == column
    })
}

/// 列の改名に追随して全テナント分の付与を新しい列名へ移す。改名先の列名に残っていた孤児の行は先に消す。
pub(crate) fn rename_column_acl_in_txn(
    write_txn: &redb::WriteTransaction,
    relation_key: &str,
    old_column: &str,
    new_column: &str,
) -> Result<()> {
    if old_column == new_column {
        return Ok(());
    }
    remove_column_acl_in_txn(write_txn, relation_key, new_column)?;
    move_matching(
        write_txn,
        &|kind, key, col| kind == KIND_COLUMN && key == relation_key && col == old_column,
        &|old| {
            (
                old.0.clone(),
                old.1,
                old.2.clone(),
                new_column.to_string(),
                old.4,
                old.5,
            )
        },
    )
}

impl Storage {
    /// `tenant` の対象 `object` の付与を返す（読み取り専用。破損は `Err`）。
    pub(crate) fn list_acl_for_object(
        &self,
        tenant: &str,
        object: &PrivilegeObject,
    ) -> Result<Vec<AclEntry>> {
        use redb::ReadableDatabase;
        let rt = self.db().begin_read()?;
        list_acl_for_object_in_txn(&rt, tenant, object)
    }
}

#[cfg(test)]
mod tests;
