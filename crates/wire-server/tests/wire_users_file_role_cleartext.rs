//! cleartext 認証での users ファイルのユーザーとカタログのロールの対応づけの結合テスト
//! （SQL-39・WIRE-2、Issue #1971）。SCRAM 経路は `wire_scram_catalog_role.rs` が担う。
//!
//! 固定する契約: ロール 0 件のテナントは従来どおり未束縛、同名の LOGIN ロールがあれば束縛される、
//! 同名ロールが無い・期限切れは誤パスワードと応答バイトまで同一の `28P01`（固定遅延以上）、
//! `NOLOGIN` は検証の後にだけ `28000`、`CONNECTION LIMIT` 超過は `53300`。
//!
//! 時間を計測するため `tests/` 直下に単独で置く（束ねない）。サーバーはポート 0・一意な一時ディレクトリで
//! 独立させる。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use engine::core::EngineCore;
use wire_server::auth::{argon2id, UserStore};

const TENANT: &str = "tenant-a";
static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir(std::path::PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wire-ufr-cleartext-{label}-{}-{seq}-{}",
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

struct Fixture {
    _dir: TempDir,
    core: Arc<EngineCore>,
    addr: std::net::SocketAddr,
}

impl Fixture {
    /// users ファイルには `ops`（tenant-a・パスワード `pw-ops`）を 1 件置く。
    fn start(label: &str) -> Self {
        let dir = TempDir::new(label);
        let users = dir.0.join("users.txt");
        let phc = argon2id::encode_phc(
            b"pw-ops",
            b"0123456789abcdef",
            &argon2id::RECOMMENDED_PARAMS,
        )
        .expect("phc");
        std::fs::write(&users, format!("ops:{TENANT}:{phc}\n")).expect("write users");
        let store = Arc::new(UserStore::load_from_file(&users).expect("users"));
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

    fn login(&self, password: &[u8]) -> Login {
        login(self.addr, "ops", password)
    }
}

enum Login {
    Ready(TcpStream),
    Rejected {
        body: Vec<u8>,
        after_ok: bool,
        elapsed: Duration,
    },
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

fn sqlstate(body: &[u8]) -> String {
    let mut pos = 0usize;
    while pos < body.len() && body[pos] != 0 {
        let code = body[pos];
        pos += 1;
        let start = pos;
        while pos < body.len() && body[pos] != 0 {
            pos += 1;
        }
        if code == b'C' {
            return String::from_utf8_lossy(&body[start..pos]).into_owned();
        }
        pos += 1;
    }
    panic!("no SQLSTATE in {body:?}");
}

fn login(addr: std::net::SocketAddr, user: &str, password: &[u8]) -> Login {
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
    let (ty, body) = read_message(&mut stream);
    assert_eq!((ty, &body[..4]), (b'R', &3i32.to_be_bytes()[..]));

    let mut pw = password.to_vec();
    pw.push(0);
    let mut msg = vec![b'p'];
    msg.extend_from_slice(&((4 + pw.len()) as i32).to_be_bytes());
    msg.extend_from_slice(&pw);
    let start = Instant::now();
    stream.write_all(&msg).expect("password");

    let (ty, body) = read_message(&mut stream);
    if ty == b'E' {
        return Login::Rejected {
            body,
            after_ok: false,
            elapsed: start.elapsed(),
        };
    }
    assert_eq!((ty, &body[..4]), (b'R', &0i32.to_be_bytes()[..]));
    for _ in 0..20 {
        let (ty, body) = read_message(&mut stream);
        match ty {
            b'Z' => return Login::Ready(stream),
            b'E' => {
                return Login::Rejected {
                    body,
                    after_ok: true,
                    elapsed: start.elapsed(),
                }
            }
            _ => {}
        }
    }
    panic!("no ReadyForQuery");
}

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

fn expect_ready(l: Login) -> TcpStream {
    match l {
        Login::Ready(s) => s,
        Login::Rejected { body, .. } => panic!("login must succeed: {}", sqlstate(&body)),
    }
}

fn expect_rejected(l: Login) -> (Vec<u8>, bool, Duration) {
    match l {
        Login::Rejected {
            body,
            after_ok,
            elapsed,
        } => (body, after_ok, elapsed),
        Login::Ready(_) => panic!("login must be rejected"),
    }
}

#[test]
fn no_roles_stays_unbound_and_same_name_role_binds() {
    let f = Fixture::start("bind");
    let mut s = expect_ready(f.login(b"pw-ops"));
    assert_eq!(
        simple_query(&mut s, "SET ROLE ops").as_deref(),
        Some("42501")
    );
    drop(s);
    f.sql(TENANT, "CREATE ROLE ops LOGIN");
    let mut s = expect_ready(f.login(b"pw-ops"));
    assert_eq!(simple_query(&mut s, "SET ROLE ops"), None);
}

#[test]
fn missing_same_name_role_is_indistinguishable_from_wrong_password() {
    let f = Fixture::start("missing");
    f.sql(TENANT, "CREATE ROLE someone_else LOGIN");
    let (body, after_ok, elapsed) = expect_rejected(f.login(b"pw-ops"));
    assert!(!after_ok);
    assert_eq!(sqlstate(&body), "28P01");
    assert!(elapsed >= wire_server::auth::AUTH_FAILURE_DELAY);
    let (wrong, _, _) = expect_rejected(f.login(b"wrong"));
    assert_eq!(body, wrong);

    // 期限切れも同じ。
    let f = Fixture::start("expired");
    f.sql(
        TENANT,
        "CREATE ROLE ops LOGIN VALID UNTIL '2000-01-01 00:00:00+00'",
    );
    let (body2, after_ok, elapsed) = expect_rejected(f.login(b"pw-ops"));
    assert!(!after_ok);
    assert!(elapsed >= wire_server::auth::AUTH_FAILURE_DELAY);
    assert_eq!(body, body2);
}

#[test]
fn nologin_and_connection_limit_are_judged_after_verification() {
    let f = Fixture::start("nologin");
    f.sql(TENANT, "CREATE ROLE ops NOLOGIN");
    let (body, after_ok, _) = expect_rejected(f.login(b"pw-ops"));
    assert!(after_ok);
    assert_eq!(sqlstate(&body), "28000");
    let (body, after_ok, _) = expect_rejected(f.login(b"wrong"));
    assert!(!after_ok);
    assert_eq!(sqlstate(&body), "28P01");

    let f = Fixture::start("limit");
    f.sql(TENANT, "CREATE ROLE ops LOGIN CONNECTION LIMIT 1");
    let _first = expect_ready(f.login(b"pw-ops"));
    let (body, after_ok, _) = expect_rejected(f.login(b"pw-ops"));
    assert!(after_ok);
    assert_eq!(sqlstate(&body), "53300");
}
