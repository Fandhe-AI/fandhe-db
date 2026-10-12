//! TASK-104（対象ビヘイビア: SEARCH-1, SEARCH-2。ポインタ: `docs/spec/05-tasks.md`
//! TASK-104・`docs/spec/04-behavior/search.md`）。ハイブリッド検索カーネル
//! （疎 BM25 = TASK-102 / RRF 融合 = TASK-103 / CJK ストップワード除去 = TASK-105）の
//! Recall 受け入れ基準を自動チェックする回帰テスト。
//!
//! `crates/engine/tests/cjk_tokenizer_impact.rs`（TASK-106）の決定的合成コーパス生成
//! （自前 xorshift64*・外部クレート不使用）＋ Recall 実測＋固定値アサーションによる
//! 回帰トラッキング方式を踏襲する。正解判定は文書の潜在トピック集合
//! （[`Doc::keywords`]）から独立に構築し、疎チャネル（テキスト）・密チャネル
//! （ベクトル）はいずれもその非完全な観測（lossy view）として生成する
//! （[`generate_corpus`] のドキュメント参照）。これにより疎のみ／密のみでしか
//! 見つからない正解例が構造的に生まれ、Recall が 1.0（理論上限への 100% 到達）に
//! 機械的に張り付かない。`engine::hybrid::hybrid_search`（密・疎の RRF 融合）を
//! 通しで測定する。測定は production API（[`SparseIndex::build`]・
//! [`ParallelSearchProvider`]・[`hybrid_search`]＋[`RrfConfig::default`]）のみを
//! 使用し、production コード（`crates/engine/src/`）は変更しない。
//!
//! 2 段のスケール条件（小規模・大規模）を持つ:
//! - 小規模段（`SMALL_NUM_DOCS` 件オーダ）: Recall@20（SEARCH-1 対応）
//! - 大規模段（`LARGE_NUM_DOCS` 件オーダ）: Recall@20・Recall@100（SEARCH-2 対応）
//!
//! 2 層構成（PR CI と閾値ゲートの分離）:
//! - 層 A（`#[test]`・常時 `cargo test` 対象）: 決定的コーパスでのヒット数を固定値
//!   アサーションで回帰トラッキングする（`cjk_tokenizer_impact.rs` と同方式）。
//!   spec の数値基準は使わないため public 資産に閾値を持ち込まない
//!   （`.claude/rules/spec-confidentiality.md`）。
//! - 層 B（`#[ignore]`・`make recall-regression` 経由）: spec 由来の Recall 下限
//!   （`HYBRID_RECALL_MIN_*` 環境変数。`.github/workflows/recall.yml` が
//!   environment `recall-gate` の Actions variables から注入）と実測値を比較する
//!   閾値ゲート。既定（非 strict）では未設定・空文字列を「ゲート未設定＝明示的に
//!   対象外」として成功終了する（silent skip にはしない。
//!   `simd_bench.rs::core5_requested_from_env` と同じ opt-in 方式）。
//!   `HYBRID_RECALL_REQUIRE_THRESHOLDS=1`（`recall.yml` からの実行（dispatch /
//!   schedule）時のみ注入される strict モードフラグ）が立っている場合は、未設定も
//!   非数値・範囲外と同様に fail-closed でテスト失敗とする——environment 作成漏れ・
//!   variable 名の誤り・variable の誤削除により「一度も評価していない run」が
//!   基準を満たした run と同じ green になる事故を防ぐ（PR #147 codex-review P1
//!   継続指摘対応。[`GateThreshold`]・[`resolve_gate_threshold`] 参照）。ログには
//!   対象名と pass/fail のみを出力し、実測値は `RECALL_VERBOSE=1`（`GITHUB_ACTIONS`
//!   下では拒否。Issue #303）の opt-in 時のみ追加出力する（`crates/engine/benches/
//!   batch_bench.rs::verbose_requested_from_env`・Issue #277〜#279 と同方針。
//!   [`resolve_verbose`]・[`verbose_requested_from_env`]・[`render_gate_line`] 参照）。
//!   実測値（Recall@k）は
//!   [`RecallResult::recall20`]/[`RecallResult::recall100`] が定義するとおり
//!   理論上限（`ceil20`/`ceil100`）を分母とする到達率であり、正解集合の総数
//!   （`total_correct`）を分母にしない（層 A と分母の意味を揃える）。
//!
//! クエリ展開の結線（Issue #306。codex-review P1 対応は PR #308）: 大規模段の
//! 層 B（[`hybrid_recall_large_scale_threshold_gate`]）は TASK-110〜113
//! （`crates/engine/tests/query_planning_recall.rs`）で確立した決定的スタブ
//! `LlmClient`（[`MockLlmClient`]。`query_planning_recall.rs::MockLlmClient` と
//! 同一実装。`tests/` 直下は独立 test crate で共有モジュールを持たないためこの
//! ファイルへ複製する）による展開ありの経路（[`QuerySource::Expanded`]・
//! [`measure_recall_with`]）で Recall@20・Recall@100 を測定し、spec の SEARCH-2
//! （クエリ展開ありを前提とするビヘイビア）の測定前提に揃える。QA セットは
//! `direct` カテゴリ（コーパス語彙 `kw_XXXX` に一致するクエリ）から生成するが、
//! そのまま展開経路へ渡すと `MockLlmClient` は `kw_` 形式の語を無変換で通すため
//! 展開ありの実測値が展開なしと構造的に一致してしまい、production の展開結線が
//! 欠落・破損してもゲートが通過してしまう（PR #308 codex-review P1 指摘）。
//! [`to_intent_query`] でクエリテキストのみを `intent` 形（`syn_XXXX` 形式。
//! コーパス語彙とは一致しない）へ書き換えたうえで展開経路に通すことで、
//! `MockLlmClient` の同義語写像（展開結果を検索入力へ反映する経路）を経由して
//! 初めて Recall が出る非恒等なゲートにする（正解集合は変更しないため既存の
//! 閾値の較正・意味は変わらない）。層 A（[`hybrid_recall_large_scale_regression`]）
//! は従来どおり `direct` 形クエリのままの展開あり経路とのパススルー等式を
//! 検証し続ける（`docs/design/hybrid-recall-regression.md`
//! 「クエリ展開の結線（Issue #306）」節も参照）。小規模段の層 B・層 A の主測定は
//! 引き続き展開なし（[`QuerySource::Baseline`]）で行う。
//!
//! `plan_query` 結線の production 経由化（Issue #306・PR #308 codex-review P1
//! 継続指摘対応）: [`to_intent_query`] だけでは、展開経路（[`expand_and_reconstruct_with`]）
//! が [`query_planner::render_full_prompt`]/[`query_planner::parse_expansion`] を
//! 固定接頭辞 `""` で直接呼ぶ手組みのままだったため、[`EngineCore::plan_query`]・
//! `with_query_planner`・辞書スナップショット（`dictionary_snapshot` →
//! `render_prompt_prefix`）という production の結線自体が欠落・破損しても検出でき
//! なかった。[`PlannerFixture`]（`path`/`body` 各 1 行のみの最小テーブルを持つ
//! `EngineCore`）を介して [`EngineCore::plan_query`] を実際に呼ぶよう
//! [`expand_and_reconstruct_with`] を書き換え、辞書スナップショットに含めた
//! センチネル値（[`DICTIONARY_WIRING_SENTINEL_PATH`]）が展開時のプロンプトに
//! 現れなければ [`MockLlmClient::complete`] が `Err(PlanError::InvalidResponse)` を
//! 返して展開自体を失敗させる（`plan_query`/`with_query_planner`/辞書スナップショット
//! 結線のいずれが壊れても検出可能。再埋め込み〔`plan_and_embed_query`〕・`USING PLAN`
//! SQL 表層は対象外のまま）。
//!
//! 既知の制約（スコープ外・フォローアップ）:
//! - 合成コーパスによる暫定測定であり、実コーパスでの評価は未了
//!   （`docs/design/hybrid-recall-regression.md` 参照。`cjk_tokenizer_impact.rs` と同種の制約）
//! - 大規模段層 B の展開結線検証は [`to_intent_query`] による疑似 `intent` 形
//!   クエリ（`syn_XXXX`。QA の正解集合・fixture 自体は `direct` カテゴリ由来）で
//!   行っており、TASK-113（PLAN-3）が定義する `intent` カテゴリ（言い換え語彙の
//!   QA セット）自体の大規模測定はこのハーネスのスコープ外のまま
//!   （フォローアップ候補。Issue 起票はユーザー判断待ち）

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::core::{CoreError, EngineCore};
use engine::hybrid::{hybrid_search, RrfConfig};
use engine::kernel::{CpuScalarProvider, SearchInput, SearchProvider};
use engine::parallel_search::ParallelSearchProvider;
use engine::policy::PolicyContext;
use engine::query_planner::{LlmClient, PlanError};
use engine::recovery::required_op_id::OperationId;
use engine::row_codec::Value;
use engine::sparse::SparseIndex;
use engine::storage::{Storage, Visibility};
use std::collections::{BTreeMap, BTreeSet};

#[path = "../src/test_util/temp_db.rs"]
mod temp_db;
use temp_db::{unique_db_path, CleanupGuard};

