//! 複数文メッセージ内の `COPY` の原子性とエラー伝播の検証（Issue #1774・親 #1772・WIRE-20。
//! 層 A・生バイトクライアント）。
//!
//! #1773（`wire20_copy_multi_statement.rs`）が確定した「COPY を CopyIn／CopyOut へ分割して
//! 実行する」経路について、失敗要因（後続文のエラー・デコード失敗・INDEX-4 超過・commit 失敗・
//! フレーム層の失敗・切断）と COPY の位置（先頭・途中・最後）の組み合わせごとに、暗黙
//! トランザクションでは先行文の変更が一切残らず `ReadyForQuery('I')` が 1 回だけ返ること、
//! 明示トランザクションでは `Failed`（`'E'`）へ遷移し本リポの 25P02 規則に従うことを固定する。
//! 遅延 FK（commit 失敗の唯一の再現手段）の DDL は wire19 と同じく engine へ直接発行する。
//! ポインタ: WIRE-20・SQL-31・INDEX-4（`docs/spec/04-behavior/` 配下）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

// 遅延 FK 付きの p/c 表を従来方式の表として作る（wire19 と同じ。`LegacyExec`）。
#[path = "../../engine/src/test_util/legacy_table.rs"]
mod legacy_table;
use legacy_table::LegacyExec as _;

use std::io::Read;
use std::sync::Arc;

use engine::batch_limits::BatchLimits;
use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

/// docs 表（任意で遅延 FK の p/c 表と BatchLimits）を持つ core を作る。
fn build_core(
    with_fk_tables: bool,
    limits: Option<BatchLimits>,
) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire20-copy-atomicity");
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
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
    if let Some(limits) = limits {
        core = core.with_batch_limits(limits);
    }
    if with_fk_tables {
        let mut ddl_session = engine::sql::mode::SessionState::default();
        ddl_session.allow_ddl();
        let sys = engine::policy::PolicyContext::new("sys").expect("valid tenant id");
        for sql in [
            "CREATE TABLE p (name TEXT)",
            "CREATE TABLE c (parent_id BIGINT REFERENCES p DEFERRABLE INITIALLY DEFERRED)",
        ] {
            core.execute_sql_in_session_legacy(&sys, &mut ddl_session, sql)
                .expect("ddl succeeds");
        }
    }
    (Arc::new(core), guard)
}

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    build_core(false, None)
}

fn new_core_with_fk() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    build_core(true, None)
}

/// 4 項目すべてを明示した INDEX-4 上限（1 バッチ 2 チャンクまで）。
fn chunk_limits() -> BatchLimits {
    BatchLimits {
        max_files_per_batch: 64,
        max_file_body_bytes: 1024 * 1024,
        max_batch_total_bytes: 1024 * 1024,
        max_batch_chunks: 2,
    }
}

fn spawn_addr(core: Arc<EngineCore>) -> (std::net::SocketAddr, common::UserStoreFile) {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    (addr, users_path)
}

fn connect_alice(addr: std::net::SocketAddr) -> std::net::TcpStream {
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

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

fn send_copy_fail(stream: &mut std::net::TcpStream, reason: &str) {
    let mut body = reason.as_bytes().to_vec();
    body.push(0);
    send_length_prefixed_message(stream, b'f', &body);
}

fn count_table(stream: &mut std::net::TcpStream, table: &str) -> usize {
    send_simple_query(stream, &format!("SELECT id FROM {table} LIMIT 100"));
    let _cols = read_row_description(stream);
    let mut n = 0;
    loop {
        let (t, body) = read_message(stream);
        match t {
            b'D' => n += 1,
            b'C' => {
                assert!(body.starts_with(b"SELECT"));
                break;
            }
            other => panic!("unexpected message {other}"),
        }
    }
    read_ready_for_query(stream);
    n
}

fn count_docs(stream: &mut std::net::TcpStream) -> usize {
    count_table(stream, "docs")
}
const INS: &str = "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja')";
const ORPHAN: &str = "INSERT INTO c (id, parent_id) VALUES (1, 999)";

/// ErrorResponse を読み、SQLSTATE（本文の 'C' フィールド）を返す。
fn read_error_sqlstate(stream: &mut std::net::TcpStream) -> String {
    let (t, body) = read_message(stream);
    assert_eq!(t, b'E', "expected ErrorResponse, got {}", t as char);
    for field in body.split(|b| *b == 0) {
        if let Some(rest) = field.strip_prefix(b"C") {
            return String::from_utf8_lossy(rest).into_owned();
        }
    }
    panic!("no SQLSTATE field in ErrorResponse");
}

/// CopyIn を受け、2 行を送って CopyDone する。
fn feed_two_rows_and_done(stream: &mut std::net::TcpStream) {
    assert_eq!(read_copy_in_response(stream), 3);
    send_copy_data(stream, b"2\t[0.0,1.0]\ten\n3\t[1.0,1.0]\ten\n");
    send_copy_done(stream);
}

/// ライタが解放済みであること（別接続の COPY FROM が完了する）を確認する。
fn assert_writer_released(addr: std::net::SocketAddr, op: &str) {
    let mut other = connect_alice(addr);
    assert_eq!(count_docs(&mut other), 0);
    send_simple_query(
        &mut other,
        &format!("COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID '{op}'"),
    );
    assert_eq!(read_copy_in_response(&mut other), 3);
    send_copy_data(&mut other, b"9\t[1.0,0.0]\tja\n");
    send_copy_done(&mut other);
    assert_eq!(read_command_complete(&mut other), "COPY 1");
    read_ready_for_query(&mut other);
}

// ---------------------------------------------------------------------------
// 暗黙トランザクション（Idle から開始し 'I' へ収束）
// ---------------------------------------------------------------------------

/// A1/A1b: COPY が成功した後の後続文エラーで COPY の行が残らず、`operation_id` 台帳も
/// 永続化されない（同じ ID の再送が成功する）。
#[test]
fn wire20_atomic_later_statement_error_rolls_back_copy_and_ledger() {
    let (core, _guard) = new_core();
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20a-a1'; \
         SELECT id FROM no_such_table LIMIT 1",
    );
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    assert_eq!(read_error_sqlstate(&mut stream), "42P01");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 0);

    // A1b: 台帳が残っていれば 23505 になる。
    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20a-a1'",
    );
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 1);
}

