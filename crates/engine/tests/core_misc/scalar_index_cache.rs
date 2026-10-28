//! `sql::scalar_index::ScalarIndexCache`（Issue #473・スカラー列二次索引の
//! 構築とテーブル世代整合キャッシュ）の SQL 表層結合テスト。
//!
//! `tests/sql_arena_cache.rs`・`tests/hnsw_cache.rs` と同じ流儀（`unique_db_path` /
//! `CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、`engine::tenant::insert_typed_row`
//! による投入）で、`EngineCore::execute_sql`（`sql::exec::execute_statement_with_cache`
//! の gated 構築を必ず経由する production 経路）と `EngineCore::
//! scalar_index_cache_stats()`（テナント ID・値を含まないカウンタのみの観測用 API）
//! を突き合わせる。
//!
//! 検証する契約（Issue #473 のスコープ: 構築とキャッシュのみ。索引は応答に
//! 使われないため、クエリ結果はいずれのケースでも索引の有無によらず不変）:
//! 1. 索引対応述語（`WHERE kind = '...'`）を持つ SCALAR 事前フィルタクエリの
//!    初回実行で `builds == 1` になり、同一テーブル世代内の反復では `hits` の
//!    みが増える（再構築されない）。
//! 2. `WHERE` を持たないクエリ（gate 対象外）では `builds` が増えない。
//! 3. 対象テーブルへの書き込みは索引キャッシュを失効させる（`entries` の
//!    次回再構築を経て最新化される）。
//! 4. 別テナント ctx のクエリはキャッシュキーが分離される（`entries` が増える）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn create_docs_table(storage: &Storage) {
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("kind", ColumnType::Text, false),
            ],
        ))
        .expect("create docs table");
}

fn insert_row(
    storage: &Storage,
    ctx: &PolicyContext,
    id: u64,
    embedding: [f32; 2],
    kind: &str,
    visibility: Visibility,
) {
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        ctx,
        id,
        visibility,
        &[
            Value::Vector(embedding.to_vec()),
            Value::Text(kind.to_string()),
        ],
        &op_id(&format!("seed-{id}")),
    )
    .expect("insert row");
}

const SELECT_WHERE: &str =
    "SELECT id FROM docs WHERE kind = 'a' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10";
const SELECT_NO_WHERE: &str = "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10";

fn result_ids(result: &engine::sql::exec::QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
}

// --- 契約 1: 索引対応述語ありクエリの初回 build → 反復ヒット ---

