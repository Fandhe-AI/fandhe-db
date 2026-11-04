//! HNSW を `kernel.rs::SearchProvider` として結線するアダプタ（Issue #407・
//! `search_engine.rs::SearchEngineKind::Hnsw` の構築先。ADR
//! `docs/design/ann-index-adoption.md` B 案の Phase 3 分解タスク #404〜#413 のうち、
//! 「エンジン選択の opt-in 経路」を担う）。
//!
//! # 本タスク時点の契約: 全件 brute-force フォールバック
//!
//! [`HnswSearchProvider`] は本 Issue 時点では `hnsw.rs::HnswIndex` を構築・保持
//! **しない**。`SearchProvider::search` は常に
//! [`crate::parallel_search::ParallelSearchProvider`] へ委譲する。
//!
//! 理由: `kernel.rs::SearchInput` は「呼び出しごとに集合が変わりうる可視行の縮約
//! ビュー」（RLS フィルタ・テーブル更新のたびに変わる）であり、`HnswIndex` は
//! 構築時点のスナップショットしか探索できない。索引済み集合と `SearchInput` の
//! 差分を安全に判定するには世代整合キャッシュが要る（`sql::sparse_cache::
//! SparseIndexCache`〔Issue #357〕・`sql::arena_cache::SqlArenaCache`〔Issue #363〕と
//! 同型の設計）。差分判定なしに索引だけを探索すると、索引構築後に追加された行・
//! 不可視化された行を検索結果へ混入・欠落させる（テナント境界・RLS 可視性契約を
//! 壊す）ため、[`SearchProvider`] の実装としての本 provider・[`SearchInput`] を
//! 直接受ける経路は安全側に倒して「索引済み集合は常に空」＝全件フォールバックの
//! ままにする。
//!
//! # #408 が接続した索引経路の seam
//!
//! 世代整合キャッシュ・索引探索・Top-k マージは Issue #408 で接続済みだが、
//! **本 provider（`SearchProvider::search`）の外側**として接続する（下記参照）。
//! `SearchProvider` trait 自体・本 provider の `search` 実装は無変更のまま：
//!
//! 1. 世代整合キャッシュは `sql::hnsw_cache::HnswIndexCache`（`(table, ctx)` ×
//!    テーブル単位世代キー）として本 provider の外側（`sql::exec::
//!    execute_statement_with_cache` から `core.rs::EngineCore::hnsw_state` 経由）に
//!    実装した。SQL 表層のフィルタなし `Ranking::Distance` クエリに限る適用条件。
//! 2. 索引側の探索は [`HnswIndex::search`](crate::hnsw::HnswIndex::search) を
//!    `HnswIndex::search(query, k, self.effective_ef(k), scratch)` の形で呼び、
//!    [`HnswSearchScratch`](crate::hnsw::HnswSearchScratch) は呼び出しスレッドごとに
//!    `thread_local!` で所有する（`hnsw.rs` モジュールドキュメント「ベクトルの
//!    所有方針」・`docs/design/hnsw-search.md` の申し送りどおり）。untrusted な
//!    `k` の上限保証は [`HnswSearchProvider::effective_ef`] ではなく
//!    `HnswIndex::search` 自身の `k > MAX_EF` fail-closed 検証が担う
//!    （[`HnswSearchProvider::effective_ef`] のドキュメンテーションコメント参照。
//!    codex-review P2 指摘・Issue #407 追記）。
//! 3. 索引側 hit と brute-force 側（未索引分）hit の Top-k マージは、
//!    `kernel.rs::TopKSelector` と同じ順序規約（スコア `total_cmp` 降順・同点 id
//!    昇順）を保った `sort_by`（安定ソート）で行う（`sort_unstable` 系は
//!    `scripts/check_sort_determinism.sh` が禁止する）。
//!
//! Rust API（`VectorCore::search`）・フィルタ付きクエリ・hybrid クエリは
//! `sql::hnsw_cache` を経由せず、本 provider の全件フォールバックのまま
//! （詳細・段階化の理由は `docs/design/hnsw-generation-cache.md` 参照）。

