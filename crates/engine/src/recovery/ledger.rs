//! テーブル単位 `operation_id` 台帳（TASK-93、対象ビヘイビア: RECOVER-2。ポインタ:
//! `docs/spec/05-tasks.md` TASK-93・`docs/spec/04-behavior/recovery.md` RECOVER-2・
//! `docs/spec/04-behavior/error-format.md` ERR-2・`docs/spec/04-behavior/sql-surface.md`
//! SQL-10）。
//!
//! `recovery::required_op_id`（TASK-92・RECOVER-1）が「`operation_id` を必須とするか」
//! を判定するのに対し、本モジュールは検証済みの [`crate::recovery::required_op_id::OperationId`]
//! を**テナント内・テーブル単位**で永続化する側を担う。台帳への追記は行の書き込み・
//! 更新・削除と**同一の `redb::WriteTransaction` 内**で行い、commit で両方が反映され、
//! 失敗（トランザクション drop）で両方が破棄される（原子性）。呼び出し元は
//! `crate::tenant::*_unchecked`（テナント境界付き書き込みガード API の内部実体）で、
//! `record_in_txn` を「スキーマ取得 → 台帳追記 → 行書き込み/削除 → commit」の順に
//! 呼ぶ（`tenant.rs` モジュールドキュメント参照）。
//!
//! 内容照合ハッシュ（TASK-101、対象ビヘイビア: RECOVER-10。[`crate::recovery::content_hash`]
//! 参照）を台帳値フォーマット v2 として追加済み。同一 `operation_id`・同一テーブルへの
//! 再送時に、ハッシュが一致すれば「同一内容の再送」（呼び出し元が `23505` へ写像。
//! TASK-94・RECOVER-3 の重複拒否契約を包含する）、不一致であれば「内容の異なる誤用」
//! （呼び出し元が `22023` へ写像）を [`LedgerRecordError`] で返す。並行書き込みの
//! 原子性は呼び出し元が同一 `redb::WriteTransaction` 内で本関数を呼ぶことで担保する
//! （本モジュールは「既存エントリを上書きしない（keep-first）」ことを引き続き
//! 恒久契約として担保する）。
//!
//! TASK-98（対象ビヘイビア: RECOVER-7）で [`LAST_OP_TABLE`] を追加した（ポインタ:
//! `docs/spec/05-tasks.md` TASK-98・`docs/spec/04-behavior/recovery.md` RECOVER-7。
//! 契約の詳細は spec 参照）。

use std::ops::Bound;

use redb::{ReadableTable, TableDefinition, TableHandle};

use crate::recovery::content_hash::ContentHash;
use crate::recovery::required_op_id::OperationId;
use crate::storage::StorageError;

/// `operation_id` 台帳テーブル（TASK-93、対象ビヘイビア: RECOVER-2）。
///
/// キーは `(tenant_id, table_name, operation_id)` の複合キー。`tenant_id` を先頭に
/// 置くことで、将来のテナント名前空間 range 走査（TASK-98・RECOVER-7）が単一 range
/// で完結する。
///
/// - `tenant_id` は**必ずサーバー側導出**（[`crate::policy::PolicyContext::tenant_id`]）
///   を使う。クライアント自己申告の `RowInput::tenant_id` はキーに使わない
///   （TABLE-12・RLS-9 と同じ原則。security.md P0）。
/// - `table_name` は [`crate::catalog::validate_identifier`] 通過済みの論理名
///   （`user_rows/` プレフィックスなし）。
/// - `operation_id` は検証済み [`OperationId`]（TASK-80。長さ上限 256・制御文字排除済み）。
///
/// 値は v1（バージョンバイトのみ）・v2（バージョンバイト＋32 バイト内容ハッシュ。
/// TASK-101・RECOVER-10）のいずれか。未知バージョンの値は fail-closed に拒否する
/// （[`decode_entry`]）。TASK-98（RECOVER-7）は本テーブルの意味・フォーマットを
/// 変えず、別テーブル [`LAST_OP_TABLE`] を追加する形で拡張した。
///
/// カタログ（`Storage::list_tables` 等）はユーザーテーブルのみを列挙する既存設計のため、
/// 本テーブル名 `op_ledger` はユーザーから見えるテーブル一覧に混入しない。
pub(crate) const OP_LEDGER_TABLE: TableDefinition<(&str, &str, &str), &[u8]> =
    TableDefinition::new("op_ledger");

/// [`OP_LEDGER_TABLE`] の値フォーマットバージョン v1（バージョンバイトのみ。内容ハッシュ
/// なし）。TASK-101 以前に書かれた既存エントリ、または `decode_entry` が
/// `StoredEntry::V1` を返す形式。
const LEDGER_ENTRY_FORMAT_VERSION_V1: u8 = 1;

/// [`OP_LEDGER_TABLE`] の値フォーマットバージョン v2（バージョンバイト＋32 バイト
/// 内容ハッシュ。TASK-101・RECOVER-10）。
const LEDGER_ENTRY_FORMAT_VERSION_V2: u8 = 2;

/// v2 台帳エントリの符号化。バージョンバイト＋[`ContentHash`] の生バイト列。
fn encode_entry_v2(hash: &ContentHash) -> Vec<u8> {
    let mut buf = Vec::with_capacity(1 + 32);
    buf.push(LEDGER_ENTRY_FORMAT_VERSION_V2);
    buf.extend_from_slice(hash.as_bytes());
    buf
}

/// 最終 commit 済み `operation_id` 照会テーブル（TASK-98、対象ビヘイビア:
/// RECOVER-7）。
///
/// キーは `(tenant_id, table_name)`。
///
/// - `tenant_id` は [`OP_LEDGER_TABLE`] と同じくサーバー側導出のみを使う
///   （security.md P0・TABLE-12・RLS-9）。
/// - カタログ列挙はユーザーテーブルのみを返すため、本テーブル名 `last_op` も
///   `op_ledger` と同様にユーザーから見えるテーブル一覧に混入しない。
pub(crate) const LAST_OP_TABLE: TableDefinition<(&str, &str), &[u8]> =
    TableDefinition::new("last_op");

/// [`LAST_OP_TABLE`] の値フォーマットバージョン v1（バージョンバイト＋`operation_id`
/// の UTF-8 バイト列）。[`OP_LEDGER_TABLE`] と同じ「バージョン付き値・未知バージョンは
/// fail-closed 拒否」方針を踏襲する。将来の拡張はバージョン繰り上げで対応する。
const LAST_OP_FORMAT_VERSION_V1: u8 = 1;

/// [`LAST_OP_TABLE`] 値の符号化。バージョンバイト＋`operation_id` の UTF-8 バイト列。
fn encode_last_op(op_id: &OperationId) -> Vec<u8> {
    let bytes = op_id.as_str().as_bytes();
    let mut buf = Vec::with_capacity(1 + bytes.len());
    buf.push(LAST_OP_FORMAT_VERSION_V1);
    buf.extend_from_slice(bytes);
    buf
}

/// [`decode_last_op`] の拒否理由（fail-closed の判定結果を運用者向け診断に残すための
/// 区分。テナント・テーブル名・値の内容は一切保持しない・Low・codex-review 指摘対応）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LastOpDecodeError {
    /// バージョンバイトが未知（空値・[`LAST_OP_FORMAT_VERSION_V1`] 以外）。
    UnknownVersion,
    /// バージョンバイト以降が不正 UTF-8。
    InvalidUtf8,
    /// UTF-8 としては妥当だが [`OperationId::parse`] が拒否する値。
    InvalidOperationId,
}

