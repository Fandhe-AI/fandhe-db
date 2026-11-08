//! レプリケーション接続の認証と TLS の強制（Issue #2177・親 #2171・RECOVER-15 (3)・WIRE-9 ポインタ。
//! 方式の記録は `docs/design/replication-streaming.md` の「実装判断（Issue #2177）」）をバイナリ子プロセスで外形検証する。
//!
//! 固定する性質: (1) 非ループバックの TLS なし構成は起動拒否・TLS 材料の不備（片方だけ・不一致・期限切れ）も起動拒否で、
//! エラー文にパスを含めない、(2) TLS を構成したリスナーへの平文接続は何も返さずに閉じる、(3) TLS 上でも不正な資格情報
//! （誤パスワード・未知のロール・テナントのロール）は拒否され、正しい資格情報では Identity から変更の受信まで進む、
//! (4) 受信側（`replica`・`replica-resync`・読み取りレプリカモード）は `--primary-tls-cert` のピンと一致する証明書の
//! プライマリにだけ資格情報を送り、不一致なら終了コード 11 の固定文言で止まる（再試行し続けない）。
//! サーバーはポート 0 と一意な一時ディレクトリで起動する。

use crate::common;
use crate::tls_client;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use engine::storage::chain_point_of;
use wire_server::replication_conn::{self, ConnReader, ConnWriter};
use wire_server::tls::client_handshake::{
    perform_client_handshake, ClientHandshakeError, PinnedServerCertificate,
};
use wire_server::tls::server_handshake::perform_server_handshake;

use super::replica_cli::{
    count_rows, phc_for, query_outcome, run, s, spawn_replica, write_credentials, ChildGuard,
    Primary as ReplicaPrimary,
};
use super::replication_cli::{hello_body, start_body, Primary, ROLE, ROLE_PW};

const E_AUTH_FAILED: u8 = 2;
const EXIT_PRIMARY_REJECTED: i32 = 11;

/// 有効な証明書・鍵の PEM を書き出す。`validity` で DER を変えられる（同じ鍵でも DER が違えばピンは不一致）。
fn write_pair(dir: &Path, stem: &str, not_before: &str, not_after: &str) -> (PathBuf, PathBuf) {
    let seed = tls_client::hex_decode32(tls_client::RFC8032_TEST1_SEED);
    let der = tls_client::build_ed25519_leaf_certificate_der_with_validity(
        &tls_client::RFC8032_TEST1_PUBLIC_KEY,
        not_before,
        not_after,
    );
    let cert = dir.join(format!("{stem}-cert.pem"));
    let key = dir.join(format!("{stem}-key.pem"));
    tls_client::write_pem_file(&cert, &tls_client::pem_wrap("CERTIFICATE", &der));
    tls_client::write_pem_file(&key, &tls_client::ed25519_pkcs8_pem(&seed));
    (cert, key)
}

/// プライマリの証明書・鍵と、ピン不一致の試験に使う別の証明書（同じ鍵・別の有効期間）。
struct Certs {
    _fx: common::TempFixtureDir,
    cert: PathBuf,
    key: PathBuf,
    other_cert: PathBuf,
}

fn certs(label: &str) -> Certs {
    let fx = common::TempFixtureDir::new(label);
    let dir = Path::new(&fx.db_path_str())
        .parent()
        .expect("parent")
        .to_path_buf();
    let (cert, key) = write_pair(&dir, "primary", "160801121924Z", "401231235959Z");
    let (other_cert, _) = write_pair(&dir, "other", "170801121924Z", "401231235959Z");
    Certs {
        _fx: fx,
        cert,
        key,
        other_cert,
    }
}

fn tls_flags(c: &Certs) -> Vec<&str> {
    vec![
        "--replication-tls-cert",
        s(&c.cert),
        "--replication-tls-key",
        s(&c.key),
    ]
}

/// TLS で接続する最小のレプリカ側クライアント（本番のクライアント側ハンドシェイクを使う）。
struct TlsReplica {
    rd: ConnReader,
    wr: ConnWriter,
}

