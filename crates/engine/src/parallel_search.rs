//! マルチスレッド並列の総当たり Top-k 検索 provider（TASK-126・対象
//! ビヘイビア: CORE-3, CORE-4, CORE-5・SEARCH-4）。本 provider が担うのは行範囲分割に
//! よる並列化のみで、ベクトル化（SIMD）演算は行わない（下記の経緯参照）。
//!
//! `core.rs` の [`crate::core::EngineCore::open`] からは `search_engine.rs::default_engine`
//! （TASK-131・CORE-9）経由で `kernel.rs::SearchProvider` の既定実装として選択・注入される
//! （`core.rs` が可視行のみに縮約した [`crate::kernel::SearchInput`] を受け取る構造は
//! 不変。可視性判定は本 provider の責務外）。エラー契約・入力検証は `kernel.rs::CpuScalarProvider` と同一
//! （[`crate::kernel::KernelError`] を共用し、`core.rs` 側の Top-k 契約検証とも整合）。
//!
//! 依存最小方針（`.claude/rules/dependency-policy.md`）に従い新規クレートは追加しない。
//! 内積計算は `kernel.rs::dot`（スカラー参照実装）をそのまま呼び出す。以前は
//! `chunks_exact(8)` ＋複数アキュムレータの自前ベクトル化を持っていたが、
//! `dim >= 16` で加算順序がスカラー参照実装と分岐し、丸め誤差により
//! `CpuScalarProvider` と Top-k の集合・順序が食い違い得る不変条件違反があった
//! （Issue #34 レビュー指摘対応）。単一の加算順序を構造的に保証するため
//! `dot` を共有する形へ変更し、並列化のみを本 provider の役割とする
//! （`std::thread::scope`（stable）による行範囲分割。外部からのスレッド数・
//! カーネル選択の上書き機構は設けない。CORE-12 の方針。実行経路自体の決定表は
//! `dispatch.rs::select_execution_path`（TASK-155・CORE-11, 12）に集約する）。
//!
//! 同時実行クエリ間の合計ワーカースレッド数は [`GLOBAL_WORKER_BUDGET`]（プロセス全体で
//! 共有する `AtomicUsize`）で調停する（Issue #34 レビュー指摘対応。security.md
//! 「不安全な設計｜無制限リソース確保（DoS）」）。予算を確保できない分は
//! スレッドを追加せず単一スレッド相当まで縮退させるのみで、行の選出対象からの除外は
//! 一切発生しない（[`ParallelSearchProvider::search`] 参照）。
//!
//! `thread_count_for`・[`WorkerBudgetGuard`]・[`MAX_THREADS_PER_QUERY`] は
//! `pub(crate)` として `hnsw::parallel_build`（HNSW 構築の並列化。Issue #406）
//! からも共有する——検索・構築の双方で「並列度の決め方」「プロセス全体の
//! ワーカー予算調停」を単一の実装に統一するため（構築側は `MAX_BUILD_THREADS`
//! という別名の定数で同値を持つ）。本モジュール自身の並列検索の挙動・
//! エラー契約は変わらない。

use crate::kernel::{
    CandidateHit, KernelError, SearchInput, SearchProvider, SubsetSearchInput, TopKSelector,
};
use std::sync::atomic::{AtomicUsize, Ordering};

/// クエリ 1 件あたりのスレッド数上限（CORE-3 の並列度の趣旨に対応）。
pub(crate) const MAX_THREADS_PER_QUERY: usize = 16;

/// プロセス全体で共有するワーカースレッド予算。`MAX_THREADS_PER_QUERY` はクエリ単独の
/// 上限に過ぎず、同時実行クエリの数だけスレッド総数が積み上がり得るため、
/// 追加で確保する（呼び出し元スレッド自身の分を除く）ワーカー数をこの上限まで
/// プロセス全体で調停する。
const MAX_TOTAL_EXTRA_WORKER_THREADS: usize = 64;

/// [`MAX_TOTAL_EXTRA_WORKER_THREADS`] を上限に、現在確保中の追加ワーカー数を
/// プロセス全体で共有するカウンタ（`Ordering::SeqCst` で十分に保守的に同期する。
/// 検索のホットパスではなく調停用のカウンタのため、性能より単純さを優先する）。
static GLOBAL_WORKER_BUDGET: AtomicUsize = AtomicUsize::new(0);

/// [`GLOBAL_WORKER_BUDGET`] から `desired` 件までの追加ワーカー枠を確保し、確保できた
/// 件数（0 件の場合もあり得る）を返す `RAII` ガード。ガードの `Drop` で必ず解放するため、
/// 途中で `?` によるアーリーリターンやワーカー panic が起きても予算がリークしない。
pub(crate) struct WorkerBudgetGuard(usize);

impl WorkerBudgetGuard {
    pub(crate) fn acquire(desired: usize) -> Self {
        let mut reserved = 0usize;
        let updated = GLOBAL_WORKER_BUDGET.try_update(Ordering::SeqCst, Ordering::SeqCst, |cur| {
            let available = MAX_TOTAL_EXTRA_WORKER_THREADS.saturating_sub(cur);
            reserved = desired.min(available);
            Some(cur.saturating_add(reserved))
        });
        // 更新が成立した場合のみ確保済みとして扱う（fail-closed）。クロージャは常に Some を
        // 返すため通常は Ok だが、万一 Err なら未確保の枠を Drop で減算しないよう 0 件にする。
        match updated {
            Ok(_) => Self(reserved),
            Err(_) => Self(0),
        }
    }

    pub(crate) fn granted(&self) -> usize {
        self.0
    }
}

impl Drop for WorkerBudgetGuard {
    fn drop(&mut self) {
        GLOBAL_WORKER_BUDGET.fetch_sub(self.0, Ordering::SeqCst);
    }
}

