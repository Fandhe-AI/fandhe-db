//! `POST /v1/query` 前段の認証ミドルウェア（Issue #754・TASK-174。対象
//! ビヘイビア HTTP-5・HTTP-6・HTTP-7。ポインタ: `docs/spec/05-tasks.md`
//! TASK-174・`docs/spec/04-behavior/http-transport.md` HTTP-5, HTTP-6,
//! HTTP-7）。
//!
//! 呼び出し文脈: [`crate::http::router::Router`] が `target == "/v1/query"`
//! を一致させたときに [`authenticate`] を呼ぶ。成功すれば返る
//! [`SessionPrincipal`] が後続（[`crate::http::query::gate::handle`]・将来の
//! op 束縛・実行〔#759・#763 以降〕）へ渡す**唯一のテナント文脈**であり、
//! ヘッダ・JSON 本文・パスからテナントを読む経路は構造上持たせない（HTTP-7
//! 「クライアント自己申告の `tenant_id` はテナント文脈を上書きしない」）。
//!
//! ## 処理順序（fail-closed。契約の一部）
//!
//! 1. [`authenticate`]（Bearer 検証。[`crate::http::session::bearer::
//!    extract_bearer_token`] → [`crate::http::session::store::SessionStore::
//!    lookup`]）を**本文検証・テナントヘッダ検査より前**に行う。未認証の
//!    クライアントに本文スキーマの挙動・ヘッダ受理挙動を探索させないため
//!    （呼び出し元 [`crate::http::router::Router`] がこの順序を実施する）。
//! 2. [`reject_tenant_headers`]（`tenant_id` 相当のヘッダ拒否）。
//! 3. 本文検証（[`crate::http::query::gate`] の責務）。
//!
//! パス位置の `tenant_id`（`/v1/query/tenant-id/...` 等）は要求自身の構文
//! 違反でありサーバー状態・テナント存在情報を含まないため、認証より前の
//! ルーティング段（[`crate::http::router`]）で判定してよい契約とする
//! （本モジュールの対象外）。
//!
//! ## トークン照合の比較方式（Issue #754 での再評価。結論: 現状維持）
//!
//! [`crate::http::session::store::SessionStore`] は `HashMap<SessionToken,
//! Entry>`（プロセスごとにランダムな `SipHash` 鍵）による照合を維持する。
//! バケット一致に到達するには鍵の知識が必要であり、一致後の `[u8; 32]::eq`
//! 短絡比較はネットワーク越しの応答時間から観測可能な側チャネルにならないと
//! 判断する。加えて欠落・不正・未知・期限切れ・close 済みのいずれも
//! [`authenticate`] は同一の [`MiddlewareError`]（`28000`・固定文言）へ収束
//! させ、`lookup` は `issued_at` を更新しないため応答内容にも状態差が出ない。
//! `store.rs` module doc の「#754 で再評価対象」はこの判断で確定する
//! （定数時間比較への置換は行わない）。

use std::time::Instant;

use engine::error_format::ErrorClass;
use engine::policy::PolicyContext;

use crate::http::headers::Headers;
use crate::http::session::bearer::{self, BearerError};
use crate::http::session::store::SessionStore;

/// [`authenticate`] を通過した要求が束縛されるテナント文脈。
///
/// フィールド非公開・構築経路は [`authenticate`] のみ（[`PolicyContext`]
/// を取り出す唯一の入口は [`SessionPrincipal::policy_context`]）。ヘッダ・
/// JSON 本文・パスからテナントを読む経路をシグネチャ上持たせないための
/// 型的な強制（HTTP-7）。
pub struct SessionPrincipal {
    ctx: PolicyContext,
    /// DDL 実行権限（NOSQL-13・TASK-207、Issue #910）。`SessionStore::
    /// lookup_grant` がセッション発行時（`issue.rs`）に確定させた値を運ぶ
    /// だけで、DDL 実行権限ゲートそのものは
    /// `engine::sql::ddl::require_ddl_permission` 一箇所が担う
    /// （`http/query/ddl.rs` のみがこのフラグを読んで
    /// `SessionState::allow_ddl` を呼ぶ）。
    ddl_allowed: bool,
    /// 認証済みユーザーに対応づけたカタログのロール（HTTP-4・Issue #2238。未束縛は `None`）。
    role: Option<crate::http::session::store::SessionRole>,
    /// 発行時に宣言されたスコープ（HTTP-15・Issue #2100。省略は `None`＝全権限）。運ぶだけで、
    /// 要求ごとの強制は `POST /v1/query` の手順 5c（Issue #2101）が行う（`session_state()` には影響させない）。
    scope: Option<std::sync::Arc<crate::http::session::scope::SessionScope>>,
}

