//! `sql::exec::execute_statement` の hybrid 実行（`ORDER BY hybrid_rrf(...)` /
//! `HYBRID(...)`）が参照する [`crate::sparse::SparseIndex`]（BM25 語彙・統計）の
//! テーブル世代整合キャッシュ（Issue #357）。
//!
//! **背景**: hybrid 実行は毎クエリ RLS 可視行走査中に本文テキストを疎コーパスへ
//! 複製蓄積し、`SparseIndex::build`（語彙 `BTreeMap` 構築・BM25 統計計算）を
//! 再実行していた。テーブル・可視集合が無変化でも build コストがクエリごとに
//! 発生する。本キャッシュは `(table, ctx, text_column_index)` 単位でこれを
//! 再利用する。
//!
//! **キーと世代**: `core.rs::PrefilterCache`（TASK-169）・`core.rs::DictionaryCache`
//! （TASK-109・PLAN-5）と同じ設計を踏襲する。キーは `(table, ctx,
//! text_column_index)` の完全一致（`PolicyContext` はテナント境界を含むため、
//! 他テナントのコーパスを構造的に参照不能にする。`text_column_index`
//! （`Ranking::Hybrid` が保持する、hybrid 本文として使う TEXT 列のインデックス。
//! `hybrid_rrf(embedding, '<vec>', <col>, '<query>')` の 3 引数目に対応）が
//! キーに含まれない場合、同一テーブルの異なる TEXT 列を本文に指定する 2 つの
//! hybrid クエリが互いのキャッシュを誤ヒットし、片方の列で構築した索引をもう
//! 片方の列のクエリへ提供してしまう。コーパス内容が問い合わせごとに異なる
//! パラメータであるため、キーへ含める）。世代は `catalog::table_generation_in_txn`
//! （テーブル単位。粒度の設計判断は
//! `docs/design/table-generation-rejection-granularity.md` 参照）を使う。
//! グローバル世代ではなくテーブル世代を使うことで、無関係な他テーブルへの
//! 書き込みでは失効しない。
//!
//! **適用条件**: 呼び出し元（`sql::exec::execute_statement`）は、`bound.
//! metadata_filters`・`bound.expr_filters` がともに空の hybrid クエリに限り本
//! キャッシュを経由させる。疎コーパス（`DocId` = アリーナのスロット番号）は
//! SCALAR 事前フィルタを通過した行にのみ割り当てられるため、フィルタが無い
//! 場合に限り「RLS 可視行のうち本文非 NULL の全行」というクエリ非依存のコーパスが
//! 成立し、同一世代内であればスロット番号の割当も含めて完全に再現される
//! （redb の走査順・RLS 判定の純粋性に依存する不変条件。`sql/exec.rs` の
//! 呼び出し箇所のコメント参照）。フィルタ付きクエリ・DISTANCE 専用クエリは
//! 呼び出し元が本キャッシュへ到達させない。
//!
//! **BM25 パラメータはキーに含めない（Issue #2121・SEARCH-13 のポインタ）**: `HYBRID` の
//! `k1`／`b` はクエリ時のスコア計算だけに作用し、索引（文書統計・postings）には焼き込まない。
//! このためキー・世代に含めず、値が違うクエリ間で同じ索引を再構築なしで再利用する。
//!
//! **テキスト処理の設定は世代に含める（Issue #2130・SEARCH-16 のポインタ。BM25 の `k1`／`b` とは
//! 逆）**: ストップワード・同義語は索引（語彙・postings）に焼き込まれるため、各エントリは
//! 構築時の [`crate::sparse::TextAnalysis::analysis_id`] を持ち、[`Self::lookup`] は一致しない
//! エントリを stale として破棄する。設定は起動時にだけ決まりプロセス内メモリの索引は再起動で
//! 消えるが、`EngineCore::with_text_analysis` での差し替えに備えた多層防御である。
//!
//! **fail-closed 契約（[`Self::lookup`] と [`Self::insert`] で非対称）**:
//! - [`Self::lookup`] は世代不一致・ロック毒化・世代読み取り失敗のいずれも
//!   「見つからなかった」として扱う（`PrefilterCache::lookup` と同じ方針。
//!   古い可能性のある索引でクエリへ応答する経路を作らない）。
//! - [`Self::insert`] は他の 2 キャッシュ（Issue #280 で `None` 統一済み）とは
//!   意図的に異なる契約を持つ。挿入対象自身が既に古い場合・ロック毒化時は
//!   キャッシュへ**反映しない**が、呼び出し元へは常に構築済みの `Arc<SparseIndex>`
//!   を返す。呼び出し元がこの索引を構築したのは自分自身のクエリの `read_txn`
//!   （単一スナップショット）上であり、そのスナップショットの中でのみ使う限り
//!   stale にはなり得ない（`PrefilterCache`/`DictionaryCache` が「キャッシュから
//!   古い可能性のあるものを取り出して使う」経路を塞ぐのに対し、本関数が禁じるのは
//!   「キャッシュへ古い索引を常駐させる」ことのみ）。
//!
//! **スコアスクラッチプールとの関係（Issue #546）**: 本キャッシュが保持する
//! `Arc<SparseIndex>` は、`SparseIndex::search`/`search_within`/`score_within`
//! が呼び出しのたびに新規確保していたスコアアキュムレータ（`acc: Vec<f64>`）を
//! 索引の寿命内で再利用するスクラッチプールを内包する（`sparse.rs::
//! SparseIndex::scratch_pool` のドキュメンテーションコメント参照）。世代不一致で
//! この `Arc` ごと失効・破棄されればスクラッチプールも一緒に破棄されるため、
//! キャッシュの世代整合契約（上記）がスクラッチの寿命管理も兼ねる。
//! [`SparseIndex::approx_heap_bytes`] はこのプールの決定的な上限値を含むため、
//! `Self::insert` の容量判定（`MAX_SPARSE_CACHE_TOTAL_BYTES`）はスクラッチ分も
//! 織り込み済みである。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::policy::PolicyContext;
use crate::sparse::SparseIndex;
use crate::storage::Storage;

