//! `COPY ... FROM STDIN` の INDEX-4 ②（1 行あたりの本文サイズ）・④（生成
//! チャンク数）上限の wire 経由確認（Issue #1178・WIRE-17）。ポインタ:
//! `docs/spec/04-behavior/wire-protocol.md` WIRE-17・`docs/spec/04-behavior/index.md`
//! INDEX-4。
//!
//! engine 側の判定順・境界は `crates/engine/tests/copy_from.rs` が確定オラクル
//! のため、本ファイルは「上限超過が `54000` の ErrorResponse になり、行が 1 件も
//! 残らず（副作用ゼロ）、接続が維持される」ことを CopyData／CopyDone のフレーミング
//! 越しに固定する。`BatchLimits` は環境変数（`FANDHE_DB_BATCH_MAX_*`）由来の既定値
//! の揺らぎを避けるため 4 項目すべてを明示する。

use crate::common;

use crate::temp_db;

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
    // 行数の拒否は ④ で行う（① は COPY の行数に掛からない。Issue #3309）。
    BatchLimits {
        max_files_per_batch: 64,
        max_file_body_bytes: 1024 * 1024,
        max_batch_total_bytes: 1024 * 1024,
        max_batch_chunks: 2,
    }
}

/// 行本文の上限を緩めた構成（① は既定の 64 のまま）。
fn wide_row_limits() -> BatchLimits {
    BatchLimits {
        max_files_per_batch: 64,
        max_file_body_bytes: 1024 * 1024,
        max_batch_total_bytes: 1024 * 1024,
        max_batch_chunks: 1000,
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

/// 超過検出が CopyDone より前（CopyData 受信中）に起きることを固定する。
///
/// 実装は超過を `errored` に保持して以後の CopyData を discard 予算
/// （`limits::COPY_DISCARD_MAX_MESSAGES`）で読み捨て、54000 の ErrorResponse は
/// CopyDone 後にしか返さない。CopyDone を送らずに予算を超える件数の CopyData を
/// 送り、discard 予算超過（`08P01`）で終端することを観測する。検出が CopyDone 時点
/// まで遅れる退行があれば、後続 CopyData は取り込み経路（feed）へ流れ予算を消費
/// しないため、このテストは 54000 の ErrorResponse も来ず失敗する。
fn assert_detected_before_copy_done(limits: BatchLimits, op: &str, over_limit_rows: &[u8]) {
    let (core, _guard) = new_core(limits);
    let mut stream = connect(core);
    start_copy(&mut stream, op);
    send_copy_data(&mut stream, over_limit_rows);
    // 予算超過の判定は件数が `MAX_MESSAGES` を超えた 1 件で発火する。サーバーが
    // 未読データを残したまま切断して RST で応答を失わないよう、ちょうどその件数だけ送る。
    for _ in 0..=wire_server::limits::COPY_DISCARD_MAX_MESSAGES {
        send_copy_data(&mut stream, b"x");
    }
    let (ty, body) = read_message(&mut stream);
    assert_eq!(ty, b'E', "expected ErrorResponse");
    let text = String::from_utf8_lossy(&body).into_owned();
    assert!(
        text.contains("08P01") && text.contains("COPY discard budget exceeded"),
        "expected discard budget error before CopyDone, got: {text:?}"
    );
    assert!(
        !text.contains("54000"),
        "limit error must wait for CopyDone: {text:?}"
    );
}

#[test]
fn wire17_copy_row_body_over_limit_is_detected_before_copy_done() {
    assert_detected_before_copy_done(
        body_limits(),
        "wire-1178-body-early",
        b"1\t[1.0,0.0]\tja\n2\t[0.0,1.0]\tja\n3\t[1.0,1.0]\televenbytes\n",
    );
}

#[test]
fn wire17_copy_rows_over_chunk_limit_are_detected_before_copy_done() {
    assert_detected_before_copy_done(
        chunk_limits(),
        "wire-1178-chunk-early",
        b"1\t[1.0,0.0]\tja\n2\t[0.0,1.0]\tja\n3\t[1.0,1.0]\tja\n",
    );
}

// --- Issue #3309: INDEX-4 ① は COPY の行数に掛からない ---

fn rows_text(from: u32, to: u32) -> Vec<u8> {
    (from..=to)
        .flat_map(|i| format!("{i}\t[1.0,0.0]\tja\n").into_bytes())
        .collect()
}

fn count_docs(stream: &mut std::net::TcpStream) -> usize {
    send_simple_query(stream, "SELECT id FROM docs LIMIT 1000");
    let mut rows = 0;
    loop {
        let (kind, _) = read_message(stream);
        match kind {
            b'D' => rows += 1,
            b'Z' => return rows,
            _ => {}
        }
    }
}

/// 既定の ①（64）を超える 100 行が、psql 形（末尾 `;` 付きの単独送信）の text／CSV で受理される。
#[test]
fn wire17_copy_more_than_64_rows_are_accepted_text_and_csv() {
    let (core, _guard) = new_core(wide_row_limits());
    let mut stream = connect(core);
    send_simple_query(&mut stream, "COPY docs (id, embedding, lang) FROM STDIN;");
    let (ty, _) = read_message(&mut stream);
    assert_eq!(ty, b'G');
    send_copy_data(&mut stream, &rows_text(1, 100));
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 100");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN WITH (FORMAT csv);",
    );
    let (ty, _) = read_message(&mut stream);
    assert_eq!(ty, b'G');
    let csv: String = (101..=200)
        .map(|i| format!("{i},\"[1.0,0.0]\",ja\n"))
        .collect();
    send_copy_data(&mut stream, csv.as_bytes());
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 100");
    read_ready_for_query(&mut stream);
    assert_eq!(count_docs(&mut stream), 200);
}

/// 65 行目より後の ④ 超過は CopyData 受信中に `54000` になり、行は 1 件も残らない。
#[test]
fn wire17_copy_chunk_overrun_after_64_rows_is_54000_with_zero_side_effects() {
    let limits = BatchLimits {
        max_files_per_batch: 64,
        max_file_body_bytes: 1024 * 1024,
        max_batch_total_bytes: 1024 * 1024,
        max_batch_chunks: 80,
    };
    let (core, _guard) = new_core(limits);
    let mut stream = connect(core);
    start_copy(&mut stream, "wire-3309-chunk-after64");
    send_copy_data(&mut stream, &rows_text(1, 81));
    send_copy_done(&mut stream);
    expect_limit_error(&mut stream, "batch chunk count");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_no_rows_and_connection_alive(&mut stream);
}

/// 65 行を送った後に 1 メッセージ上限を超えるフレームを送ると `54000` で切断され、行は残らない。
#[test]
fn wire17_copy_oversized_frame_after_64_rows_is_54000_with_zero_rows() {
    use std::io::Write;
    let (core, _guard) = new_core(wide_row_limits());
    let mut stream = connect(core.clone());
    start_copy(&mut stream, "wire-3309-oversized");
    send_copy_data(&mut stream, &rows_text(1, 65));
    let declared_len = (wire_server::framing::MAX_MESSAGE_LEN + 1) as i32;
    let mut header = vec![b'd'];
    header.extend_from_slice(&declared_len.to_be_bytes());
    stream.write_all(&header).expect("send oversized header");
    let (ty, body) = read_message(&mut stream);
    assert_eq!(ty, b'E');
    assert!(String::from_utf8_lossy(&body).contains("54000"));
    drop(stream);
    let mut other = connect(core);
    assert_no_rows_and_connection_alive(&mut other);
}

/// `--max-insert-rows` 相当の指定（`with_dml_limits`）があれば COPY もその行数で `54000`・副作用ゼロ。
#[test]
fn wire17_copy_respects_max_insert_rows_when_specified() {
    let path = temp_db::unique_db_path("wire17-copy-max-insert-rows");
    let _guard = temp_db::CleanupGuard(path.clone());
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
        .with_batch_limits(wide_row_limits())
        .with_dml_limits(engine::sql::parser::DmlLimits {
            max_insert_rows_per_statement: std::num::NonZeroUsize::new(3),
            ..Default::default()
        });
    let mut stream = connect(Arc::new(core));
    start_copy(&mut stream, "wire-3309-mir-ok");
    send_copy_data(&mut stream, &rows_text(1, 3));
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 3");
    read_ready_for_query(&mut stream);

    start_copy(&mut stream, "wire-3309-mir-ng");
    send_copy_data(&mut stream, &rows_text(11, 14));
    send_copy_done(&mut stream);
    expect_limit_error(&mut stream, "row count");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 3);
}

