//! `CREATE FUNCTION` 宣言的 UDF 呼び出し（TASK-79、対象ビヘイビア: SQL-9）の
//! 簡易クエリプロトコル経由（生バイトクライアント）検証（TASK-82 の層 A。
//! ポインタ: `docs/spec/05-tasks.md` TASK-82・
//! `docs/spec/04-behavior/sql-surface.md` SQL-9）。
//!
//! UDF 登録・結果列／`WHERE` 両位置からの呼び出し・RLS（不可視行では UDF が
//! 一切評価されない）・拒否経路の wire_code 決定性は
//! `crates/engine/tests/sql_udf_call.rs`（in-process）が確定オラクルとして
//! 検証済みのため、本ファイルは同じ規則が **wire フレーミング** 越しに、かつ
//! **同一接続セッション内の複数文**として観測できることの確認に徹する
//! （`wire_explain.rs`・`wire_hint_order.rs` と同じ流儀）。セッション境界
//! （別接続へ登録が漏れないこと）は wire 層固有の懸念のため本ファイルで
//! 追加検証する。

use crate::common;

use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::*;

fn new_core_with_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-udf-call-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(3), false)],
        ))
        .expect("create table");

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let corpus: Vec<(u64, [f32; 3])> = vec![
        (1, [3.0, 4.0, 0.0]), // norm = 5
        (2, [0.0, 0.0, 1.0]), // norm = 1
        (3, [1.0, 1.0, 1.0]), // norm = sqrt(3)
    ];
    for (id, emb) in &corpus {
        let op_id = OperationId::parse(&format!("wire-udf-op-{id}")).expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            *id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec())],
            &op_id,
        )
        .expect("insert row");
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// 同一接続セッション内で `CREATE FUNCTION` → 結果列位置からの UDF 呼び出しが
/// 成功する。`CommandComplete("CREATE FUNCTION")` → `RowDescription`/`DataRow`/
/// `CommandComplete("SELECT n")` の順に観測できること。
#[test]
fn wire_udf_call_define_then_use_in_result_column_succeeds() {
    let (core, _guard) = new_core_with_docs();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "CREATE FUNCTION norm_scale(v, s) AS s * vec_sum(vec_div(v, vec_norm(v)))",
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "CREATE FUNCTION");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT id, norm_scale(embedding, 2.0) AS score FROM docs \
         ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
    );
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id", "score"]);
    let mut rows = Vec::new();
    for _ in 0..3 {
        rows.push(read_data_row(&mut stream));
    }
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 3");
    read_ready_for_query(&mut stream);

    // id=1: norm=5 なので norm_scale(v, 2.0) = 2.0 * (3/5 + 4/5 + 0/5) = 2.8。
    let row_for_id_1 = rows
        .iter()
        .find(|row| row[0].as_deref() == Some("1"))
        .expect("id=1 row must be present");
    let score: f64 = row_for_id_1[1]
        .as_deref()
        .expect("score is not null")
        .parse()
        .expect("score must be numeric text");
    assert!(
        (score - 2.8).abs() < 1e-6,
        "expected norm_scale(v, 2.0) for id=1 to be 2.8, got {score}"
    );
}

/// `WHERE` 位置からの UDF 呼び出しも同一セッションで成功する。
#[test]
fn wire_udf_call_use_in_where_clause_succeeds() {
    let (core, _guard) = new_core_with_docs();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "CREATE FUNCTION is_unit_norm(v) AS vec_norm(v)",
    );
    read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs WHERE is_unit_norm(embedding) < 1.5 \
         ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
    );
    read_row_description(&mut stream);
    let mut ids = Vec::new();
    // norm < 1.5 を満たすのは id=2（norm=1）のみ。
    let row = read_data_row(&mut stream);
    ids.push(row[0].clone().expect("id is not null"));
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut stream);

    assert_eq!(ids, vec!["2".to_string()]);
}

