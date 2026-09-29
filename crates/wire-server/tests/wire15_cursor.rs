//! カーソル（`DECLARE`/`FETCH`/`CLOSE`。WIRE-15・TASK-218）の wire 経由結合
//! テスト。ポインタ: `docs/spec/05-tasks.md` TASK-218・
//! `docs/spec/04-behavior/wire-protocol.md` WIRE-15・
//! `docs/spec/04-behavior/error-format.md` ERR-6。
//!
//! `wire942_extended_transaction.rs`・`wire16_multi_statement.rs` と同じ流儀
//! （生バイトの wire クライアント＋in-process サーバー）で、簡易クエリ
//! プロトコル経由の `DECLARE`／`FETCH`／`CLOSE` の受理・エラー分類・RLS 分離を
//! 固定する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::*;

const TABLE: &str = "docs";

/// 次に届くメッセージの種別バイトを消費せずに覗き見る（TCP の `peek` を使う。
/// 後続の通常の読み取り〔`read_data_row`／`read_command_complete` 等〕が同じ
/// バイト列を改めて読み取れる）。`FETCH` の 1 ページに含まれる `DataRow` の
/// 件数は事前に分からない（末尾ページは要求件数未満・0 件になりうる）ため、
/// `DataRow`（`'D'`）と `CommandComplete`（`'C'`）のどちらが届いたかを見て
/// 分岐するために使う。
fn peek_message_type(stream: &mut std::net::TcpStream) -> u8 {
    let mut buf = [0u8; 1];
    loop {
        match stream.peek(&mut buf) {
            Ok(0) => panic!("connection closed while peeking message type"),
            Ok(_) => return buf[0],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) => panic!("peek failed: {e}"),
        }
    }
}

/// `docs` テーブル（`embedding VECTOR(3)` + `lang TEXT`）を持つ `EngineCore` を
/// 新設し、決定的な小規模コーパスを `tenant-a` に投入する。
fn new_core_with_rows() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    new_core_with_rows_and_write_lock_wait(None)
}

/// [`new_core_with_rows`] の書き込みゲート待機上限指定版（Issue #1178。他接続の
/// 書き込みが `55P03` へ倒れるまでの時間を短縮し、テストを決定的にする）。
fn new_core_with_rows_and_write_lock_wait(
    wait: Option<std::time::Duration>,
) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire15-cursor");
    let guard = temp_db::CleanupGuard(path.clone());
    let mut storage = Storage::open(&path).expect("open storage");
    if let Some(wait) = wait {
        storage = storage.with_write_lock_wait(wait);
    }
    storage
        .create_table(&TableSchema::new(
            TABLE,
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for id in 1..=5u64 {
        let op_id = engine::recovery::required_op_id::OperationId::parse(&format!("seed-{id}"))
            .expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            TABLE,
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![1.0, 0.0, 0.0]),
                Value::Text("ja".to_string()),
            ],
            &op_id,
        )
        .expect("insert row");
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

/// `BEGIN` → `DECLARE`（広域取得 `SELECT`）→ 複数回 `FETCH` → `CLOSE` →
/// `COMMIT` の一連が、pg 互換のタグ・`RowDescription`／`DataRow` で応答する
/// ことを固定する。
#[test]
fn wire15_declare_fetch_close_over_simple_query() {
    let (core, _guard) = new_core_with_rows();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id FROM docs LIMIT 100",
    );
    assert_eq!(read_command_complete(&mut stream), "DECLARE CURSOR");
    read_ready_for_query(&mut stream);

    let mut fetched_ids: Vec<String> = Vec::new();
    loop {
        send_simple_query(&mut stream, "FETCH 2 FROM c");
        let columns = read_row_description(&mut stream);
        assert_eq!(columns, vec!["id"]);
        let mut page_rows = Vec::new();
        // このページの行数はタグの数値部分から分かるが、先に `CommandComplete`
        // を読むと `DataRow` と混同するため、`DataRow` を上限 2 件まで読み、
        // 3 件目の読み取りを試みる代わりにタグを直接読む（`read_command_complete`
        // は内部で `C` メッセージのみを期待するため、`DataRow` が尽きた時点で
        // 呼び出す）。
        for _ in 0..2 {
            match peek_message_type(&mut stream) {
                b'D' => page_rows.push(read_data_row(&mut stream)),
                b'C' => break,
                other => panic!("unexpected message type: {other}"),
            }
        }
        let tag = read_command_complete(&mut stream);
        read_ready_for_query(&mut stream);
        assert_eq!(tag, format!("FETCH {}", page_rows.len()));
        if page_rows.is_empty() {
            break;
        }
        for row in page_rows {
            fetched_ids.push(row[0].clone().expect("id is not null"));
        }
    }
    fetched_ids.sort();
    assert_eq!(fetched_ids, vec!["1", "2", "3", "4", "5"]);

    send_simple_query(&mut stream, "CLOSE c");
    assert_eq!(read_command_complete(&mut stream), "CLOSE CURSOR");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "COMMIT");
    assert_eq!(read_command_complete(&mut stream), "COMMIT");
    read_ready_for_query(&mut stream);
}

