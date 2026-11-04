//! セッション変数 `hnsw.ef_search`（Issue #2030。ポインタ: SQL-73）の pg wire 結合テスト。
//!
//! `SET`／`RESET` のタグ（`SET`／`RESET`）と `SHOW` の行、不正値の `22023`・`SET LOCAL` の `0A000`
//! （入力値を反射しない）、Tx の `ROLLBACK` での巻き戻しを固定する。
//! 時間に依存しない。

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
    send_simple_query(s, "SHOW hnsw.ef_search");
    assert_eq!(read_row_description(s), vec!["hnsw.ef_search"]);
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
    let (mut s, _g) = setup("wire-ef-setting-simple");
    assert_eq!(show(&mut s), "64");
    set_ok(&mut s, "SET hnsw.ef_search = 200", "SET");
    assert_eq!(show(&mut s), "200");
    set_ok(&mut s, "SET hnsw.ef_search TO '300'", "SET");
    assert_eq!(show(&mut s), "300");
    set_ok(&mut s, "SET hnsw.ef_search TO DEFAULT", "SET");
    assert_eq!(show(&mut s), "64");
    set_ok(&mut s, "SET hnsw.ef_search = 9", "SET");
    set_ok(&mut s, "RESET hnsw.ef_search", "RESET");
    assert_eq!(show(&mut s), "64");
}

#[test]
fn invalid_values_and_set_local_are_rejected() {
    let (mut s, _g) = setup("wire-ef-setting-invalid");
    set_ok(&mut s, "SET hnsw.ef_search = 42", "SET");
    for (sql, code) in [
        ("SET hnsw.ef_search = 0", "22023"),
        ("SET hnsw.ef_search = 1001", "22023"),
        ("SET hnsw.ef_search = 'abcdef'", "22023"),
        ("SET LOCAL hnsw.ef_search = 1", "0A000"),
    ] {
        send_simple_query(&mut s, sql);
        let (t, body) = read_message(&mut s);
        assert_eq!(t, b'E', "{sql}");
        let text = String::from_utf8_lossy(&body).to_string();
        assert!(text.contains(&format!("C{code}")), "{sql}: {text:?}");
        assert!(
            !text.contains("abcdef") && !text.contains("1001"),
            "{text:?}"
        );
        read_ready_for_query(&mut s);
    }
    assert_eq!(show(&mut s), "42");
}

#[test]
fn set_is_rolled_back_with_transaction() {
    let (mut s, _g) = setup("wire-ef-setting-rollback");
    set_ok(&mut s, "BEGIN", "BEGIN");
    set_ok(&mut s, "SET hnsw.ef_search = 777", "SET");
    assert_eq!(show(&mut s), "777");
    set_ok(&mut s, "ROLLBACK", "ROLLBACK");
    assert_eq!(show(&mut s), "64");
}
