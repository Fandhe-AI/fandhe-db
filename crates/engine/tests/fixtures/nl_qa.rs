//! Issue #333（SEARCH-7 方式変更・クロスエンコーダリランカーの効果実測）専用の
//! 自然言語 QA 決定的 fixture。
//!
//! `crates/engine/tests/rerank_recall.rs` の合成トークン（`kw_NNNN`）コーパスは
//! 事前学習済みクロスエンコーダモデルの評価には不適（モデルが学習していない語彙）
//! なため、本ファイルは英語の自然言語文を決定的に合成する別系統の fixture を提供する。
//! `#[path = "fixtures/nl_qa.rs"] mod nl_qa;` で `tests/rerank_cross_encoder_recall.rs`
//! から共有する（`contrast_smoke.rs` の `#[path]` パターンを踏襲。`tests/` 直下では
//! ないため cargo は本ファイル単体を test target として扱わない）。
//!
//! `rerank_recall.rs::measure_rerank_recall`（`LexicalOverlapReranker` 専用に固定）は
//! 変更せず、本ファイルは `&dyn Reranker` を受け取る一般化版
//! （[`measure_recall_with_reranker`]）を独自に持つ（production コード無変更・
//! `rerank_recall.rs` の固定値・`recall.yml` の `make rerank-regression` 経路に
//! 影響させないための意図的な複製。`docs/design/rerank-recall-regression.md`
//! 「Issue #333」節参照）。
//!
//! 語彙設計: 各「潜在概念」（[`CONCEPTS`]）に 2〜3 種の表層変種（同義句・言い換え）を
//! 持たせ、文書は常に variant 0（主要な言い回し）で生成し、クエリは常に最後の
//! variant（別の言い回し）で生成する。これにより字句一致（`LexicalOverlapReranker`・
//! BM25 系）では拾いにくいが意味的には一致するクエリ・文書ペアを意図的に作り、
//! クロスエンコーダ（意味表現に基づく関連度推定）の効果が出うる構成にする
//! （正解判定は表層語ではなく潜在概念集合 [`Doc::keywords`] のみで行う点は
//! `rerank_recall.rs::Doc`/`hybrid_recall.rs::Doc` と同じ設計）。
//!
//! ## 文書テキスト生成方式（Issue #337 で再設計）
//!
//! 旧方式（Issue #333 時点）は「主要 2 概念のみを流暢な英文テンプレート
//! （[`DOC_TEMPLATES`]）へ埋め込み、3 件目以降の概念は文末へ単語＋ピリオドで
//! 機械的に追記する」構造だった。QA 生成（[`generate_nl_qa_set`]）は文書内で
//! 最も出現頻度の低い概念 2 件を正解条件に選ぶため、低頻度概念ほど 3 件目以降の
//! 追記スロットに落ちやすく、正解概念が非流暢な追記フレーズ側に偏っていた。
//! 事前学習済みクロスエンコーダ（MS MARCO 学習・流暢な自然文が前提）はこの
//! 非自然文を「関連性の低いノイズ」に近く扱ってしまい、字句一致リランカーより
//! 悪化する結果になった（詳細な原因分析は `docs/design/rerank-recall-regression.md`
//! 「Issue #333 追記」節参照。spec 本文ではなく本リポ独自の分析）。
//!
//! 新方式は文書テキストに現れる概念（dropout 後の `ordered_kw`。主要 2 件は
//! [`DOC_TEMPLATES`] の導入文、3 件目以降は [`CONCEPT_SENTENCE_TEMPLATES`]）の
//! すべてを完結した英文テンプレートへ 1 概念 1 文で埋め込み、単語の裸追記を
//! 全廃する。各文は「{ctx} という運用文脈の中で {kw} がどう振る舞うか」を説明する
//! MS MARCO passage に近い短い説明文で、導入文・3 件目以降の単独文のいずれも
//! 同じ流暢さの文体にする。これにより QA が選ぶ正解概念がどの位置（2 件目までか
//! 3 件目以降か）にあっても、テキストに現れる限り常に流暢な自然文中に現れ、
//! クロスエンコーダの学習分布（流暢な自然文ペアでの関連度判定）と整合する。
//! 3 件目以降の文テンプレート選択は、正解集合（[`Doc::keywords`]）を決める
//! キーワード抽選用の rng ストリームとは独立させている（[`generate_nl_corpus`]
//! 参照。テキスト生成方式の変更だけを独立変数にし、Issue #333 との比較可能性を
//! 保つための設計）。
//!
//! 字句ミスマッチ設計（文書は variant 0・クエリは末尾 variant）・決定的生成
//! （xorshift64* seed 固定）・疎/密チャネルの dropout・decoy 構造は変更しない
//! （比較可能性のため。Issue #337 実装計画「fixture 再設計の方針」参照）。

