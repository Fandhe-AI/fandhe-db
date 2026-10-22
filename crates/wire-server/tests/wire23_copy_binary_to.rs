//! `COPY (...) TO STDOUT WITH (FORMAT binary)`（Issue #1770・WIRE-23 のポインタ）の
//! 簡易／拡張クエリ経由（生バイトクライアント）検証（層 A）。
//!
//! 範囲: CopyOutResponse の format、ヘッダ／tuple／トレーラのバイト列、DataRow（result format 1）
//! とのフィールド一致（往復の土台）、非対応型の送出前 `0A000`、テナント境界、旧構文、複数文、
//! 明示トランザクション。COPY FROM binary は Issue #1769 の担当で本ファイルの対象外。
//! CopyData の分割はクライアントにとって意味を持たないため、payload を連結してから解析する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::Read;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

type Stream = std::net::TcpStream;
type Tuple = Vec<Option<Vec<u8>>>;

const SIGNATURE: &[u8; 11] = b"PGCOPY\n\xff\r\n\0";
/// `id = 1` の numeric 表現（ndigits 1・weight 0・sign 0・dscale 0・digit 1）。
const NUMERIC_ONE: [u8; 10] = [0, 1, 0, 0, 0, 0, 0, 0, 0, 1];

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire23-copy-binary-to");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("n", ColumnType::Integer, true),
                ColumnDef::new("flag", ColumnType::Boolean, true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
    (Arc::new(core), guard)
}

fn connect(core: Arc<EngineCore>, user: &str, pw: &str) -> Stream {
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, user, pw)
}

fn alice(core: &Arc<EngineCore>) -> Stream {
    connect(Arc::clone(core), "alice", "pw-alice")
}

fn exec(stream: &mut Stream, sql: &str) {
    send_simple_query(stream, sql);
    let _ = read_command_complete(stream);
    read_ready_for_query(stream);
}

fn insert(stream: &mut Stream, id: u32, lang: &str, n: &str, flag: &str, op: &str) {
    exec(
        stream,
        &format!(
            "INSERT INTO docs (id, embedding, lang, n, flag) VALUES ({id}, '[1.0,0.0]', '{lang}', {n}, {flag}) USING OPERATION_ID '{op}'"
        ),
    );
}

/// `CopyOutResponse` を読み、`(overall format, 列ごとの format)` を返す。
fn read_copy_out_formats(stream: &mut Stream) -> (u8, Vec<i16>) {
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'H', "expected CopyOutResponse");
    let n = i16::from_be_bytes([body[1], body[2]]) as usize;
    let cols = (0..n)
        .map(|i| i16::from_be_bytes([body[3 + 2 * i], body[4 + 2 * i]]))
        .collect();
    (body[0], cols)
}

/// CopyDone までの CopyData payload を連結して返す（分割に依存しない）。
fn read_copy_stream(stream: &mut Stream) -> Vec<u8> {
    let mut all = Vec::new();
    loop {
        let (kind, body) = read_message(stream);
        match kind {
            b'd' => all.extend_from_slice(&body),
            b'c' => return all,
            other => panic!("unexpected message {other}"),
        }
    }
}

/// ヘッダとトレーラを検証し、tuple 列を返す。
fn parse_copy_binary(bytes: &[u8]) -> Vec<Tuple> {
    assert_eq!(&bytes[..11], SIGNATURE);
    assert_eq!(&bytes[11..19], &[0u8; 8], "flags and extension length");
    let mut pos = 19;
    let mut tuples = Vec::new();
    loop {
        let n = i16::from_be_bytes([bytes[pos], bytes[pos + 1]]);
        pos += 2;
        if n == -1 {
            assert_eq!(pos, bytes.len(), "trailer must be last");
            return tuples;
        }
        let mut t = Vec::new();
        for _ in 0..n {
            let len = i32::from_be_bytes(bytes[pos..pos + 4].try_into().unwrap());
            pos += 4;
            if len < 0 {
                t.push(None);
            } else {
                t.push(Some(bytes[pos..pos + len as usize].to_vec()));
                pos += len as usize;
            }
        }
        tuples.push(t);
    }
}

fn copy_binary(stream: &mut Stream, sql: &str) -> (Vec<Tuple>, String) {
    send_simple_query(stream, sql);
    let (overall, cols) = read_copy_out_formats(stream);
    assert_eq!(overall, 1);
    assert!(cols.iter().all(|c| *c == 1));
    let tuples = parse_copy_binary(&read_copy_stream(stream));
    let tag = read_command_complete(stream);
    (tuples, tag)
}

