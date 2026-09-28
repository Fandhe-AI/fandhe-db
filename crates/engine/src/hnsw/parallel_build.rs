//! HNSW 構築の並列化（Issue #406・親 #402・前提 #404/#405。TASK-132・対象
//! ビヘイビア CORE-9・CORE-10）。`super`（`hnsw.rs`）の子モジュールであり、
//! 呼び出し元は [`super::HnswIndex::build_with_threads`]／
//! [`super::HnswIndex::build_parallel`] のみ（本モジュールの関数・型は
//! すべて非公開）。
//!
//! pgvector `hnswbuild.c` のロック粒度設計（隣接リストは要素＝ノード単位
//! ロック、エントリポイント更新のみ排他）・qdrant の逐次プレフィックス方式
//! （孤立成分の発生防止）を参考にした（手法名・ライセンスのみの参照——
//! pgvector: PostgreSQL License／qdrant: Apache-2.0。コード転記はしない）。
//!
//! # ロック不変条件
//!
//! 同時に 2 つ以上のノードロック（[`BuildGraph::links`]）を保持しない。
//! [`BuildGraph::connect`] は `from` のみ、[`BuildGraph::shrink_links`] は
//! 対象ノードのみをロックし、呼び出しごとに取得・解放してから次のロックを
//! 取る（[`insert_node_locked`] 内で `connect(node_id, neighbor)` →
//! `connect(neighbor, node_id)` → `shrink_links(neighbor)` と逐次に呼ぶ）。
//! エントリポイントロック（[`BuildGraph::entry`]）とノードロックも入れ子に
//! しない。これによりデッドロックの可能性を構造的に排除する。
//!
//! # 決定性の範囲
//!
//! レベル割当（[`BuildGraph::levels`]）は並列フェーズ開始前に `seed` から
//! 逐次確定し、以後は読み取り専用（ロック不要）。挿入順序はワークスティール
//! （`AtomicUsize::fetch_add`）による非決定的な順序になるため、構築される
//! グラフの**形状**は `threads >= 2` かつ `n > SEQUENTIAL_PREFIX_NODES` の
//! 場合、同一 `seed` でも run-to-run で変わり得る。[`super::HnswIndex::
//! search`] の決定性契約「同一索引・同一クエリで再現」自体は、構築された
//! グラフがどの形状であっても不変（探索は構築後の固定グラフに対してのみ
//! 動く）。`docs/design/hnsw-parallel-build.md` 参照。
//!
//! # 失敗契約
//!
//! ワーカーの `Err`（`NonFiniteScore` 等）は最初の 1 件を保存し、`AtomicBool`
//! の停止フラグで全ワーカーを早期終了させる。全ハンドルを必ず `join` した
//! うえで判定する（`parallel_search.rs::join_all_or_panicked` と同じ理由。
//! 未 join のハンドルが残ったまま `thread::scope` を抜けると panic が
//! そのまま伝播し fail-closed のエラー変換に到達しない）。ワーカー panic・
//! ロック poison はいずれも [`super::HnswError::WorkerPanicked`] へ変換し、
//! 部分的に結線された索引を `Ok` で返さない。

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard, TryLockError};
use std::time::Instant;

use super::{
    assign_level, compute_shrink, max_degree_for, node_vector, score_of,
    select_neighbors_heuristic_free, DeterministicRng, GraphBuilder, HnswBuildProfile, HnswError,
    HnswIndex, HnswParams, HnswWorkerStats, Node, ResidentPrecision, ScoredNode, VisitedScratch,
};

thread_local! {
    /// 現在のスレッドが [`BuildGraph::read_links`]／[`BuildGraph::write_links`]
    /// へ行った取得試行の内訳（`(blocked, acquired)`。Issue #406 追記:
    /// 8→12 スレッド頭打ち要因の切り分け計測）。`BuildGraph::observe` が
    /// `false` の呼び出し（`build_parallel_graph` が使う非観測 production
    /// 経路）は [`BuildGraph::read_links`]／[`BuildGraph::write_links`] が
    /// この TLS に一切触れない分岐へ入るため、非観測経路には計装の影響
    /// （`try_read`/`try_write` → block の二段化・TLS 書き込み）が及ばない
    /// （レビュー指摘 P1-A の修正。修正前は経路を問わず常に加算していた）。
    /// `observe == true`（[`build_parallel_graph_observed`]）の場合のみ
    /// 加算され、ワーカー終了直前に [`read_worker_lock_stats`] で読む。
    static LINK_LOCK_STATS: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
    /// 現在のスレッドが [`BuildGraph::read_links`]／[`BuildGraph::write_links`]
    /// でブロックする取得（`try_read`/`try_write` の `WouldBlock` 後の
    /// `read()`／`write()`）に落ちたときのみ累積する、ロックが実際に取れる
    /// までの待ち時間（[`HnswWorkerStats::link_lock_wait`] のドキュメンテー
    /// ションコメント参照。codex-review P2 指摘・PR #445）。
    static LINK_LOCK_WAIT: Cell<std::time::Duration> = const { Cell::new(std::time::Duration::ZERO) };
    /// 現在のスレッドが [`BuildGraph::try_promote_entry`] で実際にエントリ
    /// ポイントを更新した回数。`observe == true` の場合のみ加算される
    /// （用途・非観測経路への非影響は上記と同様）。
    static ENTRY_PROMOTION_COUNT: Cell<u64> = const { Cell::new(0) };
    /// 現在のスレッドが [`plan_links`] の層探索で「退化した候補集合」
    /// （`candidates.len() <= 1`）を観測した回数（Issue #448 追記）。並列
    /// フェーズ（`graph.node_count() > SEQUENTIAL_PREFIX_NODES` の挿入）
    /// でのみ加算し、逐次プレフィックス挿入は対象外とする——退化探索は
    /// 挿入中の別ノードへ降下した際に発見可能だが下位層の隣接リストが
    /// まだ空という並列固有の窓（H-N。モジュール冒頭「決定性の範囲」
    /// 節・`docs/design/hnsw-parallel-build.md`「Issue #448 追記」節参照）
    /// を直接示す証拠であり、パス分離（[`plan_links`]／[`publish_links`]）
    /// 導入前後の比較に使う。`observe == true` の場合のみ加算される。
    static DEGENERATE_LAYER_SEARCHES: Cell<u64> = const { Cell::new(0) };
    /// 現在のスレッドが [`ensure_reverse_link`] で実際に再結線を行った回数
    /// （Issue #448 追記）。選択近傍全てから同時 shrink により押し出された
    /// 稀なケースの発火頻度を示す。`observe == true` の場合のみ加算される。
    static REVERSE_LINK_RECONNECTS: Cell<u64> = const { Cell::new(0) };
}

/// [`LINK_LOCK_STATS`] へ 1 回分の取得試行を記録する（`observe == true` の
/// 呼び出し元からのみ呼ばれる）。`blocked`: `try_read`／`try_write` が
/// `WouldBlock` を返しブロックする取得へ落ちたか。poison 検出時（呼び出し元が
/// `Err(HnswError::WorkerPanicked)` を返す分岐）も「取得を試みた」事実として
/// `blocked=false` で記録する（レビュー指摘 P2: 修正前は poison 分岐が未計上
/// で `link_lock_acquired` が過小になり得た）。
fn record_lock_attempt(blocked: bool) {
    LINK_LOCK_STATS.with(|cell| {
        let (blocked_count, acquired_count) = cell.get();
        cell.set((
            blocked_count.saturating_add(u64::from(blocked)),
            acquired_count.saturating_add(1),
        ));
    });
}

/// [`LINK_LOCK_WAIT`] へブロックする取得 1 回分の待ち時間を加算する
/// （`record_lock_attempt(true)` と対になる呼び出し元からのみ呼ばれる）。
fn record_lock_wait(duration: std::time::Duration) {
    LINK_LOCK_WAIT.with(|cell| cell.set(cell.get().saturating_add(duration)));
}

/// [`ENTRY_PROMOTION_COUNT`] を 1 件加算する。
fn record_entry_promotion() {
    ENTRY_PROMOTION_COUNT.with(|cell| cell.set(cell.get().saturating_add(1)));
}

/// 現在のスレッドの累積ロック統計・昇格回数を読む（リセットしない）。
/// [`build_parallel_graph_observed`] のワーカーは `thread::scope` が起動する
/// 新規 OS スレッドであり、呼び出し前の値は常にゼロ（本関数はワーカー終了
/// 直前に一度だけ読む前提。他スレッドの値と混ざらない）。
fn read_worker_lock_stats() -> (u64, u64) {
    LINK_LOCK_STATS.with(|cell| cell.get())
}

/// 現在のスレッドの累積ロック待ち時間を読む（リセットしない。
/// [`read_worker_lock_stats`] と同じ「ワーカー終了直前に一度だけ読む」前提）。
fn read_worker_lock_wait() -> std::time::Duration {
    LINK_LOCK_WAIT.with(|cell| cell.get())
}

fn read_worker_entry_promotions() -> u64 {
    ENTRY_PROMOTION_COUNT.with(|cell| cell.get())
}

/// [`DEGENERATE_LAYER_SEARCHES`] を 1 件加算する（Issue #448 追記）。
fn record_degenerate_layer_search() {
    DEGENERATE_LAYER_SEARCHES.with(|cell| cell.set(cell.get().saturating_add(1)));
}

/// [`REVERSE_LINK_RECONNECTS`] を 1 件加算する（Issue #448 追記）。
fn record_reverse_link_reconnect() {
    REVERSE_LINK_RECONNECTS.with(|cell| cell.set(cell.get().saturating_add(1)));
}

/// 現在のスレッドの累積診断カウンタを読む（Issue #448 追記。
/// [`read_worker_lock_stats`] と同じ「ワーカー終了直前に一度だけ読む」前提）。
fn read_worker_degenerate_layer_searches() -> u64 {
    DEGENERATE_LAYER_SEARCHES.with(|cell| cell.get())
}

fn read_worker_reverse_link_reconnects() -> u64 {
    REVERSE_LINK_RECONNECTS.with(|cell| cell.get())
}

