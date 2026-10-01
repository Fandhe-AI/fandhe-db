//! `CREATE INDEX`／`DROP INDEX`（索引宣言。TASK-206・INDEX-7・SQL-23、
//! Issue #908）の簡易クエリプロトコル経由（生バイトクライアント）検証（層 A。
//! ポインタ: `docs/spec/05-tasks.md` TASK-206・`docs/spec/04-behavior/
//! indexing.md` INDEX-7・`docs/spec/04-behavior/sql-surface.md` SQL-23）。
//!
//! 構文・カタログ判定・結果集合の不変は `crates/engine/tests/sql_index_ddl.rs`
//! が確定オラクルとして検証済みのため、本ファイルは (1) pg 互換の
//! `CommandComplete` タグと ERR-6 の分類が wire フレーミング越しに観測できること、
//! (2) `--ddl-allowed-users`（`auth::UserStore::with_ddl_allowed_users`。
//! `CREATE TABLE`／`CREATE VIEW` と共有する唯一の DDL 実行権限ゲート）が索引 DDL
//! にもユーザー単位・fail-closed に効くこと、の確認に徹する
//! （`wire_create_table.rs`・`wire_create_view.rs` と同じ流儀）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

use common::*;

fn new_core_with_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-index-ddl");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `wire_create_table.rs::spawn_with_ddl_allowed_users` と同型のローカル版
/// （`UserStore::with_ddl_allowed_users` を直接呼ぶ。CLI 引数自体の受理・拒否は
/// `wire_ddl_permission_cli.rs` の担当）。
fn spawn_with_ddl_allowed_users(
    users_path: &std::path::Path,
    engine: Arc<EngineCore>,
    ddl_allowed_users: &[&str],
) -> SocketAddr {
    let store = UserStore::load_from_file(users_path).expect("valid user store");
    let store = if ddl_allowed_users.is_empty() {
        store
    } else {
        let names: Vec<String> = ddl_allowed_users.iter().map(|s| s.to_string()).collect();
        store
            .with_ddl_allowed_users(&names)
            .expect("ddl principals must reference known users")
    };
    let store = Arc::new(store);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            engine,
            limiter,
            Duration::from_secs(5),
        );
    });

    addr
}

fn users() -> common::UserStoreFile {
    write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ])
}

fn expect_error(stream: &mut TcpStream, sql: &str, sqlstate: &str) {
    send_simple_query(stream, sql);
    expect_error_response_with_sqlstate(stream, sqlstate);
    read_ready_for_query(stream);
}

fn expect_command(stream: &mut TcpStream, sql: &str, tag: &str) {
    send_simple_query(stream, sql);
    assert_eq!(read_command_complete(stream), tag, "{sql}");
    read_ready_for_query(stream);
}

/// 許可主体は索引を宣言・削除でき、`CommandComplete` タグは件数を持たない
/// `CREATE INDEX`／`DROP INDEX`。
#[test]
fn wire_index_ddl_succeeds_for_ddl_principal() {
    let (core, _guard) = new_core_with_docs();
    let addr = spawn_with_ddl_allowed_users(&users(), core, &["alice"]);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    expect_command(
        &mut stream,
        "CREATE INDEX idx_body ON docs (body)",
        "CREATE INDEX",
    );
    expect_command(
        &mut stream,
        "CREATE INDEX idx_vec ON docs USING hnsw (embedding)",
        "CREATE INDEX",
    );
    expect_command(&mut stream, "DROP INDEX idx_body", "DROP INDEX");
    expect_command(&mut stream, "DROP INDEX idx_vec", "DROP INDEX");
}

/// 許可されていないユーザー（および許可主体の指定が無いサーバーの全ユーザー）には、
/// 対象テーブル・索引の実在有無を問わず常に `42501` を返す（存在情報の非漏えい）。
#[test]
fn wire_index_ddl_rejects_non_principals_regardless_of_existence() {
    for allowed in [&["alice"][..], &[][..]] {
        let (core, _guard) = new_core_with_docs();
        let addr = spawn_with_ddl_allowed_users(&users(), core, allowed);
        if !allowed.is_empty() {
            let mut alice = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
            expect_command(
                &mut alice,
                "CREATE INDEX idx_body ON docs (body)",
                "CREATE INDEX",
            );
        }
        let mut bob = authenticate_to_ready_for_query(addr, "bob", "pw-bob");
        for sql in [
            "CREATE INDEX idx_x ON docs (body)",
            "CREATE INDEX idx_y ON ghost (body)",
            "DROP INDEX idx_body",
            "DROP INDEX ghost_idx",
        ] {
            expect_error(&mut bob, sql, "42501");
        }
    }
}

/// ERR-6 の分類（重複 `42P07`・索引不在 `42704`・列不在 `42703`・種別不一致
/// `42809`・種別と列型の不整合／未対応構文 `0A000`・構文外 `42601`）が wire
/// 経由で観測できる。エラー後もセッションは継続できる。
#[test]
fn wire_index_ddl_error_classification() {
    let (core, _guard) = new_core_with_docs();
    let addr = spawn_with_ddl_allowed_users(&users(), core, &["alice"]);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    expect_command(
        &mut stream,
        "CREATE INDEX idx_body ON docs (body)",
        "CREATE INDEX",
    );
    expect_error(&mut stream, "CREATE INDEX idx_body ON docs (body)", "42P07");
    expect_error(&mut stream, "CREATE INDEX docs ON docs (body)", "42P07");
    expect_error(&mut stream, "DROP INDEX ghost_idx", "42704");
    expect_error(&mut stream, "DROP INDEX docs", "42809");
    expect_error(
        &mut stream,
        "CREATE INDEX idx_x ON idx_body (body)",
        "42809",
    );
    expect_error(
        &mut stream,
        "ALTER TABLE idx_body ADD COLUMN extra TEXT",
        "42809",
    );
    expect_error(&mut stream, "CREATE INDEX idx_m ON docs (missing)", "42703");
    expect_error(&mut stream, "CREATE INDEX idx_t ON ghost (body)", "42P01");
    expect_error(
        &mut stream,
        "CREATE INDEX idx_h ON docs USING hnsw (body)",
        "0A000",
    );
    expect_error(
        &mut stream,
        "CREATE INDEX idx_b ON docs USING btree (body)",
        "0A000",
    );
    expect_error(
        &mut stream,
        "CREATE INDEX idx_p ON docs (body) WHERE body = 'x'",
        "0A000",
    );
    expect_error(&mut stream, "DROP INDEX IF EXISTS idx_body", "42601");
    expect_command(&mut stream, "DROP INDEX idx_body", "DROP INDEX");
}
