//! `wire-server replica`（レプリカの適用ループ。Issue #2174・親 #2171・RECOVER-15 ポインタ。方式の記録は
//! `docs/design/replication-streaming.md`）をバイナリ子プロセスで外形検証する。
//!
//! 実プライマリ（`--change-log --replication-bind`）から取ったベースで `replica` を起動し、書き込みへの追従、
//! kill 後の再起動で適用位置（DB の世代）から再開すること、テスト内の偽プライマリが送る欠番・並べ替え・
//! チェックサム破損・別識別子のフレームで固定の終了コードで停止し DB が変わらないこと、不正な引数・資格情報の
//! 権限・非ループバックの拒否を固定する。サーバーはポート 0 と一意な一時ディレクトリで起動する。

use crate::common;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::storage::{chain_point_of, RowInput, Storage, Visibility};

const BIN: &str = env!("CARGO_BIN_EXE_wire-server");
pub(crate) const ROLE: &str = "replica-1";
const ROLE_PW: &str = "pw-replica";
const EXIT_USAGE: i32 = 2;
const EXIT_APPLY_HALTED: i32 = 10;
const EXIT_PRIMARY_REJECTED: i32 = 11;

pub(crate) struct ChildGuard(pub(crate) Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) fn run(args: &[&str]) -> Output {
    Command::new(BIN).args(args).output().expect("run")
}

pub(crate) fn s(p: &Path) -> &str {
    p.to_str().expect("utf-8 path")
}

fn make_private_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir(path).expect("mkdir");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

pub(crate) fn phc_for(password: &str) -> String {
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

/// `role:password` の資格情報ファイル（0600）を作る。
pub(crate) fn write_credentials(dir: &Path, mode: u32) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let p = dir.join("replica-credentials");
    std::fs::write(&p, format!("{ROLE}:{ROLE_PW}\n")).expect("credentials");
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(mode)).expect("chmod");
    p
}

/// 子プロセスの標準エラーを行ごとに溜める。
pub(crate) struct Lines(Arc<Mutex<Vec<String>>>);

impl Lines {
    pub(crate) fn attach(child: &mut Child) -> Self {
        let stderr = child.stderr.take().expect("piped stderr");
        let buf = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&buf);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                sink.lock().expect("lock").push(line);
            }
        });
        Self(buf)
    }

    pub(crate) fn snapshot(&self) -> Vec<String> {
        self.0.lock().expect("lock").clone()
    }

    /// `applied lsn=<n>` の n の最大値。
    fn max_applied(&self) -> u64 {
        self.snapshot()
            .iter()
            .filter_map(|l| l.split("applied lsn=").nth(1))
            .filter_map(|r| {
                r.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
                    .parse::<u64>()
                    .ok()
            })
            .max()
            .unwrap_or(0)
    }

    pub(crate) fn wait_applied(&self, target: u64) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            if self.max_applied() >= target {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("replica did not reach lsn {target}: {:?}", self.snapshot());
    }
}

pub(crate) struct Primary {
    pub(crate) _fx: common::TempFixtureDir,
    pub(crate) _child: ChildGuard,
    pub(crate) tenant: SocketAddr,
    pub(crate) repl: SocketAddr,
    pub(crate) dir: PathBuf,
    pub(crate) sock: PathBuf,
}

impl Primary {
    pub(crate) fn start(label: &str) -> Self {
        Self::start_with(label, &[])
    }

