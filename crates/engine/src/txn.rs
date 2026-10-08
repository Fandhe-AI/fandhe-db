//! 宣言済みトランザクション分離レベルの API（TASK-88、対象ビヘイビア: TABLE-3。
//! ポインタ: `docs/spec/05-tasks.md` TASK-88・`docs/spec/04-behavior/data-model.md`
//! TABLE-3）。`storage.rs` の PERSIST-4 分離レベル注記と同一根拠
//! （`docs/spec/04-behavior/persistence.md` PERSIST-4、TASK-140 で検証済み）。
//!
//! 責務境界: `Storage` が保持する `redb::Database` の読み取り/書き込みトランザクション
//! 境界を、独自ロック層を追加せず `redb` の契約のまま型として公開する。後続の SQL
//! surface・カタログ層（TASK-85〜）から、宣言済み分離レベルの確認とトランザクション
//! ハンドルの取得に使われる想定であり、本モジュールはポリシー評価（RLS 事前フィルタ等）を
//! 行わない（`storage.rs` と同方針）。

use redb::{ReadableDatabase, ReadableTable};

use crate::recovery::commit_boundary::commit_write_txn_guarded;
use crate::storage::{
    decode_row_for_key, encode_row, map_rows_table_error, Row, RowInput, Storage, StorageError,
    BATCH_LOG_TABLE, ROWS_TABLE,
};

/// engine が宣言する分離レベル（対象ビヘイビア: TABLE-3）。
///
/// 現時点でバリアントは 1 つのみ。値の追加は `redb` の契約から外れる分離レベルへの
/// 拡張を意味するため、破壊的変更として扱う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationLevel {
    /// 単一ライタ（書き込みは直列化）・スナップショット読み取り
    /// （読み取りは開始時点でコミット済みの状態のみを見る）。
    SingleWriterSnapshotRead,
}

impl std::fmt::Display for IsolationLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // japanese-style.md: プログラム出力文字列は英語とする。
            IsolationLevel::SingleWriterSnapshotRead => {
                write!(f, "single-writer, snapshot-read")
            }
        }
    }
}

impl Storage {
    /// 宣言済みの分離レベルを返す（TABLE-3「分離レベルを確認する」操作）。
    ///
    /// `redb` の契約（`begin_write` の排他ロックによる直列化・`begin_read` のスナップ
    /// ショット分離）をそのまま宣言する固定値であり、本メソッドの呼び出し自体は何も
    /// 検証しない。実際に直列化・スナップショット性が成立していることの検証は
    /// [`Storage::begin_read`]・[`Storage::begin_write`] を使う統合テストの責務
    /// （`crates/engine/tests/txn_isolation.rs` 参照）。
    pub fn isolation_level(&self) -> IsolationLevel {
        IsolationLevel::SingleWriterSnapshotRead
    }

    /// スナップショット読み取りトランザクションを開始する（TABLE-3）。
    ///
    /// 戻り値の [`ReadSnapshot`] は開始時点でコミット済みの状態のみを見る。以降に
    /// 他のライタがコミットしても、このスナップショットが返す結果には反映されない
    /// （`redb::ReadTransaction` の契約をそのまま公開する）。
    pub fn begin_read(&self) -> crate::storage::Result<ReadSnapshot> {
        let txn = self.db().begin_read()?;
        Ok(ReadSnapshot { txn })
    }

