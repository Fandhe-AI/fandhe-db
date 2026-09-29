//! 拡張クエリプロトコル経由の明示トランザクション `BEGIN`/`COMMIT`/`ROLLBACK`
//! （SQL-31・TASK-221）の結合テスト（Issue #942 codex-review 指摘対応）。
//!
//! 以前は `extended_query::handle_execute`/`execute_portal` が
//! `engine::execute_parsed_in_session`（トランザクション文脈を持たない
//! autocommit 専用の入口）を呼んでいたため、拡張クエリプロトコル経由で
//! `BEGIN` を Parse/Bind/Execute しても `Idle` から進めず常に `0A000`
//! （`transaction_feature_not_supported`）で拒否されていた——コミット
//! メッセージ・`docs/design/explicit-transaction.md`・
//! `crates/engine/src/sql/transaction.rs` のモジュールドキュメントはいずれも
//! 「簡易クエリ・拡張クエリの両経路で受理する」と述べていたが、実装は
//! 簡易クエリ（`crate::simple_query`）経由のみ結線済みで、拡張クエリ経由は
//! 未結線のまま不一致だった。本ファイルは `handshake::post_auth_loop` が
//! 保持する接続単位の `SessionTransaction` を拡張クエリ経路でも共有する
//! ように結線した後の受理を固定する。

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
    let path = temp_db::unique_db_path("wire942-extended-transaction");
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

fn send_sync(stream: &mut std::net::TcpStream) {
    send_length_prefixed_message(stream, b'S', b"");
}

fn assert_ready_for_query(stream: &mut std::net::TcpStream) {
    let (kind, _body) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery");
}

/// `ReadyForQuery` を読み、トランザクション状態バイト（`'I'`／`'T'`／`'E'`）が
/// `expected` と一致することを検証する（WIRE-19・PR #1041 レビュー指摘 P1の
/// 回帰: 以前は明示トランザクションの状態に関わらず常に `'I'` を送出していた）。
fn assert_ready_for_query_status(stream: &mut std::net::TcpStream, expected: u8) {
    let (kind, body) = read_message(stream);
    assert_eq!(kind, b'Z', "expected ReadyForQuery");
    assert_eq!(
        body.last().copied(),
        Some(expected),
        "ReadyForQuery status byte mismatch"
    );
}

fn parse_and_bind(stream: &mut std::net::TcpStream, statement: &str, portal: &str, sql: &str) {
    send_length_prefixed_message(stream, b'P', &parse_body(statement, sql, 0));
    let (kind, body) = read_message(stream);
    assert_eq!(
        kind,
        b'1',
        "expected ParseComplete for {sql}: {}",
        String::from_utf8_lossy(&body)
    );
    send_length_prefixed_message(stream, b'B', &bind_body(portal, statement));
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'2', "expected BindComplete");
}

/// Execute し `CommandComplete` タグを読み取る（Describe を伴わない
/// トランザクション制御文・`INSERT` 用。`RowDescription` を持たない）。
fn execute_and_read_command_complete(stream: &mut std::net::TcpStream, portal: &str) -> String {
    send_length_prefixed_message(stream, b'E', &execute_body(portal, 0));
    let (kind, tag) = read_message(stream);
    assert_eq!(kind, b'C', "expected CommandComplete");
    String::from_utf8_lossy(&tag[..tag.len() - 1]).into_owned()
}

