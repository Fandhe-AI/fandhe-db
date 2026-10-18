//! `CREATE MATERIALIZED VIEW`（TABLE-42・Issue #1664）の簡易クエリ・拡張クエリプロトコル経由
//! （生バイトクライアント）の検証（層 A）。
//!
//! 意味論そのもの（構文・初回実体化・行数上限・テナント境界・列型の確定）は
//! `crates/engine/tests/table42_create_materialized_view.rs` が確定オラクルとして検証済みのため、
//! 本ファイルは `wire_create_view.rs` と同じ流儀で次に徹する。
//! - 実体化対象の文脈が認証構成（`UserStore::materialization_contexts`。`main.rs` が起動時に
//!   1 回注入するのと同じ導出）から作られ、**作成していない別テナント**の接続でも自分の文脈の
//!   実体を読めること
//! - `CommandComplete` タグ（`WITH DATA` は `SELECT <n>`、`n` は作成者自身の文脈の行数だけ。
//!   `WITH NO DATA` は `CREATE MATERIALIZED VIEW`）
//! - `55000`（未実体化）・`42809`（書き込み）の `ErrorResponse` SQLSTATE
//! - 拡張クエリ（Parse／Bind／Describe／Execute）でも同じ列メタと結果になること

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

fn allowed_store(users: &[(&str, &str, &str)], allowed: &[&str]) -> UserStore {
    let users_path = write_user_store_file(users);
    let store = UserStore::load_from_file(&users_path).expect("valid user store");
    store
        .with_ddl_allowed_users(&allowed.iter().map(|s| s.to_string()).collect::<Vec<_>>())
        .expect("allowed usernames must be known")
}

