//! `hybrid.rs` のソフトブースト機構（TASK-111・PLAN-1・EXT-4）の統合テスト。
//!
//! `query_planner.rs`（TASK-110）が返す `QueryExpansion::path_hint`/`kind_hint` から
//! `hybrid::path_hint_matches`/`kind_hint_matches` で一致判定を行い、`BoostRule` を
//! 構築して `hybrid::hybrid_search_boosted` に渡す end-to-end 経路を、`tests/hybrid.rs`
//! と同じ合成コーパスの流儀で検証する。RRF スコア計算・タイブレークそのものの
//! ユニットレベル検証は `hybrid.rs` 内の `#[cfg(test)]` で済んでいるため、本ファイルは
//! 「ハードフィルタ化しない（EXT-4）」「切り詰め前に適用する（PLAN-1）」という設計
//! 判断の振る舞い検証に絞る。受け入れ基準の数値検証そのものは TASK-112 の評価
//! ハーネス管轄のため、本ファイルでは扱わない（受け入れ基準の数値は非公開。
//! spec 本文は転記しない）。

use std::collections::BTreeSet;

use engine::hybrid::{
    hybrid_search, hybrid_search_boosted, kind_hint_matches, path_hint_matches, BoostRule,
    HybridError, RrfConfig, SOFT_BOOST_PER_MATCH,
};
use engine::kernel::{CpuScalarProvider, SearchInput};
use engine::sparse::SparseIndex;

/// `build_corpus` の 5 件（密ランク 1〜5、`k_const=60.0`）で id=3 を対象に使う
/// テストが使う加点量（codex-review P1 2 回目指摘の回帰対応）。
///
/// この合成コーパスは真の 1 位（id=1・密ランク 1）と対象ドキュメント（id=3・
/// 密ランク 3）のスコア差が `1/61 - 1/63 ≈ 0.00052` しかなく、`hybrid.rs` の
/// モジュールドキュメントが `SOFT_BOOST_PER_MATCH`（`0.0007`）について述べる
/// 「既定 `RrfConfig` 下で真の 1 位を上回れない」保証は、対象がプール最下位級
/// （`pool_depth` 由来の大きなスコア差）にいる最悪ケースを前提にしたものであり、
/// 本コーパスのように対象が真の 1 位のすぐ近く（密ランク 3）にいる場合には
/// 成立しない（`SOFT_BOOST_PER_MATCH` 自体が `0.00052` を上回るため、これを
/// そのまま使うと id=3 が id=1 を追い越してしまう）。本テストは「Top-k 圏内への
/// 浮上」だけを検証したいので、密ランク 2（id=2）とのスコア差
/// （`1/62 - 1/63 ≈ 0.000256`）は上回るが、密ランク 1（id=1）とのスコア差
/// （`≈0.00052`）は下回る値を使い、真の 1 位を上回らないまま Top-k 入りだけを
/// 起こす。
const RANK3_TOP_K_ENTRY_BOOST: f64 = 0.0004;

/// 合成コーパス 1 件分（文書 ID・検索用テキスト・低次元密ベクトル・メタデータ）。
/// `path`/`kind` はヒント一致判定の対象となるメタデータで、`sql/exec.rs` が可視行
/// から構築する想定の情報を模する。
struct Doc {
    id: u64,
    text: &'static str,
    vector: [f32; 2],
    path: &'static str,
    kind: &'static str,
}

/// 密ランクのみで融合スコアの順位が決まるよう、疎側にはクエリ語と重複しない
/// テキストを与える（`search_within` の結果が空になり、`rrf_fuse` の融合スコアが
/// 密ランクだけに依存する単純な構成にして、ブーストによる順位変動を検証しやすく
/// する）。密ベクトルはクエリ `[1.0, 0.0]` との内積降順に rank 1〜5 になるよう
/// 単調減少させてある。
fn build_corpus() -> Vec<Doc> {
    vec![
        Doc {
            id: 1,
            text: "alpha",
            vector: [1.00, 0.0],
            path: "src/other/alpha.rs",
            kind: "code",
        },
        Doc {
            id: 2,
            text: "beta",
            vector: [0.90, 0.0],
            path: "src/other/beta.rs",
            kind: "code",
        },
        // target: ヒント一致文書。ブーストなしでは密ランク 3 位で k=2 の枠外。
        Doc {
            id: 3,
            text: "gamma",
            vector: [0.80, 0.0],
            path: "src/hybrid/target.rs",
            kind: "doc",
        },
        Doc {
            id: 4,
            text: "delta",
            vector: [0.70, 0.0],
            path: "src/other/delta.rs",
            kind: "code",
        },
        Doc {
            id: 5,
            text: "epsilon",
            vector: [0.60, 0.0],
            path: "src/other/epsilon.rs",
            kind: "code",
        },
    ]
}

