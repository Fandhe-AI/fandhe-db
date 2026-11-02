//! users ファイルからの初期ロールのブートストラップ（SQL-39 (3)(5)・WIRE-2・HTTP-4 のポインタ、Issue #2238。
//! 設計記録: `docs/design/role-privilege-model.md` の D3・D4・D11・D12 と「Issue #2238 実装記録」）。
//!
//! 役割: wire-server が起動時に 1 回だけ渡す `--users` ファイルの行を、テナント単位の状態表に従って
//! カタログへ反映する。以後の認証・権限はカタログを正とし、ファイルとの食い違いでは権限を広げない。
//! 呼び出し元は `core::bootstrap`（`EngineCore::bootstrap_users_file`）で、本ファイルは Storage 水準の
//! 読み書きだけを担う（認証の照合・wire の応答は wire-server の責務）。
//!
//! 状態表（テナントごと。毎回の起動で同じ結論になり冪等）:
//! - マーカーあり → 何も書かない。食い違いを検出して報告だけする（全ロールを消しても再投入しない）。
//! - マーカーなし・ロール 1 件以上 → 採用: マーカーだけを補う。同名ロールが資格情報を全く持たない場合に限り
//!   ファイルの資格情報を補い、`--ddl-allowed-users` の列挙には DDL フラグの行を書く（全権限は付けない）。
//! - マーカーなし・ロール 0 件 → 投入: 全行から初期ロールを作り、初期権限の行とマーカーを書く。
//!
//! 全テナント分を 1 つの write txn・1 回の commit で行い、途中で失敗したら何も書かない（部分投入を残さない）。
//!
//! 表（遅延作成。読み取りの `TableDoesNotExist` は空として扱う。旧 DB はそのまま「マーカーなし」になる）:
//! - [`BOOTSTRAP_MARKER_TABLE`]: `tenant_id` → マーカー blob
//! - [`BOOTSTRAP_PRIVILEGES_TABLE`]: `(tenant_id, role_id)` → 初期権限 blob（ロール blob の属性ビットには載せない。
//!   旧バイナリの decode を壊さず、SQL の `RoleAttributes` を汚さないため）
//!
//! fail-closed: 未知の版・予約ビット・切断は固定文言の `CorruptSchema`。検証子・tenant_id は
//! エラー・Debug に出さない。

use std::collections::BTreeMap;

use redb::{ReadableTable, TableDefinition};

use crate::storage::read_source::ReadSource;

use super::{
    corrupt, create_role_in_txn, get_role_in_txn, tenant_has_roles_in_txn, validate_identifier,
    validate_tenant, validate_verifier, RoleAttributes, RoleDef, MAX_ROLES_PER_TENANT, ROLES_TABLE,
};
use crate::catalog::{CatalogError, Result, Storage};

/// テナント単位のブートストラップ済みマーカー表。キーは `tenant_id`、値はマーカー blob。
pub(crate) const BOOTSTRAP_MARKER_TABLE: TableDefinition<&str, &[u8]> =
    TableDefinition::new("role_bootstrap_markers");

/// 初期権限表。キーは `(tenant_id, role_id)`、値は初期権限 blob。
pub(crate) const BOOTSTRAP_PRIVILEGES_TABLE: TableDefinition<(&str, u64), &[u8]> =
    TableDefinition::new("role_bootstrap_privileges");

const BLOB_VERSION: u8 = 1;
const FLAG_ALL_OBJECT_PRIVILEGES: u8 = 1 << 0;
const FLAG_DDL: u8 = 1 << 1;
const FLAG_ALL: u8 = FLAG_ALL_OBJECT_PRIVILEGES | FLAG_DDL;

/// 1 回の起動で受け付ける行数の上限（`--users` の巨大入力による無制限確保の防止）。
const MAX_BOOTSTRAP_USERS: usize = 100_000;