    /// 追加の引数（同期レプリケーションの指定など）つきで起動する（Issue #2245）。
    pub(crate) fn start_with(label: &str, extra: &[&str]) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let fx = common::TempFixtureDir::new(label);
        let users = fx.users_path_str();
        let db = fx.db_path_str();
        let dir = Path::new(&db).parent().expect("parent").to_path_buf();
        std::fs::write(&users, format!("alice:tenant-a:{}\n", phc_for("pw-alice"))).expect("users");
        let roles = dir.join("replication-roles.txt");
        std::fs::write(&roles, format!("{ROLE}:{}\n", phc_for(ROLE_PW))).expect("roles");
        let sock_dir = dir.join("s");
        std::fs::create_dir(&sock_dir).expect("sock dir");
        std::fs::set_permissions(&sock_dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        let sock = sock_dir.join("b.sock");
        let mut child = Command::new(BIN)
            .args([
                "--users",
                &users,
                "--db",
                &db,
                "--bind",
                "127.0.0.1:0",
                "--ddl-allowed-users",
                "alice",
                "--change-log",
                "--backup-socket",
                s(&sock),
                "--replication-bind",
                "127.0.0.1:0",
                "--replication-roles",
                s(&roles),
                "--replication-allow",
                "127.0.0.1",
            ])
            .args(extra)
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
            dir,
            sock,
        }
    }

    pub(crate) fn session(&self) -> TenantSession {
        let stream = common::authenticate_to_ready_for_query(self.tenant, "alice", "pw-alice");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .expect("timeout");
        TenantSession(stream)
    }
}

pub(crate) struct TenantSession(pub(crate) TcpStream);

impl TenantSession {
    pub(crate) fn exec(&mut self, sql: &str) {
        common::send_simple_query(&mut self.0, sql);
        let _ = common::read_command_complete(&mut self.0);
        common::read_ready_for_query(&mut self.0);
    }

    pub(crate) fn insert(&mut self, id: u32) {
        self.exec(&format!(
            "INSERT INTO docs (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]')"
        ));
    }
}

pub(crate) fn spawn_replica(
    db: &Path,
    primary: SocketAddr,
    cred: &Path,
    extra: &[&str],
) -> (ChildGuard, Lines) {
    let mut child = Command::new(BIN)
        .args(["replica", "--db", s(db), "--primary"])
        .arg(primary.to_string())
        .args(["--credentials", s(cred)])
        .args(extra)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn replica");
    let lines = Lines::attach(&mut child);
    (ChildGuard(child), lines)
}

/// レプリカの DB を開き直して `sql` の結果行数を返す（変更ログ有効の DB でも読み取りは通る）。
pub(crate) fn count_rows(db: &Path, sql: &str) -> usize {
    let storage = Storage::open(db).expect("open");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("tenant");
    let mut session = engine::sql::mode::SessionState::default();
    let outcome = core
        .execute_sql_in_session(&ctx, &mut session, sql)
        .expect("select");
    let engine::sql::SqlOutcome::Query(result) = outcome else {
        panic!("expected Query outcome");
    };
    result.rows.len()
}

/// 実プライマリに追従し、kill 後の再起動で適用位置から再開して、終点・全行がプライマリと一致する。
#[test]
fn recover15_replica_follows_primary_and_resumes_after_kill() {
    let primary = Primary::start("replica-follow");
    let mut t = primary.session();
    t.exec("CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))");
    for id in 1..=3 {
        t.insert(id);
    }
    // ベースは稼働中のプライマリからソケット経由で取る。
    let base = primary.dir.join("base.redb");
    let o = run(&["backup", "--socket", s(&primary.sock), "--out", s(&base)]);
    assert_eq!(o.status.code(), Some(0), "{:?}", o);
    let base_lsn = chain_point_of(&base).expect("chain point").end_lsn;
    let rdir = primary.dir.join("replica");
    std::fs::create_dir(&rdir).expect("replica dir");
    let rdb = rdir.join("replica.redb");
    std::fs::copy(&base, &rdb).expect("copy base");
    let cred = write_credentials(&rdir, 0o600);

    // 追従（ベース以降の 5 commit）。
    for id in 4..=8 {
        t.insert(id);
    }
    let (mut guard, lines) = spawn_replica(&rdb, primary.repl, &cred, &[]);
    lines.wait_applied(base_lsn + 5);

    // kill（適用位置 = DB の世代から再開する）。停止中の書き込みを含めて追従する。
    let _ = guard.0.kill();
    let _ = guard.0.wait();
    for id in 9..=12 {
        t.insert(id);
    }
    let (mut guard2, lines2) = spawn_replica(&rdb, primary.repl, &cred, &[]);
    lines2.wait_applied(base_lsn + 9);
    // 再起動直後の接続は、前回までに適用した位置から始まる（先頭からやり直さない）。
    let started = lines2
        .snapshot()
        .into_iter()
        .find_map(|l| {
            l.split("connected (applied lsn=")
                .nth(1)
                .map(str::to_string)
        })
        .expect("connected line");
    assert!(
        started.starts_with(&(base_lsn + 5).to_string()),
        "resumed from the applied position: {started}"
    );

    let _ = guard2.0.kill();
    let _ = guard2.0.wait();
    assert_eq!(count_rows(&rdb, "SELECT id FROM docs LIMIT 1000"), 12);
    assert_eq!(
        count_rows(&rdb, "SELECT id FROM docs WHERE id >= 9 LIMIT 1000"),
        4
    );
    assert_eq!(
        chain_point_of(&rdb).expect("replica point").end_lsn,
        base_lsn + 9
    );
}