impl TlsReplica {
    fn connect(addr: SocketAddr, pin_file: &Path) -> Result<Self, ClientHandshakeError> {
        let pin = PinnedServerCertificate::load_file(pin_file).expect("load pin");
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("timeout");
        let session = perform_client_handshake(&mut stream, &pin)?;
        let (mut rd, wr) = replication_conn::tls(stream, session).expect("split");
        rd.set_read_timeout(Some(Duration::from_secs(20)))
            .expect("timeout");
        Ok(Self { rd, wr })
    }

    fn send(&mut self, ty: u8, body: &[u8]) {
        let mut f = vec![ty];
        f.extend_from_slice(&u32::try_from(body.len()).expect("len").to_be_bytes());
        f.extend_from_slice(body);
        self.wr.write_all(&f).expect("send frame");
    }

    fn recv(&mut self) -> Option<(u8, Vec<u8>)> {
        let mut h = [0u8; 5];
        self.rd.read_exact(&mut h).ok()?;
        let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
        assert!(len <= 300 * 1024 * 1024, "unreasonable frame length");
        let mut body = vec![0u8; len];
        self.rd.read_exact(&mut body).ok()?;
        Some((h[0], body))
    }
}

/// TLS 構成の送信側へ、非ループバックの bind（`0.0.0.0`）で起動する。
fn start_tls_primary(label: &str, c: &Certs) -> Primary {
    let mut extra = vec!["--replication-bind", "0.0.0.0:0"];
    extra.extend(tls_flags(c));
    Primary::start(label, &extra, false)
}

/// 起動時の検査: TLS なしの非ループバック・TLS 材料の不備は起動拒否で、エラー文にパスを含めない。
#[test]
fn recover15_replication_tls_flags_are_validated_at_startup() {
    let c = certs("repl-tls-flags");
    let fx = common::TempFixtureDir::new("repl-tls-flags-run");
    common::write_empty_user_store(&fx.users_path_str());
    let dir = Path::new(&fx.db_path_str())
        .parent()
        .expect("p")
        .to_path_buf();
    let roles = dir.join("roles.txt");
    std::fs::write(&roles, format!("{ROLE}:{}\n", phc_for(ROLE_PW))).expect("roles");
    let roles_s = roles.to_str().expect("utf-8").to_string();
    let expired = write_pair(&dir, "expired", "160801121924Z", "170801121924Z");
    let mismatched_key = {
        // 鍵と証明書が一致しない組（別の公開鍵を埋め込んだ証明書）。
        let seed = tls_client::hex_decode32(tls_client::RFC8032_TEST1_SEED);
        let der = tls_client::build_ed25519_leaf_certificate_der_with_validity(
            &[0x11u8; 32],
            "160801121924Z",
            "401231235959Z",
        );
        let cert = dir.join("mismatch-cert.pem");
        let key = dir.join("mismatch-key.pem");
        tls_client::write_pem_file(&cert, &tls_client::pem_wrap("CERTIFICATE", &der));
        tls_client::write_pem_file(&key, &tls_client::ed25519_pkcs8_pem(&seed));
        (cert, key)
    };
    let attempt = |bind: &str, extra: &[&str]| {
        let users = fx.users_path_str();
        let db = fx.db_path_str();
        let mut args = vec![
            "--users",
            &users,
            "--db",
            &db,
            "--bind",
            "127.0.0.1:0",
            "--change-log",
            "--replication-bind",
            bind,
            "--replication-roles",
            &roles_s,
            "--replication-allow",
            "127.0.0.1",
        ];
        args.extend_from_slice(extra);
        let o = run(&args);
        assert_ne!(o.status.code(), Some(0), "{extra:?}: {o:?}");
        String::from_utf8_lossy(&o.stderr).into_owned()
    };
    // TLS なしの非ループバックは拒否。
    let err = attempt("0.0.0.0:0", &[]);
    assert!(err.contains("must be a loopback address"), "{err}");
    // 片方だけ。
    for flag in [
        ["--replication-tls-cert", s(&c.cert)],
        ["--replication-tls-key", s(&c.key)],
    ] {
        let err = attempt("0.0.0.0:0", &flag);
        assert!(err.contains("must be given together"), "{err}");
    }
    // 不一致・期限切れ・存在しないファイル。
    for (cert, key) in [
        (&mismatched_key.0, &mismatched_key.1),
        (&expired.0, &expired.1),
        (&dir.join("none-cert.pem"), &c.key),
    ] {
        let err = attempt(
            "0.0.0.0:0",
            &[
                "--replication-tls-cert",
                s(cert),
                "--replication-tls-key",
                s(key),
            ],
        );
        assert!(err.contains("--replication-tls-cert"), "{err}");
        assert!(!err.contains(s(&dir)), "the path must not be echoed: {err}");
    }
}

