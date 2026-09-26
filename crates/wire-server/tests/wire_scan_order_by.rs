//! 広域取得へのスカラー列 `ORDER BY`（Issue #915・SQL-25・TASK-209）が
//! PostgreSQL wire プロトコル v3 の簡易クエリ経路（生バイトクライアント）で
//! 契約どおりの応答として観測できることを検証する結合テスト（層 A）。
//!
//! 実行契約そのもの（並び順の正しさ・NULL 位置・RLS 非漏えい・経路 (A)／(B) の
//! 等価性）は `crates/engine/tests/sql25_scalar_order_by.rs`（in-process）が
//! 既に確定オラクルとして検証済みのため、本ファイルは同じ規則を **wire
//! フレーミング** 越しに再確認することに絞る（`wire_scan.rs` と同方針）。

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

/// `docs(embedding VECTOR(2) NOT NULL, lang TEXT NULL)`。alice（tenant-a）から
/// 見える行: id=1 lang="c", id=2 lang="a", id=3 lang="b"（いずれも Public）。
fn new_core_scan_docs() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-scan-order-by-docs");
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

    let rows: [(u64, &str); 3] = [(1, "c"), (2, "a"), (3, "b")];
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for (id, lang) in rows {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(vec![0.0, 0.0]), Value::Text(lang.to_string())],
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

/// Issue #915: `ORDER BY <col> LIMIT n` が `lang` 昇順で行を返す。
#[test]
fn scalar_order_by_ascending_returns_rows_in_order_over_wire() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT id, lang FROM docs ORDER BY lang LIMIT 10",
    );
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id", "lang"]);

    let mut ids: Vec<String> = Vec::new();
    for _ in 0..3 {
        let row = read_data_row(&mut stream);
        ids.push(row[0].clone().expect("id must not be NULL"));
    }
    assert_eq!(ids, vec!["2", "3", "1"]); // lang: a, b, c
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// Issue #915: `ORDER BY <col> DESC LIMIT n` が降順で行を返す。
#[test]
fn scalar_order_by_descending_returns_rows_in_reverse_order_over_wire() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs ORDER BY lang DESC LIMIT 10",
    );
    let _columns = read_row_description(&mut stream);
    let mut ids: Vec<String> = Vec::new();
    for _ in 0..3 {
        let row = read_data_row(&mut stream);
        ids.push(row[0].clone().expect("id must not be NULL"));
    }
    assert_eq!(ids, vec!["1", "3", "2"]); // lang: c, b, a
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// Issue #915・§受入基準 3: スカラー順序付けとベクトル順位付けの併用は `42601`。
#[test]
fn mixing_scalar_order_by_with_vector_ranking_rejects_with_42601() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs ORDER BY lang, embedding <=> '[1,0]' LIMIT 5",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}

/// Issue #915: 未知列は `22000`。
#[test]
fn unknown_order_by_column_rejects_with_22000() {
    let (core, _guard) = new_core_scan_docs();
    let (mut stream, _users_path) = spawn_with_alice(core);

    send_simple_query(&mut stream, "SELECT id FROM docs ORDER BY nope LIMIT 5");
    expect_error_response_with_sqlstate(&mut stream, "22000");
    read_ready_for_query(&mut stream);
}
