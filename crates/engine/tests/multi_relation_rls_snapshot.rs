//! `sql::relation_snapshot`（テーブル単位 RLS 可視スナップショット・複数テーブル
//! 世代整合キャッシュ。SQL-28・RLS-10、TASK-212、Issue #924）が engine クレート
//! 外から到達可能な公開 API であることを固定する結合テスト。テナント境界の
//! 独立性（RLS-9・RLS-10）を実データで確認する（security.md P0）。
//!
//! `resolve_relation_snapshots` は `Storage`（`TableLookup`）で行を投入した後、
//! `execute_scan` 等の既存公開 API 結合テスト（`tests/sql_scan_public_api.rs`）と
//! 同じ流儀で、`Storage` session を drop してから生 `redb::Database::open` で
//! `&redb::ReadTransaction` を得る（`Storage::db()` は `pub(crate)` のまま変更
//! しないため、この経路以外に手段がない）。キャッシュ経由（`cache` 引数
//! `Some((storage, cache))`）の分岐は `&Storage` と `&redb::ReadTransaction` を
//! 同一ファイルへ同時に開く必要があり、`redb` がそれを許さないため crate 外の
//! 結合テストでは検証できない。その分岐の fail-closed 契約・容量制限・巻き添え
//! 失効防止は `sql::relation_snapshot`・`sql::generation_key` の crate 内単体
//! テストが担う。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::relation::TableRef;
use engine::sql::relation_snapshot::resolve_relation_snapshots;
use engine::storage::{RowInput, Storage, Visibility};
use redb::ReadableDatabase;

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn schema(name: &str) -> TableSchema {
    TableSchema::new(name, vec![ColumnDef::new("path", ColumnType::Text, false)])
}

/// `schema` と同じ単一 `TEXT NOT NULL` 列（`path`）に整合する metadata を
/// `row_codec::encode_scalar_columns` で組み立てて挿入する（PR #1104 レビュー対応で
/// `RelationSnapshot::build` が `validate_scalar_columns` を通すようになったため、
/// 空 metadata は非 nullable 列を持つスキーマと整合しない）。
fn seed(storage: &Storage, table: &str, id: u64, tenant: &str, visibility: Visibility) {
    let ctx = PolicyContext::new(tenant).expect("valid tenant");
    let op_id = OperationId::parse(&format!("multi-relation-rls-{table}-{tenant}-{id}"))
        .expect("valid operation_id");
    let metadata = engine::row_codec::encode_scalar_columns(
        &schema(table),
        &[engine::row_codec::Value::Text(String::new())],
    )
    .expect("encode scalar columns");
    engine::tenant::insert_row(
        storage,
        table,
        &ctx,
        id,
        &RowInput {
            tenant_id: tenant,
            visibility,
            embedding: &[],
            metadata: &metadata,
        },
        &op_id,
    )
    .expect("seed row");
}

/// 3 テナント × 2 テーブル（Public/Private 混在）で、テーブルごとの可視集合が
/// 独立に正しいこと。他テナントの Private 行の混入が 0 件であること
/// （RLS-9・RLS-10）。
#[test]
fn per_table_visibility_is_independent_across_tenants() {
    let path = unique_db_path("multi-relation-rls-independent");
    let _cleanup = CleanupGuard(path.clone());
    let (docs, notes) = {
        let storage = Storage::open(&path).expect("open storage");
        let docs = schema("docs");
        let notes = schema("notes");
        storage.create_table(&docs).expect("create docs");
        storage.create_table(&notes).expect("create notes");

        // docs: tenant-a public(1)・tenant-b private(2)・tenant-c public(3)
        seed(&storage, "docs", 1, "tenant-a", Visibility::Public);
        seed(&storage, "docs", 2, "tenant-b", Visibility::Private);
        seed(&storage, "docs", 3, "tenant-c", Visibility::Public);
        // notes: tenant-a private(10)・tenant-b public(20)
        seed(&storage, "notes", 10, "tenant-a", Visibility::Private);
        seed(&storage, "notes", 20, "tenant-b", Visibility::Public);
        (docs, notes)
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    // tenant-a は Public + 自テナントの Private のみ可視。
    let ctx_a =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public]).expect("valid ctx");
    let relations = vec![
        (TableRef::new("docs"), &docs),
        (TableRef::new("notes"), &notes),
    ];
    let result = resolve_relation_snapshots(&read_txn, &ctx_a, &relations, None).expect("resolve");

    let docs_visible = result.snapshots()[0].visible_rows();
    // tenant-a からは docs の Public 行（tenant-a の 1・tenant-c の 3）のみ可視。
    // 他テナントの Private 行（tenant-b の 2）は混入しない。
    assert_eq!(docs_visible.len(), 2);
    assert!(docs_visible
        .iter()
        .all(|(tenant, id)| !(tenant == "tenant-b" && *id == 2)));

    let notes_visible = result.snapshots()[1].visible_rows();
    // notes は Public（tenant-b の 20）のみ可視。tenant-a 自身の Private（10）は
    // ctx_a が Private を許可可視性に含めていないため不可視（`PolicyContext::
    // with_visibilities` が [Public] のみを許可）。
    assert_eq!(notes_visible, &[("tenant-b".to_string(), 20)]);
}

