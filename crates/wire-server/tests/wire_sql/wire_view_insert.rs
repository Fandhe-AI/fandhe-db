//! ビューへの `INSERT`（TABLE-43・Issue #1671）の wire 越し検証（層 A。ポインタ:
//! `docs/spec/04-behavior/data-model.md` TABLE-43）。
//!
//! 書き換えと基表への書き込みの意味論は `crates/engine/tests/table43_view_insert.rs` が確定
//! オラクルとして検証済みのため、本ファイルは (1) 簡易クエリプロトコルで `RETURNING` の
//! `RowDescription` の列名が**ビューの公開名**になること (2) 拡張クエリプロトコルで Parse／Describe の
//! 後に `CREATE OR REPLACE VIEW` が入っても Execute が現在の定義で書き換わること、を確認する。

use crate::common;

use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-view-insert");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "notes",
            vec![
                ColumnDef::new("title", ColumnType::Text, true),
                ColumnDef::new("tag", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    storage
        .create_table(&TableSchema::new(
            "notes2",
            vec![
                ColumnDef::new("title", ColumnType::Text, true),
                ColumnDef::new("tag", ColumnType::Text, true),
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

/// 管理用セッションでビュー DDL を実行する（wire 経由の DDL 権限とは独立）。
fn ddl(core: &EngineCore, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant id");
    core.execute_sql_in_session(&ctx, &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn connect(core: Arc<EngineCore>) -> TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

/// `RowDescription`（'T'）本体から列名を取り出す。
fn row_description_names(body: &[u8]) -> Vec<String> {
    let count = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut names = Vec::new();
    let mut pos = 2;
    for _ in 0..count {
        let end = body[pos..].iter().position(|b| *b == 0).expect("name nul") + pos;
        names.push(String::from_utf8_lossy(&body[pos..end]).to_string());
        // テーブル OID(4)・属性番号(2)・型 OID(4)・型サイズ(2)・型修飾子(4)・形式コード(2)。
        pos = end + 1 + 18;
    }
    names
}

fn describe_portal_body(name: &str) -> Vec<u8> {
    let mut body = vec![b'P'];
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body
}

fn sync_and_expect_ready(stream: &mut TcpStream) {
    send_length_prefixed_message(stream, b'S', b"");
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery");
}

/// 簡易クエリ: 列別名つきビューへの `INSERT ... RETURNING` は、公開名の `RowDescription` と
/// `INSERT 0 1` を返し、基表へ書き込まれる。
#[test]
fn simple_query_insert_returning_uses_public_column_names() {
    let (core, _guard) = new_core();
    ddl(
        &core,
        "CREATE VIEW nv (id, heading, label) AS SELECT id, title, tag FROM notes",
    );
    let mut stream = connect(Arc::clone(&core));

    send_simple_query(
        &mut stream,
        "INSERT INTO nv (id, heading) VALUES (1, 'hello') RETURNING id, heading, label \
         USING OPERATION_ID 'wire-view-insert-1'",
    );
    let columns = read_row_description(&mut stream);
    assert_eq!(
        columns,
        vec!["id".to_string(), "heading".to_string(), "label".to_string()]
    );
    let row = read_data_row(&mut stream);
    assert_eq!(
        row,
        vec![Some("1".to_string()), Some("hello".to_string()), None]
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    // 基表では物理名で読める。
    send_simple_query(&mut stream, "SELECT id, title FROM notes LIMIT 10");
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id".to_string(), "title".to_string()]);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![Some("1".to_string()), Some("hello".to_string())]);
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 拡張クエリ: Parse／Bind／Describe の後に `CREATE OR REPLACE VIEW` が入っても、Execute は
/// 現在の定義で書き換わる。Describe の列名は公開名。
#[test]
fn extended_protocol_execute_uses_the_current_view_definition() {
    let (core, _guard) = new_core();
    ddl(&core, "CREATE VIEW sv AS SELECT id, title FROM notes");
    let mut stream = connect(Arc::clone(&core));

    let sql = "INSERT INTO sv (id, title) VALUES (7, 'x') RETURNING title USING OPERATION_ID 'wire-view-insert-2'";
    send_length_prefixed_message(&mut stream, b'P', &parse_body("s1", sql, 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1', "expected ParseComplete");
    send_length_prefixed_message(&mut stream, b'B', &bind_body("p1", "s1"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'2', "expected BindComplete");
    send_length_prefixed_message(&mut stream, b'D', &describe_portal_body("p1"));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'T', "expected RowDescription");
    assert_eq!(row_description_names(&body), vec!["title"]);

    // Parse と Execute の間に、同じ公開列で別の基表（`notes2`）を読む定義へ置き換える。
    ddl(
        &core,
        "CREATE OR REPLACE VIEW sv AS SELECT id, title FROM notes2",
    );

    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 0));
    let (kind, row) = read_message(&mut stream);
    assert_eq!(kind, b'D', "expected DataRow");
    assert_eq!(i16::from_be_bytes([row[0], row[1]]), 1);
    let (kind, tag) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "INSERT 0 1");
    sync_and_expect_ready(&mut stream);

    // 行は現在の定義の基表（`notes2`）へ入り、置換前の基表（`notes`）は空のまま。
    send_simple_query(&mut stream, "SELECT id FROM notes2 LIMIT 10");
    let _ = read_row_description(&mut stream);
    assert_eq!(read_data_row(&mut stream), vec![Some("7".to_string())]);
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
    send_simple_query(&mut stream, "SELECT id FROM notes LIMIT 10");
    let _ = read_row_description(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 0");
    read_ready_for_query(&mut stream);
}

/// 公開名 `id` が基表の TEXT 列を指すビュー: `$1` の型推論と Bind は疑似列 `id`（整数）ではなく
/// 物理列 `title` の型に従い、文字列の Bind が整数構文エラーにならず成功する。
#[test]
fn extended_protocol_public_id_alias_to_text_column_binds_as_text() {
    let (core, _guard) = new_core();
    ddl(&core, "CREATE VIEW idv (id) AS SELECT title FROM notes");
    let mut stream = connect(Arc::clone(&core));

    let sql = "INSERT INTO idv (id) VALUES ($1) USING OPERATION_ID 'wire-view-insert-3'";
    send_length_prefixed_message(&mut stream, b'P', &parse_body("s1", sql, 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1', "expected ParseComplete");

    // Bind: 全パラメータ text 形式・1 個の値 'hello'・結果形式なし。
    let mut bind = Vec::new();
    bind.extend_from_slice(b"p1\0s1\0");
    bind.extend_from_slice(&0i16.to_be_bytes());
    bind.extend_from_slice(&1i16.to_be_bytes());
    bind.extend_from_slice(&5i32.to_be_bytes());
    bind.extend_from_slice(b"hello");
    bind.extend_from_slice(&0i16.to_be_bytes());
    send_length_prefixed_message(&mut stream, b'B', &bind);
    let (kind, _) = read_message(&mut stream);
    assert_eq!(
        kind, b'2',
        "expected BindComplete (text bind must not be an integer error)"
    );

    // この定義では基表の `id` を公開する列が無いため Execute は書き換えで拒否される（fail-closed）。
    // 本テストの対象は型推論なので、整数構文エラー（22P02）ではないことだけを確認する。
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 0));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'E', "expected ErrorResponse");
    let text = String::from_utf8_lossy(&body);
    assert!(!text.contains("22P02"), "integer parse error: {text}");
}
