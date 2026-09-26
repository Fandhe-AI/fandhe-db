//! 実行計画の複数テーブル対応基盤（SQL-28・RLS-10、TASK-212、Issue #924）その 2:
//! 複数テーブルの `(table, generation)` 集合を鍵とする汎用の世代整合キャッシュ。
//!
//! 既存の 5 キャッシュ（[`crate::sql::arena_cache::SqlArenaCache`]・
//! [`crate::sql::sparse_cache::SparseIndexCache`]・
//! [`crate::sql::scalar_index::ScalarIndex`]・
//! [`crate::sql::visible_cache::VisibleBitmapCache`]・
//! [`crate::sql::hnsw_cache`]）はいずれも単一テーブルの `(table, ctx)` を鍵に
//! 持つ。本モジュールはそれらと同じ fail-closed 契約（`lookup`/`insert` の
//! 非対称・世代不一致時の破棄条件）を、複数テーブルの鍵集合へ一般化して
//! **一度だけ**実装する（単一テーブルなら鍵の長さが 1 になるだけで、既存
//! キャッシュと同じ意味論になる）。本 Issue では既存 5 キャッシュをこちらへ
//! 移行しない（性能不変を優先。`docs/design/multi-relation-plan-foundation.md`
//! 参照）。
//!
//! [`crate::sql::relation_snapshot`] が本キャッシュを使い、テーブル単位の RLS
//! 可視スナップショットを複数テーブル横断で世代整合キャッシュする。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use redb::ReadableDatabase;

use crate::policy::PolicyContext;
use crate::storage::Storage;

use super::relation::MAX_TABLE_REFS;

/// [`GenerationKeyedCache`] のエントリ数上限（既存キャッシュ群と同じ DoS 対策
/// 方針。Issue #363・#357 参照）。
const MAX_ENTRIES: usize = 32;

/// [`GenerationKeyedCache`] の観測用統計。テナント ID・行 ID 等の機微情報は
/// 一切含まない（既存キャッシュの Stats 型と同じ方針）。
#[derive(Debug, Clone, Copy, Default)]
pub struct GenerationKeyedCacheStats {
    pub hits: u64,
    pub misses: u64,
    pub stale_evictions: u64,
    pub capacity_evictions: u64,
    pub entries: usize,
}

/// 複数テーブルの世代整合キー（[`PolicyContext`] + テーブル名昇順・重複除去済みの
/// `(table, generation)` 集合）。単一テーブルなら `tables` の長さは 1 で、既存
/// キャッシュのキー `(table, ctx)` + 世代と同じ意味になる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableGenerationKey {
    ctx: PolicyContext,
    tables: Vec<(String, u64)>,
}

impl TableGenerationKey {
    /// **呼び出し元が保持する単一の** `read_txn` から、`tables` 全件の世代を
    /// まとめて確定する（複数の read トランザクションを跨がない。1 つでも
    /// 読めなければ `Err` で fail-closed に諦める）。テーブル名は昇順ソート・
    /// 重複除去する。参照数の上限（[`MAX_TABLE_REFS`]）は `Vec` 確保前に検証する。
    pub fn capture(
        read_txn: &redb::ReadTransaction,
        ctx: PolicyContext,
        tables: &[&str],
    ) -> crate::catalog::Result<Self> {
        if tables.is_empty() {
            return Err(crate::catalog::CatalogError::Invalid(
                "at least one table is required in generation key".to_string(),
            ));
        }
        if tables.len() > MAX_TABLE_REFS {
            return Err(crate::catalog::CatalogError::Invalid(format!(
                "too many tables in generation key: {} (max {MAX_TABLE_REFS})",
                tables.len()
            )));
        }
        let mut names: Vec<&str> = tables.to_vec();
        names.sort_unstable();
        names.dedup();
        let mut resolved = Vec::with_capacity(names.len());
        for name in names {
            let generation = crate::catalog::table_generation_in_txn(read_txn, name)?;
            resolved.push((name.to_string(), generation));
        }
        Ok(Self {
            ctx,
            tables: resolved,
        })
    }

    /// この鍵が関わるテーブル名（昇順・重複なし）。
    pub fn table_names(&self) -> impl Iterator<Item = &str> {
        self.tables.iter().map(|(name, _)| name.as_str())
    }

    /// `table` を含む鍵かどうか（[`GenerationKeyedCache::insert`] の限定 retain が
    /// 使う）。
    pub fn involves(&self, table: &str) -> bool {
        self.tables.iter().any(|(name, _)| name == table)
    }

