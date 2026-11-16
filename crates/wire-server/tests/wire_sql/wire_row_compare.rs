//! WHERE の行値比較 `(a, b) <op> (c, d)`（対象ビヘイビア: SQL-9。Issue #3256）の簡易クエリ
//! プロトコル経由（生バイトクライアント）検証。
//!
//! 意味論（辞書式・三値論理・テナント境界）は `crates/engine/tests/sql_named/sql_where_row_compare.rs` が
//! 確定オラクルとして検証済みのため、本ファイルは psql と同じ末尾 `;` 付きの単独 simple query が
//! wire フレーミング越しに行を返すこと、拒否が SQLSTATE として公告され接続が継続することの確認に徹する。
//!
//! 拡張クエリ（Parse／Describe／Bind／Execute／Sync。Issue #3257・WIRE-12）の成分 `$n` も同様に、
//! ParameterDescription の OID（相手列の型）・値付き Bind・名前付き文の再利用・エラー後の接続継続を固定する。

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use crate::common::*;
use crate::temp_db;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-row-compare-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("a", ColumnType::Integer, true),
                ColumnDef::new("b", ColumnType::Integer, true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn insert(stream: &mut std::net::TcpStream, id: u32, a: &str, b: &str) {
    send_simple_query(
        stream,
        &format!(
            "INSERT INTO docs (id, embedding, a, b) VALUES ({id}, '[0.1,0.2]', {a}, {b}) \
             USING OPERATION_ID 'op-{id}'"
        ),
    );
    assert_eq!(read_command_complete(stream), "INSERT 0 1");
    read_ready_for_query(stream);
}

fn expect_ids(stream: &mut std::net::TcpStream, sql: &str, want: &[&str]) {
    send_simple_query(stream, sql);
    let _ = read_row_description(stream);
    for id in want {
        let row = read_data_row(stream);
        assert_eq!(row.first().and_then(|v| v.as_deref()), Some(*id), "{sql}");
    }
    assert_eq!(
        read_command_complete(stream),
        format!("SELECT {}", want.len())
    );
    read_ready_for_query(stream);
}

/// 末尾 `;` 付きの単独送信で、行値比較（`<`・`=`・`<>`）が行を返す。NULL 成分の行は UNKNOWN で出ない。
#[test]
fn wire_row_compare_with_trailing_semicolon() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-alice", "pw-alice")]);
    let addrs = [spawn_server_with_engine(&users_path, core)];
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");
    insert(&mut stream, 1, "1", "1");
    insert(&mut stream, 2, "3", "5");
    insert(&mut stream, 3, "3", "NULL");

    expect_ids(
        &mut stream,
        "SELECT id FROM docs WHERE (a, b) < (3, 5) ORDER BY id LIMIT 10;",
        &["1"],
    );
    expect_ids(
        &mut stream,
        "SELECT id FROM docs WHERE (a, b) = (3, 5) ORDER BY id LIMIT 10;",
        &["2"],
    );
    expect_ids(
        &mut stream,
        "SELECT id FROM docs WHERE (a, b) <> (3, 5) ORDER BY id LIMIT 10;",
        &["1"],
    );
}

/// 成分数の不一致は `42601` の ErrorResponse の後に ReadyForQuery が返り、接続は継続する。
#[test]
fn wire_row_compare_malformed_carries_sqlstate() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-alice", "pw-alice")]);
    let addrs = [spawn_server_with_engine(&users_path, core)];
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs WHERE (a, b) < (1, 2, 3) LIMIT 5;",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}

// --- 拡張クエリ（Issue #3257） ---------------------------------------------------------

fn send(stream: &mut std::net::TcpStream, kind: u8, body: &[u8]) {
    send_length_prefixed_message(stream, kind, body);
}

fn expect_kind(stream: &mut std::net::TcpStream, kind: u8) -> Vec<u8> {
    let (k, body) = read_message(stream);
    assert_eq!(
        k as char,
        kind as char,
        "unexpected message: {}",
        String::from_utf8_lossy(&body)
    );
    body
}

fn parse_msg(name: &str, sql: &str) -> Vec<u8> {
    parse_body(name, sql, 0)
}

fn bind_msg(portal: &str, stmt: &str, values: &[Option<&[u8]>]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(stmt.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes()); // パラメータ書式: すべてテキスト
    body.extend_from_slice(&(values.len() as i16).to_be_bytes());
    for v in values {
        match v {
            None => body.extend_from_slice(&(-1i32).to_be_bytes()),
            Some(bytes) => {
                body.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                body.extend_from_slice(bytes);
            }
        }
    }
    body.extend_from_slice(&0i16.to_be_bytes()); // 結果書式: テキスト
    body
}

fn describe_msg(kind: u8, name: &str) -> Vec<u8> {
    let mut body = vec![kind];
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body
}

