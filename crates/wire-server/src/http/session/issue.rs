//! `POST /v1/session` の発行パイプライン本体（Issue #752・TASK-174・
//! HTTP-4・HTTP-6。関連 HTTP-5・HTTP-7。ポインタ: `docs/spec/05-tasks.md`
//! TASK-174・`docs/spec/04-behavior/http-transport.md` HTTP-4・HTTP-6）。
//!
//! 呼び出し文脈: [`crate::http::router::Router`]（新設ルータ）が `target ==
//! "/v1/session"` を一致させたときにのみ [`handle`] を呼ぶ。本モジュールは
//! ソケット I/O を一切持たず、本文バイト列（[`crate::http::conn::Request::body`]
//! 由来）と時刻 2 種（単調時計・壁時計）だけを受け取り、応答バイト列
//! （[`Vec<u8>`]）を返す純粋な関数として実装する（`handle` を直接単体テストできる
//! ようにするための設計）。
//!
//! 手順（fail-closed。順序は契約の一部）:
//! 1. [`crate::http::body::body_as_utf8`] で本文を UTF-8 文字列へ昇格（不正なら
//!    `42601`）
//! 2. [`engine::json::parse_json`] で構文解析（不正なら `42601`）
//! 3. [`SESSION_REQUEST_SCHEMA`]（`user`・`password` の 2 つの必須文字列
//!    フィールドのみを許可する [`crate::http::query::schema::ObjectSchema`]）で
//!    意味検証。必須欠落・未知キー・型不一致はいずれも `42601`（未知キーの拒否
//!    により、クライアント自己申告の `tenant_id` 等も無視せず拒否する）
//!    続けて任意キー `scope` を [`scope::parse_scope`] で検証する（形の不正は `42601`、表名件数の
//!    超過は `54000`。HTTP-15・Issue #2100）。妥当なスコープ付きの発行は、強制（Issue #2101）が
//!    結線されるまで `0A000` の固定文言で拒否する（認証より前。判定順は 42601→0A000→28P01）
//! 4. [`crate::auth::verify`] で Argon2id 照合。失敗は `28P01`（HTTP 401）で、
//!    固定遅延・ダミー KDF による対称性は `verify` 内部が担う（本モジュールは
//!    追加の `sleep` を行わない）
//! 5. 認証成功後にのみ [`SessionStore::issue`] を呼びトークンを発行する
//!    （認証成功より前にセッション上限の判定を行わない。未認証クライアントへ
//!    KDF を経ない高速経路・セッション数のオラクルを与えないための順序）。
//!    上限超過等は [`IssueError::error_class`] が返す分類（`53300`／`XX000`）
//!    へ写像する
//! 6. 成功時は `{"token":"<...>","expires_in":<SESSION_TTL の秒数>}` を
//!    `200 OK` で返す
//!
//! いずれの応答（成功・失敗）にもテナント ID・ユーザー名を含めない
//! （security.md「エラー・ログ経由で他テナントのデータ・存在情報を漏らさない」）。

use std::time::{Instant, SystemTime};

use engine::error_format::{ClassifiedError, ErrorClass};
use engine::json::{parse_json, JsonValue};

use crate::auth::{self, UserStore};
use crate::http::error_body::escape_json_string_into;
use crate::http::query::schema::{FieldSpec, FieldType, ObjectSchema, Presence};
use crate::http::response;
use crate::http::session::scope::{self, SCOPE_SCHEMA};
use crate::http::session::store::SessionStore;
use crate::http::{body, session};

/// `POST /v1/session` の要求本文スキーマ: `user`・`password` の 2 つの必須
/// 文字列フィールドのみを許可する（未知キーは [`crate::http::query::schema`]
/// の一般則により拒否される。`nullable: false` が既定のため `null` も拒否）。
static SESSION_REQUEST_SCHEMA: ObjectSchema = ObjectSchema {
    name: "session",
    fields: &[
        FieldSpec {
            key: "user",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "password",
            presence: Presence::Required,
            ty: FieldType::String,
            nullable: false,
        },
        FieldSpec {
            key: "scope",
            presence: Presence::Optional,
            ty: FieldType::Object(&SCOPE_SCHEMA),
            nullable: false,
        },
    ],
};

