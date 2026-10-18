//! `INSERT INTO <table> (...) VALUES (...) USING OPERATION_ID '<id>'`
//! （TASK-80、対象ビヘイビア: SQL-10）の簡易クエリプロトコル経由（生バイト
//! クライアント）検証（TASK-82 の層 A。ポインタ: `docs/spec/05-tasks.md`
//! TASK-82・`docs/spec/04-behavior/sql-surface.md` SQL-10・
//! `docs/spec/04-behavior/recovery.md` RECOVER-1・RECOVER-7・RECOVER-10）。
//!
//! `operation_id` の意味論（必須化・台帳・重複拒否・再送判定・内容照合）
//! そのものは `crates/engine/tests/sql_operation_id.rs`（`execute_insert_sql`
//! 直呼び出し）・`crates/engine/tests/sql_insert_session_dispatch.rs`
//! （`execute_sql_in_session` 経由）が確定オラクルとして検証済みのため、
//! 本ファイルは同じ規則が **wire フレーミング** 越しに観測できることの確認に
//! 徹する（`wire_explain.rs`・`wire_hint_order.rs`・`wire_udf_call.rs` と
//! 同じ流儀）。基本的な受理（`CommandComplete("INSERT 0 1")`）・可視性の契約
//! （RLS-11・TASK-195。書いた本人は同一 wire セッションでも自テナントの
//! `Private` 行を読み戻せる）は `crates/wire-server/tests/wire1_simple_query.rs`
//! （`wire1_insert_is_accepted_and_row_is_visible_over_wire_select_to_own_tenant`）
//! が常時（`make ci`）回帰保護する主契約であるため、本ファイルはそれ以外の
//! 経路（省略・再送・内容不一致・パラメータ形式禁止）に集中する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-insert-operation-id-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
    (Arc::new(core), guard)
}

fn spawn_with_alice(core: Arc<EngineCore>) -> std::net::TcpStream {
    let users_path = write_user_store_file(&[("alice", "tenant-a", "pw-alice")]);
    let addr = spawn_server_with_engine(&users_path, core);
    authenticate_to_ready_for_query(addr, "alice", "pw-alice")
}

fn insert_sql(id: u64, lang: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[0.1,0.2,0.3]', '{lang}') USING OPERATION_ID '{op_id}'"
    )
}

/// 複数行 `VALUES`（SQL-16・TASK-190）の wire 文を組み立てる。
fn multi_row_insert_sql(ids: &[u64], op_id: &str) -> String {
    let rows: Vec<String> = ids
        .iter()
        .map(|id| format!("({id}, '[0.1,0.2,0.3]', 'ja')"))
        .collect();
    format!(
        "INSERT INTO docs (id, embedding, lang) VALUES {} USING OPERATION_ID '{op_id}'",
        rows.join(", ")
    )
}

fn new_core_with_docs_table_and_limits(
    limits: engine::batch_limits::BatchLimits,
) -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-insert-operation-id-docs-limits");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(3), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered)
        .with_batch_limits(limits);
    (Arc::new(core), guard)
}

/// `USING OPERATION_ID` 句の省略は書き込みトランザクション開始前に `23502`
/// （`MISSING_OPERATION_ID`）で拒否され、接続は維持される。
#[test]
fn wire_insert_missing_operation_id_clause_is_rejected_with_23502() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja')",
    );
    expect_error_response_with_sqlstate(&mut stream, "23502");
    read_ready_for_query(&mut stream);

    // 接続は維持され、続く正規の INSERT が成功すること。
    send_simple_query(&mut stream, &insert_sql(1, "ja", "wire-op-after-missing"));
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);
}

/// 同一内容の文を同一 `operation_id` で再送すると `23505`（重複拒否＝commit
/// 済み判定。RECOVER-7 の SQL 表層表現）で拒否される。
#[test]
fn wire_insert_resending_the_same_statement_is_rejected_with_23505() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);
    let sql = insert_sql(1, "ja", "wire-op-resend");

    send_simple_query(&mut stream, &sql);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, &sql);
    expect_error_response_with_sqlstate(&mut stream, "23505");
    read_ready_for_query(&mut stream);
}

