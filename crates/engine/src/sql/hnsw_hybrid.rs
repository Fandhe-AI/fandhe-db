//! hybrid 密側再取得ループ（`crate::hybrid::hybrid_search`／`hybrid_search_boosted`
//! の `dense_fetch_k` 倍増ループ）への HNSW 結線アダプタ（Issue #410・親 Issue #402・
//! 前提 #408〜#409。対応ビヘイビア: CORE-9・CORE-10・TASK-132・SEARCH-1・SEARCH-3）。
//!
//! `sql::hnsw_cache`（#408・#409）は SQL 表層のフィルタなし `Ranking::Distance`
//! クエリにのみ結線されており、hybrid の密側は生の `&dyn SearchProvider`
//! （常に全件 brute-force）のまま対象外だった（`sql::hnsw_cache` モジュール
//! ドキュメント「適用条件」節参照）。本モジュールは `sql::exec::
//! execute_statement_with_cache` の `Ranking::Hybrid` 分岐からのみ構築される
//! `SearchProvider` アダプタとして、`hybrid.rs` 自体・`SearchProvider` trait を
//! 一切変更せずに密側の索引経路を接続する。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::arena::VectorArena;
use crate::kernel::{CandidateHit, KernelError, SearchInput, SearchProvider};
use crate::sql::hnsw_cache::{
    search_prepared_resumable, HnswCacheAccess, HnswResumeState, PreparedHnswSearch,
};

/// hybrid 密側再取得ループ専用の `SearchProvider` アダプタ。
///
/// `sql::exec::execute_statement_with_cache` がクエリ開始時に一度だけ
/// `sql::hnsw_cache::prepare_full_visible`／`prepare_subset` を実行した結果
/// （`prepared`）を保持して構築する。`hybrid.rs` の密側再取得ループは同一
/// `SearchInput`（`ids`／`vectors` は不変・`k`（`dense_fetch_k`）のみ倍増）で
/// `provider.search` を複数ラウンド呼ぶ契約のため（`hybrid.rs::hybrid_search_boosted`
/// のループ本体参照）、本アダプタは呼ばれるたびに `prepared` を再利用して
/// `search_prepared` を呼ぶだけで済み、`Overlay::compute`／`IndexedBase::build`
/// （どちらも `k` に依存しない O(N) 相当の重い処理）はクエリ全体で 1 回に抑えられる。
///
/// # fail-closed な受理条件
///
/// [`SearchProvider::search`] は、渡された `input` がこのアダプタを構築した際の
/// クエリスナップショット（`arena`／`slot_ids`）と**同一のバッファ**（ポインタ・
/// 長さが一致）を参照している場合に限り索引経路を使う。1 つでも外れれば `inner`
/// （呼び出し元が元々使っていた provider。常に全件 brute-force と同じ意味論）へ
/// そのまま委譲する（別の可視集合・別の索引済みノード集合を索引経由で答えて
/// しまうことを構造的に防ぐ。RLS 相当のテナント境界は `prepared` 自体が
/// `(table, ctx)` の可視アリーナからしか解決されない `sql::hnsw_cache` の既存
/// fail-closed 契約に依拠する。`hybrid.rs` 側の可視 id 検証（`core::
/// provider_result_is_valid`・`HybridError::ProviderResultRejected`）も従来どおり
/// 全ラウンドに適用され続ける多層防御）。
pub(crate) struct HnswDenseProvider<'a> {
    access: &'a HnswCacheAccess<'a>,
    arena: &'a VectorArena,
    slot_ids: &'a [u64],
    inner: &'a dyn SearchProvider,
    prepared: PreparedHnswSearch,
    /// このアダプタが実際に索引経路（`受理条件`を満たしたラウンド）を通した回数。
    /// クエリ終了時に [`Self::finish`] で `HnswIndexCache` の統計へ反映する。
    rounds: AtomicU64,
    /// hybrid 密側再取得ループの破棄候補ヒープ保持型再開状態（Issue #505）。
    /// `SearchProvider::search` が `&self` シグネチャのため `Mutex` で包む
    /// （`RefCell` は `SearchProvider: Send + Sync`〔`hybrid.rs` が並列実行の
    /// 対象として要求〕を満たせない）。受理条件を満たさないラウンド（`inner`
    /// へ委譲）は本状態に一切触れない。2 ラウンド目以降のみ `Some` になる
    /// （1 ラウンド目は `search_prepared_resumable` 内で新規状態を構築する）。
    resume: Mutex<Option<HnswResumeState>>,
}

