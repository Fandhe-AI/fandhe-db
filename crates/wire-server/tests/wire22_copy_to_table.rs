//! テーブル形 `COPY <table> [(<col>, ...)] TO STDOUT`（Issue #1771・WIRE-22 (5)・SQL-34 の
//! ポインタ）の簡易クエリプロトコル経由（生バイトクライアント）検証。
//!
//! 意味論（括弧形との同値性・行数上限・fail-closed）は `crates/engine/tests/copy_to_table.rs` が
//! 確定オラクルのため、本ファイルは wire フレーミング（CopyOutResponse／CopyData／CopyDone／
//! ErrorResponse の順序）越しに観測できることに集中する。特に上限超過は **CopyOutResponse より前**に
//! ErrorResponse が来て、行が 1 件も送られず接続が維持されること。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::result_limits::ResultLimits;
use engine::storage::Storage;

use common::*;

fn new_core(limit: usize) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire22-copy-to-table");
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
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_result_limits(ResultLimits::new(limit));
    (Arc::new(core), guard)
}

fn connect(core: Arc<EngineCore>, user: &str, pw: &str) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, user, pw)
}

fn insert(stream: &mut std::net::TcpStream, id: u64) {
    send_simple_query(
        stream,
        &format!("INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[1.0,0.0]', 'ja')"),
    );
    let _ = read_command_complete(stream);
    read_ready_for_query(stream);
}

/// `COPY` の出力（CopyOutResponse の列数・全 CopyData 行・完了タグ）を最後まで読む。
fn read_copy_output(stream: &mut std::net::TcpStream) -> (i16, Vec<Vec<u8>>, String) {
    let (ty, head) = read_message(stream);
    assert_eq!(ty, b'H', "expected CopyOutResponse");
    let ncols = i16::from_be_bytes([head[1], head[2]]);
    let mut rows = Vec::new();
    loop {
        let (ty, body) = read_message(stream);
        match ty {
            b'd' => rows.push(body),
            b'c' => break,
            other => panic!("unexpected message {:?}", other as char),
        }
    }
    let tag = read_command_complete(stream);
    read_ready_for_query(stream);
    (ncols, rows, tag)
}

/// WIRE-22 (5): テーブル形の出力フレームは括弧形と一致する（全列・列リスト・CSV）。
#[test]
fn wire22_table_form_frames_match_parenthesized_form() {
    let (core, _guard) = new_core(100);
    let mut alice = connect(core, "alice", "pw-alice");
    for id in 1..=3 {
        insert(&mut alice, id);
    }
    for (table_sql, paren_sql) in [
        (
            "COPY docs TO STDOUT",
            "COPY (SELECT * FROM docs LIMIT 100) TO STDOUT",
        ),
        (
            "COPY docs (lang, id) TO STDOUT",
            "COPY (SELECT lang, id FROM docs LIMIT 100) TO STDOUT",
        ),
        (
            "COPY docs (id, lang) TO STDOUT WITH (FORMAT csv)",
            "COPY (SELECT id, lang FROM docs LIMIT 100) TO STDOUT WITH (FORMAT csv)",
        ),
    ] {
        send_simple_query(&mut alice, table_sql);
        let table = read_copy_output(&mut alice);
        send_simple_query(&mut alice, paren_sql);
        let paren = read_copy_output(&mut alice);
        assert_eq!(table, paren, "{table_sql}");
        assert_eq!(table.1.len(), 3);
        assert_eq!(table.2, "COPY 3");
    }
}

/// WIRE-22 (5)・SQL-34: 上限超過は最初のメッセージが ErrorResponse（54000）で、接続は維持される。
#[test]
fn wire22_table_form_over_limit_errors_before_copy_out_response() {
    let (core, _guard) = new_core(2);
    let mut alice = connect(core, "alice", "pw-alice");
    for id in 1..=3 {
        insert(&mut alice, id);
    }
    send_simple_query(&mut alice, "COPY docs TO STDOUT");
    let (ty, body) = read_message(&mut alice);
    assert_eq!(
        ty, b'E',
        "first message must be ErrorResponse, not CopyOutResponse"
    );
    let text = String::from_utf8_lossy(&body).into_owned();
    assert!(text.contains("54000"), "{text:?}");
    assert!(!text.contains("tenant-a"), "tenant id leaked: {text:?}");
    assert_eq!(read_ready_for_query_status(&mut alice), b'I');

    // 接続維持: 上限内の括弧形がそのまま動く。
    send_simple_query(&mut alice, "COPY (SELECT id FROM docs LIMIT 2) TO STDOUT");
    let (_, rows, tag) = read_copy_output(&mut alice);
    assert_eq!(rows.len(), 2);
    assert_eq!(tag, "COPY 2");
}

/// テナント境界と不在の表: 他テナントの行は出ず、不在の表は 42P01。
#[test]
fn wire22_table_form_is_tenant_scoped_and_missing_table_is_42p01() {
    let (core, _guard) = new_core(100);
    let mut alice = connect(Arc::clone(&core), "alice", "pw-alice");
    insert(&mut alice, 1);
    let mut bob = connect(core, "bob", "pw-bob");
    insert(&mut bob, 2);

    send_simple_query(&mut alice, "COPY docs (id) TO STDOUT");
    let (ncols, rows, tag) = read_copy_output(&mut alice);
    assert_eq!(ncols, 1);
    assert_eq!(rows, vec![b"1\n".to_vec()]);
    assert_eq!(tag, "COPY 1");

    send_simple_query(&mut alice, "COPY nope TO STDOUT");
    expect_error_response_with_sqlstate(&mut alice, "42P01");
    read_ready_for_query(&mut alice);
}
