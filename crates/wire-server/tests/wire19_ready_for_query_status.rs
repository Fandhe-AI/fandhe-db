//! `ReadyForQuery`（'Z'）状態バイトが明示トランザクション状態
//! （`Idle`/`InTransaction`/`Failed` → `'I'`/`'T'`/`'E'`）を反映することの
//! 残差検証（Issue #943・WIRE-19・SQL-31・TASK-221）。
//!
//! production の中核（`result_encoder::encode_ready_for_query` と、
//! `simple_query.rs`/`handshake.rs`/`extended_query.rs` の各 `ReadyForQuery`
//! 送出箇所がすべて `txn.status()` を渡す結線）は PR #1041（Issue #942 の
//! codex-review 指摘対応）で実装済みであり、
//! `wire942_extended_transaction.rs::simple_query_ready_for_query_status_reflects_transaction_state`／
//! `extended_query_sync_ready_for_query_status_reflects_transaction_state` が
//! 基本の I→T→E→I 遷移をすでに固定している
//! （`docs/design/explicit-transaction.md`「`#943` との分担」節参照）。
//! 本ファイルはその上で未固定だった異常遷移・複数文・空文・COPY 経路の
//! 状態バイトを網羅する（production コードは変更しない。テスト専任）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

/// `wire942_extended_transaction.rs::new_core_with_documents_table` と同型の
/// 小さな `documents` テーブル（`embedding VECTOR(3)` + `body TEXT`）。
fn new_core_with_documents_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire19-ready-for-query-status");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = Arc::new(EngineCore::from_storage(
        storage,
        Box::new(CpuScalarProvider),
    ));
    (core, guard)
}

fn insert_sql(id: u64, op: &str) -> String {
    format!(
        "INSERT INTO documents (id, embedding, body) VALUES ({id}, '[0.1,0.2,0.3]', 'row') USING OPERATION_ID '{op}'"
    )
}

fn spawn_alice(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "correct-horse")
}

/// トランザクション外の構文エラーの後は `'I'` のまま（PostgreSQL 準拠。
/// autocommit の各文はそれ自身の redb トランザクションで完結し、明示
/// トランザクションへは一切影響しない）。
#[test]
fn autocommit_syntax_error_leaves_status_idle() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    send_simple_query(&mut stream, "SELECT id FROM documents LIMIT 1");
    let _ = read_row_description(&mut stream);
    let _ = read_command_complete(&mut stream);
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    send_simple_query(&mut stream, "SELEC id FROM documents");
    expect_error_response_with_sqlstate(&mut stream, "42601");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    // 構文エラーの後も通常どおり継続できる（既存クライアント挙動の非破壊）。
    send_simple_query(&mut stream, "SELECT id FROM documents LIMIT 1");
    let _ = read_row_description(&mut stream);
    let _ = read_command_complete(&mut stream);
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// `BEGIN` 後に `COMMIT` すると commit されて `'I'` へ戻る（simple query 経由。
/// `wire942_extended_transaction.rs` の既存テストは `ROLLBACK` 復帰のみを
/// 固定しており、`COMMIT` による `'I'` 復帰は未固定だった）。
#[test]
fn commit_after_begin_returns_to_idle() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    send_simple_query(&mut stream, &insert_sql(1, "op-943-1"));
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    send_simple_query(&mut stream, "COMMIT");
    assert_eq!(read_command_complete(&mut stream), "COMMIT");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    // commit 済みの行が読み戻せる（read-your-writes）と、以降も `'I'`。
    send_simple_query(&mut stream, "SELECT id FROM documents WHERE id = 1 LIMIT 1");
    let _ = read_row_description(&mut stream);
    assert_eq!(read_data_row(&mut stream), vec![Some("1".to_string())]);
    let _ = read_command_complete(&mut stream);
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// 入れ子の `BEGIN`（`25001`）で `Failed` へ遷移し `'E'` になる。
#[test]
fn nested_begin_fails_transaction_with_e_status() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    send_simple_query(&mut stream, "BEGIN");
    expect_error_response_with_sqlstate(&mut stream, "25001");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// トランザクション外の `COMMIT`／`ROLLBACK`（`25P01`）は `'I'` のまま
