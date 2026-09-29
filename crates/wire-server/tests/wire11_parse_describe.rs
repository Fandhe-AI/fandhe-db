//! 拡張クエリプロトコルの Parse（'P'）・Describe（'D' 種別 S・P）の結合テスト
//! （Issue #933・#934・TASK-71・WIRE-11。`crate::extended_query` が受理する経路）。
//!
//! `engine: None`（`handle_connection_bounded` 経由の後方互換パス）での
//! Parse・Describe が従来どおり `0A000` + 切断のまま（`crate::extended_query`
//! に到達しない）ことは `tests/wire_extended_query.rs` が担う。本ファイルは
//! engine 接続済みの経路（`accept_loop_with_engine`）を対象にする。
//!
//! `Describe` が返す `RowDescription` が、同一 SQL を簡易クエリ（'Q'）で実行
//! した場合の `RowDescription` と列名が一致すること（受け入れ条件 3）と、
//! Parse／Describe が実行を伴わない（受け入れ条件 1・簡易クエリ経路の挙動が
//! 不変であること。受け入れ条件 4）を検証する。
//!
//! Issue #934 で Bind／Execute／Sync／Close／Flush が受理されるようになった
//! ことに伴い、Parse／Describe のエラーはもはや接続を閉じず、ErrorResponse
//! 送出後は Sync（'S'）まで後続メッセージを読み捨てて `ReadyForQuery` を
//! 返す「同期回復」へ収束する（`crate::extended_query` モジュールドキュメント
//! 「エラー後の同期回復」節）。本ファイルの `*_and_recovers` 系テストは
//! [`assert_error_then_recovers`] でこの契約を固定する。Bind／Execute／
//! Sync／Close／Flush 自体の受理・実行契約は `tests/wire11_bind_execute_sync.rs`
//! が担う。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::Read;
use std::net::Shutdown;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core_with_documents_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire11-parse-describe");
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

fn parse_body(name: &str, query: &str, num_param_types: i16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body.extend_from_slice(query.as_bytes());
    body.push(0);
    body.extend_from_slice(&num_param_types.to_be_bytes());
    body
}

fn describe_body(kind: u8, name: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(kind);
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body
}

fn read_message(stream: &mut std::net::TcpStream) -> (u8, Vec<u8>) {
    let mut type_byte = [0u8; 1];
    stream.read_exact(&mut type_byte).expect("read type byte");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let body_len = len.checked_sub(4).expect("length must be >= 4");
    let mut body = vec![0u8; body_len];
    stream.read_exact(&mut body).expect("read body");
    (type_byte[0], body)
}

fn assert_error_response(stream: &mut std::net::TcpStream, expected_sqlstate: &str) {
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'E', "expected ErrorResponse");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains(expected_sqlstate),
        "expected sqlstate {expected_sqlstate} in {text:?}"
    );
}

/// Issue #934 以降、本ファイルの Parse／Describe エラーは同期回復
/// （[`assert_error_then_recovers`]）に収束したため直接は使わないが、フレーム
/// 違反（malformed frame）系の回帰確認に備えて残す。
#[allow(dead_code)]
fn assert_connection_closed(stream: &mut std::net::TcpStream) {
    let mut buf = [0u8; 1];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "connection must be closed");
}

/// ErrorResponse を読んだうえで、Sync（'S'）を送って `ReadyForQuery`（'Z'）が
/// 返ること（＝接続が維持されたまま同期回復すること）を確認する（Issue #934。
/// モジュールドキュメント「エラー後の同期回復」節）。回復後に簡易クエリが
/// 通ることまで確認し、接続が実運用可能な状態のまま戻っていることを固定する。
fn assert_error_then_recovers(stream: &mut std::net::TcpStream, expected_sqlstate: &str) {
    assert_error_response(stream, expected_sqlstate);
    send_length_prefixed_message(stream, b'S', b"");
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery after Sync");
    assert_eq!(body, [b'I'], "ReadyForQuery status byte");

    send_simple_query(stream, "SELECT id FROM documents LIMIT 1");
    loop {
        let (kind, _) = read_message(stream);
        if kind == b'Z' {
            break;
        }
    }
}

/// 許可リスト検証を通過する SQL への Parse は `'1'`（ParseComplete）を返す。
/// 無名ステートメントは複数回 Parse しても件数を増やさない（黙った置換）。
#[test]
fn parse_of_valid_sql_returns_parse_complete_and_anonymous_reparse_does_not_error() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    for _ in 0..2 {
        let body = parse_body("", "SELECT id FROM documents LIMIT 1", 0);
        send_length_prefixed_message(&mut stream, b'P', &body);
        let (kind, body) = read_message(&mut stream);
        assert_eq!(kind, b'1', "expected ParseComplete");
        assert!(body.is_empty());
    }

    // 名前付きステートメントも受理できる。
    let body = parse_body("stmt1", "SELECT id FROM documents LIMIT 1", 0);
    send_length_prefixed_message(&mut stream, b'P', &body);
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1');

    stream.shutdown(Shutdown::Write).ok();
}

