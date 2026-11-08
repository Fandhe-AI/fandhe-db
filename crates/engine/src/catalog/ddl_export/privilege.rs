//! ロール・権限の状態の書き出し（RECOVER-16・Issue #3096。親モジュール: [`super`]。ポインタ:
//! SQL-39・SQL-40・RLS-12。設計メモ: `docs/design/logical-dump.md` 第 14 節）。
//!
//! 責務境界: 選択したテナントのメンバーシップ・所有者・ACL（表・列・SCHEMA・SEQUENCE・関数）・関数 ACL の
//! 実体化印・既定権限・初期権限・旧形式ハッシュ・ブートストラップ済みマーカーを、復元専用の指示文
//! （[`super::restore_directive`]）として `PostData` の末尾に渡す。`GRANT` 文にしない理由（付与者・
//! 所有者・実体化印を SQL 表層から忠実に戻せない）は設計メモに記す。復元側は [`super::restore_apply`]。
//!
//! テナント境界（P0）: 読む表はすべて先頭が `tenant_id` のキー範囲（所有者表だけは全件走査して
//! テナントで絞る）。ロール ID → 名前の写像は自テナントのロール表だけから作り、写像に無い ID（他テナントの
//! ロールを指す破損を含む）は固定文言で全体を失敗させる。SCHEMA・SEQUENCE・関数の ID も自テナントの
//! 一覧に無ければ同様。出力順は描画した文字列の昇順で、内部 ID の順に依存しない。

use std::collections::BTreeMap;

use super::super::function::list_functions_in_source;
use super::super::privilege::default_acl::{list_default_acl_in_txn, list_default_acl_init_in_txn};
use super::super::privilege::{
    list_acl_for_tenant_in_txn, list_acl_initialized_functions_in_txn,
    list_owners_for_tenant_in_txn, PrivilegeObject,
};
use super::super::role::bootstrap::{bootstrap_privileges_in_txn, tenant_bootstrapped_in_txn};
use super::super::role::{decode_membership, list_roles_in_txn, ROLE_MEMBERSHIPS_TABLE};
use super::super::sequence::list_sequence_definitions_in;
use super::super::CatalogError;
use super::access::function_signature;
use super::quote::KeyScope;
use super::restore_directive::{
    render_acl, render_acl_init, render_bootstrap_marker, render_default_acl,
    render_default_acl_init, render_membership, render_owner, render_role_bootstrap,
    render_role_legacy, Obj, Rel,
};
use super::{unsupported, Ctx, DdlObjectKind, DdlSection, Emitter, SchemaDdlError};

/// 1 テナントのメンバーシップ行の上限（ロール数の上限の 2 乗は現実的でないため、走査の暴走だけを止める）。
const MAX_MEMBERSHIP_ROWS: usize = 1_000_000;

