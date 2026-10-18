//! `DELETE FROM <table> WHERE id = <n> USING OPERATION_ID '<id>'`（SQL-18、
//! TASK-191、#867）の簡易クエリプロトコル経由（生バイトクライアント）検証
//! （層 A。ポインタ: `docs/spec/05-tasks.md` TASK-191・
//! `docs/spec/04-behavior/sql-surface.md` SQL-18・
//! `docs/spec/04-behavior/recovery.md` RECOVER-1・RECOVER-10）。
//!
//! `operation_id` の意味論（必須化・台帳照合・再送判定・0 行成功への写像）
//! そのものは `crates/engine/tests/sql_delete_single_row.rs` が確定オラクル
//! として検証済みのため、本ファイルは同じ規則が **wire フレーミング** 越しに
//! 観測できることの確認に徹する（`wire_insert_operation_id.rs` と同じ流儀）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::Read;
use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-delete-single-row-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
    (Arc::new(core), guard)
}

fn spawn_with_users(
    core: Arc<EngineCore>,
    users: &[(&str, &str, &str)],
) -> Vec<std::net::SocketAddr> {
    let users_path = write_user_store_file(users);
    vec![spawn_server_with_engine(&users_path, core)]
}

fn connect_as(addr: std::net::SocketAddr, username: &str, password: &str) -> TcpStream {
    authenticate_to_ready_for_query(addr, username, password)
}

fn insert_sql(id: u64, lang: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[0.1,0.2,0.3]', '{lang}') USING OPERATION_ID '{op_id}'"
    )
}

fn delete_sql(id: u64, op_id: &str) -> String {
    format!("DELETE FROM docs WHERE id = {id} USING OPERATION_ID '{op_id}'")
}

/// 生バイト列そのままを 1 メッセージぶん読み取る（`wire_tenant_row_id_scope.rs`
/// と同じヘルパー。応答の完全一致比較に使う）。
fn read_raw_message(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");

    let mut out = Vec::with_capacity(1 + len);
    out.push(header[0]);
    out.extend_from_slice(&len_buf);
    out.extend_from_slice(&body);
    out
}

/// 基本受理: `CommandComplete` タグが `DELETE <rows_affected>`（pg 規範）へ
/// 整形される。自テナント行の削除は `DELETE 1`。
#[test]
fn wire_delete_command_complete_tag_reflects_rows_affected_on_success() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    send_simple_query(&mut stream, &insert_sql(1, "ja", "wire-delete-seed-1"));
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, &delete_sql(1, "wire-delete-op-1"));
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "DELETE 1");
    read_ready_for_query(&mut stream);
}

/// 0 行 DELETE（未存在 id）は `DELETE 0` として成功する（エラーではない）。
#[test]
fn wire_delete_nonexistent_id_succeeds_with_delete_zero_tag() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    send_simple_query(&mut stream, &delete_sql(999, "wire-delete-op-missing"));
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "DELETE 0");
    read_ready_for_query(&mut stream);
}

/// `USING OPERATION_ID` 句の省略は書き込みトランザクション開始前に `23502`
/// で拒否され、接続は維持される（対象行は未削除のまま）。
#[test]
fn wire_delete_missing_operation_id_clause_is_rejected_with_23502() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    send_simple_query(&mut stream, &insert_sql(1, "ja", "wire-delete-seed-2"));
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DELETE FROM docs WHERE id = 1");
    expect_error_response_with_sqlstate(&mut stream, "23502");
    read_ready_for_query(&mut stream);

    // 接続は維持され、続く正規の DELETE が成功すること。
    send_simple_query(&mut stream, &delete_sql(1, "wire-delete-op-after-missing"));
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "DELETE 1");
    read_ready_for_query(&mut stream);
}

/// 同一 `operation_id`（同一 id 対象）の再送は `23505`（重複拒否）で拒否
/// される。接続は維持される。
#[test]
fn wire_delete_resending_the_same_operation_id_is_rejected_with_23505() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    send_simple_query(&mut stream, &insert_sql(1, "ja", "wire-delete-seed-3"));
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    let sql = delete_sql(1, "wire-delete-op-resend");
    send_simple_query(&mut stream, &sql);
    assert_eq!(read_command_complete(&mut stream), "DELETE 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, &sql);
    expect_error_response_with_sqlstate(&mut stream, "23505");
    read_ready_for_query(&mut stream);
}

/// RLS-9: 他テナント（bob）保持 id への DELETE と、どのテナントも保持しない
/// id への DELETE とで、wire 応答（`CommandComplete` の生バイト列）が完全に
/// 一致すること（存在情報の非漏えい。`wire_tenant_row_id_scope.rs` と同じ
/// 検証方式）。
#[test]
fn wire_delete_response_is_byte_identical_for_other_tenant_row_and_nonexistent_id() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(
        core,
        &[
            ("alice", "tenant-a", "pw-alice"),
            ("bob", "tenant-b", "pw-bob"),
        ],
    );

    // bob が id=42 を保持する。
    let mut bob_stream = connect_as(addrs[0], "bob", "pw-bob");
    send_simple_query(
        &mut bob_stream,
        &insert_sql(42, "en", "wire-delete-bob-seed"),
    );
    assert_eq!(read_command_complete(&mut bob_stream), "INSERT 0 1");
    read_ready_for_query(&mut bob_stream);

    // alice から他テナント保持 id（42）への DELETE。
    let mut alice_stream_a = connect_as(addrs[0], "alice", "pw-alice");
    send_simple_query(
        &mut alice_stream_a,
        &delete_sql(42, "wire-delete-op-other-tenant"),
    );
    let response_other_tenant = read_raw_message(&mut alice_stream_a);
    read_ready_for_query(&mut alice_stream_a);

    // alice から未存在 id（4242）への DELETE。
    let mut alice_stream_b = connect_as(addrs[0], "alice", "pw-alice");
    send_simple_query(
        &mut alice_stream_b,
        &delete_sql(4242, "wire-delete-op-nonexistent"),
    );
    let response_nonexistent = read_raw_message(&mut alice_stream_b);
    read_ready_for_query(&mut alice_stream_b);

    assert_eq!(response_other_tenant, response_nonexistent);
    // 応答内容自体は `DELETE 0`（成功）であることも確認する。
    assert!(std::str::from_utf8(&response_other_tenant)
        .unwrap_or("")
        .contains("DELETE 0"));

    // bob の行は無傷のまま。
    let mut bob_check = connect_as(addrs[0], "bob", "pw-bob");
    send_simple_query(&mut bob_check, "SELECT COUNT(*) FROM docs");
    let _cols = read_row_description(&mut bob_check);
    let row = read_data_row(&mut bob_check);
    assert_eq!(row, vec![Some("1".to_string())]);
    read_command_complete(&mut bob_check);
    read_ready_for_query(&mut bob_check);
}

/// RLS-11（TASK-195・read-your-writes）: 同一 wire セッション内で
/// `INSERT` → `DELETE` → `SELECT` を行うと、削除した行は自テナントの
/// `SELECT` からも消える。
#[test]
fn wire_insert_delete_select_removes_row_within_same_session() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    send_simple_query(&mut stream, &insert_sql(1, "ja", "wire-rls11-seed"));
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, &delete_sql(1, "wire-rls11-delete"));
    assert_eq!(read_command_complete(&mut stream), "DELETE 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 20");
    let _cols = read_row_description(&mut stream);
    // 削除済みのため行は 0 件（`CommandComplete` が直後に来る）。
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 0");
    read_ready_for_query(&mut stream);
}