fn build_sparse_index(docs: &[Doc]) -> SparseIndex {
    let refs: Vec<(u64, &str)> = docs.iter().map(|d| (d.id, d.text)).collect();
    SparseIndex::build(&refs).expect("sparse index build ok")
}

fn flatten_vectors(docs: &[Doc]) -> (Vec<u64>, Vec<f32>) {
    let ids: Vec<u64> = docs.iter().map(|d| d.id).collect();
    let vectors: Vec<f32> = docs.iter().flat_map(|d| d.vector).collect();
    (ids, vectors)
}

/// `QueryExpansion::path_hint`（TASK-110）相当のヒント文字列から、対象コーパスの
/// うち一致する id 集合を構築する（`sql/exec.rs` が可視行のパスへ
/// `path_hint_matches` を適用してブーストルールを組み立てる想定の手順を模する）。
fn ids_matching_path_hint(docs: &[Doc], hint: &str) -> BTreeSet<u64> {
    docs.iter()
        .filter(|d| path_hint_matches(hint, d.path))
        .map(|d| d.id)
        .collect()
}

fn ids_matching_kind_hint(docs: &[Doc], hint: &str) -> BTreeSet<u64> {
    docs.iter()
        .filter(|d| kind_hint_matches(hint, d.kind))
        .map(|d| d.id)
        .collect()
}

// PLAN-1 対応: ヒント一致文書（id=3）は密ランク単独では k=2 の枠外だが、ソフト
// ブーストにより Top-k 内へ浮上すること（切り詰め前適用の効果検証）。
#[test]
fn hybrid_search_boosted_lifts_hint_matching_doc_into_top_k() {
    let docs = build_corpus();
    let index = build_sparse_index(&docs);
    let (ids, vectors) = flatten_vectors(&docs);
    let query_vector = [1.0f32, 0.0];
    // pool_depth はコーパス全件を融合対象に含められる値にする。
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 10).expect("cfg ok");

    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &query_vector,
        k: 2,
    };
    // ブーストなしでは密ランク上位 2 件（id=1, id=2）が Top-k になり、id=3 は
    // 枠外になることを先に確認する（比較対象のベースライン）。
    let plain = hybrid_search(&CpuScalarProvider, input, &index, "zzz-no-match", 2, &cfg)
        .expect("hybrid search ok");
    let plain_ids: Vec<u64> = plain.iter().map(|h| h.id).collect();
    assert_eq!(plain_ids, vec![1, 2], "baseline plain_ids={plain_ids:?}");

    let path_ids = ids_matching_path_hint(&docs, "src/hybrid");
    assert_eq!(path_ids, [3].into_iter().collect::<BTreeSet<u64>>());
    let rule = BoostRule::new(&path_ids, RANK3_TOP_K_ENTRY_BOOST).expect("rule ok");

    let input2 = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &query_vector,
        k: 2,
    };
    let boosted = hybrid_search_boosted(
        &CpuScalarProvider,
        input2,
        &index,
        "zzz-no-match",
        2,
        &cfg,
        &[rule],
    )
    .expect("hybrid search boosted ok");
    let boosted_ids: Vec<u64> = boosted.iter().map(|h| h.id).collect();
    assert!(
        boosted_ids.contains(&3),
        "hint-matching doc must enter top-k: boosted_ids={boosted_ids:?}"
    );
}

// PLAN-1 対応: ヒントなし（両方 `None` 相当 = 空ルール）では既存 `hybrid_search` と
// 完全一致の結果になること。
#[test]
fn hybrid_search_boosted_with_no_hints_matches_plain_hybrid_search() {
    let docs = build_corpus();
    let index = build_sparse_index(&docs);
    let (ids, vectors) = flatten_vectors(&docs);
    let query_vector = [1.0f32, 0.0];
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 10).expect("cfg ok");

    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &query_vector,
        k: 3,
    };
    let plain = hybrid_search(&CpuScalarProvider, input, &index, "zzz-no-match", 3, &cfg)
        .expect("hybrid search ok");

    let input2 = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &query_vector,
        k: 3,
    };
    let boosted = hybrid_search_boosted(
        &CpuScalarProvider,
        input2,
        &index,
        "zzz-no-match",
        3,
        &cfg,
        &[],
    )
    .expect("hybrid search boosted ok");

    assert_eq!(plain, boosted);
}

