//! ロール形 `GRANT`／`REVOKE`（メンバーシップの付与・取り消し）の適用（SQL-39 (7)・SQL-40 のポインタ、
//! Issue #2232。設計記録: `docs/design/role-privilege-model.md` §10 の M1〜M4）。
//!
//! 役割: `sql::grant_ddl` が構造検証済みの文から作る [`MembershipRequest`] を、単一の write txn・1 回の commit で
//! メンバーシップ表へ原子的に反映する。循環（`0LP01`）・ADMIN の判定（`42501`）・付与者の記録・オプションの
//! 更新・`RESTRICT`／`CASCADE` を担う。グラフの探索は [`super::graph`] だけを呼ぶ（判定系を 2 つ作らない）。
//!
//! 判定順（決定的・fail-closed。すべて commit 前に返るため副作用ゼロ）: 実行ロールの現存確認 → 付与される
//! ロールの解決（`42704`。他テナントにだけ在るロールも不在と同じ）→ 付与先の解決（同上）→ 組ごとに
//! ADMIN の判定（`42501`）→ 循環の検査（`0LP01`）→ 書き込み。PostgreSQL の `AddRoleMems` と同じく権限の判定が
//! 循環の検査より先で、権限の無い主体にグラフの形を示さない。
//!
//! 所有者相当: DDL 権限ゲートを通過した主体（`owner_equivalent`）は、所有者の機構（#2236）までの代替として
//! 常に許可し、付与者を問わず取り消せる（PostgreSQL のスーパーユーザーの代替。ADR に逸脱として記録）。
//! 付与者ごとに別の行を持つ PostgreSQL 16 以降の形はキーの制約上再現できないため、別の付与者が付けた行への
//! 重複の付与は `Unsupported`（`0A000`）で拒否する。

use std::collections::HashSet;

use crate::catalog::{CatalogError, Storage};

use super::graph::{best_admin_grantor, is_admin_of, would_create_cycle};
use super::{
    get_membership, get_role_by_id_in_txn, get_role_in_txn, list_memberships_granted_by_in_txn,
    put_membership_in_txn, remove_membership_in_txn, validate_tenant, MembershipOptions, RoleDef,
};

/// 連鎖（CASCADE）で処理する行数の上限（実装既定値。超過は `LimitExceeded`）。
const MAX_CASCADE_STEPS: usize = 10_000;

/// ロール形のオプション名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MembershipOpt {
    Admin,
    Inherit,
    Set,
}

/// 付与先（メンバー）の指定。解決は書き込みと同じ txn の中で行う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemberSpec {
    /// `PUBLIC`（PostgreSQL と同じく `role "public" does not exist` 相当で拒否する）。
    Public,
    /// 解決済みのロール ID（`CURRENT_USER`・`SESSION_USER`。現存は txn 内で確かめる）。
    Id(u64),
    /// ロール名（自テナント内だけを引く）。
    Name(String),
}

/// [`Storage::apply_membership_changes`] への要求。
#[derive(Debug, Clone)]
pub(crate) struct MembershipRequest<'a> {
    pub(crate) is_grant: bool,
    /// 実行ロール（現在ロール）の ID。0 は不可。
    pub(crate) actor_id: u64,
    /// 所有者相当（DDL 権限ゲート通過済み）か。
    pub(crate) owner_equivalent: bool,
    pub(crate) roles: &'a [String],
    pub(crate) members: &'a [MemberSpec],
    /// `GRANT` の `WITH ...` で明示したオプション。
    pub(crate) with_options: &'a [(MembershipOpt, bool)],
    /// `REVOKE` の `<オプション> OPTION FOR`。
    pub(crate) option_for: Option<MembershipOpt>,
    pub(crate) cascade: bool,
}

/// 成功結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct MembershipOutcome {
    /// 書き込んだ（追加・更新・削除した）行数。
    pub(crate) rows_changed: usize,
}

