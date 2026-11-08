//! 既定権限（`ALTER DEFAULT PRIVILEGES`）のカタログ（SQL-40・RLS-12 のポインタ、Issue #2237。設計記録:
//! `docs/design/role-privilege-model.md` §19・D7）。
//!
//! 役割: 「この文の実行より後に作られるオブジェクト」の初期 ACL に適用する既定権限を、PostgreSQL の
//! `pg_default_acl` 相当として永続化する。書き込み（[`Storage::apply_default_acl_changes`]）は
//! `sql::default_privileges_ddl` が、作成時の適用（`apply_default_acl_to_*_in_txn`）は表・ビュー・matview・
//! 関数・SCHEMA・SEQUENCE の作成本体（`owner.rs` の所有者予約と同じ write txn）が呼ぶ。読み取り側の権限検査
//! （`object_privileges_in_txn` 等）は既存の ACL 表だけを見るため変更しない。既存のオブジェクトには作用しない。
//!
//! キー設計: [`DEFAULT_ACL_TABLE`] は `(tenant_id, role_id, schema_id, object_kind, grantee_id)` → blob
//! （版 1・`privileges u16`・`grant_options u16`）。`role_id` は対象ロール（作成者＝所有者）、`schema_id` の 0 は
//! 「全体（`IN SCHEMA` なし）」、`object_kind` は ACL と同じ 1=表・3=スキーマ・4=シーケンス・5=関数、
//! `grantee_id` の 0 は PUBLIC。先頭が `tenant_id` なので他テナントの行は構造上引けない。PostgreSQL と同じく
//! 差分ではなく「結果の ACL」を保存する。所有者自身を付与先とする行は保存しない（所有者は暗黙に全権限を持つ。
//! 既知の差）。[`DEFAULT_ACL_INIT_TABLE`] は関数の全体エントリが組み込みの既定（PUBLIC が EXECUTE）を置き換えた
//! ことの印（`acl_init` と同じ発想。他の種別の組み込みの既定は空で、印は要らない）。
//!
//! 既存データの不変: 2 表とも最初の書き込みまで作らない（遅延作成。読み取りの `TableDoesNotExist` は 0 件）。
//!
//! fail-closed: 未知の版・許可集合の外のビット・部分集合違反・0 の権限・所有者を付与先とする行・切断・末尾の
//! 余りは固定文言の `CorruptSchema`。エラーに blob の断片・tenant_id を含めない。件数上限・組み合わせ上限は
//! 検証してから確保・書き込みする。

use crate::storage::change_log::CaptureWriteTxn as _;
use redb::{ReadableTable, TableDefinition};

use crate::storage::read_source::ReadSource;

use super::super::namespace::RelationKey;
use super::super::role::graph::can_set_role;
use super::super::role::{get_role_by_id_in_txn, get_role_in_txn, validate_tenant};
use super::super::{CatalogError, Result, Storage};
use super::apply::{AclChangeError, GranteeSpec};
use super::function::mark_acl_initialized_in_txn;
use super::{
    corrupt, upsert_acl_in_txn, validate_privileges, PrivilegeObject, KIND_FUNCTION, KIND_SCHEMA,
    KIND_SEQUENCE, KIND_TABLE, PRIV_EXECUTE, PUBLIC_GRANTEE_ID,
};

/// 既定権限の専用カタログ表。キーは `(tenant_id, role_id, schema_id, object_kind, grantee_id)`。
pub(crate) const DEFAULT_ACL_TABLE: TableDefinition<(&str, u64, u64, u8, u64), &[u8]> =
    TableDefinition::new("default_acl");

/// 関数の全体エントリが組み込みの既定を置き換えた印の専用表。キーは `(tenant_id, role_id, schema_id, object_kind)`、
/// 値は版 1 バイト。
pub(crate) const DEFAULT_ACL_INIT_TABLE: TableDefinition<(&str, u64, u64, u8), &[u8]> =
    TableDefinition::new("default_acl_init");

/// blob・印の版。
const DEFAULT_ACL_VERSION: u8 = 1;

/// 1 テナントあたりの既定権限の行数の上限（実装既定値。超過は `54000`）。
pub(crate) const MAX_DEFAULT_ACL_ENTRIES_PER_TENANT: usize = 10_000;

