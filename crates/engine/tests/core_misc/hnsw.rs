//! `engine::hnsw` の結合テスト（TASK-132・対象ビヘイビア: CORE-9・CORE-10。
//! ポインタ: `docs/design/ann-index-adoption.md`「実装ガイド（B 案）」節）。
//!
//! 受け入れ条件 (a)（小規模決定的フィクスチャでの層構造・次数上限・連結性の
//! 単体テスト）を、`tests/*.rs` の既存流儀（crate 外の公開 API のみで検証する。
//! `tests/isa.rs`・`tests/dispatch.rs` と同じ位置付け）に従って検証する。
//! 受け入れ条件 (b)（構築計算量が N log N にほぼ従うことの簡易ベンチ確認）は
//! `benches/hnsw_build_bench.rs`・`tests/hnsw_build_accept.rs` が担う。

use engine::hnsw::{
    HnswError, HnswIndex, HnswParams, MAX_BUILD_THREADS, MAX_EF, MAX_HNSW_NODES, MAX_LEVEL, MAX_M,
    SEQUENTIAL_PREFIX_NODES,
};

/// 決定的シードの xorshift64*（`benches/harness/rng.rs::DeterministicRng` と同
/// アルゴリズム。結合テストは crate 外の公開 API のみを使う流儀のため、bench
/// harness には依存せず本ファイル内に独立して複製する）。
struct TestRng {
    state: u64,
}

impl TestRng {
    fn new(seed: u64) -> Self {
        let state = if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        };
        Self { state }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn next_f32(&mut self) -> f32 {
        let bits = (self.next_u64() >> 40) as u32;
        (bits as f32) / (1u32 << 24) as f32
    }

    fn next_vector(&mut self, dim: usize) -> Vec<f32> {
        (0..dim).map(|_| self.next_f32() * 2.0 - 1.0).collect()
    }
}

fn gen_corpus(seed: u64, dim: usize, rows: usize) -> Vec<f32> {
    let mut rng = TestRng::new(seed);
    let mut out = Vec::with_capacity(rows * dim);
    for _ in 0..rows {
        out.extend(rng.next_vector(dim));
    }
    out
}

const DIM: usize = 16;
const ROWS: usize = 800;
const SEED: u64 = 0xC0FF_EE12_3400;

fn build_fixture(params: HnswParams, seed: u64) -> (Vec<f32>, HnswIndex) {
    let vectors = gen_corpus(SEED, DIM, ROWS);
    let index = HnswIndex::build(params, DIM as u32, &vectors, seed).expect("build should succeed");
    (vectors, index)
}

#[test]
fn default_params_validate() {
    assert!(HnswParams::default().validate().is_ok());
}

#[test]
fn param_validation_rejects_out_of_range_values() {
    let cases = [
        HnswParams::default().with_m(0),
        HnswParams::default().with_m(1),
        HnswParams::default().with_m(MAX_M + 1),
        HnswParams::default().with_ef_construction(0),
        HnswParams::default().with_ef_construction(MAX_EF + 1),
        HnswParams::default().with_ef_search(0),
        HnswParams::default().with_ef_search(MAX_EF + 1),
    ];
    for params in cases {
        assert!(matches!(
            params.validate(),
            Err(HnswError::InvalidParams { .. })
        ));
        // validate で拒否されるパラメータは build でも同じ理由で拒否される
        // （build は validate を先頭で呼ぶ契約）。
        let vectors = gen_corpus(1, 4, 4);
        assert!(matches!(
            HnswIndex::build(params, 4, &vectors, 1),
            Err(HnswError::InvalidParams { .. })
        ));
    }
}

#[test]
fn build_rejects_dim_mismatch_and_zero_dim() {
    let vectors = vec![1.0f32, 2.0, 3.0];
    assert!(matches!(
        HnswIndex::build(HnswParams::default(), 4, &vectors, 1),
        Err(HnswError::DimMismatch { .. })
    ));
    assert!(matches!(
        HnswIndex::build(HnswParams::default(), 0, &vectors, 1),
        Err(HnswError::DimMismatch { .. })
    ));
}

#[test]
fn build_rejects_non_finite_vector() {
    let mut vectors = gen_corpus(2, 4, 4);
    vectors[5] = f32::NAN;
    assert!(matches!(
        HnswIndex::build(HnswParams::default(), 4, &vectors, 1),
        Err(HnswError::NonFiniteVector { .. })
    ));

    let mut vectors2 = gen_corpus(3, 4, 4);
    vectors2[9] = f32::INFINITY;
    assert!(matches!(
        HnswIndex::build(HnswParams::default(), 4, &vectors2, 1),
        Err(HnswError::NonFiniteVector { .. })
    ));
}