#![allow(dead_code)] // 一部の補助 API は harness 側の feature 分岐によって未使用になりうる。

use engine::hybrid::{hybrid_search, RrfConfig};
use engine::kernel::SearchInput;
use engine::parallel_search::ParallelSearchProvider;
use engine::rerank::{rerank_candidates, RerankCandidate, RerankConfig, Reranker};
use engine::sparse::SparseIndex;
use std::collections::{BTreeMap, BTreeSet};

// ---------- 決定的擬似乱数（xorshift64*。`rerank_recall.rs::Xorshift64` と同一実装。
// 外部乱数クレート不使用・依存最小方針） ----------

pub struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    pub fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_range(&mut self, n: usize) -> usize {
        assert!(n > 0, "next_range(0) は無効な呼び出し");
        (self.next_u64() % n as u64) as usize
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

fn build_zipf_cumulative_weights(n: usize) -> Vec<f64> {
    let mut acc = 0.0;
    let mut cumulative = Vec::with_capacity(n);
    for i in 0..n {
        acc += 1.0 / (i as f64 + 1.0);
        cumulative.push(acc);
    }
    cumulative
}

fn zipf_index(rng: &mut Xorshift64, cumulative_weights: &[f64]) -> usize {
    let total = *cumulative_weights.last().unwrap_or(&0.0);
    let r = rng.next_f64() * total;
    match cumulative_weights.iter().position(|&acc| r <= acc) {
        Some(i) => i,
        None => cumulative_weights.len().saturating_sub(1),
    }
}

fn one_hot_sum(vocab_size: usize, indices: impl IntoIterator<Item = usize>) -> Vec<f32> {
    let mut v = vec![0.0f32; vocab_size];
    for idx in indices {
        if let Some(slot) = v.get_mut(idx) {
            *slot = 1.0;
        }
    }
    v
}

// ---------- 潜在概念の語彙（各概念に 2〜3 種の自然言語表層変種） ----------

/// ソフトウェア・データベース・分散システム領域の一般的な英語概念を、同義句・
/// 言い換えの変種込みで固定した語彙（決定的・外部データセット不使用）。
/// `variant[0]` は文書生成、`variant[last]` はクエリ生成に使う
/// （モジュールドキュメント参照）。
const CONCEPTS: &[&[&str]] = &[
    &[
        "vector index",
        "embedding index",
        "similarity search structure",
    ],
    &[
        "row-level security",
        "tenant isolation",
        "per-tenant access control",
    ],
    &["write-ahead log", "durability log", "commit journal"],
    &["query planner", "execution planner", "statement optimizer"],
    &["connection pool", "session pool", "client connection cache"],
    &[
        "hybrid search",
        "combined lexical and vector search",
        "fused retrieval",
    ],
    &[
        "cache eviction",
        "cache replacement policy",
        "eviction strategy",
    ],
    &[
        "schema migration",
        "data model upgrade",
        "table structure change",
    ],
    &[
        "replication lag",
        "follower delay",
        "asynchronous copy delay",
    ],
    &[
        "garbage collection",
        "memory reclamation",
        "unused object cleanup",
    ],
    &[
        "load balancing",
        "traffic distribution",
        "request spreading",
    ],
    &["rate limiting", "throughput throttling", "request pacing"],
    &[
        "circuit breaker",
        "failure isolation switch",
        "fault containment mechanism",
    ],
    &["backpressure", "flow control", "producer throttling"],
    &[
        "idempotency key",
        "deduplication token",
        "retry safety identifier",
    ],
    &[
        "consistent hashing",
        "ring based partitioning",
        "hash ring distribution",
    ],
    &[
        "leader election",
        "coordinator selection",
        "primary node election",
    ],
    &[
        "quorum write",
        "majority acknowledgement write",
        "distributed consensus write",
    ],
    &[
        "snapshot isolation",
        "point in time read consistency",
        "versioned read view",
    ],
    &[
        "dead letter queue",
        "failed message queue",
        "poison message store",
    ],
    &[
        "horizontal scaling",
        "adding more nodes",
        "scale out capacity growth",
    ],
    &[
        "vertical scaling",
        "adding more resources per node",
        "scale up capacity growth",
    ],
    &[
        "observability",
        "system introspection",
        "runtime visibility",
    ],
    &[
        "distributed tracing",
        "cross service request tracking",
        "span based tracing",
    ],
    &[
        "service mesh",
        "sidecar network layer",
        "inter service communication fabric",
    ],
    &[
        "blue green deployment",
        "parallel environment rollout",
        "zero downtime release swap",
    ],
    &[
        "canary release",
        "gradual rollout",
        "staged traffic rollout",
    ],
    &[
        "feature flag",
        "runtime toggle",
        "conditional feature switch",
    ],
    &[
        "access token",
        "bearer credential",
        "short lived authorization token",
    ],
    &[
        "encryption at rest",
        "stored data encryption",
        "disk level encryption",
    ],
    &[
        "encryption in transit",
        "network traffic encryption",
        "wire level encryption",
    ],
    &["audit log", "activity trail", "compliance record"],
    &[
        "backup retention",
        "archive lifetime policy",
        "backup expiry rule",
    ],
    &[
        "disaster recovery",
        "failover recovery plan",
        "catastrophic outage recovery",
    ],
    &[
        "capacity planning",
        "resource forecasting",
        "future load estimation",
    ],
    &[
        "cost optimization",
        "spend reduction",
        "resource cost tuning",
    ],
    &["data partitioning", "sharding", "horizontal data split"],
    &["read replica", "secondary read node", "follower read copy"],
    &[
        "eventual consistency",
        "delayed convergence",
        "asynchronous consistency model",
    ],
    &[
        "strong consistency",
        "linearizable reads",
        "immediate consistency guarantee",
    ],
    &[
        "batch processing",
        "bulk job execution",
        "offline batch computation",
    ],
    &[
        "stream processing",
        "real time event handling",
        "continuous data processing",
    ],
    &[
        "schema validation",
        "input structure checking",
        "payload shape enforcement",
    ],
    &[
        "input sanitization",
        "untrusted input cleaning",
        "payload normalization",
    ],
    &[
        "dependency injection",
        "constructor based wiring",
        "inversion of control wiring",
    ],
    &[
        "retry with backoff",
        "exponential retry delay",
        "graduated retry policy",
    ],
    &["health check", "liveness probe", "readiness probe"],
    &[
        "graceful shutdown",
        "clean process termination",
        "orderly service stop",
    ],
    &[
        "connection timeout",
        "network wait limit",
        "socket deadline",
    ],
    &["thread pool", "worker pool", "execution pool"],
    &[
        "memory pressure",
        "heap exhaustion risk",
        "resource contention under load",
    ],
    &[
        "cold start latency",
        "initial invocation delay",
        "warmup penalty",
    ],
    &[
        "index rebuild",
        "index reconstruction",
        "search structure regeneration",
    ],
    &[
        "query rewriting",
        "statement transformation",
        "plan level query substitution",
    ],
    &[
        "access control list",
        "permission list",
        "authorization rule set",
    ],
    &[
        "multi tenancy",
        "shared infrastructure isolation",
        "tenant separated deployment",
    ],
    &[
        "data lineage",
        "provenance tracking",
        "origin and transformation history",
    ],
    &[
        "schema drift",
        "unexpected structure change",
        "silent shape divergence",
    ],
    &["log rotation", "log file cycling", "periodic log archiving"],
    &[
        "configuration drift",
        "unintended settings divergence",
        "environment inconsistency",
    ],
    &[
        "chaos engineering",
        "deliberate failure injection",
        "resilience testing practice",
    ],
];

const NUM_CONCEPTS: usize = CONCEPTS.len();

fn concept_form(concept_idx: usize, variant_idx: usize) -> &'static str {
    let variants = CONCEPTS[concept_idx];
    variants[variant_idx.min(variants.len() - 1)]
}

