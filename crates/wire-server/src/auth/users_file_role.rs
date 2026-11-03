//! users ファイルで認証したユーザーとカタログのロールの対応づけ（SQL-39・WIRE-2、Issue #1971。設計記録:
//! `docs/design/role-privilege-model.md` の「Issue #1971 実装記録」「Issue #2238 実装記録」）。
//!
//! 役割: パスワード照合に成功した users ファイルのユーザーを、その行のテナント内にある同名のロールへ
//! 対応づけ、セッションへ束縛するための判定を 1 か所に集める。呼び出し元は `handshake.rs` の SCRAM 経路
//! （証明の検証後）と cleartext 経路（[`verify_and_bind`]）、HTTP の `POST /v1/session`。engine の
//! [`engine::core::EngineCore::resolve_users_file_role`] を呼んで 3 値を受け、wire の応答に直結する
//! 判定（`28P01` へ倒すか否か）だけを本ファイルが担う。
//!
//! ブートストラップ後（Issue #2238）: テナントにマーカーがあるとき、素の username の資格情報は
//! ファイルの PHC・SCRAM 検証子ではなくカタログのロール（`legacy_phc`・`scram_verifier`）で照合する。
//! ファイルが担うのはテナントへの経路振り分けだけで、ファイルを書き換えても資格情報・権限は変わらない。
//!
//! fail-closed: 「カタログが正」のテナントで同名ロールが無い・期限切れ・読み取り失敗は、すべて
//! [`UsersFileBinding::Reject`] に倒し、呼び出し側が誤パスワードと同じ `28P01`・同じ遅延へ収束させる。
//! `NOLOGIN` は検証後の `28000` で扱うためここでは拒否しない。テナント・ユーザー名はログに出さない。

use std::sync::OnceLock;
use std::time::Instant;

use engine::core::{EngineCore, LoginRoleCredentials, UsersFileRoleBinding};

use super::{argon2id, scram, AuthFailure, UserStore, AUTH_FAILURE_DELAY};

/// 対応づけできたロール（セッションへ束縛する内部 ID と、検証後の判定に使う属性）。`Debug` は伏せ字。
pub(crate) struct BoundUsersFileRole {
    pub(crate) role_id: u64,
    pub(crate) role_name: String,
    pub(crate) login: bool,
    pub(crate) connection_limit: i32,
    /// テナントのブートストラップ済みマーカーがあるか（真なら資格情報の照合元はカタログ）。
    pub(crate) bootstrapped: bool,
    /// 初期ロールの全権限（セッションのオブジェクト権限の所有者相当。DDL は含まない）。
    pub(crate) all_object_privileges: bool,
    /// 初期ロールの永続の DDL フラグ（`--ddl-allowed-users` の列挙との積で DDL を許す）。
    pub(crate) ddl_flag: bool,
    /// 認証時点のカタログの SCRAM 検証子（SCRAM の証明の後に再取得して一致を確かめる）。
    pub(crate) scram_verifier: String,
}

impl std::fmt::Debug for BoundUsersFileRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BoundUsersFileRole(<redacted>)")
    }
}

