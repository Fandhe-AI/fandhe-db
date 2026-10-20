//! 行識別方式が採番方式（`RowIdentity::Allocated`）の表に対するゲートとライフサイクルの
//! 単体テスト（TABLE-23・Issue #2193）。
//!
//! 採番方式の表を作る手段は crate 内部（`TableSchema::with_row_identity`）に限られるため、
//! 結合テスト（`tests/table23_rowid_storage.rs`）ではなく本モジュールに置く。確認すること:
//! 全 DML・読み取りが `0A000` の固定文言で拒否されること（索引・検索・台帳が rowid 未対応の
//! 間の fail-closed）、拒否文面に rowid・テナント名・表名を含めないこと、`DROP TABLE`・
//! `ALTER TABLE ... RENAME TO`・`TRUNCATE` が採番状態を含めて通ること。

use crate::catalog::rowid_alloc::{allocate_in_txn, ROWID_STATE_TABLE};
use crate::catalog::{
    is_row_identity_unsupported, ColumnDef, ColumnType, DropBehavior, RowIdentity, TableSchema,
};
use crate::core::EngineCore;
use crate::kernel::CpuScalarProvider;
use crate::policy::PolicyContext;
use crate::recovery::required_op_id::OperationId;
use crate::row_codec::Value;
use crate::sql::mode::SessionState;
use crate::storage::{Storage, Visibility};
use redb::{ReadableDatabase, ReadableTableMetadata};

const TENANT: &str = "tenant-secret-name";

fn ctx() -> PolicyContext {
    PolicyContext::with_visibilities(TENANT, [Visibility::Public]).expect("ctx")
}

fn allocated_table(label: &str) -> (Storage, crate::test_util::temp_db::CleanupGuard) {
    let path = crate::test_util::temp_db::unique_db_path(label);
    let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    storage
        .create_table(
            &TableSchema::new(
                "docs",
                vec![ColumnDef::new("kind", ColumnType::Text, false)],
            )
            .with_row_identity(RowIdentity::Allocated),
        )
        .expect("create allocated table");
    (storage, guard)
}

fn assert_redacted(text: &str) {
    assert!(!text.contains(TENANT), "{text}");
    assert!(!text.chars().any(|c| c.is_ascii_digit()), "{text}");
}

fn state_rows(storage: &Storage) -> u64 {
    let txn = storage.begin_write_txn().expect("txn");
    let t = txn.open_table(ROWID_STATE_TABLE).expect("open");
    t.len().expect("len")
}

fn seed_state(storage: &Storage, tenant: &str) {
    let txn = storage.begin_write_txn().expect("txn");
    allocate_in_txn(&txn, tenant, "docs", 3).expect("alloc");
    txn.commit_raw_for_test().expect("commit");
}

#[test]
fn table23_allocated_table_schema_read_is_rejected_with_fixed_message() {
    let (storage, _g) = allocated_table("rowid-gate-schema");
    // 実在確認の `Storage::get_table_schema` は通り、行を読む relation 解決点は拒否する。
    storage
        .get_table_schema("docs")
        .expect("existence check passes");
    let read_txn = storage.db().begin_read().expect("read");
    let err = crate::catalog::get_table_schema_in_txn(&read_txn, "docs").expect_err("gated");
    assert!(is_row_identity_unsupported(&err), "{err:?}");
    assert_redacted(&err.to_string());
}

#[test]
fn table23_allocated_table_row_reads_are_rejected() {
    let (storage, _g) = allocated_table("rowid-gate-read");
    let err = storage
        .get_row_from_table("docs", TENANT, 1)
        .expect_err("gated get");
    assert!(is_row_identity_unsupported(&err), "{err:?}");
    let err = storage
        .scan_table_page("docs", None, 10)
        .expect_err("gated scan");
    assert!(is_row_identity_unsupported(&err), "{err:?}");
}

#[test]
fn table23_allocated_table_tenant_writes_are_rejected_as_0a000() {
    let (storage, _g) = allocated_table("rowid-gate-write");
    let op = OperationId::parse("op-1").expect("op");
    let err = crate::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx(),
        1,
        Visibility::Public,
        &[Value::Text("x".to_string())],
        &op,
    )
    .expect_err("gated insert");
    assert_eq!(err.wire_code(), "0A000");
    assert_redacted(&format!("{err} {err:?}"));
}

#[test]
fn table23_allocated_table_sql_is_rejected_without_leaking_values() {
    let (storage, _g) = allocated_table("rowid-gate-sql");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    for sql in [
        "SELECT id FROM docs ORDER BY id LIMIT 5",
        "INSERT INTO docs (id, kind) VALUES (1, 'x') USING OPERATION_ID 'op-a'",
        "UPDATE docs SET kind = 'y' WHERE id = 1 USING OPERATION_ID 'op-b'",
        "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'op-c'",
    ] {
        let err = core
            .execute_sql_in_session(&ctx(), &mut session, sql)
            .expect_err(sql);
        assert_ne!(err.wire_code(), "00000", "{sql}");
        let text = format!("{err} {err:?}");
        assert!(!text.contains(TENANT), "{sql}: {text}");
    }
}

#[test]
fn table23_drop_table_removes_allocator_state() {
    let (storage, _g) = allocated_table("rowid-gate-drop");
    seed_state(&storage, "t1");
    seed_state(&storage, "t2");
    assert_eq!(state_rows(&storage), 2);
    storage.drop_table("docs").expect("drop allocated table");
    assert_eq!(state_rows(&storage), 0);
}

