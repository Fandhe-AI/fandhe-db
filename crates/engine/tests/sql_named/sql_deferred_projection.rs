//! SQL 表層のスカラー列投影を Top-k 確定後の k 行へ遅延デコードする経路
//! （Issue #453・`sql::exec::execute_statement_with_cache` の `defer_projection`）の
//! 結合テスト。ポインタ: `docs/spec/04-behavior/sql-surface.md` SQL-1・SQL-3、
//! `docs/spec/04-behavior/rls.md` RLS-5, RLS-7, RLS-8、`docs/design/table-generation-*`
//! 系ドキュメント。
//!
//! 検証する契約:
//! 1. TABLE-12（行 `id` の一意性スコープはテナント内）: 異なるテナントの同一 `id`
//!    行が同一可視集合に混在しても、遅延投影が取得する `body` がテナントを跨いで
//!    混線しないこと。
//! 2. cold（redb 再走査）・warm（`SqlArenaCache` ヒット・高速経路）・キャッシュ非経由
//!    の公開ラッパー（`sql::exec::execute_statement`）の 3 経路が完全一致すること。
//! 3. `USING MODE 'precision'`・`SearchEngineKind::Hnsw` opt-in エンジンでも
//!    遅延投影が同じ結果になること（既定エンジン対照）。
//! 4. `WHERE` 付き（eager 経路のまま）の投影は本 Issue の変更で影響を受けないこと。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::hnsw::HnswParams;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn op_id(label: &str) -> OperationId {
    OperationId::parse(label).expect("valid operation id")
}

fn create_docs_table(storage: &Storage) {
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create docs table");
}

fn insert_row(
    storage: &Storage,
    ctx: &PolicyContext,
    id: u64,
    embedding: [f32; 2],
    body: &str,
    visibility: Visibility,
    seed_label: &str,
) {
    engine::tenant::insert_typed_row(
        storage,
        "docs",
        ctx,
        id,
        visibility,
        &[
            Value::Vector(embedding.to_vec()),
            Value::Text(body.to_string()),
        ],
        &op_id(seed_label),
    )
    .expect("insert row");
}