/// 失敗の分類。`sql::grant_ddl` が SQLSTATE へ写す。文言に他テナントの情報・内部キーを含めない。
#[derive(Debug)]
pub(crate) enum MembershipChangeError {
    Catalog(CatalogError),
    /// ロールが自テナントに無い（他テナントにだけ在る場合を含む）。利用者が書いた名前。
    RoleNotFound(String),
    /// 実行ロールが未束縛・不在、または ADMIN が無い（`42501`）。
    PermissionDenied,
    /// RESTRICT で依存する付与が残る（`2BP01`）。
    DependentGrants,
    /// 循環（`0LP01`）。`(付与されるロール名, 付与先名)`。
    Circular(String, String),
    /// 未対応の形（`0A000`）。固定文言。
    Unsupported(&'static str),
    /// 連鎖数の上限超過（`54000`）。
    LimitExceeded,
}

impl From<CatalogError> for MembershipChangeError {
    fn from(e: CatalogError) -> Self {
        Self::Catalog(e)
    }
}

type Result<T> = std::result::Result<T, MembershipChangeError>;

/// ADMIN の判定。許可なら記録・照合に使う付与者（所有者相当は実行ロール自身）を返す。
fn authorize(
    w: &redb::WriteTransaction,
    tenant: &str,
    req: &MembershipRequest<'_>,
    role_id: u64,
) -> Result<u64> {
    if req.owner_equivalent {
        return Ok(req.actor_id);
    }
    best_admin_grantor(w, tenant, req.actor_id, role_id)?
        .ok_or(MembershipChangeError::PermissionDenied)
}

fn pick(opts: &[(MembershipOpt, bool)], which: MembershipOpt) -> Option<bool> {
    opts.iter().find(|(o, _)| *o == which).map(|(_, v)| *v)
}

struct Resolved {
    id: u64,
    def: RoleDef,
}

fn resolve_member(w: &redb::WriteTransaction, tenant: &str, spec: &MemberSpec) -> Result<Resolved> {
    match spec {
        MemberSpec::Public => Err(MembershipChangeError::RoleNotFound("public".to_string())),
        MemberSpec::Id(id) => get_role_by_id_in_txn(w, tenant, *id)?
            .map(|def| Resolved { id: *id, def })
            .ok_or(MembershipChangeError::PermissionDenied),
        MemberSpec::Name(n) => get_role_in_txn(w, tenant, n)?
            .map(|def| Resolved { id: def.id, def })
            .ok_or_else(|| MembershipChangeError::RoleNotFound(n.clone())),
    }
}

/// `member` が `role` へ持つ ADMIN を失った後に、`member` が付けた付与のうち ADMIN の根拠を失ったものを
/// 解決する（RESTRICT は `2BP01`、CASCADE は連鎖して削除）。`member` が別の経路で ADMIN を保てば何もしない。
fn resolve_dependents(
    w: &redb::WriteTransaction,
    tenant: &str,
    member: u64,
    role: u64,
    cascade: bool,
    steps: &mut usize,
    changed: &mut usize,
) -> Result<()> {
    let mut queue = vec![(member, role)];
    while let Some((x, r)) = queue.pop() {
        *steps += 1;
        if *steps > MAX_CASCADE_STEPS {
            return Err(MembershipChangeError::LimitExceeded);
        }
        if is_admin_of(w, tenant, x, r)? {
            continue;
        }
        for (m2, g2, o2) in list_memberships_granted_by_in_txn(w, tenant, x)? {
            if g2 != r {
                continue;
            }
            if !cascade {
                return Err(MembershipChangeError::DependentGrants);
            }
            remove_membership_in_txn(w, tenant, m2, g2)?;
            *changed += 1;
            if o2.admin {
                queue.push((m2, g2));
            }
        }
    }
    Ok(())
}

/// `CREATE ROLE ... IN ROLE / ROLE / ADMIN` の句（名前の列。空なら句なし）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct NewRoleMemberships<'a> {
    pub(crate) actor_id: u64,
    pub(crate) owner_equivalent: bool,
    /// 新しいロールをメンバーにするロール（`IN ROLE`）。
    pub(crate) in_roles: &'a [String],
    /// 新しいロールへメンバーとして加えるロール（`ROLE`）。
    pub(crate) member_roles: &'a [String],
    /// 新しいロールへ ADMIN 付きで加えるロール（`ADMIN`）。
    pub(crate) admin_roles: &'a [String],
}