/// [`LINK_LOCK_STATS`]／[`ENTRY_PROMOTION_COUNT`] を「現在この関数を呼んで
/// いるスレッド」についてゼロへ戻す。[`build_parallel_graph_observed`] が
/// 逐次プレフィックス挿入（呼び出し元スレッドで実行される。`observe=true`
/// のため `insert_node_locked` 経由でこの TLS へ書き込む）を始める直前に
/// 一度だけ呼ぶ。
///
/// ワーカースレッドは `thread::scope` が呼び出しのたびに新規生成する OS
/// スレッドであり TLS は常に初期値ゼロから始まるため（[`read_worker_lock_stats`]
/// のドキュメンテーションコメント参照）、現状ワーカー統計へ他スレッド・
/// 他呼び出しの残留値が混入する経路は無い。本関数はその不変条件を
/// 「呼び出し元スレッドが本モジュールの計装付き経路を過去に直接叩いていた
/// 場合でも常に成立する」形で構造的に固定する防御的な処置であり、実際に
/// 混入するバグを修正するものではない。
fn reset_observation_tls() {
    LINK_LOCK_STATS.with(|cell| cell.set((0, 0)));
    LINK_LOCK_WAIT.with(|cell| cell.set(std::time::Duration::ZERO));
    ENTRY_PROMOTION_COUNT.with(|cell| cell.set(0));
    DEGENERATE_LAYER_SEARCHES.with(|cell| cell.set(0));
    REVERSE_LINK_RECONNECTS.with(|cell| cell.set(0));
}

/// 並列構築中の共有グラフ状態。ノード単位 `RwLock` で隣接リストを保護し、
/// エントリポイントのみ別ロックで排他する（モジュール冒頭「ロック
/// 不変条件」）。`levels` は並列フェーズ開始前に確定し以後不変（ロック不要）。
struct BuildGraph {
    dim: usize,
    params: HnswParams,
    vectors: Arc<[f32]>,
    levels: Vec<usize>,
    links: Vec<RwLock<Vec<Vec<u32>>>>,
    entry: RwLock<Option<u32>>,
    /// `true` のとき [`Self::read_links`]／[`Self::write_links`]／
    /// [`Self::try_promote_entry`] が計装（`try_read`/`try_write` → block の
    /// 二段取得・TLS への統計記録）を行う（[`build_parallel_graph_observed`]
    /// が使う）。`false`（[`build_parallel_graph`] が使う production 経路）
    /// では計装を一切経由しない直接 `.read()`／`.write()` 呼び出しになり、
    /// 計装導入前と挙動が完全に同一であることを構造的に保証する
    /// （レビュー指摘 P1-A）。
    observe: bool,
}

impl BuildGraph {
    fn new(
        params: HnswParams,
        dim: usize,
        vectors: Arc<[f32]>,
        levels: Vec<usize>,
        observe: bool,
    ) -> Self {
        let links = levels
            .iter()
            .map(|&level| RwLock::new(vec![Vec::new(); level + 1]))
            .collect();
        Self {
            dim,
            params,
            vectors,
            levels,
            links,
            entry: RwLock::new(None),
            observe,
        }
    }

    fn node_count(&self) -> usize {
        self.levels.len()
    }

    /// ノード `node` の隣接リストへの読み取りロックを取得する。
    ///
    /// `self.observe == false`（[`build_parallel_graph`] の production 経路）
    /// では計装を一切経由せず直接 `.read()` を呼ぶ——計装導入前と完全に
    /// 同一の取得方法・poison 判定になる（レビュー指摘 P1-A）。
    ///
    /// `self.observe == true`（[`build_parallel_graph_observed`]）の場合のみ、
    /// まず `try_read` を試み、成功すればブロックなしの取得として記録し、
    /// `WouldBlock` の場合のみブロックする `read()` へ二段目として落ちる。
    /// この二段化はロック待ち時間の計測精度のためのものであり、production
    /// 経路（`observe == false`）に対して「厳密にロック取得順序・待ち時間が
    /// 同一」であることは主張しない（取得結果・poison 判定のみ同一）。
    fn read_links(&self, node: u32) -> Result<RwLockReadGuard<'_, Vec<Vec<u32>>>, HnswError> {
        let lock = self
            .links
            .get(node as usize)
            .ok_or(HnswError::CapacityOverflow)?;
        if !self.observe {
            return lock.read().map_err(|_| HnswError::WorkerPanicked);
        }
        match lock.try_read() {
            Ok(guard) => {
                record_lock_attempt(false);
                Ok(guard)
            }
            Err(TryLockError::WouldBlock) => {
                record_lock_attempt(true);
                let wait_start = Instant::now();
                let result = lock.read().map_err(|_| HnswError::WorkerPanicked);
                record_lock_wait(wait_start.elapsed());
                result
            }
            Err(TryLockError::Poisoned(_)) => {
                record_lock_attempt(false);
                Err(HnswError::WorkerPanicked)
            }
        }
    }

    /// [`Self::read_links`] の書き込みロック版。`observe` による分岐・
    /// 二段取得の契約は同一。
    fn write_links(&self, node: u32) -> Result<RwLockWriteGuard<'_, Vec<Vec<u32>>>, HnswError> {
        let lock = self
            .links
            .get(node as usize)
            .ok_or(HnswError::CapacityOverflow)?;
        if !self.observe {
            return lock.write().map_err(|_| HnswError::WorkerPanicked);
        }
        match lock.try_write() {
            Ok(guard) => {
                record_lock_attempt(false);
                Ok(guard)
            }
            Err(TryLockError::WouldBlock) => {
                record_lock_attempt(true);
                let wait_start = Instant::now();
                let result = lock.write().map_err(|_| HnswError::WorkerPanicked);
                record_lock_wait(wait_start.elapsed());
                result
            }
            Err(TryLockError::Poisoned(_)) => {
                record_lock_attempt(false);
                Err(HnswError::WorkerPanicked)
            }
        }
    }

    /// 層 `level` におけるノード `node` の隣接 id 集合をロック下で複製して
    /// 返す（読み取りロックの保持時間を最小化するため、スコア計算はロック
    /// 解放後に行う。モジュール冒頭のコメント参照）。
    fn neighbors_copy(&self, level: usize, node: u32) -> Result<Vec<u32>, HnswError> {
        let guard = self.read_links(node)?;
        Ok(guard.get(level).cloned().unwrap_or_default())
    }

    /// 層 `level` における `node` の隣接リストに `to` が含まれるかを、
    /// 複製を作らず読み取りロック内で直接判定する（Issue #448 追記。
    /// [`ensure_reverse_link`] が挿入完了直前の逆方向リンク保証で使う）。
    fn has_link(&self, level: usize, node: u32, to: u32) -> Result<bool, HnswError> {
        let guard = self.read_links(node)?;
        Ok(guard.get(level).is_some_and(|links| links.contains(&to)))
    }

    /// `from -> to` への単方向リンクを層 `level` へ追加する（`from` のみを
    /// ロックする。次数上限の適用は呼び出し元の [`Self::shrink_links`] が担う）。
    fn connect(&self, from: u32, to: u32, level: usize) -> Result<(), HnswError> {
        if from == to {
            return Ok(());
        }
        let mut guard = self.write_links(from)?;
        if let Some(links) = guard.get_mut(level) {
            if !links.contains(&to) {
                links.push(to);
            }
        }
        Ok(())
    }

    /// `node` の隣接リストが次数上限を超えていれば縮退する。書き込みロック
    /// 1 回の中で「現在のリンクを読む → `compute_shrink`（純粋関数。
    /// `hnsw.rs` が [`super::HnswIndex::shrink_links`] とも共有する）で
    /// 再選択を計算 → 書き戻す」を原子的に行うため、複数ワーカーが同じ
    /// ノードを同時に縮退しようとしても競合しない。
    fn shrink_links(&self, node: u32, level: usize, protect: u32) -> Result<(), HnswError> {
        let limit = max_degree_for(&self.params, level);
        let mut guard = self.write_links(node)?;
        let current: Vec<u32> = guard.get(level).cloned().unwrap_or_default();
        if let Some(shrunk) =
            compute_shrink(&current, node, self.dim, &self.vectors, limit, protect)?
        {
            if let Some(links) = guard.get_mut(level) {
                *links = shrunk;
            }
        }
        Ok(())
    }

    /// `level` が現在のエントリレベルより高い場合のみエントリポイントを
    /// 更新する（pgvector 由来の「エントリポイント更新のみ排他」設計。
    /// 書き込みロック内で現在値を再読込してから判定するため、複数ワーカーが
    /// 同時に高いレベルへ昇格しようとしても最終的に最大レベルのノードが残る）。
    fn try_promote_entry(&self, node_id: u32, level: usize) -> Result<(), HnswError> {
        let mut guard = self.entry.write().map_err(|_| HnswError::WorkerPanicked)?;
        let should_update = match *guard {
            Some(ep) => level > self.levels.get(ep as usize).copied().unwrap_or(0),
            None => true,
        };
        if should_update {
            *guard = Some(node_id);
            if self.observe {
                record_entry_promotion();
            }
        }
        Ok(())
    }

    fn entry_snapshot(&self) -> Result<Option<u32>, HnswError> {
        Ok(*self.entry.read().map_err(|_| HnswError::WorkerPanicked)?)
    }
}

/// `ef=1` の貪欲降下（[`super::HnswIndex::greedy_descend`] のロック対応版。
/// 上位層のナビゲーション用。アルゴリズムは同一で、隣接アクセスのみ
/// [`BuildGraph::neighbors_copy`] を介する）。
fn greedy_descend_locked(
    graph: &BuildGraph,
    start: u32,
    query: &[f32],
    level: usize,
) -> Result<u32, HnswError> {
    let mut current = start;
    let mut current_best = ScoredNode {
        node: current,
        score: score_of(&graph.vectors, graph.dim, current, query)?,
    };
    loop {
        let mut improved = false;
        let neighbors = graph.neighbors_copy(level, current)?;
        for cand in neighbors {
            let cand_scored = ScoredNode {
                node: cand,
                score: score_of(&graph.vectors, graph.dim, cand, query)?,
            };
            if cand_scored > current_best {
                current = cand;
                current_best = cand_scored;
                improved = true;
            }
        }
        if !improved {
            break;
        }
    }
    Ok(current)
}