impl<'a> HnswDenseProvider<'a> {
    /// `access`・`prepared`（解決済みの索引・オーバーレイ、または `FullScan`
    /// 判定）・このクエリのアリーナ／スロット番号・`inner`（索引を使わない場合の
    /// 委譲先。既存の hybrid 経路がそのまま使っていた provider）から構築する。
    pub(crate) fn new(
        access: &'a HnswCacheAccess<'a>,
        arena: &'a VectorArena,
        slot_ids: &'a [u64],
        inner: &'a dyn SearchProvider,
        prepared: PreparedHnswSearch,
    ) -> Self {
        HnswDenseProvider {
            access,
            arena,
            slot_ids,
            inner,
            prepared,
            rounds: AtomicU64::new(0),
            resume: Mutex::new(None),
        }
    }

    /// クエリ終了時に呼び出し元（`sql::exec::execute_statement_with_cache`）が
    /// 明示的に呼ぶ（`Drop` にはしない——`rounds` の反映はロックを取る
    /// `HnswIndexCache::record_hybrid_query_rounds` を伴うため、`Drop` 内で
    /// パニック・ロック競合の意図しない挙動を持ち込まず、呼び出し元の制御下に
    /// 置く）。索引経路を一度も通らなかったクエリ（`rounds == 0`。密のみ縮退で
    /// `inner` へ全ラウンド委譲した場合を含む）は `hybrid_queries` を汚さない。
    pub(crate) fn finish(&self) {
        let rounds = self.rounds.load(Ordering::Relaxed);
        if rounds > 0 {
            self.access.cache.record_hybrid_query_rounds(rounds);
        }
    }
}

