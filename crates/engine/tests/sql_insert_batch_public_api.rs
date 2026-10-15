//! `sql::exec::execute_insert_batch`・`EngineCore::execute_bound_insert_in_session`
//! が engine クレート外から到達可能な公開 API であることと、契約（`operation_id`
//! 必須化・台帳照合・INDEX-4 上限・判定順序）を固定する結合テスト（Issue #771・
//! TASK-178・NOSQL-6）。
//!
//! `tests/sql_insert_explain_public_api.rs`（Issue #730）と同じ流儀
//! （`Storage` を直接操作し `PolicyContext::with_visibilities` で RLS 境界を確認する）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::{LedgerMode, OperationId};
use engine::row_codec::Value;
use engine::sql::exec::execute_insert_batch;
use engine::sql::parser::BoundInsert;
use engine::storage::{Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(4), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str, visibilities: impl IntoIterator<Item = Visibility>) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, visibilities).expect("valid tenant ctx")
}

fn bound(id: u64, op_id: Option<&str>) -> BoundInsert {
    BoundInsert {
        table: TABLE.to_string(),
        id,
        values: vec![
            Value::Vector(vec![id as f32, 0.0, 0.0, 0.0]),
            Value::Text("ja".to_string()),
        ],
        operation_id: op_id.map(|s| OperationId::parse(s).expect("valid operation_id")),
        volatile_default_columns: Vec::new(),
    }
}

// ---------------------------------------------------------------------
// execute_insert_batch: 到達性・複数行の単一 operation_id・再送判定
// ---------------------------------------------------------------------

#[test]
fn execute_insert_batch_writes_multiple_rows_under_one_operation_id() {
    let path = unique_db_path("sql-insert-batch-writes-multi");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let policy = ctx("tenant-a", [Visibility::Public, Visibility::Private]);

    let bounds = vec![bound(1, Some("op-1")), bound(2, Some("op-1"))];
    let outcome = execute_insert_batch(&storage, &policy, &bounds, LedgerMode::Ledgered)
        .expect("batch insert succeeds");
    assert_eq!(outcome.rows_affected, 2);
}

#[test]
fn execute_insert_batch_same_operation_id_same_content_is_duplicate() {
    let path = unique_db_path("sql-insert-batch-duplicate");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let policy = ctx("tenant-a", [Visibility::Private]);

    let bounds = vec![bound(1, Some("op-1")), bound(2, Some("op-1"))];
    execute_insert_batch(&storage, &policy, &bounds, LedgerMode::Ledgered).expect("first ok");

    // 別テーブル行 id（重複しない）だが同一 operation_id・同一内容の再送。
    let err = execute_insert_batch(&storage, &policy, &bounds, LedgerMode::Ledgered)
        .expect_err("resend must fail");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn execute_insert_batch_same_operation_id_different_content_is_mismatch() {
    let path = unique_db_path("sql-insert-batch-mismatch");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let policy = ctx("tenant-a", [Visibility::Private]);

    let first = vec![bound(1, Some("op-1")), bound(2, Some("op-1"))];
    execute_insert_batch(&storage, &policy, &first, LedgerMode::Ledgered).expect("first ok");

    let second = vec![bound(3, Some("op-1")), bound(4, Some("op-1"))];
    let err = execute_insert_batch(&storage, &policy, &second, LedgerMode::Ledgered)
        .expect_err("mismatched resend must fail");
    assert_eq!(err.wire_code(), "22023");
}

#[test]
fn execute_insert_batch_missing_operation_id_is_23502() {
    let path = unique_db_path("sql-insert-batch-missing-op-id");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let policy = ctx("tenant-a", [Visibility::Private]);

    let bounds = vec![bound(1, None), bound(2, None)];
    let err = execute_insert_batch(&storage, &policy, &bounds, LedgerMode::Ledgered)
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "23502");
}

