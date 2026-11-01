//! SQL-38 (7)(a)・Issue #1939: `SET TRANSACTION`／`SET SESSION CHARACTERISTICS AS TRANSACTION` の
//! 簡易クエリ経路の結合テスト。CommandComplete タグ `SET`・`ReadyForQuery` 状態バイト・SQLSTATE
//! （最初の問い合わせ後 `25001`・Tx 外 `25P01`）・暗黙トランザクションでの全体中止を固定する。

use crate::common;
use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("sql38-set-transaction-wire");
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
fn set_transaction_returns_set_tag_and_keeps_transaction_status() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    expect_tag(&mut stream, "BEGIN", "BEGIN", b'T');
    expect_tag(&mut stream, "SET TRANSACTION READ ONLY", "SET", b'T');
    send_simple_query(&mut stream, &insert_sql(1, "op-1"));
    expect_error_response_with_sqlstate(&mut stream, "25006");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');
    expect_tag(&mut stream, "ROLLBACK", "ROLLBACK", b'I');
    assert_eq!(count_rows(&mut stream), 0);
}

#[test]
fn set_transaction_after_a_query_is_25001_and_outside_is_25p01() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    send_simple_query(&mut stream, "SET TRANSACTION READ ONLY");
    expect_error_response_with_sqlstate(&mut stream, "25P01");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    expect_tag(&mut stream, "BEGIN", "BEGIN", b'T');
    expect_tag(&mut stream, &insert_sql(1, "op-1"), "INSERT 0 1", b'T');
    send_simple_query(&mut stream, "SET TRANSACTION READ ONLY");
    expect_error_response_with_sqlstate(&mut stream, "25001");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');
    expect_tag(&mut stream, "ROLLBACK", "ROLLBACK", b'I');
    assert_eq!(count_rows(&mut stream), 0);
}

#[test]
fn session_characteristics_are_inherited_by_later_begin() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    expect_tag(
        &mut stream,
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY",
        "SET",
        b'I',
    );
    expect_tag(&mut stream, "BEGIN", "BEGIN", b'T');
    send_simple_query(&mut stream, &insert_sql(1, "op-1"));
    expect_error_response_with_sqlstate(&mut stream, "25006");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');
    expect_tag(&mut stream, "ROLLBACK", "ROLLBACK", b'I');
}

#[test]
fn set_transaction_in_implicit_transaction_aborts_the_whole_message() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    let sql = format!(
        "SET TRANSACTION READ ONLY; {}; {}",
        insert_sql(1, "op-1"),
        insert_sql(2, "op-2")
    );
    send_simple_query(&mut stream, &sql);
    expect_error_response_with_sqlstate(&mut stream, "25P01");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_rows(&mut stream), 0);
}
