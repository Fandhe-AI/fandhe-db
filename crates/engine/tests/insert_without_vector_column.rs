//! `VECTOR` 列を持たないテーブルへの INSERT 系書き込みを受理する（Issue #995・
//! ポインタ: TABLE-12・SQL-10・SQL-16・SQL-20・#899）ことを固定する結合テスト。
//!
//! 対象は行形 `INSERT`（単一行・複数行 `VALUES`）・型付き行挿入
//! （[`engine::tenant::insert_row`]/[`engine::tenant::insert_rows`]/
//! [`engine::tenant::insert_typed_row`]）・`ON CONFLICT` UPSERT（`DO NOTHING`／
//! `DO UPDATE`）の全 INSERT 系入口。ファイル形 `INSERT`（`path`/`body` 列必須。
//! `sql::parser::bind_file_insert` が束縛時点で `VECTOR` 列必須を検証するため
//! 到達しない）は対象外。
//!
//! 受け入れ基準（本 Issue の受け入れ条件・要約）:
//! 1. `VECTOR` 列を持たないスキーマでは、INSERT 系の全入口が正当な行を受理する
//! 2. `VECTOR` 列を持つスキーマの次元検証・エラー分類（`wire_code`）は変わらない
//! 3. 行エンコード・読み取り経路が空の embedding を正しく扱う
//! 4. RLS・テナント境界・`operation_id` 台帳の契約は変わらない

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::ledger::LedgerLookup;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant::TenantWriteError;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "notes";

/// `VECTOR` 列を持たない 2 列スキーマ（`lang`・`body`。いずれも `TEXT`）。
fn no_vector_schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![
            ColumnDef::new("lang", ColumnType::Text, false),
            ColumnDef::new("body", ColumnType::Text, true),
        ],
    )
}

/// `Public`／`Private` を両方見せる ctx（SQL `INSERT` は常に `Visibility::
/// Private` で書き込むため。`sql::exec::execute_insert_with_schema` 参照）。
fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant ctx")
}

fn open_engine(name: &str) -> (EngineCore, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&no_vector_schema())
        .expect("create table without a VECTOR column");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
    (core, path)
}

fn read_back_ids(core: &EngineCore, tenant: &str) -> Vec<u64> {
    let policy = ctx(tenant);
    let sql = format!("SELECT id FROM {TABLE} LIMIT 100");
    let outcome = core
        .execute_sql(&policy, &sql)
        .expect("scan a table without a VECTOR column should succeed");
    outcome.rows.iter().map(|row| row.id).collect()
}

// ---------------------------------------------------------------------
// 基準①: INSERT 系の全入口が受理する
// ---------------------------------------------------------------------

/// [`Storage`] を単体で開く（`EngineCore` を経由しない `tenant::*` 直接呼び出し
/// 専用。`tests/sql_update_single_row.rs` の
/// `update_against_row_with_corrupt_stored_metadata_is_rejected_with_xx000` と
/// 同じ流儀で、`Storage` は `EngineCore::from_storage` へ渡すと所有権が移るため、
/// `tenant::*` への生アクセスが必要なテストは `EngineCore` を作らず `Storage`
/// を直接読み書きする）。
fn open_storage(name: &str) -> (Storage, std::path::PathBuf) {
    let path = unique_db_path(name);
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&no_vector_schema())
        .expect("create table without a VECTOR column");
    (storage, path)
}

#[test]
fn tenant_insert_row_accepts_empty_embedding_on_table_without_vector_column() {
    let (storage, path) = open_storage("insert-no-vector-row");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    engine::tenant::insert_row(
        &storage,
        TABLE,
        &policy,
        1,
        &RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Private,
            embedding: &[],
            metadata: &engine::row_codec::encode_scalar_columns(
                &no_vector_schema(),
                &[Value::Text("ja".to_string()), Value::Text("a".to_string())],
            )
            .expect("encode scalar columns"),
        },
        &OperationId::parse("op-row-1").expect("valid operation_id"),
    )
    .expect("insert_row must accept an empty embedding on a table without a VECTOR column");

    let row = storage
        .get_row_from_table(TABLE, "tenant-a", 1)
        .expect("read back row");
    assert!(
        row.embedding.is_empty(),
        "embedding must round-trip as empty, got {:?}",
        row.embedding
    );
}