    /// 書き込みトランザクションを開始する（TABLE-3。バッチ台帳を使わない素の複数行
    /// コミット用）。
    ///
    /// `redb::Database::begin_write` の排他ロックにより他の書き込みトランザクションと
    /// 直列化される（同時に開けるのは 1 本のみ。先行トランザクションが commit/abort
    /// するまで本呼び出しはブロックする）。戻り値の [`WriteTxn`] は明示的に
    /// [`WriteTxn::commit`] するまで変更を確定しない（[`WriteTxn`] のドキュメント
    /// コメント参照）。
    ///
    /// [`ROWS_TABLE`] + [`crate::storage::BATCH_LOG_TABLE`] を同一トランザクションで
    /// 扱う 2 テーブル横断コミット（TASK-90、対象ビヘイビア: TABLE-10）が必要な場合は
    /// [`Storage::begin_batch_write`] を使うこと。[`WriteTxn`] にはバッチ台帳を操作する
    /// API を持たせていない（PR #129 codex レビュー PRRT_kwDOUAKASM6bbyWf 対応。
    /// [`BatchWriteTxn`] のドキュメントコメントの「型分離の理由」参照）。
    /// 本メソッドが返す [`WriteTxn`] はバッチ台帳の不変条件の適用範囲外（Issue #133・
    /// `docs/design/batch-ledger-scope.md` 参照）。
    ///
    /// 設計メモ: この排他ロックはタイムアウトを持たず、呼び出し元が [`WriteTxn`] を
    /// 保持し続ける限り他の書き込みを無期限にブロックする。本モジュール自体には
    /// untrusted 入力からの到達経路はないが、後続の SQL surface（TASK-85〜）から
    /// 呼び出す際は、無制限ブロック（DoS）を防ぐ上位側のタイムアウト・キャンセル
    /// 制御を検討すること（[`Storage::begin_batch_write`] も同じ排他ロック契約）。
    pub fn begin_write(&self) -> crate::storage::Result<WriteTxn> {
        let txn = self.begin_write_txn()?;
        Ok(WriteTxn {
            txn,
            has_writes: false,
        })
    }

    /// バッチ台帳付き書き込みトランザクションを開始する（TASK-90、対象ビヘイビア:
    /// TABLE-10。[`ROWS_TABLE`] + [`crate::storage::BATCH_LOG_TABLE`] を同一
    /// トランザクションで扱う 2 テーブル横断コミット用）。
    ///
    /// 排他ロックの契約は [`Storage::begin_write`] と同じ（同一 `redb::Database` の
    /// 書き込みトランザクションを共有するため、[`WriteTxn`] と [`BatchWriteTxn`] は
    /// 互いに同時オープンできない）。戻り値の [`BatchWriteTxn`] が保証する不変条件は
    /// [`BatchWriteTxn`] のドキュメントコメント参照。
    pub fn begin_batch_write(&self) -> crate::storage::Result<BatchWriteTxn> {
        let txn = self.begin_write_txn()?;
        Ok(BatchWriteTxn {
            txn,
            pending_row_count: 0,
            has_writes: false,
        })
    }
}

/// [`Storage::begin_read`] が返すスナップショット読み取りハンドル（TABLE-3）。
///
/// 開始時点でコミット済みの状態のみを見る。保持している間は基盤の
/// `redb::ReadTransaction` を握り続けるが、`Storage` を直接借用はしない
/// （`redb::ReadTransaction` 自体が内部で必要な参照カウントを保持する契約のため）。
pub struct ReadSnapshot {
    txn: redb::ReadTransaction,
}

impl ReadSnapshot {
    /// テナント ID・行 ID を指定して 1 行取得する（[`Storage::get`] と同じデコード契約・
    /// 物理キー契約。対象ビヘイビア: TABLE-12）。
    ///
    /// このスナップショットの開始後にコミットされた変更（他ライタによる書き込みを
    /// 含む）は反映されない。
    pub fn get(&self, tenant_id: &str, id: u64) -> crate::storage::Result<Row> {
        let table = match self.txn.open_table(ROWS_TABLE) {
            Ok(t) => t,
            // テーブル未作成（1 行も書き込んでいない）は「存在しない」として扱う
            // （storage.rs の Storage::get と同方針）。
            Err(redb::TableError::TableDoesNotExist(_)) => return Err(StorageError::NotFound(id)),
            Err(e) => return Err(map_rows_table_error(e)),
        };
        let guard = table
            .get((tenant_id, id))?
            .ok_or(StorageError::NotFound(id))?;
        decode_row_for_key(tenant_id, id, guard.value())
    }

