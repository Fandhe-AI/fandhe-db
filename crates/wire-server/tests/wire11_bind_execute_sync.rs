//! 拡張クエリプロトコルの Bind（'B'）・Execute（'E'）・Sync（'S'）・
//! Close（'C'）・Flush（'H'）の結合テスト（Issue #934・TASK-71・WIRE-11。
//! `crate::extended_query` が受理する経路）。
//!
//! Parse／Describe 単体の契約は `tests/wire11_parse_describe.rs` が担う。本
//! ファイルは Bind 以降——portal のライフサイクル・`max_rows` による分割送出・
//! エラー後の同期回復・テナント境界——を対象にする。

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

use common::*;

fn new_core_with_documents_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire11-bind-execute-sync");
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

fn owner_ctx() -> engine::policy::PolicyContext {
    engine::policy::PolicyContext::new("tenant-a").expect("valid tenant id")
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

fn bind_body_with_binary_result_format(portal: &str, statement: &str) -> Vec<u8> {
    bind_body_with_result_formats(portal, statement, &[1])
}

/// [`bind_body`] の結果 format code 部分だけを差し替える（WIRE-14）。
/// `codes` はそのまま「結果 format code 件数＋列」として送出する
/// （0/1/対象数のいずれでもない件数を意図的に送るテストにも使う）。
fn bind_body_with_result_formats(portal: &str, statement: &str, codes: &[i16]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(statement.as_bytes());
    body.push(0);
    body.extend_from_slice(&0i16.to_be_bytes()); // param format code count
    body.extend_from_slice(&0i16.to_be_bytes()); // param count
    body.extend_from_slice(&(codes.len() as i16).to_be_bytes());
    for code in codes {
        body.extend_from_slice(&code.to_be_bytes());
    }
    body
}

fn execute_body(portal: &str, max_rows: i32) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(portal.as_bytes());
    body.push(0);
    body.extend_from_slice(&max_rows.to_be_bytes());
    body
}

fn describe_body(kind: u8, name: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(kind);
    body.extend_from_slice(name.as_bytes());
    body.push(0);
    body
}

fn close_body(kind: u8, name: &str) -> Vec<u8> {
    let mut body = Vec::new();
    body.push(kind);
    body.extend_from_slice(name.as_bytes());
    body.push(0);
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

fn assert_error_response(stream: &mut std::net::TcpStream, expected_sqlstate: &str) {
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'E', "expected ErrorResponse");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains(expected_sqlstate),
        "expected sqlstate {expected_sqlstate} in {text:?}"
    );
}

fn send_sync(stream: &mut std::net::TcpStream) {
    send_length_prefixed_message(stream, b'S', b"");
}

fn assert_ready_for_query(stream: &mut std::net::TcpStream) {
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery");
    assert_eq!(body, [b'I']);
}

fn assert_error_then_recovers(stream: &mut std::net::TcpStream, expected_sqlstate: &str) {
    assert_error_response(stream, expected_sqlstate);
    send_sync(stream);
    assert_ready_for_query(stream);
}

fn parse_and_bind(stream: &mut std::net::TcpStream, statement: &str, portal: &str, sql: &str) {
    send_length_prefixed_message(stream, b'P', &parse_body(statement, sql, 0));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'1', "expected ParseComplete");
    send_length_prefixed_message(stream, b'B', &bind_body(portal, statement));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'2', "expected BindComplete");
}

/// 受け入れ条件 1: Parse/Bind/Describe(P)/Execute(0)/Sync を送ると
/// '1' '2' 'T' 'D'* 'C' 'Z' の順に返り、同じ SQL の簡易クエリ応答とバイト単位で
/// 一致する。
#[test]
fn select_happy_path_matches_simple_query_bytes() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 3);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut extended = tcp_connect(addr);
    let mut simple = tcp_connect(addr);

    let sql = "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5";

    parse_and_bind(&mut extended, "s1", "p1", sql);
    send_length_prefixed_message(&mut extended, b'D', &describe_body(b'P', "p1"));
    let (kind, _) = read_message(&mut extended);
    assert_eq!(
        kind, b'T',
        "expected RowDescription (no ParameterDescription for portal)"
    );
    send_length_prefixed_message(&mut extended, b'E', &execute_body("p1", 0));
    let mut extended_bytes = Vec::new();
    loop {
        let (kind, body) = read_message(&mut extended);
        extended_bytes.push(kind);
        extended_bytes.extend_from_slice(&(body.len() as i32 + 4).to_be_bytes());
        extended_bytes.extend_from_slice(&body);
        if kind == b'C' {
            break;
        }
    }
    send_sync(&mut extended);
    assert_ready_for_query(&mut extended);

    send_simple_query(&mut simple, sql);
    let (kind, _) = read_message(&mut simple); // RowDescription（拡張経路では Describe が既に消費している）
    assert_eq!(kind, b'T');
    let mut simple_bytes = Vec::new();
    loop {
        let (kind, body) = read_message(&mut simple);
        if kind == b'Z' {
            break;
        }
        simple_bytes.push(kind);
        simple_bytes.extend_from_slice(&(body.len() as i32 + 4).to_be_bytes());
        simple_bytes.extend_from_slice(&body);
    }

    assert_eq!(
        extended_bytes, simple_bytes,
        "DataRow/CommandComplete bytes must match the simple query response"
    );
}

fn tcp_connect(addr: std::net::SocketAddr) -> std::net::TcpStream {
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    send_ssl_request_and_startup(&mut stream, "alice", "irrelevant-db-name");
    let auth_code = read_auth_request_type(&mut stream);
    assert_eq!(auth_code, 3);
    send_password_message(&mut stream, "correct-horse");
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("auth ok type");
    assert_eq!(header[0], b'R');
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("len");
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("code");
    assert_eq!(i32::from_be_bytes(code_buf), 0);
    let mut msg_type = read_message_type_discarding_body(&mut stream);
    let mut safety = 0;
    while msg_type != b'Z' {
        safety += 1;
        assert!(safety < 20, "too many messages before ReadyForQuery");
        msg_type = read_message_type_discarding_body(&mut stream);
    }
    stream
}