/// ロール・権限の状態を指示文として書き出す（`PostData` の最後。既定権限・初期権限が復元中に作られる
/// オブジェクトへ効かないよう、すべてのオブジェクト作成・データの後に置く）。
pub(super) fn export_access_state<E>(
    ctx: &Ctx<'_>,
    out: &mut Emitter<'_, E>,
) -> Result<(), SchemaDdlError<E>> {
    let Some(tenant) = ctx.scope.tenant() else {
        return Ok(());
    };
    let roles = list_roles_in_txn(ctx.rt, tenant)?;
    let names: BTreeMap<u64, String> = roles.iter().map(|r| (r.id, r.name.clone())).collect();
    let role_name = |id: u64| -> Result<&str, SchemaDdlError<E>> {
        names
            .get(&id)
            .map(String::as_str)
            .ok_or_else(|| unsupported("privilege", "reference to a role outside the dump scope"))
    };
    let grantee_name = |id: u64| -> Result<Option<String>, SchemaDdlError<E>> {
        if id == 0 {
            Ok(None)
        } else {
            Ok(Some(role_name(id)?.to_string()))
        }
    };

    // 関数 ID → 署名、SEQUENCE ID → 共有の名前。ACL・所有者の対象の解決に使う。
    let mut fn_sigs: BTreeMap<u64, String> = BTreeMap::new();
    for def in list_functions_in_source(ctx.rt, tenant)? {
        fn_sigs.insert(def.id, function_signature::<E>(&ctx.namer, &def)?);
    }
    let mut seq_names: BTreeMap<u64, Option<String>> = BTreeMap::new();
    for (key, def, _) in list_sequence_definitions_in(ctx.rt)? {
        match ctx.namer.scope::<E>(&key)? {
            KeyScope::Shared => {
                seq_names.insert(def.id, Some(key));
            }
            KeyScope::Tenant { .. } => {
                seq_names.insert(def.id, None);
            }
            KeyScope::Foreign => {}
        }
    }
    let rel_of = |key: &str| -> Result<Rel, SchemaDdlError<E>> {
        match ctx.namer.scope::<E>(key)? {
            KeyScope::Shared => Ok(Rel {
                schema: None,
                name: key.to_string(),
            }),
            KeyScope::Tenant { schema, name } => Ok(Rel {
                schema: Some(schema),
                name,
            }),
            KeyScope::Foreign => Err(unsupported(
                "privilege",
                "reference to a relation outside the dump scope",
            )),
        }
    };
    let obj_of = |o: &PrivilegeObject| -> Result<Obj, SchemaDdlError<E>> {
        Ok(match o {
            PrivilegeObject::Table(k) => Obj::Table(rel_of(k)?),
            PrivilegeObject::Column { table, column } => {
                Obj::Column(rel_of(table)?, column.clone())
            }
            PrivilegeObject::Schema(id) => Obj::Schema(
                ctx.namer
                    .schemas
                    .get(id)
                    .cloned()
                    .ok_or_else(|| unsupported("privilege", "schema outside the dump scope"))?,
            ),
            PrivilegeObject::Sequence(id) => match seq_names.get(id) {
                Some(Some(name)) => Obj::Sequence(name.clone()),
                Some(None) => {
                    return Err(unsupported(
                        "sequence",
                        "privilege on a schema-scoped sequence cannot be restored",
                    ))
                }
                None => return Err(unsupported("privilege", "sequence outside the dump scope")),
            },
            PrivilegeObject::Function(id) => Obj::Function(
                fn_sigs
                    .get(id)
                    .cloned()
                    .ok_or_else(|| unsupported("privilege", "function outside the dump scope"))?,
            ),
        })
    };

    // ロールの旧形式ハッシュ・初期権限・マーカー。
    let mut bootstrap: Vec<String> = Vec::new();
    for role in &roles {
        if !role.legacy_phc.is_empty() {
            let phc = std::str::from_utf8(&role.legacy_phc)
                .map_err(|_| unsupported("role", "password hash is not text"))?;
            bootstrap.push(render_role_legacy::<E>(&role.name, phc)?);
        }
        let p = bootstrap_privileges_in_txn(ctx.rt, tenant, role.id)?;
        if p.all_object_privileges || p.ddl {
            bootstrap.push(render_role_bootstrap::<E>(
                &role.name,
                p.all_object_privileges,
                p.ddl,
            )?);
        }
    }
    if tenant_bootstrapped_in_txn(ctx.rt, tenant)? {
        bootstrap.push(render_bootstrap_marker());
    }
    emit_sorted(out, DdlObjectKind::RoleBootstrap, bootstrap)?;

    // メンバーシップ。
    let mut memberships: Vec<String> = Vec::new();
    match ctx.rt.open_table(ROLE_MEMBERSHIPS_TABLE) {
        Ok(t) => {
            for entry in t
                .range((tenant, 0u64, 0u64)..)
                .map_err(CatalogError::from)?
            {
                let (k, v) = entry.map_err(CatalogError::from)?;
                let (t_id, member, granted) = k.value();
                if t_id != tenant {
                    break;
                }
                if memberships.len() >= MAX_MEMBERSHIP_ROWS {
                    return Err(unsupported("role", "too many role memberships"));
                }
                let opts = decode_membership(v.value())?;
                let grantor = match opts.grantor_id {
                    Some(g) => Some(role_name(g)?),
                    None => None,
                };
                memberships.push(render_membership::<E>(
                    role_name(granted)?,
                    role_name(member)?,
                    grantor,
                    opts.grantor_root,
                    opts.admin,
                    opts.inherit,
                    opts.set,
                )?);
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(e) => return Err(CatalogError::from(e).into()),
    }
    emit_sorted(out, DdlObjectKind::RoleMembership, memberships)?;

    // 所有者。
    let mut owners: Vec<String> = Vec::new();
    for (object, owner_id) in list_owners_for_tenant_in_txn(ctx.rt, tenant)? {
        owners.push(render_owner::<E>(&obj_of(&object)?, role_name(owner_id)?)?);
    }
    emit_sorted(out, DdlObjectKind::Owner, owners)?;

    // ACL と関数 ACL の実体化印。
    let mut acl: Vec<String> = Vec::new();
    for (object, e) in list_acl_for_tenant_in_txn(ctx.rt, tenant)? {
        acl.push(render_acl::<E>(
            &obj_of(&object)?,
            &grantee_name(e.grantee_id)?,
            role_name(e.grantor_id)?,
            e.privileges,
            e.grant_options,
        )?);
    }
    for id in list_acl_initialized_functions_in_txn(ctx.rt, tenant)? {
        let sig = fn_sigs
            .get(&id)
            .ok_or_else(|| unsupported("privilege", "function outside the dump scope"))?;
        acl.push(render_acl_init::<E>(sig)?);
    }
    emit_sorted(out, DdlObjectKind::Privilege, acl)?;

    // 既定権限と、関数の既定権限の実体化印。
    let mut defaults: Vec<String> = Vec::new();
    for row in list_default_acl_in_txn(ctx.rt, tenant)? {
        let schema = if row.schema_id == 0 {
            None
        } else {
            Some(
                ctx.namer
                    .schemas
                    .get(&row.schema_id)
                    .map(String::as_str)
                    .ok_or_else(|| unsupported("privilege", "schema outside the dump scope"))?,
            )
        };
        defaults.push(render_default_acl::<E>(
            role_name(row.role_id)?,
            schema,
            row.kind,
            &grantee_name(row.entry.grantee_id)?,
            row.entry.privileges,
            row.entry.grant_options,
        )?);
    }
    for role_id in list_default_acl_init_in_txn(ctx.rt, tenant)? {
        defaults.push(render_default_acl_init::<E>(role_name(role_id)?)?);
    }
    emit_sorted(out, DdlObjectKind::DefaultPrivilege, defaults)?;
    Ok(())
}

/// 描画した文を昇順に並べて 1 件ずつ渡す（内部 ID の順に依存しない決定的な出力）。
fn emit_sorted<E>(
    out: &mut Emitter<'_, E>,
    kind: DdlObjectKind,
    mut lines: Vec<String>,
) -> Result<(), SchemaDdlError<E>> {
    lines.sort();
    for sql in &lines {
        out.emit(DdlSection::PostData, kind, sql)?;
    }
    Ok(())
}
