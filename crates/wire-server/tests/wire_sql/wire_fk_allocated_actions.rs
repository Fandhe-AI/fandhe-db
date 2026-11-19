//! 採番方式の親の `id` 参照 FK の参照アクション連鎖と遅延検査の簡易クエリ経由検証（Issue #3180。
//! ポインタ: TABLE-17・TABLE-23）。engine 側の網羅は `crates/engine/tests/table_rls/
//! table23_id_ref_fk_actions.rs`、本ファイルは psql 相当の末尾 `;` 付き単独送信での動作に徹する。

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

fn new_core_without_tables() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-fk-allocated-actions");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `spawn_server_with_engine`（`common/mod.rs`）と同型だが、`UserStore` を
/// `--ddl-allowed-users` 相当の許可主体付きで構築できるようにしたローカル版
/// （Issue #899。CLI 経由ではなく `UserStore::with_ddl_allowed_users` を直接呼ぶ
/// ため実バイナリを子プロセス起動しない。CLI の受理・拒否自体は
/// `wire_ddl_permission_cli.rs` が別途検証する）。
fn spawn_with_ddl_allowed_users(
    users_path: &std::path::Path,
    engine: Arc<EngineCore>,
    ddl_allowed_users: &[&str],
) -> SocketAddr {
    let store = UserStore::load_from_file(users_path).expect("valid user store");
    let store = if ddl_allowed_users.is_empty() {
        store
    } else {
        let names: Vec<String> = ddl_allowed_users.iter().map(|s| s.to_string()).collect();
        store
            .with_ddl_allowed_users(&names)
            .expect("ddl principals must reference known users")
    };
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

fn connect_as(addr: SocketAddr, username: &str, password: &str) -> TcpStream {
    authenticate_to_ready_for_query(addr, username, password)
}

fn exec_ok(stream: &mut TcpStream, sql: &str, tag: &str) {
    send_simple_query(stream, sql);
    assert_eq!(read_command_complete(stream), tag, "{sql}");
    read_ready_for_query(stream);
}

fn select_count(stream: &mut TcpStream, sql: &str, expected: usize) {
    send_simple_query(stream, sql);
    let _cols = read_row_description(stream);
    for _ in 0..expected {
        let _row = read_data_row(stream);
    }
    assert_eq!(read_command_complete(stream), format!("SELECT {expected}"));
    read_ready_for_query(stream);
}

/// 採番方式の親の `id` 参照 FK の ON DELETE 連鎖が、末尾 `;` 付きの単独送信で動く（TABLE-17・TABLE-23、
/// Issue #3180）。CASCADE は `DELETE 1` で完了して子が消え、NO ACTION で参照中の親は 23503。
#[test]
fn wire_allocated_parent_delete_runs_referential_actions() {
    let (core, _guard) = new_core_without_tables();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_with_ddl_allowed_users(&users_path, core, &["alice"]);
    let mut stream = connect_as(addr, "alice", "pw-alice");

    for (sql, tag) in [
        ("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT);", "CREATE TABLE"),
        (
            "CREATE TABLE posts (author BIGINT REFERENCES users (id) ON DELETE CASCADE, body TEXT);",
            "CREATE TABLE",
        ),
        (
            "CREATE TABLE notes (author BIGINT REFERENCES users (id) ON DELETE SET NULL, body TEXT);",
            "CREATE TABLE",
        ),
        (
            "CREATE TABLE locks (author BIGINT REFERENCES users (id), body TEXT);",
            "CREATE TABLE",
        ),
        ("INSERT INTO users (id, name) VALUES (1, 'a'), (2, 'b');", "INSERT 0 2"),
        ("INSERT INTO posts (author, body) VALUES (1, 'p');", "INSERT 0 1"),
        ("INSERT INTO notes (author, body) VALUES (1, 'n');", "INSERT 0 1"),
        ("INSERT INTO locks (author, body) VALUES (2, 'l');", "INSERT 0 1"),
    ] {
        exec_ok(&mut stream, sql, tag);
    }

    send_simple_query(&mut stream, "DELETE FROM users WHERE id = 2;");
    expect_error_response_with_sqlstate(&mut stream, "23503");
    read_ready_for_query(&mut stream);

    exec_ok(&mut stream, "DELETE FROM users WHERE id = 1;", "DELETE 1");
    select_count(&mut stream, "SELECT body FROM posts LIMIT 10", 0);
    select_count(
        &mut stream,
        "SELECT body FROM notes WHERE author IS NULL LIMIT 10",
        1,
    );
}

/// `INITIALLY DEFERRED` の違反は COMMIT で 23503 になり、接続は継続して使える（Issue #3180）。
#[test]
fn wire_allocated_parent_deferred_violation_fails_at_commit() {
    let (core, _guard) = new_core_without_tables();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_with_ddl_allowed_users(&users_path, core, &["alice"]);
    let mut stream = connect_as(addr, "alice", "pw-alice");

    for (sql, tag) in [
        ("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT);", "CREATE TABLE"),
        (
            "CREATE TABLE posts (author BIGINT, body TEXT, FOREIGN KEY (author) REFERENCES users (id) DEFERRABLE INITIALLY DEFERRED);",
            "CREATE TABLE",
        ),
        ("INSERT INTO users (id, name) VALUES (1, 'a');", "INSERT 0 1"),
        ("INSERT INTO posts (author, body) VALUES (1, 'x');", "INSERT 0 1"),
        ("BEGIN;", "BEGIN"),
        ("DELETE FROM users WHERE id = 1;", "DELETE 1"),
    ] {
        exec_ok(&mut stream, sql, tag);
    }
    send_simple_query(&mut stream, "COMMIT;");
    expect_error_response_with_sqlstate(&mut stream, "23503");
    read_ready_for_query(&mut stream);

    select_count(&mut stream, "SELECT name FROM users LIMIT 10", 1);
}

/// 台帳（Issue #3182）: `USING OPERATION_ID` つきの子 INSERT と親 DELETE（CASCADE）を末尾 `;` 付きの
/// 単独送信で再送すると `23505` になり、行も連鎖も二重にならず、同じ接続で次の文が実行できる。FK 違反で
/// 失敗した文は台帳を消費せず、親を作った後の同一 operation_id の再試行が成功する。
#[test]
fn wire_allocated_fk_resend_with_operation_id_is_idempotent() {
    let (core, _guard) = new_core_without_tables();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_with_ddl_allowed_users(&users_path, core, &["alice"]);
    let mut stream = connect_as(addr, "alice", "pw-alice");

    for (sql, tag) in [
        ("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT);", "CREATE TABLE"),
        (
            "CREATE TABLE posts (author BIGINT REFERENCES users (id) ON DELETE CASCADE, body TEXT);",
            "CREATE TABLE",
        ),
    ] {
        exec_ok(&mut stream, sql, tag);
    }
    let child = "INSERT INTO posts (author, body) VALUES (1, 'p') USING OPERATION_ID 'op-child';";
    // 親が無いので 23503。台帳は消費されない。
    send_simple_query(&mut stream, child);
    expect_error_response_with_sqlstate(&mut stream, "23503");
    read_ready_for_query(&mut stream);
    exec_ok(
        &mut stream,
        "INSERT INTO users (id, name) VALUES (1, 'a') USING OPERATION_ID 'op-parent';",
        "INSERT 0 1",
    );
    exec_ok(&mut stream, child, "INSERT 0 1");
    for _ in 0..2 {
        send_simple_query(&mut stream, child);
        expect_error_response_with_sqlstate(&mut stream, "23505");
        read_ready_for_query(&mut stream);
    }
    select_count(&mut stream, "SELECT body FROM posts LIMIT 10", 1);

    let del = "DELETE FROM users WHERE id = 1 USING OPERATION_ID 'op-del';";
    exec_ok(&mut stream, del, "DELETE 1");
    select_count(&mut stream, "SELECT body FROM posts LIMIT 10", 0);
    // 同じ id の親・子を作り直しても、DELETE の再送は 23505 で新しい行を巻き込まない。
    exec_ok(
        &mut stream,
        "INSERT INTO users (id, name) VALUES (1, 'again') USING OPERATION_ID 'op-parent2';",
        "INSERT 0 1",
    );
    exec_ok(
        &mut stream,
        "INSERT INTO posts (author, body) VALUES (1, 'q') USING OPERATION_ID 'op-child2';",
        "INSERT 0 1",
    );
    send_simple_query(&mut stream, del);
    expect_error_response_with_sqlstate(&mut stream, "23505");
    read_ready_for_query(&mut stream);
    select_count(&mut stream, "SELECT name FROM users LIMIT 10", 1);
    select_count(&mut stream, "SELECT body FROM posts LIMIT 10", 1);
}

/// 越境（Issue #3182）: 別テナントの 2 ユーザーが同一の `id` の親を持つとき、他テナントだけが持つ `id`
/// を指す子の INSERT は `23503` で、どこにも無い `id` の場合と応答（エラー本文）が一致し、他テナントの
/// 存在を示さない。親の DELETE の連鎖は自テナントの子だけに及ぶ。
#[test]
fn wire_allocated_fk_is_tenant_scoped_and_hides_foreign_parents() {
    let (core, _guard) = new_core_without_tables();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_with_ddl_allowed_users(&users_path, core, &["alice"]);
    let mut a = connect_as(addr, "alice", "pw-alice");
    let mut b = connect_as(addr, "bob", "pw-bob");

    for (sql, tag) in [
        ("CREATE TABLE users (id BIGINT PRIMARY KEY, name TEXT);", "CREATE TABLE"),
        (
            "CREATE TABLE posts (author BIGINT REFERENCES users (id) ON DELETE CASCADE, body TEXT);",
            "CREATE TABLE",
        ),
    ] {
        exec_ok(&mut a, sql, tag);
    }
    exec_ok(
        &mut a,
        "INSERT INTO users (id, name) VALUES (1, 'a');",
        "INSERT 0 1",
    );
    exec_ok(
        &mut a,
        "INSERT INTO posts (author, body) VALUES (1, 'pa');",
        "INSERT 0 1",
    );
    exec_ok(
        &mut b,
        "INSERT INTO users (id, name) VALUES (1, 'b'), (777, 'b7');",
        "INSERT 0 2",
    );
    exec_ok(
        &mut b,
        "INSERT INTO posts (author, body) VALUES (1, 'pb');",
        "INSERT 0 1",
    );

    // alice から見て 777 は他テナントだけが持つ id。どこにも無い id（4242）と応答が一致する。
    let mut bodies = Vec::new();
    for id in [777, 4242] {
        send_simple_query(
            &mut a,
            &format!("INSERT INTO posts (author, body) VALUES ({id}, 'x');"),
        );
        let (ty, body) = read_message(&mut a);
        assert_eq!(ty, b'E');
        let text = String::from_utf8_lossy(&body).to_string();
        assert!(text.contains("23503"), "{text:?}");
        assert!(
            !text.contains("tenant-b") && !text.contains("b7"),
            "{text:?}"
        );
        bodies.push(text.replace(&id.to_string(), "<id>"));
        read_ready_for_query(&mut a);
    }
    assert_eq!(bodies[0], bodies[1]);

    // alice の親の削除は alice の子だけを連鎖で消し、bob の同値 id の子は残る。
    exec_ok(&mut a, "DELETE FROM users WHERE id = 1;", "DELETE 1");
    select_count(&mut a, "SELECT body FROM posts LIMIT 10", 0);
    select_count(&mut b, "SELECT body FROM posts LIMIT 10", 1);
}
