//! INSERT 経路の発火計画（TABLE-45・Issue #1723）。
//!
//! 書き込み txn 内でカタログを 1 回読み（文の開始時に同じ txn から決める。実行モデルの D5。txn の外で
//! 読むと並行 DDL との TOCTOU になる）、書き込みテナント自身の INSERT トリガを次の 3 つに分類する。
//!
//! - INSERT トリガなし: 従来経路（`Ok(None)`）
//! - BEFORE・ROW だけ: トリガ経路（`Ok(Some(chain))`。通過証が無ければ `0A000`）
//! - AFTER ROW・文トリガを含む: `0A000`（#1724・#1726 で結線するまで黙って発火させない）
//!
//! 判定は書き込みテナントの scope のカタログだけで行い、他テナントのトリガ・行の有無に依存しない
//! （存在情報を応答の差に出さない）。呼び出し元は `tenant` の挿入関数・UPSERT・生の行挿入。

use super::runtime::TriggerRuntime;
use crate::catalog::trigger::{
    list_triggers_in_txn, TriggerDef, TriggerEvents, TriggerLevel, TriggerTiming,
};
use crate::sql::allowlist::SqlSurfaceError;
use crate::tenant::TenantWriteError;

/// 発火対象の BEFORE ROW INSERT トリガと、それを呼ぶ通過証。
pub(crate) struct BeforeInsertChain<'a> {
    pub(crate) runtime: &'a TriggerRuntime,
    pub(crate) triggers: Vec<TriggerDef>,
}

fn unsupported(detail: &str) -> TenantWriteError {
    TenantWriteError::TriggerFailed(SqlSurfaceError::FeatureNotSupported {
        detail: detail.to_string(),
    })
}

/// `tenant` の表 `table` の INSERT トリガを分類する。`runtime` が `None`（未結線の経路）で INSERT
/// トリガが 1 件でもあれば `0A000`。
pub(crate) fn plan_insert<'a>(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    table: &str,
    runtime: Option<&'a TriggerRuntime>,
) -> Result<Option<BeforeInsertChain<'a>>, TenantWriteError> {
    let all = list_triggers_in_txn(write_txn, tenant, table)?;
    let insert_triggers: Vec<TriggerDef> = all
        .into_iter()
        .filter(|t| t.events.contains(TriggerEvents::INSERT))
        .collect();
    if insert_triggers.is_empty() {
        return Ok(None);
    }
    if insert_triggers
        .iter()
        .any(|t| t.timing != TriggerTiming::Before || t.level != TriggerLevel::Row)
    {
        return Err(unsupported(
            "AFTER and statement-level triggers are not supported",
        ));
    }
    match runtime {
        Some(runtime) => Ok(Some(BeforeInsertChain {
            runtime,
            triggers: insert_triggers,
        })),
        None => Err(unsupported(
            "triggers are not supported for this write path",
        )),
    }
}
