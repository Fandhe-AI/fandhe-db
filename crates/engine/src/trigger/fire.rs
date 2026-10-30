//! BEFORE／AFTER の ROW・STATEMENT トリガの実行（TABLE-45・Issue #1723・#1724・#1725・#1726）。
//!
//! `tenant/trigger_insert.rs`（INSERT）・`tenant/trigger_dml.rs`（UPDATE・DELETE）が行ごとに呼ぶ。
//! カタログ順（名前のバイト順）に各トリガの関数を
//! [`TriggerFunctionInvoker`](super::call::TriggerFunctionInvoker) 経由で呼び、前のトリガが返した行を
//! 次のトリガの NEW とする。`NULL` 返却（[`TriggerOutcome::Skip`]）はその行の残りのトリガを呼ばずに終える。
//! 呼び出し元は redb の表ハンドルを 1 つも開かずに呼ぶこと（トリガ本体が同じ write txn へ再入しても
//! 排他を壊さない。実行モデルの D5）。
//!
//! AFTER ROW は文の終わりに、変更した行ごと・トリガ名のバイト順に呼ぶ（[`fire_after_row`]）。
//! 戻り値は無視し（実行モデルの D3）、他表への書き込みは
//! [`TriggerWriteHandle`](super::write_handle::TriggerWriteHandle) 経由で行う。
//!
//! 文トリガ（FOR EACH STATEMENT。TRUNCATE を含む）は [`fire_statement`] が文につき 1 回、対象行が
//! 0 件でも発火する（Issue #1726）。

use super::call::{TriggerCall, TriggerOp, TriggerOutcome};
use super::runtime::TriggerRuntime;
use super::write_handle::TriggerWriteHandle;
use crate::catalog::trigger::{TriggerDef, TriggerLevel, TriggerTiming};
use crate::catalog::{ColumnType, TableSchema};
use crate::row_codec::Value;
use crate::sql::allowlist::SqlSurfaceError;

/// 返却行の構造違いの固定文言（値・列名・行を含めない）。
const RETURNED_ROW_MISMATCH: &str = "trigger function returned a row that does not match the table";

/// 発火時にも `WHEN` 条件を実行できないことの固定文言（定義時に拒否済みだが fail-closed に二重化する）。
const WHEN_UNSUPPORTED: &str = "trigger WHEN condition is not supported";

fn returned_row_mismatch() -> SqlSurfaceError {
    SqlSurfaceError::DatatypeMismatch {
        detail: RETURNED_ROW_MISMATCH.to_string(),
    }
}

/// トリガ関数が返した行の構造（列数・VECTOR 列の形）を検査する。型の細部（範囲・長さ）は
/// 書き込み時の行 codec が検査する。
pub(crate) fn check_returned_row(
    schema: &TableSchema,
    row: &[Value],
) -> Result<(), SqlSurfaceError> {
    if row.len() != schema.columns.len() {
        return Err(returned_row_mismatch());
    }
    for (col, value) in schema.columns.iter().zip(row) {
        match (&col.ty, value) {
            (ColumnType::Vector(dim), Value::Vector(v)) => {
                let expected = usize::try_from(*dim).map_err(|_| returned_row_mismatch())?;
                if v.len() != expected {
                    return Err(returned_row_mismatch());
                }
            }
            (ColumnType::Vector(_), Value::Null) => {}
            (ColumnType::Vector(_), _) | (_, Value::Vector(_)) => {
                return Err(returned_row_mismatch());
            }
            _ => {}
        }
    }
    Ok(())
}

