//! `redb` ベースの永続化層（TASK-140/TASK-141、対象ビヘイビア: PERSIST-1, PERSIST-2,
//! PERSIST-3, PERSIST-4。ポインタ: `docs/spec/05-tasks.md` TASK-140・TASK-141・
//! `docs/spec/04-behavior/persistence.md`）。
//!
//! 行ストア（[`ROWS_TABLE`]）の物理キーは `(tenant_id, id)` の複合キー（対象ビヘイビア:
//! TABLE-12。ポインタ: `docs/spec/04-behavior/data-model.md` TABLE-12・
//! `docs/spec/05-tasks.md` TASK-89・TASK-95）。旧フォーマット（`id` 単独キー）の DB は
//! [`StorageError::IncompatibleRowKeyFormat`] で fail-closed に拒否し、マイグレーションは
//! 提供しない（Issue #206。`catalog.rs::UserRowsTableDef` と同一契約）。
//!
//! 責務境界: ベクトル行（id・テナント ID・可視性ラベル・埋め込み・メタデータ）の永続化 API を
//! 提供する。後続の検索カーネル・カタログ層（TASK-124〜、TASK-85〜）から呼び出される想定で、
//! 本モジュールは `tenant_id`・`visibility` をスキーマとして同居保持するが、
//! ポリシー評価（可視性判定・RLS 事前フィルタ）そのものは行わない（評価は TASK-133 以降の
//! 呼び出し元の責務）。呼び出し元がメタデータ列にどのようなバイト列を格納するかは別途決める。
//!
//! 分離レベル（PERSIST-4）: `redb` の契約をそのまま宣言する。書き込みトランザクションは
//! `redb::Database::begin_write` が排他ロックを取ることで直列化され、読み取りトランザクション
//! （`begin_read`）は開始時点のスナップショットを見る（進行中の未コミット書き込みは見えない）。
//! 本モジュールは独自のロック層を追加しない。

use std::fmt;
use std::path::Path;

use redb::{ReadableDatabase, ReadableTable, TableDefinition};

/// 電源断シミュレーション用 `redb::StorageBackend` の共通モデル（TASK-145）。
/// 本クレートのユニットテスト（本モジュール下部の `tests::power_loss`）と
/// `crates/engine/tests/power_loss.rs` 統合テストの双方から `#[path]` 経由で
/// 同一ソースを取り込む。テストコードのみで使うため通常ビルドには含めない。
#[cfg(test)]
mod power_loss_model;
pub(crate) mod read_source;
pub(crate) mod writer_gate;

/// 行ストアの物理キー型（対象ビヘイビア: TABLE-12。ポインタ:
/// `docs/spec/04-behavior/data-model.md` TABLE-12）。キーはテナント ID・行 `id` の
/// 複合キーで、行 `id` の一意性スコープをテナント内に閉じる。異なるテナントは同一の
/// `id` を独立に保持できる（`crate::catalog::UserRowsTableDef` と同一契約。テーブル名
/// だけが異なる）。`redb` のタプルキーは要素順（tenant_id 昇順 → id 昇順）で全順序を
/// 持つため、全件走査は従来どおり単一の range 走査で列挙できる。
///
/// `pub(crate)`: `catalog.rs`（TASK-89・TASK-95）の `UserRowsTableDef` は本型への
/// エイリアスであり、行ストアのキー型定義を本モジュールへ一元化する（PR #194 で
/// `catalog.rs` 側にも同型を個別定義していたキー型の二重化を Issue #206 で解消）。
pub(crate) type RowStoreTableDef<'a> = TableDefinition<'a, (&'static str, u64), &'static [u8]>;

/// 行データを格納するテーブル。物理キーは [`RowStoreTableDef`]（`(tenant_id, id)`
/// 複合キー）、値は [`encode_row`] でエンコードしたバイト列。テーブル名は `docs/spec`
/// 側の成果物指定に依存しないローカルな識別子。
///
/// 旧フォーマット（物理キーが `id` 単独）の DB を開くと `redb` は
/// `TableError::TableTypeMismatch` を返す。これを黙って握りつぶすと旧行を
/// 別テナントの行として扱う fail-open になりうるため、[`map_rows_table_error`] で
/// [`StorageError::IncompatibleRowKeyFormat`] へ明示的に写像して拒否する
/// （マイグレーションは提供しない。TABLE-12 の物理キー変更に伴う恒久契約。
/// `catalog.rs::map_row_table_error` と同型）。
///
/// `pub(crate)`: `txn.rs`（TASK-88・TABLE-3）が `Storage` の公開 API を経由せず、
/// 同一テーブルに対する読み取りスナップショット・書き込みトランザクションハンドルを
/// 直接構築するために参照する。
pub(crate) const ROWS_TABLE: RowStoreTableDef<'static> = TableDefinition::new("rows");

/// [`Storage::scan_page`] のページングカーソル（行ストアの物理キーと同形の
/// `(tenant_id, id)`。対象ビヘイビア: TABLE-12。`id` 単独では再開位置を表現できない）。
/// 呼び出し元がカーソルを保持できるよう所有形（`String`）で返す。
///
/// `catalog.rs::RowCursor` は本型の re-export（生成点を本モジュールへ一元化する）。
pub type RowCursor = (String, u64);

/// [`ROWS_TABLE`]（[`RowStoreTableDef`]）専用の `open_table` エラー写像。
///
/// `TableError::TableTypeMismatch`（旧 `id` 単独キー形式の DB）を
/// [`StorageError::IncompatibleRowKeyFormat`] へ写像し fail-closed に拒否する。
/// `catalog.rs::map_row_table_error`（[`crate::catalog::UserRowsTableDef`] 専用）と
/// 同型のパターン。
pub(crate) fn map_rows_table_error(e: redb::TableError) -> StorageError {
    match e {
        redb::TableError::TableTypeMismatch { .. } => StorageError::IncompatibleRowKeyFormat,
        other => StorageError::from(other),
    }
}

/// バッチ台帳テーブル（TASK-90、対象ビヘイビア: TABLE-10。ポインタ:
/// `docs/spec/05-tasks.md` TASK-90・`docs/spec/04-behavior/data-model.md` TABLE-10）。
/// キーはバッチ通番（`batch_seq`、0 起点で連番）、値はそのバッチで [`ROWS_TABLE`] へ
/// 新規挿入した行数。[`ROWS_TABLE`] と同一の `redb::WriteTransaction` 内でのみ書き込む
/// （`txn.rs` の [`crate::txn::BatchWriteTxn::log_batch`]）ことで、2 テーブル横断の
/// トランザクション原子性（同時にコミットされる／同時に破棄される）を製品コード経路で
/// 成立させる。[`crate::recovery::ledger::OP_LEDGER_TABLE`]（`operation_id` 台帳。
/// TASK-93、対象ビヘイビア: RECOVER-2。行の書き込み/更新/削除と同一トランザクション
/// 内で原子的に追記する）と同型のパターンであり、本テーブル自体はテスト専用の
/// 使い捨てではない。
///
/// **契約の適用範囲（重要・恒久契約）**: 「台帳の row_count 合計 == [`ROWS_TABLE`] の
/// 行総数」という不変条件は、[`crate::txn::BatchWriteTxn`] だけを使って [`ROWS_TABLE`] へ
/// 書き込んだ場合にのみ [`crate::txn::BatchWriteTxn`] の公開 API（`DuplicateBatchSeq`・
/// `EmptyBatch`・`UnloggedRows`・上書き非カウントの各チェック）によって保証される。
/// [`Storage::put`]・[`Storage::put_batch`]・[`crate::txn::WriteTxn::put`]（バッチ台帳を
/// 経由しない別経路）は台帳を一切更新せず [`ROWS_TABLE`] に直接書き込めるため、これらと
/// [`crate::txn::BatchWriteTxn`] を同一 DB・同一テーブルに対して混在させると、上記の
/// 不変条件は成立しなくなる。これは型システムでは検出できない呼び出し元の責務であり、
/// 本モジュールは意図的にそれを強制しない（PR #129 codex レビュー PRRT_kwDOUAKASM6bbyWf
/// 対応。「台帳合計 == 行総数」を保証する範囲を、実際に型で保証できる範囲まで明文化して
/// 限定した）。TABLE-10 の不変条件が必要な呼び出し元は、対象テーブルへの書き込みを
/// [`crate::txn::BatchWriteTxn`] に一本化すること。
///
/// 適用範囲の検討経緯（Issue #133）は `docs/design/batch-ledger-scope.md` にポインタを、
/// 判断の詳細は private spec 側 `docs/spec/05-tasks.md`（TASK-90・TASK-93）に記載する。
pub(crate) const BATCH_LOG_TABLE: TableDefinition<u64, u64> = TableDefinition::new("batch_log");

/// ストレージ全体の書き込み世代カウンタ（TASK-133 P1・対象ビヘイビア: RLS-1〜4）。
/// 単一の固定キー [`GENERATION_KEY`] に対する値（u64）を [`bump_generation_and_commit`]
/// だけが更新する。`crate::rls::PrefilterIndex` が構築後の失効検出に使う。
pub(crate) const GENERATION_TABLE: TableDefinition<&str, u64> =
    TableDefinition::new("storage_generation");

/// [`GENERATION_TABLE`] の唯一のキー（ストレージ全体で 1 カウンタ）。
const GENERATION_KEY: &str = "generation";

/// 行エンコーディングの先頭バイト。v2（TASK-141）で RLS フィールド（`tenant_id`・
/// `visibility`）を同居させるレイアウトへ拡張した。v1 の行は RLS フィールドを持たず、
/// 暗黙のデフォルトテナント・可視性で読み出すのは fail-open（P0 違反）になるため、
/// デコード側は v1 を含む未知バージョンを一律 fail-closed に拒否する
/// （マイグレーションは提供しない。実装着手直後で永続データの互換性保証は不要）。
const ROW_FORMAT_VERSION: u8 = 2;

/// `tenant_id` の UTF-8 バイト長上限。デコード時・エンコード時ともこの値を超える
/// 長さフィールドを `Vec`/`String` へのアロケーションに使う前に拒否する
/// （.claude/rules/coding-rust.md「untrusted 入力の扱い」）。
///
/// `pub(crate)`: `arena.rs`（TASK-87、対象ビヘイビア: TABLE-8）が、コールドスタート・
/// アリーナ構築時に `tenant_id` 文字列群の総メモリ量を見積もる上限値として参照する。
pub(crate) const MAX_TENANT_ID_LEN: u16 = 256;

/// 埋め込み次元数の上限。デコード時にこの値を超える `dim` を確認した場合、
/// `Vec::with_capacity` へ渡す前に拒否する（未検証の長さフィールドを無制限アロケーションに
/// 使わない。.claude/rules/coding-rust.md「untrusted 入力の扱い」）。
pub(crate) const MAX_EMBEDDING_DIM: u32 = 65_536;

/// メタデータ列のバイト長上限。埋め込みと同様、デコード前に上限検証する。
/// `row_codec::encode_scalar_columns` が複数 `TEXT` 列を連結して組み立てる出力
/// バッファの累計上限としても共有する（`pub(crate)`。列ごとの上限
/// `row_codec::MAX_TEXT_FIELD_LEN` だけでは列数分の掛け算で本値を大幅に超え得るため、
/// row_codec 側の確保前検証が本値と同期していることを下部の const assert で強制する）。
pub(crate) const MAX_METADATA_LEN: u32 = 4 * 1024 * 1024;

/// [`Storage::scan_page`] が 1 回の呼び出しで返す行数の上限。
/// [`Storage::scan`] は行数に上限がなく、最大サイズの行（[`MAX_EMBEDDING_DIM`]・
/// [`MAX_METADATA_LEN`] 相当）が大量にある場合に一度に巨大なメモリ確保が発生し得る
/// （security.md「不安全な設計｜無制限リソース確保（DoS）」）。後続タスクで検索カーネル等が
/// 永続化層に依存する際は、この上限付きページングを使う前提とする。
///
/// `pub(crate)`: `catalog.rs`（TASK-146・EXT-1, EXT-2）のテーブルスコープ
/// `scan_table_page` が、ここと同じ行数上限を適用するために再利用する。
pub(crate) const MAX_SCAN_PAGE_LIMIT: u32 = 10_000;

/// [`Storage::scan`] が一度に確保してよい行数の上限（無制限 `Vec` 確保を避けるための
/// 契約。security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。超過時は
/// [`StorageError::ScanLimitExceeded`] で fail-closed に拒否し、呼び出し元へは
/// 上限付きページング API [`Storage::scan_page`] の利用を促す。
const MAX_SCAN_TOTAL_ROWS: usize = 100_000;

/// [`Storage::scan`] が確保してよいデコード対象バイト量（エンコード済みバイト列長で近似）の
/// 上限。[`MAX_SCAN_TOTAL_ROWS`] だけでは最大サイズの行が並んだ場合に巨大確保になり得るため、
/// 行数とバイト量の両方で上限を課す（[`MAX_SCAN_PAGE_BYTES`] と同様の考え方）。
const MAX_SCAN_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// [`Storage::scan_page`] が 1 ページで確保するデコード後バイト量（embedding + metadata）の
/// 上限。行数の上限（[`MAX_SCAN_PAGE_LIMIT`]）だけでは、最大サイズ（[`MAX_EMBEDDING_DIM`]・
/// [`MAX_METADATA_LEN`] 相当）の行が並んだ場合に依然として数十 GB 規模の確保になり得るため、
/// 行数とバイト量の両方で上限を課す。
///
/// `pub(crate)`: [`MAX_SCAN_PAGE_LIMIT`] と同じ理由で `catalog.rs` の
/// `scan_table_page` が再利用する。
pub(crate) const MAX_SCAN_PAGE_BYTES: usize = 16 * 1024 * 1024;

/// [`Storage::scan_batch_log`] が一度に確保してよいエントリ数の上限（[`MAX_SCAN_TOTAL_ROWS`]
/// と同様、無制限 `Vec` 確保を避けるための契約。security.md「不安全な設計｜無制限リソース
/// 確保（DoS）」対応）。バッチ台帳の 1 エントリは固定 16 バイト（`u64` キー + `u64` 値）の
/// ため、この上限だけで確保量が頭打ちになる（[`MAX_SCAN_TOTAL_BYTES`] 相当のバイト上限は
/// 不要）。超過時は [`StorageError::ScanLimitExceeded`] で fail-closed に拒否する
/// （PR #193 codex レビュー PRRT_kwDOUAKASM6cCITT 対応: [`Storage::scan`] 用と variant を
/// 分けない。理由は [`StorageError::ScanLimitExceeded`] のドキュメンテーションコメント参照）。
/// 最大 `batch_seq` だけが必要な採番再開経路はこの上限に依存しない
/// [`Storage::batch_log_max_seq`] を使う。
const MAX_BATCH_LOG_ROWS: usize = 1_000_000;

/// [`Storage::writer_gate`] の待機上限の既定値（SQL-31・TASK-221。実装既定値で
/// spec 由来の数値ではない。ロック待ちで [`StorageError::WriteLockTimeout`]
/// （`55P03`）を返すまでの時間）。
pub const DEFAULT_WRITE_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// 永続化層の公開エラー型。`redb` の複数のエラー型（`DatabaseError` 等）はすべて
/// `redb::Error` へ変換可能なため、それを内部に保持して一本化する。
/// ライブラリコードとして panic せず、すべての失敗を `Result` で返す
/// （coding-rust.md: engine では `Result` を返し panic させない）。
#[derive(Debug)]
pub enum StorageError {
    /// `redb` 側で発生したエラー（I/O・破損検出・トランザクション競合等）。
    Backend(redb::Error),
    /// 行データのエンコード/デコードで検出した不正値。fail-closed に拒否する
    /// （欠落・不正値を黙殺フォールバックしない）。
    Codec(String),
    /// 指定した行 ID が存在しない。
    NotFound(u64),
    /// [`Storage::scan`] の対象行数・バイト量が上限（[`MAX_SCAN_TOTAL_ROWS`]・
    /// [`MAX_SCAN_TOTAL_BYTES`]）を超過、または [`Storage::scan_batch_log`] のエントリ数が
    /// 上限（[`MAX_BATCH_LOG_ROWS`]）を超過したため fail-closed に拒否した（対象ビヘイビア:
    /// PERSIST-4・TABLE-10）。
    ///
    /// この 2 経路は同一 variant を共有する（専用 variant を新設する案（PR #193 codex
    /// レビュー PRRT_kwDOUAKASM6cCITT）・`#[non_exhaustive]` を付与する案（同
    /// PRRT_kwDOUAKASM6cB6is）はいずれも公開 enum・既存の網羅的 match への破壊的変更に
    /// あたるとして差し戻された）。
    ///
    /// **[`fmt::Display`] の互換性契約**: 本 variant の `Display` 文字列
    /// `"scan limit exceeded: use scan_page"` は既存利用者がログ・診断で参照しうる
    /// 観測可能な契約であり、variant 追加・`non_exhaustive` 化と同様、末尾への追記も
    /// 含めて告知なく変更しない（PR #193 codex レビュー再指摘対応: 補足文言の追記で
    /// あっても既存の完全一致・厳密な前方一致の参照を壊し得るため、旧文字列と完全に
    /// 同一のまま維持する）。
    ///
    /// `scan_page` は [`Storage::scan`] 経路専用の代替 API であり、
    /// [`Storage::scan_batch_log`]（台帳。ページング API を持たない）には当てはまらない。
    /// `Display` 文字列自体を経路ごとに変えず固定するため、経路別の正確な代替手段案内
    /// （`scan` → [`Storage::scan_page`]・テーブルスコープ → `catalog.rs` の
    /// `scan_table_page`・台帳 → 上限緩和や運用対応）は、どの関数を呼んだかを把握している
    /// 呼び出し元（= 内部コンテキスト）が本 variant を検出した際に `Display` をそのまま
    /// 使わず別途組み立てる（`catalog.rs::convert_storage_error`・
    /// `examples/crash_tool_cross_table.rs` の `scan_batch_log` 呼び出し箇所を参照）。
    ScanLimitExceeded,
    /// [`crate::txn::BatchWriteTxn::log_batch`] に既存の `batch_seq` を渡した。`redb` の
    /// `insert` は無条件上書きのため、検出せず通すとバッチ台帳の不変条件
    /// （`batch_seq` ごとに 1 エントリ）が壊れる。呼び出し元の採番バグを fail-closed
    /// に拒否する（security.md「不安全な設計」対応。他テナント情報は含まない）。
    DuplicateBatchSeq(u64),
    /// [`crate::txn::BatchWriteTxn`] の内部カウンタ（直近の `log_batch` 以降に `put` した
    /// 行数）が `u64` を溢れた。実運用では到達し得ない防御的分岐だが、undefined な
    /// ラップアラウンドで台帳の行数契約を壊さないよう checked 演算で明示的に検出する
    /// （coding-rust.md「整数演算は checked_* / saturating_* を使う」対応）。
    PendingRowCountOverflow,
    /// [`crate::txn::BatchWriteTxn`] で新規挿入した行を、直近の `log_batch` 以降まだ台帳へ
    /// 記録しないまま [`crate::txn::BatchWriteTxn::commit`] しようとした。台帳に記録され
    /// ない行が commit で確定すると「台帳の row_count 合計 == 行総数」という TABLE-10 の
    /// 不変条件を [`crate::txn::BatchWriteTxn`] の公開 API だけで壊せるため fail-closed に
    /// 拒否する（security.md「不安全な設計」対応。他テナント情報は含まない）。
    UnloggedRows(u64),
    /// [`crate::txn::BatchWriteTxn::log_batch`] を、直近の呼び出し以降 1 件も
    /// [`crate::txn::BatchWriteTxn::put`]（新規挿入）していない状態で呼んだ。クラッシュ
    /// 検証ツールの検証オラクル（台帳の各エントリ値 == 実際にコミットされたバッチ
    /// サイズ）と食い違うゼロ件エントリを台帳へ残さないよう fail-closed に拒否する。
    EmptyBatch,
    /// [`GENERATION_TABLE`] のカウンタが `u64` を溢れた（到達し得ない防御的分岐。
    /// coding-rust.md「整数演算は checked_*/saturating_* を使う」対応）。
    GenerationCounterOverflow,
    /// [`ROWS_TABLE`] を旧フォーマット（物理キーが `id` 単独）の DB に対して開こうと
    /// した（対象ビヘイビア: TABLE-12）。旧行を新キー形式で読み解くと別テナントの行
    /// として扱う fail-open になりうるため拒否する（マイグレーションは提供しない。
    /// [`map_rows_table_error`] のドキュメンテーションコメント参照）。`Display` には
    /// テナント ID・テーブル名を含めない（security.md「情報漏えい（エラー経由の
    /// 存在情報）」対応。`CatalogError::IncompatibleRowKeyFormat` と同一文言）。
    IncompatibleRowKeyFormat,
    /// 明示トランザクション（SQL-31・TASK-221）が単一ライタ（`TABLE-3`）を
    /// 保持している間に、別セッション（またはロック待ちの上限を超えた同一セッション）が
    /// 書き込みトランザクションを取得しようとした（[`storage::writer_gate`] の
    /// 待機上限超過）。fail-closed に拒否する（`55P03`。他テナントの情報は含まない）。
    WriteLockTimeout,
    /// 明示トランザクションを保持している当該スレッド自身が、再度書き込み
    /// トランザクションを取得しようとした（配線漏れに対する多層防御。許可リスト側で
    /// 本来は手前で拒否される想定）。到達した場合は内部矛盾として扱う。
    WriteTxnHeldByCurrentSession,
}

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Backend(e) => write!(f, "storage backend error: {e}"),
            StorageError::Codec(msg) => write!(f, "row codec error: {msg}"),
            StorageError::NotFound(id) => write!(f, "row not found: id={id}"),
            // 互換性契約として文字列全体を旧文字列と完全に同一のまま固定する（本 variant の
            // ドキュメンテーションコメント「Display の互換性契約」参照。PR #193 codex
            // レビュー再指摘対応: 末尾への補足追記であっても既存利用者の完全一致・前方一致
            // 参照を壊し得るため行わない）。`scan_page` は `Storage::scan` 経路専用の代替 API
            // であり `scan_batch_log`（台帳）経路には当てはまらないが、`Display` 文字列は
            // 経路によらず固定するため、経路別の正確な代替手段案内は呼び出し元
            // （`catalog.rs::convert_storage_error`・`examples/crash_tool_cross_table.rs`）が
            // `Display` を使わず別途組み立てる。
            StorageError::ScanLimitExceeded => {
                write!(f, "scan limit exceeded: use scan_page")
            }
            StorageError::DuplicateBatchSeq(seq) => {
                write!(f, "duplicate batch seq: seq={seq}")
            }
            StorageError::PendingRowCountOverflow => {
                write!(f, "pending row count overflow: split into smaller batches")
            }
            StorageError::UnloggedRows(count) => {
                write!(f, "unlogged rows before commit: count={count}")
            }
            StorageError::EmptyBatch => write!(f, "empty batch: no rows put since last log_batch"),
            StorageError::GenerationCounterOverflow => {
                write!(f, "storage generation counter overflow")
            }
            // `CatalogError::IncompatibleRowKeyFormat` と完全に同一の文言に揃える
            // （`catalog.rs::convert_storage_error` が本 variant をそのまま写像するため、
            // 呼び出し元から見て文言が食い違わないようにする）。
            StorageError::IncompatibleRowKeyFormat => {
                write!(f, "incompatible row store key format: rebuild required")
            }
            StorageError::WriteLockTimeout => {
                write!(f, "write lock not available: timed out waiting for writer")
            }
            StorageError::WriteTxnHeldByCurrentSession => {
                write!(f, "write transaction already held by current session")
            }
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StorageError::Backend(e) => Some(e),
            StorageError::Codec(_)
            | StorageError::NotFound(_)
            | StorageError::ScanLimitExceeded
            | StorageError::DuplicateBatchSeq(_)
            | StorageError::PendingRowCountOverflow
            | StorageError::UnloggedRows(_)
            | StorageError::EmptyBatch
            | StorageError::GenerationCounterOverflow
            | StorageError::IncompatibleRowKeyFormat
            | StorageError::WriteLockTimeout
            | StorageError::WriteTxnHeldByCurrentSession => None,
        }
    }
}