use crate::hnsw::{HnswParams, ResidentPrecision, ValidatedHnswParams, MAX_EF};
use crate::kernel::{CandidateHit, KernelError, SearchInput, SearchProvider, SubsetSearchInput};
use crate::parallel_search::ParallelSearchProvider;

/// `hnsw.iterative_scan` の値（SQL-73・Issue #2033）。pgvector と同じトークン名
/// （`off`／`relaxed_order`／`strict_order`）で SQL 表層へ公開する。既定は `StrictOrder`
/// （取りこぼしを避ける側。pgvector の既定 `off` とは異なる）。
///
/// `RelaxedOrder` は実装上 `StrictOrder` と同じ動作（出力は常に距離順）で、「順序の小さな
/// 入れ替わりを許す」契約の範囲に収まる。設定は HNSW を使える kNN の探索経路だけに効き、
/// 厳密走査とハイブリッドの結果は変えない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HnswIterativeScan {
    /// 反復しない（k が `ef_search` を超える分は打ち切る）。
    Off,
    /// 反復する（順序の入れ替わりを許す契約。現状は `StrictOrder` と同動作）。
    RelaxedOrder,
    /// 反復する（距離順を保つ）。
    #[default]
    StrictOrder,
}

impl HnswIterativeScan {
    /// SQL 表層（`SHOW`・EXPLAIN）に出すトークン名。
    pub fn as_str(self) -> &'static str {
        match self {
            HnswIterativeScan::Off => "off",
            HnswIterativeScan::RelaxedOrder => "relaxed_order",
            HnswIterativeScan::StrictOrder => "strict_order",
        }
    }
}

/// `hnsw.max_scan_tuples` の既定値（SQL-73・Issue #2033。pgvector と同じ 20,000）。
pub const DEFAULT_MAX_SCAN_TUPLES: usize = 20_000;

/// セッションの `hnsw.*` 検索設定の束（SQL-73・Issue #2030・#2033）。`None` は「上書きなし」
/// （構築時の値、または既定値）。`sql::mode::SessionState` が保持し、`core.rs` が検索実行・
/// EXPLAIN へ渡す。全フィールドが `Copy` のため [`HnswSearchProvider`] に乗せられる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HnswSessionSearch {
    /// `hnsw.ef_search`（1..=1000 を構文段で検証済み）。
    pub ef_search: Option<usize>,
    /// `hnsw.iterative_scan`。
    pub iterative_scan: Option<HnswIterativeScan>,
    /// `hnsw.max_scan_tuples`（1..=2,147,483,647 を構文段で検証済み。`core.rs` が SQL-46 の
    /// 走査行数上限との min を取ってから載せる）。
    pub max_scan_tuples: Option<usize>,
}

/// [`crate::search_engine::SearchEngineKind::Hnsw`] が構築する provider。
///
/// [`Self::new`] は [`ValidatedHnswParams`] のみを受け取る。`ValidatedHnswParams` は
/// private フィールドで [`ValidatedHnswParams::new`]（[`HnswParams::validate`] を必ず
/// 経由する）以外の経路では構築できないため、不正値を保持した provider は型として
/// 存在しえない（codex-review P1 指摘・Issue #407・PR #433 追記。実行時エラー分類の
/// 流用・偽装ではなく型システムで到達不能にする設計）。`crate::hnsw` が公開モジュール
/// （`lib.rs::pub mod hnsw`）で本構造体へ直接到達できても、検証済み値なしには
/// 構築できない点は変わらない。
#[derive(Debug, Clone, Copy)]
pub struct HnswSearchProvider {
    params: ValidatedHnswParams,
    fallback: ParallelSearchProvider,
    /// セッションの `hnsw.ef_search` 上書き値（SQL-73・Issue #2030）。`None` は構築時の
    /// `params.ef_search` を使う。値は SQL 構文段で 1..=1000 を検証済みで、
    /// [`Self::effective_ef`] の `.min(MAX_EF)` が最後の上限になる。
    session_ef_search: Option<usize>,
    /// セッションの `hnsw.iterative_scan` 上書き値（SQL-73・Issue #2033）。`None` は既定
    /// （[`HnswIterativeScan::StrictOrder`]）。
    session_iterative_scan: Option<HnswIterativeScan>,
    /// セッションの `hnsw.max_scan_tuples`（SQL-73・Issue #2033）。`None` は
    /// [`DEFAULT_MAX_SCAN_TUPLES`]。
    session_max_scan_tuples: Option<usize>,
}

