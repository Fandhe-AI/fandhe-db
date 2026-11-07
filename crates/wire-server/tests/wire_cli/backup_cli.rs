//! `wire-server backup` サブコマンドの外形検証（Issue #2152・RECOVER-13 ポインタ。方式は
//! `docs/design/backup-snapshot.md`）。バイナリを子プロセスとして起動し、終了コードと
//! 出力先の状態を確かめる。サーバーを起動するテストはポート 0 と一意な一時ディレクトリを使う。

use crate::common;

use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

/// panic 時にも子プロセスを残さないための kill＋wait ガード。
pub(crate) struct ChildGuard(pub(crate) Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

pub(crate) fn backup(db: &str, out: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(["backup", "--db", db, "--out", out])
        .output()
        .expect("run backup")
}

fn code(o: &Output) -> Option<i32> {
    o.status.code()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

pub(crate) fn write_user_store_with_alice(path: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
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
    assert!(output.status.success(), "hash-password must succeed");
    let phc = String::from_utf8(output.stdout).expect("utf-8 phc");
    std::fs::write(path, format!("alice:tenant-a:{}\n", phc.trim())).expect("write user store");
}

pub(crate) fn spawn_server(users: &str, db: &str) -> ChildGuard {
    spawn_server_with(users, db, &[])
}

fn spawn_server_with(users: &str, db: &str, extra: &[&str]) -> ChildGuard {
    ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(["--users", users, "--db", db, "--bind", "127.0.0.1:0"])
            .args(["--ddl-allowed-users", "alice"])
            .args(extra)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server"),
    )
}

/// `SELECT id` の DataRow 件数を数える。
pub(crate) fn count_rows(stream: &mut std::net::TcpStream) -> usize {
    common::send_simple_query(stream, "SELECT id FROM docs");
    let mut n = 0;
    loop {
        let (ty, _) = common::read_message(stream);
        match ty {
            b'D' => n += 1,
            b'Z' => return n,
            b'T' | b'C' => {}
            other => panic!("unexpected message type {:?}", other as char),
        }
    }
}

pub(crate) fn parent_of(path: &str) -> &Path {
    Path::new(path).parent().expect("parent dir")
}

/// 稼働中の DB への取得は拒否され、停止後の取得は成功し、出力は単独でサーバーが開いて
/// 同じ行を返せる。既存の出力先への再取得は拒否される。
#[test]
fn backup_rejects_running_db_then_succeeds_after_stop_and_restores() {
    let fx = common::TempFixtureDir::new("backup-cli");
    let users = fx.users_path_str();
    let db = fx.db_path_str();
    write_user_store_with_alice(&users);
    let out = parent_of(&db).join("snap.redb");
    let out = out.to_str().expect("utf-8 path").to_string();

    let mut a = spawn_server(&users, &db);
    let (addr, _) = common::wait_for_listening_addr_and_lines(&mut a.0, Duration::from_secs(10));
    let mut s = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    common::send_simple_query(
        &mut s,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))",
    );
    let _ = common::read_command_complete(&mut s);
    common::read_ready_for_query(&mut s);
    for id in 1..=3 {
        common::send_simple_query(
            &mut s,
            &format!("INSERT INTO docs (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]')"),
        );
        let _ = common::read_command_complete(&mut s);
        common::read_ready_for_query(&mut s);
    }

    // 稼働中: fail-closed で非 0、出力は作られず、サーバーは無傷
    let o = backup(&db, &out);
    assert_eq!(code(&o), Some(1), "stderr: {}", stderr(&o));
    assert!(stderr(&o).contains("in use"), "stderr: {}", stderr(&o));
    assert!(!Path::new(&out).exists());
    assert_eq!(count_rows(&mut s), 3);
    drop(s);
    drop(a);

    // 停止後: 成功
    let o = backup(&db, &out);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert!(String::from_utf8_lossy(&o.stdout).contains("backup written"));
    assert!(Path::new(&out).is_file());

    // 既存の出力先は上書きしない
    let o = backup(&db, &out);
    assert_eq!(code(&o), Some(2), "stderr: {}", stderr(&o));

    // 出力から別のサーバーを起動して同じ行が読める
    let mut b = spawn_server(&users, &out);
    let (addr_b, _) = common::wait_for_listening_addr_and_lines(&mut b.0, Duration::from_secs(10));
    let mut s2 = common::authenticate_to_ready_for_query(addr_b, "alice", "pw-alice");
    assert_eq!(count_rows(&mut s2), 3);
}