/// Issue #942 codex-review 指摘対応の中心テスト: Parse/Bind/Execute で
/// `BEGIN` すると（以前のように `0A000` へ落ちず）`CommandComplete BEGIN` が
/// 返ること、続けて同一トランザクション内で `INSERT` を Execute し
/// `COMMIT` すると commit されて可視になること、`ROLLBACK` すると破棄される
/// ことを固定する。
#[test]
fn begin_insert_commit_over_extended_protocol_is_accepted_and_visible() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    // BEGIN。以前は `execute_parsed_in_session`（autocommit 専用）が
    // `ParsedSql::Transaction` を無条件に `0A000` で拒否していたため、拡張
    // クエリプロトコル経由では `CommandComplete` に到達できなかった。
    parse_and_bind(&mut stream, "begin1", "pb1", "BEGIN");
    let tag = execute_and_read_command_complete(&mut stream, "pb1");
    assert_eq!(
        tag, "BEGIN",
        "BEGIN must succeed over the extended protocol"
    );
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // 同一トランザクション内で INSERT。
    let insert_sql =
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'hello') USING OPERATION_ID 'op-942-1'";
    parse_and_bind(&mut stream, "ins1", "pi1", insert_sql);
    let tag = execute_and_read_command_complete(&mut stream, "pi1");
    assert_eq!(tag, "INSERT 0 1");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // COMMIT。
    parse_and_bind(&mut stream, "commit1", "pc1", "COMMIT");
    let tag = execute_and_read_command_complete(&mut stream, "pc1");
    assert_eq!(tag, "COMMIT");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // commit 済みの行が簡易クエリ経由で読み戻せる（read-your-writes）。
    send_simple_query(&mut stream, "SELECT id FROM documents WHERE id = 1 LIMIT 1");
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id".to_string()]);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![Some("1".to_string())]);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// `BEGIN` → `INSERT` → `ROLLBACK` を拡張クエリプロトコル経由で行うと、行が
/// 一切 commit されない（`COMMIT` していない書き込みは破棄される契約。
/// `sql::transaction` モジュールドキュメント参照）ことを固定する。
#[test]
fn begin_insert_rollback_over_extended_protocol_discards_the_row() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    parse_and_bind(&mut stream, "begin1", "pb1", "BEGIN");
    let tag = execute_and_read_command_complete(&mut stream, "pb1");
    assert_eq!(tag, "BEGIN");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    let insert_sql =
        "INSERT INTO documents (id, embedding, body) VALUES (2, '[0.4,0.5,0.6]', 'discarded') USING OPERATION_ID 'op-942-2'";
    parse_and_bind(&mut stream, "ins1", "pi1", insert_sql);
    let tag = execute_and_read_command_complete(&mut stream, "pi1");
    assert_eq!(tag, "INSERT 0 1");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    parse_and_bind(&mut stream, "rollback1", "pr1", "ROLLBACK");
    let tag = execute_and_read_command_complete(&mut stream, "pr1");
    assert_eq!(tag, "ROLLBACK");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM documents WHERE id = 2 LIMIT 1");
    let _columns = read_row_description(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    // ROLLBACK 済みの行を engine API から直接確認する（wire 経由の SELECT の
    // 結果件数 0 だけでは、書き込み自体が起きていないのか破棄されたのかを
    // 区別できないため）。
    let mut session = engine::sql::mode::SessionState::default();
    let ctx = engine::policy::PolicyContext::new("tenant-a").expect("valid tenant id");
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            "SELECT id FROM documents WHERE id = 2 LIMIT 1",
        )
        .expect("select via engine API");
    match outcome {
        engine::sql::SqlOutcome::Query(result) => assert_eq!(
            result.rows.len(),
            0,
            "id=2 は ROLLBACK により commit されていないはず"
        ),
        other => panic!("expected Query, got {other:?}"),
    }
}

/// 拡張クエリプロトコルの明示トランザクション内で `UPDATE`・`DELETE`・複数行
/// `INSERT`・UPSERT が実行でき（Issue #1179）、`ROLLBACK` で全て破棄され、
/// `COMMIT` で確定すること。
#[test]
fn update_delete_multi_row_insert_and_upsert_over_extended_protocol_commit_and_rollback() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, &insert_sql(70, "op-1179-seed"));
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    let statements = [
        (
            "INSERT INTO documents (id, embedding, body) VALUES (71, '[0.1,0.2,0.3]', 'a'), (72, '[0.1,0.2,0.3]', 'b') USING OPERATION_ID 'op-1179-multi'",
            "INSERT 0 2",
        ),
        (
            "INSERT INTO documents (id, embedding, body) VALUES (70, '[0.1,0.2,0.3]', 'up') ON CONFLICT (id) DO UPDATE SET body = EXCLUDED.body USING OPERATION_ID 'op-1179-upsert'",
            "INSERT 0 1",
        ),
        (
            "UPDATE documents SET body = 'changed' WHERE id = 71 USING OPERATION_ID 'op-1179-update'",
            "UPDATE 1",
        ),
        (
            "DELETE FROM documents WHERE id = 72 USING OPERATION_ID 'op-1179-delete'",
            "DELETE 1",
        ),
    ];

    for (round, end) in [("ROLLBACK", "ROLLBACK"), ("COMMIT", "COMMIT")] {
        let (bs, bp) = (format!("b{round}"), format!("pb{round}"));
        parse_and_bind(&mut stream, &bs, &bp, "BEGIN");
        assert_eq!(execute_and_read_command_complete(&mut stream, &bp), "BEGIN");
        send_sync(&mut stream);
        assert_ready_for_query_status(&mut stream, b'T');
        for (i, (sql, expected_tag)) in statements.iter().enumerate() {
            let stmt = format!("s{round}{i}");
            let portal = format!("p{round}{i}");
            parse_and_bind(&mut stream, &stmt, &portal, sql);
            assert_eq!(
                execute_and_read_command_complete(&mut stream, &portal),
                *expected_tag
            );
            send_sync(&mut stream);
            assert_ready_for_query_status(&mut stream, b'T');
        }
        let (es, ep) = (format!("e{round}"), format!("pe{round}"));
        parse_and_bind(&mut stream, &es, &ep, end);
        assert_eq!(execute_and_read_command_complete(&mut stream, &ep), end);
        send_sync(&mut stream);
        assert_ready_for_query_status(&mut stream, b'I');

        if round == "ROLLBACK" {
            // 全て破棄され、operation_id も再利用できる（次のラウンドで同じ ID を使う）。
            assert_eq!(visible_rows_with_id(&core, 70), 1);
            assert_eq!(visible_rows_with_id(&core, 71), 0);
            assert_eq!(visible_rows_with_id(&core, 72), 0);
        }
    }
    assert_eq!(visible_rows_with_id(&core, 70), 1);
    assert_eq!(visible_rows_with_id(&core, 71), 1);
    assert_eq!(visible_rows_with_id(&core, 72), 0);
}