/// [`SparseIndexCache`] のエントリ数上限（`core.rs::MAX_PREFILTER_CACHE_ENTRIES`
/// と同じ DoS 対策方針。少数テーブル × 少数ポリシーの組み合わせを十分満たしつつ、
/// 線形走査コストが問題にならない桁に留める）。
const MAX_SPARSE_CACHE_ENTRIES: usize = 32;

/// [`SparseIndexCache`] が保持する [`SparseIndex`] 群の概算バイト量の合計上限
/// （`core.rs::MAX_PREFILTER_CACHE_TOTAL_BYTES` と同じ桁に揃える）。
const MAX_SPARSE_CACHE_TOTAL_BYTES: usize = crate::arena::MAX_ARENA_TOTAL_BYTES;

/// [`SparseIndexCache`] の観測用統計（テナント ID・行 ID 等の機微情報は含まない。
/// `core.rs::PrefilterCacheStats` と同方針）。`EngineCore::sparse_index_cache_stats`
/// からのみ公開する（`VectorCore` trait には載せない固有 API）。
#[derive(Debug, Clone, Copy, Default)]
pub struct SparseIndexCacheStats {
    /// キャッシュヒット数（世代整合まで確認できた再利用）。
    pub hits: u64,
    /// `SparseIndexCache::insert` の呼び出し回数（未登録、または世代不一致で
    /// 破棄した後に新規索引を構築してキャッシュへ挿入を試みた回数）。lookup が
    /// ミスしても、対象クエリの疎コーパスが空（`sparse_docs.is_empty()`）で
    /// `SparseIndex::build` 自体を呼ばない場合や、`precision` の完全性ゲートで
    /// DISTANCE 検索自体を実行しない場合は `insert` を呼ばないため計上されない
    /// （hits + misses が「lookup を実行した回数」と一致するとは限らない）。
    pub misses: u64,
    /// 世代不一致による破棄回数（lookup 時の stale 検出 + insert 時の一括破棄）。
    pub stale_evictions: u64,
    /// 容量上限超過による LRU 追い出し回数。
    pub capacity_evictions: u64,
    /// 現在キャッシュが保持しているエントリ数。
    pub entries: usize,
}

