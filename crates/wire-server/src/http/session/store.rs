//! NoSQL 表層のセッション認証（TASK-174・HTTP-4・HTTP-5）が使う、
//! 発行済み [`SessionToken`] → `engine::policy::PolicyContext` の対応を
//! プロセス内メモリだけで保持するストア。
//!
//! 呼び出し文脈: `POST /v1/session`（Issue #752）が認証成功時に [`SessionStore::issue`]
//! を呼びトークンを発行し、以後のリクエストで `Authorization: Bearer <token>`
//! （Issue #753・#754）を検証するミドルウェアが [`SessionStore::lookup`] を、
//! `POST /v1/session/close`（Issue #753）が [`SessionStore::close`] を呼ぶ。
//! 本モジュールはこの 3 メソッドと保持契約（TTL・上限）を提供するのみで、
//! HTTP 側の受理点（ヘッダ解析・`28000` の送出）はいずれも後続 Issue の担当。
//!
//! ## 保持契約
//! - TTL は [`crate::limits::SESSION_TTL`] 固定（発行時刻起点。`lookup` で
//!   延長しない）。期限切れは参照時（`lookup`／`close`／枠不足時の `issue`）に
//!   失効扱いとし、バックグラウンドの掃除スレッドは持たない
//! - 同時有効数は [`crate::limits::SessionLimiter`]（[`crate::limits::MAX_SESSIONS`]）
//!   で有界化する。エントリは permit 取得後にしか挿入されないため、常に
//!   エントリ数 ≤ 上限が構造的に保証される
//! - ディスク・環境変数・ログへトークン／`PolicyContext` を書き出さない
//!   （`Debug` は件数のみ）
//!
//! ## 時刻注入
//! `Instant::now()` を一切呼ばず、全メソッドが `now: Instant` を引数で受け取る
//! （単調時計。`SystemTime` は使わない）。production では後続 Issue の router が
//! `Instant::now()` を渡す。テストは任意の `now` を注入して境界（TTL ちょうど）
//! を決定的に検証できる。
//!
//! ## トークン照合の比較方式
//! `HashMap<SessionToken, Entry>` を採用する（[`token`](super::token) モジュール
//! doc の「定数時間比較は #751／#754 の設計事項」に対する本 Issue の決定）。
//! `HashMap` の `RandomState` はプロセスごとにランダムな `SipHash` 鍵を使うため、
//! 攻撃者がハッシュ衝突（バケット一致）まで到達するには鍵を知る必要があり、
//! バケット一致後の `[u8; 32]::eq` の短絡比較は実質的な側チャネルにならないと
//! 判断する。この判断はプロセス起動ごとに鍵が変わる `HashMap` の性質に依存する
//! ため、Bearer 受理点を実装する #754 で再評価対象としていたが、
//! `session::middleware`（Issue #754）のモジュール doc で現状維持
//! （定数時間比較への置換は行わない）と結論済み。

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::limits::{SessionLimiter, SessionPermit, MAX_SESSIONS, SESSION_TTL};

use super::scope::SessionScope;
use super::token::SessionToken;

/// [`SessionStore::issue`] の拒否理由。
#[derive(Debug)]
pub enum IssueError {
    /// 同時有効セッション数が上限（[`MAX_SESSIONS`]）に達しており、期限切れ
    /// エントリの回収後もなお枠が確保できなかった。
    LimitExceeded,
    /// OS の CSPRNG からのトークン生成に失敗した（[`SessionToken::generate`]）。
    TokenGeneration(std::io::Error),
    /// 内部不整合（生成トークンのキー衝突・`Mutex` の poison）。fail-closed に
    /// 拒否する経路で、通常の運用では到達しない想定（RECOVER-8 のプロセス
    /// fail-fast が panic を捕捉するため）。
    Internal,
}

impl fmt::Display for IssueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IssueError::LimitExceeded => write!(f, "session limit exceeded"),
            IssueError::TokenGeneration(e) => write!(f, "session token generation failed: {e}"),
            IssueError::Internal => write!(f, "internal session store error"),
        }
    }
}

