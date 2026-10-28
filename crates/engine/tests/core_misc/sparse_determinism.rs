//! 転置索引化（Issue #386 Phase 1・#388〜#392）後の疎（BM25）側決定性契約の回帰
//! テスト（Issue #393）。`docs/design/rrf-tie-break-determinism.md` が固定した
//! 「スコア降順・同点は `doc_id` 昇順」の全順序契約が、`sparse.rs::Candidate`
//! に対する `select_nth_unstable_by`/`sort_unstable_by`（#391・#392。同ファイル
//! `// sort-determinism: allow` マーカー付き）へ置き換わった後も
//! `SparseIndex::search_within`/`score_within`/`SparseScored::top`・
//! `hybrid::hybrid_search`（疎側再取得ループ経由）で維持されることを、公開 API
//! だけを使って固定する。BM25 式そのものの独立再実装はしない
//! （`tests/hybrid.rs` の既存方針と同じく構造的性質のみ検証する）。
//!
//! 対応 spec ビヘイビア（ポインタのみ・本文非転記）: SEARCH-1, SEARCH-3。

use std::collections::BTreeSet;

use engine::hybrid::{hybrid_search, RrfConfig};
use engine::kernel::{CpuScalarProvider, SearchInput};
use engine::parallel_search::ParallelSearchProvider;
use engine::sparse::SparseIndex;

/// 同一テキスト（同 tf・同文書長 → BM25 完全同点）の大きな同点グループを含む
/// 決定的コーパスを構築する。`tie_group_size` は 64（`TopKSelector`/バッファ境界の
/// 典型的な内部チャンクサイズの倍数）を跨ぐ大きさに設定し、同点グループが
/// 内部バッファ境界をまたいでも崩れないことを検証できるようにする。
///
/// 構成:
/// - id 1..=tie_group_size: 完全同点（同一本文 "anchor anchor anchor"、tf=3・len=3）。
///   BM25 の tf/文書長比が最大（1.0）になる構成のため、他群より必ず上位になる。
/// - id 1000..=1000+distinct_count: "anchor" の tf は 1 で固定しつつ文書長だけを
///   互いに異ならせた群（tf/文書長比が tie group（1.0）より必ず低くなるため、
///   スコアは tie group 未満で確定する。tf・文書長という異なる 2 軸で互いに
///   区別できる文書群を用意する狙い）
/// - id 2000: 一致語なし（不一致文書。ノイズ、スコア対象外）
fn build_corpus(tie_group_size: u64, distinct_count: u64) -> Vec<(u64, String)> {
    let mut docs = Vec::new();
    for id in 1..=tie_group_size {
        docs.push((id, "anchor anchor anchor".to_string()));
    }
    for i in 1..=distinct_count {
        let mut text = String::from("anchor");
        for _ in 0..i {
            text.push_str(" filler");
        }
        docs.push((1000 + i, text));
    }
    docs.push((2000, "unrelated noise text".to_string()));
    docs
}

fn build_index(docs: &[(u64, String)]) -> SparseIndex {
    let refs: Vec<(u64, &str)> = docs.iter().map(|(id, t)| (*id, t.as_str())).collect();
    SparseIndex::build(&refs).expect("sparse index build ok")
}

fn all_visible(docs: &[(u64, String)]) -> BTreeSet<u64> {
    docs.iter().map(|(id, _)| *id).collect()
}

// --- 1: search_within と score_within().top(k) がスコアのビット列を含め
// 複数の k・複数回の反復にわたって完全一致すること。

