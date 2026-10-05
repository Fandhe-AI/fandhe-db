//! `CREATE TYPE ... AS ENUM` / `DROP TYPE`（TABLE-14・SQL-23・TASK-198、
//! Issue #1194）の簡易クエリプロトコル経由（生バイトクライアント）検証（層 A）。
//!
//! 意味論そのものは `crates/engine/tests/sql_enum_type_ddl.rs` が確定オラクル。
//! 本ファイルは `wire_create_view.rs` と同じ流儀で、DDL 実行権限の wire 越しの
//! 反映・`CommandComplete` タグ・`42501`／`2BP01`／`42704`／`42710` の SQLSTATE・
//! 複数文メッセージでの `0A000` に徹する。

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
    let path = temp_db::unique_db_path("wire-create-type-docs");
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

fn allowed_store(users: &[(&str, &str, &str)], allowed: &[&str]) -> UserStore {
    let users_path = write_user_store_file(users);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    store
        .with_ddl_allowed_users(&allowed.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .expect("allowed usernames must be known")
}

/// DDL 実行権限を持たないユーザーは `CREATE TYPE`／`DROP TYPE` いずれも `42501`。
#[test]
fn wire_type_ddl_without_permission_is_rejected() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    for sql in ["CREATE TYPE mood AS ENUM ('a')", "DROP TYPE mood"] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "42501");
        read_ready_for_query(&mut stream);
    }
}

/// 許可ユーザーの `CREATE TYPE`／`DROP TYPE` は pg 互換の固定タグを返し、
/// `ALTER TABLE ADD COLUMN` で依存列がある間の `DROP TYPE` は `2BP01`。
#[test]
fn wire_type_ddl_lifecycle_and_sqlstates() {
    let (core, _guard) = new_core_with_docs_table();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "CREATE TYPE mood AS ENUM ('happy', 'sad')");
    assert_eq!(read_command_complete(&mut stream), "CREATE TYPE");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "CREATE TYPE mood AS ENUM ('x')");
    expect_error_response_with_sqlstate(&mut stream, "42710");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "ALTER TABLE docs ADD COLUMN m mood");
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP TYPE mood");
    expect_error_response_with_sqlstate(&mut stream, "2BP01");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "ALTER TABLE docs DROP COLUMN m");
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP TYPE mood");
    assert_eq!(read_command_complete(&mut stream), "DROP TYPE");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP TYPE mood");
    expect_error_response_with_sqlstate(&mut stream, "42704");
    read_ready_for_query(&mut stream);
}

/// 複数文メッセージで `CREATE TYPE` を最後以外に置くと `0A000`。
#[test]
fn wire_create_type_not_last_in_multi_statement_message_is_rejected() {
    let (core, _guard) = new_core_with_docs_table();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "CREATE TYPE mood AS ENUM ('a'); SELECT COUNT(*) FROM docs",
    );
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    read_ready_for_query(&mut stream);
}
