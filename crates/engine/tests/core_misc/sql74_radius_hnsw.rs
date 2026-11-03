//! HNSW を opt-in したエンジンでの半径検索（`WHERE <VECTOR 列> <距離演算子> <リテラル> < r`）の
//! 結合テスト（SQL-74 (2)・Issue #2036）。
//!
//! 距離述語は SCALAR 段（候補マスク）として評価されるので、誤った行（述語を満たさない行）は
//! 決して返らず、距離だけの問い合わせ（`ORDER BY` なし）は厳密走査と完全に一致する。
//! kNN と併用する問い合わせは HNSW の近似結果なので Recall の基準で照合する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::search_engine;
use engine::storage::{RowInput, Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: u32 = 16;
// `MIN_INDEXED_ROWS`（1,024）を超え、構築グラフが決定的になる規模（`hnsw_subset_mask_scan.rs` と同方針）。
const ROWS: usize = 1_200;

struct TestRng(u64);

impl TestRng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
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
        v.iter_mut().for_each(|x| *x /= norm);
    }
}

fn corpus() -> Vec<Vec<f32>> {
    let mut c = TestRng(0xC1C1_C1C1_C1C1_C1C1 ^ 77);
    let centers: Vec<Vec<f32>> = (0..6)
        .map(|_| (0..DIM).map(|_| c.next_unit()).collect())
        .collect();
    let mut rng = TestRng(77);
    (0..ROWS)
        .map(|i| {
            let mut v: Vec<f32> = centers[i % 6]
                .iter()
                .map(|x| x + rng.next_unit() * 0.2)
                .collect();
            normalize(&mut v);
            v
        })
        .collect()
}

fn lit(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", parts.join(","))
}

fn l2(a: &[f32], b: &[f32]) -> f64 {
    f64::from(a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f32>()).sqrt()
}

fn setup(label: &str) -> (EngineCore, PolicyContext, Vec<Vec<f32>>, CleanupGuard) {
    let dir = unique_db_path(label);
    let guard = CleanupGuard(dir.clone());
    let storage = Storage::open(&dir).expect("open storage");
    let schema = TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM), false),
            ColumnDef::new("tag", ColumnType::Text, false),
        ],
    );
    storage.create_table(&schema).expect("create table");
    let vectors = corpus();
    let ctx = PolicyContext::new("tenant-a").expect("valid tenant");
    let metadata =
        engine::row_codec::encode_scalar_columns(&schema, &[Value::Null, Value::Text("x".into())])
            .expect("encode metadata");
    let rows: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                i as u64 + 1,
                RowInput {
                    tenant_id: ctx.tenant_id(),
                    visibility: Visibility::Public,
                    embedding: v.as_slice(),
                    metadata: metadata.as_slice(),
                },
            )
        })
        .collect();
    let op = OperationId::parse(&format!("{label}-seed")).expect("valid op id");
    engine::tenant::insert_rows(&storage, "docs", &ctx, &rows, &op).expect("seed");
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind);
    (core, ctx, vectors, guard)
}

fn ids(core: &EngineCore, ctx: &PolicyContext, sql: &str) -> Vec<u64> {
    core.execute_sql(ctx, sql)
        .unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()))
        .rows
        .iter()
        .map(|r| r.id)
        .collect()
}

/// 距離の隣り合う相異なる値の中点（丸めに依存しないしきい値）。`quantile` は 0..1。
fn radius_at(vectors: &[Vec<f32>], q: &[f32], quantile: f64) -> f64 {
    let mut d: Vec<f64> = vectors.iter().map(|v| l2(v, q)).collect();
    d.sort_by(f64::total_cmp);
    d.dedup();
    let pos = ((d.len() as f64) * quantile) as usize;
    (d[pos] + d[pos + 1]) / 2.0
}