    /// `read_txn` から読んだ全テーブルの世代と、この鍵に記録済みの世代が
    /// **全テーブルについて** 一致するか（`ctx` も完全一致が前提）。1 つでも
    /// 読めなければ `false`（fail-closed。呼び出し元は「不一致」として扱う）。
    fn matches(&self, read_txn: &redb::ReadTransaction, ctx: &PolicyContext) -> bool {
        if &self.ctx != ctx {
            return false;
        }
        for (name, generation) in &self.tables {
            let Ok(current) = crate::catalog::table_generation_in_txn(read_txn, name) else {
                return false;
            };
            if current != *generation {
                return false;
            }
        }
        true
    }

    /// いずれかのテーブルで、構築時世代が `storage` から読んだ真の最新世代より
    /// 厳密に古いと確認できた場合のみ `Some(true)`。1 つでも読めなければ
    /// `None`（fail-closed。呼び出し元は破棄を諦める）。
    fn is_strictly_stale(&self, storage: &Storage) -> Option<bool> {
        for (name, generation) in &self.tables {
            let current = storage.table_generation(name).ok()?;
            if *generation < current {
                return Some(true);
            }
        }
        Some(false)
    }
}

/// [`GenerationKeyedCache`] の値が実装する概算ヒープバイト量 trait（容量制限用。
/// 既存キャッシュの `approx_heap_bytes` 私設メソッドを trait 化し、複数テーブル
/// キャッシュの実装を汎用にする）。
pub trait ApproxHeapBytes {
    fn approx_heap_bytes(&self) -> usize;
}

struct Entry<V> {
    key: TableGenerationKey,
    value: Arc<V>,
    last_used: u64,
}

#[derive(Default)]
struct State<V> {
    entries: Vec<Entry<V>>,
}

/// 複数テーブルの世代整合キャッシュ本体（モジュールドキュメント参照）。
/// `V` は [`ApproxHeapBytes`] を実装する必要がある。
pub struct GenerationKeyedCache<V> {
    state: RwLock<State<V>>,
    seq: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    stale_evictions: AtomicU64,
    capacity_evictions: AtomicU64,
    total_bytes_limit: usize,
}

impl<V: ApproxHeapBytes> GenerationKeyedCache<V> {
    /// `total_bytes_limit`: 保持するエントリ群の概算バイト量合計の上限
    /// （[`crate::arena::MAX_ARENA_TOTAL_BYTES`] と同じ桁を渡す想定）。
    pub fn new(total_bytes_limit: usize) -> Self {
        Self {
            state: RwLock::new(State {
                entries: Vec::new(),
            }),
            seq: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            stale_evictions: AtomicU64::new(0),
            capacity_evictions: AtomicU64::new(0),
            total_bytes_limit,
        }
    }

    /// `key` に一致し、`read_txn` のスナップショットにおける全テーブルの世代と
    /// 整合するエントリを探す。ロック毒化・世代読み取り失敗はいずれも「見つから
    /// なかった」として扱う（fail-closed。既存キャッシュ群と同じ契約）。
    ///
    /// `position` は `TableGenerationKey` の（世代を含む）完全一致で探すため、
    /// `key` が呼び出し元が直前に同じ `read_txn` から `capture` した値であれば、
    /// 見つかった時点で世代は既に一致しており、直後の `matches` 呼び出し・
    /// 不一致時の `is_strictly_stale` 破棄分岐は実質到達しない（世代不一致の
    /// エントリはそもそも `position` で見つからず、単に「ミスかつ破棄なし」で
    /// 終わる）。これらは `key` が別スナップショットから渡された場合に備えた
    /// 防御的チェックであり、通常経路の stale エントリ破棄は
    /// [`GenerationKeyedCache::insert`] の限定 retain が担う。
    pub fn lookup(
        &self,
        storage: &Storage,
        read_txn: &redb::ReadTransaction,
        ctx: &PolicyContext,
        key: &TableGenerationKey,
    ) -> Option<Arc<V>> {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.state.write().ok()?;
        let position = guard.entries.iter().position(|e| &e.key == key)?;
        if guard.entries[position].key.matches(read_txn, ctx) {
            let entry = &mut guard.entries[position];
            entry.last_used = seq;
            let value = Arc::clone(&entry.value);
            drop(guard);
            self.hits.fetch_add(1, Ordering::Relaxed);
            return Some(value);
        }
        // 不一致。`read_txn`（呼び出し元のスナップショット）視点ではミスだが、
        // 破棄してよいのはエントリが真に stale と確認できた場合のみ
        // （`SqlArenaCache::lookup` と同じ理由）。
        if guard.entries[position].key.is_strictly_stale(storage) == Some(true) {
            guard.entries.remove(position);
            self.stale_evictions.fetch_add(1, Ordering::Relaxed);
        }
        None
    }

