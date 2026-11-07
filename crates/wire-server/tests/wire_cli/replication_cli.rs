//! ストリーミングレプリケーションの送信側 endpoint（Issue #2173・親 #2171・RECOVER-15 ポインタ。
//! 方式の記録は `docs/design/replication-streaming.md`）をバイナリ子プロセスで外形検証する。
//!
//! 受信側の適用ループは #2174 の担当で、ここでは最小のテスト用クライアント（フレームを手で組み立てて
//! 読み書きするだけ）で送信側の振る舞いを確かめる: 未認証・許可外・テナントのロールの拒否、開始位置からの
//! 連続した送信、ack による送信位置と刈り込みの保護、切断・停止したレプリカがプライマリの書き込みを止めない
//! こと、バックプレッシャの切断、接続数の上限、刈り込み済み・不一致の開始位置の拒否、不正なフラグの拒否。
//! サーバーはポート 0 と一意な一時ディレクトリで起動する。

use crate::common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_wire-server");
const ROLE: &str = "replica-1";
const ROLE_PW: &str = "pw-replica";

const E_BAD_REQUEST: u8 = 1;
const E_AUTH_FAILED: u8 = 2;
const E_BUSY: u8 = 3;
const E_FOREIGN: u8 = 4;
const E_RESYNC: u8 = 5;
const E_AHEAD: u8 = 6;
const E_BACKPRESSURE: u8 = 8;

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `hash-password` サブコマンドで PHC を作る。
fn phc_for(password: &str) -> String {
    let mut child = Command::new(BIN)
        .arg("hash-password")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn hash-password");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(format!("{password}\n").as_bytes())
        .expect("write password");
    let output = child.wait_with_output().expect("wait hash-password");
    assert!(output.status.success());
    String::from_utf8(output.stdout)
        .expect("utf-8 phc")
        .trim()
        .to_string()
}

struct Primary {
    _fx: common::TempFixtureDir,
    _child: ChildGuard,
    tenant: SocketAddr,
    repl: SocketAddr,
    archive: Option<PathBuf>,
}

impl Primary {
    /// レプリケーション付きで起動する。`extra` は追加フラグ。`archive` が真ならアーカイブ（1 秒周期・
    /// 保持 0）を有効にする。
    fn start(label: &str, extra: &[&str], archive: bool) -> Self {
        let fx = common::TempFixtureDir::new(label);
        let users = fx.users_path_str();
        let db = fx.db_path_str();
        let dir = Path::new(&db).parent().expect("parent").to_path_buf();
        std::fs::write(&users, format!("alice:tenant-a:{}\n", phc_for("pw-alice"))).expect("users");
        let roles = dir.join("replication-roles.txt");
        std::fs::write(&roles, format!("{ROLE}:{}\n", phc_for(ROLE_PW))).expect("roles");
        let mut args: Vec<String> = [
            "--users",
            &users,
            "--db",
            &db,
            "--bind",
            "127.0.0.1:0",
            "--ddl-allowed-users",
            "alice",
            "--change-log",
            "--replication-bind",
            "127.0.0.1:0",
            "--replication-roles",
            roles.to_str().expect("utf-8"),
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut archive_dir = None;
        if archive {
            use std::os::unix::fs::PermissionsExt;
            let a = dir.join("archive");
            std::fs::create_dir(&a).expect("archive dir");
            std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            for s in [
                "--change-log-archive-dir",
                a.to_str().expect("utf-8"),
                "--change-log-archive-interval-secs",
                "1",
                "--change-log-retain-records",
                "0",
            ] {
                args.push(s.to_string());
            }
            archive_dir = Some(a);
        }
        if !extra.contains(&"--replication-allow") {
            args.extend(["--replication-allow".into(), "127.0.0.1".into()]);
        }
        args.extend(extra.iter().map(|s| s.to_string()));
        let mut child = Command::new(BIN)
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server");
        let (tenant, lines) =
            common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(15));
        let repl = lines
            .iter()
            .find_map(|l| l.split("replication endpoint bound to ").nth(1))
            .and_then(|a| a.trim().parse::<SocketAddr>().ok())
            .unwrap_or_else(|| panic!("no replication address: {lines:?}"));
        Self {
            _fx: fx,
            _child: ChildGuard(child),
            tenant,
            repl,
            archive: archive_dir,
        }
    }

