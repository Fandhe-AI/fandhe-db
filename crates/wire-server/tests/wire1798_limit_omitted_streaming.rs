//! `LIMIT` 省略結果の分割送出と切断時の中断（Issue #1798・SQL-34・WIRE-11 のポインタ）の
//! 結合テスト。
//!
//! 確認すること:
//! - simple / extended のどちらでも、上限内なら最後まで返り、上限超過は `54000` の
//!   ErrorResponse だけを返す（RowDescription・DataRow を 1 つも送らない）。
//! - Execute の `max_rows` 分割（PostgreSQL 17 の PortalSuspended・完了タグの規則）。
//! - 実行中にクライアントが切断しても、接続スロットが返りサーバーが健全なまま残る
//!   （切断検出は `crate::liveness`。タイミング依存のため「最終的に回復する」ことだけを固定する）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::result_limits::ResultLimits;
use engine::sql::mode::SessionState;
use engine::storage::Storage;
use wire_server::limits::SessionLimits;

use common::*;

const CAP: usize = 5;

fn ctx() -> PolicyContext {
    PolicyContext::new("tenant-a").expect("valid tenant id")
}

fn new_core(label: &str, cap: usize) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(ResultLimits::new(cap));
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(
        &ctx(),
        &mut session,
        "CREATE TABLE t (id BIGINT PRIMARY KEY, v INTEGER)",
    )
    .expect("create table");
    (Arc::new(core), guard)
}

fn seed(core: &EngineCore, from: i64, to: i64) {
    let mut start = from;
    while start <= to {
        let end = (start + 49).min(to);
        let values: Vec<String> = (start..=end)
            .map(|i| format!("({i}, {})", i * 10))
            .collect();
        core.execute_insert_sql(
            &ctx(),
            &format!("INSERT INTO t (id, v) VALUES {}", values.join(", ")),
        )
        .expect("seed");
        start = end + 1;
    }
}

fn tcp_connect(addr: std::net::SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout");
    send_ssl_request_and_startup(&mut stream, "alice", "irrelevant-db-name");
    assert_eq!(read_auth_request_type(&mut stream), 3);
    send_password_message(&mut stream, "correct-horse");
    loop {
        let (kind, _) = read_message(&mut stream);
        if kind == b'Z' {
            return stream;
        }
    }
}

fn start(label: &str, rows: i64) -> (std::net::SocketAddr, temp_db::CleanupGuard) {
    let (core, guard) = new_core(label, CAP);
    seed(&core, 1, rows);
    let users = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    (spawn_server_with_engine(&users, core), guard)
}

fn tag_of(body: &[u8]) -> String {
    String::from_utf8_lossy(&body[..body.len().saturating_sub(1)]).to_string()
}

/// 無名 statement／portal を使う（Sync を跨いで同名の再 Parse が衝突しないようにする）。
fn parse_and_bind(stream: &mut TcpStream, sql: &str) {
    send_length_prefixed_message(stream, b'P', &parse_body("", sql, 0));
    assert_eq!(read_message(stream).0, b'1', "ParseComplete");
    send_length_prefixed_message(stream, b'B', &bind_body("", ""));
    assert_eq!(read_message(stream).0, b'2', "BindComplete");
}

fn sync_and_status(stream: &mut TcpStream) -> u8 {
    send_length_prefixed_message(stream, b'S', b"");
    read_ready_for_query_status(stream)
}

/// simple・上限内: `LIMIT` 省略・`LIMIT ALL` とも全行と `SELECT n`、続けて ReadyForQuery。
#[test]
fn simple_within_cap_returns_all_rows() {
    let (addr, _g) = start("wire1798-simple-ok", CAP as i64);
    let mut stream = tcp_connect(addr);
    for sql in ["SELECT id FROM t", "SELECT id FROM t ORDER BY id LIMIT ALL"] {
        send_simple_query(&mut stream, sql);
        read_row_description(&mut stream);
        for _ in 0..CAP {
            read_data_row(&mut stream);
        }
        assert_eq!(read_command_complete(&mut stream), format!("SELECT {CAP}"));
        read_ready_for_query(&mut stream);
    }
}

/// simple・上限超過: 最初に届くのが ErrorResponse(`54000`)。RowDescription も DataRow も無い。
#[test]
fn simple_over_cap_sends_only_error_and_ready() {
    let (addr, _g) = start("wire1798-simple-over", CAP as i64 + 1);
    let mut stream = tcp_connect(addr);
    for sql in [
        "SELECT id FROM t",
        "SELECT id FROM t ORDER BY id",
        "SELECT id FROM t ORDER BY v LIMIT ALL",
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "54000");
        assert_eq!(read_ready_for_query_status(&mut stream), b'I', "{sql}");
    }
    // 接続はそのまま使える。
    send_simple_query(&mut stream, "SELECT id FROM t WHERE id <= 2");
    read_row_description(&mut stream);
    read_data_row(&mut stream);
    read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 2");
    read_ready_for_query(&mut stream);
}

