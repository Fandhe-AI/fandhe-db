//! `INSERT INTO <table> (...) VALUES (...) ON CONFLICT (id) DO NOTHING |
//! DO UPDATE SET ... USING OPERATION_ID '<id>'`（SQL-20、TASK-193、
//! Issue #872）の簡易クエリプロトコル経由（生バイトクライアント）検証
//! （層 A。ポインタ: `docs/spec/05-tasks.md` TASK-193・
//! `docs/spec/04-behavior/sql-surface.md` SQL-20・
//! `docs/spec/04-behavior/recovery.md` RECOVER-1・RECOVER-10）。
//!
//! 衝突判定・台帳照合・`rows_affected` の意味論そのものは
//! `crates/engine/tests/sql_upsert.rs` が確定オラクルとして検証済みのため、
//! 本ファイルは同じ規則が **wire フレーミング** 越しに観測できることの確認に
//! 徹する（`wire_delete_single_row.rs`・`wire_insert_operation_id.rs` と
//! 同じ流儀）。

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
    let path = temp_db::unique_db_path("wire-upsert-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
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

fn upsert_do_nothing_sql(id: u64, lang: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[0.9,0.9,0.9]', '{lang}') \
         ON CONFLICT (id) DO NOTHING USING OPERATION_ID '{op_id}'"
    )
}

fn upsert_do_update_sql(id: u64, lang: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[0.9,0.9,0.9]', '{lang}') \
         ON CONFLICT (id) DO UPDATE SET lang = EXCLUDED.lang USING OPERATION_ID '{op_id}'"
    )
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

/// 基本受理: 非衝突 UPSERT は `INSERT 0 1`（新規挿入と同じタグ。
/// `sql::exec::InsertOutcome` を共有するため `SqlOutcome::Insert` の写像は
/// 既存 `INSERT` と同一）。
#[test]
fn wire_upsert_non_conflicting_returns_insert_0_1() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        &upsert_do_nothing_sql(1, "ja", "wire-upsert-op-1"),
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);
}

/// `DO NOTHING` 衝突は `INSERT 0 0`（成功。エラーではない）。
#[test]
fn wire_upsert_do_nothing_conflict_returns_insert_0_0() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    send_simple_query(&mut stream, &insert_sql(1, "ja", "wire-upsert-seed-1"));
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        &upsert_do_nothing_sql(1, "en", "wire-upsert-op-2"),
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 0");
    read_ready_for_query(&mut stream);
}

/// `DO UPDATE` 衝突は `INSERT 0 1`（更新も新規挿入と同じ数え方。
/// `rows_affected = inserted + updated`）。
#[test]
fn wire_upsert_do_update_conflict_returns_insert_0_1() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    send_simple_query(&mut stream, &insert_sql(1, "ja", "wire-upsert-seed-2"));
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        &upsert_do_update_sql(1, "en", "wire-upsert-op-3"),
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);

    // 同一 wire セッション内で更新結果が読み戻せる（RLS-11）。
    send_simple_query(&mut stream, "SELECT lang FROM docs LIMIT 10");
    let _cols = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![Some("en".to_string())]);
    read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// `USING OPERATION_ID` 句の省略は書き込みトランザクション開始前に `23502`
/// で拒否され、接続は維持される。
#[test]
fn wire_upsert_missing_operation_id_clause_is_rejected_with_23502() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja') \
         ON CONFLICT (id) DO NOTHING",
    );
    expect_error_response_with_sqlstate(&mut stream, "23502");
    read_ready_for_query(&mut stream);

    // 接続は維持され、続く正規の UPSERT が成功すること。
    send_simple_query(
        &mut stream,
        &upsert_do_nothing_sql(1, "ja", "wire-upsert-op-after-missing"),
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);
}

/// 同一 `operation_id`（同一内容）の再送は `23505`（重複拒否）で拒否される。
/// 接続は維持される。
#[test]
fn wire_upsert_resending_the_same_operation_id_is_rejected_with_23505() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    let sql = upsert_do_nothing_sql(1, "ja", "wire-upsert-op-resend");
    send_simple_query(&mut stream, &sql);
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, &sql);
    expect_error_response_with_sqlstate(&mut stream, "23505");
    read_ready_for_query(&mut stream);
}

