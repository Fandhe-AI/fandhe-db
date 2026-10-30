//! ROW トリガの発火計画（TABLE-45・Issue #1723・#1724・#1725）。
//!
//! 書き込み txn 内でカタログを 1 回読み（文の開始時に同じ txn から決める。実行モデルの D5。txn の外で
//! 読むと並行 DDL との TOCTOU になる）、書き込みテナント自身の該当イベント（INSERT・UPDATE・DELETE）の
//! トリガを次の 3 つに分類する。
//!
//! - 該当イベントのトリガなし: 従来経路（`Ok(None)`）
//! - BEFORE ROW・AFTER ROW だけ: トリガ経路（`Ok(Some(plan))`。通過証が無ければ `0A000`）
//! - 文トリガを含む: `0A000`（#1726 で結線するまで黙って発火させない）
//!
//! 判定は書き込みテナントの scope のカタログだけで行い、他テナントのトリガ・行の有無に依存しない
//! （存在情報を応答の差に出さない）。呼び出し元は `tenant` の書き込み関数（INSERT・UPSERT・述語形・
//! id 指定形の UPDATE／DELETE・分割実行）。トリガを発火できない経路は [`guard_no_row_triggers`] で
//! 黙って飛ばさず `0A000` にする（実行モデルの D8）。

use super::runtime::TriggerRuntime;
use crate::catalog::trigger::{
    list_triggers_in_txn, TriggerDef, TriggerEvents, TriggerLevel, TriggerTiming,
};
use crate::catalog::{CatalogError, ReferentialAction, TableSchema};
use crate::sql::allowlist::SqlSurfaceError;
use crate::tenant::TenantWriteError;

/// 発火対象の ROW トリガ（BEFORE・AFTER とも名前のバイト順）と、それを呼ぶ通過証。INSERT・UPDATE・
/// DELETE で共通（イベントごとに [`plan_row_triggers`] が絞り込む）。
pub(crate) struct RowTriggerPlan<'a> {
    pub(crate) runtime: &'a TriggerRuntime,
    /// BEFORE ROW（行ごとに書き込みの前に発火する。Issue #1723）。
    pub(crate) before: Vec<TriggerDef>,
    /// AFTER ROW（文の終わりに、書いた行ごとに発火する。Issue #1724）。
    pub(crate) after: Vec<TriggerDef>,
}

/// INSERT 経路が使う従来名（Issue #1723・#1724）。
pub(crate) type InsertTriggerPlan<'a> = RowTriggerPlan<'a>;

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
) -> Result<Option<InsertTriggerPlan<'a>>, TenantWriteError> {
    plan_row_triggers(
        write_txn,
        tenant,
        table,
        TriggerEvents::INSERT,
        None,
        runtime,
    )
}

/// UPDATE の `SET` 対象列の集合（`UPDATE OF` の判定に使う。BEFORE トリガが書き換えた列は含めない。
/// 実行モデルの D7）。`schema` は列名を引くための対象表のスキーマ。
pub(crate) struct UpdateTarget<'a> {
    pub(crate) schema: &'a TableSchema,
    pub(crate) set_columns: &'a [usize],
}

/// `tenant` の表 `table` の `event`（`TriggerEvents::INSERT`・`UPDATE`・`DELETE` のいずれか）の
/// トリガを分類する。
///
/// - 該当イベントのトリガなし: `Ok(None)`（従来経路）
/// - 文トリガを含む: `0A000`（#1726 で結線するまで黙って発火させない）
/// - `runtime` が `None`（未結線の経路）で該当トリガがある: `0A000`
/// - UPDATE で `UPDATE OF` を持つトリガは、`update` の SET 列と交差しなければ外す。列名を解決できなければ
///   fail-closed に `0A000`
///
/// UPDATE・DELETE では、親側の参照アクションが及ぶ子表に該当イベントのトリガがある場合も `0A000`
/// （子表の行は参照アクションの内部で書き換わりトリガが発火しないため。実行モデルの D8）。
pub(crate) fn plan_row_triggers<'a>(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    table: &str,
    event: u8,
    update: Option<&UpdateTarget<'_>>,
    runtime: Option<&'a TriggerRuntime>,
) -> Result<Option<RowTriggerPlan<'a>>, TenantWriteError> {
    // トリガが 1 件も無い環境では以降（子表の走査を含む）を一切行わない（従来経路・性能不変）。
    if !any_triggers_exist(write_txn)? {
        return Ok(None);
    }
    if event != TriggerEvents::INSERT {
        guard_referential_children(write_txn, tenant, table, event == TriggerEvents::DELETE)?;
    }
    let all = list_triggers_in_txn(write_txn, tenant, table)?;
    let mut matching: Vec<TriggerDef> = all
        .into_iter()
        .filter(|t| t.events.contains(event))
        .collect();
    if matching.is_empty() {
        return Ok(None);
    }
    if matching.iter().any(|t| t.level != TriggerLevel::Row) {
        return Err(unsupported("statement-level triggers are not supported"));
    }
    let Some(runtime) = runtime else {
        return Err(unsupported(
            "triggers are not supported for this write path",
        ));
    };
    if event == TriggerEvents::UPDATE {
        matching = filter_update_of(matching, update)?;
    }
    let (before, after): (Vec<TriggerDef>, Vec<TriggerDef>) = matching
        .into_iter()
        .partition(|t| t.timing == TriggerTiming::Before);
    if before.is_empty() && after.is_empty() {
        return Ok(None);
    }
    Ok(Some(RowTriggerPlan {
        runtime,
        before,
        after,
    }))
}