// `redb` の各操作（begin_write・open_table・commit 等）はそれぞれ異なるエラー型を返すが、
// すべて `redb::Error` へ変換可能なので、ここで一括して `StorageError` へ橋渡しする。
//
// 設計メモ: この blanket impl（`E: Into<redb::Error>` を満たす任意の型から変換可能）は、
// coherence 制約により `StorageError` へ他の `From` 実装を個別に追加することを妨げる
// （`redb::Error` に変換可能な型は将来的にも本 impl に一元化される）。`StorageError` は
// engine ⇔ wire-server のエラー契約（`wire_code`）の土台となる型のため、`redb` 以外の
// エラー源を追加する際はこの制約を踏まえて設計を見直すこと。
impl<E> From<E> for StorageError
where
    E: Into<redb::Error>,
{
    fn from(e: E) -> Self {
        StorageError::Backend(e.into())
    }
}

pub type Result<T> = std::result::Result<T, StorageError>;

/// `redb` の書き込みトランザクションと [`writer_gate::WriterPermit`] を同じ寿命で
/// 束ねたハンドル（SQL-31・TASK-221。PR #1041 レビュー指摘）。
///
/// [`Storage::begin_write_txn`]（autocommit）・[`Storage::begin_explicit_write_txn`]
/// （明示トランザクション）の双方が返す。`redb` のライタが生きている間は必ず
/// ゲートも占有しているため、後続の書き込み要求は `redb::Database::begin_write`
/// の中で無期限に待つことはなく、ゲートの待機上限（`write_lock_wait`）で
/// `55P03` へ倒れる。`Deref`/`DerefMut` で `redb::WriteTransaction` として扱える。
///
/// フィールドの宣言順が drop 順序を決める（`txn` を先に drop＝abort してから
/// `_permit` を解放する）。並べ替えてはならない。
pub(crate) struct GatedWriteTxn {
    txn: redb::WriteTransaction,
    _permit: writer_gate::WriterPermit,
}

impl std::ops::Deref for GatedWriteTxn {
    type Target = redb::WriteTransaction;
    fn deref(&self) -> &redb::WriteTransaction {
        &self.txn
    }
}

impl std::ops::DerefMut for GatedWriteTxn {
    fn deref_mut(&mut self) -> &mut redb::WriteTransaction {
        &mut self.txn
    }
}

impl GatedWriteTxn {
    /// 変更を破棄して閉じる。`txn` を abort した後にゲートを解放する。
    pub(crate) fn abort(self) -> Result<()> {
        let GatedWriteTxn { txn, _permit } = self;
        txn.abort()?;
        Ok(())
    }

    /// テスト fixture 用: 世代を進めずに `redb` の commit だけを行う（旧来の
    /// `write_txn.commit()` 直呼び出しと同じ効果）。permit は commit 後に解放する。
    #[cfg(test)]
    pub(crate) fn commit_raw_for_test(self) -> Result<()> {
        let GatedWriteTxn { txn, _permit } = self;
        txn.commit()?;
        Ok(())
    }
}

/// commit 系の入口（[`commit_write_txn`]・[`bump_generation_and_commit`]・
/// [`crate::recovery::commit_boundary::commit`] 等）が受け取る書き込み
/// トランザクション。[`GatedWriteTxn`] はゲートの permit を commit 完了まで
/// 保持し、その後で解放する。素の `redb::WriteTransaction`（テスト fixture が
/// `db().begin_write()` で直接作るもの）は permit を持たない。
pub(crate) trait CommitTxn {
    /// commit 対象の `redb::WriteTransaction` と、commit 完了まで保持すべき
    /// permit（あれば）に分ける。呼び出し元は permit を `_permit` のような名前付き
    /// 束縛で commit 完了まで保持すること（`_` で受けると即座に解放される）。
    fn split_for_commit(self) -> (redb::WriteTransaction, Option<writer_gate::WriterPermit>);
}

impl CommitTxn for redb::WriteTransaction {
    fn split_for_commit(self) -> (redb::WriteTransaction, Option<writer_gate::WriterPermit>) {
        (self, None)
    }
}

impl CommitTxn for GatedWriteTxn {
    fn split_for_commit(self) -> (redb::WriteTransaction, Option<writer_gate::WriterPermit>) {
        let GatedWriteTxn { txn, _permit } = self;
        (txn, Some(_permit))
    }
}

/// [`GENERATION_TABLE`] を +1 する（TASK-133 P1 対応）。`write_txn.commit()` を
/// 呼ぶ前の準備段階のみを担い、この関数自体は commit を行わない。
///
/// commit 成功境界（TASK-96・RECOVER-5）: 本関数の `Err` は `write_txn.commit()` へ
/// 到達する前に確定するため、durable write は発生していない
/// （[`crate::recovery::commit_boundary::commit_and_finish`] はこの区別を使って、
/// 本関数の `Err` を通常のエラー応答として扱ってよいと判断する。一方
/// [`commit_prepared_write_txn`] の `Err` は durable かどうか呼び出し元から
/// 判別できないため同列に扱わない。詳細は `commit_boundary` モジュールの
/// ドキュメントを参照）。
///
/// テーブル単位ではなくストレージ全体で 1 カウンタにした: `Storage::put`/`put_batch`
/// は対象テーブル名を持たない旧 API のため、テーブル単位にすると経路によって
/// カウンタ更新対象を誤る余地が生まれる（誤りは fail-open に直結する）。単一カウンタは
/// 無関係なテーブルへの書き込みでも既存インデックスを失効させる（過剰拒否）が、
/// fail-closed 方向のため許容する。
pub(crate) fn prepare_generation_bump(write_txn: &redb::WriteTransaction) -> Result<()> {
    let mut gen_table = write_txn.open_table(GENERATION_TABLE)?;
    let current = gen_table
        .get(GENERATION_KEY)?
        .map(|v| v.value())
        .unwrap_or(0);
    let next = current
        .checked_add(1)
        .ok_or(StorageError::GenerationCounterOverflow)?;
    gen_table.insert(GENERATION_KEY, next)?;
    Ok(())
}

/// [`prepare_generation_bump`] 済みの `write_txn` を実際に commit する。この呼び出し
/// 自体（`write_txn.commit()`）が本クレートにおける唯一の「point of no return」
/// （TASK-96・RECOVER-5）であり、`Err` を返した場合でも durable write が実際には
/// 発生していたかどうかを呼び出し元から判別できない
/// （[`crate::recovery::commit_boundary::commit_and_finish`] はこの `Err` を
/// 「結果不明」として扱い、通常のエラー応答を返さず fail-closed に処理する）。
pub(crate) fn commit_prepared_write_txn(write_txn: redb::WriteTransaction) -> Result<()> {
    write_txn.commit()?;
    Ok(())
}

/// [`Storage::current_generation`] が共有するトランザクションスコープの実装本体
/// （TASK-109・PLAN-5 レビュー対応）。`crate::core::EngineCore::dictionary_snapshot`
/// が `catalog::get_table_schema_in_txn` と同一の `read_txn` 上で本関数を呼ぶことで、
/// スキーマ取得と世代取得を単一スナップショットへ閉じ込める（TOCTOU 対策。
/// `catalog::get_table_schema_in_txn` のドキュメンテーションコメント・
/// `EngineCore::execute_sql` の Issue #56 レビュー対応と同じ理由）。別々の `read_txn`
/// で読むと、両呼び出しの間に `alter_table_add_column` がコミットされた場合、
/// 「新スキーマの世代」と「旧スキーマ」の組み合わせという不整合なペアを観測しうる。
pub(crate) fn current_generation_in_txn(read_txn: &redb::ReadTransaction) -> Result<u64> {
    match read_txn.open_table(GENERATION_TABLE) {
        Ok(t) => Ok(t.get(GENERATION_KEY)?.map(|v| v.value()).unwrap_or(0)),
        Err(redb::TableError::TableDoesNotExist(_)) => Ok(0),
        Err(e) => Err(e.into()),
    }
}

/// [`prepare_generation_bump`] と [`commit_prepared_write_txn`] を連続実行する
/// 薄いラッパ（TASK-133 P1 対応）。本クレート内の実書き込みを伴う書き込みコミットは
/// `write_txn.commit()` を直接呼ばずすべて本関数、または
/// [`crate::recovery::commit_boundary::commit_and_finish`]（この 2 関数を個別に
/// 呼び分けて point of no return の前後を区別する経路）のいずれかを経由する。
///
/// `Storage::put`/`put_batch`・`crate::catalog` の DDL/DML・[`commit_write_txn`]
/// 経由の `crate::txn::WriteTxn`/`BatchWriteTxn` はいずれも本関数を直接呼ばず
/// [`crate::recovery::commit_boundary::commit`] 経由で世代カウントへ到達する
/// （choke point の一本化。詳細は `commit_boundary` モジュールのドキュメントを参照）。
/// 本関数自体は本クレート内のテスト fixture 等、commit 成功境界の保護区間を
/// 必要としない直接呼び出し元向けに残している。
pub(crate) fn bump_generation_and_commit(write_txn: impl CommitTxn) -> Result<()> {
    // permit は commit 完了まで保持する（`GatedWriteTxn` 参照）。
    let (write_txn, _permit) = write_txn.split_for_commit();
    prepare_generation_bump(&write_txn)?;
    commit_prepared_write_txn(write_txn)
}

/// `crate::txn::WriteTxn`/`BatchWriteTxn` の commit 経路を一本化する集約点
/// （Issue #175・TASK-133 P2 対応）。`has_writes` は呼び出し元（`txn.rs`）が
/// 自身のハンドルで redb のテーブルに触れる操作（`put`・`log_batch` 等）を
/// 1 回でも行ったかを追跡した結果を渡す契約とする。
///
/// - `has_writes == true`: [`bump_generation_and_commit`] に委譲し、従来どおり
///   世代を進めてからコミットする。ただし本関数の唯一の呼び出し元
///   [`crate::recovery::commit_boundary::commit_write_txn_guarded`] は
///   `has_writes == false` の場合にのみ本関数へ委譲する契約のため、この分岐は
///   現状の呼び出し経路網羅では到達しない（`has_writes == true` は
///   [`commit_write_txn_guarded`] 側で commit 成功境界（RECOVER-5）を張った
///   [`crate::recovery::commit_boundary::commit`] へ回される）。将来
///   `has_writes == true` を直接この関数へ渡す呼び出し元を追加する場合は
///   [`commit_write_txn_guarded`] 経由に倣うこと。
/// - `has_writes == false`: 変更が何もないため durable write を発生させず、
///   `write_txn.abort()` で閉じて世代を進めない（`crate::catalog` の空バッチ
///   経路が commit せず drop（= abort）で閉じているのと同方針）。
///
/// fail-closed の判断: `has_writes` の真偽は本関数ではなく呼び出し元の追跡に
/// 依存する。呼び出し元がテーブルに触れたかどうかの判定に迷う場合は `true`
/// （世代を進める）側に倒すことが `txn.rs` 側の契約であり、本関数はそれを
/// 前提に「過剰失効はあっても見逃し（fail-open）はない」設計とする
/// （見逃しは `crate::rls::PrefilterIndex` の RLS 相当の失効検出を素通りさせ、
/// 他テナント行の混入・削除済み可視性の残存に直結するため P0）。
pub(crate) fn commit_write_txn(write_txn: impl CommitTxn, has_writes: bool) -> Result<()> {
    if has_writes {
        bump_generation_and_commit(write_txn)
    } else {
        let (write_txn, _permit) = write_txn.split_for_commit();
        write_txn.abort()?;
        Ok(())
    }
}

/// RLS 相当のテナント境界判定に使う可視性ラベル（対象ビヘイビア: PERSIST-3）。
///
/// 本モジュールはこの値をスキーマとして同居保持するのみで、ポリシー評価（どの値なら
/// 呼び出し元へ返してよいか）は行わない（評価は TASK-133 以降の呼び出し元の責務）。
/// 永続化表現は 1 バイトの固定コードで、デコード時に未知のバイト値を検出した場合は
/// 既知の値へ黙殺フォールバックせず `StorageError::Codec` で拒否する（fail-closed。
/// 「未知値 → Public 扱い」は情報漏えいに直結するため行わない）。
///
/// 詳細は `docs/spec/04-behavior/data-model.md`（TASK-141・PERSIST-3、ポインタ表記）
/// を参照。値の追加・変更は `ROW_FORMAT_VERSION` 更新を伴う破壊的変更として扱う。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// テナント内で広く共有される可視性ラベル。
    Public,
    /// テナント内でも限定共有される可視性ラベル。
    Private,
}

impl Visibility {
    const PUBLIC_BYTE: u8 = 0x01;
    const PRIVATE_BYTE: u8 = 0x02;

    // `pub(crate)`: `row_codec.rs`（TASK-86）が独立フォーマットの行コーデックで
    // 同じ `Visibility` 型を再利用し、未知バイトの `Err` 拒否をそのまま活かすための
    // 最小限の公開範囲拡張（既存の v2 行フォーマット・`Storage` 公開 API 自体は変更しない）。
    pub(crate) fn to_byte(self) -> u8 {
        match self {
            Visibility::Public => Self::PUBLIC_BYTE,
            Visibility::Private => Self::PRIVATE_BYTE,
        }
    }

    pub(crate) fn from_byte(byte: u8) -> Result<Self> {
        match byte {
            Self::PUBLIC_BYTE => Ok(Visibility::Public),
            Self::PRIVATE_BYTE => Ok(Visibility::Private),
            other => Err(StorageError::Codec(format!(
                "unknown visibility byte: {other}"
            ))),
        }
    }
}

/// 永続化予定の行データ（呼び出し側が構築する入力形）。フィールドはすべて
/// `Copy`（参照・`Visibility`）のため `Clone`/`Copy` を導出できる（Issue #397 の
/// `content_hash` 等価性テストが複数箇所で同一値を再利用するために使う）。
#[derive(Clone, Copy)]
pub struct RowInput<'a> {
    /// RLS 相当のテナント境界判定に使う不透明な識別子（対象ビヘイビア: PERSIST-3）。
    /// 空文字列はテナント境界判定を曖昧にするため、エンコード時に拒否する（fail-closed）。
    pub tenant_id: &'a str,
    /// RLS 相当の可視性ラベル（対象ビヘイビア: PERSIST-3）。ポリシー評価はこの型の
    /// 呼び出し元（TASK-133 以降）の責務であり、本モジュールは値をそのまま保持する。
    pub visibility: Visibility,
    pub embedding: &'a [f32],
    /// 呼び出し元が定義する不透明なメタデータバイト列（本モジュールは中身を解釈しない）。
    pub metadata: &'a [u8],
}

/// 読み出した行データ（呼び出し側へ返す出力形）。
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub id: u64,
    /// RLS 相当のテナント境界判定に使う不透明な識別子（対象ビヘイビア: PERSIST-3）。
    pub tenant_id: String,
    /// RLS 相当の可視性ラベル（対象ビヘイビア: PERSIST-3）。[`RowInput::visibility`] 参照。
    pub visibility: Visibility,
    pub embedding: Vec<f32>,
    pub metadata: Vec<u8>,
}

/// 書き込みトランザクションの durability（Issue #849。構築時オプション）。
///
/// `redb::Durability` への写像は [`From<WriteDurability> for redb::Durability`] が
/// 単一の情報源。既定は [`WriteDurability::Immediate`]（`redb` 自身の既定と同一）で、
/// [`Storage::open`] はこの既定のまま挙動を変えない。[`WriteDurability::None`] を
/// 明示的に選んだ場合、commit 成功境界（RECOVER-5・`recovery::commit_boundary`）自体の
/// 判定契約は変わらないが、`abort()`（RECOVER-8・`recovery::fail_fast`）やプロセス
/// 強制終了・電源断が発生すると、直前の `Immediate` commit 以降の commit はディスクへ
/// 反映されずに失われ得る（損失ウィンドウ。詳細は
/// `docs/design/ingest-write-path.md`「Issue #849 追記」節参照）。
///
/// `redb::Durability` は `#[non_exhaustive]` だが、本 enum は engine の公開契約として
/// 独立に管理する（値の追加は破壊的変更として扱う。`IsolationLevel`〔`txn.rs`〕と
/// 同じ注記方針）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WriteDurability {
    /// commit 完了までに fsync 相当の同期を伴う（`redb` の既定・本 enum の既定）。
    #[default]
    Immediate,
    /// commit は `redb` プロセス内のバッファ上でのみ完結し、OS への `write`／
    /// `sync_data` のいずれも即座には発行しない（変更は次の `Immediate` commit・
    /// チェックポイント・キャッシュ退避まで OS に一切渡らないことがある。
    /// `storage::tests::power_loss::power_loss_scenario5_...` で実測確認済み）。
    /// 損失ウィンドウは本型のドキュメンテーションコメント参照。
    None,
}

impl From<WriteDurability> for redb::Durability {
    fn from(value: WriteDurability) -> Self {
        match value {
            WriteDurability::Immediate => redb::Durability::Immediate,
            WriteDurability::None => redb::Durability::None,
        }
    }
}

