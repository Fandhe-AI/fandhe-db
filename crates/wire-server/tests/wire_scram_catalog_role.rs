//! カタログのロール（`CREATE ROLE ... LOGIN PASSWORD`）による SCRAM-SHA-256 認証の結合テスト
//! （SQL-39・WIRE-18、Issue #2233）。
//!
//! 固定する契約: ログイン識別子（`EngineCore::login_identifier`）を user に渡した SCRAM 接続が
//! ロールの所属テナントで成立すること、ロール不在・`VALID UNTIL` 超過・誤パスワードが応答バイトまで
//! 同一の `28P01`（固定遅延以上）になること、期限切れロールの server-first の salt が不在と区別できない
//! こと、`NOLOGIN` は検証の後にだけ `28000`、`CONNECTION LIMIT` 超過は認証成功の後の `53300`、
//! 改名・削除への追随、他テナントのロール名の拒否、users ファイルのユーザーの回帰。
//!
//! 時間を計測するため `tests/` 直下に単独で置く（束ねない。`.claude/rules/coding-rust.md`）。サーバーは
//! ポート 0・一意な一時ディレクトリで独立させる。RFC 5802 のクライアント計算は `wire_scram_auth.rs` と
//! 同様に公開 API だけで独立に行う。

#[path = "common/fixture_guard.rs"]
mod fixture_guard;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine::core::EngineCore;
use wire_server::auth::hmac_sha256::{hmac_sha256, pbkdf2_hmac_sha256_one_block};
use wire_server::auth::{argon2id, base64_std, scram, UserStore};

const MOCK_KEY_SECRET: &[u8] = b"wire-scram-catalog-role-test-mock-key!!";
const TENANT: &str = "tenant-a";

static SEQ: AtomicU64 = AtomicU64::new(0);

/// 一時ディレクトリ（DB と users ファイルを置く）。drop で削除する。
struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-scram-catalog-role-{label}-{}-{}-{seq}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        std::fs::create_dir(&dir).expect("create unique dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// テスト用サーバー一式（engine 付き・SCRAM 方式）。
struct Fixture {
    _dir: TempDir,
    core: Arc<EngineCore>,
    addr: std::net::SocketAddr,
}

impl Fixture {
    /// users ファイルには `ops`（tenant-a・パスワード `pw-ops`）を 1 件置く（回帰確認用）。
    fn start(label: &str) -> Self {
        Self::start_with(label, &[])
    }

    /// `ddl_users` は `--ddl-allowed-users` 相当（Issue #1971 の優先関係の確認用）。
    fn start_with(label: &str, ddl_users: &[&str]) -> Self {
        let dir = TempDir::new(label);
        let users = dir.0.join("users.txt");
        let phc = argon2id::encode_phc(
            b"unused",
            b"0123456789abcdef",
            &argon2id::RECOMMENDED_PARAMS,
        )
        .expect("phc");
        let verifier =
            scram::generate_verifier(b"pw-ops", &[7u8; scram::SALT_LEN], scram::SCRAM_ITERATIONS)
                .expect("verifier");
        std::fs::write(
            &users,
            format!("ops:{TENANT}:{phc}:{}\n", verifier.to_verifier_string()),
        )
        .expect("write users");
        let store = Arc::new(
            UserStore::load_from_file(&users)
                .expect("users")
                .require_scram(MOCK_KEY_SECRET)
                .expect("scram")
                .with_ddl_allowed_users(
                    &ddl_users.iter().map(|u| u.to_string()).collect::<Vec<_>>(),
                )
                .expect("ddl users"),
        );
        let core = Arc::new(EngineCore::open(dir.0.join("db.redb")).expect("open engine"));
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (s2, c2) = (Arc::clone(&store), Arc::clone(&core));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (s3, c3) = (Arc::clone(&s2), Arc::clone(&c2));
                std::thread::spawn(move || {
                    let _ = wire_server::handshake::handle_connection_with_engine(stream, &s3, &c3);
                });
            }
        });
        Self {
            _dir: dir,
            core,
            addr,
        }
    }

    fn sql(&self, tenant: &str, sql: &str) {
        let ctx = engine::policy::PolicyContext::with_visibilities(
            tenant,
            [
                engine::storage::Visibility::Public,
                engine::storage::Visibility::Private,
            ],
        )
        .expect("ctx");
        let mut session = engine::sql::mode::SessionState::default();
        session.allow_ddl();
        self.core
            .execute_sql_in_session(&ctx, &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql} must succeed: {e}"));
    }

    fn ident(&self, tenant: &str, role: &str) -> String {
        EngineCore::login_identifier(tenant, role).expect("identifier")
    }

    fn login(&self, user: &str, password: &[u8]) -> Login {
        scram_login(self.addr, user, password)
    }
}