#[test]
fn search_within_and_score_within_top_are_bit_identical_across_k_and_repetitions() {
    let tie_group_size = 300u64; // 64 の倍数境界（256）を跨ぐ大きさ。
    let distinct_count = 40u64;
    let docs = build_corpus(tie_group_size, distinct_count);
    let index = build_index(&docs);
    let visible = all_visible(&docs);

    let boundary_ks: [usize; 7] = [
        1,
        5,
        128,                                            // 同点グループの内側
        tie_group_size as usize,                        // 同点グループの末尾
        tie_group_size as usize + 1,                    // 同点グループ + 1
        (tie_group_size + distinct_count) as usize,     // M
        (tie_group_size + distinct_count) as usize + 1, // M + 1（可視ヒット総数を超える）
    ];

    for &k in &boundary_ks {
        let via_search_within = index
            .search_within("anchor", k, &visible)
            .expect("search_within ok");

        for rep in 0..20 {
            let mut scored = index
                .score_within("anchor", &visible)
                .expect("score_within ok");
            let via_top = scored.top(k);
            assert_eq!(
                via_search_within, via_top,
                "k={k} rep={rep}: search_within と score_within().top(k) が乖離した \
                 (search_within={via_search_within:?}, top={via_top:?})"
            );
        }
    }
}

// --- 2: 同点グループを k で切ると id 昇順の先頭 k 件のみが残り、
// top(k1) が top(k2)（k1 <= k2）の前方一致であること。

#[test]
fn tie_group_cut_by_k_yields_smallest_doc_ids_and_prefix_consistency() {
    let tie_group_size = 300u64;
    let distinct_count = 10u64;
    let docs = build_corpus(tie_group_size, distinct_count);
    let index = build_index(&docs);
    let visible = all_visible(&docs);
    // ノイズ文書（id=2000。スコア 0）を除いた、スコア > 0 のヒット総数（M）。
    let total_hits = docs.len() - 1;

    // 同点グループ内で k を切る（グループの id は 1..=tie_group_size で密に採番）。
    for k in [1usize, 2, 50, 150, tie_group_size as usize] {
        let hits = index
            .search_within("anchor", k, &visible)
            .expect("search_within ok");
        assert_eq!(hits.len(), k, "k={k}: 件数が一致しない");
        let ids: Vec<u64> = hits.iter().map(|h| h.doc_id).collect();
        let expected: Vec<u64> = (1..=k as u64).collect();
        assert_eq!(
            ids, expected,
            "k={k}: 同点グループ内の切り詰めは id 昇順の先頭 k 件のみを残すはず"
        );
    }

    // 前方一致契約: k1 <= k2 なら top(k1) は top(k2) の前方一致な部分列。
    let ks: [usize; 5] = [1, 10, 128, 256, 305];
    let mut scored = index
        .score_within("anchor", &visible)
        .expect("score_within ok");
    let mut prev: Vec<engine::sparse::ScoredDoc> = Vec::new();
    for &k in &ks {
        let cur = scored.top(k);
        assert_eq!(cur.len(), k.min(total_hits));
        assert!(
            cur.starts_with(&prev),
            "k={k}: 前方一致契約が崩れた (prev={prev:?}, cur={cur:?})"
        );
        prev = cur;
    }

    // 冪等性: 同じ k を複数回呼んでも同じ列を返す。
    let a = scored.top(128);
    let b = scored.top(128);
    assert_eq!(a, b, "top(k) の反復呼び出しは冪等であるはず");
}

// --- 3: 投入順（doc_idx/TermId 割り当て）に依存せず、search_within の
// (doc_id, score) 列がビット一致すること。可視集合を全体／部分で与えた場合も
// 同点内 id 昇順が保たれること。