/// [`super::HnswIndex::search_layer`]（Algorithm 2）のロック対応版。アルゴリズム
/// （停止・受理判定を含む順序規約）は完全に同一で、隣接アクセスのみ
/// [`BuildGraph::neighbors_copy`] を介する（`hnsw.rs` モジュール冒頭
/// 「順序規約」節の契約を踏襲する）。
fn search_layer_locked(
    graph: &BuildGraph,
    entry_points: Vec<u32>,
    query: &[f32],
    ef: usize,
    level: usize,
    visited: &mut VisitedScratch,
) -> Result<Vec<ScoredNode>, HnswError> {
    use std::collections::BinaryHeap;

    visited.reset(graph.node_count());
    let mut candidates: BinaryHeap<ScoredNode> = BinaryHeap::new();
    let mut results: BinaryHeap<std::cmp::Reverse<ScoredNode>> = BinaryHeap::new();

    for ep in entry_points {
        match visited.mark_visited(ep as usize) {
            Some(true) => continue,
            Some(false) => {}
            None => continue,
        }
        let score = score_of(&graph.vectors, graph.dim, ep, query)?;
        let scored = ScoredNode { node: ep, score };
        candidates.push(scored);
        results.push(std::cmp::Reverse(scored));
    }

    while let Some(top_candidate) = candidates.pop() {
        if let Some(std::cmp::Reverse(worst)) = results.peek() {
            let strictly_farther =
                top_candidate.score.total_cmp(&worst.score) == std::cmp::Ordering::Less;
            if results.len() >= ef && strictly_farther {
                break;
            }
        }

        let neighbors = graph.neighbors_copy(level, top_candidate.node)?;
        for neighbor in neighbors {
            let already = match visited.mark_visited(neighbor as usize) {
                Some(seen) => seen,
                None => continue,
            };
            if already {
                continue;
            }
            let neighbor_score = score_of(&graph.vectors, graph.dim, neighbor, query)?;
            let scored = ScoredNode {
                node: neighbor,
                score: neighbor_score,
            };
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

    let mut out: Vec<ScoredNode> = results.into_iter().map(|r| r.0).collect();
    out.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
    Ok(out)
}

/// [`plan_links`] の結果（層ごとの選択近傍。`selected_per_level[l]` が層
/// `l` の選択近傍。Issue #448 追記）。
struct PlannedLinks {
    /// index = 層番号（`0..=level.min(top_level)`）。
    selected_per_level: Vec<Vec<u32>>,
    top_level: usize,
    level: usize,
}

/// 1 ノードの挿入を「探索・選択・自ノードの外向きリンク」（パス 1）と
/// 「逆方向リンク・shrink・エントリ昇格」（パス 2・[`publish_links`]）へ
/// 分離した前半（Issue #448 追記。H-N 対策。モジュール冒頭・
/// `docs/design/hnsw-parallel-build.md`「Issue #448 追記」節参照）。
///
/// 本関数の完了時点では `node_id` への逆方向リンクを一切張らないため、
/// 他ワーカーの探索から `node_id` はまだ**発見不能**（entry からの到達路が
/// 無い）。したがって他ワーカーが `node_id` を最近傍として降下してくる
/// 窓（探索側が「発見できるが下位層の隣接リストが空」の退化した候補集合を
/// 引く原因）が構造的に閉じる——`node_id` が発見可能になった時点
/// （[`publish_links`] が層 `l` の逆方向リンクを張った直後）には、
/// `node_id` の層 `l` 以下の外向きリストは既にこの関数で完成済みである。
fn plan_links(
    graph: &BuildGraph,
    node_id: u32,
    visited: &mut VisitedScratch,
) -> Result<PlannedLinks, HnswError> {
    let level = graph
        .levels
        .get(node_id as usize)
        .copied()
        .ok_or(HnswError::CapacityOverflow)?;
    let dim = graph.dim;
    let query = node_vector(&graph.vectors, dim, node_id)?;

    let current_entry = graph.entry_snapshot()?.ok_or(HnswError::WorkerPanicked)?;
    let top_level = graph
        .levels
        .get(current_entry as usize)
        .copied()
        .unwrap_or(0);

    // (i) 上位層を ef=1 の貪欲降下でたどり、挿入ノードのレベル直上での
    // 最近傍 1 件をエントリポイントとして絞り込む。
    let mut nearest = current_entry;
    if top_level > level {
        for l in ((level + 1)..=top_level).rev() {
            nearest = greedy_descend_locked(graph, nearest, query, l)?;
        }
    }

    // (ii) 挿入ノードのレベル以下の各層で ef_construction 幅の探索 →
    // ヒューリスティック近傍選択 → `node_id` 自身の外向きリンクのみを張る
    // （逆方向リンク・shrink は [`publish_links`] へ委ねる）。
    let max_l = level.min(top_level);
    let mut entry_candidates = vec![nearest];
    let mut selected_per_level: Vec<Vec<u32>> = vec![Vec::new(); max_l + 1];
    for l in (0..=max_l).rev() {
        let candidates = search_layer_locked(
            graph,
            entry_candidates.clone(),
            query,
            graph.params.ef_construction,
            l,
            visited,
        )?;
        if graph.observe
            && candidates.len() <= 1
            && graph.node_count() > super::SEQUENTIAL_PREFIX_NODES
        {
            record_degenerate_layer_search();
        }
        let selected =
            select_neighbors_heuristic_free(&candidates, graph.params.m, dim, &graph.vectors)?;

        for &neighbor in &selected {
            graph.connect(node_id, neighbor, l)?;
        }
        selected_per_level[l] = selected;

        entry_candidates = if candidates.is_empty() {
            vec![nearest]
        } else {
            candidates.iter().map(|c| c.node).collect()
        };
    }

    Ok(PlannedLinks {
        selected_per_level,
        top_level,
        level,
    })
}

/// 選択近傍の少なくとも 1 つが層 `level` で `node_id` への逆方向リンクを
/// 保持しているか確認し、いずれも保持していない（同時 `shrink_links` に
/// よる押し出しが全近傍で重なった極めて稀なケース）場合のみ
/// `selected[0]` へ再結線する（Issue #448 追記）。`compute_shrink` の
/// `protect` 契約（`hnsw.rs::compute_shrink` 参照）により、再結線後は
/// 次数上限を維持したまま `node_id` が必ず残る。
fn ensure_reverse_link(
    graph: &BuildGraph,
    node_id: u32,
    level: usize,
    selected: &[u32],
) -> Result<(), HnswError> {
    if selected.is_empty() {
        return Ok(());
    }
    for &sel in selected {
        if graph.has_link(level, sel, node_id)? {
            return Ok(());
        }
    }
    let target = selected[0];
    graph.connect(target, node_id, level)?;
    graph.shrink_links(target, level, node_id)?;
    if graph.observe {
        record_reverse_link_reconnect();
    }
    Ok(())
}

/// [`plan_links`] が確定した選択近傍を実際にグラフへ結線する後半（パス 2。
/// Issue #448 追記）。層を上から下へ処理し、各層で逆方向リンク・
/// `neighbor` 側 shrink・`node_id` 自身の防御的自己 shrink（下記コメント
/// 参照）・逆方向リンク保証（[`ensure_reverse_link`]）を行ったうえで、
/// 最後にエントリポイント昇格を試みる。
fn publish_links(
    graph: &BuildGraph,
    node_id: u32,
    planned: &PlannedLinks,
) -> Result<(), HnswError> {
    for l in (0..planned.selected_per_level.len()).rev() {
        let selected = &planned.selected_per_level[l];
        for &neighbor in selected {
            graph.connect(neighbor, node_id, l)?;
            graph.shrink_links(neighbor, l, node_id)?;
        }

        // `node_id` 自身の層 `l` リストは [`plan_links`] の
        // `select_neighbors_heuristic_free` によって `<= params.m` 本に
        // 収まっている（逐次経路 `insert_node` はこれを前提に自身の縮退を
        // 省略する）。パス分離後は `node_id` への逆方向リンクが本関数の
        // 直前のループでしか張られないため、他ノードの挿入処理が
        // `node_id` を自身の近傍として選び「`node_id` 自身のリストへ
        // 割り込んで追加する」窓（Issue #406 実装時に発見した並列固有の
        // レース。旧 `insert_node_locked` のコメント参照）は構造的に
        // 縮小するが、防御としてこの自己 shrink は維持する。
        // `protect=node_id` は `compute_shrink` の `node != protect` 分岐を
        // 満たさない（`node == protect`）ため強制保護なしの純粋な上位
        // `limit` 件選択として働く。
        graph.shrink_links(node_id, l, node_id)?;

        // 逆方向リンク保証（3.2）: 直前の `shrink_links(neighbor, l,
        // protect=node_id)` は `compute_shrink` の契約により `node_id` を
        // 必ず残すが、それは各 `neighbor` の shrink が完了した「その時点」
        // のみの保証であり、同一 `neighbor` を選んだ別ワーカーの後続
        // shrink（`protect` が別ノード）が重なると押し出され得る。ここで
        // 選択近傍のいずれかが `node_id` を保持していることを最終確認する。
        ensure_reverse_link(graph, node_id, l, selected)?;
    }

    // 挿入ノードのレベルが現行最大層を超える可能性がある場合のみ
    // エントリポイント更新を試みる（実際に更新するかは `try_promote_entry`
    // が書き込みロック内で再読込のうえ判定する。モジュール冒頭参照）。
    if planned.level > planned.top_level {
        graph.try_promote_entry(node_id, planned.level)?;
    }

    Ok(())
}

/// 1 ノードをグラフへ挿入する（[`super::HnswIndex::insert_node`] のロック
/// 対応版。Algorithm 1 相当）。並列フェーズで呼ばれる前提として、エントリ
/// ポイントは常に `Some`（呼び出し元が先に逐次プレフィックスを挿入済み。
/// [`build_parallel_graph`] 参照）であることを要求し、`None` は
/// `WorkerPanicked` として fail-closed に扱う（この分岐へは通常到達しない
/// 防御的経路）。
///
/// [`plan_links`]（探索・選択・自ノードの外向きリンク）→
/// [`publish_links`]（逆方向リンク・shrink・エントリ昇格）の 2 パスに
/// 分離した薄いラッパ（Issue #448 追記。無競合実行では旧・単一パス実装と
/// 完全に同一のグラフを生成することを
/// `plan_then_publish_matches_reference_without_contention` で固定する）。
fn insert_node_locked(
    graph: &BuildGraph,
    node_id: u32,
    visited: &mut VisitedScratch,
) -> Result<(), HnswError> {
    let planned = plan_links(graph, node_id, visited)?;
    publish_links(graph, node_id, &planned)
}

/// [`super::HnswIndex::build_with_threads`] から呼ばれる並列構築の本体。
/// `n > SEQUENTIAL_PREFIX_NODES`・`threads >= 2` であることを呼び出し元が
/// 保証する（`super::HnswIndex::build_with_threads` がそれ以外は
/// [`super::HnswIndex::build`] へ縮退させる）。
///
/// 1. レベル割当を `seed` から逐次確定する（要件 4: スレッド数に依らない
///    決定性）。
/// 2. 先頭 [`super::SEQUENTIAL_PREFIX_NODES`] 件を単一スレッドで挿入する
///    （qdrant 方式。孤立成分の発生を防ぐ）。
/// 3. 残りを `threads` 本のワーカーでワークスティール方式（`AtomicUsize::
///    fetch_add`）で並列挿入する。
/// 4. 凍結（`RwLock::into_inner`）して [`super::HnswIndex`] を組み立て、
///    既存の `repair_reachability`（単一スレッド後始末。`super::HnswIndex::
///    build` と共有）を実行する。
pub(crate) fn build_parallel_graph(
    params: HnswParams,
    precision: ResidentPrecision,
    dim: u32,
    vectors: &[f32],
    seed: u64,
    threads: usize,
    n: usize,
) -> Result<HnswIndex, HnswError> {
    let dim_usize = dim as usize;

    let mut rng = DeterministicRng::new(seed);
    let levels: Vec<usize> = (0..n).map(|_| assign_level(&mut rng, params.m)).collect();

    let owned_vectors: Arc<[f32]> = Arc::from(vectors);
    let graph = BuildGraph::new(params, dim_usize, owned_vectors.clone(), levels, false);

    let prefix_end = super::SEQUENTIAL_PREFIX_NODES.min(n);
    let mut seq_visited = VisitedScratch::default();
    for node_idx in 0..prefix_end {
        let node_id = node_idx as u32;
        if node_idx == 0 {
            // 最初のノードは近傍探索なしで直ちにエントリポイントになる
            // （`super::HnswIndex::insert_node` の `entry_point.is_none()`
            // 分岐と同じ挙動）。
            graph.try_promote_entry(node_id, graph.levels[0])?;
            continue;
        }
        insert_node_locked(&graph, node_id, &mut seq_visited)?;
    }

    let next = AtomicUsize::new(prefix_end);
    let stop = AtomicBool::new(false);
    let first_error: Mutex<Option<HnswError>> = Mutex::new(None);

    // 全ハンドルを必ず join してから判定する（モジュール冒頭「失敗契約」・
    // `parallel_search.rs::join_all_or_panicked` と同じ理由）。
    let any_panicked = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(threads);
        for _ in 0..threads {
            let graph_ref = &graph;
            let next_ref = &next;
            let stop_ref = &stop;
            let first_error_ref = &first_error;
            handles.push(scope.spawn(move || {
                let mut visited = VisitedScratch::default();
                loop {
                    if stop_ref.load(Ordering::SeqCst) {
                        break;
                    }
                    let i = next_ref.fetch_add(1, Ordering::SeqCst);
                    if i >= n {
                        break;
                    }
                    let node_id = i as u32;
                    if let Err(e) = insert_node_locked(graph_ref, node_id, &mut visited) {
                        let mut fe = first_error_ref
                            .lock()
                            .unwrap_or_else(|poison| poison.into_inner());
                        if fe.is_none() {
                            *fe = Some(e);
                        }
                        stop_ref.store(true, Ordering::SeqCst);
                        break;
                    }
                }
            }));
        }
        let mut any_panicked = false;
        for h in handles {
            if h.join().is_err() {
                any_panicked = true;
            }
        }
        any_panicked
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

    freeze(
        graph,
        params,
        precision,
        dim,
        vectors,
        dim_usize,
        owned_vectors,
        threads,
    )
}

/// [`super::HnswIndex::build_with_threads_observed`] から呼ばれる、
/// [`build_parallel_graph`] と段の区切りを揃えた観測版（Issue #406 追記:
/// 8→12 スレッド頭打ち要因の切り分け計測）。
///
/// アルゴリズム本体（レベル割当・逐次プレフィックス挿入・ワークスティール
/// 並列挿入・凍結・修復）は [`build_parallel_graph`] と共有する関数
/// （`insert_node_locked`・`assemble_graph`・`HnswIndex::repair_reachability`）
/// をそのまま呼ぶ。各段の前後に `Instant::now()` を置いて壁時間を記録する点、
/// ワーカー closure の戻り値を [`HnswWorkerStats`] にする点に加えて、
/// `BuildGraph::new` へ `observe=true` を渡すことでノードロック取得を
/// `try_read`/`try_write` → block の二段化にしロック統計を TLS へ記録する
/// （`BuildGraph::observe` のドキュメンテーションコメント参照。レビュー
/// 指摘 P1-A の修正により、この二段化・TLS 記録は本関数からのみ有効になり
/// [`build_parallel_graph`] には一切波及しない）。したがって「アルゴリズム
/// 本体を共有する」とは主張できても、ロック取得順序・待ち時間まで
/// [`build_parallel_graph`] と厳密に同一であるとは主張しない
/// （[`super::HnswIndex::build_with_threads_observed`] のドキュメンテーション
/// コメントも参照）。エラー分岐（`WorkerPanicked` の判定・`first_error` の
/// 伝播）のロジックは完全に同一。
pub(crate) fn build_parallel_graph_observed(
    params: HnswParams,
    precision: ResidentPrecision,
    dim: u32,
    vectors: &[f32],
    seed: u64,
    threads: usize,
    n: usize,
) -> Result<(HnswIndex, HnswBuildProfile), HnswError> {
    // 呼び出し元スレッドの TLS 統計を必ずゼロから始める（`reset_observation_tls`
    // のドキュメンテーションコメント参照。逐次プレフィックス挿入はこの直後に
    // 呼び出し元スレッドで実行される）。
    reset_observation_tls();

    let dim_usize = dim as usize;

    let level_assign_start = Instant::now();
    let mut rng = DeterministicRng::new(seed);
    let levels: Vec<usize> = (0..n).map(|_| assign_level(&mut rng, params.m)).collect();
    let level_assign = level_assign_start.elapsed();

    let owned_vectors: Arc<[f32]> = Arc::from(vectors);
    let graph = BuildGraph::new(params, dim_usize, owned_vectors.clone(), levels, true);

    let prefix_start = Instant::now();
    let prefix_end = super::SEQUENTIAL_PREFIX_NODES.min(n);
    let mut seq_visited = VisitedScratch::default();
    for node_idx in 0..prefix_end {
        let node_id = node_idx as u32;
        if node_idx == 0 {
            graph.try_promote_entry(node_id, graph.levels[0])?;
            continue;
        }
        insert_node_locked(&graph, node_id, &mut seq_visited)?;
    }
    let sequential_prefix = prefix_start.elapsed();

    let next = AtomicUsize::new(prefix_end);
    let stop = AtomicBool::new(false);
    let first_error: Mutex<Option<HnswError>> = Mutex::new(None);

    let parallel_start = Instant::now();
    // 全ハンドルを必ず join してから判定する（[`build_parallel_graph`] と
    // 同一の失敗契約）。各ワーカーはループ全体の壁時間・挿入件数・ロック
    // 統計・エントリ昇格回数を [`HnswWorkerStats`] として戻り値で返す
    // （共有 atomic ではなくワーカーローカルに集計する。モジュール冒頭
    // `LINK_LOCK_STATS`／`ENTRY_PROMOTION_COUNT` のドキュメンテーション
    // コメント参照）。
    let (any_panicked, workers) = std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(threads);
        for _ in 0..threads {
            let graph_ref = &graph;
            let next_ref = &next;
            let stop_ref = &stop;
            let first_error_ref = &first_error;
            handles.push(scope.spawn(move || -> HnswWorkerStats {
                let busy_start = Instant::now();
                let mut visited = VisitedScratch::default();
                let mut inserted_nodes: u64 = 0;
                loop {
                    if stop_ref.load(Ordering::SeqCst) {
                        break;
                    }
                    let i = next_ref.fetch_add(1, Ordering::SeqCst);
                    if i >= n {
                        break;
                    }
                    let node_id = i as u32;
                    match insert_node_locked(graph_ref, node_id, &mut visited) {
                        Ok(()) => inserted_nodes = inserted_nodes.saturating_add(1),
                        Err(e) => {
                            let mut fe = first_error_ref
                                .lock()
                                .unwrap_or_else(|poison| poison.into_inner());
                            if fe.is_none() {
                                *fe = Some(e);
                            }
                            stop_ref.store(true, Ordering::SeqCst);
                            break;
                        }
                    }
                }
                let busy = busy_start.elapsed();
                let (link_lock_blocked, link_lock_acquired) = read_worker_lock_stats();
                let link_lock_wait = read_worker_lock_wait();
                let entry_promotions = read_worker_entry_promotions();
                let degenerate_layer_searches = read_worker_degenerate_layer_searches();
                let reverse_link_reconnects = read_worker_reverse_link_reconnects();
                HnswWorkerStats {
                    inserted_nodes,
                    busy,
                    link_lock_blocked,
                    link_lock_acquired,
                    link_lock_wait,
                    entry_promotions,
                    degenerate_layer_searches,
                    reverse_link_reconnects,
                }
            }));
        }
        let mut any_panicked = false;
        let mut workers = Vec::with_capacity(threads);
        for h in handles {
            match h.join() {
                Ok(stats) => workers.push(stats),
                Err(_) => any_panicked = true,
            }
        }
        (any_panicked, workers)
    });
    let parallel_phase = parallel_start.elapsed();

    if any_panicked {
        return Err(HnswError::WorkerPanicked);
    }
    if let Some(e) = first_error
        .into_inner()
        .unwrap_or_else(|poison| poison.into_inner())
    {
        return Err(e);
    }

    let freeze_start = Instant::now();
    let mut builder = assemble_graph(graph, params)?;
    let freeze = freeze_start.elapsed();

    let repair_start = Instant::now();
    // Issue #447: 観測版 `repair_reachability_observed` を呼び、層別の
    // 到達不能ノード数・反復回数を `profile.repair` へ格納する。外側の
    // `repair_start.elapsed()`（`profile.repair_reachability`。既存
    // フィールド）は従来どおり残す——受け入れ条件「Σ 段別 wall <=
    // repair.wall <= repair_reachability（誤差内）」の比較対象。
    // Issue #449: 修復フェーズの最近傍探索の並列度上限として、呼び出し元
    // （`HnswIndex::build_with_threads_observed`）から引き継いだ構築スレッド
    // 数 `threads` をそのまま渡す（`WorkerBudgetGuard` の追加取得は行わず、
    // 構築全体にわたって保持済みの予算を引き継ぐ契約。`repair_reachability_
    // inner` のドキュメンテーションコメント「探索の並列化」参照）。
    let repair = builder.repair_reachability_observed(dim_usize, vectors, threads)?;
    let repair_reachability = repair_start.elapsed();

    // 平坦化（CSR 化。Issue #494）は常に最終段——`freeze`（構造的な組み立て）
    // ・`repair_reachability`（並列フェーズが生みうる上位層の到達不能ノードの
    // 修復）のいずれも可変長ビルダー表現（`GraphBuilder`）に対する in-place
    // 更新を要するため、両方が完了するまで CSR へは変換しない。
    let flatten_start = Instant::now();
    let index = HnswIndex::freeze_from(builder, dim, owned_vectors, precision)?;
    let flatten = flatten_start.elapsed();

    let profile = HnswBuildProfile {
        level_assign,
        sequential_prefix,
        parallel_phase,
        freeze,
        repair_reachability,
        flatten,
        // `total` は呼び出し元（`HnswIndex::build_with_threads_observed`）が
        // 検証・エラー分岐を含む呼び出し全体で計測し直して埋める。
        total: std::time::Duration::ZERO,
        workers,
        repair,
    };
    Ok((index, profile))
}