/// 偽プライマリが送る不整合なフレーム列の種類。
#[derive(Clone, Copy)]
enum Bad {
    /// 先頭が欠番（最初のレコードを飛ばす）。
    Gap,
    /// 順序を入れ替える。
    Reordered,
    /// チェックサムを壊す。
    Corrupt,
    /// 別の DB 識別子の Identity。
    ForeignIdentity,
    /// 適用済みの位置より先に進めという `E`（再同期が必要）。
    ResyncRequired,
}

pub(crate) fn frame(ty: u8, body: &[u8]) -> Vec<u8> {
    let mut f = vec![ty];
    f.extend_from_slice(&u32::try_from(body.len()).expect("len").to_be_bytes());
    f.extend_from_slice(body);
    f
}

pub(crate) fn read_frame(stream: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut h = [0u8; 5];
    stream.read_exact(&mut h).ok()?;
    let len = u32::from_be_bytes([h[1], h[2], h[3], h[4]]) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).ok()?;
    Some((h[0], body))
}

/// 変更ログ有効の DB に 3 行 → スナップショット（ベース）→ 3 行を足し、ベース以降のレコードを返す。
/// ベースのパス・終点の LSN・DB 識別子・timeline・ベース以降のレコード。
type BaseAndRecords = (PathBuf, u64, [u8; 16], u64, Vec<(u64, Vec<u8>)>);

fn base_and_records(dir: &Path) -> BaseAndRecords {
    let src = dir.join("src.redb");
    let mut storage = Storage::open(&src).expect("open");
    storage.enable_change_log().expect("enable");
    let put = |storage: &Storage, ids: std::ops::RangeInclusive<u64>| {
        for id in ids {
            storage
                .put(
                    id,
                    &RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[0.5, id as f32],
                        metadata: b"m",
                    },
                )
                .expect("put");
        }
    };
    put(&storage, 1..=3);
    let out = dir.join("out");
    make_private_dir(&out);
    let base = out.join("base.redb");
    storage.export_snapshot(&base).expect("snapshot");
    let point = chain_point_of(&base).expect("point");
    put(&storage, 4..=6);
    let records = storage
        .read_change_log_from(point.end_lsn + 1, 100, usize::MAX)
        .expect("records")
        .records;
    assert_eq!(records.len(), 3);
    (base, point.end_lsn, point.db_id, point.timeline, records)
}

