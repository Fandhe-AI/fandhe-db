//! `MERGE` の文トリガ（FOR EACH STATEMENT。TABLE-45・SQL-79・Issue #3355。設計記録は
//! `docs/design/sql-merge.md` の D12 と §11）の発火計画・発火・抑止の置き場。
//!
//! 責務境界: `core::merge_exec` の MERGE 実行器が、第 3 段で既存の書き込み経路
//! （`update_rows_joined_unchecked`・`delete_rows_where_with_triggers`・`execute_insert_batch_with_schema_in`）を
//! 句ごとに呼ぶ。各経路は呼び出しごとに自分の文トリガを発火するため、そのままでは文トリガが句の数だけ
//! 発火してしまう。本モジュールは次の 3 つで「操作の種類ごとに 1 回」の契約を作る。
//!
//! 1. [`MergeStatementTriggers::plan`]: WHEN 句に現れた操作の種類ごとに、既存の分類器
//!    （`trigger::plan::plan_row_triggers`）で文トリガの群を得る（実際に行を書いたかは問わない）。
//! 2. [`MergeStatementTriggers::fire_before`]／[`MergeStatementTriggers::fire_after`]: MERGE の入口と出口で
//!    1 回ずつ発火する（0 行でも発火する）。発火は redb の表ハンドルを開かずに呼ぶ契約。
//! 3. [`MergeStatementTriggerScope`]: 第 3 段の間だけ、対象表の文トリガ群を分類器が空にして返すよう
//!    抑止する（唯一の抑止点は `plan_row_triggers`）。行トリガは影響を受けない。
//!
//! 抑止は「表名」と「トリガの入れ子の深さ」の両方が一致するときだけ効く。トリガ本体が同じ表へ書く入れ子の
//! DML はより深い位置で動くため抑止されず、文トリガを正しく発火する（ここを消すと fail-open になる）。
//! 判定は書き込みテナント自身の scope のカタログだけで行い、他テナントのトリガ・行の有無に依存しない。

use super::{TenantWriteError, WriteTarget};
use crate::catalog::trigger::TriggerEvents;
use crate::catalog::TableSchema;
use crate::policy::PolicyContext;
use crate::trigger::call::TriggerOp;
use crate::trigger::plan::{plan_row_triggers, RowTriggerPlan, UpdateTarget};
use crate::trigger::write_handle::TriggerWriteHandle;
use crate::trigger::TriggerRuntime;
use std::cell::RefCell;

thread_local! {
    /// 文トリガを抑止している区間（対象表名・区間に入った時点のトリガの深さ）。区間が無ければ `None`。
    static SUPPRESSED: RefCell<Option<(String, usize)>> = const { RefCell::new(None) };
}

/// `table` の文トリガ群を分類器が空にして返すべきか。区間中・表名が一致・現在の深さが区間に入った
/// 深さと一致するときだけ真（トリガ本体が起こす入れ子の DML では偽）。
pub(crate) fn suppresses(table: &str) -> bool {
    SUPPRESSED.with(|s| {
        s.borrow().as_ref().is_some_and(|(name, depth)| {
            name == table && *depth == crate::trigger::depth::current_depth()
        })
    })
}

/// 第 3 段の間、対象表の文トリガの発火を抑止する RAII ガード。drop（早期 return・panic を含む）で
/// 直前の状態へ戻す。
pub(crate) struct MergeStatementTriggerScope {
    prev: Option<(String, usize)>,
}

impl MergeStatementTriggerScope {
    pub(crate) fn enter(table: &str) -> Self {
        let depth = crate::trigger::depth::current_depth();
        let prev = SUPPRESSED.with(|s| s.borrow_mut().replace((table.to_string(), depth)));
        Self { prev }
    }
}

impl Drop for MergeStatementTriggerScope {
    fn drop(&mut self) {
        let prev = self.prev.take();
        SUPPRESSED.with(|s| *s.borrow_mut() = prev);
    }
}

/// WHEN 句に現れた操作の種類（DO NOTHING は数えない）と、UPDATE 句の SET 列の和集合。
#[derive(Debug, Clone, Default)]
pub(crate) struct MergeActionKinds {
    pub(crate) insert: bool,
    pub(crate) update: bool,
    pub(crate) delete: bool,
    /// 全 UPDATE 句（MATCHED・BY SOURCE）の SET 列（`UPDATE OF` の判定に使う）。
    pub(crate) update_columns: Vec<usize>,
}

/// 種類ごとの文トリガの発火計画。
pub(crate) struct MergeStatementTriggers<'a> {
    insert: Option<RowTriggerPlan<'a>>,
    update: Option<RowTriggerPlan<'a>>,
    delete: Option<RowTriggerPlan<'a>>,
}