/// [`LAST_OP_TABLE`] 値のデコード。空値・未知バージョン・不正 UTF-8・
/// [`OperationId::parse`] が拒否する値はいずれも fail-closed に拒否する
/// （[`OP_LEDGER_TABLE`] 側の [`decode_entry`] と同方針）。拒否理由は
/// [`LastOpDecodeError`] で区別し、呼び出し元（[`last_operation_in_read_txn`]）が
/// 診断メッセージへ反映する。
fn decode_last_op(value: &[u8]) -> Result<OperationId, LastOpDecodeError> {
    match value.split_first() {
        Some((&LAST_OP_FORMAT_VERSION_V1, rest)) => {
            let raw = std::str::from_utf8(rest).map_err(|_| LastOpDecodeError::InvalidUtf8)?;
            OperationId::parse(raw).map_err(|_| LastOpDecodeError::InvalidOperationId)
        }
        _ => Err(LastOpDecodeError::UnknownVersion),
    }
}

/// [`decode_entry`] の返値。`ledger.rs`（本ファイル）内でのみ使う中間表現。
enum StoredEntry {
    /// v1（内容ハッシュを保持しない旧フォーマット）。
    V1,
    /// v2（内容ハッシュを保持する現行フォーマット）。
    V2 { hash: [u8; 32] },
}

/// [`delete_table_in_txn`] が 1 回の走査・削除で `keys_to_remove` に保持するキー数の
/// 上限（Issue #226 レビュー対応・codex-review 指摘）。長期利用テーブルの
/// `DROP TABLE` でも台帳サイズに比例した無制限メモリを一度に要求しないよう、
/// 対象キーが尽きるまでこの件数単位で繰り返し走査・削除する。
const DELETE_BATCH_SIZE: usize = 1024;

/// 未知の台帳値フォーマットを検出したときのエラー（fail-closed）。テナント・テーブル・
/// `operation_id` を含まない固定文言のみを返す（security.md P0）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct UnknownLedgerEntryFormat;

impl std::fmt::Display for UnknownLedgerEntryFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown operation_id ledger entry format")
    }
}

impl std::error::Error for UnknownLedgerEntryFormat {}

/// 台帳値のデコード。空値・未知バージョン・v2 のハッシュ長不一致はいずれも
/// fail-closed に拒否する（`storage.rs` の行フォーマット検証と同方針）。
fn decode_entry(value: &[u8]) -> Result<StoredEntry, UnknownLedgerEntryFormat> {
    match value.split_first() {
        Some((&LEDGER_ENTRY_FORMAT_VERSION_V1, [])) => Ok(StoredEntry::V1),
        Some((&LEDGER_ENTRY_FORMAT_VERSION_V2, rest)) => {
            let hash: [u8; 32] = rest.try_into().map_err(|_| UnknownLedgerEntryFormat)?;
            Ok(StoredEntry::V2 { hash })
        }
        _ => Err(UnknownLedgerEntryFormat),
    }
}

/// 「台帳へ書くか」を型で明示する（[`crate::recovery::required_op_id::LedgerMode::resolve`]
/// が生成する）。`Option` の `None` を黙って skip 扱いにしない（呼び出し元に判断を
/// 委ねず、`Disabled` という意図を型で残す）。
pub(crate) enum LedgerWrite<'a> {
    /// 台帳あり構成（`LedgerMode::Ledgered`）。検証済み `operation_id` を記録する。
    Record(&'a OperationId),
    /// 台帳を持たない構成（`LedgerMode::CompareOnlyWithoutLedger`）。台帳テーブルへは
    /// 一切触れない（テーブル自体も作らない）。
    Disabled,
}

/// [`record_in_txn`] の成功時の記録結果。既存キーの検出（重複・内容不一致）は
/// [`LedgerRecordError`] 側の variant として返す（TASK-101・RECOVER-10 対応で
/// `AlreadyPresent` を廃止し、呼び出し元が `?` で自然に `TenantWriteError` へ
/// 写像できる形へ整理した）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecordOutcome {
    /// 新規記録した。
    Recorded,
    /// [`LedgerWrite::Disabled`] のため台帳へ触れなかった。
    Skipped,
}

/// [`record_in_txn`] のエラー型（TASK-101、対象ビヘイビア: RECOVER-10）。
/// `crate::tenant::*_unchecked` が `?`／`match` で `TenantWriteError` へ写像する
/// （`Duplicate` → `23505`・`ContentMismatch` → `22023`・`Corrupted` → `XX000`。
/// `tenant.rs` の `TenantWriteError` 定義参照）。
#[derive(Debug)]
pub(crate) enum LedgerRecordError {
    /// 未知の台帳値フォーマットを検出した（内部事象。テナント・行内容を含まない）。
    Corrupted(StorageError),
    /// 同一 `operation_id`・同一テーブルへ、**内容が一致する**書き込みが再送された
    /// （commit 済み確定の根拠として扱ってよい。呼び出し元は `23505` へ写像する）。
    Duplicate,
    /// 同一 `operation_id`・同一テーブルへ、**内容が異なる**書き込みが再送された、
    /// または内容一致を証明できない v1 レガシーエントリへ再送された（いずれも
    /// fail-closed に「commit 済み確定の根拠にしない」側へ倒す。呼び出し元は
    /// `22023` へ写像する）。
    ContentMismatch,
}

/// `redb` の各操作は複数のエラー型を返すが、いずれも `redb::Error` へ変換可能
/// （`storage.rs` の `StorageError` 側 blanket impl と同じ理由）。ここでは対象型が
/// 異なる（`LedgerRecordError`）ため coherence 上の衝突なく独立に定義できる。
impl<E> From<E> for LedgerRecordError
where
    E: Into<redb::Error>,
{
    fn from(e: E) -> Self {
        LedgerRecordError::Corrupted(StorageError::from(e))
    }
}

/// [`crate::core::EngineCore::operation_recorded`] の照会結果（TASK-93、対象ビヘイビア:
/// RECOVER-2）。後続 TASK-94（重複拒否）・TASK-98（二層台帳照会）の共通語彙として、
/// 本モジュールで `pub` 定義する。
///
/// `NoLedger` を `NotRecorded` へ丸めない: `LedgerMode::CompareOnlyWithoutLedger`
/// （台帳を持たない構成）では台帳テーブルへ一切触れないため、「未記録」という
/// 積極的な判定と「そもそも台帳を持たない」という消極的な事実を型で区別する
/// （fail-closed な区別。呼び出し元が両者を同じ意味に扱ってしまう誤用を防ぐ）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LedgerLookup {
    /// 台帳に記録済み。
    Recorded,
    /// 台帳あり構成だが未記録。
    NotRecorded,
    /// 台帳を持たない構成（[`LedgerWrite::Disabled`]）のため照会できない。
    NoLedger,
}

/// `write_txn` 内で `(tenant_id, table, operation_id)` を台帳へ追記する（TASK-93、
/// 対象ビヘイビア: RECOVER-2）。
///
/// 呼び出し元（`crate::tenant::*_unchecked`）が保持する同一 `redb::WriteTransaction`
/// を渡すこと。台帳追記は行の書き込み/削除と同一トランザクションになるため、
/// 呼び出し元が後続で commit しなければ台帳エントリも一緒に破棄される（原子性）。
///
/// 既存キーがあれば**上書きしない**（keep-first。台帳の恒久契約。TASK-98 の
/// `last_op` 追加でもこの意味は変えない）。
///
/// `content_hash`（TASK-101・RECOVER-10）: 既存エントリが検出された場合、
/// v2（内容ハッシュ保持）なら渡された `content_hash` と照合し、一致すれば
/// [`LedgerRecordError::Duplicate`]・不一致なら [`LedgerRecordError::ContentMismatch`]
/// を返す。v1（内容ハッシュ非保持のレガシーエントリ）は内容一致を証明できないため、
/// 常に `ContentMismatch` 側へ倒す（fail-closed。commit 済み確定の根拠として誤用
/// されるのを防ぐ。本番データが存在しない前提のため移行処理は導入しない）。
/// 新規記録時は v2 フォーマットで書き込む。
pub(crate) fn record_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant_id: &str,
    table: &str,
    ledger: LedgerWrite<'_>,
    content_hash: &ContentHash,
) -> Result<RecordOutcome, LedgerRecordError> {
    record_in_txn_accepting(write_txn, tenant_id, table, ledger, content_hash, &[])
}