/// [`SparseIndexCache`] の 1 エントリ。`table`・`ctx`・`text_column_index` の組が
/// キャッシュキー（`PolicyContext` は `Hash` を実装しないため `HashMap` ではなく
/// `Vec` 線形走査で照合する。`core.rs::CacheEntry`/`DictCacheEntry` と同じ理由。
/// `text_column_index` をキーへ含める理由はモジュールドキュメント参照）。
struct CacheEntry {
    table: String,
    ctx: PolicyContext,
    text_column_index: usize,
    index: Arc<SparseIndex>,
    /// 構築時のテキスト処理設定の世代 ID（Issue #2130）。
    analysis_id: u64,
    built_generation: u64,
    approx_bytes: usize,
    /// LRU 追い出し判定用の単調シーケンス（アクセスのたびに更新）。
    last_used: u64,
}

/// ロックが保護する可変状態（[`RwLock`] 内側）。
#[derive(Default)]
struct CacheState {
    entries: Vec<CacheEntry>,
}

/// `sql::exec::execute_statement` の hybrid 実行が参照する [`SparseIndex`] の
/// テーブル世代整合キャッシュ本体（モジュールドキュメント参照）。
pub(crate) struct SparseIndexCache {
    state: RwLock<CacheState>,
    seq: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    stale_evictions: AtomicU64,
    capacity_evictions: AtomicU64,
}

impl SparseIndexCache {
    pub(crate) fn new() -> Self {
        Self {
            state: RwLock::new(CacheState::default()),
            seq: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stale_evictions: AtomicU64::new(0),
            capacity_evictions: AtomicU64::new(0),
        }
    }

