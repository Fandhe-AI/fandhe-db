//! `SAVEPOINT`／`RELEASE`／`ROLLBACK TO`（Issue #1942・SQL-69 のポインタ）の簡易クエリ経路の
//! 結合テスト。CommandComplete タグ（`SAVEPOINT`／`RELEASE`）・`ReadyForQuery` 状態バイト・
//! `ROLLBACK TO` の成功（タグ `ROLLBACK`・状態 `T`。Issue #1943）、書き込み後に作った savepoint の
//! fail-closed（`0A000`＋`E`）・Idle での `25P01` を固定する。

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

const SELECT_SQL: &str = "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 10";

/// 0 行の結果を想定し、`SELECT n` タグを読む（DataRow が来れば `read_command_complete` が失敗する）。
fn select_count(stream: &mut std::net::TcpStream, status: u8) -> String {
    send_simple_query(stream, SELECT_SQL);
    let _ = read_row_description(stream);
    let tag = read_command_complete(stream);
    assert_eq!(read_ready_for_query_status(stream), status);
    tag
}

#[test]
fn rollback_to_restores_state_and_hides_writes() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    expect_tag(&mut stream, "BEGIN", "BEGIN", b'T');
    expect_tag(&mut stream, "SAVEPOINT a", "SAVEPOINT", b'T');
    expect_tag(&mut stream, &insert_sql(1, "op-1"), "INSERT 0 1", b'T');
    expect_tag(&mut stream, "ROLLBACK TO a", "ROLLBACK", b'T');
    assert_eq!(select_count(&mut stream, b'T'), "SELECT 0");
    expect_tag(&mut stream, "COMMIT", "COMMIT", b'I');
    assert_eq!(select_count(&mut stream, b'I'), "SELECT 0");
}

#[test]
fn rollback_to_savepoint_after_writes_is_0a000_and_leaves_failed_state() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    expect_tag(&mut stream, "BEGIN", "BEGIN", b'T');
    expect_tag(&mut stream, &insert_sql(1, "op-1"), "INSERT 0 1", b'T');
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
/// Issue #1947（SQL-31・SQL-69）: savepoint を作った後に失敗した Tx は、簡易クエリの
/// `ROLLBACK TO` で `'E'` から `'T'` へ復帰し、継続して `COMMIT` できる。
/// 失敗前の書き込みは取り消され、復帰後の書き込みだけが残る。
#[test]
fn failed_transaction_recovers_with_rollback_to_and_commits() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    expect_tag(&mut stream, "BEGIN", "BEGIN", b'T');
    expect_tag(&mut stream, "SAVEPOINT a", "SAVEPOINT", b'T');
    expect_tag(&mut stream, &insert_sql(1, "op-1"), "INSERT 0 1", b'T');
    send_simple_query(&mut stream, "SELEC nonsense");
    expect_error_response_with_sqlstate(&mut stream, "42601");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    // 失敗中は ROLLBACK TO 以外 25P02。
    send_simple_query(&mut stream, "COMMIT");
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    expect_tag(&mut stream, "ROLLBACK TO a", "ROLLBACK", b'T');
    expect_tag(&mut stream, &insert_sql(3, "op-3"), "INSERT 0 1", b'T');
    expect_tag(&mut stream, "COMMIT", "COMMIT", b'I');
    // 確定したのは復帰後の 1 行（id=3）だけ。
    send_simple_query(&mut stream, SELECT_SQL);
    let _ = read_row_description(&mut stream);
    let _ = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// savepoint を作る前に失敗した Tx は復帰できない（`ROLLBACK` のみ）。
#[test]
fn failure_before_any_savepoint_cannot_recover_with_rollback_to() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    expect_tag(&mut stream, "BEGIN", "BEGIN", b'T');
    send_simple_query(&mut stream, "SELEC nonsense");
    expect_error_response_with_sqlstate(&mut stream, "42601");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');
    send_simple_query(&mut stream, "ROLLBACK TO a");
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');
    expect_tag(&mut stream, "ROLLBACK", "ROLLBACK", b'I');
}
