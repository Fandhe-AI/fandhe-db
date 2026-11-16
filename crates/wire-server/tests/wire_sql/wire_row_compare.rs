//! WHERE の行値比較 `(a, b) <op> (c, d)`（対象ビヘイビア: SQL-9。Issue #3256）の簡易クエリ
//! プロトコル経由（生バイトクライアント）検証。
//!
//! 意味論（辞書式・三値論理・テナント境界）は `crates/engine/tests/sql_named/sql_where_row_compare.rs` が
//! 確定オラクルとして検証済みのため、本ファイルは psql と同じ末尾 `;` 付きの単独 simple query が
//! wire フレーミング越しに行を返すこと、拒否が SQLSTATE として公告され接続が継続することの確認に徹する。

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use crate::common::*;
use crate::temp_db;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-row-compare-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("a", ColumnType::Integer, true),
                ColumnDef::new("b", ColumnType::Integer, true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn insert(stream: &mut std::net::TcpStream, id: u32, a: &str, b: &str) {
    send_simple_query(
        stream,
        &format!(
            "INSERT INTO docs (id, embedding, a, b) VALUES ({id}, '[0.1,0.2]', {a}, {b}) \
             USING OPERATION_ID 'op-{id}'"
        ),
    );
    assert_eq!(read_command_complete(stream), "INSERT 0 1");
    read_ready_for_query(stream);
}

fn expect_ids(stream: &mut std::net::TcpStream, sql: &str, want: &[&str]) {
    send_simple_query(stream, sql);
    let _ = read_row_description(stream);
    for id in want {
        let row = read_data_row(stream);
        assert_eq!(row.first().and_then(|v| v.as_deref()), Some(*id), "{sql}");
    }
    assert_eq!(
        read_command_complete(stream),
        format!("SELECT {}", want.len())
    );
    read_ready_for_query(stream);
}

/// 末尾 `;` 付きの単独送信で、行値比較（`<`・`=`・`<>`）が行を返す。NULL 成分の行は UNKNOWN で出ない。
#[test]
fn wire_row_compare_with_trailing_semicolon() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-alice", "pw-alice")]);
    let addrs = [spawn_server_with_engine(&users_path, core)];
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");
    insert(&mut stream, 1, "1", "1");
    insert(&mut stream, 2, "3", "5");
    insert(&mut stream, 3, "3", "NULL");

    expect_ids(
        &mut stream,
        "SELECT id FROM docs WHERE (a, b) < (3, 5) ORDER BY id LIMIT 10;",
        &["1"],
    );
    expect_ids(
        &mut stream,
        "SELECT id FROM docs WHERE (a, b) = (3, 5) ORDER BY id LIMIT 10;",
        &["2"],
    );
    expect_ids(
        &mut stream,
        "SELECT id FROM docs WHERE (a, b) <> (3, 5) ORDER BY id LIMIT 10;",
        &["1"],
    );
}

/// 成分数の不一致は `42601` の ErrorResponse の後に ReadyForQuery が返り、接続は継続する。
#[test]
fn wire_row_compare_malformed_carries_sqlstate() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-alice", "pw-alice")]);
    let addrs = [spawn_server_with_engine(&users_path, core)];
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs WHERE (a, b) < (1, 2, 3) LIMIT 5;",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}
