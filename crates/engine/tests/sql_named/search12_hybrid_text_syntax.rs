//! `HYBRID` の検索語引数をクエリ構文（フレーズ・`or`・除外・前方一致）として解釈する結線の
//! 受け入れテスト（Issue #2116。ビヘイビア: SEARCH-12・SQL-4・RLS-8。ポインタ:
//! `docs/spec/04-behavior/`。設計記録は `docs/design/fulltext-query-syntax.md` D7〜D9）。
//!
//! 密側の影響を消すため、全行の埋め込みを同一にして疎側の寄与だけを見る。疎側に現れた行は
//! `hybrid_sparse_rank()` が非 NULL になるので、構文の効果は「疎順位を持つ id の集合」で固定する。
//! `USING PLAN` の `search_terms` は literal として扱うこと（D9）も本ファイルで固定する。

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::embedding::{EmbedError, Embedder};
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sql::exec::{Cell, QueryResult};
use engine::sql::mode::SessionState;
use engine::sql::SqlOutcome;
use engine::storage::{Storage, Visibility};
use engine::text_query_limits::TextQueryLimits;

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

fn seed(storage: &Storage, tenant: &str, vis: Visibility, rows: &[(u64, &str)]) {
    let ctx = PolicyContext::with_visibilities(tenant, [Visibility::Public, Visibility::Private])
        .expect("valid tenant");
    for (id, body) in rows {
        engine::tenant::insert_typed_row(
            storage,
            "docs",
            &ctx,
            *id,
            vis,
            &[
                Value::Vector(vec![1.0, 0.0]),
                Value::Text((*body).to_string()),
            ],
            &OperationId::parse(&format!("op-{tenant}-{id}")).expect("valid operation_id"),
        )
        .expect("insert row");
    }
}

fn open(name: &str, own: &[(u64, &str)], other: &[(u64, &str)]) -> (Storage, CleanupGuard) {
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
    if !own.is_empty() {
        seed(&storage, "tenant-a", Visibility::Public, own);
    }
    if !other.is_empty() {
        seed(&storage, "tenant-b", Visibility::Private, other);
    }
    (storage, guard)
}

const OWN: &[(u64, &str)] = &[
    (1, "apple banana"),
    (2, "apple cherry"),
    (3, "banana cherry"),
    (4, "apple banana cherry"),
    (5, "applesauce juice"),
    (6, "zzz filler"),
];

fn setup(name: &str, limits: Option<TextQueryLimits>) -> (EngineCore, CleanupGuard) {
    let (storage, guard) = open(name, OWN, &[]);
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(l) = limits {
        core = core.with_text_query_limits(l);
    }
    (core, guard)
}

fn ctx() -> PolicyContext {
    PolicyContext::new("tenant-a").expect("valid tenant")
}

fn sql(q: &str) -> String {
    format!("SELECT hybrid_sparse_rank() FROM docs ORDER BY HYBRID(embedding, '[1.0,0.0]', body, '{q}') LIMIT 20")
}

fn run(core: &EngineCore, q: &str) -> QueryResult {
    core.execute_sql(&ctx(), &sql(q))
        .expect("query must succeed")
}

fn code(core: &EngineCore, q: &str) -> &'static str {
    core.execute_sql(&ctx(), &sql(q))
        .expect_err("must be rejected")
        .wire_code()
}

/// 疎順位を持つ id を昇順で返す。
fn sparse_ids(r: &QueryResult) -> Vec<u64> {
    let mut v: Vec<u64> = r
        .rows
        .iter()
        .filter(|row| !matches!(row.cells.first(), Some(Cell::Null)))
        .map(|row| row.id)
        .collect();
    v.sort_unstable();
    v
}

