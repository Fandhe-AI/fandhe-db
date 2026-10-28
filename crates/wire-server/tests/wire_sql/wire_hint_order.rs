//! `HINT ORDER(...)`（TASK-76、対象ビヘイビア: SQL-7）の簡易クエリプロトコル
//! 経由（生バイトクライアント）検証（TASK-82 の層 A。ポインタ:
//! `docs/spec/05-tasks.md` TASK-82・`docs/spec/04-behavior/sql-surface.md` SQL-7）。
//!
//! 評価順序の規則そのもの（既定順序との結果一致・許可順列の受理・不正形の
//! 拒否・暗黙 RLS 事前フィルタ＋実行時安全網が `HINT` で外せないこと）は
//! `crates/engine/tests/sql_evaluation_order.rs`（in-process）が確定オラクル
//! として検証済みのため、本ファイルは同じ規則が **wire フレーミング** 越しに
//! 観測できることの確認に徹する（`wire_explain.rs` と同じ流儀）。
//!
//! 他テナントの不許可行が `DISTANCE` 先行の `HINT` でも混入しないことは、
//! 他テナントの行を「クエリベクトルに最近傍」となるよう仕込んだコーパスで
//! 確認する（暗黙 RLS 事前フィルタが実際にスキップされていれば、結果集合の
//! 先頭が入れ替わって可視化される構成。RLS-5・RLS-7 ポインタ）。

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

/// `docs`（`embedding VECTOR(2)` + `lang TEXT`）に 2 テナント分の行を仕込む。
/// wire 認証経由の `PolicyContext`（`alice`/`tenant-a`）は `Public` ＋ 自テナント
/// `Private` を許可可視性とする（RLS-11・TASK-195・`auth::verify`）ため、
/// 他テナント（tenant-b）の `Private` 行（id=99）は許可可視性の外にあり
/// `HINT ORDER` のどの並びでも見えてはならない一方、自テナント（tenant-a）
/// の `Private` 行（id=100）は read-your-writes により可視である（RLS 違反
/// ではない）。両行ともクエリベクトルと完全一致（距離 0）させ、暗黙 RLS 事前
/// フィルタが `HINT ORDER` で外れて他テナント行が混入すれば結果集合の先頭に
/// 現れて即座に検出できるようにする（`crates/engine/tests/
/// sql_evaluation_order.rs::
/// hybrid_search_succeeds_and_stays_rls_clean_across_all_six_orders` と同型の
/// 構成）。
fn new_core_two_tenant_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-hint-order-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");

    let ctx_a =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let tenant_a_rows: Vec<(u64, [f32; 2], &str)> = vec![
        (1, [0.9, 0.1], "ja"),
        (2, [0.0, 1.0], "ja"),
        (3, [0.1, 0.9], "en"),
    ];
    for (id, emb, lang) in &tenant_a_rows {
        let op_id = OperationId::parse(&format!("wire-hint-a-{id}")).expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx_a,
            *id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(lang.to_string())],
            &op_id,
        )
        .expect("insert tenant-a row");
    }

    // tenant-b の Private 行（他テナント・距離 0）。
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx_b,
        99,
        Visibility::Private,
        &[Value::Vector(vec![1.0, 0.0]), Value::Text("fr".to_string())],
        &OperationId::parse("wire-hint-b-99").expect("valid operation_id"),
    )
    .expect("insert tenant-b row");

    // tenant-a 自身の Private 行（RLS-11・TASK-195 により wire ctx でも
    // read-your-writes として可視。距離 0 のため既定順序では先頭に来る）。
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx_a,
        100,
        Visibility::Private,
        &[Value::Vector(vec![1.0, 0.0]), Value::Text("de".to_string())],
        &OperationId::parse("wire-hint-a-100").expect("valid operation_id"),
    )
    .expect("insert tenant-a private row");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn_with_alice(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

const QUERY_VECTOR: &str = "'[1.0,0.0]'";

/// 既定順序（`HINT` なし）と `HINT ORDER(RLS, SCALAR, DISTANCE)` 明示は wire 応答
/// （`RowDescription`/`DataRow`/`CommandComplete`）が完全一致する。いずれの
/// 結果にも tenant-b の id=99 は混入しない。
#[test]
fn wire_hint_order_default_matches_explicit_rls_scalar_distance() {
    let (core, _guard) = new_core_two_tenant_docs();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        &format!("SELECT * FROM docs ORDER BY embedding <=> {QUERY_VECTOR} LIMIT 10"),
    );
    let default_columns = read_row_description(&mut stream);
    let mut default_ids = Vec::new();
    for _ in 0..4 {
        let row = read_data_row(&mut stream);
        default_ids.push(row[0].clone().expect("id is not null"));
    }
    let default_tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        &format!(
            "SELECT * FROM docs ORDER BY embedding <=> {QUERY_VECTOR} LIMIT 10 HINT ORDER(RLS, SCALAR, DISTANCE)"
        ),
    );
    let explicit_columns = read_row_description(&mut stream);
    let mut explicit_ids = Vec::new();
    for _ in 0..4 {
        let row = read_data_row(&mut stream);
        explicit_ids.push(row[0].clone().expect("id is not null"));
    }
    let explicit_tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    assert_eq!(default_columns, explicit_columns);
    assert_eq!(default_ids, explicit_ids);
    assert_eq!(default_tag, explicit_tag);
    assert_eq!(default_tag, "SELECT 4");
    // id=100 は自テナント（tenant-a）の Private 行で read-your-writes により
    // 可視（RLS-11・TASK-195）。距離 0 のため既定順序の先頭に来る。
    assert!(
        default_ids.contains(&"100".to_string()),
        "own-tenant Private row (id=100) must be visible via read-your-writes, got {default_ids:?}"
    );
    assert!(
        !default_ids.contains(&"99".to_string()),
        "other-tenant Private row (id=99) must never appear in tenant-a's wire result, got {default_ids:?}"
    );
}

