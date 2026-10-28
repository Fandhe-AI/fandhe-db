//! `scoring_boost.rs` の宣言的スコアリングブースト API（TASK-148・EXT-4）の統合
//! テスト。`tests/soft_boost.rs`（TASK-111）と同じ合成コーパスの流儀で、
//! `hybrid::rrf_fuse` の融合済み候補列に対して `scoring_boost::apply_scoring_boosts`
//! を適用する end-to-end 経路を検証する。ユニットレベルの束縛検証（`bind` の
//! fail-closed 系）は `src/scoring_boost.rs` 内の `#[cfg(test)]` で済んでいるため、
//! 本ファイルは「ハードフィルタ化しない（EXT-4）」「TASK-111 のヒント一致ヘルパとの
//! 後方互換」「決定的な再ソート」という設計判断の振る舞い検証に絞る。

use std::collections::BTreeSet;

use engine::catalog::{ColumnDef, ColumnType, TableSchema};
use engine::hybrid::{
    kind_hint_matches, path_hint_matches, rrf_fuse, HybridError, HybridHit, RrfConfig,
    MAX_BOOST_IDS,
};
use engine::kernel::CandidateHit;
use engine::scoring_boost::{apply_scoring_boosts, BoostMetadata, ScoringBoost};
use engine::sparse::ScoredDoc;

/// 1 候補分の列値集合（列名 → 値）。`BoostMetadata::new` へ渡す形をテスト側で
/// 組み立てる際の型複雑度を抑えるためのエイリアス。
type ColumnValues<'a, const N: usize> = (u64, [(&'a str, Option<&'a str>); N]);

/// 合成コーパス 1 件分。`tests/soft_boost.rs::build_corpus` と同一の密ランク構成
/// （密ランクのみで融合スコアの順位が決まるよう疎側は一致なしにする）を使う。
struct Doc {
    id: u64,
    dense_score: f32,
    path: &'static str,
    kind: &'static str,
}

fn build_corpus() -> Vec<Doc> {
    vec![
        Doc {
            id: 1,
            dense_score: 1.00,
            path: "src/other/alpha.rs",
            kind: "code",
        },
        Doc {
            id: 2,
            dense_score: 0.90,
            path: "src/other/beta.rs",
            kind: "code",
        },
        // target: メタデータ一致文書。ブーストなしでは密ランク 3 位で k=2 の枠外。
        Doc {
            id: 3,
            dense_score: 0.80,
            path: "src/hybrid/target.rs",
            kind: "doc",
        },
        Doc {
            id: 4,
            dense_score: 0.70,
            path: "src/other/delta.rs",
            kind: "code",
        },
        Doc {
            id: 5,
            dense_score: 0.60,
            path: "src/other/epsilon.rs",
            kind: "code",
        },
    ]
}

fn schema() -> TableSchema {
    TableSchema::new(
        "docs",
        vec![
            ColumnDef::new("path", ColumnType::Text, false),
            ColumnDef::new("kind", ColumnType::Text, true),
        ],
    )
}

fn fuse(docs: &[Doc], cfg: &RrfConfig) -> Vec<HybridHit> {
    let dense: Vec<CandidateHit> = docs
        .iter()
        .map(|d| CandidateHit {
            id: d.id,
            score: d.dense_score,
        })
        .collect();
    // 疎側は空（一致なし）にして、融合スコアが密ランクのみに依存する単純な構成にする
    // （`tests/soft_boost.rs` と同じ狙い）。
    let sparse: Vec<ScoredDoc> = Vec::new();
    rrf_fuse(&dense, &sparse, cfg).expect("rrf_fuse ok")
}

