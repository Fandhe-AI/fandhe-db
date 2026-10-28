//! SQL 表層専用の `VectorArena` テーブル世代整合キャッシュ（`core.rs::SqlArenaCache`。
//! Issue #363・VectorArena のテーブル世代整合キャッシュ化）の結合テスト。
//!
//! `tests/declarative_filter.rs`・`tests/sql_surface.rs` と同じ流儀（`unique_db_path` /
//! `CleanupGuard`、実 `Storage`＋`CpuScalarProvider`、`engine::tenant::insert_typed_row`
//! による投入）で、`EngineCore::execute_sql`（SQL 経由。`sql_arena_cache` を必ず
//! 経由する production 経路）と `EngineCore::sql_arena_cache_stats()`（テナント ID・
//! 行内容を含まないカウンタのみの観測用 API）を突き合わせる。
//!
//! 検証する契約:
//! 1. 同一テーブル世代内で同一 SQL を反復すると、2 回目以降は `misses` が増えず
//!    `hits` のみ増加し、結果集合はコールドキャッシュ時と完全一致する。
//! 2. 対象テーブルへの書き込み（INSERT・ALTER TABLE）はキャッシュを失効させ
//!    （`stale_evictions` 増加）、新しい行・スキーマを反映した正しい結果を返す
//!    （fail-closed。stale なキャッシュで応答しない）。
//! 3. 無関係な別テーブルへの書き込みはキャッシュを失効させない（テーブル単位
//!    世代の粒度）。
//! 4. キャッシュキーは `(table, PolicyContext)` 完全一致であり、異なるテナント・
//!    可視性境界のクエリが互いのキャッシュ・結果へ一切漏れない（RLS 不変）。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn new_core(storage: Storage) -> EngineCore {
    EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
}

