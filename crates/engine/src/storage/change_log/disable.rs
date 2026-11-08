//! 変更ログの無効化（Issue #3093・ポインタ: RECOVER-14。設計は `docs/design/change-log-pitr.md` の D13）。
//!
//! # 役割と責務境界
//!
//! 一度 `--change-log` で有効にした DB を、運用者の明示操作で「変更ログを一度も有効にしていない DB」へ
//! 戻す。`change_log` 表とメタ表（`change_log_meta`。DB 識別子・timeline・起点の世代・アーカイブ水位）を
//! 1 本の保守 commit で表ごと削除する。世代は進めずレコードも生まない。表のデータ・復元世代の表・
//! アーカイブ先のファイルには触れない。
//!
//! 呼び出し元は wire-server の運用者向けサブコマンド `change-log-disable`（構築段階の `&mut Storage`。
//! `EngineCore` 構築後・稼働中のサーバーからは到達できない）。有効化（`Storage::enable_change_log`）の対になる。
//!
//! # fail-closed
//!
//! - 判定（未アーカイブ分の有無）と削除は同じ書き込みトランザクションで行い、間に競合の窓を作らない
//! - 水位より後（未アーカイブ）のレコードが在るときは既定で拒否する。破棄は
//!   [`DisableOptions::discard_unarchived`] の明示でだけ行う
//! - 識別子を残したまま起点だけ消える中間状態は作らない（両表を 1 commit で消す）。crash しても
//!   「有効のまま」か「無効化済み」のどちらかになる
//! - 「無効化済み」の印は置かない。無効化後は識別子が無いので、増分バックアップ・PITR・検証は既存の
//!   `NotEnabled` 経路でそのまま拒否される（印の読み落としによる fail-open を作らない）
//! - エラーの `Display` は固定文言で、LSN・件数・パス・表名を含めない

use super::{
    CHANGE_LOG_TABLE, META_KEY_ARCHIVED_LSN, META_KEY_BASE_GENERATION, META_KEY_DB_ID, META_TABLE,
};
use crate::storage::{Storage, StorageError};
use redb::ReadableTable as _;

/// [`Storage::disable_change_log`] の設定。
#[derive(Debug, Clone, Copy, Default)]
pub struct DisableOptions {
    /// 真なら、外部へ書き出していない（アーカイブ済み水位より後の）レコードを破棄して無効化する。
    /// 失うのはそのレコードが覆う区間への PITR・増分の能力で、表のデータは失われない。既定は偽（拒否）。
    pub discard_unarchived: bool,
}

/// 無効化の失敗。`Display` は固定文言で、LSN・件数・パスを含めない（情報漏えい防止）。
#[derive(Debug)]
#[non_exhaustive]
pub enum DisableError {
    /// 変更ログを一度も有効にしていない DB（または一時表用）。
    NotEnabled,
    /// 未アーカイブのレコードが在り、破棄の指定が無い。何も書き換えていない。
    Unarchived,
    /// メタ表の値が不正（長さ・起点の欠落）。何も書き換えていない。
    LogCorrupt,
    /// ストレージ層のエラー。
    Storage(StorageError),
}

impl std::fmt::Display for DisableError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::NotEnabled => "change log is not enabled",
            Self::Unarchived => "change log has records that are not archived",
            Self::LogCorrupt => "change log metadata is inconsistent",
            Self::Storage(_) => "storage error during change log disable",
        };
        f.write_str(text)
    }
}

impl std::error::Error for DisableError {}

impl From<StorageError> for DisableError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

impl From<redb::StorageError> for DisableError {
    fn from(e: redb::StorageError) -> Self {
        Self::Storage(e.into())
    }
}

impl From<redb::TableError> for DisableError {
    fn from(e: redb::TableError) -> Self {
        Self::Storage(e.into())
    }
}

impl From<redb::TransactionError> for DisableError {
    fn from(e: redb::TransactionError) -> Self {
        Self::Storage(e.into())
    }
}

