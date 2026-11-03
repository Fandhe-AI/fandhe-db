//! users ファイルの初期ブートストラップ専用化（SQL-39 (3)(5)・WIRE-2・HTTP-4、Issue #2238）の結合テスト。
//!
//! 固定する契約: 初回の起動でファイルのユーザーが接続できる。2 回目以降の起動ではファイルの資格情報を
//! 読まず、カタログだけで照合する（`ALTER ROLE ... PASSWORD` の結果が再起動後も効く）。削除したロールは
//! ファイルに行が残っていても再作成されない。ファイルの追加・書き換え・`--ddl-allowed-users` の後からの
//! 追加では権限が広がらない。`--ddl-allowed-users` に無い初期ロールでも既存表を読み書きできる。
//! 不明なユーザー・ロールが無いユーザー・誤パスワードは応答バイトまで同一で `AUTH_FAILURE_DELAY` 以上。
//!
//! 起動はプロセス内で `bootstrap_users_file`＋`UserStore` の組み直しとして再現する（同じ DB・同じ
//! カタログを引き継ぐ。`main.rs` が同じ関数を同じ順序で呼ぶ）。時間を計測するため `tests/` 直下に単独で置く。

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
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
            "wire-ufb-{label}-{}-{seq}-{}",
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

fn phc(password: &[u8]) -> String {
    argon2id::encode_phc(password, b"0123456789abcdef", &argon2id::RECOMMENDED_PARAMS).expect("phc")
}

/// 1 回の「起動」: 与えたファイル内容と `--ddl-allowed-users` で `UserStore` を作り、ブートストラップを
/// 実行してから接続を受け付ける（`main.rs` と同じ順序）。
struct Boot {
    addr: SocketAddr,
    warnings: Vec<String>,
}

struct Fixture {
    dir: TempDir,
    core: Arc<EngineCore>,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let core = Arc::new(EngineCore::open(dir.0.join("db.redb")).expect("open engine"));
        Self { dir, core }
    }

    fn boot(&self, rows: &[(&str, &[u8])], ddl: &[&str]) -> Boot {
        let users = self.dir.0.join("users.txt");
        let text: String = rows
            .iter()
            .map(|(name, pw)| format!("{name}:{TENANT}:{}\n", phc(pw)))
            .collect();
        std::fs::write(&users, text).expect("write users");
        let mut store = UserStore::load_from_file(&users).expect("users");
        if !ddl.is_empty() {
            let names: Vec<String> = ddl.iter().map(|s| s.to_string()).collect();
            store = store.with_ddl_allowed_users(&names).expect("ddl users");
        }
        let store = Arc::new(store);
        let summary = wire_server::auth::bootstrap::bootstrap_users_file(&store, &self.core)
            .expect("bootstrap");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (s2, c2) = (Arc::clone(&store), Arc::clone(&self.core));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (s3, c3) = (Arc::clone(&s2), Arc::clone(&c2));
                std::thread::spawn(move || {
                    let _ = wire_server::handshake::handle_connection_with_engine(stream, &s3, &c3);
                });
            }
        });
        Boot {
            addr,
            warnings: summary.warnings,
        }
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

