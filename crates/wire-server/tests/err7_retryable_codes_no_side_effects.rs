//! 再試行してよい `wire_code`（`55P03`・`ROLLBACK` 後の `25P02`・`53300`）の各経路で、行・
//! `operation_id` 台帳・カタログ・採番状態に副作用が残らず、再試行（接続数上限は間隔を空けた
//! 再接続）が成功することを wire 越しに固定する（Issue #2421。ポインタ: ERR-7。親 TASK-341）。
//!
//! 既存テストとの分担: `write_lock_wait_parity.rs`・`wire19_deferred_writer_status.rs`・
//! `wire25_connection_limits_boundary.rs` は各コードの発生と状態バイトを固定する。本ファイルは
//! 「失敗の前後で観測できる状態が不変」「同じ `operation_id` の再試行がちょうど 1 回だけ反映される」
//! 「エラー本文に別テナントの識別子が出ない」を足す。`40001` は #2396 配下で固定済みのため対象外。
//!
//! 副作用の観測は wire 越しの `SELECT`・同一 `operation_id` での再試行・後続の DDL で行う
//! （台帳に失敗文のエントリが残っていれば再試行は反映されず、行数で判別できる）。
//! 書き込みロック待機の上限・接続数上限に依存するため、他テストへ束ねず単独バイナリにする。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::limits::{SessionLimits, SQLSTATE_TOO_MANY_CONNECTIONS};

use common::*;

const USERS: [(&str, &str, &str); 2] = [
    ("alice", "tenant-a", "correct-horse"),
    ("bob", "tenant-b", "battery-staple"),
];

fn new_core(label: &str) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(label);
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");
    let wait = wire_server::write_lock_wait_opt::resolve(Some("100")).expect("in range");
    let core = Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_write_lock_wait(wait),
    );
    (core, guard)
}

fn insert(id: u32, op: &str) -> String {
    format!(
        "INSERT INTO documents (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]') \
         USING OPERATION_ID '{op}'"
    )
}

const SELECT: &str = "SELECT id FROM documents ORDER BY id LIMIT 100";

/// `ReadyForQuery` までの全メッセージ（型, 本体）。
fn run(s: &mut TcpStream, sql: &str) -> Vec<(u8, Vec<u8>)> {
    send_simple_query(s, sql);
    let mut out = Vec::new();
    loop {
        let msg = read_message(s);
        let done = msg.0 == b'Z';
        out.push(msg);
        if done {
            return out;
        }
    }
}

fn status_of(msgs: &[(u8, Vec<u8>)]) -> u8 {
    *msgs
        .last()
        .expect("ReadyForQuery")
        .1
        .last()
        .expect("status")
}

fn error_of(msgs: &[(u8, Vec<u8>)]) -> Option<String> {
    msgs.iter()
        .find(|(t, _)| *t == b'E')
        .map(|(_, b)| String::from_utf8_lossy(b).into_owned())
}

fn tag_of(msgs: &[(u8, Vec<u8>)]) -> Option<String> {
    msgs.iter()
        .find(|(t, _)| *t == b'C')
        .map(|(_, b)| String::from_utf8_lossy(&b[..b.len().saturating_sub(1)]).into_owned())
}

/// 成功を要求し、`CommandComplete` のタグと状態バイトを検証する。
fn ok(s: &mut TcpStream, sql: &str, tag: &str, status: u8) {
    let m = run(s, sql);
    assert_eq!(error_of(&m), None, "{sql}");
    assert_eq!(tag_of(&m).as_deref(), Some(tag), "{sql}");
    assert_eq!(status_of(&m), status, "{sql}");
}

/// `sqlstate` のエラーかつ状態バイトが `status` であることを検証する。他テナントの識別子
/// （ユーザー名・テナント名）が本文に出ないことも併せて固定する。
fn err(s: &mut TcpStream, sql: &str, sqlstate: &str, status: u8) {
    let m = run(s, sql);
    let body = error_of(&m).unwrap_or_else(|| panic!("expected error for {sql}: {m:?}"));
    assert!(
        body.contains(sqlstate),
        "{sql}: want {sqlstate}, got {body:?}"
    );
    assert_eq!(status_of(&m), status, "{sql}");
    assert_no_foreign_identifiers(&body);
}

fn assert_no_foreign_identifiers(body: &str) {
    for needle in ["tenant-a", "tenant-b", "alice", "bob"] {
        assert!(!body.contains(needle), "error leaks {needle:?}: {body:?}");
    }
}

/// `SELECT id` の結果（id の文字列列）。状態バイトも検証する。
fn ids(s: &mut TcpStream, status: u8) -> Vec<String> {
    select_first_column(s, SELECT, status)
}

