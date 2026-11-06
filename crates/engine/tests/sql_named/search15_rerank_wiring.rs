//! リランカーの検索後段結線の受け入れテスト（Issue #2142。ビヘイビア: SEARCH-15・SEARCH-7・
//! SQL-4・RLS-7/8 をポインタで参照。設計記録: `docs/design/rerank-wiring-policy.md`）。
//!
//! 固定するもの: (1) off（`with_rerank` 無し・`with_rerank(off)`）の結果が完全一致、
//! (2) 取得窓の広がりが off の先頭 `k_eff` 件を変えないこと、(3) 窓内の昇格・テキスト無し行の固定、
//! (4) 密のみ・precision は適用外で素通し、(5) 契約違反・上限超過は検索を拒否せず off と同一の
//! 結果へ縮退しカウンタに残ること、(6) リランカーが受け取る候補が自テナントの可視行だけであること。

use std::sync::{Arc, Mutex};

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::EngineCore;
use engine::kernel::CpuScalarProvider;
use engine::policy::PolicyContext;
use engine::rerank::{
    CrossEncoderBackend, CrossEncoderConfig, CrossEncoderError, CrossEncoderReranker,
    LexicalOverlapReranker, MmrReranker, RerankCandidate, RerankError, RerankMethod,
    RerankSettings, RerankStats, RerankedHit, Reranker,
};
use engine::row_codec::Value;
use engine::sql::exec::QueryResult;
use engine::storage::{Storage, Visibility};

use crate::temp_db;
use temp_db::{unique_db_path, CleanupGuard};

const HYBRID: &str = "ORDER BY HYBRID(embedding, '[1.0,0.0]', body, 'apple banana'";

fn seed(storage: &Storage, tenant: &str, vis: Visibility, rows: &[(u64, [f32; 2], Option<&str>)]) {
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
                body.map_or(Value::Null, |b| Value::Text(b.to_string())),
            ],
            &engine::recovery::required_op_id::OperationId::parse(&format!("op-{tenant}-{id}"))
                .expect("valid operation_id"),
        )
        .expect("insert row");
    }
}

/// 自テナント 8 行（密は id 昇順に近い・疎は 5〜8 が語に一致）と、別テナントの強一致 3 行。
fn setup(name: &str, settings: Option<RerankSettings>) -> (EngineCore, CleanupGuard) {
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
            (1, [1.0, 0.0], Some("zzz zzz zzz")),
            (2, [0.95, 0.05], Some("zzz zzz zzz")),
            (3, [0.9, 0.1], Some("zzz zzz zzz")),
            (4, [0.85, 0.15], Some("zzz zzz zzz")),
            (5, [0.5, 0.5], Some("apple zzz zzz")),
            (6, [0.4, 0.6], Some("apple banana zzz")),
            (7, [0.3, 0.7], Some("apple apple banana")),
            (8, [0.2, 0.8], Some("apple banana apple banana")),
        ],
    );
    seed(
        &storage,
        "tenant-b",
        Visibility::Private,
        &[
            (
                11,
                [1.0, 0.0],
                Some("OTHERTENANT apple banana apple banana"),
            ),
            (
                12,
                [1.0, 0.0],
                Some("OTHERTENANT apple banana apple banana"),
            ),
            (
                13,
                [1.0, 0.0],
                Some("OTHERTENANT apple banana apple banana"),
            ),
        ],
    );
    let mut core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    if let Some(s) = settings {
        core = core.with_rerank(s);
    }
    (core, guard)
}

fn ctx() -> PolicyContext {
    PolicyContext::new("tenant-a").expect("valid tenant")
}

fn run(core: &EngineCore, sql: &str) -> QueryResult {
    core.execute_sql(&ctx(), sql).expect("query must succeed")
}

fn ids(r: &QueryResult) -> Vec<u64> {
    r.rows.iter().map(|r| r.id).collect()
}

