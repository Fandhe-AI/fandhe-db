//! `UPDATE <table> SET <col> = <lit>[, ...] WHERE id = <n>
//! USING OPERATION_ID '<id>'`（Issue #865、対象ビヘイビア: SQL-17・TASK-191）の
//! 簡易クエリプロトコル経由（生バイトクライアント）検証。ポインタ:
//! `docs/spec/05-tasks.md` TASK-191・`docs/spec/04-behavior/sql-surface.md`
//! SQL-17・`docs/spec/04-behavior/recovery.md` RECOVER-1・RECOVER-10・
//! `docs/spec/04-behavior/rls.md` RLS-7・RLS-9・RLS-11。
//!
//! `operation_id` の意味論（必須化・台帳・重複拒否・再送判定・内容照合）・
//! 0 行更新の同一性・read-merge-write の契約そのものは
//! `crates/engine/tests/sql_update_single_row.rs` が確定オラクルとして検証
//! 済みのため、本ファイルは同じ規則が **wire フレーミング** 越しに観測できる
//! ことの確認に徹する（`wire_insert_operation_id.rs`・`wire_hint_order.rs` と
//! 同じ流儀）。

use crate::common;

use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-update-single-row-docs");
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

fn spawn_two_tenants(core: Arc<EngineCore>) -> (std::net::TcpStream, std::net::TcpStream) {
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);
    let alice = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    let bob = authenticate_to_ready_for_query(addr, "bob", "pw-bob");
    (alice, bob)
}

fn insert_sql(id: u64, lang: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[0.1,0.2,0.3]', '{lang}') USING OPERATION_ID '{op_id}'"
    )
}

fn update_lang_sql(id: u64, lang: &str, op_id: &str) -> String {
    format!("UPDATE docs SET lang = '{lang}' WHERE id = {id} USING OPERATION_ID '{op_id}'")
}

/// 1 行更新は pg 互換の `CommandComplete` タグ `UPDATE 1` を返す。
#[test]
fn wire_update_single_row_returns_command_complete_update_1() {
    let (core, _guard) = new_core_with_docs_table();
    let (mut alice, _bob) = spawn_two_tenants(core);

    send_simple_query(&mut alice, &insert_sql(1, "ja", "wire-op-seed"));
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    read_ready_for_query(&mut alice);

    send_simple_query(&mut alice, &update_lang_sql(1, "en", "wire-op-update"));
    assert_eq!(read_command_complete(&mut alice), "UPDATE 1");
    read_ready_for_query(&mut alice);
}

/// 他テナント保持 id・未存在 id はいずれも `CommandComplete("UPDATE 0")` を
/// 返し、wire フレーミング越しに他テナントの存在情報を漏らさない
/// （RLS-9・security.md P0）。
#[test]
fn wire_update_zero_rows_is_identical_for_other_tenant_row_and_nonexistent_id() {
    let (core, _guard) = new_core_with_docs_table();
    let (mut alice, mut bob) = spawn_two_tenants(core);

    // bob が自テナントの行を持つ。
    send_simple_query(&mut bob, &insert_sql(1, "ja", "wire-op-bob-seed"));
    assert_eq!(read_command_complete(&mut bob), "INSERT 0 1");
    read_ready_for_query(&mut bob);

    // alice から bob の id=1 へ UPDATE。
    send_simple_query(
        &mut alice,
        &update_lang_sql(1, "en", "wire-op-other-tenant"),
    );
    assert_eq!(read_command_complete(&mut alice), "UPDATE 0");
    read_ready_for_query(&mut alice);

    // alice から未存在 id=999 へ UPDATE。応答は同一（`UPDATE 0`）。
    send_simple_query(
        &mut alice,
        &update_lang_sql(999, "en", "wire-op-nonexistent"),
    );
    assert_eq!(read_command_complete(&mut alice), "UPDATE 0");
    read_ready_for_query(&mut alice);

    // bob の行は無傷のまま（真の非漏えい確認）。
    send_simple_query(&mut bob, "SELECT lang FROM docs WHERE id = 1 LIMIT 10");
    read_row_description(&mut bob);
    let row = read_data_row(&mut bob);
    read_command_complete(&mut bob);
    assert_eq!(row, vec![Some("ja".to_string())]);
    read_ready_for_query(&mut bob);
}

