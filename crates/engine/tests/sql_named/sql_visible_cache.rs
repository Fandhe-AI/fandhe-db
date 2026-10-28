//! `sql::visible_cache::VisibleBitmapCache`（Issue #478。`GROUP BY` なし・`WHERE`
//! なしの `DecodeTier::Fast` 単一行集計専用の可視行テーブル世代整合キャッシュ）の
//! 結合テスト。`tests/sql_arena_cache.rs` と同じ流儀（`unique_db_path`／
//! `CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、`EngineCore::execute_sql`
//! 経由の production 経路、`EngineCore::visible_bitmap_cache_stats()`——テナント
//! ID・行 ID・可視件数を含まないカウンタのみの観測用 API）。
//!
//! 検証する契約:
//! 1. 受け入れ条件 (a): 同一テーブル世代内で `COUNT(*)` 等を反復すると 2 回目
//!    以降は `misses` が増えず `hits` のみ増加し、結果は完全一致する。
//! 2. 対象テーブルへの書き込みはキャッシュを失効させ（`stale_evictions` 増加）、
//!    新しい行を反映する。
//! 3. `(table, PolicyContext)` キーが異なるテナント・可視性境界のクエリを一切
//!    混同しない（RLS-7・RLS-8。他テナントの Private 行が hot キャッシュ越しに
//!    別テナントの `COUNT(*)` へ混入しない）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::sql::exec::{Cell, QueryResult};
use engine::storage::{RowInput, Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const TABLE: &str = "docs";

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
    )
}

fn ctx_for(tenant: &str, allow_private: bool) -> PolicyContext {
    if allow_private {
        PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
            .expect("valid tenant")
    } else {
        PolicyContext::new(tenant).expect("valid tenant")
    }
}

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn single_row(result: &QueryResult) -> &[Cell] {
    assert_eq!(result.rows.len(), 1, "aggregate result must be one row");
    &result.rows[0].cells
}

fn as_integer(cell: &Cell) -> u64 {
    match cell {
        Cell::Integer(v) => *v,
        other => panic!("expected Cell::Integer, got {other:?}"),
    }
}

fn insert_row(core: &EngineCore, ctx: &PolicyContext, id: u64, visibility: Visibility, seq: u64) {
    core.insert_row(
        ctx,
        TABLE,
        id,
        &RowInput {
            tenant_id: ctx.tenant_id(),
            visibility,
            embedding: &[0.0f32, 0.0f32],
            metadata: &[],
        },
        Some(&op_id(&format!("test-op-{seq}"))),
    )
    .expect("insert row");
}

/// 受け入れ条件 (a): 同一世代内の 2 回目以降は `misses` を増やさず `hits` の
/// みが増え、`COUNT(*)` の結果は 1 回目と完全一致する。
#[test]
fn count_star_hits_cache_on_second_query_within_same_generation() {
    let path = unique_db_path("visible-cache-count-hit");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", false);

    for i in 0..7u64 {
        insert_row(&core, &ctx, i, Visibility::Public, i);
    }

    let before_stats = core.visible_bitmap_cache_stats();

    let first = core
        .execute_sql(&ctx, "SELECT COUNT(*) FROM docs")
        .expect("first query should succeed");
    let first_count = as_integer(&single_row(&first)[0]);
    assert_eq!(first_count, 7);

    let after_first = core.visible_bitmap_cache_stats();
    assert_eq!(
        after_first.misses,
        before_stats.misses + 1,
        "first query must be a cache miss that populates the cache"
    );

    let second = core
        .execute_sql(&ctx, "SELECT COUNT(*) FROM docs")
        .expect("second query should succeed");
    let second_count = as_integer(&single_row(&second)[0]);
    assert_eq!(
        second_count, first_count,
        "hot cache result must match cold cache result"
    );

    let after_second = core.visible_bitmap_cache_stats();
    assert_eq!(
        after_second.misses, after_first.misses,
        "second query within the same table generation must not miss"
    );
    assert_eq!(
        after_second.hits,
        before_stats.hits + 1,
        "second query within the same table generation must hit"
    );
}

