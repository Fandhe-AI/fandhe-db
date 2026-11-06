//! `EXPLAIN SELECT` が `rerank:` 行（方式・窓幅・MMR の λ・静的な適用可否）を出すことの結合テスト
//! （Issue #2147。ビヘイビア: SEARCH-15・SQL-6 をポインタで参照。設計記録:
//! `docs/design/rerank-wiring-policy.md` D7・§10）。
//!
//! 固定するもの: (1) off では行を出さず出力が不変、(2) hybrid の末尾（`hybrid_bm25:` の直後）・密のみの末尾、
//! (3) 4 種の status、(4) 出力が可視行数・テナントに依存しない、(5) EXPLAIN が検索・リランカー・統計に
//! 触れない、(6) FORMAT JSON が TEXT と一致する。

use std::sync::{Arc, Mutex};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::rerank::{
    CrossEncoderBackend, CrossEncoderConfig, CrossEncoderError, CrossEncoderReranker,
    LexicalOverlapReranker, MmrReranker, RerankMethod, RerankSettings,
};
use engine::row_codec::Value;
use engine::sql::exec::Cell;
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const HYBRID: &str = "SELECT * FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'apple')";
const DENSE: &str = "SELECT * FROM docs ORDER BY embedding <=> '[1.0,0.0]'";

fn setup(name: &str, rerank: Option<RerankSettings>, rows: bool) -> (EngineCore, CleanupGuard) {
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
    if rows {
        let ctx = PolicyContext::with_visibilities("tenant-a", [Visibility::Public])
            .expect("valid tenant");
        for (id, emb, body) in [
            (1u64, [1.0f32, 0.0], "apple zzz"),
            (2, [0.9, 0.1], "apple apple"),
        ] {
            engine::tenant::insert_typed_row(
                &storage,
                "docs",
                &ctx,
                id,
                Visibility::Public,
                &[Value::Vector(emb.to_vec()), Value::Text(body.to_string())],
                &engine::recovery::required_op_id::OperationId::parse(&format!("op-{id}"))
                    .expect("valid operation_id"),
            )
            .expect("insert row");
        }
    }
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(s) = rerank {
        core = core.with_rerank(s);
    }
    (core, guard)
}

fn settings(method: RerankMethod, window: usize) -> RerankSettings {
    RerankSettings::new(method, window).expect("valid settings")
}

fn lexical(window: usize) -> RerankSettings {
    settings(
        RerankMethod::Lexical(LexicalOverlapReranker::default()),
        window,
    )
}

fn mmr(lambda: f64, window: usize) -> RerankSettings {
    settings(
        RerankMethod::Mmr(MmrReranker::new(lambda).expect("valid lambda")),
        window,
    )
}

