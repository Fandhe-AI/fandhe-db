//! セッション変数 `statement_timeout`（Issue #1954。ポインタ: SQL-71）の pg wire 結合テスト。
//!
//! 簡易クエリ・拡張クエリの両方で `SET`／`RESET` のタグ（`SET`／`RESET`）と `SHOW` の行、
//! 不正値の `22023`・`SET LOCAL` の `0A000`（入力値を反射しない）、Tx の `ROLLBACK` での巻き戻しを固定する。
//! 時間に依存しない。期限の発火は単独バイナリ `tests/wire1954_statement_timeout.rs`。

use crate::common;

use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

const PASSWORD: &str = "correct-horse";

fn setup(label: &str) -> (TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users_path = write_user_store_file(&[("alice", "tenant-a", PASSWORD)]);
    let addr = spawn_server_with_engine(&users_path, core);
    let stream = authenticate_to_ready_for_query(addr, "alice", PASSWORD);
    (stream, guard)
}

fn show(s: &mut TcpStream) -> String {
    send_simple_query(s, "SHOW statement_timeout");
    assert_eq!(read_row_description(s), vec!["statement_timeout"]);
    let row = read_data_row(s);
    assert_eq!(read_command_complete(s), "SHOW");
    read_ready_for_query(s);
    row[0].clone().expect("non-null")
}

fn set_ok(s: &mut TcpStream, sql: &str, tag: &str) {
    send_simple_query(s, sql);
    assert_eq!(read_command_complete(s), tag, "{sql}");
    read_ready_for_query(s);
}

#[test]
fn simple_query_set_show_reset_roundtrip() {
    let (mut s, _g) = setup("wire-st-setting-simple");
    assert_eq!(show(&mut s), "0");
    set_ok(&mut s, "SET statement_timeout = 1500", "SET");
    assert_eq!(show(&mut s), "1500ms");
    set_ok(&mut s, "SET statement_timeout TO '2min'", "SET");
    assert_eq!(show(&mut s), "2min");
    set_ok(&mut s, "SET statement_timeout TO DEFAULT", "SET");
    assert_eq!(show(&mut s), "0");
    set_ok(&mut s, "SET statement_timeout = 9", "SET");
    set_ok(&mut s, "RESET statement_timeout", "RESET");
    assert_eq!(show(&mut s), "0");
}

#[test]
fn invalid_values_and_set_local_are_rejected() {
    let (mut s, _g) = setup("wire-st-setting-invalid");
    set_ok(&mut s, "SET statement_timeout = 42", "SET");
    for (sql, code) in [
        ("SET statement_timeout = -1", "22023"),
        ("SET statement_timeout = 2147483648", "22023"),
        ("SET statement_timeout = 'abcdef'", "22023"),
        ("SET LOCAL statement_timeout = 1", "0A000"),
    ] {
        send_simple_query(&mut s, sql);
        let (t, body) = read_message(&mut s);
        assert_eq!(t, b'E', "{sql}");
        let text = String::from_utf8_lossy(&body).to_string();
        assert!(text.contains(&format!("C{code}")), "{sql}: {text:?}");
        assert!(
            !text.contains("abcdef") && !text.contains("2147483648"),
            "{text:?}"
        );
        read_ready_for_query(&mut s);
    }
    assert_eq!(show(&mut s), "42ms");
}

#[test]
fn set_is_rolled_back_with_transaction() {
    let (mut s, _g) = setup("wire-st-setting-rollback");
    set_ok(&mut s, "BEGIN", "BEGIN");
    set_ok(&mut s, "SET statement_timeout = 777", "SET");
    assert_eq!(show(&mut s), "777ms");
    set_ok(&mut s, "ROLLBACK", "ROLLBACK");
    assert_eq!(show(&mut s), "0");
}

/// 拡張クエリ: Parse／Bind／Execute／Sync の 1 往復で、完了メッセージのタグ文字列を返す。
fn extended_tag(s: &mut TcpStream, sql: &str) -> String {
    send_length_prefixed_message(s, b'P', &parse_body("", sql, 0));
    send_length_prefixed_message(s, b'B', &bind_body("", ""));
    send_length_prefixed_message(s, b'E', &execute_body("", 0));
    send_length_prefixed_message(s, b'S', &[]);
    let mut tag = None;
    loop {
        let (t, body) = read_message(s);
        match t {
            b'1' | b'2' | b'T' | b'D' => {}
            b'C' => {
                tag = Some(String::from_utf8_lossy(&body[..body.len() - 1]).to_string());
            }
            b'Z' => return tag.expect("CommandComplete"),
            other => panic!("unexpected message {} for {sql}", other as char),
        }
    }
}

#[test]
fn extended_query_set_show_reset() {
    let (mut s, _g) = setup("wire-st-setting-extended");
    assert_eq!(extended_tag(&mut s, "SET statement_timeout = '3s'"), "SET");
    assert_eq!(extended_tag(&mut s, "SHOW statement_timeout"), "SHOW");
    assert_eq!(show(&mut s), "3s");
    assert_eq!(extended_tag(&mut s, "RESET statement_timeout"), "RESET");
    assert_eq!(show(&mut s), "0");
}
