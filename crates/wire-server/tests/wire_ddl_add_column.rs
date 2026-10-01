//! `ALTER TABLE <table> ADD COLUMN <column> <type>`（TASK-202、対象ビヘイビア:
//! SQL-23）の簡易クエリプロトコル経由（生バイトクライアント）検証（層 A。
//! ポインタ: `docs/spec/05-tasks.md` TASK-202・
//! `docs/spec/04-behavior/sql-surface.md` SQL-23）。
//!
//! `ALTER TABLE` 自体の構文・実行契約（列の nullable 化・既存行が NULL で
//! 読める・世代進行・エラー分類等）は `crates/engine/tests/sql_ddl_add_column.rs`
//! が確定オラクルとして検証済みのため、本ファイルは同じ規則が **wire
//! フレーミング** 越しに観測できること、および DDL 実行権限ゲート
//! （`CREATE TABLE`／`DROP TABLE` と共有する `--ddl-allowed-users`。
//! `UserStore::with_ddl_allowed_users` → 認証成功後の `SessionState::allow_ddl`
//! → `sql::ddl::require_ddl_permission`）が認証済み `username` 単位で正しく
//! 効くことに絞る（`wire_drop_table.rs`・`wire_insert_operation_id.rs` と同じ流儀）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

use common::*;

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-ddl-add-column-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `spawn_server_with_engine`（`common/mod.rs`）と同型だが、`UserStore` を
/// 呼び出し元が組み立てた値として受け取る（`--ddl-allowed-users` 適用済みの
/// ストアをテストごとに構成するため。`wire_drop_table.rs` と同じ構成）。
fn spawn_server_with_engine_and_store(
    store: UserStore,
    engine: Arc<EngineCore>,
) -> std::net::SocketAddr {
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

/// ユーザーストアを組み立て、`ddl_allowed` に列挙した username にのみ DDL 実行
/// 権限を付与した（`--ddl-allowed-users` 相当）サーバーを起動する。
fn spawn_with_ddl_allowed_users(
    core: Arc<EngineCore>,
    users: &[(&str, &str, &str)],
    ddl_allowed: &[&str],
) -> std::net::SocketAddr {
    let users_path = write_user_store_file(users);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let store = if ddl_allowed.is_empty() {
        store
    } else {
        let names: Vec<String> = ddl_allowed.iter().map(|s| s.to_string()).collect();
        store
            .with_ddl_allowed_users(&names)
            .expect("ddl allowed users must be known usernames")
    };
    spawn_server_with_engine_and_store(store, core)
}

/// `alice` を DDL 権限保持ユーザーとして起動し、認証済みストリームを返す。
fn spawn_with_alice_as_ddl_principal(core: Arc<EngineCore>) -> std::net::TcpStream {
    let addr = spawn_with_ddl_allowed_users(core, &[("alice", "tenant-a", "pw-alice")], &["alice"]);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

/// `alice` を DDL 権限**非**保持ユーザー（`--ddl-allowed-users` 未指定相当）として
/// 起動し、認証済みストリームを返す（既定＝全 DDL 拒否。fail-closed）。
fn spawn_with_alice_without_ddl_privilege(core: Arc<EngineCore>) -> std::net::TcpStream {
    let addr = spawn_with_ddl_allowed_users(core, &[("alice", "tenant-a", "pw-alice")], &[]);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

#[test]
fn wire_ddl_principal_can_alter_table_and_receives_command_complete() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);

    send_simple_query(&mut stream, "ALTER TABLE docs ADD COLUMN note TEXT");
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "ALTER TABLE");
    read_ready_for_query(&mut stream);
}