/// 受け入れ条件: TLS を構成したリスナーへの平文の接続は、何も返されずに閉じられる（平文の Hello は解釈されない）。
#[test]
fn recover15_plaintext_connection_to_a_tls_listener_is_refused_silently() {
    let c = certs("repl-tls-plain");
    let primary = start_tls_primary("repl-tls-plain-primary", &c);
    let mut s = TcpStream::connect(primary.repl).expect("connect");
    s.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let mut frame = vec![b'H'];
    let body = hello_body(ROLE, ROLE_PW);
    frame.extend_from_slice(&u32::try_from(body.len()).expect("len").to_be_bytes());
    frame.extend_from_slice(&body);
    s.write_all(&frame).expect("send hello");
    let mut received = Vec::new();
    let _ = s.read_to_end(&mut received);
    assert!(
        received.is_empty(),
        "no bytes may come back to a plaintext peer: {received:?}"
    );
    // 平文に拒否されても、その後の TLS 接続は通常どおり使える。
    let mut r = TlsReplica::connect(primary.repl, &c.cert).expect("tls connect");
    r.send(b'H', &hello_body(ROLE, ROLE_PW));
    assert_eq!(r.recv().map(|f| f.0), Some(b'I'));
}

/// 受け入れ条件: TLS 上でも不正な資格情報は拒否され（誤パスワード・未知のロール・テナントのロール）、
/// 正しい資格情報では Identity から変更の受信まで進む。非ループバックの bind でも動く。
#[test]
fn recover15_replication_over_tls_authenticates_and_streams() {
    let c = certs("repl-tls-auth");
    let primary = start_tls_primary("repl-tls-auth-primary", &c);
    for (role, pw) in [
        (ROLE, "wrong-password"),
        ("nobody", "pw"),
        ("alice", "pw-alice"),
    ] {
        let mut r = TlsReplica::connect(primary.repl, &c.cert).expect("tls connect");
        r.send(b'H', &hello_body(role, pw));
        match r.recv() {
            Some((b'E', body)) => assert_eq!(body, vec![E_AUTH_FAILED], "role {role}"),
            other => panic!("expected AUTH_FAILED for {role}, got {other:?}"),
        }
    }

    let mut alice = primary.alice();
    alice.exec("CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))");
    let mut r = TlsReplica::connect(primary.repl, &c.cert).expect("tls connect");
    r.send(b'H', &hello_body(ROLE, ROLE_PW));
    let (ty, body) = r.recv().expect("identity");
    assert_eq!(ty, b'I');
    let db_id: [u8; 16] = body[..16].try_into().expect("db id");
    let timeline = u64::from_be_bytes(body[16..24].try_into().expect("timeline"));
    let current = u64::from_be_bytes(body[24..32].try_into().expect("current"));
    r.send(b'S', &start_body(&db_id, timeline, current));
    assert_eq!(r.recv().map(|f| f.0), Some(b'O'));
    alice.insert(1);
    let mut got = None;
    for _ in 0..50 {
        match r.recv() {
            Some((b'W', body)) => {
                got = Some(u64::from_be_bytes(body[..8].try_into().expect("lsn")));
                break;
            }
            Some((b'P', _)) => continue,
            other => panic!("unexpected frame {other:?}"),
        }
    }
    assert_eq!(got, Some(current + 1), "the commit streams over TLS");
}

/// ピン不一致の証明書では、クライアント側のハンドシェイクが失敗し、資格情報を送る前に止まる。
#[test]
fn recover15_client_refuses_a_certificate_that_is_not_the_pinned_one() {
    let c = certs("repl-tls-pin");
    let primary = start_tls_primary("repl-tls-pin-primary", &c);
    match TlsReplica::connect(primary.repl, &c.other_cert) {
        Err(ClientHandshakeError::PinMismatch) => {}
        Err(e) => panic!("expected a pin mismatch, got {e}"),
        Ok(_) => panic!("the handshake must not succeed"),
    }
}

