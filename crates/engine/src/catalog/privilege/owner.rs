//! オブジェクト所有者のカタログ（SQL-40・RLS-12 のポインタ、Issue #2236。設計記録:
//! `docs/design/role-privilege-model.md` §14）。
//!
//! 役割: 表・ビュー・matview・SCHEMA・関数の所有者ロールを永続化し、所有者判定の唯一の関数
//! [`is_owner_in_txn`] を提供する。ACL（`privilege.rs`）と同じ対象表現（[`PrivilegeObject`]）と
//! 同じライフサイクル規則（DROP・改名の同一 write txn で掃除する）を使う。権限検査の呼び出し元は
//! `sql::privilege_gate` の所有者判定、`catalog::privilege::apply`（GRANT／REVOKE の所有者相当）、
//! `sql::schema_privilege`（SCHEMA の USAGE・CREATE）、`sql::owner_ddl`（`OWNER TO`）。
//!
//! キー設計: [`OWNERS_TABLE`] は `(object_kind, object_key, tenant_id)` → blob（版 1・owner_id u64 LE）。
//! `object_kind`・`object_key` は ACL と同じ規則（relation は relation キー、SCHEMA・関数は内部 ID の
//! 10 進正規形）。relation は全テナント共有カタログにあるため、所有者はテナントごとの行にする。
//! `tenant_id` を末尾に置くのは、relation の DROP・改名で全テナント分の行を範囲走査だけで掃除する
//! ため（ACL のような全件走査を避ける）。読み書きは常に 3 成分の完全一致で、テナントを跨いで引く
//! 経路は持たない（一覧が必要な [`owner_references_role_in_txn`] は走査しつつテナントで絞る）。
//!
//! 既存データの不変: [`OWNERS_TABLE`] は最初の書き込みまで作らない（遅延作成。読み取りの
//! `TableDoesNotExist` は「所有者なし」）。所有者行の無いオブジェクトはどのロールの所有にもならず、
//! 所有者として振る舞えるのは `--ddl-allowed-users` の運用者だけ（拒否側。ADR O5）。
//!
//! fail-closed: 未知の版・owner 0・切断・末尾の余りは固定文言の `CorruptSchema`。エラーに blob の断片・
//! tenant_id を含めない。

use redb::{ReadableTable, TableDefinition};

use crate::storage::read_source::ReadSource;

use super::super::role::validate_tenant;
use super::super::{CatalogError, Result};
use super::{PrivilegeObject, KIND_COLUMN, KIND_FUNCTION, KIND_SCHEMA, KIND_TABLE};

/// 所有者の専用カタログ表。キーは `(object_kind, object_key, tenant_id)`。
pub(crate) const OWNERS_TABLE: TableDefinition<(u8, &str, &str), &[u8]> =
    TableDefinition::new("object_owners");

/// 所有者 blob の版。
const OWNER_BLOB_VERSION: u8 = 1;

/// relation の掃除で 1 度に集める行数（無制限にメモリへ載せない）。
const SWEEP_BATCH: usize = 256;

fn corrupt() -> CatalogError {
    CatalogError::CorruptSchema("invalid object owner entry".to_string())
}

/// 所有者を持てる対象のキー成分 `(kind, key)`。列は親の表の所有者を見る。
fn owner_components(object: &PrivilegeObject) -> Result<(u8, String)> {
    let (kind, key, _) = object.to_components()?;
    let kind = if kind == KIND_COLUMN {
        KIND_TABLE
    } else {
        kind
    };
    if !matches!(kind, KIND_TABLE | KIND_SCHEMA | KIND_FUNCTION) {
        return Err(CatalogError::Invalid("invalid owner object".to_string()));
    }
    Ok((kind, key))
}

fn encode_owner(owner_id: u64) -> [u8; 9] {
    let mut out = [0u8; 9];
    out[0] = OWNER_BLOB_VERSION;
    out[1..].copy_from_slice(&owner_id.to_le_bytes());
    out
}

fn decode_owner(bytes: &[u8]) -> Result<u64> {
    let [version, rest @ ..] = bytes else {
        return Err(corrupt());
    };
    let id_bytes: [u8; 8] = rest.try_into().map_err(|_| corrupt())?;
    let id = u64::from_le_bytes(id_bytes);
    if *version != OWNER_BLOB_VERSION || id == 0 {
        return Err(corrupt());
    }
    Ok(id)
}

/// 対象の所有者ロール ID（行が無ければ `None`。他テナントの行は引けない）。
pub(crate) fn owner_of_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    object: &PrivilegeObject,
) -> Result<Option<u64>> {
    validate_tenant(tenant)?;
    let (kind, key) = owner_components(object)?;
    let table = match src.open_table(OWNERS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let Some(v) = table.get((kind, key.as_str(), tenant))? else {
        return Ok(None);
    };
    Ok(Some(decode_owner(v.value())?))
}

/// `role_id` が対象の所有者か。所有者判定の唯一の関数（`role_id` 0＝PUBLIC・未束縛は常に偽）。
pub(crate) fn is_owner_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    object: &PrivilegeObject,
    role_id: u64,
) -> Result<bool> {
    if role_id == 0 {
        return Ok(false);
    }
    Ok(owner_of_in_txn(src, tenant, object)? == Some(role_id))
}

