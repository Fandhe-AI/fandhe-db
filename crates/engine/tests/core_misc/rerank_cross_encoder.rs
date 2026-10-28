//! Issue #333（SEARCH-7 方式変更）受け入れ条件 2:
//! `CrossEncoderReranker`（`crates/engine/src/rerank.rs`）の単体テストをスタブ推論
//! バックエンドで検証する。`cross-encoder` feature（`ort`/`tokenizers` 依存）に
//! 依存しない決定的スタブのみを使うため、feature 無効の既定ビルドでも常時
//! `cargo test` の対象になる（受け入れ条件 1 の「feature 無効時に従来どおり通る」を
//! 満たしたまま、方式本体の契約を feature の有無に関わらず回帰検証できる）。
//!
//! 検証する契約:
//! - 順序決定性: 同一入力を 2 回渡しても同一出力（`rerank.rs::tests` の最小限の
//!   スタブテストより広く、バッチ分割を跨いだ場合も含めて検証する）
//! - 有界化: `max_candidates` 超過拒否・バッチ分割（1 呼び出し当たりの passage 数
//!   ≤ `batch_size`）
//! - エラー契約: 長さ不一致・非有限スコア・バックエンド失敗はいずれも
//!   `rerank_candidates` 経由でも部分受理されず検索全体を拒否する（fail-closed）

use std::sync::{Arc, Mutex};

use engine::rerank::{
    rerank_candidates, CrossEncoderBackend, CrossEncoderConfig, CrossEncoderError,
    CrossEncoderReranker, RerankCandidate, RerankConfig, RerankError,
};

fn cand<'a>(id: u64, fused_score: f64, text: &'a str) -> RerankCandidate<'a> {
    RerankCandidate {
        id,
        fused_score,
        text,
    }
}

/// バッチ分割の呼び出し回数・各呼び出しの passage 数を記録しつつ、決定的スコア
/// （passage の長さ）を返すスタブ。呼び出し履歴は `Arc<Mutex<Vec<usize>>>`
/// （`CrossEncoderBackend: Send + Sync` を満たす共有可変状態）に記録し、バックエンド
/// 自体を `CrossEncoderReranker` へ move した後もテスト側が同じ `Arc` のクローンから
/// 履歴を読めるようにする（`unsafe impl Sync` を書かずに済ませる。coding-rust.md
/// 「unsafe は原則禁止」）。
struct CallCountingBackend {
    call_lengths: Arc<Mutex<Vec<usize>>>,
}

impl CallCountingBackend {
    fn new() -> (Self, Arc<Mutex<Vec<usize>>>) {
        let call_lengths = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                call_lengths: call_lengths.clone(),
            },
            call_lengths,
        )
    }
}

impl CrossEncoderBackend for CallCountingBackend {
    fn score_pairs(&self, _query: &str, passages: &[&str]) -> Result<Vec<f64>, CrossEncoderError> {
        self.call_lengths
            .lock()
            .expect("mutex not poisoned")
            .push(passages.len());
        Ok(passages.iter().map(|p| p.len() as f64).collect())
    }

    fn max_seq_len(&self) -> usize {
        512
    }
}

#[test]
fn cross_encoder_reranker_splits_candidates_into_batches_within_batch_size() {
    let cfg = CrossEncoderConfig::new(4, 200, 512).unwrap();
    let (backend, call_lengths) = CallCountingBackend::new();
    let reranker = CrossEncoderReranker::new(backend, cfg).unwrap();

    // 10 candidates, batch_size = 4 → 呼び出しは 3 回（4, 4, 2）で、いずれも
    // batch_size を超えない。
    let candidates: Vec<RerankCandidate<'_>> =
        (0..10).map(|i| cand(i, (10 - i) as f64, "abc")).collect();
    let rerank_cfg = RerankConfig::new(200, 20).unwrap();
    let hits = rerank_candidates(&reranker, "q", &candidates, &rerank_cfg).expect("ok");
    assert_eq!(hits.len(), 10);

    let lengths = call_lengths.lock().expect("mutex not poisoned").clone();
    assert_eq!(lengths, vec![4, 4, 2]);
    assert!(lengths.iter().all(|&len| len <= 4));
}

