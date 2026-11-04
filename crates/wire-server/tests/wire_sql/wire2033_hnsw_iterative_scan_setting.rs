//! セッション変数 `hnsw.iterative_scan`・`hnsw.max_scan_tuples`（Issue #2033。ポインタ: SQL-73）の pg wire 結合テスト。
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

fn show(s: &mut TcpStream, name: &str) -> String {
    send_simple_query(s, &format!("SHOW {name}"));
    assert_eq!(read_row_description(s), vec![name.to_string()]);
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
    let (mut s, _g) = setup("wire-scan-setting-simple");
    assert_eq!(show(&mut s, "hnsw.iterative_scan"), "strict_order");
    assert_eq!(show(&mut s, "hnsw.max_scan_tuples"), "20000");
    set_ok(&mut s, "SET hnsw.iterative_scan = off", "SET");
    set_ok(&mut s, "SET hnsw.max_scan_tuples TO '300'", "SET");
    assert_eq!(show(&mut s, "hnsw.iterative_scan"), "off");
    assert_eq!(show(&mut s, "hnsw.max_scan_tuples"), "300");
    set_ok(&mut s, "RESET hnsw.iterative_scan", "RESET");
    set_ok(&mut s, "SET hnsw.max_scan_tuples TO DEFAULT", "SET");
    assert_eq!(show(&mut s, "hnsw.iterative_scan"), "strict_order");
    assert_eq!(show(&mut s, "hnsw.max_scan_tuples"), "20000");
}

#[test]
fn invalid_values_and_set_local_are_rejected() {
    let (mut s, _g) = setup("wire-scan-setting-invalid");
    set_ok(&mut s, "SET hnsw.max_scan_tuples = 42", "SET");
    for (sql, code) in [
        ("SET hnsw.iterative_scan = abcdef", "22023"),
        ("SET hnsw.max_scan_tuples = 0", "22023"),
        ("SET hnsw.max_scan_tuples = 2147483648", "22023"),
        ("SET LOCAL hnsw.iterative_scan = off", "0A000"),
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
    assert_eq!(show(&mut s, "hnsw.max_scan_tuples"), "42");
}

#[test]
fn set_is_rolled_back_with_transaction() {
    let (mut s, _g) = setup("wire-scan-setting-rollback");
    set_ok(&mut s, "BEGIN", "BEGIN");
    set_ok(&mut s, "SET hnsw.iterative_scan = off", "SET");
    assert_eq!(show(&mut s, "hnsw.iterative_scan"), "off");
    set_ok(&mut s, "ROLLBACK", "ROLLBACK");
    assert_eq!(show(&mut s, "hnsw.iterative_scan"), "strict_order");
}