/// 1 文の組み合わせ総数（対象ロール × SCHEMA × 付与先）の上限（`apply.rs` の対象数上限に揃える）。
const MAX_DEFAULT_ACL_COMBINATIONS: usize = 20_000;

/// 既定権限の対象オブジェクトの種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DefaultAclKind {
    /// 表・ビュー・matview。
    Tables,
    Sequences,
    Functions,
    Schemas,
}

impl DefaultAclKind {
    fn tag(self) -> u8 {
        match self {
            Self::Tables => KIND_TABLE,
            Self::Sequences => KIND_SEQUENCE,
            Self::Functions => KIND_FUNCTION,
            Self::Schemas => KIND_SCHEMA,
        }
    }

    fn from_tag(tag: u8) -> Result<Self> {
        Ok(match tag {
            KIND_TABLE => Self::Tables,
            KIND_SEQUENCE => Self::Sequences,
            KIND_FUNCTION => Self::Functions,
            KIND_SCHEMA => Self::Schemas,
            _ => return Err(corrupt()),
        })
    }

    /// 種別ごとに許す権限ビットの集合（ACL の許可集合と同じ）。
    fn allowed(self) -> u16 {
        match self {
            Self::Tables => PrivilegeObject::Table(String::new()).allowed_privileges(),
            Self::Sequences => PrivilegeObject::Sequence(1).allowed_privileges(),
            Self::Functions => PrivilegeObject::Function(1).allowed_privileges(),
            Self::Schemas => PrivilegeObject::Schema(1).allowed_privileges(),
        }
    }
}

/// 既定権限の 1 行（キーの `role_id`・`schema_id`・種別は別に持つ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DefaultEntry {
    /// 付与先のロール ID（0 は PUBLIC）。
    pub(crate) grantee_id: u64,
    pub(crate) privileges: u16,
    /// `privileges` の部分集合（GRANT OPTION）。
    pub(crate) grant_options: u16,
}

/// [`Storage::list_default_acl`] の 1 行（確認用。`\ddp` 相当の SQL 表層ビューは後続の担当）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // 確認用の読み取り API（単体テストが使う。SQL 表層のビューは後続 Issue）。
pub(crate) struct DefaultAclRow {
    pub(crate) role_id: u64,
    /// 0 は全体（`IN SCHEMA` なし）。
    pub(crate) schema_id: u64,
    pub(crate) kind: DefaultAclKind,
    pub(crate) entry: DefaultEntry,
}

/// 組み込みの既定（全体エントリが無いときの状態）。関数は PUBLIC が EXECUTE を持ち、他は空。
fn builtin_default(kind: DefaultAclKind) -> Vec<DefaultEntry> {
    if kind == DefaultAclKind::Functions {
        vec![DefaultEntry {
            grantee_id: PUBLIC_GRANTEE_ID,
            privileges: PRIV_EXECUTE,
            grant_options: 0,
        }]
    } else {
        Vec::new()
    }
}

fn encode_blob(kind: DefaultAclKind, e: &DefaultEntry) -> Result<Vec<u8>> {
    if !validate_privileges(kind.allowed(), e.privileges, e.grant_options) {
        return Err(CatalogError::Invalid("invalid privilege set".to_string()));
    }
    let mut out = vec![DEFAULT_ACL_VERSION];
    out.extend_from_slice(&e.privileges.to_le_bytes());
    out.extend_from_slice(&e.grant_options.to_le_bytes());
    Ok(out)
}

fn decode_row(
    key: (&str, u64, u64, u8, u64),
    value: &[u8],
) -> Result<(u64, u64, DefaultAclKind, DefaultEntry)> {
    let (_, role_id, schema_id, tag, grantee_id) = key;
    let kind = DefaultAclKind::from_tag(tag)?;
    let [version, p0, p1, g0, g1] = *value else {
        return Err(corrupt());
    };
    if version != DEFAULT_ACL_VERSION || role_id == 0 {
        return Err(corrupt());
    }
    let privileges = u16::from_le_bytes([p0, p1]);
    let grant_options = u16::from_le_bytes([g0, g1]);
    // 所有者自身を付与先とする行は保存しない契約（持っていれば破損）。SCHEMA の `IN SCHEMA` 行は無い。
    if !validate_privileges(kind.allowed(), privileges, grant_options)
        || (grantee_id != PUBLIC_GRANTEE_ID && grantee_id == role_id)
        || (kind == DefaultAclKind::Schemas && schema_id != 0)
    {
        return Err(corrupt());
    }
    Ok((
        role_id,
        schema_id,
        kind,
        DefaultEntry {
            grantee_id,
            privileges,
            grant_options,
        },
    ))
}