/// クライアント側ハンドシェイクは、クレート内のサーバー側ハンドシェイクと往復できる（ピンが一致すれば成功し、
/// アプリケーションデータが双方向に通る。ピン不一致・非 TLS の相手は失敗する）。
#[test]
fn recover15_client_handshake_interoperates_with_the_server_side() {
    use std::net::TcpListener;
    let dir = common::TempFixtureDir::new("repl-tls-inproc");
    let dir = Path::new(&dir.db_path_str())
        .parent()
        .expect("parent")
        .to_path_buf();
    let (cert, _) = write_pair(&dir, "inproc", "160801121924Z", "401231235959Z");
    let (other, _) = write_pair(&dir, "inproc-other", "170801121924Z", "401231235959Z");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = std::thread::spawn(move || {
        let config = tls_client::test_config();
        // 1 本目: ピン一致（アプリケーションデータを往復）、2 本目: ピン不一致（クライアントが閉じる）。
        let (mut conn, _) = listener.accept().expect("accept");
        let session = perform_server_handshake(&mut conn, config.clone()).expect("server tls");
        let (mut rd, mut wr) = replication_conn::tls(conn, session).expect("split");
        let mut buf = [0u8; 5];
        rd.read_exact(&mut buf).expect("read");
        wr.write_all(&buf).expect("echo");
        let (mut conn2, _) = listener.accept().expect("accept 2");
        assert!(perform_server_handshake(&mut conn2, config).is_err());
    });
    let pin = PinnedServerCertificate::load_file(&cert).expect("pin");
    let mut stream = TcpStream::connect(addr).expect("connect");
    let session = perform_client_handshake(&mut stream, &pin).expect("client tls");
    let (mut rd, mut wr) = replication_conn::tls(stream, session).expect("split");
    wr.write_all(b"hello").expect("write");
    let mut echoed = [0u8; 5];
    rd.read_exact(&mut echoed).expect("read");
    assert_eq!(&echoed, b"hello");
    let wrong = PinnedServerCertificate::load_file(&other).expect("pin");
    let mut stream2 = TcpStream::connect(addr).expect("connect");
    assert_eq!(
        perform_client_handshake(&mut stream2, &wrong).err(),
        Some(ClientHandshakeError::PinMismatch)
    );
    server.join().expect("server thread");
}

/// 平文の相手（TLS を話さない）へピン付きで接続すると、資格情報を送らずに失敗する（拒否として扱う）。
#[test]
fn recover15_client_does_not_send_credentials_to_a_non_tls_peer() {
    use std::net::TcpListener;
    let dir = common::TempFixtureDir::new("repl-tls-nontls");
    let dir = Path::new(&dir.db_path_str())
        .parent()
        .expect("parent")
        .to_path_buf();
    let (cert, _) = write_pair(&dir, "nontls", "160801121924Z", "401231235959Z");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let peer = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        // 平文のレプリケーションのエラーフレームを返す相手。
        let _ = conn.write_all(&[b'E', 0, 0, 0, 1, 1]);
        let mut seen = Vec::new();
        conn.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let _ = conn.read_to_end(&mut seen);
        seen
    });
    let pin = PinnedServerCertificate::load_file(&cert).expect("pin");
    let mut stream = TcpStream::connect(addr).expect("connect");
    let err = perform_client_handshake(&mut stream, &pin).expect_err("must fail");
    assert!(err.is_rejection(), "{err}");
    let seen = peer.join().expect("peer thread");
    // 相手が受け取ったのは ClientHello（TLS の handshake レコード）だけで、フレーム（Hello）ではない。
    assert!(
        seen.first().is_none_or(|b| *b == 0x16),
        "only TLS bytes may be sent: {seen:?}"
    );
}

