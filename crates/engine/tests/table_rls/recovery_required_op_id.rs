//! `operation_id` 必須化ガードの結合テスト（TASK-92、対象ビヘイビア: RECOVER-1。
//! ポインタ: `docs/spec/05-tasks.md` TASK-92・`docs/spec/04-behavior/recovery.md`
//! RECOVER-1・`docs/spec/04-behavior/error-format.md` ERR-2・
//! `docs/spec/04-behavior/sql-surface.md` SQL-10）。
//!
//! `sql::allowlist::validate_insert`（SQL `INSERT` の構造検証段階）と
//! `core::EngineCore::{insert_row, update_row, delete_row}`（TASK-95。wire 層が DML を
//! 行う際の想定入口）の 2 経路が、`crate::recovery::required_op_id::LedgerMode` という
//! 単一のサーバー構成だけで「書き込み系操作に `operation_id` を必須とするか」を
//! 決定することを、SQL 表層に閉じない engine 横断の視点で検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::storage::{RowInput, Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";
const DIM: u32 = 3;

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn open_storage_with_table(path: &std::path::Path) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    storage
}

// --- T1: Ledgered 構成は SQL INSERT の句省略・明示 NULL をいずれも
//         書き込みトランザクション開始前に 23502 で拒否する -----------------------

#[test]
fn t1_ledgered_rejects_missing_and_explicit_null_operation_id() {
    let path = unique_db_path("recover1-t1");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    // `--require-operation-id` 相当の `Ledgered` を明示する（既定は Issue #1704 で
    // `Optional` に移った。既定の挙動は `recovery_optional_op_id.rs` が検証する）。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::Ledgered);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let missing = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a')",
        )
        .expect_err("missing clause must be rejected by default");
    assert_eq!(missing.wire_code(), "23502");

    let explicit_null = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (2, '[0.1,0.2,0.3]', 'b') USING OPERATION_ID NULL",
        )
        .expect_err("explicit NULL must be rejected by default");
    assert_eq!(explicit_null.wire_code(), "23502");

    // 行数不変（外形確認）。
    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    assert!(result.rows.is_empty());
}

// --- T2: EngineCore::{insert_row, update_row, delete_row} は operation_id が
//         None だと 23502（ストレージ到達前）で拒否し、Some だと従来どおり動く ------

#[test]
fn t2_engine_core_write_guard_rejects_none_before_reaching_storage() {
    let path = unique_db_path("recover1-t2");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_table(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::Ledgered);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let row = RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Public,
        embedding: &[0.1, 0.2, 0.3],
        metadata: &[],
    };

    // 存在しないテーブル名に対して行う: `23502`（`XX000`／`Catalog` ではない）が
    // 返ることで、行ストア・カタログへ一切到達していない
    // （＝書き込みトランザクション開始前に拒否している）ことを独立オラクルとして
    // 確認する。
    let err = core
        .insert_row(&ctx, "no_such_table", 1, &row, None)
        .expect_err("missing operation_id must be rejected before storage access");
    assert_eq!(err.wire_code(), "23502");
    let err = core
        .update_row(&ctx, "no_such_table", 1, &row, None)
        .expect_err("missing operation_id must be rejected before storage access");
    assert_eq!(err.wire_code(), "23502");
    let err = core
        .delete_row(&ctx, "no_such_table", 1, None)
        .expect_err("missing operation_id must be rejected before storage access");
    assert_eq!(err.wire_code(), "23502");

    // `Some` を渡した場合は従来どおりの契約（ここでは実在テーブルへの成功・
    // 未存在行への NotFound）が保たれる。TASK-101（RECOVER-10）: 台帳は
    // `(tenant, table, operation_id)` 単位で内容ハッシュを持ち、insert/update/delete は
    // それぞれ別の正規化入力（`content_hash` モジュール参照）を持つため、同一
    // operation_id を insert→update→delete の 3 操作に使い回すと 2 回目以降が
    // OperationIdContentMismatch になる。各操作へ別々の operation_id を使う。
    let insert_op_id = OperationId::parse("op-t2-insert").expect("valid operation_id");
    let update_op_id = OperationId::parse("op-t2-update").expect("valid operation_id");
    let delete_op_id = OperationId::parse("op-t2-delete").expect("valid operation_id");
    core.insert_row(&ctx, TABLE, 100, &row, Some(&insert_op_id))
        .expect("insert with operation_id should succeed");
    core.update_row(&ctx, TABLE, 100, &row, Some(&update_op_id))
        .expect("update with operation_id should succeed");
    core.delete_row(&ctx, TABLE, 100, Some(&delete_op_id))
        .expect("delete with operation_id should succeed");
}

