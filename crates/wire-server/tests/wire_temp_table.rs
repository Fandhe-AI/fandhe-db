//! `CREATE TEMP TABLE`・一時表の `DROP TABLE`・`ON COMMIT`（TABLE-44・Issue #1683）の
//! pg wire v3 越し（生バイトクライアント）検証。簡易クエリと拡張クエリ（Parse/Bind/Execute）の
//! 両方で、DDL 権限を持たない接続でも一時表を作成・削除できること、一時表を作った後も
//! 2 個目の `CREATE TEMP TABLE` と一時表の `DROP` が拡張クエリで通ること（暫定ゲートの
//! 例外）、`ON COMMIT DROP` の結果、別接続からは一時表が見えないことを確認する。
//!
//! 一時表の意味論（名前解決・上限・fail-closed な拒否）は
//! `crates/engine/tests/table44_temp_table_ddl.rs` が確定オラクルで、本ファイルは wire の
//! フレーミング越しに CommandComplete タグと接続維持が正しいことに徹する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-temp-table");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn send_sync(stream: &mut TcpStream) {
    send_length_prefixed_message(stream, b'S', b"");
}

fn assert_ready_for_query(stream: &mut TcpStream) {
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery");
}

/// 拡張クエリで DDL を 1 文実行し、CommandComplete のタグを返す。
fn extended_command(stream: &mut TcpStream, name: &str, sql: &str) -> String {
    send_length_prefixed_message(stream, b'P', &parse_body(name, sql, 0));
    let (kind, body) = read_message(stream);
    assert_eq!(
        kind,
        b'1',
        "expected ParseComplete for {sql}: {}",
        String::from_utf8_lossy(&body)
    );
    send_length_prefixed_message(stream, b'B', &bind_body(name, name));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'2', "expected BindComplete");
    send_length_prefixed_message(stream, b'E', &execute_body(name, 0));
    let (kind, tag) = read_message(stream);
    assert_eq!(kind, b'C', "expected CommandComplete for {sql}");
    let tag = String::from_utf8_lossy(&tag[..tag.len() - 1]).into_owned();
    send_sync(stream);
    assert_ready_for_query(stream);
    tag
}

/// 簡易クエリ: DDL 権限の無い接続でも作成・削除でき、重複は `42P07`、
/// 永続表の DDL は `42501` のまま。接続は維持される。
#[test]
fn simple_query_creates_and_drops_temp_tables_without_ddl_permission() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "CREATE TEMP TABLE t (a TEXT)");
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "CREATE TEMP TABLE t (a TEXT)");
    expect_error_response_with_sqlstate(&mut stream, "42P07");
    read_ready_for_query(&mut stream);

    // 一時表を持つ接続でも、2 個目の CREATE TEMP と一時表の DROP が通る。
    send_simple_query(
        &mut stream,
        "CREATE GLOBAL TEMPORARY TABLE u (a TEXT) ON COMMIT PRESERVE ROWS",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP TABLE t, u");
    assert_eq!(read_command_complete(&mut stream), "DROP TABLE");
    read_ready_for_query(&mut stream);

    // 一時表でなくなった名前は永続側の経路になり、DDL 権限が無ければ 42501。
    send_simple_query(&mut stream, "DROP TABLE t");
    expect_error_response_with_sqlstate(&mut stream, "42501");
    read_ready_for_query(&mut stream);
    send_simple_query(&mut stream, "CREATE TABLE p (a TEXT)");
    expect_error_response_with_sqlstate(&mut stream, "42501");
    read_ready_for_query(&mut stream);

    // ON COMMIT DROP は自動コミットの文の終わりで消えるため、再作成できる。
    for _ in 0..2 {
        send_simple_query(&mut stream, "CREATE TEMP TABLE d (a TEXT) ON COMMIT DROP");
        assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
        read_ready_for_query(&mut stream);
    }
}

/// 拡張クエリ: 一時表を作った後も、粗いゲートの例外により 2 個目の CREATE TEMP・
/// 一時表の DROP・ON COMMIT DROP の再作成が通る。
#[test]
fn extended_query_creates_and_drops_temp_tables() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    assert_eq!(
        extended_command(&mut stream, "s1", "CREATE TEMP TABLE t (a TEXT)"),
        "CREATE TABLE"
    );
    assert_eq!(
        extended_command(&mut stream, "s2", "CREATE TEMP TABLE u (a TEXT)"),
        "CREATE TABLE"
    );
    assert_eq!(
        extended_command(
            &mut stream,
            "s3",
            "CREATE TEMPORARY TABLE d (a TEXT) ON COMMIT DROP"
        ),
        "CREATE TABLE"
    );
    // d は消えているので、同名を作り直せる。
    assert_eq!(
        extended_command(
            &mut stream,
            "s4",
            "CREATE TEMPORARY TABLE d (a TEXT) ON COMMIT DROP"
        ),
        "CREATE TABLE"
    );
    assert_eq!(
        extended_command(&mut stream, "s5", "DROP TABLE t"),
        "DROP TABLE"
    );
    assert_eq!(
        extended_command(&mut stream, "s6", "CREATE TEMP TABLE t (a TEXT)"),
        "CREATE TABLE"
    );
}

/// 別の接続からは一時表が見えない（同名を作れ、永続側の DROP は権限エラー）。
/// 接続を切って張り直した接続にも引き継がれない。
#[test]
fn temp_tables_are_private_to_a_connection() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);

    let mut a = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(&mut a, "CREATE TEMP TABLE only_a (x TEXT)");
    assert_eq!(read_command_complete(&mut a), "CREATE TABLE");
    read_ready_for_query(&mut a);

    let mut b = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(&mut b, "DROP TABLE only_a");
    expect_error_response_with_sqlstate(&mut b, "42501");
    read_ready_for_query(&mut b);
    send_simple_query(&mut b, "CREATE TEMP TABLE only_a (x TEXT)");
    assert_eq!(read_command_complete(&mut b), "CREATE TABLE");
    read_ready_for_query(&mut b);

    drop(a);
    let mut c = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(&mut c, "DROP TABLE only_a");
    expect_error_response_with_sqlstate(&mut c, "42501");
    read_ready_for_query(&mut c);
}
