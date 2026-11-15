//! 単純 CASE（`CASE x WHEN v THEN ... END`。SQL-26 のポインタのみ。Issue #3279）を、psql 同様に
//! 末尾 `;` 付きで単独送信する簡易クエリ（'Q'）と拡張クエリ（Parse／Bind／Execute）で固定する
//! 層 A 結合テスト。
//!
//! engine 側が決めた結果列型が RowDescription の型 OID・DataRow の text 表現として観測できること、
//! 不正な形・上限超過が ErrorResponse の SQLSTATE として届き、その後も接続が使えること
//! （fail-closed）を確認する。wire 層の本番コードは汎用経路のまま（変更なし）。

use crate::common;

use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

/// text 型の OID（PostgreSQL の `text`）。
const TEXT_OID: i32 = 25;

fn connect() -> (TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql26-simple-case");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, true),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    for (id, n) in [(1, "1"), (2, "2"), (3, "NULL")] {
        send_simple_query(
            &mut stream,
            &format!(
                "INSERT INTO t (id, embedding, n) VALUES ({id}, '[0.1,0.1]', {n}) \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        );
        read_command_complete(&mut stream);
        read_ready_for_query(&mut stream);
    }
    (stream, guard)
}

#[test]
fn simple_query_with_trailing_semicolon_returns_selected_branch() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "SELECT CASE 2 WHEN 1 THEN 'a' WHEN 2 THEN 'b' END;");
    assert_eq!(
        read_row_description_with_types(&mut s)
            .into_iter()
            .map(|(name, oid, _)| (name, oid))
            .collect::<Vec<_>>(),
        vec![("case".to_string(), TEXT_OID)]
    );
    assert_eq!(read_data_row(&mut s), vec![Some("b".to_string())]);
    assert_eq!(read_command_complete(&mut s), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');

    // NULL は一致しない（ELSE）。
    send_simple_query(&mut s, "SELECT CASE NULL WHEN NULL THEN 'a' ELSE 'b' END;");
    read_row_description_with_types(&mut s);
    assert_eq!(read_data_row(&mut s), vec![Some("b".to_string())]);
    assert_eq!(read_command_complete(&mut s), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn table_projection_where_and_order_by_use_the_simple_form() {
    let (mut s, _g) = connect();
    send_simple_query(
        &mut s,
        "SELECT id, CASE n WHEN 1 THEN 'one' WHEN 2 THEN 'two' ELSE 'other' END AS l \
         FROM t ORDER BY id;",
    );
    read_row_description_with_types(&mut s);
    assert_eq!(
        read_data_row(&mut s),
        vec![Some("1".to_string()), Some("one".to_string())]
    );
    assert_eq!(
        read_data_row(&mut s),
        vec![Some("2".to_string()), Some("two".to_string())]
    );
    // n が NULL の行はどの WHEN とも一致せず ELSE。
    assert_eq!(
        read_data_row(&mut s),
        vec![Some("3".to_string()), Some("other".to_string())]
    );
    assert_eq!(read_command_complete(&mut s), "SELECT 3");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');

    send_simple_query(
        &mut s,
        "SELECT id FROM t WHERE CASE n WHEN 2 THEN 1 ELSE 0 END = 1 ORDER BY id;",
    );
    read_row_description_with_types(&mut s);
    assert_eq!(read_data_row(&mut s), vec![Some("2".to_string())]);
    assert_eq!(read_command_complete(&mut s), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');

    // ORDER BY: n = 2 の行が先（0）、他は後（1）。同点は id 昇順。
    send_simple_query(
        &mut s,
        "SELECT id FROM t ORDER BY CASE n WHEN 2 THEN 0 ELSE 1 END, id;",
    );
    read_row_description_with_types(&mut s);
    for expected in ["2", "1", "3"] {
        assert_eq!(read_data_row(&mut s), vec![Some(expected.to_string())]);
    }
    assert_eq!(read_command_complete(&mut s), "SELECT 3");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

/// 拡張クエリ（Parse／Bind／Execute）でもパラメータなしの単純 CASE は簡易クエリと同じ結果になる。
#[test]
fn extended_query_without_parameters_executes_the_simple_form() {
    let (mut s, _g) = connect();
    send_length_prefixed_message(
        &mut s,
        b'P',
        &parse_body(
            "s1",
            "SELECT id FROM t WHERE CASE n WHEN 2 THEN 1 ELSE 0 END = 1 ORDER BY id",
            0,
        ),
    );
    send_length_prefixed_message(&mut s, b'B', &bind_body("p1", "s1"));
    send_length_prefixed_message(&mut s, b'E', &execute_body("p1", 0));
    send_length_prefixed_message(&mut s, b'S', b"");
    let (k, body) = read_message(&mut s);
    assert_eq!(k, b'1', "ParseComplete {}", String::from_utf8_lossy(&body));
    let (k, _) = read_message(&mut s);
    assert_eq!(k, b'2', "BindComplete");
    // n = 2 の行（id 2）だけが一致する。
    let (k, body) = read_message(&mut s);
    assert_eq!(k, b'D', "DataRow");
    assert!(body.ends_with(b"2"), "{body:?}");
    let (k, _) = read_message(&mut s);
    assert_eq!(k, b'C', "CommandComplete");
    let (k, body) = read_message(&mut s);
    assert_eq!(k, b'Z');
    assert_eq!(body, [b'I']);
}

/// `$n` を単純 CASE の被演算子・比較値に置く形は、パラメータ位置の許可リスト（`sql::params`）の
/// 外のため Parse で `42601`（fail-closed）になり、同期後も接続は使える。
#[test]
fn placeholder_in_simple_case_is_rejected_at_parse_and_connection_survives() {
    let (mut s, _g) = connect();
    send_length_prefixed_message(
        &mut s,
        b'P',
        &parse_body(
            "s2",
            "SELECT id FROM t WHERE CASE n WHEN $1 THEN 1 ELSE 0 END = 1",
            0,
        ),
    );
    send_length_prefixed_message(&mut s, b'S', b"");
    expect_error_response_with_sqlstate(&mut s, "42601");
    read_ready_for_query(&mut s);
    send_simple_query(&mut s, "SELECT CASE 1 WHEN 1 THEN 'ok' END;");
    read_row_description_with_types(&mut s);
    assert_eq!(read_data_row(&mut s), vec![Some("ok".to_string())]);
    assert_eq!(read_command_complete(&mut s), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn malformed_forms_fail_closed_and_keep_connection() {
    let (mut s, _g) = connect();
    for sql in [
        "SELECT CASE n END FROM t;",
        "SELECT CASE n WHEN 1 THEN 2 FROM t;",
        "SELECT CASE n WHEN 1, 2 THEN 3 END FROM t;",
    ] {
        send_simple_query(&mut s, sql);
        expect_error_response_with_sqlstate(&mut s, "42601");
        read_ready_for_query(&mut s);
    }
    // エラー後も同じ接続で通常どおり実行できる。
    send_simple_query(&mut s, "SELECT CASE 1 WHEN 1 THEN 'ok' END;");
    read_row_description_with_types(&mut s);
    assert_eq!(read_data_row(&mut s), vec![Some("ok".to_string())]);
    assert_eq!(read_command_complete(&mut s), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}