fn result_ids(result: &QueryResult) -> Vec<u64> {
    result.rows.iter().map(|r| r.id).collect()
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

const SELECT_ALL: &str = "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10";

// --- 契約 1: 同一世代内の反復はヒットし、結果はコールドキャッシュと完全一致する ---

#[test]
fn cache_hit_avoids_rebuild_and_returns_identical_results() {
    let path = unique_db_path("sql-arena-cache-hit");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    create_docs_table(&storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    insert_row(&storage, &ctx, 1, [1.0, 0.0], "a", Visibility::Public);
    insert_row(&storage, &ctx, 2, [0.9, 0.1], "b", Visibility::Public);
    insert_row(&storage, &ctx, 3, [0.0, 1.0], "c", Visibility::Public);

    let core = new_core(storage);
    let first = core.execute_sql(&ctx, SELECT_ALL).expect("first query");
    let stats_after_first = core.sql_arena_cache_stats();
    assert_eq!(
        stats_after_first.misses, 1,
        "first query must be a cache miss"
    );
    assert_eq!(stats_after_first.hits, 0);

    for _ in 0..5 {
        let repeated = core.execute_sql(&ctx, SELECT_ALL).expect("repeated query");
        assert_eq!(
            result_ids(&repeated),
            result_ids(&first),
            "cache-warm result must match the cold-cache result exactly (ordered)"
        );
    }
    let stats_after_repeat = core.sql_arena_cache_stats();
    assert_eq!(
        stats_after_repeat.misses, 1,
        "no further redb rebuild should occur while the table generation is unchanged"
    );
    assert_eq!(stats_after_repeat.hits, 5);
    assert_eq!(stats_after_repeat.entries, 1);
}

// WHERE 句（SCALAR 事前フィルタ）はクエリごとに異なるため、キャッシュヒット時も
// 正しく再適用されることを確認する（`SqlArenaCache` はクエリの表面ではなく
// `(table, ctx)` をキーにするため、フィルタが異なる 2 クエリが同じキャッシュ
// エントリを共有してもフィルタ結果が混線しないことの回帰確認）。
#[test]
fn cache_hit_reapplies_where_filter_per_query() {
    let path = unique_db_path("sql-arena-cache-where");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    create_docs_table(&storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    insert_row(&storage, &ctx, 1, [1.0, 0.0], "code", Visibility::Public);
    insert_row(&storage, &ctx, 2, [0.9, 0.1], "doc", Visibility::Public);
    insert_row(&storage, &ctx, 3, [0.0, 1.0], "code", Visibility::Public);

    let core = new_core(storage);
    // 1 回目: フィルタなしでキャッシュを温める。
    let _ = core.execute_sql(&ctx, SELECT_ALL).expect("warm cache");
    assert_eq!(core.sql_arena_cache_stats().misses, 1);

    // 2 回目: 同じ (table, ctx) だが異なる WHERE。キャッシュはヒットするが
    // SCALAR 段の再適用で `kind = 'code'` のみに絞られなければならない。
    let filtered = core
        .execute_sql(
            &ctx,
            "SELECT * FROM docs WHERE kind = 'code' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10",
        )
        .expect("filtered query");
    assert_eq!(result_ids(&filtered), vec![1, 3]);
    let stats = core.sql_arena_cache_stats();
    assert_eq!(
        stats.misses, 1,
        "second query must reuse the cached RLS snapshot"
    );
    assert_eq!(stats.hits, 1);
}

// --- 契約 2: 対象テーブルへの書き込みはキャッシュを失効させる（fail-closed） ---

#[test]
fn cache_is_invalidated_by_insert_into_same_table() {
    let path = unique_db_path("sql-arena-cache-invalidate-insert");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    create_docs_table(&storage);
    // SQL 表層の `INSERT`（`sql::exec::execute_insert`）は書き込む行の可視性を常に
    // `Visibility::Private` に固定する契約（他テナントへの越境露出を避ける
    // fail-closed 判断。`sql::exec` モジュールドキュメント参照）。挿入した本人の
    // クエリで読み戻せることを確認するため、ctx には `Private` も許可しておく。
    let ctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    insert_row(&storage, &ctx, 1, [1.0, 0.0], "a", Visibility::Public);

    let core = new_core(storage);
    let before = core.execute_sql(&ctx, SELECT_ALL).expect("warm cache");
    assert_eq!(result_ids(&before), vec![1]);
    assert_eq!(core.sql_arena_cache_stats().misses, 1);

    // 対象テーブルへの書き込みはテーブル世代を進める（catalog.rs の全書き込み経路が
    // commit 直前に必ず `bump_table_generation_in_txn` を呼ぶ契約。
    // `tests/table_generation_bump_coverage.rs` が構造的に担保する）。`EngineCore` が
    // `Storage` の所有権を握るため、SQL 表層の `INSERT`（`core.execute_insert_sql`）を
    // 経由して書き込む（別 `Storage` ハンドルを新設しない）。
    core.execute_insert_sql(
        &ctx,
        "INSERT INTO docs (id, embedding, kind) VALUES (2, '[0.0,1.0]', 'b') \
         USING OPERATION_ID 'post-cache-insert'",
    )
    .expect("insert additional row after caching");

    let after = core
        .execute_sql(&ctx, SELECT_ALL)
        .expect("post-write query");
    assert_eq!(
        result_ids(&after),
        vec![1, 2],
        "stale cache must not hide the newly written row"
    );
    let stats = core.sql_arena_cache_stats();
    assert!(
        stats.stale_evictions >= 1,
        "table generation bump must be detected as a stale cache entry"
    );
    assert_eq!(
        stats.misses, 2,
        "post-write query must rebuild, not reuse a stale snapshot"
    );
}

// --- 契約 3: 無関係な別テーブルへの書き込みはキャッシュを失効させない ---

#[test]
fn cache_is_not_invalidated_by_write_to_unrelated_table() {
    let path = unique_db_path("sql-arena-cache-unrelated-table");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    create_docs_table(&storage);
    storage
        .create_table(&TableSchema::new(
            "other",
            vec![ColumnDef::new("embedding", ColumnType::Vector(2), false)],
        ))
        .expect("create unrelated table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    insert_row(&storage, &ctx, 1, [1.0, 0.0], "a", Visibility::Public);

    let core = new_core(storage);
    let _ = core.execute_sql(&ctx, SELECT_ALL).expect("warm cache");
    assert_eq!(core.sql_arena_cache_stats().misses, 1);

    core.execute_insert_sql(
        &ctx,
        "INSERT INTO other (id, embedding) VALUES (100, '[0.5,0.5]') \
         USING OPERATION_ID 'unrelated-write'",
    )
    .expect("insert into unrelated table");

    let _ = core
        .execute_sql(&ctx, SELECT_ALL)
        .expect("query after unrelated write");
    let stats = core.sql_arena_cache_stats();
    assert_eq!(
        stats.misses, 1,
        "a write to a different table must not bump docs' table generation"
    );
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.stale_evictions, 0);
}

// --- 契約 4: キャッシュキーは (table, ctx) 完全一致。テナント境界を越えて漏れない ---

#[test]
fn cache_isolates_tenants_and_visibility_and_never_leaks_across_ctx() {
    let path = unique_db_path("sql-arena-cache-rls-isolation");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    create_docs_table(&storage);
    let ctx_a = PolicyContext::new("tenant-a").expect("valid tenant a");
    let ctx_b = PolicyContext::new("tenant-b").expect("valid tenant b");
    // tenant-a: Public 1 件・Private 1 件。tenant-b: Public 1 件。
    insert_row(
        &storage,
        &ctx_a,
        1,
        [1.0, 0.0],
        "a-public",
        Visibility::Public,
    );
    insert_row(
        &storage,
        &ctx_a,
        2,
        [0.9, 0.1],
        "a-private",
        Visibility::Private,
    );
    insert_row(
        &storage,
        &ctx_b,
        3,
        [0.8, 0.2],
        "b-public",
        Visibility::Public,
    );

    let core = new_core(storage);

    // tenant-a（Public のみ許可）: 自分の Public 行と、tenant-b の Public 行が見える
    // （Public は全テナント共有可視のため）。tenant-a の Private 行・存在しない
    // Private 許可は含まれない。
    let a_result = core
        .execute_sql(&ctx_a, SELECT_ALL)
        .expect("tenant-a query");
    let mut a_ids = result_ids(&a_result);
    a_ids.sort_unstable();
    assert_eq!(
        a_ids,
        vec![1, 3],
        "tenant-a ctx must not see tenant-a's own Private row"
    );

    // tenant-b（Public のみ許可）: 自分と tenant-a の Public 行のみ。
    let b_result = core
        .execute_sql(&ctx_b, SELECT_ALL)
        .expect("tenant-b query");
    let mut b_ids = result_ids(&b_result);
    b_ids.sort_unstable();
    assert_eq!(b_ids, vec![1, 3]);

    let stats = core.sql_arena_cache_stats();
    assert_eq!(
        stats.entries, 2,
        "distinct PolicyContext values must occupy distinct cache entries"
    );
    assert_eq!(stats.misses, 2);

    // 反復しても互いのキャッシュへ混線しない（ヒット時も RLS 境界は保たれる）。
    let a_again = core
        .execute_sql(&ctx_a, SELECT_ALL)
        .expect("tenant-a repeat");
    let mut a_ids_again = result_ids(&a_again);
    a_ids_again.sort_unstable();
    assert_eq!(a_ids_again, vec![1, 3]);
    let stats_after = core.sql_arena_cache_stats();
    assert_eq!(stats_after.hits, 1);
    assert_eq!(stats_after.misses, 2);
}

// --- 差分テスト: hybrid・`USING MODE 'precision'`・`HINT ORDER` の各経路で、
// キャッシュあり（ヒット）／キャッシュなし（コールド）の応答が完全一致すること ---
//
// この 3 経路は候補構築後の段（hybrid 疎コーパス・precision ゲート・
// `HINT ORDER` による SCALAR/DISTANCE の順序入れ替え）でスロット番号
// （`candidate_columns[slot]` ↔ 疎 DocId ↔ provider id の 1 対 1 対応。
// TABLE-12 対応）に強く依存するため、`tests/sql_arena_cache.rs` の他テストが
// 使う `vec![id, ...]` 単純比較ではなく `QueryResult`（`columns`・`rows` の
// `id`／`score`／`cells` すべて）の完全一致で検証する。

/// `embedding VECTOR(2)`・`body TEXT` を持つ `docs` テーブルへ、hybrid・precision・
/// `HINT ORDER` のいずれの経路でも意味のある差が出るデータ（本文語彙の重なり方・
/// 距離の近さがまちまち）を投入する。
fn setup_hybrid_docs_table(storage: &Storage) {
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let rows: [(u64, [f32; 2], &str); 5] = [
        (1, [1.0, 0.0], "vector database search"),
        (2, [0.95, 0.05], "graph database engine"),
        (3, [0.0, 1.0], "unrelated text content"),
        (4, [0.5, 0.5], "hybrid vector and text search"),
        (5, [0.2, 0.8], "database systems overview"),
    ];
    for (id, emb, body) in rows {
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[Value::Vector(emb.to_vec()), Value::Text(body.to_string())],
            &op_id(&format!("hybrid-seed-{id}")),
        )
        .expect("insert row");
    }
}

/// 新規 DB＋`EngineCore` を毎回構築し（前の呼び出しで温めたキャッシュを持ち込まず、
/// この 1 回の呼び出し内でコールド→ウォームの純粋な比較にするため）、同一 `sql` を
/// 2 回実行して `QueryResult` が完全一致することを確認する。
fn assert_cold_equals_warm(db_label: &str, sql: &str) {
    let path = unique_db_path(db_label);
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    setup_hybrid_docs_table(&storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let core = new_core(storage);

    let cold = core
        .execute_sql(&ctx, sql)
        .expect("cold (cache-miss) query");
    assert_eq!(core.sql_arena_cache_stats().misses, 1);

    let warm = core.execute_sql(&ctx, sql).expect("warm (cache-hit) query");
    let stats = core.sql_arena_cache_stats();
    assert_eq!(
        stats.misses, 1,
        "second run must be a cache hit, not a rebuild"
    );
    assert_eq!(stats.hits, 1);

    assert_eq!(
        cold, warm,
        "cache-warm QueryResult (columns/rows/ids/scores/cells, in order) must equal \
         the cache-cold result exactly for: {sql}"
    );
}

#[test]
fn hybrid_rrf_cache_hit_matches_cold_cache() {
    assert_cold_equals_warm(
        "sql-arena-cache-diff-hybrid",
        "SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'vector database') LIMIT 4",
    );
}

#[test]
fn precision_mode_cache_hit_matches_cold_cache() {
    assert_cold_equals_warm(
        "sql-arena-cache-diff-precision",
        "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 1 USING MODE 'precision'",
    );
}

#[test]
fn hint_order_distance_first_cache_hit_matches_cold_cache() {
    assert_cold_equals_warm(
        "sql-arena-cache-diff-hint-order",
        "SELECT * FROM docs WHERE body = 'vector database search' \
         ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10 HINT ORDER(DISTANCE, SCALAR, RLS)",
    );
}

// `sql::exec::execute_statement_with_cache` の高速経路（WHERE・式述語・hybrid・投影用
// スカラー列参照のいずれも無い場合、SCALAR 段が恒等写像であることを利用して
// キャッシュ済みスナップショットを行単位コピーなしで直接借用する分岐。
// `SELECT id ...`〔`sql_c1_bench.rs` の規範形〕がこの経路に該当する）の結果が、
// 通常経路（`SELECT_ALL`。`docs` に `TEXT` 列があり投影対象になるため
// `needed_column_indices` が非空＝高速経路の対象外）と食い違わないことを確認する。
#[test]
fn scalar_free_projection_cache_hit_matches_cold_cache_fast_path() {
    assert_cold_equals_warm(
        "sql-arena-cache-diff-fast-path",
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 4",
    );
}

/// Issue #453: スカラー列（`body`）を投影する `LIMIT k`（k < 可視行数）は
/// `defer_projection` により Top-k 確定後の遅延デコード経路へ乗る。cold
/// （redb 再走査で `on_visible_row` を経由し `SqlArenaCache::insert` でスナップ
/// ショットを登録）・warm（`cache_fast_path_eligible` の高速経路。`Snapshot`
/// 由来の遅延デコード）で結果が完全一致することを固定する。
#[test]
fn deferred_scalar_projection_cache_hit_matches_cold_cache() {
    assert_cold_equals_warm(
        "sql-arena-cache-diff-deferred-scalar",
        "SELECT id, body FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 4",
    );
}

/// Issue #660 P1（hybrid の借用高速経路）: `SparseIndexCache` がヒットした
/// 2 回目以降の hybrid クエリは、SCALAR 段が恒等写像になるため `VectorArena` を
/// 一切複製せずキャッシュ済みスナップショットを借用する。
///
/// cold（1 回目）は疎索引キャッシュがミスのため本文列を蓄積する必要があり
/// 複製経路（`full_rebuild_copies`）を通り、warm（2 回目以降）は借用経路
/// （`fast_path_borrows`）へ載ることをカウンタで固定する（非 vacuous 性）。
/// 結果自体が cold と完全一致することは
/// `hybrid_rrf_cache_hit_matches_cold_cache` が別途固定している。
#[test]
fn hybrid_cache_hit_takes_borrowed_fast_path_without_arena_copy() {
    let path = unique_db_path("sql-arena-cache-hybrid-fast-path");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    setup_hybrid_docs_table(&storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let core = new_core(storage);

    let sql =
        "SELECT id FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'vector database') \
         LIMIT 4";

    let cold = core.execute_sql(&ctx, sql).expect("cold hybrid query");
    let after_cold = core.sql_arena_cache_stats();
    assert_eq!(after_cold.misses, 1, "1 回目はアリーナキャッシュミス");
    assert_eq!(
        after_cold.fast_path_borrows, 0,
        "1 回目はキャッシュミスのため借用高速経路を通らない"
    );

    let warm = core.execute_sql(&ctx, sql).expect("warm hybrid query");
    let after_warm = core.sql_arena_cache_stats();
    assert_eq!(after_warm.hits, 1, "2 回目はアリーナキャッシュヒット");
    assert_eq!(
        after_warm.fast_path_borrows, 1,
        "疎索引キャッシュヒット後の hybrid は借用高速経路へ載る（Issue #660 P1）"
    );
    assert_eq!(
        after_warm.full_rebuild_copies, 0,
        "借用高速経路では可視全行の VectorArena 複製が発生しない"
    );
    assert_eq!(cold, warm, "借用高速経路の結果は複製経路と完全一致する");
}

/// Issue #660 P2（hybrid の投影遅延）: 疎索引キャッシュヒット時は hybrid でも
/// `defer_projection` が効き、`body` 等のスカラー投影は Top-k 確定後の k 行
/// だけをデコードする。cold（複製経路・全行 `body` 複製）と warm（借用高速
/// 経路・遅延デコード）で投影セル値まで完全一致することを固定する。
#[test]
fn hybrid_scalar_projection_cache_hit_matches_cold_cache_deferred() {
    assert_cold_equals_warm(
        "sql-arena-cache-hybrid-deferred",
        "SELECT id, body FROM docs \
         ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'vector database') LIMIT 3",
    );
}

/// Issue #660 P2 の非 vacuous 性: `body` を投影する hybrid でも warm 実行は
/// 借用高速経路（複製 0 回）へ載る。
#[test]
fn hybrid_scalar_projection_cache_hit_takes_borrowed_fast_path() {
    let path = unique_db_path("sql-arena-cache-hybrid-proj-fast-path");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    setup_hybrid_docs_table(&storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let core = new_core(storage);

    let sql = "SELECT id, body FROM docs \
               ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'vector database') LIMIT 3";

    let cold = core
        .execute_sql(&ctx, sql)
        .expect("cold hybrid projection query");
    let warm = core
        .execute_sql(&ctx, sql)
        .expect("warm hybrid projection query");
    let stats = core.sql_arena_cache_stats();
    assert_eq!(stats.hits, 1);
    assert_eq!(
        stats.fast_path_borrows, 1,
        "投影列ありの hybrid も疎索引キャッシュヒット後は借用高速経路へ載る"
    );
    assert_eq!(
        stats.full_rebuild_copies, 0,
        "借用高速経路では可視全行の VectorArena 複製が発生しない"
    );
    assert_eq!(cold, warm, "遅延デコードの投影値は複製経路と完全一致する");
}