/// 受け入れ条件 2: `max_rows` による分割送出。5 行に対し `max_rows = 2` を
/// 3 回 Execute すると「2 行 + PortalSuspended」×2・「1 行 + CommandComplete」と
/// なり、再実行しない（重複・欠落なし）。`max_rows <= 0` は全行を返す。
#[test]
fn max_rows_splits_result_without_reexecuting() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 5);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    let sql = "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5";
    parse_and_bind(&mut stream, "s1", "p1", sql);

    let mut all_ids = Vec::new();

    // 1 回目: 2 行 + PortalSuspended
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 2));
    for _ in 0..2 {
        let (kind, body) = read_message(&mut stream);
        assert_eq!(kind, b'D');
        all_ids.push(body);
    }
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b's', "expected PortalSuspended");

    // 2 回目: 残り 2 行 + PortalSuspended
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 2));
    for _ in 0..2 {
        let (kind, body) = read_message(&mut stream);
        assert_eq!(kind, b'D');
        all_ids.push(body);
    }
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b's');

    // 3 回目: 残り 1 行 + CommandComplete（再実行なし）。タグは今回バッチの
    // 件数（1）ではなく portal 全体の累計送出行数（2+2+1=5）から組み立てる
    // （PostgreSQL の契約。PR #1013 レビュー指摘・P1）。
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 2));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'D');
    all_ids.push(body);
    let (kind, tag) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "SELECT 5");

    assert_eq!(all_ids.len(), 5, "no duplicate or missing rows");
    let mut dedup = all_ids.clone();
    dedup.sort();
    dedup.dedup();
    assert_eq!(dedup.len(), 5, "no duplicate rows across Execute calls");

    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // 負値・0 は全行を一度に返す。
    parse_and_bind(&mut stream, "s2", "p2", sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p2", 0));
    let mut count = 0;
    loop {
        let (kind, _) = read_message(&mut stream);
        if kind == b'C' {
            break;
        }
        assert_eq!(kind, b'D');
        count += 1;
    }
    assert_eq!(count, 5);
}

/// PR #1013 レビュー指摘（P0）: 中断保持する未送出行の合計バイト数上限
/// （`MAX_SUSPENDED_PORTAL_BYTES_PER_SESSION`＝16 MiB）は portal 単体ではなく
/// 接続（セッション）全体で合算して適用される契約を固定する。1 個の
/// 名前付き portal が上限未満（約 10 MiB）の分だけ中断保持している状態で、
/// 別の名前付き portal が同じ接続上でさらに約 10 MiB を中断保持しようと
/// すると、合計が上限を超えるため 2 個目の Execute は `54000` で拒否される
/// （portal 単体判定のままなら両方とも上限未満として通ってしまう）。
///
/// 1 文の SQL テキスト長には別途上限（`sql::lexer::MAX_INPUT_LEN`＝1 MiB）が
/// あるため、1 行あたり約 1 MiB の `body` を持つ行を複数（11 行）挿入し、
/// 1 行だけ送出（`max_rows=1`）して残り 10 行（約 10 MiB）を中断保持させる
/// ことで境界を作る。
#[test]
fn suspended_portal_bytes_are_accounted_across_the_whole_session_not_per_portal() {
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
    let mut stream = tcp_connect(addr);

    let sql = "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 11";

    // 1 個目の portal: 11 行中 1 行だけ送出し、残り 10 行（約 10 MiB）を
    // 中断保持する。単体では上限（16 MiB）未満のため成功する。
    parse_and_bind(&mut stream, "s1", "p1", sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 1));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'D');
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b's', "expected PortalSuspended for p1");

    // 2 個目の portal（別名。同一接続）: こちらも残り 10 行（約 10 MiB）を
    // 中断保持しようとするが、p1 の中断保持分と合算すると接続全体で
    // 約 20 MiB となり上限（16 MiB）を超えるため 54000 で拒否される。
    parse_and_bind(&mut stream, "s2", "p2", sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p2", 1));
    assert_error_then_recovers(&mut stream, "54000");

    // `assert_error_then_recovers` が送った Sync は、codex P1 是正
    // （PR #1013）により p1・p2 を問わず全 portal を破棄する（本サーバーに
    // 明示トランザクションが無く各 Sync サイクルが暗黙トランザクションに
    // 相当するため）。したがって p1 も Sync 後は portal 不在（08P01）になる
    // ——「p2 の拒否が p1 の中断状態を破壊しない」という従来の P1 保証は、
    // 「Sync 自体が意図的に両方を破棄する」という、より安全な契約に置き換
    // わった。
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 0));
    assert_error_then_recovers(&mut stream, "08P01");

    // 新しい Sync サイクルで p1 を再 Bind すれば、11 行すべてを通常どおり
    // 取得できる（statement s1 は Sync を越えて残っている）。
    send_length_prefixed_message(&mut stream, b'B', &bind_body("p1", "s1"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'2', "expected BindComplete");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 0));
    for _ in 0..11 {
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b'D');
    }
    let (kind, tag) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "SELECT 11");
}

