//! `sql::exec::execute_statement_with_cache` の DISTANCE 段（`Ranking::Distance`）が
//! 参照する [`crate::hnsw::HnswIndex`] のテーブル世代整合キャッシュ（Issue #408・
//! 親 Issue #402・前提 #404〜#407）。
//!
//! `sql::arena_cache::SqlArenaCache`（Issue #363）・`sql::sparse_cache::
//! SparseIndexCache`（Issue #357）と同じ「`(table, ctx)` × テーブル単位世代」の
//! キャッシュ設計を踏襲するが、失効時に丸ごと破棄せず**未索引分（新規行・内容が
//! 変わった行・削除/不可視化された行）を brute-force で補う**点が異なる
//! （Lance／qdrant の `indexing_threshold` 方式。手法名のみ参照でコード転記はしない）。
//! 索引済みノードの探索は [`crate::hnsw::HnswIndex::search`] を使い、未索引分は
//! 呼び出し元から渡される `&dyn SearchProvider`（`HnswSearchProvider` は常に
//! [`crate::parallel_search::ParallelSearchProvider`] へ委譲する。`hnsw/provider.rs`
//! モジュールドキュメント参照）で補う。
//!
//! **適用条件**（呼び出し元 `sql::exec::execute_statement_with_cache` が判定する。
//! 本モジュールはここでは判定しない）: `Ranking::Distance` のクエリに限る。
//! `bound.metadata_filters`・`bound.expr_filters` がともに空の場合はアリーナが
//! 「RLS 可視行の全集合」になり、同一 `(table, ctx)` の同一テーブル世代内であれば
//! スロット割当まで含めて再現される（`sql::sparse_cache` の適用条件と同じ不変条件
//! に依拠する）。SCALAR 事前フィルタが付く場合は [`search_subset_or_fallback`] が
//! 担う `Subset` 形状（Issue #409。per-query 写像でキャッシュには登録しない）
//! として扱う。hybrid の密側（`Ranking::Hybrid`）は本モジュールの
//! `search_or_fallback`／`search_subset_or_fallback` からは呼ばれず、`sql::
//! hnsw_hybrid::HnswDenseProvider`（Issue #410）が [`prepare_full_visible`]／
//! [`prepare_subset`]・[`search_prepared`] を直接呼ぶ別経路として結線される
//! （詳細は `sql::hnsw_hybrid` モジュールドキュメント・
//! `docs/design/hnsw-hybrid-iterative-scan.md` 参照）。加えて `bound.mode`
//! （`sql::mode::SearchMode::Precision`。`USING PLAN` 経由の推定を含む）が
//! `precision` のクエリも対象外（TASK-162・SEARCH-9。`precision::apply_gate` の
//! 確信度ゲートは DISTANCE 段の Top-2 マージンを厳密順位に基づいて評価する必要が
//! あり、ANN の近似近傍では真の最近傍の取りこぼしによりマージンを過大評価しうる
//! ため、常に厳密 brute-force 経路を使う）。
//!
//! 上記適用条件（DISTANCE の `FullVisible`／`Subset` 判定、および hybrid 密側の
//! 対称条件）の判定本体は [`classify_ann_plan`]（Issue #411）に集約する。
//! `sql::exec` の 4 boolean はいずれもこの関数の戻り値から導出し、`sql::explain`
//! （`EXPLAIN` の `ann_plan:` 行）も同じ関数を呼ぶ——「並行する推測」ではなく
//! 単一実装の共有により、`EXPLAIN` の回答が executor の実際の判定と乖離しない。
//!
//! **fail-closed 契約**（[`HnswIndexCache::lookup`]／[`HnswIndexCache::record_base`]／
//! `record_overlay_for`／`record_build_failed`（本モジュール内 free function）で
//! 非対称。`sql::arena_cache`・`sql::sparse_cache` と同型。Issue #280 の統一方針を
//! 踏襲する）:
//! - `lookup` は世代不一致・ロック毒化・世代読み取り失敗のいずれも「見つからな
//!   かった」として扱う。
//! - `record_*` はいずれも、常駐反映の直前に `storage` から新規 read トランザクション
//!   で世代を再照合し、対象世代と不一致・ロック毒化時はキャッシュへ**反映しない**
//!   （呼び出し元は自分のクエリの `read_txn` 上で構築した結果をそのまま使ってよい。
//!   `record_build_failed` の場合、記録できなくても呼び出し元は既に brute-force へ
//!   縮退済みのため実害はない）。
//!
//! **索引済みノードの探索結果は必ず `kernel::dot` で再計算**する（`HnswIndex::search`
//! 自身のスコアは索引構築時点のベクトルに対するものであり、`Overlay` が写像した
//! 現在のスロット・ベクトルに対するスコアと数式上は同一だが、写像の正しさ自体を
//! 二重検証する意味も兼ねる。写像先が範囲外・キー不一致の場合は当該クエリのみ
//! 全件 brute-force へ縮退し、エントリは破棄しない——同じ検証がたまたま今回だけ
//! 失敗した可能性を考慮し、次のクエリでの再試行機会を残す）。
//!
//! 対応 spec ビヘイビア（ポインタのみ・本文非転記）: CORE-9, CORE-10, TASK-132。

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use redb::ReadableDatabase;

use crate::arena::VectorArena;
use crate::hnsw::provider::HnswSearchProvider;
use crate::hnsw::{HnswError, HnswIndex, HnswSearchScratch, Ratio};
use crate::kernel::{CandidateHit, KernelError, SearchInput, SearchProvider};
use crate::policy::PolicyContext;
use crate::storage::Storage;

/// 索引を作らず常に brute-force とする行数の下限（本リポの実装既定値。#409 の
/// 可視カーディナリティ推定への置換まで固定値運用とする）。小規模テーブルでは
/// 索引構築のオーバーヘッドが探索削減分を上回るため、`arena.len()` がこれ未満の
/// 間は本キャッシュへ一切触れない。
const MIN_INDEXED_ROWS: usize = 1_024;

/// 再構築が必要と判定する差分比率（分子・分母。Lance 方式の `indexing_threshold`
/// を行数比へ単純化したもの）: `(delta + stale) * REBUILD_DELTA_RATIO.1 >
/// n * REBUILD_DELTA_RATIO.0` で再構築する。
const REBUILD_DELTA_RATIO: (u64, u64) = (1, 10);

/// [`HnswIndexCache`] のエントリ数上限（`sql::arena_cache::SqlArenaCache` と同じ
/// DoS 対策方針）。
const MAX_HNSW_CACHE_ENTRIES: usize = 8;

/// [`HnswIndexCache`] が保持する索引群の概算バイト量の合計上限。
const MAX_HNSW_CACHE_TOTAL_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// `IndexedBase::build` の決定的シード（構築入力のみに依存させ、索引済み集合が
/// クエリ間で無用に変化しないようにする定数）。並列構築（`HnswIndex::build_parallel`）
/// を使うため、`n > SEQUENTIAL_PREFIX_NODES` かつ複数スレッドで構築される場合の
/// グラフ**形状**は run-to-run で変わり得る（`hnsw.rs::build_with_threads` の
/// ドキュメント参照）。探索の決定性契約（同一索引・同一クエリで再現）自体は不変。
const HNSW_BUILD_SEED: u64 = 0x4853_4E57_4341_4348; // "HNSWCACH" の ASCII 値由来の固定定数

/// [`Overlay::slot_of_node`] における「現世代のアリーナに対応するスロットが
/// 存在しない（削除・不可視化・キー変更で失効した）」を表す番兵値。
const STALE_SLOT: u32 = u32::MAX;

/// アリーナのスロットが指す行を、索引ノードとテーブル世代をまたいで同定するための
/// キー（`(tenant_id, id)`。TABLE-12: 行 `id` の一意性スコープはテナント内のため、
/// `id` 単独では別テナントの同名行と衝突しうる）。
type RowKey = (String, u64);

/// [`HnswIndexCache`] の観測用統計（Issue #408）。テナント ID・行 ID 等の機微情報は
/// 一切含まない（`SqlArenaCacheStats`・`SparseIndexCacheStats` と同方針）。
/// `EngineCore::hnsw_index_cache_stats` からのみ公開する（`VectorCore` trait には
/// 載せない固有 API）。
#[derive(Debug, Clone, Copy, Default)]
pub struct HnswIndexCacheStats {
    /// 索引済みノードを実際に探索できた回数（`Ready` 到達かつ縮退なし）。
    pub hits: u64,
    /// 索引が存在しない・世代が離れすぎている等でミスした回数。
    pub misses: u64,
    /// `IndexedBase::build`（新規構築・再構築の両方）を呼んだ回数（成功・失敗を
    /// 問わず、`search_or_fallback` が構築を試みた時点で計上する）。
    pub builds: u64,
    /// 構築失敗（`HnswError`）でこの世代は brute-force へ縮退した回数。
    pub build_failures: u64,
    /// 差分比率が閾値を超えて再構築した回数（`builds` の内数）。
    pub rebuilds: u64,
    /// 未索引分（`Overlay::delta_slots`）を brute-force で補った呼び出し回数。
    pub delta_searches: u64,
    /// 索引を使わず全件 brute-force へ縮退した回数（`MIN_INDEXED_ROWS` 未満・
    /// 写像検証失敗・索引探索エラーを含む。`plain_scans`〔可視カーディナリティ
    /// 比が閾値未満〕・`masked_short`〔マスク付き探索の結果不足〕もこの内数）。
    pub fallbacks: u64,
    /// 可視カーディナリティ比（`visible_in_index / index.len()`）が
    /// `ValidatedHnswParams::full_scan_ratio` 未満で plain scan（アリーナ全体の
    /// brute-force）を選んだ回数（Issue #409。`fallbacks` の内数）。
    pub plain_scans: u64,
    /// マスク付き探索（[`crate::hnsw::HnswIndex::search_masked`]）の結果件数が
    /// `min(k, visible_in_index)` に満たず plain scan へ縮退した回数（Issue #409。
    /// `fallbacks` の内数）。`mask_splits_graph` とは互いに排他（分断検知時は
    /// `search_masked` 自体を呼ばないため、本フィールドには計上されない）。
    pub masked_short: u64,
    /// マスクの受理ノードが複数の連結成分に分かれ、`search_masked` を呼ぶ前に
    /// plain scan へ縮退した回数（`crate::hnsw::HnswIndex::is_mask_fully_reachable`
    /// による世代毎 1 回の分断検査。codex-review P2・Cursor Bugbot High 指摘対応・
    /// PR #435。`fallbacks` の内数。`masked_short` とは互いに排他）。
    pub mask_splits_graph: u64,
    /// `Subset` 形状（SCALAR 事前フィルタ付き DISTANCE。Issue #409）でマスク付き
    /// 探索が縮退なしで完走した回数。`hits` とは独立に数える（`hits` は
    /// `FullVisible` 形状の `Ready` 到達かつ縮退なしの定義を変えない）。
    pub subset_searches: u64,
    /// `search_masked` を呼ばず（`k > MAX_EF`）直ちに plain scan へ縮退した回数
    /// （Issue #410。`fallbacks` の内数。`masked_short`／`mask_splits_graph` とは
    /// 互いに排他。hybrid 密側再取得ループが `fetch_k` を [`crate::hnsw::MAX_EF`]
    /// 超まで倍増した場合に到達しうる。DISTANCE 単発クエリでは `k_eff` が
    /// `core::MAX_SEARCH_K` で有界化されているため通常到達しない）。
    pub ef_cap_fallbacks: u64,
    /// hybrid 密側の再取得ラウンド（`sql::hnsw_hybrid::HnswDenseProvider`）
    /// として本キャッシュへ探索を委ねた回数（Issue #410。1 クエリで複数ラウンド
    /// 呼ばれうるため `hybrid_queries` 以上になる）。
    pub hybrid_dense_searches: u64,
    /// hybrid 密側アダプタが `prepare_*` を実行したクエリ数（Issue #410。
    /// `hybrid_dense_searches / hybrid_queries` が 1 クエリあたりの平均ラウンド数）。
    pub hybrid_queries: u64,
    /// 1 クエリ内で観測された hybrid 密側再取得ラウンド数の最大値（Issue #410。
    /// `hybrid.rs::hybrid_search_boosted` の `dense_cap`／`MAX_FETCH_K` により
    /// 高々 `⌈log2(dense_cap / (2·pool_depth))⌉ + 1` に有界。停止性の観測用）。
    pub hybrid_rounds_max: u64,
    /// `IndexedBase::build` が `ResidentPrecision::F16` を要求されたにもかかわらず
    /// f16 の有限範囲（`|x| <= 65504.0`）を超える成分により `F32` へ自動縮退した
    /// 回数（Issue #514・D6。`builds` の内数）。
    pub f16_residency_fallbacks: u64,
    /// `IndexedBase::build` が `ResidentPrecision::I8` を要求されたにもかかわらず
    /// `sq8::fit_dim_params`／`sq8::encode_rows` の失敗（非有限成分・アロケーション
    /// 失敗）により `F32` へ自動縮退した回数（Issue #521・D6 と同型。`builds`
    /// の内数。`f16_residency_fallbacks` とは互いに排他——`precision` は
    /// 1 クエリあたり単一の値のため両方が同時に加算されることはない）。
    pub i8_residency_fallbacks: u64,
    /// マスク付き探索（`FullVisible`／`Subset` いずれの形状でも。
    /// [`crate::hnsw::HnswIndex::search_masked_with`]）が縮退なしで完走した際、
    /// 可視候補数が `ValidatedHnswParams::sparse_visited_max` 未満で
    /// [`crate::hnsw::VisitedSparse`]（`HashSet<u32>`）を選んだ回数（Issue #497。
    /// `hits`／`subset_searches` とは独立に数える診断用カウンタで、初回構築を
    /// 伴う呼び出し（`OverlaySuccessStat::None`）も計上に含むため両者の合計を
    /// 上回りうる。テナント境界・可視カーディナリティ・索引ノード数等のテナント
    /// 存在情報には繋がらない——採否のみを数える）。
    pub sparse_visited_searches: u64,
    /// `TraversalRegime::TwoHop`（ACORN-1・Issue #501）レジームで縮退なしに
    /// 完走したマスク付き探索回数。`hits`／`subset_searches` とは独立に数える
    /// 診断用カウンタ（`ValidatedHnswParams::acorn_max_visible_ratio` が
    /// `None`（既定）の間は常に `0`）。
    pub acorn_searches: u64,
    /// `bridge_expand`（Issue #501）が受理・候補化した 2-hop ノード数の累計
    /// （診断用。テナント境界・可視カーディナリティ等のテナント存在情報には
    /// 繋がらない——採否のみを数える）。
    pub acorn_expansions: u64,
    /// [`crate::hnsw::HnswIndex::greedy_descend_masked`] の TwoHop 限定
    /// ブリッジ降下（Issue #680）が受理・比較した 2-hop ノード数の累計
    /// （診断用。`acorn_expansions`（層 0 の `bridge_expand` が数える値）とは
    /// 別カウンタ。`acorn_searches` と同じく縮退なしに完走した TwoHop 探索
    /// のみを計上する）。
    pub acorn_descent_bridges: u64,
    /// TwoHop（ACORN-1）探索が完走した直後、橋渡し展開件数
    /// （`bridge_expand` の受理件数）が可視ノード数に対する
    /// `ValidatedHnswParams::acorn_max_expansion_ratio` を超えたため ANN 結果を
    /// 捨てて plain scan へ縮退した回数（Issue #681・親 #674。`fallbacks` の
    /// 内数。`masked_short`／`mask_splits_graph`／`ef_cap_fallbacks` とは互いに
    /// 排他——いずれも本判定より前の分岐で確定するため。TwoHop 完走前に捨てる
    /// ため `acorn_searches`／`acorn_expansions`／`acorn_descent_bridges` とも
    /// 排他〔これらは縮退なしに完走した場合のみ計上する既存契約〕。既定
    /// `acorn_max_expansion_ratio == None` の間は常に `0`。テナント境界・可視
    /// カーディナリティ等のテナント存在情報には繋がらない——採否のみを数える）。
    pub acorn_guard_fallbacks: u64,
    /// hybrid 密側再取得ループが破棄候補ヒープ保持の再開型探索（Issue #505・
    /// `crate::hnsw::ResumableMaskedSearch`）で完走したラウンド数の累計
    /// （`sql::hnsw_hybrid::HnswDenseProvider` が同一クエリ・同一バッファへの
    /// 2 回目以降の呼び出しで [`search_prepared_resumable`] の再開経路へ入り、
    /// 縮退なしで完走した場合のみ計上する。テナント ID・行 ID・スコア等の
    /// テナント存在情報には繋がらない——採否のみを数える診断用カウンタ）。
    pub hybrid_resumed_rounds: u64,
    /// `Subset` 形状（Issue #409）が plain scan（`FullScan`／`PlainScanBelowRatio`／
    /// `mask_splits_graph`／`ef_cap_fallbacks` のいずれか）へ縮退した際、
    /// `VectorArena` を複製せずキャッシュ済みスナップショットを借用したまま
    /// 候補 id マスク経路（`SearchProvider::search_subset`。Issue #654）へ委譲
    /// できた回数（Issue #676）。`fallbacks`／`plain_scans`／`mask_splits_graph`／
    /// `ef_cap_fallbacks` のいずれとも独立に数える（複製有無の観点のみを表す
    /// カウンタで、縮退理由の分類は既存フィールドが担う）。
    pub subset_mask_scans: u64,
    /// `Subset` 形状が ANN 探索へ進んだ結果、`VectorArena::
    /// build_from_cached_rls_rows_subset` による複製が発生した回数（Issue #676。
    /// `subset_mask_scans` と排他——1 クエリで縮退／ANN のどちらか一方のみ
    /// 到達する）。`subset_mask_scans > 0 && subset_arena_copies == 0` は
    /// 「複製なしマスク経路が機能している」ことの非 vacuous な証跡になる。
    pub subset_arena_copies: u64,
    /// 現在キャッシュが保持しているエントリ数。
    pub entries: usize,
}

/// 世代 `built_table_generation` の可視行全体から構築した索引済みベース。
pub(crate) struct IndexedBase {
    index: Arc<HnswIndex>,
    /// `index` のノード番号 → 構築時点のキー（`(tenant_id, id)`）。
    node_keys: Vec<RowKey>,
    /// [`Self::node_keys`] の逆引き（[`Overlay::compute`] が行単位で索引済みか判定する
    /// ために使う）。
    key_to_node: HashMap<RowKey, u32>,
    built_ctx: PolicyContext,
    built_table_generation: u64,
}

impl IndexedBase {
    /// `arena`（RLS 可視行全体。呼び出し元の適用条件によりフィルタなし）から
    /// [`crate::hnsw::HnswIndex::build_parallel`] で構築する。索引ノード番号は
    /// `arena` のスロット番号と一致する（構築直後の世代においては
    /// `slot_of_node[node] == node` が常に成立する）。
    /// `precision` は Issue #514（F16）・Issue #521（I8）で追加した常駐精度
    /// opt-in（既定 `F32`）。戻り値の `bool` 2 つはそれぞれ「要求 `F16`／`I8`
    /// が範囲外成分・非有限成分等により `F32` へ自動縮退したか」（D6。呼び出し元が
    /// `HnswIndexCacheStats::f16_residency_fallbacks`／`i8_residency_fallbacks`
    /// へ計上するために使う。`precision` は 1 クエリあたり単一の値のため両方が
    /// 同時に `true` になることはない）。
    fn build(
        arena: &VectorArena,
        params: crate::hnsw::HnswParams,
        precision: crate::hnsw::ResidentPrecision,
        built_ctx: PolicyContext,
        built_table_generation: u64,
    ) -> Result<(Self, bool, bool), HnswError> {
        let index = HnswIndex::build_parallel_with_precision(
            params,
            precision,
            arena.dim(),
            arena.vectors(),
            HNSW_BUILD_SEED,
        )?;
        let f16_fallback = precision == crate::hnsw::ResidentPrecision::F16
            && index.resident_precision() == crate::hnsw::ResidentPrecision::F32;
        let i8_fallback = precision == crate::hnsw::ResidentPrecision::I8
            && index.resident_precision() == crate::hnsw::ResidentPrecision::F32;
        let mut node_keys: Vec<RowKey> = Vec::with_capacity(arena.len());
        let mut key_to_node: HashMap<RowKey, u32> = HashMap::with_capacity(arena.len());
        for slot in 0..arena.len() {
            // `arena.tenant_id`/`arena.ids` は同じ添字系列を共有する（`arena.rs` の
            // 契約）。範囲外は構築時点では発生しない（`0..arena.len()` の走査のため）
            // が、untrusted 添字アクセス禁止の方針（coding-rust.md）に従い `[]` を
            // 使わず `unwrap_or` で防御的に扱う。
            let tenant = arena.tenant_id(slot).unwrap_or("").to_string();
            let id = arena.ids().get(slot).copied().unwrap_or(0);
            let key = (tenant, id);
            let node = slot as u32; // arena.len() <= MAX_ARENA_ROWS <= u32::MAX 前提
            key_to_node.insert(key.clone(), node);
            node_keys.push(key);
        }
        Ok((
            IndexedBase {
                index: Arc::new(index),
                node_keys,
                key_to_node,
                built_ctx,
                built_table_generation,
            },
            f16_fallback,
            i8_fallback,
        ))
    }

    /// キャッシュ容量判定用の概算バイト量。
    fn approx_heap_bytes(&self) -> usize {
        let node_keys_bytes: usize = self
            .node_keys
            .iter()
            .map(|(t, _)| t.len().saturating_add(std::mem::size_of::<RowKey>()))
            .fold(0usize, |acc, n| acc.saturating_add(n));
        // `HashMap` の実バイト量は正確には測れないため、`rls.rs::
        // hash_set_conservative_bytes` と同じ方針で `node_keys` の複製分を保守的な
        // 上振れ推定として流用する（キー・値のペアがもう 1 組分あると仮定する）。
        let key_to_node_bytes = node_keys_bytes.saturating_add(
            self.key_to_node
                .capacity()
                .saturating_mul(std::mem::size_of::<u32>()),
        );
        self.index
            .approx_heap_bytes()
            .saturating_add(node_keys_bytes)
            .saturating_add(key_to_node_bytes)
    }
}

/// 可視カーディナリティ比（`visible_in_index / index_len`）が
/// `ValidatedHnswParams::full_scan_ratio` 未満かどうかを整数比較で判定する
/// （Issue #409）。`search_with_overlay` の可視カーディナリティ切替・
/// `Overlay::compute` の分断検査（[`Overlay::mask_splits_graph`]）省略判定・
/// `prepare_subset` の早期打ち切り（Issue #488）が同じ判定式を共有する
/// ことで、3 箇所の分岐条件が将来ずれる（挙動が食い違う）ことを構造的に防ぐ。
/// `index_len == 0` は自明に「索引を信用できない」ため常に `true`。丸め誤差を
/// 避けるため `visible_in_index * denominator < index_len * numerator` の整数
/// 比較で行い、`checked_mul` のオーバーフロー時は fail-closed に `true`
/// （plain scan 側）へ倒す。
fn below_full_scan_ratio(visible_in_index: usize, index_len: usize, ratio: Ratio) -> bool {
    if index_len == 0 {
        return true;
    }
    match (
        (visible_in_index as u64).checked_mul(ratio.denominator as u64),
        (index_len as u64).checked_mul(ratio.numerator as u64),
    ) {
        (Some(lhs), Some(rhs)) => lhs < rhs,
        _ => true,
    }
}

/// マスク付き探索が可視カーディナリティ比 `visible_in_index / index_len` に
/// 応じてどう振る舞うかを表す 3 区分（Issue #501・親 #500。ACORN-1 の
/// 2-hop 展開〔`crate::hnsw::HopMode::TwoHop`〕を有効化する条件を、既存の
/// `full_scan_ratio` 切替と単一情報源で判定する）。
///
/// - `PlainScan`: `below_full_scan_ratio` と同じ（アリーナ全体の brute-force）。
/// - `OneHop`: 既存契約のマスク付き ANN 探索（`acorn_max_visible_ratio` が
///   `None`、または比が `acorn_max_visible_ratio` を超える）。
/// - `TwoHop`: `full_scan_ratio <= r <= acorn_max_visible_ratio` の区間
///   （`ValidatedHnswParams::acorn_max_visible_ratio` が `Some` のときのみ
///   到達しうる。既定 `None` の間は本レジームへ到達しない＝既存動作不変）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TraversalRegime {
    PlainScan,
    OneHop,
    TwoHop,
}

