//! `CREATE SEQUENCE` / `DROP SEQUENCE` / `ALTER SEQUENCE`（TABLE-24・SQL-23、Issue #1688・#1689）の簡易クエリプロトコル経由
//! （生バイトクライアント）検証（層 A）。
//!
//! 意味論そのものは `crates/engine/tests/table24_sequence_ddl.rs` が確定オラクル。本ファイルは
//! `wire_create_type.rs` と同じ流儀で、DDL 実行権限の wire 越しの反映（既定 `42501`）・
//! `CommandComplete` タグ・`42P07`／`42P01`／`42809` の SQLSTATE に徹する。

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
    let path = temp_db::unique_db_path("wire-sequence-ddl-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
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

/// DDL 実行権限を持たないユーザー（既定）は、SEQUENCE の有無を問わず `42501`。
#[test]
fn wire_sequence_ddl_without_permission_is_rejected() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    for sql in [
        "CREATE SEQUENCE s1",
        "DROP SEQUENCE s1",
        "DROP SEQUENCE nothere",
        "ALTER SEQUENCE s1 CYCLE",
        "ALTER SEQUENCE nothere RENAME TO x",
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "42501");
        read_ready_for_query(&mut stream);
    }
}

/// 許可ユーザーの CREATE／DROP は pg 互換の固定タグを返し、重複・不在・種別違いは各 SQLSTATE。
#[test]
fn wire_sequence_ddl_lifecycle_and_sqlstates() {
    let (core, _guard) = new_core_with_docs_table();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "CREATE SEQUENCE s1 START WITH 10 INCREMENT BY -2 MINVALUE -100 MAXVALUE 10 CYCLE",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE SEQUENCE");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "CREATE SEQUENCE s1");
    expect_error_response_with_sqlstate(&mut stream, "42P07");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "CREATE SEQUENCE docs");
    expect_error_response_with_sqlstate(&mut stream, "42P07");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP SEQUENCE docs");
    expect_error_response_with_sqlstate(&mut stream, "42809");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP TABLE s1");
    expect_error_response_with_sqlstate(&mut stream, "42809");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP SEQUENCE s1");
    assert_eq!(read_command_complete(&mut stream), "DROP SEQUENCE");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP SEQUENCE s1");
    expect_error_response_with_sqlstate(&mut stream, "42P01");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "CREATE SEQUENCE s2 CACHE 5");
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    read_ready_for_query(&mut stream);
}

/// ALTER SEQUENCE（Issue #1689）は固定タグ `ALTER SEQUENCE`（RENAME も同じ）を返し、
/// 範囲違反 `22023`・不在 `42P01`・種別違い `42809`・衝突 `42P07` を wire 越しに返す。
#[test]
fn wire_alter_sequence_tag_and_sqlstates() {
    let (core, _guard) = new_core_with_docs_table();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "CREATE SEQUENCE s1");
    assert_eq!(read_command_complete(&mut stream), "CREATE SEQUENCE");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "ALTER SEQUENCE s1 INCREMENT BY 5 CYCLE");
    assert_eq!(read_command_complete(&mut stream), "ALTER SEQUENCE");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "ALTER SEQUENCE s1 RESTART WITH 3");
    assert_eq!(read_command_complete(&mut stream), "ALTER SEQUENCE");
    read_ready_for_query(&mut stream);

    for (sql, sqlstate) in [
        ("ALTER SEQUENCE s1 MINVALUE 5", "22023"),
        ("ALTER SEQUENCE s1 INCREMENT 0", "22023"),
        ("ALTER SEQUENCE nothere CYCLE", "42P01"),
        ("ALTER SEQUENCE docs CYCLE", "42809"),
        ("ALTER SEQUENCE s1 RENAME TO docs", "42P07"),
        ("ALTER SEQUENCE s1", "42601"),
        ("ALTER SEQUENCE s1 CACHE 5", "0A000"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, sqlstate);
        read_ready_for_query(&mut stream);
    }

    send_simple_query(&mut stream, "ALTER SEQUENCE s1 RENAME TO s2");
    assert_eq!(read_command_complete(&mut stream), "ALTER SEQUENCE");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP SEQUENCE s2");
    assert_eq!(read_command_complete(&mut stream), "DROP SEQUENCE");
    read_ready_for_query(&mut stream);
}
