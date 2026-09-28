//! `VECTOR` 列を持たないテーブルへの行全体置換 UPDATE（[`RowInput`] 経由。
//! `tenant::update_row`／`core::EngineCore::update_row` の 2 入口）を受理する
//! （TABLE-1・RECOVER-4・RECOVER-10・Issue #1079）ことを固定する結合テスト。
//!
//! INSERT 系の同種契約は `tests/insert_without_vector_column.rs`（Issue #995）が
//! 固定済みで、本ファイルはその UPDATE 版に相当する。`VECTOR` 列ありスキーマの
//! 回帰は `tests/recovery_content_hash.rs`・`src/catalog.rs` の単体テストが既に
//! 固定しているため、ここでは新規に扱わない。

use engine::catalog::{CatalogError, ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant::TenantWriteError;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "notes";

/// `VECTOR` 列を持たない 2 列スキーマ（`lang`・`body`。いずれも `TEXT`。
/// `insert_without_vector_column.rs::no_vector_schema` と同一形）。
fn no_vector_schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, true),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

fn open_storage(name: &str) -> (Storage, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&no_vector_schema())
        .expect("create table without a VECTOR column");
    (storage, path)
}

fn open_engine(name: &str) -> (EngineCore, std::path::PathBuf) {
    let (storage, path) = open_storage(name);
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    (core, path)
}

fn encode_metadata(lang: &str, body: &str) -> Vec<u8> {
    engine::row_codec::encode_scalar_columns(
        &no_vector_schema(),
        &[Value::Text(lang.to_string()), Value::Text(body.to_string())],
    )
    .expect("encode scalar columns")
}

fn row_input<'a>(tenant: &'a str, metadata: &'a [u8]) -> RowInput<'a> {
    RowInput {
        tenant_id: tenant,
        visibility: Visibility::Private,
        embedding: &[],
        metadata,
    }
}

// ---------------------------------------------------------------------
// 基準①: 行全体置換 UPDATE の 2 入口が dim 0 embedding を受理する
// ---------------------------------------------------------------------

#[test]
fn tenant_update_row_accepts_empty_embedding_on_table_without_vector_column() {
    let (storage, path) = open_storage("update-no-vector-tenant");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");
    let insert_metadata = encode_metadata("ja", "a");

    engine::tenant::insert_row(
        &storage,
        TABLE,
        &policy,
        1,
        &row_input("tenant-a", &insert_metadata),
        &OperationId::parse("op-seed").expect("valid operation_id"),
    )
    .expect("seed insert must succeed");

    let update_metadata = encode_metadata("fr", "b");
    engine::tenant::update_row(
        &storage,
        TABLE,
        &policy,
        1,
        &row_input("tenant-a", &update_metadata),
        &OperationId::parse("op-update").expect("valid operation_id"),
    )
    .expect(
        "update_row must accept an empty embedding on a table without a VECTOR column \
         (Issue #1079)",
    );

    let row = storage
        .get_row_from_table(TABLE, "tenant-a", 1)
        .expect("read back row");
    assert!(row.embedding.is_empty());
    assert_eq!(row.metadata, update_metadata);
}

#[test]
fn engine_core_update_row_accepts_empty_embedding_on_table_without_vector_column() {
    let (core, path) = open_engine("update-no-vector-core");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");
    let insert_metadata = encode_metadata("ja", "a");

    core.insert_row(
        &policy,
        TABLE,
        1,
        &row_input("tenant-a", &insert_metadata),
        Some(&OperationId::parse("op-seed").expect("valid operation_id")),
    )
    .expect("seed insert must succeed");

    let update_metadata = encode_metadata("fr", "b");
    core.update_row(
        &policy,
        TABLE,
        1,
        &row_input("tenant-a", &update_metadata),
        Some(&OperationId::parse("op-update").expect("valid operation_id")),
    )
    .expect(
        "EngineCore::update_row must accept an empty embedding on a table without a \
         VECTOR column (Issue #1079)",
    );

    let sql = format!("SELECT lang, body FROM {TABLE} LIMIT 10");
    let outcome = core
        .execute_sql(&policy, &sql)
        .expect("scan a table without a VECTOR column should succeed");
    assert_eq!(outcome.rows.len(), 1);
    match &outcome.rows[0].cells[..] {
        [engine::sql::exec::Cell::Text(lang), engine::sql::exec::Cell::Text(body)] => {
            assert_eq!(lang, "fr");
            assert_eq!(body, "b");
        }
        other => panic!("unexpected cells: {other:?}"),
    }
}