    /// 内部 `redb::ReadTransaction` への `pub(crate)` アクセサ（[`Storage::db`] と
    /// 同じ「型そのものをリークさせない最小限の穴」方針）。`catalog`／
    /// `constraint` モジュールが、本型の [`Self::get`] が扱わない `user_rows/{table}`・
    /// `user_uniq/{table}` を読み取り専用で開くために使う（Issue #1070・PR #1123
    /// レビュー対応: `EngineCore::verify_unique_index_read_only` の読み取り専用
    /// 索引検証が、書き込み経路の遅延バックフィルを経由せずに済むようにする）。
    pub(crate) fn raw_txn(&self) -> &redb::ReadTransaction {
        &self.txn
    }
}

/// [`Storage::begin_write`] が返す書き込みトランザクションハンドル（TABLE-3。バッチ
/// 台帳を使わない素の複数行コミット専用）。
///
/// `redb::Database::begin_write` の排他ロックにより、生存している間は他の書き込み
/// トランザクションの `begin_write`／`begin_batch_write` をブロックする（直列化）。
/// [`WriteTxn::commit`] を呼ぶまで、書き込んだ行は他のトランザクションから見えない。
///
/// [`WriteTxn::commit`]・[`WriteTxn::abort`] のどちらも呼ばずに drop した場合は、
/// 内部の `redb::WriteTransaction` の `Drop` 実装により自動的に abort される
/// （redb 4.2.0 の契約。書き込みは確定せず、排他ロックは解放される）。
///
/// バッチ台帳（[`crate::storage::BATCH_LOG_TABLE`]）を操作する API は持たない。
/// TABLE-10 の 2 テーブル横断コミットが必要な場合は [`BatchWriteTxn`]（
/// [`Storage::begin_batch_write`] が返す）を使うこと（型分離の理由は
/// [`BatchWriteTxn`] のドキュメントコメント参照）。
pub struct WriteTxn {
    txn: crate::storage::GatedWriteTxn,
    /// このハンドルを通じて redb の [`ROWS_TABLE`] に触れる操作（[`Self::put`]）を
    /// 1 回でも行ったか（Issue #175・TASK-133 P2 対応）。`commit` 時にこれが
    /// `false` なら世代カウンタを進めない判断根拠になる。**redb テーブルに触れる
    /// 操作を本型に追加する場合は、必ずその直前で本フラグを立てること**
    /// （立て忘れは `crate::rls::PrefilterIndex` の失効検出を素通りさせる見逃し
    /// 失効＝RLS 相当のテナント境界チェックの fail-open に直結する P0 事項）。
    has_writes: bool,
}

impl WriteTxn {
    /// 単一行を書き込む（commit するまで確定しない。[`Storage::put`] と同じ
    /// upsert セマンティクス・エンコーディング契約）。新規挿入・既存 ID への
    /// 上書きのどちらも [`Self::has_writes`] を立てる対象とする（上書きは
    /// tenant_id・visibility・embedding を変え得るため、`PrefilterIndex` の可視集合・
    /// 内容に影響するため、見逃すと fail-open になる）。
    pub fn put(&mut self, id: u64, row: &RowInput<'_>) -> crate::storage::Result<()> {
        let encoded = encode_row(row)?;
        self.has_writes = true;
        let mut table = self
            .txn
            .open_table(ROWS_TABLE)
            .map_err(map_rows_table_error)?;
        table.insert((row.tenant_id, id), encoded.as_slice())?;
        Ok(())
    }

    /// トランザクションをコミットし、書き込みを確定する（[`commit_write_txn_guarded`]
    /// 経由。TASK-133 P1・Issue #175、および TASK-96・対象ビヘイビア: RECOVER-5 の
    /// commit 成功境界対応）。[`Self::put`] を 1 度も呼ばずに commit した場合は
    /// 世代カウンタを進めない（実書き込みがなければ `PrefilterIndex` を失効させる
    /// 理由がない）。1 度でも呼んでいれば（上書きのみの場合を含む）必ず世代を進める。
    pub fn commit(self) -> crate::storage::Result<()> {
        commit_write_txn_guarded(self.txn, self.has_writes)
    }

