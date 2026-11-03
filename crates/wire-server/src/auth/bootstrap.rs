//! users ファイルの初期ブートストラップ（SQL-39 (3)(5)・WIRE-2・HTTP-4 のポインタ、Issue #2238。設計記録:
//! `docs/design/role-privilege-model.md` の「Issue #2238 実装記録」）。
//!
//! 役割: 起動時（`main.rs` が `sync_login_names` の後・listen の前）に 1 回だけ、`--users` ファイルの行を
//! engine の [`engine::core::EngineCore::bootstrap_users_file`] へ渡し、結果を運用者向けの起動ログ文へ
//! 変換する。投入・採用・不一致の判定は engine が担い、本ファイルは入力の組み立てと文言化だけを持つ。
//! 以後の認証・権限はカタログを正とし（`auth::users_file_role`・`handshake.rs`・HTTP セッション発行）、
//! ファイルとの食い違いは警告で可視化するだけで権限を広げない。
//!
//! 起動ログは運用者向けのため行番号と区分だけを出す。tenant_id・username・資格情報（PHC・検証子）は含めない。

use engine::core::{BootstrapEntryKind, BootstrapError, EngineCore};

use super::UserStore;

/// ブートストラップの結果（起動ログへ出す文）。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BootstrapSummary {
    /// 投入したテナント数。
    pub tenants_bootstrapped: usize,
    /// 採用（マーカーだけを補った）テナント数。
    pub tenants_adopted: usize,
    /// 運用者へ見せる警告文（ファイルとカタログの食い違い。固定書式）。
    pub warnings: Vec<String>,
}

/// 警告文（区分ごとの固定書式。行番号だけを含む）。情報提供だけの区分は `None`。
fn warning_for(line: usize, kind: BootstrapEntryKind) -> Option<String> {
    let text = match kind {
        BootstrapEntryKind::Created | BootstrapEntryKind::Adopted => return None,
        BootstrapEntryKind::AdoptedCredentials => return None,
        BootstrapEntryKind::RoleMissing => {
            "has no matching role in the catalog; the user cannot log in (create it with CREATE ROLE)"
        }
        BootstrapEntryKind::CredentialMismatch => {
            "credentials differ from the catalog; the catalog is authoritative and the file value is ignored"
        }
        BootstrapEntryKind::DdlNotApplied => {
            "is listed in --ddl-allowed-users but the role has no DDL grant; DDL is not granted"
        }
        BootstrapEntryKind::DdlNarrowed => {
            "has a DDL grant but is not listed in --ddl-allowed-users; DDL is not granted"
        }
        BootstrapEntryKind::LegacyEmptyTenantSkipped => {
            "belongs to a tenant with no roles in an existing catalog; the role is not created from the file (create it with CREATE ROLE)"
        }
        BootstrapEntryKind::InvalidName => {
            "is not a valid role name; the user cannot log in"
        }
    };
    Some(format!("wire-server: users file line {line} {text}"))
}

/// users ファイルの行をカタログへブートストラップする（冪等。`--ddl-allowed-users` の適用後に呼ぶ）。
/// 失敗したらカタログへ何も書かれないので、呼び出し側（`main.rs`）は起動を拒否する（fail-closed）。
pub fn bootstrap_users_file(
    store: &UserStore,
    engine: &EngineCore,
) -> Result<BootstrapSummary, BootstrapError> {
    let users = store.bootstrap_users();
    let report = engine.bootstrap_users_file(&users)?;
    let warnings = report
        .entries
        .iter()
        .filter_map(|e| warning_for(e.line, e.kind))
        .collect();
    Ok(BootstrapSummary {
        tenants_bootstrapped: report.tenants_bootstrapped,
        tenants_adopted: report.tenants_adopted,
        warnings,
    })
}