/// 初期ロールが持つ権限（ロール属性とは別に永続化する。ADR D12）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BootstrapPrivileges {
    /// テナントのオブジェクト（表・ビュー・SCHEMA 等）への全権限（SQL-39 (5)）。`SET ROLE` で別ロールへ
    /// 切り替えた後は効かない（セッションのロール本人にだけ作用する）。
    pub all_object_privileges: bool,
    /// DDL 実行権限（`--ddl-allowed-users` の初回の写像）。実行時の DDL は CLI の列挙との積で判定する。
    pub ddl: bool,
}

impl BootstrapPrivileges {
    fn to_flags(self) -> u8 {
        let mut f = 0;
        if self.all_object_privileges {
            f |= FLAG_ALL_OBJECT_PRIVILEGES;
        }
        if self.ddl {
            f |= FLAG_DDL;
        }
        f
    }

    fn from_flags(flags: u8) -> Result<Self> {
        if flags & !FLAG_ALL != 0 {
            return Err(corrupt());
        }
        Ok(Self {
            all_object_privileges: flags & FLAG_ALL_OBJECT_PRIVILEGES != 0,
            ddl: flags & FLAG_DDL != 0,
        })
    }
}

/// ブートストラップへ渡す users ファイル 1 行分（wire-server が組み立てる。平文は持たない）。
/// `Debug` は伏せ字にする。
#[derive(Clone)]
pub struct BootstrapUser {
    /// ファイル上の行番号（起動ログ用。1 起算）。
    pub line: usize,
    pub username: String,
    pub tenant_id: String,
    /// Argon2id の PHC 文字列（`legacy_phc` へ取り込む）。
    pub phc: String,
    /// SCRAM 検証子（`scram_verifier` へ取り込む。なければ `None`）。
    pub scram_verifier: Option<String>,
    /// `--ddl-allowed-users` に列挙されているか。
    pub ddl_listed: bool,
}

impl std::fmt::Debug for BootstrapUser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BootstrapUser(<redacted>)")
    }
}

/// 行ごとの処理結果の区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapEntryKind {
    /// 投入でロールを作った。
    Created,
    /// 採用（既存テナント）で、同名ロールへ資格情報を補った。
    AdoptedCredentials,
    /// 採用で同名ロールを確認した（資格情報は触っていない）。
    Adopted,
    /// マーカーのあるテナント（採用を含む）で同名ロールが無い。ロールは作らない。
    RoleMissing,
    /// ファイルの資格情報がカタログの値と異なる。カタログを使う。
    CredentialMismatch,
    /// `--ddl-allowed-users` に列挙されているが、ロールに DDL フラグが無い。DDL は広げない。
    DdlNotApplied,
    /// ロールに DDL フラグがあるが列挙から外れている。DDL は狭まる。
    DdlNarrowed,
    /// 識別子として不正な username（採用・マーカーありのテナントでは警告に留める）。
    InvalidName,
}

/// 行ごとの結果（行番号と区分だけ。テナント・資格情報は含まない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootstrapEntry {
    pub line: usize,
    pub kind: BootstrapEntryKind,
}

/// 起動時のブートストラップの結果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BootstrapReport {
    pub entries: Vec<BootstrapEntry>,
    /// 投入したテナント数。
    pub tenants_bootstrapped: usize,
    /// 採用（マーカーだけを補った）テナント数。
    pub tenants_adopted: usize,
}

/// ブートストラップの失敗（固定文言。資格情報・tenant_id は含めない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootstrapError {
    /// 投入対象のテナントの行が識別子・件数の制約を満たさない（行番号だけを運ぶ。何も書かない）。
    InvalidUser { line: usize },
    /// 入力が上限を超える・値が不正。
    InvalidInput,
    /// カタログの読み書きに失敗した（破損を含む。何も書かない）。
    Catalog,
}

impl std::fmt::Display for BootstrapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidUser { line } => {
                write!(f, "users file line {line} cannot be bootstrapped as a role")
            }
            Self::InvalidInput => f.write_str("invalid bootstrap input"),
            Self::Catalog => f.write_str("role catalog bootstrap failed"),
        }
    }
}

