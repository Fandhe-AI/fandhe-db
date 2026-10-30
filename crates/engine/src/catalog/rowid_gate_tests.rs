//! 行識別方式が採番方式（`RowIdentity::Allocated`）の表に対するゲートとライフサイクルの
//! 単体テスト（TABLE-23・Issue #2193・#2199）。
//!
//! 採番方式の表を Rust API で作る手段は crate 内部（`TableSchema::with_row_identity`）に限られるため、
//! 結合テスト（`tests/table23_rowid_storage.rs`）ではなく本モジュールに置く。確認すること:
//! 利用者が行 id を指定する Rust API（行の取得・走査・id 指定の書き込み・検索）が `0A000` の固定文言で
//! 拒否されること（rowid を `id` として見せない fail-closed）、拒否文面に rowid・テナント名・表名を含めない
//! こと、SQL 表層の読み書き（暗黙の `id` を持たない表として扱う）が通ること、`DROP TABLE`・
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
fn table23_allocated_table_schema_read_is_allowed_for_sql_relation_resolution() {
    let (storage, _g) = allocated_table("rowid-gate-schema");
    // SQL 表層の relation 解決点（行を読む前のスキーマ取得）は採番方式の表も通す（Issue #2199）。
    storage
        .get_table_schema("docs")
        .expect("existence check passes");
    let read_txn = storage.db().begin_read().expect("read");
    let schema = crate::catalog::get_table_schema_in_txn(&read_txn, "docs").expect("resolved");
    assert!(schema.is_allocated());
    assert!(!schema.has_pseudo_id());
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
        false,
        &ctx(),
        ledger,
        None,
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
fn table23_sql_reads_and_writes_treat_the_table_as_having_no_implicit_id() {
    let (storage, _g) = allocated_table("rowid-gate-sql");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    // 通常の文は通る（暗黙の `id` を持たない表）。
    for sql in [
        "INSERT INTO docs (kind) VALUES ('x') USING OPERATION_ID 'op-a'",
        "SELECT COUNT(*) FROM docs",
        "SELECT kind FROM docs LIMIT 5",
        "UPDATE docs SET kind = 'y' WHERE kind = 'x' USING OPERATION_ID 'op-b'",
        "DELETE FROM docs WHERE kind = 'y' USING OPERATION_ID 'op-c'",
    ] {
        run_ddl(&core, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    // 宣言の無い `id` は未定義列（`42703`）。拒否文面にテナント名・表名を含めない。
    for sql in [
        "SELECT id FROM docs ORDER BY id LIMIT 5",
        "EXPLAIN SELECT id FROM docs ORDER BY id LIMIT 5",
        "INSERT INTO docs (id, kind) VALUES (1, 'x') USING OPERATION_ID 'op-d'",
        "UPDATE docs SET kind = 'y' WHERE id = 1 USING OPERATION_ID 'op-e'",
        "DELETE FROM docs WHERE id = 1 USING OPERATION_ID 'op-f'",
    ] {
        let err = run_ddl(&core, sql).expect_err(sql);
        assert!(err.starts_with("42703|"), "{sql}: {err}");
        assert!(!err.contains(TENANT), "{sql}: {err}");
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

/// 採番方式の親を `id` で参照する FK は、DDL 段で `0A000` の固定文言により拒否される
/// （`id` の値から親行を引く解決が従来方式の表でしか成立しないため。親が宣言した `id` 列を持つ場合も、
/// 永続化する参照先列名が物理キーの `id` 参照と区別できないため未対応。Issue #2194・#2199・TABLE-23）。
/// 主キーを持たない採番方式の親への列省略 FK は PG と同じ `42830`（`InvalidForeignKey`）。
/// 列参照 FK（`id` 以外の主キー・UNIQUE を参照）は本ゲートの対象外で、このテストでは扱わない。
#[test]
fn table23_id_reference_foreign_key_to_allocated_parent_is_rejected() {
    use crate::catalog::{CatalogError, ForeignKeyDef, ReferentialAction};
    let (storage, _guard) = allocated_table("table23-fk-id-ref-allocated");
    let child = |parent_columns: Vec<String>| {
        TableSchema::new(
            "kids",
            vec![ColumnDef::new("pid", ColumnType::BigInt, true)],
        )
        .with_foreign_keys(vec![ForeignKeyDef::new(
            vec!["pid".to_string()],
            "docs".to_string(),
            parent_columns,
            ReferentialAction::NoAction,
            ReferentialAction::NoAction,
        )])
    };
    // 主キーの無い採番方式の親: 列省略は参照先を決められない。
    let err = storage
        .create_table(&child(Vec::new()))
        .expect_err("omitted columns need a primary key on the allocated parent");
    assert!(matches!(err, CatalogError::InvalidForeignKey(_)), "{err:?}");
    assert_redacted(&err.to_string());
    // 宣言した `id` 列（主キー）を持つ採番方式の親への `id` 参照は未対応の固定文言。
    let path = crate::test_util::temp_db::unique_db_path("table23-fk-id-column-allocated");
    let _guard2 = crate::test_util::temp_db::CleanupGuard(path.clone());
    let storage2 = Storage::open(&path).expect("open");
    storage2
        .create_table(
            &TableSchema::new(
                "docs",
                vec![
                    ColumnDef::new("id", ColumnType::BigInt, false),
                    ColumnDef::new("kind", ColumnType::Text, false),
                ],
            )
            .with_primary_key(vec!["id".to_string()])
            .with_row_identity(RowIdentity::Allocated),
        )
        .expect("create allocated parent with a declared id column");
    let err = storage2
        .create_table(&child(vec!["id".to_string()]))
        .expect_err("id reference to an allocated parent must be rejected");
    assert!(is_row_identity_unsupported(&err), "{err:?}");
    assert_redacted(&err.to_string());
}

/// 採番方式かつベクトル列を持つ表（検索入口のゲート確認用）。
fn allocated_vector_table(label: &str) -> (Storage, crate::test_util::temp_db::CleanupGuard) {
    let path = crate::test_util::temp_db::unique_db_path(label);
    let guard = crate::test_util::temp_db::CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open");
    storage
        .create_table(
            &TableSchema::new(
                "docs",
                vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
            )
            .with_row_identity(RowIdentity::Allocated),
        )
        .expect("create allocated vector table");
    (storage, guard)
}

/// 検索入口: SQL の kNN は採番方式の表も扱い（応答は投影した列だけで rowid を含まない）、Rust API の検索は
/// rowid を `SearchHit.id` として露出させないため `0A000` の固定文言で拒否される（TABLE-23 の非露出・
/// RLS-9）。拒否文面にテナント名・表名を含めない。
#[test]
fn table23_allocated_table_search_entrances_follow_the_exposure_contract() {
    use crate::core::VectorCore;
    let (storage, _g) = allocated_vector_table("rowid-gate-search");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut session = SessionState::default();
    for sql in [
        "SELECT embedding FROM docs ORDER BY embedding <=> '[0,0]' LIMIT 3",
        "EXPLAIN SELECT embedding FROM docs ORDER BY embedding <=> '[0,0]' LIMIT 3",
    ] {
        core.execute_sql_in_session(&ctx(), &mut session, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
    let err = core
        .search(&ctx(), "docs", &[0.0, 0.0], 3)
        .expect_err("rust api search is gated");
    let text = format!("{err} {err:?}");
    assert!(
        text.contains("row identity mode is not supported yet"),
        "{text}"
    );
    assert!(!text.contains(TENANT) && !text.contains("docs"), "{text}");
}

/// 分割実行 DML は採番方式の表でも通る（Issue #2199。カーソルは rowid 型で、応答・エラーに rowid が出ない）。
#[test]
fn table23_allocated_table_partitioned_dml_runs() {
    let (storage, _g) = allocated_table("rowid-gate-partitioned");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    for i in 0..5 {
        run_ddl(
            &core,
            &format!("INSERT INTO docs (kind) VALUES ('k{i}') USING OPERATION_ID 'ins-{i}'"),
        )
        .expect("insert");
    }
    for sql in [
        "UPDATE docs SET kind = 'y' WHERE kind = 'k1' USING OPERATION_ID 'pp-a' PARTITIONED CHUNK 2",
        "DELETE FROM docs WHERE kind = 'y' USING OPERATION_ID 'pp-b' PARTITIONED CHUNK 2",
    ] {
        run_ddl(&core, sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
    }
}