/// 受け入れ条件 3: `INSERT ... USING OPERATION_ID` を拡張経路で実行すると
/// `INSERT 0 1` が返り、後続の SELECT から見える。同じ operation_id の再送は
/// `23505`、内容を変えた再送は `22023`（いずれも同期回復する）。
#[test]
fn insert_via_extended_protocol_is_visible_and_operation_id_conflicts_are_detected() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    let insert_sql =
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-1'";
    parse_and_bind(&mut stream, "ins1", "pi1", insert_sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pi1", 0));
    let (kind, tag) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "INSERT 0 1");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // 見える: 簡易クエリで読み戻せる。
    send_simple_query(&mut stream, "SELECT id FROM documents WHERE id = 1 LIMIT 1");
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id".to_string()]);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![Some("1".to_string())]);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    // 同一 operation_id の再送（同一内容）は 23505。
    parse_and_bind(&mut stream, "ins2", "pi2", insert_sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pi2", 0));
    assert_error_then_recovers(&mut stream, "23505");

    // 同一 operation_id・異なる内容は 22023。
    let conflicting_sql =
        "INSERT INTO documents (id, embedding, body) VALUES (2, '[0.4,0.5,0.6]', 'other') USING OPERATION_ID 'op-1'";
    parse_and_bind(&mut stream, "ins3", "pi3", conflicting_sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pi3", 0));
    assert_error_then_recovers(&mut stream, "22023");
}

/// 受け入れ条件 4: Sync による回復（許可リスト外 SQL・未存在テーブル・未定義
/// statement/portal への Bind/Describe/Execute・パラメータ数不一致・実行時
/// エラー）はいずれも ErrorResponse の後 Sync で 'Z' が返り、同じ接続で簡易
/// クエリ・拡張シーケンスの双方が続けて成功する。
#[test]
fn sync_recovers_from_a_variety_of_errors_and_connection_keeps_working() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 1);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    // 許可リスト外 SQL の Parse（`LIMIT $n` は受理位置外。WIRE-12）。
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("", "SELECT id FROM documents LIMIT $1", 0),
    );
    assert_error_then_recovers(&mut stream, "42601");

    // 未存在テーブル。
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("", "SELECT id FROM missing_table LIMIT 1", 0),
    );
    assert_error_then_recovers(&mut stream, "42P01");

    // 未定義 statement への Bind・Describe。
    send_length_prefixed_message(&mut stream, b'B', &bind_body("p", "never-parsed"));
    assert_error_then_recovers(&mut stream, "08P01");
    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'S', "never-parsed"));
    assert_error_then_recovers(&mut stream, "08P01");

    // 未定義 portal への Execute・Describe。
    send_length_prefixed_message(&mut stream, b'E', &execute_body("no-such-portal", 0));
    assert_error_then_recovers(&mut stream, "08P01");
    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'P', "no-such-portal"));
    assert_error_then_recovers(&mut stream, "08P01");

    // 実行時エラー: 未存在テーブルへの操作は Parse 時点で拒否済みのため、
    // 代わりに実行時に SqlSurfaceError を発生させる operation_id 重複を使う
    // （既存のテーブルに対する 2 回目の同一 INSERT）。
    let dup_sql =
        "INSERT INTO documents (id, embedding, body) VALUES (99, '[0.1,0.2,0.3]', 'x') USING OPERATION_ID 'dup-op'";
    parse_and_bind(&mut stream, "dup1", "pdup1", dup_sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pdup1", 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);
    parse_and_bind(&mut stream, "dup2", "pdup2", dup_sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pdup2", 0));
    assert_error_then_recovers(&mut stream, "23505");

    // 回復後、同じ接続で簡易クエリが通る。
    send_simple_query(&mut stream, "SELECT id FROM documents LIMIT 1");
    let _cols = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    // 回復後、同じ接続で拡張シーケンスが再び成功する。
    parse_and_bind(
        &mut stream,
        "ok1",
        "pok1",
        "SELECT id FROM documents LIMIT 1",
    );
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pok1", 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'D');
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);
}

/// 受け入れ条件 5: エラー後、Sync の前に送った簡易クエリには応答しない
/// （破棄される）。Sync に対して 'Z' が 1 つだけ返る。
#[test]
fn queries_sent_before_sync_after_an_error_are_discarded() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("", "SELECT id FROM documents LIMIT $1", 0),
    );
    assert_error_response(&mut stream, "42601");

    // エラー後・Sync 前に送った簡易クエリは破棄され応答しない。
    send_simple_query(&mut stream, "SELECT 1");

    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // これ以上メッセージは来ない（破棄された 'Q' に対する応答が紛れ込んで
    // いないことを、次の簡易クエリの応答が正しく単発で返ることで確認する）。
    send_simple_query(&mut stream, "SELECT id FROM documents LIMIT 1");
    let _cols = read_row_description(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 受け入れ条件 6: portal のライフサイクル。無名 portal は Sync で破棄され、
/// 本サーバーには明示トランザクション（`BEGIN`/`COMMIT`）が無く各 Sync
/// サイクルが暗黙トランザクションに相当するため、名前付き・無名を問わず
/// 全 portal が Sync で破棄される（codex P1 指摘・PR #1013。PostgreSQL の
/// トランザクション終了時 portal 破棄契約を Sync 境界へ適用）。名前付き
/// prepared statement は Sync を越えて残る（PostgreSQL と同じ挙動）。
/// Close は '3' を返し、閉じた後の Execute/Bind はエラーになる。statement
/// の Close は派生 portal も閉じる。未存在名の Close も '3' を返す。
#[test]
fn portal_lifecycle_anonymous_named_and_close() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 1);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    let sql = "SELECT id FROM documents LIMIT 1";

    // 無名 portal は Sync で破棄される。
    parse_and_bind(&mut stream, "", "", sql);
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("", 0));
    assert_error_then_recovers(&mut stream, "08P01");

    // 名前付き statement は Sync を越えて残るが、名前付き portal は Sync で
    // 破棄される（暗黙トランザクション境界。本テストの主眼）。
    parse_and_bind(&mut stream, "sN", "pN", sql);
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pN", 0));
    assert_error_then_recovers(&mut stream, "08P01");

    // 同一 Sync サイクル内（Bind してから Sync を挟まず Execute）では、
    // 名前付き portal も従来どおり実行できる。
    send_length_prefixed_message(&mut stream, b'B', &bind_body("pN", "sN"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'2', "expected BindComplete");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pN", 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'D');
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'C');

    // Close(Portal) は '3' を返し、以後の Execute はエラー。
    send_length_prefixed_message(&mut stream, b'C', &close_body(b'P', "pN"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'3', "expected CloseComplete");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pN", 0));
    assert_error_then_recovers(&mut stream, "08P01");

    // statement の Close は派生 portal も閉じる。
    parse_and_bind(&mut stream, "sX", "pX", sql);
    send_length_prefixed_message(&mut stream, b'C', &close_body(b'S', "sX"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'3');
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pX", 0));
    assert_error_then_recovers(&mut stream, "08P01");
    send_length_prefixed_message(&mut stream, b'B', &bind_body("pX2", "sX"));
    assert_error_then_recovers(&mut stream, "08P01");

    // 未存在名の Close も '3' を返す。
    send_length_prefixed_message(&mut stream, b'C', &close_body(b'S', "no-such-stmt"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'3');
    send_length_prefixed_message(&mut stream, b'C', &close_body(b'P', "no-such-portal"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'3');
}

/// codex P1 指摘（PR #1013）の回帰防止: Sync が名前付き portal も破棄する
/// ことを、[`portal_lifecycle_anonymous_named_and_close`] とは独立に固定
/// する。Sync 後の名前付き portal への Execute／Describe(Portal) はいずれも
/// portal 不在エラーになり、名前付き statement は Sync を越えて再 Bind
/// 可能なまま残る。
#[test]
fn sync_discards_named_portals_but_keeps_named_statements() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 1);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    let sql = "SELECT id FROM documents LIMIT 1";

    parse_and_bind(&mut stream, "sN", "pN", sql);
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // Sync を越えた名前付き portal への Execute は portal 不在（08P01）。
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pN", 0));
    assert_error_then_recovers(&mut stream, "08P01");

    // Sync を越えた名前付き portal への Describe(Portal) も同様に 08P01。
    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'P', "pN"));
    assert_error_then_recovers(&mut stream, "08P01");

    // 名前付き statement（sN）は Sync を越えて残っており、同名の再 Bind で
    // 新しい portal を作り直せば問題なく実行できる。
    send_length_prefixed_message(&mut stream, b'B', &bind_body("pN", "sN"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'2', "expected BindComplete");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pN", 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'D');
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);
}

