//! 生成列 `GENERATED ALWAYS AS (<式>) STORED` の宣言（`CREATE TABLE`）の wire 越し検証（層 A。
//! Issue #3327。ポインタ: `docs/spec/04-behavior/` の TABLE-50、ERR-6）。
//!
//! 宣言の意味論・SQLSTATE の表は engine の `table50_generated_column_create` が確定オラクルとして
//! 検証済みのため、本ファイルは (1) psql 形（末尾 `;` を付けた単独の simple query）で生成列つきの
//! `CREATE TABLE` が `CommandComplete` で通ること、(2) 拒否形が `ErrorResponse` の SQLSTATE で返ること、
//! (3) 計算点が未結線の間、生成列を持つ表への INSERT が `0A000` で返ること、を pg wire 越しに確認する。

use crate::common;

use crate::temp_db;

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-generated-column");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// 許可主体つき（`--ddl-allowed-users` 相当）のサーバーを起動する（`wire_create_table` と同型）。
fn spawn_with_ddl_user(users_path: &std::path::Path, engine: Arc<EngineCore>) -> SocketAddr {
    let store = UserStore::load_from_file(users_path).expect("valid user store");
    let store = store
        .with_ddl_allowed_users(&["alice".to_string()])
        .expect("ddl principals must reference known users");
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

fn connect() -> (TcpStream, temp_db::CleanupGuard) {
    let (core, guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_with_ddl_user(&users_path, core);
    (
        authenticate_to_ready_for_query(addr, "alice", "pw-alice"),
        guard,
    )
}

/// psql 形（末尾 `;`）の単独 simple query で生成列つきの表を作れる。
#[test]
fn psql_shaped_create_table_with_generated_column_succeeds() {
    let (mut stream, _guard) = connect();
    send_simple_query(
        &mut stream,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);
}

/// 拒否形は決めた SQLSTATE の `ErrorResponse` で返り、接続は使い続けられる。
#[test]
fn rejected_forms_return_sqlstate() {
    let (mut stream, _guard) = connect();
    for (sql, sqlstate) in [
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (a) VIRTUAL);",
            "42601",
        ),
        (
            "CREATE TABLE r (a INTEGER, b TIMESTAMPTZ GENERATED ALWAYS AS (now()) STORED);",
            "42P17",
        ),
        (
            "CREATE TABLE r (a INTEGER, b INTEGER GENERATED ALWAYS AS (zzz) STORED);",
            "42703",
        ),
        (
            "CREATE TABLE r (a INTEGER, b TEXT GENERATED ALWAYS AS (a + 1) STORED);",
            "42804",
        ),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, sqlstate);
        read_ready_for_query(&mut stream);
    }
    // 拒否の後も表は残らず、同名で作れる。
    send_simple_query(&mut stream, "CREATE TABLE r (a INTEGER);");
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);
}

/// 計算点が未結線の間、生成列を持つ表への書き込みは `0A000`。
#[test]
// 書き込み経路の暫定停止（#3328・#3329 が結線して外す）。
// tracked: #3328, #3329
fn insert_into_generated_table_is_rejected_with_0a000() {
    let (mut stream, _guard) = connect();
    send_simple_query(
        &mut stream,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO g (a) VALUES (1) USING OPERATION_ID 'wire-gen-1';",
    );
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    read_ready_for_query(&mut stream);
}
