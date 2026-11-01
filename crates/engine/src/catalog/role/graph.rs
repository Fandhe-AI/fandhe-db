//! ロールのメンバーシップ・グラフの探索（SQL-39 (7) のポインタ、Issue #2232。設計記録:
//! `docs/design/role-privilege-model.md` §10 の M3・M5）。
//!
//! 役割: メンバーシップ表（`super::ROLE_MEMBERSHIPS_TABLE`）を辿る**唯一の探索関数**
//! [`effective_roles_in_txn`] と、その上の薄い判定（[`is_member_of`]・[`has_privs_of`]・[`can_set_role`]・
//! [`is_admin_of`]・[`best_admin_grantor`]・[`would_create_cycle`]）を提供する。判定系を 2 つ作らないため、
//! `SET ROLE` の認可（#1963）・権限検査の共通ゲート（#1966〜#1969）・ポリシーの `TO` ロール（#2278 段 2）・
//! ACL の付与権限（`catalog::privilege::apply`）は、いずれもここだけを呼ぶ契約にする。
//!
//! 意味論は PostgreSQL 17 の公開挙動に揃える。権限の継承は `INHERIT` が真の辺だけ、`SET ROLE` の可否は `SET`
//! が真の辺だけ、循環の検出と `pg_has_role(... 'MEMBER')` はオプションを問わない全辺を辿る。
//!
//! 有界性・fail-closed: 訪問するロールは [`MAX_ROLES_PER_TENANT`] まで、読む行は [`MAX_MEMBERSHIP_SCAN`] まで。
//! 超過・破損行は `Err` で返し、判定関数が偽や許可へ倒れない。テナント接頭辞のキーだけを引くため、他テナントの
//! 行は構造上辿れない。

use std::collections::HashSet;

use redb::ReadableDatabase;

use crate::catalog::{CatalogError, Result};
use crate::storage::read_source::ReadSource;

use super::{
    list_memberships_limited, validate_tenant, MembershipOptions, MAX_MEMBERSHIP_SCAN,
    MAX_ROLES_PER_TENANT, ROLE_LIMIT_MESSAGE,
};

/// どの辺を辿るか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// `INHERIT` が真の辺だけ（権限の継承）。
    Inherit,
    /// `SET` が真の辺だけ（`SET ROLE` の可否）。
    Set,
    /// すべての辺（循環の検出・`MEMBER`）。
    Any,
}

impl Mode {
    fn follows(self, o: &MembershipOptions) -> bool {
        match self {
            Mode::Inherit => o.inherit,
            Mode::Set => o.set,
            Mode::Any => true,
        }
    }
}

fn limit_error() -> CatalogError {
    CatalogError::ViewLimitExceeded(ROLE_LIMIT_MESSAGE.to_string())
}

/// `start_role_id` から `mode` の辺で到達できるロールを、`(ロール ID, ホップ数)` の幅優先順で返す（先頭は
/// `start_role_id` 自身でホップ 0。同じホップ内は付与先 ID の昇順で決定的）。訪問数・走査行数の上限超過と
/// 破損行は `Err`（fail-closed）。
pub(crate) fn effective_roles_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    start_role_id: u64,
    mode: Mode,
) -> Result<Vec<(u64, u32)>> {
    validate_tenant(tenant)?;
    if start_role_id == 0 {
        return Err(CatalogError::Invalid("invalid role membership".to_string()));
    }
    let mut order: Vec<(u64, u32)> = vec![(start_role_id, 0)];
    let mut seen: HashSet<u64> = HashSet::from([start_role_id]);
    let mut rows = 0usize;
    let mut cursor = 0usize;
    while let Some(&(node, hops)) = order.get(cursor) {
        cursor += 1;
        let edges = list_memberships_limited(src, tenant, node, MAX_ROLES_PER_TENANT)?;
        rows = rows.saturating_add(edges.len());
        if rows > MAX_MEMBERSHIP_SCAN {
            return Err(limit_error());
        }
        for (granted, opts) in edges {
            if !mode.follows(&opts) || !seen.insert(granted) {
                continue;
            }
            if order.len() >= MAX_ROLES_PER_TENANT {
                return Err(limit_error());
            }
            order.push((granted, hops.saturating_add(1)));
        }
    }
    Ok(order)
}