/// engine API から `id` の行が可視かどうかを数える（wire 経由の結果件数 0 だけ
/// では「書き込みが起きなかった」のか「破棄された」のかを区別できないため）。
fn visible_rows_with_id(core: &EngineCore, id: u64) -> usize {
    let mut session = engine::sql::mode::SessionState::default();
    // wire 経由の INSERT は `Private` 行になりうるため、`Public` のみの
    // `PolicyContext::new` では常に 0 件になり判定が空振りする。認証済み
    // セッションと同じく両方の可視性を持つ文脈で数える。
    let ctx = engine::policy::PolicyContext::with_visibilities(
        "tenant-a",
        [
            engine::storage::Visibility::Public,
            engine::storage::Visibility::Private,
        ],
    )
    .expect("valid tenant id");
    let outcome = core
        .execute_sql_in_session(
            &ctx,
            &mut session,
            &format!("SELECT id FROM documents WHERE id = {id} LIMIT 1"),
        )
        .expect("select via engine API");
    match outcome {
        engine::sql::SqlOutcome::Query(result) => result.rows.len(),
        other => panic!("expected Query, got {other:?}"),
    }
}

fn insert_sql(id: u64, op: &str) -> String {
    format!(
        "INSERT INTO documents (id, embedding, body) VALUES ({id}, '[0.1,0.2,0.3]', 'row') USING OPERATION_ID '{op}'"
    )
}

/// 簡易クエリで `BEGIN` → `INSERT` を送り、`Active` にする。
fn begin_and_insert_over_simple_query(stream: &mut std::net::TcpStream, id: u64, op: &str) {
    send_simple_query(stream, "BEGIN");
    assert_eq!(read_command_complete(stream), "BEGIN");
    read_ready_for_query(stream);
    send_simple_query(stream, &insert_sql(id, op));
    assert_eq!(read_command_complete(stream), "INSERT 0 1");
    read_ready_for_query(stream);
}

/// `COMMIT` が `25P02` で拒否され、`ROLLBACK` で `Idle` へ戻ることを確認する。
fn expect_commit_rejected_then_rollback(stream: &mut std::net::TcpStream) {
    send_simple_query(stream, "COMMIT");
    expect_error_response_with_sqlstate(stream, "25P02");
    read_ready_for_query(stream);
    send_simple_query(stream, "ROLLBACK");
    assert_eq!(read_command_complete(stream), "ROLLBACK");
    read_ready_for_query(stream);
}

/// 簡易クエリの構文エラーでもトランザクションが `Failed` へ遷移し、後続の
/// `COMMIT` が先行する `INSERT` を永続化しないこと（PR #1041 レビュー指摘）。
#[test]
fn simple_query_parse_error_inside_transaction_blocks_commit() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 30, "op-942-30");
    send_simple_query(&mut stream, "SELEC id FROM documents");
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);

    expect_commit_rejected_then_rollback(&mut stream);
    assert_eq!(visible_rows_with_id(&core, 30), 0);
}

/// 複数文メッセージの分割エラー（文数上限超過 `54000`）でもトランザクションが
/// `Failed` へ遷移すること（PR #1041 レビュー指摘の横断確認）。
#[test]
fn simple_query_split_error_inside_transaction_blocks_commit() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 31, "op-942-31");
    let too_many = "SELECT id FROM documents LIMIT 1;"
        .repeat(engine::sql::statement_splitter::MAX_STATEMENTS_PER_QUERY + 1);
    send_simple_query(&mut stream, &too_many);
    expect_error_response_with_sqlstate(&mut stream, "54000");
    read_ready_for_query(&mut stream);

    expect_commit_rejected_then_rollback(&mut stream);
    assert_eq!(visible_rows_with_id(&core, 31), 0);
}

/// 拡張クエリプロトコルの Parse エラーでもトランザクションが `Failed` へ遷移し、
/// 後続の `COMMIT` が先行する `INSERT` を永続化しないこと（PR #1041 レビュー
/// 指摘の横断確認）。
#[test]
fn extended_parse_error_inside_transaction_blocks_commit() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 32, "op-942-32");
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("bad1", "SELEC id FROM documents", 0),
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    expect_commit_rejected_then_rollback(&mut stream);
    assert_eq!(visible_rows_with_id(&core, 32), 0);
}