// ---------------------------------------------------------------------
// 基準②: 非空 embedding は fail-closed に拒否される（存在情報を漏らさない）
// ---------------------------------------------------------------------

fn assert_rejects_non_empty_embedding(err: TenantWriteError) {
    match err {
        TenantWriteError::Catalog(CatalogError::Invalid(msg)) => {
            assert_eq!(msg, "table has no VECTOR column");
        }
        other => panic!("expected Catalog(Invalid), got {other:?}"),
    }
}

#[test]
fn tenant_update_row_rejects_non_empty_embedding_regardless_of_row_existence() {
    let (storage, path) = open_storage("update-no-vector-reject-nonempty");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");
    let insert_metadata = encode_metadata("ja", "a");

    engine::tenant::insert_row(
        &storage,
        TABLE,
        &policy,
        1,
        &row_input("tenant-a", &insert_metadata),
        &OperationId::parse("op-seed").expect("valid operation_id"),
    )
    .expect("seed insert must succeed");

    let non_empty = RowInput {
        tenant_id: "tenant-a",
        visibility: Visibility::Private,
        embedding: &[0.1, 0.2],
        metadata: &insert_metadata,
    };

    // 既存 id への非空 embedding。
    let err = engine::tenant::update_row(
        &storage,
        TABLE,
        &policy,
        1,
        &non_empty,
        &OperationId::parse("op-reject-existing").expect("valid operation_id"),
    )
    .expect_err("a non-empty embedding on a table without a VECTOR column must be rejected");
    assert_rejects_non_empty_embedding(err);

    // 既存行は変更されていないこと。
    let row = storage
        .get_row_from_table(TABLE, "tenant-a", 1)
        .expect("read back row");
    assert_eq!(row.metadata, insert_metadata);

    // 存在しない id への非空 embedding も、同じ variant・同じ文言で拒否される
    // （存在確認より前に次元検証が走るため、行の有無で応答が変わらない）。
    let err = engine::tenant::update_row(
        &storage,
        TABLE,
        &policy,
        999,
        &non_empty,
        &OperationId::parse("op-reject-missing").expect("valid operation_id"),
    )
    .expect_err("a non-empty embedding must be rejected even for a non-existent id");
    assert_rejects_non_empty_embedding(err);

    // 拒否は台帳記録より前（write txn が commit されず abort される）ため、
    // 拒否された operation_id はその後の正当な更新に再利用できる。
    let update_metadata = encode_metadata("fr", "b");
    engine::tenant::update_row(
        &storage,
        TABLE,
        &policy,
        1,
        &row_input("tenant-a", &update_metadata),
        &OperationId::parse("op-reject-existing").expect("valid operation_id"),
    )
    .expect(
        "a rejected non-empty-embedding attempt must not consume its operation_id \
         (ledger record happens after dimension validation)",
    );
}

// ---------------------------------------------------------------------
// 基準③: 既存の RLS・テナント境界・台帳契約は変わらない
// ---------------------------------------------------------------------

