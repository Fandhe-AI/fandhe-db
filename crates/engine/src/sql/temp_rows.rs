//! セッション単位の一時表の行ストア（TABLE-44・Issue #1682。親 #1680 の TEMP TABLE 系列）。
//!
//! # 責務境界
//!
//! - `TempRowStore`: 接続（セッション）ごとに 1 個だけ持つインメモリの `redb::Database`。
//!   [`crate::sql::mode::SessionState`] が `Arc` で保持するため、他接続・他テナントへ届く
//!   経路は構造上存在しない。`EngineCore`・各種キャッシュ・`SessionTransaction`・static へは
//!   持たせない（`Arc` を持つのは `SessionState` とその巻き戻し用 clone だけ）。
//! - `TempWriteTxn` / `TempReadTxn`: 行の読み書きの入口。行は永続側と同じ
//!   `(tenant_id, id) → 行バイト列`（[`crate::storage::RowStoreTableDef`]）の形で置くため、
//!   #1684 は既存の `ReadSource` ベースの読み取り本体と行形式（`row_codec`）を再利用できる。
//!   行のエンコードは呼び出し元の責務で、本モジュールは不透明なバイト列として扱う。
//!
//! # 永続化しない・残骸を残さない
//!
//! ディスクには一切書かない。永続 DB とは別インスタンスで、`writer_gate`・台帳・`batch_log`・
//! `table_generation` を通らないため、永続化・変更ログ・レプリケーションの対象外であることが
//! 構造上保証される。接続の切断（エラー終了を含む）では wire-server の接続ハンドラが
//! `SessionState` を drop して最後の `Arc` が落ち、メモリごと消える。プロセスのクラッシュ・
//! 再起動後に残る物は無い（カタログ項目もデータディレクトリ内の一時領域も作らない）。
//!
//! # 遅延生成
//!
//! 空のインメモリ DB でも生成時に約 1 MiB を確保する。`--max-connections` 分の接続が
//! 常時これを持つのを避けるため、`SessionState` は最初に行領域が要るとき
//! （#1683 の `CREATE TEMP TABLE`）に `SessionState::temp_rows_or_init` で生成する。
//! 一時表を使わないセッションのコストはゼロである。
//!
//! # 総バイト数の上限（TABLE-44 (6)）
//!
//! backend の長さは解放されたページが OS に返らないため判定に使えない。そこで
//! 「キーの tenant_id 長 + 8 + 値の長さ」の論理合計を自前で集計し、書き込みの**前に**
//! checked 演算で上限と比べ、超過は `54000`（固定文言）にする。集計は temp DB 自身の
//! メタテーブルに置き、行の書き込みと同じトランザクションで更新するため、abort で行と
//! 集計がそろって戻る。既定は [`DEFAULT_MAX_TEMP_BYTES_PER_SESSION`]（走査上限と同じ
//! 64 MiB）。最悪ケースの論理値は `--max-connections`（最大 256）× 64 MiB = 16 GiB で、
//! 物理メモリは redb のページのオーバーヘッド分だけ増える。起動時の CLI は #1683 が担う。
//! 多層防御として、backend ラッパーが物理長の上限（論理上限 × 係数 + 余裕）を超える拡張を
//! 拒否し、以後そのストアを使用不可（fail-closed）にする。通常は論理集計が先に止める。
//!
//! # redb の設定
//!
//! - cache size は既定の 1 GiB ではなく小さな値にする（インメモリでは backend と
//!   ページキャッシュの二重持ちになるため）。
//! - Durability は既定の `Immediate` のままにする。`None` だと持続化済みページの解放が次の
//!   durable commit まで持ち越され、解放ページが再利用されずメモリが増え続ける。
//!   `InMemoryBackend` の `sync_data` は何もしないため性能上の損は無い。
//!
//! # キャッシュの構造的ゲート（P0）
//!
//! [`TempReadTxn`] は `ReadSource` を実装し、`snapshot()` は常に `None` を返す。一時表は
//! 同名の永続表を隠すため、テーブル名・世代をキーにするキャッシュ（`arena_cache`・
//! `hnsw_cache`・`visible_cache`・`scalar_index` 等）へ temp の行が載ると、永続側の読み取りや
//! 他セッション・他テナントへ漏れうる。生の `redb::ReadTransaction` / `Database` を返す
//! accessor は意図的に作らない。
//!
//! # 巻き戻しとの整合
//!
//! `SessionState` の clone／restore では一時カタログは巻き戻るが、`Arc` 共有のストアの中身は
//! 巻き戻らない。そこで temp DB の書き込みトランザクションの寿命を `SessionTransaction` に
//! 合わせる契約とする（BEGIN／自動コミットの 1 文で `begin_write`、COMMIT で `commit`、
//! ROLLBACK・失敗・drop で `abort`）。この結線は #1684 の担当。カタログの巻き戻しで取り残された
//! 行領域は [`TempWriteTxn::retain_relations`] で掃除できる。
//!
//! # 後続 Issue への申し送り
//!
//! #1683（実装済み）: `CREATE TEMP TABLE` で `temp_rows_or_init`、DROP で `remove` が返す id により
//! `drop_relation`、ON COMMIT DELETE ROWS で `truncate_relation`、上限の CLI。
//! #1684: `TempReadTxn` の executor 結線・行 id 採番・`23505` 判断・暫定ゲート解除。
//! #1685: テナント境界と crash 耐性の CI 検査。