    /// `(table, ctx, text_column_index)` に一致し、`read_txn` のスナップショットに
    /// おけるテーブル世代と整合するエントリを探す。ロック毒化・世代読み取り失敗は
    /// いずれも「見つからなかった」として扱う（fail-closed。
    /// `core.rs::PrefilterCache::lookup` と同じ方針）。
    ///
    /// `read_txn` は呼び出し元（`sql::exec::execute_statement`）がこのクエリ全体で
    /// 使う単一の read トランザクションそのものを渡す契約とする（新規トランザクション
    /// を開かない）。これにより、ここで読む世代は呼び出し元がこれから走査する行集合と
    /// 同一スナップショットのものになる。
    ///
    /// **世代不一致時の破棄条件（Issue #357 レビュー指摘対応・codex-review P2・
    /// Cursor Bugbot 指摘）**: `read_txn` は呼び出し元ごとに異なるスナップショット
    /// （古い可能性がある）であり、`current_generation`（`read_txn` から読んだ世代）
    /// より新しいエントリが存在し得る。そのエントリは「この `read_txn` の視点では
    /// 使えない（ミス）」が、真に stale なわけではなく、より新しいスナップショット
    /// から見る別の in-flight クエリにとっては依然有効な場合がある。そのため
    /// エントリを見つけ次第破棄はせず、`storage`（`SparseIndexCache::insert` と
    /// 同様に新規 read トランザクションで再読取する。並行書き込みとの競合検出用）
    /// から読んだ「真に最新の」世代と比較し、エントリがそれより厳密に古い場合
    /// （`entry.built_generation < true_current_generation`）に限り stale と判定して
    /// 破棄する。世代は単調増加のため `entry.built_generation` が真の最新世代を
    /// 上回ることはない。
    pub(crate) fn lookup(
        &self,
        storage: &Storage,
        read_txn: &redb::ReadTransaction,
        table: &str,
        ctx: &PolicyContext,
        text_column_index: usize,
        analysis_id: u64,
    ) -> Option<Arc<SparseIndex>> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.state.write().ok()?;
        // 世代はロック取得後に読み直す（`PrefilterCache::lookup` と同じ理由。ロック
        // 待機中の他スレッドの挿入を誤って「不一致」と判定しないため）。
        let current_generation = crate::catalog::table_generation_in_txn(read_txn, table).ok()?;
        let position = guard.entries.iter().position(|e| {
            e.table == table && &e.ctx == ctx && e.text_column_index == text_column_index
        })?;
        // テキスト処理設定が違う索引は、世代が合っていても使わず破棄する（Issue #2130）。
        if guard.entries.get(position)?.analysis_id != analysis_id {
            guard.entries.remove(position);
            self.stale_evictions.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let built_generation = guard.entries.get(position)?.built_generation;
        if built_generation == current_generation {
            let entry = guard.entries.get_mut(position)?;
            entry.last_used = seq;
            let index = Arc::clone(&entry.index);
            drop(guard);
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(index);
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

    /// 新規構築した索引を挿入する。`built_generation` は呼び出し元が構築に使った
    /// `read_txn` のスナップショットにおけるテーブル世代（[`Self::lookup`] に渡した
    /// のと同じ `read_txn` から `catalog::table_generation_in_txn` で読んだ値）を渡す
    /// 契約とする。
    ///
    /// **戻り値は常に `Arc<SparseIndex>`（`Option` ではない）**（モジュール
    /// ドキュメント「fail-closed 契約」参照）。キャッシュへ反映できるかどうかに
    /// 関わらず、呼び出し元は返された索引を「このクエリのスナップショットから
    /// 自分で構築した索引」としてそのまま使ってよい。`storage` は挿入直前の
    /// 「現在の」テーブル世代を新規 read トランザクションで再読取するために使う
    /// （並行書き込みとの競合検出。`catalog.rs::Storage::table_generation`
    /// ドキュメント参照）。世代が一致しない、またはロック毒化・世代読み取り失敗の
    /// 場合はキャッシュへの反映のみを諦め、`index` をそのまま呼び出し元へ返す
    /// （常駐させない・古い索引を後続クエリへ渡さない、の両方を満たす）。
    pub(crate) fn insert(
        &self,
        storage: &Storage,
        table: &str,
        ctx: &PolicyContext,
        text_column_index: usize,
        index: SparseIndex,
        built_generation: u64,
    ) -> Arc<SparseIndex> {
        let index = Arc::new(index);
        self.misses.fetch_add(1, Ordering::Relaxed);

        let Ok(mut guard) = self.state.write() else {
            // ロック毒化時は世代整合を判定できないため常駐させない（fail-closed）。
            // 呼び出し元へは構築済みの索引をそのまま返す（モジュールドキュメント参照）。
            return index;
        };

        let Ok(current_generation) = storage.table_generation(table) else {
            return index;
        };
        if built_generation != current_generation {
            // 並行書き込みで挿入対象自身が既に古い。キャッシュへは反映しないが、
            // 呼び出し元の今回のクエリのスナップショットとしては整合しているため
            // そのまま返す（モジュールドキュメント参照）。
            return index;
        }

        let own_bytes = index.approx_heap_bytes();
        if own_bytes > MAX_SPARSE_CACHE_TOTAL_BYTES {
            // 単体で総量上限を超える索引は常駐させない（DoS 対策）。
            return index;
        }

        // 同一 (table, ctx, text_column_index) キーの既存エントリは挿入前に取り除く
        // （`PrefilterCache::insert` と同じ理由。重複登録による無駄な容量消費を
        // 避ける）。
        if let Some(pos) = guard.entries.iter().position(|e| {
            e.table == table && &e.ctx == ctx && e.text_column_index == text_column_index
        }) {
            guard.entries.remove(pos);
        }

        // 現在世代と不整合なエントリを先に破棄する。テーブルごとに世代カウンタが
        // 独立している（モジュールドキュメント「キーと世代」参照）ため、
        // `current_generation` は挿入対象テーブル自身の世代であり、他テーブルの
        // エントリと比較しても無意味（Issue #357 レビュー指摘対応・codex-review
        // P2・Cursor Bugbot 指摘: 従来は `built_generation == current_generation`
        // を全エントリへ一律適用しており、他テーブルの有効なエントリまで
        // 世代不一致と誤判定して破棄していた）。同一テーブルのエントリに限定して
        // 世代不一致を判定する。
        let before = guard.entries.len();
        guard
            .entries
            .retain(|e| e.table != table || e.built_generation == current_generation);
        let removed_stale = before.saturating_sub(guard.entries.len());
        if removed_stale > 0 {
            self.stale_evictions
                .fetch_add(removed_stale as u64, Ordering::Relaxed);
        }

        // それでも件数・総量が上限を超えるなら `last_used` 最小から追い出す（LRU）。
        let mut total_bytes: usize = guard
            .entries
            .iter()
            .map(|e| e.approx_bytes)
            .fold(0usize, |acc, n| acc.saturating_add(n));
        while guard.entries.len() >= MAX_SPARSE_CACHE_ENTRIES
            || total_bytes.saturating_add(own_bytes) > MAX_SPARSE_CACHE_TOTAL_BYTES
        {
            let victim = guard
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(idx, _)| idx);
            let Some(idx) = victim else {
                // これ以上追い出せるエントリがない（空）のに超過している場合は、
                // 挿入自体を諦めてキャッシュを汚さない。
                return index;
            };
            let removed = guard.entries.remove(idx);
            total_bytes = total_bytes.saturating_sub(removed.approx_bytes);
            self.capacity_evictions.fetch_add(1, Ordering::Relaxed);
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        guard.entries.push(CacheEntry {
            table: table.to_string(),
            ctx: ctx.clone(),
            text_column_index,
            index: Arc::clone(&index),
            analysis_id: index.analysis_id(),
            built_generation,
            approx_bytes: own_bytes,
            last_used: seq,
        });
        index
    }

    pub(crate) fn stats(&self) -> SparseIndexCacheStats {
        let entries = self.state.read().map(|g| g.entries.len()).unwrap_or(0);
        SparseIndexCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            stale_evictions: self.stale_evictions.load(Ordering::Relaxed),
            capacity_evictions: self.capacity_evictions.load(Ordering::Relaxed),
            entries,
        }
    }
}

/// `sql::exec::execute_statement` へ渡すキャッシュアクセス束（TASK: Issue #357）。
/// `storage`（[`SparseIndexCache::insert`] の世代再読取用）と `cache` 本体を 1
/// 引数へ束ねることで、`execute_statement` の引数数を clippy の
/// `too_many_arguments` 閾値内に保つ。`execute_statement` が `pub fn` であるのに
/// 合わせ、構造体自体も `pub`（フィールドは `pub(crate)`。`storage`/`cache` の型
/// （[`Storage`]・[`SparseIndexCache`]）は crate 外から到達できないため、この型を
/// crate 外で構築・分解することはできない）。
pub struct SparseCacheAccess<'a> {
    pub(crate) storage: &'a Storage,
    pub(crate) cache: &'a SparseIndexCache,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::policy::PolicyContext;
    use crate::storage::Storage;
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    fn ctx(tenant: &str) -> PolicyContext {
        PolicyContext::new(tenant).expect("valid tenant")
    }

