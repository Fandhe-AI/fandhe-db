//! 同じデータファイル（`--db`）を指す 2 つ目の `wire-server` の起動拒否と、稼働中の
//! 1 つ目への無影響を、バイナリ子プロセスとして外形的に検証する結合テスト
//! （Issue #2423・親 #2420・TASK-341・PERSIST-4 ポインタ。仕様は
//! `docs/spec/04-behavior/persistence.md` を参照し、本文は転記しない）。
//!
//! 段取り:
//! - R1: A の稼働中に同じ `--db` で B を起動すると、listen に到達せず非 0（通常の失敗終了）で終わる
//! - R1': 同じファイルを別表記（`./` 成分入り）で指す B' でも同様に拒否される
//! - R2: B・B' の試行後も A は生存し、既存セッション・新規接続・書き込みが健全
//! - R3: A 停止後に同じ引数で C を起動でき、A 時代の全行がディスク上に残っている
//!
//! DB オープンは bind より前にあるため、A の `listening on` 到達後に B を起動すれば
//! 順序が保証される（sleep による順序合わせはしない）。B の待ち受けはハング防止の
//! 締切付きで、ロックが効かず B が listen した場合は kill して契約未達として失敗させる。

use crate::common;

use std::io::Write;
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// panic 時にも子プロセスを残さないための kill＋wait ガード。
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `alice:tenant-a:<phc>` のユーザーストアを書く（ハッシュは `hash-password` で生成する）。
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

fn server_args(users: &str, db: &str) -> Vec<String> {
    [
        "--users",
        users,
        "--db",
        db,
        "--bind",
        "127.0.0.1:0",
        "--ddl-allowed-users",
        "alice",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn spawn_server(args: &[String]) -> ChildGuard {
    ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn wire-server"),
    )
}

/// 単純問い合わせを実行し、CommandComplete タグまでの DataRow 先頭列を返す。
fn select_ids(stream: &mut TcpStream) -> Vec<String> {
    common::send_simple_query(stream, "SELECT id FROM docs ORDER BY id");
    let mut ids = Vec::new();
    loop {
        let (ty, body) = common::read_message(stream);
        match ty {
            b'T' => {}
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
            b'C' => {}
            b'Z' => return ids,
            other => panic!("unexpected message type {:?}", other as char),
        }
    }
}

fn insert(stream: &mut TcpStream, id: u32) {
    common::send_simple_query(
        stream,
        &format!("INSERT INTO docs (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]')"),
    );
    assert_eq!(common::read_command_complete(stream), "INSERT 0 1");
    common::read_ready_for_query(stream);
}

/// 2 つ目のサーバーを起動し、listen せず非 0 の通常終了で拒否されることを確かめる。
fn assert_second_server_rejected(args: &[String]) {
    let mut b = spawn_server(args);
    let mut drain = common::drain_stderr(&mut b.0);
    let (addr, lines) = drain.wait_for_listening(Duration::from_secs(30));
    assert!(
        addr.is_none(),
        "second server must not listen on the same db file; stderr: {lines:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = b.0.try_wait().expect("try_wait") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "second server did not exit; stderr: {lines:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(
        !status.success(),
        "expected non-zero exit; stderr: {lines:?}"
    );
    assert_eq!(
        status.code(),
        Some(1),
        "expected plain failure exit: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("failed to open database")),
        "expected open failure in stderr: {lines:?}"
    );
    assert!(
        lines.iter().all(|l| !l.contains("listening on")),
        "must not reach listening: {lines:?}"
    );
}

/// R1〜R3（PERSIST-4 ポインタ）: 同一データファイルの 2 つ目のサーバーは起動を拒否され、
/// 稼働中のサーバーとデータは無傷で、A 停止後の再起動で全行が読める。
#[test]
fn second_server_on_same_db_is_rejected_and_first_is_unaffected() {
    let fixture = common::TempFixtureDir::new("persist4");
    write_user_store_with_alice(&fixture.users_path_str());
    let args = server_args(&fixture.users_path_str(), &fixture.db_path_str());

    // A の起動と seed
    let mut a = spawn_server(&args);
    let (addr, _) = common::wait_for_listening_addr_and_lines(&mut a.0, Duration::from_secs(10));
    let mut s1 = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    common::send_simple_query(
        &mut s1,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))",
    );
    let _ = common::read_command_complete(&mut s1);
    common::read_ready_for_query(&mut s1);
    insert(&mut s1, 1);
    insert(&mut s1, 2);
    assert_eq!(select_ids(&mut s1), ["1", "2"]);

    // R1: 同じ引数の B、R1': 別表記の B'
    assert_second_server_rejected(&args);
    let alt_db = fixture.db_path_str().replace("db.redb", "./db.redb");
    assert_ne!(alt_db, fixture.db_path_str());
    assert_second_server_rejected(&server_args(&fixture.users_path_str(), &alt_db));

    // R2: A が無傷
    assert!(
        a.0.try_wait().expect("try_wait A").is_none(),
        "A must be alive"
    );
    assert_eq!(select_ids(&mut s1), ["1", "2"]);
    let mut s2 = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    assert_eq!(select_ids(&mut s2), ["1", "2"]);
    insert(&mut s2, 3);
    assert_eq!(select_ids(&mut s2), ["1", "2", "3"]);
    assert_eq!(select_ids(&mut s1), ["1", "2", "3"]);

    // R3: A 停止後に C を起動し、ディスク上のデータを確認する
    drop(s1);
    drop(s2);
    drop(a);
    let mut c = spawn_server(&args);
    let (addr_c, _) = common::wait_for_listening_addr_and_lines(&mut c.0, Duration::from_secs(10));
    let mut s3 = common::authenticate_to_ready_for_query(addr_c, "alice", "pw-alice");
    assert_eq!(select_ids(&mut s3), ["1", "2", "3"]);
}
