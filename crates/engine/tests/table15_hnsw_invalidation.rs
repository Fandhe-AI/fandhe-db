//! TRUNCATE（SQL-22）と DROP TABLE（TABLE-15）後の HNSW 索引キャッシュ失効
//! （Issue #1200。関連ビヘイビア: SQL-18・RLS-9。ポインタのみ）の結合テスト。
//!
//! `sql::hnsw_cache::HnswIndexCache` は `(table, ctx)` とテーブル世代を突き合わせる
//! 設計で、TRUNCATE／DROP は同一 write txn 内で世代を進める。既存の
//! `truncate_table.rs`／`sql_drop_table.rs` は arena・可視ビットマップと
//! `Storage` 再オープン越しの再作成までしか固定していないため、本ファイルは
//! **同一プロセス（`EngineCore` を保持したまま）** で以下を固定する:
//!
//! 1. 索引を温めた後の TRUNCATE で古い id が 1 件も返らず、再投入後の結果は
//!    新しい id 帯のみで、既定エンジン（brute-force 対照）に対する Recall@10 が
//!    回帰基準（0.9）以上であること（統計カウンタで索引経路の非 vacuous を確認）
//! 2. DROP → 同名 CREATE でも同様であること
//! 3. 他テナント（Private 行のみで組む。`Public` はグローバル可視のため）の結果が
//!    TRUNCATE の前後で不変、DROP 後は `42P01` であること
//!
//! フィクスチャ規模・乱数は `hnsw_cache.rs`（`MIN_INDEXED_ROWS` 超・決定的）と同じ。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::search_engine;
use engine::sql::mode::SessionState;
use engine::storage::{RowInput, Storage, Visibility};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

struct TestRng {
    state: u64,
}

impl TestRng {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_unit(&mut self) -> f32 {
        let bits = (self.next_u64() >> 40) as u32;
        (bits as f32) / (1u32 << 24) as f32 * 2.0 - 1.0
    }
}

fn normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

/// `hnsw_cache.rs::gen_clustered_corpus` と同型の決定的コーパス。
fn gen_clustered_corpus(seed: u64, dim: usize, rows: usize, clusters: usize) -> Vec<Vec<f32>> {
    let mut center_rng = TestRng::new(seed ^ 0xC1C1_C1C1_C1C1_C1C1);
    let centers: Vec<Vec<f32>> = (0..clusters.max(1))
        .map(|_| (0..dim).map(|_| center_rng.next_unit()).collect())
        .collect();
    let mut rng = TestRng::new(seed);
    (0..rows)
        .map(|i| {
            let center = &centers[i % centers.len()];
            let mut v: Vec<f32> = center.iter().map(|c| c + rng.next_unit() * 0.2).collect();
            normalize(&mut v);
            v
        })
        .collect()
}

const DIM: u32 = 16;
const ROWS: usize = 1_200;
const OLD_BASE: u64 = 1;
const NEW_BASE: u64 = 100_001;
const BOB_BASE: u64 = 500_001;

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![ColumnDef::new("embedding", ColumnType::Vector(DIM), false)],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn row<'a>(tenant: &'a str, v: &'a [f32]) -> RowInput<'a> {
    RowInput {
        tenant_id: tenant,
        visibility: Visibility::Private,
        embedding: v,
        metadata: &[],
    }
}

/// `Storage` 直接（一括書き込み）で投入する。`core` 構築前の初期投入用。
fn seed_storage(storage: &Storage, tenant: &str, start: u64, vectors: &[Vec<f32>], tag: &str) {
    let rows: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| (start + i as u64, row(tenant, v)))
        .collect();
    let op = OperationId::parse(&format!("t15-seed-{tag}")).expect("valid op id");
    engine::tenant::insert_rows(storage, "docs", &ctx(tenant), &rows, &op).expect("seed rows");
}

