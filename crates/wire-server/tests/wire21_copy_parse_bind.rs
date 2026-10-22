//! 拡張クエリプロトコルの Parse／Bind／Describe で `COPY` 文を受理する結合テスト
//! （Issue #1759・WIRE-21 (1)・ERR-6 のポインタ。親 Issue #1758）。
//!
//! 対象は Parse／Bind／Describe まで。Execute（CopyIn／CopyOut の開始と完了）は
//! `wire21_copy_execute.rs`（Issue #1760）、CopyFail 等の後の Sync 回復の詳細は Issue #1761
//! の担当。
//! エラーは接続を閉じず、Sync まで後続を読み捨てて `ReadyForQuery` へ同期回復する
//! （`crate::extended_query` モジュールドキュメント）。

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

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire21-copy-parse-bind");
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

fn connect(core: Arc<EngineCore>) -> TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    drop(users_path);
    stream
}

fn describe_body(kind: u8, name: &str) -> Vec<u8> {
    let mut body = vec![kind];
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body
}

/// テキスト形式のパラメータ `params` と結果 format code `result_codes` を持つ Bind 本体。
fn bind_body_with(portal: &str, statement: &str, params: &[&str], result_codes: &[i16]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&(params.len() as i16).to_be_bytes());
    for p in params {
        body.extend_from_slice(&(p.len() as i32).to_be_bytes());
        body.extend_from_slice(p.as_bytes());
    }
    body.extend_from_slice(&(result_codes.len() as i16).to_be_bytes());
    for c in result_codes {
        body.extend_from_slice(&c.to_be_bytes());
    }
    body
}

fn sync(stream: &mut TcpStream) {
    send_length_prefixed_message(stream, b'S', b"");
}

/// 'Z' まで読み、ReadyForQuery のステータスバイトを返す。
fn read_until_ready(stream: &mut TcpStream) -> u8 {
    loop {
        let (kind, body) = read_message(stream);
        if kind == b'Z' {
            return body[0];
        }
    }
}

/// 簡易クエリを送り、ReadyForQuery まで読み捨てて状態バイトを返す。
fn simple(stream: &mut TcpStream, sql: &str) -> u8 {
    send_simple_query(stream, sql);
    read_until_ready(stream)
}

fn parse(stream: &mut TcpStream, name: &str, sql: &str, declared: i16) {
    send_length_prefixed_message(stream, b'P', &parse_body(name, sql, declared));
}

fn expect_parse_complete(stream: &mut TcpStream) {
    let (kind, body) = read_message(stream);
    assert_eq!(
        kind,
        b'1',
        "expected ParseComplete, got {}: {:?}",
        kind as char,
        String::from_utf8_lossy(&body)
    );
    assert!(body.is_empty());
}

/// ErrorResponse（指定 SQLSTATE）を読み、Sync で ReadyForQuery へ回復し、簡易クエリが通る
/// ことまで確認する。ErrorResponse の本体を返す。
fn expect_error_and_recover(stream: &mut TcpStream, sqlstate: &str) -> Vec<u8> {
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'E', "expected ErrorResponse");
    let text = String::from_utf8_lossy(&body).into_owned();
    assert!(text.contains(sqlstate), "expected {sqlstate} in {text:?}");
    sync(stream);
    assert_eq!(read_until_ready(stream), b'I', "recover to idle");
    assert_eq!(simple(stream, "SELECT id FROM documents LIMIT 1"), b'I');
    body
}

fn expect_no_data_describe(stream: &mut TcpStream) {
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b't', "expected ParameterDescription");
    assert_eq!(body, 0i16.to_be_bytes());
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'n', "expected NoData");
    assert!(body.is_empty());
}

const COPY_FROM: &str = "COPY documents (id, embedding, body) FROM STDIN";
const COPY_TO: &str = "COPY (SELECT id, body FROM documents LIMIT 100) TO STDOUT";

/// Parse（COPY FROM／TO）は ParseComplete、Describe(S) は ParameterDescription(0)＋NoData。
#[test]
fn parse_and_describe_statement_of_copy_return_no_data() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    for (name, sql) in [("c_from", COPY_FROM), ("c_to", COPY_TO)] {
        parse(&mut stream, name, sql, 0);
        expect_parse_complete(&mut stream);
        send_length_prefixed_message(&mut stream, b'D', &describe_body(b'S', name));
        expect_no_data_describe(&mut stream);
    }
    sync(&mut stream);
    assert_eq!(read_until_ready(&mut stream), b'I');
    stream.shutdown(Shutdown::Write).ok();
}

/// Bind（パラメータ 0・結果 format code 0／1 個）は BindComplete、Describe(P) は NoData。
#[test]
fn bind_and_describe_portal_of_copy_return_no_data() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    for (i, sql) in [COPY_FROM, COPY_TO].into_iter().enumerate() {
        let stmt = format!("c{i}");
        parse(&mut stream, &stmt, sql, 0);
        expect_parse_complete(&mut stream);
        for (j, codes) in [&[][..], &[1][..]].into_iter().enumerate() {
            let portal = format!("p{i}{j}");
            send_length_prefixed_message(
                &mut stream,
                b'B',
                &bind_body_with(&portal, &stmt, &[], codes),
            );
            let (kind, _) = read_message(&mut stream);
            assert_eq!(kind, b'2', "expected BindComplete");
            send_length_prefixed_message(&mut stream, b'D', &describe_body(b'P', &portal));
            let (kind, body) = read_message(&mut stream);
            assert_eq!(kind, b'n', "expected NoData for portal");
            assert!(body.is_empty());
        }
    }
    sync(&mut stream);
    assert_eq!(read_until_ready(&mut stream), b'I');
    stream.shutdown(Shutdown::Write).ok();
}

