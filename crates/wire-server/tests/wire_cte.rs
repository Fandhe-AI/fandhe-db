//! 非再帰 `WITH` 句（CTE。SQL-29 (b)・RLS-10 (b)、TASK-213、Issue #928）の
//! 簡易クエリプロトコル経由（生バイトクライアント）検証（層 A）。
//!
//! CTE の意味論そのもの（許可リスト・上限・RLS 暗黙適用・名前解決）は
//! `crates/engine/tests/sql29_cte.rs` が確定オラクルとして検証済みのため、
//! 本ファイルは `wire_create_view.rs` と同じ流儀で以下に徹する:
//! - `RowDescription` が CTE の公開列だけを含むこと
//! - 拒否形ごとの `ErrorResponse` SQLSTATE（`42601`・`54000`・`42P01`）
//! - 複数文メッセージ（`WITH ...; SELECT ...`）が受理されること
//! - 2 接続（別テナント）での CTE 経由読み取りの RLS 暗黙適用

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

use common::*;

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-cte-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn_server_with_engine_and_store(
    store: UserStore,
    engine: Arc<EngineCore>,
) -> std::net::SocketAddr {
    let store = Arc::new(store);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            engine,
            limiter,
            Duration::from_secs(5),
        );
    });

    addr
}

/// 単純な `WITH` クエリが受理され、`RowDescription` は CTE の公開列
/// （`id` のみ）だけを含む。
#[test]
fn wire_cte_simple_query_returns_only_exposed_columns() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja') USING OPERATION_ID 'op-1'",
    );
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "WITH ja AS (SELECT id FROM docs WHERE lang = 'ja') SELECT id FROM ja LIMIT 100",
    );
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id".to_string()]);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![Some("1".to_string())]);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// 拒否形ごとの SQLSTATE が `ErrorResponse` に現れ、接続は使用可能なまま維持
/// される。
#[test]
fn wire_cte_rejected_forms_report_expected_sqlstate_and_keep_connection_usable() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let cases: &[(&str, &str)] = &[
        (
            "WITH RECURSIVE r AS (SELECT id FROM docs) SELECT id FROM r LIMIT 10",
            "42601",
        ),
        (
            "WITH x AS (SELECT id FROM ghost) SELECT id FROM x LIMIT 10",
            "42P01",
        ),
    ];
    for (sql, expected) in cases {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, expected);
        read_ready_for_query(&mut stream);
    }

    // `54000`（定義数上限超過）: 接続がまだ使えることの対照確認も兼ねる。
    let defs: Vec<String> = (0..=16)
        .map(|i| format!("c{i} AS (SELECT id FROM docs)"))
        .collect();
    let over_limit_sql = format!("WITH {} SELECT id FROM c0 LIMIT 10", defs.join(", "));
    send_simple_query(&mut stream, &over_limit_sql);
    expect_error_response_with_sqlstate(&mut stream, "54000");
    read_ready_for_query(&mut stream);

    // 接続はまだ使用可能。
    send_simple_query(&mut stream, "SELECT COUNT(*) FROM docs");
    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// 複数文メッセージ（`WITH ...; SELECT ...`）が受理される。
#[test]
fn wire_cte_multi_statement_message_is_accepted() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "WITH x AS (SELECT id FROM docs) SELECT id FROM x LIMIT 10; SELECT COUNT(*) FROM docs",
    );
    let _columns = read_row_description(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 0");
    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// CTE 経由の読み取りには、参照した**接続自身**の `PolicyContext` で RLS が
/// 暗黙適用される（作成者〔alice〕の可視性は参照者〔bob〕へ引き継がれない。
/// RLS-10 (b)）。
#[test]
fn wire_cte_read_applies_reader_session_rls() {
    let (core, _guard) = new_core_with_docs_table();
    let store = {
        let users_path = write_user_store_file(&[
            ("alice", "tenant-a", "correct-horse"),
            ("bob", "tenant-b", "battery-staple"),
        ]);
        UserStore::load_from_file(&users_path).expect("valid user store")
    };
    let addr = spawn_server_with_engine_and_store(store, core);

    let mut alice_stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(
        &mut alice_stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja') USING OPERATION_ID 'op-alice-1'",
    );
    let _tag = read_command_complete(&mut alice_stream);
    read_ready_for_query(&mut alice_stream);

    let mut bob_stream = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    send_simple_query(
        &mut bob_stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (2, '[0.4,0.5,0.6]', 'ja') USING OPERATION_ID 'op-bob-1'",
    );
    let _tag = read_command_complete(&mut bob_stream);
    read_ready_for_query(&mut bob_stream);

    // bob は CTE 経由でも自分の行のみを見る（alice の行は不可視）。
    send_simple_query(
        &mut bob_stream,
        "WITH ja AS (SELECT id FROM docs WHERE lang = 'ja') SELECT id FROM ja LIMIT 100",
    );
    let _columns = read_row_description(&mut bob_stream);
    let row = read_data_row(&mut bob_stream);
    assert_eq!(row, vec![Some("2".to_string())]);
    let tag = read_command_complete(&mut bob_stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut bob_stream);
}