/// A2: 途中の INSERT の後の COPY がデコード失敗（次元不一致）。INSERT も残らず、
/// 後続の `SELECT 1` は実行されない。
#[test]
fn wire20_atomic_copy_decode_failure_rolls_back_preceding_insert() {
    let (core, _guard) = new_core();
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(
        &mut stream,
        &format!(
            "{INS} USING OPERATION_ID 'w20a-a2i'; \
             COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20a-a2c'; SELECT 1"
        ),
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(&mut stream, b"2\t[1.0,0.0]\tja\n3\t[1.0,0.0,0.0]\tja\n");
    send_copy_done(&mut stream);
    assert_eq!(read_error_sqlstate(&mut stream), "22000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 0);
}

/// A3: INDEX-4 超過（CopyDone 後の `54000`）。先行 INSERT も残らない。
#[test]
fn wire20_atomic_copy_batch_limit_exceeded_rolls_back_preceding_insert() {
    let (core, _guard) = build_core(false, Some(chunk_limits()));
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(
        &mut stream,
        &format!(
            "{INS} USING OPERATION_ID 'w20a-a3i'; \
             COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20a-a3c'"
        ),
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(
        &mut stream,
        b"2\t[1.0,0.0]\tja\n3\t[0.0,1.0]\tja\n4\t[1.0,1.0]\tja\n",
    );
    send_copy_done(&mut stream);
    assert_eq!(read_error_sqlstate(&mut stream), "54000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 0);
}

/// A4: COPY FROM が最後の文で、遅延 FK 違反により暗黙 commit が失敗する。`COPY n` は
/// 送られず、docs・c のどちらにも行が残らない。
#[test]
fn wire20_atomic_deferred_fk_commit_failure_at_copy_from_last() {
    let (core, _guard) = new_core_with_fk();
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(
        &mut stream,
        &format!(
            "{ORPHAN} USING OPERATION_ID 'w20a-a4i'; \
             COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20a-a4c'"
        ),
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    feed_two_rows_and_done(&mut stream);
    assert_eq!(read_error_sqlstate(&mut stream), "23503");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 0);
    assert_eq!(count_table(&mut stream, "c"), 0);
}

/// A4b: COPY が孤児行を持ち込む。FK 検査は遅延されるため CopyDone 後に `COPY 1` が先に返り、
/// 後続 `SELECT 1` の暗黙 commit で `23503` になる。行は残らない（fail-closed）。
#[test]
fn wire20_atomic_copy_from_orphan_row_never_persists() {
    let (core, _guard) = new_core_with_fk();
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(
        &mut stream,
        "COPY c (id, parent_id) FROM STDIN USING OPERATION_ID 'w20a-a4b'; SELECT 1",
    );
    assert_eq!(read_copy_in_response(&mut stream), 2);
    send_copy_data(&mut stream, b"1\t999\n");
    send_copy_done(&mut stream);
    let (t, body) = read_message(&mut stream);
    // 遅延 FK は COPY の行適用時には検査されず、最後の文 `SELECT 1` の暗黙 commit で失敗する。
    // よって `COPY 1` が先に送られ、その後に `23503`（commit 失敗）が来る順序で固定する。
    assert_eq!(t, b'C', "body={:?}", String::from_utf8_lossy(&body));
    assert!(body.starts_with(b"COPY 1"));
    assert_eq!(read_error_sqlstate(&mut stream), "23503");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_table(&mut stream, "c"), 0);
}

/// A5: COPY TO が最後の文で、暗黙 commit が失敗する。CopyOutResponse を 1 バイトも
/// 送らず `23503`。
#[test]
fn wire20_atomic_deferred_fk_commit_failure_at_copy_to_last() {
    let (core, _guard) = new_core_with_fk();
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(
        &mut stream,
        &format!(
            "{ORPHAN} USING OPERATION_ID 'w20a-a5'; \
             COPY (SELECT id FROM c LIMIT 10) TO STDOUT"
        ),
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_error_sqlstate(&mut stream), "23503");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_table(&mut stream, "c"), 0);
}

/// A6: COPY FROM が途中の文で、最後の INSERT（孤児）の commit が失敗する。
#[test]
fn wire20_atomic_deferred_fk_commit_failure_after_copy_from() {
    let (core, _guard) = new_core_with_fk();
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(
        &mut stream,
        &format!(
            "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20a-a6c'; \
             {ORPHAN} USING OPERATION_ID 'w20a-a6i'"
        ),
    );
    feed_two_rows_and_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 2");
    assert_eq!(read_error_sqlstate(&mut stream), "23503");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 0);
    assert_eq!(count_table(&mut stream, "c"), 0);
}

/// A7: COPY TO が途中の文で、後続文がエラー。CopyOut は送信済みだが INSERT は残らない。
#[test]
fn wire20_atomic_copy_to_in_the_middle_then_error_rolls_back_insert() {
    let (core, _guard) = new_core();
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(
        &mut stream,
        &format!(
            "{INS} USING OPERATION_ID 'w20a-a7'; \
             COPY (SELECT id FROM docs LIMIT 10) TO STDOUT; \
             SELECT id FROM no_such_table LIMIT 1"
        ),
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_copy_out_response(&mut stream), 1);
    assert_eq!(read_copy_data(&mut stream), b"1\n");
    read_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    assert_eq!(read_error_sqlstate(&mut stream), "42P01");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 0);
}

