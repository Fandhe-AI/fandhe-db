//! ドル引用文字列（SQL-67 (1)・Issue #2216）を簡易クエリ（'Q'）と拡張クエリ
//! （Parse／Bind／Execute。WIRE-11・WIRE-12）の両方で受理できることを、生バイト
//! クライアントで固定する層 A 結合テスト。
//!
//! 字句解析・文分割は engine（`sql::lexer`・`sql::statement_splitter`）が担い、wire 層は
//! 汎用経路のまま。ドル引用の本文中の `;` で文が分割されないこと、未終端は `42601` で
//! 拒否したうえで接続が使い続けられることを確認する。

use crate::common;

use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn connect() -> (TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql67-dollar");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    (stream, guard)
}

fn expect(stream: &mut TcpStream, kind: u8) -> Vec<u8> {
    let (k, body) = read_message(stream);
    assert_eq!(k, kind, "expected message {}", kind as char);
    body
}

#[test]
fn simple_dollar_quote_with_semicolon_is_not_split() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "SELECT $$a;b$$; SELECT 2");
    read_row_description(&mut s);
    assert_eq!(read_data_row(&mut s), vec![Some("a;b".to_string())]);
    assert_eq!(read_command_complete(&mut s), "SELECT 1");
    read_row_description(&mut s);
    assert_eq!(read_data_row(&mut s), vec![Some("2".to_string())]);
    assert_eq!(read_command_complete(&mut s), "SELECT 1");
    read_ready_for_query(&mut s);
}

#[test]
fn extended_dollar_quote_round_trip() {
    let (mut s, _g) = connect();
    send_length_prefixed_message(&mut s, b'P', &parse_body("", "SELECT $tag$it's$tag$", 0));
    send_length_prefixed_message(&mut s, b'B', &bind_body("", ""));
    send_length_prefixed_message(&mut s, b'E', &execute_body("", 0));
    send_length_prefixed_message(&mut s, b'S', b"");
    expect(&mut s, b'1');
    expect(&mut s, b'2');
    let row = expect(&mut s, b'D');
    assert!(
        row.windows(4).any(|w| w == b"it's"),
        "DataRow must carry the body: {row:?}"
    );
    expect(&mut s, b'C');
    assert_eq!(expect(&mut s, b'Z'), [b'I']);
}

#[test]
fn unterminated_dollar_quote_is_42601_and_connection_survives() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "SELECT $$secret; SELECT 2");
    expect_error_response_with_sqlstate(&mut s, "42601");
    read_ready_for_query(&mut s);

    send_simple_query(&mut s, "SELECT 1");
    read_row_description(&mut s);
    assert_eq!(read_data_row(&mut s), vec![Some("1".to_string())]);
    read_command_complete(&mut s);
    read_ready_for_query(&mut s);
}
