//! 集合演算の枝での式射影・定数枝（対象ビヘイビア: SQL-42。Issue #3270）の簡易クエリプロトコル経由
//! （生バイトクライアント）検証。
//!
//! 意味論は `crates/engine/tests/sql_t40_up/sql42_set_op_expr_projection.rs` が確定オラクルとして
//! 検証済みのため、本ファイルは psql と同じ末尾 `;` 付きの単独 simple query が wire
//! フレーミング越しに通ること、拒否が SQLSTATE として公告され接続が続くことの確認に徹する。

use crate::common;
use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-set-op-expr");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "b",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("y", ColumnType::BigInt, false),
                ColumnDef::new("s", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn insert(stream: &mut std::net::TcpStream, id: u32, y: i64, s: &str) {
    send_simple_query(
        stream,
        &format!(
            "INSERT INTO b (id, embedding, y, s) VALUES ({id}, '[0.1,0.2]', {y}, '{s}') \
             USING OPERATION_ID 'op-b-{id}'"
        ),
    );
    assert_eq!(read_command_complete(stream), "INSERT 0 1");
    read_ready_for_query(stream);
}

/// 先頭列の値を集めて昇順に並べ、期待値と比べる（集合演算の出力順は仕様外）。
fn expect_sorted_rows(stream: &mut std::net::TcpStream, sql: &str, want: &[&str]) {
    send_simple_query(stream, sql);
    let _ = read_row_description(stream);
    let mut got: Vec<String> = Vec::new();
    for _ in 0..want.len() {
        let row = read_data_row(stream);
        got.push(row[0].clone().expect("non-null cell"));
    }
    got.sort();
    let mut want_sorted: Vec<String> = want.iter().map(|s| s.to_string()).collect();
    want_sorted.sort();
    assert_eq!(got, want_sorted, "{sql}");
    assert_eq!(
        read_command_complete(stream),
        format!("SELECT {}", want.len())
    );
    read_ready_for_query(stream);
}

#[test]
fn wire_constant_and_expression_branches_with_trailing_semicolon() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-alice", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    insert(&mut stream, 1, 3, "x");
    insert(&mut stream, 2, 4, "y");

    // 受入例: 左が定数枝。
    expect_sorted_rows(
        &mut stream,
        "SELECT 7 UNION SELECT y FROM b;",
        &["3", "4", "7"],
    );
    expect_sorted_rows(
        &mut stream,
        "SELECT y+1 FROM b UNION ALL SELECT y FROM b;",
        &["3", "4", "4", "5"],
    );
    expect_sorted_rows(
        &mut stream,
        "SELECT upper(s) FROM b UNION SELECT s FROM b;",
        &["X", "Y", "x", "y"],
    );
}

#[test]
fn wire_set_op_expression_rejections_carry_sqlstate_and_keep_connection() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-alice", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    insert(&mut stream, 1, 3, "x");

    for (sql, code) in [
        // 型の不一致。
        ("SELECT 7 UNION SELECT s FROM b;", "42804"),
        // 全枝が定数の集合演算は未対応。
        ("SELECT 1 UNION SELECT 2;", "0A000"),
        // 定数枝の集計・WHERE。
        ("SELECT count(*) UNION SELECT y FROM b;", "42601"),
        ("SELECT 7 WHERE true UNION SELECT y FROM b;", "42601"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, code);
        read_ready_for_query(&mut stream);
    }
    // 拒否の後も接続は使える。
    expect_sorted_rows(&mut stream, "SELECT 7 UNION SELECT y FROM b;", &["3", "7"]);
}
