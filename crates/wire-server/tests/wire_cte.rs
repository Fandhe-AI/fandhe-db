//! 非再帰 `WITH` 句（CTE。SQL-29 (b)・RLS-10 (b)、TASK-213、Issue #928）の
//! 簡易クエリプロトコル経由（生バイトクライアント）検証（層 A）。
//!
//! CTE の意味論そのもの（許可リスト・上限・RLS 暗黙適用・名前解決）は
//! `crates/engine/tests/sql29_cte.rs` が確定オラクルとして検証済みのため、
//! 本ファイルは `wire_create_view.rs` と同じ流儀で以下に徹する:
//! - `RowDescription` が CTE の公開列だけを含むこと
//! - 拒否形ごとの `ErrorResponse` SQLSTATE（`42601`・`54000`・`42P01`）
//! - 複数文メッセージ（`WITH ...; SELECT ...`）が受理されること
//! - 2 接続（別テナント）での CTE 経由読み取りの RLS 暗黙適用

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;
use wire_server::auth::UserStore;
use wire_server::limits::ConnectionLimiter;

use common::*;

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-cte-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn_server_with_engine_and_store(
    store: UserStore,
    engine: Arc<EngineCore>,
) -> std::net::SocketAddr {
    let store = Arc::new(store);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);

    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            engine,
            limiter,
            Duration::from_secs(5),
        );
    });

    addr
}

/// 単純な `WITH` クエリが受理され、`RowDescription` は CTE の公開列
/// （`id` のみ）だけを含む。
#[test]
fn wire_cte_simple_query_returns_only_exposed_columns() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja') USING OPERATION_ID 'op-1'",
    );
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "WITH ja AS (SELECT id FROM docs WHERE lang = 'ja') SELECT id FROM ja LIMIT 100",
    );
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id".to_string()]);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![Some("1".to_string())]);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// 拒否形ごとの SQLSTATE が `ErrorResponse` に現れ、接続は使用可能なまま維持
/// される。
#[test]
fn wire_cte_rejected_forms_report_expected_sqlstate_and_keep_connection_usable() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    let cases: &[(&str, &str)] = &[
        // `UNION`（重複除去）の再帰は #1872 まで機能ゲートの `0A000`。
        (
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r",
            "0A000",
        ),
        // SQL-35・Issue #1869: 不正な再帰（アンカー項に自己参照）は `42P19`。
        (
            "WITH RECURSIVE r(n) AS (SELECT n FROM r UNION ALL SELECT id FROM docs) SELECT n FROM r",
            "42P19",
        ),
        // 反復上限（暫定 1000 回）を超える再帰は部分結果を返さず `54000`（SQL-35・Issue #1870）。
        (
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r) SELECT n FROM r",
            "54000",
        ),
        (
            "WITH x AS (SELECT id FROM ghost) SELECT id FROM x LIMIT 10",
            "42P01",
        ),
    ];
    for (sql, expected) in cases {
        send_simple_query(&mut stream, sql);
        expect_error_response_with_sqlstate(&mut stream, expected);
        read_ready_for_query(&mut stream);
    }

    // `54000`（定義数上限超過）: 接続がまだ使えることの対照確認も兼ねる。
    let defs: Vec<String> = (0..=16)
        .map(|i| format!("c{i} AS (SELECT id FROM docs)"))
        .collect();
    let over_limit_sql = format!("WITH {} SELECT id FROM c0 LIMIT 10", defs.join(", "));
    send_simple_query(&mut stream, &over_limit_sql);
    expect_error_response_with_sqlstate(&mut stream, "54000");
    read_ready_for_query(&mut stream);

    // 接続はまだ使用可能。
    send_simple_query(&mut stream, "SELECT COUNT(*) FROM docs");
    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// 複数文メッセージ（`WITH ...; SELECT ...`）が受理される。
