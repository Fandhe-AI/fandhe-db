//! `CREATE TABLE <table> (<col> <type>[, ...])`（SQL-23・TASK-85・TASK-202、
//! Issue #899）の簡易クエリプロトコル経由（生バイトクライアント）検証（層 A。
//! ポインタ: `docs/spec/05-tasks.md` TASK-85・TASK-202・
//! `docs/spec/04-behavior/sql-surface.md` SQL-23）。
//!
//! 構文・実行本体そのものは `crates/engine/tests/sql_create_table.rs` が確定
//! オラクルとして検証済みのため、本ファイルは (1) 同じ規則が wire フレーミング
//! 越しに観測できること、(2) `--ddl-allowed-users`（`auth::UserStore::
//! with_ddl_allowed_users`）による DDL 実行権限ゲートが wire 経由でも fail-closed
//! に働くこと、の確認に徹する（`wire_upsert.rs` と同じ流儀）。CLI 引数
//! `--ddl-allowed-users` 自体の受理・拒否は `wire_ddl_permission_cli.rs` の担当。

use crate::common;

use crate::temp_db;

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

use common::*;

fn new_core_without_tables() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-create-table");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `spawn_server_with_engine`（`common/mod.rs`）と同型だが、`UserStore` を
/// `--ddl-allowed-users` 相当の許可主体付きで構築できるようにしたローカル版
/// （Issue #899。CLI 経由ではなく `UserStore::with_ddl_allowed_users` を直接呼ぶ
/// ため実バイナリを子プロセス起動しない。CLI の受理・拒否自体は
/// `wire_ddl_permission_cli.rs` が別途検証する）。
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

fn connect_as(addr: SocketAddr, username: &str, password: &str) -> TcpStream {
    authenticate_to_ready_for_query(addr, username, password)
}

/// 許可主体は `CREATE TABLE` を実行でき、`CommandComplete` タグは
/// `CREATE TABLE`（件数を持たない固定タグ）。
#[test]
fn wire_create_table_succeeds_for_ddl_principal() {
    let (core, _guard) = new_core_without_tables();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_with_ddl_allowed_users(&users_path, core, &["alice"]);
    let mut stream = connect_as(addr, "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(3), body TEXT)",
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "CREATE TABLE");
    read_ready_for_query(&mut stream);
}

/// 同名の再送は `42P07`（重複拒否。既存スキーマは変更されない）。
#[test]
fn wire_create_table_duplicate_name_is_rejected() {
    let (core, _guard) = new_core_without_tables();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_with_ddl_allowed_users(&users_path, core, &["alice"]);
    let mut stream = connect_as(addr, "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, body TEXT)",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, a TEXT, b TEXT)",
    );
    expect_error_response_with_sqlstate(&mut stream, "42P07");
    read_ready_for_query(&mut stream);
}

/// `--ddl-allowed-users` に含まれないユーザーは `42501`（未許可。カタログの状態を
/// 一切観測できない。fail-closed）。
#[test]
fn wire_create_table_rejects_non_principal_user() {
    let (core, _guard) = new_core_without_tables();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_with_ddl_allowed_users(&users_path, core, &["alice"]);
    let mut stream = connect_as(addr, "bob", "pw-bob");

    send_simple_query(
        &mut stream,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, body TEXT)",
    );
    expect_error_response_with_sqlstate(&mut stream, "42501");
    read_ready_for_query(&mut stream);
}

/// `--ddl-allowed-users` 未指定のサーバーでは、ユーザーストアに存在する全ユーザーが
/// `42501` になる（許可主体なし＝全 DDL 拒否の既定）。
#[test]
fn wire_create_table_rejects_everyone_when_ddl_allowed_users_unset() {
    let (core, _guard) = new_core_without_tables();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_with_ddl_allowed_users(&users_path, core, &[]);
    let mut stream = connect_as(addr, "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, body TEXT)",
    );
    expect_error_response_with_sqlstate(&mut stream, "42501");
    read_ready_for_query(&mut stream);
}

/// 未許可の主体は、対象テーブルの有無に関わらず同じ `42501` のみを受け取り、
/// カタログ状態を観測できない（fail-closed）。一方、構造検証（カタログ照会
/// なし）は `DROP TABLE`（Issue #902）と同じく権限ゲートより前に通す設計の
/// ため、許可形状に一致しない構文は権限の有無に関わらず `42601` になる
/// （構文の正誤自体はカタログの存在情報ではないためオラクルにならない。
/// `crates/engine/tests/sql_create_table.rs::
/// create_table_rejects_unauthorized_session_for_garbage_syntax` と同じ契約）。
#[test]
fn wire_create_table_rejects_non_principal_user_even_for_malformed_syntax() {
    let (core, _guard) = new_core_without_tables();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_with_ddl_allowed_users(&users_path, core, &["alice"]);
    let mut stream = connect_as(addr, "bob", "pw-bob");

    send_simple_query(
        &mut stream,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, (()) not a schema",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}

/// 許可主体が作成したテーブルへ、そのユーザー自身が同一 wire セッション内で
/// 即座に書き込み・検索できる（RLS-11。DDL 自体はテナント非依存の共有カタログ
/// 操作だが、行データは通常どおりテナント境界に従う）。
#[test]
fn wire_create_table_then_insert_and_select_same_session() {
    let (core, _guard) = new_core_without_tables();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_with_ddl_allowed_users(&users_path, core, &["alice"]);
    let mut stream = connect_as(addr, "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "CREATE TABLE docs (id BIGINT PRIMARY KEY, embedding VECTOR(2), body TEXT)",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, body) VALUES (1, '[0.1,0.2]', 'hello') USING OPERATION_ID 'wire-create-table-op-1'",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT body FROM docs LIMIT 10");
    let _cols = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![Some("hello".to_string())]);
}

/// 暗黙の `id` を持たない新しい表（TABLE-23・Issue #2199）: PG と同じく `SELECT *` の
/// RowDescription は宣言した列だけを返し、`id` を省いた INSERT が通る。宣言の無い `id` は `42703`。
#[test]
fn wire_select_star_on_a_table_without_id_lists_declared_columns_only() {
    let (core, _guard) = new_core_without_tables();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_with_ddl_allowed_users(&users_path, core, &["alice"]);
    let mut stream = connect_as(addr, "alice", "pw-alice");

    send_simple_query(&mut stream, "CREATE TABLE notes (title TEXT, n INTEGER)");
    assert_eq!(read_command_complete(&mut stream), "CREATE TABLE");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO notes (title, n) VALUES ('a', 1) USING OPERATION_ID 'wire-noid-1'",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT * FROM notes LIMIT 10");
    assert_eq!(read_row_description(&mut stream), vec!["title", "n"]);
    let row = read_data_row(&mut stream);
    assert_eq!(row.len(), 2);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM notes LIMIT 10");
    expect_error_response_with_sqlstate(&mut stream, "42703");
    read_ready_for_query(&mut stream);
}
