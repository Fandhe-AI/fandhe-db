//! レプリカ側の変更ログ適用（Issue #2174・ポインタ: RECOVER-15。方式の記録は
//! `docs/design/replication-streaming.md`、レコード形式は `docs/design/change-log-pitr.md` の D3）。
//!
//! # 役割と責務境界
//!
//! wire-server のレプリカ受信ループ（`wire_server::replica`）が、プライマリのストリーミング送信側から
//! 受け取ったレコード（`change_log` 表の値そのまま）を、レプリカ自身の DB へ **commit 番号（LSN）順に
//! 原子的に**適用する。接続・フレーム・再接続・終了コードは wire-server が担い、engine は
//! 「受け取った列が適用先に続くか」の検査と適用だけを担う。
//!
//! - **適用位置 = `storage_generation`**。1 レコードは 1 commit で、本体に世代表の更新を含む。適用・
//!   `change_log` 表への追記・世代の更新が同一 redb トランザクションで確定するため、別の位置表は持たず、
//!   再開位置は [`Storage::replica_applied_lsn`] を読むだけで得られる（クラッシュ一貫性は commit が担保）
//! - **レプリカは変更ログを有効にしないまま開く**（`Storage::open` のみ）。ベースのコピーは変更ログを
//!   一度有効にした DB なので、通常の書き込みは `54000`（`DisabledOnEnabledDb`）で拒否される。
//!   適用だけが専用の入口（`Storage::begin_replica_apply_txn`）を通る
//! - **バッチ原子性**: 渡された列は 1 つの書き込みトランザクションで適用する。1 件でも不整合なら
//!   全体を abort し、何も適用しない（読み手には常にコミット境界の状態だけが見える）
//! - **冪等**: 適用済み（`lsn <=` 開始時の世代）のレコードは、`change_log` 表の同じ LSN の行と
//!   バイト一致なら読み飛ばし、不一致は分岐として [`ReplicaApplyError::Diverged`] で停止する
//! - **fail-closed**: 欠番・順序違反・`prev_lsn` 不一致・フレームの LSN とヘッダの不一致・DB 識別子／
//!   timeline／版数の不一致・チェックサム不正・適用不能なレコードは適用せず停止する
//!
//! 再利用: 1 レコード分の検査と本体の適用は `incremental`（ファイル再生）と共通の関数を使う。
//! エラー文言は固定の英語で、LSN・表名・行値・パスを含まない。durability は `Immediate` の保守 commit
//! （`recovery::commit_boundary::commit_maintenance`）で、呼び出し側は commit 後に ack を送る。

use std::collections::BTreeSet;

use redb::{ReadableDatabase, ReadableTable};

use super::incremental::{
    append_log_row, apply_body, check_record_chain, read_identity, IncrementalError,
};
use super::stream::{StreamIdentity, MAX_RECORD_BYTES};
use super::{
    decode_record, is_excluded_table, CaptureLimits, CHANGE_LOG_TABLE, STORAGE_FORMAT_VERSION,
};
use crate::storage::{Storage, StorageError, GENERATION_KEY, GENERATION_TABLE};

/// 1 回の適用で受け付けるレコード数の上限（untrusted な入力列向けの資源上限。実装既定値）。
pub const MAX_APPLY_RECORDS: usize = 4096;

/// レプリカ適用の失敗。`Display` は固定の英語文言で、LSN・表名・行値・パスを含まない。
#[derive(Debug)]
#[non_exhaustive]
pub enum ReplicaApplyError {
    /// 適用先がレプリカのベースではない（変更ログのメタ情報が無い、または変更ログを有効にして開いている）。
    NotReplicaBase,
    /// 別の DB 識別子または timeline のレコード。
    Foreign,
    /// 順序違反（欠番・重複の順序入れ替え・`prev_lsn` 不一致・フレームの LSN とヘッダの不一致）。
    OutOfOrder,
    /// レコードの破損（形式不正・チェックサム不一致・サイズ上限超過・世代の不一致）。
    Corrupt,
    /// レコードのストレージ形式の版数が、この実装の版数と異なる。
    UnsupportedFormat,
    /// レコードを適用できない（未知の shape・存在しない表への変更・記録範囲外の表など）。
    BadRecord,
    /// 適用済みの LSN のレコードが、適用先の同じ LSN の内容と一致しない（分岐した履歴）。
    Diverged,
    /// 1 回に渡されたレコード数が上限を超える。
    BatchTooLarge,
    /// ストレージ層のエラー。
    Storage(StorageError),
}