// EXT-4 対応: ブースト適用前後で候補集合（id 集合）が不変であること。ヒント不一致
// 文書もブーストで結果から除外されない（ハードフィルタ化しない）ことを、pool_depth
// 全体を k として取得し全件比較することで確認する。
#[test]
fn hybrid_search_boosted_does_not_change_candidate_set() {
    let docs = build_corpus();
    let index = build_sparse_index(&docs);
    let (ids, vectors) = flatten_vectors(&docs);
    let query_vector = [1.0f32, 0.0];
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 10).expect("cfg ok");
    let k = docs.len();

    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &query_vector,
        k,
    };
    let plain = hybrid_search(&CpuScalarProvider, input, &index, "zzz-no-match", k, &cfg)
        .expect("hybrid search ok");
    let mut plain_ids: Vec<u64> = plain.iter().map(|h| h.id).collect();
    plain_ids.sort_unstable();

    let path_ids = ids_matching_path_hint(&docs, "src/hybrid");
    let rule = BoostRule::new(&path_ids, RANK3_TOP_K_ENTRY_BOOST).expect("rule ok");

    let input2 = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &query_vector,
        k,
    };
    let boosted = hybrid_search_boosted(
        &CpuScalarProvider,
        input2,
        &index,
        "zzz-no-match",
        k,
        &cfg,
        &[rule],
    )
    .expect("hybrid search boosted ok");
    let mut boosted_ids: Vec<u64> = boosted.iter().map(|h| h.id).collect();
    boosted_ids.sort_unstable();

    // 候補集合（id 集合）は完全一致（ブーストは順位のみを変える）。
    assert_eq!(plain_ids, boosted_ids);
    // ヒント不一致文書（例: id=1）も結果から消えない。
    assert!(boosted_ids.contains(&1));
}

// EXT-4/PLAN-1 対応: kind_hint 一致（部分文字列一致ではなく完全一致）でも同様に
// ブーストが働くこと。path_hint・kind_hint 双方一致なら加点は和になる（複数
// ルール一致の合算は hybrid.rs 内ユニットテストで検証済みのため、ここでは単一
// ヒント種別の経路のみ end-to-end で確認する）。
#[test]
fn hybrid_search_boosted_supports_kind_hint_matching() {
    let docs = build_corpus();
    let index = build_sparse_index(&docs);
    let (ids, vectors) = flatten_vectors(&docs);
    let query_vector = [1.0f32, 0.0];
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 10).expect("cfg ok");

    let kind_ids = ids_matching_kind_hint(&docs, "doc");
    assert_eq!(kind_ids, [3].into_iter().collect::<BTreeSet<u64>>());
    let rule = BoostRule::new(&kind_ids, RANK3_TOP_K_ENTRY_BOOST).expect("rule ok");

    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &query_vector,
        k: 2,
    };
    let boosted = hybrid_search_boosted(
        &CpuScalarProvider,
        input,
        &index,
        "zzz-no-match",
        2,
        &cfg,
        &[rule],
    )
    .expect("hybrid search boosted ok");
    let boosted_ids: Vec<u64> = boosted.iter().map(|h| h.id).collect();
    assert!(
        boosted_ids.contains(&3),
        "kind-hint-matching doc must enter top-k: boosted_ids={boosted_ids:?}"
    );
}

// fail-closed 系: ルール数が上限（`MAX_BOOST_RULES`）を超える場合、検索全体が
// `TooManyBoostRules` で拒否されること（部分適用ではなく全体をエラーにする）。
#[test]
fn hybrid_search_boosted_rejects_too_many_rules() {
    use engine::hybrid::MAX_BOOST_RULES;

    let docs = build_corpus();
    let index = build_sparse_index(&docs);
    let (ids, vectors) = flatten_vectors(&docs);
    let query_vector = [1.0f32, 0.0];
    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 10).expect("cfg ok");

    let path_ids = ids_matching_path_hint(&docs, "src/hybrid");
    let rule = BoostRule::new(&path_ids, SOFT_BOOST_PER_MATCH).expect("rule ok");
    let rules: Vec<BoostRule<'_>> = (0..(MAX_BOOST_RULES + 1)).map(|_| rule).collect();

    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &query_vector,
        k: 2,
    };
    let err = hybrid_search_boosted(
        &CpuScalarProvider,
        input,
        &index,
        "zzz-no-match",
        2,
        &cfg,
        &rules,
    )
    .unwrap_err();
    assert_eq!(
        err,
        HybridError::TooManyBoostRules {
            len: MAX_BOOST_RULES + 1,
            max: MAX_BOOST_RULES,
        }
    );
}

// fail-closed 系: `BoostRule::new` が拒否する不正な `amount`（0 以下・非有限・上限
// 超過）はルール構築の時点で検索全体へ到達しない（検証付きコンストラクタのみで
// 構築可能な設計そのものの確認）。
#[test]
fn boost_rule_construction_rejects_invalid_amount_before_search() {
    let ids: BTreeSet<u64> = BTreeSet::new();
    assert!(BoostRule::new(&ids, 0.0).is_err());
    assert!(BoostRule::new(&ids, -1.0).is_err());
    assert!(BoostRule::new(&ids, f64::NAN).is_err());
    assert!(BoostRule::new(&ids, f64::INFINITY).is_err());
}