#[test]
fn results_are_independent_of_build_order_and_visible_set_representation() {
    let tie_group_size = 300u64;
    let distinct_count = 20u64;
    let mut docs = build_corpus(tie_group_size, distinct_count);

    // 整列順（ベースライン）。
    let baseline_index = build_index(&docs);
    let visible = all_visible(&docs);
    let baseline = baseline_index
        .search_within("anchor", 200, &visible)
        .expect("search_within ok");

    // 決定的シャッフル（xorshift）で投入順を変える。
    let mut state = 0x1234_5678_9abc_def1u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let n = docs.len();
    for i in (1..n).rev() {
        let j = (next() % (i as u64 + 1)) as usize;
        docs.swap(i, j);
    }
    let shuffled_index = build_index(&docs);
    let shuffled = shuffled_index
        .search_within("anchor", 200, &visible)
        .expect("search_within ok");
    assert_eq!(
        baseline, shuffled,
        "投入順をシャッフルしても search_within の (doc_id, score) 列は不変であるはず"
    );

    // 逆順投入。
    docs.reverse();
    let reversed_index = build_index(&docs);
    let reversed = reversed_index
        .search_within("anchor", 200, &visible)
        .expect("search_within ok");
    assert_eq!(
        baseline, reversed,
        "投入順を逆順にしても search_within の (doc_id, score) 列は不変であるはず"
    );

    // 可視集合を部分（同点グループの中央域 50..100 を不可視化）にした場合も、
    // 残った id（1..=49・101..=tie_group_size）が id 昇順で返ること（不可視域を
    // 挟んで前後の連続域が「飛び越えて」つながる構成で、同点内 id 昇順が
    // ギャップの有無に関わらず保たれることを検証する）。
    let partial_visible: BTreeSet<u64> = all_visible(&docs)
        .into_iter()
        .filter(|id| !(*id >= 50 && *id < 100 && *id <= tie_group_size))
        .collect();
    let partial_hits = baseline_index
        .search_within("anchor", 100, &partial_visible)
        .expect("search_within ok");
    let partial_ids: Vec<u64> = partial_hits.iter().map(|h| h.doc_id).collect();
    // 期待値: 1..=49（49 件）に続けて 100..=150（51 件）で計 100 件
    // （不可視化した 50..=99 を飛び越えて id 昇順のまま連結される）。
    let expected: Vec<u64> = (1u64..=49).chain(100u64..=150).collect();
    assert_eq!(
        partial_ids, expected,
        "部分可視集合でも同点グループ内は不可視域を飛び越えて id 昇順であるはず"
    );
    assert!(
        !partial_ids.iter().any(|id| (50..100).contains(id)),
        "不可視化した id は結果に一切現れないはず"
    );
}

// --- 4: hybrid_search の疎側境界同点グループが、pool_depth を小さくして
// 再取得ループが複数ラウンド発火する構成でも決定的であること
// （TieRank::GroupEnd によりグループが分断されないこと）。

