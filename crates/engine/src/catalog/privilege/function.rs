//! 関数単位の ACL（`object_kind = 5`）の読み取り・暗黙の既定・掃除（SQL-40・RLS-12 のポインタ、Issue #2235。
//! 設計記録: `docs/design/role-privilege-model.md`「Issue #2235 実装記録」S5・S12）。
//!
//! 役割: 関数は PostgreSQL と同じく「作成直後は PUBLIC が EXECUTE を持つ」暗黙の既定を持つ（`proacl` が
//! NULL の状態）。ACL 行だけでは「既定のまま」と「PUBLIC から取り消し済み」を区別できないため、遅延作成表
//! [`ACL_INIT_TABLE`] の印を「ACL を実体化済み」の目印にする。
//! - 印が無い: 暗黙の既定（PUBLIC が EXECUTE を持つ）。行が 1 件でもあれば不整合で `CorruptSchema`
//!   （fail-closed）。既存の関数も印が無いので従来どおり誰でも呼べる。
//! - 印がある: 行だけが真実（行が 0 件なら誰も EXECUTE を持たない）。
//!
//! 書き込み（`apply.rs`）は、印の無い関数への初回の `GRANT`／`REVOKE` で暗黙の既定を行へ展開してから判定し、
//! 差分があれば全行と印を同じ txn で書く。関数の削除（`catalog::function` の唯一の削除経路）は
//! [`remove_function_acl_in_txn`] で行と印を同じ txn で消す。`CREATE OR REPLACE FUNCTION` は内部 ID を
//! 保つので ACL を引き継ぐ。テナント境界: すべて `tenant_id` を先頭にしたキーだけを読み書きする。

use redb::{ReadableTable, TableDefinition};

use crate::storage::read_source::ReadSource;

use super::super::role::validate_tenant;
use super::super::{CatalogError, Result, Storage};
use super::owner::owner_of_in_txn;
use super::{
    corrupt, decode_row, list_acl_for_object_in_txn, object_privileges_in_txn, AclEntry,
    PrivilegeObject, ACL_TABLE, KIND_FUNCTION, MAX_ACL_ENTRIES_PER_TENANT, PRIV_EXECUTE,
    PUBLIC_GRANTEE_ID,
};

/// 「ACL を実体化済み」の印の専用カタログ表（遅延作成）。キーは `(tenant_id, object_kind, object_key)`、
/// 値は版 1 バイト。
pub(crate) const ACL_INIT_TABLE: TableDefinition<(&str, u8, &str), &[u8]> =
    TableDefinition::new("acl_init");

/// 印の版。
const ACL_INIT_VERSION: u8 = 1;

/// 関数 `function_id` の ACL が実体化済み（印がある）か。印の値が不正なら `CorruptSchema`。
pub(crate) fn acl_initialized_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    function_id: u64,
) -> Result<bool> {
    validate_tenant(tenant)?;
    let table = match src.open_table(ACL_INIT_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let key = function_id.to_string();
    let found = table.get((tenant, KIND_FUNCTION, key.as_str()))?;
    match found {
        Some(v) if v.value() == [ACL_INIT_VERSION] => Ok(true),
        Some(_) => Err(corrupt()),
        None => Ok(false),
    }
}

/// 関数 `function_id` について、`role_ids` のいずれか（と PUBLIC）が持つ権限ビットの和を返す（EXECUTE）。
/// 印が無ければ暗黙の既定で PUBLIC が EXECUTE を持つ。印が無いのに行がある場合は `CorruptSchema`。
pub(crate) fn function_privileges_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    function_id: u64,
    role_ids: &[u64],
) -> Result<u16> {
    let object = PrivilegeObject::Function(function_id);
    if !acl_initialized_in_txn(src, tenant, function_id)? {
        if !list_acl_for_object_in_txn(src, tenant, &object)?.is_empty() {
            return Err(corrupt());
        }
        return Ok(PRIV_EXECUTE);
    }
    let granted = object_privileges_in_txn(src, tenant, &object, role_ids)?;
    // 関数の所有者は暗黙に EXECUTE を持つ（SCHEMA の所有者の暗黙 USAGE と同じ扱い。PUBLIC から取り消しても
    // 所有者は呼べる。所有者行は自テナントのみ引ける）。
    let owner_bits = match owner_of_in_txn(src, tenant, &object)? {
        Some(owner) if role_ids.contains(&owner) => PRIV_EXECUTE,
        _ => 0,
    };
    Ok(granted | owner_bits)
}

