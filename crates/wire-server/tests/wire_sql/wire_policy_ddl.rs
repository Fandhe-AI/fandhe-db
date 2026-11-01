//! `CREATE POLICY`／`ALTER POLICY`／`DROP POLICY`（RLS-13・Issue #2274）の簡易クエリプロトコル経由の検証。
//!
//! エラー契約・テナント境界は `crates/engine/tests/table_rls/rls13_policy_ddl.rs`（in-process）が
//! 確定オラクルで、本ファイルは固定コマンドタグ・SQLSTATE が wire フレーミング越しに観測できることの確認に
//! 徹する（`wire_trigger_ddl.rs` と同じ流儀）。

use crate::common;

use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn serve(ddl_user: bool) -> (std::net::SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-policy-ddl");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("owner", ColumnType::Text, true),
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let store = wire_server::auth::UserStore::load_from_file(&users_path).expect("user store");
    let store = if ddl_user {
        store
            .with_ddl_allowed_users(&["alice".to_string()])
            .expect("known user")
    } else {
        store
    };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let limiter = wire_server::limits::ConnectionLimiter::new(16);
    let store = Arc::new(store);
    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            core,
            limiter,
            std::time::Duration::from_secs(5),
        );
    });
    (addr, guard)
}

/// DDL 権限のあるユーザー: 3 文とも固定タグ、重複は `42710`、不在は `42704`。
#[test]
fn wire_policy_ddl_tags_and_sqlstates_for_ddl_user() {
    let (addr, _guard) = serve(true);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "CREATE POLICY p ON docs FOR SELECT USING (owner = 'a')",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE POLICY");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "CREATE POLICY p ON docs");
    expect_error_response_with_sqlstate(&mut stream, "42710");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "ALTER POLICY p ON docs RENAME TO q");
    assert_eq!(read_command_complete(&mut stream), "ALTER POLICY");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "ALTER POLICY p ON docs USING (owner = 'b')");
    expect_error_response_with_sqlstate(&mut stream, "42704");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP POLICY q ON docs");
    assert_eq!(read_command_complete(&mut stream), "DROP POLICY");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP POLICY q ON docs");
    expect_error_response_with_sqlstate(&mut stream, "42704");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP POLICY IF EXISTS q ON docs");
    assert_eq!(read_command_complete(&mut stream), "DROP POLICY");
    read_ready_for_query(&mut stream);
}

/// DDL 権限のないユーザーは対象の有無を問わず `42501`。
#[test]
fn wire_policy_ddl_without_permission_is_42501() {
    let (addr, _guard) = serve(false);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    for sql in [
        "CREATE POLICY p ON nosuch",
        "ALTER POLICY p ON docs RENAME TO q",
        "DROP POLICY IF EXISTS p ON docs",
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "42501");
        read_ready_for_query(&mut stream);
    }
}
