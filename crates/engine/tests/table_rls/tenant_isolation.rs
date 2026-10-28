//! `engine::tenant`（TASK-89・対象ビヘイビア: TABLE-9, TABLE-11）の結合テスト。
//!
//! `crates/engine/tests/rls_prefilter.rs`（TASK-133）のシード手法（決定的
//! xorshift64*・`unique_db_path` 方式）を踏襲し、本番検索経路（`core.rs::EngineCore::search`
//! と `rls.rs::PrefilterIndex::search` の両方）に対して 200 試行 × 4 テナント巡回で
//! テナント境界の混入 0 件（TABLE-11）と可視性判定（TABLE-9）を検証する。

use std::collections::HashSet;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{EngineCore, VectorCore};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::rls::PrefilterIndex;
use engine::storage::{RowInput, Storage, Visibility};
use engine::tenant;

// ---------- 決定的擬似乱数（xorshift64*。`tests/rls_prefilter.rs` と同一実装。外部クレート不使用） ----------

struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn next_f32_signed(&mut self) -> f32 {
        (self.next_f64() * 2.0 - 1.0) as f32
    }
}

// ---------- テスト共通のセットアップ ----------
// 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
// `crates/engine/src/test_util/temp_db.rs` へ一本化した。

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// テナント単位へ分割してシード投入する共通ヘルパ（複数テストへの複製を避けるため
// `src/test_util/seed_rows.rs` へ一本化した。`temp_db.rs` と同じ取り込み方式）。
use crate::seed_rows;
use seed_rows::seed_rows_grouped_by_tenant;

const DIM: u32 = 8;
const TABLE: &str = "docs";
const TENANTS: [&str; 4] = ["tenant-0", "tenant-1", "tenant-2", "tenant-3"];

fn schema() -> TableSchema {
    TableSchema::new(
        TABLE,
        vec![ColumnDef::new("embedding", ColumnType::Vector(DIM), false)],
    )
}

/// シード時に本テストが独立に把握する行の正解データ（`Storage`/`tenant.rs` を経由しない、
/// テスト側だけが持つグラウンドトゥルース）。
struct RowMeta {
    id: u64,
    tenant_idx: usize,
    visibility: Visibility,
}

