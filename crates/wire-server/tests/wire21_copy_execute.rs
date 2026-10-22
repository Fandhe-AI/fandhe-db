//! 拡張クエリプロトコルの Execute から COPY（CopyIn／CopyOut）を開始して完了させる
//! 結合テスト（Issue #1760・WIRE-21 (2) のポインタ。親 Issue #1758）。
//!
//! Parse／Bind／Describe は `wire21_copy_parse_bind.rs`（Issue #1759）が担う。本ファイルは
//! Execute → CopyInResponse／CopyOutResponse → CopyData／CopyDone／CopyFail →
//! `CommandComplete "COPY n"` までと、完了タグ・トランザクション状態が簡易クエリの COPY と
//! 同じであることを固定する。CopyFail 後の Sync 同期回復の詳細は Issue #1761 の担当で、
//! ここでは回復に必要な最小限（Sync で ReadyForQuery に戻る）だけを確認する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::{Shutdown, TcpStream};
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

const COPY_FROM: &str = "COPY documents (id, embedding, body) FROM STDIN";
const COPY_TO: &str = "COPY (SELECT id, body FROM documents LIMIT 100) TO STDOUT";

const ROW1: &[u8] = b"1\t[1.0,0.0,0.0]\talpha\n";
const ROW2: &[u8] = b"2\t[0.0,1.0,0.0]\tbeta\n";

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire21-copy-execute");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    (core, guard)
}

fn connect_as(core: Arc<EngineCore>, user: &str) -> TcpStream {
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);
    let pw = if user == "alice" {
        "pw-alice"
    } else {
        "pw-bob"
    };
    authenticate_to_ready_for_query(addr, user, pw)
}

fn connect(core: Arc<EngineCore>) -> TcpStream {
    connect_as(core, "alice")
}

fn send(stream: &mut TcpStream, kind: u8, body: &[u8]) {
    send_length_prefixed_message(stream, kind, body);
}

fn sync(stream: &mut TcpStream) {
    send(stream, b'S', b"");
}

fn flush(stream: &mut TcpStream) {
    send(stream, b'H', b"");
}

fn read_until_ready(stream: &mut TcpStream) -> u8 {
    loop {
        let (kind, body) = read_message(stream);
        if kind == b'Z' {
            return body[0];
        }
    }
}

fn simple(stream: &mut TcpStream, sql: &str) -> u8 {
    send_simple_query(stream, sql);
    read_until_ready(stream)
}

/// 簡易クエリで `documents` の行数（現在のテナント分）を数える。
fn count_rows(stream: &mut TcpStream) -> usize {
    send_simple_query(stream, "SELECT id FROM documents LIMIT 100");
    let mut rows = 0;
    loop {
        let (kind, _) = read_message(stream);
        match kind {
            b'D' => rows += 1,
            b'Z' => return rows,
            _ => {}
        }
    }
}

/// Parse／Bind を送り、ParseComplete／BindComplete を確認する（Execute は送らない）。
fn parse_bind(stream: &mut TcpStream, sql: &str, portal: &str) {
    send(stream, b'P', &parse_body("", sql, 0));
    send(stream, b'B', &bind_body(portal, ""));
    expect(stream, b'1');
    expect(stream, b'2');
}

fn execute(stream: &mut TcpStream, portal: &str) {
    send(stream, b'E', &execute_body(portal, 0));
}

fn expect(stream: &mut TcpStream, want: u8) -> Vec<u8> {
    let (kind, body) = read_message(stream);
    assert_eq!(
        kind,
        want,
        "expected {}, got {}: {:?}",
        want as char,
        kind as char,
        String::from_utf8_lossy(&body)
    );
    body
}

fn expect_command_complete(stream: &mut TcpStream, tag: &str) {
    let body = expect(stream, b'C');
    assert_eq!(body, [tag.as_bytes(), b"\0"].concat());
}

fn copy_fail(stream: &mut TcpStream, reason: &str) {
    send(stream, b'f', &[reason.as_bytes(), b"\0"].concat());
}

/// 拡張クエリで COPY FROM を実行し 2 行を投入する（Sync まで完了させる）。
fn load_two_rows_extended(stream: &mut TcpStream) {
    parse_bind(stream, COPY_FROM, "load");
    execute(stream, "load");
    flush(stream);
    expect(stream, b'G');
    send(stream, b'd', ROW1);
    send(stream, b'd', ROW2);
    send(stream, b'c', b"");
    expect_command_complete(stream, "COPY 2");
    sync(stream);
    assert_eq!(expect(stream, b'Z'), b"I");
}

/// Execute → Flush で CopyInResponse を受け、CopyData／CopyDone で `COPY 2`。
#[test]
fn copy_in_succeeds_with_flush_driven_client() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    parse_bind(&mut s, COPY_FROM, "p");
    execute(&mut s, "p");
    flush(&mut s);
    let body = expect(&mut s, b'G');
    assert_eq!(&body[1..3], 3i16.to_be_bytes());
    send(&mut s, b'd', ROW1);
    send(&mut s, b'd', ROW2);
    send(&mut s, b'c', b"");
    expect_command_complete(&mut s, "COPY 2");
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"I");
    assert_eq!(count_rows(&mut s), 2);
    s.shutdown(Shutdown::Write).ok();
}