#[test]
fn build_rejects_overflow_induced_non_finite_score() {
    // codex-review #423 P1 指摘の回帰テスト: 各成分は有限（`f32::MAX` 近傍）
    // でも、内積計算（積・総和）は `f32` の範囲を超えて `Inf` へオーバーフロー
    // し得る。`NonFiniteVector` の入力検証（各成分の `is_finite()`）はこの
    // ケースを通してしまうため、`dot` の計算結果そのものを検証する
    // `HnswError::NonFiniteScore` が拒否することを確認する。
    let dim = 4usize;
    let big = f32::MAX / 2.0;
    // 各ノードは全成分が `big`（有限）の一様ベクトル。異なるノード同士の
    // `dot` は `dim * big * big` となり `f32::MAX` を大きく超えて `Inf` に
    // オーバーフローする。
    let vectors: Vec<f32> = (0..4).flat_map(|_| vec![big; dim]).collect();
    let err = HnswIndex::build(HnswParams::default(), dim as u32, &vectors, 1)
        .expect_err("overflowing dot product must be rejected");
    assert!(
        matches!(err, HnswError::NonFiniteScore { .. }),
        "expected NonFiniteScore, got {err:?}"
    );
}

#[test]
fn build_empty_input_yields_empty_index() {
    let index = HnswIndex::build(HnswParams::default(), 8, &[], 1).unwrap();
    assert_eq!(index.len(), 0);
    assert!(index.is_empty());
    assert_eq!(index.entry_point(), None);
    assert_eq!(index.max_level(), None);
}

#[test]
fn same_seed_produces_identical_graph() {
    let params = HnswParams::default()
        .with_m(8)
        .with_ef_construction(40)
        .with_ef_search(20);
    let (_, index_a) = build_fixture(params, 42);
    let (_, index_b) = build_fixture(params, 42);

    assert_eq!(index_a.len(), index_b.len());
    assert_eq!(index_a.entry_point(), index_b.entry_point());
    assert_eq!(index_a.max_level(), index_b.max_level());
    for node in 0..index_a.len() as u32 {
        assert_eq!(index_a.level_of(node), index_b.level_of(node));
        let level = index_a.level_of(node).unwrap();
        for l in 0..=level {
            assert_eq!(index_a.neighbors(l, node), index_b.neighbors(l, node));
        }
    }
}

#[test]
fn different_seed_yields_a_different_graph() {
    let params = HnswParams::default()
        .with_m(8)
        .with_ef_construction(40)
        .with_ef_search(20);
    let (_, index_a) = build_fixture(params, 1);
    let (_, index_b) = build_fixture(params, 2);

    // 少なくとも 1 ノードのレベルまたは隣接集合が異なることを確認する
    // （完全一致は事実上ありえないが、決定論的な検証として明示比較する）。
    let mut any_diff = index_a.max_level() != index_b.max_level();
    if !any_diff {
        for node in 0..index_a.len() as u32 {
            if index_a.level_of(node) != index_b.level_of(node) {
                any_diff = true;
                break;
            }
            let level = index_a.level_of(node).unwrap();
            for l in 0..=level {
                if index_a.neighbors(l, node) != index_b.neighbors(l, node) {
                    any_diff = true;
                    break;
                }
            }
            if any_diff {
                break;
            }
        }
    }
    assert!(
        any_diff,
        "different seeds should not yield an identical graph"
    );
}

#[test]
fn layer_structure_invariants_hold() {
    let params = HnswParams::default()
        .with_m(8)
        .with_ef_construction(40)
        .with_ef_search(20);
    let (_, index) = build_fixture(params, 7);

    let max_level = index.max_level().unwrap();
    assert!(max_level <= MAX_LEVEL);
    let entry = index.entry_point().unwrap();
    assert_eq!(index.level_of(entry), Some(max_level));

    let mut nodes_at_level_1_or_above = 0usize;
    for node in 0..index.len() as u32 {
        let level = index.level_of(node).unwrap();
        // 全ノードが層 0 に存在する。
        assert!(index.neighbors(0, node).is_some());
        // レベル l のノードは 0..=l 全層に隣接リスト（空でもよい）を持つ。
        for l in 0..=level {
            assert!(
                index.neighbors(l, node).is_some(),
                "node {node} missing layer {l} (level={level})"
            );
        }
        // レベルより上の層には属さない。
        assert!(index.neighbors(level + 1, node).is_none());
        if level >= 1 {
            nodes_at_level_1_or_above += 1;
        }
    }

    // レベル >= 1 のノード数が N/m のおおよその帯に収まること（緩い統計チェック。
    // 理論期待値は N * (1 - 1/m) 弱ではなく N/m に近い上位層在籍率）。
    let ratio = nodes_at_level_1_or_above as f64 / ROWS as f64;
    let expected = 1.0 / params.m as f64;
    assert!(
        ratio > expected * 0.3 && ratio < expected * 3.0,
        "ratio={ratio} expected~={expected}"
    );
}