impl std::error::Error for IssueError {}

impl IssueError {
    /// この拒否理由に対応する `wire_code` 分類。`LimitExceeded` は
    /// [`crate::limits::SQLSTATE_TOO_MANY_CONNECTIONS`] と同じ分類
    /// （`engine::error_format::ErrorClass::ConnectionLimitExceeded`）を
    /// 再利用し、セッション用の新規 `wire_code` は追加しない（`http/mod.rs`
    /// の「新規 `wire_code` を追加しない」契約）。
    pub fn error_class(&self) -> engine::error_format::ErrorClass {
        match self {
            IssueError::LimitExceeded => engine::error_format::ErrorClass::ConnectionLimitExceeded,
            IssueError::TokenGeneration(_) | IssueError::Internal => {
                engine::error_format::ErrorClass::InternalError
            }
        }
    }
}

/// ストア内の 1 セッションぶんの状態。`_permit` は保持しているだけで直接
/// 参照しない（`Entry` の drop で自動的に [`SessionLimiter`] の枠が解放される
/// ―― 枠解放を明示的な減算経路として複製しない設計）。
struct Entry {
    ctx: engine::policy::PolicyContext,
    /// 認証済みユーザーに対応するカタログのロール（HTTP-4・Issue #2238。同名ロールが無い・カタログが正
    /// でないテナントは `None`＝未束縛）。発行時に 1 回だけ確定し、以後は変化しない。
    role: Option<SessionRole>,
    /// DDL 実行権限（`--ddl-allowed-users`。NOSQL-13・TASK-207、Issue #910）。
    /// `issue.rs` が `auth::verify` 成功直後に `UserStore::is_ddl_allowed` から
    /// 1 回だけ確定させる値で、以後このセッションの寿命中は変化しない
    /// （テナント境界（`ctx`）とは別軸の権限。DDL 実行権限ゲートの単一判定点
    /// は `engine::sql::ddl::require_ddl_permission` のまま——本フィールドは
    /// その判定へ渡す `SessionState::allow_ddl` の入力値をセッションへ
    /// 搬送するだけで、第 2 の権限判定にはしない）。
    ddl_allowed: bool,
    /// 発行時に宣言されたスコープ（HTTP-15・Issue #2100。省略は `None`＝全権限）。発行時に 1 回だけ
    /// 確定し以後変化しない（setter なし）。強制は後続 Issue #2101。
    scope: Option<Arc<SessionScope>>,
    /// 認証済みユーザー名（Issue #2094。scan の不透明カーソルを認証主体へ束縛する AAD の入力。
    /// 発行経路が与えない場合は `None`）。`Debug` へ出さない。
    user: Option<String>,
    issued_at: Instant,
    _permit: SessionPermit,
}

/// セッションが束縛するカタログのロール（認証層が発行時に確定した値の写し。`Debug` は伏せ字）。
#[derive(Clone, PartialEq, Eq)]
pub struct SessionRole {
    pub role_id: u64,
    pub role_name: String,
    /// 初期ロールの全権限（オブジェクト権限の所有者相当。DDL は含まない）。
    pub all_object_privileges: bool,
}

impl fmt::Debug for SessionRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionRole(<redacted>)")
    }
}

/// [`SessionStore::lookup_grant`] が返す、テナント境界と DDL 実行権限の対
/// （HTTP セッション認証ミドルウェアが両方を 1 回の照会で読む入口。
/// NOSQL-13・TASK-207、Issue #910）。
#[derive(Clone, PartialEq)]
pub struct SessionGrant {
    pub ctx: engine::policy::PolicyContext,
    pub ddl_allowed: bool,
    /// 束縛するカタログのロール（未束縛は `None`）。
    pub role: Option<SessionRole>,
    /// 発行時に宣言されたスコープ（省略は `None`。HTTP-15・Issue #2100）。
    pub scope: Option<Arc<SessionScope>>,
    /// 認証済みユーザー名（Issue #2094。発行経路が与えない場合は `None`）。
    pub user: Option<String>,
}

