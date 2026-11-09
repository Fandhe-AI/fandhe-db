//! `wire-server replica-promote`（レプリカの昇格。Issue #3132・親 #2171・RECOVER-15 (4)(5) ポインタ。方式の記録は
//! `docs/design/replication-streaming.md` の「実装判断（Issue #3132）」）をバイナリ子プロセスで外形検証する。
//!
//! 実プライマリに追従させたレプリカを止めて昇格し、報告される適用済みの commit 番号がレプリカの適用位置と
//! 一致すること、昇格した DB を `--change-log` 付きで起動すると書き込みを受理すること、旧プライマリへの
//! 再接続が別系統として拒否されること、読み取りレプリカからの昇格、稼働中・レプリカのベースでない DB・
//! 不正な引数が DB を変えずに拒否されることを固定する。サーバーはポート 0 と一意な一時ディレクトリで起動する。

use super::replica_cli::{
    count_rows, phc_for, query_outcome, run, s, spawn_replica, write_credentials, ChildGuard,
    Primary,
};
use crate::common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use engine::storage::{chain_point_of, Storage};

const BIN: &str = env!("CARGO_BIN_EXE_wire-server");
const EXIT_FAILURE: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_PROMOTED_OUTPUT_FAILED: i32 = 6;
const EXIT_NOT_REPLICA_BASE: i32 = 10;
const EXIT_PRIMARY_REJECTED: i32 = 11;

/// プライマリへ追従済みのレプリカ DB を作る前提（ベース取得）。`rows` 行を書いたベースを返す。
struct Setup {
    primary: Primary,
    rdir: PathBuf,
    rdb: PathBuf,
    cred: PathBuf,
    base_lsn: u64,
}

fn setup(label: &str, rows: u32) -> Setup {
    let primary = Primary::start(label);
    let mut t = primary.session();
    t.exec("CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))");
    for id in 1..=rows {
        t.insert(id);
    }
    let base = primary.dir.join("base.redb");
    let o = run(&["backup", "--socket", s(&primary.sock), "--out", s(&base)]);
    assert_eq!(o.status.code(), Some(0), "{:?}", o);
    let base_lsn = chain_point_of(&base).expect("chain point").end_lsn;
    let rdir = primary.dir.join("replica");
    std::fs::create_dir(&rdir).expect("replica dir");
    // 異常終了（kill）したレプリカの検査は一時コピーを使うため、親は他者が書けない権限にする。
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&rdir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    }
    let rdb = rdir.join("replica.redb");
    std::fs::copy(&base, &rdb).expect("copy base");
    let cred = write_credentials(&rdir, 0o600);
    Setup {
        primary,
        rdir,
        rdb,
        cred,
        base_lsn,
    }
}

/// 昇格した DB を `--change-log` 付きの通常起動（プライマリ）で立ち上げ、テナントのセッションを返す。
fn start_as_primary(rdir: &Path, rdb: &Path) -> (ChildGuard, std::net::TcpStream) {
    let users = rdir.join("users-promoted.txt");
    std::fs::write(&users, format!("alice:tenant-a:{}\n", phc_for("pw-alice"))).expect("users");
    let mut child = Command::new(BIN)
        .args(["--users", s(&users), "--db", s(rdb)])
        .args(["--bind", "127.0.0.1:0", "--ddl-allowed-users", "alice"])
        .arg("--change-log")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn promoted primary");
    let (addr, _lines): (SocketAddr, _) =
        common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(15));
    let guard = ChildGuard(child);
    let stream = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("timeout");
    (guard, stream)
}

fn stop(guard: &mut ChildGuard) {
    let _ = guard.0.kill();
    let _ = guard.0.wait();
}