/// `degree_limits_are_respected_and_links_are_well_formed`／
/// `degree_limits_are_respected_on_duplicate_heavy_corpus` 共通の不変条件
/// 検証（次数上限・自己ループなし・重複なし・隣接先の層整合）。
fn assert_degree_and_wellformed_invariants(index: &HnswIndex) {
    for node in 0..index.len() as u32 {
        let level = index.level_of(node).unwrap();
        for l in 0..=level {
            let neighbors = index.neighbors(l, node).unwrap();
            let limit = index.max_degree(l);
            assert!(
                neighbors.len() <= limit,
                "node {node} layer {l} exceeds degree limit: {} > {limit}",
                neighbors.len()
            );
            // 自己ループなし。
            assert!(!neighbors.contains(&node));
            // 重複なし。
            let mut sorted = neighbors.to_vec();
            sorted.sort_unstable(); // 集合の一意性検証のみに使う（順序規約とは無関係。id は全順序を持つ整数）
            sorted.dedup();
            assert_eq!(
                sorted.len(),
                neighbors.len(),
                "node {node} layer {l} has duplicate neighbors"
            );
            // 隣接先が範囲内かつその層に属する。
            for &n in neighbors {
                assert!((n as usize) < index.len());
                let n_level = index.level_of(n).unwrap();
                assert!(
                    n_level >= l,
                    "neighbor {n} of node {node} does not reach layer {l}"
                );
            }
        }
    }
}

#[test]
fn degree_limits_are_respected_and_links_are_well_formed() {
    let params = HnswParams::default()
        .with_m(6)
        .with_ef_construction(32)
        .with_ef_search(16);
    let (_, index) = build_fixture(params, 11);

    assert_degree_and_wellformed_invariants(&index);
    assert_eq!(index.max_degree(0), params.m * 2);
    assert_eq!(index.max_degree(1), params.m);
}

/// 到達性修復フェーズ 2（`hnsw.rs::repair_reachability`）は重複ヘビー
/// コーパスでは残存ノードの大半（数百件規模）を処理することが判明している
/// （codex-review #423 検証時の instrumentation で確認）。`build_fixture`
/// （ランダムコーパス）はフェーズ 2 をほとんど経由しないため、次数上限の
/// 単体テストをこの重複ヘビーコーパスにも適用し、フェーズ 2 の結線
/// （id 昇順の片方向チェーン＋ entry の犠牲リンク再結線。codex-review #423
/// P1・Bugbot 指摘の是正）が次数上限を破らないことを直接検証する。
#[test]
fn degree_limits_are_respected_on_duplicate_heavy_corpus() {
    let params = HnswParams::default()
        .with_m(6)
        .with_ef_construction(32)
        .with_ef_search(16);
    for seed in 0..10u64 {
        let vectors = gen_duplicate_heavy_corpus(seed, 12, 400, 5);
        let index = HnswIndex::build(params, 12, &vectors, seed).expect("build should succeed");
        assert_degree_and_wellformed_invariants(&index);
    }
}

/// 層 `level` 上でエントリポイントから幅優先探索し、到達可能なノード集合を返す。
fn bfs_reachable(index: &HnswIndex, level: usize, start: u32) -> std::collections::HashSet<u32> {
    let mut visited = std::collections::HashSet::new();
    let mut queue = std::collections::VecDeque::new();
    visited.insert(start);
    queue.push_back(start);
    while let Some(node) = queue.pop_front() {
        if let Some(neighbors) = index.neighbors(level, node) {
            for &n in neighbors {
                if visited.insert(n) {
                    queue.push_back(n);
                }
            }
        }
    }
    visited
}

