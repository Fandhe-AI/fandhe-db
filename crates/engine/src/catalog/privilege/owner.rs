//! オブジェクト所有者のカタログ（SQL-40・RLS-12 のポインタ、Issue #2236。設計記録:
//! `docs/design/role-privilege-model.md` §14）。
//!
//! 役割: 表・ビュー・matview・SCHEMA・関数の所有者ロールを永続化し、所有者判定の唯一の関数
//! [`is_owner_in_txn`] を提供する。ACL（`privilege.rs`）と同じ対象表現（[`PrivilegeObject`]）と
//! 同じライフサイクル規則（DROP・改名の同一 write txn で掃除する）を使う。権限検査の呼び出し元は
//! `sql::privilege_gate` の所有者判定、`catalog::privilege::apply`（GRANT／REVOKE の所有者相当）、
//! `sql::schema_privilege`（SCHEMA の USAGE・CREATE）、`sql::owner_ddl`（`OWNER TO`）。
//!
//! キー設計: [`OWNERS_TABLE`] は `(object_kind, object_key, tenant_id)` → blob（版 1・owner_id u64 LE）。
//! `object_kind`・`object_key` は ACL と同じ規則（relation は relation キー、SCHEMA・関数は内部 ID の
//! 10 進正規形）。relation は全テナント共有カタログにあるため、所有者はテナントごとの行にする。
//! `tenant_id` を末尾に置くのは、relation の DROP・改名で全テナント分の行を範囲走査だけで掃除する
//! ため（ACL のような全件走査を避ける）。読み書きは常に 3 成分の完全一致で、テナントを跨いで引く
//! 経路は持たない（一覧が必要な [`owner_references_role_in_txn`] は走査しつつテナントで絞る）。
//!
//! 既存データの不変: [`OWNERS_TABLE`] は最初の書き込みまで作らない（遅延作成。読み取りの
//! `TableDoesNotExist` は「所有者なし」）。所有者行の無いオブジェクトはどのロールの所有にもならず、
//! 所有者として振る舞えるのは `--ddl-allowed-users` の運用者だけ（拒否側。ADR O5）。
//!
//! fail-closed: 未知の版・owner 0・切断・末尾の余りは固定文言の `CorruptSchema`。エラーに blob の断片・
//! tenant_id を含めない。

use redb::{ReadableTable, TableDefinition};

use crate::storage::read_source::ReadSource;

use super::super::role::{get_role_by_id_in_txn, validate_tenant};
use super::super::{CatalogError, Result};
use super::{PrivilegeObject, KIND_COLUMN, KIND_FUNCTION, KIND_SCHEMA, KIND_TABLE};

/// 所有者の専用カタログ表。キーは `(object_kind, object_key, tenant_id)`。
pub(crate) const OWNERS_TABLE: TableDefinition<(u8, &str, &str), &[u8]> =
    TableDefinition::new("object_owners");

/// 所有者 blob の版。
const OWNER_BLOB_VERSION: u8 = 1;

/// relation の掃除で 1 度に集める行数（無制限にメモリへ載せない）。
const SWEEP_BATCH: usize = 256;

/// 破壊的な操作（DROP・ALTER）の write txn 内で課す所有者検査の要求（Issue #2236）。
///
/// 検査は対象の解決（テナント内）より後・変更より前に、変更と同じ write txn の中で行う
/// （並行する `OWNER TO` との TOCTOU を防ぐ）。呼び出し元は `sql::ddl`・`sql::udf_ddl` が
/// `sql::privilege_gate::GateDecision` から組み立てる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OwnerGate {
    /// 検査しない（運用者権限で許可済みの経路、および従来の呼び出し）。
    Skip,
    /// `role_id` が対象の所有者でなければ [`CatalogError::NotObjectOwner`]。
    RequireOwner(u64),
}

impl OwnerGate {
    /// 対象 `object` について要求を満たすか検査する（満たさなければ `NotObjectOwner`）。
    pub(crate) fn check<R: ReadSource>(
        self,
        src: &R,
        tenant: &str,
        object: &PrivilegeObject,
    ) -> Result<()> {
        match self {
            Self::Skip => Ok(()),
            Self::RequireOwner(role_id) => {
                if is_owner_in_txn(src, tenant, object, role_id)? {
                    Ok(())
                } else {
                    Err(CatalogError::NotObjectOwner)
                }
            }
        }
    }
}