/// 同じ `id` を異なるテナントが持つ場合も `(tenant_id, id)` で区別されること
/// （TABLE-12。`id` 単独では行を一意に識別できない）。
#[test]
fn same_id_across_tenants_is_disambiguated_by_tenant() {
    let path = unique_db_path("multi-relation-rls-same-id");
    let _cleanup = CleanupGuard(path.clone());
    let docs = {
        let storage = Storage::open(&path).expect("open storage");
        let docs = schema("docs");
        storage.create_table(&docs).expect("create table");
        seed(&storage, "docs", 1, "tenant-a", Visibility::Public);
        seed(&storage, "docs", 1, "tenant-c", Visibility::Public);
        docs
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public]).expect("valid ctx");
    let relations = vec![(TableRef::new("docs"), &docs)];
    let result = resolve_relation_snapshots(&read_txn, &ctx, &relations, None).expect("resolve");
    let mut visible: Vec<(String, u64)> = result.snapshots()[0].visible_rows().to_vec();
    visible.sort();
    assert_eq!(
        visible,
        vec![("tenant-a".to_string(), 1), ("tenant-c".to_string(), 1)]
    );
}

/// 自己結合（同じテーブルを 2 回参照）は同じ `Arc<RelationSnapshot>` を共有し、
/// スロット（`Vec` の添字）は 2 つに保たれる。
#[test]
fn self_join_shares_snapshot_arc_across_two_slots() {
    let path = unique_db_path("multi-relation-rls-self-join");
    let _cleanup = CleanupGuard(path.clone());
    let docs = {
        let storage = Storage::open(&path).expect("open storage");
        let docs = schema("docs");
        storage.create_table(&docs).expect("create table");
        seed(&storage, "docs", 1, "tenant-a", Visibility::Public);
        docs
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public]).expect("valid ctx");
    let relations = vec![
        (TableRef::with_alias("docs", "x"), &docs),
        (TableRef::with_alias("docs", "y"), &docs),
    ];
    let result = resolve_relation_snapshots(&read_txn, &ctx, &relations, None).expect("resolve");
    assert_eq!(result.snapshots().len(), 2);
    assert!(std::sync::Arc::ptr_eq(
        &result.snapshots()[0],
        &result.snapshots()[1]
    ));
}

/// 行テーブル未作成のテーブルは空集合として扱う。
#[test]
fn table_without_any_row_yields_empty_snapshot() {
    let path = unique_db_path("multi-relation-rls-empty-table");
    let _cleanup = CleanupGuard(path.clone());
    let docs = {
        let storage = Storage::open(&path).expect("open storage");
        let docs = schema("docs");
        storage.create_table(&docs).expect("create table");
        docs
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx = PolicyContext::new("tenant-a").expect("valid ctx");
    let relations = vec![(TableRef::new("docs"), &docs)];
    let result = resolve_relation_snapshots(&read_txn, &ctx, &relations, None).expect("resolve");
    assert!(result.snapshots()[0].visible_rows().is_empty());
}

/// `TableRef` と `TableSchema` の取り違え（`table_ref.table()` と `schema.name`
/// の不一致）は fail-closed に拒否する。誤った組を素通しすると、誤ったテーブルの
/// 内容が別テーブルの世代キーでキャッシュされ RLS 可視集合が汚染されうる
/// （security.md P0 テナント境界・fail-closed）。
#[test]
fn table_ref_schema_mismatch_is_rejected() {
    let path = unique_db_path("multi-relation-rls-mismatch");
    let _cleanup = CleanupGuard(path.clone());
    let (docs, notes) = {
        let storage = Storage::open(&path).expect("open storage");
        let docs = schema("docs");
        let notes = schema("notes");
        storage.create_table(&docs).expect("create docs");
        storage.create_table(&notes).expect("create notes");
        (docs, notes)
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx = PolicyContext::new("tenant-a").expect("valid ctx");
    // "docs" を参照しているのに、渡すスキーマは "notes" のもの（取り違え）。
    let relations = vec![(TableRef::new("docs"), &notes)];
    match resolve_relation_snapshots(&read_txn, &ctx, &relations, None) {
        Err(engine::sql::allowlist::SqlSurfaceError::Internal { .. }) => {}
        Err(other) => {
            panic!("expected Internal error, got a different SqlSurfaceError variant: {other:?}")
        }
        Ok(_) => panic!("mismatched table_ref/schema must be rejected"),
    }

    // 逆方向（"notes" を参照しているのに "docs" のスキーマ）も拒否されること。
    let relations = vec![(TableRef::new("notes"), &docs)];
    assert!(resolve_relation_snapshots(&read_txn, &ctx, &relations, None).is_err());
}

/// 参照数の上限（`MAX_TABLE_REFS`）超過はアロケーション前に拒否する。
#[test]
fn too_many_relations_is_rejected() {
    let path = unique_db_path("multi-relation-rls-too-many");
    let _cleanup = CleanupGuard(path.clone());
    let docs = {
        let storage = Storage::open(&path).expect("open storage");
        let docs = schema("docs");
        storage.create_table(&docs).expect("create table");
        docs
    };

    let db = redb::Database::open(&path).expect("reopen raw database");
    let read_txn = db.begin_read().expect("begin_read");
    let ctx = PolicyContext::new("tenant-a").expect("valid ctx");
    let relations: Vec<(TableRef, &TableSchema)> = (0..(engine::sql::relation::MAX_TABLE_REFS + 1))
        .map(|i| (TableRef::with_alias("docs", format!("t{i}")), &docs))
        .collect();
    assert!(resolve_relation_snapshots(&read_txn, &ctx, &relations, None).is_err());
}
