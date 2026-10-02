//! 分割実行 DML のジョブ状態の永続化と、プロセス内の実行中ジョブ登録簿
//! （Issue #1127。ポインタ: ADR `docs/design/partitioned-dml.md` §4.1・§5.3・§6・
//! §9.2・§15.2〜§15.4、`docs/spec/04-behavior/recovery.md` RECOVER-11・RECOVER-12、
//! RLS-9・RLS-10、ERR-1・ERR-2・ERR-4、TABLE-3）。
//!
//! 述語形 `UPDATE`／`DELETE` を「チャンクごとに commit する非原子の分割実行」へ
//! 拡張する際の**永続化層**を担う。実行ループ（チャンクの走査・writer gate の取り直し。
//! #1128）、SQL 構文・部分完了コード・照会／取り消し文・同時実行数の上限（#1129）は
//! 本モジュールの対象外で、これらが乗る土台の API と不変条件だけを提供する。
//!
//! # 呼び出し元・呼び出し先の文脈
//!
//! - 呼び出し元: #1128 の実行器（チャンクの write txn 内で [`record_chunk_progress_in_txn`]
//!   ／[`complete_in_txn`] を呼び、commit は呼び出し元が 1 回だけ行う）、#1129 の
//!   照会・取り消し（[`JobRegistry`]・[`cancel_in_txn`]・[`derive_status`]）、
//!   [`crate::recovery::ledger`]（通常 DML の台帳照合でジョブ表を引く）、
//!   [`crate::catalog::Storage::drop_table`]（[`delete_table_in_txn`]）。
//! - 呼び出し先: [`crate::recovery::ledger::record_partitioned_completion_in_txn`]
//!   （完了時の台帳記録。台帳値のフォーマットは変えない）。
//!
//! # 永続化する状態と導出する状態
//!
//! 永続化するのは [`JobRecord`] の 3 状態（`Running`＝カーソルを持つ・`Cancelled`＝
//! 固定長・`Completed`＝固定長）だけである。照会時の見かけの状態（実行中／中断）は
//! 永続化せず、[`derive_status`] が「`Running` 記録かつ [`JobRegistry`] に登録あり
//! ＝実行中、登録なし＝中断」と導出する。再起動で登録簿が空になるため「再起動後は
//! 中断とみなす（自動再開しない。ADR §9.2）」が追加の書き込みなしに成り立つ。
//!
//! # 不変条件
//!
//! 1. キーのテナントはサーバー側導出（[`crate::policy::PolicyContext::tenant_id`]）に
//!    限る（[`JobKey::for_context`]。security.md P0・RLS-9）。他テナントのジョブは
//!    「記録なし」と区別できない。
//! 2. 台帳エントリがある ⇔ `Completed` 記録がある（完了は同一 txn で両方を書く）。
//! 3. [`PARTITIONED_JOB_ACTIVE_TABLE`] の索引がある ⇔ `Running` 記録がある
//!    （同一 txn で追加・削除する。不整合は [`PartitionedJobError::Corrupted`] で
//!    fail-closed）。索引は「カーソルを持つ記録（実行中＋中断）」の件数を
//!    O(上限) で数えるために持つ（完了・取り消し済みの件数は無制限に増えるため、
//!    主表の範囲走査では数えられない。ADR §15.3）。
//! 4. 行変更・カーソル・件数・台帳・完了記録への縮小は呼び出し元の同一 write txn で
//!    行い、commit は呼び出し元の 1 回（原子性・二重適用／取りこぼしの排除）。
//! 5. 永続値のデコードは fail-closed（未知のバージョン・状態・長さは拒否）。
//!
//! 2 つのテーブル名は `op_ledger` と同じくユーザーテーブルの一覧には現れない。

use std::collections::HashMap;
use std::ops::Bound;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use redb::{ReadableTable, TableDefinition, TableHandle};

use crate::policy::PolicyContext;
use crate::recovery::content_hash::ContentHash;
use crate::recovery::ledger::LedgerRecordError;
use crate::recovery::required_op_id::OperationId;
use crate::storage::StorageError;

/// ジョブ表（Issue #1127・A1）。キーは `(tenant_id, table_name, operation_id)`、
/// 値は [`JobRecord`] の符号化（[`encode_record`]）。
///
/// - `tenant_id` は必ずサーバー側導出（[`JobKey::for_context`]）。
/// - 完了・取り消し済みの記録は削除しない（再送の同一視に使う。ADR §15.3）。
pub(crate) const PARTITIONED_JOB_TABLE: TableDefinition<(&str, &str, &str), &[u8]> =
    TableDefinition::new("partitioned_dml_job");

/// カーソルを持つ記録（`Running`）の索引（Issue #1127・A8）。キーは
/// [`PARTITIONED_JOB_TABLE`] と同じ。値は 1 バイトのバージョンのみ（件数カウンタを
/// 持たないため加減算のずれが起きない）。
pub(crate) const PARTITIONED_JOB_ACTIVE_TABLE: TableDefinition<(&str, &str, &str), &[u8]> =
    TableDefinition::new("partitioned_dml_job_active");

/// 索引値のバージョンバイト。
const ACTIVE_INDEX_VALUE: [u8; 1] = [1];

/// 値フォーマットのバージョン（v1）。未知バージョンは fail-closed に拒否する。
const JOB_FORMAT_VERSION_V1: u8 = 1;
const STATE_RUNNING: u8 = 1;
const STATE_CANCELLED: u8 = 2;
const STATE_COMPLETED: u8 = 3;

/// [`delete_table_in_txn`] が 1 回の走査・削除で保持するキー数の上限
/// （`ledger::DELETE_BATCH_SIZE` と同じ理由）。
const DELETE_BATCH_SIZE: usize = 1024;

/// 本モジュールのエラー。`pub(crate)` で閉じており、公開 enum を変更しない
/// （エラーコードへの写像は #1129 の担当。本 Issue で外へ出るコードは通常 DML 側の
/// `22023` のみ）。メッセージはテナント・テーブル・`operation_id`・件数を含まない。
#[derive(Debug)]
pub(crate) enum PartitionedJobError {
    /// 永続値の破損・不変条件違反・内部の件数あふれ（`XX000` 相当。fail-closed）。
    Corrupted(StorageError),
    /// カーソルを持つ記録の数が上限に達しており、新規作成できない（`54000` 相当。
    /// 呼び出し元は commit せず txn を drop する）。
    InterruptedRecordLimitReached,
    /// 同じ `(tenant, table, operation_id)` のジョブがこのプロセスで実行中
    /// （`55P03` 相当）。登録簿が poisoned の場合も拒否側に倒す。
    AlreadyRunning,
    /// 記録のハッシュ不一致、または期待しない状態（`Running` 以外等）に対する更新要求。
    /// TOCTOU 対策として書き込み直前の再読込で検出し、拒否する。
    StateConflict,
    /// 完了時の台帳記録が既存エントリに衝突した（呼び出し元は commit しない）。
    LedgerConflict(LedgerRecordError),
}

impl<E> From<E> for PartitionedJobError
where
    E: Into<redb::Error>,
{
    fn from(e: E) -> Self {
        PartitionedJobError::Corrupted(StorageError::from(e))
    }
}

impl PartitionedJobError {
    /// 固定文言の破損エラーを作る。
    fn corrupt(msg: &'static str) -> Self {
        PartitionedJobError::Corrupted(StorageError::Codec(msg.to_string()))
    }