    /// トランザクションを明示的に中断し、書き込みを破棄する。
    pub fn abort(self) -> crate::storage::Result<()> {
        self.txn.abort()?;
        Ok(())
    }
}

/// [`Storage::begin_batch_write`] が返す、バッチ台帳付き書き込みトランザクション
/// ハンドル（TASK-90、対象ビヘイビア: TABLE-10。[`ROWS_TABLE`] +
/// [`crate::storage::BATCH_LOG_TABLE`] を同一トランザクションで扱う）。
///
/// # 型分離の理由（PR #129 codex レビュー PRRT_kwDOUAKASM6bbyWf 対応）
///
/// 当初は `WriteTxn` 1 型に `log_batch` を持たせ、「`log_batch` を 1 回以上呼んだ
/// トランザクションのみ commit 時に未台帳行を検証する」というインスタンス単位の
/// フラグで TABLE-3 用途と TABLE-10 用途を両立させていた。しかしこの設計では、
/// 同一 DB に対して「台帳を使わない `put` だけの `WriteTxn` で commit」→「別
/// トランザクションで台帳ありの `put` + `log_batch` を commit」という**トランザクション
/// をまたいだ**操作の組み合わせにより、台帳を経由しない行を公開 API だけで作り込め、
/// 「台帳の row_count 合計 == 行総数」という不変条件を破れてしまう
/// （codex レビューが指摘した具体例）。そこで TABLE-3 用の `WriteTxn` からは
/// `log_batch` を完全に撤去し、TABLE-10 用の `BatchWriteTxn` を独立した型として
/// 分離した。
///
/// # 契約の適用範囲（重要・恒久契約）
///
/// 型分離をもってしても、[`Storage::put`]・[`Storage::put_batch`]・
/// [`WriteTxn::put`] は本モジュールとは独立に [`ROWS_TABLE`] へ直接書き込めるため、
/// 「台帳の row_count 合計 == 行総数」という不変条件を DB 全体に対して型システムで
/// 強制することはできない。この不変条件が保証されるのは、**あるテーブルへの書き込みを
/// `BatchWriteTxn` だけで行った場合に限る**（[`crate::storage::BATCH_LOG_TABLE`] の
/// ドキュメントコメント参照）。`BatchWriteTxn` と他の書き込み経路を同一 DB・同一
/// テーブルに対して混在させないことは呼び出し元の責務であり、本モジュールは意図的に
/// それを検出・拒否しない。
///
/// 適用範囲の検討経緯（Issue #133）は `docs/design/batch-ledger-scope.md` にポインタを、
/// 判断の詳細は private spec 側 `docs/spec/05-tasks.md`（TASK-90・TASK-93）に記載する。
///
/// `BatchWriteTxn` 単体としては、以下をすべて満たしてはじめて `commit` が成功する:
/// - 新規挿入した行はすべて [`BatchWriteTxn::log_batch`] で台帳へ記録済みであること
///   （[`StorageError::UnloggedRows`]）
/// - `log_batch` は 1 件以上の新規挿入を伴って呼ばれること（[`StorageError::EmptyBatch`]）
/// - `batch_seq` は重複しないこと（[`StorageError::DuplicateBatchSeq`]）
/// - 同一 ID への 2 回目以降の `put`（upsert による上書き）は行数としてカウントしない
pub struct BatchWriteTxn {
    txn: crate::storage::GatedWriteTxn,
    /// 直近の [`BatchWriteTxn::log_batch`] 呼び出し以降（または `BatchWriteTxn`
    /// 生成以降）に [`BatchWriteTxn::put`] で**新規挿入**した件数（既存 ID への
    /// 上書きは含まない）。呼び出し元から任意の行数を申告させず自身が実書き込みを
    /// 数えることで、「台帳の値 == そのバッチで `ROWS_TABLE` へ新規追加した行数」
    /// という契約を保証する（PR #129 codex レビュー PRRT_kwDOUAKASM6bbQ7l・
    /// PRRT_kwDOUAKASM6bbc_I 対応）。
    pending_row_count: u64,
    /// このハンドルを通じて redb のテーブル（[`ROWS_TABLE`]・[`BATCH_LOG_TABLE`]）に
    /// 触れる操作（[`Self::put`]・[`Self::log_batch`]）を 1 回でも行ったか（Issue #175・
    /// TASK-133 P2 対応）。`pending_row_count` は「直近の `log_batch` 以降の未台帳
    /// 新規挿入数」であり `log_batch` 後は 0 に戻る・上書きを数えないため、これを
    /// 「一度も書き込んでいないか」の判定には使えない（`pending_row_count == 0` の
    /// commit を no-op と誤認すると、上書きのみのバッチや `log_batch` 済みバッチの
    /// 世代更新を見逃す fail-open になる）。本フラグを別途持つのはそのため。
    /// **redb テーブルに触れる操作を本型に追加する場合は、必ずその直前で本フラグを
    /// 立てること**（立て忘れは `WriteTxn` の同名フィールドと同じ理由で P0）。
    has_writes: bool,
}

