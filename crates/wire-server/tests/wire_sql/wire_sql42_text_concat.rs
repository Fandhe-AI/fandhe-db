//! text／varchar の `||` 連結（SQL-42 のポインタのみ。Issue #3276、親 #3275）を、psql 同様に
//! 末尾 `;` 付きで単独送信する簡易クエリ（'Q'）と拡張クエリ（Parse／Describe）で固定する層 A 結合テスト。
//!
//! engine 側が決めた結果列型（text。OID 25・len -1）が RowDescription、値と NULL 伝播が DataRow、
//! 結果長上限（54000）・未対応の組の拒否が ErrorResponse の SQLSTATE として観測でき、
//! エラー後も接続が使えること（fail-closed）を確認する。wire 層の本番コードは汎用経路のまま（変更なし）。

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
    let path = temp_db::unique_db_path("wire-sql42-text-concat");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, true),
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
    for (id, lang) in [(1, "'ja'"), (2, "NULL")] {
        send_simple_query(
            &mut stream,
            &format!(
                "INSERT INTO t (id, embedding, lang, n) VALUES ({id}, '[0.1,0.1]', {lang}, 1) \
                 USING OPERATION_ID 'seed-{id}'"
            ),
        );
        read_command_complete(&mut stream);
        read_ready_for_query(&mut stream);
    }
    (stream, guard)
}

fn expect_one_text(s: &mut TcpStream, sql: &str, text: &str) {
    send_simple_query(s, sql);
    assert_eq!(
        read_row_description_with_types(s),
        vec![("?column?".to_string(), 25, -1)],
        "{sql}"
    );
    assert_eq!(read_data_row(s), vec![Some(text.to_string())], "{sql}");
    assert_eq!(read_command_complete(s), "SELECT 1", "{sql}");
    assert_eq!(read_ready_for_query_status(s), b'I');
}

#[test]
fn simple_query_with_trailing_semicolon_returns_text_concat() {
    let (mut s, _g) = connect();
    expect_one_text(&mut s, "SELECT 'a' || 'b';", "ab");
    expect_one_text(&mut s, "SELECT lang || 'x' FROM t WHERE id = 1;", "jax");
}

#[test]
fn null_operand_propagates_as_null_cell() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "SELECT lang || 'x' FROM t WHERE id = 2;");
    assert_eq!(
        read_row_description_with_types(&mut s),
        vec![("?column?".to_string(), 25, -1)]
    );
    assert_eq!(read_data_row(&mut s), vec![None]);
    assert_eq!(read_command_complete(&mut s), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn limit_and_unsupported_pairs_fail_closed_and_keep_connection() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "SELECT lpad('x', 4194304, 'y') || 'z';");
    expect_error_response_with_sqlstate(&mut s, "54000");
    read_ready_for_query(&mut s);

    send_simple_query(&mut s, "SELECT lang || FROM t;");
    expect_error_response_with_sqlstate(&mut s, "42601");
    read_ready_for_query(&mut s);

    // エラー後も同じ接続で通常どおり実行できる。
    expect_one_text(&mut s, "SELECT 'a' || 'b';", "ab");
}

/// 拒否の SQLSTATE は被演算子の形・FROM の有無によらず型の組で決まる（Issue #3277）。
#[test]
fn rejected_pairs_use_consistent_sqlstates_and_keep_connection() {
    let (mut s, _g) = connect();
    for (sql, state) in [
        ("SELECT 1 || 2;", "42883"),
        ("SELECT n || n FROM t;", "42883"),
    ] {
        send_simple_query(&mut s, sql);
        expect_error_response_with_sqlstate(&mut s, state);
        read_ready_for_query(&mut s);
    }
    expect_one_text(&mut s, "SELECT 'a' || 'b';", "ab");
}