fn wait_exit(guard: &mut ChildGuard) -> Option<i32> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(st) = guard.0.try_wait().expect("try_wait") {
            return st.code();
        }
        assert!(Instant::now() < deadline, "process did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// 実プライマリ（TLS）から取ったベースで `replica --primary-tls-cert` が追従し、
/// ピン不一致の `replica` は終了コード 11 と固定文言で止まって何も適用しない。
/// TLS なしの `replica` は接続できず（一時障害として再接続を続け）何も適用しない。
#[test]
fn recover15_replica_follows_over_tls_and_rejects_a_mismatched_pin() {
    let c = certs("repl-tls-replica");
    let mut extra = Vec::new();
    extra.extend(tls_flags(&c));
    let primary = ReplicaPrimary::start_with("repl-tls-replica-primary", &extra);
    let mut t = primary.session();
    t.exec("CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))");
    for id in 1..=3 {
        t.insert(id);
    }
    let base = primary.dir.join("base.redb");
    let o = run(&["backup", "--socket", s(&primary.sock), "--out", s(&base)]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let base_lsn = chain_point_of(&base).expect("chain point").end_lsn;
    let rdir = primary.dir.join("replica");
    std::fs::create_dir(&rdir).expect("replica dir");
    let rdb = rdir.join("replica.redb");
    std::fs::copy(&base, &rdb).expect("copy base");
    let cred = write_credentials(&rdir, 0o600);
    for id in 4..=6 {
        t.insert(id);
    }

    // ピン不一致: 資格情報を送らずに終了コード 11 で止まる。何も適用しない。
    let (mut bad, bad_lines) = spawn_replica(
        &rdb,
        primary.repl,
        &cred,
        &["--primary-tls-cert", s(&c.other_cert)],
    );
    assert_eq!(wait_exit(&mut bad), Some(EXIT_PRIMARY_REJECTED));
    std::thread::sleep(Duration::from_millis(100));
    let out = bad_lines.snapshot();
    assert!(
        out.iter().any(|l| l.contains("TLS handshake was rejected")),
        "{out:?}"
    );
    assert!(
        !out.iter().any(|l| l.contains("connected (applied")),
        "{out:?}"
    );
    assert!(
        !out.iter().any(|l| l.contains(super::replica_cli::ROLE)),
        "the role name must not be logged: {out:?}"
    );
    assert_eq!(chain_point_of(&rdb).expect("point").end_lsn, base_lsn);

    // TLS なしの接続は確立せず、適用もされない（一時障害として再接続を続ける）。
    let (mut plain, plain_lines) = spawn_replica(&rdb, primary.repl, &cred, &[]);
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        plain.0.try_wait().expect("try_wait").is_none(),
        "keeps retrying"
    );
    let out = plain_lines.snapshot();
    assert!(
        !out.iter().any(|l| l.contains("connected (applied")),
        "{out:?}"
    );
    let _ = plain.0.kill();
    let _ = plain.0.wait();
    // kill した DB は読み取り専用では開けないので、書き込み可能で開き直して行数で確かめる（ベースの 3 行のまま）。
    assert_eq!(count_rows(&rdb, "SELECT id FROM docs LIMIT 1000"), 3);

    // ピン一致: TLS で追従する。
    let (mut ok, ok_lines) = spawn_replica(
        &rdb,
        primary.repl,
        &cred,
        &["--primary-tls-cert", s(&c.cert)],
    );
    ok_lines.wait_applied(base_lsn + 3);
    t.insert(7);
    ok_lines.wait_applied(base_lsn + 4);
    let _ = ok.0.kill();
    let _ = ok.0.wait();
    assert_eq!(count_rows(&rdb, "SELECT id FROM docs LIMIT 1000"), 7);
}

/// `replica-resync --primary-tls-cert`: 確認接続が TLS で通り、ピン不一致では何も公開しない。
#[test]
fn recover15_resync_confirms_the_primary_over_tls() {
    let c = certs("repl-tls-resync");
    let mut extra = Vec::new();
    extra.extend(tls_flags(&c));
    let primary = ReplicaPrimary::start_with("repl-tls-resync-primary", &extra);
    let mut t = primary.session();
    t.exec("CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))");
    t.insert(1);
    let rdir = primary.dir.join("replica");
    std::fs::create_dir(&rdir).expect("dir");
    let cred = write_credentials(&rdir, 0o600);
    let repl = primary.repl.to_string();
    let private = |p: &Path| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir(p).expect("mkdir");
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    };
    let resync = |out: &Path, pin: &Path| {
        run(&[
            "replica-resync",
            "--socket",
            s(&primary.sock),
            "--out",
            s(out),
            "--primary",
            &repl,
            "--primary-tls-cert",
            s(pin),
            "--credentials",
            s(&cred),
        ])
    };
    let bad_dir = primary.dir.join("bad");
    private(&bad_dir);
    let bad_out = bad_dir.join("replica.redb");
    let o = resync(&bad_out, &c.other_cert);
    assert_eq!(o.status.code(), Some(EXIT_PRIMARY_REJECTED), "{o:?}");
    assert!(
        !bad_out.exists(),
        "nothing may be published on a pin mismatch"
    );
    let ok_dir = primary.dir.join("ok");
    private(&ok_dir);
    let ok_out = ok_dir.join("replica.redb");
    let o = resync(&ok_out, &c.cert);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    assert!(ok_out.exists());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(!combined.contains(ROLE_PW), "password leaked");
}

