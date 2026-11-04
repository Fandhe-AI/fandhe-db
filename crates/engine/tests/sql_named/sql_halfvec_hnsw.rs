//! `HALFVEC(N)` 列への `USING hnsw` 宣言と、宣言済み表への距離演算子形 kNN が HNSW 索引を実際に
//! 使うことの結合テスト（Issue #2016。ポインタ: TABLE-46 (3)(4)(6)・INDEX-7、SQL-33 (5)・SQL-75）。
//!
//! 固定する内容:
//!
//! - (a) DDL: 4 種の `halfvec_*_ops` の受理と再オープン後の保持、省略は `halfvec_cosine_ops` の別名、
//!   系列の食い違いは `42804`、未知名は `42704`、HALFVEC(4000) は受理・HALFVEC(4001) は `54000`、
//!   SPARSEVEC・INT8VEC 列は従来どおり拒否
//! - (b) 宣言後の `<=>` kNN が HNSW キャッシュの索引探索（`hits`）に到達し、常駐は F16
//!   （エンジン設定は既定の F32 のまま。縮退 0 件）。宣言が無ければ到達しない
//! - (c) 同じ表の厳密走査を参照にした Recall@10 が基準以上
//! - (d) 演算子と opclass が合わない組は厳密走査へ縮退し、結果は厳密走査と一致する
//! - (e) テナント境界、(f) 未対応形の fail-closed と DML の追随、Describe と Execute の列メタ一致

use engine::catalog::{HnswOpClassFamily, IndexKind, VectorOpClass};
use engine::core::EngineCore;
use engine::policy::PolicyContext;
use engine::search_engine::{self, HnswScope};
use engine::sql::allowlist::SqlSurfaceError;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const DIM: usize = 16;
// `sql::hnsw_cache::MIN_INDEXED_ROWS`（1,024）超。
const ROWS: usize = 1_100;
const EF_SEARCH: usize = 400;
const TENANT_B_ID_BASE: i64 = 1_000_000;

fn ctx_for(tenant: &str) -> PolicyContext {
    PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant")
}

fn exec_as(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, SqlSurfaceError> {
    let mut s = SessionState::default();
    s.allow_ddl();
    core.execute_sql_in_session(&ctx_for(tenant), &mut s, sql)
}

fn ok(core: &EngineCore, tenant: &str, sql: &str) {
    exec_as(core, tenant, sql).unwrap_or_else(|e| {
        panic!(
            "{} must succeed, got {e:?}",
            sql.chars().take(80).collect::<String>()
        )
    });
}

/// SQL を実行し、成功は空文字・失敗は SQLSTATE を返す。
fn code(core: &EngineCore, sql: &str) -> String {
    exec_as(core, "tenant-a", sql)
        .map(|_| String::new())
        .unwrap_or_else(|e| e.wire_code().to_string())
}

fn cell_id(c: &Cell) -> i64 {
    match c {
        Cell::SignedInteger(n) => *n,
        Cell::Integer(n) => i64::try_from(*n).expect("id"),
        other => panic!("unexpected id cell {other:?}"),
    }
}

fn ids(core: &EngineCore, tenant: &str, sql: &str) -> Vec<i64> {
    let SqlOutcome::Query(q) =
        exec_as(core, tenant, sql).unwrap_or_else(|e| panic!("{sql} must succeed, got {e:?}"))
    else {
        panic!("{sql} must return a query result");
    };
    q.rows.iter().map(|r| cell_id(&r.cells[0])).collect()
}

fn explain(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let mut s = SessionState::default();
    match core
        .execute_sql_in_session(&ctx_for(tenant), &mut s, &format!("EXPLAIN {sql}"))
        .expect("explain")
    {
        SqlOutcome::Explain(result) => result
            .rows
            .iter()
            .filter_map(|row| match row.cells.first() {
                Some(Cell::Text(s)) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        other => panic!("expected explain, got {other:?}"),
    }
}

fn line<'a>(lines: &'a [String], prefix: &str) -> &'a str {
    lines
        .iter()
        .find(|l| l.starts_with(prefix))
        .unwrap_or_else(|| panic!("{prefix} line missing: {lines:?}"))
}

/// クラスタ構造を持つ L2 正規化済みの決定的コーパス（`tests/core_misc/hnsw_cache.rs` の
/// `gen_clustered_corpus` と同型。一様乱数は HNSW の Recall が低く出るため使わない）。
fn gen_vectors(seed: u64, rows: usize) -> Vec<Vec<f32>> {
    fn xorshift(state: &mut u64) -> f32 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        let bits = (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as u32;
        (bits as f32) / (1u32 << 24) as f32 * 2.0 - 1.0
    }
    const CLUSTERS: usize = 12;
    let mut center_state = seed ^ 0xC1C1_C1C1_C1C1_C1C1;
    let centers: Vec<Vec<f32>> = (0..CLUSTERS)
        .map(|_| (0..DIM).map(|_| xorshift(&mut center_state)).collect())
        .collect();
    let mut state = seed;
    (0..rows)
        .map(|i| {
            let center = &centers[i % CLUSTERS];
            let mut v: Vec<f32> = center
                .iter()
                .map(|c| c + xorshift(&mut state) * 0.2)
                .collect();
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                v.iter_mut().for_each(|x| *x /= norm);
            }
            v
        })
        .collect()
}