    fn create_table(storage: &Storage, name: &str) {
        storage
            .create_table(&TableSchema::new(
                name,
                vec![ColumnDef::new("path", ColumnType::Text, false)],
            ))
            .expect("create table");
    }

    fn sample_index() -> SparseIndex {
        SparseIndex::build(&[(1u64, "alpha beta"), (2u64, "beta gamma")]).unwrap()
    }

    #[test]
    fn lookup_hits_same_table_ctx_same_generation() {
        let path = unique_db_path("sparse-cache-hit");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs");
        let cache = SparseIndexCache::new();
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let inserted = cache.insert(&storage, "docs", &c, 0, sample_index(), gen);

        let read_txn2 = storage.db().begin_read().unwrap();
        let hit = cache.lookup(&storage, &read_txn2, "docs", &c, 0, 0);
        assert!(hit.is_some());
        assert!(Arc::ptr_eq(&hit.unwrap(), &inserted));
        assert_eq!(cache.stats().hits, 1);
    }

    #[test]
    fn lookup_misses_and_evicts_after_table_generation_bump() {
        let path = unique_db_path("sparse-cache-stale");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs");
        let cache = SparseIndexCache::new();
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        cache.insert(&storage, "docs", &c, 0, sample_index(), gen);

        // 対象テーブルへの書き込みでテーブル世代を進める（fail-closed の失効確認）。
        let write_txn = storage.db().begin_write().unwrap();
        crate::catalog::bump_table_generation_in_txn(&write_txn, "docs").unwrap();
        crate::recovery::commit_boundary::commit(write_txn).unwrap();

        let read_txn2 = storage.db().begin_read().unwrap();
        let hit = cache.lookup(&storage, &read_txn2, "docs", &c, 0, 0);
        assert!(hit.is_none(), "stale entry must be evicted, not reused");
        assert_eq!(cache.stats().stale_evictions, 1);
    }

    #[test]
    fn lookup_misses_for_different_tenant_ctx() {
        let path = unique_db_path("sparse-cache-tenant");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs");
        let cache = SparseIndexCache::new();
        let owner = ctx("tenant-a");
        let other = ctx("tenant-b");

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        cache.insert(&storage, "docs", &owner, 0, sample_index(), gen);

        let read_txn2 = storage.db().begin_read().unwrap();
        assert!(cache
            .lookup(&storage, &read_txn2, "docs", &other, 0, 0)
            .is_none());
    }