/// EXT-4 本体: メタデータ一致候補（id=3・`kind = 'doc'`）の順位がブーストで
/// 引き上がり、かつ候補集合（id の多重集合）が不変であること（完全除外しない）。
#[test]
fn apply_scoring_boosts_lifts_matching_candidate_without_excluding_others() {
    let docs = build_corpus();
    let cfg = RrfConfig::default();
    let mut hits = fuse(&docs, &cfg);
    let before: BTreeSet<u64> = hits.iter().map(|h| h.id).collect();

    let bound = ScoringBoost::equals("kind", "doc", 0.0004)
        .bind(&schema())
        .expect("bind ok");
    let values: Vec<ColumnValues<'_, 1>> = docs
        .iter()
        .map(|d| (d.id, [("kind", Some(d.kind))]))
        .collect();
    let metadata: Vec<BoostMetadata<'_>> = values
        .iter()
        .map(|(id, kv)| BoostMetadata::new(*id, kv))
        .collect();

    apply_scoring_boosts(&mut hits, &[bound], &metadata, &cfg).expect("apply ok");

    let after: BTreeSet<u64> = hits.iter().map(|h| h.id).collect();
    assert_eq!(before, after, "candidate set must stay unchanged (EXT-4)");

    // ブースト前は id=3 が密ランク 3 位（k=2 の枠外）。ブースト後は id=2 を上回り
    // Top-2 圏内へ浮上すること。
    let top2: Vec<u64> = hits.iter().take(2).map(|h| h.id).collect();
    assert!(top2.contains(&3), "top2={top2:?}");
}

/// EXT-4 一般化の後方互換: `Contains`/`Equals` 宣言が `path_hint_matches`/
/// `kind_hint_matches` と同一コーパスで同一の一致集合・同一の最終順位を与えること。
#[test]
fn contains_and_equals_match_same_candidates_and_ranking_as_hint_helpers() {
    let docs = build_corpus();
    let cfg = RrfConfig::default();

    // 旧ヒントヘルパ経由の一致集合（TASK-111 の呼び出し手順を模する）。
    let legacy_path_ids: BTreeSet<u64> = docs
        .iter()
        .filter(|d| path_hint_matches("src/hybrid", d.path))
        .map(|d| d.id)
        .collect();
    let legacy_kind_ids: BTreeSet<u64> = docs
        .iter()
        .filter(|d| kind_hint_matches("doc", d.kind))
        .map(|d| d.id)
        .collect();
    assert_eq!(
        legacy_path_ids, legacy_kind_ids,
        "corpus keeps both hints on id=3 only"
    );

    let path_bound = ScoringBoost::contains("path", "src/hybrid", 0.0004)
        .bind(&schema())
        .expect("bind ok");
    let values: Vec<ColumnValues<'_, 2>> = docs
        .iter()
        .map(|d| (d.id, [("path", Some(d.path)), ("kind", Some(d.kind))]))
        .collect();
    let metadata: Vec<BoostMetadata<'_>> = values
        .iter()
        .map(|(id, kv)| BoostMetadata::new(*id, kv))
        .collect();

    let mut generalized_hits = fuse(&docs, &cfg);
    apply_scoring_boosts(&mut generalized_hits, &[path_bound], &metadata, &cfg).expect("apply ok");

    let mut legacy_hits = fuse(&docs, &cfg);
    let rule = engine::hybrid::BoostRule::new(&legacy_path_ids, 0.0004).expect("rule ok");
    engine::hybrid::apply_soft_boost(&mut legacy_hits, &[rule], &cfg).expect("legacy apply ok");

    assert_eq!(generalized_hits, legacy_hits);
}

/// fail-closed 系: `NULL` メタデータは不一致（ブーストなし）。
#[test]
fn apply_scoring_boosts_treats_null_metadata_as_no_match() {
    let docs = build_corpus();
    let cfg = RrfConfig::default();
    let mut hits = fuse(&docs, &cfg);
    let before = hits.clone();

    let bound = ScoringBoost::equals("kind", "doc", 0.0004)
        .bind(&schema())
        .expect("bind ok");
    // id=3 の `kind` を NULL として渡す（一致条件を満たす値が存在しない）。
    let values: [(&str, Option<&str>); 1] = [("kind", None)];
    let metadata = vec![BoostMetadata::new(3, &values)];

    apply_scoring_boosts(&mut hits, &[bound], &metadata, &cfg).expect("apply ok");
    assert_eq!(hits, before, "NULL metadata must not affect ranking");
}