impl<'a> MergeStatementTriggers<'a> {
    /// 書き込み txn の中で、`kinds` の各種類の発火計画を 1 回ずつ作る（文の開始時に同じ txn から決める）。
    /// 抑止区間に入る**前**に呼ぶこと。
    pub(crate) fn plan(
        write_txn: &redb::WriteTransaction,
        ctx: &PolicyContext,
        table: &str,
        schema: &TableSchema,
        kinds: &MergeActionKinds,
        runtime: &'a TriggerRuntime<'a>,
    ) -> Result<Self, TenantWriteError> {
        let tenant = ctx.tenant_id();
        let mut set_columns = kinds.update_columns.clone();
        set_columns.sort_unstable();
        set_columns.dedup();
        let insert = if kinds.insert {
            plan_row_triggers(
                write_txn,
                tenant,
                table,
                TriggerEvents::INSERT,
                None,
                Some(runtime),
            )?
        } else {
            None
        };
        let update = if kinds.update {
            plan_row_triggers(
                write_txn,
                tenant,
                table,
                TriggerEvents::UPDATE,
                Some(&UpdateTarget {
                    schema,
                    set_columns: &set_columns,
                }),
                Some(runtime),
            )?
        } else {
            None
        };
        let delete = if kinds.delete {
            plan_row_triggers(
                write_txn,
                tenant,
                table,
                TriggerEvents::DELETE,
                None,
                Some(runtime),
            )?
        } else {
            None
        };
        Ok(Self {
            insert,
            update,
            delete,
        })
    }

    /// いずれかの種類にトリガ（行・文・参照アクション先の子表を含む）の発火計画があるか。`RETURNING` つきの
    /// MERGE は、トリガとの併用を書き込み前に固定文言で拒否する判定に使う（Issue #3356）。
    pub(crate) fn has_any(&self) -> bool {
        self.insert.is_some() || self.update.is_some() || self.delete.is_some()
    }

    /// BEFORE STATEMENT を INSERT → UPDATE → DELETE の順に 1 回ずつ発火する。
    pub(crate) fn fire_before(
        &self,
        writes: &TriggerWriteHandle<'_>,
        table: &str,
        schema: &TableSchema,
    ) -> Result<(), TenantWriteError> {
        for (plan, op) in Self::present([
            (&self.insert, TriggerOp::Insert),
            (&self.update, TriggerOp::Update),
            (&self.delete, TriggerOp::Delete),
        ]) {
            super::trigger_dml::fire_before_statement(plan, writes, table, schema, op)?;
        }
        Ok(())
    }

    /// AFTER STATEMENT を DELETE → UPDATE → INSERT の順に 1 回ずつ発火する（BEFORE の逆順）。
    pub(crate) fn fire_after(
        &self,
        writes: &TriggerWriteHandle<'_>,
        table: &str,
        schema: &TableSchema,
    ) -> Result<(), TenantWriteError> {
        for (plan, op) in Self::present([
            (&self.delete, TriggerOp::Delete),
            (&self.update, TriggerOp::Update),
            (&self.insert, TriggerOp::Insert),
        ]) {
            crate::trigger::fire::fire_statement(
                plan.runtime,
                writes,
                table,
                schema,
                &plan.after_stmt,
                op,
                crate::catalog::trigger::TriggerTiming::After,
            )
            .map_err(TenantWriteError::TriggerFailed)?;
        }
        Ok(())
    }

    fn present<'p>(
        list: [(&'p Option<RowTriggerPlan<'a>>, TriggerOp); 3],
    ) -> Vec<(&'p RowTriggerPlan<'a>, TriggerOp)> {
        list.into_iter()
            .filter_map(|(p, op)| p.as_ref().map(|p| (p, op)))
            .collect()
    }
}

/// 文トリガ用の書き込み窓口を作る。`runtime` は入れ子の発火に使う通過証。
pub(crate) fn statement_writes<'a>(
    write_txn: &'a redb::WriteTransaction,
    ctx: &'a PolicyContext,
    runtime: &'a TriggerRuntime<'a>,
) -> TriggerWriteHandle<'a> {
    TriggerWriteHandle::new(write_txn, ctx, Some(runtime))
}

/// 文トリガ本体が書いた表の遅延 FK の扱いを決める（`TriggerWrittenTablesScope` の内で呼ぶ。記録区間が
/// あれば `mark_written` 経由で COMMIT 時検査へ載せる）。
pub(crate) fn settle_statement_writes(
    write_txn: &redb::WriteTransaction,
    ctx: &PolicyContext,
    writes: &TriggerWriteHandle<'_>,
) -> Result<(), TenantWriteError> {
    super::trigger_insert::settle_trigger_written_tables(
        write_txn,
        &WriteTarget::InTxn(write_txn),
        ctx,
        writes,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // 区間が無ければ抑止しない。区間内は表名と深さが一致するときだけ抑止し、drop で戻る。
    #[test]
    fn scope_suppresses_only_the_same_table_at_the_same_depth() {
        assert!(!suppresses("t"));
        {
            let _scope = MergeStatementTriggerScope::enter("t");
            assert!(suppresses("t"));
            assert!(!suppresses("u"));
            let _deeper = crate::trigger::depth::enter(4).expect("depth");
            assert!(!suppresses("t"), "a nested DML must not be suppressed");
        }
        assert!(!suppresses("t"));
    }
}