fn vec_literal(v: &[f32]) -> String {
    let parts: Vec<String> = v.iter().map(|x| format!("{x:.4}")).collect();
    format!("[{}]", parts.join(","))
}

fn knn(op: &str, q: &[f32], limit: usize) -> String {
    format!(
        "SELECT id FROM hv ORDER BY h {op} '{}' LIMIT {limit}",
        vec_literal(q)
    )
}

fn insert_rows(core: &EngineCore, tenant: &str, id_base: i64, vectors: &[Vec<f32>]) {
    for (chunk_index, chunk) in vectors.chunks(50).enumerate() {
        let values: Vec<String> = chunk
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let id = id_base + (chunk_index * 50 + i) as i64;
                format!("({id}, '{}')", vec_literal(v))
            })
            .collect();
        ok(
            core,
            tenant,
            &format!("INSERT INTO hv (id, h) VALUES {}", values.join(",")),
        );
    }
}

/// 2 テナント × `ROWS` 行の HALFVEC 表を持つ HNSW opt-in エンジン。返すのは tenant-b のベクトル。
fn seeded_core(label: &str, scope: HnswScope) -> (EngineCore, CleanupGuard, std::path::PathBuf) {
    seeded_core_with(label, scope, engine::hnsw::HnswParams::default())
}

fn seeded_core_with(
    label: &str,
    scope: HnswScope,
    params: engine::hnsw::HnswParams,
) -> (EngineCore, CleanupGuard, std::path::PathBuf) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let kind = search_engine::hnsw_kind(params).expect("valid hnsw params");
    let core = EngineCore::from_storage_with_engine(storage, kind).with_hnsw_scope(scope);
    ok(
        &core,
        "tenant-a",
        &format!("CREATE TABLE hv (id BIGINT PRIMARY KEY, h HALFVEC({DIM}))"),
    );
    insert_rows(&core, "tenant-a", 0, &gen_vectors(3, ROWS));
    insert_rows(&core, "tenant-b", TENANT_B_ID_BASE, &gen_vectors(5, ROWS));
    (core, guard, path)
}

fn empty_core(label: &str) -> (EngineCore, CleanupGuard, std::path::PathBuf) {
    let path = unique_db_path(label);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    let kind =
        search_engine::hnsw_kind(engine::hnsw::HnswParams::default()).expect("valid hnsw params");
    let core =
        EngineCore::from_storage_with_engine(storage, kind).with_hnsw_scope(HnswScope::Declared);
    (core, guard, path)
}