#[test]
fn search12_syntax_narrows_and_widens_the_sparse_side() {
    let (core, _g) = setup("search12-syntax", None);
    // 演算子なし（従来どおり任意語）。
    assert_eq!(sparse_ids(&run(&core, "apple")), vec![1, 2, 4]);
    // 除外。
    assert_eq!(sparse_ids(&run(&core, "apple -banana")), vec![2]);
    // フレーズ（連続一致）。
    assert_eq!(sparse_ids(&run(&core, "\"apple banana\"")), vec![1, 4]);
    // 前方一致。
    assert_eq!(sparse_ids(&run(&core, "app*")), vec![1, 2, 4, 5]);
    // or はグループの和集合。
    assert_eq!(
        sparse_ids(&run(&core, "\"apple banana\" or \"banana cherry\"")),
        vec![1, 3, 4]
    );
    // 除外だけの検索語は疎側 0 件（密側のみの結果は残る）。
    let only_excl = run(&core, "-banana");
    assert!(sparse_ids(&only_excl).is_empty());
    assert_eq!(only_excl.rows.len(), 6);
}

#[test]
fn search12_query_without_operators_is_unchanged_by_the_syntax_path() {
    // 語中の `-`・`a-b` は区切り文字のまま（D1）。構文経路でも従来と同じ結果になる。
    let (core, _g) = setup("search12-compat", None);
    let hyphen = run(&core, "apple-banana");
    let spaced = run(&core, "apple banana");
    assert_eq!(format!("{:?}", hyphen.rows), format!("{:?}", spaced.rows));
}

#[test]
fn search12_limits_reject_with_54000_and_length_stays_22000() {
    let (core, _g) = setup("search12-limits", Some(TextQueryLimits::new(2, 1)));
    // 項数超過（3 語 > 2）。
    assert_eq!(code(&core, "apple banana cherry"), "54000");
    // 展開数超過（"app" の可視展開は apple・applesauce の 2 語 > 1）。
    assert_eq!(code(&core, "app*"), "54000");
    // 範囲内は成功する。
    assert_eq!(sparse_ids(&run(&core, "apple banana")), vec![1, 2, 3, 4]);
    // 既存のバイト長上限は従来どおり 22000。項数上限より先に判定される（D7 の検査順）。
    let long = "x ".repeat(40_000);
    assert_eq!(code(&core, &long), "22000");
}

#[test]
fn search12_prefix_expansion_counts_only_visible_terms() {
    // 他テナントだけが持つ語は展開数の判定にも結果にも影響しない（RLS-8）。
    let (storage, _g) = open(
        "search12-rls",
        OWN,
        &[
            (11, "applepie applet appleton"),
            (12, "banana banana banana"),
        ],
    );
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_text_query_limits(TextQueryLimits::new(8, 2));
    // 自テナントの展開は apple・applesauce の 2 語で上限ちょうど。他テナントの 3 語は数えない。
    assert_eq!(sparse_ids(&run(&core, "app*")), vec![1, 2, 4, 5]);
    // 他テナントの語での除外は自テナントの結果へ影響しない。
    assert_eq!(sparse_ids(&run(&core, "apple -appleton")), vec![1, 2, 4]);
}

#[test]
fn search12_degraded_dense_only_path_still_validates_the_query() {
    // 可視行に本文が 1 件もない表（疎索引を作れず密のみへ縮退）でも、データに依存しない検査
    // （項数）は行う。結果が表の内容次第で受理／拒否に変わらないようにする（fail-closed）。
    let (storage, _g) = open("search12-degraded", &[], &[]);
    let ctx_a = ctx();
    engine::tenant::insert_typed_row(
        &storage,
        "docs",
        &ctx_a,
        1,
        Visibility::Public,
        &[Value::Vector(vec![1.0, 0.0]), Value::Null],
        &OperationId::parse("op-null-1").expect("valid operation_id"),
    )
    .expect("insert null body");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_text_query_limits(TextQueryLimits::new(2, 1));
    assert_eq!(code(&core, "apple banana cherry"), "54000");
    let ok = run(&core, "apple banana");
    assert_eq!(ok.rows.len(), 1);
}

// ---- USING PLAN（D9）: LLM 出力の search_terms には構文を適用しない ----

struct FixedEmbedder;

impl Embedder for FixedEmbedder {
    fn dim(&self) -> u32 {
        4
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3, 0.4]).collect())
    }
}