/// A8: COPY TO が途中の文で binary 非対応型を理由に拒否される。後続文は実行されない。
#[test]
fn wire20_atomic_copy_to_binary_unsupported_in_the_middle_rolls_back_insert() {
    let (core, _guard) = new_core();
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(
        &mut stream,
        &format!(
            "{INS} USING OPERATION_ID 'w20a-a8'; \
             COPY (SELECT embedding FROM docs LIMIT 10) TO STDOUT WITH (FORMAT binary); SELECT 1"
        ),
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_error_sqlstate(&mut stream), "0A000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 0);
}

/// A9: INDEX-4 超過後の CopyData が読み捨て予算を超えると、ErrorResponse の後に切断される。
/// 先行 INSERT は残らず、ライタは解放される。
#[test]
fn wire20_atomic_discard_budget_exceeded_disconnects_and_releases_writer() {
    let (core, _guard) = build_core(false, Some(chunk_limits()));
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(
        &mut stream,
        &format!(
            "{INS} USING OPERATION_ID 'w20a-a9i'; \
             COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20a-a9c'"
        ),
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(
        &mut stream,
        b"2\t[1.0,0.0]\tja\n3\t[0.0,1.0]\tja\n4\t[1.0,1.0]\tja\n",
    );
    for _ in 0..=wire_server::limits::COPY_DISCARD_MAX_MESSAGES {
        send_copy_data(&mut stream, b"x");
    }
    let (t, body) = read_message(&mut stream);
    assert_eq!(t, b'E');
    assert!(String::from_utf8_lossy(&body).contains("08P01"));
    expect_connection_closed(&mut stream);
    assert_writer_released(addr, "w20a-a9-released");
}

/// A10: CopyIn 中に Terminate なしでソケットを閉じる（早期 EOF）。何も残らずライタが解放される。
#[test]
fn wire20_atomic_early_eof_during_copy_in_discards_rows_and_releases_writer() {
    let (core, _guard) = new_core();
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(
        &mut stream,
        &format!(
            "{INS} USING OPERATION_ID 'w20a-a10i'; \
             COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20a-a10c'"
        ),
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(&mut stream, b"2\t[0.0,1.0]\ten\n");
    let _ = stream.shutdown(std::net::Shutdown::Both);
    drop(stream);
    assert_writer_released(addr, "w20a-a10-released");
}

/// A11: メッセージ内の `SET search_mode` は、COPY の CopyFail で失敗したときメッセージ
/// 受信前の値へ戻る。句なし `SELECT` の件数（recall は 3 行、precision は top1 のみの 1 行）
/// で観測する（wire16 と同じ手法）。
#[test]
fn wire20_atomic_copy_fail_restores_session_state() {
    let (core, _guard) = new_core();
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    // 種データ（[1,0] に対し top1 のみ明確になる 3 行）。
    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20a-a11s'",
    );
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(
        &mut stream,
        b"1\t[1.0,0.0]\tja\n2\t[0.0,1.0]\tja\n3\t[0.0,1.0]\tja\n",
    );
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 3");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SET search_mode = 'precision'; \
         COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20a-a11c'",
    );
    assert_eq!(read_command_complete(&mut stream), "SET");
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_fail(&mut stream, "client abort");
    let _ = read_error_sqlstate(&mut stream);
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 3);

    send_simple_query(
        &mut stream,
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3",
    );
    let _ = read_row_description(&mut stream);
    for _ in 0..3 {
        let _ = read_data_row(&mut stream);
    }
    assert_eq!(
        read_command_complete(&mut stream),
        "SELECT 3",
        "SET search_mode must be rolled back to the default (recall)"
    );
    read_ready_for_query(&mut stream);
}

