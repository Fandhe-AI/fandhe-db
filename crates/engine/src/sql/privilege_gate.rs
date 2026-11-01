//! 権限検査の共通ゲート（SQL-40・SQL-23・RLS-12 ポインタ。設計は
//! `docs/design/role-privilege-model.md` の D4・D8・D10、経路一覧は
//! `docs/design/privilege-check-paths.md`）。
//!
//! 権限判定の唯一の判定点。`(主体, 対象, 必要な権限)` の形で判定し、現時点の対象は DDL 文種のみ。
//! 呼び出し元は `core.rs` の DDL 腕（[`require_ddl`]）、`sql::ddl::require_ddl_permission`
//! （[`require_server_ddl`] への薄いファサード）、`sql::search_path`（参照ゲートの省略条件）、
//! `sql::grant_ddl`（[`bound_principal`]・[`is_owner_equivalent`]）。
//!
//! 契約:
//! - DDL の可否は従来どおり `SessionState::ddl_allowed`（`--ddl-allowed-users` 由来）だけで決める。
//!   現在ロール・ACL・所有者は判定に混ぜない（ロールの束縛は DDL 権限を広げない）。
//! - `&Storage`・テナント ID を取らない。カタログ照会より前に判定できることを型で保証する
//!   （権限の無い主体に対象の有無を示さない存在オラクルの防止）。
//! - 拒否は固定文言の [`SqlSurfaceError::InsufficientPrivilege`]（`42501`）。ロール ID・文種名は含めない。
//! - 主体は認証層が 1 回だけ設定した値の写しで、SQL 文から書き換える経路は持たない。
//! - RLS の `PolicyContext` には触れない（権限判定は RLS より前段の追加の制限）。

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::mode::SessionState;

/// 検査主体。[`SessionState`] から導出する写しで、コンストラクタは [`Principal::from_session`] のみ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Principal {
    /// `--ddl-allowed-users` 由来の運用者権限（認証層が `allow_ddl` で 1 回だけ立てる）。
    ddl_allowed: bool,
    /// 現在ロール ID（未束縛は `None`。`0` は PUBLIC の予約値で未束縛と同一視する）。
    current_role_id: Option<u64>,
}

impl Principal {
    /// セッションの認証済み状態から主体を導出する。
    pub(crate) fn from_session(session: &SessionState) -> Self {
        Self {
            ddl_allowed: session.ddl_allowed(),
            current_role_id: session.current_role_id(),
        }
    }
}

/// ゲートで判定する DDL 文種。`core.rs` の DDL 腕と 1 対 1 に対応する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DdlStatementKind {
    CreateTable,
    CreateTableAs,
    DropTable,
    AlterTable,
    CreateView,
    CreateMaterializedView,
    RefreshMaterializedView,
    DropView,
    DropMaterializedView,
    CreateIndex,
    DropIndex,
    CreateType,
    DropType,
    CreateSequence,
    DropSequence,
    AlterSequence,
    CreateTrigger,
    DropTrigger,
    CreatePolicy,
    AlterPolicy,
    DropPolicy,
    CreateRole,
    DropRole,
    CreateSchema,
    DropSchema,
    AlterSchema,
    CreateFunction,
    DropFunction,
}

