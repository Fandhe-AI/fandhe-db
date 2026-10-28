//! `parallel_search.rs::ParallelSearchProvider` の結合テスト（TASK-126・対象ビヘイビア:
//! CORE-3, CORE-4, CORE-5・SEARCH-4）。
//!
//! 両 provider（`CpuScalarProvider`・`ParallelSearchProvider`）とも総当たり実装かつ内積計算
//! （`kernel.rs::dot`）を共有するため、決定的シードの合成データに対して Top-k 選出
//! 集合・順序・スコア値が dim・並列度に関わらず bit 単位で完全一致することを検証する
//! （近似検索ではなく、参照実装との等価性そのものを主張する。Issue #34 codex-review P1
//! 指摘対応: 以前は provider 側に別の加算順序を持つ自前ベクトル化があり、`dim >= 16` で
//! 丸め誤差により一致が崩れ得た）。
//!
//! `EngineCore` 経由の CORE-1/2/13 回帰は `tests/vector_core.rs` が既定構成
//! （`ParallelSearchProvider`）でそのまま担保する（本ファイルは provider 単体の契約検証）。

use engine::kernel::{CandidateHit, CpuScalarProvider, KernelError, SearchInput, SearchProvider};
use engine::parallel_search::ParallelSearchProvider;

/// テスト専用の決定的シード xorshift64*（`benches/harness/rng.rs` と同系だが、
/// 結合テストは bench クレートに依存しないため個別に持つ）。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
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

/// 決定的シードで `(ids, vectors, query)` を合成する。
fn synth_case(seed: u64, n: usize, dim: usize) -> (Vec<u64>, Vec<f32>, Vec<f32>) {
    let mut rng = Rng::new(seed);
    let ids: Vec<u64> = (0..n as u64).collect();
    let mut vectors = Vec::with_capacity(n * dim);
    for _ in 0..n {
        vectors.extend(rng.next_vector(dim));
    }
    let query = rng.next_vector(dim);
    (ids, vectors, query)
}

/// 両 provider の Top-k（集合・順序・スコア値）が bit 単位で完全一致することを検証する。
/// `dot` を共有する構造上、加算順序の分岐は起こり得ないため、境界のスコア差（マージン）
/// に関わらず常に厳密一致を主張できる。
fn assert_top_k_matches(ids: &[u64], vectors: &[f32], dim: u32, query: &[f32], k: usize) {
    let scalar_input = SearchInput {
        ids,
        vectors,
        dim,
        query,
        k,
    };
    let simd_input = SearchInput {
        ids,
        vectors,
        dim,
        query,
        k,
    };
    let scalar = CpuScalarProvider.search(scalar_input).expect("scalar ok");
    let simd = ParallelSearchProvider.search(simd_input).expect("simd ok");
    assert_eq!(
        scalar, simd,
        "top-k (ids/order/scores) must match bit-for-bit"
    );
}

#[test]
fn matches_scalar_reference_for_various_n_dim_k() {
    // n・dim（8 の倍数／非倍数を両方含む）・k（k > n・通常ケース）の組み合わせ。
    let cases: &[(u64, usize, usize, usize)] = &[
        (1, 5, 3, 2),
        (2, 5, 3, 10), // k > n
        (3, 50, 8, 5), // dim = 8 の丁度境界
        (4, 50, 9, 5), // dim = 8 の 1 つ上（remainder 1）
        (5, 200, 15, 7),
        (6, 200, 16, 7), // dim = 16 の丁度境界（複数チャンクへ入る最小）
        (7, 500, 33, 12),
        (8, 500, 64, 12),
        // Issue #510（TASK-156・CORE-14）: 4 行ブロックカーネル化の回帰用に、
        // ブロック境界（4 の倍数・非倍数）と AVX2/AVX-512 のレーン境界（8・16）を
        // 横断する組み合わせを追加する。
        (9, 4, 16, 3),    // n = ちょうど 1 ブロック
        (10, 3, 8, 2),    // n < 4（ブロックなし・端数のみ）
        (11, 9, 100, 5),  // 2 ブロック + 端数 1・dim は 8/16 いずれの倍数でもない
        (12, 10, 129, 4), // dim = 8 の倍数 + 1（AVX2 tail 1 要素）
        (13, 21, 768, 6), // dim = 16 の倍数（AVX-512 レーン境界）
    ];
    for &(seed, n, dim, k) in cases {
        let (ids, vectors, query) = synth_case(seed, n, dim);
        assert_top_k_matches(&ids, &vectors, dim as u32, &query, k);
    }
}

