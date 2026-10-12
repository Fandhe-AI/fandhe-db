//! 式投影の結果型（SQL-42。Issue #1539、親 #1537）を簡易クエリ（'Q'）と拡張クエリ
//! （Parse／Bind／Describe／Execute／Sync。WIRE-11・WIRE-13・WIRE-14）の両方で
//! 公告できることを、生バイトクライアント（`tests/common`）で固定する層 A 結合テスト。
//!
//! engine 側が決めた結果列型（整数算術は int4／int8、単精度は float4、それ以外は float8）と
//! 既定列名 `?column?` が、RowDescription の型 OID・typlen・列名、DataRow の text／binary
//! 表現、Describe（statement・portal）と実行時 RowDescription の一致として観測できる
//! ことを確認する。wire 層の本番コードは汎用経路のまま（変更なし）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn connect() -> (TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql42-expr-types");
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
        "INSERT INTO t (id, embedding, n, b, r) VALUES (1, '[0.1,0.1]', 7, 3, 2.5) \
         USING OPERATION_ID 'seed-1'",
    );
    read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
    (stream, guard)
}

/// `(列名, 型 OID, typlen, format code)`。
type Field = (String, i32, i16, i16);

fn parse_row_description(body: &[u8]) -> Vec<Field> {
    let count = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2;
    let mut out = Vec::new();
    for _ in 0..count {
        let end = body[pos..].iter().position(|b| *b == 0).expect("nul") + pos;
        let name = String::from_utf8(body[pos..end].to_vec()).expect("utf8");
        pos = end + 1;
        let oid = i32::from_be_bytes(body[pos + 6..pos + 10].try_into().unwrap());
        let typlen = i16::from_be_bytes(body[pos + 10..pos + 12].try_into().unwrap());
        let format = i16::from_be_bytes(body[pos + 16..pos + 18].try_into().unwrap());
        pos += 18;
        out.push((name, oid, typlen, format));
    }
    out
}

fn parse_data_row(body: &[u8]) -> Vec<Option<Vec<u8>>> {
    let count = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2;
    let mut out = Vec::new();
    for _ in 0..count {
        let len = i32::from_be_bytes(body[pos..pos + 4].try_into().unwrap());
        pos += 4;
        if len < 0 {
            out.push(None);
        } else {
            let len = len as usize;
            out.push(Some(body[pos..pos + len].to_vec()));
            pos += len;
        }
    }
    out
}

fn describe_body(kind: u8, name: &str) -> Vec<u8> {
    let mut b = vec![kind];
    b.extend_from_slice(name.as_bytes());
    b.push(0);
    b
}

/// 結果 format code を 1 個（全列に適用）指定する Bind 本体（パラメータなし）。
fn bind_body_result_format(portal: &str, statement: &str, result_format: i16) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(portal.as_bytes());
    b.push(0);
    b.extend_from_slice(statement.as_bytes());
    b.push(0);
    b.extend_from_slice(&0i16.to_be_bytes());
    b.extend_from_slice(&0i16.to_be_bytes());
    b.extend_from_slice(&1i16.to_be_bytes());
    b.extend_from_slice(&result_format.to_be_bytes());
    b
}

fn send(stream: &mut TcpStream, kind: u8, body: &[u8]) {
    send_length_prefixed_message(stream, kind, body);
}

fn expect(stream: &mut TcpStream, kind: u8) -> Vec<u8> {
    let (k, body) = read_message(stream);
    assert_eq!(k, kind, "expected message {}", kind as char);
    body
}

const SQL: &str = "SELECT n+1, n+b, r+r, n*1.5 FROM t LIMIT 10";

fn expected_types() -> Vec<(String, i32, i16)> {
    vec![
        ("?column?".to_string(), 23, 4),
        ("?column?".to_string(), 20, 8),
        ("?column?".to_string(), 700, 4),
        ("?column?".to_string(), 701, 8),
    ]
}

#[test]
fn simple_query_advertises_expression_types() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, SQL);
    assert_eq!(read_row_description_with_types(&mut s), expected_types());
    assert_eq!(
        read_data_row(&mut s),
        vec![
            Some("8".to_string()),
            Some("10".to_string()),
            Some("5".to_string()),
            Some("10.5".to_string())
        ]
    );
    assert_eq!(read_command_complete(&mut s), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn extended_describe_matches_runtime_row_description_in_binary() {
    let (mut s, _g) = connect();
    send(&mut s, b'P', &parse_body("s0", SQL, 0));
    send(&mut s, b'D', &describe_body(b'S', "s0"));
    send(&mut s, b'S', b"");
    expect(&mut s, b'1');
    assert_eq!(expect(&mut s, b't'), [0, 0], "no parameters");
    let stmt_fields = parse_row_description(&expect(&mut s, b'T'));
    assert_eq!(expect(&mut s, b'Z'), [b'I']);

    send(&mut s, b'B', &bind_body_result_format("", "s0", 1));
    send(&mut s, b'D', &describe_body(b'P', ""));
    send(&mut s, b'E', &execute_body("", 0));
    send(&mut s, b'S', b"");
    expect(&mut s, b'2');
    let portal_fields = parse_row_description(&expect(&mut s, b'T'));
    let expected: Vec<(String, i32, i16)> = expected_types();
    for (i, (name, oid, len)) in expected.iter().enumerate() {
        assert_eq!(&stmt_fields[i].0, name);
        assert_eq!(&portal_fields[i].0, name);
        assert_eq!(stmt_fields[i].1, *oid, "statement describe oid");
        assert_eq!(portal_fields[i].1, *oid, "portal describe oid");
        assert_eq!(stmt_fields[i].2, *len);
        assert_eq!(portal_fields[i].2, *len);
        assert_eq!(portal_fields[i].3, 1, "binary format");
    }
    let row = parse_data_row(&expect(&mut s, b'D'));
    assert_eq!(row[0], Some(vec![0, 0, 0, 8]), "int4 binary");
    assert_eq!(row[1], Some(10i64.to_be_bytes().to_vec()), "int8 binary");
    assert_eq!(row[2], Some(5.0f32.to_be_bytes().to_vec()), "float4 binary");
    assert_eq!(
        row[3],
        Some(10.5f64.to_be_bytes().to_vec()),
        "float8 binary"
    );
    expect(&mut s, b'C');
    assert_eq!(expect(&mut s, b'Z'), [b'I']);
}

#[test]
fn integer_overflow_and_division_by_zero_use_postgres_sqlstates() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "SELECT n+2147483647 FROM t LIMIT 10");
    expect_error_response_with_sqlstate(&mut s, "22003");
    read_ready_for_query(&mut s);

    send_simple_query(&mut s, "SELECT n/0 FROM t LIMIT 10");
    expect_error_response_with_sqlstate(&mut s, "22012");
    read_ready_for_query(&mut s);
}
