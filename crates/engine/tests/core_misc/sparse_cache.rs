//! `sql/sparse_cache.rs::SparseIndexCache`（Issue #357）の結合テスト。`tests/
//! sql_surface.rs` の sql4（hybrid）系テストと同じ流儀（`unique_db_path` /
//! `CleanupGuard`、`Storage::open` 直接投入 → `EngineCore::from_storage`）で
//! `EngineCore::execute_sql` を経由し、受け入れ基準（Issue #357 本文）を検証する:
//!
//! 1. 同一世代内の hybrid 連続クエリで `SparseIndex::build` が 1 回に償却される
//!    （`sparse_index_cache_stats()` の hits/misses と、両実行の結果行の完全一致で
//!    確認する）
//! 2. 世代競合・DML 後の整合が fail-closed（stale 索引で応答しない）
//! 3. hybrid 経由の RLS 不変（他テナントのコーパスを再利用しない）

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};

// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した（`tests/sql_surface.rs` と
// 同じ取り込み方式）。
use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn open_storage(path: &std::path::Path) -> Storage {
    Storage::open(path).expect("open storage")
}

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

fn insert_row(storage: &Storage, tenant: &str, id: u64, emb: [f32; 2], body: Option<&str>) {
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    let value = match body {
        Some(b) => Value::Text(b.to_string()),
        None => Value::Null,
    };
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        &ctx,
        id,
        Visibility::Private,
        &[Value::Vector(emb.to_vec()), value],
        &engine::recovery::required_op_id::OperationId::parse(&format!(
            "sparse-cache-seed-{tenant}-{id}"
        ))
        .expect("valid operation_id"),
    )
    .expect("insert row");
}

fn create_docs_table(storage: &Storage) {
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
}

const HYBRID_SQL: &str =
    "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'vector database') LIMIT 4";

// --- 受け入れ基準 1: 同一世代内の連続クエリで build が 1 回に償却される ------------

#[test]
fn acceptance1_repeated_hybrid_query_amortizes_sparse_index_build() {
    let path = unique_db_path("sparse-cache-accept1");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    create_docs_table(&storage);
    insert_row(
        &storage,
        "tenant-a",
        1,
        [1.0, 0.0],
        Some("rust vector database"),
    );
    insert_row(&storage, "tenant-a", 2, [0.0, 1.0], Some("unrelated topic"));
    insert_row(
        &storage,
        "tenant-a",
        3,
        [0.9, 0.1],
        Some("vector database engine"),
    );
    insert_row(&storage, "tenant-a", 4, [0.1, 0.9], None);

    let core = new_core(storage);
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    let first = core
        .execute_sql(&ctx, HYBRID_SQL)
        .expect("first hybrid query");
    let stats_after_first = core.sparse_index_cache_stats();
    assert_eq!(
        stats_after_first.misses, 1,
        "first query must be a cache miss"
    );
    assert_eq!(stats_after_first.hits, 0);

    let second = core
        .execute_sql(&ctx, HYBRID_SQL)
        .expect("second hybrid query");
    let stats_after_second = core.sparse_index_cache_stats();
    assert_eq!(
        stats_after_second.misses, 1,
        "no table write occurred; second query must reuse the cached index (no rebuild)"
    );
    assert_eq!(
        stats_after_second.hits, 1,
        "second query must be a cache hit"
    );

    // 償却の副作用として結果が変わっていないこと（決定性の同時検証）。
    assert_eq!(result_ids(&first), result_ids(&second));
    assert!(result_ids(&first).contains(&1));
}

// --- 受け入れ基準 2: 世代競合・DML 後の整合が fail-closed（stale 索引で応答しない） ---

#[test]
fn acceptance2_insert_after_hybrid_query_invalidates_cache_and_reflects_new_row() {
    let path = unique_db_path("sparse-cache-accept2");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    create_docs_table(&storage);
    insert_row(&storage, "tenant-a", 1, [1.0, 0.0], Some("unrelated"));
    insert_row(&storage, "tenant-a", 2, [0.0, 1.0], Some("also unrelated"));

    let core = new_core(storage);
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    // まだ検索語に一致する本文が無いクエリを 1 回実行し、キャッシュを形成する。
    let before = core
        .execute_sql(&ctx, HYBRID_SQL)
        .expect("initial hybrid query");
    assert!(
        !result_ids(&before).contains(&5),
        "row 5 must not exist yet"
    );
    let stats_before = core.sparse_index_cache_stats();
    assert_eq!(stats_before.misses, 1);

    // 検索語に強く一致する新規行を SQL INSERT で追加する（テーブル世代を進める）。
    core.execute_insert_sql(
        &ctx,
        "INSERT INTO docs (id, embedding, body) VALUES \
         (5, '[1.0,0.0]', 'vector database vector database') \
         USING OPERATION_ID 'sparse-cache-accept2-insert-5'",
    )
    .expect("insert new row matching the query terms");

    let after = core
        .execute_sql(&ctx, HYBRID_SQL)
        .expect("hybrid query after insert");
    assert!(
        result_ids(&after).contains(&5),
        "newly inserted row must be visible: stale cached index must not be served"
    );

    let stats_after = core.sparse_index_cache_stats();
    assert_eq!(
        stats_after.misses, 2,
        "table generation changed; the post-insert query must rebuild (miss), not reuse stale cache"
    );
    assert!(
        stats_after.stale_evictions >= 1,
        "the now-stale cache entry must have been evicted rather than served"
    );
}

// --- 受け入れ基準 3: hybrid 経由の RLS 不変（他テナントのコーパスを再利用しない） ---

