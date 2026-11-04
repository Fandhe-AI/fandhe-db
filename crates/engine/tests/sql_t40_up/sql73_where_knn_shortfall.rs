//! 素の `WHERE` 付き kNN（`SELECT .. WHERE <述語> ORDER BY emb <=> q LIMIT k`。hybrid ではない）の
//! 返却件数・recall を選択率別に計測する回帰テスト兼ハーネス（Issue #2032・親 #2031・
//! SQL-73 のポインタ。結果の表は `docs/design/hnsw-hybrid-iterative-scan.md` の Issue #2032 節）。
//!
//! 計測軸は「選択率 × k × 評価順（既定／`HINT ORDER(DISTANCE, SCALAR, RLS)`）× エンジン
//! （brute-force／HNSW opt-in）」。HNSW 側は索引ベースを事前に構築（ウォームアップ）し、
//! `hnsw_index_cache_stats()` の差分で実際に通った経路を分類する（分類規則は
//! `hnsw_crossdb_selectivity.rs::observed_arm_label` と同一）。tenant-b を同居させ、
//! 全計測点で越境が無いことも検査する（RLS・P0）。production コードは無変更。
//!
//! 固定する性質（#2033 で DISTANCE 先行の挙動を変える場合は、本ファイルの該当アサーションと
//! 設計文書の表を同じ PR で更新すること）:
//!
//! - 既定の評価順は両エンジンで常に `min(k, 一致行数)` 件を返す（取りこぼさない）
//! - DISTANCE 先行（SCALAR 事後フィルタ）は選択率が低いほど件数が不足する
//!
//! 表の出力: `cargo test -p fandhe-db-engine --test sql_t40_up sql73_where_knn_shortfall:: -- --nocapture`

use std::collections::{BTreeSet, HashSet};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::search_engine::{self, HnswScope};
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};

use crate::temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 16;
const ROWS_A: usize = 4_000;
const ROWS_B: usize = 1_000;
const TENANT_B_BASE: u64 = 1_000_000;
const QUERIES: usize = 20;
const CLUSTERS: usize = 10;

/// 決定的な xorshift。`[-1, 1)` の一様乱数を返す。
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        let bits = (self.0.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as u32;
        bits as f32 / (1u32 << 24) as f32 * 2.0 - 1.0
    }
}

fn normalize(v: &mut [f32]) {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-12);
    v.iter_mut().for_each(|x| *x /= n);
}

fn gen_rows(seed: u64, rows: usize, centers: &[Vec<f32>]) -> Vec<Vec<f32>> {
    let mut rng = Rng(seed);
    (0..rows)
        .map(|i| {
            let c = &centers[i % CLUSTERS];
            let mut v: Vec<f32> = c.iter().map(|x| x + 0.6 * rng.next()).collect();
            normalize(&mut v);
            v
        })
        .collect()
}

fn dist(a: &[f32], b: &[f32]) -> f32 {
    1.0 - a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>()
}