fn hybrid_sql(tail: &str) -> String {
    format!("SELECT id FROM docs {HYBRID}) {tail}")
}

fn settings(method: RerankMethod, window: usize) -> RerankSettings {
    RerankSettings::new(method, window).expect("valid window")
}

/// 入力順（融合スコア降順）の先頭から `final_k` 件をそのまま返す記録付きリランカー。
/// 受け取った候補テキストを `seen` に溜める。
struct Recording {
    seen: Arc<Mutex<Vec<String>>>,
}

impl Reranker for Recording {
    fn rerank(
        &self,
        _q: &str,
        candidates: &[RerankCandidate<'_>],
        final_k: usize,
    ) -> Result<Vec<RerankedHit>, RerankError> {
        let mut seen = self.seen.lock().expect("lock");
        seen.extend(candidates.iter().map(|c| c.text.to_string()));
        Ok(candidates
            .iter()
            .take(final_k)
            .map(|c| RerankedHit {
                id: c.id,
                score: c.fused_score,
            })
            .collect())
    }
}

/// 入力の逆順で上位 `final_k` 件を返す（窓内の昇格を決定的に作る）。
struct Reverse;

impl Reranker for Reverse {
    fn rerank(
        &self,
        _q: &str,
        candidates: &[RerankCandidate<'_>],
        final_k: usize,
    ) -> Result<Vec<RerankedHit>, RerankError> {
        Ok(candidates
            .iter()
            .rev()
            .take(final_k)
            .enumerate()
            .map(|(i, c)| RerankedHit {
                id: c.id,
                score: 1.0 - (i as f64) * 0.001,
            })
            .collect())
    }
}

/// 契約違反を注入する。
enum Bad {
    ForeignId,
    Missing,
    Oversized,
    QueryTooLong,
}

impl Reranker for Bad {
    fn rerank(
        &self,
        _q: &str,
        candidates: &[RerankCandidate<'_>],
        final_k: usize,
    ) -> Result<Vec<RerankedHit>, RerankError> {
        match self {
            Bad::ForeignId => Ok(vec![RerankedHit {
                id: u64::MAX,
                score: 1.0,
            }]),
            Bad::Missing => Ok(candidates
                .iter()
                .take(final_k.saturating_sub(1))
                .map(|c| RerankedHit {
                    id: c.id,
                    score: c.fused_score,
                })
                .collect()),
            Bad::Oversized => Ok(candidates
                .iter()
                .take(final_k + 1)
                .map(|c| RerankedHit {
                    id: c.id,
                    score: c.fused_score,
                })
                .collect()),
            Bad::QueryTooLong => Err(RerankError::QueryTextTooLong { len: 1, max: 0 }),
        }
    }
}

#[test]
fn search15_off_is_identical_with_and_without_explicit_off() {
    let (plain, _g1) = setup("search15-off-plain", None);
    let (explicit, _g2) = setup("search15-off-explicit", Some(RerankSettings::off()));
    let sqls = [
        hybrid_sql("LIMIT 3"),
        hybrid_sql("LIMIT 2 OFFSET 1"),
        format!("SELECT id FROM docs {HYBRID}) LIMIT 4 HINT ORDER(DISTANCE, SCALAR, RLS)"),
        "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3".to_string(),
        hybrid_sql("LIMIT 3 USING MODE 'precision'"),
    ];
    for sql in &sqls {
        assert_eq!(run(&plain, sql), run(&explicit, sql), "{sql}");
    }
    assert_eq!(plain.rerank_stats(), RerankStats::default());
    assert_eq!(explicit.rerank_stats(), RerankStats::default());
}

#[test]
fn search15_widened_fetch_keeps_off_prefix_and_identity_matches_off() {
    let (off, _g1) = setup("search15-prefix-off", None);
    let (idy, _g2) = setup(
        "search15-prefix-identity",
        Some(settings(RerankMethod::Identity, 8)),
    );
    for tail in [", window => 20", ", rrf_k => 10", ""] {
        for limit in ["LIMIT 2", "LIMIT 3 OFFSET 1", "LIMIT 8"] {
            let sql = format!("SELECT id FROM docs {HYBRID}{tail}) {limit}");
            assert_eq!(run(&off, &sql), run(&idy, &sql), "{sql}");
        }
    }
    let s = idy.rerank_stats();
    assert!(s.applied > 0);
    assert_eq!(s.degraded_total(), 0);
}

#[test]
fn search15_window_promotes_rows_beyond_k_and_keeps_fused_scores() {
    let (off, _g1) = setup("search15-promote-off", None);
    let (rev, _g2) = setup(
        "search15-promote-rev",
        Some(settings(RerankMethod::Custom(Arc::new(Reverse)), 6)),
    );
    let all = ids(&run(&off, &hybrid_sql("LIMIT 6")));
    assert_eq!(all.len(), 6);
    let promoted = ids(&run(&rev, &hybrid_sql("LIMIT 2")));
    assert_eq!(promoted, vec![all[5], all[4]]);
    // OFFSET は並べ替え後の列に対して適用される。
    let skipped = ids(&run(&rev, &hybrid_sql("LIMIT 1 OFFSET 1")));
    assert_eq!(skipped, vec![all[4]]);
    // 返却スコアは融合スコアのまま（SEARCH-17 (1) のスコア一致を壊さない）。
    let score_sql = |tail: &str| format!("SELECT hybrid_score() FROM docs {HYBRID}) {tail}");
    let off_rows = run(&off, &score_sql("LIMIT 6"));
    let rev_rows = run(&rev, &score_sql("LIMIT 2"));
    for row in &rev_rows.rows {
        let same = off_rows
            .rows
            .iter()
            .find(|o| o.id == row.id)
            .expect("row in off result");
        assert_eq!(same.cells, row.cells);
    }
    assert_eq!(rev.rerank_stats().applied, 3);
}

#[test]
fn search15_lexical_applies_and_counts() {
    let (off, _g1) = setup("search15-lex-off", None);
    let (lex, _g2) = setup(
        "search15-lex",
        Some(settings(
            RerankMethod::Lexical(LexicalOverlapReranker::default()),
            8,
        )),
    );
    let sql = hybrid_sql("LIMIT 4");
    let got = run(&lex, &sql);
    assert_eq!(got.rows.len(), 4);
    // 結果は自テナントの可視行だけ（別テナントの id 11〜13 は現れない）。
    assert!(ids(&got).iter().all(|id| (1..=8).contains(id)));
    // 集合は off の窓（8 件）内の並べ替えで、重複しない。
    let mut uniq = ids(&got);
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(uniq.len(), 4);
    let _ = run(&off, &sql);
    let s = lex.rerank_stats();
    assert_eq!(s.applied, 1);
    assert_eq!(s.degraded_total(), 0);
}

#[test]
fn search15_dense_only_and_precision_are_not_applicable() {
    let (off, _g1) = setup("search15-na-off", None);
    let (lex, _g2) = setup(
        "search15-na-lex",
        Some(settings(
            RerankMethod::Lexical(LexicalOverlapReranker::default()),
            8,
        )),
    );
    let dense = "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]' LIMIT 3";
    let prec = hybrid_sql("LIMIT 3 USING MODE 'precision'");
    for sql in [dense.to_string(), prec] {
        assert_eq!(run(&off, &sql), run(&lex, &sql), "{sql}");
    }
    let s = lex.rerank_stats();
    assert_eq!(s.not_applicable, 2);
    assert_eq!(s.applied, 0);
    assert_eq!(s.degraded_total(), 0);
}

#[test]
fn search15_contract_violations_degrade_to_the_off_result() {
    let (off, _g) = setup("search15-bad-off", None);
    for (name, bad) in [
        ("foreign", Bad::ForeignId),
        ("missing", Bad::Missing),
        ("oversized", Bad::Oversized),
        ("toolong", Bad::QueryTooLong),
    ] {
        let (core, _g2) = setup(
            &format!("search15-bad-{name}"),
            Some(settings(RerankMethod::Custom(Arc::new(bad)), 8)),
        );
        for tail in ["LIMIT 3", "LIMIT 2 OFFSET 1"] {
            let sql = hybrid_sql(tail);
            assert_eq!(run(&off, &sql), run(&core, &sql), "{name} {tail}");
        }
        let s = core.rerank_stats();
        assert_eq!(s.applied, 0, "{name}");
        assert_eq!(s.degraded_total(), 2, "{name}");
        if name == "toolong" {
            assert_eq!(s.degraded_input_limit, 2);
        } else {
            assert_eq!(s.degraded_contract_violation, 2, "{name}");
        }
    }
}

#[test]
fn search15_reranker_only_sees_own_tenant_visible_rows() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (core, _g) = setup(
        "search15-rls",
        Some(settings(
            RerankMethod::Custom(Arc::new(Recording { seen: seen.clone() })),
            50,
        )),
    );
    let got = run(&core, &hybrid_sql("LIMIT 3"));
    assert!(ids(&got).iter().all(|id| (1..=8).contains(id)));
    let seen = seen.lock().expect("lock");
    assert!(!seen.is_empty());
    assert!(seen.iter().all(|t| !t.contains("OTHERTENANT")));
    assert!(seen.len() <= 8);
}

#[test]
fn search15_rows_without_text_stay_in_place() {
    let path = unique_db_path("search15-null");
    let _guard = CleanupGuard(path.clone());
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
            (1, [1.0, 0.0], Some("apple banana")),
            (2, [0.9, 0.1], None),
            (3, [0.8, 0.2], Some("apple")),
            (4, [0.7, 0.3], Some("banana")),
        ],
    );
    let off = EngineCore::from_storage(storage, Box::new(CpuScalarProvider));
    let all = ids(&run(&off, &hybrid_sql("LIMIT 4")));
    drop(off);
    let storage = Storage::open(&path).expect("reopen");
    let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
        .with_rerank(settings(RerankMethod::Custom(Arc::new(Reverse)), 4));
    let got = ids(&run(&core, &hybrid_sql("LIMIT 4")));
    // 本文 NULL の id=2 は元の位置に固定され、それ以外が逆順になる。
    let pos = all.iter().position(|id| *id == 2).expect("id 2 present");
    assert_eq!(got.iter().position(|id| *id == 2), Some(pos));
    let mut others_off: Vec<u64> = all.iter().copied().filter(|i| *i != 2).collect();
    others_off.reverse();
    let others_got: Vec<u64> = got.iter().copied().filter(|i| *i != 2).collect();
    assert_eq!(others_got, others_off);
}

