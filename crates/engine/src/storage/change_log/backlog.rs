//! 未アーカイブの変更ログ量の上限と、超過時の書き込み拒否（RECOVER-14 (2)・Issue #3091）。
//!
//! アーカイブが滞る（アーカイブ先の障害・容量不足・書き込みゲートの飢餓）と、水位（`archived_lsn`）より
//! 後のレコードが `change_log` 表に際限なく溜まる。本モジュールは上限（件数・バイト）を測り、
//! 「この commit を加える前」の量が上限以上なら、[`super::CaptureSession::prepare`] が
//! `StorageError::CommitFootprintUnrecordable`（`wire_code` は `54000`）で commit 前に拒否できるようにする。
//! 拒否は durable な書き込みの前に起こるので、世代・レコード・データは確定しない（fail-closed）。
//! アーカイブが水位を進めれば、次の commit から自動で再開する。
//!
//! # 測り方（prepare で O(1)）
//!
//! - 件数: LSN は連続し 1 commit が 1 レコードなので、`(g_c - 1) - 水位` で求める（状態を持たない）
//! - バイト: メタ表の `unarchived_bytes`（LSN が `(水位, stamp]` の行の値の長さの合計）と、
//!   合計が何番目の LSN まで数えたかを示す `unarchived_bytes_lsn`（stamp）。利用者 commit が行を
//!   足す同じトランザクションで更新し、保守 commit が水位を進めるときに書き出した本体バイト数を引く。
//!   stamp が無い・不正・`g_c - 1` と合わない（prepare を通らない経路で行が増えた・復元・上限を
//!   設定していなかった期間）ときは `(水位, g_c - 1]` を走査して数え直す
//!
//! 上限を設定していない（既定の）`Storage` は本モジュールを呼ばず、メタ表を読みも書きもしない。
//!
//! # 呼び出し元
//!
//! - `CaptureSession::prepare`（[`admit`]・[`account`]）: 利用者の commit の前段
//! - `archive::maintenance_commit`（[`on_watermark_advance`]）・`archive::run_cycle`（[`at_limit`]）:
//!   アーカイブの保守 commit。上限に達しているときは書き込みゲートを譲らず通常の書き込みと並んで取る
//!   （拒否された書き手の再試行が続いても、水位を進める保守 commit が飢えないようにする）
//! - `Storage::change_log_unarchived_status`: 運用者向けの照会（テナントのセッションには出さない）
//!
//! 他テナントの書き込み量を推測させないため、クライアントへのエラーは他の拒否と同じ `54000` の
//! 固定文言のままで、件数・LSN は stderr にも出さない。

use std::sync::atomic::{AtomicBool, Ordering};

use redb::ReadableTable;

use super::{
    ChangeLogState, Unrecordable, CHANGE_LOG_TABLE, META_KEY_ARCHIVED_LSN,
    META_KEY_BASE_GENERATION, META_TABLE,
};
use crate::storage::{Result, StorageError};

/// バイトの合計（u64 LE）。
const META_KEY_UNARCHIVED_BYTES: &str = "unarchived_bytes";
/// [`META_KEY_UNARCHIVED_BYTES`] が数えた最後の LSN（u64 LE）。
const META_KEY_UNARCHIVED_BYTES_LSN: &str = "unarchived_bytes_lsn";

/// 未アーカイブ量の上限。`None` の軸は上限なし。両方 `None`（既定）なら上限は無効。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UnarchivedLimit {
    /// 未アーカイブのレコード件数の上限（1 以上）。
    pub max_records: Option<u64>,
    /// 未アーカイブのレコード値の合計バイト数の上限（1 以上）。
    pub max_bytes: Option<u64>,
}

impl UnarchivedLimit {
    /// どの軸にも上限が無いか。
    pub fn is_unlimited(&self) -> bool {
        self.max_records.is_none() && self.max_bytes.is_none()
    }

    /// 0 の上限は常に拒否になり回復できないため、設定として受け付けない。
    pub(crate) fn validate(&self) -> Result<()> {
        if self.max_records == Some(0) || self.max_bytes == Some(0) {
            return Err(StorageError::Codec(
                "unarchived change log limit must be at least 1".to_string(),
            ));
        }
        Ok(())
    }
}

/// 運用者向けの未アーカイブ量の状態（`Storage::change_log_unarchived_status`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnarchivedStatus {
    /// 未アーカイブのレコード件数。
    pub records: u64,
    /// 未アーカイブのバイト数（バイト上限を設定しているときだけ）。
    pub bytes: Option<u64>,
    /// 設定されている上限。
    pub limit: UnarchivedLimit,
    /// 上限に達していて、書き込みが拒否される状態か。
    pub at_limit: bool,
}

/// 直近の commit が上限で拒否されたことの印。`ChangeLogState` の導出に合わせ、比較は常に等しく、
/// 複製は立っていない新しい値になる（`ArchiveLock` と同じ作法）。
#[derive(Debug, Default)]
pub(crate) struct RejectFlag(AtomicBool);

impl Clone for RejectFlag {
    fn clone(&self) -> Self {
        Self::default()
    }
}

