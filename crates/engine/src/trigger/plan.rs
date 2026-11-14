//! ROW・STATEMENT トリガの発火計画（TABLE-45・Issue #1723・#1724・#1725・#1726）。
//!
//! 書き込み txn 内でカタログを 1 回読み（文の開始時に同じ txn から決める。実行モデルの D5。txn の外で
//! 読むと並行 DDL との TOCTOU になる）、書き込みテナント自身の該当イベント（INSERT・UPDATE・DELETE）の
//! トリガを次の 2 つに分類する。
//!
//! - 該当イベントのトリガなし: 従来経路（`Ok(None)`）
//! - 1 件でもある（BEFORE／AFTER × ROW／STATEMENT の 4 群に分ける）: トリガ経路（`Ok(Some(plan))`。
//!   通過証が無ければ `0A000`）。文トリガしか無い表も従来経路へ落とさない（黙って発火しないのを防ぐ）
//!
//! TRUNCATE は文トリガだけで、[`plan_truncate`] が表ごとに分類する（Issue #1726）。
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
use crate::storage::change_log::CaptureWriteTxn as _;
use crate::tenant::TenantWriteError;

/// 発火対象のトリガ（4 群とも名前のバイト順）と、それを呼ぶ通過証。INSERT・UPDATE・DELETE で共通
/// （イベントごとに [`plan_row_triggers`] が絞り込む）。型名は ROW 時代のまま残す（Issue #1726 で
/// 文トリガの群を足した）。
pub(crate) struct RowTriggerPlan<'a> {
    pub(crate) runtime: &'a TriggerRuntime<'a>,
    /// BEFORE ROW（行ごとに書き込みの前に発火する。Issue #1723）。
    pub(crate) before: Vec<TriggerDef>,
    /// AFTER ROW（文の終わりに、書いた行ごとに発火する。Issue #1724）。
    pub(crate) after: Vec<TriggerDef>,
    /// BEFORE STATEMENT（対象の列挙より前に 1 回、対象 0 件でも発火する。Issue #1726）。
    pub(crate) before_stmt: Vec<TriggerDef>,
    /// AFTER STATEMENT（AFTER ROW の後に 1 回、対象 0 件でも発火する。Issue #1726）。
    pub(crate) after_stmt: Vec<TriggerDef>,
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
    runtime: Option<&'a TriggerRuntime<'a>>,
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
/// - ROW・STATEMENT の別を問わず該当トリガあり: 4 群に分けて `Ok(Some(plan))`
/// - `runtime` が `None`（未結線の経路）で該当トリガがある: `0A000`
/// - UPDATE で `UPDATE OF` を持つトリガは、`update` の SET 列と交差しなければ外す。列名を解決できなければ
///   fail-closed に `0A000`
///
/// UPDATE・DELETE では、親側の参照アクションが及ぶ子表（Issue #3284）も見る。
///
/// - `runtime` あり: 子表に該当イベントの ROW トリガがあれば、親に該当トリガが無くても 4 群とも空の
///   `Ok(Some(plan))` を返す（参照アクションが書き換える子行ごとの発火は `constraint` の
///   `referential_triggers` が担い、親は既存のトリガ経路〔`tenant/trigger_dml.rs`〕へ載せるため）。
///   子表に FOR EACH STATEMENT トリガがあれば `0A000`（RI 内部クエリごとの文トリガの回数を再現できない）
/// - `runtime` なし（未結線の経路）: 子表に該当トリガがあれば従来どおり `0A000`（実行モデルの D8）
pub(crate) fn plan_row_triggers<'a>(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    table: &str,
    event: u8,
    update: Option<&UpdateTarget<'_>>,
    runtime: Option<&'a TriggerRuntime<'a>>,
) -> Result<Option<RowTriggerPlan<'a>>, TenantWriteError> {
    // トリガが 1 件も無い環境では以降（子表の走査を含む）を一切行わない（従来経路・性能不変）。
    if !any_triggers_exist(write_txn)? {
        return Ok(None);
    }
    let mut children_fire = false;
    if event != TriggerEvents::INSERT {
        let is_delete = event == TriggerEvents::DELETE;
        if runtime.is_some() {
            children_fire = inspect_referential_children(write_txn, tenant, table, is_delete)?;
        } else {
            guard_referential_children(write_txn, tenant, table, is_delete)?;
        }
    }
    // 子表だけがトリガを持つとき、親の群が空の計画で既存のトリガ経路へ載せる。
    let child_only = |runtime: Option<&'a TriggerRuntime<'a>>| {
        runtime
            .filter(|_| children_fire)
            .map(|runtime| RowTriggerPlan {
                runtime,
                before: Vec::new(),
                after: Vec::new(),
                before_stmt: Vec::new(),
                after_stmt: Vec::new(),
            })
    };
    let all = list_triggers_in_txn(write_txn, tenant, table)?;
    let mut matching: Vec<TriggerDef> = all
        .into_iter()
        .filter(|t| t.events.contains(event))
        .collect();
    if matching.is_empty() {
        return Ok(child_only(runtime));
    }
    let Some(runtime) = runtime else {
        return Err(unsupported(
            "triggers are not supported for this write path",
        ));
    };
    if event == TriggerEvents::UPDATE {
        matching = filter_update_of(matching, update)?;
    }
    let (row, stmt): (Vec<TriggerDef>, Vec<TriggerDef>) = matching
        .into_iter()
        .partition(|t| t.level == TriggerLevel::Row);
    let (before, after): (Vec<TriggerDef>, Vec<TriggerDef>) = row
        .into_iter()
        .partition(|t| t.timing == TriggerTiming::Before);
    let (before_stmt, after_stmt): (Vec<TriggerDef>, Vec<TriggerDef>) = stmt
        .into_iter()
        .partition(|t| t.timing == TriggerTiming::Before);
    // `UPDATE OF` の絞り込みで全て外れたときだけ従来経路（4 群とも空）。
    if before.is_empty() && after.is_empty() && before_stmt.is_empty() && after_stmt.is_empty() {
        return Ok(child_only(Some(runtime)));
    }
    Ok(Some(RowTriggerPlan {
        runtime,
        before,
        after,
        before_stmt,
        after_stmt,
    }))
}