// 行の読み書きの API は一時表への DML／SELECT の実行腕（#1684）が使う。本 Issue（#1683）の
// 時点では `truncate_relation`・`drop_relation` 以外はテストからのみ到達するため dead_code
// 警告を抑止する（`temp_catalog.rs` と同じ前例）。
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use redb::backends::InMemoryBackend;
use redb::{
    ReadableDatabase, ReadableTable, StorageBackend, TableDefinition, TableError, TableHandle,
    Value,
};

use crate::sql::allowlist::SqlSurfaceError;
use crate::sql::temp_catalog::{SessionTempCatalog, TempRelationId};
use crate::storage::{RowStoreTableDef, MAX_METADATA_LEN, MAX_TENANT_ID_LEN};

/// 1 セッションの一時表の論理総バイト数の既定上限（64 MiB。`storage` の走査上限と同値で、
/// 一時表を丸ごと走査しても走査の上限に収まる）。
pub(crate) const DEFAULT_MAX_TEMP_BYTES_PER_SESSION: u64 = 64 * 1024 * 1024;

/// 物理長の上限 = 論理上限 × この係数 + [`PHYSICAL_SLACK_BYTES`]（redb のページ・B 木の
/// オーバーヘッドと解放待ちページの余裕）。
const PHYSICAL_FACTOR: u64 = 4;
const PHYSICAL_SLACK_BYTES: u64 = 16 * 1024 * 1024;

/// redb のページキャッシュ。既定の 1 GiB を避ける（モジュールドキュメント参照）。
const TEMP_CACHE_SIZE_BYTES: usize = 1024 * 1024;

/// 1 行の値の長さの上限（行ヘッダの余裕込み）。超過は確保の前に `54000`。
const MAX_TEMP_ROW_VALUE_LEN: usize = MAX_METADATA_LEN as usize + 4096;

/// 行領域のテーブル名の接頭辞。`validate_identifier` は `/` を許さないため、
/// 識別子由来の名前とは衝突しない。
const ROWS_PREFIX: &str = "temp_rows/";

/// 関係ごとの論理バイト数（`TempRelationId` → バイト数）。
const REL_BYTES: TableDefinition<u64, u64> = TableDefinition::new("temp_rel_bytes");
/// 全体のメタ情報（現状は総バイト数のみ）。
const META: TableDefinition<&str, u64> = TableDefinition::new("temp_meta");
const META_TOTAL: &str = "total_bytes";

/// 固定文言の内部エラー。redb の詳細・行の値・他テナントの識別子を含めない。
fn internal() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "temporary storage error".to_string(),
    }
}

fn too_large() -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large("temporary table storage limit exceeded")
}

fn rows_table_name(rel: TempRelationId) -> String {
    format!("{ROWS_PREFIX}{}", rel.get())
}

fn rows_def(name: &str) -> RowStoreTableDef<'_> {
    TableDefinition::new(name)
}

