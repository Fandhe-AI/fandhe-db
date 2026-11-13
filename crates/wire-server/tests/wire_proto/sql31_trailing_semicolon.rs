//! 末尾 `;` 付きの Tx 制御文・`SET TRANSACTION`・カーソル文の結合テスト（Issue #3171・SQL-31・
//! SQL-38・SQL-69・WIRE-15・WIRE-16 のポインタ）。psql の `-f` は文末に `;` を付けて 1 メッセージに
//! 1 文ずつ送る。簡易クエリでは分割器が `Single` として元テキストを engine へ渡し、拡張クエリでは
//! `Parse` の本文がそのまま engine の検証器へ届く。両経路で受理されることを固定する。

use crate::common;
use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("sql31-trailing-semicolon-wire");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

fn insert_sql(id: u64, op: &str) -> String {
    format!(
        "INSERT INTO documents (id, embedding, body) VALUES ({id}, '[0.1,0.2,0.3]', 'row') USING OPERATION_ID '{op}';"
    )
}

fn connect(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "correct-horse")
}

fn expect_tag(stream: &mut std::net::TcpStream, sql: &str, tag: &str, status: u8) {
    send_simple_query(stream, sql);
    assert_eq!(read_command_complete(stream), tag, "{sql}");
    assert_eq!(read_ready_for_query_status(stream), status, "{sql}");
}

/// 1 文を送り、`ReadyForQuery` までに届いた `CommandComplete` タグと状態バイトを返す
/// （行を返す `FETCH` を含めて読み流す。`ErrorResponse` が来たら失敗）。
fn run_simple(stream: &mut std::net::TcpStream, sql: &str) -> (Vec<String>, u8) {
    send_simple_query(stream, sql);
    let mut tags = Vec::new();
    loop {
        let (kind, body) = read_message(stream);
        match kind {
            b'C' => tags.push(String::from_utf8_lossy(&body[..body.len() - 1]).into_owned()),
            b'T' | b'D' => {}
            b'Z' => return (tags, body.last().copied().expect("status byte")),
            other => panic!("{sql}: unexpected message {other}"),
        }
    }
}

fn count_rows(stream: &mut std::net::TcpStream) -> usize {
    send_simple_query(stream, "SELECT id FROM documents ORDER BY id LIMIT 10");
    let _ = read_row_description(stream);
    let mut n = 0;
    loop {
        let (kind, _) = read_message(stream);
        match kind {
            b'D' => n += 1,
            b'C' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    assert_eq!(read_ready_for_query_status(stream), b'I');
    n
}

#[test]
fn psql_style_begin_insert_rollback_discards_the_row() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, &insert_sql(1, "op-1"), "INSERT 0 1", b'T');
    expect_tag(&mut stream, "ROLLBACK;", "ROLLBACK", b'I');
    assert_eq!(count_rows(&mut stream), 0);
}

#[test]
fn each_tx_control_statement_with_semicolon_has_expected_tag_and_status() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    expect_tag(&mut stream, "START TRANSACTION;", "START TRANSACTION", b'T');
    expect_tag(&mut stream, &insert_sql(1, "op-1"), "INSERT 0 1", b'T');
    expect_tag(&mut stream, "SAVEPOINT s1;", "SAVEPOINT", b'T');
    expect_tag(&mut stream, &insert_sql(2, "op-2"), "INSERT 0 1", b'T');
    expect_tag(&mut stream, "ROLLBACK TO s1;", "ROLLBACK", b'T');
    expect_tag(&mut stream, "RELEASE s1;", "RELEASE", b'T');
    expect_tag(&mut stream, "END;", "COMMIT", b'I');
    assert_eq!(count_rows(&mut stream), 1);

    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, "COMMIT;", "COMMIT", b'I');
    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, "SET TRANSACTION READ ONLY;", "SET", b'T');
    expect_tag(&mut stream, "ABORT;", "ROLLBACK", b'I');
}

#[test]
fn cursor_statements_with_semicolon_are_accepted() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, &insert_sql(1, "op-1"), "INSERT 0 1", b'T');
    let (tags, status) = run_simple(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id FROM documents;",
    );
    assert_eq!(tags.len(), 1, "{tags:?}");
    assert_eq!(status, b'T');
    let (tags, status) = run_simple(&mut stream, "FETCH 1 FROM c;");
    assert_eq!(tags.len(), 1, "{tags:?}");
    assert_eq!(status, b'T');
    let (tags, status) = run_simple(&mut stream, "CLOSE c;");
    assert_eq!(tags.len(), 1, "{tags:?}");
    assert_eq!(status, b'T');
    expect_tag(&mut stream, "COMMIT;", "COMMIT", b'I');
}

/// 簡易クエリの分割器は `;;` の空文を捨てるため、psql 由来の `BEGIN;;` は従来どおり通る。
#[test]
fn double_semicolon_over_simple_query_is_absorbed_by_the_splitter() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    expect_tag(&mut stream, "BEGIN;;", "BEGIN", b'T');
    expect_tag(&mut stream, "ROLLBACK;", "ROLLBACK", b'I');
}

fn send_sync(stream: &mut std::net::TcpStream) {
    send_length_prefixed_message(stream, b'S', b"");
}

fn parse_bind_execute(stream: &mut std::net::TcpStream, sql: &str) -> String {
    send_length_prefixed_message(stream, b'P', &parse_body("", sql, 0));
    let (kind, body) = read_message(stream);
    assert_eq!(
        kind,
        b'1',
        "{sql}: expected ParseComplete: {}",
        String::from_utf8_lossy(&body)
    );
    send_length_prefixed_message(stream, b'B', &bind_body("", ""));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'2', "{sql}: expected BindComplete");
    send_length_prefixed_message(stream, b'E', &execute_body("", 0));
    let tag = loop {
        let (kind, body) = read_message(stream);
        match kind {
            b'C' => {
                break String::from_utf8_lossy(&body[..body.len() - 1]).into_owned();
            }
            b'D' => {}
            other => panic!("{sql}: unexpected message {other}"),
        }
    };
    send_sync(stream);
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'Z');
    tag
}

#[test]
fn extended_protocol_accepts_trailing_semicolon_and_rolls_back() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    assert_eq!(parse_bind_execute(&mut stream, "BEGIN;"), "BEGIN");
    assert_eq!(
        parse_bind_execute(&mut stream, &insert_sql(1, "op-1")),
        "INSERT 0 1"
    );
    assert_eq!(
        parse_bind_execute(&mut stream, "SAVEPOINT s1;"),
        "SAVEPOINT"
    );
    let declare = parse_bind_execute(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id FROM documents;",
    );
    assert!(!declare.is_empty());
    assert!(!parse_bind_execute(&mut stream, "FETCH 1 FROM c;").is_empty());
    assert!(!parse_bind_execute(&mut stream, "CLOSE c;").is_empty());
    assert_eq!(parse_bind_execute(&mut stream, "ROLLBACK;"), "ROLLBACK");
    assert_eq!(count_rows(&mut stream), 0);
}

#[test]
fn extended_protocol_rejects_double_semicolon_without_changing_state() {
    let (core, _g) = new_core();
    let mut stream = connect(core);

    send_length_prefixed_message(&mut stream, b'P', &parse_body("", "BEGIN;;", 0));
    send_sync(&mut stream);
    expect_error_response_with_sqlstate(&mut stream, "42601");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}
