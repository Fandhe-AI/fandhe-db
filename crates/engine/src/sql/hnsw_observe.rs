//! `EXPLAIN ANALYZE` が実行した 1 文の HNSW 実行時判断（キャッシュ状態・厳密走査への切り替え・
//! 反復スキャン・hybrid 密側の探索ラウンド数）を集める観測シンク（Issue #2056・SQL-76 (2)(3)）。
//!
//! `core::explain_analyze_exec` が実行段だけを [`enter`] のガードで囲み、`sql::hnsw_cache` の
//! 記録点（[`note`]）が同じスレッドの上で値を書き込む。結果は `sql::explain_analyze` が
//! `QUERY PLAN` へ併記する。
//!
//! # 不変条件
//!
//! - **ガードが無いとき何もしない。** 通常の実行・静的な `EXPLAIN` の挙動は変えない。
//! - **全テナント共通の集計 API（`hnsw_index_cache_stats`）の差分は使わない。** 他テナント・並行
//!   セッションの活動が混入し漏えいになるため、実行した 1 文の分だけをスレッドローカルに集める。
//! - 記録点は文を実行するスレッド上にある前提（`hybrid.rs`・`hnsw_cache.rs`・`hnsw_hybrid.rs` は
//!   rayon・別スレッドを使わない）。この前提は結合テストが「HNSW の行が実際に出る」ことで固定する。
//! - 借用に失敗したら記録を捨てるだけで、実行結果・エラーには影響しない。

use std::cell::RefCell;

/// 反復スキャン（SQL-73）の実行結果。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct IterativeObs {
    /// 探索ラウンド数。
    pub(crate) rounds: u64,
    /// 走査上限に到達して打ち切ったか。
    pub(crate) cap_hit: bool,
    /// DISTANCE 段が事後フィルタへ渡した可視の候補行数の合計（グラフの訪問ノード数ではない）。
    pub(crate) rows_examined: u64,
}

/// 実行した 1 文の HNSW 観測値。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HnswObservation {
    /// 既存の索引を再利用した解決の回数。
    pub(crate) cache_cached: u64,
    /// 索引を新規構築した解決の回数。
    pub(crate) cache_built: u64,
    /// 差分が大きく索引を再構築した解決の回数。
    pub(crate) cache_rebuilt: u64,
    /// 索引を使えず厳密走査になった解決の回数。
    pub(crate) cache_not_indexed: u64,
    /// 厳密走査へ切り替えた探索の回数。
    pub(crate) exact_scan_switches: u64,
    /// 反復スキャンが走ったときの結果。
    pub(crate) iterative: Option<IterativeObs>,
    /// hybrid 密側の探索の延べラウンド数。
    pub(crate) hybrid_dense_rounds: u64,
    /// HNSW 経路の試行が 1 回でもあったか。
    pub(crate) attempted: bool,
}

