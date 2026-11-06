//! `HYBRID` の調整引数 `rrf_k`・`window`・`weights` が実行器の `RrfConfig` と候補プール深さに
//! 反映されること（SEARCH-13・Issue #2120）と、`k1`／`b` がクエリ時の BM25 計算だけに作用し
//! キャッシュ済みの疎索引を再構築しないこと（Issue #2121）の結合テスト（ポインタ:
//! `docs/spec/04-behavior/search.md`）。
//!
//! 密側の順位は id 昇順（id=1 が最も近い）、疎側は id=6,5,4 の順に語 `apple` の頻度が高く、
//! id=1〜3 は疎側に現れない。この非対称なデータで重み 0・極端値の決定性（同点は id 昇順）を固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::result_limits::ResultLimits;
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::sql::mode::SessionState;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const BASE: &str = "SELECT * FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'apple'";

fn seed(storage: &Storage, tenant: &str, vis: Visibility, rows: &[(u64, [f32; 2], &str)]) {
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    for (id, emb, body) in rows {
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            *id,
            vis,
            &[
                Value::Vector(emb.to_vec()),
                Value::Text((*body).to_string()),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{tenant}-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
}

fn setup(name: &str, limits: Option<ResultLimits>) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    seed(
        &storage,
        "tenant-a",
        Visibility::Public,
        &[
            (1, [1.0, 0.0], "zzz zzz zzz"),
            (2, [0.9, 0.1], "zzz zzz zzz"),
            (3, [0.8, 0.2], "zzz zzz zzz"),
            (4, [0.7, 0.3], "apple zzz zzz"),
            (5, [0.6, 0.4], "apple apple zzz"),
            (6, [0.5, 0.5], "apple apple apple"),
        ],
    );
    // 別テナントの強く一致する行（自テナントの結果へ混ざってはならない）。
    seed(
        &storage,
        "tenant-b",
        Visibility::Private,
        &[
            (11, [1.0, 0.0], "apple apple apple"),
            (12, [1.0, 0.0], "apple apple apple"),
        ],
    );
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(l) = limits {
        core = core.with_result_limits(l);
    }
    (core, guard)
}

fn ctx() -> PolicyContext {
    PolicyContext::new("tenant-a").expect("valid tenant")
}

fn run(core: &EngineCore, tail: &str, limit: &str) -> QueryResult {
    core.execute_sql(&ctx(), &format!("{BASE}{tail}) {limit}"))
        .expect("query must succeed")
}

fn code(core: &EngineCore, tail: &str, limit: &str) -> &'static str {
    core.execute_sql(&ctx(), &format!("{BASE}{tail}) {limit}"))
        .expect_err("must be rejected")
        .wire_code()
}

fn ids(r: &QueryResult) -> Vec<u64> {
    r.rows.iter().map(|r| r.id).collect()
}

#[test]
fn search13_exec_default_values_are_byte_identical_to_unspecified() {
    let (core, _g) = setup("search13-exec-default", None);
    let base = run(&core, "", "LIMIT 6");
    for tail in [
        ", rrf_k => 60",
        ", k1 => 1.2",
        ", b => 0.75",
        ", k1 => 1.2, b => 0.75",
        ", weights => '{\"dense\": 1, \"sparse\": 1}'",
        ", rrf_k => 60, weights => '{\"dense\": 1.0, \"sparse\": 1.0}', window => 200",
    ] {
        let got = run(&core, tail, "LIMIT 6");
        assert_eq!(
            format!("{:?}", got.rows),
            format!("{:?}", base.rows),
            "{tail}"
        );
    }
}