/// `USING OPERATION_ID` 句の省略は `23502` で拒否され、接続は維持される。
#[test]
fn wire_update_missing_operation_id_clause_is_rejected_with_23502() {
    let (core, _guard) = new_core_with_docs_table();
    let (mut alice, _bob) = spawn_two_tenants(core);

    send_simple_query(&mut alice, &insert_sql(1, "ja", "wire-op-seed-missing"));
    read_command_complete(&mut alice);
    read_ready_for_query(&mut alice);

    send_simple_query(&mut alice, "UPDATE docs SET lang = 'en' WHERE id = 1");
    expect_error_response_with_sqlstate(&mut alice, "23502");
    read_ready_for_query(&mut alice);

    // 接続は維持され、続く正規の UPDATE が成功すること。
    send_simple_query(
        &mut alice,
        &update_lang_sql(1, "en", "wire-op-after-missing"),
    );
    assert_eq!(read_command_complete(&mut alice), "UPDATE 1");
    read_ready_for_query(&mut alice);
}

/// 同一内容の文を同一 `operation_id` で再送すると `23505`（重複拒否）。
#[test]
fn wire_update_resending_the_same_statement_is_rejected_with_23505() {
    let (core, _guard) = new_core_with_docs_table();
    let (mut alice, _bob) = spawn_two_tenants(core);

    send_simple_query(&mut alice, &insert_sql(1, "ja", "wire-op-seed-resend"));
    read_command_complete(&mut alice);
    read_ready_for_query(&mut alice);

    let sql = update_lang_sql(1, "en", "wire-op-resend");
    send_simple_query(&mut alice, &sql);
    assert_eq!(read_command_complete(&mut alice), "UPDATE 1");
    read_ready_for_query(&mut alice);

    send_simple_query(&mut alice, &sql);
    expect_error_response_with_sqlstate(&mut alice, "23505");
    read_ready_for_query(&mut alice);
}

/// 同一 `operation_id` で SET 値が異なる再送は内容不一致 `22023`。
#[test]
fn wire_update_resending_same_operation_id_with_different_value_is_22023() {
    let (core, _guard) = new_core_with_docs_table();
    let (mut alice, _bob) = spawn_two_tenants(core);

    send_simple_query(&mut alice, &insert_sql(1, "ja", "wire-op-seed-mismatch"));
    read_command_complete(&mut alice);
    read_ready_for_query(&mut alice);

    send_simple_query(&mut alice, &update_lang_sql(1, "en", "wire-op-mismatch"));
    assert_eq!(read_command_complete(&mut alice), "UPDATE 1");
    read_ready_for_query(&mut alice);

    send_simple_query(&mut alice, &update_lang_sql(1, "fr", "wire-op-mismatch"));
    expect_error_response_with_sqlstate(&mut alice, "22023");
    read_ready_for_query(&mut alice);
}

/// RLS-11（read-your-writes）: 同一 wire セッションで INSERT → UPDATE → SELECT
/// を発行すると、SELECT は更新後の値を返す。
#[test]
fn wire_update_read_your_writes_within_the_same_session() {
    let (core, _guard) = new_core_with_docs_table();
    let (mut alice, _bob) = spawn_two_tenants(core);

    send_simple_query(&mut alice, &insert_sql(1, "ja", "wire-op-rls11-insert"));
    assert_eq!(read_command_complete(&mut alice), "INSERT 0 1");
    read_ready_for_query(&mut alice);

    send_simple_query(
        &mut alice,
        &update_lang_sql(1, "en", "wire-op-rls11-update"),
    );
    assert_eq!(read_command_complete(&mut alice), "UPDATE 1");
    read_ready_for_query(&mut alice);

    send_simple_query(&mut alice, "SELECT lang FROM docs WHERE id = 1 LIMIT 10");
    read_row_description(&mut alice);
    let row = read_data_row(&mut alice);
    read_command_complete(&mut alice);
    assert_eq!(row, vec![Some("en".to_string())]);
    read_ready_for_query(&mut alice);
}