impl BatchWriteTxn {
    /// 単一行を書き込む（commit するまで確定しない。[`Storage::put`] と同じ
    /// upsert セマンティクス・エンコーディング契約）。[`BatchWriteTxn::log_batch`] 向けの
    /// 内部カウンタ（`pending_row_count`）は、`redb::Table::insert` の戻り値
    /// （上書き前の旧値）を見て**新規挿入のときのみ**増やす。同一 ID への 2 回目以降の
    /// `put`（上書き）はカウントしない。カウントしてしまうと同一 ID への複数回 put
    /// だけで実在行数より多い値を台帳へ記録でき、「台帳の row_count 合計 == 行総数」
    /// という TABLE-10 の契約を公開 API だけで破れてしまうため
    /// （PR #129 codex レビュー PRRT_kwDOUAKASM6bbc_I 対応）。
    ///
    /// 物理キーは `(row.tenant_id, id)`（対象ビヘイビア: TABLE-12）。同一 `id` でも
    /// テナントが異なれば別キー＝新規挿入として数える（別テナントの行が「台帳の
    /// row_count 合計 == 行総数」契約と整合する）。
    pub fn put(&mut self, id: u64, row: &RowInput<'_>) -> crate::storage::Result<()> {
        let encoded = encode_row(row)?;
        // 新規挿入・上書きのどちらも実書き込みとして扱う（Issue #175。
        // `pending_row_count` の増分条件とは独立に立てる。上書きのみのバッチは
        // `pending_row_count` が増えないため、これを根拠にすると commit で
        // 世代を進め忘れる fail-open になる）。
        self.has_writes = true;
        let mut table = self
            .txn
            .open_table(ROWS_TABLE)
            .map_err(map_rows_table_error)?;
        let previous = table.insert((row.tenant_id, id), encoded.as_slice())?;
        if previous.is_none() {
            // 呼び出し元にはトランザクション内で扱える行数の上限（TASK-90 の想定用途
            // ではバッチ 1 本あたり高々数千件）を課しており u64 を溢れさせることは
            // できないが、coding-rust.md の checked 演算方針に合わせ、万一の溢れを
            // 黙って折り返さず fail-closed に検出する。
            self.pending_row_count = self
                .pending_row_count
                .checked_add(1)
                .ok_or(StorageError::PendingRowCountOverflow)?;
        }
        Ok(())
    }

