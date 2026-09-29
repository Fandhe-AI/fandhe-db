//! `TRUNCATE TABLE <table> USING OPERATION_ID '<id>'`（TASK-193、対象ビヘイビア:
//! SQL-22、Issue #1200）の wire 経路（生バイトクライアント）検証（層 A）。
//!
//! TRUNCATE の意味論そのもの（自テナント行の全削除・台帳・FK 検査）は
//! `crates/engine/tests/truncate_table.rs` が確定オラクルとして検証済みのため、
//! 本ファイルは `simple_query::map_outcome`（簡易・拡張クエリ両経路が共有）が
//! 返す `CommandComplete` タグ `TRUNCATE TABLE`（件数なし）と、RLS-9（他テナント
//! 行数に応答が依存しないこと）・RECOVER-1/10 のエラー契約が wire 越しに維持され、
//! エラー後も接続が継続利用できることに徹する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::net::TcpStream;
use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::storage::{RowInput, Storage, Visibility};

use common::*;

const USERS: &[(&str, &str, &str)] = &[
    ("alice", "tenant-a", "correct-horse"),
    ("bob", "tenant-b", "battery-staple"),
];

fn new_core_with_docs_table() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-truncate-table");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create table");
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

/// wire 経由のセッションと同じ可視性（Public＋Private）で `Private` 行を直接投入する。
fn seed_private_rows(core: &EngineCore, tenant: &str, ids: std::ops::Range<u64>) {
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    for id in ids {
        core.insert_row(
            &ctx,
            "docs",
            id,
            &RowInput {
                tenant_id: tenant,
                visibility: Visibility::Private,
                embedding: &[0.1f32, 0.2f32],
                metadata: &[],
            },
            Some(&OperationId::parse(&format!("seed-{tenant}-{id}")).expect("valid op id")),
        )
        .expect("seed row");
    }
}

fn users_file() -> std::path::PathBuf {
    write_user_store_file(USERS)
}

fn count_over_wire(stream: &mut TcpStream) -> String {
    send_simple_query(stream, "SELECT COUNT(*) FROM docs");
    let _cols = read_row_description(stream);
    let row = read_data_row(stream);
    let _tag = read_command_complete(stream);
    read_ready_for_query(stream);
    row.into_iter().next().flatten().expect("count cell")
}

/// 簡易クエリ: タグは件数を含まない `TRUNCATE TABLE` の完全一致。
#[test]
fn wire_truncate_simple_query_returns_bare_tag() {
    let (core, _g) = new_core_with_docs_table();
    seed_private_rows(&core, "tenant-a", 1..6);
    let addr = spawn_server_with_engine(&users_file(), core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");
    assert_eq!(count_over_wire(&mut stream), "5");

    send_simple_query(
        &mut stream,
        "TRUNCATE TABLE docs USING OPERATION_ID 'op-wire-t1'",
    );
    assert_eq!(read_command_complete(&mut stream), "TRUNCATE TABLE");
    read_ready_for_query(&mut stream);
    assert_eq!(count_over_wire(&mut stream), "0");
}

/// 拡張クエリ: Parse/Bind/Execute/Sync でも同じタグで、RowDescription は出ない。
#[test]
fn wire_truncate_extended_query_returns_bare_tag() {
    let (core, _g) = new_core_with_docs_table();
    seed_private_rows(&core, "tenant-a", 1..4);
    let addr = spawn_server_with_engine(&users_file(), core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_length_prefixed_message(
        &mut stream,
        b'P',
        &parse_body("", "TRUNCATE TABLE docs USING OPERATION_ID 'op-wire-t2'", 0),
    );
    send_length_prefixed_message(&mut stream, b'B', &bind_body("", ""));
    send_length_prefixed_message(&mut stream, b'E', &execute_body("", 0));
    send_length_prefixed_message(&mut stream, b'S', &[]);

    let mut tag = None;
    loop {
        let (ty, body) = read_message(&mut stream);
        match ty {
            b'1' | b'2' => {}
            b'C' => {
                let end = body
                    .iter()
                    .position(|b| *b == 0)
                    .expect("nul-terminated tag");
                tag = Some(String::from_utf8(body[..end].to_vec()).expect("utf8 tag"));
            }
            b'Z' => break,
            other => panic!("unexpected message type {}", other as char),
        }
    }
    assert_eq!(tag.as_deref(), Some("TRUNCATE TABLE"));
    assert_eq!(count_over_wire(&mut stream), "0");
}

/// RLS-9: 他テナント行が 0 件でも多数でも alice への応答は同一で、bob の行は無傷。
#[test]
fn wire_truncate_response_independent_of_other_tenant_rows() {
    let users = users_file();

    let (core0, _g0) = new_core_with_docs_table();
    seed_private_rows(&core0, "tenant-a", 1..3);
    let addr0 = spawn_server_with_engine(&users, core0);
    let mut s0 = authenticate_to_ready_for_query(addr0, "alice", "correct-horse");
    send_simple_query(&mut s0, "TRUNCATE TABLE docs USING OPERATION_ID 'op-rls9'");
    let tag0 = read_command_complete(&mut s0);
    read_ready_for_query(&mut s0);

    let (core1, _g1) = new_core_with_docs_table();
    seed_private_rows(&core1, "tenant-a", 1..3);
    seed_private_rows(&core1, "tenant-b", 100..130);
    let addr1 = spawn_server_with_engine(&users, core1);
    let mut s1 = authenticate_to_ready_for_query(addr1, "alice", "correct-horse");
    send_simple_query(&mut s1, "TRUNCATE TABLE docs USING OPERATION_ID 'op-rls9'");
    let tag1 = read_command_complete(&mut s1);
    read_ready_for_query(&mut s1);

    assert_eq!(tag0, "TRUNCATE TABLE");
    assert_eq!(tag0, tag1);

    let mut bob = authenticate_to_ready_for_query(addr1, "bob", "battery-staple");
    assert_eq!(count_over_wire(&mut bob), "30");
}

/// RECOVER-1/10: operation_id 省略は 23502、同一 ID 再送は 23505。どちらの後も接続は継続利用できる。
#[test]
fn wire_truncate_error_contract_keeps_connection_usable() {
    let (core, _g) = new_core_with_docs_table();
    seed_private_rows(&core, "tenant-a", 1..4);
    let addr = spawn_server_with_engine(&users_file(), core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "TRUNCATE TABLE docs");
    expect_error_response_with_sqlstate(&mut stream, "23502");
    read_ready_for_query(&mut stream);
    assert_eq!(count_over_wire(&mut stream), "3");

    send_simple_query(
        &mut stream,
        "TRUNCATE TABLE docs USING OPERATION_ID 'op-dup'",
    );
    assert_eq!(read_command_complete(&mut stream), "TRUNCATE TABLE");
    read_ready_for_query(&mut stream);

    send_simple_query(
        &mut stream,
        "TRUNCATE TABLE docs USING OPERATION_ID 'op-dup'",
    );
    expect_error_response_with_sqlstate(&mut stream, "23505");
    read_ready_for_query(&mut stream);
    assert_eq!(count_over_wire(&mut stream), "0");
}
