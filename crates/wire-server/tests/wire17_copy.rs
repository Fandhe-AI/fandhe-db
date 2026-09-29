//! `COPY ... FROM STDIN`／`COPY (...) TO STDOUT`（Issue #939・WIRE-17・
//! TASK-220）の簡易クエリプロトコル経由（生バイトクライアント）検証（層 A）。
//!
//! engine 側の意味論（レコード分割・エスケープ解決・束縛・INDEX-4 逐次判定・
//! commit の原子性）は `crates/engine/tests/copy_from.rs` が確定オラクルとして
//! 検証済みのため、本ファイルは同じ規則が **wire フレーミング**
//! （CopyInResponse／CopyData／CopyDone／CopyFail／CopyOutResponse）越しに
//! 観測できることの確認に集中する（`wire_insert_operation_id.rs` と同じ流儀）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::{Read, Write};
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::framing;

use common::*;

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire17-copy-docs");
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
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `note` を nullable `TEXT` 列として持つテーブル（CSV 形式の NULL 往復検証用。
/// `new_core_with_docs_table` は全列 non-nullable のため NULL を表現できない）。
fn new_core_with_nullable_note_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire17-copy-nullable");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, false),
                ColumnDef::new("note", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `BOOLEAN`／`ARRAY`／`BYTEA`／`ENUM` 列を持つテーブル（Issue #939 レビュー
/// 指摘: cc232e6 で `bind_copy_record`／`cell_to_copy_value` へ追加した該当
/// 型がどのテストからも COPY 経路でカバーされていなかったため追加。
/// engine 側の束縛オラクルは `crates/engine/tests/copy_from.rs` が担い、本
/// テストは wire フレーミング越しの text 表現往復（COPY TO STDOUT の出力を
/// そのまま同じテーブルへ COPY FROM STDIN で再投入）に集中する）。
fn new_core_with_extended_types_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    use engine::catalog::{ArrayElemType, ArrayType, ColumnType as CT};
    let path = temp_db::unique_db_path("wire17-copy-ext");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let enum_def = storage
        .create_enum_type("mood", vec!["happy".to_string(), "sad".to_string()])
        .expect("create enum type");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("flag", CT::Boolean, true),
                ColumnDef::new(
                    "tags",
                    CT::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
                    true,
                ),
                ColumnDef::new("blob", CT::Bytea, true),
                ColumnDef::new("mood", CT::Enum(enum_def), true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn_with_alice(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

fn spawn_with_bob(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "bob", "pw-bob")
}

/// `CopyInResponse`（'G'）を読み、公告された列数を返す。
fn read_copy_in_response(stream: &mut std::net::TcpStream) -> i16 {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'G', "expected CopyInResponse");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    i16::from_be_bytes([body[1], body[2]])
}

/// `CopyOutResponse`（'H'）を読み、公告された列数を返す。
fn read_copy_out_response(stream: &mut std::net::TcpStream) -> i16 {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'H', "expected CopyOutResponse");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    i16::from_be_bytes([body[1], body[2]])
}

/// `CopyData`（'d'）を 1 個読み、本文（改行込みの生バイト列）を返す。
fn read_copy_data(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'd', "expected CopyData");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
    body
}

/// `CopyDone`（'c'）を読み、length=4（body 空）であることを確認する。
fn read_copy_done(stream: &mut std::net::TcpStream) {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read type");
    assert_eq!(header[0], b'c', "expected CopyDone");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    assert_eq!(i32::from_be_bytes(len_buf), 4, "CopyDone has no body");
}

fn send_copy_data(stream: &mut std::net::TcpStream, chunk: &[u8]) {
    send_length_prefixed_message(stream, b'd', chunk);
}

fn send_copy_done(stream: &mut std::net::TcpStream) {
    send_length_prefixed_message(stream, b'c', b"");
}

/// PostgreSQL wire v3 の `String` 型（NUL 終端の C 文字列）契約に沿って理由
/// 文字列へ末尾 NUL を付与して送る（`crates/wire-server/src/copy.rs`
/// の CopyFail 検証。codex-review 指摘・PRRT_kwDOUAKASM6lugH7）。
fn send_copy_fail(stream: &mut std::net::TcpStream, reason: &str) {
    let mut body = reason.as_bytes().to_vec();
    body.push(0);
    send_length_prefixed_message(stream, b'f', &body);
}

