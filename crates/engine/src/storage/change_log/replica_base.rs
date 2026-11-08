//! レプリカのベースの連続性検査（Issue #2176・親 #2171・ポインタ: RECOVER-15・RECOVER-13。方式の記録は
//! `docs/design/replication-streaming.md` の「実装判断（Issue #2176）」）。
//!
//! # 役割と責務境界
//!
//! 再同期（ベースバックアップからのレプリカの再構築）で、ベースの DB が「変更ログの受信へ番号の連続で
//! 厳密につながる」ことを確かめる。wire-server の `replica-resync`（新しいベースの検査）と、レプリカの
//! 適用ループの開始時（[`Storage::check_replica_base`]）の 2 か所から呼ばれる。
//!
//! 検査の内容は変更ログの `verify_continuity` と同じ規則（最後の LSN は `change_log` 表の末尾、表が空なら
//! アーカイブ水位、水位も無ければ初回有効化時の世代）で、それが `storage_generation` と一致することを
//! 求める。一致しなければ、変更ログを無効にしたまま書かれた区間（記録のない区間）があり、プライマリの
//! 送信の `applied + 1` から続けても履歴が欠ける。返す [`ChainPoint`] が切り替え点（これ以降をストリームで
//! 受ける）である。
//!
//! エラー文言は固定の英語で、LSN・パスを含まない。DB ファイルは読み取り専用で開き、書き込まない。

use std::path::Path;

use redb::{ReadableDatabase, ReadableTable};

use super::archive::read_meta_u64;
use super::incremental::{read_identity, ChainPoint, IncrementalError};
use super::replica_apply::ReplicaApplyError;
use super::{CHANGE_LOG_TABLE, META_KEY_ARCHIVED_LSN, META_KEY_BASE_GENERATION};
use crate::storage::Storage;

/// 読み取りトランザクション上で連続性を判定し、切り替え点を返す。変更ログのメタ情報が無ければ
/// `NotEnabled`、最後の LSN が世代と一致しなければ `ChainBroken`。
fn check_in_read(read: &redb::ReadTransaction) -> Result<ChainPoint, IncrementalError> {
    let (db_id, timeline) = read_identity(read)?;
    let generation = crate::storage::current_generation_in_txn(read)?;
    // 識別子はあるが起点が無いのは想定外の不整合。fail-closed に拒否する。
    let base =
        read_meta_u64(read, META_KEY_BASE_GENERATION)?.ok_or(IncrementalError::ChainBroken)?;
    let archived = read_meta_u64(read, META_KEY_ARCHIVED_LSN)?;
    let last = match read.open_table(CHANGE_LOG_TABLE) {
        Ok(t) => t
            .last()
            .map_err(|e| IncrementalError::from(crate::storage::StorageError::from(e)))?
            .map(|(k, _)| k.value()),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(crate::storage::StorageError::from(e).into()),
    };
    if last.or(archived).unwrap_or(base) != generation {
        return Err(IncrementalError::ChainBroken);
    }
    Ok(ChainPoint {
        db_id,
        timeline,
        end_lsn: generation,
    })
}

/// ベース `path`（フルバックアップの DB ファイル）を読み取り専用で開き、変更ログの連続性を検査して
/// 切り替え点を返す。再同期の CLI が、公開前の一時ファイルに対して呼ぶ。書き込み用に開かれている DB は
/// `InUse`、変更ログを有効にしていない DB は `NotEnabled`、連続しない DB は `ChainBroken`。
pub fn replica_base_point_of(path: &Path) -> Result<ChainPoint, IncrementalError> {
    let meta = std::fs::metadata(path).map_err(|_| IncrementalError::BaseUnreadable)?;
    if !meta.is_file() {
        return Err(IncrementalError::BaseUnreadable);
    }
    let db = redb::ReadOnlyDatabase::open(path).map_err(|e| match e {
        redb::DatabaseError::DatabaseAlreadyOpen => IncrementalError::InUse,
        _ => IncrementalError::BaseUnreadable,
    })?;
    let read = db
        .begin_read()
        .map_err(|e| IncrementalError::from(crate::storage::StorageError::from(e)))?;
    check_in_read(&read)
}

impl Storage {
    /// 開いているレプリカ DB の変更ログの連続性を検査し、切り替え点を返す。レプリカの適用ループが開始時に
    /// 呼ぶ（`replica` サブコマンドと読み取りレプリカモードの両方）。適用の条件
    /// （[`Storage::apply_replicated_records`] と同じベースの条件）を満たさなければ `NotReplicaBase`、
    /// 連続しなければ `BaseDiscontinuous`。
    pub fn check_replica_base(&self) -> Result<ChainPoint, ReplicaApplyError> {
        if self.temp_scoped || self.change_log.is_some() || !self.change_log_on_disk {
            return Err(ReplicaApplyError::NotReplicaBase);
        }
        let read = self.db.begin_read()?;
        check_in_read(&read).map_err(|e| match e {
            IncrementalError::NotEnabled => ReplicaApplyError::NotReplicaBase,
            IncrementalError::Storage(s) => ReplicaApplyError::Storage(s),
            _ => ReplicaApplyError::BaseDiscontinuous,
        })
    }
}

#[cfg(test)]
mod tests;