#[test]
fn search13_exec_rrf_k_changes_fusion_and_extreme_value_is_deterministic() {
    let (core, _g) = setup("search13-exec-rrfk", None);
    // 巨大な rrf_k は順位差が潰れ、所属リスト数だけで決まる。同点は id 昇順。
    let huge = ids(&run(&core, ", rrf_k => 1e300", "LIMIT 6"));
    assert_eq!(huge, vec![4, 5, 6, 1, 2, 3]);
    assert_eq!(huge, ids(&run(&core, ", rrf_k => 1e300", "LIMIT 6")));
    // 小さい rrf_k は上位順位を強く優遇する。
    let small = ids(&run(&core, ", rrf_k => 0.001", "LIMIT 6"));
    assert_eq!(small.first(), Some(&6));
    assert_ne!(small, huge);
}

#[test]
fn search13_exec_zero_weight_is_deterministic_with_id_ascending_ties() {
    let (core, _g) = setup("search13-exec-zero-weight", None);
    let dense_zero = ", weights => '{\"dense\": 0, \"sparse\": 1}'";
    let sparse_zero = ", weights => '{\"dense\": 1, \"sparse\": 0}'";
    let a = ids(&run(&core, dense_zero, "LIMIT 6"));
    assert_eq!(a, vec![6, 5, 4, 1, 2, 3]);
    assert_eq!(a, ids(&run(&core, dense_zero, "LIMIT 6")));
    let b = ids(&run(&core, sparse_zero, "LIMIT 6"));
    assert_eq!(b, vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(b, ids(&run(&core, sparse_zero, "LIMIT 6")));
}

#[test]
fn search13_exec_zero_dense_weight_on_dense_only_degrade_ties_by_id() {
    // 疎コーパスに該当語が無い密のみ縮退経路。密の重み 0 なら全件同点で id 昇順。
    let (core, _g) = setup("search13-exec-degrade-zero", None);
    let sql = "SELECT * FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'nomatchterm', \
               weights => '{\"dense\": 0, \"sparse\": 1}') LIMIT 6";
    let r = core.execute_sql(&ctx(), sql).expect("must succeed");
    assert_eq!(ids(&r), vec![1, 2, 3, 4, 5, 6]);
}

#[test]
fn search13_exec_overflowing_weights_are_rejected_as_22000() {
    let (core, _g) = setup("search13-exec-overflow", None);
    let tail = ", rrf_k => 0.000001, weights => '{\"dense\": 1.7e308, \"sparse\": 1.7e308}'";
    assert_eq!(code(&core, tail, "LIMIT 6"), "22000");
}

#[test]
fn search13_exec_window_range_is_rejected_with_22000_without_clamping() {
    let (core, _g) = setup("search13-exec-window", None);
    assert_eq!(run(&core, ", window => 3", "LIMIT 3").rows.len(), 3);
    assert_eq!(code(&core, ", window => 3", "LIMIT 5"), "22000");
    assert_eq!(code(&core, ", window => 3", "LIMIT 2 OFFSET 2"), "22000");
    // `LIMIT` 省略 + window は（22000 か構文段の拒否かによらず）受理されない。
    assert!(core
        .execute_sql(&ctx(), &format!("{BASE}, window => 3)"))
        .is_err());
}

#[test]
fn search13_exec_window_above_max_result_rows_is_rejected() {
    let (core, _g) = setup("search13-exec-window-cap", Some(ResultLimits::new(10)));
    assert_eq!(run(&core, ", window => 10", "LIMIT 3").rows.len(), 3);
    assert_eq!(code(&core, ", window => 11", "LIMIT 3"), "22000");
}

#[test]
fn search13_exec_precision_mode_uses_tuned_normalization() {
    let (core, _g) = setup("search13-exec-precision", None);
    let lim = "LIMIT 3 USING MODE 'precision'";
    let base = run(&core, "", lim);
    let same = run(
        &core,
        ", rrf_k => 60, weights => '{\"dense\": 1, \"sparse\": 1}'",
        lim,
    );
    assert_eq!(format!("{:?}", base.rows), format!("{:?}", same.rows));
    // window と precision の組み合わせは k_eff がデータ依存のため、データ件数によらず
    // 常に 0A000 で拒否する（LIMIT=window=1 の SCALAR 先行でも同じ）。
    for tail in [", window => 3", ", window => 200", ", window => 1"] {
        assert_eq!(code(&core, tail, lim), "0A000");
    }
    assert_eq!(
        code(&core, ", window => 1", "LIMIT 1 USING MODE 'precision'"),
        "0A000"
    );
    let scalar = format!("{BASE}, window => 1) WHERE id > 0 LIMIT 1 USING MODE 'precision'");
    assert_ne!(
        core.execute_sql(&ctx(), &scalar)
            .map(|_| "ok")
            .unwrap_or_else(|e| e.wire_code()),
        "XX000"
    );
}

#[test]
fn search13_exec_explain_accepts_all_tuning_args_including_k1_b() {
    let (core, _g) = setup("search13-exec-explain", None);
    let ok = format!(
        "EXPLAIN {BASE}, rrf_k => 10, window => 5, weights => '{{\"dense\": 2, \"sparse\": 1}}') LIMIT 3"
    );
    core.execute_sql_in_session(&ctx(), &mut SessionState::default(), &ok)
        .expect("explain must succeed");
    let with_bm25 = format!("EXPLAIN {BASE}, k1 => 1.2, b => 0.75) LIMIT 3");
    core.execute_sql_in_session(&ctx(), &mut SessionState::default(), &with_bm25)
        .expect("explain with k1/b must succeed (Issue #2121)");
}

const SPARSE_ONLY: &str = ", weights => '{\"dense\": 0, \"sparse\": 1}'";

#[test]
fn search13_exec_k1_changes_sparse_ranking_at_query_time() {
    let (core, _g) = setup("search13-exec-k1-rank", None);
    let tail = |extra: &str| format!("{SPARSE_ONLY}{extra}");
    assert_eq!(
        ids(&run(&core, &tail(""), "LIMIT 6")),
        vec![6, 5, 4, 1, 2, 3]
    );
    // k1 = 0 は tf が飽和せず全一致文書が同点になり、同点は id 昇順で決定的。
    assert_eq!(
        ids(&run(&core, &tail(", k1 => 0"), "LIMIT 6")),
        vec![4, 5, 6, 1, 2, 3]
    );
    // 他のクエリへ影響しない（後続の無指定は元の結果のまま）。
    assert_eq!(
        ids(&run(&core, &tail(""), "LIMIT 6")),
        vec![6, 5, 4, 1, 2, 3]
    );
}

#[test]
fn search13_exec_b_changes_sparse_ranking_with_unequal_doc_lengths() {
    let path = unique_db_path("search13-exec-b-rank");
    let _g = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ColumnDef::new("body", ColumnType::Text, true),
            ],
        ))
        .expect("create table");
    seed(
        &storage,
        "tenant-a",
        Visibility::Public,
        &[
            (1, [1.0, 0.0], "apple zzz"),
            (2, [1.0, 0.0], "apple apple zzz zzz zzz zzz zzz zzz"),
            (3, [1.0, 0.0], "zzz zzz zzz"),
            (4, [1.0, 0.0], "zzz zzz zzz"),
        ],
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let first2 = |extra: &str| {
        let r = run(&core, &format!("{SPARSE_ONLY}{extra}"), "LIMIT 4");
        ids(&r)[..2].to_vec()
    };
    // b = 0 は文書長を無視するため tf の高い長文書(2)が上位、b = 1 は短文書(1)が上位。
    assert_eq!(first2(", b => 0"), vec![2, 1]);
    assert_eq!(first2(", b => 1"), vec![1, 2]);
    assert_eq!(first2(", b => 0"), vec![2, 1]);
}