    /// 台帳側（[`crate::recovery::ledger`]）が `StorageError` で扱うための変換。
    pub(crate) fn into_storage_error(self) -> StorageError {
        match self {
            PartitionedJobError::Corrupted(e) => e,
            _ => StorageError::Codec("partitioned job state conflict".to_string()),
        }
    }
}

/// ジョブ表・登録簿のキー。テナントはサーバー側導出値に限る。
#[derive(Debug, Clone, Copy)]
pub(crate) struct JobKey<'a> {
    tenant: &'a str,
    table: &'a str,
    op_id: &'a OperationId,
}

impl<'a> JobKey<'a> {
    /// 認証済みコンテキストからテナントを取ってキーを作る。クライアントが申告した
    /// テナントを渡す経路は存在しない（security.md P0・RLS-9）。`table` は
    /// `validate_identifier` 通過済みの論理名であること。
    pub(crate) fn for_context(
        ctx: &'a PolicyContext,
        table: &'a str,
        op_id: &'a OperationId,
    ) -> Self {
        Self {
            tenant: ctx.tenant_id(),
            table,
            op_id,
        }
    }

    /// 台帳の記録経路（[`crate::recovery::ledger`]）専用。台帳側は呼び出し元
    /// （`crate::tenant::*_unchecked`）が既にサーバー側導出した `tenant_id` を
    /// 受け取っているため、同じ値でキーを作る。
    pub(super) fn from_ledger_scope(
        tenant: &'a str,
        table: &'a str,
        op_id: &'a OperationId,
    ) -> Self {
        Self {
            tenant,
            table,
            op_id,
        }
    }

    fn tuple(&self) -> (&'a str, &'a str, &'a str) {
        (self.tenant, self.table, self.op_id.as_str())
    }

    fn owned(&self) -> (String, String, String) {
        (
            self.tenant.to_string(),
            self.table.to_string(),
            self.op_id.as_str().to_string(),
        )
    }
}

/// ジョブ表の永続記録（3 状態。値は固定長・リトルエンディアン）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobRecord {
    /// カーソルを持つ記録（実行中または中断。区別は [`derive_status`]）。
    Running {
        hash: [u8; 32],
        /// 最後に走査した id（下端排他で次チャンクを走査する。`u64::MAX` は取らない）。
        cursor: u64,
        /// commit 済みの処理済み（変更）件数。
        processed: u64,
    },
    /// 取り消し済み（カーソルを持たない固定長。削除しない）。
    Cancelled { hash: [u8; 32], committed: u64 },
    /// 完了（累計件数）。台帳エントリと同じ txn で作る。
    Completed { hash: [u8; 32], total: u64 },
}

impl JobRecord {
    fn hash(&self) -> &[u8; 32] {
        match self {
            JobRecord::Running { hash, .. }
            | JobRecord::Cancelled { hash, .. }
            | JobRecord::Completed { hash, .. } => hash,
        }
    }
}

/// 値の符号化（`[ver][state][hash 32][u64 …]`）。
fn encode_record(record: &JobRecord) -> Vec<u8> {
    let mut buf = Vec::with_capacity(2 + 32 + 16);
    buf.push(JOB_FORMAT_VERSION_V1);
    match record {
        JobRecord::Running {
            hash,
            cursor,
            processed,
        } => {
            buf.push(STATE_RUNNING);
            buf.extend_from_slice(hash);
            buf.extend_from_slice(&cursor.to_le_bytes());
            buf.extend_from_slice(&processed.to_le_bytes());
        }
        JobRecord::Cancelled { hash, committed } => {
            buf.push(STATE_CANCELLED);
            buf.extend_from_slice(hash);
            buf.extend_from_slice(&committed.to_le_bytes());
        }
        JobRecord::Completed { hash, total } => {
            buf.push(STATE_COMPLETED);
            buf.extend_from_slice(hash);
            buf.extend_from_slice(&total.to_le_bytes());
        }
    }
    buf
}

/// 値のデコード。空値・未知のバージョン／状態・長さ不一致・`Running` の
/// `cursor == u64::MAX`（その範囲は走査し尽くしており `Running` のまま残らない）を
/// すべて拒否する（fail-closed。添字アクセス・`unwrap` は使わない）。
fn decode_record(value: &[u8]) -> Result<JobRecord, PartitionedJobError> {
    let bad = || PartitionedJobError::corrupt("partitioned job record is malformed");
    let (version, rest) = value.split_first().ok_or_else(bad)?;
    if *version != JOB_FORMAT_VERSION_V1 {
        return Err(bad());
    }
    let (state, body) = rest.split_first().ok_or_else(bad)?;
    let (hash, tail) = body.split_first_chunk::<32>().ok_or_else(bad)?;
    let hash = *hash;
    match *state {
        STATE_RUNNING => {
            let (cursor, tail) = tail.split_first_chunk::<8>().ok_or_else(bad)?;
            let processed: [u8; 8] = tail.try_into().map_err(|_| bad())?;
            let cursor = u64::from_le_bytes(*cursor);
            if cursor == u64::MAX {
                return Err(bad());
            }
            Ok(JobRecord::Running {
                hash,
                cursor,
                processed: u64::from_le_bytes(processed),
            })
        }
        STATE_CANCELLED => {
            let committed: [u8; 8] = tail.try_into().map_err(|_| bad())?;
            Ok(JobRecord::Cancelled {
                hash,
                committed: u64::from_le_bytes(committed),
            })
        }
        STATE_COMPLETED => {
            let total: [u8; 8] = tail.try_into().map_err(|_| bad())?;
            Ok(JobRecord::Completed {
                hash,
                total: u64::from_le_bytes(total),
            })
        }
        _ => Err(bad()),
    }
}

/// [`chunk_range`] の戻り値（行ストアの `(tenant, id)` キー範囲）。
pub(crate) type ChunkRange<'a> = (Bound<(&'a str, u64)>, Bound<(&'a str, u64)>);

/// 次チャンクの走査範囲（行ストアの `(tenant, id)` キー用。A3）。
///
/// 下端は `cursor` を**排他**にし、`cursor + 1` の加算はしない（`u64::MAX` の id を
/// 取りこぼさず、加算あふれも起こさない）。`cursor == None` は先頭から。
/// `Some(u64::MAX)` は走査済みで `None`（範囲なし）を返す。上端は常に
/// `(tenant, u64::MAX)` の閉区間でテナント境界の内側に収まる。
pub(crate) fn chunk_range(tenant: &str, cursor: Option<u64>) -> Option<ChunkRange<'_>> {
    let lower = match cursor {
        None => Bound::Included((tenant, 0)),
        Some(u64::MAX) => return None,
        Some(c) => Bound::Excluded((tenant, c)),
    };
    Some((lower, Bound::Included((tenant, u64::MAX))))
}

/// 照会用の見かけの状態（[`derive_status`] が導出する。永続化しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobStatus {
    Running { processed: u64 },
    Interrupted { processed: u64, cursor: u64 },
    Cancelled { committed: u64 },
    Completed { total: u64 },
}

/// 永続記録と登録簿から見かけの状態を導出する。記録がなく登録簿にある場合
/// （最初の変更チャンクより前）は「実行中・0 件」。記録も登録もなければ `None`。
pub(crate) fn derive_status(record: Option<&JobRecord>, registered: bool) -> Option<JobStatus> {
    match (record, registered) {
        (None, true) => Some(JobStatus::Running { processed: 0 }),
        (None, false) => None,
        (
            Some(JobRecord::Running {
                cursor, processed, ..
            }),
            registered,
        ) => Some(if registered {
            JobStatus::Running {
                processed: *processed,
            }
        } else {
            JobStatus::Interrupted {
                processed: *processed,
                cursor: *cursor,
            }
        }),
        (Some(JobRecord::Cancelled { committed, .. }), _) => Some(JobStatus::Cancelled {
            committed: *committed,
        }),
        (Some(JobRecord::Completed { total, .. }), _) => {
            Some(JobStatus::Completed { total: *total })
        }
    }
}