/// 不正な引数・パスは DB を開く前に終了コード 2 で拒否され、何も作られない。
#[test]
fn backup_rejects_invalid_arguments_and_paths() {
    let fx = common::TempFixtureDir::new("backup-cli-invalid");
    let db = fx.db_path_str();
    std::fs::write(&db, b"not a database").expect("write dummy db");
    let dir = parent_of(&db).to_path_buf();
    let s = |p: std::path::PathBuf| p.to_str().expect("utf-8 path").to_string();

    let cases = [
        // 親ディレクトリ成分
        s(dir.join("sub/../snap.redb")),
        // 親ディレクトリが無い
        s(dir.join("missing/snap.redb")),
        // 出力先が既存（DB 自身）
        db.clone(),
        // 出力先が既存のディレクトリ
        s(dir.clone()),
        String::new(),
    ];
    for out in &cases {
        let o = backup(&db, out);
        assert_eq!(code(&o), Some(2), "out={out:?} stderr: {}", stderr(&o));
    }

    // --db が無い（空 DB を作らない）
    let missing = s(dir.join("nodb.redb"));
    let o = backup(&missing, &s(dir.join("snap.redb")));
    assert_eq!(code(&o), Some(2));
    assert!(!Path::new(&missing).exists());
    assert!(!dir.join("snap.redb").exists());

    // 未知の引数・必須欠落・値欠落
    for args in [
        vec!["backup", "--bogus"],
        vec!["backup", "--db", &db],
        vec!["backup", "--db"],
    ] {
        let o = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(args)
            .output()
            .expect("run backup");
        assert_eq!(code(&o), Some(2), "stderr: {}", stderr(&o));
    }
}

/// 壊れたシンボリックリンクを出力先に指定しても書き出さない。
#[test]
fn backup_rejects_dangling_symlink_destination() {
    let fx = common::TempFixtureDir::new("backup-cli-symlink");
    let db = fx.db_path_str();
    std::fs::write(&db, b"x").expect("write dummy db");
    let link = parent_of(&db).join("link.redb");
    let target = parent_of(&db).join("target.redb");
    std::os::unix::fs::symlink(&target, &link).expect("symlink");
    let o = backup(&db, link.to_str().expect("utf-8 path"));
    assert_eq!(code(&o), Some(2), "stderr: {}", stderr(&o));
    assert!(!target.exists());
}

// ---- 経路 (ii): 稼働中のサーバーの運用者専用ソケット（Issue #2152） ----

/// 0700 の子ディレクトリを作り、その中のソケットパスを返す（パス長の上限に収める短い名前）。
fn private_socket_path(fx_db: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = parent_of(fx_db).join("s");
    std::fs::create_dir(&dir).expect("create socket dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod 0700");
    let sock = dir.join("b.sock");
    (dir, sock)
}

fn insert(stream: &mut std::net::TcpStream, id: u32) {
    common::send_simple_query(
        stream,
        &format!("INSERT INTO docs (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]')"),
    );
    assert_eq!(common::read_command_complete(stream), "INSERT 0 1");
    common::read_ready_for_query(stream);
}

fn select_ids(stream: &mut std::net::TcpStream) -> Vec<String> {
    common::send_simple_query(stream, "SELECT id FROM docs ORDER BY id");
    let mut ids = Vec::new();
    loop {
        let (ty, body) = common::read_message(stream);
        match ty {
            b'D' => {
                let len = body
                    .get(2..6)
                    .and_then(|b| <[u8; 4]>::try_from(b).ok())
                    .map(i32::from_be_bytes)
                    .and_then(|l| usize::try_from(l).ok())
                    .expect("cell length");
                let cell = body.get(6..6 + len).expect("cell bytes");
                ids.push(String::from_utf8(cell.to_vec()).expect("utf-8 cell"));
            }
            b'T' | b'C' => {}
            b'Z' => return ids,
            other => panic!("unexpected message type {:?}", other as char),
        }
    }
}