/// `Failed` 中の COPY が autocommit として実行されず `25P02` で拒否されること
/// （PR #1041 レビュー指摘）。CopyInResponse へ進まずに ErrorResponse が返る。
#[test]
fn copy_while_transaction_failed_is_rejected_with_in_failed_sql_transaction() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 33, "op-942-33");
    // 入れ子の BEGIN で Failed へ遷移させる。
    send_simple_query(&mut stream, "BEGIN");
    expect_error_response_with_sqlstate(&mut stream, "25001");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "COPY documents (id, embedding, body) FROM STDIN USING OPERATION_ID 'op-942-copy'",
    );
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
    assert_eq!(visible_rows_with_id(&core, 33), 0);
}

/// 期限切れ後は SQL を伴わない要求（Sync）でも書き込みトランザクションが
/// abort されライタが解放されること。解放されていなければ別接続の autocommit
/// `INSERT` は書き込みゲートの待機上限で `55P03` になる。元の接続の `COMMIT` には
/// 上限超過の `54000` が返る（PR #1041 レビュー指摘）。
#[test]
fn expired_transaction_releases_writer_on_next_protocol_message() {
    let path = temp_db::unique_db_path("wire942-expired-transaction");
    let _guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path)
        .expect("open storage")
        .with_write_lock_wait(std::time::Duration::from_millis(300));
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let max_duration = std::time::Duration::from_millis(200);
    let core = Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_transaction_limits(
            engine::sql::transaction::TransactionLimits {
                max_duration,
                max_statements: 1_000,
            },
        ),
    );
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 34, "op-942-34");
    std::thread::sleep(max_duration * 2);

    // SQL を伴わない要求（Sync）だけを送る。`release_if_expired` が
    // `Active` を `Failed` へ遷移させる（`Idle` へは戻さない）ため、状態は
    // 直後の Sync 時点ですでに `'E'`（WIRE-19・Issue #943）。
    send_sync(&mut stream);
    assert_ready_for_query_status(&mut stream, b'E');

    // 別接続の autocommit INSERT がライタを取得できる。
    let mut other = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(&mut other, &insert_sql(35, "op-942-35"));
    assert_eq!(read_command_complete(&mut other), "INSERT 0 1");
    read_ready_for_query(&mut other);

    // `54000` は `take_failed_error` 経由の通常のエラー報告であり、`Failed`
    // を解除しない（Sync は ignore-till-sync を解除するだけ）ため引き続き
    // `'E'`。
    send_simple_query(&mut stream, "COMMIT");
    expect_error_response_with_sqlstate(&mut stream, "54000");
    assert_ready_for_query_status(&mut stream, b'E');
    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_ready_for_query_status(&mut stream, b'I');

    assert_eq!(visible_rows_with_id(&core, 34), 0);
    assert_eq!(visible_rows_with_id(&core, 35), 1);
    // 別接続で書いた id=35 は同じ接続から読み戻せる（read-your-writes）。
    send_simple_query(&mut other, "SELECT id FROM documents WHERE id = 35 LIMIT 1");
    let _columns = read_row_description(&mut other);
    assert_eq!(read_data_row(&mut other), vec![Some("35".to_string())]);
    let _tag = read_command_complete(&mut other);
    read_ready_for_query(&mut other);
}

/// BEGIN 後にクライアントが一切送信しない（無通信）場合も、持続時間の上限に
/// 達した時点で書き込みトランザクションが abort されライタが解放されること
/// （PR #1041 レビュー指摘: 以前は次の要求を受信するまで期限を検査しなかった
/// ため、接続の読み取りタイムアウト〔既定 30 秒〕までライタを占有し続けた）。
/// 解放されていなければ別接続の autocommit `INSERT` は書き込みゲートの待機上限
/// （ここでは上限の数倍）で `55P03` になる。接続は切断されず、元の接続の
/// `COMMIT` には上限超過の `54000` が返り、`ROLLBACK` で `Idle` へ戻れる。
#[test]
fn expired_transaction_releases_writer_while_client_is_silent() {
    let path = temp_db::unique_db_path("wire942-expired-silent-transaction");
    let _guard = temp_db::CleanupGuard(path.clone());
    let max_duration = std::time::Duration::from_millis(200);
    let storage = Storage::open(&path)
        .expect("open storage")
        .with_write_lock_wait(max_duration * 10);
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_transaction_limits(
            engine::sql::transaction::TransactionLimits {
                max_duration,
                max_statements: 1_000,
            },
        ),
    );
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 39, "op-942-39");

    // 元の接続からは何も送らないまま、別接続の autocommit INSERT がライタを
    // 取得できる（上限経過後・接続の読み取りタイムアウトより十分前）。
    let started = std::time::Instant::now();
    let mut other = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(&mut other, &insert_sql(40, "op-942-40"));
    assert_eq!(read_command_complete(&mut other), "INSERT 0 1");
    read_ready_for_query(&mut other);
    assert!(
        started.elapsed() < wire_server::limits::READ_TIMEOUT,
        "the writer must be released at the transaction deadline, not at the read timeout"
    );

    // 元の接続は維持されており、期限切れを COMMIT で観測できる。期限超過は
    // `Active` を `Failed` へ遷移させ Sync では解除されないため `54000` の後も
    // `'E'`（WIRE-19・Issue #943）。
    send_simple_query(&mut stream, "COMMIT");
    expect_error_response_with_sqlstate(&mut stream, "54000");
    assert_ready_for_query_status(&mut stream, b'E');
    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_ready_for_query_status(&mut stream, b'I');

    assert_eq!(visible_rows_with_id(&core, 39), 0);
    assert_eq!(visible_rows_with_id(&core, 40), 1);
}

