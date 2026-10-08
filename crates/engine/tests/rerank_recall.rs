//! TASK-108（対象ビヘイビア: SEARCH-7。ポインタ: `docs/spec/05-tasks.md` TASK-108・
//! `docs/spec/04-behavior/search.md`）。リランキング層（TASK-107・`crates/engine/src/
//! rerank.rs`）が最終 Recall@20 を改善するかどうかを実測する効果測定回帰テスト。
//!
//! `crates/engine/tests/hybrid_recall.rs`（TASK-104）の決定的合成コーパス生成
//! （自前 xorshift64*・外部クレート不使用）・QA セット・[`RecallResult`] 相当
//! （理論上限 `ceil` を分母とする到達率）・2 層構成（PR CI と閾値ゲートの分離）を
//! そのまま複製・踏襲する（`cjk_tokenizer_impact.rs` → `hybrid_recall.rs` と同じ
//! 「複製・踏襲」方式）。production コード（`crates/engine/src/`）は原則変更しない
//! 契約だが、Issue #310 対応（[`LexicalOverlapReranker`] の既定重みを字句一致優先
//! から fused 優位へ変更。理由: 等重み既定では字句一致優先による正解脱落が実測され、
//! `after_hits20 >= baseline_hits20` の非劣化アサーションが red だった）・
//! Issue #330 対応（[`LexicalOverlapReranker`] の `rank_lexical` 同点規約を位置順位
//! から `rank_fused` と同じ GroupEnd へ統一。理由: 大規模段 improvement@20 が字句
//! 信号の構造的上限に対して未達だった）はオーナー判断による例外で、本ファイルの
//! 固定値アサーションも実測値へ合わせて更新している。
//!
//! **比較対象**（いずれも production API のみを使用。BM25/RRF/リランキングの
//! 再実装は行わない）:
//! - baseline（リランキングなし）: `hybrid::hybrid_search`（`RrfConfig::default()`・
//!   pool_depth 200）で候補プール 200 件を 1 回取得し、その先頭 20 件の Recall@20
//! - after（リランキングあり）: 同じ候補プール 200 件を [`RerankCandidate`] へ変換し、
//!   `rerank_candidates(&LexicalOverlapReranker::default(), …, final_k=20)`
//!   （`RerankConfig::default()`）の出力の Recall@20
//! - 補助計測（原因分析用）: 候補プール自体の Recall@100・Recall@200（プール深さの
//!   活用状況を見る。プールの中に正解があるのにリランキングが上位 20 件へ引き上げ
//!   られていないのか、そもそもプールに入っていないのかを切り分ける）
//!
//! 2 層構成（PR CI と閾値ゲートの分離。`hybrid_recall.rs` と同方針）:
//! - 層 A（`#[test]`・常時 `cargo test` 対象）: baseline/after の hits20 と改善量を
//!   固定値アサーションで回帰トラッキングし、「after が baseline を下回らない」
//!   ことも独立にアサートする。spec の数値基準は使わないため public 資産に閾値を
//!   持ち込まない（`.claude/rules/spec-confidentiality.md`）
//! - 層 B（`#[ignore]`・`make rerank-regression` 経由）: spec 由来の Recall 下限
//!   （`RERANK_RECALL_MIN_R20_LARGE`＝リランキング後の最終 Recall@20 の絶対下限）
//!   と実測値を比較し、あわせて非劣化（`after_hits20 >= baseline_hits20`）を
//!   ブロッキング条件とする閾値ゲート。改善幅（[`RerankRecallResult::
//!   improvement_ratio`]＝候補プール上限に対する相対比率
//!   `(after − baseline) / (pool_ceiling_hits20 − baseline_hits20)`）は
//!   SEARCH-7 改訂（2026-08-31・fandhe-db-spec#8）により実コーパス評価まで
//!   informational（非ブロッキング）へ降格した——Issue #330・#333・#337 で
//!   字句一致方式・クロスエンコーダ方式の 2 方式 × 2 fixture の全実測が
//!   improvement_ratio 0.222・0 で下限未達であり、原因が合成 fixture 側の
//!   構造要因（キーワード抽選による正解集合・表層語の偶発重複・人工的語彙密度。
//!   `docs/design/rerank-recall-regression.md`「SEARCH-7 改訂（2026-08-31）」節
//!   参照）と判明したため。`after_recall@20` 等の閾値近傍の実測値は
//!   `RECALL_VERBOSE=1`（`GITHUB_ACTIONS` 下では拒否。Issue #303）の opt-in 時に
//!   限りログ出力する一方、informational な `improvement_ratio`（判定に使わない
//!   実測値。閾値そのものは含まない）は `verbose`／`GITHUB_ACTIONS` の有無に
//!   かかわらず常時ログ出力する（codex-review P1・PR #340。通常 CI 実行から
//!   実測値が一切記録できなくなっていた抜けを塞ぐ）。`RERANK_RECALL_
//!   REQUIRE_THRESHOLDS=1`（`recall.yml` の Run step からのみ注入）で
//!   `RERANK_RECALL_MIN_R20_LARGE` の未設定を fail-closed にする strict モードを
//!   持つ（`hybrid_recall.rs::resolve_gate_threshold` と同型。`after_recall@20` の
//!   ログには対象名と pass/fail のみを出力する〔`resolve_verbose`・
//!   `verbose_requested_from_env`・`render_gate_line`〕）
//!
//! 既知の制約（スコープ外・フォローアップ）:
//! - 同梱リランカー（[`LexicalOverlapReranker`]）は方式確定までの暫定実装
//!   （クロスエンコーダ等の本命方式は依存承認制・オーナー選定待ち）であり、本測定は
//!   暫定構成の効果測定にとどまる（`docs/design/rerank-recall-regression.md` 参照）
//! - 合成コーパスによる暫定測定であり、実コーパスでの評価は未了
//!   （`hybrid_recall.rs` と同種の制約）
//! - `VectorCore` trait への統合・SQL 表層統合は後続タスクの管轄

use engine::hybrid::{hybrid_search, RrfConfig};
use engine::kernel::SearchInput;
use engine::parallel_search::ParallelSearchProvider;
use engine::rerank::{rerank_candidates, LexicalOverlapReranker, RerankCandidate, RerankConfig};
use engine::sparse::SparseIndex;
use std::collections::{BTreeMap, BTreeSet};

