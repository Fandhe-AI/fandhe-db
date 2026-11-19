//! `MERGE ... WHEN MATCHED` の実行（SQL-79・Issue #3353。親 #3349、設計記録 `docs/design/sql-merge.md`）の
//! pg wire 結合テスト。
//!
//! 簡易クエリ（psql 流の末尾 `;` 付き単独送信を含む）と拡張クエリ（Parse／Bind／Execute／Sync。値位置の
//! `$n` を含む）で、UPDATE・DELETE・DO NOTHING が `CommandComplete MERGE n`（UPDATE と DELETE の実変更行数の
//! 合計）で完了し、多重一致が ErrorResponse `21000` と `ReadyForQuery`（autocommit は `I`）になり、行が
//! 変わらないことを固定する。サーバーはポート 0・一意な一時ディレクトリで独立させ、時間計測は使わない。

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
    let path = temp_db::unique_db_path("wire-sql79-merge-matched");
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

/// `t` の `v` を k 昇順に `,` で連結した文字列（簡易クエリ）。
fn t_values(s: &mut TcpStream) -> String {
    send_simple_query(s, "SELECT v FROM t ORDER BY k");
    read_row_description(s);
    let mut out = Vec::new();
    loop {
        let (ty, body) = read_message(s);
        match ty {
            b'D' => {
                // DataRow: 列数(2) + (長さ(4) + 値)。
                let len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]);
                let len = usize::try_from(len).expect("non-null");
                out.push(String::from_utf8_lossy(&body[6..6 + len]).to_string());
            }
            b'C' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    read_ready_for_query(s);
    out.join(",")
}

fn send_sync(s: &mut TcpStream) {
    send_length_prefixed_message(s, b'S', &[]);
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
fn simple_query_update_delete_and_do_nothing_report_the_merge_tag() {
    let (addr, _g) = start();
    let mut s = client(addr);
    // psql 流の末尾 `;` 付き単独送信。UPDATE は 2 行、DO NOTHING は 0 行。
    for (sql, want) in [
        (
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DO NOTHING;",
            "MERGE 0",
        ),
        (
            "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = s.v + t.v;",
            "MERGE 2",
        ),
    ] {
        send_simple_query(&mut s, sql);
        assert_eq!(read_command_complete(&mut s), want, "{sql}");
        assert_eq!(read_ready_for_query_status(&mut s), b'I', "{sql}");
    }
    assert_eq!(t_values(&mut s), "110,220");
    // UPDATE と DELETE の混在は件数の合計。
    send_simple_query(
        &mut s,
        "MERGE INTO t USING s ON t.k = s.k \
         WHEN MATCHED AND s.v > 150 THEN DELETE WHEN MATCHED THEN UPDATE SET v = 0",
    );
    assert_eq!(read_command_complete(&mut s), "MERGE 2");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    assert_eq!(t_values(&mut s), "0");
}

#[test]
fn simple_query_multiple_matches_are_21000_and_the_connection_stays_usable() {
    let (addr, _g) = start();
    let mut s = client(addr);
    // 対象 k=1 (v=10) は 10 < 100 と 10 < 200 の 2 行に一致する。
    for sql in [
        "MERGE INTO t USING s ON t.v < s.v WHEN MATCHED THEN UPDATE SET v = s.v",
        "MERGE INTO t USING s ON t.v < s.v WHEN MATCHED THEN DELETE;",
    ] {
        send_simple_query(&mut s, sql);
        expect_error_response_with_sqlstate(&mut s, "21000");
        assert_eq!(read_ready_for_query_status(&mut s), b'I', "{sql}");
        assert_eq!(t_values(&mut s), "10,20", "{sql}");
    }
}

#[test]
fn extended_query_execute_binds_values_and_reports_the_tag_and_21000() {
    let (addr, _g) = start();
    let mut s = client(addr);
    let update = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = $1";
    send_length_prefixed_message(&mut s, b'P', &parse_body("", update, 0));
    send_length_prefixed_message(&mut s, b'B', &bind_with_text_params("", &["7"]));
    send_length_prefixed_message(&mut s, b'E', &execute_body("", 0));
    send_sync(&mut s);
    let (code, tag, status) = read_until_ready(&mut s);
    assert_eq!(code, None);
    assert_eq!(tag.as_deref(), Some("MERGE 2"));
    assert_eq!(status, b'I');
    assert_eq!(t_values(&mut s), "7,7");

    // 多重一致（7 < 100 と 7 < 200）。ErrorResponse の後も `Sync` で `ReadyForQuery` に戻る。
    let multi = "MERGE INTO t USING s ON t.v < s.v WHEN MATCHED THEN UPDATE SET v = $1";
    send_length_prefixed_message(&mut s, b'P', &parse_body("", multi, 0));
    send_length_prefixed_message(&mut s, b'B', &bind_with_text_params("", &["9"]));
    send_length_prefixed_message(&mut s, b'E', &execute_body("", 0));
    send_sync(&mut s);
    let (code, _, status) = read_until_ready(&mut s);
    assert_eq!(code.as_deref(), Some("21000"));
    assert_eq!(status, b'I');
    assert_eq!(t_values(&mut s), "7,7");
}
