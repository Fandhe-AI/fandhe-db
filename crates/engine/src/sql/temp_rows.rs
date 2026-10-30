//! セッション単位の一時表の実体ストア（TABLE-44・Issue #1682 で導入、#1684 で再設計。
//! 親 #1680 の TEMP TABLE 系列）。
//!
//! # 責務境界
//!
//! - `TempRowStore`: 接続（セッション）ごとに 1 個だけ持つ**インメモリの独立した `Storage`**
//!   （永続 DB とは別インスタンス）と、それを唯一のストレージとする専用の
//!   [`crate::core::EngineCore`]（以下「一時コア」）。[`crate::sql::mode::SessionState`] が
//!   `Arc` で保持するため、他接続・他テナントへ届く経路は構造上存在しない。`EngineCore`・各種
//!   キャッシュ・`SessionTransaction`・static へは持たせない（`Arc` を持つのは `SessionState` と
//!   その巻き戻し用 clone、および開いている一時の書き込みトランザクションだけ）。
//! - 一時コアは永続コアと**同じ実行器**（`EngineCore` の DML・読み取り・制約・索引の本体）を
//!   そのまま使う。違うのは `storage` だけで、永続 DB と物理レイアウトが同一（表ごとの
//!   `user_rows/<名前>`・カタログ・世代・索引の各表）のため、第 2 の実行器を作らない。
//!   `self.storage` を直接引く既存の経路も、一時コアでは一時 DB を引く。つまり「同名の永続表へ
//!   作用してしまう」誤りは構造上起きない（呼び出し元は `core/temp_route.rs`）。
//!
//! # 永続化しない・残骸を残さない
//!
//! ディスクには一切書かない。永続 DB とは別インスタンスで、永続 DB の `writer_gate`・台帳・
//! `batch_log`・変更ログを通らないため、永続化・変更ログ・レプリケーションの対象外であることが
//! 構造上保証される（一時コアの台帳は `LedgerMode::CompareOnlyWithoutLedger` で、台帳表に
//! 一切触れない）。接続の切断（エラー終了を含む）では wire-server の接続ハンドラが
//! `SessionState` を drop して最後の `Arc` が落ち、メモリごと消える。
//!
//! # 遅延生成
//!
//! 空のインメモリ DB でも生成時に約 1 MiB を確保する。`--max-connections` 分の接続が
//! 常時これを持つのを避けるため、`SessionState` は最初に必要になるとき
//! （`CREATE TEMP TABLE`）に `SessionState::temp_rows_or_init` で生成する。一時表を使わない
//! セッションのコストはゼロである。
//!
//! # 総バイト数の上限（TABLE-44 (6)）
//!
//! 既存の書き込み本体は一時コア専用の課金口を持たないため、一時表への書き込み文は
//! 一時の書き込みトランザクションの中で実行し、**文の実行後・commit の前**に redb の確保済み
//! ページ総量（`allocated_pages * page_size`。行・カタログ・索引・世代表をすべて含む）を
//! [`TempRowStore::enforce_budget`] で上限と比べる。超過なら commit せず abort して `54000`
//! （固定文言）にする。自動コミットの 1 文は文ごと、明示トランザクションでは文ごとに判定する
//! （超過した文のエラーでトランザクションは `Failed` になり、全体が abort される）。
//! 既定は [`DEFAULT_MAX_TEMP_BYTES_PER_SESSION`]（走査上限と同じ 64 MiB）。最悪ケースは
//! `--max-connections`（最大 256）× 64 MiB = 16 GiB。多層防御として、backend ラッパーが物理長の
//! 上限（論理上限 × 係数 + 余裕）を超える拡張を拒否し、以後そのストアを使用不可
//! （fail-closed、以降は `54000`）にする。redb は I/O エラー後にそのデータベースを使えなく
//! するため、この backstop は通常の上限判定の代わりにはできない。
//!
//! # redb の設定
//!
//! - cache size は既定の 1 GiB ではなく小さな値にする（インメモリでは backend とページ
//!   キャッシュの二重持ちになるため）。
//! - Durability は既定の `Immediate` のままにする。`None` だと持続化済みページの解放が次の
//!   durable commit まで持ち越され、解放ページが再利用されずメモリが増え続ける。
//!   `InMemoryBackend` の `sync_data` は何もしないため性能上の損は無い。
//!
//! # キャッシュのゲート（P0）
//!
//! 一時コアは自前のキャッシュ群（アリーナ・HNSW・可視行・スカラー索引など）を持つ。これらは
//! 一時 DB 専用の世代カウンタをキーにするため、永続 DB・他セッションのキャッシュと混ざらない。
//! 永続コア側のキャッシュへ一時表の行が載る経路は存在しない（一時表の文は必ず一時コアで実行する）。
//!
//! # 巻き戻しとの整合
//!
//! `SessionState` の clone／restore では一時カタログは巻き戻るが、`Arc` 共有のストアの中身は
//! 巻き戻らない。DDL（`CREATE`／`DROP TABLE`・索引）は自動コミットのみで明示トランザクション内では
//! `0A000` のため、カタログと実体がずれるのは DML の行だけである。DML は一時の書き込み
//! トランザクションの寿命を `SessionTransaction` に合わせる（BEGIN 後の最初の一時表の文で開き、
//! COMMIT で commit、ROLLBACK・失敗・drop で abort）ことで巻き戻る。
//!
//! # 後続 Issue への申し送り
//!
//! #1685: テナント境界と crash 耐性の CI 検査。

