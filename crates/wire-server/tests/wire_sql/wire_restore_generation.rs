//! `SHOW restore_generation`（RECOVER-17・RECOVER-13・Issue #2449 のポインタ）の pg wire 結合テスト。
//!
//! 列が int8（OID 20）の 1 列であること、値がテナント非依存であること、`SET`／`RESET` が `42501` で
//! 拒否され値が不変であること、StartupMessage の同名パラメータが読み捨てられることを固定する。
//! engine 側の判定は `engine/tests/sql_t40_up/recover17_restore_generation_show.rs`。

use crate::common;

use crate::temp_db;

use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

const PASSWORD: &str = "correct-horse";

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-restore-generation");
    let guard = temp_db::CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open storage");
    storage.advance_restore_generation().expect("advance");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    (core, guard)
}

fn spawn(core: &Arc<EngineCore>) -> std::net::SocketAddr {
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", PASSWORD),
        ("bob", "tenant-b", PASSWORD),
    ]);
    spawn_server_with_engine(&users_path, Arc::clone(core))
}

fn send_startup(stream: &mut TcpStream, user: &str, extra: &[(&str, &str)]) {
    let mut params = Vec::new();
    for (k, v) in [("user", user)].iter().chain(extra.iter()) {
        params.extend_from_slice(k.as_bytes());
        params.push(0);
        params.extend_from_slice(v.as_bytes());
        params.push(0);
    }
    params.push(0);
    let total_len = (4 + 4 + params.len()) as i32;
    let mut msg = Vec::new();
    msg.extend_from_slice(&total_len.to_be_bytes());
    msg.extend_from_slice(&0x0003_0000i32.to_be_bytes());
    msg.extend_from_slice(&params);
    stream.write_all(&msg).expect("send startup");
}

/// 接続して認証を通し、ReadyForQuery まで読み進める。
fn connect(addr: std::net::SocketAddr, user: &str, extra: &[(&str, &str)]) -> TcpStream {
    let mut stream = TcpStream::connect(addr).expect("connect");
    send_startup(&mut stream, user, extra);
    let (t, _) = read_message(&mut stream);
    assert_eq!(t, b'R', "AuthenticationCleartextPassword");
    send_password_message(&mut stream, PASSWORD);
    let (t, body) = read_message(&mut stream);
    assert_eq!((t, body.as_slice()), (b'R', &0i32.to_be_bytes()[..]));
    loop {
        let (t, _) = read_message(&mut stream);
        match t {
            b'S' | b'K' => {}
            b'Z' => break,
            other => panic!("unexpected message {}", other as char),
        }
    }
    stream
}

fn run(stream: &mut TcpStream, sql: &str) -> Vec<(u8, Vec<u8>)> {
    send_simple_query(stream, sql);
    let mut out = Vec::new();
    loop {
        let msg = read_message(stream);
        let done = msg.0 == b'Z';
        out.push(msg);
        if done {
            return out;
        }
    }
}

fn tag_of(body: &[u8]) -> String {
    String::from_utf8_lossy(&body[..body.len().saturating_sub(1)]).to_string()
}

/// `SHOW restore_generation` の RowDescription の型 OID・DataRow の文字列・CommandComplete タグを返す。
fn show(stream: &mut TcpStream) -> (u32, String, String) {
    let msgs = run(stream, "SHOW restore_generation");
    let t = msgs
        .iter()
        .find(|(t, _)| *t == b'T')
        .expect("RowDescription");
    let body = &t.1;
    assert_eq!(i16::from_be_bytes([body[0], body[1]]), 1, "one column");
    let name_end = body[2..].iter().position(|b| *b == 0).expect("nul") + 2;
    assert_eq!(&body[2..name_end], b"restore_generation");
    let o = name_end + 1 + 4 + 2;
    let oid = u32::from_be_bytes(body[o..o + 4].try_into().expect("oid"));
    let d = msgs.iter().find(|(t, _)| *t == b'D').expect("DataRow");
    let len = i32::from_be_bytes(d.1[2..6].try_into().expect("len")) as usize;
    let value = String::from_utf8_lossy(&d.1[6..6 + len]).to_string();
    let c = msgs
        .iter()
        .find(|(t, _)| *t == b'C')
        .expect("CommandComplete");
    (oid, value, tag_of(&c.1))
}

/// 列は int8（OID 20）、タグは `SHOW`、値は advance 済みの正の整数。
#[test]
fn show_returns_bigint_column_and_current_value() {
    let (core, _g) = new_core();
    let mut s = connect(spawn(&core), "alice", &[]);
    let (oid, value, tag) = show(&mut s);
    assert_eq!((oid, tag.as_str()), (20, "SHOW"));
    assert!(value.parse::<i64>().expect("bigint text") > 0, "{value}");
}

/// 別テナントでも同じ値。応答に他テナントを示す語を含まない。
#[test]
fn value_is_identical_for_other_tenant() {
    let (core, _g) = new_core();
    let addr = spawn(&core);
    let mut a = connect(addr, "alice", &[]);
    let mut b = connect(addr, "bob", &[]);
    assert_eq!(show(&mut a), show(&mut b));
    let raw = format!("{:?}", run(&mut b, "SHOW restore_generation"));
    assert!(!raw.contains("tenant-a") && !raw.contains("alice"));
}

/// `SET`／`RESET`／`SET LOCAL` は `42501`。値は不変で接続は継続する。
#[test]
fn mutation_is_rejected_with_42501() {
    let (core, _g) = new_core();
    let mut s = connect(spawn(&core), "alice", &[]);
    let before = show(&mut s).1;
    for sql in [
        "SET restore_generation = 999",
        "SET LOCAL restore_generation = 999",
        "RESET restore_generation",
    ] {
        let msgs = run(&mut s, sql);
        assert_eq!(msgs[0].0, b'E', "{sql}");
        let text = String::from_utf8_lossy(&msgs[0].1).to_string();
        assert!(text.contains("C42501"), "{sql}: {text:?}");
        assert!(!text.contains("999"), "value must not be reflected");
        assert_eq!(show(&mut s).1, before, "{sql}");
    }
}

/// StartupMessage の `restore_generation` は読み捨てられ、値は変わらない。
#[test]
fn startup_parameter_does_not_change_value() {
    let (core, _g) = new_core();
    let addr = spawn(&core);
    let expected = show(&mut connect(addr, "alice", &[])).1;
    let mut s = connect(addr, "alice", &[("restore_generation", "999")]);
    assert_eq!(show(&mut s).1, expected);
}
