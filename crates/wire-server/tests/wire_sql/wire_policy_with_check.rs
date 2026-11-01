//! 利用者定義ポリシーの `WITH CHECK` の書き込みへの適用（RLS-13・Issue #2277）の pg wire 結合テスト。
//! 簡易クエリ（'Q'）で `CREATE POLICY` → `ENABLE` → 違反する `INSERT` を送り、ErrorResponse の
//! SQLSTATE が `42501` になることと、違反後も接続が使え行が増えていないことを固定する。合成・原子性・
//! ON CONFLICT の契約は engine の `tests/table_rls/rls13_policy_with_check.rs` が確定オラクル。

use crate::common;

use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn connect() -> (TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-policy-with-check");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "t",
            vec![ColumnDef::new("a", ColumnType::Text, true)],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine_ddl_users(&users_path, core, &["alice"]);
    let stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    (stream, guard)
}

#[test]
fn simple_query_insert_violating_with_check_is_42501_and_connection_survives() {
    let (mut s, _g) = connect();
    for (sql, tag) in [
        ("ALTER TABLE t ENABLE ROW LEVEL SECURITY", "ALTER TABLE"),
        ("ALTER TABLE t FORCE ROW LEVEL SECURITY", "ALTER TABLE"),
        (
            "CREATE POLICY p ON t FOR INSERT WITH CHECK (a = 'ok')",
            "CREATE POLICY",
        ),
        (
            "INSERT INTO t (id, a) VALUES (1, 'ok') USING OPERATION_ID 'w1'",
            "INSERT 0 1",
        ),
    ] {
        send_simple_query(&mut s, sql);
        assert_eq!(read_command_complete(&mut s), tag, "{sql}");
        assert_eq!(read_ready_for_query_status(&mut s), b'I');
    }
    send_simple_query(
        &mut s,
        "INSERT INTO t (id, a) VALUES (2, 'secret-value') USING OPERATION_ID 'w2'",
    );
    expect_error_response_with_sqlstate(&mut s, "42501");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
    // 違反後も同じ接続で書き込める（文単位の失敗で、接続は壊れない）。
    send_simple_query(
        &mut s,
        "INSERT INTO t (id, a) VALUES (3, 'ok') USING OPERATION_ID 'w3'",
    );
    assert_eq!(read_command_complete(&mut s), "INSERT 0 1");
    assert_eq!(read_ready_for_query_status(&mut s), b'I');
}