/// 再送（同じ `operation_id` での分割実行要求）の判定結果。エラーコードへの写像は
/// #1129 の担当。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResendDecision {
    /// 記録なし。新規ジョブ。
    Fresh,
    /// `Running`（中断）でハッシュ一致。カーソルから再開する。
    Resume { cursor: u64, processed: u64 },
    /// 取り消し済み（ハッシュ一致）。
    Cancelled,
    /// 完了済み（ハッシュ一致）。重複。
    Duplicate,
    /// ハッシュ不一致（内容の異なる誤用）。
    ContentMismatch,
    /// このプロセスで実行中。
    AlreadyRunning,
}

/// 再送の判定（純粋関数）。登録簿に載っていれば最優先で `AlreadyRunning`。
pub(crate) fn classify_resend(
    record: Option<&JobRecord>,
    hash: &ContentHash,
    registry_hit: bool,
) -> ResendDecision {
    if registry_hit {
        return ResendDecision::AlreadyRunning;
    }
    let Some(record) = record else {
        return ResendDecision::Fresh;
    };
    if !hash.matches(record.hash()) {
        return ResendDecision::ContentMismatch;
    }
    match record {
        JobRecord::Running {
            cursor, processed, ..
        } => ResendDecision::Resume {
            cursor: *cursor,
            processed: *processed,
        },
        JobRecord::Cancelled { .. } => ResendDecision::Cancelled,
        JobRecord::Completed { .. } => ResendDecision::Duplicate,
    }
}

/// write txn 内でジョブ記録を引く。テーブルが無ければ（write txn の `open_table`
/// の仕様で）作成される点に注意（台帳あり構成の経路でのみ呼ぶ）。
pub(crate) fn lookup_in_write_txn(
    txn: &redb::WriteTransaction,
    key: &JobKey<'_>,
) -> Result<Option<JobRecord>, PartitionedJobError> {
    let table = txn.open_table(PARTITIONED_JOB_TABLE)?;
    let got = table.get(key.tuple())?;
    got.map(|g| decode_record(g.value())).transpose()
}

/// read txn 内でジョブ記録を引く。テーブルが無ければ `None`（照会用）。
pub(crate) fn lookup_in_read_txn(
    txn: &redb::ReadTransaction,
    key: &JobKey<'_>,
) -> Result<Option<JobRecord>, PartitionedJobError> {
    let table = match txn.open_table(PARTITIONED_JOB_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let got = table.get(key.tuple())?;
    got.map(|g| decode_record(g.value())).transpose()
}

/// `Running` 記録の件数を `(tenant, table)` 単位で数える（索引の範囲走査。
/// `stop_at` 件で打ち切るため計算量は O(上限)）。他テナント・他テーブルは混ざらない。
pub(crate) fn count_interrupted_in_txn(
    txn: &redb::WriteTransaction,
    tenant: &str,
    table: &str,
    stop_at: u64,
) -> Result<u64, PartitionedJobError> {
    let active = txn.open_table(PARTITIONED_JOB_ACTIVE_TABLE)?;
    let lower = Bound::Included((tenant, table, ""));
    let iter = active.range::<(&str, &str, &str)>((lower, Bound::Unbounded))?;
    let mut count: u64 = 0;
    for entry in iter {
        let (k, _v) = entry?;
        let (t, tb, _op) = k.value();
        if t != tenant || tb != table {
            break;
        }
        count = count
            .checked_add(1)
            .ok_or_else(|| PartitionedJobError::corrupt("partitioned job count overflow"))?;
        if count >= stop_at {
            break;
        }
    }
    Ok(count)
}

/// [`record_chunk_progress_in_txn`] の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChunkCommitOutcome {
    /// 記録がなく変更も 0 件のため何も書かなかった（カーソルはメモリ上だけで進める）。
    NoRecord,
    /// 最初の変更チャンクとして `Running` 記録を作った。
    Created,
    /// 既存の `Running` 記録のカーソルと件数を更新した。
    Updated,
}

/// チャンクの行変更と**同じ write txn** の中で、カーソルと処理済み件数を記録する
/// （A2・A8）。commit は呼び出し元（#1128）が 1 回だけ行う。`last_scanned` はこの
/// チャンクで最後に走査した id（`u64::MAX` は走査し尽くした最後のチャンクなので
/// [`complete_in_txn`] を使う）。
///
/// - 記録なしで `rows_changed == 0`: 何も書かない（ジョブ記録は最初に行を変更した
///   チャンクの commit と同時に作る）。
/// - 記録なしで `rows_changed > 0`: 索引を数え、`interrupted_limit` 件に達していれば
///   [`PartitionedJobError::InterruptedRecordLimitReached`]（上限は作成と同じ txn 内で
///   再判定する）。達していなければ `Running` を作り索引を追加する。
/// - `Running` あり: カーソルと件数を更新する。書き込み直前の再読込でハッシュ不一致、
///   `Running` 以外、カーソルが前進しない場合は [`PartitionedJobError::StateConflict`]
///   （fail-closed。TOCTOU 対策）。
pub(crate) fn record_chunk_progress_in_txn(
    txn: &redb::WriteTransaction,
    key: &JobKey<'_>,
    hash: &ContentHash,
    rows_changed: u64,
    last_scanned: u64,
    interrupted_limit: u64,
) -> Result<ChunkCommitOutcome, PartitionedJobError> {
    if last_scanned == u64::MAX {
        // この範囲は走査し尽くしており、`Running` には縮約できない。
        return Err(PartitionedJobError::StateConflict);
    }
    let prior = lookup_in_write_txn(txn, key)?;
    match prior {
        None => {
            if rows_changed == 0 {
                return Ok(ChunkCommitOutcome::NoRecord);
            }
            let existing = count_interrupted_in_txn(txn, key.tenant, key.table, interrupted_limit)?;
            if existing >= interrupted_limit {
                return Err(PartitionedJobError::InterruptedRecordLimitReached);
            }
            let record = JobRecord::Running {
                hash: *hash.as_bytes(),
                cursor: last_scanned,
                processed: rows_changed,
            };
            write_record(txn, key, &record)?;
            let mut active = txn.open_table(PARTITIONED_JOB_ACTIVE_TABLE)?;
            if active
                .insert(key.tuple(), ACTIVE_INDEX_VALUE.as_slice())?
                .is_some()
            {
                return Err(PartitionedJobError::corrupt(
                    "partitioned job active index has a stray entry",
                ));
            }
            Ok(ChunkCommitOutcome::Created)
        }
        Some(JobRecord::Running {
            hash: stored,
            cursor,
            processed,
        }) => {
            if !hash.matches(&stored) || last_scanned <= cursor {
                return Err(PartitionedJobError::StateConflict);
            }
            let processed = processed
                .checked_add(rows_changed)
                .ok_or_else(|| PartitionedJobError::corrupt("partitioned job count overflow"))?;
            let record = JobRecord::Running {
                hash: stored,
                cursor: last_scanned,
                processed,
            };
            write_record(txn, key, &record)?;
            Ok(ChunkCommitOutcome::Updated)
        }
        Some(_) => Err(PartitionedJobError::StateConflict),
    }
}

