//! `SELECT ... FOR UPDATE／SHARE ...` の行ロック句（SQL-38・Issue #2230）の pg wire 結合テスト。
//! 簡易クエリ（'Q'）の往復、拡張クエリ（Parse／Bind／Execute／Sync）、2 クライアント間の `NOWAIT` の
//! `55P03`（ReadyForQuery の状態は autocommit なら `I`）を固定する。時間計測は使わない。

use crate::common;

use crate::temp_db;

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::Storage;

use common::*;

/// 1 表を持つエンジンでサーバーを起動し、(接続先, 後始末) を返す。接続は呼び出し側が必要な数だけ張る。
fn start() -> (SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-row-lock");
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
    core.execute_sql_in_session(&ctx, &mut s, "INSERT INTO t (id) VALUES (1)")
        .expect("insert");
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    (addr, guard)
}

fn client(addr: SocketAddr) -> TcpStream {
    authenticate_to_ready_for_query(addr, "alice", "correct-horse")
}

#[test]
fn simple_query_row_lock_round_trip() {
    let (addr, _g) = start();
    let mut s = client(addr);

    send_simple_query(&mut s, "BEGIN");
    assert_eq!(read_command_complete(&mut s), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut s), b'T');

    send_simple_query(&mut s, "SELECT id FROM t WHERE id = 1 LIMIT 10 FOR UPDATE");
    assert_eq!(read_row_description(&mut s), vec!["id".to_string()]);
    assert_eq!(read_data_row(&mut s), vec![Some("1".to_string())]);
    assert_eq!(read_command_complete(&mut s), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut s), b'T');

    send_simple_query(&mut s, "COMMIT");
    assert_eq!(read_command_complete(&mut s), "COMMIT");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn simple_query_row_lock_in_autocommit_returns_rows() {
    let (addr, _g) = start();
    let mut s = client(addr);
    send_simple_query(&mut s, "SELECT id FROM t WHERE id = 1 LIMIT 10 FOR SHARE");
    assert_eq!(read_row_description(&mut s), vec!["id".to_string()]);
    assert_eq!(read_data_row(&mut s), vec![Some("1".to_string())]);
    assert_eq!(read_command_complete(&mut s), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

/// 1 つ目の接続が `BEGIN; ... FOR UPDATE` で保持している間、2 つ目の autocommit の `NOWAIT` は
/// `55P03`（ReadyForQuery は `I`）。1 つ目が COMMIT した後は成功する。
#[test]
fn nowait_from_second_client_is_55p03_until_first_commits() {
    let (addr, _g) = start();
    let mut first = client(addr);
    let mut second = client(addr);

    send_simple_query(&mut first, "BEGIN");
    assert_eq!(read_command_complete(&mut first), "BEGIN");
    read_ready_for_query(&mut first);
    send_simple_query(
        &mut first,
        "SELECT id FROM t WHERE id = 1 LIMIT 10 FOR UPDATE",
    );
    read_row_description(&mut first);
    read_data_row(&mut first);
    assert_eq!(read_command_complete(&mut first), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut first), b'T');

    let nowait = "SELECT id FROM t WHERE id = 1 LIMIT 10 FOR UPDATE NOWAIT";
    send_simple_query(&mut second, nowait);
    expect_error_response_with_sqlstate(&mut second, "55P03");
    assert_eq!(read_ready_for_query_status(&mut second), b'I');

    send_simple_query(&mut first, "COMMIT");
    assert_eq!(read_command_complete(&mut first), "COMMIT");
    assert_eq!(read_ready_for_query_status(&mut first), b'I');

    send_simple_query(&mut second, nowait);
    assert_eq!(read_row_description(&mut second), vec!["id".to_string()]);
    assert_eq!(read_data_row(&mut second), vec![Some("1".to_string())]);
    assert_eq!(read_command_complete(&mut second), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut second), b'I');
}

#[test]
fn skip_locked_is_0a000_over_the_wire() {
    let (addr, _g) = start();
    let mut s = client(addr);
    send_simple_query(
        &mut s,
        "SELECT id FROM t WHERE id = 1 LIMIT 10 FOR UPDATE SKIP LOCKED",
    );
    expect_error_response_with_sqlstate(&mut s, "0A000");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn extended_query_row_lock_round_trip() {
    let (addr, _g) = start();
    let mut s = client(addr);
    send_simple_query(&mut s, "BEGIN");
    assert_eq!(read_command_complete(&mut s), "BEGIN");
    read_ready_for_query(&mut s);

    send_length_prefixed_message(
        &mut s,
        b'P',
        &parse_body("", "SELECT id FROM t WHERE id = 1 LIMIT 10 FOR SHARE", 0),
    );
    send_length_prefixed_message(&mut s, b'B', &bind_body("", ""));
    send_length_prefixed_message(&mut s, b'E', &execute_body("", 0));
    send_length_prefixed_message(&mut s, b'S', &[]);

    let mut tags = Vec::new();
    let mut rows = 0;
    loop {
        let (t, body) = read_message(&mut s);
        match t {
            b'C' => tags.push(String::from_utf8_lossy(&body[..body.len() - 1]).to_string()),
            b'D' => rows += 1,
            b'Z' => {
                assert_eq!(body.last().copied(), Some(b'T'));
                break;
            }
            b'1' | b'2' => {}
            other => panic!("unexpected message {}", other as char),
        }
    }
    assert_eq!(tags, vec!["SELECT 1".to_string()]);
    assert_eq!(rows, 1);

    send_simple_query(&mut s, "ROLLBACK");
    assert_eq!(read_command_complete(&mut s), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}