fn corrupt() -> CatalogError {
    CatalogError::CorruptSchema("invalid object owner entry".to_string())
}

/// 所有者を持てる対象のキー成分 `(kind, key)`。列は親の表の所有者を見る。
fn owner_components(object: &PrivilegeObject) -> Result<(u8, String)> {
    let (kind, key, _) = object.to_components()?;
    let kind = if kind == KIND_COLUMN {
        KIND_TABLE
    } else {
        kind
    };
    if !matches!(kind, KIND_TABLE | KIND_SCHEMA | KIND_FUNCTION) {
        return Err(CatalogError::Invalid("invalid owner object".to_string()));
    }
    Ok((kind, key))
}

fn encode_owner(owner_id: u64) -> [u8; 9] {
    let mut out = [0u8; 9];
    out[0] = OWNER_BLOB_VERSION;
    out[1..].copy_from_slice(&owner_id.to_le_bytes());
    out
}

fn decode_owner(bytes: &[u8]) -> Result<u64> {
    let [version, rest @ ..] = bytes else {
        return Err(corrupt());
    };
    let id_bytes: [u8; 8] = rest.try_into().map_err(|_| corrupt())?;
    let id = u64::from_le_bytes(id_bytes);
    if *version != OWNER_BLOB_VERSION || id == 0 {
        return Err(corrupt());
    }
    Ok(id)
}

/// 対象の所有者ロール ID（行が無ければ `None`。他テナントの行は引けない）。
pub(crate) fn owner_of_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    object: &PrivilegeObject,
) -> Result<Option<u64>> {
    validate_tenant(tenant)?;
    let (kind, key) = owner_components(object)?;
    let table = match src.open_table(OWNERS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let Some(v) = table.get((kind, key.as_str(), tenant))? else {
        return Ok(None);
    };
    Ok(Some(decode_owner(v.value())?))
}

/// `role_id` が対象の所有者か。所有者判定の唯一の関数（`role_id` 0＝PUBLIC・未束縛は常に偽）。
pub(crate) fn is_owner_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    object: &PrivilegeObject,
    role_id: u64,
) -> Result<bool> {
    // 所有者を持てない種別（SEQUENCE）は常に「所有者でない」。ACL（GRANT OPTION・運用者権限）の
    // 判定だけで進めるため、`owner_components` の Invalid を GRANT／REVOKE へ伝播させない。
    if role_id == 0 || matches!(object, PrivilegeObject::Sequence(_)) {
        return Ok(false);
    }
    Ok(owner_of_in_txn(src, tenant, object)? == Some(role_id))
}

/// 所有者を書く（置き換え）。`owner_id` 0 と、同じ write txn 内で自テナントに現存しないロールは拒否する
/// （所有者の保存と並行する `DROP ROLE` で、存在しないロールを所有者にする孤児行を作らない）。
pub(crate) fn set_owner_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    object: &PrivilegeObject,
    owner_id: u64,
) -> Result<()> {
    validate_tenant(tenant)?;
    if owner_id == 0 {
        return Err(CatalogError::Invalid("invalid object owner".to_string()));
    }
    if get_role_by_id_in_txn(write_txn, tenant, owner_id)?.is_none() {
        return Err(CatalogError::Invalid(
            "object owner role does not exist".to_string(),
        ));
    }
    set_owner_unchecked_in_txn(write_txn, tenant, object, owner_id)
}

/// ロールの現存を確認せずに所有者を書く（`set_owner_in_txn` の本体。ロール表を持たない単体テスト用に
/// 分けてある。本番経路は必ず [`set_owner_in_txn`] を使う）。
pub(crate) fn set_owner_unchecked_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    object: &PrivilegeObject,
    owner_id: u64,
) -> Result<()> {
    validate_tenant(tenant)?;
    if owner_id == 0 {
        return Err(CatalogError::Invalid("invalid object owner".to_string()));
    }
    let (kind, key) = owner_components(object)?;
    let mut table = write_txn.open_table(OWNERS_TABLE)?;
    table.insert(
        (kind, key.as_str(), tenant),
        encode_owner(owner_id).as_slice(),
    )?;
    Ok(())
}