/// PR #1013 レビュー指摘（Cursor Bugbot Medium）の回帰防止: PostgreSQL は
/// simple Query（'Q'）の処理を無名 statement／無名 portal への暗黙の
/// Parse／Bind／Execute と同一視し、その処理時に無名 statement・無名 portal を
/// 破棄する。拡張クエリプロトコルで確立した無名 portal を Sync せずに残した
/// まま simple Query を発行すると、後続の `Execute("")` は simple Query 実行
/// 前の古い portal スナップショットを誤って再開・再実行してはならず、
/// `34000`（`no such portal`。`ProtocolViolation`＝`08P01`）で拒否される
/// べきである。
#[test]
fn simple_query_discards_unnamed_statement_and_portal() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 3);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    let sql = "SELECT id FROM documents LIMIT 1";

    // 無名 statement／無名 portal を Sync せずに確立する（PostgreSQL は
    // Sync 前でも無名 portal を保持し続ける仕様のため、この時点ではまだ
    // 生きている）。
    parse_and_bind(&mut stream, "", "", sql);

    // simple Query を発行する。これが無名 statement／portal を破棄する
    // （本テストの検証対象）。
    send_simple_query(&mut stream, sql);
    let (kind, _) = read_message(&mut stream); // RowDescription
    assert_eq!(kind, b'T');
    let (kind, _) = read_message(&mut stream); // DataRow
    assert_eq!(kind, b'D');
    let (kind, _) = read_message(&mut stream); // CommandComplete
    assert_eq!(kind, b'C');
    assert_ready_for_query(&mut stream);

    // simple Query 実行前に確立した無名 portal への Execute は、もはや
    // 存在しない portal への参照として拒否される（simple Query 実行前の
    // 古いスナップショットが誤って再開・再実行されてはならない）。
    send_length_prefixed_message(&mut stream, b'E', &execute_body("", 0));
    assert_error_then_recovers(&mut stream, "08P01");

    // 無名 statement への再 Bind も同様に拒否される（統一的に破棄されている
    // ことの確認）。
    send_length_prefixed_message(&mut stream, b'B', &bind_body("", ""));
    assert_error_then_recovers(&mut stream, "08P01");

    // 接続は生きたままであり、新しい Parse/Bind/Execute は問題なく通る。
    parse_and_bind(&mut stream, "", "", sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("", 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'D');
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);
}

/// 受け入れ条件 7: Flush は '1' を受け取れ 'Z' は来ない。body が空でない
/// Sync/Flush は `08P01` で切断される。
#[test]
fn flush_does_not_send_ready_for_query_and_nonempty_sync_flush_close_connection() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("", "SELECT id FROM documents LIMIT 1", 0),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1');
    send_length_prefixed_message(&mut stream, b'H', b"");
    // Flush 自体は応答を持たないが、直前の ParseComplete のみが flush される
    // （'1' は既に読み終えている）。次にすぐ Sync を送って 'Z' だけが来ることを
    // 確認する。
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // 非空 body の Sync は 08P01 + 切断。
    let mut stream2 = tcp_connect(addr);
    send_length_prefixed_message(&mut stream2, b'S', b"x");
    assert_error_response(&mut stream2, "08P01");
    let mut buf = [0u8; 1];
    let n = stream2.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "connection must be closed");

    // 非空 body の Flush は 08P01 + 切断。
    let mut stream3 = tcp_connect(addr);
    send_length_prefixed_message(&mut stream3, b'H', b"x");
    assert_error_response(&mut stream3, "08P01");
    let n = stream3.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "connection must be closed");
}

