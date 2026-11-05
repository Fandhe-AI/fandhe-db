//! HNSW の実行時の判断（キャッシュ状態・厳密走査への縮退・反復スキャンのラウンド数と上限到達・
//! 調べた行数）が `EXPLAIN ANALYZE` に出ることの結合テスト（Issue #2056・SQL-76 (2)(3)・
//! SQL-73 (3)(5) のポインタ）。
//!
//! 値は実行した 1 文の分だけを集めた観測で、全テナント共通の集計（`hnsw_index_cache_stats`）の
//! 差分ではない。行数系の値は可視行と自 ctx の索引だけから決まり、tenant-b の行量でバイト単位に
//! 変わらないことを固定する（P0）。

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

const ROWS_BIG: usize = 4_000;
const ROWS_SMALL: usize = 200;

const DIM: usize = 16;
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
    /// HNSW opt-in＋索引宣言済みのエンジン。`rows_a`／`rows_b` は各テナントの行数
    /// （`rows_b == 0` なら tenant-b は存在しない）。
    fn new(label: &str, rows_a: usize, rows_b: usize) -> Self {
        let path = unique_db_path(label);
        let guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage.create_table(&schema()).expect("create docs");
        let mut rng = Rng(0x2056_0001);
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
            &gen_rows(0x2056_0002, rows_a, &centers),
        );
        if rows_b > 0 {
            seed(
                &storage,
                "tenant-b",
                TENANT_B_BASE,
                &gen_rows(0x2056_0003, rows_b, &centers),
            );
        }
        let queries = gen_rows(0x2056_0004, QUERIES, &centers);
        let kind = search_engine::hnsw_kind(engine::hnsw::HnswParams::default())
            .expect("valid hnsw params");
        let core = EngineCore::from_storage_with_engine(storage, kind)
            .with_hnsw_scope(HnswScope::Declared);
        let fx = Self {
            core,
            queries,
            _guard: guard,
        };
        let mut s = SessionState::default();
        s.allow_ddl();
        fx.core
            .execute_sql_in_session(
                &ctx("tenant-a"),
                &mut s,
                "CREATE INDEX docs_embedding_hnsw ON docs USING hnsw (embedding)",
            )
            .expect("declare hnsw");
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

    fn set(&self, s: &mut SessionState, sql: &str) {
        self.core
            .execute_sql_in_session(&ctx("tenant-a"), s, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"));
    }

    fn lines_of(&self, s: &mut SessionState, sql: &str) -> Vec<String> {
        match self
            .core
            .execute_sql_in_session(&ctx("tenant-a"), s, sql)
            .unwrap_or_else(|e| panic!("{sql}: {e:?}"))
        {
            SqlOutcome::Explain(r) => r
                .rows
                .iter()
                .map(|row| match row.cells.first() {
                    Some(Cell::Text(t)) => t.clone(),
                    other => panic!("expected text, got {other:?}"),
                })
                .collect(),
            other => panic!("expected Explain, got {other:?}"),
        }
    }

    /// `EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF) <select>` の出力行（時間を含まず決定的）。
    fn analyze(&self, s: &mut SessionState, select: &str) -> Vec<String> {
        self.lines_of(
            s,
            &format!("EXPLAIN (ANALYZE, TIMING OFF, SUMMARY OFF) {select}"),
        )
    }

    fn rows_of(&self, s: &mut SessionState, sql: &str) -> Vec<u64> {
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
}

fn hnsw_lines(lines: &[String]) -> Vec<&String> {
    const KEYS: [&str; 4] = [
        "hnsw_index_cache:",
        "hnsw_exact_scan:",
        "hnsw_iterative_scan:",
        "hnsw_refetch_rounds:",
    ];
    lines
        .iter()
        .filter(|l| KEYS.iter().any(|k| l.starts_with(k)))
        .collect()
}

fn find<'a>(lines: &'a [String], prefix: &str) -> &'a str {
    lines
        .iter()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("{prefix} missing in {lines:#?}"))
}