/// 対象の所有者行（自テナント分）を消す（SCHEMA・関数の DROP から同じ write txn で呼ぶ）。
pub(crate) fn remove_owner_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    object: &PrivilegeObject,
) -> Result<()> {
    validate_tenant(tenant)?;
    let (kind, key) = owner_components(object)?;
    if owner_of_in_txn(write_txn, tenant, object)?.is_none() {
        return Ok(());
    }
    let mut table = write_txn.open_table(OWNERS_TABLE)?;
    table.remove((kind, key.as_str(), tenant))?;
    Ok(())
}

/// relation `(kind, key)` の全テナント分の行（テナント ID と blob）を最大 [`SWEEP_BATCH`] 件集める。
fn collect_relation_rows<R: ReadSource>(src: &R, key: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let table = match src.open_table(OWNERS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for entry in table.range((KIND_TABLE, key, "")..)? {
        let (k, v) = entry?;
        let (kind, k_key, tenant) = k.value();
        if kind != KIND_TABLE || k_key != key {
            break;
        }
        out.push((tenant.to_string(), v.value().to_vec()));
        if out.len() >= SWEEP_BATCH {
            break;
        }
    }
    Ok(out)
}

/// relation `relation_key` の全テナント分の所有者行を削除する（`DROP TABLE`・`DROP VIEW`・
/// `DROP MATERIALIZED VIEW` から同じ write txn で呼ぶ。同名の再作成へ所有者を引き継がせない）。
pub(crate) fn remove_relation_owners_in_txn(
    write_txn: &redb::WriteTransaction,
    relation_key: &str,
) -> Result<()> {
    loop {
        let rows = collect_relation_rows(write_txn, relation_key)?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut table = write_txn.open_table(OWNERS_TABLE)?;
        for (tenant, _) in &rows {
            table.remove((KIND_TABLE, relation_key, tenant.as_str()))?;
        }
    }
}

/// relation の改名に追随して全テナント分の所有者行を新しいキーへ移す（改名先に残った孤児は先に消す）。
pub(crate) fn rename_relation_owners_in_txn(
    write_txn: &redb::WriteTransaction,
    old_key: &str,
    new_key: &str,
) -> Result<()> {
    if old_key == new_key {
        return Ok(());
    }
    remove_relation_owners_in_txn(write_txn, new_key)?;
    loop {
        let rows = collect_relation_rows(write_txn, old_key)?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut table = write_txn.open_table(OWNERS_TABLE)?;
        for (tenant, blob) in &rows {
            table.remove((KIND_TABLE, old_key, tenant.as_str()))?;
            table.insert((KIND_TABLE, new_key, tenant.as_str()), blob.as_slice())?;
        }
    }
}

/// `tenant` で `role_id` を所有者とするオブジェクトがあるか（`DROP ROLE` の `2BP01` 判定。`catalog::role` の
/// `role_has_dependents_in_txn` から呼ばれる）。破損行は読み飛ばさず `Err`（fail-closed）。
pub(crate) fn owner_references_role_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    role_id: u64,
) -> Result<bool> {
    validate_tenant(tenant)?;
    if role_id == 0 {
        return Ok(false);
    }
    let table = match src.open_table(OWNERS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    for entry in table.iter()? {
        let (k, v) = entry?;
        let (_, _, row_tenant) = k.value();
        if row_tenant != tenant {
            continue;
        }
        if decode_owner(v.value())? == role_id {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 作成と同じ write txn で書く所有者の予約（Issue #2236）。
///
/// 作成の commit 後に別 txn で所有者を書くと、その間に別セッションが DROP して同名を作り直した場合に
/// 作り直された対象へ元の作成者が所有者として記録されてしまう。これを避けるため、作成者は作成の直前に
/// 本予約を置き、`Storage` の作成本体（write txn の中）が「新規に作った」ことを確認できた時点で
/// [`apply_pending_relation_owner_in_txn`]・[`take_pending_function_owner`] が同じ txn へ所有者行を書く。
/// 予約はスレッドローカルで、作成の呼び出し（同一スレッドで同期的に完結する）を囲む
/// [`PendingOwnerGuard`] の寿命の間だけ有効になる。名前（キー）が一致しない作成には作用しない。
#[derive(Debug, Clone)]
struct PendingOwner {
    tenant: String,
    key: String,
    role: u64,
}

thread_local! {
    static PENDING_OWNER: std::cell::RefCell<Option<PendingOwner>> =
        const { std::cell::RefCell::new(None) };
}

/// 予約の寿命を表す guard。drop で予約を解除する（エラー・panic でも残さない）。
#[must_use = "dropping the guard immediately disarms the pending owner"]
pub(crate) struct PendingOwnerGuard(());

impl Drop for PendingOwnerGuard {
    fn drop(&mut self) {
        PENDING_OWNER.with(|p| *p.borrow_mut() = None);
    }
}

/// `tenant` の `key`（relation キーまたは関数キー）を新規作成する直前に呼び、作成者 `role` を所有者として
/// 予約する。`role` 0（PUBLIC・未束縛）は予約しない（所有者の無い対象は運用者権限でしか扱えない＝拒否側）。
pub(crate) fn arm_pending_owner(tenant: &str, key: &str, role: Option<u64>) -> PendingOwnerGuard {
    let pending = role.filter(|r| *r != 0).map(|role| PendingOwner {
        tenant: tenant.to_string(),
        key: key.to_string(),
        role,
    });
    PENDING_OWNER.with(|p| *p.borrow_mut() = pending);
    PendingOwnerGuard(())
}

/// 予約が `key` に一致するときだけ `(tenant, role)` を返す。
fn pending_for(key: &str) -> Option<(String, u64)> {
    PENDING_OWNER.with(|p| {
        p.borrow()
            .as_ref()
            .filter(|pend| pend.key == key)
            .map(|pend| (pend.tenant.clone(), pend.role))
    })
}

/// relation `key` を **この write txn で新規に作った**ときに、予約された作成者を所有者として同じ txn へ書く
/// （表・ビュー・matview の作成本体から呼ぶ。予約が無い・名前が違えば何もしない）。
pub(crate) fn apply_pending_relation_owner_in_txn(
    write_txn: &redb::WriteTransaction,
    key: &str,
) -> Result<()> {
    let Some((tenant, role)) = pending_for(key) else {
        return Ok(());
    };
    set_owner_if_absent_in_txn(
        write_txn,
        &tenant,
        &PrivilegeObject::Table(key.to_string()),
        role,
    )
}

/// 関数 `function_id`（キー `key`）を **この write txn で新規に作った**ときに、予約された作成者を所有者として
/// 同じ txn へ書く（関数の作成本体から呼ぶ。`key` は正規化済みで、予約側も正規化して比較する）。
pub(crate) fn apply_pending_function_owner_in_txn(
    write_txn: &redb::WriteTransaction,
    scope: &str,
    key: &str,
    function_id: u64,
) -> Result<()> {
    let Some((tenant, role)) = PENDING_OWNER.with(|p| {
        p.borrow()
            .as_ref()
            .filter(|pend| pend.key.eq_ignore_ascii_case(key) && pend.tenant == scope)
            .map(|pend| (pend.tenant.clone(), pend.role))
    }) else {
        return Ok(());
    };
    set_owner_if_absent_in_txn(
        write_txn,
        &tenant,
        &PrivilegeObject::Function(function_id),
        role,
    )
}

/// 所有者行が無いときだけ、ロールの現存を確認して書く（`CREATE OR REPLACE` が既存の所有者を変えない）。
fn set_owner_if_absent_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    object: &PrivilegeObject,
    owner_id: u64,
) -> Result<()> {
    if owner_of_in_txn(write_txn, tenant, object)?.is_some() {
        return Ok(());
    }
    set_owner_in_txn(write_txn, tenant, object, owner_id)
}

#[cfg(test)]
mod tests;