/// [`record_in_txn`] の一般化版（Issue #876 レビュー指摘の是正）。
///
/// ハッシュ入力の正規化方式を変更した操作（例: `for_update_columns` の列順を
/// 宣言順からスキーマ列順へ変更した `tenant::update_row_columns_unchecked`）は、
/// 変更前のコードが記録した台帳エントリのハッシュと、変更後のコードが計算する
/// 正規ハッシュが食い違いうる。`legacy_hashes` にはそうした「正規化方式の変更前に
/// 実際に記録されえたハッシュ」の候補を渡す。既存エントリが `content_hash`
/// （新方式・以降の記録にも使う正準ハッシュ）と一致しなくても `legacy_hashes` の
/// いずれかと一致すれば同一内容の再送（`Duplicate`）とみなす。`legacy_hashes` は
/// 判定にのみ使い、新規記録・上書きには常に `content_hash`（正準ハッシュ）を使う
/// （keep-first 契約は変えない。「新方式のハッシュを計算できる場合は必ずそれを
/// 保存する」という不変条件を保ち、正規化方式の変遷を後続の呼び出しへ持ち越さない）。
///
/// 台帳エントリが無い場合、分割実行ジョブ表（Issue #1127・
/// [`crate::recovery::partitioned_job`]）も引く。同じ `operation_id` のジョブが
/// 実行中・中断・取り消し済み（`Running`／`Cancelled`）なら内容不一致
/// （[`LedgerRecordError::ContentMismatch`]・`22023`）として拒否する（commit 済み確定の
/// 根拠にしない。ADR `docs/design/partitioned-dml.md` §6）。`Completed` なのに台帳
/// エントリが無いのは不変条件違反なので [`LedgerRecordError::Corrupted`]（`XX000`）。
/// [`LedgerWrite::Disabled`] はジョブ表にも触れない（テーブルも作らない）。
pub(crate) fn record_in_txn_accepting(
    write_txn: &redb::WriteTransaction,
    tenant_id: &str,
    table: &str,
    ledger: LedgerWrite<'_>,
    content_hash: &ContentHash,
    legacy_hashes: &[ContentHash],
) -> Result<RecordOutcome, LedgerRecordError> {
    record_core(
        write_txn,
        tenant_id,
        table,
        ledger,
        content_hash,
        legacy_hashes,
        JobTableCheck::Enforce,
    )
}

/// 分割実行の完了経路（[`crate::recovery::partitioned_job::complete_in_txn`]）専用の
/// 台帳記録。ジョブ表の照合は行わない（自身が `Running` 記録を持つため）。それ以外の
/// 台帳の契約（keep-first・v2 値・`last_op` 更新）は通常の記録と同一。
pub(crate) fn record_partitioned_completion_in_txn(
    write_txn: &redb::WriteTransaction,
    tenant_id: &str,
    table: &str,
    op_id: &OperationId,
    content_hash: &ContentHash,
) -> Result<RecordOutcome, LedgerRecordError> {
    record_core(
        write_txn,
        tenant_id,
        table,
        LedgerWrite::Record(op_id),
        content_hash,
        &[],
        JobTableCheck::Skip,
    )
}

/// [`record_core`] がジョブ表を照合するか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobTableCheck {
    /// 通常 DML の記録経路。台帳エントリが無ければジョブ表を引いて衝突を拒否する。
    Enforce,
    /// 分割実行の完了経路。ジョブ表を引かない。
    Skip,
}

/// [`record_in_txn_accepting`]・[`record_partitioned_completion_in_txn`] の共通本体。
fn record_core(
    write_txn: &redb::WriteTransaction,
    tenant_id: &str,
    table: &str,
    ledger: LedgerWrite<'_>,
    content_hash: &ContentHash,
    legacy_hashes: &[ContentHash],
    job_check: JobTableCheck,
) -> Result<RecordOutcome, LedgerRecordError> {
    let op_id = match ledger {
        LedgerWrite::Record(op_id) => op_id,
        LedgerWrite::Disabled => return Ok(RecordOutcome::Skipped),
    };
    let mut ledger_table = write_txn.open_table(OP_LEDGER_TABLE)?;
    let key = (tenant_id, table, op_id.as_str());
    if let Some(guard) = ledger_table.get(key)? {
        let stored = decode_entry(guard.value()).map_err(|_| {
            LedgerRecordError::Corrupted(StorageError::Codec(
                "op_ledger entry has unknown format version".to_string(),
            ))
        })?;
        return match stored {
            StoredEntry::V1 => Err(LedgerRecordError::ContentMismatch),
            StoredEntry::V2 { hash } => {
                let matches_current = content_hash.matches(&hash);
                let matches_legacy =
                    !matches_current && legacy_hashes.iter().any(|legacy| legacy.matches(&hash));
                if matches_current || matches_legacy {
                    Err(LedgerRecordError::Duplicate)
                } else {
                    Err(LedgerRecordError::ContentMismatch)
                }
            }
        };
    }
    if job_check == JobTableCheck::Enforce {
        use crate::recovery::partitioned_job::{lookup_in_write_txn, JobKey, JobRecord};
        let job_key = JobKey::from_ledger_scope(tenant_id, table, op_id);
        match lookup_in_write_txn(write_txn, &job_key)
            .map_err(|e| LedgerRecordError::Corrupted(e.into_storage_error()))?
        {
            None => {}
            Some(JobRecord::Running { .. } | JobRecord::Cancelled { .. }) => {
                return Err(LedgerRecordError::ContentMismatch);
            }
            Some(JobRecord::Completed { .. }) => {
                return Err(LedgerRecordError::Corrupted(StorageError::Codec(
                    "completed partitioned job has no ledger entry".to_string(),
                )));
            }
        }
    }
    ledger_table.insert(key, encode_entry_v2(content_hash).as_slice())?;
    // `last_op`（TASK-98・RECOVER-7）: 同一 write トランザクション内で upsert する
    // （`op_ledger` の keep-first 契約とは独立。原子性は `write_txn` の commit/drop
    // に委ねる）。
    let mut last_op_table = write_txn.open_table(LAST_OP_TABLE)?;
    last_op_table.insert((tenant_id, table), encode_last_op(op_id).as_slice())?;
    Ok(RecordOutcome::Recorded)
}