/// 4 テナント × `rows_per_tenant` 行の決定的な合成コーパスを構築する。テナントごとに
/// 半数程度を `Public`・残りを `Private` にし、埋め込みは `[-1.0, 1.0)` の一様乱数
/// （`DIM` 次元）で生成する。
fn seed_corpus(storage: &Storage, rows_per_tenant: u64, seed: u64) -> Vec<RowMeta> {
    storage.create_table(&schema()).expect("create table");
    let mut rng = Xorshift64::new(seed);
    let mut metas = Vec::with_capacity((rows_per_tenant as usize) * TENANTS.len());
    let mut embeddings: Vec<Vec<f32>> = Vec::with_capacity(metas.capacity());
    let mut id = 1u64;
    for tenant_idx in 0..TENANTS.len() {
        for _ in 0..rows_per_tenant {
            let visibility = if rng.next_f64() < 0.5 {
                Visibility::Public
            } else {
                Visibility::Private
            };
            embeddings.push((0..DIM).map(|_| rng.next_f32_signed()).collect());
            metas.push(RowMeta {
                id,
                tenant_idx,
                visibility,
            });
            id += 1;
        }
    }
    let rows: Vec<(u64, RowInput<'_>)> = metas
        .iter()
        .zip(embeddings.iter())
        .map(|(m, emb)| {
            (
                m.id,
                RowInput {
                    tenant_id: TENANTS[m.tenant_idx],
                    visibility: m.visibility,
                    embedding: emb,
                    metadata: &[],
                },
            )
        })
        .collect();
    seed_rows_grouped_by_tenant(storage, TABLE, &rows);
    metas
}

fn random_query(rng: &mut Xorshift64) -> Vec<f32> {
    (0..DIM).map(|_| rng.next_f32_signed()).collect()
}

/// `metas`（テスト側のグラウンドトゥルース）から、`ctx` の可視性述語で可視な id 集合を
/// 独立に算出する。`PolicyContext::is_visible` だけを使い、`Storage`/`tenant.rs` の実装を
/// 経由しないため、`tenant.rs::visible_rows` の実装バグからも独立したオラクルになる。
fn oracle_visible_ids(metas: &[RowMeta], ctx: &PolicyContext) -> HashSet<u64> {
    metas
        .iter()
        .filter(|m| ctx.is_visible(TENANTS[m.tenant_idx], m.visibility))
        .map(|m| m.id)
        .collect()
}

// 対象ビヘイビア: TABLE-11。200 試行 × 4 テナント巡回で、本番検索経路
// （`PrefilterIndex::search`・`EngineCore::search` の両方）が返す Top-k が、テスト側の
// 独立オラクルの可視集合に必ず収まることを検証する（テナント境界 P0。全試行合計で
// 混入 0 件）。`PrefilterIndex` 経路は `tenant.rs::verify_hits` でも重ねて検証する
// （`tenant.rs` 自体の結合検証を兼ねる）。
#[test]
fn table11_zero_cross_tenant_leakage_across_200_trials_via_both_search_paths() {
    const ROWS_PER_TENANT: u64 = 50; // 4 テナント合計 200 行。
    const TRIALS: u64 = 200;
    const K: usize = 10;

    let path = unique_db_path("table11");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let metas = seed_corpus(&storage, ROWS_PER_TENANT, 5000);

    let ctxs: Vec<PolicyContext> = TENANTS
        .iter()
        .map(|t| PolicyContext::new(t).expect("valid tenant"))
        .collect();
    let oracles: Vec<HashSet<u64>> = ctxs
        .iter()
        .map(|ctx| oracle_visible_ids(&metas, ctx))
        .collect();

    // フェーズ 1: `PrefilterIndex` 経路（`&Storage` を借用する）。テナントごとに
    // インデックスを 1 回だけ構築し、以降の試行で使い回す。
    let mut foreign_visible_hit_seen = false; // TABLE-9 の正方向検証用。
    {
        let indices: Vec<PrefilterIndex<'_>> = ctxs
            .iter()
            .map(|ctx| PrefilterIndex::build(&storage, TABLE, ctx).expect("build prefilter index"))
            .collect();

        let mut rng = Xorshift64::new(6000);
        for trial in 0..TRIALS {
            let tenant_idx = (trial % TENANTS.len() as u64) as usize;
            let ctx = &ctxs[tenant_idx];
            let query = random_query(&mut rng);
            let hits = indices[tenant_idx]
                .search(ctx, &CpuScalarProvider, &query, K)
                .expect("prefilter search ok");
            let hit_ids: Vec<u64> = hits.iter().map(|h| h.id).collect();

            // 独立検証 1: テスト側オラクルとの照合（テナント境界 P0）。
            for id in &hit_ids {
                assert!(
                    oracles[tenant_idx].contains(id),
                    "cross-tenant leak via PrefilterIndex: trial={trial} tenant={} id={id}",
                    TENANTS[tenant_idx]
                );
                let meta = metas.iter().find(|m| m.id == *id).expect("id must exist");
                if meta.tenant_idx != tenant_idx {
                    foreign_visible_hit_seen = true;
                }
            }

            // 独立検証 2: `tenant.rs::verify_hits`（本タスクの統合層自体の検証を兼ねる）。
            // 照合は `(tenant_id, id)` の完全な行キーで行う（TABLE-12・RLS-9。
            // `hits` はテナント修飾済み）。
            tenant::verify_hits(&storage, TABLE, ctx, &hits)
                .expect("tenant::verify_hits must accept PrefilterIndex hits");
        }
        // `indices` はここで drop され、`storage` の借用が終わる（次フェーズで
        // `EngineCore::from_storage` へ所有権を移すための準備）。
    }

    // フェーズ 2: `EngineCore::search` 経路（`Storage` の所有権を取る）。
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let mut rng = Xorshift64::new(6001);
    for trial in 0..TRIALS {
        let tenant_idx = (trial % TENANTS.len() as u64) as usize;
        let ctx = &ctxs[tenant_idx];
        let query = random_query(&mut rng);
        let hits = core.search(ctx, TABLE, &query, K).expect("core search ok");

        for hit in &hits {
            assert!(
                oracles[tenant_idx].contains(&hit.id),
                "cross-tenant leak via EngineCore::search: trial={trial} tenant={} id={}",
                TENANTS[tenant_idx],
                hit.id
            );
            let meta = metas
                .iter()
                .find(|m| m.id == hit.id)
                .expect("id must exist");
            if meta.tenant_idx != tenant_idx {
                foreign_visible_hit_seen = true;
            }
        }
    }

    // 対象ビヘイビア: TABLE-9（正方向）。混入 0 件だけでは判定の正方向側の退行を
    // 検知できないため、他テナントの `Public` 行が実際に Top-k へ現れた試行が
    // 1 件以上あったことも確認する。
    assert!(
        foreign_visible_hit_seen,
        "expected at least one trial to surface a cross-tenant hit (see PolicyContext::is_visible)"
    );
}

// 対象ビヘイビア: TABLE-9（fail-closed）。詳細は `PolicyContext::is_visible` を参照。
#[test]
fn table9_fail_closed_without_public_grant_and_private_never_crosses_tenant() {
    const ROWS_PER_TENANT: u64 = 30;
    const K: usize = 50;

    let path = unique_db_path("table9-fail-closed");
    let _cleanup = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let metas = seed_corpus(&storage, ROWS_PER_TENANT, 7000);

    // `Private` のみ許可（`Public` を含まない）の tenant-0 ctx。
    let ctx_private_only =
        PolicyContext::with_visibilities(TENANTS[0], [Visibility::Private]).expect("valid tenant");
    let own_ids: HashSet<u64> = metas
        .iter()
        .filter(|m| m.tenant_idx == 0)
        .map(|m| m.id)
        .collect();
    let visible =
        tenant::visible_rows(&storage, TABLE, &ctx_private_only).expect("visible_rows ok");
    for row in &visible {
        assert_eq!(
            row.tenant_id, TENANTS[0],
            "Private-only ctx must never see another tenant's row (id={})",
            row.id
        );
        assert_eq!(
            row.visibility,
            Visibility::Private,
            "Private-only ctx must only see Private rows (see PolicyContext::is_visible) (id={})",
            row.id
        );
    }
    // 期待集合（tenant-0 の Private 行のみ）とも一致することを確認する。
    let expected: HashSet<u64> = metas
        .iter()
        .filter(|m| {
            m.tenant_idx == 0 && m.visibility == Visibility::Private && own_ids.contains(&m.id)
        })
        .map(|m| m.id)
        .collect();
    let actual: HashSet<u64> = visible.iter().map(|r| r.id).collect();
    assert_eq!(actual, expected);

    // `Public`・`Private` 両方許可の ctx でも、`Private` 行は依然として他テナントへは
    // 越境しないことを確認する（ポインタ: TASK-89 / TABLE-9）。
    let ctx_both =
        PolicyContext::with_visibilities(TENANTS[0], [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    let visible_both = tenant::visible_rows(&storage, TABLE, &ctx_both).expect("visible_rows ok");
    for row in &visible_both {
        if row.visibility == Visibility::Private {
            assert_eq!(
                row.tenant_id, TENANTS[0],
                "Private row must never be visible across tenants even with an explicit grant (id={})",
                row.id
            );
        }
    }
    let query = vec![0.0f32; DIM as usize];
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let hits = core
        .search(&ctx_private_only, TABLE, &query, K)
        .expect("core search ok");
    for hit in &hits {
        assert!(
            expected.contains(&hit.id),
            "EngineCore::search must respect the Private-only, fail-closed visible set (id={})",
            hit.id
        );
    }
}
