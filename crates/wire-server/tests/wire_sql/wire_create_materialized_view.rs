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

use crate::common;

use crate::temp_db;

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

/// REFRESH（Issue #1665）: タグは件数を持たない固定の `REFRESH MATERIALIZED VIEW`。REFRESH 後は
/// 作成者でない別テナントの接続でも、更新後の自分の文脈の実体を読める。
#[test]
fn wire_refresh_tag_and_other_tenants_read_refreshed_rows() {
    let (addr, _guard) = spawn("wire-mv-refresh");
    let mut alice = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut bob = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    insert(&mut alice, 1, "ja");
    insert(&mut bob, 3, "ja");
    send_simple_query(
        &mut alice,
        "CREATE MATERIALIZED VIEW mv AS SELECT id FROM docs",
    );
    assert_eq!(read_command_complete(&mut alice), "SELECT 1");
    read_ready_for_query(&mut alice);

    insert(&mut bob, 4, "en");
    // REFRESH 前の bob は旧実体（1 行）。
    let (bob_ids, _) = read_ids(&mut bob, "SELECT id FROM mv LIMIT 100");
    assert_eq!(bob_ids, vec!["3".to_string()]);

    // DDL 権限の無い bob は 42501。
    send_simple_query(&mut bob, "REFRESH MATERIALIZED VIEW mv");
    expect_error_response_with_sqlstate(&mut bob, "42501");
    read_ready_for_query(&mut bob);

    send_simple_query(&mut alice, "REFRESH MATERIALIZED VIEW mv");
    assert_eq!(
        read_command_complete(&mut alice),
        "REFRESH MATERIALIZED VIEW"
    );
    read_ready_for_query(&mut alice);

    let (bob_ids, _) = read_ids(&mut bob, "SELECT id FROM mv LIMIT 100");
    assert_eq!(bob_ids, vec!["3".to_string(), "4".to_string()]);
}

/// `WITH NO DATA` の REFRESH 後は全テナントの読み取りが `55000`、続く REFRESH で再び読める。
#[test]
fn wire_refresh_with_no_data_then_read_is_55000() {
    let (addr, _guard) = spawn("wire-mv-refresh-nodata");
    let mut alice = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut bob = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    insert(&mut bob, 3, "ja");
    send_simple_query(
        &mut alice,
        "CREATE MATERIALIZED VIEW mv AS SELECT id FROM docs",
    );
    let _ = read_command_complete(&mut alice);
    read_ready_for_query(&mut alice);

    send_simple_query(&mut alice, "REFRESH MATERIALIZED VIEW mv WITH NO DATA");
    assert_eq!(
        read_command_complete(&mut alice),
        "REFRESH MATERIALIZED VIEW"
    );
    read_ready_for_query(&mut alice);
    send_simple_query(&mut bob, "SELECT id FROM mv LIMIT 10");
    expect_error_response_with_sqlstate(&mut bob, "55000");
    read_ready_for_query(&mut bob);

    send_simple_query(&mut alice, "REFRESH MATERIALIZED VIEW mv");
    let _ = read_command_complete(&mut alice);
    read_ready_for_query(&mut alice);
    let (bob_ids, _) = read_ids(&mut bob, "SELECT id FROM mv LIMIT 10");
    assert_eq!(bob_ids, vec!["3".to_string()]);
}