#[test]
fn wire_cte_multi_statement_message_is_accepted() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "WITH x AS (SELECT id FROM docs) SELECT id FROM x LIMIT 10; SELECT COUNT(*) FROM docs",
    );
    let _columns = read_row_description(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 0");
    let _columns = read_row_description(&mut stream);
    let _row = read_data_row(&mut stream);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut stream);
}

/// CTE 経由の読み取りには、参照した**接続自身**の `PolicyContext` で RLS が
/// 暗黙適用される（作成者〔alice〕の可視性は参照者〔bob〕へ引き継がれない。
/// RLS-10 (b)）。
#[test]
fn wire_cte_read_applies_reader_session_rls() {
    let (core, _guard) = new_core_with_docs_table();
    let store = {
        let users_path = write_user_store_file(&[
            ("alice", "tenant-a", "correct-horse"),
            ("bob", "tenant-b", "battery-staple"),
        ]);
        UserStore::load_from_file(&users_path).expect("valid user store")
    };
    let addr = spawn_server_with_engine_and_store(store, core);

    let mut alice_stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(
        &mut alice_stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja') USING OPERATION_ID 'op-alice-1'",
    );
    let _tag = read_command_complete(&mut alice_stream);
    read_ready_for_query(&mut alice_stream);

    let mut bob_stream = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    send_simple_query(
        &mut bob_stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (2, '[0.4,0.5,0.6]', 'ja') USING OPERATION_ID 'op-bob-1'",
    );
    let _tag = read_command_complete(&mut bob_stream);
    read_ready_for_query(&mut bob_stream);

    // bob は CTE 経由でも自分の行のみを見る（alice の行は不可視）。
    send_simple_query(
        &mut bob_stream,
        "WITH ja AS (SELECT id FROM docs WHERE lang = 'ja') SELECT id FROM ja LIMIT 100",
    );
    let _columns = read_row_description(&mut bob_stream);
    let row = read_data_row(&mut bob_stream);
    assert_eq!(row, vec![Some("2".to_string())]);
    let tag = read_command_complete(&mut bob_stream);
    assert_eq!(tag, "SELECT 1");
    read_ready_for_query(&mut bob_stream);
}

const RECURSIVE_SEQUENCE: &str =
    "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT n FROM r";

/// SQL-35・Issue #1870: 再帰 CTE（数列）の簡易クエリ。反復評価の結果が行として返る。
#[test]
fn wire_recursive_cte_simple_query_returns_sequence() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, RECURSIVE_SEQUENCE);
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["n".to_string()]);
    for expected in ["1", "2", "3"] {
        let row = read_data_row(&mut stream);
        assert_eq!(row, vec![Some(expected.to_string())]);
    }
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// SQL-35・Issue #1870: 拡張クエリ（Parse／Bind／Describe／Execute）でも再帰 CTE が評価され、
/// Describe の列は Execute の `RowDescription` と一致する。
#[test]
fn wire_recursive_cte_extended_query_describes_and_executes() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_length_prefixed_message(&mut stream, b'P', &parse_body("s1", RECURSIVE_SEQUENCE, 0));
    let (kind, body) = read_message(&mut stream);
    assert_eq!(
        kind,
        b'1',
        "ParseComplete: {}",
        String::from_utf8_lossy(&body)
    );
    send_length_prefixed_message(&mut stream, b'B', &bind_body("p1", "s1"));
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'2', "BindComplete");
    let mut describe = vec![b'P'];
    describe.extend_from_slice(b"p1\0");
    send_length_prefixed_message(&mut stream, b'D', &describe);
    let (kind, described) = read_message(&mut stream);
    assert_eq!(kind, b'T', "RowDescription from Describe");
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p1", 0));
    let mut data_rows = 0;
    loop {
        let (kind, body) = read_message(&mut stream);
        match kind {
            b'D' => data_rows += 1,
            b'C' => {
                assert_eq!(body, b"SELECT 3\0");
                break;
            }
            other => panic!("unexpected message {other}"),
        }
    }
    assert_eq!(data_rows, 3);
    send_length_prefixed_message(&mut stream, b'S', b"");
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'Z');

    // 簡易クエリの RowDescription と、Describe が返した RowDescription は一致する。
    send_simple_query(&mut stream, RECURSIVE_SEQUENCE);
    let mut simple_description = Vec::new();
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'T');
    simple_description.extend_from_slice(&body);
    assert_eq!(simple_description, described);
    loop {
        let (kind, _) = read_message(&mut stream);
        if kind == b'Z' {
            break;
        }
    }
}
// ---- SEARCH DEPTH／BREADTH FIRST 句（SQL-35・Issue #2213）----