/// 起動が拒否される（listen せず非 0 で終わる）ことを確かめる。
fn assert_start_rejected(users: &str, db: &str, extra: &[&str]) {
    let mut b = spawn_server_with(users, db, extra);
    let mut drain = common::drain_stderr(&mut b.0);
    let (addr, lines) = drain.wait_for_listening(Duration::from_secs(30));
    assert!(addr.is_none(), "must not listen; stderr: {lines:?}");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(st) = b.0.try_wait().expect("try_wait") {
            break st;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "did not exit: {lines:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1), "stderr: {lines:?}");
}

fn backup_via_socket(sock: &Path, out: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(["backup", "--socket"])
        .arg(sock)
        .arg("--out")
        .arg(out)
        .output()
        .expect("run backup")
}

/// 稼働中のサーバーに書き込みを流しながらソケット経由で取得でき、取得開始前の commit は
/// 出力に含まれ、出力は単独で開ける。書き込みは止まらない。
#[test]
fn socket_backup_while_writing_includes_prior_commits() {
    let fx = common::TempFixtureDir::new("bk");
    let users = fx.users_path_str();
    let db = fx.db_path_str();
    write_user_store_with_alice(&users);
    let (_dir, sock) = private_socket_path(&db);
    let out = parent_of(&db).join("snap.redb");
    let sock_s = sock.to_str().expect("utf-8").to_string();

    let mut a = spawn_server_with(&users, &db, &["--backup-socket", &sock_s]);
    let (addr, _) = common::wait_for_listening_addr_and_lines(&mut a.0, Duration::from_secs(10));
    let mut s = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    common::send_simple_query(
        &mut s,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))",
    );
    let _ = common::read_command_complete(&mut s);
    common::read_ready_for_query(&mut s);
    for id in 1..=3 {
        insert(&mut s, id);
    }
    assert_eq!(
        std::fs::metadata(&sock)
            .expect("socket")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );

    // 取得中も別接続から書き込みを続ける
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_w = std::sync::Arc::clone(&stop);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
    let writer = std::thread::spawn(move || {
        let mut w = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
        let mut n = 0u32;
        while !stop_w.load(std::sync::atomic::Ordering::Relaxed) {
            insert(&mut w, 100 + n);
            n += 1;
            if n == 1 {
                let _ = ready_tx.send(());
            }
        }
        n
    });
    // 書き込みが流れ始めてから取得する
    ready_rx
        .recv_timeout(Duration::from_secs(30))
        .expect("writer started");
    let o = backup_via_socket(&sock, &out);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let written = writer.join().expect("writer must not fail");
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
    assert!(written > 0, "writes must continue during the backup");
    assert!(out.is_file());

    // 既存の出力先は拒否される
    let o = backup_via_socket(&sock, &out);
    assert_eq!(code(&o), Some(2), "stderr: {}", stderr(&o));

    // 元のサーバーは無傷
    assert!(select_ids(&mut s).len() >= 3);
    drop(s);

    // 出力を別のサーバーで開き、取得開始前の行が含まれる
    let out_s = out.to_str().expect("utf-8");
    let mut b = spawn_server(&users, out_s);
    let (addr_b, _) = common::wait_for_listening_addr_and_lines(&mut b.0, Duration::from_secs(10));
    let mut s2 = common::authenticate_to_ready_for_query(addr_b, "alice", "pw-alice");
    let ids = select_ids(&mut s2);
    for id in ["1", "2", "3"] {
        assert!(ids.iter().any(|i| i == id), "missing {id}: {ids:?}");
    }
}

/// 権限の緩い親ディレクトリ・既存の通常ファイルでは起動が拒否され、古いソケットは置換される。
#[test]
fn socket_flag_rejects_loose_paths_and_replaces_stale_socket() {
    let fx = common::TempFixtureDir::new("bk2");
    let users = fx.users_path_str();
    let db = fx.db_path_str();
    write_user_store_with_alice(&users);
    let base = parent_of(&db).to_path_buf();

    // 親ディレクトリが 0755
    let loose = base.join("l");
    std::fs::create_dir(&loose).expect("mkdir");
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let loose_sock = loose.join("b.sock");
    assert_start_rejected(
        &users,
        &db,
        &["--backup-socket", loose_sock.to_str().expect("utf-8")],
    );

    // 既存の通常ファイル・シンボリックリンク
    let (dir, sock) = private_socket_path(&db);
    std::fs::write(&sock, b"x").expect("write file");
    assert_start_rejected(
        &users,
        &db,
        &["--backup-socket", sock.to_str().expect("utf-8")],
    );
    std::fs::remove_file(&sock).expect("rm");
    let target = dir.join("t");
    std::fs::write(&target, b"x").expect("write");
    std::os::unix::fs::symlink(&target, &sock).expect("symlink");
    assert_start_rejected(
        &users,
        &db,
        &["--backup-socket", sock.to_str().expect("utf-8")],
    );
    std::fs::remove_file(&sock).expect("rm");

    // 古いソケット（リスナーを閉じて残ったファイル）は置き換えて起動できる
    drop(std::os::unix::net::UnixListener::bind(&sock).expect("bind stale"));
    assert!(sock.exists());
    let mut a = spawn_server_with(
        &users,
        &db,
        &["--backup-socket", sock.to_str().expect("utf-8")],
    );
    let (_addr, _) = common::wait_for_listening_addr_and_lines(&mut a.0, Duration::from_secs(10));
    // 稼働中のソケットは 2 台目が奪わない
    let db2 = base.join("db2.redb");
    assert_start_rejected(
        &users,
        db2.to_str().expect("utf-8"),
        &["--backup-socket", sock.to_str().expect("utf-8")],
    );
}