/// 最後のチャンク（範囲を走査し尽くした）の txn で、台帳エントリを記録し、ジョブ記録を
/// `Completed`（ハッシュ・累計件数）へ縮める（A4）。一致 0 件で走査し尽くした場合も、
/// 記録がない状態から台帳エントリと `Completed`（0 件）を直接作る。戻り値は累計件数。
///
/// 台帳に既存エントリがあれば [`PartitionedJobError::LedgerConflict`]（呼び出し元は
/// commit しない）。`Running` の索引は同じ txn で削除する。
pub(crate) fn complete_in_txn(
    txn: &redb::WriteTransaction,
    key: &JobKey<'_>,
    hash: &ContentHash,
    rows_changed_in_final_chunk: u64,
) -> Result<u64, PartitionedJobError> {
    let prior = lookup_in_write_txn(txn, key)?;
    let (prior_processed, was_running) = match prior {
        None => (0, false),
        Some(JobRecord::Running {
            hash: stored,
            processed,
            ..
        }) if hash.matches(&stored) => (processed, true),
        Some(_) => return Err(PartitionedJobError::StateConflict),
    };
    let total = prior_processed
        .checked_add(rows_changed_in_final_chunk)
        .ok_or_else(|| PartitionedJobError::corrupt("partitioned job count overflow"))?;
    crate::recovery::ledger::record_partitioned_completion_in_txn(
        txn, key.tenant, key.table, key.op_id, hash,
    )
    .map_err(|e| match e {
        LedgerRecordError::Corrupted(se) => PartitionedJobError::Corrupted(se),
        other => PartitionedJobError::LedgerConflict(other),
    })?;
    write_record(
        txn,
        key,
        &JobRecord::Completed {
            hash: *hash.as_bytes(),
            total,
        },
    )?;
    if was_running {
        remove_active_index(txn, key)?;
    }
    Ok(total)
}

/// `Running` を `Cancelled { committed }`（固定長）へ縮め、索引を削除する。記録なし・
/// `Completed`・`Cancelled` のときは何も変更せず `None`。#1129 の取り消し文が使う
/// （本 Issue では API のみ）。取り消し済み記録は削除しない。
pub(crate) fn cancel_in_txn(
    txn: &redb::WriteTransaction,
    key: &JobKey<'_>,
) -> Result<Option<u64>, PartitionedJobError> {
    match lookup_in_write_txn(txn, key)? {
        Some(JobRecord::Running {
            hash, processed, ..
        }) => {
            write_record(
                txn,
                key,
                &JobRecord::Cancelled {
                    hash,
                    committed: processed,
                },
            )?;
            remove_active_index(txn, key)?;
            Ok(Some(processed))
        }
        _ => Ok(None),
    }
}

fn write_record(
    txn: &redb::WriteTransaction,
    key: &JobKey<'_>,
    record: &JobRecord,
) -> Result<(), PartitionedJobError> {
    let mut table = txn.open_table(PARTITIONED_JOB_TABLE)?;
    table.insert(key.tuple(), encode_record(record).as_slice())?;
    Ok(())
}

fn remove_active_index(
    txn: &redb::WriteTransaction,
    key: &JobKey<'_>,
) -> Result<(), PartitionedJobError> {
    let mut active = txn.open_table(PARTITIONED_JOB_ACTIVE_TABLE)?;
    if active.remove(key.tuple())?.is_none() {
        return Err(PartitionedJobError::corrupt(
            "partitioned job active index entry is missing",
        ));
    }
    Ok(())
}

/// `DROP TABLE`（[`crate::catalog::Storage::drop_table`]）と同一 write txn で、対象
/// テーブル名のジョブ記録・索引を全テナント分削除する（同名再作成での旧ジョブ記録の
/// 引き継ぎ防止。`ledger::delete_table_in_txn` と同じ設計判断）。テーブル未作成なら
/// no-op（`list_tables` で存在確認し、`open_table` の自動作成を避ける）。
pub(crate) fn delete_table_in_txn(
    write_txn: &redb::WriteTransaction,
    table: &str,
) -> Result<(), StorageError> {
    let existing: std::collections::HashSet<String> = write_txn
        .list_tables()
        .map_err(StorageError::from)?
        .map(|handle| handle.name().to_string())
        .collect();
    for def in [PARTITIONED_JOB_TABLE, PARTITIONED_JOB_ACTIVE_TABLE] {
        if existing.contains(def.name()) {
            purge_table_rows(write_txn, def, table)?;
        }
    }
    Ok(())
}

/// 3 要素キーの表から `table_name == table` のキーを有界バッチで前方一方向に削除する。
fn purge_table_rows(
    write_txn: &redb::WriteTransaction,
    def: TableDefinition<(&str, &str, &str), &[u8]>,
    table: &str,
) -> Result<(), StorageError> {
    let mut handle = write_txn.open_table(def)?;
    let mut resume_after: Option<(String, String, String)> = None;
    loop {
        let mut keys: Vec<(String, String, String)> = Vec::new();
        let mut reached_limit = false;
        {
            let lower = match resume_after.as_ref() {
                Some((t, tb, op)) => Bound::Excluded((t.as_str(), tb.as_str(), op.as_str())),
                None => Bound::Unbounded,
            };
            let iter = handle.range::<(&str, &str, &str)>((lower, Bound::Unbounded))?;
            for entry in iter {
                let (k, _v) = entry?;
                let (t, tb, op) = k.value();
                if tb == table {
                    keys.push((t.to_string(), tb.to_string(), op.to_string()));
                    if keys.len() >= DELETE_BATCH_SIZE {
                        reached_limit = true;
                        break;
                    }
                }
            }
        }
        resume_after = keys.last().cloned();
        for (t, tb, op) in &keys {
            handle.remove((t.as_str(), tb.as_str(), op.as_str()))?;
        }
        if !reached_limit {
            return Ok(());
        }
    }
}

/// 登録簿の 1 エントリ（取り消し要求フラグ）。#1129 の取り消しが登録簿経由で
/// 実行中のジョブへ伝える受け口。
#[derive(Debug, Default)]
struct JobSlot {
    cancel_requested: AtomicBool,
}

/// プロセス内の実行中ジョブ登録簿（A7）。[`crate::storage::Storage`] が 1 DB ファイル
/// につき 1 つ保持する（writer gate と同じ単位の choke point）。永続化しない（再起動で
/// 空になる。これが [`derive_status`] の「中断」導出の根拠）。同時実行数の上限の
/// 判定は #1129 の担当で、本体は集計 API（[`Self::count_for_tenant`]・[`Self::total`]）
/// だけを提供する。
#[derive(Debug, Default)]
pub(crate) struct JobRegistry {
    inner: Mutex<HashMap<(String, String, String), Arc<JobSlot>>>,
}

impl JobRegistry {
    /// 登録する。既にあれば [`PartitionedJobError::AlreadyRunning`]。mutex が poisoned
    /// の場合も拒否側（fail-closed）に倒す。返す [`JobGuard`] の drop で登録が外れる。
    pub(crate) fn try_register(
        self: &Arc<Self>,
        key: &JobKey<'_>,
    ) -> Result<JobGuard, PartitionedJobError> {
        let mut map = self
            .inner
            .lock()
            .map_err(|_| PartitionedJobError::AlreadyRunning)?;
        let owned = key.owned();
        if map.contains_key(&owned) {
            return Err(PartitionedJobError::AlreadyRunning);
        }
        let slot = Arc::new(JobSlot::default());
        map.insert(owned.clone(), Arc::clone(&slot));
        Ok(JobGuard {
            registry: Arc::clone(self),
            key: owned,
            slot,
        })
    }