impl TraversalRegime {
    /// マスク付き探索へ渡す [`crate::hnsw::HopMode`]（`PlainScan` は探索自体を
    /// 呼ばない呼び出し元の都合上 `OneHop` を返す。呼び出し元は `PlainScan` の
    /// 場合は本値を使わず先に plain scan へ分岐する契約）。
    pub(crate) fn hop(self) -> crate::hnsw::HopMode {
        match self {
            TraversalRegime::TwoHop => crate::hnsw::HopMode::TwoHop,
            TraversalRegime::PlainScan | TraversalRegime::OneHop => crate::hnsw::HopMode::OneHop,
        }
    }
}

/// [`TraversalRegime`] を可視カーディナリティ比・両閾値から判定する（Issue #501）。
/// `full_scan_ratio` 未満は常に `PlainScan`（[`below_full_scan_ratio`] と同じ
/// 判定式・同じ fail-closed 挙動）。`acorn_max_visible_ratio` が `Some(acorn)`
/// のとき、比が `acorn` 以下（`visible_in_index * acorn.denominator <=
/// index_len * acorn.numerator`）なら `TwoHop`、そうでなければ `OneHop`。
/// オーバーフロー時は fail-closed に `OneHop`（既存契約側）へ倒す
/// （`u32 * u32` は `u64` へ必ず収まるため、この分岐は `index_len`／
/// `visible_in_index` が `u32` 範囲外の呼び出し元防御としてのみ働く）。
pub(crate) fn traversal_regime_for(
    visible_in_index: usize,
    index_len: usize,
    full_scan_ratio: Ratio,
    acorn_max_visible_ratio: Option<Ratio>,
) -> TraversalRegime {
    if below_full_scan_ratio(visible_in_index, index_len, full_scan_ratio) {
        return TraversalRegime::PlainScan;
    }
    let Some(acorn) = acorn_max_visible_ratio else {
        return TraversalRegime::OneHop;
    };
    match (
        (visible_in_index as u64).checked_mul(acorn.denominator as u64),
        (index_len as u64).checked_mul(acorn.numerator as u64),
    ) {
        (Some(lhs), Some(rhs)) if lhs <= rhs => TraversalRegime::TwoHop,
        _ => TraversalRegime::OneHop,
    }
}

/// 世代 `generation` の `arena` に対する `base` の差分オーバーレイ。
pub(crate) struct Overlay {
    generation: u64,
    /// 計算時点の `arena.len()`（`search_or_fallback` が使用時に再検証する）。
    arena_len: usize,
    /// `base.index` のノード番号 → 現世代でのスロット番号。存在しない
    /// （削除・不可視化・キー変更）場合は [`STALE_SLOT`]。
    slot_of_node: Vec<u32>,
    /// `slot_of_node` が [`STALE_SLOT`] のノード数。
    stale_nodes: usize,
    /// 未索引の行（新規・内容変更）のスロット番号。
    delta_slots: Vec<u64>,
    /// `delta_slots` と 1 対 1 対応する embedding（row-major・
    /// `delta_vectors.len() == delta_slots.len() * dim`）。
    delta_vectors: Vec<f32>,
    /// `slot_of_node[node] != STALE_SLOT` を表す候補マスク（Issue #409）。
    /// `crate::hnsw::HnswIndex::search_masked` へそのまま渡し、失効ノードが
    /// 探索結果へ混入することを構造的に防ぐ（従来の `k + stale_nodes` オーバー
    /// フェッチ方式は `k_idx > MAX_EF` で全件 brute-force へ縮退していた——
    /// `docs/design/hnsw-generation-cache.md`「既知の限界」節の申し送りを
    /// 本フィールドで解消する）。
    visible_mask: crate::hnsw::NodeMask,
    /// 索引ノードのうち現アリーナのスロットへ写像できたノード数
    /// （`base.index.len() - stale_nodes` と一致）。可視カーディナリティ切替
    /// （`sql::hnsw_cache::search_with_overlay`）の分子に使う。
    visible_in_index: usize,
    /// `visible_mask` の受理ノード全体が、単一 entry point からの誘導部分
    /// グラフ（層 0）から到達可能かどうか（`false` = 到達可能・分断なし。
    /// `crate::hnsw::HnswIndex::is_mask_fully_reachable` を本フィールド計算
    /// 時に 1 回だけ呼ぶ——`Overlay::compute` 自体が既に `visible_mask` の
    /// 構築で O(N) を要するため、追加で 1 回呼んでも漸近コストは増えない。
    /// クエリ毎の全域 BFS を避けるための償却（Cursor Bugbot High 指摘・
    /// codex-review P2 指摘・PR #435。詳細は `docs/design/
    /// hnsw-rls-cardinality-switch.md`「masked_short」節参照）。`true` なら
    /// `search_with_overlay` は `search_masked` を呼ばず直接 plain scan へ
    /// 縮退する。
    mask_splits_graph: bool,
    /// 本世代の可視カーディナリティ比から導出した [`TraversalRegime`]
    /// （Issue #501）。`search_with_overlay` が plain scan／マスク付き ANN 探索
    /// （さらにその hop）を選ぶ単一情報源——`below_full_scan_ratio` の
    /// 再計算をここへ一本化する。
    regime: TraversalRegime,
}

impl Overlay {
    /// `base`（索引済み世代のスナップショット）と `arena`（現世代の RLS 可視集合
    /// 全体）を突き合わせ、索引済み・未索引・失効の 3 分類を確定する。世代あたり
    /// 1 回（`HnswIndexCache::lookup` が `Ready` を返せない間だけ）呼ばれる想定
    /// （`sql::arena_cache` の「キャッシュミス時のみ再構築」と同じ償却）。
    ///
    /// `full_scan_ratio` は呼び出し元（`sql::exec` の DISTANCE 段）が使う
    /// [`ValidatedHnswParams::full_scan_ratio`] をそのまま渡す。可視カーディナリ
    /// ティ比が閾値未満と判明した場合、`search_with_overlay` は
    /// `mask_splits_graph` を参照する前に plain scan を選ぶ契約（本関数呼び出し元
    /// 双方で不変）のため、その場合は分断検査（[`crate::hnsw::HnswIndex::
    /// is_mask_fully_reachable`]。BFS で |受理ノード| × 次数に比例するコスト）を
    /// 省略できる（Issue #488。観測不能な値の計算を省くだけで `search_with_overlay`
    /// の出力は不変）。
    fn compute(
        base: &IndexedBase,
        arena: &VectorArena,
        generation: u64,
        full_scan_ratio: Ratio,
        acorn_max_visible_ratio: Option<Ratio>,
    ) -> Self {
        Self::compute_over_slots(
            base,
            arena,
            0..arena.len(),
            generation,
            full_scan_ratio,
            acorn_max_visible_ratio,
        )
    }

    /// [`Self::compute`] の一般化版（Issue #676）。`slots` は `arena` に対する
    /// スロット番号の順序列（`compute` は `0..arena.len()` を渡すだけのラッパー
    /// で、本関数の出力とビット単位で同一になる）。
    ///
    /// `sql::exec` の候補 id マスク経路（Issue #654）が `VectorArena::
    /// filter_cached_rls_rows_subset` で削減した候補列（`kept: &[u32]`。昇順・
    /// 重複なし）をここへ渡すと、「`kept` が指す行だけを複製した subset アリーナ
    /// に対して `compute` を呼んだ場合」と全フィールド同一の `Overlay` を、
    /// `VectorArena` を複製せずに得られる。ただし出力の座標系は `slots` の
    /// **列挙順（ordinal: `0..slots.len()`）** であり、`slot_of_node`／
    /// `delta_slots` の値は `arena` に対する実スロット番号ではない——`kept` を
    /// 実際に複製した「仮想アリーナ」における番号を表す（呼び出し元は
    /// `kept.get(ordinal)` で元スロット番号へ写像し戻す契約。§モジュール
    /// ドキュメント「Issue #676」節参照）。
    ///
    /// `full_scan_ratio` の意味・分断検査の省略条件は [`Self::compute`] と同じ
    /// （Issue #488 参照）。
    fn compute_over_slots(
        base: &IndexedBase,
        arena: &VectorArena,
        slots: impl ExactSizeIterator<Item = usize>,
        generation: u64,
        full_scan_ratio: Ratio,
        acorn_max_visible_ratio: Option<Ratio>,
    ) -> Self {
        let dim = arena.dim() as usize;
        let mut slot_of_node = vec![STALE_SLOT; base.index.len()];
        let mut delta_slots: Vec<u64> = Vec::new();
        let mut delta_vectors: Vec<f32> = Vec::new();
        let arena_len = slots.len();
        for (ordinal, actual_slot) in slots.enumerate() {
            let tenant = arena.tenant_id(actual_slot).unwrap_or("");
            let Some(&id) = arena.ids().get(actual_slot) else {
                continue;
            };
            let key: RowKey = (tenant.to_string(), id);
            let Some(&node) = base.key_to_node.get(&key) else {
                // 新規行（索引構築時点では存在しなかった）。
                if let Some(v) = arena.vector(actual_slot) {
                    delta_slots.push(ordinal as u64);
                    delta_vectors.extend_from_slice(v);
                }
                continue;
            };
            // 同一キーの行が索引構築時点から存在する: ベクトルが一致するか確認する
            // （`update_row` は同一 `(tenant, id)` のまま embedding を差し替えられる
            // ため、キー一致だけでは未変更の保証にならない）。`HnswIndex::
            // node_matches`（Issue #514・D4）は常駐精度（f32／f16）に依存しない
            // 判定を提供する——`base.index.vector(node)` は F16 常駐時に常に
            // `None` を返すため（D5）、こちらに一本化しないと F16 常駐索引の
            // 差分検出が全行を「未変更」と誤判定し、テーブル更新後もキャッシュが
            // 古いまま応答し続ける silent fallback になる。
            let same_vector = match arena.vector(actual_slot) {
                Some(v) => base.index.node_matches(node, v).unwrap_or(false),
                None => false,
            };
            if same_vector {
                if let Some(entry) = slot_of_node.get_mut(node as usize) {
                    *entry = ordinal as u32;
                }
            } else if let Some(v) = arena.vector(actual_slot) {
                // 内容変更: 索引側ノードは失効させたまま（slot_of_node は STALE_SLOT
                // のまま）、現在の内容を未索引分として扱う。
                delta_slots.push(ordinal as u64);
                delta_vectors.extend_from_slice(v);
            }
        }
        let stale_nodes = slot_of_node.iter().filter(|&&s| s == STALE_SLOT).count();
        let _ = dim; // 将来の整合検証用に保持（現状は arena.vector の長さで既に保証済み）。
        let mut visible_mask = crate::hnsw::NodeMask::new(base.index.len());
        for (node, &slot) in slot_of_node.iter().enumerate() {
            if slot != STALE_SLOT {
                // `base.index.len() <= MAX_HNSW_NODES <= u32::MAX` 前提（`IndexedBase::build`
                // のノード番号割当と同じ不変条件）。
                if let Ok(node_u32) = u32::try_from(node) {
                    visible_mask.set(node_u32);
                }
            }
        }
        let visible_in_index = base.index.len().saturating_sub(stale_nodes);
        // 世代（マスク）が変わるたび高々 1 回だけの分断検査（§`mask_splits_graph`
        // ドキュメンテーションコメント参照）。可視カーディナリティ比が
        // `full_scan_ratio` 未満（[`below_full_scan_ratio`]）なら
        // `search_with_overlay` は `mask_splits_graph` を参照する前に plain scan
        // を選ぶため、その場合は観測されない値として BFS を省略する（Issue #488。
        // `visible_in_index == 0` はこの分岐に必ず含まれる——0 件のマスクで
        // `is_mask_fully_reachable` が自明に `true` を返す旧来の性質は維持される）。
        let regime = traversal_regime_for(
            visible_in_index,
            base.index.len(),
            full_scan_ratio,
            acorn_max_visible_ratio,
        );
        let mask_splits_graph = match regime {
            TraversalRegime::PlainScan => false,
            TraversalRegime::OneHop | TraversalRegime::TwoHop => !base
                .index
                .is_mask_fully_reachable_with(&visible_mask, regime.hop()),
        };
        Overlay {
            generation,
            arena_len,
            slot_of_node,
            stale_nodes,
            delta_slots,
            delta_vectors,
            visible_mask,
            visible_in_index,
            mask_splits_graph,
            regime,
        }
    }

    /// 差分（未索引＋失効）が再構築閾値を超えているか（[`REBUILD_DELTA_RATIO`]）。
    fn needs_rebuild(&self, n: usize) -> bool {
        let churn = (self.delta_slots.len() as u64).saturating_add(self.stale_nodes as u64);
        // `churn * ratio.1 > n * ratio.0` ⟺ `churn / n > ratio.0 / ratio.1`
        // （整数演算で比率比較を行い丸め誤差を避ける）。
        churn.saturating_mul(REBUILD_DELTA_RATIO.1)
            > (n as u64).saturating_mul(REBUILD_DELTA_RATIO.0)
    }

    fn approx_heap_bytes(&self) -> usize {
        let a = self
            .slot_of_node
            .capacity()
            .saturating_mul(std::mem::size_of::<u32>());
        let b = self
            .delta_slots
            .capacity()
            .saturating_mul(std::mem::size_of::<u64>());
        let c = self
            .delta_vectors
            .capacity()
            .saturating_mul(std::mem::size_of::<f32>());
        // `NodeMask` は `u64` の語配列（1 ノード 1 ビット）。
        let d = self
            .visible_mask
            .len()
            .div_ceil(64)
            .saturating_mul(std::mem::size_of::<u64>());
        a.saturating_add(b).saturating_add(c).saturating_add(d)
    }
}

/// [`HnswIndexCache`] の 1 エントリ。
struct HnswCacheEntry {
    table: String,
    built_ctx: PolicyContext,
    /// 索引済みベース。`None` は「このテーブル・ctx では一度も構築に成功して
    /// いない」状態（初回構築が失敗した直後。`build_failed_generation` の負の
    /// キャッシュを保持する場所として本エントリ自体は必要なため、ベースなしでも
    /// エントリを作る。`base` が `Some` の場合の契約は従来どおり: `build`（新規・
    /// 再構築の両方）に成功するたびに置き換わる）。
    base: Option<Arc<IndexedBase>>,
    /// 現在保持している最新のオーバーレイ（`base.built_table_generation` より
    /// 新しい世代のもの。`None` は「`base` の世代からまだ一度もオーバーレイを
    /// 計算していない」状態、または `base` 自体が `None`）。
    overlay: Option<Arc<Overlay>>,
    /// この世代では構築（新規・再構築）に失敗し、以後の探索は brute-force へ
    /// 縮退中であることを示す負のキャッシュ。`base`・`overlay` はこの失敗前の
    /// 値のまま温存し（`base` が元々 `None` なら `None` のまま）、世代が進めば
    /// 再挑戦できるようにする（`docs/design/hnsw-generation-cache.md`「構築失敗時の
    /// 負のキャッシュ」節参照）。
    build_failed_generation: Option<u64>,
    last_used: u64,
}

impl HnswCacheEntry {
    /// 容量判定用の概算バイト量（`base`・`overlay` それぞれが `Some` の分のみ）。
    fn approx_heap_bytes(&self) -> usize {
        self.base
            .as_ref()
            .map(|b| b.approx_heap_bytes())
            .unwrap_or(0)
            .saturating_add(
                self.overlay
                    .as_ref()
                    .map(|o| o.approx_heap_bytes())
                    .unwrap_or(0),
            )
    }
}

/// ロックが保護する可変状態。
#[derive(Default)]
struct HnswCacheState {
    entries: Vec<HnswCacheEntry>,
}

/// `sql::exec::execute_statement_with_cache` の DISTANCE 段専用 [`HnswIndex`] 世代
/// 整合キャッシュ本体（モジュールドキュメント参照）。
pub(crate) struct HnswIndexCache {
    state: RwLock<HnswCacheState>,
    seq: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    builds: AtomicU64,
    build_failures: AtomicU64,
    rebuilds: AtomicU64,
    delta_searches: AtomicU64,
    fallbacks: AtomicU64,
    plain_scans: AtomicU64,
    masked_short: AtomicU64,
    mask_splits_graph: AtomicU64,
    subset_searches: AtomicU64,
    ef_cap_fallbacks: AtomicU64,
    hybrid_dense_searches: AtomicU64,
    hybrid_queries: AtomicU64,
    hybrid_rounds_max: AtomicU64,
    f16_residency_fallbacks: AtomicU64,
    i8_residency_fallbacks: AtomicU64,
    sparse_visited_searches: AtomicU64,
    acorn_searches: AtomicU64,
    acorn_expansions: AtomicU64,
    acorn_descent_bridges: AtomicU64,
    acorn_guard_fallbacks: AtomicU64,
    hybrid_resumed_rounds: AtomicU64,
    subset_mask_scans: AtomicU64,
    subset_arena_copies: AtomicU64,
}

/// [`HnswIndexCache::lookup`] の結果。
enum Lookup {
    /// 索引済みベース＋現世代のオーバーレイがそろっている（索引探索可能）。
    Ready(Arc<IndexedBase>, Arc<Overlay>),
    /// 索引済みベースはあるが、現世代のオーバーレイが未計算（`Overlay::compute` を
    /// 1 回行えば `Ready` になる）。
    NeedOverlay(Arc<IndexedBase>),
    /// 現世代では構築（新規・再構築）に失敗済み。この世代は brute-force のまま。
    BuildFailedThisGeneration,
    /// 該当エントリなし（初回、またはロック毒化等で判定不能）。
    Miss,
}

impl HnswIndexCache {
    pub(crate) fn new() -> Self {
        HnswIndexCache {
            state: RwLock::new(HnswCacheState::default()),
            seq: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            builds: AtomicU64::new(0),
            build_failures: AtomicU64::new(0),
            rebuilds: AtomicU64::new(0),
            delta_searches: AtomicU64::new(0),
            fallbacks: AtomicU64::new(0),
            plain_scans: AtomicU64::new(0),
            masked_short: AtomicU64::new(0),
            mask_splits_graph: AtomicU64::new(0),
            subset_searches: AtomicU64::new(0),
            ef_cap_fallbacks: AtomicU64::new(0),
            hybrid_dense_searches: AtomicU64::new(0),
            hybrid_queries: AtomicU64::new(0),
            hybrid_rounds_max: AtomicU64::new(0),
            f16_residency_fallbacks: AtomicU64::new(0),
            i8_residency_fallbacks: AtomicU64::new(0),
            sparse_visited_searches: AtomicU64::new(0),
            acorn_searches: AtomicU64::new(0),
            acorn_expansions: AtomicU64::new(0),
            acorn_descent_bridges: AtomicU64::new(0),
            acorn_guard_fallbacks: AtomicU64::new(0),
            hybrid_resumed_rounds: AtomicU64::new(0),
            subset_mask_scans: AtomicU64::new(0),
            subset_arena_copies: AtomicU64::new(0),
        }
    }

    /// `Subset` 形状の plain scan 縮退が候補 id マスク経路（複製なし）で完了した
    /// ことを記録する（Issue #676。`sql::exec` の DISTANCE 段が `SubsetSlotPlan::
    /// MaskScan` を選んだ全分岐で呼ぶ）。
    pub(crate) fn record_subset_mask_scan(&self) {
        self.subset_mask_scans.fetch_add(1, Ordering::Relaxed);
    }

    /// `Subset` 形状が ANN 探索へ進み `VectorArena` の複製が発生したことを記録
    /// する（Issue #676。`sql::exec` の DISTANCE 段が `SubsetSlotPlan::Ann` を
    /// 選び `build_from_cached_rls_rows_subset` を呼んだ直後に呼ぶ）。
    pub(crate) fn record_subset_arena_copy(&self) {
        self.subset_arena_copies.fetch_add(1, Ordering::Relaxed);
    }

    /// hybrid 密側再取得ループの 1 ラウンドとして本キャッシュへ探索を委ねた
    /// ことを記録する（Issue #410。`sql::hnsw_hybrid::HnswDenseProvider::search`
    /// から呼ばれる。統計はテナント ID・行 ID を含まない best-effort カウンタ）。
    pub(crate) fn record_hybrid_dense_search(&self) {
        self.hybrid_dense_searches.fetch_add(1, Ordering::Relaxed);
    }