/// 接続の結果。
enum Login {
    /// ReadyForQuery まで到達した（ストリームを保持して接続を維持する）。
    Ready(#[allow(dead_code)] TcpStream),
    /// ErrorResponse で拒否された。
    Rejected {
        body: Vec<u8>,
        /// AuthenticationOk の後に拒否されたか（検証が通った後の FATAL）。
        after_ok: bool,
        /// client-final の送信から ErrorResponse 受信までの時間（検証失敗の固定遅延の観測用）。
        elapsed: Duration,
        /// server-first の salt（モック検証子との区別不能性の確認用）。
        salt: Vec<u8>,
    },
}

fn sqlstate(body: &[u8]) -> String {
    field(body, b'C')
}

fn field(body: &[u8], want: u8) -> String {
    let mut pos = 0usize;
    while pos < body.len() {
        let code = body[pos];
        if code == 0 {
            break;
        }
        pos += 1;
        let start = pos;
        while pos < body.len() && body[pos] != 0 {
            pos += 1;
        }
        let value = String::from_utf8_lossy(&body[start..pos]).into_owned();
        pos += 1;
        if code == want {
            return value;
        }
    }
    panic!("no field {want} in {body:?}");
}

fn read_message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    (header[0], body)
}

fn auth_code(body: &[u8]) -> i32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&body[..4]);
    i32::from_be_bytes(b)
}

/// 生バイトの SCRAM クライアント（RFC 5802）。`password` の正誤は呼び出し側が決める。
fn scram_login(addr: std::net::SocketAddr, user: &str, password: &[u8]) -> Login {
    scram_login_with_hook(addr, user, password, &|| {})
}

/// [`scram_login`] に、server-first を受け取った後（client-final を送る前）に走らせる処理を足したもの。
/// 認証の待機中にロールの属性が変わる状況を作るために使う。
fn scram_login_with_hook(
    addr: std::net::SocketAddr,
    user: &str,
    password: &[u8],
    after_server_first: &dyn Fn(),
) -> Login {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("timeout");
    let mut params = Vec::new();
    params.extend_from_slice(b"user\0");
    params.extend_from_slice(user.as_bytes());
    params.push(0);
    params.push(0);
    let mut startup = Vec::new();
    startup.extend_from_slice(&((4 + 4 + params.len()) as i32).to_be_bytes());
    startup.extend_from_slice(&0x0003_0000i32.to_be_bytes());
    startup.extend_from_slice(&params);
    stream.write_all(&startup).expect("startup");

    // AuthenticationSASL
    let (ty, body) = read_message(&mut stream);
    assert_eq!(ty, b'R');
    assert_eq!(auth_code(&body), 10);

    let client_first_bare = b"n=,r=catalog-role-client-nonce".to_vec();
    let mut client_first = b"n,,".to_vec();
    client_first.extend_from_slice(&client_first_bare);
    let mut msg_body = Vec::new();
    msg_body.extend_from_slice(scram::MECHANISM_NAME.as_bytes());
    msg_body.push(0);
    msg_body.extend_from_slice(&(client_first.len() as i32).to_be_bytes());
    msg_body.extend_from_slice(&client_first);
    let mut msg = vec![b'p'];
    msg.extend_from_slice(&((4 + msg_body.len()) as i32).to_be_bytes());
    msg.extend_from_slice(&msg_body);
    stream.write_all(&msg).expect("initial response");

    // AuthenticationSASLContinue
    let (ty, body) = read_message(&mut stream);
    assert_eq!(ty, b'R');
    assert_eq!(auth_code(&body), 11);
    let server_first = body[4..].to_vec();
    let text = std::str::from_utf8(&server_first).expect("utf8");
    let (mut nonce, mut salt, mut iterations) = (String::new(), Vec::new(), 0u32);
    for f in text.split(',') {
        if let Some(v) = f.strip_prefix("r=") {
            nonce = v.to_string();
        } else if let Some(v) = f.strip_prefix("s=") {
            salt = base64_std::decode(v).expect("salt");
        } else if let Some(v) = f.strip_prefix("i=") {
            iterations = v.parse().expect("iterations");
        }
    }

    after_server_first();

    let client_final_no_proof = format!("c={},r={nonce}", base64_std::encode(b"n,,")).into_bytes();
    let mut auth_message = client_first_bare.clone();
    auth_message.push(b',');
    auth_message.extend_from_slice(&server_first);
    auth_message.push(b',');
    auth_message.extend_from_slice(&client_final_no_proof);
    let salted = pbkdf2_hmac_sha256_one_block(password, &salt, iterations);
    let client_key = hmac_sha256(&salted, b"Client Key");
    let stored_key = engine::crypto::sha256::digest(&client_key);
    let signature = hmac_sha256(&stored_key, &auth_message);
    let proof: Vec<u8> = client_key
        .iter()
        .zip(signature.iter())
        .map(|(a, b)| a ^ b)
        .collect();
    let mut client_final = client_final_no_proof;
    client_final.extend_from_slice(b",p=");
    client_final.extend_from_slice(base64_std::encode(&proof).as_bytes());
    let mut msg = vec![b'p'];
    msg.extend_from_slice(&((4 + client_final.len()) as i32).to_be_bytes());
    msg.extend_from_slice(&client_final);
    let start = Instant::now();
    stream.write_all(&msg).expect("client final");

    let (ty, body) = read_message(&mut stream);
    if ty == b'E' {
        return Login::Rejected {
            body,
            after_ok: false,
            elapsed: start.elapsed(),
            salt,
        };
    }
    assert_eq!((ty, auth_code(&body)), (b'R', 12), "SASLFinal expected");
    let (ty, body) = read_message(&mut stream);
    assert_eq!(
        (ty, auth_code(&body)),
        (b'R', 0),
        "AuthenticationOk expected"
    );
    for _ in 0..20 {
        let (ty, body) = read_message(&mut stream);
        match ty {
            b'Z' => return Login::Ready(stream),
            b'E' => {
                return Login::Rejected {
                    body,
                    after_ok: true,
                    elapsed: start.elapsed(),
                    salt,
                }
            }
            _ => {}
        }
    }
    panic!("no ReadyForQuery");
}