/// フラグなしではソケットが作られず、`--socket` の依頼は実行時の失敗になる。
#[test]
fn no_flag_creates_no_socket() {
    let fx = common::TempFixtureDir::new("bk3");
    let users = fx.users_path_str();
    let db = fx.db_path_str();
    write_user_store_with_alice(&users);
    let (dir, sock) = private_socket_path(&db);
    let mut a = spawn_server(&users, &db);
    let (_addr, _) = common::wait_for_listening_addr_and_lines(&mut a.0, Duration::from_secs(10));
    assert_eq!(std::fs::read_dir(&dir).expect("read_dir").count(), 0);
    let out = parent_of(&db).join("snap.redb");
    let o = backup_via_socket(&sock, &out);
    assert_eq!(code(&o), Some(2), "stderr: {}", stderr(&o));
    assert!(!out.exists());
}

/// 不正な要求（長すぎる行・相対パス・改行なし）で接続が閉じ、サーバーは生きていて、
/// その後の正しい要求は成功する。`--db` と `--socket` の併用は拒否される。
#[test]
fn socket_rejects_malformed_requests_and_server_survives() {
    let fx = common::TempFixtureDir::new("bk4");
    let users = fx.users_path_str();
    let db = fx.db_path_str();
    write_user_store_with_alice(&users);
    let (_dir, sock) = private_socket_path(&db);
    let mut a = spawn_server_with(
        &users,
        &db,
        &["--backup-socket", sock.to_str().expect("utf-8")],
    );
    let (_addr, _) = common::wait_for_listening_addr_and_lines(&mut a.0, Duration::from_secs(10));

    let exchange = |payload: &[u8]| -> String {
        let mut c = UnixStream::connect(&sock).expect("connect");
        c.set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        let _ = c.write_all(payload);
        let mut resp = String::new();
        let _ = c.read_to_string(&mut resp);
        resp
    };
    let long = vec![b'a'; 100_000];
    assert!(exchange(&long).starts_with("2 "));
    let mut long_nl = vec![b'/'; 5000];
    long_nl.push(b'\n');
    assert!(exchange(&long_nl).starts_with("2 "));
    assert!(exchange(b"relative/path\n").starts_with("2 "));
    assert!(exchange(b"/tmp/../etc/x\n").starts_with("2 "));
    assert!(exchange(&[0xff, 0xfe, b'\n']).starts_with("2 "));
    assert!(exchange(b"/no-such-dir-for-backup/x\n").starts_with("2 "));
    assert!(exchange(b"").is_empty() || exchange(b"").starts_with("2 "));

    // 1 バイトずつ送り続けても、要求全体の期限（5 秒）で閉じられる
    let started = std::time::Instant::now();
    let mut c = UnixStream::connect(&sock).expect("connect");
    c.set_read_timeout(Some(Duration::from_millis(500)))
        .expect("timeout");
    let mut resp = String::new();
    loop {
        if c.write_all(b"a").is_err() {
            break;
        }
        match c.read_to_string(&mut resp) {
            Ok(_) => break,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => break,
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "slow request must be cut off"
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(9),
        "deadline not enforced"
    );
    assert!(resp.is_empty() || resp.starts_with("2 "), "resp: {resp:?}");

    assert!(
        a.0.try_wait().expect("try_wait").is_none(),
        "server must be alive"
    );
    let out = parent_of(&db).join("snap.redb");
    let o = backup_via_socket(&sock, &out);
    assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));

    let o = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(["backup", "--db", &db, "--socket"])
        .arg(&sock)
        .arg("--out")
        .arg(parent_of(&db).join("x.redb"))
        .output()
        .expect("run");
    assert_eq!(code(&o), Some(2));
}