/// `DROP TABLE` 相当の DDL（[`crate::catalog::Storage::drop_table`]）と**同一の
/// `redb::WriteTransaction`** 内で呼び出し、指定テーブル名に属する台帳エントリを
/// 全テナント分まとめて削除する（Issue #226 レビュー対応・PR #226）。
///
/// 背景: 台帳キーは `(tenant_id, table_name, operation_id)` のみでテーブルの世代を
/// 持たない。`drop_table` が台帳へ触れないまま同名テーブルを再作成すると、
/// 新しい（空の）テーブルに対して旧テーブルの `operation_id` が
/// [`contains_in_read_txn`] 経由で「記録済み」と誤判定され、正当な書き込みが
/// 拒否される事故につながる。`drop_table` は物理行ストア（`user_rows/{table}`）を
/// 削除する際、既にテナント横断で不可逆削除する設計（本ファイル冒頭コメント参照）
/// のため、台帳エントリもテナントを問わず同名分をまとめて削除して整合させる。
///
/// 台帳テーブル自体が未作成（一度も `Record` されていない DB、または
/// [`LedgerWrite::Disabled`] のみで運用してきた構成）の場合は no-op で `Ok(())` を
/// 返す（`contains_in_read_txn` と同じ「テーブル不在をエラーにしない」方針）。
///
/// `redb::WriteTransaction::open_table` はテーブル不在時に**自動作成**してしまう
/// （`ReadTransaction::open_table` と異なり `TableError::TableDoesNotExist` を返さ
/// ない）ため、直接 `open_table` を呼んで結果を分岐する実装では no-op 分岐が到達
/// 不能になり、`LedgerWrite::Disabled` が前提とする「台帳テーブルを作らない」契約を
/// 破って空テーブルを commit してしまう（Issue #226 レビュー対応・Cursor Bugbot
/// 指摘）。そのため `list_tables` で存在確認してから `open_table` する。
///
/// キー先頭が `tenant_id` のため `table_name` 単独の range 走査はできず、台帳全体の
/// 走査は避けられない。ただし 1 回の走査・削除で保持するキー集合は
/// [`DELETE_BATCH_SIZE`] 件までに有界化する（長期利用テーブルの `DROP TABLE` で
/// 台帳サイズに比例した無制限メモリを一度に要求しないようにするため。Issue #226
/// レビュー対応・codex-review 指摘。行データそのものへはアクセスしない）。
///
/// 走査は毎回先頭からやり直すのではなく、直前に処理したキーの**次**から
/// `range` を再開する（前方一方向）。再開点より手前のキーは「対象外（走査済みで
/// 残す）」か「対象（削除済み）」のいずれかで再訪の必要がないため、台帳全体で
/// 線形 1 パスに収まり、バッチ化による二次オーダー化を避けられる。
pub(crate) fn delete_table_in_txn(
    write_txn: &redb::WriteTransaction,
    table: &str,
) -> Result<(), StorageError> {
    let existing_table_names: std::collections::HashSet<String> = write_txn
        .list_tables()
        .map_err(StorageError::from)?
        .map(|handle| handle.name().to_string())
        .collect();

    if existing_table_names.contains(OP_LEDGER_TABLE.name()) {
        // 同一 write txn 内で同じテーブルを二重に `open_table` すると
        // `TableAlreadyOpen` になるため、ハンドルはループ外で 1 度だけ取得する。
        let mut ledger_table = write_txn.open_table(OP_LEDGER_TABLE)?;
        // 直前バッチで最後に処理したキー（次バッチの走査再開点。上記ドキュメント参照）。
        let mut resume_after: Option<(String, String, String)> = None;
        loop {
            let mut keys_to_remove: Vec<(String, String, String)> = Vec::new();
            let mut reached_batch_limit = false;
            {
                let lower = match resume_after.as_ref() {
                    Some((tenant_id, table_name, op_id)) => {
                        Bound::Excluded((tenant_id.as_str(), table_name.as_str(), op_id.as_str()))
                    }
                    None => Bound::Unbounded,
                };
                let iter = ledger_table.range::<(&str, &str, &str)>((lower, Bound::Unbounded))?;
                for entry in iter {
                    let (k, _v) = entry?;
                    let (tenant_id, table_name, op_id) = k.value();
                    if table_name == table {
                        keys_to_remove.push((
                            tenant_id.to_string(),
                            table_name.to_string(),
                            op_id.to_string(),
                        ));
                        if keys_to_remove.len() >= DELETE_BATCH_SIZE {
                            // 走査を打ち切る位置＝直前に push したキー。次バッチはこの
                            // キーの次から再開する。
                            reached_batch_limit = true;
                            break;
                        }
                    }
                }
            }
            resume_after = keys_to_remove.last().cloned();
            for (tenant_id, table_name, op_id) in &keys_to_remove {
                ledger_table.remove((tenant_id.as_str(), table_name.as_str(), op_id.as_str()))?;
            }
            if !reached_batch_limit {
                break;
            }
        }
    }

    // `last_op`（TASK-98・RECOVER-7）: 対象テーブル名分を全テナントから削除する
    // （drop → 同名再作成での旧 `last_op` 引き継ぎ防止。有界バッチ走査パターンは
    // 上の `OP_LEDGER_TABLE` 側と揃える）。
    if existing_table_names.contains(LAST_OP_TABLE.name()) {
        let mut last_op_table = write_txn.open_table(LAST_OP_TABLE)?;
        let mut resume_after: Option<(String, String)> = None;
        loop {
            let mut keys_to_remove: Vec<(String, String)> = Vec::new();
            let mut reached_batch_limit = false;
            {
                let lower = match resume_after.as_ref() {
                    Some((tenant_id, table_name)) => {
                        Bound::Excluded((tenant_id.as_str(), table_name.as_str()))
                    }
                    None => Bound::Unbounded,
                };
                let iter = last_op_table.range::<(&str, &str)>((lower, Bound::Unbounded))?;
                for entry in iter {
                    let (k, _v) = entry?;
                    let (tenant_id, table_name) = k.value();
                    if table_name == table {
                        keys_to_remove.push((tenant_id.to_string(), table_name.to_string()));
                        if keys_to_remove.len() >= DELETE_BATCH_SIZE {
                            reached_batch_limit = true;
                            break;
                        }
                    }
                }
            }
            resume_after = keys_to_remove.last().cloned();
            for (tenant_id, table_name) in &keys_to_remove {
                last_op_table.remove((tenant_id.as_str(), table_name.as_str()))?;
            }
            if !reached_batch_limit {
                break;
            }
        }
    }

    Ok(())
}

/// write txn 内で `(tenant_id, table, op_id)` が台帳に記録済みかを照会する
/// （Issue #1127。分割実行の各チャンクが走査前に台帳を引く #1128 の土台）。
/// 台帳テーブルが未作成なら `false`（`list_tables` で存在確認し、write txn の
/// `open_table` の自動作成で空テーブルを作らない）。
#[allow(dead_code)] // #1128 の分割実行器が結線するまで未使用
pub(crate) fn lookup_in_write_txn(
    write_txn: &redb::WriteTransaction,
    tenant_id: &str,
    table: &str,
    op_id: &OperationId,
) -> Result<bool, LedgerRecordError> {
    let exists = write_txn
        .list_tables()?
        .any(|handle| handle.name() == OP_LEDGER_TABLE.name());
    if !exists {
        return Ok(false);
    }
    let ledger_table = write_txn.open_table(OP_LEDGER_TABLE)?;
    let found = match ledger_table.get((tenant_id, table, op_id.as_str()))? {
        Some(guard) => {
            decode_entry(guard.value()).map_err(|_| {
                LedgerRecordError::Corrupted(StorageError::Codec(
                    "op_ledger entry has unknown format version".to_string(),
                ))
            })?;
            true
        }
        None => false,
    };
    Ok(found)
}