/// (a) 4 種の `halfvec_*_ops` と別名を宣言でき、再オープン後も保持される。
#[test]
fn four_halfvec_opclasses_are_declared_and_persisted() {
    let (core, _g, path) = empty_core("hvh-ddl-persist");
    ok(
        &core,
        "tenant-a",
        "CREATE TABLE hv (id BIGINT PRIMARY KEY, h HALFVEC(4))",
    );
    for (name, body) in [
        ("i_alias", "(h)"),
        ("i_cos", "(h halfvec_cosine_ops)"),
        ("i_l2", "(h halfvec_l2_ops)"),
        ("i_ip", "(h HALFVEC_IP_OPS)"),
        ("i_l1", "(h halfvec_l1_ops)"),
        (
            "i_l2_with",
            "(h halfvec_l2_ops) WITH (m = 8, ef_construction = 40)",
        ),
    ] {
        assert_eq!(
            code(
                &core,
                &format!("CREATE INDEX {name} ON hv USING hnsw {body}")
            ),
            "",
            "{name}"
        );
    }
    drop(core);
    let storage = Storage::open(&path).expect("reopen");
    let defs = storage.list_indexes().expect("list");
    let find = |name: &str| defs.iter().find(|d| d.name == name).expect("index present");
    for (name, expected) in [
        ("i_alias", VectorOpClass::Cosine),
        ("i_cos", VectorOpClass::Cosine),
        ("i_l2", VectorOpClass::L2),
        ("i_ip", VectorOpClass::Ip),
        ("i_l1", VectorOpClass::L1),
        ("i_l2_with", VectorOpClass::L2),
    ] {
        assert_eq!(find(name).kind, IndexKind::Hnsw(expected), "{name}");
    }
    // 表示名は列型から導く（系列は永続化しない）。
    assert_eq!(
        HnswOpClassFamily::HalfVec.opclass_name(VectorOpClass::Ip),
        "halfvec_ip_ops"
    );
}

/// (a) 系列の食い違い・未知名・次元上限・対象外の列型。拒否された宣言は何も残さない。
#[test]
fn opclass_family_dimension_cap_and_other_column_types() {
    let (core, _g, _path) = empty_core("hvh-ddl-errors");
    ok(
        &core,
        "tenant-a",
        "CREATE TABLE hv (id BIGINT PRIMARY KEY, h HALFVEC(4))",
    );
    ok(
        &core,
        "tenant-a",
        "CREATE TABLE vv (id BIGINT PRIMARY KEY, v VECTOR(4))",
    );
    for (sql, expected) in [
        // 系列の食い違いは PostgreSQL と同じ 42804。
        (
            "CREATE INDEX x ON hv USING hnsw (h vector_cosine_ops)",
            "42804",
        ),
        ("CREATE INDEX x ON hv USING hnsw (h vector_l2_ops)", "42804"),
        (
            "CREATE INDEX x ON vv USING hnsw (v halfvec_cosine_ops)",
            "42804",
        ),
        ("CREATE INDEX x ON hv USING hnsw (h jsonb_ops)", "42704"),
        // 省略は列型の既定。
        ("CREATE INDEX ok_h ON hv USING hnsw (h)", ""),
        ("CREATE INDEX ok_v ON vv USING hnsw (v)", ""),
    ] {
        assert_eq!(code(&core, sql), expected, "{sql}");
    }
    // 拒否された宣言は何も残さない（`x` は作れる）。
    assert_eq!(
        code(&core, "CREATE INDEX x ON hv USING hnsw (h halfvec_ip_ops)"),
        ""
    );
    // 次元上限: 4,000 は受理・4,001 は 54000。
    ok(
        &core,
        "tenant-a",
        "CREATE TABLE h4000 (id BIGINT PRIMARY KEY, h HALFVEC(4000))",
    );
    ok(
        &core,
        "tenant-a",
        "CREATE TABLE h4001 (id BIGINT PRIMARY KEY, h HALFVEC(4001))",
    );
    assert_eq!(
        code(
            &core,
            "CREATE INDEX a4000 ON h4000 USING hnsw (h halfvec_cosine_ops)"
        ),
        ""
    );
    assert_eq!(
        code(
            &core,
            "CREATE INDEX a4001 ON h4001 USING hnsw (h halfvec_cosine_ops)"
        ),
        "54000"
    );
    // 対象外の列型は従来どおり拒否（0A000）。
    ok(
        &core,
        "tenant-a",
        "CREATE TABLE s (id BIGINT PRIMARY KEY, sv SPARSEVEC(8))",
    );
    ok(
        &core,
        "tenant-a",
        "CREATE TABLE i8 (id BIGINT PRIMARY KEY, iv INT8VEC(8))",
    );
    assert_eq!(
        code(&core, "CREATE INDEX a_s ON s USING hnsw (sv)"),
        "0A000"
    );
    assert_eq!(
        code(&core, "CREATE INDEX a_i8 ON i8 USING hnsw (iv)"),
        "0A000"
    );
}

