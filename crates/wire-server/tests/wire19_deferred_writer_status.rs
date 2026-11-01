//! writer（単一ライタ）の遅延取得後の各状態で、`ReadyForQuery` の状態バイト
//! （`'I'`／`'T'`／`'E'`）が正しいことを wire 越しに固定する（Issue #2410。親 #2408・TASK-335。
//! ポインタ: WIRE-19・SQL-31）。
//!
//! 明示トランザクションは最初の書き込み文で writer を取る（Issue #2400）ため、writer を持たない
//! `Active`、最初の書き込み文での `55P03`／`40001` による `Failed` といった状態が生じる。
//! `wire_proto/wire19_ready_for_query_status.rs`・`wire942_extended_transaction.rs` は
//! `BEGIN` → `INSERT` 型の遷移を固定済みで、本ファイルは遅延取得に固有の遷移（簡易・複数文・
//! 拡張クエリ・期限切れ）を担う。暗黙トランザクションの開始時取得は
//! `write_lock_wait_parity.rs` が担う。待機上限・持続時間に依存するため単独バイナリとする。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::sql::transaction::TransactionLimits;
use engine::storage::{Storage, Visibility};

use common::*;

const USERS: [(&str, &str, &str); 2] = [
    ("alice", "tenant-a", "correct-horse"),
    ("bob", "tenant-b", "battery-staple"),
];

fn new_core(
    label: &str,
    wait_ms: u64,
    max_duration: Option<Duration>,
) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path)
        .expect("open storage")
        .with_write_lock_wait(Duration::from_millis(wait_ms));
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(max_duration) = max_duration {
        core = core.with_transaction_limits(TransactionLimits {
            max_duration,
            max_statements: 1_000,
        });
    }
    (Arc::new(core), guard)
}

fn insert(id: u32) -> String {
    format!(
        "INSERT INTO documents (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]') USING OPERATION_ID 'op-2410-s-{id}'"
    )
}

const SELECT: &str = "SELECT id FROM documents LIMIT 100";

fn connect(addr: std::net::SocketAddr) -> (TcpStream, TcpStream) {
    (
        authenticate_to_ready_for_query(addr, "alice", "correct-horse"),
        authenticate_to_ready_for_query(addr, "bob", "battery-staple"),
    )
}

/// 簡易クエリで `CommandComplete` が `tag`・状態バイトが `status` で終わる 1 文を流す。
fn run_ok(stream: &mut TcpStream, sql: &str, tag: &str, status: u8) {
    send_simple_query(stream, sql);
    assert_eq!(read_command_complete(stream), tag, "{sql}");
    assert_eq!(read_ready_for_query_status(stream), status, "{sql}");
}

/// `RowDescription` の後の `DataRow` を読み飛ばし、`CommandComplete` のタグを返す。
fn read_rows_then_tag(stream: &mut TcpStream) -> String {
    let _ = read_row_description(stream);
    loop {
        let (kind, body) = read_message(stream);
        match kind {
            b'D' => continue,
            b'C' => {
                let end = body.len().saturating_sub(1);
                return String::from_utf8_lossy(&body[..end]).into_owned();
            }
            other => panic!("unexpected message {}", other as char),
        }
    }
}

/// `SELECT` を流し、応答を読み切って状態バイトを検証する（行の中身は見ない）。
fn run_select(stream: &mut TcpStream, status: u8) {
    send_simple_query(stream, SELECT);
    let _ = read_rows_then_tag(stream);
    assert_eq!(read_ready_for_query_status(stream), status);
}

/// 簡易クエリの文が `sqlstate` のエラーになり、状態バイトが `status` で終わることを検証する。
fn run_err(stream: &mut TcpStream, sql: &str, sqlstate: &str, status: u8) {
    send_simple_query(stream, sql);
    expect_error_response_with_sqlstate(stream, sqlstate);
    assert_eq!(read_ready_for_query_status(stream), status, "{sql}");
}

/// 読み取りだけの明示トランザクションは writer を取らず `'T'` のまま、他接続の書き込みを妨げない。
#[test]
fn read_only_explicit_transaction_reports_t_without_blocking_writers() {
    let (core, _g) = new_core("wire19-2410-s1", 200, None);
    let users_path = write_user_store_file(&USERS);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let (mut a, mut b) = connect(addr);

    run_ok(&mut a, "BEGIN", "BEGIN", b'T');
    run_select(&mut a, b'T');
    run_ok(&mut b, &insert(1), "INSERT 0 1", b'I');
    run_ok(&mut a, "COMMIT", "COMMIT", b'I');
}