fn param_description_oids(body: &[u8]) -> Vec<i32> {
    let n = body
        .get(..2)
        .map(|b| i16::from_be_bytes([b[0], b[1]]) as usize)
        .unwrap_or(0);
    (0..n)
        .filter_map(|i| {
            let at = 2 + 4 * i;
            let b = body.get(at..at + 4)?;
            Some(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        })
        .collect()
}

/// Execute 済みの DataRow の第 1 列（id）を集めて、CommandComplete のタグと併せて返す。
fn read_until_z(stream: &mut std::net::TcpStream) -> (Vec<String>, String) {
    let mut ids = Vec::new();
    let mut tag = String::new();
    loop {
        let (k, body) = read_message(stream);
        match k {
            b'D' => {
                // DataRow: i16 列数、続いて列ごとに i32 長さ + 値。
                let len = body
                    .get(2..6)
                    .map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
                    .unwrap_or(0);
                let end = 6usize.saturating_add(usize::try_from(len).unwrap_or(0));
                ids.push(String::from_utf8_lossy(body.get(6..end).unwrap_or(&[])).to_string());
            }
            b'C' => {
                tag = String::from_utf8_lossy(&body)
                    .trim_end_matches('\0')
                    .to_string()
            }
            b'Z' => break,
            b'E' => panic!("unexpected error: {}", String::from_utf8_lossy(&body)),
            _ => {}
        }
    }
    (ids, tag)
}

/// 末尾 `;` 付きの拡張クエリ: Describe(S) の ParameterDescription が相手列の型（int4）を公告し、
/// 値付き Bind → Execute が期待行を返す。同じ名前付き文を別の値で再利用できる。
#[test]
fn wire_row_compare_extended_query_binds_component_params() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-alice", "pw-alice")]);
    let addrs = [spawn_server_with_engine(&users_path, core)];
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");
    insert(&mut stream, 1, "1", "1");
    insert(&mut stream, 2, "3", "5");
    insert(&mut stream, 3, "3", "NULL");

    let sql = "SELECT id FROM docs WHERE (a, b) < ($1, $2) ORDER BY id LIMIT 10;";
    send(&mut stream, b'P', &parse_msg("rc", sql));
    expect_kind(&mut stream, b'1');
    send(&mut stream, b'D', &describe_msg(b'S', "rc"));
    let body = expect_kind(&mut stream, b't');
    assert_eq!(param_description_oids(&body), vec![23, 23]);
    let (k, _) = read_message(&mut stream);
    assert!(k == b'T' || k == b'n');
    send(&mut stream, b'S', b"");
    expect_kind(&mut stream, b'Z');

    for (vals, want, tag) in [
        ([b"3".as_slice(), b"5".as_slice()], vec!["1"], "SELECT 1"),
        (
            [b"3".as_slice(), b"6".as_slice()],
            vec!["1", "2"],
            "SELECT 2",
        ),
        ([b"0".as_slice(), b"0".as_slice()], vec![], "SELECT 0"),
    ] {
        send(
            &mut stream,
            b'B',
            &bind_msg("", "rc", &[Some(vals[0]), Some(vals[1])]),
        );
        send(&mut stream, b'E', &execute_body("", 0));
        send(&mut stream, b'S', b"");
        expect_kind(&mut stream, b'2');
        let (ids, got_tag) = read_until_z(&mut stream);
        assert_eq!(ids, want);
        assert_eq!(got_tag, tag);
    }
}

/// 未対応の形（相手がリテラル）は Parse で `42601`、NULL のバインドはリテラル `NULL` 成分と同じ SQLSTATE。
/// どちらも ErrorResponse の後に Sync で ReadyForQuery が返り、接続が継続する。
#[test]
fn wire_row_compare_extended_query_errors_keep_the_connection() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-alice", "pw-alice")]);
    let addrs = [spawn_server_with_engine(&users_path, core)];
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");
    insert(&mut stream, 1, "1", "1");

    // Parse で拒否（`$2` の相手が列でなくリテラル）。
    send(
        &mut stream,
        b'P',
        &parse_msg("", "SELECT id FROM docs WHERE (a, 7) < ($1, $2) LIMIT 5"),
    );
    send(&mut stream, b'S', b"");
    let body = expect_kind(&mut stream, b'E');
    assert!(String::from_utf8_lossy(&body).contains("42601"));
    expect_kind(&mut stream, b'Z');

    // NULL のバインド（リテラル NULL 成分と同じ SQLSTATE）。
    send(
        &mut stream,
        b'P',
        &parse_msg("", "SELECT id FROM docs WHERE (a, b) < ($1, $2) LIMIT 5"),
    );
    send(&mut stream, b'B', &bind_msg("", "", &[None, Some(b"5")]));
    send(&mut stream, b'E', &execute_body("", 0));
    send(&mut stream, b'S', b"");
    expect_kind(&mut stream, b'1');
    let body = expect_kind(&mut stream, b'E');
    // リテラル `NULL` 成分（裸の識別子として解決され 42703）と同じ SQLSTATE。
    assert!(String::from_utf8_lossy(&body).contains("42703"));
    expect_kind(&mut stream, b'Z');

    // 接続は継続し、簡易クエリが通る。
    expect_ids(
        &mut stream,
        "SELECT id FROM docs WHERE (a, b) = (1, 1) ORDER BY id LIMIT 10;",
        &["1"],
    );
}
