//! 実行計測フックの結合テスト（Issue #2053・SQL-76 (2)(3) のポインタ）。
//!
//! kernel 経路（`sql::exec::execute_statement_with_cache_observed`）と scan 経路
//! （`sql::scan::execute_scan_with_budget_observed`）を実ストレージ上で駆動し、
//! (1) 計測の有無で結果が完全に同じ、(2) `Project` の行数が返却行数と一致、
//! (3) 他テナントの行数を変えても全段の行数が変わらない、を固定する。
//! 時間の値はアサートしない（`Some`／`None` だけ確かめる）。

use super::{ExecStage, ExecStats};
use crate::catalog::{ColumnDef, ColumnType, TableSchema};
use crate::policy::PolicyContext;
use crate::sql::allowlist::{validate_sql, Statement};
use crate::sql::exec::QueryResult;
use crate::storage::{Storage, Visibility};
use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
use redb::ReadableDatabase;

const STAGES: [ExecStage; 6] = [
    ExecStage::Scan,
    ExecStage::Distance,
    ExecStage::PostFilter,
    ExecStage::PrecisionGate,
    ExecStage::Limit,
    ExecStage::Project,
];

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("embedding", ColumnType::Vector(2), false),
            ColumnDef::new("tag", ColumnType::Text, false),
        ],
    )
}

/// tenant-a に 6 行（tag は 1..=2 が "y"、3..=6 が "x"。cosine・内積のどちらでも id が小さいほど近い）、tenant-b に `other` 行（Private）を置く。
fn fixture(name: &str, other: u64) -> (Storage, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage.create_table(&schema()).expect("create table");
    let a = PolicyContext::new("tenant-a").expect("tenant");
    let b = PolicyContext::new("tenant-b").expect("tenant");
    let mut n = 0u32;
    let mut put = |ctx: &PolicyContext, id: u64, tag: &str| {
        n += 1;
        let op = crate::recovery::required_op_id::OperationId::parse(&format!("op-{name}-{n}"))
            .expect("op id");
        // 単位ベクトルで角度を id に対し単調に増やし、cosine と内積（HNSW 候補の採点）の
        // どちらでも「id が小さいほど問い合わせ [1.0,0.5] に近い」順位になるようにする。
        let theta = 0.5 + (id % 7) as f32 * 0.1;
        crate::tenant::insert_typed_row(
            &storage,
            "docs",
            ctx,
            id,
            Visibility::Private,
            &[
                crate::row_codec::Value::Vector(vec![theta.cos(), theta.sin()]),
                crate::row_codec::Value::Text(tag.to_string()),
            ],
            &op,
        )
        .expect("insert");
    };
    for id in 1..=6u64 {
        put(&a, id, if id <= 2 { "y" } else { "x" });
    }
    for id in 101..101 + other {
        put(&b, id, "x");
    }
    (storage, guard)
}

type Snapshot = Vec<(ExecStage, Option<(u64, u64, Option<u64>)>)>;

fn snapshot(stats: &ExecStats) -> Snapshot {
    STAGES
        .iter()
        .map(|s| {
            (
                *s,
                stats
                    .stage(*s)
                    .map(|v| (v.rows_out, v.loops, v.rows_removed_by_filter)),
            )
        })
        .collect()
}

/// fixture の行は Private のため、Private 可視の主体を使う（Public のみだと可視行 0 件で
/// アサーションが空虚に成立する）。
fn private_ctx() -> PolicyContext {
    PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
        .expect("tenant")
}

fn run_kernel(storage: &Storage, sql: &str, stats: Option<&mut ExecStats>) -> QueryResult {
    let ctx = private_ctx();
    let Statement::Select(v) = validate_sql(sql, storage).expect("valid") else {
        panic!("expected Select");
    };
    let schema = schema();
    let bound = crate::sql::parser::bind(&v, &schema).expect("bind");
    let read_txn = storage.db().begin_read().expect("read");
    crate::sql::exec::execute_statement_with_cache_observed(
        &read_txn,
        &crate::kernel::CpuScalarProvider,
        &ctx,
        &schema,
        &bound,
        &crate::precision::PrecisionPolicy::default(),
        None,
        None,
        None,
        None,
        &std::sync::Arc::new(crate::sparse::TextAnalysis::default()),
        &crate::text_query_limits::TextQueryLimits::default(),
        None,
        stats,
    )
    .expect("execute")
}