#[test]
fn acceptance3_hybrid_cache_does_not_leak_across_tenants() {
    let path = unique_db_path("sparse-cache-accept3");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    create_docs_table(&storage);
    insert_row(
        &storage,
        "tenant-a",
        1,
        [1.0, 0.0],
        Some("vector database tenant-a-private-secret"),
    );
    insert_row(
        &storage,
        "tenant-b",
        2,
        [1.0, 0.0],
        Some("vector database public info"),
    );

    let core = new_core(storage);
    let ctx_a =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let ctx_b =
        PolicyContext::with_visibilities("tenant-b", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    // tenant-a のクエリでキャッシュを形成する（tenant-a の可視行のみのコーパス）。
    let result_a = core
        .execute_sql(&ctx_a, HYBRID_SQL)
        .expect("tenant-a hybrid query");
    assert_eq!(result_ids(&result_a), vec![1]);
    let stats_after_a = core.sparse_index_cache_stats();
    assert_eq!(stats_after_a.misses, 1);
    assert_eq!(stats_after_a.hits, 0);

    // tenant-b の同一 SQL は tenant-a のコーパスを再利用しない（別 ctx → ミス）。
    let result_b = core
        .execute_sql(&ctx_b, HYBRID_SQL)
        .expect("tenant-b hybrid query");
    assert_eq!(result_ids(&result_b), vec![2]);
    let stats_after_b = core.sparse_index_cache_stats();
    assert_eq!(
        stats_after_b.misses, 2,
        "different PolicyContext (tenant) must not hit tenant-a's cache entry"
    );
    assert_eq!(stats_after_b.hits, 0);

    // tenant-a の行 1（`tenant-a-private-secret`）は tenant-b の結果に一切含まれない。
    assert!(!result_ids(&result_b).contains(&1));
}

// --- WHERE 付き hybrid クエリはキャッシュを経由しない ------------------------------

#[test]
fn where_filtered_hybrid_query_does_not_use_sparse_cache() {
    let path = unique_db_path("sparse-cache-where-bypass");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, true),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for (id, emb, body, lang) in [
        (1u64, [1.0f32, 0.0f32], "rust vector database", "en"),
        (2, [0.9, 0.1], "vector database engine", "ja"),
    ] {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Private,
            &[
                Value::Vector(emb.to_vec()),
                Value::Text(body.to_string()),
                Value::Text(lang.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!(
                "sparse-cache-where-{id}"
            ))
            .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let sql = "SELECT * FROM docs WHERE lang = 'en' \
               ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'vector database') LIMIT 4";

    core.execute_sql(&ctx, sql).expect("filtered hybrid query");
    core.execute_sql(&ctx, sql)
        .expect("filtered hybrid query (again)");

    let stats = core.sparse_index_cache_stats();
    assert_eq!(
        stats.entries, 0,
        "a hybrid query with a WHERE filter must never populate the sparse index cache"
    );
    assert_eq!(stats.hits, 0);
    assert_eq!(stats.misses, 0);
}

// --- 異なる hybrid 本文列（text_column_index）を指定する同一テーブル・同一 ctx の
//     クエリは互いのキャッシュエントリを取り違えない ---------------------------------

#[test]
fn hybrid_queries_on_different_text_columns_do_not_share_cache_entry() {
    let path = unique_db_path("sparse-cache-text-column");
    let _guard = CleanupGuard(path.clone());
    let storage = open_storage(&path);
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("title", ColumnType::Text, false),
            ],
        ))
        .expect("create table");

    // 密側の寄与を全行で揃え（同一 embedding）、疎側（body/title のどちらを本文列に
    // 指定するか）だけが順位を左右するようにする。
    let rows: [(u64, &str, &str); 4] = [
        (1, "vector database", "unrelated"),
        (2, "unrelated stuff", "totally different"),
        (3, "irrelevant", "vector database"),
        (4, "nothing", "nothing"),
    ];
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for (id, body, title) in rows {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Private,
            &[
                Value::Vector(vec![1.0, 0.0]),
                Value::Text(body.to_string()),
                Value::Text(title.to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!(
                "sparse-cache-text-column-{id}"
            ))
            .expect("valid operation_id"),
        )
        .expect("insert row");
    }

    let core = new_core(storage);
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");

    let sql_body = "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, \
                     'vector database') LIMIT 1";
    let sql_title = "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', title, \
                      'vector database') LIMIT 1";

    // body 列を本文に指定するクエリ → row 1（body に一致語を含む）が Top-1。
    let result_body = core
        .execute_sql(&ctx, sql_body)
        .expect("body-column hybrid query");
    assert_eq!(result_ids(&result_body), vec![1]);

    // 同一テーブル・同一 ctx だが本文列が異なる（title）クエリ → row 3
    // （title に一致語を含む）が Top-1 でなければならない。`text_column_index` が
    // キャッシュキーに含まれない場合、body 列用に構築した索引を誤って再利用し、
    // row 1 を Top-1 として返してしまう（このアサーションが退行を検出する）。
    let result_title = core
        .execute_sql(&ctx, sql_title)
        .expect("title-column hybrid query");
    assert_eq!(result_ids(&result_title), vec![3]);

    // 統計面でも、別の本文列は別エントリとして扱われミスになる（誤ヒットしない）。
    let stats = core.sparse_index_cache_stats();
    assert_eq!(
        stats.misses, 2,
        "each distinct text_column_index must be its own cache entry (both queries miss)"
    );
    assert_eq!(stats.hits, 0);
}
