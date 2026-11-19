//! `COPY (<集計 SELECT>) TO STDOUT`（Issue #3253・SQL-34・WIRE-22 のポインタ）の簡易クエリ
//! プロトコル経由（生バイトクライアント）検証。意味論（通常 SELECT との同値性・行数上限・
//! fail-closed）は engine 側 `copy_to_aggregate.rs` が確定オラクルのため、本ファイルは wire
//! フレーミング越しに観測できること（psql 形の末尾 `;`・CSV・上限超過が CopyOutResponse より前に
//! ErrorResponse になること）に集中する。

use crate::common;

use crate::temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::result_limits::ResultLimits;
use engine::storage::Storage;

use common::*;

fn new_core(limit: usize) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-copy-to-aggregate");
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

fn connect(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

fn seed(stream: &mut std::net::TcpStream) {
    for (id, lang) in [(1, "ja"), (2, "ja"), (3, "en")] {
        send_simple_query(
            stream,
            &format!("INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[1.0,0.0]', '{lang}')"),
        );
        let _ = read_command_complete(stream);
        read_ready_for_query(stream);
    }
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

/// 単一行集計（psql 形の末尾 `;` 付き）が 1 行を出力する。
#[test]
fn wire_copy_to_count_star_with_trailing_semicolon() {
    let (core, _guard) = new_core(100);
    let mut alice = connect(core);
    seed(&mut alice);
    send_simple_query(&mut alice, "COPY (SELECT count(*) FROM docs) TO STDOUT;");
    let (ncols, rows, tag) = read_copy_output(&mut alice);
    assert_eq!(ncols, 1);
    assert_eq!(rows, vec![b"3\n".to_vec()]);
    assert_eq!(tag, "COPY 1");
}

/// GROUP BY・ORDER BY の集計を CSV（ヘッダ付き）で出力する。
#[test]
fn wire_copy_to_grouped_aggregate_csv_with_header() {
    let (core, _guard) = new_core(100);
    let mut alice = connect(core);
    seed(&mut alice);
    send_simple_query(
        &mut alice,
        "COPY (SELECT lang, count(*) FROM docs GROUP BY lang ORDER BY lang) \
         TO STDOUT WITH (FORMAT csv, HEADER)",
    );
    let (ncols, rows, tag) = read_copy_output(&mut alice);
    assert_eq!(ncols, 2);
    assert_eq!(
        rows,
        vec![
            b"lang,count\n".to_vec(),
            b"en,1\n".to_vec(),
            b"ja,2\n".to_vec()
        ]
    );
    assert_eq!(tag, "COPY 2");
}

/// 行数上限超過は最初のメッセージが ErrorResponse（54000）で、接続は維持される。
#[test]
fn wire_copy_to_aggregate_over_limit_errors_before_copy_out_response() {
    let (core, _guard) = new_core(1);
    let mut alice = connect(core);
    seed(&mut alice);
    send_simple_query(
        &mut alice,
        "COPY (SELECT lang, count(*) FROM docs GROUP BY lang) TO STDOUT",
    );
    let (ty, body) = read_message(&mut alice);
    assert_eq!(
        ty, b'E',
        "first message must be ErrorResponse, not CopyOutResponse"
    );
    let text = String::from_utf8_lossy(&body).into_owned();
    assert!(text.contains("54000"), "{text:?}");
    assert!(!text.contains("tenant-a"), "tenant id leaked: {text:?}");
    assert_eq!(read_ready_for_query_status(&mut alice), b'I');

    // 接続維持: 単一行集計は上限 1 でも通る。
    send_simple_query(&mut alice, "COPY (SELECT count(*) FROM docs) TO STDOUT");
    let (_, rows, tag) = read_copy_output(&mut alice);
    assert_eq!(rows.len(), 1);
    assert_eq!(tag, "COPY 1");
}