    /// hybrid 密側アダプタが 1 クエリの寿命を終えた時点でラウンド数を記録する
    /// （`hybrid_queries` を 1 加算し、`hybrid_rounds_max` をこのクエリの
    /// ラウンド数で更新する。CAS ループは `Relaxed` の best-effort 統計のため
    /// 競合時は緩く失敗を許容する）。
    pub(crate) fn record_hybrid_query_rounds(&self, rounds: u64) {
        self.hybrid_queries.fetch_add(1, Ordering::Relaxed);
        let mut current = self.hybrid_rounds_max.load(Ordering::Relaxed);
        while rounds > current {
            match self.hybrid_rounds_max.compare_exchange_weak(
                current,
                rounds,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
    }

    /// `storage` は現状の実装では未参照（`sql::arena_cache::SqlArenaCache::lookup`・
    /// `sql::sparse_cache::SparseIndexCache::lookup` と異なり、世代不一致時の
    /// エントリ即時破棄をここでは行わない。破棄は `record_base`（新規ベース登録時の
    /// テーブル限定置換）・`evict_for_capacity`（LRU）・`evict_entry`（規模縮小時）に
    /// 集約している）。呼び出し規約を他 2 キャッシュと揃えるため引数として残す。
    fn lookup(
        &self,
        _storage: &Storage,
        read_txn: &redb::ReadTransaction,
        table: &str,
        ctx: &PolicyContext,
    ) -> Lookup {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let Ok(mut guard) = self.state.write() else {
            // ロック毒化: エントリの有無を判定できないため fail-closed に Miss
            // 扱いとする（codex-review P2 指摘対応: 以前はこの経路が misses に
            // 計上されず、通常の未登録エントリの初回ミスと合わせて実際のミス
            // 発生回数を過小報告していた）。
            self.misses.fetch_add(1, Ordering::Relaxed);
            return Lookup::Miss;
        };
        let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table)
        else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            return Lookup::Miss;
        };
        let Some(position) = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.built_ctx == *ctx)
        else {
            // 対象 `(table, ctx)` のエントリが一度も登録されていない、最も一般的な
            // 初回ミス（codex-review P2 指摘対応）。
            drop(guard);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return Lookup::Miss;
        };
        if let Some(entry) = guard.entries.get_mut(position) {
            entry.last_used = seq;
        }
        // このクロージャ以降は `position` を経由してのみエントリを読む
        // （`guard` を可変で持ち続けるため 2 回目の `get` は不要）。
        let entry = match guard.entries.get(position) {
            Some(e) => e,
            None => return Lookup::Miss,
        };
        if entry.build_failed_generation == Some(current_generation) {
            return Lookup::BuildFailedThisGeneration;
        }
        let Some(base) = entry.base.as_ref() else {
            // 過去に構築成功したことがない（初回失敗のみを負のキャッシュとして
            // 保持しているエントリ）。現世代では失敗していない（上のチェックを
            // 通過済み）ため、新規構築を試みさせる。
            drop(guard);
            self.misses.fetch_add(1, Ordering::Relaxed);
            return Lookup::Miss;
        };
        if let Some(overlay) = &entry.overlay {
            if overlay.generation == current_generation {
                let base = Arc::clone(base);
                let overlay = Arc::clone(overlay);
                drop(guard);
                // `hits` はここでは加算しない（codex-review P2 指摘対応）。
                // `Ready` はあくまで索引済みベース＋オーバーレイが揃っている
                // ことの通知であり、その後 `search_with_overlay` が
                // `k + stale_nodes > MAX_EF`・写像検証失敗・索引探索エラー等で
                // brute-force に縮退した場合、`hits`（実際に探索できた回数）と
                // `fallbacks`（縮退した回数）の両方に同一呼び出しが二重計上され
                // てしまう。`hits` は `search_with_overlay` が縮退なしで完了した
                // 時点でのみ加算する（呼び出し元 `search_or_fallback` 参照）。
                return Lookup::Ready(base, overlay);
            }
        }
        // ベースの世代自体が現世代と一致し、かつオーバーレイ未計算の場合も
        // （恒等オーバーレイを計算すれば `Ready` になる）、世代がさらに進んで
        // オーバーレイが古い場合も、いずれも `Overlay::compute` を 1 回行う必要が
        // ある点は同じため `NeedOverlay` で統一する。
        let base = Arc::clone(base);
        drop(guard);
        self.misses.fetch_add(1, Ordering::Relaxed);
        Lookup::NeedOverlay(base)
    }

    /// 新規構築（またはミス後の再構築）したベースを登録する。`storage` から新規に
    /// 読んだテーブル世代と `base.built_table_generation` が一致しない場合・
    /// ロック毒化時はキャッシュへの反映のみを諦める（`sql::arena_cache::
    /// SqlArenaCache::insert` と同じ fail-closed 契約）。
    fn record_base(&self, storage: &Storage, table: &str, base: IndexedBase) -> Arc<IndexedBase> {
        let base = Arc::new(base);
        let Ok(mut guard) = self.state.write() else {
            return base;
        };
        let Ok(read_txn) = storage.db().begin_read() else {
            return base;
        };
        let Ok(current_generation) = crate::catalog::table_generation_in_txn(&read_txn, table)
        else {
            return base;
        };
        if base.built_table_generation != current_generation {
            return base;
        }
        let own_bytes = base.approx_heap_bytes();
        if own_bytes > MAX_HNSW_CACHE_TOTAL_BYTES {
            // 単体で総量上限を超えるベースは常駐させない（`sql::sparse_cache::
            // SparseIndexCache::insert`／`sql::arena_cache::SqlArenaCache::insert`
            // と同じ DoS 対策契約。codex-review P1 指摘対応: これを省略すると
            // `evict_for_capacity` が全エントリを追い出しても空きが作れないまま
            // 呼び出し元が無条件に push しており、単一テナントの過大な索引が
            // 容量上限を超えて常駐し得た）。呼び出し元はこのクエリのスナップショット
            // から構築した `base` をそのまま使ってよい。
            //
            // 常駐させないだけでは終わらせず、`build_failed_generation` と同じ
            // 負のキャッシュへ「この世代はキャッシュ不可」と記録する（Cursor
            // Bugbot 指摘対応: 記録を省略すると次回参照が再び `Miss` となり、
            // `search_or_fallback` が同一世代内の毎クエリで `IndexedBase::build`
            // 〔HNSW グラフ全体の構築。brute-force より遥かに高価〕をやり直して
            // しまう。世代が進めばテーブル規模の変化次第で再度キャッシュ可能に
            // なりうるため、失敗記録と同じく世代限定の一時的な縮退として扱う）。
            mark_uncacheable_in_guard(self, &mut guard, table, &base.built_ctx, current_generation);
            return base;
        }
        if let Some(pos) = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.built_ctx == base.built_ctx)
        {
            guard.entries.remove(pos);
        }
        self.evict_for_capacity(&mut guard, own_bytes);
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        guard.entries.push(HnswCacheEntry {
            table: table.to_string(),
            built_ctx: base.built_ctx.clone(),
            base: Some(Arc::clone(&base)),
            overlay: None,
            build_failed_generation: None,
            last_used: seq,
        });
        base
    }

    /// 新規エントリ追加前に呼ぶ（`record_base`／`record_build_failed` からのみ）。
    /// エントリ数上限・総バイト量上限の両方で LRU eviction する。
    fn evict_for_capacity(
        &self,
        guard: &mut std::sync::RwLockWriteGuard<'_, HnswCacheState>,
        incoming_bytes: usize,
    ) {
        self.evict_while(guard, incoming_bytes, true);
    }

    /// 既存エントリの overlay 更新前に呼ぶ（`record_overlay_for` からのみ）。
    /// エントリ数はこの呼び出しでは増えないため、総バイト量上限のみで
    /// eviction する（Cursor Bugbot 指摘対応: `evict_for_capacity` をそのまま
    /// 使うと、overlay 更新はエントリを追加しないにもかかわらず
    /// `entries.len() >= MAX_HNSW_CACHE_ENTRIES`（キャッシュが既に満杯）の
    /// 条件だけでエビクトが発火し、build・rebuild・世代 overlay のたびに
    /// 無関係な別 `(table, ctx)` の索引エントリが巻き添えで追い出され、次回
    /// ミスして再構築を強制されていた）。
    fn evict_for_extra_bytes(
        &self,
        guard: &mut std::sync::RwLockWriteGuard<'_, HnswCacheState>,
        incoming_bytes: usize,
    ) {
        self.evict_while(guard, incoming_bytes, false);
    }

    fn evict_while(
        &self,
        guard: &mut std::sync::RwLockWriteGuard<'_, HnswCacheState>,
        incoming_bytes: usize,
        check_entry_count: bool,
    ) {
        let mut total_bytes: usize = guard
            .entries
            .iter()
            .map(HnswCacheEntry::approx_heap_bytes)
            .fold(0usize, |acc, n| acc.saturating_add(n));
        while (check_entry_count && guard.entries.len() >= MAX_HNSW_CACHE_ENTRIES)
            || total_bytes.saturating_add(incoming_bytes) > MAX_HNSW_CACHE_TOTAL_BYTES
        {
            let victim = guard
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(idx, _)| idx);
            let Some(idx) = victim else {
                return;
            };
            let removed = guard.entries.remove(idx);
            total_bytes = total_bytes.saturating_sub(removed.approx_heap_bytes());
        }
    }

    /// 指定 `(table, ctx)` のエントリのみを破棄する（当該テナントから見た
    /// `arena.len()`〔RLS 可視行数〕が `MIN_INDEXED_ROWS` 未満へ縮小した場合の
    /// 後始末）。Cursor Bugbot 指摘対応: 以前は `table` 一致のみで全 `ctx` 分を
    /// まとめて破棄しており、可視行数が少ない/0 件のテナントのクエリが同一
    /// テーブルの他テナントの構築済み索引まで巻き添えで破棄し、繰り返しフル
    /// リビルドを強制していた。`arena.len()` はテナントごとの RLS フィルタ後の
    /// 値であり、他テナントの可視行数とは独立なため `ctx` も一致条件に含める。
    ///
    /// `current_generation`（呼び出し元のクエリが見ているテーブル世代）より
    /// **新しい**世代のエントリは破棄しない（Cursor Bugbot 指摘対応: 世代チェック
    /// なしに呼んでいた旧実装では、可視行数が少ない/0 件のクエリが、並行して
    /// 別クエリが構築した「より大きい世代」の索引済みエントリまで巻き添えで
    /// 追い出し、次回の別クエリで高価な HNSW 再構築を強制し得た）。エントリの
    /// 世代は overlay があればその世代、無ければベースの構築世代、ベースも無い
    /// 〔負のキャッシュのみ〕場合は `build_failed_generation` の順に判定する。
    fn evict_entry(&self, table: &str, ctx: &PolicyContext, current_generation: u64) {
        if let Ok(mut guard) = self.state.write() {
            guard.entries.retain(|e| {
                if e.table != table || e.built_ctx != *ctx {
                    return true;
                }
                let entry_generation = e
                    .overlay
                    .as_ref()
                    .map(|o| o.generation)
                    .or_else(|| e.base.as_ref().map(|b| b.built_table_generation))
                    .or(e.build_failed_generation);
                // 世代が判定できない（全フィールド None。通常発生しない）場合は
                // 保護対象が無いとみなし破棄する（fail-closed に「破棄する」側へ
                // 倒す。破棄しても次回参照時に再構築されるだけで安全側）。
                entry_generation.is_some_and(|g| g > current_generation)
            });
        }
    }

    /// [`IndexedBase::arena_identity_mismatch_guard`] が不整合を検出した特定の
    /// `(table, ctx)` エントリのみを破棄する（Cursor Bugbot 指摘対応: 以前は
    /// ガード検出時にこのクエリを brute-force へ縮退させるだけで不整合な
    /// `IndexedBase` をキャッシュへ温存していたため、`lookup` が同一エントリへ
    /// `NeedOverlay` を返し続け、同じ `(table, ctx)` の以後の全クエリが同じ
    /// ガードへ当たり続け ANN 経路が実質的に無効化されたままになっていた。
    /// 破棄した次回の `lookup` は `Miss` となり、`search_or_fallback` が新規
    /// 構築を試みて ANN 経路へ復帰できる）。
    ///
    /// `base`（呼び出し元がガード検出直前に手にしていた不整合な `IndexedBase`）
    /// の `Arc` ポインタと一致するエントリのみを対象にする。世代チェックを
    /// 経由せず（この `base` はそもそも現在のアリーナと同定できない壊れた
    /// 状態であり保護すべき正当な理由がない）、かつ `Arc::ptr_eq` で対象を
    /// 厳密に絞ることで、並行して別スレッドが同じ `(table, ctx)` を正しい
    /// `base` へ差し替え済みだった場合にそれを巻き添えで破棄しない
    /// （テナント境界・fail-closed 契約は維持: 破棄対象は呼び出し元が既に
    /// 特定した自テーブル・自テナントのエントリ 1 件に限る）。
    fn evict_entry_if_mismatched(&self, table: &str, ctx: &PolicyContext, base: &Arc<IndexedBase>) {
        if let Ok(mut guard) = self.state.write() {
            guard.entries.retain(|e| {
                !(e.table == table
                    && e.built_ctx == *ctx
                    && e.base.as_ref().is_some_and(|b| Arc::ptr_eq(b, base)))
            });
        }
    }

    pub(crate) fn stats(&self) -> HnswIndexCacheStats {
        let entries = self.state.read().map(|g| g.entries.len()).unwrap_or(0);
        HnswIndexCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            builds: self.builds.load(Ordering::Relaxed),
            build_failures: self.build_failures.load(Ordering::Relaxed),
            rebuilds: self.rebuilds.load(Ordering::Relaxed),
            delta_searches: self.delta_searches.load(Ordering::Relaxed),
            fallbacks: self.fallbacks.load(Ordering::Relaxed),
            plain_scans: self.plain_scans.load(Ordering::Relaxed),
            masked_short: self.masked_short.load(Ordering::Relaxed),
            mask_splits_graph: self.mask_splits_graph.load(Ordering::Relaxed),
            subset_searches: self.subset_searches.load(Ordering::Relaxed),
            ef_cap_fallbacks: self.ef_cap_fallbacks.load(Ordering::Relaxed),
            hybrid_dense_searches: self.hybrid_dense_searches.load(Ordering::Relaxed),
            hybrid_queries: self.hybrid_queries.load(Ordering::Relaxed),
            hybrid_rounds_max: self.hybrid_rounds_max.load(Ordering::Relaxed),
            f16_residency_fallbacks: self.f16_residency_fallbacks.load(Ordering::Relaxed),
            i8_residency_fallbacks: self.i8_residency_fallbacks.load(Ordering::Relaxed),
            sparse_visited_searches: self.sparse_visited_searches.load(Ordering::Relaxed),
            acorn_searches: self.acorn_searches.load(Ordering::Relaxed),
            acorn_expansions: self.acorn_expansions.load(Ordering::Relaxed),
            acorn_descent_bridges: self.acorn_descent_bridges.load(Ordering::Relaxed),
            acorn_guard_fallbacks: self.acorn_guard_fallbacks.load(Ordering::Relaxed),
            hybrid_resumed_rounds: self.hybrid_resumed_rounds.load(Ordering::Relaxed),
            subset_mask_scans: self.subset_mask_scans.load(Ordering::Relaxed),
            subset_arena_copies: self.subset_arena_copies.load(Ordering::Relaxed),
            entries,
        }
    }
}

thread_local! {
    /// [`HnswIndex::search`] の呼び出しをまたいで再利用するスクラッチ（`hnsw/
    /// provider.rs` モジュールドキュメントの seam 2. が定める「呼び出しスレッドごと
    /// に呼び出し元が所有する」契約をスレッドローカルで満たす）。
    static SEARCH_SCRATCH: RefCell<HnswSearchScratch> = RefCell::new(HnswSearchScratch::default());
}

/// `sql::exec::execute_statement_with_cache` へ渡すキャッシュアクセス束
/// （Issue #408）。`storage`・`cache` に加え、`effective_ef`／構築パラメータへ
/// アクセスするための `provider`（`Copy`）を束ねる。`index_gate_cache` は
/// `catalog::hnsw_targeted_in_txn`（Issue #1065 テーブル単位の適格性ゲート）の
/// 索引カタログ全件検証の結果を索引カタログ専用世代単位で共有するキャッシュ
/// （`EngineCore::index_catalog_gate_cache` をそのまま貸し出す。`sql::exec` は
/// `EngineCore` を持たないため、本構造体経由で受け取る。codex-review P2 対応・
/// PR #1124。Issue #1154 でキーをストレージ全体世代から専用世代へ切り替え、
/// 索引宣言と無関係な行 DML では無効化されないようにした）。
pub(crate) struct HnswCacheAccess<'a> {
    pub(crate) storage: &'a Storage,
    pub(crate) cache: &'a HnswIndexCache,
    pub(crate) provider: HnswSearchProvider,
    pub(crate) index_gate_cache: &'a crate::catalog::IndexCatalogGateCache,
    /// HNSW を使うテーブルの範囲（`EngineCore::with_hnsw_scope` の設定値。
    /// `catalog::hnsw_targeted_in_txn` へそのまま渡す。Issue #1065）。
    pub(crate) hnsw_scope: crate::search_engine::HnswScope,
}

/// [`search_or_fallback`]／[`search_subset_or_fallback`] の「索引・オーバーレイの
/// 解決」段（世代整合キャッシュの lookup／構築／再構築を伴う O(N) 相当の重い処理）
/// と「Top-k 探索」段（`k` に依存し軽い）を分離するための中間表現（Issue #410）。
///
/// hybrid 密側再取得ループ（`sql::hnsw_hybrid::HnswDenseProvider`）は同一クエリで
/// `k`（＝ `fetch_k`）だけを変えて複数ラウンド探索するため、ラウンドごとに
/// `Overlay::compute`／`IndexedBase::build` をやり直すと brute-force より重くなる。
/// `prepare_full_visible`／`prepare_subset` で 1 回だけこの解決を行い、
/// [`search_prepared`] をラウンド数分呼ぶことで解決結果を再利用する。
pub(crate) enum PreparedHnswSearch {
    /// 索引済みベース＋現世代のオーバーレイが揃っている（[`search_with_overlay`]
    /// をそのまま呼べる）。
    Indexed {
        base: Arc<IndexedBase>,
        overlay: Arc<Overlay>,
        success_stat: OverlaySuccessStat,
    },
    /// 索引を使わず全件 brute-force とする（`MIN_INDEXED_ROWS` 未満・構築/再構築
    /// 失敗・写像不整合のいずれか。[`search_prepared`] が呼ばれるたびに `fallbacks`
    /// を 1 加算する——各呼び出しは実際に一度ずつ brute-force 探索を行うラウンドの
    /// ため、解決時ではなく探索時に計上する）。
    FullScan,
    /// `Subset` 形状（Issue #409）専用の早期打ち切り（Issue #488）。`arena.len()`
    /// （可視カーディナリティ `visible_in_index` の上界）の時点で既に
    /// `full_scan_ratio` 未満と判明しているため、`Overlay::compute`（行ごとの
    /// `RowKey` 生成・`HashMap` 引き・dot 比較・分断検査）を丸ごと省略して
    /// [`PreparedHnswSearch::FullScan`] と同じ全件 brute-force を選ぶ。`FullScan`
    /// と分けて持つのは、`fallbacks` に加え `plain_scans`（可視カーディナリティ比
    /// 未満での plain scan）も観測できるようにするため（[`search_with_overlay`]
    /// の下限判定ヒット時と同じ統計意味）。`FullScan` と同様、統計は解決時では
    /// なく [`search_prepared`] の探索時（hybrid 密側の複数ラウンドではラウンド
    /// ごと）に加算する。
    PlainScanBelowRatio,
}

/// `FullVisible` 形状（Issue #408。フィルタなし DISTANCE・hybrid 密側）の
/// 索引・オーバーレイ解決段。[`search_or_fallback`] のラッパーと hybrid 密側
/// アダプタ（`sql::hnsw_hybrid::HnswDenseProvider`）の双方から呼ばれる。
///
/// 引数・戻り値の意味は元の `search_or_fallback`（Issue #408 実装）から解決部分
/// のみを切り出したもの。統計計上（`builds`／`rebuilds`／`build_failures`／
/// `evict_entry`／`record_overlay_for`）は解決時点で確定する内容のみ行い、
/// `fallbacks`／`hits`／`subset_searches` 等の探索結果依存の統計は
/// [`search_with_overlay`]／[`search_prepared`] 側に残す（元実装と計上箇所を
/// 変えない）。
pub(crate) fn prepare_full_visible(
    access: &HnswCacheAccess<'_>,
    read_txn: &redb::ReadTransaction,
    table: &str,
    ctx: &PolicyContext,
    arena: &VectorArena,
) -> PreparedHnswSearch {
    let n = arena.len();
    if n < MIN_INDEXED_ROWS {
        // 現在のクエリが見ている世代より新しいエントリは巻き添えにしない
        // （`evict_entry` ドキュメント参照）。世代が判定できない場合は
        // eviction 自体をスキップする（fail-closed。誤って破棄する側へは
        // 倒さない——判定不能を「保護すべき新しい世代ではない」とみなして
        // 破棄すると、並行クエリが構築した索引を無条件で失わせてしまう）。
        if let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table) {
            access.cache.evict_entry(table, ctx, current_generation);
        }
        return PreparedHnswSearch::FullScan;
    }

    let base = match access.cache.lookup(access.storage, read_txn, table, ctx) {
        Lookup::BuildFailedThisGeneration => return PreparedHnswSearch::FullScan,
        Lookup::Ready(base, overlay) => {
            return PreparedHnswSearch::Indexed {
                base,
                overlay,
                success_stat: OverlaySuccessStat::Hits,
            };
        }
        Lookup::NeedOverlay(base) => base,
        Lookup::Miss => {
            let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table)
            else {
                return PreparedHnswSearch::FullScan;
            };
            access.cache.builds.fetch_add(1, Ordering::Relaxed);
            match IndexedBase::build(
                arena,
                access.provider.params(),
                access.provider.resident_precision(),
                ctx.clone(),
                current_generation,
            ) {
                Ok((built, f16_fallback, i8_fallback)) => {
                    if f16_fallback {
                        access
                            .cache
                            .f16_residency_fallbacks
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    if i8_fallback {
                        access
                            .cache
                            .i8_residency_fallbacks
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    access.cache.record_base(access.storage, table, built)
                }
                Err(_) => {
                    access.cache.build_failures.fetch_add(1, Ordering::Relaxed);
                    record_build_failed(access, table, ctx, current_generation);
                    return PreparedHnswSearch::FullScan;
                }
            }
        }
    };

    // ここへ到達するのは `NeedOverlay`（既存ベースに現世代のオーバーレイがまだ
    // ない）経路のみ。`Overlay::compute` を 1 回行い、差分比率次第で再構築する。
    let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table) else {
        return PreparedHnswSearch::FullScan;
    };
    if base.arena_identity_mismatch_guard(arena, ctx) {
        // 呼び出し元の適用条件（フィルタなし DISTANCE）が正しく守られていれば
        // 起こらないはずの不整合。fail-closed に全件へ縮退するだけでなく、
        // 不整合な `base` を保持するエントリ自体をキャッシュから退避する
        // （Cursor Bugbot 指摘対応「Guard disables ANN without recovery」:
        // 退避しないと `lookup` が同じ `(table, ctx)` に対し `NeedOverlay` を
        // 返し続け、以後の全クエリが同じガードに当たり続けて ANN 経路が
        // 再構築されないまま無効化されてしまう）。退避対象はこの `(table,
        // ctx)` の当該エントリのみで、他テナント・他テーブルへは波及しない。
        access.cache.evict_entry_if_mismatched(table, ctx, &base);
        return PreparedHnswSearch::FullScan;
    }
    let overlay = Overlay::compute(
        &base,
        arena,
        current_generation,
        access.provider.full_scan_ratio(),
        access.provider.acorn_max_visible_ratio(),
    );
    if overlay.needs_rebuild(n) {
        access.cache.builds.fetch_add(1, Ordering::Relaxed);
        match IndexedBase::build(
            arena,
            access.provider.params(),
            access.provider.resident_precision(),
            ctx.clone(),
            current_generation,
        ) {
            Ok((built, f16_fallback, i8_fallback)) => {
                if f16_fallback {
                    access
                        .cache
                        .f16_residency_fallbacks
                        .fetch_add(1, Ordering::Relaxed);
                }
                if i8_fallback {
                    access
                        .cache
                        .i8_residency_fallbacks
                        .fetch_add(1, Ordering::Relaxed);
                }
                access.cache.rebuilds.fetch_add(1, Ordering::Relaxed);
                let new_base = access.cache.record_base(access.storage, table, built);
                let mut identity_mask = crate::hnsw::NodeMask::new(new_base.index.len());
                for node in 0..new_base.index.len() {
                    if let Ok(node_u32) = u32::try_from(node) {
                        identity_mask.set(node_u32);
                    }
                }
                // 恒等マスク（全ノード受理）でも分断検査は省略しない
                // （§`Overlay::mask_splits_graph` ドキュメンテーションコメント
                // 参照。`repair_reachability` が層 0 の entry point 起点到達性を
                // 保証する契約に依拠しすぎず、構築直後の索引でも実際に検証する）。
                // regime（Issue #501）は「恒等マスク」＝可視全ノードのため
                // `visible_in_index == index_len` で `traversal_regime_for` から
                // 導出し、分断検査は regime が要求する hop で行う（`Overlay::
                // compute` と同じ単一情報源）。
                let identity_regime = traversal_regime_for(
                    new_base.index.len(),
                    new_base.index.len(),
                    access.provider.full_scan_ratio(),
                    access.provider.acorn_max_visible_ratio(),
                );
                let identity_splits_graph = match identity_regime {
                    TraversalRegime::PlainScan => false,
                    TraversalRegime::OneHop | TraversalRegime::TwoHop => !new_base
                        .index
                        .is_mask_fully_reachable_with(&identity_mask, identity_regime.hop()),
                };
                let identity_overlay = Arc::new(Overlay {
                    generation: current_generation,
                    arena_len: arena.len(),
                    slot_of_node: (0..new_base.index.len() as u32).collect(),
                    stale_nodes: 0,
                    delta_slots: Vec::new(),
                    delta_vectors: Vec::new(),
                    visible_mask: identity_mask,
                    visible_in_index: new_base.index.len(),
                    mask_splits_graph: identity_splits_graph,
                    regime: identity_regime,
                });
                record_overlay_for(access, table, &new_base, Arc::clone(&identity_overlay));
                PreparedHnswSearch::Indexed {
                    base: new_base,
                    overlay: identity_overlay,
                    success_stat: OverlaySuccessStat::None,
                }
            }
            Err(_) => {
                access.cache.build_failures.fetch_add(1, Ordering::Relaxed);
                record_build_failed(access, table, ctx, current_generation);
                PreparedHnswSearch::FullScan
            }
        }
    } else {
        let overlay = Arc::new(overlay);
        record_overlay_for(access, table, &base, Arc::clone(&overlay));
        PreparedHnswSearch::Indexed {
            base,
            overlay,
            success_stat: OverlaySuccessStat::None,
        }
    }
}