impl std::fmt::Display for ReplicaApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotReplicaBase => "database is not a replica base",
            Self::Foreign => "replicated record belongs to another database or timeline",
            Self::OutOfOrder => "replicated record is out of order",
            Self::Corrupt => "replicated record is corrupt",
            Self::UnsupportedFormat => "replicated record storage format is not supported",
            Self::BadRecord => "replicated record cannot be applied",
            Self::Diverged => "replicated record diverges from the applied history",
            Self::BatchTooLarge => "too many replicated records in one batch",
            Self::Storage(_) => "storage error during replica apply",
        })
    }
}

impl std::error::Error for ReplicaApplyError {}

impl From<StorageError> for ReplicaApplyError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

impl<E: Into<redb::Error>> From<E> for ReplicaApplyError {
    fn from(e: E) -> Self {
        Self::Storage(StorageError::Backend(e.into()))
    }
}

/// [`IncrementalError`]（共通の検査・適用関数が返す型）を、レプリカ適用の分類へ写す。
/// 判別できないものはストレージ層のエラーではなく、fail-closed に `BadRecord` とする。
fn map_inc(e: IncrementalError) -> ReplicaApplyError {
    match e {
        IncrementalError::NotEnabled => ReplicaApplyError::NotReplicaBase,
        IncrementalError::ChainBroken => ReplicaApplyError::OutOfOrder,
        IncrementalError::ForeignDatabase | IncrementalError::ForeignTimeline => {
            ReplicaApplyError::Foreign
        }
        IncrementalError::UnsupportedFormat => ReplicaApplyError::UnsupportedFormat,
        IncrementalError::Segment(_) => ReplicaApplyError::Corrupt,
        IncrementalError::Storage(s) => ReplicaApplyError::Storage(s),
        _ => ReplicaApplyError::BadRecord,
    }
}

type Result<T> = std::result::Result<T, ReplicaApplyError>;

/// [`Storage::apply_replicated_records`] の結果（件数と位置のみ。行の値・表名は含めない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicaApplyReport {
    /// 新たに適用したレコード数。
    pub applied: u64,
    /// 適用済みのため読み飛ばした重複レコード数。
    pub skipped_duplicates: u64,
    /// 適用後の適用済み LSN（`storage_generation`）。
    pub last_lsn: u64,
}

impl Storage {
    /// 適用済みの最後の LSN（`storage_generation`）。再接続・再起動後の再開位置（Start に載せる値）で、
    /// 常に DB から取り直す。
    pub fn replica_applied_lsn(&self) -> std::result::Result<u64, ReplicaApplyError> {
        let read = self.db.begin_read()?;
        Ok(crate::storage::current_generation_in_txn(&read)?)
    }

    /// レプリカの DB 識別子と timeline（メタ表から読む。変更ログを有効にしていないレプリカ用）。
    /// プライマリの Identity と照合して、別系統のログを受けないために使う。
    pub fn replica_identity(&self) -> std::result::Result<StreamIdentity, ReplicaApplyError> {
        let read = self.db.begin_read()?;
        let (db_id, timeline) = read_identity(&read).map_err(map_inc)?;
        Ok(StreamIdentity { db_id, timeline })
    }

