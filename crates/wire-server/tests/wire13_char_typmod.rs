//! `VARCHAR(n)`／`CHAR(n)` の OID（1043／1042）と typmod（`n + 4`）が実際の wire 経路
//! （生バイトクライアント。`tests/common`）の `RowDescription` で観測できることを検証する
//! 層 A 結合テスト（TABLE-31・Issue #1565）。単体写像は `result_encoder.rs` の
//! `mod tests` が固定しており、本ファイルは TCP 越しのバイト列とテナント境界回帰に絞る。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use common::*;

/// 長さ付き列を持つ表を 2 つ（`probe`・`probe2`）作り、tenant-a の private 行を 1 行ずつ投入する。
/// `ch`（CHAR）は #1566 の意味論に依存しないよう NULL を入れる。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire13-char-typmod");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "probe",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("vcn", ColumnType::Varchar(Some(10)), false),
                ColumnDef::new("vc", ColumnType::Varchar(None), false),
                ColumnDef::new("ch", ColumnType::Bpchar(5), true),
            ],
        ))
        .expect("create probe");
    storage
        .create_table(&TableSchema::new(
            "probe2",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("vcn", ColumnType::Varchar(Some(20)), false),
            ],
        ))
        .expect("create probe2");
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let op1 = OperationId::parse("op-wire13-char-typmod-1").expect("op id");
    engine::tenant::insert_typed_row(
        &storage,
        "probe",
        &ctx,
        1,
        Visibility::Private,
        &[
            Value::Vector(vec![1.0, 0.0]),
            Value::Text("abc".to_string()),
            Value::Text("def".to_string()),
            Value::Null,
        ],
        &op1,
    )
    .expect("insert probe");
    let op2 = OperationId::parse("op-wire13-char-typmod-2").expect("op id");
    engine::tenant::insert_typed_row(
        &storage,
        "probe2",
        &ctx,
        1,
        Visibility::Private,
        &[Value::Vector(vec![1.0, 0.0]), Value::Text("x".to_string())],
        &op2,
    )
    .expect("insert probe2");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (Arc::new(core), guard)
}

fn finish(stream: &mut std::net::TcpStream) {
    let _row = read_data_row(stream);
    let _tag = read_command_complete(stream);
    read_ready_for_query(stream);
}

/// 受入基準: クライアントが RowDescription の typmod から列長（`typmod - 4`）を取得できる。
#[test]
fn row_description_announces_varchar_and_bpchar_typmod() {
    let (core, _guard) = new_core();
    let users = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(&mut stream, "SELECT id, vcn, vc, ch FROM probe LIMIT 10");
    let cols = read_row_description_with_typmods(&mut stream);
    let got: Vec<(&str, i32, i16, i32)> = cols
        .iter()
        .map(|(n, o, l, m)| (n.as_str(), *o, *l, *m))
        .collect();
    assert_eq!(
        got,
        vec![
            ("id", 1700, -1, -1),
            ("vcn", 1043, -1, 14),
            ("vc", 1043, -1, -1),
            ("ch", 1042, -1, 9),
        ]
    );
    finish(&mut stream);
}

/// 長さの異なる VARCHAR(n) の UNION は過小な typmod を公告せず -1 にする。
#[test]
fn union_of_different_varchar_lengths_announces_unbounded_typmod() {
    let (core, _guard) = new_core();
    let users = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "SELECT vcn FROM probe UNION ALL SELECT vcn FROM probe2",
    );
    let cols = read_row_description_with_typmods(&mut stream);
    assert_eq!(cols, vec![("vcn".to_string(), 1043, -1, -1)]);
    let _r1 = read_data_row(&mut stream);
    let _r2 = read_data_row(&mut stream);
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}

/// 他テナントは行が見えないだけで、型公告（typmod 含む）は同一でテナント境界に影響しない。
#[test]
fn other_tenant_sees_same_typmod_but_zero_rows() {
    let (core, _guard) = new_core();
    let users = write_user_store_file(&[("bob", "tenant-b", "correct-horse")]);
    let addr = spawn_server_with_engine(&users, core);
    let mut stream = authenticate_to_ready_for_query(addr, "bob", "correct-horse");

    send_simple_query(&mut stream, "SELECT vcn, ch FROM probe LIMIT 10");
    let cols = read_row_description_with_typmods(&mut stream);
    assert_eq!(
        cols,
        vec![
            ("vcn".to_string(), 1043, -1, 14),
            ("ch".to_string(), 1042, -1, 9)
        ]
    );
    let tag = read_command_complete(&mut stream);
    assert_eq!(tag, "SELECT 0");
    read_ready_for_query(&mut stream);
}