impl HnswSearchProvider {
    /// 検証済みパラメータを保持する provider を構築する（infallible。検証は
    /// [`ValidatedHnswParams::new`] の時点で完了済み）。
    pub fn new(params: ValidatedHnswParams) -> Self {
        HnswSearchProvider {
            params,
            fallback: ParallelSearchProvider,
            session_ef_search: None,
            session_iterative_scan: None,
            session_max_scan_tuples: None,
        }
    }

    /// セッションの `hnsw.ef_search` 上書きを載せた複製を返す（`None` は上書きなし）。
    /// `core.rs` の検索実行が、非ハイブリッドの kNN にだけ接続のセッション値を渡すために呼ぶ。
    #[cfg(test)]
    pub(crate) fn with_session_ef_search(mut self, ef: Option<usize>) -> Self {
        self.session_ef_search = ef;
        self
    }

    /// セッションの `hnsw.*` 検索設定（ef_search・iterative_scan・max_scan_tuples）を載せた複製を
    /// 返す。`core.rs` の検索実行が、非ハイブリッドの kNN にだけ接続のセッション値を渡すために呼ぶ。
    pub(crate) fn with_session_search(mut self, s: HnswSessionSearch) -> Self {
        self.session_ef_search = s.ef_search;
        self.session_iterative_scan = s.iterative_scan;
        self.session_max_scan_tuples = s.max_scan_tuples;
        self
    }

    /// 索引ノードの常駐精度だけを差し替えた複製を返す（Issue #2016）。`HALFVEC` 列の kNN は列が f16 で
    /// 保存されているため、エンジン設定にかかわらず `F16` 常駐を要求する（値は f16 で厳密に表せるので
    /// 無損失。`core.rs` の検索実行と `EXPLAIN` が同じ上書きを通す）。
    pub(crate) fn with_resident_precision(mut self, precision: ResidentPrecision) -> Self {
        self.params = self.params.with_resident_precision(precision);
        self
    }

    /// 実効の `hnsw.iterative_scan`（上書きなしは `strict_order`）。`sql::exec` の DISTANCE 段が
    /// 反復走査・`off` の打ち切りを選ぶために読む。
    pub(crate) fn iterative_scan(&self) -> HnswIterativeScan {
        self.session_iterative_scan.unwrap_or_default()
    }

    /// 実効の `hnsw.max_scan_tuples`（上書きなしは [`DEFAULT_MAX_SCAN_TUPLES`]）。
    pub(crate) fn max_scan_tuples(&self) -> usize {
        self.session_max_scan_tuples
            .unwrap_or(DEFAULT_MAX_SCAN_TUPLES)
    }

    /// 実効の `ef_search`（`k` を考慮しない値。`iterative_scan = off` の打ち切り上限）。
    pub(crate) fn effective_ef_search(&self) -> usize {
        self.session_ef_search
            .unwrap_or(self.params.ef_search)
            .min(MAX_EF)
    }

    /// 保持している構築パラメータを返す（テスト・診断用）。
    pub fn params(&self) -> HnswParams {
        self.params.get()
    }

    /// 可視カーディナリティ切替の閾値比を返す（Issue #409。
    /// `sql::hnsw_cache::search_with_overlay` が plain scan／マスク付き ANN 探索の
    /// 切替判定に使う。`full_scan_ratio` は [`HnswParams`] ではなく
    /// [`ValidatedHnswParams`] の private フィールドとして保持する設計
    /// （`hnsw.rs::ValidatedHnswParams` モジュールドキュメント参照）ため、
    /// `HnswParams` を経由せず本 provider 経由でのみ読み取れる）。
    pub fn full_scan_ratio(&self) -> crate::hnsw::Ratio {
        self.params.full_scan_ratio()
    }