/// スコープ付き発行を拒否する固定文言。スコープの強制（Issue #2101）が結線されるまで、効いていない
/// スコープのトークンを成功として渡さない（権限を狭めたつもりが全権限になる成功の偽装を避ける）。
const SESSION_SCOPE_NOT_YET_AVAILABLE_MESSAGE: &str = "session scope is not yet supported";

/// [`handle`] 内部の分類済みエラー（`ErrorClass` ＋ クライアント向け文言）。
/// `message` を `Cow<'static, str>` にするのは、[`engine::json::JsonError::
/// client_message`] のように文言を `String` で返す型（SSOT）をそのまま
/// 使い、固定文言の複製を作らないため（`Owned` は engine 側の SSOT 由来、
/// `Borrowed` は本モジュールが自前で決める固定英語文言）。[`handle`] が
/// 応答バイト列へ写像する唯一の消費者。
struct HandleError {
    class: ErrorClass,
    message: std::borrow::Cow<'static, str>,
}

impl HandleError {
    fn new(class: ErrorClass, message: impl Into<std::borrow::Cow<'static, str>>) -> Self {
        Self {
            class,
            message: message.into(),
        }
    }
}

/// `POST /v1/session` を処理し応答バイト列を返す。[`crate::http::router::Router`]
/// から呼ばれる唯一の入口。
///
/// `now_mono` は [`SessionStore::issue`] の TTL 起点（単調時計）を取得する
/// サンク（`Fn() -> Instant`）で、本文パース・スキーマ検証・Argon2id 照合が
/// すべて成功した**後**、`SessionStore::issue` を呼ぶ直前にのみ 1 回呼ぶ
/// （即値 `Instant` ではなくサンクにするのは、呼び出し元が要求受理時点の
/// 時刻を先取りして渡すと、Argon2id 計算・セマフォ待機の時間ぶん TTL 起点が
/// 早まり「発行時刻起点で TTL 開始」という `SessionStore` の契約より実質的に
/// 短い TTL になってしまうため。Issue #813 codex-review P1 指摘）。
/// `now_wall` は応答の `Date` ヘッダ（壁時計）に使う。production は
/// `Instant::now`（関数参照）／`SystemTime::now()` を渡し、決定的単体テストは
/// 固定時刻を返すクロージャを注入できる。
pub fn handle(
    users: &UserStore,
    sessions: &SessionStore,
    body: &[u8],
    now_mono: impl Fn() -> Instant,
    now_wall: SystemTime,
) -> Vec<u8> {
    handle_with_engine(users, None, sessions, body, now_mono, now_wall)
}

/// [`handle`] の engine つき版（HTTP-4・Issue #2238）。`engine` があるときは、認証済みユーザーに対応する
/// カタログのロールへセッションを束縛し、ブートストラップ済みテナントでは資格情報の照合元もカタログに
/// なる（pg wire の cleartext 経路と同じ [`crate::auth::users_file_role::verify_and_bind`] を使う）。
/// `engine` が `None`（`Router::new`）のときは従来どおり users ファイルだけで照合し、未束縛のままにする。
pub fn handle_with_engine(
    users: &UserStore,
    engine: Option<&engine::core::EngineCore>,
    sessions: &SessionStore,
    body: &[u8],
    now_mono: impl Fn() -> Instant,
    now_wall: SystemTime,
) -> Vec<u8> {
    match handle_inner(users, engine, sessions, body, now_mono) {
        Ok(token_encoded) => {
            response::encode_ok(&success_body(&token_encoded, sessions.ttl()), now_wall)
        }
        Err(e) => response::encode_error(e.class, &e.message, now_wall),
    }
}