/// `InMemoryBackend` を包み、物理長の上限を超える拡張を拒否する backend（多層防御）。
/// 拒否したら `exhausted` を立て、ストアを以後使用不可にする（redb は I/O エラー後に
/// DB を使えない状態にしうるため）。
#[derive(Debug)]
struct CappedMemoryBackend {
    inner: InMemoryBackend,
    max_physical: u64,
    exhausted: Arc<AtomicBool>,
    physical_len: Arc<AtomicU64>,
}

impl StorageBackend for CappedMemoryBackend {
    fn len(&self) -> Result<u64, io::Error> {
        self.inner.len()
    }

    fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), io::Error> {
        self.inner.read(offset, out)
    }

    fn set_len(&self, len: u64) -> Result<(), io::Error> {
        if len > self.max_physical {
            self.exhausted.store(true, Ordering::SeqCst);
            return Err(io::Error::other("temporary storage physical limit"));
        }
        self.inner.set_len(len)?;
        self.physical_len.store(len, Ordering::SeqCst);
        Ok(())
    }

    fn sync_data(&self) -> Result<(), io::Error> {
        self.inner.sync_data()
    }

    fn write(&self, offset: u64, data: &[u8]) -> Result<(), io::Error> {
        self.inner.write(offset, data)
    }
}

/// セッション専用の一時表の行ストア（モジュールドキュメント参照）。
pub(crate) struct TempRowStore {
    db: redb::Database,
    tenant_id: String,
    max_total_bytes: u64,
    exhausted: Arc<AtomicBool>,
    physical_len: Arc<AtomicU64>,
}

impl fmt::Debug for TempRowStore {
    /// 行の内容・テナント識別子を出さない。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TempRowStore")
            .field("max_total_bytes", &self.max_total_bytes)
            .finish_non_exhaustive()
    }
}

impl TempRowStore {
    /// 既定の物理上限（論理上限 × 係数 + 余裕）でストアを作る。
    pub(crate) fn new(tenant_id: &str, max_total_bytes: u64) -> Result<Self, SqlSurfaceError> {
        let max_physical = max_total_bytes
            .saturating_mul(PHYSICAL_FACTOR)
            .saturating_add(PHYSICAL_SLACK_BYTES);
        Self::with_limits(tenant_id, max_total_bytes, max_physical)
    }

    /// 物理上限を明示してストアを作る（backstop の検証用に分離）。
    pub(crate) fn with_limits(
        tenant_id: &str,
        max_total_bytes: u64,
        max_physical: u64,
    ) -> Result<Self, SqlSurfaceError> {
        if tenant_id.len() > usize::from(MAX_TENANT_ID_LEN) {
            return Err(internal());
        }
        let exhausted = Arc::new(AtomicBool::new(false));
        let physical_len = Arc::new(AtomicU64::new(0));
        let backend = CappedMemoryBackend {
            inner: InMemoryBackend::new(),
            max_physical,
            exhausted: Arc::clone(&exhausted),
            physical_len: Arc::clone(&physical_len),
        };
        let db = redb::Builder::new()
            .set_cache_size(TEMP_CACHE_SIZE_BYTES)
            .create_with_backend(backend)
            .map_err(|_| internal())?;
        Ok(Self {
            db,
            tenant_id: tenant_id.to_string(),
            max_total_bytes,
            exhausted,
            physical_len,
        })
    }

    /// 使用不可のストア、または所有テナントと一致しない tenant_id は拒否する（fail-closed）。
    fn check(&self, tenant_id: &str) -> Result<(), SqlSurfaceError> {
        if self.exhausted.load(Ordering::SeqCst) {
            return Err(too_large());
        }
        if tenant_id != self.tenant_id {
            return Err(internal());
        }
        Ok(())
    }