/// スレッド分割の下限行数。担当行数がこれを下回るワーカーを作らないことで、
/// 小規模テーブルでの無用なスレッド生成コストを避ける（CORE-3 の「行数が小さい場合は
/// 単一スレッドへ縮退」という設計判断に対応）。
const MIN_ROWS_PER_THREAD: usize = 1024;

/// マルチスレッド並列の総当たり Top-k provider（TASK-126）。行範囲分割による
/// 並列化のみを行い、ベクトル化（SIMD）演算は実装しない（モジュール冒頭コメント参照）。
///
/// 総当たり（exhaustive）である点は [`crate::kernel::CpuScalarProvider`] と同じで、
/// 近似検索ではない。内積計算は `kernel.rs::dot` を共有するため、選出される Top-k
/// 集合・順序（同点タイブレーク含む）はスコア値も含めてスカラー参照実装と bit 単位で
/// 一致する（`crates/engine/tests/parallel_search.rs` で検証）。
#[derive(Debug, Default, Clone, Copy)]
pub struct ParallelSearchProvider;

impl SearchProvider for ParallelSearchProvider {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        let dim = input.dim as usize;
        if input.query.len() != dim {
            return Err(KernelError::DimMismatch {
                expected: input.dim,
                found: input.query.len(),
            });
        }
        // untrusted なクエリ入力の拒否契約は `CpuScalarProvider` と同一
        // （`kernel.rs::CpuScalarProvider::search` のコメント参照）。
        if input.query.iter().any(|v| !v.is_finite()) {
            return Err(KernelError::NonFiniteQuery);
        }
        if input.k == 0 || input.ids.is_empty() {
            return Ok(Vec::new());
        }

        let row_count = input.ids.len();
        let (effective_threads, _budget_guard) = acquire_execution_threads(row_count);

        let partials: Vec<TopKSelector> =
            if effective_threads <= 1 {
                vec![search_range(
                    input.ids,
                    input.vectors,
                    0,
                    dim,
                    input.query,
                    input.k,
                )]
            } else {
                // 行範囲を均等分割し、各スレッドが担当範囲だけで部分 Top-k を選出する。
                // `TopKSelector` は事前確保をせず push 時に自然成長するため、中間バッファは
                // 「実際に保持する要素数が高々 k」という意味で有界（無制限 `with_capacity`
                // 禁止。coding-rust.md・`kernel.rs::TopKSelector::new` 参照）。
                //
                // `ids`・`vectors` はどちらも切り出さずスレッドへ丸ごと渡し、各ワーカーは
                // 担当範囲の絶対行インデックス（`row_start` を起点とする）で `vectors` を
                // 参照する（`search_range` 参照）。以前は `vectors.get(vec_start..vec_end)`
                // が `None` の場合に担当範囲全体（数千行規模になり得る）を空スライスへ縮退させ、
                // `CpuScalarProvider`（壊れた行 1 件だけを skip）と選出結果が食い違う
                // バグがあった（Issue #34 レビュー指摘対応）。行単位の `get()` 失敗のみを
                // skip する現在の形は、スレッド分割の有無・分割数に依らず
                // `CpuScalarProvider` と同じ候補集合を走査することを保証する。
                let rows_per_thread = row_count.div_ceil(effective_threads);
                std::thread::scope(|scope| {
                    let mut handles = Vec::with_capacity(effective_threads);
                    let mut row_start = 0usize;
                    while row_start < row_count {
                        let row_end = row_start.saturating_add(rows_per_thread).min(row_count);
                        // `ids` の担当範囲は `row_start..row_end ⊆ 0..row_count == ids.len()` と
                        // なるよう構築しているため常に `Some` になるが、添字アクセスによる
                        // panic を避けるため `get()` で防御的に取り出す。
                        let ids_slice = input.ids.get(row_start..row_end).unwrap_or(&[]);
                        let vectors = input.vectors;
                        let query = input.query;
                        let k = input.k;
                        handles.push(scope.spawn(move || {
                            search_range(ids_slice, vectors, row_start, dim, query, k)
                        }));
                        row_start = row_end;
                    }
                    join_all_or_panicked(handles)
                })?
            };

        // 部分結果を最終セレクタへ再投入してマージする。`TopKSelector` は
        // `kernel.rs::CpuScalarProvider` と共通の選出規約（スコア降順・同点 id
        // 昇順・非有限値除外）を使うため、分割数・スレッド数に依らず選出集合・
        // 順序が決定的になる（CORE-3・SEARCH-4）。
        let mut merged = TopKSelector::new(input.k);
        for partial in partials {
            for hit in partial.into_sorted_vec() {
                merged.push(hit);
            }
        }
        Ok(merged.into_sorted_vec())
    }

    /// 行列全体（`input.vectors`）を複製せず借用したまま、`input.slots` が指す行
    /// だけを候補にして並列 Top-k を選出する（Issue #654）。並列度の決定・
    /// ワーカー予算調停・部分結果マージは [`SearchProvider::search`] と共通の
    /// 骨格（[`acquire_execution_threads`]）を共有し、範囲分割の代わりに
    /// `slots` をワーカー数分に均等分割して割り当てる（各ワーカーは自分の担当
    /// 範囲の `slots` サブスライスに対して [`search_range_by_slots`] を呼ぶ）。
    fn search_subset(
        &self,
        input: SubsetSearchInput<'_>,
    ) -> Result<Vec<CandidateHit>, KernelError> {
        let dim = input.dim as usize;
        if input.query.len() != dim {
            return Err(KernelError::DimMismatch {
                expected: input.dim,
                found: input.query.len(),
            });
        }
        if input.query.iter().any(|v| !v.is_finite()) {
            return Err(KernelError::NonFiniteQuery);
        }
        if input.k == 0 || input.slots.is_empty() {
            return Ok(Vec::new());
        }

        let slot_count = input.slots.len();
        let (effective_threads, _budget_guard) = acquire_execution_threads(slot_count);

        let partials: Vec<TopKSelector> =
            if effective_threads <= 1 {
                vec![search_range_by_slots(
                    input.slots,
                    input.vectors,
                    dim,
                    input.query,
                    input.k,
                )]
            } else {
                // `search` の並列経路（行範囲の均等分割）と同型だが、分割対象が
                // 「絶対行インデックス範囲」ではなく「`slots` 配列そのもの」である
                // 点のみが異なる（`vectors` はどのワーカーへも切り出さず丸ごと渡す）。
                let slots_per_thread = slot_count.div_ceil(effective_threads);
                std::thread::scope(|scope| {
                    let mut handles = Vec::with_capacity(effective_threads);
                    let mut start = 0usize;
                    while start < slot_count {
                        let end = start.saturating_add(slots_per_thread).min(slot_count);
                        let slots_slice = input.slots.get(start..end).unwrap_or(&[]);
                        let vectors = input.vectors;
                        let query = input.query;
                        let k = input.k;
                        handles.push(scope.spawn(move || {
                            search_range_by_slots(slots_slice, vectors, dim, query, k)
                        }));
                        start = end;
                    }
                    join_all_or_panicked(handles)
                })?
            };

        let mut merged = TopKSelector::new(input.k);
        for partial in partials {
            for hit in partial.into_sorted_vec() {
                merged.push(hit);
            }
        }
        Ok(merged.into_sorted_vec())
    }
}