/// [`handle`] の本体。成功時はトークンの base64url 表現（`String`）のみを返し、
/// 応答バイト列の組み立て（`now_wall` を要する）は呼び出し元へ委ねる
/// （ソケット I/O・時刻参照を持たない純粋な処理段として単体テストしやすくする
/// ための分離）。
fn handle_inner(
    users: &UserStore,
    engine: Option<&engine::core::EngineCore>,
    sessions: &SessionStore,
    raw_body: &[u8],
    now_mono: impl Fn() -> Instant,
) -> Result<String, HandleError> {
    let text = body::body_as_utf8(raw_body)
        .map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;

    let value: JsonValue =
        parse_json(text).map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;

    // `SchemaError::client_message()`（`missing required field: <key>`／
    // `type mismatch for field: <key>` 等）はフィールド名を含むため、本
    // エンドポイントでは意図的に使わず固定の一般文言へ丸める（`user`／
    // `password` は認証情報に近く、どの必須フィールドが欠けている／型が
    // 違うかを未認証の呼び出し元へ細分化して開示しない判断。`wire_code`＝
    // `42601` は共有する）。
    let validated = SESSION_REQUEST_SCHEMA.validate(&value).map_err(|_| {
        HandleError::new(ErrorClass::UnsupportedSqlSyntax, "invalid session request")
    })?;

    // スコープの形の検証（42601／54000）。サーバー状態を読まず、認証より前に判定する。
    let parsed_scope = scope::parse_scope(&validated)
        .map_err(|e| HandleError::new(e.error_class(), e.client_message()))?;
    // 妥当なスコープ付きの発行は、強制が未実装の間は 0A000 で拒否する（認証・セッション枠・KDF を
    // 消費しない。判定は公開済みの語彙と識別子の形だけに依存するため情報は漏れない）。
    // Issue #2101: この拒否を削除し（挙動変更のため破壊的変更として扱う）、`parse_scope` の結果を
    // `issue_with_scope` へ渡し、発行から強制までの e2e テストを追加する。
    if parsed_scope.is_some() {
        return Err(HandleError::new(
            ErrorClass::FeatureNotSupported,
            SESSION_SCOPE_NOT_YET_AVAILABLE_MESSAGE,
        ));
    }

    let user = validated.required_str("user").map_err(|_| {
        HandleError::new(ErrorClass::UnsupportedSqlSyntax, "invalid session request")
    })?;
    let password = validated.required_str("password").map_err(|_| {
        HandleError::new(ErrorClass::UnsupportedSqlSyntax, "invalid session request")
    })?;

    // 認証（固定遅延・ダミー KDF による対称性は `auth::verify` 内部が担う。
    // ここでは早期 return・別 sleep を追加しない）。
    let (ctx, binding) =
        auth::users_file_role::verify_and_bind(users, engine, user, password.as_bytes())
            .map_err(|_| HandleError::new(ErrorClass::AuthInvalid, auth::AuthFailure::MESSAGE))?;
    // 同名ロールへ束縛した場合の判定（検証の後。パスワードを知らない相手には他の失敗と区別できない）。
    // `NOLOGIN` は pg wire と同じ `28000`（固定文言）。HTTP では `CONNECTION LIMIT` を適用しない。
    let bound = match binding {
        auth::users_file_role::UsersFileBinding::Bound(b) => {
            if !b.login {
                return Err(HandleError::new(
                    ErrorClass::AuthRequired,
                    "role is not permitted to log in",
                ));
            }
            Some(b)
        }
        _ => None,
    };

    // DDL 実行権限（NOSQL-13・TASK-207、Issue #910）: `auth::verify` 成功
    // *後*にのみ `UserStore::is_ddl_allowed` を読み、セッションへ 1 回だけ
    // 確定させる（未認証経路で権限表を参照しない。pg wire 側
    // `handshake.rs` の `session.allow_ddl()` 呼び出しと同じ「認証成功後」の
    // 順序）。値はセッション発行後に変化しない——DDL 実行権限ゲート自体は
    // 引き続き `engine::sql::ddl::require_ddl_permission` 一箇所が担う。
    // ブートストラップ済みテナントでは、`--ddl-allowed-users` の列挙と永続の DDL フラグの積にする
    // （pg wire の `handshake.rs` と同じ。Issue #2238）。
    let ddl_allowed = users.is_ddl_allowed(user)
        && match &bound {
            Some(b) if b.bootstrapped => b.ddl_flag,
            _ => true,
        };
    let role = bound.map(|b| session::store::SessionRole {
        role_id: b.role_id,
        role_name: b.role_name,
        all_object_privileges: b.all_object_privileges,
    });

    // セッション上限の判定は認証成功後にのみ行う（未認証クライアントへ
    // KDF を経ない高速経路・セッション数のオラクルを与えないための順序）。
    // `now_mono()` の呼び出し自体もここまで遅延させ、Argon2id 照合・セマフォ
    // 待機の時間が TTL 起点に含まれてしまわないようにする（本関数 doc 参照）。
    let token = sessions
        .issue_with_grant(ctx, ddl_allowed, role, now_mono())
        .map_err(|e| HandleError::new(e.error_class(), issue_error_message(&e)))?;

    Ok(token.encoded())
}