/// 1 つの `(role, schema, kind)` の行を `grantee_id` の昇順で返す（表が無ければ空）。破損行は `Err`。
fn load_entries<R: ReadSource>(
    src: &R,
    tenant: &str,
    role_id: u64,
    schema_id: u64,
    kind: DefaultAclKind,
) -> Result<Vec<DefaultEntry>> {
    let table = match src.open_table(DEFAULT_ACL_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.range((tenant, role_id, schema_id, kind.tag(), 0u64)..)? {
        let (k, v) = entry?;
        let kv = k.value();
        if kv.0 != tenant || kv.1 != role_id || kv.2 != schema_id || kv.3 != kind.tag() {
            break;
        }
        if out.len() >= MAX_DEFAULT_ACL_ENTRIES_PER_TENANT {
            return Err(corrupt());
        }
        out.push(decode_row(kv, v.value())?.3);
    }
    Ok(out)
}

/// 関数の全体エントリが組み込みの既定を置き換えた印があるか。値が不正なら `CorruptSchema`。
fn init_marker_in_txn<R: ReadSource>(src: &R, tenant: &str, role_id: u64) -> Result<bool> {
    let table = match src.open_table(DEFAULT_ACL_INIT_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let found = table.get((tenant, role_id, 0u64, KIND_FUNCTION))?;
    match found {
        Some(v) if v.value() == [DEFAULT_ACL_VERSION] => Ok(true),
        Some(_) => Err(corrupt()),
        None => Ok(false),
    }
}

/// 変更・合成の基準となる状態。関数の全体エントリは、印が無ければ組み込みの既定を展開して返す。
fn effective_state<R: ReadSource>(
    src: &R,
    tenant: &str,
    role_id: u64,
    schema_id: u64,
    kind: DefaultAclKind,
) -> Result<Vec<DefaultEntry>> {
    if kind == DefaultAclKind::Functions
        && schema_id == 0
        && !init_marker_in_txn(src, tenant, role_id)?
    {
        // 印が無いのに行がある状態は不整合（fail-closed。関数 ACL の `acl_init` と同じ契約）。
        if !load_entries(src, tenant, role_id, schema_id, kind)?.is_empty() {
            return Err(corrupt());
        }
        return Ok(builtin_default(kind));
    }
    load_entries(src, tenant, role_id, schema_id, kind)
}

/// テナントの行数を `limit` まで数える（上限判定用）。
fn count_tenant_entries<R: ReadSource>(src: &R, tenant: &str, limit: usize) -> Result<usize> {
    let table = match src.open_table(DEFAULT_ACL_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    let mut n = 0usize;
    for entry in table.range((tenant, 0u64, 0u64, 0u8, 0u64)..)? {
        let (k, _) = entry?;
        if k.value().0 != tenant || n >= limit {
            break;
        }
        n += 1;
    }
    Ok(n)
}

fn sorted(mut v: Vec<DefaultEntry>) -> Vec<DefaultEntry> {
    v.sort_by_key(|e| e.grantee_id);
    v
}

/// `(role, schema, kind)` の行を `entries` で置き換える（既存の行は先に消す）。
fn replace_entries_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    role_id: u64,
    schema_id: u64,
    kind: DefaultAclKind,
    entries: &[DefaultEntry],
) -> Result<()> {
    let old = load_entries(write_txn, tenant, role_id, schema_id, kind)?;
    let mut table = write_txn.open_table_logged(DEFAULT_ACL_TABLE)?;
    for e in &old {
        table.remove((tenant, role_id, schema_id, kind.tag(), e.grantee_id))?;
    }
    for e in entries {
        let blob = encode_blob(kind, e)?;
        table.insert(
            (tenant, role_id, schema_id, kind.tag(), e.grantee_id),
            blob.as_slice(),
        )?;
    }
    Ok(())
}

fn set_init_marker_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    role_id: u64,
    present: bool,
) -> Result<()> {
    if !present {
        match ReadSource::open_table(write_txn, DEFAULT_ACL_INIT_TABLE) {
            Ok(_) => {}
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
    }
    let mut table = write_txn.open_table_logged(DEFAULT_ACL_INIT_TABLE)?;
    if present {
        table.insert(
            (tenant, role_id, 0u64, KIND_FUNCTION),
            [DEFAULT_ACL_VERSION].as_slice(),
        )?;
    } else {
        table.remove((tenant, role_id, 0u64, KIND_FUNCTION))?;
    }
    Ok(())
}

/// [`Storage::apply_default_acl_changes`] への要求。
#[derive(Debug, Clone)]
pub(crate) struct DefaultAclRequest<'a> {
    pub(crate) is_grant: bool,
    /// 実行者（現在ロール）の ID。0 は不可。
    pub(crate) actor_id: u64,
    /// 運用者権限（DDL 権限ゲート通過済み）。`FOR ROLE` の SET 経路の検査を免除する（O8 と同じ）。
    pub(crate) operator: bool,
    /// `FOR ROLE` の対象ロール名（空なら実行者）。
    pub(crate) for_roles: &'a [String],
    /// `IN SCHEMA` の SCHEMA 名（空なら全体）。
    pub(crate) schemas: &'a [String],
    pub(crate) kind: DefaultAclKind,
    pub(crate) privileges: u16,
    pub(crate) grantees: &'a [GranteeSpec],
    /// `WITH GRANT OPTION`（GRANT）／`GRANT OPTION FOR`（REVOKE）。
    pub(crate) grant_option: bool,
}

impl Storage {
    /// `tenant` の既定権限へ `ALTER DEFAULT PRIVILEGES` の GRANT／REVOKE を単一 write txn・1 回の commit で
    /// 反映する。呼び出し元は `sql::default_privileges_ddl`（現在ロールの束縛・明示 Tx のゲートの後）。
    ///
    /// 判定順（すべて commit 前に返り副作用ゼロ）: 実行者の現存 → 付与先ロールの解決（`42704`。他テナントに
    /// だけ在るロールも不在と同じ）→ 対象ロールの解決（`42704`）→ 対象ロールへの SET 経路（`42501`。運用者権限は
    /// 免除）→ SCHEMA の解決（`3F000`。他テナントも不在と同じ。CREATE 権限は検査しない）→ 組み合わせ総数
    /// （`54000`）→ 書き込み → テナントの行数上限（`54000`）。戻り値は書き換えた `(role, schema)` の組の数。
    pub(crate) fn apply_default_acl_changes(
        &self,
        tenant: &str,
        req: &DefaultAclRequest<'_>,
    ) -> std::result::Result<usize, AclChangeError> {
        validate_tenant(tenant)?;
        if req.actor_id == 0 {
            return Err(AclChangeError::PermissionDenied);
        }
        let write_txn = self
            .begin_write_txn()
            .map_err(crate::catalog::convert_storage_error)?;
        if get_role_by_id_in_txn(&*write_txn, tenant, req.actor_id)?.is_none() {
            return Err(AclChangeError::PermissionDenied);
        }
        // 付与先。
        let mut grantee_ids: Vec<u64> = Vec::new();
        for g in req.grantees {
            let id = match g {
                GranteeSpec::Public => PUBLIC_GRANTEE_ID,
                GranteeSpec::Id(id) => {
                    if get_role_by_id_in_txn(&*write_txn, tenant, *id)?.is_none() {
                        return Err(AclChangeError::PermissionDenied);
                    }
                    *id
                }
                GranteeSpec::Name(n) => get_role_in_txn(&*write_txn, tenant, n)?
                    .map(|r| r.id)
                    .ok_or_else(|| AclChangeError::GranteeNotFound(n.clone()))?,
            };
            if !grantee_ids.contains(&id) {
                grantee_ids.push(id);
            }
        }
        // 対象ロール（省略時は実行者）。
        let mut role_ids: Vec<u64> = Vec::new();
        if req.for_roles.is_empty() {
            role_ids.push(req.actor_id);
        }
        for n in req.for_roles {
            let id = get_role_in_txn(&*write_txn, tenant, n)?
                .map(|r| r.id)
                .ok_or_else(|| AclChangeError::GranteeNotFound(n.clone()))?;
            if !role_ids.contains(&id) {
                role_ids.push(id);
            }
        }
        // 対象ロールへの SET 経路（PostgreSQL 16 以降の `check_can_set_role`）。運用者権限は免除する。
        if !req.operator {
            for r in &role_ids {
                if !can_set_role(&*write_txn, tenant, req.actor_id, *r)? {
                    return Err(AclChangeError::PermissionDenied);
                }
            }
        }
        // SCHEMA（0 は全体）。
        let mut scopes: Vec<u64> = Vec::new();
        if req.schemas.is_empty() {
            scopes.push(0);
        }
        for name in req.schemas {
            let id = super::super::namespace::schema_id_in_txn(&*write_txn, tenant, name)?
                .ok_or_else(|| AclChangeError::SchemaNotFound(name.clone()))?;
            if !scopes.contains(&id) {
                scopes.push(id);
            }
        }
        let combos = role_ids
            .len()
            .checked_mul(scopes.len())
            .and_then(|n| n.checked_mul(grantee_ids.len().max(1)))
            .ok_or(AclChangeError::LimitExceeded)?;
        if combos > MAX_DEFAULT_ACL_COMBINATIONS {
            return Err(AclChangeError::LimitExceeded);
        }
        let kind = req.kind;
        let mut changed = 0usize;
        for role in &role_ids {
            for scope in &scopes {
                let before = sorted(effective_state(&*write_txn, tenant, *role, *scope, kind)?);
                let mut work = before.clone();
                for g in &grantee_ids {
                    // 所有者自身への付与は保存しない（所有者は暗黙に全権限を持つ）。
                    if *g != PUBLIC_GRANTEE_ID && g == role {
                        continue;
                    }
                    let pos = work.iter().position(|e| e.grantee_id == *g);
                    if req.is_grant {
                        match pos.and_then(|i| work.get_mut(i)) {
                            Some(e) => {
                                e.privileges |= req.privileges;
                                if req.grant_option {
                                    e.grant_options |= req.privileges;
                                }
                            }
                            None => work.push(DefaultEntry {
                                grantee_id: *g,
                                privileges: req.privileges,
                                grant_options: if req.grant_option { req.privileges } else { 0 },
                            }),
                        }
                    } else if let Some(e) = pos.and_then(|i| work.get_mut(i)) {
                        if req.grant_option {
                            e.grant_options &= !req.privileges;
                        } else {
                            e.privileges &= !req.privileges;
                            e.grant_options &= e.privileges;
                        }
                    }
                }
                work.retain(|e| e.privileges != 0);
                let work = sorted(work);
                if work == before {
                    continue;
                }
                let is_global_functions = kind == DefaultAclKind::Functions && *scope == 0;
                if is_global_functions && work == builtin_default(kind) {
                    // 組み込みの既定へ戻った: 行と印を消す。
                    replace_entries_in_txn(&write_txn, tenant, *role, *scope, kind, &[])?;
                    set_init_marker_in_txn(&write_txn, tenant, *role, false)?;
                } else {
                    replace_entries_in_txn(&write_txn, tenant, *role, *scope, kind, &work)?;
                    if is_global_functions {
                        set_init_marker_in_txn(&write_txn, tenant, *role, true)?;
                    }
                }
                changed += 1;
            }
        }
        if changed == 0 {
            return Ok(0);
        }
        if count_tenant_entries(
            &*write_txn,
            tenant,
            MAX_DEFAULT_ACL_ENTRIES_PER_TENANT.saturating_add(1),
        )? > MAX_DEFAULT_ACL_ENTRIES_PER_TENANT
        {
            return Err(AclChangeError::LimitExceeded);
        }
        crate::recovery::commit_boundary::commit(write_txn)
            .map_err(crate::catalog::convert_storage_error)?;
        Ok(changed)
    }

    /// `tenant` の既定権限の全行を `(role_id, schema_id, kind, grantee_id)` の順に返す（確認用の読み取り専用 API。
    /// `\ddp` 相当の SQL 表層ビューは後続の担当）。破損行は `Err`。
    #[allow(dead_code)] // 単体テストの確定オラクル。SQL 表層のビューは後続 Issue。
    pub(crate) fn list_default_acl(&self, tenant: &str) -> Result<Vec<DefaultAclRow>> {
        use redb::ReadableDatabase;
        validate_tenant(tenant)?;
        let rt = self.db().begin_read()?;
        let table = match ReadSource::open_table(&rt, DEFAULT_ACL_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in table.range((tenant, 0u64, 0u64, 0u8, 0u64)..)? {
            let (k, v) = entry?;
            let kv = k.value();
            if kv.0 != tenant {
                break;
            }
            if out.len() >= MAX_DEFAULT_ACL_ENTRIES_PER_TENANT {
                return Err(corrupt());
            }
            let (role_id, schema_id, kind, entry) = decode_row(kv, v.value())?;
            out.push(DefaultAclRow {
                role_id,
                schema_id,
                kind,
                entry,
            });
        }
        Ok(out)
    }
}

/// 作成時の合成（PostgreSQL の `get_user_default_acl` 相当）。初期 ACL ＝（全体エントリがあればそれ、無ければ
/// 組み込みの既定）∪（作成先 SCHEMA のスキーマ別エントリ。`scope_schema` が 0 なら無し）。結果が組み込みの
/// 既定と等しければ `None`（何も書かない）。
fn compose_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    role_id: u64,
    scope_schema: u64,
    kind: DefaultAclKind,
) -> Result<Option<Vec<DefaultEntry>>> {
    let mut merged = effective_state(src, tenant, role_id, 0, kind)?;
    if scope_schema != 0 && kind != DefaultAclKind::Schemas {
        for e in load_entries(src, tenant, role_id, scope_schema, kind)? {
            match merged.iter_mut().find(|m| m.grantee_id == e.grantee_id) {
                Some(m) => {
                    m.privileges |= e.privileges;
                    m.grant_options |= e.grant_options;
                }
                None => merged.push(e),
            }
        }
    }
    let merged = sorted(merged);
    if merged == builtin_default(kind) {
        return Ok(None);
    }
    Ok(Some(merged))
}

/// relation・関数のキーが属する SCHEMA の内部 ID（`public` の裸の名前は 0）。
fn scope_of_key(key: &str) -> Result<u64> {
    Ok(match RelationKey::parse(key)? {
        RelationKey::Public(_) => 0,
        RelationKey::InSchema { schema_id, .. } => schema_id,
    })
}

/// 合成結果を `object` の ACL 行として書く（付与者は作成者＝対象ロール。上限は既存の ACL と同じ）。
fn write_composed_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    role_id: u64,
    object: &PrivilegeObject,
    entries: &[DefaultEntry],
) -> Result<()> {
    for e in entries {
        upsert_acl_in_txn(
            write_txn,
            tenant,
            object,
            e.grantee_id,
            role_id,
            e.privileges,
            e.grant_options,
        )?;
    }
    Ok(())
}