/// 空文字列の Parse は `'1'` を返し、その Describe は
/// `ParameterDescription(0)` + `NoData` を返す。
#[test]
fn parse_of_empty_query_then_describe_returns_no_data() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_length_prefixed_message(&mut stream, b'P', &parse_body("", "", 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1');

    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'S', ""));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b't', "expected ParameterDescription");
    assert_eq!(body, 0i16.to_be_bytes());
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'n', "expected NoData");
    assert!(body.is_empty());

    stream.shutdown(Shutdown::Write).ok();
}

/// `SELECT` の Describe が返す `RowDescription` の列名は、同一 SQL を簡易
/// クエリで実行した場合の `RowDescription` と一致する（受け入れ条件 3）。
#[test]
fn describe_row_description_matches_simple_query_row_description() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let sql = "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5";
    send_length_prefixed_message(&mut stream, b'P', &parse_body("stmt1", sql, 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1');

    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'S', "stmt1"));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b't');
    assert_eq!(body, 0i16.to_be_bytes());
    let described_columns = read_row_description(&mut stream);

    send_simple_query(&mut stream, sql);
    let executed_columns = read_row_description(&mut stream);
    // 簡易クエリの残り（DataRow・CommandComplete・ReadyForQuery）を読み捨てる。
    loop {
        let (kind, _) = read_message(&mut stream);
        if kind == b'Z' {
            break;
        }
    }

    assert_eq!(described_columns, executed_columns);
    stream.shutdown(Shutdown::Write).ok();
}

/// Parse／Describe を挟んでも簡易クエリ（'Q'）の応答は不変（受け入れ条件 4）。
/// `Q` → Parse → Describe → `Q` と交互に送っても、2 回の `Q` の応答が同一の
/// バイト列であることを確認する。
#[test]
fn simple_query_response_is_unchanged_when_interleaved_with_parse_and_describe() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let sql = "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5";

    let read_full_query_response = |stream: &mut std::net::TcpStream| -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut type_byte = [0u8; 1];
            stream.read_exact(&mut type_byte).expect("read type byte");
            let mut len_buf = [0u8; 4];
            stream.read_exact(&mut len_buf).expect("read length");
            let len = i32::from_be_bytes(len_buf) as usize;
            let mut body = vec![0u8; len - 4];
            stream.read_exact(&mut body).expect("read body");
            out.push(type_byte[0]);
            out.extend_from_slice(&len_buf);
            out.extend_from_slice(&body);
            if type_byte[0] == b'Z' {
                break;
            }
        }
        out
    };

    send_simple_query(&mut stream, sql);
    let first = read_full_query_response(&mut stream);

    send_length_prefixed_message(&mut stream, b'P', &parse_body("stmt-interleave", sql, 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1');
    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'S', "stmt-interleave"));
    let (kind, _) = read_message(&mut stream); // ParameterDescription
    assert_eq!(kind, b't');
    let (kind, _) = read_message(&mut stream); // RowDescription
    assert_eq!(kind, b'T');

    send_simple_query(&mut stream, sql);
    let second = read_full_query_response(&mut stream);

    assert_eq!(
        first, second,
        "simple query response must be byte-identical"
    );
    stream.shutdown(Shutdown::Write).ok();
}

/// 許可リスト外の SQL（受理位置外の `$1`。`LIMIT $1`。WIRE-12）は Parse 時点で `42601` を返し、Sync で
/// 同期回復する（Issue #934。モジュールドキュメント参照）。
#[test]
fn parse_of_disallowed_sql_returns_42601_and_recovers() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let body = parse_body("", "SELECT id FROM documents LIMIT $1", 0);
    send_length_prefixed_message(&mut stream, b'P', &body);
    assert_error_then_recovers(&mut stream, "42601");
}

/// 未存在テーブルへの Parse は `42P01` を返し、Sync で同期回復する。
#[test]
fn parse_of_undefined_table_returns_42p01_and_recovers() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let body = parse_body("", "SELECT id FROM missing_table LIMIT 1", 0);
    send_length_prefixed_message(&mut stream, b'P', &body);
    assert_error_then_recovers(&mut stream, "42P01");
}

/// パラメータ型宣言（`num_param_types > 0`）は受理され、宣言 OID は
/// Describe(S) の `ParameterDescription` へそのまま echo される（WIRE-12）。
/// プレースホルダ数を超える宣言は `08P01` で拒否され、Sync で同期回復する。
#[test]
fn parse_with_declared_param_types_is_accepted_and_echoed() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let mut body = parse_body("s1", "SELECT id FROM documents WHERE body = $1 LIMIT 10", 1);
    body.extend_from_slice(&23i32.to_be_bytes()); // int4 OID
    send_length_prefixed_message(&mut stream, b'P', &body);
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1', "expected ParseComplete");

    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'S', "s1"));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b't', "expected ParameterDescription");
    assert_eq!(body, [0, 1, 0, 0, 0, 23]);
    let (kind, _) = read_message(&mut stream); // RowDescription
    assert_eq!(kind, b'T');

    // プレースホルダを持たない文への型宣言は過剰宣言（fail-closed）。
    let mut body = parse_body("", "SELECT id FROM documents LIMIT 1", 1);
    body.extend_from_slice(&23i32.to_be_bytes());
    send_length_prefixed_message(&mut stream, b'P', &body);
    assert_error_then_recovers(&mut stream, "08P01");
}

