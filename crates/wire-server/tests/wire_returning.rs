//! `INSERT`／`DELETE`／`UPDATE`／UPSERT の `RETURNING` 句（Issue #873・#1182・SQL-21）の
//! 簡易クエリプロトコル経由（生バイトクライアント）検証（層 A。ポインタ:
//! `docs/spec/05-tasks.md` TASK-193・`docs/spec/04-behavior/sql-surface.md`
//! SQL-21）。
//!
//! `RETURNING` の意味論（RLS 再判定・`rows_affected` と投影行数の独立性・
//! 内容照合ハッシュ非依存）そのものは `crates/engine/tests/sql_returning.rs`
//! が確定オラクルとして検証済みのため、本ファイルは同じ規則が **wire
//! フレーミング**（`RowDescription`→`DataRow`*→`CommandComplete`）越しに
//! 観測できることの確認に徹する（`wire_delete_single_row.rs`・
//! `wire_insert_operation_id.rs` と同じ流儀）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::io::Read;
use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-returning-docs");
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
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn spawn_with_users(
    core: Arc<EngineCore>,
    users: &[(&str, &str, &str)],
) -> Vec<std::net::SocketAddr> {
    let users_path = write_user_store_file(users);
    vec![spawn_server_with_engine(&users_path, core)]
}

fn connect_as(addr: std::net::SocketAddr, username: &str, password: &str) -> TcpStream {
    authenticate_to_ready_for_query(addr, username, password)
}

fn insert_sql(id: u64, lang: &str, op_id: &str) -> String {
    format!(
        "INSERT INTO docs (id, embedding, lang) VALUES ({id}, '[0.1,0.2,0.3]', '{lang}') \
         USING OPERATION_ID '{op_id}'"
    )
}

/// 生バイト列そのままを 1 メッセージぶん読み取る（`wire_delete_single_row.rs`
/// と同じヘルパー。応答の完全一致比較に使う）。
fn read_raw_message(stream: &mut TcpStream) -> Vec<u8> {
    let mut header = [0u8; 1];
    stream.read_exact(&mut header).expect("read message type");
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).expect("read length");
    let len = i32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len - 4];
    stream.read_exact(&mut body).expect("read body");

    let mut out = Vec::with_capacity(1 + len);
    out.push(header[0]);
    out.extend_from_slice(&len_buf);
    out.extend_from_slice(&body);
    out
}

/// `INSERT ... RETURNING id, lang` の wire 応答が `RowDescription`（`id`・
/// `lang`）→ `DataRow` 1 件 → `CommandComplete("INSERT 0 1")` →
/// `ReadyForQuery` の順で届くこと。
#[test]
fn wire_insert_returning_emits_row_description_data_row_and_insert_tag() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja') \
         RETURNING id, lang USING OPERATION_ID 'wire-insert-returning'",
    );

    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id".to_string(), "lang".to_string()]);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![Some("1".to_string()), Some("ja".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);
}

/// `RETURNING *` で投影したベクトル列の文字列表現は、同一行を `SELECT` した
/// 場合の `DataRow` テキストと一致する（表層を跨いだエンコードの一致）。
#[test]
fn wire_insert_returning_vector_cell_matches_select_encoding() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja') \
         RETURNING embedding USING OPERATION_ID 'wire-insert-vector'",
    );
    let _columns = read_row_description(&mut stream);
    let returning_row = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(&mut stream, "SELECT embedding FROM docs LIMIT 10");
    let _columns = read_row_description(&mut stream);
    let select_row = read_data_row(&mut stream);
    assert_eq!(read_command_complete(&mut stream), "SELECT 1");
    read_ready_for_query(&mut stream);

    assert_eq!(returning_row, select_row);
}

/// `DELETE ... RETURNING *` は削除前の値を 1 行返し、`CommandComplete` は
/// `DELETE 1`。
#[test]
fn wire_delete_returning_emits_one_data_row_and_delete_tag() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        &insert_sql(1, "ja", "wire-delete-returning-seed"),
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "DELETE FROM docs WHERE id = 1 RETURNING id, lang USING OPERATION_ID 'wire-delete-returning'",
    );
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id".to_string(), "lang".to_string()]);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![Some("1".to_string()), Some("ja".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "DELETE 1");
    read_ready_for_query(&mut stream);
}