#[test]
fn aggregate_list_may_start_with_a_string_literal_concat() {
    let (mut s, _g) = connect();
    send_simple_query(&mut s, "SELECT 'x' || lang, count(*) FROM t GROUP BY lang;");
    let _ = read_row_description_with_types(&mut s);
    let mut rows = vec![read_data_row(&mut s), read_data_row(&mut s)];
    rows.sort();
    assert_eq!(
        rows,
        vec![
            vec![None, Some("1".to_string())],
            vec![Some("xja".to_string()), Some("1".to_string())]
        ]
    );
    assert_eq!(read_command_complete(&mut s), "SELECT 2");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn extended_describe_advertises_text_result_type() {
    let (mut s, _g) = connect();
    send_length_prefixed_message(
        &mut s,
        b'P',
        &parse_body("s0", "SELECT lang || 'x' FROM t", 0),
    );
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
    let name_end = body[2..].iter().position(|b| *b == 0).expect("nul") + 2;
    assert_eq!(&body[2..name_end], b"?column?");
    let pos = name_end + 1;
    let oid = i32::from_be_bytes(body[pos + 6..pos + 10].try_into().expect("oid"));
    assert_eq!(oid, 25);
    let (k, body) = read_message(&mut s);
    assert_eq!(k, b'Z');
    assert_eq!(body, [b'I']);
}

/// 非 text オペランドの暗黙連結（Issue #3278）。末尾 `;` 付き単独送信で値が返り、
/// 種別を決められない数値との組は 0A000 で拒否されても接続が使えること。
#[test]
fn non_text_operand_concat_over_simple_query() {
    let (mut s, _g) = connect();
    expect_one_text(&mut s, "SELECT 'n=' || 5;", "n=5");
    expect_one_text(&mut s, "SELECT 5 || 'x';", "5x");
    expect_one_text(&mut s, "SELECT lang || n FROM t WHERE id = 1;", "ja1");

    send_simple_query(&mut s, "SELECT 'x' || id FROM t;");
    expect_error_response_with_sqlstate(&mut s, "0A000");
    read_ready_for_query(&mut s);
    expect_one_text(&mut s, "SELECT 'n=' || 5;", "n=5");
}

/// INSERT VALUES のリテラル直後の `||`（Issue #3278 の PR 指摘）。
#[test]
fn non_text_operand_concat_in_insert_values() {
    let (mut s, _g) = connect();
    send_simple_query(
        &mut s,
        "INSERT INTO t (id, embedding, lang, n) VALUES (7, '[0.1,0.1]', 'n=' || 5, 1) \
         USING OPERATION_ID 'ins-3278-a'",
    );
    read_command_complete(&mut s);
    read_ready_for_query(&mut s);
    send_simple_query(
        &mut s,
        "INSERT INTO t (id, embedding, lang, n) VALUES (8, '[0.1,0.1]', 5 || 'x', 1) \
         USING OPERATION_ID 'ins-3278-b'",
    );
    read_command_complete(&mut s);
    read_ready_for_query(&mut s);
    expect_one_text(&mut s, "SELECT lang || '' FROM t WHERE id = 7;", "n=5");
    expect_one_text(&mut s, "SELECT lang || '' FROM t WHERE id = 8;", "5x");
}

#[test]
fn extended_describe_advertises_text_for_non_text_concat() {
    let (mut s, _g) = connect();
    send_length_prefixed_message(&mut s, b'P', &parse_body("s1", "SELECT 'n=' || 5", 0));
    let mut describe = vec![b'S'];
    describe.extend_from_slice(b"s1");
    describe.push(0);
    send_length_prefixed_message(&mut s, b'D', &describe);
    send_length_prefixed_message(&mut s, b'S', b"");
    let (k, _) = read_message(&mut s);
    assert_eq!(k, b'1', "ParseComplete");
    let (k, _) = read_message(&mut s);
    assert_eq!(k, b't', "ParameterDescription");
    let (k, body) = read_message(&mut s);
    assert_eq!(k, b'T', "RowDescription");
    let name_end = body[2..].iter().position(|b| *b == 0).expect("nul") + 2;
    let pos = name_end + 1;
    let oid = i32::from_be_bytes(body[pos + 6..pos + 10].try_into().expect("oid"));
    assert_eq!(oid, 25);
}
