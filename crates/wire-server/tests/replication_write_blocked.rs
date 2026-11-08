//! レプリケーション送信側で「書き込みが本当に塞がる」場合の切断を外形検証する（Issue #3104・親 #2173・
//! RECOVER-15 ポインタ。方式の記録は `docs/design/replication-streaming.md`）。
//!
//! `wire_cli/replication_cli.rs` は、窓が満杯のときの遅れの判定や 1 バイトずつ読むクライアントを確かめている
//! が、カーネルの送信バッファまで詰まって `write`／`flush` がブロックしている間のプロトコル違反・遅れ・
//! ack 待ちの上限を、切断の経路まで区別して固定してはいない。ここでは `Ok` まで読んだ後は一切読まない
//! レプリカで送信を塞ぎ、次の 3 つがいずれも上限どおりに切断され、保持位置が外れて刈り込みが進み、塞がって
//! いる間もテナントの書き込みが止まらないことを確かめる。
//!
//! - ack 待ちの上限（`--replication-ack-timeout-secs`）
//! - 遅れの上限（`--replication-max-lag-records`）
//! - プロトコル違反（`BAD_REQUEST` を書けないので、監視が有界の待機の後に閉じる）
//!
//! 切断の経路は stderr の `replica '<role>' disconnected (<reason>)` の理由と、対象クライアントが `E`
//! フレームを受け取らないことで判定する。塞がらなかった環境では `E` が届き理由も異なるため、試験は素通り
//! せず失敗する（fail-closed）。
//!
//! 経過時間と、数十 MiB の送信でカーネルのバッファを詰める性質上、並行して動く他の試験の負荷の影響を受けや
//! すいので `wire_cli` へは束ねず単独のバイナリとする（README「wire-server の結合テストの束ね方と個別実行」）。
//! `Ok` を書くときの期限（`START_TIMEOUT`）は、外部から `Ok` の書き込みを塞ぐには受信バッファの縮小
//! （`SO_RCVBUF`）が要り、依存の追加になるため本試験の対象外とする。
//! 補助の型は `replication_cli.rs` の同名のものを写している（束ねないので共有できない）。

#![cfg(unix)]

#[path = "common/mod.rs"]
mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_wire-server");
const ROLE: &str = "replica-1";
const ROLE_PW: &str = "pw-replica";
const PROBE: &str = "probe-1";
const PROBE_PW: &str = "pw-probe";

const E_RESYNC: u8 = 5;

/// 閉塞時の切断の理由（stderr の固定文言）。`E` を書けないまま `write`／`flush` が失敗した経路を表す。
const BLOCKED_REASON: &str = "write failed or timed out";

/// 1 文の TEXT 本体の大きさ（pg wire の 1 メッセージの上限 1 MiB に収める）。
const STMT_BODY: usize = 512 * 1024;
/// 文数。`STMT_BODY * FILL_STMTS` が約 24 MiB で、読まないクライアントの受信窓とループバックの送信バッファ
/// の上限（通常は合わせて数 MiB）を大きく超えるので、送信は必ずブロックする。
const FILL_STMTS: usize = 48;
/// ack 待ちの上限と遅れの上限のどちらも当たらない大きさにする、未確認の量の上限。
const BIG_WINDOW: &str = "268435456";

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

struct Primary {
    _fx: common::TempFixtureDir,
    _child: ChildGuard,
    drain: common::StderrDrain,
    tenant: SocketAddr,
    repl: SocketAddr,
}

