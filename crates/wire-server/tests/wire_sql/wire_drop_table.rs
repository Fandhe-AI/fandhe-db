//! `DROP TABLE <table>`（TASK-203、対象ビヘイビア: SQL-23・TABLE-15、Issue #902）の
//! 簡易クエリプロトコル経由（生バイトクライアント）検証（層 A）。
//!
//! `DROP TABLE` の意味論そのもの（カタログ・全テナント行・台帳の削除、権限
//! ゲートの判定順序）は `crates/engine/tests/sql_drop_table.rs` が確定
//! オラクルとして検証済みのため、本ファイルは以下に徹する:
//! - DDL 実行権限（`--ddl-allowed-users`）が wire フレーミング越しに正しく
//!   反映されること（`42501` 応答・接続維持・許可ユーザーの成功）
//! - `wire_server::ddl_permission_opt`／`UserStore::with_ddl_allowed_users` の
//!   起動時 opt-in 自体の検証

use crate::common;

use crate::temp_db;

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
    let path = temp_db::unique_db_path("wire-drop-table-docs");
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

/// `spawn_server_with_engine`（`common/mod.rs`）と同型だが、`UserStore` を
/// ファイルパスからではなく呼び出し元が組み立てた値として受け取る
/// （`--ddl-allowed-users` 適用済みのストアをテストごとに構成するため）。
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

/// `--ddl-allowed-users` 未指定相当（既定の `UserStore`）では `DROP TABLE` は
/// `42501` で拒否され、接続は維持されたまま続く SELECT が成功する。
#[test]
fn wire_drop_table_without_permission_is_rejected_and_connection_stays_usable() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "DROP TABLE docs");
    expect_error_response_with_sqlstate(&mut stream, "42501");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT COUNT(*) FROM docs");
    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// `--ddl-allowed-users` で許可された username（alice）は `DROP TABLE` に成功し
/// `CommandComplete` タグ `DROP TABLE` を受け取る。許可されていない username
/// （bob。別テナント）は同じサーバー・同じテーブルに対して `42501` のまま。
#[test]
fn wire_drop_table_allowed_user_succeeds_and_others_remain_denied() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let store = store
        .with_ddl_allowed_users(&["alice".to_string()])
        .expect("alice must be a known username");
    let addr = spawn_server_with_engine_and_store(store, core);

    // bob（DDL 権限なし）は引き続き 42501。
    let mut bob_stream = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    send_simple_query(&mut bob_stream, "DROP TABLE docs");
    expect_error_response_with_sqlstate(&mut bob_stream, "42501");
    read_ready_for_query(&mut bob_stream);

    // alice（DDL 権限あり）は成功する。
    let mut alice_stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(&mut alice_stream, "DROP TABLE docs");
    let tag = read_command_complete(&mut alice_stream);
    assert_eq!(tag, "DROP TABLE");
    read_ready_for_query(&mut alice_stream);

    // テーブルは既に削除されたため、以後どちらのユーザーでも 42P01。
    send_simple_query(&mut alice_stream, "SELECT COUNT(*) FROM docs");
    expect_error_response_with_sqlstate(&mut alice_stream, "42P01");
    read_ready_for_query(&mut alice_stream);

    send_simple_query(&mut bob_stream, "SELECT COUNT(*) FROM docs");
    expect_error_response_with_sqlstate(&mut bob_stream, "42P01");
    read_ready_for_query(&mut bob_stream);
}

/// 許可ユーザーでも、対象テーブルが元々存在しなければ `42P01`。
#[test]
fn wire_drop_table_allowed_user_missing_table_is_42p01() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let store = store
        .with_ddl_allowed_users(&["alice".to_string()])
        .expect("alice must be a known username");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "DROP TABLE table_does_not_exist");
    expect_error_response_with_sqlstate(&mut stream, "42P01");
    read_ready_for_query(&mut stream);
}

/// `UserStore::with_ddl_allowed_users` は、ユーザーストアに実在しない
/// username を渡すと fail-closed に拒否する（起動時エラー相当）。
#[test]
fn with_ddl_allowed_users_rejects_unknown_username() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let err = match store.with_ddl_allowed_users(&["mallory".to_string()]) {
        Ok(_) => panic!("unknown username must be rejected"),
        Err(e) => e,
    };
    assert!(
        err.contains("mallory"),
        "error should name the offending username: {err}"
    );
}

/// `wire_server::ddl_permission_opt::parse` の異常系（Issue #902）。
#[test]
fn ddl_permission_opt_parse_rejects_malformed_values() {
    for raw in ["", ",", "alice,", ",alice", "alice,,bob", "alice,alice"] {
        assert!(
            wire_server::ddl_permission_opt::parse(raw).is_err(),
            "expected {raw:?} to be rejected"
        );
    }
    assert_eq!(
        wire_server::ddl_permission_opt::parse("alice,bob"),
        Ok(vec!["alice".to_string(), "bob".to_string()])
    );
}
