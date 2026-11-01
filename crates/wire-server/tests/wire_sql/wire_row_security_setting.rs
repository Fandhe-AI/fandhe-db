//! セッション変数 `row_security`（RLS-13・Issue #2278）の pg wire 結合テスト。
//!
//! 起動パラメータ `row_security` の反映（キー大文字小文字無視・不正値は AuthenticationOk 後に FATAL
//! `22023` で切断・重複は `08P01`）、`SET`／`SHOW`／`RESET` の往復とタグ、不正値の `22023`・
//! `SET LOCAL` の `0A000` を固定する。engine 側の判定は `table_rls/rls13_policy_roles_row_security.rs`。

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
    let path = temp_db::unique_db_path("wire-row-security-setting");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    (core, guard)
}

fn spawn(core: &Arc<EngineCore>) -> std::net::SocketAddr {
    let users_path = write_user_store_file(&[("alice", "tenant-a", PASSWORD)]);
    spawn_server_with_engine(&users_path, Arc::clone(core))
}

fn send_startup(stream: &mut TcpStream, extra: &[(&str, &str)]) {
    let mut params = Vec::new();
    for (k, v) in [("user", "alice")].iter().chain(extra.iter()) {
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
fn connect(addr: std::net::SocketAddr, extra: &[(&str, &str)]) -> TcpStream {
    let mut stream = TcpStream::connect(addr).expect("connect");
    send_startup(&mut stream, extra);
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

/// `SHOW row_security` の 1 セル（on／off）。
fn show(stream: &mut TcpStream) -> String {
    let msgs = run(stream, "SHOW row_security");
    let data = msgs
        .iter()
        .find(|(t, _)| *t == b'D')
        .expect("DataRow for SHOW");
    let body = &data.1;
    let len = i32::from_be_bytes(body[2..6].try_into().expect("len")) as usize;
    String::from_utf8_lossy(&body[6..6 + len]).to_string()
}

/// RLS-13: 既定は on。SET／RESET は `SET`／`RESET` タグで、SHOW は on／off を返す。
#[test]
fn row_security_set_show_reset_roundtrip() {
    let (core, _g) = new_core();
    let mut s = connect(spawn(&core), &[]);
    assert_eq!(show(&mut s), "on");
    let msgs = run(&mut s, "SET row_security = off");
    assert_eq!(tag_of(&msgs[0].1), "SET");
    assert_eq!(show(&mut s), "off");
    let msgs = run(&mut s, "RESET row_security");
    assert_eq!(tag_of(&msgs[0].1), "RESET");
    assert_eq!(show(&mut s), "on");
}

/// RLS-13: トランザクションの ROLLBACK で `SET row_security` が取り消される。
#[test]
fn row_security_is_rolled_back_with_transaction() {
    let (core, _g) = new_core();
    let mut s = connect(spawn(&core), &[]);
    run(&mut s, "BEGIN");
    run(&mut s, "SET row_security = off");
    assert_eq!(show(&mut s), "off");
    run(&mut s, "ROLLBACK");
    assert_eq!(show(&mut s), "on");
}

/// RLS-13: 不正値は `22023`、`SET LOCAL` は `0A000`。接続は継続し値は変わらない。
#[test]
fn row_security_invalid_values_are_rejected() {
    let (core, _g) = new_core();
    let mut s = connect(spawn(&core), &[]);
    for (sql, code) in [
        ("SET row_security = maybe", "22023"),
        ("SET LOCAL row_security = off", "0A000"),
    ] {
        let msgs = run(&mut s, sql);
        assert_eq!(msgs[0].0, b'E', "{sql}");
        let text = String::from_utf8_lossy(&msgs[0].1).to_string();
        assert!(text.contains(&format!("C{code}")), "{sql}: {text:?}");
        assert!(!text.contains("maybe"), "value must not be reflected");
    }
    assert_eq!(show(&mut s), "on");
}

/// RLS-13: 起動パラメータ `row_security=off`（キーは大文字小文字無視）が初期値・reset 値になる。
#[test]
fn startup_row_security_sets_initial_and_reset_value() {
    let (core, _g) = new_core();
    let mut s = connect(spawn(&core), &[("Row_Security", "off")]);
    assert_eq!(show(&mut s), "off");
    run(&mut s, "SET row_security = on");
    assert_eq!(show(&mut s), "on");
    run(&mut s, "RESET row_security");
    assert_eq!(show(&mut s), "off");
}

/// RLS-13: 真偽として読めない起動パラメータは AuthenticationOk の後に FATAL `22023` で切断する。
#[test]
fn invalid_startup_row_security_is_fatal_22023() {
    let (core, _g) = new_core();
    let mut stream = TcpStream::connect(spawn(&core)).expect("connect");
    send_startup(&mut stream, &[("row_security", "sometimes")]);
    let (t, _) = read_message(&mut stream);
    assert_eq!(t, b'R');
    send_password_message(&mut stream, PASSWORD);
    let (t, body) = read_message(&mut stream);
    assert_eq!((t, body.as_slice()), (b'R', &0i32.to_be_bytes()[..]));
    let (t, body) = read_message(&mut stream);
    assert_eq!(t, b'E');
    let text = String::from_utf8_lossy(&body).to_string();
    assert!(text.contains("SFATAL"), "{text:?}");
    assert!(text.contains("C22023"), "{text:?}");
    assert!(!text.contains("sometimes"), "value must not be reflected");
    expect_connection_closed(&mut stream);
}

/// RLS-13: 起動パラメータの重複は `08P01`（曖昧さを持ち込まない）。
#[test]
fn duplicate_startup_row_security_is_protocol_violation() {
    let (core, _g) = new_core();
    let mut stream = TcpStream::connect(spawn(&core)).expect("connect");
    send_startup(
        &mut stream,
        &[("row_security", "on"), ("ROW_SECURITY", "off")],
    );
    expect_error_response_with_sqlstate(&mut stream, "08P01");
}