/// extended・上限内: `max_rows` 分割は PostgreSQL 17 の規則（その回の件数・尽きた回も Suspended）。
#[test]
fn extended_within_cap_splits_like_postgres() {
    let (addr, _g) = start("wire1798-ext-ok", CAP as i64);
    let mut stream = tcp_connect(addr);

    // 5 行・max_rows=2: 2s, 2s, 1 + SELECT 1。
    parse_and_bind(&mut stream, "SELECT id FROM t ORDER BY id");
    let mut ids = Vec::new();
    for (batch, last) in [(2, false), (2, false), (1, true)] {
        send_length_prefixed_message(&mut stream, b'E', &execute_body("", 2));
        for _ in 0..batch {
            let (k, body) = read_message(&mut stream);
            assert_eq!(k, b'D');
            ids.push(body);
        }
        let (k, body) = read_message(&mut stream);
        if last {
            assert_eq!(k, b'C');
            assert_eq!(tag_of(&body), "SELECT 1");
        } else {
            assert_eq!(k, b's');
        }
    }
    assert_eq!(ids.len(), CAP);
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), CAP, "no duplicate or missing rows");
    assert_eq!(sync_and_status(&mut stream), b'I');

    // 5 行・max_rows=5: ちょうど尽きた回も Suspended、次が 0 行と SELECT 0。
    parse_and_bind(&mut stream, "SELECT id FROM t ORDER BY id");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("", CAP as i32));
    for _ in 0..CAP {
        assert_eq!(read_message(&mut stream).0, b'D');
    }
    assert_eq!(read_message(&mut stream).0, b's');
    send_length_prefixed_message(&mut stream, b'E', &execute_body("", CAP as i32));
    let (k, body) = read_message(&mut stream);
    assert_eq!(k, b'C');
    assert_eq!(tag_of(&body), "SELECT 0");
    assert_eq!(sync_and_status(&mut stream), b'I');

    // max_rows=0: 全行と SELECT n。
    parse_and_bind(&mut stream, "SELECT id FROM t ORDER BY id");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("", 0));
    for _ in 0..CAP {
        assert_eq!(read_message(&mut stream).0, b'D');
    }
    let (k, body) = read_message(&mut stream);
    assert_eq!(k, b'C');
    assert_eq!(tag_of(&body), format!("SELECT {CAP}"));
    assert_eq!(sync_and_status(&mut stream), b'I');
}

/// extended・上限超過: Execute は DataRow を 1 件も送らず ErrorResponse(`54000`)。
/// 明示トランザクション内では Sync 後の状態が `E`、`ROLLBACK` で通常に戻る。
#[test]
fn extended_over_cap_sends_no_data_row() {
    let (addr, _g) = start("wire1798-ext-over", CAP as i64 + 1);
    let mut stream = tcp_connect(addr);

    for max_rows in [0, 2] {
        parse_and_bind(&mut stream, "SELECT id FROM t ORDER BY id");
        send_length_prefixed_message(&mut stream, b'E', &execute_body("", max_rows));
        expect_error_response_with_sqlstate(&mut stream, "54000");
        assert_eq!(sync_and_status(&mut stream), b'I', "max_rows={max_rows}");
    }

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    read_ready_for_query(&mut stream);
    parse_and_bind(&mut stream, "SELECT id FROM t");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("", 2));
    expect_error_response_with_sqlstate(&mut stream, "54000");
    assert_eq!(sync_and_status(&mut stream), b'E');
    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// 切断（ベストエフォート）: 実行中に閉じても接続スロットが返り、後続の接続が通常に動く。
#[test]
fn client_disconnect_during_scan_releases_the_slot() {
    let (core, _g) = new_core("wire1798-disconnect", 100_000);
    seed(&core, 1, 3000);
    let users = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, limiter) = spawn_server_with_engine_and_limits(
        &users,
        core,
        1,
        Duration::from_secs(20),
        SessionLimits::default(),
    );

    let mut first = tcp_connect(addr);
    send_simple_query(&mut first, "SELECT id, v FROM t ORDER BY v");
    drop(first);
    wait_for_active_exact(&limiter, 0, Duration::from_secs(20));

    let mut second = connect_after_slot_available(addr, Duration::from_secs(10));
    // connect_after_slot_available は SSLRequest 応答 'N' まで進めた状態で返す。
    send_startup_message(&mut second, "alice", "irrelevant-db-name");
    assert_eq!(read_auth_request_type(&mut second), 3);
    send_password_message(&mut second, "correct-horse");
    loop {
        if read_message(&mut second).0 == b'Z' {
            break;
        }
    }
    send_simple_query(&mut second, "SELECT id FROM t WHERE id <= 2");
    read_row_description(&mut second);
    read_data_row(&mut second);
    read_data_row(&mut second);
    assert_eq!(read_command_complete(&mut second), "SELECT 2");
    read_ready_for_query(&mut second);
}

/// 切断（ベストエフォート）: Execute と Sync を続けて送り、未読の Sync を残したまま閉じても
/// 接続スロットが返る（未読バイトがあっても FIN を検出する。Issue #1798 の Codex P1）。
#[test]
fn pipelined_execute_sync_then_disconnect_releases_the_slot() {
    let (core, _g) = new_core("wire1798-disconnect-pipelined", 100_000);
    seed(&core, 1, 3000);
    let users = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let (addr, limiter) = spawn_server_with_engine_and_limits(
        &users,
        core,
        1,
        Duration::from_secs(20),
        SessionLimits::default(),
    );

    let mut first = tcp_connect(addr);
    parse_and_bind(&mut first, "SELECT id, v FROM t ORDER BY v");
    send_length_prefixed_message(&mut first, b'E', &execute_body("", 0));
    send_length_prefixed_message(&mut first, b'S', b"");
    drop(first);
    wait_for_active_exact(&limiter, 0, Duration::from_secs(20));
}