/// A12: 同じメッセージ内で `operation_id` を再利用すると拒否され、COPY の行も残らない。
#[test]
fn wire20_atomic_operation_id_reuse_in_one_message_rolls_back_copy() {
    let (core, _guard) = new_core();
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(
        &mut stream,
        &format!(
            "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20a-a12'; \
             {INS} USING OPERATION_ID 'w20a-a12'"
        ),
    );
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(&mut stream, b"2\t[0.0,1.0]\ten\n");
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    // 同一トランザクション内の operation_id 再利用は 25000（トランザクション状態不正）で拒否される。
    assert_eq!(read_error_sqlstate(&mut stream), "25000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 0);
}

// ---------------------------------------------------------------------------
// 明示トランザクション（Failed と 25P02 規則の整合）
// ---------------------------------------------------------------------------

/// B1/B2: 明示トランザクション内の CopyFail で `'E'` へ遷移し `COMMIT` は実行されない。
/// 続く `COMMIT` は本リポの fail-closed 契約で `25P02`（`'E'` のまま）、`ROLLBACK` で `'I'`。
#[test]
fn wire20_explicit_txn_copy_fail_enters_failed_and_commit_is_25p02() {
    let (core, _guard) = new_core();
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(
        &mut stream,
        &format!(
            "BEGIN; {INS} USING OPERATION_ID 'w20b-b1i'; \
             COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20b-b1c'; COMMIT"
        ),
    );
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_fail(&mut stream, "client abort");
    let _ = read_error_sqlstate(&mut stream);
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(&mut stream, "COMMIT");
    assert_eq!(read_error_sqlstate(&mut stream), "25P02");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 0);
}

/// B3/B4: `Failed` 中の COPY FROM は CopyIn へ入らず `25P02`。複数文でも先頭で止まり、
/// 後続の `SELECT 1` は実行されない。
#[test]
fn wire20_failed_txn_rejects_copy_from_without_entering_copy_in() {
    let (core, _guard) = new_core();
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');

    // B3: デコード失敗で Failed へ。
    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20b-b3a'",
    );
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(&mut stream, b"1\t[1.0,0.0,0.0]\tja\n");
    send_copy_done(&mut stream);
    assert_eq!(read_error_sqlstate(&mut stream), "22000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20b-b3b'",
    );
    assert_eq!(read_error_sqlstate(&mut stream), "25P02");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    // B4: 複数文。
    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20b-b4'; SELECT 1",
    );
    assert_eq!(read_error_sqlstate(&mut stream), "25P02");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 0);
}

/// B5: 明示トランザクション中の COPY TO が binary 非対応型で拒否されると `'E'`。
/// 後続文は実行されない。
#[test]
fn wire20_explicit_txn_copy_to_binary_unsupported_enters_failed() {
    let (core, _guard) = new_core();
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(
        &mut stream,
        "BEGIN; COPY (SELECT embedding FROM docs LIMIT 10) TO STDOUT WITH (FORMAT binary); SELECT 1",
    );
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_error_sqlstate(&mut stream), "0A000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');
    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// B6: `Failed` 中でも `ROLLBACK` の後の COPY は通常どおり実行される（autocommit）。
#[test]
fn wire20_copy_after_rollback_in_same_message_runs_normally() {
    let (core, _guard) = new_core();
    let (addr, _users) = spawn_addr(core);
    let mut stream = connect_alice(addr);
    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');
    send_simple_query(&mut stream, "SELECT id FROM no_such_table LIMIT 1");
    assert_eq!(read_error_sqlstate(&mut stream), "42P01");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(
        &mut stream,
        "ROLLBACK; COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20b-b6'",
    );
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 1);
}
