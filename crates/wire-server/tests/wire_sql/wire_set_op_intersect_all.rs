//! `INTERSECT ALL`・明示 `UNION DISTINCT`／`INTERSECT DISTINCT`（対象ビヘイビア: SQL-29。
//! Issue #3268）の簡易クエリプロトコル経由（生バイトクライアント）検証。
//!
//! 意味論は `crates/engine/tests/sql_t01_39/sql29_set_operations.rs` が確定オラクルとして
//! 検証済みのため、本ファイルは psql と同じ末尾 `;` 付きの単独 simple query が wire
//! フレーミング越しに通ること、拒否が SQLSTATE として公告されることの確認に徹する。

use crate::common;
use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-set-op-intersect-all");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    for name in ["a", "b"] {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                    ColumnDef::new("s", ColumnType::Text, false),
                ],
            ))
            .expect("create table");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn insert(stream: &mut std::net::TcpStream, table: &str, id: u32, s: &str) {
    send_simple_query(
        stream,
        &format!(
            "INSERT INTO {table} (id, embedding, s) VALUES ({id}, '[0.1,0.2]', '{s}') \
             USING OPERATION_ID 'op-{table}-{id}'"
        ),
    );
    assert_eq!(read_command_complete(stream), "INSERT 0 1");
    read_ready_for_query(stream);
}

fn expect_rows(stream: &mut std::net::TcpStream, sql: &str, want: &[&str]) {
    send_simple_query(stream, sql);
    let _ = read_row_description(stream);
    for v in want {
        let row = read_data_row(stream);
        assert_eq!(row[0].as_deref(), Some(*v), "{sql}");
    }
    assert_eq!(
        read_command_complete(stream),
        format!("SELECT {}", want.len())
    );
    read_ready_for_query(stream);
}

#[test]
fn wire_intersect_all_and_explicit_distinct_with_trailing_semicolon() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-alice", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    // a: x, x, y / b: x, z => INTERSECT ALL は x が min(2,1)=1 件。
    insert(&mut stream, "a", 1, "x");
    insert(&mut stream, "a", 2, "x");
    insert(&mut stream, "a", 3, "y");
    insert(&mut stream, "b", 1, "x");
    insert(&mut stream, "b", 2, "z");

    expect_rows(
        &mut stream,
        "SELECT s FROM a INTERSECT ALL SELECT s FROM b;",
        &["x"],
    );
    expect_rows(
        &mut stream,
        "SELECT s FROM a INTERSECT DISTINCT SELECT s FROM b;",
        &["x"],
    );
    expect_rows(
        &mut stream,
        "SELECT s FROM a UNION DISTINCT SELECT s FROM b;",
        &["x", "y", "z"],
    );
}

#[test]
fn wire_set_op_rejections_carry_sqlstate() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-alice", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    for (sql, code) in [
        ("SELECT s FROM a EXCEPT ALL SELECT s FROM b;", "42601"),
        (
            "SELECT s FROM a INTERSECT ALL DISTINCT SELECT s FROM b;",
            "42601",
        ),
        (
            "SELECT embedding FROM a INTERSECT ALL SELECT embedding FROM b;",
            "22000",
        ),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, code);
        read_ready_for_query(&mut stream);
    }
}
