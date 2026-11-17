//! カーソル `FETCH`（WIRE-15・TASK-218）を拡張クエリプロトコルで実行した
//! portal が中断保持（`PortalState::Suspended`）へ回った場合に、その後の
//! `CLOSE`／`COMMIT`／`ROLLBACK`（いずれも同一 Sync サイクル内。named portal
//! は Sync でのみ破棄されるため、Sync を跨がなければ生き残る）を挟んでから
//! 再 Execute しても、保持済みの行を送出しないことを固定する
//! （PR #1049 レビュー指摘・codex P1「終了したカーソルの FETCH portal から
//! 行を送出できる」の回帰防止）。
//!
//! `wire942_extended_transaction.rs`（トランザクション期限切れ後の portal
//! 拒否）・`wire11_bind_execute_sync.rs`（`max_rows` 分割送出）と同じ流儀
//! （生バイトの wire クライアント＋in-process サーバー）を使う。

use crate::common;

use crate::temp_db;

use std::io::Read;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core_with_documents_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire15-cursor-portal-binding");
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

fn owner_ctx() -> engine::policy::PolicyContext {
    engine::policy::PolicyContext::new("tenant-a").expect("valid tenant id")
}

fn seed_rows(core: &EngineCore, n: u64) {
    for i in 1..=n {
        let sql = format!(
            "INSERT INTO documents (id, embedding, body) VALUES ({i}, '[0.1,0.2,0.3]', 'row-{i}') USING OPERATION_ID 'seed-{i}'"
        );
        let mut session = engine::sql::mode::SessionState::default();
        core.execute_sql_in_session(&owner_ctx(), &mut session, &sql)
            .expect("seed insert succeeds");
    }
}

fn parse_body(name: &str, query: &str, num_param_types: i16) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body.extend_from_slice(query.as_bytes());
    body.push(0);
    body.extend_from_slice(&num_param_types.to_be_bytes());
    body
}

fn bind_body(portal: &str, statement: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes()); // param format code count
    body.extend_from_slice(&0i16.to_be_bytes()); // param count
    body.extend_from_slice(&0i16.to_be_bytes()); // result format code count
    body
}

fn execute_body(portal: &str, max_rows: i32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(&max_rows.to_be_bytes());
    body
}

fn read_message(stream: &mut std::net::TcpStream) -> (u8, Vec<u8>) {
    let mut type_byte = [0u8; 1];
    stream.read_exact(&mut type_byte).expect("read type byte");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let body_len = len.checked_sub(4).expect("length must be >= 4");
    let mut body = vec![0u8; body_len];
    stream.read_exact(&mut body).expect("read body");
    (type_byte[0], body)
}

fn send_sync(stream: &mut std::net::TcpStream) {
    send_length_prefixed_message(stream, b'S', b"");
}

fn parse_and_bind(stream: &mut std::net::TcpStream, statement: &str, portal: &str, sql: &str) {
    send_length_prefixed_message(stream, b'P', &parse_body(statement, sql, 0));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'1', "expected ParseComplete");
    send_length_prefixed_message(stream, b'B', &bind_body(portal, statement));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'2', "expected BindComplete");
}

/// `DECLARE`（内側 SELECT が `rows` 件を返す）→ 拡張クエリで `FETCH <rows> FROM
/// c` を named portal `pf` へ Bind → `max_rows=1` で Execute し、1 行 +
/// `PortalSuspended` を確認する（`rows >= 2` が前提）。
fn declare_and_suspend_fetch_portal(stream: &mut std::net::TcpStream, rows: u64) {
    send_simple_query(stream, "BEGIN");
    assert_eq!(read_command_complete(stream), "BEGIN");
    read_ready_for_query(stream);

    send_simple_query(
        stream,
        &format!("DECLARE c CURSOR FOR SELECT id FROM documents LIMIT {rows}"),
    );
    assert_eq!(read_command_complete(stream), "DECLARE CURSOR");
    read_ready_for_query(stream);

    parse_and_bind(stream, "sf", "pf", &format!("FETCH {rows} FROM c"));
    send_length_prefixed_message(stream, b'E', &execute_body("pf", 1));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'D', "expected DataRow");
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b's', "expected PortalSuspended");
}