// --- T3: 保護の適用可否はサーバー構成のみで決まり、クライアント入力
//         （SQL 句・セッション変数）では変えられない ------------------------------

#[test]
fn t3_protection_is_determined_only_by_server_configuration() {
    let sql = "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a')";

    let path_a = unique_db_path("recover1-t3-ledgered");
    let _guard_a = CleanupGuard(path_a.clone());
    let ledgered = EngineCore::from_storage(
        open_storage_with_table(&path_a),
        Box::new(CpuScalarProvider),
    )
    .with_ledger_mode(LedgerMode::Ledgered);

    let path_b = unique_db_path("recover1-t3-compare-only");
    let _guard_b = CleanupGuard(path_b.clone());
    let compare_only = EngineCore::from_storage(
        open_storage_with_table(&path_b),
        Box::new(CpuScalarProvider),
    )
    .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);

    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let ledgered_err = ledgered
        .execute_insert_sql(&ctx, sql)
        .expect_err("Ledgered must reject missing operation_id");
    assert_eq!(ledgered_err.wire_code(), "23502");

    compare_only
        .execute_insert_sql(&ctx, sql)
        .expect("CompareOnlyWithoutLedger must accept missing operation_id");

    // セッション経由での迂回経路が存在しないことのピン留め: `SET ledger_mode = ...`
    // 相当の文は許可リスト外（`42601`）で拒否され、その後の句省略 INSERT は
    // 依然として `23502`（`Ledgered` のまま変化しない）。
    let mut session = engine::sql::mode::SessionState::default();
    let set_err = ledgered
        .execute_sql_in_session(&ctx, &mut session, "SET ledger_mode = 'compare_only'")
        .expect_err("SET ledger_mode must not be an allowed statement");
    assert_eq!(set_err.wire_code(), "42601");

    let still_ledgered_err = ledgered
        .execute_insert_sql(&ctx, sql)
        .expect_err("Ledgered must still reject missing operation_id after the SET attempt");
    assert_eq!(still_ledgered_err.wire_code(), "23502");
}

// --- T4: CompareOnlyWithoutLedger は句省略・明示句付きのいずれも成功し、
//         読み戻せる ---------------------------------------------------------

#[test]
fn t4_compare_only_without_ledger_accepts_missing_and_explicit_operation_id() {
    let path = unique_db_path("recover1-t4");
    let _guard = CleanupGuard(path.clone());
    let core =
        EngineCore::from_storage(open_storage_with_table(&path), Box::new(CpuScalarProvider))
            .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a')",
    )
    .expect("missing clause must be accepted in compare-only mode");
    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (2, '[0.4,0.5,0.6]', 'b') USING OPERATION_ID 'op-t4'",
    )
    .expect("explicit clause must still be accepted in compare-only mode");

    let read_ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let result = core
        .execute_sql(
            &read_ctx,
            "SELECT id FROM documents ORDER BY embedding <=> '[0.1,0.2,0.3]' LIMIT 5",
        )
        .expect("select should succeed");
    let mut ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
}

// --- T5: ガードとテナント境界（RECOVER-4）は独立: operation_id の追加が
//         テナント検査を緩めない ------------------------------------------------

#[test]
fn t5_operation_id_guard_does_not_weaken_tenant_boundary() {
    let path = unique_db_path("recover1-t5");
    let _guard = CleanupGuard(path.clone());
    let core =
        EngineCore::from_storage(open_storage_with_table(&path), Box::new(CpuScalarProvider));
    let attacker = PolicyContext::new("tenant-attacker").expect("valid tenant");
    let op_id = OperationId::parse("op-t5").expect("valid operation_id");

    // 他テナント名義の新規行 + 有効な operation_id は、従来どおり `42501`
    // （`TenantWriteError::Forbidden`）で拒否される。
    let foreign_row = RowInput {
        tenant_id: "tenant-victim",
        visibility: Visibility::Public,
        embedding: &[0.1, 0.2, 0.3],
        metadata: &[],
    };
    let err = core
        .insert_row(&attacker, TABLE, 1, &foreign_row, Some(&op_id))
        .expect_err("cross-tenant insert must still be forbidden");
    assert_eq!(err.wire_code(), "42501");
}