thread_local! {
    static SINK: RefCell<Option<HnswObservation>> = const { RefCell::new(None) };
    /// true の間はキャッシュ状態（cached／built／rebuilt／not_indexed）の記録を捨てる。
    static CACHE_SUPPRESSED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// 事前構築（`prepare_base_from_full_visible`）が同じ検索の解決を既に built／rebuilt として
    /// 数えたことを、直後の部分集合解決へ引き継ぐフラグ。
    static PRE_RESOLVED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// 事前構築が解決を数え済みであることを記録する。直後の部分集合解決が [`take_pre_resolved`] で
/// 受け取り、cached を重ねて数えない（built／rebuilt と cached を排他にする。Issue #2056）。
pub(crate) fn mark_pre_resolved() {
    let _ = PRE_RESOLVED.try_with(|c| c.set(true));
}

/// [`mark_pre_resolved`] のフラグを取り出して下ろす。
pub(crate) fn take_pre_resolved() -> bool {
    PRE_RESOLVED.try_with(|c| c.replace(false)).unwrap_or(false)
}

/// キャッシュ状態の記録を一時的に止めるガード。反復スキャンの 2 ラウンド目以降が同じ検索の
/// 解決を重ねて数えないために使う（1 検索につき解決 1 回。Issue #2056）。Drop で元に戻す。
pub(crate) struct CacheSuppressGuard {
    prev: bool,
}

/// キャッシュ状態の記録を止める。返るガードが生きている間 [`note_cache`] は何もしない。
pub(crate) fn suppress_cache_notes() -> CacheSuppressGuard {
    let prev = CACHE_SUPPRESSED
        .try_with(|c| c.replace(true))
        .unwrap_or(false);
    CacheSuppressGuard { prev }
}

impl Drop for CacheSuppressGuard {
    fn drop(&mut self) {
        let prev = self.prev;
        let _ = CACHE_SUPPRESSED.try_with(|c| c.set(prev));
    }
}

/// キャッシュ状態の記録専用の [`note`]。[`suppress_cache_notes`] 中は捨てる。
pub(crate) fn note_cache(f: impl FnOnce(&mut HnswObservation)) {
    if CACHE_SUPPRESSED.try_with(|c| c.get()).unwrap_or(false) {
        return;
    }
    note(f);
}

/// 観測ガード。Drop で前の状態へ戻す（ネスト・panic でも復元される）。
pub(crate) struct HnswObserveGuard {
    prev: Option<HnswObservation>,
}

/// 観測を開始する。返るガードが生きている間だけ [`note`] が値を書き込む。
pub(crate) fn enter() -> HnswObserveGuard {
    let _ = PRE_RESOLVED.try_with(|c| c.set(false));
    let prev = SINK
        .try_with(|s| {
            s.try_borrow_mut()
                .ok()
                .map(|mut b| b.replace(HnswObservation::default()))
        })
        .ok()
        .flatten()
        .flatten();
    HnswObserveGuard { prev }
}

impl HnswObserveGuard {
    /// 観測を終えて集めた値を返す。
    pub(crate) fn finish(self) -> HnswObservation {
        SINK.try_with(|s| s.try_borrow().ok().and_then(|b| *b))
            .ok()
            .flatten()
            .unwrap_or_default()
    }
}

impl Drop for HnswObserveGuard {
    fn drop(&mut self) {
        let prev = self.prev.take();
        let _ = SINK.try_with(|s| {
            if let Ok(mut b) = s.try_borrow_mut() {
                *b = prev;
            }
        });
    }
}

/// ガードがあるときだけ `f` で観測値を更新する（`attempted` も立てる）。
pub(crate) fn note(f: impl FnOnce(&mut HnswObservation)) {
    let _ = SINK.try_with(|s| {
        if let Ok(mut b) = s.try_borrow_mut() {
            if let Some(o) = b.as_mut() {
                o.attempted = true;
                f(o);
            }
        }
    });
}

/// 現在の `built + rebuilt` の合計（ガードが無ければ 0）。解決の出口で「この呼び出し中に構築が
/// あったか」を判定するのに使う。
pub(crate) fn build_events() -> u64 {
    SINK.try_with(|s| {
        s.try_borrow()
            .ok()
            .and_then(|b| b.map(|o| o.cache_built.saturating_add(o.cache_rebuilt)))
    })
    .ok()
    .flatten()
    .unwrap_or(0)
}

/// 現在の `not_indexed` の回数（ガードが無ければ 0）。
pub(crate) fn cache_not_indexed() -> u64 {
    SINK.try_with(|s| {
        s.try_borrow()
            .ok()
            .and_then(|b| b.map(|o| o.cache_not_indexed))
    })
    .ok()
    .flatten()
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_without_guard_is_noop() {
        note(|o| o.cache_cached += 1);
        assert_eq!(build_events(), 0);
    }

    #[test]
    fn guard_collects_and_finish_returns() {
        let g = enter();
        note(|o| o.cache_built += 1);
        note(|o| o.exact_scan_switches += 2);
        let o = g.finish();
        assert_eq!(o.cache_built, 1);
        assert_eq!(o.exact_scan_switches, 2);
        assert!(o.attempted);
        // finish 後はガードが落ちて無効になる。
        note(|o| o.cache_built += 1);
        assert_eq!(build_events(), 0);
    }

    #[test]
    fn nested_guard_restores_outer() {
        let outer = enter();
        note(|o| o.cache_cached += 1);
        {
            let inner = enter();
            note(|o| o.cache_cached += 5);
            assert_eq!(inner.finish().cache_cached, 5);
        }
        assert_eq!(outer.finish().cache_cached, 1);
    }

    #[test]
    fn unfinished_guard_restores_on_drop() {
        {
            let _g = enter();
            note(|o| o.cache_built += 1);
        }
        assert_eq!(build_events(), 0);
    }

    #[test]
    fn untouched_observation_is_not_attempted() {
        let o = enter().finish();
        assert!(!o.attempted);
    }

    #[test]
    fn suppressed_cache_notes_are_dropped_and_restored() {
        let g = enter();
        note_cache(|o| o.cache_cached += 1);
        {
            let _s = suppress_cache_notes();
            note_cache(|o| o.cache_cached += 10);
            // キャッシュ以外の記録は止めない。
            note(|o| o.exact_scan_switches += 1);
        }
        note_cache(|o| o.cache_cached += 1);
        let o = g.finish();
        assert_eq!(o.cache_cached, 2);
        assert_eq!(o.exact_scan_switches, 1);
    }

    #[test]
    fn pre_resolved_flag_is_taken_once_and_reset_on_enter() {
        let g = enter();
        assert!(!take_pre_resolved());
        mark_pre_resolved();
        assert!(take_pre_resolved());
        assert!(!take_pre_resolved());
        mark_pre_resolved();
        drop(g);
        let _g2 = enter();
        assert!(!take_pre_resolved());
    }
}
