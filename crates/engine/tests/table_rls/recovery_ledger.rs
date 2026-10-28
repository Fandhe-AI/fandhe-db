//! テーブル単位 `operation_id` 台帳の結合テスト（TASK-93、対象ビヘイビア: RECOVER-2。
//! ポインタ: `docs/spec/05-tasks.md` TASK-93・`docs/spec/04-behavior/recovery.md`
//! RECOVER-2・`docs/spec/04-behavior/error-format.md` ERR-2・
//! `docs/spec/04-behavior/sql-surface.md` SQL-10）。
//!
//! `tests/recovery_required_op_id.rs`（TASK-92）と同じ流儀（実 `Storage` +
//! `CpuScalarProvider`、`EngineCore::from_storage`）で、SQL `INSERT` 表層・
//! `EngineCore::{insert_row, update_row, delete_row}`（wire 入口想定）の両経路から、
//! 台帳への追記が「行の書き込み/更新/削除と同一トランザクションで原子的」
//! 「テーブル単位・テナント単位でスコープが閉じる」「永続化される」ことを検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::storage::{RowInput, Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";
const OTHER_TABLE: &str = "other_docs";
const DIM: u32 = 3;

fn schema(table: &str) -> TableSchema {
    TableSchema::new(
        table,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("body", ColumnType::Text, false),
        ],
    )
}

fn open_storage_with_tables(path: &std::path::Path) -> Storage {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    storage
        .create_table(&schema(OTHER_TABLE))
        .expect("create other table");
    storage
}

fn op(id: &str) -> OperationId {
    OperationId::parse(id).expect("valid operation_id")
}

// --- T1: SQL INSERT ... USING OPERATION_ID 成功後、operation_recorded が Recorded を
//         返す。未使用の operation_id は NotRecorded。 -----------------------------

#[test]
fn t1_sql_insert_records_operation_id_and_unused_id_is_not_recorded() {
    let path = unique_db_path("ledger-t1");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_tables(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-t1'",
    )
    .expect("insert with operation_id must succeed");

    let lookup = core
        .operation_recorded(&ctx, TABLE, &op("op-t1"))
        .expect("lookup ok");
    assert_eq!(lookup, LedgerLookup::Recorded);

    let unused = core
        .operation_recorded(&ctx, TABLE, &op("op-t1-unused"))
        .expect("lookup ok");
    assert_eq!(unused, LedgerLookup::NotRecorded);
}

// --- T2: 原子性。行キー衝突（23505）で txn が破棄されると、台帳も未記録のまま。
//         update_row/delete_row の NotFound 経路でも同様。 --------------------------

#[test]
fn t2_ledger_write_is_atomic_with_row_write_failure() {
    let path = unique_db_path("ledger-t2");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_tables(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // id=1 を op-x で挿入。
    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-x'",
    )
    .expect("first insert must succeed");
    assert_eq!(
        core.operation_recorded(&ctx, TABLE, &op("op-x"))
            .expect("lookup ok"),
        LedgerLookup::Recorded
    );

    // 同じ id=1 を op-y で挿入 → 23505（行キー衝突）。op-y は未記録のまま。
    let err = core
        .execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.9,0.9,0.9]', 'b') USING OPERATION_ID 'op-y'",
        )
        .expect_err("duplicate row id must be rejected");
    assert_eq!(err.wire_code(), "23505");
    assert_eq!(
        core.operation_recorded(&ctx, TABLE, &op("op-y"))
            .expect("lookup ok"),
        LedgerLookup::NotRecorded,
        "atomicity: a row-write failure must discard the ledger entry recorded in the same txn"
    );

    // update_row の NotFound 経路: 存在しない id=999 を op-u で更新 → NotFound、op-u は未記録。
    let row = RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Private,
        embedding: &[0.1, 0.1, 0.1],
        metadata: &[],
    };
    let update_err = core
        .update_row(&ctx, TABLE, 999, &row, Some(&op("op-u")))
        .expect_err("update of missing row must fail");
    assert!(matches!(
        update_err,
        engine::tenant::TenantWriteError::NotFound
    ));
    assert_eq!(
        core.operation_recorded(&ctx, TABLE, &op("op-u"))
            .expect("lookup ok"),
        LedgerLookup::NotRecorded
    );

    // delete_row の NotFound 経路: 存在しない id=999 を op-d で削除 → NotFound、op-d は未記録。
    let delete_err = core
        .delete_row(&ctx, TABLE, 999, Some(&op("op-d")))
        .expect_err("delete of missing row must fail");
    assert!(matches!(
        delete_err,
        engine::tenant::TenantWriteError::NotFound
    ));
    assert_eq!(
        core.operation_recorded(&ctx, TABLE, &op("op-d"))
            .expect("lookup ok"),
        LedgerLookup::NotRecorded
    );
}

// --- T3: テーブル単位。documents で commit 済みの op-a を other_docs へ使うと成功し、
//         両テーブルとも Recorded（テーブル跨ぎで照合しない）。 --------------------

#[test]
fn t3_ledger_scope_is_per_table() {
    let path = unique_db_path("ledger-t3");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_tables(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-a'",
    )
    .expect("insert into documents must succeed");
    core.execute_insert_sql(
        &ctx,
        "INSERT INTO other_docs (id, embedding, body) VALUES (1, '[0.4,0.5,0.6]', 'b') USING OPERATION_ID 'op-a'",
    )
    .expect("reusing the same operation_id on a different table must succeed");

    assert_eq!(
        core.operation_recorded(&ctx, TABLE, &op("op-a"))
            .expect("lookup ok"),
        LedgerLookup::Recorded
    );
    assert_eq!(
        core.operation_recorded(&ctx, OTHER_TABLE, &op("op-a"))
            .expect("lookup ok"),
        LedgerLookup::Recorded
    );
}