/// `$n` を含む COPY は `42P02` で拒否し、Sync で同期回復して接続が維持される。
#[test]
fn copy_with_placeholder_is_rejected_with_42p02_and_recovers() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    parse(
        &mut stream,
        "c",
        "COPY (SELECT id FROM documents WHERE id = $1 LIMIT 100) TO STDOUT",
        0,
    );
    expect_error_and_recover(&mut stream, "42P02");
    // 型宣言つきでも、過剰宣言（08P01）より先に 42P02 を返す。
    let mut body = parse_body(
        "c2",
        "COPY (SELECT id FROM documents WHERE id = $1 LIMIT 100) TO STDOUT",
        1,
    );
    body.extend_from_slice(&23i32.to_be_bytes());
    send_length_prefixed_message(&mut stream, b'P', &body);
    expect_error_and_recover(&mut stream, "42P02");
    stream.shutdown(Shutdown::Write).ok();
}

/// 文字列リテラル内の `$1` はパラメータではなく、COPY は受理される。
#[test]
fn placeholder_inside_string_literal_is_not_a_parameter() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    parse(
        &mut stream,
        "c",
        "COPY (SELECT id FROM documents WHERE body = 'costs $1' LIMIT 100) TO STDOUT",
        0,
    );
    expect_parse_complete(&mut stream);
    sync(&mut stream);
    assert_eq!(read_until_ready(&mut stream), b'I');
    stream.shutdown(Shutdown::Write).ok();
}

/// `$n` の無い COPY への型宣言（過剰宣言）は既存どおり `08P01`。
#[test]
fn declared_param_types_on_copy_are_rejected_as_excess_with_08p01() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    let mut body = parse_body("c", COPY_FROM, 1);
    body.extend_from_slice(&23i32.to_be_bytes());
    send_length_prefixed_message(&mut stream, b'P', &body);
    expect_error_and_recover(&mut stream, "08P01");
    stream.shutdown(Shutdown::Write).ok();
}

/// COPY 文へのパラメータ 1 個の Bind は個数不一致で `08P01`。
#[test]
fn bind_with_a_parameter_to_copy_is_rejected_with_08p01() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    parse(&mut stream, "c", COPY_TO, 0);
    expect_parse_complete(&mut stream);
    send_length_prefixed_message(&mut stream, b'B', &bind_body_with("p", "c", &["1"], &[]));
    expect_error_and_recover(&mut stream, "08P01");
    stream.shutdown(Shutdown::Write).ok();
}

/// 未定義表・許可外オプションの COPY は、簡易クエリと同じ SQLSTATE で Parse が拒否する。
#[test]
fn invalid_copy_parse_matches_simple_query_sqlstate() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    for (sql, state) in [
        ("COPY nosuch (id, embedding) FROM STDIN", "42P01"),
        (
            "COPY documents (id, embedding, body) FROM STDIN WITH (FORMAT binary, DELIMITER ',')",
            "0A000",
        ),
    ] {
        send_simple_query(&mut stream, sql);
        let (kind, body) = read_message(&mut stream);
        assert_eq!(kind, b'E');
        assert!(
            String::from_utf8_lossy(&body).contains(state),
            "simple: {state}"
        );
        assert_eq!(read_until_ready(&mut stream), b'I');

        parse(&mut stream, "c", sql, 0);
        expect_error_and_recover(&mut stream, state);
    }
    stream.shutdown(Shutdown::Write).ok();
}

/// Failed なトランザクション中の COPY の Parse は既存どおり `25P02`。
#[test]
fn copy_parse_in_failed_transaction_is_rejected_with_25p02() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    assert_eq!(simple(&mut stream, "BEGIN"), b'T');
    send_simple_query(&mut stream, "SELECT id FROM nosuch_table");
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'E');
    assert_eq!(read_until_ready(&mut stream), b'E');

    parse(&mut stream, "c", COPY_FROM, 0);
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'E');
    assert!(String::from_utf8_lossy(&body).contains("25P02"));
    sync(&mut stream);
    assert_eq!(read_until_ready(&mut stream), b'E');
    assert_eq!(simple(&mut stream, "ROLLBACK"), b'I');
    stream.shutdown(Shutdown::Write).ok();
}

/// 一時表を対象にした COPY の Parse は `0A000`（fail-closed。#1760 以降で対応を判断する）。
#[test]
fn copy_on_temp_table_parse_is_rejected_with_0a000() {
    let (core, _g) = new_core();
    let mut stream = connect(core);
    assert_eq!(
        simple(
            &mut stream,
            "CREATE TEMP TABLE t (id BIGINT PRIMARY KEY, a TEXT)"
        ),
        b'I'
    );
    parse(&mut stream, "c", "COPY t (id, a) FROM STDIN", 0);
    expect_error_and_recover(&mut stream, "0A000");
    stream.shutdown(Shutdown::Write).ok();
}

/// 自テナントに無い表への COPY の Parse 応答は、テナントによらず同一（存在オラクルを作らない）。
#[test]
fn copy_parse_error_is_identical_across_tenants() {
    let (core, _g) = new_core();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "correct-horse"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut bodies = Vec::new();
    for user in ["alice", "bob"] {
        let mut stream = authenticate_to_ready_for_query(addr, user, "correct-horse");
        parse(
            &mut stream,
            "c",
            "COPY other_tenant_only (id, embedding) FROM STDIN",
            0,
        );
        bodies.push(expect_error_and_recover(&mut stream, "42P01"));
        stream.shutdown(Shutdown::Write).ok();
    }
    assert_eq!(bodies[0], bodies[1]);
}