impl std::error::Error for BootstrapError {}

impl From<redb::TableError> for BootstrapError {
    fn from(_: redb::TableError) -> Self {
        Self::Catalog
    }
}

impl From<redb::StorageError> for BootstrapError {
    fn from(_: redb::StorageError) -> Self {
        Self::Catalog
    }
}

impl From<CatalogError> for BootstrapError {
    fn from(_: CatalogError) -> Self {
        Self::Catalog
    }
}

/// テナントのマーカーがあるか。表が無ければ `false`。blob が不正なら `CorruptSchema`。
pub(crate) fn tenant_bootstrapped_in_txn<R: ReadSource>(src: &R, tenant: &str) -> Result<bool> {
    validate_tenant(tenant)?;
    let table = match src.open_table(BOOTSTRAP_MARKER_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let found = table.get(tenant)?;
    match found {
        Some(v) => {
            if v.value() != [BLOB_VERSION] {
                return Err(corrupt());
            }
            Ok(true)
        }
        None => Ok(false),
    }
}

/// ロールの初期権限を引く（行が無ければ権限なし）。
pub(crate) fn bootstrap_privileges_in_txn<R: ReadSource>(
    src: &R,
    tenant: &str,
    role_id: u64,
) -> Result<BootstrapPrivileges> {
    validate_tenant(tenant)?;
    let table = match src.open_table(BOOTSTRAP_PRIVILEGES_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => {
            return Ok(BootstrapPrivileges::default());
        }
        Err(e) => return Err(e.into()),
    };
    let found = table.get((tenant, role_id))?;
    match found {
        Some(v) => match v.value() {
            [BLOB_VERSION, flags] => BootstrapPrivileges::from_flags(*flags),
            _ => Err(corrupt()),
        },
        None => Ok(BootstrapPrivileges::default()),
    }
}

fn put_privileges(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    role_id: u64,
    privileges: BootstrapPrivileges,
) -> Result<()> {
    let blob = [BLOB_VERSION, privileges.to_flags()];
    let mut table = write_txn.open_table(BOOTSTRAP_PRIVILEGES_TABLE)?;
    table.insert((tenant, role_id), blob.as_slice())?;
    Ok(())
}

fn put_marker(write_txn: &redb::WriteTransaction, tenant: &str) -> Result<()> {
    let blob = [BLOB_VERSION];
    let mut table = write_txn.open_table(BOOTSTRAP_MARKER_TABLE)?;
    table.insert(tenant, blob.as_slice())?;
    Ok(())
}

/// `DROP ROLE` が同じ txn でロールの初期権限の行を消す（`drop_role_in_txn` から呼ぶ。行が無ければ何もしない）。
pub(crate) fn remove_bootstrap_privileges_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    role_id: u64,
) -> Result<()> {
    match ReadSource::open_table(write_txn, BOOTSTRAP_PRIVILEGES_TABLE) {
        Ok(t) => {
            if t.get((tenant, role_id))?.is_none() {
                return Ok(());
            }
        }
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(()),
        Err(e) => return Err(e.into()),
    }
    let mut table = write_txn.open_table(BOOTSTRAP_PRIVILEGES_TABLE)?;
    table.remove((tenant, role_id))?;
    Ok(())
}

/// 投入で作る初期ロールの定義（`LOGIN`・`CREATEROLE`・`INHERIT`。ADR D3）。
fn initial_role_def(user: &BootstrapUser) -> RoleDef {
    RoleDef {
        id: 0,
        name: user.username.clone(),
        attributes: RoleAttributes {
            login: true,
            inherit: true,
            createrole: true,
            createdb: false,
        },
        connection_limit: -1,
        valid_until: None,
        scram_verifier: user
            .scram_verifier
            .as_deref()
            .unwrap_or_default()
            .as_bytes()
            .to_vec(),
        legacy_phc: user.phc.as_bytes().to_vec(),
    }
}

