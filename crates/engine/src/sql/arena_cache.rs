//! `sql::exec::execute_statement_with_cache` 専用の [`crate::arena::VectorArena`]
//! テーブル世代整合キャッシュ（Issue #363・VectorArena のテーブル世代整合キャッシュ
//! 化）。
//!
//! [`crate::core::PrefilterCache`]（`EngineCore::search`・Rust API 直呼び経路が使う）
//! と役割は同種だが、失効判定の世代源泉が異なる: `PrefilterCache` はストレージ全体
//! 世代（`Storage::current_generation`）を見るのに対し、本キャッシュは**テーブル
//! 単位世代**（`catalog::table_generation_in_txn`。`USING PLAN` の I/O 前後照合が
//! 使うのと同じ源泉）を見る。SQL 表層は 1 クエリが 1 テーブルのみを対象とするため、
//! テーブル単位世代の方が「無関係な他テーブルへの書き込みでエントリを不要に失効
//! させない」点でキャッシュの有効性が高い（`docs/design/
//! table-generation-rejection-granularity.md` の粒度判断を踏襲。両者を混在させず、
//! `PrefilterCache` をテーブル単位世代へ統一する判断は本 Issue のスコープ外
//! ——`sql::exec` 経路の追加のみを対象とする）。
//!
//! **キー**: `(table, ctx)` の完全一致（[`PolicyContext`] は `PartialEq`/`Eq` を
//! テナント ID・許可可視性集合の値比較として実装しており、`ImplicitRlsHook::
//! predicate` はこれら以外の入力を一切読まない。security.md P0「テナント分離の
//! 検査を外す/緩める/バイパス経路を作らない」: ctx が 1 bit でも異なれば別エントリ
//! になり、他テナント・他可視性のスナップショットを供する経路を構造的に作らない）。
//!
//! **fail-closed 契約（[`SqlArenaCache::lookup`] と [`SqlArenaCache::insert`] で
//! 非対称。`sql::sparse_cache::SparseIndexCache` と同型 — Issue #357 のレビュー
//! 指摘対応（codex-review P1・Cursor Bugbot 指摘）で確立した契約をそのまま踏襲する）**:
//! - [`SqlArenaCache::lookup`] は世代不一致・ロック毒化・世代読み取り失敗のいずれも
//!   「見つからなかった」として扱う。ただし `read_txn`（呼び出し元のスナップ
//!   ショット）視点での不一致は、そのエントリが真に stale（`storage` から読んだ
//!   最新世代より古い）と確認できた場合のみ破棄する。`read_txn` が古い可能性が
//!   あるため、より新しい在り得るエントリを「不一致」というだけで消してはならない。
//! - [`SqlArenaCache::insert`] は挿入対象自身が既に古い場合・ロック毒化時は
//!   キャッシュへ**反映しない**が、呼び出し元へは常に構築済みの `Arc<SqlArenaSnapshot>`
//!   を返す（呼び出し元がこの結果を構築したのは自分自身のクエリの `read_txn`
//!   〔単一スナップショット〕上であり、そのスナップショットの中でのみ使う限り
//!   stale にはなり得ないため。fail-closed が守るべき対象は「stale な**キャッシュ**を
//!   別クエリへ供すること」であって、この 1 回限りの自分自身の結果ではない）。
//!   また `retain` による世代不一致エントリの一括破棄は**挿入対象テーブルに限定**
//!   する（テーブルごとに世代カウンタが独立しているため、他テーブルのエントリと
//!   世代を比較しても意味がない。比較すると無関係な他テーブルの有効なエントリまで
//!   誤って巻き添え失効させる）。
//!
//! **容量**: [`MAX_SQL_ARENA_CACHE_ENTRIES`]・[`MAX_SQL_ARENA_CACHE_TOTAL_BYTES`] を
//! 超えないよう、[`crate::core::PrefilterCache`] と同じ手順（同一キー重複除去 →
//! 挿入対象テーブルに限った現在世代との不整合エントリの一括破棄 → それでも超過
//! するなら LRU 追い出し）で管理する。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use redb::ReadableDatabase;