    /// 書き込みトランザクションを開始する。寿命は `SessionTransaction` に合わせる契約。
    pub(crate) fn begin_write(&self) -> Result<TempWriteTxn<'_>, SqlSurfaceError> {
        self.check(&self.tenant_id)?;
        let txn = self.db.begin_write().map_err(|_| internal())?;
        Ok(TempWriteTxn { txn, store: self })
    }

    /// 読み取りトランザクションを開始する。
    pub(crate) fn begin_read(&self) -> Result<TempReadTxn<'_>, SqlSurfaceError> {
        self.check(&self.tenant_id)?;
        let txn = self.db.begin_read().map_err(|_| internal())?;
        Ok(TempReadTxn { txn, store: self })
    }

    /// 確定済みの論理総バイト数。
    pub(crate) fn used_bytes(&self) -> Result<u64, SqlSurfaceError> {
        let txn = self.begin_read()?;
        match txn.txn.open_table(META) {
            Ok(t) => Ok(t
                .get(META_TOTAL)
                .map_err(|_| internal())?
                .map(|v| v.value())
                .unwrap_or(0)),
            Err(TableError::TableDoesNotExist(_)) => Ok(0),
            Err(_) => Err(internal()),
        }
    }

    /// 所有テナントと一致するか（`SessionState::temp_rows_or_init` の再利用時検証）。
    pub(crate) fn owner_matches(&self, tenant_id: &str) -> bool {
        self.tenant_id == tenant_id
    }

    /// backend の現在の物理長（メモリ上限の検証用）。
    pub(crate) fn physical_len(&self) -> u64 {
        self.physical_len.load(Ordering::SeqCst)
    }
}

/// 一時表の書き込みトランザクション。`commit` しないまま drop すると abort 扱い。
pub(crate) struct TempWriteTxn<'s> {
    txn: redb::WriteTransaction,
    store: &'s TempRowStore,
}

