//! `COPY ... FROM STDIN` の INDEX-4 ②（1 行あたりの本文サイズ）・④（生成
//! チャンク数）上限の wire 経由確認（Issue #1178・WIRE-17）。ポインタ:
//! `docs/spec/04-behavior/wire-protocol.md` WIRE-17・`docs/spec/04-behavior/index.md`
//! INDEX-4。
//!
//! engine 側の判定順・境界は `crates/engine/tests/copy_from.rs` が確定オラクル
//! のため、本ファイルは「上限超過が `54000` の ErrorResponse になり、行が 1 件も
//! 残らず（副作用ゼロ）、接続が維持される」ことを CopyData／CopyDone のフレーミング
//! 越しに固定する。`BatchLimits` は環境変数（`VECTOR_DB_BATCH_MAX_*`）由来の既定値
//! の揺らぎを避けるため 4 項目すべてを明示する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::batch_limits::BatchLimits;
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

/// 1 行の判定対象量 = `VECTOR(2)` の 8 バイト + `lang` の長さ（`id` 疑似列は 0）。
const ROW_BODY_LIMIT: usize = 18;

fn new_core(limits: BatchLimits) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire17-copy-limits");
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
    let core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(limits);
    (Arc::new(core), guard)
}

fn body_limits() -> BatchLimits {
    BatchLimits {
        max_files_per_batch: 64,
        max_file_body_bytes: ROW_BODY_LIMIT,
        max_batch_total_bytes: 1024 * 1024,
        max_batch_chunks: 64,
    }
}

fn chunk_limits() -> BatchLimits {
    // ④ < ① にして ① が先に発火しないようにする。
    BatchLimits {
        max_files_per_batch: 64,
        max_file_body_bytes: 1024 * 1024,
        max_batch_total_bytes: 1024 * 1024,
        max_batch_chunks: 2,
    }
}

fn connect(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

fn start_copy(stream: &mut std::net::TcpStream, op: &str) {
    send_simple_query(
        stream,
        &format!("COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID '{op}'"),
    );
    let (ty, _) = read_message(stream);
    assert_eq!(ty, b'G', "expected CopyInResponse");
}

fn send_copy_data(stream: &mut std::net::TcpStream, chunk: &[u8]) {
    send_length_prefixed_message(stream, b'd', chunk);
}

fn send_copy_done(stream: &mut std::net::TcpStream) {
    send_length_prefixed_message(stream, b'c', b"");
}

/// ErrorResponse を 1 つ読み、SQLSTATE `54000` とメッセージ断片、および本文・
/// tenant ID を含まないことを確認する。
fn expect_limit_error(stream: &mut std::net::TcpStream, message_fragment: &str) {
    let (ty, body) = read_message(stream);
    assert_eq!(ty, b'E', "expected ErrorResponse");
    let text = String::from_utf8_lossy(&body).into_owned();
    assert!(text.contains("54000"), "expected 54000, got: {text:?}");
    assert!(
        text.contains(message_fragment),
        "expected message containing {message_fragment:?}, got: {text:?}"
    );
    assert!(!text.contains("tenant-a"), "tenant id leaked: {text:?}");
    assert!(!text.contains("elevenbytes"), "row body leaked: {text:?}");
}

/// 副作用ゼロ（行 0 件）と接続維持（直後の簡易クエリが成功）を確認する。
fn assert_no_rows_and_connection_alive(stream: &mut std::net::TcpStream) {
    send_simple_query(stream, "SELECT id FROM docs LIMIT 10");
    let _ = read_row_description(stream);
    assert_eq!(read_command_complete(stream), "SELECT 0");
    read_ready_for_query(stream);
}

#[test]
fn wire17_copy_row_body_at_limit_is_accepted() {
    let (core, _guard) = new_core(body_limits());
    let mut stream = connect(core);
    start_copy(&mut stream, "wire-1178-body-at");
    // `lang` が 10 バイト（判定対象量 18 = 上限ちょうど）。
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\ttenbytes10\n");
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire17_copy_row_body_over_limit_is_54000_with_zero_side_effects() {
    let (core, _guard) = new_core(body_limits());
    let mut stream = connect(core);
    start_copy(&mut stream, "wire-1178-body-over");
    // 正常な 2 行 + `lang` が 11 バイト（判定対象量 19 > 18）の 3 行目。
    send_copy_data(
        &mut stream,
        b"1\t[1.0,0.0]\tja\n2\t[0.0,1.0]\tja\n3\t[1.0,1.0]\televenbytes\n",
    );
    send_copy_done(&mut stream);
    expect_limit_error(&mut stream, "body size");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_no_rows_and_connection_alive(&mut stream);
}

#[test]
fn wire17_copy_rows_at_chunk_limit_are_accepted() {
    let (core, _guard) = new_core(chunk_limits());
    let mut stream = connect(core);
    start_copy(&mut stream, "wire-1178-chunk-at");
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n2\t[0.0,1.0]\tja\n");
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 2");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire17_copy_rows_over_chunk_limit_are_54000_with_zero_side_effects() {
    let (core, _guard) = new_core(chunk_limits());
    let mut stream = connect(core);
    start_copy(&mut stream, "wire-1178-chunk-over");
    send_copy_data(
        &mut stream,
        b"1\t[1.0,0.0]\tja\n2\t[0.0,1.0]\tja\n3\t[1.0,1.0]\tja\n",
    );
    send_copy_done(&mut stream);
    expect_limit_error(&mut stream, "batch chunk count");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_no_rows_and_connection_alive(&mut stream);
}

/// 上限超過後に届く CopyData は読み捨てられ、ErrorResponse は 1 回だけ返る。
#[test]
fn wire17_copy_chunk_limit_error_discards_subsequent_copy_data() {
    let (core, _guard) = new_core(chunk_limits());
    let mut stream = connect(core);
    start_copy(&mut stream, "wire-1178-chunk-discard");
    send_copy_data(
        &mut stream,
        b"1\t[1.0,0.0]\tja\n2\t[0.0,1.0]\tja\n3\t[1.0,1.0]\tja\n",
    );
    for _ in 0..4 {
        send_copy_data(&mut stream, b"4\t[1.0,0.0]\tja\n");
    }
    send_copy_done(&mut stream);
    expect_limit_error(&mut stream, "batch chunk count");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_no_rows_and_connection_alive(&mut stream);
}
