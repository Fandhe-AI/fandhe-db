//! 変更ログのアーカイブ・保持の CLI（Issue #2167・親 #2164・RECOVER-14 ポインタ）をバイナリ子プロセスで外形検証する。
//! フラグの組み合わせの不備・不正値は DB を開く前に固定文言で拒否し、指定した起動は常駐スレッドが
//! セグメントを書き出し、`change-log-verify` サブコマンドがアーカイブの欠落・改ざんを非 0 終了で知らせる。
//! サーバーを起動するテストはポート 0 と一意な一時ディレクトリを使う。

use crate::common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_wire-server");

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn write_user_store_with_alice(path: &str) {
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
        .write_all(b"pw-alice\n")
        .expect("write password");
    let output = child.wait_with_output().expect("wait hash-password");
    assert!(output.status.success());
    let phc = String::from_utf8(output.stdout).expect("utf-8 phc");
    std::fs::write(path, format!("alice:tenant-a:{}\n", phc.trim())).expect("write user store");
}

fn spawn_server(users: &str, db: &str, extra: &[&str]) -> ChildGuard {
    ChildGuard(
        Command::new(BIN)
            .args(["--users", users, "--db", db, "--bind", "127.0.0.1:0"])
            .args(["--ddl-allowed-users", "alice"])
            .args(extra)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server"),
    )
}

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("run wire-server")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn segments(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read archive dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "seg"))
        .collect();
    v.sort();
    v
}

fn insert(stream: &mut std::net::TcpStream, id: u32) {
    common::send_simple_query(
        stream,
        &format!("INSERT INTO docs (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]')"),
    );
    assert_eq!(common::read_command_complete(stream), "INSERT 0 1");
    common::read_ready_for_query(stream);
}

/// 期限つきで `cond` が真になるのを待つ。
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

/// フラグの組み合わせの不備と不正値は、DB を作る前に固定文言で拒否される。
#[test]
fn recover14_archive_flags_are_validated_before_the_database_is_opened() {
    let fx = common::TempFixtureDir::new("cl-arc-flags");
    common::write_empty_user_store(&fx.users_path_str());
    let arc = fx.db_path_str() + ".arc";
    std::fs::create_dir(&arc).expect("archive dir");
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
            vec!["--change-log-archive-dir", &arc],
            "require --change-log",
        ),
        // 保持・周期はアーカイブ先が前提（アーカイブされていない行を刈り込む設定を作らない）。
        (
            vec!["--change-log", "--change-log-retain-records", "5"],
            "require --change-log-archive-dir",
        ),
        (
            vec!["--change-log", "--change-log-archive-interval-secs", "5"],
            "require --change-log-archive-dir",
        ),
        // 重複指定。
        (
            vec![
                "--change-log",
                "--change-log-archive-dir",
                &arc,
                "--change-log-archive-dir",
                &arc,
            ],
            "specified more than once",
        ),
        (
            vec![
                "--change-log",
                "--change-log-archive-dir",
                &arc,
                "--change-log-retain-records",
                "1",
                "--change-log-retain-records",
                "2",
            ],
            "specified more than once",
        ),
        // 不正値。
        (
            vec![
                "--change-log",
                "--change-log-archive-dir",
                "/nonexistent-archive-dir",
            ],
            "must be an existing directory",
        ),
        (
            vec![
                "--change-log",
                "--change-log-archive-dir",
                &arc,
                "--change-log-retain-records",
                "-1",
            ],
            "must be a non-negative integer",
        ),
        (
            vec![
                "--change-log",
                "--change-log-archive-dir",
                &arc,
                "--change-log-archive-interval-secs",
                "0",
            ],
            "must be between",
        ),
        (
            vec!["--change-log", "--change-log-archive-dir"],
            "requires a value",
        ),
    ];
    for (extra, expect) in cases {
        let out = base(&extra);
        assert_ne!(out.status.code(), Some(0), "{extra:?}");
        let err = stderr(&out);
        assert!(err.contains(expect), "{extra:?}: {err}");
        assert!(!err.contains("listening on"), "{err}");
        assert!(
            !err.contains("/nonexistent-archive-dir"),
            "the path must not be echoed: {err}"
        );
    }
    assert!(
        !Path::new(&fx.db_path_str()).exists(),
        "every refusal happens before the DB file is created"
    );
}