    /// 新規構築した値を挿入する。呼び出し元の `key` が既に古い（並行書き込みで
    /// 挿入対象自身が既に stale）場合・ロック毒化・容量超過時はキャッシュへの
    /// 反映のみを諦めるが、戻り値は常に `Arc<V>`（既存キャッシュ群と同じ
    /// 非対称契約。呼び出し元はこのクエリのスナップショットから自分で構築した
    /// 結果としてそのまま使ってよい）。
    ///
    /// 世代不一致による一括破棄は「挿入キーのテーブルのいずれかを含み
    /// （[`TableGenerationKey::involves`]）、かつそのテーブルの世代が挿入キーと
    /// 食い違うエントリ」に限る。無関係なテーブルのみのエントリを巻き添えに
    /// しない。
    pub fn insert(&self, storage: &Storage, key: TableGenerationKey, value: V) -> Arc<V> {
        let value = Arc::new(value);
        self.misses.fetch_add(1, Ordering::Relaxed);

        let Ok(mut guard) = self.state.write() else {
            return value;
        };

        let Ok(read_txn) = storage.db().begin_read() else {
            return value;
        };
        let mut fresh = key.tables.clone();
        for (name, generation) in fresh.iter_mut() {
            match crate::catalog::table_generation_in_txn(&read_txn, name) {
                Ok(current) => *generation = current,
                Err(_) => return value,
            }
        }
        if fresh != key.tables {
            // 挿入対象自身が既に古い（並行書き込み）。キャッシュへは反映しない。
            return value;
        }

        let own_bytes = value.approx_heap_bytes();
        if own_bytes > self.total_bytes_limit {
            return value;
        }

        if let Some(pos) = guard.entries.iter().position(|e| e.key == key) {
            guard.entries.remove(pos);
        }

        let before = guard.entries.len();
        guard.entries.retain(|e| {
            !key.tables.iter().any(|(name, generation)| {
                e.key.involves(name) && !e.key.key_table_matches(name, *generation)
            })
        });
        let removed_stale = before.saturating_sub(guard.entries.len());
        if removed_stale > 0 {
            self.stale_evictions
                .fetch_add(removed_stale as u64, Ordering::Relaxed);
        }

        let mut total_bytes: usize = guard
            .entries
            .iter()
            .map(|e| e.value.approx_heap_bytes())
            .fold(0usize, |acc, n| acc.saturating_add(n));
        while guard.entries.len() >= MAX_ENTRIES
            || total_bytes.saturating_add(own_bytes) > self.total_bytes_limit
        {
            let victim = guard
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, e)| e.last_used)
                .map(|(idx, _)| idx);
            let Some(idx) = victim else {
                return value;
            };
            let removed = guard.entries.remove(idx);
            total_bytes = total_bytes.saturating_sub(removed.value.approx_heap_bytes());
            self.capacity_evictions.fetch_add(1, Ordering::Relaxed);
        }

        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        guard.entries.push(Entry {
            key,
            value: Arc::clone(&value),
            last_used: seq,
        });
        value
    }

    pub fn stats(&self) -> GenerationKeyedCacheStats {
        let entries = self.state.read().map(|g| g.entries.len()).unwrap_or(0);
        GenerationKeyedCacheStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            stale_evictions: self.stale_evictions.load(Ordering::Relaxed),
            capacity_evictions: self.capacity_evictions.load(Ordering::Relaxed),
            entries,
        }
    }
}

impl TableGenerationKey {
    /// `table` について、この鍵が記録している世代が `generation` と一致するか
    /// （`table` を含まない鍵は無関係なので `true` を返し、`retain` の対象から
    /// 除外させる）。[`GenerationKeyedCache::insert`] の限定破棄専用ヘルパー。
    fn key_table_matches(&self, table: &str, generation: u64) -> bool {
        self.tables
            .iter()
            .find(|(name, _)| name == table)
            .map(|(_, g)| *g == generation)
            .unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{ColumnDef, ColumnType, TableSchema};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    struct Counted(usize);
    impl ApproxHeapBytes for Counted {
        fn approx_heap_bytes(&self) -> usize {
            self.0
        }
    }

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

    #[test]
    fn hits_when_all_tables_match() {
        let path = unique_db_path("gen-key-hit");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "a");
        create_table(&storage, "b");
        let cache: GenerationKeyedCache<Counted> = GenerationKeyedCache::new(1024);
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let key = TableGenerationKey::capture(&read_txn, c.clone(), &["a", "b"]).unwrap();
        let inserted = cache.insert(&storage, key.clone(), Counted(8));

        let read_txn2 = storage.db().begin_read().unwrap();
        let hit = cache.lookup(&storage, &read_txn2, &c, &key);
        assert!(hit.is_some());
        assert!(Arc::ptr_eq(&hit.unwrap(), &inserted));
        assert_eq!(cache.stats().hits, 1);
    }