/// 中断保持中の `pf` を再 Execute すると `34000`（invalid cursor name）が
/// 返り、`DataRow` を一切送出しないこと、`Sync` で同期が回復することを
/// 検証する。
fn assert_resume_rejected_without_leaking_rows(stream: &mut std::net::TcpStream) {
    send_length_prefixed_message(stream, b'E', &execute_body("pf", 0));
    let (kind, body) = read_message(stream);
    assert_eq!(
        kind, b'E',
        "a FETCH portal invalidated by CLOSE/COMMIT/ROLLBACK must not resume with buffered rows"
    );
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("34000"),
        "expected sqlstate 34000 (invalid cursor name), got {text:?}"
    );
    send_sync(stream);
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery after Sync recovery");
}

/// `CLOSE c` を挟んでから中断保持中の `pf`（`FETCH ... FROM c`）を再 Execute
/// しても、`CLOSE` 後の行は一切送出されない（PR #1049 レビュー指摘の再現
/// 条件そのもの）。
#[test]
fn suspended_fetch_portal_is_rejected_after_cursor_close() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    declare_and_suspend_fetch_portal(&mut stream, 5);

    // 同一 Sync サイクル内で `CLOSE c`（simple query。named portal は
    // simple query では破棄されない）。
    send_simple_query(&mut stream, "CLOSE c");
    assert_eq!(read_command_complete(&mut stream), "CLOSE CURSOR");
    read_ready_for_query(&mut stream);

    assert_resume_rejected_without_leaking_rows(&mut stream);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
}

/// `COMMIT` を挟んでから中断保持中の `pf` を再 Execute しても、カーソルが
/// 消えたトランザクション終了後の行は送出されない。
#[test]
fn suspended_fetch_portal_is_rejected_after_commit() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    declare_and_suspend_fetch_portal(&mut stream, 5);

    send_simple_query(&mut stream, "COMMIT");
    assert_eq!(read_command_complete(&mut stream), "COMMIT");
    read_ready_for_query(&mut stream);

    assert_resume_rejected_without_leaking_rows(&mut stream);
}

/// `ROLLBACK` を挟んでから中断保持中の `pf` を再 Execute しても、カーソルが
/// 消えたトランザクション終了後の行は送出されない。
#[test]
fn suspended_fetch_portal_is_rejected_after_rollback() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    declare_and_suspend_fetch_portal(&mut stream, 5);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);

    assert_resume_rejected_without_leaking_rows(&mut stream);
}

/// `CLOSE c` の後に同名で再 `DECLARE` しても、旧インスタンスに束縛された
/// 中断保持中の `pf` は再開できない（名前の一致だけでなくカーソル個体
/// 識別子まで検証していることの確認。`docs/design/sql-cursor.md`
/// 「wire-server 拡張クエリの portal 束縛」節参照）。
#[test]
fn suspended_fetch_portal_is_rejected_after_cursor_redeclare_with_same_name() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    declare_and_suspend_fetch_portal(&mut stream, 5);

    send_simple_query(&mut stream, "CLOSE c");
    assert_eq!(read_command_complete(&mut stream), "CLOSE CURSOR");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id FROM documents LIMIT 5",
    );
    assert_eq!(read_command_complete(&mut stream), "DECLARE CURSOR");
    read_ready_for_query(&mut stream);

    assert_resume_rejected_without_leaking_rows(&mut stream);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
}

/// 対照: `CLOSE`／`COMMIT`／`ROLLBACK` を挟まなければ、中断保持中の `pf` は
/// 従来どおり残り行を正しく送出できる（上記の拒否が過剰でないことの確認）。
#[test]
fn suspended_fetch_portal_still_resumes_without_intervening_close_or_commit() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    declare_and_suspend_fetch_portal(&mut stream, 5);

    let mut remaining = 0usize;
    loop {
        send_length_prefixed_message(&mut stream, b'E', &execute_body("pf", 1));
        let (kind, _) = read_message(&mut stream);
        if kind == b'C' {
            break;
        }
        assert_eq!(kind, b'D', "expected DataRow");
        remaining += 1;
        let (kind, _) = read_message(&mut stream);
        if kind == b'C' {
            break;
        }
        assert_eq!(kind, b's', "expected PortalSuspended");
    }
    assert_eq!(remaining, 4, "4 rows remain after the first suspended row");

    send_sync(&mut stream);
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery after Sync");

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
}

