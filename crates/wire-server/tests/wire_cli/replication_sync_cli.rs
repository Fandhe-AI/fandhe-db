//! 同期レプリケーション（オプション。Issue #2245・親 #2171・RECOVER-15 ポインタ。方式の記録は
//! `docs/design/replication-streaming.md` の「実装判断（Issue #2245）」）をバイナリ子プロセスで外形検証する。
//!
//! 実プライマリ（`--replication-sync-standby`）と実レプリカ（`wire-server replica`）で、成功を返した後に
//! プライマリを失っても commit が同期レプリカに残ること（受け入れ条件 A）、レプリカが無い間は成功が返らないこと、
//! 上限超過時の `reject`（`state=may_be_committed`・データはプライマリに残る）と `degrade`（成功を返し、縮退中は
//! 待たない）を確かめる。確認の段階（`remote_write`／`on`）の違い・同期ロールの 2 本目の接続の拒否・版数 1 の
//! 接続での `R` の拒否は、テスト内の偽レプリカ（フレームを手で組み立てる）で決定的に確かめる。
//! サーバーはポート 0 と一意な一時ディレクトリで起動する。

use crate::common;

use std::net::TcpStream;
use std::path::Path;
use std::time::{Duration, Instant};

use super::replica_cli::{
    count_rows, phc_for, run, s, spawn_replica, write_credentials, Primary as RealPrimary,
    TenantSession as RealSession, ROLE,
};
use super::replication_cli::{
    hello_body_v, start_body, Primary as FakePrimary, Replica, TenantSession, ROLE_PW,
};

const E_BAD_REQUEST: u8 = 1;
const E_BUSY: u8 = 3;

const SYNC_ON: [&str; 4] = [
    "--replication-sync-standby",
    ROLE,
    "--replication-sync-commit",
    "on",
];

fn insert_sql(id: u32) -> String {
    format!("INSERT INTO docs (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]')")
}

/// 拡張クエリプロトコルで INSERT を 1 回実行し、成功（`CommandComplete`）を確かめる。
fn extended_insert(stream: &mut TcpStream, id: u32) {
    let sql = insert_sql(id);
    let (stmt, portal) = (format!("s{id}"), format!("p{id}"));
    common::send_length_prefixed_message(stream, b'P', &common::parse_body(&stmt, &sql, 0));
    common::send_length_prefixed_message(stream, b'B', &common::bind_body(&portal, &stmt));
    common::send_length_prefixed_message(stream, b'E', &common::execute_body(&portal, 0));
    common::send_length_prefixed_message(stream, b'S', &[]);
    let mut types = Vec::new();
    loop {
        let (t, _) = common::read_message(stream);
        types.push(t);
        if t == b'Z' {
            break;
        }
    }
    assert!(!types.contains(&b'E'), "extended insert failed: {types:?}");
    assert!(types.contains(&b'C'), "no CommandComplete: {types:?}");
}

/// 版数 2 の偽レプリカで認証 → Start（適用済み = プライマリの最新）まで進める。拒否されたら `Err(コード)`。
fn begin_v2(r: &mut Replica) -> Result<(), u8> {
    r.send(b'H', &hello_body_v(ROLE, ROLE_PW, 2));
    let (ty, body) = r.recv().expect("identity frame");
    assert_eq!(ty, b'I', "expected identity: {body:?}");
    let timeline = u64::from_be_bytes(body[16..24].try_into().expect("timeline"));
    let current = u64::from_be_bytes(body[24..32].try_into().expect("current"));
    r.send(b'S', &start_body(&body[..16], timeline, current));
    match r.recv().expect("start response") {
        (b'O', _) => Ok(()),
        (b'E', b) => Err(b[0]),
        other => panic!("unexpected frame: {other:?}"),
    }
}

/// 最初のレコードを受けたら `R`（と `ack` が真なら `K`）を返す偽レプリカのスレッドを起こす。
fn drive_fake(mut r: Replica, ack: bool) {
    std::thread::spawn(move || {
        while let Some((lsn, _)) = r.next_record() {
            r.send(b'R', &lsn.to_be_bytes());
            if ack {
                r.ack(lsn);
            }
        }
    });
}

