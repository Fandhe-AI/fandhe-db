//! `EXPLAIN SELECT ... USING PLAN('<query>')` の簡易クエリプロトコル経由（生バイト
//! クライアント）検証（TASK-78、対象ビヘイビア: SQL-6。ポインタ:
//! `docs/spec/05-tasks.md` TASK-78・`docs/spec/04-behavior/sql-surface.md` SQL-6）。
//!
//! LLM 展開・モード解決の規則そのものは `crates/engine/tests/sql_explain.rs`
//! （in-process）が確定オラクルとして検証済みのため、本ファイルは同じ規則が
//! **wire フレーミング** 越しに `RowDescription`（`QUERY PLAN` 単一列）・
//! `DataRow`（決定的な行）・`CommandComplete`（`EXPLAIN` タグ・行数無し）として
//! 観測できることの確認に徹する（`wire_search_mode.rs` と同じ流儀。
//! `common/mod.rs` は `tests/wire_extended_query.rs` 用のヘルパーを共有する）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::*;

struct StubLlmClient {
    response: &'static str,
}

impl LlmClient for StubLlmClient {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(self.response.to_string())
    }
}

const EXPANSION_RESPONSE: &str =
    r#"{"search_terms": ["alpha", "beta"], "path_hint": "docs/", "kind_hint": "fn"}"#;
/// Issue #411 の HNSW opt-in テストで使う、検索語・ソフトヒントを持たない展開結果
/// （行数を固定しやすくするため。`crates/engine/tests/sql_explain.rs::
/// EXPANSION_RESPONSE_NO_HINTS` と同構成）。
const EXPANSION_RESPONSE_NO_HINTS: &str =
    r#"{"search_terms": [], "path_hint": null, "kind_hint": null}"#;

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-explain-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("docs/a.md".to_string()),
            Value::Text("alpha content".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("insert row");

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_query_planner(
        Box::new(StubLlmClient {
            response: EXPANSION_RESPONSE,
        }),
    );
    (Arc::new(core), guard)
}

fn spawn_with_alice(core: Arc<EngineCore>) -> (std::net::TcpStream, std::path::PathBuf) {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    (
        authenticate_to_ready_for_query(addr, "alice", "pw-alice"),
        users_path,
    )
}

/// SQL-6: `EXPLAIN` は `QUERY PLAN` 単一列の `RowDescription` と、検索語・
/// ソフトヒント・実効モード・指定元の決定的な行を返し、`CommandComplete` タグは
/// 行数を付けない `EXPLAIN`（pg 互換）になる。
#[test]
fn explain_returns_query_plan_column_and_expected_rows() {
    let (core, _guard) = new_core_with_docs_table();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
    );

    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["QUERY PLAN".to_string()]);

    let expected_lines = [
        "search_terms[0]: alpha",
        "search_terms[1]: beta",
        "path_hint: docs/",
        "kind_hint: fn",
        "mode: recall",
        "mode_source: default",
        // Issue #411: `new_core_with_docs_table` は `EngineCore::from_storage`
        // （provider 直接注入・`kind` 不明）経由のため `engine: (custom_provider)`。
        // codex-review P1 指摘対応（PR #437）: 実行方式を判別できないため
        // `ann_plan:` も `unknown_custom_provider`。
        "engine: (custom_provider)",
        "ann_plan: unknown_custom_provider",
        "scalar_plan: plain_scan",
    ];
    for expected in expected_lines {
        let row = read_data_row(&mut stream);
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].as_deref(), Some(expected));
    }

    assert_eq!(read_command_complete(&mut stream), "EXPLAIN");
    read_ready_for_query(&mut stream);
}

/// SQL-6 × SQL-12: `EXPLAIN` は明示 `USING MODE` の実効モード・指定元
/// （`query_clause`）も可視化する。
#[test]
fn explain_reports_query_clause_mode_source() {
    let (core, _guard) = new_core_with_docs_table();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10 USING MODE 'precision'",
    );

    let _columns = read_row_description(&mut stream);
    let mut rows = Vec::new();
    for _ in 0..9 {
        rows.push(read_data_row(&mut stream)[0].clone().expect("cell"));
    }
    assert_eq!(rows[4], "mode: precision");
    assert_eq!(rows[5], "mode_source: query_clause");
    assert_eq!(rows[6], "engine: (custom_provider)");
    assert_eq!(rows[7], "ann_plan: unknown_custom_provider");
    assert_eq!(rows[8], "scalar_plan: plain_scan");

    assert_eq!(read_command_complete(&mut stream), "EXPLAIN");
    read_ready_for_query(&mut stream);
}