/// 未定義 UDF 呼び出しは `42883`（束縛段の意味論エラー。`sql::udf_call` の
/// 既存契約。`tests/sql_udf_call.rs::unknown_function_call_is_rejected_with_42883`
/// の同型確認）で拒否され、接続は維持される。
#[test]
fn wire_udf_call_undefined_function_is_rejected_and_connection_survives() {
    let (core, _guard) = new_core_with_docs();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "SELECT id, not_registered(embedding) FROM docs \
         ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
    );
    expect_error_response_with_sqlstate(&mut stream, "42883");
    read_ready_for_query(&mut stream);

    // エラー後も接続は維持され、続くクエリが正常応答すること。
    send_simple_query(
        &mut stream,
        "SELECT id FROM docs ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
    );
    read_row_description(&mut stream);
    for _ in 0..3 {
        read_data_row(&mut stream);
    }
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// 不正な UDF 定義（本体が未定義の参照を含む）は `42883` で拒否され、レジストリは
/// 変更されない（部分登録なし。
/// `tests/sql_udf_call.rs::function_body_referencing_an_undefined_name_is_rejected_with_42883`
/// の同型確認）。
#[test]
fn wire_udf_call_invalid_definition_is_rejected_without_partial_registration() {
    let (core, _guard) = new_core_with_docs();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "CREATE FUNCTION bad_fn(v) AS undefined_name(v)",
    );
    expect_error_response_with_sqlstate(&mut stream, "42883");
    read_ready_for_query(&mut stream);

    // 部分登録されていないため、同名関数の呼び出しは未定義関数として同じ
    // `42883` で拒否される。
    send_simple_query(
        &mut stream,
        "SELECT id, bad_fn(embedding) FROM docs ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
    );
    expect_error_response_with_sqlstate(&mut stream, "42883");
    read_ready_for_query(&mut stream);
}

/// UDF 登録はセッション（接続）境界を越えて漏れない: 接続 1 で登録した関数を
/// 接続 2 から呼び出すと未定義関数として `42883` で拒否される
/// （`tests/sql_udf_call.rs::udf_defined_in_one_session_is_not_visible_from_another_session`
/// の同型確認）。
#[test]
fn wire_udf_call_registration_does_not_leak_across_connections() {
    let (core, _guard) = new_core_with_docs();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);

    let mut conn1 = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    send_simple_query(
        &mut conn1,
        "CREATE FUNCTION only_in_conn1(v) AS vec_norm(v)",
    );
    let tag = read_command_complete(&mut conn1);
    assert_eq!(tag, "CREATE FUNCTION");
    read_ready_for_query(&mut conn1);

    let mut conn2 = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    send_simple_query(
        &mut conn2,
        "SELECT id, only_in_conn1(embedding) FROM docs \
         ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 3",
    );
    expect_error_response_with_sqlstate(&mut conn2, "42883");
    read_ready_for_query(&mut conn2);
}

/// SQL-66（Issue #1920）: DDL 実行権限のあるユーザーの `CREATE FUNCTION`／`DROP FUNCTION` は
/// それぞれ固定タグ `CREATE FUNCTION`／`DROP FUNCTION` を返し、DROP 済みの名前の再 DROP は `42883`。
#[test]
fn wire_drop_function_returns_tag_for_ddl_user() {
    let (core, _guard) = new_core_with_docs();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let store = wire_server::auth::UserStore::load_from_file(&users_path).expect("user store");
    let store = store
        .with_ddl_allowed_users(&["alice".to_string()])
        .expect("known user");
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
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "CREATE FUNCTION persisted_fn(v) AS vec_norm(v)",
    );
    assert_eq!(read_command_complete(&mut stream), "CREATE FUNCTION");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP FUNCTION persisted_fn");
    assert_eq!(read_command_complete(&mut stream), "DROP FUNCTION");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "DROP FUNCTION persisted_fn");
    expect_error_response_with_sqlstate(&mut stream, "42883");
    read_ready_for_query(&mut stream);
}
/// DDL 許可ユーザー `alice` と、同じテナントの非許可ユーザー `carol`（SQL-66・Issue #1921 の検証用）。
fn spawn_server_with_ddl_user(core: Arc<EngineCore>) -> std::net::SocketAddr {
    use wire_server::auth::UserStore;
    use wire_server::limits::ConnectionLimiter;

    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("carol", "tenant-a", "pw-carol"),
    ]);
    let store = UserStore::load_from_file(&users_path)
        .expect("valid user store")
        .with_ddl_allowed_users(&["alice".to_string()])
        .expect("known user");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let store = Arc::new(store);
    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            core,
            ConnectionLimiter::new(16),
            std::time::Duration::from_secs(5),
        );
    });
    addr
}

const CALL_DBL: &str =
    "SELECT dbl(id) AS d FROM docs ORDER BY embedding <=> '[3.0,4.0,0.0]' LIMIT 1";

/// SQL-66・Issue #1921: 接続 A が `CREATE FUNCTION` した関数を、その後に開いた別の接続 B
/// （DDL 権限なし）が開始時のロードで呼べる。
#[test]
fn wire_udf_created_on_one_connection_is_callable_from_a_new_connection() {
    let (core, _guard) = new_core_with_docs();
    let addr = spawn_server_with_ddl_user(core);
    let mut a = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    send_simple_query(&mut a, "CREATE FUNCTION dbl(x) AS x * 2");
    assert_eq!(read_command_complete(&mut a), "CREATE FUNCTION");
    read_ready_for_query(&mut a);

    let mut b = authenticate_to_ready_for_query(addr, "carol", "pw-carol");
    send_simple_query(&mut b, CALL_DBL);
    assert_eq!(read_row_description(&mut b), vec!["d"]);
    let row = read_data_row(&mut b);
    assert_eq!(
        row[0].as_deref().map(|s| s.trim_end_matches(".0")),
        Some("2")
    );
    assert_eq!(read_command_complete(&mut b), "SELECT 1");
    read_ready_for_query(&mut b);
}