/// 新規に作った relation（表・ビュー・matview。キー `key`）へ、作成者 `role_id` の `TABLES` の既定権限を
/// 同じ write txn で適用する（`owner::apply_pending_relation_owner_in_txn` から呼ぶ）。
pub(crate) fn apply_default_acl_to_relation_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    role_id: u64,
    key: &str,
) -> Result<()> {
    let scope = scope_of_key(key)?;
    if let Some(entries) =
        compose_in_txn(write_txn, tenant, role_id, scope, DefaultAclKind::Tables)?
    {
        write_composed_in_txn(
            write_txn,
            tenant,
            role_id,
            &PrivilegeObject::Table(key.to_string()),
            &entries,
        )?;
    }
    Ok(())
}

/// 新規に作った関数（キー `key`・内部 ID `function_id`）へ `FUNCTIONS` の既定権限を適用する。組み込みの既定と
/// 異なるときは、行と `acl_init` の印を同じ txn で書く（印の無い関数に行だけを書くと不整合になるため）。
pub(crate) fn apply_default_acl_to_function_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    role_id: u64,
    key: &str,
    function_id: u64,
) -> Result<()> {
    let scope = scope_of_key(key)?;
    if let Some(entries) =
        compose_in_txn(write_txn, tenant, role_id, scope, DefaultAclKind::Functions)?
    {
        write_composed_in_txn(
            write_txn,
            tenant,
            role_id,
            &PrivilegeObject::Function(function_id),
            &entries,
        )?;
        mark_acl_initialized_in_txn(write_txn, tenant, function_id)?;
    }
    Ok(())
}