/// [`ParallelSearchProvider::search`]・[`ParallelSearchProvider::search_subset`]
/// が共有する並列度決定・ワーカー予算確保の骨格。`row_count`（`search` は行数、
/// `search_subset` は候補スロット数）に対して実際に使う並列度
/// （`effective_threads`）と、そのスレッド数ぶんのグローバル予算
/// （[`GLOBAL_WORKER_BUDGET`]）を確保した `WorkerBudgetGuard`（並列化しない場合は
/// `None`）を返す。ガードは戻り値の生存期間中（呼び出し元の検索が終わるまで）
/// 予算を保持し続け、`Drop` で自動的に解放される。
fn acquire_execution_threads(row_count: usize) -> (usize, Option<WorkerBudgetGuard>) {
    let desired_threads = thread_count_for(row_count);
    // `desired_threads <= 1` の場合は追加ワーカーを 1 つも生成しないため、
    // グローバル予算を消費せず単一スレッド経路をそのまま使う。`desired_threads > 1`
    // の場合のみ、これから生成する追加ワーカー数分の予算確保を試みる。確保できた
    // 数（`effective_threads`）が同時実行クエリの多さにより `desired_threads` を
    // 下回ることがあるが、その場合はパーティション数を単に減らすだけで、行を
    // 選出対象から除外することは一切ない（DoS 対策の縮退は「並列度を落とす」形
    // でのみ行い、fail-open にはしない）。
    if desired_threads <= 1 {
        return (1usize, None);
    }
    let guard = WorkerBudgetGuard::acquire(desired_threads);
    let granted = guard.granted();
    if granted <= 1 {
        // 確保できた枠が 1 以下だと実際には並列ワーカーを 1 つも起動しない。
        // その場合にガードだけを生存させ続けるとグローバル予算のスロットを
        // 検索終了まで無駄に占有し、他の同時実行クエリを不必要に飢餓状態に
        // する（Cursor Bugbot 指摘対応）。ここで即座に解放する。
        drop(guard);
        (1usize, None)
    } else {
        (granted, Some(guard))
    }
}

/// `slots`（`vectors` への行インデックス。担当範囲だけに絞り込み済み。狭義昇順・
/// 重複なしは呼び出し元契約）に対して総当たり Top-k を選出する
/// （[`ParallelSearchProvider::search_subset`] の単一スレッド経路・並列ワーカーの
/// 両方から呼ばれる共通処理。[`search_range`] の「担当範囲」が絶対行インデックス
/// レンジではなく `slots` 配列である版）。返る [`CandidateHit::id`] は
/// `slot as u64`（[`SubsetSearchInput`] のドキュメント参照）。
fn search_range_by_slots(
    slots: &[u32],
    vectors: &[f32],
    dim: usize,
    query: &[f32],
    k: usize,
) -> TopKSelector {
    let mut selector = TopKSelector::new(k);

    // Issue #510 の 4 行ブロックカーネル化と同じ理由で、4 スロット単位で
    // `dot_block4` へディスパッチする（端数・ブロック内の行欠損は 1 行ずつの
    // [`push_row`] へフォールバック）。
    let (blocks, remainder) = slots.as_chunks::<4>();
    for block in blocks {
        let row_slice = |slot: u32| -> Option<&[f32]> {
            let start = (slot as usize).saturating_mul(dim);
            let end = start.saturating_add(dim);
            vectors.get(start..end)
        };
        let [s0, s1, s2, s3] = *block;
        let (r0, r1, r2, r3) = (row_slice(s0), row_slice(s1), row_slice(s2), row_slice(s3));

        match (r0, r1, r2, r3) {
            (Some(v0), Some(v1), Some(v2), Some(v3)) => {
                let scores = crate::kernel::dot_block4([v0, v1, v2, v3], query);
                let ids4 = [s0 as u64, s1 as u64, s2 as u64, s3 as u64];
                for (id, score) in ids4.into_iter().zip(scores) {
                    push_score(&mut selector, id, score);
                }
            }
            _ => {
                push_row(&mut selector, s0 as u64, r0, query);
                push_row(&mut selector, s1 as u64, r1, query);
                push_row(&mut selector, s2 as u64, r2, query);
                push_row(&mut selector, s3 as u64, r3, query);
            }
        }
    }

    for &slot in remainder {
        let start = (slot as usize).saturating_mul(dim);
        let end = start.saturating_add(dim);
        push_row(&mut selector, slot as u64, vectors.get(start..end), query);
    }

    selector
}