/// libpq／psycopg 型の順序（Parse／Bind／Describe／Execute／Sync を一括送信）。copy-in 中に
/// 届いた最初の Sync は無視され、ReadyForQuery は完了後にちょうど 1 回だけ来る。
#[test]
fn copy_in_succeeds_with_libpq_style_batch_and_single_ready() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    send(&mut s, b'P', &parse_body("", COPY_FROM, 0));
    send(&mut s, b'B', &bind_body("", ""));
    send(&mut s, b'D', b"P\0");
    execute(&mut s, "");
    sync(&mut s);
    expect(&mut s, b'1');
    expect(&mut s, b'2');
    expect(&mut s, b'n');
    expect(&mut s, b'G');
    send(&mut s, b'd', ROW1);
    send(&mut s, b'c', b"");
    expect_command_complete(&mut s, "COPY 1");
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"I");
    // RFQ が余っていない（次の簡易クエリの応答が先頭に来て行数が数えられる）。
    assert_eq!(count_rows(&mut s), 1);
    s.shutdown(Shutdown::Write).ok();
}

#[test]
fn copy_fail_aborts_without_rows_and_recovers_on_sync() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    parse_bind(&mut s, COPY_FROM, "p");
    execute(&mut s, "p");
    flush(&mut s);
    expect(&mut s, b'G');
    send(&mut s, b'd', ROW1);
    copy_fail(&mut s, "abort");
    let body = expect(&mut s, b'E');
    assert!(String::from_utf8_lossy(&body).contains("22000"));
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"I");
    assert_eq!(count_rows(&mut s), 0);
    s.shutdown(Shutdown::Write).ok();
}

#[test]
fn decode_failure_commits_nothing() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    parse_bind(&mut s, COPY_FROM, "p");
    execute(&mut s, "p");
    flush(&mut s);
    expect(&mut s, b'G');
    send(
        &mut s,
        b'd',
        b"1\t[1.0,0.0,0.0]\talpha\n2\t[1.0,0.0]\tbeta\n",
    );
    send(&mut s, b'c', b"");
    let body = expect(&mut s, b'E');
    assert!(String::from_utf8_lossy(&body).contains("22000"));
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"I");
    assert_eq!(count_rows(&mut s), 0);
    s.shutdown(Shutdown::Write).ok();
}

#[test]
fn copy_in_inside_explicit_transaction_commits_on_commit() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    assert_eq!(simple(&mut s, "BEGIN"), b'T');
    parse_bind(&mut s, COPY_FROM, "p");
    execute(&mut s, "p");
    flush(&mut s);
    expect(&mut s, b'G');
    send(&mut s, b'd', ROW1);
    send(&mut s, b'c', b"");
    expect_command_complete(&mut s, "COPY 1");
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"T");
    assert_eq!(simple(&mut s, "COMMIT"), b'I');
    assert_eq!(count_rows(&mut s), 1);
    s.shutdown(Shutdown::Write).ok();
}

#[test]
fn copy_fail_inside_explicit_transaction_fails_the_transaction() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    assert_eq!(simple(&mut s, "BEGIN"), b'T');
    parse_bind(&mut s, COPY_FROM, "p");
    execute(&mut s, "p");
    flush(&mut s);
    expect(&mut s, b'G');
    send(&mut s, b'd', ROW1);
    copy_fail(&mut s, "abort");
    expect(&mut s, b'E');
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"E");
    assert_eq!(simple(&mut s, "ROLLBACK"), b'I');
    assert_eq!(count_rows(&mut s), 0);
    s.shutdown(Shutdown::Write).ok();
}

/// 完了済み portal の再 Execute はタグだけを再送し、CopyInResponse も行も増やさない。
#[test]
fn re_execute_of_done_portal_resends_tag_only() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    parse_bind(&mut s, COPY_FROM, "p");
    execute(&mut s, "p");
    flush(&mut s);
    expect(&mut s, b'G');
    send(&mut s, b'd', ROW1);
    send(&mut s, b'c', b"");
    expect_command_complete(&mut s, "COPY 1");
    execute(&mut s, "p");
    expect_command_complete(&mut s, "COPY 1");
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"I");
    assert_eq!(count_rows(&mut s), 1);
    s.shutdown(Shutdown::Write).ok();
}

#[test]
fn terminate_during_copy_in_closes_connection_without_rows() {
    let (core, _g) = new_core();
    let mut s = connect(Arc::clone(&core));
    parse_bind(&mut s, COPY_FROM, "p");
    execute(&mut s, "p");
    flush(&mut s);
    expect(&mut s, b'G');
    send(&mut s, b'd', ROW1);
    send(&mut s, b'X', b"");
    expect_connection_closed(&mut s);
    let mut s2 = connect(core);
    assert_eq!(count_rows(&mut s2), 0);
}

#[test]
fn non_copy_message_during_copy_in_is_protocol_violation() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    parse_bind(&mut s, COPY_FROM, "p");
    execute(&mut s, "p");
    flush(&mut s);
    expect(&mut s, b'G');
    send(&mut s, b'Q', b"SELECT 1\0");
    expect_error_response_with_sqlstate(&mut s, "08P01");
    expect_connection_closed(&mut s);
}

