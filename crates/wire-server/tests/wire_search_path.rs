//! セッション変数 `search_path`（TABLE-27・Issue #1715）の簡易・拡張クエリプロトコル経由
//! （生バイトクライアント）検証（層 A）。
//!
//! 意味論そのものは `crates/engine/tests/table27_search_path.rs` が確定オラクル。本ファイルは
//! `wire_schema_qualified.rs` と同じ流儀で、`SET`／`RESET`／`SHOW` の応答タグ・行、handshake の
//! username による `"$user"` の展開、Parse 後に `SET search_path` された拡張クエリの文が陳腐化せず
//! 再検証されること（`$n` あり・なし・Describe）、ROLLBACK での取り消しに徹する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-search-path");
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

fn simple_ok(stream: &mut TcpStream, sql: &str) -> String {
    send_simple_query(stream, sql);
    let tag = read_command_complete(stream);
    read_ready_for_query(stream);
    tag
}

fn simple_err(stream: &mut TcpStream, sql: &str, sqlstate: &str) {
    send_simple_query(stream, sql);
    expect_error_response_with_sqlstate(stream, sqlstate);
    read_ready_for_query(stream);
}

fn show(stream: &mut TcpStream) -> String {
    send_simple_query(stream, "SHOW search_path");
    let columns = read_row_description(stream);
    assert_eq!(columns, vec!["search_path".to_string()]);
    let row = read_data_row(stream);
    assert_eq!(read_command_complete(stream), "SHOW");
    read_ready_for_query(stream);
    row.first().cloned().flatten().expect("one text cell")
}

/// 値 1 個（テキスト形式）を束縛する Bind メッセージ本体。
fn bind_body_one_param(portal: &str, statement: &str, value: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes()); // param format code count
    body.extend_from_slice(&1i16.to_be_bytes()); // param count
    body.extend_from_slice(&(value.len() as i32).to_be_bytes());
    body.extend_from_slice(value.as_bytes());
    body.extend_from_slice(&0i16.to_be_bytes()); // result format code count
    body
}

fn expect_error_message(stream: &mut TcpStream, sqlstate: &str) {
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'E', "ErrorResponse");
    let needle = format!("C{sqlstate}");
    assert!(
        body.windows(needle.len()).any(|w| w == needle.as_bytes()),
        "SQLSTATE {sqlstate} expected, got {:?}",
        String::from_utf8_lossy(&body)
    );
}

fn sync(stream: &mut TcpStream) {
    send_length_prefixed_message(stream, b'S', &[]);
    assert_eq!(read_message(stream).0, b'Z');
}

/// 簡易クエリの `SET`／`RESET`／`SHOW` の応答（タグ・列名・行・`SET LOCAL` の拒否・ROLLBACK での取り消し）。
#[test]
fn wire_simple_query_set_show_reset_and_rollback() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    assert_eq!(show(&mut stream), "\"$user\", public");
    assert_eq!(
        simple_ok(&mut stream, "SET search_path TO a, public"),
        "SET"
    );
    assert_eq!(show(&mut stream), "a, public");
    assert_eq!(simple_ok(&mut stream, "RESET search_path"), "RESET");
    assert_eq!(show(&mut stream), "\"$user\", public");
    assert_eq!(
        simple_ok(&mut stream, "SET SESSION search_path = DEFAULT"),
        "SET"
    );
    simple_err(&mut stream, "SET LOCAL search_path TO a", "0A000");
    simple_err(&mut stream, "SET search_path TO ''", "22023");

    assert_eq!(simple_ok(&mut stream, "BEGIN"), "BEGIN");
    assert_eq!(simple_ok(&mut stream, "SET search_path TO x"), "SET");
    assert_eq!(show(&mut stream), "x");
    assert_eq!(simple_ok(&mut stream, "ROLLBACK"), "ROLLBACK");
    assert_eq!(show(&mut stream), "\"$user\", public");
}

/// handshake の username が `"$user"` に展開される（同名の SCHEMA があれば作成先は `0A000`、無ければ
/// `public`）。`public` を含まない path では一時表以外の未修飾名が `42P01` で届く。
#[test]
fn wire_dollar_user_expands_from_the_authenticated_username() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    assert_eq!(
        simple_ok(
            &mut stream,
            "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)"
        ),
        "CREATE TABLE"
    );
    assert_eq!(
        simple_ok(&mut stream, "CREATE SCHEMA alice"),
        "CREATE SCHEMA"
    );
    simple_err(
        &mut stream,
        "CREATE TABLE u (id BIGINT PRIMARY KEY)",
        "0A000",
    );
    assert_eq!(
        simple_ok(&mut stream, "CREATE TABLE public.u (id BIGINT PRIMARY KEY)"),
        "CREATE TABLE"
    );
    assert_eq!(simple_ok(&mut stream, "SET search_path TO nosuch"), "SET");
    simple_err(&mut stream, "SELECT id FROM t LIMIT 1", "42P01");
    simple_err(
        &mut stream,
        "CREATE TABLE w (id BIGINT PRIMARY KEY)",
        "3F000",
    );
    send_simple_query(&mut stream, "SELECT id FROM public.t LIMIT 1");
    let _ = read_row_description(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 0");
    read_ready_for_query(&mut stream);
}

