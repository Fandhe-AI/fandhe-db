//! `LOCK [TABLE]`（SQL-38・Issue #1958）の pg wire 結合テスト。簡易クエリ（'Q'）と拡張クエリ
//! （Parse／Bind／Execute／Sync）で、CommandComplete タグ `LOCK TABLE` と ReadyForQuery の
//! トランザクション状態（`I`→`T`→`I`）、トランザクション外の `25P01` を固定する。

use crate::common;

use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::Storage;

use common::*;

fn connect() -> (TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-lock-table");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let mut s = SessionState::default();
    s.allow_ddl();
    let ctx = PolicyContext::with_visibilities(
        "tenant-a",
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("tenant");
    core.execute_sql_in_session(&ctx, &mut s, "CREATE TABLE t (id BIGINT PRIMARY KEY)")
        .expect("create table");
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    (stream, guard)
}

#[test]
fn simple_query_lock_table_round_trip() {
    let (mut s, _g) = connect();

    send_simple_query(&mut s, "BEGIN");
    assert_eq!(read_command_complete(&mut s), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut s), b'T');

    send_simple_query(&mut s, "LOCK TABLE t IN ACCESS EXCLUSIVE MODE");
    assert_eq!(read_command_complete(&mut s), "LOCK TABLE");
    assert_eq!(read_ready_for_query_status(&mut s), b'T');

    send_simple_query(&mut s, "COMMIT");
    assert_eq!(read_command_complete(&mut s), "COMMIT");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn simple_query_lock_table_outside_transaction_is_25p01() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "LOCK TABLE t");
    expect_error_response_with_sqlstate(&mut s, "25P01");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn simple_query_lock_table_missing_table_fails_the_transaction() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "BEGIN");
    assert_eq!(read_command_complete(&mut s), "BEGIN");
    read_ready_for_query(&mut s);
    send_simple_query(&mut s, "LOCK TABLE nosuch NOWAIT");
    expect_error_response_with_sqlstate(&mut s, "42P01");
    assert_eq!(read_ready_for_query_status(&mut s), b'E');
    send_simple_query(&mut s, "ROLLBACK");
    assert_eq!(read_command_complete(&mut s), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn extended_query_lock_table_round_trip() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "BEGIN");
    assert_eq!(read_command_complete(&mut s), "BEGIN");
    read_ready_for_query(&mut s);

    send_length_prefixed_message(
        &mut s,
        b'P',
        &parse_body("", "LOCK TABLE t IN SHARE MODE", 0),
    );
    send_length_prefixed_message(&mut s, b'B', &bind_body("", ""));
    send_length_prefixed_message(&mut s, b'E', &execute_body("", 0));
    send_length_prefixed_message(&mut s, b'S', &[]);

    let mut tags = Vec::new();
    loop {
        let (t, body) = read_message(&mut s);
        match t {
            b'C' => tags.push(String::from_utf8_lossy(&body[..body.len() - 1]).to_string()),
            b'Z' => {
                assert_eq!(body.last().copied(), Some(b'T'));
                break;
            }
            b'1' | b'2' => {}
            other => panic!("unexpected message {}", other as char),
        }
    }
    assert_eq!(tags, vec!["LOCK TABLE".to_string()]);

    send_simple_query(&mut s, "ROLLBACK");
    assert_eq!(read_command_complete(&mut s), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}