// --- `ROLLBACK TO SAVEPOINT` とカーソル束縛 portal（Issue #1946・SQL-69 (5)） --------
//
// `ROLLBACK TO` は世代を変えないが、savepoint より後に宣言したカーソルは engine が閉じる。
// 閉じたカーソルへ束縛された中断 portal は `cursor_id` が解決できず `34000` で拒否され、
// それ以前のカーソルへ束縛された portal は続きの行を送出できる。

/// simple query を 1 つ送り、期待する完了タグと ReadyForQuery を読む。
fn simple_ok(stream: &mut std::net::TcpStream, sql: &str, tag: &str) {
    send_simple_query(stream, sql);
    assert_eq!(read_command_complete(stream), tag);
    read_ready_for_query(stream);
}

/// `FETCH 5 FROM c` を `pf` へ Bind し `max_rows=1` で中断させる。
fn suspend_fetch_portal(stream: &mut std::net::TcpStream) {
    parse_and_bind(stream, "sf", "pf", "FETCH 5 FROM c");
    send_length_prefixed_message(stream, b'E', &execute_body("pf", 1));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'D', "expected DataRow");
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b's', "expected PortalSuspended");
}

/// savepoint より後に宣言したカーソルの中断 portal は、`ROLLBACK TO` の後に再開できない。
#[test]
fn suspended_fetch_portal_is_rejected_after_rollback_to_closes_cursor() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    simple_ok(&mut stream, "BEGIN", "BEGIN");
    simple_ok(&mut stream, "SAVEPOINT a", "SAVEPOINT");
    simple_ok(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id FROM documents LIMIT 5",
        "DECLARE CURSOR",
    );
    suspend_fetch_portal(&mut stream);
    simple_ok(&mut stream, "ROLLBACK TO a", "ROLLBACK");

    assert_resume_rejected_without_leaking_rows(&mut stream);

    simple_ok(&mut stream, "ROLLBACK", "ROLLBACK");
}

/// savepoint より前に宣言したカーソルの中断 portal は、`ROLLBACK TO` の後も続きの行を出せる。
#[test]
fn suspended_fetch_portal_resumes_after_rollback_to_for_older_cursor() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    simple_ok(&mut stream, "BEGIN", "BEGIN");
    simple_ok(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id FROM documents LIMIT 5",
        "DECLARE CURSOR",
    );
    simple_ok(&mut stream, "SAVEPOINT a", "SAVEPOINT");
    suspend_fetch_portal(&mut stream);
    simple_ok(&mut stream, "ROLLBACK TO a", "ROLLBACK");

    send_length_prefixed_message(&mut stream, b'E', &execute_body("pf", 0));
    for _ in 0..4 {
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b'D', "expected DataRow");
    }
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'C', "expected CommandComplete");
    send_sync(&mut stream);
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery after Sync");

    simple_ok(&mut stream, "ROLLBACK", "ROLLBACK");
}

// --- 完了済み（Done）FETCH portal の再 Execute（PR #1049 レビュー指摘 Cursor
// Bugbot Low の回帰防止） ------------------------------------------------------
//
// 束縛検証は保持行を送出し得る `Suspended` のみが対象。全行送出済みの `Done`
// portal は `CLOSE`／`COMMIT`／`ROLLBACK` を挟んでも、`FETCH` 以外の portal と
// 同じく保持済みの完了タグだけを再送する（行は一切送出しない）。

/// `DECLARE`（内側 SELECT が `rows` 件を返す）→ 拡張クエリで `FETCH <rows> FROM
/// c` を named portal `pf` へ Bind → `max_rows=0` で Execute して全行＋
/// `CommandComplete` を受け取り、portal を `Done` にする。完了タグを返す。
fn declare_and_complete_fetch_portal(stream: &mut std::net::TcpStream, rows: u64) -> Vec<u8> {
    send_simple_query(stream, "BEGIN");
    assert_eq!(read_command_complete(stream), "BEGIN");
    read_ready_for_query(stream);

    send_simple_query(
        stream,
        &format!("DECLARE c CURSOR FOR SELECT id FROM documents LIMIT {rows}"),
    );
    assert_eq!(read_command_complete(stream), "DECLARE CURSOR");
    read_ready_for_query(stream);

    parse_and_bind(stream, "sf", "pf", &format!("FETCH {rows} FROM c"));
    send_length_prefixed_message(stream, b'E', &execute_body("pf", 0));
    for _ in 0..rows {
        let (kind, _) = read_message(stream);
        assert_eq!(kind, b'D', "expected DataRow");
    }
    let (kind, tag) = read_message(stream);
    assert_eq!(kind, b'C', "expected CommandComplete after all rows");
    tag
}