/// 受け入れ条件 8: 上限。名前付き portal の 65 個目は `54000`。portal 名が
/// 長すぎる場合は `54000`。名前付き portal の重複は `08P01`。
///
/// `assert_error_then_recovers` はエラー確認後に Sync を送るが、Sync は
/// codex P1 是正（PR #1013）により名前付き portal も破棄するため、各チェック
/// 間で portal 保持数は 0 へ戻る。重複名チェックは Sync を挟まず同一メッセージ
/// 系列内（Bind→Bind）で検証する。
#[test]
fn portal_limits_are_enforced() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 1);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    let sql = "SELECT id FROM documents LIMIT 1";
    send_length_prefixed_message(&mut stream, b'P', &parse_body("shared", sql, 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1');

    for i in 0..64 {
        let name = format!("port{i}");
        send_length_prefixed_message(&mut stream, b'B', &bind_body(&name, "shared"));
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b'2', "portal {i} should be accepted");
    }

    send_length_prefixed_message(&mut stream, b'B', &bind_body("port64", "shared"));
    assert_error_then_recovers(&mut stream, "54000");

    // portal 名が長すぎる（Sync により直前の 64 個は既に破棄済み）。
    let long_name = "a".repeat(64);
    send_length_prefixed_message(&mut stream, b'B', &bind_body(&long_name, "shared"));
    assert_error_then_recovers(&mut stream, "54000");

    // 名前付き portal の重複は 08P01（Sync を挟まず同一 portal 名で連続 Bind）。
    send_length_prefixed_message(&mut stream, b'B', &bind_body("dup0", "shared"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'2', "first bind of dup0 should be accepted");
    send_length_prefixed_message(&mut stream, b'B', &bind_body("dup0", "shared"));
    assert_error_then_recovers(&mut stream, "08P01");
}

/// 受け入れ条件 9: portal の Describe は RowDescription か NoData を返し、
/// ParameterDescription は返さない。
#[test]
fn describe_portal_returns_row_description_or_no_data_without_parameter_description() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 1);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    parse_and_bind(&mut stream, "s1", "p1", "SELECT id FROM documents LIMIT 1");
    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'P', "p1"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(
        kind, b'T',
        "expected RowDescription only (no ParameterDescription)"
    );

    // TRUNCATE 系の書き込みは結果列を持たないため NoData。
    parse_and_bind(
        &mut stream,
        "s2",
        "p2",
        "TRUNCATE TABLE documents USING OPERATION_ID 'truncate-op'",
    );
    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'P', "p2"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'n', "expected NoData");
}

/// 受け入れ条件 10（WIRE-14 結線）: `id` 列（`numeric`）への binary 形式
/// 指定は非対応型として `0A000` で回復可能（[`result_encoder::
/// column_binary_support`] が `Id` を非対応とする）。
#[test]
fn binary_result_format_on_unsupported_column_is_rejected_with_feature_not_supported() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("s1", "SELECT id FROM documents LIMIT 1", 0),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1');
    send_length_prefixed_message(
        &mut stream,
        b'B',
        &bind_body_with_binary_result_format("p1", "s1"),
    );
    assert_error_then_recovers(&mut stream, "0A000");
}

/// 受け入れ条件 10（WIRE-14 結線）: `TEXT` 列は binary 形式に対応する
/// （[`result_encoder::column_binary_support`]）。Bind が成功し、
/// Describe(Portal) の `RowDescription` が format code 1 を公告し、
/// Execute の `DataRow` は UTF-8 生バイト（text 表現とビット同一）を返す。
#[test]
fn binary_result_format_on_text_column_succeeds_and_reflects_in_row_description_and_data_row() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 1);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("s1", "SELECT body FROM documents LIMIT 1", 0),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1', "expected ParseComplete");
    send_length_prefixed_message(
        &mut stream,
        b'B',
        &bind_body_with_result_formats("p1", "s1", &[1]),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'2', "expected BindComplete");

    send_length_prefixed_message(&mut stream, b'D', &describe_body(b'P', "p1"));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'T', "expected RowDescription");
    // RowDescription 本体: field_count(i16) + [name cstr, table_oid(i32),
    // column_attnum(i16), type_oid(i32), typlen(i16), type_modifier(i32),
    // format_code(i16)] の末尾 2 バイトが format code。
    let format_code = i16::from_be_bytes([body[body.len() - 2], body[body.len() - 1]]);
    assert_eq!(
        format_code, 1,
        "RowDescription must advertise binary format"
    );

    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 0));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'D', "expected DataRow");
    // DataRow 本体: field_count(i16) + [len(i32), bytes] の 1 列。
    let value_len = i32::from_be_bytes([body[2], body[3], body[4], body[5]]) as usize;
    let value_bytes = &body[6..6 + value_len];
    assert_eq!(
        value_bytes, b"row-1",
        "binary text representation must be the same UTF-8 bytes as the text representation"
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'C', "expected CommandComplete");

    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);
}

/// 受け入れ条件 10（WIRE-14 結線）: 結果 format code の値が 0/1 以外は
/// `08P01`（body の構文違反）で回復可能。
#[test]
fn binary_result_format_invalid_code_value_is_rejected_with_protocol_violation() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("s1", "SELECT body FROM documents LIMIT 1", 0),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1');
    send_length_prefixed_message(
        &mut stream,
        b'B',
        &bind_body_with_result_formats("p1", "s1", &[2]),
    );
    assert_error_then_recovers(&mut stream, "08P01");
}

/// 受け入れ条件 10（WIRE-14 結線）: 結果 format code の個数が
/// 0・1・列数のいずれでもない場合は `08P01` で回復可能。
#[test]
fn binary_result_format_count_mismatch_is_rejected_with_protocol_violation() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("s1", "SELECT id, body FROM documents LIMIT 1", 0),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1');
    send_length_prefixed_message(
        &mut stream,
        b'B',
        &bind_body_with_result_formats("p1", "s1", &[0, 0, 0]),
    );
    assert_error_then_recovers(&mut stream, "08P01");
}