/// CLI は、ソケット型でない・権限が緩いパスへは接続せず終了コード 2 で拒否し、
/// 応答の文字列（制御文字を含みうる）を端末へ出さない。
#[test]
fn cli_distrusts_loose_sockets_and_fake_responses() {
    let fx = common::TempFixtureDir::new("bk5");
    let db = fx.db_path_str();
    let (dir, sock) = private_socket_path(&db);
    let out = parent_of(&db).join("snap.redb");

    // 通常ファイル
    std::fs::write(&sock, b"x").expect("write");
    let o = backup_via_socket(&sock, &out);
    assert_eq!(code(&o), Some(2), "stderr: {}", stderr(&o));
    std::fs::remove_file(&sock).expect("rm");

    // 権限が緩いソケット
    let l = std::os::unix::net::UnixListener::bind(&sock).expect("bind");
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666)).expect("chmod");
    let o = backup_via_socket(&sock, &out);
    assert_eq!(code(&o), Some(2), "stderr: {}", stderr(&o));

    // 0600 の偽サーバー: 制御文字つきの応答は信用せず、そのまま出さない
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    let fake = std::thread::spawn(move || {
        for _ in 0..2 {
            if let Ok((mut c, _)) = l.accept() {
                let mut buf = [0u8; 256];
                let _ = c.read(&mut buf);
                let _ = c.write_all(b"1 \x1b[31mevil\n");
            }
        }
    });
    let o = backup_via_socket(&sock, &out);
    assert_eq!(code(&o), Some(1), "stderr: {}", stderr(&o));
    assert!(!stderr(&o).contains("evil") && !stderr(&o).contains('\x1b'));
    let _ = std::os::unix::net::UnixStream::connect(&sock);
    let _ = fake.join();
    drop(dir);
}

/// 2 テナント（alice=tenant-a・bob=tenant-b）のユーザーファイルを書く（同じパスワードの検証子を共有）。
pub(crate) fn write_user_store_with_alice_and_bob(path: &str) {
    write_user_store_with_alice(path);
    let line = std::fs::read_to_string(path).expect("read user store");
    let phc = line
        .trim()
        .strip_prefix("alice:tenant-a:")
        .expect("alice line")
        .to_string();
    std::fs::write(path, format!("alice:tenant-a:{phc}\nbob:tenant-b:{phc}\n"))
        .expect("write user store");
}