/// ユーザー名・テナントを出さない（ログ経由の漏えい防止）。
impl fmt::Debug for SessionGrant {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionGrant")
            .field("ddl_allowed", &self.ddl_allowed)
            .finish_non_exhaustive()
    }
}

/// TTL 固定・同時有効数上限付きのメモリ内セッションストア（TASK-174・
/// HTTP-4・HTTP-5）。`Clone` は内部状態（`Mutex` 越しの `HashMap`・
/// [`SessionLimiter`]）を共有する（`Arc` によるハンドル複製）。
#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<Mutex<HashMap<SessionToken, Entry>>>,
    limiter: SessionLimiter,
    ttl: Duration,
}

impl fmt::Debug for SessionStore {
    /// トークン・`PolicyContext`（`tenant_id` を含む）を一切印字しない
    /// （件数・上限のみ。security.md「エラー・ログ経由で他テナントのデータ・
    /// 存在情報を漏らさない」に対する予防的な配慮）。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionStore")
            .field("active_sessions", &self.limiter.active())
            .field("max_sessions", &self.limiter.max())
            .finish()
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore {
    /// 既定の契約値（[`MAX_SESSIONS`]＝256・[`SESSION_TTL`]＝3600 秒）で
    /// 新しいストアを作る。production の唯一の構築経路。
    pub fn new() -> Self {
        Self::with_limits(MAX_SESSIONS, SESSION_TTL)
    }

    /// 上限・TTL を明示指定して新しいストアを作る（テスト用途・将来の CLI
    /// opt-in 用途。既定値は [`SessionStore::new`] のまま不変）。
    pub fn with_limits(max_sessions: usize, ttl: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            limiter: SessionLimiter::new(max_sessions),
            ttl,
        }
    }

    /// `now` 時点で `issued_at + ttl` を超えていないかを判定する（TTL ちょうど
    /// で失効。発行時刻起点・スライドしない）。
    fn is_alive(&self, issued_at: Instant, now: Instant) -> bool {
        now.saturating_duration_since(issued_at) < self.ttl
    }

    /// 期限切れエントリを一括回収する（`retain` に伴い `Entry` が drop され
    /// [`SessionPermit`] の枠が解放される）。呼び出しは [`SessionStore::issue`]
    /// が枠不足時にのみ行う（O(現在のエントリ数) ≤ [`MAX_SESSIONS`] で有界。
    /// バックグラウンド掃除スレッドは持たない）。
    fn sweep_expired_locked(&self, table: &mut HashMap<SessionToken, Entry>, now: Instant) {
        table.retain(|_, entry| self.is_alive(entry.issued_at, now));
    }

    /// `ctx` を束縛した新しいセッショントークンを発行する。
    ///
    /// 手順（fail-closed）: (1) 枠確保を試み、失敗したら期限切れエントリを
    /// 一括回収してから再試行し、それでも失敗すれば [`IssueError::LimitExceeded`]。
    /// (2) [`SessionToken::generate`] が失敗したら [`IssueError::TokenGeneration`]
    /// （確保済みの枠は `permit` の drop で自動解放される）。(3) 生成したトークンが
    /// 既にテーブルに存在する場合は上書きしない（別テナントの `ctx` への再束縛
    /// 経路を作らないため）で [`IssueError::Internal`] を返す。
    pub fn issue(
        &self,
        ctx: engine::policy::PolicyContext,
        now: Instant,
    ) -> Result<SessionToken, IssueError> {
        self.issue_with_ddl(ctx, false, now)
    }

    /// [`SessionStore::issue`] の DDL 実行権限つき版（NOSQL-13・TASK-207、
    /// Issue #910）。`issue.rs` が `auth::verify` 成功後に一度だけ呼ぶ
    /// 発行入口で、`ddl_allowed` は以後このセッションの寿命中固定される。
    /// `issue` は `ddl_allowed = false` を渡す薄いラッパーとして残す
    /// （既存呼び出し・テストの互換を保つ）。
    pub fn issue_with_ddl(
        &self,
        ctx: engine::policy::PolicyContext,
        ddl_allowed: bool,
        now: Instant,
    ) -> Result<SessionToken, IssueError> {
        self.issue_with_grant(ctx, ddl_allowed, None, now)
    }