#[test]
fn search13_exec_k1_b_reuse_cached_sparse_index_without_rebuild() {
    let (core, _g) = setup("search13-exec-k1-cache", None);
    let base = run(&core, "", "LIMIT 6");
    let s0 = core.sparse_index_cache_stats();
    assert_eq!(s0.misses, 1, "first query builds the index once");
    assert_eq!(s0.entries, 1);
    let tails = [
        ", k1 => 0.5",
        ", b => 0.2",
        ", k1 => 2, b => 1",
        ", k1 => 0, b => 0",
    ];
    for (n, tail) in tails.iter().enumerate() {
        let _ = run(&core, tail, "LIMIT 6");
        let s = core.sparse_index_cache_stats();
        assert_eq!(s.misses, 1, "{tail}: no rebuild");
        assert_eq!(s.entries, 1, "{tail}");
        assert_eq!(s.hits, s0.hits + n as u64 + 1, "{tail}");
    }
    // 調整後の無指定クエリは最初の結果とバイト一致する（キャッシュ索引に値が混入しない）。
    let after = run(&core, "", "LIMIT 6");
    assert_eq!(format!("{:?}", after.rows), format!("{:?}", base.rows));
    // 同じ調整値の再実行は同一結果。
    let a = run(&core, ", k1 => 0.5, b => 0.2", "LIMIT 6");
    let b = run(&core, ", k1 => 0.5, b => 0.2", "LIMIT 6");
    assert_eq!(format!("{:?}", a.rows), format!("{:?}", b.rows));
}