impl SearchProvider for HnswDenseProvider<'_> {
    fn search(&self, input: SearchInput<'_>) -> Result<Vec<CandidateHit>, KernelError> {
        // §本構造体ドキュメンテーションコメント「fail-closed な受理条件」参照。
        // `as_ptr()` は空スライスでもダングリングでない有効なポインタを返す
        // （`slice::as_ptr` の契約）ため、長さ比較と組み合わせれば安全に同一
        // バッファ判定に使える。
        let accepted = input.dim == self.arena.dim()
            && input.vectors.len() == self.arena.vectors().len()
            && std::ptr::eq(input.vectors.as_ptr(), self.arena.vectors().as_ptr())
            && input.ids.len() == self.slot_ids.len()
            && std::ptr::eq(input.ids.as_ptr(), self.slot_ids.as_ptr());
        if !accepted {
            return self.inner.search(input);
        }
        self.rounds.fetch_add(1, Ordering::Relaxed);
        self.access.cache.record_hybrid_dense_search();
        // `Mutex` の poison（他ラウンドが panic した）は fail-closed に「再開
        // 状態を捨てて今回はラウンド 1 として扱う」側へ倒す——不整合な状態を
        // 使い続けない。
        let mut guard = self.resume.lock().unwrap_or_else(|poisoned| {
            let mut inner = poisoned.into_inner();
            *inner = None;
            inner
        });
        search_prepared_resumable(
            self.access,
            &self.prepared,
            self.inner,
            self.arena,
            self.slot_ids,
            input.query,
            input.k,
            &mut guard,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::hnsw::provider::HnswSearchProvider;
    use crate::hnsw::ValidatedHnswParams;
    use crate::parallel_search::ParallelSearchProvider;
    use crate::policy::PolicyContext;
    use crate::rls::ImplicitRlsHook;
    use crate::sql::hnsw_cache::{prepare_full_visible, HnswIndexCache};
    use crate::storage::{RowInput, Storage, Visibility};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    const DIM: u32 = 4;

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
        let c = PolicyContext::new(tenant).expect("valid tenant");
        let op_id = crate::recovery::required_op_id::OperationId::parse(&format!(
            "hnsw-hybrid-test-{tenant}-{table}-{id}"
        ))
        .expect("valid operation_id");
        crate::tenant::insert_row(
            storage,
            table,
            &c,
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

    fn seeded_storage(table: &str, tenant: &str, rows: usize) -> (Storage, CleanupGuard) {
        let path = unique_db_path("hnsw_hybrid");
        let guard = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, table, DIM);
        for i in 0..rows {
            let v = i as f32;
            let embedding = [v, v + 1.0, v + 2.0, v + 3.0];
            seed_row(&storage, table, i as u64, tenant, &embedding);
        }
        (storage, guard)
    }

    fn build_arena(
        read_txn: &redb::ReadTransaction,
        table: &str,
        c: &PolicyContext,
    ) -> VectorArena {
        let hook = ImplicitRlsHook::new(c);
        VectorArena::build_filtered_with_rows_in_txn(
            read_txn,
            table,
            hook.predicate(),
            |_, _, _, _| Ok(true),
        )
        .expect("build arena")
    }

    /// 索引経路を使わない別バッファの `SearchInput`（受理条件を満たさない）は
    /// `inner` へ委譲され、`HnswIndexCache` の統計を一切汚さないことを固定する
    /// （§`HnswDenseProvider` ドキュメンテーションコメント「fail-closed な
    /// 受理条件」）。
    #[test]
    fn search_delegates_to_inner_for_a_different_buffer() {
        let table = "docs";
        let tenant = "tenant-a";
        let (storage, _guard) = seeded_storage(table, tenant, 2_000);
        let c = ctx(tenant);
        let read_txn = storage.db().begin_read().expect("begin read");
        let arena = build_arena(&read_txn, table, &c);
        let slot_ids: Vec<u64> = (0..arena.len() as u64).collect();
        let cache = HnswIndexCache::new();
        let hnsw_provider = HnswSearchProvider::new(ValidatedHnswParams::default());
        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: hnsw_provider,
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        let read_txn = storage.db().begin_read().expect("begin read");
        let prepared = prepare_full_visible(&access, &read_txn, table, &c, &arena);

        let inner = ParallelSearchProvider;
        let adapter = HnswDenseProvider::new(&access, &arena, &slot_ids, &inner, prepared);

        // 別のベクトル列（同じ内容でも別バッファ）を渡すと ptr-eq 判定が外れる。
        let other_vectors: Vec<f32> = arena.vectors().to_vec();
        let other_ids: Vec<u64> = slot_ids.clone();
        let input = SearchInput {
            ids: &other_ids,
            vectors: &other_vectors,
            dim: DIM,
            query: &[0.0, 1.0, 2.0, 3.0],
            k: 5,
        };
        adapter.search(input).expect("search succeeds via inner");
        adapter.finish();

        let stats = cache.stats();
        assert_eq!(
            stats.hybrid_dense_searches, 0,
            "different buffer must not be counted as an indexed hybrid round"
        );
        assert_eq!(stats.hybrid_queries, 0);
    }

    /// 同一バッファ（クエリ開始時に捕捉した `arena`／`slot_ids` そのもの）を
    /// 複数ラウンド渡すと、`prepare_full_visible` は 1 回しか走らず
    /// （`hybrid_dense_searches` がラウンド数だけ増える一方 `builds` は
    /// 増えない）、索引経路が非 vacuous に使われることを固定する。
    #[test]
    fn search_reuses_prepared_base_across_rounds_for_the_same_buffer() {
        let table = "docs";
        let tenant = "tenant-a";
        let (storage, _guard) = seeded_storage(table, tenant, 2_000);
        let c = ctx(tenant);
        let read_txn = storage.db().begin_read().expect("begin read");
        let arena = build_arena(&read_txn, table, &c);
        let slot_ids: Vec<u64> = (0..arena.len() as u64).collect();
        let cache = HnswIndexCache::new();
        let hnsw_provider = HnswSearchProvider::new(ValidatedHnswParams::default());
        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: hnsw_provider,
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        let read_txn = storage.db().begin_read().expect("begin read");
        let prepared = prepare_full_visible(&access, &read_txn, table, &c, &arena);
        let builds_after_prepare = cache.stats().builds;

        let inner = ParallelSearchProvider;
        let adapter = HnswDenseProvider::new(&access, &arena, &slot_ids, &inner, prepared);

        for round_k in [10usize, 20, 40] {
            let input = SearchInput {
                ids: &slot_ids,
                vectors: arena.vectors(),
                dim: arena.dim(),
                query: &[0.0, 1.0, 2.0, 3.0],
                k: round_k,
            };
            let hits = adapter.search(input).expect("search succeeds via index");
            assert!(!hits.is_empty());
        }
        adapter.finish();

        let stats = cache.stats();
        assert_eq!(stats.builds, builds_after_prepare, "prepare must run once");
        assert_eq!(stats.hybrid_dense_searches, 3);
        assert_eq!(stats.hybrid_queries, 1);
        assert_eq!(stats.hybrid_rounds_max, 3);
    }

    /// `k > MAX_EF` のラウンド（hybrid 密側再取得ループが `fetch_k` を
    /// `MAX_EF` 超まで倍増した場合。§`hnsw_cache.rs` の `ef_cap_fallbacks`
    /// ドキュメンテーションコメント参照）は brute-force へ縮退しつつ、要求
    /// `k` 件（可視件数が上回る場合）を返すことを固定する（空集合の誤返却
    /// 防止）。
    #[test]
    fn search_falls_back_to_brute_force_when_k_exceeds_max_ef() {
        let table = "docs";
        let tenant = "tenant-a";
        let (storage, _guard) = seeded_storage(table, tenant, 2_000);
        let c = ctx(tenant);
        let read_txn = storage.db().begin_read().expect("begin read");
        let arena = build_arena(&read_txn, table, &c);
        let slot_ids: Vec<u64> = (0..arena.len() as u64).collect();
        let cache = HnswIndexCache::new();
        let hnsw_provider = HnswSearchProvider::new(ValidatedHnswParams::default());
        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: hnsw_provider,
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        let read_txn = storage.db().begin_read().expect("begin read");
        let prepared = prepare_full_visible(&access, &read_txn, table, &c, &arena);

        let inner = ParallelSearchProvider;
        let adapter = HnswDenseProvider::new(&access, &arena, &slot_ids, &inner, prepared);

        let over_max_ef = crate::hnsw::MAX_EF + 1;
        let input = SearchInput {
            ids: &slot_ids,
            vectors: arena.vectors(),
            dim: arena.dim(),
            query: &[0.0, 1.0, 2.0, 3.0],
            k: over_max_ef,
        };
        let hits = adapter
            .search(input)
            .expect("search succeeds via full scan");
        assert_eq!(hits.len(), arena.len().min(over_max_ef));
        adapter.finish();

        let stats = cache.stats();
        assert_eq!(stats.ef_cap_fallbacks, 1);
        assert_eq!(stats.hybrid_dense_searches, 1);
    }

    /// Issue #505: 同一バッファ・同一クエリへ `k` を倍増しながら複数ラウンド
    /// 呼ぶと、2 ラウンド目以降が破棄候補ヒープ保持型の再開型探索
    /// （`sql::hnsw_cache::search_prepared_resumable`）を通り、`builds` を
    /// 増やさず `hybrid_resumed_rounds` が非 vacuous に計上されることを固定
    /// する（§`HnswIndexCacheStats::hybrid_resumed_rounds` ドキュメンテーション
    /// コメント参照）。
    #[test]
    fn search_resumes_across_rounds_for_the_same_buffer() {
        let table = "docs";
        let tenant = "tenant-a";
        let (storage, _guard) = seeded_storage(table, tenant, 2_000);
        let c = ctx(tenant);
        let read_txn = storage.db().begin_read().expect("begin read");
        let arena = build_arena(&read_txn, table, &c);
        let slot_ids: Vec<u64> = (0..arena.len() as u64).collect();
        let cache = HnswIndexCache::new();
        let hnsw_provider = HnswSearchProvider::new(ValidatedHnswParams::default());
        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: hnsw_provider,
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        let read_txn = storage.db().begin_read().expect("begin read");
        let prepared = prepare_full_visible(&access, &read_txn, table, &c, &arena);
        let builds_after_prepare = cache.stats().builds;

        let inner = ParallelSearchProvider;
        let adapter = HnswDenseProvider::new(&access, &arena, &slot_ids, &inner, prepared);

        for round_k in [10usize, 20, 40] {
            let input = SearchInput {
                ids: &slot_ids,
                vectors: arena.vectors(),
                dim: arena.dim(),
                query: &[0.0, 1.0, 2.0, 3.0],
                k: round_k,
            };
            let hits = adapter.search(input).expect("search succeeds via index");
            assert!(!hits.is_empty());
        }
        adapter.finish();

        let stats = cache.stats();
        assert_eq!(stats.builds, builds_after_prepare, "prepare must run once");
        assert_eq!(stats.hybrid_dense_searches, 3);
        assert!(
            stats.hybrid_resumed_rounds >= 1,
            "at least the 2nd/3rd rounds should have resumed the previous graph scan"
        );
    }

    /// 別バッファ（受理条件を満たさないラウンド）へ委譲しても再開状態には
    /// 一切触れないことを固定する（Issue #505。§`HnswDenseProvider::resume`
    /// ドキュメンテーションコメント参照）。
    #[test]
    fn search_for_a_different_buffer_does_not_touch_resume_state() {
        let table = "docs";
        let tenant = "tenant-a";
        let (storage, _guard) = seeded_storage(table, tenant, 2_000);
        let c = ctx(tenant);
        let read_txn = storage.db().begin_read().expect("begin read");
        let arena = build_arena(&read_txn, table, &c);
        let slot_ids: Vec<u64> = (0..arena.len() as u64).collect();
        let cache = HnswIndexCache::new();
        let hnsw_provider = HnswSearchProvider::new(ValidatedHnswParams::default());
        let gate_cache = crate::catalog::IndexCatalogGateCache::new();
        let access = HnswCacheAccess {
            storage: &storage,
            cache: &cache,
            provider: hnsw_provider,
            index_gate_cache: &gate_cache,
            hnsw_scope: crate::search_engine::HnswScope::All,
        };
        let read_txn = storage.db().begin_read().expect("begin read");
        let prepared = prepare_full_visible(&access, &read_txn, table, &c, &arena);

        let inner = ParallelSearchProvider;
        let adapter = HnswDenseProvider::new(&access, &arena, &slot_ids, &inner, prepared);

        let other_vectors: Vec<f32> = arena.vectors().to_vec();
        let other_ids: Vec<u64> = slot_ids.clone();
        for round_k in [5usize, 10] {
            let input = SearchInput {
                ids: &other_ids,
                vectors: &other_vectors,
                dim: DIM,
                query: &[0.0, 1.0, 2.0, 3.0],
                k: round_k,
            };
            adapter.search(input).expect("search succeeds via inner");
        }
        adapter.finish();

        let stats = cache.stats();
        assert_eq!(stats.hybrid_dense_searches, 0);
        assert_eq!(
            stats.hybrid_resumed_rounds, 0,
            "delegated rounds must never build or consume resume state"
        );
    }
}