/// トランザクション外の `DECLARE` は `25P01`、`FETCH`／`CLOSE` は `34000`。
/// エラー後も接続は維持される（簡易クエリのエラー契約）。
#[test]
fn wire15_cursor_statements_outside_transaction_are_rejected() {
    let (core, _guard) = new_core_with_rows();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id FROM docs LIMIT 1",
    );
    expect_error_response_with_sqlstate(&mut stream, "25P01");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "FETCH 1 FROM c");
    expect_error_response_with_sqlstate(&mut stream, "34000");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "CLOSE c");
    expect_error_response_with_sqlstate(&mut stream, "34000");
    read_ready_for_query(&mut stream);

    // 接続が維持されていることを確認する（簡易クエリの通常応答が返る）。
    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 1");
    let _ = read_row_description(&mut stream);
    let _ = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// PR #1049 レビュー指摘（P1）の wire 経由回帰: トランザクション外の
/// `DECLARE` は、内側 SELECT が存在しないテーブルを指していても `25P01` を
/// 返す（テーブル存在確認〔`UndefinedTable`〕がトランザクション状態の判定
/// より先に走ってはならない）。
#[test]
fn wire15_declare_outside_transaction_against_missing_table_is_25p01() {
    let (core, _guard) = new_core_with_rows();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id FROM missing_table LIMIT 1",
    );
    expect_error_response_with_sqlstate(&mut stream, "25P01");
    read_ready_for_query(&mut stream);
}

/// ベクトル順位付けの検索 `SELECT`（`ORDER BY <=>`）を `DECLARE` の内側として
/// 指定すると `42601`。`Active` なトランザクションは `Failed` へ遷移する。
#[test]
fn wire15_declare_rejects_vector_ranking_select_and_fails_transaction() {
    let (core, _guard) = new_core_with_rows();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "BEGIN");
    assert_eq!(read_command_complete(&mut stream), "BEGIN");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "DECLARE c CURSOR FOR SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0,0.0]' LIMIT 1",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    // `Failed` へ遷移しているため `ReadyForQuery` の状態バイトは `'E'`。
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(&mut stream, "FETCH 1 FROM c");
    expect_error_response_with_sqlstate(&mut stream, "25P02");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    read_ready_for_query(&mut stream);
}

/// RLS: 他テナントのカーソル名は不在と同一の応答（`34000`）になり、存在情報を
/// 漏らさない。
#[test]
fn wire15_cursor_names_do_not_leak_across_tenants() {
    let (core, _guard) = new_core_with_rows();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let addr = spawn_server_with_engine(&users_path, Arc::clone(&core));

    let mut alice = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(&mut alice, "BEGIN");
    assert_eq!(read_command_complete(&mut alice), "BEGIN");
    read_ready_for_query(&mut alice);
    send_simple_query(
        &mut alice,
        "DECLARE c CURSOR FOR SELECT id FROM docs LIMIT 10",
    );
    assert_eq!(read_command_complete(&mut alice), "DECLARE CURSOR");
    read_ready_for_query(&mut alice);

    let mut bob = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    send_simple_query(&mut bob, "BEGIN");
    assert_eq!(read_command_complete(&mut bob), "BEGIN");
    read_ready_for_query(&mut bob);
    send_simple_query(&mut bob, "FETCH 1 FROM c");
    expect_error_response_with_sqlstate(&mut bob, "34000");
    read_ready_for_query(&mut bob);
    send_simple_query(&mut bob, "ROLLBACK");
    assert_eq!(read_command_complete(&mut bob), "ROLLBACK");
    read_ready_for_query(&mut bob);
}