/// 常駐スレッドがセグメントを書き出し、保持設定で水位以下が刈り込まれ、検証サブコマンドが成功する。
/// セグメントの紛失・改ざんは非 0 終了で検出され、刈り込み後の再起動も連続性検査に拒否されない。
#[test]
fn recover14_archive_thread_writes_segments_and_verify_detects_loss_and_tampering() {
    let fx = common::TempFixtureDir::new("cl-arc-e2e");
    let users = fx.users_path_str();
    let db = fx.db_path_str();
    write_user_store_with_alice(&users);
    let arc = PathBuf::from(db.clone() + ".arc");
    std::fs::create_dir(&arc).expect("archive dir");
    let arc_s = arc.to_str().expect("utf-8").to_string();
    let flags = [
        "--change-log",
        "--change-log-archive-dir",
        &arc_s,
        "--change-log-archive-interval-secs",
        "1",
        "--change-log-retain-records",
        "0",
    ];

    {
        let mut server = spawn_server(&users, &db, &flags);
        let (addr, _lines) =
            common::wait_for_listening_addr_and_lines(&mut server.0, Duration::from_secs(10));
        let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
        common::send_simple_query(
            &mut alice,
            "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))",
        );
        let _ = common::read_command_complete(&mut alice);
        common::read_ready_for_query(&mut alice);
        for id in 1..=3 {
            insert(&mut alice, id);
        }
        wait_until("a segment to be archived", Duration::from_secs(20), || {
            !segments(&arc).is_empty()
        });
        // 書き込みを続けても連鎖は保たれ、後続のセグメントが増える。
        insert(&mut alice, 4);
        wait_until("a second archive pass", Duration::from_secs(20), || {
            segments(&arc).len() >= 2
        });
    }

    // 停止後に検証する（DB の水位とも突き合わせる）。
    let verify_db = run(&["change-log-verify", "--archive-dir", &arc_s, "--db", &db]);
    assert_eq!(verify_db.status.code(), Some(0), "{}", stderr(&verify_db));
    let stdout = String::from_utf8_lossy(&verify_db.stdout).into_owned();
    assert!(stdout.contains("change log archive ok"), "{stdout}");
    let verify_dir = run(&["change-log-verify", "--archive-dir", &arc_s]);
    assert_eq!(verify_dir.status.code(), Some(0), "{}", stderr(&verify_dir));

    // 刈り込み後の再起動は連続性検査に拒否されない（水位が最後の LSN を保つ）。
    {
        let mut server = spawn_server(&users, &db, &flags);
        assert!(
            common::wait_for_listening(&mut server.0, Duration::from_secs(10)),
            "restart after pruning"
        );
    }

    let files = segments(&arc);
    assert!(files.len() >= 2, "{files:?}");

    // 改ざん: 1 バイト反転するとチェックサム不一致で非 0 終了。
    let victim = files.last().expect("segment").clone();
    let original = std::fs::read(&victim).expect("read");
    let mut tampered = original.clone();
    let mid = tampered.len() / 2;
    tampered[mid] ^= 0xff;
    std::fs::write(&victim, &tampered).expect("write");
    let out = run(&["change-log-verify", "--archive-dir", &arc_s]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("checksum mismatch"),
        "{}",
        stderr(&out)
    );
    std::fs::write(&victim, &original).expect("restore");

    // 中間のセグメントの紛失は LSN の飛びとして検出される。
    let middle = files.first().expect("segment").clone();
    let saved = std::fs::read(&middle).expect("read");
    std::fs::remove_file(&middle).expect("remove");
    let out = run(&["change-log-verify", "--archive-dir", &arc_s, "--db", &db]);
    assert_eq!(out.status.code(), Some(1), "{}", stderr(&out));
    assert!(stderr(&out).contains("gap"), "{}", stderr(&out));
    std::fs::write(&middle, saved).expect("restore");
    let ok = run(&["change-log-verify", "--archive-dir", &arc_s, "--db", &db]);
    assert_eq!(ok.status.code(), Some(0), "{}", stderr(&ok));
}

/// 検証サブコマンドの引数の不備は終了コード 2（DB は開かない）。
#[test]
fn recover14_verify_subcommand_rejects_bad_arguments() {
    for args in [
        vec!["change-log-verify"],
        vec!["change-log-verify", "--archive-dir"],
        vec![
            "change-log-verify",
            "--archive-dir",
            "/nonexistent-dir-for-verify",
        ],
        vec!["change-log-verify", "--bogus", "x"],
    ] {
        let out = run(&args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        assert!(
            !stderr(&out).contains("/nonexistent-dir-for-verify"),
            "the path must not be echoed"
        );
    }
}