#[test]
fn search13_exec_extreme_k1_is_22000_and_large_k1_is_deterministic() {
    let (core, _g) = setup("search13-exec-k1-extreme", None);
    assert_eq!(code(&core, ", k1 => 1e308", "LIMIT 6"), "22000");
    assert_eq!(code(&core, ", k1 => 1e201", "LIMIT 6"), "22000");
    let a = run(&core, ", k1 => 1e6", "LIMIT 6");
    let b = run(&core, ", k1 => 1e6", "LIMIT 6");
    assert_eq!(format!("{:?}", a.rows), format!("{:?}", b.rows));
    assert_eq!(a.rows.len(), 6);
}
#[test]
fn search13_exec_tuning_does_not_leak_other_tenant_rows() {
    let (core, _g) = setup("search13-exec-tenant", None);
    let r = run(
        &core,
        ", rrf_k => 10, weights => '{\"dense\": 5, \"sparse\": 1}'",
        "LIMIT 10",
    );
    assert!(
        ids(&r).iter().all(|id| (1..=6).contains(id)),
        "{:?}",
        ids(&r)
    );
    assert_eq!(r.rows.len(), 6);
    let r = run(&core, ", k1 => 0.3, b => 0.9", "LIMIT 10");
    assert!(
        ids(&r).iter().all(|id| (1..=6).contains(id)),
        "{:?}",
        ids(&r)
    );
    assert_eq!(r.rows.len(), 6);
}

/// 疎索引が無い（可視行に本文が無い）文でも `k1`／`b` の不正値は 22000 になり、
/// データ依存で受理されない。EXPLAIN も実行と同じ検証を行う（Issue #2121）。
#[test]
fn search13_exec_invalid_k1_b_rejected_without_sparse_index_and_in_explain() {
    let (core, _g) = setup("search13-exec-k1-nosparse", None);
    let other = PolicyContext::new("tenant-without-rows").expect("valid tenant");
    for tail in [", k1 => 1e300", ", k1 => 1e201", ", b => 2"] {
        let sql = format!("{BASE}{tail}) LIMIT 3");
        let e = core
            .execute_sql(&other, &sql)
            .expect_err("must be rejected");
        assert_eq!(e.wire_code(), "22000", "{tail}");
        let e = core
            .execute_sql_in_session(
                &other,
                &mut SessionState::default(),
                &format!("EXPLAIN {sql}"),
            )
            .expect_err("explain must be rejected");
        assert_eq!(e.wire_code(), "22000", "explain {tail}");
        assert_eq!(code(&core, tail, "LIMIT 3"), "22000", "{tail}");
    }
}