impl BoundUsersFileRole {
    fn from_credentials(c: LoginRoleCredentials) -> Self {
        Self {
            role_id: c.role_id,
            role_name: c.role_name,
            login: c.login,
            connection_limit: c.connection_limit,
            bootstrapped: c.bootstrapped,
            all_object_privileges: c.initial_privileges.all_object_privileges,
            ddl_flag: c.initial_privileges.ddl,
            scram_verifier: c.scram_verifier,
        }
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
                UsersFileBinding::Bound(BoundUsersFileRole::from_credentials(c))
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

/// SCRAM 照合のダミー検証子（実値の無いロールでも同じ回数の PBKDF2 を走らせるための固定値）。
fn dummy_scram_verifier() -> &'static scram::ScramVerifier {
    static DUMMY: OnceLock<scram::ScramVerifier> = OnceLock::new();
    DUMMY.get_or_init(|| {
        match engine::crypto::scram_verifier::generate_verifier(
            b"dummy password",
            &[0u8; scram::SALT_LEN],
            scram::SCRAM_ITERATIONS,
        ) {
            Ok(v) => v,
            // 固定入力のため到達しない。万一でも照合が常に不一致になる形で fail-closed にする。
            Err(_) => scram::ScramVerifier {
                iterations: scram::SCRAM_ITERATIONS,
                salt: vec![0u8; scram::SALT_LEN],
                stored_key: [0u8; scram::KEY_LEN],
                server_key: [0u8; scram::KEY_LEN],
            },
        }
    })
}

/// 定数時間のバイト列比較（長さは公開情報）。
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// 平文パスワードが SCRAM 検証子と一致するか（cleartext 方式でカタログの検証子を照合する）。
fn scram_password_matches(verifier: &scram::ScramVerifier, password: &[u8]) -> bool {
    match engine::crypto::scram_verifier::generate_verifier(
        password,
        &verifier.salt,
        verifier.iterations,
    ) {
        Ok(derived) => {
            let stored = ct_eq(&derived.stored_key, &verifier.stored_key);
            let server = ct_eq(&derived.server_key, &verifier.server_key);
            stored & server
        }
        Err(_) => false,
    }
}

/// カタログのロールの資格情報で平文パスワードを照合する（ブートストラップ済みテナントの cleartext・HTTP）。
/// 旧形式ハッシュ（`legacy_phc`）があればそれで、無ければ SCRAM 検証子で照合する。どちらの経路でも
/// Argon2id と SCRAM の照合を 1 回ずつ実行して計算量を揃える（実値が無ければダミー）。
fn catalog_password_matches(c: &LoginRoleCredentials, password: &[u8]) -> bool {
    let has_phc = !c.legacy_phc.is_empty();
    let phc_to_check: &str = if has_phc {
        &c.legacy_phc
    } else {
        super::dummy_phc()
    };
    let argon_ok = match argon2id::verify_phc(phc_to_check, password) {
        Ok(m) => m,
        Err(e) => {
            engine::log_stderr!(
                "wire-server: password verification could not run the KDF ({e:?}); treating as authentication failure"
            );
            false
        }
    };
    let parsed = scram::ScramVerifier::parse(&c.scram_verifier)
        .ok()
        .filter(|v| v.iterations == scram::SCRAM_ITERATIONS && v.salt.len() == scram::SALT_LEN);
    let scram_ok =
        scram_password_matches(parsed.as_ref().unwrap_or(dummy_scram_verifier()), password);
    if has_phc {
        argon_ok
    } else {
        parsed.is_some() && scram_ok
    }
}

/// マーカーのあるテナントの素の username の照合（資格情報はカタログだけ。ファイルの PHC は読まない）。
fn verify_against_catalog(
    start: Instant,
    tenant_id: &str,
    c: LoginRoleCredentials,
    password: &[u8],
) -> Result<(engine::policy::PolicyContext, UsersFileBinding), AuthFailure> {
    let matches = catalog_password_matches(&c, password);
    let ctx = if matches && !c.is_expired() {
        super::session_policy_context(tenant_id).ok()
    } else {
        None
    };
    let Some(ctx) = ctx else {
        let elapsed = start.elapsed();
        if elapsed < AUTH_FAILURE_DELAY {
            std::thread::sleep(AUTH_FAILURE_DELAY - elapsed);
        }
        return Err(AuthFailure);
    };
    Ok((
        ctx,
        UsersFileBinding::Bound(BoundUsersFileRole::from_credentials(c)),
    ))
}

/// 認証失敗の KDF 負荷を揃えるための空回し（Argon2id と SCRAM の PBKDF2 を 1 回ずつ。結果は捨てる）。
/// ロールの不在・読み取り失敗・未知ユーザーが、実在ロールの照合（両 KDF を実行）と所要時間で
/// 区別できないようにする（存在のオラクル対策）。
fn burn_kdfs(password: &[u8], argon_too: bool) {
    if argon_too {
        let _ = argon2id::verify_phc(super::dummy_phc(), password);
    }
    let _ = scram_password_matches(dummy_scram_verifier(), password);
}

/// 起点から [`AUTH_FAILURE_DELAY`] に達するまで待ってから認証失敗を返す。
fn fail_after_delay(start: Instant) -> AuthFailure {
    let elapsed = start.elapsed();
    if elapsed < AUTH_FAILURE_DELAY {
        std::thread::sleep(AUTH_FAILURE_DELAY - elapsed);
    }
    AuthFailure
}

/// cleartext 用の「パスワード照合＋ロール対応づけ」。照合の前に起点を取り、対応づけの失敗でも
/// [`AUTH_FAILURE_DELAY`] に達するまで待つ（誤パスワードと遅延で区別できない。J6）。戻りの
/// [`UsersFileBinding`] は `Unbound` か `Bound` のみ。HTTP の `POST /v1/session`（HTTP-4）からも
/// 同じ関数を使う。ブートストラップ済みテナントでは資格情報の照合元がカタログになる（Issue #2238）。
///
/// カタログの引きは 1 回だけで、その結果を照合と束縛の両方に使う（引き直しの間にカタログが
/// 変わっても、未照合のロールへ束縛しない）。読み取り失敗・同名ロール不在は認証失敗（fail-closed。
/// ファイルの旧パスワードへは落とさない）。engine がある場合は、どの経路でも Argon2id と PBKDF2 を
/// 1 回ずつ実行して計算量を揃える。
pub(crate) fn verify_and_bind(
    store: &UserStore,
    engine: Option<&EngineCore>,
    username: &str,
    password: &[u8],
) -> Result<(engine::policy::PolicyContext, UsersFileBinding), AuthFailure> {
    let start = Instant::now();
    let Some(engine) = engine else {
        let ctx = super::verify(store, username, password)?;
        return Ok((ctx, UsersFileBinding::Unbound));
    };
    let Some(tenant) = store.tenant_of(username) else {
        // 未知ユーザー: 実在ロールと同じ KDF 回数（Argon2id は `verify` 内のダミー照合）。
        burn_kdfs(password, false);
        let _ = super::verify_from(store, username, password, start);
        return Err(fail_after_delay(start));
    };
    match engine.resolve_users_file_role(tenant, username) {
        Ok(UsersFileRoleBinding::Role(c)) if c.bootstrapped => {
            verify_against_catalog(start, tenant, c, password)
        }
        Ok(UsersFileRoleBinding::Role(c)) => {
            // マーカー無し（移行前）。ファイルの PHC で照合し、同じ引き結果のロールへ束縛する。
            burn_kdfs(password, false);
            let ctx = super::verify_from(store, username, password, start)?;
            if c.is_expired() {
                return Err(fail_after_delay(start));
            }
            Ok((
                ctx,
                UsersFileBinding::Bound(BoundUsersFileRole::from_credentials(c)),
            ))
        }
        Ok(UsersFileRoleBinding::CatalogNotAuthoritative) => {
            burn_kdfs(password, false);
            let ctx = super::verify_from(store, username, password, start)?;
            Ok((ctx, UsersFileBinding::Unbound))
        }
        Ok(UsersFileRoleBinding::Missing) => {
            burn_kdfs(password, true);
            Err(fail_after_delay(start))
        }
        Err(_) => {
            engine::log_stderr!(
                "wire-server: role catalog lookup failed while verifying a users-file user; treating as authentication failure"
            );
            burn_kdfs(password, true);
            Err(fail_after_delay(start))
        }
    }
}
