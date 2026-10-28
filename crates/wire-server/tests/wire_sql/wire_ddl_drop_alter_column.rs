//! `ALTER TABLE ... DROP COLUMN`／`ALTER COLUMN ... TYPE`（TABLE-19・SQL-23、
//! Issue #1167）の簡易クエリプロトコル経由（生バイトクライアント）検証。
//! 構文・実行契約は `crates/engine/tests/sql_ddl_drop_alter_column.rs` が確定
//! オラクルで、本ファイルは wire フレーミング越しの CommandComplete／
//! ErrorResponse の SQLSTATE と ReadyForQuery 復帰、DDL 権限ゲートに絞る。

use crate::common;

use crate::temp_db;

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
    let path = temp_db::unique_db_path("wire-ddl-drop-alter-column");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("note", ColumnType::Text, true),
                ColumnDef::new("qty", ColumnType::Integer, true),
                ColumnDef::new(
                    "amount",
                    ColumnType::Numeric {
                        precision: 5,
                        scale: 2,
                    },
                    true,
                ),
            ],
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
fn wire_drop_column_and_alter_type_return_command_complete() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);
    for sql in [
        "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(12,2)",
        "ALTER TABLE docs DROP COLUMN note",
    ] {
        send_simple_query(&mut stream, sql);
        assert_eq!(read_command_complete(&mut stream), "ALTER TABLE");
        read_ready_for_query(&mut stream);
    }
}

#[test]
fn wire_error_sqlstates_and_ready_for_query_recovery() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);
    for (sql, code) in [
        ("ALTER TABLE docs DROP COLUMN missing", "42703"),
        ("ALTER TABLE nope DROP COLUMN note", "42P01"),
        ("ALTER TABLE docs DROP COLUMN embedding", "42601"),
        ("ALTER TABLE docs DROP COLUMN tenant_id", "42601"),
        ("ALTER TABLE docs ALTER COLUMN note TYPE INTEGER", "42804"),
        // 同一型への変更は 42804（NUMERIC の精度縮小は代入変換のある一般変換として受理される。
        // TABLE-40・Issue #1636）。
        ("ALTER TABLE docs ALTER COLUMN qty TYPE INTEGER", "42804"),
        ("ALTER TABLE docs DROP COLUMN note CASCADE", "42601"),
        ("ALTER TABLE docs DROP COLUMN IF EXISTS note", "42601"),
        // USING の式が未定義列を参照する束縛エラー。
        (
            "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(9,2) USING missing",
            "42703",
        ),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, code);
        read_ready_for_query(&mut stream);
    }
}

/// ErrorResponse（'E'）の本文を読み、SQLSTATE を確認して本文を返す。
fn read_error_body_with_sqlstate(stream: &mut std::net::TcpStream, sqlstate: &str) -> String {
    use std::io::Read;
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'E', "expected ErrorResponse");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    let text = String::from_utf8_lossy(&body).into_owned();
    assert!(text.contains(sqlstate), "expected {sqlstate}, got {text:?}");
    text
}

