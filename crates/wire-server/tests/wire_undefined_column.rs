//! FROM ありの SELECT／DML が存在しない列を参照したときの SQLSTATE が `42703` で
//! あること（Issue #2332。ポインタ: ERR-2・ERR-6）を、PostgreSQL wire v3 の簡易
//! クエリ経路（生バイトクライアント）で確認する結合テスト（層 A）。
//!
//! 分類そのものの網羅（JOIN・サブクエリ・副作用なし・型不正との境界）は
//! `crates/engine/tests/sql_undefined_column_42703.rs` が担うため、本ファイルは
//! wire フレーミング越しの `ErrorResponse` の SQLSTATE・文言と、テナント間で応答が
//! 同一であること（他テナントの存在情報を漏らさない）に絞る。

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

/// `docs(embedding VECTOR(2), lang TEXT)`。tenant-a にだけ 1 行（id=1）を持たせる。
fn new_core() -> (Arc<EngineCore>, temp_db::CleanupGuard) {
    let path = temp_db::unique_db_path("wire-undefined-column");
    let guard = temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("lang", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx,
        1,
        Visibility::Public,
        &[Value::Vector(vec![0.0, 0.0]), Value::Text("ja".to_string())],
        &engine::recovery::required_op_id::OperationId::parse("test-op-1")
            .expect("valid operation_id"),
    )
    .expect("insert row");
    (
        Arc::new(EngineCore::from_storage(
            storage,
            Box::new(CpuScalarProvider),
        )),
        guard,
    )
}

/// 受け入れ条件の経路（投影・WHERE・ORDER BY・GROUP BY・INSERT 列リスト・UPDATE SET・
/// DELETE WHERE・IN サブクエリ外側列）が `42703` と同一文言で拒否され、接続が継続する。
/// tenant-a（行あり）と tenant-b（行なし）で応答が同一。
#[test]
fn undefined_column_is_42703_over_wire_and_identical_across_tenants() {
    let (core, _guard) = new_core();
    let users_path = write_user_store_file(&[
        ("alice", "tenant-a", "pw-alice"),
        ("bob", "tenant-b", "pw-bob"),
    ]);
    let addr = spawn_server_with_engine(&users_path, core);

    for (user, pw) in [("alice", "pw-alice"), ("bob", "pw-bob")] {
        let mut stream = authenticate_to_ready_for_query(addr, user, pw);
        for sql in [
            "SELECT ghost FROM docs LIMIT 10",
            "SELECT id FROM docs WHERE ghost = 'x' LIMIT 10",
            "SELECT id FROM docs ORDER BY ghost LIMIT 10",
            "SELECT ghost, COUNT(*) FROM docs GROUP BY ghost",
            "INSERT INTO docs (id, embedding, ghost) VALUES (2, '[0.1,0.2]', 'x') \
             USING OPERATION_ID 'wire-undef-ins'",
            "UPDATE docs SET ghost = 'x' WHERE id = 1 USING OPERATION_ID 'wire-undef-upd'",
            "DELETE FROM docs WHERE ghost = 'x' USING OPERATION_ID 'wire-undef-del'",
            "SELECT id FROM docs WHERE ghost IN (SELECT lang FROM docs LIMIT 10) LIMIT 10",
        ] {
            send_simple_query(&mut stream, sql);
            expect_error_response_with_sqlstate_and_message(
                &mut stream,
                "42703",
                "column does not exist: ghost",
            );
            read_ready_for_query(&mut stream);
        }
    }
}