impl Storage {
    /// ロールの作成と `IN ROLE`・`ROLE`・`ADMIN` の付与を、単一 write txn・1 回の commit で行う（Issue #2232）。
    /// どれか 1 つでも失敗したらロールの作成ごと commit しない。作成の失敗は `Catalog`（呼び出し側が
    /// `42710` などへ写す）、付与の失敗は [`MembershipChangeError`] の各分類で返る。
    pub(crate) fn create_role_with_memberships(
        &self,
        tenant: &str,
        def: &RoleDef,
        m: &NewRoleMemberships<'_>,
    ) -> Result<u64> {
        validate_tenant(tenant)?;
        let write_txn = self
            .begin_write_txn()
            .map_err(crate::catalog::convert_storage_error)?;
        let w: &redb::WriteTransaction = &write_txn;
        let id = super::create_role_in_txn(w, tenant, def)?;
        let me = [MemberSpec::Name(def.name.clone())];
        let me_role = [def.name.clone()];
        let base = MembershipRequest {
            is_grant: true,
            actor_id: m.actor_id,
            owner_equivalent: m.owner_equivalent,
            roles: &[],
            members: &[],
            with_options: &[],
            option_for: None,
            cascade: false,
        };
        // IN ROLE: 新しいロールを各ロールのメンバーにする。
        if !m.in_roles.is_empty() {
            let req = MembershipRequest {
                roles: m.in_roles,
                members: &me,
                ..base.clone()
            };
            apply_in_txn(w, tenant, &req)?;
        }
        // ROLE／ADMIN: 各ロールを新しいロールのメンバーにする（ADMIN は ADMIN 付き）。
        let admin_on = [(MembershipOpt::Admin, true)];
        for (names, with_options) in [(m.member_roles, &[][..]), (m.admin_roles, &admin_on[..])] {
            if names.is_empty() {
                continue;
            }
            let members: Vec<MemberSpec> =
                names.iter().map(|n| MemberSpec::Name(n.clone())).collect();
            let req = MembershipRequest {
                roles: &me_role,
                members: &members,
                with_options,
                ..base.clone()
            };
            apply_in_txn(w, tenant, &req)?;
        }
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(crate::catalog::convert_storage_error)?;
        Ok(id)
    }

    /// `tenant` のメンバーシップへロール形の `GRANT`／`REVOKE` を単一 write txn・1 回の commit で反映する。
    ///
    /// 呼び出し元は `sql::grant_ddl::execute_grant_revoke`（現在ロールの束縛・明示 Tx のゲートの後）と、
    /// `CREATE ROLE ... IN ROLE`（`sql::role_ddl`）。エラーはすべて commit 前に返る。変更が 0 件ならそのまま
    /// 破棄する。relation の世代は進めない（メンバーシップは表の計画に影響しない。#1961 と同じ）。
    pub(crate) fn apply_membership_changes(
        &self,
        tenant: &str,
        req: &MembershipRequest<'_>,
    ) -> Result<MembershipOutcome> {
        validate_tenant(tenant)?;
        if req.actor_id == 0 {
            return Err(MembershipChangeError::PermissionDenied);
        }
        let write_txn = self
            .begin_write_txn()
            .map_err(crate::catalog::convert_storage_error)?;
        let w: &redb::WriteTransaction = &write_txn;
        let rows = apply_in_txn(w, tenant, req)?;
        if rows == 0 {
            return Ok(MembershipOutcome::default());
        }
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(crate::catalog::convert_storage_error)?;
        Ok(MembershipOutcome { rows_changed: rows })
    }
}

/// 呼び出し側が持つ write txn の中で要求を適用する（`CREATE ROLE ... IN ROLE` が作成と同じ txn で呼ぶ）。
/// 変更した行数を返す。
pub(crate) fn apply_in_txn(
    w: &redb::WriteTransaction,
    tenant: &str,
    req: &MembershipRequest<'_>,
) -> Result<usize> {
    if req.actor_id == 0 || get_role_by_id_in_txn(w, tenant, req.actor_id)?.is_none() {
        return Err(MembershipChangeError::PermissionDenied);
    }
    let mut roles: Vec<Resolved> = Vec::new();
    let mut seen_roles: HashSet<u64> = HashSet::new();
    for n in req.roles {
        let def = get_role_in_txn(w, tenant, n)?
            .ok_or_else(|| MembershipChangeError::RoleNotFound(n.clone()))?;
        if seen_roles.insert(def.id) {
            roles.push(Resolved { id: def.id, def });
        }
    }
    let mut members: Vec<Resolved> = Vec::new();
    let mut seen_members: HashSet<u64> = HashSet::new();
    for m in req.members {
        let r = resolve_member(w, tenant, m)?;
        if seen_members.insert(r.id) {
            members.push(r);
        }
    }
    let mut changed = 0usize;
    let mut steps = 0usize;
    for role in &roles {
        // PostgreSQL と同じく、ADMIN の判定はロール単位で最初に行う。
        let grantor = authorize(w, tenant, req, role.id)?;
        for member in &members {
            if req.is_grant {
                if would_create_cycle(w, tenant, member.id, role.id)? {
                    return Err(MembershipChangeError::Circular(
                        role.def.name.clone(),
                        member.def.name.clone(),
                    ));
                }
                changed += grant_one(w, tenant, req, role, member, grantor)?;
            } else {
                changed += revoke_one(w, tenant, req, role, member, grantor, &mut steps)?;
            }
        }
    }
    Ok(changed)
}