/// `key=<n>` を取り出す。
fn num(line: &str, key: &str) -> u64 {
    let pat = format!("{key}=");
    let at = line.find(&pat).unwrap_or_else(|| panic!("{key} in {line}")) + pat.len();
    line[at..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .expect("number")
}

/// SQL-76 (2)(3): 可視行が索引の最小規模に満たないと厳密走査へ縮退し、ANALYZE に出る。
/// 静的な EXPLAIN は HNSW の行を出さない（実行時の判断は ANALYZE だけ）。
#[test]
fn degrade_to_exact_scan_is_reported() {
    let fx = Fixture::new("sql2056-degrade", ROWS_SMALL, 0);
    let mut s = SessionState::default();
    let sel = fx.sql(None, "LIMIT 10", false, 0);
    let stat = fx.lines_of(&mut s, &format!("EXPLAIN {sel}"));
    assert!(hnsw_lines(&stat).is_empty(), "static EXPLAIN: {stat:#?}");
    let out = fx.analyze(&mut s, &sel);
    assert_eq!(
        find(&out, "hnsw_index_cache:"),
        "hnsw_index_cache: cached=0,built=0,rebuilt=0,not_indexed=1"
    );
    assert!(find(&out, "hnsw_exact_scan:").starts_with("hnsw_exact_scan: true,switches="));
    assert!(!out.iter().any(|l| l.starts_with("hnsw_iterative_scan")));
    assert_eq!(fx.rows_of(&mut s, &sel).len(), 10);
}

/// SQL-76 (2): 新しい DB の最初の ANALYZE は構築、同じ文の 2 回目は再利用。
#[test]
fn cache_state_built_then_cached() {
    let fx = Fixture::new("sql2056-cache", ROWS_BIG, 0);
    let mut s = SessionState::default();
    let sel = fx.sql(None, "LIMIT 10", false, 0);
    let first = fx.analyze(&mut s, &sel);
    assert_eq!(
        find(&first, "hnsw_index_cache:"),
        "hnsw_index_cache: cached=0,built=1,rebuilt=0,not_indexed=0",
        "{first:#?}"
    );
    assert_eq!(
        find(&first, "hnsw_exact_scan:"),
        "hnsw_exact_scan: false,switches=0"
    );
    let second = fx.analyze(&mut s, &sel);
    assert_eq!(
        find(&second, "hnsw_index_cache:"),
        "hnsw_index_cache: cached=1,built=0,rebuilt=0,not_indexed=0"
    );
}

/// SQL-73 (3): 選択的な述語＋距離先行＋小さい走査上限では上限到達が ANALYZE で確認できる。
/// 上限を十分大きくすれば到達しない。
#[test]
fn iterative_scan_cap_hit_is_reported() {
    let fx = Fixture::new("sql2056-iter", ROWS_BIG, 0);
    let mut s = SessionState::default();
    let sel = fx.sql(Some("bucket < 10"), "LIMIT 10", true, 0);
    fx.set(&mut s, "SET hnsw.max_scan_tuples = 40");
    let capped = fx.analyze(&mut s, &sel);
    let it = find(&capped, "hnsw_iterative_scan:");
    assert!(it.contains("cap_hit=true"), "{capped:#?}");
    assert!(num(it, "rounds") >= 2, "{it}");
    assert!(num(it, "rows_examined") > 0, "{it}");
    fx.set(&mut s, "SET hnsw.max_scan_tuples = 20000");
    let free = fx.analyze(&mut s, &sel);
    let it = find(&free, "hnsw_iterative_scan:");
    assert!(it.contains("cap_hit=false"), "{free:#?}");
}

/// P0・SQL-76 (3)・SQL-73 (5): tenant-b の行量が 0 でも 4000 でも tenant-a の出力はバイト単位で
/// 一致する（縮退・構築・再利用・反復のいずれの形でも）。
#[test]
fn output_is_independent_of_other_tenant_volume() {
    for (rows_a, pred, cap) in [
        (ROWS_SMALL, None, None),
        (ROWS_BIG, None, None),
        (ROWS_BIG, Some("bucket < 10"), Some(40)),
    ] {
        let mut outs = Vec::new();
        for rows_b in [0usize, 4_000] {
            let fx = Fixture::new("sql2056-iso", rows_a, rows_b);
            let mut s = SessionState::default();
            if let Some(c) = cap {
                fx.set(&mut s, &format!("SET hnsw.max_scan_tuples = {c}"));
            }
            let sel = fx.sql(pred, "LIMIT 10", pred.is_some(), 0);
            let first = fx.analyze(&mut s, &sel);
            let second = fx.analyze(&mut s, &sel);
            assert!(!hnsw_lines(&first).is_empty(), "vacuous: {first:#?}");
            outs.push((first, second));
        }
        assert_eq!(outs[0], outs[1], "rows_a={rows_a} pred={pred:?}");
    }
}

/// ANALYZE の後に同じ文を通常実行しても結果は変わらない。HNSW を使わない文は HNSW の行を出さない。
#[test]
fn analyze_does_not_change_results() {
    let fx = Fixture::new("sql2056-same", ROWS_BIG, 0);
    let mut s = SessionState::default();
    let sel = fx.sql(None, "LIMIT 10", false, 1);
    let before = fx.rows_of(&mut s, &sel);
    let _ = fx.analyze(&mut s, &sel);
    assert_eq!(fx.rows_of(&mut s, &sel), before);
    let plain = fx.analyze(&mut s, "SELECT id FROM docs ORDER BY id LIMIT 3");
    assert!(hnsw_lines(&plain).is_empty(), "{plain:#?}");
}
