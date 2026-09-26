//! `SELECT DISTINCT`・`COUNT(DISTINCT <expr>)`（SQL-25 (c)・TASK-209）が
//! PostgreSQL wire プロトコル v3 の簡易クエリ経路（生バイトクライアント）で
//! 契約どおりの応答（`RowDescription`／`DataRow`／`CommandComplete`／
//! `ErrorResponse` の SQLSTATE）として観測できることを検証する結合テスト
//! （ポインタ: `docs/spec/05-tasks.md` TASK-209、
//! `docs/spec/04-behavior/sql-surface.md` SQL-25 (c)（関連: SQL-8・SQL-13・
//! SQL-14）、`docs/spec/04-behavior/rls.md` RLS-7・RLS-8）。
//!
//! 集計値・拒否形状そのものは `crates/engine/tests/sql25_distinct.rs`
//! （in-process）が既に確定オラクルとして検証済みのため、本ファイルは同じ
//! 規則を **wire フレーミング** 越しに再確認することに徹する（`wire_aggregate.rs`
//! と同じ既存フィクスチャ・オラクル値を再利用し、独自の再計算はしない）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::*;

/// `wire_aggregate.rs::new_core_aggregate_docs` と同一のフィクスチャ
/// （`docs(embedding VECTOR(2), lang TEXT)`。alice〔tenant-a〕の可視行は
/// Public 3 件 + 自テナント Private 1 件の計 4 件: id=1 (tenant-a, "ja") /
/// id=2 (tenant-b, "en") / id=3 (tenant-c, "ja") / id=11 (tenant-a, "xx")）。
/// `lang` の異なり値は `{en, ja, xx}` の 3 件、`COUNT(DISTINCT lang)=3`、
/// `COUNT(DISTINCT id)=4`（`id` に重複はない）。`"xx"` は自テナントの Private
/// 行にしか存在しない値のため、`SELECT DISTINCT lang` に現れても RLS 違反では
/// ない（read-your-writes。RLS-11・TASK-195）が、他テナント専有の値が
/// 混入していないことの対照として使う。
fn new_core_aggregate_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-sql25-distinct-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    let public_rows: [(&str, u64, [f32; 2], &str); 3] = [
        ("tenant-a", 1, [1.0, 0.0], "ja"),
        ("tenant-b", 2, [0.0, 1.0], "en"),
        ("tenant-c", 3, [-1.0, 0.0], "ja"),
    ];
    for (tenant, id, emb, lang) in public_rows {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(lang.to_string())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert public row");
    }

    let private_rows: [(&str, u64, [f32; 2]); 2] =
        [("tenant-a", 11, [1.0, 0.0]), ("tenant-b", 12, [0.0, 1.0])];
    for (tenant, id, emb) in private_rows {
        let ctx =
            PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
                .expect("valid tenant");
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Private,
            &[Value::Vector(emb.to_vec()), Value::Text("xx".to_string())],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert private row");
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn connect_alice(addr: std::net::SocketAddr) -> std::net::TcpStream {
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

fn spawn_with_alice(core: Arc<EngineCore>) -> (std::net::TcpStream, std::path::PathBuf) {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    (connect_alice(addr), users_path)
}

/// SQL-25 (c): `SELECT DISTINCT <TEXT 列>` が RowDescription・DataRow で
/// 異なり値（キー昇順）を返す。
#[test]
fn select_distinct_returns_key_ascending_rows_over_wire() {
    let (core, _guard) = new_core_aggregate_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "SELECT DISTINCT lang FROM docs");
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["lang"]);
    let mut rows = Vec::new();
    for _ in 0..3 {
        let row = read_data_row(&mut stream);
        rows.push(row[0].clone().expect("lang is not nullable"));
    }
    assert_eq!(
        rows,
        vec!["en", "ja", "xx"],
        "distinct values must be key-ascending (own-tenant Private row's \"xx\" included)"
    );
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// SQL-25 (c): `WHERE`／`ORDER BY ... DESC`／`LIMIT` を伴う `SELECT DISTINCT`。
#[test]
fn select_distinct_with_where_order_by_desc_and_limit_over_wire() {
    let (core, _guard) = new_core_aggregate_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT DISTINCT lang FROM docs ORDER BY lang DESC LIMIT 1",
    );
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("xx"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// SQL-25 (c): `COUNT(DISTINCT lang)`／`COUNT(DISTINCT id)` が別名どおりの列名・
/// オラクルどおりの値で返る。
#[test]
fn count_distinct_returns_expected_cardinality_over_wire() {
    let (core, _guard) = new_core_aggregate_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT COUNT(DISTINCT lang) AS n_lang, COUNT(DISTINCT id) AS n_id FROM docs",
    );
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["n_lang", "n_id"]);
    let row = read_data_row(&mut stream);
    let actual: Vec<Option<&str>> = row.iter().map(|c| c.as_deref()).collect();
    assert_eq!(actual, vec![Some("3"), Some("4")]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// SQL-25 (c): 空集合（`WHERE` が可視行に一切一致しない）でも `COUNT(DISTINCT)`
/// は `0`（PostgreSQL 互換の空集合契約。既存の `COUNT` と同じ）。
#[test]
fn count_distinct_empty_set_contract_is_zero_over_wire() {
    let (core, _guard) = new_core_aggregate_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT COUNT(DISTINCT lang) AS n FROM docs WHERE lang = 'zz'",
    );
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("0"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// RLS-7・RLS-8: 他テナントにしか存在しない `Private` 行の値が
/// `SELECT DISTINCT`／`COUNT(DISTINCT)` のいずれにも現れない（bob〔tenant-b〕の
/// Private 行 id=12 は alice〔tenant-a〕からは不可視）。
#[test]
fn distinct_and_count_distinct_never_reveal_other_tenants_private_rows() {
    let (core, _guard) = new_core_aggregate_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    // alice の可視 lang は {en, ja, xx}（tenant-b の Private 行 id=12 の embedding
    // は他テナント専有だが lang は同じ "xx" なので、代わりに件数〔4〕で
    // 確認する。tenant-b の Private 行が数に含まれていれば 5 になるはず）。
    send_simple_query(&mut stream, "SELECT COUNT(DISTINCT id) AS n FROM docs");
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(
        row[0].as_deref(),
        Some("4"),
        "tenant-b's private row (id=12) must not be counted for alice"
    );
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// 拒否経路の fail-closed 確認（`wire_aggregate.rs::
/// sql_aggregate_rejections_are_fail_closed_and_connection_survives` と同じ
/// 形式）: 各エラーが期待 SQLSTATE で返り、接続は破棄されず、セッションが
/// 汚染されない。
#[test]
fn sql25_distinct_rejections_are_fail_closed_and_connection_survives() {
    let (core, _guard) = new_core_aggregate_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    for (sql, expected_sqlstate) in [
        ("SELECT COUNT(DISTINCT embedding) FROM docs", "22000"),
        ("SELECT DISTINCT embedding FROM docs", "22000"),
        ("SELECT DISTINCT * FROM docs", "42601"),
        ("SELECT COUNT(DISTINCT *) FROM docs", "42601"),
        ("SELECT SUM(DISTINCT id) FROM docs", "42601"),
        ("SELECT DISTINCT lang FROM docs GROUP BY lang", "42601"),
        (
            "EXPLAIN SELECT DISTINCT lang FROM docs USING PLAN(full_scan())",
            "42601",
        ),
    ] {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, expected_sqlstate);
        read_ready_for_query(&mut stream);
    }

    // セッションが汚染されず、直後の DISTINCT クエリが正常に成功する。
    send_simple_query(&mut stream, "SELECT COUNT(DISTINCT lang) AS n FROM docs");
    let _columns = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("3"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// 後方互換: 列名 `distinct` を持つ表への既存の解釈が wire 越しにも壊れない。
#[test]
fn column_named_distinct_keeps_existing_projection_semantics_over_wire() {
    let path = temp_db::unique_db_path("wire-sql25-distinct-colname");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("distinct", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        1,
        Visibility::Public,
        &[Value::Vector(vec![1.0, 0.0]), Value::Text("x".to_string())],
        &engine::recovery::required_op_id::OperationId::parse("test-op-1")
            .expect("valid operation_id"),
    )
    .expect("insert row");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    let (mut stream, _users_path) = spawn_with_alice(core);
    let _guard = guard;

    send_simple_query(&mut stream, "SELECT distinct FROM docs LIMIT 5");
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["distinct"]);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("x"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}
