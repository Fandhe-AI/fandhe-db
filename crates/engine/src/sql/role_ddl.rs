//! `CREATE ROLE`／`CREATE USER`／`DROP ROLE` の実行本体（SQL-39 のポインタ、Issue #1962。設計記録:
//! `docs/design/role-privilege-model.md`）。
//!
//! 役割: 構造検証済みの文（`sql::allowlist::{ValidatedCreateRole, ValidatedDropRole}`）をロールカタログ
//! （`catalog::role`）への書き込みへ写し、`CatalogError` を SQLSTATE の契約へ写像する。呼び出し元は
//! `core.rs` の `ParsedSql::{CreateRole, DropRole}` 腕で、DDL 権限ゲート（`sql::ddl::require_ddl_permission`。
//! 権限なしは対象の有無を問わず `42501`）と明示トランザクション内の `0A000` を通過した後にだけ来る。
//! `CREATEROLE`／`ADMIN OPTION` による判定への差し替えは #1966・#1971 の担当で、それまでは DDL 権限ゲートで
//! 代替する（ポリシー DDL と同じ方針）。
//!
//! パスワード: 平文は SCRAM-SHA-256 検証子（`engine::crypto::scram_verifier`。wire-server の認証と同一形式）
//! にして保存し、平文は保存もエラー応答への反映もしない。salt は CSPRNG（`crypto::random`）で作り、乱数源が
//! 読めなければロールを作らない（fail-closed）。
//!
//! ログイン名対応表（`catalog::role::register_login_in_txn`）へは登録しない: テナント修飾子を提供する経路が
//! まだ無く、カタログのロールで認証する経路（#2233）も無いため、`LOGIN` 属性はロール blob に保存するだけで
//! ログインはできない状態になる（fail-closed）。修飾子の提供と登録は #2233 の担当。
//!
//! テナント境界（P0）: scope は呼び出し元が渡す検証済みの `tenant_id` だけ。他テナントのロールは不在と
//! 同じ経路・同じ `42704` になる。エラー文言は固定で、内部 ID・tenant_id・依存元・パスワードを含めない。

use crate::catalog::role::{
    DropRolesError, RoleDef, ROLE_ALREADY_EXISTS_MESSAGE, ROLE_HAS_DEPENDENTS_MESSAGE,
};
use crate::catalog::CatalogError;
use crate::crypto::scram_verifier::{
    generate_verifier, GenerateVerifierError, ScramVerifier, SALT_LEN, SCRAM_ITERATIONS,
};
use crate::sql::allowlist::{
    RolePassword, SqlSurfaceError, ValidatedCreateRole, ValidatedDropRole,
};
use crate::storage::Storage;

/// パスワード（平文または検証子文字列）の最大バイト数。超過は `22023`。
const MAX_PASSWORD_BYTES: usize = 1024;

/// 検証子文字列の接頭辞（既にハッシュ済みの形で渡された場合の判別用）。
const VERIFIER_PREFIX: &str = "SCRAM-SHA-256$";

/// `CREATE ROLE`／`CREATE USER` の成功応答（結果を持たない。wire はコマンドタグ `CREATE ROLE` を返す）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CreateRoleOutcome {}

/// `DROP ROLE` の成功応答。`IF EXISTS` で 1 件以上が不在のため skip したかだけを持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DropRoleOutcome {
    pub(crate) skipped: bool,
}

impl DropRoleOutcome {
    /// `IF EXISTS` により対象不在の名前を skip した場合に真。
    pub fn skipped(&self) -> bool {
        self.skipped
    }
}

