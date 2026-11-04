//! `hnsw.iterative_scan`・`hnsw.max_scan_tuples`（Issue #2033・親 #2031・SQL-73 のポインタ）が
//! HNSW の kNN 探索経路へ効くことの結合テスト。
//!
//! 取りこぼすのは `HINT ORDER(DISTANCE, SCALAR, RLS)`（DISTANCE 先行・SCALAR 事後フィルタ）だけで、
//! 既定の評価順は元から `min(k, 一致行数)` 件を返す。設定が効く範囲は「HNSW を使える
//! `ORDER BY .. <=> ..` の DISTANCE 先行」に限り、厳密走査（brute-force）・hybrid は設定値によらず
//! 結果が変わらない（SQL-73 (4)）。tenant-b を同居させ、越境が無いことと tenant-b の行数が
//! 結果を左右しないことも固定する（RLS・P0）。

use std::collections::HashSet;

use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::row_codec::{encode_scalar_columns, Value};
use engine::search_engine::{self, HnswScope};
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{RowInput, Storage, Visibility};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::recovery::required_op_id::OperationId;

use crate::temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 16;
const ROWS_A: usize = 4_000;
const TENANT_B_BASE: u64 = 1_000_000;
const QUERIES: usize = 8;
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
            encode_scalar_columns(&schema, &[Value::Null, Value::Integer(bucket_of(i))])
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

struct Fixture {
    core: EngineCore,
    queries: Vec<Vec<f32>>,
    _guard: CleanupGuard,
}

impl Fixture {
    /// `hnsw` が真なら HNSW opt-in＋索引宣言＋ベース構築済み、偽なら厳密走査エンジン。
    /// `rows_b` は tenant-b の行数（0 なら tenant-b は存在しない）。
    fn new(label: &str, hnsw: bool, rows_b: usize) -> Self {
        let path = unique_db_path(label);
        let guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create docs");
        let mut rng = Rng(0x2033_0001);
        let centers: Vec<Vec<f32>> = (0..CLUSTERS)
            .map(|_| {
                let mut c: Vec<f32> = (0..DIM).map(|_| rng.next()).collect();
                normalize(&mut c);
                c
            })
            .collect();
        seed(
            &storage,
            "tenant-a",
            1,
            &gen_rows(0x2033_0002, ROWS_A, &centers),
        );
        if rows_b > 0 {
            seed(
                &storage,
                "tenant-b",
                TENANT_B_BASE,
                &gen_rows(0x2033_0003, rows_b, &centers),
            );
        }
        let queries = gen_rows(0x2033_0004, QUERIES, &centers);
        let core = if hnsw {
            let kind = search_engine::hnsw_kind(engine::hnsw::HnswParams::default())
                .expect("valid hnsw params");
            EngineCore::from_storage_with_engine(storage, kind).with_hnsw_scope(HnswScope::Declared)
        } else {
            EngineCore::from_storage(storage, search_engine::default_engine())
        };
        let fx = Self {
            core,
            queries,
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
            let before = fx.core.hnsw_index_cache_stats().builds;
            fx.run(
                &mut SessionState::default(),
                &fx.sql(None, "LIMIT 10", false, 0),
            );
            assert!(
                fx.core.hnsw_index_cache_stats().builds > before,
                "warm-up must build the base"
            );
        }
        fx
    }