fn error_detail_is_may_be_committed(session: &mut TenantSession, sql: &str) {
    common::send_simple_query(&mut session.0, sql);
    common::expect_error_response_with_sqlstate_and_message(
        &mut session.0,
        "XX000",
        "state=may_be_committed",
    );
    common::read_ready_for_query(&mut session.0);
}

/// 受け入れ条件 A: 同期（`on`）で成功を返した commit は、直後にプライマリを失っても同期レプリカに残る。
/// レプリカが居ない間は成功が返らず、レプリカが起動すると返る（上限なし）。
#[test]
fn recover15_sync_commit_survives_primary_loss() {
    let mut primary = RealPrimary::start_with("sync-accept", &SYNC_ON);
    // ベースは何も書く前に取る（以降の commit はレプリカの確認を待つ）。
    let base = primary.dir.join("base.redb");
    let o = run(&["backup", "--socket", s(&primary.sock), "--out", s(&base)]);
    assert_eq!(o.status.code(), Some(0), "{o:?}");
    let rdir = primary.dir.join("replica");
    std::fs::create_dir(&rdir).expect("replica dir");
    let rdb = rdir.join("replica.redb");
    std::fs::copy(&base, &rdb).expect("copy base");
    let cred = write_credentials(&rdir, 0o600);

    // レプリカが居ない間、commit の応答は返らない。
    let mut session = primary.session();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        session.exec("CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))");
        done_tx.send(()).expect("send");
        session
    });
    assert!(
        done_rx.recv_timeout(Duration::from_millis(1500)).is_err(),
        "no success may be returned before the replica confirms"
    );
    // レプリカを起動すると応答が返る。
    let (mut replica, _lines) = spawn_replica(&rdb, primary.repl, &cred, &[]);
    done_rx
        .recv_timeout(Duration::from_secs(60))
        .expect("the commit is confirmed once the replica runs");
    let mut session: RealSession = handle.join().expect("join");

    // 簡易クエリ・明示トランザクション・拡張クエリの各経路。
    session.insert(1);
    session.insert(2);
    session.exec("BEGIN");
    session.insert(3);
    session.exec("COMMIT");
    extended_insert(&mut session.0, 4);
    // 複数文のメッセージ（暗黙トランザクションの 1 commit）。
    common::send_simple_query(
        &mut session.0,
        &format!("{}; {}", insert_sql(5), insert_sql(6)),
    );
    let _ = common::read_command_complete(&mut session.0);
    let _ = common::read_command_complete(&mut session.0);
    common::read_ready_for_query(&mut session.0);

    // 最後の成功を受けた直後にプライマリを失う。
    let _ = primary._child.0.kill();
    let _ = primary._child.0.wait();
    let _ = replica.0.kill();
    let _ = replica.0.wait();
    assert_eq!(count_rows(&rdb, "SELECT id FROM docs LIMIT 1000"), 6);
}

/// 上限超過の `reject`: レプリカが無いと `state=may_be_committed` のエラーになり、データはプライマリに
/// 残る（同じ行の再送は一意制約違反。二重実行にならない）。
#[test]
fn recover15_sync_reject_after_timeout_keeps_the_commit_on_the_primary() {
    let p = FakePrimary::start(
        "sync-reject",
        &[
            "--replication-sync-standby",
            ROLE,
            "--replication-sync-timeout-secs",
            "1",
            "--replication-sync-on-timeout",
            "reject",
        ],
        false,
    );
    let mut t = p.alice();
    let started = Instant::now();
    error_detail_is_may_be_committed(
        &mut t,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))",
    );
    assert!(started.elapsed() >= Duration::from_millis(900));
    error_detail_is_may_be_committed(&mut t, &insert_sql(1));
    // commit はプライマリで確定している。
    common::send_simple_query(&mut t.0, "SELECT id FROM docs LIMIT 10");
    let mut rows = 0;
    loop {
        let (ty, _) = common::read_message(&mut t.0);
        match ty {
            b'D' => rows += 1,
            b'Z' => break,
            b'E' => panic!("select failed"),
            _ => {}
        }
    }
    assert_eq!(rows, 1, "the rejected commit is visible on the primary");
    common::send_simple_query(&mut t.0, &insert_sql(1));
    common::expect_error_response_with_sqlstate(&mut t.0, "23505");
}

