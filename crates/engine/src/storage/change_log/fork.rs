//! 復元の完了時に変更ログの timeline を分岐させる（Issue #3100・ポインタ: RECOVER-14・RECOVER-17。
//! 設計は `docs/design/change-log-pitr.md` の「D5 の実装判断（Issue #3100）」）。
//!
//! # 役割と責務境界
//!
//! 物理の復元（ベースのみ・増分・PITR）で作った DB は、元の DB と同じ DB 識別子・同じ timeline の
//! ままで LSN だけが巻き戻る。両方に書き込みが続くと、同じ (DB 識別子, timeline, LSN) で中身の違う
//! レコードが 2 系統できる。本モジュールは、復元の世代番号を進める保守 commit
//! （`restore_generation` の `advance_in_txn`。wire-server の `restore` が共通で通る完了点）から呼ばれ、
//! 同じトランザクションで timeline を新しい値へ上げ、起点を分岐点へ作り直す。これにより、
//! 元の系統と復元した系統の増分・アーカイブは timeline が違うので、既存の連鎖検査
//! （`ForeignTimeline`・アーカイブ検証）が DB に触れる前に拒否する。
//!
//! # 規則
//!
//! - 新しい timeline は `max(新しい復元の世代, 旧 timeline + 1)`（`checked_add`。溢れたら `Err`）
//! - `change_log` 表の行（旧 timeline のヘッダを持つ）はすべて削除し、メタの `timeline` を新しい値、
//!   `base_generation` と `archived_lsn` を分岐点（`storage_generation`）へ書き換える。DB 識別子と
//!   `storage_generation` は変えない
//! - 書き換える前に [`super::verify_continuity`] と同じ規則で連続性を確かめ、満たさなければ `Err`
//!   （起点の作り直しで記録の欠けを隠さない。fail-closed）
//! - 変更ログを一度も有効にしていない DB（メタ表が無い）では何もしない。`open_table` は表を作って
//!   DB を「変更ログ必須」に変えてしまうため、存在は `list_tables` で確かめる
//! - レプリカの再同期（`replica_resync`）はプライマリの timeline を保つ必要があるため、本関数を呼ばない
//! - 増分・PITR の適用は「ベースと同じ timeline」を要求するので、分岐は必ず適用と整合検査の後に行う

use redb::{ReadableTable as _, TableHandle as _};

use super::{
    CHANGE_LOG_META_TABLE_NAME, CHANGE_LOG_TABLE, CHANGE_LOG_TABLE_NAME, DB_ID_LEN,
    META_KEY_ARCHIVED_LSN, META_KEY_BASE_GENERATION, META_KEY_DB_ID, META_KEY_TIMELINE, META_TABLE,
};
use crate::storage::{Result, StorageError, GENERATION_KEY, GENERATION_TABLE};

fn invalid() -> StorageError {
    StorageError::Codec("invalid change log identity".to_string())
}

fn broken() -> StorageError {
    StorageError::Codec("change log continuity broken".to_string())
}

fn u64_of(bytes: &[u8]) -> Result<u64> {
    Ok(u64::from_le_bytes(bytes.try_into().map_err(|_| invalid())?))
}

/// 分岐後の timeline を決める純粋関数。`max(generation, old + 1)`。溢れたら `Err`（丸めない）。
pub(crate) fn next_timeline(old: u64, generation: u64) -> Result<u64> {
    let bumped = old
        .checked_add(1)
        .ok_or_else(|| StorageError::Codec("change log timeline limit reached".to_string()))?;
    Ok(bumped.max(generation))
}

/// 書き込みトランザクション上で timeline を分岐させる。変更ログのメタが無ければ `Ok(None)`、
/// 分岐したら `Ok(Some(新しい timeline))`。失敗したときは呼び出し側がトランザクションを commit せず
/// 破棄するので、何も書かれない。
pub(crate) fn fork_timeline_in_txn(
    txn: &redb::WriteTransaction,
    generation: u64,
) -> Result<Option<u64>> {
    let names: Vec<String> = txn.list_tables()?.map(|h| h.name().to_string()).collect();
    let has = |n: &str| names.iter().any(|x| x == n);
    if !has(CHANGE_LOG_META_TABLE_NAME) {
        return Ok(None);
    }
    // `open_table` は未存在の表を作ってしまうため、世代の表・本体表は存在を確かめてから開く。
    let storage_generation = if has(GENERATION_TABLE.name()) {
        // 許可（clippy::disallowed_methods）: 読むだけで書かない。
        #[allow(clippy::disallowed_methods)]
        let t = txn.open_table(GENERATION_TABLE)?;
        let v = t.get(GENERATION_KEY)?.map(|v| v.value()).unwrap_or(0);
        v
    } else {
        0
    };
    // 許可（clippy::disallowed_methods）: メタ表・本体表は記録範囲の例外（D3）で、捕捉の対象外。
    #[allow(clippy::disallowed_methods)]
    let mut meta = txn.open_table(META_TABLE)?;
    if !meta
        .get(META_KEY_DB_ID)?
        .is_some_and(|v| v.value().len() == DB_ID_LEN)
    {
        return Err(invalid());
    }
    let old_timeline = match meta.get(META_KEY_TIMELINE)? {
        Some(v) => u64_of(v.value())?,
        None => return Err(invalid()),
    };
    let base = match meta.get(META_KEY_BASE_GENERATION)? {
        Some(v) => u64_of(v.value())?,
        None => return Err(broken()),
    };
    let archived = match meta.get(META_KEY_ARCHIVED_LSN)? {
        Some(v) => Some(u64_of(v.value())?),
        None => None,
    };
    let has_log = has(CHANGE_LOG_TABLE_NAME);
    let last = if has_log {
        #[allow(clippy::disallowed_methods)]
        let log = txn.open_table(CHANGE_LOG_TABLE)?;
        let l = log.last()?.map(|(k, _)| k.value());
        l
    } else {
        None
    };
    if last.or(archived).unwrap_or(base) != storage_generation {
        return Err(broken());
    }
    let new_timeline = next_timeline(old_timeline, generation)?;
    meta.insert(META_KEY_TIMELINE, new_timeline.to_le_bytes().as_slice())?;
    let point = storage_generation.to_le_bytes();
    meta.insert(META_KEY_BASE_GENERATION, point.as_slice())?;
    meta.insert(META_KEY_ARCHIVED_LSN, point.as_slice())?;
    drop(meta);
    // 許可（clippy::disallowed_methods）: 変更ログの表自体の作り直し（記録範囲の例外。D3）。
    #[allow(clippy::disallowed_methods)]
    {
        if has_log {
            txn.delete_table(CHANGE_LOG_TABLE)?;
        }
        txn.open_table(CHANGE_LOG_TABLE)?;
    }
    Ok(Some(new_timeline))
}

#[cfg(test)]
mod tests;
