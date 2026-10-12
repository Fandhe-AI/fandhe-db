//! FROM なし SELECT（Issue #1532・SQL-43）を簡易クエリ（'Q'）と拡張クエリ
//! （Parse／Bind／Describe／Execute／Sync。WIRE-11・WIRE-13・WIRE-14）の両方で
//! 返せることを、生バイトクライアント（`tests/common`）で固定する層 A 結合テスト。
//!
//! engine 側の構文分岐・定数式束縛・Describe は #1531 で実装済みで、wire 層は
//! 汎用経路（`simple_query`／`extended_query`）のまま ConstSelect を通す。
//! 本テストはその往復と、Describe（statement・portal）の列メタが実行時の
//! `RowDescription` と食い違わないこと（食い違いは fail-open 方向の欠陥）を観測する。
//! psql 相当（簡易クエリ）と tokio-postgres 相当（全列 binary 要求）・
//! psycopg／node pg 相当（text 結果）の送信列を再現する。
//! テーブルは作らず、ConstSelect がストレージに依存しないことも兼ねて確認する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn connect() -> (TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql43-from-less");
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

fn cstr_tag(body: &[u8]) -> String {
    String::from_utf8(body[..body.len() - 1].to_vec()).expect("utf8")
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

fn error_sqlstate(body: &[u8]) -> String {
    let mut pos = 0;
    while pos < body.len() && body[pos] != 0 {
        let code = body[pos];
        let end = body[pos + 1..].iter().position(|b| *b == 0).unwrap() + pos + 1;
        if code == b'C' {
            return String::from_utf8(body[pos + 1..end].to_vec()).unwrap();
        }
        pos = end + 1;
    }
    panic!("no SQLSTATE field");
}

/// 拡張クエリのエラー後に Sync で回復し、続けて `SELECT 1` が通ることを確認する。
fn expect_error_then_recover(stream: &mut TcpStream) -> String {
    let code = error_sqlstate(&expect(stream, b'E'));
    send(stream, b'S', b"");
    assert_eq!(expect(stream, b'Z'), [b'I']);
    send_simple_query(stream, "SELECT 1");
    read_row_description(stream);
    read_data_row(stream);
    read_command_complete(stream);
    read_ready_for_query(stream);
    code
}

#[test]
fn simple_select_one_returns_int4_row() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "SELECT 1");
    assert_eq!(
        read_row_description_with_types(&mut s),
        vec![("?column?".to_string(), 23, 4)]
    );
    assert_eq!(read_data_row(&mut s), vec![Some("1".to_string())]);
    assert_eq!(read_command_complete(&mut s), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn simple_alias_text_and_lowercase_semicolon() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "SELECT 1 AS one, 'a'");
    assert_eq!(
        read_row_description_with_types(&mut s),
        vec![("one".to_string(), 23, 4), ("?column?".to_string(), 25, -1)]
    );
    assert_eq!(
        read_data_row(&mut s),
        vec![Some("1".to_string()), Some("a".to_string())]
    );
    assert_eq!(read_command_complete(&mut s), "SELECT 1");
    read_ready_for_query(&mut s);

    send_simple_query(&mut s, "select 1;");
    read_row_description(&mut s);
    assert_eq!(read_data_row(&mut s), vec![Some("1".to_string())]);
    read_command_complete(&mut s);
    read_ready_for_query(&mut s);
}

