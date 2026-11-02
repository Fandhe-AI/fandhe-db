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
/// `ALTER ROLE`／`ALTER USER`（`RENAME TO` を含む）はタグ `ALTER ROLE`。不在は `42704`、禁止属性は `42501`
/// （SQL-39、Issue #1963）。
#[test]
fn wire_alter_role_tag_and_sqlstates() {
    let (addr, _guard) = serve(true);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    send_simple_query(&mut stream, "CREATE ROLE r1");
    assert_eq!(read_command_complete(&mut stream), "CREATE ROLE");
    read_ready_for_query(&mut stream);

    for sql in [
        "ALTER ROLE r1 WITH LOGIN CREATEDB",
        "ALTER USER r1 PASSWORD 'dummy-pw'",
        "ALTER ROLE r1 RENAME TO r2",
    ] {
        send_simple_query(&mut stream, sql);
        assert_eq!(read_command_complete(&mut stream), "ALTER ROLE", "{sql}");
        read_ready_for_query(&mut stream);
    }

    for (sql, state) in [
        ("ALTER ROLE r1 LOGIN", "42704"),
        ("ALTER ROLE r2 SUPERUSER", "42501"),
        ("ALTER ROLE r2 VALID UNTIL 'infinity'", "0A000"),
        ("ALTER ROLE r2 BOGUS", "42601"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, state);
        read_ready_for_query(&mut stream);
    }
}

/// 認証主体のロールが未束縛の wire セッションでは、`SET ROLE <名前>` は名前の有無を問わず `42501`、
/// `RESET ROLE` はタグ `RESET`、`SET ROLE NONE` はタグ `SET`（SQL-39、Issue #1963）。
#[test]
fn wire_set_role_on_unbound_session() {
    let (addr, _guard) = serve(true);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    send_simple_query(&mut stream, "CREATE ROLE r1");
    assert_eq!(read_command_complete(&mut stream), "CREATE ROLE");
    read_ready_for_query(&mut stream);

    for sql in ["SET ROLE r1", "SET ROLE nosuch", "SET SESSION ROLE r1"] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "42501");
        read_ready_for_query(&mut stream);
    }

    send_simple_query(&mut stream, "RESET ROLE");
    assert_eq!(read_command_complete(&mut stream), "RESET");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SET ROLE NONE");
    assert_eq!(read_command_complete(&mut stream), "SET");
    read_ready_for_query(&mut stream);
}