/// `DISTANCE` を先頭に置く許可順列（`HINT ORDER(DISTANCE, SCALAR, RLS)`）でも
/// 暗黙 RLS 事前フィルタは wire 越しに外れず、id=99（他テナント Private・
/// 距離 0・最近傍）・id=100（自テナント Private・未許可可視性・距離 0）は
/// いずれも混入しない。
#[test]
fn wire_hint_order_distance_first_does_not_leak_other_tenant_row() {
    let (core, _guard) = new_core_two_tenant_docs();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        &format!(
            "SELECT * FROM docs ORDER BY embedding <=> {QUERY_VECTOR} LIMIT 10 HINT ORDER(DISTANCE, SCALAR, RLS)"
        ),
    );
    read_row_description(&mut stream);
    let mut ids = Vec::new();
    for _ in 0..4 {
        let row = read_data_row(&mut stream);
        ids.push(row[0].clone().expect("id is not null"));
    }
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 4");
    read_ready_for_query(&mut stream);

    // id=100（自テナント Private・read-your-writes）は可視、id=99（他テナント
    // Private）は不可視のまま（RLS-11・TASK-195）。
    assert!(
        ids.contains(&"100".to_string()),
        "own-tenant Private row (id=100) must be visible via read-your-writes, got {ids:?}"
    );
    assert!(
        !ids.contains(&"99".to_string()),
        "DISTANCE-first HINT ORDER must not leak other-tenant Private row (id=99) over wire, got {ids:?}"
    );
}

/// 段の省略・重複・未知トークンはいずれも `42601` で拒否され、接続は維持される
/// （続くクエリが応答すること）。
#[test]
fn wire_hint_order_malformed_forms_are_rejected_and_connection_survives() {
    let (core, _guard) = new_core_two_tenant_docs();
    let mut stream = spawn_with_alice(core);

    let malformed = [
        format!("SELECT * FROM docs ORDER BY embedding <=> {QUERY_VECTOR} LIMIT 10 HINT ORDER(RLS, SCALAR)"),
        format!(
            "SELECT * FROM docs ORDER BY embedding <=> {QUERY_VECTOR} LIMIT 10 HINT ORDER(RLS, RLS, SCALAR)"
        ),
        format!(
            "SELECT * FROM docs ORDER BY embedding <=> {QUERY_VECTOR} LIMIT 10 HINT ORDER(RLS, SCALAR, ATTACKER)"
        ),
        format!("SELECT * FROM docs ORDER BY embedding <=> {QUERY_VECTOR} LIMIT 10 HINT ORDER()"),
    ];
    for sql in &malformed {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, "42601");
        read_ready_for_query(&mut stream);
    }

    // エラー後も接続は維持され、続くクエリが正常応答すること。
    send_simple_query(
        &mut stream,
        &format!("SELECT * FROM docs ORDER BY embedding <=> {QUERY_VECTOR} LIMIT 10"),
    );
    read_row_description(&mut stream);
    for _ in 0..4 {
        read_data_row(&mut stream);
    }
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 4");
    read_ready_for_query(&mut stream);
}