#[test]
fn simple_multi_statement_returns_two_result_sets() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "SELECT 1; SELECT 2");
    for expected in ["1", "2"] {
        read_row_description(&mut s);
        assert_eq!(read_data_row(&mut s), vec![Some(expected.to_string())]);
        assert_eq!(read_command_complete(&mut s), "SELECT 1");
    }
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn simple_in_explicit_transaction_keeps_status() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "BEGIN");
    read_command_complete(&mut s);
    assert_eq!(read_ready_for_query_status(&mut s), b'T');
    send_simple_query(&mut s, "SELECT 1");
    read_row_description(&mut s);
    assert_eq!(read_data_row(&mut s), vec![Some("1".to_string())]);
    read_command_complete(&mut s);
    assert_eq!(read_ready_for_query_status(&mut s), b'T');
    send_simple_query(&mut s, "COMMIT");
    read_command_complete(&mut s);
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn extended_prepare_describe_statement_then_binary_query_matches() {
    let (mut s, _g) = connect();
    // tokio-postgres の prepare 相当
    send(&mut s, b'P', &parse_body("s0", "SELECT 1", 0));
    send(&mut s, b'D', &describe_body(b'S', "s0"));
    send(&mut s, b'S', b"");
    expect(&mut s, b'1');
    let pd = expect(&mut s, b't');
    assert_eq!(pd, [0, 0], "no parameters");
    let stmt_fields = parse_row_description(&expect(&mut s, b'T'));
    assert_eq!(stmt_fields, vec![("?column?".to_string(), 23, 4, 0)]);
    assert_eq!(expect(&mut s, b'Z'), [b'I']);

    // tokio-postgres の query 相当（全列 binary）
    send(&mut s, b'B', &bind_body_result_format("", "s0", 1));
    send(&mut s, b'D', &describe_body(b'P', ""));
    send(&mut s, b'E', &execute_body("", 0));
    send(&mut s, b'S', b"");
    expect(&mut s, b'2');
    let portal_fields = parse_row_description(&expect(&mut s, b'T'));
    // Describe 整合: portal は format 欄のみ 1、他は statement 版・簡易クエリ版と一致する。
    assert_eq!(portal_fields, vec![("?column?".to_string(), 23, 4, 1)]);
    assert_eq!(stmt_fields[0].0, portal_fields[0].0);
    assert_eq!(stmt_fields[0].1, portal_fields[0].1);
    assert_eq!(stmt_fields[0].2, portal_fields[0].2);
    let row = parse_data_row(&expect(&mut s, b'D'));
    assert_eq!(row, vec![Some(vec![0, 0, 0, 1])]);
    assert_eq!(cstr_tag(&expect(&mut s, b'C')), "SELECT 1");
    assert_eq!(expect(&mut s, b'Z'), [b'I']);

    // 簡易クエリ版との整合
    send_simple_query(&mut s, "SELECT 1");
    let simple = read_row_description_with_types(&mut s);
    assert_eq!(simple[0], ("?column?".to_string(), 23, 4));
}

#[test]
fn extended_unnamed_text_result_round_trip() {
    let (mut s, _g) = connect();
    // psycopg／node pg 相当（無名 statement・text 結果）
    send(&mut s, b'P', &parse_body("", "SELECT 1", 0));
    send(&mut s, b'B', &bind_body("", ""));
    send(&mut s, b'D', &describe_body(b'P', ""));
    send(&mut s, b'E', &execute_body("", 0));
    send(&mut s, b'S', b"");
    expect(&mut s, b'1');
    expect(&mut s, b'2');
    let fields = parse_row_description(&expect(&mut s, b'T'));
    assert_eq!(fields, vec![("?column?".to_string(), 23, 4, 0)]);
    assert_eq!(
        parse_data_row(&expect(&mut s, b'D')),
        vec![Some(b"1".to_vec())]
    );
    assert_eq!(cstr_tag(&expect(&mut s, b'C')), "SELECT 1");
    assert_eq!(expect(&mut s, b'Z'), [b'I']);
}

#[test]
fn extended_rejects_select_star_and_recovers() {
    let (mut s, _g) = connect();
    send(&mut s, b'P', &parse_body("", "SELECT *", 0));
    assert_eq!(expect_error_then_recover(&mut s), "42601");
}

#[test]
fn extended_rejects_param_placeholder_and_recovers() {
    let (mut s, _g) = connect();
    // 型が決まらない `$n` は許可位置外として拒否のまま（fail-closed）。
    send(&mut s, b'P', &parse_body("", "SELECT $1", 0));
    assert_eq!(expect_error_then_recover(&mut s), "42601");
}