/// 距離だけの問い合わせ（`ORDER BY` なし）は、HNSW を opt-in していても厳密走査の結果と完全に一致する。
#[test]
fn distance_only_query_is_exact_with_hnsw_enabled() {
    let (core, ctx, vectors, _g) = setup("r74-hnsw-exact");
    // 索引を先に構築させる（フィルタ無しの kNN）。
    let q = &vectors[0];
    let _ = ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT 10",
            lit(q)
        ),
    );
    let r = radius_at(&vectors, q, 0.2);
    let mut want: Vec<u64> = (0..ROWS)
        .filter(|i| l2(&vectors[*i], q) < r)
        .map(|i| i as u64 + 1)
        .collect();
    want.sort_unstable();
    let mut got = ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM docs WHERE embedding <-> '{}' < {r:e}",
            lit(q)
        ),
    );
    got.sort_unstable();
    assert_eq!(got, want);
}

/// kNN と併用した半径検索は、述語を満たさない行を決して返さず、参照実装に対する Recall が基準以上。
/// `HINT ORDER(DISTANCE, ...)` を付けても結果は変わらない（取りこぼしを避けるため事前フィルタに倒される）。
#[test]
fn radius_with_knn_never_returns_out_of_radius_rows() {
    let (core, ctx, vectors, _g) = setup("r74-hnsw-knn");
    let _ = ids(
        &core,
        &ctx,
        &format!(
            "SELECT id FROM docs ORDER BY embedding <=> '{}' LIMIT 10",
            lit(&vectors[0])
        ),
    );
    const K: usize = 10;
    let mut total = 0usize;
    let mut hit = 0usize;
    for n in 0..10 {
        let q = &vectors[n * (ROWS / 10)];
        // 半径はクエリの近傍 40% 程度。
        let r = radius_at(&vectors, q, 0.4);
        let sql = format!(
            "SELECT id FROM docs WHERE embedding <-> '{}' < {r:e} \
             ORDER BY embedding <=> '{}' LIMIT {K}",
            lit(q),
            lit(q)
        );
        let got = ids(&core, &ctx, &sql);
        assert_eq!(got.len(), K, "{sql}");
        for id in &got {
            let v = &vectors[(*id - 1) as usize];
            assert!(l2(v, q) < r, "row {id} is outside the radius");
        }
        let hinted = ids(
            &core,
            &ctx,
            &format!("{sql} HINT ORDER(DISTANCE, SCALAR, RLS)"),
        );
        assert_eq!(hinted, got);

        // 参照: 半径内を内積の降順で上位 K（同点は id 昇順）。
        let mut cand: Vec<(u64, f32)> = (0..ROWS)
            .filter(|i| l2(&vectors[*i], q) < r)
            .map(|i| {
                (
                    i as u64 + 1,
                    vectors[i].iter().zip(q).map(|(a, b)| a * b).sum::<f32>(),
                )
            })
            .collect();
        cand.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let want: std::collections::HashSet<u64> = cand.iter().take(K).map(|x| x.0).collect();
        total += K;
        hit += got.iter().filter(|id| want.contains(id)).count();
    }
    let recall = hit as f64 / total as f64;
    assert!(recall >= 0.9, "recall {recall} below the regression floor");
}

/// EXPLAIN が距離述語を含む文でも成功し、`HINT ORDER` の有無で同じ経路を示す。
#[test]
fn explain_reports_same_plan_regardless_of_hint_order() {
    let (core, ctx, vectors, _g) = setup("r74-hnsw-explain");
    let q = lit(&vectors[0]);
    let base = format!(
        "EXPLAIN SELECT id FROM docs WHERE embedding <-> '{q}' < 0.5 \
         ORDER BY embedding <=> '{q}' LIMIT 5"
    );
    let mut session = engine::sql::mode::SessionState::default();
    let mut text = |sql: &str| -> String {
        let out = core.execute_sql_in_session(&ctx, &mut session, sql);
        format!(
            "{:?}",
            out.unwrap_or_else(|e| panic!("{sql}: {}", e.wire_code()))
        )
    };
    let plain = text(&base);
    let hinted = text(&format!("{base} HINT ORDER(DISTANCE, SCALAR, RLS)"));
    assert!(plain.contains("ann_plan"), "{plain}");
    assert_eq!(plain, hinted);
}