/// `REFRESH ... CONCURRENTLY`（TABLE-42 (4)・Issue #1666）: 適格な UNIQUE 索引が無ければ `55000`、
/// 作成後は固定タグ `REFRESH MATERIALIZED VIEW` で成功し、他テナントは更新後の実体を読める。
#[test]
fn wire_refresh_concurrently_prereqs_and_tag() {
    let (addr, _guard) = spawn("wire-mv-refresh-conc");
    let mut alice = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut bob = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    insert(&mut bob, 3, "ja");
    send_simple_query(
        &mut alice,
        "CREATE MATERIALIZED VIEW mv AS SELECT id FROM docs",
    );
    let _ = read_command_complete(&mut alice);
    read_ready_for_query(&mut alice);
    insert(&mut bob, 4, "en");

    send_simple_query(&mut alice, "REFRESH MATERIALIZED VIEW CONCURRENTLY mv");
    expect_error_response_with_sqlstate(&mut alice, "55000");
    read_ready_for_query(&mut alice);

    send_simple_query(&mut alice, "CREATE UNIQUE INDEX mv_id ON mv (id)");
    let _ = read_command_complete(&mut alice);
    read_ready_for_query(&mut alice);
    send_simple_query(&mut alice, "REFRESH MATERIALIZED VIEW CONCURRENTLY mv");
    assert_eq!(
        read_command_complete(&mut alice),
        "REFRESH MATERIALIZED VIEW"
    );
    read_ready_for_query(&mut alice);
    let (bob_ids, _) = read_ids(&mut bob, "SELECT id FROM mv LIMIT 100");
    assert_eq!(bob_ids, vec!["3".to_string(), "4".to_string()]);
}

/// matview への索引と `DROP MATERIALIZED VIEW`（TABLE-42 (5)・Issue #1667）の CommandComplete タグ・
/// 権限ゲート・他テナント不可視。意味論は `crates/engine/tests/table42_matview_index_drop.rs`
/// が確定オラクル。
#[test]
fn wire_index_and_drop_tags_permission_gate_and_other_tenants_see_the_drop() {
    let (addr, _guard) = spawn("wire-mv-index-drop");
    let mut alice = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    let mut bob = authenticate_to_ready_for_query(addr, "bob", "battery-staple");
    insert(&mut alice, 1, "ja");
    insert(&mut bob, 2, "en");

    send_simple_query(
        &mut alice,
        "CREATE MATERIALIZED VIEW mv AS SELECT id, lang FROM docs",
    );
    assert_eq!(read_command_complete(&mut alice), "SELECT 1");
    read_ready_for_query(&mut alice);

    // 索引の作成タグ。DDL 権限の無い bob は存在の有無によらず 42501。
    send_simple_query(&mut alice, "CREATE UNIQUE INDEX ux_id ON mv (id)");
    assert_eq!(read_command_complete(&mut alice), "CREATE INDEX");
    read_ready_for_query(&mut alice);
    for sql in [
        "CREATE INDEX ix_b ON mv (lang)",
        "DROP MATERIALIZED VIEW mv",
        "DROP MATERIALIZED VIEW ghost",
    ] {
        send_simple_query(&mut bob, sql);
        expect_error_response_with_sqlstate(&mut bob, "42501");
        read_ready_for_query(&mut bob);
    }
    // 索引があっても bob は自分の文脈の行だけを読む。
    let (bob_ids, _) = read_ids(&mut bob, "SELECT id FROM mv LIMIT 100");
    assert_eq!(bob_ids, vec!["2".to_string()]);

    // DROP のタグ。依存する表の RESTRICT 削除は 2BP01。
    send_simple_query(&mut alice, "DROP TABLE docs");
    expect_error_response_with_sqlstate(&mut alice, "2BP01");
    read_ready_for_query(&mut alice);
    send_simple_query(&mut alice, "DROP MATERIALIZED VIEW mv");
    assert_eq!(read_command_complete(&mut alice), "DROP MATERIALIZED VIEW");
    read_ready_for_query(&mut alice);
    // 作成していない bob の接続でも、削除後は 42P01（55000 ではない）。
    send_simple_query(&mut bob, "SELECT id FROM mv LIMIT 10");
    expect_error_response_with_sqlstate(&mut bob, "42P01");
    read_ready_for_query(&mut bob);
    // IF EXISTS の no-op も固定タグ。
    send_simple_query(&mut alice, "DROP MATERIALIZED VIEW IF EXISTS mv");
    assert_eq!(read_command_complete(&mut alice), "DROP MATERIALIZED VIEW");
    read_ready_for_query(&mut alice);
}