#[test]
fn search15_repeated_query_and_write_txn_still_rerank() {
    let (off, _g1) = setup("search15-cache-off", None);
    let (rev, _g2) = setup(
        "search15-cache-rev",
        Some(settings(RerankMethod::Custom(Arc::new(Reverse)), 6)),
    );
    let all = ids(&run(&off, &hybrid_sql("LIMIT 6")));
    let expected = vec![all[5], all[4]];
    // 同じ文を繰り返す（疎索引・アリーナのキャッシュにヒットした 2 回目以降も同じ結果）。
    for _ in 0..3 {
        assert_eq!(ids(&run(&rev, &hybrid_sql("LIMIT 2"))), expected);
    }
    assert_eq!(rev.rerank_stats().applied, 3);

    // 書き込みトランザクション内（未 commit 行を含む）でもテキストを取れてリランクされる。
    let mut session = engine::sql::mode::SessionState::default();
    let mut txn = rev.new_session_transaction();
    rev.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "BEGIN")
        .expect("begin");
    rev.execute_sql_in_txn(
        &ctx(),
        &mut session,
        &mut txn,
        "INSERT INTO docs (id, embedding, body) VALUES (21, '[0.1,0.9]', 'apple banana') \
         USING OPERATION_ID 'search15-txn-21'",
    )
    .expect("insert");
    let outcome = rev
        .execute_sql_in_txn(&ctx(), &mut session, &mut txn, &hybrid_sql("LIMIT 2"))
        .expect("select in txn");
    let engine::sql::SqlOutcome::Query(q) = outcome else {
        panic!("expected query");
    };
    assert_eq!(q.rows.len(), 2);
    assert!(rev.rerank_stats().applied >= 4);
    rev.execute_sql_in_txn(&ctx(), &mut session, &mut txn, "ROLLBACK")
        .expect("rollback");
}