/// マーカーのあるテナントの 1 行を点検して食い違いを報告する（読み取りだけ）。
fn inspect_user<R: ReadSource>(
    src: &R,
    tenant: &str,
    user: &BootstrapUser,
    out: &mut Vec<BootstrapEntry>,
) -> Result<()> {
    let line = user.line;
    if validate_identifier(&user.username).is_err() {
        out.push(BootstrapEntry {
            line,
            kind: BootstrapEntryKind::InvalidName,
        });
        return Ok(());
    }
    let Some(role) = get_role_in_txn(src, tenant, &user.username)? else {
        out.push(BootstrapEntry {
            line,
            kind: BootstrapEntryKind::RoleMissing,
        });
        return Ok(());
    };
    let file_scram = user
        .scram_verifier
        .as_deref()
        .unwrap_or_default()
        .as_bytes();
    let phc_differs = role.legacy_phc != user.phc.as_bytes();
    let scram_differs = !file_scram.is_empty() && role.scram_verifier != file_scram;
    if phc_differs || scram_differs {
        out.push(BootstrapEntry {
            line,
            kind: BootstrapEntryKind::CredentialMismatch,
        });
    }
    let privileges = bootstrap_privileges_in_txn(src, tenant, role.id)?;
    if user.ddl_listed && !privileges.ddl {
        out.push(BootstrapEntry {
            line,
            kind: BootstrapEntryKind::DdlNotApplied,
        });
    } else if !user.ddl_listed && privileges.ddl {
        out.push(BootstrapEntry {
            line,
            kind: BootstrapEntryKind::DdlNarrowed,
        });
    }
    Ok(())
}

/// 全テナント分のブートストラップを `write_txn` の中で行う（commit は呼び出し側。失敗時は呼び出し側が
/// txn を捨てるので何も残らない）。
pub(crate) fn bootstrap_in_txn(
    write_txn: &redb::WriteTransaction,
    users: &[BootstrapUser],
) -> std::result::Result<BootstrapReport, BootstrapError> {
    if users.len() > MAX_BOOTSTRAP_USERS {
        return Err(BootstrapError::InvalidInput);
    }
    let mut by_tenant: BTreeMap<&str, Vec<&BootstrapUser>> = BTreeMap::new();
    for u in users {
        by_tenant.entry(u.tenant_id.as_str()).or_default().push(u);
    }
    let mut report = BootstrapReport::default();
    for (tenant, mut rows) in by_tenant {
        validate_tenant(tenant).map_err(|_| BootstrapError::InvalidInput)?;
        rows.sort_by_key(|u| u.line);
        let marked = tenant_bootstrapped_in_txn(write_txn, tenant)?;
        if marked {
            for u in rows {
                inspect_user(write_txn, tenant, u, &mut report.entries)?;
            }
            continue;
        }
        if tenant_has_roles_in_txn(write_txn, tenant)? {
            adopt_tenant(write_txn, tenant, &rows, &mut report)?;
            report.tenants_adopted += 1;
        } else {
            bootstrap_tenant(write_txn, tenant, &rows, &mut report)?;
            report.tenants_bootstrapped += 1;
        }
    }
    Ok(report)
}

