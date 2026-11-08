//! 論理ダンプの復元専用の指示の適用（RECOVER-16・Issue #3096。親モジュール: [`super`]。指示の文法:
//! [`super::restore_directive`]。設計メモ: `docs/design/logical-dump.md` 第 14 節）。
//!
//! 責務境界: wire-server の `dump-restore`（`EngineCore::apply_restore_directive` 経由）が、ダンプの
//! 指示枠 1 件を **単一の write txn・1 回の commit** でカタログへ反映する。名前の解決（ロール・SCHEMA・
//! relation・関数・SEQUENCE）は指定されたテナントの中だけで行い、不在は他テナントにだけ在る場合も含め
//! 区別なく [`RestoreDirectiveError::NotFound`] にする（存在の oracle にしない）。書き込みは既存の
//! 保存層の関数（形状・許可集合・上限の検証を持つ）だけを使い、SQL 表層の権限検査は通さない
//! （付与者・所有者・実体化印を元の状態のまま戻すため）。したがって本入口は復元 CLI 専用で、wire・HTTP
//! のセッションからは到達できない（信頼できる出典のダンプだけを復元する前提は設計メモに記す）。
//!
//! エラー文言は固定で、名前・パスワードハッシュ・テナントを含めない。

use std::fmt;

use super::super::function::list_functions_in_source;
use super::super::namespace::{schema_id_in_txn, schema_names_for_tenant_in_txn, RelationKey};
use super::super::privilege::default_acl::DefaultEntry;
use super::super::privilege::{
    mark_acl_initialized_in_txn, put_default_acl_init_in_txn, put_default_acl_row_in_txn,
    upsert_acl_in_txn, PrivilegeObject, PUBLIC_GRANTEE_ID,
};
use super::super::role::bootstrap::{put_marker, put_privileges, BootstrapPrivileges};
use super::super::role::{
    alter_role_in_txn, get_role_in_txn, put_membership_in_txn, validate_tenant, MembershipOptions,
};
use super::super::sequence::list_sequence_definitions_in;
use super::super::{CatalogError, Storage};
use super::access::function_signature;
use super::quote::{KeyScope, Namer};
use super::restore_directive::{parse, Directive, Obj, Rel};
use super::SchemaDdlError;

/// 指示の適用の失敗（固定文言。名前・ハッシュ・テナントを含まない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreDirectiveError {
    /// 指示文が文法に合わない。
    Malformed,
    /// 参照する名前（ロール・SCHEMA・relation・関数・SEQUENCE）が指定のテナントに無い。
    NotFound,
    /// 値が保存形式の契約に合わない・既に値がある。
    Rejected,
    /// 保存層の失敗。
    Storage,
}

impl RestoreDirectiveError {
    /// SQLSTATE 風の `wire_code`（復元 CLI の失敗表示用）。
    pub fn wire_code(self) -> &'static str {
        match self {
            Self::Malformed => "42601",
            Self::NotFound => "42704",
            Self::Rejected => "22023",
            Self::Storage => "XX000",
        }
    }
}

impl fmt::Display for RestoreDirectiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "malformed restore directive",
            Self::NotFound => "restore directive refers to an unknown object",
            Self::Rejected => "restore directive was rejected",
            Self::Storage => "restore directive could not be stored",
        })
    }
}

impl std::error::Error for RestoreDirectiveError {}

impl From<CatalogError> for RestoreDirectiveError {
    fn from(e: CatalogError) -> Self {
        match e {
            CatalogError::Invalid(_)
            | CatalogError::ViewLimitExceeded(_)
            | CatalogError::CorruptSchema(_) => Self::Rejected,
            _ => Self::Storage,
        }
    }
}

impl<E> From<SchemaDdlError<E>> for RestoreDirectiveError {
    fn from(_: SchemaDdlError<E>) -> Self {
        // 署名の描画に失敗する関数は、書き出しの時点で失敗しているはずの形（fail-closed で拒否する）。
        Self::Rejected
    }
}

