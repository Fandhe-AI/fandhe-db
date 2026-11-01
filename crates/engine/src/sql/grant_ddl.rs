//! `GRANT`／`REVOKE` の実行本体（SQL-40、Issue #1965。設計記録: `docs/design/role-privilege-model.md` §8）。
//!
//! 役割: 構造検証済みの文（`sql::allowlist::ValidatedGrantRevoke`）を権限カタログ（`catalog::privilege` の
//! `Storage::apply_acl_changes`）への書き込み要求へ写し、`AclChangeError` を SQLSTATE の契約へ写像する。
//! 呼び出し元は `core.rs` の `ParsedSql::GrantRevoke` 腕で、[`require_grant_principal`]（現在ロールの束縛。
//! 未束縛は対象・ロールの有無を問わず `42501`）と明示トランザクション内の `0A000` を通過した後にだけ来る。
//!
//! 付与者は現在ロール（`SessionState::current_role_id`）。付与できるのは付与者が持つ GRANT OPTION の範囲
//! だけで、DDL 権限ゲートを通過した主体は所有者の機構（#2236）までの代替として「所有者相当」に扱う。
//! ロール形（`GRANT <ロール> TO <ロール>`）の実行は #2232 の担当のため `0A000`。
//!
//! テナント境界（P0）: scope は呼び出し元が渡す検証済みの `tenant_id` だけ。他テナントのロール・スキーマは
//! 不在と同じ経路・同じ SQLSTATE になる。エラー文言は固定か利用者自身が書いた名前だけで、内部キー・
//! tenant_id・ACL の断片を含めない。PostgreSQL の警告（`01006`／`01007` 相当）は結果へ積むだけで wire へは
//! 出さない。

use crate::catalog::privilege::{
    AclChangeError, AclItem, AclRequest, AclScope, AclWarning, GranteeSpec,
};
use crate::catalog::CatalogError;
use crate::sql::allowlist::{
    GrantScope, GrantTarget, Grantee, SqlSurfaceError, ValidatedGrantRevoke,
};
use crate::sql::mode::SessionState;
use crate::storage::Storage;

/// `GRANT`／`REVOKE` の成功応答。wire はコマンドタグ `GRANT`／`REVOKE` を返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantRevokeOutcome {
    pub(crate) is_revoke: bool,
    pub(crate) warning: Option<AclWarning>,
}

impl GrantRevokeOutcome {
    /// `REVOKE` の応答なら真（wire のコマンドタグの選択に使う）。
    pub fn is_revoke(&self) -> bool {
        self.is_revoke
    }

    /// 付与・取り消しできる権限が 1 つも無かった（PostgreSQL の `01007` 相当の警告）か。
    pub fn had_no_privileges_warning(&self) -> bool {
        self.warning == Some(AclWarning::NoPrivileges)
    }

    /// 一部の権限だけが付与・取り消された（`01006` 相当の警告）か。
    pub fn had_partial_warning(&self) -> bool {
        self.warning == Some(AclWarning::NotAllPrivileges)
    }
}

/// 現在ロールが束縛されているかの唯一の判定点。未束縛（認証層が束縛していない現状の全セッション）は
/// カタログ照会・名前解決・`0A000` 判定のいずれよりも前に `42501`（fail-closed）へ落とす。主体の無い付与は
/// 記録できず、対象の有無を権限の無い主体に示さない（存在オラクルの防止）。
pub(crate) fn require_grant_principal(session: &SessionState) -> Result<u64, SqlSurfaceError> {
    crate::sql::privilege_gate::bound_principal(session)
}

