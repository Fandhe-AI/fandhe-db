//! `schema.table` のテーブル参照の名前解決（TABLE-27・Issue #1714）の簡易・拡張クエリプロトコル
//! 経由（生バイトクライアント）検証（層 A）。
//!
//! 意味論そのものは `crates/engine/tests/table27_qualified_names.rs` が確定オラクル。本ファイルは
//! `wire_schema_ddl.rs` と同じ流儀で、`public.t` が簡易・拡張クエリの両方で未修飾の `t` と同じに
//! 動くこと、および SQLSTATE（`3F000`／`42P01`／`0A000`）が wire へ届くことに徹する。

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
    let path = temp_db::unique_db_path("wire-schema-qualified");
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

fn simple_ok(stream: &mut std::net::TcpStream, sql: &str) -> String {
    send_simple_query(stream, sql);
    let tag = read_command_complete(stream);
    read_ready_for_query(stream);
    tag
}

/// 簡易クエリで `public.t` が未修飾と同じに動き、解決エラーが SQLSTATE で届く。
#[test]
fn wire_simple_query_public_qualified_names() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    assert_eq!(
        simple_ok(
            &mut stream,
            "CREATE TABLE public.t (id BIGINT PRIMARY KEY, name TEXT)"
        ),
        "CREATE TABLE"
    );
    assert_eq!(
        simple_ok(
            &mut stream,
            "INSERT INTO public.t (id, name) VALUES (1, 'a')"
        ),
        "INSERT 0 1"
    );
    send_simple_query(&mut stream, "SELECT id, name FROM public.t LIMIT 10");
    let _ = read_message(&mut stream); // RowDescription
    let row = read_data_row(&mut stream);
    assert_eq!(row.first().cloned().flatten().as_deref(), Some("1"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    assert_eq!(simple_ok(&mut stream, "DROP TABLE public.t"), "DROP TABLE");

    simple_ok(&mut stream, "CREATE SCHEMA s1");
    for (sql, sqlstate) in [
        ("SELECT id FROM ghost.t LIMIT 1", "3F000"),
        ("SELECT id FROM s1.t LIMIT 1", "42P01"),
        ("CREATE TABLE s1.t (name TEXT)", "0A000"),
        ("SELECT id FROM pg_catalog.t LIMIT 1", "0A000"),
        ("SELECT id FROM pg_temp.t LIMIT 1", "0A000"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, sqlstate);
        read_ready_for_query(&mut stream);
    }
}

/// 拡張クエリ（Parse／Bind／Execute／Sync）でも `public.t` が動き、Parse 段で解決エラーになる。
#[test]
fn wire_extended_query_public_qualified_names() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    simple_ok(
        &mut stream,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    simple_ok(&mut stream, "INSERT INTO t (id, name) VALUES (1, 'a')");

    let sql = "SELECT id, name FROM public.t LIMIT 10";
    send_length_prefixed_message(&mut stream, b'P', &parse_body("s1", sql, 0));
    assert_eq!(read_message(&mut stream).0, b'1', "ParseComplete");
    send_length_prefixed_message(&mut stream, b'B', &bind_body("p1", "s1"));
    assert_eq!(read_message(&mut stream).0, b'2', "BindComplete");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 0));
    let mut rows = 0;
    loop {
        let (kind, _) = read_message(&mut stream);
        match kind {
            b'D' => rows += 1,
            b'C' => break,
            other => panic!("unexpected message {}", other as char),
        }
    }
    assert_eq!(rows, 1);
    send_length_prefixed_message(&mut stream, b'S', &[]);
    assert_eq!(read_message(&mut stream).0, b'Z');

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("s2", "SELECT id FROM ghost.t LIMIT 1", 0),
    );
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'E', "ErrorResponse");
    assert!(
        body.windows(6).any(|w| w == b"C3F000"),
        "SQLSTATE 3F000 expected"
    );
    send_length_prefixed_message(&mut stream, b'S', &[]);
    assert_eq!(read_message(&mut stream).0, b'Z');
}