// ---------------------------------------------------------------------------
// Issue #1178: FETCH 件数の範囲境界（`MAX_SEARCH_K` = 10 000）と、他セッションの
// 書き込みに対する行集合の不変（TABLE-3）。
// ---------------------------------------------------------------------------

fn simple_ok(stream: &mut std::net::TcpStream, sql: &str, expected_tag: &str) {
    send_simple_query(stream, sql);
    assert_eq!(read_command_complete(stream), expected_tag);
    read_ready_for_query(stream);
}

/// `BEGIN` → `DECLARE c`（全行対象）まで進める。
fn begin_and_declare(stream: &mut std::net::TcpStream) {
    simple_ok(stream, "BEGIN", "BEGIN");
    simple_ok(
        stream,
        "DECLARE c CURSOR FOR SELECT id FROM docs LIMIT 100",
        "DECLARE CURSOR",
    );
}

/// `FETCH` の 1 ページ分の行を読み、`(行, タグ)` を返す（`RowDescription` から
/// `CommandComplete`、`ReadyForQuery` まで消費する）。
fn read_fetch_page(stream: &mut std::net::TcpStream) -> (Vec<String>, String) {
    let _ = read_row_description(stream);
    let mut ids = Vec::new();
    while peek_message_type(stream) == b'D' {
        let row = read_data_row(stream);
        ids.push(row[0].clone().expect("id is not null"));
    }
    let tag = read_command_complete(stream);
    let status = read_ready_for_query_status(stream);
    assert_eq!(status, b'T', "transaction must remain Active after FETCH");
    (ids, tag)
}

/// `FETCH n` が範囲外（`22000`）で失敗し、先に行が送られないこと、および
/// トランザクションが `Failed`（`'E'`）へ遷移することを確認して `ROLLBACK` する。
fn assert_fetch_rejected_with_22000(fetch_sql: &str) {
    let (core, _guard) = new_core_with_rows();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    begin_and_declare(&mut stream);

    send_simple_query(&mut stream, fetch_sql);
    // RowDescription／DataRow が先に届かず、最初のメッセージが ErrorResponse。
    assert_eq!(peek_message_type(&mut stream), b'E');
    expect_error_response_with_sqlstate(&mut stream, "22000");
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    send_simple_query(&mut stream, "ROLLBACK");
    assert_eq!(read_command_complete(&mut stream), "ROLLBACK");
    assert_eq!(read_ready_for_query_status(&mut stream), b'I');
}

/// 上限ちょうど（10 000）は受理され、10 001 は `22000`。公開 API の
/// `validate_search_limit` を定数ずれのオラクルとして併用する。
#[test]
fn wire15_fetch_count_boundary_matches_max_search_k() {
    assert!(engine::sql::parser::validate_search_limit(10_000).is_ok());
    let err = engine::sql::parser::validate_search_limit(10_001).expect_err("over limit");
    assert_eq!(err.wire_code(), "22000");

    let (core, _guard) = new_core_with_rows();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    begin_and_declare(&mut stream);

    send_simple_query(&mut stream, "FETCH 10000 FROM c");
    let (ids, tag) = read_fetch_page(&mut stream);
    assert_eq!(ids.len(), 5);
    assert_eq!(tag, "FETCH 5");

    simple_ok(&mut stream, "COMMIT", "COMMIT");
}

#[test]
fn wire15_fetch_count_over_max_search_k_is_22000_and_fails_transaction() {
    assert_fetch_rejected_with_22000("FETCH 10001 FROM c");
}

/// `u32` の最大値でも範囲検証（`22000`）に到達する。
#[test]
fn wire15_fetch_count_u32_max_is_22000() {
    assert_fetch_rejected_with_22000("FETCH 4294967295 FROM c");
}