#[test]
fn where_query_builds_once_then_hits_within_same_generation() {
    let path = unique_db_path("scalar-index-cache-build-hit");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    create_docs_table(&storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    insert_row(&storage, &ctx, 1, [1.0, 0.0], "a", Visibility::Public);
    insert_row(&storage, &ctx, 2, [0.9, 0.1], "b", Visibility::Public);
    insert_row(&storage, &ctx, 3, [0.0, 1.0], "a", Visibility::Public);

    let core = new_core(storage);
    let first = core.execute_sql(&ctx, SELECT_WHERE).expect("first query");
    let stats_after_first = core.scalar_index_cache_stats();
    assert_eq!(
        stats_after_first.builds, 1,
        "first WHERE query must build the scalar index once"
    );
    assert_eq!(stats_after_first.entries, 1);

    for _ in 0..5 {
        let repeated = core
            .execute_sql(&ctx, SELECT_WHERE)
            .expect("repeated query");
        // Issue #473 のスコープでは索引は応答に一切使われないため、結果は
        // 索引のヒット・ミスに関わらず常に全走査と完全一致する。
        assert_eq!(result_ids(&repeated), result_ids(&first));
    }
    let stats_after_repeat = core.scalar_index_cache_stats();
    assert_eq!(
        stats_after_repeat.builds, 1,
        "no further scalar index rebuild should occur while the table generation is unchanged"
    );
    assert_eq!(stats_after_repeat.hits, 5);
    assert_eq!(stats_after_repeat.entries, 1);
    assert_eq!(stats_after_repeat.build_failures, 0);
}

// --- 契約 2: WHERE なしクエリは gate 対象外で build が増えない ---

#[test]
fn query_without_where_never_builds_scalar_index() {
    let path = unique_db_path("scalar-index-cache-no-where");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    create_docs_table(&storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    insert_row(&storage, &ctx, 1, [1.0, 0.0], "a", Visibility::Public);

    let core = new_core(storage);
    for _ in 0..3 {
        core.execute_sql(&ctx, SELECT_NO_WHERE)
            .expect("no-where query");
    }
    let stats = core.scalar_index_cache_stats();
    assert_eq!(
        stats.builds, 0,
        "a query without index-eligible predicates must not build the scalar index"
    );
    assert_eq!(stats.entries, 0);
}

// --- 契約 3: 書き込みでキャッシュが失効し、次回クエリで再構築される ---

#[test]
fn write_to_table_invalidates_and_rebuilds_scalar_index() {
    let path = unique_db_path("scalar-index-cache-invalidate");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    create_docs_table(&storage);
    // SQL 表層の `INSERT` は書き込む行の可視性を常に `Visibility::Private` に
    // 固定する契約（`sql::exec` モジュールドキュメント参照）のため、挿入した本人の
    // クエリで読み戻せるよう ctx には `Private` も許可しておく
    // （`tests/sql_arena_cache.rs::cache_is_invalidated_by_insert_into_same_table`
    // と同じ流儀）。
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    insert_row(&storage, &ctx, 1, [1.0, 0.0], "a", Visibility::Public);

    let core = new_core(storage);
    core.execute_sql(&ctx, SELECT_WHERE)
        .expect("first query builds index");
    assert_eq!(core.scalar_index_cache_stats().builds, 1);

    // 対象テーブルへの書き込みでテーブル世代が進む。`EngineCore` が `Storage` の
    // 所有権を握るため、SQL 表層の `INSERT`（`execute_insert_sql`）を経由する。
    core.execute_insert_sql(
        &ctx,
        "INSERT INTO docs (id, embedding, kind) VALUES (4, '[0.1,0.9]', 'a') \
         USING OPERATION_ID 'post-write'",
    )
    .expect("insert after core construction");

    let second = core
        .execute_sql(&ctx, SELECT_WHERE)
        .expect("second query after write");
    // 索引は応答に使われないため、書き込み後の結果は全走査経由でそのまま
    // 新しい行を反映する（索引の再構築が結果へ影響しないことも確認する）。
    assert!(
        result_ids(&second).contains(&4),
        "post-write row must be visible via the unchanged full-scan response path"
    );
    let stats = core.scalar_index_cache_stats();
    assert_eq!(
        stats.builds, 2,
        "a write must invalidate the cached scalar index and trigger a rebuild"
    );
}

// --- 契約 4: キャッシュキーは (table, ctx) 完全一致で分離される ---

#[test]
fn cache_key_is_isolated_per_tenant_ctx() {
    let path = unique_db_path("scalar-index-cache-tenant-isolation");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    create_docs_table(&storage);
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant");
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant");
    insert_row(&storage, &ctx_a, 1, [1.0, 0.0], "a", Visibility::Public);
    insert_row(&storage, &ctx_b, 2, [0.0, 1.0], "a", Visibility::Public);

    let core = new_core(storage);
    core.execute_sql(&ctx_a, SELECT_WHERE)
        .expect("tenant-a query");
    core.execute_sql(&ctx_b, SELECT_WHERE)
        .expect("tenant-b query");

    let stats = core.scalar_index_cache_stats();
    assert_eq!(
        stats.entries, 2,
        "distinct (table, ctx) keys must occupy separate cache entries"
    );
    assert_eq!(stats.builds, 2);
}