/// (b)(c) 宣言後の `<=>` kNN が HNSW の索引探索に到達し、常駐は F16。Recall@10 が基準以上。
/// 宣言前（厳密走査）の Top-10 を参照にする。エンジン設定は既定の F32 のままにして、
/// HALFVEC 側の上書きが効いていることも確かめる。
#[test]
fn declared_halfvec_knn_uses_hnsw_with_f16_residency_and_recall() {
    // 探索幅は VECTOR 表の同条件と同じ値を使う（同じ fixture で VECTOR と HALFVEC の Recall が
    // 一致することを実測で確認済み）。
    let params = engine::hnsw::HnswParams {
        ef_search: EF_SEARCH,
        ..engine::hnsw::HnswParams::default()
    };
    let (core, _g, _path) = seeded_core_with("hvh-ann", HnswScope::Declared, params);
    let queries: Vec<Vec<f32>> = gen_vectors(3, ROWS)
        .into_iter()
        .step_by(ROWS / 10)
        .take(10)
        .collect();

    // 宣言なし（Declared）: 索引へは到達せず、結果が参照になる。
    let reference: Vec<Vec<i64>> = queries
        .iter()
        .map(|q| ids(&core, "tenant-a", &knn("<=>", q, 10)))
        .collect();
    assert_eq!(core.hnsw_index_cache_stats().hits, 0, "no declaration");
    let before = explain(&core, "tenant-a", &knn("<=>", &queries[0], 10));
    // 宣言が無い HALFVEC 表は従来の scan 経路（ANN の計画行を持たない）。
    assert!(
        !before.iter().any(|l| l.starts_with("ann_plan:")),
        "{before:?}"
    );

    assert_eq!(
        code(
            &core,
            "CREATE INDEX hv_h ON hv USING hnsw (h halfvec_cosine_ops)"
        ),
        ""
    );
    let plan = explain(&core, "tenant-a", &knn("<=>", &queries[0], 10));
    assert!(line(&plan, "ann_plan:").contains("hnsw"), "{plan:?}");
    assert!(
        line(&plan, "hnsw_params:").contains("resident=f16"),
        "{plan:?}"
    );
    assert!(
        line(&plan, "hnsw_params:").contains("metric=cosine"),
        "{plan:?}"
    );

    let mut recalls = Vec::new();
    for (q, truth) in queries.iter().zip(&reference) {
        let got = ids(&core, "tenant-a", &knn("<=>", q, 10));
        let hit = got.iter().filter(|id| truth.contains(id)).count();
        recalls.push(hit as f64 / truth.len() as f64);
    }
    let stats = core.hnsw_index_cache_stats();
    assert!(stats.hits > 0, "ANN must reach the HNSW index: {stats:?}");
    assert_eq!(stats.f16_residency_fallbacks, 0, "{stats:?}");
    let min = recalls.iter().cloned().fold(f64::INFINITY, f64::min);
    let mean = recalls.iter().sum::<f64>() / recalls.len() as f64;
    eprintln!(
        "halfvec hnsw recall@10: rows={ROWS} dim={DIM} queries={} min={min:.3} mean={mean:.3}",
        recalls.len()
    );
    assert!(min >= 0.9, "recall@10 min {min} (all: {recalls:?})");
}

/// (d) 演算子と opclass が合わない組は厳密走査へ縮退し、結果は厳密走査と一致する。
#[test]
fn opclass_mismatch_degrades_to_exact_scan() {
    let (core, _g, _path) = seeded_core("hvh-mismatch", HnswScope::Declared);
    let q = &gen_vectors(91, 1)[0];
    let exact_cos = ids(&core, "tenant-a", &knn("<=>", q, 10));
    let exact_ip = ids(&core, "tenant-a", &knn("<#>", q, 10));
    assert_eq!(
        code(
            &core,
            "CREATE INDEX hv_l2 ON hv USING hnsw (h halfvec_l2_ops)"
        ),
        ""
    );
    assert_eq!(ids(&core, "tenant-a", &knn("<=>", q, 10)), exact_cos);
    assert_eq!(ids(&core, "tenant-a", &knn("<#>", q, 10)), exact_ip);
    let plan = explain(&core, "tenant-a", &knn("<=>", q, 10));
    assert!(!plan.iter().any(|l| l.starts_with("ann_plan:")), "{plan:?}");
    assert_eq!(core.hnsw_index_cache_stats().hits, 0);
}

