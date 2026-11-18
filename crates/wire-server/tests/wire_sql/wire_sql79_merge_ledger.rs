//! `MERGE ... USING OPERATION_ID` の台帳（再送）と `EXPLAIN MERGE`（SQL-79・Issue #3357）の pg wire 結合テスト。
//!
//! 簡易クエリ（psql 流の末尾 `;` 付き単独送信を含む）で CommandComplete `MERGE n`、同一内容の再送が
//! ErrorResponse `23505`、内容違いが `22023`（いずれも ReadyForQuery `I`）、拡張クエリ（Parse／Bind `$1`／
//! Execute）の再送が `23505`、明示 Tx の ROLLBACK 後の再送が成功すること、`EXPLAIN MERGE` が木の各行を
//! DataRow で返し `$n` が `42601` になることを固定する。
//! サーバーはポート 0・一意な一時ディレクトリで独立させ、時間計測は使わない。

use crate::common;

use crate::temp_db;

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::Storage;

use common::*;

const MERGE: &str = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = s.v";

fn start() -> (SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql79-merge-ledger");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let mut s = SessionState::default();
    s.allow_ddl();
    let ctx = PolicyContext::with_visibilities(
        "tenant-a",
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("tenant");
    for sql in [
        "CREATE TABLE t (k BIGINT PRIMARY KEY, v BIGINT)",
        "CREATE TABLE s (k BIGINT PRIMARY KEY, v BIGINT)",
        "INSERT INTO t (k, v) VALUES (1, 10), (2, 20)",
        "INSERT INTO s (k, v) VALUES (1, 100), (2, 200)",
    ] {
        core.execute_sql_in_session(&ctx, &mut s, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    (addr, guard)
}

fn client(addr: SocketAddr) -> TcpStream {
    authenticate_to_ready_for_query(addr, "alice", "correct-horse")
}

fn sum_v(s: &mut TcpStream) -> String {
    send_simple_query(s, "SELECT SUM(v) FROM t");
    read_row_description(s);
    let row = read_data_row(s);
    let _tag = read_command_complete(s);
    read_ready_for_query(s);
    row.first().cloned().flatten().expect("sum")
}

fn send_sync(s: &mut TcpStream) {
    send_length_prefixed_message(s, b'S', &[]);
}

/// 簡易クエリの成功（`MERGE n` と ReadyForQuery `I`）。
fn merge_ok(s: &mut TcpStream, sql: &str, tag: &str) {
    send_simple_query(s, sql);
    assert_eq!(read_command_complete(s), tag, "{sql}");
    assert_eq!(read_ready_for_query_status(s), b'I', "{sql}");
}

/// 簡易クエリの失敗（ErrorResponse と ReadyForQuery の状態）。
fn merge_err(s: &mut TcpStream, sql: &str, code: &str, status: u8) {
    send_simple_query(s, sql);
    expect_error_response_with_sqlstate(s, code);
    assert_eq!(read_ready_for_query_status(s), status, "{sql}");
}

fn bind_with_text_params(portal: &str, statement: &str, values: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes());
    body.extend_from_slice(&i16::try_from(values.len()).expect("count").to_be_bytes());
    for v in values {
        body.extend_from_slice(&i32::try_from(v.len()).expect("len").to_be_bytes());
        body.extend_from_slice(v.as_bytes());
    }
    body.extend_from_slice(&0i16.to_be_bytes());
    body
}

/// 拡張クエリ 1 往復の（ErrorResponse の SQLSTATE、CommandComplete のタグ）。
fn read_round(s: &mut TcpStream) -> (Option<String>, Option<String>) {
    let (mut code, mut tag) = (None, None);
    loop {
        let (ty, body) = read_message(s);
        match ty {
            b'E' => {
                let text = String::from_utf8_lossy(&body).to_string();
                code = text
                    .split('\0')
                    .find_map(|f| f.strip_prefix('C').map(str::to_string));
            }
            b'C' => {
                tag = Some(
                    String::from_utf8_lossy(&body)
                        .trim_end_matches('\0')
                        .to_string(),
                );
            }
            b'Z' => return (code, tag),
            _ => {}
        }
    }
}

// 簡易クエリ: 1 回目は `MERGE 2`、同一内容の再送は `23505`、内容違いは `22023`。行は再送で変わらない。
#[test]
fn simple_query_resend_is_23505_and_mismatch_is_22023() {
    let (addr, _g) = start();
    let mut s = client(addr);
    let sql = format!("{MERGE} USING OPERATION_ID 'w-1';");
    merge_ok(&mut s, &sql, "MERGE 2");
    assert_eq!(sum_v(&mut s), "300");
    merge_err(&mut s, &sql, "23505", b'I');
    merge_err(&mut s, &sql.replace(';', ""), "23505", b'I');
    // 内容違い（別の WHEN の形）は `22023`。
    merge_err(
        &mut s,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE USING OPERATION_ID 'w-1';",
        "22023",
        b'I',
    );
    assert_eq!(sum_v(&mut s), "300");
}

// 拡張クエリ: Parse・Bind `$1`・Execute の再送は `23505`、値違いは `22023`。
#[test]
fn extended_query_resend_is_23505() {
    let (addr, _g) = start();
    let mut s = client(addr);
    let sql = format!("{MERGE} USING OPERATION_ID $1");
    send_length_prefixed_message(&mut s, b'P', &parse_body("m1", &sql, 0));
    send_sync(&mut s);
    assert_eq!(read_round(&mut s), (None, None));
    for (portal, expected_code, expected_tag) in
        [("p1", None, Some("MERGE 2")), ("p2", Some("23505"), None)]
    {
        send_length_prefixed_message(&mut s, b'B', &bind_with_text_params(portal, "m1", &["x-1"]));
        send_length_prefixed_message(&mut s, b'E', &execute_body(portal, 0));
        send_sync(&mut s);
        let (code, tag) = read_round(&mut s);
        assert_eq!(code.as_deref(), expected_code, "{portal}");
        assert_eq!(tag.as_deref(), expected_tag, "{portal}");
    }
    assert_eq!(sum_v(&mut s), "300");
}

// 明示 Tx: ROLLBACK 後は台帳が残らず再送が成功し、COMMIT 後の再送は `23505`。
#[test]
fn explicit_transaction_rollback_releases_the_operation_id() {
    let (addr, _g) = start();
    let mut s = client(addr);
    let sql = format!("{MERGE} USING OPERATION_ID 'tx-1'");
    send_simple_query(&mut s, "BEGIN");
    assert_eq!(read_command_complete(&mut s), "BEGIN");
    read_ready_for_query(&mut s);
    send_simple_query(&mut s, &sql);
    assert_eq!(read_command_complete(&mut s), "MERGE 2");
    assert_eq!(read_ready_for_query_status(&mut s), b'T');
    send_simple_query(&mut s, "ROLLBACK");
    assert_eq!(read_command_complete(&mut s), "ROLLBACK");
    read_ready_for_query(&mut s);
    assert_eq!(sum_v(&mut s), "30");
    merge_ok(&mut s, &sql, "MERGE 2");
    merge_err(&mut s, &sql, "23505", b'I');
}

/// DataRow 本体（列数 i16、列ごとに長さ i32 と値）の先頭列を文字列にする。
fn first_column(body: &[u8]) -> String {
    let len = body
        .get(2..6)
        .map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .expect("length");
    let len = usize::try_from(len).expect("non-null");
    String::from_utf8_lossy(body.get(6..6 + len).expect("value")).to_string()
}

// `EXPLAIN MERGE` は木の各行を DataRow で返し、台帳・行に触れない。`$n` は ErrorResponse `42601`。
#[test]
fn explain_merge_returns_the_tree_and_rejects_params() {
    let (addr, _g) = start();
    let mut s = client(addr);
    for sql in [
        format!("EXPLAIN {MERGE} USING OPERATION_ID 'e-1'"),
        format!("EXPLAIN {MERGE};"),
    ] {
        send_simple_query(&mut s, &sql);
        read_row_description(&mut s);
        let mut lines = Vec::new();
        let tag = loop {
            let (ty, body) = read_message(&mut s);
            match ty {
                b'D' => lines.push(first_column(&body)),
                b'C' => {
                    break String::from_utf8_lossy(&body)
                        .trim_end_matches('\0')
                        .to_string()
                }
                other => panic!("unexpected message {other}"),
            }
        };
        assert_eq!(lines.first().map(String::as_str), Some("merge"), "{sql}");
        assert!(lines.contains(&"-> join".to_string()), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("e-1")), "{lines:?}");
        assert!(tag.starts_with("EXPLAIN"), "{tag}");
        assert_eq!(read_ready_for_query_status(&mut s), b'I');
    }
    // EXPLAIN は台帳を消費しない（同じ operation_id の本実行が成功する）。
    merge_ok(
        &mut s,
        &format!("{MERGE} USING OPERATION_ID 'e-1'"),
        "MERGE 2",
    );
    // `$1` つきの EXPLAIN の Parse は ErrorResponse `42601`。
    let with_param = format!("EXPLAIN {}", MERGE.replace("s.v", "$1"));
    send_length_prefixed_message(&mut s, b'P', &parse_body("", &with_param, 0));
    send_sync(&mut s);
    assert_eq!(read_round(&mut s).0.as_deref(), Some("42601"));
}