/// 2 テナントがそれぞれ書き続けている最中にソケット経由で取得し（RECOVER-13・Issue #2153）、
/// 出力から起動したサーバーで、(1) 各テナントの取得前の行が読め、(2) 各接続の書き込みは
/// 欠番のない先頭からの連続（単一 commit 点の接頭辞）で、(3) 他テナントの行は互いに見えない。
#[test]
fn socket_backup_under_two_tenant_writers_restores_prefixes_and_keeps_boundary() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};

    let fx = common::TempFixtureDir::new("bk-tenants");
    let users = fx.users_path_str();
    let db = fx.db_path_str();
    write_user_store_with_alice_and_bob(&users);
    let (_dir, sock) = private_socket_path(&db);
    let sock_s = sock.to_str().expect("utf-8").to_string();

    let mut a = spawn_server_with(&users, &db, &["--backup-socket", &sock_s]);
    let (addr, _) = common::wait_for_listening_addr_and_lines(&mut a.0, Duration::from_secs(10));
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
    let mut bob = common::authenticate_to_ready_for_query(addr, "bob", "pw-alice");
    for id in 10..=11 {
        insert(&mut bob, id);
    }

    let stop = Arc::new(AtomicBool::new(false));
    // 各 writer が完了した INSERT の件数（応答を受けた後に更新する）。
    let done_a = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let done_b = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let spawn_writer = |user: &'static str,
                        base: u32,
                        tx: mpsc::Sender<()>,
                        done: Arc<std::sync::atomic::AtomicU32>| {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut w = common::authenticate_to_ready_for_query(addr, user, "pw-alice");
            let mut n = 0u32;
            while !stop.load(Ordering::Relaxed) {
                insert(&mut w, base + n);
                n += 1;
                done.store(n, Ordering::SeqCst);
                if n == 1 {
                    let _ = tx.send(());
                }
            }
            n
        })
    };
    let (tx_a, rx_a) = mpsc::channel();
    let (tx_b, rx_b) = mpsc::channel();
    let wa = spawn_writer("alice", 100_000, tx_a, Arc::clone(&done_a));
    let wb = spawn_writer("bob", 200_000, tx_b, Arc::clone(&done_b));
    rx_a.recv_timeout(Duration::from_secs(30))
        .expect("alice writer started");
    rx_b.recv_timeout(Duration::from_secs(30))
        .expect("bob writer started");
    // 取得の最中に両 writer のいずれかの INSERT が完了した実行（取得開始前の完了数 < 取得完了後の完了数）が
    // 現れるまで、別名の出力へ取得を繰り返す。上限に達したら失敗にする。
    let mut chosen: Option<(std::path::PathBuf, u32, u32)> = None;
    for attempt in 0..40 {
        let cand = parent_of(&db).join(format!("snap{attempt}.redb"));
        let (a0, b0) = (done_a.load(Ordering::SeqCst), done_b.load(Ordering::SeqCst));
        let o = backup_via_socket(&sock, &cand);
        let (a1, b1) = (done_a.load(Ordering::SeqCst), done_b.load(Ordering::SeqCst));
        assert_eq!(code(&o), Some(0), "stderr: {}", stderr(&o));
        if a1 + b1 > a0 + b0 {
            chosen = Some((cand, a0, b0));
            break;
        }
    }
    stop.store(true, Ordering::Relaxed);
    let wrote_a = wa.join().expect("alice writer must not fail");
    let wrote_b = wb.join().expect("bob writer must not fail");
    let (out, done_a0, done_b0) = chosen.expect("no backup overlapped a completed write");
    assert!(
        done_a0 >= 1 && done_b0 >= 1,
        "writers started before backup"
    );
    // 既存の出力先は拒否される
    let o = backup_via_socket(&sock, &out);
    assert_eq!(code(&o), Some(2), "stderr: {}", stderr(&o));
    drop(alice);
    drop(bob);
    drop(a);

    let out_s = out.to_str().expect("utf-8");
    let mut b = spawn_server(&users, out_s);
    let (addr_b, _) = common::wait_for_listening_addr_and_lines(&mut b.0, Duration::from_secs(10));
    let ids_of = |user: &str| -> Vec<u32> {
        let mut s = common::authenticate_to_ready_for_query(addr_b, user, "pw-alice");
        select_ids(&mut s)
            .iter()
            .map(|i| i.parse().expect("numeric id"))
            .collect()
    };
    let check = |ids: &[u32],
                 own_seed: &[u32],
                 base: u32,
                 other_base: u32,
                 done_before: u32,
                 wrote: u32| {
        for s in own_seed {
            assert!(ids.contains(s), "missing seed {s}: {ids:?}");
        }
        let run: Vec<u32> = ids
            .iter()
            .copied()
            .filter(|i| (base..base + 100_000).contains(i))
            .collect();
        let want: Vec<u32> = (base..base + run.len() as u32).collect();
        assert_eq!(run, want, "writes must be a gapless prefix");
        // 取得の開始前に完了した書き込みはすべて含まれる（空や古い出力は不可）。
        assert!(
            !run.is_empty() && run[0] == base,
            "first write missing: {run:?}"
        );
        assert!(
            run.len() as u32 >= done_before,
            "snapshot lost completed writes: {} < {done_before}",
            run.len()
        );
        assert!(run.len() as u32 <= wrote);
        assert!(
            ids.iter()
                .all(|i| !(other_base..other_base + 100_000).contains(i)),
            "other tenant rows must not be visible: {ids:?}"
        );
        assert_eq!(ids.len(), own_seed.len() + run.len(), "no unexpected rows");
    };
    let alice_ids = ids_of("alice");
    let bob_ids = ids_of("bob");
    check(&alice_ids, &[1, 2, 3], 100_000, 200_000, done_a0, wrote_a);
    check(&bob_ids, &[10, 11], 200_000, 100_000, done_b0, wrote_b);
    assert!(!bob_ids.iter().any(|i| [1, 2, 3].contains(i)));
    assert!(!alice_ids.iter().any(|i| [10, 11].contains(i)));
}