// ANN opt-in（Issue #412）検索エンジン切替 fixture。既定（`RecallEngine::
// BruteForce`）では本モジュールの型を一切構築せず、以下の層 A・既存層 B の
// 実測値・固定値アサーションに影響を与えない。
#[path = "fixtures/recall_engine.rs"]
mod recall_engine;
use recall_engine::{AnnStats, RecallEngine, SqlHybridFixture};

// ---------- 決定的擬似乱数（xorshift64*。外部クレート不使用。cjk_tokenizer_impact.rs と同一実装） ----------

/// コーパス生成専用の決定的擬似乱数生成器（テスト再現性のため外部の乱数クレートは
/// 使わず、xorshift64* をこのファイル内に自前実装する。依存最小方針）。
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

/// トピック `idx` に対応するキーワードトークンを決定的に合成する。固定語彙表を
/// 使わず語彙数（`vocab_size`）をコーパス規模に応じて可変にすることで、規模が
/// 大きくなっても QA セットの正解集合（キーワード 2 語の AND 交差）が肥大化せず
/// 絞り込まれた状態を維持できるようにする（`sparse::tokenize` の ASCII 単語境界規則の
/// 下で必ず 1 トークンになるよう、アンダースコア区切りの接頭辞＋4 桁ゼロ埋め数字とする）。
fn topic_token(idx: usize) -> String {
    format!("kw_{idx:04}")
}

/// [`topic_token`] の逆写像。展開結果（`QueryExpansion::search_terms`）から
/// 再構成クエリの密ベクトルを組み立てる際に使う
/// （`query_planning_recall.rs::parse_topic_index` と同一実装。`tests/` 直下は
/// 独立 test crate のため複製する）。
fn parse_topic_index(token: &str) -> Option<usize> {
    token.strip_prefix("kw_")?.parse().ok()
}

/// [`synonym_token`] 形式（`syn_XXXX`）の逆写像。プレフィックス不一致・非数値は
/// `None`（想定外の語は写像しない = そのまま通す。[`MockLlmClient::complete`]
/// 参照）。`query_planning_recall.rs::parse_synonym_index` と同一実装を複製し
/// `MockLlmClient` を同一契約に保つ。[`hybrid_recall_large_scale_threshold_gate`]
/// が [`to_intent_query`] で構成する `syn_XXXX` 形クエリの展開時にも使われる
/// （Issue #306 codex-review P1 対応・PR #308）。
fn parse_synonym_index(token: &str) -> Option<usize> {
    token.strip_prefix("syn_")?.parse().ok()
}

/// トピック `idx` に対応する「言い換え語彙」トークン（[`topic_token`] の同義語版。
/// コーパス語彙とは重ならない形式）。`query_planning_recall.rs::synonym_token` と
/// 同一実装を複製する。[`to_intent_query`] が `direct` カテゴリの QA クエリ
/// （`kw_XXXX` 形式）を `intent` 形（`syn_XXXX` 形式）へ書き換える際に使う
/// （Issue #306 codex-review P1 対応・PR #308）。
fn synonym_token(idx: usize) -> String {
    format!("syn_{idx:04}")
}

/// 文脈語プール（内容語の間に挟む機能語。BM25 の相対比較には影響しない飾り）。
const FILLER_WORDS: [&str; 10] = [
    "the", "a", "an", "of", "for", "and", "with", "in", "on", "note",
];

/// 合成コーパス 1 文書。`keywords` は生成時に既知の「正解（潜在）トピック集合」＝
/// トピックインデックス（[`topic_token`] 参照）で、QA セットの正解文書判定
/// （[`generate_qa_set`] の `inverted` 構築）にのみ使う——`text`（疎チャネル）・
/// `vector`（密チャネル）はいずれもこの潜在集合の非完全な観測（lossy view。
/// [`generate_corpus`] のドロップアウト／デコイのドキュメント参照）であり、
/// 正解判定そのものには使わない。正解集合の生成（`keywords`）と検索特徴量
/// （`text`・`vector`）を分離することで、疎のみ／密のみでしか見つからない
/// 正解例・どちらからも見つけにくい正解例が構造的に生まれ、Recall が
/// 天井（1.0）に張り付かない現実的な分布になる（codex-review 指摘対応）。
struct Doc {
    id: u64,
    text: String,
    keywords: BTreeSet<usize>,
    vector: Vec<f32>,
}

/// QA セット 1 件。`correct` は生成時に既知の正解文書 ID 集合、`query_vector` は
/// 正解集合と相関する密クエリベクトル。
struct QaCase {
    query_text: String,
    query_vector: Vec<f32>,
    correct: BTreeSet<u64>,
}

/// トピック `idx` を「密ベクトル空間の次元 `idx`」へ直接対応させる one-hot 構成
/// （次元数 = `vocab_size`）。密ベクトルは基底ベクトル（この関数）の和として
/// 合成するため、与えた次元集合との共通部分数がそのまま内積のスコアになる——
/// BM25 が AND 一致の文書を上位に置く挙動と同型の、密チャネル側の決定的な信号を
/// 作るための設計（ランダム方向ベクトルの平均ではトピック間のクロス項ノイズが
/// AND 一致信号を上回り、スコアの再現性・解釈性が失われるため採用しない。
/// `docs/design/hybrid-recall-regression.md` 参照）。与える次元集合は文書の潜在
/// トピック集合そのものとは限らない（[`generate_corpus`] のドロップアウト／デコイに
/// よる lossy view）ため、密ベクトルと疎テキストが常に一致するとは限らない。
fn one_hot_sum(vocab_size: usize, indices: impl IntoIterator<Item = usize>) -> Vec<f32> {
    let mut v = vec![0.0f32; vocab_size];
    for idx in indices {
        if let Some(slot) = v.get_mut(idx) {
            *slot = 1.0;
        }
    }
    v
}

/// [`build_zipf_cumulative_weights`]/[`zipf_index`] は `cjk_tokenizer_impact.rs` と同一の
/// Zipf 近似分布（重み `1/(i+1)`）で、先頭語ほど高頻度・末尾語ほど低頻度にし、QA セットの
/// 正解集合を小さく絞り込める語彙分布を作る。
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
/// `MAX_CORPUS_BYTES` に対応）。環境変数からサイズを受け取らず、テスト内定数のみで
/// 規模を決めることで無制限アロケーションを防ぐ（coding-rust.md「untrusted 入力の扱い」）。
const MAX_CORPUS_DOCS_GUARD: usize = 100_000;

/// fixture パラメータ（spec 由来の数値基準ではなく、本テストハーネスが Recall を
/// 1.0 未満の現実的な分布にするために実験的に選んだ確率）。文書の潜在トピック集合
/// （[`Doc::keywords`]）のうち、テキスト（疎チャネル）へ反映されず脱落する確率。
/// 「latent トピックはあるが埋め込み・索引が捉え損ねる」を模する（[`generate_corpus`]
/// のドキュメント参照）。
const TEXT_KEYWORD_DROPOUT_PROB: f64 = 0.18;

/// fixture パラメータ（同上）。潜在トピック集合のうち密ベクトルへ反映されず脱落する
/// 確率。
const VECTOR_KEYWORD_DROPOUT_PROB: f64 = 0.18;

/// fixture パラメータ（同上）。密ベクトルに、文書の潜在トピック集合に含まれない
/// 無関係な次元が 1 つ紛れ込む（decoy）確率。埋め込みが無関係トピックへ誤って
/// 反応する状況を模し、密チャネルの偽陽性（クラウディング）を作る。
const VECTOR_DECOY_PROB: f64 = 0.12;