    /// ACORN-1 の 2-hop 展開（`hnsw::HopMode::TwoHop`）を有効化する可視比率の
    /// 上限を返す（Issue #501・親 #500。`None`＝既定・無効）。`full_scan_ratio`
    /// と同じ理由（`ValidatedHnswParams` の private フィールド）で本 provider
    /// 経由でのみ読み取れる。`sql::hnsw_cache::traversal_regime_for` が
    /// レジーム判定に使う。
    pub(crate) fn acorn_max_visible_ratio(&self) -> Option<crate::hnsw::Ratio> {
        self.params.acorn_max_visible_ratio()
    }

    /// TwoHop 展開過多ガード（Issue #681・親 #674）の上限比を返す。
    /// `full_scan_ratio` と同じ理由（`ValidatedHnswParams` の private
    /// フィールド）で本 provider 経由でのみ読み取れる。`None`＝既定・無効。
    /// `sql::hnsw_cache::search_with_overlay` が TwoHop 完走直後の事後判定に使う。
    pub(crate) fn acorn_max_expansion_ratio(&self) -> Option<crate::hnsw::Ratio> {
        self.params.acorn_max_expansion_ratio()
    }

    /// visited 集合の切替閾値を返す（Issue #497。`full_scan_ratio` と同じ理由
    /// （`ValidatedHnswParams` の private フィールド）で本 provider 経由でのみ
    /// 読み取れる）。`sql::hnsw_cache::search_with_overlay` が
    /// `HnswIndex::search_masked_with` へそのまま渡す。
    pub fn sparse_visited_max(&self) -> usize {
        self.params.sparse_visited_max()
    }

    /// 構築時に要求した索引ノードの常駐精度（Issue #514）。`full_scan_ratio`
    /// と同じ理由（`ValidatedHnswParams` の private フィールド）で本 provider
    /// 経由でのみ読み取れる。`sql::hnsw_cache::IndexedBase::build` が索引構築
    /// 時にこの値を [`crate::hnsw::HnswIndex::build_parallel_with_precision`]
    /// へ渡す。
    pub fn resident_precision(&self) -> crate::hnsw::ResidentPrecision {
        self.params.resident_precision()
    }

    /// `k` 件の Top-k を得るために `HnswIndex::search` の `ef` 引数へ渡す値を返す
    /// （`self.params.ef_search.max(k).min(MAX_EF)`）。
    ///
    /// **戻り値は `k` に依存する**（例: 構築時 `ef_search=32` で `k=64` を渡すと
    /// `64` を返す。codex-review P2 指摘・Issue #407 追記で「戻り値は `k` に
    /// 一切影響されない」という誤記載を訂正した）。ただし本メソッドが行うのは
    /// 「`k` との `max` を取ったうえで [`MAX_EF`] へクランプする」ことだけで、
    /// **引数 `k` 自体を書き換えて `HnswIndex::search` へ渡すわけではない**
    /// （`k` はそのまま呼び出し元から `HnswIndex::search` の `k` 引数へ渡る）。
    /// `HnswIndex::search` 自身も内部で同じ `ef.max(k)` を計算するため
    /// （`hnsw.rs::HnswIndex::search` の実装）、本メソッドの戻り値をそのまま
    /// `ef` として渡しても渡さなくても実効 `ef` は同じ値に揃う。本メソッドの
    /// 主な役割は `ef_search`（構築時パラメータ、またはセッションの `hnsw.ef_search`
    /// 上書き。後者は利用者が指定するが SQL 構文段で 1..=1000 を検証済み）を
    /// [`MAX_EF`] 内へ収めることにある。`.min(MAX_EF)` が最後の上限。
    ///
    /// untrusted な `k`（wire 経由で到達しうる `SearchInput::k`）に対する実際の
    /// 上限保証は本メソッドではなく、`HnswIndex::search` 自身の検証が担う
    /// （`ef.max(k)` を計算する**前**に `k > MAX_EF` を fail-closed で
    /// `Err(HnswError::InvalidParams)` として拒否する。#408 がこのメソッドを
    /// 呼ぶ契約。モジュールドキュメント「seam」節 2. 参照）。
    pub fn effective_ef(&self, k: usize) -> usize {
        self.session_ef_search
            .unwrap_or(self.params.ef_search)
            .max(k)
            .min(MAX_EF)
    }
}