// ---------------------------------------------------------------------
// COPY FROM STDIN
// ---------------------------------------------------------------------

#[test]
fn wire17_copy_from_stdin_text_format_round_trips_through_select() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'wire-copy-1'",
    );
    let ncols = read_copy_in_response(&mut stream);
    assert_eq!(ncols, 3);

    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_copy_data(&mut stream, b"2\t[0.0,1.0]\tja\n");
    send_copy_done(&mut stream);

    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "COPY 2");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 100");
    let _cols = read_row_description(&mut stream);
    let mut ids: Vec<u64> = Vec::new();
    for _ in 0..2 {
        let row = read_data_row(&mut stream);
        ids.push(row[0].as_ref().unwrap().parse().unwrap());
    }
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

#[test]
fn wire17_copy_from_stdin_csv_format_is_accepted() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN WITH (FORMAT csv) USING OPERATION_ID 'wire-copy-2'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, b"1,\"[1.0,0.0]\",ja\n");
    send_copy_done(&mut stream);

    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "COPY 1");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire17_copy_from_stdin_missing_operation_id_is_rejected_before_copy_in_response() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(&mut stream, "COPY docs (id, embedding, lang) FROM STDIN");
    expect_error_response_with_sqlstate(&mut stream, "23502");
    read_ready_for_query(&mut stream);

    // 通常のクエリが引き続き正常に動くこと（CopyInResponse を送っていない
    // ため接続状態が壊れていないことの確認）。
    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10");
    let _cols = read_row_description(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 列名の typo（列リストにスキーマ実在しない列名）は `CopyInResponse`
/// （`G`）を送出する前に拒否されなければならない（codex-review 指摘・
/// Issue #939 レビュー対応。修正前は各行のデコード時にしか検出できず、
/// クライアントは既に copy mode へ入ってからデータ送信を開始してしまって
/// いた）。`missing_operation_id` テストと同じ「`CopyInResponse` を送出して
/// いないので接続状態は壊れていない」ことの確認まで含める。
#[test]
fn wire17_copy_from_stdin_unknown_column_is_rejected_before_copy_in_response() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, alng) FROM STDIN USING OPERATION_ID 'wire-copy-unknown-col'",
    );
    expect_error_response_with_sqlstate(&mut stream, "22000");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10");
    let _cols = read_row_description(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 0", "no row must have been committed");
    read_ready_for_query(&mut stream);
}

/// 非 nullable 列（`lang`）が列リストから欠落している場合も、
/// `CopyInResponse` を送出する前に拒否されなければならない（同上）。
/// エラー契約は `bind_copy_record`（`fill_omitted_columns` 経由）・INSERT・
/// 行内 `\N` 処理と同じ NOT NULL 違反として `23502` に統一する
/// （PR #1051 codex-review 指摘。列リストでの事前検出とデコード時検出とで
/// エラー契約が経路依存で揺れないようにする）。
#[test]
fn wire17_copy_from_stdin_missing_non_nullable_column_is_rejected_before_copy_in_response() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding) FROM STDIN USING OPERATION_ID 'wire-copy-missing-col'",
    );
    expect_error_response_with_sqlstate(&mut stream, "23502");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10");
    let _cols = read_row_description(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 0", "no row must have been committed");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire17_copy_from_stdin_invalid_row_is_rejected_with_zero_side_effects() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'wire-copy-3'",
    );
    let _ = read_copy_in_response(&mut stream);
    // 2 行目の embedding 次元が宣言（2）と不一致。
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n2\t[1.0,0.0,0.0]\tja\n");
    send_copy_done(&mut stream);

    expect_error_response_with_sqlstate(&mut stream, "22000");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10");
    let cols = read_row_description(&mut stream);
    let _ = cols;
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 0", "no row must have been committed");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire17_copy_from_stdin_duplicate_operation_id_matches_multi_row_insert_ledger() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'wire-copy-shared'",
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'wire-copy-shared'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_copy_done(&mut stream);

    expect_error_response_with_sqlstate(&mut stream, "23505");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire17_copy_from_stdin_copy_fail_writes_zero_rows() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'wire-copy-4'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_copy_fail(&mut stream, "client aborted the copy locally");

    expect_error_response_with_sqlstate(&mut stream, "22000");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10");
    let _cols = read_row_description(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 0", "CopyFail must not write any row");
    read_ready_for_query(&mut stream);
}