/// 最初の書き込み文で writer を取得しても状態は `'T'` のまま。取得後は他接続の書き込みが
/// `55P03`（`'I'`）になり、`COMMIT` 後は成功する。
#[test]
fn first_write_acquires_writer_and_status_stays_t() {
    let (core, _g) = new_core("wire19-2410-s2", 200, None);
    let users_path = write_user_store_file(&USERS);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let (mut a, mut b) = connect(addr);

    run_ok(&mut a, "BEGIN", "BEGIN", b'T');
    run_select(&mut a, b'T');
    run_ok(&mut a, &insert(1), "INSERT 0 1", b'T');
    run_err(&mut b, &insert(2), "55P03", b'I');
    run_ok(&mut a, "COMMIT", "COMMIT", b'I');
    run_ok(&mut b, &insert(2), "INSERT 0 1", b'I');
}

/// 他接続が writer を握っていても `BEGIN`・`SELECT` は待たず `'T'`。最初の書き込み文が
/// `55P03` になると `'E'`（`25P02` を経て `ROLLBACK` で `'I'`）。保持側は `'T'` のまま。
#[test]
fn first_write_55p03_moves_to_failed_e_until_rollback() {
    let (core, _g) = new_core("wire19-2410-s3", 150, None);
    let users_path = write_user_store_file(&USERS);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let (mut a, mut b) = connect(addr);

    run_ok(&mut b, "BEGIN", "BEGIN", b'T');
    run_ok(&mut b, &insert(1), "INSERT 0 1", b'T');

    run_ok(&mut a, "BEGIN", "BEGIN", b'T');
    run_select(&mut a, b'T');
    run_err(&mut a, &insert(2), "55P03", b'E');
    run_err(&mut a, SELECT, "25P02", b'E');
    run_err(&mut a, "COMMIT", "25P02", b'E');
    run_ok(&mut a, "ROLLBACK", "ROLLBACK", b'I');

    run_ok(&mut b, "COMMIT", "COMMIT", b'I');
}

/// 読んだ表に BEGIN 後の commit が入った状態の最初の書き込みは `40001`・`'E'`。`ROLLBACK` で
/// `'I'` に戻り、書き込みは残らず、再試行は成功する。エラー本文に表名・テナント名を含めない。
#[test]
fn serialization_failure_40001_moves_to_failed_e_and_retry_succeeds() {
    let path = temp_db::unique_db_path("wire19-2410-s4");
    let _g = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::with_visibilities("sys", [Visibility::Public, Visibility::Private])
        .expect("tenant");
    for t in ["sf1", "sf2"] {
        let mut session = SessionState::default();
        session.allow_ddl();
        core.execute_sql_in_session(&ctx, &mut session, &format!("CREATE TABLE {t} (n BIGINT)"))
            .expect("create table");
    }
    let core = Arc::new(core);
    let users_path = write_user_store_file(&USERS);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut a = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut other = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    run_ok(&mut a, "BEGIN", "BEGIN", b'T');
    send_simple_query(&mut a, "SELECT n FROM sf1 LIMIT 100");
    let _ = read_rows_then_tag(&mut a);
    assert_eq!(read_ready_for_query_status(&mut a), b'T');
    run_ok(
        &mut other,
        "INSERT INTO sf1 (n) VALUES (1)",
        "INSERT 0 1",
        b'I',
    );

    send_simple_query(&mut a, "INSERT INTO sf2 (n) VALUES (2)");
    let (kind, body) = read_message(&mut a);
    assert_eq!(kind, b'E');
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("40001"), "{text:?}");
    assert!(
        !text.contains("tenant") && !text.contains("sf1") && !text.contains("sf2"),
        "{text:?}"
    );
    assert_eq!(read_ready_for_query_status(&mut a), b'E');
    run_err(&mut a, "SELECT n FROM sf1 LIMIT 100", "25P02", b'E');
    run_ok(&mut a, "ROLLBACK", "ROLLBACK", b'I');

    // 書き込みは残らず（sf2 は空）、再試行は成功する。
    send_simple_query(&mut a, "SELECT n FROM sf2 LIMIT 100");
    assert_eq!(read_rows_then_tag(&mut a), "SELECT 0");
    assert_eq!(read_ready_for_query_status(&mut a), b'I');
    run_ok(&mut a, "BEGIN", "BEGIN", b'T');
    send_simple_query(&mut a, "SELECT n FROM sf1 LIMIT 100");
    assert_eq!(read_rows_then_tag(&mut a), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut a), b'T');
    run_ok(&mut a, "INSERT INTO sf2 (n) VALUES (2)", "INSERT 0 1", b'T');
    run_ok(&mut a, "COMMIT", "COMMIT", b'I');
}

