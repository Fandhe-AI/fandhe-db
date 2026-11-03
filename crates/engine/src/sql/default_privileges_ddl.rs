//! `ALTER DEFAULT PRIVILEGES` の実行本体（SQL-40・RLS-12 のポインタ、Issue #2237。設計記録:
//! `docs/design/role-privilege-model.md` §19）。
//!
//! 役割: 構造検証済みの文（`sql::allowlist::ValidatedAlterDefaultPrivileges`）を既定権限カタログ
//! （`catalog::privilege::default_acl` の `Storage::apply_default_acl_changes`。解決・認可・書き込みを単一の
//! write txn で行う）への要求へ写し、`AclChangeError` を SQLSTATE の契約へ写像する（写像は `GRANT`／`REVOKE`
//! と共有の [`crate::sql::grant_ddl::map_acl_error`]）。呼び出し元は `core.rs` の
//! `ParsedSql::AlterDefaultPrivileges` 腕で、現在ロールの束縛（未束縛は対象・ロールの有無を問わず `42501`）と
//! 明示トランザクション内の `0A000` を通過した後にだけ来る。DDL 権限ゲートは置かない（`GRANT` と同じ。
//! `FOR ROLE` の認可は対象ロールへの SET 経路で行い、運用者権限は免除）。
//!
//! テナント境界（P0）: scope は呼び出し元が渡す検証済みの `tenant_id` だけ。他テナントのロールは不在と同じ
//! `42704`、他テナントの SCHEMA は不在と同じ `3F000`。エラー文言は固定か利用者自身が書いた名前だけで、
//! 内部キー・tenant_id・ACL の断片を含めない。

use crate::catalog::privilege::default_acl::{DefaultAclKind, DefaultAclRequest};
use crate::catalog::privilege::GranteeSpec;
use crate::sql::allowlist::{
    DefaultAclTarget, Grantee, SqlSurfaceError, ValidatedAlterDefaultPrivileges,
};
use crate::sql::mode::SessionState;
use crate::storage::Storage;

/// `ALTER DEFAULT PRIVILEGES` の成功応答。wire はコマンドタグ `ALTER DEFAULT PRIVILEGES` を返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlterDefaultPrivilegesOutcome {
    pub(crate) is_revoke: bool,
}

impl AlterDefaultPrivilegesOutcome {
    /// `REVOKE` の応答なら真（API 利用者向け。wire のコマンドタグは変わらない）。
    pub fn is_revoke(&self) -> bool {
        self.is_revoke
    }
}

/// `ALTER DEFAULT PRIVILEGES` を実行する（現在ロールの束縛・明示 Tx のゲートは呼び出し元で通過済み）。
pub(crate) fn execute_alter_default_privileges(
    storage: &Storage,
    tenant_id: &str,
    session: &SessionState,
    stmt: &ValidatedAlterDefaultPrivileges,
) -> Result<AlterDefaultPrivilegesOutcome, SqlSurfaceError> {
    let actor_id = crate::sql::grant_ddl::require_grant_principal(session)?;
    let mut grantees = Vec::with_capacity(stmt.grantees.len());
    for g in &stmt.grantees {
        grantees.push(match g {
            Grantee::Public => GranteeSpec::Public,
            Grantee::Role(n) => GranteeSpec::Name(n.clone()),
            Grantee::CurrentRole => GranteeSpec::Id(actor_id),
            Grantee::SessionUser => match session.session_role_id() {
                Some(id) if id != 0 => GranteeSpec::Id(id),
                _ => return Err(SqlSurfaceError::InsufficientPrivilege),
            },
        });
    }
    let req = DefaultAclRequest {
        is_grant: stmt.is_grant,
        actor_id,
        // 運用者判定は DDL 権限のみ（初期ロールの全権限では FOR ROLE の SET 経路検査を免除しない。Issue #2238）。
        operator: session.ddl_allowed(),
        for_roles: &stmt.roles,
        schemas: &stmt.schemas,
        kind: match stmt.target {
            DefaultAclTarget::Tables => DefaultAclKind::Tables,
            DefaultAclTarget::Sequences => DefaultAclKind::Sequences,
            DefaultAclTarget::Functions => DefaultAclKind::Functions,
            DefaultAclTarget::Schemas => DefaultAclKind::Schemas,
        },
        privileges: stmt.privileges,
        grantees: &grantees,
        grant_option: stmt.grant_option,
    };
    storage
        .apply_default_acl_changes(tenant_id, &req)
        .map_err(crate::sql::grant_ddl::map_acl_error)?;
    Ok(AlterDefaultPrivilegesOutcome {
        is_revoke: !stmt.is_grant,
    })
}
