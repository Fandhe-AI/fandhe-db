//! `wire-server backup` サブコマンドの外形検証（Issue #2152・RECOVER-13 ポインタ。方式は
//! `docs/design/backup-snapshot.md`）。バイナリを子プロセスとして起動し、終了コードと
//! 出力先の状態を確かめる。サーバーを起動するテストはポート 0 と一意な一時ディレクトリを使う。

use crate::common;

use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::Duration;

/// panic 時にも子プロセスを残さないための kill＋wait ガード。
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn backup(db: &str, out: &str) -> Output {
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

fn write_user_store_with_alice(path: &str) {
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

fn spawn_server(users: &str, db: &str) -> ChildGuard {
    ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args([
                "--users",
                users,
                "--db",
                db,
                "--bind",
                "127.0.0.1:0",
                "--ddl-allowed-users",
                "alice",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server"),
    )
}

/// `SELECT id` の DataRow 件数を数える。
fn count_rows(stream: &mut std::net::TcpStream) -> usize {
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

fn parent_of(path: &str) -> &Path {
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