// `recall_engine` fixture（下記）が `super::temp_db` を参照するため、取り込み側
// である本ファイルでクレートルートに 1 回だけ宣言する
// （`tests/hnsw_cache.rs` 等と同じ取り込み方式。`recall_engine.rs` 自身が
// `mod temp_db` を宣言すると同一物理ファイルの二重 `mod` になり
// `clippy::duplicate_mod` に抵触するため、宣言はここへ一本化する）。
#[path = "../src/test_util/temp_db.rs"]
mod temp_db;

// ANN opt-in（Issue #412）検索エンジン切替 fixture。既定（`RecallEngine::
// BruteForce`）では本モジュールの型を一切構築せず、以下の層 A・既存層 B の
// 実測値・固定値アサーションに影響を与えない。
#[path = "fixtures/recall_engine.rs"]
mod recall_engine;
use recall_engine::{AnnStats, RecallEngine, SqlHybridFixture};

// ---------- 決定的擬似乱数（xorshift64*。外部クレート不使用。hybrid_recall.rs と同一実装） ----------

/// コーパス生成専用の決定的擬似乱数生成器（`hybrid_recall.rs::Xorshift64` と同一実装。
/// テスト再現性のため外部の乱数クレートは使わず、xorshift64* をこのファイル内に
/// 自前実装する。依存最小方針）。
struct Xorshift64 {
    state: u64,
}