/// 新規に作った SCHEMA（内部 ID `schema_id`）へ、所有者 `role_id` の `SCHEMAS` の既定権限（全体エントリのみ）を
/// 適用する（`create_schema_with_limit` から呼ぶ）。
pub(crate) fn apply_default_acl_to_schema_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    role_id: u64,
    schema_id: u64,
) -> Result<()> {
    if let Some(entries) = compose_in_txn(write_txn, tenant, role_id, 0, DefaultAclKind::Schemas)? {
        write_composed_in_txn(
            write_txn,
            tenant,
            role_id,
            &PrivilegeObject::Schema(schema_id),
            &entries,
        )?;
    }
    Ok(())
}

/// 新規に作った SEQUENCE（キー `key`・内部 ID `sequence_id`）へ `SEQUENCES` の既定権限を適用する。SEQUENCE は
/// 所有者の機構を持たないため、付与者は作成ロールになる（運用者権限の無い作成ロールは取り消せない。既知の制約）。
pub(crate) fn apply_default_acl_to_sequence_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    role_id: u64,
    key: &str,
    sequence_id: u64,
) -> Result<()> {
    let scope = scope_of_key(key)?;
    if let Some(entries) =
        compose_in_txn(write_txn, tenant, role_id, scope, DefaultAclKind::Sequences)?
    {
        write_composed_in_txn(
            write_txn,
            tenant,
            role_id,
            &PrivilegeObject::Sequence(sequence_id),
            &entries,
        )?;
    }
    Ok(())
}

