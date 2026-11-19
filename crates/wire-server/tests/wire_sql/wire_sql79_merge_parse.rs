//! `MERGE` 文の構文解析と実行（SQL-79・Issue #3351・#3353）の pg wire 結合テスト。
//!
//! 簡易クエリ（psql 流の末尾 `;` 付き単独送信を含む）と拡張クエリ（Parse／Bind／Execute／Sync と
//! Parse／Describe／Sync）で、解析を通った MERGE が `CommandComplete MERGE n` で完了し、
//! ReadyForQuery の状態が正しいことを固定する（`RETURNING` つきは `wire_sql79_merge_returning.rs`。Issue #3356）。束縛と値位置の `$n` は `wire_sql79_merge_bind.rs`（Issue #3352）、`21000`・
//! 拡張クエリの `$n` の実行は `wire_sql79_merge_matched.rs`（Issue #3353）。
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

const MERGE_SQL: &str = "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN DELETE";

fn start() -> (SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql79-merge");
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
        "INSERT INTO t (k, v) VALUES (1, 10)",
        "INSERT INTO s (k, v) VALUES (1, 100)",
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

/// `t` の行数（簡易クエリの `SELECT COUNT(*)`）。
fn count_t(s: &mut TcpStream) -> String {
    send_simple_query(s, "SELECT COUNT(*) FROM t");
    read_row_description(s);
    let row = read_data_row(s);
    let _tag = read_command_complete(s);
    read_ready_for_query(s);
    row.first().cloned().flatten().expect("count")
}

/// 拡張クエリの 1 往復で返ってきた ErrorResponse の SQLSTATE（無ければ `None`）と、最後の
/// ReadyForQuery の状態を読む。`Sync` までの全メッセージを消費する。
fn read_until_ready(s: &mut TcpStream) -> (Option<String>, u8) {
    let mut code = None;
    loop {
        let (ty, body) = read_message(s);
        match ty {
            b'E' => {
                let text = String::from_utf8_lossy(&body).to_string();
                code = text
                    .split('\0')
                    .find_map(|f| f.strip_prefix('C').map(str::to_string));
            }
            b'Z' => return (code, body.first().copied().unwrap_or(0)),
            _ => {}
        }
    }
}

fn send_sync(s: &mut TcpStream) {
    send_length_prefixed_message(s, b'S', &[]);
}

/// 拡張クエリの 1 往復で返ってきた最後の CommandComplete のタグ（無ければ `None`）。
fn read_until_ready_with_tag(s: &mut TcpStream) -> (Option<String>, Option<String>, u8) {
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

const NOT_MATCHED_SQL: &str = "MERGE INTO t USING s ON t.k = s.k WHEN NOT MATCHED THEN \
     INSERT (k, v) VALUES (s.k, s.v)";

#[test]
fn simple_query_merge_alone_with_semicolon_runs_and_ready_idle() {
    let (addr, _g) = start();
    let mut s = client(addr);
    for sql in [MERGE_SQL.to_string(), format!("{MERGE_SQL};")] {
        send_simple_query(&mut s, &sql);
        assert_eq!(read_command_complete(&mut s), "MERGE 1", "{sql}");
        assert_eq!(read_ready_for_query_status(&mut s), b'I');
        assert_eq!(count_t(&mut s), "0", "{sql}");
        // 次の形のために対象の行を戻す。
        send_simple_query(&mut s, "INSERT INTO t (k, v) VALUES (1, 10)");
        assert_eq!(read_command_complete(&mut s), "INSERT 0 1");
        read_ready_for_query(&mut s);
    }
    assert_eq!(count_t(&mut s), "1");
}

// NOT MATCHED を含む形は #3354 から実行される。元だけにある行（k = 2）を 1 回目で挿入し（`MERGE 1`）、
// 2 回目（末尾 `;` 付き）は一致するので何もしない（`MERGE 0`）。
#[test]
fn simple_query_not_matched_alone_with_semicolon_runs_and_ready_idle() {
    let (addr, _g) = start();
    let mut s = client(addr);
    send_simple_query(&mut s, "INSERT INTO s (k, v) VALUES (2, 200)");
    assert_eq!(read_command_complete(&mut s), "INSERT 0 1");
    read_ready_for_query(&mut s);
    for (sql, want) in [
        (NOT_MATCHED_SQL.to_string(), "MERGE 1"),
        (format!("{NOT_MATCHED_SQL};"), "MERGE 0"),
    ] {
        send_simple_query(&mut s, &sql);
        assert_eq!(read_command_complete(&mut s), want, "{sql}");
        assert_eq!(read_ready_for_query_status(&mut s), b'I');
        assert_eq!(count_t(&mut s), "2", "{sql}");
    }
}

#[test]
fn simple_query_merge_in_explicit_transaction_commits_and_a_failure_fails_the_transaction() {
    let (addr, _g) = start();
    let mut s = client(addr);
    send_simple_query(&mut s, "BEGIN");
    assert_eq!(read_command_complete(&mut s), "BEGIN");
    read_ready_for_query(&mut s);
    send_simple_query(&mut s, MERGE_SQL);
    assert_eq!(read_command_complete(&mut s), "MERGE 1");
    assert_eq!(read_ready_for_query_status(&mut s), b'T');
    send_simple_query(&mut s, "ROLLBACK");
    assert_eq!(read_command_complete(&mut s), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    assert_eq!(count_t(&mut s), "1");

    // 失敗した MERGE は既存の失敗文と同じ規約（Tx は失敗状態 `E`）。ここでは多重一致の `21000`。
    send_simple_query(&mut s, "INSERT INTO s (k, v) VALUES (2, 200)");
    assert_eq!(read_command_complete(&mut s), "INSERT 0 1");
    read_ready_for_query(&mut s);
    send_simple_query(&mut s, "BEGIN");
    assert_eq!(read_command_complete(&mut s), "BEGIN");
    read_ready_for_query(&mut s);
    send_simple_query(
        &mut s,
        "MERGE INTO t USING s ON t.v < s.v WHEN MATCHED THEN DELETE",
    );
    expect_error_response_with_sqlstate(&mut s, "21000");
    assert_eq!(read_ready_for_query_status(&mut s), b'E');
    send_simple_query(&mut s, "ROLLBACK");
    assert_eq!(read_command_complete(&mut s), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    assert_eq!(count_t(&mut s), "1");
}

#[test]
fn simple_query_merge_followed_by_another_statement_keeps_the_write_rule() {
    let (addr, _g) = start();
    let mut s = client(addr);
    // MERGE は `Write` に分類されるため、後ろに文が続くメッセージ全体は 1 つの暗黙トランザクションで実行される
    // （既存の DML と同じ扱い）。
    send_simple_query(&mut s, &format!("{MERGE_SQL}; SELECT COUNT(*) FROM t"));
    assert_eq!(read_command_complete(&mut s), "MERGE 1");
    read_row_description(&mut s);
    let row = read_data_row(&mut s);
    assert_eq!(row.first().cloned().flatten().as_deref(), Some("0"));
    let _tag = read_command_complete(&mut s);
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    assert_eq!(count_t(&mut s), "0");
}

#[test]
fn simple_query_syntax_error_is_42601() {
    let (addr, _g) = start();
    let mut s = client(addr);
    send_simple_query(&mut s, "MERGE INTO t USING s ON t.k = s.k");
    expect_error_response_with_sqlstate(&mut s, "42601");
    read_ready_for_query(&mut s);
}

#[test]
fn extended_query_execute_runs_and_describe_succeeds() {
    let (addr, _g) = start();
    let mut s = client(addr);

    // Parse／Bind／Execute／Sync。拡張クエリの Execute も `MERGE n` で完了する。
    send_length_prefixed_message(&mut s, b'P', &parse_body("m1", MERGE_SQL, 0));
    send_length_prefixed_message(&mut s, b'B', &bind_body("p1", "m1"));
    send_length_prefixed_message(&mut s, b'E', &execute_body("p1", 0));
    send_sync(&mut s);
    let (code, tag, status) = read_until_ready_with_tag(&mut s);
    assert_eq!(code, None);
    assert_eq!(tag.as_deref(), Some("MERGE 1"));
    assert_eq!(status, b'I');

    // Parse／Describe（文）／Sync。Describe は束縛を通って成功する（結果列なし。Issue #3352）。
    send_length_prefixed_message(&mut s, b'P', &parse_body("m2", MERGE_SQL, 0));
    let mut describe = vec![b'S'];
    describe.extend_from_slice(b"m2\0");
    send_length_prefixed_message(&mut s, b'D', &describe);
    send_sync(&mut s);
    let (code, status) = read_until_ready(&mut s);
    assert_eq!(code, None);
    assert_eq!(status, b'I');

    assert_eq!(count_t(&mut s), "0");
}

#[test]
fn extended_query_placeholders_in_unsupported_positions_are_42601() {
    let (addr, _g) = start();
    let mut s = client(addr);
    // 値位置以外の `$n`（ON・WHEN の AND の条件・式の内側・部分クエリの元）は Parse の時点で拒否される
    // （値位置の `$n` は `wire_sql79_merge_bind.rs`）。
    for (i, sql) in [
        "MERGE INTO t USING s ON t.k = $1 WHEN MATCHED THEN DELETE",
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED AND s.v = $1 THEN DELETE",
        "MERGE INTO t USING s ON t.k = s.k WHEN MATCHED THEN UPDATE SET v = $1 + 1",
        "MERGE INTO t USING (SELECT k FROM s WHERE v = $1) AS q ON t.k = q.k WHEN MATCHED THEN DELETE",
    ]
    .iter()
    .enumerate()
    {
        send_length_prefixed_message(&mut s, b'P', &parse_body(&format!("m3-{i}"), sql, 0));
        send_sync(&mut s);
        let (code, _) = read_until_ready(&mut s);
        assert_eq!(code.as_deref(), Some("42601"), "{sql}");
    }
}
