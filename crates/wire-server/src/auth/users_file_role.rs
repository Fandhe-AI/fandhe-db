//! users ファイルで認証したユーザーとカタログのロールの対応づけ（SQL-39・WIRE-2、Issue #1971。設計記録:
//! `docs/design/role-privilege-model.md` の「Issue #1971 実装記録」）。
//!
//! 役割: パスワード照合に成功した users ファイルのユーザーを、その行のテナント内にある同名のロールへ
//! 対応づけ、セッションへ束縛するための判定を 1 か所に集める。呼び出し元は `handshake.rs` の SCRAM 経路
//! （証明の検証後）と cleartext 経路（[`verify_and_bind`]）。engine の
//! [`engine::core::EngineCore::resolve_users_file_role`] を呼んで 3 値を受け、wire の応答に直結する
//! 判定（`28P01` へ倒すか否か）だけを本ファイルが担う。
//!
//! fail-closed: 「カタログが正」のテナントで同名ロールが無い・期限切れ・読み取り失敗は、すべて
//! [`UsersFileBinding::Reject`] に倒し、呼び出し側が誤パスワードと同じ `28P01`・同じ遅延へ収束させる。
//! `NOLOGIN` は検証後の `28000` で扱うためここでは拒否しない。テナント・ユーザー名はログに出さない。

use std::time::Instant;

use engine::core::{EngineCore, UsersFileRoleBinding};

use super::{AuthFailure, UserStore, AUTH_FAILURE_DELAY};

/// 対応づけできたロール（セッションへ束縛する内部 ID と、検証後の判定に使う属性）。`Debug` は伏せ字。
pub(crate) struct BoundUsersFileRole {
    pub(crate) role_id: u64,
    pub(crate) role_name: String,
    pub(crate) login: bool,
    pub(crate) connection_limit: i32,
}

impl std::fmt::Debug for BoundUsersFileRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BoundUsersFileRole(<redacted>)")
    }
}

/// 対応づけの判定結果。
#[derive(Debug)]
pub(crate) enum UsersFileBinding {
    /// 束縛しない（カタログが正ではない、または `engine` が無い。従来どおり）。
    Unbound,
    /// 同名ロールへ束縛する。
    Bound(BoundUsersFileRole),
    /// 認証失敗として扱う（呼び出し側は誤パスワードと同じ応答へ畳む）。
    Reject,
}

/// 照合済みの `(tenant_id, username)` に対応するロールを判定する（J1〜J4・J8・J9）。
pub(crate) fn bind_users_file_user(
    engine: Option<&EngineCore>,
    tenant_id: &str,
    username: &str,
) -> UsersFileBinding {
    let Some(engine) = engine else {
        return UsersFileBinding::Unbound;
    };
    match engine.resolve_users_file_role(tenant_id, username) {
        Ok(UsersFileRoleBinding::CatalogNotAuthoritative) => UsersFileBinding::Unbound,
        Ok(UsersFileRoleBinding::Missing) => UsersFileBinding::Reject,
        Ok(UsersFileRoleBinding::Role(c)) => {
            if c.is_expired() {
                UsersFileBinding::Reject
            } else {
                UsersFileBinding::Bound(BoundUsersFileRole {
                    role_id: c.role_id,
                    role_name: c.role_name,
                    login: c.login,
                    connection_limit: c.connection_limit,
                })
            }
        }
        Err(_) => {
            // テナント・ユーザーは記録しない（固定文言）。
            engine::log_stderr!(
                "wire-server: role catalog lookup failed while binding a users-file user; treating as authentication failure"
            );
            UsersFileBinding::Reject
        }
    }
}

/// cleartext 用の「パスワード照合＋ロール対応づけ」。照合の前に起点を取り、対応づけの失敗でも
/// [`AUTH_FAILURE_DELAY`] に達するまで待つ（誤パスワードと遅延で区別できない。J6）。戻りの
/// [`UsersFileBinding`] は `Unbound` か `Bound` のみ。HTTP のセッション確立からも再利用できる形にしてある
/// （HTTP-4 の結線は #2238 の担当）。
pub(crate) fn verify_and_bind(
    store: &UserStore,
    engine: Option<&EngineCore>,
    username: &str,
    password: &[u8],
) -> Result<(engine::policy::PolicyContext, UsersFileBinding), AuthFailure> {
    let start = Instant::now();
    let ctx = super::verify(store, username, password)?;
    match bind_users_file_user(engine, ctx.tenant_id(), username) {
        UsersFileBinding::Reject => {
            let elapsed = start.elapsed();
            if elapsed < AUTH_FAILURE_DELAY {
                std::thread::sleep(AUTH_FAILURE_DELAY - elapsed);
            }
            Err(AuthFailure)
        }
        binding => Ok((ctx, binding)),
    }
}
