//! 広域取得の `OFFSET`（Issue #916。ポインタ: SQL-25 (b)・TASK-209）が
//! PostgreSQL wire プロトコル v3 の簡易クエリ経路（生バイトクライアント）で
//! 契約どおりの応答（`DataRow`／`CommandComplete`／`ErrorResponse` の SQLSTATE）
//! として観測できることを検証する結合テスト（層 A。`wire_scan.rs` と同方針）。
//!
//! 実行契約そのもの（可視行のみを対象とした計数・範囲検証・GROUP BY 適用順）は
//! `crates/engine/tests/sql25_offset.rs`（in-process）が既に確定オラクルとして
//! 検証済みのため、本ファイルは同じ規則を **wire フレーミング** 越しに再確認する
//! ことに徹する（`wire_scan.rs` の fixture・helper をそのまま再利用する）。

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

/// `docs(embedding VECTOR(2), lang TEXT)` を持つ `EngineCore` を新設する。
/// alice（tenant-a）から可視な Public 行を id=1..=5 として書き込む（`OFFSET` の
/// ページング検証に十分な件数）。
fn new_core_scan_docs_for_offset() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-scan-offset-docs");
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

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for id in 1..=5u64 {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![id as f32, 0.0]),
                Value::Text("ja".to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("test-op-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert public row");
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

/// Issue #916: `LIMIT n OFFSET m` が `DataRow`（`LIMIT` 件）／`CommandComplete`
/// として観測でき、返る id が「可視総数から `OFFSET` を引いた残り件数のうち
/// 先頭 `LIMIT` 件」に一致する。
#[test]
fn scan_with_offset_returns_remaining_rows_over_wire() {
    let (core, _guard) = new_core_scan_docs_for_offset();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10 OFFSET 2");
    let _columns = read_row_description(&mut stream);
    let mut ids: Vec<String> = Vec::new();
    for _ in 0..3 {
        let row = read_data_row(&mut stream);
        ids.push(row[0].clone().expect("id must not be NULL"));
    }
    ids.sort();
    assert_eq!(ids, vec!["3", "4", "5"]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// Issue #916: `OFFSET` が可視総数以上なら空集合（`DataRow` 0 件）を返す。
#[test]
fn scan_with_offset_beyond_visible_count_returns_empty_set() {
    let (core, _guard) = new_core_scan_docs_for_offset();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10 OFFSET 5");
    let _columns = read_row_description(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 0");
    read_ready_for_query(&mut stream);
}

/// Issue #916・SQL-25 (b)・TASK-209: `OFFSET` が `MAX_SEARCH_K`（10,000）を
/// 超えると `22000`（範囲外）になる。
#[test]
fn scan_rejects_offset_over_max_search_k_with_22000() {
    let (core, _guard) = new_core_scan_docs_for_offset();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 5 OFFSET 10001");
    expect_error_response_with_sqlstate(&mut stream, "22000");
    read_ready_for_query(&mut stream);
}

/// Issue #916: 検索 SELECT（`ORDER BY` 経路）は wire 越しでも `OFFSET` を
/// 構造上受理しない（`42601`。広域取得のみが対象という契約の wire 越し確認）。
#[test]
fn search_select_with_offset_still_rejected_with_42601_over_wire() {
    let (core, _guard) = new_core_scan_docs_for_offset();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs ORDER BY embedding <=> '[0,0]' LIMIT 5 OFFSET 1",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}

/// Issue #916: `OFFSET` の後ろに余剰トークン（`USING MODE`）が続く形は許可
/// リスト外（`42601`）。
#[test]
fn scan_rejects_trailing_using_mode_after_offset_with_42601() {
    let (core, _guard) = new_core_scan_docs_for_offset();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs LIMIT 5 OFFSET 1 USING MODE 'precision'",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}