/// `NOT NULL`／`DEFAULT` 付きの ADD COLUMN は受理される（Issue #1169）。
#[test]
fn wire_add_column_with_not_null_and_default_receives_command_complete() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);

    for sql in [
        "ALTER TABLE docs ADD COLUMN lang TEXT NOT NULL DEFAULT 'ja'",
        "ALTER TABLE docs ADD COLUMN n INTEGER DEFAULT 3",
        "ALTER TABLE docs ADD COLUMN d DATE NOT NULL DEFAULT '2020-01-01'",
        "ALTER TABLE docs ADD COLUMN u UUID NOT NULL DEFAULT '00000000-0000-0000-0000-000000000001'",
        "ALTER TABLE docs ADD COLUMN ts TIMESTAMP NOT NULL DEFAULT '2020-01-01 00:00:00'",
    ] {
        send_simple_query(&mut stream, sql);
        let tag = read_command_complete(&mut stream);
        assert_eq!(tag, "ALTER TABLE", "{sql}");
        read_ready_for_query(&mut stream);
    }
}

#[test]
fn wire_added_column_is_selectable_over_the_same_wire_session() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);

    send_simple_query(&mut stream, "ALTER TABLE docs ADD COLUMN note TEXT");
    assert_eq!(read_command_complete(&mut stream), "ALTER TABLE");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, note) VALUES (1, '[0.1,0.2]', 'hello') USING OPERATION_ID 'op-1'",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT note FROM docs WHERE note = 'hello' LIMIT 10",
    );
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["note".to_string()]);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![Some("hello".to_string())]);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire_non_ddl_principal_is_rejected_with_42501() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_without_ddl_privilege(core);

    send_simple_query(&mut stream, "ALTER TABLE docs ADD COLUMN note TEXT");
    expect_error_response_with_sqlstate(&mut stream, "42501");
    read_ready_for_query(&mut stream);
}

/// DDL 権限ゲートはカタログ照会より必ず先に判定する——権限の無いユーザーへ
/// テーブル・列の存在有無を一切返さない（security.md P0）。
#[test]
fn wire_permission_denial_precedes_catalog_lookup_over_wire() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_without_ddl_privilege(core);

    for sql in [
        "ALTER TABLE nonexistent_table ADD COLUMN note TEXT",
        "ALTER TABLE docs ADD COLUMN embedding TEXT",
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "42501");
        read_ready_for_query(&mut stream);
    }
}

#[test]
fn wire_undefined_table_is_rejected_with_42p01() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);

    send_simple_query(
        &mut stream,
        "ALTER TABLE nonexistent_table ADD COLUMN note TEXT",
    );
    expect_error_response_with_sqlstate(&mut stream, "42P01");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire_duplicate_column_is_rejected_with_42701() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);

    send_simple_query(&mut stream, "ALTER TABLE docs ADD COLUMN embedding TEXT");
    expect_error_response_with_sqlstate(&mut stream, "42701");
    read_ready_for_query(&mut stream);
}

/// `DATE` 列の不正な `DEFAULT` は engine の SQLSTATE 写像どおり拒否される（Issue #1279）。
#[test]
fn wire_date_default_errors_are_mapped_to_sqlstate() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);

    for (sql, code) in [
        (
            "ALTER TABLE docs ADD COLUMN d DATE DEFAULT '2020-02-30'",
            "22008",
        ),
        ("ALTER TABLE docs ADD COLUMN d DATE DEFAULT 'abc'", "22007"),
        ("ALTER TABLE docs ADD COLUMN d DATE DEFAULT 1", "42601"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, code);
        read_ready_for_query(&mut stream);
    }
}

/// `UUID` 列の不正な `DEFAULT` は engine の SQLSTATE 写像どおり拒否される（Issue #1281）。
#[test]
fn wire_uuid_default_errors_are_mapped_to_sqlstate() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);

    for (sql, code) in [
        ("ALTER TABLE docs ADD COLUMN u UUID DEFAULT 'abc'", "22P02"),
        ("ALTER TABLE docs ADD COLUMN u UUID DEFAULT 1", "42601"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, code);
        read_ready_for_query(&mut stream);
    }
}