/// 決定的シード付き擬似乱数でトピック相関コーパスと QA セット（`direct` カテゴリ相当:
/// 最も出現頻度の低いキーワード 2 語の AND 組み合わせ）を生成する。`num_docs`・
/// `num_queries`・`vocab_size` は呼び出し側（テスト内定数）が固定し、環境変数からは
/// 受け取らない。`vocab_size` はコーパス規模に応じて選ぶ（QA 正解集合の絞り込み度を
/// 保つため。[`topic_token`] のドキュメント参照）。
///
/// 正解判定（`inverted` → QA の `correct`）は文書の潜在トピック集合
/// （[`Doc::keywords`]）から構築し、疎チャネル（`text`）・密チャネル（`vector`）は
/// いずれもこの潜在集合の非完全な観測として独立に生成する
/// （[`TEXT_KEYWORD_DROPOUT_PROB`]・[`VECTOR_KEYWORD_DROPOUT_PROB`]・
/// [`VECTOR_DECOY_PROB`]）。ドロップアウト・デコイはいずれも 0/1 の one-hot 次元への
/// 操作のみで、浮動小数点の連続ノイズは加えない——`ParallelSearchProvider`
/// の加算順序（並列分割）に依存する丸め誤差を避け、層 A の固定値アサーションが
/// スレッド数に左右されず再現可能であることを保証するため。
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

        // 正解判定用の inverted index は潜在トピック集合 `kw_set` から構築する
        // （疎・密いずれの lossy view にも依存しない。ground truth の独立性）。
        for &kw_idx in &kw_set {
            inverted.entry(kw_idx).or_default().push(doc_id);
        }

        // ---- 疎チャネル（テキスト）の lossy view: 潜在トピックを確率的に脱落させる ----
        let mut text_keywords: Vec<usize> = kw_set
            .iter()
            .copied()
            .filter(|_| rng.next_f64() >= TEXT_KEYWORD_DROPOUT_PROB)
            .collect();
        if text_keywords.is_empty() {
            // 全脱落は疎チャネルを完全に無効化してしまうため、最低 1 語は残す
            // （`num_keywords >= 3` により `kw_set` は必ず非空）。
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

        // ---- 密チャネル（ベクトル）の lossy view: 脱落＋無関係トピックの混入 ----
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
/// （`cjk_tokenizer_impact.rs::generate_qa_set` と同方式）。密クエリベクトルは選んだ
/// 2 語を [`one_hot_sum`] で符号化したもの（疎クエリと対応する AND 信号。クエリ自体は
/// 潜在トピック集合から直接構成する「意図が明確なクエリ」であり、[`generate_corpus`]
/// の lossy view はドキュメント側にのみ適用する）。
///
/// 正規化済み語ペア（`(min(a,b), max(a,b))`）を [`BTreeSet`] で追跡し、同一ペアの
/// 重複登録を除外する（codex-review 指摘対応。重複を許すと Recall が特定の語ペアへ
/// 偏り、少数の「簡単な」クエリの繰り返しで指標が水増しされるため）。重複除外の結果
/// `qa.len()` は `num_queries` を下回りうる（コーパス中のユニークな語ペア数が上限のため）。
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

// ---------- Recall 測定ヘルパ（production API 経由。engine::hybrid::hybrid_search） ----------

/// Recall@20・Recall@100 の実測結果（ヒット数と理論上限）。
///
/// `total_correct`（正解集合の総数）を分母にすると、正解集合が k 件を超えるクエリが
/// 混ざった場合に理論上の最大値が 1.0 未満に頭打ちになる（天井効果）。本ハーネスの
/// [`recall20`](RecallResult::recall20)/[`recall100`](RecallResult::recall100) は
/// 分母に理論上限 `ceil20`/`ceil100`（Σmin(k,\|correct_q\|)）を使い、達成可能な上限に
/// 対する到達率として層 A（回帰トラッキング）・層 B（spec 閾値ゲート）の両方で同じ
/// 意味の値を扱えるようにする。`total_correct` 自体は層 A の固定値アサーション対象
/// として残す。
struct RecallResult {
    total_correct: usize,
    hits20: usize,
    hits100: usize,
    ceil20: usize,
    ceil100: usize,
}

impl RecallResult {
    /// Recall@20 = hits20 / ceil20（理論上限に対する到達率）。`ceil20 == 0` は
    /// QA セットが空の場合のみ起こり得る呼び出し側の不変条件違反であり、
    /// その場合は NaN となって呼び出し側の `>=` 比較が false（fail-closed）になる。
    fn recall20(&self) -> f64 {
        self.hits20 as f64 / self.ceil20 as f64
    }

    /// Recall@100 = hits100 / ceil100（[`recall20`](Self::recall20) と同じ理由で
    /// 理論上限を分母にする）。
    fn recall100(&self) -> f64 {
        self.hits100 as f64 / self.ceil100 as f64
    }
}

/// [`PlannerFixture`] が [`EngineCore::plan_query`] の辞書スナップショット
/// （`dictionary_snapshot` → [`query_planner::render_prompt_prefix`]）へ確実に
/// 含める、`plan_query` 結線の生死を判定するためのセンチネル値（`path` 列に
/// 1 行だけ挿入する。`render_prompt_prefix` は `dictionary.file_tree.paths` を
/// そのまま出力するため、`Files` 節の 1 行としてバイト同一に現れる契約
/// （`query_planner.rs::render_prompt_prefix` 参照）。codex-review P1 対応
/// （Issue #306・PR #308）: これが `prompt` に現れない場合は
/// [`EngineCore::plan_query`]（`with_query_planner`／辞書スナップショットの結線）
/// が壊れている signal であり、[`MockLlmClient::complete`] はここで
/// `Err(PlanError::InvalidResponse)` を返して展開自体を失敗させる。
const DICTIONARY_WIRING_SENTINEL_PATH: &str = "hybrid-recall-wiring-sentinel/marker.md";

/// `direct` 語彙（`kw_XXXX`。本ファイルの QA セットの唯一のカテゴリ）を無変換で通す
/// 決定的スタブ `LlmClient`（`query_planning_recall.rs::MockLlmClient` と同一実装。
/// `tests/` 直下は独立 test crate で共有モジュールを持たないためこのファイルへ複製する。
/// 実 Ollama へは接続しない＝TASK-110 時点からの継続制約）。
///
/// `complete` に渡される `prompt` は [`EngineCore::plan_query`]（[`PlannerFixture`]
/// 経由）が `dictionary_snapshot` → `render_prompt_prefix` → `render_full_prompt` の
/// production 結線で組み立てたものであり、本ファイルが直接 `render_full_prompt` を
/// 呼んで手組みすることはしない（codex-review P1 対応・Issue #306・PR #308。
/// 以前はここを手組みプロンプトに固定接頭辞 `""` を渡していたため
/// `EngineCore::plan_query`／`with_query_planner`／辞書スナップショットの結線が
/// 欠落・破損してもこのゲートは検出できなかった）。
struct MockLlmClient;

impl LlmClient for MockLlmClient {
    fn complete(&self, prompt: &str) -> Result<String, PlanError> {
        // [`DICTIONARY_WIRING_SENTINEL_PATH`] のドキュメント参照。辞書コンテキストの
        // 適用が欠落・破損すると `prompt` からこの行が消えるため、その場合は
        // production の `LlmClient::complete` 契約が想定する「応答不正」と同じ
        // `Err` 系統（`PlanError::InvalidResponse`）で fail-closed に展開を失敗させる
        // （`.expect` で `plan_query` の呼び出し側が確実にパニックし、ゲートが赤くなる）。
        if !prompt.contains(DICTIONARY_WIRING_SENTINEL_PATH) {
            return Err(PlanError::InvalidResponse);
        }

        // `render_full_prompt` は接頭辞の直後に "\n# Question\n" ＋サニタイズ済み
        // 質問＋"\n" を追記する契約（`query_planner.rs::render_full_prompt` 参照）。
        // マーカ以降の 1 行を質問本文として取り出す。
        let question = prompt
            .split("# Question\n")
            .nth(1)
            .and_then(|rest| rest.lines().next())
            .unwrap_or("");

        let mapped_terms: Vec<String> = question
            .split_whitespace()
            .map(|token| match parse_synonym_index(token) {
                Some(idx) => topic_token(idx),
                None => token.to_string(),
            })
            .collect();

        // `parse_expansion`（production API）がそのまま受理できる最小 JSON を
        // 組み立てる（検索語は制御文字を含まない ASCII トークンのみなので、
        // 文字列エスケープは不要）。
        let terms_json = mapped_terms
            .iter()
            .map(|t| format!("\"{t}\""))
            .collect::<Vec<_>>()
            .join(",");
        Ok(format!(
            "{{\"search_terms\":[{terms_json}],\"path_hint\":null,\"kind_hint\":null}}"
        ))
    }
}

/// `plan_query`（[`query_planner`]・辞書スナップショット）の唯一の呼び出し口。
/// 検索対象コーパス（[`SearchFixture`]）とは無関係の、`EngineCore::plan_query`
/// （TASK-110。`with_query_planner`・`dictionary_snapshot` の production 結線）を
/// 実際に経由するためだけの最小構成（`path`/`body` 各 1 行のみのテーブル）を保持する
/// （codex-review P1 対応・Issue #306・PR #308。`tests/sql_using_plan.rs` の
/// `EngineCore::from_storage(..).with_query_planner(..)` 流儀を踏襲）。
struct PlannerFixture {
    core: EngineCore,
    ctx: PolicyContext,
    _guard: CleanupGuard,
}

impl PlannerFixture {
    const TABLE: &'static str = "docs";

    fn new() -> Self {
        let path = unique_db_path("hybrid-recall-query-expansion");
        let guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .create_table(&TableSchema::new(
                Self::TABLE,
                vec![
                    // `insert_typed_row` は VECTOR 列を要求する（`tenant.rs`
                    // `insert_typed_row_unchecked` 参照）。この器は `plan_query`
                    // （辞書スナップショット・展開）のためだけに存在し検索対象では
                    // ないため次元は最小の 1 で十分。
                    ColumnDef::new("embedding", ColumnType::Vector(1), false),
                    ColumnDef::new("path", ColumnType::Text, false),
                    ColumnDef::new("body", ColumnType::Text, false),
                ],
            ))
            .expect("create dictionary table");

        let ctx =
            PolicyContext::with_visibilities("hybrid-recall-test-tenant", [Visibility::Public])
                .expect("valid tenant");
        let op_id =
            OperationId::parse("hybrid-recall-dictionary-seed-1").expect("valid operation_id");
        engine::tenant::insert_typed_row(
            &storage,
            Self::TABLE,
            &ctx,
            1,
            Visibility::Public,
            &[
                Value::Vector(vec![0.0]),
                Value::Text(DICTIONARY_WIRING_SENTINEL_PATH.to_string()),
                Value::Text("hybrid_recall.rs query expansion wiring sentinel row".to_string()),
            ],
            &op_id,
        )
        .expect("insert dictionary seed row");

        let core = EngineCore::from_storage(storage, Box::new(CpuScalarProvider))
            .with_query_planner(Box::new(MockLlmClient));
        Self {
            core,
            ctx,
            _guard: guard,
        }
    }

    /// [`EngineCore::plan_query`] を呼ぶ薄いラッパ（`Self::TABLE` 固定）。
    fn plan(&self, question: &str) -> Result<engine::query_planner::QueryExpansion, CoreError> {
        self.core.plan_query(&self.ctx, Self::TABLE, question)
    }
}

/// `baseline_query_text` を質問として [`PlannerFixture::plan`]（[`EngineCore::
/// plan_query`] の production 結線。辞書スナップショット取得 → 固定接頭辞
/// レンダリング → `LlmClient::complete` → 厳格パースの一連）に通し、得られた
/// `QueryExpansion::search_terms` から再構成クエリ（疎チャネル用テキスト・密
/// チャネル用 one-hot ベクトル）を組み立てる。再埋め込み（`plan_and_embed_query`・
/// `USING PLAN` SQL 表層）は対象外——本ハーネスは密チャネルの再構成に
/// [`one_hot_sum`] を使う既存契約を維持する（codex-review P1 対応・Issue #306・
/// PR #308）。
fn expand_and_reconstruct_with(
    fixture: &PlannerFixture,
    vocab_size: usize,
    baseline_query_text: &str,
) -> (String, Vec<f32>) {
    let expansion = fixture.plan(baseline_query_text).expect("plan_query ok");

    let text = expansion.search_terms.join(" ");
    let indices = expansion
        .search_terms
        .iter()
        .filter_map(|t| parse_topic_index(t));
    let vector = one_hot_sum(vocab_size, indices);
    (text, vector)
}

/// `direct` カテゴリの QA ケース（`query_text` が `kw_XXXX kw_XXXX` 形式）を
/// `intent` 形（`syn_XXXX syn_XXXX` 形式）へ書き換える（Issue #306 codex-review
/// P1 対応・PR #308）。`kw_XXXX` のまま [`expand_and_reconstruct_with`] へ渡すと
/// [`MockLlmClient`] は `parse_synonym_index` が `kw_` 接頭辞を認識せず無変換で
/// 通すため、展開経路が実際に検索結果へ寄与しているかをゲートが検出できない
/// （`direct` のまま展開しても展開なしと構造的に同じ検索が行われてしまう）。
/// `syn_XXXX` はコーパス語彙（`kw_XXXX`）と一致しないため、[`MockLlmClient`] の
/// 同義語写像（`query_planning_recall.rs::intent_baseline` と同型の構成）を経由
/// して初めて検索が成立する——production の展開結線（[`PlannerFixture`] 経由の
/// [`EngineCore::plan_query`]・展開結果の検索入力への反映）が欠落・破損すれば
/// `syn_XXXX` はコーパスのどの文書ともマッチせず Recall が崩壊するため、この
/// ゲートは非恒等な検証になる（`plan_query` 自体の結線破損は
/// [`DICTIONARY_WIRING_SENTINEL_PATH`] のセンチネル検証が別途捕捉する）。
/// 正解集合（`correct`）は `kw_XXXX` のトピックインデックスから独立に決まる
/// ため変更しない。
fn to_intent_query(qa: &QaCase) -> QaCase {
    let intent_text = qa
        .query_text
        .split_whitespace()
        .map(|token| match parse_topic_index(token) {
            Some(idx) => synonym_token(idx),
            None => token.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ");
    QaCase {
        query_text: intent_text,
        // Expanded 経路（[`measure_recall_against`]）は `query_vector` を参照せず
        // 展開結果からベクトルを再構成するため、元のベクトルをそのまま引き継いで
        // 構わない（フィールド整合目的のみ）。
        query_vector: qa.query_vector.clone(),
        correct: qa.correct.clone(),
    }
}

/// [`measure_recall_with`] が測定するクエリ経路の選択（Issue #306。SEARCH-2 の
/// 測定前提＝クエリ展開ありへ揃えるための切り替え）。
enum QuerySource<'a> {
    /// 展開なし（`QaCase::query_text`/`query_vector` をそのまま使う）。既存の
    /// 層 A 固定値回帰・小規模段層 B はこの経路のまま。
    Baseline,
    /// 展開あり（[`expand_and_reconstruct_with`] で再構成したクエリを使う）。
    /// 大規模段層 B（[`hybrid_recall_large_scale_threshold_gate`]）が使う。
    /// [`EngineCore::plan_query`] の production 結線を実際に経由するため
    /// `&dyn LlmClient` ではなく [`PlannerFixture`] を保持する（codex-review P1
    /// 対応・Issue #306・PR #308）。
    Expanded(&'a PlannerFixture),
}

/// [`measure_recall_with`]／複数回の測定（baseline・expanded 双方を同じ
/// コーパスに対して測る大規模段層 A 等）で疎索引・密ベクトル配列の構築
/// （[`SparseIndex::build`] とコーパス全件のベクトル平坦化）を使い回すための
/// バンドル。索引構築はコーパス規模に対して線形コストがかかるため、同一
/// コーパスに対する複数回の測定で毎回再構築しない（Issue #306 で大規模段層 A に
/// パススルー等式ガードを追加したことによる実行時間の悪化を避ける）。
struct SearchFixture {
    ids: Vec<u64>,
    vectors: Vec<f32>,
    dim: usize,
    sparse_index: SparseIndex,
}

impl SearchFixture {
    /// [`SparseIndex::build`]（production API）でコーパスから疎索引を構築し、
    /// 密ベクトルを ID 順に平坦化する。テスト内で BM25 の再実装は行わない。
    fn build(docs: &[Doc]) -> Self {
        let refs: Vec<(u64, &str)> = docs.iter().map(|d| (d.id, d.text.as_str())).collect();
        let sparse_index = SparseIndex::build(&refs).expect("sparse index build ok");
        let ids: Vec<u64> = docs.iter().map(|d| d.id).collect();
        let dim = docs.first().map_or(0, |d| d.vector.len());
        let vectors: Vec<f32> = docs.iter().flat_map(|d| d.vector.iter().copied()).collect();
        Self {
            ids,
            vectors,
            dim,
            sparse_index,
        }
    }
}

/// [`ParallelSearchProvider`]・[`hybrid_search`]（`RrfConfig::default()` = spec
/// 採用構成: 等重み・pool_depth 200・k_const 60）という production の検索経路の
/// みを用いて、事前構築済みの `fixture`（[`SearchFixture::build`]）に対する
/// Recall@20・Recall@100 を測定する。`source` が [`QuerySource::Expanded`] の
/// 場合、各 QA ケースのクエリを [`expand_and_reconstruct_with`]（production の
/// クエリ展開経路）で再構成してから検索する（Issue #306）。
fn measure_recall_against(
    fixture: &SearchFixture,
    qa: &[QaCase],
    source: QuerySource<'_>,
) -> RecallResult {
    let provider = ParallelSearchProvider;
    let cfg = RrfConfig::default();

    let mut total_correct = 0usize;
    let mut hits20 = 0usize;
    let mut hits100 = 0usize;
    let mut ceil20 = 0usize;
    let mut ceil100 = 0usize;

    for case in qa {
        total_correct += case.correct.len();
        ceil20 += case.correct.len().min(20);
        ceil100 += case.correct.len().min(100);

        // `Baseline` は既存の QA クエリをそのまま複製して使い、`Expanded` は
        // production のクエリ展開経路（[`expand_and_reconstruct_with`]）で
        // 再構成したクエリを使う。QA 件数（最大 100 件オーダ）に対する複製コストは
        // 無視できるため、参照の分岐より単純さを優先する。
        let (query_text, query_vector): (String, Vec<f32>) = match &source {
            QuerySource::Baseline => (case.query_text.clone(), case.query_vector.clone()),
            QuerySource::Expanded(planner) => {
                expand_and_reconstruct_with(planner, fixture.dim, &case.query_text)
            }
        };

        let input = SearchInput {
            ids: &fixture.ids,
            vectors: &fixture.vectors,
            dim: fixture.dim as u32,
            query: &query_vector,
            k: 100,
        };
        let hits = hybrid_search(
            &provider,
            input,
            &fixture.sparse_index,
            &query_text,
            100,
            &cfg,
        )
        .expect("hybrid_search ok");

        hits20 += hits
            .iter()
            .take(20)
            .filter(|h| case.correct.contains(&h.id))
            .count();
        hits100 += hits.iter().filter(|h| case.correct.contains(&h.id)).count();
    }

    RecallResult {
        total_correct,
        hits20,
        hits100,
        ceil20,
        ceil100,
    }
}

/// Issue #307（SEARCH-1）: 融合前の密チャネル単体・疎チャネル単体それぞれの
/// Recall@20 を測定する（[`measure_recall`] と同じ production API 経由。密は
/// [`SearchProvider::search`] の Top-20 をそのまま、疎は
/// [`SparseIndex::search_within`] の Top-20 をそのまま正解判定に使う。RRF 融合
/// （[`hybrid_search`]）は経由しない）。呼び出し側では「融合がどちらのチャネル
/// 単体も下回らない（SEARCH-3 相当）」ことを実測値どうしの関係アサーションと
/// して確認する（数値そのものは public テストへ記録しない。受け入れ条件 2）。
fn measure_channel_recall20(docs: &[Doc], qa: &[QaCase]) -> (usize, usize) {
    let refs: Vec<(u64, &str)> = docs.iter().map(|d| (d.id, d.text.as_str())).collect();
    let sparse_index = SparseIndex::build(&refs).expect("sparse index build ok");

    let ids: Vec<u64> = docs.iter().map(|d| d.id).collect();
    let dim = docs.first().map_or(0, |d| d.vector.len());
    let vectors: Vec<f32> = docs.iter().flat_map(|d| d.vector.iter().copied()).collect();
    let provider = ParallelSearchProvider;
    let visible_ids: BTreeSet<u64> = ids.iter().copied().collect();

    let mut dense_hits20 = 0usize;
    let mut sparse_hits20 = 0usize;

    for case in qa {
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: dim as u32,
            query: &case.query_vector,
            k: 20,
        };
        let dense_top20 = provider.search(input).expect("dense search ok");
        dense_hits20 += dense_top20
            .iter()
            .take(20)
            .filter(|h| case.correct.contains(&h.id))
            .count();

        let sparse_top20 = sparse_index
            .search_within(&case.query_text, 20, &visible_ids)
            .expect("sparse search ok");
        sparse_hits20 += sparse_top20
            .iter()
            .take(20)
            .filter(|d| case.correct.contains(&d.doc_id))
            .count();
    }

    (dense_hits20, sparse_hits20)
}

/// [`SearchFixture::build`] → [`measure_recall_against`] の薄いラッパ。索引・
/// ベクトル配列を使い回さない 1 回限りの測定呼び出し（既存の層 A・小規模段層 B の
/// 呼び出しが使う）に用いる。
fn measure_recall_with(docs: &[Doc], qa: &[QaCase], source: QuerySource<'_>) -> RecallResult {
    let fixture = SearchFixture::build(docs);
    measure_recall_against(&fixture, qa, source)
}

/// [`measure_recall_with`] の展開なし（[`QuerySource::Baseline`]）の薄いラッパ。
/// 既存呼び出し（層 A 固定値回帰・小規模段層 B）は無変更でこちらを使い続ける。
fn measure_recall(docs: &[Doc], qa: &[QaCase]) -> RecallResult {
    measure_recall_with(docs, qa, QuerySource::Baseline)
}

/// `MIN_INDEXED_ROWS`（`sql::hnsw_cache.rs` の非公開定数）の値。ANN opt-in が
/// 実際に索引を構築する下限行数（Issue #412）。当該定数自体は `pub(crate)` の
/// ため結合テストから参照できず、`tests/hnsw_cache.rs`・`tests/recall_engine_
/// fixture.rs` と同じ値をここへ複製する。
const MIN_INDEXED_ROWS: usize = 1_024;

/// ANN opt-in（[`RecallEngine::Hnsw`]）測定経路（Issue #412）。[`SqlHybridFixture`]
/// （SQL 表層 `EngineCore::execute_sql` 経由。既定エンジン測定が使う
/// [`measure_recall_against`]（in-memory `hybrid_search` 直接呼び出し）とは別の
/// production 経路）で Top-100 を取得し、[`RecallResult`] と同じ分母規約
/// （`ceil20`/`ceil100`）で集計する。`docs.len() >= MIN_INDEXED_ROWS` の場合は
/// 非 vacuous（実際に索引構築・hybrid 密側再取得ループが索引経路を通ったこと）
/// を、未満の場合は逆に索引が構築されていないこと（構造的に brute-force。
/// ANN 通過とは数えない）を固定する。
fn measure_recall_via_hnsw(
    docs: &[Doc],
    qa: &[QaCase],
    source: QuerySource<'_>,
    engine: RecallEngine,
) -> (RecallResult, AnnStats) {
    let dim = docs.first().map_or(0, |d| d.vector.len());
    let rows: Vec<(u64, Vec<f32>, String)> = docs
        .iter()
        .map(|d| (d.id, d.vector.clone(), d.text.clone()))
        .collect();
    let fixture = SqlHybridFixture::new(dim as u32, &rows, engine);

    let mut total_correct = 0usize;
    let mut hits20 = 0usize;
    let mut hits100 = 0usize;
    let mut ceil20 = 0usize;
    let mut ceil100 = 0usize;

    for case in qa {
        total_correct += case.correct.len();
        ceil20 += case.correct.len().min(20);
        ceil100 += case.correct.len().min(100);

        let (query_text, query_vector): (String, Vec<f32>) = match &source {
            QuerySource::Baseline => (case.query_text.clone(), case.query_vector.clone()),
            QuerySource::Expanded(planner) => {
                expand_and_reconstruct_with(planner, dim, &case.query_text)
            }
        };

        let hits = fixture.hybrid_top(&query_vector, &query_text, 100);
        hits20 += hits
            .iter()
            .take(20)
            .filter(|(id, _)| case.correct.contains(id))
            .count();
        hits100 += hits
            .iter()
            .filter(|(id, _)| case.correct.contains(id))
            .count();
    }

    fixture.assert_ann_non_vacuous(docs.len() >= MIN_INDEXED_ROWS);
    let stats = fixture.stats();
    (
        RecallResult {
            total_correct,
            hits20,
            hits100,
            ceil20,
            ceil100,
        },
        stats,
    )
}

/// 非 vacuous 統計を数値を含まない形式でログへ出す（`gate` はテスト名。
/// spec-confidentiality の許可範囲内で、閾値・実測 Recall は含めない）。
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

/// コーパスが `sparse.rs` の各上限に収まることを検証する（健全性チェック。テスト
/// ハーネス自身にも「無制限なコーパス生成を許さない」設計指針を適用する）。
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

// ---------- 層 A: 小規模段（数百件オーダ。SEARCH-1 対応。固定値回帰トラッキング） ----------

const SMALL_NUM_DOCS: usize = 400;
const SMALL_NUM_QUERIES: usize = 60;
const SMALL_VOCAB_SIZE: usize = 60;
const SMALL_SEED: u64 = 0x5EED_0104_5341_1101;

/// TASK-104（SEARCH-1）: 小規模コーパス（数百件オーダ）での Recall@20 を実測する。
///
/// 決定的コーパス・QA セットのため実測値は再現可能であり、下部の固定値アサーションで
/// 回帰トラッキングする。検索カーネル（トークナイザ・BM25・RRF 融合・密検索 provider）
/// への変更で数値が変化した場合はこのテストが失敗する。
#[test]
fn hybrid_recall_small_scale_regression() {
    let verbose = verbose_requested_from_env();
    let (docs, qa) = generate_corpus(
        SMALL_SEED,
        SMALL_NUM_DOCS,
        SMALL_NUM_QUERIES,
        SMALL_VOCAB_SIZE,
    );
    assert_corpus_within_limits(&docs);
    assert!(!qa.is_empty());
    for case in &qa {
        assert!(!case.correct.is_empty());
    }
    // QA 件数（[`generate_qa_set`] の語ペア重複除外後）も決定的コーパスに対しては
    // 固定値であり、フィクスチャが実質的に縮退していないことの回帰トラッキングを兼ねる。
    assert_eq!(qa.len(), 60, "重複除外後の QA 件数が変化した");

    let r = measure_recall(&docs, &qa);
    if verbose {
        println!(
            "=== TASK-104 小規模段 Recall（docs={} queries={} total_correct={}） ===",
            docs.len(),
            qa.len(),
            r.total_correct
        );
        println!(
            "Recall@20={:.4} ({}/{} of ceil20; total_correct={})",
            r.recall20(),
            r.hits20,
            r.ceil20,
            r.total_correct
        );
    }

    // 疎（テキスト）・密（ベクトル）の各チャネルは正解トピック集合の非完全な観測
    // （[`generate_corpus`] のドロップアウト／デコイ）であるため、Recall@20 は 1.0
    // （理論上限 `ceil20` への 100% 到達）に張り付かない。`total_correct`/`ceil20`/
    // `hits20` を固定値で回帰トラッキングする（検索カーネルやフィクスチャの変更で
    // 数値が変化した場合はこのテストが失敗する。数値基準・実測値の public 記載は
    // オーナー判断で許可済み・`.claude/rules/spec-confidentiality.md` 参照）。
    // Issue #310（RRF 融合の同点順位規約 `TieRank::GroupEnd`・密プール境界の同点
    // グループ完全化）適用前の `hits20` は 171（`docs/design/
    // hybrid-recall-regression.md`「Issue #310: engine 側改善」節参照）。
    // Issue #320 大規模段追加調査で導入した非正スコア候補の順位付け除外
    // （`hybrid.rs::trim_non_positive_score_tail`）は、取得列が exhaustive
    // （可視集合全体を覆い切っている）場合には適用しない契約へ改めた
    // （`hybrid.rs::resolve_boundary_tie_group` 参照。`sql_precision_mode` の
    // 密・疎順位不一致 → RRF 同点 → 空集合契約〔SEARCH-9〕がスコア 0 の候補にも
    // 依存するため）。この小規模コーパスでは取得列が exhaustive になるケースが
    // 多く、非正スコア候補が除外対象から外れた結果 `hits20` は 180 から 182 へ
    // 戻った。さらに Issue #320 codex-review P1 指摘対応で、非 exhaustive 時の
    // 除外自体を疎（BM25）チャネル限定へ改めた（密の 0・負値は有効な相対順位
    // であり無シグナルではないため）。この変更では本フィクスチャの `hits20` は
    // 変化しない。
    assert_eq!(r.total_correct, 202, "正解集合の総数が変化した");
    assert_eq!(r.ceil20, 202, "Recall@20 の理論上限が変化した");
    assert_eq!(r.hits20, 182, "小規模段の Recall@20 hit 数が変化した");

    // Issue #307（SEARCH-1）: 密単体・疎単体チャネルの Recall@20 を実測し、
    // 融合が両単体のいずれも下回らないことを関係アサーションとして回帰
    // トラッキングする。加えて両チャネルの hit 数自体も固定値で記録する
    // （`docs/design/hybrid-recall-regression.md` 参照）。両チャネルとも 0 件
    // ヒットでは「下回らない」が自明に成立してしまう（vacuous pass）ため、
    // 正解を一部でも拾えていることも合わせて確認する。
    let (dense_hits20, sparse_hits20) = measure_channel_recall20(&docs, &qa);
    if verbose {
        println!(
            "channel Recall@20: dense={dense_hits20}/{} sparse={sparse_hits20}/{} fused={}/{}",
            r.ceil20, r.ceil20, r.hits20, r.ceil20
        );
    }
    assert_eq!(dense_hits20, 151, "密単体の Recall@20 hit 数が変化した");
    assert_eq!(sparse_hits20, 166, "疎単体の Recall@20 hit 数が変化した");
    assert!(
        dense_hits20 > 0 && sparse_hits20 > 0,
        "密単体・疎単体のいずれかが Recall@20 で正解を 1 件も拾えていない（比較が vacuous pass になる）"
    );
    assert!(
        r.hits20 >= dense_hits20.max(sparse_hits20),
        "融合が密単体・疎単体いずれかを下回った（実測値どうしの関係。理論保証ではない）"
    );
}

// ---------- 層 A: 大規模段（数万件オーダ。SEARCH-2 対応。固定値回帰トラッキング） ----------

const LARGE_NUM_DOCS: usize = 20_000;
const LARGE_NUM_QUERIES: usize = 100;
const LARGE_VOCAB_SIZE: usize = 800;
const LARGE_SEED: u64 = 0x5EED_0104_4C41_5247;

/// TASK-104（SEARCH-2）: 大規模コーパス（数万件オーダ）での Recall@20・Recall@100 を
/// 実測する。デバッグビルドでの実行時間を計測した上で PR CI（`cargo test`）に含めている
/// （`ParallelSearchProvider` の並列化・低次元密ベクトルにより実測上許容範囲。実行時間が
/// 悪化した場合は本テストを `#[ignore]` 側へ移し `.github/workflows/recall.yml` 専用に
/// する判断へ切り替えること）。
#[test]
fn hybrid_recall_large_scale_regression() {
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

    // Issue #306: 展開なし（baseline）・展開あり（[`QuerySource::Expanded`]の
    // パススルー等式ガード）の両方を同一コーパスに対して測るため、疎索引・密
    // ベクトル配列（[`SearchFixture`]）を 1 回だけ構築して使い回す。
    let fixture = SearchFixture::build(&docs);
    let r = measure_recall_against(&fixture, &qa, QuerySource::Baseline);
    if verbose {
        println!(
            "=== TASK-104 大規模段 Recall（docs={} queries={} total_correct={}） ===",
            docs.len(),
            qa.len(),
            r.total_correct
        );
        println!(
            "Recall@20={:.4} ({}/{} of ceil20)  Recall@100={:.4} ({}/{} of ceil100; total_correct={})",
            r.recall20(),
            r.hits20,
            r.ceil20,
            r.recall100(),
            r.hits100,
            r.ceil100,
            r.total_correct
        );
    }

    // `hybrid_recall_small_scale_regression` と同じ理由（[`generate_corpus`] の
    // lossy view）で Recall@20/Recall@100 は 1.0 に張り付かない。`hits`/`ceil`/
    // `total_correct` を固定値で回帰トラッキングする（数値基準・実測値の public
    // 記載はオーナー判断で許可済み・`.claude/rules/spec-confidentiality.md`
    // 参照）。Issue #310 適用前の `hits20`/`hits100` はそれぞれ 328/645
    // （`docs/design/hybrid-recall-regression.md`「Issue #310: engine 側改善」
    // 節参照）。
    assert_eq!(r.total_correct, 997, "正解集合の総数が変化した");
    assert_eq!(r.ceil20, 421, "Recall@20 の理論上限が変化した");
    assert_eq!(r.ceil100, 707, "Recall@100 の理論上限が変化した");
    assert_eq!(r.hits20, 385, "大規模段の Recall@20 hit 数が変化した");
    assert_eq!(r.hits100, 648, "大規模段の Recall@100 hit 数が変化した");

    // Issue #306: 大規模段層 B（[`hybrid_recall_large_scale_threshold_gate`]）が
    // 使う展開あり経路（[`QuerySource::Expanded`]・[`MockLlmClient`]）のパススルー
    // 回帰ガード。QA は `direct` カテゴリ（`kw_XXXX`）のみで `MockLlmClient` は
    // その語を無変換で通すため、展開ありの測定値は展開なしと理論上完全一致する
    // （`query_planning_recall.rs` の同種アサーション「direct カテゴリで展開ありが
    // 展開なしと一致」と同じ性質）。この等式が崩れた場合は展開パーサ・スタブ・
    // 再構成経路のいずれかの回帰であり、層 B の測定前提が壊れていることを示す。
    let planner_fixture = PlannerFixture::new();
    let r_exp = measure_recall_against(&fixture, &qa, QuerySource::Expanded(&planner_fixture));
    assert_eq!(
        r_exp.hits20, r.hits20,
        "展開あり経路（MockLlmClient）の Recall@20 hit 数が展開なしと一致しなかった（パススルー性質が崩れた）"
    );
    assert_eq!(
        r_exp.hits100, r.hits100,
        "展開あり経路（MockLlmClient）の Recall@100 hit 数が展開なしと一致しなかった（パススルー性質が崩れた）"
    );
    assert_eq!(
        r_exp.ceil20, r.ceil20,
        "展開あり経路（MockLlmClient）の Recall@20 理論上限が展開なしと一致しなかった"
    );
    assert_eq!(
        r_exp.ceil100, r.ceil100,
        "展開あり経路（MockLlmClient）の Recall@100 理論上限が展開なしと一致しなかった"
    );
}

/// [`ParallelSearchProvider`]（production 実装）へ委譲しつつ、境界同点グループ
/// 完全化（`hybrid.rs` 内部実装）の再取得ループが要求する `SearchInput::k`
/// （＝密側 `fetch_k`）の最大値をクエリ単位で観測する診断用ラッパ（Issue #320
/// 大規模段追加調査）。`hybrid_search` は `SearchProvider` を `&dyn` で受け取る
/// ため、密側の再取得進行はこのラッパ経由でのみテストから観測できる
/// （`hybrid.rs::MAX_FETCH_K` は `pub(crate)` のため本クレート外の統合テストからは
/// 参照できない。可視集合サイズ [`LARGE_NUM_DOCS`] への到達有無で代用する:
/// 本フィクスチャでは `MAX_FETCH_K`〔`MAX_POOL_DEPTH * 4` = 400,000〕が可視集合
/// サイズ〔20,000〕を上回るため、再取得ループの実質的な上限は可視集合サイズで
/// 頭打ちになる）。挙動そのものは変えない（`search` は無条件に `inner` へ委譲）。
struct MaxKTrackingProvider {
    inner: ParallelSearchProvider,
    max_k_seen: std::sync::atomic::AtomicUsize,
}

impl MaxKTrackingProvider {
    fn new() -> Self {
        Self {
            inner: ParallelSearchProvider,
            max_k_seen: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    fn reset(&self) {
        self.max_k_seen
            .store(0, std::sync::atomic::Ordering::Relaxed);
    }

    fn max_k_seen(&self) -> usize {
        self.max_k_seen.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl SearchProvider for MaxKTrackingProvider {
    fn search(
        &self,
        input: SearchInput<'_>,
    ) -> Result<Vec<engine::kernel::CandidateHit>, engine::kernel::KernelError> {
        self.max_k_seen
            .fetch_max(input.k, std::sync::atomic::Ordering::Relaxed);
        self.inner.search(input)
    }
}

/// TASK-104（SEARCH-2）Issue #320 codex-review P1 指摘対応の直接固定:
/// 非正スコア（無シグナル）候補の境界同点グループ完全化からの除外
/// （`hybrid.rs::trim_non_positive_score_tail`）は疎（BM25）チャネル限定へ
/// 改めたため、密側の再取得ループが可視集合全体（[`LARGE_NUM_DOCS`] =
/// 20,000 件）まで到達するクエリが 0 件でなくなること自体は許容する
/// （密の 0・負値も有効な相対順位であり、除外すると recall が落ちるため。
/// `resolve_boundary_tie_group` ドキュメント参照）。本テストは代わりに
/// 密側の再取得ループが常に可視集合サイズ（このフィクスチャでは
/// `MAX_FETCH_K` > `LARGE_NUM_DOCS` のため実質的な上限＝
/// `dense_cap = MAX_FETCH_K.min(visible_ids.len())`）を超えて要求しない
/// （＝再取得ループが無限に伸び続けない）ことを固定する。
#[test]
fn hybrid_recall_large_scale_dense_refetch_is_bounded_by_visible_set_size() {
    let verbose = verbose_requested_from_env();
    let (docs, qa) = generate_corpus(
        LARGE_SEED,
        LARGE_NUM_DOCS,
        LARGE_NUM_QUERIES,
        LARGE_VOCAB_SIZE,
    );
    assert_corpus_within_limits(&docs);

    let fixture = SearchFixture::build(&docs);
    let provider = MaxKTrackingProvider::new();
    let cfg = RrfConfig::default();
    let dim = fixture.dim as u32;

    let mut reached_visible_set = 0usize;
    let mut max_k_across_queries = 0usize;
    for case in &qa {
        provider.reset();
        let input = SearchInput {
            ids: &fixture.ids,
            vectors: &fixture.vectors,
            dim,
            query: &case.query_vector,
            k: 20,
        };
        let _ = hybrid_search(
            &provider,
            input,
            &fixture.sparse_index,
            &case.query_text,
            20,
            &cfg,
        )
        .expect("hybrid search ok");
        let max_k = provider.max_k_seen();
        max_k_across_queries = max_k_across_queries.max(max_k);
        if max_k >= LARGE_NUM_DOCS {
            reached_visible_set += 1;
        }
    }

    if verbose {
        println!(
            "=== Issue #320 大規模段 密側再取得診断（docs={} queries={}） ===",
            docs.len(),
            qa.len()
        );
        println!(
            "dense fetch_k max across queries={max_k_across_queries}  \
             queries reaching visible set size ({LARGE_NUM_DOCS})={reached_visible_set}"
        );
    }

    assert!(
        max_k_across_queries <= LARGE_NUM_DOCS,
        "密側の再取得ループが可視集合サイズ（実質的な上限 dense_cap）を超えて \
         fetch_k を要求した（`dense_cap = MAX_FETCH_K.min(input.ids.len())` の \
         算出自体が壊れていない限り本来到達しない構造的な保証の固定）"
    );
    // 決定的コーパス・QA セットに対する実測値を固定値で回帰トラッキングする
    // （本ハーネスの他アサーションと同じ方針。codex-review P1 指摘対応前は
    // 密チャネルの非正スコア除外により 0 件、Issue #310 時点（境界完全化導入
    // 前）は 53 件だった）。
    assert_eq!(
        reached_visible_set, 40,
        "密チャネルの再取得ループが可視集合全体まで到達したクエリ数が変化した"
    );
}

// ---------- 層 B: spec 閾値ゲート（`#[ignore]`。`make recall-regression` 専用） ----------

/// `HYBRID_RECALL_MIN_*` 環境変数（`(0.0, 1.0]` の浮動小数点）の解決結果。
enum GateThreshold {
    /// 環境変数が未設定、または GitHub Actions の未設定 repo/environment variable が
    /// 解決する空文字列（`.github/workflows/bench.yml` の `BENCH_MAX_P95_MS` 等と
    /// 同じ経路。`crates/engine/benches/simd_bench.rs::min_recall_from_env`
    /// 冒頭コメント参照）。
    NotConfigured,
    /// 設定済みで `(0.0, 1.0]` の範囲内。この場合のみ実測値と比較する。
    Value(f64),
}

/// `HYBRID_RECALL_MIN_*` 環境変数を読み取る。未設定・空文字列は
/// [`GateThreshold::NotConfigured`]、非数値・範囲外は fail-closed（`Err`。呼び出し側で
/// テスト失敗として扱う）、それ以外は [`GateThreshold::Value`] を返す。数値そのもの
/// （spec の Recall 下限）はこのファイル・ログのいずれにもハードコードしない
/// （`.claude/rules/spec-confidentiality.md`）。
fn recall_threshold_from_env(var: &str) -> Result<GateThreshold, String> {
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
    if !(value > 0.0 && value <= 1.0) {
        return Err(format!("{var} must be within (0.0, 1.0]"));
    }
    Ok(GateThreshold::Value(value))
}

/// `HYBRID_RECALL_REQUIRE_THRESHOLDS` 環境変数（`"1"` のときのみ true）。
/// `.github/workflows/recall.yml` からの実行（dispatch / schedule）時のみ
/// 注入される strict モードフラグ（PR #147 codex-review P1 継続指摘対応）。
///
/// strict モードが無効（ローカル `make recall-regression`・仮に PR 経由で
/// `--ignored` を明示指定して実行した場合）は、閾値未設定を「対象外」として
/// 明示的に成功終了する opt-in 挙動を維持する
/// （`simd_bench.rs::core5_requested_from_env` と同型）。
///
/// strict モードが有効な場合は未設定を「一度も評価していない run」とみなし、
/// 非数値・範囲外と同様に fail-closed でテスト失敗とする。これにより
/// environment 作成漏れ・variable 名の打ち間違い・variable の誤削除で
/// `HYBRID_RECALL_MIN_*` が読めなくなった週次 run が、実際に基準を満たした
/// run と同じ green として埋もれる事故を防ぐ（PR #147 codex-review P1 指摘対応）。
fn strict_thresholds_required() -> bool {
    std::env::var("HYBRID_RECALL_REQUIRE_THRESHOLDS")
        .map(|v| v.trim() == "1")
        .unwrap_or(false)
}

/// [`recall_threshold_from_env`] を読み取り、[`GateThreshold::NotConfigured`] を
/// strict モード（[`strict_thresholds_required`]）に応じて分岐させる共通ヘルパ。
/// strict モード有効時の未設定は fail-closed（`panic!`）、無効時は `None`
/// （呼び出し側で「対象外」を出力して early return する）。非数値・範囲外は
/// strict モードの有無によらず常に fail-closed とする。
fn resolve_gate_threshold(var: &str) -> Option<f64> {
    match recall_threshold_from_env(var) {
        Ok(GateThreshold::Value(v)) => Some(v),
        Ok(GateThreshold::NotConfigured) => {
            if strict_thresholds_required() {
                panic!(
                    "{var} is not configured but HYBRID_RECALL_REQUIRE_THRESHOLDS=1 (strict mode: this run must evaluate all HYBRID_RECALL_MIN_* thresholds; see .github/workflows/recall.yml and the recall-gate environment variables)"
                );
            }
            None
        }
        Err(msg) => panic!("{var} invalid: {msg}"),
    }
}

// ---------- 実測値の既定非出力（Issue #303）。`RECALL_VERBOSE` opt-in ゲート ----------

/// `RECALL_VERBOSE` の生値と `GITHUB_ACTIONS` 判定を引数化した純関数（単体テスト可能・
/// 環境変数を直接読まない）。`raw == Some("1")` の厳密一致のみ verbose 要求とみなす
/// （`crates/engine/benches/harness/sql_c1.rs::resolve_verbose` と同じ規約。public
/// ログへの実測値露出を左右する opt-in のため空白付き値・他表記へ寛容に倒さない）。
/// `under_github_actions` が真の場合、verbose 要求は defense-in-depth として `Err` で
/// fail-closed に拒否する（`.github/workflows/recall.yml` は `RECALL_VERBOSE` を
/// 注入しない運用と二重化する。Issue #303・#277〜#279 と同方針）。
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
/// する（各ゲート・層 A 回帰の冒頭、コーパス生成前に呼ぶ）。`GITHUB_ACTIONS` の有無は
/// 値を解釈せず存在有無のみで判定する（`crates/engine/benches/simd_bench.rs::
/// running_under_github_actions` と同方針）。
fn verbose_requested_from_env() -> bool {
    let raw = std::env::var("RECALL_VERBOSE").ok();
    match resolve_verbose(raw.as_deref(), std::env::var_os("GITHUB_ACTIONS").is_some()) {
        Ok(v) => v,
        Err(msg) => panic!("{msg}"),
    }
}

/// 閾値ゲート判定行の描画。`verbose=false`（既定）では `gate`・`metric`・`pass` のみを
/// 含み実測値（`value`）を含めない（PR #224・CORE-5 対応と同方針。pass/fail と実測値の
/// 併記による非公開閾値の逆算を防ぐ）。`verbose=true` では `value=<f64:.4>` を付加する
/// （`crates/engine/benches/harness/sql_c1.rs::render_p95_line` と同型）。
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
            "hybrid_recall_small_scale_threshold_gate",
            "recall@20",
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
            "hybrid_recall_small_scale_threshold_gate",
            "recall@20",
            0.8465,
            true,
            true,
        );
        assert!(line.contains("value=0.8465"));
        assert!(line.contains("pass=true"));
    }
}

/// TASK-104（SEARCH-1）層 B: 小規模段 Recall@20（[`RecallResult::recall20`]。分母は
/// 理論上限 `ceil20`）が `HYBRID_RECALL_MIN_R20_SMALL`（Actions variables 由来）以上
/// であることを確認する閾値ゲート。未設定は既定（非 strict）では「対象外」として
/// 明示的に成功終了し、strict モード（[`strict_thresholds_required`]）では
/// fail-closed でテスト失敗とする。設定済みで非数値・範囲外は常に fail-closed
/// でテスト失敗とする（skip しない）。ログには対象名と pass/fail のみを出力し、
/// 注入された閾値・実測値の数値は出力しない（`RECALL_VERBOSE=1` opt-in 時のみ
/// 実測値を追加出力する。Issue #303・[`render_gate_line`] 参照）。
#[test]
#[ignore = "spec 閾値（Actions variables 由来）が必要なため既定では実行しない。make recall-regression で実行する"]
fn hybrid_recall_small_scale_threshold_gate() {
    let verbose = verbose_requested_from_env();
    let min_r20 = match resolve_gate_threshold("HYBRID_RECALL_MIN_R20_SMALL") {
        Some(v) => v,
        None => {
            println!(
                "hybrid_recall_small_scale_threshold_gate: HYBRID_RECALL_MIN_R20_SMALL not configured; gate not enabled (explicit no-op, not a failure)"
            );
            return;
        }
    };

    let (docs, qa) = generate_corpus(
        SMALL_SEED,
        SMALL_NUM_DOCS,
        SMALL_NUM_QUERIES,
        SMALL_VOCAB_SIZE,
    );
    // ANN opt-in（Issue #412・R1）。`SMALL_NUM_DOCS`（400）は `MIN_INDEXED_ROWS`
    // （1,024）未満のため、`RecallEngine::Hnsw` を指定しても構造的に brute-force
    // のまま（`measure_recall_via_hnsw` が非 vacuous 検証で `builds == 0` を固定
    // する）。ANN 通過とは数えないが、SQL 表層経由の測定経路自体は既定と同じ
    // ように検証できる。
    let engine = RecallEngine::from_env();
    let r = match engine {
        RecallEngine::BruteForce => measure_recall(&docs, &qa),
        RecallEngine::Hnsw | RecallEngine::HnswF16 | RecallEngine::HnswI8 => {
            let (r, stats) = measure_recall_via_hnsw(&docs, &qa, QuerySource::Baseline, engine);
            print_ann_stats("hybrid_recall_small_scale_threshold_gate", engine, &stats);
            r
        }
    };
    let recall20 = r.recall20();
    let pass = recall20 >= min_r20;

    println!(
        "hybrid_recall_small_scale_threshold_gate: engine={}",
        engine.token()
    );
    println!(
        "{}",
        render_gate_line(
            "hybrid_recall_small_scale_threshold_gate",
            "recall@20",
            recall20,
            pass,
            verbose
        )
    );
    assert!(
        pass,
        "small-scale Recall@20 below HYBRID_RECALL_MIN_R20_SMALL"
    );
}

/// TASK-104（SEARCH-2）層 B: 大規模段 Recall@20・Recall@100 がそれぞれ
/// `HYBRID_RECALL_MIN_R20_LARGE`・`HYBRID_RECALL_MIN_R100_LARGE` 以上であることを
/// 確認する閾値ゲート。契約は [`hybrid_recall_small_scale_threshold_gate`] と同一だが、
/// 2 つの下限を独立に解決する（片方のみ設定済みの場合は設定済みの側だけを判定し、
/// 未設定側は既定では「対象外」を出力する。両方未設定かつ非 strict の場合のみ
/// コーパス生成前に早期 return して成功終了する。strict モードでは
/// [`resolve_gate_threshold`] が未設定を検出した時点で fail-closed になる）。
///
/// Issue #306: 測定は展開あり経路（[`QuerySource::Expanded`]・[`MockLlmClient`]。
/// TASK-110〜113 の決定的スタブ `LlmClient` による production のクエリ展開経路）で
/// 行い、SEARCH-2 の測定前提（クエリ展開あり）に揃える。fixture パラメータ・
/// 閾値・環境変数名・出力形式は変更しない。
///
/// codex-review P1 対応（Issue #306・PR #308。2 段構成）:
/// 1. QA を `direct` カテゴリのまま（`kw_XXXX` 形式）展開経路へ渡すと
///    `MockLlmClient` は無変換で通し、実測値が展開なしと構造的に一致してしまう
///    問題を [`to_intent_query`] で解消する。`intent` 形（`syn_XXXX` 形式。
///    コーパス語彙とは一致しない）へ書き換えたクエリを使うことで、
///    `MockLlmClient` の同義語写像（展開結果を実際の検索入力へ反映する経路）を
///    経由して初めて Recall が出る非恒等なゲートにする。正解集合・fixture
///    パラメータ・閾値は `direct` カテゴリのものをそのまま使う
///    （[`to_intent_query`] は `query_text` のみを書き換え、`correct` は
///    変更しない）ため、既存の `HYBRID_RECALL_MIN_R20_LARGE`/
///    `HYBRID_RECALL_MIN_R100_LARGE` の較正・意味は変わらない。
/// 2. (1) は展開経路自体（[`expand_and_reconstruct_with`]）が
///    `query_planner::render_full_prompt`/`parse_expansion` を固定接頭辞 `""` で
///    直接呼ぶ手組みのままだったため、`EngineCore::plan_query`・
///    `with_query_planner`・辞書スナップショットという production の結線自体が
///    欠落・破損しても検出できない継続指摘を受けた（[`PlannerFixture`] の
///    ドキュメント参照）。展開経路を [`PlannerFixture`] 経由の
///    [`EngineCore::plan_query`] へ置き換え、辞書スナップショットに含めた
///    センチネル（[`DICTIONARY_WIRING_SENTINEL_PATH`]）が展開時のプロンプトに
///    現れない場合は展開自体を `Err` で失敗させることで、(1)（`syn_XXXX` の
///    同義語写像）と (2)（`plan_query`/`with_query_planner`/辞書スナップショット
///    の結線）の両方が壊れれば Recall 崩壊・テスト失敗する二重の非恒等ゲートに
///    なる。小規模段の層 B・層 A の主測定は引き続き展開なし
///    （[`QuerySource::Baseline`]）で行う。層 A のパススルー等式ガード
///    （[`hybrid_recall_large_scale_regression`]）は `direct` 形クエリのままの
///    展開あり経路を検証対象とし続けるため無変更。
#[test]
#[ignore = "spec 閾値（Actions variables 由来）が必要なため既定では実行しない。make recall-regression で実行する"]
fn hybrid_recall_large_scale_threshold_gate() {
    let verbose = verbose_requested_from_env();
    let min_r20 = resolve_gate_threshold("HYBRID_RECALL_MIN_R20_LARGE");
    let min_r100 = resolve_gate_threshold("HYBRID_RECALL_MIN_R100_LARGE");

    if min_r20.is_none() && min_r100.is_none() {
        println!(
            "hybrid_recall_large_scale_threshold_gate: HYBRID_RECALL_MIN_R20_LARGE/HYBRID_RECALL_MIN_R100_LARGE not configured; gate not enabled (explicit no-op, not a failure)"
        );
        return;
    }

    // 数値を含まない固定文言（実測値・閾値は含めない。spec-confidentiality P0）。
    println!(
        "hybrid_recall_large_scale_threshold_gate: query expansion=enabled, non-identity intent-form queries (deterministic stub LlmClient, Issue #306 / PR #308)"
    );

    let (docs, qa) = generate_corpus(
        LARGE_SEED,
        LARGE_NUM_DOCS,
        LARGE_NUM_QUERIES,
        LARGE_VOCAB_SIZE,
    );
    let intent_qa: Vec<QaCase> = qa.iter().map(to_intent_query).collect();
    let planner_fixture = PlannerFixture::new();
    // ANN opt-in（Issue #412・R1）。`LARGE_NUM_DOCS`（20,000）は
    // `MIN_INDEXED_ROWS`（1,024）を大きく上回るため、`RecallEngine::Hnsw` は
    // 実際に索引を構築する（`measure_recall_via_hnsw` の非 vacuous 検証で
    // `builds >= 1` を固定）。`build_parallel` はこの規模では並列構築される
    // ため索引グラフ自体は run-to-run で変わりうるが、本ゲートは固定値では
    // なく閾値比較のみを判定するため影響しない。
    let engine = RecallEngine::from_env();
    let r = match engine {
        RecallEngine::BruteForce => {
            measure_recall_with(&docs, &intent_qa, QuerySource::Expanded(&planner_fixture))
        }
        RecallEngine::Hnsw | RecallEngine::HnswF16 | RecallEngine::HnswI8 => {
            let (r, stats) = measure_recall_via_hnsw(
                &docs,
                &intent_qa,
                QuerySource::Expanded(&planner_fixture),
                engine,
            );
            print_ann_stats("hybrid_recall_large_scale_threshold_gate", engine, &stats);
            r
        }
    };
    println!(
        "hybrid_recall_large_scale_threshold_gate: engine={}",
        engine.token()
    );
    let recall20 = r.recall20();
    let recall100 = r.recall100();

    let mut pass = true;
    match min_r20 {
        Some(min) => {
            let pass20 = recall20 >= min;
            pass &= pass20;
            println!(
                "{}",
                render_gate_line(
                    "hybrid_recall_large_scale_threshold_gate",
                    "recall@20",
                    recall20,
                    pass20,
                    verbose
                )
            );
        }
        None => {
            println!(
                "hybrid_recall_large_scale_threshold_gate: HYBRID_RECALL_MIN_R20_LARGE not configured; sub-check not enabled"
            );
        }
    }
    match min_r100 {
        Some(min) => {
            let pass100 = recall100 >= min;
            pass &= pass100;
            println!(
                "{}",
                render_gate_line(
                    "hybrid_recall_large_scale_threshold_gate",
                    "recall@100",
                    recall100,
                    pass100,
                    verbose
                )
            );
        }
        None => {
            println!(
                "hybrid_recall_large_scale_threshold_gate: HYBRID_RECALL_MIN_R100_LARGE not configured; sub-check not enabled"
            );
        }
    }

    assert!(
        pass,
        "large-scale Recall@20/Recall@100 below configured HYBRID_RECALL_MIN_* threshold"
    );
}