    /// バッチ台帳（[`crate::storage::BATCH_LOG_TABLE`]）へ、直近の `log_batch` 以降に
    /// 実際に [`BatchWriteTxn::put`] で**新規挿入**した行数（既存 ID への上書きは
    /// 含まない）を 1 エントリとして書き込む（TASK-90、対象ビヘイビア: TABLE-10）。
    /// 同一 ID への複数回 `put` は `pending_row_count` を増やさないため、
    /// `log_batch(seq, 99)` のような過大申告だけでなく「同一 ID への put の
    /// 繰り返し」でも実在行数を超える値を記録できない
    /// （PR #129 codex レビュー PRRT_kwDOUAKASM6bbc_I 対応）。[`BatchWriteTxn::put`] と
    /// 同一の `redb::WriteTransaction`（`self.txn`）内で操作するため、
    /// [`BatchWriteTxn::commit`]・[`BatchWriteTxn::abort`] のどちらを呼んでも
    /// [`ROWS_TABLE`] への行書き込みと本エントリは常に運命を共にする（2 テーブル
    /// 横断で原子的にコミット／破棄される）。
    ///
    /// `row_count` を呼び出し元から受け取らないのは、任意の値を渡せると
    /// `log_batch(seq, 0)` のみ・過大申告といった「実書き込み数と独立した値」を
    /// 公開 API だけで永続化できてしまい、台帳の契約を保証できないため
    /// （PR #129 codex レビュー PRRT_kwDOUAKASM6bbQ7l 対応）。呼び出し成功後は
    /// カウンタを 0 にリセットし、次の `log_batch` は「その後の `put` 件数」のみを
    /// 記録する。
    ///
    /// 直近の `log_batch`（または `BatchWriteTxn` 生成）以降 1 件も新規挿入していない
    /// 状態（`pending_row_count == 0`）で呼ぶと [`StorageError::EmptyBatch`] で
    /// fail-closed に拒否する。ゼロ件エントリを許すと、クラッシュ検証ツールの検証
    /// オラクル（台帳の各エントリ値 == 実際にコミットされたバッチサイズ）と食い違う
    /// ため（PR #129 codex レビュー PRRT_kwDOUAKASM6bbnm7 対応）。
    ///
    /// 既存の `batch_seq` を渡すと [`StorageError::DuplicateBatchSeq`] で fail-closed に
    /// 拒否する（`redb` の `insert` は無条件上書きのため、検出しないと呼び出し元の
    /// 採番バグ・再試行ミスがバッチ台帳の不変条件（`batch_seq` ごとに 1 エントリ）を
    /// 静かに破壊する。security.md「不安全な設計」対応）。
    pub fn log_batch(&mut self, batch_seq: u64) -> crate::storage::Result<()> {
        if self.pending_row_count == 0 {
            return Err(StorageError::EmptyBatch);
        }
        // ここから先は redb の `BATCH_LOG_TABLE` に触れる（Issue #175）。
        // 上の `EmptyBatch` 早期 return は redb に一切触れないため対象外。
        self.has_writes = true;
        let mut table = self.txn.open_table(BATCH_LOG_TABLE)?;
        if table.get(batch_seq)?.is_some() {
            return Err(StorageError::DuplicateBatchSeq(batch_seq));
        }
        table.insert(batch_seq, self.pending_row_count)?;
        self.pending_row_count = 0;
        Ok(())
    }

    /// トランザクションをコミットし、書き込みを確定する。
    ///
    /// 直近の `log_batch` 以降に新規挿入したのに台帳へ記録していない行
    /// （`pending_row_count != 0`）がないかを常に検証し、あれば
    /// [`StorageError::UnloggedRows`] で fail-closed に拒否する。`BatchWriteTxn` は
    /// TABLE-10 専用の型であり（[`WriteTxn`] と分離済み）、この検証を無条件に適用
    /// してよい（PR #129 codex レビュー PRRT_kwDOUAKASM6bbnm4・PRRT_kwDOUAKASM6bbyWf
    /// 対応）。
    ///
    /// 世代カウンタの制御は `has_writes`（[`Self::put`]・[`Self::log_batch`] のいずれかを
    /// 1 回でも呼んだか）で判断し、[`commit_write_txn_guarded`] に委譲する（Issue #175・
    /// TASK-133 P2、TASK-96・RECOVER-5 対応）。`pending_row_count == 0` を
    /// 「一度も書き込んでいない」判定に
    /// **使わない**: `log_batch` 成功後や上書きのみのバッチでも 0 になり得るため、これを
    /// 根拠にすると実際に書き込み済みのバッチを誤って no-op 扱いし、世代更新を見逃す
    /// fail-open になる（`has_writes` を導入した理由そのもの）。
    pub fn commit(self) -> crate::storage::Result<()> {
        if self.pending_row_count != 0 {
            return Err(StorageError::UnloggedRows(self.pending_row_count));
        }
        commit_write_txn_guarded(self.txn, self.has_writes)
    }