/// （そもそも `Active` に入っていないため `Failed` へは遷移しない）。
#[test]
fn commit_or_rollback_outside_transaction_leaves_status_idle() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    send_simple_query(&mut stream, "COMMIT");
    expect_error_response_with_sqlstate(&mut stream, "25P01");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    send_simple_query(&mut stream, "ROLLBACK");
    expect_error_response_with_sqlstate(&mut stream, "25P01");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// `Failed` 中の `COMMIT` は（PostgreSQL と異なり）`25P02` で拒否され `'E'` の
/// まま——`Failed` を解除するのは `ROLLBACK` のみという fail-closed 契約
/// （spec SQL-31 の既知の意図的逸脱。`docs/design/explicit-transaction.md`
/// 参照）。続く `ROLLBACK` で `'I'` へ戻る。
#[test]
fn commit_while_failed_is_rejected_and_stays_failed_until_rollback() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    send_simple_query(&mut stream, "SELEC id FROM documents");
    expect_error_response_with_sqlstate(&mut stream, "42601");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(&mut stream, "COMMIT");
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// `T` 中の空クエリ（`EmptyQueryResponse`。型バイト `'I'` を持つが
/// `ReadyForQuery` とは別メッセージ）はトランザクション状態を変えない。
#[test]
fn empty_query_inside_transaction_leaves_status_in_transaction() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    send_simple_query(&mut stream, "");
    expect_empty_query_response(&mut stream);
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// 複数文メッセージ（WIRE-16）: `BEGIN; INSERT ...` は最後の
/// `ReadyForQuery` が `'T'`（`check_write_placement` が `BEGIN` を含む
/// メッセージ内では書き込み文の位置を問わず許可する）。
#[test]
fn multi_statement_begin_then_insert_ends_in_transaction() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    let sql = format!("BEGIN; {}", insert_sql(10, "op-943-10"));
    send_simple_query(&mut stream, &sql);
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    send_simple_query(&mut stream, "COMMIT");
    assert_eq!(read_command_complete(&mut stream), "COMMIT");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// 複数文メッセージ: `BEGIN; INSERT ...; COMMIT` は 1 メッセージ内で完結し
/// 最後の `ReadyForQuery` が `'I'`。
#[test]
fn multi_statement_begin_insert_commit_ends_idle() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    let sql = format!("BEGIN; {}; COMMIT", insert_sql(11, "op-943-11"));
    send_simple_query(&mut stream, &sql);
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_command_complete(&mut stream), "COMMIT");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    send_simple_query(
        &mut stream,
        "SELECT id FROM documents WHERE id = 11 LIMIT 1",
    );
    let _ = read_row_description(&mut stream);
    assert_eq!(read_data_row(&mut stream), vec![Some("11".to_string())]);
    let _ = read_command_complete(&mut stream);
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// 複数文メッセージ: `BEGIN; <構文エラー>` は `BEGIN` の `CommandComplete` の
/// 後にエラーで打ち切られ、`Failed`（`'E'`）へ遷移する。`ROLLBACK` で
/// `'I'` へ戻る。
#[test]
fn multi_statement_begin_then_syntax_error_ends_failed() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    send_simple_query(&mut stream, "BEGIN; SELEC id FROM documents");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    expect_error_response_with_sqlstate(&mut stream, "42601");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// 複数文メッセージ: トランザクション外の複数 `SELECT` は最後の
/// `ReadyForQuery` が `'I'`（明示トランザクションを一切使わない既存挙動の
/// 非破壊確認）。
#[test]
fn multi_statement_without_transaction_stays_idle() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    send_simple_query(
        &mut stream,
        "SELECT id FROM documents LIMIT 1; SELECT id FROM documents LIMIT 1",
    );
    let _ = read_row_description(&mut stream);
    let _ = read_command_complete(&mut stream);
    let _ = read_row_description(&mut stream);
    let _ = read_command_complete(&mut stream);
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// 複数文メッセージ: トランザクション外で途中の文がエラーになっても
/// （明示トランザクションに入っていないため）最後の `ReadyForQuery` は
/// `'I'` のまま。
#[test]
fn multi_statement_error_without_transaction_stays_idle() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    // 先頭文は数値リテラルのみで `classify_statement` が `Rejected`
    // （`Write` ではない）に分類する非文（`statement_splitter` の単体テスト
    // `classify_statement("123")` 参照）。`SELEC ...`（未知の先頭トークン）は
    // 字句上 `Token::Ident(_)` の fail-closed 既定で `Write` 扱いになり、
    // 複数文中の非末尾書き込みとして暗黙トランザクション（Issue #1175）へ
    // 振り分けられるため、この形（逐次実行）の確認には使えない。
    send_simple_query(&mut stream, "123; SELECT id FROM documents LIMIT 1");
    expect_error_response_with_sqlstate(&mut stream, "42601");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// CopyInResponse（'G'）を読み捨てる（型バイト・長さ・本文）。