/// 1 メッセージ内の `BEGIN; ...` でも writer は最初の書き込み文まで遅延する。`BEGIN; SELECT` は
/// 待たずに `'T'`、`BEGIN; SELECT; INSERT` は `BEGIN`・`SELECT` の応答の後に `55P03`・`'E'`
/// （暗黙トランザクションが開始時に待つ挙動との対照）。
#[test]
fn multi_statement_with_begin_defers_writer_and_reports_e_on_55p03() {
    let (core, _g) = new_core("wire19-2410-s5", 150, None);
    let users_path = write_user_store_file(&USERS);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let (mut a, mut b) = connect(addr);

    run_ok(&mut b, "BEGIN", "BEGIN", b'T');
    run_ok(&mut b, &insert(1), "INSERT 0 1", b'T');

    send_simple_query(&mut a, &format!("BEGIN; {SELECT}"));
    assert_eq!(read_command_complete(&mut a), "BEGIN");
    let _ = read_rows_then_tag(&mut a);
    assert_eq!(read_ready_for_query_status(&mut a), b'T');
    run_ok(&mut a, "ROLLBACK", "ROLLBACK", b'I');

    send_simple_query(&mut a, &format!("BEGIN; {SELECT}; {}", insert(2)));
    assert_eq!(read_command_complete(&mut a), "BEGIN");
    let _ = read_rows_then_tag(&mut a);
    expect_error_response_with_sqlstate(&mut a, "55P03");
    assert_eq!(read_ready_for_query_status(&mut a), b'E');
    run_ok(&mut a, "ROLLBACK", "ROLLBACK", b'I');

    run_ok(&mut b, "ROLLBACK", "ROLLBACK", b'I');
}

/// Parse/Bind/Execute/Sync の 1 往復。成功なら `Ok(CommandComplete タグ)`、`ErrorResponse` なら
/// `Err(本文)`。続く `Sync` の `ReadyForQuery` 状態バイトを `status` と照合する。
fn extended(stream: &mut TcpStream, sql: &str, status: u8) -> Result<String, String> {
    send_length_prefixed_message(stream, b'P', &parse_body("", sql, 0));
    assert_eq!(read_message(stream).0, b'1', "ParseComplete for {sql}");
    send_length_prefixed_message(stream, b'B', &bind_body("", ""));
    assert_eq!(read_message(stream).0, b'2', "BindComplete for {sql}");
    send_length_prefixed_message(stream, b'E', &execute_body("", 0));
    let outcome = loop {
        let (kind, body) = read_message(stream);
        match kind {
            b'D' => continue,
            b'C' => {
                let end = body.len().saturating_sub(1);
                break Ok(String::from_utf8_lossy(&body[..end]).into_owned());
            }
            b'E' => break Err(String::from_utf8_lossy(&body).into_owned()),
            other => panic!("unexpected message {}", other as char),
        }
    };
    send_length_prefixed_message(stream, b'S', b"");
    assert_eq!(read_ready_for_query_status(stream), status, "{sql}");
    outcome
}

/// 拡張クエリでも `Sync` の状態バイトは遅延取得後の状態に従う（`BEGIN`→`'T'`、`SELECT`→`'T'`、
/// 最初の書き込みの `55P03`→`'E'`、`ROLLBACK`→`'I'`）。
#[test]
fn extended_query_sync_status_follows_deferred_writer_states() {
    let (core, _g) = new_core("wire19-2410-s6", 150, None);
    let users_path = write_user_store_file(&USERS);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let (mut a, mut b) = connect(addr);

    run_ok(&mut b, "BEGIN", "BEGIN", b'T');
    run_ok(&mut b, &insert(1), "INSERT 0 1", b'T');

    assert_eq!(extended(&mut a, "BEGIN", b'T').as_deref(), Ok("BEGIN"));
    assert!(extended(&mut a, SELECT, b'T').is_ok());
    let err = extended(&mut a, &insert(2), b'E').expect_err("55P03");
    assert!(err.contains("55P03"), "{err:?}");
    assert_eq!(
        extended(&mut a, "ROLLBACK", b'I').as_deref(),
        Ok("ROLLBACK")
    );

    run_ok(&mut b, "ROLLBACK", "ROLLBACK", b'I');
}

/// writer を持たない `Active`（`BEGIN`・`SELECT` のみ）も持続時間の上限で `Failed` になる。
/// 次の文は `54000`・`'E'`、`ROLLBACK` で `'I'`。待っている間も他接続は書き込め、接続は切れない。
#[test]
fn expired_transaction_without_writer_reports_e_on_next_statement() {
    let max_duration = Duration::from_millis(200);
    let (core, _g) = new_core("wire19-2410-s7", 150, Some(max_duration));
    let users_path = write_user_store_file(&USERS);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let (mut a, mut b) = connect(addr);

    run_ok(&mut a, "BEGIN", "BEGIN", b'T');
    run_select(&mut a, b'T');
    std::thread::sleep(max_duration * 2);

    run_ok(&mut b, &insert(1), "INSERT 0 1", b'I');
    run_err(&mut a, SELECT, "54000", b'E');
    run_ok(&mut a, "ROLLBACK", "ROLLBACK", b'I');
}