/// Issue #922（SQL-27）: `EXPLAIN` の対象を通常検索へ拡大したため、`USING
/// PLAN` を伴わない通常 `SELECT` への前置はもはや拒否されず受理される
/// （受理テストへ反転。LLM I/O を行わないため `search_terms`／`path_hint`／
/// `kind_hint` を含まない、`mode` から始まる短い行のみを返す）。
#[test]
fn explain_accepts_plain_select_without_using_plan() {
    let (core, _guard) = new_core_with_docs_table();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "EXPLAIN SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
    );

    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["QUERY PLAN".to_string()]);

    let expected_lines = [
        "mode: recall",
        "mode_source: default",
        "engine: (custom_provider)",
        "ann_plan: unknown_custom_provider",
        "scalar_plan: plain_scan",
    ];
    for expected in expected_lines {
        let row = read_data_row(&mut stream);
        assert_eq!(row.len(), 1);
        assert_eq!(row[0].as_deref(), Some(expected));
    }

    assert_eq!(read_command_complete(&mut stream), "EXPLAIN");
    read_ready_for_query(&mut stream);
}

fn new_hnsw_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-explain-hnsw-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        1,
        Visibility::Public,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("docs/a.md".to_string()),
            Value::Text("alpha content".to_string()),
        ],
        &engine::recovery::required_op_id::OperationId::parse("test-op")
            .expect("valid operation_id"),
    )
    .expect("insert row");

    let kind = engine::search_engine::hnsw_kind(engine::hnsw::HnswParams::default())
        .expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind).with_query_planner(Box::new(
        StubLlmClient {
            response: EXPANSION_RESPONSE_NO_HINTS,
        },
    ));
    (Arc::new(core), guard)
}

/// Issue #411: HNSW opt-in エンジンでは `engine: hnsw`・`hnsw_params:`（既定値）・
/// `ann_plan: hnsw_full_visible`（`WHERE` なし）を返す。
#[test]
fn explain_reports_hnsw_engine_and_full_visible_ann_plan() {
    let (core, _guard) = new_hnsw_core_with_docs_table();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
    );

    let _columns = read_row_description(&mut stream);
    let mut rows = Vec::new();
    for _ in 0..8 {
        rows.push(read_data_row(&mut stream)[0].clone().expect("cell"));
    }
    assert_eq!(rows[4], "engine: hnsw");
    assert_eq!(
        rows[5],
        "hnsw_params: m=16,ef_construction=100,ef_search=64,resident=f32,sparse_visited_max=0"
    );
    assert_eq!(rows[6], "ann_plan: hnsw_full_visible");
    assert_eq!(rows[7], "scalar_plan: plain_scan");

    assert_eq!(read_command_complete(&mut stream), "EXPLAIN");
    read_ready_for_query(&mut stream);
}

/// Issue #411: HNSW opt-in でも `USING MODE 'precision'` は
/// `ann_plan: plain_scan_precision`（TASK-162・SEARCH-9。厳密 brute-force 経路
/// を常に使う）。
#[test]
fn explain_reports_plain_scan_precision_ann_plan_for_hnsw_precision_mode() {
    let (core, _guard) = new_hnsw_core_with_docs_table();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10 USING MODE 'precision'",
    );

    let _columns = read_row_description(&mut stream);
    let mut rows = Vec::new();
    for _ in 0..8 {
        rows.push(read_data_row(&mut stream)[0].clone().expect("cell"));
    }
    assert_eq!(rows[4], "engine: hnsw");
    assert_eq!(rows[6], "ann_plan: plain_scan_precision");
    assert_eq!(rows[7], "scalar_plan: plain_scan");

    assert_eq!(read_command_complete(&mut stream), "EXPLAIN");
    read_ready_for_query(&mut stream);
}