    fn alice(&self) -> TenantSession {
        let stream = common::authenticate_to_ready_for_query(self.tenant, "alice", "pw-alice");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("timeout");
        TenantSession(stream)
    }

    fn setup_table(&self) -> TenantSession {
        let mut s = self.alice();
        s.exec("CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))");
        s
    }
}

struct TenantSession(TcpStream);

impl TenantSession {
    fn exec(&mut self, sql: &str) {
        common::send_simple_query(&mut self.0, sql);
        let _ = common::read_command_complete(&mut self.0);
        common::read_ready_for_query(&mut self.0);
    }

    fn insert(&mut self, id: u32) {
        self.exec(&format!(
            "INSERT INTO docs (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]')"
        ));
    }
}

/// テスト用の最小のレプリカ側クライアント。
struct Replica(TcpStream);

fn hello_body(role: &str, password: &str) -> Vec<u8> {
    let mut b = 1u16.to_be_bytes().to_vec();
    b.push(u8::try_from(role.len()).expect("role len"));
    b.extend_from_slice(role.as_bytes());
    b.extend_from_slice(password.as_bytes());
    b
}

fn start_body(db_id: &[u8], timeline: u64, applied: u64) -> Vec<u8> {
    let mut b = db_id.to_vec();
    b.extend_from_slice(&timeline.to_be_bytes());
    b.extend_from_slice(&applied.to_be_bytes());
    b
}

impl Replica {
    fn connect(addr: SocketAddr) -> Self {
        let s = TcpStream::connect(addr).expect("connect");
        s.set_read_timeout(Some(Duration::from_secs(20)))
            .expect("timeout");
        Self(s)
    }

    fn send(&mut self, ty: u8, body: &[u8]) {
        let mut f = vec![ty];
        f.extend_from_slice(&u32::try_from(body.len()).expect("len").to_be_bytes());
        f.extend_from_slice(body);
        self.0.write_all(&f).expect("send frame");
    }

    /// 1 フレームを読む。切断・タイムアウトは `None`。
    fn recv(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut h = [0u8; 5];
        self.0.read_exact(&mut h).ok()?;
        let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
        assert!(len <= 300 * 1024 * 1024, "unreasonable frame length");
        let mut body = vec![0u8; len];
        self.0.read_exact(&mut body).ok()?;
        Some((h[0], body))
    }

    /// 認証して `I`（識別子）まで進める。(db_id, timeline, current)。
    fn login(&mut self, role: &str, password: &str) -> ([u8; 16], u64, u64) {
        self.send(b'H', &hello_body(role, password));
        let (ty, body) = self.recv().expect("identity frame");
        assert_eq!(ty, b'I', "expected identity, got {ty} {body:?}");
        let db_id: [u8; 16] = body[..16].try_into().expect("db id");
        let timeline = u64::from_be_bytes(body[16..24].try_into().expect("timeline"));
        let current = u64::from_be_bytes(body[24..32].try_into().expect("current"));
        (db_id, timeline, current)
    }

    /// 認証 → Start → `O` まで。
    fn begin(&mut self, applied: u64) -> ([u8; 16], u64) {
        let (db_id, timeline, _) = self.login(ROLE, ROLE_PW);
        self.send(b'S', &start_body(&db_id, timeline, applied));
        let (ty, body) = self.recv().expect("ok frame");
        assert_eq!(ty, b'O', "expected ok, got {ty} {body:?}");
        (db_id, timeline)
    }

    fn ack(&mut self, lsn: u64) {
        self.send(b'K', &lsn.to_be_bytes());
    }

    /// 次の `W` を読む（`P` は読み飛ばす）。`(lsn, レコード)`。
    fn next_record(&mut self) -> Option<(u64, Vec<u8>)> {
        loop {
            let (ty, body) = self.recv()?;
            match ty {
                b'W' => {
                    let lsn = u64::from_be_bytes(body[..8].try_into().expect("lsn"));
                    return Some((lsn, body[8..].to_vec()));
                }
                b'P' => continue,
                other => panic!("unexpected frame {other}: {body:?}"),
            }
        }
    }