    #[test]
    fn lookup_misses_for_different_text_column_index_same_table_and_ctx() {
        // 同一テーブル・同一 ctx でも、hybrid 本文として使う TEXT 列
        // （`text_column_index`）が異なれば別クエリ・別コーパスであり、キャッシュを
        // 取り違えてはならない（モジュールドキュメント「キーと世代」参照。異なる列
        // で構築した索引を誤って提供すると、コーパス内容自体が異なるため誤った
        // ハイブリッドスコアを返しかねない）。
        let path = unique_db_path("sparse-cache-text-column");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs");
        let cache = SparseIndexCache::new();
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        // text_column_index = 1（例: body 列）で挿入する。
        cache.insert(&storage, "docs", &c, 1, sample_index(), gen);

        // text_column_index = 2（例: title 列）での lookup はミスでなければならない。
        let read_txn2 = storage.db().begin_read().unwrap();
        assert!(
            cache
                .lookup(&storage, &read_txn2, "docs", &c, 2, 0)
                .is_none(),
            "a different text_column_index must not hit the entry built for another column"
        );
        // 同一 text_column_index (=1) の lookup は引き続きヒットする。
        let read_txn3 = storage.db().begin_read().unwrap();
        assert!(cache
            .lookup(&storage, &read_txn3, "docs", &c, 1, 0)
            .is_some());
    }

    #[test]
    fn insert_does_not_cache_when_generation_already_advanced_but_still_returns_index() {
        let path = unique_db_path("sparse-cache-insert-stale");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs");
        let cache = SparseIndexCache::new();
        let c = ctx("tenant-a");

        // `built_generation` に、実際より 1 つ古い世代を渡す（並行書き込みで自身が
        // 既に stale になったことを模す）。
        let read_txn = storage.db().begin_read().unwrap();
        let stale_gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        let write_txn = storage.db().begin_write().unwrap();
        crate::catalog::bump_table_generation_in_txn(&write_txn, "docs").unwrap();
        crate::recovery::commit_boundary::commit(write_txn).unwrap();

        let returned = cache.insert(&storage, "docs", &c, 0, sample_index(), stale_gen);
        // 呼び出し元へは索引そのものを返す（Issue #280 の他 2 キャッシュと異なり
        // `None` にしない。モジュールドキュメント参照）。
        assert!(returned.approx_heap_bytes() > 0);

        // ただしキャッシュへは反映されていない（次の lookup はミス）。
        let read_txn2 = storage.db().begin_read().unwrap();
        assert!(cache
            .lookup(&storage, &read_txn2, "docs", &c, 0, 0)
            .is_none());
        assert_eq!(cache.stats().entries, 0);
    }

    #[test]
    fn capacity_eviction_keeps_entries_within_limit() {
        let path = unique_db_path("sparse-cache-capacity");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let cache = SparseIndexCache::new();
        let c = ctx("tenant-a");
        for i in 0..(MAX_SPARSE_CACHE_ENTRIES + 1) {
            let table = format!("docs_{i}");
            create_table(&storage, &table);
            let read_txn = storage.db().begin_read().unwrap();
            let gen = crate::catalog::table_generation_in_txn(&read_txn, &table).unwrap();
            cache.insert(&storage, &table, &c, 0, sample_index(), gen);
        }
        let stats = cache.stats();
        assert!(stats.entries <= MAX_SPARSE_CACHE_ENTRIES);
        assert!(stats.capacity_evictions >= 1);
    }

