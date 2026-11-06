//! `EXPLAIN SELECT ... ORDER BY HYBRID(...)` が BM25・RRF 調整引数の実効値を `hybrid_rrf:`／
//! `hybrid_bm25:` 行として出すこと（SEARCH-13・Issue #3051。ポインタ:
//! `docs/spec/04-behavior/search.md`、ADR は `docs/design/hybrid-tuning-parameters.md` §12）の結合テスト。
//!
//! 既存行の不変、指定元（`default`／`specified`）の区別、可視件数に依存する値を出さないこと
//! （テナント間で出力が一致する）、不正な引数が実行時と同じ `wire_code` で拒否されること、
//! FORMAT JSON が TEXT と一致することを固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::result_limits::ResultLimits;
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const BASE: &str = "SELECT * FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'apple'";

const DEFAULT_RRF: &str = "hybrid_rrf: rrf_k=60.0[default],window=200[default],dense_weight=1.0[default],sparse_weight=1.0[default]";
const DEFAULT_BM25: &str = "hybrid_bm25: k1=1.2[default],b=0.75[default]";

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
            (3, [0.8, 0.2], "apple zzz zzz"),
            (4, [0.7, 0.3], "apple apple zzz"),
        ],
    );
    // 別テナントの行（tenant-c は可視行 0 件）。
    seed(
        &storage,
        "tenant-b",
        Visibility::Private,
        &[(11, [1.0, 0.0], "apple apple apple")],
    );
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(l) = limits {
        core = core.with_result_limits(l);
    }
    (core, guard)
}

fn ctx(tenant: &str) -> PolicyContext {
    PolicyContext::new(tenant).expect("valid tenant")
}

fn outcome(core: &EngineCore, tenant: &str, sql: &str) -> Result<SqlOutcome, &'static str> {
    let mut session = SessionState::default();
    core.execute_sql_in_session(&ctx(tenant), &mut session, sql)
        .map_err(|e| e.wire_code())
}

fn lines_as(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    match outcome(core, tenant, sql).unwrap_or_else(|c| panic!("{sql} failed with {c}")) {
        SqlOutcome::Explain(r) | SqlOutcome::Query(r) => r
            .rows
            .iter()
            .map(|row| match &row.cells[0] {
                Cell::Text(s) | Cell::Json(s) => s.clone(),
                other => panic!("expected text, got {other:?}"),
            })
            .collect(),
        other => panic!("expected Explain, got {other:?}"),
    }
}

fn explain(core: &EngineCore, tail: &str, limit: &str) -> Vec<String> {
    lines_as(core, "tenant-a", &format!("EXPLAIN {BASE}{tail}) {limit}"))
}

fn hybrid_rows(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| l.starts_with("hybrid_"))
        .cloned()
        .collect()
}

fn others(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter(|l| !l.starts_with("hybrid_"))
        .cloned()
        .collect()
}

#[test]
fn search13_explain_omitted_arguments_show_defaults_after_scalar_plan() {
    let (core, _g) = setup("search13-explain-default", None);
    let lines = explain(&core, "", "LIMIT 3");
    assert_eq!(lines.len(), 7, "{lines:?}");
    assert!(lines[0].starts_with("mode: recall"), "{lines:?}");
    assert!(lines[4].starts_with("scalar_plan: "), "{lines:?}");
    assert_eq!(lines[5], DEFAULT_RRF);
    assert_eq!(lines[6], DEFAULT_BM25);
}

#[test]
fn search13_explain_specified_arguments_show_effective_values() {
    let (core, _g) = setup("search13-explain-specified", None);
    let lines = explain(
        &core,
        ", k1 => 1.5, b => 0.5, rrf_k => 10, window => 5, weights => '{\"dense\": 2, \"sparse\": 1}'",
        "LIMIT 3",
    );
    assert_eq!(
        hybrid_rows(&lines),
        [
            "hybrid_rrf: rrf_k=10.0[specified],window=5[specified],dense_weight=2.0[specified],sparse_weight=1.0[specified]",
            "hybrid_bm25: k1=1.5[specified],b=0.5[specified]",
        ]
    );
}

#[test]
fn search13_explain_partial_specification_distinguishes_sources() {
    let (core, _g) = setup("search13-explain-partial", None);
    let l = explain(&core, ", k1 => 2", "LIMIT 3");
    assert_eq!(
        hybrid_rows(&l)[1],
        "hybrid_bm25: k1=2.0[specified],b=0.75[default]"
    );
    assert_eq!(hybrid_rows(&l)[0], DEFAULT_RRF);
    let l = explain(
        &core,
        ", weights => '{\"dense\": 0, \"sparse\": 3}'",
        "LIMIT 3",
    );
    assert_eq!(
        hybrid_rows(&l)[0],
        "hybrid_rrf: rrf_k=60.0[default],window=200[default],dense_weight=0.0[specified],sparse_weight=3.0[specified]"
    );
    // 既定値と同じ値でも明示したので specified。
    let l = explain(&core, ", rrf_k => 60", "LIMIT 3");
    assert_eq!(
        hybrid_rows(&l)[0],
        "hybrid_rrf: rrf_k=60.0[specified],window=200[default],dense_weight=1.0[default],sparse_weight=1.0[default]"
    );
}