/// `std::thread::scope` 配下で spawn した全ハンドルを必ず `join()` してから結果をまとめる。
/// [`ParallelSearchProvider::search`] の並列経路から呼ばれる。
///
/// `collect::<Result<_, _>>()` を直接使うと最初の `Err`（panic）で早期リターンし、
/// 残りのハンドルが `join()` されないまま関数を抜ける。`std::thread::scope` は
/// 未 `join` のハンドルが残っているとスコープ終了時に自らそのハンドルを `join()` し、
/// それが panic であれば `scope` 自体が再 panic する。その場合
/// `KernelError::WorkerPanicked` へのマッピング（fail-closed）に到達せず、呼び出し元まで
/// panic が伝播してしまう（Cursor Bugbot 指摘対応）。本関数は先に全ハンドルを `join()` し
/// 尽くしてから判定することでこれを避ける。
fn join_all_or_panicked(
    handles: Vec<std::thread::ScopedJoinHandle<'_, TopKSelector>>,
) -> Result<Vec<TopKSelector>, KernelError> {
    let join_results: Vec<_> = handles.into_iter().map(|h| h.join()).collect();
    let mut partials = Vec::with_capacity(join_results.len());
    let mut any_panicked = false;
    for result in join_results {
        match result {
            Ok(partial) => partials.push(partial),
            Err(_) => any_panicked = true,
        }
    }
    if any_panicked {
        return Err(KernelError::WorkerPanicked);
    }
    Ok(partials)
}

/// 利用可能な並列度を [`MAX_THREADS_PER_QUERY`] でクランプし、担当行数が
/// [`MIN_ROWS_PER_THREAD`] を割り込まない範囲に収める。
pub(crate) fn thread_count_for(row_count: usize) -> usize {
    let available = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(MAX_THREADS_PER_QUERY);
    let by_rows = row_count / MIN_ROWS_PER_THREAD;
    available.min(by_rows).max(1)
}

/// [`ParallelSearchProvider::search`] が `row_count` 件のテーブルに対して実際に
/// 起動しようとするワーカースレッド数（[`GLOBAL_WORKER_BUDGET`] による同時実行
/// クエリ間の縮退は考慮しない上限値）を呼び出し元へ公開する。`thread_count_for` は
/// 非 `pub` のため、ベンチ・スモークテストなど本クレート外から「この行数・この
/// 実行環境で実際に複数スレッドへ分割されるか」を事前に判定したい呼び出し元
/// （`benches/simd_bench.rs` の診断 A/B 等。Cursor Bugbot 指摘・PR #222）向けの
/// 薄いラッパーとして用意する。
///
/// 返り値が 2 未満の場合、その `row_count` での `search` は
/// `std::thread::available_parallelism()`（CI runner・ローカル環境のコア数に依存）
/// 由来の縮退も含めて単一スレッド経路（並列化なし）を通ることを意味する。
pub fn expected_thread_count(row_count: usize) -> usize {
    thread_count_for(row_count)
}

/// `ids`（担当範囲だけに絞り込み済み）と `vectors`（絞り込まず全行分。`row_offset` を
/// 起点とする絶対行インデックスで参照する）に対して総当たり Top-k を選出する。
/// 単一スレッド経路・並列ワーカーの両方から呼ばれる共通処理。
///
/// `vectors` を担当範囲で事前に切り出さず絶対インデックスで参照するのは、`get()` に
/// よる境界チェックを行単位で行うためで、`kernel.rs::CpuScalarProvider::search`
/// （行単位で `vectors.get(start..end)` を試し、失敗した 1 行だけを skip する）と
/// 挙動を完全に一致させる（Issue #34 レビュー指摘対応。以前は担当範囲全体を
/// 事前に切り出しており、範囲内の 1 行でも `vectors` が不足しているとパーティション
/// 全体が消えて `CpuScalarProvider` と選出結果が食い違うバグがあった）。
fn search_range(
    ids: &[u64],
    vectors: &[f32],
    row_offset: usize,
    dim: usize,
    query: &[f32],
    k: usize,
) -> TopKSelector {
    let mut selector = TopKSelector::new(k);

    // Issue #510（TASK-156・CORE-14）: 4 行単位でブロックカーネル
    // （`kernel::dot_block4`）へディスパッチし、行間でクエリのロードを 1 回に
    // 共有する。端数（4 の倍数に満たない末尾）とブロック内で 1 行でも
    // `vectors` が不足する場合は、既存の 1 行ずつの経路（[`push_row`]）へ
    // フォールバックする。選出（`TopKSelector`）はスコア・id の全順序で行うため
    // push 順には依存せず、ブロック化しても結果集合・順序は 1 行版と変わらない。
    let (blocks, remainder) = ids.as_chunks::<4>();
    let mut base_row = row_offset;
    for block_ids in blocks {
        let row_slice = |offset: usize| -> Option<&[f32]> {
            let row = base_row.saturating_add(offset);
            let start = row.saturating_mul(dim);
            let end = start.saturating_add(dim);
            vectors.get(start..end)
        };
        let (r0, r1, r2, r3) = (row_slice(0), row_slice(1), row_slice(2), row_slice(3));
        // `as_chunks::<4>()` の型契約上 `block_ids: &[u64; 4]` は常に長さ 4 のため
        // 分解は infallible（`chunks_exact` 時代の到達不能フォールバックは不要）。
        let [id0, id1, id2, id3] = *block_ids;

        match (r0, r1, r2, r3) {
            (Some(v0), Some(v1), Some(v2), Some(v3)) => {
                let scores = crate::kernel::dot_block4([v0, v1, v2, v3], query);
                let ids4 = [id0, id1, id2, id3];
                for (id, score) in ids4.into_iter().zip(scores) {
                    push_score(&mut selector, id, score);
                }
            }
            _ => {
                push_row(&mut selector, id0, r0, query);
                push_row(&mut selector, id1, r1, query);
                push_row(&mut selector, id2, r2, query);
                push_row(&mut selector, id3, r3, query);
            }
        }
        base_row = base_row.saturating_add(4);
    }

    for (offset, &id) in remainder.iter().enumerate() {
        let row = base_row.saturating_add(offset);
        let start = row.saturating_mul(dim);
        let end = start.saturating_add(dim);
        push_row(&mut selector, id, vectors.get(start..end), query);
    }

    selector
}

