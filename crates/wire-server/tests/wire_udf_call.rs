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

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

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