    /// 次のフレームが `E` でコードが `expect` であること。
    fn expect_error(&mut self, expect: u8) {
        match self.recv() {
            Some((b'E', body)) => assert_eq!(body, vec![expect]),
            other => panic!("expected error {expect}, got {other:?}"),
        }
    }

    /// 切断（EOF）されるまで読み捨てる。`E` を見たらそのコードを返す。
    fn drain_until_closed(&mut self) -> Option<u8> {
        let mut code = None;
        while let Some((ty, body)) = self.recv() {
            if ty == b'E' {
                code = body.first().copied();
            }
        }
        code
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("run")
}

fn wait_until(what: &str, timeout: Duration, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    panic!("timed out waiting for {what}");
}

/// 不正なフラグ・組み合わせは、DB を作る前に固定文言で拒否される。
#[test]
fn recover15_replication_flags_are_validated_before_the_database_is_opened() {
    let fx = common::TempFixtureDir::new("repl-flags");
    common::write_empty_user_store(&fx.users_path_str());
    let dir = Path::new(&fx.db_path_str())
        .parent()
        .expect("p")
        .to_path_buf();
    let roles = dir.join("roles.txt");
    std::fs::write(&roles, format!("{ROLE}:{}\n", phc_for(ROLE_PW))).expect("roles");
    let roles_s = roles.to_str().expect("utf-8").to_string();
    let bad_roles = dir.join("bad-roles.txt");
    std::fs::write(&bad_roles, "no-colon-line\n").expect("bad roles");
    let bad_roles_s = bad_roles.to_str().expect("utf-8").to_string();
    // `--users` と同じ形式のファイルをロールの資格情報に指定しても通用しない（行の形式が違う）。
    let users_like = dir.join("users-like.txt");
    std::fs::write(
        &users_like,
        format!("alice:tenant-a:{}\n", phc_for("pw-alice")),
    )
    .expect("users-like");
    let users_like_s = users_like.to_str().expect("utf-8").to_string();
    let base = |extra: &[&str]| -> Output {
        let users = fx.users_path_str();
        let db = fx.db_path_str();
        let mut args = vec!["--users", &users, "--db", &db, "--bind", "127.0.0.1:0"];
        args.extend_from_slice(extra);
        run(&args)
    };
    let cases: Vec<(Vec<&str>, &str)> = vec![
        // --change-log が前提。
        (
            vec![
                "--replication-bind",
                "127.0.0.1:0",
                "--replication-roles",
                &roles_s,
                "--replication-allow",
                "127.0.0.1",
            ],
            "require --change-log",
        ),
        // 従属フラグは bind が前提。
        (
            vec!["--change-log", "--replication-allow", "127.0.0.1"],
            "require --replication-bind",
        ),
        // ロール・許可リストは必須。
        (
            vec![
                "--change-log",
                "--replication-bind",
                "127.0.0.1:0",
                "--replication-allow",
                "127.0.0.1",
            ],
            "requires --replication-roles",
        ),
        (
            vec![
                "--change-log",
                "--replication-bind",
                "127.0.0.1:0",
                "--replication-roles",
                &roles_s,
            ],
            "requires at least one --replication-allow",
        ),
        // 非ループバックの平文は拒否する。
        (
            vec![
                "--change-log",
                "--replication-bind",
                "0.0.0.0:0",
                "--replication-roles",
                &roles_s,
                "--replication-allow",
                "127.0.0.1",
            ],
            "must be a loopback address",
        ),
        // 許可リストの不正値（全許可を含む）。
        (
            vec![
                "--change-log",
                "--replication-bind",
                "127.0.0.1:0",
                "--replication-roles",
                &roles_s,
                "--replication-allow",
                "0.0.0.0/0",
            ],
            "must be an IP address or CIDR",
        ),
        // 重複指定。
        (
            vec![
                "--change-log",
                "--replication-bind",
                "127.0.0.1:0",
                "--replication-bind",
                "127.0.0.1:0",
            ],
            "specified more than once",
        ),
        // 不正値・範囲外。
        (
            vec![
                "--change-log",
                "--replication-bind",
                "127.0.0.1:0",
                "--replication-roles",
                &roles_s,
                "--replication-allow",
                "127.0.0.1",
                "--replication-max-connections",
                "0",
            ],
            "is out of range",
        ),
        (
            vec![
                "--change-log",
                "--replication-bind",
                "127.0.0.1:0",
                "--replication-roles",
                &roles_s,
                "--replication-allow",
                "127.0.0.1",
                "--replication-max-unacked-bytes",
                "10",
            ],
            "is out of range",
        ),
        (
            vec![
                "--change-log",
                "--replication-bind",
                "127.0.0.1:0",
                "--replication-roles",
                &roles_s,
                "--replication-allow",
                "127.0.0.1",
                "--replication-ack-timeout-secs",
                "-1",
            ],
            "must be a non-negative integer",
        ),
        (
            vec![
                "--change-log",
                "--replication-bind",
                "127.0.0.1:0",
                "--replication-roles",
                &bad_roles_s,
                "--replication-allow",
                "127.0.0.1",
            ],
            "line 1 is malformed",
        ),
        (
            vec![
                "--change-log",
                "--replication-bind",
                "127.0.0.1:0",
                "--replication-roles",
                &users_like_s,
                "--replication-allow",
                "127.0.0.1",
            ],
            "line 1 is malformed",
        ),
        (
            vec![
                "--change-log",
                "--replication-bind",
                "127.0.0.1:0",
                "--replication-roles",
                &roles_s,
                "--replication-allow",
                "127.0.0.1",
                "--replication-max-lag-records",
                "0",
            ],
            "is out of range",
        ),
        (
            vec!["--change-log", "--replication-bind"],
            "requires a value",
        ),
    ];
    for (extra, expect) in cases {
        let out = base(&extra);
        assert_ne!(out.status.code(), Some(0), "{extra:?}");
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(err.contains(expect), "{extra:?}: {err}");
        assert!(!err.contains("listening on"), "{err}");
        assert!(
            !err.contains("no-colon-line"),
            "file content must not be echoed: {err}"
        );
    }
    assert!(
        !Path::new(&fx.db_path_str()).exists(),
        "every refusal happens before the DB file is created"
    );
    // 逆向き: ロールの資格情報ファイルを `--users` に指定しても起動しない。
    let out = run(&[
        "--users",
        &roles_s,
        "--db",
        &fx.db_path_str(),
        "--bind",
        "127.0.0.1:0",
    ]);
    assert_ne!(out.status.code(), Some(0));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("listening on"));
}

