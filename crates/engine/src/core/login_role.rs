//! カタログのロールによる認証の入口（SQL-39・WIRE-18、Issue #2233。設計記録:
//! `docs/design/role-privilege-model.md` の「Issue #2233 実装記録」）。
//!
//! 役割: wire-server の SCRAM 認証が、接続の user（ログイン識別子）から所属テナントと検証子・属性を
//! 引くための公開 API だけを担う。認証の照合そのもの（SCRAM の往復・モック検証子・遅延・`28P01`／`28000`／
//! `53300` の送出）は wire-server 側の責務で、本ファイルは判定をしない。呼び出し元は wire-server の
//! `handshake.rs`（認証）と `main.rs`（起動時の後追い登録・`login-identifier` サブコマンド）。
//!
//! fail-closed: 破損・読み取り失敗は内容を持たない [`LoginLookupError`] に畳み、呼び出し側が「未知の
//! ロール」と同じ経路（モック検証子）へ倒せるようにする。検証子・tenant_id は Debug／エラーに出さない。

use super::EngineCore;
use crate::catalog::role;

/// ログイン識別子の解決に失敗した（破損・読み取り失敗・不正な入力）。内容は持たない（固定文言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoginLookupError;

impl std::fmt::Display for LoginLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("login lookup failed")
    }
}

impl std::error::Error for LoginLookupError {}

/// 認証に必要なロールの資格情報（平文は持たない）。`Debug` は伏せ字にする。
#[derive(Clone, PartialEq, Eq)]
pub struct LoginRoleCredentials {
    /// ロールが属するテナント（接続のテナントはこの値だけで決まる。クライアントの申告は使わない）。
    pub tenant_id: String,
    /// ロールの内部 ID。
    pub role_id: u64,
    /// 修飾子を含まない生のロール名（`"$user"` の展開用）。
    pub role_name: String,
    /// 保存済みの SCRAM 検証子（`SCRAM-SHA-256$...`。空は未設定＝パスワードなし）。
    pub scram_verifier: String,
    /// `LOGIN` 属性（偽のロールは検証の後に拒否する）。
    pub login: bool,
    /// `VALID UNTIL`（UTC マイクロ秒。`None` は期限なし）。
    pub valid_until: Option<i64>,
    /// `CONNECTION LIMIT`（-1 は無制限）。
    pub connection_limit: i32,
}

impl std::fmt::Debug for LoginRoleCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoginRoleCredentials(<redacted>)")
    }
}

impl LoginRoleCredentials {
    /// `VALID UNTIL` を過ぎている（または時計を読めない）なら真。期限ちょうども失効とする。
    /// 時計が読めないときは失効側へ倒す（fail-closed）。
    pub fn is_expired(&self) -> bool {
        match self.valid_until {
            None => false,
            Some(until) => match crate::sql::time_fn::now_micros() {
                Ok(now) => until <= now,
                Err(_) => true,
            },
        }
    }
}

impl EngineCore {
    /// `identifier` がカタログのログイン識別子の正規形か（形だけの判定。存在は調べない）。認証経路の
    /// 振り分け（カタログか users ファイルか）に使う。入力の形だけで決まるため存在のオラクルにならない。
    pub fn is_login_identifier(identifier: &str) -> bool {
        role::decode_login_identifier(identifier).is_ok()
    }

    /// テナントとロール名から、接続の user に渡すログイン識別子を作る（DB は開かない純関数）。
    pub fn login_identifier(tenant_id: &str, role_name: &str) -> Result<String, LoginLookupError> {
        role::login_identifier_for(tenant_id, role_name).map_err(|_| LoginLookupError)
    }

    /// ログイン識別子からロールの資格情報を引く。未登録・形が不正は `Ok(None)`。NOLOGIN のロールも返す
    /// （検証の後に拒否するため）。破損・読み取り失敗は `Err`（呼び出し側はモック検証子へ倒す）。
    pub fn resolve_login_role(
        &self,
        identifier: &str,
    ) -> Result<Option<LoginRoleCredentials>, LoginLookupError> {
        if !Self::is_login_identifier(identifier) {
            return Ok(None);
        }
        let resolved = self
            .storage
            .resolve_login(identifier)
            .map_err(|_| LoginLookupError)?;
        let Some(resolved) = resolved else {
            return Ok(None);
        };
        let scram_verifier =
            String::from_utf8(resolved.role.scram_verifier).map_err(|_| LoginLookupError)?;
        Ok(Some(LoginRoleCredentials {
            tenant_id: resolved.tenant,
            role_id: resolved.role.id,
            role_name: resolved.role.name,
            scram_verifier,
            login: resolved.role.attributes.login,
            valid_until: resolved.role.valid_until,
            connection_limit: resolved.role.connection_limit,
        }))
    }

    /// ログイン名対応表の後追い登録（起動時に 1 回。冪等。登録した件数を返す）。失敗したら起動を拒否する
    /// （fail-closed）。#1962 以降に作られた既存のロールを対応表へ載せるために使う。
    pub fn sync_login_names(&self) -> Result<usize, LoginLookupError> {
        self.storage
            .sync_login_names()
            .map_err(|_| LoginLookupError)
    }
}