use crate::arena::VectorArena;
use crate::policy::PolicyContext;
use crate::storage::Storage;

/// [`SqlArenaCache`] のエントリ数上限（Issue #363。`core.rs::PrefilterCache`
/// （TASK-169）と同じ DoS 対策方針を踏襲する）。
const MAX_SQL_ARENA_CACHE_ENTRIES: usize = 32;

/// [`SqlArenaCache`] が保持するスナップショット群（アリーナ本体＋行 metadata 複製）の
/// 概算バイト量の合計上限（`core.rs::MAX_PREFILTER_CACHE_TOTAL_BYTES` と同じ桁に
/// 揃える）。
const MAX_SQL_ARENA_CACHE_TOTAL_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// [`SqlArenaCache`] の観測用統計（Issue #363）。テナント ID・行 ID 等の機微情報は
/// 一切含まない（`core.rs::PrefilterCacheStats` と同じ方針。`VectorCore` trait には
/// 載せない固有 API `EngineCore::sql_arena_cache_stats` としてのみ公開する）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SqlArenaCacheStats {
    /// キャッシュヒット数（テーブル世代整合まで確認できた再利用）。
    pub hits: u64,
    /// キャッシュミス数（未登録、またはテーブル世代不一致で破棄した後の再構築）。
    pub misses: u64,
    /// テーブル世代不一致による破棄回数。
    pub stale_evictions: u64,
    /// 容量上限超過による LRU 追い出し回数。
    pub capacity_evictions: u64,
    /// 現在キャッシュが保持しているエントリ数。
    pub entries: usize,
    /// キャッシュヒット後に SCALAR 段が恒等写像と判定でき、`VectorArena` を
    /// 一切複製せずキャッシュ済みスナップショットを借用したまま実行できた回数
    /// （`sql::exec::execute_statement_with_cache` の高速経路。Issue #363・
    /// hybrid への拡張は Issue #660 の改善候補 P1）。
    pub fast_path_borrows: u64,
    /// キャッシュヒットにもかかわらず、SCALAR 段に実質的な処理があるため
    /// 可視全行を `VectorArena::build_from_cached_rls_rows` で複製し直した回数。
    /// hybrid 経路が高速経路へ載ったことの非 vacuous な証跡（＝この値が増えない
    /// こと）として `tests/sql_arena_cache.rs` が固定する。
    pub full_rebuild_copies: u64,
}

/// `sql::exec::execute_statement_with_cache` が [`SqlArenaCache`] に格納・再利用する
/// スナップショット（Issue #363）。`arena`（RLS 段のみを適用して構築した
/// [`VectorArena`]）と `metadata`（`arena` とスロット添字が 1 対 1 に対応する行
/// metadata の複製。`VectorArena::build_from_cached_rls_rows` のドキュメント参照）を
/// 一組で保持する。SCALAR 段（`WHERE`）はクエリごとに異なるため事前適用しない
/// （キャッシュヒット時にクエリごと `on_visible_row` を再適用する。
/// `sql::exec` モジュールドキュメント参照）。
pub(crate) struct SqlArenaSnapshot {
    arena: VectorArena,
    metadata: Vec<Vec<u8>>,
    built_ctx: PolicyContext,
    built_table_generation: u64,
    /// 行ごとのノルム²（`<=>` 全走査の C2 入力。Issue #3217）。初回の `<=>` 全走査で
    /// 一度だけ `arena` から計算する遅延初期化。スナップショット自身と同じ寿命で破棄
    /// される（世代・ctx 単位。他テナントのスナップショットとは共有しない）。確保失敗は
    /// `None`（C1 経路へ縮退する fail-closed）。追加メモリは行数 × 4 バイト。
    row_norms_sq: std::sync::OnceLock<Option<Box<[f32]>>>,
}

