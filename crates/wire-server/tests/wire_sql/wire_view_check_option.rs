//! ビューの `WITH CHECK OPTION` 違反（TABLE-43 (2)・Issue #1673）の wire 越し検証（層 A。ポインタ:
//! `docs/spec/04-behavior/data-model.md` TABLE-43、ERR-6 の `44000` 行）。
//!
//! 検査の意味論（入れ子・NULL・順序・副作用なし）は `crates/engine/tests/table43_view_check_option.rs`
//! が確定オラクルとして検証済みのため、本ファイルは簡易クエリ・拡張クエリの両プロトコルで
//! ErrorResponse の SQLSTATE が `44000` になり、文言がビュー名・行の値を含まない固定文言であることと、
//! 違反した文が行を残さないことを確認する。

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
    let path = temp_db::unique_db_path("wire-view-check-option");
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

const FIXED_MESSAGE: &str = "new row violates check option for view";

/// 簡易クエリ: 違反は `44000`・固定文言。適合する文は成功し、違反した文は行を残さない。
#[test]
fn simple_query_violation_is_44000_with_a_fixed_message() {
    let (core, _guard) = new_core();
    ddl(
        &core,
        "CREATE VIEW rv AS SELECT id, title, tag FROM notes WHERE tag = 'pub' WITH CHECK OPTION",
    );
    let mut stream = connect(Arc::clone(&core));

    send_simple_query(
        &mut stream,
        "INSERT INTO rv (id, title, tag) VALUES (1, 'secret-title', 'priv') \
         USING OPERATION_ID 'wire-wco-1'",
    );
    expect_error_response_with_sqlstate_and_message(&mut stream, "44000", FIXED_MESSAGE);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO rv (id, title, tag) VALUES (1, 'hello', 'pub') \
         USING OPERATION_ID 'wire-wco-1'",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    // UPDATE でビューの外へ出す更新も `44000`。行は変わらない。
    send_simple_query(
        &mut stream,
        "UPDATE rv SET tag = 'priv' WHERE id = 1 USING OPERATION_ID 'wire-wco-2'",
    );
    expect_error_response_with_sqlstate_and_message(&mut stream, "44000", FIXED_MESSAGE);
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id, tag FROM notes LIMIT 10");
    let _ = read_row_description(&mut stream);
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("1".to_string()), Some("pub".to_string())]
    );
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// 拡張クエリ: Parse／Bind／Execute の違反も `44000`。Sync で ReadyForQuery に戻る。
#[test]
fn extended_protocol_violation_is_44000() {
    let (core, _guard) = new_core();
    ddl(
        &core,
        "CREATE VIEW rv AS SELECT id, title, tag FROM notes WHERE tag = 'pub' WITH LOCAL CHECK OPTION",
    );
    let mut stream = connect(Arc::clone(&core));

    let sql =
        "INSERT INTO rv (id, title, tag) VALUES (5, 'x', 'priv') USING OPERATION_ID 'wire-wco-3'";
    send_length_prefixed_message(&mut stream, b'P', &parse_body("s1", sql, 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1', "expected ParseComplete");
    send_length_prefixed_message(&mut stream, b'B', &bind_body("p1", "s1"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'2', "expected BindComplete");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 0));
    expect_error_response_with_sqlstate_and_message(&mut stream, "44000", FIXED_MESSAGE);
    send_length_prefixed_message(&mut stream, b'S', b"");
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery");

    send_simple_query(&mut stream, "SELECT id FROM notes LIMIT 10");
    let _ = read_row_description(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 0");
    read_ready_for_query(&mut stream);
}