fn read_copy_in_response(stream: &mut std::net::TcpStream) {
    use std::io::Read;
    let mut header = [0u8; 1];
    stream
        .read_exact(&mut header)
        .expect("read CopyInResponse type");
    assert_eq!(header[0], b'G', "expected CopyInResponse");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");
}

fn send_copy_data(stream: &mut std::net::TcpStream, row: &[u8]) {
    use std::io::Write;
    let mut frame = vec![b'd'];
    frame.extend_from_slice(&((row.len() + 4) as i32).to_be_bytes());
    frame.extend_from_slice(row);
    stream.write_all(&frame).expect("send CopyData");
}

fn send_copy_done(stream: &mut std::net::TcpStream) {
    use std::io::Write;
    let mut done = vec![b'c'];
    done.extend_from_slice(&4i32.to_be_bytes());
    stream.write_all(&done).expect("send CopyDone");
}

/// `Active`（`InTransaction`）中の `COPY ... FROM STDIN` は共有書き込み
/// トランザクションへ書き込み（Issue #1179）、完了後の `ReadyForQuery` は `'T'` の
/// まま。`COMMIT` で確定し、別の文から見える。
#[test]
fn copy_inside_active_transaction_keeps_t_status_and_commits() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    send_simple_query(
        &mut stream,
        "COPY documents (id, embedding, body) FROM STDIN USING OPERATION_ID 'op-943-copy'",
    );
    read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, b"20\t[0.1,0.2,0.3]\tcopied\n");
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    send_simple_query(&mut stream, "COMMIT");
    assert_eq!(read_command_complete(&mut stream), "COMMIT");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    send_simple_query(&mut stream, "SELECT id FROM documents LIMIT 10");
    let _ = read_row_description(&mut stream);
    let _ = read_data_row(&mut stream);
    let _ = read_command_complete(&mut stream);
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// 明示トランザクション中の `COPY (...) TO STDOUT` は自トランザクションの未 commit の
/// 変更を返し（Issue #1179）、完了後の `ReadyForQuery` は `'T'` のまま。別接続からは
/// 未 commit の行が見えない。
#[test]
fn copy_to_stdout_inside_active_transaction_returns_uncommitted_rows() {
    use std::io::Read;
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(Arc::clone(&core));

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');
    send_simple_query(&mut stream, &insert_sql(7, "op-1179-copy-to"));
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    send_simple_query(
        &mut stream,
        "COPY (SELECT id FROM documents LIMIT 100) TO STDOUT",
    );
    let mut rows: Vec<Vec<u8>> = Vec::new();
    loop {
        let mut header = [0u8; 1];
        stream.read_exact(&mut header).expect("read type");
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).expect("read len");
        let len = i32::from_be_bytes(len_buf) as usize;
        let mut body = vec![0u8; len - 4];
        stream.read_exact(&mut body).expect("read body");
        match header[0] {
            b'H' => {}
            b'd' => rows.push(body),
            b'c' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    assert_eq!(rows, vec![b"7\n".to_vec()]);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    // 別接続からは未 commit の行が見えない。
    let mut other = spawn_alice(Arc::clone(&core));
    send_simple_query(&mut other, "SELECT id FROM documents LIMIT 10");
    let _ = read_row_description(&mut other);
    let _ = read_command_complete(&mut other);
    assert_eq!(read_ready_for_query_status(&mut other), b'I');

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// `Active` 中の COPY を `ROLLBACK` すると書き込みは破棄され、`Active` 中の COPY を
/// クライアントが `CopyFail` で中断した場合は `Failed`（`'E'`）へ遷移する。
#[test]
fn copy_fail_inside_active_transaction_fails_transaction_with_e_status() {
    use std::io::Write;
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    send_simple_query(
        &mut stream,
        "COPY documents (id, embedding, body) FROM STDIN USING OPERATION_ID 'op-943-copy-fail'",
    );
    read_copy_in_response(&mut stream);
    send_copy_data(&mut stream, b"20\t[0.1,0.2,0.3]\tcopied\n");
    let msg = b"abort\0";
    let mut frame = vec![b'f'];
    frame.extend_from_slice(&((msg.len() + 4) as i32).to_be_bytes());
    frame.extend_from_slice(msg);
    stream.write_all(&frame).expect("send CopyFail");
    expect_error_response_with_sqlstate(&mut stream, "22000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// `Active` 中の COPY 中に行データが不正な場合（デコード失敗）も `Failed` へ遷移する。
#[test]
fn copy_decode_error_inside_active_transaction_fails_transaction() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    send_simple_query(
        &mut stream,
        "COPY documents (id, embedding, body) FROM STDIN USING OPERATION_ID 'op-943-copy-bad'",
    );
    read_copy_in_response(&mut stream);
    // embedding 次元が宣言（3）と不一致。
    send_copy_data(&mut stream, b"20\t[0.1,0.2]\tcopied\n");
    send_copy_done(&mut stream);
    expect_error_response_with_sqlstate(&mut stream, "22000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// `Idle` 中に COPY が完了すると（`crate::copy::run` へ委譲される唯一の
/// 状態）`ReadyForQuery` は常に `'I'`（`copy.rs` は状態バイトを引数に取らず
/// 固定で `Idle` を送出する設計。`post_auth_loop` が `Idle` の場合しか
/// `copy::run` へ委譲しないため整合する）。
#[test]
fn copy_completes_with_idle_status_outside_transaction() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    send_simple_query(
        &mut stream,
        "COPY documents (id, embedding, body) FROM STDIN USING OPERATION_ID 'op-943-copy-idle'",
    );
    // CopyInResponse('G') → CopyData 1 行 → CopyDone。
    let mut header = [0u8; 1];
    use std::io::{Read, Write};
    stream
        .read_exact(&mut header)
        .expect("read CopyInResponse type");
    assert_eq!(header[0], b'G', "expected CopyInResponse");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read len");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");

    let row = b"20\t[0.1,0.2,0.3]\tcopied\n";
    let mut frame = Vec::new();
    frame.push(b'd');
    frame.extend_from_slice(&((row.len() + 4) as i32).to_be_bytes());
    frame.extend_from_slice(row);
    stream.write_all(&frame).expect("send CopyData");
    let mut done = Vec::new();
    done.push(b'c');
    done.extend_from_slice(&4i32.to_be_bytes());
    stream.write_all(&done).expect("send CopyDone");

    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// 拡張クエリプロトコルで Execute 段のエラー（Parse/Bind は成功し、実行時に
/// `23505` の重複 `operation_id` で失敗する）が起きた場合も、Sync 時点で
/// `Failed`（`'E'`）へ遷移する。`ROLLBACK` の Parse/Bind/Execute → Sync で
/// `'I'` へ戻る（`wire942_extended_transaction.rs` は Parse エラー・Bind
/// エラー・Describe エラーのみ固定しており、Execute 段の実行時エラーは
/// 未固定だった）。
#[test]
fn extended_execute_time_error_inside_transaction_fails_with_e_status_at_sync() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    fn parse_and_bind(stream: &mut std::net::TcpStream, statement: &str, portal: &str, sql: &str) {
        send_length_prefixed_message(stream, b'P', &parse_body(statement, sql, 0));
        let (kind, _) = read_message(stream);
        assert_eq!(kind, b'1', "expected ParseComplete");
        send_length_prefixed_message(stream, b'B', &bind_body(portal, statement));
        let (kind, _) = read_message(stream);
        assert_eq!(kind, b'2', "expected BindComplete");
    }
    fn send_sync(stream: &mut std::net::TcpStream) {
        send_length_prefixed_message(stream, b'S', b"");
    }

    // BEGIN。
    parse_and_bind(&mut stream, "begin1", "pb1", "BEGIN");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pb1", 0));
    let (kind, tag) = read_message(&mut stream);
    assert_eq!(kind, b'C', "expected CommandComplete");
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "BEGIN");
    send_sync(&mut stream);
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    // 同一 operation_id の INSERT を 2 回 Execute する（1 回目は成功、2 回目は
    // 台帳照合による `23505` で Execute 段で失敗する。RECOVER-3/TASK-101）。
    let insert = insert_sql(21, "op-943-dup");
    parse_and_bind(&mut stream, "ins1", "pi1", &insert);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pi1", 0));
    let (kind, tag) = read_message(&mut stream);
    assert_eq!(kind, b'C', "expected CommandComplete for first insert");
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "INSERT 0 1");
    send_sync(&mut stream);
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    parse_and_bind(&mut stream, "ins2", "pi2", &insert);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pi2", 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(
        kind, b'E',
        "expected ErrorResponse for duplicate operation_id"
    );
    send_sync(&mut stream);
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    // ROLLBACK の Parse/Bind/Execute → Sync で `'I'` へ戻る。
    parse_and_bind(&mut stream, "rb1", "prb1", "ROLLBACK");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("prb1", 0));
    let (kind, tag) = read_message(&mut stream);
    assert_eq!(kind, b'C', "expected CommandComplete for ROLLBACK");
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "ROLLBACK");
    send_sync(&mut stream);
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// 拡張クエリプロトコル: トランザクション外で Execute 段がエラーになっても
/// （明示トランザクションを使っていないため）Sync 後は `'I'` のまま、以降の
/// 簡易クエリも通常どおり継続できる。
#[test]
fn extended_execute_time_error_outside_transaction_stays_idle_at_sync() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    let insert = insert_sql(22, "op-943-outside-dup");
    // 事前に一度 autocommit で投入し、台帳に operation_id を記録させる。
    send_simple_query(&mut stream, &insert);
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    send_length_prefixed_message(&mut stream, b'P', &parse_body("dup1", &insert, 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1', "expected ParseComplete");
    send_length_prefixed_message(&mut stream, b'B', &bind_body("pdup1", "dup1"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'2', "expected BindComplete");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pdup1", 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(
        kind, b'E',
        "expected ErrorResponse for duplicate operation_id"
    );
    send_length_prefixed_message(&mut stream, b'S', b"");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    // 続く簡易クエリも通常どおり動く（事前投入の id=22 が 1 行返るため
    // `DataRow` を読み飛ばしてから `CommandComplete` を読む）。
    send_simple_query(&mut stream, "SELECT id FROM documents LIMIT 1");
    let _ = read_row_description(&mut stream);
    let _ = read_data_row(&mut stream);
    let _ = read_command_complete(&mut stream);
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// 暗黙トランザクション（Issue #1175）: 書き込みが最後以外にある複数文メッセージは、
/// 成功して commit された場合も、途中でエラーになりロールバックされた場合も、最後の
/// `ReadyForQuery` が `'I'`（`'T'`／`'E'` にならない）。
#[test]
fn implicit_transaction_success_and_failure_both_report_idle() {
    let (core, _guard) = new_core_with_documents_table();
    let mut stream = spawn_alice(core);

    send_simple_query(
        &mut stream,
        &format!(
            "{}; {}",
            insert_sql(1, "imp-ok-1"),
            insert_sql(2, "imp-ok-2")
        ),
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    send_simple_query(
        &mut stream,
        &format!(
            "{}; SELECT id FROM no_such_table LIMIT 1",
            insert_sql(3, "imp-ng-1")
        ),
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    expect_error_response_with_sqlstate(&mut stream, "42P01");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');

    // 失敗後も接続は `Failed`（`25P02`）にならず、次のメッセージを処理できる。
    send_simple_query(&mut stream, "SELECT id FROM documents LIMIT 10");
    let _ = read_row_description(&mut stream);
    let _ = read_data_row(&mut stream);
    let _ = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 2");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}