/// 完了済み `pf` を再 Execute すると、エラーも `DataRow` も返さず初回と同じ
/// 完了タグの `CommandComplete` のみが返り、`Sync` で `ReadyForQuery` に戻る。
fn assert_done_replay_resends_tag_only(stream: &mut std::net::TcpStream, expected_tag: &[u8]) {
    send_length_prefixed_message(stream, b'E', &execute_body("pf", 0));
    let (kind, body) = read_message(stream);
    assert_eq!(
        kind,
        b'C',
        "a completed FETCH portal must replay CommandComplete, got {:?}",
        String::from_utf8_lossy(&body)
    );
    assert_eq!(
        body, expected_tag,
        "replayed completion tag must be unchanged"
    );
    send_sync(stream);
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery after Sync");
}

#[test]
fn done_fetch_portal_replays_completion_after_cursor_close() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 3);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let tag = declare_and_complete_fetch_portal(&mut stream, 3);

    send_simple_query(&mut stream, "CLOSE c");
    assert_eq!(read_command_complete(&mut stream), "CLOSE CURSOR");
    read_ready_for_query(&mut stream);

    assert_done_replay_resends_tag_only(&mut stream, &tag);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
}

#[test]
fn done_fetch_portal_replays_completion_after_commit() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 3);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let tag = declare_and_complete_fetch_portal(&mut stream, 3);

    send_simple_query(&mut stream, "COMMIT");
    assert_eq!(read_command_complete(&mut stream), "COMMIT");
    read_ready_for_query(&mut stream);

    assert_done_replay_resends_tag_only(&mut stream, &tag);
}

#[test]
fn done_fetch_portal_replays_completion_after_rollback() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 3);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let tag = declare_and_complete_fetch_portal(&mut stream, 3);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);

    assert_done_replay_resends_tag_only(&mut stream, &tag);
}

// --- FETCH の応答生成失敗後に行を飛ばさない（PR #1049 レビュー指摘 codex P1） ---

/// `ErrorResponse`（'E'）を読み、SQLSTATE（`C` フィールド）を返す。
fn read_error_sqlstate(stream: &mut std::net::TcpStream) -> String {
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'E', "expected ErrorResponse, got {:?}", kind as char);
    let mut fields = body.split(|b| *b == 0);
    for field in fields.by_ref() {
        if let Some((b'C', code)) = field.split_first() {
            return String::from_utf8_lossy(code).into_owned();
        }
    }
    panic!("ErrorResponse without SQLSTATE: {body:?}");
}