/// 参照アクションが書き換える子表 `table` の `event`（`UPDATE` か `DELETE`）の ROW トリガを分類する
/// （Issue #3284。`constraint` の `referential_triggers` が子行ごとの発火の前に 1 回呼ぶ）。
///
/// 該当する ROW トリガが無い（`UPDATE OF` が `update` の列と交差しない場合を含む）ときは `Ok(None)`。
/// 文トリガは親の計画の段で拒否済みだが、取りこぼしても黙って飛ばさないよう、ここでも `0A000`。
/// 書き込みテナント自身の scope のカタログだけを見る（他テナントの定義は構造上引けない）。
pub(crate) fn plan_child_row_triggers<'a>(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    table: &str,
    event: u8,
    update: Option<&UpdateTarget<'_>>,
    runtime: &'a TriggerRuntime<'a>,
) -> Result<Option<RowTriggerPlan<'a>>, TenantWriteError> {
    let mut matching: Vec<TriggerDef> = list_triggers_in_txn(write_txn, tenant, table)?
        .into_iter()
        .filter(|t| t.events.contains(event))
        .collect();
    if matching.is_empty() {
        return Ok(None);
    }
    if event == TriggerEvents::UPDATE {
        matching = filter_update_of(matching, update)?;
    }
    if matching.iter().any(|t| t.level == TriggerLevel::Statement) {
        return Err(unsupported(STATEMENT_ON_REFERENTIAL_CHILD));
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
        before_stmt: Vec::new(),
        after_stmt: Vec::new(),
    }))
}

/// TRUNCATE の発火計画（表ごと。BEFORE／AFTER 別のトリガ。Issue #1726）。
pub(crate) struct TruncatePlan {
    /// BEFORE TRUNCATE（名前のバイト順）。
    pub(crate) before: Vec<TriggerDef>,
    /// AFTER TRUNCATE（名前のバイト順）。
    pub(crate) after: Vec<TriggerDef>,
}

impl TruncatePlan {
    /// 発火するトリガが 1 件も無いか。
    pub(crate) fn is_empty(&self) -> bool {
        self.before.is_empty() && self.after.is_empty()
    }
}