/// `degrade`: 上限の後に成功を返し、縮退中の commit は待たずに通る。
#[test]
fn recover15_sync_degrade_passes_after_timeout_and_then_does_not_wait() {
    let p = FakePrimary::start(
        "sync-degrade",
        &[
            "--replication-sync-standby",
            ROLE,
            "--replication-sync-timeout-secs",
            "1",
            "--replication-sync-on-timeout",
            "degrade",
        ],
        false,
    );
    let mut t = p.alice();
    let started = Instant::now();
    t.exec("CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))");
    assert!(started.elapsed() >= Duration::from_millis(900), "waited");
    let next = Instant::now();
    t.insert(1);
    assert!(
        next.elapsed() < Duration::from_millis(500),
        "degraded commits do not wait"
    );
}

/// 確認の段階: `R` だけでは `on` は満たされず、`remote_write` は満たされる。`K` は `on` も満たす。
#[test]
fn recover15_sync_stage_decides_which_report_confirms() {
    let table = "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))";
    let timeout = [
        "--replication-sync-timeout-secs",
        "2",
        "--replication-sync-on-timeout",
        "reject",
    ];
    // `on` + `R` のみ: 確認されない。
    let mut args = SYNC_ON.to_vec();
    args.extend(timeout);
    let p = FakePrimary::start("sync-stage-on-r", &args, false);
    let mut fake = Replica::connect(p.repl);
    begin_v2(&mut fake).expect("begin");
    drive_fake(fake, false);
    let mut t = p.alice();
    error_detail_is_may_be_committed(&mut t, table);
    drop(p);

    // `remote_write` + `R` のみ: 確認される。
    let mut args = vec![
        "--replication-sync-standby",
        ROLE,
        "--replication-sync-commit",
        "remote_write",
    ];
    args.extend(timeout);
    let p = FakePrimary::start("sync-stage-rw", &args, false);
    let mut fake = Replica::connect(p.repl);
    begin_v2(&mut fake).expect("begin");
    drive_fake(fake, false);
    p.alice().exec(table);
    drop(p);

    // `on` + `K`: 確認される。
    let mut args = SYNC_ON.to_vec();
    args.extend(timeout);
    let p = FakePrimary::start("sync-stage-on-k", &args, false);
    let mut fake = Replica::connect(p.repl);
    begin_v2(&mut fake).expect("begin");
    drive_fake(fake, true);
    p.alice().exec(table);
}

/// 同期ロールの 2 本目の接続は `BUSY`（ロールとレプリカを 1 対 1 に保つ）。1 本目が切れれば再接続できる。
#[test]
fn recover15_sync_standby_allows_one_connection_per_role() {
    let p = FakePrimary::start("sync-busy", &SYNC_ON, false);
    let mut first = Replica::connect(p.repl);
    begin_v2(&mut first).expect("first");
    let mut second = Replica::connect(p.repl);
    assert_eq!(begin_v2(&mut second), Err(E_BUSY));
    drop(first);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut again = Replica::connect(p.repl);
        match begin_v2(&mut again) {
            Ok(()) => break,
            Err(E_BUSY) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(c) => panic!("reconnect failed: {c}"),
        }
    }
}

/// `R` は版数 2 の接続だけが送れる。版数 1 の `R`・巻き戻る `R`・送信済みを超える `R` は `BAD_REQUEST`。
#[test]
fn recover15_received_frame_is_validated() {
    let p = FakePrimary::start("sync-r-frame", &[], false);
    let mut alice = p.alice();
    alice.exec("CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))");

    // 版数 1 の `R`。
    let mut v1 = Replica::connect(p.repl);
    v1.begin(0);
    let (lsn, _) = v1.next_record().expect("record");
    v1.send(b'R', &lsn.to_be_bytes());
    assert_eq!(v1.drain_until_closed(), Some(E_BAD_REQUEST));

    // 版数 2: 送信済みを超える `R`。
    let mut v2 = Replica::connect(p.repl);
    v2.send(b'H', &hello_body_v(ROLE, ROLE_PW, 2));
    let (_, body) = v2.recv().expect("identity");
    v2.send(
        b'S',
        &start_body(
            &body[..16],
            u64::from_be_bytes(body[16..24].try_into().expect("timeline")),
            0,
        ),
    );
    assert_eq!(v2.recv().expect("ok").0, b'O');
    let (lsn, _) = v2.next_record().expect("record");
    v2.send(b'R', &(lsn + 1000).to_be_bytes());
    assert_eq!(v2.drain_until_closed(), Some(E_BAD_REQUEST));

    // 版数 2: 巻き戻る `R`。
    let mut v2b = Replica::connect(p.repl);
    v2b.send(b'H', &hello_body_v(ROLE, ROLE_PW, 2));
    let (_, body) = v2b.recv().expect("identity");
    v2b.send(
        b'S',
        &start_body(
            &body[..16],
            u64::from_be_bytes(body[16..24].try_into().expect("timeline")),
            0,
        ),
    );
    assert_eq!(v2b.recv().expect("ok").0, b'O');
    let (first, _) = v2b.next_record().expect("record");
    alice.insert(1);
    let (second, _) = v2b.next_record().expect("record");
    v2b.send(b'R', &second.to_be_bytes());
    v2b.send(b'R', &first.to_be_bytes());
    assert_eq!(v2b.drain_until_closed(), Some(E_BAD_REQUEST));
}

