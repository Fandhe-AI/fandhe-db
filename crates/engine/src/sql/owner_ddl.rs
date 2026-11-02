//! `ALTER {TABLE|VIEW|MATERIALIZED VIEW|SCHEMA} <名> OWNER TO <ロール>` の実行本体（SQL-40・RLS-12 の
//! ポインタ、Issue #2236。設計記録: `docs/design/role-privilege-model.md` §14 の O8〜O10・O16）。
//!
//! 役割: 構造検証済みの文（`sql::allowlist::ValidatedAlterOwner`）を権限カタログの所有者変更
//! （`Storage::alter_object_owner`。解決・判定・書き込みを単一の write txn で行う）へ写し、`AclChangeError`
//! を SQLSTATE の契約へ写像する。呼び出し元は `core.rs` の `ParsedSql::AlterOwner` 腕で、権限ゲート
//! （`privilege_gate` の `AlterOwner`）と現在ロールの束縛・明示トランザクション内の `0A000` を通過した
//! 後にだけ来る。
//!
//! 順序の契約: 現在ロールの束縛（未束縛は対象・ロールの有無を問わず `42501`。カタログ照会より前）→
//! 自テナント内の対象の解決（不在 `42P01`／`3F000`・種別違い `42809`）→ 新所有者の解決（`42704`。
//! 他テナントにだけ在るロールは不在と同じ）→ 実行者の判定（運用者権限、または現在の所有者かつ新所有者へ
//! SET 可能。満たさなければ `42501`）。エラー文言は固定か利用者自身が書いた名前だけで、tenant_id・
//! ロール ID・所有者の情報を含めない。

use crate::catalog::privilege::NewOwnerRef;
use crate::sql::allowlist::{AlterOwnerKind, RoleSpec, SqlSurfaceError, ValidatedAlterOwner};
use crate::sql::mode::SessionState;
use crate::storage::Storage;

/// `ALTER ... OWNER TO` の成功応答。wire はコマンドタグ `ALTER <種別>` を返す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlterOwnerOutcome {
    pub(crate) kind: AlterOwnerKind,
}

impl AlterOwnerOutcome {
    /// 変更した対象の種別（wire のコマンドタグの選択に使う）。
    pub fn kind(&self) -> AlterOwnerKind {
        self.kind
    }
}

/// `OWNER TO` を実行する（権限ゲート・明示 Tx のゲートは呼び出し元で通過済み）。
pub(crate) fn execute_alter_owner(
    storage: &Storage,
    tenant_id: &str,
    session: &SessionState,
    stmt: &ValidatedAlterOwner,
) -> Result<AlterOwnerOutcome, SqlSurfaceError> {
    // 現在ロールの束縛（カタログ照会・名前解決より前。運用者権限があっても束縛が要る）。
    let actor = crate::sql::privilege_gate::bound_principal(session)?;
    let operator = crate::sql::privilege_gate::is_owner_equivalent(session);
    let new_owner = match &stmt.new_owner {
        RoleSpec::Name(n) => NewOwnerRef::Name(n),
        RoleSpec::CurrentRole => NewOwnerRef::Id(actor),
        RoleSpec::SessionUser => match session.session_role_id() {
            Some(id) if id != 0 => NewOwnerRef::Id(id),
            _ => return Err(SqlSurfaceError::InsufficientPrivilege),
        },
    };
    storage
        .alter_object_owner(tenant_id, stmt.kind, &stmt.name, actor, operator, new_owner)
        .map_err(crate::sql::grant_ddl::map_acl_error)?;
    Ok(AlterOwnerOutcome { kind: stmt.kind })
}

/// `CREATE SCHEMA` が記録する所有者のロール ID を決める。`AUTHORIZATION <ロール>` があればそのロール
/// （自テナントのカタログだけから解決。不在・他テナントは同じ `42704`。`CURRENT_USER`・`SESSION_USER` は
/// 束縛済みのロールで、未束縛は `42501`）。無ければ束縛中の現在ロール（未束縛は `None`）。DDL 権限ゲートは
/// 呼び出し元で通過済みで、運用者権限の主体だけが他のロールを指定できる。
pub(crate) fn resolve_schema_owner(
    storage: &Storage,
    tenant_id: &str,
    session: &SessionState,
    authorization: Option<&RoleSpec>,
) -> Result<Option<u64>, SqlSurfaceError> {
    match authorization {
        None => Ok(session.current_role_id().filter(|id| *id != 0)),
        Some(RoleSpec::CurrentRole) => {
            crate::sql::privilege_gate::bound_principal(session).map(Some)
        }
        Some(RoleSpec::SessionUser) => match session.session_role_id() {
            Some(id) if id != 0 => Ok(Some(id)),
            _ => Err(SqlSurfaceError::InsufficientPrivilege),
        },
        Some(RoleSpec::Name(name)) => match storage.get_role(tenant_id, name) {
            Ok(Some(role)) => Ok(Some(role.id)),
            Ok(None) => Err(SqlSurfaceError::undefined_role(name.clone())),
            Err(_) => Err(SqlSurfaceError::Internal {
                detail: "role catalog operation failed".to_string(),
            }),
        },
    }
}

#[cfg(test)]
mod tests;