/// SQL-66・Issue #1921: 同時に開いている接続 B は、A の `DROP FUNCTION` を次の Query で反映する。
#[test]
fn wire_udf_drop_on_one_connection_is_reflected_on_another_open_connection() {
    let (core, _guard) = new_core_with_docs();
    let addr = spawn_server_with_ddl_user(core);
    let mut a = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    send_simple_query(&mut a, "CREATE FUNCTION dbl(x) AS x * 2");
    assert_eq!(read_command_complete(&mut a), "CREATE FUNCTION");
    read_ready_for_query(&mut a);

    let mut b = authenticate_to_ready_for_query(addr, "carol", "pw-carol");
    send_simple_query(&mut b, CALL_DBL);
    assert_eq!(read_row_description(&mut b), vec!["d"]);
    let _ = read_data_row(&mut b);
    assert_eq!(read_command_complete(&mut b), "SELECT 1");
    read_ready_for_query(&mut b);

    send_simple_query(&mut a, "DROP FUNCTION dbl");
    assert_eq!(read_command_complete(&mut a), "DROP FUNCTION");
    read_ready_for_query(&mut a);

    send_simple_query(&mut b, CALL_DBL);
    expect_error_response_with_sqlstate(&mut b, "42883");
    read_ready_for_query(&mut b);
}
/// SQL-66 (2)(3)・Issue #1917: `CREATE OR REPLACE FUNCTION` は固定タグ `CREATE FUNCTION` を返して
/// 別接続の次の Query へ反映され、`DROP FUNCTION IF EXISTS` は対象が無くてもタグ `DROP FUNCTION` で成功し、
/// 引数名の変更は `42P13`・同名の別シグネチャは `42723` になる。
#[test]
fn wire_create_or_replace_and_drop_if_exists() {
    let (core, _guard) = new_core_with_docs();
    let addr = spawn_server_with_ddl_user(core);
    let mut a = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    send_simple_query(&mut a, "CREATE OR REPLACE FUNCTION dbl(x) AS x * 2");
    assert_eq!(read_command_complete(&mut a), "CREATE FUNCTION");
    read_ready_for_query(&mut a);

    let mut b = authenticate_to_ready_for_query(addr, "carol", "pw-carol");
    send_simple_query(&mut b, CALL_DBL);
    assert_eq!(read_row_description(&mut b), vec!["d"]);
    let row = read_data_row(&mut b);
    assert_eq!(
        row[0].as_deref().map(|s| s.trim_end_matches(".0")),
        Some("2")
    );
    assert_eq!(read_command_complete(&mut b), "SELECT 1");
    read_ready_for_query(&mut b);

    send_simple_query(&mut a, "CREATE OR REPLACE FUNCTION dbl(x) AS x * 5");
    assert_eq!(read_command_complete(&mut a), "CREATE FUNCTION");
    read_ready_for_query(&mut a);

    send_simple_query(&mut b, CALL_DBL);
    assert_eq!(read_row_description(&mut b), vec!["d"]);
    let row = read_data_row(&mut b);
    assert_eq!(
        row[0].as_deref().map(|s| s.trim_end_matches(".0")),
        Some("5")
    );
    assert_eq!(read_command_complete(&mut b), "SELECT 1");
    read_ready_for_query(&mut b);

    send_simple_query(&mut a, "CREATE OR REPLACE FUNCTION dbl(z) AS z");
    expect_error_response_with_sqlstate(&mut a, "42P13");
    read_ready_for_query(&mut a);
    send_simple_query(&mut a, "CREATE OR REPLACE FUNCTION dbl(x, y) AS x");
    expect_error_response_with_sqlstate(&mut a, "42723");
    read_ready_for_query(&mut a);

    send_simple_query(&mut a, "DROP FUNCTION IF EXISTS nothing_here");
    assert_eq!(read_command_complete(&mut a), "DROP FUNCTION");
    read_ready_for_query(&mut a);
    send_simple_query(&mut a, "DROP FUNCTION IF EXISTS dbl CASCADE");
    assert_eq!(read_command_complete(&mut a), "DROP FUNCTION");
    read_ready_for_query(&mut a);
    send_simple_query(&mut b, CALL_DBL);
    expect_error_response_with_sqlstate(&mut b, "42883");
    read_ready_for_query(&mut b);
}