type Res<T> = Result<T, RestoreDirectiveError>;

/// 解決の文脈（自テナントの SCHEMA 名の対応を 1 度だけ引く）。
struct Resolver<'a> {
    txn: &'a redb::WriteTransaction,
    tenant: &'a str,
}

impl Resolver<'_> {
    fn role_id(&self, name: &str) -> Res<u64> {
        match get_role_in_txn(self.txn, self.tenant, name) {
            Ok(Some(r)) => Ok(r.id),
            Ok(None) | Err(CatalogError::Invalid(_)) => Err(RestoreDirectiveError::NotFound),
            Err(e) => Err(e.into()),
        }
    }

    fn grantee_id(&self, g: &Option<String>) -> Res<u64> {
        match g {
            Some(n) => self.role_id(n),
            None => Ok(PUBLIC_GRANTEE_ID),
        }
    }

    fn schema_id(&self, name: &str) -> Res<u64> {
        match schema_id_in_txn(self.txn, self.tenant, name) {
            Ok(Some(id)) => Ok(id),
            Ok(None) => Err(RestoreDirectiveError::NotFound),
            Err(e) => Err(e.into()),
        }
    }

    /// relation のキー（表・ビューが現存するもの）。
    fn rel_key(&self, rel: &Rel) -> Res<String> {
        let key = match &rel.schema {
            None => RelationKey::Public(rel.name.clone()),
            Some(s) => RelationKey::InSchema {
                schema_id: self.schema_id(s)?,
                name: rel.name.clone(),
            },
        };
        let key = key.encode();
        // 形（識別子の検証）を通してから存在を確かめる。
        RelationKey::parse(&key).map_err(|_| RestoreDirectiveError::Rejected)?;
        let exists = super::super::table_exists_in_txn(self.txn, &key)?
            || super::super::view_exists_in_txn(self.txn, &key)?;
        if exists {
            Ok(key)
        } else {
            Err(RestoreDirectiveError::NotFound)
        }
    }

    fn function_id(&self, signature: &str) -> Res<u64> {
        let namer = Namer {
            schemas: schema_names_for_tenant_in_txn(self.txn, self.tenant)?,
        };
        let mut found = None;
        for def in list_functions_in_source(self.txn, self.tenant)? {
            if function_signature::<()>(&namer, &def)? == signature {
                if found.is_some() {
                    return Err(RestoreDirectiveError::Rejected);
                }
                found = Some(def.id);
            }
        }
        found.ok_or(RestoreDirectiveError::NotFound)
    }

    fn sequence_id(&self, name: &str) -> Res<u64> {
        let namer = Namer::default();
        for (key, def, _) in list_sequence_definitions_in(self.txn)? {
            if key == name && matches!(namer.scope::<()>(&key), Ok(KeyScope::Shared)) {
                return Ok(def.id);
            }
        }
        Err(RestoreDirectiveError::NotFound)
    }

    fn object(&self, obj: &Obj) -> Res<PrivilegeObject> {
        Ok(match obj {
            Obj::Table(r) => PrivilegeObject::Table(self.rel_key(r)?),
            Obj::Column(r, c) => PrivilegeObject::Column {
                table: self.rel_key(r)?,
                column: c.clone(),
            },
            Obj::Schema(s) => PrivilegeObject::Schema(self.schema_id(s)?),
            Obj::Sequence(s) => PrivilegeObject::Sequence(self.sequence_id(s)?),
            Obj::Function(sig) => PrivilegeObject::Function(self.function_id(sig)?),
        })
    }
}