#[test]
fn execute_insert_batch_rejects_duplicate_id_within_batch() {
    let path = unique_db_path("sql-insert-batch-dup-id-in-batch");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let policy = ctx("tenant-a", [Visibility::Private]);

    let bounds = vec![bound(1, Some("op-1")), bound(1, Some("op-1"))];
    let err = execute_insert_batch(&storage, &policy, &bounds, LedgerMode::Ledgered)
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn execute_insert_batch_rejects_mixed_table_or_operation_id() {
    let path = unique_db_path("sql-insert-batch-mixed");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let policy = ctx("tenant-a", [Visibility::Private]);

    let mut second = bound(2, Some("op-2"));
    second.table = TABLE.to_string();
    let bounds = vec![bound(1, Some("op-1")), second];
    let err = execute_insert_batch(&storage, &policy, &bounds, LedgerMode::Ledgered)
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn execute_insert_batch_rejects_empty_batch() {
    let path = unique_db_path("sql-insert-batch-empty");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let policy = ctx("tenant-a", [Visibility::Private]);

    let bounds: Vec<BoundInsert> = vec![];
    let err = execute_insert_batch(&storage, &policy, &bounds, LedgerMode::Ledgered)
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn execute_insert_batch_single_row_matches_execute_insert_ledger_space() {
    // n==1 は execute_insert へ委譲し、SQL-10 単行 INSERT と同一の台帳ハッシュ
    // 空間で再送判定される（表層横断の再送検知）。
    let path = unique_db_path("sql-insert-batch-single-row-ledger");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let policy = ctx("tenant-a", [Visibility::Private]);

    let single = vec![bound(1, Some("op-1"))];
    execute_insert_batch(&storage, &policy, &single, LedgerMode::Ledgered).expect("first ok");

    // SQL-10 側 (execute_insert) から同一 operation_id・同一内容で再送。
    let same_content = single[0].clone();
    let err =
        engine::sql::exec::execute_insert(&storage, &policy, &same_content, LedgerMode::Ledgered)
            .expect_err("cross-surface resend must fail");
    assert_eq!(err.wire_code(), "23505");
}

#[test]
fn execute_insert_batch_other_tenant_same_id_succeeds() {
    // TABLE-12・RLS-9: 他テナントが同じ行 id を保持していても本経路は成功する
    // （物理キーは (tenant_id, id) で名前空間化されているため）。
    let path = unique_db_path("sql-insert-batch-other-tenant-same-id");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");

    let tenant_a = ctx("tenant-a", [Visibility::Private]);
    let tenant_b = ctx("tenant-b", [Visibility::Private]);

    let bounds_a = vec![bound(1, Some("op-1")), bound(2, Some("op-1"))];
    execute_insert_batch(&storage, &tenant_a, &bounds_a, LedgerMode::Ledgered)
        .expect("tenant-a batch ok");

    let bounds_b = vec![bound(1, Some("op-1")), bound(2, Some("op-1"))];
    execute_insert_batch(&storage, &tenant_b, &bounds_b, LedgerMode::Ledgered)
        .expect("tenant-b batch with same ids/operation_id ok");
}

// ---------------------------------------------------------------------
// EngineCore::execute_bound_insert_in_session: 判定順序・INDEX-4 上限
// ---------------------------------------------------------------------

fn open_engine(name: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, path)
}

fn open_engine_with_limits(
    name: &str,
    limits: engine::batch_limits::BatchLimits,
) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core =
        EngineCore::from_storage(storage, Box::new(CpuScalarProvider)).with_batch_limits(limits);
    (core, path)
}

#[test]
fn execute_bound_insert_in_session_writes_rows() {
    let (core, path) = open_engine("core-insert-session-writes");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a", [Visibility::Private]);
    let op_id = OperationId::parse("op-1").expect("valid operation_id");

    let outcome = core
        .execute_bound_insert_in_session(&policy, TABLE, 2, Some(&op_id), |_schema| {
            Ok(vec![bound(1, Some("op-1")), bound(2, Some("op-1"))])
        })
        .expect("insert session ok");
    assert_eq!(outcome.rows_affected, 2);

    assert_eq!(
        core.operation_recorded(&policy, TABLE, &op_id)
            .expect("ledger lookup should succeed"),
        LedgerLookup::Recorded
    );
}

#[test]
fn execute_bound_insert_in_session_missing_operation_id_precedes_schema_lookup() {
    // `23502` の判定はテーブル不存在（`42P01`）より前に行う（判定順序が契約）。
    let (core, path) = open_engine("core-insert-session-missing-op-id-order");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a", [Visibility::Private]);

    let err = core
        .execute_bound_insert_in_session(&policy, "no_such_table", 1, None, |_schema| {
            Ok(vec![bound(1, None)])
        })
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "23502");
}

#[test]
fn execute_bound_insert_in_session_rejects_empty_batch() {
    let (core, path) = open_engine("core-insert-session-empty");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a", [Visibility::Private]);
    let op_id = OperationId::parse("op-1").expect("valid operation_id");

    let err = core
        .execute_bound_insert_in_session(&policy, TABLE, 0, Some(&op_id), |_schema| Ok(vec![]))
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "22000");
}

