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
//! 行トリガの `WHEN` は呼び出しの直前に [`TriggerRuntime::eval_when`] で評価し、TRUE のときだけ関数を呼ぶ
//! （Issue #3190。実行モデルの D7）。
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

/// 文トリガで `WHEN` 条件を実行できないことの固定文言（定義時に拒否済みだが fail-closed に二重化する）。
const WHEN_UNSUPPORTED: &str = "trigger WHEN condition is not supported";

/// 行トリガ 1 つの `WHEN` 条件を評価し、発火してよいかを返す（Issue #3190。実行モデルの D7）。
/// `when_sql` が無ければ常に `true`。TRUE だけが発火で、FALSE・NULL は不発火（次のトリガへ進む）。
/// 評価エラーはそのまま返し、呼び出し側が文全体を失敗させる（fail-closed）。
#[allow(clippy::too_many_arguments)]
fn when_passes(
    runtime: &TriggerRuntime,
    writes: &TriggerWriteHandle<'_>,
    table: &str,
    schema: &TableSchema,
    trigger: &TriggerDef,
    op: TriggerOp,
    level: TriggerLevel,
    old: Option<&[Value]>,
    new_row: &[Value],
    timing: TriggerTiming,
) -> Result<bool, SqlSurfaceError> {
    let Some(when_sql) = trigger.when_sql.as_deref() else {
        return Ok(true);
    };
    runtime.eval_when(
        &TriggerCall {
            trigger_name: &trigger.name,
            function_key: &trigger.function_key,
            table,
            schema,
            op,
            level,
            old_row: old,
            new_row,
            args: &trigger.tg_args,
            timing,
            writes,
            host: runtime.host(),
        },
        when_sql,
    )
}

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
        // WHEN は呼び出し直前の NEW（前のトリガが書き換えた最新の行）で評価する（D7）。
        if !when_passes(
            runtime,
            writes,
            table,
            schema,
            trigger,
            op,
            TriggerLevel::Row,
            old,
            &row,
            TriggerTiming::Before,
        )? {
            continue;
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
            host: runtime.host(),
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

/// AFTER ROW トリガのうち、変更した 1 行で `WHEN` が TRUE のもののインデックス（`triggers` 内の位置。
/// カタログ順）を返す（Issue #3190。実行モデルの D7: 行の変更の時点で評価し、偽ならキューに積まない）。
/// 評価エラーはそのまま返し、呼び出し側が文全体を失敗させる（fail-closed）。呼び出し元は行の変更の
/// 直後（表ハンドルを開いていない区間）で呼ぶこと。
#[allow(clippy::too_many_arguments)]
pub(crate) fn select_after_row(
    runtime: &TriggerRuntime,
    writes: &TriggerWriteHandle<'_>,
    table: &str,
    schema: &TableSchema,
    triggers: &[TriggerDef],
    op: TriggerOp,
    old: Option<&[Value]>,
    new_row: &[Value],
) -> Result<Vec<usize>, SqlSurfaceError> {
    let mut selected = Vec::new();
    for (idx, trigger) in triggers.iter().enumerate() {
        crate::interrupt::check()?;
        if when_passes(
            runtime,
            writes,
            table,
            schema,
            trigger,
            op,
            TriggerLevel::Row,
            old,
            new_row,
            TriggerTiming::After,
        )? {
            selected.push(idx);
        }
    }
    Ok(selected)
}

/// [`select_after_row`] が選んだ AFTER ROW トリガ（`selected` は `triggers` 内の位置）の本体だけを順に
/// 実行する。`WHEN` は再評価しない（D7）。戻り値（`Proceed`・`Skip`）は無視し、返却行の構造も検査しない
/// （実行モデルの D3）。エラーは呼び出し側（`tenant/trigger_insert.rs`・`trigger_dml.rs`）が文全体の
/// 失敗にする。呼び出し元は redb の表ハンドルを 1 つも開かずに呼ぶこと。
#[allow(clippy::too_many_arguments)]
pub(crate) fn invoke_after_row(
    runtime: &TriggerRuntime,
    writes: &TriggerWriteHandle<'_>,
    table: &str,
    schema: &TableSchema,
    triggers: &[TriggerDef],
    selected: &[usize],
    op: TriggerOp,
    old: Option<&[Value]>,
    new_row: &[Value],
) -> Result<(), SqlSurfaceError> {
    for trigger in selected.iter().filter_map(|i| triggers.get(*i)) {
        crate::interrupt::check()?;
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
            host: runtime.host(),
        })?;
    }
    Ok(())
}

/// `triggers`（AFTER・ROW のみ。カタログ順）を、変更した 1 行に対して `WHEN` の評価と本体の実行を続けて
/// 行う（Issue #1724・#1725）。行の変更時に評価してキューへ積む経路は [`select_after_row`] と
/// [`invoke_after_row`] を使う。INSERT は `old = None`・`new_row` が最終 NEW、UPDATE は `(OLD, 最終 NEW)`、
/// DELETE は `old = Some` と空の `new_row`。
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
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
    let selected = select_after_row(runtime, writes, table, schema, triggers, op, old, new_row)?;
    invoke_after_row(
        runtime, writes, table, schema, triggers, &selected, op, old, new_row,
    )
}

/// `triggers`（INSERT・AFTER・ROW のみ）を、書いた 1 行の最終 NEW に対して順に発火する（Issue #1724）。
#[cfg(test)]
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
            host: runtime.host(),
        })?;
    }
    Ok(())
}
