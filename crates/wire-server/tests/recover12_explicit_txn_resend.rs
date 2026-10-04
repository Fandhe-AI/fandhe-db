//! 明示トランザクションの成否確定手順（RECOVER-12・Issue #1354）の pg wire 層 A 検証。
//! ポインタ: `docs/spec/04-behavior/` の RECOVER-12・RECOVER-10・ERR-2・SQL-31・WIRE-19。
//!
//! `COMMIT` の応答を受け取れなかったクライアントが、新しい接続の `BEGIN` の内側で先頭文を
//! 同じ `operation_id` で再送して成否を確定する手順を、生バイトの wire クライアントで固定する。
//! 台帳由来の `23505`（commit 済みの根拠）と行制約由来の `23505`（根拠にならない）は
//! `ErrorResponse` の `C` が同じため、`M`（固定文言。engine の `client_message()`）で区別する
//! ——pg wire の `ErrorResponse` は ERR-1 の既存形式（`S`/`C`/`M`）で、`code` ラベルを運ばない。
//! 3 クライアントの層 B（`three_client_e2e.rs`）は `#[ignore]` のため、この区別を `make ci` で
//! 常時守るのは本ファイルだけである。engine 側の順序保証は
//! `crates/engine/tests/recover12_explicit_txn_resend.rs` が担う。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use common::*;

/// 台帳由来の重複（commit 済み再送）の固定文言。
const LEDGER_MESSAGE: &str = "operation_id already recorded with the same content";
/// 行制約（`PRIMARY KEY`／`UNIQUE`）由来の固定文言。
const ROW_CONSTRAINT_MESSAGE: &str = "unique constraint violation";

/// `orders (code TEXT PRIMARY KEY, sku TEXT UNIQUE)` を持つ一時 DB と engine を用意する。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("recover12-explicit-txn-resend-wire");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    session.allow_ddl();
    let sys = PolicyContext::with_visibilities("sys", [Visibility::Public, Visibility::Private])
        .expect("tenant");
    core.execute_sql_in_session(
        &sys,
        &mut session,
        "CREATE TABLE orders (code TEXT PRIMARY KEY, sku TEXT UNIQUE)",
    )
    .expect("create table");
    (Arc::new(core), guard)
}

fn ins(id: u64, code: &str, sku: &str, op: &str) -> String {
    format!(
        "INSERT INTO orders (id, code, sku) VALUES ({id}, '{code}', '{sku}') \
         USING OPERATION_ID '{op}'"
    )
}

/// 簡易クエリで 1 文を送り、成功（`CommandComplete` が `expected_tag`）を確認する。
fn exec_ok(stream: &mut TcpStream, sql: &str, expected_tag: &str) {
    send_simple_query(stream, sql);
    assert_eq!(read_command_complete(stream), expected_tag, "{sql}");
    read_ready_for_query(stream);
}

/// 簡易クエリで 1 文を送り、`23505` と固定文言 `message` で拒否され、`ReadyForQuery` が
/// `Failed`（`'E'`）になることを確認する。
fn exec_rejected_23505(stream: &mut TcpStream, sql: &str, message: &str) {
    send_simple_query(stream, sql);
    expect_error_response_with_sqlstate_and_message(stream, "23505", message);
    assert_eq!(read_ready_for_query_status(stream), b'E', "{sql}");
}

fn rollback(stream: &mut TcpStream) {
    send_simple_query(stream, "ROLLBACK");
    assert_eq!(read_command_complete(stream), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(stream), b'I');
}

fn connect(addr: std::net::SocketAddr) -> TcpStream {
    authenticate_to_ready_for_query(addr, "alice", "correct-horse")
}

fn visible_rows(core: &EngineCore, tenant: &str) -> usize {
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("tenant");
    core.execute_sql(&ctx, "SELECT id FROM orders LIMIT 100")
        .expect("scan")
        .rows
        .len()
}