/// 文書生成に使う variant（常に variant 0 = 主要な言い回し）。
fn doc_form(concept_idx: usize) -> &'static str {
    concept_form(concept_idx, 0)
}

/// クエリ生成に使う variant（常に末尾 variant = 別の言い回し。字句一致では拾いにくい
/// が意味的には同じ表現を作るための意図的なずらし。モジュールドキュメント参照）。
fn query_form(concept_idx: usize) -> &'static str {
    let last = CONCEPTS[concept_idx].len() - 1;
    concept_form(concept_idx, last)
}

const CONTEXTS: [&str; 8] = [
    "a production cluster",
    "the query planner",
    "a distributed system",
    "a multi region deployment",
    "an internal platform team",
    "a customer facing service",
    "a nightly batch pipeline",
    "an on call runbook",
];

const DOC_TEMPLATES: [&str; 4] = [
    "This note explains how {a} relates to {b} inside {ctx}.",
    "A brief discussion of {a} and its effect on {b} during an incident in {ctx}.",
    "{ctx} documentation covers {a}, which often interacts with {b}.",
    "Engineers reviewing {a} should also consider {b} when operating {ctx}.",
];

const QUERY_TEMPLATES: [&str; 3] = [
    "How does {a} affect {b}?",
    "What is the relationship between {a} and {b}?",
    "Why does {a} sometimes impact {b}?",
];