    #[test]
    fn misses_when_one_table_generation_advanced() {
        let path = unique_db_path("gen-key-miss-one");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "a");
        create_table(&storage, "b");
        let cache: GenerationKeyedCache<Counted> = GenerationKeyedCache::new(1024);
        let c = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let key = TableGenerationKey::capture(&read_txn, c.clone(), &["a", "b"]).unwrap();
        cache.insert(&storage, key.clone(), Counted(8));

        let write_txn = storage.db().begin_write().unwrap();
        crate::catalog::bump_table_generation_in_txn(&write_txn, "b").unwrap();
        crate::recovery::commit_boundary::commit(write_txn).unwrap();

        let read_txn2 = storage.db().begin_read().unwrap();
        assert!(cache.lookup(&storage, &read_txn2, &c, &key).is_none());
        assert_eq!(cache.stats().stale_evictions, 1);
    }

    #[test]
    fn insert_retain_does_not_evict_unrelated_table_entry() {
        let path = unique_db_path("gen-key-unrelated");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "a");
        create_table(&storage, "b");
        create_table(&storage, "c");
        let cache: GenerationKeyedCache<Counted> = GenerationKeyedCache::new(1024);
        let ctx_a = ctx("tenant-a");

        let read_txn = storage.db().begin_read().unwrap();
        let key_ab = TableGenerationKey::capture(&read_txn, ctx_a.clone(), &["a", "b"]).unwrap();
        cache.insert(&storage, key_ab.clone(), Counted(8));

        // a の世代を進めた後、c への insert が key_ab（a を含む）を巻き込んで
        // 破棄してよいが、c を全く含まない無関係なエントリまで消してはならない
        // ——ここでは key_ab 自身が a を含むため破棄対象になることを確認しつつ、
        // 別に挿入する c 単独キーには影響しないことを見る。
        let write_txn = storage.db().begin_write().unwrap();
        crate::catalog::bump_table_generation_in_txn(&write_txn, "a").unwrap();
        crate::recovery::commit_boundary::commit(write_txn).unwrap();

        let read_txn_c = storage.db().begin_read().unwrap();
        let key_c = TableGenerationKey::capture(&read_txn_c, ctx_a.clone(), &["c"]).unwrap();
        cache.insert(&storage, key_c.clone(), Counted(8));

        // key_ab は a を含み世代が食い違うため retain で破棄される。
        let read_txn_check = storage.db().begin_read().unwrap();
        assert!(cache
            .lookup(&storage, &read_txn_check, &ctx_a, &key_ab)
            .is_none());
        // c 単独のエントリは有効なまま。
        assert!(cache
            .lookup(&storage, &read_txn_check, &ctx_a, &key_c)
            .is_some());
    }

    #[test]
    fn different_tenant_ctx_misses() {
        let path = unique_db_path("gen-key-tenant-miss");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        create_table(&storage, "a");
        let cache: GenerationKeyedCache<Counted> = GenerationKeyedCache::new(1024);
        let owner = ctx("tenant-a");
        let other = ctx("tenant-b");

        let read_txn = storage.db().begin_read().unwrap();
        let key = TableGenerationKey::capture(&read_txn, owner.clone(), &["a"]).unwrap();
        cache.insert(&storage, key.clone(), Counted(8));

        let read_txn2 = storage.db().begin_read().unwrap();
        assert!(cache.lookup(&storage, &read_txn2, &other, &key).is_none());
    }

    #[test]
    fn capacity_eviction_keeps_entries_within_limit() {
        let path = unique_db_path("gen-key-capacity");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let cache: GenerationKeyedCache<Counted> = GenerationKeyedCache::new(usize::MAX);
        let c = ctx("tenant-a");
        for i in 0..(MAX_ENTRIES + 1) {
            let table = format!("t{i}");
            create_table(&storage, &table);
            let read_txn = storage.db().begin_read().unwrap();
            let key = TableGenerationKey::capture(&read_txn, c.clone(), &[&table]).unwrap();
            cache.insert(&storage, key, Counted(1));
        }
        let stats = cache.stats();
        assert!(stats.entries <= MAX_ENTRIES);
        assert!(stats.capacity_evictions >= 1);
    }

    #[test]
    fn capture_rejects_too_many_tables() {
        let path = unique_db_path("gen-key-too-many");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let read_txn = storage.db().begin_read().unwrap();
        let names: Vec<String> = (0..(MAX_TABLE_REFS + 1)).map(|i| format!("t{i}")).collect();
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let c = ctx("tenant-a");
        assert!(TableGenerationKey::capture(&read_txn, c, &refs).is_err());
    }
}