const RECURSIVE_SEARCH_DEPTH: &str = "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL \
     SELECT n + 1 FROM r WHERE n < 3) SEARCH DEPTH FIRST BY n SET o \
     SELECT n, o FROM r ORDER BY o";
const RECURSIVE_SEARCH_BREADTH: &str = "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL \
     SELECT n + 1 FROM r WHERE n < 3) SEARCH BREADTH FIRST BY n SET o \
     SELECT n, o FROM r ORDER BY o";

/// 順序列の型 OID は深さ優先が `record[]`（2287）・幅優先が `record`（2249）で、値は PostgreSQL の
/// `array_out`／`record_out` 形式のテキスト。
#[test]
fn wire_recursive_search_advertises_record_oids_and_text_values() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    for (sql, oid, expected) in [
        (
            RECURSIVE_SEARCH_DEPTH,
            2287,
            ["{(1)}", "{(1),(2)}", "{(1),(2),(3)}"],
        ),
        (RECURSIVE_SEARCH_BREADTH, 2249, ["(0,1)", "(1,2)", "(2,3)"]),
    ] {
        send_simple_query(&mut stream, sql);
        let columns = read_row_description_with_types(&mut stream);
        assert_eq!(columns.len(), 2, "{sql}");
        assert_eq!(columns[1], ("o".to_string(), oid, -1), "{sql}");
        for (i, want) in expected.iter().enumerate() {
            let row = read_data_row(&mut stream);
            assert_eq!(row[0], Some((i + 1).to_string()), "{sql}");
            assert_eq!(row[1].as_deref(), Some(*want), "{sql}");
        }
        assert_eq!(read_command_complete(&mut stream), "SELECT 3");
        read_ready_for_query(&mut stream);
    }
}

/// 順序列のバイナリ形式（`record_send`）は未対応: 結果列へ binary を要求すると `0A000` で当該文だけ
/// 拒否され、接続は維持される。
#[test]
fn wire_recursive_search_binary_result_format_is_0a000() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("s1", RECURSIVE_SEARCH_DEPTH, 0),
    );
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'1', "ParseComplete");
    // 全結果列を binary（format code 1）で要求する Bind。
    let mut bind = Vec::new();
    bind.extend_from_slice(b"p1\0s1\0");
    bind.extend_from_slice(&0i16.to_be_bytes()); // param format codes
    bind.extend_from_slice(&0i16.to_be_bytes()); // params
    bind.extend_from_slice(&1i16.to_be_bytes()); // result format code count
    bind.extend_from_slice(&1i16.to_be_bytes()); // binary
    send_length_prefixed_message(&mut stream, b'B', &bind);
    let (kind, body) = read_message(&mut stream);
    assert_eq!(kind, b'E', "ErrorResponse");
    assert!(
        String::from_utf8_lossy(&body).contains("0A000"),
        "{}",
        String::from_utf8_lossy(&body)
    );
    send_length_prefixed_message(&mut stream, b'S', b"");
    let (kind, _) = read_message(&mut stream);
    assert_eq!(kind, b'Z');

    // 回復後、同じ接続でテキスト形式は通る。
    send_simple_query(&mut stream, RECURSIVE_SEARCH_DEPTH);
    let _ = read_row_description(&mut stream);
    for _ in 0..3 {
        let _ = read_data_row(&mut stream);
    }
    assert_eq!(read_command_complete(&mut stream), "SELECT 3");
    read_ready_for_query(&mut stream);
}