impl SqlArenaSnapshot {
    /// `sql::exec::execute_statement_with_cache`（キャッシュミス時）が、RLS 通過行の
    /// 採取結果（[`crate::arena::SqlArenaCaptureBuilder::finish`]）とクエリ実行時の
    /// `ctx`・テーブル世代からスナップショットを組み立てる。
    pub(crate) fn new(
        arena: VectorArena,
        metadata: Vec<Vec<u8>>,
        built_ctx: PolicyContext,
        built_table_generation: u64,
    ) -> Self {
        Self {
            arena,
            metadata,
            built_ctx,
            built_table_generation,
            row_norms_sq: std::sync::OnceLock::new(),
        }
    }

    /// `arena` の各行のノルム²（スロット順）。初回呼び出しで一度だけ計算して常駐させる
    /// （Issue #3217）。`sql::exec` の `<=>` 全走査が、実際に走査する arena がこの
    /// スナップショットの `arena()` そのものである場合に限り `CosineSearchInput::
    /// row_norms_sq` へ渡す。確保失敗時は `None`（呼び出し側は C1 経路へ縮退する）。
    pub(crate) fn row_norms_sq(&self) -> Option<&[f32]> {
        self.row_norms_sq
            .get_or_init(|| {
                crate::kernel::row_norms_sq(self.arena.vectors(), self.arena.dim())
                    .ok()
                    .map(Vec::into_boxed_slice)
            })
            .as_deref()
    }

    /// テスト専用: ノルム² が既に計算・常駐済みか（遅延計算の検証用。Issue #3217）。
    #[cfg(test)]
    pub(crate) fn row_norms_sq_is_computed(&self) -> bool {
        self.row_norms_sq.get().is_some()
    }

    pub(crate) fn arena(&self) -> &VectorArena {
        &self.arena
    }

    pub(crate) fn metadata(&self) -> &[Vec<u8>] {
        &self.metadata
    }

    fn built_ctx(&self) -> &PolicyContext {
        &self.built_ctx
    }

    fn built_table_generation(&self) -> u64 {
        self.built_table_generation
    }

    /// [`crate::sql::scalar_index::ScalarIndex::build`] が索引の世代整合キー
    /// （`built_ctx`）をこのスナップショットから引き継ぐための crate 内公開
    /// アクセサ（Issue #473）。`built_ctx`（本モジュール内 private）と同じ値を
    /// 返す薄いラッパー。
    pub(crate) fn built_ctx_for_index(&self) -> &PolicyContext {
        &self.built_ctx
    }

    /// [`crate::sql::scalar_index::ScalarIndex::build`] が索引の世代整合キー
    /// （`built_table_generation`）をこのスナップショットから引き継ぐための
    /// crate 内公開アクセサ（Issue #473）。
    pub(crate) fn built_table_generation_for_index(&self) -> u64 {
        self.built_table_generation
    }

    /// キャッシュ容量判定用の概算バイト量（`arena` 本体＋`metadata` 複製の実バイト数）。
    fn approx_heap_bytes(&self) -> usize {
        let metadata_bytes: usize = self
            .metadata
            .iter()
            .map(|m| m.len().saturating_add(std::mem::size_of::<Vec<u8>>()))
            .fold(0usize, |acc, n| acc.saturating_add(n));
        // 行ノルム²（遅延確保）は初期化の有無によらず上限見積もりとして常に算入し、
        // 総量上限の判定が遅延確保で破れないようにする（Issue #3217）。
        let norms_bytes = self.arena.len().saturating_mul(std::mem::size_of::<f32>());
        self.arena
            .approx_heap_bytes()
            .saturating_add(metadata_bytes)
            .saturating_add(norms_bytes)
    }
}

/// [`SqlArenaCache`] の 1 エントリ。`table`・`ctx` の組がキャッシュキー
/// （`PolicyContext` は `Hash` を実装しないため `HashMap` ではなく `Vec` 線形走査で
/// 照合する。`core.rs::CacheEntry`/`DictCacheEntry`・
/// `sql::sparse_cache::CacheEntry` と同じ理由）。
struct SqlArenaCacheEntry {
    table: String,
    snapshot: Arc<SqlArenaSnapshot>,
    last_used: u64,
}

/// ロックが保護する可変状態（[`RwLock`] 内側）。
#[derive(Default)]
struct SqlArenaCacheState {
    entries: Vec<SqlArenaCacheEntry>,
}