impl SearchProvider for HnswSearchProvider {
    /// モジュールドキュメント「本タスク時点の契約」節のとおり、常に
    /// [`ParallelSearchProvider`] へ委譲する（全件 brute-force フォールバック）。
    /// エラー型・順序規約（スコア降順・同点 id 昇順）・入力検証（次元不一致・
    /// 非有限クエリ・`k == 0`）は委譲先とビット単位で同一になる。
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        self.fallback.search(input)
    }

    /// Issue #676: `sql::hnsw_cache` の `Subset` 形状（SCALAR 事前フィルタ付き
    /// DISTANCE）が plain scan へ縮退した際、複製済みアリーナではなくキャッシュ
    /// 済みスナップショットの `VectorArena` を借用したまま候補スロットだけを
    /// 探索する「候補 id マスク経路」（Issue #654）へ載せるためのオーバーライド。
    /// `SearchProvider::search_subset` の既定実装（`slots` を一時バッファへ
    /// gather してから `Self::search` を呼ぶ）へ委譲すると gather 自体が複製に
    /// なり Issue #676 の効果（`VectorArena` 複製の回避）が得られないため、
    /// `self.fallback`（[`ParallelSearchProvider`]）の複製なしオーバーライドへ
    /// そのまま委譲する。`fallback` へ委譲する `search` と対称であり、ビット
    /// 一致契約（`hnsw/provider.rs` モジュールドキュメント「本タスク時点の契約」
    /// 節）も同様に保たれる。
    fn search_subset(
        &self,
        input: SubsetSearchInput<'_>,
    ) -> Result<Vec<CandidateHit>, KernelError> {
        self.fallback.search_subset(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xorshift64star(state: &mut u64) -> u64 {
        // hnsw.rs::tests・hnsw_search テストと同型の決定的 xorshift64* 生成器
        // （手法名のみ参照。コード転記元は本モジュールが唯一の実装）。
        *state ^= *state >> 12;
        *state ^= *state << 25;
        *state ^= *state >> 27;
        state.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn deterministic_corpus(n: usize, dim: usize, seed: u64) -> Vec<f32> {
        let mut state = seed.max(1);
        let mut out = Vec::with_capacity(n * dim);
        for _ in 0..n * dim {
            let bits = xorshift64star(&mut state);
            // [-1.0, 1.0) の範囲に収める簡易変換。
            let v = ((bits >> 40) as f32 / (1u32 << 24) as f32) * 2.0 - 1.0;
            out.push(v);
        }
        out
    }

    fn ids_for(n: usize) -> Vec<u64> {
        (0..n as u64).collect()
    }

    #[test]
    fn dim_mismatch_matches_fallback() {
        let provider = HnswSearchProvider::new(ValidatedHnswParams::default());
        let vectors = deterministic_corpus(4, 3, 7);
        let ids = ids_for(4);
        let query = vec![0.0_f32, 1.0]; // dim=3 のはずが 2 要素
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 3,
            query: &query,
            k: 2,
        };
        let err = provider.search(input).expect_err("dim mismatch must error");
        assert_eq!(
            err,
            KernelError::DimMismatch {
                expected: 3,
                found: 2
            }
        );
    }

    #[test]
    fn non_finite_query_matches_fallback() {
        let provider = HnswSearchProvider::new(ValidatedHnswParams::default());
        let vectors = deterministic_corpus(4, 3, 11);
        let ids = ids_for(4);
        let query = vec![f32::NAN, 0.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 3,
            query: &query,
            k: 2,
        };
        let err = provider
            .search(input)
            .expect_err("non-finite query must error");
        assert_eq!(err, KernelError::NonFiniteQuery);
    }

    #[test]
    fn k_zero_and_empty_input_return_empty() {
        let provider = HnswSearchProvider::new(ValidatedHnswParams::default());
        let vectors = deterministic_corpus(4, 3, 13);
        let ids = ids_for(4);
        let query = vec![0.0_f32, 0.0, 0.0];

        let zero_k = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 3,
            query: &query,
            k: 0,
        };
        assert_eq!(provider.search(zero_k).unwrap(), Vec::new());

        let empty_ids: Vec<u64> = Vec::new();
        let empty_vectors: Vec<f32> = Vec::new();
        let empty_input = SearchInput {
            ids: &empty_ids,
            vectors: &empty_vectors,
            dim: 3,
            query: &query,
            k: 2,
        };
        assert_eq!(provider.search(empty_input).unwrap(), Vec::new());
    }

    #[test]
    fn matches_cpu_scalar_reference_bit_for_bit() {
        // フォールバック委譲のため CpuScalarProvider（TopKSelector 経由の参照実装）と
        // 完全一致することを決定的コーパスで確認する（同点タイブレーク含む）。
        use crate::kernel::CpuScalarProvider;

        let dim = 5usize;
        let n = 200usize;
        let vectors = deterministic_corpus(n, dim, 99);
        let ids = ids_for(n);
        let query = deterministic_corpus(1, dim, 4242);

        let hnsw_provider = HnswSearchProvider::new(ValidatedHnswParams::default());
        let reference = CpuScalarProvider;

        for k in [1usize, 5, 20, 200] {
            let input_a = SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: dim as u32,
                query: &query,
                k,
            };
            let input_b = SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: dim as u32,
                query: &query,
                k,
            };
            let got = hnsw_provider.search(input_a).unwrap();
            let want = reference.search(input_b).unwrap();
            assert_eq!(got, want, "k={k}");
        }
    }

    #[test]
    fn search_subset_matches_fallback_and_gathered_reference_bit_for_bit() {
        // Issue #676: `HnswSearchProvider::search_subset` の委譲先
        // （`ParallelSearchProvider::search_subset`）とビット一致すること、
        // かつ「候補行を一時バッファへ複製してから `search`」した参照結果
        // （`SearchProvider::search_subset` の既定実装が行う gather）とも
        // 一致すること（同点誘発を含む重複ヘビーコーパス）を固定する。
        use crate::kernel::SubsetSearchInput;

        let dim = 5usize;
        let n = 64usize;
        // 同点を誘発するため一部の行を意図的に複製する。
        let base = deterministic_corpus(n / 2, dim, 4242);
        let mut vectors = base.clone();
        vectors.extend_from_slice(&base);
        let query = deterministic_corpus(1, dim, 99);
        // 昇順・重複なし・範囲外スロットを含む候補（既定実装の skip 規約検証）。
        let slots: Vec<u32> = (0..n as u32)
            .step_by(3)
            .chain(std::iter::once(n as u32 + 5))
            .collect();

        let provider = HnswSearchProvider::new(ValidatedHnswParams::default());
        let fallback = ParallelSearchProvider;

        for k in [1usize, 5, 20, n] {
            let input = SubsetSearchInput {
                slots: &slots,
                vectors: &vectors,
                dim: dim as u32,
                query: &query,
                k,
            };
            let got = provider.search_subset(input).unwrap();

            let input_fallback = SubsetSearchInput {
                slots: &slots,
                vectors: &vectors,
                dim: dim as u32,
                query: &query,
                k,
            };
            let want_fallback = fallback.search_subset(input_fallback).unwrap();
            assert_eq!(got, want_fallback, "k={k} (vs fallback)");

            // gather 参照: 候補行を一時バッファへ複製してから通常の `search` を呼ぶ。
            let mut gathered: Vec<f32> = Vec::new();
            let mut gathered_ids: Vec<u64> = Vec::new();
            for &slot in &slots {
                let start = slot as usize * dim;
                let end = start + dim;
                if end <= vectors.len() {
                    gathered.extend_from_slice(&vectors[start..end]);
                    gathered_ids.push(slot as u64);
                }
            }
            let input_gathered = SearchInput {
                ids: &gathered_ids,
                vectors: &gathered,
                dim: dim as u32,
                query: &query,
                k,
            };
            let want_gathered = fallback.search(input_gathered).unwrap();
            assert_eq!(got, want_gathered, "k={k} (vs gathered reference)");
        }
    }

    #[test]
    fn effective_ef_clamps_to_max_ef_and_respects_k() {
        let small_ef = HnswSearchProvider::new(
            ValidatedHnswParams::new(HnswParams {
                m: 16,
                ef_construction: 100,
                ef_search: 10,
            })
            .unwrap(),
        );
        // k > ef_search のときは k まで引き上げる。
        assert_eq!(small_ef.effective_ef(50), 50);
        // k <= ef_search のときは ef_search をそのまま使う。
        assert_eq!(small_ef.effective_ef(1), 10);

        let large_ef = HnswSearchProvider::new(
            ValidatedHnswParams::new(HnswParams {
                m: 16,
                ef_construction: 100,
                ef_search: MAX_EF,
            })
            .unwrap(),
        );
        // untrusted な k が MAX_EF を超えても MAX_EF でクランプする。
        assert_eq!(large_ef.effective_ef(MAX_EF + 10_000), MAX_EF);
    }

    #[test]
    fn session_ef_search_overrides_built_value() {
        let base = HnswSearchProvider::new(
            ValidatedHnswParams::new(HnswParams {
                m: 16,
                ef_construction: 100,
                ef_search: 10,
            })
            .unwrap(),
        );
        // 上書きなしは従来値。
        assert_eq!(base.with_session_ef_search(None).effective_ef(1), 10);
        // 上書きありはその値（k との max）。
        let over = base.with_session_ef_search(Some(200));
        assert_eq!(over.effective_ef(1), 200);
        assert_eq!(over.effective_ef(500), 500);
        // MAX_EF での上限。
        assert_eq!(
            base.with_session_ef_search(Some(MAX_EF + 5))
                .effective_ef(1),
            MAX_EF
        );
        // 構築時パラメータ自体は変わらない。
        assert_eq!(over.params().ef_search, 10);
    }

    #[test]
    fn session_iterative_scan_defaults_and_overrides() {
        let base =
            HnswSearchProvider::new(ValidatedHnswParams::new(HnswParams::default()).unwrap());
        assert_eq!(base.iterative_scan(), HnswIterativeScan::StrictOrder);
        assert_eq!(base.max_scan_tuples(), DEFAULT_MAX_SCAN_TUPLES);
        let over = base.with_session_search(HnswSessionSearch {
            ef_search: Some(7),
            iterative_scan: Some(HnswIterativeScan::Off),
            max_scan_tuples: Some(50),
        });
        assert_eq!(over.iterative_scan(), HnswIterativeScan::Off);
        assert_eq!(over.max_scan_tuples(), 50);
        assert_eq!(over.effective_ef_search(), 7);
        assert_eq!(HnswIterativeScan::RelaxedOrder.as_str(), "relaxed_order");
    }

    #[test]
    fn params_accessor_returns_constructed_value() {
        let params = HnswParams {
            m: 8,
            ef_construction: 50,
            ef_search: 32,
        };
        let provider = HnswSearchProvider::new(ValidatedHnswParams::new(params).unwrap());
        assert_eq!(provider.params(), params);
    }

    // 不正な `HnswParams` は `ValidatedHnswParams::new` の時点で拒否され、
    // `HnswSearchProvider::new` へは到達しない（型で保証。codex-review P1 指摘・
    // Issue #407・PR #433 追記）。`HnswSearchProvider::new` 自身は
    // `ValidatedHnswParams` 以外を受け取れないためコンパイルもできない。
    #[test]
    fn validated_hnsw_params_new_rejects_invalid_params() {
        let invalid = HnswParams {
            m: 1, // HnswParams::validate は m < 2 を拒否する
            ..HnswParams::default()
        };
        assert!(ValidatedHnswParams::new(invalid).is_err());
    }
}