/// BEFORE ROW の連鎖の共通実体。`old`（UPDATE・DELETE の OLD）と `new_row`（INSERT・UPDATE の NEW。
/// DELETE は空）を `op` に応じて渡す。`Ok(None)` は `NULL` 返却（その行の操作をやめる）、
/// `Ok(Some(row))` は最終 NEW。
#[allow(clippy::too_many_arguments)]
fn fire_before_chain(
    runtime: &TriggerRuntime,
    writes: &TriggerWriteHandle<'_>,
    table: &str,
    schema: &TableSchema,
    triggers: &[TriggerDef],
    op: TriggerOp,
    old: Option<&[Value]>,
    new_row: Vec<Value>,
) -> Result<Option<Vec<Value>>, SqlSurfaceError> {
    let mut row = new_row;
    for trigger in triggers {
        // 行ごと・呼び出しごとに割り込み（`57014`）を確認する。
        crate::interrupt::check()?;
        if trigger.when_sql.is_some() {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: WHEN_UNSUPPORTED.to_string(),
            });
        }
        let outcome = runtime.invoke(&TriggerCall {
            trigger_name: &trigger.name,
            function_key: &trigger.function_key,
            table,
            schema,
            op,
            level: TriggerLevel::Row,
            old_row: old,
            new_row: &row,
            args: &trigger.tg_args,
            timing: TriggerTiming::Before,
            writes,
        })?;
        match outcome {
            TriggerOutcome::Skip => return Ok(None),
            TriggerOutcome::Proceed(next) => {
                // DELETE は NEW を持たないため、返却行は使わず構造も検査しない（実行モデルの D3）。
                if op != TriggerOp::Delete {
                    check_returned_row(schema, &next)?;
                    row = next;
                }
            }
        }
    }
    Ok(Some(row))
}

/// `triggers`（INSERT・BEFORE・ROW のみ。カタログ順）を `new_row` に対して順に発火する。
/// `Ok(None)` はその行の挿入をやめる（`NULL` 返却）、`Ok(Some(row))` は書き込む最終行。
pub(crate) fn fire_before_row_insert(
    runtime: &TriggerRuntime,
    writes: &TriggerWriteHandle<'_>,
    table: &str,
    schema: &TableSchema,
    triggers: &[TriggerDef],
    new_row: Vec<Value>,
) -> Result<Option<Vec<Value>>, SqlSurfaceError> {
    fire_before_chain(
        runtime,
        writes,
        table,
        schema,
        triggers,
        TriggerOp::Insert,
        None,
        new_row,
    )
}

/// `triggers`（UPDATE・BEFORE・ROW。カタログ順）を `old`（更新前の行）と `new_row`（`SET` 適用後の
/// NEW の候補）に対して順に発火する（Issue #1725）。`Ok(None)` はその行の更新をやめる（`NULL` 返却）、
/// `Ok(Some(row))` は書き込む最終 NEW。
pub(crate) fn fire_before_row_update(
    runtime: &TriggerRuntime,
    writes: &TriggerWriteHandle<'_>,
    table: &str,
    schema: &TableSchema,
    triggers: &[TriggerDef],
    old: &[Value],
    new_row: Vec<Value>,
) -> Result<Option<Vec<Value>>, SqlSurfaceError> {
    fire_before_chain(
        runtime,
        writes,
        table,
        schema,
        triggers,
        TriggerOp::Update,
        Some(old),
        new_row,
    )
}

/// `triggers`（DELETE・BEFORE・ROW。カタログ順）を `old`（削除前の行）に対して順に発火する
/// （Issue #1725）。`true` は削除を続行、`false` は `NULL` 返却（その行を削除しない）。続行時の返却行は
/// 使わない（実行モデルの D3）。
pub(crate) fn fire_before_row_delete(
    runtime: &TriggerRuntime,
    writes: &TriggerWriteHandle<'_>,
    table: &str,
    schema: &TableSchema,
    triggers: &[TriggerDef],
    old: &[Value],
) -> Result<bool, SqlSurfaceError> {
    let outcome = fire_before_chain(
        runtime,
        writes,
        table,
        schema,
        triggers,
        TriggerOp::Delete,
        Some(old),
        Vec::new(),
    )?;
    Ok(outcome.is_some())
}