/// [`PreparedHnswSearch`] を使って 1 ラウンド分の Top-k 探索を行う（Issue #410）。
/// `Indexed` なら [`search_with_overlay`] へ、`FullScan` なら `slot_ids` を使う
/// 全件 brute-force へディスパッチする（`fallbacks` はここで加算する。§
/// [`PreparedHnswSearch::FullScan`] ドキュメンテーションコメント参照）。
pub(crate) fn search_prepared(
    access: &HnswCacheAccess<'_>,
    prepared: &PreparedHnswSearch,
    provider: &dyn SearchProvider,
    arena: &VectorArena,
    slot_ids: &[u64],
    query: &[f32],
    k: usize,
) -> Result<Vec<CandidateHit>, KernelError> {
    match prepared {
        PreparedHnswSearch::Indexed {
            base,
            overlay,
            success_stat,
        } => search_with_overlay(
            access,
            Arc::clone(base),
            Arc::clone(overlay),
            provider,
            arena,
            query,
            k,
            *success_stat,
        ),
        PreparedHnswSearch::FullScan => {
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            let input = SearchInput {
                ids: slot_ids,
                vectors: arena.vectors(),
                dim: arena.dim(),
                query,
                k,
            };
            provider.search(input)
        }
        PreparedHnswSearch::PlainScanBelowRatio => {
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            access.cache.plain_scans.fetch_add(1, Ordering::Relaxed);
            let input = SearchInput {
                ids: slot_ids,
                vectors: arena.vectors(),
                dim: arena.dim(),
                query,
                k,
            };
            provider.search(input)
        }
    }
}

/// クエリ 1 本の寿命だけ [`sql::hnsw_hybrid::HnswDenseProvider`] が保持する、
/// hybrid 密側再取得ループの破棄候補ヒープ保持型再開状態（Issue #505）。
/// `HnswDenseProvider` は同一 `PreparedHnswSearch`（`base`／`overlay`）を
/// クエリ全体で固定して保持するため、本状態を構築した索引・オーバーレイと
/// 次ラウンドで参照するそれらが食い違う心配はない（`sql::hnsw_hybrid` モジュール
/// ドキュメンテーションコメント「fail-closed な受理条件」参照）——クエリ・
/// `ef` 単調性の整合検査は [`crate::hnsw::HnswIndex::search_masked_resume`]
/// 自身が担う。
pub(crate) struct HnswResumeState {
    search: crate::hnsw::ResumableMaskedSearch,
}

/// [`search_prepared`] の再開型版（Issue #505・親 #504。`sql::hnsw_hybrid::
/// HnswDenseProvider` の 2 ラウンド目以降が呼ぶ）。`PreparedHnswSearch::Indexed`
/// （`TraversalRegime::OneHop` に限る。`TwoHop`・前段ガード〔アリーナ不一致・
/// `PlainScan`・`mask_splits_graph`・`k > MAX_EF`〕はいずれも状態化せず既存の
/// 単発経路〔[`search_with_overlay`]／brute-force〕へ縮退し `*resume` を破棄する）
/// に限り、`resume` が `Some` ならグラフ探索を再開し、`None`（初回、または
/// 直前ラウンドが前段ガードで縮退した）ならラウンド 1 として開始する。
/// 索引の生の探索結果を受け取ってからの後段（結果件数充足検査・スロット写像・
/// delta マージ・ソート・成功統計）は [`finish_indexed_search`] を単発経路と
/// 共有する。
///
/// 縮退なしで再開型探索（`resume` が `Some` だった側）が完走した場合のみ
/// `hybrid_resumed_rounds` を加算する（診断用。テナント境界・存在情報には
/// 繋がらない）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn search_prepared_resumable(
    access: &HnswCacheAccess<'_>,
    prepared: &PreparedHnswSearch,
    provider: &dyn SearchProvider,
    arena: &VectorArena,
    slot_ids: &[u64],
    query: &[f32],
    k: usize,
    resume: &mut Option<HnswResumeState>,
) -> Result<Vec<CandidateHit>, KernelError> {
    let (base, overlay, success_stat) = match prepared {
        PreparedHnswSearch::Indexed {
            base,
            overlay,
            success_stat,
        } => (base, overlay, *success_stat),
        PreparedHnswSearch::FullScan => {
            *resume = None;
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            let input = SearchInput {
                ids: slot_ids,
                vectors: arena.vectors(),
                dim: arena.dim(),
                query,
                k,
            };
            return provider.search(input);
        }
        PreparedHnswSearch::PlainScanBelowRatio => {
            *resume = None;
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            access.cache.plain_scans.fetch_add(1, Ordering::Relaxed);
            let input = SearchInput {
                ids: slot_ids,
                vectors: arena.vectors(),
                dim: arena.dim(),
                query,
                k,
            };
            return provider.search(input);
        }
    };

    if overlay.arena_len != arena.len() {
        *resume = None;
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        return full_scan_with_arena(provider, arena, query, k);
    }
    if overlay.regime == TraversalRegime::PlainScan {
        *resume = None;
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        access.cache.plain_scans.fetch_add(1, Ordering::Relaxed);
        return full_scan_with_arena(provider, arena, query, k);
    }
    if overlay.mask_splits_graph {
        *resume = None;
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        access
            .cache
            .mask_splits_graph
            .fetch_add(1, Ordering::Relaxed);
        return full_scan_with_arena(provider, arena, query, k);
    }
    if k > crate::hnsw::MAX_EF {
        *resume = None;
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        access
            .cache
            .ef_cap_fallbacks
            .fetch_add(1, Ordering::Relaxed);
        return full_scan_with_arena(provider, arena, query, k);
    }

    let hop = overlay.regime.hop();
    if hop == crate::hnsw::HopMode::TwoHop {
        // 再開型は HopMode::OneHop 限定（`crate::hnsw::ResumableMaskedSearch`
        // ドキュメンテーションコメント参照）。TwoHop レジームのラウンドは
        // 状態化せず既存の単発経路（`bridge_expand` を含む完全な探索）へ倒す。
        // 展開過多ガード（Issue #681）も `search_with_overlay` 側でそのまま
        // 適用される。
        *resume = None;
        return search_with_overlay(
            access,
            Arc::clone(base),
            Arc::clone(overlay),
            provider,
            arena,
            query,
            k,
            success_stat,
        );
    }

    let ef = access.provider.effective_ef(k);
    let mask = Some(&overlay.visible_mask);

    let (index_hits, used_resume) = if let Some(mut state) = resume.take() {
        match base
            .index
            .search_masked_resume(&mut state.search, query, k, ef, mask)
        {
            Ok(hits) => {
                *resume = Some(state);
                (hits, true)
            }
            Err(_) => {
                // 整合検査に失敗（別クエリ・ef 減少等。呼び出し元
                // `HnswDenseProvider` は同一クエリを固定して渡す契約のため
                // 通常到達しないが、fail-closed に状態を捨ててラウンド 1 から
                // やり直す）。
                match base
                    .index
                    .search_masked_resumable_start(query, k, ef, mask, hop)
                {
                    Ok((hits, new_state)) => {
                        *resume = Some(HnswResumeState { search: new_state });
                        (hits, false)
                    }
                    Err(_) => {
                        *resume = None;
                        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
                        return full_scan_with_arena(provider, arena, query, k);
                    }
                }
            }
        }
    } else {
        match base
            .index
            .search_masked_resumable_start(query, k, ef, mask, hop)
        {
            Ok((hits, new_state)) => {
                *resume = Some(HnswResumeState { search: new_state });
                (hits, false)
            }
            Err(_) => {
                *resume = None;
                access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
                return full_scan_with_arena(provider, arena, query, k);
            }
        }
    };

    // `fallbacks`（キャッシュ全体で共有される `AtomicU64`）の呼び出し前後
    // 差分では、並行クエリが同時に別ラウンドで縮退した場合に本呼び出し自身が
    // 縮退していないのに差分が非 0 になり得る（codex-review P2 指摘対応・
    // PR #619）。`finish_indexed_search` に呼び出しローカルな `Cell<bool>` を
    // 渡し、当該呼び出しが実際にどこかの分岐で縮退したかを直接判定する。
    let fell_back = std::cell::Cell::new(false);
    let result = finish_indexed_search(
        access,
        base,
        overlay,
        provider,
        arena,
        query,
        k,
        success_stat,
        index_hits,
        None,
        hop,
        0,
        0,
        &fell_back,
    );
    // 縮退なし（`finish_indexed_search` が `fell_back` を立てずに完走した）で
    // 再開経路（`resume` が `Some` だった側）を通ったラウンドのみ計上する
    // （診断用カウンタ。§`HnswIndexCacheStats::hybrid_resumed_rounds`）。
    // `result` がエラーの場合（codex-review P2 指摘）は「縮退なしで完走」の
    // 契約を満たさないため、`fell_back` が立っていなくても計上しない。
    if used_resume && !fell_back.get() && result.is_ok() {
        access
            .cache
            .hybrid_resumed_rounds
            .fetch_add(1, Ordering::Relaxed);
    }
    result
}

/// DISTANCE 段の索引済み探索＋未索引分 brute-force 併用の本体（Issue #408。
/// Issue #410 で「解決」（[`prepare_full_visible`]）と「探索」（[`search_prepared`]）
/// を分離した薄いラッパーへ書き換えた。単発の DISTANCE クエリでは解決・探索を
/// それぞれ 1 回ずつ呼ぶだけであり、統計・戻り値・エラー契約は分離前と同一）。
///
/// 戻り値は `KernelError` のみ（HNSW 索引固有のエラー・写像検証失敗はすべて当該
/// クエリの brute-force 縮退として吸収し、呼び出し元へは伝播させない。呼び出し元
/// `sql::exec::execute_statement_with_cache` は既存の `Ranking::Distance` 分岐と
/// 同じ `map_kernel_error` でエラー処理できる）。
///
/// 引数 9 個は `clippy::too_many_arguments`（閾値 7）を超えるが、`access`
/// （キャッシュ・provider 束）と個々のクエリ実行時値（`read_txn`／`table`／
/// `ctx`／`arena`／`slot_ids`／`provider`／`query`／`k`）はいずれも意味の異なる
/// 独立した入力であり、`sql::exec::execute_statement_with_cache` の内部専用
/// （`pub(crate)`）関数としてここで許容する（`sql::arena_cache`／`sql::sparse_cache`
/// が `execute_statement_with_cache` 自体で同じ許容をしているのと同じ理由）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn search_or_fallback(
    access: &HnswCacheAccess<'_>,
    read_txn: &redb::ReadTransaction,
    table: &str,
    ctx: &PolicyContext,
    arena: &VectorArena,
    slot_ids: &[u64],
    provider: &dyn SearchProvider,
    query: &[f32],
    k: usize,
) -> Result<Vec<CandidateHit>, KernelError> {
    let prepared = prepare_full_visible(access, read_txn, table, ctx, arena);
    search_prepared(access, &prepared, provider, arena, slot_ids, query, k)
}

/// `Subset` 形状（Issue #409。SCALAR 事前フィルタ付き DISTANCE。`sql::exec::
/// execute_statement_with_cache` の適用条件は §モジュールドキュメント
/// 「適用条件」節・`docs/design/hnsw-rls-cardinality-switch.md` 参照）の索引・
/// オーバーレイ解決段（Issue #410 で [`prepare_full_visible`] と対称の形へ分離）。
/// `FullVisible`（[`prepare_full_visible`]）との違いは 3 点（いずれも「アリーナが
/// 可視全集合ではなく WHERE 適用後の部分集合」という前提の違いから来る）:
///
/// 1. `arena.len() < MIN_INDEXED_ROWS` でもエントリを evict しない（フィルタで
///    絞られた行数が少ないことは「テーブル自体が小規模」を意味しない）。
/// 2. `Lookup::Miss`（索引未構築）では新規構築を試みず、常に `FullScan` へ縮退
///    する（`base` はフィルタなしクエリが可視全集合から構築する契約——部分集合
///    アリーナから `IndexedBase::build` を呼ぶと以後の `FullVisible` 経路が
///    誤った base を再利用してしまう）。
/// 3. per-query の `Overlay::compute` 結果を [`record_overlay_for`] へ登録しない
///    （`needs_rebuild` も評価しない）——WHERE で除外された行が churn に見えて
///    無用な再構築を誘発したり、`FullVisible` の次クエリが参照するはずの
///    キャッシュ済みオーバーレイを汚したりしないようにする。`Overlay::compute`
///    自体は `k` に依存しないため、hybrid 密側再取得ループの複数ラウンドでも
///    1 回の呼び出し結果をそのまま使い回せる。
pub(crate) fn prepare_subset(
    access: &HnswCacheAccess<'_>,
    read_txn: &redb::ReadTransaction,
    table: &str,
    ctx: &PolicyContext,
    arena: &VectorArena,
) -> PreparedHnswSearch {
    let base = match access.cache.lookup(access.storage, read_txn, table, ctx) {
        Lookup::BuildFailedThisGeneration | Lookup::Miss => return PreparedHnswSearch::FullScan,
        Lookup::Ready(base, _full_arena_overlay) => base,
        Lookup::NeedOverlay(base) => base,
    };

    if base.arena_identity_mismatch_guard(arena, ctx) {
        // `FullVisible` 経路（`prepare_full_visible`）と異なり、ここでは巻き添えを
        // 避けるためエントリを退避しない（この不整合は通常発生しない防御的
        // 経路であり、`FullVisible` クエリが正しく参照し続けられる余地を
        // 残す。§本関数ドキュメンテーションコメント参照）。
        return PreparedHnswSearch::FullScan;
    }

    // 早期打ち切り（Issue #488）: `overlay.visible_in_index <= arena.len()`
    // （Subset アリーナに含まれない索引済みノードは `Overlay::compute` が
    // 構造的に `STALE_SLOT` として除外するため、写像を計算する前でも
    // 上界として使える）。この上界がすでに `full_scan_ratio` 未満なら、
    // 実際の重なりに関わらず `search_with_overlay` は必ず plain scan を選ぶ
    // （[`below_full_scan_ratio`] は `visible_in_index` に対して単調。
    // 上界が閾値未満なら実値もその関数で必ず閾値未満と判定される）ため、
    // `Overlay::compute`（行ごとの `RowKey` 生成・`HashMap` 引き・dot 比較・
    // 分断検査）を丸ごと省略できる。出力は `Overlay::compute` を実行した
    // 場合と完全に同一（`search_with_overlay` が選ぶ分岐が同じ）。
    if below_full_scan_ratio(
        arena.len(),
        base.index.len(),
        access.provider.full_scan_ratio(),
    ) {
        // 統計（`fallbacks`／`plain_scans`）は解決時ではなく `search_prepared`
        // の探索時に加算する（`FullScan` と同じ方針。hybrid 密側の複数ラウンドで
        // 本関数が 1 回しか呼ばれない場合でも、ラウンドごとに正しく計上される）。
        return PreparedHnswSearch::PlainScanBelowRatio;
    }

    let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table) else {
        return PreparedHnswSearch::FullScan;
    };

    // per-query の写像を計算する（キャッシュへは登録しない。§本関数
    // ドキュメンテーションコメント参照）。`Overlay::compute` は「渡された
    // arena のスロット番号系列に対する索引済みノードの写像」を汎用的に計算
    // するため、可視全集合ではなく WHERE 適用後の部分集合アリーナを渡しても
    // そのまま正しく動く（部分集合に含まれない索引済みノードは自然に
    // `STALE_SLOT` として除外される）。
    let overlay = Arc::new(Overlay::compute(
        &base,
        arena,
        current_generation,
        access.provider.full_scan_ratio(),
        access.provider.acorn_max_visible_ratio(),
    ));
    PreparedHnswSearch::Indexed {
        base,
        overlay,
        success_stat: OverlaySuccessStat::SubsetSearches,
    }
}

/// `Subset` 形状（Issue #409）の索引済み探索＋未索引分 brute-force 併用の本体
/// （Issue #410 で [`prepare_subset`]／[`search_prepared`] への薄いラッパーへ
/// 書き換えた。単発の DISTANCE クエリでは解決・探索をそれぞれ 1 回ずつ呼ぶだけ
/// であり、統計・戻り値・エラー契約は分離前と同一）。
#[allow(clippy::too_many_arguments)]
pub(crate) fn search_subset_or_fallback(
    access: &HnswCacheAccess<'_>,
    read_txn: &redb::ReadTransaction,
    table: &str,
    ctx: &PolicyContext,
    arena: &VectorArena,
    slot_ids: &[u64],
    provider: &dyn SearchProvider,
    query: &[f32],
    k: usize,
) -> Result<Vec<CandidateHit>, KernelError> {
    let prepared = prepare_subset(access, read_txn, table, ctx, arena);
    search_prepared(access, &prepared, provider, arena, slot_ids, query, k)
}

/// [`prepare_subset_from_slots`] の結果（Issue #676）。`Subset` 形状の候補が
/// スカラー列二次索引の候補削減（Issue #654・`sql::exec` の SCALAR 段。
/// `filter_cached_rls_rows_subset` が返した候補スロット列 `kept: &[u32]`）
/// から plain scan へ縮退する場合、`VectorArena` の複製（`build_from_cached_rls_rows_subset`）
/// を経ずキャッシュ済みスナップショットを借用したまま `SearchProvider::
/// search_subset` へ委譲できることを表す。
pub(crate) enum SubsetSlotPlan {
    /// plain scan 縮退（統計は本関数内で計上済み）。呼び出し元は
    /// `provider.search_subset(kept, snapshot_arena.vectors(), ..)` を使う
    /// （`VectorArena` を複製しない）。
    MaskScan,
    /// ANN 探索へ進む。呼び出し元が `kept` から subset アリーナを複製し
    /// `search_prepared` を呼ぶ（`success_stat` は常に `SubsetSearches`）。
    Ann(PreparedHnswSearch),
}

/// `Subset` 形状（Issue #409）の候補削減後スロット列（`kept: &[u32]`。Issue #654・
/// `sql::exec` の SCALAR 段が `VectorArena::filter_cached_rls_rows_subset` で
/// 削減した候補。狭義昇順・重複なし）に対し、`VectorArena` を複製する**前**に
/// plain scan／ANN の判定を確定する（Issue #676）。
///
/// [`prepare_subset`] との違いは、可視カーディナリティ判定の入力が「複製済み
/// subset アリーナの `arena.len()`」ではなく「`kept.len()`（複製前に判明する
/// 正確な候補件数）」である点と、`Overlay::compute` の代わりに
/// [`Overlay::compute_over_slots`] で `snapshot_arena` を複製せず走査する点のみ。
/// 判定順・統計計上は [`prepare_subset`]／[`search_prepared`]／
/// [`search_with_overlay`] の前段ガードと同じ順・同じ意味を保つ（単発 DISTANCE
/// クエリのため解決時点で計上する。呼び出し元は本関数の呼び出しを 1 クエリ
/// あたり 1 回に限る契約——`search_prepared` のように複数ラウンド呼ばれる
/// hybrid 密側では使わない。hybrid `Subset` 形状は対象外のまま）:
///
/// 1. `lookup` が `Miss`／`BuildFailedThisGeneration` → `fallbacks` → `MaskScan`
/// 2. `arena_identity_mismatch_guard` 不一致 → `fallbacks` → `MaskScan`
/// 3. `below_full_scan_ratio(kept.len(), base.index.len(), ..)` →
///    `fallbacks`＋`plain_scans` → `MaskScan`（`Overlay` 未計算。[`prepare_subset`]
///    の早期打ち切り〔Issue #488〕と同じ節約）
/// 4. `table_generation_in_txn` 失敗 → `fallbacks` → `MaskScan`
/// 5. `overlay.regime == PlainScan` → `fallbacks`＋`plain_scans` → `MaskScan`
/// 6. `overlay.mask_splits_graph` → `fallbacks`＋`mask_splits_graph` → `MaskScan`
/// 7. `k > MAX_EF` → `fallbacks`＋`ef_cap_fallbacks` → `MaskScan`
/// 8. それ以外 → `Ann(Indexed)`（`subset_searches` は呼び出し元が探索完走後に
///    加算する。既存 `OverlaySuccessStat::SubsetSearches` の契約のまま）
///
/// `MaskScan` を返す全分岐で [`HnswIndexCache::record_subset_mask_scan`] を、
/// `Ann` を返す場合は呼び出し元が複製直後に
/// [`HnswIndexCache::record_subset_arena_copy`] を呼ぶ契約（本関数内では
/// 複製を行わないため呼ばない）。
pub(crate) fn prepare_subset_from_slots(
    access: &HnswCacheAccess<'_>,
    read_txn: &redb::ReadTransaction,
    table: &str,
    ctx: &PolicyContext,
    snapshot_arena: &VectorArena,
    kept: &[u32],
    k: usize,
) -> SubsetSlotPlan {
    let mask_scan = |access: &HnswCacheAccess<'_>| {
        access.cache.record_subset_mask_scan();
        SubsetSlotPlan::MaskScan
    };

    let base = match access.cache.lookup(access.storage, read_txn, table, ctx) {
        Lookup::BuildFailedThisGeneration | Lookup::Miss => {
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            return mask_scan(access);
        }
        Lookup::Ready(base, _full_arena_overlay) => base,
        Lookup::NeedOverlay(base) => base,
    };

    if base.arena_identity_mismatch_guard(snapshot_arena, ctx) {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        return mask_scan(access);
    }

    // 早期打ち切り（Issue #488 と同じ節約。§本関数ドキュメンテーションコメント
    // 3.）: 複製前に判明する `kept.len()` を可視カーディナリティの正確な値
    // として使う（`filter_cached_rls_rows_subset` が返す `kept` は WHERE 通過後
    // の候補そのものであり、`prepare_subset` の「複製済み subset アリーナの
    // `arena.len()`」と異なり上界ではなく正確な値）。
    if below_full_scan_ratio(
        kept.len(),
        base.index.len(),
        access.provider.full_scan_ratio(),
    ) {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        access.cache.plain_scans.fetch_add(1, Ordering::Relaxed);
        return mask_scan(access);
    }

    let Ok(current_generation) = crate::catalog::table_generation_in_txn(read_txn, table) else {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        return mask_scan(access);
    };

    let overlay = Overlay::compute_over_slots(
        &base,
        snapshot_arena,
        kept.iter().map(|&s| s as usize),
        current_generation,
        access.provider.full_scan_ratio(),
        access.provider.acorn_max_visible_ratio(),
    );

    if overlay.regime == TraversalRegime::PlainScan {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        access.cache.plain_scans.fetch_add(1, Ordering::Relaxed);
        return mask_scan(access);
    }
    if overlay.mask_splits_graph {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        access
            .cache
            .mask_splits_graph
            .fetch_add(1, Ordering::Relaxed);
        return mask_scan(access);
    }
    if k > crate::hnsw::MAX_EF {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        access
            .cache
            .ef_cap_fallbacks
            .fetch_add(1, Ordering::Relaxed);
        return mask_scan(access);
    }

    SubsetSlotPlan::Ann(PreparedHnswSearch::Indexed {
        base,
        overlay: Arc::new(overlay),
        success_stat: OverlaySuccessStat::SubsetSearches,
    })
}

impl IndexedBase {
    /// `arena`・`ctx` が本ベースの構築時と取り違えられていないことの検証
    /// （`Overlay::compute` は `arena` の全スロットを本ベースのノードキー・
    /// 次元と突き合わせて走査するため、`base` 自体が別テーブル・別テナント
    /// 由来のまま渡された場合に不整合な写像を作ってしまう）。
    ///
    /// 実際に検証可能な 2 つの不変条件で判定する（`Ready`/`NeedOverlay` に
    /// 到達する経路では `lookup` が `(table, ctx)` の完全一致でエントリを
    /// 選んでいるため通常は発生しないが、fail-closed の最終防御として
    /// 冗長に確認する。security.md P0「テナント分離の検査を外す/緩める/
    /// バイパス経路を作らない」）:
    /// - `self.index.dim()` と `arena.dim()` が一致すること（別テーブル・
    ///   別次元のアリーナ取り違えを検出する）
    /// - `self.built_ctx` と `ctx` が一致すること（別テナント・別可視性の
    ///   `PolicyContext` 取り違えを検出する。`PolicyContext` の `PartialEq`
    ///   はテナント ID・許可可視性集合の完全一致を要求する）
    ///
    /// 一致していれば `false`（検証通過）、いずれか不一致なら `true`
    /// （呼び出し元は全件 brute-force へ縮退する）を返す。
    fn arena_identity_mismatch_guard(&self, arena: &VectorArena, ctx: &PolicyContext) -> bool {
        self.index.dim() != arena.dim() || self.built_ctx != *ctx
    }
}

fn record_build_failed(
    access: &HnswCacheAccess<'_>,
    table: &str,
    ctx: &PolicyContext,
    generation: u64,
) {
    let Ok(mut guard) = access.cache.state.write() else {
        return;
    };
    let Ok(read_txn) = access.storage.db().begin_read() else {
        return;
    };
    let Ok(current_generation) = crate::catalog::table_generation_in_txn(&read_txn, table) else {
        return;
    };
    if current_generation != generation {
        return;
    }
    mark_uncacheable_in_guard(access.cache, &mut guard, table, ctx, generation);
}