#[test]
fn matches_scalar_reference_with_duplicate_vectors_tie() {
    // 同一ベクトルの複製による同点を、選出段のタイブレーク（id 昇順）が両 provider で
    // 一致することの確認に使う。
    let dim = 8usize;
    let mut rng = Rng::new(42);
    let base = rng.next_vector(dim);
    let n = 6;
    let ids: Vec<u64> = (0..n as u64).collect();
    let mut vectors = Vec::with_capacity(n * dim);
    for _ in 0..n {
        vectors.extend(base.clone());
    }
    let query = rng.next_vector(dim);
    let k = 3;

    let scalar_input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: dim as u32,
        query: &query,
        k,
    };
    let simd_input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: dim as u32,
        query: &query,
        k,
    };
    let scalar = CpuScalarProvider.search(scalar_input).expect("scalar ok");
    let simd = ParallelSearchProvider.search(simd_input).expect("simd ok");
    assert_eq!(scalar, simd);
    // 同点タイブレークで id 昇順の最小 k 件（0,1,2）が選ばれるはず。
    let selected_ids: Vec<u64> = simd.iter().map(|h| h.id).collect();
    assert_eq!(selected_ids, vec![0, 1, 2]);
}

#[test]
fn matches_scalar_reference_with_non_finite_rows_mixed_in() {
    let ids = [1u64, 2, 3, 4, 5];
    let dim = 4usize;
    // id=3 の行に NaN、id=5 の行に Inf を混入させる。
    #[rustfmt::skip]
    let vectors = [
        1.0, 0.0, 0.0, 0.0,
        2.0, 0.0, 0.0, 0.0,
        f32::NAN, 0.0, 0.0, 0.0,
        3.0, 0.0, 0.0, 0.0,
        f32::INFINITY, 0.0, 0.0, 0.0,
    ];
    let query = [1.0, 0.0, 0.0, 0.0];
    let scalar_input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: dim as u32,
        query: &query,
        k: 3,
    };
    let simd_input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: dim as u32,
        query: &query,
        k: 3,
    };
    let scalar = CpuScalarProvider.search(scalar_input).expect("scalar ok");
    let simd = ParallelSearchProvider.search(simd_input).expect("simd ok");
    assert_eq!(scalar, simd);
    assert_eq!(
        simd,
        vec![
            CandidateHit { id: 4, score: 3.0 },
            CandidateHit { id: 2, score: 2.0 },
            CandidateHit { id: 1, score: 1.0 },
        ]
    );
}

#[test]
fn multi_thread_path_matches_scalar_reference_at_scale() {
    // `parallel_search.rs::MIN_ROWS_PER_THREAD` を超える規模でマルチスレッド経路を
    // 実際に使わせたうえで、スカラー参照実装と一致することを確認する（CORE-3・SEARCH-4）。
    let dim = 32usize;
    let n = 5000;
    let (ids, vectors, query) = synth_case(99, n, dim);
    assert_top_k_matches(&ids, &vectors, dim as u32, &query, 20);
}

#[test]
fn error_contract_matches_scalar_reference() {
    let ids = [1u64];
    let vectors = [1.0f32, 0.0];

    // 次元不一致
    let bad_query = [1.0f32, 0.0, 0.0];
    let scalar_err = CpuScalarProvider
        .search(SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &bad_query,
            k: 1,
        })
        .unwrap_err();
    let simd_err = ParallelSearchProvider
        .search(SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &bad_query,
            k: 1,
        })
        .unwrap_err();
    assert_eq!(scalar_err, simd_err);
    assert_eq!(
        simd_err,
        KernelError::DimMismatch {
            expected: 2,
            found: 3
        }
    );

    // 非有限クエリ
    let nan_query = [f32::NAN, 0.0];
    let scalar_err = CpuScalarProvider
        .search(SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &nan_query,
            k: 1,
        })
        .unwrap_err();
    let simd_err = ParallelSearchProvider
        .search(SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &nan_query,
            k: 1,
        })
        .unwrap_err();
    assert_eq!(scalar_err, simd_err);
    assert_eq!(simd_err, KernelError::NonFiniteQuery);

    // 空入力・k=0
    let empty_ids: [u64; 0] = [];
    let empty_vectors: [f32; 0] = [];
    let query = [1.0f32, 0.0];
    let scalar_hits = CpuScalarProvider
        .search(SearchInput {
            ids: &empty_ids,
            vectors: &empty_vectors,
            dim: 2,
            query: &query,
            k: 5,
        })
        .expect("scalar ok");
    let simd_hits = ParallelSearchProvider
        .search(SearchInput {
            ids: &empty_ids,
            vectors: &empty_vectors,
            dim: 2,
            query: &query,
            k: 5,
        })
        .expect("simd ok");
    assert!(scalar_hits.is_empty());
    assert!(simd_hits.is_empty());

    let simd_k0 = ParallelSearchProvider
        .search(SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 0,
        })
        .expect("simd ok");
    assert!(simd_k0.is_empty());
}