/// `redb::Database` を保持する永続化層のハンドル。
///
/// wire-server の接続ハンドラや検索カーネルからは直接ではなく、このハンドルを介して
/// 行データへアクセスする想定（呼び出し元は `Storage` を通じてのみ永続化状態を触る）。
pub struct Storage {
    db: redb::Database,
    /// 書き込みトランザクション生成の choke point（[`Storage::begin_write_txn`]）が
    /// 適用する durability 設定（Issue #849）。
    durability: WriteDurability,
    /// [`Storage::begin_write_txn`] の呼び出し回数（テスト専用の非 vacuous 証跡）。
    /// production ビルドには含めない。取りこぼし検査（全書き込みトランザクション
    /// 生成箇所が choke point を通ることの検証）に使う。
    #[cfg(test)]
    write_txn_creations: std::sync::atomic::AtomicU64,
    /// 明示トランザクション（SQL-31・TASK-221）と autocommit の間で単一ライタを
    /// 安全に共有する choke point（[`writer_gate`] 参照）。
    writer_gate: std::sync::Arc<writer_gate::WriterGate>,
    /// 分割実行 DML の実行中ジョブ登録簿（Issue #1127）。1 DB ファイルにつき 1 つの
    /// プロセス内 choke point という点で [`Self::writer_gate`] と同形。
    #[allow(dead_code)]
    // #1128 の実行器が結線するまで [`Self::partitioned_job_registry`] 経由のみ
    partitioned_jobs: std::sync::Arc<crate::recovery::partitioned_job::JobRegistry>,
    /// 書き込みトランザクション取得（autocommit・明示トランザクション双方）が
    /// [`Self::writer_gate`] を待つ上限（既定 30 秒）。超過時は
    /// [`StorageError::WriteLockTimeout`]（`55P03`）を返す。
    write_lock_wait: std::time::Duration,
}

