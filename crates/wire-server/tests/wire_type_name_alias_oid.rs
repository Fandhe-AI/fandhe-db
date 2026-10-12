//! 型名エイリアス（`INT`・`INT4`・`INT8`・`FLOAT4`・`FLOAT8`。Issue #1529・TABLE-29）で
//! 作った列の `RowDescription` が、正規型と同じ PostgreSQL 組み込み OID・typlen を
//! 公告することを生バイトクライアント（`tests/common`）で固定する層 A 結合テスト。
//! エイリアスは catalog 上で正規の `ColumnType` として保存されるため、公告は
//! `result_encoder::column_wire_type` の既存写像に一本化される（本テストはその観測）。
//! `ALTER COLUMN ... TYPE <別名>` で変えた列も同様に確認する。

#[path = "common/mod.rs"]
mod common;

#[path = "../../engine/src/test_util/temp_db.rs"]
mod temp_db;

use std::sync::Arc;

use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use common::*;

/// `(列名, 期待 OID, 期待 typlen)`。`WireType` とは独立に本テスト側で持つ。
fn expected_types() -> Vec<(&'static str, i32, i16)> {
    vec![
        ("a", 23, 4),
        ("b", 23, 4),
        ("c", 20, 8),
        ("d", 700, 4),
        ("e", 701, 8),
        ("f", 25, -1),
        ("g", 20, 8),
        // Issue #2304: BOOL・FLOAT・FLOAT(24)・FLOAT(53)・TIMESTAMP WITHOUT TIME ZONE。
        ("h", 16, 1),
        ("i", 701, 8),
        ("j", 700, 4),
        ("k", 701, 8),
        ("l", 1114, 8),
    ]
}

fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-alias-oid");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let mut session = SessionState::default();
    session.allow_ddl();
    for sql in [
        "CREATE TABLE p (embedding VECTOR(2) NOT NULL, a INT, b INT4, c INT8, d FLOAT4, \
         e FLOAT8, f INT4[], g INTEGER, h BOOL, i FLOAT, j FLOAT(24), k FLOAT(53), \
         l TIMESTAMP WITHOUT TIME ZONE)",
        "ALTER TABLE p ALTER COLUMN g TYPE INT8",
    ] {
        core.execute_sql_in_session(&ctx, &mut session, sql)
            .expect("ddl");
    }
    (Arc::new(core), guard)
}

#[test]
fn alias_columns_announce_canonical_oids_and_typlen() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[("alice", "tenant-a", "correct-horse")]);
    let addr = spawn_server_with_engine(&users_path, core);
    let mut stream = authenticate_to_ready_for_query(addr, "alice", "correct-horse");

    send_simple_query(
        &mut stream,
        "SELECT a, b, c, d, e, f, g, h, i, j, k, l FROM p LIMIT 10",
    );
    let columns = read_row_description_with_types(&mut stream);
    let expected = expected_types();
    assert_eq!(columns.len(), expected.len());
    for ((name, oid, typlen), (en, eo, et)) in columns.iter().zip(expected.iter()) {
        assert_eq!(name, en);
        assert_eq!(oid, eo, "oid mismatch for column={name}");
        assert_eq!(typlen, et, "typlen mismatch for column={name}");
    }
    let _tag = read_command_complete(&mut stream);
    read_ready_for_query(&mut stream);
}