#[test]
fn cross_encoder_reranker_is_deterministic_across_repeated_calls() {
    let cfg = CrossEncoderConfig::new(8, 200, 512).unwrap();
    let (backend, _call_lengths) = CallCountingBackend::new();
    let reranker = CrossEncoderReranker::new(backend, cfg).unwrap();
    let candidates: Vec<RerankCandidate<'_>> = vec![
        cand(1, 3.0, "short"),
        cand(2, 2.0, "a much longer passage text"),
        cand(3, 1.0, "medium length passage"),
    ];
    let rerank_cfg = RerankConfig::default();
    let first = rerank_candidates(&reranker, "q", &candidates, &rerank_cfg).expect("ok");
    let second = rerank_candidates(&reranker, "q", &candidates, &rerank_cfg).expect("ok");
    assert_eq!(first, second);
    // 決定的スコア（長さ）順: id=2（最長）> id=3 > id=1（最短）。
    assert_eq!(
        first.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![2, 3, 1]
    );
}

struct FixedScoreBackend {
    scores: Vec<f64>,
}

impl CrossEncoderBackend for FixedScoreBackend {
    fn score_pairs(&self, _query: &str, passages: &[&str]) -> Result<Vec<f64>, CrossEncoderError> {
        Ok(self.scores.iter().copied().take(passages.len()).collect())
    }

    fn max_seq_len(&self) -> usize {
        512
    }
}

#[test]
fn cross_encoder_reranker_rejects_max_candidates_exceeded_via_rerank_candidates() {
    // `max_candidates` を候補数より小さく設定し、`rerank_candidates`（`pool_depth`
    // は十分大きい）を通過してもなお `CrossEncoderReranker` 自身が拒否することを
    // 固定する。
    let cfg = CrossEncoderConfig::new(8, 2, 512).unwrap();
    let backend = FixedScoreBackend {
        scores: vec![1.0, 2.0, 3.0],
    };
    let reranker = CrossEncoderReranker::new(backend, cfg).unwrap();
    let candidates = [cand(1, 3.0, "a"), cand(2, 2.0, "b"), cand(3, 1.0, "c")];
    let rerank_cfg = RerankConfig::new(200, 20).unwrap();
    let err = rerank_candidates(&reranker, "q", &candidates, &rerank_cfg).unwrap_err();
    assert_eq!(
        err,
        RerankError::CrossEncoder(CrossEncoderError::TooManyCandidates { len: 3, max: 2 })
    );
}

struct LengthMismatchBackend;

impl CrossEncoderBackend for LengthMismatchBackend {
    fn score_pairs(&self, _query: &str, passages: &[&str]) -> Result<Vec<f64>, CrossEncoderError> {
        Ok(vec![0.0; passages.len() + 1])
    }

    fn max_seq_len(&self) -> usize {
        512
    }
}

#[test]
fn cross_encoder_reranker_rejects_backend_length_mismatch_via_rerank_candidates() {
    let cfg = CrossEncoderConfig::new(8, 200, 512).unwrap();
    let reranker = CrossEncoderReranker::new(LengthMismatchBackend, cfg).unwrap();
    let candidates = [cand(1, 2.0, "a"), cand(2, 1.0, "b")];
    let rerank_cfg = RerankConfig::default();
    let err = rerank_candidates(&reranker, "q", &candidates, &rerank_cfg).unwrap_err();
    assert!(matches!(
        err,
        RerankError::CrossEncoder(CrossEncoderError::LengthMismatch {
            expected: 2,
            got: 3
        })
    ));
}

struct NonFiniteBackend;

impl CrossEncoderBackend for NonFiniteBackend {
    fn score_pairs(&self, _query: &str, passages: &[&str]) -> Result<Vec<f64>, CrossEncoderError> {
        let mut v = vec![1.0; passages.len()];
        if let Some(last) = v.last_mut() {
            *last = f64::INFINITY;
        }
        Ok(v)
    }