fn expect_rejected(l: Login) -> (Vec<u8>, bool, Duration, Vec<u8>) {
    match l {
        Login::Rejected {
            body,
            after_ok,
            elapsed,
            salt,
        } => (body, after_ok, elapsed, salt),
        Login::Ready(_) => panic!("login must be rejected"),
    }
}

fn expect_ready(l: Login) -> TcpStream {
    match l {
        Login::Ready(s) => s,
        Login::Rejected { body, .. } => panic!("login must succeed: {}", sqlstate(&body)),
    }
}

#[test]
fn catalog_role_logs_in_with_scram() {
    let f = Fixture::start("ok");
    f.sql(TENANT, "CREATE ROLE app LOGIN PASSWORD 'pw-app'");
    let _ready = expect_ready(f.login(&f.ident(TENANT, "app"), b"pw-app"));
}

#[test]
fn missing_expired_and_wrong_password_are_indistinguishable() {
    let f = Fixture::start("indist");
    f.sql(
        TENANT,
        "CREATE ROLE live LOGIN PASSWORD 'pw-live' VALID UNTIL 'infinity'",
    );
    let missing_ident = f.ident(TENANT, "later_expired");
    // 不在のときの server-first の salt（モック）。
    let (missing_body, after_ok, elapsed, missing_salt) =
        expect_rejected(f.login(&missing_ident, b"whatever"));
    assert!(!after_ok);
    assert_eq!(sqlstate(&missing_body), "28P01");
    assert!(elapsed >= wire_server::auth::AUTH_FAILURE_DELAY);

    // 同じ識別子のロールを期限切れで作る: 正しいパスワードでも同じ 28P01・同じ salt。
    f.sql(
        TENANT,
        "CREATE ROLE later_expired LOGIN PASSWORD 'pw-exp' VALID UNTIL '2000-01-01 00:00:00+00'",
    );
    let (expired_body, after_ok, elapsed, expired_salt) =
        expect_rejected(f.login(&missing_ident, b"pw-exp"));
    assert!(!after_ok);
    assert!(elapsed >= wire_server::auth::AUTH_FAILURE_DELAY);
    assert_eq!(expired_salt, missing_salt, "expired must use the mock salt");

    let (wrong_body, after_ok, elapsed, _) =
        expect_rejected(f.login(&f.ident(TENANT, "live"), b"wrong"));
    assert!(!after_ok);
    assert!(elapsed >= wire_server::auth::AUTH_FAILURE_DELAY);

    assert_eq!(missing_body, expired_body);
    assert_eq!(missing_body, wrong_body);
}