impl TempWriteTxn<'_> {
    fn table_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        Ok(self
            .txn
            .list_tables()
            .map_err(|_| internal())?
            .any(|h| h.name() == name))
    }

    fn row_cost(tenant_id: &str, value_len: usize) -> Result<u64, SqlSurfaceError> {
        if value_len > MAX_TEMP_ROW_VALUE_LEN {
            return Err(too_large());
        }
        let n = tenant_id
            .len()
            .checked_add(8)
            .and_then(|n| n.checked_add(value_len))
            .ok_or_else(too_large)?;
        u64::try_from(n).map_err(|_| too_large())
    }

    fn read_rel_bytes(&self, rel: TempRelationId) -> Result<u64, SqlSurfaceError> {
        let t = self.txn.open_table(REL_BYTES).map_err(|_| internal())?;
        let v = t.get(rel.get()).map_err(|_| internal())?;
        Ok(v.map(|v| v.value()).unwrap_or(0))
    }

    fn read_total(&self) -> Result<u64, SqlSurfaceError> {
        let t = self.txn.open_table(META).map_err(|_| internal())?;
        let v = t.get(META_TOTAL).map_err(|_| internal())?;
        Ok(v.map(|v| v.value()).unwrap_or(0))
    }

    /// 関係 `rel` の集計へ `add` を加え `sub` を引く。総量が上限を超えるなら、
    /// 何も書かずに `54000`。呼び出し元は行の変更の**前**に呼ぶ。
    fn charge(&self, rel: TempRelationId, add: u64, sub: u64) -> Result<(), SqlSurfaceError> {
        let rel_cur = self.read_rel_bytes(rel)?;
        let total_cur = self.read_total()?;
        let new_rel = rel_cur
            .checked_sub(sub)
            .ok_or_else(internal)?
            .checked_add(add)
            .ok_or_else(too_large)?;
        let new_total = total_cur
            .checked_sub(sub)
            .ok_or_else(internal)?
            .checked_add(add)
            .ok_or_else(too_large)?;
        if new_total > self.store.max_total_bytes {
            return Err(too_large());
        }
        let mut r = self.txn.open_table(REL_BYTES).map_err(|_| internal())?;
        r.insert(rel.get(), new_rel).map_err(|_| internal())?;
        drop(r);
        let mut m = self.txn.open_table(META).map_err(|_| internal())?;
        m.insert(META_TOTAL, new_total).map_err(|_| internal())?;
        Ok(())
    }

    /// 行を挿入する。`Ok(true)` は挿入、`Ok(false)` は同じ id が既に有り何も変えていない
    /// （重複の `23505` 判断は呼び出し元）。上限超過は `54000`。
    pub(crate) fn insert_row(
        &self,
        rel: TempRelationId,
        tenant_id: &str,
        id: u64,
        bytes: &[u8],
    ) -> Result<bool, SqlSurfaceError> {
        self.store.check(tenant_id)?;
        let cost = Self::row_cost(tenant_id, bytes.len())?;
        let name = rows_table_name(rel);
        if self.table_exists(&name)? {
            let t = self
                .txn
                .open_table(rows_def(&name))
                .map_err(|_| internal())?;
            let existing = t.get((tenant_id, id)).map_err(|_| internal())?;
            if existing.is_some() {
                return Ok(false);
            }
        }
        self.charge(rel, cost, 0)?;
        let mut t = self
            .txn
            .open_table(rows_def(&name))
            .map_err(|_| internal())?;
        t.insert((tenant_id, id), bytes).map_err(|_| internal())?;
        Ok(true)
    }

    /// 行を置換する（無ければ挿入）。集計は差分で更新する。
    pub(crate) fn put_row(
        &self,
        rel: TempRelationId,
        tenant_id: &str,
        id: u64,
        bytes: &[u8],
    ) -> Result<(), SqlSurfaceError> {
        self.store.check(tenant_id)?;
        let new_cost = Self::row_cost(tenant_id, bytes.len())?;
        let name = rows_table_name(rel);
        let mut old_cost = 0u64;
        if self.table_exists(&name)? {
            let t = self
                .txn
                .open_table(rows_def(&name))
                .map_err(|_| internal())?;
            let old = t.get((tenant_id, id)).map_err(|_| internal())?;
            if let Some(old) = old {
                old_cost = Self::row_cost(tenant_id, old.value().len())?;
            }
        }
        self.charge(rel, new_cost, old_cost)?;
        let mut t = self
            .txn
            .open_table(rows_def(&name))
            .map_err(|_| internal())?;
        t.insert((tenant_id, id), bytes).map_err(|_| internal())?;
        Ok(())
    }

    /// 行を削除する。存在したかを返す。
    pub(crate) fn delete_row(
        &self,
        rel: TempRelationId,
        tenant_id: &str,
        id: u64,
    ) -> Result<bool, SqlSurfaceError> {
        self.store.check(tenant_id)?;
        let name = rows_table_name(rel);
        if !self.table_exists(&name)? {
            return Ok(false);
        }
        let mut t = self
            .txn
            .open_table(rows_def(&name))
            .map_err(|_| internal())?;
        let removed = t.remove((tenant_id, id)).map_err(|_| internal())?;
        let removed_len = removed.map(|v| v.value().len());
        drop(t);
        match removed_len {
            Some(len) => {
                self.charge(rel, 0, Self::row_cost(tenant_id, len)?)?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// 行を取得する（自トランザクションの未 commit 書き込みを含む）。
    pub(crate) fn get_row(
        &self,
        rel: TempRelationId,
        tenant_id: &str,
        id: u64,
    ) -> Result<Option<Vec<u8>>, SqlSurfaceError> {
        self.store.check(tenant_id)?;
        let name = rows_table_name(rel);
        if !self.table_exists(&name)? {
            return Ok(None);
        }
        let t = self
            .txn
            .open_table(rows_def(&name))
            .map_err(|_| internal())?;
        let v = t.get((tenant_id, id)).map_err(|_| internal())?;
        Ok(v.map(|v| v.value().to_vec()))
    }

    /// 関係の全行を消す（TRUNCATE・ON COMMIT DELETE ROWS 用）。
    pub(crate) fn truncate_relation(&self, rel: TempRelationId) -> Result<(), SqlSurfaceError> {
        self.drop_relation(rel)
    }

    /// 関係の行領域と集計を削除する（DROP 用）。無ければ何もしない。
    pub(crate) fn drop_relation(&self, rel: TempRelationId) -> Result<(), SqlSurfaceError> {
        self.store.check(&self.store.tenant_id)?;
        let name = rows_table_name(rel);
        if self.table_exists(&name)? {
            self.txn
                .delete_table(rows_def(&name))
                .map_err(|_| internal())?;
        }
        let rel_cur = self.read_rel_bytes(rel)?;
        let total = self
            .read_total()?
            .checked_sub(rel_cur)
            .ok_or_else(internal)?;
        let mut r = self.txn.open_table(REL_BYTES).map_err(|_| internal())?;
        r.remove(rel.get()).map_err(|_| internal())?;
        drop(r);
        let mut m = self.txn.open_table(META).map_err(|_| internal())?;
        m.insert(META_TOTAL, total).map_err(|_| internal())?;
        Ok(())
    }

    /// カタログに無い関係の行領域（カタログの巻き戻しで取り残された孤児）を削除する。
    pub(crate) fn retain_relations(
        &self,
        catalog: &SessionTempCatalog,
    ) -> Result<(), SqlSurfaceError> {
        let live: BTreeSet<u64> = catalog
            .names()
            .filter_map(|n| catalog.get(n))
            .map(|e| e.id.get())
            .collect();
        let mut orphans = BTreeSet::new();
        for h in self.txn.list_tables().map_err(|_| internal())? {
            let id = h
                .name()
                .strip_prefix(ROWS_PREFIX)
                .and_then(|s| s.parse::<u64>().ok());
            if let Some(id) = id {
                if !live.contains(&id) {
                    orphans.insert(id);
                }
            }
        }
        // 行領域の無い集計だけが残ったエントリも対象にする。
        {
            let r = self.txn.open_table(REL_BYTES).map_err(|_| internal())?;
            for entry in r.iter().map_err(|_| internal())? {
                let (k, _) = entry.map_err(|_| internal())?;
                if !live.contains(&k.value()) {
                    orphans.insert(k.value());
                }
            }
        }
        for id in orphans {
            self.drop_relation(TempRelationId::from_raw(id))?;
        }
        Ok(())
    }

    /// 確定する。
    pub(crate) fn commit(self) -> Result<(), SqlSurfaceError> {
        self.txn.commit().map_err(|_| internal())
    }

    /// 破棄する（行と集計がそろって元に戻る）。
    pub(crate) fn abort(self) -> Result<(), SqlSurfaceError> {
        self.txn.abort().map_err(|_| internal())
    }
}

/// 一時表の読み取りトランザクション。`ReadSource` を実装し、`snapshot()` は常に `None`
/// （キャッシュの構造的ゲート。モジュールドキュメント参照）。
pub(crate) struct TempReadTxn<'s> {
    txn: redb::ReadTransaction,
    store: &'s TempRowStore,
}

impl TempReadTxn<'_> {
    /// 行を取得する。
    pub(crate) fn get_row(
        &self,
        rel: TempRelationId,
        tenant_id: &str,
        id: u64,
    ) -> Result<Option<Vec<u8>>, SqlSurfaceError> {
        self.store.check(tenant_id)?;
        let name = rows_table_name(rel);
        match self.txn.open_table(rows_def(&name)) {
            Ok(t) => {
                let v = t.get((tenant_id, id)).map_err(|_| internal())?;
                Ok(v.map(|v| v.value().to_vec()))
            }
            Err(TableError::TableDoesNotExist(_)) => Ok(None),
            Err(_) => Err(internal()),
        }
    }

    /// 関係の全行を id 昇順で返す。総量はストアの論理上限で頭打ちになる。
    pub(crate) fn scan_relation(
        &self,
        rel: TempRelationId,
        tenant_id: &str,
    ) -> Result<Vec<(u64, Vec<u8>)>, SqlSurfaceError> {
        self.store.check(tenant_id)?;
        let name = rows_table_name(rel);
        let t = match self.txn.open_table(rows_def(&name)) {
            Ok(t) => t,
            Err(TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(_) => return Err(internal()),
        };
        let mut out = Vec::new();
        for entry in t.iter().map_err(|_| internal())? {
            let (k, v) = entry.map_err(|_| internal())?;
            out.push((k.value().1, v.value().to_vec()));
        }
        Ok(out)
    }
}

impl crate::storage::read_source::ReadSource for TempReadTxn<'_> {
    type Table<'s, K: redb::Key + 'static, V: Value + 'static>
        = redb::ReadOnlyTable<K, V>
    where
        Self: 's;

    fn open_table<'s, K: redb::Key + 'static, V: Value + 'static>(
        &'s self,
        def: TableDefinition<'_, K, V>,
    ) -> Result<Self::Table<'s, K, V>, TableError> {
        self.txn.open_table(def)
    }

    fn snapshot(&self) -> Option<&redb::ReadTransaction> {
        None
    }
}
