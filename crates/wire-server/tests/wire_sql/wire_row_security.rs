//! `ALTER TABLE ... ENABLE／DISABLE／FORCE／NO FORCE ROW LEVEL SECURITY`（RLS-13・Issue #2275）の
//! pg wire 結合テスト。簡易クエリ（'Q'）で CommandComplete タグ `ALTER TABLE` と、DDL 実行権限の無い
//! 接続の `42501` を固定する。構文・状態遷移・テナント独立の契約は engine の
//! `tests/table_rls/rls13_row_security.rs` が確定オラクル。

use crate::common;

use crate::temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn connect(ddl_allowed: bool) -> (TcpStream, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-row-security");
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
    let allowed: &[&str] = if ddl_allowed { &["alice"] } else { &[] };
    let addr = spawn_server_with_engine_ddl_users(&users_path, core, allowed);
    let stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    (stream, guard)
}

#[test]
fn simple_query_row_security_statements_complete_as_alter_table() {
    let (mut s, _g) = connect(true);
    for sql in [
        "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE t FORCE ROW LEVEL SECURITY",
        "ALTER TABLE t NO FORCE ROW LEVEL SECURITY",
        "ALTER TABLE t DISABLE ROW LEVEL SECURITY",
    ] {
        send_simple_query(&mut s, sql);
        assert_eq!(read_command_complete(&mut s), "ALTER TABLE", "{sql}");
        assert_eq!(read_ready_for_query_status(&mut s), b'I');
    }
}

#[test]
fn simple_query_row_security_without_ddl_permission_is_42501() {
    let (mut s, _g) = connect(false);
    for sql in [
        "ALTER TABLE t ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE nosuch ENABLE ROW LEVEL SECURITY",
    ] {
        send_simple_query(&mut s, sql);
        expect_error_response_with_sqlstate(&mut s, "42501");
        assert_eq!(read_ready_for_query_status(&mut s), b'I');
    }
}