#[test]
fn execute_bound_insert_in_session_rejects_operation_id_mismatch_between_guard_and_bound() {
    // 判定 1（早期ガード。引数 `operation_id`）を通過した値と、`bind` closure が
    // 構築した `BoundInsert.operation_id` が異なる場合は `22000` で拒否する
    // （PR #823 Bugbot 指摘）。実書き込み（`execute_insert_batch`）は
    // `bounds[0].operation_id` を台帳キーとして再解決するため、この一致検証
    // がないと判定 1 のガードと実際の台帳キーが食い違い得る。
    let (core, path) = open_engine("core-insert-session-op-id-mismatch-guard-vs-bound");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a", [Visibility::Private]);
    let guard_op_id = OperationId::parse("op-guard").expect("valid operation_id");

    let err = core
        .execute_bound_insert_in_session(&policy, TABLE, 1, Some(&guard_op_id), |_schema| {
            Ok(vec![bound(1, Some("op-bound-differs"))])
        })
        .expect_err("must reject operation_id mismatch");
    assert_eq!(err.wire_code(), "22000");

    // 実際には書き込まれていない（いずれの operation_id 台帳にも記録されない）。
    assert_eq!(
        core.operation_recorded(&policy, TABLE, &guard_op_id)
            .expect("ledger lookup should succeed"),
        LedgerLookup::NotRecorded
    );
}

#[test]
fn execute_bound_insert_in_session_row_count_limit_precedes_bind() {
    let (core, path) = open_engine_with_limits(
        "core-insert-session-row-limit",
        engine::batch_limits::BatchLimits {
            max_files_per_batch: 1,
            ..engine::batch_limits::BatchLimits::default()
        },
    );
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a", [Visibility::Private]);
    let op_id = OperationId::parse("op-1").expect("valid operation_id");

    let err = core
        .execute_bound_insert_in_session(&policy, TABLE, 2, Some(&op_id), |_schema| {
            panic!("bind must not be called when the row count limit is exceeded")
        })
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "54000");
}

#[test]
fn execute_bound_insert_in_session_batch_total_bytes_limit_is_54000() {
    let (core, path) = open_engine_with_limits(
        "core-insert-session-total-bytes-limit",
        engine::batch_limits::BatchLimits {
            max_batch_total_bytes: 1,
            ..engine::batch_limits::BatchLimits::default()
        },
    );
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a", [Visibility::Private]);
    let op_id = OperationId::parse("op-1").expect("valid operation_id");

    let err = core
        .execute_bound_insert_in_session(&policy, TABLE, 2, Some(&op_id), |_schema| {
            Ok(vec![bound(1, Some("op-1")), bound(2, Some("op-1"))])
        })
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "54000");
}

#[test]
fn execute_bound_insert_in_session_chunk_total_limit_is_54000() {
    let (core, path) = open_engine_with_limits(
        "core-insert-session-chunk-limit",
        engine::batch_limits::BatchLimits {
            max_batch_chunks: 1,
            ..engine::batch_limits::BatchLimits::default()
        },
    );
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a", [Visibility::Private]);
    let op_id = OperationId::parse("op-1").expect("valid operation_id");

    let err = core
        .execute_bound_insert_in_session(&policy, TABLE, 2, Some(&op_id), |_schema| {
            Ok(vec![bound(1, Some("op-1")), bound(2, Some("op-1"))])
        })
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "54000");
}

#[test]
fn execute_bound_insert_in_session_over_limit_leaves_ledger_unrecorded() {
    let (core, path) = open_engine_with_limits(
        "core-insert-session-over-limit-no-partial",
        engine::batch_limits::BatchLimits {
            max_batch_chunks: 1,
            ..engine::batch_limits::BatchLimits::default()
        },
    );
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a", [Visibility::Public, Visibility::Private]);
    let op_id = OperationId::parse("op-1").expect("valid operation_id");

    let err = core
        .execute_bound_insert_in_session(&policy, TABLE, 2, Some(&op_id), |_schema| {
            Ok(vec![bound(1, Some("op-1")), bound(2, Some("op-1"))])
        })
        .expect_err("must reject");
    assert_eq!(err.wire_code(), "54000");

    // over-limit は書き込み txn に到達しないため、台帳は未記録のまま
    // （fail-closed。副作用ゼロで拒否される契約）。
    assert_eq!(
        core.operation_recorded(&policy, TABLE, &op_id)
            .expect("ledger lookup should succeed"),
        LedgerLookup::NotRecorded
    );
}

#[test]
fn execute_bound_insert_in_session_compare_only_without_ledger_accepts_none() {
    let path = unique_db_path("core-insert-session-compare-only");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(LedgerMode::CompareOnlyWithoutLedger);
    let policy = ctx("tenant-a", [Visibility::Private]);

    let outcome = core
        .execute_bound_insert_in_session(&policy, TABLE, 1, None, |_schema| {
            Ok(vec![bound(1, None)])
        })
        .expect("insert session ok");
    assert_eq!(outcome.rows_affected, 1);
}