/// 空文の Parse でも、型宣言 1 個以上は過剰宣言として `08P01`（fail-closed）。
/// 宣言 0 個は従来どおり成功する。
#[test]
fn parse_of_empty_query_with_declared_param_types_returns_08p01_and_recovers() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let mut body = parse_body("", "", 1);
    body.extend_from_slice(&23i32.to_be_bytes());
    send_length_prefixed_message(&mut stream, b'P', &body);
    assert_error_then_recovers(&mut stream, "08P01");

    send_length_prefixed_message(&mut stream, b'P', &parse_body("", "", 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(
        kind, b'1',
        "expected ParseComplete for empty query without types"
    );
}

/// 名前付きステートメントの重複 Parse は `08P01` で拒否され、Sync で同期回復する。
#[test]
fn duplicate_named_statement_returns_08p01_and_recovers() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let sql = "SELECT id FROM documents LIMIT 1";
    send_length_prefixed_message(&mut stream, b'P', &parse_body("dup", sql, 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1');

    send_length_prefixed_message(&mut stream, b'P', &parse_body("dup", sql, 0));
    assert_error_then_recovers(&mut stream, "08P01");
}

/// 未定義のステートメント名への Describe は `08P01` で拒否される。
#[test]
fn describe_of_unknown_statement_returns_08p01_and_recovers() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'S', "never-parsed"));
    assert_error_then_recovers(&mut stream, "08P01");
}

/// Describe の対象が未定義の portal（種別 'P'）の場合は `08P01` で拒否され、
/// Sync で同期回復する（Issue #934 で portal 対象の Describe 自体は受理される
/// ようになった。portal の構築〔Bind〕・実行契約は `tests/
/// wire11_bind_execute_sync.rs` の担当）。
#[test]
fn describe_of_unknown_portal_returns_08p01_and_recovers() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'P', ""));
    assert_error_then_recovers(&mut stream, "08P01");
}

/// 名前付きステートメントを上限（`MAX_PREPARED_STATEMENTS_PER_SESSION`＝64）
/// まで Parse できるが、65 件目は `54000` で拒否され、Sync で同期回復する。
#[test]
fn exceeding_named_statement_limit_returns_54000_and_recovers() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let sql = "SELECT id FROM documents LIMIT 1";
    for i in 0..64 {
        let name = format!("s{i}");
        send_length_prefixed_message(&mut stream, b'P', &parse_body(&name, sql, 0));
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b'1', "statement {i} should be accepted");
    }

    send_length_prefixed_message(&mut stream, b'P', &parse_body("s64", sql, 0));
    assert_error_then_recovers(&mut stream, "54000");
}

/// ステートメント名がバイト長上限（`MAX_STATEMENT_NAME_LEN`＝63）を超えると
/// `54000` で拒否され、Sync で同期回復する。
#[test]
fn statement_name_too_long_returns_54000_and_recovers() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let long_name = "a".repeat(64);
    let body = parse_body(&long_name, "SELECT id FROM documents LIMIT 1", 0);
    send_length_prefixed_message(&mut stream, b'P', &body);
    assert_error_then_recovers(&mut stream, "54000");
}

/// Parse の body 構造不正（NUL 終端欠落）は `08P01` で拒否され、Sync で
/// 同期回復する（body 自体はメッセージ境界確定後に判明する不正のため回復可能。
/// モジュールドキュメント「エラー後の同期回復」節参照）。
#[test]
fn malformed_parse_body_returns_08p01_and_recovers() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    // 終端 NUL の無い名前だけを送る（クエリ文字列・パラメータ件数が続かない）。
    send_length_prefixed_message(&mut stream, b'P', b"no-nul-terminator");
    assert_error_then_recovers(&mut stream, "08P01");
}

/// テナント間で Describe の応答（列メタデータ）がバイト一致すること（RLS-9・
/// テナント境界: Describe は行データ・行の有無を一切反映しない）。
#[test]
fn describe_response_is_identical_across_tenants() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "correct-horse"),
    ]);

    let sql = "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5";

    let describe_once = |user: &str| -> Vec<u8> {
        let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
        let mut stream = authenticate_to_ready_for_query(addr, user, "correct-horse");
        send_length_prefixed_message(&mut stream, b'P', &parse_body("stmt1", sql, 0));
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b'1');
        send_length_prefixed_message(&mut stream, b'D', &describe_body(b'S', "stmt1"));
        let (kind, param_body) = read_message(&mut stream);
        assert_eq!(kind, b't');
        let (kind2, row_body) = read_message(&mut stream);
        assert_eq!(kind2, b'T');
        let mut out = param_body;
        out.extend_from_slice(&row_body);
        out
    };

    let alice_bytes = describe_once("alice");
    let bob_bytes = describe_once("bob");
    assert_eq!(alice_bytes, bob_bytes);
}
