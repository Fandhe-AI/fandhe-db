//! 復元の世代番号（RECOVER-17・RECOVER-13・TASK-340・親 Issue #2446。実装は Issue #2448。
//! ポインタ記録は `docs/design/restore-generation.md`、契約は RECOVER-17）の永続化と進行。
//!
//! 責務境界: データベース全体で 1 つの値（テナント情報を含まない）を専用の redb 表に持ち、
//! [`Storage::restore_generation`] で読み、[`Storage::advance_restore_generation`] で進める。
//! 呼び出し元は wire-server の `restore`（物理・増分・PITR が共通で通る完了点。#2450）で、
//! 論理の復元（#2451）も完了点で同じ API を呼ぶ予定。セッションからの参照（#2449）は読み取り側を使い、
//! 構文は `SHOW restore_generation`（engine の `core.rs`）。
//!
//! 復元をまたぐ高水位（Issue #3105）は `Storage::advance_restore_generation_above` が呼び出し元から
//! 受け取る下限として扱い、状態ファイルの形式は [`high_water`] が持つ（置き場所は設計ドキュメント）。
//!
//! 値が壊れている・欠けている・上限に達した場合は黙って補わず `Err` とする（fail-closed）。
//! 書き込みは変更ログの記録に関与しない保守 commit で行う。

use redb::{ReadableDatabase, ReadableTable as _, TableDefinition, TableHandle as _};

use super::{Result, Storage, StorageError};

/// 復元の世代番号を持つ redb 表の名前。
pub(crate) const RESTORE_GENERATION_TABLE_NAME: &str = "restore_generation";

const TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new(RESTORE_GENERATION_TABLE_NAME);
const KEY: &str = "generation";
/// 値のバイト数（u64 のリトルエンディアン）。
const VALUE_LEN: usize = 8;

/// 世代の上限（`bigint` として公開できる最大値）。
pub(crate) const MAX_RESTORE_GENERATION: u64 = i64::MAX as u64;

fn invalid() -> StorageError {
    StorageError::Codec("invalid restore generation".to_string())
}

/// 保存された値を検査して世代にする。長さ不正・上限超過は拒否する。
fn decode(bytes: &[u8]) -> Result<u64> {
    let arr: [u8; VALUE_LEN] = bytes.try_into().map_err(|_| invalid())?;
    let v = u64::from_le_bytes(arr);
    if v > MAX_RESTORE_GENERATION {
        return Err(invalid());
    }
    Ok(v)
}

/// 次の世代を決める純粋関数。上限に達している・下限が範囲外の場合は `Err`（丸めない）。
pub(crate) fn next_generation(current: u64, floor: u64) -> Result<u64> {
    if current > MAX_RESTORE_GENERATION {
        return Err(invalid());
    }
    // 下限（時計由来）が範囲外なのは世代の上限到達ではなく時計の異常なので、文言を分ける。
    if floor > MAX_RESTORE_GENERATION {
        return Err(StorageError::Codec(
            "system clock is out of range".to_string(),
        ));
    }
    let bumped = current
        .checked_add(1)
        .filter(|v| *v <= MAX_RESTORE_GENERATION)
        .ok_or_else(|| StorageError::Codec("restore generation limit reached".to_string()))?;
    Ok(bumped.max(floor))
}

/// 現在時刻に基づく下限。時計が取れない・範囲外の場合は `Err`。
fn clock_floor() -> Result<u64> {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| StorageError::Codec("system clock is before the epoch".to_string()))?;
    u64::try_from(d.as_micros())
        .map_err(|_| StorageError::Codec("system clock is out of range".to_string()))
}

impl Storage {
    /// 復元の世代番号を読む（RECOVER-17）。表が無ければ 0（新規作成した DB）。表があるのに値が欠けて
    /// いる・壊れている場合は `Err`（fail-closed）。全テナント共通の値で、呼び出し元のテナントに依らない。
    pub fn restore_generation(&self) -> Result<u64> {
        let read = self.db.begin_read()?;
        read_generation(&read)
    }

