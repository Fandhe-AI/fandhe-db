//! トランザクション制御の別名 `START TRANSACTION`／`END`／`ABORT`（Issue #1930・SQL-69 (1)・
//! SQL-31）の簡易クエリ経路の結合テスト。CommandComplete タグ（`START TRANSACTION`／
//! `COMMIT`／`ROLLBACK`）・`ReadyForQuery` 状態バイト・失敗状態の扱い・複数文メッセージを固定する。
//! 拡張クエリ側は `wire942_extended_transaction.rs` に置く。

use crate::common;
use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("sql69-txn-aliases-wire");
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

fn count_rows(stream: &mut std::net::TcpStream) -> usize {
    send_simple_query(stream, "SELECT id FROM documents ORDER BY id LIMIT 10");
    let _ = read_row_description(stream);
    let mut n = 0;
    loop {
        let (kind, _) = read_message(stream);
        match kind {
            b'D' => n += 1,
            b'C' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    assert_eq!(read_ready_for_query_status(stream), b'I');
    n
}

#[test]
fn start_transaction_end_and_abort_tags_and_status() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    expect_tag(&mut stream, "START TRANSACTION", "START TRANSACTION", b'T');
    expect_tag(&mut stream, &insert_sql(1, "op-1"), "INSERT 0 1", b'T');
    expect_tag(&mut stream, "END", "COMMIT", b'I');
    assert_eq!(count_rows(&mut stream), 1);

    expect_tag(&mut stream, "START TRANSACTION", "START TRANSACTION", b'T');
    expect_tag(&mut stream, &insert_sql(2, "op-2"), "INSERT 0 1", b'T');
    expect_tag(&mut stream, "ABORT", "ROLLBACK", b'I');
    assert_eq!(count_rows(&mut stream), 1);
}

#[test]
fn end_in_failed_transaction_is_25p02_and_abort_recovers() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    expect_tag(&mut stream, "START TRANSACTION", "START TRANSACTION", b'T');
    send_simple_query(&mut stream, "SELEC id FROM documents");
    expect_error_response_with_sqlstate(&mut stream, "42601");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(&mut stream, "END");
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    expect_tag(&mut stream, "ABORT", "ROLLBACK", b'I');
}

#[test]
fn multi_statement_message_with_aliases_commits_atomically() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    let sql = format!(
        "START TRANSACTION; {}; {}; END",
        insert_sql(1, "op-1"),
        insert_sql(2, "op-2")
    );
    send_simple_query(&mut stream, &sql);
    for tag in ["START TRANSACTION", "INSERT 0 1", "INSERT 0 1", "COMMIT"] {
        assert_eq!(read_command_complete(&mut stream), tag);
    }
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_rows(&mut stream), 2);
}

#[test]
fn chain_and_prepare_transaction_are_0a000() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    send_simple_query(&mut stream, "PREPARE TRANSACTION 'x'");
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    expect_tag(&mut stream, "START TRANSACTION", "START TRANSACTION", b'T');
    send_simple_query(&mut stream, "END AND CHAIN");
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');
    expect_tag(&mut stream, "ABORT", "ROLLBACK", b'I');
}