impl PartialEq for RejectFlag {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl Eq for RejectFlag {}

impl RejectFlag {
    fn raise(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// 水位を進める保守 commit が成功したら下ろす（まだ超過なら次の拒否で再び立つ）。
    pub(crate) fn lower(&self) {
        self.0.store(false, Ordering::Relaxed);
    }

    fn is_raised(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

fn corrupt() -> StorageError {
    StorageError::Codec("invalid change log metadata".to_string())
}

/// メタ表の u64 LE の値。長さが 8 でなければ `None`（不正値は「無い」と同じく数え直しに回す）。
fn read_u64(
    meta: &impl ReadableTable<&'static str, &'static [u8]>,
    key: &str,
) -> Result<Option<u64>> {
    Ok(meta
        .get(key)?
        .and_then(|v| <[u8; 8]>::try_from(v.value()).ok())
        .map(u64::from_le_bytes))
}

/// 水位。`archived_lsn`、無ければ初回有効化時の世代。値の長さが不正なら fail-closed に `Err`。
fn watermark(meta: &impl ReadableTable<&'static str, &'static [u8]>) -> Result<u64> {
    for key in [META_KEY_ARCHIVED_LSN, META_KEY_BASE_GENERATION] {
        if let Some(v) = meta.get(key)? {
            let b: [u8; 8] = v.value().try_into().map_err(|_| corrupt())?;
            return Ok(u64::from_le_bytes(b));
        }
    }
    Err(corrupt())
}

/// stamp が `expect_last` と一致するときの合計バイト数。
fn valid_counter(
    meta: &impl ReadableTable<&'static str, &'static [u8]>,
    expect_last: u64,
) -> Result<Option<u64>> {
    if read_u64(meta, META_KEY_UNARCHIVED_BYTES_LSN)? != Some(expect_last) {
        return Ok(None);
    }
    read_u64(meta, META_KEY_UNARCHIVED_BYTES)
}

/// `(w, last]` の行の値の長さの合計（数え直し）。
fn sum_bytes(log: &impl ReadableTable<u64, &'static [u8]>, w: u64, last: u64) -> Result<u64> {
    let Some(first) = w.checked_add(1) else {
        return Ok(0);
    };
    if first > last {
        return Ok(0);
    }
    let mut total: u64 = 0;
    for row in log.range(first..=last)? {
        let (_, v) = row?;
        let len = u64::try_from(v.value().len()).map_err(|_| corrupt())?;
        total = total.checked_add(len).ok_or_else(corrupt)?;
    }
    Ok(total)
}

/// 利用者 commit の前段の判定。この commit を加える**前**の未アーカイブ量が上限以上なら拒否する
/// （「加えた後」で判定すると、上限より大きい 1 レコードが空の backlog でも通らなくなるため。
/// 超過は最大 1 レコード分に収まる）。`g_c` は採番済みの世代（= このレコードの LSN）。
pub(super) fn admit(
    write_txn: &redb::WriteTransaction,
    state: &ChangeLogState,
    g_c: u64,
) -> Result<()> {
    let limit = &state.unarchived_limit;
    if limit.is_unlimited() {
        return Ok(());
    }
    let last = g_c
        .checked_sub(1)
        .ok_or_else(|| Unrecordable::Clock.refuse())?;
    // 許可（clippy::disallowed_methods）: メタ表と本体表は記録範囲の例外（D3）で、捕捉の対象外。
    #[allow(clippy::disallowed_methods)]
    let meta = write_txn.open_table(META_TABLE)?;
    let w = watermark(&meta)?;
    let mut full = limit
        .max_records
        .is_some_and(|m| last.saturating_sub(w) >= m);
    if let (false, Some(max)) = (full, limit.max_bytes) {
        let bytes = match valid_counter(&meta, last)? {
            Some(b) => b,
            None => {
                #[allow(clippy::disallowed_methods)]
                let log = write_txn.open_table(CHANGE_LOG_TABLE)?;
                sum_bytes(&log, w, last)?
            }
        };
        full = bytes >= max;
    }
    if full {
        state.rejected.raise();
        return Err(Unrecordable::UnarchivedLimit.refuse());
    }
    Ok(())
}

/// レコードを `change_log` 表へ足した直後（同じトランザクション）にバイトの合計へ加算する。
/// stamp が 1 つ前の LSN と合わなければ、足した行を含めて数え直す。
pub(super) fn account(
    write_txn: &redb::WriteTransaction,
    state: &ChangeLogState,
    g_c: u64,
    record_len: usize,
) -> Result<()> {
    if state.unarchived_limit.max_bytes.is_none() {
        return Ok(());
    }
    let prev = g_c
        .checked_sub(1)
        .ok_or_else(|| Unrecordable::Clock.refuse())?;
    let len = u64::try_from(record_len).map_err(|_| corrupt())?;
    #[allow(clippy::disallowed_methods)]
    let mut meta = write_txn.open_table(META_TABLE)?;
    let incremental = valid_counter(&meta, prev)?.and_then(|b| b.checked_add(len));
    let total = match incremental {
        Some(t) => t,
        None => {
            let w = watermark(&meta)?;
            #[allow(clippy::disallowed_methods)]
            let log = write_txn.open_table(CHANGE_LOG_TABLE)?;
            sum_bytes(&log, w, g_c)?
        }
    };
    meta.insert(META_KEY_UNARCHIVED_BYTES, total.to_le_bytes().as_slice())?;
    meta.insert(META_KEY_UNARCHIVED_BYTES_LSN, g_c.to_le_bytes().as_slice())?;
    Ok(())
}

/// 有効化時に、バイトの合計を現在の行から数え直して書く（`enable_change_log_with_unarchived_limit` が
/// 保守 commit で呼ぶ）。復元・レプリカ・機能導入前の DB でも、最初の判定が正しい値で行われる。
pub(crate) fn recount_into(write_txn: &redb::WriteTransaction) -> Result<()> {
    #[allow(clippy::disallowed_methods)]
    let mut meta = write_txn.open_table(META_TABLE)?;
    let w = watermark(&meta)?;
    #[allow(clippy::disallowed_methods)]
    let log = write_txn.open_table(CHANGE_LOG_TABLE)?;
    let last = log.last()?.map_or(w, |(k, _)| k.value()).max(w);
    let total = sum_bytes(&log, w, last)?;
    meta.insert(META_KEY_UNARCHIVED_BYTES, total.to_le_bytes().as_slice())?;
    meta.insert(META_KEY_UNARCHIVED_BYTES_LSN, last.to_le_bytes().as_slice())?;
    Ok(())
}

/// 保守 commit が水位を `new_w` へ進めるときの整合。`body_bytes` は書き出したセグメントの本体
/// バイト数（レコード値の連結なので、合計から引く値と一致する）。stamp が `new_w` 未満・バイト上限が
/// 無い・引き算が下に溢れるときは、不整合を残さないよう両キーを削除する（次の判定で数え直す）。
pub(super) fn on_watermark_advance(
    meta: &mut redb::Table<'_, &'static str, &'static [u8]>,
    state: &ChangeLogState,
    new_w: u64,
    body_bytes: u64,
) -> Result<()> {
    let Some(stamp) = read_u64(meta, META_KEY_UNARCHIVED_BYTES_LSN)? else {
        return Ok(());
    };
    let remaining = if state.unarchived_limit.max_bytes.is_some() && stamp >= new_w {
        read_u64(meta, META_KEY_UNARCHIVED_BYTES)?.and_then(|b| b.checked_sub(body_bytes))
    } else {
        None
    };
    match remaining {
        Some(r) => {
            meta.insert(META_KEY_UNARCHIVED_BYTES, r.to_le_bytes().as_slice())?;
        }
        None => {
            meta.remove(META_KEY_UNARCHIVED_BYTES)?;
            meta.remove(META_KEY_UNARCHIVED_BYTES_LSN)?;
        }
    }
    Ok(())
}

/// 読み取りトランザクションから、現在の未アーカイブの件数・バイトと、上限に達しているかを求める。
fn measure(
    read: &redb::ReadTransaction,
    state: &ChangeLogState,
) -> Result<(u64, Option<u64>, bool)> {
    let meta = match read.open_table(META_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok((0, None, false)),
        Err(e) => return Err(e.into()),
    };
    let w = watermark(&meta)?;
    let log = match read.open_table(CHANGE_LOG_TABLE) {
        Ok(t) => Some(t),
        Err(redb::TableError::TableDoesNotExist(_)) => None,
        Err(e) => return Err(e.into()),
    };
    let last = match &log {
        Some(t) => t.last()?.map_or(w, |(k, _)| k.value()).max(w),
        None => w,
    };
    let records = last.saturating_sub(w);
    let limit = &state.unarchived_limit;
    let bytes = match (limit.max_bytes, &log) {
        (None, _) => None,
        (Some(_), None) => Some(0),
        (Some(_), Some(t)) => Some(match valid_counter(&meta, last)? {
            Some(b) => b,
            None => sum_bytes(t, w, last)?,
        }),
    };
    let full = limit.max_records.is_some_and(|m| records >= m)
        || matches!((limit.max_bytes, bytes), (Some(m), Some(b)) if b >= m);
    Ok((records, bytes, full))
}

/// 上限に達している（または直近の commit を拒否した）か。`run_cycle` が保守 commit のゲートの取り方を
/// 選ぶのに使う。上限が無効なら常に偽。
pub(super) fn at_limit(read: &redb::ReadTransaction, state: &ChangeLogState) -> Result<bool> {
    if state.unarchived_limit.is_unlimited() {
        return Ok(false);
    }
    if state.rejected.is_raised() {
        return Ok(true);
    }
    Ok(measure(read, state)?.2)
}

/// 運用者向けの状態。
pub(crate) fn status(
    read: &redb::ReadTransaction,
    state: &ChangeLogState,
) -> Result<UnarchivedStatus> {
    let (records, bytes, full) = measure(read, state)?;
    Ok(UnarchivedStatus {
        records,
        bytes,
        limit: state.unarchived_limit,
        at_limit: !state.unarchived_limit.is_unlimited() && (full || state.rejected.is_raised()),
    })
}

#[cfg(test)]
mod tests;
