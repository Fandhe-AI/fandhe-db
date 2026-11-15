//! `LIKE ANY／ALL`（`ILIKE`・`NOT LIKE`・`~~` 系演算子を含む。対象ビヘイビア: SQL-9。関連: SQL-54・
//! TABLE-35。Issue #3258）の簡易クエリプロトコル経由（生バイトクライアント）検証。
//!
//! 意味論（三値論理・上限・テナント境界）は `crates/engine/tests/sql_named/sql_like_quantified.rs` が
//! 確定オラクルとして検証済みのため、本ファイルは psql と同じ末尾 `;` 付きの単独 simple query が
//! wire フレーミング越しに通ること、拒否が SQLSTATE として公告されることの確認に徹する。

use crate::common;
use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-like-quantified-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("s", ColumnType::Text, true),
                ColumnDef::new(
                    "arr",
                    ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
                    true,
                ),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn insert(stream: &mut std::net::TcpStream, id: u32, s: &str, arr: &str) {
    send_simple_query(
        stream,
        &format!(
            "INSERT INTO docs (id, embedding, s, arr) VALUES ({id}, '[0.1,0.2]', '{s}', '{arr}') \
             USING OPERATION_ID 'op-{id}'"
        ),
    );
    assert_eq!(read_command_complete(stream), "INSERT 0 1");
    read_ready_for_query(stream);
}

/// 末尾 `;` 付きの単独送信で行と CommandComplete が返る。
fn expect_ids(stream: &mut std::net::TcpStream, sql: &str, want: &[&str]) {
    send_simple_query(stream, sql);
    let _ = read_row_description(stream);
    for id in want {
        let row = read_data_row(stream);
        assert_eq!(row[0].as_deref(), Some(*id), "{sql}");
    }
    assert_eq!(
        read_command_complete(stream),
        format!("SELECT {}", want.len())
    );
    read_ready_for_query(stream);
}

#[test]
fn wire_like_any_all_and_tilde_operators_with_trailing_semicolon() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-alice", "pw-alice")]);
    let addrs = [spawn_server_with_engine(&users_path, core)];
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");
    insert(&mut stream, 1, "apple", "{a%,b%}");
    insert(&mut stream, 2, "Banana", "{b%}");

    expect_ids(
        &mut stream,
        "SELECT id FROM docs WHERE s LIKE ANY (ARRAY['a%','b%']) ORDER BY id LIMIT 10;",
        &["1"],
    );
    expect_ids(
        &mut stream,
        "SELECT id FROM docs WHERE s ILIKE ANY (arr) ORDER BY id LIMIT 10;",
        &["1", "2"],
    );
    expect_ids(
        &mut stream,
        "SELECT id FROM docs WHERE s NOT LIKE ALL (ARRAY['a%']) ORDER BY id LIMIT 10;",
        &["2"],
    );
    expect_ids(
        &mut stream,
        "SELECT id FROM docs WHERE s ~~ 'a%' ORDER BY id LIMIT 10;",
        &["1"],
    );
    expect_ids(
        &mut stream,
        "SELECT id FROM docs WHERE s !~~* ALL (ARRAY['b%']) ORDER BY id LIMIT 10;",
        &["1"],
    );
}

/// 要素数 1025 は `54000`、サブクエリ形は `0A000`、ESCAPE 付きは `42601` が ErrorResponse で返る。
#[test]
fn wire_like_quantified_rejections_carry_sqlstate() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-alice", "pw-alice")]);
    let addrs = [spawn_server_with_engine(&users_path, core)];
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    let literal = (0..1025)
        .map(|i| format!("p{i}%"))
        .collect::<Vec<_>>()
        .join(",");
    send_simple_query(
        &mut stream,
        &format!("SELECT id FROM docs WHERE s LIKE ANY ('{{{literal}}}') LIMIT 5;"),
    );
    expect_error_response_with_sqlstate(&mut stream, "54000");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs WHERE s LIKE ANY (SELECT arr FROM docs) LIMIT 5;",
    );
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs WHERE s LIKE ANY (arr) ESCAPE '!' LIMIT 5;",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}