#[test]
fn wire17_copy_from_stdin_rejects_unexpected_message_type_and_closes() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'wire-copy-5'",
    );
    let _ = read_copy_in_response(&mut stream);
    // 拡張クエリプロトコルの Parse メッセージは COPY サブプロトコル中は想定外。
    send_length_prefixed_message(&mut stream, b'P', b"\0select 1\0\0\0");

    expect_error_response_with_sqlstate(&mut stream, "08P01");
    expect_connection_closed(&mut stream);
}

/// CopyData（'d'）の宣言長が `framing::MAX_MESSAGE_LEN` を超過した場合、通常の
/// 簡易クエリ 'Q' 経路（フレーミング違反は `wire_code` 付き ErrorResponse を
/// 送ってから接続終了）と同じ診断契約になること（Issue #939 レビュー指摘:
/// 修正前は `copy.rs::frame_err_to_io` が `FrameError::TooLarge` を汎用
/// `io::Error` へ潰し、`ErrorResponse` を一切送らずに切断していた）。
#[test]
fn wire17_copy_from_stdin_oversized_copy_data_frame_gets_error_response_before_close() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'wire-copy-oversized'",
    );
    let _ = read_copy_in_response(&mut stream);

    // `TooLarge` は長さフィールドのみで判定されるため、実際に巨大な本文を
    // 送る必要はない（`framing::read_length_prefixed_body` は本文を読む前に
    // 宣言長を検証して打ち切る）。
    let declared_len = (framing::MAX_MESSAGE_LEN + 1) as i32;
    let mut header = Vec::new();
    header.push(b'd');
    header.extend_from_slice(&declared_len.to_be_bytes());
    stream.write_all(&header).expect("send oversized header");

    expect_error_response_with_sqlstate(&mut stream, "54000");
    expect_connection_closed(&mut stream);
}

/// CopyDone（'c'）の宣言長が固定 4 バイトと異なる（`FrameError::Malformed`）
/// 場合も、oversized `CopyData` と同じく `wire_code`（`08P01`）付き
/// ErrorResponse を送ってから接続を終了すること（Issue #939 レビュー指摘の
/// 再発防止: 修正は `'d'` 分岐だけでなく `run_copy_from` ループ内の全フレーム
/// 読み取りへ及ぶべきことの確認）。
#[test]
fn wire17_copy_from_stdin_malformed_copy_done_frame_gets_error_response_before_close() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'wire-copy-malformed-done'",
    );
    let _ = read_copy_in_response(&mut stream);

    // CopyDone は body を持たない固定 5 バイト（type + length=4）のはずだが、
    // 宣言長 3（`read_length_prefixed_body(stream, 4, 4)` の最小値未満）を送る。
    let mut header = Vec::new();
    header.push(b'c');
    header.extend_from_slice(&3i32.to_be_bytes());
    stream
        .write_all(&header)
        .expect("send malformed CopyDone header");

    expect_error_response_with_sqlstate(&mut stream, "08P01");
    expect_connection_closed(&mut stream);
}

// ---------------------------------------------------------------------
// COPY (...) TO STDOUT
// ---------------------------------------------------------------------

#[test]
fn wire17_copy_to_stdout_returns_only_own_tenant_rows() {
    let (core, _guard) = new_core_with_docs_table();
    let mut alice = spawn_with_alice(Arc::clone(&core));

    send_simple_query(
        &mut alice,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'to-op-1'",
    );
    let _tag = read_command_complete(&mut alice);
    read_ready_for_query(&mut alice);

    let mut bob = spawn_with_bob(Arc::clone(&core));
    send_simple_query(
        &mut bob,
        "INSERT INTO docs (id, embedding, lang) VALUES (2, '[0.0,1.0]', 'ja') USING OPERATION_ID 'to-op-2'",
    );
    let _tag = read_command_complete(&mut bob);
    read_ready_for_query(&mut bob);

    send_simple_query(&mut alice, "COPY (SELECT id FROM docs LIMIT 100) TO STDOUT");
    let ncols = read_copy_out_response(&mut alice);
    assert_eq!(ncols, 1);
    let row = read_copy_data(&mut alice);
    assert_eq!(row, b"1\n");
    read_copy_done(&mut alice);
    let tag = read_command_complete(&mut alice);
    assert_eq!(tag, "COPY 1");
    read_ready_for_query(&mut alice);
}

#[test]
fn wire17_copy_to_stdout_rejects_ranked_select() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY (SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10) TO STDOUT",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}