use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use redb::backends::InMemoryBackend;
use redb::StorageBackend;

use crate::catalog::TableSchema;
use crate::core::EngineCore;
use crate::policy::PolicyContext;
use crate::sql::allowlist::SqlSurfaceError;
use crate::storage::{GatedWriteTxn, Storage, MAX_TENANT_ID_LEN};

/// 1 セッションの一時表の総バイト数の既定上限（64 MiB。`storage` の走査上限と同値で、
/// 一時表を丸ごと走査しても走査の上限に収まる）。実行時の上限は
/// [`crate::temp_table_limits::TempTableLimits`] が持ち、本定数は同値であることの検証（テスト）用。
#[cfg(test)]
pub(crate) const DEFAULT_MAX_TEMP_BYTES_PER_SESSION: u64 = 64 * 1024 * 1024;

/// 物理長の上限 = 論理上限 × この係数 + [`PHYSICAL_SLACK_BYTES`]（redb のページ・B 木の
/// オーバーヘッドと解放待ちページの余裕）。
const PHYSICAL_FACTOR: u64 = 4;
const PHYSICAL_SLACK_BYTES: u64 = 16 * 1024 * 1024;

/// redb のページキャッシュ。既定の 1 GiB を避ける（モジュールドキュメント参照）。
const TEMP_CACHE_SIZE_BYTES: usize = 1024 * 1024;

/// 固定文言の内部エラー。redb の詳細・行の値・他テナントの識別子を含めない。
fn internal() -> SqlSurfaceError {
    SqlSurfaceError::Internal {
        detail: "temporary storage error".to_string(),
    }
}

fn too_large() -> SqlSurfaceError {
    SqlSurfaceError::payload_too_large("temporary table storage limit exceeded")
}

/// `InMemoryBackend` を包み、物理長の上限を超える拡張を拒否する backend（多層防御）。
/// 拒否したら `exhausted` を立て、ストアを以後使用不可にする（redb は I/O エラー後に
/// DB を使えない状態にするため）。
#[derive(Debug)]
struct CappedMemoryBackend {
    inner: InMemoryBackend,
    max_physical: u64,
    exhausted: Arc<AtomicBool>,
    /// 現在の物理長（`set_len` が成功するたびに更新する。総バイト数の上限判定の高速経路が読む）。
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

/// 一時表を対象にする `COPY FROM STDIN` の宛先（`CopyInSession`／`CopyInBatch` が運ぶ）。
/// 開始時の一時カタログ世代を持ち、COPY の受信中に一時表が作り直された場合は commit 時に拒否する。
#[derive(Debug, Clone)]
pub(crate) struct TempCopyTarget {
    pub(crate) store: Arc<TempRowStore>,
    pub(crate) epoch: u64,
}

/// セッション専用の一時表ストア（モジュールドキュメント参照）。
pub(crate) struct TempRowStore {
    core: EngineCore,
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
    /// 既定の物理上限（論理上限 × 係数 + 余裕）でストアを作る。`template` は構成（各種上限）を
    /// 引き継ぐ永続コアで、一時コアはその設定値だけをコピーする（ストレージ・キャッシュ・
    /// embedder 等は共有しない）。
    pub(crate) fn new(
        tenant_id: &str,
        max_total_bytes: u64,
        template: &EngineCore,
    ) -> Result<Self, SqlSurfaceError> {
        let max_physical = max_total_bytes
            .saturating_mul(PHYSICAL_FACTOR)
            .saturating_add(PHYSICAL_SLACK_BYTES);
        Self::with_limits(tenant_id, max_total_bytes, max_physical, template)
    }