// --- T4: テナント単位。tenant-b が同一テーブルへ op-a を使うと成功。tenant-c から
//         operation_recorded(op-a) は NotRecorded（他テナントのエントリを観測できない）。

#[test]
fn t4_ledger_scope_is_per_tenant() {
    let path = unique_db_path("ledger-t4");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_tables(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
    let ctx_c = PolicyContext::new("tenant-c").expect("valid tenant");

    core.execute_insert_sql(
        &ctx_a,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-a'",
    )
    .expect("tenant-a insert must succeed");
    core.execute_insert_sql(
        &ctx_b,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.4,0.5,0.6]', 'b') USING OPERATION_ID 'op-a'",
    )
    .expect("tenant-b reusing the same operation_id must succeed (different tenant namespace)");

    assert_eq!(
        core.operation_recorded(&ctx_a, TABLE, &op("op-a"))
            .expect("lookup ok"),
        LedgerLookup::Recorded
    );
    assert_eq!(
        core.operation_recorded(&ctx_b, TABLE, &op("op-a"))
            .expect("lookup ok"),
        LedgerLookup::Recorded
    );
    assert_eq!(
        core.operation_recorded(&ctx_c, TABLE, &op("op-a"))
            .expect("lookup ok"),
        LedgerLookup::NotRecorded,
        "tenant-c must not observe tenant-a/tenant-b's ledger entries"
    );
}

// --- T5: 永続性。EngineCore を drop → 同一パスを Storage::open で再オープン →
//         Recorded のまま。 ------------------------------------------------------

#[test]
fn t5_ledger_entry_survives_reopen() {
    let path = unique_db_path("ledger-t5");
    let _guard = CleanupGuard(path.clone());
    {
        let storage = open_storage_with_tables(&path);
        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
        let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
        core.execute_insert_sql(
            &ctx,
            "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-t5'",
        )
        .expect("insert must succeed");
        // `core`（および内部の `Storage`）はここで drop される。
    }

    let reopened = Storage::open(&path).expect("reopen storage");
    let core = EngineCore::from_storage(reopened, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    assert_eq!(
        core.operation_recorded(&ctx, TABLE, &op("op-t5"))
            .expect("lookup ok"),
        LedgerLookup::Recorded
    );
}

// --- T6: EngineCore::{insert_row, update_row, delete_row}（wire 入口想定）でも
//         それぞれ別 operation_id が記録される。 ----------------------------------

#[test]
fn t6_engine_core_row_apis_record_distinct_operation_ids() {
    let path = unique_db_path("ledger-t6");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_tables(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    let row = RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Private,
        embedding: &[0.1, 0.2, 0.3],
        metadata: &[],
    };
    core.insert_row(&ctx, TABLE, 1, &row, Some(&op("op-insert")))
        .expect("insert_row must succeed");
    core.update_row(&ctx, TABLE, 1, &row, Some(&op("op-update")))
        .expect("update_row must succeed");
    core.delete_row(&ctx, TABLE, 1, Some(&op("op-delete")))
        .expect("delete_row must succeed");

    for id in ["op-insert", "op-update", "op-delete"] {
        assert_eq!(
            core.operation_recorded(&ctx, TABLE, &op(id))
                .expect("lookup ok"),
            LedgerLookup::Recorded,
            "operation_id {id} must be recorded"
        );
    }
}

// --- T7: CompareOnlyWithoutLedger では operation_id 付きで書いても NoLedger
//         （台帳へ書かない）。 -----------------------------------------------------

#[test]
fn t7_compare_only_without_ledger_never_records() {
    let path = unique_db_path("ledger-t7");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_tables(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-t7'",
    )
    .expect("insert must succeed under compare-only mode too");

    assert_eq!(
        core.operation_recorded(&ctx, TABLE, &op("op-t7"))
            .expect("lookup ok"),
        LedgerLookup::NoLedger,
        "compare-only mode must never touch the ledger table"
    );
}

// --- T8: keep-first。同一 op-a で 2 回目の書き込み（別 id・別内容）後も Recorded の
//         まま（2 回目自体は内容照合ハッシュ（TASK-101・RECOVER-10。TASK-94・
//         RECOVER-3 の重複拒否契約を包含する）により `22023` で拒否される。
//         `tests/recovery_content_hash.rs` 参照。ここでは拒否後も 1 回目の記録が
//         保持されることのみ検証する）。

#[test]
fn t8_keep_first_entry_remains_recorded_after_second_use() {
    let path = unique_db_path("ledger-t8");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage_with_tables(&path);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (1, '[0.1,0.2,0.3]', 'a') USING OPERATION_ID 'op-a'",
    )
    .expect("first insert must succeed");
    assert_eq!(
        core.operation_recorded(&ctx, TABLE, &op("op-a"))
            .expect("lookup ok"),
        LedgerLookup::Recorded
    );

    // 2 回目の書き込み（別 id・別内容）。内容照合ハッシュ（TASK-101・RECOVER-10）に
    // より `22023` で拒否される（`tests/recovery_content_hash.rs` が拒否そのものを
    // 検証する）。本テストは keep-first 契約（拒否後も 1 回目の記録が保持されること）
    // のみを検証するため、成否は断定せず結果を握り潰す。
    let _ = core.execute_insert_sql(
        &ctx,
        "INSERT INTO documents (id, embedding, body) VALUES (2, '[0.4,0.5,0.6]', 'c') USING OPERATION_ID 'op-a'",
    );

    assert_eq!(
        core.operation_recorded(&ctx, TABLE, &op("op-a"))
            .expect("lookup ok"),
        LedgerLookup::Recorded,
        "keep-first: the ledger entry must remain recorded regardless of the second use's outcome"
    );
}