// ---- データ変更 CTE の実行（SQL-63・Issue #1876）----

fn insert_doc(stream: &mut std::net::TcpStream, id: u32, lang: &str) {
    send_simple_query(
        stream,
        &format!(
            "INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[0.1,0.2,0.3]', '{lang}') \
             USING OPERATION_ID 'op-seed-{id}'"
        ),
    );
    let _tag = read_command_complete(stream);
    read_ready_for_query(stream);
}

fn count_docs(stream: &mut std::net::TcpStream) -> String {
    send_simple_query(stream, "SELECT COUNT(*) FROM docs");
    let _columns = read_row_description(stream);
    let row = read_data_row(stream);
    let _tag = read_command_complete(stream);
    read_ready_for_query(stream);
    row.into_iter().next().flatten().expect("count cell")
}

/// Sync まで読み進め、受け取ったメッセージ種別を順に返す。
fn read_until_ready(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut types = Vec::new();
    loop {
        let (t, _body) = read_message(stream);
        types.push(t);
        if t == b'Z' {
            return types;
        }
    }
}

/// 簡易クエリ: 書き込みと SELECT が 1 文で成立し、`RowDescription`・`DataRow`・`SELECT n` が返る。
#[test]
fn wire_data_modifying_cte_simple_query_writes_and_returns_rows() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    insert_doc(&mut stream, 1, "ja");
    insert_doc(&mut stream, 2, "en");

    let sql = "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id, lang) \
               SELECT id FROM d LIMIT 10 USING OPERATION_ID 'op-dm-1'";
    send_simple_query(&mut stream, sql);
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id".to_string()]);
    assert_eq!(read_data_row(&mut stream), vec![Some("1".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
    assert_eq!(count_docs(&mut stream), "1");

    // 同一文の再送は 23505 で、再実行されない。
    send_simple_query(&mut stream, sql);
    expect_error_response_with_sqlstate(&mut stream, "23505");
    read_ready_for_query(&mut stream);
    assert_eq!(count_docs(&mut stream), "1");
}

/// 複数文メッセージ（暗黙トランザクション）: 先行文の書き込みを後続文が読み、未対応の形は
/// 固定 `0A000` で何も変えない。
#[test]
fn wire_data_modifying_cte_multi_statement_message_is_one_implicit_transaction() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    insert_doc(&mut stream, 1, "ja");

    send_simple_query(
        &mut stream,
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT id FROM d LIMIT 10; \
         SELECT COUNT(*) FROM docs",
    );
    let _columns = read_row_description(&mut stream);
    assert_eq!(read_data_row(&mut stream), vec![Some("1".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    let _columns = read_row_description(&mut stream);
    assert_eq!(read_data_row(&mut stream), vec![Some("0".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    insert_doc(&mut stream, 2, "ja");
    // 副文を読まない形（RETURNING なしの副文を参照）は未対応。表は変わらない。
    send_simple_query(
        &mut stream,
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja') SELECT id FROM d LIMIT 10",
    );
    expect_error_response_with_sqlstate(&mut stream, "0A000");
    read_ready_for_query(&mut stream);
    assert_eq!(count_docs(&mut stream), "1");
}

/// 明示トランザクション内の実行は ROLLBACK で全副作用が消える。
#[test]
fn wire_data_modifying_cte_in_explicit_transaction_rolls_back() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    insert_doc(&mut stream, 1, "ja");

    send_simple_query(&mut stream, "BEGIN");
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
    send_simple_query(
        &mut stream,
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT id FROM d LIMIT 10",
    );
    let _columns = read_row_description(&mut stream);
    assert_eq!(read_data_row(&mut stream), vec![Some("1".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
    send_simple_query(&mut stream, "ROLLBACK");
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
    assert_eq!(count_docs(&mut stream), "1");
}

/// 拡張クエリ: Describe（portal）は書き込みを起こさず、Execute で初めて書き込む。
#[test]
fn wire_data_modifying_cte_extended_query_describe_does_not_write() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    insert_doc(&mut stream, 1, "ja");

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body(
            "s1",
            "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) \
             SELECT id FROM d LIMIT 10",
            0,
        ),
    );
    send_length_prefixed_message(&mut stream, b'B', &bind_body("p1", "s1"));
    send_length_prefixed_message(&mut stream, b'D', b"Pp1\0");
    send_length_prefixed_message(&mut stream, b'S', &[]);
    assert_eq!(read_until_ready(&mut stream), vec![b'1', b'2', b'T', b'Z']);
    assert_eq!(count_docs(&mut stream), "1", "Describe must not write");

    // portal は Sync（暗黙トランザクションの終端）で破棄されるため、準備済みの文から Bind し直す。
    send_length_prefixed_message(&mut stream, b'B', &bind_body("p2", "s1"));
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p2", 0));
    send_length_prefixed_message(&mut stream, b'S', &[]);
    assert_eq!(read_until_ready(&mut stream), vec![b'2', b'D', b'C', b'Z']);
    assert_eq!(count_docs(&mut stream), "0");
}

/// 副文が複数ある文と、主クエリが実表を読む文を簡易クエリで実行できる（Issue #1877）。全副文と主クエリは
/// 文の開始時点の状態を読み、書き込みは同じ文で成立する。
#[test]
fn wire_data_modifying_cte_several_substatements_and_table_main_query() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    insert_doc(&mut stream, 1, "ja");
    insert_doc(&mut stream, 2, "en");

    // 2 つの副文。b は a の削除を見ず、en の行だけを更新する。
    send_simple_query(
        &mut stream,
        "WITH a AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id), \
         b AS (UPDATE docs SET lang = 'ja' WHERE lang = 'en' RETURNING id) \
         SELECT id FROM b LIMIT 10",
    );
    let _columns = read_row_description(&mut stream);
    assert_eq!(read_data_row(&mut stream), vec![Some("2".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
    assert_eq!(count_docs(&mut stream), "1");

    // 主クエリが実表を読む形は、削除前の状態を返す。
    send_simple_query(
        &mut stream,
        "WITH d AS (DELETE FROM docs WHERE lang = 'ja' RETURNING id) SELECT id FROM docs LIMIT 10",
    );
    let _columns = read_row_description(&mut stream);
    assert_eq!(read_data_row(&mut stream), vec![Some("2".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);
    assert_eq!(count_docs(&mut stream), "0");
}

/// 拡張クエリ: 主クエリが実表を読む形の Describe は書き込みを起こさない（Issue #1877）。
#[test]
fn wire_data_modifying_cte_table_main_query_describe_does_not_write() {
    let (core, _guard) = new_core_with_docs_table();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    let addr = spawn_server_with_engine_and_store(store, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    insert_doc(&mut stream, 1, "ja");

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body(
            "s2",
            "WITH a AS (DELETE FROM docs WHERE lang = 'ja'), \
             b AS (DELETE FROM docs WHERE id = 99) SELECT id FROM docs LIMIT 10",
            0,
        ),
    );
    send_length_prefixed_message(&mut stream, b'B', &bind_body("p1", "s2"));
    send_length_prefixed_message(&mut stream, b'D', b"Pp1\0");
    send_length_prefixed_message(&mut stream, b'S', &[]);
    assert_eq!(read_until_ready(&mut stream), vec![b'1', b'2', b'T', b'Z']);
    assert_eq!(count_docs(&mut stream), "1", "Describe must not write");

    send_length_prefixed_message(&mut stream, b'B', &bind_body("p2", "s2"));
    send_length_prefixed_message(&mut stream, b'E', &execute_body("p2", 0));
    send_length_prefixed_message(&mut stream, b'S', &[]);
    assert_eq!(read_until_ready(&mut stream), vec![b'2', b'D', b'C', b'Z']);
    assert_eq!(count_docs(&mut stream), "0");
}