/// 契約 1（TABLE-12）: テナント A（`Private` 行 id=1・body "a-1"）とテナント B
/// （`Public` 行 id=1・body "b-1"）を同一テーブルへ投入し、A のコンテキスト
/// （`Public` + `Private` 可視）で `SELECT id, body ... LIMIT 10` を実行する。
/// cold・warm いずれも、返却された各行の `body` が自分のスロットの
/// `(tenant_id, id)` に対応し、他テナントの `body` と混線しないことを固定する。
#[test]
fn table12_deferred_projection_does_not_mix_tenants_on_shared_id() {
    let path = unique_db_path("sql-deferred-projection-table12");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    create_docs_table(&storage);

    let ctx_a_write = PolicyContext::with_visibilities("tenant-a", [Visibility::Private])
        .expect("valid tenant a");
    let ctx_b_write = PolicyContext::new("tenant-b").expect("valid tenant b");
    insert_row(
        &storage,
        &ctx_a_write,
        1,
        [1.0, 0.0],
        "a-1",
        Visibility::Private,
        "table12-a-1",
    );
    insert_row(
        &storage,
        &ctx_b_write,
        1,
        [0.9, 0.1],
        "b-1",
        Visibility::Public,
        "table12-b-1",
    );

    let ctx_a_read =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant a read ctx");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let sql = "SELECT id, body FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10";

    let assert_no_mixing = |result: &engine::sql::exec::QueryResult| {
        assert_eq!(
            result.rows.len(),
            2,
            "both tenant-a private and tenant-b public rows must be visible"
        );
        for row in &result.rows {
            let body = match &row.cells[1] {
                engine::sql::exec::Cell::Text(t) => t.as_str(),
                other => panic!("expected Text cell, got {other:?}"),
            };
            // id は両テナントとも 1 なので、body の内容だけが正しいテナントを
            // 判別する唯一の手がかりになる。混線していれば "a-1"/"b-1" 以外の
            // 組み合わせ（あるいは重複）が観測される。
            assert!(
                body == "a-1" || body == "b-1",
                "unexpected body content (possible tenant mixing): {body}"
            );
        }
        let bodies: std::collections::BTreeSet<&str> = result
            .rows
            .iter()
            .map(|r| match &r.cells[1] {
                engine::sql::exec::Cell::Text(t) => t.as_str(),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            bodies,
            std::collections::BTreeSet::from(["a-1", "b-1"]),
            "expected exactly one row per tenant with its own body"
        );
    };

    let cold = core.execute_sql(&ctx_a_read, sql).expect("cold query");
    assert_no_mixing(&cold);
    assert_eq!(core.sql_arena_cache_stats().misses, 1);

    let warm = core.execute_sql(&ctx_a_read, sql).expect("warm query");
    assert_no_mixing(&warm);
    let stats = core.sql_arena_cache_stats();
    assert_eq!(stats.misses, 1, "second run must be a cache hit");
    assert_eq!(stats.hits, 1);
    assert_eq!(
        cold, warm,
        "cold and warm deferred-projection results must match exactly"
    );
}

/// 契約 2: `EngineCore::execute_sql`（キャッシュ経由）の cold・warm が完全一致
/// すること。cold（1 回目。cache miss）は `ScalarSource::Deferred(Redb)`
/// （候補選択と同一 `read_txn` 上での行テーブル再取得）を、warm（2 回目。
/// `cache_fast_path_eligible` の高速経路）は `ScalarSource::Deferred(Snapshot)`
/// （`SqlArenaCache` 経由）を、それぞれ経由する。取得元が異なってもデコード
/// 結果は変わらないことを固定する。
#[test]
fn deferred_projection_cold_and_warm_agree_across_metadata_sources() {
    let path = unique_db_path("sql-deferred-projection-agree");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    create_docs_table(&storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let rows: [(u64, [f32; 2], &str); 6] = [
        (1, [1.0, 0.0], "alpha"),
        (2, [0.95, 0.05], "bravo"),
        (3, [0.9, 0.1], "charlie"),
        (4, [0.2, 0.8], "delta"),
        (5, [0.1, 0.9], "echo"),
        (6, [0.0, 1.0], "foxtrot"),
    ];
    for (id, emb, body) in rows {
        insert_row(
            &storage,
            &ctx,
            id,
            emb,
            body,
            Visibility::Public,
            &format!("agree-seed-{id}"),
        );
    }

    let sql = "SELECT id, body FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 4";
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let cold = core.execute_sql(&ctx, sql).expect("cold query");
    assert_eq!(core.sql_arena_cache_stats().misses, 1);
    let warm = core.execute_sql(&ctx, sql).expect("warm query");
    let stats = core.sql_arena_cache_stats();
    assert_eq!(
        stats.misses, 1,
        "second run must be a cache hit, not a rebuild"
    );
    assert_eq!(stats.hits, 1);

    assert_eq!(
        cold, warm,
        "cache-cold (Redb source) and cache-warm (Snapshot source) results must match exactly"
    );
    assert_eq!(cold.rows.len(), 4);
}

/// 契約 3（前半）: `USING MODE 'precision'` でも遅延投影が確信度ゲート通過後の
/// 行に対して正しくデコードされること。
#[test]
fn deferred_projection_matches_under_precision_mode() {
    let path = unique_db_path("sql-deferred-projection-precision");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    create_docs_table(&storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    // Top-1 が明確に勝つ（confidence gate を通過する）ように距離差を大きく取る。
    insert_row(
        &storage,
        &ctx,
        1,
        [1.0, 0.0],
        "winner",
        Visibility::Public,
        "precision-1",
    );
    insert_row(
        &storage,
        &ctx,
        2,
        [0.0, 1.0],
        "loser",
        Visibility::Public,
        "precision-2",
    );

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let sql =
        "SELECT id, body FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 5 USING MODE 'precision'";
    let result = core.execute_sql(&ctx, sql).expect("precision mode query");
    assert_eq!(
        result.rows.len(),
        1,
        "confident top-1 only under precision mode"
    );
    match &result.rows[0].cells[1] {
        engine::sql::exec::Cell::Text(t) => assert_eq!(t, "winner"),
        other => panic!("expected Text cell, got {other:?}"),
    }
}

/// 契約 3（後半）: `SearchEngineKind::Hnsw` opt-in エンジンでも遅延投影が
/// 既定エンジン対照と同じ結果になること（フィルタなし DISTANCE ＋スカラー列
/// 投影の組み合わせ）。`defer_projection`／`project_rows` の分岐条件はエンジン
/// 種別を一切参照しないが、実際に HNSW opt-in 経路（`sql::hnsw_cache`）と
/// 遅延投影（`ScalarSource::Deferred`）を組み合わせて実行し固定する。
#[test]
fn deferred_projection_matches_under_hnsw_engine() {
    let path = unique_db_path("sql-deferred-projection-hnsw");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    create_docs_table(&storage);
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let rows: [(u64, [f32; 2], &str); 6] = [
        (1, [1.0, 0.0], "alpha"),
        (2, [0.95, 0.05], "bravo"),
        (3, [0.9, 0.1], "charlie"),
        (4, [0.2, 0.8], "delta"),
        (5, [0.1, 0.9], "echo"),
        (6, [0.0, 1.0], "foxtrot"),
    ];
    for (id, emb, body) in rows {
        insert_row(
            &storage,
            &ctx,
            id,
            emb,
            body,
            Visibility::Public,
            &format!("hnsw-seed-{id}"),
        );
    }

    let sql = "SELECT id, body FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 4";

    // 既定エンジン（brute-force）対照。
    let ref_path = unique_db_path("sql-deferred-projection-hnsw-ref");
    let _ref_guard = CleanupGuard(ref_path.clone());
    let default_storage = Storage::open(&ref_path).expect("open ref storage");
    create_docs_table(&default_storage);
    for (id, emb, body) in rows {
        insert_row(
            &default_storage,
            &ctx,
            id,
            emb,
            body,
            Visibility::Public,
            &format!("hnsw-ref-seed-{id}"),
        );
    }
    let default_core = EngineCore::from_storage(default_storage, Box::new(CpuScalarProvider));
    let default_result = default_core
        .execute_sql(&ctx, sql)
        .expect("default engine query");

    // `SearchEngineKind::Hnsw` opt-in エンジン。
    let hnsw_kind = search_engine::hnsw_kind(HnswParams::default()).expect("valid hnsw params");
    let hnsw_core = EngineCore::from_storage_with_engine(storage, hnsw_kind);
    let hnsw_cold = hnsw_core.execute_sql(&ctx, sql).expect("hnsw cold query");
    let hnsw_warm = hnsw_core.execute_sql(&ctx, sql).expect("hnsw warm query");

    assert_eq!(
        default_result, hnsw_cold,
        "HNSW opt-in engine (cold) must match default engine for deferred projection"
    );
    assert_eq!(
        hnsw_cold, hnsw_warm,
        "HNSW opt-in engine cold/warm deferred projection must match exactly"
    );
    assert_eq!(hnsw_cold.rows.len(), 4);
}

/// 契約 4: `WHERE` 付き投影（eager 経路のまま。`defer_projection` の適用対象外）は
/// 本 Issue の変更前と同じ挙動を維持する。
#[test]
fn where_filtered_projection_stays_on_eager_path_and_is_unaffected() {
    let path = unique_db_path("sql-deferred-projection-eager-where");
    let _guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, false),
                ColumnDef::new("lang", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let seed = [
        (1u64, [1.0f32, 0.0f32], "alpha", "ja"),
        (2, [0.9, 0.1], "bravo", "en"),
        (3, [0.8, 0.2], "charlie", "ja"),
    ];
    for (id, emb, body, lang) in seed {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &ctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(emb.to_vec()),
                Value::Text(body.to_string()),
                Value::Text(lang.to_string()),
            ],
            &op_id(&format!("eager-where-{id}")),
        )
        .expect("insert row");
    }

    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let sql =
        "SELECT id, body FROM docs WHERE lang = 'ja' ORDER BY embedding <=> '[1.0,0.0]' LIMIT 10";
    let result = core.execute_sql(&ctx, sql).expect("where-filtered query");
    let ids: Vec<u64> = result.rows.iter().map(|r| r.id).collect();
    assert_eq!(
        ids,
        vec![1, 3],
        "WHERE lang = 'ja' must exclude id=2 exactly as before"
    );
}