fn select_first_column(s: &mut TcpStream, sql: &str, status: u8) -> Vec<String> {
    let m = run(s, sql);
    assert_eq!(error_of(&m), None);
    assert_eq!(status_of(&m), status);
    m.iter()
        .filter(|(t, _)| *t == b'D')
        .map(|(_, b)| {
            let len = i32::from_be_bytes([b[2], b[3], b[4], b[5]]) as usize;
            String::from_utf8_lossy(&b[6..6 + len]).into_owned()
        })
        .collect()
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn connect(addr: SocketAddr) -> (TcpStream, TcpStream) {
    (
        authenticate_to_ready_for_query(addr, "alice", "correct-horse"),
        authenticate_to_ready_for_query(addr, "bob", "battery-staple"),
    )
}

/// bob が `BEGIN` + `INSERT`（id 900）で writer を握る。
fn hold_writer(b: &mut TcpStream) {
    ok(b, "BEGIN", "BEGIN", b'T');
    ok(b, &insert(900, "err7-holder"), "INSERT 0 1", b'T');
}

/// autocommit の `55P03` は行・台帳・採番状態を変えず、同じ `operation_id` の再試行が 1 回だけ反映される。
#[test]
fn autocommit_55p03_leaves_no_side_effects_and_same_operation_id_retry_applies_once() {
    let (core, _g) = new_core("err7-t1");
    let users = write_user_store_file(&USERS);
    let addr = spawn_server_with_engine_ddl_users(&users, Arc::clone(&core), &["alice"]);
    let (mut a, mut b) = connect(addr);
    ok(&mut a, "CREATE SEQUENCE err7_s", "CREATE SEQUENCE", b'I');
    ok(
        &mut a,
        "CREATE TABLE err7_n (n BIGINT)",
        "CREATE TABLE",
        b'I',
    );
    let sel_n = "SELECT n FROM err7_n ORDER BY n LIMIT 100";

    hold_writer(&mut b);
    let seq_insert =
        "INSERT INTO err7_n (n) VALUES (nextval('err7_s')) USING OPERATION_ID 'err7-a-seq'";
    assert!(ids(&mut a, b'I').is_empty());
    assert_eq!(ids(&mut b, b'T'), strs(&["900"]));
    assert!(select_first_column(&mut a, sel_n, b'I').is_empty());

    err(&mut a, &insert(10, "err7-a-10"), "55P03", b'I');
    err(&mut a, seq_insert, "55P03", b'I');
    assert!(ids(&mut a, b'I').is_empty());
    assert_eq!(ids(&mut b, b'T'), strs(&["900"]));

    ok(&mut b, "ROLLBACK", "ROLLBACK", b'I');
    ok(&mut a, &insert(10, "err7-a-10"), "INSERT 0 1", b'I');
    // 採番が失敗文で進んでいなければ最初の値 1 が使われる。台帳に残っていれば反映されない。
    ok(&mut a, seq_insert, "INSERT 0 1", b'I');
    assert_eq!(ids(&mut a, b'I'), strs(&["10"]));
    assert_eq!(select_first_column(&mut a, sel_n, b'I'), strs(&["1"]));
    assert!(ids(&mut b, b'I').is_empty());
}

/// autocommit の DDL の `55P03` はカタログに何も残さず、解放後の同じ DDL が成功する。
#[test]
fn autocommit_ddl_55p03_leaves_no_catalog_entry_and_retry_succeeds() {
    let (core, _g) = new_core("err7-t2");
    let users = write_user_store_file(&USERS);
    let addr = spawn_server_with_engine_ddl_users(&users, Arc::clone(&core), &["alice"]);
    let (mut a, mut b) = connect(addr);

    hold_writer(&mut b);
    err(&mut a, "CREATE TABLE err7_new (n BIGINT)", "55P03", b'I');
    err(&mut a, "SELECT n FROM err7_new LIMIT 1", "42P01", b'I');

    ok(&mut b, "ROLLBACK", "ROLLBACK", b'I');
    ok(
        &mut a,
        "CREATE TABLE err7_new (n BIGINT)",
        "CREATE TABLE",
        b'I',
    );
    ok(
        &mut a,
        "INSERT INTO err7_new (n) VALUES (5) USING OPERATION_ID 'err7-ddl-1'",
        "INSERT 0 1",
        b'I',
    );
}

/// 明示トランザクションの最初の書き込み文の `55P03` → 失敗状態の文は `25P02` → `ROLLBACK` →
/// `BEGIN` からの全体再試行（同じ `operation_id`）が 1 回だけ反映される。
#[test]
fn explicit_txn_first_write_55p03_then_rollback_and_full_retry_commits_once() {
    let (core, _g) = new_core("err7-t3");
    let users = write_user_store_file(&USERS);
    let addr = spawn_server_with_engine(&users, Arc::clone(&core));
    let (mut a, mut b) = connect(addr);

    hold_writer(&mut b);
    ok(&mut a, "BEGIN", "BEGIN", b'T');
    assert!(ids(&mut a, b'T').is_empty());
    err(&mut a, &insert(1, "err7-t-1"), "55P03", b'E');
    err(&mut a, &insert(2, "err7-t-2"), "25P02", b'E');
    err(&mut a, SELECT, "25P02", b'E');
    err(&mut a, "COMMIT", "25P02", b'E');
    ok(&mut a, "ROLLBACK", "ROLLBACK", b'I');
    assert!(ids(&mut a, b'I').is_empty());
    assert_eq!(ids(&mut b, b'T'), strs(&["900"]));

    ok(&mut b, "ROLLBACK", "ROLLBACK", b'I');
    ok(&mut a, "BEGIN", "BEGIN", b'T');
    ok(&mut a, &insert(1, "err7-t-1"), "INSERT 0 1", b'T');
    ok(&mut a, &insert(2, "err7-t-2"), "INSERT 0 1", b'T');
    ok(&mut a, "COMMIT", "COMMIT", b'I');
    assert_eq!(ids(&mut a, b'I'), strs(&["1", "2"]));
    assert!(ids(&mut b, b'I').is_empty());
}

/// 書き込み後の実行時エラー（同一主キーの重複）で失敗状態になったトランザクションへの文は
/// `25P02`、`ROLLBACK` で失敗前の書き込みも含め何も残らず、`BEGIN` からの全体再試行が 1 回ずつ
/// 反映される。元の原因（`23505`）自体は再試行してよいコードではなく、本ケースは `25P02` で
/// 拒否された文と `ROLLBACK` されたトランザクションが何も残さないことの確認である。
#[test]
fn failed_txn_25p02_rolls_back_prior_writes_and_full_retry_commits_once() {
    let (core, _g) = new_core("err7-t4");
    let users = write_user_store_file(&USERS);
    let addr = spawn_server_with_engine(&users, Arc::clone(&core));
    let (mut a, _b) = connect(addr);

    ok(&mut a, "BEGIN", "BEGIN", b'T');
    ok(&mut a, &insert(1, "err7-f-1"), "INSERT 0 1", b'T');
    err(&mut a, &insert(1, "err7-f-dup"), "23505", b'E');
    err(&mut a, &insert(3, "err7-f-3"), "25P02", b'E');
    ok(&mut a, "ROLLBACK", "ROLLBACK", b'I');
    assert!(ids(&mut a, b'I').is_empty());

    ok(&mut a, "BEGIN", "BEGIN", b'T');
    ok(&mut a, &insert(1, "err7-f-1"), "INSERT 0 1", b'T');
    ok(&mut a, &insert(3, "err7-f-3"), "INSERT 0 1", b'T');
    ok(&mut a, "COMMIT", "COMMIT", b'I');
    assert_eq!(ids(&mut a, b'I'), strs(&["1", "3"]));
}

/// 全体の接続数上限の `53300` は接続を確立させず（`ReadyForQuery` なし・枠を消費せず）、保持中の
/// 接続の観測可能な状態を変えない。枠が空いた後の再接続は認証まで通り、書き込みが 1 回だけ反映される。
#[test]
fn max_connections_53300_has_no_side_effects_and_reconnect_after_interval_succeeds() {
    let (core, _g) = new_core("err7-t5");
    let users = write_user_store_file(&USERS);
    let (addr, limiter) = spawn_server_with_engine_and_limits(
        &users,
        Arc::clone(&core),
        2,
        Duration::from_secs(5),
        SessionLimits::default(),
    );
    let mut a = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut b = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    ok(&mut b, &insert(900, "err7-b-900"), "INSERT 0 1", b'I');
    wait_for_active_exact(&limiter, 2, Duration::from_secs(5));
    assert!(ids(&mut a, b'I').is_empty());

    let mut extra = TcpStream::connect(addr).expect("connect over the limit");
    extra
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("set read timeout");
    expect_error_response_with_sqlstate(&mut extra, SQLSTATE_TOO_MANY_CONNECTIONS);
    expect_connection_closed(&mut extra);
    assert_eq!(limiter.active(), 2, "rejection must not consume a permit");
    assert!(ids(&mut a, b'I').is_empty());
    assert_eq!(ids(&mut b, b'I'), strs(&["900"]));

    // bob の接続を閉じて枠の回収を待ってから（間隔を空けて）再接続する。
    drop(b);
    wait_for_active_exact(&limiter, 1, Duration::from_secs(5));
    let mut a2 = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    ok(&mut a2, &insert(1, "err7-c-1"), "INSERT 0 1", b'I');
    assert_eq!(ids(&mut a, b'I'), strs(&["1"]));
}
