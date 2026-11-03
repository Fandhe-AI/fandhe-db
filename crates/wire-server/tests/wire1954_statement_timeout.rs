//! `statement_timeout` の発火（`57014`）の結合テスト（Issue #1954。ポインタ: SQL-71）。
//!
//! 時間に依存する（確実に 1ms を大きく超える読み取り文を使う）ため、`tests/` 直下の単独バイナリに置く
//! （README の「wire-server の結合テストの束ね方」に従い束ねない）。アサーションは「最終的に `57014`」
//! だけを固定し、経過秒数は測らず sleep 待ちも置かない。重い文は、全行が不一致になる非等価 ON の
//! ネステッドループ JOIN（約 625 万回の比較。比較回数の資源上限 1000 万回には届かない）で、#1952 で
//! 確認点が入った結合ループを通る。
//!
//! - T1: `SET statement_timeout = 1` の後に重い文が `57014`（所定文言）。`= 0` なら同じ文が成功する
//! - T2: 同一 `'Q'` メッセージ内の `SET statement_timeout = 1; <重い文>` で後続文が `57014`（文単位）
//! - T3: 明示 Tx で `57014` → 次の文は `25P02` → `ROLLBACK` で Idle に戻る
//! - T4: `SAVEPOINT` 後の `57014` は `ROLLBACK TO` で復帰でき、値も巻き戻る
//! - T5: 拡張クエリの Execute でも `57014`
//! - T6: 起動時 CLI の既定値 `--statement-timeout-ms 1` が文に効く

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::Write;
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

const PASSWORD: &str = "correct-horse";
const TIMEOUT_MSG: &str = "canceling statement due to statement timeout";
/// 全行が不一致（`v` は 0..2500、`w` は常に 1,000,000）の非等価 ON JOIN。出力は 0 行。
const HEAVY: &str = "SELECT a.id FROM t a JOIN t b ON a.v > b.w";
const ROWS: usize = 2500;

fn seed(s: &mut TcpStream) {
    let ddl = run(
        s,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INTEGER, w INTEGER)",
    );
    assert!(error_of(&ddl).is_none(), "{ddl:?}");
    for chunk in (0..ROWS).collect::<Vec<_>>().chunks(50) {
        let values: Vec<String> = chunk
            .iter()
            .map(|i| format!("({i}, {i}, 1000000)"))
            .collect();
        let ins = run(
            s,
            &format!("INSERT INTO t (id, v, w) VALUES {}", values.join(", ")),
        );
        assert!(error_of(&ins).is_none(), "{:?}", error_of(&ins));
    }
}

fn setup(label: &str) -> (TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users_path = write_user_store_file(&[("alice", "tenant-a", PASSWORD)]);
    let addr = spawn_server_with_engine_ddl_users(&users_path, core, &["alice"]);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", PASSWORD);
    seed(&mut stream);
    (stream, guard)
}

/// ReadyForQuery までの全メッセージ（型, 本体）。
fn run(s: &mut TcpStream, sql: &str) -> Vec<(u8, Vec<u8>)> {
    send_simple_query(s, sql);
    let mut out = Vec::new();
    loop {
        let msg = read_message(s);
        let done = msg.0 == b'Z';
        out.push(msg);
        if done {
            return out;
        }
    }
}

fn error_of(msgs: &[(u8, Vec<u8>)]) -> Option<String> {
    msgs.iter()
        .find(|(t, _)| *t == b'E')
        .map(|(_, b)| String::from_utf8_lossy(b).to_string())
}

fn assert_timeout(msgs: &[(u8, Vec<u8>)]) {
    let e = error_of(msgs).expect("ErrorResponse expected");
    assert!(e.contains("C57014"), "{e:?}");
    assert!(e.contains(TIMEOUT_MSG), "{e:?}");
}

fn status_of(msgs: &[(u8, Vec<u8>)]) -> u8 {
    msgs.last().expect("ReadyForQuery").1[0]
}