/// 採用: マーカーを補い、同名ロールへ DDL フラグ（列挙された行のみ）と、資格情報が空の場合の取り込みを行う。
fn adopt_tenant(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    rows: &[&BootstrapUser],
    report: &mut BootstrapReport,
) -> std::result::Result<(), BootstrapError> {
    put_marker(write_txn, tenant)?;
    for u in rows {
        let line = u.line;
        if validate_identifier(&u.username).is_err() {
            report.entries.push(BootstrapEntry {
                line,
                kind: BootstrapEntryKind::InvalidName,
            });
            continue;
        }
        let Some(role) = get_role_in_txn(write_txn, tenant, &u.username)? else {
            report.entries.push(BootstrapEntry {
                line,
                kind: BootstrapEntryKind::RoleMissing,
            });
            continue;
        };
        let mut kind = BootstrapEntryKind::Adopted;
        if role.scram_verifier.is_empty()
            && role.legacy_phc.is_empty()
            && validate_verifier(u.phc.as_bytes())
        {
            let mut next = role.clone();
            next.legacy_phc = u.phc.as_bytes().to_vec();
            if let Some(v) = u.scram_verifier.as_deref() {
                if validate_verifier(v.as_bytes()) {
                    next.scram_verifier = v.as_bytes().to_vec();
                }
            }
            let blob = super::encode_role_def(&next)?;
            let mut table = write_txn.open_table(ROLES_TABLE)?;
            table.insert((tenant, u.username.as_str()), blob.as_slice())?;
            kind = BootstrapEntryKind::AdoptedCredentials;
        } else if !role.scram_verifier.is_empty() || !role.legacy_phc.is_empty() {
            // 資格情報を持つ既存ロールの採用: ファイルの値とカタログの値が異なれば報告する
            // （カタログが正。ファイルの値は使わない）。
            let file_scram = u.scram_verifier.as_deref().unwrap_or_default().as_bytes();
            let phc_differs = role.legacy_phc != u.phc.as_bytes();
            let scram_differs = !file_scram.is_empty() && role.scram_verifier != file_scram;
            if phc_differs || scram_differs {
                kind = BootstrapEntryKind::CredentialMismatch;
            }
        }
        if u.ddl_listed {
            put_privileges(
                write_txn,
                tenant,
                role.id,
                BootstrapPrivileges {
                    all_object_privileges: false,
                    ddl: true,
                },
            )?;
        }
        report.entries.push(BootstrapEntry { line, kind });
    }
    Ok(())
}

/// 投入: 全行から初期ロールを作る。不正な行・件数上限の超過があれば全体を失敗させる（何も書かない）。
fn bootstrap_tenant(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    rows: &[&BootstrapUser],
    report: &mut BootstrapReport,
) -> std::result::Result<(), BootstrapError> {
    if rows.len() > MAX_ROLES_PER_TENANT {
        return Err(BootstrapError::InvalidUser {
            line: rows.get(MAX_ROLES_PER_TENANT).map_or(0, |u| u.line),
        });
    }
    for u in rows {
        let def = initial_role_def(u);
        let shape_ok = validate_identifier(&u.username).is_ok()
            && validate_verifier(&def.legacy_phc)
            && validate_verifier(&def.scram_verifier);
        if !shape_ok {
            return Err(BootstrapError::InvalidUser { line: u.line });
        }
        let id = create_role_in_txn(write_txn, tenant, &def)
            .map_err(|_| BootstrapError::InvalidUser { line: u.line })?;
        put_privileges(
            write_txn,
            tenant,
            id,
            BootstrapPrivileges {
                all_object_privileges: true,
                ddl: u.ddl_listed,
            },
        )?;
        report.entries.push(BootstrapEntry {
            line: u.line,
            kind: BootstrapEntryKind::Created,
        });
    }
    put_marker(write_txn, tenant)?;
    Ok(())
}

impl Storage {
    /// users ファイルの行を単一 write txn・1 回の commit でカタログへ反映する（起動時に 1 回。冪等）。
    /// 失敗したら何も書かない。表の世代は進めない（ロール専用の表だけを書く）。
    pub(crate) fn bootstrap_users_file(
        &self,
        users: &[BootstrapUser],
    ) -> std::result::Result<BootstrapReport, BootstrapError> {
        let write_txn = self
            .begin_write_txn()
            .map_err(super::super::convert_storage_error)?;
        let report = bootstrap_in_txn(&write_txn, users)?;
        let wrote = report.tenants_bootstrapped + report.tenants_adopted > 0;
        if wrote {
            crate::recovery::commit_boundary::commit(write_txn)
                .map_err(super::super::convert_storage_error)?;
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests;