/// 未認証・誤パスワード・未知のロール・テナントのロール・不正なフレームは拒否される。許可リスト外の送信元には
/// 何も返さず閉じる。
#[test]
fn recover15_unauthenticated_and_tenant_role_connections_are_refused() {
    let p = Primary::start("repl-auth", &[], false);

    // 認証前に Start を送る。
    let mut r = Replica::connect(p.repl);
    r.send(b'S', &start_body(&[0; 16], 1, 0));
    r.expect_error(E_BAD_REQUEST);
    assert_eq!(r.drain_until_closed(), None);

    // 誤パスワード・未知のロール・テナントのロール（alice の正しいパスワード）。
    for (role, pw) in [
        (ROLE, "wrong"),
        ("nobody", ROLE_PW),
        ("alice", "pw-alice"),
        ("tenant-a", "pw-alice"),
    ] {
        let mut r = Replica::connect(p.repl);
        let started = Instant::now();
        r.send(b'H', &hello_body(role, pw));
        r.expect_error(E_AUTH_FAILED);
        assert!(
            started.elapsed() >= Duration::from_millis(150),
            "failures carry a fixed delay"
        );
    }

    // 不正な Hello（版数違い・長さ超過のフレーム）。
    let mut r = Replica::connect(p.repl);
    let mut bad = hello_body(ROLE, ROLE_PW);
    bad[1] = 9;
    r.send(b'H', &bad);
    r.expect_error(E_BAD_REQUEST);
    let mut r = Replica::connect(p.repl);
    r.0.write_all(&[b'H', 0x7f, 0xff, 0xff, 0xff])
        .expect("header only");
    r.expect_error(E_BAD_REQUEST);
    assert_eq!(r.drain_until_closed(), None);

    // 正しいロールは通る。
    let mut ok = Replica::connect(p.repl);
    let (_, timeline, _) = ok.login(ROLE, ROLE_PW);
    assert!(timeline >= 1);

    // レプリケーションのロールはテナント向けのポートでは使えない。
    let mut s = TcpStream::connect(p.tenant).expect("connect tenant");
    common::send_ssl_request_and_startup(&mut s, ROLE, "db");
    let code = common::read_auth_request_type(&mut s);
    assert_eq!(code, 3);
    common::send_password_message(&mut s, ROLE_PW);
    common::expect_error_response_with_sqlstate(&mut s, "28P01");
}

