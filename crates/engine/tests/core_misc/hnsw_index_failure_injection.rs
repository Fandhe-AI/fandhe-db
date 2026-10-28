//! `sql::hnsw_cache::HnswIndexCache`（Issue #408・親 #402・前提 #404〜#407）の索引
//! 構築失敗時の縮退（R3）を検証する結合テスト。`tests/index_failure_injection.rs`
//! の流儀（公開 API のみで注入する。テスト専用 seam なし）を踏襲する。
//!
//! **注入方式**: 全次元 `f32::MAX` の「毒行」を 1 件混入させると、`HnswIndex::build`
//! （`insert_node` 内の内積計算がオーバーフローする）が `HnswError::NonFiniteScore`
//! で失敗する一方、`CpuScalarProvider`／`ParallelSearchProvider`（brute-force）は
//! 当該行を非有限スコアとしてスキップし通常どおり応答する。`row_codec`／
//! `tenant::insert_rows` は embedding の大きさ・有限性を検証しないため、毒行は
//! そのまま格納・読み戻しできる（本ファイル冒頭の `poison_row_round_trips_unaltered`
//! で固定）。
//!
//! 検証する契約（R3）:
//! - 初回 SELECT で索引構築が失敗し `build_failures == 1`、結果は既定エンジンと
//!   完全一致（brute-force 縮退）。
//! - 同一世代内の 2 回目の SELECT でも `build_failures` は増えない（負のキャッシュ。
//!   同一世代での構築再試行連打を防ぐ）。
//! - 再オープン（`drop` → `Storage::open` → `from_storage_with_engine`）後も同様。
//! - 毒行を `delete_row`（新世代）すると次の SELECT で `builds` が増加し索引経路へ
//!   復帰する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, VectorCore};
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::search_engine;
use engine::storage::{RowInput, Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 16;
// `MIN_INDEXED_ROWS`（1,024）超・`SEQUENTIAL_PREFIX_NODES`（256）超だが
// `thread_count_for` の実質並列化閾値（2,048）未満（`hnsw_cache.rs` モジュール
// ドキュメント・`tests/hnsw_cache.rs` と同じ fixture 方針）。
const ROWS: usize = 1_100;

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![ColumnDef::new("embedding", ColumnType::Vector(DIM), false)],
    )
}

/// 決定的な通常行ベクトル（成分は `[0.25, 1.0]`。plan で確認済みの毒行注入前提を
/// 満たす: 通常行どうしの内積でもオーバーフローしない範囲）。
fn normal_vector(seed: u64) -> Vec<f32> {
    let mut state = seed.max(1);
    (0..DIM)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let bits = (state >> 40) as u32;
            0.25 + (bits as f32 / (1u32 << 24) as f32) * 0.75
        })
        .collect()
}

fn poison_vector() -> Vec<f32> {
    vec![f32::MAX; DIM as usize]
}

fn seed_normal_rows(storage: &Storage, ctx: &PolicyContext, start_id: u64, count: usize) {
    let vectors: Vec<Vec<f32>> = (0..count)
        .map(|i| normal_vector(start_id + i as u64))
        .collect();
    let rows: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                start_id + i as u64,
                RowInput {
                    tenant_id: ctx.tenant_id(),
                    visibility: Visibility::Public,
                    embedding: v.as_slice(),
                    metadata: &[],
                },
            )
        })
        .collect();
    let op_id = OperationId::parse(&format!("hnsw-fail-inject-seed-{start_id}"))
        .expect("valid operation_id");
    engine::tenant::insert_rows(storage, "docs", ctx, &rows, &op_id).expect("seed normal rows");
}

/// 毒行が `insert_rows` → `row_codec` → `get_row` の往復で無変化に読み戻せることを
/// 確認する（実装計画ステップ 8 が要求する事前確認。拒否・丸めが発生する場合は
/// 別の注入方式を検討する必要があるため、最初にこれを固定する）。
#[test]
fn poison_row_round_trips_unaltered() {
    let dir = unique_db_path("hnsw-fail-inject-roundtrip");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let poison = poison_vector();
    let op_id = OperationId::parse("hnsw-fail-inject-roundtrip").expect("valid operation_id");
    engine::tenant::insert_row(
        &storage,
        "docs",
        &ctx,
        1,
        &RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &poison,
            metadata: &[],
        },
        &op_id,
    )
    .expect("insert poison row");

    let core = EngineCore::from_storage(storage, search_engine::default_engine());
    let row = core
        .get_row(&ctx, "docs", "tenant-a", 1)
        .expect("get_row should succeed");
    assert_eq!(
        row.embedding, poison,
        "poison row must round-trip through row_codec unaltered (no clamping/rejection)"
    );
}

fn open_hnsw_core(storage: Storage) -> EngineCore {
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    EngineCore::from_storage_with_engine(storage, kind)
}

