//! `SAVEPOINT`／`RELEASE`／`ROLLBACK TO`（Issue #1942・SQL-69 のポインタ）の簡易クエリ経路の
//! 結合テスト。CommandComplete タグ（`SAVEPOINT`／`RELEASE`）・`ReadyForQuery` 状態バイト・
//! 巻き戻し未実装の `ROLLBACK TO` の fail-closed（`0A000`＋`E`）・Idle での `25P01` を固定する。

use crate::common;
use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("sql69-savepoint-wire");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

fn insert_sql(id: u64, op: &str) -> String {
    format!(
        "INSERT INTO documents (id, embedding, body) VALUES ({id}, '[0.1,0.2,0.3]', 'row') USING OPERATION_ID '{op}'"
    )
}

fn connect(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "correct-horse")
}

fn expect_tag(stream: &mut std::net::TcpStream, sql: &str, tag: &str, status: u8) {
    send_simple_query(stream, sql);
    assert_eq!(read_command_complete(stream), tag, "{sql}");
    assert_eq!(read_ready_for_query_status(stream), status, "{sql}");
}

#[test]
fn savepoint_and_release_tags_and_status() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    expect_tag(&mut stream, "BEGIN", "BEGIN", b'T');
    expect_tag(&mut stream, &insert_sql(1, "op-1"), "INSERT 0 1", b'T');
    expect_tag(&mut stream, "SAVEPOINT a", "SAVEPOINT", b'T');
    expect_tag(&mut stream, "RELEASE SAVEPOINT a", "RELEASE", b'T');
    expect_tag(&mut stream, "COMMIT", "COMMIT", b'I');
}

#[test]
fn rollback_to_is_0a000_and_leaves_failed_state() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    expect_tag(&mut stream, "BEGIN", "BEGIN", b'T');
    expect_tag(&mut stream, "SAVEPOINT a", "SAVEPOINT", b'T');
    send_simple_query(&mut stream, "ROLLBACK TO a");
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(&mut stream, "ROLLBACK TO a");
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');
    expect_tag(&mut stream, "ROLLBACK", "ROLLBACK", b'I');
}

#[test]
fn savepoint_outside_transaction_is_25p01_and_idle() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    send_simple_query(&mut stream, "SAVEPOINT a");
    expect_error_response_with_sqlstate(&mut stream, "25P01");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

#[test]
fn release_unknown_savepoint_is_3b001() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    expect_tag(&mut stream, "BEGIN", "BEGIN", b'T');
    send_simple_query(&mut stream, "RELEASE nope");
    expect_error_response_with_sqlstate(&mut stream, "3B001");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');
    expect_tag(&mut stream, "ROLLBACK", "ROLLBACK", b'I');
}
