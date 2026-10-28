//! 障害回復の 2 経路回復の結合テスト（TASK-98、対象ビヘイビア: RECOVER-7。ポインタ:
//! `docs/spec/05-tasks.md` TASK-98・`docs/spec/04-behavior/recovery.md` RECOVER-7。
//! 契約の詳細は spec 参照）。
//!
//! `tests/recovery_ledger.rs`（TASK-93）・`tests/recovery_content_hash.rs`（TASK-101）
//! と同じ流儀（実 `Storage` + `CpuScalarProvider`、`EngineCore::from_storage`）で、
//! 同一内容の `operation_id` 再送（`23505`）と [`EngineCore::last_operation_id`]
//! による照会の 2 経路を検証する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LastOperationLookup;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::storage::{RowInput, Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "documents";
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

fn new_core(path: &std::path::Path) -> EngineCore {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn op(id: &str) -> OperationId {
    OperationId::parse(id).expect("valid operation_id")
}

fn row<'a>(embedding: &'a [f32], metadata: &'a [u8]) -> RowInput<'a> {
    RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Public,
        embedding,
        metadata,
    }
}

// --- B1: 再送経路。成功 commit 後、別セッション相当の後続 commit（別 operation_id）
//         を挟んでも、同一内容の再送は 23505 のまま検出できる。 -----------------

#[test]
fn b1_resend_detection_is_unaffected_by_subsequent_commits() {
    let path = unique_db_path("two-path-b1");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.insert_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.1, 0.2, 0.3], b"a"),
        Some(&op("op-b1")),
    )
    .expect("first insert must succeed");

    // 別セッション相当の後続 commit（別 id・別 operation_id）を挟む。
    core.insert_row(
        &ctx,
        TABLE,
        2,
        &row(&[0.4, 0.5, 0.6], b"b"),
        Some(&op("op-b1-later")),
    )
    .expect("later insert must succeed");

    // 同一内容の再送は、後続 commit を挟んでもなお 23505 = commit 済み確定。
    let err = core
        .insert_row(
            &ctx,
            TABLE,
            1,
            &row(&[0.1, 0.2, 0.3], b"a"),
            Some(&op("op-b1")),
        )
        .expect_err("same content resend must still be detected");
    assert_eq!(err.wire_code(), "23505");
}

// --- B2: 未 commit 側。commit されなかった operation_id の再送は通常実行（成功）
//         ＝未 commit と確定できる。 --------------------------------------------

#[test]
fn b2_unused_operation_id_executes_normally_confirming_not_committed() {
    let path = unique_db_path("two-path-b2");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    // op-b2 はまだ一度も使われていない。再送のつもりで送っても通常の新規挿入として
    // 成功する＝当該 operation_id の書き込みはこれまで commit されていなかったと
    // 確定できる。
    core.insert_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.1, 0.2, 0.3], b"a"),
        Some(&op("op-b2")),
    )
    .expect("first use of a fresh operation_id must succeed normally");

    assert_eq!(
        core.last_operation_id(&ctx, TABLE).expect("lookup ok"),
        LastOperationLookup::Committed(op("op-b2"))
    );
}

// --- B3: 照会経路。成功 commit 直後は last_operation_id が当該 ID を返す。同一
//         テーブルへの後続 commit 後は新しい ID に置き換わる。 -------------------

#[test]
fn b3_last_operation_id_is_advisory_and_replaced_by_later_commit() {
    let path = unique_db_path("two-path-b3");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.insert_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.1, 0.2, 0.3], b"a"),
        Some(&op("op-b3-first")),
    )
    .expect("first insert must succeed");
    assert_eq!(
        core.last_operation_id(&ctx, TABLE).expect("lookup ok"),
        LastOperationLookup::Committed(op("op-b3-first")),
        "immediately after commit, last_operation_id must return the just-committed id"
    );

    // 同一テーブルへの後続 commit。
    core.insert_row(
        &ctx,
        TABLE,
        2,
        &row(&[0.4, 0.5, 0.6], b"b"),
        Some(&op("op-b3-second")),
    )
    .expect("second insert must succeed");

    // 照会結果は最新値に置き換わり、op-b3-first の成否確定にはもう使えない。
    assert_eq!(
        core.last_operation_id(&ctx, TABLE).expect("lookup ok"),
        LastOperationLookup::Committed(op("op-b3-second")),
        "a later commit on the same table must replace the advisory last_operation_id value"
    );
}

