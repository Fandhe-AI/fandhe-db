//! 明示トランザクション内の名前付き portal を Sync を越えて保持する契約
//! （WIRE-11・SQL-31・TASK-221・Issue #1174）の層 A 結合テスト。
//!
//! `extended_query::handle_sync` は `BEGIN` 中（同一トランザクション世代で
//! Bind した名前付き portal のみ）を保持し、それ以外は全破棄する。Sync を
//! 越えた portal は `COMMIT`／`ROLLBACK`／abort で次メッセージ処理の先頭で
//! 失効する（`handshake::post_auth_loop`）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn connect() -> (TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire11-portal-txn-lifetime");
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
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    for id in 1..=2 {
        send_simple_query(
            &mut stream,
            &format!(
                "INSERT INTO documents (id, embedding, body) VALUES ({id}, '[0.1,0.2,0.3]', 'row') USING OPERATION_ID 'op-1174-{id}'"
            ),
        );
        assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
        read_ready_for_query(&mut stream);
    }
    (stream, guard)
}

fn simple(stream: &mut TcpStream, sql: &str, tag: &str) {
    send_simple_query(stream, sql);
    assert_eq!(read_command_complete(stream), tag);
    read_ready_for_query(stream);
}

fn sync_expect(stream: &mut TcpStream, status: u8) {
    send_length_prefixed_message(stream, b'S', b"");
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery");
    assert_eq!(body.last().copied(), Some(status));
}

fn parse(stream: &mut TcpStream, statement: &str, sql: &str) {
    send_length_prefixed_message(stream, b'P', &parse_body(statement, sql, 0));
    assert_eq!(read_message(stream).0, b'1', "expected ParseComplete");
}

fn bind(stream: &mut TcpStream, portal: &str, statement: &str) {
    send_length_prefixed_message(stream, b'B', &bind_body(portal, statement));
    assert_eq!(read_message(stream).0, b'2', "expected BindComplete");
}

fn execute_all(stream: &mut TcpStream, portal: &str, max_rows: i32) -> (usize, u8) {
    send_length_prefixed_message(stream, b'E', &execute_body(portal, max_rows));
    let mut rows = 0;
    loop {
        let (kind, _) = read_message(stream);
        match kind {
            b'D' => rows += 1,
            b'C' | b's' => return (rows, kind),
            other => panic!("unexpected message {:?}", other as char),
        }
    }
}

/// portal 不在で Execute が拒否され（行を送出せず）、Sync で回復する。
fn assert_execute_rejected(stream: &mut TcpStream, portal: &str, status: u8) {
    send_length_prefixed_message(stream, b'E', &execute_body(portal, 0));
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'E', "portal must not be usable");
    assert!(
        String::from_utf8_lossy(&body).contains("C08P01"),
        "expected 08P01 for a missing portal"
    );
    sync_expect(stream, status);
}

const SELECT: &str = "SELECT id FROM documents LIMIT 10";

#[test]
fn named_portal_survives_sync_inside_transaction_and_is_describable() {
    let (mut s, _g) = connect();
    simple(&mut s, "BEGIN", "BEGIN");
    parse(&mut s, "s1", SELECT);
    bind(&mut s, "p1", "s1");
    sync_expect(&mut s, b'T');

    send_length_prefixed_message(&mut s, b'D', b"Pp1\0");
    assert_eq!(read_message(&mut s).0, b'T', "expected RowDescription");
    assert_eq!(execute_all(&mut s, "p1", 0), (2, b'C'));
    sync_expect(&mut s, b'T');
    simple(&mut s, "ROLLBACK", "ROLLBACK");
}

#[test]
fn suspended_portal_resumes_across_sync_inside_transaction() {
    let (mut s, _g) = connect();
    simple(&mut s, "BEGIN", "BEGIN");
    parse(&mut s, "s1", SELECT);
    bind(&mut s, "p1", "s1");
    assert_eq!(execute_all(&mut s, "p1", 1), (1, b's'));
    sync_expect(&mut s, b'T');
    assert_eq!(execute_all(&mut s, "p1", 0), (1, b'C'));
    sync_expect(&mut s, b'T');
    simple(&mut s, "ROLLBACK", "ROLLBACK");
}

#[test]
fn portals_are_discarded_at_sync_outside_transaction() {
    let (mut s, _g) = connect();
    parse(&mut s, "s1", SELECT);
    bind(&mut s, "p1", "s1");
    sync_expect(&mut s, b'I');
    assert_execute_rejected(&mut s, "p1", b'I');
}

#[test]
fn unnamed_portal_is_discarded_at_sync_inside_transaction() {
    let (mut s, _g) = connect();
    simple(&mut s, "BEGIN", "BEGIN");
    parse(&mut s, "s1", SELECT);
    bind(&mut s, "", "s1");
    sync_expect(&mut s, b'T');
    // エラーで明示トランザクションは Failed（'E'）になる。
    assert_execute_rejected(&mut s, "", b'E');
    simple(&mut s, "ROLLBACK", "ROLLBACK");
}

#[test]
fn retained_portal_expires_after_commit_or_rollback() {
    for end in ["COMMIT", "ROLLBACK"] {
        let (mut s, _g) = connect();
        simple(&mut s, "BEGIN", "BEGIN");
        parse(&mut s, "s1", SELECT);
        bind(&mut s, "p1", "s1");
        sync_expect(&mut s, b'T');
        simple(&mut s, end, end);
        assert_execute_rejected(&mut s, "p1", b'I');
    }
}

#[test]
fn retained_portal_does_not_leak_into_next_transaction_and_name_is_reusable() {
    let (mut s, _g) = connect();
    simple(&mut s, "BEGIN", "BEGIN");
    parse(&mut s, "s1", SELECT);
    bind(&mut s, "p1", "s1");
    sync_expect(&mut s, b'T');
    simple(&mut s, "COMMIT", "COMMIT");
    simple(&mut s, "BEGIN", "BEGIN");
    // 失効済みの旧 portal が重複名判定に残らない。
    bind(&mut s, "p1", "s1");
    assert_eq!(execute_all(&mut s, "p1", 0), (2, b'C'));
    sync_expect(&mut s, b'T');
    simple(&mut s, "ROLLBACK", "ROLLBACK");
}

#[test]
fn portal_bound_while_idle_is_discarded_when_begin_happens_in_same_cycle() {
    let (mut s, _g) = connect();
    parse(&mut s, "s1", SELECT);
    bind(&mut s, "p1", "s1");
    parse(&mut s, "sb", "BEGIN");
    bind(&mut s, "pb", "sb");
    assert_eq!(execute_all(&mut s, "pb", 0), (0, b'C'));
    sync_expect(&mut s, b'T');
    assert_execute_rejected(&mut s, "p1", b'E');
    simple(&mut s, "ROLLBACK", "ROLLBACK");
}

#[test]
fn retained_portal_is_discarded_by_extended_commit() {
    let (mut s, _g) = connect();
    simple(&mut s, "BEGIN", "BEGIN");
    parse(&mut s, "s1", SELECT);
    bind(&mut s, "p1", "s1");
    sync_expect(&mut s, b'T');
    parse(&mut s, "sc", "COMMIT");
    bind(&mut s, "pc", "sc");
    assert_eq!(execute_all(&mut s, "pc", 0), (0, b'C'));
    sync_expect(&mut s, b'I');
    assert_execute_rejected(&mut s, "p1", b'I');
}