    #[test]
    fn insert_does_not_evict_valid_entries_of_other_tables() {
        // Issue #357 レビュー指摘対応（codex-review P2・Cursor Bugbot 指摘）:
        // `insert` の世代不一致 retain がテーブル横断で全エントリを見てしまうと、
        // 挿入対象テーブルの世代（他テーブルとは無関係な値）とたまたま一致しない
        // 他テーブルの有効なエントリまで誤って破棄してしまう。
        let path = unique_db_path("sparse-cache-cross-table");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs_a");
        create_table(&storage, "docs_b");
        let cache = SparseIndexCache::new();
        let c = ctx("tenant-a");

        // docs_a へ挿入した後、書き込みで世代を進める（docs_a 自身の世代のみ）。
        let read_txn_a = storage.db().begin_read().unwrap();
        let gen_a = crate::catalog::table_generation_in_txn(&read_txn_a, "docs_a").unwrap();
        cache.insert(&storage, "docs_a", &c, 0, sample_index(), gen_a);
        let write_txn = storage.db().begin_write().unwrap();
        crate::catalog::bump_table_generation_in_txn(&write_txn, "docs_a").unwrap();
        crate::recovery::commit_boundary::commit(write_txn).unwrap();

        // docs_b へ挿入する時点で docs_a のキャッシュエントリは docs_a 自身の
        // 世代とは既に不整合（bump 済み）だが、docs_b への insert がこれを
        // 巻き込んで破棄してはならない。
        let read_txn_b = storage.db().begin_read().unwrap();
        let gen_b = crate::catalog::table_generation_in_txn(&read_txn_b, "docs_b").unwrap();
        cache.insert(&storage, "docs_b", &c, 0, sample_index(), gen_b);

        assert_eq!(
            cache.stats().entries,
            2,
            "docs_b insert must not evict docs_a's differently-generationed entry"
        );
    }

    #[test]
    fn lookup_does_not_delete_newer_entry_when_caller_snapshot_is_stale() {
        // Issue #357 レビュー指摘対応（Cursor Bugbot 指摘）: 呼び出し元の
        // `read_txn` が古いスナップショットの場合、そのスナップショット視点の
        // 世代と不一致というだけで、より新しい（真に有効な）エントリを
        // 削除してはならない。
        let path = unique_db_path("sparse-cache-old-snapshot");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs");
        let cache = SparseIndexCache::new();
        let c = ctx("tenant-a");

        // 古いスナップショット（in-flight クエリを模す）を先に開始しておく。
        let old_read_txn = storage.db().begin_read().unwrap();

        // その後、世代を 1 つ進めてから新しい索引を挿入する（新しいスナップ
        // ショットの世代で構築・挿入）。
        let write_txn = storage.db().begin_write().unwrap();
        crate::catalog::bump_table_generation_in_txn(&write_txn, "docs").unwrap();
        crate::recovery::commit_boundary::commit(write_txn).unwrap();
        let fresh_read_txn = storage.db().begin_read().unwrap();
        let fresh_gen = crate::catalog::table_generation_in_txn(&fresh_read_txn, "docs").unwrap();
        cache.insert(&storage, "docs", &c, 0, sample_index(), fresh_gen);
        assert_eq!(cache.stats().entries, 1);

        // 古いスナップショットからの lookup はミス（世代が違うので使えない）だが、
        // 真に新しい有効なエントリを削除してはならない。
        let miss = cache.lookup(&storage, &old_read_txn, "docs", &c, 0, 0);
        assert!(miss.is_none());
        assert_eq!(
            cache.stats().entries,
            1,
            "an old-snapshot lookup miss must not delete a genuinely newer, still-valid entry"
        );

        // 新しいスナップショットからの lookup は引き続きヒットする。
        let hit = cache.lookup(&storage, &fresh_read_txn, "docs", &c, 0, 0);
        assert!(hit.is_some());
    }

    #[test]
    fn lookup_evicts_entry_built_under_a_different_text_analysis() {
        let path = unique_db_path("sparse-cache-analysis");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "docs");
        let cache = SparseIndexCache::new();
        let c = ctx("tenant-a");
        let analysis =
            Arc::new(crate::sparse::TextAnalysis::from_lists(Some("the"), None).expect("analysis"));
        let id = analysis.analysis_id();
        let built = SparseIndex::build_with_analysis(&[(1u64, "the cat")], analysis).unwrap();

        let read_txn = storage.db().begin_read().unwrap();
        let gen = crate::catalog::table_generation_in_txn(&read_txn, "docs").unwrap();
        cache.insert(&storage, "docs", &c, 0, built, gen);

        let read_txn2 = storage.db().begin_read().unwrap();
        // 既定の設定（ID 0）での lookup は外れ、エントリは stale として破棄される。
        assert!(cache
            .lookup(&storage, &read_txn2, "docs", &c, 0, 0)
            .is_none());
        assert_eq!(cache.stats().stale_evictions, 1);
        assert_eq!(cache.stats().entries, 0);
        assert_eq!(cache.stats().hits, 0);
        assert_ne!(id, 0);
    }
}