fn reaches<R: ReadSource>(
    src: &R,
    tenant: &str,
    from: u64,
    target: u64,
    mode: Mode,
) -> Result<bool> {
    Ok(effective_roles_in_txn(src, tenant, from, mode)?
        .iter()
        .any(|(r, _)| *r == target))
}

/// `role_id` が `target_id` のメンバーか（オプションを問わない。自分自身は真。`pg_has_role(... 'MEMBER')`）。
pub(crate) fn is_member_of<R: ReadSource>(
    src: &R,
    tenant: &str,
    role_id: u64,
    target_id: u64,
) -> Result<bool> {
    reaches(src, tenant, role_id, target_id, Mode::Any)
}

/// `role_id` が `target_id` の権限を継承するか（`INHERIT` の辺だけ。自分自身は真。`'USAGE'`）。
pub(crate) fn has_privs_of<R: ReadSource>(
    src: &R,
    tenant: &str,
    role_id: u64,
    target_id: u64,
) -> Result<bool> {
    reaches(src, tenant, role_id, target_id, Mode::Inherit)
}

/// `role_id` が `SET ROLE target_id` できるか（`SET` の辺だけ。自分自身は真。`'SET'`）。
pub(crate) fn can_set_role<R: ReadSource>(
    src: &R,
    tenant: &str,
    role_id: u64,
    target_id: u64,
) -> Result<bool> {
    reaches(src, tenant, role_id, target_id, Mode::Set)
}

/// `role_id` の継承閉包（自分自身を含む）の中で、`target_id` への直接のメンバーシップで `ADMIN` が真の行を
/// 持つ最も近いロールを返す（同点はホップ順・ID 昇順）。PostgreSQL の `select_best_admin` 相当。
pub(crate) fn best_admin_grantor<R: ReadSource>(
    src: &R,
    tenant: &str,
    role_id: u64,
    target_id: u64,
) -> Result<Option<u64>> {
    if role_id == target_id {
        return Ok(None);
    }
    for (node, _) in effective_roles_in_txn(src, tenant, role_id, Mode::Inherit)? {
        if node == target_id {
            continue;
        }
        if let Some(o) = super::get_membership(src, tenant, node, target_id)? {
            if o.admin {
                return Ok(Some(node));
            }
        }
    }
    Ok(None)
}

/// `role_id` が `target_id` のメンバーシップを付与・取り消せるか（ADMIN。ロールは自分自身の ADMIN を持たない）。
/// 所有者相当の主体の許可は呼び出し側（DDL ゲート）が先に判断する。
pub(crate) fn is_admin_of<R: ReadSource>(
    src: &R,
    tenant: &str,
    role_id: u64,
    target_id: u64,
) -> Result<bool> {
    Ok(best_admin_grantor(src, tenant, role_id, target_id)?.is_some())
}

/// `granted_id` を `member_id` へ付与すると循環になるか（自分自身・または `granted_id` から `member_id` へ
/// オプションを問わず到達できる場合）。`0LP01` の判定。
pub(crate) fn would_create_cycle<R: ReadSource>(
    src: &R,
    tenant: &str,
    member_id: u64,
    granted_id: u64,
) -> Result<bool> {
    if member_id == granted_id {
        return Ok(true);
    }
    reaches(src, tenant, granted_id, member_id, Mode::Any)
}
/// `pg_has_role` の権限文字列の 1 語。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RolePriv {
    Member,
    Usage,
    Set,
    /// `... WITH ADMIN OPTION`／`... WITH GRANT OPTION`（どちらも ADMIN の判定に帰着する）。
    Admin,
}