/// RLS-13: 利用者の行ポリシーで隠れた行は、リランカーへ渡る候補にも結果にも現れない。
#[test]
fn search15_user_row_policy_hides_rows_from_the_reranker() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (core, _g) = setup(
        "search15-policy",
        Some(settings(
            RerankMethod::Custom(Arc::new(Recording { seen: seen.clone() })),
            50,
        )),
    );
    let mut session = engine::sql::mode::SessionState::default();
    session.allow_ddl();
    for ddl in [
        "ALTER TABLE docs ENABLE ROW LEVEL SECURITY",
        "ALTER TABLE docs FORCE ROW LEVEL SECURITY",
        "CREATE POLICY hide_filler ON docs AS PERMISSIVE FOR ALL USING (body <> 'zzz zzz zzz')",
    ] {
        core.execute_sql_in_session(&ctx(), &mut session, ddl)
            .unwrap_or_else(|e| panic!("{ddl}: {} {}", e.wire_code(), e.client_message()));
    }
    let got = run(&core, &hybrid_sql("LIMIT 8"));
    assert!(!got.rows.is_empty());
    assert!(
        ids(&got).iter().all(|id| (5..=8).contains(id)),
        "{:?}",
        ids(&got)
    );
    let seen = seen.lock().expect("lock");
    assert!(!seen.is_empty());
    assert!(seen
        .iter()
        .all(|t| t != "zzz zzz zzz" && !t.contains("OTHERTENANT")));
}
// ---------------------------------------------------------------------------
// MMR リランカー（Issue #2143）。設計記録: `docs/design/rerank-wiring-policy.md` D3・D4・D6。
// ---------------------------------------------------------------------------