#[test]
fn table23_rename_moves_allocator_state() {
    let (storage, _g) = allocated_table("rowid-gate-rename");
    seed_state(&storage, "t1");
    storage
        .alter_table_rename("docs", "docs2")
        .expect("rename allocated table");
    let txn = storage.begin_write_txn().expect("txn");
    let r = allocate_in_txn(&txn, "t1", "docs2", 1).expect("alloc after rename");
    assert_eq!(r.start().raw_for_state(), 4);
}

#[test]
fn table23_truncate_is_allowed_and_keeps_allocator_state() {
    let (storage, _g) = allocated_table("rowid-gate-truncate");
    seed_state(&storage, TENANT);
    let op = OperationId::parse("op-trunc").expect("op");
    let ledger = crate::recovery::required_op_id::LedgerMode::Ledgered
        .resolve(Some(&op))
        .expect("ledger");
    crate::tenant::truncate_tables_unchecked(
        crate::tenant::WriteTarget::Autocommit(&storage),
        &["docs".to_string()],
        DropBehavior::Restrict,
        &ctx(),
        ledger,
    )
    .expect("truncate allowed");
    // 再利用しない: TRUNCATE 後も次の払い出しは続きから。
    let txn = storage.begin_write_txn().expect("txn");
    let r = allocate_in_txn(&txn, TENANT, "docs", 1).expect("alloc");
    assert_eq!(r.start().raw_for_state(), 4);
}

/// DDL を許可したセッションで SQL 文を実行し、失敗は `wire_code|message` で返す。
fn run_ddl(core: &EngineCore, sql: &str) -> Result<crate::sql::SqlOutcome, String> {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(&ctx(), &mut session, sql)
        .map_err(|e| format!("{}|{e}", e.wire_code()))
}

#[test]
fn table23_sql_drop_table_works_on_allocated_table() {
    let (storage, _g) = allocated_table("rowid-gate-sql-drop");
    seed_state(&storage, TENANT);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    run_ddl(&core, "DROP TABLE docs").expect("DROP TABLE must pass");
}

#[test]
fn table23_sql_rename_works_on_allocated_table() {
    let (storage, _g) = allocated_table("rowid-gate-sql-rename");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    run_ddl(&core, "ALTER TABLE docs RENAME TO docs2").expect("RENAME must pass");
}

#[test]
fn table23_sql_truncate_works_on_allocated_table() {
    let (storage, _g) = allocated_table("rowid-gate-sql-truncate");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    run_ddl(&core, "TRUNCATE TABLE docs USING OPERATION_ID 'op-t'").expect("TRUNCATE must pass");
}

#[test]
fn table23_sql_reads_and_writes_are_rejected_as_0a000() {
    let (storage, _g) = allocated_table("rowid-gate-sql-0a000");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for sql in [
        "SELECT id FROM docs ORDER BY id LIMIT 5",
        "SELECT COUNT(*) FROM docs",
        "EXPLAIN SELECT id FROM docs ORDER BY id LIMIT 5",
        "INSERT INTO docs (id, kind) VALUES (1, 'x') USING OPERATION_ID 'op-a'",
        "UPDATE docs SET kind = 'y' WHERE id = 1 USING OPERATION_ID 'op-b'",
        "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'op-c'",
    ] {
        let err = run_ddl(&core, sql).expect_err(sql);
        assert!(err.starts_with("0A000|"), "{sql}: {err}");
        assert!(
            err.contains("row identity mode is not supported yet"),
            "{sql}"
        );
        assert!(
            !err.contains(TENANT) && !err.contains("docs"),
            "{sql}: {err}"
        );
    }
}

/// 採番状態は再オープン後も覆われ、続きの値から払い出される（クラッシュ後に重複しない）。
#[test]
fn table23_allocator_state_survives_reopen() {
    let path = crate::test_util::temp_db::unique_db_path("rowid-reopen");
    let _g = crate::test_util::temp_db::CleanupGuard(path.clone());
    {
        let storage = Storage::open(&path).expect("open");
        storage
            .create_table(
                &TableSchema::new(
                    "docs",
                    vec![ColumnDef::new("kind", ColumnType::Text, false)],
                )
                .with_row_identity(RowIdentity::Allocated),
            )
            .expect("create");
        seed_state(&storage, TENANT);
    }
    let storage = Storage::open(&path).expect("reopen");
    let txn = storage.begin_write_txn().expect("txn");
    let r = allocate_in_txn(&txn, TENANT, "docs", 1).expect("alloc");
    assert_eq!(r.start().raw_for_state(), 4);
}

/// `v15` 以外で `rowmode:` を持てる版は無い。後続の版が `rowmode:` を持つ場合は
/// `require_legacy_row_identity_read` の判定（先頭行の一致）も更新が要る。
#[test]
fn table23_only_v15_carries_rowmode_and_is_caught_by_read_gate() {
    let schema = TableSchema::new(
        "docs",
        vec![ColumnDef::new("kind", ColumnType::Text, false)],
    )
    .with_row_identity(RowIdentity::Allocated);
    let bytes = crate::catalog::encode_schema(&schema).expect("encode");
    assert!(bytes.starts_with(b"v15\n"));
}