fn run_against_fake_primary(label: &str, bad: Bad) {
    let fx = common::TempFixtureDir::new(label);
    let dir = Path::new(&fx.db_path_str())
        .parent()
        .expect("parent")
        .to_path_buf();
    let (base, base_lsn, db_id, timeline, records) = base_and_records(&dir);
    let rdb = dir.join("replica.redb");
    std::fs::copy(&base, &rdb).expect("copy");
    let cred = write_credentials(&dir, 0o600);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");

    let mut ident_db = db_id;
    if matches!(bad, Bad::ForeignIdentity) {
        ident_db[0] ^= 0xff;
    }
    let mut recs = records;
    match bad {
        Bad::Gap => {
            recs.remove(0);
        }
        Bad::Reordered => recs.swap(0, 1),
        Bad::Corrupt => {
            if let Some(last) = recs[0].1.last_mut() {
                *last ^= 0xff;
            }
        }
        Bad::ForeignIdentity | Bad::ResyncRequired => {}
    }
    let server = std::thread::spawn(move || {
        let (mut conn, _) = listener.accept().expect("accept");
        conn.set_read_timeout(Some(Duration::from_secs(20))).ok();
        let (ty, _) = read_frame(&mut conn).expect("hello");
        assert_eq!(ty, b'H');
        let mut ident = ident_db.to_vec();
        ident.extend_from_slice(&timeline.to_be_bytes());
        ident.extend_from_slice(&(base_lsn + 3).to_be_bytes());
        conn.write_all(&frame(b'I', &ident)).expect("identity");
        if matches!(bad, Bad::ForeignIdentity) {
            // Start を送らずに閉じられる（読み取りが EOF になる）ことを確かめる。
            assert!(
                read_frame(&mut conn).is_none(),
                "no Start for a foreign primary"
            );
            return;
        }
        let (ty, start) = read_frame(&mut conn).expect("start");
        assert_eq!(ty, b'S');
        assert_eq!(
            u64::from_be_bytes(start[24..32].try_into().expect("applied")),
            base_lsn
        );
        if matches!(bad, Bad::ResyncRequired) {
            conn.write_all(&frame(b'E', &[5])).expect("error");
            return;
        }
        let mut ok = db_id.to_vec();
        ok.extend_from_slice(&timeline.to_be_bytes());
        ok.extend_from_slice(&(base_lsn + 3).to_be_bytes());
        let mut out = frame(b'O', &ok);
        for (lsn, raw) in &recs {
            let mut body = lsn.to_be_bytes().to_vec();
            body.extend_from_slice(raw);
            out.extend_from_slice(&frame(b'W', &body));
        }
        conn.write_all(&out).expect("records");
        // 停止後に閉じられるまで読み捨てる。
        while read_frame(&mut conn).is_some() {}
    });

    let (mut guard, lines) = spawn_replica(&rdb, addr, &cred, &[]);
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(st) = guard.0.try_wait().expect("try_wait") {
            break st;
        }
        assert!(
            Instant::now() < deadline,
            "replica did not stop: {:?}",
            lines.snapshot()
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    let expected = match bad {
        Bad::ForeignIdentity | Bad::ResyncRequired => EXIT_PRIMARY_REJECTED,
        _ => EXIT_APPLY_HALTED,
    };
    assert_eq!(status.code(), Some(expected), "{:?}", lines.snapshot());
    server.join().expect("fake primary");
    // 適用は 1 件も進まず、DB は直前の整合状態のまま。
    assert_eq!(chain_point_of(&rdb).expect("point").end_lsn, base_lsn);
    // 資格情報・パスワードは標準エラーに出ない。
    for l in lines.snapshot() {
        assert!(!l.contains(ROLE_PW), "password leaked: {l}");
    }
    // 再同期が必要なときだけ、固有の文言で他の拒否と区別できる（終了コードは 11 のまま。Issue #2176）。
    let resync_line = lines
        .snapshot()
        .iter()
        .any(|l| l.contains("resync required"));
    assert_eq!(
        resync_line,
        matches!(bad, Bad::ResyncRequired),
        "{:?}",
        lines.snapshot()
    );
}

#[test]
fn recover15_replica_halts_on_a_gap() {
    run_against_fake_primary("replica-gap", Bad::Gap);
}

#[test]
fn recover15_replica_halts_on_reordered_records() {
    run_against_fake_primary("replica-reorder", Bad::Reordered);
}

#[test]
fn recover15_replica_halts_on_a_corrupt_checksum() {
    run_against_fake_primary("replica-corrupt", Bad::Corrupt);
}

#[test]
fn recover15_replica_reports_a_foreign_primary_without_sending_start() {
    run_against_fake_primary("replica-foreign", Bad::ForeignIdentity);
}

#[test]
fn recover15_replica_reports_when_resync_is_required() {
    run_against_fake_primary("replica-resync", Bad::ResyncRequired);
}

/// 不正な引数・資格情報の権限・非ループバック・重複フラグは接続前に拒否される。
#[test]
fn recover15_replica_rejects_bad_arguments_before_connecting() {
    let fx = common::TempFixtureDir::new("replica-args");
    let dir = Path::new(&fx.db_path_str())
        .parent()
        .expect("parent")
        .to_path_buf();
    let db = dir.join("replica.redb");
    std::fs::write(&db, b"not a real database").expect("db");
    let good = write_credentials(&dir, 0o600);
    let loose_dir = dir.join("loose");
    std::fs::create_dir(&loose_dir).expect("dir");
    let loose = write_credentials(&loose_dir, 0o644);
    let base = |extra: &[&str]| -> Output {
        let mut args = vec!["replica", "--db", s(&db), "--credentials", s(&good)];
        args.extend_from_slice(extra);
        run(&args)
    };
    let cases: Vec<Output> = vec![
        base(&["--primary", "127.0.0.1:9", "--primary", "127.0.0.1:9"]),
        base(&["--primary", "10.0.0.5:9"]),
        base(&["--primary", "127.0.0.1:9", "--bind", "127.0.0.1:0"]),
        base(&["--primary", "127.0.0.1:9", "--max-backoff-secs", "0"]),
        run(&[
            "replica",
            "--db",
            s(&db),
            "--primary",
            "127.0.0.1:9",
            "--credentials",
            s(&loose),
        ]),
        run(&[
            "replica",
            "--db",
            s(&dir.join("absent.redb")),
            "--primary",
            "127.0.0.1:9",
            "--credentials",
            s(&good),
        ]),
    ];
    for o in cases {
        assert_eq!(o.status.code(), Some(EXIT_USAGE), "{:?}", o);
        let err = String::from_utf8_lossy(&o.stderr).into_owned();
        assert!(!err.contains(ROLE_PW), "password leaked: {err}");
    }
    // 存在しなかった DB は作られない。
    assert!(!dir.join("absent.redb").exists());
}
// ---- 読み取りレプリカモード（Issue #2175）----

/// simple query を送り、DataRow の件数（成功）または ErrorResponse の SQLSTATE（失敗）を返す。
pub(crate) fn query_outcome(stream: &mut TcpStream, sql: &str) -> Result<usize, String> {
    common::send_simple_query(stream, sql);
    let mut rows = 0;
    let mut error: Option<String> = None;
    loop {
        let (ty, body) = common::read_message(stream);
        match ty {
            b'D' => rows += 1,
            b'E' => {
                // フィールド列 `<code byte><cstring>` から `C`（SQLSTATE）を拾う。
                let mut rest = body.as_slice();
                while let Some((&code, tail)) = rest.split_first() {
                    if code == 0 {
                        break;
                    }
                    let end = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
                    if code == b'C' {
                        error = Some(String::from_utf8_lossy(&tail[..end]).into_owned());
                    }
                    rest = tail.get(end + 1..).unwrap_or(&[]);
                }
            }
            b'Z' => break,
            _ => {}
        }
    }
    match error {
        Some(code) => Err(code),
        None => Ok(rows),
    }
}

/// 読み取りレプリカ（`--replica-primary`）が、プライマリの書き込みに追従して読み取りに応じ、
/// 書き込み・DDL・明示 READ WRITE を `25006` で拒否し、拒否の前後で内容が変わらないこと。
#[test]
fn recover15_read_replica_serves_reads_and_rejects_writes() {
    let primary = Primary::start("read-replica");
    let mut t = primary.session();
    t.exec("CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))");
    for id in 1..=3 {
        t.insert(id);
    }
    let base = primary.dir.join("base.redb");
    let o = run(&["backup", "--socket", s(&primary.sock), "--out", s(&base)]);
    assert_eq!(o.status.code(), Some(0), "{:?}", o);
    let rdir = primary.dir.join("replica");
    std::fs::create_dir(&rdir).expect("replica dir");
    let rdb = rdir.join("replica.redb");
    std::fs::copy(&base, &rdb).expect("copy base");
    let cred = write_credentials(&rdir, 0o600);
    let users = rdir.join("users.txt");
    std::fs::write(&users, format!("alice:tenant-a:{}\n", phc_for("pw-alice"))).expect("users");
    for id in 4..=6 {
        t.insert(id);
    }

    let mut child = Command::new(BIN)
        .args([
            "--users",
            s(&users),
            "--db",
            s(&rdb),
            "--bind",
            "127.0.0.1:0",
        ])
        .args(["--replica-primary", &primary.repl.to_string()])
        .args(["--replica-credentials", s(&cred)])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn read replica");
    let (addr, _lines) =
        common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(15));
    let _guard = ChildGuard(child);
    let mut r = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    r.set_read_timeout(Some(Duration::from_secs(20)))
        .expect("timeout");

    // 追従: ポーリング（上限つき）でプライマリの 6 行がレプリカの読み取りに現れる。
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if query_outcome(&mut r, "SELECT id FROM docs LIMIT 1000") == Ok(6) {
            break;
        }
        assert!(Instant::now() < deadline, "replica did not catch up");
        std::thread::sleep(Duration::from_millis(100));
    }
    // 書き込み・DDL・明示 READ WRITE・COPY FROM は 25006。接続は生きたまま次の読み取りに応じる。
    for sql in [
        "INSERT INTO docs (id, embedding) VALUES (100, '[0.1,0.2,0.3]')",
        "UPDATE docs SET embedding = '[0.9,0.9,0.9]' WHERE id = 1",
        "DELETE FROM docs WHERE id = 1",
        "CREATE TABLE t2 (id BIGINT PRIMARY KEY, embedding VECTOR(3))",
        "BEGIN READ WRITE",
        "COPY docs (id, embedding) FROM STDIN",
    ] {
        assert_eq!(
            query_outcome(&mut r, sql),
            Err("25006".to_string()),
            "sql: {sql}"
        );
    }
    // 修飾子なしの BEGIN の中でも書き込みは拒否され、ROLLBACK で戻れる。
    assert_eq!(query_outcome(&mut r, "BEGIN"), Ok(0));
    assert_eq!(
        query_outcome(
            &mut r,
            "INSERT INTO docs (id, embedding) VALUES (101, '[0.1,0.2,0.3]')"
        ),
        Err("25006".to_string())
    );
    assert_eq!(query_outcome(&mut r, "ROLLBACK"), Ok(0));
    // 拒否の後も内容は変わらない。
    assert_eq!(
        query_outcome(&mut r, "SELECT id FROM docs LIMIT 1000"),
        Ok(6)
    );
    assert_eq!(
        query_outcome(&mut r, "SELECT id FROM docs WHERE id = 100"),
        Ok(0)
    );
}