impl Storage {
    /// 指定パスの `redb` データベースを開く。ファイルが存在しなければ新規作成する
    /// （`redb::Database::create` の契約）。durability は既定
    /// （[`WriteDurability::Immediate`]）のまま固定（挙動は変更しない）。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_durability(path, WriteDurability::default())
    }

    /// 指定パスの `redb` データベースを、書き込みトランザクションの durability を
    /// 明示指定して開く（Issue #849。構築時オプション）。[`WriteDurability::default`]
    /// を渡した場合は [`Storage::open`] と完全に同一の挙動になる。
    pub fn open_with_durability(
        path: impl AsRef<Path>,
        durability: WriteDurability,
    ) -> Result<Self> {
        let db = redb::Database::create(path)?;
        Ok(Self {
            db,
            durability,
            #[cfg(test)]
            write_txn_creations: std::sync::atomic::AtomicU64::new(0),
            writer_gate: writer_gate::WriterGate::new(),
            partitioned_jobs: Default::default(),
            write_lock_wait: DEFAULT_WRITE_LOCK_WAIT,
        })
    }

    /// 実行中の分割実行ジョブ登録簿（Issue #1127）。tenant 層・#1128 の実行器が
    /// `WriteTarget::Autocommit(&Storage)` 経由で届くよう `Storage` に置く。
    #[allow(dead_code)] // #1128 の実行器・#1129 の照会／取り消しが結線するまで未使用
    pub(crate) fn partitioned_job_registry(
        &self,
    ) -> &std::sync::Arc<crate::recovery::partitioned_job::JobRegistry> {
        &self.partitioned_jobs
    }

    /// 書き込みロック待ちの上限（[`Self::writer_gate`]）を明示指定する（トランザクション
    /// 上限を短くしたテスト・opt-in CLI 用）。既定値（[`DEFAULT_WRITE_LOCK_WAIT`]）を
    /// 使う場合は呼ぶ必要がない。
    pub fn with_write_lock_wait(mut self, wait: std::time::Duration) -> Self {
        self.write_lock_wait = wait;
        self
    }

    /// 書き込みトランザクション生成の choke point（Issue #849）。
    ///
    /// 本メソッドのみが `redb::Database::begin_write` を呼ぶ（単文/バッチ INSERT・
    /// UPDATE・DELETE・DDL・ファイル形 INSERT・宣言済み分離レベル API〔`txn.rs`〕を
    /// 含む全書き込み経路が本メソッド経由になるよう `storage.rs`・`txn.rs`・
    /// `tenant.rs`・`catalog.rs` の生成箇所を統一する。取りこぼし検査は
    /// `crates/engine/src/storage.rs` の `durability` 系テスト参照）。
    ///
    /// 既定値（[`WriteDurability::Immediate`]）は `redb` 自身の既定と同一のため
    /// `set_durability` を呼ばない（挙動・トランザクション開始シーケンスをビット同一に
    /// 保つ。受け入れ条件1「既存シグネチャ・挙動がビット同一」対応）。
    /// `set_durability` は「このトランザクションで persistent savepoint を
    /// 作成/削除していない限り成功する」契約（`redb` `=4.2.0`）であり、`begin_write`
    /// 直後に呼ぶ本実装は構造的に失敗しない。
    ///
    /// autocommit 経路（本メソッド）も明示トランザクションと同じく
    /// [`Self::writer_gate`] を待機上限つきで取得し、返す [`GatedWriteTxn`] が
    /// commit/abort されるまで保持する（`redb` のライタが生きている間はゲートも
    /// 占有する。[`writer_gate`] モジュールドキュメント参照）。
    pub(crate) fn begin_write_txn(&self) -> Result<GatedWriteTxn> {
        let permit = self.acquire_writer_permit()?;
        let txn = self.begin_write_txn_raw()?;
        Ok(GatedWriteTxn {
            txn,
            _permit: permit,
        })
    }

    /// 分割実行（述語形 UPDATE／DELETE のチャンク。ADR `docs/design/partitioned-dml.md`
    /// §8.2）用の入口。通常の書き込みが待機している間はゲートを取得せず譲る。
    /// `tenant::partitioned_dml` の実行器がチャンクごとに呼ぶ。待機上限超過は
    /// `WriteLockTimeout`（`55P03`）。
    #[allow(dead_code)] // 実行器（tenant::partitioned_dml）が結線されるまで未使用
    pub(crate) fn begin_partitioned_chunk_txn(&self) -> Result<GatedWriteTxn> {
        let permit = self
            .writer_gate
            .acquire_yielding(self.write_lock_wait)
            .map_err(|e| match e {
                writer_gate::GateError::Timeout => StorageError::WriteLockTimeout,
                writer_gate::GateError::HeldByCurrentThread => {
                    StorageError::WriteTxnHeldByCurrentSession
                }
            })?;
        let txn = self.begin_write_txn_raw()?;
        Ok(GatedWriteTxn {
            txn,
            _permit: permit,
        })
    }

    /// ゲートを待機上限（[`Self::write_lock_wait`]）つきで取得する。超過は
    /// `WriteLockTimeout`、同一スレッドからの再入は `WriteTxnHeldByCurrentSession`
    /// （いずれも `55P03`）。
    fn acquire_writer_permit(&self) -> Result<writer_gate::WriterPermit> {
        self.writer_gate
            .acquire(self.write_lock_wait)
            .map_err(|e| match e {
                writer_gate::GateError::Timeout => StorageError::WriteLockTimeout,
                writer_gate::GateError::HeldByCurrentThread => {
                    StorageError::WriteTxnHeldByCurrentSession
                }
            })
    }

    /// ゲートを介さず `redb::Database::begin_write` を直接呼ぶ内部ヘルパー。
    /// [`Self::begin_write_txn`]（autocommit）と
    /// [`Self::begin_explicit_write_txn`]（明示トランザクション）の双方が、
    /// それぞれ異なるゲート保持方針で本ヘルパーを共有する。
    fn begin_write_txn_raw(&self) -> Result<redb::WriteTransaction> {
        let mut txn = self.db.begin_write()?;
        if !matches!(self.durability, WriteDurability::Immediate) {
            txn.set_durability(self.durability.into())?;
        }
        #[cfg(test)]
        self.write_txn_creations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(txn)
    }

    /// 明示トランザクション（SQL-31・TASK-221）用の入口。ゲートの保持方針は
    /// autocommit（[`Self::begin_write_txn`]）と同一で、返す [`GatedWriteTxn`] を
    /// 呼び出し元が `COMMIT`/`ROLLBACK` まで保持する。
    pub(crate) fn begin_explicit_write_txn(&self) -> Result<GatedWriteTxn> {
        self.begin_write_txn()
    }

    /// [`Storage::begin_write_txn`] の呼び出し回数（テスト専用）。全書き込み経路が
    /// choke point を経由していることの非 vacuous な証跡として使う。
    #[cfg(test)]
    pub(crate) fn write_txn_creations(&self) -> u64 {
        self.write_txn_creations
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 内部 `redb::Database` ハンドルへの `pub(crate)` アクセサ。
    ///
    /// `txn.rs`（TASK-88・TABLE-3）が宣言済み分離レベルのトランザクション API
    /// （[`Storage::begin_read`](crate::txn) 相当）を実装する際に、`Storage` の外へ
    /// `redb::Database` 型そのものをリークさせずに到達するための最小限の穴。
    /// 公開 API・挙動は変更しない。
    pub(crate) fn db(&self) -> &redb::Database {
        &self.db
    }

    /// 単一行を書き込み、コミットする（対象ビヘイビア: PERSIST-1・TABLE-12）。
    /// 物理キーは `(row.tenant_id, id)`（[`RowStoreTableDef`]）。`tenant_id` は
    /// [`encode_row`] の検証（空拒否・[`MAX_TENANT_ID_LEN`]）を通った値のみキーへ使う。
    ///
    /// バッチ台帳（[`BATCH_LOG_TABLE`]）を経由しない経路（恒久契約。
    /// `docs/design/batch-ledger-scope.md` 参照）。TABLE-10 の不変条件が必要な場合は
    /// [`Storage::begin_batch_write`] が返す [`crate::txn::BatchWriteTxn`] を使うこと。
    pub fn put(&self, id: u64, row: &RowInput<'_>) -> Result<()> {
        let encoded = encode_row(row)?;
        let write_txn = self.begin_write_txn()?;
        {
            let mut table = write_txn
                .open_table(ROWS_TABLE)
                .map_err(map_rows_table_error)?;
            table.insert((row.tenant_id, id), encoded.as_slice())?;
        }
        // commit 成功境界（TASK-96・RECOVER-5）の choke point 経由でコミットする
        // （`crate::recovery::commit_boundary` 参照。commit 成功後の応答一意性を
        // 構造的に保証する）。
        crate::recovery::commit_boundary::commit(write_txn)
    }

    /// 複数行を単一トランザクションで書き込む（対象ビヘイビア: PERSIST-2・TABLE-12）。
    /// 空スライスの場合はトランザクションを開かず即座に成功を返す。物理キーは
    /// [`Storage::put`] と同じ `(row.tenant_id, id)`。
    ///
    /// [`Storage::put`] と同様、バッチ台帳（[`BATCH_LOG_TABLE`]）を経由しない経路
    /// （恒久契約。`docs/design/batch-ledger-scope.md` 参照）。TABLE-10 の不変条件が
    /// 必要な場合は [`Storage::begin_batch_write`] が返す [`crate::txn::BatchWriteTxn`] を
    /// 使うこと。
    pub fn put_batch(&self, rows: &[(u64, RowInput<'_>)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let write_txn = self.begin_write_txn()?;
        {
            let mut table = write_txn
                .open_table(ROWS_TABLE)
                .map_err(map_rows_table_error)?;
            // clear 再利用の 1 面スクラッチ（Issue #398。`tenant.rs` の同パターンと
            // 揃える）。txn 内でのみ有効なため、行数分の `Vec<u8>` 確保を避ける。
            let mut scratch: Vec<u8> = Vec::new();
            for (id, row) in rows {
                scratch.clear();
                encode_row_into(&mut scratch, row)?;
                table.insert((row.tenant_id, *id), scratch.as_slice())?;
            }
        }
        // commit 成功境界（TASK-96・RECOVER-5）の choke point 経由でコミットする
        // （[`Storage::put`] と同じ理由）。
        crate::recovery::commit_boundary::commit(write_txn)
    }

    /// 現在の書き込み世代を返す（TASK-133 P1 対応。読み取り専用。1 回の `read_txn` を
    /// 開いて閉じるだけの単純な値取得で、スナップショットは保持しない）。
    /// `crate::rls::PrefilterIndex::build`/`search` が失効検出の前後比較に使う
    /// （安全性は呼び出し元が前後の値を比較することで担保する。本メソッド自体は
    /// 世代値以外の一貫性を何も保証しない）。
    pub(crate) fn current_generation(&self) -> Result<u64> {
        let read_txn = self.db.begin_read()?;
        current_generation_in_txn(&read_txn)
    }

    /// テナント ID・行 ID を指定して 1 行取得する（スナップショット読み取り。対象
    /// ビヘイビア: TABLE-12）。物理キーは `(tenant_id, id)` の複合キーのため、`id`
    /// 単独では行を一意に指せない（`catalog.rs` の `get_row_from_table` と同じ理由）。
    pub fn get(&self, tenant_id: &str, id: u64) -> Result<Row> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(ROWS_TABLE) {
            Ok(t) => t,
            // テーブル未作成（1 行も書き込んでいない）は「存在しない」として扱う。
            Err(redb::TableError::TableDoesNotExist(_)) => return Err(StorageError::NotFound(id)),
            Err(e) => return Err(map_rows_table_error(e)),
        };
        let guard = table
            .get((tenant_id, id))?
            .ok_or(StorageError::NotFound(id))?;
        decode_row_for_key(tenant_id, id, guard.value())
    }

    /// 全行をスナップショット読み取りで走査する（対象ビヘイビア: PERSIST-4・TABLE-12）。
    /// 列挙順は物理キー順（`tenant_id` 昇順 → `id` 昇順）で、テナントをまたぐと
    /// 単純な `id` 昇順にはならない（呼び出し元が `id` 順を仮定する場合は明示的に
    /// ソートすること）。
    ///
    /// 総行数が [`MAX_SCAN_TOTAL_ROWS`]、総バイト量（エンコード済みバイト列長で近似）が
    /// [`MAX_SCAN_TOTAL_BYTES`] のいずれかを超える場合は、部分的な結果を黙って切り詰めず
    /// [`StorageError::ScanLimitExceeded`] で fail-closed に拒否する
    /// （security.md「不安全な設計｜無制限リソース確保（DoS）」対応）。
    /// 大規模データベースを扱う呼び出し元（検索カーネル等）は、この上限を前提にせず
    /// 上限付きページング API [`Storage::scan_page`] を使うこと。
    pub fn scan(&self) -> Result<Vec<Row>> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(ROWS_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(map_rows_table_error(e)),
        };
        let mut out = Vec::new();
        let mut bytes_used: usize = 0;
        for entry in table.iter()? {
            let (k, v) = entry?;
            let (tenant_id, id) = k.value();
            let raw = v.value();
            check_scan_limits(out.len(), bytes_used, raw.len())?;
            bytes_used = bytes_used.saturating_add(raw.len());
            out.push(decode_row_for_key(tenant_id, id, raw)?);
        }
        Ok(out)
    }

    /// 物理キー昇順（`(tenant_id, id)`。TABLE-12）で最大 `limit` 件を走査する
    /// 上限付きページング API（[`scan`](Self::scan) の無制限確保を避けるための代替。
    /// 対象ビヘイビア: PERSIST-4）。
    ///
    /// `after` に前回呼び出しで返したカーソル（[`RowCursor`]）を渡すと、その直後の
    /// キーから再開する。初回は `None` を渡す。戻り値の第 2 要素は次回呼び出しに
    /// 渡すべきカーソル（続きがなければ `None`）。複合キーでは「次のキー」を算術で
    /// 導出できないため `Bound::Excluded` の range で表現する
    /// （`catalog.rs::scan_table_page` と同型のページング契約）。
    ///
    /// `limit` は [`MAX_SCAN_PAGE_LIMIT`] で切り詰める（呼び出し元が誤って大きな値を
    /// 渡しても一度の確保が上限を超えないようにする）。`limit == 0` は空ページを返す。
    /// さらに、行数が上限未満でも、ページ内のデコード対象バイト量（embedding + metadata
    /// 相当。エンコード済みバイト列長で近似）が [`MAX_SCAN_PAGE_BYTES`] を超える場合は
    /// その時点でページを打ち切る（最大サイズの行が並んだ場合の巨大確保を避けるため。
    /// 1 行のみで超過する場合はその 1 行を含めて返し、無限ループを避ける）。
    pub fn scan_page(
        &self,
        after: Option<(&str, u64)>,
        limit: u32,
    ) -> Result<(Vec<Row>, Option<RowCursor>)> {
        let limit = limit.min(MAX_SCAN_PAGE_LIMIT) as usize;
        if limit == 0 {
            return Ok((Vec::new(), None));
        }

        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(ROWS_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok((Vec::new(), None)),
            Err(e) => return Err(map_rows_table_error(e)),
        };

        // カーソルの直後から走査を開始する（`Bound::Excluded`）。複合キーでは
        // 「次のキー」を算術で導出できないため、除外境界付き range で表現する。
        let range_start = match after {
            Some(cursor) => std::ops::Bound::Excluded(cursor),
            None => std::ops::Bound::Unbounded,
        };

        let mut out = Vec::new();
        let mut bytes_used: usize = 0;
        // 行数上限・バイト上限のいずれかで打ち切った場合のみ「続きがあるかもしれない」。
        // イテレータが自然に尽きた（テーブル末尾に到達した）場合は打ち切りではない。
        let mut capped = false;
        let mut last_key: Option<RowCursor> = None;
        for entry in table.range::<(&str, u64)>((range_start, std::ops::Bound::Unbounded))? {
            if out.len() == limit {
                capped = true;
                break;
            }
            let (k, v) = entry?;
            let (tenant_id, id) = k.value();
            let raw = v.value();
            if !out.is_empty() && bytes_used.saturating_add(raw.len()) > MAX_SCAN_PAGE_BYTES {
                capped = true;
                break;
            }
            out.push(decode_row_for_key(tenant_id, id, raw)?);
            last_key = Some((tenant_id.to_string(), id));
            bytes_used = bytes_used.saturating_add(raw.len());
        }

        let cursor_for_next = if capped { last_key } else { None };
        Ok((out, cursor_for_next))
    }

    /// [`BATCH_LOG_TABLE`] の全エントリを `batch_seq` 昇順で読み出す（対象ビヘイビア:
    /// TABLE-10）。再起動後の検証専用の読み取り（採番再開には
    /// [`Storage::batch_log_max_seq`] を使う。全件走査を要する検証オラクル
    /// （`crash_tool_cross_table.rs` の `verify_inner` 等）向け）。
    ///
    /// エントリ数が [`MAX_BATCH_LOG_ROWS`] を超える場合は、[`Storage::scan`] と同様に
    /// 部分的な結果を黙って切り詰めず [`StorageError::ScanLimitExceeded`] で
    /// fail-closed に拒否する（security.md「不安全な設計｜無制限リソース確保（DoS）」
    /// 対応。バッチ台帳はコミットごとに増え続けるため、大きな DB では無制限確保が
    /// メモリ枯渇につながり得る）。
    ///
    /// **呼び出し元への注意（Issue #131）**: 台帳にはページング API が無いため、この
    /// エラーをそのまま利用者へ露出する呼び出し元は `ScanLimitExceeded` の `Display`
    /// （固定文言 `"scan limit exceeded: use scan_page"`。[`StorageError::ScanLimitExceeded`]
    /// のドキュメンテーションコメント「Display の互換性契約」参照）をそのまま使わないこと。
    /// `scan_page` は [`Storage::scan`] 専用の代替 API であり台帳には存在しない。また台帳
    /// エントリ数を削減する compact/rotate 相当の API・運用手順も本クレートには存在しない
    /// （通常の compaction は論理エントリ数を減らさず、rotation で台帳を捨てれば検証対象
    /// そのものを失うため、いずれも実行可能な代替手段ではない: PR #193 codex レビュー
    /// 再指摘対応）。呼び出し元は本 variant を検出した際、実行不能な手段を示唆せず
    /// 「この呼び出し元では上限を超えた台帳を扱えない（検証不能）」という事実のみを
    /// 明示すること（変換例: `examples/crash_tool_cross_table.rs` の `scan_batch_log`
    /// 呼び出し箇所）。
    pub fn scan_batch_log(&self) -> Result<Vec<(u64, u64)>> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(BATCH_LOG_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let mut out = Vec::new();
        for entry in table.iter()? {
            check_batch_log_limit(out.len())?;
            let (k, v) = entry?;
            out.push((k.value(), v.value()));
        }
        Ok(out)
    }

    /// [`BATCH_LOG_TABLE`] の最大 `batch_seq` を返す（対象ビヘイビア: TABLE-10）。
    /// 台帳テーブル未作成・空の場合は `Ok(None)`。
    ///
    /// [`Storage::scan_batch_log`] と異なり全エントリを `Vec` へ確保せず、redb の
    /// B-tree 実装が持つ最終キー取得（`ReadableTable::last`）だけを使う（O(log n)・
    /// アロケーションなし）ため、[`MAX_BATCH_LOG_ROWS`] に依存しない。採番再開経路
    /// （`crash_tool_cross_table.rs` の `find_resume_state`）が呼ぶ想定で、台帳件数が
    /// 上限を超えた DB でも再開できるようにするための専用 API（Issue #132）。
    pub fn batch_log_max_seq(&self) -> Result<Option<u64>> {
        let read_txn = self.db.begin_read()?;
        let table = match read_txn.open_table(BATCH_LOG_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // `AccessGuard` は read txn（延いては `table`）を借用したままなので、
        // `u64` へコピーしてから返す（`table`/`read_txn` のスコープを抜けた後に
        // 借用が残る E0597 を避ける）。
        let max_seq = table.last()?.map(|(k, _v)| k.value());
        Ok(max_seq)
    }
}

/// [`Storage::scan`] の行数・バイト量上限判定を切り出した純粋関数（テスト容易性のため）。
/// `rows`・`bytes_used` は走査済み件数・バイト量、`next_len` はこれから加える 1 行の
/// エンコード済みバイト長。超過時は [`StorageError::ScanLimitExceeded`] を返す。
/// 判定条件（`>=` / `>` / `saturating_add`）は既存の `scan` 実装から変更しない。
fn check_scan_limits(rows: usize, bytes_used: usize, next_len: usize) -> Result<()> {
    if rows >= MAX_SCAN_TOTAL_ROWS || bytes_used.saturating_add(next_len) > MAX_SCAN_TOTAL_BYTES {
        return Err(StorageError::ScanLimitExceeded);
    }
    Ok(())
}

/// [`Storage::scan_batch_log`] のエントリ数上限判定を切り出した純粋関数（テスト容易性の
/// ため）。`entries` は走査済みエントリ数。超過時は [`StorageError::ScanLimitExceeded`]
/// を返す（`scan` 用と同一 variant。理由は同 variant のドキュメンテーションコメント参照）。
/// 判定条件は既存の `scan_batch_log` 実装から変更しない。
fn check_batch_log_limit(entries: usize) -> Result<()> {
    if entries >= MAX_BATCH_LOG_ROWS {
        return Err(StorageError::ScanLimitExceeded);
    }
    Ok(())
}

/// 行を固定レイアウトでエンコードする（serde 系依存を増やさない方針。dependency-policy.md）。
///
/// レイアウト（v2、対象ビヘイビア: PERSIST-3）:
/// `[version: u8=2][tenant_len: u16 le][tenant bytes][visibility: u8]`
/// `[dim: u32 le][embedding: dim * f32 le][metadata_len: u32 le][metadata bytes]`
///
/// RLS フィールド（`tenant_id`・`visibility`）を先頭側に置くのは、後続の RLS 事前フィルタ
/// （TASK-133）が embedding をデコードせずテナント判定できる余地を残すため。
/// バージョンバイトと非構造化のメタデータバイト列により、TASK-146（次元固定カタログ）等の
/// 後続スキーマ拡張が非互換変更なしに行えるようにしている。
///
/// `pub(crate)`: `txn.rs`（TASK-88）の書き込みトランザクションハンドルが、`Storage::put`
/// と同一のエンコーディングで行を書き込むために再利用する。
///
/// 実体は [`encode_row_into`] へ委譲する薄いラッパー（Issue #398）。単発の書き込み
/// （`txn.rs::WriteTxn::put`／`BatchWriteTxn::put` 等、行数分のバッファ再利用が
/// 割に合わない呼び出し元）はこちらを使い続けてよい。バッチ経路（`tenant.rs`・
/// `catalog.rs`）は `encode_row_into` を直接呼び、呼び出し元が保持するバッファへ
/// 追記することで行ごとの `Vec<u8>` 確保を避ける。
pub(crate) fn encode_row(row: &RowInput<'_>) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    encode_row_into(&mut buf, row)?;
    Ok(buf)
}

/// [`encode_row_into`] が書き込む前に呼び出し元が把握しておくためのバイト長計算
/// （検証込み）。バッチ経路（`tenant.rs::insert_rows_unchecked`）が連続 arena の
/// 総サイズを事前に `checked_add` で積算し、`try_reserve_exact` へ渡すために使う
/// （Issue #398）。検証条件・エラー種別は `encode_row_into` と同一に保つ
/// （どちらかだけが通る／拒否する行が発生しないようにするため）。
pub(crate) fn encoded_row_len(row: &RowInput<'_>) -> Result<usize> {
    let tenant_len = validate_tenant_id(row.tenant_id)?;
    let dim = validate_embedding_dim(row.embedding)?;
    let metadata_len = validate_metadata_len(row.metadata)?;
    let len = 1usize
        .checked_add(2)
        .and_then(|n| n.checked_add(tenant_len as usize))
        .and_then(|n| n.checked_add(1))
        .and_then(|n| n.checked_add(4))
        .and_then(|n| n.checked_add((dim as usize).checked_mul(4)?))
        .and_then(|n| n.checked_add(4))
        .and_then(|n| n.checked_add(metadata_len as usize))
        .ok_or_else(|| StorageError::Codec("encoded row length overflow".to_string()))?;
    Ok(len)
}

/// `tenant_id` の検証（空文字列拒否・[`MAX_TENANT_ID_LEN`] 上限）。
/// [`encode_row_into`]・[`encoded_row_len`] で検証条件を一致させるための共有関数。
fn validate_tenant_id(tenant_id: &str) -> Result<u16> {
    if tenant_id.is_empty() {
        return Err(StorageError::Codec(
            "tenant_id must not be empty".to_string(),
        ));
    }
    let tenant_bytes = tenant_id.as_bytes();
    let tenant_len = u16::try_from(tenant_bytes.len()).map_err(|_| {
        StorageError::Codec(format!("tenant_id too long: {} bytes", tenant_bytes.len()))
    })?;
    if tenant_len > MAX_TENANT_ID_LEN {
        return Err(StorageError::Codec(format!(
            "tenant_id length {tenant_len} exceeds limit {MAX_TENANT_ID_LEN}"
        )));
    }
    Ok(tenant_len)
}

/// embedding 次元数の検証（[`MAX_EMBEDDING_DIM`] 上限）。
/// [`encode_row_into`]・[`encoded_row_len`] で検証条件を一致させるための共有関数。
fn validate_embedding_dim(embedding: &[f32]) -> Result<u32> {
    let dim = u32::try_from(embedding.len()).map_err(|_| {
        StorageError::Codec(format!("embedding dim too large: {}", embedding.len()))
    })?;
    if dim > MAX_EMBEDDING_DIM {
        return Err(StorageError::Codec(format!(
            "embedding dim {dim} exceeds limit {MAX_EMBEDDING_DIM}"
        )));
    }
    Ok(dim)
}

/// メタデータ長の検証（[`MAX_METADATA_LEN`] 上限）。
/// [`encode_row_into`]・[`encoded_row_len`] で検証条件を一致させるための共有関数。
fn validate_metadata_len(metadata: &[u8]) -> Result<u32> {
    let metadata_len = u32::try_from(metadata.len())
        .map_err(|_| StorageError::Codec(format!("metadata too large: {}", metadata.len())))?;
    if metadata_len > MAX_METADATA_LEN {
        return Err(StorageError::Codec(format!(
            "metadata length {metadata_len} exceeds limit {MAX_METADATA_LEN}"
        )));
    }
    Ok(metadata_len)
}

/// `values` を LE バイト列として `buf` の末尾へ追記するバルク変換ヘルパ（Issue #398）。
/// `to_le_bytes` ベースのためホストエンディアンに依存しない（LE ホストでは
/// memcpy 相当へ最適化される）。要素ごとの `extend_from_slice` 呼び出しを
/// 避け、事前に確保した領域へ `chunks_exact_mut` で書き込む。
/// `unsafe`・`bytemuck` 等の再解釈は使わない（dependency-policy・coding-rust）。
fn extend_f32_le(buf: &mut Vec<u8>, values: &[f32]) -> Result<()> {
    let start = buf.len();
    let bytes = values
        .len()
        .checked_mul(4)
        .ok_or_else(|| StorageError::Codec("embedding byte length overflow".to_string()))?;
    let new_len = start
        .checked_add(bytes)
        .ok_or_else(|| StorageError::Codec("embedding byte length overflow".to_string()))?;
    buf.try_reserve(bytes)
        .map_err(|_| StorageError::Codec("failed to reserve embedding buffer".to_string()))?;
    buf.resize(new_len, 0);
    let dst = buf
        .get_mut(start..)
        .ok_or_else(|| StorageError::Codec("embedding buffer write out of bounds".to_string()))?;
    let (chunks, _remainder) = dst.as_chunks_mut::<4>();
    for (chunk, v) in chunks.iter_mut().zip(values) {
        *chunk = v.to_le_bytes();
    }
    Ok(())
}

/// 行を v2 レイアウトで `buf` の末尾へ追記する（対象ビヘイビア: PERSIST-3。
/// レイアウトは [`encode_row`] のドキュメンテーションコメント参照）。`buf` を
/// `clear` しない append 型のため、呼び出し元は連続 arena（`tenant.rs`。1
/// バッチ 1 バッファ）・clear 再利用スクラッチ（`catalog.rs`・`tenant.rs` の
/// 別経路）いずれの再利用パターンにも使える（Issue #398）。
///
/// 検証（`validate_tenant_id`・`validate_embedding_dim`・`validate_metadata_len`）
/// → サイズ計算（[`encoded_row_len`] と同一の `checked_*` 演算）→
/// `try_reserve`（無制限 `Vec::with_capacity` 禁止・coding-rust）→ 書き込み
/// の順で行うため、`Err` を返す場合 `buf` は呼び出し前と同一の長さ・内容の
/// まま（部分追記が残らない）。
///
/// `pub(crate)`: `tenant.rs`（`insert_rows_unchecked`・`replace_typed_rows_by_text_key`）・
/// `catalog.rs`（`insert_rows_into_table`）・`storage.rs::Storage::put_batch` が
/// バッチ書き込み経路で直接呼ぶ。
pub(crate) fn encode_row_into(buf: &mut Vec<u8>, row: &RowInput<'_>) -> Result<()> {
    let tenant_len = validate_tenant_id(row.tenant_id)?;
    let dim = validate_embedding_dim(row.embedding)?;
    let metadata_len = validate_metadata_len(row.metadata)?;

    let additional = encoded_row_len(row)?;
    buf.try_reserve(additional)
        .map_err(|_| StorageError::Codec("failed to reserve row buffer".to_string()))?;

    buf.push(ROW_FORMAT_VERSION);
    buf.extend_from_slice(&tenant_len.to_le_bytes());
    buf.extend_from_slice(row.tenant_id.as_bytes());
    buf.push(row.visibility.to_byte());
    buf.extend_from_slice(&dim.to_le_bytes());
    extend_f32_le(buf, row.embedding)?;
    buf.extend_from_slice(&metadata_len.to_le_bytes());
    buf.extend_from_slice(row.metadata);
    Ok(())
}

/// 行バイト列の先頭（バージョン・`tenant_id`・`visibility`）だけをデコードする
/// [`decode_row`] の共有前段。embedding・metadata のバイト列へは触れないため、
/// それらが破損していても本関数の成否には影響しない。
///
/// 呼び出し文脈: [`decode_row`]（フル デコード）・[`decode_row_tenant_and_visibility`]
/// （ヘッダのみ）に加え、`sql/aggregate.rs`・`sql/group_by.rs` の走査ループ
/// （TASK-166/167・SQL-13/14）が、ヘッダのみでの可視性判定と本体デコード
/// （[`decode_row_body_into`]）を分離二段階で行うために直接呼ぶ（Issue #349。
/// 従来は可視性判定用のヘッダデコードと `decode_row_for_key` 内部の
/// フルデコード用ヘッダデコードが二重に走っていた）。ロジックを 1 箇所に
/// 集約することで、検証条件（`tenant_len` 上限・UTF-8・空文字列拒否等）が
/// 呼び出し元間で食い違わないようにする。
/// 成功時は `(tenant_id, visibility, visibility バイトの直後のオフセット)` を返す。
/// このオフセットは [`decode_row_body_into`] へそのまま渡せる（本体デコードの
/// 再開位置）。
///
/// `tenant_id` は `buf` を借用した `&str`（所有化しない）。呼び出し元は `buf` の
/// 生存期間中のみこの値を参照できる。ヘッダ比較（可視性判定）の経路を行ごとの
/// ヒープアロケーションなしで処理するための設計（Issue #174。PR #151 の性能
/// フォローアップ）。行を所有化して保持する必要がある場合（[`decode_row`] が
/// `Row` を構築する場合）は、呼び出し元が明示的に `.to_string()` する。
pub(crate) fn decode_row_header(buf: &[u8]) -> Result<(&str, Visibility, usize)> {
    let version = *buf
        .first()
        .ok_or_else(|| StorageError::Codec("row buffer is empty".to_string()))?;
    if version != ROW_FORMAT_VERSION {
        return Err(StorageError::Codec(format!(
            "unsupported row format version: {version}"
        )));
    }
    let mut offset = 1usize;

    let tenant_len_field_end = offset
        .checked_add(2)
        .ok_or_else(|| StorageError::Codec("offset overflow at tenant_len field".to_string()))?;
    let tenant_len_bytes = buf.get(offset..tenant_len_field_end).ok_or_else(|| {
        StorageError::Codec("row buffer truncated at tenant_len field".to_string())
    })?;
    let tenant_len_arr: [u8; 2] = tenant_len_bytes
        .try_into()
        .map_err(|_| StorageError::Codec("tenant_len field is not 2 bytes".to_string()))?;
    let tenant_len = u16::from_le_bytes(tenant_len_arr);
    if tenant_len > MAX_TENANT_ID_LEN {
        return Err(StorageError::Codec(format!(
            "tenant_id length {tenant_len} exceeds limit {MAX_TENANT_ID_LEN}"
        )));
    }
    offset = offset
        .checked_add(2)
        .ok_or_else(|| StorageError::Codec("offset overflow after tenant_len field".to_string()))?;

    let tenant_end = offset
        .checked_add(tenant_len as usize)
        .ok_or_else(|| StorageError::Codec("offset overflow after tenant_id field".to_string()))?;
    let tenant_bytes = buf.get(offset..tenant_end).ok_or_else(|| {
        StorageError::Codec("row buffer truncated at tenant_id field".to_string())
    })?;
    if tenant_bytes.is_empty() {
        return Err(StorageError::Codec(
            "tenant_id must not be empty".to_string(),
        ));
    }
    let tenant_id = std::str::from_utf8(tenant_bytes)
        .map_err(|_| StorageError::Codec("tenant_id is not valid UTF-8".to_string()))?;
    offset = tenant_end;

    let visibility_byte = *buf.get(offset).ok_or_else(|| {
        StorageError::Codec("row buffer truncated at visibility field".to_string())
    })?;
    let visibility = Visibility::from_byte(visibility_byte)?;
    offset = offset
        .checked_add(1)
        .ok_or_else(|| StorageError::Codec("offset overflow after visibility field".to_string()))?;

    Ok((tenant_id, visibility, offset))
}

/// 行バイト列から `tenant_id`・`visibility` だけを取り出す（[`decode_row_header`] の
/// 薄いラッパー）。embedding・metadata のデコードは行わない。
///
/// 呼び出し文脈: `arena.rs::VectorArena::build_filtered` が、可視性判定（呼び出し元の
/// `predicate`）に必要な最小限の情報だけを、embedding のデコード（次元検証を含む）を
/// 経ずに安全に取得するために使う。これにより、不可視行（他テナント行）の embedding
/// 部分が破損していても、その破損を検出する前に `predicate` で弾いてスキップできる
/// （他テナントのデータ破損状態が対象テナントの検索可用性へ干渉しない設計にする。
/// codex P0 対応・Issue #137）。
///
/// ヘッダ部自体（バージョン・`tenant_len`・`tenant_id`・`visibility`）が破損していて
/// 可視性を判定できない場合は、`decode_row_header` と同じ理由で `Err` を返す
/// （呼び出し元はこの行を「不可視だからスキップ」とは判断できないため fail-closed。
/// `arena.rs` 側のドキュメント参照）。
///
/// `tenant_id` は `buf` を借用した `&str`（[`decode_row_header`] 参照）。ヘッダ比較
/// のみを行う呼び出し元（`PolicyContext::is_visible` への受け渡し）は借用のまま
/// 完結するため、この経路は行ごとのヒープアロケーションを伴わない（Issue #174）。
pub(crate) fn decode_row_tenant_and_visibility(buf: &[u8]) -> Result<(&str, Visibility)> {
    let (tenant_id, visibility, _offset_after_header) = decode_row_header(buf)?;
    Ok((tenant_id, visibility))
}

/// 集計の 3 段階デコード（`docs/design/aggregate-decode-skip.md`・Issue #1258）が
/// 埋め込み本体（`Vec<f32>` 化）を読まないことを直接観測するための計数プローブ。
///
/// `#[cfg(test)]` 限定で production ビルドには含まれない。`sql/aggregate.rs`・
/// `sql/group_by.rs` の走査ループは呼び出しスレッド上で同期実行されるため、
/// 並列実行される他テストの計数が混入しないよう global atomic ではなく
/// thread_local を使う。計数は各デコード関数の入口で 1 回加算するだけで、検証・
/// エラー経路（fail-closed 契約）には触れない。
#[cfg(test)]
pub(crate) mod decode_probe {
    use std::cell::Cell;

    thread_local! {
        static EMBEDDING_DECODES: Cell<u64> = const { Cell::new(0) };
        static DIM_AND_METADATA_DECODES: Cell<u64> = const { Cell::new(0) };
    }

    /// 計数のスナップショット。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct DecodeCounts {
        /// 埋め込み本体をデコードした回数（`decode_row_body_into`・`decode_row`）。
        pub(crate) embedding: u64,
        /// 構造検証のみの借用デコード回数（`decode_row_dim_and_metadata_borrowed`）。
        pub(crate) dim_and_metadata: u64,
    }

    impl DecodeCounts {
        /// `before` からの増分を返す。
        pub(crate) fn since(self, before: DecodeCounts) -> DecodeCounts {
            DecodeCounts {
                embedding: self.embedding - before.embedding,
                dim_and_metadata: self.dim_and_metadata - before.dim_and_metadata,
            }
        }
    }

    pub(crate) fn record_embedding_decode() {
        EMBEDDING_DECODES.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn record_dim_and_metadata_decode() {
        DIM_AND_METADATA_DECODES.with(|c| c.set(c.get() + 1));
    }

    pub(crate) fn snapshot() -> DecodeCounts {
        DecodeCounts {
            embedding: EMBEDDING_DECODES.with(Cell::get),
            dim_and_metadata: DIM_AND_METADATA_DECODES.with(Cell::get),
        }
    }
}

/// 行バイト列から `metadata`（スカラー列ペイロード）だけを取り出す（[`decode_row`] と
/// 同じヘッダ・embedding フィールド検証を経るが、embedding は `Vec<f32>` へ確保せず
/// 長さ検証のみでオフセットを進める）。`metadata` は `buf` を借用した `&[u8]`。
///
/// 呼び出し文脈: `tenant.rs::replace_typed_rows_by_text_key` が、テナント名前空間内の
/// 全行を走査してテキスト列（例: `path`）の一致判定を行う際に使う。当該経路は行ごとに
/// embedding をヒープ確保する必要がないため、[`decode_row`] を使うと
/// `MAX_SCANNED_ROWS` 級の走査で無視できない一時確保が発生し得る
/// （coding-rust.md「不安全な設計 / DoS」対応。`decode_row_tenant_and_visibility` が
/// 可視性判定のためにヘッダのみを借用で返す設計と同じ方針）。
/// 行バイト列から embedding・metadata を取り出す（[`decode_row`] と同じヘッダ・
/// 各フィールド検証を経るが、embedding は呼び出し元が渡した `out_embedding` へ
/// `clear()` 後に書き込む——[`decode_row`] のように行ごとに新しい `Vec<f32>` を
/// 確保しない。`out_embedding` の capacity は呼び出し元が複数行にわたって
/// 使い回すことを想定する（同一次元のテーブルを走査する限り、2 行目以降は
/// 再確保が発生しない）。`metadata` は [`decode_row_metadata_borrowed`] と同じく
/// `buf` を借用した `&[u8]`。返り値は `(dim, metadata)`。
///
/// 呼び出し文脈: `arena.rs::VectorArena::build_filtered_with_rows_and_limits_in_txn`
/// が、可視行 1 件ごとに `decode_row` を呼んで `Vec<f32>` を確保・即座に
/// `GrowableArenaBuffers::push_row`（単一の平坦バッファへ `extend_from_slice`）
/// でコピーしてから破棄していた冗長な確保を避けるために使う（Issue #314・
/// SQL-1・TASK-83 条件7: SQL 表層 C1 経路の固定コスト削減。行数に比例する
/// ヒープ確保回数を削るのが目的で、RLS 判定順序（`predicate` 通過後にのみ
/// embedding をデコードする契約）は変えない）。
///
/// ヘッダデコード（[`decode_row_header`]）を内部で行ってから本体デコード
/// （[`decode_row_body_into`]）へ委譲する薄いラッパー。呼び出し元が可視性判定の
/// ためにヘッダを別途デコード済みの場合（`sql/aggregate.rs`・`sql/group_by.rs`。
/// Issue #349）は、このラッパーではなく [`decode_row_body_into`] を直接呼んで
/// ヘッダの二重デコードを避けること。
pub(crate) fn decode_row_embedding_and_metadata_into<'a>(
    buf: &'a [u8],
    out_embedding: &mut Vec<f32>,
) -> Result<(u32, &'a [u8])> {
    let (_tenant_id, _visibility, offset) = decode_row_header(buf)?;
    decode_row_body_into(buf, offset, out_embedding)
}