struct StubLlm(&'static str);

impl LlmClient for StubLlm {
    fn complete(&self, _prompt: &str) -> Result<String, PlanError> {
        Ok(self.0.to_string())
    }
}

fn plan_core(name: &str, response: &'static str) -> (EngineCore, CleanupGuard) {
    let path = unique_db_path(name);
    let guard = CleanupGuard(path.clone());
    let storage = Storage::open(&path).expect("open storage");
    storage
        .create_table(&TableSchema::new(
            "docs",
            vec![
                ColumnDef::new("embedding", ColumnType::Vector(4), false),
                ColumnDef::new("path", ColumnType::Text, false),
                ColumnDef::new("body", ColumnType::Text, false),
            ],
        ))
        .expect("create table");
    let pctx =
        PolicyContext::with_visibilities("tenant-a", [Visibility::Public, Visibility::Private])
            .expect("valid tenant");
    for (id, path_text, body) in [
        (1u64, "docs/a.md", "alpha content in english"),
        (2u64, "docs/b.md", "beta content in english"),
    ] {
        engine::tenant::insert_typed_row(
            &storage,
            "docs",
            &pctx,
            id,
            Visibility::Public,
            &[
                Value::Vector(vec![0.1, 0.2, 0.3, 0.4]),
                Value::Text(path_text.to_string()),
                Value::Text(body.to_string()),
            ],
            &OperationId::parse(&format!("plan-op-{id}")).expect("valid operation_id"),
        )
        .expect("insert row");
    }
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_embedder(Box::new(FixedEmbedder))
        .with_query_planner(Box::new(StubLlm(response)));
    (core, guard)
}

const NEGATED_TERMS: &str =
    r#"{"search_terms": ["alpha", "-beta"], "path_hint": null, "kind_hint": null}"#;
const PLAIN_TERMS: &str =
    r#"{"search_terms": ["alpha", "beta"], "path_hint": null, "kind_hint": null}"#;

fn rows_of(core: &EngineCore) -> Vec<(u64, f64)> {
    core.execute_sql(
        &PolicyContext::new("tenant-a").expect("valid tenant"),
        "SELECT id FROM docs USING PLAN('find content') LIMIT 10",
    )
    .expect("USING PLAN must succeed")
    .rows
    .iter()
    .map(|r| (r.id, r.score))
    .collect()
}

#[test]
fn search12_using_plan_treats_search_terms_as_literal() {
    // `-beta` が除外として働けば行 2 は疎側から落ちてスコアが下がる。literal なら語 `beta` として
    // 扱われ、`beta` を含まない通常の検索語（PLAIN_TERMS）と同一の順位・スコアになる。
    let (neg, _g1) = plan_core("search12-plan-neg", NEGATED_TERMS);
    let (plain, _g2) = plan_core("search12-plan-plain", PLAIN_TERMS);
    assert_eq!(rows_of(&neg), rows_of(&plain));
}

#[test]
fn search12_explain_search_terms_match_the_literal_execution() {
    let (core, _g) = plan_core("search12-plan-explain", NEGATED_TERMS);
    let mut session = SessionState::default();
    let outcome = core
        .execute_sql_in_session(
            &PolicyContext::new("tenant-a").expect("valid tenant"),
            &mut session,
            "EXPLAIN SELECT id FROM docs USING PLAN('find content') LIMIT 10",
        )
        .expect("EXPLAIN must succeed");
    let SqlOutcome::Explain(result) = outcome else {
        panic!("expected Explain outcome");
    };
    let lines: Vec<String> = result
        .rows
        .iter()
        .filter_map(|r| match r.cells.first() {
            Some(Cell::Text(s)) => Some(s.clone()),
            _ => None,
        })
        .collect();
    assert!(
        lines.contains(&"search_terms[0]: alpha".to_string()),
        "{lines:?}"
    );
    // 演算子付きの語も加工されずそのまま出る（実行時も literal として使われる）。
    assert!(
        lines.contains(&"search_terms[1]: -beta".to_string()),
        "{lines:?}"
    );
}