impl From<redb::CommitError> for DisableError {
    fn from(e: redb::CommitError) -> Self {
        Self::Storage(e.into())
    }
}

type Result<T> = std::result::Result<T, DisableError>;

impl Storage {
    /// 変更ログを無効化する（RECOVER-14・Issue #3093。設計は `docs/design/change-log-pitr.md` の D13）。
    /// `change_log` 表とメタ表を 1 本の保守 commit で削除し、DB を「一度も有効にしていない DB」へ戻す。
    ///
    /// - 既定では、アーカイブ済み水位より後のレコードが在ると [`DisableError::Unarchived`] で拒否する
    ///   （何も書かない）。`opts.discard_unarchived` を真にしたときだけ破棄して進む
    /// - 一度も有効にしていない DB・一時表用ストレージは [`DisableError::NotEnabled`]
    /// - 成功後は [`Self::change_log_required`] が偽になり、有効化せずに書き込める。増分バックアップ・PITR は
    ///   以後 `NotEnabled` で拒否される。再有効化すると DB 識別子が作り直され、以前のバックアップ・
    ///   アーカイブとは連鎖しない（別 DB として扱われる）
    /// - 失敗（`Err`）した `Storage` の有効状態は呼び出し前のまま
    ///
    /// `&mut self` なのは `Arc` 化前の構築段階でだけ無効化できるようにするため（`enable_change_log` と同じ）。
    pub fn disable_change_log(&mut self, opts: &DisableOptions) -> Result<()> {
        if self.temp_scoped {
            return Err(DisableError::NotEnabled);
        }
        // 保守 commit に有効状態を持ち込まない。`wrap_gated` の「有効にしていない起動」の拒否を避け、
        // capture session も付けないため、一時的に外す（`enable_change_log_with` と同じ手順）。
        let guard_on_disk = std::mem::replace(&mut self.change_log_on_disk, false);
        let guard_state = self.change_log.take();
        let result = self.disable_in_txn(opts);
        if result.is_err() {
            self.change_log_on_disk = guard_on_disk;
            self.change_log = guard_state;
        }
        result
    }

    fn disable_in_txn(&self, opts: &DisableOptions) -> Result<()> {
        let txn = self.begin_write_txn()?;
        let (watermark, last) = {
            // 許可（clippy::disallowed_methods）: メタ表と本体表は記録範囲の例外（D3）で、捕捉の対象外。
            #[allow(clippy::disallowed_methods)]
            let meta = txn.open_table(META_TABLE)?;
            if meta.get(META_KEY_DB_ID)?.is_none() {
                drop(meta);
                txn.abort()?;
                return Err(DisableError::NotEnabled);
            }
            let base = match meta.get(META_KEY_BASE_GENERATION)? {
                Some(v) => {
                    u64::from_le_bytes(v.value().try_into().map_err(|_| DisableError::LogCorrupt)?)
                }
                None => return Err(DisableError::LogCorrupt),
            };
            let archived = match meta.get(META_KEY_ARCHIVED_LSN)? {
                Some(v) => Some(u64::from_le_bytes(
                    v.value().try_into().map_err(|_| DisableError::LogCorrupt)?,
                )),
                None => None,
            };
            #[allow(clippy::disallowed_methods)]
            let log = txn.open_table(CHANGE_LOG_TABLE)?;
            let last = log.last()?.map(|(k, _)| k.value());
            (archived.unwrap_or(base), last)
        };
        if last.is_some_and(|l| l > watermark) && !opts.discard_unarchived {
            txn.abort()?;
            return Err(DisableError::Unarchived);
        }
        // 許可（clippy::disallowed_methods）: 変更ログの表自体の削除（記録範囲の例外。D3）。
        #[allow(clippy::disallowed_methods)]
        {
            txn.delete_table(CHANGE_LOG_TABLE)?;
            txn.delete_table(META_TABLE)?;
        }
        crate::recovery::commit_boundary::commit_maintenance(txn)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