/// COPY TO の出力を同じテーブルへ COPY FROM で再投入した際に値が往復する
/// こと（`docs/design/wire-copy-protocol.md`「対称性」節の固定）。
#[test]
fn wire17_copy_to_stdout_output_round_trips_through_copy_from_stdin() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'rt-op-1'",
    );
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "COPY (SELECT id, lang FROM docs LIMIT 10) TO STDOUT",
    );
    let _ = read_copy_out_response(&mut stream);
    let row = read_copy_data(&mut stream);
    read_copy_done(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    // `id\tlang\n` 形式の出力を、`embedding` を補って別 id で再投入する。
    let text = String::from_utf8(row).expect("utf8 copy output");
    let mut parts = text.trim_end_matches('\n').splitn(2, '\t');
    let _id = parts.next().expect("id field");
    let lang = parts.next().expect("lang field");

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'rt-op-2'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, format!("2\t[0.0,1.0]\t{lang}\n").as_bytes());
    send_copy_done(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "COPY 1");
    read_ready_for_query(&mut stream);
}

/// `BOOLEAN`／`ARRAY`／`BYTEA`／`ENUM` 列を含む `COPY (...) TO STDOUT` の
/// text 表現出力を、そのまま同じテーブルへ `COPY ... FROM STDIN` で別 id へ
/// 再投入すると値が往復すること（Issue #939 レビュー指摘: cc232e6 で追加した
/// 4 型の COPY 束縛・エンコードがいずれのテストからもカバーされていなかった
/// ため追加。`wire17_copy_to_stdout_output_round_trips_through_copy_from_stdin`
/// と同じ検証形）。
#[test]
fn wire17_copy_to_stdout_output_round_trips_extended_column_types() {
    let (core, _guard) = new_core_with_extended_types_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, flag, tags, blob, mood) FROM STDIN USING OPERATION_ID 'ext-rt-op-1'",
    );
    let _ = read_copy_in_response(&mut stream);
    // text 形式の生入力: BYTEA リテラルの `\x` はバックスラッシュエスケープ
    // 越しに表現するため `\\x` と 2 個書く（`decode_text_field` が `\\` を
    // 単一のバックスラッシュへ解決してから `\xdeadbeef` として束縛される）。
    send_copy_data(
        &mut stream,
        b"1\t[1.0,0.0]\tt\t{ja,en}\t\\\\xdeadbeef\thappy\n",
    );
    send_copy_done(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "COPY 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "COPY (SELECT id, flag, tags, blob, mood FROM docs LIMIT 10) TO STDOUT",
    );
    let _ = read_copy_out_response(&mut stream);
    let row = read_copy_data(&mut stream);
    read_copy_done(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    let text = String::from_utf8(row).expect("utf8 copy output");
    let line = text.trim_end_matches('\n');
    assert_eq!(
        line, "1\tt\t{ja,en}\t\\\\xdeadbeef\thappy",
        "text表現は BOOLEAN=t/f・ARRAY={{...}}・BYTEA=\\x エスケープ・ENUM=素の label",
    );

    // その出力（id 列を除く）をそのまま別 id へ再投入すると値が往復する。
    let mut parts = line.splitn(2, '\t');
    let _id = parts.next().expect("id field");
    let rest = parts.next().expect("remaining fields");

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, flag, tags, blob, mood) FROM STDIN USING OPERATION_ID 'ext-rt-op-2'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, format!("2\t[0.0,1.0]\t{rest}\n").as_bytes());
    send_copy_done(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "COPY 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT flag, tags, blob, mood FROM docs WHERE id = 2 LIMIT 1",
    );
    let _cols = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(
        row,
        vec![
            Some("t".to_string()),
            Some("{ja,en}".to_string()),
            Some("\\xdeadbeef".to_string()),
            Some("happy".to_string()),
        ]
    );
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// CSV 形式の `COPY (...) TO STDOUT` が NULL 列を出力し、その出力を同じ
/// テーブルへ CSV 形式 `COPY ... FROM STDIN` で再投入すると NULL のまま戻る
/// こと（レビュー指摘: text 形式の `\N` をそのまま CSV へ流用すると、CSV 側の
/// NULL 表現（引用符なし空フィールド）と食い違い "\N" という文字列に化けて
/// 往復が壊れていた。`crates/wire-server/src/copy.rs::encode_copy_data_row_into`
/// の CSV 分岐固定）。
#[test]
fn wire17_copy_csv_null_round_trips_through_copy_from_stdin() {
    let (core, _guard) = new_core_with_nullable_note_table();
    let mut stream = spawn_with_alice(core);

    // `note` を列リストから省略すると nullable 列は NULL のまま挿入される
    // （`INSERT` の許可形状は `VALUES` 内の `NULL` キーワードを受理しないため、
    // 明示的な NULL 値はこの形で作る。`sql::parser::bind_insert_row` 参照）。
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'csv-null-op-1'",
    );
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "COPY (SELECT id, lang, note FROM docs LIMIT 10) TO STDOUT WITH (FORMAT csv)",
    );
    let _ = read_copy_out_response(&mut stream);
    let row = read_copy_data(&mut stream);
    read_copy_done(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    // `id,lang,note\n` の CSV 出力で `note` が NULL の場合は引用符なし空
    // フィールド（`\N` ではない）として出力されていること。
    let text = String::from_utf8(row).expect("utf8 copy output");
    let line = text.trim_end_matches('\n');
    assert_eq!(line, "1,ja,", "CSV NULL must be an unquoted empty field");

    // その出力を CSV 形式で再投入すると note は NULL のまま戻る（"\\N" という
    // 文字列として読み込まれてはいけない）。
    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang, note) FROM STDIN WITH (FORMAT csv) USING OPERATION_ID 'csv-null-op-2'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, b"2,\"[0.0,1.0]\",ja,\n");
    send_copy_done(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "COPY 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT note FROM docs WHERE id = 2 LIMIT 1");
    let _cols = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![None], "note must round-trip as NULL, not \"\\N\"");
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// COPY FROM STDIN の途中で Terminate（'X'）が届いた場合、CopyDone の 4 バイト
/// 長さフィールドと対称に自身の長さフィールド（4 バイト固定・body 厳密に空）を
/// 確実に消費すること（レビュー指摘: 消費しないと `post_auth_loop` がその
/// 4 バイトを次のメッセージ種別バイトとして誤読しデシンクする）。
///
/// Terminate は通常の簡易クエリループと同じく接続そのものを終了させる
/// 契約（PostgreSQL 本家の Terminate 契約と同じ。codex-review P1 指摘・
/// discussion_r4096720859: 以前は COPY 中の Terminate を消費するだけで
/// 通常のクエリループへ戻ってしまい、クライアントが自発的にソケットを
/// 閉じない限り接続スロットを保持し続けていた）ため、Terminate 後は
/// サーバー側が能動的に接続を閉じる（追加の読み取りが EOF になる）ことを
/// 確認する。
#[test]
fn wire17_copy_from_stdin_terminate_mid_copy_closes_connection() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'terminate-mid-copy'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_length_prefixed_message(&mut stream, b'X', b"");

    // サーバーが接続を能動的に閉じていれば、これ以上何を送っても応答は
    // 来ず、読み取りは EOF（`Ok(0)`）になる（デシンクしていれば代わりに
    // 何らかの応答バイト列が返り、`expect_connection_closed` が検出する）。
    expect_connection_closed(&mut stream);
}