/// `u32` を超える値の分類は `SELECT ... LIMIT` と同一（SQL-15 との一致）。
/// 具体的な SQLSTATE は固定せず、両者が一致することだけを固定する。
#[test]
fn wire15_fetch_count_beyond_u32_matches_select_limit_parity() {
    fn sqlstate_of_error(stream: &mut std::net::TcpStream) -> String {
        let (ty, body) = read_message(stream);
        assert_eq!(ty, b'E', "expected ErrorResponse");
        // ErrorResponse は `<field type byte><cstring>` の列。`C` が SQLSTATE。
        let mut rest: &[u8] = &body;
        while let Some((&field, tail)) = rest.split_first() {
            if field == 0 {
                break;
            }
            let end = tail.iter().position(|&b| b == 0).expect("nul-terminated");
            let (value, next) = tail.split_at(end);
            if field == b'C' {
                return String::from_utf8_lossy(value).into_owned();
            }
            rest = &next[1..];
        }
        panic!("SQLSTATE field missing");
    }

    let (core, _guard) = new_core_with_rows();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 4294967296");
    let select_code = sqlstate_of_error(&mut stream);
    read_ready_for_query(&mut stream);

    begin_and_declare(&mut stream);
    send_simple_query(&mut stream, "FETCH 4294967296 FROM c");
    let fetch_code = sqlstate_of_error(&mut stream);
    assert_eq!(read_ready_for_query_status(&mut stream), b'E');

    assert_eq!(fetch_code, select_code);
}

/// 拡張クエリの Parse 時点でも同じ範囲検証が働く（カーソル不要）。
#[test]
fn wire15_fetch_count_over_max_search_k_via_extended_parse_is_22000() {
    let (core, _guard) = new_core_with_rows();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_length_prefixed_message(&mut stream, b'P', &parse_body("", "FETCH 10001 FROM c", 0));
    expect_error_response_with_sqlstate(&mut stream, "22000");
    send_length_prefixed_message(&mut stream, b'S', b"");
    let (ty, _) = read_message(&mut stream);
    assert_eq!(ty, b'Z', "ReadyForQuery expected after Sync");
}

fn insert_row_sql(id: u64, op: &str) -> String {
    format!(
        "INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[0.1,0.2,0.3]', 'ja') USING OPERATION_ID '{op}'"
    )
}

/// 他セッション（同一テナント・別テナントとも）の書き込みは、明示トランザクションが
/// 単一ライタの permit を保持している間は `55P03` になり、カーソルの行集合は
/// 変わらない。`COMMIT` 後の新しいスナップショットで初めて書き込みが見える。
#[test]
fn wire15_cursor_row_set_is_unaffected_by_other_session_write_during_fetch() {
    let (core, _guard) =
        new_core_with_rows_and_write_lock_wait(Some(std::time::Duration::from_millis(300)));
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "correct-horse"),
        ("alice2", "tenant-a", "correct-horse"),
        ("bob", "tenant-b", "battery-staple"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);

    let mut a = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    begin_and_declare(&mut a);
    send_simple_query(&mut a, "FETCH 2 FROM c");
    let (mut seen, tag) = read_fetch_page(&mut a);
    assert_eq!(tag, "FETCH 2");

    // 同一テナント・別テナントの他接続の書き込みはライタ待機上限で `55P03`。
    let mut b = authenticate_to_ready_for_query(addr, "alice2", "correct-horse");
    let mut c = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    for (stream, id, op) in [(&mut b, 99u64, "op-1178-b"), (&mut c, 98u64, "op-1178-c")] {
        send_simple_query(stream, &insert_row_sql(id, op));
        let (ty, body) = read_message(stream);
        assert_eq!(ty, b'E');
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("55P03"), "expected 55P03, got: {text:?}");
        // 他テナント ID・行 ID を応答へ含めない。
        assert!(!text.contains("tenant-"), "tenant id leaked: {text:?}");
        assert!(!text.contains(&id.to_string()), "row id leaked: {text:?}");
        read_ready_for_query(stream);
    }

    // 残りのカーソル行は元の集合の残りだけ（他セッションの行を含まない）。
    send_simple_query(&mut a, "FETCH 100 FROM c");
    let (rest, tag) = read_fetch_page(&mut a);
    assert_eq!(tag, "FETCH 3");
    seen.extend(rest);
    seen.sort();
    assert_eq!(seen, vec!["1", "2", "3", "4", "5"]);

    simple_ok(&mut a, "COMMIT", "COMMIT");

    // ライタ解放後は他接続の書き込みが通る。
    simple_ok(&mut b, &insert_row_sql(99, "op-1178-b2"), "INSERT 0 1");

    // 新しい明示トランザクションのスナップショットでは commit 済みの行が見える。
    begin_and_declare(&mut a);
    send_simple_query(&mut a, "FETCH 100 FROM c");
    let (mut all, tag) = read_fetch_page(&mut a);
    assert_eq!(tag, "FETCH 6");
    all.sort();
    assert_eq!(all, vec!["1", "2", "3", "4", "5", "99"]);
    simple_ok(&mut a, "COMMIT", "COMMIT");
}