/// `tenant` の表 `table` の TRUNCATE トリガを分類する（書き込みテナントの scope のカタログだけを見る）。
/// 該当が無ければ空の計画。該当があり `runtime` が `None`（未結線の経路）なら `0A000`。TRUNCATE は
/// 文トリガだけ（行トリガは定義の段で拒否済み）。参照アクションは発火しないので子表は見ない。
pub(crate) fn plan_truncate(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    table: &str,
    runtime: Option<&TriggerRuntime>,
) -> Result<TruncatePlan, TenantWriteError> {
    let empty = TruncatePlan {
        before: Vec::new(),
        after: Vec::new(),
    };
    if !any_triggers_exist(write_txn)? {
        return Ok(empty);
    }
    let matching: Vec<TriggerDef> = list_triggers_in_txn(write_txn, tenant, table)?
        .into_iter()
        .filter(|t| t.events.contains(TriggerEvents::TRUNCATE))
        .collect();
    if matching.is_empty() {
        return Ok(empty);
    }
    if runtime.is_none() {
        return Err(unsupported(
            "triggers are not supported for this write path",
        ));
    }
    let (before, after): (Vec<TriggerDef>, Vec<TriggerDef>) = matching
        .into_iter()
        .partition(|t| t.timing == TriggerTiming::Before);
    Ok(TruncatePlan { before, after })
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

/// 分割実行 DML（Issue #3283）が、該当イベントの文トリガ（BEFORE／AFTER STATEMENT）を持つ表を固定文言の
/// `0A000` で拒否するための検査。「文につき 1 回・全行の前後」は、チャンクごとに commit し再開もある
/// 非原子実行では保証できない（毎チャンク発火か初回・最終のみかが再開・取消・上限停止で曖昧になる）ため、
/// 黙って飛ばさず拒否する（実行モデルの D8）。行トリガだけの計画は通す。
pub(crate) fn reject_statement_triggers(plan: &RowTriggerPlan<'_>) -> Result<(), TenantWriteError> {
    if plan.before_stmt.is_empty() && plan.after_stmt.is_empty() {
        return Ok(());
    }
    Err(unsupported(
        "statement triggers are not supported for partitioned DML",
    ))
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
    let table = match write_txn.open_table_logged(crate::catalog::trigger::TRIGGERS_TABLE) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(TenantWriteError::from(CatalogError::from(e))),
    };
    let len = table
        .len()
        .map_err(|e| TenantWriteError::from(CatalogError::from(e)))?;
    Ok(len > 0)
}

/// 子表に FOR EACH STATEMENT トリガがあるときの固定文言（値・表名を含めない）。
const STATEMENT_ON_REFERENTIAL_CHILD: &str =
    "statement-level triggers on tables affected by referential actions are not supported";

/// 子表に該当イベントのトリガがあるときの固定文言（未結線の経路）。
const TRIGGER_ON_REFERENTIAL_CHILD: &str =
    "triggers on tables affected by referential actions are not supported";

/// 参照アクションが及ぶ子表の走査の判定方式。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ChildWalk {
    /// 未結線の経路: 該当イベントのトリガ（行・文）が 1 件でもあれば `0A000`。
    RejectAny,
    /// 結線済みの経路: ROW トリガの有無を返し、FOR EACH STATEMENT トリガは `0A000`。
    FireRow,
}

/// 参照アクション（CASCADE・SET NULL・SET DEFAULT）が及ぶ子表を、カタログの FK 宣言だけから再帰的に
/// たどり、該当イベントのトリガがある子表を `mode` に応じて扱う（実行モデルの D8・Issue #3284）。
/// 訪問済み集合で循環を断つ。過大近似（実際には書き換わらない行も含む）で、拒否側に倒す。
/// 戻り値は子表に該当イベントの ROW トリガがあったか（`FireRow` のみ意味を持つ）。
fn walk_referential_children(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    table: &str,
    is_delete: bool,
    mode: ChildWalk,
) -> Result<bool, TenantWriteError> {
    let mut found_row = false;
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
            // 対象表自身（自己参照 FK）も子表として数える。
            for t in list_triggers_in_txn(write_txn, tenant, &child)?
                .iter()
                .filter(|t| t.events.contains(event))
            {
                match mode {
                    ChildWalk::RejectAny => {
                        return Err(unsupported(TRIGGER_ON_REFERENTIAL_CHILD));
                    }
                    ChildWalk::FireRow if t.level == TriggerLevel::Statement => {
                        return Err(unsupported(STATEMENT_ON_REFERENTIAL_CHILD));
                    }
                    ChildWalk::FireRow => found_row = true,
                }
            }
            let key = (child, child_delete);
            if !visited.contains(&key) {
                visited.push(key.clone());
                stack.push(key);
            }
        }
    }
    Ok(found_row)
}

/// 未結線の経路（`runtime` なし・[`guard_unwired_dml`]）向け。子表に該当イベントのトリガがあれば `0A000`
/// （子表の行は参照アクション内で書き換わりトリガが発火しないため、黙って飛ばさない）。
fn guard_referential_children(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    table: &str,
    is_delete: bool,
) -> Result<(), TenantWriteError> {
    walk_referential_children(write_txn, tenant, table, is_delete, ChildWalk::RejectAny).map(|_| ())
}

/// 結線済みの経路向け。子表に該当イベントの ROW トリガがあれば `true`（`constraint` が子行ごとに発火する）。
/// 子表に FOR EACH STATEMENT トリガがあれば、固定文言の `0A000`。
fn inspect_referential_children(
    write_txn: &redb::WriteTransaction,
    tenant: &str,
    table: &str,
    is_delete: bool,
) -> Result<bool, TenantWriteError> {
    walk_referential_children(write_txn, tenant, table, is_delete, ChildWalk::FireRow)
}