/// 3 件目以降の概念（[`DOC_TEMPLATES`] の導入文に入らない残りの概念語）を、
/// 単語の裸追記ではなく完結した英文へ 1 概念 1 文で埋め込むためのテンプレート
/// （モジュールドキュメント「文書テキスト生成方式（Issue #337 で再設計）」参照）。
/// `{kw}` は当該概念の variant 0（[`doc_form`]）、`{ctx}` は文書全体で共有する
/// 運用文脈（[`CONTEXTS`]）に置換する。MS MARCO passage に近い、運用状況・
/// 因果関係を説明する短い文体で統一し、[`DOC_TEMPLATES`] による導入文と地続きの
/// 流暢さを保つ。
const CONCEPT_SENTENCE_TEMPLATES: [&str; 10] = [
    "Teams operating {ctx} frequently rely on {kw} to keep the system predictable under load.",
    "A common failure mode surfaced in {ctx} is tied to {kw}, which can quietly degrade reliability if it is ignored.",
    "When engineers investigate incidents in {ctx}, {kw} is usually one of the first areas they check.",
    "Documentation for {ctx} recommends monitoring {kw} closely, since problems there tend to cascade quickly.",
    "Understanding {kw} helps operators of {ctx} reason about trade-offs during peak traffic.",
    "In postmortems written for {ctx}, {kw} shows up repeatedly as either a root cause or a contributing factor.",
    "Onboarding material for {ctx} usually introduces {kw} early, since most later topics build on it.",
    "Capacity reviews for {ctx} routinely revisit {kw} to decide whether current limits still make sense.",
    "Changes to {ctx} are often evaluated by how they interact with {kw} in production.",
    "Runbooks for {ctx} describe how {kw} behaves during degraded conditions and how to recover from it.",
];

/// 合成コーパス 1 文書（`rerank_recall.rs::Doc` と同じ役割: `text`（疎チャネル）・
/// `vector`（密チャネル）はいずれも潜在概念集合 `keywords` の非完全な観測であり、
/// 正解判定そのものには使わない）。
pub struct Doc {
    pub id: u64,
    pub text: String,
    pub keywords: BTreeSet<usize>,
    pub vector: Vec<f32>,
}

/// QA セット 1 件（`rerank_recall.rs::QaCase` と同じ役割）。
pub struct QaCase {
    pub query_text: String,
    pub query_vector: Vec<f32>,
    pub correct: BTreeSet<u64>,
}

/// コーパス規模の上限ガード（`sparse.rs` の上限に対応。`rerank_recall.rs::
/// MAX_CORPUS_DOCS_GUARD` と同一値）。
pub const MAX_CORPUS_DOCS_GUARD: usize = 100_000;

const TEXT_KEYWORD_DROPOUT_PROB: f64 = 0.18;
const VECTOR_KEYWORD_DROPOUT_PROB: f64 = 0.18;
const VECTOR_DECOY_PROB: f64 = 0.12;