/// `COUNT(id)`・`SUM(id)`・`MIN(id)`・`MAX(id)` いずれも `DecodeTier::Fast` の
/// hot キャッシュ経路で cold と同一結果を返す。キャッシュキーは
/// `(table, PolicyContext)` のみで集計式を含まないため、同一 `EngineCore` を
/// 使い回すと最初の `COUNT(id)` 呼び出しでキャッシュが作られ、後続の
/// `SUM`/`MIN`/`MAX` の「cold」呼び出しが実際には既存キャッシュへヒットして
/// しまい cold/hot 比較が成立しない。集計ごとに新しい `EngineCore`（＝新しい
/// 空のキャッシュ）を使うことで、各集計の cold 呼び出しが真に
/// `VisibleBitmapCache` を経由しない通常走査であることを保証する
/// （codex-review 指摘対応）。
#[test]
fn id_aggregates_match_between_cold_and_hot_cache() {
    for sql in [
        "SELECT COUNT(id) FROM docs",
        "SELECT SUM(id) FROM docs",
        "SELECT MIN(id) FROM docs",
        "SELECT MAX(id) FROM docs",
    ] {
        let path = unique_db_path("visible-cache-id-aggregates");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create table");
        let core = new_core(storage);
        let ctx = ctx_for("tenant-a", false);

        for i in 1..=5u64 {
            insert_row(&core, &ctx, i, Visibility::Public, i);
        }

        let before_stats = core.visible_bitmap_cache_stats();
        let cold = core.execute_sql(&ctx, sql).expect("cold query");
        let cold_value = single_row(&cold)[0].clone();
        let after_cold = core.visible_bitmap_cache_stats();
        assert_eq!(
            after_cold.misses,
            before_stats.misses + 1,
            "cold query for `{sql}` must be a genuine cache miss"
        );

        let hot = core.execute_sql(&ctx, sql).expect("hot query");
        let hot_value = single_row(&hot)[0].clone();
        let after_hot = core.visible_bitmap_cache_stats();
        assert_eq!(
            after_hot.hits,
            after_cold.hits + 1,
            "hot query for `{sql}` must hit the cache populated by the cold query"
        );
        assert_eq!(cold_value, hot_value, "cold/hot mismatch for `{sql}`");
    }
}

/// 対象テーブルへの書き込み後は失効し、新しい行数を反映する（fail-closed:
/// stale なキャッシュで応答しない）。
#[test]
fn insert_after_hit_invalidates_and_reflects_new_rows() {
    let path = unique_db_path("visible-cache-invalidate");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = new_core(storage);
    let ctx = ctx_for("tenant-a", false);

    insert_row(&core, &ctx, 1, Visibility::Public, 1);
    let first = core
        .execute_sql(&ctx, "SELECT COUNT(*) FROM docs")
        .expect("query should succeed");
    assert_eq!(as_integer(&single_row(&first)[0]), 1);

    insert_row(&core, &ctx, 2, Visibility::Public, 2);

    // 失効判定は次回 lookup 時に行われる（`VisibleBitmapCache::lookup` が
    // `storage` から真に最新の世代を再読取して stale と確認できた場合のみ
    // 破棄する契約。モジュールドキュメント参照）。
    let second = core
        .execute_sql(&ctx, "SELECT COUNT(*) FROM docs")
        .expect("query should succeed");
    assert_eq!(
        as_integer(&single_row(&second)[0]),
        2,
        "must reflect the newly inserted row, not a stale cached count"
    );
    let stats_after_second = core.visible_bitmap_cache_stats();
    assert!(
        stats_after_second.stale_evictions >= 1,
        "write to the cached table must evict the stale entry on the next lookup"
    );
}

/// `(table, PolicyContext)` キー: tenant-a の Private 行を hot にした後の
/// tenant-b（Public のみ可視）の `COUNT(*)` は自身の可視件数のままで、
/// tenant-a の非公開行の存在・件数が混入しない（RLS-7・RLS-8）。
#[test]
fn cache_key_never_leaks_across_tenants() {
    let path = unique_db_path("visible-cache-tenant-isolation");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let core = new_core(storage);

    let tenant_a = ctx_for("tenant-a", true);
    let tenant_b = ctx_for("tenant-b", false);

    // tenant-a: Public 2 件・Private 3 件。tenant-a 自身は allow_private
    // なので、自身の Public/Private 全件 + 他テナントの Public 行が見える
    // （Public は visibility 定義上どのテナントからも可視）。
    insert_row(&core, &tenant_a, 1, Visibility::Public, 1);
    insert_row(&core, &tenant_a, 2, Visibility::Public, 2);
    insert_row(&core, &tenant_a, 3, Visibility::Private, 3);
    insert_row(&core, &tenant_a, 4, Visibility::Private, 4);
    insert_row(&core, &tenant_a, 5, Visibility::Private, 5);

    // tenant-b: Public 1 件のみ。
    insert_row(&core, &tenant_b, 100, Visibility::Public, 6);

    // tenant-a の視点を先に hot にする（自身の Public 2 件・Private 3 件 +
    // tenant-b の Public 1 件 = 6 件）。
    let a_result = core
        .execute_sql(&tenant_a, "SELECT COUNT(*) FROM docs")
        .expect("tenant-a query should succeed");
    assert_eq!(as_integer(&single_row(&a_result)[0]), 6);
    let a_result_again = core
        .execute_sql(&tenant_a, "SELECT COUNT(*) FROM docs")
        .expect("tenant-a hot query should succeed");
    assert_eq!(as_integer(&single_row(&a_result_again)[0]), 6);

    // tenant-b は自身の Public 行（tenant-a の Public 2 件 + 自身の Public 1 件 =
    // 3 件。tenant-a の Private 行は非可視）だけを見る。tenant-a のキャッシュ
    // エントリとは別キーのためミスから始まり、独自の可視件数を返す。
    let b_result = core
        .execute_sql(&tenant_b, "SELECT COUNT(*) FROM docs")
        .expect("tenant-b query should succeed");
    assert_eq!(
        as_integer(&single_row(&b_result)[0]),
        3,
        "tenant-b must see only its own visible rows, unaffected by tenant-a's hot cache entry"
    );
}
