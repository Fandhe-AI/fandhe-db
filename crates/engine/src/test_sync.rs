//! テスト専用の同期点（feature `test-sync-points` 限定。Issue #1363・TABLE-15）。
//!
//! 役割: 「DROP TABLE の実行中にクエリが走る」交差を結合テストで決定的に起こすため、
//! `catalog` の `Storage::drop_table`（commit 直前）と `core` の `execute_read_statement`
//! （読み取りスナップショット確定後）から、テストが登録したコールバックを呼ぶ。
//! コールバックへ渡すのは [`SyncPoint`] の種別のみで、行・テナント・テーブル名は渡さない。
//! テナント境界・RLS・認証の経路には触れず、既定ビルドと wire-server には含まれない。

/// 同期点の種別。
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPoint {
    /// `drop_table` の write txn 内で全削除を終え、commit する直前。
    DropTableBeforeCommit,
    /// 読み取り文の実行開始時点（読み取りスナップショット確定後・スキーマ解決前）。
    ReadStatementSnapshotAcquired,
}

/// 同期点で呼ばれるコールバック（`Storage::set_sync_hook` で設定する）。
pub type SyncHook = std::sync::Arc<dyn Fn(SyncPoint) + Send + Sync>;