/// `UPDATE OF` を持つトリガを SET 列との交差で絞る。`update` が `None`（SET 列を知らない経路）で
/// `UPDATE OF` を持つトリガがあれば fail-closed に `0A000`。
fn filter_update_of(
    triggers: Vec<TriggerDef>,
    update: Option<&UpdateTarget<'_>>,
) -> Result<Vec<TriggerDef>, TenantWriteError> {
    let mut out = Vec::with_capacity(triggers.len());
    for t in triggers {
        if t.update_of.is_empty() {
            out.push(t);
            continue;
        }
        let Some(update) = update else {
            return Err(unsupported("UPDATE OF triggers are not supported here"));
        };
        let mut hit = false;
        for name in &t.update_of {
            let Some(idx) = update.schema.columns.iter().position(|c| &c.name == name) else {
                return Err(unsupported("UPDATE OF column cannot be resolved"));
            };
            if update.set_columns.contains(&idx) {
                hit = true;
            }
        }
        if hit {
            out.push(t);
        }
    }
    Ok(out)
}

/// 任意のイベント集合（`events_mask`）に該当するトリガ（行・文を問わない）が表にあれば `0A000`。
/// 分割実行 DML・NoSQL・UPSERT など、トリガを発火できない経路が黙って飛ばさないための検査
/// （カタログだけを見る。他テナントの定義は構造上引けない）。
pub(crate) fn guard_no_row_triggers(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    table: &str,
    events_mask: u8,
) -> Result<(), TenantWriteError> {
    if !any_triggers_exist(write_txn)? {
        return Ok(());
    }
    let hit = list_triggers_in_txn(write_txn, tenant, table)?
        .iter()
        .any(|t| t.events.bits() & events_mask != 0);
    if hit {
        return Err(unsupported(
            "triggers are not supported for this write path",
        ));
    }
    Ok(())
}

/// UPDATE／DELETE を、トリガの結線が無い経路（NoSQL・RETURNING 付き・分割実行など）から行うときの検査。
/// 対象表の該当イベントのトリガ（行・文）と、参照アクションが及ぶ子表のトリガのどちらかがあれば `0A000`。
pub(crate) fn guard_unwired_dml(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    table: &str,
    is_delete: bool,
) -> Result<(), TenantWriteError> {
    let mask = if is_delete {
        TriggerEvents::DELETE
    } else {
        TriggerEvents::UPDATE
    };
    guard_no_row_triggers(write_txn, tenant, table, mask)?;
    if any_triggers_exist(write_txn)? {
        guard_referential_children(write_txn, tenant, table, is_delete)?;
    }
    Ok(())
}

/// トリガ表に 1 件でも定義があるか（全テナント。有無の判定だけで中身は読まない）。
fn any_triggers_exist(write_txn: &redb::WriteTransaction) -> Result<bool, TenantWriteError> {
    use redb::ReadableTableMetadata;
    let table = match write_txn.open_table(crate::catalog::trigger::TRIGGERS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(TenantWriteError::from(CatalogError::from(e))),
    };
    let len = table
        .len()
        .map_err(|e| TenantWriteError::from(CatalogError::from(e)))?;
    Ok(len > 0)
}

/// 参照アクション（CASCADE・SET NULL・SET DEFAULT）が及ぶ子表を、カタログの FK 宣言だけから再帰的に
/// たどり、該当イベントのトリガがある子表があれば `0A000`（子表の行は参照アクション内で書き換わり
/// トリガが発火しないため、黙って飛ばさない。実行モデルの D8）。訪問済み集合で循環を断つ。
fn guard_referential_children(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    table: &str,
    is_delete: bool,
) -> Result<(), TenantWriteError> {
    let mut visited: Vec<(String, bool)> = vec![(table.to_string(), is_delete)];
    let mut stack: Vec<(String, bool)> = vec![(table.to_string(), is_delete)];
    while let Some((parent, delete)) = stack.pop() {
        let children = crate::catalog::referencing_foreign_keys_in_txn(write_txn, &parent)?;
        for (child_schema, fk) in children {
            let action = if delete {
                fk.on_delete()
            } else {
                fk.on_update()
            };
            let child_delete = match action {
                ReferentialAction::NoAction => continue,
                ReferentialAction::Cascade => delete,
                ReferentialAction::SetNull | ReferentialAction::SetDefault => false,
            };
            let event = if child_delete {
                TriggerEvents::DELETE
            } else {
                TriggerEvents::UPDATE
            };
            let child = child_schema.name.clone();
            // 対象表自身（自己参照 FK）は子表として数えない。自身のトリガは通常経路で発火し、
            // 参照アクションで書き換わる同表の別行の扱いは設計記録の既知の制限に書く。
            let hit = child != table
                && list_triggers_in_txn(write_txn, tenant, &child)?
                    .iter()
                    .any(|t| t.events.contains(event));
            if hit {
                return Err(unsupported(
                    "triggers on tables affected by referential actions are not supported",
                ));
            }
            let key = (child, child_delete);
            if !visited.contains(&key) {
                visited.push(key.clone());
                stack.push(key);
            }
        }
    }
    Ok(())
}
