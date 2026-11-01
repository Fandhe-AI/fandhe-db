//! `--write-lock-wait-ms` の解決結果（`write_lock_wait_opt::resolve`）を注入した
//! `EngineCore` が、wire 越しに書き込みロック待機超過を SQLSTATE `55P03` の
//! `ErrorResponse` として返し、接続が `ReadyForQuery` に戻ること、holder の解放後は
//! 通常どおり書き込めることを固定する（Issue #1554。ポインタ: SQL-46）。
//! `wire942_extended_transaction.rs` と同じ流儀。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

#[test]
fn write_lock_wait_timeout_is_55p03_over_wire_and_recovers_after_release() {
    let path = temp_db::unique_db_path("write-lock-wait-parity");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    let wait = wire_server::write_lock_wait_opt::resolve(Some("100")).expect("in range");
    let core = Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_write_lock_wait(wait),
    );
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut a = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut b = authenticate_to_ready_for_query(addr, "bob", "battery-staple");

    let insert = |id: u32| {
        format!(
            "INSERT INTO documents (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]') USING OPERATION_ID 'op-1554-{id}'"
        )
    };

    // A が BEGIN + INSERT で writer を握る。
    send_simple_query(&mut a, "BEGIN");
    assert_eq!(read_command_complete(&mut a), "BEGIN");
    read_ready_for_query(&mut a);
    send_simple_query(&mut a, &insert(1));
    assert_eq!(read_command_complete(&mut a), "INSERT 0 1");
    read_ready_for_query(&mut a);

    // B の autocommit INSERT は待機超過で 55P03、接続は ReadyForQuery に戻る。
    send_simple_query(&mut b, &insert(2));
    expect_error_response_with_sqlstate(&mut b, "55P03");
    read_ready_for_query(&mut b);

    // A が ROLLBACK した後は B の INSERT が成功する。
    send_simple_query(&mut a, "ROLLBACK");
    assert_eq!(read_command_complete(&mut a), "ROLLBACK");
    read_ready_for_query(&mut a);
    send_simple_query(&mut b, &insert(2));
    assert_eq!(read_command_complete(&mut b), "INSERT 0 1");
    read_ready_for_query(&mut b);
}

/// 書き込みが最後以外にある複数文メッセージ（暗黙トランザクション。WIRE-16・SQL-31）は、writer を
/// メッセージ先頭で取るため、writer が握られている間は先頭文の応答を送る前に `55P03` になる
/// （Issue #2401）。接続は `'I'` に戻り、明示の `BEGIN` は待たない。
#[test]
fn implicit_multi_statement_message_is_55p03_at_start_while_writer_is_held() {
    let path = temp_db::unique_db_path("write-lock-wait-implicit");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    let wait = wire_server::write_lock_wait_opt::resolve(Some("100")).expect("in range");
    let core = Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_write_lock_wait(wait),
    );
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut a = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut b = authenticate_to_ready_for_query(addr, "bob", "battery-staple");

    let insert = |id: u32| {
        format!(
            "INSERT INTO documents (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]') USING OPERATION_ID 'op-2401-{id}'"
        )
    };
    let message = format!("{}; {}", insert(2), insert(3));

    send_simple_query(&mut a, "BEGIN");
    assert_eq!(read_command_complete(&mut a), "BEGIN");
    read_ready_for_query(&mut a);
    send_simple_query(&mut a, &insert(1));
    assert_eq!(read_command_complete(&mut a), "INSERT 0 1");
    read_ready_for_query(&mut a);

    // 先頭文の CommandComplete より前に ErrorResponse が来る（1 文も実行されない）。
    send_simple_query(&mut b, &message);
    expect_error_response_with_sqlstate(&mut b, "55P03");
    assert_eq!(read_ready_for_query_status(&mut b), b'I');

    // 対照: 明示の BEGIN は writer を待たず成功する。
    send_simple_query(&mut b, "BEGIN");
    assert_eq!(read_command_complete(&mut b), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut b), b'T');
    send_simple_query(&mut b, "ROLLBACK");
    assert_eq!(read_command_complete(&mut b), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut b), b'I');

    send_simple_query(&mut a, "ROLLBACK");
    assert_eq!(read_command_complete(&mut a), "ROLLBACK");
    read_ready_for_query(&mut a);

    // 解放後は同じメッセージが成功する。
    send_simple_query(&mut b, &message);
    assert_eq!(read_command_complete(&mut b), "INSERT 0 1");
    assert_eq!(read_command_complete(&mut b), "INSERT 0 1");
    assert_eq!(read_ready_for_query_status(&mut b), b'I');
}

/// 件数・列を固定した共通 fixture（`documents` 表 1 つ・待機上限 `wait_ms`）。
fn new_core_for_2410(label: &str, wait_ms: &str) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    let wait = wire_server::write_lock_wait_opt::resolve(Some(wait_ms)).expect("in range");
    let core = Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_write_lock_wait(wait),
    );
    (core, guard)
}

fn insert_2410(id: u32) -> String {
    format!(
        "INSERT INTO documents (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]') USING OPERATION_ID 'op-2410-{id}'"
    )
}

const SELECT_2410: &str = "SELECT id FROM documents LIMIT 100";

/// `SELECT` 1 文ぶんの応答（`RowDescription`・`DataRow`・`CommandComplete`）を読み、
/// 先頭列のテキスト値を返す。
fn read_select_ids(stream: &mut std::net::TcpStream) -> Vec<String> {
    let _columns = read_row_description(stream);
    let mut ids = Vec::new();
    loop {
        let (kind, body) = read_message(stream);
        match kind {
            b'D' => {
                let len = i32::from_be_bytes(body[2..6].try_into().expect("len")) as usize;
                ids.push(String::from_utf8_lossy(&body[6..6 + len]).into_owned());
            }
            b'C' => return ids,
            other => panic!("unexpected message {}", other as char),
        }
    }
}