/// 決定的シード付き擬似乱数で自然言語コーパスと QA セットを生成する
/// （`rerank_recall.rs::generate_corpus` と同型のアルゴリズム。語彙・文生成のみ
/// 自然言語向けに差し替えている）。`vocab_size` は密ベクトルの次元数
/// （[`NUM_CONCEPTS`] 以上を要求。呼び出し側の不変条件違反は untrusted 入力経路では
/// ないため `assert!` で検出する）。
pub fn generate_nl_corpus(
    seed: u64,
    num_docs: usize,
    num_queries: usize,
) -> (Vec<Doc>, Vec<QaCase>) {
    assert!(
        num_docs <= MAX_CORPUS_DOCS_GUARD,
        "MAX_CORPUS_DOCS を超過してはならない"
    );
    let vocab_size = NUM_CONCEPTS;

    let mut rng = Xorshift64::new(seed);
    let zipf_weights = build_zipf_cumulative_weights(vocab_size);
    // 3 概念目以降の文テンプレート選択（[`CONCEPT_SENTENCE_TEMPLATES`]）専用の
    // 独立した rng ストリーム。`rng`（キーワード集合・dropout・vector・ctx/doc
    // テンプレート選択に使う主ストリーム）から分離することで、Issue #337 の文書
    // テキスト生成方式の変更が正解集合（`kw_set`・`inverted`。QA の `correct` は
    // ここから決まる）の抽選そのものへ波及しない不変条件を保つ（テキスト生成の
    // 変更だけを独立変数にするための意図的な設計。Issue #333 との比較可能性）。
    let mut sentence_rng = Xorshift64::new(seed ^ 0x9E37_79B9_7F4A_7C15);

    let mut docs = Vec::with_capacity(num_docs);
    let mut inverted: BTreeMap<usize, Vec<u64>> = BTreeMap::new();

    for doc_id in 0..num_docs as u64 {
        let num_keywords = 2 + rng.next_range(3); // 2..=4（概念語彙が有限のため rerank_recall.rs より小さめ）
        let mut kw_set: BTreeSet<usize> = BTreeSet::new();
        while kw_set.len() < num_keywords {
            kw_set.insert(zipf_index(&mut rng, &zipf_weights));
        }

        for &kw_idx in &kw_set {
            inverted.entry(kw_idx).or_default().push(doc_id);
        }

        let mut ordered_kw: Vec<usize> = kw_set
            .iter()
            .copied()
            .filter(|_| rng.next_f64() >= TEXT_KEYWORD_DROPOUT_PROB)
            .collect();
        if ordered_kw.len() < 2 {
            ordered_kw = kw_set.iter().copied().take(2).collect();
        }
        if ordered_kw.len() < 2 {
            // 概念数が極端に少ないコーパス（テスト専用の極小規模）向けの保険。
            ordered_kw = vec![ordered_kw[0], ordered_kw[0]];
        }
        let ctx = CONTEXTS[rng.next_range(CONTEXTS.len())];
        let template = DOC_TEMPLATES[rng.next_range(DOC_TEMPLATES.len())];
        let mut text = template
            .replace("{a}", doc_form(ordered_kw[0]))
            .replace("{b}", doc_form(ordered_kw[1]))
            .replace("{ctx}", ctx);
        // 3 概念目以降も [`CONCEPT_SENTENCE_TEMPLATES`] で 1 概念 1 文の完結した
        // 英文として追記する（旧方式の単語＋ピリオド追記を廃止。Issue #337）。
        // テンプレート選択は概念ごとに rng で決定的に選び、同一概念でも文書間で
        // 文体が単調にならないようにする。
        for &extra in ordered_kw.iter().skip(2) {
            let sentence_template = CONCEPT_SENTENCE_TEMPLATES
                [sentence_rng.next_range(CONCEPT_SENTENCE_TEMPLATES.len())];
            let sentence = sentence_template
                .replace("{kw}", doc_form(extra))
                .replace("{ctx}", ctx);
            text.push(' ');
            text.push_str(&sentence);
        }

        let mut vector_keywords: Vec<usize> = kw_set
            .iter()
            .copied()
            .filter(|_| rng.next_f64() >= VECTOR_KEYWORD_DROPOUT_PROB)
            .collect();
        if rng.next_f64() < VECTOR_DECOY_PROB {
            let decoy = zipf_index(&mut rng, &zipf_weights);
            if !kw_set.contains(&decoy) {
                vector_keywords.push(decoy);
            }
        }
        let vector = one_hot_sum(vocab_size, vector_keywords.iter().copied());

        docs.push(Doc {
            id: doc_id,
            text,
            keywords: kw_set,
            vector,
        });
    }

    let qa = generate_nl_qa_set(&mut rng, &docs, &inverted, vocab_size, num_queries);
    (docs, qa)
}