    /// [`SessionStore::issue_with_ddl`] のロール束縛つき版（HTTP-4・Issue #2238）。`issue.rs` が認証成功後に
    /// 1 回だけ呼ぶ。`role` は認証済みユーザーに対応づけたカタログのロール（未束縛は `None`）。
    pub fn issue_with_grant(
        &self,
        ctx: engine::policy::PolicyContext,
        ddl_allowed: bool,
        role: Option<SessionRole>,
        now: Instant,
    ) -> Result<SessionToken, IssueError> {
        self.issue_with_scope(ctx, ddl_allowed, role, None, now)
    }

    /// [`SessionStore::issue_with_grant`] のスコープつき版（HTTP-15・Issue #2100）。`scope` は発行時に
    /// トークンへ束縛され、メモリ内だけで保持される（失効・TTL で消える）。既存の入口は `None` を渡す
    /// 薄いラッパーで互換を保つ。
    pub fn issue_with_scope(
        &self,
        ctx: engine::policy::PolicyContext,
        ddl_allowed: bool,
        role: Option<SessionRole>,
        scope: Option<SessionScope>,
        now: Instant,
    ) -> Result<SessionToken, IssueError> {
        self.issue_with_user(ctx, None, ddl_allowed, role, scope, now)
    }

    /// [`SessionStore::issue_with_scope`] の認証ユーザー名つき版（Issue #2094）。`issue.rs` が認証成功後に
    /// 呼ぶ production の発行入口。`user` は scan のカーソルを認証主体へ束縛するためにだけ使う。
    pub fn issue_with_user(
        &self,
        ctx: engine::policy::PolicyContext,
        user: Option<String>,
        ddl_allowed: bool,
        role: Option<SessionRole>,
        scope: Option<SessionScope>,
        now: Instant,
    ) -> Result<SessionToken, IssueError> {
        let permit = match self.limiter.try_acquire() {
            Some(permit) => permit,
            None => {
                {
                    let mut table = self.inner.lock().map_err(|_| IssueError::Internal)?;
                    self.sweep_expired_locked(&mut table, now);
                }
                self.limiter
                    .try_acquire()
                    .ok_or(IssueError::LimitExceeded)?
            }
        };

        let token = SessionToken::generate().map_err(IssueError::TokenGeneration)?;

        let mut table = self.inner.lock().map_err(|_| IssueError::Internal)?;
        if table.contains_key(&token) {
            // CSPRNG 32 バイト出力の衝突は実務上無視できる確率だが、万一
            // 発生した場合に既存エントリ（別テナントの可能性がある）を
            // 上書きしない fail-closed な多層防御。
            return Err(IssueError::Internal);
        }
        table.insert(
            token.clone(),
            Entry {
                ctx,
                role,
                ddl_allowed,
                scope: scope.map(Arc::new),
                user,
                issued_at: now,
                _permit: permit,
            },
        );
        Ok(token)
    }

    /// `token` に束縛された `PolicyContext` を返す。存在しない・期限切れの
    /// いずれも `None`（両者を呼び出し側から区別できない存在オラクル非公開
    /// 設計。期限切れエントリはその場で回収し枠を解放する）。
    ///
    /// `issued_at` は更新しない（TTL は固定・スライドしない）。
    pub fn lookup(
        &self,
        token: &SessionToken,
        now: Instant,
    ) -> Option<engine::policy::PolicyContext> {
        let mut table = self.inner.lock().ok()?;
        let alive = self.is_alive(table.get(token)?.issued_at, now);
        if alive {
            table.get(token).map(|entry| entry.ctx.clone())
        } else {
            table.remove(token);
            None
        }
    }

