//! `ARRAY` 列型（TABLE-14・TASK-198、Issue #888）の簡易クエリプロトコル経由
//! （生バイトクライアント）検証（層 A。ポインタ: `docs/spec/05-tasks.md`
//! TASK-198・`docs/spec/04-behavior/data-model.md` TABLE-14・
//! `docs/spec/04-behavior/wire-protocol.md` WIRE-13）。
//!
//! 配列列そのものの符号化・意味論は `crates/engine/tests/composite_types.rs`
//! が確定オラクルとして検証済みのため、本ファイルは同じ規則が **wire
//! フレーミング** 越しに観測できることの確認に徹する
//! （`wire_delete_single_row.rs` と同じ流儀）。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ArrayElemType, ArrayType, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::storage::Storage;

use common::*;

fn new_core_with_array_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-array-column-docs");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new(
                    "tags",
                    ColumnType::Array(ArrayType::new(ArrayElemType::Text, 4).expect("array ty")),
                    true,
                ),
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

/// INSERT・SELECT を simple query 経由で往復し、`RowDescription` の列名と
/// `DataRow` の PostgreSQL 配列テキスト表現（引用・エスケープを含む）を確認する。
#[test]
fn wire_array_column_insert_and_select_roundtrip() {
    let (core, _guard) = new_core_with_array_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-alice", "pw-alice")]);
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        r#"INSERT INTO docs (id, embedding, tags) VALUES (1, '[0.1,0.2]', '{a,"b c",""}') USING OPERATION_ID 'op-1'"#,
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "INSERT 0 1");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT id, tags FROM docs WHERE id = 1 LIMIT 1",
    );
    let names = read_row_description(&mut stream);
    assert_eq!(names, vec!["id".to_string(), "tags".to_string()]);
    let row = read_data_row(&mut stream);
    assert_eq!(row[0].as_deref(), Some("1"));
    assert_eq!(row[1].as_deref(), Some(r#"{a,"b c",""}"#));
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 空配列と NULL 列が区別されて往復すること。
#[test]
fn wire_array_column_distinguishes_null_and_empty_array() {
    let (core, _guard) = new_core_with_array_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-alice", "pw-alice")]);
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, tags) VALUES (1, '[0.1,0.2]', '{}') USING OPERATION_ID 'op-1'",
    );
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding) VALUES (2, '[0.1,0.2]') USING OPERATION_ID 'op-2'",
    );
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT id, tags FROM docs WHERE id = 1 LIMIT 1",
    );
    let _ = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[1].as_deref(), Some("{}"));
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "SELECT id, tags FROM docs WHERE id = 2 LIMIT 1",
    );
    let _ = read_row_description(&mut stream);
    let row = read_data_row(&mut stream);
    assert_eq!(row[1], None);
    let _ = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 要素数がスキーマの `max_len` を超過する INSERT は `54000`（`PayloadTooLarge`）。
#[test]
fn wire_array_literal_exceeding_max_len_is_rejected_with_54000() {
    let (core, _guard) = new_core_with_array_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-alice", "pw-alice")]);
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    // tags は max_len=4。5 要素は超過。
    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, tags) VALUES (1, '[0.1,0.2]', '{a,b,c,d,e}') USING OPERATION_ID 'op-1'",
    );
    expect_error_response_with_sqlstate(&mut stream, "54000");
    read_ready_for_query(&mut stream);
}

/// NULL 要素（引用なし）は機能未対応の `22000`、閉じていない引用は形式不正の `22P02`。
#[test]
fn wire_array_literal_format_violations_are_rejected_with_expected_codes() {
    let (core, _guard) = new_core_with_array_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-alice", "pw-alice")]);
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    send_simple_query(
        &mut stream,
        "INSERT INTO docs (id, embedding, tags) VALUES (1, '[0.1,0.2]', '{a,null,b}') USING OPERATION_ID 'op-1'",
    );
    expect_error_response_with_sqlstate(&mut stream, "22000");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        r#"INSERT INTO docs (id, embedding, tags) VALUES (2, '[0.1,0.2]', '{"a}') USING OPERATION_ID 'op-2'"#,
    );
    expect_error_response_with_sqlstate(&mut stream, "22P02");
    read_ready_for_query(&mut stream);
}

/// `WHERE` で配列列を参照する述語は拒否される（D-A8。等価・パス演算子は本
/// Issue の対象外のまま）。
#[test]
fn wire_where_referencing_array_column_is_rejected() {
    let (core, _guard) = new_core_with_array_table();
    let addrs = spawn_with_users(core, &[("alice", "tenant-alice", "pw-alice")]);
    let mut stream = authenticate_to_ready_for_query(addrs[0], "alice", "pw-alice");

    send_simple_query(&mut stream, "SELECT id FROM docs WHERE tags = 'x' LIMIT 10");
    // 型不一致による拒否（`22000`。engine 側の確定契約は
    // `composite_types.rs::where_referencing_array_column_is_rejected` が担う）。
    expect_error_response_with_sqlstate(&mut stream, "22000");
    read_ready_for_query(&mut stream);
}