/// `setup` の自テナント 8 行の埋め込み（id 1〜8）。
const EMB: [[f32; 2]; 8] = [
    [1.0, 0.0],
    [0.95, 0.05],
    [0.9, 0.1],
    [0.85, 0.15],
    [0.5, 0.5],
    [0.4, 0.6],
    [0.3, 0.7],
    [0.2, 0.8],
];

fn emb_of(id: u64) -> [f32; 2] {
    EMB[usize::try_from(id).expect("small id") - 1]
}

fn cos(a: [f32; 2], b: [f32; 2]) -> f64 {
    let dot = f64::from(a[0] * b[0] + a[1] * b[1]);
    let na = f64::from(a[0] * a[0] + a[1] * a[1]).sqrt();
    let nb = f64::from(b[0] * b[0] + b[1] * b[1]).sqrt();
    dot / (na * nb)
}

fn mmr(lambda: f64) -> RerankMethod {
    RerankMethod::Mmr(MmrReranker::new(lambda).expect("valid lambda"))
}

const DENSE_COS: &str = "SELECT id FROM docs ORDER BY embedding <=> '[1.0,0.0]'";
const DENSE_L2: &str = "SELECT id FROM docs ORDER BY embedding <#> '[1.0,0.0]'";

/// 受け取った候補 id を溜め、入力順の先頭 `final_k` 件を返す（埋め込みを読む方式の記録用）。
struct RecordingIds {
    seen: Arc<Mutex<Vec<u64>>>,
}