/// 拡張クエリの `FETCH`（`max_rows=1`）が中断保持バイト上限（セッション全体
/// 16 MiB。別 portal の中断保持分と合算）を超えて `54000` で失敗した場合、
/// エンジン側で取得位置が進んでいても、明示トランザクションは `Failed` へ遷移し
/// カーソルごと破棄される。同じトランザクションの次の `FETCH` は行を返さず
/// `25P02` になり（行を飛ばした位置から再開しない）、`ROLLBACK` 後に同じ
/// カーソルを宣言し直せば先頭行から取得できる。
#[test]
fn fetch_response_overflow_fails_transaction_instead_of_skipping_rows() {
    const ROW_BODY_LEN: usize = 1_000_000;
    const ROW_COUNT: u64 = 11;

    let (core, _guard) = new_core_with_documents_table();
    let big_body = "x".repeat(ROW_BODY_LEN);
    for id in 1..=ROW_COUNT {
        let sql = format!(
            "INSERT INTO documents (id, embedding, body) VALUES ({id}, '[0.1,0.2,0.3]', '{big_body}') USING OPERATION_ID 'seed-big-{id}'"
        );
        let mut session = engine::sql::mode::SessionState::default();
        core.execute_sql_in_session(&owner_ctx(), &mut session, &sql)
            .expect("seed big-body insert succeeds");
    }
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    read_ready_for_query(&mut stream);
    send_simple_query(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id, body FROM documents LIMIT 11",
    );
    assert_eq!(read_command_complete(&mut stream), "DECLARE CURSOR");
    read_ready_for_query(&mut stream);

    // 別 portal（通常の広域取得）で約 10 MiB を中断保持させる。
    parse_and_bind(
        &mut stream,
        "sfill",
        "pfill",
        "SELECT id, body FROM documents LIMIT 11",
    );
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pfill", 1));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'D', "expected DataRow for the filler portal");
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b's', "expected PortalSuspended for the filler portal");

    // `FETCH` の残り 10 行（約 10 MiB）を中断保持しようとすると合計が上限を
    // 超え、1 行も送らずに `54000` で失敗する。
    parse_and_bind(&mut stream, "sf", "pf", "FETCH 11 FROM c");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pf", 1));
    assert_eq!(read_error_sqlstate(&mut stream), "54000");
    send_sync(&mut stream);
    let (kind, status) = read_message(&mut stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery after Sync");
    assert_eq!(
        status.first(),
        Some(&b'E'),
        "the explicit transaction must be failed after the FETCH response failure"
    );

    // 次の `FETCH` は（飛ばした位置からの）行を返さず 25P02。
    send_simple_query(&mut stream, "FETCH 1 FROM c");
    assert_eq!(read_error_sqlstate(&mut stream), "25P02");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);

    // 宣言し直せば先頭行（id=1）から取得できる。
    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    read_ready_for_query(&mut stream);
    send_simple_query(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id FROM documents LIMIT 11",
    );
    assert_eq!(read_command_complete(&mut stream), "DECLARE CURSOR");
    read_ready_for_query(&mut stream);
    send_simple_query(&mut stream, "FETCH 1 FROM c");
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'T', "expected RowDescription");
    let (kind, row) = read_message(&mut stream);
    assert_eq!(kind, b'D', "expected DataRow");
    // DataRow: 列数 i16 → 1 列目の長さ i32 → 値（テキスト形式）。
    let len = i32::from_be_bytes(row[2..6].try_into().expect("length")) as usize;
    assert_eq!(
        &row[6..6 + len],
        b"1",
        "the first FETCH must return the first row"
    );
    assert_eq!(read_command_complete(&mut stream), "FETCH 1");
    read_ready_for_query(&mut stream);
    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
}

// --- Sync を越えて保持された FETCH portal（WIRE-11・Issue #1174） ---

/// 明示トランザクション中は、中断保持中の `FETCH` portal が Sync を越えて
/// 保持され、世代・カーソル識別子が一致する限り残りの行で再開できる。
#[test]
fn suspended_fetch_portal_resumes_across_sync_inside_transaction() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 3);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    declare_and_suspend_fetch_portal(&mut stream, 3);
    send_sync(&mut stream);
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'Z');
    assert_eq!(body.last().copied(), Some(b'T'));

    send_length_prefixed_message(&mut stream, b'E', &execute_body("pf", 0));
    for _ in 0..2 {
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b'D', "expected remaining DataRow after Sync");
    }
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'C', "expected CommandComplete");
    send_sync(&mut stream);
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'Z');
}

/// Sync を越えた `FETCH` portal でも、Sync 後に `CLOSE c` を挟めば従来どおり
/// `34000` で拒否され、行は送出されない（世代・識別子の照合を維持）。
#[test]
fn fetch_portal_kept_across_sync_is_rejected_after_cursor_close() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    declare_and_suspend_fetch_portal(&mut stream, 5);
    send_sync(&mut stream);
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'Z');

    send_simple_query(&mut stream, "CLOSE c");
    assert_eq!(read_command_complete(&mut stream), "CLOSE CURSOR");
    read_ready_for_query(&mut stream);

    assert_resume_rejected_without_leaking_rows(&mut stream);
}

// --- writer 取得前後のカーソルのスナップショット（Issue #2411。WIRE-15・SQL-19・SQL-31） ---

