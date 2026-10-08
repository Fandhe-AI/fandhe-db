//! HNSW（Hierarchical Navigable Small World）グラフの構築（TASK-132・対象ビヘイビア:
//! CORE-9・CORE-10。ポインタ: `docs/design/ann-index-adoption.md`「実装ガイド（B 案）」節）。
//!
//! 本モジュールの範囲は**グラフ構築（Algorithm 1〜4 相当）＋ ef-探索 top-k 検索
//! （Algorithm 5 相当。[`HnswIndex::search`]。#405 で追加）＋ 並列構築（#406）＋
//! `search_engine.rs` への `SearchEngineKind::Hnsw` 結線（[`provider`] サブモジュール、
//! #407）**。世代整合キャッシュ・RLS 統合・永続化はいずれも別タスク（#408〜#409）の
//! 担当であり、本モジュールは触れない（`provider` は本タスク時点、索引を保持せず
//! 全件 brute-force フォールバックする——詳細は `provider` モジュールドキュメント
//! 参照）。ADR（`docs/design/ann-index-adoption.md`）の「非契約的な実装詳細」区分に
//! 基づき、spec 側の確定を待たずに着手している。
//!
//! # ベクトルの所有方針（codex-review PR #430 P1 指摘への対応で変更）
//!
//! [`HnswIndex::build`] は `&[f32]`（`arena.rs::VectorArena` 等が所有する
//! row-major 連続バッファ）を借用してグラフ（隣接リスト・レベル・
//! エントリポイント）を構築するが、構築完了時にその内容を `Arc<[f32]>`
//! として 1 回だけコピーし [`HnswIndex`] 自身に**所有**させる。
//! [`HnswIndex::search`] はもはや `vectors` を引数に取らず、常にこの
//! 内部スナップショットを参照する——旧設計（呼び出し元がビルド後も
//! バッファを所有し続け、`search` へ毎回 `&[f32]` で貸し出す）では、
//! 呼び出し元が構築後にバッファを書き換える・行順を入れ替える・別の
//! バッファへ差し替えるといった事故を `search` 側が検出できるかは
//! 長さ照合とサンプリングに依存し、サンプリング対象外の位置への
//! 書き換えは正常入力として静かに受理されグラフと `vectors` の対応が
//! 崩れたまま誤った top-k を返しかねなかった（初出時の対応:
//! サンプリング・フィンガープリント照合。指摘: そのサンプリング自体が
//! 検出漏れの余地を残す）。`HnswIndex` が唯一の正本を所有する設計へ
//! 変更したことで、`search` に「別バッファが渡される」という入力の
//! クラス自体が存在しなくなり、照合ロジックなしに構造的に防げる。
//!
//! この設計変更により `build` 呼び出し 1 回あたり `n * dim * 4` バイトの
//! 追加コピーが恒久的に発生する（768 次元 × 100 万行で 3 GB 級。旧設計が
//! 避けていたコスト）。トレードオフとして受け入れた判断であり、#408 の
//! 世代整合キャッシュ・#406 の並列構築で `VectorArena` 側が最初から
//! `Arc<[f32]>` を持つ構成に変えられれば、このコピーは `Arc::clone`
//! （参照カウントの増分のみ）に縮退できる——`arena.rs::VectorArena` は
//! 本 Issue 時点で `vectors: Vec<f32>` のまま（`Arc` 化していない）ため、
//! この縮退は #408 側の設計課題として申し送る。
//!
//! # 距離カーネル
//!
//! `kernel::dot`（`isa.rs` の実行時検出 SIMD カーネルへの唯一の委譲経路）を使う。
//! スコアは内積で「大きいほど近い」（`kernel.rs::CandidateHit` と同じ規約。cosine
//! 距離は呼び出し元が正規化済みベクトルを渡すことで内積に一致させる既存契約を
//! 踏襲する）。ヒューリスティックの `d(e, q) < d(e, r)` は本モジュール内では
//! `dot(e, q) > dot(e, r)` と読み替える。
//!
//! # 順序規約
//!
//! 候補ヒープの同点タイブレークは `kernel.rs::MinHeapItem` と同じ「スコア
//! `total_cmp` 降順・同点は id（ノード番号）昇順」を踏襲する。`f32` は全順序を
//! 持たないため `total_cmp` を使い、非有限値は構築入力の段で拒否する
//! （[`HnswError::NonFiniteVector`]）ため探索段では常に有限値のみを比較する。
//! ソートは安定ソート（`sort_by`）のみを使う（`sort_unstable_by` 系は
//! `scripts/check_sort_determinism.sh` が禁止する）。入力ベクトルが有限
//! （`NonFiniteVector` 検証済み）でも、有限な大きな `f32` 同士の積・総和は
//! オーバーフロー（`Inf`）や `NaN` になり得るため、`dot` の呼び出し直後にも
//! 結果が有限か検証し、非有限なら `HnswError::NonFiniteScore` として拒否する
//! （codex-review #423 P1 指摘）。
//!
//! # レベル割当の決定性・非暗号 PRNG
//!
//! `assign_level` は本モジュール専用の xorshift64*（`benches/harness/rng.rs::
//! DeterministicRng` と同アルゴリズム。`src/` からは bench harness を参照できない
//! ため小さく複製する）を `seed` で初期化し、同一 `seed`・同一入力なら
//! 完全に同一のグラフを構築する。**非暗号 PRNG であり、鍵・トークン等の
//! セキュリティ用途に転用してはならない**（OWASP A02）。
//!
//! # 上限・untrusted 入力の扱い（coding-rust.md・security.md）
//!
//! `dim`・`vectors.len()` の整合、[`MAX_HNSW_NODES`]・[`MAX_LEVEL`] による上限、
//! `HnswParams::validate` による `m`・`ef_construction`・`ef_search` の上限
//! （[`MAX_M`]・[`MAX_EF`]）を構築前に検証する。オフセット計算は `checked_*`、
//! ノード id 変換は `u32::try_from` を使い、スライス添字は `get()` のみで
//! `unwrap`／`expect`／`[]` を使わない。`unsafe` は使わない。環境変数・feature flag
//! による経路上書きは設けない（CORE-12 踏襲）。

use std::collections::{BinaryHeap, HashSet, VecDeque};
use std::fmt;
use std::sync::Arc;

use crate::kernel::dot;

/// 凍結済みグラフの CSR（Compressed Sparse Row）表現（Issue #494）。
/// [`HnswIndex`] は構築完了後、可変長ビルダー表現（[`GraphBuilder`]）を
/// この表現へ 1 回だけ平坦化する（[`HnswIndex::freeze_from`] 参照）。
mod csr;
/// [`NodeVectors::I8`] 専用の per-search `NodeSource` 実装（Issue #522）。
/// `pub(super)` 限定で本モジュール外へは公開しない（[`prefetch`] と同じ方針）。
mod i8_query;
mod parallel_build;
/// `search_layer` の隣接ループへ挿入する受理判定後 prefetch（Issue #490）。
/// `pub(super)` 限定で本モジュール外へは公開しない。
mod prefetch;
/// `kernel.rs::SearchProvider` への結線（Issue #407・`search_engine.rs::
/// SearchEngineKind::Hnsw` の構築先）。本タスク時点は全件 brute-force
/// フォールバック（詳細は `provider` モジュールドキュメント参照）。
pub mod provider;
pub use provider::HnswSearchProvider;

/// 次数上限の安全上限（DoS 防止。`HnswParams::validate` が `m` をこの値以下に
/// 制限する）。
pub const MAX_M: usize = 128;

/// `ef_construction`／`ef_search` の上限（`core.rs::MAX_SEARCH_K` と同値。
/// untrusted な呼び出し元がここを起点に無制限の候補集合を要求できないようにする）。
pub const MAX_EF: usize = 10_000;

/// [`two_hop_effective_ef`] が `ef.max(k)` へ掛け合わせる倍率の上限（Issue
/// #680）。TwoHop（ACORN-1）が到達する既定レジームでは
/// `1/10 <= full_scan_ratio <= acorn_max_visible_ratio <= 4/10` 相当の
/// 可視比率（`node_count.div_ceil(visible)` がおおむね `3..=10`）を想定するが、
/// `--hnsw-full-scan-ratio`（Issue #657）で下限が外れる呼び出しでも `ef_eff`
/// が無制限に膨らまないよう固定上限で二重に抑える（`MAX_EF` によるクランプと
/// 独立の安全弁。DoS 防止）。値の根拠は
/// `docs/design/hnsw-rls-cardinality-switch.md`「Issue #680」節参照。
pub(crate) const ACORN_EF_SCALE_MAX: usize = 8;

/// 構築可能なノード数の上限（`arena::MAX_ARENA_ROWS` と同値。ノード id を `u32` で
/// 表現できることの裏付けでもある）。
pub const MAX_HNSW_NODES: usize = 1_000_000;

/// 層数の絶対上限（`assign_level` の結果をこの値でクランプする。理論上は対数
/// オーダーで極小確率の外れ値しか生じないが、上限を設けないと未検証の外れ値が
/// `Vec` のネストを無制限に増やしうるため fail-closed に固定する）。
pub const MAX_LEVEL: usize = 32;

/// 並列構築（[`HnswIndex::build_with_threads`]・[`HnswIndex::build_parallel`]。
/// Issue #406）で逐次に構築する先頭ノード数（qdrant 方式。孤立成分の発生を
/// 防ぐ。本リポ採用値・非規範）。`parallel_build` モジュールの並列フェーズは
/// この件数を超えるノードのみを対象にする。
pub const SEQUENTIAL_PREFIX_NODES: usize = 256;

/// 並列構築のスレッド数上限（`parallel_search::MAX_THREADS_PER_QUERY` と同値。
/// [`HnswIndex::build_parallel`] が決める並列度もこの上限でクランプされる）。
pub const MAX_BUILD_THREADS: usize = 16;

/// `repair_reachability`（フェーズ 1）の反復回数の絶対上限。意図的に
/// `member_count`／`n` に比例させない（比例させると入力規模に応じて
/// 計算量 DoS を招く。`repair_reachability_inner` のドキュメンテーション
/// コメント参照）。Issue #447 でベンチ・テストから参照できるようモジュール
/// レベルへ昇格した（元は関数ローカル定数。値・用途は不変）。
pub const PRECISE_REPAIR_CAP: usize = 64;

/// [`HnswIndex::build_with_threads_observed`] が返す並列構築の段別プロファイル
/// （Issue #406 追記: 8→12 スレッド頭打ち要因の切り分け計測）。
///
/// レベル割当・逐次プレフィックス・凍結・`repair_reachability` はいずれも
/// スレッド数に依らず単一スレッドで実行される段であり、これらの合計
/// （`sequential_prefix` 等）が `total` に占める比率（Amdahl の法則でいう
/// 逐次割合）が頭打ちの構造要因かどうかを、実際に並列化される
/// `parallel_phase` と切り分けて確認できるようにする。合否閾値は持たない
/// 情報提供専用の実測値（`.claude/rules/spec-confidentiality.md` オーナー
/// 判断範囲・数値基準/実測値は公開可）。
#[derive(Debug, Clone, Default)]
pub struct HnswBuildProfile {
    /// レベル割当（`seed` からの逐次確定。並列フェーズ開始前）。
    pub level_assign: std::time::Duration,
    /// 先頭 [`SEQUENTIAL_PREFIX_NODES`] 件の逐次挿入（縮退経路ではここへ
    /// `total` 相当の全量を積む。[`HnswIndex::build_with_threads_observed`]
    /// 参照）。
    pub sequential_prefix: std::time::Duration,
    /// `thread::scope` による並列挿入フェーズ全体の壁時間（ワーカー起動〜
    /// 全 join 完了まで）。縮退経路では 0（ワーカーが存在しないため）。
    pub parallel_phase: std::time::Duration,
    /// 並列フェーズ完了後、`RwLock` を解いて [`HnswIndex`] の内部表現へ
    /// 組み立て直す段（`repair_reachability` を含まない）。
    pub freeze: std::time::Duration,
    /// 全ノード挿入後の到達性修復パス（単一スレッド。モジュール内
    /// `repair_reachability` のドキュメンテーションコメント参照）。
    pub repair_reachability: std::time::Duration,
    /// 可変長ビルダー表現（[`GraphBuilder`]）から CSR（[`csr::CsrGraph`]）へ
    /// 平坦化する段（Issue #494。`freeze`・`repair_reachability` の両方が
    /// 完了した後の最終段。`build`（逐次）の縮退経路ではこの区切りが
    /// 存在しないため `sequential_prefix` へ全量を積み、本フィールドは
    /// `Duration::ZERO` のままにする——[`HnswIndex::build_with_threads_observed`]
    /// の縮退経路ドキュメンテーションコメント参照）。
    pub flatten: std::time::Duration,
    /// `build_with_threads_observed` 呼び出し全体の壁時間（上記各段の合計
    /// より長くなり得る——検証・エラー分岐等の測定対象外区間を含むため）。
    pub total: std::time::Duration,
    /// 並列フェーズの各ワーカースレッドの観測値。縮退経路では空。
    pub workers: Vec<HnswWorkerStats>,
    /// `repair_reachability` 内訳統計（Issue #447 追記: 修復対象ノード数・
    /// 反復回数の観測フック。`repair_reachability` フィールド（壁時間の
    /// みの既存フィールド）とは独立に、層ごとの到達不能ノード数・フェーズ 1
    /// 反復回数・フェーズ 2 結線数を観測する。`threads==1`／`n<=
    /// SEQUENTIAL_PREFIX_NODES` の縮退経路でも本フィールドのみ埋まる
    /// （`build_with_threads_observed` のドキュメンテーションコメント参照。
    /// 他の既存フィールドの意味・値は不変）。`.claude/rules/
    /// spec-confidentiality.md` オーナー判断範囲・数値基準/実測値は公開可）。
    pub repair: HnswRepairStats,
}

/// 層ごとの `repair_reachability` 統計（Issue #447）。`repair_reachability_inner`
/// の観測版（`OBSERVE=true`）のみが埋める。非観測経路（`build`・
/// `build_with_threads`・`parallel_build::freeze`）は本構造体を生成しない
/// （観測コストを一切乗せない設計。下記 `repair_reachability_inner` 参照）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HnswRepairLevelStats {
    /// 対象層番号（0 始まり）。
    pub level: usize,
    /// 当該層のフェーズ 1 開始時点で entry から到達不能だったノード数
    /// （フェーズ 1 の最初の反復の BFS 結果をそのまま数え上げる。追加の
    /// BFS は行わない）。
    pub unreachable_before: u64,
    /// フェーズ 1 で実際に未到達ノードを見つけて結線した反復回数
    /// （上限 [`PRECISE_REPAIR_CAP`]）。
    pub phase1_iterations: u64,
    /// フェーズ 1 が [`PRECISE_REPAIR_CAP`] 回まで完走した（＝反復回数の
    /// 絶対上限に到達した）か。
    pub phase1_cap_hit: bool,
    /// フェーズ 2 でチェーン結線した残存ノード数（`remaining.len()`）。
    pub phase2_nodes: u64,
    /// フェーズ 2 で entry の `shrink_links` により犠牲になったリンクを
    /// 再結線した件数（0 または 1）。
    pub phase2_entry_relinked: u64,
    /// フェーズ 1（BFS＋厳密修復ループ）の壁時間。
    pub phase1_wall: std::time::Duration,
    /// フェーズ 2（チェーン結線）の壁時間。
    pub phase2_wall: std::time::Duration,
}

/// `repair_reachability` 呼び出し全体の統計（Issue #447）。
#[derive(Debug, Clone, Default)]
pub struct HnswRepairStats {
    /// 層 `0..=max_level` の順（`levels.len() == max_level + 1`。エントリ
    /// 不在時は空のまま）。
    pub levels: Vec<HnswRepairLevelStats>,
    /// 観測版 `repair_reachability_inner::<true>` 本体の壁時間（各層の
    /// フェーズ wall の総和以上・呼び出し元の外側計測
    /// （`HnswBuildProfile::repair_reachability`）以下になる入れ子区間）。
    pub wall: std::time::Duration,
}

/// 並列構築フェーズにおける 1 ワーカースレッドの観測値（Issue #406 追記）。
#[derive(Debug, Clone, Default)]
pub struct HnswWorkerStats {
    /// このワーカーが実際に挿入したノード数（ワークスティールのため
    /// ワーカー間で不均等になり得る）。
    pub inserted_nodes: u64,
    /// このワーカーのループ全体（`fetch_add` によるノード取得を含む）の壁時間。
    pub busy: std::time::Duration,
    /// `BuildGraph::read_links`／`write_links` で `try_read`／`try_write` が
    /// `WouldBlock` を返し、ブロックする取得（`read`／`write`）へ落ちた回数。
    pub link_lock_blocked: u64,
    /// `BuildGraph::read_links`／`write_links` の取得試行総数
    /// （`link_lock_blocked` 込み）。
    pub link_lock_acquired: u64,
    /// `link_lock_blocked` に数えた取得（`try_read`／`try_write` が
    /// `WouldBlock` を返しブロックする取得へ落ちた場合）のみ、実際に
    /// ロックが取れるまで `Instant` で計測した待ち時間の累積
    /// （codex-review P2 指摘・PR #445: ロック競合が頭打ちの主要因かどうかを
    /// `busy` に対する割合で判定できるようにする。観測版
    /// `build_parallel_graph_observed` 限定の計装であり、成功する
    /// `try_read`／`try_write` 経路には追加の `Instant::now()` 呼び出しを
    /// 乗せない）。
    pub link_lock_wait: std::time::Duration,
    /// このワーカーが `try_promote_entry` で実際にエントリポイントを
    /// 更新した回数。
    pub entry_promotions: u64,
    /// このワーカーが `plan_links` の層探索で退化した候補集合
    /// （`candidates.len() <= 1`）を観測した回数（Issue #448 追記。
    /// `parallel_build.rs::DEGENERATE_LAYER_SEARCHES` 参照）。
    pub degenerate_layer_searches: u64,
    /// このワーカーが `ensure_reverse_link` で実際に再結線を行った回数
    /// （Issue #448 追記。`parallel_build.rs::REVERSE_LINK_RECONNECTS` 参照）。
    pub reverse_link_reconnects: u64,
}

/// マスク付き探索（[`HnswIndex::search_masked_with_hop`]）が非受理ノードの
/// リンクをどこまで中継点として辿るかを表す（Issue #501・親 #500。ACORN-1
/// 方式。ポインタ: CORE-9・CORE-10・TASK-132）。
///
/// - `OneHop`（既定）: 非受理ノードは訪問済みマークのみ付けて打ち切る
///   （既存契約。`accept.is_none()` の場合と同じくビット同一を保つ）。
/// - `TwoHop`: 非受理ノードを 1 段だけ中継点として使い、その隣接
///   （2-hop 先）にいる受理ノードのみを候補化する（[`bridge_expand`]）。
///   非受理ノードのベクトルは一切参照しない（I1 不変。§本モジュール
///   `search_layer_in` ドキュメンテーションコメント参照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HopMode {
    OneHop,
    TwoHop,
}

/// 整数比（`u32/u32`）。`f32` は `HnswParams` の `Copy + PartialEq + Eq` derive と
/// 両立しない（`f32` は `Eq` を実装しない）ため、Issue #401 の `REBUILD_DELTA_RATIO`
/// （`(u64, u64)` タプル）と同じ発想で構造体化した（Issue #409。`sql::hnsw_cache`
/// の可視カーディナリティ切替閾値 `ValidatedHnswParams::full_scan_ratio` に使う）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ratio {
    pub numerator: u32,
    pub denominator: u32,
}

impl fmt::Display for Ratio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.numerator, self.denominator)
    }
}

/// HNSW 構築パラメータ。
///
/// 既定値（`M=16`／`ef_construction=100`／`ef_search=64`）は ADR 起票 Issue #403
/// に記載の本リポ採用値（非規範的な実装既定値。spec 側の確定値ではない）。
///
/// `#[non_exhaustive]` は付与しない（codex-review P1 指摘・Issue #409・PR #435）:
/// 既に公開済みの本構造体へ後付けで `#[non_exhaustive]` を付けると、外部クレートが
/// 既存フィールドで構築する構造体リテラル（構造体更新構文を使わないもの含む）が
/// コンパイル不能になり、それ自体が破壊的変更になる（`docs/design/
/// error-enum-non-exhaustive-policy.md` と同じ判断）。可視カーディナリティ切替の
/// 閾値比（Issue #409）は本構造体へフィールド追加せず、検証済みラッパー
/// [`ValidatedHnswParams`] の private フィールド（`full_scan_ratio`）として持たせる
/// ことで、本構造体の公開フィールド集合は変更しない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HnswParams {
    /// 挿入後に各ノードが層 1 以上で保持する隣接数の目安（層 0 は `2*m` まで許容する。
    /// Malkov & Yashunin 2016 の記法と同じ）。
    pub m: usize,
    /// 挿入時の貪欲探索の候補幅。
    pub ef_construction: usize,
    /// 探索時の候補幅（本モジュールでは構築後のパラメータ保持のみ。実際の探索は #405）。
    pub ef_search: usize,
}

impl Default for HnswParams {
    fn default() -> Self {
        Self {
            m: 16,
            ef_construction: 100,
            ef_search: 64,
        }
    }
}

impl HnswParams {
    /// パラメータの範囲検証。`m < 2`（`m=1` だとレベル割当の `mL = 1/ln(m)` が
    /// 定義できない・次数上限が実用にならない）・`m > MAX_M`・`ef_construction == 0`・
    /// `ef_construction > MAX_EF`・`ef_search == 0`・`ef_search > MAX_EF` を拒否する。
    pub fn validate(&self) -> Result<(), HnswError> {
        if self.m < 2 {
            return Err(HnswError::InvalidParams {
                reason: "m must be >= 2",
            });
        }
        if self.m > MAX_M {
            return Err(HnswError::InvalidParams {
                reason: "m exceeds MAX_M",
            });
        }
        if self.ef_construction == 0 {
            return Err(HnswError::InvalidParams {
                reason: "ef_construction must be >= 1",
            });
        }
        if self.ef_construction > MAX_EF {
            return Err(HnswError::InvalidParams {
                reason: "ef_construction exceeds MAX_EF",
            });
        }
        if self.ef_search == 0 {
            return Err(HnswError::InvalidParams {
                reason: "ef_search must be >= 1",
            });
        }
        if self.ef_search > MAX_EF {
            return Err(HnswError::InvalidParams {
                reason: "ef_search exceeds MAX_EF",
            });
        }
        Ok(())
    }

    /// `m` だけを差し替えたコピーを返す（`HnswParams::default()` と組み合わせて
    /// 使うビルダー用アクセサ。構造体リテラル `..HnswParams::default()` と等価だが
    /// 呼び出し元の記述を短くする）。
    pub fn with_m(mut self, m: usize) -> Self {
        self.m = m;
        self
    }

    /// `ef_construction` だけを差し替えたコピーを返す（[`Self::with_m`] 参照）。
    pub fn with_ef_construction(mut self, ef_construction: usize) -> Self {
        self.ef_construction = ef_construction;
        self
    }

    /// `ef_search` だけを差し替えたコピーを返す（[`Self::with_m`] 参照）。
    pub fn with_ef_search(mut self, ef_search: usize) -> Self {
        self.ef_search = ef_search;
        self
    }
}

/// [`HnswParams::validate`] を通過済みであることを型で保証するラッパー
/// （codex-review P1 指摘・Issue #407・PR #433 追記）。
///
/// フィールドは private のため、[`Self::new`]（[`HnswParams::validate`] を必ず経由する）
/// 以外の経路では構築できない。`crate::search_engine::SearchEngineKind::Hnsw` の
/// payload をこの型にすることで、不正な `HnswParams` を保持した `SearchEngineKind`・
/// [`crate::hnsw::provider::HnswSearchProvider`] がそもそも型として存在しえなくなる
/// （実行時エラー分類の流用・偽装ではなく、型システムで到達不能にする）。
///
/// 可視カーディナリティ切替の閾値比 `full_scan_ratio`（Issue #409。既定 1/10。
/// `sql::hnsw_cache::HnswIndexCache` が「可視候補数 ÷ 索引ノード数」の比が
/// この値未満なら plain scan、以上ならマスク付き ANN 探索を選ぶ判定に使う）は
/// 本ラッパーの private フィールドとして持つ（codex-review P1 指摘・Issue #409・
/// PR #435 是正）。[`HnswParams`] へ直接フィールド追加すると、既に公開済みの
/// 同構造体を使う外部クレートの構造体リテラルを破壊する（`#[non_exhaustive]`
/// 後付けも同様に破壊的。`docs/design/error-enum-non-exhaustive-policy.md` と
/// 同じ判断）ため、`ValidatedHnswParams` は元々 [`Self::new`] 経由でしか構築
/// できない private フィールドの型であることを利用し、フィールド追加を非破壊に
/// 収める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedHnswParams {
    params: HnswParams,
    full_scan_ratio: Ratio,
    resident_precision: ResidentPrecision,
    sparse_visited_max: usize,
    /// ACORN-1 の 2-hop 展開（[`HopMode::TwoHop`]）を有効化する可視比率の
    /// 上限（Issue #501・親 #500。既定 `None`＝無効・既存動作を不変に保つ）。
    /// `Some(ratio)` のとき `sql::hnsw_cache::traversal_regime_for` が
    /// `full_scan_ratio <= r <= ratio` のレジームを `TwoHop` と判定する。
    acorn_max_visible_ratio: Option<Ratio>,
    /// TwoHop（ACORN-1）探索 1 回の橋渡し展開件数が可視ノード数に対して
    /// この比を超えたら plain scan へ fail-closed に縮退する上限比
    /// （Issue #681・親 #674。既定 `None`＝ガード無効・既存動作を不変に保つ）。
    /// `sql::hnsw_cache::search_with_overlay` が TwoHop 完走直後の事後判定
    /// （[`acorn_expansions_exceed`]）に使う。`acorn_max_visible_ratio` が
    /// `None` のままでも受理する独立フィールド（`hop == TwoHop` レジームへ
    /// 到達しない設定では単に観測されない）。
    acorn_max_expansion_ratio: Option<Ratio>,
}

/// HNSW 索引ノードの常駐ベクトル表現（Issue #514・親 #513。ポインタ:
/// TASK-132・TASK-156・CORE-16）。
///
/// `F16` は `SearchEngineKind::Hnsw` opt-in 経路限定の追加 opt-in
/// （[`ValidatedHnswParams::with_resident_precision`]）であり、既定は `F32`
/// （既存の全動作を不変に保つ）。`docs/design/simd-intrinsics-adoption.md`
/// 決定 5 のとおり、索引ヒットの最終スコアは常に `kernel::dot`（f32・
/// アリーナ再計算）を経由するため、この選択は候補生成段の常駐表現のみに影響する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResidentPrecision {
    /// f32 常駐（既定。既存の全経路と挙動不変）。
    #[default]
    F32,
    /// f16（IEEE 754 binary16）常駐。`isa::F16Kernel::dot_f16` による昇格
    /// dot で候補生成スコアを計算する。1 成分でも f16 の有限範囲
    /// （`|x| <= 65504.0`）を超える場合は凍結時に `F32` へ自動縮退する
    /// （`HnswIndex::resident_precision` が実効値を返す）。
    F16,
    /// 対称スカラー量子化（SQ8。次元ごと min/max 由来のスケール）による i8
    /// 常駐（Issue #521・親 #520）。`sq8::dot_i8_f32` による復号 dot（格納側
    /// のみ低精度化しクエリは f32 のまま）で候補生成スコアを計算する。
    /// `sq8::fit_dim_params`／`sq8::encode_rows` が失敗した場合（非有限成分・
    /// アロケーション失敗）は凍結時に `F32` へ自動縮退する
    /// （`HnswIndex::resident_precision` が実効値を返す。F16 と同じ D6 契約）。
    I8,
}

impl fmt::Display for ResidentPrecision {
    /// `EXPLAIN` の `hnsw_params: resident=<value>`（Issue #514・R5）が使う
    /// 閉じた語彙表記。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ResidentPrecision::F32 => write!(f, "f32"),
            ResidentPrecision::F16 => write!(f, "f16"),
            ResidentPrecision::I8 => write!(f, "i8"),
        }
    }
}

/// [`ValidatedHnswParams`] の `full_scan_ratio` 既定値（1/10。Issue #409）。
const DEFAULT_FULL_SCAN_RATIO: Ratio = Ratio {
    numerator: 1,
    denominator: 10,
};

/// [`ValidatedHnswParams`] の `sparse_visited_max` 既定値（Issue #497）。
/// `0` は「常にビットマップ visited のみを使う」（既存の全動作を不変に保つ）
/// ことを意味する。閾値の既定値確定・可視比率別の費用対効果測定は後続
/// Issue #498 の担当（`docs/design/hnsw-search.md`「visited 集合の 3 実装」
/// 節の申し送り）。
const DEFAULT_SPARSE_VISITED_MAX: usize = 0;

impl ValidatedHnswParams {
    /// `params` を [`HnswParams::validate`] で検証し、通過した場合のみ構築する。
    /// `full_scan_ratio` は既定値（[`DEFAULT_FULL_SCAN_RATIO`]）、
    /// `sparse_visited_max` は既定値（[`DEFAULT_SPARSE_VISITED_MAX`]）で
    /// 初期化される。差し替えたい場合は [`Self::with_full_scan_ratio`]・
    /// [`Self::with_sparse_visited_max`] を使う。
    pub fn new(params: HnswParams) -> Result<Self, HnswError> {
        params.validate()?;
        Ok(Self {
            params,
            full_scan_ratio: DEFAULT_FULL_SCAN_RATIO,
            resident_precision: ResidentPrecision::F32,
            sparse_visited_max: DEFAULT_SPARSE_VISITED_MAX,
            acorn_max_visible_ratio: None,
            acorn_max_expansion_ratio: None,
        })
    }

    /// visited 集合の切替閾値を返す（Issue #497。
    /// [`HnswIndex::search_masked_with`] がマスク付き探索で `mask.count_ones()`
    /// がこの値未満のとき [`VisitedSparse`] を選ぶ。`0`（既定）は常に
    /// [`VisitedBitmap`] を使うことを意味する）。
    pub fn sparse_visited_max(&self) -> usize {
        self.sparse_visited_max
    }

    /// `sparse_visited_max` だけを差し替えたコピーを返す（Issue #497・opt-in。
    /// 検証を要さないためシグネチャは [`Result`] を返さない
    /// [`Self::with_resident_precision`] と同型）。既定値のまま（`0`）だと
    /// 既存の全動作を不変に保つ（`docs/design/benchmark-judgement-policy.md`
    /// の趣旨に沿い、未計測の性能変更を既定にしない）。
    pub fn with_sparse_visited_max(mut self, max: usize) -> Self {
        self.sparse_visited_max = max;
        self
    }

    /// 索引ノードの常駐精度（[`ResidentPrecision`]）を返す（構築時に指定した
    /// 静的設定値。凍結時の自動縮退による実効値は [`HnswIndex::resident_precision`]
    /// を参照する）。
    pub fn resident_precision(&self) -> ResidentPrecision {
        self.resident_precision
    }

    /// `resident_precision` だけを差し替えたコピーを返す（Issue #514・opt-in。
    /// 検証を要さないためシグネチャは [`Result`] を返さない
    /// `with_full_scan_ratio` とは異なる）。
    pub fn with_resident_precision(mut self, precision: ResidentPrecision) -> Self {
        self.resident_precision = precision;
        self
    }

    /// 検証済みの内部値を返す（`m`／`ef_construction`／`ef_search` フィールドへの
    /// 読み取りアクセス用。書き込みは許さない＝再検証なしに値を変更できない）。
    pub fn get(&self) -> HnswParams {
        self.params
    }

    /// `full_scan_ratio` を返す（`sql::hnsw_cache::search_with_overlay` が可視
    /// カーディナリティ切替の判定に使う。Issue #409）。
    pub fn full_scan_ratio(&self) -> Ratio {
        self.full_scan_ratio
    }

    /// `full_scan_ratio` だけを差し替えたコピーを返す。`ratio.denominator == 0`・
    /// `ratio.numerator > ratio.denominator` は [`HnswError::InvalidParams`] として
    /// 拒否する（元 `HnswParams::validate` が担っていた検証をここへ移設。
    /// Issue #409・codex-review P1 是正・PR #435）。
    pub fn with_full_scan_ratio(mut self, ratio: Ratio) -> Result<Self, HnswError> {
        if ratio.denominator == 0 {
            return Err(HnswError::InvalidParams {
                reason: "full_scan_ratio denominator must be >= 1",
            });
        }
        if ratio.numerator > ratio.denominator {
            return Err(HnswError::InvalidParams {
                reason: "full_scan_ratio numerator must not exceed denominator",
            });
        }
        if let Some(acorn) = self.acorn_max_visible_ratio {
            if ratio_lt(acorn, ratio) {
                return Err(HnswError::InvalidParams {
                    reason: "full_scan_ratio must not exceed acorn_max_visible_ratio",
                });
            }
        }
        self.full_scan_ratio = ratio;
        Ok(self)
    }

    /// ACORN-1 の 2-hop 展開を有効化する可視比率の上限を返す（Issue #501。
    /// `None`＝既定・無効）。
    pub fn acorn_max_visible_ratio(&self) -> Option<Ratio> {
        self.acorn_max_visible_ratio
    }

    /// `acorn_max_visible_ratio` だけを差し替えたコピーを返す（Issue #501・
    /// opt-in）。`ratio.denominator == 0`・`ratio.numerator > ratio.denominator`・
    /// `ratio < full_scan_ratio`（2-hop レジームが plain scan 未満の可視比率で
    /// 発火し得ることになり無意味かつ安全側の前提〔#500〕を崩す）はいずれも
    /// `HnswError::InvalidParams` として拒否する（fail-closed）。
    pub fn with_acorn_max_visible_ratio(mut self, ratio: Ratio) -> Result<Self, HnswError> {
        if ratio.denominator == 0 {
            return Err(HnswError::InvalidParams {
                reason: "acorn_max_visible_ratio denominator must be >= 1",
            });
        }
        if ratio.numerator > ratio.denominator {
            return Err(HnswError::InvalidParams {
                reason: "acorn_max_visible_ratio numerator must not exceed denominator",
            });
        }
        if ratio_lt(ratio, self.full_scan_ratio) {
            return Err(HnswError::InvalidParams {
                reason: "acorn_max_visible_ratio must not be less than full_scan_ratio",
            });
        }
        self.acorn_max_visible_ratio = Some(ratio);
        Ok(self)
    }

    /// TwoHop 展開過多ガード（Issue #681）の上限比を返す（`None`＝既定・無効）。
    pub fn acorn_max_expansion_ratio(&self) -> Option<Ratio> {
        self.acorn_max_expansion_ratio
    }

    /// `acorn_max_expansion_ratio` だけを差し替えたコピーを返す（Issue #681・
    /// opt-in）。`ratio.denominator == 0`・`ratio.numerator > ratio.denominator`
    /// は `HnswError::InvalidParams` として拒否する（fail-closed。他フィールドとの
    /// 順序制約は課さない——`acorn_max_visible_ratio` が `None` のままでも受理する。
    /// `numerator == 0` は受理する（「展開が 1 件でもあれば縮退」の意味）。
    pub fn with_acorn_max_expansion_ratio(mut self, ratio: Ratio) -> Result<Self, HnswError> {
        if ratio.denominator == 0 {
            return Err(HnswError::InvalidParams {
                reason: "acorn_max_expansion_ratio denominator must be >= 1",
            });
        }
        if ratio.numerator > ratio.denominator {
            return Err(HnswError::InvalidParams {
                reason: "acorn_max_expansion_ratio numerator must not exceed denominator",
            });
        }
        self.acorn_max_expansion_ratio = Some(ratio);
        Ok(self)
    }
}

/// `a < b` を `u64` へワイド化した交差乗算で判定する（`u32 * u32` は `u64` へ
/// 収まるため `checked_mul` は不要。`sql::hnsw_cache` の可視比率判定と同じ
/// 比較方式に揃える。Issue #501）。
fn ratio_lt(a: Ratio, b: Ratio) -> bool {
    (a.numerator as u64) * (b.denominator as u64) < (b.numerator as u64) * (a.denominator as u64)
}

impl std::ops::Deref for ValidatedHnswParams {
    type Target = HnswParams;
    fn deref(&self) -> &HnswParams {
        &self.params
    }
}

impl Default for ValidatedHnswParams {
    /// `HnswParams::default()` は本モジュールのテスト
    /// （`hnsw_default_params_pass_validation`／`search_engine.rs::
    /// hnsw_default_params_pass_validation`）で常に検証を通過することを固定済みの
    /// 定数のため、untrusted 入力経路ではなく `.expect` の使用が
    /// `coding-rust.md` の禁止規約（受信データ経路での `unwrap`/`expect` 禁止）に
    /// 抵触しない。
    fn default() -> Self {
        ValidatedHnswParams::new(HnswParams::default())
            .expect("HnswParams::default() is a fixed constant known to pass validate()")
    }
}

/// [`HnswIndex::build`] の失敗要因。`Display`／`std::error::Error` を実装し
/// ライブラリコードとして panic せず `Result` で契約する（coding-rust.md）。
/// ベクトル値そのものは含めない（テナント情報・行データを含まない索引という
/// 本モジュールの契約を、エラー経路でも壊さないため）。
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HnswError {
    /// `HnswParams::validate` が拒否した。
    InvalidParams { reason: &'static str },
    /// `dim == 0`、または `vectors.len()` が `dim` の整数倍でない。
    DimMismatch { dim: u32, len: usize },
    /// ノード数が [`MAX_HNSW_NODES`] を超える。
    TooManyNodes { nodes: usize },
    /// 入力ベクトルに NaN／Inf が含まれる（構築後に順序を壊す経路を作らないため
    /// 構築段で拒否する）。
    NonFiniteVector { node: usize },
    /// `dot` の計算結果が非有限（NaN／Inf）だった。各入力要素は
    /// `NonFiniteVector` 検証で有限であることを確認済みでも、有限な大きな
    /// `f32` 同士の積・その総和は `Inf`（オーバーフロー）や `NaN`（正負の
    /// `Inf` の加算）になり得る。この非有限スコアが `ScoredNode::cmp`・
    /// ヒープ・近傍選択へそのまま入ると「探索段では常に有限値のみを比較
    /// する」契約が破れ、順序が壊れたグラフを正常結果として返してしまう
    /// ため、`dot` を呼ぶ全経路（[`HnswIndex::score`]・
    /// [`HnswIndex::repair_reachability`]・
    /// [`HnswIndex::select_neighbors_heuristic`]・[`HnswIndex::shrink_links`]）
    /// で計算直後に検証し fail-closed で拒否する。
    NonFiniteScore { node: u32 },
    /// オフセット・容量計算が `usize`／`u32` の範囲を超えた。
    CapacityOverflow,
    /// [`HnswIndex::search`]（#405）のクエリベクトルの次元が索引の次元
    /// （[`HnswIndex::dim`]）と一致しない。
    QueryDimMismatch { expected: u32, found: usize },
    /// [`HnswIndex::search`] のクエリベクトルに NaN／Inf が含まれる。
    /// `kernel.rs::KernelError::NonFiniteQuery` と同じ理由（wire 経由の
    /// untrusted 入力を `total_cmp` の順序に委ねず事前拒否する）で、探索段の
    /// 入口で検証する。
    NonFiniteQuery,
    /// [`HnswIndex::build_with_threads`]／[`HnswIndex::build_parallel`]
    /// （Issue #406）の構築ワーカーが panic した、またはノード単位ロック
    /// （`parallel_build::BuildGraph::links`）・エントリポイントロック
    /// （`parallel_build::BuildGraph::entry`）が poison した。全ハンドルを
    /// join したうえで fail-closed に拒否し、部分的に結線された索引を
    /// `Ok` で返さない（モジュール冒頭「失敗契約」参照）。
    WorkerPanicked,
}

impl fmt::Display for HnswError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HnswError::InvalidParams { reason } => write!(f, "invalid HNSW params: {reason}"),
            HnswError::DimMismatch { dim, len } => {
                write!(
                    f,
                    "vector buffer length {len} is not a multiple of dim {dim}"
                )
            }
            HnswError::TooManyNodes { nodes } => {
                write!(
                    f,
                    "node count {nodes} exceeds MAX_HNSW_NODES ({MAX_HNSW_NODES})"
                )
            }
            HnswError::NonFiniteVector { node } => {
                write!(f, "vector for node {node} contains a non-finite value")
            }
            HnswError::NonFiniteScore { node } => {
                write!(f, "dot product score for node {node} is non-finite")
            }
            HnswError::CapacityOverflow => write!(f, "capacity computation overflowed"),
            HnswError::QueryDimMismatch { expected, found } => write!(
                f,
                "hnsw search query dim mismatch: expected={expected} found={found}"
            ),
            HnswError::NonFiniteQuery => write!(f, "hnsw search query contains non-finite value"),
            HnswError::WorkerPanicked => {
                write!(
                    f,
                    "hnsw parallel build worker panicked or a lock was poisoned"
                )
            }
        }
    }
}

impl std::error::Error for HnswError {}

/// 決定的シードの xorshift64* PRNG（`benches/harness/rng.rs::DeterministicRng` と
/// 同アルゴリズム。`src/` から bench harness を参照できないため独立に複製する）。
///
/// 非暗号 PRNG。レベル割当専用でありセキュリティ用途に転用しない（OWASP A02）。
struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
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

    /// `(0.0, 1.0]` の単位区間に収まる f64 疑似乱数値を返す（`0` を除外するのは
    /// `assign_level` が `-ln(u)` を取るため。`0` だと `ln(0) = -inf` になり
    /// レベルが発散する）。
    fn next_open01(&mut self) -> f64 {
        // 53bit の仮数部精度で一様分布を作る（f64 の仮数部ビット数に合わせる）。
        let bits = self.next_u64() >> 11; // 53bit
        let u = (bits as f64) / (1u64 << 53) as f64;
        // u ∈ [0, 1) を (0, 1] へシフトする（1.0 - u なら u=0 のとき 1.0 になる）。
        1.0 - u
    }
}

/// レベル割当（Malkov & Yashunin 2016 Algorithm 1 の `l = floor(-ln(unif(0,1)) * mL)`）。
/// `mL = 1 / ln(m)`。結果は [`MAX_LEVEL`] でクランプする（`HnswParams::validate` が
/// `m >= 2` を保証するため `ln(m)` は必ず正の有限値になる）。
fn assign_level(rng: &mut DeterministicRng, m: usize) -> usize {
    let m_l = 1.0 / (m as f64).ln();
    let u = rng.next_open01();
    let level = (-u.ln() * m_l).floor();
    if !level.is_finite() || level <= 0.0 {
        0
    } else {
        (level as usize).min(MAX_LEVEL)
    }
}

/// 候補ヒープの要素。スコア降順・同点は id 昇順を「強い」とする順序を `Ord` に
/// 埋め込む（`kernel.rs::MinHeapItem` と同じ規約。構築入力は事前に非有限値を
/// 拒否済みのため `total_cmp` の呼び出しは常に有限値同士になる）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ScoredNode {
    node: u32,
    score: f32,
}

impl Eq for ScoredNode {}

impl PartialOrd for ScoredNode {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ScoredNode {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score
            .total_cmp(&other.score)
            .then(other.node.cmp(&self.node))
    }
}

/// 1 ノード分のグラフ状態。`links[l]` が層 `l` の隣接リスト（`l` は `0..=level`）。
/// ベクトル本体は持たない（モジュール冒頭「ベクトルの所有方針」参照）。
#[derive(Debug)]
struct Node {
    level: usize,
    links: Vec<Vec<u32>>,
}

/// [`search_layer_in`]（旧 `search_layer_with` の本体。#405・#494）が構築中
/// （[`GraphBuilder`]）・凍結後（[`csr::CsrGraph`]）のどちらの隣接表現にも
/// 依存せず動作できるようにする最小インターフェース（Issue #494・
/// `docs/design/hnsw-index.md` §14.4）。両実装とも [`Node::level`] を反映した
/// `level_of`・当該レベルの隣接スライスを返す `neighbors`（`level >
/// level_of(node)` またはノード範囲外は `None`）・現在のノード総数を返す
/// `node_count` を持つ。
pub(crate) trait Adjacency {
    /// ノード `node` が割り当てられたレベル。存在しないノードは `None`。
    fn level_of(&self, node: u32) -> Option<usize>;
    /// 層 `level` におけるノード `node` の隣接リスト。存在しない層・ノードは
    /// `None`（ノードのレベルが `level` 未満の場合を含む）。
    fn neighbors(&self, level: usize, node: u32) -> Option<&[u32]>;
    /// 現在のノード総数。
    fn node_count(&self) -> usize;
}

/// 構築中の可変長グラフ表現（Issue #494 で [`HnswIndex`] から分離。
/// `docs/design/hnsw-index.md` §14.2 の「2 相構成」の前半を担う）。
///
/// 並列構築（[`parallel_build`]）と凍結後の [`HnswIndex::repair_reachability`]
/// 相当の修復パスは `connect`／`shrink_links` による可変長 in-place 更新を
/// 要するため、構築中は本表現（ノードごとに個別確保した `Vec<Vec<u32>>`）を
/// 維持し、全ノード挿入・修復が完了した時点で 1 回だけ [`csr::CsrGraph`] へ
/// 平坦化する（[`HnswIndex::freeze_from`] 参照。平坦化は必ず最終段——
/// #449 系の修復並列化がこの構造へ追加の書き込みを差し込む場合も、
/// 本表現に対して行い、平坦化後の `CsrGraph` へは書き込まない契約とする）。
///
/// `vectors`（row-major バッファ）は保持しない——構築中の各メソッドは
/// 既存の呼び出し規約どおり `vectors: &[f32]` を引数で受け取る（`build` の
/// ループが所有権を持つ借用元バッファをそのまま渡す）。
pub(crate) struct GraphBuilder {
    params: HnswParams,
    nodes: Vec<Node>,
    entry_point: Option<u32>,
}

impl Adjacency for GraphBuilder {
    fn level_of(&self, node: u32) -> Option<usize> {
        self.nodes.get(node as usize).map(|n| n.level)
    }

    fn neighbors(&self, level: usize, node: u32) -> Option<&[u32]> {
        self.nodes
            .get(node as usize)
            .and_then(|n| n.links.get(level))
            .map(|l| l.as_slice())
    }

    fn node_count(&self) -> usize {
        self.nodes.len()
    }
}

/// 構築済み HNSW グラフ。`build` 完了時に渡された `vectors` の内容を
/// `Arc<[f32]>` として所有する（モジュール冒頭「ベクトルの所有方針」節・
/// codex-review PR #430 P1 指摘対応）。`search` は呼び出し元からベクトルを
/// 受け取らず、常にこの不変スナップショットのみを参照するため、長さ・
/// 内容が食い違うバッファが渡されるという事故のクラス自体が存在しない。
#[derive(Debug)]
pub struct HnswIndex {
    params: HnswParams,
    dim: u32,
    /// 凍結済みグラフ（Issue #494。[`csr::CsrGraph`]）。構築中の可変長
    /// 表現（[`GraphBuilder`]）から [`HnswIndex::freeze_from`] が 1 回だけ
    /// 平坦化する。
    graph: csr::CsrGraph,
    entry_point: Option<u32>,
    /// `build` 時点の `vectors`（row-major・`len() == graph.node_count() * dim`）
    /// の不変スナップショット（Issue #514 で [`NodeVectors`] へ一般化。
    /// `search` はこれを [`NodeSource`] 経由で参照する）。
    vectors: NodeVectors,
    /// `vectors` の実効常駐精度（Issue #514）。構築時に要求した精度が
    /// f16 の有限範囲を超える成分により `F32` へ自動縮退した場合、この値は
    /// 要求値と異なる（[`Self::resident_precision`] が返すのは常にこの実効値）。
    resident_precision: ResidentPrecision,
}

/// HNSW 索引ノードの常駐ベクトル本体（Issue #514）。`F32` は既存経路と
/// ビット同一（`Arc<[f32]>` をそのまま保持）、`F16` は `f16::encode_rows` で
/// 1 回エンコードした IEEE 754 binary16 ビット列（`Arc<[u16]>`）を保持する。
/// いずれも row-major 連続バッファで `len() == node_count * dim`。
#[derive(Debug, Clone)]
pub(crate) enum NodeVectors {
    F32(Arc<[f32]>),
    F16(Arc<[u16]>),
    /// 対称 SQ8 常駐（Issue #521）。`codes`（row-major・`len() == node_count *
    /// dim`）と、凍結時に 1 回だけ `sq8::fit_dim_params` で求めた次元ごとの
    /// スケール（`params.dim() == dim`）を対で保持する。`row_sums`
    /// （Issue #522。`sq8::row_sums` が凍結時に 1 回だけ求める `len() ==
    /// node_count` の行和。VNNI 系整数カーネルの符号復元に使う——モジュール
    /// `sq8.rs` 冒頭「整数 i8×i8 dot」節参照。NEON dotprod〔Issue #525〕・
    /// i16 widen・Scalar は符号付き×符号付き積のため `row_sums` を参照しない）
    /// は `codes`・`params` と寿命・
    /// 対応関係が完全に一致する（同じ `freeze_from` 呼び出しで一括生成し、
    /// いずれか 1 つでも失敗すれば 3 つとも作らず `F32` へ縮退する）。
    I8 {
        codes: Arc<[i8]>,
        params: Arc<crate::sq8::Sq8DimParams>,
        row_sums: Arc<[i32]>,
    },
}

impl NodeVectors {
    /// 概算ヒープバイト量（[`HnswIndex::approx_heap_bytes`] が使う）。
    fn approx_bytes(&self) -> usize {
        match self {
            NodeVectors::F32(v) => v.len().saturating_mul(std::mem::size_of::<f32>()),
            NodeVectors::F16(v) => v.len().saturating_mul(std::mem::size_of::<u16>()),
            NodeVectors::I8 {
                codes,
                params,
                row_sums,
            } => codes
                .len()
                .saturating_mul(std::mem::size_of::<i8>())
                .saturating_add(params.approx_heap_bytes())
                .saturating_add(row_sums.len().saturating_mul(std::mem::size_of::<i32>())),
        }
    }
}

/// 探索中の候補生成スコア計算を抽象化する境界（Issue #514）。構築経路
/// （[`GraphBuilder`]。常に f32）は `[f32]` の実装（既存動作そのまま。
/// `score_of`／`prefetch::touch_node_vector` へ委譲）を、索引凍結後の探索経路
/// （[`HnswIndex`]）は [`NodeVectors`] の実装を使う。[`search_layer_in`]・
/// [`prefetch::PrefetchPolicy`] がこの境界を通じて常駐精度に依存せず動作する。
/// `?Sized` は `[f32]`（unsized）を実装対象に含めるため。
pub(crate) trait NodeSource {
    fn score(&self, dim: usize, node: u32, query: &[f32]) -> Result<f32, HnswError>;
    fn touch_prefetch(&self, dim: usize, node: u32);
}

impl NodeSource for [f32] {
    fn score(&self, dim: usize, node: u32, query: &[f32]) -> Result<f32, HnswError> {
        score_of(self, dim, node, query)
    }
    fn touch_prefetch(&self, dim: usize, node: u32) {
        prefetch::touch_node_vector(self, dim, node);
    }
}

impl NodeSource for NodeVectors {
    fn score(&self, dim: usize, node: u32, query: &[f32]) -> Result<f32, HnswError> {
        match self {
            NodeVectors::F32(v) => score_of(v, dim, node, query),
            NodeVectors::F16(v) => {
                let row = node_vector_u16(v, dim, node)?;
                let score = crate::isa::current_f16().dot_f16(row, query);
                if !score.is_finite() {
                    return Err(HnswError::NonFiniteScore { node });
                }
                Ok(score)
            }
            NodeVectors::I8 { codes, params, .. } => {
                let row = node_vector_i8(codes, dim, node)?;
                let score = crate::sq8::dot_i8_f32(row, params.scales(), query);
                if !score.is_finite() {
                    return Err(HnswError::NonFiniteScore { node });
                }
                Ok(score)
            }
        }
    }

    fn touch_prefetch(&self, dim: usize, node: u32) {
        match self {
            NodeVectors::F32(v) => prefetch::touch_node_vector(v, dim, node),
            NodeVectors::F16(v) => prefetch::touch_node_vector_u16(v, dim, node),
            NodeVectors::I8 { codes, .. } => prefetch::touch_node_vector_i8(codes, dim, node),
        }
    }
}

/// `vectors`（f16 ビット表現の row-major バッファ）から `node` 番目の行を
/// 切り出す（[`node_vector`] の f16 版。untrusted 添字アクセスを避けるため
/// `get()` のみを使う）。
fn node_vector_u16(vectors: &[u16], dim: usize, node: u32) -> Result<&[u16], HnswError> {
    let node_usize = node as usize;
    let start = node_usize.checked_mul(dim).ok_or(HnswError::DimMismatch {
        dim: dim as u32,
        len: vectors.len(),
    })?;
    let end = start.checked_add(dim).ok_or(HnswError::DimMismatch {
        dim: dim as u32,
        len: vectors.len(),
    })?;
    vectors.get(start..end).ok_or(HnswError::DimMismatch {
        dim: dim as u32,
        len: vectors.len(),
    })
}

/// `vectors`（SQ8 格納コードの row-major バッファ）から `node` 番目の行を
/// 切り出す（[`node_vector_u16`] の i8 版。Issue #521）。
fn node_vector_i8(vectors: &[i8], dim: usize, node: u32) -> Result<&[i8], HnswError> {
    let node_usize = node as usize;
    let start = node_usize.checked_mul(dim).ok_or(HnswError::DimMismatch {
        dim: dim as u32,
        len: vectors.len(),
    })?;
    let end = start.checked_add(dim).ok_or(HnswError::DimMismatch {
        dim: dim as u32,
        len: vectors.len(),
    })?;
    vectors.get(start..end).ok_or(HnswError::DimMismatch {
        dim: dim as u32,
        len: vectors.len(),
    })
}

/// `search_layer` が呼び出しをまたいで再利用する visited 集合（世代カウンタ
/// 方式）。挿入ごとに新規の `Vec<bool>` を割り当てて毎回ゼロ初期化すると、
/// `build` は各挿入で少なくとも層 0 の `search_layer` を 1 回呼ぶため
/// Σ_{i=1..N} O(i) = O(N^2) の初期化コストが積み上がる（codex-review #423
/// P1 指摘）。本構造体は `epoch` 配列を挿入間で使い回し、リセットを
/// カウンタのインクリメントだけの O(1) にすることでこれを避ける。
/// `current` は `u64` とし、`build` 1 回あたりの呼び出し回数
/// （高々 `N * (MAX_LEVEL+1)` 程度）に対して十分な余裕を持たせ、桁あふれ
/// 処理そのものを不要にする（到達しない分岐を残さない）。
#[derive(Debug, Default)]
pub(crate) struct VisitedScratch {
    epoch: Vec<u64>,
    current: u64,
}

impl VisitedScratch {
    /// 次の呼び出しに備えてリセットする。`len` は呼び出し時点の
    /// `self.nodes.len()`（構築中は挿入のたびに増加するため、呼び出し
    /// ごとに現在値を渡す。使い回すバッファは伸長のみで縮めない）。
    fn reset(&mut self, len: usize) {
        if self.epoch.len() < len {
            self.epoch.resize(len, 0);
        }
        self.current += 1;
    }

    /// `id` を訪問済みとして記録する。戻り値は「今回のリセット以降で
    /// 既に訪問済みだったか」（`true`＝既訪問なのでスキップ、`false`＝
    /// 新規訪問なので処理を続行）。範囲外の `id` は `None`
    /// （呼び出し元は untrusted 添字アクセスをせず `continue` する）。
    fn mark_visited(&mut self, id: usize) -> Option<bool> {
        let slot = self.epoch.get_mut(id)?;
        let already = *slot == self.current;
        *slot = self.current;
        Some(already)
    }
}

/// [`search_layer`](HnswIndex::search_layer) が visited 集合として要求する
/// 最小インターフェース（#405）。構築経路（[`VisitedScratch`]。世代カウンタ
/// 方式で挿入ごとの O(N) 初期化コストを避ける）と探索経路（[`VisitedBitmap`]。
/// 1 ノード 1 bit でクエリ間の長期保持スクラッチに適する）の 2 実装を同じ
/// `search_layer` から共有するための境界。選定理由の詳細は
/// `docs/design/hnsw-search.md` 参照。
pub(crate) trait VisitedSet {
    /// 呼び出しに先立ちリセットする。`len` は索引の現在のノード数。
    fn reset(&mut self, len: usize);
    /// `id` を訪問済みとして記録する。戻り値・範囲外時の扱いは各実装の
    /// `mark_visited` に合わせる（`Some(既訪問か)`／範囲外は `None`）。
    fn mark_visited(&mut self, id: usize) -> Option<bool>;
    /// `id` の visited スロットを早期に load する（Issue #490。`search_layer`
    /// の受理判定後 prefetch の一部）。読み取りのみ・状態変更なし・範囲外は
    /// 何もしない（fail-closed）ため `&self` で足りる。
    fn prefetch_slot(&self, id: usize);
}

impl VisitedSet for VisitedScratch {
    fn reset(&mut self, len: usize) {
        VisitedScratch::reset(self, len);
    }

    fn mark_visited(&mut self, id: usize) -> Option<bool> {
        VisitedScratch::mark_visited(self, id)
    }

    fn prefetch_slot(&self, id: usize) {
        prefetch::touch_word(self.epoch.get(id));
    }
}

/// [`HnswIndex::search`]（#405）専用の visited 集合（1 ノード 1 bit の
/// ビットマップ方式）。[`VisitedScratch`] の世代カウンタ方式は構築経路の
/// O(N^2) 初期化回避を目的に導入されたものだが、探索経路はクエリごとに
/// 呼ばれ [`HnswSearchScratch`] としてスレッドごとに長期保持される想定のため、
/// メモリ効率（epoch 方式の 8 分の 1）を優先してビットマップを採用する
/// （選定理由の詳細は `docs/design/hnsw-search.md` 参照）。
#[derive(Debug, Default)]
struct VisitedBitmap {
    words: Vec<u64>,
}

impl VisitedBitmap {
    /// `len` ノード分を保持できるよう語数を伸長したうえで全ビットをクリア
    /// する（縮小はしない。呼び出し元が同一スクラッチを異なる索引規模へ
    /// 使い回す想定のため、再確保コストより多少の未使用メモリを許容する）。
    fn reset(&mut self, len: usize) {
        let words_needed = len.div_ceil(64);
        if self.words.len() < words_needed {
            self.words.resize(words_needed, 0);
        }
        for w in self.words.iter_mut() {
            *w = 0;
        }
    }

    /// `id` を訪問済みとして記録する。範囲外の `id` は `None`（呼び出し元は
    /// untrusted 添字アクセスをせず `continue` する。coding-rust.md）。
    fn mark_visited(&mut self, id: usize) -> Option<bool> {
        let word_idx = id / 64;
        let bit_idx = id % 64;
        let word = self.words.get_mut(word_idx)?;
        let mask = 1u64 << bit_idx;
        let already = (*word & mask) != 0;
        *word |= mask;
        Some(already)
    }

    /// `id` が設定済みかを読み取り専用で判定する（状態を変更しない）。範囲外
    /// の `id` は `false`（未確保領域＝未訪問と同義。coding-rust.md の
    /// untrusted 添字アクセス禁止に従い `[]` は使わない）。[`ResumableMaskedSearch`]
    /// （Issue #505）の候補復帰判定（`expanded`／`in_candidates` の照会）専用。
    fn is_set(&self, id: usize) -> bool {
        let word_idx = id / 64;
        let bit_idx = id % 64;
        match self.words.get(word_idx) {
            Some(word) => (*word & (1u64 << bit_idx)) != 0,
            None => false,
        }
    }
}

impl VisitedSet for VisitedBitmap {
    fn reset(&mut self, len: usize) {
        VisitedBitmap::reset(self, len);
    }

    fn mark_visited(&mut self, id: usize) -> Option<bool> {
        VisitedBitmap::mark_visited(self, id)
    }

    fn prefetch_slot(&self, id: usize) {
        prefetch::touch_word(self.words.get(id / 64));
    }
}

/// [`HnswIndex::search_masked_with`] が使う visited 集合の疎な実装（Issue #497。
/// faiss の `VisitedTable` 方式——可視カーディナリティが小さいときは
/// `unordered_set` へ切り替える——を参考にした 3 つめの [`VisitedSet`] 実装）。
/// [`VisitedBitmap`] は毎クエリ `reset` で索引ノード数 N に比例する `N/64` 語の
/// 全クリアを行うため、マスク付き探索で可視候補が索引に対して極小のケースでも
/// N 全体分のコストがかかる。本実装は `HashSet<u32>::clear` （容量は保持し
/// 確保コストを償却する）で `reset` を行い、実際に訪問したノード数にのみ比例
/// させる。切替規則・到達可能性の実測条件は `docs/design/hnsw-search.md`
/// 「visited 集合の 3 実装」節参照。
#[derive(Debug, Default)]
struct VisitedSparse {
    set: HashSet<u32>,
    len: usize,
}

impl VisitedSparse {
    /// `len` ノード分を扱えるようにする。既訪問マークは全て消すが、
    /// `HashSet` の内部確保容量は保持する（クエリをまたいだ確保コストの
    /// 償却。[`VisitedBitmap::reset`] が語配列を伸長のみで縮めないのと
    /// 同じ方針）。
    fn reset(&mut self, len: usize) {
        self.set.clear();
        self.len = len;
    }

    /// `id` を訪問済みとして記録する。範囲外の `id` は `None`（呼び出し元は
    /// untrusted 添字アクセスをせず `continue` する。coding-rust.md）。
    fn mark_visited(&mut self, id: usize) -> Option<bool> {
        if id >= self.len {
            return None;
        }
        let Ok(id_u32) = u32::try_from(id) else {
            return None;
        };
        Some(!self.set.insert(id_u32))
    }
}

impl VisitedSet for VisitedSparse {
    fn reset(&mut self, len: usize) {
        VisitedSparse::reset(self, len);
    }

    fn mark_visited(&mut self, id: usize) -> Option<bool> {
        VisitedSparse::mark_visited(self, id)
    }

    /// `HashSet` はスロットを事前 load できないため no-op（[`VisitedBitmap`]・
    /// [`VisitedScratch`] と異なりハッシュテーブルのバケット位置は
    /// `mark_visited` 自体を呼ばないと分からない）。非受理ノードへ先読みしない
    /// という P0 契約は `search_layer_in` 側の受理判定が担い、本実装には
    /// 影響しない。
    fn prefetch_slot(&self, _id: usize) {}
}

/// [`HnswIndex::search_masked_with`] がどちらの visited 実装を選んだかを表す
/// （Issue #497。診断・統計専用——`sql::hnsw_cache::HnswIndexCacheStats::
/// sparse_visited_searches` の計上に使う。テナント境界・可視カーディナリティ
/// 等の実行時縮退情報は含まない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VisitedKind {
    /// [`VisitedBitmap`]（既定）。
    Dense,
    /// [`VisitedSparse`]（`ValidatedHnswParams::sparse_visited_max` の閾値未満の
    /// 可視候補数でのみ選ばれる）。
    Sparse,
}

/// [`HnswIndex::search_masked`]（Issue #409。Issue #431 是正で意味を拡張）が
/// 受け取る候補マスク。索引ノード（`build` 時に割り当てたノード番号）のうち、
/// 探索経路（貪欲降下・ビーム探索の候補集合展開）へ使ってよいもの＝結果集合へ
/// 含めてよいものを 1 ビット 1 ノードで表す（`docs/design/ann-index-adoption.md`
/// 「RLS／フィルタとの相互作用と折衷案」節の「非可視ノードを探索経路として通過
/// させる設計は不採用」という P0 安全条件により、候補・結果の受理は同一マスクで
/// 統一する）。テナント境界そのものではなく、`sql::hnsw_cache` がクエリ時点の
/// 候補集合（アリーナ）と索引ノードの差分を表現する装置。
///
/// `VisitedBitmap` と同型だが役割が異なる（訪問済み管理 ≠ 受理可否）ため
/// 別の型として持つ。
#[derive(Debug, Clone)]
pub struct NodeMask {
    words: Vec<u64>,
    len: usize,
    /// 設定済みビット数（Issue #497）。`set` の増分でのみ更新する O(1)
    /// カウンタ。`set` 以外に `words` を書き換える経路が無いことを前提に
    /// 同期を保つ（`NodeMask` に unset API は存在しない）。旧実装は
    /// `count_ones()` 呼び出しのたびに `words` を全語走査しており、これは
    /// 避けたいビットマップ visited の `reset` と同じ O(N/64) オーダーで、
    /// 「可視候補数で visited 実装を切り替える」判定にそのまま使うと自己
    /// 矛盾になる（`docs/design/hnsw-search.md`「visited 集合の切替」節）。
    ones: usize,
}

impl NodeMask {
    /// `len` ノード分（すべて false）で初期化する。
    pub fn new(len: usize) -> Self {
        Self {
            words: vec![0u64; len.div_ceil(64)],
            len,
            ones: 0,
        }
    }

    /// `node` を受理対象に加える。範囲外は無視する（呼び出し元が索引の
    /// `len()` 以内の値のみを渡す契約。fail-closed に「何も起きない」側へ倒す）。
    /// 既に設定済みのビットを二重に `set` しても `ones` は増えない。
    pub fn set(&mut self, node: u32) {
        let idx = node as usize;
        if idx >= self.len {
            return;
        }
        let word_idx = idx / 64;
        let bit_idx = idx % 64;
        if let Some(word) = self.words.get_mut(word_idx) {
            let bit = 1u64 << bit_idx;
            if (*word & bit) == 0 {
                self.ones += 1;
            }
            *word |= bit;
        }
    }

    /// `node` が受理対象か（範囲外は `false`。`unwrap`/`[]` を使わない）。
    pub fn get(&self, node: u32) -> bool {
        let idx = node as usize;
        if idx >= self.len {
            return false;
        }
        let word_idx = idx / 64;
        let bit_idx = idx % 64;
        match self.words.get(word_idx) {
            Some(word) => (*word & (1u64 << bit_idx)) != 0,
            None => false,
        }
    }

    /// マスクが表すノード総数（構築時の索引ノード数と一致する契約。
    /// [`HnswIndex::search_masked`] がこの値と `self.len()` の不一致を
    /// `HnswError::InvalidParams` として拒否する）。
    pub fn len(&self) -> usize {
        self.len
    }

    /// マスクが空（`len == 0`）か。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 受理対象に設定されているノード数（O(1)。`self.ones` を返すだけ。
    /// Issue #497: `set` の増分でのみ維持されるため語走査は行わない）。
    pub fn count_ones(&self) -> usize {
        self.ones
    }
}

/// [`HnswIndex::search`]（#405）の呼び出しをまたいで再利用するスクラッチ。
/// 呼び出し元（将来の provider）がスレッドごとに 1 つ所有し、クエリごとに
/// 使い回す想定（モジュール冒頭「ベクトルの所有方針」節と同じ、確保コストを
/// 呼び出し元へ償却させる方針）。`Default` から始めれば初回呼び出しで索引
/// 規模に応じて自動的に伸長する。
///
/// Issue #497 で `sparse`（[`VisitedSparse`]。マスク付き探索の可視候補数が
/// 静的閾値未満のときに選ばれる）・`last_visited_kind`（直近の
/// [`HnswIndex::search_masked_with`] 呼び出しがどちらの visited 実装を
/// 使ったかの診断用記録）を追加した。両フィールドとも private（`pub(crate)`
/// アクセサ [`Self::last_visited_kind`] 経由でのみ読める）。
#[derive(Debug, Default)]
pub struct HnswSearchScratch {
    visited: VisitedBitmap,
    sparse: VisitedSparse,
    last_visited_kind: Option<VisitedKind>,
    /// 直近の [`HnswIndex::search_masked_with_hop`] 呼び出しが `bridge_expand`
    /// 経由で受理・候補化した 2-hop ノード数の累計（Issue #501。診断用）。
    /// `hop == HopMode::OneHop` の呼び出し（既定・`search_masked`/
    /// `search_masked_with` 経由を含む）では常に `0`。早期 `return` 経路
    /// でも `0` にリセットする（`last_visited_kind` と同じ扱い）。
    last_acorn_expansions: u64,
    /// 直近の [`HnswIndex::search_masked_with_hop`] 呼び出しが
    /// [`HnswIndex::greedy_descend_masked`] のブリッジ降下（Issue #680）経由で
    /// 受理・比較した 2-hop ノード数の累計。`hop == HopMode::OneHop` の呼び
    /// 出し（`search_masked_resumable_start` 経由を含む）では常に `0`。
    /// `last_acorn_expansions`（層 0 の `bridge_expand` が数える値）とは
    /// 別カウンタ（Issue #681 の閾値判定が前者の意味に依存するため）。
    last_acorn_descent_bridges: u64,
}

impl HnswSearchScratch {
    /// 直近の [`HnswIndex::search_masked_with`] 呼び出しが選んだ visited
    /// 実装（Issue #497）。呼び出しが早期 `return`（`k == 0`・空索引・
    /// 受理ノードなし等）で層 0 探索まで到達しなかった場合は `None`。
    /// `sql::hnsw_cache` の診断用統計（`sparse_visited_searches`）が使う。
    pub(crate) fn last_visited_kind(&self) -> Option<VisitedKind> {
        self.last_visited_kind
    }

    /// 直近の [`HnswIndex::search_masked_with_hop`] 呼び出しが記録した
    /// ACORN-1 の 2-hop 展開件数（Issue #501）。`sql::hnsw_cache` の診断用
    /// 統計（`acorn_expansions`）が使う。
    pub(crate) fn last_acorn_expansions(&self) -> u64 {
        self.last_acorn_expansions
    }

    /// 直近呼び出しのブリッジ降下 2-hop ノード数（Issue #680。診断用）。
    pub(crate) fn last_acorn_descent_bridges(&self) -> u64 {
        self.last_acorn_descent_bridges
    }

    /// [`Self::last_visited_kind`] の診断専用の薄いラッパー（Issue #498。
    /// `hybrid::sparse_refetch_observed` と同じ非既定 feature `bench-internals`
    /// 限定パターン）。`benches/hnsw_search_bench.rs` が
    /// `sparse_visited_max` の単一ビルド A/B で「意図した visited 実装が
    /// 実際に選ばれたか」を計測フェーズの全呼び出しで検証するために使う。
    /// `Some(true)` は [`VisitedKind::Sparse`]、`Some(false)` は
    /// [`VisitedKind::Dense`]、`None` は層 0 探索まで到達しなかった呼び出し
    /// （早期 return）を表す。`bench-internals` 未指定ビルド（`wire-server`・
    /// 既定の `cargo build -p fandhe-db-engine`）には結線されない。
    #[cfg(feature = "bench-internals")]
    pub fn last_visited_kind_is_sparse(&self) -> Option<bool> {
        self.last_visited_kind
            .map(|k| matches!(k, VisitedKind::Sparse))
    }
}

/// 層 `level` におけるノードの隣接リスト最大次数を返す（層 0 は `2*m`、
/// 層 1 以上は `m`。[`HnswIndex::max_degree`] の内部実装から共有する）。
fn max_degree_for(params: &HnswParams, level: usize) -> usize {
    if level == 0 {
        params.m.saturating_mul(2)
    } else {
        params.m
    }
}

impl GraphBuilder {
    /// 全ノード挿入後の決定的な後始末パス。`insert_node`／`shrink_links` の
    /// `protect` 引数（呼び出し時点のみの保護）だけでは、後続ノードの挿入が
    /// 同じ近傍を再度枝刈りして到達不能ノードを生む残差ケースを閉じきれない
    /// （`docs/design/hnsw-graph-construction.md`「逆方向リンクの到達性保証」
    /// 節参照）。各層でエントリポイントから BFS
    /// し、到達できないノードが残っていれば、その層の到達済み集合中で最も
    /// 近い（`dot` が最大の）ノードへ双方向リンクを追加して修復する。
    ///
    /// # 2 フェーズ構成（計算量の上限。codex-review #423 P1 指摘）
    ///
    /// 検証の過程で、`shrink_links` によるヒューリスティック再選択を修復
    /// バッチとして複数ノードへ一括適用すると、ある未到達ノードを直すための
    /// 枝刈りが**無関係な別の**既存ノードの唯一の到達経路を巻き込んで壊し、
    /// 新たな到達不能ノードを生む whack-a-mole が起こり得ることが分かった。
    /// そのためフェーズ 1 は 1 ノードずつ確定的に修復し、直後に BFS を
    /// やり直して次の未到達ノード（新たに生まれたものを含む）を選ぶ
    /// ワークリスト方式を取る（`shrink_links`・`protect` つきで次数上限を
    /// 維持する厳密な修復）。この「全体 BFS ＋ 到達済み全ノードとの `dot`
    /// 計算」を伴う反復は、旧実装では上限を `member_count` の定数倍として
    /// おり、残差の多い入力（重複ベクトルが多い adversarial な入力等）では
    /// 反復回数・1 反復あたりのコストの双方が入力規模に比例して膨らみ、
    /// 少なくとも O(N^2) 相当となって `MAX_HNSW_NODES`（100 万）まで受理
    /// する構築 API 全体を計算量 DoS にさらしていた。フェーズ 1 の反復回数は
    /// 入力規模に依存しない小さな絶対上限 [`PRECISE_REPAIR_CAP`] に固定し、
    /// それを超えて残る未到達ノードはフェーズ 2 が閉じる。
    ///
    /// フェーズ 2 は残存ノードを id 昇順の**片方向チェーン**（`entry ->
    /// remaining[0] -> remaining[1] -> ...`）として連結するだけで残りを
    /// 閉じる。旧実装（全残存ノードをエントリポイントへ直結）は
    /// (1) `connect` の重複検査（`Vec::contains`）を経てエントリポイントの
    /// 隣接リストが残存ノード数に比例して伸び続け二次関数的コストになる
    /// （Bugbot 指摘）、(2) `shrink_links` を一切呼ばないため次数が
    /// `max_degree` を大幅に超え得る（codex-review #423 P1 指摘）、という
    /// 2 つの問題を持っていた。チェーン方式では各ノードが新たに得る次数は
    /// 高々 1（チェーンの「出発点」役を一度だけ務める）なので、
    /// `connect` 直後に `shrink_links` を掛けても 1 ノードあたり
    /// O(`max_degree`) に収まり、全体で O(remaining.len()) を保ったまま
    /// 次数上限も維持できる。`shrink_links` は「次数が上限を超えていれば
    /// `protect` を強制的に残しつつヒューリスティックで上限内へ再選択し、
    /// 超えていなければ何もしない」契約（同関数のドキュメンテーション
    /// コメント参照）を持つため、チェーンの起点を entry の現在の次数に
    /// 関わらず常に選べる（"余裕があるか" を事前に走査する必要がなく、
    /// 失敗しうる分岐も生まれない）。entry への `shrink_links` 適用が
    /// entry の既存リンクを 1 本犠牲にし得る点は、フェーズ 1 が到達済み
    /// 任意ノードへ毎回同じ `shrink_links` を適用しているのと同じ性質の
    /// リスクであり、新たに導入するものではない。フェーズ 1 の上限を
    /// 入力非依存の定数に保つことで、層あたりの
    /// 総コストは O(`PRECISE_REPAIR_CAP` * N + N) に収まり、`MAX_LEVEL` も
    /// 定数上限（32）であるため `HnswIndex::build` 全体では入力規模に対し
    /// ほぼ線形（N log N 契約の範囲内）に収まる。
    /// `threads` は最近傍探索フェーズ（下記 [`repair_reachability_inner`]
    /// (Self::repair_reachability_inner) 「Issue #449」節）の並列度上限。
    /// 呼び出し元（`build_inner`・`parallel_build::freeze`）が
    /// [`HnswIndex::build_with_threads`] 等から引き継いだ構築スレッド数を
    /// そのまま渡す契約（`WorkerBudgetGuard` の追加取得は行わない——下記
    /// [`repair_reachability_inner`](Self::repair_reachability_inner)
    /// 「Issue #449」節の「予算引き継ぎ方針」参照）。
    fn repair_reachability(
        &mut self,
        dim: usize,
        vectors: &[f32],
        threads: usize,
    ) -> Result<(), HnswError> {
        self.repair_reachability_inner::<false>(dim, vectors, threads)
            .map(|_| ())
    }

    /// [`repair_reachability`](Self::repair_reachability) の観測版（Issue #447:
    /// 修復対象ノード数・反復回数の観測フック）。層ごとの到達不能ノード数・
    /// フェーズ 1 反復回数・フェーズ 2 結線数・段別壁時間を
    /// [`HnswRepairStats`] として返す。非観測経路（`build`・
    /// `build_with_threads`・`parallel_build::freeze`）は
    /// [`repair_reachability`](Self::repair_reachability) を呼ぶため本メソッドの
    /// 計装コストを一切負わない。`threads` の意味は
    /// [`repair_reachability`](Self::repair_reachability) と同一。
    fn repair_reachability_observed(
        &mut self,
        dim: usize,
        vectors: &[f32],
        threads: usize,
    ) -> Result<HnswRepairStats, HnswError> {
        self.repair_reachability_inner::<true>(dim, vectors, threads)
    }

    /// 全ノード挿入後の決定的な後始末パス。`insert_node`／`shrink_links` の
    /// `protect` 引数（呼び出し時点のみの保護）だけでは、後続ノードの挿入が
    /// 同じ近傍を再度枝刈りして到達不能ノードを生む残差ケースを閉じきれない
    /// （`docs/design/hnsw-graph-construction.md`「逆方向リンクの到達性保証」
    /// 節参照）。各層でエントリポイントから BFS
    /// し、到達できないノードが残っていれば、その層の到達済み集合中で最も
    /// 近い（`dot` が最大の）ノードへ双方向リンクを追加して修復する。
    ///
    /// # 2 フェーズ構成（計算量の上限。codex-review #423 P1 指摘）
    ///
    /// 検証の過程で、`shrink_links` によるヒューリスティック再選択を修復
    /// バッチとして複数ノードへ一括適用すると、ある未到達ノードを直すための
    /// 枝刈りが**無関係な別の**既存ノードの唯一の到達経路を巻き込んで壊し、
    /// 新たな到達不能ノードを生む whack-a-mole が起こり得ることが分かった。
    /// そのためフェーズ 1 は 1 ノードずつ確定的に修復し、直後に BFS を
    /// やり直して次の未到達ノード（新たに生まれたものを含む）を選ぶ
    /// ワークリスト方式を取る（`shrink_links`・`protect` つきで次数上限を
    /// 維持する厳密な修復）。この「全体 BFS ＋ 到達済み全ノードとの `dot`
    /// 計算」を伴う反復は、旧実装では上限を `member_count` の定数倍として
    /// おり、残差の多い入力（重複ベクトルが多い adversarial な入力等）では
    /// 反復回数・1 反復あたりのコストの双方が入力規模に比例して膨らみ、
    /// 少なくとも O(N^2) 相当となって `MAX_HNSW_NODES`（100 万）まで受理
    /// する構築 API 全体を計算量 DoS にさらしていた。フェーズ 1 の反復回数は
    /// 入力規模に依存しない小さな絶対上限 [`PRECISE_REPAIR_CAP`] に固定し、
    /// それを超えて残る未到達ノードはフェーズ 2 が閉じる。
    ///
    /// フェーズ 2 は残存ノードを id 昇順の**片方向チェーン**（`entry ->
    /// remaining[0] -> remaining[1] -> ...`）として連結するだけで残りを
    /// 閉じる。旧実装（全残存ノードをエントリポイントへ直結）は
    /// (1) `connect` の重複検査（`Vec::contains`）を経てエントリポイントの
    /// 隣接リストが残存ノード数に比例して伸び続け二次関数的コストになる
    /// （Bugbot 指摘）、(2) `shrink_links` を一切呼ばないため次数が
    /// `max_degree` を大幅に超え得る（codex-review #423 P1 指摘）、という
    /// 2 つの問題を持っていた。チェーン方式では各ノードが新たに得る次数は
    /// 高々 1（チェーンの「出発点」役を一度だけ務める）なので、
    /// `connect` 直後に `shrink_links` を掛けても 1 ノードあたり
    /// O(`max_degree`) に収まり、全体で O(remaining.len()) を保ったまま
    /// 次数上限も維持できる。`shrink_links` は「次数が上限を超えていれば
    /// `protect` を強制的に残しつつヒューリスティックで上限内へ再選択し、
    /// 超えていなければ何もしない」契約（同関数のドキュメンテーション
    /// コメント参照）を持つため、チェーンの起点を entry の現在の次数に
    /// 関わらず常に選べる（"余裕があるか" を事前に走査する必要がなく、
    /// 失敗しうる分岐も生まれない）。entry への `shrink_links` 適用が
    /// entry の既存リンクを 1 本犠牲にし得る点は、フェーズ 1 が到達済み
    /// 任意ノードへ毎回同じ `shrink_links` を適用しているのと同じ性質の
    /// リスクであり、新たに導入するものではない。フェーズ 1 の上限を
    /// 入力非依存の定数に保つことで、層あたりの
    /// 総コストは O(`PRECISE_REPAIR_CAP` * N + N) に収まり、`MAX_LEVEL` も
    /// 定数上限（32）であるため `HnswIndex::build` 全体では入力規模に対し
    /// ほぼ線形（N log N 契約の範囲内）に収まる。
    ///
    /// # 観測分離（`OBSERVE`。Issue #447）
    ///
    /// `OBSERVE` を `const` ジェネリックにすることで、`OBSERVE=false`
    /// （[`repair_reachability`](Self::repair_reachability) 経由。`build`・
    /// `build_with_threads`・`parallel_build::freeze` が使う非観測経路）では
    /// 単相化によって計測分岐・`Instant::now()`・カウンタ更新のコードが
    /// 一切残らない（PR #445 の `BuildGraph::observe` 分岐と同じ方針）。
    /// `OBSERVE=true`（[`repair_reachability_observed`]
    /// (Self::repair_reachability_observed) 経由）でのみ [`HnswRepairStats`]
    /// を採取する。グラフ操作の順序・比較・タイブレークは `OBSERVE` の値に
    /// 関わらず完全に同一（観測が挙動へ影響しない）。
    /// 入力非依存の定数に保つことで、層あたりの
    /// 総コストは O(`PRECISE_REPAIR_CAP` * N + N) に収まり、`MAX_LEVEL` も
    /// 定数上限（32）であるため `HnswIndex::build` 全体では入力規模に対し
    /// ほぼ線形（N log N 契約の範囲内）に収まる。
    ///
    /// # 観測分離（`OBSERVE`。Issue #447）
    ///
    /// `OBSERVE` を `const` ジェネリックにすることで、`OBSERVE=false`
    /// （[`repair_reachability`](Self::repair_reachability) 経由。`build`・
    /// `build_with_threads`・`parallel_build::freeze` が使う非観測経路）では
    /// 単相化によって計測分岐・`Instant::now()`・カウンタ更新のコードが
    /// 一切残らない（PR #445 の `BuildGraph::observe` 分岐と同じ方針）。
    /// `OBSERVE=true`（[`repair_reachability_observed`]
    /// (Self::repair_reachability_observed) 経由）でのみ [`HnswRepairStats`]
    /// を採取する。グラフ操作の順序・比較・タイブレークは `OBSERVE` の値に
    /// 関わらず完全に同一（観測が挙動へ影響しない）。
    ///
    /// # 探索の並列化（`threads`。Issue #449）
    ///
    /// フェーズ 1 の各反復が行う「到達済み集合内の最近傍探索」（`dot` を
    /// 到達済みノード数だけ計算する読み取り専用の走査）を、`threads` を
    /// 上限に [`nearest_reachable`] へ分割・並列実行させる。修復先の決定
    /// （`connect`／`shrink_links` の可変更新）自体は本メソッドが逐次のまま
    /// 適用するため、`&mut self` の借用規則を破らずに済む（探索＝不変借用の
    /// 読み取り専用ヘルパ、結線＝可変借用の逐次適用、という 2 相構成）。
    ///
    /// 並列度は [`repair_workers_for`] が
    /// `crate::parallel_search::thread_count_for`（検索側の並列度決定と同一
    /// 関数）を経由して決める——`threads==1`（`build`・`build_with_threads`
    /// の縮退経路）では常に 1 に縮退し、[`nearest_reachable`] はワーカーを
    /// 一切起動しない逐次経路のみを通る。`WorkerBudgetGuard`
    /// （`parallel_search.rs`）の追加取得はここでは行わない——呼び出し元
    /// （`HnswIndex::build_parallel`／`build_parallel_with_precision`）が
    /// 構築全体（並列挿入フェーズを含む）にわたって保持済みの予算を
    /// `threads` としてそのまま引き継ぐ契約であり、二重に予算を計上しない
    /// ため（`build_with_threads`〔明示スレッド数指定〕の並列挿入フェーズも
    /// 同様に追加取得なしで `threads` 本を起動する既存契約に揃えた）。
    ///
    /// 探索フェーズの比較・タイブレークはモジュール冒頭「順序規約」（スコア
    /// `total_cmp` 降順・同点は id 昇順）に従い、この規約は全順序を成す
    /// （[`better_repair_candidate`] 参照）。全順序であることから、到達済み
    /// 集合をどう分割し・各ワーカーの局所最良をどの順序で縮約しても、
    /// 最終的に選ばれる修復先ノードは分割・縮約の順序に依存せず一意に
    /// 定まる——`threads` の値によらず本メソッドが返すグラフはビット同一
    /// になる（`docs/design/rrf-tie-break-determinism.md`「維持すべき不変
    /// 条件」と同方針。`crates/engine/src/hnsw.rs` 内 `#[cfg(test)] mod tests`
    /// の `repair_reachability_inner` 完全一致テストで機械検証する）。
    ///
    /// # 冗長な BFS の省略（Issue #449）
    ///
    /// フェーズ 1 の各反復は必ず BFS（[`bfs_reachable_mask`]）から始まる。
    /// フェーズ 1 が「未到達ノードが見つからず `break`」で終わった場合、
    /// その `break` 直前に計算した BFS 結果はグラフを一切変更していない
    /// 状態のまま得られたものであり、フェーズ 2 が使う到達集合と完全に
    /// 一致する（フェーズ 1・フェーズ 2 とも同じ `entry` から同じグラフに
    /// 対して BFS するため）。よってこの場合はフェーズ 2 の BFS を再実行
    /// せず、フェーズ 1 最終反復の結果をそのまま使い回す。逆に、フェーズ 1
    /// が反復回数の上限まで完走した場合は最終反復で必ず結線（`connect`／
    /// `shrink_links`）が起きているため、フェーズ 2 は BFS を再実行して
    /// グラフの最新状態を反映する（`mutated_since_bfs` フラグで判定）。
    /// グラフの出力自体はこの省略の前後で変わらない（省略するのは「変更が
    /// 無いと分かっている再計算」のみ）。
    fn repair_reachability_inner<const OBSERVE: bool>(
        &mut self,
        dim: usize,
        vectors: &[f32],
        threads: usize,
    ) -> Result<HnswRepairStats, HnswError> {
        let inner_start = if OBSERVE {
            Some(std::time::Instant::now())
        } else {
            None
        };
        let mut stats = HnswRepairStats::default();

        let Some(entry) = self.entry_point else {
            return Ok(stats);
        };
        let Some(max_level) = self.level_of(entry) else {
            return Ok(stats);
        };
        for level in 0..=max_level {
            let mut level_stats = HnswRepairLevelStats {
                level,
                ..HnswRepairLevelStats::default()
            };
            let phase1_start = if OBSERVE {
                Some(std::time::Instant::now())
            } else {
                None
            };
            // 当該層に属するノード id（昇順）を層ごとに 1 回だけ構築する
            // （Issue #449: 毎反復・フェーズ 2 の `(0..len).filter(level_of
            // >= level)` 全走査を層ごとに 1 回へ削減。集合としての内容は
            // 従来の毎回フィルタと同一）。
            let members: Vec<u32> = (0..self.nodes.len() as u32)
                .filter(|&n| self.level_of(n).map(|l| l >= level).unwrap_or(false))
                .collect();

            // フェーズ 1: 全体 BFS ＋ 到達済み全ノードとの `dot` 計算を伴う
            // 厳密な修復を `PRECISE_REPAIR_CAP` 回までに限定する。
            let mut phase1_completed = 0usize;
            // フェーズ 1 の最終反復で得た到達集合（フェーズ 2 の冗長 BFS
            // 省略に使う。上記ドキュメンテーションコメント「冗長な BFS の
            // 省略」参照）。
            let mut last_mask: Option<NodeMask> = None;
            let mut mutated_since_bfs = false;
            for iter in 0..PRECISE_REPAIR_CAP {
                let mask = self.bfs_reachable_mask(level, entry);
                mutated_since_bfs = false;

                // `members` を 1 回だけ走査し、到達済み部分列（`reachable`。
                // 最近傍探索の候補集合）と最初の未到達ノードを同時に確定する
                // （Issue #449: 従来の 2 回の独立した `filter` 走査を統合）。
                let mut reachable: Vec<u32> = Vec::with_capacity(members.len());
                let mut missing_node: Option<u32> = None;
                let mut unreachable_count: u64 = 0;
                for &m in &members {
                    if mask.get(m) {
                        reachable.push(m);
                    } else {
                        if missing_node.is_none() {
                            missing_node = Some(m);
                        }
                        unreachable_count += 1;
                    }
                }
                if OBSERVE && iter == 0 {
                    // フェーズ 1 の最初の反復で得られる BFS 結果をそのまま
                    // 流用して到達不能ノード数を数える（追加の BFS を
                    // 入れない。この走査自体の時間は `phase1_wall` に含める
                    // ——`unreachable_before` のドキュメンテーションコメント
                    // 参照）。
                    level_stats.unreachable_before = unreachable_count;
                }
                last_mask = Some(mask);

                let Some(node) = missing_node else { break };

                // 到達済み集合内の最近傍探索（読み取り専用・Issue #449 で
                // 並列化対象。`&mut self` を要する結線はこの下の `if let
                // Some` 内でのみ行う）。
                let workers = repair_workers_for(reachable.len(), threads);
                let best = nearest_reachable(vectors, dim, node, &reachable, workers)?;
                // `reachable` は entry 自身を含むため必ず 1 件以上存在し、`best`
                // は常に `Some` になる（entry 自身が候補になり得る）。`None` は
                // `reachable` が空という到達不能な状態であり、fail-closed で
                // 何もしない（次の反復の BFS が変化のないまま同じ `node` を
                // 選び続けることになるが、フェーズ 1 の反復回数上限で有限に
                // 打ち切られ、フェーズ 2 が確定的に閉じる）。
                if let Some((target, _)) = best {
                    self.connect(node, target, level);
                    self.connect(target, node, level);
                    self.shrink_links(target, level, dim, vectors, node)?;
                    self.shrink_links(node, level, dim, vectors, target)?;
                    mutated_since_bfs = true;
                }
                phase1_completed = iter + 1;
            }
            if OBSERVE {
                level_stats.phase1_iterations = phase1_completed as u64;
                level_stats.phase1_cap_hit = phase1_completed >= PRECISE_REPAIR_CAP;
                if let Some(start) = phase1_start {
                    level_stats.phase1_wall = start.elapsed();
                }
            }

            let phase2_start = if OBSERVE {
                Some(std::time::Instant::now())
            } else {
                None
            };
            // フェーズ 2: フェーズ 1 の絶対上限までで解消しなかった残りを、
            // 上記モジュールコメントのとおり id 昇順の片方向チェーンで
            // 確定的に閉じる。`remaining` は `members`（既に昇順）由来なので
            // 既に決定的な id 昇順である。
            //
            // `mutated_since_bfs` が立っていなければ、フェーズ 1 最終反復の
            // BFS（`last_mask`）以降グラフは変化していないため、この BFS
            // 結果をそのまま使い回す（上記「冗長な BFS の省略」参照）。
            // `PRECISE_REPAIR_CAP > 0` なのでループは必ず 1 回以上実行され、
            // `last_mask` は常に `Some`。
            let mask = if mutated_since_bfs {
                self.bfs_reachable_mask(level, entry)
            } else {
                last_mask.unwrap_or_else(|| self.bfs_reachable_mask(level, entry))
            };
            let remaining: Vec<u32> = members.iter().copied().filter(|n| !mask.get(*n)).collect();
            if OBSERVE {
                level_stats.phase2_nodes = remaining.len() as u64;
            }
            if let Some((&head, tail)) = remaining.split_first() {
                // チェーンは entry を起点にする: `entry -> head -> tail[0]
                // -> tail[1] -> ...`。`entry -> head` の 1 本だけが「既に
                // 到達済みのノード（entry 自身）」の隣接リストを変更する
                // 危険な結線であり、それ以降の `tail` への結線はすべて
                // 「直前まで未到達だった（＝他ノードの到達性に寄与しない）
                // orphan 同士」の結線なので安全（下記ループのコメント参照）。
                //
                // `entry` は他の到達済みノードへの唯一の到達経路を握って
                // いる場合があるため、`shrink_links(entry, ...)` が次数
                // 超過を解消する際に既存リンクを 1 本犠牲にすると、その
                // 犠牲先ノードが到達不能に戻り得る（Phase 1 はこれを
                // 「1 ノードずつ直して BFS をやり直す」ワークリスト方式で
                // 検知・再修復するが、Phase 2 は計算量上限のためそれをしない
                // 設計）。そのためここだけは特別に、`shrink_links` 適用前後
                // で entry の隣接集合を比較し、犠牲になったノード（あれば
                // 高々 1 件。`shrink_links` は次数超過分の 1 件しか削らない）
                // をチェーンの末尾へ追加で連結し直すことで、この 1 箇所の
                // リスクだけを O(1) の追加コストで確定的に解消する。
                let old_entry_links: Vec<u32> = self
                    .neighbors(level, entry)
                    .map(|links| links.to_vec())
                    .unwrap_or_default();
                self.connect(entry, head, level);
                self.shrink_links(entry, level, dim, vectors, head)?;
                let evicted = self.neighbors(level, entry).and_then(|new_links| {
                    old_entry_links
                        .into_iter()
                        .find(|old| !new_links.contains(old))
                });

                let mut prev = head;
                for &node in tail {
                    // `connect(prev, node, level)` は `prev` 自身の隣接
                    // リストのみを伸ばす（`node` 側は変化しない。モジュール
                    // 冒頭のノード表現）。`prev` はこの時点でまだ
                    // 未到達だったノード（またはチェーンの `head`）であり、
                    // 未到達ノードの「自身の」隣接リストは（BFS が一度も
                    // 辿っていないため）他ノードの到達性に寄与していない。
                    // よってここで `shrink_links(prev, ...)` が `prev` の
                    // 既存リンクを 1 本犠牲にしても安全。
                    self.connect(prev, node, level);
                    self.shrink_links(prev, level, dim, vectors, node)?;
                    prev = node;
                }

                // entry の shrink で犠牲になったノードがあれば、チェーンの
                // 末尾（= 直前まで未到達だった orphan）から結線し直す。
                // `prev` は orphan なので、ここでの `shrink_links` も上記と
                // 同じ理由で安全。
                if let Some(evicted) = evicted {
                    self.connect(prev, evicted, level);
                    self.shrink_links(prev, level, dim, vectors, evicted)?;
                    if OBSERVE {
                        level_stats.phase2_entry_relinked = 1;
                    }
                }
            }
            if OBSERVE {
                if let Some(start) = phase2_start {
                    level_stats.phase2_wall = start.elapsed();
                }
                stats.levels.push(level_stats);
            }
        }
        if OBSERVE {
            if let Some(start) = inner_start {
                stats.wall = start.elapsed();
            }
        }
        Ok(stats)
    }

    /// 層 `level` 上でノード `start` からリンクを辿って到達可能なノード集合を
    /// ビットマップで返す（`repair_reachability` 専用の内部 BFS。`tests/hnsw.rs`
    /// は公開 API `neighbors` を使い同等の BFS を独立に実装して検証する）。
    ///
    /// Issue #449: 到達集合の表現を `HashSet<u32>`（SipHash によるハッシュ
    /// コスト・エントリごとのヒープ確保）から [`NodeMask`]（1 ノード 1 bit の
    /// ビットマップ）＋ `Vec<u32>` キューへ置換した。BFS が辿る到達可能
    /// ノードの**集合そのもの**は不変（訪問順・到達判定ロジックは変えて
    /// いない）ため、呼び出し元（`repair_reachability_inner`）が導く修復結果
    /// は表現変更の前後で完全に一致する。
    fn bfs_reachable_mask(&self, level: usize, start: u32) -> NodeMask {
        let mut visited = NodeMask::new(self.nodes.len());
        let mut queue: VecDeque<u32> = VecDeque::new();
        visited.set(start);
        queue.push_back(start);
        while let Some(node) = queue.pop_front() {
            if let Some(neighbors) = self.neighbors(level, node) {
                for &n in neighbors {
                    if !visited.get(n) {
                        visited.set(n);
                        queue.push_back(n);
                    }
                }
            }
        }
        visited
    }

    /// 1 ノードをグラフへ挿入する（Algorithm 1 相当）。探索段（不変参照のみ）と
    /// 結線段（可変）を関数分離しておくのは、#406（並列構築）が要素単位ロックへ
    /// 差し替える際にこの境界をそのまま流用できるようにするため。
    fn insert_node(
        &mut self,
        node_id: u32,
        level: usize,
        dim: usize,
        vectors: &[f32],
        visited: &mut VisitedScratch,
    ) -> Result<(), HnswError> {
        self.nodes.push(Node {
            level,
            links: vec![Vec::new(); level + 1],
        });

        let query = node_vector(vectors, dim, node_id)?;

        let (current_entry, top_level) = match self.entry_point {
            Some(ep) => {
                let ep_level = self.level_of(ep).unwrap_or(0);
                (ep, ep_level)
            }
            None => {
                self.entry_point = Some(node_id);
                return Ok(());
            }
        };

        // (i) 上位層（挿入ノードのレベルより上）を ef=1 の貪欲降下でたどり、
        // 挿入ノードのレベル直上での最近傍 1 件をエントリポイントとして絞り込む。
        let mut nearest = current_entry;
        if top_level > level {
            for l in ((level + 1)..=top_level).rev() {
                nearest = self.greedy_descend(nearest, query, l, dim, vectors)?;
            }
        }

        // (ii) 挿入ノードのレベル以下の各層で ef_construction 幅の探索 →
        // ヒューリスティック近傍選択 → 双方向リンク。
        let mut entry_candidates = vec![nearest];
        for l in (0..=level.min(top_level)).rev() {
            let candidates = self.search_layer(
                entry_candidates.clone(),
                query,
                self.params.ef_construction,
                l,
                dim,
                vectors,
                visited,
                None,
            )?;
            // 層 0 は次数上限が最大 2*m まで許容される（`shrink_links` が参照する
            // `max_degree_for` 側で扱う）が、新規ノード自身の選択本数は Algorithm 1
            // の記法どおり層を問わず常に `m` 本にする。
            let selected =
                select_neighbors_heuristic_free(&candidates, self.params.m, dim, vectors)?;

            for &neighbor in &selected {
                self.connect(node_id, neighbor, l);
                self.connect(neighbor, node_id, l);
                self.shrink_links(neighbor, l, dim, vectors, node_id)?;
            }

            entry_candidates = if candidates.is_empty() {
                vec![nearest]
            } else {
                candidates.iter().map(|c| c.node).collect()
            };
        }

        // (vi) 挿入ノードのレベルが現行最大層を超えるならエントリポイントを更新する。
        if level > top_level {
            self.entry_point = Some(node_id);
        }

        Ok(())
    }

    /// `ef=1` の貪欲降下（Algorithm 2 の `ef=1` 特殊形。上位層のナビゲーション用）。
    fn greedy_descend(
        &self,
        start: u32,
        query: &[f32],
        level: usize,
        dim: usize,
        vectors: &[f32],
    ) -> Result<u32, HnswError> {
        let mut current = start;
        let mut current_best = ScoredNode {
            node: current,
            score: self.score(current, query, dim, vectors)?,
        };
        loop {
            let mut improved = false;
            if let Some(neighbors) = self.neighbors(level, current) {
                for &cand in neighbors {
                    let cand_scored = ScoredNode {
                        node: cand,
                        score: self.score(cand, query, dim, vectors)?,
                    };
                    // スコアのみでなく `ScoredNode::cmp`（スコア降順・同点は id 昇順）
                    // で比較する。同点時にモジュール冒頭の順序契約から外れないため。
                    if cand_scored > current_best {
                        current = cand;
                        current_best = cand_scored;
                        improved = true;
                    }
                }
            }
            if !improved {
                break;
            }
        }
        Ok(current)
    }

    /// `from -> to` への単方向リンクを層 `level` へ追加する（重複・自己ループは
    /// 追加しない）。次数上限の適用は呼び出し元の [`Self::shrink_links`] が担う。
    fn connect(&mut self, from: u32, to: u32, level: usize) {
        if from == to {
            return;
        }
        let Some(node) = self.nodes.get_mut(from as usize) else {
            return;
        };
        let Some(links) = node.links.get_mut(level) else {
            return;
        };
        if !links.contains(&to) {
            links.push(to);
        }
    }

    /// `node` の層 `level` における隣接数が次数上限を超えていれば、その隣接集合
    /// 全体へヒューリスティック近傍選択を再適用して上限内へ縮退させる
    /// （Algorithm 1 の「次数上限超過時の再選択」段）。
    ///
    /// `protect` は、この呼び出し直前に `insert_node` が `node <-> protect` へ
    /// 張ったばかりの逆方向リンク先（新規挿入ノード自身）。ヒューリスティックが
    /// `protect` を枝刈りしてしまうと、新規ノードへの唯一の入口だった逆方向
    /// リンクが失われ、エントリポイントからの到達路が残らないまま孤立し得る
    /// （挿入ノードは自身の外向きリンクは持つが、探索はエントリポイントから
    /// 既存ノードの隣接リストを辿って到達するため入方向のリンクが要る）。
    /// ヒューリスティック選択後に `protect` が漏れて
    /// いれば、選択済み集合中で最もスコアが低い（＝末尾の）要素と差し替えて
    /// 強制的に残す。
    ///
    /// この保証は「`protect` の挿入時点で選ばれた各近傍が `protect` への逆方向
    /// リンクを保持する」ことのみを担保する insertion-time の不変条件であり、
    /// 後続の別ノード挿入がこれらの近傍を再度 `shrink_links` する際に `protect`
    /// が漏れる余地までは塞がない（グローバルな到達性の恒久保証ではない）。
    /// 全ノード挿入後に残るその残差ケースは `HnswIndex::build` 末尾の
    /// [`Self::repair_reachability`] が閉じる（この関数自体は呼ばない。
    /// 呼ぶと、他の未到達ノードを直すための枝刈りが無関係な第三のノードの
    /// 唯一の到達経路を巻き込んで壊す whack-a-mole が起こり得るため）。
    /// 詳細は `docs/design/hnsw-graph-construction.md`
    /// 「逆方向リンクの到達性保証」節参照。
    fn shrink_links(
        &mut self,
        node: u32,
        level: usize,
        dim: usize,
        vectors: &[f32],
        protect: u32,
    ) -> Result<(), HnswError> {
        let limit = max_degree_for(&self.params, level);
        let current_links: Vec<u32> = self
            .nodes
            .get(node as usize)
            .and_then(|n| n.links.get(level))
            .cloned()
            .unwrap_or_default();
        // 縮退の計算本体（読み取り→再選択）は `compute_shrink` という純粋関数に
        // 切り出し済み。並列構築（`parallel_build`。Issue #406）は書き込みロック
        // 1 回の中で「現在のリンクを読む→`compute_shrink`→書き戻す」を原子的に
        // 行うことで同じ計算を共有する（`docs/design/hnsw-parallel-build.md`
        // 参照）。逐次経路（本メソッド）はロック不要のため読み→計算→書き込みを
        // そのまま `self.nodes` への 2 回のアクセスとして行う。
        if let Some(shrunk) = compute_shrink(&current_links, node, dim, vectors, limit, protect)? {
            if let Some(n) = self.nodes.get_mut(node as usize) {
                if let Some(links) = n.links.get_mut(level) {
                    *links = shrunk;
                }
            }
        }
        Ok(())
    }

    /// `dot(node, query)`。[`HnswIndex::score`] と同じ委譲（`GraphBuilder`
    /// は `vectors` を保持しないため、呼び出し元から渡された `vectors` を
    /// そのまま [`score_of`] へ渡す）。
    fn score(
        &self,
        node: u32,
        query: &[f32],
        dim: usize,
        vectors: &[f32],
    ) -> Result<f32, HnswError> {
        score_of(vectors, dim, node, query)
    }

    /// 構築経路の `search_layer`（[`HnswIndex::search_layer`] と同型。
    /// 構築中は常にパイプライン prefetch（[`prefetch::PipelinePrefetch`]）を
    /// 使う。[`search_layer_in`] の型パラメータ `A` を `Self` に単相化する
    /// だけの薄い委譲。
    #[allow(clippy::too_many_arguments)]
    fn search_layer<V: VisitedSet>(
        &self,
        entry_points: Vec<u32>,
        query: &[f32],
        ef: usize,
        level: usize,
        dim: usize,
        vectors: &[f32],
        visited: &mut V,
        accept: Option<&NodeMask>,
    ) -> Result<Vec<ScoredNode>, HnswError> {
        // 構築経路は常に `HopMode::OneHop`（ACORN-1・Issue #501 の 2-hop 展開は
        // マスク付き探索限定。§ `HopMode` ドキュメンテーションコメント参照）。
        let mut acorn_expansions = 0u64;
        search_layer_in(
            self,
            entry_points,
            query,
            ef,
            level,
            dim,
            vectors,
            visited,
            accept,
            &prefetch::PipelinePrefetch,
            HopMode::OneHop,
            &mut acorn_expansions,
        )
    }
}

impl HnswIndex {
    /// row-major 連続バッファ（`VectorArena::vectors()` と同レイアウト。
    /// `vectors[node * dim .. node * dim + dim]` が `node` 番目のベクトル）から
    /// 単一スレッドで構築する。
    ///
    /// 検証順序: パラメータ → 次元整合 → ノード数上限 → 非有限値。空入力
    /// （`vectors` が空）は空索引を返す（エラーにしない。呼び出し元が未挿入の
    /// テーブルへ構築を試みる自然なケースのため）。
    pub fn build(
        params: HnswParams,
        dim: u32,
        vectors: &[f32],
        seed: u64,
    ) -> Result<Self, HnswError> {
        Self::build_inner::<false>(params, ResidentPrecision::F32, dim, vectors, seed)
            .map(|(index, _)| index)
    }

    /// [`Self::build`] の常駐精度 opt-in 版（Issue #514・親 #513。ポインタ:
    /// TASK-132・TASK-156・CORE-16）。`precision` に [`ResidentPrecision::F16`]
    /// を指定すると、凍結時（[`Self::freeze_from`]）に索引ノードを f16 常駐へ
    /// エンコードする。1 成分でも f16 の有限範囲（`|x| <= 65504.0`）を超える
    /// 場合は `F32` へ自動縮退する（`Self::resident_precision` が実効値を返す。
    /// D6）。グラフ構築（挿入・修復）は精度によらず常に f32 で行う
    /// （`docs/design/hnsw-f16-resident.md` 参照。決定 7）。
    pub fn build_with_precision(
        params: HnswParams,
        precision: ResidentPrecision,
        dim: u32,
        vectors: &[f32],
        seed: u64,
    ) -> Result<Self, HnswError> {
        Self::build_impl(params, precision, dim, vectors, seed)
    }

    /// [`Self::build`]／[`Self::build_with_precision`] が共有する構築本体
    /// （`OBSERVE=false` に固定した [`build_inner`](Self::build_inner) の薄い
    /// ラッパー）。
    fn build_impl(
        params: HnswParams,
        precision: ResidentPrecision,
        dim: u32,
        vectors: &[f32],
        seed: u64,
    ) -> Result<Self, HnswError> {
        Self::build_inner::<false>(params, precision, dim, vectors, seed).map(|(index, _)| index)
    }

    /// [`build`](Self::build) と同一アルゴリズムを実行しつつ、
    /// `repair_reachability` の観測統計（[`HnswRepairStats`]。Issue #447）を
    /// あわせて返す。常駐精度は常に [`ResidentPrecision::F32`]（呼び出し元の
    /// `build_with_threads_observed` の縮退経路が threads=1 基線を得るために
    /// 使う唯一の呼び出し元で、精度 opt-in（Issue #514）とは無関係）。返す
    /// グラフは [`build`](Self::build) と完全に同一（`OBSERVE` は計測有無の
    /// みを切り替え、グラフ操作へは一切影響しない。`build_inner` の
    /// ドキュメンテーションコメント参照）。
    fn build_observed(
        params: HnswParams,
        dim: u32,
        vectors: &[f32],
        seed: u64,
    ) -> Result<(Self, HnswRepairStats), HnswError> {
        Self::build_inner::<true>(params, ResidentPrecision::F32, dim, vectors, seed)
    }

    /// [`build`](Self::build)・[`build_with_precision`]
    /// (Self::build_with_precision)・[`build_observed`](Self::build_observed)
    /// が共有する本体（Issue #447・Issue #514）。`OBSERVE=false` では
    /// `repair_reachability_inner::<false>` の単相化により計測分岐が消え、
    /// `build` は Issue #447 追加前と完全に同一の命令列になる。`OBSERVE=true`
    /// では層別の到達不能ノード数・反復回数を [`HnswRepairStats`] として
    /// 併せて返す。`precision` は凍結時（[`Self::freeze_from`]）の索引ノード
    /// 常駐表現にのみ影響し（Issue #514）、グラフ構築（挿入・修復）は精度に
    /// よらず常に f32 で行う。
    fn build_inner<const OBSERVE: bool>(
        params: HnswParams,
        precision: ResidentPrecision,
        dim: u32,
        vectors: &[f32],
        seed: u64,
    ) -> Result<(Self, HnswRepairStats), HnswError> {
        let dim_usize = dim as usize;
        let n = validate_build_input(&params, dim, vectors)?;

        // `vectors` の不変スナップショットを取り、以降 `search` はこれのみを
        // 参照する（モジュール冒頭「ベクトルの所有方針」節・codex-review PR
        // #430 P1 指摘対応。呼び出し元が構築後に借用元バッファを書き換えても
        // この Arc の中身は変化しない）。
        let owned_vectors: Arc<[f32]> = Arc::from(vectors);
        let mut builder = GraphBuilder {
            params,
            nodes: Vec::with_capacity(n),
            entry_point: None,
        };
        if n == 0 {
            let index = Self::freeze_from(builder, dim, owned_vectors, precision)?;
            return Ok((index, HnswRepairStats::default()));
        }

        let mut rng = DeterministicRng::new(seed);
        // 全挿入をまたいで使い回す visited スクラッチ（`VisitedScratch` 参照。
        // 挿入ごとに新規確保しないことで search_layer の初期化コストを
        // O(N^2) から O(N) 相当へ落とす）。
        let mut visited = VisitedScratch::default();
        // 挿入順はノード番号昇順に固定する（呼び出し元の入力順＝挿入順。決定性の
        // 唯一の自由度は `seed` によるレベル割当だけにする）。
        for node_idx in 0..n {
            let level = assign_level(&mut rng, params.m);
            let node_id = node_idx as u32; // n <= MAX_HNSW_NODES であることを上で検証済み
            builder.insert_node(node_id, level, dim_usize, vectors, &mut visited)?;
        }

        // `build`／`build_with_precision`／`build_observed` はいずれも単一
        // スレッドの逐次経路であり、修復フェーズの最近傍探索（Issue #449）も
        // 常に `threads=1`（ワーカーを起動しない縮退経路）で実行する。
        let repair_stats = builder.repair_reachability_inner::<OBSERVE>(dim_usize, vectors, 1)?;

        let index = Self::freeze_from(builder, dim, owned_vectors, precision)?;
        Ok((index, repair_stats))
    }

    /// [`GraphBuilder`]（構築完了・`repair_reachability` 完了後のもの）を
    /// CSR（[`csr::CsrGraph`]）へ 1 回だけ平坦化し、[`HnswIndex`] として凍結
    /// する（Issue #494・`docs/design/hnsw-index.md` §14.2「2 相構成」の後半。
    /// 平坦化は常に最終段——`build`・並列構築（`parallel_build::freeze`）の
    /// いずれもここへ到達する直前に修復パスを終えている契約）。
    ///
    /// `precision` が [`ResidentPrecision::F16`] の場合、`vectors`（f32）を
    /// `f16::encode_rows` で 1 回エンコードする（Issue #514）。範囲外成分
    /// （`|x| > 65504.0`）を検出した場合は `F32` へ自動縮退し（D6）、実効精度は
    /// [`Self::resident_precision`] から確認できる。
    fn freeze_from(
        builder: GraphBuilder,
        dim: u32,
        vectors: Arc<[f32]>,
        precision: ResidentPrecision,
    ) -> Result<Self, HnswError> {
        let GraphBuilder {
            params,
            nodes,
            entry_point,
        } = builder;
        let graph = csr::CsrGraph::from_nodes(&nodes)?;
        let (node_vectors, resident_precision) = match precision {
            ResidentPrecision::F32 => (NodeVectors::F32(vectors), ResidentPrecision::F32),
            ResidentPrecision::F16 => {
                let mut bits = Vec::new();
                match crate::f16::encode_rows(&vectors, &mut bits) {
                    Ok(()) => (NodeVectors::F16(Arc::from(bits)), ResidentPrecision::F16),
                    // 範囲外成分を含む場合は F32 常駐へ縮退する（D6。索引全体を
                    // 拒否せず、性能崖〔非有限スコアの毎クエリ brute-force
                    // 縮退〕を避ける fail-closed な選択）。
                    Err(_) => (NodeVectors::F32(vectors), ResidentPrecision::F32),
                }
            }
            ResidentPrecision::I8 => match crate::sq8::fit_dim_params(dim as usize, &vectors) {
                Ok(fit) => {
                    let mut codes = Vec::new();
                    match crate::sq8::encode_rows(dim as usize, &vectors, &fit, &mut codes) {
                        // row_sums（Issue #522）は codes・params と同じ
                        // freeze_from 呼び出し内で 1 回だけ計算し、3 つ組の
                        // いずれか 1 つでも失敗すれば I8 常駐そのものを諦めて
                        // F32 へ縮退する（NodeVectors::I8 ドキュメンテーション
                        // コメント「寿命・対応関係が完全に一致する」契約）。
                        Ok(()) => match crate::sq8::row_sums(dim as usize, &codes) {
                            Ok(sums) => (
                                NodeVectors::I8 {
                                    codes: Arc::from(codes),
                                    params: Arc::new(fit),
                                    row_sums: Arc::from(sums),
                                },
                                ResidentPrecision::I8,
                            ),
                            Err(_) => (NodeVectors::F32(vectors), ResidentPrecision::F32),
                        },
                        // encode_rows は fit_dim_params と同じ非有限判定を防御的に
                        // 再検査するのみで通常は到達しないが、到達した場合も同じ
                        // fail-closed 方針（D6）で F32 へ縮退する。
                        Err(_) => (NodeVectors::F32(vectors), ResidentPrecision::F32),
                    }
                }
                Err(_) => (NodeVectors::F32(vectors), ResidentPrecision::F32),
            },
        };
        Ok(HnswIndex {
            params,
            dim,
            graph,
            entry_point,
            vectors: node_vectors,
            resident_precision,
        })
    }

    /// [`build`](Self::build) と同じグラフを、要素単位ロック（ノードごとの
    /// `RwLock`）とエントリポイント更新のみの排他で並列構築する（Issue #406・
    /// 親 #402。pgvector `hnswbuild.c` のロック粒度設計・qdrant の逐次
    /// プレフィックス方式を参考にした。手法名のみ参照でコード転記はしない）。
    ///
    /// `threads == 1` または `n <= `[`SEQUENTIAL_PREFIX_NODES`] の場合は
    /// [`build`](Self::build) と完全に同一のグラフを返す（内部実装は
    /// `parallel_build::build_parallel_graph` に委譲せず [`build`](Self::build)
    /// をそのまま呼ぶ）。`threads >= 2` かつ `n > `[`SEQUENTIAL_PREFIX_NODES`]
    /// の場合、先頭 [`SEQUENTIAL_PREFIX_NODES`] 件は逐次挿入し、残りを
    /// `AtomicUsize` によるワークスティール方式で並列挿入する——挿入順が
    /// 非決定的になるため、構築されるグラフの**形状**は同一 `seed` でも
    /// run-to-run で変わり得る（レベル割当は並列フェーズ開始前に `seed` から
    /// 逐次確定するため不変。[`HnswIndex::search`] の決定性契約「同一索引・
    /// 同一クエリで再現」自体は不変。詳細は `docs/design/hnsw-parallel-build.md`
    /// 参照）。
    ///
    /// # エラー
    ///
    /// `threads == 0` または `threads > `[`MAX_BUILD_THREADS`] は
    /// [`HnswError::InvalidParams`]。構築ワーカーの panic・ロック poison は
    /// [`HnswError::WorkerPanicked`]（fail-closed。部分的に結線された索引を
    /// `Ok` で返さない）。
    pub fn build_with_threads(
        params: HnswParams,
        dim: u32,
        vectors: &[f32],
        seed: u64,
        threads: usize,
    ) -> Result<Self, HnswError> {
        Self::build_with_threads_impl(params, ResidentPrecision::F32, dim, vectors, seed, threads)
    }

    /// [`Self::build_with_threads`]／[`Self::build_parallel`] が共有する
    /// 常駐精度対応版の構築本体（Issue #514）。
    fn build_with_threads_impl(
        params: HnswParams,
        precision: ResidentPrecision,
        dim: u32,
        vectors: &[f32],
        seed: u64,
        threads: usize,
    ) -> Result<Self, HnswError> {
        if threads == 0 || threads > MAX_BUILD_THREADS {
            return Err(HnswError::InvalidParams {
                reason: "threads must be in 1..=MAX_BUILD_THREADS",
            });
        }
        let n = validate_build_input(&params, dim, vectors)?;
        if threads == 1 || n <= SEQUENTIAL_PREFIX_NODES {
            return Self::build_impl(params, precision, dim, vectors, seed);
        }
        parallel_build::build_parallel_graph(params, precision, dim, vectors, seed, threads, n)
    }

    /// [`build_with_threads`](Self::build_with_threads) と同一アルゴリズム・
    /// 同一エラー契約を共有しつつ、段別の壁時間・ワーカー統計
    /// （[`HnswBuildProfile`]）を合わせて返す観測版（Issue #406 追記:
    /// 8→12 スレッド頭打ち要因の切り分け計測。`docs/design/
    /// hnsw-parallel-build.md` 参照）。
    ///
    /// `threads == 1` または `n <= `[`SEQUENTIAL_PREFIX_NODES`] の縮退経路
    /// （[`build`](Self::build) をそのまま呼ぶ）に限り
    /// [`build_with_threads`](Self::build_with_threads) と完全に同一のグラフを
    /// 返す。`threads >= 2` かつ `n > `[`SEQUENTIAL_PREFIX_NODES`] の並列経路は
    /// ワークスティールに依存するため、[`build_with_threads`]
    /// (Self::build_with_threads) と同様グラフの**形状**が run-to-run で
    /// 変わり得る（この非決定性自体は観測の有無に関わらない
    /// `build_with_threads` 既存の契約。モジュール `parallel_build` 冒頭
    /// 「決定性の範囲」節参照）。
    ///
    /// 呼び出し先（`parallel_build::build_parallel_graph_observed`）は
    /// `build_parallel_graph` と別の実装だが、ノード挿入・凍結・修復の
    /// アルゴリズム本体（`insert_node_locked`・`assemble_graph`・
    /// `repair_reachability`）は完全に共有する関数をそのまま呼ぶ。ただし
    /// 段別計測のため `BuildGraph` のノードロック取得を `try_read`/
    /// `try_write` → block の二段化にする計装を追加しており、この計装は
    /// 観測版（`observe=true`）のみに閉じ、非観測版
    /// （[`build_with_threads`](Self::build_with_threads) が使う
    /// `observe=false`）には一切波及しない（`parallel_build::BuildGraph::
    /// observe` 参照。レビュー指摘 P1-A）。したがって
    /// `build_with_threads_one_matches_sequential_build_exactly` 等の既存
    /// 完全一致テストは非観測版のみを対象にするため無変更のまま green だが、
    /// 「ロック取得順序・待ち時間まで非観測版と厳密に同一」であることは
    /// 主張しない（グラフの構築結果・poison 判定は同一）。
    ///
    /// `threads == 1` または `n <= `[`SEQUENTIAL_PREFIX_NODES`] の縮退経路
    /// （[`build`](Self::build) を呼ぶ）では、計測できる段の区切りが
    /// 存在しないため所要時間の全量を `sequential_prefix` へ積み、
    /// `workers` は空のままにする（縮退の事実がプロファイルから分かる）。
    ///
    /// # エラー
    ///
    /// [`build_with_threads`](Self::build_with_threads) と同一（`threads` の
    /// 範囲外は [`HnswError::InvalidParams`]、ワーカー panic・ロック poison は
    /// [`HnswError::WorkerPanicked`]）。
    pub fn build_with_threads_observed(
        params: HnswParams,
        dim: u32,
        vectors: &[f32],
        seed: u64,
        threads: usize,
    ) -> Result<(Self, HnswBuildProfile), HnswError> {
        let total_start = std::time::Instant::now();
        if threads == 0 || threads > MAX_BUILD_THREADS {
            return Err(HnswError::InvalidParams {
                reason: "threads must be in 1..=MAX_BUILD_THREADS",
            });
        }
        let n = validate_build_input(&params, dim, vectors)?;
        if threads == 1 || n <= SEQUENTIAL_PREFIX_NODES {
            let seq_start = std::time::Instant::now();
            // 縮退経路（threads=1 基線。Issue #447）: `build_observed` は
            // `build` と完全に同一のグラフを返しつつ `repair` 統計だけを
            // 追加で埋める。既存フィールド（`sequential_prefix`・
            // `repair_reachability`・`workers`）の値・意味は不変のまま
            // （下記コメント参照）。
            let (index, repair) = Self::build_observed(params, dim, vectors, seed)?;
            let profile = HnswBuildProfile {
                sequential_prefix: seq_start.elapsed(),
                total: total_start.elapsed(),
                repair,
                ..HnswBuildProfile::default()
            };
            return Ok((index, profile));
        }
        let (index, mut profile) = parallel_build::build_parallel_graph_observed(
            params,
            ResidentPrecision::F32,
            dim,
            vectors,
            seed,
            threads,
            n,
        )?;
        profile.total = total_start.elapsed();
        Ok((index, profile))
    }

    /// [`build_with_threads`](Self::build_with_threads) のスレッド数を
    /// [`crate::parallel_search::ParallelSearchProvider`] と同じ決定方法
    /// （`thread_count_for` による行数依存の並列度算出・プロセス全体の
    /// `WorkerBudgetGuard` による同時実行間の調停）で自動的に決める（Issue
    /// #406 要件 5。#407／#408 の既定結線先はこちら）。決定された並列度が
    /// 1 以下、またはグローバル予算を確保できなかった場合は
    /// [`build`](Self::build)（逐次・完全決定的）へ縮退する（`ParallelSearchProvider`
    /// と同じ「並列度を落とすだけで失敗させない」縮退規則）。
    ///
    /// `thread_count_for` は検索側の `MIN_ROWS_PER_THREAD`（1,024）を
    /// 1 スレッドあたりの担当行数の下限として使う（`available_parallelism`
    /// と `row_count / MIN_ROWS_PER_THREAD` の小さい方）ため、実質的な並列化
    /// 閾値は `MIN_ROWS_PER_THREAD * 2`（2,048。`available_parallelism >= 2`
    /// の環境で `desired >= 2` になる最小の `n`）であり、本メソッドが別途
    /// 課す [`SEQUENTIAL_PREFIX_NODES`]（256）より大きい。したがって
    /// `n` が 257..2047 の範囲では [`build_with_threads`](Self::
    /// build_with_threads) に明示的なスレッド数を渡せば並列化されるが、
    /// 本メソッドは検索側の閾値をそのまま流用する設計判断により逐次へ
    /// 縮退する（構築 1 ノードあたりのコストは検索 1 クエリの `dot` 計算
    /// より大幅に重いため、この閾値が構築にとって保守的すぎる可能性は
    /// 残るが、#407／#408 が実運用で結線する際に単一の決定方法を共有する
    /// 利点を優先した。見直しが必要になれば構築専用の閾値を別途持たせる）。
    pub fn build_parallel(
        params: HnswParams,
        dim: u32,
        vectors: &[f32],
        seed: u64,
    ) -> Result<Self, HnswError> {
        Self::build_parallel_impl(params, ResidentPrecision::F32, dim, vectors, seed)
    }

    /// [`Self::build_parallel`] の常駐精度 opt-in 版（Issue #514）。
    /// [`Self::build_with_precision`] と同じ精度契約（f16 範囲外成分での
    /// `F32` 自動縮退・グラフ構築は常に f32）を、並列構築経路でも維持する。
    pub fn build_parallel_with_precision(
        params: HnswParams,
        precision: ResidentPrecision,
        dim: u32,
        vectors: &[f32],
        seed: u64,
    ) -> Result<Self, HnswError> {
        Self::build_parallel_impl(params, precision, dim, vectors, seed)
    }

    /// [`Self::build_parallel`]／[`Self::build_parallel_with_precision`] が
    /// 共有する構築本体。
    fn build_parallel_impl(
        params: HnswParams,
        precision: ResidentPrecision,
        dim: u32,
        vectors: &[f32],
        seed: u64,
    ) -> Result<Self, HnswError> {
        let n = validate_build_input(&params, dim, vectors)?;
        let desired = crate::parallel_search::thread_count_for(n).min(MAX_BUILD_THREADS);
        if desired <= 1 || n <= SEQUENTIAL_PREFIX_NODES {
            return Self::build_impl(params, precision, dim, vectors, seed);
        }
        let guard = crate::parallel_search::WorkerBudgetGuard::acquire(desired);
        let granted = guard.granted();
        if granted <= 1 {
            drop(guard);
            return Self::build_impl(params, precision, dim, vectors, seed);
        }
        let result = Self::build_with_threads_impl(params, precision, dim, vectors, seed, granted);
        drop(guard);
        result
    }

    /// [`Self::greedy_descend`] のマスク付き版（Issue #431・codex-review P0
    /// 是正）。構築経路（`insert_node`）はマスクを持たないため引き続き
    /// `greedy_descend` を直接使い、本関数は探索経路（[`Self::search_masked`]）
    /// 専用とする。
    ///
    /// `mask` が `Some` の場合、`mask` が受理しないノードのスコアは一切計算
    /// しない（`self.score` を呼ばない＝当該ノードのベクトルへ触れない）。
    /// `start` 自身が非受理なら降下を行わず `Ok(None)` を返す（呼び出し元は
    /// この層より下へ安全に進められる `nearest` を持たないため、探索全体を
    /// 打ち切る——`sql::hnsw_cache::search_with_overlay` の `masked_short` 経由で
    /// plain scan へ縮退する）。`start` が受理される場合は隣接ノードのうち
    /// 受理されるものだけを候補として貪欲降下する（非受理ノードは経路上の
    /// 中継点としても使わない）。
    ///
    /// `mask` が `None` の場合は常に受理したとみなし、`greedy_descend` と同じ
    /// 計算列（呼び出し順序込み）を辿るため、[`Self::search_masked`] が
    /// `mask: None` で呼んだときに [`Self::search`] とビット同一の結果を返す
    /// 契約（`crate::hnsw::tests::search_masked_none_matches_search`）を崩さない。
    ///
    /// `hop == HopMode::TwoHop` かつ `mask` が `Some` のときに限り、非受理の
    /// 隣接ノードを 1 段だけ橋渡しの中継点として使い、その先（2-hop 先）の
    /// 受理ノードもこの層の降下候補に含める（Issue #680）。低可視比率
    /// （マスクが疎）の TwoHop 経路では上位層のノード数が層 0 の約 `1/M`
    /// しかなく、受理隣接がほぼ無いまま降下が早期停止し層 0 の探索起点が
    /// entry point 付近に固定されてしまう問題（Issue #674 Phase 2 の実測・
    /// `docs/design/hnsw-rls-cardinality-switch.md`「Issue #680」節）への
    /// 対処。橋渡しノード自身・非受理の 2-hop ノードは一切スコア計算しない
    /// （I1 不変。§`bridge_expand` と同じ「不適合ノードのリンクのみを中継点
    /// として使う」規約）。`hop == HopMode::OneHop` または `mask == None` の
    /// ときはこの分岐に入らず、既存ループとバイト単位で同一の計算列を辿る
    /// （`search_masked_two_hop_matches_search_when_mask_is_none`・
    /// `search_masked_none_matches_search` のビット同一契約を維持）。
    ///
    /// `descent_bridges` には、この呼び出し全体（複数層の降下ループを含まず、
    /// 1 回の `greedy_descend_masked` 呼び出し分）で受理・比較した 2-hop
    /// ノードの延べ数を加算する（診断用。層 0 の `bridge_expand` が数える
    /// `acorn_expansions` とは別カウンタとして扱う——Issue #681 が
    /// `acorn_expansions` の意味〔層 0 受理件数〕を閾値判定に使う前提を崩さない
    /// ため）。
    ///
    /// 停止性: 降下ループは `current_best` の厳密な改善でのみ継続するため、
    /// 橋渡し降下を加えても有限性は変わらない（同じ橋渡しノードを別反復で
    /// 再走査しうるが、visited を持たない従来の降下ループ自体がそうであり、
    /// 反復回数はグラフの次数上限で有界）。1 反復あたりの追加コストは高々
    /// `M_level` 本の非受理隣接 × `M_level` 本の 2-hop 隣接。
    #[allow(clippy::too_many_arguments)]
    fn greedy_descend_masked(
        &self,
        start: u32,
        query: &[f32],
        level: usize,
        dim: usize,
        vectors: &dyn NodeSource,
        mask: Option<&NodeMask>,
        hop: HopMode,
        descent_bridges: &mut u64,
    ) -> Result<Option<u32>, HnswError> {
        let is_ok = |node: u32| mask.map(|m| m.get(node)).unwrap_or(true);
        if !is_ok(start) {
            return Ok(None);
        }
        let bridge_enabled = hop == HopMode::TwoHop && mask.is_some();
        let mut current = start;
        let mut current_best = ScoredNode {
            node: current,
            score: self.score(current, query, dim, vectors)?,
        };
        loop {
            let mut improved = false;
            if let Some(neighbors) = self.neighbors(level, current) {
                for &cand in neighbors {
                    if !is_ok(cand) {
                        if bridge_enabled {
                            if let Some(bridged) = self.neighbors(level, cand) {
                                for &two_hop in bridged {
                                    if !is_ok(two_hop) {
                                        continue;
                                    }
                                    *descent_bridges = descent_bridges.saturating_add(1);
                                    let two_hop_scored = ScoredNode {
                                        node: two_hop,
                                        score: self.score(two_hop, query, dim, vectors)?,
                                    };
                                    if two_hop_scored > current_best {
                                        current = two_hop;
                                        current_best = two_hop_scored;
                                        improved = true;
                                    }
                                }
                            }
                        }
                        continue;
                    }
                    let cand_scored = ScoredNode {
                        node: cand,
                        score: self.score(cand, query, dim, vectors)?,
                    };
                    if cand_scored > current_best {
                        current = cand;
                        current_best = cand_scored;
                        improved = true;
                    }
                }
            }
            if !improved {
                break;
            }
        }
        Ok(Some(current))
    }

    /// [`Self::search_masked`] 用: 固定 entry point（[`Self::entry_point`]）が
    /// `mask` に非受理のときの代替探索起点選択（codex-review P2 指摘対応。
    /// `search_masked` ドキュメンテーションコメント参照）。全ノードを 1 度だけ
    /// 線形走査し、`mask` が受理するノードのうちレベル最大のもの（同点は id
    /// 最小。決定性を保つため）を返す。受理ノードが 1 つも無ければ `None`。
    ///
    /// entry point の非受理は探索経路上の他ノードの受理可否とは独立事象（RLS
    /// フィルタ・行削除で entry point だけが偶然除外される場合を含む）なので、
    /// この走査コストは fixed entry point 1 点の非受理だけで索引全体を次の
    /// 再構築まで brute-force へ縮退させる事故を避けるための対価として許容する
    /// （このパス自体、entry point が非受理の場合にのみ発生する）。
    fn find_alternate_entry(&self, mask: &NodeMask) -> Option<u32> {
        let mut best: Option<(u32, usize)> = None;
        for idx in 0..self.graph.node_count() {
            let Ok(id) = u32::try_from(idx) else {
                continue;
            };
            if !mask.get(id) {
                continue;
            }
            // `idx < node_count()` の範囲であることは上のループ条件が保証する
            // ため、`level_of` は必ず `Some` を返す（CSR 化前の `Vec<Node>`
            // 直接走査と等価。`unwrap_or(0)` は範囲外到達不能の防御的処理）。
            let level = self.level_of(id).unwrap_or(0);
            match best {
                Some((_, best_level)) if best_level >= level => {}
                _ => best = Some((id, level)),
            }
        }
        best.map(|(id, _)| id)
    }

    /// [`Self::search_masked`]・[`Self::is_mask_fully_reachable`] が共有する
    /// 探索起点選択（codex-review P2 指摘対応・PR #435 で分離）。固定
    /// entry point（[`Self::entry_point`]）が `mask` に受理されていればそれを、
    /// されていなければ [`Self::find_alternate_entry`] が選ぶ代替起点を返す。
    /// 受理ノードが 1 つも無ければ `None`。
    ///
    /// 到達可能性の検査（`is_mask_fully_reachable`）と実探索
    /// （`search_masked`）が異なる起点を使うと、検査が「探索が実際に使う
    /// 起点」とは無関係な結果を返しかねない（Cursor Bugbot High 指摘・
    /// PR #435）。本関数を両者から呼ぶことで起点選択を構造的に一致させる。
    fn search_entry_for_mask(&self, mask: &NodeMask) -> Option<u32> {
        match self.entry_point {
            Some(e) if mask.get(e) => Some(e),
            Some(_) => self.find_alternate_entry(mask),
            None => None,
        }
    }

    /// マスクの受理ノード全体が、[`Self::search_entry_for_mask`] が選ぶ単一
    /// entry point からの誘導部分グラフ（層 0 の隣接リストのみを辿る。
    /// ベクトルへのアクセス・スコア計算は一切行わない）から到達可能かどうかを
    /// 判定する（codex-review P2 指摘対応・PR #435）。
    ///
    /// マスクが複数の連結成分に分かれている場合、単一 entry point からの
    /// 探索は entry 側の成分にしか到達できず、`search_masked` の件数検査
    /// （`sql::hnsw_cache::search_with_overlay` の `masked_short`）だけでは
    /// 「別成分により近い受理ノードを一度も探索していない」ケースを
    /// 検出できない。本関数はその分断の有無を判定する。
    ///
    /// **クエリ毎には呼ばない契約**（Cursor Bugbot High 指摘・PR #435:
    /// 旧実装は `search_masked` からクエリ毎に層 0 の全受理ノード BFS を
    /// 呼んでおり、サブ線形探索の前に必ず O(V+E) の全域探索が発生していた）。
    /// 呼び出し元（`sql::hnsw_cache::Overlay::compute`）はマスクが変わる
    /// たび（世代が進むたび）1 回だけ本関数を呼び、結果を `Overlay` へ
    /// キャッシュする——`Overlay::compute` はマスク自体の構築で既に
    /// O(N) を要するため、本関数を追加で 1 回呼んでも漸近コストは増えない。
    ///
    /// 辿るのは実探索（[`Self::search_layer`]／[`Self::greedy_descend_masked`]）
    /// と同じ規約——非受理ノードは訪問済みにするのみで、その先の隣接ノードへ
    /// は一切辿らない（Issue #431 是正・`search_masked_does_not_traverse_
    /// through_a_rejected_bridge_node` で固定した契約と同じ）——に限定し、
    /// 実探索が構造的に到達できないノードを「到達可能」と誤判定しないように
    /// する。
    ///
    /// **本関数が検査に使う起点は、[`Self::search_masked`] が層 0 探索の
    /// 初期候補集合へ必ず含める起点と同一でなければならない**（codex-review
    /// P1 指摘対応・PR #435）。層 0 の隣接は枝刈りで有向になり得るため、
    /// クエリ依存の貪欲降下が着地した別ノードだけを起点に層 0 探索を始めると、
    /// 本関数が「到達可能」と判定した成分へ実探索が構造的に届かない場合が
    /// ある。両者が同じ起点を層 0 探索へ持ち込む限り、本関数が真を返す
    /// マスクについて `search_masked` が受理ノード全件へ到達できることが
    /// 保証される。
    // production 経路は `sql::hnsw_cache` が `is_mask_fully_reachable_with` を
    // 直接呼ぶ（`TraversalRegime::hop()` を明示的に渡す単一情報源。Issue #501）
    // ため、`OneHop` 専用の本ラッパーはテストからのみ参照される
    // （`select_neighbors_heuristic` と同じ方針で `#[cfg(test)]` にする）。
    #[cfg(test)]
    pub(crate) fn is_mask_fully_reachable(&self, mask: &NodeMask) -> bool {
        self.is_mask_fully_reachable_with(mask, HopMode::OneHop)
    }

    /// [`Self::is_mask_fully_reachable`] の hop 指定版（Issue #501）。
    /// `hop == HopMode::TwoHop` のとき、BFS は非受理ノードを 1 段だけ中継点
    /// として使い（[`bridge_expand`]）、実探索（[`Self::search_masked_with_hop`]）
    /// と同じ到達規則で分断の有無を判定する——両者が異なる規則を実装すると、
    /// 偽の分断判定（`TwoHop` が発火しない）か偽の到達可能判定（recall バグ）
    /// のどちらかを招く（`docs/design/hnsw-rls-cardinality-switch.md`
    /// 「Issue #501」節「同期」参照）。
    pub(crate) fn is_mask_fully_reachable_with(&self, mask: &NodeMask, hop: HopMode) -> bool {
        let target = mask.count_ones();
        if target == 0 {
            return true;
        }
        let Some(start) = self.search_entry_for_mask(mask) else {
            // 受理ノードが 1 つ以上あるのに起点が選べない防御的分岐
            // （`search_entry_for_mask` は受理ノードが 1 つでもあれば
            // `find_alternate_entry` で必ず見つけるはず）。fail-closed に
            // 「到達不能」とみなす。
            return false;
        };
        let mut visited = VisitedBitmap::default();
        self.accepted_reachable_count(start, mask, target, &mut visited, hop) >= target
    }

    /// [`Self::is_mask_fully_reachable_with`] の BFS 本体。層 0 の隣接リストのみを
    /// 辿る BFS（辺の走査のみ）で `start` から到達可能な受理ノード数を数え、
    /// `target` 件に達した時点で早期終了する。`visited` は呼び出し元が
    /// 所有するスクラッチ（本関数専用に確保する使い捨て。呼び出し頻度が
    /// クエリ毎ではなく世代毎のため、`HnswSearchScratch` を共有する必要はない）。
    /// `hop == HopMode::TwoHop` のとき非受理ノードは [`bridge_expand`] で
    /// 1 段だけ中継点として使う（§呼び出し元ドキュメンテーションコメント
    /// 「同期」参照）。
    fn accepted_reachable_count(
        &self,
        start: u32,
        mask: &NodeMask,
        target: usize,
        visited: &mut VisitedBitmap,
        hop: HopMode,
    ) -> usize {
        visited.reset(self.graph.node_count());
        if visited.mark_visited(start as usize) != Some(false) {
            return 0;
        }
        if !mask.get(start) {
            // 非受理ノードを起点に辿ることはない契約（呼び出し元は受理済みの
            // `nearest` のみを渡す）。防御的に「到達可能な受理ノード 0 件」を
            // 返す。
            return 0;
        }
        let mut count = 1usize;
        if count >= target {
            return count;
        }
        let mut queue: VecDeque<u32> = VecDeque::new();
        queue.push_back(start);
        while let Some(node) = queue.pop_front() {
            let Some(neighbors) = self.neighbors(0, node) else {
                continue;
            };
            for &neighbor in neighbors {
                match visited.mark_visited(neighbor as usize) {
                    Some(false) => {}
                    _ => continue,
                }
                if !mask.get(neighbor) {
                    // 非受理ノードはここで打ち切り、この先へは辿らない
                    // （§本関数ドキュメンテーションコメント参照）——
                    // ただし `hop == TwoHop` のときのみ、この非受理ノードを
                    // 1 段だけ中継点として使い、その先（2-hop）の受理ノード
                    // を [`bridge_expand`] 経由でキューへ加える。BFS 到達可能
                    // 判定という関数の性質上、`target` 到達後も
                    // `bridge_expand` 呼び出し自体は最後まで完了させる
                    // （`neighbors` 1 本分の追加コストのみで停止性は崩れない）。
                    if hop == HopMode::TwoHop {
                        let _ = bridge_expand(&self.graph, 0, neighbor, mask, visited, |two_hop| {
                            count = count.saturating_add(1);
                            queue.push_back(two_hop);
                            Ok(())
                        });
                        if count >= target {
                            return count;
                        }
                    }
                    continue;
                }
                count = count.saturating_add(1);
                if count >= target {
                    return count;
                }
                queue.push_back(neighbor);
            }
        }
        count
    }

    /// `search_layer`（Algorithm 2）。層 `level` 上で `entry_points` から出発し、
    /// 幅 `ef` の貪欲拡張探索を行い、`dot` 降順（同点は id 昇順）に並んだ最大 `ef`
    /// 件の候補を返す。`pub(crate)` にして #405（探索 API）が `ef_search` で再利用
    /// できるようにする。
    ///
    /// `visited` は呼び出し元（`insert_node`／`build` あるいはテスト）が
    /// 全呼び出しをまたいで所有する visited 集合（[`VisitedSet`]）。挿入ごとに
    /// 新規の `Vec<bool>` を確保しゼロ初期化していた旧実装は、`build` が
    /// 挿入ごとに少なくとも層 0 で本関数を呼ぶため合計 O(N^2) の初期化コスト
    /// になっていた（codex-review #423 P1 指摘）。構築経路は世代カウンタ方式の
    /// [`VisitedScratch`]、探索経路（#405・[`HnswIndex::search`]）はビットマップ
    /// 方式の [`VisitedBitmap`] を渡し、両者は `V: VisitedSet` のジェネリック
    /// パラメータとして本関数に共有される。各呼び出しの先頭で `reset` するだけ
    /// でよい契約は変わらない。
    /// `accept`（Issue #409。`sql::hnsw_cache::search_with_overlay` のマスク付き
    /// 探索から渡される）が `Some` の場合、`accept` が受理しないノードは候補ヒープ
    /// （`candidates`）へも一切積まない——訪問済みマークは付けるが、スコア計算
    /// （`self.score`。索引ノードのベクトルへのアクセスを伴う）自体を行わず、
    /// その隣接ノードへの探索も一切行わない（Issue #431・codex-review P0 是正。
    /// 旧実装は結果ヒープのみをマスクし候補ヒープは全ノードへ拡張していたため、
    /// 非受理ノード（削除・不可視化・キー変更で失効したノード）が探索経路
    /// （打ち切り判定の基準となる `results` の充足・以降の隣接探索）へ影響して
    /// いた。`docs/design/ann-index-adoption.md`「RLS／フィルタとの相互作用と
    /// 折衷案」節が定める「非可視ノードを探索経路として通過させる設計は不採用」
    /// という P0 安全条件に反していたため、`accept` を候補ヒープの受理判定にも
    /// 適用する形へ統一した）。`None`（既存の呼び出し元。`search`・構築経路）では
    /// 常に受理したのと同じ振る舞いになり、`search_layer` はビット同一の結果を
    /// 返す（`crate::hnsw::tests::search_masked_none_matches_search` 参照）。
    /// このマスクはテナント境界そのものではなく、クエリ時点の候補集合（アリーナ）
    /// と索引ノードの差分を表す装置だが、上記の是正後は非受理ノードのベクトルへ
    /// 一切触れない（実テナント境界は索引構築入力・呼び出し元の
    /// `provider_result_is_valid`・`RlsSafetyNet` の多層防御が担う。
    /// `sql::hnsw_cache` モジュールドキュメント参照）。
    // production 経路は `search_masked_with_hop` が `search_layer_with_hop` を
    // 直接呼ぶ（Issue #501）ため、`OneHop`・`PipelinePrefetch` 固定の本ラッパー
    // はテストからのみ参照される（`is_mask_fully_reachable` と同じ方針）。
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)] // visited・accept 追加で 9 引数。既存の precision.rs・arena.rs と同じ方針で許容する。
    pub(crate) fn search_layer<V: VisitedSet>(
        &self,
        entry_points: Vec<u32>,
        query: &[f32],
        ef: usize,
        level: usize,
        dim: usize,
        vectors: &NodeVectors,
        visited: &mut V,
        accept: Option<&NodeMask>,
    ) -> Result<Vec<ScoredNode>, HnswError> {
        // production 経路は常にパイプライン prefetch（Issue #490）。ビット
        // 同一性・P0 契約（非受理ノード非先読み）の機械検証は
        // `tests::search_layer_prefetch_*` が `search_layer_with` を
        // `NoPrefetch`／`RecordingPrefetch` で直接呼んで行う。
        self.search_layer_with(
            entry_points,
            query,
            ef,
            level,
            dim,
            vectors,
            visited,
            accept,
            &prefetch::PipelinePrefetch,
        )
    }

    /// [`Self::search_layer`] の本体。`prefetch`（Issue #490。
    /// [`prefetch::PrefetchPolicy`]）を型パラメータ化し、production は
    /// [`prefetch::PipelinePrefetch`]（ZST・単相化でコストゼロ）を、
    /// テストは `NoPrefetch`／`RecordingPrefetch` を渡してビット同一性・
    /// 「非受理ノードへは先読みしない」P0 契約を機械検証する。停止条件・
    /// 受理判定・順序規約は [`Self::search_layer`] の既存契約から一切変更
    /// していない（先読みは demand load の発行位置を早めるだけで、
    /// 探索結果・比較順序には影響しない）。
    ///
    /// 本体は [`search_layer_in`]（Issue #494。[`Adjacency`] でジェネリック化
    /// した自由関数。構築中の [`GraphBuilder::search_layer`] とも共有する）へ
    /// 委譲する薄いラッパー。`self.graph`（[`csr::CsrGraph`]）を渡すだけで、
    /// アルゴリズム本体・停止条件・受理判定は変更していない。
    // `Self::search_layer`（`#[cfg(test)]`。上のドキュメンテーションコメント
    // 参照）とテストの直接呼び出しからのみ参照される（Issue #501。production
    // 経路は `search_layer_with_hop` を使う）。
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(in crate::hnsw) fn search_layer_with<V: VisitedSet, P: prefetch::PrefetchPolicy>(
        &self,
        entry_points: Vec<u32>,
        query: &[f32],
        ef: usize,
        level: usize,
        dim: usize,
        vectors: &NodeVectors,
        visited: &mut V,
        accept: Option<&NodeMask>,
        prefetch: &P,
    ) -> Result<Vec<ScoredNode>, HnswError> {
        // 既存の全呼び出し元（`search_layer`・テスト直呼び出し）は
        // `HopMode::OneHop`（ACORN-1 の 2-hop 展開なし＝既存契約とビット
        // 同一）を渡す。`TwoHop` 経路専用の新規呼び出しは
        // [`Self::search_layer_with_hop`] を使う（Issue #501。既存の
        // 呼び出し元を変更しないことで R1 のビット同一性を構造的に保つ）。
        let mut acorn_expansions = 0u64;
        search_layer_in(
            &self.graph,
            entry_points,
            query,
            ef,
            level,
            dim,
            vectors,
            visited,
            accept,
            prefetch,
            HopMode::OneHop,
            &mut acorn_expansions,
        )
    }

    /// [`Self::search_layer_with`] の hop 指定版（Issue #501。ACORN-1 の
    /// 2-hop 展開を有効化する唯一の呼び出し経路。[`Self::search_masked_with_hop`]
    /// からのみ呼ばれる）。`acorn_expansions` は [`bridge_expand`] が受理・
    /// 候補化した 2-hop ノード数の累計を書き戻す出力引数（呼び出し元が
    /// `HnswSearchScratch::last_acorn_expansions` へ転記する）。
    #[allow(clippy::too_many_arguments)]
    pub(in crate::hnsw) fn search_layer_with_hop<V: VisitedSet, P: prefetch::PrefetchPolicy>(
        &self,
        entry_points: Vec<u32>,
        query: &[f32],
        ef: usize,
        level: usize,
        dim: usize,
        vectors: &dyn NodeSource,
        visited: &mut V,
        accept: Option<&NodeMask>,
        prefetch: &P,
        hop: HopMode,
        acorn_expansions: &mut u64,
    ) -> Result<Vec<ScoredNode>, HnswError> {
        search_layer_in(
            &self.graph,
            entry_points,
            query,
            ef,
            level,
            dim,
            vectors,
            visited,
            accept,
            prefetch,
            hop,
            acorn_expansions,
        )
    }

    /// 近傍選択ヒューリスティック（Algorithm 4）。既定は `extend_candidates=false`・
    /// `keep_pruned_connections=true`（余った枠を枝刈り済み候補で埋め、次数を
    /// 確保する）。この既定の採用理由・#405 での見直し余地は
    /// `docs/design/hnsw-graph-construction.md` に記録する。`extend_candidates=true`
    /// 形（候補の隣接をさらに候補へ加える拡張）は実装しない（到達しない分岐は
    /// 検証されないままコードに残り将来のバグ源になるため。既定を有効化する
    /// 場合に別途実装する）。
    ///
    /// 構築経路（[`GraphBuilder::insert_node`]）は本体（[`select_neighbors_heuristic_free`]）
    /// を直接呼ぶため、本メソッドは production 経路からは呼ばれない
    /// （Issue #494）。テスト（`select_neighbors_heuristic_prunes_redundant_close_candidates`）
    /// が `HnswIndex` 経由で純粋関数の挙動を検証するために残す。
    #[cfg(test)]
    fn select_neighbors_heuristic(
        &self,
        candidates: &[ScoredNode],
        m: usize,
        dim: usize,
        vectors: &[f32],
    ) -> Result<Vec<u32>, HnswError> {
        // 本体は `self` を一切参照しない純粋関数（`select_neighbors_heuristic_free`）
        // へ切り出し済み。並列構築（`parallel_build`。Issue #406）の
        // `BuildGraph` からも同じ実装を共有する。
        select_neighbors_heuristic_free(candidates, m, dim, vectors)
    }

    /// `dot(node, query)`。ノード id が範囲外／`vectors` が短すぎる場合は
    /// `NonFiniteVector` 相当ではなく、境界検証は [`node_vector`] が担う
    /// （呼び出し元は構築時に検証済みの id しか渡さない内部専用パス）。結果が
    /// 非有限（オーバーフロー・`NaN`）なら `NonFiniteScore` として拒否する
    /// （モジュール冒頭「順序規約」節参照）。
    fn score(
        &self,
        node: u32,
        query: &[f32],
        dim: usize,
        vectors: &dyn NodeSource,
    ) -> Result<f32, HnswError> {
        // Issue #514: 常駐精度（f32／f16／i8）に依存しない [`NodeSource::score`]
        // へ委譲する（Issue #522 で `vectors` を [`NodeVectors`] 固定から
        // `&dyn NodeSource` へ一般化し、`search_masked_with_hop` が
        // `hnsw::i8_query::PreparedI8Source` を渡せるようにした）。
        vectors.score(dim, node, query)
    }

    /// 構築済みパラメータを返す。
    pub fn params(&self) -> &HnswParams {
        &self.params
    }

    /// ベクトルの次元数（`build` に渡した `dim`）。
    pub fn dim(&self) -> u32 {
        self.dim
    }

    /// 構築済みノード数。
    pub fn len(&self) -> usize {
        self.graph.node_count()
    }

    /// ノード数が 0 か。
    pub fn is_empty(&self) -> bool {
        self.graph.node_count() == 0
    }

    /// `build` 時点の内部スナップショット（`self.vectors`）から `node` 番目の
    /// ベクトルを返す（Issue #408・`sql::hnsw_cache` 専用の公開アクセサ）。
    /// `node_vector` の公開版だが、こちらは範囲外時に `Err` ではなく `None` を
    /// 返す（呼び出し元がテーブル世代整合キャッシュの差分判定〔`Overlay::compute`〕
    /// で「索引済みノードが現在も存在するか」を確認する用途のため、専用の
    /// エラー型を経由させる必要がない）。
    ///
    /// f16 常駐（[`ResidentPrecision::F16`]・Issue #514）のときは f32 表現が
    /// 存在しないため常に `None` を返す（D5）。f16 のビット表現は
    /// [`Self::vector_f16`] を使う。
    pub fn vector(&self, node: u32) -> Option<&[f32]> {
        match &self.vectors {
            NodeVectors::F32(v) => node_vector(v, self.dim as usize, node).ok(),
            NodeVectors::F16(_) => None,
            NodeVectors::I8 { .. } => None,
        }
    }

    /// [`Self::vector`] の f16 常駐版（Issue #514）。`resident_precision() ==
    /// ResidentPrecision::F32` のときは常に `None`（D5）。
    pub fn vector_f16(&self, node: u32) -> Option<&[u16]> {
        match &self.vectors {
            NodeVectors::F32(_) => None,
            NodeVectors::F16(v) => node_vector_u16(v, self.dim as usize, node).ok(),
            NodeVectors::I8 { .. } => None,
        }
    }

    /// [`Self::vector`] の SQ8（i8）常駐版（Issue #521）。`resident_precision()
    /// == ResidentPrecision::I8` のときのみ `Some` を返す（D5 と同型）。
    pub fn vector_i8(&self, node: u32) -> Option<&[i8]> {
        match &self.vectors {
            NodeVectors::F32(_) | NodeVectors::F16(_) => None,
            NodeVectors::I8 { codes, .. } => node_vector_i8(codes, self.dim as usize, node).ok(),
        }
    }

    /// 索引ノードの実効常駐精度（Issue #514）。`build_with_precision` 等で
    /// 要求した精度が f16 範囲外成分により `F32` へ自動縮退した場合
    /// （D6）、この値は要求値と異なる。
    pub fn resident_precision(&self) -> ResidentPrecision {
        self.resident_precision
    }

    /// 索引済みノード `node` の格納ベクトルが `candidate`（呼び出し元のクエリ
    /// スナップショットから取り出した現在の行）と「一致するとみなせるか」を
    /// 判定する（Issue #514・D4）。`sql::hnsw_cache::Overlay::compute` の
    /// 差分検出（世代進行直後に再インデックスすべき行の特定）が使う。
    ///
    /// - `F32` 常駐: `node_vector` を `to_bits()` で厳密比較する（旧
    ///   `sql::hnsw_cache::vectors_bit_equal`〔本 Issue で撤去・本メソッドへ
    ///   統合〕と同じ判定）。
    /// - `F16` 常駐: `candidate` を `f16::f32_to_f16_bits` で符号化したビット列と
    ///   格納ビット列を比較する。f16 分解能未満の摂動は「未変更」と判定される
    ///   （最終スコアは常に f32 アリーナから再計算されるため結果の正しさは
    ///   保たれ、影響はグラフ近傍構造の再利用判定のみに限られる）。
    /// - `node` が範囲外、または `candidate.len() != dim` の場合は `None`
    ///   （呼び出し元は差分ありとして扱う）。
    pub(crate) fn node_matches(&self, node: u32, candidate: &[f32]) -> Option<bool> {
        let dim = self.dim as usize;
        if candidate.len() != dim {
            return None;
        }
        match &self.vectors {
            NodeVectors::F32(v) => {
                let stored = node_vector(v, dim, node).ok()?;
                Some(
                    stored
                        .iter()
                        .zip(candidate.iter())
                        .all(|(a, b)| a.to_bits() == b.to_bits()),
                )
            }
            NodeVectors::F16(v) => {
                let stored = node_vector_u16(v, dim, node).ok()?;
                Some(
                    stored
                        .iter()
                        .zip(candidate.iter())
                        .all(|(&a, &b)| a == crate::f16::f32_to_f16_bits(b)),
                )
            }
            NodeVectors::I8 { codes, params, .. } => {
                let stored = node_vector_i8(codes, dim, node).ok()?;
                // 先に範囲検査を行う（Issue #521）。対称量子化の表現域は
                // 次元ごとに `[-127*scale_d, 127*scale_d]`（`scale_d == 0` の
                // 次元は `0` のみ）で、これは fit 時点の `[min_d, max_d]` を
                // 包含する。`candidate` がこの範囲を超える場合、量子化すると
                // クランプにより「たまたま同じコード」になり得るため（127 段の
                // 粗い分解能で、fit 済み範囲の極値にあった行がさらに大きい値へ
                // 更新されても「未変更」と誤判定される穴）、比較の前にこの
                // ケースを不一致として弾く。最終スコアは常に f32 アリーナから
                // 再計算されるため結果の正しさは範囲検査の有無に関わらず不変
                // （影響はグラフ近傍構造の再利用判定のみ）。
                for (&c, &scale) in candidate.iter().zip(params.scales().iter()) {
                    let limit = f64::from(scale) * 127.0;
                    // `scale` は fit_dim_params が f64 で算出したスケールを
                    // f32 へ丸めた値（真値よりわずかに小さくなり得る）。
                    // limit（127 倍した表現域上限）もその丸め誤差ぶん真の
                    // 最大絶対値を下回ることがあり（例: fit 時最大絶対値が
                    // ちょうど 1.0 のとき limit ≈ 0.9999999963）、未変更の
                    // 極値行を誤って「範囲外＝changed」と判定し不要な
                    // 再構築を招く（PR #617 codex-review P2・Cursor Bugbot
                    // 指摘）。f32 の 1 ULP 相当（`f32::EPSILON`）の相対
                    // 許容誤差を加えて吸収する。範囲検査は近傍構造の再利用
                    // 判定のみに使われ最終スコアは常に f32 で再計算される
                    // ため、この許容誤差の拡大が結果の正しさに影響しない。
                    //
                    // ただし相対許容誤差だけでは subnormal 域の scale
                    // （例: 次元の最大絶対値が `f32::from_bits(190)` 程度の
                    // 極小値で `fit_dim_params` が `scale = f32::from_bits(1)`
                    // を返す場合）を吸収できない。subnormal 域では f32 の
                    // 刻み幅（ULP）が値に比例せず `f32::MIN_POSITIVE` 未満の
                    // 固定の絶対ステップになるため、`limit.abs() * EPSILON`
                    // が実際の丸め誤差より小さくなり、未変更ベクトルが
                    // `Some(false)`（不一致）と誤判定されうる（PR #617
                    // codex-review P2 指摘）。scale 自体の 1 ULP 分の絶対
                    // ステップ（`f32::next_up` との差。subnormal 域でも
                    // ビット単位で正しく求まる）を 127 倍した絶対許容誤差を
                    // フロアとして追加で持たせ、相対許容誤差とのより大きい方を
                    // 採る。
                    let scale_ulp = f64::from(scale.next_up()) - f64::from(scale);
                    let tol = (limit.abs() * f64::from(f32::EPSILON)).max(scale_ulp * 127.0);
                    if f64::from(c).abs() > limit + tol {
                        return Some(false);
                    }
                }
                Some(
                    stored
                        .iter()
                        .zip(candidate.iter())
                        .zip(params.scales().iter())
                        .all(|((&r, &c), &scale)| {
                            r == crate::sq8::quantize_scalar_f64(f64::from(c), f64::from(scale))
                        }),
                )
            }
        }
    }

    /// 索引本体（CSR 隣接表現・複製ベクトル）の概算ヒープバイト量（Issue #408。
    /// `sql::hnsw_cache::HnswIndexCache` の容量判定・観測用統計が使う。
    /// Issue #494 で CSR 化した後は [`csr::CsrGraph::approx_heap_bytes`] へ
    /// 委譲する）。`self.vectors`（Issue #514: [`NodeVectors`]。常駐精度に
    /// 応じ f32／f16 いずれかのバイト量）＋ [`csr::CsrGraph`] の 4 配列の
    /// `capacity()` を合算する。
    pub fn approx_heap_bytes(&self) -> usize {
        self.vectors
            .approx_bytes()
            .saturating_add(self.graph.approx_heap_bytes())
    }

    /// グラフ全体の最大層（エントリポイントのレベル）。空索引では `None`。
    pub fn max_level(&self) -> Option<usize> {
        self.entry_point.and_then(|ep| self.level_of(ep))
    }

    /// エントリポイントのノード id。空索引では `None`。
    pub fn entry_point(&self) -> Option<u32> {
        self.entry_point
    }

    /// ノード `node` が割り当てられたレベル。存在しないノードは `None`。
    pub fn level_of(&self, node: u32) -> Option<usize> {
        Adjacency::level_of(&self.graph, node)
    }

    /// 層 `level` におけるノード `node` の隣接リスト。存在しない層・ノードは
    /// `None`（ノードのレベルが `level` 未満の場合を含む）。
    pub fn neighbors(&self, level: usize, node: u32) -> Option<&[u32]> {
        Adjacency::neighbors(&self.graph, level, node)
    }

    /// 層 `level` における最大次数（層 0 は `2*m`、層 1 以上は `m`）。テスト・
    /// `EXPLAIN`（#411 の担当範囲）が参照する想定の公開ヘルパ。
    pub fn max_degree(&self, level: usize) -> usize {
        max_degree_for(&self.params, level)
    }

    /// ef-探索 top-k 検索（Malkov & Yashunin 2016 Algorithm 5 相当。TASK-132・
    /// CORE-9・CORE-10。#405 の担当範囲）。上位層を `ef=1` の
    /// [`greedy_descend`](Self::greedy_descend) で降下したのち、層 0 を幅
    /// `ef.max(k)` で [`search_layer`](Self::search_layer) によりビーム探索し、
    /// 上位 `k` 件を返す。結果は `kernel.rs::CandidateHit` と同じ順序規約
    /// （スコア降順・同点は id 昇順）で、`id` は内部スナップショット
    /// （`self.vectors`）上のノード番号（0 始まり）を `u64` 化したもの。
    ///
    /// ベクトル本体は引数で受け取らず、[`build`](Self::build) 時に取得した
    /// 内部スナップショット（`self.vectors`。row-major 連続バッファ、
    /// `self.vectors.len() == self.len() * dim`。モジュール冒頭「ベクトルの
    /// 所有方針」節）のみを参照する。`scratch` は クエリをまたいで呼び出し元が
    /// 再利用する [`HnswSearchScratch`]。
    ///
    /// 決定性の保証範囲は「同一索引・同一クエリ・任意のスクラッチ状態で
    /// 結果が再現する」までであり、総当たり経路（`kernel.rs`）が持つ境界
    /// 同点グループの完全化までは保証しない（spec 側の規範化は #405 の
    /// 担当外。詳細は `docs/design/hnsw-search.md` 参照）。
    ///
    /// # エラー
    ///
    /// 検証順序は次のとおり（すべて fail-closed）: クエリ次元不一致
    /// （[`HnswError::QueryDimMismatch`]）→ クエリの非有限値
    /// （[`HnswError::NonFiniteQuery`]。`kernel.rs::KernelError::NonFiniteQuery`
    /// と同じ理由で `total_cmp` の順序に委ねず事前拒否する）→ `ef`／`k` の
    /// 上限超過（[`HnswError::InvalidParams`]。`MAX_EF` を上限に流用し、
    /// untrusted な呼び出し元が無制限の候補集合を要求できないようにする）。
    /// `k == 0` または空索引は空の `Ok(Vec::new())` を返す。ベクトル本体は
    /// `build` 時に取得した内部スナップショット（`self.vectors`）を使うため、
    /// 呼び出し元がベクトル集合を渡す経路が存在せず、長さ・内容の不一致
    /// エラーはそもそも構造的に発生しない（モジュール冒頭「ベクトルの
    /// 所有方針」節・codex-review PR #430 P1 指摘対応）。
    pub fn search(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        scratch: &mut HnswSearchScratch,
    ) -> Result<Vec<crate::kernel::CandidateHit>, HnswError> {
        self.search_masked(query, k, ef, None, scratch)
    }

    /// [`Self::search`] のマスク付き版（Issue #409。Issue #431・codex-review P0
    /// 是正で上位層の貪欲降下も含めマスクを一貫適用する形へ変更。さらに
    /// codex-review P2 指摘対応で固定 entry point 自体が非受理の場合の代替
    /// 探索起点選択を追加）。`mask` が `Some` の場合、上位層の貪欲降下
    /// （[`Self::greedy_descend_masked`]）・層 0 のビーム探索
    /// （[`Self::search_layer`]）のいずれも `mask` を候補マスクとして渡し、
    /// `mask` が受理しないノード（削除・不可視化・キー変更で失効したノード）は
    /// 探索経路のどの段階でも一切スコア計算・探索の起点として使わない
    /// （`docs/design/ann-index-adoption.md`「RLS／フィルタとの相互作用と折衷案」
    /// 節の P0 安全条件）。
    ///
    /// 固定 entry point（[`Self::entry_point`]）自体が `mask` に非受理の場合、
    /// 直ちに空集合へ縮退せず、[`Self::search_entry_for_mask`] で受理ノードの
    /// 中から代替探索起点（受理ノードのうちレベル最大・同点は id 最小）を選び、
    /// その代替起点が存在する最上層から通常どおり降下する（可視カーディナリティ
    /// が `full_scan_ratio` 以上で ANN 経路を選ぶ設計—Issue #409—と整合させる
    /// ため、entry point 1 点の非受理だけで索引全体が次の再構築まで無効化される
    /// 事故を避ける）。受理ノードが 1 つも無い場合のみ空の結果を返し
    /// （`Ok(Vec::new())`）、呼び出し元（`sql::hnsw_cache::search_with_overlay`）
    /// が `masked_short` として plain scan へ縮退する。
    ///
    /// マスクが複数の連結成分に分かれ、選んだ探索起点の成分だけでは受理ノード
    /// を覆い切れない場合の検出は本関数の責務ではない
    /// （Cursor Bugbot High 指摘・PR #435: クエリ毎に層 0 の全受理ノード BFS を
    /// 行うとサブ線形探索の前に必ず O(V+E) が発生し ANN の目的を損なう）。
    /// 呼び出し元（`sql::hnsw_cache::Overlay::compute`）が世代（マスク）が
    /// 変わるたび 1 回だけ [`Self::is_mask_fully_reachable`] を呼び、分断が
    /// あれば本関数自体を呼ばず plain scan へ縮退する契約になっている
    /// （`docs/design/hnsw-rls-cardinality-switch.md`「masked_short」節）。
    ///
    /// 層 0 探索の初期候補には、[`Self::search_entry_for_mask`] が選び
    /// [`Self::is_mask_fully_reachable`] が到達可能性を検査した起点
    /// （`checked_entry`）を、クエリ依存の多層貪欲降下が着地したノード
    /// （`nearest`）と併せて必ず含める（codex-review P1 指摘対応・
    /// PR #435）。層 0 の隣接は枝刈りで有向になり得るため、降下後ノード
    /// だけを初期候補にすると、検査済み起点からは到達できていた成分へ
    /// 降下後ノードからは辿り着けない場合があり、`is_mask_fully_reachable`
    /// が「分断なし」と判定したマスクでも一部の受理ノード（別成分のより
    /// 近い候補を含む）を取りこぼしかねない。両者が異なる場合のみ 2 起点を
    /// 渡すため、`mask` が `None`（`checked_entry == nearest == entry`）
    /// のときは従来どおり単一起点のままで下記のビット同一契約を崩さない。
    ///
    /// `None` の場合は [`Self::search`] とビット同一の結果を返す
    /// （`crate::hnsw::tests::search_masked_none_matches_search` で機械検証）。
    ///
    /// # エラー
    ///
    /// [`Self::search`] と同じ検証順序に加え、`mask.len() != self.len()`
    /// （索引のノード数と不一致）は [`HnswError::InvalidParams`] として拒否する
    /// （fail-closed。呼び出し元が古い索引世代のマスクを渡す事故を検出する）。
    pub fn search_masked(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        mask: Option<&NodeMask>,
        scratch: &mut HnswSearchScratch,
    ) -> Result<Vec<crate::kernel::CandidateHit>, HnswError> {
        self.search_masked_with(query, k, ef, mask, DEFAULT_SPARSE_VISITED_MAX, scratch)
    }

    /// [`Self::search_masked`] の visited 集合切替版（Issue #497）。マスク付き
    /// 探索（`mask == Some(_)`）の可視候補数（`mask.count_ones()`。Issue #497で
    /// O(1) 化済み）が `sparse_visited_max` 未満のとき、層 0 のビーム探索で
    /// [`VisitedBitmap`] の代わりに [`VisitedSparse`]（`HashSet<u32>`）を使う。
    /// `mask == None` のときは `sparse_visited_max` の値に関わらず常に
    /// [`VisitedBitmap`] を使う（[`Self::search`] とのビット同一契約を無条件に
    /// 維持する。`search_masked_none_matches_search` が固定）。
    ///
    /// [`Self::search_masked`] は `sparse_visited_max` に既定値
    /// （[`DEFAULT_SPARSE_VISITED_MAX`] = 0。常に dense）を渡して本関数へ委譲する
    /// 薄いラッパー。呼び出し元（`sql::hnsw_cache::search_with_overlay`）は
    /// `ValidatedHnswParams::sparse_visited_max`（構築時 opt-in）をそのまま渡す。
    ///
    /// 選ばれた visited 実装は `scratch.last_visited_kind()`
    /// （[`VisitedKind`]。診断用）で観測できる。層 0 探索まで到達しない早期
    /// `return` 経路（`k == 0`・空索引・受理ノードなし等）では `None` のまま
    /// 残す。
    ///
    /// # エラー
    ///
    /// 検証順序・戻り値の契約は [`Self::search_masked`] と同一（本関数へ委譲
    /// するだけで検証ロジック自体は変更していない）。
    pub fn search_masked_with(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        mask: Option<&NodeMask>,
        sparse_visited_max: usize,
        scratch: &mut HnswSearchScratch,
    ) -> Result<Vec<crate::kernel::CandidateHit>, HnswError> {
        self.search_masked_with_hop(
            query,
            k,
            ef,
            mask,
            sparse_visited_max,
            HopMode::OneHop,
            scratch,
        )
    }

    /// [`Self::search_masked_with`] の hop 指定版（Issue #501。ACORN-1 の
    /// 2-hop 展開〔[`HopMode::TwoHop`]〕を有効化する唯一の呼び出し経路）。
    /// `hop == HopMode::OneHop` のときは本関数・[`Self::search_masked_with`]・
    /// [`Self::search_masked`] はビット同一の結果を返す（既存動作を不変に
    /// 保つ。呼び出し元は `sql::hnsw_cache::search_with_overlay` が
    /// `sql::hnsw_cache::TraversalRegime`〔可視カーディナリティ×
    /// `ValidatedHnswParams::acorn_max_visible_ratio` から導出〕を渡す）。
    ///
    /// `scratch.last_acorn_expansions()` に本呼び出しが `bridge_expand`
    /// 経由で受理・候補化した 2-hop ノード数を書き戻す（`hop == OneHop` では
    /// 常に `0`）。
    ///
    /// # エラー
    ///
    /// 検証順序・戻り値の契約は [`Self::search_masked`] と同一（本関数へ委譲
    /// するだけで検証ロジック自体は変更していない）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn search_masked_with_hop(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        mask: Option<&NodeMask>,
        sparse_visited_max: usize,
        hop: HopMode,
        scratch: &mut HnswSearchScratch,
    ) -> Result<Vec<crate::kernel::CandidateHit>, HnswError> {
        // 早期 return 経路で前回呼び出しの記録を持ち越さない（Issue #497・#501）。
        // 層 0 探索へ到達した場合のみ、その直前で上書きする。
        scratch.last_visited_kind = None;
        scratch.last_acorn_expansions = 0;
        scratch.last_acorn_descent_bridges = 0;

        let dim_usize = self.dim as usize;
        if query.len() != dim_usize {
            return Err(HnswError::QueryDimMismatch {
                expected: self.dim,
                found: query.len(),
            });
        }
        if query.iter().any(|v| !v.is_finite()) {
            return Err(HnswError::NonFiniteQuery);
        }
        if ef == 0 || ef > MAX_EF {
            return Err(HnswError::InvalidParams {
                reason: "ef must be in 1..=MAX_EF",
            });
        }
        if k > MAX_EF {
            return Err(HnswError::InvalidParams {
                reason: "k exceeds MAX_EF",
            });
        }
        if let Some(m) = mask {
            if m.len() != self.graph.node_count() {
                return Err(HnswError::InvalidParams {
                    reason: "mask length does not match index node count",
                });
            }
        }
        if k == 0 || self.graph.node_count() == 0 {
            return Ok(Vec::new());
        }

        // I8 常駐（Issue #522）の候補生成は、このクエリの二重量子化
        // （`crate::sq8::prepare_query`）を探索本体（`greedy_descend_masked`・
        // `search_layer_with_hop`）が使う `score` 呼び出しのたびに繰り返さない
        // よう、この呼び出し 1 回につき 1 回だけ準備する
        // （`hnsw::i8_query::PreparedI8Source`）。F32／F16 常駐、または
        // `prepare_query` の失敗時（`PreparedI8Source::new` 内で `Dequant`
        // へ縮退）はいずれも既存の `NodeVectors::score`（`&self.vectors`）を
        // そのまま使う。
        let prepared_i8;
        let source: &dyn NodeSource = if let NodeVectors::I8 {
            codes,
            params,
            row_sums,
        } = &self.vectors
        {
            prepared_i8 = i8_query::PreparedI8Source::new(codes, row_sums, params, query);
            &prepared_i8
        } else {
            &self.vectors
        };

        let Some(entry) = self.entry_point else {
            return Ok(Vec::new());
        };
        let Some(top_level) = self.max_level() else {
            return Ok(Vec::new());
        };

        // 固定 entry point が非受理なら、受理ノードの中から代替探索起点を選ぶ
        // （codex-review P2 指摘対応。§本関数ドキュメンテーションコメント参照）。
        // `search_entry_for_mask` は [`Self::is_mask_fully_reachable`] と同じ
        // 起点選択を共有する（検査と探索で起点を一致させる。Cursor Bugbot
        // High 指摘・PR #435）。代替起点は自身が存在する最上層
        // （`alt_level <= top_level`。全ノードは層 0..=自身の level に存在
        // するため）までしか降下できないので、以降の降下ループの上限も
        // それに合わせて縮める。
        // `checked_entry` は `is_mask_fully_reachable` が到達可能性を検査した
        // のと同じ起点（`search_entry_for_mask` の戻り値）を保持する
        // （codex-review P1 指摘対応・PR #435）。`nearest` はこの後の多層
        // 貪欲降下でクエリ依存の別ノードへ移動するため、両者は一致すると
        // 限らない。層 0 の隣接は枝刈りで有向になり得るので、降下後ノード
        // だけを層 0 探索の初期候補にすると、検査済み起点からは到達できて
        // いた成分へ降下後ノードからは辿り着けない場合がある——検査で
        // 「分断なし」と判定されたにもかかわらず、実探索が構造的に一部の
        // 受理ノードを取りこぼす（本関数の呼び出し元は `is_mask_fully_
        // reachable` が真のときのみ `search_masked` を呼ぶ契約のため、この
        // 食い違いは fail-closed 側で吸収されず recall バグとして顕在化する）。
        let (mut nearest, effective_top, checked_entry) = match mask {
            Some(m) => match self.search_entry_for_mask(m) {
                Some(start) if start == entry => (start, top_level, start),
                Some(alt) => {
                    let alt_level = self.level_of(alt).unwrap_or(0);
                    (alt, top_level.min(alt_level), alt)
                }
                // 受理ノードが 1 つも無い: ANN 経路では応答不能なので
                // plain scan への縮退に委ねる。
                None => return Ok(Vec::new()),
            },
            None => (entry, top_level, entry),
        };
        let mut descent_bridges = 0u64;
        if effective_top > 0 {
            for l in (1..=effective_top).rev() {
                nearest = match self.greedy_descend_masked(
                    nearest,
                    query,
                    l,
                    dim_usize,
                    source,
                    mask,
                    hop,
                    &mut descent_bridges,
                )? {
                    Some(n) => n,
                    // 到達しない防御的分岐: `nearest` は代替起点選択の時点で
                    // 受理済みであることを検証しており、`greedy_descend_masked`
                    // は受理済みの候補へしか `current` を進めないため、以降の
                    // 呼び出しでも常に受理済みノードを渡している。
                    None => return Ok(Vec::new()),
                };
            }
        }

        // 連結成分の分断検出は呼び出し元（`sql::hnsw_cache::Overlay::compute`）が
        // [`Self::is_mask_fully_reachable`] で世代毎に 1 回だけ行う契約
        // （§本関数ドキュメンテーションコメント参照）。ここではクエリ毎の
        // 全域 BFS を行わない。

        // k > ef のとき結果集合が k 件に満たない事故を防ぐため、実効 ef を
        // `ef.max(k)` へ引き上げる（hnswlib 等の一般的慣行。詳細は
        // `docs/design/hnsw-search.md` 参照）。ef・k は共に上で MAX_EF 以下と
        // 検証済みのため `ef.max(k)` も MAX_EF 以下。
        //
        // `hop == HopMode::TwoHop` かつ `mask` が `Some` のときのみ、可視比率
        // に応じて実効 ef をさらに底上げする（Issue #680。§`two_hop_effective_ef`
        // ドキュメンテーションコメント参照）。橋渡し展開で 1 クエリあたり
        // 多数の遠方候補が流入し、既定の `ef` だと `results` ヒープが遠方候補で
        // 埋まって早期打ち切りを誘発する事象への対処。OneHop・`mask == None`
        // では従来どおり `ef.max(k)` のまま（式自体を分岐で切り替え、既存の
        // 計算は一切変えない）。
        let ef_eff = if hop == HopMode::TwoHop {
            if let Some(m) = mask {
                two_hop_effective_ef(ef, k, self.graph.node_count(), m.count_ones())
            } else {
                ef.max(k)
            }
        } else {
            ef.max(k)
        };

        // 層 0 探索の初期候補には降下後ノード（`nearest`）に加え、`mask` が
        // `Some` かつ両者が異なる場合は検査済み起点（`checked_entry`）も
        // 必ず含める（§上の `checked_entry` コメント参照）。`mask` が `None`
        // のときは `checked_entry == nearest == entry` なので従来どおり単一
        // 起点となり、[`Self::search`] とのビット同一契約
        // （`search_masked_none_matches_search`）は変わらない。
        let level0_entry_points = if mask.is_some() && checked_entry != nearest {
            vec![nearest, checked_entry]
        } else {
            vec![nearest]
        };

        // visited 集合の切替（Issue #497）: `mask` が `Some` かつ可視候補数
        // （`count_ones()`。O(1)）が `sparse_visited_max` 未満のときのみ
        // `VisitedSparse` を選ぶ。`mask == None` は常に dense
        // （§本関数ドキュメンテーションコメント参照。`search` とのビット
        // 同一契約を無条件に維持する）。
        let use_sparse =
            matches!(mask.map(|m| m.count_ones()), Some(ones) if ones < sparse_visited_max);
        scratch.last_visited_kind = Some(if use_sparse {
            VisitedKind::Sparse
        } else {
            VisitedKind::Dense
        });
        let mut acorn_expansions = 0u64;
        let results = if use_sparse {
            self.search_layer_with_hop(
                level0_entry_points,
                query,
                ef_eff,
                0,
                dim_usize,
                source,
                &mut scratch.sparse,
                mask,
                &prefetch::PipelinePrefetch,
                hop,
                &mut acorn_expansions,
            )?
        } else {
            self.search_layer_with_hop(
                level0_entry_points,
                query,
                ef_eff,
                0,
                dim_usize,
                source,
                &mut scratch.visited,
                mask,
                &prefetch::PipelinePrefetch,
                hop,
                &mut acorn_expansions,
            )?
        };
        scratch.last_acorn_expansions = acorn_expansions;
        scratch.last_acorn_descent_bridges = descent_bridges;

        let out: Vec<crate::kernel::CandidateHit> = results
            .into_iter()
            .take(k)
            .map(|s| crate::kernel::CandidateHit {
                id: s.node as u64,
                score: s.score,
            })
            .collect();
        Ok(out)
    }
}

/// クエリ単位で保持する再開型層 0 探索状態（Issue #505・親 #503／#458／#455。
/// pgvector `hnswscan.c` 型の「破棄候補を保持し、次ラウンドで自己昇格＋候補
/// 復帰して層 0 探索を再開する」方式。`docs/design/hnsw-hybrid-iterative-scan.md`
/// 「Phase B 再検討（Issue #504）」節の契約に従う独立実装——[`search_layer_in`]
/// 本体は不変・複製しない設計は見送り、TwoHop・[`bridge_expand`] との相互作用
/// を切り離すため `HopMode::OneHop` 限定で層 0 探索の要点のみを再実装した
/// （見送り理由・同値性の根拠は `docs/design/hnsw-hybrid-iterative-scan.md`
/// 「実装記録（Issue #505）」節参照）。
///
/// hybrid 密側再取得ループ（`hybrid.rs::hybrid_search_boosted` の
/// `dense_fetch_k` 倍増）は同一クエリを `ef`／`k` を大きくしながら複数ラウンド
/// 呼び直す。素朴な実装（[`HnswIndex::search_masked_with_hop`] を毎ラウンド
/// 呼び直す）はラウンドごとに visited をリセットし層 0 ビーム探索をゼロから
/// やり直すため、全ラウンド合計の隣接走査が Σ_r visited_r になり得る。本状態は
/// `candidates`（未展開候補）／`results`（現在の top-`ef_eff`）／`discarded`
/// （非受理・`results` 追い出しで一度捨てたノード）／`expanded`（隣接走査
/// 済み）／`in_candidates`（現在 `candidates` に積まれているか）／
/// `visited`（発見済みか）を呼び出し元（`sql::hnsw_hybrid::HnswDenseProvider`）
/// がクエリの寿命だけ保持することで、全ラウンド合計の隣接走査を高々索引
/// ノード数まで押さえる。
///
/// # 決定性・正しさの保証範囲
///
/// ラウンド 1（[`HnswIndex::search_masked_resumable_start`] 単体）は
/// [`HnswIndex::search_masked_with_hop`]（`HopMode::OneHop`）とビット同一
/// （停止条件を「pop 直後判定」から「pop 前の peek 判定」へ移すのみで、
/// 単発実行の `results` 出力には影響しない——停止条件が成立する場合、元の
/// 実装も pop した候補をそのまま捨てて `break` するだけで、その候補は
/// どのみち `results`・`candidates` へ一切反映されないため）。
/// `candidates` が尽きた時点（exhaustive。以後のラウンドで候補復帰しても
/// 新規発見が増えない）の結果はブルートフォース対照と厳密一致する。
/// 非 exhaustive な途中ラウンド（`ef` 依存の `worst_ok` 判定を経る）は
/// 再実行型と一致を保証しない（本モジュールの契約はここまで。性能上の
/// 採否・前後比較は Issue #506 の担当）。
#[derive(Debug)]
pub(crate) struct ResumableMaskedSearch {
    /// 未展開の候補（最大要素＝次に展開すべきノードが `peek` で分かる
    /// 最大ヒープ）。
    candidates: BinaryHeap<ScoredNode>,
    /// 現在の top-`ef_eff`（最悪要素が `peek` で分かるよう `Reverse` で
    /// 包んだ最小ヒープ）。
    results: BinaryHeap<std::cmp::Reverse<ScoredNode>>,
    /// 非受理（`worst_ok` 不成立で候補ヒープへ積まれなかった）ノード、または
    /// `results.pop()` で top-`ef_eff` から追い出されたノード。次ラウンドの
    /// 自己昇格・候補復帰（[`HnswIndex::search_masked_resume`]）の対象。
    discarded: Vec<ScoredNode>,
    /// `candidates` から pop され隣接走査を終えたノード（1 ノード高々 1 回。
    /// 一度立てたら以後は永続的に「展開不要」を意味するため降ろす操作は
    /// 持たない）。
    expanded: VisitedBitmap,
    /// 現在 `candidates` ヒープに積まれている（未展開の）ノード。候補復帰
    /// 時の二重 push 防止にのみ使う（`expanded` が立てば以後 push 対象から
    /// 恒久的に外れるため、本フラグを「降ろす」操作は不要）。
    in_candidates: VisitedBitmap,
    /// 発見済み（一度でも候補として評価された）ノード。`search_layer_in` の
    /// `visited` と同じ役割。
    visited: VisitedBitmap,
    /// クエリの bit 表現（整合検査用。`f32::to_bits` 比較で別クエリの状態を
    /// 誤って再開しないことを保証する）。
    query_bits: Vec<u32>,
    /// 直前ラウンドの `ef_eff`（`ef.max(k)`）。次ラウンドは `>=` を要求する
    /// （自己昇格が既存 `results` を一切降格しないことの前提）。
    last_ef: usize,
    /// 状態構築時点の索引ノード数（世代整合検査用）。
    node_count: usize,
    /// 状態構築時点のマスク長（マスク無しは `None`）。
    mask_len: Option<usize>,
    /// テスト専用の実展開回数ログ（codex-review P2 指摘対応・PR #619）。
    /// `expanded`（[`VisitedBitmap`]）はビットフラグのため「一度でも
    /// 立ったか」しか分からず、同一ノードが `candidates` から複数回 pop
    /// され複数回展開される二重展開バグを検出できない。`resumable_run` が
    /// ノードを実際に展開する（`mark_visited` が `Some(false)` を返した）
    /// たびに node id を push し、テストが出現回数を数えて「各ノード高々
    /// 1 回」を実カウンタで検証する。production の探索結果・計算量には
    /// 影響しない（cfg(test) 限定）。
    #[cfg(test)]
    expansion_log: Vec<u32>,
}

impl HnswIndex {
    /// [`ResumableMaskedSearch`] のラウンド 1: 検証・起点解決・層 0 探索を
    /// 行い、状態を返す。`hop == HopMode::TwoHop` は状態化せず拒否する
    /// （§ [`ResumableMaskedSearch`] ドキュメンテーションコメント参照。
    /// 呼び出し元 `sql::hnsw_cache::search_prepared_resumable` は
    /// `TraversalRegime::TwoHop` のラウンドでは本関数を呼ばず既存の単発経路
    /// （[`Self::search_masked_with_hop`]）へ倒す）。
    ///
    /// 検証順序・起点解決（固定 entry point 非受理時の代替起点選択・上位層
    /// 貪欲降下）は [`Self::search_masked_with_hop`] と同一ロジックを踏襲する
    /// （既存ホットパスは変更していないため複製だが、対象は起点解決のみで
    /// 層 0 探索本体は複製しない——両者は [`Self::resumable_offer_entry`]・
    /// [`Self::resumable_run`] を共有する）。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn search_masked_resumable_start(
        &self,
        query: &[f32],
        k: usize,
        ef: usize,
        mask: Option<&NodeMask>,
        hop: HopMode,
    ) -> Result<(Vec<crate::kernel::CandidateHit>, ResumableMaskedSearch), HnswError> {
        if hop == HopMode::TwoHop {
            return Err(HnswError::InvalidParams {
                reason: "resumable search does not support HopMode::TwoHop",
            });
        }
        let dim_usize = self.dim as usize;
        if query.len() != dim_usize {
            return Err(HnswError::QueryDimMismatch {
                expected: self.dim,
                found: query.len(),
            });
        }
        if query.iter().any(|v| !v.is_finite()) {
            return Err(HnswError::NonFiniteQuery);
        }
        if ef == 0 || ef > MAX_EF {
            return Err(HnswError::InvalidParams {
                reason: "ef must be in 1..=MAX_EF",
            });
        }
        if k > MAX_EF {
            return Err(HnswError::InvalidParams {
                reason: "k exceeds MAX_EF",
            });
        }
        let node_count = self.graph.node_count();
        let mask_len = match mask {
            Some(m) => {
                if m.len() != node_count {
                    return Err(HnswError::InvalidParams {
                        reason: "mask length does not match index node count",
                    });
                }
                Some(m.len())
            }
            None => None,
        };

        let mut state = ResumableMaskedSearch {
            candidates: BinaryHeap::new(),
            results: BinaryHeap::new(),
            discarded: Vec::new(),
            expanded: VisitedBitmap::default(),
            in_candidates: VisitedBitmap::default(),
            visited: VisitedBitmap::default(),
            query_bits: query.iter().map(|v| v.to_bits()).collect(),
            last_ef: 0,
            node_count,
            mask_len,
            #[cfg(test)]
            expansion_log: Vec::new(),
        };
        state.expanded.reset(node_count);
        state.in_candidates.reset(node_count);
        state.visited.reset(node_count);

        if k == 0 || node_count == 0 {
            return Ok((Vec::new(), state));
        }
        let Some(entry) = self.entry_point else {
            return Ok((Vec::new(), state));
        };
        let Some(top_level) = self.max_level() else {
            return Ok((Vec::new(), state));
        };

        // I8 常駐（Issue #522）の候補生成は、このクエリの二重量子化
        // （`crate::sq8::prepare_query`）を `search_masked_with_hop` と同様に
        // 呼び出し 1 回につき 1 回だけ準備する（`hnsw::i8_query::
        // PreparedI8Source`）。準備を怠り `self.vectors`（`NodeVectors::score`）
        // を直接使うと、I8 索引で本経路（resumable）だけが
        // `search_masked_with_hop` と異なる（毎回デクォンタイズする）ビームを
        // 辿り、ラウンド 1 のビット同一契約が崩れうる（Cursor Bugbot 指摘）。
        // F32／F16 常駐、または `prepare_query` の失敗時は既存の
        // `NodeVectors::score`（`&self.vectors`）をそのまま使う。
        let prepared_i8;
        let source: &dyn NodeSource = if let NodeVectors::I8 {
            codes,
            params,
            row_sums,
        } = &self.vectors
        {
            prepared_i8 = i8_query::PreparedI8Source::new(codes, row_sums, params, query);
            &prepared_i8
        } else {
            &self.vectors
        };

        // 起点解決は `Self::search_masked_with_hop` と同一ロジック
        // （§関数ドキュメンテーションコメント参照）。
        let (mut nearest, effective_top, checked_entry) = match mask {
            Some(m) => match self.search_entry_for_mask(m) {
                Some(start) if start == entry => (start, top_level, start),
                Some(alt) => {
                    let alt_level = self.level_of(alt).unwrap_or(0);
                    (alt, top_level.min(alt_level), alt)
                }
                None => return Ok((Vec::new(), state)),
            },
            None => (entry, top_level, entry),
        };
        // 本関数は冒頭で `hop == HopMode::TwoHop` を拒否済みのため、ここで
        // `greedy_descend_masked` へ渡す hop は常に `HopMode::OneHop`
        // （橋渡し降下は起動しない・ビット同一契約は不変。Issue #680）。
        let mut descent_bridges = 0u64;
        if effective_top > 0 {
            for l in (1..=effective_top).rev() {
                nearest = match self.greedy_descend_masked(
                    nearest,
                    query,
                    l,
                    dim_usize,
                    source,
                    mask,
                    HopMode::OneHop,
                    &mut descent_bridges,
                )? {
                    Some(n) => n,
                    None => return Ok((Vec::new(), state)),
                };
            }
        }

        let level0_entry_points = if mask.is_some() && checked_entry != nearest {
            vec![nearest, checked_entry]
        } else {
            vec![nearest]
        };

        let ef_eff = ef.max(k);
        for ep in level0_entry_points {
            self.resumable_offer_entry(&mut state, ep, query, mask, dim_usize, source)?;
        }
        self.resumable_run(&mut state, query, mask, dim_usize, ef_eff, source)?;
        state.last_ef = ef_eff;
        let out = resumable_snapshot(&state, k);
        Ok((out, state))
    }

    /// [`ResumableMaskedSearch`] のラウンド r ≥ 2: 整合検査 → 自己昇格 →
    /// 候補復帰 → 探索再開（`docs/design/hnsw-hybrid-iterative-scan.md`
    /// 「再開手順」節）。整合検査（クエリ・索引ノード数・マスク長の一致、
    /// `ef_eff` が単調非減少）に外れた場合は `Err` を返す——呼び出し元
    /// （`sql::hnsw_cache::search_prepared_resumable`）は状態を破棄し
    /// [`Self::search_masked_resumable_start`] からやり直す契約（fail-closed。
    /// 「不整合な状態を黙って使い続けない」ことを優先する）。
    pub(crate) fn search_masked_resume(
        &self,
        state: &mut ResumableMaskedSearch,
        query: &[f32],
        k: usize,
        ef: usize,
        mask: Option<&NodeMask>,
    ) -> Result<Vec<crate::kernel::CandidateHit>, HnswError> {
        let dim_usize = self.dim as usize;
        if query.len() != dim_usize {
            return Err(HnswError::QueryDimMismatch {
                expected: self.dim,
                found: query.len(),
            });
        }
        if query.iter().any(|v| !v.is_finite()) {
            return Err(HnswError::NonFiniteQuery);
        }
        if ef == 0 || ef > MAX_EF {
            return Err(HnswError::InvalidParams {
                reason: "ef must be in 1..=MAX_EF",
            });
        }
        if k > MAX_EF {
            return Err(HnswError::InvalidParams {
                reason: "k exceeds MAX_EF",
            });
        }
        let node_count = self.graph.node_count();
        let mask_len = match mask {
            Some(m) => {
                if m.len() != node_count {
                    return Err(HnswError::InvalidParams {
                        reason: "mask length does not match index node count",
                    });
                }
                Some(m.len())
            }
            None => None,
        };
        let ef_eff = ef.max(k);
        let query_bits: Vec<u32> = query.iter().map(|v| v.to_bits()).collect();
        if state.node_count != node_count
            || state.mask_len != mask_len
            || state.query_bits != query_bits
            || ef_eff < state.last_ef
        {
            return Err(HnswError::InvalidParams {
                reason: "resume state does not match this query/index generation",
            });
        }

        // 自己昇格: results ∪ discarded を全順序（`ScoredNode::Ord`。スコア
        // 降順・同点は id 昇順）で再ソートし、上位 ef_eff 件を results へ、
        // 残りを discarded へ戻す。`ef_eff >= state.last_ef` のため、これは
        // 既存 results（旧 ef_eff 以内で既に採用済みの要素）を一切降格しない
        // 単調な操作である（設計 doc「自己昇格」節の `admitted` 縮約——
        // `results ∪ discarded` の全順序 top-ef_eff は「discarded 全ノードを
        // best-first に再評価し満たすものを results へ直接挿入する」操作と
        // 同値。同値性の根拠は `docs/design/hnsw-hybrid-iterative-scan.md`
        // 「実装記録（Issue #505）」節参照）。
        let mut merged: Vec<ScoredNode> =
            Vec::with_capacity(state.results.len() + state.discarded.len());
        merged.extend(state.results.drain().map(|r| r.0));
        merged.append(&mut state.discarded);
        merged.sort_by(|a, b| b.cmp(a));
        let split = merged.len().min(ef_eff);
        state.results = merged[..split]
            .iter()
            .copied()
            .map(std::cmp::Reverse)
            .collect();
        state.discarded = merged[split..].to_vec();

        // 候補復帰: discarded だけでなく自己昇格で results 側へ移った
        // ノードも含め、merged（今回の re-sort 対象全体）のうち expanded
        // 未設定・in_candidates 未設定のものを candidates へ push する
        // （設計 doc「候補復帰」節）。discarded 限定にすると、直前まで
        // discarded で未展開だったノードが今回の自己昇格で results へ
        // 昇格した場合に候補復帰の対象から漏れ、そのノードの隣接が
        // 一切展開されないまま探索が停止しうる（codex-review・Cursor
        // Bugbot 指摘。PR #619）。push は merged 各要素の内容自体は
        // 変えない——results／discarded どちらに残った要素も最終出力
        // （自己昇格の対象）としては引き続き有効なままで、単に「今回も
        // 隣接探索の起点として再考する」候補に追加で加わるだけ。
        for node in &merged {
            let idx = node.node as usize;
            if state.expanded.is_set(idx) || state.in_candidates.is_set(idx) {
                continue;
            }
            state.candidates.push(*node);
            state.in_candidates.mark_visited(idx);
        }

        // I8 常駐（Issue #522）: `search_masked_resumable_start` と同様、
        // この呼び出し 1 回につき 1 回だけクエリを二重量子化して隣接探索
        // 全体で共有する（Cursor Bugbot 指摘。§`search_masked_resumable_start`
        // ドキュメンテーションコメント参照）。
        let prepared_i8;
        let source: &dyn NodeSource = if let NodeVectors::I8 {
            codes,
            params,
            row_sums,
        } = &self.vectors
        {
            prepared_i8 = i8_query::PreparedI8Source::new(codes, row_sums, params, query);
            &prepared_i8
        } else {
            &self.vectors
        };

        self.resumable_run(state, query, mask, dim_usize, ef_eff, source)?;
        state.last_ef = ef_eff;
        Ok(resumable_snapshot(state, k))
    }

    /// 初期探索起点の発見・受理・ヒープ挿入（`search_layer_in` の entry_points
    /// ループと同一ロジック——`worst_ok` 判定・`results` 容量による追い出しは
    /// 行わない。entry point は通常 1〜2 点のみのため、この非対称は元実装
    /// からそのまま引き継ぐビット同一契約の一部）。
    #[allow(clippy::too_many_arguments)]
    fn resumable_offer_entry(
        &self,
        state: &mut ResumableMaskedSearch,
        node: u32,
        query: &[f32],
        mask: Option<&NodeMask>,
        dim: usize,
        vectors: &dyn NodeSource,
    ) -> Result<(), HnswError> {
        match state.visited.mark_visited(node as usize) {
            Some(true) => return Ok(()),
            Some(false) => {}
            None => return Ok(()),
        }
        let is_accepted = mask.map(|m| m.get(node)).unwrap_or(true);
        if !is_accepted {
            return Ok(());
        }
        let score = vectors.score(dim, node, query)?;
        let scored = ScoredNode { node, score };
        state.candidates.push(scored);
        state.in_candidates.mark_visited(node as usize);
        state.results.push(std::cmp::Reverse(scored));
        Ok(())
    }

    /// 層 0 ビーム探索の本体（`search_layer_in` の while ループ・`HopMode::
    /// OneHop` 経路と同一の停止条件・受理判定・順序規約。停止条件のみ
    /// 「pop してから判定」ではなく「peek で判定してから pop」に変えている
    /// （§ [`ResumableMaskedSearch`] ドキュメンテーションコメント「決定性・
    /// 正しさの保証範囲」参照）。`candidates` から pop したノードが既に
    /// [`ResumableMaskedSearch::expanded`] 済みの場合（候補復帰による重複
    /// push）は隣接走査せず読み捨てる。
    #[allow(clippy::too_many_arguments)]
    fn resumable_run(
        &self,
        state: &mut ResumableMaskedSearch,
        query: &[f32],
        mask: Option<&NodeMask>,
        dim: usize,
        ef_eff: usize,
        vectors: &dyn NodeSource,
    ) -> Result<(), HnswError> {
        let is_accepted = |node: u32| mask.map(|m| m.get(node)).unwrap_or(true);
        loop {
            let stop = match (state.candidates.peek(), state.results.peek()) {
                (Some(top), Some(std::cmp::Reverse(worst))) => {
                    state.results.len() >= ef_eff
                        && top.score.total_cmp(&worst.score) == std::cmp::Ordering::Less
                }
                _ => false,
            };
            if stop {
                break;
            }
            let Some(top_candidate) = state.candidates.pop() else {
                break;
            };
            if state.expanded.mark_visited(top_candidate.node as usize) == Some(true) {
                continue;
            }
            #[cfg(test)]
            state.expansion_log.push(top_candidate.node);
            let Some(neighbors) = self.graph.neighbors(0, top_candidate.node) else {
                continue;
            };
            for &neighbor in neighbors {
                let already = match state.visited.mark_visited(neighbor as usize) {
                    Some(seen) => seen,
                    None => continue,
                };
                if already {
                    continue;
                }
                if !is_accepted(neighbor) {
                    // TwoHop（ACORN-1・`bridge_expand`）は本経路では未対応
                    // （§ [`ResumableMaskedSearch`] ドキュメンテーション
                    // コメント参照。呼び出し元が `hop == TwoHop` を拒否する）。
                    continue;
                }
                let neighbor_score = vectors.score(dim, neighbor, query)?;
                let scored = ScoredNode {
                    node: neighbor,
                    score: neighbor_score,
                };
                let worst_ok = match state.results.peek() {
                    Some(std::cmp::Reverse(worst)) => {
                        state.results.len() < ef_eff
                            || scored.score.total_cmp(&worst.score) != std::cmp::Ordering::Less
                    }
                    None => true,
                };
                if worst_ok {
                    state.candidates.push(scored);
                    state.in_candidates.mark_visited(neighbor as usize);
                    state.results.push(std::cmp::Reverse(scored));
                    if state.results.len() > ef_eff {
                        if let Some(std::cmp::Reverse(evicted)) = state.results.pop() {
                            state.discarded.push(evicted);
                        }
                    }
                } else {
                    state.discarded.push(scored);
                }
            }
        }
        Ok(())
    }
}

/// [`ResumableMaskedSearch::results`] を出力順（スコア降順・同点は id 昇順。
/// `search_layer_in` の最終ソートと同一の比較述語——[`make sort-determinism-
/// check`] が拾う `sort_unstable_*` ではなく安定な `sort_by` を使う）に整列し
/// 上位 `k` 件を返す。`state.results` 自体は消費しない（呼び出し元が次ラウンド
/// も保持し続けるため）。
fn resumable_snapshot(state: &ResumableMaskedSearch, k: usize) -> Vec<crate::kernel::CandidateHit> {
    let mut out: Vec<ScoredNode> = state.results.iter().map(|r| r.0).collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
    out.into_iter()
        .take(k)
        .map(|s| crate::kernel::CandidateHit {
            id: s.node as u64,
            score: s.score,
        })
        .collect()
}

/// TwoHop（ACORN-1）経路限定の実効 `ef` 底上げ（Issue #680）。
///
/// 低可視比率（マスクが疎）の TwoHop 経路では、[`bridge_expand`] の橋渡し
/// 展開で 1 クエリあたり多数の遠方候補（非受理ノードの 2-hop 先）が幅 `ef`
/// のビームへ流入し、`results` ヒープが遠方候補で埋まって早期打ち切りを
/// 誘発する（Issue #674 Phase 2 の実測）。可視比率が低いほど（＝橋渡しで
/// 混入する遠方候補の割合が高いほど）`ef` を大きく底上げすることで、真に
/// 近い受理ノードがヒープから押し出される事故を緩和する。
///
/// - `base = ef.max(k)`（既存の `ef_eff` 計算と同じ。呼び出し元は `hop`・
///   `mask` に応じてこの関数を呼ぶか `ef.max(k)` をそのまま使うかを切り替える
///   ——本関数自体は TwoHop 判定を持たない純粋関数）。
/// - `visible == 0` は呼び出し元で到達不能（`is_mask_fully_reachable_with`
///   が false）となり `search_masked_with_hop` 自体が空集合を返す経路のため
///   実質到達しないが、ゼロ除算を避け `base` をそのまま返す（防御的）。
/// - `scale = node_count.div_ceil(visible)`（可視比率の逆数の切り上げ）を
///   `1..=ACORN_EF_SCALE_MAX` へクランプする——`--hnsw-full-scan-ratio`
///   （Issue #657）で可視比率の下限が既定域から外れた呼び出しでも `ef_eff`
///   が無制限に膨らまないための安全弁。
/// - 最終結果は `MAX_EF` でもクランプする（呼び出し元の `ef`・`k` は既に
///   `MAX_EF` 以下と検証済みだが、乗算後の値がそれを超えないことをここでも
///   保証する。二重の安全弁）。
/// - `saturating_mul`／`div_ceil` を使い、untrusted な `node_count`・
///   `visible`（wire 経由の SCALAR フィルタ選択率に依存）に対しても整数
///   オーバーフローを未定義動作にしない（`coding-rust.md`「untrusted 入力の
///   扱い」）。
pub(crate) fn two_hop_effective_ef(
    ef: usize,
    k: usize,
    node_count: usize,
    visible: usize,
) -> usize {
    let base = ef.max(k);
    if visible == 0 {
        return base;
    }
    let scale = node_count.div_ceil(visible).clamp(1, ACORN_EF_SCALE_MAX);
    base.saturating_mul(scale).min(MAX_EF)
}

/// TwoHop（ACORN-1）探索 1 回の橋渡し展開件数 `expansions`（層 0
/// [`bridge_expand`] の受理件数）が可視ノード数 `visible` に対する上限比
/// `ratio` を超えたかを判定する純粋関数（Issue #681・親 #674）。
///
/// `sql::hnsw_cache::search_with_overlay` が TwoHop 完走直後（事後）に
/// `true` を返された場合、その ANN 結果を捨てて plain scan（既存の縮退経路。
/// `full_scan_with_arena`）へ fail-closed に切り替える——探索コストは
/// 既に払っているが、展開過多による Recall 低下（Issue #674 Phase 2 の
/// 実測）より結果の正しさを優先する。
///
/// - `expansions * ratio.denominator > visible * ratio.numerator`
///   （`below_full_scan_ratio` と同じ整数交差乗算の比較方式に揃える）。
/// - `u64::checked_mul` を使い、untrusted な `expansions`／`visible`
///   （wire 経由の SCALAR フィルタ選択率・クエリ内容に依存し得る）に対して
///   整数オーバーフローを未定義動作にしない。オーバーフロー時は縮退側
///   （`true`）へ fail-closed に倒す（`coding-rust.md`「untrusted 入力の扱い」）。
/// - `visible == 0` は呼び出し元（`Overlay::compute` の分断検査）で通常
///   到達不能だが、防御的に `expansions > 0` なら縮退側（`true`）へ倒す
///   （ゼロ除算を避けつつ fail-closed の方向を保つ）。
/// - 層 0 の `bridge_expand` は各 2-hop ノードを高々 1 回しか
///   `visited.mark_visited` を通さない（`bridge_expand` ドキュメンテーション
///   コメント「停止性」節）ため `expansions <= visible` が構造的に成立する。
///   したがって `ratio = 1/1` は構造的に発火不能（常に `false`）。
pub(crate) fn acorn_expansions_exceed(expansions: u64, visible: usize, ratio: Ratio) -> bool {
    if visible == 0 {
        return expansions > 0;
    }
    let lhs = expansions.checked_mul(ratio.denominator as u64);
    let rhs = (visible as u64).checked_mul(ratio.numerator as u64);
    match (lhs, rhs) {
        (Some(lhs), Some(rhs)) => lhs > rhs,
        // オーバーフロー時は fail-closed に縮退側へ倒す。
        _ => true,
    }
}

/// [`HnswIndex::search_layer_with`]／[`GraphBuilder::search_layer`] が
/// 共有する `search_layer`（Algorithm 2）の本体（Issue #494。凍結後の
/// [`csr::CsrGraph`]・構築中の [`GraphBuilder`] のどちらの隣接表現からも
/// 呼べるよう [`Adjacency`] でジェネリック化した自由関数）。層 `level` 上で
/// `entry_points` から出発し、幅 `ef` の貪欲拡張探索を行い、`dot` 降順
/// （同点は id 昇順）に並んだ最大 `ef` 件の候補を返す。
///
/// `visited` は呼び出し元（`insert_node`／`build` あるいはテスト）が全
/// 呼び出しをまたいで所有する visited 集合（[`VisitedSet`]）。構築経路は
/// 世代カウンタ方式の [`VisitedScratch`]、探索経路（[`HnswIndex::search`]）は
/// ビットマップ方式の [`VisitedBitmap`] を渡す。`accept`（Issue #409）が
/// `Some` の場合、受理しないノードは候補ヒープへも一切積まない——訪問済み
/// マークは付けるが、スコア計算（索引ノードのベクトルへのアクセスを伴う）
/// 自体を行わず、その隣接ノードへの探索も一切行わない（Issue #431・
/// codex-review P0 是正。`docs/design/ann-index-adoption.md`「RLS／
/// フィルタとの相互作用と折衷案」節の P0 安全条件）。`None` の場合は常に
/// 受理したのと同じ振る舞いになる（`search_masked_none_matches_search` 参照）。
/// 非受理（不適合）1-hop ノード `bridge` の隣接リストを 1 段だけ中継点として
/// 辿り、未訪問かつ受理の 2-hop ノードを `on_accepted` へ渡す（ACORN-1・
/// Issue #501。呼び出し元は [`search_layer_in`]（ビーム探索）・
/// [`HnswIndex::accepted_reachable_count`]（BFS 到達可能性検査）の 2 者で、
/// 双方が同一実装を共有することで「非受理ノードに出会ったときの規則」を
/// ビット同一に保つ（`docs/design/hnsw-rls-cardinality-switch.md`
/// 「Issue #501」節「同期」参照。BFS と探索が異なる規則を実装すると、
/// 偽の分断判定（2-hop が発火しない）か偽の到達可能判定（recall バグ）の
/// どちらかを招く）。
///
/// # 停止性（R2・DoS ガード）
///
/// `bridge` は呼び出し元が「1-hop 非受理として初めて visited へ記録した
/// 直後」にのみ渡す契約（`bridge` 自身は既に visited 済み）。本関数はその
/// `bridge` の隣接リストを読むだけで、2-hop 先が非受理の場合は visited を
/// 一切付けない（別の 1-hop 経路から改めて中継点として使えるようにする
/// ためだが、その別経路が `bridge_expand` を呼ぶのは「その 2-hop ノード
/// 自身が誰かの 1-hop 非受理隣接として初めて visited されたとき」のみ——
/// つまり本関数が呼ばれる回数はクエリ全体で「1-hop 非受理として visited
/// された回数」以下に構造的に有界であり、各ノードは高々 1 回しか
/// `visited.mark_visited` を通過しない。したがって隣接リスト読み取りの
/// 総量はマスクの有無によらず `O(N・M0)`（`N`＝索引ノード数・`M0`＝層 0 の
/// 最大次数）で有界（`docs/design/hnsw-rls-cardinality-switch.md`
/// 「Issue #501」節「停止性」参照。明示的な訪問予算上限は設けない）。
///
/// # I1（ベクトル非参照）不変
///
/// 非受理ノード（`bridge` 自身・2-hop 先が非受理の場合）のベクトルには
/// 一切アクセスしない——`vectors.score` を呼ぶのは `on_accepted` へ渡す
/// 受理済み 2-hop ノードのみ（呼び出し元がスコア計算を担う。本関数自体は
/// スコア計算を行わない）。
///
/// 戻り値は `on_accepted` へ渡した（受理・候補化した）2-hop ノード数
/// （統計用。`HnswSearchScratch::last_acorn_expansions`・
/// `sql::hnsw_cache::HnswIndexCacheStats::acorn_expansions` が使う）。
fn bridge_expand<A: Adjacency, V: VisitedSet>(
    graph: &A,
    level: usize,
    bridge: u32,
    accept: &NodeMask,
    visited: &mut V,
    mut on_accepted: impl FnMut(u32) -> Result<(), HnswError>,
) -> Result<usize, HnswError> {
    let mut expanded = 0usize;
    let Some(neighbors) = graph.neighbors(level, bridge) else {
        return Ok(0);
    };
    for &two_hop in neighbors {
        if !accept.get(two_hop) {
            // 非受理の 2-hop ノードは visited を付けない（§関数ドキュメン
            // テーションコメント「停止性」参照。別の 1-hop 非受理隣接
            // からも中継点として使えるようにするため）。3-hop（さらに先）
            // へは辿らない——展開は 1 段のみ。
            continue;
        }
        // 受理ノードは通常の 1-hop 受理隣接と同じ「visited を先に付けてから
        // 処理する」規約（Issue #431 是正済みの既存規則）に合わせる。
        match visited.mark_visited(two_hop as usize) {
            Some(false) => {}
            _ => continue,
        }
        on_accepted(two_hop)?;
        expanded = expanded.saturating_add(1);
    }
    Ok(expanded)
}

#[allow(clippy::too_many_arguments)]
fn search_layer_in<
    V: VisitedSet,
    P: prefetch::PrefetchPolicy,
    A: Adjacency,
    S: NodeSource + ?Sized,
>(
    graph: &A,
    entry_points: Vec<u32>,
    query: &[f32],
    ef: usize,
    level: usize,
    dim: usize,
    vectors: &S,
    visited: &mut V,
    accept: Option<&NodeMask>,
    prefetch: &P,
    hop: HopMode,
    acorn_expansions: &mut u64,
) -> Result<Vec<ScoredNode>, HnswError> {
    visited.reset(graph.node_count());
    let mut candidates: BinaryHeap<ScoredNode> = BinaryHeap::new();
    // 結果集合は最小ヒープとして扱いたいので `Reverse` で包む。
    let mut results: BinaryHeap<std::cmp::Reverse<ScoredNode>> = BinaryHeap::new();
    let is_accepted = |node: u32| accept.map(|m| m.get(node)).unwrap_or(true);

    for ep in entry_points {
        match visited.mark_visited(ep as usize) {
            Some(true) => continue,
            Some(false) => {}
            None => continue,
        }
        if !is_accepted(ep) {
            // 非受理（stale・不可視）ノードは候補ヒープへも一切積まない
            // （§関数ドキュメンテーションコメント参照。訪問済みマークのみ
            // 付けてスコア計算・以降の探索を行わない）。
            continue;
        }
        let score = vectors.score(dim, ep, query)?;
        let scored = ScoredNode { node: ep, score };
        candidates.push(scored);
        results.push(std::cmp::Reverse(scored));
    }

    while let Some(top_candidate) = candidates.pop() {
        // 候補集合の最良要素が、結果集合中の最悪要素より「厳密に」劣るなら
        // 打ち切る（Algorithm 2 の停止条件）。ここは `ScoredNode::cmp`（id
        // 昇順タイブレーク込みの複合順序）ではなく **スコアのみ**の比較に
        // 限定する。複合順序で判定すると、スコアが同点で id が大きいだけの
        // 候補まで「より遠い」と誤判定して打ち切ってしまい、その候補の
        // 未訪問隣接ノードがより近い可能性を探索し損なう（同点候補が
        // 生じやすい重複 embedding で顕在化。
        // `docs/design/hnsw-graph-construction.md`「`search_layer` の
        // 停止・受理判定: 順序規約の使い分け」節参照）。
        // id 順の複合順序は結果集合の内容（`results.pop()` によるヒープ
        // 内での追い出し順）・最終出力の安定ソートでのみ使い、探索を続ける
        // か否かの判定には使わない。
        if let Some(std::cmp::Reverse(worst)) = results.peek() {
            let strictly_farther =
                top_candidate.score.total_cmp(&worst.score) == std::cmp::Ordering::Less;
            if results.len() >= ef && strictly_farther {
                break;
            }
        }

        if let Some(neighbors) = graph.neighbors(level, top_candidate.node) {
            // Issue #490: hnswlib `searchBaseLayerST` に倣うソフトウェア
            // パイプライン先読み。隣接リストの先頭要素をループ開始前に、
            // 以降は各反復 `j` の先頭で `j+1` 番目を先読みする（距離 1）。
            // 受理判定後にのみ触れる P0 契約（Issue #431 是正。§関数
            // ドキュメンテーションコメント参照）を守るため、`is_accepted`
            // を通過したノードのみを先読み対象にする——`is_accepted` は
            // `NodeMask::get` の純粋なビット判定で副作用を持たないため、
            // 自身の反復時に再評価しても意味は変わらない。
            if let Some(&first) = neighbors.first() {
                if is_accepted(first) {
                    prefetch.prefetch_neighbor(first, visited, vectors, dim);
                }
            }
            for (j, &neighbor) in neighbors.iter().enumerate() {
                if let Some(&next) = neighbors.get(j + 1) {
                    if is_accepted(next) {
                        prefetch.prefetch_neighbor(next, visited, vectors, dim);
                    }
                }
                let already = match visited.mark_visited(neighbor as usize) {
                    Some(seen) => seen,
                    None => continue,
                };
                if already {
                    continue;
                }
                if !is_accepted(neighbor) {
                    // 非受理ノードは訪問済みにするのみで候補ヒープへは
                    // 積まない（Issue #431 是正。§関数ドキュメンテーション
                    // コメント参照）。スコア計算（このノードのベクトルへの
                    // アクセス）自体を行わず、この隣接ノード経由でのさらに
                    // 先の探索も一切行わない——ただし `hop == TwoHop`
                    // （Issue #501・ACORN-1）のときのみ、この非受理ノードを
                    // 1 段だけ中継点として使い、その先（2-hop）にいる受理
                    // ノードを [`bridge_expand`] 経由で候補化する。非受理
                    // ノード自身のベクトルには一切アクセスしない（I1 不変。
                    // §関数ドキュメンテーションコメント参照）。
                    if hop == HopMode::TwoHop {
                        if let Some(mask) = accept {
                            let expanded =
                                bridge_expand(graph, level, neighbor, mask, visited, |two_hop| {
                                    let score = vectors.score(dim, two_hop, query)?;
                                    let scored = ScoredNode {
                                        node: two_hop,
                                        score,
                                    };
                                    let worst_ok = match results.peek() {
                                        Some(std::cmp::Reverse(worst)) => {
                                            results.len() < ef
                                                || scored.score.total_cmp(&worst.score)
                                                    != std::cmp::Ordering::Less
                                        }
                                        None => true,
                                    };
                                    if worst_ok {
                                        candidates.push(scored);
                                        results.push(std::cmp::Reverse(scored));
                                        if results.len() > ef {
                                            results.pop();
                                        }
                                    }
                                    Ok(())
                                })?;
                            *acorn_expansions = acorn_expansions.saturating_add(expanded as u64);
                        }
                    }
                    continue;
                }
                let neighbor_score = vectors.score(dim, neighbor, query)?;
                let scored = ScoredNode {
                    node: neighbor,
                    score: neighbor_score,
                };
                // 打ち切り判定と同じ理由でスコアのみの比較に限定する
                // （`scored` が `worst` とスコア同点なら、id 順の複合順序で
                // 「劣る」と判定されても受理する）。`worst_ok` を満たす
                // 隣接ノードは（上の `is_accepted` チェックを通過済みのため）
                // 候補ヒープ・結果ヒープの双方へ積む。
                let worst_ok = match results.peek() {
                    Some(std::cmp::Reverse(worst)) => {
                        results.len() < ef
                            || scored.score.total_cmp(&worst.score) != std::cmp::Ordering::Less
                    }
                    None => true,
                };
                if worst_ok {
                    candidates.push(scored);
                    results.push(std::cmp::Reverse(scored));
                    if results.len() > ef {
                        results.pop();
                    }
                }
            }
        }
    }

    let mut out: Vec<ScoredNode> = results.into_iter().map(|r| r.0).collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
    Ok(out)
}

/// row-major バッファから `node` 番目のベクトルスライスを取り出す。範囲外
/// アクセスは `[]` を使わず `get()` で検出し、`CapacityOverflow` として拒否する
/// （coding-rust.md: untrusted 添字アクセス禁止。本関数はモジュール内部専用だが
/// `node_id` は `u32::try_from` 済みの構築時検証を経ているため、通常この分岐へは
/// 到達しない防御的経路である）。
fn node_vector(vectors: &[f32], dim: usize, node: u32) -> Result<&[f32], HnswError> {
    let start = (node as usize)
        .checked_mul(dim)
        .ok_or(HnswError::CapacityOverflow)?;
    let end = start.checked_add(dim).ok_or(HnswError::CapacityOverflow)?;
    vectors.get(start..end).ok_or(HnswError::CapacityOverflow)
}

/// [`GraphBuilder::repair_reachability_inner`] のフェーズ 1 が使う最近傍探索
/// （[`nearest_reachable`]）の並列度を決める方針関数（Issue #449。方針
/// （何本立てるか）と機構（実際の分割走査。[`nearest_reachable`] 側）を分離し、
/// それぞれを独立にテストできるようにする）。
///
/// `crate::parallel_search::thread_count_for`（検索側の並列度決定と同一関数。
/// `MIN_ROWS_PER_THREAD` による小規模時の 1 本への縮退を含む）を到達済み集合
/// のサイズで評価し、構築側が引き継いだ `threads` 上限でさらにクランプする。
/// `threads==1`（`build`・`build_with_threads` の縮退経路）では常に 1 を返し、
/// [`nearest_reachable`] は並列分岐を一切通らない。
fn repair_workers_for(reachable_len: usize, threads: usize) -> usize {
    crate::parallel_search::thread_count_for(reachable_len).min(threads.max(1))
}

/// `current` と `candidate`（`(id, score)`）のうち、モジュール冒頭「順序規約」
/// （スコア `total_cmp` 降順・同点は id 昇順）に従って採用すべき方を返す。
/// この規約は全順序を成すため、[`nearest_reachable`] が到達集合をどう分割し・
/// 各ワーカーの局所最良をどの順序で縮約しても、最終的に選ばれる候補は分割・
/// 縮約の順序に依存せず一意に定まる（`repair_reachability_inner` の
/// ドキュメンテーションコメント「探索の並列化」参照）。
fn better_repair_candidate(
    current: Option<(u32, f32)>,
    candidate: (u32, f32),
) -> Option<(u32, f32)> {
    match current {
        None => Some(candidate),
        Some((cur_id, cur_score)) => match candidate.1.total_cmp(&cur_score) {
            std::cmp::Ordering::Greater => Some(candidate),
            std::cmp::Ordering::Equal if candidate.0 < cur_id => Some(candidate),
            _ => current,
        },
    }
}

/// `candidates`（`reachable` の全体または 1 ワーカー分のチャンク）を逐次走査し
/// `query` に最も近い（`dot` 最大・同点 id 昇順）候補を返す（[`nearest_reachable`]
/// の逐次経路・並列ワーカー本体の双方が共有する機構）。非有限スコアは
/// `HnswError::NonFiniteScore` として拒否する（`repair_reachability_inner` の
/// 既存契約と同一。モジュール冒頭「距離カーネル」節参照）。
fn nearest_reachable_scan(
    vectors: &[f32],
    dim: usize,
    query: &[f32],
    candidates: &[u32],
) -> Result<Option<(u32, f32)>, HnswError> {
    let mut best: Option<(u32, f32)> = None;
    for &candidate in candidates {
        let cand_vec = node_vector(vectors, dim, candidate)?;
        let score = dot(query, cand_vec);
        if !score.is_finite() {
            return Err(HnswError::NonFiniteScore { node: candidate });
        }
        best = better_repair_candidate(best, (candidate, score));
    }
    Ok(best)
}

/// 到達済み集合 `reachable`（id 昇順）から `node` に最も近い候補を返す
/// （[`GraphBuilder::repair_reachability_inner`] フェーズ 1 が使う読み取り
/// 専用の探索。Issue #449）。`workers<=1` または `reachable.len()<=1` では
/// 分割せず [`nearest_reachable_scan`] を直接呼ぶ（ワーカーを一切起動しない
/// 逐次経路。`threads==1` の `build`・`build_with_threads` 縮退経路はここへ
/// 到達する）。`workers>=2` では `reachable` を `workers` 個の連続チャンクへ
/// 分割し、各チャンクを別スレッド（`std::thread::scope`）で
/// [`nearest_reachable_scan`] に掛けて局所最良を求め、
/// [`better_repair_candidate`] の全順序で縮約する——縮約順序に依存せず結果は
/// 一意に定まるため、`workers` の値によらずビット同一の結果を返す
/// （呼び出し元 `repair_reachability_inner` のドキュメンテーションコメント
/// 「探索の並列化」参照）。
///
/// ワーカーの panic（`join` 失敗）は `HnswError::WorkerPanicked` として
/// 構築全体を拒否する（`parallel_build::build_parallel_graph` の
/// `first_error`／`any_panicked` パターンと同型の fail-closed 契約。部分的に
/// 探索したまま `Ok` を返さない）。
fn nearest_reachable(
    vectors: &[f32],
    dim: usize,
    node: u32,
    reachable: &[u32],
    workers: usize,
) -> Result<Option<(u32, f32)>, HnswError> {
    let query = node_vector(vectors, dim, node)?;
    if workers <= 1 || reachable.len() <= 1 {
        return nearest_reachable_scan(vectors, dim, query, reachable);
    }
    #[cfg(test)]
    REPAIR_PARALLEL_LAUNCHES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let chunk_len = reachable.len().div_ceil(workers).max(1);
    let first_error: std::sync::Mutex<Option<HnswError>> = std::sync::Mutex::new(None);
    let (any_panicked, locals): (bool, Vec<Option<(u32, f32)>>) = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for chunk in reachable.chunks(chunk_len) {
            let first_error_ref = &first_error;
            handles.push(scope.spawn(move || -> Option<(u32, f32)> {
                match nearest_reachable_scan(vectors, dim, query, chunk) {
                    Ok(best) => best,
                    Err(e) => {
                        let mut fe = first_error_ref
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        if fe.is_none() {
                            *fe = Some(e);
                        }
                        None
                    }
                }
            }));
        }
        let mut any_panicked = false;
        let mut locals = Vec::with_capacity(handles.len());
        for h in handles {
            match h.join() {
                Ok(v) => locals.push(v),
                Err(_) => {
                    any_panicked = true;
                    locals.push(None);
                }
            }
        }
        (any_panicked, locals)
    });
    if any_panicked {
        return Err(HnswError::WorkerPanicked);
    }
    if let Some(e) = first_error
        .into_inner()
        .unwrap_or_else(|poison| poison.into_inner())
    {
        return Err(e);
    }
    let mut best: Option<(u32, f32)> = None;
    for local in locals.into_iter().flatten() {
        best = better_repair_candidate(best, local);
    }
    Ok(best)
}

/// [`nearest_reachable`] が `workers>=2` の並列分岐を実際に通った回数
/// （テスト専用の非 vacuous 検証カウンタ。Issue #449。`#[cfg(test)]` の
/// 内外で完全に消える——production バイナリには一切残らない）。
#[cfg(test)]
static REPAIR_PARALLEL_LAUNCHES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// `build`（`HnswParams::validate` → 次元整合 → ノード数上限 → 非有限値の
/// 検証順序。モジュール `build` ドキュメンテーションコメント参照）と
/// `build_with_threads`／`build_parallel`（Issue #406）が共有する入力検証。
/// 検証済みノード数 `n` を返す。
fn validate_build_input(
    params: &HnswParams,
    dim: u32,
    vectors: &[f32],
) -> Result<usize, HnswError> {
    params.validate()?;

    if dim == 0 {
        return Err(HnswError::DimMismatch {
            dim,
            len: vectors.len(),
        });
    }
    let dim_usize = dim as usize;
    if !vectors.len().is_multiple_of(dim_usize) {
        return Err(HnswError::DimMismatch {
            dim,
            len: vectors.len(),
        });
    }
    let n = vectors.len() / dim_usize;
    if n > MAX_HNSW_NODES {
        return Err(HnswError::TooManyNodes { nodes: n });
    }
    // ノード id を u32 で表現できることを構築前に確定させる（MAX_HNSW_NODES は
    // u32::MAX よりずっと小さいため通常は失敗しないが、上限定数の将来変更に
    // 備えて明示的に検証する。coding-rust.md: `checked_*`／`try_into` の使用）。
    if u32::try_from(n).is_err() {
        return Err(HnswError::CapacityOverflow);
    }

    for (node_idx, chunk) in vectors.chunks_exact(dim_usize).enumerate() {
        if chunk.iter().any(|v| !v.is_finite()) {
            return Err(HnswError::NonFiniteVector { node: node_idx });
        }
    }

    Ok(n)
}

/// `dot(node, query)`。[`HnswIndex::score`] と並列構築（`parallel_build`。
/// Issue #406）の `BuildGraph` の双方から共有される純粋関数（`self` を
/// 参照しないため元々自然にジェネリック化できた）。境界検証は
/// [`node_vector`] が担う。結果が非有限なら `NonFiniteScore` として拒否する
/// （モジュール冒頭「順序規約」節参照）。
fn score_of(vectors: &[f32], dim: usize, node: u32, query: &[f32]) -> Result<f32, HnswError> {
    let v = node_vector(vectors, dim, node)?;
    let d = dot(v, query);
    if !d.is_finite() {
        return Err(HnswError::NonFiniteScore { node });
    }
    Ok(d)
}

/// 近傍選択ヒューリスティック（Algorithm 4）の本体。[`HnswIndex::
/// select_neighbors_heuristic`] と並列構築の `BuildGraph` の双方から共有される
/// 純粋関数。既定は `extend_candidates=false`・`keep_pruned_connections=true`
/// （余った枠を枝刈り済み候補で埋め、次数を確保する）。この既定の採用理由は
/// `docs/design/hnsw-graph-construction.md` に記録する。
fn select_neighbors_heuristic_free(
    candidates: &[ScoredNode],
    m: usize,
    dim: usize,
    vectors: &[f32],
) -> Result<Vec<u32>, HnswError> {
    const KEEP_PRUNED_CONNECTIONS: bool = true;

    // 候補は search_layer が既にスコア降順で返すため、優先度付きキューへ
    // 詰め直す代わりにそのまま消費できるが、Algorithm 4 の記法に合わせて
    // 「未処理候補」を降順に保った Vec として扱う。
    let working: Vec<ScoredNode> = candidates.to_vec();

    let mut selected: Vec<ScoredNode> = Vec::new();
    let mut discarded: Vec<ScoredNode> = Vec::new();

    for cand in working {
        if selected.len() >= m {
            break;
        }
        let cand_vec = node_vector(vectors, dim, cand.node)?;
        // 「候補が既選択集合のどの要素よりも近い場合のみ採用する」枝刈り規則
        // （Algorithm 4）。dot は大きいほど近いため `>` が「より近い」の向き。
        let mut keep = true;
        for &sel in &selected {
            let sel_vec = node_vector(vectors, dim, sel.node)?;
            let d_to_selected = dot(cand_vec, sel_vec);
            if !d_to_selected.is_finite() {
                return Err(HnswError::NonFiniteScore { node: cand.node });
            }
            if d_to_selected > cand.score {
                keep = false;
                break;
            }
        }
        if keep {
            selected.push(cand);
        } else {
            discarded.push(cand);
        }
    }

    if KEEP_PRUNED_CONNECTIONS {
        let mut i = 0;
        while selected.len() < m {
            let Some(extra) = discarded.get(i) else {
                break;
            };
            selected.push(*extra);
            i += 1;
        }
    }

    selected.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
    Ok(selected.into_iter().map(|s| s.node).collect())
}

/// [`HnswIndex::shrink_links`] と並列構築の `BuildGraph::shrink_links` の
/// 双方から共有される、縮退の計算のみを行う純粋関数（読み書き分離）。
/// `current_links` の要素数が `limit` 以下かつ `protect` を含む（または
/// `node == protect`）なら `Ok(None)`（変更不要）。それ以外は
/// `Ok(Some(new_links))` を返す。`protect` を強制的に残す契約・順序規約は
/// [`HnswIndex::shrink_links`] のドキュメンテーションコメント参照。
/// 呼び出し元（逐次経路は `&mut self.nodes` への 2 回のアクセス、並列
/// 経路は 1 回の書き込みロック内）が書き戻しを担う。
///
/// PR #431 codex-review（Cursor Bugbot）Medium 指摘の修正: 並列構築
/// （`BuildGraph::shrink_links`）では `connect(node, protect, level)` と
/// この関数を呼ぶ `shrink_links(node, level, protect)` が別々のロック
/// 獲得（[`BuildGraph::connect`]・[`BuildGraph::shrink_links`]）であるため、
/// その間に別ワーカーが同じ `node` へ異なる `protect` で `shrink_links` を
/// 実行し、こちらの `protect` を `current_links` から先に落とすレースが
/// 起こり得る。旧実装は「`protect` は `current_links`（延いては `scored`）
/// に必ず含まれる」という、逐次構築（`HnswIndex::insert_node`。同一スレッド
/// 内で `connect` 直後に `shrink_links` が走り割り込みが無い）でのみ成立する
/// 前提に依存しており、`protect` を明示的に `push` した直後に
/// `scored`（`current_links` 由来。`protect` を含まない場合がある）で
/// 再フィルタしてしまい `push` した `protect` を再び落としていた。
/// 本実装は `protect` が `current_links` に無い場合も `node_vector`
/// から直接そのスコアを算出し、`protect` を含む完全なスコア表
/// （`all_scored`）で再フィルタすることで、`protect` が最終的な
/// 結果集合（`limit` 件以内）に必ず残ることを保証する。
fn compute_shrink(
    current_links: &[u32],
    node: u32,
    dim: usize,
    vectors: &[f32],
    limit: usize,
    protect: u32,
) -> Result<Option<Vec<u32>>, HnswError> {
    let protect_present = node == protect || current_links.contains(&protect);
    if current_links.len() <= limit && protect_present {
        return Ok(None);
    }

    let node_vec = node_vector(vectors, dim, node)?;
    let mut scored: Vec<ScoredNode> = Vec::with_capacity(current_links.len());
    for &id in current_links {
        let v = node_vector(vectors, dim, id)?;
        let d = dot(node_vec, v);
        if !d.is_finite() {
            return Err(HnswError::NonFiniteScore { node: id });
        }
        scored.push(ScoredNode { node: id, score: d });
    }
    scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));

    let mut shrunk = select_neighbors_heuristic_free(&scored, limit, dim, vectors)?;
    // `node == protect`（自己ループ）は `connect` が張らないため起こらないが、
    // 呼び出し契約が壊れても panic せず何もしない防御的分岐にしておく。
    if node != protect && !shrunk.contains(&protect) {
        if shrunk.len() >= limit {
            // `select_neighbors_heuristic_free` は高々 `limit` 件しか返さない
            // ため、この分岐に来る時点で `shrunk.len() == limit` であり、
            // `limit >= 2`（`HnswParams::validate` の `m >= 2` から層 0 も
            // 層 1 以上も導出される）なので `pop` は必ず要素を持つ。
            shrunk.pop();
        }
        shrunk.push(protect);
        // `protect` が `current_links`（延いては `scored`）に無い場合が
        // あり得るため（上記ドキュメンテーションコメント参照）、`scored`
        // をそのまま並び順の基準にはできない。`protect` 自身のスコアを
        // 直接算出し、`scored` に無ければ補ったうえで、差し替え後も
        // 「スコア降順・同点は id 昇順」の順序（モジュール冒頭の順序規約）
        // を保つよう並び替える。
        let mut all_scored = scored.clone();
        if !all_scored.iter().any(|s| s.node == protect) {
            let protect_vec = node_vector(vectors, dim, protect)?;
            let protect_score = dot(node_vec, protect_vec);
            if !protect_score.is_finite() {
                return Err(HnswError::NonFiniteScore { node: protect });
            }
            all_scored.push(ScoredNode {
                node: protect,
                score: protect_score,
            });
        }
        all_scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
        shrunk = all_scored
            .iter()
            .filter(|s| shrunk.contains(&s.node))
            .map(|s| s.node)
            .collect();
    }
    Ok(Some(shrunk))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gen_corpus(seed: u64, dim: usize, rows: usize) -> Vec<f32> {
        let mut rng = DeterministicRng::new(seed);
        let mut out = Vec::with_capacity(rows * dim);
        for _ in 0..rows {
            for _ in 0..dim {
                let bits = rng.next_u64() >> 40;
                let f = (bits as f32) / (1u32 << 24) as f32;
                out.push(f * 2.0 - 1.0);
            }
        }
        out
    }

    /// 直接組んだ `Node` 列（可変長ビルダー表現）から `HnswIndex` を構成する
    /// テスト専用ヘルパ（Issue #494・凍結時の CSR 平坦化）。`HnswIndex` の
    /// 内部表現が `CsrGraph`（[`csr::CsrGraph`]）へ変わったため、テストは
    /// `build`／並列構築を経由せず直接グラフ形状を指定したい場合でも
    /// [`GraphBuilder`] を経由してから [`HnswIndex::freeze_from`] で凍結する
    /// 必要がある。`repair_reachability` は呼ばない（テストが指定したグラフ
    /// 形状をそのまま保持するため。呼ぶとテストが意図的に作った未修復の
    /// 状態が変わってしまう）。
    fn index_from_nodes(
        params: HnswParams,
        dim: u32,
        nodes: Vec<Node>,
        entry_point: Option<u32>,
        vectors: Arc<[f32]>,
    ) -> HnswIndex {
        let builder = GraphBuilder {
            params,
            nodes,
            entry_point,
        };
        HnswIndex::freeze_from(builder, dim, vectors, ResidentPrecision::F32)
            .expect("test fixture nodes must be valid for CSR flattening")
    }

    /// Issue #490: prefetch を一切行わない `PrefetchPolicy`（テスト専用）。
    /// `PipelinePrefetch` とのビット同一性の対照として使う。production
    /// バイナリには到達しない（`#[cfg(test)]` の `mod tests` 内限定）。
    #[derive(Debug, Default, Clone, Copy)]
    struct NoPrefetch;

    impl prefetch::PrefetchPolicy for NoPrefetch {
        fn prefetch_neighbor<V: VisitedSet, S: NodeSource + ?Sized>(
            &self,
            _node: u32,
            _visited: &V,
            _v: &S,
            _d: usize,
        ) {
        }
    }

    /// Issue #490: 先読み要求されたノード id を記録する `PrefetchPolicy`
    /// （テスト専用）。「受理判定後にのみ先読みする」P0 契約を、記録内容が
    /// すべて `NodeMask` の受理ノードであることの直接検証で固定する。
    #[derive(Debug, Default)]
    struct RecordingPrefetch {
        seen: std::cell::RefCell<Vec<u32>>,
    }

    impl prefetch::PrefetchPolicy for RecordingPrefetch {
        fn prefetch_neighbor<V: VisitedSet, S: NodeSource + ?Sized>(
            &self,
            node: u32,
            _visited: &V,
            _v: &S,
            _d: usize,
        ) {
            self.seen.borrow_mut().push(node);
        }
    }

    #[test]
    fn vector_returns_none_out_of_range_and_some_in_range() {
        let dim = 4u32;
        let vectors = gen_corpus(1, dim as usize, 10);
        let index = HnswIndex::build(HnswParams::default(), dim, &vectors, 7).unwrap();
        assert!(index.vector(0).is_some());
        assert!(index.vector(9).is_some());
        assert_eq!(index.vector(10), None, "out-of-range node must be None");
        assert_eq!(
            index.vector(0).unwrap(),
            &vectors[0..dim as usize],
            "vector() must return the same bytes build() was given"
        );
    }

    #[test]
    fn approx_heap_bytes_at_least_covers_raw_vector_storage() {
        let dim = 8usize;
        let rows = 50usize;
        let vectors = gen_corpus(2, dim, rows);
        let index = HnswIndex::build(HnswParams::default(), dim as u32, &vectors, 11).unwrap();
        let raw_bytes = rows * dim * std::mem::size_of::<f32>();
        assert!(
            index.approx_heap_bytes() >= raw_bytes,
            "approx_heap_bytes must at least cover the raw vector copy"
        );
    }

    #[test]
    fn default_params_match_documented_values() {
        let p = HnswParams::default();
        assert_eq!(p.m, 16);
        assert_eq!(p.ef_construction, 100);
        assert_eq!(p.ef_search, 64);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn validate_rejects_out_of_range_params() {
        assert!(HnswParams {
            m: 1,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HnswParams {
            m: MAX_M + 1,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HnswParams {
            ef_construction: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HnswParams {
            ef_construction: MAX_EF + 1,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HnswParams {
            ef_search: 0,
            ..Default::default()
        }
        .validate()
        .is_err());
        assert!(HnswParams {
            ef_search: MAX_EF + 1,
            ..Default::default()
        }
        .validate()
        .is_err());
    }

    /// `full_scan_ratio` の既定値・不正比拒否（Issue #409。`sql::hnsw_cache::
    /// search_with_overlay` が可視カーディナリティ切替の判定に使う比率）。
    /// 分母 0・分子 > 分母は `HnswError::InvalidParams` で fail-closed に拒否される
    /// ことを固定する（`sql::hnsw_cache` 側の閾値前後テストは
    /// `tests/hnsw_cache.rs` を参照）。
    #[test]
    fn full_scan_ratio_defaults_and_rejects_invalid_ratios() {
        let v = ValidatedHnswParams::new(HnswParams::default()).expect("valid params");
        assert_eq!(
            v.full_scan_ratio(),
            Ratio {
                numerator: 1,
                denominator: 10
            },
            "full_scan_ratio must default to 1/10"
        );

        assert!(
            v.with_full_scan_ratio(Ratio {
                numerator: 0,
                denominator: 0,
            })
            .is_err(),
            "denominator == 0 must be rejected"
        );
        assert!(
            v.with_full_scan_ratio(Ratio {
                numerator: 11,
                denominator: 10,
            })
            .is_err(),
            "numerator > denominator must be rejected"
        );

        let replaced = v
            .with_full_scan_ratio(Ratio {
                numerator: 9,
                denominator: 10,
            })
            .expect("valid ratio must be accepted");
        assert_eq!(
            replaced.full_scan_ratio(),
            Ratio {
                numerator: 9,
                denominator: 10
            }
        );
    }

    /// `acorn_max_visible_ratio`（Issue #501）の既定値・検証規則（`den==0`・
    /// `num>den`・`ratio < full_scan_ratio` はいずれも拒否、境界一致
    /// `ratio == full_scan_ratio` は受理）を固定する。
    #[test]
    fn acorn_max_visible_ratio_defaults_none_and_rejects_invalid_ratios() {
        let v = ValidatedHnswParams::new(HnswParams::default()).expect("valid params");
        assert_eq!(
            v.acorn_max_visible_ratio(),
            None,
            "acorn_max_visible_ratio must default to None (existing behavior unchanged)"
        );

        assert!(
            v.with_acorn_max_visible_ratio(Ratio {
                numerator: 0,
                denominator: 0,
            })
            .is_err(),
            "denominator == 0 must be rejected"
        );
        assert!(
            v.with_acorn_max_visible_ratio(Ratio {
                numerator: 11,
                denominator: 10,
            })
            .is_err(),
            "numerator > denominator must be rejected"
        );
        assert!(
            v.with_acorn_max_visible_ratio(Ratio {
                numerator: 1,
                denominator: 20,
            })
            .is_err(),
            "acorn ratio below the default full_scan_ratio (1/10) must be rejected"
        );

        // 境界一致（acorn == full_scan_ratio）は受理する。
        let at_boundary = v
            .with_acorn_max_visible_ratio(Ratio {
                numerator: 1,
                denominator: 10,
            })
            .expect("acorn == full_scan_ratio must be accepted");
        assert_eq!(
            at_boundary.acorn_max_visible_ratio(),
            Some(Ratio {
                numerator: 1,
                denominator: 10
            })
        );

        let widened = v
            .with_acorn_max_visible_ratio(Ratio {
                numerator: 1,
                denominator: 2,
            })
            .expect("acorn ratio above full_scan_ratio must be accepted");

        // 後付けで full_scan_ratio を acorn_max_visible_ratio より大きくする
        // 変更は拒否する（逆転防止。Issue #501）。
        assert!(
            widened
                .with_full_scan_ratio(Ratio {
                    numerator: 6,
                    denominator: 10,
                })
                .is_err(),
            "raising full_scan_ratio above acorn_max_visible_ratio must be rejected"
        );
        // full_scan_ratio 側との境界一致は受理する。
        assert!(widened
            .with_full_scan_ratio(Ratio {
                numerator: 1,
                denominator: 2,
            })
            .is_ok());
    }

    /// `acorn_max_expansion_ratio`（Issue #681・親 #674）の既定値・検証規則を
    /// 固定する。`acorn_max_visible_ratio` と異なり `full_scan_ratio` との
    /// 順序制約は課さない（独立フィールド。§`ValidatedHnswParams` ドキュメント
    /// コメント参照）。
    #[test]
    fn acorn_max_expansion_ratio_defaults_none_and_rejects_invalid_ratios() {
        let v = ValidatedHnswParams::new(HnswParams::default()).expect("valid params");
        assert_eq!(
            v.acorn_max_expansion_ratio(),
            None,
            "acorn_max_expansion_ratio must default to None (existing behavior unchanged)"
        );

        assert!(
            v.with_acorn_max_expansion_ratio(Ratio {
                numerator: 0,
                denominator: 0,
            })
            .is_err(),
            "denominator == 0 must be rejected"
        );
        assert!(
            v.with_acorn_max_expansion_ratio(Ratio {
                numerator: 2,
                denominator: 1,
            })
            .is_err(),
            "numerator > denominator must be rejected"
        );

        // `numerator == 0` は受理する（「展開が 1 件でもあれば縮退」の意味）。
        let zero_ratio = v
            .with_acorn_max_expansion_ratio(Ratio {
                numerator: 0,
                denominator: 1,
            })
            .expect("numerator == 0 must be accepted");
        assert_eq!(
            zero_ratio.acorn_max_expansion_ratio(),
            Some(Ratio {
                numerator: 0,
                denominator: 1
            })
        );

        // `1/1` も受理する（構造的に発火不能な上限だが、値としては妥当）。
        assert!(v
            .with_acorn_max_expansion_ratio(Ratio {
                numerator: 1,
                denominator: 1,
            })
            .is_ok());

        // `acorn_max_visible_ratio` が未設定（`None`）のままでも受理する
        // （独立フィールド。`hop == TwoHop` レジームへ到達しない設定では
        // 単に観測されないだけで、拒否理由にはならない）。
        assert_eq!(v.acorn_max_visible_ratio(), None);
        assert!(v
            .with_acorn_max_expansion_ratio(Ratio {
                numerator: 1,
                denominator: 2,
            })
            .is_ok());
    }

    /// [`acorn_expansions_exceed`]（Issue #681）の境界値・fail-closed 方向を
    /// 固定する。`ratio = 1/1` は `bridge_expand` の停止性契約
    /// （各 2-hop ノードは高々 1 回しか visited を通らない＝`expansions <=
    /// visible` が構造的に成立）により常に発火不能であることも確認する。
    #[test]
    fn acorn_expansions_exceed_boundary_and_overflow_behavior() {
        let ratio_0_1 = Ratio {
            numerator: 0,
            denominator: 1,
        };
        let ratio_1_1 = Ratio {
            numerator: 1,
            denominator: 1,
        };
        let ratio_1_100 = Ratio {
            numerator: 1,
            denominator: 100,
        };

        // `0/1`: 展開が 1 件でもあれば発火。
        assert!(!acorn_expansions_exceed(0, 100, ratio_0_1));
        assert!(acorn_expansions_exceed(1, 100, ratio_0_1));

        // `1/1`: `expansions <= visible` が構造的に成立するため常に発火不能。
        assert!(!acorn_expansions_exceed(0, 100, ratio_1_1));
        assert!(!acorn_expansions_exceed(100, 100, ratio_1_1));

        // `1/100`: 境界（等しい）は非発火、超過は発火。
        assert!(!acorn_expansions_exceed(10, 1000, ratio_1_100));
        assert!(acorn_expansions_exceed(11, 1000, ratio_1_100));

        // `visible == 0` は防御的に `expansions > 0` で発火（呼び出し元の
        // 分断検査で通常到達しないが、ゼロ除算を避けつつ fail-closed を保つ）。
        assert!(!acorn_expansions_exceed(0, 0, ratio_1_1));
        assert!(acorn_expansions_exceed(1, 0, ratio_1_1));

        // オーバーフロー（`checked_mul` 失敗）は fail-closed に縮退側（`true`）
        // へ倒す。分母 2 との乗算で `u64::MAX` を超えさせる。
        assert!(acorn_expansions_exceed(
            u64::MAX,
            1,
            Ratio {
                numerator: 1,
                denominator: 2,
            }
        ));
        assert!(acorn_expansions_exceed(
            1,
            usize::MAX,
            Ratio {
                numerator: 2,
                denominator: 1,
            }
        ));
    }

    #[test]
    fn assign_level_is_deterministic_for_same_seed() {
        let m = 16;
        let mut a = DeterministicRng::new(42);
        let mut b = DeterministicRng::new(42);
        let levels_a: Vec<usize> = (0..1000).map(|_| assign_level(&mut a, m)).collect();
        let levels_b: Vec<usize> = (0..1000).map(|_| assign_level(&mut b, m)).collect();
        assert_eq!(levels_a, levels_b);
    }

    #[test]
    fn assign_level_distribution_is_roughly_geometric() {
        // P(level >= 1) はおおむね 1/m。厳密な統計検定ではなく、明らかな実装崩れ
        // （例: 常に 0 を返す・毎回 MAX_LEVEL に張り付く）を検知する緩い帯で確認する。
        let m = 16usize;
        let mut rng = DeterministicRng::new(7);
        let samples = 20_000;
        let at_least_one = (0..samples)
            .filter(|_| assign_level(&mut rng, m) >= 1)
            .count();
        let ratio = at_least_one as f64 / samples as f64;
        let expected = 1.0 / m as f64;
        assert!(
            ratio > expected * 0.5 && ratio < expected * 2.0,
            "ratio={ratio} expected~={expected}"
        );
    }

    #[test]
    fn select_neighbors_heuristic_prunes_redundant_close_candidates() {
        // 2 次元の単位ベクトルで手動検証可能な配置を作る（dot を cosine 類似度として
        // 扱う既定契約に合わせ、正規化済みベクトルで構成する）:
        // クエリ = 0°、候補 A = 10°（クエリに極めて近い）、候補 B = 12°（A に
        // さらに近く、A に対して冗長）、候補 C = -80°（クエリからは離れているが
        // A からも離れており、A を経由してクエリより近づける代替ルートにならない）。
        // m=2 で選ぶと、B は「A に対する方がクエリに対するより近い」ため枝刈りされ、
        // A・C が採用される（B は A の近傍探索で別途到達可能なため、A・B を両方
        // 直結するのは次数の無駄という Algorithm 4 の意図どおりの挙動）。
        let dim = 2usize;
        let a = [0.9848f32, 0.1736f32]; // 10°
        let b = [0.9781f32, 0.2079f32]; // 12°（A に極めて近い）
        let c = [0.1736f32, -0.9848f32]; // -80°（A からも離れている）
        let vectors: Vec<f32> = [a, b, c].concat();
        let params = HnswParams {
            m: 2,
            ..Default::default()
        };
        let index = index_from_nodes(
            params,
            dim as u32,
            vec![
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            Some(0),
            // 本テストは select_neighbors_heuristic を直接呼ぶのみで search() を
            // 経由しないため、`vectors` の内容は使われない（プレースホルダで
            // 十分）。
            Arc::from(vectors.clone()),
        );
        let query = &[1.0f32, 0.0f32];
        let candidates = vec![
            ScoredNode {
                node: 0,
                score: dot(&a, query),
            },
            ScoredNode {
                node: 1,
                score: dot(&b, query),
            },
            ScoredNode {
                node: 2,
                score: dot(&c, query),
            },
        ];
        let selected = index
            .select_neighbors_heuristic(&candidates, 2, dim, &vectors)
            .unwrap();
        assert!(selected.contains(&0));
        // B(1) は A(0) に対してクエリより近いため枝刈りされ、C(2) が代わりに採用される。
        assert!(selected.contains(&2));
        assert!(!selected.contains(&1));
    }

    #[test]
    fn build_rejects_dim_mismatch() {
        let err = HnswIndex::build(HnswParams::default(), 4, &[1.0, 2.0, 3.0], 1).unwrap_err();
        assert!(matches!(err, HnswError::DimMismatch { .. }));
    }

    #[test]
    fn build_rejects_zero_dim() {
        let err = HnswIndex::build(HnswParams::default(), 0, &[1.0, 2.0], 1).unwrap_err();
        assert!(matches!(err, HnswError::DimMismatch { .. }));
    }

    #[test]
    fn build_rejects_non_finite_vector() {
        let vectors = vec![1.0, f32::NAN, 0.0, 1.0];
        let err = HnswIndex::build(HnswParams::default(), 2, &vectors, 1).unwrap_err();
        assert!(matches!(err, HnswError::NonFiniteVector { .. }));
    }

    #[test]
    fn build_empty_input_yields_empty_index() {
        let index = HnswIndex::build(HnswParams::default(), 4, &[], 1).unwrap();
        assert_eq!(index.len(), 0);
        assert!(index.is_empty());
        assert_eq!(index.entry_point(), None);
        assert_eq!(index.max_level(), None);
    }

    #[test]
    fn search_layer_finds_true_nearest_neighbor_in_small_corpus() {
        let dim = 8;
        let rows = 300;
        let vectors = gen_corpus(0xABCD_1234, dim, rows);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 99).unwrap();

        // 総当たりで真の最近傍を求め、ef_construction 幅の search_layer 結果に
        // 含まれる率がおおむね高いことを検証する（結合テストの受け入れ条件は
        // tests/hnsw.rs 側で厳密に検証するため、ここでは内部関数の健全性のみ確認）。
        let mut hits = 0;
        let queries = 20;
        for q in 0..queries {
            let query = gen_corpus(0x1111_0000 + q as u64, dim, 1);
            let mut brute: Vec<ScoredNode> = (0..rows as u32)
                .map(|n| ScoredNode {
                    node: n,
                    score: dot(node_vector(&vectors, dim, n).unwrap(), &query),
                })
                .collect();
            brute.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
            let true_nearest = brute[0].node;

            let ep = index.entry_point().unwrap();
            let mut visited = VisitedScratch::default();
            let found = index
                .search_layer(
                    vec![ep],
                    &query,
                    params.ef_construction,
                    0,
                    dim,
                    &index.vectors,
                    &mut visited,
                    None,
                )
                .unwrap();
            if found.iter().any(|c| c.node == true_nearest) {
                hits += 1;
            }
        }
        assert!(hits as f64 / queries as f64 >= 0.9, "hits={hits}/{queries}");
    }

    /// `search_layer` の停止・受理判定が `ScoredNode` の複合順序（id 昇順
    /// タイブレーク込み）ではなく**スコアのみ**で行われることを直接検証する
    /// （`docs/design/hnsw-graph-construction.md`「`search_layer` の停止・
    /// 受理判定: 順序規約の使い分け」節参照）。手作りの 3 ノードグラフ
    /// （`entry(id0) -> node1(id1, entry と厳密同点スコア) -> node2(id2, 全体
    /// 最良スコア)`。node0 と node2 の間には直接リンクを張らない）に対し、
    /// `ef=1` で `search_layer` を呼ぶと、複合順序で判定した場合は
    /// entry と node1 の同点比較で `node1.cmp(entry) == Less`（id が大きい方が
    /// 複合順序では「より遠い」）となるため、受理判定（`worst_ok`）が node1 を
    /// 拒否し、node1 経由でしか到達できない node2 を発見できない。スコアのみの
    /// 判定であれば同点は拒否されず node1 の隣接探索まで進み、最終的に
    /// より良いスコアの node2 が結果に残る。
    #[test]
    fn search_layer_continues_through_tied_score_candidates_to_find_a_strictly_closer_node() {
        let dim = 1usize;
        // dim=1 の `dot(v, q) = v[0] * q[0]` なので `q=[1.0]` のときスコアは
        // ノード値そのものになる。
        let vectors: Vec<f32> = vec![10.0, 10.0, 20.0];
        let index = index_from_nodes(
            HnswParams::default(),
            dim as u32,
            vec![
                Node {
                    level: 0,
                    links: vec![vec![1]],
                },
                Node {
                    level: 0,
                    links: vec![vec![2]],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            Some(0),
            // 本テストは search_layer を直接呼ぶのみで search() を経由しない
            // ため、`vectors` フィールドの内容は使われない（プレースホルダで
            // 十分）。
            Arc::from(vectors.clone()),
        );
        let query = [1.0f32];
        let mut visited = VisitedScratch::default();
        let results = index
            .search_layer(
                vec![0],
                &query,
                1,
                0,
                dim,
                &index.vectors,
                &mut visited,
                None,
            )
            .expect("search_layer should succeed");
        assert_eq!(
            results.iter().map(|s| s.node).collect::<Vec<_>>(),
            vec![2],
            "search_layer must traverse through a tied-score node to reach a \
             strictly closer one; an id-tiebreak (complex-order) stopping or \
             admission predicate would stop at the tie and miss node 2"
        );
    }

    /// Issue #431（codex-review P0 是正）の回帰テスト: 非受理（stale・不可視）
    /// ノードが探索経路の中継点として使われず、そのノード経由でしか到達
    /// できない受理ノードは結果に現れないことを確認する。
    ///
    /// `node0 -> node1 -> node2` の一本道（`node0`・`node2` 間に直接リンクは
    /// 張らない）を作り、`node1` を非受理としてマスクする。マスクなし
    /// （`None`）では `node1` を中継して最良スコアの `node2` を発見できるが、
    /// マスクありでは `node1` が候補ヒープへ一切積まれなくなったため
    /// （§`search_layer` ドキュメンテーションコメント参照）`node2` へは
    /// 到達できず、`node0` 単体の部分結果を返す。
    ///
    /// このマスクは「`node0` と `node2` の 2 つの連結成分に分断されている」
    /// 状態でもあり、`node0` だけを完全な top-k として返すと `node0` より
    /// 真に近い `node2`（マスク受理済み）を一切探索しないまま結果を確定して
    /// しまう recall バグになる——その分断検出は本関数の責務ではなく、
    /// 呼び出し元（`sql::hnsw_cache::Overlay::compute`）が世代毎に 1 回
    /// [`HnswIndex::is_mask_fully_reachable`] を呼んで検出し、`search_masked`
    /// 自体を呼ばず plain scan へ縮退する契約になっている（クエリ毎の全域
    /// BFS を避けるため。Cursor Bugbot High 指摘・PR #435）。同じグラフで
    /// `is_mask_fully_reachable` が分断を検出することは
    /// `is_mask_fully_reachable_detects_unreachable_component_even_when_
    /// reachable_component_satisfies_k` で固定する。
    #[test]
    fn search_masked_does_not_traverse_through_a_rejected_bridge_node() {
        let dim = 1usize;
        // dim=1 の `dot(v, q) = v[0] * q[0]` なので `q=[1.0]` のときスコアは
        // ノード値そのもの。node2 が最良スコアだが node1 経由でしか到達できない。
        let vectors: Vec<f32> = vec![10.0, 15.0, 20.0];
        let index = index_from_nodes(
            HnswParams::default(),
            dim as u32,
            vec![
                Node {
                    level: 0,
                    links: vec![vec![1]],
                },
                Node {
                    level: 0,
                    links: vec![vec![2]],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            Some(0),
            Arc::from(vectors),
        );
        let query = [1.0f32];
        let mut scratch = HnswSearchScratch::default();

        let unmasked = index
            .search_masked(&query, 3, 10, None, &mut scratch)
            .expect("unmasked search should succeed");
        assert_eq!(
            unmasked.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![2, 1, 0],
            "unmasked search must traverse node0 -> node1 -> node2 and rank by score"
        );

        let mut mask = NodeMask::new(index.len());
        mask.set(0);
        // node1（橋渡しノード）は非受理のまま。
        mask.set(2);
        let masked = index
            .search_masked(&query, 3, 10, Some(&mask), &mut scratch)
            .expect("masked search should succeed");
        assert_eq!(
            masked.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![0],
            "masked search must not traverse through the rejected bridge node1, so \
             node2 (mask-accepted, unreachable without node1) must not appear; the \
             graph-split detection that prevents this partial result from being \
             treated as a complete top-k is the caller's responsibility (§this \
             function's doc comment), not search_masked's"
        );

        // 同じグラフで分断検出（呼び出し元の責務）が正しく機能することを固定する。
        assert!(
            !index.is_mask_fully_reachable(&mask),
            "node2 is mask-accepted but unreachable from node0 without traversing \
             the rejected bridge node1, so the mask must be reported as split"
        );
    }

    /// ACORN-1 の 2-hop 展開（Issue #501・`HopMode::TwoHop`）は、上の
    /// `search_masked_does_not_traverse_through_a_rejected_bridge_node` と
    /// 同じグラフ形状で node1（非受理）を橋渡し役として使い、node2 へ到達
    /// できることを固定する。`is_mask_fully_reachable_with` も同じ規則を
    /// 共有するため到達可能と判定する（BFS と探索の同期。§`bridge_expand`
    /// ドキュメンテーションコメント参照）。
    #[test]
    fn search_masked_two_hop_traverses_through_a_rejected_bridge_node() {
        let dim = 1usize;
        let vectors: Vec<f32> = vec![10.0, 15.0, 20.0];
        let index = index_from_nodes(
            HnswParams::default(),
            dim as u32,
            vec![
                Node {
                    level: 0,
                    links: vec![vec![1]],
                },
                Node {
                    level: 0,
                    links: vec![vec![2]],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            Some(0),
            Arc::from(vectors),
        );
        let query = [1.0f32];
        let mut scratch = HnswSearchScratch::default();

        let mut mask = NodeMask::new(index.len());
        mask.set(0);
        // node1（橋渡しノード）は非受理のまま。
        mask.set(2);

        assert!(
            index.is_mask_fully_reachable_with(&mask, HopMode::TwoHop),
            "TwoHop 展開により node1 を橋渡しとして node2 へ到達できるはず"
        );

        let masked = index
            .search_masked_with_hop(
                &query,
                3,
                10,
                Some(&mask),
                DEFAULT_SPARSE_VISITED_MAX,
                HopMode::TwoHop,
                &mut scratch,
            )
            .expect("two-hop masked search should succeed");
        assert_eq!(
            masked.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![2, 0],
            "TwoHop 展開は node1（非受理）を中継点として node2 を候補化し、\
             node2（score 20）が node0（score 10）より上位に来るはず"
        );
        assert_eq!(
            scratch.last_acorn_expansions(),
            1,
            "node1 経由で受理・候補化した 2-hop ノードは node2 の 1 件のみ"
        );

        // `HopMode::OneHop` は既存契約のまま変わらない（ビット同一）。
        let masked_one_hop = index
            .search_masked_with_hop(
                &query,
                3,
                10,
                Some(&mask),
                DEFAULT_SPARSE_VISITED_MAX,
                HopMode::OneHop,
                &mut scratch,
            )
            .expect("one-hop masked search should succeed");
        assert_eq!(
            masked_one_hop.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![0]
        );
        assert_eq!(scratch.last_acorn_expansions(), 0);
    }

    /// `TwoHop` は 1 段のみ展開する（3-hop 先へは辿らない）ことを、
    /// `0 -> 1(非受理) -> 2(非受理) -> 3(受理)` の鎖状グラフで固定する。
    /// node3 は node1 からは 2-hop（node2 経由）先だが、node2 自身が非受理
    /// のため `bridge_expand` は node2 を候補化せず、node3 はどちらの
    /// 経路（探索・BFS）からも到達不能のまま。
    #[test]
    fn search_masked_two_hop_does_not_traverse_three_hops() {
        let dim = 1usize;
        let vectors: Vec<f32> = vec![10.0, 11.0, 12.0, 20.0];
        let index = index_from_nodes(
            HnswParams::default(),
            dim as u32,
            vec![
                Node {
                    level: 0,
                    links: vec![vec![1]],
                },
                Node {
                    level: 0,
                    links: vec![vec![2]],
                },
                Node {
                    level: 0,
                    links: vec![vec![3]],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            Some(0),
            Arc::from(vectors),
        );
        let query = [1.0f32];
        let mut scratch = HnswSearchScratch::default();

        let mut mask = NodeMask::new(index.len());
        mask.set(0);
        // node1・node2 はいずれも非受理のまま。
        mask.set(3);

        assert!(
            !index.is_mask_fully_reachable_with(&mask, HopMode::TwoHop),
            "node3 is 3 hops away through two rejected bridges; TwoHop must not \
             report it reachable (expansion is a single hop only)"
        );

        let masked = index
            .search_masked_with_hop(
                &query,
                4,
                10,
                Some(&mask),
                DEFAULT_SPARSE_VISITED_MAX,
                HopMode::TwoHop,
                &mut scratch,
            )
            .expect("two-hop masked search should succeed");
        assert_eq!(
            masked.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![0],
            "node3 must not appear: bridge_expand only inspects node1's own \
             neighbors (node2, itself rejected) and never recurses further"
        );
    }

    /// [`crate::hnsw::NodeSource`] への `score` 呼び出しを記録するテスト専用
    /// ラッパー（I1・P0 不変の機械検証用。Issue #501）。非受理ノードのベクトル
    /// へは一切アクセスしないという契約を、`TwoHop` 探索中に記録された
    /// score 呼び出し先ノード集合が受理ノードのみであることで直接検証する。
    struct RecordingNodeSource<'a> {
        inner: &'a NodeVectors,
        scored: std::cell::RefCell<Vec<u32>>,
    }

    impl NodeSource for RecordingNodeSource<'_> {
        fn score(&self, dim: usize, node: u32, query: &[f32]) -> Result<f32, HnswError> {
            self.scored.borrow_mut().push(node);
            self.inner.score(dim, node, query)
        }
        fn touch_prefetch(&self, dim: usize, node: u32) {
            self.inner.touch_prefetch(dim, node);
        }
    }

    /// I1（ベクトル非参照・P0・不変）の機械検証: `TwoHop` 探索中に `score` が
    /// 呼ばれるのは受理ノードのみであり、非受理ノード（橋渡し役の node1・
    /// node3）へは一度も呼ばれないことを固定する。非 vacuous
    /// （受理 2-hop ノードへの呼び出しは 1 件以上ある）ことも確認する。
    #[test]
    fn search_masked_two_hop_never_scores_rejected_nodes() {
        let dim = 1usize;
        // node0(受理) -> node1(非受理,橋) -> node2(受理,2-hop)
        //             \-> node3(非受理,橋) -> node4(非受理,2-hop 先も非受理)
        let vectors: Vec<f32> = vec![10.0, 11.0, 20.0, 12.0, 13.0];
        let index = index_from_nodes(
            HnswParams::default(),
            dim as u32,
            vec![
                Node {
                    level: 0,
                    links: vec![vec![1, 3]],
                },
                Node {
                    level: 0,
                    links: vec![vec![2]],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
                Node {
                    level: 0,
                    links: vec![vec![4]],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            Some(0),
            Arc::from(vectors),
        );
        let query = [1.0f32];

        let mut mask = NodeMask::new(index.len());
        mask.set(0);
        mask.set(2);

        let source = RecordingNodeSource {
            inner: &index.vectors,
            scored: std::cell::RefCell::new(Vec::new()),
        };
        let mut visited = VisitedBitmap::default();
        let mut expansions = 0u64;
        let result = search_layer_in(
            &index.graph,
            vec![0],
            &query,
            10,
            0,
            dim,
            &source,
            &mut visited,
            Some(&mask),
            &prefetch::PipelinePrefetch,
            HopMode::TwoHop,
            &mut expansions,
        )
        .expect("two-hop search_layer_in should succeed");

        assert_eq!(
            result.iter().map(|s| s.node).collect::<Vec<_>>(),
            vec![2, 0],
            "node2 (accepted 2-hop via rejected bridge node1) must be found"
        );
        let scored = source.scored.into_inner();
        assert!(
            scored.contains(&0) && scored.contains(&2),
            "score must be called for accepted nodes (non-vacuous): {scored:?}"
        );
        assert!(
            !scored.contains(&1) && !scored.contains(&3) && !scored.contains(&4),
            "score must never be called for rejected nodes (I1 invariant): {scored:?}"
        );
        assert_eq!(
            expansions, 1,
            "only node2 is accepted among the 2-hop candidates"
        );
    }

    /// Issue #680: 上位層の貪欲降下（[`HnswIndex::greedy_descend_masked`]）が
    /// `hop == HopMode::TwoHop` のときのみ、非受理隣接（橋渡しノード）越しに
    /// 2-hop 先の受理ノードへ移動できることを固定する。`OneHop` はこれまで
    /// どおり非受理隣接を無視して起点に留まる（ビット同一契約）。
    ///
    /// グラフ形状: node0（level1・受理・起点）--level1--> node1（level1・
    /// **非受理**・橋渡し）--level1--> node2（level1・受理・遠方だがスコアが
    /// 高い）。`greedy_descend_masked` はレベル 1 の降下 1 回分のみを検査する
    /// （呼び出し元 `search_masked_with_hop`／`search_masked_resumable_start`
    /// が層ごとに繰り返し呼ぶ設計そのものは無変更）。
    #[test]
    fn greedy_descend_masked_two_hop_bridges_to_a_better_candidate_via_a_rejected_neighbor() {
        let dim = 1usize;
        // dim=1 の `dot(v, q) = v[0] * q[0]`・`q=[1.0]` なのでスコアは値そのもの。
        // node1（橋渡し）の値は非受理のため一切参照されない前提でわざと
        // 「スコアだけ見れば最良」の値を入れ、I1（非受理ノードのベクトル非
        // 参照）を壊していれば誤って選ばれてしまう構図にする。
        let vectors: Vec<f32> = vec![5.0, 999.0, 50.0];
        let index = index_from_nodes(
            HnswParams::default(),
            dim as u32,
            vec![
                Node {
                    level: 1,
                    links: vec![Vec::new(), vec![1]],
                },
                Node {
                    level: 1,
                    links: vec![Vec::new(), vec![2]],
                },
                Node {
                    level: 1,
                    links: vec![Vec::new(), Vec::new()],
                },
            ],
            Some(0),
            Arc::from(vectors),
        );
        let query = [1.0f32];

        let mut mask = NodeMask::new(index.len());
        mask.set(0);
        mask.set(2);
        // node1（橋渡し）は非受理のまま。

        let source: &dyn NodeSource = &index.vectors;

        let mut bridges_one_hop = 0u64;
        let result_one_hop = index
            .greedy_descend_masked(
                0,
                &query,
                1,
                dim,
                source,
                Some(&mask),
                HopMode::OneHop,
                &mut bridges_one_hop,
            )
            .expect("one-hop descend should succeed");
        assert_eq!(
            result_one_hop,
            Some(0),
            "OneHop must stay at node0: node1 is rejected and must not be used as a bridge"
        );
        assert_eq!(
            bridges_one_hop, 0,
            "OneHop must never count descent bridges"
        );

        let mut bridges_two_hop = 0u64;
        let result_two_hop = index
            .greedy_descend_masked(
                0,
                &query,
                1,
                dim,
                source,
                Some(&mask),
                HopMode::TwoHop,
                &mut bridges_two_hop,
            )
            .expect("two-hop descend should succeed");
        assert_eq!(
            result_two_hop,
            Some(2),
            "TwoHop must bridge through the rejected node1 to reach node2              (score 50 > node0's score 5)"
        );
        assert_eq!(
            bridges_two_hop, 1,
            "exactly one 2-hop node (node2) was accepted and scored via bridging"
        );
    }

    /// I1（ベクトル非参照・P0・不変）の機械検証（降下版）: `greedy_descend_masked`
    /// の `TwoHop` ブリッジ降下が `score` を呼ぶのは受理ノード（起点・2-hop
    /// 先の受理ノード）のみであり、橋渡し役の非受理ノードへは一度も呼ばれない
    /// ことを固定する。非 vacuous（受理 2-hop ノードへの呼び出しは 1 件以上）
    /// も確認する。
    #[test]
    fn greedy_descend_masked_two_hop_never_scores_the_bridge_node() {
        let dim = 1usize;
        let vectors: Vec<f32> = vec![5.0, 999.0, 50.0];
        let index = index_from_nodes(
            HnswParams::default(),
            dim as u32,
            vec![
                Node {
                    level: 1,
                    links: vec![Vec::new(), vec![1]],
                },
                Node {
                    level: 1,
                    links: vec![Vec::new(), vec![2]],
                },
                Node {
                    level: 1,
                    links: vec![Vec::new(), Vec::new()],
                },
            ],
            Some(0),
            Arc::from(vectors),
        );
        let query = [1.0f32];

        let mut mask = NodeMask::new(index.len());
        mask.set(0);
        mask.set(2);

        let source = RecordingNodeSource {
            inner: &index.vectors,
            scored: std::cell::RefCell::new(Vec::new()),
        };
        let mut bridges = 0u64;
        let result = index
            .greedy_descend_masked(
                0,
                &query,
                1,
                dim,
                &source,
                Some(&mask),
                HopMode::TwoHop,
                &mut bridges,
            )
            .expect("two-hop descend should succeed");
        assert_eq!(result, Some(2));
        let scored = source.scored.into_inner();
        assert!(
            scored.contains(&0) && scored.contains(&2),
            "score must be called for accepted nodes (non-vacuous): {scored:?}"
        );
        assert!(
            !scored.contains(&1),
            "score must never be called for the rejected bridge node1 (I1 invariant): {scored:?}"
        );
    }

    /// `two_hop_effective_ef`（Issue #680）: 純粋関数の境界値・クランプ規則を
    /// 固定する。
    #[test]
    fn two_hop_effective_ef_clamps_scale_and_upper_bound() {
        // 基本形: `visible` が `node_count` の 1/4 なら scale=4。
        assert_eq!(two_hop_effective_ef(64, 10, 1000, 250), 64 * 4);
        // `ef < k` は `ef.max(k)` を先に適用してから倍率をかける。
        assert_eq!(two_hop_effective_ef(4, 64, 1000, 250), 64 * 4);
        // `visible == 0`（呼び出し元の到達可能性検査で通常到達しないが、
        // 防御的にゼロ除算を避け `base` をそのまま返す）。
        assert_eq!(two_hop_effective_ef(64, 10, 1000, 0), 64);
        // scale は `ACORN_EF_SCALE_MAX` でクランプされる（可視比率が既定域
        // より極端に低い場合の安全弁）。
        let scaled = two_hop_effective_ef(64, 10, 1_000_000, 1);
        assert_eq!(scaled, 64 * ACORN_EF_SCALE_MAX);
        // 最終結果は `MAX_EF` でもクランプされる（二重の安全弁）。
        let capped = two_hop_effective_ef(MAX_EF, 10, 1_000_000, 1);
        assert_eq!(capped, MAX_EF);
        // オーバーフロー耐性: 巨大な `node_count`／小さい `visible` でも
        // panic せず `saturating_mul`／`div_ceil` で有限値に収まる。
        let huge = two_hop_effective_ef(MAX_EF, MAX_EF, usize::MAX, 1);
        assert_eq!(huge, MAX_EF);
    }

    /// `accept == None` のとき `HopMode::TwoHop` は `HopMode::OneHop`・
    /// [`HnswIndex::search`] とビット同一の結果を返す（`is_accepted` が常に
    /// `true` を返すため非受理分岐そのものへ到達しない。既存動作を不変に
    /// 保つ契約の一部）。
    #[test]
    fn search_masked_two_hop_matches_search_when_mask_is_none() {
        let dim = 4usize;
        let vectors = gen_corpus(0xACC0_1234, dim, 64);
        let index = HnswIndex::build(
            HnswParams::default().with_m(8).with_ef_construction(32),
            dim as u32,
            &vectors,
            7,
        )
        .expect("build should succeed");
        let query = gen_corpus(0xACC0_5678, dim, 1);
        let mut scratch = HnswSearchScratch::default();

        let expected = index
            .search(&query, 10, 32, &mut scratch)
            .expect("search should succeed");
        let two_hop = index
            .search_masked_with_hop(
                &query,
                10,
                32,
                None,
                DEFAULT_SPARSE_VISITED_MAX,
                HopMode::TwoHop,
                &mut scratch,
            )
            .expect("two-hop search_masked_with_hop(mask=None) should succeed");
        assert_eq!(
            two_hop
                .iter()
                .map(|h| (h.id, h.score.to_bits()))
                .collect::<Vec<_>>(),
            expected
                .iter()
                .map(|h| (h.id, h.score.to_bits()))
                .collect::<Vec<_>>(),
            "mask=None must be bit-identical between HopMode::TwoHop and search()"
        );
        assert_eq!(scratch.last_acorn_expansions(), 0);
    }

    /// `TwoHop` 探索は決定的（同一索引・同一クエリ・同一マスクで新規スクラッチ
    /// を使っても完全一致する）ことを固定する。
    #[test]
    fn search_masked_two_hop_is_deterministic() {
        let dim = 4usize;
        let vectors = gen_corpus(0x7EE7_0001, dim, 96);
        let index = HnswIndex::build(
            HnswParams::default().with_m(8).with_ef_construction(32),
            dim as u32,
            &vectors,
            11,
        )
        .expect("build should succeed");
        let query = gen_corpus(0x7EE7_0002, dim, 1);

        // 偶数 id のみ受理（隣接構造次第で非受理ノードを跨ぐ橋渡しが起こる）。
        let mut mask = NodeMask::new(index.len());
        for id in 0..index.len() {
            if id % 2 == 0 {
                mask.set(id as u32);
            }
        }

        let mut scratch_a = HnswSearchScratch::default();
        let run1 = index
            .search_masked_with_hop(
                &query,
                10,
                32,
                Some(&mask),
                DEFAULT_SPARSE_VISITED_MAX,
                HopMode::TwoHop,
                &mut scratch_a,
            )
            .expect("run1 should succeed");

        let mut scratch_b = HnswSearchScratch::default();
        let run2 = index
            .search_masked_with_hop(
                &query,
                10,
                32,
                Some(&mask),
                DEFAULT_SPARSE_VISITED_MAX,
                HopMode::TwoHop,
                &mut scratch_b,
            )
            .expect("run2 should succeed");

        assert_eq!(
            run1.iter()
                .map(|h| (h.id, h.score.to_bits()))
                .collect::<Vec<_>>(),
            run2.iter()
                .map(|h| (h.id, h.score.to_bits()))
                .collect::<Vec<_>>(),
            "TwoHop search must be deterministic across independent scratches"
        );
        assert_eq!(
            scratch_a.last_acorn_expansions(),
            scratch_b.last_acorn_expansions()
        );
    }

    /// 停止性（R2・DoS ガード）: `bridge_expand` が呼ばれる回数（＝非受理
    /// ノードの隣接リストを読む回数）は、クエリ全体で「1-hop 非受理として
    /// 初めて visited されたノード数」以下に構造的に有界であることを、
    /// [`Adjacency::neighbors`] 呼び出し回数を記録するラッパーで固定する
    /// （§`bridge_expand` ドキュメンテーションコメント「停止性」参照）。
    struct CountingAdjacency<'a, A: Adjacency> {
        inner: &'a A,
        calls: std::cell::RefCell<std::collections::HashMap<u32, u32>>,
    }

    impl<A: Adjacency> Adjacency for CountingAdjacency<'_, A> {
        fn level_of(&self, node: u32) -> Option<usize> {
            self.inner.level_of(node)
        }
        fn neighbors(&self, level: usize, node: u32) -> Option<&[u32]> {
            *self.calls.borrow_mut().entry(node).or_insert(0) += 1;
            self.inner.neighbors(level, node)
        }
        fn node_count(&self) -> usize {
            self.inner.node_count()
        }
    }

    #[test]
    fn search_masked_two_hop_reads_each_node_adjacency_at_most_once() {
        let dim = 4usize;
        let vectors = gen_corpus(0xB0DE_0001, dim, 200);
        let index = HnswIndex::build(
            HnswParams::default().with_m(8).with_ef_construction(48),
            dim as u32,
            &vectors,
            13,
        )
        .expect("build should succeed");
        let query = gen_corpus(0xB0DE_0002, dim, 1);

        // 3 個に 1 個だけ受理する疎なマスク（非受理ノードを跨ぐ橋渡しを
        // 誘発しやすくする）。
        let mut mask = NodeMask::new(index.len());
        for id in 0..index.len() {
            if id % 3 == 0 {
                mask.set(id as u32);
            }
        }

        let counting = CountingAdjacency {
            inner: &index.graph,
            calls: std::cell::RefCell::new(std::collections::HashMap::new()),
        };
        let mut visited = VisitedBitmap::default();
        let mut expansions = 0u64;
        let _ = search_layer_in(
            &counting,
            vec![0],
            &query,
            32,
            0,
            dim,
            &index.vectors,
            &mut visited,
            Some(&mask),
            &prefetch::PipelinePrefetch,
            HopMode::TwoHop,
            &mut expansions,
        )
        .expect("two-hop search_layer_in should succeed");

        let calls = counting.calls.into_inner();
        let max_reads = calls.values().copied().max().unwrap_or(0);
        assert!(
            max_reads <= 1,
            "each node's adjacency list must be read at most once per query \
             (bridge_expand marks the bridge node visited before expanding it), \
             but observed {max_reads} reads for some node: {calls:?}"
        );
    }

    #[test]
    fn visited_bitmap_reset_clears_all_bits_and_only_grows() {
        let mut bm = VisitedBitmap::default();
        bm.reset(10);
        assert_eq!(bm.mark_visited(3), Some(false));
        assert_eq!(bm.mark_visited(3), Some(true));
        // 伸長のみで縮めない: 一旦 200 まで広げてから 5 へ縮めても、以前確保した
        // 語も次の reset で全クリアされる（縮小しないことの安全側確認）。
        bm.reset(200);
        assert_eq!(bm.mark_visited(150), Some(false));
        bm.reset(5);
        assert_eq!(
            bm.mark_visited(150),
            Some(false),
            "reset は全クリアなので縮小後の呼び出しでも既訪問と誤判定してはならない"
        );
    }

    #[test]
    fn visited_bitmap_out_of_range_id_returns_none() {
        let mut bm = VisitedBitmap::default();
        bm.reset(10);
        assert_eq!(bm.mark_visited(999), None);
    }

    /// [`VisitedSparse`]（Issue #497）が [`VisitedBitmap`] と同じ契約
    /// （範囲外は `None`・二重マークは `Some(true)`・新規は `Some(false)`・
    /// `reset` 後は容量を保持したまま全訪問済みマークが消える）を満たすことを
    /// 固定する。
    #[test]
    fn visited_sparse_matches_visited_bitmap_contract() {
        let mut vs = VisitedSparse::default();
        vs.reset(10);
        assert_eq!(vs.mark_visited(3), Some(false));
        assert_eq!(vs.mark_visited(3), Some(true));
        // 範囲外は None（VisitedBitmap と同じ fail-closed 契約）。
        assert_eq!(vs.mark_visited(999), None);

        // reset は全クリア: 別の len へ伸長したあとに縮めても、以前の
        // マークが誤って「既訪問」判定を汚染してはならない。
        vs.reset(200);
        assert_eq!(vs.mark_visited(150), Some(false));
        vs.reset(5);
        assert_eq!(
            vs.mark_visited(150),
            None,
            "reset(5) 後は 150 が範囲外になるため None（VisitedBitmap は語配列を \
             伸長のみで縮めないため既訪問扱いになるのに対し、HashSet は clear \
             するため『範囲外』の判定が先に効く——いずれも fail-closed で\
             「訪問済みと誤判定して探索を打ち切らない」側に倒れる点は同じ）"
        );
        // clear 後も内部確保容量は保持する契約（`HashSet::clear` の挙動）。
        // 観測可能な副作用は無いため、reset 後に通常どおり動作することのみ
        // 固定する。
        vs.reset(10);
        assert_eq!(vs.mark_visited(3), Some(false));
    }

    /// [`NodeMask::count_ones`]（Issue #497 で O(1) 化）が語走査での再計算と
    /// 一致すること・同一ビットの二重 `set` で増えないこと・範囲外 `set` は
    /// 無視されることを固定する。
    #[test]
    fn node_mask_count_ones_matches_word_scan_and_is_idempotent() {
        let mut mask = NodeMask::new(130);
        assert_eq!(mask.count_ones(), 0);

        mask.set(0);
        mask.set(63);
        mask.set(64);
        mask.set(129);
        assert_eq!(mask.count_ones(), 4);

        // 同一ビットの二重 set は増えない。
        mask.set(0);
        mask.set(129);
        assert_eq!(mask.count_ones(), 4);

        // 範囲外 set は無視される（fail-closed）。
        mask.set(130);
        mask.set(u32::MAX);
        assert_eq!(mask.count_ones(), 4);

        // 語走査での再計算と一致する（回帰保険。`count_ones` の実装が
        // `self.ones` を返さず語走査に戻っても検知できるよう、期待値は
        // ビット位置から独立に導出する）。
        let expected: usize = [0u32, 63, 64, 129]
            .iter()
            .filter(|&&node| mask.get(node))
            .count();
        assert_eq!(mask.count_ones(), expected);
    }

    /// 手作りの最小グラフ（`search_layer_continues_through_tied_score_candidates_
    /// to_find_a_strictly_closer_node` と同じ 3 ノード構成）で、上位層の貪欲降下
    /// →層 0 のビーム探索という `search` の経路が正しく動作することを確認する。
    #[test]
    fn search_finds_expected_top_k_on_minimal_graph() {
        let dim = 1usize;
        let vectors: Vec<f32> = vec![10.0, 10.0, 20.0];
        let index = index_from_nodes(
            HnswParams::default(),
            dim as u32,
            vec![
                Node {
                    level: 0,
                    links: vec![vec![1]],
                },
                Node {
                    level: 0,
                    links: vec![vec![2]],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            Some(0),
            // search() は `self.vectors`（build 時の不変スナップショット）を
            // 参照するため、struct literal でも同じ内容を設定する。
            Arc::from(vectors.clone()),
        );
        let query = [1.0f32];
        let mut scratch = HnswSearchScratch::default();
        let results = index.search(&query, 2, 1, &mut scratch).unwrap();
        assert_eq!(
            results.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![2, 0],
            "search must beam through the tied-score node to reach node 2, then \
             fall back to node 0 (score 10.0) as the 2nd best"
        );
    }

    /// Issue #490: 受理判定後 prefetch の有無で `search_layer` の結果が
    /// ビット同一であることを、複数フィクスチャ・複数 `ef` で機械検証する
    /// （§`search_layer_with` ドキュメンテーションコメント参照）。
    #[test]
    fn search_layer_prefetch_is_bit_identical_to_no_prefetch() {
        let dim = 8usize;
        // クラスタ構造ありコーパス・重複ヘビーコーパス（同点誘発）・小 dim の
        // 3 フィクスチャ。
        let cluster = gen_corpus(41, dim, 300);
        let mut duplicate_heavy = gen_corpus(43, dim, 20);
        duplicate_heavy = duplicate_heavy
            .iter()
            .cycle()
            .take(300 * dim)
            .copied()
            .collect();
        let small_dim = gen_corpus(45, 2, 300);
        let fixtures: [(&str, usize, &[f32]); 3] = [
            ("cluster", dim, &cluster),
            ("duplicate_heavy", dim, &duplicate_heavy),
            ("small_dim", 2, &small_dim),
        ];
        for (name, fixture_dim, vectors) in fixtures {
            let params = HnswParams {
                m: 8,
                ef_construction: 40,
                ef_search: 20,
            };
            let index = HnswIndex::build(params, fixture_dim as u32, vectors, 7)
                .unwrap_or_else(|e| panic!("build failed for fixture {name}: {e:?}"));
            let entry = index.entry_point().expect("non-empty index has an entry");
            let query = gen_corpus(99, fixture_dim, 1);
            for &ef in &[1usize, 10, 40] {
                let mut visited_no = VisitedScratch::default();
                let via_no = index
                    .search_layer_with(
                        vec![entry],
                        &query,
                        ef,
                        0,
                        fixture_dim,
                        &index.vectors,
                        &mut visited_no,
                        None,
                        &NoPrefetch,
                    )
                    .unwrap_or_else(|e| {
                        panic!("search_layer_with(NoPrefetch) failed for {name}/ef={ef}: {e:?}")
                    });
                let mut visited_pipeline = VisitedScratch::default();
                let via_pipeline = index
                    .search_layer_with(
                        vec![entry],
                        &query,
                        ef,
                        0,
                        fixture_dim,
                        &index.vectors,
                        &mut visited_pipeline,
                        None,
                        &prefetch::PipelinePrefetch,
                    )
                    .unwrap_or_else(|e| {
                        panic!(
                            "search_layer_with(PipelinePrefetch) failed for {name}/ef={ef}: {e:?}"
                        )
                    });
                assert_eq!(
                    via_no.len(),
                    via_pipeline.len(),
                    "fixture {name}/ef={ef}: result length must match"
                );
                for (a, b) in via_no.iter().zip(via_pipeline.iter()) {
                    assert_eq!(
                        a.node, b.node,
                        "fixture {name}/ef={ef}: node order must match"
                    );
                    assert_eq!(
                        a.score.to_bits(),
                        b.score.to_bits(),
                        "fixture {name}/ef={ef}: score must be bit-identical with/without prefetch"
                    );
                }
            }
        }
    }

    /// Issue #490: `NodeMask` によるフィルタあり探索（`Subset` 形状）でも
    /// prefetch の有無でビット同一であることを検証する。
    #[test]
    fn search_layer_prefetch_with_mask_is_bit_identical() {
        let dim = 8usize;
        let n = 300;
        let vectors = gen_corpus(51, dim, n);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 51).unwrap();
        let mut mask = NodeMask::new(index.len());
        for node in 0..index.len() {
            if node % 2 == 0 {
                mask.set(node as u32);
            }
        }
        // `search_layer_with` を直接呼ぶため、`search_masked` が持つ代替
        // entry point 選択（非受理な固定 entry point からの縮退回避）は
        // 経由しない。本テストの entry point は受理済みノード（偶数）を
        // 直接選ぶことで、探索が空集合へ縮退せず非 vacuous な検証になる
        // ようにする。
        let entry = 0u32;
        assert!(
            mask.get(entry),
            "test setup: entry point must be mask-accepted"
        );
        let query = gen_corpus(123, dim, 1);
        let ef = 40usize;

        let mut visited_no = VisitedScratch::default();
        let via_no = index
            .search_layer_with(
                vec![entry],
                &query,
                ef,
                0,
                dim,
                &index.vectors,
                &mut visited_no,
                Some(&mask),
                &NoPrefetch,
            )
            .unwrap();
        let mut visited_pipeline = VisitedScratch::default();
        let via_pipeline = index
            .search_layer_with(
                vec![entry],
                &query,
                ef,
                0,
                dim,
                &index.vectors,
                &mut visited_pipeline,
                Some(&mask),
                &prefetch::PipelinePrefetch,
            )
            .unwrap();
        assert_eq!(via_no, via_pipeline);
        assert!(
            !via_no.is_empty(),
            "masked search should find some hits (non-vacuous)"
        );
    }

    /// Issue #490 の P0 契約（本 Issue で追加した先読み処理は非受理ノードの
    /// ベクトル・visited スロットのいずれへも一切触れない）を、実際に
    /// 先読み要求されたノード id を記録して直接検証する
    /// （`RecordingPrefetch`）。通常探索が非受理ノードへも訪問済みマークを
    /// 付ける既存契約（Issue #431・`search_layer` ドキュメンテーション
    /// コメント参照）はこの検証の対象外——本テストは `prefetch_neighbor`
    /// 呼び出しだけを記録し `visited.mark_visited` は見ない。記録が空だと
    /// 検証が vacuous になるため、非空であることもあわせて固定する。
    #[test]
    fn search_layer_prefetch_never_touches_rejected_nodes() {
        let dim = 8usize;
        let n = 300;
        let vectors = gen_corpus(61, dim, n);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 61).unwrap();
        let mut mask = NodeMask::new(index.len());
        for node in 0..index.len() {
            if node % 2 == 0 {
                mask.set(node as u32);
            }
        }
        // 上のテストと同じ理由（entry point は受理済みノードを直接選ぶ）。
        let entry = 0u32;
        assert!(
            mask.get(entry),
            "test setup: entry point must be mask-accepted"
        );
        let query = gen_corpus(321, dim, 1);
        let recorder = RecordingPrefetch::default();
        let mut visited = VisitedScratch::default();
        let _ = index
            .search_layer_with(
                vec![entry],
                &query,
                40,
                0,
                dim,
                &index.vectors,
                &mut visited,
                Some(&mask),
                &recorder,
            )
            .unwrap();
        let seen = recorder.seen.borrow();
        assert!(
            !seen.is_empty(),
            "test must exercise at least one prefetch call (vacuous pass prevention)"
        );
        for &node in seen.iter() {
            assert!(
                mask.get(node),
                "prefetch must never be requested for a mask-rejected node {node}"
            );
        }
    }

    #[test]
    fn search_masked_none_matches_search() {
        let dim = 8usize;
        let vectors = gen_corpus(7, dim, 200);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 7).unwrap();
        let query = gen_corpus(999, dim, 1);
        let mut scratch_a = HnswSearchScratch::default();
        let mut scratch_b = HnswSearchScratch::default();
        let via_search = index.search(&query, 10, 40, &mut scratch_a).unwrap();
        let via_masked = index
            .search_masked(&query, 10, 40, None, &mut scratch_b)
            .unwrap();
        assert_eq!(via_search, via_masked);

        // Issue #497: `mask == None` は `sparse_visited_max` の値に関わらず
        // 常に dense（`VisitedBitmap`）を使う。`usize::MAX`（マスクさえあれば
        // 必ず sparse を選ぶ極端値）を渡しても `search` とのビット同一契約は
        // 変わらないことを固定する。
        let mut scratch_c = HnswSearchScratch::default();
        let via_masked_force_sparse_threshold = index
            .search_masked_with(&query, 10, 40, None, usize::MAX, &mut scratch_c)
            .unwrap();
        assert_eq!(via_search, via_masked_force_sparse_threshold);
        assert_eq!(
            scratch_c.last_visited_kind(),
            Some(VisitedKind::Dense),
            "mask == None must always select the dense visited implementation"
        );
    }

    /// [`HnswIndex::search_masked_with`] が visited 実装（`VisitedBitmap`／
    /// `VisitedSparse`）のどちらを選んでも結果がビット同一であることを、
    /// マスクの形状が異なる複数フィクスチャで機械検証する（Issue #497 の
    /// 受け入れ条件 1）。`sparse_visited_max = usize::MAX`（マスクがあれば
    /// 必ず sparse）と `0`（常に dense。[`DEFAULT_SPARSE_VISITED_MAX`]）を
    /// 同一マスク・同一クエリへ渡し、結果集合・順序（`dot` 降順・同点 id
    /// 昇順）が完全一致することを固定する。
    #[test]
    fn search_masked_with_force_sparse_matches_force_dense_bit_identical() {
        let dim = 8usize;

        // フィクスチャ 1: 通常コーパス・偶数ノードのみ受理（密度 50%）。
        let normal_vectors = gen_corpus(61, dim, 200);
        let normal_index = HnswIndex::build(
            HnswParams {
                m: 8,
                ef_construction: 40,
                ef_search: 20,
            },
            dim as u32,
            &normal_vectors,
            200,
        )
        .unwrap();
        let mut normal_mask = NodeMask::new(normal_index.len());
        for node in 0..normal_index.len() {
            if node % 2 == 0 {
                normal_mask.set(node as u32);
            }
        }

        // フィクスチャ 2: 重複ヘビーコーパス（同点誘発。
        // `search_layer_prefetch_is_bit_identical_to_no_prefetch` と同型）・
        // 3 分の 1 のノードのみ受理。
        let mut duplicate_heavy = gen_corpus(63, dim, 20);
        duplicate_heavy = duplicate_heavy
            .iter()
            .cycle()
            .take(200 * dim)
            .copied()
            .collect();
        let duplicate_index = HnswIndex::build(
            HnswParams {
                m: 8,
                ef_construction: 40,
                ef_search: 20,
            },
            dim as u32,
            &duplicate_heavy,
            200,
        )
        .unwrap();
        let mut duplicate_mask = NodeMask::new(duplicate_index.len());
        for node in 0..duplicate_index.len() {
            if node % 3 == 0 {
                duplicate_mask.set(node as u32);
            }
        }

        // フィクスチャ 3: 単一ノードのみ受理（可視候補数 1。sparse 経路の
        // 最小ケース）。
        let single_vectors = gen_corpus(65, dim, 150);
        let single_index = HnswIndex::build(
            HnswParams {
                m: 8,
                ef_construction: 40,
                ef_search: 20,
            },
            dim as u32,
            &single_vectors,
            150,
        )
        .unwrap();
        let mut single_mask = NodeMask::new(single_index.len());
        single_mask.set(0);

        // フィクスチャ 4: 固定 entry point を非受理にした代替起点経路
        // （`search_masked_falls_back_to_alternate_entry_when_fixed_entry_masked_out`
        // と同型）。
        let entry_vectors = gen_corpus(67, dim, 300);
        let entry_index = HnswIndex::build(
            HnswParams {
                m: 8,
                ef_construction: 40,
                ef_search: 20,
            },
            dim as u32,
            &entry_vectors,
            300,
        )
        .unwrap();
        let entry = entry_index
            .entry_point()
            .expect("non-empty index has an entry point");
        let mut entry_mask = NodeMask::new(entry_index.len());
        for node in 0..entry_index.len() as u32 {
            if node != entry {
                entry_mask.set(node);
            }
        }

        let cases: [(&str, &HnswIndex, &NodeMask, u64); 4] = [
            ("normal", &normal_index, &normal_mask, 1234),
            ("duplicate_heavy", &duplicate_index, &duplicate_mask, 5678),
            ("single_visible", &single_index, &single_mask, 91),
            ("entry_excluded", &entry_index, &entry_mask, 4321),
        ];

        for (name, index, mask, query_seed) in cases {
            let query = gen_corpus(query_seed, dim, 1);
            let mut scratch_sparse = HnswSearchScratch::default();
            let mut scratch_dense = HnswSearchScratch::default();
            let via_sparse = index
                .search_masked_with(&query, 10, 40, Some(mask), usize::MAX, &mut scratch_sparse)
                .unwrap();
            let via_dense = index
                .search_masked_with(&query, 10, 40, Some(mask), 0, &mut scratch_dense)
                .unwrap();
            assert_eq!(
                via_sparse, via_dense,
                "fixture={name}: sparse and dense visited implementations must return \
                 bit-identical results"
            );
            // 可視候補が 1 件以上あるフィクスチャでは実際に sparse 側が選ばれた
            // ことも確認する（閾値判定そのものの回帰も兼ねる）。
            if mask.count_ones() > 0 {
                assert_eq!(
                    scratch_sparse.last_visited_kind(),
                    Some(VisitedKind::Sparse),
                    "fixture={name}: sparse_visited_max=usize::MAX with a non-empty mask \
                     must select VisitedSparse"
                );
            }
            assert_eq!(
                scratch_dense.last_visited_kind(),
                Some(VisitedKind::Dense),
                "fixture={name}: sparse_visited_max=0 must always select VisitedBitmap"
            );
        }
    }

    #[test]
    fn search_masked_results_are_subset_of_mask() {
        let dim = 8usize;
        let n = 200;
        let vectors = gen_corpus(11, dim, n);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 11).unwrap();
        let mut mask = NodeMask::new(index.len());
        // 偶数ノードのみ受理する（密度 50%）。
        for node in 0..index.len() {
            if node % 2 == 0 {
                mask.set(node as u32);
            }
        }
        // Issue #431 是正により、探索経路（貪欲降下・ビーム探索の候補展開）は
        // マスク外ノードを一切通過しなくなった。エントリポイント自身が非受理でも
        // 代替探索起点選択（codex-review P2 指摘対応・§`search_masked` ドキュメン
        // テーションコメント参照）により空集合へは縮退しなくなったため、本テスト
        // ではエントリポイントを意図的に mask から外したまま「受理ノードの
        // 部分集合が返る」性質を検証する（代替起点選択の直接的な検証は
        // `search_masked_falls_back_to_alternate_entry_when_fixed_entry_masked_out`）。
        let entry = index
            .entry_point()
            .expect("non-empty index has an entry point");
        let _ = entry;
        let query = gen_corpus(1234, dim, 1);
        let mut scratch = HnswSearchScratch::default();
        let results = index
            .search_masked(&query, 20, 80, Some(&mask), &mut scratch)
            .unwrap();
        assert!(!results.is_empty(), "masked search should find some hits");
        for hit in &results {
            assert!(
                mask.get(hit.id as u32),
                "hit {} must be within the mask",
                hit.id
            );
        }
    }

    #[test]
    fn search_masked_all_false_returns_empty() {
        let dim = 8usize;
        let n = 100;
        let vectors = gen_corpus(3, dim, n);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 3).unwrap();
        let mask = NodeMask::new(index.len());
        let query = gen_corpus(55, dim, 1);
        let mut scratch = HnswSearchScratch::default();
        let results = index
            .search_masked(&query, 10, 40, Some(&mask), &mut scratch)
            .unwrap();
        assert!(results.is_empty());
    }

    /// codex-review P2 指摘対応（§`search_masked` ドキュメンテーションコメント
    /// 「固定 entry point 自体が `mask` に非受理の場合」節）: 固定 entry point
    /// だけをマスク外にし、他の受理ノードは十分に残す（密度 99%）。旧実装では
    /// entry point の非受理だけで貪欲降下の起点を持てず直ちに空集合を返して
    /// いたが、代替探索起点選択（[`HnswIndex::find_alternate_entry`]）により
    /// 受理ノードから探索できることを確認する。
    #[test]
    fn search_masked_falls_back_to_alternate_entry_when_fixed_entry_masked_out() {
        let dim = 8usize;
        let n = 300;
        let vectors = gen_corpus(21, dim, n);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 21).unwrap();
        let entry = index
            .entry_point()
            .expect("non-empty index has an entry point");

        // 固定 entry point だけを非受理にし、他の全ノードを受理する
        // （`NodeMask` に unset API は無いため、entry を除いて `set` する）。
        let mut mask = NodeMask::new(index.len());
        for node in 0..index.len() as u32 {
            if node != entry {
                mask.set(node);
            }
        }
        assert!(!mask.get(entry), "fixed entry point must be masked out");

        let query = gen_corpus(4321, dim, 1);
        let mut scratch = HnswSearchScratch::default();
        let results = index
            .search_masked(&query, 20, 80, Some(&mask), &mut scratch)
            .unwrap();
        assert!(
            !results.is_empty(),
            "masked search must fall back to an alternate entry point instead of \
             degrading to an empty result solely because the fixed entry point is \
             masked out"
        );
        for hit in &results {
            assert!(
                mask.get(hit.id as u32),
                "hit {} must be within the mask (never the excluded entry point {entry})",
                hit.id
            );
            assert_ne!(hit.id as u32, entry);
        }
    }

    /// [`HnswIndex::search_entry_for_mask`] を [`HnswIndex::search_masked`]（探索）
    /// と [`HnswIndex::is_mask_fully_reachable`]（分断検査）の双方が共有して
    /// いることの回帰テスト（Cursor Bugbot High 指摘・PR #435）。固定 entry
    /// point（`node0`）を非受理にし、代替起点として選ばれるはずの `node1` を
    /// 起点とする一本道 `node1 -> node2 -> node3`（全て受理）だけをマスクに
    /// 含める。もし検査側が代替起点選択を共有せず誤って `node0`（非受理）
    /// から辿ろうとすれば、`is_mask_fully_reachable` の内部 BFS は開始点が
    /// 非受理のため直ちに 0 件を返し、分断なしのマスクを「分断あり」と
    /// 誤検知して不要な plain scan を招く。共有選択が機能していれば、
    /// 検査は `search_masked` と同じ `node1` から辿り、分断なしと正しく
    /// 判定する。
    #[test]
    fn is_mask_fully_reachable_uses_the_same_alternate_entry_as_search_masked() {
        let dim = 1usize;
        let vectors: Vec<f32> = vec![0.0, 1.0, 2.0, 3.0];
        let index = index_from_nodes(
            HnswParams::default(),
            dim as u32,
            vec![
                // node0: entry point（マスクで非受理にする）。他ノードとは
                // 無関係な孤立ノードにしておき、誤って起点に使われた場合の
                // 挙動が明確になるようにする。
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
                Node {
                    level: 0,
                    links: vec![vec![2]],
                },
                Node {
                    level: 0,
                    links: vec![vec![3]],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            Some(0),
            Arc::from(vectors),
        );

        let mut mask = NodeMask::new(index.len());
        mask.set(1);
        mask.set(2);
        mask.set(3);
        // node0（entry point）は非受理のまま。

        assert!(
            index.is_mask_fully_reachable(&mask),
            "the mask's accepted nodes (1, 2, 3) form a single chain reachable from \
             the shared alternate entry point (node1); if the reachability check used \
             a different (rejected) start node it would wrongly report a split"
        );

        let query = [1.0f32];
        let mut scratch = HnswSearchScratch::default();
        let results = index
            .search_masked(&query, 3, 10, Some(&mask), &mut scratch)
            .expect("masked search should succeed");
        assert_eq!(
            results.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![3, 2, 1],
            "search_masked must reach all three accepted nodes via the same \
             alternate entry point the reachability check used"
        );
    }

    /// codex-review P1 指摘の中核シナリオ（PR #435）: 到達可能性を検査した
    /// 起点（[`HnswIndex::search_entry_for_mask`] が選ぶ `node0`）と、クエリ
    /// 依存の多層貪欲降下が層 0 探索へ渡す起点（`node2`）が異なり、かつ層 0
    /// の隣接が有向（`node0 -> {node1, node2}` だが `node2` に出辺なし）な
    /// 場合を手作りグラフで再現する。
    ///
    /// - `node0`（entry・level1）はクエリスコア 5.0、level1 の隣接が
    ///   `node2`（level1・スコア 10.0）のみで、貪欲降下は必ず `node0` から
    ///   `node2` へ移動する。
    /// - `node0` の層 0 隣接は `{node1, node2}` を直接含むが、`node2` の層 0
    ///   隣接は空——`node2` を唯一の起点にして層 0 探索を始めると `node1`
    ///   （クエリに最も近い・スコア 100.0）へ構造的に到達できない。
    /// - `is_mask_fully_reachable` は検査済み起点 `node0` から辿るため
    ///   `node1`／`node2` いずれにも到達でき、マスクは「分断なし」と判定
    ///   される（呼び出し元は `search_masked` を素通しで呼ぶ契約）。
    ///
    /// 修正前は層 0 探索の初期候補が降下後ノード `node2` のみだったため、
    /// 分断なしと判定されたにもかかわらず最も近い `node1` を取りこぼした。
    /// 本テストは層 0 探索の初期候補へ検査済み起点 `node0` を必ず含める
    /// ことで `node1` が結果に現れることを固定する。
    #[test]
    fn search_masked_includes_checked_entry_as_level0_seed_when_descent_lands_elsewhere() {
        let dim = 1usize;
        // dot(v, q) = v[0] * q[0]、q=[1.0] なのでスコアは値そのもの。
        let vectors: Vec<f32> = vec![5.0, 100.0, 10.0];
        let index = index_from_nodes(
            HnswParams::default(),
            dim as u32,
            vec![
                // node0: entry。level1 隣接は node2（スコア上位のため貪欲降下
                // は必ずここへ移動する）。level0 隣接は node1・node2 の両方
                // （有向: node2 側からの逆辺は無い）。
                Node {
                    level: 1,
                    links: vec![vec![1, 2], vec![2]],
                },
                // node1: クエリに最も近い（スコア 100.0）。level0 隣接なし
                // （孤立扱いで十分。node0 からの片道到達だけを検証する）。
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
                // node2: 貪欲降下の着地点。level0 隣接なし（node1 へは辿れ
                // ない）。level1 隣接なし（降下はここで停止する）。
                Node {
                    level: 1,
                    links: vec![Vec::new(), Vec::new()],
                },
            ],
            Some(0),
            Arc::from(vectors),
        );

        let mut mask = NodeMask::new(index.len());
        mask.set(0);
        mask.set(1);
        mask.set(2);

        assert!(
            index.is_mask_fully_reachable(&mask),
            "all three accepted nodes must be reachable from the checked entry \
             (node0), which has direct level-0 edges to both node1 and node2"
        );

        let query = [1.0f32];
        let mut scratch = HnswSearchScratch::default();
        let masked = index
            .search_masked(&query, 3, 10, Some(&mask), &mut scratch)
            .expect("masked search should succeed");
        assert_eq!(
            masked.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![1, 2, 0],
            "search_masked must seed level-0 search with the checked entry \
             (node0) in addition to the post-descent node (node2), so node1 \
             (reachable only via node0's directed level-0 edge) must not be \
             dropped even though is_mask_fully_reachable reports no split"
        );
    }

    /// codex-review P2 指摘の中核シナリオ（PR #435。呼び出し元側の分断検査へ
    /// 移設した際に overlay 時判定のテストへ書き換え）: マスクが複数の連結
    /// 成分に分かれ、かつ **entry 側の成分だけで結果件数 `k` を満たして
    /// しまう**場合（`search_masked_does_not_traverse_through_a_rejected_
    /// bridge_node` のケースは `k` が成分サイズを上回り件数検査でも検出
    /// できたが、本ケースは件数検査だけでは検出できない）。
    ///
    /// entry 側成分 `{node0(entry), node1, node2}`（`0 -> 1 -> 2` の一本道、
    /// スコアは 1.0/2.0/3.0）はすべて受理され、`k=2` の結果件数は成分内だけで
    /// 満たせる。一方、entry 側成分と辺で一切繋がっていない孤立ノード
    /// `node3`（スコア 100.0・受理済み）がクエリに最も近い真の正解だが、
    /// 単一 entry point からの誘導部分グラフ探索では構造的に到達不能。
    /// [`HnswIndex::is_mask_fully_reachable`]（`sql::hnsw_cache::Overlay::
    /// compute` が世代毎に 1 回呼ぶ、`search_masked` とは独立の分断検査）が
    /// 「entry 側成分（3 件）だけではマスクの受理ノード総数（4 件）を覆え
    /// ない」ことを検出することを固定する。`search_masked` 自体はこの検査を
    /// 行わないため、`node1`・`node2` だけの部分結果を返す（件数検査は通過
    /// するが node3 を取りこぼす——この recall バグを実際に防ぐのは呼び
    /// 出し元が `is_mask_fully_reachable` を見て `search_masked` 自体を
    /// 呼ばない判断をすること）。
    #[test]
    fn is_mask_fully_reachable_detects_unreachable_component_even_when_reachable_component_satisfies_k(
    ) {
        let dim = 1usize;
        let vectors: Vec<f32> = vec![1.0, 2.0, 3.0, 100.0];
        let index = index_from_nodes(
            HnswParams::default(),
            dim as u32,
            vec![
                Node {
                    level: 0,
                    links: vec![vec![1]],
                },
                Node {
                    level: 0,
                    links: vec![vec![2]],
                },
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
                // node3: entry 側成分とは辺を一切持たない孤立ノード（スコア最良）。
                Node {
                    level: 0,
                    links: vec![Vec::new()],
                },
            ],
            Some(0),
            Arc::from(vectors),
        );
        let query = [1.0f32];
        let mut scratch = HnswSearchScratch::default();

        let mut mask = NodeMask::new(index.len());
        mask.set(0);
        mask.set(1);
        mask.set(2);
        mask.set(3); // node3 も受理（到達不能なだけで RLS 上は可視）。

        // 分断検査（呼び出し元の責務）は「entry 側成分だけで k を満たす」
        // ケースでも到達不能な node3 を正しく検出する。
        assert!(
            !index.is_mask_fully_reachable(&mask),
            "node3 is mask-accepted but structurally unreachable from the single \
             entry point, so the mask must be reported as split even though the \
             reachable component alone would satisfy k=2"
        );

        // `search_masked` 自体はこの検査を行わないため、entry 側成分内の
        // 部分結果（node3 を欠く）をそのまま返す——この防止は呼び出し元が
        // 上記の `is_mask_fully_reachable` を見て `search_masked` を呼ばない
        // 判断をすることで実現される（§本関数ドキュメンテーションコメント
        // 参照）。
        let masked = index
            .search_masked(&query, 2, 10, Some(&mask), &mut scratch)
            .expect("masked search should succeed");
        assert_eq!(
            masked.iter().map(|h| h.id).collect::<Vec<_>>(),
            vec![2, 1],
            "search_masked itself only explores the entry-reachable component and \
             returns its top-k (node2, node1); node3 is absent because search_masked \
             does not perform graph-split detection"
        );
    }

    #[test]
    fn search_masked_rejects_length_mismatch() {
        let dim = 4usize;
        let vectors = gen_corpus(2, dim, 50);
        let params = HnswParams {
            m: 6,
            ef_construction: 32,
            ef_search: 16,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 2).unwrap();
        // 索引のノード数より短いマスクは拒否される（fail-closed）。
        let mask = NodeMask::new(index.len().saturating_sub(1));
        let query = gen_corpus(8, dim, 1);
        let mut scratch = HnswSearchScratch::default();
        let err = index
            .search_masked(&query, 5, 20, Some(&mask), &mut scratch)
            .unwrap_err();
        assert!(matches!(err, HnswError::InvalidParams { .. }));
    }

    /// codex-review PR #430 P1 指摘への対応で `HnswIndex` は `build` 時の
    /// `vectors` を `Arc<[f32]>` として所有するよう変更した（モジュール冒頭
    /// 「ベクトルの所有方針」節）。呼び出し元が `build` に渡した元のバッファ
    /// （`Vec<f32>`）を構築後に書き換えても、`search` は build 時点で取得した
    /// 不変スナップショットのみを参照するため一切影響を受けないことを固定する
    /// ——旧設計（`search` へ毎回 `&[f32]` を渡す方式）ではサンプリング対象外
    /// の書き換えが静かに受理され得たが、この設計では「別バッファが search に
    /// 渡される」という入力のクラス自体が存在しない。
    #[test]
    fn search_is_unaffected_by_mutations_to_the_caller_owned_build_buffer() {
        let dim = 8usize;
        let rows = 50usize;
        let mut vectors = gen_corpus(0xAAAA_1111, dim, rows);
        let index =
            HnswIndex::build(HnswParams::default(), dim as u32, &vectors, 0xBBBB_2222).unwrap();

        let query: Vec<f32> = vectors[0..dim].to_vec();
        let mut scratch = HnswSearchScratch::default();
        let before = index.search(&query, 5, 32, &mut scratch).unwrap();

        // ノード 0 とノード 1 の行を入れ替え、さらにノード 40（サンプリング
        // 方式なら見逃しうる位置）の要素も書き換える。呼び出し元が所有する
        // `vectors` を直接破壊しているが、index はこのバッファを一切参照しない。
        let (front, back) = vectors.split_at_mut(dim);
        front[..dim].swap_with_slice(&mut back[..dim]);
        vectors[40 * dim] += 1.0;
        drop(vectors); // index が自身のスナップショットのみで完結することを明示する。

        let after = index.search(&query, 5, 32, &mut scratch).unwrap();
        assert_eq!(
            before, after,
            "search must be based solely on the build-time snapshot, unaffected by \
             the caller mutating or dropping its own copy of the vectors buffer"
        );
    }

    /// PR #431 codex-review（Cursor Bugbot）Medium 指摘の回帰: `protect` が
    /// `current_links` に含まれない状態（並列構築で他ワーカーが先に
    /// `protect` への逆方向リンクを縮退させてしまうレース。`compute_shrink`
    /// のドキュメンテーションコメント参照）でも、`compute_shrink` の結果
    /// 集合には必ず `protect` が含まれ、かつ `limit` 件を超えないことを
    /// 固定する。
    #[test]
    fn compute_shrink_always_retains_protect_even_when_absent_from_current_links() {
        let dim = 1usize;
        // ノード 0（自分自身）とノード 1..=5 の 1 次元ベクトルを用意する。
        // `protect`（ノード 6）は `current_links` に含めない
        // （＝並列構築下で既に他ワーカーに縮退されてしまった状況を模す）。
        let vectors: Vec<f32> = vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        let current_links: Vec<u32> = vec![1, 2, 3, 4, 5];
        let limit = 3usize;
        let protect = 6u32;

        let shrunk = compute_shrink(&current_links, 0, dim, &vectors, limit, protect)
            .expect("finite 1-d vectors must not overflow")
            .expect("current_links.len() > limit must trigger a shrink");

        assert!(
            shrunk.contains(&protect),
            "protect must survive the shrink even when absent from current_links, got {shrunk:?}"
        );
        assert!(
            shrunk.len() <= limit,
            "shrink result must not exceed the degree limit, got {shrunk:?}"
        );
    }

    /// 上記の派生ケース: `current_links.len() <= limit` でも `protect` が
    /// 欠けていれば `Ok(None)`（変更不要）を返してはならない——`None` は
    /// 呼び出し元に「既存のリンクのままでよい」と伝えるため、`protect` が
    /// 欠けたまま何も書き戻されないと `protect` は永久に失われる。
    #[test]
    fn compute_shrink_adds_protect_when_missing_even_under_the_degree_limit() {
        let dim = 1usize;
        let vectors: Vec<f32> = vec![0.0, 1.0, 2.0, 6.0];
        let current_links: Vec<u32> = vec![1, 2];
        let limit = 3usize;
        let protect = 3u32;

        let shrunk = compute_shrink(&current_links, 0, dim, &vectors, limit, protect)
            .expect("finite 1-d vectors must not overflow")
            .expect("protect missing from current_links must not short-circuit to Ok(None)");

        assert!(
            shrunk.contains(&protect),
            "protect must be added even when current_links is already within the degree limit, \
             got {shrunk:?}"
        );
        assert!(shrunk.len() <= limit, "got {shrunk:?}");
    }

    // ---------- f16 常駐（Issue #514・親 #513。ポインタ: TASK-132・TASK-156・CORE-16） ----------

    /// T-H1: 同一入力で `build`（F32）と `build_with_precision(F16)` の
    /// グラフ（`entry_point`／`level_of`／`neighbors`）が全ノード・全層で一致
    /// すること（D7: 構築は常に f32、精度は凍結時にのみ影響する）。
    #[test]
    fn f16_precision_produces_identical_graph_shape_to_f32() {
        let dim = 8usize;
        let n = 200;
        let vectors = gen_corpus(0x0514_af16, dim, n);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let f32_index = HnswIndex::build(params, dim as u32, &vectors, 42).unwrap();
        let f16_index = HnswIndex::build_with_precision(
            params,
            ResidentPrecision::F16,
            dim as u32,
            &vectors,
            42,
        )
        .unwrap();

        assert_eq!(f16_index.resident_precision(), ResidentPrecision::F16);
        assert_eq!(f32_index.entry_point(), f16_index.entry_point());
        assert_eq!(f32_index.len(), f16_index.len());
        for node in 0..f32_index.len() as u32 {
            assert_eq!(
                f32_index.level_of(node),
                f16_index.level_of(node),
                "node={node}"
            );
            let max_level = f32_index.level_of(node).unwrap_or(0);
            for level in 0..=max_level {
                assert_eq!(
                    f32_index.neighbors(level, node),
                    f16_index.neighbors(level, node),
                    "node={node} level={level}"
                );
            }
        }
    }

    /// T-H2: F16 常駐索引の `approx_heap_bytes` が F32 常駐索引より小さいこと
    /// （R1: 常駐メモリ半減が目的）。
    #[test]
    fn f16_precision_uses_less_heap_than_f32() {
        let dim = 32usize;
        let n = 500;
        let vectors = gen_corpus(0x1620, dim, n);
        let params = HnswParams::default();
        let f32_index = HnswIndex::build(params, dim as u32, &vectors, 7).unwrap();
        let f16_index = HnswIndex::build_with_precision(
            params,
            ResidentPrecision::F16,
            dim as u32,
            &vectors,
            7,
        )
        .unwrap();
        assert!(
            f16_index.approx_heap_bytes() < f32_index.approx_heap_bytes(),
            "f16={} f32={}",
            f16_index.approx_heap_bytes(),
            f32_index.approx_heap_bytes()
        );
    }

    /// T-H3: `node_matches`（D4）・`vector`／`vector_f16`（D5）の契約。
    #[test]
    fn f16_node_matches_and_accessors_follow_d4_d5_contract() {
        let dim = 4usize;
        let n = 50;
        let vectors = gen_corpus(0x0d4, dim, n);
        let f16_index = HnswIndex::build_with_precision(
            HnswParams::default(),
            ResidentPrecision::F16,
            dim as u32,
            &vectors,
            3,
        )
        .unwrap();
        assert_eq!(f16_index.resident_precision(), ResidentPrecision::F16);

        // D5: F16 常駐時は `vector()` は常に None、`vector_f16()` は Some。
        assert!(f16_index.vector(0).is_none());
        assert!(f16_index.vector_f16(0).is_some());
        assert_eq!(f16_index.vector_f16(u32::try_from(n).unwrap()), None);

        // D4: 未変更（同一 f32 行）は一致と判定される。
        let row: Vec<f32> = vectors[0..dim].to_vec();
        assert_eq!(f16_index.node_matches(0, &row), Some(true));

        // D4: 大きな変更は不一致と判定される（f16 の分解能を大きく超える差）。
        let mut changed = row.clone();
        changed[0] += 100.0;
        assert_eq!(f16_index.node_matches(0, &changed), Some(false));

        // D4: f16 分解能未満の摂動は「未変更」と判定されうる（最終スコアは
        // 常に f32 アリーナから再計算されるため結果の正しさには影響しない）。
        let mut tiny = row.clone();
        tiny[0] += 1e-7;
        assert_eq!(f16_index.node_matches(0, &tiny), Some(true));

        // 範囲外ノード・次元不一致は None。
        assert_eq!(
            f16_index.node_matches(u32::try_from(n).unwrap(), &row),
            None
        );
        assert_eq!(f16_index.node_matches(0, &row[..dim - 1]), None);
    }

    /// T-H4: f16 の有限範囲（`|x| <= 65504.0`）を超える成分を含む入力を `F16`
    /// 指定で build すると `F32` へ自動縮退し（D6）、`search` は成功する。
    #[test]
    fn f16_precision_falls_back_to_f32_when_a_component_is_out_of_range() {
        let dim = 4usize;
        let mut vectors = gen_corpus(0x0fa11, dim, 30);
        // 1 成分だけ f16 範囲外にする。
        if let Some(v) = vectors.get_mut(2) {
            *v = 100_000.0;
        }
        let index = HnswIndex::build_with_precision(
            HnswParams::default(),
            ResidentPrecision::F16,
            dim as u32,
            &vectors,
            9,
        )
        .unwrap();
        assert_eq!(
            index.resident_precision(),
            ResidentPrecision::F32,
            "out-of-range component must trigger fallback to F32 residency"
        );
        // 縮退後も通常どおり探索できる（F32 常駐のため `vector()` は Some）。
        assert!(index.vector(0).is_some());
        let query = gen_corpus(0x0fa12, dim, 1);
        let mut scratch = HnswSearchScratch::default();
        let hits = index.search(&query, 5, 40, &mut scratch).unwrap();
        assert!(!hits.is_empty());
    }

    /// T-H5: F16 常駐索引の `search` が brute-force（`kernel::dot`）対照で
    /// 妥当な Recall@10 を達成すること（クラスタ構造ありフィクスチャ。層 A
    /// 縮小規模）。既存の F32 版 `hnsw_search.rs` 層 A と同じ判定方式を、
    /// f16 常駐についても固定する。
    #[test]
    fn f16_precision_search_achieves_reasonable_recall_against_brute_force() {
        let dim = 16usize;
        let n = 600;
        // クラスタ構造ありフィクスチャ（`hnsw_search.rs` の層 A と同じ方式:
        // 少数の中心点周辺に密集させる）。
        let mut rng = DeterministicRng::new(0xc111);
        let n_clusters = 6usize;
        let centers: Vec<f32> = (0..n_clusters * dim)
            .map(|_| {
                let bits = rng.next_u64() >> 40;
                ((bits as f32) / (1u32 << 24) as f32) * 2.0 - 1.0
            })
            .collect();
        let mut vectors = Vec::with_capacity(n * dim);
        for i in 0..n {
            let c = i % n_clusters;
            for d in 0..dim {
                let bits = rng.next_u64() >> 40;
                let jitter = ((bits as f32) / (1u32 << 24) as f32) * 0.1 - 0.05;
                vectors.push(centers[c * dim + d] + jitter);
            }
        }

        let params = HnswParams {
            m: 16,
            ef_construction: 100,
            ef_search: 64,
        };
        let index = HnswIndex::build_with_precision(
            params,
            ResidentPrecision::F16,
            dim as u32,
            &vectors,
            11,
        )
        .unwrap();
        assert_eq!(index.resident_precision(), ResidentPrecision::F16);

        let queries = 40;
        let k = 10usize;
        let ef = 64usize;
        let mut scratch = HnswSearchScratch::default();
        let mut recall_hits = 0usize;
        let mut recall_total = 0usize;
        for q in 0..queries {
            let query = gen_corpus(0x0c1a55 + q as u64, dim, 1);
            let mut brute: Vec<ScoredNode> = (0..n as u32)
                .map(|node| ScoredNode {
                    node,
                    score: dot(node_vector(&vectors, dim, node).unwrap(), &query),
                })
                .collect();
            brute.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
            let truth: std::collections::HashSet<u32> =
                brute.iter().take(k).map(|s| s.node).collect();

            let hits = index.search(&query, k, ef, &mut scratch).unwrap();
            recall_total += truth.len();
            recall_hits += hits
                .iter()
                .filter(|h| truth.contains(&(h.id as u32)))
                .count();
        }
        let recall = recall_hits as f64 / recall_total as f64;
        assert!(
            recall >= 0.7,
            "f16 resident recall@{k} too low: {recall} ({recall_hits}/{recall_total})"
        );
    }

    // ---------- SQ8（i8）常駐（Issue #521・親 #520。ポインタ: TASK-132・TASK-156・CORE-16） ----------

    /// T-I1: 同一入力で `build`（F32）と `build_with_precision(I8)` の
    /// グラフ（`entry_point`／`level_of`／`neighbors`）が全ノード・全層で
    /// 一致すること（f16 版 T-H1 と同型。凍結時にのみ精度が影響する）。
    #[test]
    fn i8_precision_produces_identical_graph_shape_to_f32() {
        let dim = 8usize;
        let n = 200;
        let vectors = gen_corpus(0x0521_a1a8, dim, n);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let f32_index = HnswIndex::build(params, dim as u32, &vectors, 42).unwrap();
        let i8_index = HnswIndex::build_with_precision(
            params,
            ResidentPrecision::I8,
            dim as u32,
            &vectors,
            42,
        )
        .unwrap();

        assert_eq!(i8_index.resident_precision(), ResidentPrecision::I8);
        assert_eq!(f32_index.entry_point(), i8_index.entry_point());
        assert_eq!(f32_index.len(), i8_index.len());
        for node in 0..f32_index.len() as u32 {
            assert_eq!(
                f32_index.level_of(node),
                i8_index.level_of(node),
                "node={node}"
            );
            let max_level = f32_index.level_of(node).unwrap_or(0);
            for level in 0..=max_level {
                assert_eq!(
                    f32_index.neighbors(level, node),
                    i8_index.neighbors(level, node),
                    "node={node} level={level}"
                );
            }
        }
    }

    /// T-I2: I8 常駐索引の `approx_heap_bytes` が F32 常駐索引より小さいこと
    /// （i8 はベクトル本体が約 1/4。`params`〔次元ごとのスケール〕分を
    /// 差し引いても F32 を下回る）。
    #[test]
    fn i8_precision_uses_less_heap_than_f32() {
        let dim = 32usize;
        let n = 500;
        let vectors = gen_corpus(0x1521, dim, n);
        let params = HnswParams::default();
        let f32_index = HnswIndex::build(params, dim as u32, &vectors, 7).unwrap();
        let i8_index =
            HnswIndex::build_with_precision(params, ResidentPrecision::I8, dim as u32, &vectors, 7)
                .unwrap();
        assert!(
            i8_index.approx_heap_bytes() < f32_index.approx_heap_bytes(),
            "i8={} f32={}",
            i8_index.approx_heap_bytes(),
            f32_index.approx_heap_bytes()
        );
    }

    /// T-I3: `node_matches`・`vector`／`vector_i8` の契約（f16 版 T-H3 と同型）。
    /// 加えて i8 固有の「範囲外は先に不一致と判定する」契約（Issue #521。
    /// クランプによる誤「未変更」判定の回帰防止）を固定する。
    #[test]
    fn i8_node_matches_and_accessors_follow_contract() {
        let dim = 4usize;
        let n = 50;
        // gen_corpus は [-1, 1] の値のみを生成する（対称量子化のスケールは
        // 高々 1/127 程度、範囲は高々 [-1, 1] 付近に収まる）。
        let vectors = gen_corpus(0x0521, dim, n);
        let i8_index = HnswIndex::build_with_precision(
            HnswParams::default(),
            ResidentPrecision::I8,
            dim as u32,
            &vectors,
            3,
        )
        .unwrap();
        assert_eq!(i8_index.resident_precision(), ResidentPrecision::I8);

        // I8 常駐時は `vector()` は常に None、`vector_i8()` は Some。
        assert!(i8_index.vector(0).is_none());
        assert!(i8_index.vector_i8(0).is_some());
        assert_eq!(i8_index.vector_i8(u32::try_from(n).unwrap()), None);

        // 未変更（同一 f32 行）は一致と判定される。
        let row: Vec<f32> = vectors[0..dim].to_vec();
        assert_eq!(i8_index.node_matches(0, &row), Some(true));

        // fit 済み範囲（高々 [-1, 1] 付近）を大きく超える候補は、範囲検査
        // （Issue #521）が量子化前に先んじて不一致と判定する——127 段の粗い
        // 量子化ではクランプにより「たまたま同じコード」になり得るため
        // （クランプによる誤「未変更」判定の回帰防止）。
        let mut out_of_range = row.clone();
        out_of_range[0] = 500.0;
        assert_eq!(i8_index.node_matches(0, &out_of_range), Some(false));

        // 範囲外ノード・次元不一致は None。
        assert_eq!(i8_index.node_matches(u32::try_from(n).unwrap(), &row), None);
        assert_eq!(i8_index.node_matches(0, &row[..dim - 1]), None);
    }

    /// T-I3b: `node_matches` の範囲検査が subnormal スケールでも未変更
    /// ベクトルを誤って不一致と判定しないことを固定する（PR #617
    /// codex-review P2 指摘）。ある次元の最大絶対値が `f32::from_bits(190)`
    /// 程度の極小値（subnormal）のとき `fit_dim_params` が
    /// `scale = f32::from_bits(1)`（f32 の最小正 subnormal）を受理しうる。
    /// subnormal 域では f32 の刻み幅（ULP）が値に比例せず固定の絶対ステップに
    /// なるため、相対許容誤差（`limit.abs() * f32::EPSILON`）だけでは
    /// `scale * 127` の丸め誤差を吸収できず、fit 時点そのままの未変更行が
    /// `Some(false)`（不一致）と誤判定されて世代更新のたびに不要な索引
    /// 再構築を誘発しうる（Overlay::compute が全行変更扱いする経路）。
    #[test]
    fn i8_node_matches_absorbs_subnormal_scale_rounding_error() {
        let dim = 4usize;
        let n = 30;
        let mut vectors = gen_corpus(0x0521b, dim, n);
        // 次元 0 を全行同じ subnormal 極小値へ揃える（min_d == max_d ==
        // f32::from_bits(190) なので max_abs もその値になり、scale は
        // subnormal 域に丸まる）。
        let extreme = f32::from_bits(190);
        for row in vectors.chunks_exact_mut(dim) {
            row[0] = extreme;
        }
        let i8_index = HnswIndex::build_with_precision(
            HnswParams::default(),
            ResidentPrecision::I8,
            dim as u32,
            &vectors,
            5,
        )
        .unwrap();
        assert_eq!(i8_index.resident_precision(), ResidentPrecision::I8);

        // 未変更（fit にそのまま使った行）は一致と判定されなければならない。
        let row: Vec<f32> = vectors[0..dim].to_vec();
        assert_eq!(i8_index.node_matches(0, &row), Some(true));
    }

    /// T-I4: 非有限成分を含む入力を `I8` 指定で build すると `F32` へ自動縮退し
    /// （D6 と同型）、`search` は成功する（`fit_dim_params`／`encode_rows` の
    /// `Sq8Error::NonFinite` を経由する経路。`validate_build_input` が通常は
    /// 非有限成分を事前拒否するため到達しないが、`freeze_from` 自体の
    /// fail-closed 契約を独立に固定する）。このコーパス自体は常に有限な
    /// ため実際には縮退せず I8 のまま build が成功することを確認するのみで、
    /// 縮退分岐そのものは踏まない（縮退分岐を実際に踏む検証は直後の
    /// `i8_precision_falls_back_to_f32_when_scale_underflows` が担う。
    /// PR #617 codex-review P2 指摘: 本テストのみでは縮退分岐の壊れを
    /// 検出できない）。
    #[test]
    fn i8_precision_falls_back_to_f32_when_fit_or_encode_fails() {
        let dim = 4usize;
        let vectors = gen_corpus(0x0fa1a8, dim, 30);
        let index = HnswIndex::build_with_precision(
            HnswParams::default(),
            ResidentPrecision::I8,
            dim as u32,
            &vectors,
            9,
        )
        .unwrap();
        // このコーパスは常に有限のため通常は I8 のまま。
        assert_eq!(index.resident_precision(), ResidentPrecision::I8);
        let query = gen_corpus(0x0fa1a9, dim, 1);
        let mut scratch = HnswSearchScratch::default();
        let hits = index.search(&query, 5, 40, &mut scratch).unwrap();
        assert!(!hits.is_empty());
    }

    /// T-I4b: `fit_dim_params` が `Sq8Error::ScaleOutOfRange` を実際に返す
    /// 入力（次元 0 を非零・かつ f64→f32 丸めでスケールが 0.0 へ
    /// アンダーフローする極小の定数値へ揃えたもの）を `I8` 指定で build
    /// すると、`freeze_from` が実際に `F32` 常駐へ縮退し（D6 と同型）、
    /// `search` はそのまま成功することを固定する（PR #617 codex-review P2
    /// 指摘対応。上の T-I4 は有限コーパスのみで縮退分岐を一度も踏まないため、
    /// 本テストで縮退分岐が壊れていないことを直接検証する）。
    #[test]
    fn i8_precision_falls_back_to_f32_when_scale_underflows() {
        let dim = 4usize;
        let mut vectors = gen_corpus(0x0fa1aa, dim, 30);
        // f32 の最小正 subnormal（約 1.4e-45）に対し scale = max_abs / 127
        // が下回るよう、次元 0 を全行同じ極小値へ差し替える（min_d ==
        // max_d == 1e-44 なので max_abs == 1e-44、scale ≈ 7.9e-47 は f32
        // へ丸めると 0.0 になる）。
        for row in vectors.chunks_exact_mut(dim) {
            row[0] = 1e-44;
        }
        let index = HnswIndex::build_with_precision(
            HnswParams::default(),
            ResidentPrecision::I8,
            dim as u32,
            &vectors,
            11,
        )
        .unwrap();
        assert_eq!(index.resident_precision(), ResidentPrecision::F32);
        let query = gen_corpus(0x0fa1ab, dim, 1);
        let mut scratch = HnswSearchScratch::default();
        let hits = index.search(&query, 5, 40, &mut scratch).unwrap();
        assert!(!hits.is_empty());
    }

    /// T-I5: I8 常駐索引の `search` が brute-force（`kernel::dot`）対照で
    /// 妥当な Recall@10 を達成すること（クラスタ構造ありフィクスチャ・層 A
    /// 縮小規模。f16 版 T-H5 と同型。127 段の粗い量子化のため f16 より緩い
    /// 閾値を使う——実測値は informational として扱う。詳細な受け入れ基準・
    /// Recall ゲート同一閾値検証は後続 Issue #523 の担当）。
    #[test]
    fn i8_precision_search_achieves_reasonable_recall_against_brute_force() {
        let dim = 16usize;
        let n = 600;
        let mut rng = DeterministicRng::new(0xc121);
        let n_clusters = 6usize;
        let centers: Vec<f32> = (0..n_clusters * dim)
            .map(|_| {
                let bits = rng.next_u64() >> 40;
                ((bits as f32) / (1u32 << 24) as f32) * 2.0 - 1.0
            })
            .collect();
        let mut vectors = Vec::with_capacity(n * dim);
        for i in 0..n {
            let c = i % n_clusters;
            for d in 0..dim {
                let bits = rng.next_u64() >> 40;
                let jitter = ((bits as f32) / (1u32 << 24) as f32) * 0.1 - 0.05;
                vectors.push(centers[c * dim + d] + jitter);
            }
        }

        let params = HnswParams {
            m: 16,
            ef_construction: 100,
            ef_search: 64,
        };
        let index = HnswIndex::build_with_precision(
            params,
            ResidentPrecision::I8,
            dim as u32,
            &vectors,
            11,
        )
        .unwrap();
        assert_eq!(index.resident_precision(), ResidentPrecision::I8);

        let queries = 40;
        let k = 10usize;
        let ef = 64usize;
        let mut scratch = HnswSearchScratch::default();
        let mut recall_hits = 0usize;
        let mut recall_total = 0usize;
        for q in 0..queries {
            let query = gen_corpus(0x0c1b55 + q as u64, dim, 1);
            let mut brute: Vec<ScoredNode> = (0..n as u32)
                .map(|node| ScoredNode {
                    node,
                    score: dot(node_vector(&vectors, dim, node).unwrap(), &query),
                })
                .collect();
            brute.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
            let truth: std::collections::HashSet<u32> =
                brute.iter().take(k).map(|s| s.node).collect();

            let hits = index.search(&query, k, ef, &mut scratch).unwrap();
            recall_total += truth.len();
            recall_hits += hits
                .iter()
                .filter(|h| truth.contains(&(h.id as u32)))
                .count();
        }
        let recall = recall_hits as f64 / recall_total as f64;
        assert!(
            recall >= 0.5,
            "i8 resident recall@{k} too low: {recall} ({recall_hits}/{recall_total})"
        );
    }

    // ------------------------------------------------------------------
    // Issue #449: repair_reachability の到達不能ノード探索・再接続の並列化
    // ------------------------------------------------------------------

    /// `repair_workers_for` の方針（Issue #449）: 到達集合が小さい（既定
    /// `MIN_ROWS_PER_THREAD`=1,024 未満）場合は `threads` の値によらず常に
    /// 1 に縮退し、十分大きい場合は `threads` と実行環境の並列度の小さい方
    /// まで増える。
    #[test]
    fn repair_workers_for_clamps_small_reachable_sets_and_respects_threads_cap() {
        assert_eq!(
            repair_workers_for(400, 12),
            1,
            "MIN_ROWS_PER_THREAD 未満の到達集合は 1 スレッドへ縮退するはず"
        );
        assert_eq!(
            repair_workers_for(400, 1),
            1,
            "threads=1（縮退経路）は常に 1"
        );

        let available = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1);
        let expected = crate::parallel_search::thread_count_for(100_000).min(12);
        assert_eq!(
            repair_workers_for(100_000, 12),
            expected,
            "十分大きい到達集合では available_parallelism と threads の小さい方まで増える"
        );
        if available >= 2 {
            assert!(
                repair_workers_for(100_000, 12) >= 2,
                "この実行環境（available_parallelism={available}）では複数スレッドまで増えるはず"
            );
        }
    }

    /// `nearest_reachable` の並列経路（`workers>=2`）が逐次経路
    /// （`workers==1`）とビット同一の結果を返すことを、実際に並列分岐を通した
    /// 上で固定する（Issue #449「探索の並列化」。非 vacuous 性は
    /// `REPAIR_PARALLEL_LAUNCHES` カウンタで検証する）。
    #[test]
    fn nearest_reachable_matches_across_worker_counts_and_actually_parallelizes() {
        let dim = 8usize;
        let n = 5_000usize;
        let vectors = gen_corpus(0x4E45_4152_4553_5449u64, dim, n);
        // 到達集合は 0 番ノードを除く全ノード（`node` 自身は候補に含めない
        // 既存契約——`repair_reachability_inner` 側で `reachable` は
        // `bfs_reachable_mask` の到達集合から作るため `node` 自身は通常含み
        // 得るが、ここでは機構単体テストのため任意の候補列を渡す）。
        let reachable: Vec<u32> = (1..n as u32).collect();
        let node = 0u32;

        let before = REPAIR_PARALLEL_LAUNCHES.load(std::sync::atomic::Ordering::Relaxed);
        let sequential = nearest_reachable(&vectors, dim, node, &reachable, 1)
            .expect("sequential nearest_reachable must succeed");
        let after_sequential = REPAIR_PARALLEL_LAUNCHES.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            before, after_sequential,
            "workers=1 は並列分岐を通らないはず"
        );

        for workers in [2usize, 4, 8] {
            let parallel = nearest_reachable(&vectors, dim, node, &reachable, workers)
                .expect("parallel nearest_reachable must succeed");
            assert_eq!(
                sequential, parallel,
                "workers={workers} の結果が逐次経路とビット一致しない"
            );
        }
        let after_parallel = REPAIR_PARALLEL_LAUNCHES.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            after_parallel > after_sequential,
            "workers>=2 の呼び出しで並列分岐が実際に起動しているはず（非 vacuous 性の検証）"
        );
    }

    /// 全ノード挿入後・修復前の `GraphBuilder` を組み立てるテスト専用ヘルパ
    /// （Issue #449。`build_inner` の挿入ループと同じ手順を、修復
    /// （`repair_reachability_inner`）呼び出し前で止めて返す。異なる
    /// `threads` で修復した結果を比較するテストが、修復前の同一グラフを
    /// 複数回・決定的に再現するために使う）。
    fn build_unrepaired(
        params: HnswParams,
        dim: usize,
        vectors: &[f32],
        seed: u64,
    ) -> GraphBuilder {
        let n = vectors.len() / dim;
        let mut builder = GraphBuilder {
            params,
            nodes: Vec::with_capacity(n),
            entry_point: None,
        };
        let mut rng = DeterministicRng::new(seed);
        let mut visited = VisitedScratch::default();
        for node_idx in 0..n {
            let level = assign_level(&mut rng, params.m);
            let node_id = node_idx as u32;
            builder
                .insert_node(node_id, level, dim, vectors, &mut visited)
                .expect("insertion must succeed on this deterministic corpus");
        }
        builder
    }

    /// `GraphBuilder`（修復前）を id 昇順 → レベル → 各層のリンク列の順で
    /// FNV-1a 64bit ハッシュへ投入する（`tests/hnsw.rs::
    /// graph_fingerprint_is_stable_across_representation_change` と同じ
    /// 方式。private フィールドへ直接アクセスできる本モジュール内テスト
    /// 限定のヘルパ）。
    fn fingerprint_builder(builder: &GraphBuilder) -> u64 {
        fn fnv1a_update(mut hash: u64, bytes: &[u8]) -> u64 {
            const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
            for &b in bytes {
                hash ^= b as u64;
                hash = hash.wrapping_mul(FNV_PRIME);
            }
            hash
        }
        const FNV_OFFSET_BASIS: u64 = 0xCBF2_9CE4_8422_2325;

        let mut hash = FNV_OFFSET_BASIS;
        hash = fnv1a_update(hash, &(builder.nodes.len() as u64).to_le_bytes());
        hash = fnv1a_update(
            hash,
            &builder
                .entry_point
                .map(|e| e as i64)
                .unwrap_or(-1)
                .to_le_bytes(),
        );
        for node in &builder.nodes {
            hash = fnv1a_update(hash, &(node.level as u64).to_le_bytes());
            for links in &node.links {
                hash = fnv1a_update(hash, &(links.len() as u64).to_le_bytes());
                for &nb in links {
                    hash = fnv1a_update(hash, &nb.to_le_bytes());
                }
            }
        }
        hash
    }

    /// `clusters` 個のクラスタ中心の完全な複製で行を埋める重複ヘビーコーパス
    /// （`tests/hnsw.rs::gen_duplicate_heavy_corpus` と同じ設計意図——完全同点
    /// スコアを誘発しフェーズ 1／フェーズ 2 の双方を確実に発火させる。本
    /// モジュール内テスト専用の独立実装）。
    fn gen_duplicate_heavy_corpus_local(
        seed: u64,
        dim: usize,
        rows: usize,
        clusters: usize,
    ) -> Vec<f32> {
        let mut rng = DeterministicRng::new(seed);
        let centers: Vec<Vec<f32>> = (0..clusters.max(1))
            .map(|_| {
                (0..dim)
                    .map(|_| {
                        let bits = rng.next_u64() >> 40;
                        (bits as f32) / (1u32 << 24) as f32 * 2.0 - 1.0
                    })
                    .collect()
            })
            .collect();
        let mut out = Vec::with_capacity(rows * dim);
        for i in 0..rows {
            out.extend_from_slice(&centers[i % centers.len()]);
        }
        out
    }

    /// `repair_reachability_inner`（Issue #449 の並列化後）が `threads` の値
    /// によらずビット同一のグラフを返すことを、重複ヘビーコーパス（フェーズ
    /// 1・フェーズ 2 双方を確実に発火させる）で固定する end-to-end テスト。
    /// 修復前の `GraphBuilder` は `build_unrepaired` で決定的に再構築し
    /// （`GraphBuilder` は `Clone` を実装しないため、insertion が決定的で
    /// あることを利用して複数回同じグラフを作り直す）、各 `threads` で
    /// 修復した結果を [`fingerprint_builder`] で比較する。
    #[test]
    fn repair_reachability_inner_is_thread_count_invariant_on_duplicate_heavy_graph() {
        let dim = 12usize;
        let rows = 3_000usize;
        let clusters = 6usize;
        let seed = 0x5EED_0449u64;
        let vectors = gen_duplicate_heavy_corpus_local(seed, dim, rows, clusters);
        let params = HnswParams::default().with_m(6).with_ef_construction(32);

        let mut fingerprints = Vec::new();
        for &threads in &[1usize, 2, 4] {
            let mut builder = build_unrepaired(params, dim, &vectors, seed);
            let stats = builder
                .repair_reachability_inner::<true>(dim, &vectors, threads)
                .expect("repair must succeed on this deterministic corpus");
            // 非 vacuous 性: フェーズ 1・フェーズ 2 の双方が実際に発火した
            // ことを確認する（重複ヘビーコーパスが意図どおり同点スコアを
            // 誘発していることの検証）。
            assert!(
                stats.levels.iter().any(|l| l.phase1_iterations > 0),
                "threads={threads}: フェーズ 1 が一度も発火しなかった"
            );
            assert!(
                stats.levels.iter().any(|l| l.phase2_nodes > 0),
                "threads={threads}: フェーズ 2 が一度も発火しなかった"
            );
            fingerprints.push((threads, fingerprint_builder(&builder)));
        }

        let (base_threads, base_fp) = fingerprints[0];
        for &(threads, fp) in &fingerprints[1..] {
            assert_eq!(
                base_fp, fp,
                "threads={base_threads} と threads={threads} でグラフが一致しない"
            );
        }
    }
    // ------------------------------------------------------------------
    // Issue #505: `HnswDenseProvider` 向け再開型探索（`ResumableMaskedSearch`）。
    // §計画「6.1 hnsw.rs 単体」に対応する。
    // ------------------------------------------------------------------

    /// マスク受理ノードに対するブルートフォース Top-k（`dot` 降順・同点は id
    /// 昇順）。`ResumableMaskedSearch` が exhaustive（`candidates` が尽きた
    /// 状態）に達したときの厳密性を確認する対照実装。
    fn brute_force_masked_top_k(
        vectors: &[f32],
        dim: usize,
        query: &[f32],
        mask: Option<&NodeMask>,
        k: usize,
    ) -> Vec<crate::kernel::CandidateHit> {
        let rows = vectors.len() / dim;
        let mut scored: Vec<ScoredNode> = (0..rows)
            .filter(|&i| mask.map(|m| m.get(i as u32)).unwrap_or(true))
            .map(|i| {
                let row = &vectors[i * dim..(i + 1) * dim];
                ScoredNode {
                    node: i as u32,
                    score: dot(row, query),
                }
            })
            .collect();
        scored.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
        scored
            .into_iter()
            .take(k)
            .map(|s| crate::kernel::CandidateHit {
                id: s.node as u64,
                score: s.score,
            })
            .collect()
    }

    #[test]
    fn resumable_start_matches_search_masked_bit_identical() {
        let dim = 8usize;
        let vectors = gen_corpus(31, dim, 300);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 5).unwrap();
        let query = gen_corpus(3001, dim, 1);

        let mut mask = NodeMask::new(300);
        for i in 0..300u32 {
            if i % 3 == 0 {
                mask.set(i);
            }
        }
        // ビット同一性は到達可能性（マスクの連結性）に依存しない——
        // 両経路とも同じ起点解決・同じ層 0 ビーム探索を行うため、マスクが
        // 分断されていてもラウンド 1 の出力は一致するはず。

        for mask_opt in [None, Some(&mask)] {
            for &ef in &[1usize, 10, 40] {
                let mut scratch = HnswSearchScratch::default();
                let expected = index
                    .search_masked_with_hop(
                        &query,
                        10,
                        ef,
                        mask_opt,
                        DEFAULT_SPARSE_VISITED_MAX,
                        HopMode::OneHop,
                        &mut scratch,
                    )
                    .unwrap();
                let (actual, _state) = index
                    .search_masked_resumable_start(&query, 10, ef, mask_opt, HopMode::OneHop)
                    .unwrap();
                assert_eq!(
                    expected,
                    actual,
                    "ef={ef} mask_some={} でラウンド 1 がビット同一でない",
                    mask_opt.is_some()
                );
            }
        }
    }

    /// I8 常駐（Issue #522）で再開型経路が `search_masked_with_hop` と
    /// 異なるビームを辿らないことを固定する（Cursor Bugbot 指摘・PR #619
    /// レビュー）。再開型経路（`search_masked_resumable_start`／
    /// `search_masked_resume`）がクエリの二重量子化
    /// （`hnsw::i8_query::PreparedI8Source`）を使わず `NodeVectors::score`
    /// の毎回デクォンタイズ経路のままだと、I8 索引でだけ両経路のスコア・
    /// 展開順が食い違い、ラウンド 1 の出力がビット同一でなくなり得た
    /// （ラウンド 1 のビット同一は `F32` 版と同じく契約——
    /// `resumable_start_matches_search_masked_bit_identical` 参照。
    /// 2 ラウンド目以降の厳密一致は本フィクスチャでの実測固定であり、
    /// 一般契約として保証されるのは「ラウンド 1」と「exhaustive 終了時」の
    /// みである。§`docs/design/hnsw-hybrid-iterative-scan.md`「決定性契約」節）。
    #[test]
    fn resumable_i8_precision_matches_search_masked_with_hop_bit_identical_across_rounds() {
        let dim = 8usize;
        let vectors = gen_corpus(41, dim, 300);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index =
            HnswIndex::build_with_precision(params, ResidentPrecision::I8, dim as u32, &vectors, 6)
                .unwrap();
        assert_eq!(index.resident_precision(), ResidentPrecision::I8);
        let query = gen_corpus(4001, dim, 1);

        let mut mask = NodeMask::new(300);
        for i in 0..300u32 {
            if i % 3 == 0 {
                mask.set(i);
            }
        }

        for mask_opt in [None, Some(&mask)] {
            // ラウンド 1: `search_masked_resumable_start` が
            // `search_masked_with_hop` とビット同一であること。
            let mut scratch = HnswSearchScratch::default();
            let expected_round1 = index
                .search_masked_with_hop(
                    &query,
                    10,
                    10,
                    mask_opt,
                    DEFAULT_SPARSE_VISITED_MAX,
                    HopMode::OneHop,
                    &mut scratch,
                )
                .unwrap();
            let (actual_round1, mut state) = index
                .search_masked_resumable_start(&query, 10, 10, mask_opt, HopMode::OneHop)
                .unwrap();
            assert_eq!(
                expected_round1,
                actual_round1,
                "I8 常駐でラウンド 1 がビット同一でない（mask_some={}）",
                mask_opt.is_some()
            );

            // ラウンド 2 以降（`search_masked_resume`）も、同じ ef で
            // `search_masked_with_hop` を単発呼び出した結果とビット同一で
            // あること（`resumable_run`／`resumable_offer_entry` の
            // 隣接探索本体が I8 索引でも準備済みクエリを使うことの検証）。
            for &ef in &[20usize, 40] {
                let expected = index
                    .search_masked_with_hop(
                        &query,
                        10,
                        ef,
                        mask_opt,
                        DEFAULT_SPARSE_VISITED_MAX,
                        HopMode::OneHop,
                        &mut scratch,
                    )
                    .unwrap();
                let actual = index
                    .search_masked_resume(&mut state, &query, 10, ef, mask_opt)
                    .unwrap();
                assert_eq!(
                    expected,
                    actual,
                    "I8 常駐で ef={ef} のラウンドがビット同一でない（mask_some={}）",
                    mask_opt.is_some()
                );
            }
        }
    }

    #[test]
    fn resumable_start_rejects_two_hop() {
        let dim = 8usize;
        let vectors = gen_corpus(32, dim, 50);
        let index = HnswIndex::build(HnswParams::default(), dim as u32, &vectors, 5).unwrap();
        let query = gen_corpus(3002, dim, 1);
        let err = index
            .search_masked_resumable_start(&query, 5, 10, None, HopMode::TwoHop)
            .unwrap_err();
        assert!(matches!(err, HnswError::InvalidParams { .. }));
    }

    #[test]
    fn resume_reaches_exhaustive_and_matches_brute_force() {
        let dim = 8usize;
        let rows = 200usize;
        let vectors = gen_corpus(33, dim, rows);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 9).unwrap();
        let query = gen_corpus(3003, dim, 1);

        let mut mask = NodeMask::new(rows);
        for i in 0..rows as u32 {
            if i % 2 == 0 {
                mask.set(i);
            }
        }
        assert!(index.is_mask_fully_reachable(&mask));

        let k = 10usize;
        let (mut hits, mut state) = index
            .search_masked_resumable_start(&query, k, 1, Some(&mask), HopMode::OneHop)
            .unwrap();
        let mut ef = 1usize;
        // 索引ノード数を上限に ef を倍増し続け、候補が尽きるまで再開する。
        for _ in 0..32 {
            if ef >= rows {
                break;
            }
            ef = (ef * 2).min(rows);
            hits = index
                .search_masked_resume(&mut state, &query, k, ef, Some(&mask))
                .unwrap();
        }

        let expected = brute_force_masked_top_k(&vectors, dim, &query, Some(&mask), k);
        assert_eq!(
            hits, expected,
            "ef を索引ノード数まで拡張した exhaustive 探索はブルートフォースと厳密一致するはず"
        );
    }

    /// 設計 doc の星型反例（起点 1 点にのみ全葉が接続するグラフ）で「候補復帰
    /// のみで自己昇格しない」誤実装を検出する回帰テスト（codex-review PR #589
    /// 指摘対応。§計画「6.1」参照）。
    #[test]
    fn resume_star_graph_promotes_discarded_every_round() {
        let dim = 4usize;
        // ノード 0（起点。最高スコア）と葉ノード 1..=7（起点にのみ接続）。
        let mut raw = vec![0f32; 8 * dim];
        // 起点は全方向へ均等な単位ベクトルに近い値を持たせ、葉は起点との
        // dot が単調減少するよう構成する（`score = 10 - node` の等価物と
        // なるよう第 1 成分だけを使う単純な埋め込み）。
        for node in 0..8usize {
            raw[node * dim] = (10 - node) as f32;
        }
        let query = {
            let mut q = vec![0f32; dim];
            q[0] = 1.0;
            q
        };
        let vectors: Arc<[f32]> = raw.clone().into();

        let mut nodes = Vec::new();
        // 起点（node 0）は葉 1..=7 全てへ双方向リンクを持つ星型。
        let leaves: Vec<u32> = (1..8u32).collect();
        nodes.push(Node {
            level: 0,
            links: vec![leaves.clone()],
        });
        for _ in 1..8 {
            nodes.push(Node {
                level: 0,
                links: vec![vec![0u32]],
            });
        }
        let index = index_from_nodes(HnswParams::default(), dim as u32, nodes, Some(0), vectors);

        // k・ef を段階的に増やす（codex-review P2 指摘対応・PR #619）。
        // `ef_eff = ef.max(k)` のため、最終ラウンドと同じ k=8 を初回から
        // 使うと ef_eff が初回から常に 8 となり、discarded に落ちた葉が
        // 一度も生じないまま test が green になり得る（自己昇格からの
        // 復帰欠落という退行を検出できない）。k・ef の双方を小さい値から
        // 段階的に引き上げることで、各ラウンドで新たに discarded から
        // results へ自己昇格するノードが実際に発生する状態を作る。
        let (mut hits, mut state) = index
            .search_masked_resumable_start(&query, 2, 2, None, HopMode::OneHop)
            .unwrap();
        for &(k_round, ef_round) in &[(4usize, 4usize), (6, 6), (8, 8)] {
            hits = index
                .search_masked_resume(&mut state, &query, k_round, ef_round, None)
                .unwrap();
        }
        assert_eq!(
            hits.len(),
            8,
            "自己昇格が働かないと ef=2 で discarded に落ちた葉が最終結果へ戻らない"
        );
        let ids: std::collections::HashSet<u64> = hits.iter().map(|h| h.id).collect();
        assert_eq!(ids.len(), 8, "全ノードが重複なく揃うはず");
    }

    #[test]
    fn resume_expands_each_node_at_most_once() {
        let dim = 8usize;
        let rows = 150usize;
        // 重複ヘビーコーパス（同点誘発）で二重展開が起きないことを確認する。
        let mut vectors = gen_corpus(34, dim, 1);
        vectors = vectors.repeat(rows);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 3).unwrap();
        let query = gen_corpus(3004, dim, 1);

        let k = 5usize;
        let (_hits, mut state) = index
            .search_masked_resumable_start(&query, k, 1, None, HopMode::OneHop)
            .unwrap();
        let mut ef = 1usize;
        for _ in 0..8 {
            ef = (ef * 2).min(rows);
            let _ = index
                .search_masked_resume(&mut state, &query, k, ef, None)
                .unwrap();
        }
        assert!(
            state.candidates.is_empty(),
            "ef を索引ノード数まで拡張すれば candidates は尽きるはず（この式が \
             成り立たないと下の等式は非 vacuous でなくなる）"
        );
        let expanded_count = (0..rows).filter(|&i| state.expanded.is_set(i)).count();
        let in_candidates_count = (0..rows).filter(|&i| state.in_candidates.is_set(i)).count();
        // `candidates` へ一度でも積まれたノード（`in_candidates`）は、
        // `candidates` が尽きた時点で必ず一度は pop され `expanded` が
        // 立っている（`resumable_run` は pop 直後に無条件で `expanded` を
        // 立てる。§ `ResumableMaskedSearch::in_candidates` ドキュメンテーション
        // コメント参照）。
        assert_eq!(
            expanded_count, in_candidates_count,
            "candidates が尽きた時点で「一度でも積まれたノード」と「展開済みノード」は一致するはず"
        );
        // ビットフラグ（`expanded`）は「一度でも立ったか」しか分からず、
        // 同一ノードが `candidates` へ 2 回 push され 2 回展開される二重
        // 展開バグを検出できない（codex-review P2 指摘対応・PR #619）。
        // `expansion_log`（テスト専用の実カウンタ）で「各ノードの実展開
        // 回数」を直接検証し、「各ノード展開は高々 1 回」を実質的に固定する。
        let mut expansion_counts: std::collections::HashMap<u32, usize> =
            std::collections::HashMap::new();
        for &node in &state.expansion_log {
            *expansion_counts.entry(node).or_insert(0) += 1;
        }
        assert_eq!(
            state.expansion_log.len(),
            expanded_count,
            "実展開ログの件数は expanded ビットが立っているノード数と一致するはず"
        );
        assert!(
            expansion_counts.values().all(|&count| count == 1),
            "各ノードの実展開回数は高々 1 回のはず（二重展開が起きていれば \
             expansion_log に同一ノードが複数回記録される）: {expansion_counts:?}"
        );
    }

    #[test]
    fn resume_is_deterministic_across_independent_states() {
        let dim = 8usize;
        let rows = 150usize;
        let mut base = gen_corpus(35, dim, 1);
        base = base.repeat(rows / 3 + 1);
        base.truncate(rows * dim);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &base, 4).unwrap();
        let query = gen_corpus(3005, dim, 1);
        let k = 6usize;

        let run = || {
            let (mut hits, mut state) = index
                .search_masked_resumable_start(&query, k, 1, None, HopMode::OneHop)
                .unwrap();
            let mut ef = 1usize;
            for _ in 0..6 {
                ef = (ef * 2).min(rows);
                hits = index
                    .search_masked_resume(&mut state, &query, k, ef, None)
                    .unwrap();
            }
            hits
        };
        let a = run();
        let b = run();
        assert_eq!(a, b, "同一索引・同一クエリ・同一 ef 列は決定的であるはず");
    }

    #[test]
    fn resume_with_same_ef_extends_prefix() {
        let dim = 8usize;
        let rows = 200usize;
        let vectors = gen_corpus(36, dim, rows);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 20,
        };
        let index = HnswIndex::build(params, dim as u32, &vectors, 6).unwrap();
        let query = gen_corpus(3006, dim, 1);

        let (small, mut state) = index
            .search_masked_resumable_start(&query, 3, 40, None, HopMode::OneHop)
            .unwrap();
        let large = index
            .search_masked_resume(&mut state, &query, 6, 40, None)
            .unwrap();
        assert_eq!(
            &large[..small.len()],
            small.as_slice(),
            "ef 同値で k のみ増やしたラウンドは前ラウンドの前方一致拡張であるはず"
        );
    }

    #[test]
    fn resume_rejects_mismatch_and_restarts() {
        let dim = 8usize;
        let rows = 120usize;
        let vectors = gen_corpus(37, dim, rows);
        let index = HnswIndex::build(HnswParams::default(), dim as u32, &vectors, 8).unwrap();
        let query_a = gen_corpus(3007, dim, 1);
        let query_b = gen_corpus(3008, dim, 1);

        let (_hits, mut state) = index
            .search_masked_resumable_start(&query_a, 5, 10, None, HopMode::OneHop)
            .unwrap();

        // クエリ不一致。
        let err = index
            .search_masked_resume(&mut state, &query_b, 5, 20, None)
            .unwrap_err();
        assert!(matches!(err, HnswError::InvalidParams { .. }));

        // ef 減少（`ef.max(k)` が単調非減少という前提を破る）。
        let err = index
            .search_masked_resume(&mut state, &query_a, 5, 1, None)
            .unwrap_err();
        assert!(matches!(err, HnswError::InvalidParams { .. }));
    }
}