/// `pg_has_role` の失敗。
#[derive(Debug)]
pub(crate) enum HasRoleError {
    /// ロールが自テナントに無い（他テナントにだけ在る場合を含む）。利用者が書いた名前。
    RoleNotFound(String),
    /// 権限文字列に未知の語がある（`22023`）。利用者が書いた語。
    InvalidPrivilege(String),
    Catalog(CatalogError),
}

impl From<CatalogError> for HasRoleError {
    fn from(e: CatalogError) -> Self {
        Self::Catalog(e)
    }
}

/// カンマ区切り・大文字小文字無視・前後の空白無視の権限文字列を許可リストで解釈する。
fn parse_role_privileges(privs: &str) -> std::result::Result<Vec<RolePriv>, HasRoleError> {
    let mut out = Vec::new();
    for raw in privs.split(',') {
        let norm = raw
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_ascii_uppercase();
        let p = match norm.as_str() {
            "MEMBER" => RolePriv::Member,
            "USAGE" => RolePriv::Usage,
            "SET" => RolePriv::Set,
            "MEMBER WITH ADMIN OPTION"
            | "USAGE WITH ADMIN OPTION"
            | "SET WITH ADMIN OPTION"
            | "MEMBER WITH GRANT OPTION"
            | "USAGE WITH GRANT OPTION"
            | "SET WITH GRANT OPTION" => RolePriv::Admin,
            _ => {
                let shown: String = raw.trim().chars().take(64).collect();
                return Err(HasRoleError::InvalidPrivilege(shown));
            }
        };
        out.push(p);
    }
    Ok(out)
}

/// `pg_has_role(user, role, privs)` の評価（ID 解決済み）。権限のどれか 1 つでも満たせば真（PostgreSQL と同じ OR）。
/// 未知の語は、先に真になった語があっても必ず `InvalidPrivilege`（文字列全体を先に検証する）。
pub(crate) fn has_role_privilege<R: ReadSource>(
    src: &R,
    tenant: &str,
    user_id: u64,
    role_id: u64,
    privs: &str,
) -> std::result::Result<bool, HasRoleError> {
    let parsed = parse_role_privileges(privs)?;
    let mut result = false;
    for p in parsed {
        result |= match p {
            RolePriv::Member => is_member_of(src, tenant, user_id, role_id)?,
            RolePriv::Usage => has_privs_of(src, tenant, user_id, role_id)?,
            RolePriv::Set => can_set_role(src, tenant, user_id, role_id)?,
            RolePriv::Admin => is_admin_of(src, tenant, user_id, role_id)?,
        };
    }
    Ok(result)
}

impl crate::catalog::Storage {
    /// `pg_has_role` を名前で評価する（読み取り専用の 1 スナップショット）。`user` が `None` なら
    /// `current_user_id`（束縛された現在ロール。未束縛の判断は呼び出し側）を使う。ロール名はどちらも自テナント内
    /// だけを引き、他テナントにだけ在る名前も不在と同じ `RoleNotFound` にする（存在オラクルの防止）。
    pub(crate) fn has_role_privilege_by_name(
        &self,
        tenant: &str,
        user: Option<&str>,
        current_user_id: u64,
        role: &str,
        privs: &str,
    ) -> std::result::Result<bool, HasRoleError> {
        let rt = self.db().begin_read().map_err(CatalogError::from)?;
        let user_id = match user {
            Some(n) => {
                super::get_role_in_txn(&rt, tenant, n)?
                    .ok_or_else(|| HasRoleError::RoleNotFound(n.to_string()))?
                    .id
            }
            None => current_user_id,
        };
        let role_id = super::get_role_in_txn(&rt, tenant, role)?
            .ok_or_else(|| HasRoleError::RoleNotFound(role.to_string()))?
            .id;
        has_role_privilege(&rt, tenant, user_id, role_id, privs)
    }
}