    /// 復元の世代番号を進めて永続化し、新しい値を返す（RECOVER-17。復元の完了点で 1 回だけ呼ぶ）。
    ///
    /// - 変更ログを有効にした DB を有効にしないまま開いた復元先でも書ける（保守 commit。`storage_generation`
    ///   も LSN も動かさず、変更ログのレコードも生まない）
    /// - 変更ログを有効にした `Storage` と一時表用ストレージでは拒否する（復元先の確定前の DB にだけ使う）
    /// - 失敗したときは何も書かない
    /// - 保証するのは新しい値が元の値より大きいこと。同じ元からの繰り返し復元での単調な増加は、復元間で
    ///   共有する高水位を渡す [`Self::advance_restore_generation_above`] が担う（Issue #3105。高水位を
    ///   使わないこの経路は時計の下限だけが頼りで、時計が戻ると増えない場合がある）
    pub fn advance_restore_generation(&mut self) -> Result<u64> {
        self.advance_restore_generation_with_floor(clock_floor()?)
    }

    /// [`Self::advance_restore_generation`] の下限指定版（時計に依存しないテストが使う）。
    pub(crate) fn advance_restore_generation_with_floor(&mut self, floor: u64) -> Result<u64> {
        if self.temp_scoped || self.change_log.is_some() {
            return Err(StorageError::Codec(
                "restore generation can only be advanced on a closed restore target".to_string(),
            ));
        }
        // 変更ログを有効にしたことのある DB を有効にしないまま開いた復元先は、通常の書き込みを
        // 拒否される。この保守 commit は記録対象を持たないので、`enable_change_log` と同じく
        // 検査を一時的に外し、結果にかかわらず戻す。
        let guard = std::mem::replace(&mut self.change_log_on_disk, false);
        let result = self.advance_in_txn(floor);
        self.change_log_on_disk = guard;
        result
    }

    fn advance_in_txn(&self, floor: u64) -> Result<u64> {
        let txn = self.begin_write_txn()?;
        // `open_table` は未存在の表を作成してしまうため、作成前に存在を確かめる。0 を許すのは表が
        // 無かった場合（新規作成した DB）だけで、既存の表からキーが欠けた状態は拒否する（fail-closed）。
        let table_existed = txn
            .list_tables()?
            .any(|handle| handle.name() == RESTORE_GENERATION_TABLE_NAME);
        // 許可（clippy::disallowed_methods）: 本表は復元の世代専用で、変更ログの捕捉・表世代の対象外。
        #[allow(clippy::disallowed_methods)]
        let mut table = txn.open_table(TABLE)?;
        let stored: Option<Vec<u8>> = table.get(KEY)?.map(|v| v.value().to_vec());
        let current = match stored {
            Some(bytes) => decode(&bytes)?,
            None if !table_existed => 0,
            None => return Err(invalid()),
        };
        let next = next_generation(current, floor)?;
        table.insert(KEY, next.to_le_bytes().as_slice())?;
        drop(table);
        crate::recovery::commit_boundary::commit_maintenance(txn)?;
        Ok(next)
    }
}

impl Storage {
    /// 復元をまたぐ高水位 `high_water`（これまでに発行した世代の最大値）より大きい値へ世代を進める
    /// （Issue #3105。wire-server の `restore --generation-state` が状態ファイルから読んだ値を渡す）。
    /// 新しい値は `max(保存値 + 1, 時計の下限, high_water + 1)`。`high_water` が上限以上なら `Err`（丸めない）。
    /// それ以外の条件は [`Self::advance_restore_generation`] と同じ。
    pub fn advance_restore_generation_above(&mut self, high_water: u64) -> Result<u64> {
        self.advance_restore_generation_above_with_floor(high_water, clock_floor()?)
    }

    /// [`Self::advance_restore_generation_above`] の時計下限指定版（時計に依存しないテストが使う）。
    pub(crate) fn advance_restore_generation_above_with_floor(
        &mut self,
        high_water: u64,
        floor: u64,
    ) -> Result<u64> {
        let above = high_water
            .checked_add(1)
            .filter(|v| *v <= MAX_RESTORE_GENERATION)
            .ok_or_else(|| StorageError::Codec("restore generation limit reached".to_string()))?;
        self.advance_restore_generation_with_floor(floor.max(above))
    }
}

/// 読み取りトランザクションから世代を読む（表が無ければ 0）。
fn read_generation(read: &redb::ReadTransaction) -> Result<u64> {
    let table = match read.open_table(TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(0),
        Err(e) => return Err(e.into()),
    };
    match table.get(KEY)? {
        Some(v) => decode(v.value()),
        None => Err(invalid()),
    }
}

pub(crate) mod high_water;

#[cfg(test)]
mod tests;
