//! ウィンドウ関数（SQL-30・TASK-214、Issue #930）が PostgreSQL wire プロトコル
//! v3 の簡易クエリ経路（生バイトクライアント）で契約どおりの応答
//! （`RowDescription`／`DataRow`／`CommandComplete`／`ErrorResponse` の
//! SQLSTATE）として観測できることを検証する結合テスト（層 A。
//! `docs/design/three-client-e2e-harness.md` 参照）。
//!
//! 実行契約そのもの（値・決定性・拒否マトリクス・RLS 不変性）は
//! `crates/engine/tests/sql30_window.rs`（in-process）が既に確定オラクルとして
//! 検証済みのため、本ファイルは同じ規則を **wire フレーミング** 越しに
//! 再確認することに徹する（`wire_scan.rs`・`wire_aggregate.rs` と同方針）。

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

const TABLE: &str = "docs";

fn new_core_window_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-window-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("score", ColumnType::Integer, false),
            ],
        ))
        .expect("create table");

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let rows: [(u64, &str, i32); 3] = [(1, "ja", 10), (2, "ja", 20), (3, "en", 5)];
    for (id, lang, score) in rows {
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[Value::Text(lang.to_string()), Value::Integer(score)],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
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

/// 簡易クエリ経路で `ROW_NUMBER() OVER (...)` が `RowDescription`（text OID）・
/// `DataRow`・`CommandComplete` として返る。
#[test]
fn row_number_over_wire_returns_expected_rows() {
    let (core, _guard) = new_core_window_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT id, ROW_NUMBER() OVER (PARTITION BY lang ORDER BY score) FROM docs LIMIT 10",
    );
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id", "row_number"]);

    let mut rows: Vec<(String, String)> = Vec::new();
    for _ in 0..3 {
        let row = read_data_row(&mut stream);
        let id = row[0].clone().expect("id must not be NULL");
        let rn = row[1].clone().expect("row_number must not be NULL");
        rows.push((id, rn));
    }
    rows.sort();
    assert_eq!(
        rows,
        vec![
            ("1".to_string(), "1".to_string()),
            ("2".to_string(), "2".to_string()),
            ("3".to_string(), "1".to_string()),
        ]
    );
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// `GROUP BY` とウィンドウ項目の併用は `42601` の `ErrorResponse` になる。
#[test]
fn window_combined_with_group_by_is_rejected_over_wire() {
    let (core, _guard) = new_core_window_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT lang, COUNT(*) OVER () FROM docs GROUP BY lang",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}