/// RLS-9: 他テナント（bob）保持 id への `DELETE ... RETURNING` と未存在 id
/// への `DELETE ... RETURNING` とで、wire 応答（`RowDescription`＋
/// `CommandComplete("DELETE 0")`＋`ReadyForQuery` の生バイト列）が完全に
/// 一致すること（存在情報の非漏えい。`wire_delete_single_row.rs` と同じ
/// 検証方式）。
#[test]
fn wire_delete_returning_response_is_byte_identical_for_other_tenant_row_and_nonexistent_id() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(
        core,
        &[
            ("alice", "tenant-a", "pw-alice"),
            ("bob", "tenant-b", "pw-bob"),
        ],
    );

    let mut bob_stream = connect_as(addrs[0], "bob", "pw-bob");
    send_simple_query(
        &mut bob_stream,
        &insert_sql(42, "en", "wire-delete-returning-bob-seed"),
    );
    assert_eq!(read_command_complete(&mut bob_stream), "INSERT 0 1");
    read_ready_for_query(&mut bob_stream);

    let mut alice_stream_a = connect_as(addrs[0], "alice", "pw-alice");
    send_simple_query(
        &mut alice_stream_a,
        "DELETE FROM docs WHERE id = 42 RETURNING id, lang USING OPERATION_ID 'wire-delete-returning-other-tenant'",
    );
    let row_desc_other_tenant = read_raw_message(&mut alice_stream_a);
    let complete_other_tenant = read_raw_message(&mut alice_stream_a);
    let rfq_other_tenant = read_raw_message(&mut alice_stream_a);

    let mut alice_stream_b = connect_as(addrs[0], "alice", "pw-alice");
    send_simple_query(
        &mut alice_stream_b,
        "DELETE FROM docs WHERE id = 4242 RETURNING id, lang USING OPERATION_ID 'wire-delete-returning-nonexistent'",
    );
    let row_desc_nonexistent = read_raw_message(&mut alice_stream_b);
    let complete_nonexistent = read_raw_message(&mut alice_stream_b);
    let rfq_nonexistent = read_raw_message(&mut alice_stream_b);

    assert_eq!(row_desc_other_tenant, row_desc_nonexistent);
    assert_eq!(complete_other_tenant, complete_nonexistent);
    assert_eq!(rfq_other_tenant, rfq_nonexistent);
    assert_eq!(
        std::str::from_utf8(&complete_other_tenant[5..]).unwrap(),
        "DELETE 0\0"
    );

    // bob の行は無傷。
    send_simple_query(&mut bob_stream, "SELECT COUNT(*) FROM docs");
    let _columns = read_row_description(&mut bob_stream);
    let count_row = read_data_row(&mut bob_stream);
    assert_eq!(count_row, vec![Some("1".to_string())]);
    read_command_complete(&mut bob_stream);
    read_ready_for_query(&mut bob_stream);
}

/// `RETURNING` を `USING OPERATION_ID` の後ろに置いた文（構文上の禁止位置）
/// は `42601` で拒否され、接続は維持される。
#[test]
fn wire_returning_after_using_clause_is_rejected_with_42601() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja') \
         USING OPERATION_ID 'wire-returning-bad-order' RETURNING id",
    );
    expect_error_response_with_sqlstate(&mut stream, "42601");
    read_ready_for_query(&mut stream);

    // 接続は維持され、続く通常の（RETURNING なし）文が成功すること。
    send_simple_query(
        &mut stream,
        &insert_sql(2, "ja", "wire-returning-after-bad-order"),
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);
}
// --- UPDATE・述語形 DELETE・UPSERT の RETURNING（Issue #1182・SQL-21） ---

/// 単一行 `UPDATE ... RETURNING` は更新後の値を 1 行返し、`CommandComplete` は
/// `UPDATE 1`。
#[test]
fn wire_update_returning_emits_post_update_row_and_update_tag() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        &insert_sql(1, "ja", "wire-update-returning-seed"),
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "UPDATE docs SET lang = 'en' WHERE id = 1 RETURNING id, lang \
         USING OPERATION_ID 'wire-update-returning'",
    );
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id".to_string(), "lang".to_string()]);
    let row = read_data_row(&mut stream);
    assert_eq!(row, vec![Some("1".to_string()), Some("en".to_string())]);
    assert_eq!(read_command_complete(&mut stream), "UPDATE 1");
    read_ready_for_query(&mut stream);
}

/// 述語形 `UPDATE`／`DELETE ... RETURNING` は一致した全行を返し、タグの件数は
/// 実際に変更した行数。
#[test]
fn wire_predicate_update_and_delete_returning_emit_all_matched_rows() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    for (id, op) in [(1u64, "wire-pred-seed-1"), (2, "wire-pred-seed-2")] {
        send_simple_query(&mut stream, &insert_sql(id, "ja", op));
        assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
        read_ready_for_query(&mut stream);
    }

    send_simple_query(
        &mut stream,
        "UPDATE docs SET lang = 'en' WHERE lang = 'ja' RETURNING id, lang \
         USING OPERATION_ID 'wire-pred-update'",
    );
    let _columns = read_row_description(&mut stream);
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("1".to_string()), Some("en".to_string())]
    );
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("2".to_string()), Some("en".to_string())]
    );
    assert_eq!(read_command_complete(&mut stream), "UPDATE 2");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "DELETE FROM docs WHERE lang = 'en' RETURNING id, lang \
         USING OPERATION_ID 'wire-pred-delete'",
    );
    let _columns = read_row_description(&mut stream);
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("1".to_string()), Some("en".to_string())]
    );
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("2".to_string()), Some("en".to_string())]
    );
    assert_eq!(read_command_complete(&mut stream), "DELETE 2");
    read_ready_for_query(&mut stream);
}