/// WIRE-15（2026-10-16 改訂注記）・SQL-19・SQL-31・Issue #2411: writer 取得後に宣言したカーソルを
/// 拡張クエリの portal で `max_rows` 分割して払い出しても、行集合は取得時点の最新の確定状態
/// （他接続の commit）に自分の未 commit 変更を加えたものになる。
#[test]
fn extended_declare_after_writer_fetch_portal_reads_acquire_time_snapshot() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 3);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    read_ready_for_query(&mut stream);

    // BEGIN の後に他接続（同一テナント）が commit する。
    let mut other = engine::sql::mode::SessionState::default();
    core.execute_sql_in_session(
        &owner_ctx(),
        &mut other,
        "INSERT INTO documents (id, embedding, body) VALUES (99, '[0.1,0.2,0.3]', 'other-99') \
         USING OPERATION_ID 'w4-other'",
    )
    .expect("other session commit");

    // 自分の書き込みで writer を取得する（documents は未読のため 40001 にならない）。
    send_simple_query(
        &mut stream,
        "INSERT INTO documents (id, embedding, body) VALUES (50, '[0.1,0.2,0.3]', 'own-50') \
         USING OPERATION_ID 'w4-own'",
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id, body FROM documents LIMIT 100",
    );
    assert_eq!(read_command_complete(&mut stream), "DECLARE CURSOR");
    read_ready_for_query(&mut stream);

    parse_and_bind(&mut stream, "sf", "pf", "FETCH 100 FROM c");
    let mut bodies: Vec<String> = Vec::new();
    loop {
        send_length_prefixed_message(&mut stream, b'E', &execute_body("pf", 2));
        // 1 回の Execute で DataRow が最大 2 件、続けて PortalSuspended か CommandComplete が届く。
        let mut finished = false;
        loop {
            let (kind, body) = read_message(&mut stream);
            match kind {
                b'D' => bodies.push(String::from_utf8_lossy(&body).into_owned()),
                b's' => break,
                b'C' => {
                    finished = true;
                    break;
                }
                other => panic!("unexpected message {other}"),
            }
        }
        if finished {
            break;
        }
    }
    assert_eq!(bodies.len(), 5, "3 seeded + other session's 99 + own 50");
    assert!(bodies.iter().any(|b| b.contains("other-99")));
    assert!(bodies.iter().any(|b| b.contains("own-50")));
    send_sync(&mut stream);
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery after Sync");

    send_simple_query(&mut stream, "COMMIT");
    assert_eq!(read_command_complete(&mut stream), "COMMIT");
    read_ready_for_query(&mut stream);
}
/// 拡張クエリの `MOVE`（Issue #3336・WIRE-27）: Describe(Portal) は NoData、Execute は行なしの
/// `CommandComplete`（`MOVE n`）で、その後の `FETCH` が移動先の次の行から始まる。
#[test]
fn move_over_extended_query_describes_no_data_and_moves_position() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    read_ready_for_query(&mut stream);
    send_simple_query(
        &mut stream,
        "DECLARE c SCROLL CURSOR FOR SELECT id FROM documents LIMIT 100",
    );
    assert_eq!(read_command_complete(&mut stream), "DECLARE CURSOR");
    read_ready_for_query(&mut stream);

    parse_and_bind(&mut stream, "sm", "pm", "MOVE FORWARD 2 IN c");
    send_length_prefixed_message(&mut stream, b'D', b"Ppm\0");
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'n', "MOVE must describe as NoData");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pm", 0));
    let (kind, tag) = read_message(&mut stream);
    assert_eq!(kind, b'C', "MOVE must complete without rows");
    assert_eq!(tag, b"MOVE 2\0");

    parse_and_bind(&mut stream, "sf", "pf", "FETCH NEXT FROM c");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pf", 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'D', "expected DataRow for the row after the move");
    let (kind, tag) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    assert_eq!(tag, b"FETCH 1\0");
    send_sync(&mut stream);
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'Z');

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
}
// --- SCROLL／MOVE の拡張クエリ（Issue #3337・WIRE-27・WIRE-15） ------------------------

/// `DataRow` 本体（列数 i16 → 各列 長さ i32 ＋値）の第 1 列の文字列。
fn data_row_first_column(body: &[u8]) -> String {
    let len_bytes: [u8; 4] = body
        .get(2..6)
        .and_then(|s| s.try_into().ok())
        .expect("DataRow has a first column length");
    let len = usize::try_from(i32::from_be_bytes(len_bytes)).expect("non-null column");
    let value = body.get(6..6 + len).expect("DataRow first column bytes");
    String::from_utf8(value.to_vec()).expect("utf8 id")
}