// --- B4: テナント境界。tenant-B の commit は tenant-A の last_operation_id に影響
//         しない・tenant-A から tenant-B の値が観測できない（RLS-9 原則）。 -------

#[test]
fn b4_last_operation_id_is_isolated_per_tenant() {
    let path = unique_db_path("two-path-b4");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");

    core.insert_row(
        &ctx_a,
        TABLE,
        1,
        &RowInput {
            tenant_id: "tenant-a",
            ..row(&[0.1, 0.2, 0.3], b"a")
        },
        Some(&op("op-b4-a")),
    )
    .expect("tenant-a insert must succeed");

    // tenant-a 側の照会は tenant-a 自身の値を返す。
    assert_eq!(
        core.last_operation_id(&ctx_a, TABLE).expect("lookup ok"),
        LastOperationLookup::Committed(op("op-b4-a"))
    );

    // tenant-b はまだ何も書いていないため NotFound（tenant-a の値が漏れ出さない）。
    assert_eq!(
        core.last_operation_id(&ctx_b, TABLE).expect("lookup ok"),
        LastOperationLookup::NotFound,
        "tenant-b must not observe tenant-a's last_operation_id"
    );

    core.insert_row(
        &ctx_b,
        TABLE,
        1,
        &RowInput {
            tenant_id: "tenant-b",
            ..row(&[0.7, 0.8, 0.9], b"c")
        },
        Some(&op("op-b4-b")),
    )
    .expect("tenant-b insert must succeed");

    // tenant-b の commit は tenant-a の照会結果に影響しない。
    assert_eq!(
        core.last_operation_id(&ctx_a, TABLE).expect("lookup ok"),
        LastOperationLookup::Committed(op("op-b4-a")),
        "tenant-b's commit must not affect tenant-a's last_operation_id"
    );
    assert_eq!(
        core.last_operation_id(&ctx_b, TABLE).expect("lookup ok"),
        LastOperationLookup::Committed(op("op-b4-b"))
    );
}

// --- B5: CompareOnlyWithoutLedger では照会は NoLedger を返す（NotFound へ丸めない）。

#[test]
fn b5_compare_only_without_ledger_returns_no_ledger() {
    let path = unique_db_path("two-path-b5");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema(TABLE)).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.insert_row(&ctx, TABLE, 1, &row(&[0.1, 0.2, 0.3], b"a"), None)
        .expect("insert without operation_id must succeed under CompareOnlyWithoutLedger");

    assert_eq!(
        core.last_operation_id(&ctx, TABLE).expect("lookup ok"),
        LastOperationLookup::NoLedger,
        "a ledger-less configuration must not be observed as NotFound"
    );
}

// --- B6: UPDATE/DELETE 経路。行更新・削除の commit でも last_op が置き換わる
//         （全書き込み経路の一般化）。 -----------------------------------------

#[test]
fn b6_last_operation_id_is_updated_by_update_and_delete_paths() {
    let path = unique_db_path("two-path-b6");
    let _guard = CleanupGuard(path.clone());
    let core = new_core(&path);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    core.insert_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.1, 0.2, 0.3], b"a"),
        Some(&op("op-b6-insert")),
    )
    .expect("insert must succeed");
    assert_eq!(
        core.last_operation_id(&ctx, TABLE).expect("lookup ok"),
        LastOperationLookup::Committed(op("op-b6-insert"))
    );

    core.update_row(
        &ctx,
        TABLE,
        1,
        &row(&[0.4, 0.5, 0.6], b"updated"),
        Some(&op("op-b6-update")),
    )
    .expect("update must succeed");
    assert_eq!(
        core.last_operation_id(&ctx, TABLE).expect("lookup ok"),
        LastOperationLookup::Committed(op("op-b6-update")),
        "a committed UPDATE must advance last_operation_id"
    );

    core.delete_row(&ctx, TABLE, 1, Some(&op("op-b6-delete")))
        .expect("delete must succeed");
    assert_eq!(
        core.last_operation_id(&ctx, TABLE).expect("lookup ok"),
        LastOperationLookup::Committed(op("op-b6-delete")),
        "a committed DELETE must advance last_operation_id"
    );
}