    /// 物理上限を明示してストアを作る（backstop の検証用に分離）。
    pub(crate) fn with_limits(
        tenant_id: &str,
        max_total_bytes: u64,
        max_physical: u64,
        template: &EngineCore,
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
        let storage =
            Storage::open_with_backend(backend, TEMP_CACHE_SIZE_BYTES).map_err(|_| internal())?;
        Ok(Self {
            core: EngineCore::for_temp_session(template, storage),
            tenant_id: tenant_id.to_string(),
            max_total_bytes,
            exhausted,
            physical_len,
        })
    }

    /// 一時コア。使用不可（物理 backstop 到達）のストアは拒否する（fail-closed）。
    pub(crate) fn core(&self) -> Result<&EngineCore, SqlSurfaceError> {
        if self.exhausted.load(Ordering::SeqCst) {
            return Err(too_large());
        }
        Ok(&self.core)
    }

    /// 所有テナントと一致しない tenant_id は固定文言の `Internal` で拒否する（fail-closed）。
    pub(crate) fn check_tenant(&self, tenant_id: &str) -> Result<(), SqlSurfaceError> {
        if tenant_id != self.tenant_id {
            return Err(internal());
        }
        Ok(())
    }

    /// 所有テナントと一致するか（`SessionState::temp_rows_or_init` の再利用時検証）。
    pub(crate) fn owner_matches(&self, tenant_id: &str) -> bool {
        self.tenant_id == tenant_id
    }

    /// 所有テナント用の `PolicyContext`（`ON COMMIT DELETE ROWS` が自動で行を消すときに使う。
    /// 一時 DB の行はすべて所有テナントの行である）。
    fn owner_ctx(&self) -> Result<PolicyContext, SqlSurfaceError> {
        PolicyContext::new(&self.tenant_id).map_err(|_| internal())
    }

    fn storage(&self) -> Result<&Storage, SqlSurfaceError> {
        Ok(self.core()?.storage_handle())
    }

    /// 書き込みトランザクションを開始する。一時 DB の writer gate は永続 DB のものとは別で、
    /// 同じスレッドが既に一時の書き込みトランザクションを保持していれば（再入）待たずに
    /// `55P03` を返す（単一ライタによるデッドロックを作らない）。
    pub(crate) fn begin_write(&self) -> Result<GatedWriteTxn, SqlSurfaceError> {
        self.storage()?
            .begin_write_txn()
            .map_err(|_| SqlSurfaceError::LockNotAvailable)
    }

    /// 書き込みトランザクションを確定する。
    pub(crate) fn commit(&self, txn: GatedWriteTxn) -> Result<(), SqlSurfaceError> {
        crate::recovery::commit_boundary::commit(txn).map_err(|_| internal())
    }

    /// 確保済みページ総量が上限を超えていれば `54000`。文の実行後・commit の前に呼ぶ
    /// （モジュールドキュメント「総バイト数の上限」）。
    pub(crate) fn enforce_budget(
        &self,
        txn: &redb::WriteTransaction,
    ) -> Result<(), SqlSurfaceError> {
        if self.exhausted.load(Ordering::SeqCst) {
            return Err(too_large());
        }
        // 高速経路: 確保済みページの総量は物理長以下なので、物理長が上限以内なら上限以内と言える
        // （`txn.stats()` は全表の木を辿る O(確保ページ数) の処理で、文ごとに呼ぶと一時表が大きいほど
        // 1 文あたりの費用が線形に増える）。物理長は解放しても即座には縮まない（commit で縮む）ため、
        // 上限を超えている間だけ正確な確保済み総量を測り直す。
        if self.physical_len.load(Ordering::SeqCst) <= self.max_total_bytes {
            return Ok(());
        }
        let stats = txn.stats().map_err(|_| internal())?;
        let used = stats
            .allocated_pages()
            .checked_mul(u64::try_from(stats.page_size()).map_err(|_| internal())?)
            .ok_or_else(too_large)?;
        if used > self.max_total_bytes {
            return Err(too_large());
        }
        Ok(())
    }

    /// 確定済みの確保済みページ総量（診断・テスト用）。一時の書き込みトランザクションが開いて
    /// いる間は呼べない（`55P03`）。
    #[cfg(test)]
    pub(crate) fn used_bytes(&self) -> Result<u64, SqlSurfaceError> {
        let txn = self.begin_write()?;
        let stats = txn.stats().map_err(|_| internal())?;
        let used = stats
            .allocated_pages()
            .saturating_mul(u64::try_from(stats.page_size()).unwrap_or(u64::MAX));
        drop(txn);
        Ok(used)
    }

