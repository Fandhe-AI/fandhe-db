//! 複数文メッセージ内の `COPY` を分割して実行する検証（Issue #1773・親 #1772・WIRE-20。
//! 層 A・生バイトクライアント）。
//!
//! 暗黙トランザクション（書き込みが最後以外にある複数文メッセージ）の中でも、COPY の文で
//! CopyIn／CopyOut サブプロトコルへ入り、CopyDone の後に同じメッセージの続きの文へ進む。
//! 応答の順序（先行文 → COPY のサブプロトコル → `COPY n` → 後続文 → `ReadyForQuery` を 1 回）と
//! commit 位置を固定する。失敗時のロールバックの網羅は `wire20_copy_atomicity.rs`（#1774）が担う（ここでは最小確認のみ）。

use crate::common;

use crate::temp_db;

use std::io::Read;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

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
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
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

fn count_docs(stream: &mut std::net::TcpStream) -> usize {
    send_simple_query(stream, "SELECT id FROM docs LIMIT 100");
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

/// T1: COPY が先頭で後ろに SELECT が続く。COPY した行が続きの文から見える。
#[test]
fn wire20_copy_from_then_select_responds_in_order() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);
    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20-t1'; SELECT id FROM docs LIMIT 10",
    );
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n2\t[0.0,1.0]\ten\n");
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 2");
    let _cols = read_row_description(&mut stream);
    let _ = read_data_row(&mut stream);
    let _ = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 2");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 2);
}

/// T2: 最後の文が COPY FROM。INSERT と COPY の両方が commit される。
#[test]
fn wire20_insert_then_copy_from_last_commits_both() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'w20-t2a'; \
         COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20-t2b'",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(&mut stream, b"2\t[0.0,1.0]\ten\n");
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 2);
}

/// T3: 最後の文が COPY TO。未 commit の INSERT 行が CopyOut に含まれ、commit される。
#[test]
fn wire20_insert_then_copy_to_last_sees_uncommitted_row_and_commits() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'w20-t3'; \
         COPY (SELECT id FROM docs LIMIT 100) TO STDOUT",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_copy_out_response(&mut stream), 1);
    assert_eq!(read_copy_data(&mut stream), b"1\n");
    read_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 1);
}

/// T4: COPY TO が途中。最後にだけ ReadyForQuery が来る。
#[test]
fn wire20_copy_to_in_the_middle_responds_in_order() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'w20-t4'; \
         COPY (SELECT id FROM docs LIMIT 100) TO STDOUT; SELECT id FROM docs LIMIT 1",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_copy_out_response(&mut stream), 1);
    assert_eq!(read_copy_data(&mut stream), b"1\n");
    read_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    let _cols = read_row_description(&mut stream);
    let _ = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 1);
}

/// T5: 1 メッセージに COPY FROM が 2 回。CopyIn が順に開始・完了する。
#[test]
fn wire20_two_copy_from_in_one_message_run_sequentially() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);
    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20-t5a'; \
         COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20-t5b'; \
         SELECT id FROM docs LIMIT 10",
    );
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(&mut stream, b"2\t[0.0,1.0]\ten\n");
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    let _cols = read_row_description(&mut stream);
    let _ = read_data_row(&mut stream);
    let _ = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 2");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// T6: 文数の上限（17 文）超過は COPY を含んでいても CopyIn へ入らず `54000`。
#[test]
fn wire20_too_many_statements_with_copy_is_rejected_before_copy_in() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);
    let mut sql =
        String::from("COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20-t6'");
    for _ in 0..16 {
        sql.push_str("; SELECT 1");
    }
    send_simple_query(&mut stream, &sql);
    expect_error_response_with_sqlstate(&mut stream, "54000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 0);
}

/// T7a: 暗黙トランザクション内の CopyFail で先行 INSERT も含め何も残らず、デシンクしない。
#[test]
fn wire20_copy_fail_rolls_back_whole_message_and_stays_in_sync() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'w20-t7a'; \
         COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20-t7b'; SELECT 1",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(&mut stream, b"2\t[0.0,1.0]\ten\n");
    send_copy_fail(&mut stream, "client abort");
    let (t, _) = read_message(&mut stream);
    assert_eq!(t, b'E', "expected ErrorResponse");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 0);
}

/// T7b: 暗黙トランザクション内の COPY 中に Terminate を送ると接続が閉じ、何も残らない。
#[test]
fn wire20_terminate_during_copy_closes_connection_and_discards_rows() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(Arc::clone(&core));
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'w20-t7c'; \
         COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20-t7d'",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_length_prefixed_message(&mut stream, b'X', b"");
    expect_connection_closed(&mut stream);

    let mut second = spawn_with_alice(core);
    assert_eq!(count_docs(&mut second), 0);
}

/// T8a: 明示トランザクションの文中の COPY（既存の逐次経路）の応答順を固定する。
#[test]
fn wire20_copy_inside_explicit_transaction_responds_in_order() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);
    send_simple_query(
        &mut stream,
        "BEGIN; COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20-t8'; COMMIT",
    );
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_copy_in_response(&mut stream), 3);
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    assert_eq!(read_command_complete(&mut stream), "COMMIT");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 1);
}