    fn sql(&self, pred: Option<&str>, tail: &str, distance_first: bool, q: usize) -> String {
        let w = pred.map(|p| format!(" WHERE {p}")).unwrap_or_default();
        let h = if distance_first {
            " HINT ORDER(DISTANCE, SCALAR, RLS)"
        } else {
            ""
        };
        format!(
            "SELECT id FROM docs{w} ORDER BY embedding <=> '{}' {tail}{h}",
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

    fn set(&self, s: &mut SessionState, sql: &str) {
        self.core
            .execute_sql_in_session(&ctx("tenant-a"), s, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }

    /// 全クエリの結果（距離先行・LIMIT k）を返す。越境・述語違反・重複は常に検査する。
    fn run_all(
        &self,
        s: &mut SessionState,
        pred: Option<(&str, i32)>,
        k: usize,
        distance_first: bool,
    ) -> Vec<Vec<u64>> {
        (0..QUERIES)
            .map(|q| {
                let got = self.run(
                    s,
                    &self.sql(pred.map(|p| p.0), &format!("LIMIT {k}"), distance_first, q),
                );
                let set: HashSet<u64> = got.iter().copied().collect();
                assert_eq!(set.len(), got.len(), "duplicate ids");
                for id in &got {
                    assert!(
                        (1..=ROWS_A as u64).contains(id),
                        "id {id} outside tenant-a: tenant boundary crossed"
                    );
                    if let Some((_, x)) = pred {
                        assert!(bucket_of(*id as usize - 1) < x, "predicate violated");
                    }
                }
                got
            })
            .collect()
    }
}

fn matches_below(x: i32) -> usize {
    (0..ROWS_A).filter(|i| bucket_of(*i) < x).count()
}

/// 選択率 1%・5%・10%・50%（`bucket < X`）。
const SELECTIVITIES: [(&str, i32); 4] = [
    ("bucket < 10", 10),
    ("bucket < 50", 50),
    ("bucket < 100", 100),
    ("bucket < 500", 500),
];

/// SQL-73: 既定（`strict_order`）と `relaxed_order` は、DISTANCE 先行でも `min(k, 一致行数)` 件に届く。
/// 反復経路を通ったこと（`iterative_scan_rounds`）と、`off` では従来どおり件数が不足することも固定する。
#[test]
fn iterative_scan_fills_distance_first_postfilter() {
    let fx = Fixture::new("sql2033-fill", true, 1_000);
    for (sql, x) in SELECTIVITIES {
        for k in [10usize, 100] {
            let want = k.min(matches_below(x));
            for mode in ["strict_order", "relaxed_order"] {
                let mut s = SessionState::default();
                fx.set(&mut s, &format!("SET hnsw.iterative_scan = {mode}"));
                let before = fx.core.hnsw_index_cache_stats();
                for got in fx.run_all(&mut s, Some((sql, x)), k, true) {
                    assert_eq!(got.len(), want, "{mode} {sql} k={k}");
                }
                let after = fx.core.hnsw_index_cache_stats();
                assert!(
                    after.iterative_scan_rounds > before.iterative_scan_rounds,
                    "{mode} {sql} k={k}: iterative path must be exercised"
                );
            }
        }
    }
    // 既定のセッション（未設定）も同じ。
    let mut s = SessionState::default();
    for got in fx.run_all(&mut s, Some(("bucket < 10", 10)), 10, true) {
        assert_eq!(got.len(), 10);
    }
    // `off`: 反復しない。1% の選択率では k=10 に届かない問い合わせが出る。
    let mut off = SessionState::default();
    fx.set(&mut off, "SET hnsw.iterative_scan = off");
    let before = fx.core.hnsw_index_cache_stats();
    let results = fx.run_all(&mut off, Some(("bucket < 10", 10)), 10, true);
    assert!(
        results.iter().any(|r| r.len() < 10),
        "off must leave the post-filter shortfall"
    );
    assert_eq!(
        fx.core.hnsw_index_cache_stats().iterative_scan_rounds,
        before.iterative_scan_rounds,
        "off must not iterate"
    );
}

/// SQL-73: 反復の結果は、可視全集合の厳密な事後フィルタ top-k と高い再現率で一致する。
#[test]
fn iterative_scan_matches_exact_postfiltered_topk() {
    let hnsw = Fixture::new("sql2033-exact-h", true, 1_000);
    let bf = Fixture::new("sql2033-exact-b", false, 1_000);
    let mut sh = SessionState::default();
    let mut sb = SessionState::default();
    let (mut hit, mut total) = (0usize, 0usize);
    for (sql, x) in SELECTIVITIES {
        let a = hnsw.run_all(&mut sh, Some((sql, x)), 10, true);
        // 厳密走査の「一致行の真の top-k」は DISTANCE 先行では得られないため、既定の評価順で求める。
        let truth = bf.run_all(&mut sb, Some((sql, x)), 10, false);
        for (g, t) in a.iter().zip(&truth) {
            let ts: HashSet<u64> = t.iter().copied().collect();
            hit += g.iter().filter(|id| ts.contains(id)).count();
            total += t.len();
        }
    }
    assert!(
        hit as f64 / total as f64 >= 0.9,
        "recall {hit}/{total} below 0.9"
    );
}

/// SQL-73: `max_scan_tuples` が小さいと部分的な結果を返し（エラーにしない）、上限到達が数えられる。
#[test]
fn max_scan_tuples_caps_iteration_without_error() {
    let fx = Fixture::new("sql2033-cap", true, 1_000);
    let mut s = SessionState::default();
    fx.set(&mut s, "SET hnsw.max_scan_tuples = 20");
    let before = fx.core.hnsw_index_cache_stats();
    let results = fx.run_all(&mut s, Some(("bucket < 3", 3)), 10, true);
    let after = fx.core.hnsw_index_cache_stats();
    assert!(results.iter().all(|r| r.len() < 10));
    assert!(
        after.iterative_scan_cap_hits > before.iterative_scan_cap_hits,
        "cap hit must be recorded"
    );
    // 上限を既定へ戻せば目標件数（一致 12 行中の 10）に届く。
    fx.set(&mut s, "RESET hnsw.max_scan_tuples");
    for got in fx.run_all(&mut s, Some(("bucket < 3", 3)), 10, true) {
        assert_eq!(got.len(), 10);
    }
    // 最大値でも SQL-46 の走査上限と `MAX_SEARCH_K` で頭打ちになり、完走する。
    fx.set(&mut s, "SET hnsw.max_scan_tuples = 2147483647");
    for got in fx.run_all(&mut s, Some(("bucket < 3", 3)), 10, true) {
        assert_eq!(got.len(), 10);
    }
}

/// SQL-73: `off` は k が `ef_search` を超える分を打ち切る。反復する設定では打ち切らない。
#[test]
fn off_truncates_to_ef_search() {
    let fx = Fixture::new("sql2033-off", true, 0);
    let mut s = SessionState::default();
    fx.set(&mut s, "SET hnsw.ef_search = 5");
    fx.set(&mut s, "SET hnsw.iterative_scan = off");
    for got in fx.run_all(&mut s, None, 20, false) {
        assert_eq!(got.len(), 5, "off must cap at ef_search");
    }
    fx.set(&mut s, "SET hnsw.iterative_scan = strict_order");
    for got in fx.run_all(&mut s, None, 20, false) {
        assert_eq!(got.len(), 20);
    }
}

/// SQL-73: 既定の評価順・厳密走査エンジンの結果は 3 つの設定値で変わらない。
#[test]
fn settings_do_not_change_non_target_paths() {
    let bf = Fixture::new("sql2033-nt-b", false, 1_000);
    let hnsw = Fixture::new("sql2033-nt-h", true, 1_000);
    let modes = ["off", "relaxed_order", "strict_order"];
    for fx in [&bf, &hnsw] {
        // 厳密走査の DISTANCE 先行（bf のみ対象外）と、既定の評価順（両エンジン）。
        for (distance_first, only_bf) in [(true, true), (false, false)] {
            if only_bf && std::ptr::eq(fx, &hnsw) {
                continue;
            }
            let mut base = SessionState::default();
            let reference = fx.run_all(&mut base, Some(("bucket < 10", 10)), 10, distance_first);
            for mode in modes {
                let mut s = SessionState::default();
                fx.set(&mut s, &format!("SET hnsw.iterative_scan = {mode}"));
                fx.set(&mut s, "SET hnsw.max_scan_tuples = 30");
                let got = fx.run_all(&mut s, Some(("bucket < 10", 10)), 10, distance_first);
                assert_eq!(
                    got, reference,
                    "mode {mode} distance_first={distance_first}"
                );
            }
        }
    }
    // 厳密走査の DISTANCE 先行は設定によらず件数が不足する（SQL-73 (4)）。
    let mut s = SessionState::default();
    let r = bf.run_all(&mut s, Some(("bucket < 10", 10)), 10, true);
    assert!(r.iter().any(|g| g.len() < 10));
    // DESC・LIMIT 省略は厳密走査経路で、設定によらず同じ。
    let mut a = SessionState::default();
    let mut b = SessionState::default();
    bf.set(&mut b, "SET hnsw.iterative_scan = off");
    for q in 0..2 {
        let sql = bf.sql(Some("bucket < 10"), "DESC LIMIT 10", false, q);
        assert_eq!(bf.run(&mut a, &sql), bf.run(&mut b, &sql));
    }
}

/// SQL-73: `OFFSET m` を併用しても目標件数は `m + k` で、先頭 `m` 件を捨てた `k` 件を返す。
#[test]
fn iterative_scan_honors_offset() {
    let fx = Fixture::new("sql2033-offset", true, 0);
    let mut s = SessionState::default();
    let all = fx.run(&mut s, &fx.sql(Some("bucket < 10"), "LIMIT 10", true, 0));
    let paged = fx.run(
        &mut s,
        &fx.sql(Some("bucket < 10"), "LIMIT 5 OFFSET 5", true, 0),
    );
    assert_eq!(all.len(), 10);
    assert_eq!(paged.len(), 5);
    assert_eq!(paged, all[5..].to_vec());
}

/// SQL-73・RLS: tenant-b の行数（0 行／1,000 行）は tenant-a の結果を左右しない。
#[test]
fn tenant_b_rows_do_not_affect_iterative_results() {
    let without = Fixture::new("sql2033-rls-0", true, 0);
    let with = Fixture::new("sql2033-rls-1k", true, 1_000);
    for mode in ["strict_order", "off"] {
        let mut a = SessionState::default();
        let mut b = SessionState::default();
        without.set(&mut a, &format!("SET hnsw.iterative_scan = {mode}"));
        with.set(&mut b, &format!("SET hnsw.iterative_scan = {mode}"));
        let ra = without.run_all(&mut a, Some(("bucket < 50", 50)), 10, true);
        let rb = with.run_all(&mut b, Some(("bucket < 50", 50)), 10, true);
        assert_eq!(ra, rb, "mode {mode}");
    }
}