/// `read_txn` 内で `(tenant_id, table, op_id)` が台帳に記録済みかを照会する（TASK-93、
/// 対象ビヘイビア: RECOVER-2）。台帳テーブルが未作成（台帳を一度も使っていない DB、
/// または [`LedgerWrite::Disabled`] のみで運用してきた構成）の場合は `false` を返す
/// （テーブル不在をエラーにしない）。
pub(crate) fn contains_in_read_txn(
    read_txn: &redb::ReadTransaction,
    tenant_id: &str,
    table: &str,
    op_id: &OperationId,
) -> Result<bool, StorageError> {
    let ledger_table = match read_txn.open_table(OP_LEDGER_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(StorageError::from(e)),
    };
    let key = (tenant_id, table, op_id.as_str());
    match ledger_table.get(key)? {
        Some(guard) => {
            // v1・v2 のいずれも「記録済み」と判定する（[`decode_entry`] が未知
            // フォーマットのみ拒否する。中身のハッシュ値は照会用途では不要）。
            decode_entry(guard.value()).map_err(|_| {
                StorageError::Codec("op_ledger entry has unknown format version".to_string())
            })?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// [`crate::core::EngineCore::last_operation_id`] の照会結果（TASK-98、対象ビヘイビア:
/// RECOVER-7。契約の詳細は spec 参照）。[`LedgerLookup`] と同じ設計原則で `NoLedger`
/// を `NotFound` へ丸めない（`LedgerMode::CompareOnlyWithoutLedger` では台帳を一切
/// 持たないため「未記録」と「そもそも台帳を持たない」を型で区別する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastOperationLookup {
    /// テーブルあたり最後に commit 記録された `operation_id`。
    Committed(OperationId),
    /// 台帳あり構成だが、当該テーブルへの記録がまだ 1 件もない（真の未記録）。
    NotFound,
    /// 台帳を持たない構成（[`LedgerWrite::Disabled`]）のため照会できない。
    NoLedger,
    /// 正確な最終 `operation_id` を復元できないため `NotFound`（真の未記録）へ
    /// 丸めず fail-closed に区別する区分（TASK-98・RECOVER-7、codex-review P1
    /// 指摘対応）。
    Unavailable,
}

/// [`last_operation_in_read_txn`] の生の照会結果。呼び出し元（[`crate::tenant::last_operation`]
/// 経由で [`crate::core::EngineCore::last_operation_id`]）が `LastOperationLookup` へ写像する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LastOperationRaw {
    /// `last_op` に記録済み。
    Found(OperationId),
    /// `last_op`・`op_ledger` いずれにも当該 `(tenant_id, table)` の記録がない
    /// （真の未記録）。
    NotFound,
    /// [`LastOperationLookup::Unavailable`] に対応する状態（詳細は同 variant 参照）。
    LegacyLedgerWithoutLastOp,
}

/// `op_ledger` に `(tenant_id, table)` の記録が 1 件でも存在するかを確認する
/// （TASK-98、対象ビヘイビア: RECOVER-7・codex-review P1 指摘対応。契約の詳細は
/// spec 参照）。[`last_operation_in_read_txn`] が [`LastOperationRaw::LegacyLedgerWithoutLastOp`]
/// を判定するために使う。
///
/// `op_ledger` のキーは `(tenant_id, table_name, operation_id)` の辞書順のため、
/// `(tenant_id, table, "")` を下限とする range の先頭 1 件を見るだけで判定でき、
/// テーブル全体の走査は不要（`operation_id` は空文字列を許さない検証済み値のため、
/// 空文字列は常に下限として機能する）。
fn op_ledger_has_entry_for(
    read_txn: &redb::ReadTransaction,
    tenant_id: &str,
    table: &str,
) -> Result<bool, StorageError> {
    let ledger_table = match read_txn.open_table(OP_LEDGER_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(StorageError::from(e)),
    };
    let lower = Bound::Included((tenant_id, table, ""));
    let mut iter = ledger_table.range::<(&str, &str, &str)>((lower, Bound::Unbounded))?;
    match iter.next() {
        Some(entry) => {
            let (k, _v) = entry?;
            let (found_tenant, found_table, _op_id) = k.value();
            Ok(found_tenant == tenant_id && found_table == table)
        }
        None => Ok(false),
    }
}

/// `read_txn` 内で `(tenant_id, table)` の最終 commit 済み `operation_id` を照会する
/// （TASK-98、対象ビヘイビア: RECOVER-7。契約の詳細は spec 参照）。`last_op` に
/// テーブル不在・記録なしのいずれでも、[`op_ledger_has_entry_for`] を用いて
/// `NotFound` を確定する前に [`LastOperationRaw::LegacyLedgerWithoutLastOp`] 判定を
/// 経る（codex-review P1 指摘対応）。未知フォーマットバージョン・不正 UTF-8・不正な
/// `operation_id` はいずれも fail-closed に拒否する（[`decode_last_op`]）。
///
/// 呼び出し元（[`crate::tenant::last_operation`]）が `tenant_id` をサーバー側導出値に
/// 限定する（security.md P0・TABLE-12・RLS-9）。
pub(crate) fn last_operation_in_read_txn(
    read_txn: &redb::ReadTransaction,
    tenant_id: &str,
    table: &str,
) -> Result<LastOperationRaw, StorageError> {
    let last_op_table = match read_txn.open_table(LAST_OP_TABLE) {
        Ok(t) => Some(t),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(StorageError::from(e)),
    };

    if let Some(t) = last_op_table.as_ref() {
        if let Some(guard) = t.get((tenant_id, table))? {
            return decode_last_op(guard.value())
                .map(LastOperationRaw::Found)
                .map_err(|e| {
                    // 診断メッセージのみを理由ごとに分ける（テナント・テーブル名・値の
                    // 内容は含めない・security.md P0）。fail-closed の判定自体はどの
                    // 理由でも同じ。
                    let msg = match e {
                        LastOpDecodeError::UnknownVersion => {
                            "last_op entry has unknown format version"
                        }
                        LastOpDecodeError::InvalidUtf8 => "last_op entry has invalid utf-8 payload",
                        LastOpDecodeError::InvalidOperationId => {
                            "last_op entry has invalid operation_id"
                        }
                    };
                    StorageError::Codec(msg.to_string())
                });
        }
    }

    if op_ledger_has_entry_for(read_txn, tenant_id, table)? {
        return Ok(LastOperationRaw::LegacyLedgerWithoutLastOp);
    }
    Ok(LastOperationRaw::NotFound)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::required_op_id::OperationId;
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    fn op(id: &str) -> OperationId {
        OperationId::parse(id).expect("valid operation_id")
    }

    fn hash(seed: &str) -> ContentHash {
        ContentHash::for_test(seed.as_bytes())
    }

    // (a) 記録 → 同一 txn commit 後に contains が true になる。
    #[test]
    fn record_then_commit_is_observable_in_read_txn() {
        let path = unique_db_path("ledger-a");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        let outcome = record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-a")),
            &hash("content-a"),
        )
        .expect("record");
        assert_eq!(outcome, RecordOutcome::Recorded);
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let found = contains_in_read_txn(&read_txn, "tenant-a", "documents", &op("op-a"))
            .expect("contains");
        assert!(found);
    }

    // (b) Record 後に txn を drop すると、コミットされず false のまま。
    #[test]
    fn record_then_drop_without_commit_is_not_observable() {
        let path = unique_db_path("ledger-b");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        {
            let write_txn = db.begin_write().expect("begin write");
            let outcome = record_in_txn(
                &write_txn,
                "tenant-a",
                "documents",
                LedgerWrite::Record(&op("op-b")),
                &hash("content-b"),
            )
            .expect("record");
            assert_eq!(outcome, RecordOutcome::Recorded);
            // 明示的に commit しない: drop により abort される。
        }

        let read_txn = db.begin_read().expect("begin read");
        let found = contains_in_read_txn(&read_txn, "tenant-a", "documents", &op("op-b"))
            .expect("contains");
        assert!(!found);
    }

    // (c-1) 同一キー・同一内容の 2 回目は Duplicate（TASK-101・RECOVER-10。呼び出し元が
    // `23505` へ写像する）で、値（台帳エントリ）は変わらない（keep-first）。
    #[test]
    fn second_record_of_same_key_and_content_is_duplicate_and_keeps_first_value() {
        let path = unique_db_path("ledger-c1");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        let first = record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-c")),
            &hash("content-c"),
        )
        .expect("record first");
        assert_eq!(first, RecordOutcome::Recorded);
        let second = record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-c")),
            &hash("content-c"),
        )
        .expect_err("same content resend must be rejected as Duplicate");
        assert!(matches!(second, LedgerRecordError::Duplicate));
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let found = contains_in_read_txn(&read_txn, "tenant-a", "documents", &op("op-c"))
            .expect("contains");
        assert!(found);
    }

    // (c-2) 同一キー・内容が異なる 2 回目は ContentMismatch（呼び出し元が `22023` へ
    // 写像する。fail-closed: commit 済み確定の根拠にしない）。
    #[test]
    fn second_record_of_same_key_with_different_content_is_content_mismatch() {
        let path = unique_db_path("ledger-c2");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-c2")),
            &hash("content-original"),
        )
        .expect("record first");
        let second = record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-c2")),
            &hash("content-different"),
        )
        .expect_err("different content resend must be rejected as ContentMismatch");
        assert!(matches!(second, LedgerRecordError::ContentMismatch));
    }

    // (d) Disabled は台帳テーブルを作らない。
    #[test]
    fn disabled_write_does_not_create_ledger_table() {
        let path = unique_db_path("ledger-d");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        let outcome = record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Disabled,
            &hash("unused"),
        )
        .expect("record");
        assert_eq!(outcome, RecordOutcome::Skipped);
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let found = contains_in_read_txn(&read_txn, "tenant-a", "documents", &op("op-d"))
            .expect("contains on missing table must be false, not an error");
        assert!(!found);
    }

    // (e-legacy) v1（内容ハッシュ非保持）の既存エントリへの再送は、内容一致を証明
    // できないため常に ContentMismatch へ倒す（TASK-101・RECOVER-10 の fail-closed
    // 設計判断。commit 済み確定の根拠として誤って `23505` を返さない）。
    #[test]
    fn resend_to_legacy_v1_entry_is_rejected_as_content_mismatch() {
        let path = unique_db_path("ledger-e-legacy");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        {
            let mut table = write_txn.open_table(OP_LEDGER_TABLE).expect("open table");
            // v1 フォーマット（バージョンバイトのみ）を直接挿入して、TASK-101 以前の
            // 台帳エントリを再現する。
            table
                .insert(
                    ("tenant-a", "documents", "op-legacy"),
                    [LEDGER_ENTRY_FORMAT_VERSION_V1].as_slice(),
                )
                .expect("insert raw v1 entry");
        }
        write_txn.commit().expect("commit");

        let write_txn2 = db.begin_write().expect("begin write");
        let err = record_in_txn(
            &write_txn2,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-legacy")),
            &hash("any-content"),
        )
        .expect_err("legacy v1 entry must be rejected as ContentMismatch");
        assert!(matches!(err, LedgerRecordError::ContentMismatch));
    }

    // (e) 未知フォーマットバージョンの値は fail-closed（デコードエラー）。
    #[test]
    fn unknown_format_version_is_rejected_fail_closed() {
        let path = unique_db_path("ledger-e");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        {
            let mut table = write_txn.open_table(OP_LEDGER_TABLE).expect("open table");
            table
                .insert(("tenant-a", "documents", "op-e"), [0xffu8].as_slice())
                .expect("insert raw unknown-version entry");
        }
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let err = contains_in_read_txn(&read_txn, "tenant-a", "documents", &op("op-e"))
            .expect_err("unknown format version must be rejected");
        assert!(matches!(err, StorageError::Codec(_)));

        // record_in_txn 側でも同じキーへの追記が同様に拒否される（keep-first の
        // 分岐に入る前に、未知フォーマットの既存値を検出して fail-closed）。
        let write_txn2 = db.begin_write().expect("begin write");
        let err2 = record_in_txn(
            &write_txn2,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-e")),
            &hash("content-e"),
        )
        .expect_err("unknown format version must be rejected on record too");
        assert!(matches!(err2, LedgerRecordError::Corrupted(_)));
    }

    // (f) delete_table_in_txn は指定テーブル名分を全テナントから削除し、他テーブル名の
    // エントリには触れない（Issue #226 レビュー対応: drop→同名再作成での旧台帳
    // エントリ引き継ぎ防止）。
    #[test]
    fn delete_table_in_txn_removes_all_tenants_for_table_only() {
        let path = unique_db_path("ledger-f");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-f1")),
            &hash("content-f1"),
        )
        .expect("record tenant-a/documents");
        record_in_txn(
            &write_txn,
            "tenant-b",
            "documents",
            LedgerWrite::Record(&op("op-f2")),
            &hash("content-f2"),
        )
        .expect("record tenant-b/documents");
        record_in_txn(
            &write_txn,
            "tenant-a",
            "other_table",
            LedgerWrite::Record(&op("op-f3")),
            &hash("content-f3"),
        )
        .expect("record tenant-a/other_table");
        write_txn.commit().expect("commit initial records");

        let write_txn = db.begin_write().expect("begin write");
        delete_table_in_txn(&write_txn, "documents").expect("delete_table_in_txn");
        write_txn.commit().expect("commit delete");

        let read_txn = db.begin_read().expect("begin read");
        assert!(
            !contains_in_read_txn(&read_txn, "tenant-a", "documents", &op("op-f1"))
                .expect("contains a/documents")
        );
        assert!(
            !contains_in_read_txn(&read_txn, "tenant-b", "documents", &op("op-f2"))
                .expect("contains b/documents")
        );
        assert!(
            contains_in_read_txn(&read_txn, "tenant-a", "other_table", &op("op-f3"))
                .expect("contains a/other_table must survive")
        );

        // drop 後に同名テーブルへ同じ operation_id で再記録できる（旧エントリが
        // 引き継がれ NotRecorded ではなく誤って Recorded 扱いになる事故が起きない）。
        let write_txn = db.begin_write().expect("begin write");
        let outcome = record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-f1")),
            &hash("content-f1-v2"),
        )
        .expect("re-record after drop");
        assert_eq!(outcome, RecordOutcome::Recorded);
        write_txn.commit().expect("commit re-record");
    }

    // (g) 台帳テーブル未作成のまま delete_table_in_txn を呼んでもエラーにならない。
    #[test]
    fn delete_table_in_txn_is_noop_when_ledger_table_missing() {
        let path = unique_db_path("ledger-g");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        delete_table_in_txn(&write_txn, "documents")
            .expect("no-op when ledger table has never been created");
        write_txn.commit().expect("commit");
    }

    // (h) delete_table_in_txn は台帳テーブル未作成の DB に対して呼んでも op_ledger
    // テーブル自体を作成しない（LedgerWrite::Disabled が前提とする「台帳を作らない」
    // 契約を守る。Issue #226 レビュー対応・Cursor Bugbot 指摘の回帰テスト）。
    #[test]
    fn delete_table_in_txn_does_not_create_ledger_table_as_side_effect() {
        let path = unique_db_path("ledger-h");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        delete_table_in_txn(&write_txn, "documents").expect("no-op");
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let table_names: Vec<String> = read_txn
            .list_tables()
            .expect("list_tables")
            .map(|handle| handle.name().to_string())
            .collect();
        assert!(
            !table_names
                .iter()
                .any(|name| name == OP_LEDGER_TABLE.name()),
            "delete_table_in_txn must not create op_ledger as a side effect: {table_names:?}"
        );
    }

    // (i) DELETE_BATCH_SIZE を超える件数の対象エントリでも、バッチ分割走査が
    // 打ち切られずに全件削除され、他テーブル名のエントリは残る（Issue #226
    // レビュー対応・codex-review 指摘のメモリ有界化に伴う複数バッチ経路の回帰
    // テスト。テナントを跨いだ削除である契約も同時に確認する）。
    #[test]
    fn delete_table_in_txn_removes_entries_across_multiple_batches() {
        let path = unique_db_path("ledger-i");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        // 対象テーブル分は 2 テナントに分散させ、合計で DELETE_BATCH_SIZE を超えさせる。
        let per_tenant = DELETE_BATCH_SIZE + 7;
        let write_txn = db.begin_write().expect("begin write");
        for tenant in ["tenant-a", "tenant-b"] {
            for i in 0..per_tenant {
                record_in_txn(
                    &write_txn,
                    tenant,
                    "documents",
                    LedgerWrite::Record(&op(&format!("op-{i:06}"))),
                    &hash(&format!("content-{tenant}-{i:06}")),
                )
                .expect("record");
            }
        }
        // 削除対象外（別テーブル名）。キー順で対象の前後どちらにも現れるようにする。
        for table in ["alpha_table", "zeta_table"] {
            for i in 0..3 {
                record_in_txn(
                    &write_txn,
                    "tenant-a",
                    table,
                    LedgerWrite::Record(&op(&format!("op-{i:06}"))),
                    &hash(&format!("content-{table}-{i:06}")),
                )
                .expect("record");
            }
        }
        write_txn.commit().expect("commit");

        let write_txn = db.begin_write().expect("begin write");
        delete_table_in_txn(&write_txn, "documents").expect("delete_table_in_txn");
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        for tenant in ["tenant-a", "tenant-b"] {
            for i in 0..per_tenant {
                let found = contains_in_read_txn(
                    &read_txn,
                    tenant,
                    "documents",
                    &op(&format!("op-{i:06}")),
                )
                .expect("contains");
                assert!(!found, "target entry must be removed: {tenant} op-{i:06}");
            }
        }
        for table in ["alpha_table", "zeta_table"] {
            for i in 0..3 {
                let found =
                    contains_in_read_txn(&read_txn, "tenant-a", table, &op(&format!("op-{i:06}")))
                        .expect("contains");
                assert!(found, "other table entry must survive: {table} op-{i:06}");
            }
        }
    }

    // --- TASK-98・RECOVER-7: `last_op` の照会 API ------------------------------

    // (A1) 成功 commit 後、last_operation_in_read_txn が当該 operation_id を返す。
    #[test]
    fn last_operation_returns_committed_operation_id() {
        let path = unique_db_path("last-op-a1");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-a1")),
            &hash("content-a1"),
        )
        .expect("record");
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let found = last_operation_in_read_txn(&read_txn, "tenant-a", "documents")
            .expect("last_operation_in_read_txn");
        assert_eq!(found, LastOperationRaw::Found(op("op-a1")));
    }

    // (A2) 同一テーブルへの後続 commit で last_op が置き換わる。一方、op_ledger
    // op_ledger の既存エントリは削除・置換されない（keep-first 併存確認）。
    #[test]
    fn last_operation_is_replaced_by_subsequent_commit_while_op_ledger_keeps_first() {
        let path = unique_db_path("last-op-a2");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-a2-first")),
            &hash("content-a2-first"),
        )
        .expect("record first");
        write_txn.commit().expect("commit first");

        let write_txn = db.begin_write().expect("begin write");
        record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-a2-second")),
            &hash("content-a2-second"),
        )
        .expect("record second");
        write_txn.commit().expect("commit second");

        let read_txn = db.begin_read().expect("begin read");
        let last = last_operation_in_read_txn(&read_txn, "tenant-a", "documents")
            .expect("last_operation_in_read_txn");
        assert_eq!(last, LastOperationRaw::Found(op("op-a2-second")));
        // op_ledger は両方とも記録済みのまま（keep-first で先着エントリも消えない）。
        assert!(
            contains_in_read_txn(&read_txn, "tenant-a", "documents", &op("op-a2-first"))
                .expect("contains first")
        );
        assert!(
            contains_in_read_txn(&read_txn, "tenant-a", "documents", &op("op-a2-second"))
                .expect("contains second")
        );
    }

    // (A3) txn drop（未 commit）では last_op が更新されない。
    #[test]
    fn last_operation_not_observable_without_commit() {
        let path = unique_db_path("last-op-a3");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        {
            let write_txn = db.begin_write().expect("begin write");
            record_in_txn(
                &write_txn,
                "tenant-a",
                "documents",
                LedgerWrite::Record(&op("op-a3")),
                &hash("content-a3"),
            )
            .expect("record");
            // 明示的に commit しない: drop により abort される。
        }

        let read_txn = db.begin_read().expect("begin read");
        let found = last_operation_in_read_txn(&read_txn, "tenant-a", "documents")
            .expect("last_operation_in_read_txn");
        assert_eq!(found, LastOperationRaw::NotFound);
    }

    // (A4) Duplicate（内容一致再送）・ContentMismatch の拒否経路では last_op が
    // 変わらない。
    #[test]
    fn last_operation_unchanged_on_rejected_resend() {
        let path = unique_db_path("last-op-a4");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-a4")),
            &hash("content-a4"),
        )
        .expect("record first");
        // 同一内容の再送は Duplicate。
        let dup = record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-a4")),
            &hash("content-a4"),
        )
        .expect_err("same content resend must be Duplicate");
        assert!(matches!(dup, LedgerRecordError::Duplicate));
        // 内容が異なる別 operation_id での再送は書き込み自体は別キーなので新規記録
        // されるが、ここでは同一キーへの内容不一致を確認する。
        let mismatch = record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-a4")),
            &hash("content-a4-different"),
        )
        .expect_err("different content resend must be ContentMismatch");
        assert!(matches!(mismatch, LedgerRecordError::ContentMismatch));
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let found = last_operation_in_read_txn(&read_txn, "tenant-a", "documents")
            .expect("last_operation_in_read_txn");
        assert_eq!(found, LastOperationRaw::Found(op("op-a4")));
    }

    // (A5) LedgerWrite::Disabled は last_op テーブルを作らない・照会はテーブル不在で
    // None。
    #[test]
    fn last_operation_disabled_write_does_not_create_table() {
        let path = unique_db_path("last-op-a5");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Disabled,
            &hash("unused"),
        )
        .expect("record");
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let found = last_operation_in_read_txn(&read_txn, "tenant-a", "documents")
            .expect("last_operation_in_read_txn on missing table must be NotFound, not an error");
        assert_eq!(found, LastOperationRaw::NotFound);
        let table_names: Vec<String> = read_txn
            .list_tables()
            .expect("list_tables")
            .map(|handle| handle.name().to_string())
            .collect();
        assert!(!table_names.iter().any(|name| name == LAST_OP_TABLE.name()));
    }

    // (A6) 未知フォーマットバージョン・不正 UTF-8 の last_op 値は fail-closed に拒否。
    #[test]
    fn last_operation_unknown_format_version_is_rejected_fail_closed() {
        let path = unique_db_path("last-op-a6");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        {
            let mut table = write_txn.open_table(LAST_OP_TABLE).expect("open table");
            table
                .insert(("tenant-a", "documents"), [0xffu8].as_slice())
                .expect("insert raw unknown-version entry");
        }
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let err = last_operation_in_read_txn(&read_txn, "tenant-a", "documents")
            .expect_err("unknown format version must be rejected");
        assert!(matches!(err, StorageError::Codec(_)));
    }

    // (A6-b) 不正 UTF-8 のバイト列も同様に fail-closed。
    #[test]
    fn last_operation_invalid_utf8_is_rejected_fail_closed() {
        let path = unique_db_path("last-op-a6b");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        {
            let mut table = write_txn.open_table(LAST_OP_TABLE).expect("open table");
            table
                .insert(
                    ("tenant-a", "documents"),
                    [LAST_OP_FORMAT_VERSION_V1, 0xff, 0xfe].as_slice(),
                )
                .expect("insert raw invalid-utf8 entry");
        }
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let err = last_operation_in_read_txn(&read_txn, "tenant-a", "documents")
            .expect_err("invalid utf8 must be rejected");
        assert!(matches!(err, StorageError::Codec(_)));
    }

    // (A6-c) UTF-8 としては妥当だが OperationId::parse が拒否する値（ここでは空文字列）
    // も fail-closed。A6/A6-b とは異なる診断メッセージになることを確認する
    // （codex-review 指摘対応・3 つの拒否理由を単一メッセージへ丸めない）。
    #[test]
    fn last_operation_invalid_operation_id_is_rejected_fail_closed() {
        let path = unique_db_path("last-op-a6c");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        {
            let mut table = write_txn.open_table(LAST_OP_TABLE).expect("open table");
            table
                .insert(
                    ("tenant-a", "documents"),
                    [LAST_OP_FORMAT_VERSION_V1].as_slice(),
                )
                .expect("insert raw entry with empty operation_id payload");
        }
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let err = last_operation_in_read_txn(&read_txn, "tenant-a", "documents")
            .expect_err("invalid operation_id must be rejected");
        match &err {
            StorageError::Codec(msg) => {
                assert!(
                    msg.contains("invalid operation_id"),
                    "expected an operation_id-specific diagnostic, got: {msg}"
                );
            }
            other => panic!("expected StorageError::Codec, got: {other:?}"),
        }
    }

    // (A7) delete_table_in_txn が対象テーブルの last_op を全テナント分削除し、他
    // テーブル分は残す。last_op テーブル未作成時は no-op かつ副作用でテーブルを
    // 作らない。
    #[test]
    fn delete_table_in_txn_removes_last_op_for_table_only() {
        let path = unique_db_path("last-op-a7");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        record_in_txn(
            &write_txn,
            "tenant-a",
            "documents",
            LedgerWrite::Record(&op("op-a7-1")),
            &hash("content-a7-1"),
        )
        .expect("record tenant-a/documents");
        record_in_txn(
            &write_txn,
            "tenant-b",
            "documents",
            LedgerWrite::Record(&op("op-a7-2")),
            &hash("content-a7-2"),
        )
        .expect("record tenant-b/documents");
        record_in_txn(
            &write_txn,
            "tenant-a",
            "other_table",
            LedgerWrite::Record(&op("op-a7-3")),
            &hash("content-a7-3"),
        )
        .expect("record tenant-a/other_table");
        write_txn.commit().expect("commit initial records");

        let write_txn = db.begin_write().expect("begin write");
        delete_table_in_txn(&write_txn, "documents").expect("delete_table_in_txn");
        write_txn.commit().expect("commit delete");

        let read_txn = db.begin_read().expect("begin read");
        assert_eq!(
            last_operation_in_read_txn(&read_txn, "tenant-a", "documents")
                .expect("last_operation tenant-a/documents"),
            LastOperationRaw::NotFound
        );
        assert_eq!(
            last_operation_in_read_txn(&read_txn, "tenant-b", "documents")
                .expect("last_operation tenant-b/documents"),
            LastOperationRaw::NotFound
        );
        assert_eq!(
            last_operation_in_read_txn(&read_txn, "tenant-a", "other_table")
                .expect("last_operation tenant-a/other_table must survive"),
            LastOperationRaw::Found(op("op-a7-3"))
        );
    }

    // (A7-b) last_op テーブル未作成のまま delete_table_in_txn を呼んでもエラーに
    // ならず、副作用で last_op テーブルを作らない。
    #[test]
    fn delete_table_in_txn_does_not_create_last_op_table_as_side_effect() {
        let path = unique_db_path("last-op-a7b");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        delete_table_in_txn(&write_txn, "documents").expect("no-op");
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        let table_names: Vec<String> = read_txn
            .list_tables()
            .expect("list_tables")
            .map(|handle| handle.name().to_string())
            .collect();
        assert!(!table_names.iter().any(|name| name == LAST_OP_TABLE.name()));
    }

    // --- codex-review P1 指摘対応: `last_op` 導入前 DB の照会 --------------------

    // (A8) `last_op` テーブルが未作成のまま `op_ledger` にのみ記録がある
    // （`last_op` 導入〔TASK-98〕前に書かれた DB を模す）場合、`NotFound`
    // （未記録）へ丸めず `LegacyLedgerWithoutLastOp` を返す。
    #[test]
    fn last_operation_returns_legacy_when_only_op_ledger_has_entry_and_last_op_table_missing() {
        let path = unique_db_path("last-op-a8");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        // `record_in_txn` は両テーブルを同一トランザクションで書くため、旧 DB を
        // 模すには `op_ledger` のみへ直接書き込む（`last_op` には触れない）。
        let write_txn = db.begin_write().expect("begin write");
        {
            let mut ledger_table = write_txn.open_table(OP_LEDGER_TABLE).expect("open table");
            ledger_table
                .insert(
                    ("tenant-a", "documents", "op-a8"),
                    encode_entry_v2(&hash("content-a8")).as_slice(),
                )
                .expect("insert legacy entry");
        }
        write_txn.commit().expect("commit");

        let read_txn = db.begin_read().expect("begin read");
        // `last_op` テーブル自体が未作成であることの前提確認。
        let table_names: Vec<String> = read_txn
            .list_tables()
            .expect("list_tables")
            .map(|handle| handle.name().to_string())
            .collect();
        assert!(!table_names.iter().any(|name| name == LAST_OP_TABLE.name()));

        let found = last_operation_in_read_txn(&read_txn, "tenant-a", "documents")
            .expect("last_operation_in_read_txn");
        assert_eq!(found, LastOperationRaw::LegacyLedgerWithoutLastOp);
    }

    // (A9) `last_op` テーブル自体は存在する（他テーブルへの post-upgrade commit で
    // 作成済み）が、照会対象の `(tenant_id, table)` にはまだ記録がなく、`op_ledger`
    // 側にのみ記録が残っている場合も `LegacyLedgerWithoutLastOp` を返す。
    #[test]
    fn last_operation_returns_legacy_when_last_op_table_exists_but_lacks_entry_for_table() {
        let path = unique_db_path("last-op-a9");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        // 旧 `op_ledger` 記録（`documents` テーブル分。`last_op` には書かない）。
        let write_txn = db.begin_write().expect("begin write");
        {
            let mut ledger_table = write_txn.open_table(OP_LEDGER_TABLE).expect("open table");
            ledger_table
                .insert(
                    ("tenant-a", "documents", "op-a9-legacy"),
                    encode_entry_v2(&hash("content-a9-legacy")).as_slice(),
                )
                .expect("insert legacy entry");
        }
        write_txn.commit().expect("commit legacy entry");

        // アップグレード後、別テーブル（`other_table`）への正規経路の書き込みで
        // `last_op` テーブルが作成される。
        let write_txn = db.begin_write().expect("begin write");
        record_in_txn(
            &write_txn,
            "tenant-a",
            "other_table",
            LedgerWrite::Record(&op("op-a9-other")),
            &hash("content-a9-other"),
        )
        .expect("record other_table");
        write_txn.commit().expect("commit other_table");

        let read_txn = db.begin_read().expect("begin read");
        // `last_op` テーブルは存在するが、`documents`/`tenant-a` の記録は持たない前提。
        let table_names: Vec<String> = read_txn
            .list_tables()
            .expect("list_tables")
            .map(|handle| handle.name().to_string())
            .collect();
        assert!(table_names.iter().any(|name| name == LAST_OP_TABLE.name()));

        let found = last_operation_in_read_txn(&read_txn, "tenant-a", "documents")
            .expect("last_operation_in_read_txn");
        assert_eq!(found, LastOperationRaw::LegacyLedgerWithoutLastOp);

        // 一方、正規経路で書かれた other_table は通常どおり Found を返す。
        let found_other = last_operation_in_read_txn(&read_txn, "tenant-a", "other_table")
            .expect("last_operation_in_read_txn other_table");
        assert_eq!(found_other, LastOperationRaw::Found(op("op-a9-other")));
    }

    // (A10) `last_op` テーブルが存在していても、`op_ledger` 側にも当該
    // `(tenant_id, table)` の記録が一切ない場合は真の `NotFound` のまま
    // （`LegacyLedgerWithoutLastOp` へ誤判定しない）。
    #[test]
    fn last_operation_returns_not_found_when_neither_table_has_entry_even_if_last_op_table_exists()
    {
        let path = unique_db_path("last-op-a10");
        let _guard = CleanupGuard(path.clone());
        let db = redb::Database::create(&path).expect("create db");

        let write_txn = db.begin_write().expect("begin write");
        record_in_txn(
            &write_txn,
            "tenant-a",
            "other_table",
            LedgerWrite::Record(&op("op-a10-other")),
            &hash("content-a10-other"),
        )
        .expect("record other_table");
        write_txn.commit().expect("commit other_table");

        let read_txn = db.begin_read().expect("begin read");
        let found = last_operation_in_read_txn(&read_txn, "tenant-a", "never-written")
            .expect("last_operation_in_read_txn");
        assert_eq!(found, LastOperationRaw::NotFound);
    }
}