/// `wire17_copy_from_stdin_terminate_mid_copy_closes_connection` が確認できない
/// 「COPY は Terminate によって中断され行が commit されない」契約を、
/// 同一 engine を使う別接続から確認する。
#[test]
fn wire17_copy_from_stdin_terminate_mid_copy_commits_no_row() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(Arc::clone(&core));

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'terminate-mid-copy-2'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_length_prefixed_message(&mut stream, b'X', b"");
    drop(stream);

    let mut stream2 = spawn_with_alice(core);
    send_simple_query(&mut stream2, "SELECT id FROM docs LIMIT 10");
    let _cols = read_row_description(&mut stream2);
    let tag = read_command_complete(&mut stream2);
    assert_eq!(
        tag, "SELECT 0",
        "COPY was abandoned by Terminate, no row committed"
    );
    read_ready_for_query(&mut stream2);
}

/// COPY FROM STDIN の途中で本文が空（宣言長ちょうど 4）の Flush（'H'）／
/// Sync（'S'）が届いた場合は通常どおり無視して COPY を継続できること。
#[test]
fn wire17_copy_from_stdin_flush_sync_without_body_are_ignored() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'flush-sync-noop'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_length_prefixed_message(&mut stream, b'H', b"");
    send_length_prefixed_message(&mut stream, b'S', b"");
    send_copy_data(&mut stream, b"2\t[0.0,1.0]\tja\n");
    send_copy_done(&mut stream);

    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "COPY 2");
    read_ready_for_query(&mut stream);
}