#[test]
fn search13_explain_default_window_follows_fetch_count() {
    let (core, _g) = setup("search13-explain-window", Some(ResultLimits::new(300)));
    let l = explain(&core, "", "LIMIT 250 OFFSET 10");
    assert!(hybrid_rows(&l)[0].contains("window=260[default]"), "{l:?}");
}

#[test]
fn search13_explain_existing_rows_are_unchanged_and_non_hybrid_has_no_rows() {
    let (core, _g) = setup("search13-explain-existing", None);
    let plain = explain(&core, "", "LIMIT 3");
    let tuned = explain(&core, ", k1 => 1.5, rrf_k => 10", "LIMIT 3");
    assert_eq!(others(&plain), others(&tuned));
    let knn = lines_as(
        &core,
        "tenant-a",
        "EXPLAIN SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3",
    );
    assert!(hybrid_rows(&knn).is_empty(), "{knn:?}");
}

#[test]
fn search13_explain_function_form_shows_same_rows() {
    let (core, _g) = setup("search13-explain-func", None);
    let l = lines_as(
        &core,
        "tenant-a",
        "EXPLAIN SELECT * FROM docs ORDER BY hybrid_rrf(embedding, '[1.0,0.0]', body, 'apple', k1 => 1.5) LIMIT 3",
    );
    assert_eq!(
        hybrid_rows(&l),
        [
            DEFAULT_RRF.to_string(),
            "hybrid_bm25: k1=1.5[specified],b=0.75[default]".to_string()
        ]
    );
}

#[test]
fn search13_explain_precision_default_window_is_data_independent() {
    let (core, _g) = setup("search13-explain-precision", None);
    // DISTANCE 先行（SCALAR 事後フィルタ）の precision だけ取得件数が可視件数に依存する。
    let sql =
        format!("EXPLAIN {BASE}) LIMIT 3 HINT ORDER(DISTANCE, SCALAR, RLS) USING MODE 'precision'");
    let a = lines_as(&core, "tenant-a", &sql);
    // 可視行 0 件のテナントでも出力が一致する（可視件数を漏らさない）。
    let c = lines_as(&core, "tenant-c", &sql);
    assert_eq!(a, c);
    assert_eq!(
        hybrid_rows(&a)[0],
        "hybrid_rrf: rrf_k=60.0[default],window=dynamic[default],dense_weight=1.0[default],sparse_weight=1.0[default]"
    );
    // SCALAR 先行（既定）の precision は静的に決まる（fetch_count.max(2) と既定深さの大きい方）。
    let sql = "EXPLAIN SELECT * FROM docs WHERE body = 'apple zzz zzz' ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'apple') LIMIT 3 USING MODE 'precision'";
    let a = lines_as(&core, "tenant-a", sql);
    assert_eq!(a, lines_as(&core, "tenant-c", sql));
    assert!(hybrid_rows(&a)[0].contains("window=200[default]"), "{a:?}");
}

#[test]
fn search13_explain_rejects_invalid_arguments_like_execution() {
    let (core, _g) = setup("search13-explain-errors", None);
    for (tail, limit) in [
        (", window => 100000000", "LIMIT 3"),
        (", window => 1", "LIMIT 3"),
        (", window => 5", ""),
        (", window => 5", "LIMIT 3 USING MODE 'precision'"),
        (", k1 => 1e300", "LIMIT 3"),
        (", b => 2", "LIMIT 3"),
    ] {
        let sql = format!("{BASE}{tail}) {limit}");
        let run = outcome(&core, "tenant-a", &sql).expect_err("execution must be rejected");
        let exp = outcome(&core, "tenant-a", &format!("EXPLAIN {sql}"))
            .expect_err("EXPLAIN must be rejected");
        assert_eq!(run, exp, "{tail} {limit}");
    }
}

#[test]
fn search13_explain_json_matches_text() {
    let (core, _g) = setup("search13-explain-json", None);
    let sql = format!("{BASE}, k1 => 1.5) LIMIT 3");
    let text = lines_as(&core, "tenant-a", &format!("EXPLAIN {sql}"));
    let json = lines_as(&core, "tenant-a", &format!("EXPLAIN (FORMAT JSON) {sql}"));
    assert_eq!(json.len(), 1);
    for row in hybrid_rows(&text) {
        let (k, v) = row.split_once(": ").expect("key: value");
        assert!(json[0].contains(&format!("\"{k}\"")), "{}", json[0]);
        assert!(json[0].contains(v), "{}", json[0]);
    }
}

#[test]
fn search13_explain_verbose_and_analyze_keep_rows_before_extra_sections() {
    let (core, _g) = setup("search13-explain-verbose", None);
    let sql = format!("{BASE}) LIMIT 3");
    for opt in ["VERBOSE", "ANALYZE"] {
        let l = lines_as(&core, "tenant-a", &format!("EXPLAIN ({opt}) {sql}"));
        let pos = l.iter().position(|x| x == DEFAULT_RRF).expect("rrf row");
        assert_eq!(l[pos + 1], DEFAULT_BM25, "{opt}: {l:?}");
        assert!(l[pos - 1].starts_with("scalar_plan: "), "{opt}: {l:?}");
    }
}