impl Primary {
    /// アーカイブ（1 秒周期・保持 0）付きで起動する。`extra` は追加フラグ。ロールは対象の `replica-1` と
    /// 保持位置の確認用の `probe-1` の 2 つ。
    fn start(label: &str, extra: &[&str]) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let fx = common::TempFixtureDir::new(label);
        let users = fx.users_path_str();
        let db = fx.db_path_str();
        let dir = Path::new(&db).parent().expect("parent").to_path_buf();
        std::fs::write(&users, format!("alice:tenant-a:{}\n", phc_for("pw-alice"))).expect("users");
        let roles = dir.join("replication-roles.txt");
        std::fs::write(
            &roles,
            format!(
                "{ROLE}:{}\n{PROBE}:{}\n",
                phc_for(ROLE_PW),
                phc_for(PROBE_PW)
            ),
        )
        .expect("roles");
        let archive = dir.join("archive");
        std::fs::create_dir(&archive).expect("archive dir");
        std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o755)).expect("chmod");
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
            "--replication-allow",
            "127.0.0.1",
            "--change-log-archive-dir",
            archive.to_str().expect("utf-8"),
            "--change-log-archive-interval-secs",
            "1",
            "--change-log-retain-records",
            "0",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        args.extend(extra.iter().map(|s| s.to_string()));
        let mut child = Command::new(BIN)
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server");
        let mut drain = common::drain_stderr(&mut child);
        let (tenant, lines) = drain.wait_for_listening(Duration::from_secs(15));
        let tenant = tenant.unwrap_or_else(|| panic!("no listen address: {lines:?}"));
        let repl = lines
            .iter()
            .find_map(|l| l.split("replication endpoint bound to ").nth(1))
            .and_then(|a| a.trim().parse::<SocketAddr>().ok())
            .unwrap_or_else(|| panic!("no replication address: {lines:?}"));
        Self {
            _fx: fx,
            _child: ChildGuard(child),
            drain,
            tenant,
            repl,
        }
    }

    fn alice(&self) -> TenantSession {
        let stream = common::authenticate_to_ready_for_query(self.tenant, "alice", "pw-alice");
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .expect("timeout");
        TenantSession(stream)
    }

    /// `replica-1` の切断の行（`replica 'replica-1' disconnected (<理由>)`）を待ち、理由と観測時刻を返す。
    fn wait_disconnect(&self, timeout: Duration) -> (String, Instant) {
        let marker = format!("replica '{ROLE}' disconnected (");
        let deadline = Instant::now() + timeout;
        loop {
            let found = self.drain.tail_lines().iter().find_map(|l| {
                l.split(marker.as_str())
                    .nth(1)
                    .map(|r| r.trim_end().trim_end_matches(')').to_string())
            });
            if let Some(reason) = found {
                return (reason, Instant::now());
            }
            assert!(
                Instant::now() < deadline,
                "no disconnect line: {:?}",
                self.drain.tail_lines()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// 保持位置からの Start が刈り込み済み（再同期が必要）か。受け付けられたら偽。`probe-1` で確かめる。
    fn resync_required_from(&self, applied: u64) -> bool {
        let mut probe = Replica::connect(self.repl);
        let (db_id, timeline, _) = probe.login(PROBE, PROBE_PW);
        probe.send(b'S', &start_body(&db_id, timeline, applied));
        matches!(probe.recv(), Some((b'E', body)) if body == vec![E_RESYNC])
    }

    /// プライマリの最新 LSN（`probe-1` の Identity から読む）。
    fn current_lsn(&self) -> u64 {
        Replica::connect(self.repl).login(PROBE, PROBE_PW).2
    }

    /// 表を作り、`replica-1` を最新 LSN から接続して `Ok` まで読む。以降は一切読まない（送信を塞ぐ）。
    /// `(レプリカ, 開始位置, テナントのセッション)`。
    fn begin_blocked_target(&self) -> (Replica, u64, TenantSession) {
        let mut alice = self.alice();
        alice.exec("CREATE TABLE blobs (id BIGINT PRIMARY KEY, body TEXT)");
        let mut r = Replica::connect(self.repl);
        let (db_id, timeline, base) = r.login(ROLE, ROLE_PW);
        r.send(b'S', &start_body(&db_id, timeline, base));
        let (ty, body) = r.recv().expect("ok frame");
        assert_eq!(ty, b'O', "expected ok, got {ty} {body:?}");
        (r, base, alice)
    }

    /// 約 24 MiB を 1 トランザクション（= 変更ログの 1 レコード）で書き、送信を確実に塞ぐ。その後、送信ループが
    /// ブロックに入るまで少し待つ。
    fn fill(&self, alice: &mut TenantSession) {
        let before = self.current_lsn();
        let body = "x".repeat(STMT_BODY);
        alice.exec("BEGIN");
        for id in 0..FILL_STMTS {
            alice.exec(&format!(
                "INSERT INTO blobs (id, body) VALUES ({id}, '{body}')"
            ));
        }
        alice.exec("COMMIT");
        assert_eq!(
            self.current_lsn(),
            before + 1,
            "the fill is one change-log record"
        );
        std::thread::sleep(Duration::from_millis(500));
    }
}

struct TenantSession(TcpStream);

impl TenantSession {
    fn exec(&mut self, sql: &str) {
        common::send_simple_query(&mut self.0, sql);
        let _ = common::read_command_complete(&mut self.0);
        common::read_ready_for_query(&mut self.0);
    }

    /// 小さい書き込みを `ids` の分だけ流し、`bound` 未満で終わること（塞がったレプリカがプライマリの書き込みを
    /// 止めないこと）を確かめる。
    fn assert_small_writes_flow(&mut self, ids: std::ops::Range<u32>, bound: Duration) {
        let started = Instant::now();
        for id in ids {
            self.exec(&format!(
                "INSERT INTO blobs (id, body) VALUES ({}, 's')",
                10_000 + id
            ));
        }
        assert!(
            started.elapsed() < bound,
            "writes proceed while the replica is write-blocked: {:?}",
            started.elapsed()
        );
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
        s.set_read_timeout(Some(Duration::from_secs(30)))
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

    fn ack(&mut self, lsn: u64) {
        self.send(b'K', &lsn.to_be_bytes());
    }

    /// 切断（EOF）されるまで読み捨てる。`E` を見たらそのコードを返す。塞がった経路では `E` は書かれない。
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

/// 切断が閉塞の経路（理由が固定文言・`E` なし）であること、保持位置が外れて刈り込みが進むこと、切断後も
/// 書き込めることを確かめる共通の検査。
fn assert_cut_while_blocked(
    p: &Primary,
    target: &mut Replica,
    base: u64,
    reason: &str,
    alice: &mut TenantSession,
) {
    assert_eq!(reason, BLOCKED_REASON, "cut through the write-blocked path");
    assert_eq!(
        target.drain_until_closed(),
        None,
        "no error frame can be written while blocked"
    );
    wait_until(
        "the released retention floor to let pruning proceed",
        Duration::from_secs(30),
        || p.resync_required_from(base),
    );
    alice.assert_small_writes_flow(100..103, Duration::from_secs(15));
}

/// 送信が塞がったまま ack が来ないレプリカは、`ack_timeout` の上限どおりに切られ、保持位置が外れる。
#[test]
fn recover15_ack_timeout_cuts_a_write_blocked_replica_and_releases_retention() {
    const T: Duration = Duration::from_secs(2);
    let p = Primary::start(
        "repl-blocked-ack",
        &[
            "--replication-ack-timeout-secs",
            "2",
            "--replication-max-unacked-bytes",
            BIG_WINDOW,
        ],
    );
    let (mut r, base, mut alice) = p.begin_blocked_target();
    p.fill(&mut alice);
    let t0 = Instant::now();
    alice.assert_small_writes_flow(0..5, Duration::from_secs(15));
    let (reason, seen) = p.wait_disconnect(Duration::from_secs(30));
    let elapsed = seen.duration_since(t0);
    assert!(
        elapsed >= T - Duration::from_millis(1500),
        "not cut before the ack timeout: {elapsed:?}"
    );
    assert!(
        elapsed < T + Duration::from_secs(10),
        "cut at the ack timeout bound: {elapsed:?}"
    );
    assert_cut_while_blocked(&p, &mut r, base, &reason, &mut alice);
}

/// 送信が塞がっている間に遅れの上限を超えたレプリカは、`ack_timeout` を待たずに切られ、保持位置が外れる。
/// `ack_timeout` を大きくして、切断が遅れの上限によるものだと区別できるようにする。
#[test]
fn recover15_lag_limit_cuts_a_write_blocked_replica_and_releases_retention() {
    let p = Primary::start(
        "repl-blocked-lag",
        &[
            "--replication-ack-timeout-secs",
            "600",
            "--replication-max-lag-records",
            "3",
            "--replication-max-unacked-bytes",
            BIG_WINDOW,
        ],
    );
    let (mut r, base, mut alice) = p.begin_blocked_target();
    p.fill(&mut alice);
    let started = Instant::now();
    // 遅れ（最新 - 確認応答済み）を 3 より大きくする。この書き込みが塞がっている間の書き込みの確認も兼ねる。
    alice.assert_small_writes_flow(0..4, Duration::from_secs(15));
    let (reason, seen) = p.wait_disconnect(Duration::from_secs(30));
    assert!(
        seen.duration_since(started) < Duration::from_secs(15),
        "cut by the lag limit, far earlier than the ack timeout"
    );
    assert_cut_while_blocked(&p, &mut r, base, &reason, &mut alice);
}

/// 送信が塞がっている間のプロトコル違反は、`BAD_REQUEST` を書けないので、通知の有界の待機（約 1 秒）の後に
/// 監視が閉じる。`ack_timeout` を大きくして、切断が違反によるものだと区別できるようにする。
#[test]
fn recover15_protocol_violation_while_write_blocked_is_cut_after_a_bounded_wait() {
    let p = Primary::start(
        "repl-blocked-violation",
        &[
            "--replication-ack-timeout-secs",
            "600",
            "--replication-max-unacked-bytes",
            BIG_WINDOW,
        ],
    );
    let (mut r, base, mut alice) = p.begin_blocked_target();
    p.fill(&mut alice);
    alice.assert_small_writes_flow(0..3, Duration::from_secs(15));
    // 送信済みを超える LSN の ack は不正。
    r.ack(u64::MAX);
    let t1 = Instant::now();
    let (reason, seen) = p.wait_disconnect(Duration::from_secs(30));
    let waited = seen.duration_since(t1);
    assert!(
        waited >= Duration::from_millis(900),
        "waited the bounded notice grace before closing: {waited:?}"
    );
    assert!(
        waited < Duration::from_secs(10),
        "closed after a bounded wait, not at the ack timeout: {waited:?}"
    );
    assert_cut_while_blocked(&p, &mut r, base, &reason, &mut alice);
}