/// COPY FROM STDIN の途中で本文付き（宣言長が 4 バイトを超える）Flush
/// （'H'）／Sync（'S'）が届いた場合、PostgreSQL wire v3 上この 2 種類は
/// length=4（body 厳密に空）以外の形状を持たないため `08P01` で fail-closed
/// に拒否し接続を終了すること（codex-review 指摘・
/// discussion PRRT_kwDOUAKASM6ltrHb: 旧実装は最大長のみを検証し任意の本文を
/// 読み捨てて正常受理していた）。'H'／'S' いずれも同じ検証経路を通ることを
/// 確認する。
#[test]
fn wire17_copy_from_stdin_flush_sync_with_body_is_rejected() {
    for type_byte in *b"HS" {
        let (core, _guard) = new_core_with_docs_table();
        let mut stream = spawn_with_alice(core);

        send_simple_query(
            &mut stream,
            "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'flush-sync-body-reject'",
        );
        let _ = read_copy_in_response(&mut stream);
        send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
        send_length_prefixed_message(&mut stream, type_byte, b"unexpected-body");

        expect_error_response_with_sqlstate(&mut stream, "08P01");
        expect_connection_closed(&mut stream);
    }
}

/// 本文が空の Flush（'H'）を連送しても、フレームヘッダー分の固定
/// オーバーヘッド（`limits::COPY_DISCARD_FRAME_OVERHEAD_BYTES`）とメッセージ
/// 件数上限（`limits::COPY_DISCARD_MAX_MESSAGES`）により最終的に `08P01` の
/// ErrorResponse を送ってから接続を終了すること（codex-review 指摘・
/// PRRT_kwDOUAKASM6ltyY2: 旧実装は `CopyData`（'d'）のバイト量・本文付き
/// Flush／Sync のバイト量しかこの上限に数えず、body が空のフレームを連送
/// すると予算を一切消費できず、クライアントが極小フレームを送り続けることで
/// DoS 上限を無制限に回避し接続スロットを占有し続けられた）。
#[test]
fn wire17_copy_from_stdin_empty_flush_sync_spam_counts_toward_discard_budget() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'flush-spam-discard-budget'",
    );
    let _ = read_copy_in_response(&mut stream);

    // メッセージ件数上限を超えるまで本文が空の Flush を送り続ける。
    for _ in 0..=wire_server::limits::COPY_DISCARD_MAX_MESSAGES {
        send_length_prefixed_message(&mut stream, b'H', b"");
    }

    expect_error_response_with_sqlstate(&mut stream, "08P01");
    expect_connection_closed(&mut stream);
}

/// 行デコードエラー（`session.feed` 失敗）後の読み捨てモード中に
/// `limits::COPY_DISCARD_MAX_BYTES` を超えた場合、旧実装は応答なしの生
/// `io::Error` で切断していた（codex-review P1 指摘・discussion_
/// r4096720869）。他の frame エラー経路と同じく `08P01` の ErrorResponse を
/// 送ってから接続を終了する契約であることを確認する。
#[test]
fn wire17_copy_from_stdin_discard_budget_exceeded_after_row_error_gets_error_response_before_close()
{
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-discard-after-error'",
    );
    let _ = read_copy_in_response(&mut stream);
    // 1 行目の embedding 次元が宣言（2）と不一致のため、直後に読み捨て
    // モードへ入る。
    send_copy_data(&mut stream, b"1\t[1.0,0.0,0.0]\tja\n");

    // 読み捨てモード中の `CopyData` も `limits::COPY_DISCARD_MAX_BYTES`
    // （16 MiB）の対象。1 メッセージあたりの消費量はフレームヘッダー分の
    // 固定オーバーヘッド（`limits::COPY_DISCARD_FRAME_OVERHEAD_BYTES`）も
    // 含むため、`framing::MAX_MESSAGE_LEN - 4` バイトの body では 16 回で
    // 予算を超える（各回サーバーは本文を全て読み終えてから予算判定するため、
    // 最後の 1 回まではクライアント側の書き込みが必ず成功する）。
    let body = vec![b'x'; framing::MAX_MESSAGE_LEN - 4];
    for _ in 0..16 {
        send_copy_data(&mut stream, &body);
    }

    expect_error_response_with_sqlstate(&mut stream, "08P01");
    expect_connection_closed(&mut stream);
}