/// 3 テナントのユーザーストアから、`main.rs` と同じ導出で実体化対象文脈を注入した core を作る。
fn spawn(label: &str) -> (std::net::SocketAddr, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path(label);
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
    let store = allowed_store(
        &[
            ("alice", "tenant-a", "correct-horse"),
            ("bob", "tenant-b", "battery-staple"),
            ("carol", "tenant-c", "staple-battery"),
        ],
        &["alice"],
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_materialization_contexts(store.materialization_contexts().expect("contexts"));
    let core = Arc::new(core);
    let store = Arc::new(store);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let limiter = ConnectionLimiter::new(16);
    std::thread::spawn(move || {
        wire_server::server::accept_loop_with_engine(
            listener,
            store,
            core,
            limiter,
            Duration::from_secs(5),
        );
    });
    (addr, guard)
}

fn insert(stream: &mut std::net::TcpStream, id: u64, lang: &str) {
    send_simple_query(
        stream,
        &format!(
            "INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[0.1,0.2,0.3]', '{lang}') USING OPERATION_ID 'op-mv-{id}'"
        ),
    );
    let _tag = read_command_complete(stream);
    read_ready_for_query(stream);
}

fn read_ids(stream: &mut std::net::TcpStream, sql: &str) -> (Vec<String>, String) {
    send_simple_query(stream, sql);
    let _columns = read_row_description(stream);
    let mut ids = Vec::new();
    loop {
        let (kind, body) = read_message(stream);
        match kind {
            b'D' => {
                // DataRow: i16 列数, 以降 (i32 長さ, 本体) の列。1 列目だけ読む。
                let len = i32::from_be_bytes(body[2..6].try_into().expect("len"));
                let value = &body[6..6 + len as usize];
                ids.push(String::from_utf8(value.to_vec()).expect("utf8"));
            }
            b'C' => {
                let tag = String::from_utf8(body[..body.len() - 1].to_vec()).expect("tag");
                read_ready_for_query(stream);
                return (ids, tag);
            }
            other => panic!("unexpected message {other}"),
        }
    }
}

/// 許可ユーザーが作成すると、タグは作成者自身の文脈の行数だけを運び、作成していない別テナントの
/// 接続でも自分の文脈の実体を読める（他テナントの行は混ざらない）。
#[test]
fn wire_with_data_tag_counts_only_the_creators_context_and_other_tenants_read_their_own() {
    let (addr, _guard) = spawn("wire-mv-with-data");
    let mut alice = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut bob = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    insert(&mut alice, 1, "ja");
    insert(&mut alice, 2, "en");
    insert(&mut bob, 3, "ja");

    send_simple_query(
        &mut alice,
        "CREATE MATERIALIZED VIEW mv AS SELECT id FROM docs",
    );
    // alice の文脈で見える行は自分の 2 行だけ。bob の 1 行は合算されない。
    assert_eq!(read_command_complete(&mut alice), "SELECT 2");
    read_ready_for_query(&mut alice);

    let (alice_ids, tag) = read_ids(&mut alice, "SELECT id FROM mv LIMIT 100");
    assert_eq!(
        (alice_ids, tag.as_str()),
        (vec!["1".into(), "2".into()], "SELECT 2")
    );
    // bob は作成していないが、認証構成から導出された文脈の実体を読める。
    let (bob_ids, tag) = read_ids(&mut bob, "SELECT id FROM mv LIMIT 100");
    assert_eq!((bob_ids, tag.as_str()), (vec!["3".into()], "SELECT 1"));
    // carol は行を持たないが実体化対象で、空の結果が読める（`55000` ではない）。
    let mut carol = authenticate_to_ready_for_query(addr, "carol", "staple-battery");
    let (carol_ids, tag) = read_ids(&mut carol, "SELECT id FROM mv LIMIT 100");
    assert_eq!(
        (carol_ids, tag.as_str()),
        (Vec::<String>::new(), "SELECT 0")
    );
}

/// `WITH NO DATA` は定義だけを作り（タグ `CREATE MATERIALIZED VIEW`）、読み取りは `55000`。
/// 書き込みは `42809`。DDL 権限の無いユーザーは `42501`。
#[test]
fn wire_no_data_write_and_permission_errors() {
    let (addr, _guard) = spawn("wire-mv-errors");
    let mut bob = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    send_simple_query(
        &mut bob,
        "CREATE MATERIALIZED VIEW mv AS SELECT id FROM docs",
    );
    expect_error_response_with_sqlstate(&mut bob, "42501");
    read_ready_for_query(&mut bob);

    let mut alice = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    send_simple_query(
        &mut alice,
        "CREATE MATERIALIZED VIEW mv AS SELECT id FROM docs WITH NO DATA",
    );
    assert_eq!(
        read_command_complete(&mut alice),
        "CREATE MATERIALIZED VIEW"
    );
    read_ready_for_query(&mut alice);

    for stream in [&mut alice, &mut bob] {
        send_simple_query(stream, "SELECT id FROM mv LIMIT 10");
        expect_error_response_with_sqlstate(stream, "55000");
        read_ready_for_query(stream);
    }

    send_simple_query(
        &mut alice,
        "INSERT INTO mv (id) VALUES (9) USING OPERATION_ID 'op-mv-w'",
    );
    expect_error_response_with_sqlstate(&mut alice, "42809");
    read_ready_for_query(&mut alice);

    // `IF NOT EXISTS` の no-op は固定タグ。
    send_simple_query(
        &mut alice,
        "CREATE MATERIALIZED VIEW IF NOT EXISTS mv AS SELECT id FROM docs",
    );
    assert_eq!(
        read_command_complete(&mut alice),
        "CREATE MATERIALIZED VIEW"
    );
    read_ready_for_query(&mut alice);
}

/// 拡張クエリ（Parse／Bind／Describe portal／Execute／Sync）でも、matview の読み取りが
/// 簡易クエリと同じ列メタ・行を返す。
#[test]
fn wire_extended_query_reads_a_materialized_view() {
    let (addr, _guard) = spawn("wire-mv-extended");
    let mut alice = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    insert(&mut alice, 1, "ja");
    send_simple_query(
        &mut alice,
        "CREATE MATERIALIZED VIEW mv AS SELECT id, lang FROM docs",
    );
    assert_eq!(read_command_complete(&mut alice), "SELECT 1");
    read_ready_for_query(&mut alice);

    send_length_prefixed_message(
        &mut alice,
        b'P',
        &parse_body("", "SELECT id, lang FROM mv LIMIT 10", 0),
    );
    send_length_prefixed_message(&mut alice, b'B', &bind_body("", ""));
    send_length_prefixed_message(&mut alice, b'D', b"P\0");
    send_length_prefixed_message(&mut alice, b'E', &execute_body("", 0));
    send_length_prefixed_message(&mut alice, b'S', &[]);

    let mut kinds = Vec::new();
    let mut data_rows = 0;
    let mut tag = String::new();
    loop {
        let (kind, body) = read_message(&mut alice);
        kinds.push(kind);
        match kind {
            b'D' => data_rows += 1,
            b'C' => tag = String::from_utf8(body[..body.len() - 1].to_vec()).expect("tag"),
            b'Z' => break,
            b'E' => panic!("unexpected ErrorResponse: {body:?}"),
            _ => {}
        }
    }
    assert!(kinds.contains(&b'T'), "RowDescription expected: {kinds:?}");
    assert_eq!(data_rows, 1);
    assert_eq!(tag, "SELECT 1");
}
