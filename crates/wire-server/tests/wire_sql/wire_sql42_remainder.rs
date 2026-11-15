//! `%`（剰余）演算子（SQL-42 のポインタのみ。Issue #3273、親 #3272）を、psql 同様に末尾 `;` 付きで
//! 単独送信する簡易クエリ（'Q'）と拡張クエリ（Parse／Describe）で固定する層 A 結合テスト。
//!
//! engine 側が決めた結果列型（整数の剰余は int4／int8、PostgreSQL 17 準拠）が RowDescription の
//! 型 OID・DataRow の text 表現として観測できること、0 除算（22012）・float 型被演算子（42883）が
//! ErrorResponse の SQLSTATE として届き、その後も接続が使えること（fail-closed）を確認する。
//! wire 層の本番コードは汎用経路のまま（変更なし）。

use crate::common;

use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn connect() -> (TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql42-remainder");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, false),
                ColumnDef::new("b", ColumnType::BigInt, false),
                ColumnDef::new("r", ColumnType::Real, false),
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
    send_simple_query(
        &mut stream,
        "INSERT INTO t (id, embedding, n, b, r) VALUES (1, '[0.1,0.1]', -7, 10, 2.5) \
         USING OPERATION_ID 'seed-1'",
    );
    read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
    (stream, guard)
}

fn expect_one_value(s: &mut TcpStream, sql: &str, oid: i32, len: i16, text: &str) {
    send_simple_query(s, sql);
    assert_eq!(
        read_row_description_with_types(s),
        vec![("?column?".to_string(), oid, len)],
        "{sql}"
    );
    assert_eq!(read_data_row(s), vec![Some(text.to_string())], "{sql}");
    assert_eq!(read_command_complete(s), "SELECT 1", "{sql}");
    assert_eq!(read_ready_for_query_status(s), b'I');
}

#[test]
fn simple_query_with_trailing_semicolon_returns_integer_remainder() {
    let (mut s, _g) = connect();
    // 符号は被除数に従う（PostgreSQL 17: -7 % 3 = -1）。
    expect_one_value(&mut s, "SELECT 7 % 3;", 23, 4, "1");
    expect_one_value(&mut s, "SELECT n % 3 FROM t;", 23, 4, "-1");
    expect_one_value(&mut s, "SELECT b % 4 FROM t;", 20, 8, "2");
    expect_one_value(&mut s, "SELECT n % b FROM t;", 20, 8, "-7");
}

#[test]
fn division_by_zero_and_float_operands_fail_closed_and_keep_connection() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "SELECT 1 % 0;");
    expect_error_response_with_sqlstate(&mut s, "22012");
    read_ready_for_query(&mut s);

    send_simple_query(&mut s, "SELECT r % 2 FROM t;");
    expect_error_response_with_sqlstate(&mut s, "42883");
    read_ready_for_query(&mut s);

    send_simple_query(&mut s, "SELECT n % FROM t;");
    expect_error_response_with_sqlstate(&mut s, "42601");
    read_ready_for_query(&mut s);

    // エラー後も同じ接続で通常どおり実行できる。
    expect_one_value(&mut s, "SELECT 10 % 4;", 23, 4, "2");
}

#[test]
fn extended_describe_advertises_integer_remainder_type() {
    let (mut s, _g) = connect();
    send_length_prefixed_message(&mut s, b'P', &parse_body("s0", "SELECT n % 3 FROM t", 0));
    let mut describe = vec![b'S'];
    describe.extend_from_slice(b"s0");
    describe.push(0);
    send_length_prefixed_message(&mut s, b'D', &describe);
    send_length_prefixed_message(&mut s, b'S', b"");
    let (k, _) = read_message(&mut s);
    assert_eq!(k, b'1', "ParseComplete");
    let (k, body) = read_message(&mut s);
    assert_eq!(k, b't', "ParameterDescription");
    assert_eq!(body, [0, 0], "no parameters");
    let (k, body) = read_message(&mut s);
    assert_eq!(k, b'T', "RowDescription");
    // 列数 1、列名 `?column?` の後ろの型 OID は 23（int4）。
    let name_end = body[2..].iter().position(|b| *b == 0).expect("nul") + 2;
    assert_eq!(&body[2..name_end], b"?column?");
    let pos = name_end + 1;
    let oid = i32::from_be_bytes(body[pos + 6..pos + 10].try_into().expect("oid"));
    assert_eq!(oid, 23);
    let (k, body) = read_message(&mut s);
    assert_eq!(k, b'Z');
    assert_eq!(body, [b'I']);
}