fn query_ids(core: &EngineCore, ctx: &PolicyContext, query: &[f32]) -> Vec<u64> {
    let parts: Vec<String> = query.iter().map(|x| x.to_string()).collect();
    let sql = format!(
        "SELECT id FROM docs ORDER BY embedding <=> '[{}]' LIMIT 10",
        parts.join(",")
    );
    core.execute_sql(ctx, &sql)
        .expect("query should succeed despite index build failure")
        .rows
        .iter()
        .map(|r| r.id)
        .collect()
}

/// R3 本体: 索引構築失敗 → brute-force 縮退（この世代のみ）→ 同一世代では再試行
/// しない（負のキャッシュ）→ 再オープン後も同様 → 毒行削除後の次世代で索引経路へ
/// 復帰する。
#[test]
fn build_failure_degrades_to_brute_force_and_recovers_after_poison_removed() {
    let dir = unique_db_path("hnsw-fail-inject-main");
    let _cleanup = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");

    seed_normal_rows(&storage, &ctx, 1, ROWS);
    let op_poison = OperationId::parse("hnsw-fail-inject-poison").expect("valid operation_id");
    let poison = poison_vector();
    engine::tenant::insert_row(
        &storage,
        "docs",
        &ctx,
        ROWS as u64 + 1,
        &RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &poison,
            metadata: &[],
        },
        &op_poison,
    )
    .expect("insert poison row");

    // 対照用の brute-force 側は独立した Storage へ同一行を複製する。
    let ref_dir = unique_db_path("hnsw-fail-inject-ref");
    let _ref_cleanup = CleanupGuard(ref_dir.clone());
    let ref_storage = Storage::open(&ref_dir).expect("open ref storage");
    ref_storage
        .create_table(&schema())
        .expect("create ref table");
    seed_normal_rows(&ref_storage, &ctx, 1, ROWS);
    engine::tenant::insert_row(
        &ref_storage,
        "docs",
        &ctx,
        ROWS as u64 + 1,
        &RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &poison,
            metadata: &[],
        },
        &op_poison,
    )
    .expect("insert poison row (ref)");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    let query = normal_vector(1);

    let core = open_hnsw_core(storage);
    let got1 = query_ids(&core, &ctx, &query);
    let want = query_ids(&ref_core, &ctx, &query);
    assert_eq!(
        got1, want,
        "first query must degrade to brute-force and match the default engine exactly"
    );
    let stats1 = core.hnsw_index_cache_stats();
    assert_eq!(stats1.build_failures, 1);
    assert_eq!(
        stats1.builds, 1,
        "builds counts every IndexedBase::build attempt, including failed ones \
         (Issue #408 codex-review P2 fix: build_failures alone under-reported attempts)"
    );

    // 同一世代内の 2 回目の SELECT は再試行しない（負のキャッシュ）。
    let got2 = query_ids(&core, &ctx, &query);
    assert_eq!(got2, want);
    let stats2 = core.hnsw_index_cache_stats();
    assert_eq!(
        stats2.build_failures, 1,
        "the same generation must not retry the failed build"
    );
    assert_eq!(
        stats2.builds, 1,
        "the negative cache must prevent a second build attempt in the same generation"
    );

    // 再オープンしても同様（負のキャッシュは EngineCore インスタンスに閉じる
    // ため、再オープン後は改めて 1 回失敗するのが正しい fail-closed 挙動）。
    drop(core);
    let storage_reopened = Storage::open(&dir).expect("reopen storage");
    let core2 = open_hnsw_core(storage_reopened);
    let got3 = query_ids(&core2, &ctx, &query);
    assert_eq!(got3, want);
    let stats3 = core2.hnsw_index_cache_stats();
    assert_eq!(stats3.build_failures, 1);
    assert_eq!(
        stats3.builds, 1,
        "reopening starts a fresh cache instance, so this is its own first (failed) attempt"
    );

    // コミット済み行が欠落・重複なく読めることも確認する（RECOVER-9 系の既存
    // 注入試験と同じ観点）。
    for id in [1u64, ROWS as u64, ROWS as u64 + 1] {
        assert!(
            core2.get_row(&ctx, "docs", "tenant-a", id).is_ok(),
            "committed row id={id} must remain readable after a build failure"
        );
    }

    // 毒行を削除（新世代）すると、次の SELECT で索引経路へ復帰する。
    let op_delete = OperationId::parse("hnsw-fail-inject-delete").expect("valid operation_id");
    core2
        .delete_row(&ctx, "docs", ROWS as u64 + 1, Some(&op_delete))
        .expect("delete poison row");

    let got4 = query_ids(&core2, &ctx, &query);
    assert!(
        !got4.contains(&(ROWS as u64 + 1)),
        "the deleted poison row must not appear"
    );
    let stats4 = core2.hnsw_index_cache_stats();
    assert_eq!(
        stats4.builds, 2,
        "the next generation (poison removed) must attempt and succeed a build on top of \
         the earlier failed attempt (1 failed + 1 succeeded)"
    );
    assert_eq!(stats4.build_failures, 1, "no new failures after recovery");
}