/// `BEGIN` → `DECLARE c {head} CURSOR`（5 行）まで進める。
fn begin_and_declare_c(stream: &mut std::net::TcpStream, head: &str) {
    simple_ok(stream, "BEGIN", "BEGIN");
    simple_ok(
        stream,
        &format!("DECLARE c {head} CURSOR FOR SELECT id FROM documents LIMIT 100"),
        "DECLARE CURSOR",
    );
}

/// Parse（末尾 `;` 付き可）→ Bind → Describe(Portal) → Execute(max_rows=0) を流し、
/// `(RowDescription の有無, DataRow の id 列, CommandComplete のタグ)` を返す（Sync は呼び側）。
fn run_extended(
    stream: &mut std::net::TcpStream,
    sql: &str,
    max_rows: i32,
) -> (bool, Vec<String>, String) {
    static NEXT_ID: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let id = NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (stmt, portal) = (format!("sx{id}"), format!("px{id}"));
    parse_and_bind(stream, &stmt, &portal, sql);
    send_length_prefixed_message(stream, b'D', format!("P{portal}\0").as_bytes());
    let (kind, _) = read_message(stream);
    let described_rows = match kind {
        b'T' => true,
        b'n' => false,
        other => panic!("unexpected Describe response {other}"),
    };
    send_length_prefixed_message(stream, b'E', &execute_body(&portal, max_rows));
    let mut ids = Vec::new();
    loop {
        let (kind, body) = read_message(stream);
        match kind {
            b'D' => ids.push(data_row_first_column(&body)),
            b'C' => {
                let tag = String::from_utf8_lossy(&body)
                    .trim_end_matches('\0')
                    .to_string();
                return (described_rows, ids, tag);
            }
            other => panic!("{sql}: unexpected message {other}"),
        }
    }
}

fn sync_and_expect_status(stream: &mut std::net::TcpStream, status: u8) {
    send_sync(stream);
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery after Sync");
    assert_eq!(body.last().copied(), Some(status));
}

/// SCROLL カーソルの各向きが拡張クエリ（Parse の末尾 `;` 付き）でも PG と同じ行・タグで返る。
#[test]
fn scroll_directions_over_extended_query_match_simple_query() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    begin_and_declare_c(&mut stream, "SCROLL");

    let script: &[(&str, &[&str], &str)] = &[
        ("FETCH ABSOLUTE -1 FROM c;", &["5"], "FETCH 1"),
        ("FETCH PRIOR FROM c", &["4"], "FETCH 1"),
        ("FETCH RELATIVE -2 FROM c;", &["2"], "FETCH 1"),
        ("FETCH BACKWARD 2 FROM c", &["1"], "FETCH 1"),
        ("FETCH FIRST FROM c;", &["1"], "FETCH 1"),
        ("FETCH FORWARD 2 FROM c", &["2", "3"], "FETCH 2"),
    ];
    for (sql, ids, tag) in script {
        let (described, got, got_tag) = run_extended(&mut stream, sql, 0);
        assert!(described, "{sql}: FETCH describes its row shape");
        let want: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
        assert_eq!((got, got_tag.as_str()), (want, *tag), "{sql}");
    }
    sync_and_expect_status(&mut stream, b'T');
    simple_ok(&mut stream, "ROLLBACK", "ROLLBACK");
}

/// `FETCH ALL` で末尾の後へ進めてから `FETCH BACKWARD ALL` を `max_rows=2` で Execute すると、
/// 中断 portal の再 Execute を挟んでも降順の行が漏れなく重複なく届く。
#[test]
fn backward_all_portal_suspends_and_resumes_in_descending_order() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    begin_and_declare_c(&mut stream, "SCROLL");
    simple_ok(&mut stream, "MOVE FORWARD ALL IN c", "MOVE 5");

    parse_and_bind(&mut stream, "sb", "pb", "FETCH BACKWARD ALL FROM c");
    let mut ids: Vec<String> = Vec::new();
    let mut suspensions = 0;
    loop {
        send_length_prefixed_message(&mut stream, b'E', &execute_body("pb", 2));
        let mut finished = false;
        loop {
            let (kind, body) = read_message(&mut stream);
            match kind {
                b'D' => ids.push(data_row_first_column(&body)),
                b's' => {
                    suspensions += 1;
                    break;
                }
                b'C' => {
                    finished = true;
                    break;
                }
                other => panic!("unexpected message {other}"),
            }
        }
        if finished {
            break;
        }
    }
    assert_eq!(ids, ["5", "4", "3", "2", "1"]);
    assert_eq!(suspensions, 2, "5 rows at max_rows=2 suspend twice");
    sync_and_expect_status(&mut stream, b'T');
    simple_ok(&mut stream, "ROLLBACK", "ROLLBACK");
}

