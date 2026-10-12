//! `--write-lock-wait-ms` の解決結果（`write_lock_wait_opt::resolve`）を注入した
//! `EngineCore` が、wire 越しに書き込みロック待機超過を SQLSTATE `55P03` の
//! `ErrorResponse` として返し、接続が `ReadyForQuery` に戻ること、holder の解放後は
//! 通常どおり書き込めることを固定する（Issue #1554。ポインタ: SQL-46）。
//! `wire942_extended_transaction.rs` と同じ流儀。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

#[test]
fn write_lock_wait_timeout_is_55p03_over_wire_and_recovers_after_release() {
    let path = temp_db::unique_db_path("write-lock-wait-parity");
    let _guard = temp_db::CleanupGuard(path.clone());
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
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut a = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut b = authenticate_to_ready_for_query(addr, "bob", "battery-staple");

    let insert = |id: u32| {
        format!(
            "INSERT INTO documents (id, embedding) VALUES ({id}, '[0.1,0.2,0.3]') USING OPERATION_ID 'op-1554-{id}'"
        )
    };

    // A が BEGIN + INSERT で writer を握る。
    send_simple_query(&mut a, "BEGIN");
    assert_eq!(read_command_complete(&mut a), "BEGIN");
    read_ready_for_query(&mut a);
    send_simple_query(&mut a, &insert(1));
    assert_eq!(read_command_complete(&mut a), "INSERT 0 1");
    read_ready_for_query(&mut a);

    // B の autocommit INSERT は待機超過で 55P03、接続は ReadyForQuery に戻る。
    send_simple_query(&mut b, &insert(2));
    expect_error_response_with_sqlstate(&mut b, "55P03");
    read_ready_for_query(&mut b);

    // A が ROLLBACK した後は B の INSERT が成功する。
    send_simple_query(&mut a, "ROLLBACK");
    assert_eq!(read_command_complete(&mut a), "ROLLBACK");
    read_ready_for_query(&mut a);
    send_simple_query(&mut b, &insert(2));
    assert_eq!(read_command_complete(&mut b), "INSERT 0 1");
    read_ready_for_query(&mut b);
}