// --- Issue #3310: ① を外したあとの既定構成での資源上限 ---------------------------

/// 既定構成（①64・② 16 MiB・③ 64 MiB・④ 4096）を環境変数に依らず明示した上限。
fn explicit_default_limits() -> BatchLimits {
    BatchLimits {
        max_files_per_batch: 64,
        max_file_body_bytes: engine::chunking::MAX_INPUT_BYTES,
        max_batch_total_bytes: engine::incremental::MAX_INDEX_TOTAL_BYTES,
        max_batch_chunks: engine::incremental::MAX_CHUNKS_PER_FILE,
    }
}

/// 固定する上限: ④（既定 4096）。1 MiB 以内の CopyData 1 通に収まる 30,000 行は `54000` の
/// ErrorResponse → ReadyForQuery `I` → 行 0 件で、接続は維持される。
#[test]
fn wire17_copy_tens_of_thousands_of_rows_are_54000_under_default_limits() {
    let (core, _guard) = new_core(explicit_default_limits());
    let mut stream = connect(core);
    start_copy(&mut stream, "wire-3310-30k");
    let rows = rows_text(1, 30_000);
    assert!(
        rows.len() < 1 << 20,
        "fixture must fit in one CopyData frame"
    );
    send_copy_data(&mut stream, &rows);
    send_copy_done(&mut stream);
    expect_limit_error(&mut stream, "batch chunk count");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_no_rows_and_connection_alive(&mut stream);
}

/// 固定する上限: ②（既定 16 MiB）。1 行を 1000 KiB の CopyData 複数通に分け、最後の通で改行を
/// 送る。行が 16 MiB を超えた時点で `54000`・行 0 件・接続維持（読み捨ての予算内）。
#[test]
fn wire17_copy_single_huge_row_is_54000_under_default_limits() {
    let (core, _guard) = new_core(explicit_default_limits());
    let mut stream = connect(core);
    start_copy(&mut stream, "wire-3310-huge-row");
    let piece = vec![b'q'; 1000 * 1024];
    let mut first = b"1\t[1.0,0.0]\t".to_vec();
    first.extend_from_slice(&piece);
    send_copy_data(&mut stream, &first);
    let pieces = engine::chunking::MAX_INPUT_BYTES / piece.len() + 1;
    for _ in 0..pieces {
        send_copy_data(&mut stream, &piece);
    }
    send_copy_data(&mut stream, b"\n");
    send_copy_done(&mut stream);
    expect_limit_error(&mut stream, "");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_no_rows_and_connection_alive(&mut stream);
}