#[test]
fn tenant_insert_rows_batch_accepts_empty_embeddings_on_table_without_vector_column() {
    let (storage, path) = open_storage("insert-no-vector-rows-batch");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");
    let schema = no_vector_schema();

    let metadata_1 = engine::row_codec::encode_scalar_columns(
        &schema,
        &[Value::Text("ja".to_string()), Value::Text("a".to_string())],
    )
    .expect("encode scalar columns");
    let metadata_2 = engine::row_codec::encode_scalar_columns(
        &schema,
        &[Value::Text("en".to_string()), Value::Text("b".to_string())],
    )
    .expect("encode scalar columns");
    let rows = [
        (
            1u64,
            RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Private,
                embedding: &[],
                metadata: &metadata_1,
            },
        ),
        (
            2u64,
            RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Private,
                embedding: &[],
                metadata: &metadata_2,
            },
        ),
    ];

    engine::tenant::insert_rows(
        &storage,
        TABLE,
        &policy,
        &rows,
        &OperationId::parse("op-rows-batch-1").expect("valid operation_id"),
    )
    .expect("insert_rows must accept empty embeddings on a table without a VECTOR column");

    for id in [1u64, 2u64] {
        let row = storage
            .get_row_from_table(TABLE, "tenant-a", id)
            .unwrap_or_else(|e| panic!("read back row {id}: {e:?}"));
        assert!(row.embedding.is_empty());
    }
}

#[test]
fn tenant_insert_typed_row_accepts_table_without_vector_column() {
    let (storage, path) = open_storage("insert-no-vector-typed-row");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    engine::tenant::insert_typed_row(
        &storage,
        TABLE,
        &policy,
        1,
        Visibility::Private,
        &[Value::Text("ja".to_string()), Value::Text("a".to_string())],
        &OperationId::parse("op-typed-1").expect("valid operation_id"),
    )
    .expect("insert_typed_row must accept a table without a VECTOR column");

    let row = storage
        .get_row_from_table(TABLE, "tenant-a", 1)
        .expect("read back row");
    assert!(row.embedding.is_empty());
}

#[test]
fn sql_single_row_insert_accepts_table_without_vector_column() {
    let (core, path) = open_engine("insert-no-vector-sql-single");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let outcome = core
        .execute_insert_sql(
            &policy,
            &format!(
                "INSERT INTO {TABLE} (id, lang, body) VALUES (1, 'ja', 'a') \
                 USING OPERATION_ID 'op-sql-single'"
            ),
        )
        .expect("single-row INSERT into a table without a VECTOR column should succeed");
    assert_eq!(outcome.rows_affected, 1);
    assert_eq!(read_back_ids(&core, "tenant-a"), vec![1]);
}

#[test]
fn sql_multi_row_insert_accepts_table_without_vector_column() {
    let (core, path) = open_engine("insert-no-vector-sql-multi");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let outcome = core
        .execute_insert_sql(
            &policy,
            &format!(
                "INSERT INTO {TABLE} (id, lang, body) VALUES \
                 (1, 'ja', 'a'), (2, 'en', 'b') USING OPERATION_ID 'op-sql-multi'"
            ),
        )
        .expect("multi-row INSERT into a table without a VECTOR column should succeed");
    assert_eq!(outcome.rows_affected, 2);
    let mut ids = read_back_ids(&core, "tenant-a");
    ids.sort_unstable();
    assert_eq!(ids, vec![1, 2]);
}