    /// 登録済みか（照会用）。poisoned の場合は fail-closed に「登録あり」とする。
    pub(crate) fn is_registered(&self, key: &JobKey<'_>) -> bool {
        match self.inner.lock() {
            Ok(map) => map.contains_key(&key.owned()),
            Err(_) => true,
        }
    }

    /// 実行中ジョブへ取り消しを要求する。登録があれば `true`。
    pub(crate) fn request_cancel(&self, key: &JobKey<'_>) -> bool {
        match self.inner.lock() {
            Ok(map) => match map.get(&key.owned()) {
                Some(slot) => {
                    slot.cancel_requested.store(true, Ordering::SeqCst);
                    true
                }
                None => false,
            },
            Err(_) => false,
        }
    }

    /// テナント単位の実行中ジョブ数（poisoned の場合は `usize::MAX` で上限判定を拒否側へ）。
    pub(crate) fn count_for_tenant(&self, tenant: &str) -> usize {
        match self.inner.lock() {
            Ok(map) => map.keys().filter(|(t, _, _)| t == tenant).count(),
            Err(_) => usize::MAX,
        }
    }

    /// 全体の実行中ジョブ数（poisoned の場合は `usize::MAX`）。
    pub(crate) fn total(&self) -> usize {
        match self.inner.lock() {
            Ok(map) => map.len(),
            Err(_) => usize::MAX,
        }
    }
}

/// 登録の RAII ガード。drop（正常終了・エラー・panic・接続断のいずれでも）で登録が外れる。
#[derive(Debug)]
pub(crate) struct JobGuard {
    registry: Arc<JobRegistry>,
    key: (String, String, String),
    slot: Arc<JobSlot>,
}

impl JobGuard {
    /// 取り消しが要求されているか（実行器がチャンク境界で確認する）。
    pub(crate) fn cancel_requested(&self) -> bool {
        self.slot.cancel_requested.load(Ordering::SeqCst)
    }
}