/// 各文書から最も出現頻度の低い概念 2 件（AND 組み合わせ）を選び、正解集合が
/// コーパス全体に対して十分に絞り込まれたクエリを構成する
/// （`rerank_recall.rs::generate_qa_set` と同型。クエリ文の言い回しは
/// [`query_form`]＝文書生成に使った variant 0 とは別の variant を使う）。
fn generate_nl_qa_set(
    rng: &mut Xorshift64,
    docs: &[Doc],
    inverted: &BTreeMap<usize, Vec<u64>>,
    vocab_size: usize,
    num_queries: usize,
) -> Vec<QaCase> {
    let mut order: Vec<usize> = (0..docs.len()).collect();
    for i in (1..order.len()).rev() {
        let j = rng.next_range(i + 1);
        order.swap(i, j);
    }

    let mut qa = Vec::with_capacity(num_queries);
    let mut seen_pairs: BTreeSet<(usize, usize)> = BTreeSet::new();
    for &doc_idx in &order {
        if qa.len() >= num_queries {
            break;
        }
        let doc = &docs[doc_idx];
        if doc.keywords.len() < 2 {
            continue;
        }
        let mut kws: Vec<usize> = doc.keywords.iter().copied().collect();
        kws.sort_by_key(|k| inverted.get(k).map_or(0, Vec::len));
        let (a, b) = (kws[0], kws[1]);

        let pair = (a.min(b), a.max(b));
        if !seen_pairs.insert(pair) {
            continue;
        }

        let set_a: BTreeSet<u64> = inverted.get(&a).into_iter().flatten().copied().collect();
        let set_b: BTreeSet<u64> = inverted.get(&b).into_iter().flatten().copied().collect();
        let correct: BTreeSet<u64> = set_a.intersection(&set_b).copied().collect();
        if correct.is_empty() {
            continue;
        }

        let template = QUERY_TEMPLATES[rng.next_range(QUERY_TEMPLATES.len())];
        let query_text = template
            .replace("{a}", query_form(a))
            .replace("{b}", query_form(b));

        qa.push(QaCase {
            query_text,
            query_vector: one_hot_sum(vocab_size, [a, b]),
            correct,
        });
    }

    qa
}

/// コーパスが `sparse.rs` の各上限に収まることを検証する
/// （`rerank_recall.rs::assert_corpus_within_limits` と同一実装）。
pub fn assert_corpus_within_limits(docs: &[Doc]) {
    assert!(!docs.is_empty());
    assert!(
        docs.len() <= MAX_CORPUS_DOCS_GUARD,
        "MAX_CORPUS_DOCS を超過してはならない"
    );
    let mut total_bytes: usize = 0;
    for doc in docs {
        assert!(
            doc.text.len() <= 1024 * 1024,
            "MAX_DOC_BYTES を超過してはならない"
        );
        total_bytes += doc.text.len();
    }
    assert!(
        total_bytes <= 64 * 1024 * 1024,
        "MAX_CORPUS_BYTES を超過してはならない"
    );
}

// ---------- Recall 測定ヘルパ（`&dyn Reranker` へ一般化。`rerank_recall.rs::
// measure_rerank_recall`〔`LexicalOverlapReranker` 専用〕とは意図的な複製で production
// コード・既存ゲートへは影響させない） ----------

/// [`measure_recall_with_reranker`] の結果（`rerank_recall.rs::RerankRecallResult` と
/// 同じフィールド構成・同じ理由付け）。
pub struct RerankRecallResult {
    pub total_correct: usize,
    pub baseline_hits20: usize,
    pub after_hits20: usize,
    pub pool_hits100: usize,
    pub pool_hits200: usize,
    pub pool_ceiling_hits20: usize,
    pub ceil20: usize,
    pub ceil100: usize,
    pub ceil200: usize,
}

impl RerankRecallResult {
    pub fn baseline_recall20(&self) -> f64 {
        self.baseline_hits20 as f64 / self.ceil20 as f64
    }

    pub fn after_recall20(&self) -> f64 {
        self.after_hits20 as f64 / self.ceil20 as f64
    }

    pub fn pool_recall100(&self) -> f64 {
        self.pool_hits100 as f64 / self.ceil100 as f64
    }

    pub fn pool_recall200(&self) -> f64 {
        self.pool_hits200 as f64 / self.ceil200 as f64
    }