impl SessionPrincipal {
    /// 発行時に宣言されたスコープ（省略は `None`）。読み取り専用で、書き換え経路は無い。
    pub fn scope(&self) -> Option<&crate::http::session::scope::SessionScope> {
        self.scope.as_deref()
    }

    /// この要求に束縛されたテナント文脈。
    pub fn policy_context(&self) -> &PolicyContext {
        &self.ctx
    }

    /// この要求の主体から engine の `SessionState` を作る、HTTP 表層で唯一の入口（SQL-40・NOSQL-2・
    /// Issue #1969）。`http/query/*` の各 op ハンドラは `SessionState::default()` を自前で組み立てず、
    /// ここから受け取った値を engine の bound API へ渡す（engine の権限ゲートが SQL 表層と同じ判定を行う）。
    ///
    /// 契約: `ddl_allowed` は認証層（セッション発行）が確定した値の写しで、JSON 本文・パスから主体を
    /// 変える経路は持たない。認証済みユーザーに同名のカタログのロールが対応づいていれば、ここでロールを束縛する
    /// （Issue #2238。pg wire の `handshake.rs` と同じ。初期ロールの全権限は経路を問わずここで写す）。
    /// 対応づくロールが無い（カタログが正でないテナント）ときは未束縛のまま（従来と同一）。
    pub fn session_state(&self) -> engine::sql::mode::SessionState {
        let mut session = engine::sql::mode::SessionState::default();
        if self.ddl_allowed {
            session.allow_ddl();
        }
        if let Some(role) = &self.role {
            session.set_session_user(&role.role_name);
            session.bind_role_ids(role.role_id, role.role_id);
            if role.all_object_privileges {
                session.grant_bootstrap_all_privileges();
            }
        }
        session
    }

    /// このセッションが DDL（`create_table`／`alter_table`／`drop_table`）を
    /// 実行できるか（NOSQL-13・TASK-207、Issue #910）。
    pub fn ddl_allowed(&self) -> bool {
        self.ddl_allowed
    }
}

impl std::fmt::Debug for SessionPrincipal {
    /// `PolicyContext`（`tenant_id` を含む）を一切印字しない
    /// （`.claude/rules/security.md`「エラー・ログ経由で他テナントのデータ・
    /// 存在情報を漏らさない」。[`SessionStore`] の `Debug` と同じ配慮）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionPrincipal").finish_non_exhaustive()
    }
}

/// [`authenticate`] の拒否理由。欠落・不正 Bearer・未知／期限切れ／close
/// 済みトークンのいずれも [`ErrorClass::AuthRequired`]（`28000`）へ写像し、
/// `client_message()` は [`bearer::MESSAGE`] の固定文言を返す（存在オラクル
/// 非公開設計。`bearer::BearerError`・`SessionStore::lookup` と同じ方針）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiddlewareError {
    /// `Authorization: Bearer` ヘッダの受信データ経路での拒否。
    Bearer(BearerError),
    /// ヘッダの形式は正しいが `SessionStore::lookup` が `None`（未知・期限切れの
    /// いずれかだが区別しない）。
    Unknown,
}

impl MiddlewareError {
    /// この失敗の分類（常に [`ErrorClass::AuthRequired`]）。
    pub const fn error_class(&self) -> ErrorClass {
        ErrorClass::AuthRequired
    }

    /// クライアントへ返す固定文言（常に [`bearer::MESSAGE`]）。
    pub const fn client_message(&self) -> &'static str {
        bearer::MESSAGE
    }
}

/// `headers` から `Authorization: Bearer <token>` を検証し、有効であれば
/// `sessions` に束縛された [`SessionPrincipal`] を返す。
///
/// 手順（fail-closed）: [`bearer::extract_bearer_token`] でトークンを取り出し
/// （失敗は [`MiddlewareError::Bearer`]）、[`SessionStore::lookup`] で
/// `PolicyContext` を解決する（`None` は [`MiddlewareError::Unknown`]。未知・
/// 期限切れ・close 済みのいずれも区別しない）。
pub fn authenticate(
    sessions: &SessionStore,
    headers: &Headers<'_>,
    now_mono: impl Fn() -> Instant,
) -> Result<SessionPrincipal, MiddlewareError> {
    let token = bearer::extract_bearer_token(headers).map_err(MiddlewareError::Bearer)?;
    match sessions.lookup_grant(&token, now_mono()) {
        Some(grant) => Ok(SessionPrincipal {
            ctx: grant.ctx,
            ddl_allowed: grant.ddl_allowed,
            role: grant.role,
            scope: grant.scope,
        }),
        None => Err(MiddlewareError::Unknown),
    }
}