/// NO SCROLL の逆方向は ErrorResponse `55000`（Sync 後の ReadyForQuery は `E`）。
/// 行の上の `MOVE 0` は Describe が NoData で、実行は `MOVE 1`。
#[test]
fn no_scroll_backward_is_55000_and_move_zero_is_accepted_over_extended_query() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    begin_and_declare_c(&mut stream, "NO SCROLL");

    let (_, ids, tag) = run_extended(&mut stream, "FETCH 2 FROM c", 0);
    assert_eq!(
        (ids, tag.as_str()),
        (vec!["1".into(), "2".into()], "FETCH 2")
    );
    let (described, ids, tag) = run_extended(&mut stream, "MOVE 0 IN c;", 0);
    assert!(!described, "MOVE describes as NoData");
    assert_eq!((ids.len(), tag.as_str()), (0, "MOVE 1"));
    let (_, ids, tag) = run_extended(&mut stream, "FETCH NEXT FROM c", 0);
    assert_eq!((ids, tag.as_str()), (vec!["3".to_string()], "FETCH 1"));

    parse_and_bind(&mut stream, "sp", "pp", "FETCH PRIOR FROM c");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pp", 0));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'E', "NO SCROLL backward must be rejected");
    assert!(String::from_utf8_lossy(&body).contains("55000"));
    send_sync(&mut stream);
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'Z');
    assert_eq!(body.last().copied(), Some(b'E'));
    simple_ok(&mut stream, "ROLLBACK", "ROLLBACK");
}

/// savepoint より前に宣言した SCROLL カーソルの逆方向の中断 portal は `ROLLBACK TO` の後も
/// 続きの行を出せる（older cursor の逆方向版）。後に宣言したカーソルの中断 portal は `34000`。
#[test]
fn backward_suspended_portal_follows_rollback_to_contract() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    begin_and_declare_c(&mut stream, "SCROLL");
    simple_ok(&mut stream, "MOVE FORWARD ALL IN c", "MOVE 5");
    simple_ok(&mut stream, "SAVEPOINT a", "SAVEPOINT");
    simple_ok(
        &mut stream,
        "DECLARE d SCROLL CURSOR FOR SELECT id FROM documents LIMIT 100",
        "DECLARE CURSOR",
    );
    simple_ok(&mut stream, "MOVE FORWARD ALL IN d", "MOVE 5");

    // c（older）と d（newer）の逆方向 portal をそれぞれ 1 行で中断させる。
    for (stmt, portal, cursor) in [("sc", "pc", "c"), ("sd", "pd", "d")] {
        parse_and_bind(
            &mut stream,
            stmt,
            portal,
            &format!("FETCH BACKWARD ALL FROM {cursor}"),
        );
        send_length_prefixed_message(&mut stream, b'E', &execute_body(portal, 1));
        let (kind, body) = read_message(&mut stream);
        assert_eq!(kind, b'D');
        assert_eq!(data_row_first_column(&body), "5");
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b's', "expected PortalSuspended");
    }
    simple_ok(&mut stream, "ROLLBACK TO a", "ROLLBACK");

    // older: 続きの 4 行が降順で出る。
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pc", 0));
    let mut ids = Vec::new();
    loop {
        let (kind, body) = read_message(&mut stream);
        match kind {
            b'D' => ids.push(data_row_first_column(&body)),
            b'C' => break,
            other => panic!("unexpected message {other}"),
        }
    }
    assert_eq!(ids, ["4", "3", "2", "1"]);
    // newer: 閉じたカーソルへの束縛は 34000 で、行を送出しない。
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pd", 0));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'E', "closed cursor portal must not resume");
    assert!(String::from_utf8_lossy(&body).contains("34000"));
    send_sync(&mut stream);
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'Z');
    simple_ok(&mut stream, "ROLLBACK", "ROLLBACK");
}