#[test]
fn nologin_is_rejected_only_after_valid_proof() {
    let f = Fixture::start("nologin");
    f.sql(TENANT, "CREATE ROLE grp NOLOGIN PASSWORD 'pw-grp'");
    let ident = f.ident(TENANT, "grp");
    let (body, after_ok, _, _) = expect_rejected(f.login(&ident, b"pw-grp"));
    assert!(after_ok);
    assert_eq!(sqlstate(&body), "28000");
    assert_eq!(field(&body, b'S'), "FATAL");
    assert!(!String::from_utf8_lossy(&body).contains(TENANT));
    // 誤ったパスワードは他の失敗と同じ 28P01（NOLOGIN かどうかは分からない）。
    let (body, after_ok, _, _) = expect_rejected(f.login(&ident, b"wrong"));
    assert!(!after_ok);
    assert_eq!(sqlstate(&body), "28P01");
}

#[test]
fn connection_limit_is_enforced_after_authentication() {
    let f = Fixture::start("limit");
    f.sql(
        TENANT,
        "CREATE ROLE one LOGIN PASSWORD 'pw-one' CONNECTION LIMIT 1",
    );
    f.sql(
        TENANT,
        "CREATE ROLE zero LOGIN PASSWORD 'pw-zero' CONNECTION LIMIT 0",
    );
    let ident = f.ident(TENANT, "one");
    let first = expect_ready(f.login(&ident, b"pw-one"));
    let (body, after_ok, _, _) = expect_rejected(f.login(&ident, b"pw-one"));
    assert!(after_ok, "limit is checked after authentication succeeds");
    assert_eq!(sqlstate(&body), "53300");
    // 誤パスワードは上限に関係なく 28P01（上限超過は認証成功後にだけ分かる）。
    let (body, after_ok, _, _) = expect_rejected(f.login(&ident, b"wrong"));
    assert!(!after_ok);
    assert_eq!(sqlstate(&body), "28P01");
    // 切断後は再び接続できる（ガードが数を戻す）。サーバー側の切断検知を待つ。
    drop(first);
    let mut ok = false;
    for _ in 0..50 {
        if let Login::Ready(_) = f.login(&ident, b"pw-one") {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(ok, "slot must be released after disconnect");
    let (body, after_ok, _, _) = expect_rejected(f.login(&f.ident(TENANT, "zero"), b"pw-zero"));
    assert!(after_ok);
    assert_eq!(sqlstate(&body), "53300");
}

#[test]
fn rename_and_drop_follow_the_login_map() {
    let f = Fixture::start("follow");
    f.sql(TENANT, "CREATE ROLE old_n LOGIN PASSWORD 'pw'");
    f.sql(TENANT, "ALTER ROLE old_n RENAME TO new_n");
    let (body, _, _, _) = expect_rejected(f.login(&f.ident(TENANT, "old_n"), b"pw"));
    assert_eq!(sqlstate(&body), "28P01");
    let _ready = expect_ready(f.login(&f.ident(TENANT, "new_n"), b"pw"));
    f.sql(TENANT, "DROP ROLE new_n");
    let (body, _, _, _) = expect_rejected(f.login(&f.ident(TENANT, "new_n"), b"pw"));
    assert_eq!(sqlstate(&body), "28P01");
}

#[test]
fn another_tenants_role_name_is_rejected() {
    let f = Fixture::start("tenant");
    f.sql("tenant-b", "CREATE ROLE only_b LOGIN PASSWORD 'pw-b'");
    // 自テナントの修飾子と他テナントにしか無いロール名の組み合わせ。
    let (body, _, _, _) = expect_rejected(f.login(&f.ident(TENANT, "only_b"), b"pw-b"));
    assert_eq!(sqlstate(&body), "28P01");
    // 本来のテナントの識別子では接続できる。
    let _ready = expect_ready(f.login(&f.ident("tenant-b", "only_b"), b"pw-b"));
}

#[test]
fn users_file_user_still_authenticates() {
    let f = Fixture::start("users");
    let _ready = expect_ready(f.login("ops", b"pw-ops"));
    let (body, _, _, _) = expect_rejected(f.login("ops", b"wrong"));
    assert_eq!(sqlstate(&body), "28P01");
}

#[test]
fn malformed_identifier_is_treated_like_unknown_user() {
    let f = Fixture::start("malformed");
    let (body, after_ok, elapsed, _) = expect_rejected(f.login("3:abcxyz-not-canonical:", b"x"));
    assert!(!after_ok);
    assert_eq!(sqlstate(&body), "28P01");
    assert!(elapsed >= wire_server::auth::AUTH_FAILURE_DELAY);
}

/// client-final の待機中にロールの属性が変わった場合、検証の成功後の再取得で拒否される
/// （待機前の古い値で接続を許可しない。Issue #2233）。
#[test]
fn attributes_changed_while_waiting_are_rechecked() {
    // NOLOGIN への変更は AuthenticationOk の後の 28000。
    let f = Fixture::start("recheck-nologin");
    f.sql(TENANT, "CREATE ROLE app LOGIN PASSWORD 'pw-app'");
    let ident = f.ident(TENANT, "app");
    let l = scram_login_with_hook(f.addr, &ident, b"pw-app", &|| {
        f.sql(TENANT, "ALTER ROLE app NOLOGIN");
    });
    let (body, after_ok, _, _) = expect_rejected(l);
    assert!(after_ok);
    assert_eq!(sqlstate(&body), "28000");

    // CONNECTION LIMIT 0 への変更は 53300。
    let f = Fixture::start("recheck-limit");
    f.sql(TENANT, "CREATE ROLE app LOGIN PASSWORD 'pw-app'");
    let ident = f.ident(TENANT, "app");
    let l = scram_login_with_hook(f.addr, &ident, b"pw-app", &|| {
        f.sql(TENANT, "ALTER ROLE app CONNECTION LIMIT 0");
    });
    let (body, after_ok, _, _) = expect_rejected(l);
    assert!(after_ok);
    assert_eq!(sqlstate(&body), "53300");

    // 待機中の期限切れ・削除・パスワード変更は通常の 28P01（存在を区別させない）。
    for (label, stmt) in [
        ("expired", "ALTER ROLE app VALID UNTIL '2000-01-01'"),
        ("dropped", "DROP ROLE app"),
        ("password", "ALTER ROLE app PASSWORD 'pw-new'"),
    ] {
        let f = Fixture::start(&format!("recheck-{label}"));
        f.sql(TENANT, "CREATE ROLE app LOGIN PASSWORD 'pw-app'");
        let ident = f.ident(TENANT, "app");
        let l = scram_login_with_hook(f.addr, &ident, b"pw-app", &|| f.sql(TENANT, stmt));
        let (body, after_ok, _, _) = expect_rejected(l);
        assert!(!after_ok, "{label}");
        assert_eq!(sqlstate(&body), "28P01", "{label}");
    }
}

/// 確立済みの接続で簡易クエリを 1 本流し、ReadyForQuery までに ErrorResponse があればその SQLSTATE を返す。
fn simple_query(stream: &mut TcpStream, sql: &str) -> Option<String> {
    let mut body = sql.as_bytes().to_vec();
    body.push(0);
    let mut msg = vec![b'Q'];
    msg.extend_from_slice(&((4 + body.len()) as i32).to_be_bytes());
    msg.extend_from_slice(&body);
    stream.write_all(&msg).expect("query");
    let mut code = None;
    loop {
        let (ty, body) = read_message(stream);
        match ty {
            b'E' => code = Some(sqlstate(&body)),
            b'Z' => return code,
            _ => {}
        }
    }
}

// ---- Issue #1971: users ファイルのユーザーとカタログのロールの対応づけ（SQL-39・WIRE-2） ----

#[test]
fn users_file_user_without_any_role_stays_unbound() {
    let f = Fixture::start("ufr-unbound");
    let mut s = expect_ready(f.login("ops", b"pw-ops"));
    // 束縛されていないセッションは SET ROLE を許可しない（従来どおり）。
    assert_eq!(
        simple_query(&mut s, "SET ROLE ops").as_deref(),
        Some("42501")
    );
}

#[test]
fn users_file_user_binds_to_same_name_role() {
    let f = Fixture::start("ufr-bound");
    f.sql(TENANT, "CREATE ROLE ops LOGIN");
    let mut s = expect_ready(f.login("ops", b"pw-ops"));
    assert_eq!(simple_query(&mut s, "SET ROLE ops"), None);
}

#[test]
fn users_file_user_without_same_name_role_is_rejected_like_wrong_password() {
    let f = Fixture::start("ufr-missing");
    f.sql(TENANT, "CREATE ROLE someone_else LOGIN");
    let (body, after_ok, elapsed, _) = expect_rejected(f.login("ops", b"pw-ops"));
    assert!(!after_ok);
    assert_eq!(sqlstate(&body), "28P01");
    assert!(elapsed >= wire_server::auth::AUTH_FAILURE_DELAY);
    let (wrong, _, _, _) = expect_rejected(f.login("ops", b"wrong"));
    assert_eq!(body, wrong, "must be byte-identical to a wrong password");
}

#[test]
fn users_file_user_with_nologin_or_expired_role() {
    let f = Fixture::start("ufr-nologin");
    f.sql(TENANT, "CREATE ROLE ops NOLOGIN");
    let (body, after_ok, _, _) = expect_rejected(f.login("ops", b"pw-ops"));
    assert!(after_ok);
    assert_eq!(sqlstate(&body), "28000");
    let (body, after_ok, _, _) = expect_rejected(f.login("ops", b"wrong"));
    assert!(!after_ok);
    assert_eq!(sqlstate(&body), "28P01");

    let f = Fixture::start("ufr-expired");
    f.sql(
        TENANT,
        "CREATE ROLE ops LOGIN VALID UNTIL '2000-01-01 00:00:00+00'",
    );
    let (body, after_ok, elapsed, _) = expect_rejected(f.login("ops", b"pw-ops"));
    assert!(!after_ok);
    assert_eq!(sqlstate(&body), "28P01");
    assert!(elapsed >= wire_server::auth::AUTH_FAILURE_DELAY);
}

#[test]
fn users_file_user_does_not_match_another_tenants_role() {
    let f = Fixture::start("ufr-tenant");
    f.sql("tenant-b", "CREATE ROLE ops LOGIN");
    // 自テナントにロールが無ければ従来どおり（他テナントのロールは見えない）。
    let mut s = expect_ready(f.login("ops", b"pw-ops"));
    assert_eq!(
        simple_query(&mut s, "SET ROLE ops").as_deref(),
        Some("42501")
    );
    // 自テナントに別のロールができると、同名ロールが無い ops は拒否される。
    f.sql(TENANT, "CREATE ROLE other LOGIN");
    let (body, _, _, _) = expect_rejected(f.login("ops", b"pw-ops"));
    assert_eq!(sqlstate(&body), "28P01");
}

#[test]
fn users_file_user_connection_limit_follows_the_role() {
    let f = Fixture::start("ufr-limit");
    f.sql(TENANT, "CREATE ROLE ops LOGIN CONNECTION LIMIT 1");
    let _first = expect_ready(f.login("ops", b"pw-ops"));
    let (body, after_ok, _, _) = expect_rejected(f.login("ops", b"pw-ops"));
    assert!(after_ok);
    assert_eq!(sqlstate(&body), "53300");
}

#[test]
fn ddl_allowed_users_never_widens_beyond_the_catalog() {
    // 指定ありでも同名ロールが無ければ拒否（DDL 権限も付かない）。
    let f = Fixture::start_with("ufr-ddl-missing", &["ops"]);
    f.sql(TENANT, "CREATE ROLE other LOGIN");
    let (body, _, _, _) = expect_rejected(f.login("ops", b"pw-ops"));
    assert_eq!(sqlstate(&body), "28P01");

    // 指定ありかつ同名 LOGIN ロール: 従来の写像を維持して DDL が通る。
    let f = Fixture::start_with("ufr-ddl-listed", &["ops"]);
    f.sql(TENANT, "CREATE ROLE ops LOGIN");
    let mut s = expect_ready(f.login("ops", b"pw-ops"));
    assert_eq!(simple_query(&mut s, "CREATE TABLE ufr_t (id BIGINT)"), None);

    // 指定なしかつ同名 LOGIN ロール: 束縛しても DDL 権限は広がらない。
    let f = Fixture::start("ufr-ddl-unlisted");
    f.sql(TENANT, "CREATE ROLE ops LOGIN");
    let mut s = expect_ready(f.login("ops", b"pw-ops"));
    assert_eq!(
        simple_query(&mut s, "CREATE TABLE ufr_t (id BIGINT)").as_deref(),
        Some("42501")
    );
}