/// [`reject_tenant_headers`] の拒否理由。`tenant_id` 相当のヘッダが 1 件以上
/// 見つかった。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TenantHeaderError;

/// クライアントへ返す固定文言（ヘッダ名を echo しない）。
pub const TENANT_HEADER_MESSAGE: &str =
    "tenant context is derived from the session; per-request tenant headers are not accepted";

impl TenantHeaderError {
    /// この失敗の分類（常に [`ErrorClass::UnsupportedSqlSyntax`]。`42601`）。
    pub const fn error_class(&self) -> ErrorClass {
        ErrorClass::UnsupportedSqlSyntax
    }

    /// クライアントへ返す固定文言（常に [`TENANT_HEADER_MESSAGE`]）。
    pub const fn client_message(&self) -> &'static str {
        TENANT_HEADER_MESSAGE
    }
}

/// `name`（生のヘッダ名バイト列）が `tenant_id` 相当かを判定する（閉じた
/// 規則）。
///
/// 手順: ASCII 小文字化 → `_` を `-` へ正規化 → 先頭の `x-` を 1 回だけ除去
/// → `{"tenant", "tenant-id", "tenantid"}` のいずれかに一致すれば `true`。
/// `X-Tenant-Foo` のような無関係な接頭辞一致は対象外（正規化後の全体一致の
/// み判定するため）。
fn is_tenant_header_name(name: &[u8]) -> bool {
    let mut normalized: Vec<u8> = name
        .iter()
        .map(|&b| match b {
            b'_' => b'-',
            other => other.to_ascii_lowercase(),
        })
        .collect();
    if let Some(rest) = normalized.strip_prefix(b"x-") {
        normalized = rest.to_vec();
    }
    matches!(
        normalized.as_slice(),
        b"tenant" | b"tenant-id" | b"tenantid"
    )
}