    /// 一時表を一時 DB のカタログへ作る（`CREATE TEMP TABLE`。スキーマは呼び出し元が永続表と
    /// 同じ規則で構築・検証済み）。
    ///
    /// 作成後に容量上限を検査し、超過なら作った実体を戻して `54000` を返す（DDL も上限を迂回
    /// させない。`--max-temp-bytes` 超過のまま確定した状態を残さない）。
    pub(crate) fn create_table(&self, schema: &TableSchema) -> Result<(), SqlSurfaceError> {
        self.storage()?.create_table(schema).map_err(|e| match e {
            // 一時索引など、一時カタログ（表名のみ）に載らない関係との名前衝突は永続表と同じ `42P07`。
            crate::catalog::CatalogError::TableAlreadyExists(name) => {
                SqlSurfaceError::duplicate_table(name)
            }
            _ => internal(),
        })?;
        if let Err(e) = self.enforce_budget_after_ddl() {
            let _ = self.drop_tables(std::slice::from_ref(&schema.name));
            return Err(e);
        }
        Ok(())
    }

    /// 一時 DB に同名の関係（表・ビュー・索引）が既にあるか。`CREATE TEMP TABLE IF NOT EXISTS` の
    /// 一時索引との衝突判定に使う（自セッション専用の一時 DB だけを見るため存在オラクルにならない）。
    pub(crate) fn relation_name_exists(&self, name: &str) -> Result<bool, SqlSurfaceError> {
        self.storage()?
            .relation_name_exists(name)
            .map_err(|_| internal())
    }

    /// DDL（確定済み）の直後に容量上限を検査する。新しい書き込みトランザクションで
    /// [`Self::enforce_budget`] を呼び、変更せず abort する。超過なら `54000`。
    /// 呼び出し元は超過時に DDL を戻す責務を持つ。
    pub(crate) fn enforce_budget_after_ddl(&self) -> Result<(), SqlSurfaceError> {
        let txn = self.begin_write()?;
        let result = self.enforce_budget(&txn);
        drop(txn);
        result
    }

    /// 一時表を一時 DB から削除する（行・索引・世代を含む）。存在しない名前は無視する。
    pub(crate) fn drop_tables(&self, names: &[String]) -> Result<(), SqlSurfaceError> {
        if names.is_empty() {
            return Ok(());
        }
        self.storage()?
            .drop_tables(names, true, crate::catalog::DropBehavior::Restrict)
            .map(|_| ())
            .map_err(|_| internal())
    }

    /// `ON COMMIT DELETE ROWS` の行全削除と `ON COMMIT DROP` の表削除を単一の書き込み
    /// トランザクションで原子的に適用する（片方だけ確定した状態を残さない）。
    pub(crate) fn apply_on_commit(
        &self,
        delete_rows: &[String],
        dropped: &[String],
    ) -> Result<(), SqlSurfaceError> {
        if delete_rows.is_empty() && dropped.is_empty() {
            return Ok(());
        }
        let ctx = self.owner_ctx()?;
        let txn = self.begin_write()?;
        if !delete_rows.is_empty() {
            crate::tenant::truncate_tables_unchecked(
                crate::tenant::WriteTarget::InTxn(&txn),
                delete_rows,
                crate::catalog::DropBehavior::Restrict,
                false,
                &ctx,
                crate::recovery::ledger::LedgerWrite::Disabled,
                // 一時表にトリガは定義できない（定義の段で `0A000`）。
                None,
            )
            .map_err(|_| internal())?;
        }
        if !dropped.is_empty() {
            self.core()?
                .storage_handle()
                .drop_tables_in_txn(&txn, dropped, true, crate::catalog::DropBehavior::Restrict)
                .map_err(|_| internal())?;
        }
        // `Err` を返すと `txn` が drop され abort する（部分適用を残さない）。
        self.commit(txn)
    }

    /// 一時表の全行を消す（`ON COMMIT DELETE ROWS`）。永続側の `TRUNCATE` と同じ本体を使い、
    /// 台帳には触れない（`LedgerWrite::Disabled`）。実行経路は [`Self::apply_on_commit`] で、
    /// これは単体テスト用。
    #[cfg(test)]
    pub(crate) fn truncate_tables(&self, names: &[String]) -> Result<(), SqlSurfaceError> {
        if names.is_empty() {
            return Ok(());
        }
        let ctx = self.owner_ctx()?;
        crate::tenant::truncate_tables_unchecked(
            crate::tenant::WriteTarget::Autocommit(self.storage()?),
            names,
            crate::catalog::DropBehavior::Restrict,
            false,
            &ctx,
            crate::recovery::ledger::LedgerWrite::Disabled,
            None,
        )
        .map(|_| ())
        .map_err(|_| internal())
    }
}