#[test]
fn simple_column_reference_is_rejected() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "SELECT x");
    expect_error_response_with_sqlstate(&mut s, "42601");
    read_ready_for_query(&mut s);
}
/// 0 行結果（RowDescription のみ・DataRow なし・`SELECT 0`）を読み、ReadyForQuery が `I` で
/// あることを確認する（Issue #2329・SQL-43）。
fn expect_simple_zero_rows(s: &mut TcpStream, sql: &str) {
    send_simple_query(s, sql);
    assert_eq!(
        read_row_description_with_types(s),
        vec![("?column?".to_string(), 23, 4)]
    );
    assert_eq!(read_command_complete(s), "SELECT 0");
    assert_eq!(read_ready_for_query_status(s), b'I');
}

#[test]
fn simple_where_limit_offset_select_zero_or_one_row() {
    let (mut s, _g) = connect();
    expect_simple_zero_rows(&mut s, "SELECT 1 WHERE false");
    expect_simple_zero_rows(&mut s, "SELECT 1 OFFSET 1");
    for sql in ["SELECT 1 WHERE true", "SELECT 1 LIMIT 1 OFFSET 0"] {
        send_simple_query(&mut s, sql);
        assert_eq!(
            read_row_description_with_types(&mut s),
            vec![("?column?".to_string(), 23, 4)]
        );
        assert_eq!(read_data_row(&mut s), vec![Some("1".to_string())]);
        assert_eq!(read_command_complete(&mut s), "SELECT 1");
        assert_eq!(read_ready_for_query_status(&mut s), b'I');
    }
}

#[test]
fn simple_negative_limit_offset_and_non_boolean_where_are_rejected() {
    let (mut s, _g) = connect();
    for (sql, code) in [
        ("SELECT 1 LIMIT -1", "2201W"),
        ("SELECT 1 OFFSET -1", "2201X"),
        ("SELECT 1 WHERE 1", "42804"),
    ] {
        send_simple_query(&mut s, sql);
        expect_error_response_with_sqlstate(&mut s, code);
        read_ready_for_query(&mut s);
    }
    // セッションは継続する。
    expect_simple_zero_rows(&mut s, "SELECT 1 LIMIT 0");
}

#[test]
fn extended_limit_zero_describes_columns_and_returns_no_rows() {
    let (mut s, _g) = connect();
    send(&mut s, b'P', &parse_body("s0", "SELECT 1 LIMIT 0", 0));
    send(&mut s, b'D', &describe_body(b'S', "s0"));
    send(&mut s, b'S', b"");
    expect(&mut s, b'1');
    expect(&mut s, b't');
    assert_eq!(
        parse_row_description(&expect(&mut s, b'T')),
        vec![("?column?".to_string(), 23, 4, 0)]
    );
    assert_eq!(expect(&mut s, b'Z'), [b'I']);

    send(&mut s, b'B', &bind_body("", "s0"));
    send(&mut s, b'D', &describe_body(b'P', ""));
    send(&mut s, b'E', &execute_body("", 0));
    send(&mut s, b'S', b"");
    expect(&mut s, b'2');
    assert_eq!(
        parse_row_description(&expect(&mut s, b'T')),
        vec![("?column?".to_string(), 23, 4, 0)]
    );
    assert_eq!(cstr_tag(&expect(&mut s, b'C')), "SELECT 0");
    assert_eq!(expect(&mut s, b'Z'), [b'I']);
}

#[test]
fn extended_negative_limit_fails_at_execute_and_recovers() {
    let (mut s, _g) = connect();
    send(&mut s, b'P', &parse_body("", "SELECT 1 LIMIT -1", 0));
    send(&mut s, b'B', &bind_body("", ""));
    send(&mut s, b'E', &execute_body("", 0));
    expect(&mut s, b'1');
    expect(&mut s, b'2');
    assert_eq!(expect_error_then_recover(&mut s), "2201W");
}
