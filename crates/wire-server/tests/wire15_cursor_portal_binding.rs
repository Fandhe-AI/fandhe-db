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

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

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