/// Parse／Bind 後に表が DROP されたら、Execute は CopyInResponse を送らず `42P01` で拒否する。
#[test]
fn dropped_table_between_bind_and_execute_is_rejected_before_copy_in() {
    let (core, _g) = new_core();
    // DROP TABLE は DDL 許可主体のみ（`--ddl-allowed-users` 相当）。
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let store = wire_server::auth::UserStore::load_from_file(&users_path)
        .expect("user store")
        .with_ddl_allowed_users(&["alice".to_string()])
        .expect("ddl principal");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let limiter = wire_server::limits::ConnectionLimiter::new(16);
    let engine = Arc::clone(&core);
    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            Arc::new(store),
            engine,
            limiter,
            std::time::Duration::from_secs(5),
        );
    });
    let mut s = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    parse_bind(&mut s, COPY_FROM, "p");
    send_simple_query(&mut s, "DROP TABLE documents");
    expect(&mut s, b'C');
    assert_eq!(read_until_ready(&mut s), b'I');
    execute(&mut s, "p");
    let body = expect(&mut s, b'E');
    assert!(String::from_utf8_lossy(&body).contains("42P01"));
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"I");
    s.shutdown(Shutdown::Write).ok();
}

#[test]
fn copy_to_stdout_streams_rows_and_completes() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    load_two_rows_extended(&mut s);
    parse_bind(&mut s, COPY_TO, "o");
    execute(&mut s, "o");
    flush(&mut s);
    let body = expect(&mut s, b'H');
    assert_eq!(&body[1..3], 2i16.to_be_bytes());
    expect(&mut s, b'd');
    expect(&mut s, b'd');
    expect(&mut s, b'c');
    expect_command_complete(&mut s, "COPY 2");
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"I");
    s.shutdown(Shutdown::Write).ok();
}

/// 'G'/'H' から CommandComplete までのバイト列（kind, body）を集める。
fn collect_until_command_complete(stream: &mut TcpStream) -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    loop {
        let m = read_message(stream);
        let done = m.0 == b'C';
        out.push(m);
        if done {
            return out;
        }
    }
}

/// 同じ入力に対し、簡易クエリの COPY と拡張クエリの COPY が同一のバイト列を返す。
#[test]
fn extended_copy_responses_match_simple_query_byte_for_byte() {
    let (core_a, _ga) = new_core();
    let mut simple_conn = connect(core_a);
    send_simple_query(&mut simple_conn, COPY_FROM);
    let mut simple_in = vec![read_message(&mut simple_conn)];
    send(&mut simple_conn, b'd', ROW1);
    send(&mut simple_conn, b'd', ROW2);
    send(&mut simple_conn, b'c', b"");
    simple_in.extend(collect_until_command_complete(&mut simple_conn));
    assert_eq!(read_until_ready(&mut simple_conn), b'I');
    send_simple_query(&mut simple_conn, COPY_TO);
    let simple_out = collect_until_command_complete(&mut simple_conn);
    assert_eq!(read_until_ready(&mut simple_conn), b'I');

    let (core_b, _gb) = new_core();
    let mut ext = connect(core_b);
    parse_bind(&mut ext, COPY_FROM, "p");
    execute(&mut ext, "p");
    flush(&mut ext);
    let mut ext_in = vec![read_message(&mut ext)];
    send(&mut ext, b'd', ROW1);
    send(&mut ext, b'd', ROW2);
    send(&mut ext, b'c', b"");
    ext_in.extend(collect_until_command_complete(&mut ext));
    sync(&mut ext);
    assert_eq!(expect(&mut ext, b'Z'), b"I");
    parse_bind(&mut ext, COPY_TO, "o");
    execute(&mut ext, "o");
    flush(&mut ext);
    let ext_out = collect_until_command_complete(&mut ext);
    sync(&mut ext);
    assert_eq!(expect(&mut ext, b'Z'), b"I");

    assert_eq!(simple_in, ext_in);
    assert_eq!(simple_out, ext_out);
}

/// tenant-a が拡張クエリの COPY で投入した行は tenant-b から見えない。CopyFail のエラー応答は
/// テナントによらずバイト同一。
#[test]
fn extended_copy_respects_tenant_boundary() {
    let (core, _g) = new_core();
    let mut alice = connect_as(Arc::clone(&core), "alice");
    let mut bob = connect_as(Arc::clone(&core), "bob");
    load_two_rows_extended(&mut alice);
    assert_eq!(count_rows(&mut alice), 2);
    assert_eq!(count_rows(&mut bob), 0);

    let mut errors = Vec::new();
    for s in [&mut alice, &mut bob] {
        parse_bind(s, COPY_FROM, "p");
        execute(s, "p");
        flush(s);
        expect(s, b'G');
        copy_fail(s, "abort");
        errors.push(expect(s, b'E'));
        sync(s);
        assert_eq!(expect(s, b'Z'), b"I");
    }
    assert_eq!(errors[0], errors[1]);
}
