//! 疑似列 `id`・列・式への `AS` 別名（SQL-42。Issue #1540、親 #1537）を簡易クエリ（'Q'）と
//! 拡張クエリ（Parse／Describe／Bind／Execute／Sync。WIRE-11・WIRE-13・WIRE-14）の両方で
//! 公告できることを、生バイトクライアント（`tests/common`）で固定する層 A 結合テスト。
//!
//! engine 側が決めた結果列名（別名）と型 OID（別名付き `id` は numeric 1700、列は元の列型、
//! 整数算術は int4）が RowDescription に現れ、Describe（statement・portal）と実行時
//! RowDescription が一致すること、曖昧な `ORDER BY` 別名が SQLSTATE `42702` の
//! ErrorResponse になることを確認する。wire 層の本番コードは汎用経路のまま（変更なし）。

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
    let path = temp_db::unique_db_path("wire-sql42-alias");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("n", ColumnType::Integer, false),
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
    for (id, n) in [(1, 5), (2, 7)] {
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

fn parse_row_description_names_oids(body: &[u8]) -> Vec<(String, i32)> {
    let count = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2;
    let mut out = Vec::new();
    for _ in 0..count {
        let end = body[pos..].iter().position(|b| *b == 0).expect("nul") + pos;
        let name = String::from_utf8(body[pos..end].to_vec()).expect("utf8");
        pos = end + 1;
        let oid = i32::from_be_bytes(body[pos + 6..pos + 10].try_into().unwrap());
        pos += 18;
        out.push((name, oid));
    }
    out
}

fn describe_body(kind: u8, name: &str) -> Vec<u8> {
    let mut b = vec![kind];
    b.extend_from_slice(name.as_bytes());
    b.push(0);
    b
}

fn bind_body_text(portal: &str, statement: &str) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(portal.as_bytes());
    b.push(0);
    b.extend_from_slice(statement.as_bytes());
    b.push(0);
    b.extend_from_slice(&0i16.to_be_bytes());
    b.extend_from_slice(&0i16.to_be_bytes());
    b.extend_from_slice(&0i16.to_be_bytes());
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

const SQL: &str = "SELECT id AS i, n AS x, n+1 AS m FROM t ORDER BY m DESC LIMIT 10";

fn expected_fields() -> Vec<(String, i32)> {
    vec![
        ("i".to_string(), 1700),
        ("x".to_string(), 23),
        ("m".to_string(), 23),
    ]
}

#[test]
fn simple_query_advertises_alias_names_and_types() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, SQL);
    let fields: Vec<(String, i32)> = read_row_description_with_types(&mut s)
        .into_iter()
        .map(|(n, o, _)| (n, o))
        .collect();
    assert_eq!(fields, expected_fields());
    // m の降順なので id 2 → id 1。DataRow のテキスト表現は別名なしと同じ。
    assert_eq!(
        read_data_row(&mut s),
        vec![
            Some("2".to_string()),
            Some("7".to_string()),
            Some("8".to_string())
        ]
    );
    assert_eq!(
        read_data_row(&mut s),
        vec![
            Some("1".to_string()),
            Some("5".to_string()),
            Some("6".to_string())
        ]
    );
    assert_eq!(read_command_complete(&mut s), "SELECT 2");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn extended_describe_matches_runtime_row_description() {
    let (mut s, _g) = connect();
    send(&mut s, b'P', &parse_body("s0", SQL, 0));
    send(&mut s, b'D', &describe_body(b'S', "s0"));
    send(&mut s, b'S', b"");
    expect(&mut s, b'1');
    assert_eq!(expect(&mut s, b't'), [0, 0], "no parameters");
    let stmt_fields = parse_row_description_names_oids(&expect(&mut s, b'T'));
    assert_eq!(expect(&mut s, b'Z'), [b'I']);

    send(&mut s, b'B', &bind_body_text("", "s0"));
    send(&mut s, b'D', &describe_body(b'P', ""));
    send(&mut s, b'E', &execute_body("", 0));
    send(&mut s, b'S', b"");
    expect(&mut s, b'2');
    let portal_fields = parse_row_description_names_oids(&expect(&mut s, b'T'));
    assert_eq!(stmt_fields, expected_fields());
    assert_eq!(portal_fields, expected_fields());
    expect(&mut s, b'D');
    expect(&mut s, b'D');
    expect(&mut s, b'C');
    assert_eq!(expect(&mut s, b'Z'), [b'I']);
}

#[test]
fn ambiguous_order_by_alias_is_42702() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "SELECT n AS a, n+1 AS a FROM t ORDER BY a LIMIT 10");
    expect_error_response_with_sqlstate(&mut s, "42702");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}