/// 許可リストにない送信元は、資格情報が正しくても何も返されずに閉じられる。
#[test]
fn recover15_peer_outside_the_allow_list_is_closed_silently() {
    let p = Primary::start(
        "repl-allow",
        &[
            "--replication-allow",
            "10.9.9.9",
            "--replication-allow",
            "192.0.2.0/24",
        ],
        false,
    );
    let mut r = Replica::connect(p.repl);
    // 送ってもサーバーは読まずに閉じる（書き込みの結果は問わない）。
    let _ = r.0.write_all(&[b'H', 0, 0, 0, 0]);
    let mut buf = [0u8; 1];
    let n = r.0.read(&mut buf);
    assert!(
        matches!(n, Ok(0) | Err(_)),
        "no bytes are returned to a peer outside the allow list: {n:?}"
    );
}

/// 開始位置からの送信がコミット順・LSN の連続で届き、ack で送信位置（刈り込みの保護）が進む。
/// 確認応答していない区間は残り、確認応答済みの区間は刈り込まれて、その開始位置は拒否される。
#[test]
fn recover15_records_stream_in_commit_order_and_ack_moves_the_retention_floor() {
    let p = Primary::start("repl-stream", &[], true);
    // アーカイブと刈り込みが有効なため、保持位置（接続中のレプリカの ack 位置）を持たない区間は直ちに
    // 刈り込まれる。レプリカを先に接続してから書き込む。
    // 起動時の初期化（ロールの同期など）の commit は、保持位置が無いため既に刈り込まれていてよい。
    let mut a = Replica::connect(p.repl);
    let (db_id, timeline, base) = a.login(ROLE, ROLE_PW);
    a.send(b'S', &start_body(&db_id, timeline, base));
    assert_eq!(a.recv().expect("ok").0, b'O');
    let mut alice = p.setup_table();
    for id in 1..=5 {
        alice.insert(id);
    }
    // CREATE TABLE が複数の commit になりうるため、先頭から受け取った LSN の連続を確かめる。
    let create_commits = {
        let mut probe = Replica::connect(p.repl);
        let (_, _, current) = probe.login(ROLE, ROLE_PW);
        current - base - 5
    };
    let total = create_commits + 5;
    let mut lsns = Vec::new();
    for _ in 0..total {
        let (lsn, rec) = a.next_record().expect("record");
        assert_eq!(&rec[..4], b"FCLR", "self-contained change log record");
        lsns.push(lsn);
    }
    assert_eq!(lsns, (base + 1..=base + total).collect::<Vec<_>>());

    // ライブの追記もコミット順に続く。
    alice.insert(6);
    let (lsn, _) = a.next_record().expect("live record");
    assert_eq!(lsn, base + total + 1);

    // 確認応答した位置（total - 2）より後は刈り込まれず、それ以前は（アーカイブ後に）刈り込まれる。
    let acked = base + total - 2;
    a.ack(acked);
    wait_until(
        "records up to the ack to be pruned",
        Duration::from_secs(30),
        || {
            let mut probe = Replica::connect(p.repl);
            let (db_id, timeline, _) = probe.login(ROLE, ROLE_PW);
            probe.send(b'S', &start_body(&db_id, timeline, 0));
            matches!(probe.recv(), Some((b'E', body)) if body == vec![E_RESYNC])
        },
    );
    // 確認応答済みの位置からは再開でき、未確認の区間が残っている。
    let mut b = Replica::connect(p.repl);
    b.begin(acked);
    let mut again = Vec::new();
    for _ in 0..3 {
        again.push(b.next_record().expect("kept record").0);
    }
    assert_eq!(
        again,
        vec![base + total - 1, base + total, base + total + 1]
    );
    // 確認応答の違反（送信済みより先・後退）は切断される。
    b.ack(1_000_000);
    assert_eq!(b.drain_until_closed(), Some(E_BAD_REQUEST));
    assert!(p.archive.as_ref().is_some_and(|d| d.is_dir()));
}