    /// [`SessionStore::lookup`] と同じ照合契約で、`PolicyContext` に加えて
    /// DDL 実行権限も一度に返す（NOSQL-13・TASK-207、Issue #910）。
    /// `http/session/middleware.rs` の `authenticate` がこちらを使い、
    /// `SessionPrincipal` へ `ddl_allowed` を運ぶ。
    pub fn lookup_grant(&self, token: &SessionToken, now: Instant) -> Option<SessionGrant> {
        let mut table = self.inner.lock().ok()?;
        let alive = self.is_alive(table.get(token)?.issued_at, now);
        if alive {
            table.get(token).map(|entry| SessionGrant {
                ctx: entry.ctx.clone(),
                ddl_allowed: entry.ddl_allowed,
                role: entry.role.clone(),
                scope: entry.scope.clone(),
                user: entry.user.clone(),
            })
        } else {
            table.remove(token);
            None
        }
    }

    /// `token` を無効化する。取り出したエントリが有効期限内だった場合のみ
    /// `true`（未知・期限切れは `false`。二重 close が「無効」として観測
    /// できる）。枠解放は取り出した `Entry` の drop に一本化する。
    pub fn close(&self, token: &SessionToken, now: Instant) -> bool {
        let Ok(mut table) = self.inner.lock() else {
            return false;
        };
        match table.remove(token) {
            Some(entry) => self.is_alive(entry.issued_at, now),
            None => false,
        }
    }

    /// 現在の有効セッション数（観測用。期限切れだが未回収のエントリも含む
    /// ―― [`SessionLimiter::active`] をそのまま公開する）。
    pub fn active_sessions(&self) -> usize {
        self.limiter.active()
    }