/// `EngineCore` 経由（1 行ずつ）で投入する。TRUNCATE／再作成後の再投入用。
fn seed_core(core: &EngineCore, tenant: &str, start: u64, vectors: &[Vec<f32>], tag: &str) {
    let c = ctx(tenant);
    for (i, v) in vectors.iter().enumerate() {
        let op = OperationId::parse(&format!("t15-{tag}-{i}")).expect("valid op id");
        core.insert_row(&c, "docs", start + i as u64, &row(tenant, v), Some(&op))
            .expect("insert row");
    }
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

fn query_sql(q: &[f32]) -> String {
    format!(
        "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT 10",
        vec_literal(q)
    )
}

fn query_ids(core: &EngineCore, c: &PolicyContext, q: &[f32]) -> Vec<u64> {
    core.execute_sql(c, &query_sql(q))
        .expect("query should succeed")
        .rows
        .iter()
        .map(|r| r.id)
        .collect()
}

fn recall_at_k(got: &[u64], want: &[u64]) -> f64 {
    if want.is_empty() {
        return 1.0;
    }
    let set: std::collections::HashSet<u64> = want.iter().copied().collect();
    got.iter().filter(|id| set.contains(id)).count() as f64 / want.len() as f64
}

fn sql_in_session(core: &EngineCore, c: &PolicyContext, sql: &str) {
    let mut session = SessionState::default();
    session.allow_ddl();
    core.execute_sql_in_session(c, &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"));
}

fn queries_from(corpus: &[Vec<f32>]) -> Vec<Vec<f32>> {
    (0..20).map(|i| corpus[i * (ROWS / 20)].clone()).collect()
}

/// 温め: 索引を構築し、hit を発生させる（`builds == 1`・`hits > 0`）。
fn warm_up(core: &EngineCore, alice: &PolicyContext, queries: &[Vec<f32>]) {
    for q in queries {
        let ids = query_ids(core, alice, q);
        assert_eq!(ids.len(), 10);
    }
    let stats = core.hnsw_index_cache_stats();
    assert_eq!(stats.builds, 1, "warm-up must build the HNSW index once");
    assert!(stats.hits > 0, "warm-up must reuse the index");
}

/// 失効後の共通オラクル。旧 id 帯の混入 0 件・対照 Recall・索引再構築・再利用。
fn assert_fresh_generation(
    core: &EngineCore,
    alice: &PolicyContext,
    new_corpus: &[Vec<f32>],
    builds_before: u64,
) {
    let ref_path = unique_db_path("t15-hnsw-ref");
    let _ref_guard = CleanupGuard(ref_path.clone());
    let ref_storage = Storage::open(&ref_path).expect("open ref storage");
    ref_storage.create_table(&schema()).expect("create ref");
    seed_storage(&ref_storage, "tenant-a", NEW_BASE, new_corpus, "ref");
    let ref_core = EngineCore::from_storage(ref_storage, search_engine::default_engine());

    // 温め段階の hit と区別するため、再投入後のクエリ前の hit 数を控える。
    let hits_before = core.hnsw_index_cache_stats().hits;
    let mut recalls = Vec::new();
    for q in &queries_from(new_corpus) {
        let got = query_ids(core, alice, q);
        assert_eq!(got.len(), 10);
        assert!(
            got.iter()
                .all(|id| (NEW_BASE..NEW_BASE + ROWS as u64).contains(id)),
            "stale (pre-invalidation) ids must never be returned: {got:?}"
        );
        let want = query_ids(&ref_core, alice, q);
        recalls.push(recall_at_k(&got, &want));
    }
    let avg = recalls.iter().sum::<f64>() / recalls.len() as f64;
    assert!(avg >= 0.9, "recall against brute-force too low: {avg}");
    let stats = core.hnsw_index_cache_stats();
    assert!(
        stats.builds > builds_before,
        "HNSW index must be rebuilt for the new generation"
    );
    assert!(
        stats.hits > hits_before,
        "rebuilt index must be reused by the post-invalidation queries"
    );
}

fn new_hnsw_core(path: &std::path::Path) -> (EngineCore, Vec<Vec<f32>>) {
    let storage = Storage::open(path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let old_corpus = gen_clustered_corpus(1, DIM as usize, ROWS, 12);
    seed_storage(&storage, "tenant-a", OLD_BASE, &old_corpus, "old-a");
    // 他テナントの Private 行（MIN_INDEXED_ROWS 未満）。
    let bob_corpus = gen_clustered_corpus(7, DIM as usize, 20, 4);
    seed_storage(&storage, "tenant-b", BOB_BASE, &bob_corpus, "old-b");
    let kind = search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("params");
    (
        EngineCore::from_storage_with_engine(storage, kind),
        old_corpus,
    )
}

/// SQL-22・SQL-18: TRUNCATE 後、HNSW キャッシュから古い行が返らない。他テナントは不変。
#[test]
fn truncate_invalidates_hnsw_index_cache() {
    let path = unique_db_path("t15-hnsw-truncate");
    let _guard = CleanupGuard(path.clone());
    let (core, old_corpus) = new_hnsw_core(&path);
    let alice = ctx("tenant-a");
    let bob = ctx("tenant-b");
    let queries = queries_from(&old_corpus);
    warm_up(&core, &alice, &queries);
    let bob_q = gen_clustered_corpus(7, DIM as usize, 20, 4)[0].clone();
    let bob_before = query_ids(&core, &bob, &bob_q);
    assert_eq!(bob_before.len(), 10);
    assert!(bob_before.iter().all(|id| *id >= BOB_BASE));
    let builds_before = core.hnsw_index_cache_stats().builds;

    sql_in_session(
        &core,
        &alice,
        "TRUNCATE TABLE docs USING OPERATION_ID 't15-trunc'",
    );
    for q in &queries {
        assert!(
            query_ids(&core, &alice, q).is_empty(),
            "TRUNCATEd tenant must see no rows"
        );
    }
    assert_eq!(query_ids(&core, &bob, &bob_q), bob_before);

    let new_corpus = gen_clustered_corpus(2, DIM as usize, ROWS, 12);
    seed_core(&core, "tenant-a", NEW_BASE, &new_corpus, "new-a");
    assert_fresh_generation(&core, &alice, &new_corpus, builds_before);
    assert_eq!(query_ids(&core, &bob, &bob_q), bob_before);
}

/// TABLE-15: 同一プロセスでの DROP → 同名 CREATE でも HNSW キャッシュ経由の古いヒットが無い。
#[test]
fn drop_then_recreate_invalidates_hnsw_index_cache_in_same_process() {
    let path = unique_db_path("t15-hnsw-drop");
    let _guard = CleanupGuard(path.clone());
    let (core, old_corpus) = new_hnsw_core(&path);
    let alice = ctx("tenant-a");
    let bob = ctx("tenant-b");
    let queries = queries_from(&old_corpus);
    warm_up(&core, &alice, &queries);
    let builds_before = core.hnsw_index_cache_stats().builds;

    sql_in_session(&core, &alice, "DROP TABLE docs");
    // 削除直後は誰の ctx でも 42P01（他テナントの存在情報にも依存しない）。
    for c in [&alice, &bob] {
        let err = core
            .execute_sql(c, &query_sql(&queries[0]))
            .expect_err("dropped table must be undefined");
        assert_eq!(err.wire_code(), "42P01");
    }

    sql_in_session(
        &core,
        &alice,
        &format!("CREATE TABLE docs (embedding VECTOR({DIM}))"),
    );
    let new_corpus = gen_clustered_corpus(2, DIM as usize, ROWS, 12);
    seed_core(&core, "tenant-a", NEW_BASE, &new_corpus, "new-a");
    assert_fresh_generation(&core, &alice, &new_corpus, builds_before);
    // 他テナントの旧行は DROP で消えており、再作成後のテーブルには存在しない。
    assert!(query_ids(&core, &bob, &queries[0]).is_empty());
}