/// 不一致の DB 識別子・timeline、先走った開始位置、不正な Start は拒否される。
#[test]
fn recover15_start_requests_are_validated() {
    let p = Primary::start("repl-start", &[], false);
    let mut alice = p.setup_table();
    alice.insert(1);

    let mut r = Replica::connect(p.repl);
    let (db_id, timeline, current) = r.login(ROLE, ROLE_PW);
    assert!(current >= 2, "CREATE TABLE and INSERT commit");
    let mut other = db_id;
    other[0] ^= 0xff;
    r.send(b'S', &start_body(&other, timeline, 0));
    r.expect_error(E_FOREIGN);

    let mut r = Replica::connect(p.repl);
    let (db_id, timeline, _) = r.login(ROLE, ROLE_PW);
    r.send(b'S', &start_body(&db_id, timeline + 1, 0));
    r.expect_error(E_FOREIGN);

    let mut r = Replica::connect(p.repl);
    let (db_id, timeline, _) = r.login(ROLE, ROLE_PW);
    r.send(b'S', &start_body(&db_id, timeline, current + 1));
    r.expect_error(E_AHEAD);

    let mut r = Replica::connect(p.repl);
    let (db_id, timeline, _) = r.login(ROLE, ROLE_PW);
    r.send(b'S', &start_body(&db_id, timeline, u64::MAX));
    r.expect_error(E_AHEAD);

    let mut r = Replica::connect(p.repl);
    r.login(ROLE, ROLE_PW);
    r.send(b'S', &[0u8; 31]);
    r.expect_error(E_BAD_REQUEST);

    // 追いついた位置からは受け付けられ、新しい commit が届く。
    let mut r = Replica::connect(p.repl);
    r.begin(current);
    alice.insert(2);
    assert_eq!(r.next_record().expect("record").0, current + 1);
}

/// 切断したレプリカ・読まないレプリカがいても、プライマリの書き込みは止まらない。
/// 未確認の量の上限に達したまま ack が進まないレプリカは切られる（バックプレッシャ）。
#[test]
fn recover15_slow_or_gone_replicas_never_block_primary_writes() {
    let p = Primary::start(
        "repl-backpressure",
        &[
            "--replication-max-unacked-bytes",
            "1024",
            "--replication-ack-timeout-secs",
            "1",
        ],
        false,
    );
    let mut alice = p.setup_table();

    // 途中で切断するレプリカ。
    {
        let mut gone = Replica::connect(p.repl);
        gone.begin(0);
        alice.insert(1);
        let _ = gone.next_record();
    }
    // 読まず ack もしないレプリカ。
    let mut stalled = Replica::connect(p.repl);
    stalled.begin(0);

    let started = Instant::now();
    for id in 2..=60 {
        alice.insert(id);
    }
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "writes proceed while replicas are gone or stalled"
    );

    // 読み続けて ack しないレプリカは、上限に達した後に切られる。
    let mut slow = Replica::connect(p.repl);
    slow.begin(0);
    let mut received = 0usize;
    let mut closed_with = None;
    let cut = Instant::now();
    while let Some((ty, body)) = slow.recv() {
        match ty {
            b'W' => received += 1,
            b'E' => closed_with = body.first().copied(),
            b'P' => {}
            other => panic!("unexpected frame {other}"),
        }
    }
    assert_eq!(closed_with, Some(E_BACKPRESSURE));
    assert!(
        cut.elapsed() >= Duration::from_millis(900),
        "timeout elapsed first"
    );
    assert!(
        (1..62).contains(&received),
        "stopped sending at the window: {received}"
    );
    // 切られた後も書き込みは続く。
    alice.insert(100);
    // 読まなかったレプリカも切られる（書き込みタイムアウト）。
    let _ = stalled.drain_until_closed();
}

/// 同時接続数の上限（認証済みの枠）を超えた接続は、認証の後に断られ、空きが出るまで入れない。
#[test]
fn recover15_connection_limit_is_enforced() {
    let p = Primary::start("repl-limit", &["--replication-max-connections", "1"], false);
    let mut first = Replica::connect(p.repl);
    first.login(ROLE, ROLE_PW);
    let mut second = Replica::connect(p.repl);
    second.send(b'H', &hello_body(ROLE, ROLE_PW));
    second.expect_error(E_BUSY);
    drop(first);
    wait_until("the slot to be released", Duration::from_secs(10), || {
        let mut r = Replica::connect(p.repl);
        r.send(b'H', &hello_body(ROLE, ROLE_PW));
        matches!(r.recv(), Some((b'I', _)))
    });
}