/// SQL 表層（`sql::exec::execute_statement_with_cache`）専用の [`VectorArena`] 世代
/// 整合キャッシュ本体（モジュールドキュメント参照）。
pub(crate) struct SqlArenaCache {
    state: RwLock<SqlArenaCacheState>,
    seq: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    stale_evictions: AtomicU64,
    capacity_evictions: AtomicU64,
    fast_path_borrows: AtomicU64,
    full_rebuild_copies: AtomicU64,
}

impl SqlArenaCache {
    pub(crate) fn new() -> Self {
        Self {
            state: RwLock::new(SqlArenaCacheState::default()),
            seq: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stale_evictions: AtomicU64::new(0),
            capacity_evictions: AtomicU64::new(0),
            fast_path_borrows: AtomicU64::new(0),
            full_rebuild_copies: AtomicU64::new(0),
        }
    }

    /// キャッシュヒット後に借用高速経路（複製なし）を選んだことを記録する。
    /// 呼び出し元は `sql::exec::execute_statement_with_cache` のみ。
    pub(crate) fn record_fast_path_borrow(&self) {
        self.fast_path_borrows.fetch_add(1, Ordering::Relaxed);
    }

    /// キャッシュヒット後に可視全行の複製（`build_from_cached_rls_rows`）へ
    /// 落ちたことを記録する。呼び出し元は
    /// `sql::exec::execute_statement_with_cache` のみ。
    pub(crate) fn record_full_rebuild_copy(&self) {
        self.full_rebuild_copies.fetch_add(1, Ordering::Relaxed);
    }

