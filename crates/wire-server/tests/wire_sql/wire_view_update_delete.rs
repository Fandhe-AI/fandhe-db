//! ビューへの `UPDATE`／`DELETE`（TABLE-43・Issue #1672）の wire 越し検証（層 A。ポインタ:
//! `docs/spec/04-behavior/data-model.md` TABLE-43）。
//!
//! 書き換えと基表への書き込みの意味論は `crates/engine/tests/table43_view_update_delete.rs` が確定
//! オラクルとして検証済みのため、本ファイルは (1) 簡易クエリプロトコルで `RETURNING` の
//! `RowDescription` の列名が**ビューの公開名**になり、`CommandComplete` の行数がビューの外の行を
//! 含まないこと (2) 拡張クエリプロトコルで Parse／Describe の後に `CREATE OR REPLACE VIEW` が入っても
//! Execute が現在の定義で書き換わること、を確認する。

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
    let path = temp_db::unique_db_path("wire-view-update-delete");
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

/// 書き込み文（`RETURNING` なし）を簡易クエリで流し、`CommandComplete` のタグを返す。
fn exec(stream: &mut TcpStream, sql: &str) -> String {
    send_simple_query(stream, sql);
    let tag = read_command_complete(stream);
    read_ready_for_query(stream);
    tag
}

/// 簡易クエリ: 列別名・述語つきビューへの `UPDATE`／`DELETE ... RETURNING` は、公開名の
/// `RowDescription` を返し、`CommandComplete` の行数はビューの外の行を含まない。
#[test]
fn simple_query_update_delete_returning_uses_public_column_names() {
    let (core, _guard) = new_core();
    ddl(
        &core,
        "CREATE VIEW nv (id, heading, label) AS SELECT id, title, tag FROM notes WHERE tag = 'a'",
    );
    let mut stream = connect(Arc::clone(&core));
    assert_eq!(
        exec(
            &mut stream,
            "INSERT INTO notes (id, title, tag) VALUES (1, 't1', 'a'), (2, 't2', 'b') \
             USING OPERATION_ID 'wire-view-ud-0'"
        ),
        "INSERT 0 2"
    );

    send_simple_query(
        &mut stream,
        "UPDATE nv SET heading = 'new' WHERE id > 0 RETURNING id, heading \
         USING OPERATION_ID 'wire-view-ud-1'",
    );
    assert_eq!(
        read_row_description(&mut stream),
        vec!["id".to_string(), "heading".to_string()]
    );
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("1".to_string()), Some("new".to_string())]
    );
    assert_eq!(read_command_complete(&mut stream), "UPDATE 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "DELETE FROM nv WHERE id > 0 RETURNING heading USING OPERATION_ID 'wire-view-ud-2'",
    );
    assert_eq!(
        read_row_description(&mut stream),
        vec!["heading".to_string()]
    );
    assert_eq!(read_data_row(&mut stream), vec![Some("new".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "DELETE 1");
    read_ready_for_query(&mut stream);

    // ビューの外の行（id 2）は残り、値も変わらない。
    send_simple_query(&mut stream, "SELECT id, title FROM notes LIMIT 10");
    let _ = read_row_description(&mut stream);
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("2".to_string()), Some("t2".to_string())]
    );
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 拡張クエリ: Parse／Bind／Describe の後に `CREATE OR REPLACE VIEW` が入っても、Execute は
/// 現在の定義で書き換わる。Describe の列名は公開名。
#[test]
fn extended_protocol_update_uses_the_current_view_definition() {
    let (core, _guard) = new_core();
    ddl(&core, "CREATE VIEW sv AS SELECT id, title FROM notes");
    let mut stream = connect(Arc::clone(&core));
    for (table, op) in [("notes", "wire-view-ud-3"), ("notes2", "wire-view-ud-4")] {
        assert_eq!(
            exec(
                &mut stream,
                &format!(
                    "INSERT INTO {table} (id, title) VALUES (7, 'x') USING OPERATION_ID '{op}'"
                )
            ),
            "INSERT 0 1"
        );
    }

    let sql = "UPDATE sv SET title = 'z' WHERE id = 7 RETURNING title USING OPERATION_ID 'wire-view-ud-5'";
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
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "UPDATE 1");
    sync_and_expect_ready(&mut stream);

    // 更新は現在の定義の基表（`notes2`）にだけ入り、置換前の基表（`notes`）は変わらない。
    for (table, expected) in [("notes2", "z"), ("notes", "x")] {
        send_simple_query(&mut stream, &format!("SELECT title FROM {table} LIMIT 10"));
        let _ = read_row_description(&mut stream);
        assert_eq!(read_data_row(&mut stream), vec![Some(expected.to_string())]);
        let _ = read_command_complete(&mut stream);
        read_ready_for_query(&mut stream);
    }
}
