//! `rerank.rs`（TASK-107。対象ビヘイビア: SEARCH-6, SEARCH-7, SEARCH-8）の統合テスト。
//!
//! `tests/hybrid.rs` の流儀（合成コーパス・fail-closed 系・実装差し替え検証）に合わせ、
//! `hybrid_search`（`hybrid.rs`。TASK-103）の出力を `rerank_candidates` へ接続する経路を
//! 検証する。SEARCH-6（候補プール非拡張・既定深さの整合）・SEARCH-7（再順位付けの動作・
//! 決定性・ベースラインとの比較構造）を扱う。SEARCH-8（効果測定の追跡）は本テストの
//! 対象外であり、TASK-108（Issue #39）が効果実測・Recall 回帰の管轄を担う。

use engine::hybrid::{hybrid_search, HybridHit, RrfConfig};
use engine::kernel::{CpuScalarProvider, SearchInput};
use engine::rerank::{
    rerank_candidates, IdentityReranker, LexicalOverlapReranker, RerankCandidate, RerankConfig,
    RerankError, RerankedHit, Reranker,
};
use engine::sparse::SparseIndex;

/// `hybrid_search` の融合ヒットへ、リランカー用の文書テキストを添えて
/// `RerankCandidate` 列へ変換するテストヘルパ（本番コードでは呼び出し元
/// （`core.rs` 相当）が可視行から同様に組み立てる想定）。
fn to_candidates<'a>(hits: &[HybridHit], texts: &'a [(u64, &'a str)]) -> Vec<RerankCandidate<'a>> {
    hits.iter()
        .map(|h| {
            let text = texts
                .iter()
                .find(|(id, _)| *id == h.id)
                .map(|(_, t)| *t)
                .unwrap_or("");
            RerankCandidate {
                id: h.id,
                fused_score: h.score,
                text,
            }
        })
        .collect()
}

#[test]
fn rerank_config_default_pool_depth_matches_rrf_default() {
    // SEARCH-6 対応: 候補プールの既定深さは `hybrid::RrfConfig::default()`（200）と
    // 整合していることを固定する。
    assert_eq!(
        RerankConfig::default().pool_depth(),
        RrfConfig::default().pool_depth()
    );
}

#[test]
fn rerank_output_is_subset_of_hybrid_pool() {
    // SEARCH-6 対応: リランカーは候補プールを拡張しない（出力 id 集合 ⊆ 入力 id 集合）
    // ことを `hybrid_search` の実出力を使って統合レベルで確認する。
    let rrf_cfg = RrfConfig::default();
    let docs: Vec<(u64, &str)> = vec![
        (1, "vector search kernel reranking"),
        (2, "unrelated filler content about weather"),
        (3, "database query planner internals"),
    ];
    let index = SparseIndex::build(&docs).expect("build ok");
    let ids = [1u64, 2, 3];
    let vectors = [1.0f32, 1.0, 1.0];
    let query = [1.0f32];
    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 1,
        query: &query,
        k: 3,
    };
    let hybrid_hits = hybrid_search(
        &CpuScalarProvider,
        input,
        &index,
        "vector search",
        3,
        &rrf_cfg,
    )
    .expect("hybrid ok");
    let candidates = to_candidates(&hybrid_hits, &docs);
    let input_ids: std::collections::BTreeSet<u64> = candidates.iter().map(|c| c.id).collect();

    let rerank_cfg = RerankConfig::new(3, 3).unwrap();
    let hits = rerank_candidates(&IdentityReranker, "vector search", &candidates, &rerank_cfg)
        .expect("rerank ok");
    assert!(
        hits.iter().all(|h| input_ids.contains(&h.id)),
        "reranked hits must be a subset of the hybrid candidate pool"
    );

    let reranker = LexicalOverlapReranker::default();
    let hits =
        rerank_candidates(&reranker, "vector search", &candidates, &rerank_cfg).expect("rerank ok");
    assert!(
        hits.iter().all(|h| input_ids.contains(&h.id)),
        "reranked hits must be a subset of the hybrid candidate pool"
    );
}

#[test]
fn lexical_reranker_resurfaces_matching_document_over_identity_baseline() {
    // SEARCH-7 対応: 融合スコアでは下位に沈んでいても、クエリと字句一致する文書が
    // LexicalOverlapReranker では上位へ再浮上する一方、IdentityReranker（ベースライン）
    // は入力順序（融合スコア降順）をそのまま保持することを比較構造として確認する。
    let docs: Vec<(u64, &str)> = vec![
        (1, "completely unrelated content"),
        (2, "database query planner internals"),
        (3, "vector search kernel reranking behavior"),
    ];
    // 融合スコアは id=1 が最上位になるよう手組みする（本テストの関心は
    // rerank_candidates 以降の挙動であり、hybrid_search 自体の融合結果は使わない）。
    let candidates = vec![
        RerankCandidate {
            id: 1,
            fused_score: 3.0,
            text: docs[0].1,
        },
        RerankCandidate {
            id: 2,
            fused_score: 2.0,
            text: docs[1].1,
        },
        RerankCandidate {
            id: 3,
            fused_score: 1.0,
            text: docs[2].1,
        },
    ];
    let cfg = RerankConfig::new(3, 3).unwrap();

    let baseline = rerank_candidates(&IdentityReranker, "vector search kernel", &candidates, &cfg)
        .expect("identity ok");
    assert_eq!(
        baseline.iter().map(|h| h.id).collect::<Vec<_>>(),
        vec![1, 2, 3],
        "identity reranker must preserve fused-score order"
    );

    // 既定重み（[`LexicalOverlapReranker::default`]。Issue #310 対応で fused 優位
    // 3.0:1.0 へ変更済み）でも融合順位 1 位（id=1）の優位が字句一致信号と
    // 拮抗しうるため、字句一致信号を優勢にする重み構成（`lexical_weight` を
    // 大きく取る）で再浮上を検証する。
    let reranker = LexicalOverlapReranker::new(60.0, 1.0, 5.0).expect("valid weights");
    let reranked = rerank_candidates(&reranker, "vector search kernel", &candidates, &cfg)
        .expect("lexical ok");
    assert_eq!(
        reranked[0].id, 3,
        "lexically matching doc must surface to top despite lower fused score"
    );
}

#[test]
fn lexical_reranker_is_deterministic_across_repeated_calls() {
    // SEARCH-7 対応: 同一入力に対する決定性の再現性を確認する。
    let candidates = vec![
        RerankCandidate {
            id: 1,
            fused_score: 3.0,
            text: "vector search kernel",
        },
        RerankCandidate {
            id: 2,
            fused_score: 2.0,
            text: "vector search kernel",
        },
        RerankCandidate {
            id: 3,
            fused_score: 1.0,
            text: "unrelated",
        },
    ];
    let cfg = RerankConfig::new(3, 3).unwrap();
    let reranker = LexicalOverlapReranker::default();
    let a = rerank_candidates(&reranker, "vector search kernel", &candidates, &cfg).expect("ok");
    let b = rerank_candidates(&reranker, "vector search kernel", &candidates, &cfg).expect("ok");
    assert_eq!(a, b, "reranking must be deterministic for identical input");
}

#[test]
fn rerank_candidates_rejects_final_k_zero_via_config() {
    // final_k=0 は RerankConfig::new が構築時点で拒否する（fail-closed の構築経路）。
    let err = RerankConfig::new(10, 0).unwrap_err();
    assert_eq!(err, RerankError::InvalidConfig);
}

#[test]
fn rerank_candidates_rejects_final_k_exceeding_pool_depth_via_config() {
    let err = RerankConfig::new(2, 5).unwrap_err();
    assert_eq!(err, RerankError::InvalidConfig);
}

#[test]
fn rerank_candidates_rejects_pool_exceeding_max_pool_depth() {
    let candidates: Vec<RerankCandidate<'static>> = (0..3)
        .map(|i| RerankCandidate {
            id: i,
            fused_score: (3 - i) as f64,
            text: "x",
        })
        .collect();
    let cfg = RerankConfig::new(2, 2).unwrap();
    let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
    assert_eq!(err, RerankError::TooManyCandidates { len: 3, max: 2 });
}

#[test]
fn rerank_candidates_rejects_non_finite_fused_score() {
    let candidates = vec![RerankCandidate {
        id: 1,
        fused_score: f64::NAN,
        text: "x",
    }];
    let cfg = RerankConfig::default();
    let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
    assert_eq!(err, RerankError::NonFiniteScore);
}

#[test]
fn rerank_candidates_rejects_duplicate_input_id() {
    let candidates = vec![
        RerankCandidate {
            id: 1,
            fused_score: 2.0,
            text: "x",
        },
        RerankCandidate {
            id: 1,
            fused_score: 1.0,
            text: "y",
        },
    ];
    let cfg = RerankConfig::default();
    let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
    assert_eq!(err, RerankError::DuplicateId);
}

#[test]
fn rerank_candidates_rejects_unsorted_input() {
    let candidates = vec![
        RerankCandidate {
            id: 1,
            fused_score: 1.0,
            text: "x",
        },
        RerankCandidate {
            id: 2,
            fused_score: 2.0,
            text: "y",
        },
    ];
    let cfg = RerankConfig::default();
    let err = rerank_candidates(&IdentityReranker, "q", &candidates, &cfg).unwrap_err();
    assert_eq!(err, RerankError::UnsortedInput);
}

/// [`Reranker`] の契約違反（候補外 id を返す）を模したモック
/// （テナント境界の fail-closed 検証: 候補外 id は事後フィルタせず検索全体を拒否する）。
struct LeakyReranker;
impl Reranker for LeakyReranker {
    fn rerank(
        &self,
        _query_text: &str,
        _candidates: &[RerankCandidate<'_>],
        _final_k: usize,
    ) -> Result<Vec<RerankedHit>, RerankError> {
        Ok(vec![RerankedHit {
            id: 424_242,
            score: 1.0,
        }])
    }
}

#[test]
fn rerank_candidates_rejects_reranker_returning_id_outside_candidate_pool() {
    let candidates = vec![RerankCandidate {
        id: 1,
        fused_score: 1.0,
        text: "x",
    }];
    let cfg = RerankConfig::default();
    let err = rerank_candidates(&LeakyReranker, "q", &candidates, &cfg).unwrap_err();
    assert_eq!(err, RerankError::ForeignId);
}

/// [`Reranker`] の契約違反（要求件数超過を返す）を模したモック。
struct OverflowingReranker;
impl Reranker for OverflowingReranker {
    fn rerank(
        &self,
        _query_text: &str,
        candidates: &[RerankCandidate<'_>],
        _final_k: usize,
    ) -> Result<Vec<RerankedHit>, RerankError> {
        Ok(candidates
            .iter()
            .map(|c| RerankedHit {
                id: c.id,
                score: c.fused_score,
            })
            .collect())
    }
}

#[test]
fn rerank_candidates_rejects_reranker_output_exceeding_final_k() {
    let candidates = vec![
        RerankCandidate {
            id: 1,
            fused_score: 2.0,
            text: "x",
        },
        RerankCandidate {
            id: 2,
            fused_score: 1.0,
            text: "y",
        },
    ];
    let cfg = RerankConfig::new(2, 1).unwrap();
    let err = rerank_candidates(&OverflowingReranker, "q", &candidates, &cfg).unwrap_err();
    assert_eq!(err, RerankError::OversizedResult { len: 2, max: 1 });
}