/// T10: 暗黙トランザクション最後の COPY TO が binary 非対応型（vector）で拒否されるとき、
/// 暗黙 commit より前に拒否されるので先行 INSERT は永続化されない（原子性。PR #2649 指摘）。
#[test]
fn wire20_insert_then_copy_to_binary_unsupported_type_does_not_commit_insert() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[1.0,0.0]', 'ja') USING OPERATION_ID 'w20-t10'; \
         COPY (SELECT embedding FROM docs LIMIT 10) TO STDOUT WITH (FORMAT binary)",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    let (t, _) = read_message(&mut stream);
    assert_eq!(t, b'E', "expected ErrorResponse");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 0);
}
/// 持続時間上限つきのエンジンで暗黙トランザクションの COPY を受信させるサーバーを起動する。
fn spawn_with_short_max_duration() -> (
    std::net::SocketAddr,
    temp_db::CleanupGuard,
    common::UserStoreFile,
) {
    let limits = wire_server::transaction_limits_opt::resolve(
        wire_server::transaction_limits_opt::TransactionLimitsRawFlags {
            max_duration_ms: Some("300"),
            max_statements: None,
        },
    )
    .expect("valid --max-transaction-duration-ms");
    let (core, guard) = new_core_with_docs_table();
    let core = Arc::try_unwrap(core)
        .unwrap_or_else(|_| panic!("sole owner"))
        .with_transaction_limits(limits);
    let users = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users, Arc::new(core));
    (addr, guard, users)
}

/// 接続が応答なしで閉じられた（EOF またはリセット）ことを確認する。
fn assert_closed_without_response(stream: &mut std::net::TcpStream) {
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(3)))
        .expect("client read timeout");
    let mut buf = [0u8; 1];
    match stream.read(&mut buf) {
        Ok(0) | Err(_) => {}
        other => panic!("connection must close without a response: {other:?}"),
    }
}

/// 別接続の書き込みがライタ待ちで失敗せず完了する（ライタが解放済み）ことを確認する。
fn assert_writer_released(addr: std::net::SocketAddr) {
    let mut other = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    send_simple_query(
        &mut other,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20-released'",
    );
    assert_eq!(read_copy_in_response(&mut other), 3);
    send_copy_data(&mut other, b"9\t[1.0,0.0]\tja\n");
    send_copy_done(&mut other);
    assert_eq!(read_command_complete(&mut other), "COPY 1");
    read_ready_for_query(&mut other);
}

/// Issue #1773 レビュー指摘: 暗黙トランザクションの COPY 受信が無通信のまま持続時間上限を
/// 越えたら、応答なしで接続を閉じてライタを解放する。
#[test]
fn wire20_implicit_copy_idle_past_max_duration_releases_writer() {
    let (addr, _guard, _users) = spawn_with_short_max_duration();
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20-idle'; SELECT 1",
    );
    assert_eq!(read_copy_in_response(&mut stream), 3);
    assert_closed_without_response(&mut stream);
    assert_writer_released(addr);
}

/// Issue #1773 レビュー指摘: CopyData の本文を少しずつ送り続けても、持続時間上限で
/// 接続が閉じられライタが解放される。
#[test]
fn wire20_implicit_copy_slow_body_past_max_duration_releases_writer() {
    use std::io::Write;
    let (addr, _guard, _users) = spawn_with_short_max_duration();
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "pw-alice");
    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20-slow'; SELECT 1",
    );
    assert_eq!(read_copy_in_response(&mut stream), 3);
    // 長さ 1004 の CopyData を宣言し、本文を 1 バイトずつ送る。
    let mut frame = vec![b'd'];
    frame.extend_from_slice(&1004i32.to_be_bytes());
    let _ = stream.write_all(&frame);
    for _ in 0..40 {
        if stream.write_all(b"x").is_err() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    assert_closed_without_response(&mut stream);
    assert_writer_released(addr);
}

/// Issue #1773 レビュー指摘（PR #2649）: 明示トランザクション中の COPY FROM は、`post_auth_loop` が
/// 読み取りタイムアウトを短い poll 値（約 100ms）へ変更していても、CopyData を 100ms 以上
/// 送らないだけでは切断されない（受信予算は接続本来のタイムアウト）。
#[test]
fn wire20_explicit_txn_copy_idle_beyond_poll_interval_is_not_disconnected() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);
    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');
    send_simple_query(
        &mut stream,
        "COPY docs (id, embedding, lang) FROM STDIN USING OPERATION_ID 'w20-poll'",
    );
    assert_eq!(read_copy_in_response(&mut stream), 3);
    std::thread::sleep(std::time::Duration::from_millis(500));
    send_copy_data(&mut stream, b"1\t[1.0,0.0]\tja\n");
    std::thread::sleep(std::time::Duration::from_millis(500));
    send_copy_done(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "COPY 1");
    assert_eq!(read_ready_for_query_status(&mut stream), b'T');
    send_simple_query(&mut stream, "COMMIT");
    assert_eq!(read_command_complete(&mut stream), "COMMIT");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
    assert_eq!(count_docs(&mut stream), 1);
}