/// パスワードを保存用の検証子文字列にする。空文字列は「パスワード無し」（空）。
///
/// - `SCRAM-SHA-256$` で始まる文字列は既にハッシュ済みとみなし、構文・反復回数・salt 長が認証側
///   （wire-server の `UserStore`）の要求と一致するときだけそのまま保存する。満たさなければ平文として
///   扱わず `22023`
/// - `md5` + 16 進 32 桁は `0A000`（MD5 認証は持たない）
/// - それ以外は平文として CSPRNG の salt で検証子を生成する（印字可能 ASCII 以外は `22023`）
///
/// どのエラーにもパスワードの内容・長さを含めない。
fn build_verifier(password: Option<&RolePassword>) -> Result<Vec<u8>, SqlSurfaceError> {
    let Some(pw) = password else {
        return Ok(Vec::new());
    };
    let bytes = pw.as_bytes();
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    if bytes.len() > MAX_PASSWORD_BYTES {
        return Err(SqlSurfaceError::InvalidParameterValue {
            detail: "invalid password",
        });
    }
    if bytes.starts_with(VERIFIER_PREFIX.as_bytes()) {
        let parsed = std::str::from_utf8(bytes)
            .ok()
            .and_then(|s| ScramVerifier::parse(s).ok())
            .filter(|v| v.iterations == SCRAM_ITERATIONS && v.salt.len() == SALT_LEN)
            .ok_or(SqlSurfaceError::InvalidParameterValue {
                detail: "invalid password verifier",
            })?;
        return Ok(parsed.to_verifier_string().into_bytes());
    }
    let md5_hex = bytes.strip_prefix(b"md5").filter(|h| h.len() == 32);
    if md5_hex.is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit)) {
        return Err(SqlSurfaceError::feature_not_supported(
            "MD5 password verifiers are not supported",
        ));
    }
    let salt =
        crate::crypto::random::read_urandom(SALT_LEN).map_err(|_| SqlSurfaceError::Internal {
            detail: "password hashing failed".to_string(),
        })?;
    let verifier = generate_verifier(bytes, &salt, SCRAM_ITERATIONS).map_err(|e| match e {
        GenerateVerifierError::NonPrintableAsciiPassword => {
            SqlSurfaceError::InvalidParameterValue {
                detail: "invalid password",
            }
        }
        GenerateVerifierError::InvalidSaltLength => SqlSurfaceError::Internal {
            detail: "password hashing failed".to_string(),
        },
    })?;
    Ok(verifier.to_verifier_string().into_bytes())
}

/// `CREATE ROLE`／`CREATE USER` を実行する（権限・明示 Tx のゲートは呼び出し元で通過済み）。
pub(crate) fn execute_create_role(
    storage: &Storage,
    tenant_id: &str,
    stmt: &ValidatedCreateRole,
) -> Result<CreateRoleOutcome, SqlSurfaceError> {
    let scram_verifier = build_verifier(stmt.password.as_ref())?;
    let def = RoleDef {
        id: 0,
        name: stmt.name.clone(),
        attributes: stmt.attributes,
        connection_limit: -1,
        valid_until: None,
        scram_verifier,
        legacy_phc: Vec::new(),
    };
    storage
        .create_role(tenant_id, &def)
        .map_err(|e| map_role_error(e, &stmt.name))?;
    Ok(CreateRoleOutcome {})
}

/// `DROP ROLE` を実行する。全件を 1 つの write txn で処理し、途中で失敗したら全件取り消す。
pub(crate) fn execute_drop_role(
    storage: &Storage,
    tenant_id: &str,
    stmt: &ValidatedDropRole,
) -> Result<DropRoleOutcome, SqlSurfaceError> {
    match storage.drop_roles(tenant_id, &stmt.names, stmt.if_exists) {
        Ok(removed) => Ok(DropRoleOutcome {
            skipped: removed < stmt.names.len(),
        }),
        Err(DropRolesError::NotFound(name)) => Err(SqlSurfaceError::UndefinedRole { name }),
        Err(DropRolesError::HasDependents(name)) => {
            Err(SqlSurfaceError::RoleHasDependents { name })
        }
        Err(DropRolesError::Catalog(e)) => Err(map_role_error(
            e,
            stmt.names.first().map(String::as_str).unwrap_or_default(),
        )),
    }
}

/// ロールカタログの失敗を SQL 表層の契約へ写像する（同名の既存 `42710`・ロール不在 `42704`・依存あり
/// `2BP01`・件数上限 `54000`・形状不正 `22000`・ロック timeout `55P03`・その他 `XX000`）。文言は固定で、
/// 内部 ID・tenant_id・依存元・パスワードを含めない。`role_name` は利用者自身が書いた名前。
pub(crate) fn map_role_error(e: CatalogError, role_name: &str) -> SqlSurfaceError {
    match e {
        CatalogError::Invalid(m) if m == ROLE_ALREADY_EXISTS_MESSAGE => {
            SqlSurfaceError::DuplicateRole {
                name: role_name.to_string(),
            }
        }
        CatalogError::Invalid(m) if m == ROLE_HAS_DEPENDENTS_MESSAGE => {
            SqlSurfaceError::RoleHasDependents {
                name: role_name.to_string(),
            }
        }
        CatalogError::Invalid(_) => SqlSurfaceError::invalid_input("invalid role definition"),
        CatalogError::ViewLimitExceeded(_) => SqlSurfaceError::payload_too_large("too many roles"),
        CatalogError::WriteLockTimeout => SqlSurfaceError::LockNotAvailable,
        _ => SqlSurfaceError::Internal {
            detail: "role catalog operation failed".to_string(),
        },
    }
}

#[cfg(test)]
mod tests;