/// `(table, ctx)` の指定世代 `generation` を「この世代は brute-force へ縮退中」の
/// 負のキャッシュとして記録する（`record_build_failed`〔構築失敗〕・
/// `HnswIndexCache::record_base`〔構築成功したが単体で `MAX_HNSW_CACHE_TOTAL_BYTES`
/// を超えキャッシュ不可〕の共通処理）。既存エントリがあれば `base`／`overlay` は
/// 温存したまま `build_failed_generation` のみ更新し、無ければ `base: None` の
/// 負のキャッシュ専用エントリを新設する。世代が進めば次回参照時に再度構築を
/// 試行できる（`docs/design/hnsw-generation-cache.md`「構築失敗時の負のキャッシュ」
/// 節参照）。呼び出し元は書き込みロック `guard` を既に保持している前提。
fn mark_uncacheable_in_guard(
    cache: &HnswIndexCache,
    guard: &mut std::sync::RwLockWriteGuard<'_, HnswCacheState>,
    table: &str,
    ctx: &PolicyContext,
    generation: u64,
) {
    if let Some(entry) = guard
        .entries
        .iter_mut()
        .find(|e| e.table == table && e.built_ctx == *ctx)
    {
        entry.build_failed_generation = Some(generation);
        return;
    }
    // 既存エントリが無い場合（一度も構築成功したことがないテーブル・ctx での
    // 初回失敗、またはキャッシュ不可判定）: `base: None` の負のキャッシュ専用
    // エントリを新設する。これを省略すると、毎クエリ `IndexedBase::build`
    // （索引構築本体・決して安価ではない）を再試行し続けてしまう（同一世代内の
    // 再試行連打を防ぐという本節の目的そのものを一度も構築成功していないテーブルで
    // 満たせなくなる）。
    cache.evict_for_capacity(guard, 0);
    let seq = cache.seq.fetch_add(1, Ordering::Relaxed);
    guard.entries.push(HnswCacheEntry {
        table: table.to_string(),
        built_ctx: ctx.clone(),
        base: None,
        overlay: None,
        build_failed_generation: Some(generation),
        last_used: seq,
    });
}

fn record_overlay_for(
    access: &HnswCacheAccess<'_>,
    table: &str,
    base: &Arc<IndexedBase>,
    overlay: Arc<Overlay>,
) {
    let Ok(mut guard) = access.cache.state.write() else {
        return;
    };
    let Ok(read_txn) = access.storage.db().begin_read() else {
        return;
    };
    let Ok(current_generation) = crate::catalog::table_generation_in_txn(&read_txn, table) else {
        return;
    };
    if overlay.generation != current_generation {
        return;
    }
    let overlay_bytes = overlay.approx_heap_bytes();
    let entry_total_bytes = base.approx_heap_bytes().saturating_add(overlay_bytes);
    if entry_total_bytes > MAX_HNSW_CACHE_TOTAL_BYTES {
        // base + overlay の合計だけで総量上限を超える場合は overlay を常駐させ
        // ない（`record_base` と同じ DoS 対策契約。codex-review P1 指摘対応:
        // これを省略すると overlay 追加後の合計容量が再判定されず、テナント・
        // テーブルごとの overlay 差分（`delta_vectors` 等）の蓄積で容量上限を
        // 超えて常駐し得た）。呼び出し元はこのクエリで計算した `overlay` を
        // その場の探索にそのまま使ってよい（`search_or_fallback` は
        // `record_overlay_for` の戻り値を見ず、呼び出し元が保持する `overlay` で
        // 続けて `search_with_overlay` を呼ぶ）。
        //
        // 常駐させないだけでは終わらせず、`record_base`〔単体超過〕・
        // `record_build_failed`〔構築失敗〕と同じ負のキャッシュへ「この世代は
        // キャッシュ不可」と記録する（Bugbot "Oversized overlay skips negative
        // cache" 指摘対応: 記録を省略すると次回参照が再び `NeedOverlay` となり、
        // `search_or_fallback` が同一世代内の毎クエリで `Overlay::compute`
        // 〔delta_slots・delta_vectors の再構築を伴う〕をやり直してしまう）。
        mark_uncacheable_in_guard(
            access.cache,
            &mut guard,
            table,
            &base.built_ctx,
            current_generation,
        );
        return;
    }
    // 対象エントリが既に overlay を保持している場合（世代進行に伴う再計算等）、
    // `HnswCacheEntry::approx_heap_bytes` はその旧 overlay 分を含んだ値を
    // 返すため、`evict_while` が合算する `total_bytes` にも旧 overlay が
    // 計上済みである。ここで新 overlay の全量をそのまま追加分として渡すと
    // 旧 overlay 分を二重計上してしまい、世代更新のたびに無関係エントリや
    // 更新対象自身を過剰に追い出し、次回クエリで高価な HNSW 再構築を強制し
    // 得る（codex-review P2 指摘対応）。旧 overlay 分を差し引いた純増分のみを
    // 渡すことで二重計上を避ける（`record_base` と異なりエントリ数はこの
    // 呼び出しで増えないため、エントリ数上限は判定条件に含めない。
    // Cursor Bugbot 指摘対応: `evict_for_capacity` をそのまま使うとキャッシュが
    // 既に満杯なだけで overlay 更新のたびに無関係な別エントリが巻き添えで
    // 追い出されていた）。このエントリ自身が LRU の対象に選ばれ追い出される
    // 可能性はあるが、その場合は下の `find` が失敗し overlay を反映しない
    // だけであり、容量上限を超えて常駐することはない（fail-closed）。
    let existing_overlay_bytes = guard
        .entries
        .iter()
        .find(|e| {
            e.table == table
                && e.built_ctx == base.built_ctx
                && e.base.as_ref().is_some_and(|b| Arc::ptr_eq(b, base))
        })
        .and_then(|e| e.overlay.as_ref())
        .map(|o| o.approx_heap_bytes())
        .unwrap_or(0);
    let extra_bytes = overlay_bytes.saturating_sub(existing_overlay_bytes);
    access.cache.evict_for_extra_bytes(&mut guard, extra_bytes);
    if let Some(entry) = guard.entries.iter_mut().find(|e| {
        e.table == table
            && e.built_ctx == base.built_ctx
            && e.base.as_ref().is_some_and(|b| Arc::ptr_eq(b, base))
    }) {
        entry.overlay = Some(overlay);
        entry.build_failed_generation = None;
    }
}

/// [`search_with_overlay`] が完了時に成功統計をどこへ加算するか（Issue #409）。
/// `NeedOverlay`（再構築直後）経路からは `None` を渡し従来どおり計上しない。
#[derive(Clone, Copy)]
pub(crate) enum OverlaySuccessStat {
    /// `FullVisible` 形状・`Lookup::Ready` 到達（`hits` の既存契約を変えない）。
    Hits,
    /// `Subset` 形状（SCALAR 事前フィルタ付き DISTANCE。Issue #409）。
    SubsetSearches,
    /// 再構築直後の恒等オーバーレイ等、成功統計を計上しない経路。
    None,
}

/// [`Lookup::Ready`]（または `NeedOverlay` から新規計算した直後）に共通する索引
/// 探索＋未索引分 brute-force のマージ本体（Issue #409 で「可視候補数 ÷ 索引
/// ノード数」の比による plain scan／マスク付き ANN 探索の切替へ書き換え。
/// 旧実装の `k + stale_nodes` オーバーフェッチ〔`k_idx > MAX_EF` で即座に全件
/// brute-force へ縮退〕は撤去し、`Overlay::visible_mask` を
/// `crate::hnsw::HnswIndex::search_masked` へ渡すことで失効ノードを構造的に
/// 除外する。詳細は `docs/design/hnsw-rls-cardinality-switch.md` 参照）。
///
/// `success_stat` は本関数が縮退なしで完了した場合にどの統計を加算するかを
/// 指定する（[`OverlaySuccessStat`] 参照。codex-review P2 指摘対応の延長:
/// `lookup` 側で先に加算すると、本関数がその後の分岐で縮退した場合に成功統計と
/// `fallbacks` の両方へ同一呼び出しが二重計上されてしまうため、常に本関数の
/// 末尾でのみ確定させる）。
///
/// 引数 8 個は `clippy::too_many_arguments`（閾値 7）を超えるが、`search_or_fallback`
/// と同じ方針（本ファイル内 `#[allow(clippy::too_many_arguments)]` 参照）で許容する。
#[allow(clippy::too_many_arguments)]
fn search_with_overlay(
    access: &HnswCacheAccess<'_>,
    base: Arc<IndexedBase>,
    overlay: Arc<Overlay>,
    provider: &dyn SearchProvider,
    arena: &VectorArena,
    query: &[f32],
    k: usize,
    success_stat: OverlaySuccessStat,
) -> Result<Vec<CandidateHit>, KernelError> {
    if overlay.arena_len != arena.len() {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        return full_scan_with_arena(provider, arena, query, k);
    }

    // 可視カーディナリティ切替（Issue #409・`ValidatedHnswParams::full_scan_ratio`。
    // Issue #501 で `TraversalRegime` へ一般化）: `overlay.regime` が
    // `Overlay::compute`（分断検査省略判定）・`prepare_subset`（早期打ち切り）
    // と同一の判定式（[`below_full_scan_ratio`] 由来）を共有する単一情報源
    // （Issue #488 の方針をそのまま踏襲。ここでの再計算を撤去した）。
    if overlay.regime == TraversalRegime::PlainScan {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        access.cache.plain_scans.fetch_add(1, Ordering::Relaxed);
        return full_scan_with_arena(provider, arena, query, k);
    }

    // マスクが複数の連結成分に分かれ、単一 entry point からの探索では受理
    // ノード全体を辿り切れないことが `Overlay::compute`（世代毎に 1 回）で
    // 判明済みの場合、`search_masked` 自体を呼ばず直ちに plain scan へ縮退
    // する（Cursor Bugbot High／codex-review P2 指摘・PR #435。§`Overlay::
    // mask_splits_graph` ドキュメンテーションコメント参照。`masked_short` と
    // 区別できるよう独立カウンタへ計上する）。
    if overlay.mask_splits_graph {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        access
            .cache
            .mask_splits_graph
            .fetch_add(1, Ordering::Relaxed);
        return full_scan_with_arena(provider, arena, query, k);
    }

    // hybrid 密側再取得ループ（Issue #410。`sql::hnsw_hybrid::HnswDenseProvider`）
    // は `fetch_k`（＝ここでの `k`）を [`crate::hnsw::MAX_EF`] 超まで倍増しうる
    // （`hybrid.rs::MAX_FETCH_K` は `MAX_EF` の 4 倍）。`search_masked` 自身も
    // `k > MAX_EF` を `HnswError::InvalidParams` として拒否するが、その `Err`
    // 分岐を汎用 `fallbacks` としてしか区別できない（`HnswError::InvalidParams`
    // の `reason` 文字列比較で判別すると実装詳細への依存になる）ため、ここで
    // 検証順序を先取りして `ef_cap_fallbacks` を独立計上する（`search_masked`
    // 自体は呼ばない。呼んでも同じ結果に帰着するが二重検証を避ける）。
    if k > crate::hnsw::MAX_EF {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        access
            .cache
            .ef_cap_fallbacks
            .fetch_add(1, Ordering::Relaxed);
        return full_scan_with_arena(provider, arena, query, k);
    }

    let ef = access.provider.effective_ef(k);
    // visited 集合の切替（Issue #497）: `access.provider.sparse_visited_max()`
    // は構築時の静的設定値（既定 0＝常に dense）を `HnswIndex::search_masked_with_hop`
    // へそのまま渡す。選ばれた実装は `scratch.last_visited_kind()` から読み、
    // 縮退なしで完走した場合のみ `sparse_visited_searches` へ計上する
    // （下の `masked_short`／`arena_identity_mismatch_guard` 等の縮退判定より
    // 後段で計上するため、クロージャ内で `Option<VisitedKind>` を一緒に返す）。
    // `hop`（Issue #501）は `overlay.regime`（`Overlay::compute` が
    // 世代毎に 1 回だけ判定した単一情報源）からそのまま導出する——本関数側で
    // 独自に可視カーディナリティ比を再判定しない。
    let hop = overlay.regime.hop();
    let (index_hits, visited_kind, acorn_expansions, acorn_descent_bridges) =
        SEARCH_SCRATCH.with(|scratch| {
            let mut scratch = scratch.borrow_mut();
            let result = base.index.search_masked_with_hop(
                query,
                k,
                ef,
                Some(&overlay.visible_mask),
                access.provider.sparse_visited_max(),
                hop,
                &mut scratch,
            );
            (
                result,
                scratch.last_visited_kind(),
                scratch.last_acorn_expansions(),
                scratch.last_acorn_descent_bridges(),
            )
        });
    let index_hits = match index_hits {
        Ok(hits) => hits,
        Err(_) => {
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            return full_scan_with_arena(provider, arena, query, k);
        }
    };

    // TwoHop（ACORN-1）展開過多ガード（Issue #681・親 #674）: 橋渡し展開
    // （層 0 `bridge_expand` の受理件数。`acorn_expansions`）が可視ノード数
    // に対する `ValidatedHnswParams::acorn_max_expansion_ratio` を超えた場合、
    // 探索コストは既に払っているが ANN 結果を捨てて plain scan（既存の縮退
    // 経路。テナント境界は `full_scan_with_arena` が使う ctx 可視アリーナの
    // ままで一切緩めない）へ fail-closed に切り替える——展開過多による Recall
    // 低下（Issue #674 Phase 2 の実測）より結果の正しさを優先する。既定
    // `acorn_max_expansion_ratio == None` の間はこの分岐へ一切入らず、
    // 以降のコード経路（`finish_indexed_search` の呼び出し・統計）はビット
    // 同一のまま変わらない。
    if hop == crate::hnsw::HopMode::TwoHop {
        if let Some(ratio) = access.provider.acorn_max_expansion_ratio() {
            if crate::hnsw::acorn_expansions_exceed(
                acorn_expansions,
                overlay.visible_in_index,
                ratio,
            ) {
                access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
                access
                    .cache
                    .acorn_guard_fallbacks
                    .fetch_add(1, Ordering::Relaxed);
                return full_scan_with_arena(provider, arena, query, k);
            }
        }
    }

    // 単発経路は当該呼び出し限定の縮退判定を使わない（診断用
    // `hybrid_resumed_rounds` は再開型経路 `search_prepared_resumable` のみが
    // 参照する）ため、ここでは使い捨ての `Cell` を渡す。
    let fell_back = std::cell::Cell::new(false);
    finish_indexed_search(
        access,
        &base,
        &overlay,
        provider,
        arena,
        query,
        k,
        success_stat,
        index_hits,
        visited_kind,
        hop,
        acorn_expansions,
        acorn_descent_bridges,
        &fell_back,
    )
}

/// [`search_with_overlay`]（単発探索）・[`search_prepared_resumable`]
/// （Issue #505 の再開型探索。`sql::hnsw_hybrid::HnswDenseProvider`）が共有する
/// 後段（結果件数の充足検査・スロット写像＋`(tenant_id, id)` 照合・
/// `kernel::dot` 再計算・未索引分 delta マージ・ソート/重複排除/truncate・
/// 成功統計）。索引の生の探索結果（`index_hits`）を受け取るところから開始する
/// ため、呼び出し元が単発 [`crate::hnsw::HnswIndex::search_masked_with_hop`]・
/// 再開型 [`crate::hnsw::HnswIndex::search_masked_resumable_start`]／
/// [`Self::search_masked_resume`] のどちらを使ったかによらず同一の後段契約
/// （エラー・統計・出力順）を保つ。
///
/// 本関数自体は再開状態（`ResumableMaskedSearch`）に一切触れない——本関数が
/// 内部で plain scan／brute-force へ縮退した場合でも、それは「この 1 ラウンドの
/// 出力を brute-force で埋め合わせた」だけであり、既にグラフ探索を終えて
/// `resume` へ格納済みの状態（`candidates`／`discarded` 等）自体は引き続き
/// 有効（次ラウンドも再開できる）。再開状態の破棄が必要なのは、そもそも
/// [`crate::hnsw::HnswIndex::search_masked_resumable_start`]／
/// `search_masked_resume` を呼ばずに済ませた前段ガード
/// （`search_prepared_resumable` 側。アリーナ不一致・`PlainScan`・
/// `mask_splits_graph`・`k > MAX_EF`）だけである。
#[allow(clippy::too_many_arguments)]
fn finish_indexed_search(
    access: &HnswCacheAccess<'_>,
    base: &IndexedBase,
    overlay: &Overlay,
    provider: &dyn SearchProvider,
    arena: &VectorArena,
    query: &[f32],
    k: usize,
    success_stat: OverlaySuccessStat,
    index_hits: Vec<crate::kernel::CandidateHit>,
    visited_kind: Option<crate::hnsw::VisitedKind>,
    hop: crate::hnsw::HopMode,
    acorn_expansions: u64,
    acorn_descent_bridges: u64,
    fell_back: &std::cell::Cell<bool>,
) -> Result<Vec<CandidateHit>, KernelError> {
    // マスク付き探索の結果件数が「可視ノード数と要求 k の小さい方」に満たない
    // 場合、ビーム幅内でグラフ探索が可視ノードを十分辿り切れなかったことを
    // 意味する（§`docs/design/hnsw-rls-cardinality-switch.md`「masked_short」節）。
    // `#410` の担当である `ef` 拡張再探索は行わず、fail-closed に plain scan へ
    // 縮退して k 件充足を保証する。
    let expected = k.min(overlay.visible_in_index);
    if index_hits.len() < expected {
        access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
        access.cache.masked_short.fetch_add(1, Ordering::Relaxed);
        fell_back.set(true);
        return full_scan_with_arena(provider, arena, query, k);
    }

    let mut mapped: Vec<CandidateHit> = Vec::with_capacity(index_hits.len());
    for hit in index_hits {
        let node = hit.id as u32;
        let Some(&slot) = overlay.slot_of_node.get(node as usize) else {
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            fell_back.set(true);
            return full_scan_with_arena(provider, arena, query, k);
        };
        if slot == STALE_SLOT {
            // `search_masked` はマスク受理ノードのみを結果に含むはずであり、
            // 通常この分岐には到達しない（`overlay.visible_mask` は
            // `slot_of_node[node] != STALE_SLOT` のノードのみを受理する）。
            // マスクとオーバーレイの不整合という想定外事態を fail-closed に
            // 全件 brute-force へ倒す（黙って握りつぶさない）。
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            fell_back.set(true);
            return full_scan_with_arena(provider, arena, query, k);
        }
        let slot_usize = slot as usize;
        let Some(node_key) = base.node_keys.get(node as usize) else {
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            fell_back.set(true);
            return full_scan_with_arena(provider, arena, query, k);
        };
        let arena_tenant = arena.tenant_id(slot_usize);
        let arena_id = arena.ids().get(slot_usize).copied();
        if arena_tenant != Some(node_key.0.as_str()) || arena_id != Some(node_key.1) {
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            fell_back.set(true);
            return full_scan_with_arena(provider, arena, query, k);
        }
        let Some(vec_at_slot) = arena.vector(slot_usize) else {
            access.cache.fallbacks.fetch_add(1, Ordering::Relaxed);
            fell_back.set(true);
            return full_scan_with_arena(provider, arena, query, k);
        };
        let score = crate::kernel::dot(vec_at_slot, query);
        mapped.push(CandidateHit {
            id: slot as u64,
            score,
        });
    }

    if !overlay.delta_slots.is_empty() {
        access.cache.delta_searches.fetch_add(1, Ordering::Relaxed);
        let delta_input = SearchInput {
            ids: &overlay.delta_slots,
            vectors: &overlay.delta_vectors,
            dim: arena.dim(),
            query,
            k,
        };
        let delta_hits = provider.search(delta_input)?;
        mapped.extend(delta_hits);
    }

    mapped.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id)));
    mapped.dedup_by(|a, b| a.id == b.id);
    mapped.truncate(k);
    match success_stat {
        // ここへ到達するのは、上記のいずれの縮退分岐（`fallbacks` 加算）も通らず
        // 索引探索が完了した場合のみ。成功統計の契約はこの時点で初めて確定する。
        OverlaySuccessStat::Hits => {
            access.cache.hits.fetch_add(1, Ordering::Relaxed);
        }
        OverlaySuccessStat::SubsetSearches => {
            access.cache.subset_searches.fetch_add(1, Ordering::Relaxed);
        }
        OverlaySuccessStat::None => {}
    }
    // 縮退なしで完走した場合のみ、実際に選ばれた visited 実装を計上する
    // （Issue #497）。`hits`／`subset_searches` とは独立に数える診断用カウンタ。
    if visited_kind == Some(crate::hnsw::VisitedKind::Sparse) {
        access
            .cache
            .sparse_visited_searches
            .fetch_add(1, Ordering::Relaxed);
    }
    // `TraversalRegime::TwoHop`（ACORN-1・Issue #501）で縮退なしに完走した
    // 場合のみ `acorn_searches`／`acorn_expansions` を計上する（`hits`／
    // `subset_searches` と同じく独立カウンタ。既定 `acorn_max_visible_ratio ==
    // None` の間は `hop` が常に `OneHop` のためこの分岐へ到達しない）。
    if hop == crate::hnsw::HopMode::TwoHop {
        access.cache.acorn_searches.fetch_add(1, Ordering::Relaxed);
        access
            .cache
            .acorn_expansions
            .fetch_add(acorn_expansions, Ordering::Relaxed);
        access
            .cache
            .acorn_descent_bridges
            .fetch_add(acorn_descent_bridges, Ordering::Relaxed);
    }
    Ok(mapped)
}

fn full_scan_with_arena(
    provider: &dyn SearchProvider,
    arena: &VectorArena,
    query: &[f32],
    k: usize,
) -> Result<Vec<CandidateHit>, KernelError> {
    let mut slot_ids: Vec<u64> = Vec::with_capacity(arena.len());
    for slot in 0..arena.len() {
        slot_ids.push(slot as u64);
    }
    let input = SearchInput {
        ids: &slot_ids,
        vectors: arena.vectors(),
        dim: arena.dim(),
        query,
        k,
    };
    provider.search(input)
}

/// ANN（HNSW）経路の静的な適用判定（Issue #411。`sql::exec` の 4 条件
/// （`hnsw_full_visible_eligible`／`hnsw_subset_eligible`／
/// `hnsw_hybrid_full_visible_eligible`／`hnsw_hybrid_subset_eligible`）と
/// `sql::explain` が同一の実装を共有するための単一情報源）。
///
/// `sql::exec` 側は本関数の戻り値と `is_hybrid` の真偽から 4 boolean を導出する
/// （`!is_hybrid && plan == FullVisible` のように組み合わせる。導出が現行の 4 式と
/// ビット一致することは in-module テスト [`tests::classify_ann_plan_matches_exec_eligibility_truth_table`]
/// で固定する）。`sql::explain::build_explain_result` は `EXPLAIN` が検索本体を
/// 実行しない契約（モジュールドキュメント冒頭参照）を保つため、実行時の縮退結果
/// ではなくクエリ形状とエンジン設定から決まるこの**静的判定**をそのまま報告する
/// （実行時 fail-closed 縮退・hybrid 再取得ラウンド数は対象外。
/// `docs/design/explain-search-engine-exposure.md` 参照）。
///
/// TASK-186・NOSQL-10 の前提として Issue #730 で公開 API へ昇格した
/// （`sql::explain::build_explain_result` を engine クレート外から実際に呼ぶには
/// 入力である本 enum も構築できる必要があるため。`#[non_exhaustive]` を付け
/// クレート外からの網羅 `match` を禁止する〔`SearchEngineKind` と同じ拡張点
/// 保護。クレート内の網羅 `match`（`sql::explain::ann_plan_token` 等）は
/// `#[non_exhaustive]` の影響を受けない〕）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum AnnPlan {
    /// エンジンが `SearchEngineKind::Hnsw` でない（`hnsw_cache` が `None`）ため
    /// 常に全件 brute-force。`SearchEngineKind` が既知（`CpuScalarBruteForce`／
    /// `ParallelBruteForce`）の場合のみこの分類を返す。
    PlainScanEngine,
    /// HNSW だが `precision` モード（TASK-162・SEARCH-9）のため厳密 brute-force。
    PlainScanPrecision,
    /// 索引経路・`FullVisible` 形状（フィルタなし、または DISTANCE 先行で
    /// SCALAR 条件を事後判定する形。上記コメント「Issue #409」参照）。
    HnswFullVisible,
    /// 索引経路・`Subset` 形状（SCALAR 事前フィルタ付き。per-query 写像・
    /// キャッシュ非登録）。
    HnswSubset,
    /// `EngineCore::search_engine_kind()` が `None`（`with_provider`／
    /// `from_storage` 経由でカスタム [`crate::kernel::SearchProvider`] を直接
    /// 注入した構築経路。`kind` と provider の対応をこの構造体自身は検証
    /// できない）ため、実際に ANN か brute-force かを `EngineCore` 側からは
    /// 判別できない（codex-review P1 指摘・PR #437。`hnsw_enabled == false` を
    /// 「厳密 brute-force と確定している」`PlainScanEngine` へ丸めてしまうと、
    /// カスタム provider が内部で ANN 相当の近似探索を行っていても `EXPLAIN`
    /// が plain scan と誤断定する）。
    UnknownCustomProvider,
}