/// 所有者を書く（置き換え）。`owner_id` 0 は拒否する。
pub(crate) fn set_owner_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    object: &PrivilegeObject,
    owner_id: u64,
) -> Result<()> {
    validate_tenant(tenant)?;
    if owner_id == 0 {
        return Err(CatalogError::Invalid("invalid object owner".to_string()));
    }
    let (kind, key) = owner_components(object)?;
    let mut table = write_txn.open_table(OWNERS_TABLE)?;
    table.insert(
        (kind, key.as_str(), tenant),
        encode_owner(owner_id).as_slice(),
    )?;
    Ok(())
}

/// 所有者行が無いときだけ書く（`CREATE OR REPLACE` が既存の所有者を変えないため）。
pub(crate) fn set_owner_if_absent_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    object: &PrivilegeObject,
    owner_id: u64,
) -> Result<()> {
    if owner_of_in_txn(write_txn, tenant, object)?.is_some() {
        return Ok(());
    }
    set_owner_in_txn(write_txn, tenant, object, owner_id)
}

/// 対象の所有者行（自テナント分）を消す（SCHEMA・関数の DROP から同じ write txn で呼ぶ）。
pub(crate) fn remove_owner_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    object: &PrivilegeObject,
) -> Result<()> {
    validate_tenant(tenant)?;
    let (kind, key) = owner_components(object)?;
    if owner_of_in_txn(write_txn, tenant, object)?.is_none() {
        return Ok(());
    }
    let mut table = write_txn.open_table(OWNERS_TABLE)?;
    table.remove((kind, key.as_str(), tenant))?;
    Ok(())
}

/// relation `(kind, key)` の全テナント分の行（テナント ID と blob）を最大 [`SWEEP_BATCH`] 件集める。
fn collect_relation_rows<R: ReadSource>(src: &R, key: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let table = match src.open_table(OWNERS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.range((KIND_TABLE, key, "")..)? {
        let (k, v) = entry?;
        let (kind, k_key, tenant) = k.value();
        if kind != KIND_TABLE || k_key != key {
            break;
        }
        out.push((tenant.to_string(), v.value().to_vec()));
        if out.len() >= SWEEP_BATCH {
            break;
        }
    }
    Ok(out)
}

/// relation `relation_key` の全テナント分の所有者行を削除する（`DROP TABLE`・`DROP VIEW`・
/// `DROP MATERIALIZED VIEW` から同じ write txn で呼ぶ。同名の再作成へ所有者を引き継がせない）。
pub(crate) fn remove_relation_owners_in_txn(
    write_txn: &redb::WriteTransaction,
    relation_key: &str,
) -> Result<()> {
    loop {
        let rows = collect_relation_rows(write_txn, relation_key)?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut table = write_txn.open_table(OWNERS_TABLE)?;
        for (tenant, _) in &rows {
            table.remove((KIND_TABLE, relation_key, tenant.as_str()))?;
        }
    }
}

/// relation の改名に追随して全テナント分の所有者行を新しいキーへ移す（改名先に残った孤児は先に消す）。
pub(crate) fn rename_relation_owners_in_txn(
    write_txn: &redb::WriteTransaction,
    old_key: &str,
    new_key: &str,
) -> Result<()> {
    if old_key == new_key {
        return Ok(());
    }
    remove_relation_owners_in_txn(write_txn, new_key)?;
    loop {
        let rows = collect_relation_rows(write_txn, old_key)?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut table = write_txn.open_table(OWNERS_TABLE)?;
        for (tenant, blob) in &rows {
            table.remove((KIND_TABLE, old_key, tenant.as_str()))?;
            table.insert((KIND_TABLE, new_key, tenant.as_str()), blob.as_slice())?;
        }
    }
}

/// `tenant` で `role_id` を所有者とするオブジェクトがあるか（`DROP ROLE` の `2BP01` 判定。`catalog::role` の
/// `role_has_dependents_in_txn` から呼ばれる）。破損行は読み飛ばさず `Err`（fail-closed）。
pub(crate) fn owner_references_role_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    role_id: u64,
) -> Result<bool> {
    validate_tenant(tenant)?;
    if role_id == 0 {
        return Ok(false);
    }
    let table = match src.open_table(OWNERS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    for entry in table.iter()? {
        let (k, v) = entry?;
        let (_, _, row_tenant) = k.value();
        if row_tenant != tenant {
            continue;
        }
        if decode_owner(v.value())? == role_id {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests;