fn lines(core: &EngineCore, tenant: &str, sql: &str) -> Vec<String> {
    let ctx = PolicyContext::new(tenant).expect("valid tenant");
    let mut session = SessionState::default();
    match core
        .execute_sql_in_session(&ctx, &mut session, sql)
        .unwrap_or_else(|e| panic!("{sql} failed with {}", e.wire_code()))
    {
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

fn explain(core: &EngineCore, body: &str) -> Vec<String> {
    lines(core, "tenant-a", &format!("EXPLAIN {body}"))
}

fn rerank_rows(l: &[String]) -> Vec<String> {
    l.iter()
        .filter(|x| x.starts_with("rerank:"))
        .cloned()
        .collect()
}

#[test]
fn search15_explain_off_has_no_rerank_row_and_output_is_unchanged() {
    let (plain, _g1) = setup("s15x-off-a", None, true);
    let (off, _g2) = setup("s15x-off-b", Some(RerankSettings::off()), true);
    for sql in [format!("{HYBRID} LIMIT 3"), format!("{DENSE} LIMIT 3")] {
        let a = explain(&plain, &sql);
        let b = explain(&off, &sql);
        assert!(rerank_rows(&a).is_empty(), "{a:?}");
        assert_eq!(a, b);
    }
}

#[test]
fn search15_explain_hybrid_lexical_is_eligible_after_bm25_row() {
    let (core, _g) = setup("s15x-lex", Some(lexical(50)), true);
    let l = explain(&core, &format!("{HYBRID} LIMIT 3"));
    assert_eq!(
        l.last().expect("rows"),
        "rerank: method=lexical,window=50,status=eligible"
    );
    assert!(l[l.len() - 2].starts_with("hybrid_bm25: "), "{l:?}");
}

#[test]
fn search15_explain_mmr_shows_lambda_for_hybrid_and_dense() {
    let (core, _g) = setup("s15x-mmr", Some(mmr(0.5, 20)), true);
    let want = "rerank: method=mmr,window=20,mmr_lambda=0.5,status=eligible";
    let h = explain(&core, &format!("{HYBRID} LIMIT 3"));
    assert_eq!(h.last().expect("rows"), want);
    let d = explain(&core, &format!("{DENSE} LIMIT 3"));
    assert_eq!(d.last().expect("rows"), want);
    assert!(d[d.len() - 2].starts_with("scalar_plan: "), "{d:?}");
}

#[test]
fn search15_explain_dense_text_method_is_not_applicable() {
    let (core, _g) = setup("s15x-dense-text", Some(lexical(50)), true);
    let d = explain(&core, &format!("{DENSE} LIMIT 3"));
    assert_eq!(
        rerank_rows(&d),
        ["rerank: method=lexical,window=50,status=not_applicable_dense_text"]
    );
}

#[test]
fn search15_explain_precision_is_not_applicable_and_data_independent() {
    let (core, _g) = setup("s15x-prec", Some(mmr(0.7, 30)), true);
    for sql in [
        format!("{HYBRID} LIMIT 3 USING MODE 'precision'"),
        format!("{HYBRID} LIMIT 3 HINT ORDER(DISTANCE, SCALAR, RLS) USING MODE 'precision'"),
        format!("{DENSE} LIMIT 3 USING MODE 'precision'"),
    ] {
        let a = explain(&core, &sql);
        assert_eq!(
            rerank_rows(&a),
            ["rerank: method=mmr,window=30,mmr_lambda=0.7,status=not_applicable_precision"],
            "{sql}"
        );
        let c = lines(&core, "tenant-c", &format!("EXPLAIN {sql}"));
        assert_eq!(a, c, "{sql}");
    }
}

#[test]
fn search15_explain_mmr_budget_boundary() {
    let (core, _g) = setup("s15x-budget", Some(mmr(0.5, 10_000)), true);
    // 10_000 * 100 = 1_000_000 は予算内、101 件は超過。
    let ok = explain(&core, &format!("{HYBRID} LIMIT 100"));
    assert!(
        ok.last().expect("rows").ends_with("status=eligible"),
        "{ok:?}"
    );
    let over = explain(&core, &format!("{HYBRID} LIMIT 101"));
    assert!(
        over.last()
            .expect("rows")
            .ends_with("status=degraded_mmr_budget"),
        "{over:?}"
    );
}

struct Stub(Arc<Mutex<Vec<String>>>);

impl CrossEncoderBackend for Stub {
    fn score_pairs(&self, _q: &str, passages: &[&str]) -> Result<Vec<f64>, CrossEncoderError> {
        self.0
            .lock()
            .expect("lock")
            .extend(passages.iter().map(|p| p.to_string()));
        Ok(passages.iter().map(|p| p.len() as f64).collect())
    }

    fn max_seq_len(&self) -> usize {
        512
    }
}

#[test]
fn search15_explain_cross_encoder_never_calls_backend_or_stats() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let cfg = CrossEncoderConfig::new(4, 8, 512).expect("cfg");
    let r = CrossEncoderReranker::new(Stub(seen.clone()), cfg).expect("reranker");
    let (core, _g) = setup(
        "s15x-ce",
        Some(settings(RerankMethod::cross_encoder(r), 8)),
        true,
    );
    let before = core.rerank_stats();
    let l = explain(&core, &format!("{HYBRID} LIMIT 3"));
    assert_eq!(
        l.last().expect("rows"),
        "rerank: method=cross_encoder,window=8,status=eligible"
    );
    assert_eq!(core.rerank_stats(), before);
    assert!(seen.lock().expect("lock").is_empty());
}

#[test]
fn search15_explain_is_independent_of_visible_rows() {
    let (full, _g1) = setup("s15x-vis-full", Some(mmr(0.5, 20)), true);
    let (empty, _g2) = setup("s15x-vis-empty", Some(mmr(0.5, 20)), false);
    for sql in [format!("{HYBRID} LIMIT 3"), format!("{DENSE} LIMIT 3")] {
        let a = explain(&full, &sql);
        assert_eq!(a, explain(&empty, &sql), "{sql}");
        assert_eq!(a, lines(&full, "tenant-c", &format!("EXPLAIN {sql}")));
    }
}

#[test]
fn search15_explain_json_matches_text() {
    let (core, _g) = setup("s15x-json", Some(mmr(0.5, 20)), true);
    let sql = format!("{HYBRID} LIMIT 3");
    let text = explain(&core, &sql);
    let json = lines(&core, "tenant-a", &format!("EXPLAIN (FORMAT JSON) {sql}"));
    assert_eq!(json.len(), 1);
    let row = rerank_rows(&text).remove(0);
    let (k, v) = row.split_once(": ").expect("key: value");
    assert!(json[0].contains(&format!("\"{k}\"")), "{}", json[0]);
    assert!(json[0].contains(v), "{}", json[0]);
}