fn stdout_of(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr_of(o: &std::process::Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// 追従したレプリカを昇格する。報告された適用済みの commit 番号がレプリカの適用位置と一致し、昇格した DB を
/// プライマリとして起動すると書き込みが通り昇格前の行がすべて読め、旧プライマリへの再接続は別系統として拒否される。
#[test]
fn recover15_promoted_replica_accepts_writes_and_reports_the_applied_lsn() {
    let mut st = setup("promote-main", 3);
    let mut t = st.primary.session();
    for id in 4..=6 {
        t.insert(id);
    }
    let (mut replica, lines) = spawn_replica(&st.rdb, st.primary.repl, &st.cred, &[]);
    lines.wait_applied(st.base_lsn + 3);
    stop(&mut replica);
    let base_point = chain_point_of(&st.primary.dir.join("base.redb")).expect("base point");
    let applied_lsn = st.base_lsn + 3;

    let o = run(&["replica-promote", "--db", s(&st.rdb)]);
    assert_eq!(o.status.code(), Some(0), "{:?}", o);
    let out = stdout_of(&o);
    assert!(
        out.contains(&format!(
            "replica-promote done: applied_lsn={}",
            applied_lsn
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!("commits after lsn={}", applied_lsn)),
        "{out}"
    );
    let promoted = chain_point_of(&st.rdb).expect("promoted point");
    assert_eq!(promoted.end_lsn, applied_lsn);
    assert_eq!(promoted.db_id, base_point.db_id);
    assert!(
        promoted.timeline > base_point.timeline,
        "the timeline forks"
    );
    assert_eq!(count_rows(&st.rdb, "SELECT id FROM docs LIMIT 1000"), 6);

    // 旧プライマリ（稼働中）への再接続は別系統として拒否される（取り違えの防止）。
    let o = run(&[
        "replica",
        "--db",
        s(&st.rdb),
        "--primary",
        &st.primary.repl.to_string(),
        "--credentials",
        s(&st.cred),
    ]);
    assert_eq!(o.status.code(), Some(EXIT_PRIMARY_REJECTED), "{:?}", o);
    assert!(!stderr_of(&o).contains("pw-replica"));

    // 昇格した DB をプライマリとして起動すると書き込みが通る。
    let (mut guard, mut conn) = start_as_primary(&st.rdir, &st.rdb);
    assert_eq!(
        query_outcome(&mut conn, "SELECT id FROM docs LIMIT 1000"),
        Ok(6)
    );
    assert_eq!(
        query_outcome(
            &mut conn,
            "INSERT INTO docs (id, embedding) VALUES (100, '[0.1,0.2,0.3]')"
        ),
        Ok(0)
    );
    assert_eq!(
        query_outcome(&mut conn, "SELECT id FROM docs LIMIT 1000"),
        Ok(7)
    );
    stop(&mut guard);
    stop(&mut st.primary._child);
}

/// 読み取りレプリカ（`--replica-primary`）は書き込みを `25006` で拒否する。止めて昇格し、プライマリとして起動すると
/// 書き込みが通る。
#[test]
fn recover15_read_replica_can_be_promoted() {
    let mut st = setup("promote-read-replica", 3);
    let users = st.rdir.join("users.txt");
    std::fs::write(&users, format!("alice:tenant-a:{}\n", phc_for("pw-alice"))).expect("users");
    let mut child = Command::new(BIN)
        .args(["--users", s(&users), "--db", s(&st.rdb)])
        .args(["--bind", "127.0.0.1:0"])
        .args(["--replica-primary", &st.primary.repl.to_string()])
        .args(["--replica-credentials", s(&st.cred)])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn read replica");
    let (addr, _lines) =
        common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(15));
    let mut guard = ChildGuard(child);
    let mut r = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    r.set_read_timeout(Some(Duration::from_secs(20)))
        .expect("timeout");
    let deadline = Instant::now() + Duration::from_secs(60);
    while query_outcome(&mut r, "SELECT id FROM docs LIMIT 1000") != Ok(3) {
        assert!(Instant::now() < deadline, "replica did not catch up");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(
        query_outcome(
            &mut r,
            "INSERT INTO docs (id, embedding) VALUES (100, '[0.1,0.2,0.3]')"
        ),
        Err("25006".to_string())
    );
    // 稼働中の昇格は拒否される。
    let o = run(&["replica-promote", "--db", s(&st.rdb)]);
    assert_eq!(o.status.code(), Some(EXIT_FAILURE), "{:?}", o);
    stop(&mut guard);

    let o = run(&["replica-promote", "--db", s(&st.rdb)]);
    assert_eq!(o.status.code(), Some(0), "{:?}", o);
    let (mut promoted, mut conn) = start_as_primary(&st.rdir, &st.rdb);
    assert_eq!(
        query_outcome(
            &mut conn,
            "INSERT INTO docs (id, embedding) VALUES (100, '[0.1,0.2,0.3]')"
        ),
        Ok(0)
    );
    stop(&mut promoted);
    stop(&mut st.primary._child);
}

/// 追従中のレプリカがあるあいだの昇格は、DB を変えずに実行時の失敗（1）で拒否され、レプリカは追従を続ける。
#[test]
fn recover15_promote_is_refused_while_the_replica_runs() {
    let mut st = setup("promote-running", 3);
    let (mut replica, lines) = spawn_replica(&st.rdb, st.primary.repl, &st.cred, &[]);
    lines.wait_applied(st.base_lsn);
    let o = run(&["replica-promote", "--db", s(&st.rdb)]);
    assert_eq!(o.status.code(), Some(EXIT_FAILURE), "{:?}", o);
    assert!(stdout_of(&o).is_empty());
    assert!(stderr_of(&o).contains("stop the replica first"), "{:?}", o);
    // 追従は続く。
    let mut t = st.primary.session();
    t.insert(50);
    lines.wait_applied(st.base_lsn + 1);
    stop(&mut replica);
    stop(&mut st.primary._child);
}

/// 変更ログを一度も有効にしていない DB は、レプリカのベースではないので終了コード 10 で拒否し、DB を変えない。
#[test]
fn recover15_promote_refuses_a_database_that_is_not_a_replica_base() {
    let fx = common::TempFixtureDir::new("promote-plain");
    let db = PathBuf::from(fx.db_path_str());
    drop(Storage::open(&db).expect("create plain db"));
    let o = run(&["replica-promote", "--db", s(&db)]);
    assert_eq!(o.status.code(), Some(EXIT_NOT_REPLICA_BASE), "{:?}", o);
    assert!(stdout_of(&o).is_empty());
    // 変更ログのメタを作っていない（DB を「変更ログ必須」に変えていない）。
    assert!(matches!(
        chain_point_of(&db),
        Err(engine::storage::IncrementalError::NotEnabled)
    ));
    let err = stderr_of(&o);
    assert!(!err.contains(&fx.db_path_str()), "path leaked: {err}");
}

/// 引数の不備（`--db` なし・重複・未知のフラグ・存在しないパス）は DB を開かず終了コード 2。
#[test]
fn recover15_promote_rejects_bad_arguments() {
    let fx = common::TempFixtureDir::new("promote-args");
    let db = PathBuf::from(fx.db_path_str());
    std::fs::write(&db, b"not a real database").expect("db");
    let absent = db.with_file_name("absent.redb");
    let cases: Vec<Vec<&str>> = vec![
        vec!["replica-promote"],
        vec!["replica-promote", "--db"],
        vec!["replica-promote", "--db", s(&db), "--db", s(&db)],
        vec!["replica-promote", "--db", s(&db), "--bogus", "x"],
        vec!["replica-promote", "--db", s(&absent)],
    ];
    for args in cases {
        let o = run(&args);
        assert_eq!(o.status.code(), Some(EXIT_USAGE), "{args:?}: {:?}", o);
        assert!(stdout_of(&o).is_empty());
    }
    assert!(!absent.exists(), "a missing database must not be created");
    assert_eq!(
        std::fs::read(&db).expect("read"),
        b"not a real database".to_vec()
    );
}

/// 昇格は確定したが標準出力へ書けないとき（`/dev/full`）は、「DB は変わらない」終了コード 1 ではなく専用の 6 を返し、
/// DB は昇格済みで、標準エラーが再実行しないよう知らせる。
#[cfg(target_os = "linux")]
#[test]
fn recover15_promote_output_failure_after_promotion_is_not_exit_1() {
    let mut st = setup("promote-outfail", 3);
    let (mut replica, lines) = spawn_replica(&st.rdb, st.primary.repl, &st.cred, &[]);
    lines.wait_applied(st.base_lsn);
    stop(&mut replica);
    let base_point = chain_point_of(&st.primary.dir.join("base.redb")).expect("base point");
    let full = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .expect("open /dev/full");
    let o = Command::new(BIN)
        .args(["replica-promote", "--db", s(&st.rdb)])
        .stdout(Stdio::from(full))
        .stderr(Stdio::piped())
        .output()
        .expect("run");
    assert_eq!(
        o.status.code(),
        Some(EXIT_PROMOTED_OUTPUT_FAILED),
        "{:?}",
        o
    );
    assert_ne!(o.status.code(), Some(EXIT_FAILURE));
    let err = stderr_of(&o);
    assert!(err.contains("WAS promoted"), "{err}");
    assert!(err.contains("do not run replica-promote again"), "{err}");
    let promoted = chain_point_of(&st.rdb).expect("promoted point");
    assert!(promoted.timeline > base_point.timeline);
    stop(&mut st.primary._child);
}