#[test]
fn hybrid_sparse_tie_group_across_pool_boundary_is_deterministic_over_multiple_refetch_rounds() {
    // 疎側のみが厚く同点になるコーパス（密ベクトルは全件同一で密側も安定させる）。
    let tie_group_size = 300u64; // pool_depth=8 から 16→32→64→128→256→512 と
                                 // 複数ラウンドの再取得が必要になる大きさ。
    let docs: Vec<(u64, String, [f32; 2])> = (1..=tie_group_size)
        .map(|id| (id, "anchor anchor anchor".to_string(), [1.0, 0.0]))
        .collect();

    let refs: Vec<(u64, &str)> = docs.iter().map(|(id, t, _)| (*id, t.as_str())).collect();
    let index = SparseIndex::build(&refs).expect("sparse index build ok");
    let ids: Vec<u64> = docs.iter().map(|(id, _, _)| *id).collect();
    let vectors: Vec<f32> = docs.iter().flat_map(|(_, _, v)| *v).collect();

    let cfg = RrfConfig::new(60.0, 1.0, 1.0, 8).expect("valid rrf config (small pool_depth)");

    for rep in 0..20 {
        let input_scalar = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &[1.0, 0.0],
            k: 8,
        };
        let scalar_hits =
            hybrid_search(&CpuScalarProvider, input_scalar, &index, "anchor", 8, &cfg)
                .expect("hybrid_search (scalar) ok");

        let input_parallel = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &[1.0, 0.0],
            k: 8,
        };
        let parallel_hits = hybrid_search(
            &ParallelSearchProvider,
            input_parallel,
            &index,
            "anchor",
            8,
            &cfg,
        )
        .expect("hybrid_search (parallel) ok");

        assert_eq!(
            scalar_hits, parallel_hits,
            "rep={rep}: CpuScalarProvider と ParallelSearchProvider で疎側同点境界の \
             結果が乖離した"
        );

        // TieRank::GroupEnd（既定）の下では、全件が同一融合スコアの同点グループに
        // 属するこの構成では、返る id 集合が id 昇順の先頭 k 件（1..=8）で
        // 安定するはず（境界同点グループが分断されない）。
        let result_ids: Vec<u64> = scalar_hits.iter().map(|h| h.id).collect();
        assert_eq!(
            result_ids,
            vec![1, 2, 3, 4, 5, 6, 7, 8],
            "rep={rep}: 疎側全件同点構成での hybrid_search 結果が id 昇順の \
             先頭 k 件から外れた"
        );
    }
}
// --- 5（codex-review 指摘対応・PR #428・Medium。さらに bugbot 再指摘・PR #428
// レビュースレッド対応）: 当初のテスト 4 は tie group の全 300 件が同一本文の
// ため、疎側境界同点グループの完全化（`sparse_refetch_loop` の複数ラウンド
// 発火・`TieRank::GroupEnd` によるグループ末尾順位の割当）をスキップする
// 退行が起きても、密側の（同じく全件同点な）タイブレークだけで id 昇順 1..=8
// が再現できてしまい、いずれの経路でも結果が一致する vacuous pass になって
// いた。
//
// 続く 1 回目の対策（競合する密優位グループの追加。id 昇順にわずかに減衰する
// 密スコア＝競合グループ内で密ランクが 1..=8 と個別に確定する構成）もなお
// vacuous だった: `pool_depth=8` に対し疎側の早期打ち切り（不完全な取得分
// だけで `TieRank::GroupEnd` を確定してしまう退行。最悪でもグループ末尾順位
// `pool_depth*2=16` 止まり・RRF `1/76`）は、競合グループの個々の密ランク
// 1..=8（RRF 最良 `1/61`〜最悪 `1/68`）のどれにも勝てない。密側の「同点で
// ないランク付け」を「疎側のグループ一律順位」と比較する限り、疎側の退行が
// どれだけ悪化しても競合側の**個々の最良ランク**（`1/61`）を超えることは
// 構造上あり得ない（同点グループの一律順位は `1..=pool_depth*2` の**先頭**
// ではなく末尾側の順位しか取り得ないため）。
//
// 本テスト（2 回目の対策）は、密側の競合グループもまた**互いに同点**にする
// ことで「密側もグループ一律順位」に揃え、疎側のグループ一律順位と直接比較
// 可能にする。`pool_depth=P`・疎側 tie group サイズ `G`・密側競合グループ
// サイズ `C` を `2*P < C < G` を満たすよう選ぶ（本テストは `P=4`・`G=20`・
// `C=14`）:
// - 正しい完全化（`sparse_refetch_loop` が `fetch_k` を `8→16→32` と伸ばし
//   `G=20` 件全体を捕捉）の下では、疎側同点グループの一律順位は `G=20`
//   （RRF `1/80`）に確定し、密側競合グループの一律順位 `C=14`（RRF `1/74`）
//   に劣るため、競合グループが最終 top-k（`k=4`）を独占する（実測:
//   id 501..=504）。
// - もし疎側の早期打ち切り退行（1 ラウンド目 `fetch_k=2*P=8` で確定してしまい
//   `G=20` まで伸ばさない）が起きれば、疎側同点グループの一律順位は `2*P=8`
//   （RRF `1/68`）へ**改善**し、`C=14`（RRF `1/74`）を上回るため、疎側同点
//   グループ全体が競合グループを押しのけて top-k を独占する（本テストが red
//   になる想定挙動: id 1..=4）。
//
// `k=4 <= pool_depth=4` の契約（[`hybrid_search`] は `k > cfg.pool_depth()`
// を `HybridError::InvalidK` で拒否）を保ったまま、密側競合グループ自身も
// `pool_depth` を跨ぐ同点グループとして境界完全化（[`complete_boundary_tie_group`]）
// を経由する（`C=14 > P=4` のため密側も 1 ラウンド分の再取得を要する。密側の
// 再取得自体は本テストの主眼ではないが、密側の同点タイブレークが id 依存に
// ならないことは同時に検証される）。
#[test]
fn hybrid_sparse_tie_group_boundary_completion_is_load_bearing_against_competing_dense_group() {
    let pool_depth = 4usize;
    let tie_group_size = 20u64;
    let competitor_size = 14u64;
    let k = 4usize;

    let mut ids: Vec<u64> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    let mut vectors: Vec<f32> = Vec::new();

    // 疎側でのみ一致する巨大同点グループ。密側は完全に無関係な方向 [0,1]
    // （クエリ [1,0] との内積が 0）で全件同一のため、密側は本グループを
    // 一切優遇しない。`tie_group_size=20` は `pool_depth*2=8` を跨ぎ、
    // `sparse_refetch_loop` が `fetch_k` を複数ラウンド（8→16→32）伸ばして
    // 初めて全件を捕捉できる大きさ。
    for id in 1..=tie_group_size {
        ids.push(id);
        texts.push("anchor anchor anchor".to_string());
        vectors.push(0.0);
        vectors.push(1.0);
    }
    // 密側でのみ一致する競合グループ。疎側は "anchor" と無関係のため BM25
    // score が 0 になり疎チャネルから除外される（`zero_is_no_signal`）。
    // 密側は全件同一スコア（クエリ [1,0] との内積が 1.0 で完全一致）に
    // 揃えることで、密側もまた `TieRank::GroupEnd` による一律順位
    // （`competitor_size=14`）を割り当てられる構成にする。`competitor_size=14`
    // は `pool_depth*2=8` より大きいため密側の境界完全化（1 ラウンド分の
    // 再取得）も経由する。
    for i in 0..competitor_size {
        let id = 501 + i;
        ids.push(id);
        texts.push("unrelated filler text".to_string());
        vectors.push(1.0);
        vectors.push(0.0);
    }

    let refs: Vec<(u64, &str)> = ids
        .iter()
        .zip(texts.iter())
        .map(|(&id, t)| (id, t.as_str()))
        .collect();
    let index = SparseIndex::build(&refs).expect("sparse index build ok");

    let cfg =
        RrfConfig::new(60.0, 1.0, 1.0, pool_depth).expect("valid rrf config (small pool_depth)");

    let input_scalar = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &[1.0, 0.0],
        k,
    };
    let scalar_hits = hybrid_search(&CpuScalarProvider, input_scalar, &index, "anchor", k, &cfg)
        .expect("hybrid_search (scalar) ok");

    let input_parallel = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 2,
        query: &[1.0, 0.0],
        k,
    };
    let parallel_hits = hybrid_search(
        &ParallelSearchProvider,
        input_parallel,
        &index,
        "anchor",
        k,
        &cfg,
    )
    .expect("hybrid_search (parallel) ok");

    assert_eq!(
        scalar_hits, parallel_hits,
        "CpuScalarProvider と ParallelSearchProvider で競合グループ構成の結果が乖離した"
    );

    let result_ids: Vec<u64> = scalar_hits.iter().map(|h| h.id).collect();
    let expected: Vec<u64> = (501u64..(501 + k as u64)).collect();
    assert_eq!(
        result_ids, expected,
        "疎側巨大同点グループの境界完全化（fetch_k を tie_group_size まで伸ばし \
         グループ末尾順位 20 を確定）が正しければ、密側同点競合グループ（一律順位 14） \
         が最終 top-k を独占するはず。疎側が早期打ち切り（fetch_k=pool_depth*2=8 で \
         確定）していれば、疎側の一律順位が 8（RRF 1/68）へ改善し密側（RRF 1/74）を \
         上回るため疎側同点グループが混入し、本アサーションが red になる \
         （境界完全化の退行を実際に検出できる構成）"
    );
}