/// 受け入れ条件 11: テナント境界。他テナントの Private 行に関わる INSERT 衝突
/// 応答バイト列が、行が存在しない場合と完全に一致する（RLS-9）。他テナントの
/// Private 行が拡張経路の SELECT に混ざらない（RLS-7）。
#[test]
fn tenant_boundary_holds_over_the_extended_protocol() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "correct-horse"),
    ]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));

    // alice が Private 行（id=1）を書く。
    let mut alice = std::net::TcpStream::connect(addr).expect("connect");
    send_ssl_request_and_startup(&mut alice, "alice", "db");
    let _ = read_auth_request_type(&mut alice);
    send_password_message(&mut alice, "correct-horse");
    let mut header = [0u8; 1];
    alice.read_exact(&mut header).expect("auth ok");
    let mut len_buf = [0u8; 4];
    alice.read_exact(&mut len_buf).expect("len");
    let mut code_buf = [0u8; 4];
    alice.read_exact(&mut code_buf).expect("code");
    let mut msg_type = read_message_type_discarding_body(&mut alice);
    while msg_type != b'Z' {
        msg_type = read_message_type_discarding_body(&mut alice);
    }
    send_simple_query(
        &mut alice,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'alice-secret') USING OPERATION_ID 'alice-op'",
    );
    let _tag = read_command_complete(&mut alice);
    read_ready_for_query(&mut alice);

    // bob が拡張経路で同じ id への INSERT を試みる（他テナント所有の物理
    // キー衝突。台帳キー空間はテナント非依存のため、bob 視点では未存在 id への
    // 通常の書き込みと区別できない応答になるべき——本テストは bob が同じ
    // operation_id を使った場合の応答と、別 id・別 operation_id を使った場合の
    // 応答本文が「行の存在を漏らさない」ことを、簡易クエリの SELECT で
    // alice の行が bob から見えないことのみを固定する（RLS-7 側の確認）。
    let mut bob = tcp_connect_as(addr, "bob", "correct-horse");
    send_simple_query(&mut bob, "SELECT id FROM documents LIMIT 10");
    let _cols = read_row_description(&mut bob);
    // bob からは alice の Private 行が見えないため、直後は CommandComplete。
    let (kind, tag) = read_message(&mut bob);
    assert_eq!(kind, b'C', "no rows visible to bob");
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "SELECT 0");
    read_ready_for_query(&mut bob);

    // bob が拡張経路でも同じ不可視性を確認する。
    parse_and_bind_as(&mut bob, "s1", "p1", "SELECT id FROM documents LIMIT 10");
    send_length_prefixed_message(&mut bob, b'E', &execute_body("p1", 0));
    let (kind, tag) = read_message(&mut bob);
    assert_eq!(
        kind, b'C',
        "no rows visible to bob over extended protocol either"
    );
    assert_eq!(String::from_utf8_lossy(&tag[..tag.len() - 1]), "SELECT 0");
    send_sync(&mut bob);
    assert_ready_for_query(&mut bob);
}

fn tcp_connect_as(addr: std::net::SocketAddr, user: &str, password: &str) -> std::net::TcpStream {
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    send_ssl_request_and_startup(&mut stream, user, "db");
    let _ = read_auth_request_type(&mut stream);
    send_password_message(&mut stream, password);
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("auth ok");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("len");
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("code");
    let mut msg_type = read_message_type_discarding_body(&mut stream);
    while msg_type != b'Z' {
        msg_type = read_message_type_discarding_body(&mut stream);
    }
    stream
}

fn parse_and_bind_as(stream: &mut std::net::TcpStream, statement: &str, portal: &str, sql: &str) {
    send_length_prefixed_message(stream, b'P', &parse_body(statement, sql, 0));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'1');
    send_length_prefixed_message(stream, b'B', &bind_body(portal, statement));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'2');
}

/// 受け入れ条件 12: `engine: None`（後方互換パス）では B/E/S/C/H は従来どおり
/// `0A000` + 切断のまま（`tests/wire_extended_query.rs` が既に固定しているが、
/// 本ファイルの命名規約に沿ってここでも 1 本だけ回帰確認する）。
#[test]
fn extended_messages_without_engine_are_still_rejected_and_closed() {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_accepting_one(&users_path);
    let mut stream = std::net::TcpStream::connect(addr).expect("connect");
    send_ssl_request_and_startup(&mut stream, "alice", "db");
    let _ = read_auth_request_type(&mut stream);
    send_password_message(&mut stream, "correct-horse");
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("auth ok");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("len");
    let mut code_buf = [0u8; 4];
    stream.read_exact(&mut code_buf).expect("code");
    let mut msg_type = read_message_type_discarding_body(&mut stream);
    while msg_type != b'Z' {
        msg_type = read_message_type_discarding_body(&mut stream);
    }

    send_length_prefixed_message(&mut stream, b'B', b"");
    assert_error_response(&mut stream, "0A000");
    let mut buf = [0u8; 1];
    let n = stream.read(&mut buf).unwrap_or(0);
    assert_eq!(n, 0, "connection must be closed");
}

/// 受け入れ条件 13: 簡易クエリの不変性。拡張シーケンスを挟んでも簡易クエリの
/// 応答バイト列が変わらない。
#[test]
fn simple_query_response_is_unchanged_across_extended_sequences() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 1);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    let sql = "SELECT id FROM documents LIMIT 1";

    let read_full = |stream: &mut std::net::TcpStream| -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let (kind, body) = read_message(stream);
            out.push(kind);
            out.extend_from_slice(&(body.len() as i32 + 4).to_be_bytes());
            out.extend_from_slice(&body);
            if kind == b'Z' {
                break;
            }
        }
        out
    };

    send_simple_query(&mut stream, sql);
    let first = read_full(&mut stream);

    parse_and_bind(&mut stream, "sI", "pI", sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pI", 0));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'D');
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'C');
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    send_simple_query(&mut stream, sql);
    let second = read_full(&mut stream);

    assert_eq!(
        first, second,
        "simple query response must be byte-identical"
    );
}