/// 明示トランザクション中の `COPY FROM STDIN`（Issue #1179）で、クライアントが
/// CopyInResponse の後に何も送らない（あるいは少しずつ送り続ける）場合も、
/// 持続時間の上限で接続を閉じ、共有書き込みトランザクションを破棄してライタを
/// 解放すること（単一ライタを無期限に保持させない DoS 対策。フレーム受信期限
/// `framing::FrameDeadlineGuard` が COPY サブプロトコルの受信にも及ぶ）。解放されて
/// いなければ別接続の autocommit `INSERT` は書き込みゲートの待機上限で `55P03` になる。
#[test]
fn stalled_copy_inside_transaction_releases_writer_at_the_deadline() {
    let path = temp_db::unique_db_path("wire1179-stalled-copy-in-transaction");
    let _guard = temp_db::CleanupGuard(path.clone());
    let max_duration = std::time::Duration::from_millis(300);
    let storage = Storage::open(&path)
        .expect("open storage")
        .with_write_lock_wait(max_duration * 10);
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_transaction_limits(
            engine::sql::transaction::TransactionLimits {
                max_duration,
                max_statements: 1_000,
            },
        ),
    );
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    read_ready_for_query(&mut stream);
    send_simple_query(
        &mut stream,
        "COPY documents (id, embedding, body) FROM STDIN USING OPERATION_ID 'op-1179-copy'",
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'G', "expected CopyInResponse");

    // 以降は何も送らない。上限経過後に別接続の autocommit INSERT がライタを取得できる。
    let started = std::time::Instant::now();
    let mut other = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(&mut other, &insert_sql(60, "op-1179-60"));
    assert_eq!(read_command_complete(&mut other), "INSERT 0 1");
    read_ready_for_query(&mut other);
    assert!(
        started.elapsed() < wire_server::limits::READ_TIMEOUT,
        "the writer must be released at the transaction deadline, not at the read timeout"
    );
    assert_eq!(visible_rows_with_id(&core, 60), 1);
}

/// 持続時間の上限で `Failed` へ遷移した後は、実行を開始済みの portal
/// （`Suspended`：残り行あり／`Done`：完了済み）への Execute も成功に見せず、
/// 期限切れの `54000` を返すこと（PR #1041 レビュー指摘: 以前は `Ready` 以外の
/// portal が `execute_parsed_in_txn` を通らず、abort 後も残り行の送出や
/// `CommandComplete` の再送を続けていた）。
fn run_started_portal_after_expiry_case(fully_consume_first: bool, label: &str) {
    let path = temp_db::unique_db_path(label);
    let _guard = temp_db::CleanupGuard(path.clone());
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
    let max_duration = std::time::Duration::from_millis(200);
    let core = Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_transaction_limits(
            engine::sql::transaction::TransactionLimits {
                max_duration,
                max_statements: 1_000,
            },
        ),
    );
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    for (id, op) in [(41, "op-942-41"), (42, "op-942-42")] {
        send_simple_query(&mut stream, &insert_sql(id, op));
        assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
        read_ready_for_query(&mut stream);
    }
    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    read_ready_for_query(&mut stream);

    parse_and_bind(&mut stream, "s1", "p1", "SELECT id FROM documents LIMIT 10");
    if fully_consume_first {
        send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 0));
        loop {
            let (kind, _) = read_message(&mut stream);
            if kind == b'C' {
                break;
            }
            assert_eq!(kind, b'D', "expected DataRow or CommandComplete");
        }
    } else {
        send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 1));
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b'D', "expected DataRow");
        let (kind, _) = read_message(&mut stream);
        assert_eq!(kind, b's', "expected PortalSuspended");
    }

    // 上限を超えるまで無通信のまま待つ（受信待ちの打ち切りで Failed へ遷移する）。
    std::thread::sleep(max_duration * 2);

    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 1));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(
        kind, b'E',
        "a started portal must not run after the transaction expired"
    );
    assert!(
        body.windows(6).any(|w| w == b"C54000"),
        "the first request after expiry must report 54000"
    );
    send_sync(&mut stream);
    assert_ready_for_query_status(&mut stream, b'E');

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
}