    /// `(table, ctx)` に一致し、`read_txn` のスナップショットにおけるテーブル世代と
    /// 整合するエントリを探す。ロック毒化・世代読み取り失敗はいずれも「見つから
    /// なかった」として扱う（fail-closed。`core.rs::PrefilterCache::lookup` と同じ
    /// 方針）。
    ///
    /// `read_txn` は呼び出し元（`sql::exec::execute_statement_with_cache`）がこの
    /// クエリ全体で使う単一の read トランザクションそのものを渡す契約とする
    /// （新規トランザクションを開かない）。これにより、ここで読む世代は呼び出し元が
    /// これから走査する行集合と同一スナップショットのものになる。
    ///
    /// **世代不一致時の破棄条件（Issue #363 レビュー指摘対応・Cursor Bugbot 指摘。
    /// Issue #357・`sql::sparse_cache::SparseIndexCache::lookup` と同じ契約）**:
    /// `read_txn` は呼び出し元ごとに異なるスナップショット（古い可能性がある）で
    /// あり、`current_generation`（`read_txn` から読んだ世代）より新しいエントリが
    /// 存在し得る。そのエントリは「この `read_txn` の視点では使えない（ミス）」が、
    /// 真に stale なわけではなく、より新しいスナップショットから見る別の in-flight
    /// クエリにとっては依然有効な場合がある。そのためエントリを見つけ次第破棄は
    /// せず、`storage`（[`Self::insert`] と同様に新規 read トランザクションで
    /// 再読取する。並行書き込みとの競合検出用）から読んだ「真に最新の」世代と比較
    /// し、エントリがそれより厳密に古い場合（`built_table_generation <
    /// true_current_generation`）に限り stale と判定して破棄する。世代は単調増加の
    /// ため `built_table_generation` が真の最新世代を上回ることはない。
    pub(crate) fn lookup(
        &self,
        storage: &Storage,
        read_txn: &redb::ReadTransaction,
        table: &str,
        ctx: &PolicyContext,
    ) -> Option<Arc<SqlArenaSnapshot>> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.state.write().ok()?;
        // 世代はロック取得後に読み直す（`PrefilterCache::lookup` と同じ理由。ロック
        // 待機中の他スレッドの挿入を誤って「不一致」と判定しないため）。
        let current_generation = crate::catalog::table_generation_in_txn(read_txn, table).ok()?;
        let position = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.snapshot.built_ctx() == ctx)?;
        let built_generation = guard
            .entries
            .get(position)?
            .snapshot
            .built_table_generation();
        if built_generation == current_generation {
            let entry = guard.entries.get_mut(position)?;
            entry.last_used = seq;
            let snapshot = Arc::clone(&entry.snapshot);
            drop(guard);
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(snapshot);
        }
        // 不一致。`read_txn`（呼び出し元のスナップショット）視点ではミスだが、
        // 破棄してよいのはエントリが真に stale（最新世代より古い）と確認できた
        // 場合のみ（上記ドキュメント参照）。真の最新世代を読めない場合は破棄を
        // 諦める（fail-closed。古い可能性のある `read_txn` の世代だけを根拠に
        // 有効な可能性があるエントリを消さない）。
        let Ok(true_current_generation) = storage.table_generation(table) else {
            return None;
        };
        if built_generation < true_current_generation {
            guard.entries.remove(position);
            self.stale_evictions.fetch_add(1, Ordering::Relaxed);
        }
        None
    }

    /// 新規構築したスナップショットを挿入する。`storage` から新規に読んだテーブル
    /// 世代と `snapshot.built_table_generation()` が一致しない場合（並行書き込みで
    /// 挿入対象自身が既に古い）・世代を確認できない場合はキャッシュへの反映のみを
    /// 諦める（`sql::sparse_cache::SparseIndexCache::insert` と同じ fail-closed
    /// 契約。Issue #280 対応の踏襲）。
    ///
    /// **戻り値は常に `Arc<SqlArenaSnapshot>`（`Option` ではない）**（モジュール
    /// ドキュメント「fail-closed 契約」参照）。キャッシュへ反映できるかどうかに
    /// 関わらず、呼び出し元は返されたスナップショットを「このクエリのスナップ
    /// ショットから自分で構築した結果」としてそのまま使ってよい。
    pub(crate) fn insert(
        &self,
        storage: &Storage,
        table: &str,
        ctx: &PolicyContext,
        snapshot: SqlArenaSnapshot,
    ) -> Arc<SqlArenaSnapshot> {
        let snapshot = Arc::new(snapshot);
        self.misses.fetch_add(1, Ordering::Relaxed);

        let Ok(mut guard) = self.state.write() else {
            return snapshot;
        };

        let Ok(read_txn) = storage.db().begin_read() else {
            return snapshot;
        };
        let Ok(current_generation) = crate::catalog::table_generation_in_txn(&read_txn, table)
        else {
            return snapshot;
        };
        if snapshot.built_table_generation() != current_generation {
            return snapshot;
        }

        let own_bytes = snapshot.approx_heap_bytes();
        if own_bytes > MAX_SQL_ARENA_CACHE_TOTAL_BYTES {
            return snapshot;
        }

        if let Some(pos) = guard
            .entries
            .iter()
            .position(|e| e.table == table && e.snapshot.built_ctx() == ctx)
        {
            guard.entries.remove(pos);
        }

        // 挿入対象テーブルに限定して世代不一致エントリを破棄する（Issue #363
        // レビュー指摘対応・Cursor Bugbot 指摘: テーブルごとに世代カウンタが独立
        // している〔モジュールドキュメント参照〕ため、`current_generation` は
        // 挿入対象テーブル自身の世代であり、他テーブルのエントリと比較しても
        // 無意味。従来は全エントリへ一律適用しており、他テーブルの有効なエントリ
        // まで世代不一致と誤判定して巻き添え破棄していた）。
        let before = guard.entries.len();
        guard.entries.retain(|e| {
            e.table != table || e.snapshot.built_table_generation() == current_generation
        });
        let removed_stale = before.saturating_sub(guard.entries.len());
        if removed_stale > 0 {
            self.stale_evictions
                .fetch_add(removed_stale as u64, Ordering::Relaxed);
        }

        let mut total_bytes: usize = guard
            .entries
            .iter()
            .map(|e| e.snapshot.approx_heap_bytes())
            .fold(0usize, |acc, n| acc.saturating_add(n));
        while guard.entries.len() >= MAX_SQL_ARENA_CACHE_ENTRIES
            || total_bytes.saturating_add(own_bytes) > MAX_SQL_ARENA_CACHE_TOTAL_BYTES
        {
            let victim = guard
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(idx, _)| idx);
            let Some(idx) = victim else {
                return snapshot;
            };
            let removed = guard.entries.remove(idx);
            total_bytes = total_bytes.saturating_sub(removed.snapshot.approx_heap_bytes());
            self.capacity_evictions.fetch_add(1, Ordering::Relaxed);
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        guard.entries.push(SqlArenaCacheEntry {
            table: table.to_string(),
            snapshot: Arc::clone(&snapshot),
            last_used: seq,
        });
        snapshot
    }

    pub(crate) fn stats(&self) -> SqlArenaCacheStats {
        let entries = self.state.read().map(|g| g.entries.len()).unwrap_or(0);
        SqlArenaCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            stale_evictions: self.stale_evictions.load(Ordering::Relaxed),
            capacity_evictions: self.capacity_evictions.load(Ordering::Relaxed),
            entries,
            fast_path_borrows: self.fast_path_borrows.load(Ordering::Relaxed),
            full_rebuild_copies: self.full_rebuild_copies.load(Ordering::Relaxed),
        }
    }
}

