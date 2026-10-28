//! `hnsw::provider::HnswSearchProvider`（Issue #407）の外部公開 API のみを使った
//! 契約テスト。クレート内 `#[cfg(test)]`（`hnsw/provider.rs` 内）は内部関数を直接
//! 使えるが、本ファイルは `crates/wire-server` 等の外部利用者と同じ到達性
//! （`engine::hnsw::provider::HnswSearchProvider`・`engine::kernel::*`）でのみ検証する。

use engine::hnsw::provider::HnswSearchProvider;
use engine::hnsw::{HnswParams, ValidatedHnswParams};
use engine::kernel::{CpuScalarProvider, KernelError, SearchInput, SearchProvider};

fn corpus(n: usize, dim: usize) -> (Vec<u64>, Vec<f32>) {
    let ids: Vec<u64> = (0..n as u64).collect();
    // 決定的だが単純な生成（外部公開 API のみで完結させるため xorshift 等は使わない）。
    let mut vectors = Vec::with_capacity(n * dim);
    for i in 0..n {
        for d in 0..dim {
            let v = ((i * 31 + d * 7) % 17) as f32 / 17.0 - 0.5;
            vectors.push(v);
        }
    }
    (ids, vectors)
}

// SearchProvider trait 実装として登録できること（object-safety・注入点の型適合）。
#[test]
fn hnsw_provider_is_object_safe_search_provider() {
    let provider = HnswSearchProvider::new(ValidatedHnswParams::default());
    let _boxed: Box<dyn SearchProvider> = Box::new(provider);
}

// 次元不一致・非有限クエリ・空入力・k==0 のエラー契約が `kernel.rs::SearchProvider`
// 共通契約どおりであることを確認する（`ParallelSearchProvider` へのフォールバック契約。
// `search_engine.rs` モジュールドキュメント「本タスク時点は全件 brute-force
// フォールバック」参照）。
#[test]
fn hnsw_provider_input_validation_matches_provider_contract() {
    let provider = HnswSearchProvider::new(ValidatedHnswParams::default());
    let (ids, vectors) = corpus(5, 4);

    // 次元不一致。
    let bad_query = vec![0.0_f32, 0.0, 0.0];
    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 4,
        query: &bad_query,
        k: 2,
    };
    assert_eq!(
        provider.search(input).unwrap_err(),
        KernelError::DimMismatch {
            expected: 4,
            found: 3
        }
    );

    // 非有限クエリ。
    let nan_query = vec![f32::NAN, 0.0, 0.0, 0.0];
    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 4,
        query: &nan_query,
        k: 2,
    };
    assert_eq!(
        provider.search(input).unwrap_err(),
        KernelError::NonFiniteQuery
    );

    // k == 0 は空 Ok。
    let query = vec![0.0_f32, 0.0, 0.0, 0.0];
    let input = SearchInput {
        ids: &ids,
        vectors: &vectors,
        dim: 4,
        query: &query,
        k: 0,
    };
    assert_eq!(provider.search(input).unwrap(), Vec::new());
}

// フォールバック契約（Top-k・同点タイブレークまで含めた bit 単位一致）を
// 参照実装 `CpuScalarProvider` との比較で確認する（外部公開 API のみで完結）。
#[test]
fn hnsw_provider_matches_cpu_scalar_reference_via_public_api() {
    let dim = 4usize;
    let (ids, vectors) = corpus(30, dim);
    let query = vec![0.1_f32, -0.2, 0.05, 0.3];

    let hnsw_provider = HnswSearchProvider::new(ValidatedHnswParams::default());
    let reference = CpuScalarProvider;

    for k in [1usize, 5, 10, 30] {
        let input_hnsw = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: dim as u32,
            query: &query,
            k,
        };
        let input_ref = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: dim as u32,
            query: &query,
            k,
        };
        let got = hnsw_provider.search(input_hnsw).unwrap();
        let want = reference.search(input_ref).unwrap();
        assert_eq!(got, want, "k={k}");
    }
}

// `params()`／`effective_ef()` が構築時の値・契約どおりに動くことを確認する
// （#408 が使う契約関数。`effective_ef` の境界: k<ef／k>ef／k>MAX_EF）。
#[test]
fn hnsw_provider_params_and_effective_ef_public_contract() {
    let params = HnswParams::default()
        .with_m(12)
        .with_ef_construction(80)
        .with_ef_search(20);
    let provider = HnswSearchProvider::new(ValidatedHnswParams::new(params).unwrap());
    assert_eq!(provider.params(), params);

    assert_eq!(provider.effective_ef(5), 20); // k <= ef_search
    assert_eq!(provider.effective_ef(50), 50); // k > ef_search
    assert_eq!(
        provider.effective_ef(engine::hnsw::MAX_EF + 1),
        engine::hnsw::MAX_EF
    ); // untrusted k は MAX_EF でクランプ
}

// 不正な `HnswParams` は `ValidatedHnswParams::new`（外部公開 API）の時点で
// 拒否され、`HnswSearchProvider::new` へは到達しない（codex-review P1 指摘・
// Issue #407・PR #433 追記。`HnswSearchProvider::new` は `ValidatedHnswParams`
// 以外を受け取れないため、この不変条件は型で保証される）。
#[test]
fn validated_hnsw_params_new_rejects_invalid_params_via_public_api() {
    let invalid = HnswParams::default().with_m(1); // HnswParams::validate は m < 2 を拒否する
    assert!(ValidatedHnswParams::new(invalid).is_err());
}