impl DdlStatementKind {
    /// 全文種（表の全域性をテストで確認するための列挙）。
    #[cfg(test)]
    pub(crate) const ALL: &'static [DdlStatementKind] = &[
        Self::CreateTable,
        Self::CreateTableAs,
        Self::DropTable,
        Self::AlterTable,
        Self::CreateView,
        Self::CreateMaterializedView,
        Self::RefreshMaterializedView,
        Self::DropView,
        Self::DropMaterializedView,
        Self::CreateIndex,
        Self::DropIndex,
        Self::CreateType,
        Self::DropType,
        Self::CreateSequence,
        Self::DropSequence,
        Self::AlterSequence,
        Self::CreateTrigger,
        Self::DropTrigger,
        Self::CreatePolicy,
        Self::AlterPolicy,
        Self::DropPolicy,
        Self::CreateRole,
        Self::DropRole,
        Self::CreateSchema,
        Self::DropSchema,
        Self::AlterSchema,
        Self::CreateFunction,
        Self::DropFunction,
    ];

    /// 文種と必要権限の表。ワイルドカード腕を使わない網羅 `match` で、文種の追加時に表の更新を強制する。
    /// 現時点はすべて運用者権限。オブジェクトごとの所有者・SCHEMA の CREATE・CREATEROLE への移行は
    /// 後続 Issue（#2236・#2234・#1971）で、各腕をその権限へ差し替える。
    pub(crate) fn required_privilege(self) -> RequiredPrivilege {
        match self {
            Self::CreateTable
            | Self::CreateTableAs
            | Self::DropTable
            | Self::AlterTable
            | Self::CreateView
            | Self::CreateMaterializedView
            | Self::RefreshMaterializedView
            | Self::DropView
            | Self::DropMaterializedView
            | Self::CreateIndex
            | Self::DropIndex
            | Self::CreateType
            | Self::DropType
            | Self::CreateSequence
            | Self::DropSequence
            | Self::AlterSequence
            | Self::CreateTrigger
            | Self::DropTrigger
            | Self::CreatePolicy
            | Self::AlterPolicy
            | Self::DropPolicy
            | Self::CreateRole
            | Self::DropRole
            | Self::CreateSchema
            | Self::DropSchema
            | Self::AlterSchema
            | Self::CreateFunction
            | Self::DropFunction => RequiredPrivilege::ServerDdl,
        }
    }
}

/// 判定に必要な権限。後続の #1967〜#1969 がオブジェクト権限（`PRIV_*` ビット）の variant を足す拡張点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequiredPrivilege {
    /// 全テナント共有カタログを変更する運用者権限（`--ddl-allowed-users`）。
    ServerDdl,
}

/// 判定の対象。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PrivilegeTarget {
    /// DDL 文種。
    Ddl(DdlStatementKind),
}

/// 権限判定の唯一の関数。権限不足は固定文言の `42501`（fail-closed）。
/// `RequiredPrivilege` に variant を足したときは、未実装の判定を `Ok` に倒さず拒否側にする
/// （ワイルドカード腕を置かない）。
pub(crate) fn check(principal: &Principal, target: PrivilegeTarget) -> Result<(), SqlSurfaceError> {
    let required = match target {
        PrivilegeTarget::Ddl(kind) => kind.required_privilege(),
    };
    check_required(principal, required)
}

fn check_required(
    principal: &Principal,
    required: RequiredPrivilege,
) -> Result<(), SqlSurfaceError> {
    match required {
        RequiredPrivilege::ServerDdl => {
            if principal.ddl_allowed {
                Ok(())
            } else {
                Err(SqlSurfaceError::InsufficientPrivilege)
            }
        }
    }
}

/// `core.rs` の DDL 腕から呼ぶ。文種付きで [`check`] を経由する。
pub(crate) fn require_ddl(
    session: &SessionState,
    kind: DdlStatementKind,
) -> Result<(), SqlSurfaceError> {
    check(
        &Principal::from_session(session),
        PrivilegeTarget::Ddl(kind),
    )
}

/// 文種を特定しない呼び出し（`require_ddl_permission` のファサード・束ねた腕）用。
/// 必要権限 `ServerDdl` を直接判定し、結果は [`require_ddl`] と常に一致する。
pub(crate) fn require_server_ddl(session: &SessionState) -> Result<(), SqlSurfaceError> {
    check_required(
        &Principal::from_session(session),
        RequiredPrivilege::ServerDdl,
    )
}

/// 現在ロールの束縛判定。未束縛と PUBLIC の予約 ID `0` は `42501`（カタログ照会・`0A000` より前）。
pub(crate) fn bound_principal(session: &SessionState) -> Result<u64, SqlSurfaceError> {
    match Principal::from_session(session).current_role_id {
        Some(id) if id != 0 => Ok(id),
        _ => Err(SqlSurfaceError::InsufficientPrivilege),
    }
}

/// 所有者の機構（#2236）ができるまでの代替判定。運用者権限の有無と一致し、ロールの束縛に影響されない。
pub(crate) fn is_owner_equivalent(session: &SessionState) -> bool {
    require_server_ddl(session).is_ok()
}

#[cfg(test)]
mod tests;