/// commit 済みトランザクションの再送は台帳由来の文言、台帳に当たらない値衝突・行 id 衝突は
/// 行制約由来の文言で拒否される（`C` は同じ `23505`、`M` で区別できる）。
#[test]
fn resend_of_committed_transaction_is_distinguishable_from_row_constraint_conflict() {
    let (core, _guard) = new_core();
    let users = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users, Arc::clone(&core));

    // 接続 1: COMMIT まで完了する（応答を失ったクライアントを模す）。
    {
        let mut s = connect(addr);
        exec_ok(&mut s, "BEGIN", "BEGIN");
        exec_ok(&mut s, &ins(1, "c1", "s1", "tx-1"), "INSERT 0 1");
        exec_ok(&mut s, &ins(2, "c2", "s2", "tx-2"), "INSERT 0 1");
        exec_ok(&mut s, "COMMIT", "COMMIT");
    }
    assert_eq!(visible_rows(&core, "tenant-a"), 2);

    // 接続 2: 新しい BEGIN で先頭文を再送 → 台帳由来（commit 済みの根拠）。
    let mut s = connect(addr);
    exec_ok(&mut s, "BEGIN", "BEGIN");
    exec_rejected_23505(&mut s, &ins(1, "c1", "s1", "tx-1"), LEDGER_MESSAGE);
    rollback(&mut s);

    // 対照: 制約列の値が衝突するだけで台帳に当たらない文は行制約由来。
    exec_ok(&mut s, "BEGIN", "BEGIN");
    exec_rejected_23505(
        &mut s,
        &ins(3, "c1", "s3", "other-3"),
        ROW_CONSTRAINT_MESSAGE,
    );
    rollback(&mut s);
    exec_ok(&mut s, "BEGIN", "BEGIN");
    exec_rejected_23505(
        &mut s,
        &ins(4, "c4", "s2", "other-4"),
        ROW_CONSTRAINT_MESSAGE,
    );
    rollback(&mut s);

    assert_eq!(visible_rows(&core, "tenant-a"), 2, "nothing was applied");
}

/// `COMMIT` 前に接続が切れたトランザクションは何も残さず、新しい接続での再送は成功する。
/// 再送が別の確定済み行と衝突する場合は行制約由来で、commit 済みとは判定されない。
#[test]
fn resend_after_connection_drop_before_commit_is_not_a_commit_proof() {
    let (core, _guard) = new_core();
    let users = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users, Arc::clone(&core));

    {
        let mut s = connect(addr);
        exec_ok(&mut s, "BEGIN", "BEGIN");
        exec_ok(&mut s, &ins(5, "e", "se", "r-1"), "INSERT 0 1");
        // COMMIT を送らずに接続を閉じる。
    }

    // 別経路（autocommit）が、別 id・同じ制約列の値を確定させる。サーバーが切断を検知して
    // 書き込みトランザクションを解放するまで待機上限内で待つため、ここで成功すれば解放済み。
    {
        let mut s = connect(addr);
        exec_ok(&mut s, &ins(6, "e", "se", "other-6"), "INSERT 0 1");
    }

    let mut s = connect(addr);
    exec_ok(&mut s, "BEGIN", "BEGIN");
    exec_rejected_23505(&mut s, &ins(5, "e", "se", "r-1"), ROW_CONSTRAINT_MESSAGE);
    rollback(&mut s);

    // 衝突を解消すると、同内容の再送が通常成功する（台帳に未記録だった証跡）。
    exec_ok(
        &mut s,
        "DELETE FROM orders WHERE id = 6 USING OPERATION_ID 'del-6'",
        "DELETE 1",
    );
    exec_ok(&mut s, "BEGIN", "BEGIN");
    exec_ok(&mut s, &ins(5, "e", "se", "r-1"), "INSERT 0 1");
    exec_ok(&mut s, "COMMIT", "COMMIT");
    assert_eq!(visible_rows(&core, "tenant-a"), 1);

    // 回復後に同じ文を再送すると台帳由来になる。
    exec_ok(&mut s, "BEGIN", "BEGIN");
    exec_rejected_23505(&mut s, &ins(5, "e", "se", "r-1"), LEDGER_MESSAGE);
    rollback(&mut s);
}