#[test]
fn suspended_portal_is_rejected_after_transaction_expiry() {
    run_started_portal_after_expiry_case(false, "wire942-suspended-portal-expired");
}

#[test]
fn completed_portal_is_rejected_after_transaction_expiry() {
    run_started_portal_after_expiry_case(true, "wire942-done-portal-expired");
}

/// BEGIN 後にクライアントが 1 つのフレームの本文を少しずつ送り続けても、持続
/// 時間の上限でフレームの受信を打ち切り、接続を閉じてライタを解放すること
/// （PR #1041 レビュー指摘: 以前は型バイト受信後に読み取りタイムアウトを戻して
/// いたため、読み取りタイムアウト未満の間隔で送り続ければ上限後もライタを
/// 保持できた）。
#[test]
fn expired_transaction_releases_writer_while_client_drips_a_frame_body() {
    let path = temp_db::unique_db_path("wire942-expired-drip-transaction");
    let _guard = temp_db::CleanupGuard(path.clone());
    let max_duration = std::time::Duration::from_millis(300);
    let storage = Storage::open(&path)
        .expect("open storage")
        .with_write_lock_wait(max_duration * 10);
    storage
        .create_table(&TableSchema::new(
            "documents",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = Arc::new(
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_transaction_limits(
            engine::sql::transaction::TransactionLimits {
                max_duration,
                max_statements: 1_000,
            },
        ),
    );
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 47, "op-942-47");

    // 'Q' の型バイトと 1 KiB を宣言する長さだけ送り、本文は 1 バイトずつ送る。
    let mut writer = stream.try_clone().expect("clone stream");
    std::thread::spawn(move || {
        use std::io::Write as _;
        let mut header = vec![b'Q'];
        header.extend_from_slice(&1028i32.to_be_bytes());
        if writer.write_all(&header).is_err() {
            return;
        }
        for _ in 0..1024 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if writer.write_all(b" ").is_err() {
                return;
            }
        }
    });

    let started = std::time::Instant::now();
    let mut other = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(&mut other, &insert_sql(48, "op-942-48"));
    assert_eq!(read_command_complete(&mut other), "INSERT 0 1");
    read_ready_for_query(&mut other);
    assert!(
        started.elapsed() < max_duration * 10,
        "the writer must be released at the transaction deadline even while a frame is dripping"
    );

    // 元の接続は応答なしで閉じられる（途中まで送った書き込みは確定しない）。
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .expect("set read timeout");
    let mut buf = [0u8; 1];
    let closed = match stream.read(&mut buf) {
        Ok(0) => true,
        Ok(_) => false,
        Err(e) => !matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ),
    };
    assert!(closed, "the connection must be closed without a response");
    assert_eq!(visible_rows_with_id(&core, 47), 0);
    assert_eq!(visible_rows_with_id(&core, 48), 1);
}

/// 拡張クエリの 1 メッセージを送り、ErrorResponse が返ったあと Sync で
/// ReadyForQuery まで進める。
fn send_expect_error_then_sync(stream: &mut std::net::TcpStream, type_byte: u8, body: &[u8]) {
    send_length_prefixed_message(stream, type_byte, body);
    let (kind, _) = read_message(stream);
    assert_eq!(kind, b'E', "expected ErrorResponse");
    send_sync(stream);
    assert_ready_for_query(stream);
}

/// 拡張クエリプロトコルの Bind エラーでも明示トランザクションが `Failed` へ
/// 遷移し、後続の `COMMIT` が先行する `INSERT` を永続化しないこと（PR #1041
/// レビュー指摘）。
#[test]
fn extended_bind_error_inside_transaction_blocks_commit() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 36, "op-942-36");
    // 存在しないステートメントへの Bind。
    send_expect_error_then_sync(&mut stream, b'B', &bind_body("p1", "no-such-statement"));

    expect_commit_rejected_then_rollback(&mut stream);
    assert_eq!(visible_rows_with_id(&core, 36), 0);
}

/// 拡張クエリプロトコルの Describe エラーでも明示トランザクションが `Failed` へ
/// 遷移し、後続の `COMMIT` が先行する `INSERT` を永続化しないこと（PR #1041
/// レビュー指摘）。
#[test]
fn extended_describe_error_inside_transaction_blocks_commit() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 37, "op-942-37");
    // 存在しないステートメントの Describe。
    let mut body = vec![b'S'];
    body.extend_from_slice(b"no-such-statement\0");
    send_expect_error_then_sync(&mut stream, b'D', &body);

    expect_commit_rejected_then_rollback(&mut stream);
    assert_eq!(visible_rows_with_id(&core, 37), 0);
}

