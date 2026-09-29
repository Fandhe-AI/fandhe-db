//! `ALTER TABLE ... DROP COLUMN`／`ALTER COLUMN ... TYPE`（TABLE-19・SQL-23、
//! Issue #1167）の簡易クエリプロトコル経由（生バイトクライアント）検証。
//! 構文・実行契約は `crates/engine/tests/sql_ddl_drop_alter_column.rs` が確定
//! オラクルで、本ファイルは wire フレーミング越しの CommandComplete／
//! ErrorResponse の SQLSTATE と ReadyForQuery 復帰、DDL 権限ゲートに絞る。

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
    let path = temp_db::unique_db_path("wire-ddl-drop-alter-column");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("note", ColumnType::Text, true),
                ColumnDef::new("qty", ColumnType::Integer, true),
                ColumnDef::new(
                    "amount",
                    ColumnType::Numeric {
                        precision: 5,
                        scale: 2,
                    },
                    true,
                ),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `spawn_server_with_engine`（`common/mod.rs`）と同型だが、`UserStore` を
/// 呼び出し元が組み立てた値として受け取る（`--ddl-allowed-users` 適用済みの
/// ストアをテストごとに構成するため。`wire_drop_table.rs` と同じ構成）。
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

/// ユーザーストアを組み立て、`ddl_allowed` に列挙した username にのみ DDL 実行
/// 権限を付与した（`--ddl-allowed-users` 相当）サーバーを起動する。
fn spawn_with_ddl_allowed_users(
    core: Arc<EngineCore>,
    users: &[(&str, &str, &str)],
    ddl_allowed: &[&str],
) -> std::net::SocketAddr {
    let users_path = write_user_store_file(users);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let store = if ddl_allowed.is_empty() {
        store
    } else {
        let names: Vec<String> = ddl_allowed.iter().map(|s| s.to_string()).collect();
        store
            .with_ddl_allowed_users(&names)
            .expect("ddl allowed users must be known usernames")
    };
    spawn_server_with_engine_and_store(store, core)
}

/// `alice` を DDL 権限保持ユーザーとして起動し、認証済みストリームを返す。
fn spawn_with_alice_as_ddl_principal(core: Arc<EngineCore>) -> std::net::TcpStream {
    let addr = spawn_with_ddl_allowed_users(core, &[("alice", "tenant-a", "pw-alice")], &["alice"]);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

/// `alice` を DDL 権限**非**保持ユーザー（`--ddl-allowed-users` 未指定相当）として
/// 起動し、認証済みストリームを返す（既定＝全 DDL 拒否。fail-closed）。
fn spawn_with_alice_without_ddl_privilege(core: Arc<EngineCore>) -> std::net::TcpStream {
    let addr = spawn_with_ddl_allowed_users(core, &[("alice", "tenant-a", "pw-alice")], &[]);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

#[test]
fn wire_drop_column_and_alter_type_return_command_complete() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);
    for sql in [
        "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(12,2)",
        "ALTER TABLE docs DROP COLUMN note",
    ] {
        send_simple_query(&mut stream, sql);
        assert_eq!(read_command_complete(&mut stream), "ALTER TABLE");
        read_ready_for_query(&mut stream);
    }
}

#[test]
fn wire_error_sqlstates_and_ready_for_query_recovery() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);
    for (sql, code) in [
        ("ALTER TABLE docs DROP COLUMN missing", "42703"),
        ("ALTER TABLE nope DROP COLUMN note", "42P01"),
        ("ALTER TABLE docs DROP COLUMN embedding", "42601"),
        ("ALTER TABLE docs DROP COLUMN tenant_id", "42601"),
        ("ALTER TABLE docs ALTER COLUMN note TYPE INTEGER", "42804"),
        (
            "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(4,2)",
            "42804",
        ),
        ("ALTER TABLE docs DROP COLUMN note CASCADE", "42601"),
        ("ALTER TABLE docs DROP COLUMN IF EXISTS note", "42601"),
        (
            "ALTER TABLE docs ALTER COLUMN amount SET DATA TYPE NUMERIC(9,2)",
            "42601",
        ),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, code);
        read_ready_for_query(&mut stream);
    }
}

#[test]
fn wire_dependent_check_column_is_rejected_with_2bp01() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);
    send_simple_query(
        &mut stream,
        "ALTER TABLE docs ADD CONSTRAINT qty_ck CHECK (qty > 0)",
    );
    assert_eq!(read_command_complete(&mut stream), "ALTER TABLE");
    read_ready_for_query(&mut stream);
    for sql in [
        "ALTER TABLE docs DROP COLUMN qty",
        "ALTER TABLE docs ALTER COLUMN qty TYPE BIGINT",
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "2BP01");
        read_ready_for_query(&mut stream);
    }
}

#[test]
fn wire_non_ddl_principal_is_rejected_with_42501_regardless_of_existence() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_without_ddl_privilege(core);
    for sql in [
        "ALTER TABLE docs DROP COLUMN note",
        "ALTER TABLE nope DROP COLUMN note",
        "ALTER TABLE docs DROP COLUMN missing",
        "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(12,2)",
        "ALTER TABLE nope ALTER COLUMN x TYPE TEXT",
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "42501");
        read_ready_for_query(&mut stream);
    }
}