/// 一様に散った整数列。`bucket < X` で選択率 X/1000 になる（各値がちょうど 4 行）。
fn bucket_of(i: usize) -> i32 {
    ((i * 7919) % 1000) as i32
}

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(DIM as u32), false),
            ColumnDef::new("bucket", ColumnType::Integer, false),
            ColumnDef::new("cluster", ColumnType::Text, false),
        ],
    )
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn seed(storage: &Storage, tenant: &str, id_base: u64, vectors: &[Vec<f32>]) {
    let schema = schema();
    let meta: Vec<Vec<u8>> = (0..vectors.len())
        .map(|i| {
            encode_scalar_columns(
                &schema,
                &[
                    Value::Null,
                    Value::Integer(bucket_of(i)),
                    Value::Text(format!("c{}", i % CLUSTERS)),
                ],
            )
            .expect("encode metadata")
        })
        .collect();
    let rows: Vec<(u64, RowInput<'_>)> = vectors
        .iter()
        .enumerate()
        .map(|(i, v)| {
            (
                id_base + i as u64,
                RowInput {
                    tenant_id: tenant,
                    visibility: Visibility::Private,
                    embedding: v.as_slice(),
                    metadata: meta[i].as_slice(),
                },
            )
        })
        .collect();
    let op = OperationId::parse(&format!("seed-{tenant}")).expect("valid op id");
    engine::tenant::insert_rows(storage, "docs", &ctx(tenant), &rows, &op).expect("seed rows");
}

fn lit(v: &[f32]) -> String {
    let p: Vec<String> = v.iter().map(|x| x.to_string()).collect();
    format!("[{}]", p.join(","))
}

/// 計測用フィクスチャ（tenant-a: 4,000 行・tenant-b: 1,000 行）。
struct Fixture {
    core: EngineCore,
    vectors: Vec<Vec<f32>>,
    queries: Vec<Vec<f32>>,
    hnsw: bool,
    _guard: CleanupGuard,
}

impl Fixture {
    fn new(label: &str, hnsw: bool) -> Self {
        let path = unique_db_path(label);
        let guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create docs");
        let mut rng = Rng(0x2032_0001);
        let centers: Vec<Vec<f32>> = (0..CLUSTERS)
            .map(|_| {
                let mut c: Vec<f32> = (0..DIM).map(|_| rng.next()).collect();
                normalize(&mut c);
                c
            })
            .collect();
        let vectors = gen_rows(0x2032_0002, ROWS_A, &centers);
        let vectors_b = gen_rows(0x2032_0003, ROWS_B, &centers);
        seed(&storage, "tenant-a", 1, &vectors);
        seed(&storage, "tenant-b", TENANT_B_BASE, &vectors_b);
        let queries = gen_rows(0x2032_0004, QUERIES, &centers);
        let core = if hnsw {
            let kind = search_engine::hnsw_kind(engine::hnsw::HnswParams::default())
                .expect("valid hnsw params");
            EngineCore::from_storage_with_engine(storage, kind).with_hnsw_scope(HnswScope::Declared)
        } else {
            EngineCore::from_storage(storage, search_engine::default_engine())
        };
        let fx = Self {
            core,
            vectors,
            queries,
            hnsw,
            _guard: guard,
        };
        if hnsw {
            let mut s = SessionState::default();
            s.allow_ddl();
            fx.core
                .execute_sql_in_session(
                    &ctx("tenant-a"),
                    &mut s,
                    "CREATE INDEX docs_embedding_hnsw ON docs USING hnsw (embedding)",
                )
                .expect("declare hnsw");
            // `Subset` 経路は索引ベース未構築だと plain scan へ縮退する契約のため、
            // フィルタなし kNN を 1 回流してベースを構築させる。
            let before = fx.core.hnsw_index_cache_stats().builds;
            fx.run(&mut SessionState::default(), &fx.sql(None, 10, false, 0));
            assert!(
                fx.core.hnsw_index_cache_stats().builds > before,
                "warm-up must build the base"
            );
        }
        fx
    }

    fn sql(&self, pred: Option<&str>, k: usize, distance_first: bool, q: usize) -> String {
        let w = pred.map(|p| format!(" WHERE {p}")).unwrap_or_default();
        let h = if distance_first {
            " HINT ORDER(DISTANCE, SCALAR, RLS)"
        } else {
            ""
        };
        format!(
            "SELECT id FROM docs{w} ORDER BY embedding <=> '{}' LIMIT {k}{h}",
            lit(&self.queries[q])
        )
    }

    fn run(&self, s: &mut SessionState, sql: &str) -> Vec<u64> {
        match self
            .core
            .execute_sql_in_session(&ctx("tenant-a"), s, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        {
            SqlOutcome::Query(r) => r
                .rows
                .iter()
                .filter_map(|row| match row.cells.first() {
                    Some(Cell::Integer(v)) => Some(*v),
                    _ => None,
                })
                .collect(),
            other => panic!("expected Query, got {other:?}"),
        }
    }

    fn run_set(&self, s: &mut SessionState, ef: u32) {
        self.core
            .execute_sql_in_session(&ctx("tenant-a"), s, &format!("SET hnsw.ef_search = {ef}"))
            .expect("set ef_search");
    }

    fn ann_plan(&self, sql: &str) -> String {
        let mut s = SessionState::default();
        match self
            .core
            .execute_sql_in_session(&ctx("tenant-a"), &mut s, &format!("EXPLAIN {sql}"))
            .expect("explain")
        {
            SqlOutcome::Explain(r) => r
                .rows
                .iter()
                .filter_map(|row| match row.cells.first() {
                    Some(Cell::Text(t)) => t.strip_prefix("ann_plan: ").map(str::to_string),
                    _ => None,
                })
                .next()
                .expect("ann_plan line"),
            other => panic!("expected Explain, got {other:?}"),
        }
    }
}

/// 述語（SQL 断片と行ごとの判定）。
struct Pred {
    name: &'static str,
    sql: Option<&'static str>,
    matches: fn(usize) -> bool,
}

fn preds() -> Vec<Pred> {
    macro_rules! b {
        ($name:expr, $sql:expr, $x:expr) => {
            Pred {
                name: $name,
                sql: Some($sql),
                matches: |i| bucket_of(i) < $x,
            }
        };
    }
    vec![
        Pred {
            name: "none (100%)",
            sql: None,
            matches: |_| true,
        },
        b!("bucket<500 (50%)", "bucket < 500", 500),
        b!("bucket<200 (20%)", "bucket < 200", 200),
        b!("bucket<100 (10%)", "bucket < 100", 100),
        b!("bucket<99 (9.9%)", "bucket < 99", 99),
        b!("bucket<50 (5%)", "bucket < 50", 50),
        b!("bucket<10 (1%)", "bucket < 10", 10),
        b!("bucket<3 (0.3%, 12 rows)", "bucket < 3", 3),
        b!("bucket<2 (0.2%, 8 rows)", "bucket < 2", 2),
        b!("bucket<0 (0%)", "bucket < 0", 0),
        Pred {
            name: "cluster=c0 (10%)",
            sql: Some("cluster = 'c0'"),
            matches: |i| i % CLUSTERS == 0,
        },
        Pred {
            name: "cluster IN c0,c1 (20%)",
            sql: Some("cluster IN ('c0', 'c1')"),
            matches: |i| i % CLUSTERS < 2,
        },
        Pred {
            name: "cluster IN c0..c4 (50%)",
            sql: Some("cluster IN ('c0', 'c1', 'c2', 'c3', 'c4')"),
            matches: |i| i % CLUSTERS < 5,
        },
    ]
}

/// 厳密なコサイン距離の top-k（`allowed` を満たす行 index の中から。id = index + 1）。
fn oracle(vectors: &[Vec<f32>], q: &[f32], k: usize, allowed: impl Fn(usize) -> bool) -> Vec<u64> {
    let mut d: Vec<(f32, u64)> = vectors
        .iter()
        .enumerate()
        .filter(|(i, _)| allowed(*i))
        .map(|(i, v)| (dist(v, q), i as u64 + 1))
        .collect();
    d.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    d.into_iter().take(k).map(|(_, id)| id).collect()
}

/// `hnsw_crossdb_selectivity.rs::observed_arm_label` と同一の優先順位。
fn arm(acorn: u64, subset: u64, plain: u64, split: u64, short: u64, full: u64) -> &'static str {
    if acorn > 0 {
        "ann_masked_two_hop"
    } else if subset > 0 {
        "ann_masked"
    } else if plain > 0 {
        "plain_scan_ratio"
    } else if split > 0 {
        "plain_scan_mask_split"
    } else if short > 0 {
        "plain_scan_masked_short"
    } else if full > 0 {
        "ann_full_visible"
    } else {
        "plain_scan_other"
    }
}

struct Measure {
    avg_returned: f64,
    min_returned: usize,
    /// 全クエリで `min(k, 一致行数)` 件を返したか。
    never_short: bool,
    /// 全クエリで id 集合が「現状仕様上の期待集合」と一致したか。
    exact_expected: bool,
    fill: f64,
    /// 一致行の真の top-k に対する再現率。
    recall: f64,
    arms: BTreeSet<&'static str>,
    ann_plan: String,
    want_len: usize,
}

/// 1 計測点（述語 × k × 評価順）を全クエリで実行し、不変条件を検査して集計する。
fn measure(fx: &Fixture, p: &Pred, k: usize, distance_first: bool, ef: Option<u32>) -> Measure {
    let matches = (0..ROWS_A).filter(|i| (p.matches)(*i)).count();
    let want_len = k.min(matches);
    let mut s = SessionState::default();
    if let Some(ef) = ef {
        fx.run_set(&mut s, ef);
    }
    let ann_plan = fx.ann_plan(&fx.sql(p.sql, k, distance_first, 0));
    let (mut ret_sum, mut exp_sum, mut min_ret) = (0usize, 0usize, usize::MAX);
    let (mut hit, mut truth) = (0usize, 0usize);
    let (mut never_short, mut exact_expected) = (true, true);
    let mut arms = BTreeSet::new();
    for q in 0..QUERIES {
        let before = fx.core.hnsw_index_cache_stats();
        let got = fx.run(&mut s, &fx.sql(p.sql, k, distance_first, q));
        let after = fx.core.hnsw_index_cache_stats();
        if fx.hnsw {
            arms.insert(arm(
                after.acorn_searches - before.acorn_searches,
                after.subset_searches - before.subset_searches,
                after.plain_scans - before.plain_scans,
                after.mask_splits_graph - before.mask_splits_graph,
                after.masked_short - before.masked_short,
                after.hits - before.hits,
            ));
        }
        // 全計測点の不変条件: 件数上限・述語充足・越境なし・重複なし・距離昇順。
        assert!(got.len() <= k, "{}: more than k rows", p.name);
        let got_set: HashSet<u64> = got.iter().copied().collect();
        assert_eq!(got_set.len(), got.len(), "{}: duplicate ids", p.name);
        for id in &got {
            assert!(
                (1..=ROWS_A as u64).contains(id),
                "{}: id outside tenant-a: crossing the tenant boundary",
                p.name
            );
            assert!(
                (p.matches)(*id as usize - 1),
                "{}: predicate violated",
                p.name
            );
        }
        let ds: Vec<f32> = got
            .iter()
            .map(|id| dist(&fx.vectors[*id as usize - 1], &fx.queries[q]))
            .collect();
        assert!(
            ds.windows(2).all(|w| w[0] <= w[1] + 1e-5),
            "{}: not sorted",
            p.name
        );

        let truth_ids = oracle(&fx.vectors, &fx.queries[q], k, |i| (p.matches)(i));
        let expected: HashSet<u64> = if distance_first {
            oracle(&fx.vectors, &fx.queries[q], k, |_| true)
                .into_iter()
                .filter(|id| (p.matches)(*id as usize - 1))
                .collect()
        } else {
            truth_ids.iter().copied().collect()
        };
        exact_expected &= got_set == expected;
        never_short &= got.len() == want_len;
        ret_sum += got.len();
        exp_sum += want_len;
        min_ret = min_ret.min(got.len());
        hit += truth_ids.iter().filter(|id| got_set.contains(id)).count();
        truth += truth_ids.len();
    }
    Measure {
        avg_returned: ret_sum as f64 / QUERIES as f64,
        min_returned: min_ret,
        never_short,
        exact_expected,
        fill: if exp_sum == 0 {
            1.0
        } else {
            ret_sum as f64 / exp_sum as f64
        },
        recall: if truth == 0 {
            1.0
        } else {
            hit as f64 / truth as f64
        },
        arms,
        ann_plan,
        want_len,
    }
}

fn row(engine: &str, order: &str, p: &Pred, k: usize, ef: Option<u32>, m: &Measure) -> String {
    let route = if m.arms.is_empty() {
        "-".to_string()
    } else {
        m.arms.iter().copied().collect::<Vec<_>>().join("+")
    };
    format!(
        "| {engine} | {order} | {} | {k} | {} | {:.1} / {} (want {}) | {:.3} | {:.3} | {route} | {} |",
        p.name,
        ef.map_or("default".to_string(), |e| e.to_string()),
        m.avg_returned,
        m.min_returned,
        m.want_len,
        m.fill,
        m.recall,
        m.ann_plan,
    )
}

/// SQL-73: 既定の評価順（RLS→SCALAR→DISTANCE）は両エンジン・全選択率で
/// `min(k, 一致行数)` 件を返し、brute-force では厳密（recall 1.0）。越境も無い。
#[test]
fn default_order_never_falls_short_on_both_engines() {
    for hnsw in [false, true] {
        let fx = Fixture::new(
            if hnsw {
                "sql73-def-hnsw"
            } else {
                "sql73-def-bf"
            },
            hnsw,
        );
        for p in preds() {
            for k in [10usize, 100] {
                let m = measure(&fx, &p, k, false, None);
                assert!(
                    m.never_short,
                    "hnsw={hnsw} {} k={k}: default order fell short",
                    p.name
                );
                if !hnsw {
                    assert!(
                        m.exact_expected && m.recall == 1.0,
                        "bf {} k={k} not exact",
                        p.name
                    );
                    assert!(
                        !m.ann_plan.starts_with("hnsw"),
                        "{}: {}",
                        p.name,
                        m.ann_plan
                    );
                } else if p.sql.is_some() {
                    assert!(
                        m.ann_plan.starts_with("hnsw_subset"),
                        "{}: {}",
                        p.name,
                        m.ann_plan
                    );
                    // plain scan へ縮退した点は厳密一致。
                    if m.arms.iter().all(|a| a.starts_with("plain_scan")) {
                        assert!(
                            m.exact_expected,
                            "hnsw {} k={k}: plain scan must be exact",
                            p.name
                        );
                    }
                }
            }
        }
    }
}

/// SQL-73: DISTANCE 先行（SCALAR 事後フィルタ）は可視全集合の top-k を先に取るため、
/// brute-force では返却 id が「全可視 top-k ∩ 一致行」と一致し、選択率が低いと件数が不足する。
/// #2033 等でこの挙動を変える場合は本アサーションと設計文書の Issue #2032 節の表を同時に更新する。
#[test]
fn distance_first_postfilter_falls_short_by_selectivity() {
    let fx = Fixture::new("sql73-dist-bf", false);
    let mut observed_short = false;
    for p in preds() {
        for k in [10usize, 100] {
            let m = measure(&fx, &p, k, true, None);
            assert!(
                m.exact_expected,
                "bf distance-first {} k={k}: set mismatch",
                p.name
            );
            assert!(!m.ann_plan.starts_with("hnsw"), "{}", m.ann_plan);
            observed_short |= !m.never_short;
            if p.sql.is_none() {
                assert!(m.never_short, "no-filter query must return k rows");
            }
        }
    }
    assert!(
        observed_short,
        "distance-first must fall short at some selectivity (non-vacuous)"
    );
}

/// SQL-73: HNSW でも不変条件（越境なし・述語充足・件数上限）を保ち、`EXPLAIN` の `ann_plan`
/// が評価順どおりになる。DISTANCE 先行の件数不足は反復スキャン（#2033）で解消される。値（recall）は固定しない。
#[test]
fn hnsw_routes_match_explain_and_distance_first_only_falls_short() {
    let fx = Fixture::new("sql73-hnsw-routes", true);
    let mut default_short = false;
    let mut distance_short = false;
    for p in preds() {
        for k in [10usize, 100] {
            default_short |= !measure(&fx, &p, k, false, None).never_short;
            let f = measure(&fx, &p, k, true, None);
            distance_short |= !f.never_short;
            assert!(
                f.ann_plan.starts_with("hnsw_full_visible"),
                "{}: {}",
                p.name,
                f.ann_plan
            );
        }
    }
    assert!(!default_short, "default order must never fall short");
    // #2033 以降は既定の反復スキャン（strict_order）が DISTANCE 先行の件数不足を解消する。
    assert!(
        !distance_short,
        "hnsw distance-first must be filled by iterative scan (#2033)"
    );
}

/// 設計文書の実測表を標準出力へ出す（`--nocapture`）。`hnsw.ef_search` の補助軸を含む。
#[test]
fn where_knn_shortfall_table() {
    println!("| engine | order | predicate | k | ef_search | returned avg / min | fill | recall@k | route | ann_plan |");
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    for hnsw in [false, true] {
        let fx = Fixture::new(
            if hnsw {
                "sql73-tbl-hnsw"
            } else {
                "sql73-tbl-bf"
            },
            hnsw,
        );
        let engine = if hnsw { "hnsw" } else { "brute-force" };
        let efs: Vec<Option<u32>> = if hnsw {
            vec![None, Some(10), Some(200)]
        } else {
            vec![None]
        };
        for ef in efs {
            for p in preds() {
                for k in [10usize, 100] {
                    for (order, df) in [("default", false), ("distance-first", true)] {
                        if ef.is_some() && (k != 10 || df) {
                            continue;
                        }
                        let m = measure(&fx, &p, k, df, ef);
                        println!("{}", row(engine, order, &p, k, ef, &m));
                    }
                }
            }
        }
    }
}