    /// このストアの TTL（[`SessionStore::issue`] が発行するセッションの
    /// 有効期限）。応答の `expires_in` 等、呼び出し元が実際の保持契約値を
    /// 参照する必要がある箇所は [`crate::limits::SESSION_TTL`] 定数を直接
    /// 使わずこのアクセサーを経由する（[`SessionStore::with_limits`] で
    /// 既定と異なる TTL を注入したストアとの乖離を防ぐため。Issue #813
    /// codex-review P2 指摘）。
    pub fn ttl(&self) -> Duration {
        self.ttl
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::policy::PolicyContext;

    fn ctx(tenant: &str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant id in test")
    }

    /// 認証ユーザー名はグラントに運ばれ（Issue #2094）、`Debug` には出ない。
    #[test]
    fn grant_carries_user_and_debug_redacts_it() {
        let store = SessionStore::new();
        let now = Instant::now();
        let t = store
            .issue_with_user(
                ctx("tenant-x"),
                Some("alice-user".into()),
                false,
                None,
                None,
                now,
            )
            .expect("issue");
        let grant = store.lookup_grant(&t, now).expect("grant");
        assert_eq!(grant.user.as_deref(), Some("alice-user"));
        let dbg = format!("{grant:?}");
        assert!(
            !dbg.contains("alice-user") && !dbg.contains("tenant-x"),
            "{dbg}"
        );
        let t2 = store.issue(ctx("tenant-x"), now).expect("issue");
        assert!(store.lookup_grant(&t2, now).expect("grant").user.is_none());
    }

    #[test]
    fn issue_up_to_max_then_rejects_with_limit_exceeded() {
        let store = SessionStore::new();
        let now = Instant::now();

        for i in 0..MAX_SESSIONS {
            store
                .issue(ctx("tenant"), now)
                .unwrap_or_else(|_| panic!("issue #{i} must succeed"));
        }
        assert_eq!(store.active_sessions(), MAX_SESSIONS);

        let err = store
            .issue(ctx("tenant"), now)
            .expect_err("issue beyond MAX_SESSIONS must fail");
        assert!(matches!(err, IssueError::LimitExceeded));
        assert_eq!(
            err.error_class(),
            engine::error_format::ErrorClass::ConnectionLimitExceeded
        );
    }

    #[test]
    fn ttl_boundary_expires_exactly_at_ttl() {
        let store = SessionStore::new();
        let base = Instant::now();
        let token = store.issue(ctx("tenant"), base).expect("issue");

        let just_before = base + SESSION_TTL - Duration::from_secs(1);
        assert!(store.lookup(&token, just_before).is_some());

        // just_before の lookup で issued_at が延長されないことを確認する
        // （固定 TTL 契約）。境界ちょうどで失効する。
        let at_ttl = base + SESSION_TTL;
        assert!(store.lookup(&token, at_ttl).is_none());
    }

    #[test]
    fn expired_entry_frees_slot_allowing_new_issue_beyond_max() {
        let store = SessionStore::with_limits(4, Duration::from_secs(60));
        let base = Instant::now();
        for _ in 0..4 {
            store.issue(ctx("tenant"), base).expect("issue");
        }
        assert_eq!(store.active_sessions(), 4);

        let after_ttl = base + Duration::from_secs(61);
        let token = store
            .issue(ctx("tenant"), after_ttl)
            .expect("expired entries must be swept to free a slot");
        assert_eq!(store.active_sessions(), 1);
        assert!(store.lookup(&token, after_ttl).is_some());
    }

    #[test]
    fn lookup_of_expired_token_recovers_slot() {
        let store = SessionStore::with_limits(4, Duration::from_secs(60));
        let base = Instant::now();
        let token = store.issue(ctx("tenant"), base).expect("issue");
        assert_eq!(store.active_sessions(), 1);

        let after_ttl = base + Duration::from_secs(61);
        assert!(store.lookup(&token, after_ttl).is_none());
        assert_eq!(store.active_sessions(), 0);
    }

    #[test]
    fn close_returns_true_once_then_false() {
        let store = SessionStore::new();
        let now = Instant::now();
        let token = store.issue(ctx("tenant"), now).expect("issue");

        assert!(store.close(&token, now));
        assert!(store.lookup(&token, now).is_none());
        assert_eq!(store.active_sessions(), 0);

        assert!(!store.close(&token, now), "double close must return false");
    }

    #[test]
    fn unknown_token_lookup_and_close_have_no_side_effects() {
        let store = SessionStore::new();
        let now = Instant::now();
        let issued = store.issue(ctx("tenant"), now).expect("issue");
        let unknown = SessionToken::generate().expect("generate unrelated token");

        assert!(store.lookup(&unknown, now).is_none());
        assert!(!store.close(&unknown, now));
        assert_eq!(store.active_sessions(), 1);
        assert!(store.lookup(&issued, now).is_some());
    }

    #[test]
    fn tokens_resolve_only_to_their_own_tenant_context() {
        let store = SessionStore::new();
        let now = Instant::now();
        let token_a = store.issue(ctx("tenant-a"), now).expect("issue a");
        let token_b = store.issue(ctx("tenant-b"), now).expect("issue b");

        assert_eq!(store.lookup(&token_a, now), Some(ctx("tenant-a")));
        assert_eq!(store.lookup(&token_b, now), Some(ctx("tenant-b")));
        assert_ne!(
            store.lookup(&token_a, now),
            store.lookup(&token_b, now),
            "tokens must not cross-resolve to another tenant's context"
        );
    }

    #[test]
    fn concurrent_issue_never_exceeds_max_sessions() {
        const MAX: usize = 16;
        let store = SessionStore::with_limits(MAX, Duration::from_secs(60));
        let now = Instant::now();

        let results: Vec<bool> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..(MAX * 2))
                .map(|_| {
                    let store = store.clone();
                    scope.spawn(move || store.issue(ctx("tenant"), now).is_ok())
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("thread join"))
                .collect()
        });