/// 認証前の接続は認証済みの枠を使わず、認証に失敗し続ける送信元は、バックオフの間は何も返されずに閉じられる。
/// 成功すれば数え直される。
#[test]
fn recover15_repeated_auth_failures_back_off_per_source() {
    let p = Primary::start(
        "repl-backoff",
        &["--replication-max-connections", "1"],
        false,
    );
    // 認証前の接続（Hello を送らない）が複数あっても、認証済みの枠 1 は塞がれない。
    let idle: Vec<Replica> = (0..3).map(|_| Replica::connect(p.repl)).collect();
    let mut ok = Replica::connect(p.repl);
    ok.login(ROLE, ROLE_PW);
    drop(ok);
    drop(idle);
    for _ in 0..5 {
        let mut r = Replica::connect(p.repl);
        r.send(b'H', &hello_body(ROLE, "wrong"));
        r.expect_error(E_AUTH_FAILED);
    }
    // 5 回の失敗の後はバックオフ中。正しい資格情報でも何も返されない。
    let mut r = Replica::connect(p.repl);
    let _ = r.0.write_all(&[b'H', 0, 0, 0, 0]);
    assert!(r.recv().is_none(), "no reply while backing off");
    std::thread::sleep(Duration::from_millis(1300));
    let mut r = Replica::connect(p.repl);
    r.login(ROLE, ROLE_PW);
}

/// 保持位置からの Start が刈り込み済み（再同期が必要）か。受け付けられたら偽。
fn resync_required_from(p: &Primary, applied: u64) -> bool {
    let mut probe = Replica::connect(p.repl);
    let (db_id, timeline, _) = probe.login(ROLE, ROLE_PW);
    probe.send(b'S', &start_body(&db_id, timeline, applied));
    matches!(probe.recv(), Some((b'E', body)) if body == vec![E_RESYNC])
}

/// 起動直後の最新 LSN から始めるレプリカを接続する（起動時の commit は保持位置が無く刈り込まれてよい）。
fn begin_at_current(p: &Primary) -> (Replica, u64) {
    let mut r = Replica::connect(p.repl);
    let (db_id, timeline, base) = r.login(ROLE, ROLE_PW);
    r.send(b'S', &start_body(&db_id, timeline, base));
    assert_eq!(r.recv().expect("ok").0, b'O');
    (r, base)
}

/// ack を一度も返さずに接続を保つレプリカは、ハートビートが続いていても `ack_timeout` の後に切られ、
/// 保持位置が外れて刈り込みが進む。
#[test]
fn recover15_replica_that_never_acks_is_cut_and_retention_is_released() {
    let p = Primary::start(
        "repl-silent",
        &["--replication-ack-timeout-secs", "1"],
        true,
    );
    let (mut r, base) = begin_at_current(&p);
    let mut alice = p.setup_table();
    alice.insert(1);
    // 保持位置が残っている間は、その位置から再開できる。
    assert!(!resync_required_from(&p, base));
    // 読み続けるだけで ack しない。切断（バックプレッシャのコード）まで待つ。
    assert_eq!(r.drain_until_closed(), Some(E_BACKPRESSURE));
    wait_until(
        "the released retention floor to let pruning proceed",
        Duration::from_secs(30),
        || resync_required_from(&p, base),
    );
}

/// 読み取りを極端に遅くして書き込みを塞ぐレプリカも、期限で切られる（書き込みタイムアウトをすり抜けない）。
#[test]
fn recover15_replica_reading_too_slowly_is_cut_at_the_deadline() {
    let p = Primary::start("repl-slow", &["--replication-ack-timeout-secs", "1"], true);
    let (mut r, base) = begin_at_current(&p);
    let mut alice = p.alice();
    alice.exec("CREATE TABLE blobs (id BIGINT PRIMARY KEY, body TEXT)");
    let body = "x".repeat(200 * 1024);
    for id in 0..40 {
        alice.exec(&format!(
            "INSERT INTO blobs (id, body) VALUES ({id}, '{body}')"
        ));
    }
    // 1 バイトずつしか読まない。サーバー側の書き込みは塞がるが、監視が期限で切る。
    let mut byte = [0u8; 1];
    for _ in 0..3 {
        let _ = r.0.read(&mut byte);
        std::thread::sleep(Duration::from_millis(300));
    }
    wait_until(
        "the slow reader to be cut and its retention released",
        Duration::from_secs(30),
        || resync_required_from(&p, base),
    );
}