    /// トランザクションを明示的に中断し、書き込みを破棄する。
    pub fn abort(self) -> crate::storage::Result<()> {
        self.txn.abort()?;
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    //! `WriteTxn`/`BatchWriteTxn` の commit 経路が「実書き込みの有無」に基づいて
    //! 世代カウンタを制御することを検証する（Issue #175・TASK-133 P2 対応）。
    //! 見逃し失効（fail-open）方向の退行がないことを重点的に確認する
    //! （上書きのみの commit・`log_batch` 済みで `pending_row_count == 0` の commit を
    //! no-op と誤認しないケース）。

    use super::*;
    use crate::storage::{RowInput, Visibility};

    /// テストごとに一意な DB ファイルパスを払い出す（storage.rs の同名ヘルパーと同じ方針）。
    fn unique_db_path(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "fandhe-db-engine-txn-{label}-{}-{seq}.redb",
            std::process::id()
        ));
        path
    }

    struct CleanupGuard(std::path::PathBuf);
    impl Drop for CleanupGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn write_txn_commit_without_put_does_not_bump_generation() {
        let path = unique_db_path("write-noop");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let txn = storage.begin_write().expect("begin_write");
        txn.commit().expect("commit without put");

        assert_eq!(
            storage.current_generation().expect("current_generation"),
            0,
            "put を 1 度も呼ばない commit は世代を進めないこと"
        );
    }

    #[test]
    fn write_txn_commit_with_new_row_bumps_generation() {
        let path = unique_db_path("write-new-row");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let mut txn = storage.begin_write().expect("begin_write");
        txn.put(
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0],
                metadata: &[],
            },
        )
        .expect("put");
        txn.commit().expect("commit with put");

        assert_eq!(
            storage.current_generation().expect("current_generation"),
            1,
            "新規行の put を伴う commit は世代を進めること"
        );
    }

    #[test]
    fn write_txn_commit_with_overwrite_of_existing_id_bumps_generation() {
        let path = unique_db_path("write-overwrite");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .put(
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0],
                    metadata: &[],
                },
            )
            .expect("initial put");
        let generation_after_setup = storage.current_generation().expect("current_generation");

        // 同一 tenant_id・同一 id（= 同一物理キー）で visibility・embedding を変えて
        // 上書き。tenant_id を変えると物理キーが (tenant_id, id) の複合キーのため
        // 別行への新規挿入になってしまい「上書き」の検証にならない（TABLE-12）。
        // 上書きは pending_row_count のような新規カウンタでは検出できないため、
        // fail-open ガードとして重要（2.1 節「実書き込みの定義」参照）。
        let mut txn = storage.begin_write().expect("begin_write");
        txn.put(
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Private,
                embedding: &[9.0],
                metadata: &[],
            },
        )
        .expect("overwrite put");
        txn.commit().expect("commit with overwrite");

        assert_eq!(
            storage.current_generation().expect("current_generation"),
            generation_after_setup + 1,
            "既存 ID への上書き put を伴う commit は世代を進めること（fail-open ガード）"
        );
    }

    #[test]
    fn write_txn_abort_after_put_does_not_bump_generation() {
        let path = unique_db_path("write-abort");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let mut txn = storage.begin_write().expect("begin_write");
        txn.put(
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0],
                metadata: &[],
            },
        )
        .expect("put");
        txn.abort().expect("abort");

        assert_eq!(
            storage.current_generation().expect("current_generation"),
            0,
            "put 後の abort は書き込みを破棄し世代を進めないこと（既存挙動の固定）"
        );
    }

    #[test]
    fn batch_write_txn_commit_without_put_or_log_batch_does_not_bump_generation() {
        let path = unique_db_path("batch-noop");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let txn = storage.begin_batch_write().expect("begin_batch_write");
        txn.commit().expect("commit without put/log_batch");

        assert_eq!(
            storage.current_generation().expect("current_generation"),
            0,
            "put/log_batch を 1 度も呼ばない commit は世代を進めないこと"
        );
    }

    #[test]
    fn batch_write_txn_commit_after_put_and_log_batch_bumps_generation() {
        let path = unique_db_path("batch-put-log");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let mut txn = storage.begin_batch_write().expect("begin_batch_write");
        txn.put(
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: &[1.0],
                metadata: &[],
            },
        )
        .expect("put");
        txn.log_batch(0).expect("log_batch");
        txn.commit()
            .expect("commit with pending_row_count == 0 after log_batch");

        assert_eq!(
            storage.current_generation().expect("current_generation"),
            1,
            "log_batch 済みで pending_row_count == 0 の commit も世代を進めること"
        );
    }

    #[test]
    fn batch_write_txn_commit_with_overwrite_only_bumps_generation() {
        let path = unique_db_path("batch-overwrite-only");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .put(
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0],
                    metadata: &[],
                },
            )
            .expect("initial put");
        let generation_after_setup = storage.current_generation().expect("current_generation");

        // 同一 tenant_id・同一 id（= 同一物理キー）で上書き。tenant_id を変えると
        // 物理キーが (tenant_id, id) の複合キーのため別行への新規挿入になり
        // （previous.is_none() == true）、本テストが検証したい「新規挿入 0 件」の
        // 前提が崩れて log_batch 必須（UnloggedRows）になってしまう（TABLE-12）。
        // 新規挿入が 0 件（既存 ID の上書きのみ）なので pending_row_count は 0 の
        // ままであり、log_batch は不要（呼ぶと EmptyBatch で拒否される）。
        // pending_row_count を根拠にすると commit で世代更新を見逃す核心ケース。
        let mut txn = storage.begin_batch_write().expect("begin_batch_write");
        txn.put(
            1,
            &RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Private,
                embedding: &[9.0],
                metadata: &[],
            },
        )
        .expect("overwrite put");
        txn.commit().expect("commit with overwrite-only batch");

        assert_eq!(
            storage.current_generation().expect("current_generation"),
            generation_after_setup + 1,
            "上書きのみ（新規挿入 0 件）の commit も世代を進めること（fail-open ガード）"
        );
    }

    #[test]
    fn batch_write_txn_rejected_empty_log_batch_then_commit_does_not_bump_generation() {
        let path = unique_db_path("batch-empty-log-batch");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let mut txn = storage.begin_batch_write().expect("begin_batch_write");
        let result = txn.log_batch(0);
        assert!(matches!(result, Err(StorageError::EmptyBatch)));
        txn.commit().expect("commit after rejected log_batch");

        assert_eq!(
            storage.current_generation().expect("current_generation"),
            0,
            "EmptyBatch で拒否された log_batch は redb に触れないため、\
             その後の commit は世代を進めないこと"
        );
    }

    /// 取りこぼし検査（Issue #849）: 宣言済み分離レベルの書き込みトランザクション API
    /// （`begin_write`・`begin_batch_write`）が
    /// [`crate::storage::Storage::begin_write_txn`] choke point を経由することを、
    /// 呼び出し前後の `write_txn_creations()` の差分で非 vacuous に確認する。
    #[test]
    fn begin_write_and_begin_batch_write_go_through_storage_choke_point() {
        let path = unique_db_path("durability-choke-point-txn");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        assert_eq!(storage.write_txn_creations(), 0);

        let txn = storage.begin_write().expect("begin_write");
        txn.commit().expect("commit");
        assert_eq!(storage.write_txn_creations(), 1, "begin_write");

        let txn = storage.begin_batch_write().expect("begin_batch_write");
        txn.commit().expect("commit");
        assert_eq!(storage.write_txn_creations(), 2, "begin_batch_write");
    }
}