/// 対照: エラーなしで COMMIT すれば行が確定する（上の各テストの行数判定が
/// 空振りしていないことの確認）。
#[test]
fn begin_insert_commit_over_simple_query_persists_the_row() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 38, "op-942-38");
    send_simple_query(&mut stream, "COMMIT");
    assert_eq!(read_command_complete(&mut stream), "COMMIT");
    read_ready_for_query(&mut stream);
    assert_eq!(visible_rows_with_id(&core, 38), 1);
}

/// `Failed` 中は構文エラーの文も parse より前に `25P02` で拒否され、`ROLLBACK`
/// だけは受理されること（PR #1041 レビュー指摘）。
#[test]
fn syntax_error_while_failed_is_rejected_with_in_failed_sql_transaction() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 39, "op-942-39");
    send_simple_query(&mut stream, "BEGIN");
    expect_error_response_with_sqlstate(&mut stream, "25001");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELEC id FROM documents");
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
    assert_eq!(visible_rows_with_id(&core, 39), 0);
}

/// 拡張クエリプロトコルでも `Failed` 中は `ROLLBACK` 以外の Parse・Bind を
/// `25P02` で拒否し、`ROLLBACK` の Parse/Bind/Execute は受理すること
/// （`Failed` になる前に Parse 済みのステートメントの Bind も拒否する。PR #1041
/// レビュー指摘）。
#[test]
fn extended_parse_and_bind_while_failed_are_rejected_except_rollback() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 43, "op-942-43");
    // Failed になる前に INSERT を Parse しておく。
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("ins-before", &insert_sql(44, "op-942-44"), 0),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1', "expected ParseComplete");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "BEGIN");
    expect_error_response_with_sqlstate(&mut stream, "25001");
    read_ready_for_query(&mut stream);

    // 構文エラーの Parse も 25P02。
    send_length_prefixed_message(&mut stream, b'P', &parse_body("bad", "SELEC 1", 0));
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // Failed 前に Parse 済みの INSERT の Bind も 25P02。
    send_length_prefixed_message(&mut stream, b'B', &bind_body("pi", "ins-before"));
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // ROLLBACK は Parse/Bind/Execute とも受理される。
    parse_and_bind(&mut stream, "rb", "prb", "ROLLBACK");
    assert_eq!(
        execute_and_read_command_complete(&mut stream, "prb"),
        "ROLLBACK"
    );
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    assert_eq!(visible_rows_with_id(&core, 43), 0);
    assert_eq!(visible_rows_with_id(&core, 44), 0);
}

/// `Failed` 中は、フレーム本体の構造検証の後、機能・対象の検証より先に状態を
/// 判定し `25P02` を返すこと（PR #1041 レビュー指摘: 以前は Parse のパラメータ
/// 型 OID 指定が `0A000`、Bind の不正な format code・未登録ステートメント、
/// Execute の未登録 portal がそれぞれ別のエラーを先に返していた）。
#[test]
fn extended_functional_errors_while_failed_are_reported_as_in_failed_sql_transaction() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 45, "op-942-45");
    // Failed になる前に INSERT を Parse しておく（format code 検証の対象）。
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("ins-before", &insert_sql(46, "op-942-46"), 0),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1', "expected ParseComplete");
    send_sync(&mut stream);
    assert_ready_for_query(&mut stream);

    // 入れ子の BEGIN で Failed へ遷移させる。
    send_simple_query(&mut stream, "BEGIN");
    expect_error_response_with_sqlstate(&mut stream, "25001");
    read_ready_for_query(&mut stream);

    // Parse: パラメータ型 OID を 1 件指定（未対応機能の 0A000 より先に 25P02）。
    let mut typed_parse = parse_body("typed", "SELECT id FROM documents LIMIT 1", 1);
    typed_parse.extend_from_slice(&25i32.to_be_bytes());
    send_length_prefixed_message(&mut stream, b'P', &typed_parse);
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    send_sync(&mut stream);
    assert_ready_for_query_status(&mut stream, b'E');

    // Bind: 未登録ステートメント。
    send_length_prefixed_message(&mut stream, b'B', &bind_body("p-unknown", "no-such"));
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    send_sync(&mut stream);
    assert_ready_for_query_status(&mut stream, b'E');

    // Bind: 不正なパラメータ format code（binary 指定）。
    let mut bad_format = Vec::new();
    bad_format.extend_from_slice(b"p-bad\0ins-before\0");
    bad_format.extend_from_slice(&1i16.to_be_bytes()); // param format code count
    bad_format.extend_from_slice(&1i16.to_be_bytes()); // binary
    bad_format.extend_from_slice(&0i16.to_be_bytes()); // param count
    bad_format.extend_from_slice(&0i16.to_be_bytes()); // result format code count
    send_length_prefixed_message(&mut stream, b'B', &bad_format);
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    send_sync(&mut stream);
    assert_ready_for_query_status(&mut stream, b'E');

    // Execute: 未登録 portal。
    send_length_prefixed_message(&mut stream, b'E', &execute_body("no-such-portal", 0));
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    send_sync(&mut stream);
    assert_ready_for_query_status(&mut stream, b'E');

    // ROLLBACK は引き続き受理される。
    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);

    // 対照: トランザクション外では従来どおり機能検証のエラーが返る。
    send_length_prefixed_message(&mut stream, b'P', &typed_parse);
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    send_sync(&mut stream);
    assert_ready_for_query_status(&mut stream, b'I');

    assert_eq!(visible_rows_with_id(&core, 45), 0);
    assert_eq!(visible_rows_with_id(&core, 46), 0);
}