/// codex-review P0 指摘（PRRT_kwDOUAKASM6luiS5）: `errored` が `None`（＝
/// まだ正常に取り込み処理中）の間、body が空（宣言長 4）の `CopyData` を
/// 連送しても `session.feed` は進捗（`running_bytes` の増加・行確定）を
/// 一切伴わないため、旧実装ではどの上限（`COPY_DISCARD_MAX_BYTES`／
/// `COPY_DISCARD_MAX_MESSAGES`）も消費されず接続スロットを無期限に占有
/// できた。エラー未発生の状態でも空 `CopyData` の連送が同じ discard 予算へ
/// 数えられ、上限超過時に `08P01` で fail-closed に切断されることを確認する。
#[test]
fn wire17_copy_from_stdin_empty_copy_data_spam_before_any_error_counts_toward_discard_budget() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'empty-copy-data-spam'",
    );
    let _ = read_copy_in_response(&mut stream);

    // 行エラーを一切起こさないまま、本文が空の CopyData をメッセージ件数
    // 上限を超えるまで送り続ける。
    for _ in 0..=wire_server::limits::COPY_DISCARD_MAX_MESSAGES {
        send_copy_data(&mut stream, b"");
    }

    expect_error_response_with_sqlstate(&mut stream, "08P01");
    expect_connection_closed(&mut stream);
}

/// codex-review P1 指摘（PRRT_kwDOUAKASM6lugH7）: `CopyFail` の理由文字列は
/// PostgreSQL wire v3 の `String` 型（NUL 終端の C 文字列）契約に従う必要が
/// あり、旧実装は宣言長のみを検証し本文を無条件に読み捨てていたため NUL
/// 終端を欠く本文も正常な `CopyFail` として受理していた。末尾 NUL を持たない
/// 本文は `08P01` で fail-closed に拒否され、接続が終了することを確認する。
#[test]
fn wire17_copy_from_stdin_copy_fail_without_nul_terminator_is_rejected() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-fail-no-nul'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    // NUL 終端を付けない生の理由文字列（`send_copy_fail` は正しく NUL を
    // 付与するため、ここでは直接メッセージを組み立てる）。
    send_length_prefixed_message(&mut stream, b'f', b"no terminator here");

    expect_error_response_with_sqlstate(&mut stream, "08P01");
    expect_connection_closed(&mut stream);
}

/// 本文が空（宣言長 4）の `CopyFail` も `String` 型として不正（NUL 終端が
/// 存在しない）であり同じく `08P01` で拒否されることを確認する。
#[test]
fn wire17_copy_from_stdin_copy_fail_with_empty_body_is_rejected() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-fail-empty-body'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_length_prefixed_message(&mut stream, b'f', b"");

    expect_error_response_with_sqlstate(&mut stream, "08P01");
    expect_connection_closed(&mut stream);
}

/// 埋め込み NUL（末尾以外の位置に NUL を含む）本文も `String` 型として不正
/// であり `08P01` で拒否されることを確認する。
#[test]
fn wire17_copy_from_stdin_copy_fail_with_embedded_nul_is_rejected() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-fail-embedded-nul'",
    );
    let _ = read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_length_prefixed_message(&mut stream, b'f', b"bad\0reason\0");

    expect_error_response_with_sqlstate(&mut stream, "08P01");
    expect_connection_closed(&mut stream);
}

// ---------------------------------------------------------------------
// 複数文メッセージとの相互作用（WIRE-16・TASK-219・Issue #938 との整合）。
//
// `handshake::post_auth_loop` の `'Q'` 分岐は `is_copy_statement` を
// メッセージ全文へ適用してから分岐するため（`crate::copy` モジュール
// ドキュメント参照）、以下の 3 形を固定する:
//   (1) 単一の `COPY` 文に許容される末尾セミコロン 1 個は受理される
//       （`sql::allowlist::Parser::expect_end_of_statement` が単一文と同じく
//       許容する）。
//   (2) `COPY ...; <他の文>` は `is_copy_statement` が真になり
//       `crate::copy::run` へ委譲されるが、`expect_end_of_statement` が
//       余剰トークンを検出して `42601` へ落ちる（CopyInResponse は一度も
//       送出されない）。
//   (3) `<他の文>; COPY ...` は `is_copy_statement` が偽（先頭が `COPY` で
//       ない）になり `simple_query::execute_and_respond` の複数文経路へ流れる。
//       `COPY` は `sql::allowlist::validate_sql` の許可形状に含まれないため
//       （`sql::copy` モジュールドキュメント参照）、2 文目の実行時に `42601`
//       で拒否される。
// いずれの形でも `post_auth_loop` の後続メッセージ読み取りがデシンクしない
// （後続の通常クエリが正しく処理される）ことをあわせて確認する。
// ---------------------------------------------------------------------

