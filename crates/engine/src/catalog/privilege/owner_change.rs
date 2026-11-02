//! 所有者の変更（`ALTER ... OWNER TO`）の永続化本体（SQL-40・RLS-12 のポインタ、Issue #2236。設計記録:
//! `docs/design/role-privilege-model.md` §14 の O8〜O10）。
//!
//! 役割: 対象の解決・新所有者の解決・実行者の判定・所有者行の書き込み・ACL の書き換え・世代の更新を
//! **単一の write txn・1 回の commit** で行う（検査と変更の間に並行する `OWNER TO`／DROP が割り込む
//! TOCTOU を作らない）。呼び出し元は `sql::owner_ddl` だけで、現在ロールの束縛（未束縛は `42501`）は
//! 呼び出し元がカタログ照会より前に確認済みである。
//!
//! 順序（ADR D10）: 自テナント内で対象を解決（不在・他テナントは同じ結果）→ 新所有者を解決（`42704`）→
//! 実行者の判定（`42501`）→ 書き込み。実行者は運用者権限（`operator`）または対象の現在の所有者。所有者が
//! 実行する場合は PostgreSQL と同じく (a) 現在ロールから新所有者へ SET ROLE できること、(b) 対象がテナント
//! SCHEMA に属するとき新所有者がその SCHEMA の CREATE を持つこと、を要求する。運用者は管理経路として
//! (a)(b) を免除する。
//!
//! ACL の書き換え（O10）: 旧所有者が grantor・grantee として載る行を新所有者へ置き換え、衝突は権限ビットの
//! OR で併合する。置き換えで grantee と grantor が同じ新所有者になる行は消す（所有者の権限は暗黙のため）。
//! 旧所有者の付与が取り消せないまま残る fail-open を作らない。

use std::collections::BTreeMap;

use redb::ReadableTable;

use crate::sql::allowlist::AlterOwnerKind;

use super::super::role::{
    get_role_by_id_in_txn, get_role_in_txn, role_can_set_in_txn, validate_tenant,
};
use super::super::{CatalogError, Storage};
use super::apply::{commit_with_bumps, relation_kind, AclChangeError, RelKind};
use super::owner::{is_owner_in_txn, owner_of_in_txn, set_owner_in_txn};
use super::schema::schema_privileges_in_txn;
use super::{
    list_acl_for_object_in_txn, remove_acl_in_txn, upsert_acl_in_txn, PrivilegeObject, ACL_TABLE,
    KIND_COLUMN, MAX_ACL_ENTRIES_PER_TENANT, PRIV_CREATE,
};

/// 新しい所有者の指定（解決は write txn の中で自テナントのカタログだけから行う）。
#[derive(Debug, Clone, Copy)]
pub(crate) enum NewOwnerRef<'a> {
    /// 利用者が書いたロール名。
    Name(&'a str),
    /// 認証層・セッションが保持するロール ID（`CURRENT_USER`・`SESSION_USER`）。
    Id(u64),
}

/// ACL の書き換え対象（relation は表と列の行、SCHEMA は SCHEMA の行）を集める。
fn acl_objects_of(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    object: &PrivilegeObject,
) -> Result<Vec<PrivilegeObject>, AclChangeError> {
    let mut out = vec![object.clone()];
    let PrivilegeObject::Table(key) = object else {
        return Ok(out);
    };
    let acl = match crate::storage::read_source::ReadSource::open_table(write_txn, ACL_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(out),
        Err(e) => return Err(CatalogError::from(e).into()),
    };
    let mut seen: Vec<String> = Vec::new();
    for entry in acl
        .range((tenant, KIND_COLUMN, key.as_str(), "", 0u64, 0u64)..)
        .map_err(CatalogError::from)?
    {
        let (k, _) = entry.map_err(CatalogError::from)?;
        let kv = k.value();
        if kv.0 != tenant || kv.1 != KIND_COLUMN || kv.2 != key {
            break;
        }
        if seen.len() >= MAX_ACL_ENTRIES_PER_TENANT {
            return Err(AclChangeError::LimitExceeded);
        }
        if !seen.iter().any(|c| c == kv.3) {
            seen.push(kv.3.to_string());
        }
    }
    for column in seen {
        out.push(PrivilegeObject::Column {
            table: key.clone(),
            column,
        });
    }
    Ok(out)
}

/// 対象 1 つの ACL を旧所有者から新所有者へ書き換える（併合・自己付与の削除は module doc の通り）。
fn rewrite_acl_for_object(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    object: &PrivilegeObject,
    old_owner: u64,
    new_owner: u64,
) -> Result<(), AclChangeError> {
    let entries = list_acl_for_object_in_txn(write_txn, tenant, object)?;
    if !entries
        .iter()
        .any(|e| e.grantee_id == old_owner || e.grantor_id == old_owner)
    {
        return Ok(());
    }
    // (grantee, grantor) -> (privileges, grant_options)
    let mut merged: BTreeMap<(u64, u64), (u16, u16)> = BTreeMap::new();
    for e in &entries {
        let grantee = if e.grantee_id == old_owner {
            new_owner
        } else {
            e.grantee_id
        };
        let grantor = if e.grantor_id == old_owner {
            new_owner
        } else {
            e.grantor_id
        };
        // 所有者が自分自身へ付けた行は暗黙の権限と重複するため残さない。
        if grantee == grantor {
            continue;
        }
        let slot = merged.entry((grantee, grantor)).or_insert((0, 0));
        slot.0 |= e.privileges;
        slot.1 |= e.grant_options;
    }
    for e in &entries {
        remove_acl_in_txn(write_txn, tenant, object, e.grantee_id, e.grantor_id)?;
    }
    for ((grantee, grantor), (privileges, grant_options)) in merged {
        upsert_acl_in_txn(
            write_txn,
            tenant,
            object,
            grantee,
            grantor,
            privileges,
            grant_options & privileges,
        )?;
    }
    Ok(())
}

