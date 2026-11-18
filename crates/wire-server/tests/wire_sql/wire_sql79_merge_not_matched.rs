//! `MERGE ... WHEN NOT MATCHED [BY TARGET]` の INSERT と `WHEN NOT MATCHED BY SOURCE`
//! （SQL-79・Issue #3354。親 #3349、設計記録 `docs/design/sql-merge.md`）の pg wire 結合テスト。
//!
//! 簡易クエリ（psql 流の末尾 `;` 付き単独送信を含む）と拡張クエリ（Parse／Bind／Execute／Sync。INSERT の値の
//! `$n` を含む）で、INSERT・BY SOURCE の DELETE／UPDATE が `CommandComplete MERGE n`（UPDATE・DELETE・
//! INSERT の実変更行数の合計）で完了し、失敗が ErrorResponse の SQLSTATE と `ReadyForQuery` の状態になり、
//! 行が変わらないことを固定する。サーバーはポート 0・一意な一時ディレクトリで独立させ、時間計測は使わない。

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

fn start() -> (SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql79-merge-not-matched");
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
        // t = {1:10, 2:20}、s = {2:200, 3:300}。2 は一致、1 は BY SOURCE、3 は BY TARGET。
        "INSERT INTO t (k, v) VALUES (1, 10), (2, 20)",
        "INSERT INTO s (k, v) VALUES (2, 200), (3, 300)",
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

/// `t` の `k:v` を k 昇順に `,` で連結した文字列（簡易クエリ）。
fn t_rows(s: &mut TcpStream) -> String {
    send_simple_query(s, "SELECT k, v FROM t ORDER BY k");
    read_row_description(s);
    let mut out = Vec::new();
    loop {
        let (ty, body) = read_message(s);
        match ty {
            b'D' => {
                // DataRow: 列数(2) + (長さ(4) + 値) × 2。
                let mut pos = 2usize;
                let mut cells = Vec::new();
                for _ in 0..2 {
                    let len = i32::from_be_bytes([
                        body[pos],
                        body[pos + 1],
                        body[pos + 2],
                        body[pos + 3],
                    ]);
                    let len = usize::try_from(len).expect("non-null");
                    pos += 4;
                    cells.push(String::from_utf8_lossy(&body[pos..pos + len]).to_string());
                    pos += len;
                }
                out.push(cells.join(":"));
            }
            b'C' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    read_ready_for_query(s);
    out.join(",")
}

/// 拡張クエリの `Sync` までの全メッセージを読み、(ErrorResponse の SQLSTATE, 最後の CommandComplete の
/// タグ, ReadyForQuery の状態) を返す。
fn read_until_ready(s: &mut TcpStream) -> (Option<String>, Option<String>, u8) {
    let mut code = None;
    let mut tag = None;
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
            b'Z' => return (code, tag, body.first().copied().unwrap_or(0)),
            _ => {}
        }
    }
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

#[test]
fn simple_query_insert_and_by_source_report_the_merge_tag_with_semicolon() {
    let (addr, _g) = start();
    let mut s = client(addr);
    // psql 流の末尾 `;` 付き単独送信。INSERT（k = 3）が 1 行、BY SOURCE の UPDATE（k = 1）が 1 行。
    for (sql, want) in [
        (
            "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN INSERT (k, v) VALUES (s.k, s.v);",
            "MERGE 1",
        ),
        (
            "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED BY SOURCE THEN UPDATE SET v = t.v + 1;",
            "MERGE 1",
        ),
    ] {
        send_simple_query(&mut s, sql);
        assert_eq!(read_command_complete(&mut s), want, "{sql}");
        assert_eq!(read_ready_for_query_status(&mut s), b'I', "{sql}");
    }
    assert_eq!(t_rows(&mut s), "1:11,2:20,3:300");
    // 3 種の混在は件数の合計（2: MATCHED の UPDATE、3 は既に挿入済みで一致、1: BY SOURCE の DELETE）。
    send_simple_query(
        &mut s,
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = s.v \
         WHEN NOT MATCHED THEN INSERT (k, v) VALUES (s.k, s.v) \
         WHEN NOT MATCHED BY SOURCE THEN DELETE;",
    );
    assert_eq!(read_command_complete(&mut s), "MERGE 3");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    assert_eq!(t_rows(&mut s), "2:200,3:300");
}

#[test]
fn simple_query_failure_is_reported_and_leaves_the_rows_unchanged() {
    let (addr, _g) = start();
    let mut s = client(addr);
    // 一意性違反（ON の残余を常に偽にして全行を未一致にし、既存の主キー k = 2 へ挿入させる）。
    send_simple_query(
        &mut s,
        "MERGE INTO t USING s ON t.k = s.k AND t.v < 0 \
         WHEN NOT MATCHED THEN INSERT (k, v) VALUES (s.k, s.v);",
    );
    expect_error_response_with_sqlstate(&mut s, "23505");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    assert_eq!(t_rows(&mut s), "1:10,2:20");
}

#[test]
fn simple_query_in_an_explicit_transaction_commits_and_rolls_back() {
    let (addr, _g) = start();
    let mut s = client(addr);
    let merge =
        "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN INSERT (k, v) VALUES (s.k, s.v)";
    send_simple_query(&mut s, "BEGIN");
    assert_eq!(read_command_complete(&mut s), "BEGIN");
    read_ready_for_query(&mut s);
    send_simple_query(&mut s, merge);
    assert_eq!(read_command_complete(&mut s), "MERGE 1");
    assert_eq!(read_ready_for_query_status(&mut s), b'T');
    send_simple_query(&mut s, "ROLLBACK");
    assert_eq!(read_command_complete(&mut s), "ROLLBACK");
    read_ready_for_query(&mut s);
    assert_eq!(t_rows(&mut s), "1:10,2:20");
    send_simple_query(&mut s, "BEGIN");
    assert_eq!(read_command_complete(&mut s), "BEGIN");
    read_ready_for_query(&mut s);
    send_simple_query(&mut s, merge);
    assert_eq!(read_command_complete(&mut s), "MERGE 1");
    read_ready_for_query(&mut s);
    send_simple_query(&mut s, "COMMIT");
    assert_eq!(read_command_complete(&mut s), "COMMIT");
    read_ready_for_query(&mut s);
    assert_eq!(t_rows(&mut s), "1:10,2:20,3:300");
}

#[test]
fn extended_query_insert_values_bind_parameters() {
    let (addr, _g) = start();
    let mut s = client(addr);
    // INSERT の値の `$n`。元の行（3:300）が未一致なので 1 行挿入される。
    let sql =
        "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN INSERT (k, v) VALUES (s.k, $1)";
    send_length_prefixed_message(&mut s, b'P', &parse_body("", sql, 0));
    send_length_prefixed_message(&mut s, b'B', &bind_with_text_params("", &["777"]));
    send_length_prefixed_message(&mut s, b'E', &execute_body("", 0));
    send_length_prefixed_message(&mut s, b'S', &[]);
    let (code, tag, status) = read_until_ready(&mut s);
    assert_eq!(code, None);
    assert_eq!(tag.as_deref(), Some("MERGE 1"));
    assert_eq!(status, b'I');
    assert_eq!(t_rows(&mut s), "1:10,2:20,3:777");

    // 不正な値は ErrorResponse（22P02）で、行は変わらない。
    send_length_prefixed_message(&mut s, b'P', &parse_body("", "MERGE INTO t USING s ON t.k = s.k AND t.v < 0 WHEN NOT MATCHED BY SOURCE THEN UPDATE SET v = $1", 0));
    send_length_prefixed_message(&mut s, b'B', &bind_with_text_params("", &["not-a-number"]));
    send_length_prefixed_message(&mut s, b'E', &execute_body("", 0));
    send_length_prefixed_message(&mut s, b'S', &[]);
    let (code, _tag, status) = read_until_ready(&mut s);
    assert_eq!(code.as_deref(), Some("22P02"));
    assert_eq!(status, b'I');
    assert_eq!(t_rows(&mut s), "1:10,2:20,3:777");
}
