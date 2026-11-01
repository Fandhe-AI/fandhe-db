//! `GRANT`／`REVOKE`（SQL-40・Issue #1965）の簡易クエリプロトコル経由の検証。
//!
//! 現状は認証層が現在ロールを束縛しない（#1971）ため、全セッションの `GRANT`／`REVOKE` は対象・ロールの
//! 有無を問わず同じ `42501` になる（DDL 権限の有無にも依らない）。構文エラー `42601`・範囲外の形 `0A000`・
//! 不正な付与 `0LP01` は状態に依存せず wire フレーミング越しに観測できる。確定オラクルは
//! `crates/engine/tests/table_rls/sql40_grant_revoke.rs`（in-process）と `sql::grant_ddl` の単体テスト。

use crate::common;

use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn serve(ddl_user: bool) -> (std::net::SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-grant-revoke");
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

/// 現在ロールが未束縛のセッションは、DDL 権限の有無・対象の有無を問わず `42501`（存在オラクルの防止）。
#[test]
fn wire_grant_revoke_without_bound_role_is_42501_for_every_target() {
    for ddl_user in [false, true] {
        let (addr, _guard) = serve(ddl_user);
        let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
        for sql in [
            "GRANT SELECT ON docs TO bob",
            "GRANT SELECT ON nosuch TO bob",
            "GRANT SELECT (nosuchcol) ON docs TO nosuchrole",
            "REVOKE SELECT ON docs FROM bob",
            "REVOKE ALL ON nosuch FROM PUBLIC CASCADE",
            "GRANT r1 TO r2",
        ] {
            send_simple_query(&mut stream, sql);
            expect_error_response_with_sqlstate(&mut stream, "42501");
            read_ready_for_query(&mut stream);
        }
    }
}

/// 構文エラー・範囲外の形・不正な付与は状態に依存せず固定の SQLSTATE。
#[test]
fn wire_grant_revoke_static_rejections() {
    let (addr, _guard) = serve(true);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    for (sql, state) in [
        ("GRANT SELECT ON docs", "42601"),
        ("GRANT FROBNICATE ON docs TO bob", "42601"),
        ("REVOKE SELECT ON docs TO bob", "42601"),
        ("GRANT USAGE ON TABLE docs TO bob", "0LP01"),
        ("GRANT SELECT ON docs TO PUBLIC WITH GRANT OPTION", "0LP01"),
        ("GRANT USAGE ON SCHEMA public TO bob", "0A000"),
        ("GRANT SELECT ON docs TO bob GRANTED BY carol", "0A000"),
        (
            "ALTER DEFAULT PRIVILEGES GRANT SELECT ON TABLES TO bob",
            "0A000",
        ),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, state);
        read_ready_for_query(&mut stream);
    }
}
