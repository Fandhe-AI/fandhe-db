//! `--require-where-for-dml`（Issue #2210・SQL-36 ポインタ）をバイナリ子プロセスとして起動し、
//! 値なしスイッチの受理・拒否（fail-closed）・起動ログ・`WHERE` 省略 DML の拒否（`42601`）を
//! 外形的に検証する結合テスト。`tests/wire_require_operation_id_cli.rs` と同じ流儀。
//!
//! - R1: 単体・`--surface nosql` との併用で `listening on` に到達する
//! - R2: 重複・`=` 連結形・後続の値は非 0 終了
//! - R3: 起動ログ行は指定時のみ、`listening on` より前に出る
//! - R4: 指定時は `WHERE` 省略の DELETE／UPDATE が `42601`、行は残り、明示した `WHERE` は成功する
//! - R5: 未指定（既定）では `WHERE` 省略の DELETE が成功する

#[path = "common/mod.rs"]
mod common;

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

const FLAG: &str = "--require-where-for-dml";

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

fn spawn(args: &[String]) -> std::process::Child {
    Command::new(env!("CARGO_BIN_EXE_wire-server"))
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn wire-server")
}

/// `alice:tenant-a:<phc>` のユーザーストアを書く（ハッシュは `hash-password`
/// サブコマンドで生成し、平文ハッシュをテストへ決め打ちしない）。
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

/// R1: 単体指定・`--surface nosql` 併用のいずれも `listening on` に到達する。
#[test]
fn flag_alone_and_with_nosql_surface_reach_listening() {
    for extra in [vec![FLAG], vec![FLAG, "--surface", "nosql"]] {
        let fixture = common::TempFixtureDir::new("w1");
        common::write_empty_user_store(&fixture.users_path_str());
        let mut args = base_args(&fixture);
        args.extend(extra.iter().map(|s| s.to_string()));
        let mut child = spawn(&args);
        let listening = common::wait_for_listening(&mut child, Duration::from_secs(10));
        let _ = child.kill();
        let _ = child.wait();
        assert!(listening, "args={extra:?}: must reach listening state");
    }
}

/// R2: 重複指定・`=` 連結形・後続の値はいずれも fail-closed。
#[test]
fn duplicate_or_valued_flag_is_rejected() {
    let fixture = common::TempFixtureDir::new("w2");
    common::write_empty_user_store(&fixture.users_path_str());
    let cases: [(&[&str], &str); 3] = [
        (&[FLAG, FLAG], "specified more than once"),
        (&["--require-where-for-dml=true"], "unknown argument"),
        (&[FLAG, "true"], "unknown argument"),
    ];
    for (extra_args, expected) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_wire-server"))
            .args(base_args(&fixture))
            .args(extra_args)
            .output()
            .expect("spawn wire-server");
        assert!(
            !output.status.success(),
            "args={extra_args:?}: expected non-zero exit"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(expected),
            "args={extra_args:?}: expected {expected:?} in stderr, got: {stderr}"
        );
    }
}

/// R3: 起動ログ行は指定時のみ出て、`listening on` より前に位置する。
#[test]
fn startup_notice_only_when_flag_given_and_before_listening() {
    for with_flag in [true, false] {
        let fixture = common::TempFixtureDir::new("w3");
        common::write_empty_user_store(&fixture.users_path_str());
        let mut args = base_args(&fixture);
        if with_flag {
            args.push(FLAG.to_string());
        }
        let mut child = spawn(&args);
        let (_addr, lines) =
            common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));
        let _ = child.kill();
        let _ = child.wait();
        let notice_pos = lines.iter().position(|l| l.contains(FLAG));
        let listening_pos = lines.iter().position(|l| l.contains("listening on"));
        if with_flag {
            let n = notice_pos.expect("notice line");
            assert!(
                listening_pos.is_none_or(|l| n < l),
                "notice must precede listening: {lines:?}"
            );
        } else {
            assert!(notice_pos.is_none(), "no notice expected: {lines:?}");
        }
    }
}

/// R4: 指定時は `WHERE` 省略の DELETE／UPDATE が `42601`。行は残り、明示した `WHERE` は成功する。
#[test]
fn flag_rejects_omitted_where_with_42601() {
    let fixture = common::TempFixtureDir::new("w4");
    write_user_store_with_alice(&fixture.users_path_str());
    let mut args = base_args(&fixture);
    args.extend(
        [FLAG, "--ddl-allowed-users", "alice"]
            .iter()
            .map(|s| s.to_string()),
    );
    let mut child = spawn(&args);
    let (addr, _lines) =
        common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));
    let mut stream = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    common::send_simple_query(
        &mut stream,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))",
    );
    let _ = common::read_command_complete(&mut stream);
    common::read_ready_for_query(&mut stream);

    common::send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding) VALUES (1, '[0.1,0.2,0.3]')",
    );
    assert_eq!(common::read_command_complete(&mut stream), "INSERT 0 1");
    common::read_ready_for_query(&mut stream);

    for sql in ["DELETE FROM docs", "UPDATE docs SET id = 2"] {
        common::send_simple_query(&mut stream, sql);
        common::expect_error_response_with_sqlstate(&mut stream, "42601");
        common::read_ready_for_query(&mut stream);
    }

    // 行が残っていること: 明示した WHERE の DELETE が 1 行に作用する。
    common::send_simple_query(&mut stream, "DELETE FROM docs WHERE id = 1");
    assert_eq!(common::read_command_complete(&mut stream), "DELETE 1");
    common::read_ready_for_query(&mut stream);

    let _ = child.kill();
    let _ = child.wait();
}

/// R5: フラグ未指定（既定）では `WHERE` 省略の DELETE が成功する（PostgreSQL 互換）。
#[test]
fn default_accepts_omitted_where() {
    let fixture = common::TempFixtureDir::new("w5");
    write_user_store_with_alice(&fixture.users_path_str());
    let mut args = base_args(&fixture);
    args.extend(
        ["--ddl-allowed-users", "alice"]
            .iter()
            .map(|s| s.to_string()),
    );
    let mut child = spawn(&args);
    let (addr, _lines) =
        common::wait_for_listening_addr_and_lines(&mut child, Duration::from_secs(10));
    let mut stream = common::authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    common::send_simple_query(
        &mut stream,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3))",
    );
    let _ = common::read_command_complete(&mut stream);
    common::read_ready_for_query(&mut stream);

    common::send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding) VALUES (1, '[0.1,0.2,0.3]')",
    );
    assert_eq!(common::read_command_complete(&mut stream), "INSERT 0 1");
    common::read_ready_for_query(&mut stream);

    common::send_simple_query(&mut stream, "DELETE FROM docs");
    assert_eq!(common::read_command_complete(&mut stream), "DELETE 1");
    common::read_ready_for_query(&mut stream);

    let _ = child.kill();
    let _ = child.wait();
}