/// [`decode_row_embedding_and_metadata_into`] の本体デコード部分（Issue #349 で
/// ヘッダデコードから分離）。`offset` は [`decode_row_header`] が返した
/// 「visibility バイトの直後のオフセット」をそのまま渡す——呼び出し元が
/// 可視性判定のために既にヘッダをデコード済みの場合、本関数はヘッダを
/// 再デコードしない。`out_embedding`・返り値の契約は
/// [`decode_row_embedding_and_metadata_into`] と同一。
///
/// 呼び出し文脈: `sql/aggregate.rs::execute_aggregate`・
/// `sql/group_by.rs::execute_grouped_aggregate` の走査ループが、
/// [`decode_row_header`] で可視性判定した直後にこの関数で本体をデコードする
/// （二段階デコード。ヘッダを 2 回読まない）。TABLE-12（キー側 `tenant_id` と
/// ヘッダ側 `tenant_id` の整合検査）は本関数の責務外——呼び出し元が
/// [`decode_row_header`] の返り値と物理キーを比較して行う（旧
/// `decode_row_for_key` が担っていた検査の呼び出し元側への移設）。
pub(crate) fn decode_row_body_into<'a>(
    buf: &'a [u8],
    offset: usize,
    out_embedding: &mut Vec<f32>,
) -> Result<(u32, &'a [u8])> {
    #[cfg(test)]
    decode_probe::record_embedding_decode();
    let mut offset = offset;
    let dim_field_end = offset
        .checked_add(4)
        .ok_or_else(|| StorageError::Codec("offset overflow at dim field".to_string()))?;
    let dim_bytes = buf
        .get(offset..dim_field_end)
        .ok_or_else(|| StorageError::Codec("row buffer truncated at dim field".to_string()))?;
    let dim_arr: [u8; 4] = dim_bytes
        .try_into()
        .map_err(|_| StorageError::Codec("dim field is not 4 bytes".to_string()))?;
    let dim = u32::from_le_bytes(dim_arr);
    if dim > MAX_EMBEDDING_DIM {
        return Err(StorageError::Codec(format!(
            "embedding dim {dim} exceeds limit {MAX_EMBEDDING_DIM}"
        )));
    }
    offset = offset
        .checked_add(4)
        .ok_or_else(|| StorageError::Codec("offset overflow after dim field".to_string()))?;

    let embedding_bytes_len = (dim as usize)
        .checked_mul(4)
        .ok_or_else(|| StorageError::Codec("embedding byte length overflow".to_string()))?;
    let embedding_end = offset
        .checked_add(embedding_bytes_len)
        .ok_or_else(|| StorageError::Codec("offset overflow after embedding field".to_string()))?;
    let embedding_bytes = buf.get(offset..embedding_end).ok_or_else(|| {
        StorageError::Codec("row buffer truncated at embedding field".to_string())
    })?;
    // `out_embedding` の capacity は上限検証済みの `dim`（`MAX_EMBEDDING_DIM` 以下）に
    // 有界。`clear()` は既存 capacity を保持するため、呼び出し元が同一バッファを
    // 複数行にわたって渡す限り 2 行目以降は再確保が発生しない。
    out_embedding.clear();
    out_embedding.reserve(dim as usize);
    for chunk in embedding_bytes.as_chunks::<4>().0 {
        out_embedding.push(f32::from_le_bytes(*chunk));
    }
    offset = embedding_end;

    let metadata_len_field_end = offset
        .checked_add(4)
        .ok_or_else(|| StorageError::Codec("offset overflow at metadata_len field".to_string()))?;
    let metadata_len_bytes = buf.get(offset..metadata_len_field_end).ok_or_else(|| {
        StorageError::Codec("row buffer truncated at metadata_len field".to_string())
    })?;
    let metadata_len_arr: [u8; 4] = metadata_len_bytes
        .try_into()
        .map_err(|_| StorageError::Codec("metadata_len field is not 4 bytes".to_string()))?;
    let metadata_len = u32::from_le_bytes(metadata_len_arr);
    if metadata_len > MAX_METADATA_LEN {
        return Err(StorageError::Codec(format!(
            "metadata length {metadata_len} exceeds limit {MAX_METADATA_LEN}"
        )));
    }
    offset = offset.checked_add(4).ok_or_else(|| {
        StorageError::Codec("offset overflow after metadata_len field".to_string())
    })?;

    let metadata_end = offset
        .checked_add(metadata_len as usize)
        .ok_or_else(|| StorageError::Codec("offset overflow after metadata field".to_string()))?;
    let metadata_bytes = buf
        .get(offset..metadata_end)
        .ok_or_else(|| StorageError::Codec("row buffer truncated at metadata field".to_string()))?;
    if metadata_end != buf.len() {
        return Err(StorageError::Codec(
            "row buffer has trailing bytes beyond declared metadata length".to_string(),
        ));
    }

    Ok((dim, metadata_bytes))
}

pub(crate) fn decode_row_metadata_borrowed(buf: &[u8]) -> Result<&[u8]> {
    decode_row_dim_and_metadata_borrowed(buf).map(|(_dim, metadata)| metadata)
}

/// [`decode_row_metadata_borrowed`] の dim 込み版。embedding 自体は境界検証のみで
/// `f32` へ変換・確保しない点は同じだが、次元検証（`schema.vector_dim()` との
/// 比較）を必要とする呼び出し元向けに `dim` も返す。
///
/// 呼び出し文脈: `sql::aggregate.rs`／`sql::group_by.rs` の集計行走査が、
/// embedding 自体は参照しないが `VECTOR` 列の NULL 判定（`dim == 0`）や次元検証は
/// 必要とするケース（Issue #350: embedding 非参照集計のデコードスキップ）で使う。
/// embedding を `Vec<f32>` へ確保する必要がある場合は
/// [`decode_row_embedding_and_metadata_into`] を使う。
pub(crate) fn decode_row_dim_and_metadata_borrowed(buf: &[u8]) -> Result<(u32, &[u8])> {
    #[cfg(test)]
    decode_probe::record_dim_and_metadata_decode();
    let (_tenant_id, _visibility, mut offset) = decode_row_header(buf)?;

    let dim_field_end = offset
        .checked_add(4)
        .ok_or_else(|| StorageError::Codec("offset overflow at dim field".to_string()))?;
    let dim_bytes = buf
        .get(offset..dim_field_end)
        .ok_or_else(|| StorageError::Codec("row buffer truncated at dim field".to_string()))?;
    let dim_arr: [u8; 4] = dim_bytes
        .try_into()
        .map_err(|_| StorageError::Codec("dim field is not 4 bytes".to_string()))?;
    let dim = u32::from_le_bytes(dim_arr);
    if dim > MAX_EMBEDDING_DIM {
        return Err(StorageError::Codec(format!(
            "embedding dim {dim} exceeds limit {MAX_EMBEDDING_DIM}"
        )));
    }
    offset = offset
        .checked_add(4)
        .ok_or_else(|| StorageError::Codec("offset overflow after dim field".to_string()))?;

    let embedding_bytes_len = (dim as usize)
        .checked_mul(4)
        .ok_or_else(|| StorageError::Codec("embedding byte length overflow".to_string()))?;
    let embedding_end = offset
        .checked_add(embedding_bytes_len)
        .ok_or_else(|| StorageError::Codec("offset overflow after embedding field".to_string()))?;
    // embedding バイト列は境界検証のみ行い、`f32` へは変換・確保しない。
    if buf.get(offset..embedding_end).is_none() {
        return Err(StorageError::Codec(
            "row buffer truncated at embedding field".to_string(),
        ));
    }
    offset = embedding_end;

    let metadata_len_field_end = offset
        .checked_add(4)
        .ok_or_else(|| StorageError::Codec("offset overflow at metadata_len field".to_string()))?;
    let metadata_len_bytes = buf.get(offset..metadata_len_field_end).ok_or_else(|| {
        StorageError::Codec("row buffer truncated at metadata_len field".to_string())
    })?;
    let metadata_len_arr: [u8; 4] = metadata_len_bytes
        .try_into()
        .map_err(|_| StorageError::Codec("metadata_len field is not 4 bytes".to_string()))?;
    let metadata_len = u32::from_le_bytes(metadata_len_arr);
    if metadata_len > MAX_METADATA_LEN {
        return Err(StorageError::Codec(format!(
            "metadata length {metadata_len} exceeds limit {MAX_METADATA_LEN}"
        )));
    }
    offset = offset.checked_add(4).ok_or_else(|| {
        StorageError::Codec("offset overflow after metadata_len field".to_string())
    })?;

    let metadata_end = offset
        .checked_add(metadata_len as usize)
        .ok_or_else(|| StorageError::Codec("offset overflow after metadata field".to_string()))?;
    let metadata_bytes = buf
        .get(offset..metadata_end)
        .ok_or_else(|| StorageError::Codec("row buffer truncated at metadata field".to_string()))?;
    if metadata_end != buf.len() {
        return Err(StorageError::Codec(
            "row buffer has trailing bytes beyond declared metadata length".to_string(),
        ));
    }

    Ok((dim, metadata_bytes))
}

/// キー側 `tenant_id`（複合キーの一部）とヘッダ側 `tenant_id`（エンコード済み行
/// 内容）の整合を検査する（対象ビヘイビア: TABLE-12）。raw `redb` 書き込みや
/// 将来のバグでキーのテナントと行内容のテナントがずれた行を黙って返さない
/// fail-closed な検査。[`decode_row_for_key`] から切り出した実体で、embedding を
/// 完全デコードしない部分デコード経路（Issue #350 の集計走査）でも同じ検査を
/// 1 本化して再利用する。
pub(crate) fn verify_row_key_tenant(key_tenant: &str, header_tenant: &str) -> Result<()> {
    if header_tenant != key_tenant {
        return Err(StorageError::Codec("row key tenant mismatch".to_string()));
    }
    Ok(())
}

/// [`encode_row`] の逆変換。欠落・不正値はすべて `Err` で拒否する（fail-closed。
/// 黙殺フォールバックで既知の型・デフォルト値へ落とさない）。添字アクセス `[]` ではなく
/// `get()` を使い、境界外アクセスを未定義動作にしない。
///
/// `pub(crate)`: `txn.rs`（TASK-88）の読み取りスナップショットハンドルが、`Storage::get`
/// と同一のデコード・fail-closed 契約で行を読み出すために再利用する。
pub(crate) fn decode_row(id: u64, buf: &[u8]) -> Result<Row> {
    #[cfg(test)]
    decode_probe::record_embedding_decode();
    let (tenant_id, visibility, mut offset) = decode_row_header(buf)?;

    let dim_field_end = offset
        .checked_add(4)
        .ok_or_else(|| StorageError::Codec("offset overflow at dim field".to_string()))?;
    let dim_bytes = buf
        .get(offset..dim_field_end)
        .ok_or_else(|| StorageError::Codec("row buffer truncated at dim field".to_string()))?;
    let dim_arr: [u8; 4] = dim_bytes
        .try_into()
        .map_err(|_| StorageError::Codec("dim field is not 4 bytes".to_string()))?;
    let dim = u32::from_le_bytes(dim_arr);
    if dim > MAX_EMBEDDING_DIM {
        return Err(StorageError::Codec(format!(
            "embedding dim {dim} exceeds limit {MAX_EMBEDDING_DIM}"
        )));
    }
    offset = offset
        .checked_add(4)
        .ok_or_else(|| StorageError::Codec("offset overflow after dim field".to_string()))?;

    let embedding_bytes_len = (dim as usize)
        .checked_mul(4)
        .ok_or_else(|| StorageError::Codec("embedding byte length overflow".to_string()))?;
    let embedding_end = offset
        .checked_add(embedding_bytes_len)
        .ok_or_else(|| StorageError::Codec("offset overflow after embedding field".to_string()))?;
    let embedding_bytes = buf.get(offset..embedding_end).ok_or_else(|| {
        StorageError::Codec("row buffer truncated at embedding field".to_string())
    })?;
    // 上限検証済みの dim に基づくため、無制限確保にはならない。
    let mut embedding = Vec::with_capacity(dim as usize);
    for chunk in embedding_bytes.as_chunks::<4>().0 {
        embedding.push(f32::from_le_bytes(*chunk));
    }
    offset = embedding_end;

    let metadata_len_field_end = offset
        .checked_add(4)
        .ok_or_else(|| StorageError::Codec("offset overflow at metadata_len field".to_string()))?;
    let metadata_len_bytes = buf.get(offset..metadata_len_field_end).ok_or_else(|| {
        StorageError::Codec("row buffer truncated at metadata_len field".to_string())
    })?;
    let metadata_len_arr: [u8; 4] = metadata_len_bytes
        .try_into()
        .map_err(|_| StorageError::Codec("metadata_len field is not 4 bytes".to_string()))?;
    let metadata_len = u32::from_le_bytes(metadata_len_arr);
    if metadata_len > MAX_METADATA_LEN {
        return Err(StorageError::Codec(format!(
            "metadata length {metadata_len} exceeds limit {MAX_METADATA_LEN}"
        )));
    }
    offset = offset.checked_add(4).ok_or_else(|| {
        StorageError::Codec("offset overflow after metadata_len field".to_string())
    })?;

    let metadata_end = offset
        .checked_add(metadata_len as usize)
        .ok_or_else(|| StorageError::Codec("offset overflow after metadata field".to_string()))?;
    let metadata_bytes = buf
        .get(offset..metadata_end)
        .ok_or_else(|| StorageError::Codec("row buffer truncated at metadata field".to_string()))?;
    if metadata_end != buf.len() {
        return Err(StorageError::Codec(
            "row buffer has trailing bytes beyond declared metadata length".to_string(),
        ));
    }

    Ok(Row {
        id,
        tenant_id: tenant_id.to_string(),
        visibility,
        embedding,
        metadata: metadata_bytes.to_vec(),
    })
}