/// UPSERT `RETURNING` は変更した行のみ返し、タグは `INSERT 0 <inserted+updated>`。
#[test]
fn wire_upsert_returning_emits_changed_rows_and_insert_tag() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-a", "pw-alice")]);
    let mut stream = connect_as(addrs[0], "alice", "pw-alice");

    send_simple_query(&mut stream, &insert_sql(1, "ja", "wire-upsert-seed"));
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);

    // id=1 は衝突（DO NOTHING で返さない）、id=2 は新規挿入。
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, lang) VALUES \
         (1, '[0.1,0.2,0.3]', 'en'), (2, '[0.1,0.2,0.3]', 'fr') \
         ON CONFLICT (id) DO NOTHING RETURNING id, lang \
         USING OPERATION_ID 'wire-upsert-returning'",
    );
    let columns = read_row_description(&mut stream);
    assert_eq!(columns, vec!["id".to_string(), "lang".to_string()]);
    assert_eq!(
        read_data_row(&mut stream),
        vec![Some("2".to_string()), Some("fr".to_string())]
    );
    assert_eq!(read_command_complete(&mut stream), "INSERT 0 1");
    read_ready_for_query(&mut stream);
}

/// RLS-9・RLS-10: 他テナント保持 id への単一行 `UPDATE ... RETURNING` と未存在 id
/// への同文とで、wire 応答（`RowDescription`＋`CommandComplete("UPDATE 0")`＋
/// `ReadyForQuery` の生バイト列）が完全に一致する（存在情報の非漏えい）。
#[test]
fn wire_update_returning_response_is_byte_identical_for_other_tenant_row_and_nonexistent_id() {
    let (core, _guard) = new_core_with_docs_table();
    let addrs = spawn_with_users(
        core,
        &[
            ("alice", "tenant-a", "pw-alice"),
            ("bob", "tenant-b", "pw-bob"),
        ],
    );

    let mut bob_stream = connect_as(addrs[0], "bob", "pw-bob");
    send_simple_query(
        &mut bob_stream,
        &insert_sql(42, "en", "wire-update-returning-bob-seed"),
    );
    assert_eq!(read_command_complete(&mut bob_stream), "INSERT 0 1");
    read_ready_for_query(&mut bob_stream);

    let mut alice_stream_a = connect_as(addrs[0], "alice", "pw-alice");
    send_simple_query(
        &mut alice_stream_a,
        "UPDATE docs SET lang = 'xx' WHERE id = 42 RETURNING id, lang \
         USING OPERATION_ID 'wire-update-returning-other-tenant'",
    );
    let row_desc_other_tenant = read_raw_message(&mut alice_stream_a);
    let complete_other_tenant = read_raw_message(&mut alice_stream_a);
    let rfq_other_tenant = read_raw_message(&mut alice_stream_a);

    let mut alice_stream_b = connect_as(addrs[0], "alice", "pw-alice");
    send_simple_query(
        &mut alice_stream_b,
        "UPDATE docs SET lang = 'xx' WHERE id = 4242 RETURNING id, lang \
         USING OPERATION_ID 'wire-update-returning-nonexistent'",
    );
    let row_desc_nonexistent = read_raw_message(&mut alice_stream_b);
    let complete_nonexistent = read_raw_message(&mut alice_stream_b);
    let rfq_nonexistent = read_raw_message(&mut alice_stream_b);

    assert_eq!(row_desc_other_tenant, row_desc_nonexistent);
    assert_eq!(complete_other_tenant, complete_nonexistent);
    assert_eq!(rfq_other_tenant, rfq_nonexistent);
    assert_eq!(
        std::str::from_utf8(&complete_other_tenant[5..]).unwrap(),
        "UPDATE 0\0"
    );

    // bob の行は無傷（lang は en のまま）。
    send_simple_query(&mut bob_stream, "SELECT lang FROM docs LIMIT 10");
    let _columns = read_row_description(&mut bob_stream);
    assert_eq!(read_data_row(&mut bob_stream), vec![Some("en".to_string())]);
    read_command_complete(&mut bob_stream);
    read_ready_for_query(&mut bob_stream);
}