#[test]
fn each_layer_is_connected_from_the_entry_point() {
    let params = HnswParams::default()
        .with_m(8)
        .with_ef_construction(48)
        .with_ef_search(24);
    let (_, index) = build_fixture(params, 55);
    let entry = index.entry_point().unwrap();
    let max_level = index.max_level().unwrap();
    // 上位層（level >= 1）が実際に構築・検証されることを保証する（上位層が
    // 常に 0 のまま偶然 green になる回帰を防ぐ）。
    assert!(
        max_level >= 1,
        "fixture did not exercise any upper layer (max_level={max_level})"
    );

    for level in 0..=max_level {
        // その層に属するノード集合を集める。
        let members: std::collections::HashSet<u32> = (0..index.len() as u32)
            .filter(|&n| index.level_of(n).map(|l| l >= level).unwrap_or(false))
            .collect();
        assert!(
            members.contains(&entry),
            "entry point missing from layer {level}"
        );
        let reachable = bfs_reachable(&index, level, entry);
        let missing: Vec<u32> = members.difference(&reachable).copied().collect();
        assert!(
            missing.is_empty(),
            "layer {level} not fully connected from entry point; unreachable nodes: {missing:?}"
        );
    }
}

/// `each_layer_is_connected_from_the_entry_point` と同じ判定（各層でエント
/// リポイントからの BFS が全メンバへ到達する）を任意の索引へ適用する共通化
/// ヘルパ。ランダム構成・重複ヘビーコーパスの連結性テスト（下記）から再利用
/// する。
fn assert_fully_connected_from_entry(index: &HnswIndex) {
    let Some(entry) = index.entry_point() else {
        return;
    };
    let Some(max_level) = index.max_level() else {
        return;
    };
    for level in 0..=max_level {
        let members: std::collections::HashSet<u32> = (0..index.len() as u32)
            .filter(|&n| index.level_of(n).map(|l| l >= level).unwrap_or(false))
            .collect();
        let reachable = bfs_reachable(index, level, entry);
        let missing: Vec<u32> = members.difference(&reachable).copied().collect();
        assert!(
            missing.is_empty(),
            "level {level} not fully connected from entry point; unreachable nodes: {missing:?}"
        );
    }
}

/// `clusters` 個のクラスタ中心を生成し、各行をいずれかの中心の**完全な複製**
/// にする（ジッタなし）。同一クラスタに属する行同士は任意のクエリに対し
/// 常に厳密同点スコアになるため、逆方向リンクの枝刈り・`search_layer` の
/// 停止／受理判定の双方を同時に強くストレスする決定的コーパス
/// （`docs/design/hnsw-graph-construction.md`「逆方向リンクの到達性保証」
/// 「`search_layer` の停止・受理判定: 順序規約の使い分け」各節参照）。
fn gen_duplicate_heavy_corpus(seed: u64, dim: usize, rows: usize, clusters: usize) -> Vec<f32> {
    let mut rng = TestRng::new(seed);
    let centers: Vec<Vec<f32>> = (0..clusters.max(1)).map(|_| rng.next_vector(dim)).collect();
    let mut out = Vec::with_capacity(rows * dim);
    for i in 0..rows {
        let center = &centers[i % centers.len()];
        out.extend_from_slice(center);
    }
    out
}

/// 逆方向リンク枝刈りによる到達不能・`search_layer` の同点早期打ち切りは
/// いずれも固定 1 fixture（`build_fixture`／`SEED`）では顕在化しなかった
/// 入力依存のバグだったため、複数 seed × 複数 `(rows, dim, m)` のランダム
/// 構成、および完全同点スコアを誘発する重複ヘビーコーパスの双方で最終
/// グラフの全層連結性を検証する（受け入れ条件 (a) の拡張）。
#[test]
fn randomized_configs_and_duplicate_heavy_corpus_stay_fully_connected() {
    struct Config {
        dim: usize,
        rows: usize,
        m: usize,
    }
    let configs = [
        Config {
            dim: 8,
            rows: 300,
            m: 4,
        },
        Config {
            dim: 16,
            rows: 500,
            m: 8,
        },
        Config {
            dim: 32,
            rows: 200,
            m: 16,
        },
    ];
    for (ci, cfg) in configs.iter().enumerate() {
        for seed_idx in 0..10u64 {
            let seed = (0xA5A5_0000_0000_0000u64) ^ ((ci as u64) << 32) ^ seed_idx;
            let vectors = gen_corpus(seed ^ 0x1111_1111, cfg.dim, cfg.rows);
            let params = HnswParams::default()
                .with_m(cfg.m)
                .with_ef_construction(48)
                .with_ef_search(24);
            let index = HnswIndex::build(params, cfg.dim as u32, &vectors, seed)
                .expect("build should succeed");
            assert_fully_connected_from_entry(&index);
        }
    }

    for seed in 0..10u64 {
        let vectors = gen_duplicate_heavy_corpus(seed, 12, 400, 5);
        let params = HnswParams::default()
            .with_m(6)
            .with_ef_construction(32)
            .with_ef_search(16);
        let index = HnswIndex::build(params, 12, &vectors, seed).expect("build should succeed");
        assert_fully_connected_from_entry(&index);
    }
}

