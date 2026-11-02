//! SCHEMA 単位の ACL（`object_kind = 3`）の読み取りと掃除（SQL-40・RLS-12 のポインタ、Issue #2234。
//! 設計記録: `docs/design/role-privilege-model.md`「Issue #2234 実装記録」）。
//!
//! 役割: 書き込み（`GRANT`／`REVOKE`）は `apply.rs`、USAGE 検査は `core.rs` の SCHEMA 解決器が本ファイルの
//! [`schema_privileges_in_txn`]／[`Storage::schema_privileges`] を引く。`DROP SCHEMA` は同じ write txn で
//! [`remove_schema_acl_in_txn`] を呼び、削除済み SCHEMA の ACL 行が `DROP ROLE` の依存検査
//! （`acl_references_role_in_txn`）とテナントあたりの件数上限を消費し続けないようにする。
//!
//! テナント境界（P0）: 走査は `(tenant_id, kind, id)` の範囲だけで、他テナントの行は構造上引けない。
//! 破損行は読み飛ばさず `Err`（fail-closed）。

use redb::ReadableTable;

use crate::storage::read_source::ReadSource;

use super::super::role::validate_tenant;
use super::super::{CatalogError, Result, Storage};
use super::{
    corrupt, decode_row, list_acl_for_object_in_txn, PrivilegeObject, ACL_TABLE, KIND_SCHEMA,
    MAX_ACL_ENTRIES_PER_TENANT, PUBLIC_GRANTEE_ID,
};

/// SCHEMA `schema_id` について、`role_ids` のいずれか（と PUBLIC）が持つ権限ビットの和を返す。
///
/// 継承（メンバーシップ経由）は #2232 の担当で、呼び出し元が渡すロール集合に含める。本関数は渡された集合の
/// 直接の行と PUBLIC の行だけを見る。ACL 表が無い・行が無ければ 0。
pub(crate) fn schema_privileges_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    schema_id: u64,
    role_ids: &[u64],
) -> Result<u16> {
    let entries = list_acl_for_object_in_txn(src, tenant, &PrivilegeObject::Schema(schema_id))?;
    Ok(entries
        .iter()
        .filter(|e| e.grantee_id == PUBLIC_GRANTEE_ID || role_ids.contains(&e.grantee_id))
        .fold(0u16, |acc, e| acc | e.privileges))
}

/// `tenant` の SCHEMA `schema_id` の ACL 行をすべて削除する（`DROP SCHEMA` の同一 write txn から呼ぶ）。
/// 他の種別・他の SCHEMA・他テナントの行には触れない。表が無ければ何もしない。
pub(crate) fn remove_schema_acl_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    schema_id: u64,
) -> Result<()> {
    validate_tenant(tenant)?;
    let key = schema_id.to_string();
    let mut doomed: Vec<(u64, u64)> = Vec::new();
    {
        let table = match ReadSource::open_table(write_txn, ACL_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(e) => return Err(CatalogError::from(e)),
        };
        for entry in table.range((tenant, KIND_SCHEMA, key.as_str(), "", 0u64, 0u64)..)? {
            let (k, v) = entry?;
            let kv = k.value();
            if kv.0 != tenant || kv.1 != KIND_SCHEMA || kv.2 != key {
                break;
            }
            if doomed.len() >= MAX_ACL_ENTRIES_PER_TENANT {
                return Err(corrupt());
            }
            // 構造の破損は削除せずに `Err` にする（検証してから消す）。
            decode_row(kv, v.value())?;
            doomed.push((kv.4, kv.5));
        }
    }
    if doomed.is_empty() {
        return Ok(());
    }
    let mut table = write_txn.open_table(ACL_TABLE)?;
    for (grantee, grantor) in doomed {
        table.remove((tenant, KIND_SCHEMA, key.as_str(), "", grantee, grantor))?;
    }
    Ok(())
}

impl Storage {
    /// 読み取り専用スナップショットで [`schema_privileges_in_txn`] を引く（SCHEMA 解決器の USAGE 検査。
    /// `core.rs` の `schema_resolver_for` から呼ばれる）。
    pub(crate) fn schema_privileges(
        &self,
        tenant: &str,
        schema_id: u64,
        role_ids: &[u64],
    ) -> Result<u16> {
        use redb::ReadableDatabase;
        let rt = self.db().begin_read()?;
        schema_privileges_in_txn(&rt, tenant, schema_id, role_ids)
    }
}

#[cfg(test)]
mod tests;