fn run_scan(storage: &Storage, sql: &str, stats: Option<&mut ExecStats>) -> QueryResult {
    let ctx = private_ctx();
    let Statement::Scan(v) = validate_sql(sql, storage).expect("valid") else {
        panic!("expected Scan");
    };
    let schema = schema();
    let udfs = crate::sql::udf_call::UdfRegistry::default();
    let bound = crate::sql::parser::bind_scan(&v, &schema, &udfs).expect("bind");
    let read_txn = storage.db().begin_read().expect("read");
    crate::sql::scan::execute_scan_with_budget_observed(
        &read_txn,
        &ctx,
        &schema,
        &bound,
        crate::arena::MAX_ARENA_TOTAL_BYTES,
        stats,
    )
    .expect("execute")
}

const KERNEL_SQLS: [&str; 5] = [
    "SELECT tag FROM docs ORDER BY embedding <=> '[1.0,0.5]' LIMIT 3",
    "SELECT tag FROM docs WHERE tag = 'x' ORDER BY embedding <=> '[1.0,0.5]' LIMIT 3",
    "SELECT tag FROM docs WHERE tag = 'x' ORDER BY embedding <=> '[1.0,0.5]' LIMIT 3 \
     HINT ORDER(DISTANCE, SCALAR, RLS)",
    "SELECT tag FROM docs ORDER BY embedding <=> '[1.0,0.5]' LIMIT 3 OFFSET 1",
    "SELECT tag FROM docs ORDER BY embedding <=> '[1.0,0.5]' LIMIT 3 USING MODE 'precision'",
];

const SCAN_SQLS: [&str; 3] = [
    "SELECT tag FROM docs LIMIT 4",
    "SELECT tag FROM docs WHERE tag = 'x' ORDER BY id LIMIT 2",
    "SELECT tag FROM docs ORDER BY tag LIMIT 3",
];

/// SQL-76 (2): 計測の有無で結果が同じで、Project の行数が返却行数と一致する。
#[test]
fn observed_matches_unobserved_and_project_rows_equal_result_rows() {
    let (storage, _g) = fixture("exec-stats-parity", 5);
    for sql in KERNEL_SQLS {
        let plain = run_kernel(&storage, sql, None);
        let mut st = ExecStats::new(true);
        let seen = run_kernel(&storage, sql, Some(&mut st));
        assert_eq!(plain.rows, seen.rows, "{sql}");
        // precision は確信度ゲートで 0 件になりうるため、それ以外で可視行が実在することを確かめる。
        if !sql.contains("precision") {
            assert!(!seen.rows.is_empty(), "visible rows must exist: {sql}");
        }
        let p = st.stage(ExecStage::Project).expect("project");
        assert_eq!(p.rows_out as usize, seen.rows.len(), "{sql}");
        assert!(p.elapsed.is_some(), "{sql}");
    }
    for sql in SCAN_SQLS {
        let plain = run_scan(&storage, sql, None);
        let mut st = ExecStats::new(true);
        let seen = run_scan(&storage, sql, Some(&mut st));
        assert_eq!(plain.rows, seen.rows, "{sql}");
        assert!(!seen.rows.is_empty(), "visible rows must exist: {sql}");
        let s = st.stage(ExecStage::Scan).expect("scan");
        // LIMIT の早期終了後も、出力行数は返した件数（走査した件数ではない）。
        assert_eq!(s.rows_out as usize, seen.rows.len(), "{sql}");
    }
}

/// SQL-76 (3): 他テナントの行数（0 件と多数件）で、全段の行数が変わらない。同じ文を 2 回流して
/// キャッシュ経路が絡んでも値が同じ。
#[test]
fn stage_rows_do_not_depend_on_other_tenant_rows() {
    let (few, _g1) = fixture("exec-stats-tenant-few", 0);
    let (many, _g2) = fixture("exec-stats-tenant-many", 40);
    for sql in KERNEL_SQLS {
        let snap = |s: &Storage| {
            let mut st = ExecStats::new(false);
            run_kernel(s, sql, Some(&mut st));
            snapshot(&st)
        };
        assert_eq!(snap(&few), snap(&many), "{sql}");
    }
    for sql in SCAN_SQLS {
        let snap = |s: &Storage| {
            let mut st = ExecStats::new(false);
            run_scan(s, sql, Some(&mut st));
            snapshot(&st)
        };
        assert_eq!(snap(&few), snap(&many), "{sql}");
    }
}