/// USING 付きの型変換が wire 経由で成功し（CommandComplete）、行の変換に失敗した場合は
/// `22P02` の ErrorResponse でメッセージに行の値が含まれず、表も変わらない
/// （TABLE-40・Issue #1636）。
#[test]
fn wire_using_conversion_succeeds_and_row_error_does_not_leak_values() {
    let (core, _guard) = new_core_with_docs_table();
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();
    let ctx = engine::policy::PolicyContext::with_visibilities(
        "tenant-a",
        [engine::storage::Visibility::Public],
    )
    .expect("tenant");
    for (id, note) in [(1, "10"), (2, "not-a-number-SECRET")] {
        core.execute_sql_in_session(
            &ctx,
            &mut session,
            &format!(
                "INSERT INTO docs (id, embedding, note) VALUES ({id}, '[0.1,0.2]', '{note}') \
                 USING OPERATION_ID 'wire-using-{id}'"
            ),
        )
        .expect("seed row");
    }
    let mut stream = spawn_with_alice_as_ddl_principal(core);
    send_simple_query(
        &mut stream,
        "ALTER TABLE docs ALTER COLUMN note TYPE INTEGER USING note::integer",
    );
    let body = read_error_body_with_sqlstate(&mut stream, "22P02");
    assert!(!body.contains("SECRET"), "value leaked: {body:?}");
    read_ready_for_query(&mut stream);
    // 違反行を除くと同じ文が成功する。
    send_simple_query(
        &mut stream,
        "DELETE FROM docs WHERE id = 2 USING OPERATION_ID 'wire-using-del'",
    );
    assert_eq!(read_command_complete(&mut stream), "DELETE 1");
    read_ready_for_query(&mut stream);
    send_simple_query(
        &mut stream,
        "ALTER TABLE docs ALTER COLUMN note TYPE INTEGER USING note::integer",
    );
    assert_eq!(read_command_complete(&mut stream), "ALTER TABLE");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire_dependent_check_column_is_rejected_with_2bp01() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);
    send_simple_query(
        &mut stream,
        "ALTER TABLE docs ADD CONSTRAINT qty_ck CHECK (qty > 0)",
    );
    assert_eq!(read_command_complete(&mut stream), "ALTER TABLE");
    read_ready_for_query(&mut stream);
    send_simple_query(&mut stream, "ALTER TABLE docs DROP COLUMN qty");
    expect_error_response_with_sqlstate(&mut stream, "2BP01");
    read_ready_for_query(&mut stream);
}

/// CHECK 参照列でも拡大変換（INTEGER→BIGINT）は受理され（Issue #1427）、値の変換を伴う
/// 一般変換は CHECK を新しい型で束縛できなければ束縛エラー（`qty > 0` は TEXT と数値を比較
/// できず 42883）、違反する値になる変換は 23514（TABLE-40・Issue #1637）。
#[test]
fn wire_check_column_widening_is_accepted_and_conversion_revalidates_the_check() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);
    send_simple_query(
        &mut stream,
        "ALTER TABLE docs ADD CONSTRAINT qty_ck CHECK (qty > 0)",
    );
    assert_eq!(read_command_complete(&mut stream), "ALTER TABLE");
    read_ready_for_query(&mut stream);
    send_simple_query(&mut stream, "ALTER TABLE docs ALTER COLUMN qty TYPE TEXT");
    expect_error_response_with_sqlstate(&mut stream, "42883");
    read_ready_for_query(&mut stream);
    send_simple_query(&mut stream, "ALTER TABLE docs ALTER COLUMN qty TYPE BIGINT");
    assert_eq!(read_command_complete(&mut stream), "ALTER TABLE");
    read_ready_for_query(&mut stream);
}

/// 依存オブジェクトを持つ列の一般変換（TABLE-40・Issue #1637）: ビューが参照する列は 0A000、
/// 参照しない列は受理される。
#[test]
fn wire_view_column_conversion_is_rejected_with_0a000_and_unreferenced_is_accepted() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_as_ddl_principal(core);
    send_simple_query(&mut stream, "CREATE VIEW v_qty AS SELECT qty FROM docs");
    assert_eq!(read_command_complete(&mut stream), "CREATE VIEW");
    read_ready_for_query(&mut stream);
    send_simple_query(
        &mut stream,
        "ALTER TABLE docs ALTER COLUMN qty TYPE SMALLINT USING qty * 1",
    );
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    read_ready_for_query(&mut stream);
    send_simple_query(
        &mut stream,
        "ALTER TABLE docs ALTER COLUMN note TYPE VARCHAR(8) USING note",
    );
    assert_eq!(read_command_complete(&mut stream), "ALTER TABLE");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire_non_ddl_principal_is_rejected_with_42501_regardless_of_existence() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice_without_ddl_privilege(core);
    for sql in [
        "ALTER TABLE docs DROP COLUMN note",
        "ALTER TABLE nope DROP COLUMN note",
        "ALTER TABLE docs DROP COLUMN missing",
        "ALTER TABLE docs ALTER COLUMN amount TYPE NUMERIC(12,2)",
        "ALTER TABLE nope ALTER COLUMN x TYPE TEXT",
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "42501");
        read_ready_for_query(&mut stream);
    }
}