/// 印を書く（`apply.rs` が初回の変更と同じ write txn で呼ぶ）。
pub(crate) fn mark_acl_initialized_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    function_id: u64,
) -> Result<()> {
    validate_tenant(tenant)?;
    if function_id == 0 {
        return Err(CatalogError::Invalid(
            "invalid privilege object".to_string(),
        ));
    }
    let key = function_id.to_string();
    let mut table = write_txn.open_table(ACL_INIT_TABLE)?;
    table.insert(
        (tenant, KIND_FUNCTION, key.as_str()),
        [ACL_INIT_VERSION].as_slice(),
    )?;
    Ok(())
}

/// 暗黙の既定を展開した行（PUBLIC・EXECUTE。付与者は現在ロール）。
pub(crate) fn implicit_default_entry(grantor_id: u64) -> AclEntry {
    AclEntry {
        grantee_id: PUBLIC_GRANTEE_ID,
        grantor_id,
        privileges: PRIV_EXECUTE,
        grant_options: 0,
    }
}

/// `tenant` の関数 `function_id` の ACL 行と印を削除する（関数の削除と同じ write txn から呼ぶ）。他の種別・
/// 他の関数・他テナントの行には触れない。表が無ければ何もしない。
pub(crate) fn remove_function_acl_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    function_id: u64,
) -> Result<()> {
    validate_tenant(tenant)?;
    let key = function_id.to_string();
    let mut doomed: Vec<(u64, u64)> = Vec::new();
    match ReadSource::open_table(write_txn, ACL_TABLE) {
        Ok(table) => {
            for entry in table.range((tenant, KIND_FUNCTION, key.as_str(), "", 0u64, 0u64)..)? {
                let (k, v) = entry?;
                let kv = k.value();
                if kv.0 != tenant || kv.1 != KIND_FUNCTION || kv.2 != key {
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
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(e) => return Err(CatalogError::from(e)),
    }
    if !doomed.is_empty() {
        let mut table = write_txn.open_table(ACL_TABLE)?;
        for (grantee, grantor) in doomed {
            table.remove((tenant, KIND_FUNCTION, key.as_str(), "", grantee, grantor))?;
        }
    }
    match ReadSource::open_table(write_txn, ACL_INIT_TABLE) {
        Ok(_) => {}
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => return Err(CatalogError::from(e)),
    }
    let mut table = write_txn.open_table(ACL_INIT_TABLE)?;
    table.remove((tenant, KIND_FUNCTION, key.as_str()))?;
    Ok(())
}

impl Storage {
    /// `role_id` が EXECUTE を持たない `tenant` のカタログ由来の関数キー（小文字）の集合を、1 回の読み取り
    /// txn で返す（`core.rs` が文の実行区間の頭で `sql::object_privilege::FunctionAclScope` へ渡す。S7）。
    /// 関数は 1 テナント最大 [`crate::catalog::function::MAX_FUNCTIONS`] 件なので集合は有界。読み取りの
    /// 失敗・破損は `Err`（呼び出し側は文を失敗させる。fail-closed）。
    pub(crate) fn denied_function_keys(
        &self,
        tenant: &str,
        role_id: u64,
    ) -> Result<std::collections::BTreeSet<String>> {
        use redb::ReadableDatabase;
        let rt = self.db().begin_read()?;
        let mut denied = std::collections::BTreeSet::new();
        for (key, id) in super::super::function::function_ids_in_txn(&rt, tenant)? {
            if function_privileges_in_txn(&rt, tenant, id, &[role_id])? & PRIV_EXECUTE == 0 {
                denied.insert(key.to_ascii_lowercase());
            }
        }
        Ok(denied)
    }

    /// 読み取り専用スナップショットで [`function_privileges_in_txn`] を引く。
    pub(crate) fn function_privileges(
        &self,
        tenant: &str,
        function_id: u64,
        role_ids: &[u64],
    ) -> Result<u16> {
        use redb::ReadableDatabase;
        let rt = self.db().begin_read()?;
        function_privileges_in_txn(&rt, tenant, function_id, role_ids)
    }
}

#[cfg(test)]
mod tests;