#[test]
fn sql_upsert_do_nothing_and_do_update_accept_table_without_vector_column() {
    let (core, path) = open_engine("insert-no-vector-upsert");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    // 新規挿入。
    let outcome = core
        .execute_insert_sql(
            &policy,
            &format!(
                "INSERT INTO {TABLE} (id, lang, body) VALUES (1, 'ja', 'a') \
                 ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-1'"
            ),
        )
        .expect("UPSERT insert path must accept a table without a VECTOR column");
    assert_eq!(outcome.rows_affected, 1);

    // 衝突・DO NOTHING（変更なし）。
    let outcome = core
        .execute_insert_sql(
            &policy,
            &format!(
                "INSERT INTO {TABLE} (id, lang, body) VALUES (1, 'fr', 'z') \
                 ON CONFLICT (id) DO NOTHING USING OPERATION_ID 'op-upsert-2'"
            ),
        )
        .expect("UPSERT DO NOTHING must accept a table without a VECTOR column");
    assert_eq!(outcome.rows_affected, 0);

    // 衝突・DO UPDATE（`lang` を更新）。
    let outcome = core
        .execute_insert_sql(
            &policy,
            &format!(
                "INSERT INTO {TABLE} (id, lang, body) VALUES (1, 'fr', 'z') \
                 ON CONFLICT (id) DO UPDATE SET lang = EXCLUDED.lang \
                 USING OPERATION_ID 'op-upsert-3'"
            ),
        )
        .expect("UPSERT DO UPDATE must accept a table without a VECTOR column");
    assert_eq!(outcome.rows_affected, 1);

    let result = core
        .execute_sql(&policy, &format!("SELECT lang, body FROM {TABLE} LIMIT 10"))
        .expect("read back upserted row");
    assert_eq!(result.rows.len(), 1);
    match &result.rows[0].cells[..] {
        [Cell::Text(lang), Cell::Text(body)] => {
            assert_eq!(lang, "fr");
            assert_eq!(body, "a", "DO UPDATE must only touch the SET column");
        }
        other => panic!("unexpected cells: {other:?}"),
    }
}

// ---------------------------------------------------------------------
// fail-closed 側: 非空 embedding は引き続き拒否される
// ---------------------------------------------------------------------

#[test]
fn tenant_insert_row_still_rejects_non_empty_embedding_on_table_without_vector_column() {
    let (storage, path) = open_storage("insert-no-vector-reject-nonempty");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let err = engine::tenant::insert_row(
        &storage,
        TABLE,
        &policy,
        1,
        &RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Private,
            embedding: &[0.1, 0.2],
            metadata: &engine::row_codec::encode_scalar_columns(
                &no_vector_schema(),
                &[Value::Text("ja".to_string()), Value::Text("a".to_string())],
            )
            .expect("encode scalar columns"),
        },
        &OperationId::parse("op-reject-1").expect("valid operation_id"),
    )
    .expect_err("a non-empty embedding on a table without a VECTOR column must be rejected");
    assert!(matches!(
        err,
        TenantWriteError::Catalog(engine::catalog::CatalogError::Invalid(_))
    ));
    let (rows, _cursor) = storage
        .scan_table_page(TABLE, None, 10)
        .expect("scan table");
    assert!(rows.is_empty());
}

// ---------------------------------------------------------------------
// 基準②: VECTOR 列を持つスキーマの次元検証・エラー分類は変わらない
// ---------------------------------------------------------------------