#[test]
fn tenant_update_row_still_enforces_tenant_and_existence_checks() {
    let (storage, path) = open_storage("update-no-vector-boundary");
    let _guard = CleanupGuard(path);
    let policy_a = ctx("tenant-a");
    let policy_b = ctx("tenant-b");
    let metadata_a = encode_metadata("ja", "a");
    let metadata_b = encode_metadata("en", "b");

    engine::tenant::insert_row(
        &storage,
        TABLE,
        &policy_a,
        1,
        &row_input("tenant-a", &metadata_a),
        &OperationId::parse("op-seed-a").expect("valid operation_id"),
    )
    .expect("tenant-a seed insert must succeed");
    engine::tenant::insert_row(
        &storage,
        TABLE,
        &policy_b,
        1,
        &row_input("tenant-b", &metadata_b),
        &OperationId::parse("op-seed-b").expect("valid operation_id"),
    )
    .expect("tenant-b seed insert must succeed");
    // tenant-a は持たない id=2 を tenant-b だけが持つ状態を作る（他テナントの
    // 存在情報が応答に漏れないことの対照。物理キーは (ctx.tenant_id(), id) で
    // 名前空間化される〔TABLE-12〕ため、id=1 のように両テナントが同じ id を
    // 使っていると policy_a の照会は常に自テナント行に構造的に閉じてしまい、
    // 「他テナントにのみ存在する id」を区別できない対照にならない）。
    engine::tenant::insert_row(
        &storage,
        TABLE,
        &policy_b,
        2,
        &row_input("tenant-b", &metadata_b),
        &OperationId::parse("op-seed-b-2").expect("valid operation_id"),
    )
    .expect("tenant-b seed insert (id=2) must succeed");

    // 存在しない id は NotFound。
    let err = engine::tenant::update_row(
        &storage,
        TABLE,
        &policy_a,
        999,
        &row_input("tenant-a", &metadata_a),
        &OperationId::parse("op-missing").expect("valid operation_id"),
    )
    .expect_err("updating a non-existent id must fail");
    assert!(matches!(err, TenantWriteError::NotFound));

    // tenant-b にのみ存在する id=2 も、tenant-a から見ると同じ NotFound
    // （区別不能。security.md P0）。
    let err_other_tenant_id = engine::tenant::update_row(
        &storage,
        TABLE,
        &policy_a,
        2,
        &row_input("tenant-a", &metadata_a),
        &OperationId::parse("op-cross-tenant").expect("valid operation_id"),
    )
    .expect_err("an id that only another tenant owns must fail as NotFound");
    assert!(matches!(err_other_tenant_id, TenantWriteError::NotFound));

    // tenant-b の行自体は変わっていない（`Storage::get_row_from_table` は
    // テナント文字列を直接引数に取る生ストレージ読み取りで、ctx のテナント境界
    // チェックは経由しない。ここでは行の値そのものが不変であることの確認）。
    let row = storage
        .get_row_from_table(TABLE, "tenant-b", 1)
        .expect("tenant-b's row must remain reachable via its own key");
    assert_eq!(row.metadata, metadata_b);

    // `row.tenant_id` が ctx と不一致なら Forbidden。
    let err = engine::tenant::update_row(
        &storage,
        TABLE,
        &policy_a,
        1,
        &row_input("tenant-b", &metadata_a),
        &OperationId::parse("op-forbidden").expect("valid operation_id"),
    )
    .expect_err("row.tenant_id mismatching ctx must be Forbidden");
    assert!(matches!(err, TenantWriteError::Forbidden));
}

#[test]
fn tenant_update_row_enforces_operation_id_ledger_contract() {
    let (storage, path) = open_storage("update-no-vector-ledger");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");
    let insert_metadata = encode_metadata("ja", "a");

    engine::tenant::insert_row(
        &storage,
        TABLE,
        &policy,
        1,
        &row_input("tenant-a", &insert_metadata),
        &OperationId::parse("op-seed").expect("valid operation_id"),
    )
    .expect("seed insert must succeed");

    let update_metadata = encode_metadata("fr", "b");
    let op_id = OperationId::parse("op-update").expect("valid operation_id");

    engine::tenant::update_row(
        &storage,
        TABLE,
        &policy,
        1,
        &row_input("tenant-a", &update_metadata),
        &op_id,
    )
    .expect("first update must succeed");

    // 同一 operation_id・同一内容の再送は重複として拒否される（RECOVER-10）。
    let err = engine::tenant::update_row(
        &storage,
        TABLE,
        &policy,
        1,
        &row_input("tenant-a", &update_metadata),
        &op_id,
    )
    .expect_err("identical resend must be rejected as a duplicate");
    assert!(matches!(err, TenantWriteError::DuplicateOperationId));

    // 同一 operation_id・異なる内容は内容不一致として拒否される。
    let mismatched_metadata = encode_metadata("de", "c");
    let err = engine::tenant::update_row(
        &storage,
        TABLE,
        &policy,
        1,
        &row_input("tenant-a", &mismatched_metadata),
        &op_id,
    )
    .expect_err("mismatched resend must be rejected");
    assert!(matches!(err, TenantWriteError::OperationIdContentMismatch));
}