/// PR #1013 レビュー指摘（codex P1）の回帰防止: `ignore_till_sync`（拡張クエリ
/// プロトコルのエラー後の同期回復モード）中に受け取る `'X'`（Terminate）も、
/// 通常分岐と同じく length=4・body 厳密に空であることを検証してから終了する。
/// 検証をすり抜けて即座に `Ok(())` を返す実装では、長さフィールド不正な
/// malformed Terminate が「エラー後」という条件だけで正規の Terminate として
/// 受理されてしまう（フレーミング検証契約の回避）。修正後は他の malformed
/// frame と同じ `08P01`（invalid message frame）の `ErrorResponse` を受け取って
/// から接続が閉じる。
#[test]
fn malformed_terminate_during_ignore_till_sync_is_rejected_not_silently_closed() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    // 許可リスト外 SQL の Parse でエラー後の同期回復モードへ入る。
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("", "SELECT id FROM documents LIMIT $1", 0),
    );
    assert_error_response(&mut stream, "42601");

    // length=5（body 1 バイト）の malformed Terminate。正規の Terminate は
    // length=4・body 厳密に空。ここでは長さフィールド自体を短く宣言する
    // （length=3 は最小許容値 4 未満）。長さ検証は body を読む前に働くため、
    // 本テストでは body バイトを一切送らない（送出済みバイトが未読のまま
    // 残ると TCP の RST 経由で `ConnectionReset` になり、正常系の
    // クリーンな EOF と区別が付かなくなるため）。
    let mut msg = Vec::new();
    msg.push(b'X');
    msg.extend_from_slice(&3i32.to_be_bytes());
    stream.write_all(&msg).expect("send malformed terminate");

    // 修正前は body を一切読まず即座に `Ok(())` で正常終了していたため、
    // クライアントは ErrorResponse を受け取らずそのまま EOF になっていた。
    // 修正後は他の malformed frame と同じ経路（`handshake::respond_and_close`）
    // で 08P01 の ErrorResponse を受け取ってから接続が閉じる。
    assert_error_response(&mut stream, "08P01");

    // 接続はこの後閉じる（クリーンな EOF、または OS 依存で
    // `ConnectionReset` になることを許容する。いずれも「malformed
    // Terminate が正規の Terminate として静かに受理されたわけではない」
    // ことの確認が目的で、切断の正確な種別までは固定しない）。
    let mut buf = [0u8; 1];
    match stream.read(&mut buf) {
        Ok(n) => assert_eq!(n, 0, "connection must close after the malformed Terminate"),
        Err(e) => assert_eq!(
            e.kind(),
            std::io::ErrorKind::ConnectionReset,
            "unexpected read error after malformed Terminate: {e:?}"
        ),
    }
}

/// codex P0 指摘（PR #1013）の回帰防止: `ignore_till_sync`
/// （`extended_query` モジュールドキュメント「エラー後の同期回復」節）中に
/// 読み捨て対象となるメッセージ（'H' 等）の本文が、宣言長より短いバイト数で
/// 接続が切断された場合、その切り詰めを正常な読み捨てとして受理せず
/// fail-closed に接続を閉じる（`framing::discard_body` が `io::copy` の
/// 戻り値〔実コピー長〕を宣言長と照合するようになったことの確認）。
#[test]
fn truncated_body_during_ignore_till_sync_discard_closes_the_connection() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    // 許可リスト外 SQL の Parse でエラー後の同期回復モードへ入る。
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("", "SELECT id FROM documents LIMIT $1", 0),
    );
    assert_error_response(&mut stream, "42601");

    // Flush（'H'）を、宣言長 20（body 16 バイト）だが実際には 5 バイトしか
    // 送らないまま書き込み側を閉じることで、読み捨て中の途中切断を再現する。
    let mut msg = Vec::new();
    msg.push(b'H');
    msg.extend_from_slice(&20i32.to_be_bytes());
    msg.extend_from_slice(&[0u8; 5]);
    stream
        .write_all(&msg)
        .expect("send truncated Flush header+partial body");
    stream
        .shutdown(std::net::Shutdown::Write)
        .expect("shutdown write half to signal EOF mid-body");

    // 修正前は `io::copy` の戻り値を確認せず、EOF による切り詰めコピーを
    // そのまま `Ok(())` として受理していたため、この後もサーバーは
    // 同期回復モードのまま次のメッセージ（Sync）を待ち続けていた。
    // 修正後は `FrameError::Truncated` として fail-closed に接続を閉じる
    // （エラー応答は送出しない。回復不能なフレーミング違反は他の
    // `Truncated` 系と同じ扱い）。
    let mut buf = [0u8; 1];
    match stream.read(&mut buf) {
        Ok(n) => assert_eq!(
            n, 0,
            "connection must close after the truncated discard body"
        ),
        Err(e) => assert_eq!(
            e.kind(),
            std::io::ErrorKind::ConnectionReset,
            "unexpected read error after truncated discard body: {e:?}"
        ),
    }
}