/// 同一 `operation_id` で内容の異なる文を再発行すると `22023`（内容不一致。
/// RECOVER-10）で拒否される。
#[test]
fn wire_insert_same_operation_id_different_content_is_rejected_with_22023() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(&mut stream, &insert_sql(1, "ja", "wire-op-mismatch"));
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);

    // id・lang が異なる別内容の文へ同一 operation_id を使い回す。
    send_simple_query(&mut stream, &insert_sql(2, "en", "wire-op-mismatch"));
    expect_error_response_with_sqlstate(&mut stream, "22023");
    read_ready_for_query(&mut stream);
}

/// `USING OPERATION_ID $1`（拡張クエリプロトコル向けパラメータ形式）は簡易
/// クエリプロトコル経由では構文エラー（`42601`）で拒否される（MVP は簡易
/// クエリプロトコルの文字列リテラル規範形のみを受理する。SQL-10 の「専用句を
/// 唯一の規範経路とする」契約）。
#[test]
fn wire_insert_operation_id_dollar_placeholder_is_rejected_as_syntax_error() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja') USING OPERATION_ID $1",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}

/// 検索モード句（`USING MODE`）を `INSERT` へ付与するのは許可形状に存在しない
/// ため `42601` で拒否される（SQL-12 の「モード指定は読み取り系検索クエリに
/// のみ有効」契約を wire 経由でも確認する）。
#[test]
fn wire_insert_with_using_mode_clause_is_rejected_as_syntax_error() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja') USING MODE 'recall' USING OPERATION_ID 'wire-op-mode'",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);
}

// ---------------------------------------------------------------------
// 複数行 VALUES（SQL-16・TASK-190）の wire フレーミング越しの確認
// （Issue #863）
// ---------------------------------------------------------------------

/// 複数行 `VALUES` は `CommandComplete("INSERT 0 N")`（`N` は行数）で受理され、
/// 同一 wire セッションから RLS-11（read-your-writes）で全行を読み戻せる。
#[test]
fn wire_insert_multi_row_command_complete_tag_reports_row_count() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        &multi_row_insert_sql(&[1, 2, 3], "wire-op-multi-row"),
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 3");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT id FROM docs LIMIT 100");
    let _columns = read_row_description(&mut stream);
    let mut ids: Vec<u64> = Vec::new();
    for _ in 0..3 {
        let row = read_data_row(&mut stream);
        let id: u64 = row[0]
            .as_ref()
            .expect("id must not be NULL")
            .parse()
            .expect("id must be numeric");
        ids.push(id);
    }
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2, 3]);
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 3");
    read_ready_for_query(&mut stream);
}

/// `BatchLimits`（INDEX-4）を絞った構成で複数行 `VALUES` が上限超過となる場合、
/// `54000` で拒否されたうえで接続は維持され、後続の正規 INSERT が成功する
/// （NoSQL 表層 `rows[]` と同一の上限判定を SQL 表層の wire 経由でも共有する
/// ことの確認。`crates/engine/tests/insert_multi_row.rs` の対応する engine
/// 層テストが確定オラクル）。
#[test]
fn wire_insert_multi_row_over_batch_limits_is_rejected_with_54000_and_connection_survives() {
    let (core, _guard) = new_core_with_docs_table_and_limits(engine::batch_limits::BatchLimits {
        max_files_per_batch: 1,
        ..engine::batch_limits::BatchLimits::default()
    });
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        &multi_row_insert_sql(&[1, 2], "wire-op-over-limit"),
    );
    expect_error_response_with_sqlstate(&mut stream, "54000");
    read_ready_for_query(&mut stream);

    // 接続は維持され、上限内の後続 INSERT が成功すること。
    send_simple_query(
        &mut stream,
        &insert_sql(1, "ja", "wire-op-after-over-limit"),
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);
}

/// バッチ内 `id` 重複は `23505` で拒否されたうえで接続は維持される。
#[test]
fn wire_insert_multi_row_duplicate_id_within_batch_is_rejected_with_23505() {
    let (core, _guard) = new_core_with_docs_table();
    let mut stream = spawn_with_alice(core);

    send_simple_query(
        &mut stream,
        &multi_row_insert_sql(&[1, 1, 2], "wire-op-dup-id"),
    );
    expect_error_response_with_sqlstate(&mut stream, "23505");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, &insert_sql(1, "ja", "wire-op-after-dup-id"));
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);
}