    pub fn improvement_headroom(&self) -> usize {
        self.pool_ceiling_hits20
            .saturating_sub(self.baseline_hits20)
    }

    /// SEARCH-7（fandhe-db-spec#7）の相対基準と同じ定義（`rerank_recall.rs::
    /// RerankRecallResult::improvement_ratio` と同一ロジック）。改善余地が
    /// コーパス全体理論上限 `ceil20` の 1% 未満なら `None`（分母 0 近傍の不安定化を
    /// 避ける fail-closed 対策）。
    pub fn improvement_ratio(&self) -> Option<f64> {
        let headroom = self.improvement_headroom();
        if (headroom as f64) < 0.01 * self.ceil20 as f64 {
            return None;
        }
        let improved = self.after_hits20.saturating_sub(self.baseline_hits20);
        Some(improved as f64 / headroom as f64)
    }
}

/// `hybrid_search`（`RrfConfig::default()`）で候補プールを取得し、baseline（先頭 20
/// 件）・`reranker` による after・補助計測（プール Recall@100/@200）を測定する
/// （`rerank_recall.rs::measure_rerank_recall` と同型。`reranker` を差し替え可能にした
/// 一般化版であり、`LexicalOverlapReranker`・`CrossEncoderReranker` のいずれでも
/// 同じ fixture・同じ手順で比較できる）。
pub fn measure_recall_with_reranker(
    docs: &[Doc],
    qa: &[QaCase],
    reranker: &dyn Reranker,
) -> RerankRecallResult {
    let refs: Vec<(u64, &str)> = docs.iter().map(|d| (d.id, d.text.as_str())).collect();
    let sparse_index = SparseIndex::build(&refs).expect("sparse index build ok");

    let ids: Vec<u64> = docs.iter().map(|d| d.id).collect();
    let dim = docs.first().map_or(0, |d| d.vector.len());
    let vectors: Vec<f32> = docs.iter().flat_map(|d| d.vector.iter().copied()).collect();
    let provider = ParallelSearchProvider;
    let hybrid_cfg = RrfConfig::default();
    let rerank_cfg = RerankConfig::default();
    let doc_text_by_id: BTreeMap<u64, &str> =
        docs.iter().map(|d| (d.id, d.text.as_str())).collect();

    let mut total_correct = 0usize;
    let mut baseline_hits20 = 0usize;
    let mut after_hits20 = 0usize;
    let mut pool_hits100 = 0usize;
    let mut pool_hits200 = 0usize;
    let mut pool_ceiling_hits20 = 0usize;
    let mut ceil20 = 0usize;
    let mut ceil100 = 0usize;
    let mut ceil200 = 0usize;

    for case in qa {
        total_correct += case.correct.len();
        ceil20 += case.correct.len().min(20);
        ceil100 += case.correct.len().min(100);
        ceil200 += case.correct.len().min(200);

        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: dim as u32,
            query: &case.query_vector,
            k: 100,
        };
        let pool = hybrid_search(
            &provider,
            input,
            &sparse_index,
            &case.query_text,
            hybrid_cfg.pool_depth(),
            &hybrid_cfg,
        )
        .expect("hybrid_search ok");

        baseline_hits20 += pool
            .iter()
            .take(20)
            .filter(|h| case.correct.contains(&h.id))
            .count();

        pool_hits100 += pool
            .iter()
            .take(100)
            .filter(|h| case.correct.contains(&h.id))
            .count();
        let pool_hits_this_case = pool.iter().filter(|h| case.correct.contains(&h.id)).count();
        pool_hits200 += pool_hits_this_case;
        pool_ceiling_hits20 += pool_hits_this_case.min(20);

        let candidates: Vec<RerankCandidate<'_>> = pool
            .iter()
            .map(|h| RerankCandidate {
                id: h.id,
                fused_score: h.score,
                text: doc_text_by_id.get(&h.id).copied().unwrap_or(""),
            })
            .collect();
        let reranked = rerank_candidates(reranker, &case.query_text, &candidates, &rerank_cfg)
            .expect("rerank_candidates ok");
        after_hits20 += reranked
            .iter()
            .filter(|h| case.correct.contains(&h.id))
            .count();
    }

    RerankRecallResult {
        total_correct,
        baseline_hits20,
        after_hits20,
        pool_hits100,
        pool_hits200,
        pool_ceiling_hits20,
        ceil20,
        ceil100,
        ceil200,
    }
}