/// 暗黙トランザクション（書き込みが最後以外の複数文メッセージ。WIRE-16・SQL-31）の writer は
/// メッセージの開始時に取る（Issue #2410）。先頭が読み取りでも、writer が握られている間は
/// 先頭文の `RowDescription` より前に `55P03` になり、状態は `'I'` に戻る。
/// 明示トランザクションの遅延取得（最初の書き込み文で取る）との差を観測する。
#[test]
fn implicit_message_with_leading_read_is_55p03_before_any_row_description() {
    let (core, _guard) = new_core_for_2410("write-lock-wait-2410-leading-read", "100");
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut a = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut b = authenticate_to_ready_for_query(addr, "bob", "battery-staple");

    send_simple_query(&mut b, "BEGIN");
    assert_eq!(read_command_complete(&mut b), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut b), b'T');
    send_simple_query(&mut b, &insert_2410(1));
    assert_eq!(read_command_complete(&mut b), "INSERT 0 1");
    assert_eq!(read_ready_for_query_status(&mut b), b'T');

    let message = format!("{SELECT_2410}; {}; {SELECT_2410}", insert_2410(2));
    send_simple_query(&mut a, &message);
    let (kind, body) = read_message(&mut a);
    assert_eq!(kind, b'E', "first message must be ErrorResponse");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("55P03"), "{text:?}");
    assert!(
        !text.contains("tenant-b") && !text.contains("bob"),
        "{text:?}"
    );
    assert_eq!(read_ready_for_query_status(&mut a), b'I');

    send_simple_query(&mut b, "ROLLBACK");
    assert_eq!(read_command_complete(&mut b), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut b), b'I');

    // 解放後は同じメッセージが成功する。
    send_simple_query(&mut a, &message);
    assert!(read_select_ids(&mut a).is_empty());
    assert_eq!(read_command_complete(&mut a), "INSERT 0 1");
    assert_eq!(read_select_ids(&mut a), vec!["2".to_string()]);
    assert_eq!(read_ready_for_query_status(&mut a), b'I');
}

/// 対照: 書き込みが最後の複数文メッセージは暗黙トランザクションではなく順次実行のため、
/// 先頭の `SELECT` の応答が先に届き、その後の `INSERT` で `55P03` になる（Issue #2410）。
#[test]
fn sequential_message_with_trailing_write_runs_leading_read_before_55p03() {
    let (core, _guard) = new_core_for_2410("write-lock-wait-2410-sequential", "100");
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut a = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut b = authenticate_to_ready_for_query(addr, "bob", "battery-staple");

    send_simple_query(&mut b, "BEGIN");
    assert_eq!(read_command_complete(&mut b), "BEGIN");
    read_ready_for_query(&mut b);
    send_simple_query(&mut b, &insert_2410(1));
    assert_eq!(read_command_complete(&mut b), "INSERT 0 1");
    read_ready_for_query(&mut b);

    send_simple_query(&mut a, &format!("{SELECT_2410}; {}", insert_2410(2)));
    assert!(read_select_ids(&mut a).is_empty());
    expect_error_response_with_sqlstate(&mut a, "55P03");
    assert_eq!(read_ready_for_query_status(&mut a), b'I');

    send_simple_query(&mut b, "ROLLBACK");
    assert_eq!(read_command_complete(&mut b), "ROLLBACK");
    read_ready_for_query(&mut b);
}

/// 暗黙トランザクションは先頭文の実行前に writer を待ち、取得後にスナップショットを開く
/// （Issue #2410。`begin_with_origin` の暗黙腕の順序）。writer を握る同一テナントの別接続が
/// commit した後に先頭の `SELECT` がその行を読めること、待機中は応答が 1 バイトも届かないことを確認する。
#[test]
fn implicit_transaction_waits_before_first_statement_and_reads_after_acquiring_writer() {
    let (core, _guard) = new_core_for_2410("write-lock-wait-2410-snapshot", "5000");
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut holder = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut a = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut holder, "BEGIN");
    assert_eq!(read_command_complete(&mut holder), "BEGIN");
    read_ready_for_query(&mut holder);
    send_simple_query(&mut holder, &insert_2410(1));
    assert_eq!(read_command_complete(&mut holder), "INSERT 0 1");
    read_ready_for_query(&mut holder);

    let message = format!("{SELECT_2410}; {}; {SELECT_2410}", insert_2410(2));
    send_simple_query(&mut a, &message);
    std::thread::sleep(std::time::Duration::from_millis(300));
    a.set_read_timeout(Some(std::time::Duration::from_millis(50)))
        .expect("set timeout");
    let mut probe = [0u8; 1];
    let err = a
        .peek(&mut probe)
        .expect_err("no response while waiting for the writer");
    assert!(
        matches!(
            err.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
        "{err:?}"
    );
    a.set_read_timeout(None).expect("reset timeout");

    send_simple_query(&mut holder, "COMMIT");
    assert_eq!(read_command_complete(&mut holder), "COMMIT");
    assert_eq!(read_ready_for_query_status(&mut holder), b'I');

    assert_eq!(read_select_ids(&mut a), vec!["1".to_string()]);
    assert_eq!(read_command_complete(&mut a), "INSERT 0 1");
    let mut last = read_select_ids(&mut a);
    last.sort();
    assert_eq!(last, vec!["1".to_string(), "2".to_string()]);
    assert_eq!(read_ready_for_query_status(&mut a), b'I');
}