fn login(addr: SocketAddr, user: &str, password: &[u8]) -> Login {
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

/// 単純問い合わせを実行し、エラーがあればその SQLSTATE を返す。
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

fn ready(l: Login) -> TcpStream {
    match l {
        Login::Ready(s) => s,
        Login::Rejected { body, .. } => panic!("login must succeed: {}", sqlstate(&body)),
    }
}

fn rejected(l: Login) -> (Vec<u8>, bool, Duration) {
    match l {
        Login::Rejected {
            body,
            after_ok,
            elapsed,
        } => (body, after_ok, elapsed),
        Login::Ready(_) => panic!("login must be rejected"),
    }
}

/// 認証失敗（`28P01`・認証 OK の前・固定遅延以上）の応答本文を返す。
fn auth_failure_body(l: Login) -> Vec<u8> {
    let (body, after_ok, elapsed) = rejected(l);
    assert!(!after_ok);
    assert_eq!(sqlstate(&body), "28P01");
    assert!(elapsed >= wire_server::auth::AUTH_FAILURE_DELAY);
    body
}

#[test]
fn first_start_logs_in_and_later_starts_use_the_catalog_only() {
    let f = Fixture::new("catalog");
    let rows: [(&str, &[u8]); 2] = [("ops", b"pw-ops"), ("dev", b"pw-dev")];
    let b1 = f.boot(&rows, &["ops"]);
    assert!(b1.warnings.is_empty());
    let mut s = ready(login(b1.addr, "ops", b"pw-ops"));
    // 初期ロールは CREATEROLE を持つ。パスワードの変更はカタログへ書かれる。
    assert_eq!(
        simple_query(&mut s, "ALTER ROLE ops PASSWORD 'pw-new'"),
        None
    );
    drop(s);

    // 2 回目の起動（同じファイル）: ファイルのパスワードは通らず、カタログの新しいパスワードが通る。
    let b2 = f.boot(&rows, &["ops"]);
    auth_failure_body(login(b2.addr, "ops", b"pw-ops"));
    drop(ready(login(b2.addr, "ops", b"pw-new")));
    // 変更していないロールは、取り込み済みの資格情報でそのまま通る。
    drop(ready(login(b2.addr, "dev", b"pw-dev")));
    // 食い違いは起動ログ用の警告として検出される（行番号だけを含み、テナント・資格情報を含まない）。
    assert!(b2.warnings.iter().any(|w| w.contains("line 1")));
    assert!(b2
        .warnings
        .iter()
        .all(|w| !w.contains(TENANT) && !w.contains("argon2")));
}

#[test]
fn dropped_role_is_not_recreated_from_the_file() {
    let f = Fixture::new("dropped");
    let rows: [(&str, &[u8]); 2] = [("ops", b"pw-ops"), ("dev", b"pw-dev")];
    let b1 = f.boot(&rows, &["ops"]);
    let mut s = ready(login(b1.addr, "ops", b"pw-ops"));
    assert_eq!(simple_query(&mut s, "DROP ROLE dev"), None);
    drop(s);

    let b2 = f.boot(&rows, &["ops"]);
    auth_failure_body(login(b2.addr, "dev", b"pw-dev"));
    assert!(b2.warnings.iter().any(|w| w.contains("line 2")));
    // 再作成されていない（ロール一覧に居ない）ので、ops から見ても SET ROLE できない。
    let mut s = ready(login(b2.addr, "ops", b"pw-ops"));
    assert_eq!(
        simple_query(&mut s, "SET ROLE dev").as_deref(),
        Some("42704")
    );
}

#[test]
fn file_edits_and_later_ddl_listing_do_not_widen_anything() {
    let f = Fixture::new("widen");
    let rows: [(&str, &[u8]); 2] = [("ops", b"pw-ops"), ("dev", b"pw-dev")];
    f.boot(&rows, &["ops"]);

    // ファイルに行を足し、ops のパスワードを書き換え、`--ddl-allowed-users` へ dev を足して再起動する。
    let edited: [(&str, &[u8]); 3] = [
        ("ops", b"pw-file-new"),
        ("dev", b"pw-dev"),
        ("eve", b"pw-eve"),
    ];
    let b2 = f.boot(&edited, &["ops", "dev"]);
    // 追加した行はロールを作らず、接続できない。
    auth_failure_body(login(b2.addr, "eve", b"pw-eve"));
    // ファイル上で書き換えたパスワードは効かず、カタログの旧パスワードだけが通る。
    auth_failure_body(login(b2.addr, "ops", b"pw-file-new"));
    let mut ops = ready(login(b2.addr, "ops", b"pw-ops"));
    // ops は列挙かつ DDL フラグありなので DDL できる。
    assert_eq!(simple_query(&mut ops, "CREATE TABLE t (id BIGINT)"), None);
    // dev は列挙を後から足しても DDL は広がらない（永続の DDL フラグが無い）。
    let mut dev = ready(login(b2.addr, "dev", b"pw-dev"));
    assert_eq!(
        simple_query(&mut dev, "CREATE TABLE u (id BIGINT)").as_deref(),
        Some("42501")
    );
    // 列挙に無い初期ロールでも、既存表は全権限で読み書きできる（回帰しない）。
    assert_eq!(simple_query(&mut dev, "INSERT INTO t VALUES (1)"), None);
    assert_eq!(simple_query(&mut dev, "SELECT * FROM t"), None);
    drop((ops, dev));

    // 列挙から外すと DDL は狭まる。
    let b3 = f.boot(&rows, &["dev"]);
    let mut ops = ready(login(b3.addr, "ops", b"pw-ops"));
    assert_eq!(
        simple_query(&mut ops, "CREATE TABLE v (id BIGINT)").as_deref(),
        Some("42501")
    );
    // DDL が無くても全権限なので読み取りは通る。
    assert_eq!(simple_query(&mut ops, "SELECT * FROM t"), None);
}

#[test]
fn unknown_missing_and_wrong_password_look_the_same() {
    let f = Fixture::new("oracle");
    let rows: [(&str, &[u8]); 2] = [("ops", b"pw-ops"), ("dev", b"pw-dev")];
    let b1 = f.boot(&rows, &["ops"]);
    let mut s = ready(login(b1.addr, "ops", b"pw-ops"));
    assert_eq!(simple_query(&mut s, "DROP ROLE dev"), None);
    drop(s);
    let b2 = f.boot(&rows, &["ops"]);
    let unknown = auth_failure_body(login(b2.addr, "nobody", b"pw-ops"));
    let missing_role = auth_failure_body(login(b2.addr, "dev", b"pw-dev"));
    let wrong = auth_failure_body(login(b2.addr, "ops", b"wrong"));
    assert_eq!(unknown, missing_role);
    assert_eq!(unknown, wrong);
}
/// HTTP の `POST /v1/session`（HTTP-4）も、ブートストラップ済みテナントではカタログで照合し、
/// 同名ロールへセッションを束縛する。ファイルのパスワードは `ALTER ROLE ... PASSWORD` の後は通らない。
#[test]
fn http_session_issue_authenticates_against_the_catalog() {
    use wire_server::http::session::issue::handle_with_engine;
    use wire_server::http::session::store::SessionStore;

    let f = Fixture::new("http");
    let rows: [(&str, &[u8]); 1] = [("ops", b"pw-ops")];
    f.boot(&rows, &["ops"]);
    let users_path = f.dir.0.join("users.txt");
    let users = UserStore::load_from_file(&users_path).expect("users");
    let sessions = SessionStore::new();
    let post = |pw: &str| {
        let body = format!(r#"{{"user":"ops","password":"{pw}"}}"#);
        let resp = handle_with_engine(
            &users,
            Some(&f.core),
            &sessions,
            body.as_bytes(),
            Instant::now,
            std::time::SystemTime::now(),
        );
        String::from_utf8(resp).expect("utf8 response")
    };
    assert!(post("pw-ops").starts_with("HTTP/1.1 200"));

    let ctx = engine::policy::PolicyContext::with_visibilities(
        TENANT,
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("ctx");
    let mut admin = engine::sql::mode::SessionState::default();
    admin.allow_ddl();
    f.core
        .execute_sql_in_session(&ctx, &mut admin, "ALTER ROLE ops PASSWORD 'pw-changed'")
        .expect("alter role");

    let rejected = post("pw-ops");
    assert!(rejected.starts_with("HTTP/1.1 401"), "{rejected}");
    assert!(!rejected.contains(TENANT));
    assert!(post("pw-changed").starts_with("HTTP/1.1 200"));
}
