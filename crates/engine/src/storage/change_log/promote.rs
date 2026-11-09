//! レプリカの昇格の保守 commit（Issue #3132・親 #2171・ポインタ: RECOVER-15 (4)(5)。方式の記録は
//! `docs/design/replication-streaming.md` の「実装判断（Issue #3132）」）。
//!
//! # 役割と責務境界
//!
//! 停止中のレプリカ DB（変更ログのメタを持ち、変更ログを有効にせず開いたもの）を、運用者の明示操作で
//! プライマリとして再起動できる状態へ切り替える。wire-server の `replica-promote` サブコマンドが、
//! 構築段階の `&mut Storage` に対して呼ぶ（`EngineCore`・SQL・NoSQL・レプリケーションのリスナーからは
//! 届かない。`disable_change_log` と同じ運用者専用の入口）。
//!
//! 1 本の保守 commit で「ベースの検査（メタの存在と連続性）→ timeline の分岐 → 適用済みの commit 番号の
//! 確定」を行う。分岐は復元（`fork`）と同じ規則で、旧プライマリ・他のレプリカの系統とは timeline が
//! 変わるため、昇格した DB へ旧系統のレコード・アーカイブ・増分が混ざる経路は既存の `Foreign` 系の
//! 検査が拒否する。失敗したときは書き込みトランザクションを commit せず破棄するので、DB は何も変わらない
//! （fail-closed）。
//!
//! # 範囲
//!
//! - `restore_generation` は進めない。昇格は復元ではなく、アプリへ「復元された」と知らせる値を動かさない
//!   （Issue #2176 の判断と同じ）。
//! - 分岐後の timeline は `max(時計の下限, 旧 timeline + 1)`。同じプライマリのレプリカを別々に昇格しても
//!   `(DB 識別子, timeline)` が衝突しにくい。同じマイクロ秒に 2 台を昇格する・時計が戻った場合は衝突しうる
//!   既知の制約で、運用上は 1 台だけを昇格する。
//! - 失われる範囲（旧プライマリで本 DB が受け取っていない commit）は、本 DB からは「適用済みの LSN より後
//!   すべて」としか分からない。上限は報告できない。
//! - 台帳（RECOVER-15 (5)）は通常の表として複製済みで、昇格のための変更は要らない。

use redb::ReadableDatabase as _;

use super::fork;
use super::incremental::IncrementalError;
use super::replica_base::check_in_read;
use crate::storage::{Storage, StorageError};

/// 昇格の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromoteReport {
    /// 昇格時点で確定している適用済みの commit 番号（LSN）。これより後の旧プライマリの commit は失われる。
    pub applied_lsn: u64,
    /// 分岐後の timeline。
    pub timeline: u64,
}

/// 昇格の失敗。`Display` は固定の英語で、LSN・パスを含まない。
#[derive(Debug)]
#[non_exhaustive]
pub enum PromoteError {
    /// レプリカのベースではない（変更ログのメタが無い、または変更ログを有効にして開いている）。
    NotReplicaBase,
    /// 変更ログが commit 番号と連続しない（記録のない区間がある）。
    BaseDiscontinuous,
    /// timeline の上限に達した。
    TimelineLimit,
    /// システム時計を取得できない。
    Clock,
    /// ストレージ層のエラー。
    Storage(StorageError),
}

impl std::fmt::Display for PromoteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotReplicaBase => "database is not a replica base",
            Self::BaseDiscontinuous => {
                "replica change log is not continuous with its commit number"
            }
            Self::TimelineLimit => "change log timeline limit reached",
            Self::Clock => "system clock is unavailable",
            Self::Storage(_) => "storage error during replica promotion",
        })
    }
}

impl std::error::Error for PromoteError {}

impl From<StorageError> for PromoteError {
    fn from(e: StorageError) -> Self {
        Self::Storage(e)
    }
}

impl Storage {
    /// 停止中のレプリカ DB を昇格する（時計の下限を使う）。詳細は [`Self::promote_replica_with_floor`]。
    pub fn promote_replica(&mut self) -> Result<PromoteReport, PromoteError> {
        let floor =
            crate::storage::restore_generation::clock_floor().map_err(|_| PromoteError::Clock)?;
        self.promote_replica_with_floor(floor)
    }

    /// 時計の下限 `floor` を指定した昇格（時計に依存しないテストが使う）。変更ログを有効にして開いた
    /// `Storage`・一時表用ストレージ・変更ログのメタが無い DB・連続しない DB は拒否し、何も書かない。
    pub(crate) fn promote_replica_with_floor(
        &mut self,
        floor: u64,
    ) -> Result<PromoteReport, PromoteError> {
        if self.temp_scoped || self.change_log.is_some() || !self.change_log_on_disk {
            return Err(PromoteError::NotReplicaBase);
        }
        // 変更ログを有効にしたことのある DB を有効にしないまま開いたレプリカは、通常の書き込みを
        // 拒否される。この保守 commit は記録対象を持たないので、`enable_change_log` と同じく検査を
        // 一時的に外し、結果にかかわらず戻す。
        let guard = std::mem::replace(&mut self.change_log_on_disk, false);
        let result = self.promote_in_txn(floor);
        self.change_log_on_disk = guard;
        result
    }

    fn promote_in_txn(&self, floor: u64) -> Result<PromoteReport, PromoteError> {
        // 書き込みゲートを持った状態で検査する（検査と分岐の間に別の書き込みが入らない）。
        let txn = self.begin_write_txn()?;
        let point = {
            let read = self
                .db
                .begin_read()
                .map_err(|e| PromoteError::Storage(StorageError::from(e)))?;
            check_in_read(&read).map_err(|e| match e {
                IncrementalError::NotEnabled => PromoteError::NotReplicaBase,
                IncrementalError::Storage(s) => PromoteError::Storage(s),
                _ => PromoteError::BaseDiscontinuous,
            })?
        };
        // 溢れは分岐の前に分類する（`fork` 側の `next_timeline` も同じ規則で拒否する）。
        if point.timeline.checked_add(1).is_none() {
            return Err(PromoteError::TimelineLimit);
        }
        let timeline = match fork::fork_timeline_in_txn(&txn, floor)? {
            Some(t) => t,
            // 検査を通ったのにメタが無いのは想定外。fail-closed に拒否する。
            None => return Err(PromoteError::NotReplicaBase),
        };
        crate::recovery::commit_boundary::commit_maintenance(txn)?;
        Ok(PromoteReport {
            applied_lsn: point.end_lsn,
            timeline,
        })
    }
}

#[cfg(test)]
mod tests;