impl Reranker for RecordingIds {
    fn rerank(
        &self,
        _q: &str,
        candidates: &[RerankCandidate<'_>],
        final_k: usize,
    ) -> Result<Vec<RerankedHit>, RerankError> {
        let mut seen = self.seen.lock().expect("lock");
        seen.extend(candidates.iter().map(|c| c.id));
        Ok(candidates
            .iter()
            .take(final_k)
            .map(|c| RerankedHit {
                id: c.id,
                score: c.fused_score,
            })
            .collect())
    }
}

#[test]
fn search15_mmr_lambda_one_matches_off_for_hybrid_and_dense() {
    let (off, _g1) = setup("search15-mmr1-off", None);
    for window in [3, 8, 50] {
        let (core, _g2) = setup(
            &format!("search15-mmr1-{window}"),
            Some(settings(mmr(1.0), window)),
        );
        let mut sqls = vec![
            hybrid_sql("LIMIT 2"),
            hybrid_sql("LIMIT 3 OFFSET 1"),
            hybrid_sql("LIMIT 8"),
        ];
        for base in [DENSE_COS, DENSE_L2] {
            for tail in ["LIMIT 2", "LIMIT 3 OFFSET 1", "LIMIT 8"] {
                sqls.push(format!("{base} {tail}"));
            }
        }
        for sql in &sqls {
            assert_eq!(run(&off, sql), run(&core, sql), "window={window} {sql}");
        }
        let s = core.rerank_stats();
        assert_eq!(s.applied, sqls.len() as u64, "window={window}");
        assert_eq!(s.degraded_total(), 0);
        assert_eq!(s.not_applicable, 0);
    }
}

#[test]
fn search15_mmr_lambda_zero_promotes_diverse_rows_in_hybrid() {
    let (off, _g1) = setup("search15-mmr0-off", None);
    let (core, _g2) = setup("search15-mmr0", Some(settings(mmr(0.0), 8)));
    let window = ids(&run(&off, &hybrid_sql("LIMIT 8")));
    let got = ids(&run(&core, &hybrid_sql("LIMIT 2")));
    assert_eq!(got.len(), 2);
    // 1 件目は常に off の先頭。2 件目は 1 件目と最も似ていない窓内の行。
    assert_eq!(got[0], window[0]);
    let first = emb_of(got[0]);
    let expected = window[1..]
        .iter()
        .copied()
        .min_by(|a, b| {
            cos(first, emb_of(*a))
                .total_cmp(&cos(first, emb_of(*b)))
                .then(a.cmp(b))
        })
        .expect("window has rows");
    assert_eq!(got[1], expected);
    assert_eq!(core.rerank_stats().applied, 1);
}

#[test]
fn search15_mmr_applies_to_dense_only_and_keeps_distances() {
    let (off, _g1) = setup("search15-mmrd-off", None);
    let (core, _g2) = setup("search15-mmrd", Some(settings(mmr(0.0), 8)));
    for base in [DENSE_COS, DENSE_L2] {
        let sql = format!("{base} LIMIT 2");
        let off_rows = run(&off, &format!("{base} LIMIT 8"));
        let all = ids(&off_rows);
        let got = run(&core, &sql);
        let g = ids(&got);
        assert_eq!(g[0], all[0], "{base}");
        let first = emb_of(g[0]);
        let expected = all[1..]
            .iter()
            .copied()
            .min_by(|a, b| {
                cos(first, emb_of(*a))
                    .total_cmp(&cos(first, emb_of(*b)))
                    .then(a.cmp(b))
            })
            .expect("window has rows");
        assert_eq!(g[1], expected, "{base}");
        // 返却スコア（距離）は元の距離のまま: off の同じ行と一致する。
        for row in &got.rows {
            let same = off_rows
                .rows
                .iter()
                .find(|o| o.id == row.id)
                .expect("row in off result");
            assert_eq!(same, row, "{base}");
        }
    }
    let s = core.rerank_stats();
    assert_eq!(s.applied, 2);
    assert_eq!(s.degraded_total(), 0);
}

#[test]
fn search15_mmr_over_budget_degrades_to_off_without_widening() {
    let (off, _g1) = setup("search15-budget-off", None);
    // 10000 × min(200, 10000) = 2,000,000 > 1,000,000。
    let (core, _g2) = setup("search15-budget", Some(settings(mmr(0.0), 10_000)));
    let sqls = [hybrid_sql("LIMIT 200"), format!("{DENSE_COS} LIMIT 200")];
    for sql in &sqls {
        assert_eq!(run(&off, sql), run(&core, sql), "{sql}");
    }
    let s = core.rerank_stats();
    assert_eq!(s.degraded_mmr_budget, 2);
    assert_eq!(s.applied, 0);
    assert_eq!(s.not_applicable, 0);
    // 予算内（LIMIT 100: 10000 × 100 = 1,000,000）は適用される。
    let _ = run(&core, &format!("{DENSE_COS} LIMIT 100"));
    assert_eq!(core.rerank_stats().applied, 1);
}

#[test]
fn search15_dense_contract_violation_refetches_the_off_result() {
    let (off, _g) = setup("search15-drefetch-off", None);
    for (name, bad) in [
        ("foreign", Bad::ForeignId),
        ("missing", Bad::Missing),
        ("oversized", Bad::Oversized),
    ] {
        let (core, _g2) = setup(
            &format!("search15-drefetch-{name}"),
            Some(settings(RerankMethod::CustomEmbedding(Arc::new(bad)), 8)),
        );
        for tail in ["LIMIT 3", "LIMIT 2 OFFSET 1"] {
            for base in [DENSE_COS, DENSE_L2] {
                let sql = format!("{base} {tail}");
                assert_eq!(run(&off, &sql), run(&core, &sql), "{name} {sql}");
            }
        }
        let s = core.rerank_stats();
        assert_eq!(s.applied, 0, "{name}");
        assert_eq!(s.degraded_contract_violation, 4, "{name}");
        assert_eq!(s.not_applicable, 0, "{name}");
    }
}

#[test]
fn search15_mmr_is_not_applied_in_precision_mode() {
    let (off, _g1) = setup("search15-mmrp-off", None);
    let (core, _g2) = setup("search15-mmrp", Some(settings(mmr(0.0), 8)));
    let sql = hybrid_sql("LIMIT 3 USING MODE 'precision'");
    assert_eq!(run(&off, &sql), run(&core, &sql));
    let s = core.rerank_stats();
    assert_eq!(s.not_applicable, 1);
    assert_eq!(s.applied, 0);
}

#[test]
fn search15_embedding_reranker_only_sees_own_tenant_visible_rows() {
    for dense in [false, true] {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (core, _g) = setup(
            &format!("search15-mmr-rls-{dense}"),
            Some(settings(
                RerankMethod::CustomEmbedding(Arc::new(RecordingIds { seen: seen.clone() })),
                50,
            )),
        );
        let sql = if dense {
            format!("{DENSE_COS} LIMIT 3")
        } else {
            hybrid_sql("LIMIT 3")
        };
        let got = run(&core, &sql);
        assert!(ids(&got).iter().all(|id| (1..=8).contains(id)));
        let seen = seen.lock().expect("lock");
        assert!(!seen.is_empty());
        // 候補 id はアリーナのスロット番号。アリーナは自テナントの可視 8 行だけなので 0..8 に収まり、
        // 別テナントの強一致 3 行（スロット 8 以降になるはずの行）は候補に現れない。
        assert!(seen.iter().all(|slot| *slot < 8), "{seen:?}");
        assert!(seen.len() <= 8, "{seen:?}");
    }
}

#[test]
fn search15_dense_text_rerankers_stay_not_applicable() {
    // 字句一致系は密のみに適用しない（ADR D3）。MMR の追加で変わらないことを固定する。
    let (off, _g1) = setup("search15-dtext-off", None);
    let (core, _g2) = setup(
        "search15-dtext",
        Some(settings(RerankMethod::Custom(Arc::new(Reverse)), 8)),
    );
    let sql = format!("{DENSE_COS} LIMIT 3");
    assert_eq!(run(&off, &sql), run(&core, &sql));
    assert_eq!(core.rerank_stats().not_applicable, 1);
}

// ---------- Issue #2144: クロスエンコーダ方式（スタブバックエンドで決定的に検証） ----------

/// passage の長さをスコアにする決定的スタブ。受け取ったテキストを記録する。
struct StubBackend {
    seen: Arc<Mutex<Vec<String>>>,
    fail: bool,
}

impl CrossEncoderBackend for StubBackend {
    fn score_pairs(&self, _q: &str, passages: &[&str]) -> Result<Vec<f64>, CrossEncoderError> {
        if self.fail {
            return Err(CrossEncoderError::Backend("stub failure".to_string()));
        }
        self.seen
            .lock()
            .expect("lock")
            .extend(passages.iter().map(|p| p.to_string()));
        Ok(passages.iter().map(|p| p.len() as f64).collect())
    }

