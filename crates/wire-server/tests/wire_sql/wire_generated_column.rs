//! 生成列 `GENERATED ALWAYS AS (<式>) STORED` の宣言（`CREATE TABLE`）の wire 越し検証（層 A。
//! Issue #3327。ポインタ: `docs/spec/04-behavior/` の TABLE-50、ERR-6）。
//!
//! 宣言の意味論・SQLSTATE の表は engine の `table50_generated_column_create` が確定オラクルとして
//! 検証済みのため、本ファイルは (1) psql 形（末尾 `;` を付けた単独の simple query）で生成列つきの
//! `CREATE TABLE` が `CommandComplete` で通ること、(2) 拒否形が `ErrorResponse` の SQLSTATE で返ること、
//! (3) INSERT・UPDATE・UPSERT の計算（Issue #3328）と生成列への値の指定の `428C9`、を pg wire 越しに確認する。

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

/// psql 形（末尾 `;` 付きの単独送信）の INSERT・UPDATE・UPSERT で生成列が計算され、SELECT が計算後の値を返す
/// （Issue #3328）。
#[test]
fn insert_update_upsert_compute_the_generated_column() {
    let (mut stream, _guard) = connect();
    send_simple_query(
        &mut stream,
        "CREATE TABLE g (k INTEGER PRIMARY KEY, a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO g (k, a) VALUES (1, 1) USING OPERATION_ID 'wire-gen-1';",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "UPDATE g SET a = 5 WHERE k = 1 USING OPERATION_ID 'wire-gen-2';",
    );
    assert_eq!(read_command_complete(&mut stream), "UPDATE 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO g (k, a) VALUES (1, 7) ON CONFLICT (k) DO UPDATE SET a = EXCLUDED.a USING OPERATION_ID 'wire-gen-3';",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT a, b FROM g;");
    let _ = read_row_description(&mut stream);
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("7".to_string()), Some("14".to_string())]
    );
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// 生成列への値の指定は `428C9`。拒否の後も接続は使える。
#[test]
fn explicit_value_for_generated_column_is_rejected_with_428c9() {
    let (mut stream, _guard) = connect();
    send_simple_query(
        &mut stream,
        "CREATE TABLE g (a INTEGER, b INTEGER GENERATED ALWAYS AS (a * 2) STORED);",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);

    for sql in [
        "INSERT INTO g (a, b) VALUES (1, 2) USING OPERATION_ID 'wire-gen-4';",
        "UPDATE g SET b = 1 WHERE a = 1 USING OPERATION_ID 'wire-gen-5';",
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "428C9");
        read_ready_for_query(&mut stream);
    }
}