/// `sql::exec::execute_statement_with_cache` へ渡すキャッシュアクセス束
/// （Issue #363）。`storage`（[`SqlArenaCache::insert`]・[`SqlArenaCache::lookup`]
/// の世代再読取用）と `cache` 本体を 1 引数へ束ねることで、`execute_statement` の
/// 引数数を clippy の `too_many_arguments` 閾値内に保つ
/// （`sql::sparse_cache::SparseCacheAccess` と同じ理由・構造）。
pub(crate) struct ArenaCacheAccess<'a> {
    pub(crate) storage: &'a Storage,
    pub(crate) cache: &'a SqlArenaCache,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::policy::PolicyContext;
    use crate::storage::Storage;
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    /// Issue #3217: ノルム² は初回で一度だけ計算され、以降は同じスライスを返す。
    #[test]
    fn row_norms_sq_is_computed_once_and_matches_kernel() {
        let path = unique_db_path("arena-cache-norms-once");
        let _guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs");
        let c = ctx("tenant-a");
        let snapshot = sample_snapshot(&storage, "docs", &c, 0);
        let first = snapshot.row_norms_sq().expect("norms");
        let second = snapshot.row_norms_sq().expect("norms again");
        assert!(std::ptr::eq(first, second));
        let expected =
            crate::kernel::row_norms_sq(snapshot.arena().vectors(), snapshot.arena().dim())
                .expect("kernel norms");
        assert_eq!(first, expected.as_slice());
        assert_eq!(first.len(), snapshot.arena().len());
    }

    fn ctx(tenant: &str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant")
    }

    fn create_table(storage: &Storage, name: &str) {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![
                    ColumnDef::new("path", ColumnType::Text, false),
                    ColumnDef::new("embedding", ColumnType::Vector(2), false),
                ],
            ))
            .expect("create table");
    }

    /// 空テーブルから構築した最小構成の `VectorArena` を使い、[`SqlArenaSnapshot`]
    /// を組み立てる（本モジュールの単体テストはキャッシュの世代整合ロジックのみを
    /// 検証対象とし、アリーナの中身は問わないため）。
    fn sample_snapshot(
        storage: &Storage,
        table: &str,
        ctx: &PolicyContext,
        generation: u64,
    ) -> SqlArenaSnapshot {
        let arena = VectorArena::build(storage, table).expect("build empty arena");
        SqlArenaSnapshot::new(arena, Vec::new(), ctx.clone(), generation)
    }

    #[test]
    fn lookup_hits_same_table_ctx_same_generation() {
        let path = unique_db_path("arena-cache-hit");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs");
        let cache = SqlArenaCache::new();
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let inserted = cache.insert(
            &storage,
            "docs",
            &c,
            sample_snapshot(&storage, "docs", &c, gen),
        );

        let read_txn2 = storage.db().begin_read().unwrap();
        let hit = cache.lookup(&storage, &read_txn2, "docs", &c);
        assert!(hit.is_some());
        assert!(Arc::ptr_eq(&hit.unwrap(), &inserted));
        assert_eq!(cache.stats().hits, 1);
    }

    #[test]
    fn lookup_misses_and_evicts_after_table_generation_bump() {
        let path = unique_db_path("arena-cache-stale");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs");
        let cache = SqlArenaCache::new();
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        cache.insert(
            &storage,
            "docs",
            &c,
            sample_snapshot(&storage, "docs", &c, gen),
        );

        let write_txn = storage.db().begin_write().unwrap();
        crate::catalog::bump_table_generation_in_txn(&write_txn, "docs").unwrap();
        crate::recovery::commit_boundary::commit(write_txn).unwrap();

        let read_txn2 = storage.db().begin_read().unwrap();
        let hit = cache.lookup(&storage, &read_txn2, "docs", &c);
        assert!(hit.is_none(), "stale entry must be evicted, not reused");
        assert_eq!(cache.stats().stale_evictions, 1);
    }

    #[test]
    fn lookup_misses_for_different_tenant_ctx() {
        let path = unique_db_path("arena-cache-tenant");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs");
        let cache = SqlArenaCache::new();
        let owner = ctx("tenant-a");
        let other = ctx("tenant-b");

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        cache.insert(
            &storage,
            "docs",
            &owner,
            sample_snapshot(&storage, "docs", &owner, gen),
        );

        let read_txn2 = storage.db().begin_read().unwrap();
        assert!(cache.lookup(&storage, &read_txn2, "docs", &other).is_none());
    }

    #[test]
    fn insert_does_not_cache_when_generation_already_advanced_but_still_returns_snapshot() {
        let path = unique_db_path("arena-cache-insert-stale");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs");
        let cache = SqlArenaCache::new();
        let c = ctx("tenant-a");

        // `built_table_generation` に、実際より 1 つ古い世代を渡す（並行書き込みで
        // 自身が既に stale になったことを模す）。
        let read_txn = storage.db().begin_read().unwrap();
        let stale_gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let write_txn = storage.db().begin_write().unwrap();
        crate::catalog::bump_table_generation_in_txn(&write_txn, "docs").unwrap();
        crate::recovery::commit_boundary::commit(write_txn).unwrap();

        let returned = cache.insert(
            &storage,
            "docs",
            &c,
            sample_snapshot(&storage, "docs", &c, stale_gen),
        );
        // 呼び出し元へはスナップショットそのものを返す（`SparseIndexCache` と同じ
        // 契約。モジュールドキュメント参照）。
        assert_eq!(returned.built_table_generation(), stale_gen);

        // ただしキャッシュへは反映されていない（次の lookup はミス）。
        let read_txn2 = storage.db().begin_read().unwrap();
        assert!(cache.lookup(&storage, &read_txn2, "docs", &c).is_none());
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn capacity_eviction_keeps_entries_within_limit() {
        let path = unique_db_path("arena-cache-capacity");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let cache = SqlArenaCache::new();
        let c = ctx("tenant-a");
        for i in 0..(MAX_SQL_ARENA_CACHE_ENTRIES + 1) {
            let table = format!("docs_{i}");
            create_table(&storage, &table);
            let read_txn = storage.db().begin_read().unwrap();
            let gen = crate::catalog::table_generation_in_txn(&read_txn, &table).unwrap();
            cache.insert(
                &storage,
                &table,
                &c,
                sample_snapshot(&storage, &table, &c, gen),
            );
        }
        let stats = cache.stats();
        assert!(stats.entries <= MAX_SQL_ARENA_CACHE_ENTRIES);
        assert!(stats.capacity_evictions >= 1);
    }

    #[test]
    fn insert_does_not_evict_valid_entries_of_other_tables() {
        // Issue #363 レビュー指摘対応（Cursor Bugbot 指摘）: `insert` の世代不一致
        // retain がテーブル横断で全エントリを見てしまうと、挿入対象テーブルの世代
        // （他テーブルとは無関係な値）とたまたま一致しない他テーブルの有効な
        // エントリまで誤って破棄してしまう。
        let path = unique_db_path("arena-cache-cross-table");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs_a");
        create_table(&storage, "docs_b");
        let cache = SqlArenaCache::new();
        let c = ctx("tenant-a");

        // docs_a へ挿入した後、書き込みで世代を進める（docs_a 自身の世代のみ）。
        let read_txn_a = storage.db().begin_read().unwrap();
        let gen_a = crate::catalog::table_generation_in_txn(&read_txn_a, "docs_a").unwrap();
        cache.insert(
            &storage,
            "docs_a",
            &c,
            sample_snapshot(&storage, "docs_a", &c, gen_a),
        );
        let write_txn = storage.db().begin_write().unwrap();
        crate::catalog::bump_table_generation_in_txn(&write_txn, "docs_a").unwrap();
        crate::recovery::commit_boundary::commit(write_txn).unwrap();

        // docs_b へ挿入する時点で docs_a のキャッシュエントリは docs_a 自身の
        // 世代とは既に不整合（bump 済み）だが、docs_b への insert がこれを
        // 巻き込んで破棄してはならない。
        let read_txn_b = storage.db().begin_read().unwrap();
        let gen_b = crate::catalog::table_generation_in_txn(&read_txn_b, "docs_b").unwrap();
        cache.insert(
            &storage,
            "docs_b",
            &c,
            sample_snapshot(&storage, "docs_b", &c, gen_b),
        );

        assert_eq!(
            cache.stats().entries,
            2,
            "docs_b insert must not evict docs_a's differently-generationed entry"
        );

        // docs_a への次のクエリは、依然として有効な自身のエントリをヒットできる
        // （世代が bump 済みなので実際にはミスするが、エントリ自体は残っている
        // ことを既に上のアサーションで確認済み。ここでは lookup がクラッシュせず
        // fail-closed に振る舞うことのみ追加確認する）。
        let read_txn_a2 = storage.db().begin_read().unwrap();
        assert!(cache.lookup(&storage, &read_txn_a2, "docs_a", &c).is_none());
    }

    #[test]
    fn lookup_does_not_delete_newer_entry_when_caller_snapshot_is_stale() {
        // Issue #363 レビュー指摘対応（Cursor Bugbot 指摘）: 呼び出し元の `read_txn`
        // が古いスナップショットの場合、そのスナップショット視点の世代と不一致と
        // いうだけで、より新しい（真に有効な）エントリを削除してはならない。
        let path = unique_db_path("arena-cache-old-snapshot");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs");
        let cache = SqlArenaCache::new();
        let c = ctx("tenant-a");

        // 古いスナップショット（in-flight クエリを模す）を先に開始しておく。
        let old_read_txn = storage.db().begin_read().unwrap();

        // その後、世代を 1 つ進めてから新しいスナップショットを挿入する（新しい
        // スナップショットの世代で構築・挿入）。
        let write_txn = storage.db().begin_write().unwrap();
        crate::catalog::bump_table_generation_in_txn(&write_txn, "docs").unwrap();
        crate::recovery::commit_boundary::commit(write_txn).unwrap();
        let fresh_read_txn = storage.db().begin_read().unwrap();
        let fresh_gen = crate::catalog::table_generation_in_txn(&fresh_read_txn, "docs").unwrap();
        cache.insert(
            &storage,
            "docs",
            &c,
            sample_snapshot(&storage, "docs", &c, fresh_gen),
        );
        assert_eq!(cache.stats().entries, 1);

        // 古いスナップショットからの lookup はミス（世代が違うので使えない）だが、
        // 真に新しい有効なエントリを削除してはならない。
        let miss = cache.lookup(&storage, &old_read_txn, "docs", &c);
        assert!(miss.is_none());
        assert_eq!(
            cache.stats().entries,
            1,
            "an old-snapshot lookup miss must not delete a genuinely newer, still-valid entry"
        );

        // 新しいスナップショットからの lookup は引き続きヒットする。
        let hit = cache.lookup(&storage, &fresh_read_txn, "docs", &c);
        assert!(hit.is_some());
    }
}