/// 指示 1 件を `txn` の中で適用する（commit は呼び出し側）。
fn apply_in_txn(txn: &redb::WriteTransaction, tenant: &str, d: &Directive) -> Res<()> {
    let r = Resolver { txn, tenant };
    match d {
        Directive::RoleLegacy { role, phc } => {
            let current = match get_role_in_txn(txn, tenant, role) {
                Ok(Some(r)) => r,
                Ok(None) | Err(CatalogError::Invalid(_)) => {
                    return Err(RestoreDirectiveError::NotFound)
                }
                Err(e) => return Err(e.into()),
            };
            if !current.legacy_phc.is_empty() {
                return Err(RestoreDirectiveError::Rejected);
            }
            let bytes = phc.clone().into_bytes();
            alter_role_in_txn(txn, tenant, role, |def| def.legacy_phc = bytes)?;
        }
        Directive::RoleBootstrap {
            role,
            all_objects,
            ddl,
        } => put_privileges(
            txn,
            tenant,
            r.role_id(role)?,
            BootstrapPrivileges {
                all_object_privileges: *all_objects,
                ddl: *ddl,
            },
        )?,
        Directive::BootstrapMarker => put_marker(txn, tenant)?,
        Directive::Membership {
            role,
            member,
            grantor,
            root,
            admin,
            inherit,
            set,
        } => {
            let grantor_id = match grantor {
                Some(g) => Some(r.role_id(g)?),
                None => None,
            };
            put_membership_in_txn(
                txn,
                tenant,
                r.role_id(member)?,
                r.role_id(role)?,
                MembershipOptions {
                    admin: *admin,
                    inherit: *inherit,
                    set: *set,
                    grantor_id,
                    grantor_root: *root && grantor_id.is_some(),
                },
            )?;
        }
        Directive::Owner { obj, owner } => {
            let object = r.object(obj)?;
            super::super::privilege::owner::set_owner_in_txn(
                txn,
                tenant,
                &object,
                r.role_id(owner)?,
            )?;
        }
        Directive::Acl {
            obj,
            grantee,
            grantor,
            privileges,
            options,
        } => {
            let object = r.object(obj)?;
            upsert_acl_in_txn(
                txn,
                tenant,
                &object,
                r.grantee_id(grantee)?,
                r.role_id(grantor)?,
                *privileges,
                *options,
            )?;
        }
        Directive::AclInit { function } => {
            mark_acl_initialized_in_txn(txn, tenant, r.function_id(function)?)?;
        }
        Directive::DefaultAcl {
            role,
            schema,
            kind,
            grantee,
            privileges,
            options,
        } => {
            let schema_id = match schema {
                Some(s) => r.schema_id(s)?,
                None => 0,
            };
            put_default_acl_row_in_txn(
                txn,
                tenant,
                r.role_id(role)?,
                schema_id,
                *kind,
                &DefaultEntry {
                    grantee_id: r.grantee_id(grantee)?,
                    privileges: *privileges,
                    grant_options: *options,
                },
            )?;
        }
        Directive::DefaultAclInit { role } => {
            put_default_acl_init_in_txn(txn, tenant, r.role_id(role)?)?;
        }
    }
    Ok(())
}

impl Storage {
    /// 論理ダンプの指示文 1 件を `tenant` へ適用する（単一 write txn・1 回の commit。失敗したら何も書かない）。
    ///
    /// 呼び出し元は wire-server の `dump-restore`（空の一時 DB への復元）と結合試験だけ。SQL・NoSQL の
    /// 表層・認証済みセッションからは呼べない。
    pub fn apply_restore_directive(
        &self,
        tenant: &str,
        text: &str,
    ) -> Result<(), RestoreDirectiveError> {
        validate_tenant(tenant).map_err(|_| RestoreDirectiveError::Rejected)?;
        let directive = parse(text).map_err(|_| RestoreDirectiveError::Malformed)?;
        let txn = self
            .begin_write_txn()
            .map_err(|e| RestoreDirectiveError::from(super::super::convert_storage_error(e)))?;
        apply_in_txn(&txn, tenant, &directive)?;
        crate::recovery::commit_boundary::commit(txn)
            .map_err(|e| RestoreDirectiveError::from(super::super::convert_storage_error(e)))?;
        Ok(())
    }
}