impl Xorshift64 {
    fn new(seed: u64) -> Self {
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
        // n == 0 は呼び出し側（コーパス生成ロジック自身）の不変条件違反であり、
        // untrusted 入力経路ではないため coding-rust.md の unwrap 禁止は適用されない。
        assert!(n > 0, "next_range(0) は無効な呼び出し");
        (self.next_u64() % n as u64) as usize
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

// ---------- 合成コーパスの語彙（トピック = キーワード。密ベクトルのトピック相関に利用） ----------

/// トピック `idx` に対応するキーワードトークンを決定的に合成する
/// （`hybrid_recall.rs::topic_token` と同一実装）。
fn topic_token(idx: usize) -> String {
    format!("kw_{idx:04}")
}

/// 文脈語プール（内容語の間に挟む機能語。BM25 の相対比較には影響しない飾り）。
const FILLER_WORDS: [&str; 10] = [
    "the", "a", "an", "of", "for", "and", "with", "in", "on", "note",
];

/// 合成コーパス 1 文書（`hybrid_recall.rs::Doc` と同一の役割・構造）。`keywords` は
/// 生成時に既知の「正解（潜在）トピック集合」で、QA セットの正解文書判定にのみ使う。
/// `text`（疎チャネル）・`vector`（密チャネル）はいずれもこの潜在集合の非完全な観測
/// （lossy view）であり、正解判定そのものには使わない。
struct Doc {
    id: u64,
    text: String,
    keywords: BTreeSet<usize>,
    vector: Vec<f32>,
}

/// QA セット 1 件（`hybrid_recall.rs::QaCase` と同一の役割）。
struct QaCase {
    query_text: String,
    query_vector: Vec<f32>,
    correct: BTreeSet<u64>,
}

/// トピック `idx` を「密ベクトル空間の次元 `idx`」へ直接対応させる one-hot 構成
/// （`hybrid_recall.rs::one_hot_sum` と同一実装。密ベクトルは基底ベクトルの和として
/// 合成し、与えた次元集合との共通部分数がそのまま内積のスコアになる決定的な信号を
/// 作る）。
fn one_hot_sum(vocab_size: usize, indices: impl IntoIterator<Item = usize>) -> Vec<f32> {
    let mut v = vec![0.0f32; vocab_size];
    for idx in indices {
        if let Some(slot) = v.get_mut(idx) {
            *slot = 1.0;
        }
    }
    v
}

/// Zipf 近似分布（重み `1/(i+1)`）。`hybrid_recall.rs::build_zipf_cumulative_weights`/
/// `zipf_index` と同一実装。
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

/// コーパス規模の上限ガード（`sparse.rs` の `MAX_CORPUS_DOCS`/`MAX_DOC_BYTES`/
/// `MAX_CORPUS_BYTES` に対応。`hybrid_recall.rs::MAX_CORPUS_DOCS_GUARD` と同一）。
/// 環境変数からサイズを受け取らず、テスト内定数のみで規模を決める
/// （coding-rust.md「untrusted 入力の扱い」）。
const MAX_CORPUS_DOCS_GUARD: usize = 100_000;

/// fixture パラメータ（`hybrid_recall.rs` と同一値。spec 由来の数値基準ではなく、
/// Recall を 1.0 未満の現実的な分布にするために実験的に選んだ確率）。
const TEXT_KEYWORD_DROPOUT_PROB: f64 = 0.18;
const VECTOR_KEYWORD_DROPOUT_PROB: f64 = 0.18;
const VECTOR_DECOY_PROB: f64 = 0.12;

/// 決定的シード付き擬似乱数でトピック相関コーパスと QA セットを生成する
/// （`hybrid_recall.rs::generate_corpus` と同一実装。`direct` カテゴリ相当:
/// 最も出現頻度の低いキーワード 2 語の AND 組み合わせ）。
fn generate_corpus(
    seed: u64,
    num_docs: usize,
    num_queries: usize,
    vocab_size: usize,
) -> (Vec<Doc>, Vec<QaCase>) {
    assert!(
        num_docs <= MAX_CORPUS_DOCS_GUARD,
        "MAX_CORPUS_DOCS を超過してはならない"
    );

    let mut rng = Xorshift64::new(seed);
    let zipf_weights = build_zipf_cumulative_weights(vocab_size);

    let mut docs = Vec::with_capacity(num_docs);
    let mut inverted: BTreeMap<usize, Vec<u64>> = BTreeMap::new();

    for doc_id in 0..num_docs as u64 {
        let num_keywords = 3 + rng.next_range(4); // 3..=6
        let mut kw_set: BTreeSet<usize> = BTreeSet::new();
        while kw_set.len() < num_keywords {
            kw_set.insert(zipf_index(&mut rng, &zipf_weights));
        }

        for &kw_idx in &kw_set {
            inverted.entry(kw_idx).or_default().push(doc_id);
        }

        let mut text_keywords: Vec<usize> = kw_set
            .iter()
            .copied()
            .filter(|_| rng.next_f64() >= TEXT_KEYWORD_DROPOUT_PROB)
            .collect();
        if text_keywords.is_empty() {
            if let Some(&first) = kw_set.iter().next() {
                text_keywords.push(first);
            }
        }

        let mut text = String::new();
        for (i, &kw_idx) in text_keywords.iter().enumerate() {
            if i > 0 {
                text.push(' ');
                text.push_str(FILLER_WORDS[rng.next_range(FILLER_WORDS.len())]);
                text.push(' ');
            }
            text.push_str(&topic_token(kw_idx));
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

    let qa = generate_qa_set(&mut rng, &docs, &inverted, vocab_size, num_queries);
    (docs, qa)
}

/// 各文書から最も出現頻度の低いキーワード 2 語（AND 組み合わせ）を選び、正解集合が
/// コーパス全体に対して十分に絞り込まれた `direct` クエリを構成する
/// （`hybrid_recall.rs::generate_qa_set` と同一実装）。
fn generate_qa_set(
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

        qa.push(QaCase {
            query_text: format!("{} {}", topic_token(a), topic_token(b)),
            query_vector: one_hot_sum(vocab_size, [a, b]),
            correct,
        });
    }

    qa
}

// ---------- Recall 測定ヘルパ（production API 経由: hybrid_search + rerank_candidates） ----------

/// baseline（リランキングなし）・after（リランキングあり）・補助計測（プール
/// Recall@100/@200。原因分析用）を 1 回のクエリ走査でまとめて測定した結果。
///
/// `hybrid_recall.rs::RecallResult` と同じ理由で、分母には正解集合の総数
/// （`total_correct`）ではなく理論上限 `ceil20`/`ceil100`/`ceil200`
/// （Σmin(k,\|correct_q\|)）を使う。
struct RerankRecallResult {
    total_correct: usize,
    baseline_hits20: usize,
    after_hits20: usize,
    pool_hits100: usize,
    pool_hits200: usize,
    /// クエリ単位の `min(20, プール（200件）内の正解数)` の総和。「候補プール内に
    /// 完璧な並び替えを施した場合に上位 20 件で回収しうる理論上限」（Issue #330・
    /// SEARCH-7 改訂で導入。`ceil20` はコーパス全体に対する理論上限であり、プールに
    /// 入っていない正解＝候補生成段の課題まで含むため、リランキング単独の改善余地を
    /// 測るにはこちらを分母に使う）。
    pool_ceiling_hits20: usize,
    ceil20: usize,
    ceil100: usize,
    ceil200: usize,
}

impl RerankRecallResult {
    fn baseline_recall20(&self) -> f64 {
        self.baseline_hits20 as f64 / self.ceil20 as f64
    }

    fn after_recall20(&self) -> f64 {
        self.after_hits20 as f64 / self.ceil20 as f64
    }

    fn pool_recall100(&self) -> f64 {
        self.pool_hits100 as f64 / self.ceil100 as f64
    }

    fn pool_recall200(&self) -> f64 {
        self.pool_hits200 as f64 / self.ceil200 as f64
    }

    /// baseline から見た「候補プール内でリランキングが到達しうる改善余地」
    /// （`pool_ceiling_hits20 − baseline_hits20`）。baseline は候補プールの部分集合
    /// （先頭 20 件）に対する測定であり、`pool_ceiling_hits20` は同一プール全体を
    /// 分母とするため常に `baseline_hits20 <= pool_ceiling_hits20`（負にならない）。
    fn improvement_headroom(&self) -> usize {
        self.pool_ceiling_hits20
            .saturating_sub(self.baseline_hits20)
    }

    /// baseline からの改善幅を、絶対差（after − baseline）ではなく候補プール上限に
    /// 対する相対比率 `(after − baseline) / (pool_ceiling_hits20 − baseline_hits20)`
    /// として表したもの（Issue #330・SEARCH-7 改訂）。改善余地（分母）がコーパス
    /// 全体理論上限 `ceil20` の 1%（`< 0.01 × ceil20`）未満の場合は、構造的にほぼ
    /// 改善不可能な状況であり相対比率の分母が 0 に近づき不安定になるため `None` を
    /// 返す（fail-closed の分母 0 対策を兼ねる）。この値は SEARCH-7 改訂
    /// （2026-08-31・fandhe-db-spec#8）により実コーパス評価まで informational
    /// （非ブロッキング。層 B ゲートの判定には使わない）。層 A・層 B いずれも実測値の
    /// ログ出力にのみ使う。
    fn improvement_ratio(&self) -> Option<f64> {
        let headroom = self.improvement_headroom();
        if (headroom as f64) < 0.01 * self.ceil20 as f64 {
            return None;
        }
        let improved = self.after_hits20.saturating_sub(self.baseline_hits20);
        Some(improved as f64 / headroom as f64)
    }
}

/// [`SparseIndex::build`]・[`ParallelSearchProvider`]・[`hybrid_search`]
/// （`RrfConfig::default()`＝pool_depth 200）で候補プールを 1 回取得し、その同じ
/// プールから baseline（先頭 20 件）・after（[`rerank_candidates`] ＋
/// [`LexicalOverlapReranker::default`]・`RerankConfig::default()`＝final_k 20）・
/// 補助計測（プール Recall@100/@200）を測定する。BM25/RRF/リランキングの
/// 再実装はテスト内で行わない（production コード `crates/engine/src/` は変更しない）。
fn measure_rerank_recall(docs: &[Doc], qa: &[QaCase]) -> RerankRecallResult {
    let refs: Vec<(u64, &str)> = docs.iter().map(|d| (d.id, d.text.as_str())).collect();
    let sparse_index = SparseIndex::build(&refs).expect("sparse index build ok");

    let ids: Vec<u64> = docs.iter().map(|d| d.id).collect();
    let dim = docs.first().map_or(0, |d| d.vector.len());
    let vectors: Vec<f32> = docs.iter().flat_map(|d| d.vector.iter().copied()).collect();
    let provider = ParallelSearchProvider;
    let hybrid_cfg = RrfConfig::default();
    let rerank_cfg = RerankConfig::default();
    let reranker = LexicalOverlapReranker::default();
    // クエリ・プールの二重ループ内で毎回線形探索しないよう、id → text のルック
    // アップテーブルをループ外で 1 度だけ構築する（doc 数 × プール数の掛け算を避ける）。
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
        // 候補プール（融合スコア降順・同点 id 昇順で整列済み。`hybrid_search`
        // の順序契約 = `rerank_candidates` が要求する入力順序契約と同一）を
        // `pool_depth`（= `hybrid_cfg.pool_depth()` = `rerank_cfg.pool_depth()`
        // = 200）件取得する。
        let pool = hybrid_search(
            &provider,
            input,
            &sparse_index,
            &case.query_text,
            hybrid_cfg.pool_depth(),
            &hybrid_cfg,
        )
        .expect("hybrid_search ok");

        // ---- baseline: リランキングなし（プール先頭 20 件） ----
        baseline_hits20 += pool
            .iter()
            .take(20)
            .filter(|h| case.correct.contains(&h.id))
            .count();

        // ---- 補助計測: プール自体の Recall@100/@200（原因分析用） ----
        pool_hits100 += pool
            .iter()
            .take(100)
            .filter(|h| case.correct.contains(&h.id))
            .count();
        let pool_hits_this_case = pool.iter().filter(|h| case.correct.contains(&h.id)).count();
        pool_hits200 += pool_hits_this_case;
        // クエリ単位の「プール内に完璧に並び替えた場合の上位 20 件到達上限」
        // （Issue #330・SEARCH-7 改訂）。
        pool_ceiling_hits20 += pool_hits_this_case.min(20);

        // ---- after: リランキングあり ----
        let candidates: Vec<RerankCandidate<'_>> = pool
            .iter()
            .map(|h| RerankCandidate {
                id: h.id,
                fused_score: h.score,
                text: doc_text_by_id.get(&h.id).copied().unwrap_or(""),
            })
            .collect();
        let reranked = rerank_candidates(&reranker, &case.query_text, &candidates, &rerank_cfg)
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

/// `MIN_INDEXED_ROWS`（`sql::hnsw_cache.rs` の非公開定数）の値。`hybrid_recall.rs::
/// MIN_INDEXED_ROWS` と同じ値の複製（Issue #412）。
const MIN_INDEXED_ROWS: usize = 1_024;

/// ANN opt-in（[`RecallEngine::Hnsw`]）版の [`measure_rerank_recall`]（Issue #412）。
/// 候補プール（200 件）を [`SqlHybridFixture`]（SQL 表層 `EngineCore::execute_sql`
/// 経由）から取得する点のみが異なり、baseline／after／補助計測の集計ロジックは
/// 同一（`RerankCandidate::fused_score` は `sql/exec.rs` の hybrid 分岐が書き込む
/// `ResultRow::score`＝RRF 融合スコアであり、in-memory 版の `h.score` と同じ意味）。
fn measure_rerank_recall_via_hnsw(
    docs: &[Doc],
    qa: &[QaCase],
    engine: RecallEngine,
) -> (RerankRecallResult, AnnStats) {
    let dim = docs.first().map_or(0, |d| d.vector.len());
    let rows: Vec<(u64, Vec<f32>, String)> = docs
        .iter()
        .map(|d| (d.id, d.vector.clone(), d.text.clone()))
        .collect();
    let fixture = SqlHybridFixture::new(dim as u32, &rows, engine);
    let rerank_cfg = RerankConfig::default();
    let reranker = LexicalOverlapReranker::default();
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

        // 候補プール（融合スコア降順・同点 id 昇順。`hybrid_top` の契約は
        // `hybrid_search` と同一 `RrfConfig::default()`＝pool_depth 200 の SQL
        // 表層分岐が返す順序と一致する）。
        let pool = fixture.hybrid_top(&case.query_vector, &case.query_text, 200);

        baseline_hits20 += pool
            .iter()
            .take(20)
            .filter(|(id, _)| case.correct.contains(id))
            .count();
        pool_hits100 += pool
            .iter()
            .take(100)
            .filter(|(id, _)| case.correct.contains(id))
            .count();
        let pool_hits_this_case = pool
            .iter()
            .filter(|(id, _)| case.correct.contains(id))
            .count();
        pool_hits200 += pool_hits_this_case;
        pool_ceiling_hits20 += pool_hits_this_case.min(20);

        let candidates: Vec<RerankCandidate<'_>> = pool
            .iter()
            .map(|(id, score)| RerankCandidate {
                id: *id,
                fused_score: *score,
                text: doc_text_by_id.get(id).copied().unwrap_or(""),
            })
            .collect();
        let reranked = rerank_candidates(&reranker, &case.query_text, &candidates, &rerank_cfg)
            .expect("rerank_candidates ok");
        after_hits20 += reranked
            .iter()
            .filter(|h| case.correct.contains(&h.id))
            .count();
    }

    fixture.assert_ann_non_vacuous(docs.len() >= MIN_INDEXED_ROWS);
    let stats = fixture.stats();
    (
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
        },
        stats,
    )
}

/// 非 vacuous 統計を数値を含まない形式でログへ出す（`hybrid_recall.rs::
/// print_ann_stats` と同型の複製）。
fn print_ann_stats(gate: &str, engine: RecallEngine, stats: &AnnStats) {
    println!(
        "{gate}: engine={} builds={} build_failures={} rebuilds={} hybrid_dense_searches={} hybrid_queries={} ef_cap_fallbacks={} f16_residency_fallbacks={} hybrid_resumed_rounds={} f16_kernel={:?} i8_residency_fallbacks={} i8_kernel={:?}",
        engine.token(),
        stats.builds,
        stats.build_failures,
        stats.rebuilds,
        stats.hybrid_dense_searches,
        stats.hybrid_queries,
        stats.ef_cap_fallbacks,
        stats.f16_residency_fallbacks,
        stats.hybrid_resumed_rounds,
        engine::isa::current_f16(),
        stats.i8_residency_fallbacks,
        engine::isa::current_i8(),
    );
}

/// コーパスが `sparse.rs` の各上限に収まることを検証する（`hybrid_recall.rs::
/// assert_corpus_within_limits` と同一実装。テストハーネス自身にも「無制限な
/// コーパス生成を許さない」設計指針を適用する）。
fn assert_corpus_within_limits(docs: &[Doc]) {
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

// ---------- 層 A: 大規模段（数万件オーダ。SEARCH-7 のスケール条件対応。固定値回帰トラッキング） ----------

const LARGE_NUM_DOCS: usize = 20_000;
const LARGE_NUM_QUERIES: usize = 100;
const LARGE_VOCAB_SIZE: usize = 800;
// `hybrid_recall.rs::LARGE_SEED` とは異なる専用シードを使う（同一シード・同一規模
// パラメータであれば決定的に同一コーパスが生成されるが、本ファイルは
// `hybrid_recall.rs` に依存しない自己完結を保つため、シードも本ファイル固有の
// 定数として複製する）。
const LARGE_SEED: u64 = 0x5EED_0108_4C41_5247;

/// TASK-108（SEARCH-7）層 A: 大規模コーパス（数万件オーダ）で baseline（リランキング
/// なし）と after（リランキングあり）の最終 Recall@20 を実測し、固定値アサーションで
/// 回帰トラッキングする。あわせて「after が baseline を下回らない」ことを独立に
/// アサートする（リランキング層が Recall を悪化させていないことの最小保証。
/// spec の数値基準は使わない）。
#[test]
fn rerank_recall_large_scale_regression() {
    let verbose = verbose_requested_from_env();
    let (docs, qa) = generate_corpus(
        LARGE_SEED,
        LARGE_NUM_DOCS,
        LARGE_NUM_QUERIES,
        LARGE_VOCAB_SIZE,
    );
    assert_corpus_within_limits(&docs);
    assert!(!qa.is_empty());
    for case in &qa {
        assert!(!case.correct.is_empty());
    }
    assert_eq!(qa.len(), 100, "重複除外後の QA 件数が変化した");

    let r = measure_rerank_recall(&docs, &qa);
    if verbose {
        println!(
            "=== TASK-108 大規模段 Recall（docs={} queries={} total_correct={}） ===",
            docs.len(),
            qa.len(),
            r.total_correct
        );
        println!(
            "baseline Recall@20={:.4} ({}/{})  after Recall@20={:.4} ({}/{})  improvement={:.4}",
            r.baseline_recall20(),
            r.baseline_hits20,
            r.ceil20,
            r.after_recall20(),
            r.after_hits20,
            r.ceil20,
            r.after_recall20() - r.baseline_recall20(),
        );
        println!(
            "pool Recall@100={:.4} ({}/{})  pool Recall@200={:.4} ({}/{})",
            r.pool_recall100(),
            r.pool_hits100,
            r.ceil100,
            r.pool_recall200(),
            r.pool_hits200,
            r.ceil200,
        );
        println!(
            "pool_ceiling_hits20={}  improvement_ratio={}",
            r.pool_ceiling_hits20,
            r.improvement_ratio()
                .map(|v| format!("{v:.4}"))
                .unwrap_or_else(|| "N/A (negligible headroom; auto-pass)".to_string()),
        );
    }

    // `hits`/`ceil`/`total_correct` を固定値で回帰トラッキングする（検索カーネル・
    // リランカー・フィクスチャの変更で数値が変化した場合はこのテストが失敗する。
    // 数値基準・実測値の public 記載はオーナー判断で許可済み・
    // `.claude/rules/spec-confidentiality.md` 参照）。
    assert_eq!(r.total_correct, 1049, "正解集合の総数が変化した");
    assert_eq!(r.ceil20, 410, "Recall@20 の理論上限が変化した");
    assert_eq!(r.ceil100, 913, "Recall@100 の理論上限が変化した");
    assert_eq!(r.ceil200, 1049, "Recall@200 の理論上限が変化した");
    assert_eq!(
        r.baseline_hits20, 387,
        "baseline（リランキングなし）の Recall@20 hit 数が変化した"
    );
    assert_eq!(
        r.after_hits20, 389,
        "after（リランキングあり）の Recall@20 hit 数が変化した"
    );
    assert_eq!(r.pool_hits100, 837, "プール Recall@100 hit 数が変化した");
    assert_eq!(r.pool_hits200, 951, "プール Recall@200 hit 数が変化した");
    assert_eq!(
        r.pool_ceiling_hits20, 396,
        "候補プール内でリランキングが到達しうる Recall@20 上限（pool_ceiling_hits20）が変化した"
    );

    // SEARCH-7 契約メモ: `after_hits20 >= baseline_hits20`（リランキング層が
    // Recall を悪化させていないことの独立検証）。境界同点グループ完全化の
    // フォールバックを位置ベースの部分採用から観測範囲の全保持へ変更した
    // Issue #320 codex-review P1 指摘対応適用後、この非劣化アサーションが一時
    // 破れていた（本フィクスチャでは baseline 387 > after 383）。原因は
    // `rank_fused`（Issue #320 で `TieRank::GroupEnd` へ揃え済み）の不整合では
    // なく、`LexicalOverlapReranker` の等重み既定（fused_weight = lexical_weight
    // = 1.0）で字句一致順位の寄与が融合順位の寄与を上回り、字句一致トークンが
    // 脱落した正解文書が字句一致した decoy に逆転されたことによる。Issue #310
    // 対応で既定重みを `fused_weight:lexical_weight = 3.0:1.0`（fused 優位）へ
    // 変更し、この非劣化を回復した（after 388 ≥ baseline 387）。
    //
    // Issue #330（大規模段 improvement@20 ゲート未達の是正）対応: `rank_lexical`
    // の同点規約を位置順位から `rank_fused` と同じ GroupEnd へ統一し、字句信号の
    // 構造的上限（`docs/design/rerank-recall-regression.md`「Issue #330」節参照）
    // まで改善幅を引き上げた（after 389 ≥ baseline 387。採用比率の実測は同節参照）。
    //
    // 同 Issue #330（fandhe-db-spec#7 改訂・SEARCH-7）でゲート側の基準を絶対差
    // （after − baseline）から候補プール上限に対する相対比率
    // （[`RerankRecallResult::improvement_ratio`]。分母 `pool_ceiling_hits20 −
    // baseline_hits20` が構造的な改善余地を表す）へ再定義した。字句信号の構造的
    // 上限に達している本フィクスチャでは
    // `improvement_ratio() = (389 − 387) / (396 − 387) = 2 / 9 ≈ 0.222`。
    assert!(
        r.after_hits20 >= r.baseline_hits20,
        "リランキング層（LexicalOverlapReranker）が baseline（リランキングなし）の Recall@20 を悪化させた"
    );
}

// ---------- 層 B: spec 閾値ゲート（`#[ignore]`。`make rerank-regression` 専用） ----------

#[cfg(test)]
mod improvement_ratio_tests {
    use super::RerankRecallResult;

    /// テスト用の [`RerankRecallResult`] を直接構築するヘルパ（`measure_rerank_recall`
    /// を経由せず、`improvement_ratio` の境界条件を単体で固定する。PR #332
    /// codex-review P2 対応・Issue #330: 層 A の固定値回帰テスト
    /// （`rerank_recall_large_scale_regression`）が通る通常経路〔`Some(2/9)`〕以外の
    /// `headroom == 0`・1% 境界の直前/直後の組み合わせが未検証だった指摘への対応
    /// （`improvement_ratio` は SEARCH-7 改訂・2026-08-31 以降 informational だが、
    /// 実測値ログとして計算し続けるためこの単体テストは維持する）。他フィールド
    /// （`total_correct`/`pool_hits100`/`pool_hits200`/`ceil100`/`ceil200`）は
    /// `improvement_ratio` の計算に使われないためダミー値で固定する。
    fn result_with(
        baseline_hits20: usize,
        after_hits20: usize,
        pool_ceiling_hits20: usize,
        ceil20: usize,
    ) -> RerankRecallResult {
        RerankRecallResult {
            total_correct: 0,
            baseline_hits20,
            after_hits20,
            pool_hits100: 0,
            pool_hits200: 0,
            pool_ceiling_hits20,
            ceil20,
            ceil100: 0,
            ceil200: 0,
        }
    }

    #[test]
    fn improvement_ratio_none_when_headroom_zero() {
        // headroom = pool_ceiling_hits20 - baseline_hits20 = 0（ceiling == baseline。
        // 分母 0 で改善余地が構造的に存在しない）。
        let r = result_with(100, 100, 100, 1000);
        assert_eq!(r.improvement_headroom(), 0);
        assert_eq!(r.improvement_ratio(), None);
    }

    #[test]
    fn improvement_ratio_none_just_below_one_percent_boundary() {
        // ceil20 = 1000 → 1% しきい値 = 10。headroom = 9（境界直前）は分母不安定と
        // みなし None（自動充足扱い）にしなければならない。
        let r = result_with(100, 105, 109, 1000);
        assert_eq!(r.improvement_headroom(), 9);
        assert_eq!(r.improvement_ratio(), None);
    }

    #[test]
    fn improvement_ratio_some_at_exact_one_percent_boundary() {
        // headroom = 10 = ちょうど 1%（境界。`< 0.01 * ceil20` は偽になり Some を返す）。
        let r = result_with(100, 105, 110, 1000);
        assert_eq!(r.improvement_headroom(), 10);
        assert_eq!(r.improvement_ratio(), Some(0.5));
    }

    #[test]
    fn improvement_ratio_some_just_above_one_percent_boundary() {
        let r = result_with(100, 106, 111, 1000);
        assert_eq!(r.improvement_headroom(), 11);
        let ratio = r
            .improvement_ratio()
            .expect("headroom above the 1% threshold must be Some");
        assert!((ratio - (6.0 / 11.0)).abs() < 1e-12);
    }

    #[test]
    fn improvement_ratio_matches_large_scale_regression_fixture_value() {
        // `rerank_recall_large_scale_regression` の固定値（after 389・baseline 387・
        // pool_ceiling_hits20 396・ceil20 410。同テストのアサーション参照）から算出
        // される比率 2/9（同テストのドキュメンテーションコメントに明記済み・
        // `.claude/rules/spec-confidentiality.md` の実測値公開許可の範囲内）と
        // `improvement_ratio()` の算出結果が一致することを固定する。
        let r = result_with(387, 389, 396, 410);
        let ratio = r
            .improvement_ratio()
            .expect("large-scale regression fixture headroom is well above the 1% threshold");
        assert!((ratio - (2.0 / 9.0)).abs() < 1e-12);
    }
}

/// `RERANK_RECALL_MIN_*` 環境変数（`(0.0, 1.0]` の浮動小数点）の解決結果
/// （`hybrid_recall.rs::GateThreshold` と同一の役割）。
enum GateThreshold {
    /// 環境変数が未設定、または GitHub Actions の未設定 repo/environment variable が
    /// 解決する空文字列。
    NotConfigured,
    /// 設定済みで `(0.0, 1.0]` の範囲内。この場合のみ実測値と比較する。
    Value(f64),
}

/// 環境変数を f64 として読み取り、`validate` で許容範囲を検査する共通ヘルパ
/// （[`recall_threshold_from_env`] が範囲を差し替えて再利用する）。未設定・空文字列は
/// [`GateThreshold::NotConfigured`]、
/// 非数値・範囲外は fail-closed（`Err`）、それ以外は [`GateThreshold::Value`] を
/// 返す。数値そのもの（spec の Recall 下限）はこのファイル・ログのいずれにも
/// ハードコードしない（`.claude/rules/spec-confidentiality.md`）。
fn threshold_from_env(
    var: &str,
    validate: impl Fn(f64) -> bool,
    range_desc: &str,
) -> Result<GateThreshold, String> {
    let raw = match std::env::var(var) {
        Ok(v) => v,
        Err(std::env::VarError::NotPresent) => return Ok(GateThreshold::NotConfigured),
        Err(std::env::VarError::NotUnicode(_)) => {
            return Err(format!("{var} is not valid unicode"));
        }
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(GateThreshold::NotConfigured);
    }
    let value: f64 = trimmed
        .parse()
        .map_err(|_| format!("{var} must be a floating-point number"))?;
    if !validate(value) {
        return Err(format!("{var} must be within {range_desc}"));
    }
    Ok(GateThreshold::Value(value))
}

/// `RERANK_RECALL_MIN_R20_LARGE` 環境変数を読み取る（`(0.0, 1.0]` の絶対下限。
/// 0 は「Recall@20 が 0 でよい」という無意味な設定になるため許容しない）。
fn recall_threshold_from_env(var: &str) -> Result<GateThreshold, String> {
    threshold_from_env(var, |v| v > 0.0 && v <= 1.0, "(0.0, 1.0]")
}

/// `RERANK_RECALL_REQUIRE_THRESHOLDS` 環境変数（`"1"` のときのみ true）。
/// `.github/workflows/recall.yml` からの実行（dispatch / schedule）時のみ
/// 注入される strict モードフラグ（`hybrid_recall.rs::strict_thresholds_required`
/// と同型）。
fn strict_thresholds_required() -> bool {
    std::env::var("RERANK_RECALL_REQUIRE_THRESHOLDS")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// `resolver`（[`recall_threshold_from_env`]）を読み取り、[`GateThreshold::
/// NotConfigured`] を strict モード（[`strict_thresholds_required`]）に応じて
/// 分岐させる共通ヘルパ（`hybrid_recall.rs::resolve_gate_threshold` と同一の役割）。
/// strict モード有効時の未設定は fail-closed（`panic!`）、無効時は `None`
/// （呼び出し側で「対象外」を出力して early return する）。非数値・範囲外は
/// strict モードの有無によらず常に fail-closed とする。
fn resolve_gate_threshold_with(
    var: &str,
    resolver: impl Fn(&str) -> Result<GateThreshold, String>,
) -> Option<f64> {
    match resolver(var) {
        Ok(GateThreshold::Value(v)) => Some(v),
        Ok(GateThreshold::NotConfigured) => {
            if strict_thresholds_required() {
                panic!(
                    "{var} is not configured but RERANK_RECALL_REQUIRE_THRESHOLDS=1 (strict mode: this run must evaluate all RERANK_RECALL_MIN_* thresholds; see .github/workflows/recall.yml and the recall-gate environment variables)"
                );
            }
            None
        }
        Err(msg) => panic!("{var} invalid: {msg}"),
    }
}

/// [`RERANK_RECALL_MIN_R20_LARGE`] 用の [`resolve_gate_threshold_with`]。
fn resolve_gate_threshold(var: &str) -> Option<f64> {
    resolve_gate_threshold_with(var, recall_threshold_from_env)
}

// ---------- 実測値の既定非出力（Issue #303）。`RECALL_VERBOSE` opt-in ゲート ----------
// `hybrid_recall.rs` の同名ヘルパと同一実装（`tests/` 直下は独立 test crate・
// 共有モジュール無しの既存慣行に合わせてファイルごとに複製する）。

/// `RECALL_VERBOSE` の生値と `GITHUB_ACTIONS` 判定を引数化した純関数（単体テスト可能）。
/// `hybrid_recall.rs::resolve_verbose` と同一契約（Issue #303）。
fn resolve_verbose(raw: Option<&str>, under_github_actions: bool) -> Result<bool, &'static str> {
    let requested = raw == Some("1");
    if requested && under_github_actions {
        return Err(
            "RECALL_VERBOSE=1 is refused while running under GitHub Actions (GITHUB_ACTIONS is set); rerun outside GitHub Actions to print measured values",
        );
    }
    Ok(requested)
}

/// 環境変数を読み取って [`resolve_verbose`] へ渡し、`Err` は `panic!` で fail-closed に
/// する（各ゲート・層 A 回帰の冒頭、コーパス生成前に呼ぶ）。
fn verbose_requested_from_env() -> bool {
    let raw = std::env::var("RECALL_VERBOSE").ok();
    match resolve_verbose(raw.as_deref(), std::env::var_os("GITHUB_ACTIONS").is_some()) {
        Ok(v) => v,
        Err(msg) => panic!("{msg}"),
    }
}

/// 閾値ゲート判定行の描画（`hybrid_recall.rs::render_gate_line` と同一契約）。
/// `verbose=false`（既定）では実測値を含めず、`verbose=true` では `value=<f64:.4>`
/// を付加する。
fn render_gate_line(gate: &str, metric: &str, value: f64, pass: bool, verbose: bool) -> String {
    if verbose {
        format!("{gate}: {metric} value={value:.4} pass={pass}")
    } else {
        format!("{gate}: {metric} pass={pass}")
    }
}

#[cfg(test)]
mod verbose_gate_tests {
    use super::{render_gate_line, resolve_verbose};

    #[test]
    fn resolve_verbose_defaults_to_false_when_unset() {
        assert_eq!(resolve_verbose(None, false), Ok(false));
    }

    #[test]
    fn resolve_verbose_true_on_exact_match_outside_github_actions() {
        assert_eq!(resolve_verbose(Some("1"), false), Ok(true));
    }

    #[test]
    fn resolve_verbose_rejects_non_exact_values() {
        for raw in [" 1", "true", "0", ""] {
            assert_eq!(resolve_verbose(Some(raw), false), Ok(false));
        }
    }

    #[test]
    fn resolve_verbose_fails_closed_under_github_actions_when_requested() {
        assert!(resolve_verbose(Some("1"), true).is_err());
    }

    #[test]
    fn resolve_verbose_unset_under_github_actions_does_not_block_normal_gate_runs() {
        assert_eq!(resolve_verbose(None, true), Ok(false));
    }

    #[test]
    fn render_gate_line_non_verbose_excludes_measured_value() {
        let line = render_gate_line(
            "rerank_recall_large_scale_threshold_gate",
            "after_recall@20",
            0.8465,
            false,
            false,
        );
        assert!(!line.contains("0.8465"));
        assert!(!line.contains("value="));
        assert!(line.contains("pass=false"));
    }

    #[test]
    fn render_gate_line_verbose_includes_measured_value() {
        let line = render_gate_line(
            "rerank_recall_large_scale_threshold_gate",
            "after_recall@20",
            0.8465,
            true,
            true,
        );
        assert!(line.contains("value=0.8465"));
        assert!(line.contains("pass=true"));
    }
}

/// TASK-108（SEARCH-7）層 B: 大規模段のリランキング後の最終 Recall@20 が
/// `RERANK_RECALL_MIN_R20_LARGE`（絶対下限）以上、かつ非劣化
/// （`after_hits20 >= baseline_hits20`）を保つことを確認する閾値ゲート。
/// baseline からの改善幅（[`RerankRecallResult::improvement_ratio`]）は
/// SEARCH-7 改訂（2026-08-31・fandhe-db-spec#8）により実コーパス評価まで
/// informational（非ブロッキング）へ降格した——2 方式（字句一致・クロス
/// エンコーダ）× 2 fixture の全実測が下限未達で、原因が合成 fixture 側の構造要因と
/// 判明したため（`docs/design/rerank-recall-regression.md`「SEARCH-7 改訂
/// （2026-08-31）」節参照）。実測比率はログにのみ出力し、pass/fail の判定には
/// 使わない。契約は `hybrid_recall.rs::hybrid_recall_large_scale_threshold_gate`
/// と同型（未設定かつ非 strict の場合はコーパス生成前に早期 return して成功終了
/// する。strict モードでは [`resolve_gate_threshold`] が未設定を検出した時点で
/// fail-closed になる）。`after_recall@20` の実測値は注入された閾値の近傍情報を
/// 含むため `RECALL_VERBOSE=1` opt-in 時のみ追加出力する（Issue #303・
/// [`render_gate_line`] 参照。秘匿境界は閾値そのものであり、常時出力する導出入力の
/// hits 件数は層 A の公開固定アサート値〔spec-confidentiality のオーナー判断
/// 2026-08-29 により公開可〕と同一で新規情報を含まない——verbose ゲートは
/// `hybrid_recall.rs` と揃えたログ最小化方針であって機密境界ではない）。一方 informational な `improvement_ratio`（判定に
/// 使わない実測値）は閾値を含まないため、`verbose`／`GITHUB_ACTIONS` の有無に
/// かかわらず常時出力する（codex-review P1・PR #340。`GITHUB_ACTIONS` 下では
/// `RECALL_VERBOSE=1` が fail-closed に拒否され、通常 CI 実行から実測値を一切
/// 記録できなくなっていた抜けを塞ぐ）。
#[test]
#[ignore = "spec 閾値（Actions variables 由来）が必要なため既定では実行しない。make rerank-regression で実行する"]
fn rerank_recall_large_scale_threshold_gate() {
    let verbose = verbose_requested_from_env();
    let min_r20_abs = resolve_gate_threshold("RERANK_RECALL_MIN_R20_LARGE");

    let Some(min) = min_r20_abs else {
        println!(
            "rerank_recall_large_scale_threshold_gate: RERANK_RECALL_MIN_R20_LARGE not configured; gate not enabled (explicit no-op, not a failure)"
        );
        return;
    };

    let (docs, qa) = generate_corpus(
        LARGE_SEED,
        LARGE_NUM_DOCS,
        LARGE_NUM_QUERIES,
        LARGE_VOCAB_SIZE,
    );
    // ANN opt-in（Issue #412・R1）。`LARGE_NUM_DOCS`（20,000）は
    // `MIN_INDEXED_ROWS`（1,024）を上回るため `RecallEngine::Hnsw` は実際に
    // 索引を構築する（`measure_rerank_recall_via_hnsw` の非 vacuous 検証で
    // `builds >= 1` を固定）。
    let engine = RecallEngine::from_env();
    let r = match engine {
        RecallEngine::BruteForce => measure_rerank_recall(&docs, &qa),
        RecallEngine::Hnsw | RecallEngine::HnswF16 | RecallEngine::HnswI8 => {
            let (r, stats) = measure_rerank_recall_via_hnsw(&docs, &qa, engine);
            print_ann_stats("rerank_recall_large_scale_threshold_gate", engine, &stats);
            r
        }
    };
    println!(
        "rerank_recall_large_scale_threshold_gate: engine={}",
        engine.token()
    );
    let after_recall20 = r.after_recall20();

    // ブロッキング条件: (1) after_recall@20 が RERANK_RECALL_MIN_R20_LARGE 以上、
    // (2) 非劣化（after_hits20 >= baseline_hits20。層 A の固定値検証と同じ条件）。
    let pass_abs = after_recall20 >= min;
    let non_degraded = r.after_hits20 >= r.baseline_hits20;
    let pass = pass_abs && non_degraded;
    println!(
        "{}",
        render_gate_line(
            "rerank_recall_large_scale_threshold_gate",
            "after_recall@20",
            after_recall20,
            pass_abs,
            verbose
        )
    );
    println!(
        "rerank_recall_large_scale_threshold_gate: non_degraded (after_hits20 >= baseline_hits20) pass={non_degraded}"
    );

    // 改善幅は informational（非ブロッキング。SEARCH-7 改訂 2026-08-31・
    // fandhe-db-spec#8）。判定に使わない実測値のため `verbose`／`GITHUB_ACTIONS` の
    // 有無にかかわらず常時出力する（閾値近傍の詳細出力を絞る `render_gate_line` の
    // verbose ゲートとは別扱い。codex-review P1・PR #340: informational な
    // improvement_ratio の実測値が通常 CI 実行〔`GITHUB_ACTIONS` 下で
    // `RECALL_VERBOSE=1` は fail-closed に拒否される〕から一切記録されなくなる
    // 抜けを塞ぐ。閾値そのもの〔`RERANK_RECALL_MIN_*`〕は引き続き出力しない）。
    println!(
        "rerank_recall_large_scale_threshold_gate: improvement_ratio@20 inputs (informational, non-blocking since SEARCH-7 rev 2026-08-31) baseline_hits20={} after_hits20={} pool_ceiling_hits20={}",
        r.baseline_hits20, r.after_hits20, r.pool_ceiling_hits20
    );
    match r.improvement_ratio() {
        Some(ratio) => {
            println!(
                "rerank_recall_large_scale_threshold_gate: improvement_ratio@20 (informational, non-blocking since SEARCH-7 rev 2026-08-31) value={ratio:.4}"
            );
        }
        None => {
            println!(
                "rerank_recall_large_scale_threshold_gate: improvement_ratio@20 headroom negligible (informational, non-blocking since SEARCH-7 rev 2026-08-31)"
            );
        }
    }

    assert!(
        pass,
        "reranked Recall@20 is below RERANK_RECALL_MIN_R20_LARGE, or the reranked result degraded below baseline"
    );
}
