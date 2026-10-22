//! 拡張クエリ COPY の失敗後の Sync 同期回復の結合テスト（Issue #1761・WIRE-21 (3) のポインタ。
//! 親 Issue #1758）。
//!
//! COPY の失敗後は ErrorResponse を Sync 前に届け、Sync まで後続メッセージ（COPY 系を含む）を
//! 読み捨て、Sync で ReadyForQuery を返して接続を継続する。読み捨て予算超過・本文付き
//! CopyDone・Terminate は fail-closed に切断する。成功系・単純な CopyFail 回復は
//! `wire21_copy_execute.rs`（Issue #1760）が担う。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::Read;
use std::net::{Shutdown, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

const COPY_FROM: &str = "COPY documents (id, embedding, body) FROM STDIN";

const ROW1: &[u8] = b"1\t[1.0,0.0,0.0]\talpha\n";

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire21-copy-sync-recovery");
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

fn connect_as(core: Arc<EngineCore>, user: &str) -> TcpStream {
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);
    let pw = if user == "alice" {
        "pw-alice"
    } else {
        "pw-bob"
    };
    authenticate_to_ready_for_query(addr, user, pw)
}

fn connect(core: Arc<EngineCore>) -> TcpStream {
    connect_as(core, "alice")
}

fn send(stream: &mut TcpStream, kind: u8, body: &[u8]) {
    send_length_prefixed_message(stream, kind, body);
}

fn sync(stream: &mut TcpStream) {
    send(stream, b'S', b"");
}

fn flush(stream: &mut TcpStream) {
    send(stream, b'H', b"");
}

fn read_until_ready(stream: &mut TcpStream) -> u8 {
    loop {
        let (kind, body) = read_message(stream);
        if kind == b'Z' {
            return body[0];
        }
    }
}

fn simple(stream: &mut TcpStream, sql: &str) -> u8 {
    send_simple_query(stream, sql);
    read_until_ready(stream)
}

/// 簡易クエリで `documents` の行数（現在のテナント分）を数える。
fn count_rows(stream: &mut TcpStream) -> usize {
    send_simple_query(stream, "SELECT id FROM documents LIMIT 100");
    let mut rows = 0;
    loop {
        let (kind, _) = read_message(stream);
        match kind {
            b'D' => rows += 1,
            b'Z' => return rows,
            _ => {}
        }
    }
}

/// Parse／Bind を送り、ParseComplete／BindComplete を確認する（Execute は送らない）。
fn parse_bind(stream: &mut TcpStream, sql: &str, portal: &str) {
    send(stream, b'P', &parse_body("", sql, 0));
    send(stream, b'B', &bind_body(portal, ""));
    expect(stream, b'1');
    expect(stream, b'2');
}

fn execute(stream: &mut TcpStream, portal: &str) {
    send(stream, b'E', &execute_body(portal, 0));
}

fn expect(stream: &mut TcpStream, want: u8) -> Vec<u8> {
    let (kind, body) = read_message(stream);
    assert_eq!(
        kind,
        want,
        "expected {}, got {}: {:?}",
        want as char,
        kind as char,
        String::from_utf8_lossy(&body)
    );
    body
}

fn copy_fail(stream: &mut TcpStream, reason: &str) {
    send(stream, b'f', &[reason.as_bytes(), b"\0"].concat());
}

/// 拡張クエリで COPY を開始し、CopyFail で失敗させて ErrorResponse まで読む（同期回復モードへ入る）。
fn fail_copy(stream: &mut TcpStream) {
    parse_bind(stream, COPY_FROM, "p");
    execute(stream, "p");
    flush(stream);
    expect(stream, b'G');
    send(stream, b'd', ROW1);
    copy_fail(stream, "abort");
    expect(stream, b'E');
}

/// 一定時間サーバーから何も届かない（応答が無い）ことを確認する。
fn assert_silent(stream: &mut TcpStream) {
    stream
        .set_read_timeout(Some(Duration::from_millis(300)))
        .expect("set timeout");
    let mut b = [0u8; 1];
    match stream.read(&mut b) {
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            ) => {}
        other => panic!("expected silence, got {other:?}"),
    }
    stream.set_read_timeout(None).expect("reset timeout");
}

fn select_ok_extended(stream: &mut TcpStream) {
    parse_bind(stream, "SELECT id FROM documents LIMIT 10", "sel");
    execute(stream, "sel");
    sync(stream);
    loop {
        let (kind, body) = read_message(stream);
        assert_ne!(kind, b'E', "{}", String::from_utf8_lossy(&body));
        if kind == b'Z' {
            assert_eq!(body, b"I");
            return;
        }
    }
}