/// `GRANT`／`REVOKE` を実行する（現在ロールの束縛・明示 Tx のゲートは呼び出し元で通過済み）。
pub(crate) fn execute_grant_revoke(
    storage: &Storage,
    tenant_id: &str,
    session: &SessionState,
    stmt: &ValidatedGrantRevoke,
) -> Result<GrantRevokeOutcome, SqlSurfaceError> {
    let grantor_id = require_grant_principal(session)?;
    let objects = match &stmt.target {
        GrantTarget::Roles(_) => {
            return Err(SqlSurfaceError::feature_not_supported(
                "GRANT/REVOKE of role membership is not supported yet",
            ));
        }
        GrantTarget::Objects(o) => o,
    };
    let mut grantees = Vec::with_capacity(objects.grantees.len());
    for g in &objects.grantees {
        grantees.push(match g {
            Grantee::Public => GranteeSpec::Public,
            Grantee::Role(n) => GranteeSpec::Name(n.clone()),
            Grantee::CurrentRole => GranteeSpec::Id(grantor_id),
            Grantee::SessionUser => match session.session_role_id() {
                Some(id) if id != 0 => GranteeSpec::Id(id),
                _ => return Err(SqlSurfaceError::InsufficientPrivilege),
            },
        });
    }
    let items: Vec<AclItem> = objects
        .items
        .iter()
        .map(|i| AclItem {
            bits: i.bits,
            columns: i.columns.clone(),
        })
        .collect();
    let req = AclRequest {
        is_grant: stmt.is_grant,
        grantor_id,
        owner_equivalent: crate::sql::privilege_gate::is_owner_equivalent(session),
        scope: match objects.scope {
            GrantScope::Tables => AclScope::Tables,
            GrantScope::Schemas => AclScope::Schemas,
            GrantScope::AllTablesInSchemas => AclScope::AllTablesInSchemas,
        },
        tables: &objects.tables,
        schemas: &objects.schemas,
        items: &items,
        grantees: &grantees,
        grant_option: objects.grant_option,
        cascade: objects.cascade,
    };
    let applied = storage
        .apply_acl_changes(tenant_id, &req)
        .map_err(map_acl_error)?;
    Ok(GrantRevokeOutcome {
        is_revoke: !stmt.is_grant,
        warning: applied.warning,
    })
}

/// 権限カタログの失敗を SQL 表層の契約へ写像する（付与先ロール不在 `42704`・表不在 `42P01`・SCHEMA 不在 `3F000`・種別違い
/// `42809`・列不在 `42703`・権限なし `42501`・依存する付与 `2BP01`・不正な付与 `0LP01`・未対応 `0A000`・
/// 件数上限 `54000`・ロック timeout `55P03`・その他 `XX000`）。文言は固定か利用者自身が書いた名前だけ。
pub(crate) fn map_acl_error(e: AclChangeError) -> SqlSurfaceError {
    use crate::sql::policy_ddl::user_facing_relation_name;
    match e {
        AclChangeError::GranteeNotFound(n) => SqlSurfaceError::undefined_role(n),
        AclChangeError::ColumnNotFound(c) => SqlSurfaceError::undefined_column(c),
        AclChangeError::SchemaNotFound(n) => SqlSurfaceError::invalid_schema_name(n),
        AclChangeError::PermissionDenied => SqlSurfaceError::InsufficientPrivilege,
        AclChangeError::DependentPrivileges => SqlSurfaceError::DependentObjectsStillExist {
            name: "privilege grant".to_string(),
        },
        AclChangeError::InvalidGrant(m) => SqlSurfaceError::invalid_grant_operation(m),
        AclChangeError::Unsupported(m) => SqlSurfaceError::feature_not_supported(m),
        AclChangeError::LimitExceeded => {
            SqlSurfaceError::payload_too_large("too many privilege targets")
        }
        AclChangeError::Catalog(c) => match c {
            CatalogError::TableNotFound(name) => SqlSurfaceError::UndefinedTable {
                name: user_facing_relation_name(&name),
            },
            CatalogError::WrongObjectKind(name) => SqlSurfaceError::WrongObjectType {
                name: user_facing_relation_name(&name),
            },
            CatalogError::ViewLimitExceeded(_) => {
                SqlSurfaceError::payload_too_large("too many privilege entries")
            }
            CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
            CatalogError::Invalid(_) => SqlSurfaceError::invalid_input("invalid privilege entry"),
            _ => SqlSurfaceError::Internal {
                detail: "privilege catalog operation failed".to_string(),
            },
        },
    }
}

#[cfg(test)]
mod tests;