/// 並列フェーズ完了後の `BuildGraph` を [`super::HnswIndex`] の内部表現へ
/// 組み立て直す（`repair_reachability` を呼ばない構造的な組み立てのみ。
/// Issue #406 追記で [`build_parallel_graph_observed`] が「凍結」段と
/// 「修復」段の壁時間を分けて計測できるよう [`freeze`] から切り出した——
/// [`freeze`] の外部から見た挙動はこの切り出し前後で変わらない）。
/// 各ノードの `RwLock` を `into_inner` で消費し（`Vec<Node>` への再コピー
/// なし）。
///
/// 戻り値は [`super::HnswIndex`] ではなく可変長ビルダー表現
/// （[`super::GraphBuilder`]。Issue #494）——`repair_reachability`（並列
/// フェーズが生みうる上位層の到達不能ノードを閉じる修復パス）が
/// `connect`／`shrink_links` による可変長 in-place 更新を要するため、
/// CSR（[`super::csr::CsrGraph`]）への平坦化は修復完了後の最終段
/// （[`super::HnswIndex::freeze_from`]）まで行わない。
fn assemble_graph(graph: BuildGraph, params: HnswParams) -> Result<GraphBuilder, HnswError> {
    let entry_point = graph
        .entry
        .into_inner()
        .map_err(|_| HnswError::WorkerPanicked)?;
    let levels = graph.levels;
    let mut nodes: Vec<Node> = Vec::with_capacity(graph.links.len());
    for (idx, lock) in graph.links.into_iter().enumerate() {
        let links = lock.into_inner().map_err(|_| HnswError::WorkerPanicked)?;
        let level = levels.get(idx).copied().unwrap_or(0);
        nodes.push(Node { level, links });
    }

    Ok(GraphBuilder {
        params,
        nodes,
        entry_point,
    })
}