/// Parse 後に（簡易クエリで）`SET search_path` すると、Bind／Describe は新しい path で検証し直す
/// （`$n` なし・あり）。`public` が見える path へ戻せば同じ準備済み文が再び実行できる。
#[test]
fn wire_extended_query_revalidates_after_search_path_changes() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    simple_ok(
        &mut stream,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, name TEXT)",
    );
    simple_ok(&mut stream, "INSERT INTO t (id, name) VALUES (1, 'a')");

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("plain", "SELECT id FROM t LIMIT 5", 0),
    );
    assert_eq!(read_message(&mut stream).0, b'1');
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("param", "SELECT id FROM t WHERE id = $1 LIMIT 5", 0),
    );
    assert_eq!(read_message(&mut stream).0, b'1');
    sync(&mut stream);

    // 既定の path では Bind／Execute できる。
    send_length_prefixed_message(&mut stream, b'B', &bind_body("p0", "plain"));
    assert_eq!(read_message(&mut stream).0, b'2');
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p0", 0));
    loop {
        let (kind, _) = read_message(&mut stream);
        match kind {
            b'D' => {}
            b'C' => break,
            other => panic!("unexpected message {}", other as char),
        }
    }
    sync(&mut stream);

    assert_eq!(simple_ok(&mut stream, "SET search_path TO nosuch"), "SET");

    // `$n` なし。
    send_length_prefixed_message(&mut stream, b'B', &bind_body("p1", "plain"));
    expect_error_message(&mut stream, "42P01");
    sync(&mut stream);
    // `$n` あり。
    send_length_prefixed_message(&mut stream, b'B', &bind_body_one_param("p2", "param", "1"));
    expect_error_message(&mut stream, "42P01");
    sync(&mut stream);
    // Describe（Statement）。
    send_length_prefixed_message(&mut stream, b'D', b"Splain\0");
    expect_error_message(&mut stream, "42P01");
    sync(&mut stream);
    // Parse 自体も新しい path で検証される。
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("late", "SELECT id FROM t LIMIT 5", 0),
    );
    expect_error_message(&mut stream, "42P01");
    sync(&mut stream);

    // path を戻せば同じ準備済み文が実行できる。
    assert_eq!(simple_ok(&mut stream, "RESET search_path"), "RESET");
    send_length_prefixed_message(&mut stream, b'B', &bind_body_one_param("p3", "param", "1"));
    assert_eq!(read_message(&mut stream).0, b'2');
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p3", 0));
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
    sync(&mut stream);
}
/// DataRow（`D`）本体の先頭 1 セルをテキストで取り出す（NULL は `None`）。
fn first_cell(body: &[u8]) -> Option<String> {
    let len = i32::from_be_bytes(body.get(2..6)?.try_into().ok()?);
    if len < 0 {
        return None;
    }
    let end = 6usize.checked_add(usize::try_from(len).ok()?)?;
    Some(String::from_utf8_lossy(body.get(6..end)?).into_owned())
}

/// 簡易クエリの `current_schema()`・`current_schemas(true)`（TABLE-27 (7)・Issue #1718）。
/// RowDescription の型 OID（text 25／text[] 1009）、handshake の username と同名の SCHEMA、
/// 裸の `current_schema`（Rails 形）、`SET search_path` への追随を確かめる。
#[test]
fn wire_simple_query_current_schema_functions() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "SELECT current_schema(), current_schemas(true)",
    );
    let cols = read_row_description_with_oids(&mut stream);
    assert_eq!(
        cols,
        vec![
            ("current_schema".to_string(), 25),
            ("current_schemas".to_string(), 1009)
        ]
    );
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("public"));
    assert_eq!(row[1].as_deref(), Some("{pg_catalog,public}"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    simple_ok(&mut stream, "CREATE SCHEMA alice");
    send_simple_query(&mut stream, "SELECT current_schema");
    assert_eq!(read_row_description(&mut stream), vec!["current_schema"]);
    assert_eq!(read_data_row(&mut stream)[0].as_deref(), Some("alice"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    simple_ok(&mut stream, "SET search_path TO nosuch");
    send_simple_query(
        &mut stream,
        "SELECT current_schema(), current_schemas(false)",
    );
    let _ = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0], None);
    assert_eq!(row[1].as_deref(), Some("{}"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// 拡張クエリ: Parse 済みの `SELECT current_schema()` は、Parse 後の `SET search_path` の値を
/// Execute のたびに束縛し直して返す（陳腐化しない）。Describe は型を返す。
#[test]
fn wire_extended_query_current_schema_follows_set() {
    let (core, _guard) = new_core();
    let store = allowed_store(&[("alice", "tenant-a", "correct-horse")], &["alice"]);
    let addr = spawn_server(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("cs", "SELECT current_schema()", 0),
    );
    assert_eq!(read_message(&mut stream).0, b'1');
    send_length_prefixed_message(&mut stream, b'D', b"Scs\0");
    assert_eq!(read_message(&mut stream).0, b't');
    assert_eq!(read_message(&mut stream).0, b'T');
    sync(&mut stream);

    let run = |stream: &mut TcpStream, portal: &str| -> Option<String> {
        send_length_prefixed_message(stream, b'B', &bind_body(portal, "cs"));
        assert_eq!(read_message(stream).0, b'2');
        send_length_prefixed_message(stream, b'E', &execute_body(portal, 0));
        let mut cell = None;
        loop {
            let (kind, body) = read_message(stream);
            match kind {
                b'D' => cell = first_cell(&body),
                b'C' => break,
                other => panic!("unexpected message {}", other as char),
            }
        }
        sync(stream);
        cell
    };
    assert_eq!(run(&mut stream, "p0").as_deref(), Some("public"));
    simple_ok(&mut stream, "SET search_path TO nosuch");
    assert_eq!(run(&mut stream, "p1"), None);
    simple_ok(&mut stream, "RESET search_path");
    assert_eq!(run(&mut stream, "p2").as_deref(), Some("public"));
}