/// PR #1013 レビュー指摘（Cursor Bugbot Medium）の回帰防止: 実行を試みて
/// 失敗した portal（`operation_id` 重複により `execute_parsed_in_session`
/// 自体がエラーを返すケース）は Sync を越えても `Ready` へ戻らず、次の
/// Execute は再実行されずに拒否される。以前は `PortalState::Failed` を
/// `execute_parsed_in_session` の成功後にしか立てていなかったため、呼び出し
/// 自体が失敗する経路ではこの保護が効かず、同じ named portal への再 Execute
/// が文を再実行しうる状態だった（named portal は Sync で破棄されないため
/// unnamed portal と異なりこの経路が回復を生き延びる）。
#[test]
fn failed_execute_marks_named_portal_failed_and_rejects_reexecute() {
    let (core, _guard) = new_core_with_documents_table();
    seed_rows(&core, 1);
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = tcp_connect(addr);

    // `seed_rows` が既に `operation_id = 'seed-1'` で id=1 を投入済みのため、
    // 同じ `operation_id` の再送は許可リスト検証（Parse）を通過するが、
    // `execute_parsed_in_session` の実行時（台帳照合）に `23505` で失敗する
    // ――「実行呼び出し自体が Err を返す」経路を再現する。
    let dup_sql = "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'row-1') USING OPERATION_ID 'seed-1'";
    parse_and_bind(&mut stream, "sF", "pF", dup_sql);

    send_length_prefixed_message(&mut stream, b'E', &execute_body("pF", 0));
    assert_error_then_recovers(&mut stream, "23505");

    // named portal は Sync を越えて残るため、同じ portal へ再度 Execute する。
    // 修正前は `Ready` のままだったため再実行を試み同じ `23505` が返っていた
    // （＝実行が繰り返されていた）のに対し、修正後は `PortalFailed` として
    // `08P01` を返し、実行し直すには新しい Bind が必要になる。
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pF", 0));
    assert_error_then_recovers(&mut stream, "08P01");
}

/// PR #1013 レビュー指摘（codex P1）の回帰防止: 書き込み系文（`RETURNING`
/// 付き `INSERT`）の commit 成功後、中断バイト上限超過により後処理が失敗した
/// 場合、`ErrorResponse` に `state=may_be_committed` の `D`（detail）フィールドが
/// 付き、通常の（commit 前の）失敗とは区別できる（`RECOVER-5` (3)・ERR-5 の
/// 既存 detail 契約を、panic 経由の緊急応答だけでなく通常の Err 経路にも
/// 拡張適用したことの確認）。
#[test]
fn post_commit_suspended_bytes_overflow_carries_may_be_committed_detail() {
    // `MAX_SUSPENDED_PORTAL_BYTES_PER_SESSION`（16 MiB）に対し、フィラー portal
    // 単体では収まるが、`INSERT ... RETURNING` の結果行を加えると上限を
    // 超える組み合わせを作る（`suspended_portal_bytes_are_accounted_across_
    // the_whole_session_not_per_portal` と同じ手法）。
    const ROW_BODY_LEN: usize = 820_000;
    const ROW_COUNT: u64 = 21; // 1 行即時送出 + 20 行中断保持 ≈ 16,400,000 バイト
    const RETURNING_BODY_LEN: usize = 500_000; // ≈16,900,000 バイトへ押し上げる

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
    let mut stream = tcp_connect(addr);

    // 他の named portal を中断状態のまま保持させ、中断保持バイト上限
    // （セッション全体で判定）にほぼ（だが超えない量まで）達させる。
    let filler_sql = format!(
        "SELECT id, body FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT {ROW_COUNT}"
    );
    parse_and_bind(&mut stream, "sFiller", "pFiller", &filler_sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pFiller", 1));
    let (kind, _) = read_message(&mut stream); // 1 行分の DataRow（即時送出）
    assert_eq!(kind, b'D');
    let (kind, _) = read_message(&mut stream); // PortalSuspended（残り 20 行）
    assert_eq!(
        kind, b's',
        "filler portal must stay under the session limit on its own"
    );

    // `INSERT ... RETURNING` は commit 成功後に結果行をエンコードする。
    // 中断保持バイト上限の判定は「送出予定（`take`）に入らない行」だけを
    // 対象にするため（`execute_portal` の中断バイト上限判定コメント参照）、
    // ここでは 2 行分の `VALUES` を持つ複数行 `INSERT ... RETURNING`
    // （1 行目は極小、2 行目に大きな `body`）を `max_rows=1` で実行し、
    // 2 行目を中断保持対象にする。フィラー portal が既に保持している
    // バイト数と 2 行目の中断保持見込みバイト数を合算すると上限
    // （16 MiB）を超えるため、この Execute の後処理（中断バイト上限判定）が
    // 必ず失敗する（単一行 `RETURNING` は常に `take == total` となり
    // この判定ループへ到達しないため、複数行形が必要）。
    let returning_body = "y".repeat(RETURNING_BODY_LEN);
    let insert_sql = format!(
        "INSERT INTO documents (id, embedding, body) VALUES (99998, '[0.1,0.2,0.3]', 'small'), (99999, '[0.1,0.2,0.3]', '{returning_body}') RETURNING body USING OPERATION_ID 'op-returning-overflow'"
    );
    parse_and_bind(&mut stream, "sIns", "pIns", &insert_sql);
    send_length_prefixed_message(&mut stream, b'E', &execute_body("pIns", 1));

    let (kind, body) = read_message(&mut stream);
    assert_eq!(
        kind, b'E',
        "expected the suspended-bytes limit to be exceeded"
    );
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("54000"),
        "expected 54000 (suspended bytes exceeded), got {text:?}"
    );
    assert!(
        text.contains("state=may_be_committed"),
        "post-commit failure must carry the may_be_committed detail: {text:?}"
    );
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // commit は実際に成功しているため、行は見える（後処理失敗であって書き込み
    // 失敗ではないことの確認）。
    send_simple_query(
        &mut stream,
        "SELECT id FROM documents WHERE id = 99999 LIMIT 1",
    );
    let (kind, dbg_body) = read_message(&mut stream);
    assert_eq!(
        kind,
        b'T',
        "expected RowDescription, got {kind} body={:?}",
        String::from_utf8_lossy(&dbg_body)
    );
    let _row = read_data_row(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}