/// 暗黙の HNSW（`All`）は HALFVEC 表に作らない: 宣言が無ければ `All` でも索引へ到達しない。
#[test]
fn all_scope_does_not_create_implicit_hnsw_for_halfvec() {
    let (core, _g, _path) = seeded_core("hvh-all", HnswScope::All);
    let q = &gen_vectors(13, 1)[0];
    let _ = ids(&core, "tenant-a", &knn("<=>", q, 10));
    let stats = core.hnsw_index_cache_stats();
    assert_eq!((stats.hits, stats.builds), (0, 0), "{stats:?}");
}

/// (e) テナント境界、(f) 未対応形の fail-closed、DML の追随、Describe の一致。
#[test]
fn tenant_isolation_fail_closed_forms_and_dml_follow() {
    let (core, _g, _path) = seeded_core("hvh-misc", HnswScope::Declared);
    assert_eq!(
        code(
            &core,
            "CREATE INDEX hv_h ON hv USING hnsw (h halfvec_cosine_ops)"
        ),
        ""
    );
    let q = &gen_vectors(21, 1)[0];
    // テナント境界: 他テナントの id が混ざらない（ANN 経路）。
    for _ in 0..3 {
        let a = ids(&core, "tenant-a", &knn("<=>", q, 50));
        let b = ids(&core, "tenant-b", &knn("<=>", q, 50));
        assert_eq!(a.len(), 50);
        assert!(a.iter().all(|id| *id < TENANT_B_ID_BASE), "{a:?}");
        assert!(b.iter().all(|id| *id >= TENANT_B_ID_BASE), "{b:?}");
    }
    assert!(core.hnsw_index_cache_stats().hits > 0);

    // 未対応の形は従来のコードのまま。
    assert_eq!(code(&core, &knn("<->", q, 5)), "0A000");
    assert_eq!(code(&core, &knn("<+>", q, 5)), "0A000");
    assert_eq!(
        code(
            &core,
            &format!(
                "SELECT id FROM hv ORDER BY h <=> '{}' LIMIT 5 USING MODE 'precision'",
                vec_literal(q)
            )
        ),
        "0A000"
    );

    // DESC は索引を使わない（hits が増えない）。
    let hits = core.hnsw_index_cache_stats().hits;
    let desc = format!(
        "SELECT id FROM hv ORDER BY h <=> '{}' DESC LIMIT 5",
        vec_literal(q)
    );
    assert_eq!(ids(&core, "tenant-a", &desc).len(), 5);
    assert_eq!(core.hnsw_index_cache_stats().hits, hits);

    // INSERT 後に新しく最近傍になった行が結果に出る（索引追随）。
    ok(
        &core,
        "tenant-a",
        &format!(
            "INSERT INTO hv (id, h) VALUES (9999999, '{}')",
            vec_literal(q)
        ),
    );
    let after = ids(&core, "tenant-a", &knn("<=>", q, 3));
    assert_eq!(after.first(), Some(&9_999_999), "{after:?}");

    // Describe と Execute の結果列が同じ（ANN 経路でも scan 経路と同じ列メタ）。
    let sql = knn("<=>", q, 3);
    let session = SessionState::default();
    let prepared = core.parse_sql_prepared(&sql).expect("prepare");
    let described = core
        .describe_prepared_in_session(&session, &prepared)
        .expect("describe")
        .expect("columns");
    let SqlOutcome::Query(result) = exec_as(&core, "tenant-a", &sql).expect("execute") else {
        panic!("query expected");
    };
    assert_eq!(described.len(), result.columns.len());
    for (d, e) in described.iter().zip(result.columns.iter()) {
        assert_eq!(d, e);
    }
}