#[test]
fn sql_insert_dim_mismatch_and_missing_value_on_table_with_vector_column_are_unchanged() {
    let path = unique_db_path("insert-with-vector-unchanged");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("lang", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_ledger_mode(engine::recovery::required_op_id::LedgerMode::Ledgered);
    let policy = ctx("tenant-a");

    // 次元不一致は 22000。
    let err = core
        .execute_insert_sql(
            &policy,
            "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2,0.3]', 'ja') \
             USING OPERATION_ID 'op-dim-mismatch'",
        )
        .expect_err("embedding dim mismatch must still be rejected");
    assert_eq!(err.wire_code(), "22000");
    let result = core
        .execute_sql(&policy, "SELECT id FROM docs LIMIT 10")
        .expect("scan docs table");
    assert!(result.rows.is_empty());

    // 必須（非 nullable）の `VECTOR` 列の値が欠けた INSERT は `23502`
    // （`NotNullViolation`。TABLE-16・TASK-204、Issue #904。旧 `22000` から
    // 契約変更）。
    let err = core
        .execute_insert_sql(
            &policy,
            "INSERT INTO docs (id, lang) VALUES (1, 'ja') USING OPERATION_ID 'op-missing-vector'",
        )
        .expect_err("a missing NOT NULL VECTOR column value must still be rejected");
    assert_eq!(err.wire_code(), "23502");
    let result = core
        .execute_sql(&policy, "SELECT id FROM docs LIMIT 10")
        .expect("scan docs table");
    assert!(result.rows.is_empty());

    // 正しい次元は成功する（VECTOR 列ありテーブルの既存挙動が不変であることの
    // 対照）。
    let outcome = core
        .execute_insert_sql(
            &policy,
            "INSERT INTO docs (id, embedding, lang) VALUES (1, '[0.1,0.2]', 'ja') \
             USING OPERATION_ID 'op-dim-ok'",
        )
        .expect("matching embedding dim must still succeed");
    assert_eq!(outcome.rows_affected, 1);
}

// ---------------------------------------------------------------------
// 基準③: 空 embedding の読み取り
// ---------------------------------------------------------------------

#[test]
fn count_and_group_by_on_table_without_vector_column_after_insert() {
    let (core, path) = open_engine("insert-no-vector-aggregate");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    for (id, lang, op_id) in [
        (1u64, "ja", "op-agg-1"),
        (2u64, "en", "op-agg-2"),
        (3u64, "ja", "op-agg-3"),
    ] {
        core.execute_insert_sql(
            &policy,
            &format!(
                "INSERT INTO {TABLE} (id, lang, body) VALUES ({id}, '{lang}', 'x') \
                 USING OPERATION_ID '{op_id}'"
            ),
        )
        .expect("insert into a table without a VECTOR column should succeed");
    }

    let result = core
        .execute_sql(&policy, &format!("SELECT COUNT(*) FROM {TABLE}"))
        .expect("COUNT(*) on a table without a VECTOR column should succeed");
    match &result.rows[0].cells[..] {
        [Cell::Integer(n)] => assert_eq!(*n, 3),
        other => panic!("unexpected cells: {other:?}"),
    }

    let result = core
        .execute_sql(
            &policy,
            &format!("SELECT lang, COUNT(*) FROM {TABLE} GROUP BY lang"),
        )
        .expect("GROUP BY on a table without a VECTOR column should succeed");
    let mut rows: Vec<(String, u64)> = result
        .rows
        .iter()
        .map(|row| match &row.cells[..] {
            [Cell::Text(lang), Cell::Integer(n)] => (lang.clone(), *n),
            other => panic!("unexpected cells: {other:?}"),
        })
        .collect();
    rows.sort();
    assert_eq!(rows, vec![("en".to_string(), 1), ("ja".to_string(), 2)]);
}

#[test]
fn update_and_delete_on_table_without_vector_column_after_insert() {
    let (core, path) = open_engine("insert-no-vector-update-delete");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    core.execute_insert_sql(
        &policy,
        &format!(
            "INSERT INTO {TABLE} (id, lang, body) VALUES (1, 'ja', 'a') \
             USING OPERATION_ID 'op-ud-insert'"
        ),
    )
    .expect("insert into a table without a VECTOR column should succeed");

    // 単一行 UPDATE（TEXT 列のみ）。
    let outcome = core
        .execute_update_sql(
            &policy,
            &format!(
                "UPDATE {TABLE} SET lang = 'fr' WHERE id = 1 USING OPERATION_ID 'op-ud-update'"
            ),
        )
        .expect("single-row UPDATE on a table without a VECTOR column should succeed");
    assert_eq!(outcome.rows_affected, 1);

    // DELETE（単一行）。
    let outcome = core
        .execute_delete_sql(
            &policy,
            &format!("DELETE FROM {TABLE} WHERE id = 1 USING OPERATION_ID 'op-ud-delete'"),
        )
        .expect("DELETE on a table without a VECTOR column should succeed");
    assert_eq!(outcome.rows_affected, 1);
    assert!(read_back_ids(&core, "tenant-a").is_empty());
}

#[test]
fn distance_search_on_table_without_vector_column_is_still_rejected_with_22000() {
    let (core, path) = open_engine("insert-no-vector-distance-rejected");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    core.execute_insert_sql(
        &policy,
        &format!(
            "INSERT INTO {TABLE} (id, lang, body) VALUES (1, 'ja', 'a') \
             USING OPERATION_ID 'op-distance-seed'"
        ),
    )
    .expect("insert into a table without a VECTOR column should succeed");

    let err = core
        .execute_sql(
            &policy,
            &format!("SELECT id FROM {TABLE} ORDER BY lang <=> '[0.1,0.2]' LIMIT 5"),
        )
        .expect_err("DISTANCE search on a table without a VECTOR column must still be rejected");
    assert_eq!(err.wire_code(), "22000");
}

// ---------------------------------------------------------------------
// 基準④: operation_id 台帳・RLS・TABLE-12 の契約は変わらない
// ---------------------------------------------------------------------

#[test]
fn missing_operation_id_is_rejected_on_table_without_vector_column() {
    let (core, path) = open_engine("insert-no-vector-missing-op-id");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");

    let err = core
        .execute_insert_sql(
            &policy,
            &format!("INSERT INTO {TABLE} (id, lang, body) VALUES (1, 'ja', 'a')"),
        )
        .expect_err("missing USING OPERATION_ID must be rejected");
    assert_eq!(err.wire_code(), "23502");
}

#[test]
fn resend_same_operation_id_is_23505_and_mismatched_content_is_22023() {
    let (core, path) = open_engine("insert-no-vector-resend");
    let _guard = CleanupGuard(path);
    let policy = ctx("tenant-a");
    let sql = format!(
        "INSERT INTO {TABLE} (id, lang, body) VALUES (1, 'ja', 'a') \
         USING OPERATION_ID 'op-resend'"
    );

    core.execute_insert_sql(&policy, &sql)
        .expect("first insert should succeed");

    // 同一内容の再送 → 23505。
    let err = core
        .execute_insert_sql(&policy, &sql)
        .expect_err("identical resend must be rejected as a duplicate");
    assert_eq!(err.wire_code(), "23505");

    // 内容不一致の再送（同じ operation_id・異なる値）→ 22023。
    let mismatched_sql = format!(
        "INSERT INTO {TABLE} (id, lang, body) VALUES (2, 'en', 'b') \
         USING OPERATION_ID 'op-resend'"
    );
    let err = core
        .execute_insert_sql(&policy, &mismatched_sql)
        .expect_err("mismatched resend must be rejected");
    assert_eq!(err.wire_code(), "22023");

    assert_eq!(
        core.operation_recorded(&policy, TABLE, &OperationId::parse("op-resend").unwrap())
            .expect("ledger lookup must not error"),
        LedgerLookup::Recorded
    );
}

#[test]
fn rls_hides_other_tenants_private_rows_on_table_without_vector_column() {
    let (core, path) = open_engine("insert-no-vector-rls");
    let _guard = CleanupGuard(path);

    core.execute_insert_sql(
        &ctx("tenant-a"),
        &format!(
            "INSERT INTO {TABLE} (id, lang, body) VALUES (1, 'ja', 'a') \
             USING OPERATION_ID 'op-rls-a'"
        ),
    )
    .expect("tenant-a insert should succeed");

    // 別テナントからは見えない。
    assert!(read_back_ids(&core, "tenant-b").is_empty());
    // 投入した本人には見える（RLS-11・read-your-writes）。
    assert_eq!(read_back_ids(&core, "tenant-a"), vec![1]);
}

#[test]
fn different_tenants_may_independently_use_the_same_id_on_table_without_vector_column() {
    let (core, path) = open_engine("insert-no-vector-table12");
    let _guard = CleanupGuard(path);

    core.execute_insert_sql(
        &ctx("tenant-a"),
        &format!(
            "INSERT INTO {TABLE} (id, lang, body) VALUES (1, 'ja', 'a') \
             USING OPERATION_ID 'op-table12-a'"
        ),
    )
    .expect("tenant-a insert should succeed");

    // 物理キーは (tenant_id, id) で名前空間化されるため、別テナントが同じ id を
    // 独立に使える（TABLE-12）。
    core.execute_insert_sql(
        &ctx("tenant-b"),
        &format!(
            "INSERT INTO {TABLE} (id, lang, body) VALUES (1, 'en', 'b') \
             USING OPERATION_ID 'op-table12-b'"
        ),
    )
    .expect("tenant-b must be able to independently use the same id");

    assert_eq!(read_back_ids(&core, "tenant-a"), vec![1]);
    assert_eq!(read_back_ids(&core, "tenant-b"), vec![1]);
}