    fn max_seq_len(&self) -> usize {
        512
    }
}

fn stub_method(
    seen: &Arc<Mutex<Vec<String>>>,
    fail: bool,
    timeout: Option<std::time::Duration>,
) -> RerankMethod {
    let cfg = CrossEncoderConfig::new(4, 8, 512).expect("cfg");
    let mut r = CrossEncoderReranker::new(
        StubBackend {
            seen: seen.clone(),
            fail,
        },
        cfg,
    )
    .expect("reranker");
    if let Some(t) = timeout {
        r = r.with_timeout(t);
    }
    RerankMethod::cross_encoder(r)
}

#[test]
fn search15_cross_encoder_applies_with_stub_and_sees_only_own_tenant() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (core, _g) = setup(
        "search15-ce-apply",
        Some(settings(stub_method(&seen, false, None), 8)),
    );
    let got = run(&core, &hybrid_sql("LIMIT 3"));
    // 最長テキスト（id 8）が先頭に昇格する。
    assert_eq!(ids(&got).first(), Some(&8));
    assert_eq!(core.rerank_stats().applied, 1);
    let seen = seen.lock().expect("lock");
    assert!(!seen.is_empty());
    assert!(seen.iter().all(|t| !t.contains("OTHERTENANT")));
}

#[test]
fn search15_cross_encoder_timeout_degrades_to_off_result() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (off, _g1) = setup("search15-ce-to-off", None);
    let (core, _g2) = setup(
        "search15-ce-to",
        Some(settings(
            stub_method(&seen, false, Some(std::time::Duration::ZERO)),
            8,
        )),
    );
    let sql = hybrid_sql("LIMIT 3");
    assert_eq!(run(&off, &sql), run(&core, &sql));
    let s = core.rerank_stats();
    assert_eq!(s.applied, 0);
    assert_eq!(s.degraded_timeout, 1);
}

#[test]
fn search15_cross_encoder_backend_failure_degrades_to_off_result() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (off, _g1) = setup("search15-ce-fail-off", None);
    let (core, _g2) = setup(
        "search15-ce-fail",
        Some(settings(stub_method(&seen, true, None), 8)),
    );
    let sql = hybrid_sql("LIMIT 3");
    assert_eq!(run(&off, &sql), run(&core, &sql));
    let s = core.rerank_stats();
    assert_eq!(s.applied, 0);
    assert_eq!(s.degraded_backend_error, 1);
}