/// 読み取りレプリカのフラグは、変更ログ・送信側・バックアップ・ベースでない DB と併用できない（DB を開く前に拒否）。
#[test]
fn recover15_read_replica_rejects_bad_combinations() {
    let fx = common::TempFixtureDir::new("read-replica-args");
    let users = fx.users_path_str();
    let db = fx.db_path_str();
    let dir = Path::new(&db).parent().expect("parent").to_path_buf();
    std::fs::write(&users, format!("alice:tenant-a:{}\n", phc_for("pw-alice"))).expect("users");
    // ベースではない通常の DB（存在するファイル）。
    {
        let _ = Storage::open(&db).expect("create plain db");
    }
    let cred = write_credentials(&dir, 0o600);
    let base_args = |extra: &[&str]| -> Output {
        let mut a = vec![
            "--users",
            &users,
            "--db",
            &db,
            "--bind",
            "127.0.0.1:0",
            "--replica-primary",
            "127.0.0.1:9",
            "--replica-credentials",
            s(&cred),
        ];
        a.extend_from_slice(extra);
        run(&a)
    };
    for extra in [
        &["--change-log"][..],
        &["--backup-socket", "/nonexistent/b.sock"][..],
        &["--replica-primary", "127.0.0.1:9"][..],
    ] {
        let o = base_args(extra);
        assert_ne!(o.status.code(), Some(0), "{extra:?}: {:?}", o);
    }
    // 併用の問題が無くても、ベースでない DB は起動を拒否する（listen しない）。
    let o = base_args(&[]);
    assert_ne!(o.status.code(), Some(0), "{:?}", o);
    let err = String::from_utf8_lossy(&o.stderr).into_owned();
    assert!(err.contains("not a replica base"), "{err}");
}
