//! `CREATE SCHEMA` / `DROP SCHEMA`（TABLE-27・TABLE-36・SQL-23、Issue #1713）の簡易クエリプロトコル
//! 経由（生バイトクライアント）検証（層 A）。
//!
//! 意味論そのものは `crates/engine/tests/table27_schema_ddl.rs` が確定オラクル。本ファイルは
//! `wire_sequence_ddl.rs` と同じ流儀で、`CommandComplete` タグ・SQLSTATE
//! （`42P06`／`3F000`／`42939`／`42501`）・2 テナント接続での独立性と存在の秘匿に徹する。
//! `2BP01`（中身のある SCHEMA の DROP）は DDL でオブジェクトを tenant SCHEMA に作れない段階では
//! wire から到達できないため、engine 側の crate 内テストで検証する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-schema-ddl");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn_server(store: UserStore, engine: Arc<EngineCore>) -> std::net::SocketAddr {
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

/// DDL 実行権限を持たないユーザー（既定）は、SCHEMA の有無を問わず `42501`。
#[test]
fn wire_schema_ddl_without_permission_is_rejected() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    for sql in [
        "CREATE SCHEMA s1",
        "CREATE SCHEMA pg_x",
        "DROP SCHEMA s1",
        "DROP SCHEMA IF EXISTS s1",
        "DROP SCHEMA public",
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "42501");
        read_ready_for_query(&mut stream);
    }
}

/// 許可ユーザーの CREATE／DROP は固定タグを返し、重複・不在・予約名・組み込み名は各 SQLSTATE。
#[test]
fn wire_schema_ddl_lifecycle_and_sqlstates() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "CREATE SCHEMA s1");
    assert_eq!(read_command_complete(&mut stream), "CREATE SCHEMA");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "CREATE SCHEMA IF NOT EXISTS s1");
    assert_eq!(read_command_complete(&mut stream), "CREATE SCHEMA");
    read_ready_for_query(&mut stream);

    for (sql, sqlstate) in [
        ("CREATE SCHEMA s1", "42P06"),
        ("CREATE SCHEMA public", "42P06"),
        ("CREATE SCHEMA pg_x", "42939"),
        ("CREATE SCHEMA IF NOT EXISTS pg_x", "42939"),
        ("DROP SCHEMA public", "42501"),
        ("DROP SCHEMA ghost", "3F000"),
        ("CREATE SCHEMA AUTHORIZATION bob", "0A000"),
        ("CREATE SCHEMA a.b", "42601"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, sqlstate);
        read_ready_for_query(&mut stream);
    }

    send_simple_query(&mut stream, "DROP SCHEMA s1");
    assert_eq!(read_command_complete(&mut stream), "DROP SCHEMA");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP SCHEMA IF EXISTS s1");
    assert_eq!(read_command_complete(&mut stream), "DROP SCHEMA");
    read_ready_for_query(&mut stream);
}

/// `ALTER SCHEMA ... RENAME TO` と `ALTER TABLE ... SET SCHEMA`（Issue #1716）の CommandComplete タグと
/// SQLSTATE（権限なしは対象の有無によらず `42501`）。
#[test]
fn wire_alter_schema_and_set_schema_tags_and_sqlstates() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    for (sql, tag) in [
        ("CREATE SCHEMA a", "CREATE SCHEMA"),
        (
            "CREATE TABLE items (id BIGINT PRIMARY KEY, qty INTEGER)",
            "CREATE TABLE",
        ),
        ("ALTER SCHEMA a RENAME TO b", "ALTER SCHEMA"),
        ("ALTER TABLE items SET SCHEMA public", "ALTER TABLE"),
    ] {
        send_simple_query(&mut stream, sql);
        assert_eq!(read_command_complete(&mut stream), tag, "{sql}");
        read_ready_for_query(&mut stream);
    }

    for (sql, sqlstate) in [
        ("ALTER SCHEMA public RENAME TO z", "42501"),
        ("ALTER SCHEMA ghost RENAME TO z", "3F000"),
        ("ALTER SCHEMA b RENAME TO pg_x", "42939"),
        ("ALTER SCHEMA b OWNER TO bob", "0A000"),
        ("ALTER TABLE items SET SCHEMA b", "42501"),
        ("ALTER TABLE items SET SCHEMA ghost", "3F000"),
        ("ALTER TABLE items SET SCHEMA pg_catalog", "0A000"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, sqlstate);
        read_ready_for_query(&mut stream);
    }
}

/// 2 テナントの接続で同名が独立し、他テナントにしか無い名前の DROP は存在しない名前と
/// 同じ SQLSTATE・同じ文言になる（存在の秘匿）。
#[test]
fn wire_schema_ddl_is_tenant_scoped_and_hides_existence() {
    let (core, _guard) = new_core();
    let store = allowed_store(
        &[
            ("alice", "tenant-a", "correct-horse"),
            ("bob", "tenant-b", "battery-staple"),
        ],
        &["alice", "bob"],
    );
    let addr = spawn_server(store, core);
    let mut alice = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut bob = authenticate_to_ready_for_query(addr, "bob", "battery-staple");

    for stream in [&mut alice, &mut bob] {
        send_simple_query(stream, "CREATE SCHEMA shared");
        assert_eq!(read_command_complete(stream), "CREATE SCHEMA");
        read_ready_for_query(stream);
    }
    send_simple_query(&mut bob, "CREATE SCHEMA only_bob");
    assert_eq!(read_command_complete(&mut bob), "CREATE SCHEMA");
    read_ready_for_query(&mut bob);

    // alice から見て only_bob は存在しない名前と同じ応答（名前部分以外は同一文言）。
    send_simple_query(&mut alice, "DROP SCHEMA only_bob");
    expect_error_response_with_sqlstate_and_message(
        &mut alice,
        "3F000",
        "schema does not exist: only_bob",
    );
    read_ready_for_query(&mut alice);
    send_simple_query(&mut alice, "DROP SCHEMA ghost");
    expect_error_response_with_sqlstate_and_message(
        &mut alice,
        "3F000",
        "schema does not exist: ghost",
    );
    read_ready_for_query(&mut alice);

    // bob の shared を消しても alice の shared は残る。
    send_simple_query(&mut bob, "DROP SCHEMA shared");
    assert_eq!(read_command_complete(&mut bob), "DROP SCHEMA");
    read_ready_for_query(&mut bob);
    send_simple_query(&mut alice, "CREATE SCHEMA shared");
    expect_error_response_with_sqlstate(&mut alice, "42P06");
    read_ready_for_query(&mut alice);
}