/// [`session::store::IssueError`] → クライアント向け固定英語文言（内部詳細を
/// 反映しない。TokenGeneration の `io::Error` 詳細もここでは使わない）。
fn issue_error_message(e: &session::store::IssueError) -> &'static str {
    match e {
        session::store::IssueError::LimitExceeded => "session limit exceeded",
        session::store::IssueError::TokenGeneration(_) | session::store::IssueError::Internal => {
            "internal error"
        }
    }
}

/// 成功本文（`{"token":"...","expires_in":<秒数>}`）を組み立てる。トークンは
/// base64url アルファベットのみで JSON エスケープ不要な文字集合だが、
/// 将来の表現変更への防御として [`escape_json_string_into`] を通す。
///
/// `ttl` は呼び出し元（[`handle`]）が実際にトークンを発行した
/// [`SessionStore`] の [`SessionStore::ttl`] から取る。固定定数
/// `crate::limits::SESSION_TTL` を直接使わないのは、[`SessionStore::with_limits`] で
/// 既定と異なる TTL を注入したストアを `Router::new` が受け取った場合に
/// 応答の `expires_in` と実際の失効時刻が乖離しないようにするため
/// （Issue #813 codex-review P2 指摘）。
fn success_body(token_encoded: &str, ttl: std::time::Duration) -> String {
    let mut out = String::with_capacity(64);
    out.push('{');
    out.push_str("\"token\":\"");
    escape_json_string_into(&mut out, token_encoded);
    out.push_str("\",\"expires_in\":");
    out.push_str(&ttl.as_secs().to_string());
    out.push('}');
    out
}

#[cfg(test)]
mod tests {
    /// フィクスチャ用一時ディレクトリを `Drop` で削除するガード（Issue #1303）。
    /// 削除失敗はテストを失敗させず stderr へ出すのみ。
    struct DirCleanup(std::path::PathBuf);