/// `TIMESTAMP` 列の不正な `DEFAULT` は engine の SQLSTATE 写像どおり拒否される（Issue #1280）。
#[test]
fn wire_timestamp_default_errors_are_mapped_to_sqlstate() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);

    for (sql, code) in [
        (
            "ALTER TABLE docs ADD COLUMN t TIMESTAMP DEFAULT '2020-02-30 00:00:00'",
            "22008",
        ),
        (
            "ALTER TABLE docs ADD COLUMN t TIMESTAMP DEFAULT 'abc'",
            "22007",
        ),
        ("ALTER TABLE docs ADD COLUMN t TIMESTAMP DEFAULT 1", "42601"),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, code);
        read_ready_for_query(&mut stream);
    }
}

#[test]
fn wire_vector_column_is_rejected_with_0a000() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);

    send_simple_query(&mut stream, "ALTER TABLE docs ADD COLUMN v VECTOR(4)");
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire_malformed_syntax_is_rejected_with_42601() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);

    for sql in [
        "ALTER TABLE docs ADD note TEXT",
        "ALTER TABLE docs ADD COLUMN IF NOT EXISTS note TEXT",
        // 列制約のうち `UNIQUE`／`DEFAULT NULL`／リテラル種別不一致は引き続き拒否する
        // （`NOT NULL`／`DEFAULT <literal>` は Issue #1169 で受理）。
        "ALTER TABLE docs ADD COLUMN note TEXT UNIQUE",
        "ALTER TABLE docs ADD COLUMN note TEXT DEFAULT NULL",
        "ALTER TABLE docs ADD COLUMN note TEXT DEFAULT 1",
        // `DROP COLUMN embedding` は VECTOR 列保護（Issue #1167 以降）で 42601。
        "ALTER TABLE docs DROP COLUMN embedding",
        "ALTER TABLE docs ADD COLUMN note TEXT USING OPERATION_ID 'op-1'",
        // 予約列名（疑似列・RLS 内部列）は構造検証段階で拒否する。
        "ALTER TABLE docs ADD COLUMN tenant_id TEXT",
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "42601");
        read_ready_for_query(&mut stream);
    }
}

/// 別のユーザー（`bob`）は `--ddl-allowed-users` に含まれていなくても、`alice` の
/// テナントとは無関係にカタログを共有する（DDL はテナント境界の外側にある
/// 概念であり、成功すれば以後どのテナントの接続からも新しい列が見える）。
#[test]
fn wire_alter_table_by_one_principal_is_visible_to_other_tenants_over_wire() {
    let (core, _guard) = new_core_with_docs_table();
    let addr = spawn_with_ddl_allowed_users(
        core,
        &[
            ("alice", "tenant-a", "pw-alice"),
            ("bob", "tenant-b", "pw-bob"),
        ],
        &["alice"],
    );

    let mut alice_stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    send_simple_query(&mut alice_stream, "ALTER TABLE docs ADD COLUMN note TEXT");
    assert_eq!(read_command_complete(&mut alice_stream), "ALTER TABLE");
    read_ready_for_query(&mut alice_stream);

    let mut bob_stream = authenticate_to_ready_for_query(addr, "bob", "pw-bob");
    send_simple_query(&mut bob_stream, "SELECT note FROM docs LIMIT 10");
    let columns = read_row_description(&mut bob_stream);
    assert_eq!(columns, vec!["note".to_string()]);
    let tag = read_command_complete(&mut bob_stream);
    assert_eq!(tag, "SELECT 0");
    read_ready_for_query(&mut bob_stream);

    // bob は DDL 権限が無い（`--ddl-allowed-users alice` のみ）。
    send_simple_query(&mut bob_stream, "ALTER TABLE docs ADD COLUMN other TEXT");
    expect_error_response_with_sqlstate(&mut bob_stream, "42501");
    read_ready_for_query(&mut bob_stream);
}