        let successes = results.iter().filter(|ok| **ok).count();
        assert_eq!(successes, MAX, "exactly MAX issues may succeed");
        assert!(store.active_sessions() <= MAX);
    }

    #[test]
    fn debug_output_does_not_leak_tokens_or_tenant_ids() {
        let store = SessionStore::new();
        let now = Instant::now();
        let token = store.issue(ctx("super-secret-tenant"), now).expect("issue");
        let encoded = token.encoded();

        let debug_output = format!("{store:?}");
        assert!(!debug_output.contains(&encoded));
        assert!(!debug_output.contains("super-secret-tenant"));
        assert!(debug_output.contains("active_sessions"));
    }

    #[test]
    fn with_limits_uses_given_bounds_not_defaults() {
        let store = SessionStore::with_limits(1, Duration::from_secs(1));
        let now = Instant::now();
        store.issue(ctx("tenant"), now).expect("first issue");
        assert!(matches!(
            store.issue(ctx("tenant"), now),
            Err(IssueError::LimitExceeded)
        ));
    }

    #[test]
    fn issue_defaults_ddl_allowed_to_false() {
        // 既定の `issue`（`issue_with_ddl(ctx, false, now)` の薄いラッパー）は
        // DDL 実行権限を一切付与しない（fail-closed。NOSQL-13・TASK-207、
        // Issue #910）。
        let store = SessionStore::new();
        let now = Instant::now();
        let token = store.issue(ctx("tenant"), now).expect("issue");
        let grant = store.lookup_grant(&token, now).expect("lookup_grant");
        assert!(!grant.ddl_allowed);
        assert_eq!(grant.ctx, ctx("tenant"));
    }

    #[test]
    fn issue_with_ddl_true_round_trips_via_lookup_grant() {
        let store = SessionStore::new();
        let now = Instant::now();
        let token = store
            .issue_with_ddl(ctx("tenant"), true, now)
            .expect("issue_with_ddl");
        let grant = store.lookup_grant(&token, now).expect("lookup_grant");
        assert!(grant.ddl_allowed);
    }

    #[test]
    fn lookup_grant_expires_at_ttl_like_lookup() {
        let store = SessionStore::with_limits(4, Duration::from_secs(60));
        let base = Instant::now();
        let token = store
            .issue_with_ddl(ctx("tenant"), true, base)
            .expect("issue");
        let after_ttl = base + Duration::from_secs(61);
        assert!(store.lookup_grant(&token, after_ttl).is_none());
    }

    #[test]
    fn default_new_matches_production_constants() {
        let store = SessionStore::new();
        assert_eq!(store.limiter.max(), MAX_SESSIONS);
        assert_eq!(store.ttl, SESSION_TTL);
    }

    #[test]
    fn scope_is_bound_at_issue_and_returned_by_lookup_grant() {
        use crate::http::session::scope::scope_for_test;
        let store = SessionStore::new();
        let now = Instant::now();
        let scope = scope_for_test(&["secret_table"], true);
        let token = store
            .issue_with_scope(ctx("tenant"), false, None, Some(scope.clone()), now)
            .expect("issue");
        let grant = store.lookup_grant(&token, now).expect("grant");
        assert_eq!(grant.scope.as_deref(), Some(&scope));
        assert!(!format!("{grant:?}").contains("secret_table"));
    }

    #[test]
    fn legacy_issue_entrypoints_have_no_scope() {
        let store = SessionStore::new();
        let now = Instant::now();
        for token in [
            store.issue(ctx("t"), now).expect("issue"),
            store.issue_with_ddl(ctx("t"), true, now).expect("ddl"),
            store
                .issue_with_grant(ctx("t"), false, None, now)
                .expect("grant"),
        ] {
            assert!(store
                .lookup_grant(&token, now)
                .expect("grant")
                .scope
                .is_none());
        }
    }

    #[test]
    fn scope_disappears_after_ttl_and_close() {
        use crate::http::session::scope::scope_for_test;
        let store = SessionStore::new();
        let base = Instant::now();
        let scoped = || Some(scope_for_test(&["docs"], false));
        let t1 = store
            .issue_with_scope(ctx("t"), false, None, scoped(), base)
            .expect("issue");
        assert!(store.lookup_grant(&t1, base + SESSION_TTL).is_none());
        let t2 = store
            .issue_with_scope(ctx("t"), false, None, scoped(), base)
            .expect("issue");
        assert!(store.close(&t2, base));
        assert!(store.lookup_grant(&t2, base).is_none());
    }
}