/// (1) 単一 `COPY` 文の末尾に許容セミコロンが 1 個だけ付いた形は通常どおり
/// 受理される（`SELECT ...;` と同じ既存契約。Issue #938 の分割器はこの形を
/// `SplitOutcome::Single` として素通しし、`is_copy_statement` はそもそも
/// `sql::statement_splitter` を経由しないため影響を受けない）。
#[test]
fn wire17_copy_from_stdin_with_trailing_semicolon_is_accepted() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'trailing-semi';",
    );
    let ncols = read_copy_in_response(&mut stream);
    assert_eq!(ncols, 3);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_copy_done(&mut stream);

    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "COPY 1");
    read_ready_for_query(&mut stream);
}

/// (2) `COPY ... FROM STDIN ...; SELECT 1`（Issue #1175）: COPY が書き込みで最後以外に
/// あるため、メッセージ全体が暗黙トランザクションになる。トランザクション内の COPY は
/// 未対応（`0A000`）なので CopyIn サブプロトコルへは入らず（`CopyInResponse` は
/// 一度も送出されない）、`ReadyForQuery` は `'I'`。エラー後も接続はデシンクせず、
/// 後続の通常クエリを正しく処理できる。
#[test]
fn wire17_copy_from_stdin_followed_by_another_statement_is_rejected_without_entering_copy_in() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'trailing-stmt'; SELECT 1",
    );
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    // デシンクしていなければ後続クエリは通常どおり処理される。
    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10");
    let _cols = read_row_description(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(
        tag, "SELECT 0",
        "rejected COPY must not have inserted any row"
    );
    read_ready_for_query(&mut stream);
}

/// (3) `SELECT ...; COPY ... FROM STDIN ...`（Issue #1175）: 書き込みは最後の 1 文だけ
/// なので従来どおり逐次実行され、2 文目の `COPY` は PostgreSQL と同じく CopyIn
/// サブプロトコルへ入って完了する（従来は `42601`）。1 文目の応答は通常どおり届き、
/// `ReadyForQuery` は最後にちょうど 1 回。
#[test]
fn wire17_copy_from_stdin_as_last_statement_of_multi_statement_message_runs_copy_in() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs LIMIT 1; COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'non-first'",
    );
    let _cols = read_row_description(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 0");
    let ncols = read_copy_in_response(&mut stream);
    assert_eq!(ncols, 3);
    send_copy_data(&mut stream, b"7\t[1.0,0.0]\tja\n");
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10");
    let _cols = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("7"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// (4) `COPY (SELECT ...) TO STDOUT; SELECT 1`（Issue #1175）: `COPY ... TO` は読み取り
/// なので逐次実行され、CopyOut の応答の後に続きの文の応答が届く。`ReadyForQuery` は
/// 最後の文の後に 1 回だけ。
#[test]
fn wire17_copy_to_stdout_followed_by_select_responds_in_order() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'to-then-select'",
    );
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "COPY (SELECT id FROM docs LIMIT 100) TO STDOUT; SELECT id FROM docs LIMIT 1",
    );
    let ncols = read_copy_out_response(&mut stream);
    assert_eq!(ncols, 1);
    assert_eq!(read_copy_data(&mut stream), b"1\n");
    read_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    let _cols = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("1"));
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// (5) `INSERT ...; COPY ... FROM STDIN`（Issue #1175）: 暗黙トランザクション内の COPY は
/// 未対応（`0A000`）。先行する INSERT の応答は届くが、INSERT もロールバックされる。
#[test]
fn wire17_insert_then_copy_from_stdin_is_rejected_and_rolled_back() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'ins-then-copy'; \
         COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'copy-after-insert'",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 10");
    let _cols = read_row_description(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 0");
    read_ready_for_query(&mut stream);
}