    impl Drop for DirCleanup {
        fn drop(&mut self) {
            if let Err(e) = std::fs::remove_dir_all(&self.0) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!(
                        "warning: failed to remove temp dir {}: {e}",
                        self.0.display()
                    );
                }
            }
        }
    }

    use super::*;
    use crate::auth::argon2id;
    use crate::limits::SESSION_TTL;

    fn store_with(records: &[(&str, &str, &str)]) -> UserStore {
        let mut content = String::new();
        for (username, tenant_id, password) in records {
            let salt = b"0123456789abcdef";
            let phc =
                argon2id::encode_phc(password.as_bytes(), salt, &argon2id::RECOMMENDED_PARAMS)
                    .expect("valid phc encoding");
            content.push_str(&format!("{username}:{tenant_id}:{phc}\n"));
        }
        let dir = std::env::temp_dir().join(format!(
            "wire-server-session-issue-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::create_dir(&dir).expect("create unique fixture dir");
        let _dir_cleanup = DirCleanup(dir.clone());
        let path = dir.join("users.txt");
        std::fs::write(&path, content).expect("write fixture");
        UserStore::load_from_file(&path).expect("valid user store")
    }

    #[test]
    fn valid_credentials_return_200_with_token_and_ttl() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let body = br#"{"user":"alice","password":"pw-alice"}"#;

        let response = handle(&store, &sessions, body, Instant::now, SystemTime::now());
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 200 "), "got: {text}");
        assert!(text.contains("\"expires_in\":3600"), "got: {text}");
        assert!(!text.contains("tenant-a"), "must not leak tenant id");
    }

    #[test]
    fn unknown_user_rejects_with_28p01() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let body = br#"{"user":"bob","password":"whatever"}"#;

        let response = handle(&store, &sessions, body, Instant::now, SystemTime::now());
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 401 "), "got: {text}");
        assert!(text.contains("28P01"), "got: {text}");
    }

    #[test]
    fn wrong_password_rejects_with_28p01() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let body = br#"{"user":"alice","password":"wrong"}"#;

        let response = handle(&store, &sessions, body, Instant::now, SystemTime::now());
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 401 "), "got: {text}");
        assert!(text.contains("28P01"), "got: {text}");
    }

    #[test]
    fn missing_password_field_rejects_with_42601() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let body = br#"{"user":"alice"}"#;

        let response = handle(&store, &sessions, body, Instant::now, SystemTime::now());
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn unknown_field_rejects_with_42601() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let body = br#"{"user":"alice","password":"pw-alice","tenant_id":"other"}"#;

        let response = handle(&store, &sessions, body, Instant::now, SystemTime::now());
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn invalid_json_rejects_with_42601() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let body = b"not json";

        let response = handle(&store, &sessions, body, Instant::now, SystemTime::now());
        let text = String::from_utf8(response).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 400 "), "got: {text}");
        assert!(text.contains("42601"), "got: {text}");
    }

    #[test]
    fn session_limit_exceeded_rejects_with_53300_only_after_successful_auth() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::with_limits(1, SESSION_TTL);
        let body = br#"{"user":"alice","password":"pw-alice"}"#;

        let first = handle(&store, &sessions, body, Instant::now, SystemTime::now());
        assert!(String::from_utf8_lossy(&first).starts_with("HTTP/1.1 200 "));

        let second = handle(&store, &sessions, body, Instant::now, SystemTime::now());
        let text = String::from_utf8(second).expect("utf-8 response");
        assert!(text.starts_with("HTTP/1.1 503 "), "got: {text}");
        assert!(text.contains("53300"), "got: {text}");
    }

    #[test]
    fn ddl_allowed_user_session_carries_ddl_permission() {
        // NOSQL-13・TASK-207、Issue #910: `is_ddl_allowed` なユーザーの
        // セッションのみ `ddl_allowed` を持つ（handshake.rs の
        // `session.allow_ddl()` と同じ「認証成功後に確定」順序）。
        // `handle_inner` を直接使い、発行済みトークンを
        // `SessionStore::lookup_grant` へ通して実際のフラグ値を検証する
        // （`handle` の応答本文は `ddl_allowed` を含まない契約のため）。
        use crate::http::session::token::SessionToken;

        let store = store_with(&[
            ("alice", "tenant-a", "pw-alice"),
            ("bob", "tenant-a", "pw-bob"),
        ]);
        let store = store
            .with_ddl_allowed_users(&["alice".to_string()])
            .expect("alice is a known username");
        let sessions = SessionStore::new();
        let now = Instant::now();

        let alice_token_encoded = handle_inner(
            &store,
            None,
            &sessions,
            br#"{"user":"alice","password":"pw-alice"}"#,
            move || now,
        )
        .map_err(|_| "alice login must succeed")
        .expect("alice login succeeds");
        let bob_token_encoded = handle_inner(
            &store,
            None,
            &sessions,
            br#"{"user":"bob","password":"pw-bob"}"#,
            move || now,
        )
        .map_err(|_| "bob login must succeed")
        .expect("bob login succeeds");

        let alice_token = SessionToken::parse(&alice_token_encoded).expect("parse alice token");
        let bob_token = SessionToken::parse(&bob_token_encoded).expect("parse bob token");

        assert!(
            sessions
                .lookup_grant(&alice_token, now)
                .expect("alice session")
                .ddl_allowed
        );
        assert!(
            !sessions
                .lookup_grant(&bob_token, now)
                .expect("bob session")
                .ddl_allowed
        );
    }

    #[test]
    fn two_successful_logins_yield_different_tokens() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let body = br#"{"user":"alice","password":"pw-alice"}"#;

        let first = handle(&store, &sessions, body, Instant::now, SystemTime::now());
        let second = handle(&store, &sessions, body, Instant::now, SystemTime::now());
        assert_ne!(first, second, "reissued tokens must differ");
    }

    fn status_and_body(response: &[u8]) -> (String, String) {
        let text = String::from_utf8(response.to_vec()).expect("utf-8 response");
        let status = text.lines().next().unwrap_or_default().to_string();
        let body = text
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        (status, body)
    }

    #[test]
    fn valid_scope_is_rejected_with_0a000_before_authentication() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        // 正しいパスワードでも誤ったパスワードでも同一応答（認証結果を観測させない）。
        let ok = br#"{"user":"alice","password":"pw-alice","scope":{"tables":["docs"]}}"#;
        let bad = br#"{"user":"alice","password":"wrong","scope":{"tables":["docs"]}}"#;
        let r1 = handle(&store, &sessions, ok, Instant::now, SystemTime::now());
        let r2 = handle(&store, &sessions, bad, Instant::now, SystemTime::now());
        let (s1, b1) = status_and_body(&r1);
        let (s2, b2) = status_and_body(&r2);
        assert!(s1.starts_with("HTTP/1.1 501 "), "got: {s1}");
        assert!(b1.contains("0A000") && b1.contains(SESSION_SCOPE_NOT_YET_AVAILABLE_MESSAGE));
        assert_eq!((s1, b1), (s2, b2));
        assert_eq!(sessions.active_sessions(), 0);
    }

    #[test]
    fn scope_response_does_not_depend_on_table_existence() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let a = br#"{"user":"alice","password":"pw-alice","scope":{"tables":["exists_maybe"]}}"#;
        let b = br#"{"user":"alice","password":"pw-alice","scope":{"tables":["other_tenant_t"]}}"#;
        let ra = handle(&store, &sessions, a, Instant::now, SystemTime::now());
        let rb = handle(&store, &sessions, b, Instant::now, SystemTime::now());
        assert_eq!(status_and_body(&ra), status_and_body(&rb));
    }

    #[test]
    fn malformed_scope_is_rejected_with_42601_without_echo() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        for body in [
            &br#"{"user":"alice","password":"pw-alice","scope":{"ops":["zzz_op"]}}"#[..],
            br#"{"user":"alice","password":"pw-alice","scope":{"tables":["bad.name"]}}"#,
            br#"{"user":"alice","password":"pw-alice","scope":{"zzz_key":1}}"#,
            br#"{"user":"alice","password":"pw-alice","scope":null}"#,
        ] {
            let r = handle(&store, &sessions, body, Instant::now, SystemTime::now());
            let (status, text) = status_and_body(&r);
            assert!(status.starts_with("HTTP/1.1 400 "), "got: {status}");
            assert!(text.contains("42601"));
            for leaked in ["zzz_op", "bad.name", "zzz_key"] {
                assert!(!text.contains(leaked), "leaked {leaked}: {text}");
            }
        }
        assert_eq!(sessions.active_sessions(), 0);
    }

    #[test]
    fn too_many_scope_tables_is_rejected_with_54000() {
        let store = store_with(&[("alice", "tenant-a", "pw-alice")]);
        let sessions = SessionStore::new();
        let names: Vec<String> = (0..=crate::limits::MAX_SCOPE_TABLES)
            .map(|i| format!("\"t{i}\""))
            .collect();
        let body = format!(
            r#"{{"user":"alice","password":"pw-alice","scope":{{"tables":[{}]}}}}"#,
            names.join(",")
        );
        let r = handle(
            &store,
            &sessions,
            body.as_bytes(),
            Instant::now,
            SystemTime::now(),
        );
        let (status, text) = status_and_body(&r);
        assert!(status.starts_with("HTTP/1.1 413 "), "got: {status}");
        assert!(text.contains("54000"));
    }
}
