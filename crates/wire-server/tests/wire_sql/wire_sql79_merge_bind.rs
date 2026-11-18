//! `MERGE` の束縛（Issue #3352。親 #3349）の pg wire 結合テスト。
//!
//! 簡易クエリ（psql 流の末尾 `;` 付き単独送信を含む）で束縛エラーの SQLSTATE と ReadyForQuery の状態を、
//! 拡張クエリで値位置の `$n` の Parse 成功・ParameterDescription（対象の列型の OID）・Bind／Execute の
//! 結果（束縛エラーはそのコード、束縛を通った `WHEN MATCHED` だけの形は実行〔#3353〕、NOT MATCHED を含む形は
//! 固定文言の `0A000`）を固定する。
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

const INT8_OID: u32 = 20;
const TEXT_OID: u32 = 25;

fn start() -> (SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql79-merge-bind");
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
        "CREATE TABLE t (k BIGINT PRIMARY KEY, v BIGINT, name TEXT)",
        "CREATE TABLE s (k BIGINT PRIMARY KEY, v BIGINT, name TEXT)",
        "INSERT INTO t (k, v, name) VALUES (1, 10, 'a')",
        "INSERT INTO s (k, v, name) VALUES (1, 100, 'b')",
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

fn count_t(s: &mut TcpStream) -> String {
    send_simple_query(s, "SELECT COUNT(*) FROM t");
    read_row_description(s);
    let row = read_data_row(s);
    let _tag = read_command_complete(s);
    read_ready_for_query(s);
    row.first().cloned().flatten().expect("count")
}

fn bind_with_text_params(statement: &str, values: &[&str]) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(0); // portal ""
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

/// `Sync` までの全メッセージを読み、(ParameterDescription の OID 列, ErrorResponse の SQLSTATE,
/// ReadyForQuery の状態) を返す。
fn read_until_ready(s: &mut TcpStream) -> (Vec<u32>, Option<String>, u8) {
    let mut oids = Vec::new();
    let mut code = None;
    loop {
        let (ty, body) = read_message(s);
        match ty {
            b't' => {
                let n = usize::from(u16::from_be_bytes([body[0], body[1]]));
                for i in 0..n {
                    let at = 2 + i * 4;
                    oids.push(u32::from_be_bytes([
                        body[at],
                        body[at + 1],
                        body[at + 2],
                        body[at + 3],
                    ]));
                }
            }
            b'E' => {
                let text = String::from_utf8_lossy(&body).to_string();
                code = text
                    .split('\0')
                    .find_map(|f| f.strip_prefix('C').map(str::to_string));
            }
            b'Z' => return (oids, code, body.first().copied().unwrap_or(0)),
            _ => {}
        }
    }
}

#[test]
fn simple_query_binding_errors_keep_their_sqlstate_with_and_without_semicolon() {
    let (addr, _g) = start();
    let mut s = client(addr);
    for (sql, want) in [
        (
            "MERGE INTO t USING s ON t.nope = s.k WHEN MATCHED THEN DELETE",
            "42703",
        ),
        (
            "MERGE INTO t USING s ON k = s.k WHEN MATCHED THEN DELETE",
            "42702",
        ),
        (
            "MERGE INTO t USING s ON z.k = s.k WHEN MATCHED THEN DELETE",
            "42P01",
        ),
        (
            "MERGE INTO t USING s ON t.name = s.v WHEN MATCHED THEN DELETE",
            "42804",
        ),
    ] {
        for text in [sql.to_string(), format!("{sql};")] {
            send_simple_query(&mut s, &text);
            expect_error_response_with_sqlstate(&mut s, want);
            assert_eq!(read_ready_for_query_status(&mut s), b'I', "{text}");
        }
    }
    assert_eq!(count_t(&mut s), "1");
}

#[test]
fn simple_query_binding_error_in_explicit_transaction_fails_the_transaction() {
    let (addr, _g) = start();
    let mut s = client(addr);
    send_simple_query(&mut s, "BEGIN");
    assert_eq!(read_command_complete(&mut s), "BEGIN");
    read_ready_for_query(&mut s);
    send_simple_query(
        &mut s,
        "MERGE INTO t USING s ON t.nope = s.k WHEN MATCHED THEN DELETE",
    );
    expect_error_response_with_sqlstate(&mut s, "42703");
    assert_eq!(read_ready_for_query_status(&mut s), b'E');
    send_simple_query(&mut s, "ROLLBACK");
    assert_eq!(read_command_complete(&mut s), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    assert_eq!(count_t(&mut s), "1");
}

#[test]
fn extended_query_value_placeholders_are_parsed_and_described_with_the_target_column_types() {
    let (addr, _g) = start();
    let mut s = client(addr);
    let cases: [(&str, &[u32]); 3] = [
        (
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = $1, name = $2",
            &[INT8_OID, TEXT_OID],
        ),
        (
            "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN INSERT (name, v) VALUES ($1, $2)",
            &[TEXT_OID, INT8_OID],
        ),
        (
            // 列リスト省略は対象表の宣言順（k, v, name）へ当てる。
            "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN INSERT VALUES (s.k, $1, $2);",
            &[INT8_OID, TEXT_OID],
        ),
    ];
    for (i, (sql, want)) in cases.iter().enumerate() {
        let name = format!("m{i}");
        send_length_prefixed_message(&mut s, b'P', &parse_body(&name, sql, 0));
        let mut describe = vec![b'S'];
        describe.extend_from_slice(name.as_bytes());
        describe.push(0);
        send_length_prefixed_message(&mut s, b'D', &describe);
        send_length_prefixed_message(&mut s, b'S', &[]);
        let (oids, code, status) = read_until_ready(&mut s);
        assert_eq!(oids, *want, "{sql}");
        // Parse・Describe は束縛を通って成功する（結果列なし。実行は Execute の固定文言 0A000）。
        assert_eq!(code, None, "{sql}");
        assert_eq!(status, b'I');
    }
    assert_eq!(count_t(&mut s), "1");
}

// 束縛を通った NOT MATCHED の形は、実行を入れる #3354 まで固定文言の `0A000` で拒否する。
// tracked: #3354
#[test]
fn extended_query_execute_reports_binding_errors_before_the_fixed_0a000() {
    let (addr, _g) = start();
    let mut s = client(addr);
    for (sql, values, want) in [
        (
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET nope = $1",
            vec!["7"],
            "42703",
        ),
        (
            "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN INSERT (k, v) VALUES (s.name, $1)",
            vec!["7"],
            "42804",
        ),
        (
            "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN INSERT (k, v) VALUES (s.k, $1)",
            vec!["7"],
            "0A000",
        ),
    ] {
        send_length_prefixed_message(&mut s, b'P', &parse_body("", sql, 0));
        send_length_prefixed_message(&mut s, b'B', &bind_with_text_params("", &values));
        send_length_prefixed_message(&mut s, b'E', &execute_body("", 0));
        send_length_prefixed_message(&mut s, b'S', &[]);
        let (_, code, status) = read_until_ready(&mut s);
        assert_eq!(code.as_deref(), Some(want), "{sql}");
        assert_eq!(status, b'I');
    }
    assert_eq!(count_t(&mut s), "1");
}