/// `headers` に `tenant_id` 相当のヘッダ（[`is_tenant_header_name`]）が
/// 1 件でも含まれていれば拒否する。値は読まない・応答へ含めない
/// （HTTP-7。クライアント自己申告のテナント指定を無視して続行するのではなく
/// 拒否する fail-closed 設計）。
pub fn reject_tenant_headers(headers: &Headers<'_>) -> Result<(), TenantHeaderError> {
    for (name, _value) in headers.iter() {
        if is_tenant_header_name(name) {
            return Err(TenantHeaderError);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::headers::{parse_headers, HeaderParse};
    use crate::http::session::token::SessionToken;
    use std::time::Duration;

    fn leak(v: Vec<u8>) -> &'static [u8] {
        Box::leak(v.into_boxed_slice())
    }

    fn headers_from(raw: &[u8]) -> Headers<'static> {
        let mut input = raw.to_vec();
        input.extend_from_slice(b"Content-Length: 0\r\n\r\n");
        match parse_headers(leak(input)).expect("header parse should succeed") {
            HeaderParse::Complete { headers, .. } => headers,
            other => panic!("expected Complete, got {other:?}"),
        }
    }

    fn ctx(tenant: &str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant id in test")
    }

    // --- authenticate ----------------------------------------------------

    #[test]
    fn missing_authorization_rejects_with_unified_error() {
        let sessions = SessionStore::new();
        let headers = headers_from(b"");
        let err = authenticate(&sessions, &headers, Instant::now).unwrap_err();
        assert_eq!(err.error_class(), ErrorClass::AuthRequired);
        assert_eq!(err.client_message(), bearer::MESSAGE);
        assert!(matches!(err, MiddlewareError::Bearer(BearerError::Missing)));
    }

    #[test]
    fn malformed_bearer_rejects_with_unified_error() {
        let sessions = SessionStore::new();
        let headers = headers_from(b"Authorization: Basic dXNlcjpwYXNz\r\n");
        let err = authenticate(&sessions, &headers, Instant::now).unwrap_err();
        assert_eq!(err.error_class(), ErrorClass::AuthRequired);
        assert_eq!(err.client_message(), bearer::MESSAGE);
    }

    #[test]
    fn unknown_token_rejects_with_unknown() {
        let sessions = SessionStore::new();
        let unknown = SessionToken::generate().expect("generate token");
        let raw = format!("Authorization: Bearer {}\r\n", unknown.encoded());
        let headers = headers_from(raw.as_bytes());
        let err = authenticate(&sessions, &headers, Instant::now).unwrap_err();
        assert_eq!(err, MiddlewareError::Unknown);
        assert_eq!(err.error_class(), ErrorClass::AuthRequired);
        assert_eq!(err.client_message(), bearer::MESSAGE);
    }

    #[test]
    fn expired_token_rejects_with_unknown() {
        let sessions = SessionStore::with_limits(4, Duration::from_secs(60));
        let base = Instant::now();
        let token = sessions.issue(ctx("tenant-a"), base).expect("issue");
        let raw = format!("Authorization: Bearer {}\r\n", token.encoded());
        let headers = headers_from(raw.as_bytes());

        let after_ttl = base + Duration::from_secs(61);
        let err = authenticate(&sessions, &headers, move || after_ttl).unwrap_err();
        assert_eq!(err, MiddlewareError::Unknown);
    }

    #[test]
    fn closed_token_rejects_with_unknown() {
        let sessions = SessionStore::new();
        let now = Instant::now();
        let token = sessions.issue(ctx("tenant-a"), now).expect("issue");
        assert!(sessions.close(&token, now));

        let raw = format!("Authorization: Bearer {}\r\n", token.encoded());
        let headers = headers_from(raw.as_bytes());
        let err = authenticate(&sessions, &headers, move || now).unwrap_err();
        assert_eq!(err, MiddlewareError::Unknown);
    }

    #[test]
    fn valid_token_resolves_to_issued_policy_context() {
        let sessions = SessionStore::new();
        let now = Instant::now();
        let token = sessions.issue(ctx("tenant-a"), now).expect("issue");
        let raw = format!("Authorization: Bearer {}\r\n", token.encoded());
        let headers = headers_from(raw.as_bytes());

        let principal = authenticate(&sessions, &headers, move || now).expect("authenticate");
        assert_eq!(principal.policy_context(), &ctx("tenant-a"));
    }

    #[test]
    fn authenticate_carries_ddl_allowed_from_session_grant() {
        // NOSQL-13・TASK-207、Issue #910: `authenticate` は
        // `SessionStore::lookup_grant` 経由で DDL 実行権限を
        // `SessionPrincipal` へ運ぶ（`lookup` ではなく）。
        let sessions = SessionStore::new();
        let now = Instant::now();
        let token = sessions
            .issue_with_ddl(ctx("tenant-a"), true, now)
            .expect("issue_with_ddl");
        let raw = format!("Authorization: Bearer {}\r\n", token.encoded());
        let headers = headers_from(raw.as_bytes());

        let principal = authenticate(&sessions, &headers, move || now).expect("authenticate");
        assert!(principal.ddl_allowed());
    }

    #[test]
    fn session_state_mirrors_ddl_allowed_and_leaves_the_role_unbound() {
        // SQL-40・Issue #1969: HTTP 表層の `SessionState` は `session_state()` だけで作る。
        // 運用者権限は認証層が確定した値の写しで、現在ロールは #2238 まで未束縛。
        let sessions = SessionStore::new();
        let now = Instant::now();
        for ddl in [false, true] {
            let token = sessions
                .issue_with_ddl(ctx("tenant-a"), ddl, now)
                .expect("issue_with_ddl");
            let raw = format!("Authorization: Bearer {}\r\n", token.encoded());
            let headers = headers_from(raw.as_bytes());
            let principal = authenticate(&sessions, &headers, move || now).expect("authenticate");
            let state = principal.session_state();
            assert_eq!(state.ddl_allowed(), ddl);
            assert_eq!(state.current_role_id(), None);
            assert!(!state.object_privileges_enforced());
        }
    }

    #[test]
    fn authenticate_defaults_ddl_allowed_to_false_for_plain_issue() {
        let sessions = SessionStore::new();
        let now = Instant::now();
        let token = sessions.issue(ctx("tenant-a"), now).expect("issue");
        let raw = format!("Authorization: Bearer {}\r\n", token.encoded());
        let headers = headers_from(raw.as_bytes());

        let principal = authenticate(&sessions, &headers, move || now).expect("authenticate");
        assert!(!principal.ddl_allowed());
    }

    #[test]
    fn tenant_a_token_never_resolves_to_tenant_b_context() {
        let sessions = SessionStore::new();
        let now = Instant::now();
        let token_a = sessions.issue(ctx("tenant-a"), now).expect("issue a");

        let raw = format!("Authorization: Bearer {}\r\n", token_a.encoded());
        let headers = headers_from(raw.as_bytes());
        let principal = authenticate(&sessions, &headers, move || now).expect("authenticate");
        assert_ne!(principal.policy_context(), &ctx("tenant-b"));
    }

    #[test]
    fn lookup_via_authenticate_does_not_consume_session_slot() {
        // HTTP-5 非 vacuous: `lookup` は枠を消費しない（発行後に繰り返し
        // `authenticate` を通しても `active_sessions` は不変）。
        let sessions = SessionStore::with_limits(1, Duration::from_secs(60));
        let now = Instant::now();
        let token = sessions.issue(ctx("tenant-a"), now).expect("issue");
        let raw = format!("Authorization: Bearer {}\r\n", token.encoded());
        let headers = headers_from(raw.as_bytes());

        for _ in 0..5 {
            authenticate(&sessions, &headers, move || now).expect("authenticate");
        }
        assert_eq!(sessions.active_sessions(), 1);
    }

    #[test]
    fn debug_output_does_not_leak_tenant_id() {
        let sessions = SessionStore::new();
        let now = Instant::now();
        let token = sessions
            .issue(ctx("super-secret-tenant"), now)
            .expect("issue");
        let raw = format!("Authorization: Bearer {}\r\n", token.encoded());
        let headers = headers_from(raw.as_bytes());

        let principal = authenticate(&sessions, &headers, move || now).expect("authenticate");
        let debug_output = format!("{principal:?}");
        assert!(!debug_output.contains("super-secret-tenant"));
    }

    // --- reject_tenant_headers --------------------------------------------

    #[test]
    fn accepts_headers_without_tenant_marker() {
        let headers = headers_from(b"Authorization: Bearer x\r\nHost: localhost\r\n");
        assert_eq!(reject_tenant_headers(&headers), Ok(()));
    }

    #[test]
    fn rejects_known_tenant_header_spellings() {
        for header_line in [
            "Tenant-Id: other\r\n",
            "X-Tenant-Id: other\r\n",
            "x-tenant_id: other\r\n",
            "TENANT: other\r\n",
            "X-Tenant: other\r\n",
            "TenantId: other\r\n",
        ] {
            let headers = headers_from(header_line.as_bytes());
            let result = reject_tenant_headers(&headers);
            assert!(result.is_err(), "expected rejection for {header_line:?}");
            let err = result.unwrap_err();
            assert_eq!(err.error_class(), ErrorClass::UnsupportedSqlSyntax);
            assert_eq!(err.client_message(), TENANT_HEADER_MESSAGE);
        }
    }

    #[test]
    fn unrelated_prefix_header_is_not_rejected() {
        let headers = headers_from(b"X-Tenant-Foo: bar\r\n");
        assert_eq!(reject_tenant_headers(&headers), Ok(()));
    }

    #[test]
    fn tenant_header_error_message_does_not_echo_header_name() {
        let headers = headers_from(b"X-Tenant-Id: other\r\n");
        let err = reject_tenant_headers(&headers).unwrap_err();
        assert!(!err.client_message().contains("X-Tenant-Id"));
    }

    #[test]
    fn scope_is_carried_to_principal_without_affecting_session_state() {
        use crate::http::session::scope::scope_for_test;
        let sessions = SessionStore::new();
        let now = Instant::now();
        let scope = scope_for_test(&["docs"], true);
        let scoped = sessions
            .issue_with_scope(ctx("t"), false, None, Some(scope.clone()), now)
            .expect("issue");
        let plain = sessions.issue(ctx("t"), now).expect("issue");

        let raw = format!("Authorization: Bearer {}\r\n", scoped.encoded());
        let p = authenticate(&sessions, &headers_from(raw.as_bytes()), || now).expect("auth");
        assert_eq!(p.scope(), Some(&scope));
        let raw = format!("Authorization: Bearer {}\r\n", plain.encoded());
        let q = authenticate(&sessions, &headers_from(raw.as_bytes()), || now).expect("auth");
        assert!(q.scope().is_none());
        // スコープの強制は gate 側（#2101）。session_state は有無で変わらない。
        assert_eq!(
            format!("{:?}", p.session_state()),
            format!("{:?}", q.session_state())
        );
    }
}
