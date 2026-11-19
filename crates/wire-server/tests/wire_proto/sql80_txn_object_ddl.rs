//! 明示・暗黙トランザクション内の VIEW・SEQUENCE・SCHEMA・ENUM 型の DDL（SQL-80・SQL-31・WIRE-16 の
//! ポインタ、Issue #3315。設計記録: `docs/design/explicit-transaction.md`「明示トランザクション内の DDL の
//! 方式」・`docs/design/wire-multi-statement.md`）を、生バイトの wire クライアント越しに検証する結合テスト。
//!
//! psql の `-f` と同じく文末に `;` を付けて 1 メッセージに 1 文ずつ送り、COMMIT で原子的に確定し、
//! ROLLBACK・接続断・後続の失敗では何も残らないことを固定する。`sql80_txn_table_ddl.rs` と同じ流儀。

use crate::common;
use crate::temp_db;

use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("sql80-txn-object-ddl-wire");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

/// DDL 権限を持つユーザー `alice`（`--ddl-allowed-users` 相当）で接続する。
fn connect(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine_ddl_users(&users_path, core, &["alice"]);
    authenticate_to_ready_for_query(addr, "alice", "correct-horse")
}

fn expect_tag(stream: &mut std::net::TcpStream, sql: &str, tag: &str, status: u8) {
    send_simple_query(stream, sql);
    assert_eq!(read_command_complete(stream), tag, "{sql}");
    assert_eq!(read_ready_for_query_status(stream), status, "{sql}");
}

fn expect_error(stream: &mut std::net::TcpStream, sql: &str, sqlstate: &str, status: u8) {
    send_simple_query(stream, sql);
    expect_error_response_with_sqlstate(stream, sqlstate);
    assert_eq!(read_ready_for_query_status(stream), status, "{sql}");
}

#[test]
fn psql_style_object_ddl_commits_atomically() {
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, "CREATE SEQUENCE q;", "CREATE SEQUENCE", b'T');
    expect_tag(
        &mut stream,
        "CREATE TYPE mood AS ENUM ('a', 'b');",
        "CREATE TYPE",
        b'T',
    );
    expect_tag(&mut stream, "CREATE SCHEMA app;", "CREATE SCHEMA", b'T');
    // SCHEMA の DDL の後は SCHEMA の DDL 以外を拒否する（暫定。受け皿は #3311）ため最後に置く。
    expect_tag(&mut stream, "COMMIT;", "COMMIT", b'I');
    // 確定後は重複作成が拒否される（別トランザクションから見える）。
    expect_error(&mut stream, "CREATE SCHEMA app;", "42P06", b'I');
    expect_error(&mut stream, "CREATE SEQUENCE q;", "42P07", b'I');
    expect_error(
        &mut stream,
        "CREATE TYPE mood AS ENUM ('x');",
        "42710",
        b'I',
    );
}

#[test]
fn psql_style_object_ddl_rolls_back_without_residue() {
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
    expect_tag(&mut stream, "CREATE SEQUENCE q;", "CREATE SEQUENCE", b'T');
    expect_tag(
        &mut stream,
        "CREATE TYPE mood AS ENUM ('a');",
        "CREATE TYPE",
        b'T',
    );
    expect_tag(&mut stream, "CREATE SCHEMA app;", "CREATE SCHEMA", b'T');
    expect_tag(&mut stream, "ROLLBACK;", "ROLLBACK", b'I');
    // 同名の再作成が通る（カタログに残骸が無い）。
    expect_tag(&mut stream, "CREATE SCHEMA app;", "CREATE SCHEMA", b'I');
    expect_tag(&mut stream, "CREATE SEQUENCE q;", "CREATE SEQUENCE", b'I');
    expect_tag(
        &mut stream,
        "CREATE TYPE mood AS ENUM ('a');",
        "CREATE TYPE",
        b'I',
    );
}

#[test]
fn disconnect_inside_an_object_ddl_transaction_leaves_nothing() {
    let (core, _guard) = new_core();
    {
        let mut stream = connect(Arc::clone(&core));
        expect_tag(&mut stream, "BEGIN;", "BEGIN", b'T');
        expect_tag(&mut stream, "CREATE SEQUENCE q;", "CREATE SEQUENCE", b'T');
        expect_tag(&mut stream, "CREATE SCHEMA app;", "CREATE SCHEMA", b'T');
        // COMMIT せずに切断する。
    }
    let mut stream = connect(core);
    // writer の解放を待つため、短いリトライで再作成が通ることを確かめる。
    let mut created = false;
    for _ in 0..50 {
        send_simple_query(&mut stream, "CREATE SEQUENCE q;");
        let (kind, _) = read_message(&mut stream);
        let _ = read_ready_for_query_status_after(&mut stream, kind);
        if kind == b'C' {
            created = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(created, "the aborted transaction must leave nothing behind");
    expect_tag(&mut stream, "CREATE SCHEMA app;", "CREATE SCHEMA", b'I');
}

/// 直前のメッセージが `CommandComplete`（`C`）／`ErrorResponse`（`E`）のとき、続く `ReadyForQuery` まで読み捨てる。
fn read_ready_for_query_status_after(stream: &mut std::net::TcpStream, _kind: u8) -> u8 {
    read_ready_for_query_status(stream)
}

#[test]
fn implicit_transaction_with_object_ddl_commits_as_a_whole() {
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    send_simple_query(
        &mut stream,
        "CREATE SEQUENCE s1; CREATE TYPE mood AS ENUM ('a')",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE SEQUENCE");
    assert_eq!(read_command_complete(&mut stream), "CREATE TYPE");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    expect_error(&mut stream, "CREATE SEQUENCE s1;", "42P07", b'I');
    expect_error(
        &mut stream,
        "CREATE TYPE mood AS ENUM ('x');",
        "42710",
        b'I',
    );
}

#[test]
fn implicit_transaction_failure_rolls_back_the_object_ddl() {
    let (core, _guard) = new_core();
    let mut stream = connect(core);
    // 後続の文が失敗する（存在しない SEQUENCE の削除）と、先行の DDL もロールバックされる。
    send_simple_query(&mut stream, "CREATE SEQUENCE s1; DROP SEQUENCE ghost");
    assert_eq!(read_command_complete(&mut stream), "CREATE SEQUENCE");
    expect_error_response_with_sqlstate(&mut stream, "42P01");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    expect_tag(&mut stream, "CREATE SEQUENCE s1;", "CREATE SEQUENCE", b'I');
}