fn parse_data_row(body: &[u8]) -> Tuple {
    let n = i16::from_be_bytes([body[0], body[1]]) as usize;
    let mut pos = 2;
    let mut cells = Vec::new();
    for _ in 0..n {
        let len = i32::from_be_bytes(body[pos..pos + 4].try_into().unwrap());
        pos += 4;
        if len < 0 {
            cells.push(None);
        } else {
            cells.push(Some(body[pos..pos + len as usize].to_vec()));
            pos += len as usize;
        }
    }
    cells
}

#[test]
fn wire23_copy_to_binary_golden_bytes_for_one_row() {
    let (core, _g) = new_core();
    let mut s = alice(&core);
    insert(&mut s, 1, "ja", "7", "true", "op-1");

    send_simple_query(
        &mut s,
        "COPY (SELECT id, lang FROM docs LIMIT 10) TO STDOUT WITH (FORMAT binary)",
    );
    let (overall, cols) = read_copy_out_formats(&mut s);
    assert_eq!((overall, cols), (1, vec![1, 1]));
    // 先頭 CopyData はヘッダ + 最初の tuple、トレーラは単独の CopyData。
    let (kind, first) = read_message(&mut s);
    assert_eq!(kind, b'd');
    let mut expect = Vec::new();
    expect.extend_from_slice(SIGNATURE);
    expect.extend_from_slice(&[0; 8]);
    expect.extend_from_slice(&2i16.to_be_bytes());
    expect.extend_from_slice(&10i32.to_be_bytes());
    expect.extend_from_slice(&NUMERIC_ONE);
    expect.extend_from_slice(&2i32.to_be_bytes());
    expect.extend_from_slice(b"ja");
    assert_eq!(first, expect);
    let (kind, trailer) = read_message(&mut s);
    assert_eq!((kind, trailer), (b'd', vec![0xff, 0xff]));
    let (kind, _) = read_message(&mut s);
    assert_eq!(kind, b'c');
    assert_eq!(read_command_complete(&mut s), "COPY 1");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn wire23_copy_to_binary_zero_rows_is_header_and_trailer_only() {
    let (core, _g) = new_core();
    let mut s = alice(&core);
    send_simple_query(
        &mut s,
        "COPY (SELECT id FROM docs LIMIT 10) TO STDOUT WITH (FORMAT binary)",
    );
    let (overall, cols) = read_copy_out_formats(&mut s);
    assert_eq!((overall, cols), (1, vec![1]));
    let (kind, body) = read_message(&mut s);
    assert_eq!(kind, b'd');
    let mut expect = SIGNATURE.to_vec();
    expect.extend_from_slice(&[0; 8]);
    expect.extend_from_slice(&[0xff, 0xff]);
    assert_eq!(body, expect);
    assert_eq!(read_message(&mut s).0, b'c');
    assert_eq!(read_command_complete(&mut s), "COPY 0");
    read_ready_for_query(&mut s);
}

/// COPY binary の tuple フィールドは、同じ SELECT を result format 1 で取った DataRow の
/// フィールドと同一バイト列（NULL を含む）。
#[test]
fn wire23_copy_to_binary_fields_match_data_row_binary_fields() {
    let (core, _g) = new_core();
    let mut s = alice(&core);
    insert(&mut s, 1, "ja", "7", "true", "op-1");
    insert(&mut s, 2, "en", "NULL", "false", "op-2");
    let q = "SELECT id, lang, n, flag FROM docs LIMIT 10";
    let (mut tuples, tag) = copy_binary(&mut s, &format!("COPY ({q}) TO STDOUT (FORMAT binary)"));
    assert_eq!(tag, "COPY 2");
    read_ready_for_query(&mut s);
    tuples.sort_by_key(|t| t[0].clone());

    send_length_prefixed_message(&mut s, b'P', &parse_body("s", q, 0));
    assert_eq!(read_message(&mut s).0, b'1');
    let mut bind = bind_body("p", "s");
    let n = bind.len();
    bind[n - 2..].copy_from_slice(&1i16.to_be_bytes());
    bind.extend_from_slice(&1i16.to_be_bytes());
    send_length_prefixed_message(&mut s, b'B', &bind);
    assert_eq!(read_message(&mut s).0, b'2');
    send_length_prefixed_message(&mut s, b'E', &execute_body("p", 0));
    let mut rows: Vec<Tuple> = Vec::new();
    loop {
        let (kind, body) = read_message(&mut s);
        match kind {
            b'D' => rows.push(parse_data_row(&body)),
            b'C' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    send_length_prefixed_message(&mut s, b'S', b"");
    assert_eq!(read_message(&mut s).0, b'Z');
    rows.sort_by_key(|t| t[0].clone());
    assert_eq!(tuples, rows);
    let with_null = tuples
        .iter()
        .find(|t| t[1].as_deref() == Some(b"en".as_slice()))
        .expect("row en");
    assert_eq!(with_null[2], None, "NULL is carried as -1");
}

#[test]
fn wire23_copy_to_binary_legacy_syntax_matches_new_syntax() {
    let (core, _g) = new_core();
    let mut s = alice(&core);
    insert(&mut s, 1, "ja", "7", "true", "op-1");
    let (a, _) = copy_binary(
        &mut s,
        "COPY (SELECT id, lang FROM docs LIMIT 10) TO STDOUT WITH (FORMAT binary)",
    );
    read_ready_for_query(&mut s);
    let (b, _) = copy_binary(
        &mut s,
        "COPY (SELECT id, lang FROM docs LIMIT 10) TO STDOUT BINARY",
    );
    read_ready_for_query(&mut s);
    assert_eq!(a, b);
}

#[test]
fn wire23_copy_to_binary_unsupported_type_is_rejected_before_copy_out() {
    let (core, _g) = new_core();
    let mut s = alice(&core);
    insert(&mut s, 1, "ja", "7", "true", "op-1");
    send_simple_query(
        &mut s,
        "COPY (SELECT embedding FROM docs LIMIT 10) TO STDOUT WITH (FORMAT binary)",
    );
    // CopyOutResponse より前に 0A000。接続は維持される。
    expect_error_response_with_sqlstate(&mut s, "0A000");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    send_simple_query(&mut s, "SELECT id FROM docs LIMIT 1");
    let _ = read_row_description(&mut s);
    let _ = read_data_row(&mut s);
    let _ = read_command_complete(&mut s);
    read_ready_for_query(&mut s);
}

#[test]
fn wire23_copy_to_binary_returns_only_own_tenant_rows() {
    let (core, _g) = new_core();
    let mut a = alice(&core);
    let mut b = connect(Arc::clone(&core), "bob", "pw-bob");
    insert(&mut a, 1, "ja", "7", "true", "op-a");
    insert(&mut b, 2, "en", "8", "false", "op-b");
    let (tuples, tag) = copy_binary(
        &mut a,
        "COPY (SELECT lang FROM docs LIMIT 10) TO STDOUT WITH (FORMAT binary)",
    );
    assert_eq!(tag, "COPY 1");
    assert_eq!(tuples, vec![vec![Some(b"ja".to_vec())]]);
    read_ready_for_query(&mut a);
}

#[test]
fn wire23_copy_to_binary_followed_by_select_responds_in_order() {
    let (core, _g) = new_core();
    let mut s = alice(&core);
    insert(&mut s, 1, "ja", "7", "true", "op-1");
    send_simple_query(
        &mut s,
        "COPY (SELECT lang FROM docs LIMIT 10) TO STDOUT WITH (FORMAT binary); SELECT id FROM docs LIMIT 1",
    );
    let _ = read_copy_out_formats(&mut s);
    let _ = read_copy_stream(&mut s);
    assert_eq!(read_command_complete(&mut s), "COPY 1");
    let _ = read_row_description(&mut s);
    let _ = read_data_row(&mut s);
    assert_eq!(read_command_complete(&mut s), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}

#[test]
fn wire23_copy_to_binary_in_transaction_sees_uncommitted_and_failure_marks_failed() {
    let (core, _g) = new_core();
    let mut s = alice(&core);
    exec(&mut s, "BEGIN");
    insert(&mut s, 1, "ja", "7", "true", "op-1");
    let (tuples, tag) = copy_binary(
        &mut s,
        "COPY (SELECT lang FROM docs LIMIT 10) TO STDOUT WITH (FORMAT binary)",
    );
    assert_eq!((tuples.len(), tag.as_str()), (1, "COPY 1"));
    assert_eq!(read_ready_for_query_status(&mut s), b'T');
    send_simple_query(
        &mut s,
        "COPY (SELECT embedding FROM docs LIMIT 10) TO STDOUT WITH (FORMAT binary)",
    );
    expect_error_response_with_sqlstate(&mut s, "0A000");
    assert_eq!(read_ready_for_query_status(&mut s), b'E');
}

#[test]
fn wire23_copy_to_binary_conflicting_option_is_rejected() {
    let (core, _g) = new_core();
    let mut s = alice(&core);
    send_simple_query(
        &mut s,
        "COPY (SELECT id FROM docs LIMIT 10) TO STDOUT WITH (FORMAT binary, DELIMITER ',')",
    );
    expect_error_response_with_sqlstate(&mut s, "0A000");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    // 追加のバイトが届かないこと。
    s.set_read_timeout(Some(std::time::Duration::from_millis(50)))
        .expect("timeout");
    let mut b = [0u8; 1];
    assert!(s.read(&mut b).is_err());
}