/// 読み取りレプリカモードの `--replica-primary-tls-cert` が `--primary-tls-cert` へ写像され、TLS で追従する。
#[test]
fn recover15_read_replica_follows_over_tls() {
    let c = certs("repl-tls-readreplica");
    let mut extra = Vec::new();
    extra.extend(tls_flags(&c));
    let primary = ReplicaPrimary::start_with("repl-tls-readreplica-primary", &extra);
    let mut t = primary.session();
    t.exec("CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))");
    for id in 1..=3 {
        t.insert(id);
    }
    let base = primary.dir.join("base.redb");
    let o = run(&["backup", "--socket", s(&primary.sock), "--out", s(&base)]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let rdir = primary.dir.join("replica");
    std::fs::create_dir(&rdir).expect("dir");
    let rdb = rdir.join("replica.redb");
    std::fs::copy(&base, &rdb).expect("copy base");
    let cred = write_credentials(&rdir, 0o600);
    let users = rdir.join("users.txt");
    std::fs::write(&users, format!("alice:tenant-a:{}\n", phc_for("pw-alice"))).expect("users");
    for id in 4..=6 {
        t.insert(id);
    }
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            s(&users),
            "--db",
            s(&rdb),
            "--bind",
            "127.0.0.1:0",
        ])
        .args(["--replica-primary", &primary.repl.to_string()])
        .args(["--replica-primary-tls-cert", s(&c.cert)])
        .args(["--replica-credentials", s(&cred)])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn read replica");
    let (addr, _lines) =
        common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(15));
    let _guard = ChildGuard(child);
    let mut r = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    r.set_read_timeout(Some(Duration::from_secs(20)))
        .expect("timeout");
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if query_outcome(&mut r, "SELECT id FROM docs LIMIT 1000") == Ok(6) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the read replica did not catch up"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 非ループバックの `--primary` は、ピンなしでは起動拒否（`replica`・`replica-resync`・読み取りレプリカ共通）。
#[test]
fn recover15_non_loopback_primary_requires_the_pin() {
    let fx = common::TempFixtureDir::new("repl-tls-nonloop");
    let dir = Path::new(&fx.db_path_str())
        .parent()
        .expect("p")
        .to_path_buf();
    let db = dir.join("r.redb");
    std::fs::write(&db, b"x").expect("db");
    let cred = write_credentials(&dir, 0o600);
    let o = run(&[
        "replica",
        "--db",
        s(&db),
        "--primary",
        "10.255.255.1:5999",
        "--credentials",
        s(&cred),
    ]);
    assert_eq!(o.status.code(), Some(2), "{o:?}");
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(err.contains("unless --primary-tls-cert is given"), "{err}");
    // 不正なピンは固定文言（パスを含まない）で拒否する。
    let o = run(&[
        "replica",
        "--db",
        s(&db),
        "--primary",
        "10.255.255.1:5999",
        "--primary-tls-cert",
        s(&dir.join("missing-pin.pem")),
        "--credentials",
        s(&cred),
    ]);
    assert_eq!(o.status.code(), Some(2), "{o:?}");
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(err.contains("--primary-tls-cert:"), "{err}");
    assert!(!err.contains("missing-pin"), "{err}");
}