/// `tenant` の SCHEMA `schema_id` に紐づく既定権限の行を全て消す（`DROP SCHEMA` の同一 write txn から呼ぶ。
/// PostgreSQL の自動依存と同じく SCHEMA の空判定の対象にはしない）。他の SCHEMA・他テナントの行には触れない。
pub(crate) fn remove_schema_default_acl_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    schema_id: u64,
) -> Result<()> {
    validate_tenant(tenant)?;
    let mut doomed: Vec<(u64, u8, u64)> = Vec::new();
    {
        let table = match ReadSource::open_table(write_txn, DEFAULT_ACL_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let mut seen = 0usize;
        for entry in table.range((tenant, 0u64, 0u64, 0u8, 0u64)..)? {
            let (k, v) = entry?;
            let kv = k.value();
            if kv.0 != tenant {
                break;
            }
            seen += 1;
            if seen > MAX_DEFAULT_ACL_ENTRIES_PER_TENANT {
                return Err(corrupt());
            }
            // 構造の破損は削除せずに `Err` にする（検証してから消す）。
            decode_row(kv, v.value())?;
            if kv.2 == schema_id {
                doomed.push((kv.1, kv.3, kv.4));
            }
        }
    }
    if doomed.is_empty() {
        return Ok(());
    }
    let mut table = write_txn.open_table_logged(DEFAULT_ACL_TABLE)?;
    for (role, tag, grantee) in doomed {
        table.remove((tenant, role, schema_id, tag, grantee))?;
    }
    Ok(())
}

/// `tenant` の既定権限に、ロール `role_id` が対象ロールまたは付与先として載る行（と印）があるか
/// （`DROP ROLE` の `2BP01` 判定。`catalog::role::role_has_dependents_in_txn` から呼ばれる）。PUBLIC の
/// 予約 ID 0 は対象外。テナントの範囲だけを走査し、件数超過・破損行は読み飛ばさず `Err`（fail-closed）。
pub(crate) fn default_acl_references_role_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    role_id: u64,
) -> Result<bool> {
    validate_tenant(tenant)?;
    if role_id == PUBLIC_GRANTEE_ID {
        return Ok(false);
    }
    match src.open_table(DEFAULT_ACL_TABLE) {
        Ok(table) => {
            let mut seen = 0usize;
            for entry in table.range((tenant, 0u64, 0u64, 0u8, 0u64)..)? {
                let (k, v) = entry?;
                let kv = k.value();
                if kv.0 != tenant {
                    break;
                }
                seen += 1;
                if seen > MAX_DEFAULT_ACL_ENTRIES_PER_TENANT {
                    return Err(corrupt());
                }
                decode_row(kv, v.value())?;
                if kv.1 == role_id || kv.4 == role_id {
                    return Ok(true);
                }
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => {}
        Err(e) => return Err(e.into()),
    }
    // 関数の全体エントリが空（全て取り消し済み）でも印だけが残る。対象ロールとしての参照に数える。
    init_marker_in_txn(src, tenant, role_id)
}

/// `tenant` の既定権限の行をすべて返す（論理ダンプの書き出し専用。Issue #3096）。先頭が `tenant_id` の
/// キー範囲だけを走査する。破損行・上限超過は `Err`。
pub(crate) fn list_default_acl_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
) -> Result<Vec<DefaultAclRow>> {
    validate_tenant(tenant)?;
    let table = match src.open_table(DEFAULT_ACL_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.range((tenant, 0u64, 0u64, 0u8, 0u64)..)? {
        let (k, v) = entry?;
        let kv = k.value();
        if kv.0 != tenant {
            break;
        }
        if out.len() >= MAX_DEFAULT_ACL_ENTRIES_PER_TENANT {
            return Err(corrupt());
        }
        let (role_id, schema_id, kind, entry) = decode_row(kv, v.value())?;
        out.push(DefaultAclRow {
            role_id,
            schema_id,
            kind,
            entry,
        });
    }
    Ok(out)
}

/// `tenant` で「関数の全体エントリが組み込みの既定を置き換えた」印のあるロール ID を昇順で返す
/// （論理ダンプの書き出し専用。Issue #3096）。関数・全体以外の印や不正な値は `CorruptSchema`。
pub(crate) fn list_default_acl_init_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
) -> Result<Vec<u64>> {
    validate_tenant(tenant)?;
    let table = match src.open_table(DEFAULT_ACL_INIT_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.range((tenant, 0u64, 0u64, 0u8)..)? {
        let (k, v) = entry?;
        let (t, role_id, schema_id, kind) = k.value();
        if t != tenant {
            break;
        }
        if schema_id != 0
            || kind != KIND_FUNCTION
            || role_id == 0
            || v.value() != [DEFAULT_ACL_VERSION]
        {
            return Err(corrupt());
        }
        out.push(role_id);
    }
    Ok(out)
}

/// 既定権限の 1 行を書く（論理ダンプの復元専用。Issue #3096）。種別ごとの許可集合・所有者自身への付与の
/// 禁止・`IN SCHEMA` を持てない SCHEMA 種別は、保存形式の契約（[`decode_row`] が破損と見なす形）と同じ
/// 規則で拒否する（`Invalid`）。ロール・SCHEMA の現存は呼び出し側（復元の名前解決）が確かめる。
pub(crate) fn put_default_acl_row_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    role_id: u64,
    schema_id: u64,
    kind: DefaultAclKind,
    entry: &DefaultEntry,
) -> Result<()> {
    validate_tenant(tenant)?;
    if role_id == 0
        || (entry.grantee_id != PUBLIC_GRANTEE_ID && entry.grantee_id == role_id)
        || (kind == DefaultAclKind::Schemas && schema_id != 0)
    {
        return Err(CatalogError::Invalid(
            "invalid default privilege entry".to_string(),
        ));
    }
    let blob = encode_blob(kind, entry)?;
    let mut table = write_txn.open_table_logged(DEFAULT_ACL_TABLE)?;
    table.insert(
        (tenant, role_id, schema_id, kind.tag(), entry.grantee_id),
        blob.as_slice(),
    )?;
    Ok(())
}

/// ロール `role_id` の関数の既定権限の印を書く（論理ダンプの復元専用。Issue #3096）。
pub(crate) fn put_default_acl_init_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    role_id: u64,
) -> Result<()> {
    validate_tenant(tenant)?;
    if role_id == 0 {
        return Err(CatalogError::Invalid(
            "invalid default privilege entry".to_string(),
        ));
    }
    set_init_marker_in_txn(write_txn, tenant, role_id, true)
}

#[cfg(test)]
mod tests;
