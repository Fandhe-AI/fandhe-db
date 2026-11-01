//! `CREATE ROLE`／`CREATE USER`／`DROP ROLE`（SQL-39 のポインタ、Issue #1962）の簡易クエリプロトコル経由の検証。
//!
//! エラー契約・テナント境界・パスワードの非漏えいは `crates/engine/tests/table_rls/sql39_role_ddl.rs`
//! （in-process）が確定オラクルで、本ファイルは固定コマンドタグ・SQLSTATE が wire フレーミング越しに
//! 観測できることの確認に徹する（`wire_policy_ddl.rs` と同じ流儀）。

use crate::common;

use crate::temp_db;

use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn serve(ddl_user: bool) -> (std::net::SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-role-ddl");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
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

/// DDL 権限のあるユーザー: `CREATE ROLE`／`CREATE USER` は同じタグ `CREATE ROLE`、`DROP ROLE` は
/// `DROP ROLE`。重複は `42710`、不在は `42704`、`IF EXISTS` は成功。
#[test]
fn wire_role_ddl_tags_and_sqlstates_for_ddl_user() {
    let (addr, _guard) = serve(true);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    send_simple_query(&mut stream, "CREATE ROLE r1 NOLOGIN");
    assert_eq!(read_command_complete(&mut stream), "CREATE ROLE");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "CREATE USER u1 PASSWORD 'dummy-pw'");
    assert_eq!(read_command_complete(&mut stream), "CREATE ROLE");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "CREATE USER r1");
    expect_error_response_with_sqlstate_and_message(&mut stream, "42710", "already exists");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "CREATE ROLE r2 SUPERUSER");
    expect_error_response_with_sqlstate(&mut stream, "42501");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP ROLE r1, u1");
    assert_eq!(read_command_complete(&mut stream), "DROP ROLE");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP ROLE r1");
    expect_error_response_with_sqlstate(&mut stream, "42704");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP ROLE IF EXISTS r1");
    assert_eq!(read_command_complete(&mut stream), "DROP ROLE");
    read_ready_for_query(&mut stream);
}

/// DDL 権限のないユーザーは対象の有無を問わず `42501`。
#[test]
fn wire_role_ddl_without_permission_is_42501() {
    let (addr, _guard) = serve(false);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    for sql in [
        "CREATE ROLE r",
        "CREATE USER u PASSWORD 'dummy-pw'",
        "DROP ROLE nosuch",
        "DROP ROLE IF EXISTS nosuch",
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "42501");
        read_ready_for_query(&mut stream);
    }
}