/// 同期フラグの不正な組み合わせは、DB を作る前に固定文言で拒否される。
#[test]
fn recover15_sync_flags_are_validated_before_the_database_is_opened() {
    let fx = common::TempFixtureDir::new("sync-flags");
    common::write_empty_user_store(&fx.users_path_str());
    let dir = Path::new(&fx.db_path_str())
        .parent()
        .expect("p")
        .to_path_buf();
    let roles = dir.join("roles.txt");
    std::fs::write(&roles, format!("{ROLE}:{}\n", phc_for(ROLE_PW))).expect("roles");
    let roles_s = roles.to_str().expect("utf-8").to_string();
    let with_base = |extra: &[&str]| {
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
            "127.0.0.1:0",
            "--replication-roles",
            &roles_s,
            "--replication-allow",
            "127.0.0.1",
        ];
        args.extend_from_slice(extra);
        run(&args)
    };
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec!["--replication-sync-standby", "nobody"],
            "must name a role defined in --replication-roles",
        ),
        (
            vec!["--replication-sync-commit", "on"],
            "require --replication-sync-standby",
        ),
        (
            vec![
                "--replication-sync-standby",
                ROLE,
                "--replication-sync-commit",
                "local",
            ],
            "must be remote_write, on or remote_apply",
        ),
        (
            vec![
                "--replication-sync-standby",
                ROLE,
                "--replication-sync-timeout-secs",
                "5",
            ],
            "requires --replication-sync-on-timeout",
        ),
        (
            vec![
                "--replication-sync-standby",
                ROLE,
                "--replication-sync-on-timeout",
                "reject",
            ],
            "requires --replication-sync-timeout-secs",
        ),
        (
            vec![
                "--replication-sync-standby",
                ROLE,
                "--replication-sync-timeout-secs",
                "0",
                "--replication-sync-on-timeout",
                "reject",
            ],
            "is out of range",
        ),
        (
            vec![
                "--replication-sync-standby",
                ROLE,
                "--replication-sync-timeout-secs",
                "5",
                "--replication-sync-on-timeout",
                "ignore",
            ],
            "must be degrade or reject",
        ),
        (
            vec![
                "--replication-sync-standby",
                ROLE,
                "--replication-sync-standby",
                ROLE,
            ],
            "repeats a role",
        ),
        (
            vec![
                "--replication-sync-standby",
                ROLE,
                "--replication-sync-commit",
                "on",
                "--replication-sync-commit",
                "on",
            ],
            "specified more than once",
        ),
    ];
    for (extra, expect) in cases {
        let out = with_base(&extra);
        assert_ne!(out.status.code(), Some(0), "{extra:?}");
        let err = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(err.contains(expect), "{extra:?}: {err}");
        assert!(!err.contains("listening on"), "{err}");
    }
    // bind なしの同期フラグ。
    let users = fx.users_path_str();
    let db = fx.db_path_str();
    let out = run(&[
        "--users",
        &users,
        "--db",
        &db,
        "--bind",
        "127.0.0.1:0",
        "--change-log",
        "--replication-sync-standby",
        ROLE,
    ]);
    assert_ne!(out.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&out.stderr).contains("require --replication-bind"));
    assert!(
        !Path::new(&fx.db_path_str()).exists(),
        "every refusal happens before the DB file is created"
    );
}