    /// 受信したレコード列 `(フレームの LSN, レコードのバイト列)` を、適用先の世代に続けて 1 つの書き込み
    /// トランザクションで適用する。1 件でも不整合なら何も適用しない（abort）。成功時は
    /// `Immediate` の commit で確定してから戻る（呼び出し側はその後に ack を送る）。
    pub fn apply_replicated_records(
        &self,
        records: &[(u64, Vec<u8>)],
    ) -> Result<ReplicaApplyReport> {
        // レプリカのベースは「変更ログのメタ情報を持ち、変更ログを有効にしていない」DB に限る。
        if self.temp_scoped || self.change_log.is_some() || !self.change_log_on_disk {
            return Err(ReplicaApplyError::NotReplicaBase);
        }
        if records.len() > MAX_APPLY_RECORDS {
            return Err(ReplicaApplyError::BatchTooLarge);
        }
        let max_payload = CaptureLimits::default().max_payload_bytes;
        let txn = self.begin_replica_apply_txn()?;
        // 書き込みゲートを保持した状態で読むので、確定済みの状態は txn の開始状態と一致する。
        let (db_id, timeline, base_generation, mut names) = {
            let read = self.db.begin_read()?;
            let (db_id, timeline) = read_identity(&read).map_err(map_inc)?;
            let generation = crate::storage::current_generation_in_txn(&read)?;
            let names: BTreeSet<String> = read
                .list_tables()?
                .map(|h| redb::TableHandle::name(&h).to_string())
                .filter(|n| !is_excluded_table(n))
                .collect();
            (db_id, timeline, generation, names)
        };
        let mut current = base_generation;
        let mut applied = 0u64;
        let mut skipped = 0u64;
        // 受信列の直前 LSN。適用位置とは別に、重複区間を含めて列全体の連続性（+1 刻み）を検査する。
        let mut prev_received: Option<u64> = None;
        for (frame_lsn, raw) in records {
            if raw.len() > MAX_RECORD_BYTES {
                return Err(ReplicaApplyError::Corrupt);
            }
            let rec = decode_record(raw, max_payload).map_err(|_| ReplicaApplyError::Corrupt)?;
            let h = rec.header;
            if h.lsn != *frame_lsn {
                return Err(ReplicaApplyError::OutOfOrder);
            }
            if let Some(p) = prev_received {
                if p.checked_add(1) != Some(h.lsn) {
                    return Err(ReplicaApplyError::OutOfOrder);
                }
            }
            prev_received = Some(h.lsn);
            if h.lsn <= base_generation {
                // 適用済みの区間の再送。内容が一致すれば無害、食い違えば分岐。
                check_duplicate(&txn, raw, &h, &db_id, timeline)?;
                skipped = skipped.saturating_add(1);
                // 再送は先頭側に限る（新規適用の後に現れた適用済みの LSN は順序違反）。
                if applied > 0 {
                    return Err(ReplicaApplyError::OutOfOrder);
                }
                continue;
            }
            check_record_chain(&h, &db_id, timeline, current).map_err(map_inc)?;
            apply_body(&txn, &mut names, rec.body).map_err(map_inc)?;
            append_log_row(&txn, h.lsn, raw).map_err(map_inc)?;
            // 本体が世代表を含み、世代が LSN に一致していること（欠けていれば fail-closed）。
            if generation_in_txn(&txn)? != h.lsn {
                return Err(ReplicaApplyError::Corrupt);
            }
            current = h.lsn;
            applied = applied.saturating_add(1);
        }
        if applied == 0 {
            txn.abort()?;
        } else {
            crate::recovery::commit_boundary::commit_maintenance(txn)?;
        }
        Ok(ReplicaApplyReport {
            applied,
            skipped_duplicates: skipped,
            last_lsn: current,
        })
    }
}

/// 書き込みトランザクション内の世代（`storage_generation`）。
fn generation_in_txn(txn: &redb::WriteTransaction) -> Result<u64> {
    // 許可（clippy::disallowed_methods）: 世代表の読み取りだけ（書かない）。
    #[allow(clippy::disallowed_methods)]
    let t = txn.open_table(GENERATION_TABLE)?;
    let v = t.get(GENERATION_KEY)?.map(|v| v.value()).unwrap_or(0);
    Ok(v)
}

/// 適用済みの LSN のレコード `raw`（ヘッダ `h`）が、適用先の `change_log` 表の同じ LSN の行と
/// バイト一致することを確かめる。行が無い（刈り込み済み）ときは照合の基準が無いので、ヘッダの
/// DB 識別子・timeline・版数・`prev_lsn == lsn - 1` だけを確かめる（`incremental::check_overlap` と同じ方針）。
fn check_duplicate(
    txn: &redb::WriteTransaction,
    raw: &[u8],
    h: &super::RecordHeader,
    db_id: &[u8; super::DB_ID_LEN],
    timeline: u64,
) -> Result<()> {
    // 許可（clippy::disallowed_methods）: 本体表は記録範囲の例外（D3）で、読み取りだけ。
    #[allow(clippy::disallowed_methods)]
    let logged: Option<Vec<u8>> = match txn.open_table(CHANGE_LOG_TABLE) {
        Ok(t) => t.get(h.lsn)?.map(|v| v.value().to_vec()),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(e.into()),
    };
    match logged {
        Some(row) if row == raw => Ok(()),
        Some(_) => Err(ReplicaApplyError::Diverged),
        None => {
            if h.db_id != *db_id || h.timeline != timeline {
                return Err(ReplicaApplyError::Foreign);
            }
            if h.storage_format != STORAGE_FORMAT_VERSION {
                return Err(ReplicaApplyError::UnsupportedFormat);
            }
            if h.lsn.checked_sub(1) != Some(h.prev_lsn) {
                return Err(ReplicaApplyError::OutOfOrder);
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