/// 受け入れ条件: デコード失敗の ErrorResponse は Sync 前に届き、Flush は無応答、
/// Sync で ReadyForQuery、その後の拡張・簡易クエリが成功する。
#[test]
fn error_is_delivered_before_sync_and_connection_recovers() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    parse_bind(&mut s, COPY_FROM, "p");
    execute(&mut s, "p");
    flush(&mut s);
    expect(&mut s, b'G');
    send(&mut s, b'd', b"1\t[1.0,0.0]\talpha\n");
    send(&mut s, b'c', b"");
    let body = expect(&mut s, b'E');
    assert!(String::from_utf8_lossy(&body).contains("22000"));
    flush(&mut s);
    assert_silent(&mut s);
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"I");
    select_ok_extended(&mut s);
    assert_eq!(count_rows(&mut s), 0);
    s.shutdown(Shutdown::Write).ok();
}

/// CopyInResponse 前の失敗（`Failed` 状態のトランザクションでの COPY 開始＝`25P02`）の後に
/// 届く CopyData／CopyDone／CopyFail は応答なしで読み捨てられ、Sync で回復する。
#[test]
fn copy_messages_after_pre_copy_failure_are_discarded() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    assert_eq!(simple(&mut s, "BEGIN"), b'T');
    fail_copy(&mut s);
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"E");
    send(&mut s, b'P', &parse_body("", COPY_FROM, 0));
    let body = expect(&mut s, b'E');
    assert!(String::from_utf8_lossy(&body).contains("25P02"));
    send(&mut s, b'd', ROW1);
    send(&mut s, b'c', b"");
    copy_fail(&mut s, "late");
    assert_silent(&mut s);
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"E");
    assert_eq!(simple(&mut s, "ROLLBACK"), b'I');
    assert_eq!(simple(&mut s, "SELECT 1"), b'I');
    s.shutdown(Shutdown::Write).ok();
}

/// COPY 失敗後、Sync 前に送ったパイプライン（Bind／Execute／Flush）は読み捨てられる。
#[test]
fn pipelined_messages_after_copy_failure_are_discarded() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    fail_copy(&mut s);
    parse_bind_unchecked(&mut s);
    assert_silent(&mut s);
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"I");
    select_ok_extended(&mut s);
    s.shutdown(Shutdown::Write).ok();
}

fn parse_bind_unchecked(stream: &mut TcpStream) {
    send(stream, b'P', &parse_body("", "SELECT id FROM documents", 0));
    send(stream, b'B', &bind_body("x", ""));
    execute(stream, "x");
    flush(stream);
}

/// 明示トランザクション中の COPY 失敗でトランザクションは `Failed`（'E'）になり、
/// 後続の文は `25P02`。ROLLBACK で回復する。
#[test]
fn failure_inside_explicit_transaction_fails_it_and_rollback_recovers() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    assert_eq!(simple(&mut s, "BEGIN"), b'T');
    fail_copy(&mut s);
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"E");
    send(&mut s, b'P', &parse_body("", "SELECT id FROM documents", 0));
    let body = expect(&mut s, b'E');
    assert!(String::from_utf8_lossy(&body).contains("25P02"));
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"E");
    assert_eq!(simple(&mut s, "ROLLBACK"), b'I');
    assert_eq!(count_rows(&mut s), 0);
    s.shutdown(Shutdown::Write).ok();
}

/// COPY TO の対象が存在しない場合も ErrorResponse の後 Sync で回復する。
#[test]
fn copy_to_failure_recovers_on_sync() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    send(
        &mut s,
        b'P',
        &parse_body("", "COPY (SELECT id FROM missing_table) TO STDOUT", 0),
    );
    expect(&mut s, b'E');
    sync(&mut s);
    assert_eq!(expect(&mut s, b'Z'), b"I");
    assert_eq!(count_rows(&mut s), 0);
    s.shutdown(Shutdown::Write).ok();
}

/// 同期回復中に COPY 系メッセージを件数上限超で送り続けると `08P01` で切断する。
#[test]
fn discard_budget_overflow_in_recovery_closes_connection() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    fail_copy(&mut s);
    for _ in 0..4100 {
        send(&mut s, b'd', b"");
    }
    expect_error_response_with_sqlstate(&mut s, "08P01");
    expect_connection_closed(&mut s);
}

/// Sync で読み捨て予算は 0 に戻る（回復ごとに予算が独立）。
#[test]
fn discard_budget_resets_on_sync() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    for _ in 0..3 {
        fail_copy(&mut s);
        for _ in 0..2000 {
            send(&mut s, b'd', b"");
        }
        sync(&mut s);
        assert_eq!(expect(&mut s, b'Z'), b"I");
    }
    s.shutdown(Shutdown::Write).ok();
}

/// 同期回復中の本文付き CopyDone は固定形状違反として切断する。
#[test]
fn copy_done_with_body_in_recovery_closes_connection() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    fail_copy(&mut s);
    send(&mut s, b'c', b"x");
    expect(&mut s, b'E');
    expect_connection_closed(&mut s);
}

/// 同期回復中の Terminate は接続を閉じる。
#[test]
fn terminate_in_recovery_closes_connection() {
    let (core, _g) = new_core();
    let mut s = connect(core);
    fail_copy(&mut s);
    send(&mut s, b'X', b"");
    expect_connection_closed(&mut s);
}