/// [`classify_ann_plan`] の入力（`sql::exec::execute_statement_with_cache` と
/// `sql::explain` 双方が同じ形へ組み立てる）。TASK-186・NOSQL-10 の前提として
/// Issue #730 で公開 API へ昇格した（フィールドはすべて `bool` でテナント存在
/// 情報を持たないため、`#[non_exhaustive]` は付けずクレート外が構造体リテラルで
/// 直接組み立てられるようにする）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnnShapeInput {
    /// `hnsw_cache.is_some()`（`EngineCore::hnsw_state` の有無。
    /// `SearchEngineKind::Hnsw` opt-in 構築時のみ `true`）。
    pub hnsw_enabled: bool,
    /// `EngineCore::search_engine_kind().is_none()`（`with_provider`／
    /// `from_storage` 経由でカスタム provider を直接注入した構築経路。
    /// Issue #411 追記・codex-review P1 指摘・PR #437）。`hnsw_enabled == false`
    /// の場合のみ参照し、`true` を「実行方式が不明」（[`AnnPlan::
    /// UnknownCustomProvider`]）、`false` を「エンジン種別は既知で brute-force
    /// と確定している」（[`AnnPlan::PlainScanEngine`]）に振り分ける。`sql::exec`
    /// の 4 boolean（`hnsw_full_visible_eligible` 等）はこの分岐に依存しない
    /// （`UnknownCustomProvider` も `PlainScanEngine` も等しく `HnswFullVisible`／
    /// `HnswSubset` ではないため）ため、`sql::exec` は常に `false` を渡してよい。
    pub engine_kind_unknown: bool,
    /// `Ranking::Hybrid` かどうか。分類本体（[`classify_ann_plan`]）は
    /// `is_hybrid` を参照しない（`FullVisible`／`Subset` の形状判定自体は
    /// `Ranking` の種別に依存しないため。上記コメント「Issue #410」参照）。
    /// `sql::exec` 側の 4 boolean 導出・判別テストが 1 つの構造体から
    /// 4 通りすべてを再現できるように、この構造体に残す。
    pub is_hybrid: bool,
    /// `bound.mode`（`USING PLAN` 経由の推定を含む）が `precision` か。
    pub is_precision: bool,
    /// `bound.metadata_filters` と `bound.expr_filters` がともに空か。
    pub filters_empty: bool,
    /// `ExecutionPlan::from_evaluation_order(..).scalar_prefilter`
    /// （SCALAR 段が DISTANCE 段より先に評価されるか）。
    pub scalar_prefilter: bool,
}