/// fail-closed 系: 未知列・`VECTOR` 列・空リテラル・長大リテラル・不正 `amount` の
/// 束縛拒否、宣言数上限超過の適用拒否。
#[test]
fn bind_and_apply_reject_invalid_declarations() {
    let mut vector_schema = schema();
    vector_schema
        .columns
        .push(ColumnDef::new("body", ColumnType::Vector(2), false));

    assert!(ScoringBoost::equals("missing", "doc", 0.001)
        .bind(&vector_schema)
        .is_err());
    assert!(ScoringBoost::equals("body", "x", 0.001)
        .bind(&vector_schema)
        .is_err());
    assert!(ScoringBoost::equals("kind", "", 0.001)
        .bind(&vector_schema)
        .is_err());
    // `row_codec::MAX_TEXT_FIELD_LEN`（4 * 1024 * 1024 バイト）超過。
    let too_long = "x".repeat(4 * 1024 * 1024 + 1);
    assert!(ScoringBoost::equals("kind", too_long, 0.001)
        .bind(&vector_schema)
        .is_err());
    assert!(ScoringBoost::equals("kind", "doc", 0.0)
        .bind(&vector_schema)
        .is_err());
    assert!(ScoringBoost::equals("kind", "doc", f64::NAN)
        .bind(&vector_schema)
        .is_err());

    let bound = ScoringBoost::equals("kind", "doc", 0.0004)
        .bind(&schema())
        .expect("bind ok");
    let boosts: Vec<_> = (0..=engine::scoring_boost::MAX_SCORING_BOOSTS)
        .map(|_| bound.clone())
        .collect();
    let mut hits = vec![HybridHit { id: 1, score: 0.1 }];
    assert!(apply_scoring_boosts(&mut hits, &boosts, &[], &RrfConfig::default()).is_err());
}

/// 決定性: 同一入力への 2 回適用で同一順序（`scripts/check_sort_determinism.sh`
/// 方針との整合）。`apply_scoring_boosts` はスコアを累積的に加点するため、2 回目の
/// 適用結果と 1 回だけ適用した結果を比較するのではなく、同一の初期状態から独立に
/// 2 回計算した結果同士が一致することを確認する。
#[test]
fn apply_scoring_boosts_is_deterministic_across_repeated_runs() {
    let docs = build_corpus();
    let cfg = RrfConfig::default();
    let bound = ScoringBoost::contains("path", "src/hybrid", 0.0004)
        .bind(&schema())
        .expect("bind ok");
    let values: Vec<ColumnValues<'_, 1>> = docs
        .iter()
        .map(|d| (d.id, [("path", Some(d.path))]))
        .collect();
    let metadata: Vec<BoostMetadata<'_>> = values
        .iter()
        .map(|(id, kv)| BoostMetadata::new(*id, kv))
        .collect();

    let mut run1 = fuse(&docs, &cfg);
    apply_scoring_boosts(&mut run1, std::slice::from_ref(&bound), &metadata, &cfg)
        .expect("apply ok");

    let mut run2 = fuse(&docs, &cfg);
    apply_scoring_boosts(&mut run2, &[bound], &metadata, &cfg).expect("apply ok");

    assert_eq!(run1, run2);
}

/// DoS 回避（PR #260 codex-review・cursor[bot] 指摘対応）: `metadata.len() >
/// MAX_BOOST_IDS` はブースト一致判定の線形走査（`boosts.len() * metadata.len()`）
/// より前に拒否されること。走査を実行してから `BoostRule::new` の
/// `MAX_BOOST_IDS` 検査で事後的に拒否されるのではなく、`apply_scoring_boosts`
/// 自身が事前に `HybridError::TooManyBoostIds` を返す（アロケーション・走査前の
/// 上限検証という fail-closed 方針を維持する）ことを固定する。
#[test]
fn apply_scoring_boosts_rejects_oversized_metadata_before_scanning() {
    let bound = ScoringBoost::equals("kind", "doc", 0.0004)
        .bind(&schema())
        .expect("bind ok");
    // 値は全件 `None`（不一致）にし、万一走査が最後まで実行されても一致 id 集合が
    // 空のまま素通りしないことを確認できるようにする。
    let values: Vec<ColumnValues<'_, 1>> = (0..=(MAX_BOOST_IDS as u64))
        .map(|id| (id, [("kind", None)]))
        .collect();
    let metadata: Vec<BoostMetadata<'_>> = values
        .iter()
        .map(|(id, kv)| BoostMetadata::new(*id, kv))
        .collect();
    assert_eq!(metadata.len(), MAX_BOOST_IDS + 1);

    let mut hits = vec![HybridHit { id: 1, score: 0.1 }];
    let err = apply_scoring_boosts(&mut hits, &[bound], &metadata, &RrfConfig::default())
        .expect_err("oversized metadata must be rejected");
    assert_eq!(
        err,
        HybridError::TooManyBoostIds {
            len: metadata.len(),
            max: MAX_BOOST_IDS,
        }
    );
}
