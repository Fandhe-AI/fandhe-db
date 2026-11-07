//! 変更ログのストリーミング送信用の読み出し口（Issue #2173・RECOVER-15 ポインタ。方式の記録は
//! `docs/design/replication-streaming.md`、レコード形式は `docs/design/change-log-pitr.md` の D3）。
//!
//! 責務境界: wire-server のレプリケーション送信側（`wire_server::replication`）が、接続中のレプリカへ
//! 送るレコード（`change_log` 表の値そのまま）を読む。engine は「LSN の連続した区間の取り出し」と
//! 「レプリカが確認応答した位置より後を刈り込みから守る保持位置」だけを担い、認証・フレーム・
//! バックプレッシャ・接続数は wire-server が担う。読み取りトランザクションしか取らないため、
//! 送信（レプリカの遅延・停止を含む）が書き込みを止めることは構造的にない。
//!
//! 開始位置が刈り込み済み（アーカイブへ移った）区間なら [`StreamError::Pruned`] で拒否する。
//! アーカイブからの読み出しと再同期は #2176 の担当。

use std::sync::Arc;

use redb::ReadableDatabase;

use super::archive::check_row;
use super::{CaptureLimits, ChangeLogState, CHANGE_LOG_TABLE, DB_ID_LEN, HEADER_LEN};
use crate::storage::{Storage, StorageError};

/// 1 レコードの最大バイト数（ヘッダ・チェックサム込み）。ペイロード長の上限（捕捉側の上限と同じ）に
/// 固定部を足した値で、受信側（レプリカ）がフレームの長さを確保前に検証する基準になる。
pub const MAX_RECORD_BYTES: usize = 256 * 1024 * 1024 + HEADER_LEN + 32;

/// ストリーミング読み出しの失敗。`Display` は固定文言で、LSN・件数・表名を含めない。
#[derive(Debug)]
pub enum StreamError {
    /// 変更ログを有効にしていない `Storage`（または一時表用）。
    NotEnabled,
    /// 要求した開始位置のレコードが `change_log` 表に無い（刈り込み済み、または有効化より前）。
    Pruned,
    /// 要求した開始位置がプライマリの最新より先にある（別系統・巻き戻されたプライマリ）。
    Ahead,
    /// `change_log` 表の側に欠落・破損がある（fail-closed に送らない）。
    Corrupt,
    /// ストレージ層のエラー。
    Storage(StorageError),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotEnabled => "change log is not enabled",
            Self::Pruned => "requested start position is no longer available",
            Self::Ahead => "requested start position is ahead of the primary",
            Self::Corrupt => "change log table is inconsistent",
            Self::Storage(_) => "storage error during change log read",
        })
    }
}

impl std::error::Error for StreamError {}

impl From<redb::StorageError> for StreamError {
    fn from(e: redb::StorageError) -> Self {
        Self::Storage(e.into())
    }
}

impl From<redb::TableError> for StreamError {
    fn from(e: redb::TableError) -> Self {
        Self::Storage(e.into())
    }
}

impl From<redb::TransactionError> for StreamError {
    fn from(e: redb::TransactionError) -> Self {
        Self::Storage(e.into())
    }
}

impl From<StorageError> for StreamError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

type Result<T> = std::result::Result<T, StreamError>;

/// DB 識別子と timeline（レプリカが別系統のログを受けないための照合値）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamIdentity {
    pub db_id: [u8; DB_ID_LEN],
    pub timeline: u64,
}

/// [`Storage::read_change_log_from`] の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeLogBatch {
    /// `(LSN, レコードのバイト列)`。LSN は要求した開始位置から欠番なく昇順に連続する。
    pub records: Vec<(u64, Vec<u8>)>,
    /// 読み取った時点のプライマリの最新 LSN。
    pub current_lsn: u64,
}

/// レプリカの保持位置の登録（RAII）。保持している間、確認応答済みの位置より後のレコードは刈り込まれない。
/// `Drop` で登録が外れる（切断したレプリカが刈り込みを止め続けない）。
#[derive(Debug)]
pub struct ReplicaPin {
    state: Arc<ChangeLogState>,
    id: u64,
}

impl ReplicaPin {
    /// 確認応答済みの位置を `acked_lsn` へ進める（後退させない）。
    pub fn advance(&self, acked_lsn: u64) {
        let mut pins = self.state.replica_pins.lock();
        if let Some(slot) = pins.get_mut(&self.id) {
            if acked_lsn > *slot {
                *slot = acked_lsn;
            }
        }
    }
}

impl Drop for ReplicaPin {
    fn drop(&mut self) {
        self.state.replica_pins.lock().remove(&self.id);
    }
}