    fn max_seq_len(&self) -> usize {
        512
    }
}

#[test]
fn cross_encoder_reranker_rejects_backend_non_finite_score_via_rerank_candidates() {
    let cfg = CrossEncoderConfig::new(8, 200, 512).unwrap();
    let reranker = CrossEncoderReranker::new(NonFiniteBackend, cfg).unwrap();
    let candidates = [cand(1, 2.0, "a"), cand(2, 1.0, "b")];
    let rerank_cfg = RerankConfig::default();
    let err = rerank_candidates(&reranker, "q", &candidates, &rerank_cfg).unwrap_err();
    assert_eq!(
        err,
        RerankError::CrossEncoder(CrossEncoderError::NonFiniteScore)
    );
}

struct AlwaysFailingBackend;

impl CrossEncoderBackend for AlwaysFailingBackend {
    fn score_pairs(&self, _query: &str, _passages: &[&str]) -> Result<Vec<f64>, CrossEncoderError> {
        Err(CrossEncoderError::Backend(
            "stub backend intentionally failing".to_string(),
        ))
    }

    fn max_seq_len(&self) -> usize {
        512
    }
}

#[test]
fn cross_encoder_reranker_rejects_whole_search_on_backend_failure() {
    // fail-closed: バッチの一部だけが失敗しても部分受理はせず検索全体を拒否する。
    let cfg = CrossEncoderConfig::new(2, 200, 512).unwrap();
    let reranker = CrossEncoderReranker::new(AlwaysFailingBackend, cfg).unwrap();
    let candidates: Vec<RerankCandidate<'_>> =
        (0..5).map(|i| cand(i, (5 - i) as f64, "x")).collect();
    let rerank_cfg = RerankConfig::default();
    let err = rerank_candidates(&reranker, "q", &candidates, &rerank_cfg).unwrap_err();
    assert!(matches!(
        err,
        RerankError::CrossEncoder(CrossEncoderError::Backend(_))
    ));
}

#[test]
fn cross_encoder_config_new_rejects_out_of_range_values() {
    assert_eq!(
        CrossEncoderConfig::new(0, 200, 512).unwrap_err(),
        CrossEncoderError::InvalidConfig
    );
    assert_eq!(
        CrossEncoderConfig::new(300, 200, 512).unwrap_err(),
        CrossEncoderError::InvalidConfig
    );
    assert_eq!(
        CrossEncoderConfig::new(8, 0, 512).unwrap_err(),
        CrossEncoderError::InvalidConfig
    );
    assert_eq!(
        CrossEncoderConfig::new(8, 200, 0).unwrap_err(),
        CrossEncoderError::InvalidConfig
    );
    assert_eq!(
        CrossEncoderConfig::new(8, 200, 100_000).unwrap_err(),
        CrossEncoderError::InvalidConfig
    );
    assert!(CrossEncoderConfig::new(8, 200, 512).is_ok());
}

#[test]
fn cross_encoder_reranker_output_is_subset_of_input_pool() {
    // SEARCH-6 対応: リランカーはプールを拡張しない（出力 ⊆ 入力）。
    let cfg = CrossEncoderConfig::new(8, 200, 512).unwrap();
    let backend = FixedScoreBackend {
        scores: vec![5.0, 4.0, 3.0, 2.0, 1.0],
    };
    let reranker = CrossEncoderReranker::new(backend, cfg).unwrap();
    let candidates: Vec<RerankCandidate<'_>> =
        (0..5).map(|i| cand(i, (5 - i) as f64, "x")).collect();
    let rerank_cfg = RerankConfig::default();
    let hits = rerank_candidates(&reranker, "q", &candidates, &rerank_cfg).expect("ok");
    let input_ids: std::collections::BTreeSet<u64> = candidates.iter().map(|c| c.id).collect();
    assert!(hits.iter().all(|h| input_ids.contains(&h.id)));
}