impl Storage {
    /// 対象 `name` の所有者を `new_owner` へ変更する（module doc の順序・規則）。`actor` は現在ロール ID、
    /// `operator` は運用者権限（`--ddl-allowed-users`）の有無。所有者が変わらなければ何も書かない。
    /// 変更した relation は世代を進めて DDL として記録する（`USING PLAN` の世代照合の契約）。
    pub(crate) fn alter_object_owner(
        &self,
        tenant: &str,
        kind: AlterOwnerKind,
        name: &str,
        actor: u64,
        operator: bool,
        new_owner: NewOwnerRef<'_>,
    ) -> Result<(), AclChangeError> {
        validate_tenant(tenant)?;
        if actor == 0 {
            return Err(AclChangeError::PermissionDenied);
        }
        let write_txn = self
            .begin_write_txn()
            .map_err(crate::catalog::convert_storage_error)?;
        // 束縛ロールの現存（DROP 済みの古い束縛で変更しない）。
        if get_role_by_id_in_txn(&*write_txn, tenant, actor)?.is_none() {
            return Err(AclChangeError::PermissionDenied);
        }
        // 1. 対象（自テナント内で解決する。SCHEMA の不在・他テナントは同じ `SchemaNotFound`）。
        let (object, schema_of_relation, bump_key) = match kind {
            AlterOwnerKind::Schema => {
                let id = crate::catalog::namespace::schema_id_in_txn(&*write_txn, tenant, name)?
                    .ok_or_else(|| AclChangeError::SchemaNotFound(name.to_string()))?;
                (PrivilegeObject::Schema(id), None, None)
            }
            AlterOwnerKind::Table | AlterOwnerKind::View | AlterOwnerKind::MaterializedView => {
                let actual = relation_kind(&write_txn, tenant, name)?;
                let matches = match kind {
                    AlterOwnerKind::Table => true,
                    AlterOwnerKind::View => actual == RelKind::View,
                    AlterOwnerKind::MaterializedView => actual == RelKind::Matview,
                    AlterOwnerKind::Schema => false,
                };
                if !matches {
                    return Err(CatalogError::WrongObjectKind(name.to_string()).into());
                }
                let schema_id = match crate::catalog::namespace::RelationKey::parse(name) {
                    Ok(crate::catalog::namespace::RelationKey::InSchema { schema_id, .. }) => {
                        Some(schema_id)
                    }
                    _ => None,
                };
                (
                    PrivilegeObject::Table(name.to_string()),
                    schema_id,
                    Some(name.to_string()),
                )
            }
        };
        // 2. 新しい所有者（自テナントのカタログだけから解決。不在・他テナントは同じ `GranteeNotFound`）。
        let new_owner_id = match new_owner {
            NewOwnerRef::Name(n) => get_role_in_txn(&*write_txn, tenant, n)?
                .map(|r| r.id)
                .ok_or_else(|| AclChangeError::GranteeNotFound(n.to_string()))?,
            NewOwnerRef::Id(id) => {
                if get_role_by_id_in_txn(&*write_txn, tenant, id)?.is_none() {
                    return Err(AclChangeError::PermissionDenied);
                }
                id
            }
        };
        // 3. 実行者の判定。
        if !operator {
            if !is_owner_in_txn(&*write_txn, tenant, &object, actor)? {
                return Err(AclChangeError::PermissionDenied);
            }
            if !role_can_set_in_txn(&*write_txn, tenant, actor, new_owner_id)? {
                return Err(AclChangeError::PermissionDenied);
            }
            if let Some(schema_id) = schema_of_relation {
                let bits =
                    schema_privileges_in_txn(&*write_txn, tenant, schema_id, &[new_owner_id])?;
                if bits & PRIV_CREATE == 0 {
                    return Err(AclChangeError::PermissionDenied);
                }
            }
        }
        // 4. 書き込み。
        let old_owner = owner_of_in_txn(&*write_txn, tenant, &object)?;
        if old_owner == Some(new_owner_id) {
            return Ok(());
        }
        set_owner_in_txn(&write_txn, tenant, &object, new_owner_id)?;
        if let Some(old) = old_owner {
            for o in acl_objects_of(&write_txn, tenant, &object)? {
                rewrite_acl_for_object(&write_txn, tenant, &o, old, new_owner_id)?;
            }
        }
        let bumps: Vec<String> = bump_key.into_iter().collect();
        commit_with_bumps(write_txn, &bumps)
    }
}