/// [`ROWS_TABLE`] の読み取り経路（[`Storage::get`]・[`Storage::scan`]・
/// [`Storage::scan_page`]・[`crate::txn::ReadSnapshot::get`]）専用の [`decode_row`] 包み。
/// 物理キー側の `tenant_id`（`key_tenant`）とエンコード済み行内の `tenant_id` が
/// 一致することを確認してから返す（対象ビヘイビア: TABLE-12）。
///
/// raw `redb` 書き込みや将来のバグでキーのテナントと行内容のテナントがずれた行を
/// 黙って返さない fail-closed な整合検査であり、`decode_row` 単体では検出できない
/// （`decode_row` はキー側 `tenant_id` を知らない）。不一致は
/// [`StorageError::Codec`] で拒否する（テナント境界の異常検知であり、
/// `IncompatibleRowKeyFormat` のような DB 全体のフォーマット不整合とは性質が異なる
/// ため既存 variant を再利用する）。
pub(crate) fn decode_row_for_key(key_tenant: &str, id: u64, raw: &[u8]) -> Result<Row> {
    let row = decode_row(id, raw)?;
    verify_row_key_tenant(key_tenant, &row.tenant_id)?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(embedding: &[f32], metadata: &[u8]) -> Vec<u8> {
        sample_row_with_rls("tenant-a", Visibility::Public, embedding, metadata)
    }

    fn sample_row_with_rls(
        tenant_id: &str,
        visibility: Visibility,
        embedding: &[f32],
        metadata: &[u8],
    ) -> Vec<u8> {
        encode_row(&RowInput {
            tenant_id,
            visibility,
            embedding,
            metadata,
        })
        .unwrap()
    }

    #[test]
    fn row_roundtrips_through_encode_decode() {
        let embedding = vec![0.5_f32, -1.0, 2.25];
        let metadata = b"opaque".to_vec();
        let buf = sample_row_with_rls("tenant-a", Visibility::Private, &embedding, &metadata);
        let row = decode_row(7, &buf).unwrap();
        assert_eq!(row.id, 7);
        assert_eq!(row.tenant_id, "tenant-a");
        assert_eq!(row.visibility, Visibility::Private);
        assert_eq!(row.embedding, embedding);
        assert_eq!(row.metadata, metadata);
    }

    #[test]
    fn row_roundtrips_with_empty_embedding_and_metadata() {
        let buf = sample_row(&[], &[]);
        let row = decode_row(1, &buf).unwrap();
        assert!(row.embedding.is_empty());
        assert!(row.metadata.is_empty());
    }

    #[test]
    fn decode_row_rejects_version1_row_without_rls_fields() {
        // v1（RLS フィールドを持たない）行を模したバッファ。version=1 の行を暗黙の
        // デフォルトテナントとして読み出すのは fail-open（P0 違反）であり、拒否しなければ
        // ならない。
        let mut v1_buf = Vec::new();
        v1_buf.push(1u8); // ROW_FORMAT_VERSION（旧 v1）
        v1_buf.extend_from_slice(&0u32.to_le_bytes()); // dim=0
        v1_buf.extend_from_slice(&0u32.to_le_bytes()); // metadata_len=0
        let err = decode_row(1, &v1_buf).expect_err("v1 row must be rejected");
        assert!(matches!(err, StorageError::Codec(_)));
    }

    #[test]
    fn decode_row_rejects_unknown_visibility_byte() {
        let mut buf = sample_row(&[1.0], b"m");
        // レイアウト: [version][tenant_len(2)][tenant bytes]["tenant-a" は 8 バイト][visibility]
        let visibility_offset = 1 + 2 + "tenant-a".len();
        buf[visibility_offset] = 0xFF;
        let err = decode_row(1, &buf).expect_err("unknown visibility byte must be rejected");
        assert!(matches!(err, StorageError::Codec(_)));
    }

    #[test]
    fn encode_row_rejects_empty_tenant_id() {
        let result = encode_row(&RowInput {
            tenant_id: "",
            visibility: Visibility::Public,
            embedding: &[],
            metadata: &[],
        });
        assert!(matches!(result, Err(StorageError::Codec(_))));
    }

    #[test]
    fn encode_row_rejects_oversized_tenant_id() {
        let huge_tenant = "t".repeat((MAX_TENANT_ID_LEN as usize) + 1);
        let result = encode_row(&RowInput {
            tenant_id: &huge_tenant,
            visibility: Visibility::Public,
            embedding: &[],
            metadata: &[],
        });
        assert!(matches!(result, Err(StorageError::Codec(_))));
    }

    #[test]
    fn decode_row_rejects_non_utf8_tenant_bytes() {
        let mut buf = sample_row(&[1.0], b"m");
        // "tenant-a" の先頭バイトを不正な UTF-8 継続バイトへ書き換える。
        let tenant_start = 1 + 2;
        buf[tenant_start] = 0xFF;
        let err = decode_row(1, &buf).expect_err("non-UTF-8 tenant_id must be rejected");
        assert!(matches!(err, StorageError::Codec(_)));
    }

    #[test]
    fn decode_row_rejects_tenant_len_exceeding_limit() {
        let mut buf = sample_row(&[1.0], b"m");
        let oversized = MAX_TENANT_ID_LEN + 1;
        buf[1..3].copy_from_slice(&oversized.to_le_bytes());
        let err = decode_row(1, &buf).expect_err("oversized tenant_len must be rejected");
        assert!(matches!(err, StorageError::Codec(_)));
    }

    #[test]
    fn decode_row_rejects_zero_length_tenant_id() {
        // encode_row 経由では空 tenant_id を作れないため（エンコード時に拒否される）、
        // decode_row 側の空 tenant_id 拒否パス（RLS が黙って無効化される fail-open 経路）を
        // 単体で検証するために、tenant_len=0 の一貫したバッファを直接組み立てる。
        let mut buf = Vec::new();
        buf.push(ROW_FORMAT_VERSION);
        buf.extend_from_slice(&0u16.to_le_bytes()); // tenant_len = 0
        buf.push(Visibility::Public.to_byte());
        buf.extend_from_slice(&0u32.to_le_bytes()); // dim = 0
        buf.extend_from_slice(&0u32.to_le_bytes()); // metadata_len = 0
        let err = decode_row(1, &buf).expect_err("zero-length tenant_id must be rejected");
        assert!(matches!(err, StorageError::Codec(_)));
    }

    #[test]
    fn decode_row_rejects_truncated_buffer() {
        let buf = sample_row(&[1.0, 2.0], b"meta");
        for cut in 1..buf.len() {
            assert!(
                decode_row(1, &buf[..cut]).is_err(),
                "truncated buffer of length {cut} should not decode successfully"
            );
        }
    }

    #[test]
    fn decode_row_rejects_unknown_version() {
        let mut buf = sample_row(&[1.0], b"m");
        buf[0] = 0xFF;
        assert!(decode_row(1, &buf).is_err());
    }

    #[test]
    fn decode_row_rejects_trailing_garbage() {
        let mut buf = sample_row(&[1.0], b"m");
        buf.push(0);
        assert!(decode_row(1, &buf).is_err());
    }

    #[test]
    fn decode_row_rejects_oversized_dim_without_allocating() {
        // dim をアロケーション上限より大きい値に書き換えたバッファ。
        // decode_row は Vec::with_capacity を呼ぶ前に dim の上限検証で拒否するべき。
        let mut buf = sample_row(&[1.0], b"m");
        let oversized = MAX_EMBEDDING_DIM + 1;
        // レイアウト: [version(1)][tenant_len(2)]["tenant-a"(8)][visibility(1)][dim(4)]。
        let dim_offset = 1 + 2 + "tenant-a".len() + 1;
        buf[dim_offset..dim_offset + 4].copy_from_slice(&oversized.to_le_bytes());
        assert!(decode_row(1, &buf).is_err());
    }

    #[test]
    fn encode_row_rejects_oversized_metadata() {
        let huge = vec![0u8; (MAX_METADATA_LEN as usize) + 1];
        let result = encode_row(&RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[],
            metadata: &huge,
        });
        assert!(result.is_err());
    }

    // Issue #398: `encode_row_into`（append 型）が `encode_row`（ラッパー）と
    // 完全一致すること・エラー時にバッファを変更しないこと・エンディアン非依存
    // であることを検証する。

    fn sample_rows_for_encode_into_matrix() -> Vec<RowInput<'static>> {
        // embedding 長 0/1/3/128・metadata 空/非空・tenant 長の組み合わせ
        // （4.5 節）。`'static` にするためリテラル・リークした Vec を使う。
        const EMB0: &[f32] = &[];
        const EMB1: &[f32] = &[1.5];
        const EMB3: &[f32] = &[1.0, -2.5, 0.0];
        let emb128: &'static [f32] = Box::leak(vec![0.25_f32; 128].into_boxed_slice());
        let long_tenant: &'static str =
            Box::leak(("t".repeat(MAX_TENANT_ID_LEN as usize)).into_boxed_str());
        vec![
            RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Public,
                embedding: EMB0,
                metadata: b"",
            },
            RowInput {
                tenant_id: "tenant-a",
                visibility: Visibility::Private,
                embedding: EMB1,
                metadata: b"meta",
            },
            RowInput {
                tenant_id: "t",
                visibility: Visibility::Public,
                embedding: EMB3,
                metadata: b"",
            },
            RowInput {
                tenant_id: long_tenant,
                visibility: Visibility::Private,
                embedding: emb128,
                metadata: b"opaque-metadata-bytes",
            },
        ]
    }

    #[test]
    fn encode_row_into_matches_encode_row() {
        for row in sample_rows_for_encode_into_matrix() {
            let expected = encode_row(&row).unwrap();
            let mut buf = Vec::new();
            encode_row_into(&mut buf, &row).unwrap();
            assert_eq!(buf, expected);
        }
    }

    #[test]
    fn encode_row_into_appends_after_existing_prefix() {
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[1.0, 2.0],
            metadata: b"meta",
        };
        let expected = encode_row(&row).unwrap();
        let prefix = vec![0xAAu8, 0xBB, 0xCC];
        let mut buf = prefix.clone();
        encode_row_into(&mut buf, &row).unwrap();
        assert_eq!(&buf[..prefix.len()], prefix.as_slice());
        assert_eq!(&buf[prefix.len()..], expected.as_slice());
    }

    #[test]
    fn encode_row_into_leaves_buffer_unchanged_on_error() {
        let bad_rows = [
            RowInput {
                tenant_id: "",
                visibility: Visibility::Public,
                embedding: &[],
                metadata: &[],
            },
            RowInput {
                tenant_id: &"t".repeat((MAX_TENANT_ID_LEN as usize) + 1),
                visibility: Visibility::Public,
                embedding: &[],
                metadata: &[],
            },
        ];
        for row in &bad_rows {
            let mut buf = vec![1u8, 2, 3];
            let snapshot = buf.clone();
            let result = encode_row_into(&mut buf, row);
            assert!(result.is_err());
            assert_eq!(buf, snapshot, "buffer must be unchanged after Err");
        }
    }

    #[test]
    fn encode_row_into_embedding_bytes_are_little_endian_fixed() {
        // ホストに依存しない固定期待バイト列（`to_le_bytes` の定義どおり）。
        let embedding: Vec<f32> = vec![1.0, -2.5, f32::MIN_POSITIVE, f32::NAN, 0.0, -0.0];
        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &embedding,
            metadata: b"",
        };
        let mut buf = Vec::new();
        encode_row_into(&mut buf, &row).unwrap();

        // ヘッダ長: version(1) + tenant_len(2) + "tenant-a"(8) + visibility(1) + dim(4)
        let header_len = 1 + 2 + "tenant-a".len() + 1 + 4;
        let embedding_bytes = &buf[header_len..header_len + embedding.len() * 4];

        // 参照ループ: `to_le_bytes` を直接連結したもの（ホスト非依存の定義）。
        let mut reference = Vec::new();
        for v in &embedding {
            reference.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(embedding_bytes, reference.as_slice());

        // 固定バイト列の一例（1.0f32 → 00 00 80 3F）。
        assert_eq!(&embedding_bytes[0..4], &[0x00, 0x00, 0x80, 0x3F]);

        // decode_row で往復一致（NaN はビットパターン比較）。
        let decoded = decode_row(1, &buf).unwrap();
        assert_eq!(decoded.embedding.len(), embedding.len());
        for (a, b) in decoded.embedding.iter().zip(embedding.iter()) {
            assert_eq!(a.to_bits(), b.to_bits());
        }
    }

    #[test]
    fn encoded_row_len_matches_actual_encoded_len() {
        for row in sample_rows_for_encode_into_matrix() {
            let expected_len = encode_row(&row).unwrap().len();
            assert_eq!(encoded_row_len(&row).unwrap(), expected_len);
        }

        let oversized_metadata = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[],
            metadata: &vec![0u8; (MAX_METADATA_LEN as usize) + 1],
        };
        assert!(encoded_row_len(&oversized_metadata).is_err());
    }

    // 一時 DB パス払い出し（`unique_db_path` / `CleanupGuard`）は Issue #173 で
    // `crate::test_util::temp_db` へ一本化した（旧: このモジュール内の複製）。
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};

    #[test]
    fn decode_row_tenant_and_visibility_matches_full_decode_header() {
        // ヘッダのみ decode（本 PR で借用 &str 化した経路）が、フル decode の
        // Row.tenant_id / Row.visibility と常に一致することを示す同等性テスト
        // （Issue #174: 借用化で判定結果が変わっていないことの担保）。
        let cases: &[(&str, Visibility)] = &[
            ("tenant-a", Visibility::Public),
            ("tenant-b", Visibility::Private),
            ("テナント-あ", Visibility::Public), // マルチバイト UTF-8
            (
                "t".repeat(MAX_TENANT_ID_LEN as usize).leak(),
                Visibility::Private,
            ), // 上限ちょうど
        ];
        for &(tenant_id, visibility) in cases {
            let buf = sample_row_with_rls(tenant_id, visibility, &[1.0, 2.0], b"meta");
            let (header_tenant, header_visibility) =
                decode_row_tenant_and_visibility(&buf).unwrap();
            let row = decode_row(1, &buf).unwrap();
            assert_eq!(header_tenant, row.tenant_id);
            assert_eq!(header_visibility, row.visibility);
        }
    }

    #[test]
    fn decode_row_tenant_and_visibility_borrows_from_input_buffer() {
        // 返却 &str が buf そのものを指すこと（コピーでなく借用であること）を
        // ポインタ範囲と長さで確認する（Issue #174: ヘッダ比較経路の非アロケーション化の
        // 決定的な証明。#[global_allocator] によるカウントは並列テスト下で非決定的になり
        // やすく依存追加も避けたいため採用しない）。
        let buf = sample_row_with_rls("tenant-a", Visibility::Public, &[1.0], b"m");
        let (tenant_id, _visibility) = decode_row_tenant_and_visibility(&buf).unwrap();
        let buf_range = buf.as_ptr_range();
        assert!(buf_range.contains(&tenant_id.as_ptr()));
        assert_eq!(tenant_id.len(), "tenant-a".len());
    }

    #[test]
    fn decode_row_tenant_and_visibility_fails_closed_on_same_header_corruptions_as_decode_row() {
        // ヘッダ破損時、ヘッダのみ decode とフル decode の両方が Err を返すこと
        // （fail-closed の契約が借用化後も維持されていることの確認。Issue #174）。
        let corrupt_cases: Vec<Vec<u8>> = vec![
            Vec::new(), // 空バッファ
            {
                let mut buf = sample_row(&[1.0], b"m");
                buf[0] = 0xFF; // 未知版数
                buf
            },
            {
                let mut buf = sample_row(&[1.0], b"m");
                let oversized = MAX_TENANT_ID_LEN + 1;
                buf[1..3].copy_from_slice(&oversized.to_le_bytes()); // tenant_len 上限超過
                buf
            },
            {
                // ヘッダ領域自体（visibility バイトの直前）で切り詰める。末尾（dim/metadata）
                // のみの切り詰めはヘッダのみ decode の成否に影響しないため、ヘッダ decode も
                // 失敗させるにはヘッダ領域内で切り詰める必要がある。
                let buf = sample_row(&[1.0], b"m");
                let visibility_offset = 1 + 2 + "tenant-a".len();
                buf[..visibility_offset].to_vec()
            },
            {
                let mut buf = Vec::new();
                buf.push(ROW_FORMAT_VERSION);
                buf.extend_from_slice(&0u16.to_le_bytes()); // tenant_len = 0（空 tenant_id）
                buf.push(Visibility::Public.to_byte());
                buf.extend_from_slice(&0u32.to_le_bytes());
                buf.extend_from_slice(&0u32.to_le_bytes());
                buf
            },
            {
                let mut buf = sample_row(&[1.0], b"m");
                let tenant_start = 1 + 2;
                buf[tenant_start] = 0xFF; // 非 UTF-8
                buf
            },
            {
                let mut buf = sample_row(&[1.0], b"m");
                let visibility_offset = 1 + 2 + "tenant-a".len();
                buf[visibility_offset] = 0xFF; // 未知 visibility バイト
                buf
            },
        ];
        for buf in corrupt_cases {
            let header_result = decode_row_tenant_and_visibility(&buf);
            let full_result = decode_row(1, &buf);
            assert!(header_result.is_err(), "header decode should fail-closed");
            assert!(full_result.is_err(), "full decode should fail-closed");
        }
    }

    // Issue #314・SQL-1・TASK-83 条件7: `decode_row_embedding_and_metadata_into`
    // （行ごとのヒープ確保を避ける借用デコード API。`arena.rs` の可視行走査から
    // 呼ばれる）が `decode_row` と同じ結果（embedding・metadata）を返すことを
    // 確認する。
    #[test]
    fn decode_row_embedding_and_metadata_into_matches_full_decode() {
        let embedding = [1.0_f32, -2.5, 0.0, 3.75];
        let metadata = b"scalar-columns";
        let buf = sample_row(&embedding, metadata);

        let full = decode_row(1, &buf).expect("full decode");

        let mut scratch: Vec<f32> = Vec::new();
        let (dim, borrowed_metadata) =
            decode_row_embedding_and_metadata_into(&buf, &mut scratch).expect("borrowed decode");

        assert_eq!(dim, embedding.len() as u32);
        assert_eq!(scratch, full.embedding);
        assert_eq!(borrowed_metadata, full.metadata.as_slice());
    }

    // Issue #349: `sql/aggregate.rs`・`sql/group_by.rs` の走査ループが使う
    // 「ヘッダを 1 回だけデコードし、そのオフセットを本体デコードへ引き継ぐ」経路
    // （`decode_row_header` → `decode_row_body_into`）が、ヘッダを再デコードする
    // 従来経路（`decode_row_embedding_and_metadata_into`）と同じ結果を返すことを
    // 確認する（オフセット引き継ぎの正しさの roundtrip 検証）。
    #[test]
    fn decode_row_header_then_body_into_matches_decode_row_embedding_and_metadata_into() {
        let embedding = [4.5_f32, -1.0, 0.0, 9.25, 2.0];
        let metadata = b"header-body-split";
        let buf = sample_row(&embedding, metadata);

        let (tenant_id, visibility, offset) = decode_row_header(&buf).expect("header decode");
        assert_eq!(tenant_id, "tenant-a");
        assert_eq!(visibility, Visibility::Public);

        let mut scratch_split: Vec<f32> = Vec::new();
        let (dim_split, metadata_split) =
            decode_row_body_into(&buf, offset, &mut scratch_split).expect("body decode");

        let mut scratch_direct: Vec<f32> = Vec::new();
        let (dim_direct, metadata_direct) =
            decode_row_embedding_and_metadata_into(&buf, &mut scratch_direct)
                .expect("direct decode");

        assert_eq!(dim_split, dim_direct);
        assert_eq!(scratch_split, scratch_direct);
        assert_eq!(metadata_split, metadata_direct);
    }

    // Issue #349: ヘッダは正常だが本体（dim 超過）が破損している場合、
    // `decode_row_body_into` は fail-closed に `Err` を返す（ヘッダデコードの
    // 成否と本体デコードの成否は独立に検証されるべきという契約の確認）。
    #[test]
    fn decode_row_body_into_rejects_dim_exceeding_limit() {
        let mut buf = sample_row(&[1.0_f32], b"m");
        let (_tenant_id, _visibility, offset) = decode_row_header(&buf).expect("header decode");
        // dim フィールド（オフセット直後 4 バイト）を上限超過値へ書き換える。
        let oversized_dim = (MAX_EMBEDDING_DIM + 1).to_le_bytes();
        buf[offset..offset + 4].copy_from_slice(&oversized_dim);

        let mut scratch: Vec<f32> = Vec::new();
        assert!(decode_row_body_into(&buf, offset, &mut scratch).is_err());
    }

    // Issue #349: ヘッダは正常だが本体が途中で truncate されている場合も
    // `decode_row_body_into` は fail-closed に `Err` を返す。
    #[test]
    fn decode_row_body_into_rejects_truncated_body() {
        let buf = sample_row(&[1.0_f32, 2.0, 3.0], b"metadata-here");
        let (_tenant_id, _visibility, offset) = decode_row_header(&buf).expect("header decode");
        // embedding の途中で切り詰める（metadata_len・metadata に到達しない）。
        let truncated = &buf[..offset + 4 + 4];

        let mut scratch: Vec<f32> = Vec::new();
        assert!(decode_row_body_into(truncated, offset, &mut scratch).is_err());
    }

    // Issue #349: 末尾に余剰バイトが付与された行は `decode_row_body_into` でも
    // 従来どおり拒否される（`decode_row`/`decode_row_embedding_and_metadata_into`
    // と同じ「宣言済み metadata 長を超えるバイトを許さない」契約を本体分割後も
    // 維持することの確認）。
    #[test]
    fn decode_row_body_into_rejects_trailing_bytes() {
        let mut buf = sample_row(&[1.0_f32], b"m");
        let (_tenant_id, _visibility, offset) = decode_row_header(&buf).expect("header decode");
        buf.push(0xFF);

        let mut scratch: Vec<f32> = Vec::new();
        assert!(decode_row_body_into(&buf, offset, &mut scratch).is_err());
    }

    // 呼び出し元（`arena.rs`）は 1 本の `scratch: Vec<f32>` を可視行の走査ループ全体で
    // 使い回す契約（構築時は行数分の `Vec<f32>` を個別確保しない設計。上記対応
    // Issue 参照）。同一次元の行を 2 回デコードしても capacity が伸長しない
    // （＝2 回目の呼び出しで再確保が発生しない）ことを、既存の
    // `decode_row_tenant_and_visibility_borrows_from_input_buffer` と同じ「ポインタ／
    // capacity に基づく確保有無の証明」方針（本ファイル上部のコメント参照。
    // `#[global_allocator]` によるカウントは並列テスト下で非決定的になりやすく
    // 依存追加も避けたいため採用しない、という既存方針を踏襲する）で確認する。
    #[test]
    fn decode_row_embedding_and_metadata_into_reuses_scratch_capacity_across_calls() {
        let dim = 8;
        let embedding_a: Vec<f32> = (0..dim).map(|i| i as f32).collect();
        let embedding_b: Vec<f32> = (0..dim).map(|i| -(i as f32)).collect();
        let buf_a = sample_row(&embedding_a, b"a");
        let buf_b = sample_row(&embedding_b, b"bb");

        let mut scratch: Vec<f32> = Vec::new();
        decode_row_embedding_and_metadata_into(&buf_a, &mut scratch).expect("decode row a");
        let capacity_after_first = scratch.capacity();
        let ptr_after_first = scratch.as_ptr();
        assert!(
            capacity_after_first >= embedding_a.len(),
            "first decode must reserve at least dim capacity"
        );

        decode_row_embedding_and_metadata_into(&buf_b, &mut scratch).expect("decode row b");
        // capacity の一致だけでは「たまたま同じ大きさで再確保した」場合を見逃す
        // （新しい確保・解放のポインタが偶然一致する可能性は排除できないが、通常の
        // アロケータでは同一プロセス内の確保直後に同一ポインタが再利用されることは
        // 稀であり、実装が誤って毎回新しい `Vec` を作る退行へ落ちた場合はほぼ確実に
        // ポインタが変わる）。ポインタ同一性を主な証拠、capacity 一致を補助的な
        // 証拠として両方確認する。
        assert_eq!(
            scratch.as_ptr(),
            ptr_after_first,
            "同一次元の 2 回目の呼び出しでバッファの確保先ポインタが変わっては \
             ならない（行ごとの新規ヒープ確保を避ける契約。Issue #314）"
        );
        assert_eq!(
            scratch.capacity(),
            capacity_after_first,
            "同一次元の 2 回目の呼び出しで capacity が伸長してはならない"
        );
        assert_eq!(
            scratch, embedding_b,
            "2 回目の呼び出し後は最新行の embedding を保持する"
        );
    }

    // Issue #314 レビュー指摘（Low）: `decode_row_embedding_and_metadata_into` の
    // fail-closed 分岐（オーバーフロー安全・MAX_EMBEDDING_DIM・MAX_METADATA_LEN 上限・
    // trailing bytes 検証）は既存 `decode_row` と同一のはずだが、テストは正常系
    // （`_matches_full_decode`・`_reuses_scratch_capacity_across_calls`）のみで
    // 拒否系のテストカバレッジが無かった。`decode_row` の既存拒否系テスト
    // （`decode_row_rejects_oversized_dim_without_allocating`・
    // `decode_row_rejects_trailing_garbage`・`encode_row_rejects_oversized_metadata`）と
    // 対になる形で、`decode_row` と `decode_row_embedding_and_metadata_into` の両方が
    // 同じ入力を fail-closed で拒否することを確認する。
    #[test]
    fn decode_row_embedding_and_metadata_into_rejects_oversized_dim_without_allocating() {
        // dim をアロケーション上限より大きい値に書き換えたバッファ。
        // `decode_row_embedding_and_metadata_into` は `out_embedding.reserve` を呼ぶ前に
        // dim の上限検証で拒否するべき（decode_row_rejects_oversized_dim_without_allocating
        // と同じ意図）。
        let mut buf = sample_row(&[1.0], b"m");
        let oversized = MAX_EMBEDDING_DIM + 1;
        // レイアウト: [version(1)][tenant_len(2)]["tenant-a"(8)][visibility(1)][dim(4)]。
        let dim_offset = 1 + 2 + "tenant-a".len() + 1;
        buf[dim_offset..dim_offset + 4].copy_from_slice(&oversized.to_le_bytes());

        let mut scratch: Vec<f32> = Vec::new();
        assert!(decode_row(1, &buf).is_err());
        assert!(decode_row_embedding_and_metadata_into(&buf, &mut scratch).is_err());
    }

    #[test]
    fn decode_row_embedding_and_metadata_into_rejects_oversized_metadata_len() {
        // metadata_len フィールドを MAX_METADATA_LEN 超過に書き換え、実バッファは
        // 追随させない（decode_row 側でも metadata_len 検証はバッファ長に依らず
        // フィールド値のみで先に拒否される契約）。
        let mut buf = sample_row(&[1.0], b"m");
        let dim_offset = 1 + 2 + "tenant-a".len() + 1;
        let metadata_len_offset = dim_offset + 4 + 4; // dim=1 分の embedding バイト列（4 バイト）後
        let oversized = MAX_METADATA_LEN + 1;
        buf[metadata_len_offset..metadata_len_offset + 4].copy_from_slice(&oversized.to_le_bytes());

        let mut scratch: Vec<f32> = Vec::new();
        assert!(decode_row(1, &buf).is_err());
        assert!(decode_row_embedding_and_metadata_into(&buf, &mut scratch).is_err());
    }

    #[test]
    fn decode_row_embedding_and_metadata_into_rejects_trailing_garbage() {
        let mut buf = sample_row(&[1.0], b"m");
        buf.push(0);

        let mut scratch: Vec<f32> = Vec::new();
        assert!(decode_row(1, &buf).is_err());
        assert!(decode_row_embedding_and_metadata_into(&buf, &mut scratch).is_err());
    }

    #[test]
    fn decode_row_embedding_and_metadata_into_rejects_truncated_buffer() {
        // embedding フィールドの途中でバッファを切り詰める（dim=2 を宣言しつつ
        // embedding バイト列を 1 要素分しか持たない）。
        let buf = sample_row(&[1.0, 2.0], b"m");
        let dim_offset = 1 + 2 + "tenant-a".len() + 1;
        let truncated = &buf[..dim_offset + 4 + 4]; // dim フィールド + embedding 4 バイトのみ

        let mut scratch: Vec<f32> = Vec::new();
        assert!(decode_row(1, truncated).is_err());
        assert!(decode_row_embedding_and_metadata_into(truncated, &mut scratch).is_err());
    }

    // --- decode_row_dim_and_metadata_borrowed / verify_row_key_tenant（Issue #350） ---

    #[test]
    fn decode_row_dim_and_metadata_borrowed_matches_full_decode() {
        let embedding = vec![1.0_f32, 2.0, 3.0];
        let metadata = b"scalar-payload".to_vec();
        let buf = sample_row(&embedding, &metadata);

        let full = decode_row(1, &buf).expect("full decode");
        let (dim, borrowed_metadata) =
            decode_row_dim_and_metadata_borrowed(&buf).expect("dim+metadata decode");
        assert_eq!(dim as usize, full.embedding.len());
        assert_eq!(borrowed_metadata, full.metadata.as_slice());
    }

    #[test]
    fn decode_row_dim_and_metadata_borrowed_agrees_with_metadata_borrowed_wrapper() {
        let buf = sample_row(&[1.0, 2.0], b"m");
        let (_dim, via_dim_fn) =
            decode_row_dim_and_metadata_borrowed(&buf).expect("dim+metadata decode");
        let via_wrapper = decode_row_metadata_borrowed(&buf).expect("metadata-only decode");
        assert_eq!(via_dim_fn, via_wrapper);
    }

    #[test]
    fn decode_row_dim_and_metadata_borrowed_rejects_same_corruptions_as_decode_row() {
        // metadata_len を MAX_METADATA_LEN 超過に書き換えても、`decode_row`（完全
        // デコード）と同じ理由で `Err` になる（部分デコード経路が検証を弱めない
        // ことの固定）。
        let mut buf = sample_row(&[1.0], b"m");
        let dim_offset = 1 + 2 + "tenant-a".len() + 1;
        let metadata_len_offset = dim_offset + 4 + 4;
        let oversized = MAX_METADATA_LEN + 1;
        buf[metadata_len_offset..metadata_len_offset + 4].copy_from_slice(&oversized.to_le_bytes());

        assert!(decode_row(1, &buf).is_err());
        assert!(decode_row_dim_and_metadata_borrowed(&buf).is_err());
    }

    #[test]
    fn verify_row_key_tenant_accepts_matching_tenant() {
        assert!(verify_row_key_tenant("tenant-a", "tenant-a").is_ok());
    }

    #[test]
    fn verify_row_key_tenant_rejects_mismatched_tenant() {
        let err = verify_row_key_tenant("tenant-a", "tenant-b").unwrap_err();
        assert!(matches!(err, StorageError::Codec(_)));
    }

    #[test]
    fn decode_row_for_key_and_verify_row_key_tenant_agree_on_mismatch() {
        // `decode_row_for_key`（完全デコード経路の TABLE-12 検査）と
        // `verify_row_key_tenant`（部分デコード経路が再利用する切り出し済み検査）
        // が同じ入力に対して同じ結論（拒否）に達することを確認する。
        let buf = sample_row(&[1.0], b"m");
        assert!(decode_row_for_key("tenant-b", 1, &buf).is_err());
        assert!(verify_row_key_tenant("tenant-b", "tenant-a").is_err());
    }

    // TASK-133 P1 対応: 書き込みコミットのたびに世代カウンタが単調増加し、無関係な
    // 読み取り操作では増加しないことを確認する。
    #[test]
    fn current_generation_increments_only_on_write_commits() {
        let path = unique_db_path("generation");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        assert_eq!(storage.current_generation().expect("gen 0"), 0);

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
            .expect("put row 1");
        assert_eq!(storage.current_generation().expect("gen after put"), 1);

        storage
            .put_batch(&[(
                2,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[2.0],
                    metadata: &[],
                },
            )])
            .expect("put_batch row 2");
        assert_eq!(
            storage.current_generation().expect("gen after put_batch"),
            2
        );

        // 読み取りのみの操作は世代を進めない。
        let _ = storage.get("tenant-a", 1).expect("get row 1");
        let _ = storage.scan().expect("scan");
        assert_eq!(
            storage.current_generation().expect("gen after reads"),
            2,
            "read-only operations must not bump the generation counter"
        );
    }

    /// 取りこぼし検査（Issue #849）: `Storage::put`/`put_batch`（Rust API 単行/複数行
    /// 書き込み）が [`Storage::begin_write_txn`] choke point を経由することを、
    /// 呼び出し前後の `write_txn_creations()` の差分で非 vacuous に確認する。
    /// 空バッチはトランザクションを開かない既存契約（[`Storage::put_batch`] の
    /// ドキュメンテーションコメント参照）に対応し、カウンタが増加しないことも固定する。
    #[test]
    fn put_and_put_batch_go_through_storage_choke_point() {
        let path = unique_db_path("durability-choke-point-put");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        assert_eq!(storage.write_txn_creations(), 0);

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
            .expect("put");
        assert_eq!(storage.write_txn_creations(), 1, "put");

        storage
            .put_batch(&[
                (
                    2,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[2.0],
                        metadata: &[],
                    },
                ),
                (
                    3,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[3.0],
                        metadata: &[],
                    },
                ),
            ])
            .expect("put_batch");
        assert_eq!(
            storage.write_txn_creations(),
            2,
            "put_batch は複数行でも単一トランザクション"
        );

        storage.put_batch(&[]).expect("empty put_batch");
        assert_eq!(
            storage.write_txn_creations(),
            2,
            "空バッチはトランザクションを開かない既存契約が choke point 経由でも不変"
        );
    }

    #[test]
    fn scan_page_paginates_in_id_order_and_reports_continuation_cursor() {
        let path = unique_db_path("scan-page");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let embeddings: Vec<[f32; 1]> = (0..25u64).map(|i| [i as f32]).collect();
        let rows: Vec<(u64, RowInput<'_>)> = (0..25u64)
            .map(|i| {
                (
                    i,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &embeddings[i as usize],
                        metadata: b"m",
                    },
                )
            })
            .collect();
        storage.put_batch(&rows).expect("seed rows");

        let (page1, cursor1) = storage.scan_page(None, 10).expect("first page");
        assert_eq!(page1.len(), 10);
        assert_eq!(page1.first().map(|r| r.id), Some(0));
        assert_eq!(page1.last().map(|r| r.id), Some(9));
        assert_eq!(cursor1, Some(("tenant-a".to_string(), 9)));

        let cursor1_ref = cursor1.as_ref().map(|(t, id)| (t.as_str(), *id));
        let (page2, cursor2) = storage.scan_page(cursor1_ref, 10).expect("second page");
        assert_eq!(page2.len(), 10);
        assert_eq!(page2.first().map(|r| r.id), Some(10));
        assert_eq!(page2.last().map(|r| r.id), Some(19));
        assert_eq!(cursor2, Some(("tenant-a".to_string(), 19)));

        let cursor2_ref = cursor2.as_ref().map(|(t, id)| (t.as_str(), *id));
        let (page3, cursor3) = storage
            .scan_page(cursor2_ref, 10)
            .expect("third (partial) page");
        assert_eq!(page3.len(), 5);
        assert_eq!(page3.first().map(|r| r.id), Some(20));
        assert_eq!(page3.last().map(|r| r.id), Some(24));
        // 末尾未満の件数しか返らなかったので、続きがないことを示す `None` を返す。
        assert_eq!(cursor3, None);
    }

    #[test]
    fn scan_page_clamps_limit_to_max_page_limit() {
        let path = unique_db_path("scan-page-clamp");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        storage
            .put(
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0],
                    metadata: b"m",
                },
            )
            .expect("seed row");

        // limit に MAX_SCAN_PAGE_LIMIT を超える値を渡しても panic せず、切り詰めて処理される。
        let (page, _cursor) = storage
            .scan_page(None, MAX_SCAN_PAGE_LIMIT + 1_000)
            .expect("scan with oversized limit request");
        assert_eq!(page.len(), 1);
    }

    #[test]
    fn scan_page_caps_page_by_byte_budget_even_under_row_limit() {
        let path = unique_db_path("scan-page-bytes");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // 最大メタデータ長（MAX_METADATA_LEN = 4 MiB）の行を 5 件用意する。行数上限
        // （limit=100）には遠く及ばないが、エンコード済みバイト量の合計はすぐに
        // MAX_SCAN_PAGE_BYTES（16 MiB）を超えるため、バイト上限で打ち切られるはずである。
        let big_metadata = vec![0u8; MAX_METADATA_LEN as usize];
        let rows: Vec<(u64, RowInput<'_>)> = (0..5u64)
            .map(|i| {
                (
                    i,
                    RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &[],
                        metadata: &big_metadata,
                    },
                )
            })
            .collect();
        storage.put_batch(&rows).expect("seed large rows");

        let (page1, cursor1) = storage
            .scan_page(None, 100)
            .expect("first page capped by byte budget");
        assert!(
            page1.len() < 5,
            "byte budget must cap the page before the row limit (100) is reached, got {} rows",
            page1.len()
        );
        assert!(
            !page1.is_empty(),
            "at least one row must be returned even under a tight byte budget"
        );
        assert!(
            cursor1.is_some(),
            "a capped page must report a continuation cursor"
        );

        // 続きのページを取得し、最終的に全 5 行を過不足なく読み切れること。
        let mut all_ids: Vec<u64> = page1.iter().map(|r| r.id).collect();
        let mut cursor = cursor1;
        loop {
            let cursor_ref = cursor.as_ref().map(|(t, id)| (t.as_str(), *id));
            let (page, next_cursor) = storage
                .scan_page(cursor_ref, 100)
                .expect("subsequent page after byte-budget cap");
            all_ids.extend(page.iter().map(|r| r.id));
            if next_cursor.is_none() {
                break;
            }
            cursor = next_cursor;
        }
        all_ids.sort_unstable();
        assert_eq!(all_ids, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn scan_page_with_zero_limit_returns_empty_page() {
        let path = unique_db_path("scan-page-zero");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");
        let (page, cursor) = storage.scan_page(None, 0).expect("scan with zero limit");
        assert!(page.is_empty());
        assert_eq!(cursor, None);
    }

    // 対象ビヘイビア: TABLE-12（ポインタ: `docs/spec/04-behavior/data-model.md`
    // TABLE-12）。旧フォーマット（物理キーが `id` 単独）の DB を [`ROWS_TABLE`]
    // （複合キー `(tenant_id, id)`）で開こうとする全経路が `IncompatibleRowKeyFormat`
    // で fail-closed に拒否し、DB 内容を一切変更しないことを固定する（Issue #206）。
    #[test]
    fn rows_table_rejects_legacy_id_only_key_format() {
        let path = unique_db_path("legacy-key-format");
        let _cleanup = CleanupGuard(path.clone());

        // raw redb で旧フォーマット（`id` 単独キー）の `rows` テーブルを作り、
        // 1 行だけ書き込んでおく（内容が不変であることの検証対象）。
        const LEGACY_ROWS_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("rows");
        {
            let db = redb::Database::create(&path).expect("create legacy database");
            let write_txn = db.begin_write().expect("begin write txn");
            {
                let mut table = write_txn
                    .open_table(LEGACY_ROWS_TABLE)
                    .expect("open legacy table");
                table.insert(1u64, &b"legacy-row"[..]).expect("insert row");
            }
            write_txn.commit().expect("commit legacy row");
        }

        let storage = Storage::open(&path).expect("open storage over legacy db");

        let row = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[1.0],
            metadata: &[],
        };

        // 読み取り経路。
        assert!(matches!(
            storage.get("tenant-a", 1),
            Err(StorageError::IncompatibleRowKeyFormat)
        ));
        assert!(matches!(
            storage.scan(),
            Err(StorageError::IncompatibleRowKeyFormat)
        ));
        assert!(matches!(
            storage.scan_page(None, 10),
            Err(StorageError::IncompatibleRowKeyFormat)
        ));

        // 書き込み経路（`Storage::put`/`put_batch`）。
        assert!(matches!(
            storage.put(2, &row),
            Err(StorageError::IncompatibleRowKeyFormat)
        ));
        assert!(matches!(
            storage.put_batch(&[(2, row)]),
            Err(StorageError::IncompatibleRowKeyFormat)
        ));

        // `txn` 経由（`ReadSnapshot::get`・`WriteTxn::put`・`BatchWriteTxn::put`）。
        let snapshot = storage.begin_read().expect("begin_read");
        assert!(matches!(
            snapshot.get("tenant-a", 1),
            Err(StorageError::IncompatibleRowKeyFormat)
        ));

        let row_for_write_txn = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[1.0],
            metadata: &[],
        };
        let mut write_txn = storage.begin_write().expect("begin_write");
        assert!(matches!(
            write_txn.put(2, &row_for_write_txn),
            Err(StorageError::IncompatibleRowKeyFormat)
        ));
        // `redb::Database::begin_write` の排他ロックは 1 本のみ同時に持てるため、
        // 次の `begin_batch_write` の前に必ず手放す（保持したままだと後続の
        // `begin_write` 呼び出しがデッドロックする）。
        write_txn.abort().expect("abort write_txn");

        let row_for_batch_txn = RowInput {
            tenant_id: "tenant-a",
            visibility: Visibility::Public,
            embedding: &[1.0],
            metadata: &[],
        };
        let mut batch_txn = storage.begin_batch_write().expect("begin_batch_write");
        assert!(matches!(
            batch_txn.put(2, &row_for_batch_txn),
            Err(StorageError::IncompatibleRowKeyFormat)
        ));
        batch_txn.abort().expect("abort batch_txn");

        // `Storage` を drop してファイルロックを解放してから raw redb で再オープンする
        // （`tests/persistence.rs` と同じ制約）。
        drop(storage);

        // `Display` にテナント ID・テーブル名を含めない（security.md「情報漏えい」対応）。
        let message = StorageError::IncompatibleRowKeyFormat.to_string();
        assert_eq!(
            message,
            "incompatible row store key format: rebuild required"
        );
        assert!(!message.contains("tenant-a"));
        assert!(!message.contains("rows"));

        // 旧行の内容が変更されていないことを raw redb で確認する。
        let db = redb::Database::open(&path).expect("reopen legacy database raw");
        let read_txn = db.begin_read().expect("begin_read raw");
        let table = read_txn
            .open_table(LEGACY_ROWS_TABLE)
            .expect("open legacy table after failed writes");
        let guard = table
            .get(1u64)
            .expect("get legacy row")
            .expect("row exists");
        assert_eq!(guard.value(), b"legacy-row");
    }

    // 対象ビヘイビア: TABLE-12。異なるテナントは同一 `id` を独立に保持できる
    // （物理キーが `(tenant_id, id)` の複合キーのため）。
    #[test]
    fn same_id_across_tenants_are_independent_rows() {
        let path = unique_db_path("same-id-cross-tenant");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        storage
            .put(
                1,
                &RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0],
                    metadata: b"a",
                },
            )
            .expect("put tenant-a row 1");
        storage
            .put(
                1,
                &RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Private,
                    embedding: &[2.0],
                    metadata: b"b",
                },
            )
            .expect("put tenant-b row 1");

        let row_a = storage.get("tenant-a", 1).expect("get tenant-a row 1");
        assert_eq!(row_a.metadata, b"a");
        let row_b = storage.get("tenant-b", 1).expect("get tenant-b row 1");
        assert_eq!(row_b.metadata, b"b");

        let mut scanned = storage.scan().expect("scan");
        scanned.sort_by(|x, y| x.tenant_id.cmp(&y.tenant_id));
        assert_eq!(scanned.len(), 2);
        assert_eq!(
            scanned
                .iter()
                .map(|r| (r.tenant_id.as_str(), r.id))
                .collect::<Vec<_>>(),
            vec![("tenant-a", 1), ("tenant-b", 1)]
        );

        let (page, cursor) = storage.scan_page(None, 100).expect("scan_page");
        assert_eq!(page.len(), 2);
        assert_eq!(cursor, None);
    }

    // 対象ビヘイビア: TABLE-12。`(tenant_id, id)` 順のページングがテナント境界を
    // 跨いでも取りこぼし・重複なく全件列挙できることを固定する。
    #[test]
    fn scan_page_cursor_resumes_across_tenant_boundary() {
        let path = unique_db_path("scan-page-cross-tenant");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        let rows: Vec<(u64, RowInput<'_>)> = vec![
            (
                1,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[1.0],
                    metadata: &[],
                },
            ),
            (
                2,
                RowInput {
                    tenant_id: "tenant-a",
                    visibility: Visibility::Public,
                    embedding: &[2.0],
                    metadata: &[],
                },
            ),
            (
                1,
                RowInput {
                    tenant_id: "tenant-b",
                    visibility: Visibility::Public,
                    embedding: &[3.0],
                    metadata: &[],
                },
            ),
        ];
        storage.put_batch(&rows).expect("seed rows");

        let mut all: Vec<(String, u64)> = Vec::new();
        let mut cursor: Option<RowCursor> = None;
        loop {
            let cursor_ref = cursor.as_ref().map(|(t, id)| (t.as_str(), *id));
            let (page, next_cursor) = storage
                .scan_page(cursor_ref, 1)
                .expect("scan_page with limit=1");
            all.extend(page.iter().map(|r| (r.tenant_id.clone(), r.id)));
            match next_cursor {
                None => break,
                Some(next) => cursor = Some(next),
            }
        }

        all.sort();
        assert_eq!(
            all,
            vec![
                ("tenant-a".to_string(), 1),
                ("tenant-a".to_string(), 2),
                ("tenant-b".to_string(), 1),
            ]
        );
    }

    // Issue #131 / PR #193 codex レビュー対応: `Display` の固定文言
    // "scan limit exceeded: use scan_page" は既存利用者が完全一致・前方一致等で
    // 参照しうる観測可能な互換性契約であり、告知なく削除・改変しない（末尾への
    // 補足追記も含む。本 variant のドキュメンテーションコメント参照）。経路別の
    // 正確な代替手段案内は呼び出し元（`catalog.rs::convert_storage_error`・
    // `examples/crash_tool_cross_table.rs` 等）が `Display` を使わず別途生成する。

    #[test]
    fn scan_limit_error_message_matches_legacy_string_exactly() {
        let message = StorageError::ScanLimitExceeded.to_string();
        // 互換性契約: 旧文字列と完全に同一（バイト一致）であること。
        assert_eq!(message, "scan limit exceeded: use scan_page");
    }

    #[test]
    fn check_scan_limits_rejects_row_and_byte_overrun() {
        // 行数境界: MAX_SCAN_TOTAL_ROWS 件目を追加しようとすると拒否される。
        assert!(matches!(
            check_scan_limits(MAX_SCAN_TOTAL_ROWS, 0, 1),
            Err(StorageError::ScanLimitExceeded)
        ));
        assert!(check_scan_limits(MAX_SCAN_TOTAL_ROWS - 1, 0, 1).is_ok());

        // バイト境界: 追加後に MAX_SCAN_TOTAL_BYTES を超えると拒否される。
        assert!(matches!(
            check_scan_limits(0, MAX_SCAN_TOTAL_BYTES, 1),
            Err(StorageError::ScanLimitExceeded)
        ));
        assert!(check_scan_limits(0, MAX_SCAN_TOTAL_BYTES, 0).is_ok());
    }

    #[test]
    fn check_batch_log_limit_rejects_overrun() {
        assert!(matches!(
            check_batch_log_limit(MAX_BATCH_LOG_ROWS),
            Err(StorageError::ScanLimitExceeded)
        ));
        assert!(check_batch_log_limit(MAX_BATCH_LOG_ROWS - 1).is_ok());
    }

    #[test]
    fn scan_limit_errors_have_no_source() {
        use std::error::Error;
        assert!(StorageError::ScanLimitExceeded.source().is_none());
    }

    // 対象ビヘイビア: TABLE-10。台帳テーブル未作成（DB 作成直後で一度も log_batch
    // していない）状態では `None` を返す（fail-closed だが「未作成 = 0 件」を過不足なく
    // 表現する契約。scan_batch_log の空 Vec 返却と同じ扱い）。
    #[test]
    fn batch_log_max_seq_returns_none_when_table_does_not_exist() {
        let path = unique_db_path("batch-log-max-seq-no-table");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        assert_eq!(
            storage.batch_log_max_seq().expect("batch_log_max_seq"),
            None
        );
    }

    // 対象ビヘイビア: TABLE-10。挿入順ではなくキー最大値を返すこと（redb の B-tree
    // 末尾キー取得であり、挿入順に依存する実装への退行を検知する）。
    #[test]
    fn batch_log_max_seq_returns_max_key_not_insertion_order() {
        let path = unique_db_path("batch-log-max-seq-max-key");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        // `Storage` の private フィールド（`db`）は同一モジュール内なので直接触れる。
        // 公開 API は log_batch 経由（BatchWriteTxn）のみだが、ここでは
        // batch_log_max_seq 単体の「キー最大値」契約だけを最小構成で検証したいため、
        // 台帳テーブルへ非連続キーを直接書き込む。
        {
            let write_txn = storage.db.begin_write().expect("begin_write");
            {
                let mut table = write_txn
                    .open_table(BATCH_LOG_TABLE)
                    .expect("open batch log table");
                table.insert(3u64, 10u64).expect("insert seq 3");
                table.insert(0u64, 10u64).expect("insert seq 0");
                table.insert(7u64, 10u64).expect("insert seq 7");
            }
            write_txn.commit().expect("commit");
        }

        assert_eq!(
            storage.batch_log_max_seq().expect("batch_log_max_seq"),
            Some(7)
        );
    }

    // 対象ビヘイビア: TABLE-10。境界値 `u64::MAX` を含む場合もそのまま返すこと
    // （呼び出し元の `checked_add` による fail-closed 拒否は呼び出し元の責務であり、
    // 本 API 自身は値の解釈をしない）。
    #[test]
    fn batch_log_max_seq_returns_u64_max_boundary() {
        let path = unique_db_path("batch-log-max-seq-u64-max");
        let _cleanup = CleanupGuard(path.clone());
        let storage = Storage::open(&path).expect("open storage");

        {
            let write_txn = storage.db.begin_write().expect("begin_write");
            {
                let mut table = write_txn
                    .open_table(BATCH_LOG_TABLE)
                    .expect("open batch log table");
                table.insert(1u64, 10u64).expect("insert seq 1");
                table.insert(u64::MAX, 10u64).expect("insert seq u64::MAX");
            }
            write_txn.commit().expect("commit");
        }

        assert_eq!(
            storage.batch_log_max_seq().expect("batch_log_max_seq"),
            Some(u64::MAX)
        );
    }

    /// 電源断シミュレーションによるクラッシュ耐性の再検証（TASK-145、ポインタ:
    /// `docs/spec/05-tasks.md`。関連ビヘイビア: PERSIST-3、ポインタ:
    /// `docs/spec/04-behavior/persistence.md`）。検証対象の契約内容は上記ポインタ先を
    /// 参照。他シナリオは
    /// `crates/engine/tests/power_loss.rs`（raw `redb::Database` を直接操作する統合テスト）
    /// を参照。
    ///
    /// 本サブモジュールを `crate` 内の `#[cfg(test)]` に閉じているのは、本番の
    /// `Storage::put`/`Storage::get`（＝実際の `encode_row`/`decode_row`）経由で検証
    /// しつつ、`Storage` の公開 API へバックエンド差し替え用のコンストラクタを一切
    /// 増やさないため。`Storage { db }` の private フィールドへは同一クレート内の
    /// 子モジュールから直接アクセスできるので、feature 限定の `pub fn` を用意せずに
    /// テスト専用の `redb::Database`（`StorageBackend` 差し替え済み）を渡せる。
    mod power_loss {
        use super::*;

        // 電源断シミュレーション用 `StorageBackend` の基本実装（`BackendState`/
        // `PowerLossBackend`）は `crates/engine/tests/power_loss.rs` と共有する
        // ため `power_loss_model`（`crate::storage::power_loss_model`）へ分離済み。
        // 詳細・分離理由は同モジュールの doc を参照。
        use crate::storage::power_loss_model::PowerLossBackend;
        use redb::StorageBackend;

        /// 新規（空）の [`PowerLossBackend`] 上に `redb::Database` を開く。
        fn open_fresh() -> (PowerLossBackend, redb::Database) {
            let backend = PowerLossBackend::new();
            let db = redb::Builder::new()
                .create_with_backend(backend.clone())
                .expect("open fresh database on PowerLossBackend");
            (backend, db)
        }

        /// 指定バイト列を初期像として `redb::Database` を開き直す（「電源断後の再起動」に
        /// 相当）。破損像で開けない場合は `Err` をそのまま返す。
        fn reopen_from_image(
            image: Vec<u8>,
        ) -> std::result::Result<redb::Database, redb::DatabaseError> {
            redb::Builder::new().create_with_backend(PowerLossBackend::from_bytes(image))
        }

        // シナリオ 4（対応する契約: PERSIST-3。ポインタ: `docs/spec/04-behavior/persistence.md`）。
        // `PowerLossBackend` で差し替えた `redb::Database` を、本モジュール（`storage` の
        // 子孫）の特権で `Storage { db }` へ直接渡し（公開 API のバイパス用コンストラクタは
        // 追加しない）、書き込み（`Storage::put`）・読み出し（`Storage::get`）ともに本番の
        // `encode_row`/`decode_row` を経由させる。電源断前後どちらも `Storage::get` で
        // 読み出して同じ内容が復元されることを確認する。
        #[test]
        fn power_loss_scenario4_rls_fields_survive_crash_after_commit() {
            let (backend, raw_db) = open_fresh();
            let storage = Storage {
                db: raw_db,
                durability: WriteDurability::default(),
                write_txn_creations: std::sync::atomic::AtomicU64::new(0),
                writer_gate: writer_gate::WriterGate::new(),
                partitioned_jobs: Default::default(),
                write_lock_wait: DEFAULT_WRITE_LOCK_WAIT,
            };

            // embedding/metadata は空スライスにしない。空だと encoder/decoder が
            // これらのフィールドを常に欠落させる退行があっても電源断前後の比較が
            // 偶然一致してしまい、「全フィールド保持」を検証したことにならない。
            let row1_embedding = [0.5_f32, -1.0, 2.25];
            let row1_metadata = b"row1-metadata".to_vec();
            let row2_embedding = [3.0_f32, 0.0, -4.5, 1.25];
            let row2_metadata = b"row2-metadata".to_vec();

            storage
                .put(
                    1,
                    &RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &row1_embedding,
                        metadata: &row1_metadata,
                    },
                )
                .expect("put row 1 via production Storage API");
            storage
                .put(
                    2,
                    &RowInput {
                        tenant_id: "tenant-b",
                        visibility: Visibility::Private,
                        embedding: &row2_embedding,
                        metadata: &row2_metadata,
                    },
                )
                .expect("put row 2 via production Storage API");

            // `Storage::put` の commit が実際に `sync_data()` まで完了していることを
            // 確認してから `durable_snapshot()` を電源断像として使う。ここを確認しないと、
            // 仮に `PowerLossBackend::write` が sync を待たず durable へ書き戻す実装（本来は
            // 契約違反）へ退行しても本テストが検出できなくなる。
            assert_eq!(
                backend.pending_write_count(),
                0,
                "Storage::put の commit 完了後は log が sync 済みで空になっているはず \
                 （0 件でなければ、まだ sync されていない書き込みを電源断像に含めてしまう）"
            );

            let row1_before = storage
                .get("tenant-a", 1)
                .expect("decode row 1 via production Storage API before crash");
            let row2_before = storage
                .get("tenant-b", 2)
                .expect("decode row 2 via production Storage API before crash");

            let crash_image = backend.durable_snapshot();
            drop(storage);

            let recovered_raw_db =
                reopen_from_image(crash_image).expect("reopen after crash must succeed");
            let recovered_storage = Storage {
                db: recovered_raw_db,
                durability: WriteDurability::default(),
                write_txn_creations: std::sync::atomic::AtomicU64::new(0),
                writer_gate: writer_gate::WriterGate::new(),
                partitioned_jobs: Default::default(),
                write_lock_wait: DEFAULT_WRITE_LOCK_WAIT,
            };

            let row1_after = recovered_storage
                .get("tenant-a", 1)
                .expect("decode row 1 via production Storage API after crash");
            assert_eq!(
                row1_after, row1_before,
                "row 1 は電源断前後で production decode_row の結果が完全一致していなければならない"
            );
            assert_eq!(row1_after.tenant_id, "tenant-a");
            assert_eq!(row1_after.visibility, Visibility::Public);
            assert_eq!(
                row1_after.embedding, row1_embedding,
                "row 1 の embedding フィールドが電源断後も欠落・変質なく保持されているはず"
            );
            assert_eq!(
                row1_after.metadata, row1_metadata,
                "row 1 の metadata フィールドが電源断後も欠落・変質なく保持されているはず"
            );

            let row2_after = recovered_storage
                .get("tenant-b", 2)
                .expect("decode row 2 via production Storage API after crash");
            assert_eq!(
                row2_after, row2_before,
                "row 2 は電源断前後で production decode_row の結果が完全一致していなければならない"
            );
            assert_eq!(row2_after.tenant_id, "tenant-b");
            assert_eq!(row2_after.visibility, Visibility::Private);
            assert_eq!(
                row2_after.embedding, row2_embedding,
                "row 2 の embedding フィールドが電源断後も欠落・変質なく保持されているはず"
            );
            assert_eq!(
                row2_after.metadata, row2_metadata,
                "row 2 の metadata フィールドが電源断後も欠落・変質なく保持されているはず"
            );
        }

        // シナリオ 5（Issue #849。`WriteDurability::None` opt-in の損失ウィンドウを
        // シナリオ 4 と対にして固定する）。`Storage { durability: WriteDurability::None }`
        // を本モジュールの特権で直接構築し、commit 成功応答を受け取った直後の行を
        // 読み出せること（read-your-writes は維持される）を確認したうえで、
        // `sync_data()` を一度も呼ばずに取った `durable_snapshot()`（＝電源断で必ず
        // 残る像）から再オープンすると、その行が失われることを確認する。シナリオ 4
        // （`WriteDurability::Immediate` は同じ手順で行が残る）との対比により、
        // `Storage::begin_write_txn` の `set_durability` 分岐が実際に効いていることを
        // 非 vacuous に固定する。`docs/design/ingest-write-path.md`「Issue #849 追記」節の
        // 損失ウィンドウの記述に対応する回帰固定。
        #[test]
        fn power_loss_scenario5_none_durability_commit_can_be_lost_before_sync() {
            let (backend, raw_db) = open_fresh();
            let storage = Storage {
                db: raw_db,
                durability: WriteDurability::None,
                write_txn_creations: std::sync::atomic::AtomicU64::new(0),
                writer_gate: writer_gate::WriterGate::new(),
                partitioned_jobs: Default::default(),
                write_lock_wait: DEFAULT_WRITE_LOCK_WAIT,
            };

            let embedding = [1.5_f32, -2.0, 0.25];
            let metadata = b"scenario5-metadata".to_vec();

            storage
                .put(
                    1,
                    &RowInput {
                        tenant_id: "tenant-a",
                        visibility: Visibility::Public,
                        embedding: &embedding,
                        metadata: &metadata,
                    },
                )
                .expect("put via production Storage API with WriteDurability::None");

            // commit 成功応答を受け取った直後の行が、この時点で読み出せることを
            // 確認する（read-your-writes は `redb` の契約として維持される）。
            let row_before_crash = storage
                .get("tenant-a", 1)
                .expect("decode row via production Storage API before crash");
            assert_eq!(row_before_crash.embedding, embedding);

            // 電源断像は「直近 sync_data() 時点」のみを反映する（`durable_snapshot()`
            // のドキュメンテーションコメント参照）。本テストでは一度も `sync_data()` を
            // 呼んでいないため、`None` durability の commit 内容は一切含まれない。
            let crash_image = backend.durable_snapshot();
            drop(storage);

            let recovered_raw_db =
                reopen_from_image(crash_image).expect("reopen after crash must succeed");
            let recovered_storage = Storage {
                db: recovered_raw_db,
                durability: WriteDurability::default(),
                write_txn_creations: std::sync::atomic::AtomicU64::new(0),
                writer_gate: writer_gate::WriterGate::new(),
                partitioned_jobs: Default::default(),
                write_lock_wait: DEFAULT_WRITE_LOCK_WAIT,
            };

            // commit 成功応答を受け取ったはずの行が、電源断後は失われている
            // （`docs/design/ingest-write-path.md`「Issue #849 追記」節の損失ウィンドウ）。
            let after = recovered_storage.get("tenant-a", 1);
            assert!(
                matches!(after, Err(StorageError::NotFound(1))),
                "WriteDurability::None の commit は sync 前の電源断で失われ得る \
                 （fail-closed に NotFound を返すべきで、破損データを黙って返しては \
                 ならない）: {after:?}"
            );
        }

        // 否定コントロール: `sync_data()` を呼ばない限り `write()` の内容は
        // `durable_snapshot()`（＝電源断で必ず残る像）へ反映されないことを直接確認する。
        // これがないと、`PowerLossBackend::write` が sync を待たず即座に `durable` を
        // 書き換える実装（commit/sync 契約違反）へ退行しても、シナリオ 4 は偶然
        // 成功し続けてしまう（恒真化）。`StorageBackend` トレイトのメソッドを
        // `redb::Database` を介さず直接呼び、モデルの sync ゲーティングだけを検証する。
        #[test]
        fn power_loss_backend_durable_image_excludes_writes_until_sync_data() {
            let backend = PowerLossBackend::new();

            backend
                .write(0, b"uncommitted-write")
                .expect("write to backend");
            assert_eq!(
                backend.pending_write_count(),
                1,
                "write() は log に記録されているはず（0 件だと本テストは何も検証していない）"
            );
            assert!(
                backend.durable_snapshot().is_empty(),
                "sync_data() を呼ぶ前は、write() の内容が durable_snapshot()（電源断で \
                 必ず残る像）へ反映されていてはならない"
            );

            backend.sync_data().expect("sync_data");
            assert_eq!(
                backend.pending_write_count(),
                0,
                "sync_data() 後は log が空になっているはず"
            );
            assert_eq!(
                backend.durable_snapshot(),
                b"uncommitted-write",
                "sync_data() を呼んだ後は、write() の内容が durable_snapshot() へ \
                 反映されていなければならない"
            );
        }

        // 退行検出用: `write()` が現在の EOF を越える offset へ書き込んだ場合に
        // `len()` が追従して伸長することを直接確認する。ここを確認しないと、
        // `write()` が `log` へ積むだけで `state.len` を更新しない実装（EOF 越え
        // write を無視する契約違反）へ退行しても検出できない。`len()` が古いままだと、
        // `sync_data()` 後の `durable_snapshot()` の実長が `len()` の申告値を超え、
        // `redb` から見た「ファイル長」と実データが矛盾する。
        #[test]
        fn power_loss_backend_write_past_eof_extends_len() {
            let backend = PowerLossBackend::from_bytes(b"short".to_vec());
            assert_eq!(backend.len().expect("len before write"), 5);

            // 既存の 5 バイトより後ろ（offset 10）へ書き込み、EOF を越えて伸長させる。
            backend.write(10, b"tail").expect("write past current EOF");
            assert_eq!(
                backend.len().expect("len after EOF-crossing write"),
                14,
                "write() が EOF を越えた分だけ len() も伸長しているはず"
            );

            backend.sync_data().expect("sync_data");
            let durable = backend.durable_snapshot();
            assert_eq!(
                durable.len(),
                backend.len().expect("len after sync_data") as usize,
                "sync_data() 後は durable_snapshot() の実長が len() の申告値と \
                 一致していなければならない（不一致は EOF 越え write で len が \
                 追従しない退行の兆候）"
            );
        }
    }

    /// 別スレッドで `f` を実行し、`bound` 以内に結果が返らなければ `None` を返す
    /// （書き込みゲートの回帰テスト用。無期限待ちの退行でテスト自体が止まらない
    /// ように、呼び出し元は `None` を観測したら保持中のトランザクションを解放
    /// してから失敗させる）。
    fn run_with_bound<T: Send + 'static>(
        storage: &std::sync::Arc<Storage>,
        bound: std::time::Duration,
        f: impl FnOnce(&Storage) -> T + Send + 'static,
    ) -> (Option<T>, std::thread::JoinHandle<()>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let storage = std::sync::Arc::clone(storage);
        let handle = std::thread::spawn(move || {
            let _ = tx.send(f(&storage));
        });
        (rx.recv_timeout(bound).ok(), handle)
    }

    /// autocommit の書き込みトランザクションが `redb` のライタを保持している間、
    /// 別セッションの明示トランザクション `BEGIN`・autocommit 書き込みはいずれも
    /// `redb::Database::begin_write` の中で無期限に待たず、ゲートの待機上限で
    /// `WriteLockTimeout`（`55P03`）になる（PR #1041 レビュー指摘の回帰テスト）。
    #[test]
    fn writer_gate_is_held_for_the_whole_autocommit_write_txn_lifetime() {
        let path = unique_db_path("writer-gate-autocommit-lifetime");
        let _cleanup = CleanupGuard(path.clone());
        let wait = std::time::Duration::from_millis(100);
        let storage = std::sync::Arc::new(
            Storage::open(&path)
                .expect("open storage")
                .with_write_lock_wait(wait),
        );
        let bound = wait * 20;

        let held = storage.begin_write_txn().expect("autocommit write txn");

        let (explicit, explicit_handle) = run_with_bound(&storage, bound, |s| {
            s.begin_explicit_write_txn().map(|_| ())
        });
        let (autocommit, autocommit_handle) =
            run_with_bound(&storage, bound, |s| s.begin_write_txn().map(|_| ()));

        // 退行時に別スレッドが `redb` の中で待ち続けていても join できるよう、
        // 判定より先に保持中のトランザクションを解放する。
        drop(held);
        explicit_handle.join().expect("explicit thread");
        autocommit_handle.join().expect("autocommit thread");

        assert!(
            matches!(explicit, Some(Err(StorageError::WriteLockTimeout))),
            "explicit BEGIN must time out with WriteLockTimeout, got {explicit:?}"
        );
        assert!(
            matches!(autocommit, Some(Err(StorageError::WriteLockTimeout))),
            "autocommit write must time out with WriteLockTimeout, got {autocommit:?}"
        );

        // 解放後は取得できる。
        let txn = storage.begin_write_txn().expect("write txn after release");
        txn.abort().expect("abort");
    }
}