/// `Failed` 中は複数文メッセージの分割エラーも `25P02` で拒否すること
/// （個々の文と同じ扱い。PR #1041 レビュー指摘）。
#[test]
fn split_error_while_failed_is_rejected_with_in_failed_sql_transaction() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    begin_and_insert_over_simple_query(&mut stream, 45, "op-942-45");
    send_simple_query(&mut stream, "BEGIN");
    expect_error_response_with_sqlstate(&mut stream, "25001");
    read_ready_for_query(&mut stream);

    let too_many = "SELECT id FROM documents LIMIT 1;"
        .repeat(engine::sql::statement_splitter::MAX_STATEMENTS_PER_QUERY + 1);
    send_simple_query(&mut stream, &too_many);
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
    assert_eq!(visible_rows_with_id(&core, 45), 0);
}

/// PR #1041 レビュー指摘（P1）の回帰: 簡易クエリプロトコル経由の
/// `ReadyForQuery` 状態バイトが、明示トランザクションの状態
/// （`Idle`/`Active`/`Failed`）に応じて `'I'`/`'T'`/`'E'` へ正しく写像される
/// こと（WIRE-19）。以前は `SessionTransaction` 導入後も常に `'I'` を固定
/// 送出しており、`BEGIN` 後もクライアントからトランザクションが終了した
/// ように見えていた。
#[test]
fn simple_query_ready_for_query_status_reflects_transaction_state() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    // Idle: 通常の autocommit 文の後は 'I'。
    send_simple_query(&mut stream, "SELECT id FROM documents LIMIT 1");
    let _ = read_message(&mut stream); // RowDescription
    let _ = read_message(&mut stream); // CommandComplete
    assert_ready_for_query_status(&mut stream, b'I');

    // Active: BEGIN 直後・トランザクション内の文の後は 'T'。
    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    assert_ready_for_query_status(&mut stream, b'T');

    send_simple_query(&mut stream, &insert_sql(46, "op-942-46"));
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    assert_ready_for_query_status(&mut stream, b'T');

    // Failed: トランザクション内でエラーになった文の後は 'E'。
    send_simple_query(&mut stream, "SELEC id FROM documents");
    expect_error_response_with_sqlstate(&mut stream, "42601");
    assert_ready_for_query_status(&mut stream, b'E');

    // ROLLBACK で Idle へ戻り、以降は 'I'。
    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_ready_for_query_status(&mut stream, b'I');
    assert_eq!(visible_rows_with_id(&core, 46), 0);
}

/// 上と同じ契約（WIRE-19）を拡張クエリプロトコルの Sync（'S'）経由で固定する
/// （PR #1041 レビュー指摘 P1。`extended_query::handle_sync` は以前
/// `SessionTransaction` の状態を一切参照せず常に `'I'` を送出していた）。
#[test]
fn extended_query_sync_ready_for_query_status_reflects_transaction_state() {
    let (core, _guard) = new_core_with_documents_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    // Active: BEGIN の Parse/Bind/Execute の後、Sync 時点でも 'T'。
    parse_and_bind(&mut stream, "begin47", "pbegin47", "BEGIN");
    assert_eq!(
        execute_and_read_command_complete(&mut stream, "pbegin47"),
        "BEGIN"
    );
    send_sync(&mut stream);
    assert_ready_for_query_status(&mut stream, b'T');

    // Failed: Bind エラーの後、Sync 時点で 'E'。
    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("bad47", "SELEC id FROM documents", 0),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'E', "expected ErrorResponse for malformed SQL");
    send_sync(&mut stream);
    assert_ready_for_query_status(&mut stream, b'E');

    // ROLLBACK で Idle へ戻り、Sync 時点で 'I'。
    parse_and_bind(&mut stream, "rb47", "prb47", "ROLLBACK");
    assert_eq!(
        execute_and_read_command_complete(&mut stream, "prb47"),
        "ROLLBACK"
    );
    send_sync(&mut stream);
    assert_ready_for_query_status(&mut stream, b'I');
}