/// 保持位置からの遅れ（レコード数）の上限を超えたレプリカは切られ、保持位置が外れる。再接続しても、
/// 遅れた位置からは再同期が必要（`RESYNC_REQUIRED`）として拒否される。
#[test]
fn recover15_replica_beyond_the_lag_limit_is_cut() {
    let p = Primary::start("repl-lag", &["--replication-max-lag-records", "3"], true);
    let (mut r, base) = begin_at_current(&p);
    let mut alice = p.setup_table();
    for id in 1..=6 {
        alice.insert(id);
    }
    assert_eq!(r.drain_until_closed(), Some(E_RESYNC));
    assert!(resync_required_from(&p, base), "lagging start is refused");
    // 遅れが上限以内の位置からは受け付けられる。
    let mut probe = Replica::connect(p.repl);
    let (_, _, current) = probe.login(ROLE, ROLE_PW);
    assert!(!resync_required_from(&p, current.saturating_sub(1)));
}

/// 送信ウィンドウが満杯で送信ループがログを読まない間も、監視がプライマリの最新 LSN を自分で読んで遅れを判定する。
/// `ack_timeout` を大きくして、切断が遅れの上限によるものだと区別できるようにする。
#[test]
fn recover15_lag_limit_applies_while_the_send_window_is_full() {
    let p = Primary::start(
        "repl-lag-window",
        &[
            "--replication-max-unacked-bytes",
            "1024",
            "--replication-max-lag-records",
            "3",
            "--replication-ack-timeout-secs",
            "600",
        ],
        true,
    );
    let (mut r, base) = begin_at_current(&p);
    let mut alice = p.setup_table();
    // ウィンドウ（1024 バイト）が埋まって送信が止まった後も、commit を積み続ける。
    for id in 1..=12 {
        alice.insert(id);
    }
    let started = Instant::now();
    assert_eq!(r.drain_until_closed(), Some(E_RESYNC));
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "cut by the lag limit, far earlier than the ack timeout"
    );
    wait_until(
        "the released retention floor to let pruning proceed",
        Duration::from_secs(30),
        || resync_required_from(&p, base),
    );
}

/// 不正な Ack（後退・送信済みの超過・型違い・長さ違反）は、固定コード `BAD_REQUEST` を返してから切断される。
#[test]
fn recover15_invalid_acks_get_bad_request_before_the_close() {
    let p = Primary::start("repl-bad-ack", &[], false);
    let mut alice = p.setup_table();
    let cases: [(&str, Box<dyn Fn(&mut Replica, u64)>); 4] = [
        (
            "regressing ack",
            Box::new(|r, lsn| {
                r.ack(lsn);
                r.ack(lsn - 1);
            }),
        ),
        (
            "ack beyond what was sent",
            Box::new(|r, lsn| r.ack(lsn + 1_000)),
        ),
        (
            "wrong frame type",
            Box::new(|r, lsn| r.send(b'X', &lsn.to_be_bytes())),
        ),
        ("wrong ack length", Box::new(|r, _| r.send(b'K', &[0u8; 4]))),
    ];
    for (n, (what, bad)) in cases.into_iter().enumerate() {
        let (mut r, _) = begin_at_current(&p);
        alice.insert(u32::try_from(n).expect("small") + 1);
        let (lsn, _) = r.next_record().expect("record");
        bad(&mut r, lsn);
        assert_eq!(r.drain_until_closed(), Some(E_BAD_REQUEST), "{what}");
    }
    // 長さが上限を超えるフレームも同様。
    let (mut r, _) = begin_at_current(&p);
    r.0.write_all(&[b'K', 0, 0, 0, 100])
        .expect("oversize header");
    assert_eq!(r.drain_until_closed(), Some(E_BAD_REQUEST), "oversize ack");
}