/// RLS-9: 他テナント（bob）保持 id への UPSERT（`DO NOTHING`）と、どのテナントも
/// 保持しない id への UPSERT とで、wire 応答（`CommandComplete` の生バイト列）が
/// 完全に一致すること（存在情報の非漏えい。他テナント保持 id は本構文では常に
/// 「非衝突＝新規挿入」として扱われるため、いずれも `INSERT 0 1` として成功する。
/// `wire_tenant_row_id_scope.rs` と同じ検証方式）。
#[test]
fn wire_upsert_response_is_byte_identical_for_other_tenant_row_and_nonexistent_id() {
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
        &insert_sql(42, "en", "wire-upsert-bob-seed"),
    );
    assert_eq!(read_command_complete(&mut bob_stream), "INSERT 0 1");
    read_ready_for_query(&mut bob_stream);

    // alice から他テナント保持 id（42）への UPSERT。
    let mut alice_stream_a = connect_as(addrs[0], "alice", "pw-alice");
    send_simple_query(
        &mut alice_stream_a,
        &upsert_do_nothing_sql(42, "ja", "wire-upsert-op-other-tenant"),
    );
    let response_other_tenant = read_raw_message(&mut alice_stream_a);
    read_ready_for_query(&mut alice_stream_a);

    // alice から未存在 id（4242）への UPSERT。
    let mut alice_stream_b = connect_as(addrs[0], "alice", "pw-alice");
    send_simple_query(
        &mut alice_stream_b,
        &upsert_do_nothing_sql(4242, "ja", "wire-upsert-op-nonexistent"),
    );
    let response_nonexistent = read_raw_message(&mut alice_stream_b);
    read_ready_for_query(&mut alice_stream_b);

    assert_eq!(response_other_tenant, response_nonexistent);
    assert!(std::str::from_utf8(&response_other_tenant)
        .unwrap_or("")
        .contains("INSERT 0 1"));

    // bob の行は無傷のまま（他テナントの UPSERT による上書きが起きていない）。
    let mut bob_check = connect_as(addrs[0], "bob", "pw-bob");
    send_simple_query(&mut bob_check, "SELECT lang FROM docs LIMIT 10");
    let _cols = read_row_description(&mut bob_check);
    let row = read_data_row(&mut bob_check);
    assert_eq!(row, vec![Some("en".to_string())]);
    read_command_complete(&mut bob_check);
    read_ready_for_query(&mut bob_check);
}

// --- ON CONFLICT の UNIQUE 制約列への拡張（TABLE-16、Issue #1074） --------

fn new_core_with_unique_column_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-upsert-unique-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("a", ColumnType::Text, false)],
        ))
        .expect("create table");
    storage
        .alter_table_add_unique_constraint("docs", &["a"])
        .expect("add UNIQUE(a) constraint");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn insert_a_sql(id: u64, a: &str, op_id: &str) -> String {
    format!("INSERT INTO docs (id, a) VALUES ({id}, '{a}') USING OPERATION_ID '{op_id}'")
}

fn upsert_on_conflict_a_do_nothing_sql(id: u64, a: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO docs (id, a) VALUES ({id}, '{a}') \
         ON CONFLICT (a) DO NOTHING USING OPERATION_ID '{op_id}'"
    )
}

/// RLS-9: UNIQUE 対象（`(id)` ではなく `(a)`）の UPSERT でも、他テナントが同じ
/// 値を保持している場合と、どこにも存在しない場合とで応答が完全に一致する
/// （`wire_upsert_response_is_byte_identical_for_other_tenant_row_and_
/// nonexistent_id` の UNIQUE 対象版。同じ確定オラクル・同じ読み取りヘルパーを
/// 再利用する）。
#[test]
fn wire_upsert_unique_target_response_is_byte_identical_for_other_tenant_row_and_nonexistent_value()
{
    let (core, _guard) = new_core_with_unique_column_table();
    let addrs = spawn_with_users(
        core,
        &[
            ("alice", "tenant-a", "pw-alice"),
            ("bob", "tenant-b", "pw-bob"),
        ],
    );

    // bob が `a='x'` を保持する。
    let mut bob_stream = connect_as(addrs[0], "bob", "pw-bob");
    send_simple_query(
        &mut bob_stream,
        &insert_a_sql(1, "x", "wire-upsert-u-bob-seed"),
    );
    assert_eq!(read_command_complete(&mut bob_stream), "INSERT 0 1");
    read_ready_for_query(&mut bob_stream);

    // alice から他テナント保持値（`a='x'`）への UNIQUE 対象 UPSERT。
    let mut alice_stream_a = connect_as(addrs[0], "alice", "pw-alice");
    send_simple_query(
        &mut alice_stream_a,
        &upsert_on_conflict_a_do_nothing_sql(2, "x", "wire-upsert-u-op-other-tenant"),
    );
    let response_other_tenant = read_raw_message(&mut alice_stream_a);
    read_ready_for_query(&mut alice_stream_a);

    // alice から未存在値（`a='z'`）への UNIQUE 対象 UPSERT。
    let mut alice_stream_b = connect_as(addrs[0], "alice", "pw-alice");
    send_simple_query(
        &mut alice_stream_b,
        &upsert_on_conflict_a_do_nothing_sql(3, "z", "wire-upsert-u-op-nonexistent"),
    );
    let response_nonexistent = read_raw_message(&mut alice_stream_b);
    read_ready_for_query(&mut alice_stream_b);

    assert_eq!(response_other_tenant, response_nonexistent);
    assert!(std::str::from_utf8(&response_other_tenant)
        .unwrap_or("")
        .contains("INSERT 0 1"));

    // bob の行は無傷のまま。
    let mut bob_check = connect_as(addrs[0], "bob", "pw-bob");
    send_simple_query(&mut bob_check, "SELECT a FROM docs LIMIT 10");
    let _cols = read_row_description(&mut bob_check);
    let row = read_data_row(&mut bob_check);
    assert_eq!(row, vec![Some("x".to_string())]);
    read_command_complete(&mut bob_check);
    read_ready_for_query(&mut bob_check);
}