impl Drop for JobGuard {
    fn drop(&mut self) {
        // poisoned でも解除は安全（エントリを外す側）なので inner を取り出して行う。
        let mut map = match self.registry.inner.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        map.remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::ledger::{self, LedgerWrite};
    use crate::test_util::temp_db::{unique_db_path, CleanupGuard};
    use redb::ReadableDatabase;

    fn op(id: &str) -> OperationId {
        OperationId::parse(id).expect("valid operation_id")
    }

    fn hash(seed: &str) -> ContentHash {
        ContentHash::for_test(seed.as_bytes())
    }

    fn open_db(tag: &str) -> (redb::Database, CleanupGuard) {
        let path = unique_db_path(tag);
        let guard = CleanupGuard(path.clone());
        (redb::Database::create(&path).expect("create db"), guard)
    }

    fn key<'a>(tenant: &'a str, table: &'a str, op_id: &'a OperationId) -> JobKey<'a> {
        JobKey::from_ledger_scope(tenant, table, op_id)
    }

    const ROWS: TableDefinition<(&str, u64), &[u8]> = TableDefinition::new("test_rows");

    fn read_job(db: &redb::Database, k: &JobKey<'_>) -> Option<JobRecord> {
        let rt = db.begin_read().expect("read");
        lookup_in_read_txn(&rt, k).expect("lookup")
    }

    fn active_count(db: &redb::Database) -> usize {
        let rt = db.begin_read().expect("read");
        match rt.open_table(PARTITIONED_JOB_ACTIVE_TABLE) {
            Ok(t) => t.iter().expect("iter").count(),
            Err(_) => 0,
        }
    }

    fn running_count(db: &redb::Database) -> usize {
        let rt = db.begin_read().expect("read");
        match rt.open_table(PARTITIONED_JOB_TABLE) {
            Ok(t) => t
                .iter()
                .expect("iter")
                .filter(|e| {
                    let (_, v) = e.as_ref().expect("entry");
                    matches!(
                        decode_record(v.value()).expect("decode"),
                        JobRecord::Running { .. }
                    )
                })
                .count(),
            Err(_) => 0,
        }
    }

    fn rows_count(db: &redb::Database) -> usize {
        let rt = db.begin_read().expect("read");
        match rt.open_table(ROWS) {
            Ok(t) => t.iter().expect("iter").count(),
            Err(_) => 0,
        }
    }

    // --- 符号化 -------------------------------------------------------------

    #[test]
    fn record_roundtrips_for_every_state() {
        let h = [7u8; 32];
        for r in [
            JobRecord::Running {
                hash: h,
                cursor: 42,
                processed: 9,
            },
            JobRecord::Cancelled {
                hash: h,
                committed: 3,
            },
            JobRecord::Completed { hash: h, total: 11 },
        ] {
            assert_eq!(decode_record(&encode_record(&r)).expect("decode"), r);
        }
    }

    #[test]
    fn decode_rejects_unknown_version_state_and_length() {
        let good = encode_record(&JobRecord::Running {
            hash: [1; 32],
            cursor: 1,
            processed: 1,
        });
        assert!(decode_record(&[]).is_err());
        let mut bad_ver = good.clone();
        bad_ver[0] = 9;
        assert!(decode_record(&bad_ver).is_err());
        let mut bad_state = good.clone();
        bad_state[1] = 9;
        assert!(decode_record(&bad_state).is_err());
        assert!(decode_record(&good[..good.len() - 1]).is_err());
        let mut long = good.clone();
        long.push(0);
        assert!(decode_record(&long).is_err());
        let cancelled = encode_record(&JobRecord::Cancelled {
            hash: [1; 32],
            committed: 1,
        });
        assert!(decode_record(&cancelled[..cancelled.len() - 1]).is_err());
        // Running で cursor == u64::MAX は拒否する。
        let mut max_cursor = good.clone();
        max_cursor[2 + 32..2 + 32 + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode_record(&max_cursor).is_err());
    }

    // --- カーソル範囲 -------------------------------------------------------

    #[test]
    fn chunk_range_excludes_cursor_and_never_adds() {
        let (lo, hi) = chunk_range("t", Some(5)).expect("range");
        assert_eq!(lo, Bound::Excluded(("t", 5)));
        assert_eq!(hi, Bound::Included(("t", u64::MAX)));
        let (lo, _) = chunk_range("t", None).expect("range");
        assert_eq!(lo, Bound::Included(("t", 0)));
        let (lo, _) = chunk_range("t", Some(u64::MAX - 1)).expect("range");
        assert_eq!(lo, Bound::Excluded(("t", u64::MAX - 1)));
    }

    #[test]
    fn chunk_range_at_u64_max_is_exhausted() {
        assert!(chunk_range("t", Some(u64::MAX)).is_none());
    }

    #[test]
    fn chunk_range_scans_row_with_max_id_and_stays_in_tenant() {
        let (db, _g) = open_db("pj-range");
        let wt = db.begin_write().expect("w");
        {
            let mut t = wt.open_table(ROWS).expect("open");
            for (tenant, id) in [("a", 1u64), ("a", u64::MAX), ("b", 0), ("b", 7)] {
                t.insert((tenant, id), b"x".as_slice()).expect("ins");
            }
        }
        wt.commit().expect("commit");
        let rt = db.begin_read().expect("r");
        let t = rt.open_table(ROWS).expect("open");
        let ids = |cursor: Option<u64>| -> Vec<u64> {
            match chunk_range("a", cursor) {
                None => vec![],
                Some(r) => t
                    .range::<(&str, u64)>(r)
                    .expect("range")
                    .map(|e| e.expect("e").0.value().1)
                    .collect(),
            }
        };
        assert_eq!(ids(None), vec![1, u64::MAX]);
        assert_eq!(ids(Some(1)), vec![u64::MAX]);
        assert_eq!(ids(Some(u64::MAX)), Vec::<u64>::new());
    }

    // --- 原子性・カーソル ---------------------------------------------------

    #[test]
    fn chunk_progress_and_rows_roll_back_together_when_txn_dropped() {
        let (db, _g) = open_db("pj-atomic");
        let id = op("op-1");
        let k = key("t1", "docs", &id);
        {
            let wt = db.begin_write().expect("w");
            {
                let mut t = wt.open_table(ROWS).expect("open");
                t.insert(("t1", 1u64), b"x".as_slice()).expect("ins");
            }
            let out = record_chunk_progress_in_txn(&wt, &k, &hash("h"), 1, 1, 10).expect("p");
            assert_eq!(out, ChunkCommitOutcome::Created);
            // commit せず drop。
        }
        assert_eq!(rows_count(&db), 0);
        assert!(read_job(&db, &k).is_none());
        assert_eq!(active_count(&db), 0);

        let wt = db.begin_write().expect("w");
        {
            let mut t = wt.open_table(ROWS).expect("open");
            t.insert(("t1", 1u64), b"x".as_slice()).expect("ins");
        }
        record_chunk_progress_in_txn(&wt, &k, &hash("h"), 1, 1, 10).expect("p");
        wt.commit().expect("commit");
        assert_eq!(rows_count(&db), 1);
        assert!(matches!(
            read_job(&db, &k),
            Some(JobRecord::Running {
                cursor: 1,
                processed: 1,
                ..
            })
        ));
    }

    #[test]
    fn chunk_progress_updates_cursor_and_processed_monotonically() {
        let (db, _g) = open_db("pj-cursor");
        let id = op("op-1");
        let k = key("t1", "docs", &id);
        for (changed, last) in [(3u64, 10u64), (0, 20), (4, 30)] {
            let wt = db.begin_write().expect("w");
            record_chunk_progress_in_txn(&wt, &k, &hash("h"), changed, last, 10).expect("p");
            wt.commit().expect("c");
        }
        assert!(matches!(
            read_job(&db, &k),
            Some(JobRecord::Running {
                cursor: 30,
                processed: 7,
                ..
            })
        ));
        // カーソルが前進しない・ハッシュ不一致は fail-closed。
        let wt = db.begin_write().expect("w");
        assert!(matches!(
            record_chunk_progress_in_txn(&wt, &k, &hash("h"), 1, 30, 10),
            Err(PartitionedJobError::StateConflict)
        ));
        assert!(matches!(
            record_chunk_progress_in_txn(&wt, &k, &hash("other"), 1, 40, 10),
            Err(PartitionedJobError::StateConflict)
        ));
        assert!(matches!(
            record_chunk_progress_in_txn(&wt, &k, &hash("h"), 1, u64::MAX, 10),
            Err(PartitionedJobError::StateConflict)
        ));
    }

    #[test]
    fn no_record_until_first_changing_chunk() {
        let (db, _g) = open_db("pj-first");
        let id = op("op-1");
        let k = key("t1", "docs", &id);
        let wt = db.begin_write().expect("w");
        assert_eq!(
            record_chunk_progress_in_txn(&wt, &k, &hash("h"), 0, 5, 10).expect("p"),
            ChunkCommitOutcome::NoRecord
        );
        wt.commit().expect("c");
        assert!(read_job(&db, &k).is_none());
        let wt = db.begin_write().expect("w");
        assert_eq!(
            record_chunk_progress_in_txn(&wt, &k, &hash("h"), 2, 9, 10).expect("p"),
            ChunkCommitOutcome::Created
        );
        wt.commit().expect("c");
        assert!(matches!(
            read_job(&db, &k),
            Some(JobRecord::Running {
                cursor: 9,
                processed: 2,
                ..
            })
        ));
    }

    // --- 完了 ---------------------------------------------------------------

    #[test]
    fn complete_shrinks_running_to_completed_and_records_ledger() {
        let (db, _g) = open_db("pj-complete");
        let id = op("op-1");
        let k = key("t1", "docs", &id);
        let h = hash("h");
        let wt = db.begin_write().expect("w");
        record_chunk_progress_in_txn(&wt, &k, &h, 3, 10, 10).expect("p");
        wt.commit().expect("c");
        assert_eq!(active_count(&db), 1);

        let wt = db.begin_write().expect("w");
        assert_eq!(complete_in_txn(&wt, &k, &h, 2).expect("complete"), 5);
        wt.commit().expect("c");

        assert!(matches!(
            read_job(&db, &k),
            Some(JobRecord::Completed { total: 5, .. })
        ));
        assert_eq!(active_count(&db), 0);
        let rt = db.begin_read().expect("r");
        assert!(ledger::contains_in_read_txn(&rt, "t1", "docs", &id).expect("contains"));
        assert_eq!(
            ledger::last_operation_in_read_txn(&rt, "t1", "docs").expect("last"),
            ledger::LastOperationRaw::Found(id.clone())
        );
    }

    #[test]
    fn zero_match_exhaustion_creates_ledger_and_completed_record_directly() {
        let (db, _g) = open_db("pj-zero");
        let id = op("op-1");
        let k = key("t1", "docs", &id);
        let wt = db.begin_write().expect("w");
        assert_eq!(complete_in_txn(&wt, &k, &hash("h"), 0).expect("c"), 0);
        wt.commit().expect("c");
        assert!(matches!(
            read_job(&db, &k),
            Some(JobRecord::Completed { total: 0, .. })
        ));
        let rt = db.begin_read().expect("r");
        assert!(ledger::contains_in_read_txn(&rt, "t1", "docs", &id).expect("contains"));
    }

    #[test]
    fn complete_fails_closed_when_ledger_has_foreign_entry() {
        let (db, _g) = open_db("pj-conflict");
        let id = op("op-1");
        let k = key("t1", "docs", &id);
        let wt = db.begin_write().expect("w");
        record_chunk_progress_in_txn(&wt, &k, &hash("h"), 1, 3, 10).expect("p");
        wt.commit().expect("c");
        // 通常 DML 相当が先に同じ operation_id を台帳へ記録していた状況を作る。
        // （Running を通常 DML が拒否する不変条件の外側にある破損状態の再現。）
        let wt = db.begin_write().expect("w");
        ledger::record_partitioned_completion_in_txn(&wt, "t1", "docs", &id, &hash("foreign"))
            .expect("seed");
        wt.commit().expect("c");

        let wt = db.begin_write().expect("w");
        assert!(matches!(
            complete_in_txn(&wt, &k, &hash("h"), 1),
            Err(PartitionedJobError::LedgerConflict(
                LedgerRecordError::ContentMismatch
            ))
        ));
        drop(wt);
        assert!(matches!(read_job(&db, &k), Some(JobRecord::Running { .. })));
    }

    // --- 取り消し・数え方・上限 ---------------------------------------------

    #[test]
    fn cancel_shrinks_to_fixed_record_and_excludes_from_count() {
        let (db, _g) = open_db("pj-cancel");
        let id = op("op-1");
        let k = key("t1", "docs", &id);
        let wt = db.begin_write().expect("w");
        record_chunk_progress_in_txn(&wt, &k, &hash("h"), 4, 8, 10).expect("p");
        wt.commit().expect("c");
        let wt = db.begin_write().expect("w");
        assert_eq!(
            count_interrupted_in_txn(&wt, "t1", "docs", 100).expect("n"),
            1
        );
        assert_eq!(cancel_in_txn(&wt, &k).expect("cancel"), Some(4));
        assert_eq!(
            count_interrupted_in_txn(&wt, "t1", "docs", 100).expect("n"),
            0
        );
        // 2 回目は変更なし。
        assert_eq!(cancel_in_txn(&wt, &k).expect("cancel"), None);
        wt.commit().expect("c");
        let rec = read_job(&db, &k).expect("kept");
        assert!(matches!(rec, JobRecord::Cancelled { committed: 4, .. }));
        assert_eq!(encode_record(&rec).len(), 2 + 32 + 8);
        assert_eq!(active_count(&db), 0);
    }

    #[test]
    fn interrupted_limit_is_rechecked_inside_creating_txn() {
        let (db, _g) = open_db("pj-limit");
        let ids: Vec<OperationId> = (0..3).map(|i| op(&format!("op-{i}"))).collect();
        for id in ids.iter().take(2) {
            let wt = db.begin_write().expect("w");
            record_chunk_progress_in_txn(&wt, &key("t1", "docs", id), &hash("h"), 1, 1, 2)
                .expect("p");
            wt.commit().expect("c");
        }
        let wt = db.begin_write().expect("w");
        {
            let mut t = wt.open_table(ROWS).expect("open");
            t.insert(("t1", 9u64), b"x".as_slice()).expect("ins");
        }
        assert!(matches!(
            record_chunk_progress_in_txn(&wt, &key("t1", "docs", &ids[2]), &hash("h"), 1, 9, 2),
            Err(PartitionedJobError::InterruptedRecordLimitReached)
        ));
        drop(wt);
        assert_eq!(rows_count(&db), 0);
        assert!(read_job(&db, &key("t1", "docs", &ids[2])).is_none());
        // 別テーブル・別テナントは数えない。
        let wt = db.begin_write().expect("w");
        record_chunk_progress_in_txn(&wt, &key("t1", "other", &ids[2]), &hash("h"), 1, 1, 2)
            .expect("other table is independent");
        record_chunk_progress_in_txn(&wt, &key("t2", "docs", &ids[2]), &hash("h"), 1, 1, 2)
            .expect("other tenant is independent");
    }

    #[test]
    fn active_index_count_matches_running_records() {
        let (db, _g) = open_db("pj-index");
        let ids: Vec<OperationId> = (0..5).map(|i| op(&format!("op-{i}"))).collect();
        for id in &ids {
            let wt = db.begin_write().expect("w");
            record_chunk_progress_in_txn(&wt, &key("t1", "docs", id), &hash("h"), 1, 1, 100)
                .expect("p");
            wt.commit().expect("c");
        }
        let wt = db.begin_write().expect("w");
        complete_in_txn(&wt, &key("t1", "docs", &ids[0]), &hash("h"), 1).expect("complete");
        cancel_in_txn(&wt, &key("t1", "docs", &ids[1])).expect("cancel");
        wt.commit().expect("c");
        assert_eq!(active_count(&db), 3);
        assert_eq!(running_count(&db), 3);
    }

    #[test]
    fn count_interrupted_is_scoped_to_tenant_and_table() {
        let (db, _g) = open_db("pj-scope");
        let id = op("op-1");
        let wt = db.begin_write().expect("w");
        for (t, tb) in [("t1", "docs"), ("t1", "other"), ("t2", "docs")] {
            record_chunk_progress_in_txn(&wt, &key(t, tb, &id), &hash("h"), 1, 1, 100).expect("p");
        }
        assert_eq!(
            count_interrupted_in_txn(&wt, "t1", "docs", 100).expect("n"),
            1
        );
        assert_eq!(
            count_interrupted_in_txn(&wt, "t2", "docs", 100).expect("n"),
            1
        );
        assert_eq!(
            count_interrupted_in_txn(&wt, "t3", "docs", 100).expect("n"),
            0
        );
    }

    #[test]
    fn other_tenant_lookup_is_indistinguishable_from_absent() {
        let (db, _g) = open_db("pj-tenant");
        let id = op("op-1");
        let wt = db.begin_write().expect("w");
        record_chunk_progress_in_txn(&wt, &key("t1", "docs", &id), &hash("h"), 1, 1, 100)
            .expect("p");
        wt.commit().expect("c");
        assert!(read_job(&db, &key("t1", "docs", &id)).is_some());
        assert_eq!(read_job(&db, &key("t2", "docs", &id)), None);
        assert_eq!(read_job(&db, &key("t1", "docs2", &id)), None);
    }

    // --- 再送判定・状態導出 -------------------------------------------------

    #[test]
    fn classify_resend_covers_every_branch() {
        let h = hash("h");
        let other = hash("o");
        let running = JobRecord::Running {
            hash: *h.as_bytes(),
            cursor: 5,
            processed: 2,
        };
        let cancelled = JobRecord::Cancelled {
            hash: *h.as_bytes(),
            committed: 2,
        };
        let completed = JobRecord::Completed {
            hash: *h.as_bytes(),
            total: 2,
        };
        assert_eq!(classify_resend(None, &h, false), ResendDecision::Fresh);
        assert_eq!(
            classify_resend(Some(&running), &h, true),
            ResendDecision::AlreadyRunning
        );
        assert_eq!(
            classify_resend(Some(&running), &h, false),
            ResendDecision::Resume {
                cursor: 5,
                processed: 2
            }
        );
        assert_eq!(
            classify_resend(Some(&running), &other, false),
            ResendDecision::ContentMismatch
        );
        assert_eq!(
            classify_resend(Some(&cancelled), &h, false),
            ResendDecision::Cancelled
        );
        assert_eq!(
            classify_resend(Some(&cancelled), &other, false),
            ResendDecision::ContentMismatch
        );
        assert_eq!(
            classify_resend(Some(&completed), &h, false),
            ResendDecision::Duplicate
        );
        assert_eq!(
            classify_resend(Some(&completed), &other, false),
            ResendDecision::ContentMismatch
        );
    }

    #[test]
    fn derive_status_uses_registry_for_running_records() {
        let running = JobRecord::Running {
            hash: [0; 32],
            cursor: 5,
            processed: 2,
        };
        assert_eq!(
            derive_status(Some(&running), true),
            Some(JobStatus::Running { processed: 2 })
        );
        assert_eq!(
            derive_status(Some(&running), false),
            Some(JobStatus::Interrupted {
                processed: 2,
                cursor: 5
            })
        );
        assert_eq!(
            derive_status(None, true),
            Some(JobStatus::Running { processed: 0 })
        );
        assert_eq!(derive_status(None, false), None);
    }

    #[test]
    fn persisted_running_is_reported_interrupted_after_reopen() {
        let path = unique_db_path("pj-reopen");
        let _g = CleanupGuard(path.clone());
        let id = op("op-1");
        {
            let db = redb::Database::create(&path).expect("create");
            let wt = db.begin_write().expect("w");
            record_chunk_progress_in_txn(&wt, &key("t1", "docs", &id), &hash("h"), 2, 6, 10)
                .expect("p");
            wt.commit().expect("c");
        }
        let db = redb::Database::create(&path).expect("reopen");
        let registry = Arc::new(JobRegistry::default());
        let k = key("t1", "docs", &id);
        let rec = read_job(&db, &k).expect("record");
        assert_eq!(
            derive_status(Some(&rec), registry.is_registered(&k)),
            Some(JobStatus::Interrupted {
                processed: 2,
                cursor: 6
            })
        );
        // 自動では何も書かれない（記録は Running のまま）。
        assert!(matches!(read_job(&db, &k), Some(JobRecord::Running { .. })));
    }

    // --- 登録簿 -------------------------------------------------------------

    #[test]
    fn registry_rejects_duplicate_registration() {
        let reg = Arc::new(JobRegistry::default());
        let id = op("op-1");
        let k = key("t1", "docs", &id);
        let _guard = reg.try_register(&k).expect("first");
        assert!(matches!(
            reg.try_register(&k),
            Err(PartitionedJobError::AlreadyRunning)
        ));
        assert!(reg.is_registered(&k));
        assert_eq!(reg.count_for_tenant("t1"), 1);
        assert_eq!(reg.count_for_tenant("t2"), 0);
        assert_eq!(reg.total(), 1);
        // 別テナントの同名は独立。
        let k2 = key("t2", "docs", &id);
        let _g2 = reg.try_register(&k2).expect("other tenant");
    }

    #[test]
    fn registry_guard_unregisters_on_drop() {
        let reg = Arc::new(JobRegistry::default());
        let id = op("op-1");
        let k = key("t1", "docs", &id);
        let guard = reg.try_register(&k).expect("first");
        assert!(!guard.cancel_requested());
        assert!(reg.request_cancel(&k));
        assert!(guard.cancel_requested());
        drop(guard);
        assert!(!reg.is_registered(&k));
        assert!(!reg.request_cancel(&k));
        let _again = reg.try_register(&k).expect("re-register after drop");
    }

    #[test]
    fn registry_guard_unregisters_on_panic() {
        let reg = Arc::new(JobRegistry::default());
        let id = op("op-1");
        let k = key("t1", "docs", &id);
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = reg.try_register(&k).expect("register");
            panic!("simulated executor panic");
        }));
        assert!(r.is_err());
        assert!(!reg.is_registered(&k));
    }

    // --- DROP TABLE ---------------------------------------------------------

    #[test]
    fn drop_table_removes_job_records_for_all_tenants_and_keeps_other_tables() {
        let (db, _g) = open_db("pj-drop");
        let id = op("op-1");
        let wt = db.begin_write().expect("w");
        for (t, tb) in [("t1", "docs"), ("t2", "docs"), ("t1", "keep")] {
            record_chunk_progress_in_txn(&wt, &key(t, tb, &id), &hash("h"), 1, 1, 100).expect("p");
        }
        wt.commit().expect("c");
        let wt = db.begin_write().expect("w");
        delete_table_in_txn(&wt, "docs").expect("purge");
        wt.commit().expect("c");
        assert_eq!(read_job(&db, &key("t1", "docs", &id)), None);
        assert_eq!(read_job(&db, &key("t2", "docs", &id)), None);
        assert!(read_job(&db, &key("t1", "keep", &id)).is_some());
        assert_eq!(active_count(&db), 1);
    }

    #[test]
    fn delete_table_does_not_create_job_tables() {
        let (db, _g) = open_db("pj-drop-noop");
        let wt = db.begin_write().expect("w");
        delete_table_in_txn(&wt, "docs").expect("noop");
        let names: Vec<String> = wt
            .list_tables()
            .expect("list")
            .map(|h| h.name().to_string())
            .collect();
        assert!(names.is_empty());
    }

    // --- 台帳との接続 -------------------------------------------------------

    fn seed_running(db: &redb::Database, id: &OperationId) {
        let wt = db.begin_write().expect("w");
        record_chunk_progress_in_txn(&wt, &key("t1", "docs", id), &hash("job"), 1, 1, 10)
            .expect("p");
        wt.commit().expect("c");
    }

    #[test]
    fn normal_record_rejects_operation_id_of_running_and_cancelled_job() {
        let (db, _g) = open_db("pj-ledger-reject");
        let id = op("op-1");
        seed_running(&db, &id);
        for round in 0..2 {
            let wt = db.begin_write().expect("w");
            let r =
                ledger::record_in_txn(&wt, "t1", "docs", LedgerWrite::Record(&id), &hash("normal"));
            assert!(matches!(r, Err(LedgerRecordError::ContentMismatch)));
            // 台帳エントリは残らない。
            drop(wt);
            let rt = db.begin_read().expect("r");
            assert!(!ledger::contains_in_read_txn(&rt, "t1", "docs", &id).expect("c"));
            if round == 0 {
                let wt = db.begin_write().expect("w");
                cancel_in_txn(&wt, &key("t1", "docs", &id)).expect("cancel");
                wt.commit().expect("c");
            }
        }
        // 別 operation_id・別テーブルは通る。
        let wt = db.begin_write().expect("w");
        let other = op("op-2");
        ledger::record_in_txn(&wt, "t1", "docs", LedgerWrite::Record(&other), &hash("n"))
            .expect("other id");
        ledger::record_in_txn(&wt, "t1", "other", LedgerWrite::Record(&id), &hash("n"))
            .expect("other table");
        ledger::record_in_txn(&wt, "t2", "docs", LedgerWrite::Record(&id), &hash("n"))
            .expect("other tenant");
    }

    #[test]
    fn normal_record_against_completed_job_is_content_mismatch_via_ledger() {
        let (db, _g) = open_db("pj-ledger-completed");
        let id = op("op-1");
        let wt = db.begin_write().expect("w");
        complete_in_txn(&wt, &key("t1", "docs", &id), &hash("job"), 0).expect("complete");
        wt.commit().expect("c");
        let wt = db.begin_write().expect("w");
        let r = ledger::record_in_txn(&wt, "t1", "docs", LedgerWrite::Record(&id), &hash("normal"));
        assert!(matches!(r, Err(LedgerRecordError::ContentMismatch)));
    }

    #[test]
    fn completed_job_without_ledger_entry_is_corrupted() {
        let (db, _g) = open_db("pj-ledger-corrupt");
        let id = op("op-1");
        let wt = db.begin_write().expect("w");
        write_record(
            &wt,
            &key("t1", "docs", &id),
            &JobRecord::Completed {
                hash: [0; 32],
                total: 1,
            },
        )
        .expect("seed");
        wt.commit().expect("c");
        let wt = db.begin_write().expect("w");
        let r = ledger::record_in_txn(&wt, "t1", "docs", LedgerWrite::Record(&id), &hash("n"));
        assert!(matches!(r, Err(LedgerRecordError::Corrupted(_))));
    }

    #[test]
    fn disabled_ledger_does_not_create_job_table() {
        let (db, _g) = open_db("pj-ledger-disabled");
        let wt = db.begin_write().expect("w");
        let out = ledger::record_in_txn(&wt, "t1", "docs", LedgerWrite::Disabled, &hash("n"))
            .expect("skip");
        assert_eq!(out, ledger::RecordOutcome::Skipped);
        let names: Vec<String> = wt
            .list_tables()
            .expect("list")
            .map(|h| h.name().to_string())
            .collect();
        assert!(!names.iter().any(|n| n == PARTITIONED_JOB_TABLE.name()));
    }

    #[test]
    fn ledger_lookup_in_write_txn_reports_presence() {
        let (db, _g) = open_db("pj-ledger-lookup");
        let id = op("op-1");
        let wt = db.begin_write().expect("w");
        assert!(!ledger::lookup_in_write_txn(&wt, "t1", "docs", &id).expect("l"));
        ledger::record_in_txn(&wt, "t1", "docs", LedgerWrite::Record(&id), &hash("n")).expect("r");
        assert!(ledger::lookup_in_write_txn(&wt, "t1", "docs", &id).expect("l"));
        assert!(!ledger::lookup_in_write_txn(&wt, "t2", "docs", &id).expect("l"));
    }
}