/// [`search_range`] の 1 行分の候補投入（ブロック化のフォールバック経路・端数行の
/// 双方が共有する）。`vector` が `None`（アリーナ側の不変条件破れ）・スコアが
/// 非有限（NaN/Inf 混入）のいずれの場合も `kernel.rs::CpuScalarProvider::search` と
/// 同じ理由で当該行だけを候補から除外する（呼び出し全体は継続。厳密な
/// fail-closed ではない点は `kernel.rs` 側のコメント参照）。
fn push_row(selector: &mut TopKSelector, id: u64, vector: Option<&[f32]>, query: &[f32]) {
    let Some(vector) = vector else {
        return;
    };
    push_score(selector, id, crate::kernel::dot(vector, query));
}

/// 計算済みスコア 1 件の投入（`dot_block4` 経路・[`push_row`] の双方が共有する
/// 非有限値除外ロジック）。
fn push_score(selector: &mut TopKSelector, id: u64, score: f32) {
    if !score.is_finite() {
        // 格納ベクトルの NaN/Inf 混入に対する除外
        // （`kernel.rs::CpuScalarProvider::search` と同じ理由）。
        return;
    }
    selector.push(CandidateHit { id, score });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_k_returns_highest_dot_product_scores() {
        let ids = [1u64, 2, 3, 4];
        let vectors = [1.0, 0.0, 2.0, 0.0, 0.0, 1.0, 3.0, 0.0];
        let query = [1.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 2,
        };
        let hits = ParallelSearchProvider.search(input).expect("search ok");
        assert_eq!(
            hits,
            vec![
                CandidateHit { id: 4, score: 3.0 },
                CandidateHit { id: 2, score: 2.0 }
            ]
        );
    }

    #[test]
    fn non_finite_query_is_rejected() {
        let ids = [1u64];
        let vectors = [1.0, 0.0];
        for query in [
            [f32::NAN, 0.0],
            [f32::INFINITY, 0.0],
            [0.0, f32::NEG_INFINITY],
        ] {
            let input = SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: 2,
                query: &query,
                k: 1,
            };
            let err = ParallelSearchProvider.search(input).unwrap_err();
            assert_eq!(err, KernelError::NonFiniteQuery, "query={query:?}");
        }
    }

    #[test]
    fn dim_mismatch_query_is_rejected() {
        let ids = [1u64];
        let vectors = [1.0, 0.0];
        let query = [1.0, 0.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 1,
        };
        let err = ParallelSearchProvider.search(input).unwrap_err();
        assert_eq!(
            err,
            KernelError::DimMismatch {
                expected: 2,
                found: 3
            }
        );
    }

    #[test]
    fn k_zero_returns_empty() {
        let ids = [1u64];
        let vectors = [1.0, 0.0];
        let query = [1.0, 0.0];
        let input = SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: 2,
            query: &query,
            k: 0,
        };
        let hits = ParallelSearchProvider.search(input).expect("search ok");
        assert!(hits.is_empty());
    }

    #[test]
    fn multi_thread_path_is_deterministic_across_repeated_runs() {
        // MIN_ROWS_PER_THREAD を超える規模でマルチスレッド経路を実際に使わせ、
        // 同一入力を 2 回実行して結果が完全一致することを確認する（CORE-3・SEARCH-4）。
        let dim = 16usize;
        let row_count = MIN_ROWS_PER_THREAD * 4 + 7;
        let mut ids = Vec::with_capacity(row_count);
        let mut vectors = Vec::with_capacity(row_count * dim);
        for i in 0..row_count {
            ids.push(i as u64);
            for d in 0..dim {
                vectors.push(((i * dim + d) % 97) as f32 * 0.01 - 0.5);
            }
        }
        let query: Vec<f32> = (0..dim).map(|d| (d as f32) * 0.1 - 0.5).collect();

        let run = || {
            let input = SearchInput {
                ids: &ids,
                vectors: &vectors,
                dim: dim as u32,
                query: &query,
                k: 20,
            };
            ParallelSearchProvider.search(input).expect("search ok")
        };
        let first = run();
        let second = run();
        assert_eq!(first, second);
        assert_eq!(first.len(), 20);
    }

    // object-safety の固定（CORE-13）: `Box<dyn SearchProvider>` として保持できること。
    #[test]
    fn provider_is_object_safe() {
        let _boxed: Box<dyn SearchProvider> = Box::new(ParallelSearchProvider);
    }

    #[test]
    fn search_range_with_nonzero_offset_skips_only_the_row_straddling_the_vectors_boundary() {
        // Issue #34 レビュー指摘の回帰テスト: 非 0 の `row_offset`（実際の並列パスで
        // 各ワーカーが受け取る値）を使い、担当範囲の途中で `vectors` が尽きる状況で
        // パーティション全体ではなく境界の 1 行だけが除外されることを、
        // `thread_count_for` の実際のスレッド数（CI 環境のコア数依存）に依存せず
        // 直接検証する。`row_offset = 0` だと `row = row_offset + idx` が旧実装の
        // `row = idx` と区別できないため、必ず非 0 の offset で境界をまたぐケースにする。
        let dim = 2usize;
        // 6 行分（0..6）の embedding のうち、末尾の行 5 だけ 1 要素分足りない
        // （`vectors.len() == 11` は `6 * dim == 12` に対して 1 要素不足）。
        let vectors = [
            1.0f32, 0.0, // row 0
            2.0, 0.0, // row 1
            3.0, 0.0, // row 2
            4.0, 0.0, // row 3
            5.0, 0.0, // row 4
            6.0, // row 5（1 要素欠落・境界外）
        ];
        let query = [1.0f32, 0.0];
        // 担当範囲は絶対行 4..6（id=100 → row 4, id=101 → row 5）。
        let ids = [100u64, 101];
        let row_offset = 4usize;

        let selector = search_range(&ids, &vectors, row_offset, dim, &query, 10);
        let hits = selector.into_sorted_vec();

        // row 4（id=100）は `vectors[8..10]` が範囲内 → score 5.0 で選出される。
        // row 5（id=101）は `vectors[10..12]` が範囲外 → 当該行だけ除外される
        // （パーティション全体が消える旧実装のバグなら空集合になり、このアサーションが
        // 落ちる）。
        assert_eq!(
            hits,
            vec![CandidateHit {
                id: 100,
                score: 5.0
            }]
        );
    }

    #[test]
    fn search_range_with_non_multiple_of_4_row_count_matches_scalar_reference() {
        // Issue #510（TASK-156・CORE-14）の 4 行ブロックカーネル化の回帰: 行数が
        // 4 の倍数でない（2 ブロック＋端数 1）場合でも `search_range`
        // （`ParallelSearchProvider` が使う共通経路）の選出結果が
        // `CpuScalarProvider`（1 行版参照実装）とビット単位で一致することを確認する。
        use crate::kernel::CpuScalarProvider;

        let dim = 6usize;
        let n = 9usize; // chunks_exact(4) で 2 ブロック + 端数 1 行。
        let mut ids = Vec::with_capacity(n);
        let mut vectors = Vec::with_capacity(n * dim);
        for i in 0..n {
            ids.push(i as u64);
            for d in 0..dim {
                vectors.push(((i * dim + d) % 17) as f32 * 0.1 - 0.7);
            }
        }
        let query: Vec<f32> = (0..dim).map(|d| (d as f32) * 0.13 - 0.4).collect();

        let make_input = || SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: dim as u32,
            query: &query,
            k: 5,
        };

        let simd_hits = ParallelSearchProvider
            .search(make_input())
            .expect("simd ok");
        let scalar_hits = CpuScalarProvider.search(make_input()).expect("scalar ok");
        assert_eq!(
            simd_hits, scalar_hits,
            "n が 4 の倍数でない場合もブロック化経路と 1 行版参照実装の選出が一致すること"
        );
    }

    #[test]
    fn search_range_block_with_one_missing_row_only_skips_that_row() {
        // 4 行ブロック内で末尾 1 行だけ `vectors` が不足する場合、ブロック全体
        // ではなく当該行のみが選出対象から除外されること（`dot_block4_impl` の
        // `uniform_len` 判定不成立 → per-row フォールバック経路が正しく機能する
        // ことの直接検証。Issue #510。フラット配列の構造上「途中の行だけ欠落」は
        // 表現できない——欠落は必ず末尾行から生じる——ため、
        // `search_range_with_nonzero_offset_skips_only_the_row_straddling_the_vectors_boundary`
        // と同じ「末尾行が範囲外」の構図を、4 行ブロックの内部で検証する）。
        let dim = 2usize;
        // 3 行分（0..3）のみ完全で、ブロック内 4 行目（行 3・id=13）はまるごと
        // 範囲外（`vectors.len() == 6` は `4 * dim == 8` に対して 2 要素不足）。
        let vectors = [
            1.0f32, 0.0, // row 0 (id=10)
            2.0, 0.0, // row 1 (id=11)
            3.0, 0.0, // row 2 (id=12)
        ];
        let query = [1.0f32, 0.0];
        let ids = [10u64, 11, 12, 13];

        let selector = search_range(&ids, &vectors, 0, dim, &query, 10);
        let hits = selector.into_sorted_vec();

        // row 3（id=13）だけが `vectors.get(6..8)` の範囲外で除外され、他 3 行は
        // 通常どおり選出される。
        assert_eq!(
            hits,
            vec![
                CandidateHit { id: 12, score: 3.0 },
                CandidateHit { id: 11, score: 2.0 },
                CandidateHit { id: 10, score: 1.0 },
            ]
        );
    }

    #[test]
    fn search_range_with_dim_zero_and_k_zero_does_not_panic() {
        // Issue #510: 4 行ブロックカーネル化後も `dim == 0`／`k == 0` で panic せず
        // 空集合を返すこと（`as_chunks::<8>()` 等のブロックカーネル内部が dim=0 の
        // 空スライスを問題なく扱えることの回帰）。
        let ids = [1u64, 2, 3, 4, 5];
        let vectors: [f32; 0] = [];
        let query: [f32; 0] = [];

        let selector = search_range(&ids, &vectors, 0, 0, &query, 0);
        assert!(selector.into_sorted_vec().is_empty());
    }

    #[test]
    fn multi_thread_path_matches_scalar_reference_when_vectors_are_truncated() {
        // レビューで実機再現された条件（dim=32, n=5000, 末尾 1 行分の vectors を
        // truncate）を縮小再現し、`ParallelSearchProvider`（マルチスレッド経路を強制する
        // 規模）と `CpuScalarProvider` の選出集合が一致することを確認する。
        use crate::kernel::CpuScalarProvider;

        let dim = 16usize;
        let row_count = MIN_ROWS_PER_THREAD * 2 + 3;
        let mut ids = Vec::with_capacity(row_count);
        let mut vectors = Vec::with_capacity(row_count * dim);
        for i in 0..row_count {
            ids.push(i as u64);
            for d in 0..dim {
                vectors.push(((i * dim + d) % 89) as f32 * 0.01 - 0.4);
            }
        }
        // 末尾行 1 行分の vectors を truncate し、`ids.len() * dim != vectors.len()` の
        // 不変条件破れを再現する。
        vectors.truncate(vectors.len() - dim / 2);
        let query: Vec<f32> = (0..dim).map(|d| (d as f32) * 0.05 - 0.3).collect();

        let make_input = || SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: dim as u32,
            query: &query,
            k: 20,
        };

        let simd_hits = ParallelSearchProvider
            .search(make_input())
            .expect("simd ok");
        let scalar_hits = CpuScalarProvider.search(make_input()).expect("scalar ok");

        // `dot` を共有するため id・score とも bit 単位で一致するはず（Issue #34
        // codex-review P1 指摘対応: 以前の自前ベクトル化は dim>=16 で加算順序が
        // 分岐し、値の一致は保証できなかった）。
        assert_eq!(
            simd_hits, scalar_hits,
            "ParallelSearchProvider と CpuScalarProvider の選出 id・score が一致すること"
        );
    }

    #[test]
    fn multi_thread_path_matches_scalar_reference_for_near_tie_scores_at_dim_ge_16() {
        // codex-review P1 指摘の回帰テスト: `dim >= 16` かつマルチスレッド経路
        // （`MIN_ROWS_PER_THREAD` を超える規模）で、複数行がほぼ同一スコア（僅差）に
        // なるよう意図的に作った入力に対しても、`ParallelSearchProvider` と
        // `CpuScalarProvider` の Top-k が完全一致することを確認する。以前の自前
        // ベクトル化（複数アキュムレータでの並び替え加算）は、この種の僅差入力で
        // 丸め誤差により集合・順序がスカラー参照実装と食い違い得た。
        use crate::kernel::CpuScalarProvider;

        let dim = 32usize;
        let row_count = MIN_ROWS_PER_THREAD * 3 + 11;
        let mut ids = Vec::with_capacity(row_count);
        let mut vectors = Vec::with_capacity(row_count * dim);
        for i in 0..row_count {
            ids.push(i as u64);
            for d in 0..dim {
                // 行ごとにベクトル要素の並び順だけを変える（総和はほぼ同じになるよう
                // 値の集合を固定し、行内の順序を row index で回転させる）ことで、
                // 内積の加算順序に依存する丸め誤差が出やすい「ほぼ同スコア」の
                // 候補群を作る。
                let phase = (i + d) % dim;
                vectors.push(((phase % 17) as f32) * 0.1 - 0.8);
            }
        }
        let query: Vec<f32> = (0..dim).map(|d| ((d % 13) as f32) * 0.1 - 0.6).collect();

        let make_input = || SearchInput {
            ids: &ids,
            vectors: &vectors,
            dim: dim as u32,
            query: &query,
            k: 10,
        };

        let simd_hits = ParallelSearchProvider
            .search(make_input())
            .expect("simd ok");
        let scalar_hits = CpuScalarProvider.search(make_input()).expect("scalar ok");

        assert_eq!(
            simd_hits, scalar_hits,
            "僅差スコアの Top-k 境界でも ParallelSearchProvider と CpuScalarProvider の \
             選出 id・順序・score が一致すること"
        );
    }

    #[test]
    fn scoped_worker_panic_is_mapped_to_worker_panicked_error_without_repanicking() {
        // Cursor Bugbot 指摘の回帰テスト: `search()` の並列経路が使う
        // `join_all_or_panicked` を直接検証する（`search()` 本体と同一の関数を通すため、
        // 手書きの複製がドリフトして検知漏れになる余地がない）。
        //
        // 意図的にパニックするハンドルを 2 つ、先頭と末尾に置く。`collect::<Result<_,
        // _>>()` を素朴に使う旧実装だと、先頭ハンドルの `join()` で最初の `Err` を
        // 受け取った時点で早期リターンし、末尾のパニックするハンドルは一度も `join()`
        // されない。`std::thread::scope` は未 `join` のハンドルが残っているとスコープ
        // 終了時に自らそれを `join()` し、それも panic していればスコープ自身が
        // 再 panic する（`KernelError::WorkerPanicked` へのマッピングに到達する前に
        // panic が呼び出し元まで伝播してしまう。中間に正常終了するハンドルだけでは
        // 未 `join` のまま残っても再 panic せずこのバグを見逃すため、末尾も必ず
        // パニックさせる）。`join_all_or_panicked` は全ハンドルを先に `join()` し
        // 尽くしてから判定するため、この再 panic が起きないことを以下で確認する。
        //
        // パニックメッセージが標準の panic hook 経由でテスト出力に出るのを避けるため、
        // 一時的に無音の hook へ差し替える。
        let previous_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));
        let result = std::thread::scope(|scope| {
            let panicking_handle_a =
                scope.spawn(|| -> TopKSelector { panic!("induced for test (first)") });
            let ok_handle = scope.spawn(|| TopKSelector::new(1));
            let panicking_handle_b =
                scope.spawn(|| -> TopKSelector { panic!("induced for test (last)") });
            join_all_or_panicked(vec![panicking_handle_a, ok_handle, panicking_handle_b])
        });
        std::panic::set_hook(previous_hook);

        // `TopKSelector` は `PartialEq` を実装しないため `Ok` 側の値比較はできないが、
        // `Err` 変種であることの確認だけで本テストの目的（fail-closed マッピングと
        // 再 panic なし）には十分。
        assert!(matches!(result, Err(KernelError::WorkerPanicked)));
    }

    // Issue #654: `search_subset` の単一スレッド経路が、`slots` の行を gather して
    // `CpuScalarProvider::search` を呼んだ場合とビット同一の結果になること。
    #[test]
    fn search_subset_single_thread_matches_gathered_scalar_reference() {
        use crate::kernel::CpuScalarProvider;

        let dim = 6usize;
        let rows = 20usize; // MIN_ROWS_PER_THREAD 未満・単一スレッド経路を強制。
        let mut vectors = Vec::with_capacity(rows * dim);
        for i in 0..rows {
            for d in 0..dim {
                vectors.push(((i * dim + d) % 17) as f32 * 0.1 - 0.7);
            }
        }
        let query: Vec<f32> = (0..dim).map(|d| (d as f32) * 0.13 - 0.4).collect();
        let slots: Vec<u32> = vec![1, 3, 4, 7, 9, 12, 15, 19];

        let subset_hits = ParallelSearchProvider
            .search_subset(SubsetSearchInput {
                slots: &slots,
                vectors: &vectors,
                dim: dim as u32,
                query: &query,
                k: 4,
            })
            .expect("search_subset ok");

        let mut gathered = Vec::new();
        let mut ids = Vec::new();
        for &slot in &slots {
            let start = slot as usize * dim;
            gathered.extend_from_slice(&vectors[start..start + dim]);
            ids.push(slot as u64);
        }
        let reference_hits = CpuScalarProvider
            .search(SearchInput {
                ids: &ids,
                vectors: &gathered,
                dim: dim as u32,
                query: &query,
                k: 4,
            })
            .expect("scalar ok");

        assert_eq!(subset_hits, reference_hits);
    }

    // Issue #654: `search_subset` の並列経路（候補数が `MIN_ROWS_PER_THREAD` を
    // 超える規模）が、gather 済み `CpuScalarProvider::search` とビット同一の結果に
    // なること。同点誘発（同一ベクトルを多数含む）フィクスチャでタイブレークの
    // 一致も検証する。偶数スロットのみへ絞り込んだ後の候補数（`rows` の約半分）
    // が `thread_count_for` で複数スレッドへ分割される規模（`MIN_ROWS_PER_THREAD` の
    // 2 倍以上）を確実に超えるよう、絞り込み前の `rows` を候補数ベースで決める
    // （codex-review 指摘対応・PR #664: 絞り込み後 1539 件では
    // `thread_count_for(1539) == 1` となり並列経路の分割・部分結果マージが
    // 未検証だった）。
    #[test]
    fn search_subset_parallel_path_matches_gathered_scalar_reference_with_ties() {
        use crate::kernel::CpuScalarProvider;

        let dim = 8usize;
        // 偶数スロットへの絞り込みで候補数がおよそ半分になるため、候補数が
        // `MIN_ROWS_PER_THREAD * 2` を上回るよう `rows` を余裕を持って確保する。
        let rows = MIN_ROWS_PER_THREAD * 6 + 5;
        let mut vectors = Vec::with_capacity(rows * dim);
        for i in 0..rows {
            for d in 0..dim {
                // 同点を誘発するため、行を 16 件ごとに繰り返すパターンにする。
                let phase = (i % 16) * dim + d;
                vectors.push(((phase % 11) as f32) * 0.1 - 0.5);
            }
        }
        let query: Vec<f32> = (0..dim).map(|d| ((d % 5) as f32) * 0.2 - 0.4).collect();
        // 候補スロットは全行の約半分（偶数番のみ）を昇順・重複なしで選ぶ。
        let slots: Vec<u32> = (0..rows as u32).filter(|s| s % 2 == 0).collect();

        // 実行環境が複数コアを持つ場合に限り、この規模で実際に並列経路（複数
        // ワーカーへのスロット分割＋部分結果マージ）へ入ることを非 vacuous に
        // 確認する（codex-review 指摘対応・PR #664）。`available_parallelism` が
        // 1（コンテナ制約等）を返す環境では `thread_count_for` が単一スレッドへ
        // 縮退するのは仕様どおりであり、CI 環境のコア数に依存してテスト自体が
        // 不安定化しないよう条件付きにする。
        if std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            >= 2
        {
            assert!(
                thread_count_for(slots.len()) >= 2,
                "この規模（候補数 {}）では並列経路を通るはず",
                slots.len()
            );
        }

        let subset_hits = ParallelSearchProvider
            .search_subset(SubsetSearchInput {
                slots: &slots,
                vectors: &vectors,
                dim: dim as u32,
                query: &query,
                k: 15,
            })
            .expect("search_subset ok");

        let mut gathered = Vec::new();
        let mut ids = Vec::new();
        for &slot in &slots {
            let start = slot as usize * dim;
            gathered.extend_from_slice(&vectors[start..start + dim]);
            ids.push(slot as u64);
        }
        let reference_hits = CpuScalarProvider
            .search(SearchInput {
                ids: &ids,
                vectors: &gathered,
                dim: dim as u32,
                query: &query,
                k: 15,
            })
            .expect("scalar ok");

        assert_eq!(subset_hits, reference_hits);
        assert_eq!(subset_hits.len(), 15);
    }

    #[test]
    fn search_subset_object_safe_and_k_zero_or_empty_returns_empty() {
        let boxed: Box<dyn SearchProvider> = Box::new(ParallelSearchProvider);
        let vectors = [1.0f32, 0.0];
        let query = [1.0f32, 0.0];
        let empty: [u32; 0] = [];
        assert!(boxed
            .search_subset(SubsetSearchInput {
                slots: &[0],
                vectors: &vectors,
                dim: 2,
                query: &query,
                k: 0,
            })
            .expect("k=0 ok")
            .is_empty());
        assert!(boxed
            .search_subset(SubsetSearchInput {
                slots: &empty,
                vectors: &vectors,
                dim: 2,
                query: &query,
                k: 1,
            })
            .expect("empty slots ok")
            .is_empty());
    }

    #[test]
    fn search_range_by_slots_with_non_multiple_of_4_and_missing_row_skips_only_that_row() {
        // Issue #654: `search_range_by_slots` の 4 スロットブロック化 + フォールバック
        // が `search_range`（絶対行版）と同じ「欠損 1 行だけ除外」規約を守ること。
        let dim = 2usize;
        let vectors = [
            1.0f32, 0.0, // slot 0
            2.0, 0.0, // slot 1
            3.0, 0.0, // slot 2
        ]; // slot 3 は範囲外（vectors.len()==6 は 4*dim==8 に対して不足）。
        let query = [1.0f32, 0.0];
        let slots = [0u32, 1, 2, 3];

        let selector = search_range_by_slots(&slots, &vectors, dim, &query, 10);
        let hits = selector.into_sorted_vec();
        assert_eq!(
            hits,
            vec![
                CandidateHit { id: 2, score: 3.0 },
                CandidateHit { id: 1, score: 2.0 },
                CandidateHit { id: 0, score: 1.0 },
            ]
        );
    }
}