fn grant_one(
    w: &redb::WriteTransaction,
    tenant: &str,
    req: &MembershipRequest<'_>,
    role: &Resolved,
    member: &Resolved,
    grantor: u64,
) -> Result<usize> {
    match get_membership(w, tenant, member.id, role.id)? {
        None => {
            // 既定値: ADMIN は偽、INHERIT は付与時点の付与先ロールの属性、SET は真。
            let opts = MembershipOptions {
                admin: pick(req.with_options, MembershipOpt::Admin).unwrap_or(false),
                inherit: pick(req.with_options, MembershipOpt::Inherit)
                    .unwrap_or(member.def.attributes.inherit),
                set: pick(req.with_options, MembershipOpt::Set).unwrap_or(true),
                grantor_id: Some(grantor),
            };
            put_membership_in_txn(w, tenant, member.id, role.id, opts)?;
            Ok(1)
        }
        Some(existing) => {
            let same_grantor = existing.grantor_id == Some(grantor);
            if !req.owner_equivalent && !same_grantor {
                return Err(MembershipChangeError::Unsupported(
                    "membership already granted by another grantor is not supported",
                ));
            }
            // 明示したオプションだけを更新する。明示が無ければ何も変えない（PostgreSQL は警告のみ）。
            let mut updated = existing;
            if let Some(v) = pick(req.with_options, MembershipOpt::Admin) {
                updated.admin = v;
            }
            if let Some(v) = pick(req.with_options, MembershipOpt::Inherit) {
                updated.inherit = v;
            }
            if let Some(v) = pick(req.with_options, MembershipOpt::Set) {
                updated.set = v;
            }
            if updated == existing {
                return Ok(0);
            }
            put_membership_in_txn(w, tenant, member.id, role.id, updated)?;
            Ok(1)
        }
    }
}

fn revoke_one(
    w: &redb::WriteTransaction,
    tenant: &str,
    req: &MembershipRequest<'_>,
    role: &Resolved,
    member: &Resolved,
    grantor: u64,
    steps: &mut usize,
) -> Result<usize> {
    let Some(existing) = get_membership(w, tenant, member.id, role.id)? else {
        return Ok(0);
    };
    // 所有者相当は付与者を問わず取り消せる。そうでなければ付与者が一致する行だけ（不一致は警告相当で何もしない）。
    if !req.owner_equivalent && existing.grantor_id != Some(grantor) {
        return Ok(0);
    }
    let mut changed = 0usize;
    let mut lost_admin = false;
    match req.option_for {
        None => {
            remove_membership_in_txn(w, tenant, member.id, role.id)?;
            changed += 1;
            lost_admin = existing.admin;
        }
        Some(opt) => {
            let mut updated = existing;
            match opt {
                MembershipOpt::Admin => {
                    updated.admin = false;
                    lost_admin = existing.admin;
                }
                MembershipOpt::Inherit => updated.inherit = false,
                MembershipOpt::Set => updated.set = false,
            }
            if updated != existing {
                put_membership_in_txn(w, tenant, member.id, role.id, updated)?;
                changed += 1;
            }
        }
    }
    if lost_admin {
        resolve_dependents(
            w,
            tenant,
            member.id,
            role.id,
            req.cascade,
            steps,
            &mut changed,
        )?;
    }
    // INHERIT を失う（または行ごと消える）ことによる、ADMIN の到達性の変化は追跡しない。依存の解決は ADMIN の
    // 根拠である直接の行に限る（到達性の再評価は後続で扱う。ADR §10 の対象外）。
    Ok(changed)
}