/// [`AnnShapeInput`] から [`AnnPlan`] を決定する（純粋関数・副作用なし）。
/// `sql::exec` の現行 4 式と同値であることは
/// [`tests::classify_ann_plan_matches_exec_eligibility_truth_table`] で固定する。
/// TASK-186・NOSQL-10 の前提として Issue #730 で公開 API へ昇格した
/// （`sql::explain::build_explain_result` が `EXPLAIN` と同じ `ann_plan:` 値を
/// 返すための単一情報源を、engine クレート外からも共有できるようにするため）。
pub fn classify_ann_plan(input: AnnShapeInput) -> AnnPlan {
    if !input.hnsw_enabled {
        return if input.engine_kind_unknown {
            AnnPlan::UnknownCustomProvider
        } else {
            AnnPlan::PlainScanEngine
        };
    }
    if input.is_precision {
        return AnnPlan::PlainScanPrecision;
    }
    if input.filters_empty || !input.scalar_prefilter {
        AnnPlan::HnswFullVisible
    } else {
        AnnPlan::HnswSubset
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::rls::ImplicitRlsHook;
    use crate::storage::{RowInput, Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    /// [`classify_ann_plan`] からの導出（`!is_hybrid && plan == FullVisible` 等）が
    /// `sql::exec::execute_statement_with_cache` の現行 4 式とビット一致することを
    /// 32 通り（2^5: hnsw_enabled・is_hybrid・is_precision・filters_empty・
    /// scalar_prefilter）全数で固定する（Issue #411）。旧式はここに複製し、
    /// `sql/exec.rs` 側は本関数呼び出しへ置換済み（二重管理を避けるため、
    /// `exec.rs` を読む代わりにこの固定テストが両者の同値性を保証する）。
    #[test]
    fn classify_ann_plan_matches_exec_eligibility_truth_table() {
        for hnsw_enabled in [false, true] {
            for is_hybrid in [false, true] {
                for is_precision in [false, true] {
                    for filters_empty in [false, true] {
                        for scalar_prefilter in [false, true] {
                            // `sql/exec.rs` の現行 4 式（置換前のオリジナル）。
                            let expected_full_visible = hnsw_enabled
                                && !is_hybrid
                                && !is_precision
                                && (filters_empty || !scalar_prefilter);
                            let expected_subset = hnsw_enabled
                                && !is_hybrid
                                && !is_precision
                                && !filters_empty
                                && scalar_prefilter;
                            let expected_hybrid_full_visible = hnsw_enabled
                                && is_hybrid
                                && !is_precision
                                && (filters_empty || !scalar_prefilter);
                            let expected_hybrid_subset = hnsw_enabled
                                && is_hybrid
                                && !is_precision
                                && !filters_empty
                                && scalar_prefilter;

                            let plan = classify_ann_plan(AnnShapeInput {
                                hnsw_enabled,
                                engine_kind_unknown: false,
                                is_hybrid,
                                is_precision,
                                filters_empty,
                                scalar_prefilter,
                            });

                            let got_full_visible = !is_hybrid && plan == AnnPlan::HnswFullVisible;
                            let got_subset = !is_hybrid && plan == AnnPlan::HnswSubset;
                            let got_hybrid_full_visible =
                                is_hybrid && plan == AnnPlan::HnswFullVisible;
                            let got_hybrid_subset = is_hybrid && plan == AnnPlan::HnswSubset;

                            assert_eq!(
                                got_full_visible, expected_full_visible,
                                "hnsw_enabled={hnsw_enabled} is_hybrid={is_hybrid} \
                                 is_precision={is_precision} filters_empty={filters_empty} \
                                 scalar_prefilter={scalar_prefilter}"
                            );
                            assert_eq!(
                                got_subset, expected_subset,
                                "hnsw_enabled={hnsw_enabled} is_hybrid={is_hybrid} \
                                 is_precision={is_precision} filters_empty={filters_empty} \
                                 scalar_prefilter={scalar_prefilter}"
                            );
                            assert_eq!(
                                got_hybrid_full_visible, expected_hybrid_full_visible,
                                "hnsw_enabled={hnsw_enabled} is_hybrid={is_hybrid} \
                                 is_precision={is_precision} filters_empty={filters_empty} \
                                 scalar_prefilter={scalar_prefilter}"
                            );
                            assert_eq!(
                                got_hybrid_subset, expected_hybrid_subset,
                                "hnsw_enabled={hnsw_enabled} is_hybrid={is_hybrid} \
                                 is_precision={is_precision} filters_empty={filters_empty} \
                                 scalar_prefilter={scalar_prefilter}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn classify_ann_plan_plain_scan_engine_when_hnsw_disabled() {
        let plan = classify_ann_plan(AnnShapeInput {
            hnsw_enabled: false,
            engine_kind_unknown: false,
            is_hybrid: false,
            is_precision: false,
            filters_empty: true,
            scalar_prefilter: false,
        });
        assert_eq!(plan, AnnPlan::PlainScanEngine);
    }

    /// codex-review P1 指摘対応（PR #437）: `hnsw_enabled == false` かつ
    /// `engine_kind_unknown == true`（`EngineCore::search_engine_kind()` が
    /// `None` を返す `with_provider`／`from_storage` 経由）では、実際に ANN か
    /// brute-force かを判別できないため `PlainScanEngine`（厳密 brute-force と
    /// 確定）ではなく `UnknownCustomProvider` を返すことを固定する。
    #[test]
    fn classify_ann_plan_unknown_custom_provider_when_engine_kind_unknown() {
        let plan = classify_ann_plan(AnnShapeInput {
            hnsw_enabled: false,
            engine_kind_unknown: true,
            is_hybrid: false,
            is_precision: false,
            filters_empty: true,
            scalar_prefilter: false,
        });
        assert_eq!(plan, AnnPlan::UnknownCustomProvider);
    }

    #[test]
    fn classify_ann_plan_plain_scan_precision_when_precision_mode() {
        let plan = classify_ann_plan(AnnShapeInput {
            hnsw_enabled: true,
            engine_kind_unknown: false,
            is_hybrid: false,
            is_precision: true,
            filters_empty: true,
            scalar_prefilter: false,
        });
        assert_eq!(plan, AnnPlan::PlainScanPrecision);
    }

    #[test]
    fn classify_ann_plan_hnsw_full_visible_when_filters_empty() {
        let plan = classify_ann_plan(AnnShapeInput {
            hnsw_enabled: true,
            engine_kind_unknown: false,
            is_hybrid: false,
            is_precision: false,
            filters_empty: true,
            scalar_prefilter: false,
        });
        assert_eq!(plan, AnnPlan::HnswFullVisible);
    }

    #[test]
    fn classify_ann_plan_hnsw_subset_when_scalar_prefilter() {
        let plan = classify_ann_plan(AnnShapeInput {
            hnsw_enabled: true,
            engine_kind_unknown: false,
            is_hybrid: false,
            is_precision: false,
            filters_empty: false,
            scalar_prefilter: true,
        });
        assert_eq!(plan, AnnPlan::HnswSubset);
    }

    /// 恒等マスク（全ノード受理）＋全ノード可視の `visible_in_index` を返す
    /// テスト専用ヘルパ（Issue #409 で `Overlay` に追加した 2 フィールドを
    /// 「未失効の索引をそのまま使う」テストで簡潔に埋めるため）。
    fn identity_mask_and_visible(base: &IndexedBase) -> (crate::hnsw::NodeMask, usize) {
        let mut mask = crate::hnsw::NodeMask::new(base.index.len());
        for node in 0..base.index.len() {
            if let Ok(node_u32) = u32::try_from(node) {
                mask.set(node_u32);
            }
        }
        (mask, base.index.len())
    }

    fn ctx(tenant: &str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant")
    }

    fn create_table(storage: &Storage, name: &str, dim: u32) {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![ColumnDef::new("embedding", ColumnType::Vector(dim), false)],
            ))
            .expect("create table");
    }

    fn seed_row(storage: &Storage, table: &str, id: u64, tenant: &str, embedding: &[f32]) {
        let ctx = PolicyContext::new(tenant).expect("valid tenant");
        let op_id = crate::recovery::required_op_id::OperationId::parse(&format!(
            "hnsw-cache-test-{tenant}-{table}-{id}"
        ))
        .expect("valid operation_id");
        crate::tenant::insert_row(
            storage,
            table,
            &ctx,
            id,
            &RowInput {
                tenant_id: tenant,
                visibility: Visibility::Public,
                embedding,
                metadata: &[],
            },
            &op_id,
        )
        .expect("seed row");
    }

    fn build_arena(
        read_txn: &redb::ReadTransaction,
        table: &str,
        ctx: &PolicyContext,
    ) -> VectorArena {
        let hook = ImplicitRlsHook::new(ctx);
        VectorArena::build_filtered_with_rows_in_txn(
            read_txn,
            table,
            hook.predicate(),
            |_, _, _, _| Ok(true),
        )
        .expect("build arena")
    }

    #[test]
    fn insert_does_not_cache_when_generation_already_advanced_but_still_returns_base() {
        let path = unique_db_path("hnsw-cache-stale-base");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let stale_gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let write_txn = storage.db().begin_write().unwrap();
        crate::catalog::bump_table_generation_in_txn(&write_txn, "docs").unwrap();
        crate::recovery::commit_boundary::commit(write_txn).unwrap();

        let arena = build_arena(&read_txn, "docs", &c);
        let built = IndexedBase::build(
            &arena,
            crate::hnsw::HnswParams::default(),
            crate::hnsw::ResidentPrecision::F32,
            c.clone(),
            stale_gen,
        )
        .expect("build")
        .0;
        let returned = cache.record_base(&storage, "docs", built);
        assert_eq!(returned.built_table_generation, stale_gen);

        // キャッシュへは反映されていない。
        let read_txn2 = storage.db().begin_read().unwrap();
        assert!(matches!(
            cache.lookup(&storage, &read_txn2, "docs", &c),
            Lookup::Miss
        ));
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn lookup_misses_for_different_tenant_ctx() {
        let path = unique_db_path("hnsw-cache-tenant");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let owner = ctx("tenant-a");
        let other = ctx("tenant-b");

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let arena = build_arena(&read_txn, "docs", &owner);
        let built = IndexedBase::build(
            &arena,
            crate::hnsw::HnswParams::default(),
            crate::hnsw::ResidentPrecision::F32,
            owner.clone(),
            gen,
        )
        .expect("build")
        .0;
        cache.record_base(&storage, "docs", built);

        let read_txn2 = storage.db().begin_read().unwrap();
        assert!(matches!(
            cache.lookup(&storage, &read_txn2, "docs", &other),
            Lookup::Miss
        ));
    }

    #[test]
    fn record_base_evicts_only_target_table_generation_mismatch_free() {
        // `record_base` はテーブル横断で既存エントリを巻き込んで破棄しない
        // （`SqlArenaCache::insert` の cross-table 非破壊テストと同型）。
        let path = unique_db_path("hnsw-cache-cross-table");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs_a", 4);
        create_table(&storage, "docs_b", 4);
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        let read_txn_a = storage.db().begin_read().unwrap();
        let gen_a = crate::catalog::table_generation_in_txn(&read_txn_a, "docs_a").unwrap();
        let arena_a = build_arena(&read_txn_a, "docs_a", &c);
        let built_a = IndexedBase::build(
            &arena_a,
            crate::hnsw::HnswParams::default(),
            crate::hnsw::ResidentPrecision::F32,
            c.clone(),
            gen_a,
        )
        .expect("build a")
        .0;
        cache.record_base(&storage, "docs_a", built_a);

        let read_txn_b = storage.db().begin_read().unwrap();
        let gen_b = crate::catalog::table_generation_in_txn(&read_txn_b, "docs_b").unwrap();
        let arena_b = build_arena(&read_txn_b, "docs_b", &c);
        let built_b = IndexedBase::build(
            &arena_b,
            crate::hnsw::HnswParams::default(),
            crate::hnsw::ResidentPrecision::F32,
            c.clone(),
            gen_b,
        )
        .expect("build b")
        .0;
        cache.record_base(&storage, "docs_b", built_b);

        assert_eq!(cache.stats().entries, 2);
    }

    #[test]
    fn evict_entry_removes_only_the_matching_ctx_entry_for_that_table() {
        // Cursor Bugbot 指摘対応（PR #434）: 可視行数が少ない/0 件のテナントの
        // クエリで `evict_entry` が呼ばれても、同一テーブルの他テナントの
        // 構築済みエントリは温存されることを固定する回帰テスト。
        let path = unique_db_path("hnsw-cache-evict-entry");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let c1 = ctx("tenant-a");
        let c2 = ctx("tenant-b");
        let mut built_generation = 0u64;

        for c in [&c1, &c2] {
            let read_txn = storage.db().begin_read().unwrap();
            let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
            built_generation = gen;
            let arena = build_arena(&read_txn, "docs", c);
            let built = IndexedBase::build(
                &arena,
                crate::hnsw::HnswParams::default(),
                crate::hnsw::ResidentPrecision::F32,
                c.clone(),
                gen,
            )
            .expect("build")
            .0;
            cache.record_base(&storage, "docs", built);
        }
        assert_eq!(cache.stats().entries, 2);
        // 現在のクエリが見ている世代（`built_generation`）以下のエントリのみが
        // 破棄対象になる契約を固定する（Cursor Bugbot 指摘対応・PR #434）。
        cache.evict_entry("docs", &c1, built_generation);
        assert_eq!(cache.stats().entries, 1);
        cache.evict_entry("docs", &c2, built_generation);
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn evict_entry_does_not_evict_a_newer_generation_entry() {
        // Cursor Bugbot 指摘対応（PR #434）: 可視行数が縮小した古い世代のクエリが
        // `evict_entry` を呼んでも、並行して別クエリが構築した「より新しい世代」
        // のエントリは巻き添えで破棄されないことを固定する回帰テスト。
        let path = unique_db_path("hnsw-cache-evict-entry-generation");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let old_generation = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let arena = build_arena(&read_txn, "docs", &c);
        let built = IndexedBase::build(
            &arena,
            crate::hnsw::HnswParams::default(),
            crate::hnsw::ResidentPrecision::F32,
            c.clone(),
            old_generation,
        )
        .expect("build")
        .0;
        cache.record_base(&storage, "docs", built);
        assert_eq!(cache.stats().entries, 1);

        // このエントリより古い世代（`old_generation` 未満）を「現在のクエリの
        // 世代」として `evict_entry` を呼んでも、より新しいエントリは保護され
        // 破棄されない。
        let stale_current_generation = old_generation.saturating_sub(1);
        cache.evict_entry("docs", &c, stale_current_generation);
        assert_eq!(
            cache.stats().entries,
            1,
            "より新しい世代のエントリが誤って破棄された"
        );

        // 現在のクエリの世代がエントリの世代以上であれば、通常どおり破棄される。
        cache.evict_entry("docs", &c, old_generation);
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn overlay_classifies_unchanged_new_and_changed_rows() {
        let path = unique_db_path("hnsw-cache-overlay-classify");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let c = ctx("tenant-a");
        seed_row(&storage, "docs", 1, "tenant-a", &[1.0, 0.0, 0.0, 0.0]);
        seed_row(&storage, "docs", 2, "tenant-a", &[0.0, 1.0, 0.0, 0.0]);

        let read_txn = storage.db().begin_read().unwrap();
        let gen0 = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let arena0 = build_arena(&read_txn, "docs", &c);
        let base = IndexedBase::build(
            &arena0,
            crate::hnsw::HnswParams::default(),
            crate::hnsw::ResidentPrecision::F32,
            c.clone(),
            gen0,
        )
        .expect("build")
        .0;

        // id=2 の内容を変更し、id=3 を新規追加する。
        let op_id_update =
            crate::recovery::required_op_id::OperationId::parse("hnsw-cache-overlay-update")
                .unwrap();
        crate::tenant::update_row(
            &storage,
            "docs",
            &c,
            2,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[0.0, 0.0, 1.0, 0.0],
                metadata: &[],
            },
            &op_id_update,
        )
        .expect("update row");
        seed_row(&storage, "docs", 3, "tenant-a", &[0.0, 0.0, 0.0, 1.0]);

        let read_txn2 = storage.db().begin_read().unwrap();
        let gen1 = crate::catalog::table_generation_in_txn(&read_txn2, "docs").unwrap();
        assert!(gen1 > gen0);
        let arena1 = build_arena(&read_txn2, "docs", &c);
        let overlay = Overlay::compute(
            &base,
            &arena1,
            gen1,
            Ratio {
                numerator: 1,
                denominator: 10,
            },
            None,
        );

        // id=1 は変更なしなのでどこかのノードが失効していない（stale_nodes は id=2
        // 分の 1 件のみ）。
        assert_eq!(overlay.stale_nodes, 1);
        // 未索引分は id=2（変更）・id=3（新規）の 2 件。
        assert_eq!(overlay.delta_slots.len(), 2);
        // 差分（stale 1 + delta 2 = 3 件）が全体（3 件）の 10% を大きく超えるため、
        // このミニチュアフィクスチャでは再構築閾値を超える（`needs_rebuild` の
        // 挙動自体は `search_or_fallback` の rebuild 分岐で結合テストする）。
        assert!(overlay.needs_rebuild(arena1.len()));
    }

    /// `arena` 内で `id` を持つ行のスロット番号を線形探索する（テスト専用。
    /// `arena.ids()` は `VectorArena::build_*` が確定した順序を保つため、
    /// `kept` 候補列を組み立てる際に元スロット番号を求めるのに使う）。
    fn slot_for_id(arena: &VectorArena, id: u64) -> u32 {
        arena
            .ids()
            .iter()
            .position(|&x| x == id)
            .map(|i| i as u32)
            .expect("id must exist in arena")
    }

    /// 2 つの `Overlay` が全フィールドにわたって一致することを検証する（Issue
    /// #676。`Overlay` は `PartialEq`/`Debug` を導出していないため、テスト
    /// 専用に手動でフィールド単位の比較を行う）。
    fn assert_overlays_equal(a: &Overlay, b: &Overlay) {
        assert_eq!(a.generation, b.generation, "generation");
        assert_eq!(a.arena_len, b.arena_len, "arena_len");
        assert_eq!(a.slot_of_node, b.slot_of_node, "slot_of_node");
        assert_eq!(a.stale_nodes, b.stale_nodes, "stale_nodes");
        assert_eq!(a.delta_slots, b.delta_slots, "delta_slots");
        assert_eq!(a.delta_vectors, b.delta_vectors, "delta_vectors");
        assert_eq!(a.visible_in_index, b.visible_in_index, "visible_in_index");
        assert_eq!(
            a.mask_splits_graph, b.mask_splits_graph,
            "mask_splits_graph"
        );
        assert_eq!(a.regime, b.regime, "regime");
        // `NodeMask` は `PartialEq` を持たないため、可視ノード番号の集合として比較する。
        let mask_bits = |m: &crate::hnsw::NodeMask| -> Vec<u32> {
            (0..m.len() as u32).filter(|&n| m.get(n)).collect()
        };
        assert_eq!(
            mask_bits(&a.visible_mask),
            mask_bits(&b.visible_mask),
            "visible_mask"
        );
    }

    #[test]
    fn compute_over_slots_full_range_matches_compute() {
        // Issue #676: `compute` は `compute_over_slots(.., 0..arena.len(), ..)` の
        // ラッパーになったため、両者は定義上ビット同一になるはずだが、リグレッション
        // 検出のため独立に呼び出して比較する。
        let path = unique_db_path("hnsw-cache-overlay-full-range");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let c = ctx("tenant-a");
        seed_row(&storage, "docs", 1, "tenant-a", &[1.0, 0.0, 0.0, 0.0]);
        seed_row(&storage, "docs", 2, "tenant-a", &[0.0, 1.0, 0.0, 0.0]);
        seed_row(&storage, "docs", 3, "tenant-a", &[0.0, 0.0, 1.0, 0.0]);

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let arena = build_arena(&read_txn, "docs", &c);
        let base = IndexedBase::build(
            &arena,
            crate::hnsw::HnswParams::default(),
            crate::hnsw::ResidentPrecision::F32,
            c.clone(),
            gen,
        )
        .expect("build")
        .0;
        let ratio = Ratio {
            numerator: 1,
            denominator: 10,
        };

        let via_compute = Overlay::compute(&base, &arena, gen, ratio, None);
        let via_slots =
            Overlay::compute_over_slots(&base, &arena, 0..arena.len(), gen, ratio, None);
        assert_overlays_equal(&via_compute, &via_slots);
    }

    #[test]
    fn compute_over_slots_subset_matches_compute_on_copied_subset_arena() {
        // Issue #676: `compute_over_slots(snapshot, kept)`（複製なし）が
        // `compute(build_from_cached_rls_rows_subset(kept))`（複製あり・従来の
        // HNSW `Subset` 経路）と全フィールド同一の `Overlay` を返すことを固定する。
        // 内容変更行（id=2）・新規行（id=5）・kept から除外される未変更行（id=3。
        // WHERE で除外されただけで削除ではない）を含むフィクスチャで検証する。
        let path = unique_db_path("hnsw-cache-overlay-subset-equiv");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let c = ctx("tenant-a");
        seed_row(&storage, "docs", 1, "tenant-a", &[1.0, 0.0, 0.0, 0.0]);
        seed_row(&storage, "docs", 2, "tenant-a", &[0.0, 1.0, 0.0, 0.0]);
        seed_row(&storage, "docs", 3, "tenant-a", &[0.0, 0.0, 1.0, 0.0]);
        seed_row(&storage, "docs", 4, "tenant-a", &[0.0, 0.0, 0.0, 1.0]);

        let read_txn0 = storage.db().begin_read().unwrap();
        let gen0 = crate::catalog::table_generation_in_txn(&read_txn0, "docs").unwrap();
        let arena0 = build_arena(&read_txn0, "docs", &c);
        let base = IndexedBase::build(
            &arena0,
            crate::hnsw::HnswParams::default(),
            crate::hnsw::ResidentPrecision::F32,
            c.clone(),
            gen0,
        )
        .expect("build")
        .0;

        let op_id_update =
            crate::recovery::required_op_id::OperationId::parse("hnsw-cache-overlay-subset-update")
                .unwrap();
        crate::tenant::update_row(
            &storage,
            "docs",
            &c,
            2,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[0.0, 1.0, 1.0, 0.0],
                metadata: &[],
            },
            &op_id_update,
        )
        .expect("update row");
        seed_row(&storage, "docs", 5, "tenant-a", &[1.0, 0.0, 0.0, 1.0]);

        let read_txn1 = storage.db().begin_read().unwrap();
        let gen1 = crate::catalog::table_generation_in_txn(&read_txn1, "docs").unwrap();
        let arena1 = build_arena(&read_txn1, "docs", &c);

        // kept: id=3 を除いた昇順・重複なしの候補スロット（WHERE が id=3 を
        // 除外したことを模す）。
        let mut kept: Vec<u32> = [1u64, 2, 4, 5]
            .iter()
            .map(|&id| slot_for_id(&arena1, id))
            .collect();
        kept.sort_unstable();

        let ratio = Ratio {
            numerator: 1,
            denominator: 10,
        };
        let via_mask = Overlay::compute_over_slots(
            &base,
            &arena1,
            kept.iter().map(|&s| s as usize),
            gen1,
            ratio,
            None,
        );

        // 対照: `kept` を実際に複製した subset アリーナに対し従来どおり `compute`
        // を呼ぶ（Issue #654 以前の HNSW `Subset` 経路と同型）。
        let metadata: Vec<Vec<u8>> = vec![Vec::new(); arena1.len()];
        let subset_arena = VectorArena::build_from_cached_rls_rows_subset(
            "docs",
            arena1.dim(),
            &arena1,
            &metadata,
            &kept,
            |_, _, _, _| Ok(true),
            crate::arena::MAX_ARENA_ROWS,
            crate::arena::MAX_ARENA_TOTAL_BYTES,
        )
        .expect("build subset arena");
        let via_copy = Overlay::compute(&base, &subset_arena, gen1, ratio, None);

        assert_overlays_equal(&via_mask, &via_copy);
        // 非 vacuous であることの確認（id=2 の内容変更・id=5 の新規で delta が
        // 発生し、kept.len()（4）が全体の視認集合と異なることを確かめる）。
        assert_eq!(via_mask.delta_slots.len(), 2);
        assert_eq!(via_mask.arena_len, 4);
    }

    #[test]
    fn lookup_counts_miss_for_unregistered_entry() {
        // codex-review P2 指摘対応（PR #434）: 対象 `(table, ctx)` のエントリが
        // 一度も登録されていない、最も一般的な初回ミスが `misses` に計上され
        // ないまま返っていた（旧実装）ことの回帰固定。
        let path = unique_db_path("hnsw-cache-miss-count");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        assert!(matches!(
            cache.lookup(&storage, &read_txn, "docs", &c),
            Lookup::Miss
        ));
        assert_eq!(cache.stats().misses, 1);
    }

    #[test]
    fn search_with_overlay_does_not_double_count_hits_on_fallback() {
        // codex-review P2 指摘対応（PR #434）: `Lookup::Ready` 到達の時点で
        // `hits` を先に加算していたため、その後 `search_with_overlay` が
        // 縮退（本テストでは `overlay.arena_len` を意図的に不一致にして最初の
        // 縮退分岐を発火させる）した場合、`hits` と `fallbacks` の両方に同一
        // 呼び出しが二重計上されていた（旧実装では hits == 1 になっていた）。
        let path = unique_db_path("hnsw-cache-hits-fallback");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        seed_row(&storage, "docs", 1, "tenant-a", &[1.0, 0.0, 0.0, 0.0]);
        seed_row(&storage, "docs", 2, "tenant-a", &[0.0, 1.0, 0.0, 0.0]);
        let c = ctx("tenant-a");
        let cache = HnswIndexCache::new();

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let arena = build_arena(&read_txn, "docs", &c);
        let base = IndexedBase::build(
            &arena,
            crate::hnsw::HnswParams::default(),
            crate::hnsw::ResidentPrecision::F32,
            c.clone(),
            gen,
        )
        .expect("build")
        .0;
        let base = Arc::new(base);

        let (visible_mask, visible_in_index) = identity_mask_and_visible(&base);
        let overlay = Arc::new(Overlay {
            generation: gen,
            arena_len: arena.len() + 1,
            slot_of_node: (0..base.index.len() as u32).collect(),
            stale_nodes: 0,
            delta_slots: Vec::new(),
            delta_vectors: Vec::new(),
            visible_mask,
            visible_in_index,
            mask_splits_graph: false,
            regime: TraversalRegime::OneHop,
        });

        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(crate::hnsw::ValidatedHnswParams::default()),
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        let provider = crate::kernel::CpuScalarProvider;
        let query = [1.0, 0.0, 0.0, 0.0];
        let result = search_with_overlay(
            &access,
            base,
            overlay,
            &provider,
            &arena,
            &query,
            1,
            OverlaySuccessStat::Hits,
        )
        .expect("fallback search succeeds");
        assert_eq!(result.len(), 1);

        let stats = cache.stats();
        assert_eq!(stats.fallbacks, 1, "縮退は 1 回のみ計上される");
        assert_eq!(
            stats.hits, 0,
            "縮退した呼び出しは hits に計上されない（旧実装は 1 になっていた）"
        );
    }

    #[test]
    fn record_overlay_for_does_not_evict_unrelated_entries_when_cache_is_at_entry_capacity() {
        // Cursor Bugbot 指摘対応（PR #434）: overlay 更新はエントリを追加しない
        // にもかかわらず、以前は `entries.len() >= MAX_HNSW_CACHE_ENTRIES`
        // （キャッシュが既に満杯）という条件だけで build・rebuild・世代 overlay
        // のたびに無関係な別 `(table, ctx)` の索引エントリが巻き添えで追い出され
        // ていた。エントリ数がちょうど上限に達している状態で overlay 更新して
        // も、総バイト量が上限を超えない限りエントリ数が変わらないことを固定する。
        let path = unique_db_path("hnsw-cache-overlay-no-evict");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        let mut bases = Vec::new();
        for i in 0..MAX_HNSW_CACHE_ENTRIES {
            let table = format!("docs_{i}");
            create_table(&storage, &table, 4);
            let read_txn = storage.db().begin_read().unwrap();
            let gen = crate::catalog::table_generation_in_txn(&read_txn, &table).unwrap();
            let arena = build_arena(&read_txn, &table, &c);
            let built = IndexedBase::build(
                &arena,
                crate::hnsw::HnswParams::default(),
                crate::hnsw::ResidentPrecision::F32,
                c.clone(),
                gen,
            )
            .expect("build")
            .0;
            let base = cache.record_base(&storage, &table, built);
            bases.push((table, base));
        }
        assert_eq!(cache.stats().entries, MAX_HNSW_CACHE_ENTRIES);

        let (table0, base0) = &bases[0];
        let gen0 =
            crate::catalog::table_generation_in_txn(&storage.db().begin_read().unwrap(), table0)
                .unwrap();
        let (visible_mask, visible_in_index) = identity_mask_and_visible(base0);
        let overlay = Arc::new(Overlay {
            generation: gen0,
            arena_len: base0.index.len(),
            slot_of_node: (0..base0.index.len() as u32).collect(),
            stale_nodes: 0,
            delta_slots: Vec::new(),
            delta_vectors: Vec::new(),
            visible_mask,
            visible_in_index,
            mask_splits_graph: false,
            regime: TraversalRegime::OneHop,
        });
        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(crate::hnsw::ValidatedHnswParams::default()),
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        record_overlay_for(&access, table0, base0, overlay);

        assert_eq!(
            cache.stats().entries,
            MAX_HNSW_CACHE_ENTRIES,
            "overlay 更新だけではエントリ数が変わらないため、他エントリを巻き添えで \
             追い出してはならない"
        );
    }

    #[test]
    fn mark_uncacheable_in_guard_causes_lookup_to_report_build_failed_this_generation() {
        // Cursor Bugbot 指摘対応（PR #434・"Uncacheable HNSW rebuilt every query"）:
        // `record_base` が単体で `MAX_HNSW_CACHE_TOTAL_BYTES` を超えるベースを
        // 拒否する経路（`mark_uncacheable_in_guard` 呼び出し）と、既存の
        // `record_build_failed`（構築失敗）は同じ負のキャッシュ機構を共有する
        // （リファクタで抽出済み）。実際に 1GiB 超のベースを構築する重量級テストの
        // 代わりに、共有機構そのものを直接検証する: 一度 `mark_uncacheable_in_guard`
        // で「この世代はキャッシュ不可」と記録すれば、以後同一世代内の `lookup` は
        // 毎回 `BuildFailedThisGeneration` を返し、`search_or_fallback` は
        // `IndexedBase::build`（HNSW グラフ全体の構築）を再試行しない
        // （呼び出し元は `full_scan` へ直行する）。
        let path = unique_db_path("hnsw-cache-mark-uncacheable");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();

        // まだエントリが無い状態からの記録（初回構築成功時点で単体超過と判明した
        // ケース、または一度も構築成功していないテーブルでの初回失敗ケースに対応）。
        {
            let mut guard = cache.state.write().unwrap();
            mark_uncacheable_in_guard(&cache, &mut guard, "docs", &c, gen);
        }
        assert_eq!(cache.stats().entries, 1);

        let read_txn2 = storage.db().begin_read().unwrap();
        match cache.lookup(&storage, &read_txn2, "docs", &c) {
            Lookup::BuildFailedThisGeneration => {}
            _ => panic!(
                "同一世代内の lookup は BuildFailedThisGeneration を返し、\
                 毎クエリのフル再構築を避けなければならない"
            ),
        }

        // 世代が進めば負のキャッシュは新しい世代には適用されず、再挑戦できる
        // （`docs/design/hnsw-generation-cache.md`「構築失敗時の負のキャッシュ」節）。
        let write_txn = storage.db().begin_write().unwrap();
        crate::catalog::bump_table_generation_in_txn(&write_txn, "docs").unwrap();
        crate::recovery::commit_boundary::commit(write_txn).unwrap();
        let read_txn3 = storage.db().begin_read().unwrap();
        match cache.lookup(&storage, &read_txn3, "docs", &c) {
            Lookup::Miss => {}
            _ => panic!("世代が進んだ後は再構築を試みられるよう Miss を返すべき"),
        }
    }

    #[test]
    fn arena_identity_mismatch_guard_detects_dim_and_ctx_mismatch() {
        // codex-review P2 指摘対応（PR #434「アリーナ不整合ガードが常に無効」）:
        // 旧実装は引数を無視して常に `false` を返し、呼び出し元の fail-closed
        // 分岐に決して到達しなかった。dim 不一致（別テーブル取り違え）・
        // `PolicyContext` 不一致（別テナント取り違え）のいずれも検出することを
        // 固定する。
        let path = unique_db_path("hnsw-cache-arena-identity-guard");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs4", 4);
        create_table(&storage, "docs8", 8);
        let c_a = ctx("tenant-a");
        let c_b = ctx("tenant-b");

        let read_txn = storage.db().begin_read().unwrap();
        let gen4 = crate::catalog::table_generation_in_txn(&read_txn, "docs4").unwrap();
        let arena4 = build_arena(&read_txn, "docs4", &c_a);
        let base = IndexedBase::build(
            &arena4,
            crate::hnsw::HnswParams::default(),
            crate::hnsw::ResidentPrecision::F32,
            c_a.clone(),
            gen4,
        )
        .expect("build")
        .0;

        // dim・ctx とも本ベースの構築時と一致: 検証通過（fail-closed 分岐に入らない）。
        assert!(!base.arena_identity_mismatch_guard(&arena4, &c_a));

        // dim 不一致（別テーブル由来のアリーナ取り違え）。
        let arena8 = build_arena(&read_txn, "docs8", &c_a);
        assert!(base.arena_identity_mismatch_guard(&arena8, &c_a));

        // ctx 不一致（別テナント取り違え）。
        assert!(base.arena_identity_mismatch_guard(&arena4, &c_b));
    }

    #[test]
    fn builds_stat_counts_failed_build_attempts() {
        // codex-review P2 指摘対応（PR #434「builds が失敗した構築試行を計上
        // しない」）: 旧実装は `record_base`（構築成功後）でのみ `builds` を
        // 加算しており、`IndexedBase::build` が `HnswError` で失敗した場合は
        // `build_failures` のみ計上され `builds` は増えなかった。NaN
        // embedding を含む `MIN_INDEXED_ROWS` 件のコーパスで構築を確実に
        // 失敗させ、失敗試行も `builds` に計上されることを固定する
        // （`search_or_fallback` 自体は brute-force へ縮退し `Ok` を返す）。
        let path = unique_db_path("hnsw-cache-builds-count-failure");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let c = ctx("tenant-a");

        let embeddings: Vec<[f32; 4]> = (0..MIN_INDEXED_ROWS)
            .map(|i| {
                if i == 0 {
                    [f32::NAN, 0.0, 0.0, 0.0]
                } else {
                    [i as f32, 0.0, 0.0, 0.0]
                }
            })
            .collect();
        let rows: Vec<(u64, RowInput<'_>)> = embeddings
            .iter()
            .enumerate()
            .map(|(i, e)| {
                (
                    i as u64,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: e.as_slice(),
                        metadata: &[],
                    },
                )
            })
            .collect();
        let op_id =
            crate::recovery::required_op_id::OperationId::parse("hnsw-cache-builds-count-failure")
                .expect("valid operation_id");
        crate::tenant::insert_rows(&storage, "docs", &c, &rows, &op_id).expect("bulk insert");

        let read_txn = storage.db().begin_read().unwrap();
        let arena = build_arena(&read_txn, "docs", &c);
        assert!(arena.len() >= MIN_INDEXED_ROWS);
        let slot_ids: Vec<u64> = (0..arena.len() as u64).collect();

        let cache = HnswIndexCache::new();
        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(crate::hnsw::ValidatedHnswParams::default()),
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        let provider = crate::kernel::CpuScalarProvider;
        let query = [1.0, 0.0, 0.0, 0.0];
        let result = search_or_fallback(
            &access, &read_txn, "docs", &c, &arena, &slot_ids, &provider, &query, 1,
        );
        assert!(
            result.is_ok(),
            "NaN 混入コーパスは構築失敗後 brute-force へ縮退し、クエリ自体は成功する"
        );

        let stats = cache.stats();
        assert_eq!(
            stats.builds, 1,
            "失敗した構築試行も builds に計上されるべき（旧実装は 0 のままだった）"
        );
        assert_eq!(stats.build_failures, 1);
    }

    /// 恒等マスク（warm `FullVisible`。フィルタなし DISTANCE で可視行全体が
    /// そのままアリーナになる形状）＋ `repair_reachability` 済みグラフでは、
    /// [`crate::hnsw::HnswIndex::is_mask_fully_reachable`]（`Overlay::compute`
    /// が世代毎に 1 回だけ呼ぶ分断検査）が「分断なし」と判定し、`search_masked`
    /// によるマスク付き ANN 探索がクエリ毎に実際に使われる（`masked_short`・
    /// `mask_splits_graph` いずれにも計上されず plain scan へ縮退しない）ことを
    /// 固定する（Cursor Bugbot High／codex-review P2 指摘対応・PR #435）。
    ///
    /// 1 回目のクエリは `Lookup::Miss` から新規構築（`Overlay::compute` 経由の
    /// 恒等オーバーレイ。`search_or_fallback` の `NeedOverlay` 相当分岐）、
    /// 2 回目は `Lookup::Ready`（`hits` 計上経路）を通し、いずれも ANN 経路が
    /// 使われることを確認する——このテストが red になる場合、
    /// `repair_reachability` は層 0 の entry point 起点到達性を保証していない
    /// ことになる（本関数末尾のコメント参照）。
    #[test]
    fn is_mask_fully_reachable_accepts_identity_mask_on_a_repaired_graph_and_ann_path_is_used() {
        let path = unique_db_path("hnsw-cache-identity-mask-ann-path");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let c = ctx("tenant-a");

        // MIN_INDEXED_ROWS 件・複数層が生じ得る規模のコーパス（HNSW_BUILD_SEED
        // に基づく決定的なレベル割当で、この件数なら通常複数層になる）。
        let embeddings: Vec<[f32; 4]> = (0..MIN_INDEXED_ROWS)
            .map(|i| {
                [
                    (i as f32) * 0.001,
                    ((i * 7) % 997) as f32 * 0.001,
                    ((i * 13) % 991) as f32 * 0.001,
                    ((i * 29) % 983) as f32 * 0.001,
                ]
            })
            .collect();
        let rows: Vec<(u64, RowInput<'_>)> = embeddings
            .iter()
            .enumerate()
            .map(|(i, e)| {
                (
                    i as u64,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: e.as_slice(),
                        metadata: &[],
                    },
                )
            })
            .collect();
        let op_id =
            crate::recovery::required_op_id::OperationId::parse("hnsw-cache-identity-mask-ann")
                .expect("valid operation_id");
        crate::tenant::insert_rows(&storage, "docs", &c, &rows, &op_id).expect("bulk insert");

        let read_txn = storage.db().begin_read().unwrap();
        let arena = build_arena(&read_txn, "docs", &c);
        assert!(arena.len() >= MIN_INDEXED_ROWS);
        let slot_ids: Vec<u64> = (0..arena.len() as u64).collect();

        let cache = HnswIndexCache::new();
        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(crate::hnsw::ValidatedHnswParams::default()),
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        let provider = crate::kernel::CpuScalarProvider;
        let query = [0.5, 0.25, 0.1, 0.9];

        let first = search_or_fallback(
            &access, &read_txn, "docs", &c, &arena, &slot_ids, &provider, &query, 10,
        );
        assert!(
            first.is_ok(),
            "warm-up query (Miss -> build) should succeed"
        );

        let second = search_or_fallback(
            &access, &read_txn, "docs", &c, &arena, &slot_ids, &provider, &query, 10,
        );
        assert!(second.is_ok(), "second query (Ready) should succeed");

        let stats = cache.stats();
        assert!(
            stats.hits >= 1,
            "identity mask must be judged fully reachable (mask_splits_graph == \
             false) so the Ready-path query reaches the ANN search_masked call \
             instead of degrading to plain scan; if this fails, \
             repair_reachability does not guarantee level-0 entry-point-anchored \
             reachability and the graph-split detection must descend through \
             levels instead of a level-0-only BFS"
        );
        assert_eq!(
            stats.mask_splits_graph, 0,
            "identity mask on a repaired graph must not be reported as split"
        );
        assert_eq!(
            stats.masked_short, 0,
            "ANN search must complete without falling short on this corpus"
        );
    }

    /// Issue #514（T-C1〜T-C2）: F16 常駐 opt-in（`ValidatedHnswParams::
    /// with_resident_precision(F16)`）で `IndexedBase::build` が実際に索引
    /// 探索へ到達し（`hits > 0`。非 vacuous）、`f16_residency_fallbacks == 0`
    /// （この範囲内の埋め込み値では自動縮退が起きない）ことを確認する。加えて
    /// 各ヒットのスコアが既定エンジン（`CpuScalarProvider`。f32 brute-force）の
    /// 同 id のスコアと `to_bits()` 一致すること（R3・#408 契約: 索引ヒットの
    /// 最終スコアは常に `kernel::dot` による f32 アリーナ再計算）を固定する。
    #[test]
    fn f16_resident_precision_hits_the_ann_path_and_matches_default_engine_scores_exactly() {
        let path = unique_db_path("hnsw-cache-f16-resident");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let c = ctx("tenant-a");

        let embeddings: Vec<[f32; 4]> = (0..MIN_INDEXED_ROWS)
            .map(|i| {
                [
                    (i as f32) * 0.001,
                    ((i * 7) % 997) as f32 * 0.001,
                    ((i * 13) % 991) as f32 * 0.001,
                    ((i * 29) % 983) as f32 * 0.001,
                ]
            })
            .collect();
        let rows: Vec<(u64, RowInput<'_>)> = embeddings
            .iter()
            .enumerate()
            .map(|(i, e)| {
                (
                    i as u64,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: e.as_slice(),
                        metadata: &[],
                    },
                )
            })
            .collect();
        let op_id = crate::recovery::required_op_id::OperationId::parse("hnsw-cache-f16-resident")
            .expect("valid operation_id");
        crate::tenant::insert_rows(&storage, "docs", &c, &rows, &op_id).expect("bulk insert");

        let read_txn = storage.db().begin_read().unwrap();
        let arena = build_arena(&read_txn, "docs", &c);
        assert!(arena.len() >= MIN_INDEXED_ROWS);
        let slot_ids: Vec<u64> = (0..arena.len() as u64).collect();

        let cache = HnswIndexCache::new();
        let f16_params = crate::hnsw::ValidatedHnswParams::default()
            .with_resident_precision(crate::hnsw::ResidentPrecision::F16);
        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(f16_params),
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        let ann_provider = crate::kernel::CpuScalarProvider;
        let default_provider = crate::kernel::CpuScalarProvider;
        let query = [0.5, 0.25, 0.1, 0.9];

        // 1 回目（Miss -> build）・2 回目（Ready）の双方が成功すること。
        let first = search_or_fallback(
            &access,
            &read_txn,
            "docs",
            &c,
            &arena,
            &slot_ids,
            &ann_provider,
            &query,
            10,
        )
        .expect("warm-up query (Miss -> build) should succeed");
        let ann_hits = search_or_fallback(
            &access,
            &read_txn,
            "docs",
            &c,
            &arena,
            &slot_ids,
            &ann_provider,
            &query,
            10,
        )
        .expect("second query (Ready) should succeed");
        assert!(!first.is_empty());
        assert!(!ann_hits.is_empty(), "non-vacuous: must return hits");

        let stats = cache.stats();
        assert!(
            stats.hits >= 1,
            "F16 resident precision must still reach the indexed ANN search path \
             (Ready + non-degraded), got stats={stats:?}"
        );
        assert_eq!(
            stats.f16_residency_fallbacks, 0,
            "embeddings in this fixture are well within the f16 finite range and \
             must not trigger the F32 fallback (D6)"
        );

        // R3: 既定エンジン（f32 brute-force）と同 id のスコアが完全一致する
        // こと（索引ヒットの最終スコアは常に f32 アリーナ再計算のため、f16
        // 常駐の候補生成は探索順序にのみ影響し、返るスコア自体は変えない）。
        let baseline = default_provider
            .search(crate::kernel::SearchInput {
                ids: &slot_ids,
                vectors: arena.vectors(),
                dim: arena.dim(),
                query: &query,
                k: 10,
            })
            .expect("baseline search must succeed");
        let baseline_by_id: HashMap<u64, f32> =
            baseline.into_iter().map(|h| (h.id, h.score)).collect();
        for hit in &ann_hits {
            if let Some(&expected) = baseline_by_id.get(&hit.id) {
                assert_eq!(
                    hit.score.to_bits(),
                    expected.to_bits(),
                    "id={} ann_score={} baseline_score={}",
                    hit.id,
                    hit.score,
                    expected
                );
            }
        }
    }

    /// Issue #521: I8（SQ8）常駐 opt-in（`ValidatedHnswParams::
    /// with_resident_precision(I8)`）で `IndexedBase::build` が実際に索引
    /// 探索へ到達し（非 vacuous）、`i8_residency_fallbacks == 0` であることを
    /// 確認する。加えて各ヒットのスコアが既定エンジン（f32 brute-force）の
    /// 同 id のスコアと `to_bits()` 一致すること（索引ヒットの最終スコアは
    /// 常に `kernel::dot` による f32 アリーナ再計算）を固定する
    /// （f16 版 `f16_resident_precision_hits_the_ann_path_and_matches_
    /// default_engine_scores_exactly` と同型）。
    #[test]
    fn i8_resident_precision_hits_the_ann_path_and_matches_default_engine_scores_exactly() {
        let path = unique_db_path("hnsw-cache-i8-resident");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let c = ctx("tenant-a");

        let embeddings: Vec<[f32; 4]> = (0..MIN_INDEXED_ROWS)
            .map(|i| {
                [
                    (i as f32) * 0.001,
                    ((i * 7) % 997) as f32 * 0.001,
                    ((i * 13) % 991) as f32 * 0.001,
                    ((i * 29) % 983) as f32 * 0.001,
                ]
            })
            .collect();
        let rows: Vec<(u64, RowInput<'_>)> = embeddings
            .iter()
            .enumerate()
            .map(|(i, e)| {
                (
                    i as u64,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: e.as_slice(),
                        metadata: &[],
                    },
                )
            })
            .collect();
        let op_id = crate::recovery::required_op_id::OperationId::parse("hnsw-cache-i8-resident")
            .expect("valid operation_id");
        crate::tenant::insert_rows(&storage, "docs", &c, &rows, &op_id).expect("bulk insert");

        let read_txn = storage.db().begin_read().unwrap();
        let arena = build_arena(&read_txn, "docs", &c);
        assert!(arena.len() >= MIN_INDEXED_ROWS);
        let slot_ids: Vec<u64> = (0..arena.len() as u64).collect();

        let cache = HnswIndexCache::new();
        let i8_params = crate::hnsw::ValidatedHnswParams::default()
            .with_resident_precision(crate::hnsw::ResidentPrecision::I8);
        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(i8_params),
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        let ann_provider = crate::kernel::CpuScalarProvider;
        let default_provider = crate::kernel::CpuScalarProvider;
        let query = [0.5, 0.25, 0.1, 0.9];

        // 1 回目（Miss -> build）・2 回目（Ready）の双方が成功すること。
        let first = search_or_fallback(
            &access,
            &read_txn,
            "docs",
            &c,
            &arena,
            &slot_ids,
            &ann_provider,
            &query,
            10,
        )
        .expect("warm-up query (Miss -> build) should succeed");
        let ann_hits = search_or_fallback(
            &access,
            &read_txn,
            "docs",
            &c,
            &arena,
            &slot_ids,
            &ann_provider,
            &query,
            10,
        )
        .expect("second query (Ready) should succeed");
        assert!(!first.is_empty());
        assert!(!ann_hits.is_empty(), "non-vacuous: must return hits");

        let stats = cache.stats();
        assert!(
            stats.hits >= 1,
            "I8 resident precision must still reach the indexed ANN search path \
             (Ready + non-degraded), got stats={stats:?}"
        );
        assert_eq!(
            stats.i8_residency_fallbacks, 0,
            "embeddings in this fixture are finite and must not trigger the F32 \
             fallback (D6 と同型。Issue #521)"
        );

        // 索引ヒットの最終スコアは常に f32 アリーナ再計算のため、i8 常駐の
        // 候補生成は探索順序にのみ影響し、返るスコア自体は既定エンジンと
        // ビット一致する。k=10 baseline search を id 突き合わせに使うと、
        // ANN が（近似探索ゆえに）その baseline の上位 10 件に含まれない
        // id を返した場合に検証が黙ってスキップされてしまう（PR #617
        // codex-review P2 指摘）。`slot_ids` は `0..arena.len()` の恒等
        // 写像（id == arena 上の行インデックス）であることを利用し、
        // 各 ANN ヒットの期待スコアを baseline の Top-k 集合に頼らず
        // `arena.vector(id)` から `kernel::dot` で直接算出することで、
        // 全ヒットが必ず検証される（ID 不在は arena 不変条件違反として
        // 即座に panic）。`default_provider` を使った k=10 baseline
        // search 自体は非 vacuous 性（結果が空でないこと）の確認にのみ残す。
        let baseline = default_provider
            .search(crate::kernel::SearchInput {
                ids: &slot_ids,
                vectors: arena.vectors(),
                dim: arena.dim(),
                query: &query,
                k: 10,
            })
            .expect("baseline search must succeed");
        assert!(!baseline.is_empty(), "baseline search must be non-vacuous");
        for hit in &ann_hits {
            let expected_vector = arena
                .vector(hit.id as usize)
                .unwrap_or_else(|| panic!("ann hit id={} must exist in arena", hit.id));
            let expected = crate::kernel::dot(expected_vector, &query);
            assert_eq!(
                hit.score.to_bits(),
                expected.to_bits(),
                "id={} ann_score={} expected_score={}",
                hit.id,
                hit.score,
                expected
            );
        }
    }

    #[test]
    fn record_overlay_for_marks_uncacheable_when_oversized() {
        // Cursor Bugbot 指摘対応（PR #434「Oversized overlay skips negative
        // cache」）: `record_overlay_for` は base+overlay の合計が
        // `MAX_HNSW_CACHE_TOTAL_BYTES` を超える overlay を常駐させないだけで
        // 負のキャッシュへ記録していなかったため、次回 lookup が再び
        // `NeedOverlay` となり毎クエリ `Overlay::compute` を再実行してしまって
        // いた。`record_base`・`record_build_failed` と同じ負のキャッシュへ
        // 記録し、以後同一世代の lookup が `BuildFailedThisGeneration`
        // （overlay 再計算なし）を返すことを固定する。
        let path = unique_db_path("hnsw-cache-overlay-oversized");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let arena = build_arena(&read_txn, "docs", &c);
        let built = IndexedBase::build(
            &arena,
            crate::hnsw::HnswParams::default(),
            crate::hnsw::ResidentPrecision::F32,
            c.clone(),
            gen,
        )
        .expect("build")
        .0;
        let base = cache.record_base(&storage, "docs", built);
        assert_eq!(cache.stats().entries, 1);

        // `MAX_HNSW_CACHE_TOTAL_BYTES` を超える容量を持つ overlay を偽装する。
        // `Vec::with_capacity` は要素を書き込まない限り仮想メモリの予約に
        // 留まる（`len` は 0 のまま）ため、実メモリを消費せず `capacity()` の
        // みを大きくできる。
        let oversized_len = (MAX_HNSW_CACHE_TOTAL_BYTES / std::mem::size_of::<f32>()) + 1_000_000;
        let delta_vectors: Vec<f32> = Vec::with_capacity(oversized_len);
        let (visible_mask, visible_in_index) = identity_mask_and_visible(&base);
        let overlay = Arc::new(Overlay {
            generation: gen,
            arena_len: arena.len(),
            slot_of_node: (0..base.index.len() as u32).collect(),
            stale_nodes: 0,
            delta_slots: Vec::new(),
            delta_vectors,
            visible_mask,
            visible_in_index,
            mask_splits_graph: false,
            regime: TraversalRegime::OneHop,
        });
        assert!(overlay.approx_heap_bytes() > MAX_HNSW_CACHE_TOTAL_BYTES);

        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(crate::hnsw::ValidatedHnswParams::default()),
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        record_overlay_for(&access, "docs", &base, overlay);

        // 負のキャッシュ専用の新規エントリは追加しない（`record_base` が既に
        // 作った `(table, ctx)` のエントリへ `build_failed_generation` を
        // 上書きするだけ）。
        assert_eq!(cache.stats().entries, 1);

        let read_txn2 = storage.db().begin_read().unwrap();
        match cache.lookup(&storage, &read_txn2, "docs", &c) {
            Lookup::BuildFailedThisGeneration => {}
            _ => panic!(
                "oversized overlay を破棄した世代は BuildFailedThisGeneration を返し、\
                 毎クエリの Overlay::compute 再計算を避けなければならない"
            ),
        }
    }

    #[test]
    fn arena_identity_mismatch_guard_evicts_entry_and_recovers_ann_next_query() {
        // Cursor Bugbot 指摘対応（PR #434「Guard disables ANN without
        // recovery」）: `arena_identity_mismatch_guard` が検出した不整合な
        // `IndexedBase` を退避せずに brute-force へ縮退するだけだと、`lookup`
        // が同じエントリへ `NeedOverlay` を返し続け、以後の全クエリが同じ
        // ガードに当たり ANN 経路が再構築されないまま無効化され続ける。
        // dim 不整合の base を仕込んだ後、ガード経路を通った次のクエリでは
        // 再構築（builds 増加）され ANN 経路が復帰することを固定する。
        let path = unique_db_path("hnsw-cache-guard-recovery");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        create_table(&storage, "aux", 8);
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        // `docs`（dim=4）に索引化対象の行数を投入する。
        let embeddings: Vec<[f32; 4]> = (0..MIN_INDEXED_ROWS)
            .map(|i| [i as f32, 0.0, 0.0, 0.0])
            .collect();
        let rows: Vec<(u64, RowInput<'_>)> = embeddings
            .iter()
            .enumerate()
            .map(|(i, e)| {
                (
                    i as u64,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: e.as_slice(),
                        metadata: &[],
                    },
                )
            })
            .collect();
        let op_id =
            crate::recovery::required_op_id::OperationId::parse("hnsw-cache-guard-recovery")
                .expect("valid operation_id");
        crate::tenant::insert_rows(&storage, "docs", &c, &rows, &op_id).expect("bulk insert");

        // `aux`（dim=8）から構築した `IndexedBase` を、`docs` の現世代番号を
        // 偽って `(table="docs", ctx=c)` のエントリとして登録する。これは
        // 「呼び出し元の適用条件が正しく守られていれば起こらないはずの不整合」
        // （モジュールドキュメント参照）を意図的に再現するための細工であり、
        // `arena_identity_mismatch_guard` の fail-closed 検出そのものが本テストの
        // 対象。
        let read_txn = storage.db().begin_read().unwrap();
        let docs_gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let aux_arena = build_arena(&read_txn, "aux", &c);
        let mismatched_base = IndexedBase::build(
            &aux_arena,
            crate::hnsw::HnswParams::default(),
            crate::hnsw::ResidentPrecision::F32,
            c.clone(),
            docs_gen,
        )
        .expect("build mismatched base")
        .0;
        cache.record_base(&storage, "docs", mismatched_base);
        assert_eq!(
            cache.stats().entries,
            1,
            "細工した不整合エントリが登録される"
        );

        let docs_arena = build_arena(&read_txn, "docs", &c);
        assert!(docs_arena.len() >= MIN_INDEXED_ROWS);
        assert_ne!(
            docs_arena.dim(),
            8,
            "docs アリーナと細工した base の次元が不整合であることが前提"
        );
        let slot_ids: Vec<u64> = (0..docs_arena.len() as u64).collect();

        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(crate::hnsw::ValidatedHnswParams::default()),
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        let provider = crate::kernel::CpuScalarProvider;
        let query = [1.0, 0.0, 0.0, 0.0];

        // 1 回目: ガードが不整合を検出し brute-force へ縮退する。このとき
        // 不整合エントリはキャッシュから退避され、`builds` は増加しない
        // （新規構築はまだ試みていない）。
        let first = search_or_fallback(
            &access,
            &read_txn,
            "docs",
            &c,
            &docs_arena,
            &slot_ids,
            &provider,
            &query,
            1,
        );
        assert!(first.is_ok(), "ガード経路でも brute-force で成功応答を返す");
        assert_eq!(
            cache.stats().entries,
            0,
            "不整合エントリはガード検出時にキャッシュから退避される"
        );
        assert_eq!(
            cache.stats().builds,
            0,
            "ガード検出のみでは新規構築を試みない"
        );

        // 2 回目: エントリが退避済みのため `lookup` は `Miss` を返し、正しい
        // dim（4）で新規構築される（ANN 経路の再構築発火）。
        let second = search_or_fallback(
            &access,
            &read_txn,
            "docs",
            &c,
            &docs_arena,
            &slot_ids,
            &provider,
            &query,
            1,
        );
        assert!(second.is_ok());
        let stats_after_rebuild = cache.stats();
        assert_eq!(
            stats_after_rebuild.builds, 1,
            "退避後の次クエリで再構築が発火する（builds 増加）"
        );
        assert_eq!(
            stats_after_rebuild.entries, 1,
            "正しい dim の base が再登録される"
        );

        // 3 回目: 再構築済みの正しい base に対し、現世代のオーバーレイが
        // ない状態から `NeedOverlay` → `Ready` へ到達し、ガードは再発火せず
        // ANN 経路（索引探索）が復帰していることを確認する。
        let third = search_or_fallback(
            &access,
            &read_txn,
            "docs",
            &c,
            &docs_arena,
            &slot_ids,
            &provider,
            &query,
            1,
        );
        assert!(third.is_ok());
        let stats_final = cache.stats();
        assert_eq!(
            stats_final.builds, 1,
            "3 回目は既存の正しい base を再利用し、再構築は発生しない"
        );
        assert!(
            stats_final.hits >= 1,
            "ANN 経路（索引探索・縮退なし）が復帰していることを固定する"
        );
    }

    /// [`prepare_subset`] の早期打ち切り（Issue #488）を、統合テスト
    /// （`crates/engine/tests/hnsw_cache.rs` の
    /// `full_scan_ratio_plain_scan_below_ratio_subset_shape_matches_brute_force_and_never_leaks_across_tenants`）
    /// が観測する `subset_searches == 0` とは別の経路で直接固定する
    /// （codex-review P2 指摘対応・PR #606）。
    ///
    /// `subset_searches == 0` は `Overlay::compute` を経由する旧来のフォール
    /// バック（`prepare_subset` に早期打ち切りを実装せず、`Overlay::compute`
    /// → `below_full_scan_ratio` 判定 → `search_with_overlay` 内で plain scan
    /// を選ぶ経路）でも成立してしまい、`plain_scans > 0` と組み合わせても
    /// 「`Overlay::compute` そのものを省略した」ことの証拠にはならない。本
    /// テストは `prepare_subset` の戻り値（[`PreparedHnswSearch`]）を
    /// `matches!` で直接検査し、早期打ち切り固有の分岐（`Overlay::compute`
    /// 呼び出し前に確定する `PlainScanBelowRatio`）を実際に踏んだことを
    /// 固定する。対称条件（比が `full_scan_ratio` 以上）では同じ関数が
    /// `Overlay::compute` を経由する `Indexed` を返すことも併せて確認し、
    /// 分岐が比の大小に応じて実際に切り替わっていることを示す（早期打ち切り
    /// が常に発火する実装の誤りではないことの検査）。
    #[test]
    fn prepare_subset_returns_plain_scan_below_ratio_directly_without_computing_overlay() {
        let path = unique_db_path("hnsw-cache-prepare-subset-early-cutoff");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        let cache = HnswIndexCache::new();
        let c = ctx("tenant-a");

        // 索引済みベース（20 ノード）を用意する。`prepare_subset` は
        // `Lookup::Miss` では常に `FullScan` へ縮退する契約（§`prepare_subset`
        // ドキュメンテーションコメント「2.」）のため、`IndexedBase::build` で
        // 直接構築してキャッシュへ登録し（`MIN_INDEXED_ROWS` は
        // `prepare_full_visible` 側の下限であり、ここでは無関係）、
        // `prepare_subset` からは常に `Lookup::Ready`／`NeedOverlay` として
        // 見える状態にする。
        const N: u64 = 20;
        let embeddings: Vec<[f32; 4]> = (0..N).map(|i| [i as f32, 0.0, 0.0, 0.0]).collect();
        let rows: Vec<(u64, RowInput<'_>)> = embeddings
            .iter()
            .enumerate()
            .map(|(i, e)| {
                (
                    i as u64,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: e.as_slice(),
                        metadata: &[],
                    },
                )
            })
            .collect();
        let op_id = crate::recovery::required_op_id::OperationId::parse(
            "hnsw-cache-prepare-subset-early-cutoff",
        )
        .expect("valid operation_id");
        crate::tenant::insert_rows(&storage, "docs", &c, &rows, &op_id).expect("bulk insert");

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let full_arena = build_arena(&read_txn, "docs", &c);
        assert_eq!(full_arena.len(), N as usize);
        let built = IndexedBase::build(
            &full_arena,
            crate::hnsw::HnswParams::default(),
            crate::hnsw::ResidentPrecision::F32,
            c.clone(),
            gen,
        )
        .expect("build base")
        .0;
        assert_eq!(built.index.len(), N as usize);
        cache.record_base(&storage, "docs", built);

        // `full_scan_ratio` を 60/100 に固定する。`tag` 列は無いためスカラー
        // フィルタは `on_visible_row` の細工で模擬する（`prepare_subset` は
        // 渡されたアリーナが WHERE 適用後の部分集合であることだけを前提に
        // しており、実際のスカラーフィルタ経路を通す必要はない）。
        let ratio = Ratio {
            numerator: 60,
            denominator: 100,
        };
        let params = crate::hnsw::ValidatedHnswParams::new(crate::hnsw::HnswParams::default())
            .expect("valid hnsw params")
            .with_full_scan_ratio(ratio)
            .expect("valid full_scan_ratio");
        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(params),
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };

        // 5/20 = 0.25 < 0.6: 早期打ち切り分岐を踏むはずの部分集合アリーナ
        // （`Overlay::compute` を経由せず `PlainScanBelowRatio` を直接返す）。
        let below_arena = VectorArena::build_filtered_with_rows_in_txn(
            &read_txn,
            "docs",
            crate::rls::ImplicitRlsHook::new(&c).predicate(),
            |_, id, _, _| Ok(id < 5),
        )
        .expect("build below-ratio subset arena");
        assert_eq!(below_arena.len(), 5);
        let below_prepared = prepare_subset(&access, &read_txn, "docs", &c, &below_arena);
        assert!(
            matches!(below_prepared, PreparedHnswSearch::PlainScanBelowRatio),
            "arena.len() / index.len() = 0.25 < full_scan_ratio = 0.6 は \
             Overlay::compute を経由せず PlainScanBelowRatio を直接返すはず"
        );

        // 19/20 = 0.95 >= 0.6: 対称条件（早期打ち切りを踏まない）。同じ
        // `prepare_subset` が `Overlay::compute` を経由する `Indexed` を返す
        // ことを確認し、分岐が比の大小に応じて実際に切り替わっていることを
        // 示す。
        let above_arena = VectorArena::build_filtered_with_rows_in_txn(
            &read_txn,
            "docs",
            crate::rls::ImplicitRlsHook::new(&c).predicate(),
            |_, id, _, _| Ok(id < 19),
        )
        .expect("build at-or-above-ratio subset arena");
        assert_eq!(above_arena.len(), 19);
        let above_prepared = prepare_subset(&access, &read_txn, "docs", &c, &above_arena);
        assert!(
            matches!(above_prepared, PreparedHnswSearch::Indexed { .. }),
            "arena.len() / index.len() = 0.95 >= full_scan_ratio = 0.6 では \
             早期打ち切りを踏まず Overlay::compute 経由の Indexed を返すはず"
        );
    }

    /// Issue #505: `search_prepared_resumable` を同一クエリ・`k` 倍増で
    /// 複数ラウンド呼ぶと 2 ラウンド目以降が破棄候補ヒープ保持型の再開型探索を
    /// 通り、`hybrid_resumed_rounds` が非 vacuous に計上され `builds` は増えず、
    /// 各ラウンドの返却が `len <= k`・score 降順・id 昇順・可視集合内・一意で
    /// あることを固定する。
    #[test]
    fn search_prepared_resumable_reuses_graph_scan_across_rounds() {
        let path = unique_db_path("hnsw-cache-resumable-rounds");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        for i in 0..2_000u64 {
            let v = i as f32;
            seed_row(
                &storage,
                "docs",
                i,
                "tenant-a",
                &[v, v + 1.0, v + 2.0, v + 3.0],
            );
        }
        let c = ctx("tenant-a");
        let cache = HnswIndexCache::new();
        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(crate::hnsw::ValidatedHnswParams::default()),
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        let read_txn = storage.db().begin_read().unwrap();
        let arena = build_arena(&read_txn, "docs", &c);
        let slot_ids: Vec<u64> = (0..arena.len() as u64).collect();
        let read_txn = storage.db().begin_read().unwrap();
        let prepared = prepare_full_visible(&access, &read_txn, "docs", &c, &arena);
        let builds_after_prepare = cache.stats().builds;

        let provider = crate::kernel::CpuScalarProvider;
        let mut resume: Option<HnswResumeState> = None;
        let query = [0.0f32, 1.0, 2.0, 3.0];
        for k in [10usize, 20, 40] {
            let hits = search_prepared_resumable(
                &access,
                &prepared,
                &provider,
                &arena,
                &slot_ids,
                &query,
                k,
                &mut resume,
            )
            .expect("resumable search succeeds");
            assert!(hits.len() <= k);
            assert!(hits.windows(2).all(
                |w| w[0].score >= w[1].score && (w[0].score > w[1].score || w[0].id < w[1].id)
            ));
            let mut ids: Vec<u64> = hits.iter().map(|h| h.id).collect();
            let dedup_len = {
                ids.sort_unstable();
                ids.dedup();
                ids.len()
            };
            assert_eq!(dedup_len, hits.len(), "hits must be unique by id");
            for h in &hits {
                assert!((h.id as usize) < arena.len());
            }
        }

        let stats = cache.stats();
        assert_eq!(stats.builds, builds_after_prepare, "prepare must run once");
        assert!(
            stats.hybrid_resumed_rounds >= 1,
            "at least the 2nd/3rd rounds should reuse the previous graph scan"
        );
    }

    /// Issue #505: `k > MAX_EF` のラウンドは再開型探索を試みず状態を破棄する
    /// （`search_prepared` の既存 `ef_cap_fallbacks` 縮退契約と同じ挙動）。
    #[test]
    fn search_prepared_resumable_resets_state_when_k_exceeds_max_ef() {
        let path = unique_db_path("hnsw-cache-resumable-ef-cap");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs", 4);
        for i in 0..2_000u64 {
            let v = i as f32;
            seed_row(
                &storage,
                "docs",
                i,
                "tenant-a",
                &[v, v + 1.0, v + 2.0, v + 3.0],
            );
        }
        let c = ctx("tenant-a");
        let cache = HnswIndexCache::new();
        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: HnswSearchProvider::new(crate::hnsw::ValidatedHnswParams::default()),
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        let read_txn = storage.db().begin_read().unwrap();
        let arena = build_arena(&read_txn, "docs", &c);
        let slot_ids: Vec<u64> = (0..arena.len() as u64).collect();
        let read_txn = storage.db().begin_read().unwrap();
        let prepared = prepare_full_visible(&access, &read_txn, "docs", &c, &arena);

        let provider = crate::kernel::CpuScalarProvider;
        let mut resume: Option<HnswResumeState> = None;
        let query = [0.0f32, 1.0, 2.0, 3.0];
        let over_max_ef = crate::hnsw::MAX_EF + 1;
        let hits = search_prepared_resumable(
            &access,
            &prepared,
            &provider,
            &arena,
            &slot_ids,
            &query,
            over_max_ef,
            &mut resume,
        )
        .expect("full scan succeeds");
        assert_eq!(hits.len(), arena.len().min(over_max_ef));
        assert!(
            resume.is_none(),
            "ef_cap_fallbacks must discard resume state"
        );
        assert_eq!(cache.stats().ef_cap_fallbacks, 1);
        assert_eq!(cache.stats().hybrid_resumed_rounds, 0);
    }
}
