//! スカラーサブクエリの行数違反 `21000`（SQL-29・ERR-6、Issue #1432）の pg wire 経由検証。
//!
//! 意味論（WHERE 値位置・投影位置・入れ子の遅延の有無）は
//! `crates/engine/tests/sql29_projection_subquery.rs` と `sql29_subquery_scalar.rs` が
//! 確定オラクル。本ファイルは簡易クエリで `21000` の ErrorResponse が届き、その後
//! ReadyForQuery を受けて同一接続で後続文が成功する（接続回復）ことを固定する。
//! 拡張クエリプロトコルはサブクエリを含む文の Bind（投影列の導出）が `42601` で拒否される
//! 設計（`docs/design/sql-subquery.md` の対象外記述）のため Execute に到達せず `21000` は
//! 発生しない。その現行契約（`42601`・Sync 後の ReadyForQuery・接続回復）を固定する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

use common::*;

const WHERE_FORM: &str =
    "SELECT id FROM items WHERE qty = (SELECT qty FROM refs LIMIT 10) LIMIT 10";
const PROJECTION_FORM: &str = "SELECT name, (SELECT qty FROM refs LIMIT 10) FROM items LIMIT 10";
const NESTED_SOME: &str = "SELECT name, (SELECT name FROM refs WHERE qty = (SELECT qty FROM refs LIMIT 10) LIMIT 1) FROM items LIMIT 10";
const NESTED_NONE: &str = "SELECT name, (SELECT name FROM refs WHERE qty = (SELECT qty FROM refs LIMIT 10) LIMIT 1) FROM items WHERE qty > 999 LIMIT 10";
const RECOVERY: &str = "SELECT id FROM items WHERE qty = 10 LIMIT 10";

fn cols() -> Vec<ColumnDef> {
    vec![
        ColumnDef::new("embedding", ColumnType::Vector(2), false),
        ColumnDef::new("name", ColumnType::Text, true),
        ColumnDef::new("qty", ColumnType::BigInt, true),
    ]
}

fn start_server() -> (SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql29-scalar-subquery");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    for t in ["items", "refs"] {
        storage
            .create_table(&TableSchema::new(t, cols()))
            .expect("create table");
    }
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = Arc::new(UserStore::load_from_file(&users_path).expect("valid user store"));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let limiter = ConnectionLimiter::new(16);
    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            core,
            limiter,
            Duration::from_secs(5),
        );
    });
    (addr, guard)
}

fn seed(stream: &mut TcpStream) {
    let inserts = [
        "INSERT INTO items (id, embedding, name, qty) VALUES (1, '[0.1,0.2]', 'a', 10) USING OPERATION_ID 'i1'",
        "INSERT INTO items (id, embedding, name, qty) VALUES (2, '[0.1,0.2]', 'b', 20) USING OPERATION_ID 'i2'",
        "INSERT INTO refs (id, embedding, name, qty) VALUES (1, '[0.1,0.2]', 'x', 1) USING OPERATION_ID 'r1'",
        "INSERT INTO refs (id, embedding, name, qty) VALUES (2, '[0.1,0.2]', 'y', 2) USING OPERATION_ID 'r2'",
    ];
    for sql in inserts {
        send_simple_query(stream, sql);
        let _ = read_command_complete(stream);
        read_ready_for_query(stream);
    }
}

fn expect_simple_21000(stream: &mut TcpStream, sql: &str) {
    send_simple_query(stream, sql);
    expect_error_response_with_sqlstate(stream, "21000");
    read_ready_for_query(stream);
}

fn expect_simple_recovered(stream: &mut TcpStream) {
    send_simple_query(stream, RECOVERY);
    let _ = read_row_description(stream);
    let row = read_data_row(stream);
    assert_eq!(row, vec![Some("1".to_string())]);
    assert_eq!(read_command_complete(stream), "SELECT 1");
    read_ready_for_query(stream);
}

/// 簡易クエリ: 3 形で 21000、外側 0 行の入れ子は成功、各エラー後に接続が回復する。
#[test]
fn wire_simple_query_cardinality_violation_21000_and_recovery() {
    let (addr, _guard) = start_server();
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    seed(&mut stream);

    for sql in [WHERE_FORM, PROJECTION_FORM, NESTED_SOME] {
        expect_simple_21000(&mut stream, sql);
        expect_simple_recovered(&mut stream);
    }

    send_simple_query(&mut stream, NESTED_NONE);
    assert_eq!(read_row_description(&mut stream).len(), 2);
    assert_eq!(read_command_complete(&mut stream), "SELECT 0");
    read_ready_for_query(&mut stream);
    expect_simple_recovered(&mut stream);
}

fn parse_and_sync_recover_check(stream: &mut TcpStream, statement: &str, sql: &str) {
    send_length_prefixed_message(stream, b'P', &parse_body(statement, sql, 0));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'1', "expected ParseComplete for {sql}");
    send_length_prefixed_message(stream, b'B', &bind_body("", statement));
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'E', "expected ErrorResponse at Bind for {sql}");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("C42601\0"),
        "expected SQLSTATE 42601: {text:?}"
    );
    send_length_prefixed_message(stream, b'S', b"");
    assert_eq!(read_ready_for_query_status(stream), b'I');
}

/// 拡張クエリ: サブクエリ形は Bind で 42601、Sync で ReadyForQuery('I')、後続の拡張クエリも成功。
#[test]
fn wire_extended_query_subquery_forms_are_rejected_and_connection_recovers() {
    let (addr, _guard) = start_server();
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    seed(&mut stream);

    for (i, sql) in [WHERE_FORM, PROJECTION_FORM, NESTED_SOME, NESTED_NONE]
        .iter()
        .enumerate()
    {
        parse_and_sync_recover_check(&mut stream, &format!("s{i}"), sql);
    }

    // 回復: サブクエリを含まない拡張クエリが同一接続で成功する。
    send_length_prefixed_message(&mut stream, b'P', &parse_body("sr", RECOVERY, 0));
    assert_eq!(read_message(&mut stream).0, b'1');
    send_length_prefixed_message(&mut stream, b'B', &bind_body("pr", "sr"));
    assert_eq!(read_message(&mut stream).0, b'2');
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pr", 0));
    assert_eq!(read_message(&mut stream).0, b'D', "expected DataRow");
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    assert_eq!(String::from_utf8_lossy(&body[..body.len() - 1]), "SELECT 1");
    send_length_prefixed_message(&mut stream, b'S', b"");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}