#[test]
fn set_then_heavy_statement_times_out_and_zero_disables() {
    let (mut s, _g) = setup("wire1954-t1");
    run(&mut s, "SET statement_timeout = 1");
    assert_timeout(&run(&mut s, HEAVY));
    // 打ち切り後も接続は使え、無効化すれば同じ文が成功する。
    run(&mut s, "SET statement_timeout = 0");
    let ok = run(&mut s, HEAVY);
    assert!(error_of(&ok).is_none(), "{ok:?}");
}

#[test]
fn set_in_same_message_applies_to_following_statement() {
    let (mut s, _g) = setup("wire1954-t2");
    assert_timeout(&run(&mut s, &format!("SET statement_timeout = 1; {HEAVY}")));
}

#[test]
fn explicit_txn_fails_then_rolls_back() {
    let (mut s, _g) = setup("wire1954-t3");
    run(&mut s, "BEGIN");
    run(&mut s, "SET statement_timeout = 1");
    let r = run(&mut s, HEAVY);
    assert_timeout(&r);
    assert_eq!(status_of(&r), b'E');
    let next = run(&mut s, "SELECT a.id FROM t a WHERE a.id = 1");
    assert!(error_of(&next).expect("error").contains("C25P02"));
    let rb = run(&mut s, "ROLLBACK");
    assert_eq!(status_of(&rb), b'I');
    // ROLLBACK で SET も巻き戻り、無効（0）へ戻る。
    send_simple_query(&mut s, "SHOW statement_timeout");
    assert_eq!(read_row_description(&mut s), vec!["statement_timeout"]);
    assert_eq!(read_data_row(&mut s)[0].as_deref(), Some("0"));
    let _ = read_command_complete(&mut s);
    read_ready_for_query(&mut s);
}

#[test]
fn savepoint_recovers_after_timeout() {
    let (mut s, _g) = setup("wire1954-t4");
    run(&mut s, "BEGIN");
    run(&mut s, "SAVEPOINT a");
    run(&mut s, "SET statement_timeout = 1");
    assert_timeout(&run(&mut s, HEAVY));
    let rt = run(&mut s, "ROLLBACK TO a");
    assert!(error_of(&rt).is_none(), "{rt:?}");
    assert_eq!(status_of(&rt), b'T');
    // 値も巻き戻っているため、後続文は成功する（重い文でも期限なし）。
    let ok = run(&mut s, "SELECT a.id FROM t a WHERE a.id = 1");
    assert!(error_of(&ok).is_none(), "{ok:?}");
    run(&mut s, "ROLLBACK");
}

#[test]
fn extended_execute_times_out() {
    let (mut s, _g) = setup("wire1954-t5");
    run(&mut s, "SET statement_timeout = 1");
    send_length_prefixed_message(&mut s, b'P', &parse_body("", HEAVY, 0));
    send_length_prefixed_message(&mut s, b'B', &bind_body("", ""));
    send_length_prefixed_message(&mut s, b'E', &execute_body("", 0));
    send_length_prefixed_message(&mut s, b'S', &[]);
    let mut msgs = Vec::new();
    loop {
        let m = read_message(&mut s);
        let done = m.0 == b'Z';
        msgs.push(m);
        if done {
            break;
        }
    }
    assert_timeout(&msgs);
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
    assert!(output.status.success());
    let phc = String::from_utf8(output.stdout).expect("utf-8 phc");
    std::fs::write(path, format!("alice:tenant-a:{}\n", phc.trim())).expect("write user store");
}

#[test]
fn cli_default_applies_to_statements() {
    let fixture = TempFixtureDir::new("wire1954-t6");
    write_user_store_with_alice(&fixture.users_path_str());
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args([
            "--users",
            &fixture.users_path_str(),
            "--db",
            &fixture.db_path_str(),
            "--bind",
            "127.0.0.1:0",
            "--ddl-allowed-users",
            "alice",
            "--statement-timeout-ms",
            "1",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server");
    let (addr, _lines) = wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));
    let mut s = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    // 1ms の既定値の下でもシードの DDL・INSERT は通る規模にし、SET で一時的に無効化してから流し込む。
    run(&mut s, "SET statement_timeout = 0");
    seed(&mut s);
    run(&mut s, "RESET statement_timeout");
    assert_timeout(&run(&mut s, HEAVY));
    let _ = child.kill();
    let _ = child.wait();
}
