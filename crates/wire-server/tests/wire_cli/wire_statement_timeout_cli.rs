//! `--statement-timeout-ms`（Issue #1954。ポインタ: SQL-71・SQL-46）をバイナリ子プロセスとして起動し、
//! 起動拒否（fail-closed）と、既定値が各セッションの `SHOW`／`RESET` に反映されることを外形的に検証する。
//!
//! - R1: 値欠落・重複指定・範囲外・不正値は非 0 終了で、stderr にフラグ名を含む
//! - R2: 指定した既定値が `SHOW statement_timeout` に出て、`SET` で上書きでき、`RESET` で既定へ戻る
//! - R3: 未指定の既定は `0`
//!
//! 期限の発火（`57014`）は単独バイナリ `tests/wire1954_statement_timeout.rs` が担当する。

use crate::common;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

const FLAG: &str = "--statement-timeout-ms";

fn base_args(fixture: &common::TempFixtureDir) -> Vec<String> {
    vec![
        "--users".to_string(),
        fixture.users_path_str(),
        "--db".to_string(),
        fixture.db_path_str(),
        "--bind".to_string(),
        "127.0.0.1:0".to_string(),
    ]
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

/// R1: 欠落・重複・範囲外・不正値はいずれも起動エラー。
#[test]
fn invalid_flag_usage_is_rejected() {
    let fixture = common::TempFixtureDir::new("st1");
    common::write_empty_user_store(&fixture.users_path_str());
    let cases: [(&[&str], &str); 8] = [
        (&[FLAG], FLAG),
        (&[FLAG, "10", FLAG, "20"], "specified more than once"),
        (&[FLAG, "-1"], FLAG),
        (&[FLAG, "+5"], FLAG),
        (&[FLAG, "2147483648"], FLAG),
        (&[FLAG, "abc"], FLAG),
        (&[FLAG, ""], FLAG),
        (&[FLAG, "99999999999999999999999"], FLAG),
    ];
    for (extra, expected) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(base_args(&fixture))
            .args(extra)
            .output()
            .expect("spawn wire-server");
        assert!(!output.status.success(), "args={extra:?}: expected failure");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected),
            "args={extra:?}: expected {expected:?}, got: {stderr}"
        );
    }
}

fn show(stream: &mut std::net::TcpStream) -> String {
    common::send_simple_query(stream, "SHOW statement_timeout");
    let cols = common::read_row_description(stream);
    assert_eq!(cols, vec!["statement_timeout".to_string()]);
    let row = common::read_data_row(stream);
    let _ = common::read_command_complete(stream);
    common::read_ready_for_query(stream);
    row[0].clone().expect("non-null")
}

fn run_ok(stream: &mut std::net::TcpStream, sql: &str, tag: &str) {
    common::send_simple_query(stream, sql);
    assert_eq!(common::read_command_complete(stream), tag);
    common::read_ready_for_query(stream);
}

fn session_with(extra: &[&str], label: &str) -> (std::process::Child, std::net::TcpStream) {
    let fixture = Box::leak(Box::new(common::TempFixtureDir::new(label)));
    write_user_store_with_alice(&fixture.users_path_str());
    let mut args = base_args(fixture);
    args.extend(extra.iter().map(|s| s.to_string()));
    let mut child = Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server");
    let (addr, _lines) =
        common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));
    let stream = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    (child, stream)
}

/// R2: 既定値が SHOW に出て、SET で上書きでき、RESET で既定へ戻る。
#[test]
fn default_value_is_visible_overridable_and_reset_target() {
    let (mut child, mut s) = session_with(&[FLAG, "1500"], "st2");
    assert_eq!(show(&mut s), "1500ms");
    run_ok(&mut s, "SET statement_timeout = '5s'", "SET");
    assert_eq!(show(&mut s), "5s");
    run_ok(&mut s, "SET statement_timeout = 0", "SET");
    assert_eq!(show(&mut s), "0");
    run_ok(&mut s, "RESET statement_timeout", "RESET");
    assert_eq!(show(&mut s), "1500ms");
    let _ = child.kill();
    let _ = child.wait();
}

/// R3: 未指定の既定は `0`（無効）。
#[test]
fn unspecified_default_is_zero() {
    let (mut child, mut s) = session_with(&[], "st3");
    assert_eq!(show(&mut s), "0");
    let _ = child.kill();
    let _ = child.wait();
}
