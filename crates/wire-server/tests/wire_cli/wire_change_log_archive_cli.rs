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

/// アーカイブ先を作る。他者が書けるディレクトリは起動を拒否されるため、umask に依らず 0755 にする。
fn make_archive_dir(path: impl AsRef<Path>) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir(path.as_ref()).expect("archive dir");
    std::fs::set_permissions(path.as_ref(), std::fs::Permissions::from_mode(0o755))
        .expect("chmod archive dir");
}

/// SIGKILL で止めたサーバーの DB は、読み取り専用 open が書き込み（修復）を要すると拒否する。テストでは
/// 書き込み可能に開いて閉じ、修復を済ませてから `--db` 付きの検証を呼ぶ。
fn repair(db: &str) {
    drop(engine::storage::Storage::open(db).expect("open to repair"));
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
    make_archive_dir(&arc);
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
    make_archive_dir(&arc);
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
    repair(&db);
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

    repair(&db);
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

/// 他者が書き込めるアーカイブ先は、一時名・セグメントの差し替えを許すため起動前に拒否される。
#[test]
fn recover14_archive_dir_writable_by_others_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let fx = common::TempFixtureDir::new("cl-arc-shared");
    common::write_empty_user_store(&fx.users_path_str());
    let arc = fx.db_path_str() + ".arc";
    std::fs::create_dir(&arc).expect("archive dir");
    std::fs::set_permissions(&arc, std::fs::Permissions::from_mode(0o777)).expect("chmod");
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
        "--change-log-archive-dir",
        &arc,
    ]);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        stderr(&out).contains("writable by group or other users"),
        "{}",
        stderr(&out)
    );
    assert!(!Path::new(&db).exists(), "refused before the DB is created");
}
/// 未アーカイブ量の上限のフラグ（Issue #3091・RECOVER-14 (2) ポインタ）の不備は、DB を作る前に固定文言で
/// 拒否され、文言に値・パスを含まない。
#[test]
fn recover14_unarchived_limit_flags_are_validated_before_the_database_is_opened() {
    let fx = common::TempFixtureDir::new("cl-lim-flags");
    common::write_empty_user_store(&fx.users_path_str());
    let arc = fx.db_path_str() + ".arc";
    make_archive_dir(&arc);
    let base = |extra: &[&str]| -> Output {
        let users = fx.users_path_str();
        let db = fx.db_path_str();
        let mut args = vec!["--users", &users, "--db", &db, "--bind", "127.0.0.1:0"];
        args.extend_from_slice(extra);
        run(&args)
    };
    let bytes = "--change-log-max-unarchived-bytes";
    let records = "--change-log-max-unarchived-records";
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (vec![bytes, "100"], "require --change-log"),
        (
            vec!["--change-log", bytes, "100"],
            "require --change-log-archive-dir",
        ),
        (
            vec!["--change-log", "--change-log-archive-dir", &arc, bytes, "0"],
            "must be at least 1",
        ),
        (
            vec![
                "--change-log",
                "--change-log-archive-dir",
                &arc,
                records,
                "0",
            ],
            "must be at least 1",
        ),
        (
            vec![
                "--change-log",
                "--change-log-archive-dir",
                &arc,
                records,
                "12x",
            ],
            "must be a non-negative integer",
        ),
        (
            vec![
                "--change-log",
                "--change-log-archive-dir",
                &arc,
                bytes,
                "5",
                bytes,
                "6",
            ],
            "specified more than once",
        ),
        (
            vec!["--change-log", "--change-log-archive-dir", &arc, records],
            "requires a value",
        ),
    ];
    for (extra, expect) in cases {
        let out = base(&extra);
        assert_ne!(out.status.code(), Some(0), "{extra:?}");
        let err = stderr(&out);
        assert!(err.contains(expect), "{extra:?}: {err}");
        assert!(!err.contains("listening on"), "{err}");
        assert!(!err.contains("12x") && !err.contains(&arc), "{err}");
    }
    assert!(
        !Path::new(&fx.db_path_str()).exists(),
        "every refusal happens before the DB file is created"
    );
}

/// 上限に達すると書き込みは `54000` で拒否され、アーカイブが進めば再開する。周期を最大にして、起動直後の
/// 1 周期だけが走るようにする（時間計測に依存しない）。止めて同じ DB で再起動すると、起動直後の周期が
/// アーカイブして挿入が通る。
#[test]
fn recover14_unarchived_limit_refuses_writes_with_54000_and_resumes_after_restart() {
    let fx = common::TempFixtureDir::new("cl-lim-e2e");
    let users = fx.users_path_str();
    let db = fx.db_path_str();
    write_user_store_with_alice(&users);
    let arc = PathBuf::from(db.clone() + ".arc");
    make_archive_dir(&arc);
    let arc_s = arc.to_str().expect("utf-8").to_string();
    let flags = [
        "--change-log",
        "--change-log-archive-dir",
        &arc_s,
        "--change-log-archive-interval-secs",
        "86400",
        "--change-log-max-unarchived-records",
        "3",
    ];

    let mut stderr_text = String::new();
    {
        let mut server = spawn_server(&users, &db, &flags);
        let mut drain = common::drain_stderr(&mut server.0);
        let (addr, _lines) = drain.wait_for_listening(Duration::from_secs(10));
        let addr = addr.expect("listening");
        let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
        common::send_simple_query(
            &mut alice,
            "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))",
        );
        let _ = common::read_command_complete(&mut alice);
        common::read_ready_for_query(&mut alice);
        insert(&mut alice, 1);
        insert(&mut alice, 2);
        // 未アーカイブが 3 件（上限）に達したので、次の書き込みは拒否される。
        common::send_simple_query(
            &mut alice,
            "INSERT INTO docs (id, embedding) VALUES (3, '[0.1,0.2,0.3]')",
        );
        common::expect_error_response_with_sqlstate(&mut alice, "54000");
        common::read_ready_for_query(&mut alice);
        let _ = server.0.kill();
        let _ = server.0.wait();
        // 読み取りスレッドが EOF まで流し切るのを期限つきで待つ。
        wait_until("the refusal line", Duration::from_secs(10), || {
            stderr_text = drain.tail_lines().join("\n");
            stderr_text.contains("unarchived change log is at its limit")
        });
    }
    assert!(
        stderr_text.contains("unarchived change log is at its limit"),
        "{stderr_text}"
    );
    // 起動時の周期はまだ書き出していない（CREATE 以降の分）。
    let before = segments(&arc).len();

    // 再起動すると起動直後の周期がアーカイブし、書き込みが通る。
    repair(&db);
    let mut server = spawn_server(&users, &db, &flags);
    let (addr, _lines) =
        common::wait_for_listening_addr_and_lines(&mut server.0, Duration::from_secs(10));
    wait_until("the startup archive pass", Duration::from_secs(20), || {
        segments(&arc).len() > before
    });
    let mut alice = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    insert(&mut alice, 3);
}