/// `triggers`（AFTER・ROW のみ。カタログ順）を、変更した 1 行に対して順に発火する（Issue #1724・#1725）。
/// INSERT は `old = None`・`new_row` が最終 NEW、UPDATE は `(OLD, 最終 NEW)`、DELETE は `old = Some` と
/// 空の `new_row`。戻り値（`Proceed`・`Skip`）は無視し、返却行の構造も検査しない（実行モデルの D3）。
/// エラーは [`SqlSurfaceError`] のまま返し、呼び出し側（`tenant/trigger_insert.rs`・`trigger_dml.rs`）が
/// 文全体を失敗させる。呼び出し元は redb の表ハンドルを 1 つも開かずに呼ぶこと。
#[allow(clippy::too_many_arguments)]
pub(crate) fn fire_after_row(
    runtime: &TriggerRuntime,
    writes: &TriggerWriteHandle<'_>,
    table: &str,
    schema: &TableSchema,
    triggers: &[TriggerDef],
    op: TriggerOp,
    old: Option<&[Value]>,
    new_row: &[Value],
) -> Result<(), SqlSurfaceError> {
    for trigger in triggers {
        crate::interrupt::check()?;
        if trigger.when_sql.is_some() {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: WHEN_UNSUPPORTED.to_string(),
            });
        }
        let _ignored = runtime.invoke(&TriggerCall {
            trigger_name: &trigger.name,
            function_key: &trigger.function_key,
            table,
            schema,
            op,
            level: TriggerLevel::Row,
            old_row: old,
            new_row,
            args: &trigger.tg_args,
            timing: TriggerTiming::After,
            writes,
        })?;
    }
    Ok(())
}

/// `triggers`（INSERT・AFTER・ROW のみ）を、書いた 1 行の最終 NEW に対して順に発火する（Issue #1724）。
pub(crate) fn fire_after_row_insert(
    runtime: &TriggerRuntime,
    writes: &TriggerWriteHandle<'_>,
    table: &str,
    schema: &TableSchema,
    triggers: &[TriggerDef],
    new_row: &[Value],
) -> Result<(), SqlSurfaceError> {
    fire_after_row(
        runtime,
        writes,
        table,
        schema,
        triggers,
        TriggerOp::Insert,
        None,
        new_row,
    )
}

/// `triggers`（FOR EACH STATEMENT。BEFORE か AFTER のどちらか一方の群。カタログ順）を、文につき 1 回、
/// 名前のバイト順に発火する（Issue #1726。実行モデルの D4 の段 2・段 7）。`op` は INSERT・UPDATE・
/// DELETE・TRUNCATE のいずれか。NEW／OLD は持たない（`old_row: None`・`new_row` は空。D1）。
/// 戻り値は無視する（文トリガは行を返さない。D3）。`when_sql` があれば発火時にも `0A000` とする。
/// 呼び出し元は redb の表ハンドルを 1 つも開かずに呼ぶこと（D5）。対象行が 0 件でも呼ぶ。
pub(crate) fn fire_statement(
    runtime: &TriggerRuntime,
    writes: &TriggerWriteHandle<'_>,
    table: &str,
    schema: &TableSchema,
    triggers: &[TriggerDef],
    op: TriggerOp,
    timing: TriggerTiming,
) -> Result<(), SqlSurfaceError> {
    for trigger in triggers {
        crate::interrupt::check()?;
        if trigger.when_sql.is_some() {
            return Err(SqlSurfaceError::FeatureNotSupported {
                detail: WHEN_UNSUPPORTED.to_string(),
            });
        }
        let _ignored = runtime.invoke(&TriggerCall {
            trigger_name: &trigger.name,
            function_key: &trigger.function_key,
            table,
            schema,
            op,
            level: TriggerLevel::Statement,
            old_row: None,
            new_row: &[],
            args: &trigger.tg_args,
            timing,
            writes,
        })?;
    }
    Ok(())
}