#[test]
fn build_rejects_node_count_beyond_max_hnsw_nodes() {
    // dim=1 なら MAX_HNSW_NODES+1 行分でも約 4 MB で確保可能（テストとして許容範囲）。
    // 上限超過は build の入力検証の先頭付近（次元整合の直後）で弾かれ、実際の
    // グラフ構築（O(N log N) 級の処理）へは進まない契約であることを確認する。
    let vectors = vec![0.0f32; MAX_HNSW_NODES + 1];
    assert!(matches!(
        HnswIndex::build(HnswParams::default(), 1, &vectors, 1),
        Err(HnswError::TooManyNodes { .. })
    ));
}
/// Issue #494: 凍結時の CSR 平坦化・`search_layer` の CSR 参照化の前後で、
/// `build` が返すグラフ（`entry_point`／各ノードの `level_of`／各層の
/// `neighbors`）がビット同一であることを固定する回帰テスト。
///
/// 本テストは公開 API（`entry_point()`／`level_of()`／`neighbors()`）のみを
/// 走査し、ノード id 昇順 → レベル昇順 → 各層のリンク列（返された順序の
/// まま。並べ替えない）で FNV-1a 64bit ハッシュへ投入する。CSR 化は
/// 隣接リストの要素順を一切変えない契約（`docs/design/hnsw-index.md` §14）
/// のため、この値は表現変更の前後で完全に一致するはずである。
///
/// 期待値は本 Issue の実装前（CSR 化前・commit `2ca1536` 時点の
/// `Vec<Vec<u32>>` 表現）で 1 度だけ採取した固定値。以降はこの値からの
/// 逸脱を「グラフの構築結果が変わった」として検出する安全網とする。
#[test]
fn graph_fingerprint_is_stable_across_representation_change() {
    /// FNV-1a 64bit（依存追加なしの自作実装。鍵・トークン等のセキュリティ
    /// 用途には使わない非暗号ハッシュ）。
    fn fnv1a_update(mut hash: u64, bytes: &[u8]) -> u64 {
        const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
        for &b in bytes {
            hash ^= b as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        hash
    }

    const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;

    let dim = 8usize;
    let n = 400usize;
    let vectors = gen_corpus(0x00C0_FFEEu64, dim, n);
    let params = HnswParams::default().with_m(8).with_ef_construction(40);
    let index = HnswIndex::build(params, dim as u32, &vectors, 0x00C0_FFEEu64)
        .expect("build must succeed on this deterministic corpus");

    let mut hash = FNV_OFFSET_BASIS;
    hash = fnv1a_update(hash, &(index.len() as u64).to_le_bytes());
    hash = fnv1a_update(
        hash,
        &index
            .entry_point()
            .map(|e| e as i64)
            .unwrap_or(-1)
            .to_le_bytes(),
    );
    hash = fnv1a_update(
        hash,
        &index
            .max_level()
            .map(|l| l as i64)
            .unwrap_or(-1)
            .to_le_bytes(),
    );

    for node in 0..index.len() as u32 {
        let level = index
            .level_of(node)
            .expect("node within len() must have a level");
        hash = fnv1a_update(hash, &(level as u64).to_le_bytes());
        for l in 0..=level {
            let neighbors = index
                .neighbors(l, node)
                .expect("level <= level_of(node) must yield Some");
            hash = fnv1a_update(hash, &(neighbors.len() as u64).to_le_bytes());
            for &nb in neighbors {
                hash = fnv1a_update(hash, &nb.to_le_bytes());
            }
        }
    }

    // 採取元コミット: 2ca1536（CSR 化前・`Vec<Vec<u32>>` 表現。手順は本テスト
    // 冒頭ドキュメンテーションコメント参照）。
    const EXPECTED_FINGERPRINT: u64 = 0x5597_d0e9_0e1e_9898;
    assert_eq!(
        hash, EXPECTED_FINGERPRINT,
        "graph fingerprint changed (got {hash:#x}); re-check CSR flattening preserves link order"
    );
}

/// `repair_reachability` の観測統計（Issue #447）が非 vacuous であることを、
/// フェーズ 2（片方向チェーン結線）が確実に発火する重複ヘビーコーパス
/// （`gen_duplicate_heavy_corpus`。完全同点スコアを誘発しフェーズ 1 の
/// `PRECISE_REPAIR_CAP` を使い切りやすい）で固定する。`tests/hnsw.rs` の
/// 既存流儀（crate 外の公開 API のみ）に従い `build_with_threads_observed`
/// （`threads=1` の縮退経路。`profile.repair` のみを埋める設計。
/// `HnswIndex::build_with_threads_observed` のドキュメンテーションコメント
/// 参照）経由で観測する。
#[test]
fn repair_stats_on_duplicate_heavy_corpus_report_phase2_nodes() {
    let dim = 12usize;
    let rows = 400usize;
    let clusters = 5usize;
    let vectors = gen_duplicate_heavy_corpus(0x5EED_0001, dim, rows, clusters);
    let params = HnswParams::default()
        .with_m(6)
        .with_ef_construction(32)
        .with_ef_search(16);

    let (index, profile) =
        HnswIndex::build_with_threads_observed(params, dim as u32, &vectors, 0x5EED_0001, 1)
            .expect("build should succeed");

    assert_degree_and_wellformed_invariants(&index);
    assert_fully_connected_from_entry(&index);

    let phase2_total: u64 = profile.repair.levels.iter().map(|l| l.phase2_nodes).sum();
    let cap_hits_total: u64 = profile
        .repair
        .levels
        .iter()
        .filter(|l| l.phase1_cap_hit)
        .count() as u64;
    assert!(
        phase2_total > 0,
        "duplicate-heavy corpus should force phase 2 chain-linking to fire at least once"
    );
    assert!(
        cap_hits_total > 0,
        "duplicate-heavy corpus should exhaust PRECISE_REPAIR_CAP on at least one level"
    );
}

/// 同一 seed・同一入力で 2 回構築した場合、`repair` 統計のカウンタ系
/// フィールド（`wall` 系の壁時間を除く）が完全に一致することを固定する
/// （逐次経路は完全決定的。`docs/design/hnsw-parallel-build.md` の並列経路
/// 非決定性契約とは無関係）。
#[test]
fn repair_stats_are_deterministic_on_sequential_path() {
    let dim = 12usize;
    let rows = 400usize;
    let clusters = 5usize;
    let vectors = gen_duplicate_heavy_corpus(0x5EED_0002, dim, rows, clusters);
    let params = HnswParams::default()
        .with_m(6)
        .with_ef_construction(32)
        .with_ef_search(16);

    let (_, profile_a) =
        HnswIndex::build_with_threads_observed(params, dim as u32, &vectors, 0x5EED_0002, 1)
            .expect("build should succeed");
    let (_, profile_b) =
        HnswIndex::build_with_threads_observed(params, dim as u32, &vectors, 0x5EED_0002, 1)
            .expect("build should succeed");

    assert_eq!(profile_a.repair.levels.len(), profile_b.repair.levels.len());
    for (a, b) in profile_a
        .repair
        .levels
        .iter()
        .zip(profile_b.repair.levels.iter())
    {
        assert_eq!(a.level, b.level);
        assert_eq!(a.unreachable_before, b.unreachable_before);
        assert_eq!(a.phase1_iterations, b.phase1_iterations);
        assert_eq!(a.phase1_cap_hit, b.phase1_cap_hit);
        assert_eq!(a.phase2_nodes, b.phase2_nodes);
        assert_eq!(a.phase2_entry_relinked, b.phase2_entry_relinked);
    }
}

/// HNSW 構築の並列化（Issue #406）の不変条件テスト。要素単位 `RwLock`・
/// エントリポイント更新のみ排他という設計が、逐次構築と同じ次数上限・
/// 連結性・レベル割当を維持することを検証する（実装計画 §6.2）。
mod parallel_build_invariants {
    use super::*;

    /// `n > SEQUENTIAL_PREFIX_NODES` を満たす規模のコーパスを生成する
    /// （`gen_corpus`／`gen_duplicate_heavy_corpus` はそのまま再利用する）。
    const PARALLEL_ROWS: usize = SEQUENTIAL_PREFIX_NODES + 800;

    #[test]
    fn build_with_threads_zero_or_over_max_is_rejected() {
        let vectors = gen_corpus(1, 4, 10);
        assert!(matches!(
            HnswIndex::build_with_threads(HnswParams::default(), 4, &vectors, 1, 0),
            Err(HnswError::InvalidParams { .. })
        ));
        assert!(matches!(
            HnswIndex::build_with_threads(
                HnswParams::default(),
                4,
                &vectors,
                1,
                MAX_BUILD_THREADS + 1
            ),
            Err(HnswError::InvalidParams { .. })
        ));
    }

    /// 全ノードのレベル割当（`seed` 由来）はスレッド数に依らず不変
    /// （要件 4）。並列フェーズは挿入順序のみを非決定化し、レベル割当は
    /// 並列フェーズ開始前に逐次確定するため、`threads` を変えてもレベル
    /// 列は完全一致する。
    #[test]
    fn levels_are_invariant_across_thread_counts() {
        let dim = 8usize;
        let vectors = gen_corpus(0x5EED_5EED, dim, PARALLEL_ROWS);
        let params = HnswParams::default()
            .with_m(8)
            .with_ef_construction(40)
            .with_ef_search(20);
        let seed = 123u64;

        let seq = HnswIndex::build(params, dim as u32, &vectors, seed).unwrap();
        for threads in [2usize, 4, 8] {
            let par =
                HnswIndex::build_with_threads(params, dim as u32, &vectors, seed, threads).unwrap();
            assert_eq!(seq.len(), par.len());
            for node in 0..seq.len() as u32 {
                assert_eq!(
                    seq.level_of(node),
                    par.level_of(node),
                    "threads={threads} node={node}"
                );
            }
            // `repair_reachability` は `0..=level_of(entry_point)` の層しか
            // 走査しない（`hnsw.rs::repair_reachability` のドキュメンテーション
            // コメント参照）ため、並列フェーズのエントリポイント更新競合
            // （`parallel_build::BuildGraph::try_promote_entry`）が、真の
            // 最大レベルより低いノードをエントリポイントに残してしまうと、
            // それより高いレベルのノードが後始末の対象から漏れて到達不能に
            // なりかねない。全ノードのレベルがエントリポイントのレベルを
            // 超えないこと（＝エントリポイントが常に真の最大レベルを保持
            // すること）を明示的に固定する。
            let max_level = par.max_level().unwrap();
            for node in 0..par.len() as u32 {
                assert!(
                    par.level_of(node).unwrap() <= max_level,
                    "threads={threads} node={node} level exceeds entry point's max_level"
                );
            }
        }
    }

    /// `threads = 4` で複数 seed × 複数 `(dim, rows, m)` を構築し、次数上限・
    /// 自己ループなし・重複なし・隣接先の層整合・エントリポイントからの
    /// 全層連結性を検証する（CI runner のコア数に依存させないよう `threads`
    /// は固定値で指定する）。
    #[test]
    fn parallel_build_maintains_degree_and_connectivity_invariants() {
        struct Config {
            dim: usize,
            rows: usize,
            m: usize,
        }
        let configs = [
            Config {
                dim: 8,
                rows: SEQUENTIAL_PREFIX_NODES + 300,
                m: 6,
            },
            Config {
                dim: 16,
                rows: SEQUENTIAL_PREFIX_NODES + 700,
                m: 10,
            },
        ];
        for (ci, cfg) in configs.iter().enumerate() {
            for seed_idx in 0..4u64 {
                let seed = (0xB0B0_0000_0000_0000u64) ^ ((ci as u64) << 32) ^ seed_idx;
                let vectors = gen_corpus(seed ^ 0x2222_2222, cfg.dim, cfg.rows);
                let params = HnswParams::default()
                    .with_m(cfg.m)
                    .with_ef_construction(48)
                    .with_ef_search(24);
                let index =
                    HnswIndex::build_with_threads(params, cfg.dim as u32, &vectors, seed, 4)
                        .expect("parallel build should succeed");
                assert_degree_and_wellformed_invariants(&index);
                assert_fully_connected_from_entry(&index);
            }
        }
    }

    /// 重複ヘビーコーパス（完全同点スコアを誘発）でも並列構築が次数上限・
    /// 連結性を破らないことを確認する（`tests/hnsw.rs` の逐次版
    /// `degree_limits_are_respected_on_duplicate_heavy_corpus` の並列版）。
    #[test]
    fn parallel_build_on_duplicate_heavy_corpus_maintains_invariants() {
        let params = HnswParams::default()
            .with_m(6)
            .with_ef_construction(32)
            .with_ef_search(16);
        for seed in 0..4u64 {
            let vectors = gen_duplicate_heavy_corpus(seed, 12, PARALLEL_ROWS, 5);
            let index = HnswIndex::build_with_threads(params, 12, &vectors, seed, 4)
                .expect("parallel build should succeed");
            assert_degree_and_wellformed_invariants(&index);
            assert_fully_connected_from_entry(&index);
        }
    }

    /// 構築入力に非有限値由来のオーバーフロースコアを混入させた場合、
    /// 並列ワーカーの `Err` が停止フラグ経由で正しく伝播し、panic せず
    /// `Err` を返すことを確認する（実装計画 §6.2 の失敗伝播テスト）。
    ///
    /// PR #431 codex-review（Cursor Bugbot）Medium 指摘の回帰: 旧実装は
    /// 全件（先頭 [`SEQUENTIAL_PREFIX_NODES`] 件を含む）がオーバーフローを
    /// 誘発するコーパスだったため、逐次プレフィックス挿入（`insert_node_
    /// locked` を単一スレッドで直列に呼ぶループ）の時点で既に `Err` が
    /// 返り、並列ワーカーループ・`first_error` mutex・`stop` フラグが
    /// 一度も実行されないまま失敗していた。逐次プレフィックス分をゼロ
    /// ベクトル（互いの `dot` は常に厳密に `0.0` で有限。`0.0 * 有限値`
    /// は常に `0.0`）にし、並列フェーズでのみ挿入される後半だけをオーバー
    /// フロー誘発ベクトルに変えることで、逐次プレフィックスは必ず成功し、
    /// 並列ワーカーが挿入中に初めて `NonFiniteScore` を検出して `stop`
    /// フラグを立てる経路を実際に通す。
    ///
    /// 決定性の根拠: `search_layer_locked` はタイスコア（ここでは全ゼロ
    /// ベクトル同士の `dot == 0.0`）では `strictly_farther` による打ち切り
    /// が働かず、到達可能な全ノードを走査し尽くすまで探索を続ける
    /// （同関数の実装参照）。オーバーフロー誘発ベクトルは十分な数
    /// （`OVERFLOW_TAIL_NODES`。並列スレッド数より十分大きく取り、初回の
    /// 数件が真に並行して衝突なく挿入され得ても後続では既に確定済みの
    /// オーバーフローノードが必ずグラフへ連結済みになるようにする）だけ
    /// 混入するため、後続の挿入が全ゼロ成分を辿るいずれかの経路で先行
    /// オーバーフローノードへ到達し、`score_of` がその場で `Err` を返す。
    ///
    /// この `Err` がワーカー段（逐次プレフィックス段ではなく）由来である
    /// ことは構造的に保証される: `build_parallel_graph`（本テストが呼ぶ
    /// `HnswIndex::build_with_threads` の内部実装）は逐次プレフィックス
    /// ループを `?` で早期リターンするため、そこで失敗すれば並列スコープへ
    /// 到達しない。かつ、逐次プレフィックス後始末（`repair_reachability`
    /// を含む `freeze`）はワーカーの `first_error` が `None` の場合にしか
    /// 呼ばれない。したがって `NonFiniteScore` を観測できるのは、逐次
    /// プレフィックス（本テストではゼロベクトルのみで常に有限）か、
    /// ワーカーの `insert_node_locked` 呼び出しのいずれかに限られ、前者を
    /// 排除した本テストでは後者からの伝播であることが保証される。
    #[test]
    fn parallel_build_propagates_worker_errors_without_panicking() {
        const OVERFLOW_TAIL_NODES: usize = 64;

        let dim = 4usize;
        let big = f32::MAX / 2.0;
        let mut vectors: Vec<f32> =
            Vec::with_capacity((SEQUENTIAL_PREFIX_NODES + OVERFLOW_TAIL_NODES) * dim);
        vectors.extend(std::iter::repeat_n(0.0f32, SEQUENTIAL_PREFIX_NODES * dim));
        vectors.extend(std::iter::repeat_n(big, OVERFLOW_TAIL_NODES * dim));

        let err = HnswIndex::build_with_threads(HnswParams::default(), dim as u32, &vectors, 1, 4)
            .expect_err("overflowing dot product must be rejected even in the parallel path");
        assert!(
            matches!(err, HnswError::NonFiniteScore { .. }),
            "expected NonFiniteScore, got {err:?}"
        );
    }
}