impl Storage {
    /// ストリーミングの照合値（DB 識別子・timeline）。変更ログが有効でなければ `None`。
    pub fn change_log_stream_identity(&self) -> Option<StreamIdentity> {
        if self.temp_scoped {
            return None;
        }
        self.change_log.as_ref().map(|s| StreamIdentity {
            db_id: s.db_id,
            timeline: s.timeline,
        })
    }

    /// `from_lsn` から始まる連続したレコードを、件数 `max_records` とバイト数 `max_bytes` の上限まで読む
    /// （最低 1 件は上限に関わらず返す。`max_records` が 0 のときは空で、最新 LSN だけを返す）。`from_lsn` が最新の次ならレコードは空
    /// （追いついている）。最新より先なら [`StreamError::Ahead`]、`change_log` 表に無ければ
    /// [`StreamError::Pruned`]。各行はアーカイブと同じ検査（キーとヘッダの LSN・prev・DB 識別子・timeline・
    /// チェックサム）を通ってから返す（壊れた行は [`StreamError::Corrupt`]）。
    pub fn read_change_log_from(
        &self,
        from_lsn: u64,
        max_records: usize,
        max_bytes: usize,
    ) -> Result<ChangeLogBatch> {
        if self.temp_scoped {
            return Err(StreamError::NotEnabled);
        }
        let state = self.change_log.as_ref().ok_or(StreamError::NotEnabled)?;
        let read = self.db.begin_read()?;
        let current = crate::storage::current_generation_in_txn(&read)?;
        let next_after_current = current.checked_add(1).ok_or(StreamError::Corrupt)?;
        if from_lsn > next_after_current {
            return Err(StreamError::Ahead);
        }
        if from_lsn == next_after_current || max_records == 0 {
            return Ok(ChangeLogBatch {
                records: Vec::new(),
                current_lsn: current,
            });
        }
        let log = match read.open_table(CHANGE_LOG_TABLE) {
            Ok(t) => t,
            Err(redb::TableError::TableDoesNotExist(_)) => return Err(StreamError::Pruned),
            Err(e) => return Err(e.into()),
        };
        let max_payload = CaptureLimits::default().max_payload_bytes;
        let mut records: Vec<(u64, Vec<u8>)> = Vec::new();
        let mut bytes = 0usize;
        let mut expect = from_lsn;
        let mut format: Option<u32> = None;
        for entry in log.range(from_lsn..=current)? {
            if max_records == 0 || records.len() >= max_records {
                break;
            }
            let (k, v) = entry?;
            let (key, value) = (k.value(), v.value());
            if key != expect {
                // 範囲の先頭が無いのは刈り込み済み、途中の欠番は破損。
                return Err(if records.is_empty() {
                    StreamError::Pruned
                } else {
                    StreamError::Corrupt
                });
            }
            if !records.is_empty() && bytes.saturating_add(value.len()) > max_bytes {
                break;
            }
            check_row(
                key,
                value,
                expect,
                &state.db_id,
                state.timeline,
                &mut format,
                max_payload,
            )
            .map_err(|_| StreamError::Corrupt)?;
            bytes = bytes.saturating_add(value.len());
            records.push((key, value.to_vec()));
            expect = expect.checked_add(1).ok_or(StreamError::Corrupt)?;
        }
        if records.is_empty() && max_records > 0 {
            // `from_lsn <= current` なのに先頭の行が無い。
            return Err(StreamError::Pruned);
        }
        Ok(ChangeLogBatch {
            records,
            current_lsn: current,
        })
    }

    /// レプリカの保持位置を登録する。`applied_lsn` はレプリカが適用済みの最後の LSN で、その次
    /// （`applied_lsn + 1`）から送れること（`change_log` 表に在る、または最新の次）を確かめてから返す。
    /// 返した [`ReplicaPin`] を保持する間、確認応答済みの位置より後のレコードは刈り込まれない。
    ///
    /// 登録してから存在を確かめる順序にして、刈り込みの保守 commit（登録簿のロックを commit まで
    /// 保持する）との競合で、確かめた範囲が直後に消える隙間を作らない。
    pub fn pin_change_log_for_replica(&self, applied_lsn: u64) -> Result<ReplicaPin> {
        if self.temp_scoped {
            return Err(StreamError::NotEnabled);
        }
        let state = Arc::clone(self.change_log.as_ref().ok_or(StreamError::NotEnabled)?);
        let next = applied_lsn.checked_add(1).ok_or(StreamError::Ahead)?;
        let id = state
            .replica_pins
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        state.replica_pins.lock().insert(id, applied_lsn);
        let pin = ReplicaPin { state, id };
        // 存在確認だけが目的で、レコードは 1 件読んで捨てる。
        self.read_change_log_from(next, 1, usize::MAX)?;
        Ok(pin)
    }
}

#[cfg(test)]
mod tests;