/// 並列フェーズ完了後の `BuildGraph` を [`super::HnswIndex`] へ凍結する
/// ([`assemble_graph`] による構造的な組み立て → 既存の逐次後始末
/// `repair_reachability`〔並列フェーズが生みうる上位層の到達不能ノードを
/// 閉じる。モジュール冒頭「決定性の範囲」節参照〕 → [`super::HnswIndex::
/// freeze_from`] による CSR 平坦化〔Issue #494。常に最終段〕)。
///
/// `threads` は修復フェーズの最近傍探索（Issue #449）の並列度上限として
/// そのまま引き継ぐ（呼び出し元 `build_parallel_graph` が受け取った構築
/// スレッド数と同一値。`WorkerBudgetGuard` の追加取得は行わない——
/// `repair_reachability_inner` のドキュメンテーションコメント「探索の
/// 並列化」参照）。
// Issue #449 で修復フェーズの並列度 `threads` を追加し 8 引数（閾値 7）を
// 超えたが、`hnsw.rs`・`sql/hnsw_cache.rs` の既存関数群と同じ方針で許容する
// （本ファイル内 `#[allow(clippy::too_many_arguments)]` 参照）。
#[allow(clippy::too_many_arguments)]
fn freeze(
    graph: BuildGraph,
    params: HnswParams,
    precision: ResidentPrecision,
    dim: u32,
    original_vectors: &[f32],
    dim_usize: usize,
    owned_vectors: Arc<[f32]>,
    threads: usize,
) -> Result<HnswIndex, HnswError> {
    let mut builder = assemble_graph(graph, params)?;
    builder.repair_reachability(dim_usize, original_vectors, threads)?;
    HnswIndex::freeze_from(builder, dim, owned_vectors, precision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hnsw::{HnswIndex as PubHnswIndex, MAX_BUILD_THREADS};

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

    /// `BuildGraph` のノードロックが poison した場合（ワーカーがロック保持中に
    /// panic した想定）、凍結が `WorkerPanicked` として fail-closed に拒否
    /// されることを確認する（モジュール冒頭「失敗契約」節）。
    #[test]
    fn freeze_reports_worker_panicked_when_a_node_lock_is_poisoned() {
        let params = HnswParams::default();
        let dim = 4usize;
        let vectors: Vec<f32> = vec![0.0; dim * 3];
        let owned: Arc<[f32]> = Arc::from(vectors.as_slice());
        let graph = BuildGraph::new(params, dim, owned.clone(), vec![0, 0, 0], false);

        // 意図的にロック保持中に panic させ poison を作る（`catch_unwind` で
        // テストプロセス自体を落とさずに済ませる）。
        let lock = &graph.links[0];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = lock.write().unwrap();
            panic!("intentional poison for WorkerPanicked test");
        }));
        assert!(result.is_err());
        assert!(lock.is_poisoned());

        let err = freeze(
            graph,
            params,
            ResidentPrecision::F32,
            dim as u32,
            &vectors,
            dim,
            owned,
            1,
        )
        .unwrap_err();
        assert_eq!(err, HnswError::WorkerPanicked);
    }

    #[test]
    fn build_with_threads_one_matches_sequential_build_exactly() {
        let dim = 6usize;
        let rows = 800usize;
        let vectors = gen_corpus(0x1234_5678, dim, rows);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 32,
        };
        let sequential = PubHnswIndex::build(params, dim as u32, &vectors, 7).unwrap();
        let parallel =
            PubHnswIndex::build_with_threads(params, dim as u32, &vectors, 7, 1).unwrap();

        assert_eq!(sequential.entry_point(), parallel.entry_point());
        assert_eq!(sequential.max_level(), parallel.max_level());
        for node in 0..rows as u32 {
            assert_eq!(sequential.level_of(node), parallel.level_of(node));
            let seq_level = sequential.level_of(node).unwrap();
            for level in 0..=seq_level {
                assert_eq!(
                    sequential.neighbors(level, node),
                    parallel.neighbors(level, node),
                    "node={node} level={level}"
                );
            }
        }
    }

    #[test]
    fn build_with_threads_rejects_zero_and_over_max() {
        let params = HnswParams::default();
        let vectors = gen_corpus(1, 4, 10);
        assert!(matches!(
            PubHnswIndex::build_with_threads(params, 4, &vectors, 1, 0).unwrap_err(),
            HnswError::InvalidParams { .. }
        ));
        assert!(matches!(
            PubHnswIndex::build_with_threads(params, 4, &vectors, 1, MAX_BUILD_THREADS + 1)
                .unwrap_err(),
            HnswError::InvalidParams { .. }
        ));
    }

    #[test]
    fn build_with_threads_small_n_matches_sequential_build_regardless_of_threads() {
        // n <= SEQUENTIAL_PREFIX_NODES では並列フェーズが起動せず、常に
        // `build` と同一のグラフになる。
        let dim = 4usize;
        let rows = 100usize; // < SEQUENTIAL_PREFIX_NODES(256)
        let vectors = gen_corpus(0xAAAA, dim, rows);
        let params = HnswParams::default();
        let sequential = PubHnswIndex::build(params, dim as u32, &vectors, 3).unwrap();
        let parallel =
            PubHnswIndex::build_with_threads(params, dim as u32, &vectors, 3, 4).unwrap();
        assert_eq!(sequential.entry_point(), parallel.entry_point());
        for node in 0..rows as u32 {
            assert_eq!(sequential.level_of(node), parallel.level_of(node));
        }
    }

    // --------------------------------------------------
    // Issue #406 レビュー指摘 P1-B: `build_with_threads_observed` の契約テスト
    // （縮退経路の完全一致・並列経路のワーカー統計整合・エラー契約の共有）
    // --------------------------------------------------

    fn normalize(v: &mut [f32]) {
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in v.iter_mut() {
                *x /= norm;
            }
        }
    }

    /// `tests/hnsw_search.rs::gen_clustered_corpus` と同じ発想の、緩い
    /// クラスタ構造を持つ L2 正規化済み決定的コーパス（`src` 内部テストは
    /// crate 内部の `DeterministicRng` のみを使う流儀のため独立に複製する）。
    fn gen_clustered_corpus(seed: u64, dim: usize, rows: usize, clusters: usize) -> Vec<f32> {
        let mut center_rng = DeterministicRng::new(seed ^ 0xC1C1_C1C1_C1C1_C1C1);
        let next_unit = |rng: &mut DeterministicRng| -> f32 {
            let bits = rng.next_u64() >> 40;
            (bits as f32) / (1u32 << 24) as f32 * 2.0 - 1.0
        };
        let centers: Vec<Vec<f32>> = (0..clusters.max(1))
            .map(|_| (0..dim).map(|_| next_unit(&mut center_rng)).collect())
            .collect();
        let mut rng = DeterministicRng::new(seed);
        let mut out = Vec::with_capacity(rows * dim);
        for i in 0..rows {
            let center = &centers[i % centers.len()];
            let mut v: Vec<f32> = center
                .iter()
                .map(|c| c + next_unit(&mut rng) * 0.2)
                .collect();
            normalize(&mut v);
            out.extend(v);
        }
        out
    }

    fn gen_queries(seed: u64, dim: usize, clusters: usize, count: usize) -> Vec<Vec<f32>> {
        let mut center_rng = DeterministicRng::new(seed ^ 0xC1C1_C1C1_C1C1_C1C1);
        let next_unit = |rng: &mut DeterministicRng| -> f32 {
            let bits = rng.next_u64() >> 40;
            (bits as f32) / (1u32 << 24) as f32 * 2.0 - 1.0
        };
        let centers: Vec<Vec<f32>> = (0..clusters.max(1))
            .map(|_| (0..dim).map(|_| next_unit(&mut center_rng)).collect())
            .collect();
        (0..count)
            .map(|i| {
                let mut rng = DeterministicRng::new(seed.wrapping_add(i as u64 + 1));
                let center = &centers[i % centers.len()];
                let mut v: Vec<f32> = center
                    .iter()
                    .map(|c| c + next_unit(&mut rng) * 0.2)
                    .collect();
                normalize(&mut v);
                v
            })
            .collect()
    }

    /// brute-force（`CpuScalarProvider`。production の総当たりカーネル）対照で
    /// Recall@10 を計測する（`tests/hnsw_search.rs::recall_at_10` と同じ発想）。
    fn recall_at_10(
        index: &PubHnswIndex,
        vectors: &[f32],
        dim: usize,
        rows: usize,
        ef: usize,
        queries: &[Vec<f32>],
    ) -> f64 {
        use crate::hnsw::HnswSearchScratch;
        use crate::kernel::{CpuScalarProvider, SearchInput, SearchProvider};
        use std::collections::HashSet;

        let ids: Vec<u64> = (0..rows as u64).collect();
        let provider = CpuScalarProvider;
        let mut scratch = HnswSearchScratch::default();
        let mut hits_total = 0usize;
        for query in queries {
            let brute = provider
                .search(SearchInput {
                    ids: &ids,
                    vectors,
                    dim: dim as u32,
                    query,
                    k: 10,
                })
                .expect("brute-force search must succeed");
            let brute_ids: HashSet<u64> = brute.iter().map(|h| h.id).collect();

            let hnsw = index
                .search(query, 10, ef, &mut scratch)
                .expect("hnsw search must succeed");
            let hit = hnsw.iter().filter(|h| brute_ids.contains(&h.id)).count();
            hits_total += hit;
        }
        hits_total as f64 / (queries.len() as f64 * 10.0)
    }

    /// 縮退経路（`n <= SEQUENTIAL_PREFIX_NODES`）では `build_with_threads_observed`
    /// が `build` と完全に同一のグラフを返し、`workers` が空・`parallel_phase`
    /// がゼロであることを確認する（レビュー指摘 P1-B）。
    #[test]
    fn build_with_threads_observed_degenerate_path_matches_build_exactly() {
        let dim = 4usize;
        let rows = 100usize; // < SEQUENTIAL_PREFIX_NODES(256)
        let vectors = gen_corpus(0xAAAA, dim, rows);
        let params = HnswParams::default();
        let sequential = PubHnswIndex::build(params, dim as u32, &vectors, 3).unwrap();
        let (observed, profile) =
            PubHnswIndex::build_with_threads_observed(params, dim as u32, &vectors, 3, 4).unwrap();

        assert_eq!(sequential.entry_point(), observed.entry_point());
        assert_eq!(sequential.max_level(), observed.max_level());
        for node in 0..rows as u32 {
            assert_eq!(sequential.level_of(node), observed.level_of(node));
            let seq_level = sequential.level_of(node).unwrap();
            for level in 0..=seq_level {
                assert_eq!(
                    sequential.neighbors(level, node),
                    observed.neighbors(level, node),
                    "node={node} level={level}"
                );
            }
        }
        assert!(profile.workers.is_empty());
        assert_eq!(profile.parallel_phase, std::time::Duration::ZERO);
        assert!(profile.sequential_prefix > std::time::Duration::ZERO || rows == 0);
        assert!(profile.total >= profile.sequential_prefix);
    }

    /// 並列経路（`threads >= 2` かつ `n > SEQUENTIAL_PREFIX_NODES`）では、
    /// ワーカー統計の内訳（挿入件数の合計・ロック統計の整合・段別壁時間の
    /// 内訳）が非 vacuous であり、既定エンジン対照 Recall@10 が逐次構築と
    /// 同水準（`tests/hnsw_search.rs::parallel_build_recall_at_10_matches_
    /// sequential_build_within_margin` と同じ `-0.02` マージン）であることを
    /// 確認する（レビュー指摘 P1-B）。
    ///
    /// クエリ集合は 1000 件（Issue #1083・計測条件の是正。`docs/design/
    /// hnsw-parallel-build.md` の「決定性契約」のとおり `threads >= 2` の
    /// 並列構築は挿入順が非決定的で、実スレッドスケジューリングにより
    /// `observed` のグラフ形状が実行のたびに変わり得る。100 件では
    /// クエリ標本のノイズにより Recall@10 の測定値自体が run ごとに
    /// 数 % 揺れ、負荷下では稀に `-0.02` マージンを超えて間欠失敗して
    /// いた（負荷再現・taskset 2 コア制限下 40 試行 × ef 2 種で失敗率
    /// 約 1.6%、最大差分 0.04）。クエリを 1000 件に増やすと同条件・
    /// 同試行回数で失敗 0・最大差分 0.0065 まで縮小した。マージン
    /// `-0.02` 自体・`m`／`ef_construction` 等の構築パラメータは変更
    /// していない——推定量のノイズを減らす計測条件の是正であり、閾値の
    /// 弱体化ではない）。
    #[test]
    fn build_with_threads_observed_parallel_path_reports_consistent_worker_stats() {
        let dim = 16usize;
        let rows = super::super::SEQUENTIAL_PREFIX_NODES + 1_200;
        let clusters = 20usize;
        let vectors = gen_clustered_corpus(0xB0B0_1234, dim, rows, clusters);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 32,
        };
        let seed = 0x1122_3344_5566;
        let threads = 4usize;

        let sequential = PubHnswIndex::build(params, dim as u32, &vectors, seed).unwrap();
        let (observed, profile) =
            PubHnswIndex::build_with_threads_observed(params, dim as u32, &vectors, seed, threads)
                .unwrap();

        assert_eq!(profile.workers.len(), threads);
        let total_inserted: u64 = profile.workers.iter().map(|w| w.inserted_nodes).sum();
        assert_eq!(
            total_inserted,
            (rows - super::super::SEQUENTIAL_PREFIX_NODES) as u64
        );
        for w in &profile.workers {
            assert!(w.link_lock_blocked <= w.link_lock_acquired);
            // ロック待ち時間はワーカーループ全体の壁時間（`busy`）の内訳の
            // 一部であり、これを超えることはない（codex-review P2 指摘・
            // PR #445）。
            assert!(
                w.link_lock_wait <= w.busy,
                "link_lock_wait={:?} busy={:?}",
                w.link_lock_wait,
                w.busy
            );
        }
        let total_acquired: u64 = profile.workers.iter().map(|w| w.link_lock_acquired).sum();
        assert!(total_acquired > 0);
        assert!(
            profile.total
                >= profile.level_assign
                    + profile.sequential_prefix
                    + profile.parallel_phase
                    + profile.freeze
                    + profile.repair_reachability
        );

        let queries = gen_queries(0x51DE_0007, dim, clusters, 1000);
        for ef in [64usize, 256] {
            let seq_recall = recall_at_10(&sequential, &vectors, dim, rows, ef, &queries);
            let obs_recall = recall_at_10(&observed, &vectors, dim, rows, ef, &queries);
            assert!(
                obs_recall >= seq_recall - 0.02,
                "ef={ef} observed Recall@10={obs_recall} must be within 0.02 of \
                 sequential Recall@10={seq_recall}"
            );
        }
    }

    /// `profile.repair`（Issue #447）の段別 wall が呼び出し元の外側計測
    /// （`profile.repair_reachability`）の入れ子区間になっていること、
    /// 層ごとのカウンタが「反復回数は上限を超えない」「フェーズ 2 が
    /// 発火した層は必ずフェーズ 1 が上限まで走っている」等の不変条件を
    /// 満たすことを固定する（受け入れ条件 2）。
    #[test]
    fn build_with_threads_observed_repair_stats_are_consistent_with_stage_wall() {
        let dim = 16usize;
        let rows = super::super::SEQUENTIAL_PREFIX_NODES + 1_200;
        let clusters = 20usize;
        let vectors = gen_clustered_corpus(0xC0FF_EE01, dim, rows, clusters);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 32,
        };
        let seed = 0x7788_99AA_BBCC;
        let threads = 4usize;

        let (observed, profile) =
            PubHnswIndex::build_with_threads_observed(params, dim as u32, &vectors, seed, threads)
                .unwrap();

        let phase_wall_sum: std::time::Duration = profile
            .repair
            .levels
            .iter()
            .map(|l| l.phase1_wall + l.phase2_wall)
            .sum();
        assert!(
            phase_wall_sum <= profile.repair.wall,
            "phase_wall_sum={phase_wall_sum:?} repair.wall={:?}",
            profile.repair.wall
        );
        assert!(
            profile.repair.wall <= profile.repair_reachability,
            "repair.wall={:?} repair_reachability={:?}",
            profile.repair.wall,
            profile.repair_reachability
        );
        // タイマー往復分だけの誤差を許容する（絶対 5ms または相対 10% の
        // 大きい方。CI 高負荷時のフレーキー化を避けるための緩い許容——
        // 計画書「判断・リスク」節参照）。
        let gap = profile
            .repair_reachability
            .saturating_sub(profile.repair.wall);
        let tolerance = std::cmp::max(
            std::time::Duration::from_millis(5),
            profile.repair_reachability / 10,
        );
        assert!(
            gap <= tolerance,
            "gap={gap:?} tolerance={tolerance:?} repair_reachability={:?} repair.wall={:?}",
            profile.repair_reachability,
            profile.repair.wall
        );

        assert_eq!(
            profile.repair.levels.len(),
            observed.max_level().map(|l| l + 1).unwrap_or(0)
        );
        for level_stats in &profile.repair.levels {
            assert!(level_stats.phase1_iterations as usize <= super::super::PRECISE_REPAIR_CAP);
            if level_stats.phase2_nodes > 0 {
                assert!(
                    level_stats.phase1_cap_hit,
                    "level={} phase2_nodes>0 だが phase1_cap_hit=false",
                    level_stats.level
                );
            }
            if level_stats.unreachable_before == 0 {
                assert_eq!(level_stats.phase1_iterations, 0);
                assert_eq!(level_stats.phase2_nodes, 0);
            }
        }
    }

    /// 縮退経路（threads=1 基線。Issue #447）では `profile.repair.levels` が
    /// 非空に埋まる一方、既存フィールド（`repair_reachability`＝ゼロ・
    /// `workers`＝空）の意味は不変であること、グラフが `build` と完全に
    /// 一致することを確認する（設計「縮退経路の扱い」節）。
    #[test]
    fn build_with_threads_observed_degenerate_path_fills_repair_stats_only() {
        let dim = 4usize;
        let rows = 100usize; // < SEQUENTIAL_PREFIX_NODES(256)
        let vectors = gen_corpus(0xD00D, dim, rows);
        let params = HnswParams::default();
        let sequential = PubHnswIndex::build(params, dim as u32, &vectors, 9).unwrap();
        let (observed, profile) =
            PubHnswIndex::build_with_threads_observed(params, dim as u32, &vectors, 9, 4).unwrap();

        assert_eq!(sequential.entry_point(), observed.entry_point());
        for node in 0..rows as u32 {
            assert_eq!(sequential.level_of(node), observed.level_of(node));
        }
        assert!(profile.workers.is_empty());
        assert_eq!(profile.repair_reachability, std::time::Duration::ZERO);
        assert!(!profile.repair.levels.is_empty());
        assert!(profile.repair.wall <= profile.sequential_prefix);
    }

    /// `threads==0`／`threads > MAX_BUILD_THREADS`・非有限値のエラー契約が
    /// `build_with_threads` と同一の variant を返すことを確認する
    /// （レビュー指摘 P1-B）。
    #[test]
    fn build_with_threads_observed_shares_error_contract_with_build_with_threads() {
        let params = HnswParams::default();
        let vectors = gen_corpus(1, 4, 10);
        assert!(matches!(
            PubHnswIndex::build_with_threads_observed(params, 4, &vectors, 1, 0).unwrap_err(),
            HnswError::InvalidParams { .. }
        ));
        assert!(matches!(
            PubHnswIndex::build_with_threads_observed(
                params,
                4,
                &vectors,
                1,
                MAX_BUILD_THREADS + 1
            )
            .unwrap_err(),
            HnswError::InvalidParams { .. }
        ));

        let mut bad_vectors = gen_corpus(2, 4, 300);
        bad_vectors[0] = f32::NAN;
        assert_eq!(
            PubHnswIndex::build_with_threads(params, 4, &bad_vectors, 1, 4).unwrap_err(),
            PubHnswIndex::build_with_threads_observed(params, 4, &bad_vectors, 1, 4).unwrap_err()
        );

        assert_eq!(
            PubHnswIndex::build_with_threads(params, 3, &vectors, 1, 4).unwrap_err(),
            PubHnswIndex::build_with_threads_observed(params, 3, &vectors, 1, 4).unwrap_err()
        );
    }

    // --------------------------------------------------
    // `LINK_LOCK_STATS`／`ENTRY_PROMOTION_COUNT`（thread_local!）が呼び出し間・
    // スレッド間で混入しないことの固定テスト。
    // --------------------------------------------------

    /// `assign_level` を用いて実装（[`build_parallel_graph_observed`]）と同じ
    /// 手順でレベル列を再現し、並列フェーズでのエントリ昇格回数の期待値を
    /// 独立に計算し直す。
    ///
    /// エントリ昇格は「現在のエントリレベルより高いレベルのノードが現れた
    /// ときのみ発生する」ため、逐次プレフィックス終了時点の最大レベルを
    /// `l_prefix` とすると、並列フェーズ側で `l_prefix` を上回る**相異なる**
    /// レベル値の個数が 0 個ならば並列フェーズでの昇格は必ず 0 回、1 個
    /// ならば（その値を持つノードが複数あっても最初の 1 件だけが成功する
    /// ため）必ず 1 回になり、いずれもワークスティールの完了順序に依存
    /// しない絶対値になる（2 個以上だと実際の完了順序に依存し非決定的に
    /// なり得るため、本ヘルパーで事前に fixture 側から除外する）。
    fn expected_entry_promotions(seed: u64, m: usize, n: usize) -> u64 {
        let mut rng = DeterministicRng::new(seed);
        let levels: Vec<usize> = (0..n).map(|_| assign_level(&mut rng, m)).collect();
        let prefix_end = super::super::SEQUENTIAL_PREFIX_NODES.min(n);
        let l_prefix = levels[..prefix_end].iter().copied().max().unwrap_or(0);
        let mut distinct: Vec<usize> = levels[prefix_end..]
            .iter()
            .copied()
            .filter(|&level| level > l_prefix)
            .collect();
        distinct.sort_unstable();
        distinct.dedup();
        distinct.len() as u64
    }

    /// `expected_entry_promotions` が完了順序非依存で確定する（`<= 1`）
    /// fixture を、決定的な seed の探索で見つける。見つからない場合は
    /// テスト fixture 自体の不備として明示的に panic させる
    /// （vacuous pass 防止）。
    fn find_deterministic_promotion_seed(base: u64, m: usize, n: usize) -> (u64, u64) {
        for candidate in 0..500u64 {
            let seed = base ^ candidate;
            let expected = expected_entry_promotions(seed, m, n);
            if expected <= 1 {
                return (seed, expected);
            }
        }
        panic!("failed to find a seed with a deterministic (<=1) parallel-phase promotion count");
    }

    /// 同一スレッドで `build_with_threads_observed` を連続 2 回呼んでも、
    /// 前回呼び出しの残留値がワーカー統計（挿入件数合計・ロック統計・
    /// エントリ昇格回数）へ混入しないことを固定する。
    ///
    /// `Σ entry_promotions` はワークスティールの完了順序に依存し得る値
    /// なので「1 回目 == 2 回目」の比較だけでは非決定性由来の偶然一致とも
    /// 区別できない。そこで `expected_entry_promotions` で完了順序非依存に
    /// 確定する fixture を選び、両回とも**その絶対値**と一致することまで
    /// 検証する（TLS 混入があれば絶対値からずれて検出できる）。
    #[test]
    fn build_with_threads_observed_repeated_calls_do_not_leak_tls_across_calls() {
        let dim = 16usize;
        let rows = super::super::SEQUENTIAL_PREFIX_NODES + 1_200;
        let m = 8usize;
        let params = HnswParams {
            m,
            ef_construction: 40,
            ef_search: 32,
        };
        let threads = 4usize;

        let (seed, expected) = find_deterministic_promotion_seed(0x9E37_79B9_0000_0000u64, m, rows);
        let vectors = gen_clustered_corpus(seed, dim, rows, 20);

        let assert_profile = |profile: &HnswBuildProfile, label: &str| {
            assert_eq!(profile.workers.len(), threads, "{label}: workers.len()");
            let total_inserted: u64 = profile.workers.iter().map(|w| w.inserted_nodes).sum();
            assert_eq!(
                total_inserted,
                (rows - super::super::SEQUENTIAL_PREFIX_NODES) as u64,
                "{label}: Σ inserted_nodes"
            );
            for w in &profile.workers {
                assert!(
                    w.link_lock_blocked <= w.link_lock_acquired,
                    "{label}: link_lock_blocked <= link_lock_acquired"
                );
            }
            let total_promotions: u64 = profile.workers.iter().map(|w| w.entry_promotions).sum();
            assert_eq!(
                total_promotions, expected,
                "{label}: Σ entry_promotions が期待値（完了順序非依存の絶対値）と \
                 一致しない（呼び出し元スレッドの TLS 残値混入の疑い）"
            );
        };

        let (_, profile1) =
            PubHnswIndex::build_with_threads_observed(params, dim as u32, &vectors, seed, threads)
                .unwrap();
        assert_profile(&profile1, "call1");

        let (_, profile2) =
            PubHnswIndex::build_with_threads_observed(params, dim as u32, &vectors, seed, threads)
                .unwrap();
        assert_profile(&profile2, "call2");
    }

    /// 呼び出し元スレッド（このテスト関数自身のスレッド）の TLS 統計を
    /// 意図的に大きな値へ汚してから `build_with_threads_observed` を呼び、
    /// 汚染値がワーカー統計へ一切混入しないことを固定する。
    ///
    /// [`build_with_threads_observed_repeated_calls_do_not_leak_tls_across_calls`]
    /// が「自然に生じる残留（逐次プレフィックス分）が混入しないか」を見るのに
    /// 対し、本テストは「呼び出し元スレッドの TLS に何が残っていても混入しない」
    /// ことを不自然に大きい値で判別しやすくして直接検証する。
    #[test]
    fn build_with_threads_observed_worker_stats_are_unaffected_by_caller_thread_tls_pollution() {
        let dim = 16usize;
        let rows = super::super::SEQUENTIAL_PREFIX_NODES + 1_200;
        let m = 8usize;
        let params = HnswParams {
            m,
            ef_construction: 40,
            ef_search: 32,
        };
        let threads = 4usize;

        let (seed, expected) = find_deterministic_promotion_seed(0x1357_9BDF_0000_0000u64, m, rows);
        let vectors = gen_clustered_corpus(seed, dim, rows, 20);

        // 呼び出し元スレッドの TLS を、ワーカー統計に混ざれば一発で判別できる
        // 桁の値へ汚染する。
        const POLLUTION: u64 = 1_000_000;
        for _ in 0..POLLUTION {
            record_lock_attempt(true);
        }
        for _ in 0..POLLUTION {
            record_entry_promotion();
        }

        let (_, profile) =
            PubHnswIndex::build_with_threads_observed(params, dim as u32, &vectors, seed, threads)
                .unwrap();

        assert_eq!(profile.workers.len(), threads);
        let total_inserted: u64 = profile.workers.iter().map(|w| w.inserted_nodes).sum();
        assert_eq!(
            total_inserted,
            (rows - super::super::SEQUENTIAL_PREFIX_NODES) as u64
        );
        for w in &profile.workers {
            assert!(
                w.link_lock_acquired < POLLUTION,
                "worker link_lock_acquired={} が呼び出し元スレッドの汚染値と \
                 同オーダーになっており混入の疑いがある",
                w.link_lock_acquired
            );
            assert!(w.link_lock_blocked <= w.link_lock_acquired);
        }
        let total_promotions: u64 = profile.workers.iter().map(|w| w.entry_promotions).sum();
        assert_eq!(
            total_promotions, expected,
            "呼び出し元スレッドで汚染した entry_promotions がワーカー統計へ \
             混入した疑い"
        );
    }

    // --------------------------------------------------
    // Issue #448: パス分離（plan_links／publish_links）・逆方向リンク保証
    // --------------------------------------------------

    /// 逐次プレフィックス（単一スレッド）で `insert_node_locked` を使い
    /// `prefix_end` 件挿入した `BuildGraph` を返す（テスト用ヘルパ）。
    fn seeded_graph(
        params: HnswParams,
        dim: usize,
        vectors: &[f32],
        levels: Vec<usize>,
        prefix_end: usize,
    ) -> BuildGraph {
        let owned: Arc<[f32]> = Arc::from(vectors);
        let graph = BuildGraph::new(params, dim, owned, levels, false);
        let mut visited = VisitedScratch::default();
        for node_idx in 0..prefix_end {
            let node_id = node_idx as u32;
            if node_idx == 0 {
                graph.try_promote_entry(node_id, graph.levels[0]).unwrap();
                continue;
            }
            insert_node_locked(&graph, node_id, &mut visited).unwrap();
        }
        graph
    }

    /// H-N（挿入中ノードが発見可能になった時点で下位層の隣接リストが空の窓）
    /// が構造的に閉じていることを固定する: `plan_links` 完了時点で対象
    /// ノードの全層の外向きリストは既に完成している一方、いずれの選択近傍
    /// からも対象ノードへの逆方向リンクはまだ存在しない（＝発見不能）。
    /// `publish_links` 後は各層で少なくとも 1 つの選択近傍が逆方向リンクを
    /// 保持する（逆方向リンク保証。3.2）。
    #[test]
    fn plan_links_completes_outbound_links_before_publish_makes_node_discoverable() {
        let dim = 6usize;
        let rows = 300usize;
        let vectors = gen_corpus(0x9999_1111, dim, rows);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 32,
        };
        let mut rng = DeterministicRng::new(11);
        let levels: Vec<usize> = (0..rows)
            .map(|_| assign_level(&mut rng, params.m))
            .collect();

        let prefix_end = rows - 1;
        let graph = seeded_graph(params, dim, &vectors, levels, prefix_end);

        let z = prefix_end as u32;
        let mut visited = VisitedScratch::default();
        let planned = plan_links(&graph, z, &mut visited).unwrap();
        assert!(!planned.selected_per_level.is_empty());

        for (l, selected) in planned.selected_per_level.iter().enumerate() {
            let out = graph.neighbors_copy(l, z).unwrap();
            assert_eq!(
                &out, selected,
                "layer {l}: plan_links 完了時点で z の外向きリストは既に完成しているはず"
            );
            for &n in selected {
                assert!(
                    !graph.has_link(l, n, z).unwrap(),
                    "layer {l}: publish 前の neighbor {n} は z への逆方向リンクを持ってはいけない"
                );
            }
        }

        publish_links(&graph, z, &planned).unwrap();

        for (l, selected) in planned.selected_per_level.iter().enumerate() {
            if selected.is_empty() {
                continue;
            }
            let has_any = selected.iter().any(|&n| graph.has_link(l, n, z).unwrap());
            assert!(
                has_any,
                "layer {l}: publish_links 後は少なくとも 1 つの選択近傍が z への逆方向リンクを保持するはず"
            );
        }
    }

    /// `plan_links` のみが完了し `publish_links` がまだ行われていないノード
    /// を探索の起点（他ワーカーが降下中に発見したと仮定）にしても、H-N の
    /// 旧窓のように候補集合が退化（`[起点のみ]`）しないことを固定する。
    #[test]
    fn search_from_planned_but_unpublished_node_does_not_degenerate() {
        let dim = 6usize;
        let rows = 300usize;
        let vectors = gen_corpus(0x9999_2222, dim, rows);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 32,
        };
        let mut rng = DeterministicRng::new(13);
        let levels: Vec<usize> = (0..rows)
            .map(|_| assign_level(&mut rng, params.m))
            .collect();

        let prefix_end = rows - 1;
        let graph = seeded_graph(params, dim, &vectors, levels, prefix_end);

        let z = prefix_end as u32;
        let mut visited = VisitedScratch::default();
        let planned = plan_links(&graph, z, &mut visited).unwrap();
        // z 自身のベクトルをクエリに使い、z を起点として層 0 を探索する
        // （他ワーカーが z へ降下してきた状況の代理）。
        let query = node_vector(&graph.vectors, dim, z).unwrap().to_vec();
        let mut search_visited = VisitedScratch::default();
        let results = search_layer_locked(
            &graph,
            vec![z],
            &query,
            params.ef_construction,
            0,
            &mut search_visited,
        )
        .unwrap();
        assert!(
            results.len() > 1,
            "z は plan_links 完了時点で既に層 0 の外向きリンクを持つため、z を起点にした探索は z 単独へ退化しないはず（実際の件数={}）",
            results.len()
        );
        assert!(!planned.selected_per_level[0].is_empty());
    }

    /// `ensure_reverse_link` の再結線分岐: 選択近傍全てから同時 shrink に
    /// より押し出された（極めて稀な）ケースを人為的に再現し、再結線後に
    /// 次数上限を維持したまま z が必ず戻ることを固定する（3.2）。
    #[test]
    fn ensure_reverse_link_reconnects_when_all_selected_links_were_dropped() {
        let dim = 4usize;
        let rows = 300usize;
        let vectors = gen_corpus(0x9999_3333, dim, rows);
        let params = HnswParams {
            m: 8,
            ef_construction: 40,
            ef_search: 32,
        };
        let mut rng = DeterministicRng::new(17);
        let levels: Vec<usize> = (0..rows)
            .map(|_| assign_level(&mut rng, params.m))
            .collect();

        let prefix_end = rows - 1;
        let graph = seeded_graph(params, dim, &vectors, levels, prefix_end);

        let z = prefix_end as u32;
        let mut visited = VisitedScratch::default();
        let planned = plan_links(&graph, z, &mut visited).unwrap();
        publish_links(&graph, z, &planned).unwrap();

        let l = 0usize;
        let selected = &planned.selected_per_level[l];
        assert!(!selected.is_empty());

        // z への逆方向リンクを全ての選択近傍から人為的に取り除く（同時
        // shrink が全近傍で重なった状況を模擬）。
        for &n in selected {
            let mut guard = graph.write_links(n).unwrap();
            if let Some(links) = guard.get_mut(l) {
                links.retain(|&x| x != z);
            }
        }
        for &n in selected {
            assert!(!graph.has_link(l, n, z).unwrap());
        }

        ensure_reverse_link(&graph, z, l, selected).unwrap();

        let has_any = selected.iter().any(|&n| graph.has_link(l, n, z).unwrap());
        assert!(
            has_any,
            "ensure_reverse_link 後は selected[0] が z への逆方向リンクを保持するはず"
        );
        // 次数上限（`max_degree_for`）を超えていないことも確認する。
        let target = selected[0];
        let limit = max_degree_for(&params, l);
        let out = graph.neighbors_copy(l, target).unwrap();
        assert!(
            out.len() <= limit,
            "再結線後も target={target} の層 {l} 隣接リストは次数上限 {limit} 以内のはず（実際={}）",
            out.len()
        );
    }

    /// パス分離（`plan_links`／`publish_links`）と旧・単一パス手順が、
    /// 無競合（単一スレッド）実行では完全に同一のグラフを生成することを
    /// 固定する。
    ///
    /// `build_with_threads(..., threads=1)` は `build_with_threads_impl` の
    /// `threads == 1` 分岐で逐次 `HnswIndex::build`（分離前の単一パス実装）
    /// へ丸ごと委譲するだけであり、`plan_links`／`publish_links`（新実装）
    /// を一切通らない（codex-review・Cursor Bugbot 指摘。PR #596）。本テストは
    /// それを避け、`insert_node_locked`（新実装。内部で `plan_links` →
    /// `publish_links` を呼ぶ）を全ノードに対し単一スレッド・逐次で直接
    /// 呼び出して `BuildGraph` を構築し（呼び出し順は `build_parallel_graph`
    /// の逐次プレフィックス挿入ループと同じ「1 件ずつ挿入完了させてから次へ」
    /// であり、同一スレッド内では競合が発生し得ない）、`freeze`（`assemble_graph`
    /// → `repair_reachability` → `freeze_from`。`build_parallel_graph` と
    /// 共有する後始末）で `HnswIndex` へ組み立てたうえで、逐次
    /// `HnswIndex::build`（`hnsw.rs::insert_node`。分離前の「層ごとに両方向」
    /// 手順を維持したまま）の結果と比較する。レベル割当は両経路とも
    /// `DeterministicRng::new(seed)` → `assign_level` を同じ順序で呼ぶため
    /// 一致する（`build_inner`・`build_parallel_graph` 参照）。
    #[test]
    fn plan_then_publish_matches_sequential_insert_without_contention() {
        let dim = 5usize;
        let rows = 500usize;
        let vectors = gen_corpus(0x2468_1357, dim, rows);
        let params = HnswParams {
            m: 6,
            ef_construction: 24,
            ef_search: 16,
        };
        let seed = 42u64;

        let sequential = PubHnswIndex::build(params, dim as u32, &vectors, seed).unwrap();

        let mut rng = DeterministicRng::new(seed);
        let levels: Vec<usize> = (0..rows)
            .map(|_| assign_level(&mut rng, params.m))
            .collect();
        let owned: Arc<[f32]> = Arc::from(vectors.as_slice());
        let graph = BuildGraph::new(params, dim, owned.clone(), levels, false);
        let mut visited = VisitedScratch::default();
        for node_idx in 0..rows {
            let node_id = node_idx as u32;
            if node_idx == 0 {
                graph.try_promote_entry(node_id, graph.levels[0]).unwrap();
                continue;
            }
            insert_node_locked(&graph, node_id, &mut visited).unwrap();
        }
        let via_new_path = freeze(
            graph,
            params,
            ResidentPrecision::F32,
            dim as u32,
            &vectors,
            dim,
            owned,
            1,
        )
        .unwrap();

        assert_eq!(sequential.entry_point(), via_new_path.entry_point());
        assert_eq!(sequential.max_level(), via_new_path.max_level());
        for node in 0..rows as u32 {
            assert_eq!(sequential.level_of(node), via_new_path.level_of(node));
            let seq_level = sequential.level_of(node).unwrap();
            for level in 0..=seq_level {
                assert_eq!(
                    sequential.neighbors(level, node),
                    via_new_path.neighbors(level, node),
                    "node={node} level={level}"
                );
            }
        }
    }

    /// 導入後の並列構築（threads=12・数万点規模）で `degenerate_layer_searches`
    /// が到達不能ノード増分と同程度以下の低水準に収まることを実測する
    /// informational テスト（Issue #448。`#[ignore]`・手動専用。導入前との
    /// 定量的な前後比較〔`docs/design/hnsw-parallel-build.md`「Issue #447
    /// 追記」節の基線: threads=12 で層 0 到達不能ノード増分 ≈16〜18〕は
    /// `make bench-hnsw-parallel-build` による運用者実測へ申し送る）。
    #[test]
    #[ignore]
    fn observed_build_at_high_thread_count_reports_low_degenerate_layer_searches() {
        let dim = 32usize;
        let rows = super::super::SEQUENTIAL_PREFIX_NODES + 20_000;
        let vectors = gen_clustered_corpus(0x448_0001, dim, rows, 40);
        let params = HnswParams::default();
        let seed = 0x448_1234_5678;
        let threads = 12usize;

        let (_, profile) =
            PubHnswIndex::build_with_threads_observed(params, dim as u32, &vectors, seed, threads)
                .unwrap();

        let total_degenerate: u64 = profile
            .workers
            .iter()
            .map(|w| w.degenerate_layer_searches)
            .sum();
        let total_reconnects: u64 = profile
            .workers
            .iter()
            .map(|w| w.reverse_link_reconnects)
            .sum();
        println!(
            "hnsw_parallel_build issue448: threads={threads} rows={rows} \
             degenerate_layer_searches={total_degenerate} reverse_link_reconnects={total_reconnects} \
             repair_unreachable_sum={}",
            profile
                .repair
                .levels
                .iter()
                .map(|l| l.unreachable_before)
                .sum::<u64>()
        );
    }
}
