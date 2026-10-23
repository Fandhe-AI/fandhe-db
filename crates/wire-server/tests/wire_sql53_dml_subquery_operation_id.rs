//! サブクエリつき述語形 `UPDATE`／`DELETE` の `USING OPERATION_ID` 再送冪等性の pg wire 経由検証
//! （SQL-53・RECOVER-11、Issue #2650）。
//!
//! 意味論の確定オラクルは `crates/engine/tests/sql53_dml_where_subquery.rs`。本ファイルは簡易・拡張
//! クエリの双方で、受理（影響件数の CommandComplete）・同一文の再送 `23505`・別文の再利用 `22023`・
//! 内側の表を変更した後の再送 `23505`・エラー後の接続回復を固定する。

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

const DEL: &str =
    "DELETE FROM docs WHERE lang IN (SELECT lang FROM langs LIMIT 10) USING OPERATION_ID 'w-1'";
const DEL_OTHER: &str =
    "DELETE FROM docs WHERE lang IN (SELECT lang FROM langs LIMIT 5) USING OPERATION_ID 'w-1'";
const UPD: &str = "UPDATE docs SET lang = 'xx' WHERE EXISTS (SELECT id FROM langs LIMIT 1) USING OPERATION_ID 'w-2'";
const UPD_OTHER: &str = "UPDATE docs SET lang = 'yy' WHERE EXISTS (SELECT id FROM langs LIMIT 1) USING OPERATION_ID 'w-2'";

fn start_server() -> (SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql53-dml-subquery-opid");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create docs");
    storage
        .create_table(&TableSchema::new(
            "langs",
            vec![ColumnDef::new("lang", ColumnType::Text, false)],
        ))
        .expect("create langs");
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

fn simple_ok(stream: &mut TcpStream, sql: &str) -> String {
    send_simple_query(stream, sql);
    let tag = read_command_complete(stream);
    read_ready_for_query(stream);
    tag
}

fn simple_err(stream: &mut TcpStream, sql: &str, sqlstate: &str) {
    send_simple_query(stream, sql);
    expect_error_response_with_sqlstate(stream, sqlstate);
    read_ready_for_query(stream);
}

fn seed(stream: &mut TcpStream) {
    for sql in [
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2]', 'ja') USING OPERATION_ID 'd1'",
        "INSERT INTO docs (id, embedding, lang) VALUES (2, '[0.1,0.2]', 'en') USING OPERATION_ID 'd2'",
        "INSERT INTO langs (id, lang) VALUES (1, 'ja') USING OPERATION_ID 'l1'",
    ] {
        simple_ok(stream, sql);
    }
}

/// 簡易クエリ: 受理 → 再送 23505 → 別文 22023 → 内側変更後の再送 23505。各エラー後に接続が回復する。
#[test]
fn wire_simple_query_subquery_dml_operation_id() {
    let (addr, _guard) = start_server();
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    seed(&mut stream);

    assert_eq!(simple_ok(&mut stream, DEL), "DELETE 1");
    simple_err(&mut stream, DEL, "23505");
    simple_err(&mut stream, DEL_OTHER, "22023");

    assert_eq!(simple_ok(&mut stream, UPD), "UPDATE 1");
    assert_eq!(
        simple_ok(&mut stream, "DELETE FROM langs WHERE lang = 'ja'"),
        "DELETE 1"
    );
    // 内側が 0 行になった後でも再送は 23505。
    simple_err(&mut stream, UPD, "23505");
    simple_err(&mut stream, UPD_OTHER, "22023");
}

fn extended_exec(stream: &mut TcpStream, name: &str, sql: &str) -> Result<String, String> {
    send_length_prefixed_message(stream, b'P', &parse_body(name, sql, 0));
    assert_eq!(read_message(stream).0, b'1', "ParseComplete");
    send_length_prefixed_message(stream, b'B', &bind_body("", name));
    let (kind, body) = read_message(stream);
    assert_eq!(
        kind,
        b'2',
        "BindComplete: {:?}",
        String::from_utf8_lossy(&body)
    );
    send_length_prefixed_message(stream, b'E', &execute_body("", 0));
    let (kind, body) = read_message(stream);
    let result = match kind {
        b'C' => Ok(String::from_utf8_lossy(&body[..body.len() - 1]).to_string()),
        b'E' => Err(String::from_utf8_lossy(&body).to_string()),
        other => panic!("unexpected message {other}"),
    };
    send_length_prefixed_message(stream, b'S', b"");
    assert_eq!(read_ready_for_query_status(stream), b'I');
    result
}

/// 拡張クエリ: Parse/Bind/Execute/Sync で受理され、再送 23505・別文 22023 になる。
#[test]
fn wire_extended_query_subquery_dml_operation_id() {
    let (addr, _guard) = start_server();
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    seed(&mut stream);

    assert_eq!(
        extended_exec(&mut stream, "s0", DEL).expect("first"),
        "DELETE 1"
    );
    let err = extended_exec(&mut stream, "s1", DEL).expect_err("resend");
    assert!(err.contains("C23505\0"), "{err:?}");
    let err = extended_exec(&mut stream, "s2", DEL_OTHER).expect_err("reuse");
    assert!(err.contains("C22023\0"), "{err:?}");
    // 内側の表を変更した後の再送も 23505。接続は回復している。
    assert_eq!(
        simple_ok(&mut stream, "DELETE FROM langs WHERE lang = 'ja'"),
        "DELETE 1"
    );
    let err = extended_exec(&mut stream, "s3", DEL).expect_err("resend after inner change");
    assert!(err.contains("C23505\0"), "{err:?}");
    assert_eq!(
        extended_exec(&mut stream, "s4", UPD).expect("update"),
        "UPDATE 0"
    );
}