/// timing=false のとき全段の elapsed は None。DISTANCE 先行 + WHERE で PostFilter が記録され、
/// removed は前後の件数の差になる。
#[test]
fn timing_off_has_no_elapsed_and_postfilter_records_removed() {
    let (storage, _g) = fixture("exec-stats-postfilter", 3);
    let mut st = ExecStats::new(false);
    run_kernel(&storage, KERNEL_SQLS[2], Some(&mut st));
    for s in STAGES {
        if let Some(v) = st.stage(s) {
            assert!(v.elapsed.is_none());
        }
    }
    let d = st.stage(ExecStage::Distance).expect("distance");
    let p = st.stage(ExecStage::PostFilter).expect("postfilter");
    assert_eq!(
        d.rows_out,
        p.rows_out + p.rows_removed_by_filter.expect("removed")
    );
    // 通常の DISTANCE 後段なしの文では PostFilter は記録されない。
    let mut st2 = ExecStats::new(false);
    run_kernel(&storage, KERNEL_SQLS[0], Some(&mut st2));
    assert!(st2.stage(ExecStage::PostFilter).is_none());
    assert!(st2.stage(ExecStage::PrecisionGate).is_none());
}

/// Issue #2053（PR #2986 指摘）: HNSW 反復スキャンの DISTANCE 先行経路でも、事後フィルタを
/// PostFilter 段として記録する。Distance の rows_out はフィルタ前の候補件数、PostFilter は
/// 前後の件数（差が removed）で、`Distance.rows_out == PostFilter.rows_out + removed` が成り立つ。
#[test]
fn iterative_scan_records_postfilter_and_prefilter_distance_rows() {
    let (storage, _g) = fixture("exec-stats-iter", 3);
    let cache = crate::sql::hnsw_cache::HnswIndexCache::new();
    let gate_cache = crate::catalog::IndexCatalogGateCache::new();
    let access = crate::sql::hnsw_cache::HnswCacheAccess {
        storage: &storage,
        cache: &cache,
        provider: crate::hnsw::HnswSearchProvider::new(crate::hnsw::ValidatedHnswParams::default()),
        index_gate_cache: &gate_cache,
        hnsw_scope: crate::search_engine::HnswScope::All,
        graph_metric: None,
        build_options: None,
    };
    let sql = KERNEL_SQLS[2];
    // fixture の行は Private のため、Private 可視の主体で流す。
    let ctx = private_ctx();
    let Statement::Select(v) = validate_sql(sql, &storage).expect("valid") else {
        panic!("expected Select");
    };
    let schema = schema();
    let bound = crate::sql::parser::bind(&v, &schema).expect("bind");
    let read_txn = storage.db().begin_read().expect("read");
    let mut st = ExecStats::new(false);
    crate::sql::exec::execute_statement_with_cache_observed(
        &read_txn,
        &crate::kernel::CpuScalarProvider,
        &ctx,
        &schema,
        &bound,
        &crate::precision::PrecisionPolicy::default(),
        None,
        None,
        Some(access),
        None,
        &std::sync::Arc::new(crate::sparse::TextAnalysis::default()),
        &crate::text_query_limits::TextQueryLimits::default(),
        None,
        Some(&mut st),
    )
    .expect("execute");
    let d = st.stage(ExecStage::Distance).expect("distance");
    let p = st.stage(ExecStage::PostFilter).expect("postfilter");
    assert!(p.loops >= 1);
    assert_eq!(
        d.rows_out,
        p.rows_out + p.rows_removed_by_filter.expect("removed")
    );
    // 近い順は id 1..=6。第 1 ラウンドの候補 3 件（id 1,2,3）は x が id 3 の 1 件だけで目標 3 件に
    // 届かず、倍の 6 件で再探索し x が 4 件（id 3..=6）になって終了する。
    // 段の値はラウンドの合計（候補 3+6、フィルタ後 1+4）。
    assert_eq!((p.loops, d.rows_out, p.rows_out), (2, 9, 5));
}
